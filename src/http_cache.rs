use chrono::{DateTime, Utc};
use salvo::prelude::*;

pub fn not_modified(
    req: &Request,
    res: &mut Response,
    etag: &str,
    modified: DateTime<Utc>,
) -> bool {
    let matches = if let Some(value) = req.headers().get("if-none-match") {
        value.to_str().is_ok_and(|value| {
            value.split(',').any(|tag| {
                let tag = tag.trim();
                tag == "*"
                    || tag.strip_prefix("W/").unwrap_or(tag)
                        == etag.strip_prefix("W/").unwrap_or(etag)
            })
        })
    } else {
        req.headers()
            .get("if-modified-since")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| DateTime::parse_from_rfc2822(s).ok())
            .is_some_and(|since| modified.timestamp() <= since.timestamp())
    };
    if matches {
        res.status_code(StatusCode::NOT_MODIFIED);
        res.headers_mut().insert("etag", etag.parse().unwrap());
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
    matches
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
    DateTime::parse_from_rfc2822(value).is_ok_and(|date| modified.timestamp() <= date.timestamp())
}

#[cfg(test)]
mod tests {
    use salvo::test::{ResponseExt, TestClient};

    use super::*;

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
}
