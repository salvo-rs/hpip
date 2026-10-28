use std::sync::Arc;

use salvo::prelude::*;

use crate::config::{AppConfig, log_msg};
use crate::util::USER_AGENT;

/// Check authentication. Returns Some(Response) if auth fails, None if auth passes.
/// Takes config directly to avoid borrow conflicts with depot.
pub fn check_auth(req: &Request, config: &Arc<AppConfig>) -> Option<Response> {
    if config.global_auth_data.is_none() && config.path_auth_data.is_empty() {
        return None;
    }

    let mut auth = config.global_auth_data.as_ref();

    if !config.path_auth_data.is_empty() {
        let mut path = req.uri().path();
        if path.starts_with('/') {
            path = &path[1..];
        }
        if path.ends_with('/') {
            path = &path[..path.len() - 1];
        }

        let mut search_path = path;
        while !search_path.is_empty() {
            if let Some(pad) = config.path_auth_data.get(search_path) {
                auth = pad.as_ref();
                break;
            }
            search_path = &search_path[..search_path.rfind('/').unwrap_or(0)];
        }
    }

    let auth = auth?;

    let remote = req.remote_addr().to_string();
    let method = req.method().to_string();
    let url = req.uri().path().to_string();

    // Check Authorization header
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

        if auth.0 == username && auth.1 == password {
            log_msg(
                config.log,
                &format!("{} correctly authorised to {} {}", remote, method, url),
            );
            return None;
        } else {
            log_msg(
                config.log,
                &format!(
                    "{} requested to {} {} with invalid credentials",
                    remote, method, url
                ),
            );
            let mut resp = Response::new();
            resp.status_code(StatusCode::UNAUTHORIZED);
            resp.headers_mut()
                .insert("WWW-Authenticate", "Basic realm=\"hpip\"".parse().unwrap());
            resp.headers_mut()
                .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
            resp.render(Text::Plain("Supplied credentials invalid.\n"));
            return Some(resp);
        }
    }

    log_msg(
        config.log,
        &format!(
            "{} requested to {} {} without authorisation",
            remote, method, url
        ),
    );

    let mut resp = Response::new();
    resp.status_code(StatusCode::UNAUTHORIZED);
    resp.headers_mut()
        .insert("WWW-Authenticate", "Basic realm=\"hpip\"".parse().unwrap());
    resp.headers_mut()
        .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
    resp.render(Text::Plain("Credentials required.\n"));
    Some(resp)
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
