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

        if matches!(req.method().as_str(), "GET" | "HEAD") {
            res.headers_mut()
                .append("vary", "Accept, Accept-Encoding".parse().unwrap());
        }

        if matches!(req.method().as_str(), "GET" | "HEAD")
            && res.status_code.is_some_and(|s| s.is_success())
            && !res.headers().contains_key("content-encoding")
            && depot
                .get_typed::<crate::handler::head::UnresolvedFileEncoding>()
                .is_err()
            && let Some(accept) = crate::encoding::request_accept_encoding(req)
            && !crate::encoding::identity_acceptable(&accept)
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
    use salvo::test::{ResponseExt, TestClient};

    use super::*;

    #[tokio::test]
    async fn repeated_encoding_fields_match_the_combined_value_for_files_and_listings() {
        use std::io::Read;

        let dir = crate::test_support::TestDir::new();
        let contents = "repeated coding fields\n".repeat(2000);
        std::fs::write(dir.path().join("file.txt"), &contents).unwrap();
        std::fs::write(dir.path().join("small.txt"), "small").unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &[],
        )));
        for fields in [vec!["identity;q=0", "gzip"], vec!["identity;q=0,gzip"]] {
            for method in ["GET", "HEAD"] {
                let mut request = salvo::test::RequestBuilder::new(
                    "http://localhost/file.txt",
                    method.parse().unwrap(),
                );
                for value in &fields {
                    request = request.add_header("Accept-Encoding", *value, false);
                }
                let mut response = request.send(&service).await;
                assert_eq!(response.status_code, Some(StatusCode::OK));
                assert_eq!(response.headers()["content-encoding"], "gzip");
                if method == "GET" {
                    let bytes = response.take_bytes(None).await.unwrap();
                    let mut decoded = String::new();
                    flate2::read::GzDecoder::new(bytes.as_ref())
                        .read_to_string(&mut decoded)
                        .unwrap();
                    assert_eq!(decoded, contents);
                }
            }
            let mut request = TestClient::get("http://localhost/");
            for value in fields {
                request = request.add_header("Accept-Encoding", value, false);
            }
            let response = request.send(&service).await;
            assert_eq!(response.status_code, Some(StatusCode::OK));
            assert_eq!(response.headers()["content-encoding"], "gzip");
        }
        for request in [
            TestClient::get("http://localhost/small.txt"),
            TestClient::head("http://localhost/small.txt"),
        ] {
            let response = request
                .add_header("Accept-Encoding", "gzip", false)
                .add_header("Accept-Encoding", "identity;q=0", false)
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::NOT_ACCEPTABLE));
        }
    }

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
