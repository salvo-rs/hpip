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
        crate::handler::archive::serve_archive_from_get(req, res, &config, archive_type).await;
        return;
    }

    let url_path_raw = req.uri().path().to_string();
    let decoded_path = percent_encoding::percent_decode_str(&url_path_raw)
        .decode_utf8()
        .map(|s| s.to_string());
    let Ok(url_path_str) = decoded_path else {
        handle_invalid_url(req, res, &config);
        return;
    };

    let segments: Vec<&str> = url_path_raw.split('/').filter(|s| !s.is_empty()).collect();

    let (mut req_p, mut symlink, url_err) = resolve_path(
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
            .find(|rp| rp.is_file())
    {
        symlink |= is_symlink(&rp);
        let logical = crate::util::path::decoded_url_path(&url_path_raw).unwrap();
        let logical = Path::new(&logical).with_extension(rp.extension().unwrap_or_default());
        if let Some(response) = crate::hoops::auth::check_decoded_auth(
            req,
            &config,
            &logical.to_string_lossy().replace('\\', "/"),
        ) {
            *res = response;
            return;
        }
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
        handle_get_file(req, res, &config, &req_p, false).await;
    } else {
        if let Some(index) = index_file(&config, &req_p) {
            let index_url = format!(
                "{}/index.{}",
                url_path_raw.trim_end_matches('/'),
                index
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .unwrap_or("")
            );
            if let Some(response) = crate::hoops::auth::check_auth_path(req, &config, &index_url) {
                *res = response;
                return;
            }
            if !url_path_raw.ends_with('/') {
                res.status_code(StatusCode::SEE_OTHER);
                res.headers_mut()
                    .insert("location", format!("{url_path_raw}/").parse().unwrap());
                return;
            }
            handle_get_file(req, res, &config, &index, false).await;
            return;
        }
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

/// GET and HEAD select the same authorized default-index candidate.
pub(super) fn index_file(config: &AppConfig, directory: &Path) -> Option<PathBuf> {
    config
        .check_indices
        .then(|| {
            INDEX_EXTENSIONS
                .iter()
                .map(|extension| directory.join("index").with_extension(extension))
                .find(|candidate| {
                    candidate.is_file()
                        && !config.is_symlink_denied(is_symlink(candidate), candidate)
                })
        })
        .flatten()
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
        if let Ok(file) = std::fs::File::open(try_404)
            && let Ok(metadata) = file.metadata()
        {
            res.status_code(StatusCode::NOT_FOUND);
            res.headers_mut().insert(
                salvo::http::header::CONTENT_TYPE,
                mime_type.parse().unwrap(),
            );
            res.headers_mut()
                .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
            let length = metadata.len();
            res.headers_mut().insert(
                salvo::http::header::CONTENT_LENGTH,
                length.to_string().parse().unwrap(),
            );
            stream_file(res, tokio::fs::File::from_std(file), length);
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
    let file = match tokio::fs::File::open(req_p).await {
        Ok(file) => file,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                res.status_code(StatusCode::FORBIDDEN);
                res.render(Text::Html(error_html_text(
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

    serve_open_file(req, res, config, req_p, file, is_404).await;
}

async fn serve_open_file(
    req: &mut Request,
    res: &mut Response,
    config: &Arc<AppConfig>,
    req_p: &Path,
    mut file: tokio::fs::File,
    is_404: bool,
) {
    let mime_type = guess_mime_type(req_p, &config.mime_type_overrides);
    let remote = req.remote_addr().to_string();

    let metadata = match file.metadata().await {
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

    if !is_404
        && let Some(range) = req
            .headers()
            .get(salvo::http::header::RANGE)
            .and_then(|header| header.to_str().ok())
            // Ignore unsupported units and multipart ranges rather than claiming
            // that the current representation cannot satisfy them.
            .filter(|range| range.starts_with("bytes=") && !range.contains(','))
            .map(str::to_owned)
    {
        let tag = format!("\"{etag}\"");
        if crate::http_cache::range_matches(req, &tag, modified) {
            if crate::http_cache::preconditions(req, res, Some(&tag), modified) {
                return;
            }
            handle_get_file_range(req, res, config, req_p, file, metadata, &range).await;
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
    if let Some(encoding) = file_encoding_candidate(req, config, req_p, flen)
        && let Some(encoded) =
            encoded_file_job(config, &file, req_p, &etag, encoding, &remote).await
    {
        let response_etag = format!("\"{}-{}\"", etag, encoding_name(encoding));
        if !is_404 && crate::http_cache::preconditions(req, res, Some(&response_etag), modified) {
            return;
        }
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
        res.headers_mut()
            .insert(salvo::http::header::ETAG, response_etag.parse().unwrap());
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
        res.headers_mut().insert(
            salvo::http::header::CONTENT_LENGTH,
            encoded.length.to_string().parse().unwrap(),
        );
        stream_file(res, tokio::fs::File::from_std(encoded.file), encoded.length);
        return;
    }

    // Serve file without encoding
    if !is_404
        && crate::http_cache::preconditions(req, res, Some(&format!("\"{}\"", etag)), modified)
    {
        return;
    }
    if file.seek(std::io::SeekFrom::Start(0)).await.is_err() {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        return;
    }

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

pub(super) fn file_encoding_candidate(
    req: &Request,
    config: &AppConfig,
    path: &Path,
    length: u64,
) -> Option<EncodingType> {
    if config.encoded_temp_dir.is_none()
        || length <= MIN_ENCODING_SIZE
        || length >= MAX_ENCODING_SIZE
        || path.extension().is_some_and(extension_is_blacklisted)
    {
        return None;
    }
    response_encoding(req.headers().get("accept-encoding")?.to_str().ok()?)
}

/// None means the coding requires reading the source to determine its gain.
/// Some(None) is a current negative cache entry; Some(Some(_)) pins an encoded file.
pub(super) fn cached_file_encoding(
    config: &AppConfig,
    path: &Path,
    etag: &str,
    encoding: EncodingType,
) -> Option<Option<EncodedFile>> {
    let hash = *config
        .cache_fs_files
        .read()
        .ok()?
        .get(&format!("{}:{etag}", path.display()))?;
    let cache = config.cache_fs.read().ok()?;
    let ((path, present, _), atime) = cache.get(&(hash, encoding))?;
    if *present {
        let encoded = EncodedFile::open(path)?;
        atime.store(precise_time_ns(), AtomicOrdering::Relaxed);
        Some(Some(encoded))
    } else if precise_time_ns().saturating_sub(atime.load(AtomicOrdering::Relaxed))
        <= crate::config::NEGATIVE_CACHE_TTL_NS
    {
        Some(None)
    } else {
        None
    }
}

pub(super) async fn encoded_file_job(
    config: &Arc<AppConfig>,
    source: &tokio::fs::File,
    path: &Path,
    etag: &str,
    encoding: EncodingType,
    remote: &str,
) -> Option<EncodedFile> {
    if let Some(cached) = cached_file_encoding(config, path, etag, encoding) {
        return cached;
    }
    let mut source = source.try_clone().await.ok()?.into_std().await;
    let work_config = config.clone();
    let path = path.to_path_buf();
    let etag = etag.to_owned();
    let remote = remote.to_owned();
    config
        .spawn_blocking(move || {
            try_encoded_file(&work_config, &mut source, &path, &etag, encoding, &remote)
        })?
        .await
        .ok()
        .flatten()
}

fn try_encoded_file(
    config: &AppConfig,
    source: &mut std::fs::File,
    req_p: &Path,
    etag: &str,
    encoding: EncodingType,
    remote: &str,
) -> Option<EncodedFile> {
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
            let h = file_hash_reader(source).ok()?;
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
        && let Some(encoded) = EncodedFile::open(&path)
    {
        return Some(encoded);
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
    if encode_open_file(source.try_clone().ok()?, &pending.0, encoding) {
        let resp_meta = pending.0.metadata().ok()?;
        let resp_len = resp_meta.len();
        let orig_len = source
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
        let encoded = EncodedFile::open(&resp_p)?;
        if !config.cache_file(cache_key, resp_p.clone(), resp_len) {
            let _ = std::fs::remove_file(&resp_p);
            return None;
        }

        return Some(encoded);
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

/// Opening before releasing the job keeps the representation alive across eviction.
pub(super) struct EncodedFile {
    file: std::fs::File,
    pub(super) length: u64,
}

impl EncodedFile {
    fn open(path: &Path) -> Option<Self> {
        let file = std::fs::File::open(path).ok()?;
        let length = file.metadata().ok()?.len();
        Some(Self { file, length })
    }
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
    mut file: tokio::fs::File,
    metadata: std::fs::Metadata,
    range_str: &str,
) {
    let remote = req.remote_addr().to_string();
    let mime_type = guess_mime_type(req_p, &config.mime_type_overrides);
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
) {
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
            res.render(Text::Html(error_html_text(
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
                        &mut std::fs::File::open(&path).unwrap(),
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
                    &mut flate2::read::GzDecoder::new(compressed.file),
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

    #[tokio::test]
    async fn cold_and_cached_compressed_responses_stream() {
        let dir = crate::test_support::TestDir::new();
        let data = "payload ".repeat(10_000);
        std::fs::write(dir.path().join("data.txt"), &data).unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &[],
        )));
        for _ in 0..2 {
            let mut response = TestClient::get("http://localhost/data.txt")
                .add_header("Accept-Encoding", "gzip", true)
                .send(&service)
                .await;
            assert_eq!(response.headers()["content-encoding"], "gzip");
            assert!(response.body.is_channel());
            assert_eq!(response.take_string().await.unwrap(), data);
        }
    }

    #[test]
    fn opened_encoding_survives_cache_eviction() {
        let dir = crate::test_support::TestDir::new();
        let path = dir.path().join("data.txt");
        let data = b"payload ".repeat(10_000);
        std::fs::write(&path, &data).unwrap();
        let config = crate::test_support::config(dir.path(), &["--encoded-filesystem", "4096"]);
        let encoded = super::try_encoded_file(
            &config,
            &mut std::fs::File::open(&path).unwrap(),
            &path,
            "test",
            crate::config::EncodingType::Gzip,
            "test",
        )
        .unwrap();
        let cached_path = config
            .cache_fs
            .read()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .0
            .0
            .clone();
        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, [0; 4096]).unwrap();
        assert!(config.cache_file(
            (
                blake3::hash(b"replacement"),
                crate::config::EncodingType::Gzip
            ),
            replacement,
            4096
        ));
        assert!(!cached_path.exists());
        let mut decoded = Vec::new();
        std::io::Read::read_to_end(
            &mut flate2::read::GzDecoder::new(encoded.file),
            &mut decoded,
        )
        .unwrap();
        assert_eq!(decoded, data);
    }

    #[tokio::test]
    async fn index_and_custom_error_files_also_stream() {
        let dir = crate::test_support::TestDir::new();
        let data = vec![31; 3 * 1024 * 1024];
        std::fs::write(dir.path().join("index.html"), &data).unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &[],
        )));
        let mut response = TestClient::get("http://localhost/").send(&service).await;
        assert_eq!(response.status_code, Some(StatusCode::OK));
        assert!(response.body.is_channel());
        assert_eq!(response.take_bytes(None).await.unwrap().as_ref(), data);

        let fallback = dir.path().join("error.html");
        std::fs::write(&fallback, &data).unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &["--404", fallback.to_str().unwrap()],
        )));
        let mut response = TestClient::get("http://localhost/missing")
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::NOT_FOUND));
        assert!(response.body.is_channel());
        assert_eq!(response.take_bytes(None).await.unwrap().as_ref(), data);
    }
}

#[cfg(test)]
mod selected_resource_tests {
    #[cfg(unix)]
    use salvo::test::ResponseExt;
    use salvo::test::TestClient;

    use super::*;
    use crate::test_support::{TestDir, config};

    #[tokio::test]
    async fn extension_candidates_cannot_turn_into_unprotected_directory_indexes() {
        let dir = TestDir::new();
        std::fs::create_dir(dir.path().join("asset.html")).unwrap();
        std::fs::write(dir.path().join("asset.html/index.html"), "protected index").unwrap();
        let service = Service::new(crate::build_router(config(
            dir.path(),
            &["-x", "--path-auth", "asset.html/index.html=user:pass"],
        )));
        let response = TestClient::get("http://localhost/asset.html/index.html")
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::UNAUTHORIZED));
        for path in ["/asset/", "/asset"] {
            let response = TestClient::get(format!("http://localhost{path}"))
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::NOT_FOUND), "{path}");
        }
    }

    #[tokio::test]
    async fn implicit_file_selection_preserves_the_selected_files_authentication() {
        let dir = TestDir::new();
        std::fs::write(dir.path().join("index.html"), "index contents").unwrap();
        std::fs::write(dir.path().join("asset.html"), "asset contents").unwrap();
        let service = Service::new(crate::build_router(config(
            dir.path(),
            &[
                "-x",
                "--path-auth",
                "index.html=user:pass",
                "--path-auth",
                "asset.html=user:pass",
            ],
        )));
        for path in ["/", "/asset"] {
            let response = TestClient::get(format!("http://localhost{path}"))
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::UNAUTHORIZED));
            let response = TestClient::get(format!("http://localhost{path}"))
                .basic_auth("user", Some("pass"))
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::OK));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn default_indexes_and_extension_candidates_obey_symlink_policy() {
        let fixture = TestDir::new();
        let hosted = fixture.path().join("served");
        std::fs::create_dir(&hosted).unwrap();
        std::fs::write(fixture.path().join("outside.txt"), "outside").unwrap();
        for name in ["index.html", "asset.html"] {
            std::os::unix::fs::symlink("../outside.txt", hosted.join(name)).unwrap();
        }
        for flags in [&["-l", "-x"][..], &["-l", "-x", "-s"], &["-l", "-x", "-r"]] {
            let service = Service::new(crate::build_router(config(&hosted, flags)));
            for path in ["/", "/asset"] {
                let mut response = TestClient::get(format!("http://localhost{path}"))
                    .send(&service)
                    .await;
                if flags.len() == 2 {
                    assert_eq!(response.status_code, Some(StatusCode::OK));
                    assert_eq!(response.take_string().await.unwrap(), "outside");
                } else {
                    assert_eq!(response.status_code, Some(StatusCode::NOT_FOUND));
                }
            }
        }
    }
}

#[cfg(test)]
mod snapshot_tests {
    use salvo::test::{ResponseExt, TestClient};

    use super::*;

    #[tokio::test]
    async fn full_and_range_headers_and_bytes_use_the_opened_version() {
        let dir = crate::test_support::TestDir::new();
        let path = dir.path().join("data.txt");
        let replacement = dir.path().join("replacement");
        let config = crate::test_support::config(dir.path(), &["-e"]);
        for range in [None, Some("bytes=1-4")] {
            std::fs::write(&path, b"original snapshot").unwrap();
            std::fs::write(
                &replacement,
                b"a longer replacement that must not be mixed into the response",
            )
            .unwrap();
            let opened = tokio::fs::File::open(&path).await.unwrap();
            let tag = format!("\"{}\"", file_etag(&opened.metadata().await.unwrap()));
            std::fs::rename(&replacement, &path).unwrap();
            let mut request = TestClient::get("http://localhost/data.txt").build();
            if let Some(range) = range {
                request
                    .headers_mut()
                    .insert("range", range.parse().unwrap());
                request
                    .headers_mut()
                    .insert("if-range", tag.parse().unwrap());
            }
            let mut response = Response::new();
            serve_open_file(&mut request, &mut response, &config, &path, opened, false).await;
            assert_eq!(response.headers()["etag"], tag);
            let expected: &[u8] = if range.is_some() {
                b"rigi"
            } else {
                b"original snapshot"
            };
            assert_eq!(
                response.headers()["content-length"],
                expected.len().to_string()
            );
            assert_eq!(response.take_bytes(None).await.unwrap().as_ref(), expected);
        }
    }

    #[test]
    fn hash_and_compression_share_the_opened_version_after_path_replacement() {
        let dir = crate::test_support::TestDir::new();
        let path = dir.path().join("data.txt");
        let replacement = dir.path().join("replacement");
        let original = b"original fixture ".repeat(10_000);
        std::fs::write(&path, &original).unwrap();
        std::fs::write(&replacement, b"different replacement ".repeat(12_000)).unwrap();
        let mut opened = std::fs::File::open(&path).unwrap();
        let tag = file_etag(&opened.metadata().unwrap());
        std::fs::rename(&replacement, &path).unwrap();
        let config = crate::test_support::config(dir.path(), &[]);
        let encoded = try_encoded_file(
            &config,
            &mut opened,
            &path,
            &tag,
            EncodingType::Gzip,
            "test",
        )
        .unwrap();
        let mut contents = Vec::new();
        std::io::Read::read_to_end(
            &mut flate2::read::GzDecoder::new(encoded.file),
            &mut contents,
        )
        .unwrap();
        assert_eq!(contents, original);
        assert!(
            config
                .cache_fs
                .read()
                .unwrap()
                .contains_key(&(blake3::hash(&original), EncodingType::Gzip))
        );
    }
}
