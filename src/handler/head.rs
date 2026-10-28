use std::sync::Arc;

use salvo::prelude::*;

use crate::config::AppConfig;
use crate::util::*;

/// Inspect a resource without reading its body or producing an archive.
#[handler]
pub async fn handle_head(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let config = depot.get_typed::<Arc<AppConfig>>().unwrap();
    if let Some(response) = crate::hoops::auth::check_auth(req, config) {
        *res = response;
        res.body(salvo::http::ResBody::None);
        return;
    }
    let segments: Vec<_> = req
        .uri()
        .path()
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let (mut path, symlink, invalid) = resolve_path(
        &config.hosted_directory.1,
        &segments,
        config.follow_symlinks,
    );
    if invalid {
        res.status_code(StatusCode::BAD_REQUEST);
        return;
    }
    if !path.exists()
        && config.strip_extensions
        && path.extension().is_none()
        && let Some(candidate) = INDEX_EXTENSIONS
            .iter()
            .map(|ext| path.with_extension(ext))
            .find(|p| p.is_file())
    {
        path = candidate;
    }
    if config.is_symlink_denied(symlink, &path) || !path.exists() {
        res.status_code(StatusCode::NOT_FOUND);
        return;
    }
    if crate::handler::rfsapi::wants_rfsapi(req) {
        res.status_code(StatusCode::OK);
        res.headers_mut()
            .insert("content-type", "application/json".parse().unwrap());
        return;
    }
    if config.archives
        && let Some(kind) = crate::handler::archive::try_get_accept_archive(req)
    {
        res.status_code(StatusCode::OK);
        res.headers_mut()
            .insert("content-type", kind.default_mime().parse().unwrap());
        return;
    }
    if path.is_dir() {
        let index = config
            .check_indices
            .then(|| {
                INDEX_EXTENSIONS
                    .iter()
                    .map(|ext| path.join("index").with_extension(ext))
                    .find(|p| p.is_file() && !config.is_symlink_denied(is_symlink(p), p))
            })
            .flatten();
        if let Some(index) = index {
            if !req.uri().path().ends_with('/') {
                res.status_code(StatusCode::SEE_OTHER);
                res.headers_mut().insert(
                    "location",
                    format!("{}/", req.uri().path()).parse().unwrap(),
                );
                return;
            }
            path = index;
        } else {
            res.status_code(if config.generate_listings {
                StatusCode::OK
            } else {
                StatusCode::NOT_FOUND
            });
            res.headers_mut()
                .insert("content-type", "text/html; charset=utf-8".parse().unwrap());
            return;
        }
    }
    let Ok(metadata) = path.metadata() else {
        res.status_code(StatusCode::NOT_FOUND);
        return;
    };
    res.status_code(StatusCode::OK);
    res.headers_mut().insert(
        "content-type",
        guess_mime_type(&path, &config.mime_type_overrides)
            .parse()
            .unwrap(),
    );
    res.headers_mut().insert(
        "content-length",
        file_length(&metadata, &path).to_string().parse().unwrap(),
    );
    res.headers_mut().insert(
        "etag",
        format!("\"{}\"", file_etag(&metadata)).parse().unwrap(),
    );
    res.headers_mut().insert(
        "last-modified",
        file_time_modified(&metadata)
            .format("%a, %d %b %Y %T GMT")
            .to_string()
            .parse()
            .unwrap(),
    );
    res.headers_mut()
        .insert("accept-ranges", "bytes".parse().unwrap());
    res.headers_mut()
        .insert("server", USER_AGENT.parse().unwrap());
}

#[cfg(test)]
mod tests {
    use salvo::test::{ResponseExt, TestClient};

    use super::*;

    #[tokio::test]
    async fn head_has_file_headers_without_a_body() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("file.txt"), "fixture").unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &[],
        )));
        let mut head = TestClient::head("http://localhost/file.txt")
            .send(&service)
            .await;
        let get = TestClient::get("http://localhost/file.txt")
            .send(&service)
            .await;
        assert_eq!(head.status_code, Some(StatusCode::OK));
        for header in ["content-type", "content-length", "etag", "last-modified"] {
            assert_eq!(head.headers().get(header), get.headers().get(header));
        }
        assert_eq!(head.take_bytes(None).await.unwrap().len(), 0);
        let response = TestClient::head("http://localhost/missing.txt")
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::NOT_FOUND));
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &["--auth", "a:b"],
        )));
        let response = TestClient::head("http://localhost/file.txt")
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::UNAUTHORIZED));
    }
}
