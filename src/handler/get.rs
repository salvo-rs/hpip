use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use salvo::prelude::*;

use crate::config::{AppConfig, EncodingType, log_msg};
use crate::encoding::compress::*;
use crate::encoding::*;
use crate::options::WebDavLevel;
use crate::util::*;

#[handler]
pub async fn handle_get(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let config = depot.obtain::<Arc<AppConfig>>().unwrap().clone();

    if let Some(resp) = crate::hoops::auth::check_auth(req, &config) {
        *res = resp;
        return;
    }

    // Check for RFSAPI (Accept: application/json)
    if crate::handler::rfsapi::wants_rfsapi(req) {
        crate::handler::rfsapi::handle_rfsapi_inner(req, res, &config).await;
        return;
    }

    // Check for archive request via Accept header
    if config.archives
        && let Some(archive_type) = crate::handler::archive::try_get_accept_archive(req)
    {
        crate::handler::archive::serve_archive_from_get(req, res, &config, archive_type);
        return;
    }

    let url_path_raw = req.uri().path().to_string();
    let url_path_str = percent_encoding::percent_decode_str(&url_path_raw)
        .decode_utf8()
        .map(|s| s.to_string())
        .unwrap_or_default();

    let segments: Vec<&str> = url_path_str.split('/').filter(|s| !s.is_empty()).collect();

    let (mut req_p, symlink, url_err) = resolve_path(
        &config.hosted_directory.1,
        &segments,
        config.follow_symlinks,
    );

    if url_err {
        handle_invalid_url(req, res, &config);
        return;
    }

    // Strip extensions: if file doesn't exist and has no extension, try with index extensions
    if !req_p.exists()
        && req_p.extension().is_none()
        && config.strip_extensions
        && let Some(rp) = INDEX_EXTENSIONS
            .iter()
            .map(|ext| req_p.with_extension(ext))
            .find(|rp| rp.exists())
    {
        req_p = rp;
    }

    if !req_p.exists() || config.is_symlink_denied(symlink, &req_p) {
        handle_nonexistent_get(req, res, &config, &req_p, &url_path_str);
        return;
    }

    let metadata = match req_p.metadata() {
        Ok(m) => m,
        Err(_) => {
            handle_nonexistent_get(req, res, &config, &req_p, &url_path_str);
            return;
        }
    };

    let is_file = is_actually_file(&metadata.file_type(), &req_p);

    if is_file {
        // Check for Range header - copy it before passing req mutably
        let range_header = req
            .headers()
            .get(salvo::http::header::RANGE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        if let Some(range_str) = range_header {
            handle_get_file_range(req, res, &config, &req_p, &range_str);
            return;
        }
        handle_get_file(req, res, &config, &req_p, false).await;
    } else {
        handle_get_dir(req, res, &config, &req_p, &url_path_str, &url_path_raw);
    }
}

fn handle_invalid_url(_req: &mut Request, res: &mut Response, config: &AppConfig) {
    let remote = ""; // simplified for now
    log_msg(config.log, &format!("{} invalid URL request", remote));

    let body = error_html(
        "400 Bad Request",
        "The request URL was invalid.",
        "<p>Percent-encoding decoded to invalid UTF-8.</p>",
    );
    res.status_code(StatusCode::BAD_REQUEST);
    res.headers_mut().insert(
        salvo::http::header::CONTENT_TYPE,
        "text/html; charset=utf-8".parse().unwrap(),
    );
    res.headers_mut()
        .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
    res.render(Text::Html(body));
}

fn handle_nonexistent_get(
    req: &mut Request,
    res: &mut Response,
    config: &AppConfig,
    req_p: &Path,
    url_path: &str,
) {
    let remote = req.remote_addr().to_string();
    log_msg(
        config.log,
        &format!(
            "{} requested nonexistent entity {}",
            remote,
            req_p.display()
        ),
    );

    // Try 404 fallback file
    if let Some(ref try_404) = config.try_404
        && try_404.metadata().map(|m| !m.is_dir()).unwrap_or(false)
    {
        let mime_type = guess_mime_type(try_404, &config.mime_type_overrides);
        if let Ok(data) = std::fs::read(try_404) {
            res.status_code(StatusCode::NOT_FOUND);
            res.headers_mut().insert(
                salvo::http::header::CONTENT_TYPE,
                mime_type.parse().unwrap(),
            );
            res.headers_mut()
                .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
            res.write_body(data).ok();
            return;
        }
    }

    let body = error_html(
        "404 Not Found",
        format!(
            "The requested entity \"{}\" doesn't exist.",
            NoHtmlLiteral(url_path)
        ),
        "",
    );
    res.status_code(StatusCode::NOT_FOUND);
    res.headers_mut().insert(
        salvo::http::header::CONTENT_TYPE,
        "text/html; charset=utf-8".parse().unwrap(),
    );
    res.headers_mut()
        .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
    res.render(Text::Html(body));
}

async fn handle_get_file(
    req: &mut Request,
    res: &mut Response,
    config: &AppConfig,
    req_p: &Path,
    is_404: bool,
) {
    let mime_type = guess_mime_type(req_p, &config.mime_type_overrides);
    let remote = req.remote_addr().to_string();

    let metadata = match req_p.metadata() {
        Ok(m) => m,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                res.status_code(StatusCode::FORBIDDEN);
                res.render(Text::Html(error_html(
                    "403 Forbidden",
                    "Access denied.",
                    "",
                )));
            } else {
                res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            }
            return;
        }
    };

    let etag = file_etag(&metadata);
    let modified = file_time_modified(&metadata);
    let flen = file_length(&metadata, &req_p);

    // Check If-None-Match / If-Modified-Since for 304
    if !is_404 {
        if let Some(inm) = req.headers().get(salvo::http::header::IF_NONE_MATCH)
            && let Ok(inm_str) = inm.to_str()
            && inm_str.contains(&etag)
        {
            log_msg(config.log, &format!("{} Not Modified", remote));
            res.status_code(StatusCode::NOT_MODIFIED);
            res.headers_mut().insert(
                salvo::http::header::ETAG,
                format!("\"{}\"", etag).parse().unwrap(),
            );
            res.headers_mut()
                .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
            return;
        }
        if let Some(ims) = req.headers().get(salvo::http::header::IF_MODIFIED_SINCE)
            && let Ok(ims_str) = ims.to_str()
            && let Ok(since) = chrono::DateTime::parse_from_rfc2822(ims_str)
            && modified <= since.with_timezone(&chrono::Utc)
        {
            log_msg(config.log, &format!("{} Not Modified", remote));
            res.status_code(StatusCode::NOT_MODIFIED);
            res.headers_mut().insert(
                salvo::http::header::ETAG,
                format!("\"{}\"", etag).parse().unwrap(),
            );
            res.headers_mut()
                .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
            return;
        }
    }

    log_msg(
        config.log,
        &format!(
            "{} was served file {} as {}",
            remote,
            req_p.display(),
            mime_type
        ),
    );

    // Check if we should try encoding
    if config.encoded_temp_dir.is_some()
        && flen > MIN_ENCODING_SIZE
        && flen < MAX_ENCODING_SIZE
        && req_p
            .extension()
            .map(|s| !extension_is_blacklisted(s))
            .unwrap_or(true)
        && let Some(accept_enc) = req
            .headers()
            .get(salvo::http::header::ACCEPT_ENCODING)
            .and_then(|v| v.to_str().ok())
        && let Some(encoding) = response_encoding(accept_enc)
        && let Some(encoded_data) = try_encoded_file(config, req_p, &etag, encoding, &remote)
    {
        res.status_code(if is_404 {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::OK
        });
        res.headers_mut().insert(
            salvo::http::header::CONTENT_TYPE,
            mime_type.parse().unwrap(),
        );
        res.headers_mut().insert(
            salvo::http::header::CONTENT_ENCODING,
            encoding_name(encoding).parse().unwrap(),
        );
        res.headers_mut().insert(
            salvo::http::header::ETAG,
            format!("\"{}\"", etag).parse().unwrap(),
        );
        res.headers_mut().insert(
            salvo::http::header::LAST_MODIFIED,
            modified
                .format("%a, %d %b %Y %T GMT")
                .to_string()
                .parse()
                .unwrap(),
        );
        res.headers_mut()
            .insert(salvo::http::header::ACCEPT_RANGES, "bytes".parse().unwrap());
        res.headers_mut()
            .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
        res.write_body(encoded_data).ok();
        return;
    }

    // Serve file without encoding
    let file_data = match tokio::fs::read(req_p).await {
        Ok(data) => data,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                res.status_code(StatusCode::FORBIDDEN);
                res.render(Text::Html(error_html(
                    "403 Forbidden",
                    format!("Can't access {}.", req_p.display()),
                    "",
                )));
            } else {
                res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            }
            return;
        }
    };

    res.status_code(if is_404 {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::OK
    });
    res.headers_mut().insert(
        salvo::http::header::CONTENT_TYPE,
        mime_type.parse().unwrap(),
    );
    res.headers_mut().insert(
        salvo::http::header::ETAG,
        format!("\"{}\"", etag).parse().unwrap(),
    );
    res.headers_mut().insert(
        salvo::http::header::LAST_MODIFIED,
        modified
            .format("%a, %d %b %Y %T GMT")
            .to_string()
            .parse()
            .unwrap(),
    );
    res.headers_mut()
        .insert(salvo::http::header::ACCEPT_RANGES, "bytes".parse().unwrap());
    res.headers_mut().insert(
        salvo::http::header::CONTENT_LENGTH,
        flen.to_string().parse().unwrap(),
    );
    res.headers_mut()
        .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
    res.write_body(file_data).ok();
}

fn try_encoded_file(
    config: &AppConfig,
    req_p: &Path,
    etag: &str,
    encoding: EncodingType,
    remote: &str,
) -> Option<Vec<u8>> {
    // Check if we have it cached
    let hash = {
        let cache = config.cache_fs_files.read().ok()?;
        cache.get(etag).cloned()
    };

    let hash = match hash {
        Some(h) => h,
        None => {
            let h = file_hash(req_p).ok()?;
            config
                .cache_fs_files
                .write()
                .ok()?
                .insert(etag.to_string(), h);
            h
        }
    };

    let cache_key = (hash, encoding);

    // Check fs cache
    {
        let cache = config.cache_fs.read().ok()?;
        if let Some(((resp_p, true, _), atime)) = cache.get(&cache_key)
            && let Ok(data) = std::fs::read(resp_p)
        {
            atime.store(precise_time_ns(), AtomicOrdering::Relaxed);
            let orig_len = req_p
                .metadata()
                .map(|m| file_length(&m, req_p))
                .unwrap_or(0);
            log_msg(
                config.log,
                &format!(
                    "{} encoded as {} for {:.1}% ratio (cached)",
                    remote,
                    encoding_name(encoding),
                    (orig_len as f64) / (data.len() as f64) * 100.0
                ),
            );
            return Some(data);
        }
        if let Some(((_, false, _), _)) = cache.get(&cache_key) {
            // Previously determined not worth encoding
            return None;
        }
    }

    // Encode the file
    config.create_temp_dir(&config.encoded_temp_dir);
    let temp_dir = &config.encoded_temp_dir.as_ref()?.1;
    let mut resp_p = temp_dir.join(cache_key.0.to_hex().as_str());

    let ext_str = req_p.extension().and_then(|e| e.to_str()).unwrap_or("");
    let enc_ext = encoding_extension(encoding);
    if ext_str.is_empty() {
        resp_p.set_extension(enc_ext);
    } else {
        resp_p.set_extension(format!("{}.{}", ext_str, enc_ext));
    }

    if encode_file(req_p, &resp_p, encoding) {
        let resp_meta = resp_p.metadata().ok()?;
        let resp_len = resp_meta.len();
        let orig_len = req_p
            .metadata()
            .map(|m| file_length(&m, req_p))
            .unwrap_or(0);
        let gain = (orig_len as f64) / (resp_len as f64);

        if gain < MIN_ENCODING_GAIN || resp_len > config.encoded_filesystem_limit {
            // Not worth keeping
            let mut cache = config.cache_fs.write().ok()?;
            cache.insert(
                cache_key,
                ((PathBuf::new(), false, 0), AtomicU64::new(u64::MAX)),
            );
            let _ = std::fs::remove_file(&resp_p);
            return None;
        }

        log_msg(
            config.log,
            &format!(
                "{} encoded as {} for {:.1}% ratio",
                remote,
                encoding_name(encoding),
                gain * 100.0
            ),
        );

        let data = std::fs::read(&resp_p).ok()?;
        let mut cache = config.cache_fs.write().ok()?;
        config
            .cache_fs_size
            .fetch_add(resp_len, AtomicOrdering::Relaxed);
        cache.insert(
            cache_key,
            ((resp_p, true, resp_len), AtomicU64::new(precise_time_ns())),
        );

        return Some(data);
    }

    log_msg(
        config.log,
        &format!(
            "{} failed to encode as {}, sending identity",
            remote,
            encoding_name(encoding)
        ),
    );
    None
}

fn handle_get_file_range(
    req: &mut Request,
    res: &mut Response,
    config: &AppConfig,
    req_p: &Path,
    range_str: &str,
) {
    let remote = req.remote_addr().to_string();
    let mime_type = guess_mime_type(req_p, &config.mime_type_overrides);
    let metadata = match req_p.metadata() {
        Ok(m) => m,
        Err(_) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            return;
        }
    };
    let flen = file_length(&metadata, req_p);
    let etag = file_etag(&metadata);
    let modified = file_time_modified(&metadata);

    // Parse "bytes=N-M" range header
    if !range_str.starts_with("bytes=") {
        res.status_code(StatusCode::RANGE_NOT_SATISFIABLE);
        res.render(Text::Html(error_html(
            "416 Range Not Satisfiable",
            "Custom ranges are unsupported.",
            "",
        )));
        return;
    }

    let range_spec = &range_str[6..];
    // Only support single range
    if range_spec.contains(',') {
        res.status_code(StatusCode::RANGE_NOT_SATISFIABLE);
        res.render(Text::Html(error_html(
            "416 Range Not Satisfiable",
            "More than one range is unsupported.",
            "",
        )));
        return;
    }

    let (from, to) = if let Some(pos) = range_spec.find('-') {
        let from_str = &range_spec[..pos];
        let to_str = &range_spec[pos + 1..];

        if from_str.is_empty() {
            // suffix range: -N
            let n: u64 = match to_str.parse() {
                Ok(n) => n,
                Err(_) => {
                    res.status_code(StatusCode::RANGE_NOT_SATISFIABLE);
                    return;
                }
            };
            if n > flen {
                res.status_code(StatusCode::RANGE_NOT_SATISFIABLE);
                return;
            }
            (flen - n, flen - 1)
        } else if to_str.is_empty() {
            // open-ended: N-
            let from: u64 = match from_str.parse() {
                Ok(n) => n,
                Err(_) => {
                    res.status_code(StatusCode::RANGE_NOT_SATISFIABLE);
                    return;
                }
            };
            if from >= flen {
                res.status_code(StatusCode::NO_CONTENT);
                return;
            }
            (from, flen - 1)
        } else {
            // closed: N-M
            let from: u64 = match from_str.parse() {
                Ok(n) => n,
                Err(_) => {
                    res.status_code(StatusCode::RANGE_NOT_SATISFIABLE);
                    return;
                }
            };
            let to: u64 = match to_str.parse() {
                Ok(n) => n,
                Err(_) => {
                    res.status_code(StatusCode::RANGE_NOT_SATISFIABLE);
                    return;
                }
            };
            (from, to)
        }
    } else {
        res.status_code(StatusCode::RANGE_NOT_SATISFIABLE);
        return;
    };

    log_msg(
        config.log,
        &format!(
            "{} was served byte range {}-{} of file {} as {}",
            remote,
            from,
            to,
            req_p.display(),
            mime_type
        ),
    );

    let content_len = to + 1 - from;
    let mut f = match File::open(req_p) {
        Ok(f) => f,
        Err(_) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            return;
        }
    };

    if f.seek(SeekFrom::Start(from)).is_err() {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        return;
    }

    let mut buf = vec![0u8; content_len as usize];
    if f.read_exact(&mut buf).is_err() {
        // Read what we can
        buf.truncate(0);
        f.seek(SeekFrom::Start(from)).ok();
        let mut temp = Vec::new();
        f.take(content_len).read_to_end(&mut temp).ok();
        buf = temp;
    }

    res.status_code(StatusCode::PARTIAL_CONTENT);
    res.headers_mut().insert(
        salvo::http::header::CONTENT_TYPE,
        mime_type.parse().unwrap(),
    );
    res.headers_mut().insert(
        salvo::http::header::CONTENT_RANGE,
        format!("bytes {}-{}/{}", from, to, flen).parse().unwrap(),
    );
    res.headers_mut().insert(
        salvo::http::header::CONTENT_LENGTH,
        content_len.to_string().parse().unwrap(),
    );
    res.headers_mut().insert(
        salvo::http::header::ETAG,
        format!("\"{}+{}-{}\"", etag, from, to).parse().unwrap(),
    );
    res.headers_mut().insert(
        salvo::http::header::LAST_MODIFIED,
        modified
            .format("%a, %d %b %Y %T GMT")
            .to_string()
            .parse()
            .unwrap(),
    );
    res.headers_mut()
        .insert(salvo::http::header::ACCEPT_RANGES, "bytes".parse().unwrap());
    res.headers_mut()
        .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
    res.write_body(buf).ok();
}

fn handle_get_dir(
    req: &mut Request,
    res: &mut Response,
    config: &AppConfig,
    req_p: &Path,
    url_path: &str,
    url_path_raw: &str,
) {
    // Check for index files
    if config.check_indices {
        let mut idx = req_p.join("index");
        for ext in INDEX_EXTENSIONS {
            idx.set_extension(ext);
            if idx.exists()
                && (!config.follow_symlinks
                    || !config.sandbox_symlinks
                    || is_descendant_of(req_p, &config.hosted_directory.1))
            {
                // Check if URL ends with slash
                if url_path_raw.ends_with('/') {
                    // Serve index file directly
                    let mime_type = guess_mime_type(&idx, &config.mime_type_overrides);
                    let remote = req.remote_addr().to_string();
                    log_msg(
                        config.log,
                        &format!(
                            "{} found index file for directory {}",
                            remote,
                            req_p.display()
                        ),
                    );
                    if let Ok(data) = std::fs::read(&idx) {
                        let metadata = idx.metadata().ok();
                        res.status_code(StatusCode::OK);
                        res.headers_mut().insert(
                            salvo::http::header::CONTENT_TYPE,
                            mime_type.parse().unwrap(),
                        );
                        if let Some(ref m) = metadata {
                            let etag = file_etag(m);
                            res.headers_mut().insert(
                                salvo::http::header::ETAG,
                                format!("\"{}\"", etag).parse().unwrap(),
                            );
                            res.headers_mut().insert(
                                salvo::http::header::LAST_MODIFIED,
                                file_time_modified(m)
                                    .format("%a, %d %b %Y %T GMT")
                                    .to_string()
                                    .parse()
                                    .unwrap(),
                            );
                        }
                        res.headers_mut()
                            .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
                        res.write_body(data).ok();
                        return;
                    }
                } else {
                    // Redirect to add trailing slash
                    let new_url = format!("{}/", url_path_raw);
                    log_msg(
                        config.log,
                        &format!(
                            "Redirecting to {} - found index file index.{}",
                            new_url, ext
                        ),
                    );
                    res.status_code(StatusCode::SEE_OTHER);
                    res.headers_mut()
                        .insert(salvo::http::header::LOCATION, new_url.parse().unwrap());
                    res.headers_mut()
                        .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
                    return;
                }
            }
        }
    }

    if !config.generate_listings {
        handle_nonexistent_get(req, res, config, req_p, url_path);
        return;
    }

    handle_dir_listing(req, res, config, req_p, url_path);
}

fn handle_dir_listing(
    req: &mut Request,
    res: &mut Response,
    config: &AppConfig,
    req_p: &Path,
    url_path: &str,
) {
    let remote = req.remote_addr().to_string();
    let is_root = url_path == "/";
    let mut relpath_escaped = escape_specials(url_path);
    if relpath_escaped.as_bytes().last() != Some(&b'/') {
        relpath_escaped.to_mut().push('/');
    }
    let show_file_management_controls = config.writes_temp_dir.is_some();

    log_msg(
        config.log,
        &format!(
            "{} was served directory listing for {}",
            remote,
            req_p.display()
        ),
    );

    let parent_f = |out: &mut Vec<u8>| {
        if !is_root {
            let mut parentpath = relpath_escaped.as_bytes();
            while parentpath.last() == Some(&b'/') {
                parentpath = &parentpath[0..parentpath.len() - 1];
            }
            while parentpath.last() != Some(&b'/') {
                parentpath = &parentpath[0..parentpath.len() - 1];
            }
            let modified = file_time_modified_p(req_p.parent().unwrap_or(req_p));
            let _ = write!(
                out,
                "<tr id=\"..\"><td><a href=\"{}\" tabindex=\"-1\" class=\"back_arrow_icon\"></a></td> <td><a \
                 href=\"{}\">Parent directory</a></td> <td><a href=\"{}\" tabindex=\"-1\"><time ms={}>{}</time></a></td> \
                 <td><a href=\"{}\" tabindex=\"-1\">&nbsp;</a></td> <td><a href=\"{}\" tabindex=\"-1\">&nbsp;</a></td></tr>",
                unsafe { std::str::from_utf8_unchecked(parentpath) },
                unsafe { std::str::from_utf8_unchecked(parentpath) },
                unsafe { std::str::from_utf8_unchecked(parentpath) },
                modified.timestamp_millis(),
                modified.format("%F %T"),
                unsafe { std::str::from_utf8_unchecked(parentpath) },
                unsafe { std::str::from_utf8_unchecked(parentpath) },
            );
        }
    };

    let rd = match req_p.read_dir() {
        Ok(rd) => rd,
        Err(_) => {
            res.status_code(StatusCode::FORBIDDEN);
            res.render(Text::Html(error_html(
                "403 Forbidden",
                format!("Can't access {}.", url_path),
                "",
            )));
            return;
        }
    };

    let list_f = |out: &mut Vec<u8>| {
        let mut list: Vec<_> = rd
            .filter_map(|p| p.ok())
            .filter(|f| {
                let fp = f.path();
                let symlink = is_symlink(&fp);
                !config.is_symlink_denied(symlink, &fp)
            })
            .collect();

        list.sort_by(|lhs, rhs| {
            let lhs_is_file = is_actually_file(
                &lhs.file_type()
                    .unwrap_or(lhs.metadata().unwrap().file_type()),
                lhs.path(),
            );
            let rhs_is_file = is_actually_file(
                &rhs.file_type()
                    .unwrap_or(rhs.metadata().unwrap().file_type()),
                rhs.path(),
            );
            (
                lhs_is_file,
                lhs.file_name().to_string_lossy().to_lowercase(),
            )
                .cmp(&(
                    rhs_is_file,
                    rhs.file_name().to_string_lossy().to_lowercase(),
                ))
        });

        for f in list {
            let path = f.path();
            let is_file = path
                .metadata()
                .map(|m| is_actually_file(&m.file_type(), &path))
                .unwrap_or(false);
            let fmeta = match f.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let fname = f.file_name().to_string_lossy().to_string();
            let len = file_length(&fmeta, &path);
            let modified = file_time_modified(&fmeta);

            let size_display = if is_file {
                format!(
                    "<abbr title=\"{} B\">{}</abbr>",
                    len,
                    HumanReadableSize(len)
                )
            } else {
                "&nbsp;".to_string()
            };

            let manage_col = if show_file_management_controls {
                let mut s = String::from(
                    "<td><a href class=\"delete_file_icon\" onclick=\"delete_onclick(arguments[0])\">Delete</a>",
                );
                if config.webdav >= WebDavLevel::MkColMoveOnly {
                    s.push_str(" <a href class=\"rename_icon\" onclick=\"rename_onclick(arguments[0])\">Rename</a>");
                }
                s.push_str("</td>");
                s
            } else {
                String::new()
            };

            let escaped_fname = encode_tail_if_trimmed(escape_specials(&fname));

            let _ = writeln!(
                out,
                "<tr id=\"{}\"><td><a href=\"{}{}\" tabindex=\"-1\" class=\"{}{}_icon\"></a></td> <td><a \
                 href=\"{}{}\">{}{}</a></td> <td><a href=\"{}{}\" tabindex=\"-1\"><time ms={}>{}</time></a></td> \
                 <td><a href=\"{}{}\" tabindex=\"-1\">{}</a></td> {}</tr>",
                NoDoubleQuotes(&fname),
                relpath_escaped,
                escaped_fname,
                if is_file { "file" } else { "dir" },
                file_icon_suffix(&path, is_file),
                relpath_escaped,
                escaped_fname,
                NoHtmlLiteral(&fname),
                if is_file { "" } else { "/" },
                relpath_escaped,
                escaped_fname,
                modified.timestamp_millis(),
                modified.format("%F %T"),
                relpath_escaped,
                escaped_fname,
                size_display,
                manage_col,
            );
        }
    };

    let body = directory_listing_html(
        &relpath_escaped[!is_root as usize..],
        if show_file_management_controls {
            concat!(
                r#"<style>"#,
                include_str!(concat!(env!("OUT_DIR"), "/assets/upload.css")),
                r#"</style>"#,
                r#"<script>"#,
                include_str!(concat!(env!("OUT_DIR"), "/assets/upload.js"))
            )
        } else {
            ""
        },
        if show_file_management_controls {
            include_str!(concat!(env!("OUT_DIR"), "/assets/manage_desktop.js"))
        } else {
            ""
        },
        if show_file_management_controls {
            concat!(
                include_str!(concat!(env!("OUT_DIR"), "/assets/manage.js")),
                r#"</script>"#
            )
        } else {
            ""
        },
        parent_f,
        list_f,
        if show_file_management_controls {
            "<hr /><p>Upload via drag&drop, paste, or <input type=\"file\" multiple />.</p>"
        } else {
            ""
        },
        if show_file_management_controls {
            "<th>Manage</th>"
        } else {
            ""
        },
        if show_file_management_controls && config.webdav >= WebDavLevel::MkColMoveOnly {
            "<tr id='new\"directory'><td><a tabindex=\"-1\" href class=\"new_dir_icon\"></a></td><td colspan=3><a href>Create directory</a></td><td><a tabindex=\"-1\" href>&nbsp;</a></td></tr>"
        } else {
            ""
        },
        if config.archives {
            concat!(
                "<hr /><form method=post enctype=text/plain><p>Archive as ",
                include_str!(concat!(
                    env!("OUT_DIR"),
                    "/assets/directory_listing_achive_inputs.html"
                )),
                ".</p></form>"
            )
        } else {
            ""
        },
    );

    // Try encoding the generated response
    let encoded_body = try_encode_generated_response(req, config, &body);

    res.status_code(StatusCode::OK);
    res.headers_mut().insert(
        salvo::http::header::CONTENT_TYPE,
        "text/html; charset=utf-8".parse().unwrap(),
    );
    res.headers_mut()
        .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());

    if let Some((enc_data, enc_type)) = encoded_body {
        res.headers_mut().insert(
            salvo::http::header::CONTENT_ENCODING,
            encoding_name(enc_type).parse().unwrap(),
        );
        res.write_body(enc_data).ok();
    } else {
        res.write_body(body.into_bytes()).ok();
    }
}

fn try_encode_generated_response(
    req: &mut Request,
    config: &AppConfig,
    body: &str,
) -> Option<(Vec<u8>, EncodingType)> {
    let accept_enc = req
        .headers()
        .get(salvo::http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())?;

    let encoding = response_encoding(accept_enc)?;
    let hash = blake3::hash(body.as_bytes());
    let cache_key = (hash, encoding);

    // Check gen cache
    {
        if let Ok(cache) = config.cache_gen.read()
            && let Some((enc_resp, atime)) = cache.get(&cache_key)
        {
            atime.store(precise_time_ns(), AtomicOrdering::Relaxed);
            return Some((enc_resp.clone(), encoding));
        }
    }

    // Encode
    let enc_resp = encode_str(body, encoding)?;

    if enc_resp.len() as u64 <= config.encoded_generated_limit
        && let Ok(mut cache) = config.cache_gen.write()
    {
        config
            .cache_gen_size
            .fetch_add(enc_resp.len() as u64, AtomicOrdering::Relaxed);
        cache.insert(
            cache_key,
            (enc_resp.clone(), AtomicU64::new(precise_time_ns())),
        );
    }

    Some((enc_resp, encoding))
}
