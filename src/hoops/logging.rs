use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use salvo::prelude::*;

use crate::config::{AppConfig, log_msg};

/// Request logging hoop (logs method and URL for every request).
pub struct LoggingHoop;
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

fn request_record(req: &Request, res: &Response, id: u64, elapsed: Duration) -> String {
    serde_json::json!({
        "request_id": format!("{}-{id}", std::process::id()),
        "remote": req.remote_addr().to_string(),
        "method": req.method().as_str(),
        "path": req.uri().path(),
        "status": res.status_code.map(|s| s.as_u16()),
        "handler_elapsed_us": elapsed.as_micros(),
        "expected_body_bytes": res.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()),
    }).to_string()
}

#[handler]
impl LoggingHoop {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let started = Instant::now();
        let id = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);

        ctrl.call_next(req, depot, res).await;

        if let Ok(config) = depot.get_typed::<Arc<AppConfig>>()
            && config.log.0
        {
            log_msg(config.log, &request_record(req, res, id, started.elapsed()));
        }
    }
}

#[cfg(test)]
mod tests {
    use salvo::test::TestClient;

    use super::*;

    #[test]
    fn logs_do_not_include_query_secrets_or_invent_a_status() {
        let req = TestClient::get("http://localhost/file?token=secret").build();
        let output = request_record(&req, &Response::new(), 1, Duration::from_millis(2));
        assert!(!output.contains("secret"));
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["path"], "/file");
        assert!(value["status"].is_null());
        assert_eq!(value["handler_elapsed_us"], 2000);
    }
}
