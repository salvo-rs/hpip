use std::sync::Arc;

use salvo::prelude::*;

use crate::config::AppConfig;
use crate::options::WebDavLevel;

/// Hoop that adds custom headers and DAV header to every response.
pub struct AdditionalHeadersHoop;

#[handler]
impl AdditionalHeadersHoop {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        ctrl.call_next(req, depot, res).await;

        if let Ok(config) = depot.get_typed::<Arc<AppConfig>>() {
            // Add DAV header if WebDAV is enabled
            if config.webdav >= WebDavLevel::All {
                res.headers_mut().insert("DAV", "1".parse().unwrap());
            }

            // Add custom headers
            for (name, value) in &config.additional_headers {
                if let Ok(hv) = salvo::http::HeaderValue::from_bytes(value)
                    && let Ok(hn) = salvo::http::HeaderName::from_bytes(name.as_bytes())
                {
                    res.headers_mut().append(hn, hv);
                }
            }
        }
    }
}
