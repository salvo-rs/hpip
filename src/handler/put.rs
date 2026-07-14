use std::path::PathBuf;
use std::sync::Arc;

use salvo::prelude::*;
use tokio::io::AsyncWriteExt;

use crate::config::AppConfig;
use crate::util::*;

#[handler]
pub async fn handle_put(req: &mut Request, depot: &mut Depot, res: &mut Response) {
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

    let (req_p, symlink, url_err) = resolve_path(
        &config.hosted_directory.1,
        &segments,
        config.follow_symlinks,
    );

    if url_err {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Text::Html(error_html(
            "400 Bad Request",
            "The request URL was invalid.",
            "<p>Percent-encoding decoded to invalid UTF-8.</p>",
        )));
        return;
    }

    if req_p.is_dir() {
        res.status_code(StatusCode::METHOD_NOT_ALLOWED);
        res.render(Text::Html(error_html(
            "405 Method Not Allowed",
            "Can't PUT on a directory.",
            format!(
                "<p>Allowed methods: {}</p>",
                config.allowed_methods.join(", ")
            ),
        )));
        return;
    }

    if detect_file_as_dir(&req_p) {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Text::Html(error_html(
            "400 Bad Request",
            "The request URL was invalid.",
            "<p>Attempted to use file as directory.</p>",
        )));
        return;
    }

    if req.headers().get("content-range").is_some() {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Text::Html(error_html(
            "400 Bad Request",
            "<a href=\"https://tools.ietf.org/html/rfc7231#section-4.3.3\">RFC7231 forbids partial-content PUT requests.</a>",
            "",
        )));
        return;
    }

    let illegal = config.is_symlink_denied_nonexistent(symlink, &req_p);
    if illegal {
        res.status_code(StatusCode::NOT_FOUND);
        res.render(Text::Html(error_html(
            "404 Not Found",
            format!("The requested entity \"{}\" doesn't exist.", url_path_str),
            "",
        )));
        return;
    }

    // Read the body
    let body_bytes = match req.payload().await {
        Ok(data) => data.to_vec(),
        Err(_) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Text::Html(error_html(
                "503 Service Unavailable",
                "File not created.",
                "Failed to read request body",
            )));
            return;
        }
    };

    // Get modification time from headers
    let mtime = req
        .headers()
        .get("x-last-modified")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| {
            req.headers()
                .get("x-oc-mtime")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .map(|s| s * 1000)
        });

    let remote = req.remote_addr().to_string();

    // Ensure parent directories exist
    if let Some(parent) = req_p.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }

    let existent = req_p.exists();

    // Write file
    let result = write_file(&config, &req_p, &body_bytes).await;
    match result {
        Ok(()) => {
            if let Some(ms) = mtime {
                let path = req_p.clone();
                tokio::task::spawn_blocking(move || {
                    set_mtime(&path, ms);
                })
                .await
                .ok();
            }

            crate::config::log_msg(
                config.log,
                &format!(
                    "{} {} {}, size: {}B",
                    remote,
                    if existent { "replaced" } else { "created" },
                    req_p.display(),
                    body_bytes.len()
                ),
            );

            res.status_code(if existent {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::CREATED
            });
            res.headers_mut()
                .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
        }
        Err(e) => {
            crate::config::log_msg(config.log, &format!("{} File not created. {}", remote, e));
            res.status_code(StatusCode::SERVICE_UNAVAILABLE);
            res.render(Text::Html(error_html(
                "503 Service Unavailable",
                "File not created.",
                format!("{}", e),
            )));
        }
    }
}

async fn write_file(
    config: &AppConfig,
    req_p: &PathBuf,
    data: &[u8],
) -> Result<(), std::io::Error> {
    // Try direct creation first
    match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(req_p)
        .await
    {
        Ok(mut file) => {
            file.write_all(data).await?;
            file.flush().await?;
            Ok(())
        }
        Err(_) => {
            // File exists or error; use temp file approach
            config.create_temp_dir(&config.writes_temp_dir);
            let temp_dir = &config.writes_temp_dir.as_ref().unwrap().1;
            let temp_file_p =
                temp_dir.join(req_p.file_name().unwrap_or(std::ffi::OsStr::new("upload")));

            // Write to temp
            {
                let mut temp_file = tokio::fs::File::create(&temp_file_p).await?;
                temp_file.write_all(data).await?;
                temp_file.flush().await?;
            }

            // Copy temp to dest
            let result = tokio::fs::copy(&temp_file_p, req_p).await;
            let _ = tokio::fs::remove_file(&temp_file_p).await;
            result?;
            Ok(())
        }
    }
}
