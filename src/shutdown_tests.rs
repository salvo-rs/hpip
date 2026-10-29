use std::sync::Arc;
use std::time::Duration;

use salvo::conn::Acceptor;
use salvo::prelude::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

struct DelayedResponse {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[handler]
impl DelayedResponse {
    async fn handle(&self, res: &mut Response) {
        self.entered.notify_one();
        self.release.notified().await;
        res.render("completed before cleanup");
    }
}

#[tokio::test]
async fn graceful_shutdown_waits_for_an_inflight_response() {
    let acceptor = TcpListener::new("127.0.0.1:0").try_bind().await.unwrap();
    let address = acceptor.holdings()[0]
        .local_addr
        .clone()
        .into_std()
        .unwrap();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let router = Router::new().get(DelayedResponse {
        entered: entered.clone(),
        release: release.clone(),
    });
    let server = Server::new(acceptor);
    let handle = server.handle();
    let task = tokio::spawn(server.try_serve(router));
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    handle.stop_graceful(Some(Duration::from_secs(3)));
    assert!(!task.is_finished());
    release.notify_one();
    let mut response = String::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_string(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(response.ends_with("completed before cleanup"));
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn cleanup_drain_waits_for_an_inflight_handler_without_a_worker() {
    let directory = crate::test_support::TestDir::new();
    let config = crate::test_support::config(directory.path(), &[]);
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let service = Arc::new(Service::new(
        Router::new()
            .hoop(crate::InjectConfig {
                config: config.clone(),
            })
            .goal(DelayedResponse {
                entered: entered.clone(),
                release: release.clone(),
            }),
    ));
    let request_service = service.clone();
    let request = tokio::spawn(async move {
        salvo::test::TestClient::get("http://localhost/")
            .send(&*request_service)
            .await
    });
    entered.notified().await;
    let drain_config = config.clone();
    let mut drain = tokio::spawn(async move { drain_config.drain_blocking_jobs().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut drain)
            .await
            .is_err(),
        "cleanup must not race a request that has not spawned a blocking job"
    );
    release.notify_one();
    assert_eq!(request.await.unwrap().status_code, Some(StatusCode::OK));
    tokio::time::timeout(Duration::from_secs(2), drain)
        .await
        .unwrap()
        .unwrap();
    let response = salvo::test::TestClient::get("http://localhost/")
        .send(&*service)
        .await;
    assert_eq!(response.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
}

#[tokio::test]
async fn cancellation_releases_the_last_request_before_cleanup_drain_finishes() {
    let directory = crate::test_support::TestDir::new();
    let config = crate::test_support::config(directory.path(), &[]);
    let entered = Arc::new(Notify::new());
    let service = Service::new(
        Router::new()
            .hoop(crate::InjectConfig {
                config: config.clone(),
            })
            .goal(DelayedResponse {
                entered: entered.clone(),
                release: Arc::new(Notify::new()),
            }),
    );
    let request = tokio::spawn(async move {
        salvo::test::TestClient::get("http://localhost/")
            .send(&service)
            .await
    });
    entered.notified().await;
    let drain_config = config.clone();
    let mut drain = tokio::spawn(async move { drain_config.drain_blocking_jobs().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut drain)
            .await
            .is_err()
    );
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(2), drain)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn request_exit_wakes_every_cleanup_waiter_and_closing_rejects_new_requests() {
    let tracker = Arc::new(crate::request_lifecycle::RequestTracker::default());
    let request = tracker.enter().unwrap();
    tracker.close();
    assert!(tracker.enter().is_none());
    let mut waiters = Vec::new();
    for _ in 0..4 {
        let tracker = tracker.clone();
        waiters.push(tokio::spawn(async move { tracker.wait_idle().await }));
    }
    for waiter in &mut waiters {
        assert!(
            tokio::time::timeout(Duration::from_millis(10), waiter)
                .await
                .is_err()
        );
    }
    drop(request);
    for waiter in waiters {
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .unwrap()
            .unwrap();
    }
    // Waiting after the final exit must not require another notification.
    tokio::time::timeout(Duration::from_secs(2), tracker.wait_idle())
        .await
        .unwrap();
}

#[tokio::test]
async fn forced_connection_shutdown_drains_a_partially_received_upload() {
    let directory = crate::test_support::TestDir::new();
    let destination = directory.path().join("file.txt");
    std::fs::write(&destination, b"original").unwrap();
    let config = crate::test_support::config(directory.path(), &["-w"]);
    let acceptor = TcpListener::new("127.0.0.1:0").try_bind().await.unwrap();
    let address = acceptor.holdings()[0]
        .local_addr
        .clone()
        .into_std()
        .unwrap();
    let server = Server::new(acceptor);
    let handle = server.handle();
    let server_task = tokio::spawn(server.try_serve(crate::build_router(config.clone())));
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream
        .write_all(
            b"PUT /file.txt HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2048\r\n\r\npartial",
        )
        .await
        .unwrap();
    let has_staging = || {
        std::fs::read_dir(directory.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".hpip-upload-")
        })
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while !has_staging() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    handle.stop_graceful(Some(Duration::from_millis(20)));
    tokio::time::timeout(Duration::from_secs(3), server_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), config.drain_blocking_jobs())
        .await
        .unwrap();
    assert!(
        !has_staging(),
        "handler cleanup must finish before temporary-directory cleanup"
    );
    assert_eq!(std::fs::read(destination).unwrap(), b"original");
}
