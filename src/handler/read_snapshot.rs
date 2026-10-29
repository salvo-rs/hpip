//! Select and open a resource before server-owned namespace mutations can rebind it.

use std::fs::{File, Metadata, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use salvo::prelude::*;

use crate::config::AppConfig;
use crate::util::write_conditions::MUTATION_LOCK;

pub(super) fn request(req: &Request) -> Request {
    let mut snapshot = Request::new();
    *snapshot.uri_mut() = req.uri().clone();
    *snapshot.method_mut() = req.method().clone();
    *snapshot.headers_mut() = req.headers().clone();
    *snapshot.remote_addr_mut() = req.remote_addr().clone();
    snapshot
}

pub(super) async fn run<R: Send + 'static>(
    req: &Request,
    config: &Arc<AppConfig>,
    work: impl FnOnce(&mut Request, &Arc<AppConfig>) -> R + Send + 'static,
) -> Result<R, StatusCode> {
    let mut req = request(req);
    let work_config = config.clone();
    // Dropping the request future closes this channel, including while the
    // blocking worker is waiting for an unrelated namespace transaction.
    let (cancel_on_drop, cancelled) = tokio::sync::oneshot::channel::<()>();
    let runtime = tokio::runtime::Handle::current();
    let job = config
        .spawn_read_snapshot(move || {
            let guard = runtime.block_on(async {
                tokio::select! {
                    biased;
                    _ = cancelled => None,
                    guard = MUTATION_LOCK.lock() => Some(guard),
                }
            });
            let Some(_guard) = guard else {
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            };
            Ok(work(&mut req, &work_config))
        })
        .await
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let result = job.await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    drop(cancel_on_drop);
    result
}

pub(super) struct OpenedFile {
    pub path: PathBuf,
    pub source: File,
    pub metadata: Metadata,
    pub mime: String,
}

impl OpenedFile {
    /// Call only while holding the namespace lock. NONBLOCK prevents a FIFO
    /// without a writer from holding that lock indefinitely during open.
    pub fn open(config: &AppConfig, path: &Path) -> std::io::Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let source = options.open(path)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // The owned descriptor is valid for both fcntl calls. No other
            // descriptor or thread can change its flags during construction.
            let flags = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_GETFL) };
            if flags < 0
                || unsafe {
                    libc::fcntl(source.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK)
                } < 0
            {
                return Err(std::io::Error::last_os_error());
            }
        }
        let metadata = source.metadata()?;
        let mime = crate::util::guess_mime_type(path, &config.mime_type_overrides);
        Ok(Self {
            path: path.to_owned(),
            source,
            metadata,
            mime,
        })
    }
}

#[cfg(test)]
mod tests {
    use salvo::test::{ResponseExt, TestClient};

    use super::*;

    #[tokio::test]
    async fn published_source_and_mime_stay_pinned_without_holding_the_namespace_lock() {
        let dir = crate::test_support::TestDir::new();
        let path = dir.path().join("data");
        let replacement = dir.path().join("replacement");
        let content = b"published text fixture\n".repeat(20_000);
        std::fs::write(&path, b"initial").unwrap();
        std::fs::write(&replacement, &content).unwrap();
        let config = crate::test_support::config(dir.path(), &["-e"]);
        let service = Service::new(crate::build_router(config.clone()));
        let guard = MUTATION_LOCK.lock().await;
        let request = tokio::spawn(async move {
            TestClient::get("http://localhost/data")
                .send(&service)
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while config.read_snapshot_jobs.available_permits() == 4 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!request.is_finished());
        std::fs::rename(&replacement, &path).unwrap();
        drop(guard);
        let mut response = request.await.unwrap();
        assert_eq!(response.status_code, Some(StatusCode::OK));
        assert_eq!(response.headers()["content-type"], "text/plain");
        // A large response is still waiting for its body consumer. Publication
        // can proceed and must not change either the source bytes or MIME.
        let _guard = tokio::time::timeout(std::time::Duration::from_secs(2), MUTATION_LOCK.lock())
            .await
            .unwrap();
        std::fs::write(&replacement, [0_u8; 100]).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert_eq!(response.take_bytes(None).await.unwrap().as_ref(), content);
        assert_eq!(config.read_snapshot_jobs.available_permits(), 4);
    }

    #[tokio::test]
    async fn snapshot_capacity_is_bounded_and_independent_of_other_work() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("data.txt"), "fixture").unwrap();
        let config = crate::test_support::config(dir.path(), &["-e"]);
        let service = Service::new(crate::build_router(config.clone()));
        let _general = config
            .blocking_jobs
            .clone()
            .acquire_many_owned(4)
            .await
            .unwrap();
        let _archives = config
            .archive_jobs
            .clone()
            .acquire_many_owned(4)
            .await
            .unwrap();
        for request in [
            TestClient::get("http://localhost/data.txt"),
            TestClient::head("http://localhost/data.txt"),
        ] {
            assert_eq!(
                request.send(&service).await.status_code,
                Some(StatusCode::OK)
            );
        }
        let snapshots = config
            .read_snapshot_jobs
            .clone()
            .acquire_many_owned(4)
            .await
            .unwrap();
        let mut pending = Vec::new();
        for head in [false, true] {
            let request_config = config.clone();
            pending.push(tokio::spawn(async move {
                let service = Service::new(crate::build_router(request_config));
                let request = if head {
                    TestClient::head("http://localhost/data.txt")
                } else {
                    TestClient::get("http://localhost/data.txt")
                };
                request.send(&service).await
            }));
        }
        for request in &mut pending {
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(10), request)
                    .await
                    .is_err()
            );
        }
        drop(snapshots);
        for request in pending {
            assert_eq!(request.await.unwrap().status_code, Some(StatusCode::OK));
        }
        assert_eq!(
            TestClient::head("http://localhost/data.txt")
                .send(&service)
                .await
                .status_code,
            Some(StatusCode::OK)
        );
    }

    #[tokio::test]
    async fn cancelling_a_waiting_snapshot_releases_its_worker_without_the_namespace_lock() {
        let dir = crate::test_support::TestDir::new();
        let config = crate::test_support::config(dir.path(), &[]);
        let _guard = MUTATION_LOCK.lock().await;
        let request_config = config.clone();
        let request = tokio::spawn(async move {
            let req = Request::new();
            run(&req, &request_config, |_, _| {
                panic!("cancelled work must not run")
            })
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while config.read_snapshot_jobs.available_permits() == 4 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        let _permits = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            config.read_snapshot_jobs.clone().acquire_many_owned(4),
        )
        .await
        .unwrap()
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn opening_a_fifo_without_a_writer_does_not_stall_the_namespace_lock() {
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;
        let dir = crate::test_support::TestDir::new();
        let path = dir.path().join("fifo");
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // The NUL-terminated path is owned throughout the call.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let config = crate::test_support::config(dir.path(), &[]);
        let req = Request::new();
        let opened = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            run(&req, &config, move |_, config| {
                OpenedFile::open(config, &path)
            }),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(
            unsafe { libc::fcntl(opened.source.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );
        let _guard = tokio::time::timeout(std::time::Duration::from_secs(2), MUTATION_LOCK.lock())
            .await
            .unwrap();
    }
}
