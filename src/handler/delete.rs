use std::sync::Arc;

use salvo::prelude::*;

use crate::config::AppConfig;
use crate::util::*;

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

    let url_path_str = percent_encoding::percent_decode_str(req.uri().path())
        .decode_utf8()
        .map(|s| s.to_string())
        .unwrap_or_default();

    let segments: Vec<&str> = url_path_str.split('/').filter(|s| !s.is_empty()).collect();

    // Don't follow symlinks for delete, just resolve to get the actual entry
    let (req_p, symlink, url_err) = resolve_path(&config.hosted_directory.1, &segments, false);

    if url_err {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Text::Html(error_html(
            "400 Bad Request",
            "The request URL was invalid.",
            "<p>Percent-encoding decoded to invalid UTF-8.</p>",
        )));
        return;
    }

    if !req_p.exists() || config.is_symlink_denied(symlink, &req_p) {
        res.status_code(StatusCode::NOT_FOUND);
        res.render(Text::Html(error_html(
            "404 Not Found",
            format!("The requested entity \"{}\" doesn't exist.", url_path_str),
            "",
        )));
        return;
    }

    let remote = req.remote_addr().to_string();
    let req_p_clone = req_p.clone();
    let is_file = req_p
        .metadata()
        .map(|m| is_actually_file(&m.file_type(), &req_p))
        .unwrap_or(false);

    let result = if is_file {
        tokio::fs::remove_file(&req_p_clone).await
    } else {
        tokio::fs::remove_dir_all(&req_p_clone).await
    };

    match result {
        Ok(()) => {
            crate::config::log_msg(
                config.log,
                &format!(
                    "{} deleted {} {}",
                    remote,
                    if is_file {
                        "file"
                    } else if symlink {
                        "symlink"
                    } else {
                        "directory"
                    },
                    req_p_clone.display()
                ),
            );
            res.status_code(StatusCode::NO_CONTENT);
            res.headers_mut()
                .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
        }
        Err(e) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Text::Html(error_html(
                "500 Internal Server Error",
                "Failed to delete the entity.",
                format!("{}", e),
            )));
        }
    }
}
