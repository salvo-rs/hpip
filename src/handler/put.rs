use std::path::Path;
#[cfg(any(not(unix), test))]
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use salvo::hyper::body::Body;
use salvo::prelude::*;
use tokio::io::AsyncWriteExt;

use crate::config::AppConfig;
use crate::util::write_conditions::{MUTATION_LOCK, WriteConditions};
use crate::util::*;

#[derive(Debug)]
enum UploadError {
    PreconditionFailed,
    TooLarge,
    BodyRead(std::io::Error),
    Io(std::io::Error),
}

impl From<std::io::Error> for UploadError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[handler]
pub async fn handle_put(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let config = depot.get_typed::<Arc<AppConfig>>().unwrap().clone();

    if let Some(resp) = crate::hoops::auth::check_auth(req, &config) {
        *res = resp;
        return;
    }

    if config.writes_temp_dir.is_none() {
        res.status_code(StatusCode::FORBIDDEN);
        res.headers_mut()
            .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
        let body = error_html(
            "403 Forbidden",
            "This feature is currently disabled.",
            "<p>Ask the server administrator to pass <samp>-w</samp> to the executable to enable support for write requests.</p>",
        );
        res.headers_mut().insert(
            salvo::http::header::CONTENT_TYPE,
            "text/html; charset=utf-8".parse().unwrap(),
        );
        res.render(Text::Html(body));
        return;
    }

    let url_path_raw = req.uri().path().to_string();
    let decoded_path = percent_encoding::percent_decode_str(&url_path_raw)
        .decode_utf8()
        .map(|s| s.to_string());

    let segments: Vec<&str> = url_path_raw.split('/').filter(|s| !s.is_empty()).collect();

    let (req_p, symlink, url_err) = resolve_path(
        &config.hosted_directory.1,
        &segments,
        config.follow_symlinks,
    );

    if url_err || decoded_path.is_err() {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Text::Html(error_html(
            "400 Bad Request",
            "The request URL was invalid.",
            "<p>Percent-encoding decoded to invalid UTF-8.</p>",
        )));
        return;
    }
    let url_path_str = decoded_path.unwrap();

    if req_p.is_dir() {
        res.status_code(StatusCode::METHOD_NOT_ALLOWED);
        res.render(Text::Html(error_html(
            "405 Method Not Allowed",
            "Can't PUT on a directory.",
            format!(
                "<p>Allowed methods: {}</p>",
                config.allowed_methods.join(", ")
            ),
        )));
        return;
    }

    if detect_file_as_dir(&req_p) {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Text::Html(error_html(
            "400 Bad Request",
            "The request URL was invalid.",
            "<p>Attempted to use file as directory.</p>",
        )));
        return;
    }

    if req.headers().get("content-range").is_some() {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Text::Html(error_html(
            "400 Bad Request",
            "<a href=\"https://tools.ietf.org/html/rfc7231#section-4.3.3\">RFC7231 forbids partial-content PUT requests.</a>",
            "",
        )));
        return;
    }

    let illegal = config.is_symlink_denied_nonexistent(symlink, &req_p);
    if illegal {
        res.status_code(StatusCode::NOT_FOUND);
        res.render(Text::Html(error_html_text(
            "404 Not Found",
            format!("The requested entity \"{}\" doesn't exist.", url_path_str),
            "",
        )));
        return;
    }

    let conditions = match WriteConditions::from_request(req) {
        Ok(conditions) => conditions,
        Err(status) => {
            res.status_code(status);
            return;
        }
    };

    if req
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > config.upload_limit)
    {
        res.status_code(StatusCode::PAYLOAD_TOO_LARGE);
        return;
    }

    // Get modification time from headers
    let mtime = req
        .headers()
        .get("x-last-modified")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| {
            req.headers()
                .get("x-oc-mtime")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .and_then(|s| s.checked_mul(1000))
        });

    let remote = req.remote_addr().to_string();

    // Reject failed conditions before creating any parent directory. Publication
    // still rechecks them after the request body has been staged completely.
    let prepare_path = req_p.clone();
    let prepare_conditions = conditions.clone();
    let Some(preparation) = config.spawn_blocking(move || {
        let _guard = MUTATION_LOCK.blocking_lock();
        if !prepare_conditions.allows_path(&prepare_path)? {
            return Err(UploadError::PreconditionFailed);
        }
        if let Some(parent) = prepare_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(())
    }) else {
        res.status_code(StatusCode::SERVICE_UNAVAILABLE);
        return;
    };
    let result = match preparation.await {
        Ok(Ok(())) => {
            write_request_file(
                &req_p,
                req.take_body(),
                &conditions,
                config.upload_limit,
                mtime,
            )
            .await
        }
        Ok(Err(error)) => Err(error),
        Err(error) => Err(UploadError::Io(std::io::Error::other(error))),
    };
    match result {
        Ok((existent, uploaded_bytes)) => {
            crate::config::log_msg(
                config.log,
                &format!(
                    "{} {} {}, size: {}B",
                    remote,
                    if existent { "replaced" } else { "created" },
                    req_p.display(),
                    uploaded_bytes
                ),
            );

            res.status_code(if existent {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::CREATED
            });
            res.headers_mut()
                .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
        }
        Err(UploadError::PreconditionFailed) => {
            res.status_code(StatusCode::PRECONDITION_FAILED);
        }
        Err(UploadError::TooLarge) => {
            res.status_code(StatusCode::PAYLOAD_TOO_LARGE);
        }
        Err(UploadError::BodyRead(error)) => {
            res.status_code(if error.kind() == std::io::ErrorKind::TimedOut {
                StatusCode::REQUEST_TIMEOUT
            } else {
                StatusCode::BAD_REQUEST
            });
        }
        Err(UploadError::Io(e)) => {
            crate::config::log_msg(config.log, &format!("{} File not created. {}", remote, e));
            res.status_code(StatusCode::SERVICE_UNAVAILABLE);
            res.render(Text::Html(error_html_text(
                "503 Service Unavailable",
                "File not created.",
                format!("{}", e),
            )));
        }
    }
}

/// An upload is never visible at its destination until the complete file is ready.
/// The guard also removes unfinished files when the request future is cancelled.
struct PendingUpload {
    #[cfg(not(unix))]
    path: PathBuf,
    #[cfg(unix)]
    parent: std::fs::File,
    #[cfg(unix)]
    name: std::ffi::CString,
}

impl PendingUpload {
    fn create(destination: &Path) -> std::io::Result<(Self, tokio::fs::File)> {
        static NEXT_UPLOAD: AtomicU64 = AtomicU64::new(0);
        let parent = destination.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "upload has no parent")
        })?;
        #[cfg(unix)]
        let parent = {
            use std::os::unix::fs::OpenOptionsExt;
            // Searching a writable directory need not grant permission to list it.
            #[cfg(any(target_os = "linux", target_os = "android"))]
            let access = libc::O_PATH;
            #[cfg(target_vendor = "apple")]
            let access = libc::O_SEARCH;
            #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
            let access = libc::O_RDONLY;
            std::fs::File::options()
                .read(true)
                .custom_flags(access | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(parent)?
        };
        loop {
            let id = NEXT_UPLOAD.fetch_add(1, Ordering::Relaxed);
            let name = format!(".hpip-upload-{}-{id}", std::process::id());
            #[cfg(unix)]
            {
                use std::os::fd::{AsRawFd, FromRawFd};
                let name = std::ffi::CString::new(name).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "upload name contains NUL",
                    )
                })?;
                // The directory descriptor and NUL-terminated name live through the call.
                let fd = unsafe {
                    libc::openat(
                        parent.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_WRONLY
                            | libc::O_CREAT
                            | libc::O_EXCL
                            | libc::O_CLOEXEC
                            | libc::O_NOFOLLOW,
                        // C varargs need an int-width value; macOS mode_t is u16.
                        0o600 as libc::c_uint,
                    )
                };
                if fd >= 0 {
                    // A successful openat transfers this new descriptor to File exactly once.
                    let file = unsafe { std::fs::File::from_raw_fd(fd) };
                    return Ok((Self { parent, name }, tokio::fs::File::from_std(file)));
                }
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error);
                }
            }
            #[cfg(not(unix))]
            {
                let path = parent.join(name);
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);
                match options.open(&path) {
                    Ok(file) => return Ok((Self { path }, tokio::fs::File::from_std(file))),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
            }
        }
    }

    async fn commit(self, destination: &Path, create_only: bool) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::ffi::OsStrExt;
            let destination =
                std::ffi::CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "upload destination contains NUL",
                    )
                })?;
            tokio::task::spawn_blocking(move || {
                // Anchor the source entry even if MOVE renamed its parent while the
                // body was arriving. The destination still names the requested URL.
                // Both C strings and the owned source directory descriptor remain live.
                let result = unsafe {
                    if create_only {
                        libc::linkat(
                            self.parent.as_raw_fd(),
                            self.name.as_ptr(),
                            libc::AT_FDCWD,
                            destination.as_ptr(),
                            0,
                        )
                    } else {
                        libc::renameat(
                            self.parent.as_raw_fd(),
                            self.name.as_ptr(),
                            libc::AT_FDCWD,
                            destination.as_ptr(),
                        )
                    }
                };
                if result == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            })
            .await
            .map_err(std::io::Error::other)?
        }
        #[cfg(not(unix))]
        {
            // Both paths are on the same filesystem. Rename replaces atomically;
            // never fall back to copying over the live destination.
            if create_only {
                // Publishing a new directory entry is atomic even against another
                // process creating the same target. Never replace an existing file.
                tokio::fs::hard_link(&self.path, destination).await
            } else {
                tokio::fs::rename(&self.path, destination).await
            }
        }
    }
}

impl Drop for PendingUpload {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // The descriptor outlives cleanup, including after its directory moves.
            unsafe {
                libc::unlinkat(self.parent.as_raw_fd(), self.name.as_ptr(), 0);
            }
        }
        #[cfg(not(unix))]
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn write_request_file(
    destination: &Path,
    mut body: salvo::http::ReqBody,
    conditions: &WriteConditions,
    limit: u64,
    mtime: Option<u64>,
) -> Result<(bool, u64), UploadError> {
    let (pending, mut file) = PendingUpload::create(destination)?;
    let mut length = 0_u64;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15 * 60);
    loop {
        let frame_deadline =
            (tokio::time::Instant::now() + std::time::Duration::from_secs(30)).min(deadline);
        let frame = tokio::time::timeout_at(
            frame_deadline,
            std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)),
        )
        .await
        .map_err(|_| {
            UploadError::BodyRead(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "upload body timed out",
            ))
        })?;
        let Some(frame) = frame else {
            break;
        };
        let frame = frame.map_err(UploadError::BodyRead)?;
        if let Ok(bytes) = frame.into_data() {
            length = length
                .checked_add(bytes.len() as u64)
                .ok_or(UploadError::TooLarge)?;
            if length > limit {
                return Err(UploadError::TooLarge);
            }
            file.write_all(&bytes).await?;
        }
    }
    file.flush().await?;
    if let Some(ms) = mtime {
        let handle = file.try_clone().await?.into_std().await;
        tokio::task::spawn_blocking(move || set_mtime_f(&handle, ms))
            .await
            .map_err(std::io::Error::other)??;
    }
    file.sync_all().await?;
    let _commit_lock = MUTATION_LOCK.lock().await;
    let metadata = match tokio::fs::metadata(destination).await {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if !conditions.allows_metadata(metadata.as_ref())? {
        return Err(UploadError::PreconditionFailed);
    }
    if let Some(metadata) = metadata.as_ref() {
        file.set_permissions(metadata.permissions()).await?;
    }
    drop(file);
    pending
        .commit(destination, conditions.create_only())
        .await
        .map_err(|error| {
            if conditions.create_only() && error.kind() == std::io::ErrorKind::AlreadyExists {
                UploadError::PreconditionFailed
            } else {
                UploadError::Io(error)
            }
        })?;
    Ok((metadata.is_some(), length))
}

#[cfg(test)]
async fn write_file_conditional(
    destination: &Path,
    data: &[u8],
    conditions: &WriteConditions,
) -> Result<bool, UploadError> {
    write_request_file(
        destination,
        salvo::http::ReqBody::Once(salvo::hyper::body::Bytes::copy_from_slice(data)),
        conditions,
        u64::MAX,
        None,
    )
    .await
    .map(|(existed, _)| existed)
}

#[cfg(test)]
mod upload_tests {
    use super::*;

    async fn write_file(destination: &Path, data: &[u8]) -> Result<bool, UploadError> {
        write_file_conditional(destination, data, &WriteConditions::default()).await
    }

    fn fixture() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "hpip-atomic-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn same_basename_uploads_do_not_share_staging_files() {
        let dir = fixture();
        let a = dir.join("a");
        let b = dir.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let a_file = a.join("same.txt");
        let b_file = b.join("same.txt");
        let a_data = vec![b'a'; 128 * 1024];
        let b_data = vec![b'b'; 128 * 1024];
        let (ra, rb) = tokio::join!(write_file(&a_file, &a_data), write_file(&b_file, &b_data));
        ra.unwrap();
        rb.unwrap();
        assert_eq!(
            std::fs::read(a.join("same.txt")).unwrap(),
            vec![b'a'; 128 * 1024]
        );
        assert_eq!(
            std::fs::read(b.join("same.txt")).unwrap(),
            vec![b'b'; 128 * 1024]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn failed_commit_preserves_destination_and_cleans_staging() {
        let dir = fixture();
        let destination = dir.join("existing-directory");
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(destination.join("keep"), b"original").unwrap();
        assert!(write_file(&destination, b"new").await.is_err());
        assert_eq!(
            std::fs::read(destination.join("keep")).unwrap(),
            b"original"
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn abandoned_upload_is_invisible_and_removed() {
        let dir = fixture();
        let destination = dir.join("file");
        std::fs::write(&destination, b"original").unwrap();
        let (pending, mut file) = PendingUpload::create(&destination).unwrap();
        file.write_all(b"unfinished").await.unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"original");
        drop(file);
        drop(pending);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_upload_is_removed_after_its_parent_moves() {
        let dir = fixture();
        let parent = dir.join("parent");
        let moved = dir.join("moved");
        std::fs::create_dir(&parent).unwrap();
        let (pending, mut file) = PendingUpload::create(&parent.join("file")).unwrap();
        file.write_all(b"unfinished").await.unwrap();
        std::fs::rename(&parent, &moved).unwrap();
        drop(file);
        drop(pending);
        assert_eq!(std::fs::read_dir(&moved).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_publication_is_removed_after_its_parent_moves() {
        let dir = fixture();
        let parent = dir.join("parent");
        let moved = dir.join("moved");
        std::fs::create_dir(&parent).unwrap();
        let destination = parent.join("file");
        let (pending, mut file) = PendingUpload::create(&destination).unwrap();
        file.write_all(b"complete but unpublished").await.unwrap();
        drop(file);
        std::fs::rename(&parent, &moved).unwrap();
        assert!(pending.commit(&destination, false).await.is_err());
        assert_eq!(std::fs::read_dir(&moved).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    #[tokio::test]
    async fn uploads_do_not_require_directory_listing_permission() {
        use std::os::unix::fs::PermissionsExt;
        let dir = fixture();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o300)).unwrap();
        let created = PendingUpload::create(&dir.join("file"));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (pending, file) = created.unwrap();
        drop(file);
        drop(pending);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod conditional_upload_tests {
    use super::*;

    #[tokio::test]
    async fn only_one_concurrent_create_only_upload_succeeds() {
        let directory =
            std::env::temp_dir().join(format!("hpip-create-only-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let destination = directory.join("file");
        let _ = std::fs::remove_file(&destination);
        let conditions = WriteConditions {
            if_none_match: Some("*".into()),
            ..Default::default()
        };
        let (first, second) = tokio::join!(
            write_file_conditional(&destination, b"first", &conditions),
            write_file_conditional(&destination, b"second", &conditions)
        );
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        assert!(
            matches!(first, Err(UploadError::PreconditionFailed))
                || matches!(second, Err(UploadError::PreconditionFailed))
        );
        let bytes = std::fs::read(&destination).unwrap();
        assert!(bytes == b"first" || bytes == b"second");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn stale_etag_cannot_overwrite_current_content() {
        let directory =
            std::env::temp_dir().join(format!("hpip-stale-etag-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let destination = directory.join("file");
        std::fs::write(&destination, b"original").unwrap();
        let old_tag = file_etag(&std::fs::metadata(&destination).unwrap());
        let conditions = WriteConditions {
            if_match: Some(format!("\"{old_tag}\"")),
            ..Default::default()
        };
        assert!(
            write_file_conditional(&destination, b"new version is longer", &conditions)
                .await
                .is_ok()
        );
        assert!(matches!(
            write_file_conditional(&destination, b"stale", &conditions).await,
            Err(UploadError::PreconditionFailed)
        ));
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"new version is longer"
        );
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn if_match_uses_strong_comparison_and_if_none_match_uses_weak_comparison() {
        let mut conditions = WriteConditions {
            if_match: Some("W/\"tag\"".into()),
            ..Default::default()
        };
        assert!(!conditions.allows(Some("tag")));
        conditions.if_match = Some("\"other\", \"tag\"".into());
        assert!(conditions.allows(Some("tag")));
        conditions.if_none_match = Some("W/\"tag\"".into());
        assert!(!conditions.allows(Some("tag")));
        assert!(!conditions.allows(None));
    }
}

#[cfg(test)]
mod parent_precondition_tests {
    use salvo::test::TestClient;

    use super::*;
    use crate::test_support::{TestDir, config};

    #[tokio::test]
    async fn failed_versions_never_create_parent_directories() {
        let fixture = TestDir::new();
        let service = Service::new(crate::build_router(config(fixture.path(), &["-w"])));
        for tag in ["\"stale\"", "*"] {
            let response = TestClient::put("http://localhost/missing/nested/file")
                .add_header("If-Match", tag, true)
                .body("upload")
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::PRECONDITION_FAILED));
            assert!(!fixture.path().join("missing").exists());
        }
        let response = TestClient::put("http://localhost/missing/nested/file")
            .add_header("If-None-Match", "*", true)
            .body("first")
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::CREATED));
        let response = TestClient::put("http://localhost/missing/nested/file")
            .add_header("If-None-Match", "*", true)
            .body("second")
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::PRECONDITION_FAILED));
        assert_eq!(
            std::fs::read(fixture.path().join("missing/nested/file")).unwrap(),
            b"first"
        );
    }

    #[tokio::test]
    async fn rejected_preparation_has_no_filesystem_side_effects() {
        let fixture = TestDir::new();
        let config = config(fixture.path(), &["-w"]);
        let _permits = config
            .blocking_jobs
            .clone()
            .acquire_many_owned(4)
            .await
            .unwrap();
        let service = Service::new(crate::build_router(config));
        let response = TestClient::put("http://localhost/missing/nested/file")
            .body("upload")
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!fixture.path().join("missing").exists());
    }
}

#[cfg(test)]
mod streaming_upload_tests {
    use std::task::{Context, Poll};

    #[tokio::test]
    async fn requested_timestamp_is_applied_before_upload_publication() {
        let directory = TestDir::new();
        let service = Service::new(crate::build_router(config(directory.path(), &["-w"])));
        let response = TestClient::put("http://localhost/timed.txt")
            .add_header("x-last-modified", "1600000000123", true)
            .body("timestamp fixture")
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::CREATED));
        let modified = std::fs::metadata(directory.path().join("timed.txt"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            modified
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            1_600_000_000_123
        );
    }

    use salvo::hyper::body::{Bytes, Frame};
    use salvo::test::TestClient;

    use super::*;
    use crate::test_support::{TestDir, config};

    #[tokio::test]
    async fn uploads_larger_than_default_payload_limit_succeed() {
        let directory = TestDir::new();
        let service = Service::new(crate::build_router(config(directory.path(), &["-w"])));
        let bytes = vec![b'x'; 128 * 1024];
        let response = TestClient::put("http://localhost/large.bin")
            .body(bytes.clone())
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::CREATED));
        assert_eq!(
            std::fs::read(directory.path().join("large.bin")).unwrap(),
            bytes
        );
    }

    #[tokio::test]
    async fn oversized_stream_preserves_destination_and_removes_staging() {
        let directory = TestDir::new();
        let destination = directory.path().join("file");
        std::fs::write(&destination, b"original").unwrap();
        let result = write_request_file(
            &destination,
            salvo::http::ReqBody::Once(Bytes::from_static(b"too large")),
            &WriteConditions::default(),
            4,
            None,
        )
        .await;
        assert!(matches!(result, Err(UploadError::TooLarge)));
        assert_eq!(std::fs::read(&destination).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        let service = Service::new(crate::build_router(config(
            directory.path(),
            &["-w", "--max-upload-size", "4"],
        )));
        let response = TestClient::put("http://localhost/file")
            .body("oversized")
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::PAYLOAD_TOO_LARGE));
    }

    struct InterruptedBody(bool);

    impl Body for InterruptedBody {
        type Data = Bytes;
        type Error = salvo::BoxedError;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
            if !self.0 {
                self.0 = true;
                Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"partial")))))
            } else {
                Poll::Ready(Some(Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "interrupted",
                )))))
            }
        }
    }

    #[tokio::test]
    async fn interrupted_body_never_publishes_partial_content() {
        let directory = TestDir::new();
        let destination = directory.path().join("file");
        std::fs::write(&destination, b"original").unwrap();
        let body = salvo::http::ReqBody::Boxed {
            inner: Box::pin(InterruptedBody(false)),
            fuse_config: None,
        };
        assert!(matches!(
            write_request_file(&destination, body, &WriteConditions::default(), 1024, None).await,
            Err(UploadError::BodyRead(_))
        ));
        assert_eq!(std::fs::read(destination).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
