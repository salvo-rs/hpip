use std::borrow::Cow;
use std::sync::Arc;

use salvo::prelude::*;

use crate::config::{AppConfig, log_msg};
use crate::util::USER_AGENT;
use crate::util::path::decoded_url_path;

/// Check authentication. Returns Some(Response) if auth fails, None if auth passes.
/// Takes config directly to avoid borrow conflicts with depot.
pub fn check_auth(req: &Request, config: &Arc<AppConfig>) -> Option<Response> {
    check_auth_path(req, config, req.uri().path())
}

/// Check a URL using the same decoded components as filesystem resolution.
pub fn check_auth_path(req: &Request, config: &AppConfig, url_path: &str) -> Option<Response> {
    let Some(path) = decoded_url_path(url_path) else {
        let mut response = Response::new();
        response.status_code(StatusCode::BAD_REQUEST);
        response.render(Text::Plain("Invalid resource path.\n"));
        return Some(response);
    };
    check_decoded_auth(req, config, &path)
}

/// Keep collection filesystem walks off the runtime and reject overload before
/// spawning. Synchronous workers call `check_tree_auth` directly to avoid
/// recursively reserving another worker slot.
pub async fn check_tree_auth_async(
    req: &Request,
    config: &Arc<AppConfig>,
    url_path: &str,
) -> Option<Response> {
    if config.path_auth_data.is_empty() {
        // Without descendant rules, authorization never traverses a collection.
        return check_auth_path(req, config, url_path);
    }
    let mut snapshot = Request::new();
    *snapshot.uri_mut() = req.uri().clone();
    *snapshot.method_mut() = req.method().clone();
    *snapshot.headers_mut() = req.headers().clone();
    *snapshot.remote_addr_mut() = req.remote_addr().clone();
    let path = url_path.to_owned();
    let work_config = config.clone();
    let Some(job) = config.spawn_blocking(move || check_tree_auth(&snapshot, &work_config, &path))
    else {
        return Some(tree_worker_error(StatusCode::SERVICE_UNAVAILABLE));
    };
    match job.await {
        Ok(result) => result,
        Err(_) => Some(tree_worker_error(StatusCode::INTERNAL_SERVER_ERROR)),
    }
}

fn tree_worker_error(status: StatusCode) -> Response {
    let mut response = Response::new();
    response.status_code(status);
    response.render(Text::Plain(
        "Collection authorization is temporarily unavailable.\n",
    ));
    response
}

/// Require authorization for the whole collection before producing output or
/// mutating any member. Different descendant credentials can therefore make a
/// collection operation unavailable until the policies are made consistent.
pub fn check_tree_auth(req: &Request, config: &AppConfig, url_path: &str) -> Option<Response> {
    if let Some(response) = check_auth_path(req, config, url_path) {
        return Some(response);
    }
    if config.path_auth_data.is_empty() {
        return None;
    }
    let path = decoded_url_path(url_path)?;
    let mut roots = vec![path.clone()];
    if let Some(actual_path) = canonical_auth_path(config, &path) {
        roots.push(actual_path);
    }
    // Include configured future descendants when authorizing a COPY target.
    for rule in config.path_auth_data.keys() {
        if roots
            .iter()
            .any(|root| root.is_empty() || path_rule_matches(rule, root))
            && let Some(response) = check_decoded_auth(req, config, rule)
        {
            return Some(response);
        }
    }

    let root = config.hosted_directory.1.join(&path);
    if !root.exists() {
        return None;
    }
    // Inspect real members as well, including directory aliases that point to
    // protected resources outside the requested logical subtree.
    for (count, entry) in walkdir::WalkDir::new(&root)
        .follow_links(config.follow_symlinks)
        .max_depth(256)
        .into_iter()
        .filter_entry(|entry| !crate::util::path::is_internal_name(entry.file_name()))
        .enumerate()
    {
        let Ok(entry) = entry else {
            return Some(collection_error(
                "Cannot authorize every collection member.\n",
            ));
        };
        if count >= 100_000 || (entry.depth() == 256 && entry.file_type().is_dir()) {
            return Some(collection_error(
                "Collection authorization limit exceeded.\n",
            ));
        }
        let Ok(relative) = entry.path().strip_prefix(&config.hosted_directory.1) else {
            return Some(collection_error("Invalid collection member path.\n"));
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if let Some(response) = check_decoded_auth(req, config, &relative) {
            return Some(response);
        }
    }
    None
}

fn collection_error(message: &'static str) -> Response {
    let mut response = Response::new();
    response.status_code(StatusCode::FORBIDDEN);
    response.render(Text::Plain(message));
    response
}

type Credentials = (String, Option<String>);

/// A decoded resource name is never percent-decoded a second time.
pub fn check_decoded_auth(req: &Request, config: &AppConfig, path: &str) -> Option<Response> {
    if config.global_auth_data.is_none() && config.path_auth_data.is_empty() {
        return None;
    }

    let mut required = Vec::new();
    if let Some(auth) = auth_for_path(config, path) {
        required.push(auth);
    }
    // Resolve existing ancestors as well: a new file under a directory alias
    // must not bypass the real directory's policy.
    if let Some(actual_path) = canonical_auth_path(config, path)
        && let Some(auth) = auth_for_path(config, &actual_path)
        && !required.contains(&auth)
    {
        required.push(auth);
    }
    if required.is_empty() {
        return None;
    }

    let remote = req.remote_addr().to_string();
    let method = req.method().to_string();
    let url = req.uri().path().to_string();

    if let Some(credentials) = supplied_credentials(req) {
        if required.iter().all(|expected| **expected == credentials) {
            log_msg(
                config.log,
                &format!("{} correctly authorised to {} {}", remote, method, url),
            );
            return None;
        }
        log_msg(
            config.log,
            &format!(
                "{} requested to {} {} with invalid credentials",
                remote, method, url
            ),
        );
        return Some(unauthorized_response("Supplied credentials invalid.\n"));
    }

    log_msg(
        config.log,
        &format!(
            "{} requested to {} {} without authorisation",
            remote, method, url
        ),
    );
    Some(unauthorized_response("Credentials required.\n"))
}

fn supplied_credentials(req: &Request) -> Option<Credentials> {
    if let Some(auth_header) = req.headers().get(salvo::http::header::AUTHORIZATION)
        && let Ok(auth_str) = auth_header.to_str()
        && let Some((scheme, credentials)) = auth_str.split_once(' ')
        && scheme.eq_ignore_ascii_case("Basic")
        && let Ok(decoded) = base64_decode(credentials.trim_start_matches(' '))
        && let Ok(cred_str) = String::from_utf8(decoded)
    {
        let (username, password) = if let Some(pos) = cred_str.find(':') {
            let u = &cred_str[..pos];
            let p = &cred_str[pos + 1..];
            (
                u.to_string(),
                if p.is_empty() {
                    None
                } else {
                    Some(p.to_string())
                },
            )
        } else {
            (cred_str.clone(), None)
        };

        return Some((username, password));
    }
    None
}

fn unauthorized_response(message: &'static str) -> Response {
    let mut resp = Response::new();
    resp.status_code(StatusCode::UNAUTHORIZED);
    resp.headers_mut()
        .insert("WWW-Authenticate", "Basic realm=\"hpip\"".parse().unwrap());
    resp.headers_mut()
        .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
    resp.render(Text::Plain(message));
    resp
}

fn auth_for_path<'a>(config: &'a AppConfig, path: &str) -> Option<&'a Credentials> {
    config
        .path_auth_data
        .iter()
        .filter(|(rule, _)| path_rule_matches(path, rule))
        .max_by_key(|(rule, _)| rule.len())
        .map_or(config.global_auth_data.as_ref(), |(_, auth)| auth.as_ref())
}

fn path_rule_matches(path: &str, rule: &str) -> bool {
    let path = policy_key(path);
    let rule = policy_key(rule);
    let prefix: &str = &rule;
    path == rule
        || path
            .strip_prefix(prefix)
            .is_some_and(|tail| tail.starts_with('/'))
}

fn policy_key(path: &str) -> Cow<'_, str> {
    #[cfg(windows)]
    {
        Cow::Owned(path.to_lowercase())
    }
    #[cfg(not(windows))]
    {
        Cow::Borrowed(path)
    }
}

fn canonical_auth_path(config: &AppConfig, path: &str) -> Option<String> {
    let mut current = config.hosted_directory.1.join(path);
    let mut suffix = Vec::new();
    let canonical = loop {
        match current.canonicalize() {
            Ok(canonical) => break canonical,
            Err(_) => {
                suffix.push(current.file_name()?.to_owned());
                current = current.parent()?.to_path_buf();
            }
        }
    };
    let root = config.hosted_directory.1.canonicalize().ok()?;
    let mut relative = canonical.strip_prefix(root).ok()?.to_path_buf();
    for component in suffix.into_iter().rev() {
        relative.push(component);
    }
    Some(relative.to_string_lossy().replace('\\', "/"))
}

fn base64_decode(s: &str) -> Result<Vec<u8>, ()> {
    // Simple base64 decoder
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let s = s.trim_end_matches('=');
    let mut result = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0;

    for &b in s.as_bytes() {
        let val = TABLE.iter().position(|&t| t == b).ok_or(())? as u32;
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            result.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use salvo::test::{ResponseExt, TestClient};

    #[tokio::test]
    async fn basic_credentials_accept_scheme_casing_and_space_separators() {
        let dir = TestDir::new();
        std::fs::create_dir(dir.path().join("private")).unwrap();
        std::fs::write(dir.path().join("private/file.txt"), "protected").unwrap();
        for flags in [
            &["--auth", "user:pass"][..],
            &["--path-auth", "private=user:pass"][..],
        ] {
            let service = Service::new(crate::build_router(config(dir.path(), flags)));
            for prefix in ["Basic ", "basic ", "BASIC ", "bAsIc ", "Basic  "] {
                let mut response = TestClient::get("http://localhost/private/file.txt")
                    .add_header("Authorization", format!("{prefix}dXNlcjpwYXNz"), true)
                    .send(&service)
                    .await;
                assert_eq!(response.status_code, Some(StatusCode::OK), "{prefix:?}");
                assert_eq!(response.take_string().await.unwrap(), "protected");
            }
            for value in [
                "basic dXNlcjp3cm9uZw==",
                "BasicX dXNlcjpwYXNz",
                "BasicdXNlcjpwYXNz",
                "Basic\tdXNlcjpwYXNz",
                "basic ",
            ] {
                let response = TestClient::get("http://localhost/private/file.txt")
                    .add_header("Authorization", value, true)
                    .send(&service)
                    .await;
                assert_eq!(response.status_code, Some(StatusCode::UNAUTHORIZED));
                assert!(response.headers().contains_key("www-authenticate"));
            }
        }
    }

    #[tokio::test]
    async fn internal_names_are_hidden_from_json_and_webdav_collections() {
        let dir = TestDir::new();
        std::fs::create_dir(dir.path().join(".HPIP-INTERNAL-state")).unwrap();
        std::fs::write(dir.path().join(".hpip-upload-staging"), "incomplete").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "visible").unwrap();
        let service = Service::new(crate::build_router(config(dir.path(), &["-d"])));
        let mut response = TestClient::get("http://localhost/")
            .add_header("Accept", "application/json", true)
            .send(&service)
            .await;
        let json = response.take_string().await.unwrap();
        assert!(json.contains("visible.txt"));
        assert!(!json.to_ascii_lowercase().contains(".hpip-"));
        for body in [
            "",
            "<D:propfind xmlns:D=\"DAV:\"><D:prop><D:getcontentlength/></D:prop></D:propfind>",
        ] {
            let mut request = TestClient::get("http://localhost/")
                .add_header("Depth", "1", true)
                .text(body)
                .build();
            *request.method_mut() = "PROPFIND".parse().unwrap();
            let mut response = service.handle(request).await;
            assert_eq!(response.status_code, Some(StatusCode::MULTI_STATUS));
            let xml = response.take_string().await.unwrap();
            assert!(xml.contains("visible.txt"));
            assert!(!xml.to_ascii_lowercase().contains(".hpip-"));
        }
    }

    #[tokio::test]
    async fn collection_operations_require_all_descendant_policies() {
        let dir = TestDir::new();
        std::fs::create_dir_all(dir.path().join("collection/private")).unwrap();
        std::fs::write(dir.path().join("collection/private/file.txt"), "protected").unwrap();
        let service = Service::new(crate::build_router(config(
            dir.path(),
            &[
                "-w",
                "-d",
                "-A",
                "--path-auth",
                "collection/private=user:pass",
                "--path-auth",
                "target/private=other:secret",
            ],
        )));
        for accept in ["application/x-tar", "application/zip"] {
            let response = TestClient::get("http://localhost/collection/")
                .add_header("Accept", accept, true)
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::UNAUTHORIZED));
            let mut response = TestClient::get("http://localhost/collection/")
                .basic_auth("user", Some("pass"))
                .add_header("Accept", accept, true)
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::OK));
            assert!(!response.take_bytes(None).await.unwrap().is_empty());
        }
        for method in ["DELETE", "PROPFIND", "COPY", "MOVE"] {
            let mut request = TestClient::get("http://localhost/collection/")
                .add_header(
                    "Depth",
                    if method == "PROPFIND" {
                        "1"
                    } else {
                        "infinity"
                    },
                    true,
                )
                .add_header("Destination", "/target", true)
                .build();
            *request.method_mut() = method.parse().unwrap();
            let response = service.handle(request).await;
            assert_eq!(
                response.status_code,
                Some(StatusCode::UNAUTHORIZED),
                "{method}"
            );
            assert!(dir.path().join("collection/private/file.txt").exists());
            assert!(!dir.path().join("target").exists());
        }
        // Authorizing the source alone still cannot create a protected child
        // of an as-yet nonexistent destination collection.
        let mut request = TestClient::get("http://localhost/collection/")
            .basic_auth("user", Some("pass"))
            .add_header("Destination", "/target", true)
            .build();
        *request.method_mut() = "COPY".parse().unwrap();
        let response = service.handle(request).await;
        assert_eq!(response.status_code, Some(StatusCode::UNAUTHORIZED));
        assert!(!dir.path().join("target").exists());
    }

    #[tokio::test]
    async fn asynchronous_collection_checks_keep_the_same_authorization_results() {
        let dir = TestDir::new();
        std::fs::create_dir_all(dir.path().join("collection/private")).unwrap();
        std::fs::write(dir.path().join("collection/private/file"), b"protected").unwrap();
        let config = config(dir.path(), &["--path-auth", "collection/private=user:pass"]);
        for password in [None, Some("wrong"), Some("pass")] {
            let mut request = TestClient::get("http://localhost/collection/");
            if let Some(password) = password {
                request = request.basic_auth("user", Some(password));
            }
            let request = request.build();
            let expected = check_tree_auth(&request, &config, "/collection/")
                .map(|response| response.status_code);
            let actual = check_tree_auth_async(&request, &config, "/collection/")
                .await
                .map(|response| response.status_code);
            assert_eq!(actual, expected);
            assert_eq!(actual.is_none(), password == Some("pass"));
        }
    }

    #[tokio::test]
    async fn busy_collection_workers_reject_async_routes_without_side_effects() {
        let dir = TestDir::new();
        std::fs::create_dir_all(dir.path().join("collection/private")).unwrap();
        std::fs::write(dir.path().join("collection/private/file"), b"protected").unwrap();
        let config = config(
            dir.path(),
            &[
                "-w",
                "-d",
                "-A",
                "--path-auth",
                "collection/private=user:pass",
            ],
        );
        let _workers = config
            .blocking_jobs
            .clone()
            .acquire_many_owned(4)
            .await
            .unwrap();
        let service = Service::new(crate::build_router(config));
        for method in ["GET", "HEAD", "PROPFIND", "DELETE", "POST"] {
            let mut request = TestClient::get("http://localhost/collection/")
                .basic_auth("user", Some("pass"))
                .add_header("Accept", "application/x-tar", true)
                .add_header("Depth", "1", true)
                .add_header("Content-Type", "text/plain", true)
                .body("vendor=http\narchive=yes-i-really-want-one\ntype=tar\n")
                .build();
            *request.method_mut() = method.parse().unwrap();
            let response = service.handle(request).await;
            assert_eq!(
                response.status_code,
                Some(StatusCode::SERVICE_UNAVAILABLE),
                "{method}"
            );
            assert!(dir.path().join("collection/private/file").exists());
        }
    }

    #[tokio::test]
    async fn copy_and_archive_complete_with_one_available_worker_slot() {
        let dir = TestDir::new();
        std::fs::create_dir_all(dir.path().join("collection/private")).unwrap();
        std::fs::write(dir.path().join("collection/private/file"), b"protected").unwrap();
        let config = config(
            dir.path(),
            &[
                "-w",
                "-d",
                "-A",
                "--path-auth",
                "collection/private=user:pass",
            ],
        );
        let _workers = config
            .blocking_jobs
            .clone()
            .acquire_many_owned(3)
            .await
            .unwrap();
        let service = Service::new(crate::build_router(config));
        let mut request = TestClient::get("http://localhost/collection/")
            .basic_auth("user", Some("pass"))
            .add_header("Destination", "/copied", true)
            .build();
        *request.method_mut() = "COPY".parse().unwrap();
        let response =
            tokio::time::timeout(std::time::Duration::from_secs(2), service.handle(request))
                .await
                .unwrap();
        assert_eq!(response.status_code, Some(StatusCode::CREATED));
        assert_eq!(
            std::fs::read(dir.path().join("copied/private/file")).unwrap(),
            b"protected"
        );
        let mut response = TestClient::get("http://localhost/collection/")
            .basic_auth("user", Some("pass"))
            .add_header("Accept", "application/x-tar", true)
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::OK));
        let bytes =
            tokio::time::timeout(std::time::Duration::from_secs(2), response.take_bytes(None))
                .await
                .unwrap()
                .unwrap();
        let mut archive = tar::Archive::new(bytes.as_ref());
        assert!(archive.entries().unwrap().any(|entry| entry.unwrap().path().unwrap() == std::path::Path::new("private/file")));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recursive_alias_members_inherit_protection_outside_the_logical_subtree() {
        let dir = TestDir::new();
        std::fs::create_dir_all(dir.path().join("public")).unwrap();
        std::fs::create_dir_all(dir.path().join("private")).unwrap();
        std::fs::write(dir.path().join("private/file.txt"), "protected").unwrap();
        std::os::unix::fs::symlink("../private", dir.path().join("public/alias")).unwrap();
        let service = Service::new(crate::build_router(config(
            dir.path(),
            &["-d", "--path-auth", "private=user:pass"],
        )));
        let mut request = TestClient::get("http://localhost/public/")
            .add_header("Depth", "1", true)
            .build();
        *request.method_mut() = "PROPFIND".parse().unwrap();
        let response = service.handle(request).await;
        assert_eq!(response.status_code, Some(StatusCode::UNAUTHORIZED));
    }

    use super::*;
    use crate::test_support::{TestDir, config};

    #[tokio::test]
    async fn encoded_paths_require_the_same_credentials() {
        let dir = TestDir::new();
        std::fs::create_dir(dir.path().join("private")).unwrap();
        std::fs::write(dir.path().join("private/file.txt"), "protected").unwrap();
        let service = Service::new(crate::build_router(config(
            dir.path(),
            &["--path-auth", "private=user:pass"],
        )));
        for path in [
            "/private/file.txt",
            "/%70rivate/file.txt",
            "//private//file.txt",
        ] {
            let response = TestClient::get(format!("http://localhost{path}"))
                .send(&service)
                .await;
            assert_eq!(
                response.status_code,
                Some(StatusCode::UNAUTHORIZED),
                "{path}"
            );
        }
        let mut response = TestClient::get("http://localhost/%70rivate/file.txt")
            .basic_auth("user", Some("pass"))
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::OK));
        assert_eq!(response.take_string().await.unwrap(), "protected");
    }

    #[test]
    fn rules_match_component_boundaries() {
        assert!(path_rule_matches("private/file.txt", "private"));
        assert!(path_rule_matches("private", "private"));
        assert!(!path_rule_matches("private-other/file.txt", "private"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_case_aliases_cannot_bypass_rules() {
        let dir = TestDir::new();
        std::fs::create_dir(dir.path().join("Private")).unwrap();
        std::fs::write(dir.path().join("Private/file.txt"), "protected").unwrap();
        let service = Service::new(crate::build_router(config(
            dir.path(),
            &["--path-auth", "Private=user:pass"],
        )));
        let response = TestClient::get("http://localhost/private/file.txt")
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::UNAUTHORIZED));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn filesystem_aliases_inherit_target_protection() {
        let dir = TestDir::new();
        std::fs::create_dir(dir.path().join("private")).unwrap();
        std::fs::write(dir.path().join("private/file.txt"), "protected").unwrap();
        std::os::unix::fs::symlink("private", dir.path().join("alias")).unwrap();
        let service = Service::new(crate::build_router(config(
            dir.path(),
            &["--path-auth", "private=user:pass"],
        )));
        let response = TestClient::get("http://localhost/alias/file.txt")
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::UNAUTHORIZED));
        let response = TestClient::put("http://localhost/alias/new.txt")
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::UNAUTHORIZED));
    }
}
