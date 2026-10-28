use chrono::{DateTime, Utc};
use salvo::prelude::*;

use crate::util::entity_tag::{EntityTagList, request_tag_field};
use crate::util::http_date::request_date;

/// Evaluate file preconditions in RFC 9110 order before sending a body or Range.
/// Returns true when a final 304 or 412 response has been prepared.
pub fn preconditions(
    req: &Request,
    res: &mut Response,
    etag: Option<&str>,
    modified: DateTime<Utc>,
) -> bool {
    evaluate_preconditions(req, res, etag, Some(modified))
}

/// Evaluate conditions for an existing generated representation without a stable
/// validator. A source file's ETag or timestamp cannot identify JSON or an archive.
pub fn unversioned_preconditions(req: &Request, res: &mut Response) -> bool {
    if !res.status_code.is_some_and(|status| status.is_success()) {
        return false;
    }
    if matches!(req.method().as_str(), "GET" | "HEAD")
        && !res.headers().contains_key("content-encoding")
        && crate::encoding::request_accept_encoding(req)
            .is_some_and(|accept| !crate::encoding::identity_acceptable(&accept))
    {
        res.status_code(StatusCode::NOT_ACCEPTABLE);
        res.headers_mut().remove("content-length");
        res.body(salvo::http::ResBody::None);
        return true;
    }
    evaluate_preconditions(req, res, None, None)
}

fn evaluate_preconditions(
    req: &Request,
    res: &mut Response,
    etag: Option<&str>,
    modified: Option<DateTime<Utc>>,
) -> bool {
    let matches_tag_field = |value: &str, weak| {
        match EntityTagList::parse(value) {
            // This function is called only after selecting an existing resource;
            // a cold HEAD can know it exists without knowing its encoded ETag.
            Some(EntityTagList::Any) => true,
            Some(tags) => tags.matches(etag, weak),
            None => false,
        }
    };
    let failed = match request_tag_field(req, "if-match") {
        Ok(Some(value)) => !matches_tag_field(&value, false),
        Err(_) => true,
        Ok(None) => modified.is_some_and(|modified| {
            request_date(req, "if-unmodified-since")
                .is_some_and(|date| modified.timestamp() > date.timestamp())
        }),
    };
    let matches = match request_tag_field(req, "if-none-match") {
        Ok(Some(value)) => matches_tag_field(&value, true),
        Err(_) => false,
        Ok(None) => modified.is_some_and(|modified| {
            request_date(req, "if-modified-since")
                .is_some_and(|since| modified.timestamp() <= since.timestamp())
        }),
    };
    if failed || matches {
        res.status_code(
            if failed || !matches!(req.method().as_str(), "GET" | "HEAD") {
                StatusCode::PRECONDITION_FAILED
            } else {
                StatusCode::NOT_MODIFIED
            },
        );
        if let Some(etag) = etag {
            res.headers_mut().insert("etag", etag.parse().unwrap());
        }
        if let Some(modified) = modified {
            res.headers_mut().insert(
                "last-modified",
                modified
                    .format("%a, %d %b %Y %T GMT")
                    .to_string()
                    .parse()
                    .unwrap(),
            );
        }
        res.headers_mut().remove("content-length");
        if res.status_code == Some(StatusCode::PRECONDITION_FAILED) {
            // The default error catcher can create a new, unencoded body.
            res.headers_mut().remove("content-encoding");
            res.headers_mut().remove("content-range");
        }
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
    async fn generated_coding_metadata_is_cleared_on_failure_but_retained_on_304() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("file.txt"), "fixture").unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &[],
        )));
        for method in ["GET", "HEAD"] {
            let mut response =
                salvo::test::RequestBuilder::new("http://localhost/", method.parse().unwrap())
                    .add_header("Accept-Encoding", "gzip", true)
                    .add_header("If-Match", "\"stale\"", true)
                    .send(&service)
                    .await;
            assert_eq!(response.status_code, Some(StatusCode::PRECONDITION_FAILED));
            assert!(!response.headers().contains_key("content-encoding"));
            if method == "GET" {
                assert!(response.take_string().await.unwrap().contains("412"));
            }
            let mut response =
                salvo::test::RequestBuilder::new("http://localhost/", method.parse().unwrap())
                    .add_header("Accept-Encoding", "gzip", true)
                    .add_header("If-None-Match", "*", true)
                    .send(&service)
                    .await;
            assert_eq!(response.status_code, Some(StatusCode::NOT_MODIFIED));
            assert_eq!(response.headers()["content-encoding"], "gzip");
            assert!(response.take_bytes(None).await.unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn conditional_failures_do_not_describe_the_encoded_success_body() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("file.txt"), "gzip fixture\n".repeat(2000)).unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &[],
        )));
        let initial = TestClient::get("http://localhost/file.txt")
            .add_header("Accept-Encoding", "gzip", true)
            .send(&service)
            .await;
        let tag = initial.headers()["etag"].to_str().unwrap().to_owned();
        for method in ["GET", "HEAD"] {
            let mut response = salvo::test::RequestBuilder::new(
                "http://localhost/file.txt",
                method.parse().unwrap(),
            )
            .add_header("Accept-Encoding", "gzip", true)
            .add_header("If-Match", "\"stale\"", true)
            .add_header("Range", "bytes=0-5", true)
            .send(&service)
            .await;
            assert_eq!(response.status_code, Some(StatusCode::PRECONDITION_FAILED));
            assert!(!response.headers().contains_key("content-encoding"));
            assert!(!response.headers().contains_key("content-range"));
            if method == "GET" {
                assert!(response.take_string().await.unwrap().contains("412"));
            }
        }
        let response = TestClient::head("http://localhost/file.txt")
            .add_header("Accept-Encoding", "gzip", true)
            .add_header("If-None-Match", tag, true)
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::NOT_MODIFIED));
        assert_eq!(response.headers()["content-encoding"], "gzip");
    }

    #[tokio::test]
    async fn generated_representations_apply_existence_conditions_without_file_validators() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("file.txt"), "fixture").unwrap();
        let config = crate::test_support::config(dir.path(), &["-A"]);
        let service = Service::new(crate::build_router(config.clone()));
        let file = TestClient::get("http://localhost/file.txt")
            .send(&service)
            .await;
        let file_tag = file.headers()["etag"].to_str().unwrap();
        for media in [
            "application/json",
            "application/x-tar",
            "application/zip",
            "text/html",
        ] {
            for method in ["GET", "HEAD"] {
                for (name, value, status) in [
                    ("If-Match", "*", StatusCode::OK),
                    ("If-None-Match", "*", StatusCode::NOT_MODIFIED),
                    ("If-Match", file_tag, StatusCode::PRECONDITION_FAILED),
                    ("If-None-Match", file_tag, StatusCode::OK),
                    (
                        "If-Modified-Since",
                        "Wed, 01 Jan 2031 00:00:00 GMT",
                        StatusCode::OK,
                    ),
                    (
                        "If-Unmodified-Since",
                        "Wed, 01 Jan 2020 00:00:00 GMT",
                        StatusCode::OK,
                    ),
                ] {
                    let mut response = salvo::test::RequestBuilder::new(
                        "http://localhost/",
                        method.parse().unwrap(),
                    )
                    .add_header("Accept", media, true)
                    .add_header(name, value, true)
                    .send(&service)
                    .await;
                    assert_eq!(
                        response.status_code,
                        Some(status),
                        "{media} {method} {name}"
                    );
                    assert!(!response.headers().contains_key("etag"));
                    assert!(!response.headers().contains_key("last-modified"));
                    let body = response.take_bytes(None).await.unwrap();
                    if status == StatusCode::NOT_MODIFIED
                        || (method == "HEAD" && status.is_success())
                    {
                        assert!(body.is_empty());
                    }
                }
            }
        }
        assert_eq!(config.archive_jobs.available_permits(), 4);
        let form = "vendor=http\narchive=yes-i-really-want-one\ntype=tar\n";
        let response = TestClient::post("http://localhost/")
            .add_header("Content-Type", "text/plain", true)
            .add_header("If-None-Match", "*", true)
            .body(form)
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::PRECONDITION_FAILED));
        assert_eq!(config.archive_jobs.available_permits(), 4);
    }

    #[tokio::test]
    async fn generated_conditions_preserve_auth_missing_negotiation_and_worker_failures() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("file.txt"), "fixture").unwrap();
        let config = crate::test_support::config(dir.path(), &["-A"]);
        let service = Service::new(crate::build_router(config.clone()));
        let private = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &["-A", "--auth", "user:pass"],
        )));
        let private_tree = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &["-A", "--path-auth", "file.txt=user:pass"],
        )));
        for media in ["application/json", "application/x-tar", "application/zip"] {
            for method in ["GET", "HEAD"] {
                for (target, path, encoding, status) in [
                    (&private, "/", "identity", StatusCode::UNAUTHORIZED),
                    (&service, "/missing", "identity", StatusCode::NOT_FOUND),
                    (&service, "/", "*;q=0", StatusCode::NOT_ACCEPTABLE),
                ] {
                    let response = salvo::test::RequestBuilder::new(
                        format!("http://localhost{path}"),
                        method.parse().unwrap(),
                    )
                    .add_header("Accept", media, true)
                    .add_header("Accept-Encoding", encoding, true)
                    .add_header("If-None-Match", "*", true)
                    .send(target)
                    .await;
                    assert_eq!(
                        response.status_code,
                        Some(status),
                        "{media} {method} {path}"
                    );
                }
            }
        }
        for media in ["application/x-tar", "application/zip"] {
            for method in ["GET", "HEAD"] {
                let response =
                    salvo::test::RequestBuilder::new("http://localhost/", method.parse().unwrap())
                        .add_header("Accept", media, true)
                        .add_header("If-None-Match", "*", true)
                        .send(&private_tree)
                        .await;
                assert_eq!(response.status_code, Some(StatusCode::UNAUTHORIZED));
            }
        }
        let _metadata = config
            .blocking_jobs
            .clone()
            .acquire_many_owned(4)
            .await
            .unwrap();
        let _archives = config
            .archive_jobs
            .clone()
            .acquire_many_owned(4)
            .await
            .unwrap();
        for media in ["application/json", "application/x-tar", "application/zip"] {
            let get = TestClient::get("http://localhost/")
                .add_header("Accept", media, true)
                .add_header("If-None-Match", "*", true)
                .send(&service)
                .await;
            assert_eq!(
                get.status_code,
                Some(StatusCode::SERVICE_UNAVAILABLE),
                "{media}"
            );
            let mut head = TestClient::head("http://localhost/")
                .add_header("Accept", media, true)
                .add_header("If-None-Match", "*", true)
                .send(&service)
                .await;
            assert_eq!(head.status_code, Some(StatusCode::NOT_MODIFIED), "{media}");
            assert!(head.take_bytes(None).await.unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn json_snapshot_entry_limits_take_precedence_over_existence_conditions() {
        let dir = crate::test_support::TestDir::new();
        for index in 0..=crate::util::metadata::MAX_METADATA_ENTRIES {
            std::fs::write(dir.path().join(index.to_string()), "").unwrap();
        }
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &[],
        )));
        let response = TestClient::get("http://localhost/")
            .add_header("Accept", "application/json", true)
            .add_header("If-None-Match", "*", true)
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::INSUFFICIENT_STORAGE));
    }

    #[tokio::test]
    async fn repeated_date_conditions_are_ignored_for_reads_and_writes() {
        let dir = crate::test_support::TestDir::new();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "original").unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &["-w"],
        )));
        let past = "Wed, 01 Jan 2020 00:00:00 GMT";
        let future = "Wed, 01 Jan 2031 00:00:00 GMT";
        for method in ["GET", "HEAD"] {
            for (name, first, second) in [
                ("If-Modified-Since", future, past),
                ("If-Unmodified-Since", past, future),
            ] {
                let response = salvo::test::RequestBuilder::new(
                    "http://localhost/file.txt",
                    method.parse().unwrap(),
                )
                .add_header(name, first, false)
                .add_header(name, second, false)
                .send(&service)
                .await;
                assert_eq!(
                    response.status_code,
                    Some(StatusCode::OK),
                    "{method} {name}"
                );
            }
        }
        let response = TestClient::put("http://localhost/file.txt")
            .add_header("If-Unmodified-Since", past, false)
            .add_header("If-Unmodified-Since", future, false)
            .body("replacement")
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::NO_CONTENT));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "replacement");
        let response = TestClient::put("http://localhost/file.txt")
            .add_header("If-Match", r#""obsolete""#, true)
            .add_header("If-Unmodified-Since", past, false)
            .add_header("If-Unmodified-Since", future, false)
            .body("must not overwrite")
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::PRECONDITION_FAILED));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "replacement");
    }

    #[tokio::test]
    async fn coding_negotiation_failures_precede_file_conditions_and_ranges() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("small.txt"), "small").unwrap();
        std::fs::write(dir.path().join("empty.txt"), "").unwrap();
        std::fs::write(
            dir.path().join("image.png"),
            "blacklisted coding\n".repeat(2000),
        )
        .unwrap();
        let mut state = 0x8b56_e3c9_u32;
        let noise: Vec<_> = (0..8192)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        std::fs::write(dir.path().join("noise.txt"), noise).unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &[],
        )));
        for path in ["small.txt", "empty.txt", "image.png", "noise.txt"] {
            for method in ["GET", "HEAD"] {
                for condition in [
                    None,
                    Some(("If-None-Match", "*")),
                    Some(("If-Match", r#""obsolete""#)),
                    Some(("If-Modified-Since", "Wed, 01 Jan 2031 00:00:00 GMT")),
                ] {
                    for range in [None, Some("bytes=0-1")] {
                        let mut request = salvo::test::RequestBuilder::new(
                            format!("http://localhost/{path}"),
                            method.parse().unwrap(),
                        )
                        .add_header(
                            "Accept-Encoding",
                            "gzip,identity;q=0",
                            true,
                        );
                        if let Some((name, value)) = condition {
                            request = request.add_header(name, value, true);
                        }
                        if let Some(value) = range {
                            request = request.add_header("Range", value, true);
                        }
                        let mut response = request.send(&service).await;
                        assert_eq!(
                            response.status_code,
                            Some(StatusCode::NOT_ACCEPTABLE),
                            "{path} {method} {condition:?} {range:?}"
                        );
                        assert!(!response.headers().contains_key("etag"));
                        assert!(!response.headers().contains_key("content-range"));
                        // A 406 may carry the framework's ordinary error page.
                        response.take_bytes(None).await.unwrap();
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn each_cold_head_resolves_negotiation_before_wildcard_and_date_conditions() {
        let dir = crate::test_support::TestDir::new();
        let mut state = 0x8b56_e3c9_u32;
        let noise: Vec<_> = (0..8192)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        std::fs::write(dir.path().join("noise.txt"), noise).unwrap();
        for (name, value) in [
            ("If-Match", "*"),
            ("If-None-Match", "*"),
            ("If-Modified-Since", "Wed, 01 Jan 2031 00:00:00 GMT"),
            ("If-Unmodified-Since", "Wed, 01 Jan 2020 00:00:00 GMT"),
        ] {
            // Each scenario has a fresh cache and no preceding GET.
            let cold = Service::new(crate::build_router(crate::test_support::config(
                dir.path(),
                &[],
            )));
            let response = TestClient::head("http://localhost/noise.txt")
                .add_header("Accept-Encoding", "gzip,identity;q=0", true)
                .add_header(name, value, true)
                .send(&cold)
                .await;
            assert_eq!(
                response.status_code,
                Some(StatusCode::NOT_ACCEPTABLE),
                "{name}"
            );
            assert!(!response.headers().contains_key("etag"));
            assert!(!response.headers().contains_key("content-encoding"));
        }
    }

    #[tokio::test]
    async fn repeated_tag_field_lines_protect_reads_and_mutations() {
        let dir = crate::test_support::TestDir::new();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "original").unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &["-w"],
        )));
        let initial = TestClient::get("http://localhost/file.txt")
            .send(&service)
            .await;
        let tag = initial.headers()["etag"].to_str().unwrap();
        for method in ["GET", "HEAD"] {
            for (header, expected) in [
                ("If-Match", StatusCode::OK),
                ("If-None-Match", StatusCode::NOT_MODIFIED),
            ] {
                let response = salvo::test::RequestBuilder::new(
                    "http://localhost/file.txt",
                    method.parse().unwrap(),
                )
                .add_header(header, r#""unrelated,*,tag""#, false)
                .add_header(header, tag, false)
                .send(&service)
                .await;
                assert_eq!(response.status_code, Some(expected), "{method} {header}");
            }
        }
        for method in ["PUT", "DELETE"] {
            let response = salvo::test::RequestBuilder::new(
                "http://localhost/file.txt",
                method.parse().unwrap(),
            )
            .add_header("If-None-Match", r#""unrelated""#, false)
            .add_header("If-None-Match", tag, false)
            .body("replacement")
            .send(&service)
            .await;
            assert_eq!(response.status_code, Some(StatusCode::PRECONDITION_FAILED));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");

            let response = salvo::test::RequestBuilder::new(
                "http://localhost/file.txt",
                method.parse().unwrap(),
            )
            .add_header("If-Match", tag, false)
            .add_header("If-Match", "*", false)
            .body("replacement")
            .send(&service)
            .await;
            assert_eq!(response.status_code, Some(StatusCode::BAD_REQUEST));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
        }
    }

    #[tokio::test]
    async fn ranges_do_not_replace_the_selected_encoding_or_its_preconditions() {
        use std::io::Read;

        let dir = crate::test_support::TestDir::new();
        let contents = "range coding fixture\n".repeat(2000);
        std::fs::write(dir.path().join("file.txt"), &contents).unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &[],
        )));
        let initial = TestClient::get("http://localhost/file.txt")
            .add_header("Accept-Encoding", "gzip", true)
            .send(&service)
            .await;
        let tag = initial.headers()["etag"].to_str().unwrap().to_owned();
        for encoding in ["gzip", "gzip,identity;q=0"] {
            for condition in [None, Some(("If-Match", tag.as_str()))] {
                let mut request = TestClient::get("http://localhost/file.txt")
                    .add_header("Accept-Encoding", encoding, true)
                    .add_header("Range", "bytes=0-9", true);
                if let Some((name, value)) = condition {
                    request = request.add_header(name, value, true);
                }
                let mut response = request.send(&service).await;
                assert_eq!(response.status_code, Some(StatusCode::OK));
                assert_eq!(response.headers()["content-encoding"], "gzip");
                assert_eq!(response.headers()["etag"], tag);
                assert!(!response.headers().contains_key("content-range"));
                let bytes = response.take_bytes(None).await.unwrap();
                let mut decoded = String::new();
                flate2::read::GzDecoder::new(bytes.as_ref())
                    .read_to_string(&mut decoded)
                    .unwrap();
                assert_eq!(decoded, contents);
            }
            let response = TestClient::get("http://localhost/file.txt")
                .add_header("Accept-Encoding", encoding, true)
                .add_header("Range", "bytes=0-9", true)
                .add_header("If-None-Match", &tag, true)
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::NOT_MODIFIED));
            assert_eq!(response.headers()["etag"], tag);
        }
        let mut identity = TestClient::get("http://localhost/file.txt")
            .add_header("Accept-Encoding", "identity", true)
            .add_header("Range", "bytes=0-9", true)
            .send(&service)
            .await;
        assert_eq!(identity.status_code, Some(StatusCode::PARTIAL_CONTENT));
        assert_eq!(identity.take_string().await.unwrap(), &contents[..10]);
    }

    #[tokio::test]
    async fn quoted_commas_never_turn_an_opaque_tag_into_a_wildcard() {
        let dir = crate::test_support::TestDir::new();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "original").unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &["-w"],
        )));
        let initial = TestClient::get("http://localhost/file.txt")
            .send(&service)
            .await;
        let tag = initial.headers()["etag"].to_str().unwrap();
        for method in ["GET", "HEAD"] {
            for (header, value, expected) in [
                (
                    "If-None-Match",
                    r#""prefix,*,suffix""#.to_owned(),
                    StatusCode::OK,
                ),
                (
                    "If-Match",
                    r#""prefix,*,suffix""#.to_owned(),
                    StatusCode::PRECONDITION_FAILED,
                ),
                (
                    "If-None-Match",
                    format!(r#""ignored,comma", W/{tag}"#),
                    StatusCode::NOT_MODIFIED,
                ),
                (
                    "If-Match",
                    format!(r#""ignored,comma", {tag}"#),
                    StatusCode::OK,
                ),
            ] {
                let response = salvo::test::RequestBuilder::new(
                    "http://localhost/file.txt",
                    method.parse().unwrap(),
                )
                .add_header(header, value, true)
                .send(&service)
                .await;
                assert_eq!(response.status_code, Some(expected), "{method} {header}");
            }
        }
        let response = TestClient::put("http://localhost/file.txt")
            .add_header("If-Match", r#""prefix,*,suffix""#, true)
            .body("replacement")
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::PRECONDITION_FAILED));
        let response = salvo::test::RequestBuilder::new(
            "http://localhost/file.txt",
            salvo::http::Method::DELETE,
        )
        .add_header("If-Match", r#""prefix", *"#, true)
        .send(&service)
        .await;
        assert_eq!(response.status_code, Some(StatusCode::BAD_REQUEST));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "original");
    }

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
    async fn cold_head_if_none_match_resolves_the_selected_validator() {
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
        for fields in [
            vec![tag.to_owned()],
            vec![format!("W/{tag}")],
            vec![r#""other,*,tag""#.to_owned(), tag.to_owned()],
        ] {
            let cold = Service::new(crate::build_router(crate::test_support::config(
                dir.path(),
                &[],
            )));
            let mut request = TestClient::head("http://localhost/file.txt").add_header(
                "Accept-Encoding",
                "gzip",
                true,
            );
            for value in fields {
                request = request.add_header("If-None-Match", value, false);
            }
            let mut response = request.send(&cold).await;
            assert_eq!(response.status_code, Some(StatusCode::NOT_MODIFIED));
            assert_eq!(response.headers()["etag"], tag);
            assert!(response.take_bytes(None).await.unwrap().is_empty());
        }
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
