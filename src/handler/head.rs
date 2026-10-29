use std::sync::Arc;

use salvo::prelude::*;

use crate::config::AppConfig;
use crate::util::*;

/// A cold compression decision needs body reads; HEAD omits those representation fields.
pub(crate) struct UnresolvedFileEncoding;

/// Inspect a resource without reading its body or producing an archive.
#[handler]
pub async fn handle_head(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let config = depot.get_typed::<Arc<AppConfig>>().unwrap().clone();
    let opened = match super::read_snapshot::run(req, &config, select_head_snapshot).await {
        Ok(Ok(opened)) => opened,
        Ok(Err(mut response)) => {
            response.body(salvo::http::ResBody::None);
            *res = *response;
            return;
        }
        Err(status) => {
            res.status_code(status);
            return;
        }
    };
    let path = opened.path;
    let mime_type = opened.mime;
    let metadata = opened.metadata;
    let source = tokio::fs::File::from_std(opened.source);
    let etag = match file_etag(&metadata, &source) {
        Ok(etag) => etag,
        Err(_) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            return;
        }
    };
    let mut response_etag = Some(format!("\"{etag}\""));
    let mut length = Some(file_length(&metadata, &path));
    let identity_allowed = crate::encoding::request_accept_encoding(req)
        .is_none_or(|accept| crate::encoding::identity_acceptable(&accept));
    if let Some(encoding) =
        super::get::file_encoding_candidate(req, &config, &path, length.unwrap())
    {
        let mut cached = super::get::cached_file_encoding(&config, &path, &etag, encoding);
        let specific_tag = ["if-match", "if-none-match"].iter().any(|name| {
            crate::util::entity_tag::request_tag_field(req, name)
                .ok()
                .flatten()
                .is_some_and(|value| value.trim() != "*")
        });
        let conditional_without_identity = !identity_allowed
            && [
                "if-match",
                "if-none-match",
                "if-modified-since",
                "if-unmodified-since",
            ]
            .iter()
            .any(|name| req.headers().contains_key(*name));
        if cached.is_none() && (specific_tag || conditional_without_identity) {
            // Specific tag conditions need the actual selected coding, even
            // when this HEAD is the first request after the cache was lost.
            // Resolve wildcard/date conditions too when identity cannot be sent,
            // since a coding failure must win over a conditional 304 or 412.
            // Ordinary cold HEAD requests still avoid all encoding work.
            cached = Some(
                super::get::encoded_file_job(
                    &config,
                    &source,
                    &path,
                    &etag,
                    encoding,
                    &req.remote_addr().to_string(),
                )
                .await,
            );
        }
        match cached {
            Some(Some(encoded)) => {
                let coding = crate::encoding::encoding_name(encoding);
                response_etag = Some(format!("\"{etag}-{coding}\""));
                length = Some(encoded.length);
                res.headers_mut()
                    .insert("content-encoding", coding.parse().unwrap());
            }
            Some(None) => {}
            None => {
                // Neither claim identity nor create an encoding just to answer HEAD.
                response_etag = None;
                length = None;
                depot.insert_typed(UnresolvedFileEncoding);
            }
        }
    }
    // Resolve a known identity-only negotiation failure before any condition.
    // Ordinary cold HEADs may still omit fields whose coding requires body reads.
    if length.is_some() && !res.headers().contains_key("content-encoding") && !identity_allowed {
        res.status_code(StatusCode::NOT_ACCEPTABLE);
        return;
    }
    if crate::http_cache::preconditions(
        req,
        res,
        response_etag.as_deref(),
        file_time_modified(&metadata),
    ) {
        return;
    }
    res.status_code(StatusCode::OK);
    res.headers_mut()
        .insert("content-type", mime_type.parse().unwrap());
    if let Some(length) = length {
        res.headers_mut()
            .insert("content-length", length.to_string().parse().unwrap());
    }
    if let Some(etag) = response_etag {
        res.headers_mut().insert("etag", etag.parse().unwrap());
    }
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

/// Called while the namespace lock protects authorization, selection and open.
fn select_head_snapshot(
    req: &mut Request,
    config: &Arc<AppConfig>,
) -> Result<super::read_snapshot::OpenedFile, Box<Response>> {
    let mut res = Response::new();
    if let Some(response) = crate::hoops::auth::check_auth(req, config) {
        res = response;
        res.body(salvo::http::ResBody::None);
        return Err(Box::new(res));
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
        return Err(Box::new(res));
    }

    // These GET representations inspect the original resource; extension and
    // default-index selection applies only to ordinary file responses.
    let representation = super::representation::select(req, config, &path);
    if representation == super::representation::Representation::NotAcceptable {
        res.status_code(StatusCode::NOT_ACCEPTABLE);
        return Err(Box::new(res));
    }
    if representation == super::representation::Representation::Json {
        res.status_code(
            if !path.exists() || config.is_symlink_denied(symlink, &path) {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::OK
            },
        );
        res.headers_mut()
            .insert("content-type", "application/json".parse().unwrap());
        crate::http_cache::unversioned_preconditions(req, &mut res);
        return Err(Box::new(res));
    }
    if let super::representation::Representation::Archive(kind) = representation {
        // Descendant-policy walks keep their existing general-worker admission,
        // but run inline in this snapshot instead of nesting a blocking job.
        let _authorization = if config.path_auth_data.is_empty() {
            None
        } else {
            match config.blocking_jobs.clone().try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(_) => {
                    res.status_code(StatusCode::SERVICE_UNAVAILABLE);
                    return Err(Box::new(res));
                }
            }
        };
        if let Some(response) = crate::hoops::auth::check_tree_auth(req, config, req.uri().path()) {
            res = response;
            res.body(salvo::http::ResBody::None);
            return Err(Box::new(res));
        }
        if !path.exists() || config.is_symlink_denied(symlink, &path) {
            res.status_code(StatusCode::NOT_FOUND);
            return Err(Box::new(res));
        }
        res.status_code(StatusCode::OK);
        res.headers_mut()
            .insert("content-type", kind.default_mime().parse().unwrap());
        crate::http_cache::unversioned_preconditions(req, &mut res);
        return Err(Box::new(res));
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
            res = response;
            res.body(salvo::http::ResBody::None);
            return Err(Box::new(res));
        }
        path = candidate;
    }
    if config.is_symlink_denied(symlink, &path) || !path.exists() {
        res.status_code(StatusCode::NOT_FOUND);
        return Err(Box::new(res));
    }
    if path.is_dir() {
        let index = super::get::index_file(config, &path);
        if let Some(index) = index {
            let index_url = format!(
                "{}/index.{}",
                req.uri().path().trim_end_matches('/'),
                index.extension().and_then(|ext| ext.to_str()).unwrap_or("")
            );
            if let Some(response) = crate::hoops::auth::check_auth_path(req, config, &index_url) {
                res = response;
                res.body(salvo::http::ResBody::None);
                return Err(Box::new(res));
            }
            if !req.uri().path().ends_with('/') {
                res.status_code(StatusCode::SEE_OTHER);
                res.headers_mut().insert(
                    "location",
                    format!("{}/", req.uri().path()).parse().unwrap(),
                );
                return Err(Box::new(res));
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
            if config.generate_listings
                && let Some(encoding) = crate::encoding::request_accept_encoding(req)
                    .and_then(|accept| crate::encoding::response_encoding(&accept))
            {
                // Generated GET listings always use the selected coding; no
                // directory traversal or compression work is needed for HEAD.
                res.headers_mut().insert(
                    "content-encoding",
                    crate::encoding::encoding_name(encoding).parse().unwrap(),
                );
            }
            crate::http_cache::unversioned_preconditions(req, &mut res);
            return Err(Box::new(res));
        }
    }
    super::read_snapshot::OpenedFile::open(config, &path).map_err(|_| {
        res.status_code(StatusCode::NOT_FOUND);
        Box::new(res)
    })
}

#[cfg(test)]
mod tests {
    use salvo::test::{ResponseExt, TestClient};

    use super::*;

    #[tokio::test]
    async fn directory_head_selects_the_get_coding_without_a_listing_job_or_body() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("file.txt"), "fixture").unwrap();
        let config = crate::test_support::config(dir.path(), &[]);
        let service = Service::new(crate::build_router(config.clone()));
        for (accept, status, coding) in [
            ("gzip,identity;q=0", StatusCode::OK, Some("gzip")),
            ("identity", StatusCode::OK, None),
            ("*;q=0", StatusCode::NOT_ACCEPTABLE, None),
        ] {
            let get = TestClient::get("http://localhost/")
                .add_header("Accept-Encoding", accept, true)
                .send(&service)
                .await;
            assert_eq!(get.status_code, Some(status), "GET {accept}");
            assert_eq!(
                get.headers()
                    .get("content-encoding")
                    .and_then(|v| v.to_str().ok()),
                coding
            );
            let _permits = config
                .blocking_jobs
                .clone()
                .acquire_many_owned(4)
                .await
                .unwrap();
            let mut head = TestClient::head("http://localhost/")
                .add_header("Accept-Encoding", accept, true)
                .send(&service)
                .await;
            assert_eq!(head.status_code, get.status_code, "HEAD {accept}");
            assert_eq!(
                head.headers().get("content-encoding"),
                get.headers().get("content-encoding")
            );
            if status.is_success() {
                assert!(!head.headers().contains_key("content-length"));
                assert!(head.take_bytes(None).await.unwrap().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn directory_indexes_share_file_negotiation_preconditions_and_ranges() {
        let dir = crate::test_support::TestDir::new();
        std::fs::create_dir(dir.path().join("folder")).unwrap();
        std::fs::write(
            dir.path().join("folder/index.html"),
            "index fixture\n".repeat(2000),
        )
        .unwrap();
        let config = crate::test_support::config(dir.path(), &[]);
        let service = Service::new(crate::build_router(config.clone()));
        let cold = TestClient::head("http://localhost/folder/")
            .add_header("Accept-Encoding", "gzip,identity;q=0", true)
            .send(&service)
            .await;
        assert_eq!(cold.status_code, Some(StatusCode::OK));
        assert!(!cold.headers().contains_key("etag"));
        assert!(config.cache_fs_files.read().unwrap().is_empty());
        let direct = TestClient::get("http://localhost/folder/index.html")
            .add_header("Accept-Encoding", "gzip,identity;q=0", true)
            .send(&service)
            .await;
        let tag = direct.headers()["etag"].to_str().unwrap();
        for method in ["GET", "HEAD"] {
            let request = if method == "GET" {
                TestClient::get("http://localhost/folder/")
            } else {
                TestClient::head("http://localhost/folder/")
            };
            let response = request
                .add_header("Accept-Encoding", "gzip,identity;q=0", true)
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::OK));
            for header in [
                "content-type",
                "content-encoding",
                "content-length",
                "etag",
                "last-modified",
            ] {
                assert_eq!(
                    response.headers().get(header),
                    direct.headers().get(header),
                    "{method} {header}"
                );
            }
            let request = if method == "GET" {
                TestClient::get("http://localhost/folder/")
            } else {
                TestClient::head("http://localhost/folder/")
            };
            let response = request
                .add_header("Accept-Encoding", "gzip", true)
                .add_header("If-None-Match", tag, true)
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::NOT_MODIFIED));
        }
        let mut range = TestClient::get("http://localhost/folder/")
            .add_header("Range", "bytes=0-4", true)
            .send(&service)
            .await;
        assert_eq!(range.status_code, Some(StatusCode::PARTIAL_CONTENT));
        assert_eq!(range.take_string().await.unwrap(), "index");
        let redirect = TestClient::get("http://localhost/folder")
            .send(&service)
            .await;
        assert_eq!(redirect.status_code, Some(StatusCode::SEE_OTHER));
        assert_eq!(redirect.headers()["location"], "/folder/");
        let disabled = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &["-e"],
        )));
        for request in [
            TestClient::get("http://localhost/folder/"),
            TestClient::head("http://localhost/folder/"),
        ] {
            let response = request
                .add_header("Accept-Encoding", "gzip,identity;q=0", true)
                .send(&disabled)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::NOT_ACCEPTABLE));
        }
    }

    #[tokio::test]
    async fn head_uses_cached_coding_and_omits_unknown_representation_fields() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("file.txt"), "compressible\n".repeat(2000)).unwrap();
        let config = crate::test_support::config(dir.path(), &[]);
        let service = Service::new(crate::build_router(config.clone()));
        let mut cold = TestClient::head("http://localhost/file.txt")
            .add_header("Accept-Encoding", "gzip,identity;q=0", true)
            .send(&service)
            .await;
        assert_eq!(cold.status_code, Some(StatusCode::OK));
        for header in ["content-length", "content-encoding", "etag"] {
            assert!(!cold.headers().contains_key(header), "{header}");
        }
        assert!(cold.take_bytes(None).await.unwrap().is_empty());
        assert!(config.cache_fs_files.read().unwrap().is_empty());
        let get = TestClient::get("http://localhost/file.txt")
            .add_header("Accept-Encoding", "gzip,identity;q=0", true)
            .send(&service)
            .await;
        assert_eq!(get.status_code, Some(StatusCode::OK));
        assert_eq!(get.headers()["content-encoding"], "gzip");
        // Cached GET and HEAD do not need a worker, even when all workers are busy.
        let _workers = config
            .blocking_jobs
            .clone()
            .acquire_many_owned(4)
            .await
            .unwrap();
        for method in ["GET", "HEAD"] {
            let request = if method == "GET" {
                TestClient::get("http://localhost/file.txt")
            } else {
                TestClient::head("http://localhost/file.txt")
            };
            let response = request
                .add_header("Accept-Encoding", "gzip,identity;q=0", true)
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::OK));
            for header in ["content-length", "content-encoding", "etag"] {
                assert_eq!(
                    response.headers().get(header),
                    get.headers().get(header),
                    "{method} {header}"
                );
            }
        }
        let response = TestClient::head("http://localhost/file.txt")
            .add_header("Accept-Encoding", "gzip,identity;q=0", true)
            .add_header(
                "If-None-Match",
                get.headers()["etag"].to_str().unwrap(),
                true,
            )
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::NOT_MODIFIED));
        assert_eq!(response.headers().get("etag"), get.headers().get("etag"));
    }

    #[tokio::test]
    async fn head_preserves_identity_rejections_for_ineligible_and_negative_codings() {
        let dir = crate::test_support::TestDir::new();
        for name in ["file.txt", "file.png"] {
            std::fs::write(dir.path().join(name), "compressible\n".repeat(2000)).unwrap();
        }
        std::fs::write(dir.path().join("small.txt"), "small").unwrap();
        for (flags, path) in [
            (vec!["-e"], "file.txt"),
            (vec![], "file.png"),
            (vec![], "small.txt"),
            (vec!["--encoded-filesystem", "1"], "file.txt"),
        ] {
            let config = crate::test_support::config(dir.path(), &flags);
            let service = Service::new(crate::build_router(config));
            // A GET records the compression-budget rejection as a negative entry.
            if flags.contains(&"--encoded-filesystem") {
                let get = TestClient::get(format!("http://localhost/{path}"))
                    .add_header("Accept-Encoding", "gzip,identity;q=0", true)
                    .send(&service)
                    .await;
                assert_eq!(get.status_code, Some(StatusCode::NOT_ACCEPTABLE));
            }
            let response = TestClient::head(format!("http://localhost/{path}"))
                .add_header("Accept-Encoding", "gzip,identity;q=0", true)
                .send(&service)
                .await;
            assert_eq!(
                response.status_code,
                Some(StatusCode::NOT_ACCEPTABLE),
                "{flags:?} {path}"
            );
        }
    }

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
