use std::path::Path;
#[cfg(any(not(unix), test))]
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use salvo::prelude::*;
use tokio::io::AsyncWriteExt;

use crate::config::AppConfig;
use crate::util::*;

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

    let url_path_str = percent_encoding::percent_decode_str(req.uri().path())
        .decode_utf8()
        .map(|s| s.to_string())
        .unwrap_or_default();

    let segments: Vec<&str> = url_path_str.split('/').filter(|s| !s.is_empty()).collect();

    let (req_p, symlink, url_err) = resolve_path(
        &config.hosted_directory.1,
        &segments,
        config.follow_symlinks,
    );

    if url_err {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Text::Html(error_html(
            "400 Bad Request",
            "The request URL was invalid.",
            "<p>Percent-encoding decoded to invalid UTF-8.</p>",
        )));
        return;
    }

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
        res.render(Text::Html(error_html(
            "404 Not Found",
            format!("The requested entity \"{}\" doesn't exist.", url_path_str),
            "",
        )));
        return;
    }

    // Read the body
    let body_bytes = match req.payload().await {
        Ok(data) => data.to_vec(),
        Err(_) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Text::Html(error_html(
                "503 Service Unavailable",
                "File not created.",
                "Failed to read request body",
            )));
            return;
        }
    };

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
                .map(|s| s * 1000)
        });

    let remote = req.remote_addr().to_string();

    // Ensure parent directories exist
    if let Some(parent) = req_p.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }

    let existent = req_p.exists();

    // Write file
    let result = write_file(&req_p, &body_bytes).await;
    match result {
        Ok(()) => {
            if let Some(ms) = mtime {
                let path = req_p.clone();
                tokio::task::spawn_blocking(move || {
                    set_mtime(&path, ms);
                })
                .await
                .ok();
            }

            crate::config::log_msg(
                config.log,
                &format!(
                    "{} {} {}, size: {}B",
                    remote,
                    if existent { "replaced" } else { "created" },
                    req_p.display(),
                    body_bytes.len()
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
        Err(e) => {
            crate::config::log_msg(config.log, &format!("{} File not created. {}", remote, e));
            res.status_code(StatusCode::SERVICE_UNAVAILABLE);
            res.render(Text::Html(error_html(
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
                        0o600 as libc::mode_t,
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

    async fn commit(self, destination: &Path) -> std::io::Result<()> {
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
                    libc::renameat(
                        self.parent.as_raw_fd(),
                        self.name.as_ptr(),
                        libc::AT_FDCWD,
                        destination.as_ptr(),
                    )
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
            tokio::fs::rename(&self.path, destination).await
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

async fn write_file(destination: &Path, data: &[u8]) -> std::io::Result<()> {
    let (pending, mut file) = PendingUpload::create(destination)?;
    file.write_all(data).await?;
    file.flush().await?;
    file.sync_all().await?;
    if let Ok(metadata) = tokio::fs::metadata(destination).await {
        file.set_permissions(metadata.permissions()).await?;
    }
    drop(file);
    pending.commit(destination).await
}

#[cfg(test)]
mod upload_tests {
    use super::*;

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
        assert!(pending.commit(&destination).await.is_err());
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
