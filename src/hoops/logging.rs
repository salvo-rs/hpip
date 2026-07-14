use std::sync::Arc;

use salvo::prelude::*;

use crate::config::{AppConfig, log_msg};

/// Request logging hoop (logs method and URL for every request).
pub struct LoggingHoop;

#[handler]
impl LoggingHoop {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let method = req.method().to_string();
        let uri = req.uri().to_string();
        let remote = req.remote_addr().to_string();

        ctrl.call_next(req, depot, res).await;

        if let Ok(config) = depot.get_typed::<Arc<AppConfig>>() {
            let status = res.status_code.unwrap_or(StatusCode::OK);
            if config.log.0 {
                log_msg(
                    config.log,
                    &format!("{} {} {} -> {}", remote, method, uri, status.as_u16()),
                );
            }
        }
    }
}
