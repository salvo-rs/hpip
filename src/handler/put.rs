use std::path::{Path, PathBuf};
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
    Rejected(Box<Response>),
    PreconditionFailed,
    TooLarge,
    BodyRead(std::io::Error),
    Io(std::io::Error),
}

struct UploadAuthority {
    request: Request,
    config: Arc<AppConfig>,
}

struct PreparedUpload {
    pending: PendingUpload,
    file: tokio::fs::File,
    target: UploadTarget,
}

enum UploadTarget {
    Request(Box<UploadAuthority>),
    #[cfg(test)]
    Filesystem(PathBuf),
}

impl UploadAuthority {
    /// Call while holding MUTATION_LOCK, both before preparing parents and
    /// immediately before publishing. A previously resolved path is not authority.
    fn current_path(&self) -> Result<PathBuf, UploadError> {
        if let Some(response) = crate::hoops::auth::check_auth(&self.request, &self.config) {
            return Err(UploadError::Rejected(Box::new(response)));
        }
        let segments: Vec<_> = self
            .request
            .uri()
            .path()
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        let (path, symlink, invalid) = resolve_path(
            &self.config.hosted_directory.1,
            &segments,
            self.config.follow_symlinks,
        );
        let status = if invalid || detect_file_as_dir(&path) {
            Some(StatusCode::BAD_REQUEST)
        } else if self.config.is_symlink_denied_nonexistent(symlink, &path) {
            Some(StatusCode::NOT_FOUND)
        } else if path.is_dir() {
            Some(StatusCode::METHOD_NOT_ALLOWED)
        } else {
            None
        };
        if let Some(status) = status {
            let mut response = Response::new();
            response.status_code(status);
            return Err(UploadError::Rejected(Box::new(response)));
        }
        Ok(path)
    }
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
    let mut snapshot = Request::new();
    *snapshot.uri_mut() = req.uri().clone();
    *snapshot.method_mut() = req.method().clone();
    *snapshot.headers_mut() = req.headers().clone();
    *snapshot.remote_addr_mut() = req.remote_addr().clone();
    let authority = UploadAuthority {
        request: snapshot,
        config: config.clone(),
    };
    let prepare_conditions = conditions.clone();
    let Some(preparation) = config.spawn_blocking(move || {
        let _guard = MUTATION_LOCK.blocking_lock();
        let prepare_path = authority.current_path()?;
        if !prepare_conditions.allows_path(&prepare_path)? {
            return Err(UploadError::PreconditionFailed);
        }
        if let Some(parent) = prepare_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Create staging under the same namespace lock as its authorization.
        let (pending, file) = PendingUpload::create(&prepare_path)?;
        Ok(PreparedUpload {
            pending,
            file,
            target: UploadTarget::Request(Box::new(authority)),
        })
    }) else {
        res.status_code(StatusCode::SERVICE_UNAVAILABLE);
        return;
    };
    let result = match preparation.await {
        Ok(Ok(prepared)) => {
            write_prepared_file(
                &config,
                prepared,
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
        Err(UploadError::Rejected(response)) => *res = *response,
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

    fn commit(self, destination: &Path, create_only: bool) -> std::io::Result<()> {
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
            // The caller owns the mutation lock on this blocking worker for the
            // entire transaction, including this filesystem publication.
            // The owned source descriptor anchors staging if its parent moved;
            // both NUL-terminated names remain valid throughout the syscall.
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
        }
        #[cfg(not(unix))]
        {
            // Both paths are on the same filesystem. Rename replaces atomically;
            // never fall back to copying over the live destination.
            if create_only {
                // Publishing a new directory entry is atomic even against another
                // process creating the same target. Never replace an existing file.
                std::fs::hard_link(&self.path, destination)
            } else {
                std::fs::rename(&self.path, destination)
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

async fn write_prepared_file(
    config: &Arc<AppConfig>,
    prepared: PreparedUpload,
    mut body: salvo::http::ReqBody,
    conditions: &WriteConditions,
    limit: u64,
    mtime: Option<u64>,
) -> Result<(bool, u64), UploadError> {
    let PreparedUpload {
        pending,
        mut file,
        target,
    } = prepared;
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
    let file = file.into_std().await;
    let conditions = conditions.clone();
    commit_upload(config, move || {
        let destination = match target {
            UploadTarget::Request(authority) => authority.current_path()?,
            #[cfg(test)]
            UploadTarget::Filesystem(destination) => destination,
        };
        let metadata = match path_metadata_and_etag(&destination) {
            Ok(snapshot) => Some(snapshot),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(UploadError::Io(error)),
        };
        if !conditions.allows_snapshot(metadata.as_ref())? {
            return Err(UploadError::PreconditionFailed);
        }
        if let Some((metadata, _)) = metadata.as_ref() {
            file.set_permissions(metadata.permissions())?;
        }
        drop(file);
        pending
            .commit(&destination, conditions.create_only())
            .map_err(|error| {
                if conditions.create_only() && error.kind() == std::io::ErrorKind::AlreadyExists {
                    UploadError::PreconditionFailed
                } else {
                    UploadError::Io(error)
                }
            })?;
        Ok((metadata.is_some(), length))
    })
    .await
}

/// A cancelled request may stop waiting for a commit, but cannot drop its lock
/// while an admitted filesystem transaction is still publishing in the worker.
async fn commit_upload<R: Send + 'static>(
    config: &Arc<AppConfig>,
    work: impl FnOnce() -> Result<R, UploadError> + Send + 'static,
) -> Result<R, UploadError> {
    let (cancel_on_drop, cancelled) = tokio::sync::oneshot::channel::<()>();
    let runtime = tokio::runtime::Handle::current();
    let job = config
        .spawn_blocking(move || {
            let guard = runtime.block_on(async {
                tokio::select! {
                    biased;
                    _ = cancelled => None,
                    guard = MUTATION_LOCK.lock() => Some(guard),
                }
            });
            let Some(_guard) = guard else {
                return Err(UploadError::BodyRead(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "upload cancelled before publication",
                )));
            };
            work()
        })
        .ok_or_else(|| {
            UploadError::Io(std::io::Error::other(
                "upload publication worker unavailable",
            ))
        })?;
    let result = job.await.map_err(std::io::Error::other)?;
    drop(cancel_on_drop);
    result
}

#[cfg(test)]
mod publication_lifetime_tests {
    use std::time::Duration;

    use super::*;
    use crate::test_support::{TestDir, config};

    async fn staged(destination: &Path) -> (PendingUpload, std::fs::File) {
        let (pending, mut file) = PendingUpload::create(destination).unwrap();
        file.write_all(b"complete replacement").await.unwrap();
        file.flush().await.unwrap();
        (pending, file.into_std().await)
    }

    #[tokio::test]
    async fn cancelled_publication_keeps_its_lock_and_drain_permit_until_the_worker_finishes() {
        let fixture = TestDir::new();
        let path = fixture.path().join("file");
        std::fs::write(&path, b"original").unwrap();
        let config = config(fixture.path(), &["-w"]);
        let (pending, file) = staged(&path).await;
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let request_config = config.clone();
        let destination = path.clone();
        let request = tokio::spawn(async move {
            commit_upload(&request_config, move || {
                entered.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(5)).unwrap();
                drop(file);
                pending.commit(&destination, false)?;
                Ok(())
            })
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), ready)
            .await
            .unwrap()
            .unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), MUTATION_LOCK.lock())
                .await
                .is_err()
        );
        let drain_config = config.clone();
        let mut drain = tokio::spawn(async move { drain_config.drain_blocking_jobs().await });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut drain)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), drain)
            .await
            .unwrap()
            .unwrap();
        let _guard = tokio::time::timeout(Duration::from_secs(2), MUTATION_LOCK.lock())
            .await
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"complete replacement");
        assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn cancellation_before_the_commit_lock_discards_staging_without_waiting_for_the_lock() {
        let fixture = TestDir::new();
        let path = fixture.path().join("file");
        std::fs::write(&path, b"original").unwrap();
        let config = config(fixture.path(), &["-w"]);
        let (pending, file) = staged(&path).await;
        let _guard = MUTATION_LOCK.lock().await;
        let request_config = config.clone();
        let destination = path.clone();
        let request = tokio::spawn(async move {
            commit_upload(&request_config, move || {
                drop(file);
                pending.commit(&destination, false)?;
                Ok(())
            })
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while config.blocking_jobs.available_permits() == 4 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        let _all = tokio::time::timeout(
            Duration::from_secs(2),
            config.blocking_jobs.clone().acquire_many_owned(4),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn unavailable_publication_workers_leave_the_original_and_remove_staging() {
        for stopped in [false, true] {
            let fixture = TestDir::new();
            let path = fixture.path().join("file");
            std::fs::write(&path, b"original").unwrap();
            let config = config(fixture.path(), &["-w"]);
            if stopped {
                config.drain_blocking_jobs().await;
            }
            let _all = config
                .blocking_jobs
                .clone()
                .acquire_many_owned(4)
                .await
                .unwrap();
            let (pending, file) = staged(&path).await;
            let destination = path.clone();
            let result = commit_upload(&config, move || {
                drop(file);
                pending.commit(&destination, false)?;
                Ok(())
            })
            .await;
            assert!(matches!(result, Err(UploadError::Io(_))));
            assert_eq!(std::fs::read(&path).unwrap(), b"original");
            assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 1);
        }
    }
}

#[cfg(test)]
async fn write_request_file(
    destination: &Path,
    body: salvo::http::ReqBody,
    conditions: &WriteConditions,
    limit: u64,
    mtime: Option<u64>,
) -> Result<(bool, u64), UploadError> {
    let (pending, file) = PendingUpload::create(destination)?;
    let config = crate::test_support::config(destination.parent().unwrap(), &["-w"]);
    write_prepared_file(
        &config,
        PreparedUpload {
            pending,
            file,
            target: UploadTarget::Filesystem(destination.to_path_buf()),
        },
        body,
        conditions,
        limit,
        mtime,
    )
    .await
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
mod namespace_upload_tests {
    use std::task::{Context, Poll};

    use salvo::hyper::body::{Bytes, Frame};
    use salvo::test::TestClient;
    use tokio::sync::oneshot;

    use super::*;
    use crate::test_support::{TestDir, config};

    struct PausedBody {
        entered: Option<oneshot::Sender<()>>,
        release: oneshot::Receiver<()>,
        sent: bool,
    }

    impl Body for PausedBody {
        type Data = Bytes;
        type Error = salvo::BoxedError;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
            if self.sent {
                return Poll::Ready(None);
            }
            if let Some(entered) = self.entered.take() {
                entered.send(()).unwrap();
            }
            match std::future::Future::poll(Pin::new(&mut self.release), cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(_) => {
                    self.sent = true;
                    Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(
                        b"complete upload",
                    )))))
                }
            }
        }
    }

    fn paused_request(path: &str) -> (Request, oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, ready) = oneshot::channel();
        let (release, wait) = oneshot::channel();
        let mut request = TestClient::put(format!("http://localhost{path}")).build();
        *request.body_mut() = salvo::http::ReqBody::Boxed {
            inner: Box::pin(PausedBody {
                entered: Some(entered),
                release: wait,
                sent: false,
            }),
            fuse_config: None,
        };
        (request, ready, release)
    }

    #[tokio::test]
    async fn commit_rejects_a_current_collection_and_cleans_staging() {
        let fixture = TestDir::new();
        let target = fixture.path().join("file");
        std::fs::write(&target, b"initial file").unwrap();
        let prepared = fixture.path().join("prepared");
        std::fs::create_dir(&prepared).unwrap();
        std::fs::write(prepared.join("member"), b"preserved collection").unwrap();
        let service = Service::new(crate::build_router(config(fixture.path(), &["-w"])));
        let (request, entered, release) = paused_request("/file");
        let job = tokio::spawn(async move { service.handle(request).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), entered)
            .await
            .unwrap()
            .unwrap();
        {
            let _guard = MUTATION_LOCK.lock().await;
            crate::util::namespace::rename_no_replace(&target, &fixture.path().join("retired"))
                .unwrap();
            crate::util::namespace::rename_no_replace(&prepared, &target).unwrap();
        }
        release.send(()).unwrap();
        assert_eq!(
            job.await.unwrap().status_code,
            Some(StatusCode::METHOD_NOT_ALLOWED)
        );
        assert_eq!(
            std::fs::read(target.join("member")).unwrap(),
            b"preserved collection"
        );
        assert_eq!(
            std::fs::read(fixture.path().join("retired")).unwrap(),
            b"initial file"
        );
        assert!(std::fs::read_dir(fixture.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".hpip-upload-")
        }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn streamed_upload_revalidates_current_alias_authority_and_policy() {
        use std::os::unix::fs::symlink;

        for policy in ["auth", "no-follow", "sandbox"] {
            let fixture = TestDir::new();
            let root = fixture.path().join("served");
            std::fs::create_dir_all(root.join("active/alias")).unwrap();
            std::fs::create_dir(root.join("prepared")).unwrap();
            let target = if policy == "sandbox" {
                fixture.path().join("outside")
            } else {
                root.join("private")
            };
            std::fs::create_dir(&target).unwrap();
            std::fs::write(target.join("file"), b"protected resource").unwrap();
            std::fs::write(root.join("active/alias/file"), b"initial resource").unwrap();
            symlink(&target, root.join("prepared/alias")).unwrap();
            let flags = match policy {
                "auth" => vec!["-w", "--path-auth", "private=user:pass"],
                "no-follow" => vec!["-w", "-s"],
                _ => vec!["-w", "-r"],
            };
            let service = Service::new(crate::build_router(config(&root, &flags)));
            let (request, entered, release) = paused_request("/active/alias/file");
            let job = tokio::spawn(async move { service.handle(request).await });
            tokio::time::timeout(std::time::Duration::from_secs(2), entered)
                .await
                .unwrap()
                .unwrap();
            {
                let _guard = MUTATION_LOCK.lock().await;
                crate::util::namespace::rename_no_replace(
                    &root.join("active"),
                    &root.join("retired"),
                )
                .unwrap();
                crate::util::namespace::copy_no_replace(
                    &root.join("prepared"),
                    &root.join("active"),
                    true,
                )
                .unwrap();
            }
            release.send(()).unwrap();
            assert_eq!(
                job.await.unwrap().status_code,
                Some(if policy == "auth" {
                    StatusCode::UNAUTHORIZED
                } else {
                    StatusCode::NOT_FOUND
                }),
                "{policy}"
            );
            assert_eq!(
                std::fs::read(target.join("file")).unwrap(),
                b"protected resource",
                "{policy}"
            );
            assert_eq!(
                std::fs::read(root.join("retired/alias/file")).unwrap(),
                b"initial resource",
                "{policy}"
            );
            assert_eq!(
                std::fs::read_dir(root.join("retired/alias"))
                    .unwrap()
                    .count(),
                1,
                "{policy}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn streamed_upload_publishes_to_the_current_authorized_alias_target() {
        use std::os::unix::fs::symlink;

        let fixture = TestDir::new();
        for name in ["active", "prepared", "original", "replacement"] {
            std::fs::create_dir(fixture.path().join(name)).unwrap();
        }
        std::fs::write(fixture.path().join("original/file"), b"original").unwrap();
        std::fs::write(fixture.path().join("replacement/file"), b"replacement").unwrap();
        symlink(
            fixture.path().join("original"),
            fixture.path().join("active/alias"),
        )
        .unwrap();
        symlink(
            fixture.path().join("replacement"),
            fixture.path().join("prepared/alias"),
        )
        .unwrap();
        let service = Service::new(crate::build_router(config(fixture.path(), &["-w"])));
        let (request, entered, release) = paused_request("/active/alias/file");
        let job = tokio::spawn(async move { service.handle(request).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), entered)
            .await
            .unwrap()
            .unwrap();
        {
            let _guard = MUTATION_LOCK.lock().await;
            crate::util::namespace::rename_no_replace(
                &fixture.path().join("active"),
                &fixture.path().join("retired"),
            )
            .unwrap();
            crate::util::namespace::copy_no_replace(
                &fixture.path().join("prepared"),
                &fixture.path().join("active"),
                true,
            )
            .unwrap();
        }
        release.send(()).unwrap();
        assert_eq!(job.await.unwrap().status_code, Some(StatusCode::NO_CONTENT));
        assert_eq!(
            std::fs::read(fixture.path().join("original/file")).unwrap(),
            b"original"
        );
        assert_eq!(
            std::fs::read(fixture.path().join("replacement/file")).unwrap(),
            b"complete upload"
        );
        assert_eq!(
            std::fs::read_dir(fixture.path().join("original"))
                .unwrap()
                .count(),
            1
        );
    }
}

#[cfg(test)]
mod upload_tests {
    use super::*;

    async fn write_file(destination: &Path, data: &[u8]) -> Result<bool, UploadError> {
        write_file_conditional(destination, data, &WriteConditions::default()).await
    }

    #[cfg(windows)]
    #[test]
    fn dropping_an_unconsumed_prepared_upload_removes_staging() {
        let directory = crate::test_support::TestDir::new();
        let destination = directory.path().join("file");
        let (pending, file) = PendingUpload::create(&destination).unwrap();
        let prepared = PreparedUpload {
            pending,
            file,
            target: UploadTarget::Filesystem(destination),
        };
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        drop(prepared);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
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
        assert!(pending.commit(&destination, false).is_err());
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
        let old_tag = path_metadata_and_etag(&destination).unwrap().1;
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
