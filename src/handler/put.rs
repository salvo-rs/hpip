use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use salvo::prelude::*;
use tokio::io::AsyncWriteExt;

use crate::config::AppConfig;
use crate::util::*;

// Serialize the precondition check and publication of PUTs in this process.
static UPLOAD_COMMIT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Default, Clone)]
struct WriteConditions {
    if_match: Option<String>,
    if_none_match: Option<String>,
}

impl WriteConditions {
    fn from_request(req: &Request) -> Result<Self, StatusCode> {
        let header = |name| {
            req.headers()
                .get(name)
                .map(|value| {
                    value
                        .to_str()
                        .map(str::to_owned)
                        .map_err(|_| StatusCode::BAD_REQUEST)
                })
                .transpose()
        };
        Ok(Self {
            if_match: header("if-match")?,
            if_none_match: header("if-none-match")?,
        })
    }

    fn allows(&self, current: Option<&str>) -> bool {
        let matches = |value: &str, weak: bool| {
            if value.trim() == "*" {
                return current.is_some();
            }
            value.split(',').any(|candidate| {
                let candidate = candidate.trim();
                let candidate = if weak {
                    candidate.strip_prefix("W/").unwrap_or(candidate)
                } else {
                    candidate
                };
                current.is_some_and(|etag| candidate == format!("\"{etag}\""))
            })
        };
        self.if_match
            .as_deref()
            .is_none_or(|value| matches(value, false))
            && self
                .if_none_match
                .as_deref()
                .is_none_or(|value| !matches(value, true))
    }

    fn create_only(&self) -> bool {
        self.if_none_match
            .as_deref()
            .is_some_and(|value| value.trim() == "*")
    }
}

#[derive(Debug)]
enum UploadError {
    PreconditionFailed,
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

    let conditions = match WriteConditions::from_request(req) {
        Ok(conditions) => conditions,
        Err(status) => {
            res.status_code(status);
            return;
        }
    };

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

    // Write file
    let result = write_file_conditional(&req_p, &body_bytes, &conditions).await;
    match result {
        Ok(existent) => {
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
        Err(UploadError::PreconditionFailed) => {
            res.status_code(StatusCode::PRECONDITION_FAILED);
        }
        Err(UploadError::Io(e)) => {
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
    path: PathBuf,
}

impl PendingUpload {
    fn create(destination: &Path) -> std::io::Result<(Self, tokio::fs::File)> {
        static NEXT_UPLOAD: AtomicU64 = AtomicU64::new(0);
        let parent = destination.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "upload has no parent")
        })?;
        loop {
            let id = NEXT_UPLOAD.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(".hpip-upload-{}-{id}", std::process::id()));
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => return Ok((Self { path }, tokio::fs::File::from_std(file))),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    async fn commit(self, destination: &Path, create_only: bool) -> std::io::Result<()> {
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

impl Drop for PendingUpload {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn write_file_conditional(
    destination: &Path,
    data: &[u8],
    conditions: &WriteConditions,
) -> Result<bool, UploadError> {
    let (pending, mut file) = PendingUpload::create(destination)?;
    file.write_all(data).await?;
    file.flush().await?;
    file.sync_all().await?;
    let _commit_lock = UPLOAD_COMMIT_LOCK.lock().await;
    let metadata = match tokio::fs::metadata(destination).await {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let current_etag = metadata.as_ref().map(file_etag);
    if !conditions.allows(current_etag.as_deref()) {
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
    Ok(metadata.is_some())
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
