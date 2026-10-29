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
    run_in_pool(
        req,
        config,
        move |req, config, _guard| work(req, config),
        std::time::Duration::from_secs(30),
        SnapshotPool::Read,
    )
    .await
}

/// Metadata keeps its existing general-worker admission. The collector owns the
/// namespace guard so it can release it before rendering or compression.
pub(super) async fn run_metadata<R: Send + 'static>(
    req: &Request,
    config: &Arc<AppConfig>,
    work: impl FnOnce(&mut Request, &Arc<AppConfig>, tokio::sync::MutexGuard<'static, ()>) -> R
    + Send
    + 'static,
) -> Result<R, StatusCode> {
    run_in_pool(
        req,
        config,
        work,
        std::time::Duration::from_secs(30),
        SnapshotPool::Metadata,
    )
    .await
}

enum SnapshotPool {
    Read,
    Metadata,
}

async fn run_in_pool<R: Send + 'static>(
    req: &Request,
    config: &Arc<AppConfig>,
    work: impl FnOnce(&mut Request, &Arc<AppConfig>, tokio::sync::MutexGuard<'static, ()>) -> R
    + Send
    + 'static,
    timeout: std::time::Duration,
    pool: SnapshotPool,
) -> Result<R, StatusCode> {
    let deadline = tokio::time::Instant::now() + timeout;
    tokio::time::timeout_at(deadline, async {
        let mut req = request(req);
        let work_config = config.clone();
        // Dropping the request future closes this channel, including while the
        // blocking worker is waiting for an unrelated namespace transaction.
        let (cancel_on_drop, cancelled) = tokio::sync::oneshot::channel::<()>();
        let runtime = tokio::runtime::Handle::current();
        let worker = move || {
            let guard = runtime.block_on(async {
                tokio::select! {
                    biased;
                    _ = cancelled => None,
                    _ = work_config.wait_for_shutdown() => None,
                    _ = tokio::time::sleep_until(deadline) => None,
                    guard = MUTATION_LOCK.lock() => Some(guard),
                }
            });
            let Some(guard) = guard else {
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            };
            if work_config.is_shutting_down() || tokio::time::Instant::now() >= deadline {
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            }
            Ok(work(&mut req, &work_config, guard))
        };
        let job = match pool {
            SnapshotPool::Read => config.spawn_read_snapshot(worker).await,
            SnapshotPool::Metadata => config.spawn_blocking(worker),
        }
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
        let result = job.await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        drop(cancel_on_drop);
        result
    })
    .await
    .unwrap_or(Err(StatusCode::SERVICE_UNAVAILABLE))
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
    use salvo::test::{RequestBuilder, ResponseExt, TestClient};

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

    #[tokio::test]
    async fn shutdown_releases_an_admitted_snapshot_waiting_for_the_namespace() {
        let dir = crate::test_support::TestDir::new();
        let config = crate::test_support::config(dir.path(), &[]);
        let guard = MUTATION_LOCK.lock().await;
        let request_config = config.clone();
        let mut request = tokio::spawn(async move {
            let req = Request::new();
            run(&req, &request_config, |_, _| ()).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while config.read_snapshot_jobs.available_permits() == 4 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let drain_config = config.clone();
        let drain = tokio::spawn(async move { drain_config.drain_blocking_jobs().await });
        let result =
            tokio::time::timeout(std::time::Duration::from_millis(200), &mut request).await;
        let stopped_without_namespace = result.is_ok();
        if let Ok(result) = result {
            assert_eq!(result.unwrap(), Err(StatusCode::SERVICE_UNAVAILABLE));
        }
        // Always release the writer and join admitted work, including when this
        // regression is run against a version without shutdown cancellation.
        drop(guard);
        if !stopped_without_namespace {
            request.await.unwrap().unwrap();
        }
        drain.await.unwrap();
        assert!(
            stopped_without_namespace,
            "shutdown waited for an unrelated namespace writer"
        );
    }

    #[tokio::test]
    async fn snapshot_deadline_covers_the_admitted_namespace_wait() {
        let dir = crate::test_support::TestDir::new();
        let config = crate::test_support::config(dir.path(), &[]);
        let _guard = MUTATION_LOCK.lock().await;
        let request = Request::new();
        for pool in [SnapshotPool::Read, SnapshotPool::Metadata] {
            let semaphore = match pool {
                SnapshotPool::Read => config.read_snapshot_jobs.clone(),
                SnapshotPool::Metadata => config.blocking_jobs.clone(),
            };
            let result = run_in_pool(
                &request,
                &config,
                |_, _, _| panic!("expired snapshot must not start"),
                std::time::Duration::from_millis(20),
                pool,
            )
            .await;
            assert_eq!(result, Err(StatusCode::SERVICE_UNAVAILABLE));
            let _permits = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                semaphore.acquire_many_owned(4),
            )
            .await
            .unwrap()
            .unwrap();
        }
    }

    #[tokio::test]
    async fn cancelled_running_snapshots_remain_owned_until_the_worker_exits() {
        let dir = crate::test_support::TestDir::new();
        let config = crate::test_support::config(dir.path(), &[]);
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, waiting) = std::sync::mpsc::channel();
        let request_config = config.clone();
        let request = tokio::spawn(async move {
            let request = Request::new();
            run(&request, &request_config, move |_, _| {
                started.send(()).unwrap();
                waiting.recv().unwrap();
            })
            .await
        });
        ready.await.unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert_eq!(config.read_snapshot_jobs.available_permits(), 3);
        let drain_config = config.clone();
        let drain = tokio::spawn(async move { drain_config.drain_blocking_jobs().await });
        while !config.is_shutting_down() {
            tokio::task::yield_now().await;
        }
        assert!(!drain.is_finished());
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), drain)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn delayed_json_and_propfind_waiters_release_capacity_on_cancel_or_shutdown() {
        for propfind in [false, true] {
            for shutdown in [false, true] {
                let dir = crate::test_support::TestDir::new();
                std::fs::write(dir.path().join("file.txt"), "fixture").unwrap();
                let config = crate::test_support::config(dir.path(), &["-d"]);
                let guard = MUTATION_LOCK.lock().await;
                let request_config = config.clone();
                let mut request = tokio::spawn(async move {
                    if propfind {
                        let service = Service::new(crate::build_router(request_config));
                        RequestBuilder::new(
                            "http://localhost/file.txt",
                            "PROPFIND".parse().unwrap(),
                        )
                        .add_header("Depth", "0", true)
                        .send(&service)
                        .await
                    } else {
                        let mut request = TestClient::get("http://localhost/file.txt").build();
                        let mut response = Response::new();
                        crate::handler::rfsapi::handle_rfsapi_inner(
                            &mut request,
                            &mut response,
                            &request_config,
                        )
                        .await;
                        response
                    }
                });
                tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    while config.blocking_jobs.available_permits() == 4 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                if shutdown {
                    config.close_admission();
                    let response =
                        tokio::time::timeout(std::time::Duration::from_secs(2), &mut request)
                            .await
                            .unwrap()
                            .unwrap();
                    assert_eq!(response.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
                } else {
                    request.abort();
                    assert!(request.await.unwrap_err().is_cancelled());
                }
                let _permits = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    config.blocking_jobs.clone().acquire_many_owned(4),
                )
                .await
                .unwrap()
                .unwrap();
                drop(guard);
            }
        }
    }

    #[tokio::test]
    async fn already_expired_snapshots_never_start_namespace_work() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = crate::test_support::TestDir::new();
        let config = crate::test_support::config(dir.path(), &[]);
        for pool in [SnapshotPool::Read, SnapshotPool::Metadata] {
            let started = Arc::new(AtomicBool::new(false));
            let marker = started.clone();
            let request = Request::new();
            let result = run_in_pool(
                &request,
                &config,
                move |_, _, _| marker.store(true, Ordering::SeqCst),
                std::time::Duration::ZERO,
                pool,
            )
            .await;
            assert_eq!(result, Err(StatusCode::SERVICE_UNAVAILABLE));
            // Join all admitted work even if the response deadline fired first.
            let _reads = config
                .read_snapshot_jobs
                .clone()
                .acquire_many_owned(4)
                .await
                .unwrap();
            let _metadata = config
                .blocking_jobs
                .clone()
                .acquire_many_owned(4)
                .await
                .unwrap();
            assert!(!started.load(Ordering::SeqCst));
        }
    }
}
