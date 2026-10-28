use chrono::{DateTime, Utc};
use salvo::prelude::*;

/// Evaluate file preconditions in RFC 9110 order before sending a body or Range.
/// Returns true when a final 304 or 412 response has been prepared.
pub fn preconditions(
    req: &Request,
    res: &mut Response,
    etag: Option<&str>,
    modified: DateTime<Utc>,
) -> bool {
    let failed = if let Some(value) = req.headers().get("if-match") {
        !value.to_str().is_ok_and(|value| {
            value.split(',').any(|tag| {
                let tag = tag.trim();
                tag == "*"
                    || (!tag.starts_with("W/")
                        && etag.is_some_and(|etag| !etag.starts_with("W/") && tag == etag))
            })
        })
    } else {
        req.headers()
            .get("if-unmodified-since")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| DateTime::parse_from_rfc2822(value).ok())
            .is_some_and(|date| modified.timestamp() > date.timestamp())
    };
    let matches = if let Some(value) = req.headers().get("if-none-match") {
        value.to_str().is_ok_and(|value| {
            value.split(',').any(|tag| {
                let tag = tag.trim();
                tag == "*"
                    || etag.is_some_and(|etag| {
                        tag.strip_prefix("W/").unwrap_or(tag)
                            == etag.strip_prefix("W/").unwrap_or(etag)
                    })
            })
        })
    } else {
        req.headers()
            .get("if-modified-since")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| DateTime::parse_from_rfc2822(s).ok())
            .is_some_and(|since| modified.timestamp() <= since.timestamp())
    };
    if failed || matches {
        res.status_code(if failed {
            StatusCode::PRECONDITION_FAILED
        } else {
            StatusCode::NOT_MODIFIED
        });
        if let Some(etag) = etag {
            res.headers_mut().insert("etag", etag.parse().unwrap());
        }
        res.headers_mut().insert(
            "last-modified",
            modified
                .format("%a, %d %b %Y %T GMT")
                .to_string()
                .parse()
                .unwrap(),
        );
        res.headers_mut().remove("content-length");
        res.body(salvo::http::ResBody::None);
    }
    failed || matches
}

pub fn range_matches(req: &Request, etag: &str, modified: DateTime<Utc>) -> bool {
    let Some(value) = req.headers().get("if-range") else {
        return true;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    if value.starts_with('"') {
        return value == etag;
    }
    DateTime::parse_from_rfc2822(value).is_ok_and(|date| modified.timestamp() == date.timestamp())
}

#[cfg(test)]
mod tests {
    use salvo::test::{ResponseExt, TestClient};

    use super::*;

    #[tokio::test]
    async fn unsupported_ranges_fall_back_to_a_full_conditional_response() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("file.txt"), "fixture").unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &[],
        )));
        for range in ["items=0-2", "bytes=0-1,3-4"] {
            let mut response = TestClient::get("http://localhost/file.txt")
                .add_header("Range", range, true)
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::OK));
            assert!(!response.headers().contains_key("content-range"));
            assert_eq!(response.take_string().await.unwrap(), "fixture");
            let response = TestClient::get("http://localhost/file.txt")
                .add_header("Range", range, true)
                .add_header("If-None-Match", "*", true)
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::NOT_MODIFIED));
        }
        let response = TestClient::get("http://localhost/file.txt")
            .add_header("Range", "bytes=99-", true)
            .send(&service)
            .await;
        assert_eq!(
            response.status_code,
            Some(StatusCode::RANGE_NOT_SATISFIABLE)
        );
        assert_eq!(response.headers()["content-range"], "bytes */7");
    }

    #[tokio::test]
    async fn failed_read_preconditions_precede_cache_and_range_responses() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("file.txt"), "fixture").unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &[],
        )));
        let current = TestClient::get("http://localhost/file.txt")
            .send(&service)
            .await;
        let tag = current.headers()["etag"].to_str().unwrap();
        for method in ["GET", "HEAD"] {
            for headers in [
                vec![("If-Match", "\"obsolete\"")],
                vec![("If-Unmodified-Since", "Wed, 01 Jan 2020 00:00:00 GMT")],
                vec![("If-Match", "\"obsolete\""), ("If-None-Match", "*")],
                vec![
                    ("If-Unmodified-Since", "Wed, 01 Jan 2020 00:00:00 GMT"),
                    ("If-Modified-Since", "Wed, 01 Jan 2031 00:00:00 GMT"),
                ],
                vec![("If-Match", "\"obsolete\""), ("Range", "bytes=0-2")],
            ] {
                let mut builder = if method == "GET" {
                    TestClient::get("http://localhost/file.txt")
                } else {
                    TestClient::head("http://localhost/file.txt")
                };
                for (name, value) in headers {
                    builder = builder.add_header(name, value, true);
                }
                let mut response = builder.send(&service).await;
                assert_eq!(
                    response.status_code,
                    Some(StatusCode::PRECONDITION_FAILED),
                    "{method}"
                );
                assert!(!response.headers().contains_key("content-range"));
                assert_ne!(
                    response.take_bytes(None).await.unwrap().as_ref(),
                    b"fixture"
                );
            }
        }
        for condition in [
            tag.to_owned(),
            format!("\"obsolete\", {tag}"),
            "*".to_owned(),
        ] {
            let mut response = TestClient::get("http://localhost/file.txt")
                .add_header("If-Match", condition, true)
                .add_header("If-Unmodified-Since", "Wed, 01 Jan 2020 00:00:00 GMT", true)
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::OK));
            assert_eq!(response.take_string().await.unwrap(), "fixture");
        }
        let response = TestClient::get("http://localhost/file.txt")
            .add_header("If-Match", format!("W/{tag}"), true)
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::PRECONDITION_FAILED));
    }

    #[tokio::test]
    async fn conditional_cold_head_resolves_the_selected_strong_validator() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("file.txt"), "compressible\n".repeat(2000)).unwrap();
        let warm = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &[],
        )));
        let get = TestClient::get("http://localhost/file.txt")
            .add_header("Accept-Encoding", "gzip", true)
            .send(&warm)
            .await;
        let tag = get.headers()["etag"].to_str().unwrap();
        let cold = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &[],
        )));
        let mut head = TestClient::head("http://localhost/file.txt")
            .add_header("Accept-Encoding", "gzip", true)
            .add_header("If-Match", tag, true)
            .send(&cold)
            .await;
        assert_eq!(head.status_code, Some(StatusCode::OK));
        assert_eq!(head.headers()["etag"], tag);
        assert!(head.take_bytes(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn if_range_requires_the_current_date_or_a_strong_entity_tag() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("file.txt"), "fixture").unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &[],
        )));
        let full = TestClient::get("http://localhost/file.txt")
            .send(&service)
            .await;
        let date = full.headers()["last-modified"].to_str().unwrap();
        let tag = full.headers()["etag"].to_str().unwrap();
        for validator in [date.to_owned(), tag.to_owned()] {
            let mut response = TestClient::get("http://localhost/file.txt")
                .add_header("Range", "bytes=0-2", true)
                .add_header("If-Range", validator, true)
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::PARTIAL_CONTENT));
            assert_eq!(response.take_string().await.unwrap(), "fix");
        }
        for validator in [
            "Wed, 01 Jan 2031 00:00:00 GMT".to_owned(),
            "Wed, 01 Jan 2020 00:00:00 GMT".to_owned(),
            format!("W/{tag}"),
            "invalid date".to_owned(),
        ] {
            let mut response = TestClient::get("http://localhost/file.txt")
                .add_header("Range", "bytes=0-2", true)
                .add_header("If-Range", validator, true)
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::OK));
            assert_eq!(response.take_string().await.unwrap(), "fixture");
        }
    }

    #[tokio::test]
    async fn preconditions_use_exact_tags_and_ignore_dates_when_tags_are_present() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("file.txt"), "fixture").unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &[],
        )));
        let r = TestClient::get("http://localhost/file.txt")
            .add_header("If-None-Match", "*", true)
            .send(&service)
            .await;
        assert_eq!(r.status_code, Some(StatusCode::NOT_MODIFIED));
        let r = TestClient::get("http://localhost/file.txt")
            .add_header("If-None-Match", "\"different\"", true)
            .add_header("If-Modified-Since", "Wed, 01 Jan 2031 00:00:00 GMT", true)
            .send(&service)
            .await;
        assert_eq!(r.status_code, Some(StatusCode::OK));
        let mut r = TestClient::get("http://localhost/file.txt")
            .add_header("Range", "bytes=0-2", true)
            .add_header("If-Range", "\"obsolete\"", true)
            .send(&service)
            .await;
        assert_eq!(r.status_code, Some(StatusCode::OK));
        assert_eq!(r.take_string().await.unwrap(), "fixture");
    }

    #[tokio::test]
    async fn encoded_representations_have_distinct_validators_and_vary() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("file.txt"), "compress me\n".repeat(2000)).unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &[],
        )));
        let plain = TestClient::get("http://localhost/file.txt")
            .send(&service)
            .await;
        let gzip = TestClient::get("http://localhost/file.txt")
            .add_header("Accept-Encoding", "gzip", true)
            .send(&service)
            .await;
        let tag = gzip.headers().get("etag").unwrap().to_str().unwrap();
        assert_ne!(plain.headers().get("etag"), gzip.headers().get("etag"));
        assert!(
            gzip.headers()
                .get("vary")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("Accept-Encoding")
        );
        let r = TestClient::get("http://localhost/file.txt")
            .add_header("Accept-Encoding", "gzip", true)
            .add_header("If-None-Match", tag, true)
            .send(&service)
            .await;
        assert_eq!(r.status_code, Some(StatusCode::NOT_MODIFIED));
    }

    #[tokio::test]
    async fn ranges_reuse_the_full_representation_validator() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("file.txt"), "fixture").unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &[],
        )));
        let full = TestClient::get("http://localhost/file.txt")
            .send(&service)
            .await;
        let tag = full.headers()["etag"].to_str().unwrap();
        let mut partial = TestClient::get("http://localhost/file.txt")
            .add_header("Range", "bytes=0-2", true)
            .add_header("If-Range", tag, true)
            .send(&service)
            .await;
        assert_eq!(partial.status_code, Some(StatusCode::PARTIAL_CONTENT));
        assert_eq!(partial.headers()["etag"], full.headers()["etag"]);
        assert_eq!(partial.take_string().await.unwrap(), "fix");
    }
}
