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
    let (mut path, mut symlink, invalid) = resolve_path(
        &config.hosted_directory.1,
        &segments,
        config.follow_symlinks,
    );
    if invalid {
        res.status_code(StatusCode::BAD_REQUEST);
        return;
    }

    // These GET representations inspect the original resource; extension and
    // default-index selection applies only to ordinary file responses.
    if crate::handler::rfsapi::wants_rfsapi(req) {
        res.status_code(
            if !path.exists() || config.is_symlink_denied(symlink, &path) {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::OK
            },
        );
        res.headers_mut()
            .insert("content-type", "application/json".parse().unwrap());
        return;
    }
    if config.archives
        && let Some(kind) = crate::handler::archive::try_get_accept_archive(req)
    {
        if let Some(response) = crate::hoops::auth::check_tree_auth(req, config, req.uri().path()) {
            *res = response;
            res.body(salvo::http::ResBody::None);
            return;
        }
        if !path.exists() || config.is_symlink_denied(symlink, &path) {
            res.status_code(StatusCode::NOT_FOUND);
            return;
        }
        res.status_code(StatusCode::OK);
        res.headers_mut()
            .insert("content-type", kind.default_mime().parse().unwrap());
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
        symlink |= is_symlink(&candidate);
        let logical = crate::util::path::decoded_url_path(req.uri().path()).unwrap();
        let logical = std::path::Path::new(&logical)
            .with_extension(candidate.extension().unwrap_or_default());
        if let Some(response) = crate::hoops::auth::check_decoded_auth(
            req,
            config,
            &logical.to_string_lossy().replace('\\', "/"),
        ) {
            *res = response;
            res.body(salvo::http::ResBody::None);
            return;
        }
        path = candidate;
    }
    if config.is_symlink_denied(symlink, &path) || !path.exists() {
        res.status_code(StatusCode::NOT_FOUND);
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
            let index_url = format!(
                "{}/index.{}",
                req.uri().path().trim_end_matches('/'),
                index.extension().and_then(|ext| ext.to_str()).unwrap_or("")
            );
            if let Some(response) = crate::hoops::auth::check_auth_path(req, config, &index_url) {
                *res = response;
                res.body(salvo::http::ResBody::None);
                return;
            }
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
    if crate::http_cache::not_modified(
        req,
        res,
        &format!("\"{}\"", file_etag(&metadata)),
        file_time_modified(&metadata),
    ) {
        return;
    }
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
    async fn head_authorizes_selected_files_before_metadata_or_redirects() {
        let dir = crate::test_support::TestDir::new();
        std::fs::create_dir(dir.path().join("folder")).unwrap();
        for name in ["index.html", "asset.html", "folder/index.html"] {
            std::fs::write(dir.path().join(name), "protected metadata").unwrap();
        }
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &[
                "-x",
                "--path-auth",
                "index.html=user:pass",
                "--path-auth",
                "asset.html=user:pass",
                "--path-auth",
                "folder/index.html=user:pass",
            ],
        )));
        for path in ["/", "/asset", "/folder", "/folder/"] {
            let head = TestClient::head(format!("http://localhost{path}"))
                .send(&service)
                .await;
            let get = TestClient::get(format!("http://localhost{path}"))
                .send(&service)
                .await;
            assert_eq!(head.status_code, Some(StatusCode::UNAUTHORIZED), "{path}");
            assert_eq!(head.status_code, get.status_code);
            assert!(head.headers().get("etag").is_none());
            assert!(head.headers().get("location").is_none());
        }
        let mut response = TestClient::head("http://localhost/asset")
            .basic_auth("user", Some("pass"))
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::OK));
        assert_eq!(response.headers().get("content-length").unwrap(), "18");
        assert!(response.take_bytes(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn head_json_and_archives_do_not_apply_file_extension_fallback() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("asset.html"), "actual file").unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &["-x", "-A"],
        )));
        for accept in ["application/json", "application/x-tar", "application/zip"] {
            let head = TestClient::head("http://localhost/asset")
                .add_header("Accept", accept, true)
                .send(&service)
                .await;
            let get = TestClient::get("http://localhost/asset")
                .add_header("Accept", accept, true)
                .send(&service)
                .await;
            assert_eq!(head.status_code, Some(StatusCode::NOT_FOUND), "{accept}");
            assert_eq!(head.status_code, get.status_code);
        }
        let mut response = TestClient::head("http://localhost/asset.html")
            .add_header("Accept", "application/json", true)
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::OK));
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/json"
        );
        assert!(response.take_bytes(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn head_archive_checks_descendants_without_producing_archive_bytes() {
        let dir = crate::test_support::TestDir::new();
        std::fs::create_dir_all(dir.path().join("collection/private")).unwrap();
        std::fs::write(dir.path().join("collection/private/file.txt"), "protected").unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &["-A", "--path-auth", "collection/private=user:pass"],
        )));
        for accept in ["application/x-tar", "application/zip"] {
            let head = TestClient::head("http://localhost/collection/")
                .add_header("Accept", accept, true)
                .send(&service)
                .await;
            let get = TestClient::get("http://localhost/collection/")
                .add_header("Accept", accept, true)
                .send(&service)
                .await;
            assert_eq!(head.status_code, Some(StatusCode::UNAUTHORIZED));
            assert_eq!(head.status_code, get.status_code);
            let mut head = TestClient::head("http://localhost/collection/")
                .basic_auth("user", Some("pass"))
                .add_header("Accept", accept, true)
                .send(&service)
                .await;
            assert_eq!(head.status_code, Some(StatusCode::OK));
            assert_eq!(head.headers().get("content-type").unwrap(), accept);
            assert!(head.headers().get("content-length").is_none());
            assert!(head.take_bytes(None).await.unwrap().is_empty());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn head_selected_symlinks_obey_no_follow_and_sandbox_options() {
        let dir = crate::test_support::TestDir::new();
        let hosted = dir.path().join("served");
        std::fs::create_dir(&hosted).unwrap();
        std::fs::write(dir.path().join("outside.txt"), "outside").unwrap();
        for name in ["index.html", "asset.html"] {
            std::os::unix::fs::symlink("../outside.txt", hosted.join(name)).unwrap();
        }
        for flags in [&["-l", "-x", "-s"][..], &["-l", "-x", "-r"]] {
            let service = Service::new(crate::build_router(crate::test_support::config(
                &hosted, flags,
            )));
            for path in ["/", "/asset"] {
                let head = TestClient::head(format!("http://localhost{path}"))
                    .send(&service)
                    .await;
                assert_eq!(head.status_code, Some(StatusCode::NOT_FOUND));
                assert!(head.headers().get("etag").is_none());
            }
        }
    }

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
