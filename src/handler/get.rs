use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use salvo::prelude::*;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::config::{AppConfig, EncodingType, log_msg};
use crate::encoding::*;
use crate::options::WebDavLevel;
use crate::util::*;

#[handler]
pub async fn handle_get(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let config = depot.get_typed::<Arc<AppConfig>>().unwrap().clone();

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
            handle_get_file_range(req, res, &config, &req_p, &range_str).await;
            return;
        }
        handle_get_file(req, res, &config, &req_p, false).await;
    } else {
        let mut work_request = Request::new();
        *work_request.uri_mut() = req.uri().clone();
        *work_request.headers_mut() = req.headers().clone();
        let work_config = config.clone();
        match config.spawn_blocking(move || {
            let mut response = Response::new();
            handle_get_dir(
                &mut work_request,
                &mut response,
                &work_config,
                &req_p,
                &url_path_str,
                &url_path_raw,
            );
            response
        }) {
            Some(job) => match job.await {
                Ok(response) => *res = response,
                Err(_) => {
                    res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
                }
            },
            None => {
                res.status_code(StatusCode::SERVICE_UNAVAILABLE);
            }
        }
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
    config: &Arc<AppConfig>,
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
        && let Some(encoded_data) = encoded_file_job(config, req_p, &etag, encoding, &remote).await
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
    let file = match tokio::fs::File::open(req_p).await {
        Ok(file) => file,
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
    stream_file(res, file, flen);
}

async fn encoded_file_job(
    config: &Arc<AppConfig>,
    path: &Path,
    etag: &str,
    encoding: EncodingType,
    remote: &str,
) -> Option<Vec<u8>> {
    let work_config = config.clone();
    let path = path.to_path_buf();
    let etag = etag.to_owned();
    let remote = remote.to_owned();
    config
        .spawn_blocking(move || try_encoded_file(&work_config, &path, &etag, encoding, &remote))?
        .await
        .ok()
        .flatten()
}

fn try_encoded_file(
    config: &AppConfig,
    req_p: &Path,
    etag: &str,
    encoding: EncodingType,
    remote: &str,
) -> Option<Vec<u8>> {
    // Include the path because metadata tags can collide between different files.
    let index_key = format!("{}:{etag}", req_p.display());
    // Check if we have it cached
    let hash = {
        let cache = config.cache_fs_files.read().ok()?;
        cache.get(&index_key).cloned()
    };

    let hash = match hash {
        Some(h) => h,
        None => {
            let h = file_hash(req_p).ok()?;
            config.remember_hash(index_key, h);
            h
        }
    };

    let cache_key = (hash, encoding);
    // Fixed stripes bound coordination metadata while coalescing identical work.
    let _generation = config.encoding_gates[hash.as_bytes()[0] as usize % 64]
        .lock()
        .ok()?;

    let cached_path = {
        let cache = config.cache_fs.read().ok()?;
        match cache.get(&cache_key) {
            Some(((path, true, _), atime)) => {
                atime.store(precise_time_ns(), AtomicOrdering::Relaxed);
                Some(path.clone())
            }
            Some(((_, false, _), atime))
                if precise_time_ns().saturating_sub(atime.load(AtomicOrdering::Relaxed))
                    <= crate::config::NEGATIVE_CACHE_TTL_NS =>
            {
                return None;
            }
            Some(((_, false, _), _)) | None => None,
        }
    };
    if let Some(path) = cached_path
        && let Ok(data) = std::fs::read(path)
    {
        return Some(data);
    }

    // Encode the file
    config.create_temp_dir(&config.encoded_temp_dir);
    let temp_dir = &config.encoded_temp_dir.as_ref()?.1;
    static NEXT_ENCODING: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT_ENCODING.fetch_add(1, AtomicOrdering::Relaxed);
    let mut resp_p = temp_dir.join(format!(
        "{}-{}-{sequence}",
        cache_key.0.to_hex(),
        std::process::id()
    ));

    let ext_str = req_p.extension().and_then(|e| e.to_str()).unwrap_or("");
    let enc_ext = encoding_extension(encoding);
    if ext_str.is_empty() {
        resp_p.set_extension(enc_ext);
    } else {
        resp_p.set_extension(format!("{}.{}", ext_str, enc_ext));
    }

    let pending = PendingEncoding(resp_p.with_extension(format!("{}.partial", enc_ext)));
    if encode_file(req_p, &pending.0, encoding) {
        let resp_meta = pending.0.metadata().ok()?;
        let resp_len = resp_meta.len();
        let orig_len = req_p
            .metadata()
            .map(|m| file_length(&m, req_p))
            .unwrap_or(0);
        let gain = (orig_len as f64) / (resp_len as f64);

        if gain < MIN_ENCODING_GAIN || resp_len > config.encoded_filesystem_limit {
            // Not worth keeping
            config.cache_negative(cache_key);
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

        std::fs::rename(&pending.0, &resp_p).ok()?;
        let data = std::fs::read(&resp_p).ok()?;
        if !config.cache_file(cache_key, resp_p.clone(), resp_len) {
            let _ = std::fs::remove_file(&resp_p);
            return None;
        }

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

struct PendingEncoding(PathBuf);

impl Drop for PendingEncoding {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn handle_get_file_range(
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

    let Some((from, to)) = parse_byte_range(range_str, flen) else {
        res.status_code(StatusCode::RANGE_NOT_SATISFIABLE);
        res.headers_mut().insert(
            salvo::http::header::CONTENT_RANGE,
            format!("bytes */{flen}").parse().unwrap(),
        );
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

    let content_len = to - from + 1;
    let mut file = match tokio::fs::File::open(req_p).await {
        Ok(file) => file,
        Err(_) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            return;
        }
    };
    if file.seek(std::io::SeekFrom::Start(from)).await.is_err() {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        return;
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
    stream_file(res, file, content_len);
}

/// Normalize one byte range before doing arithmetic or allocating buffers.
fn parse_byte_range(header: &str, length: u64) -> Option<(u64, u64)> {
    if length == 0 {
        return None;
    }
    let spec = header.strip_prefix("bytes=")?;
    let (first, last) = spec.split_once('-')?;
    if first.is_empty() {
        let suffix = last.parse::<u64>().ok()?;
        (suffix != 0).then(|| (length.saturating_sub(suffix), length - 1))
    } else {
        let start = first.parse::<u64>().ok()?;
        let end = if last.is_empty() {
            length - 1
        } else {
            last.parse::<u64>().ok()?.min(length - 1)
        };
        (start < length && start <= end).then_some((start, end))
    }
}

/// The channel has backpressure; a request retains at most a small file chunk.
fn stream_file(res: &mut Response, mut file: tokio::fs::File, mut remaining: u64) {
    let mut sender = res.channel();
    tokio::spawn(async move {
        while remaining != 0 && !sender.is_closed() {
            let mut chunk = vec![0; remaining.min(64 * 1024) as usize];
            match file.read(&mut chunk).await {
                Ok(0) => {
                    sender.send_error(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "file shortened during transfer",
                    ));
                    break;
                }
                Ok(count) => {
                    chunk.truncate(count);
                    remaining -= count as u64;
                    if sender.send_data(chunk).await.is_err() {
                        break;
                    }
                }
                Err(error) => {
                    sender.send_error(error);
                    break;
                }
            }
        }
    });
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
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                let name = entry.file_name();
                if is_internal_name(&name) || config.is_symlink_denied(is_symlink(&path), &path) {
                    return None;
                }
                // Each entry is inspected once, never inside the sort comparator.
                let metadata = path.metadata().ok()?;
                let is_file = is_actually_file(&metadata.file_type(), &path);
                let name = name.to_string_lossy().into_owned();
                Some((is_file, name.to_lowercase(), name, path, metadata))
            })
            .collect();
        list.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));

        for (is_file, _, fname, path, fmeta) in list {
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
    // Fixed stripes bound coordination metadata while coalescing identical work.
    let _generation = config.encoding_gates[hash.as_bytes()[0] as usize % 64]
        .lock()
        .ok()?;

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

    config.cache_generated(cache_key, &enc_resp);

    Some((enc_resp, encoding))
}

#[cfg(test)]
mod range_tests {
    use salvo::prelude::*;
    use salvo::test::{ResponseExt, TestClient};

    use super::parse_byte_range;

    #[test]
    fn normalizes_ranges_without_overflow() {
        for (header, expected) in [
            ("bytes=0-8", Some((0, 5))),
            ("bytes=-20", Some((0, 5))),
            ("bytes=1-", Some((1, 5))),
            ("bytes=0-18446744073709551615", Some((0, 5))),
            ("bytes=18446744073709551615-", None),
            ("bytes=5-1", None),
            ("bytes=10-", None),
            ("bytes=-0", None),
            ("bytes=0-1,2-3", None),
            ("bytes=0-18446744073709551616", None),
        ] {
            assert_eq!(parse_byte_range(header, 6), expected, "{header}");
        }
        assert_eq!(parse_byte_range("bytes=0-", 0), None);
    }

    #[tokio::test]
    async fn range_responses_match_normalized_file_bytes() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("data.txt"), b"abcdef").unwrap();
        std::fs::write(dir.path().join("empty.txt"), b"").unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &[],
        )));
        for header in ["bytes=0-8", "bytes=-20", "bytes=0-18446744073709551615"] {
            let mut response = TestClient::get("http://localhost/data.txt")
                .add_header("Range", header, true)
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::PARTIAL_CONTENT));
            assert_eq!(response.headers()["content-range"], "bytes 0-5/6");
            assert_eq!(response.headers()["content-length"], "6");
            assert!(response.body.is_channel());
            assert_eq!(response.take_bytes(None).await.unwrap(), "abcdef");
        }
        for path in ["data.txt", "empty.txt"] {
            let response = TestClient::get(format!("http://localhost/{path}"))
                .add_header("Range", "bytes=10-", true)
                .send(&service)
                .await;
            assert_eq!(
                response.status_code,
                Some(StatusCode::RANGE_NOT_SATISFIABLE)
            );
            assert!(response.headers().contains_key("content-range"));
        }
    }

    #[tokio::test]
    async fn large_download_uses_a_channel_and_preserves_contents() {
        let dir = crate::test_support::TestDir::new();
        let data = vec![42; 3 * 1024 * 1024 + 7];
        std::fs::write(dir.path().join("data.bin"), &data).unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &["-e"],
        )));
        let mut response = TestClient::get("http://localhost/data.bin")
            .send(&service)
            .await;
        assert!(response.body.is_channel());
        assert_eq!(response.take_bytes(None).await.unwrap().as_ref(), data);
    }

    #[test]
    fn concurrent_cold_encodings_publish_one_complete_file() {
        let dir = crate::test_support::TestDir::new();
        let path = dir.path().join("data.txt");
        let data = b"compressible fixture\n".repeat(20_000);
        std::fs::write(&path, &data).unwrap();
        let config = crate::test_support::config(dir.path(), &[]);
        let barrier = std::sync::Barrier::new(12);
        std::thread::scope(|scope| {
            let mut tasks = Vec::new();
            for _ in 0..12 {
                tasks.push(scope.spawn(|| {
                    barrier.wait();
                    super::try_encoded_file(
                        &config,
                        &path,
                        "fixture",
                        crate::config::EncodingType::Gzip,
                        "test",
                    )
                    .unwrap()
                }));
            }
            for task in tasks {
                let compressed = task.join().unwrap();
                let mut decoded = Vec::new();
                std::io::Read::read_to_end(
                    &mut flate2::read::GzDecoder::new(compressed.as_slice()),
                    &mut decoded,
                )
                .unwrap();
                assert_eq!(decoded, data);
            }
        });
        let cache = config.cache_fs.read().unwrap();
        assert_eq!(cache.len(), 1);
        let ((cached_path, _, size), _) = cache.values().next().unwrap();
        assert_eq!(
            config
                .cache_fs_size
                .load(std::sync::atomic::Ordering::Relaxed),
            *size
        );
        assert_eq!(std::fs::metadata(cached_path).unwrap().len(), *size);
        assert_eq!(
            std::fs::read_dir(&config.encoded_temp_dir.as_ref().unwrap().1)
                .unwrap()
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn plain_files_stay_available_when_expensive_jobs_are_busy() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("data.txt"), b"available").unwrap();
        let config = crate::test_support::config(dir.path(), &["-A"]);
        let permits = config
            .blocking_jobs
            .clone()
            .acquire_many_owned(4)
            .await
            .unwrap();
        let service = Service::new(crate::build_router(config.clone()));
        let mut response = TestClient::get("http://localhost/data.txt")
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::OK));
        assert_eq!(response.take_string().await.unwrap(), "available");
        let response = TestClient::get("http://localhost/").send(&service).await;
        assert_eq!(response.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
        let response = TestClient::get("http://localhost/")
            .add_header("Accept", "application/x-tar", true)
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
        drop(permits);
    }

    #[tokio::test]
    async fn listing_sorts_snapshots_and_hides_private_staging_names() {
        let dir = crate::test_support::TestDir::new();
        std::fs::create_dir(dir.path().join("Zdirectory")).unwrap();
        std::fs::write(dir.path().join("afile.txt"), b"visible").unwrap();
        std::fs::write(dir.path().join(".HPIP-UPLOAD-secret"), b"private").unwrap();
        std::fs::create_dir(dir.path().join(".hpip-internal-secret")).unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &[],
        )));
        let mut response = TestClient::get("http://localhost/").send(&service).await;
        let body = response.take_string().await.unwrap();
        assert_eq!(response.status_code, Some(StatusCode::OK));
        assert!(body.find("Zdirectory").unwrap() < body.find("afile.txt").unwrap());
        assert!(!body.contains(".HPIP-UPLOAD-secret"));
        assert!(!body.contains(".hpip-internal-secret"));
    }
}
