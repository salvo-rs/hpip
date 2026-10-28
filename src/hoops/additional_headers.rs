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

        if matches!(req.method().as_str(), "GET" | "HEAD")
            && res.status_code.is_some_and(|s| s.is_success())
            && !res.headers().contains_key("content-encoding")
            && let Some(accept) = req
                .headers()
                .get("accept-encoding")
                .and_then(|h| h.to_str().ok())
            && !crate::encoding::identity_acceptable(accept)
        {
            res.status_code(StatusCode::NOT_ACCEPTABLE);
            res.body(salvo::http::ResBody::None);
            res.headers_mut().remove("content-length");
            res.headers_mut().remove("content-range");
        }

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

#[cfg(test)]
mod tests {
    use salvo::test::TestClient;

    use super::*;

    #[tokio::test]
    async fn rejects_an_identity_response_when_all_codings_are_forbidden() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("file.txt"), "small").unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &[],
        )));
        let response = TestClient::get("http://localhost/file.txt")
            .add_header("Accept-Encoding", "*;q=0", true)
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::NOT_ACCEPTABLE));
    }
}
