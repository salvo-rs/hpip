use std::sync::Arc;

use salvo::prelude::*;

use crate::config::AppConfig;
use crate::util::write_conditions::{MUTATION_LOCK, WriteConditions};
use crate::util::*;

fn commit_delete(
    req: &Request,
    config: &Arc<AppConfig>,
    conditions: &WriteConditions,
) -> std::io::Result<Response> {
    let _guard = MUTATION_LOCK.blocking_lock();
    if let Some(response) = crate::hoops::auth::check_auth(req, config) {
        return Ok(response);
    }
    let segments: Vec<_> = req
        .uri()
        .path()
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    // Resolve the current entry, keeping DELETE's policy of removing links.
    let (path, symlink, invalid) = resolve_path(&config.hosted_directory.1, &segments, false);
    let mut response = Response::new();
    if invalid {
        response.status_code(StatusCode::BAD_REQUEST);
        return Ok(response);
    }
    if !path.exists() || config.is_symlink_denied(symlink, &path) {
        response.status_code(StatusCode::NOT_FOUND);
        return Ok(response);
    }
    let metadata = path.metadata()?;
    let is_file = is_actually_file(&metadata.file_type(), &path);
    if !is_file
        && let Some(response) = crate::hoops::auth::check_tree_auth(req, config, req.uri().path())
    {
        return Ok(response);
    }
    if !conditions.allows_path(&path)? {
        response.status_code(StatusCode::PRECONDITION_FAILED);
        return Ok(response);
    }
    if is_file {
        std::fs::remove_file(&path)?;
    } else {
        std::fs::remove_dir_all(&path)?;
    }
    response.status_code(StatusCode::NO_CONTENT);
    Ok(response)
}

#[handler]
pub async fn handle_delete(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let config = depot.get_typed::<Arc<AppConfig>>().unwrap().clone();

    if let Some(resp) = crate::hoops::auth::check_auth(req, &config) {
        *res = resp;
        return;
    }

    if config.writes_temp_dir.is_none() {
        res.status_code(StatusCode::FORBIDDEN);
        res.headers_mut()
            .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
        let body = error_html(
            "403 Forbidden",
            "This feature is currently disabled.",
            "<p>Ask the server administrator to pass <samp>-w</samp> to the executable to enable support for write requests.</p>",
        );
        res.headers_mut().insert(
            salvo::http::header::CONTENT_TYPE,
            "text/html; charset=utf-8".parse().unwrap(),
        );
        res.render(Text::Html(body));
        return;
    }

    let url_path_raw = req.uri().path().to_string();
    let decoded_path = percent_encoding::percent_decode_str(&url_path_raw)
        .decode_utf8()
        .map(|s| s.to_string());

    let segments: Vec<&str> = url_path_raw.split('/').filter(|s| !s.is_empty()).collect();

    // Don't follow symlinks for delete, just resolve to get the actual entry
    let (req_p, symlink, url_err) = resolve_path(&config.hosted_directory.1, &segments, false);

    if url_err || decoded_path.is_err() {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Text::Html(error_html(
            "400 Bad Request",
            "The request URL was invalid.",
            "<p>Percent-encoding decoded to invalid UTF-8.</p>",
        )));
        return;
    }
    let url_path_str = decoded_path.unwrap();

    if !req_p.exists() || config.is_symlink_denied(symlink, &req_p) {
        res.status_code(StatusCode::NOT_FOUND);
        res.render(Text::Html(error_html_text(
            "404 Not Found",
            format!("The requested entity \"{}\" doesn't exist.", url_path_str),
            "",
        )));
        return;
    }

    let remote = req.remote_addr().to_string();
    let is_file = req_p
        .metadata()
        .map(|m| is_actually_file(&m.file_type(), &req_p))
        .unwrap_or(false);

    if !is_file
        && let Some(response) =
            crate::hoops::auth::check_tree_auth_async(req, &config, req.uri().path()).await
    {
        *res = response;
        return;
    }

    let conditions = match WriteConditions::from_request(req) {
        Ok(conditions) => conditions,
        Err(status) => {
            res.status_code(status);
            return;
        }
    };
    let mut snapshot = Request::new();
    *snapshot.uri_mut() = req.uri().clone();
    *snapshot.method_mut() = req.method().clone();
    *snapshot.headers_mut() = req.headers().clone();
    *snapshot.remote_addr_mut() = req.remote_addr().clone();
    let work_config = config.clone();
    let Some(job) =
        config.spawn_blocking(move || commit_delete(&snapshot, &work_config, &conditions))
    else {
        res.status_code(StatusCode::SERVICE_UNAVAILABLE);
        return;
    };
    let result = job
        .await
        .unwrap_or_else(|error| Err(std::io::Error::other(error)));

    match result {
        Ok(response) => {
            if response.status_code == Some(StatusCode::NO_CONTENT) {
                crate::config::log_msg(config.log, &format!("{} deleted {}", remote, url_path_str));
            }
            *res = response;
            res.headers_mut()
                .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
        }
        Err(e) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Text::Html(error_html_text(
                "500 Internal Server Error",
                "Failed to delete the entity.",
                format!("{}", e),
            )));
        }
    }
}

#[cfg(test)]
mod error_text_tests {
    use salvo::test::{RequestBuilder, ResponseExt};

    use super::*;
    use crate::test_support::{TestDir, config};

    #[tokio::test]
    async fn delete_not_found_keeps_markup_in_the_path_as_text() {
        let directory = TestDir::new();
        let service = Service::new(crate::build_router(config(directory.path(), &["-w"])));
        let mut response = RequestBuilder::new(
            "http://localhost/%3Cimg%20data-review%3Dhpip%3E",
            salvo::http::Method::DELETE,
        )
        .send(&service)
        .await;
        assert_eq!(response.status_code, Some(StatusCode::NOT_FOUND));
        let html = response.take_string().await.unwrap();
        assert!(html.contains("&lt;img data-review=hpip&gt;"));
        assert!(!html.contains("<img data-review=hpip>"));
    }
}

#[cfg(test)]
mod commit_tests {
    use salvo::test::RequestBuilder;

    use super::*;
    use crate::test_support::{TestDir, config};

    #[test]
    fn commit_rechecks_collection_authority_and_current_type_after_publication() {
        for protected in [true, false] {
            let fixture = TestDir::new();
            let active = fixture.path().join("active");
            std::fs::write(&active, b"initial file").unwrap();
            let flags = if protected {
                vec!["-w", "--path-auth", "active/private=user:pass"]
            } else {
                vec!["-w"]
            };
            let config = config(fixture.path(), &flags);
            let snapshot =
                RequestBuilder::new("http://localhost/active", salvo::http::Method::DELETE).build();
            assert!(crate::hoops::auth::check_auth(&snapshot, &config).is_none());
            assert!(active.is_file());
            let prepared = fixture.path().join("prepared");
            std::fs::create_dir(&prepared).unwrap();
            std::fs::write(prepared.join("private"), b"collection member").unwrap();
            {
                let _guard = MUTATION_LOCK.blocking_lock();
                crate::util::namespace::rename_no_replace(&active, &fixture.path().join("retired"))
                    .unwrap();
                crate::util::namespace::rename_no_replace(&prepared, &active).unwrap();
            }
            let response = commit_delete(&snapshot, &config, &WriteConditions::default()).unwrap();
            assert_eq!(
                response.status_code,
                Some(if protected {
                    StatusCode::UNAUTHORIZED
                } else {
                    StatusCode::NO_CONTENT
                })
            );
            assert_eq!(active.exists(), protected);
            assert_eq!(
                std::fs::read(fixture.path().join("retired")).unwrap(),
                b"initial file"
            );
            if protected {
                assert_eq!(
                    std::fs::read(active.join("private")).unwrap(),
                    b"collection member"
                );
            }
        }
    }
}
