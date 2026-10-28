//! Isolated filesystem fixtures and the same CLI configuration used in production.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use salvo::prelude::*;
use salvo::test::{ResponseExt, TestClient};

use crate::config::AppConfig;
use crate::options::Options;

static NEXT: AtomicU64 = AtomicU64::new(0);

pub(crate) struct TestDir {
    path: PathBuf,
}

impl TestDir {
    pub(crate) fn new() -> Self {
        let base = std::env::var_os("HPIP_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("hpip-tests");
        std::fs::create_dir_all(&base).unwrap();
        loop {
            let path = base.join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self { path },
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("cannot create test fixture: {e}"),
            }
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        // The path is created exclusively above; never remove an externally supplied root.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

pub(crate) fn config(root: &Path, flags: &[&str]) -> Arc<AppConfig> {
    let mut flags = flags.to_vec();
    flags.extend(["-t", root.to_str().unwrap(), "-q", "-q", "-q"]);
    Arc::new(AppConfig::new(&Options::parse_test(root, &flags).unwrap()))
}

#[tokio::test]
async fn serves_files_and_enforces_global_authentication() {
    let dir = TestDir::new();
    std::fs::write(dir.path().join("file.txt"), "fixture").unwrap();
    let service = Service::new(crate::build_router(config(dir.path(), &[])));
    let mut response = TestClient::get("http://localhost/file.txt")
        .send(&service)
        .await;
    assert_eq!(response.status_code, Some(StatusCode::OK));
    assert_eq!(response.take_string().await.unwrap(), "fixture");

    let service = Service::new(crate::build_router(config(
        dir.path(),
        &["--auth", "user:pass"],
    )));
    let response = TestClient::get("http://localhost/file.txt")
        .send(&service)
        .await;
    assert_eq!(response.status_code, Some(StatusCode::UNAUTHORIZED));
    let mut response = TestClient::get("http://localhost/file.txt")
        .add_header("Authorization", "Basic dXNlcjpwYXNz", true)
        .send(&service)
        .await;
    assert_eq!(response.status_code, Some(StatusCode::OK));
    assert_eq!(response.take_string().await.unwrap(), "fixture");
}
