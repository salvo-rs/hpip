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
