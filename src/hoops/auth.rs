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
    let url = req.uri().to_string();

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
        && auth_str.starts_with("Basic ")
        && let Ok(decoded) = base64_decode(&auth_str[6..])
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
    #[cfg(windows)]
    let (path, rule) = (path.to_lowercase(), rule.to_lowercase());
    path == rule
        || path
            .strip_prefix(&*rule)
            .is_some_and(|tail| tail.starts_with('/'))
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
            let response = TestClient::get("http://localhost/collection/")
                .basic_auth("user", Some("pass"))
                .add_header("Accept", accept, true)
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::OK));
        }
        for method in ["DELETE", "PROPFIND", "COPY", "MOVE"] {
            let mut request = TestClient::get("http://localhost/collection/")
                .add_header("Depth", "infinity", true)
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
            .add_header("Depth", "infinity", true)
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
