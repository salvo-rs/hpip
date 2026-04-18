#[cfg(unix)]
use std::collections::btree_map::{BTreeMap, Entry as BTreeMapEntry};
use std::io::{self, ErrorKind as IoErrorKind, Result as IoResult};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::str;
use std::sync::Arc;
use std::time::SystemTime;

use salvo::prelude::*;
use tar::Builder as TarBuilder;
#[cfg(unix)]
use tar::{EntryType as TarEntryType, Header as TarHeader};
use walkdir::WalkDir;
use zip::CompressionMethod as ZipCompressionMethod;
use zip::write::{FullFileOptions as ZipFileOptions, ZipWriter};

use crate::config::{AppConfig, log_msg};
use crate::encoding::{MAX_ENCODING_SIZE, MIN_ENCODING_SIZE, extension_is_blacklisted};
use crate::util::*;

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum ArchiveType {
    Tar,
    Zip,
}

impl ArchiveType {
    pub fn from_mime(mime: &str) -> Option<ArchiveType> {
        let lower = mime.to_ascii_lowercase();
        if !lower.starts_with("application/") {
            return None;
        }
        let sub = &lower["application/".len()..];
        match sub {
            "x-tar" | "tar" => Some(ArchiveType::Tar),
            "zip" | "x-zip-compressed" => Some(ArchiveType::Zip),
            _ => None,
        }
    }

    pub fn default_mime(self) -> &'static str {
        match self {
            ArchiveType::Tar => "application/x-tar",
            ArchiveType::Zip => "application/zip",
        }
    }

    pub fn suffix(self) -> &'static str {
        match self {
            ArchiveType::Tar => "tar",
            ArchiveType::Zip => "zip",
        }
    }
}

impl str::FromStr for ArchiveType {
    type Err = ();

    fn from_str(s: &str) -> Result<ArchiveType, ()> {
        match s.to_ascii_lowercase().as_str() {
            "tar" => Ok(ArchiveType::Tar),
            "zip" => Ok(ArchiveType::Zip),
            _ => Err(()),
        }
    }
}

impl std::fmt::Display for ArchiveType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ArchiveType::Tar => f.write_str("tar"),
            ArchiveType::Zip => f.write_str("ZIP"),
        }
    }
}

/// Handle POST requests for archive downloads (form-based)
#[handler]
pub async fn handle_post_archive(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let config = depot.obtain::<Arc<AppConfig>>().unwrap().clone();

    if !config.archives {
        res.status_code(StatusCode::METHOD_NOT_ALLOWED);
        return;
    }

    if let Some(resp) = crate::hoops::auth::check_auth(req, &config) {
        *res = resp;
        return;
    }

    // Parse POST body for archive type
    let content_type = req
        .headers()
        .get(salvo::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if content_type != "text/plain" {
        res.status_code(StatusCode::BAD_REQUEST);
        return;
    }

    let body_bytes = req
        .payload()
        .await
        .ok()
        .map(|b| b.to_vec())
        .unwrap_or_default();
    let body_str = str::from_utf8(&body_bytes).unwrap_or("");

    let mut archive_type: Option<ArchiveType> = None;
    let mut vendor = false;
    let mut really = false;
    for l in body_str.lines() {
        if !vendor && l == "vendor=http" {
            vendor = true;
        } else if !really && l == "archive=yes-i-really-want-one" {
            really = true;
        } else if let Some(tp) = l.strip_prefix("type=") {
            archive_type = tp.parse().ok();
        }
    }

    if !(vendor && really) {
        res.status_code(StatusCode::BAD_REQUEST);
        return;
    }

    let at = match archive_type {
        Some(at) => at,
        None => {
            res.status_code(StatusCode::BAD_REQUEST);
            return;
        }
    };

    serve_archive(req, res, &config, at);
}

/// Handle GET requests with Accept header for archives
pub fn try_get_accept_archive(req: &Request) -> Option<ArchiveType> {
    req.headers()
        .get(salvo::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .and_then(|accept| {
            accept
                .split(',')
                .map(|s| s.trim().split(';').next().unwrap_or("").trim())
                .find_map(ArchiveType::from_mime)
        })
}

/// Serve an archive from a GET request with Accept header
pub fn serve_archive_from_get(
    req: &Request,
    res: &mut Response,
    config: &AppConfig,
    archive_type: ArchiveType,
) {
    serve_archive(req, res, config, archive_type);
}

fn serve_archive(req: &Request, res: &mut Response, config: &AppConfig, archive_type: ArchiveType) {
    let url_path_raw = req.uri().path().to_string();
    let segments: Vec<&str> = url_path_raw.split('/').filter(|s| !s.is_empty()).collect();
    let (req_p, symlink, url_err) = resolve_path(
        &config.hosted_directory.1,
        &segments,
        config.follow_symlinks,
    );

    if url_err {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Text::Html(error_html(
            "400 Bad Request",
            "Percent-encoding decoded to invalid UTF-8.",
            "",
        )));
        return;
    }

    if !req_p.exists() || config.is_symlink_denied(symlink, &req_p) {
        res.status_code(StatusCode::NOT_FOUND);
        res.render(Text::Html(error_html(
            "404 Not Found",
            "The requested entity doesn't exist.",
            "",
        )));
        return;
    }

    let remote = req.remote_addr().to_string();

    // Build attachment filename
    let mut attachment = url_path_raw.trim_matches('/').to_owned();
    if attachment.is_empty() {
        attachment = "all".to_string();
    }
    attachment.push('.');
    attachment.push_str(archive_type.suffix());

    log_msg(
        config.log,
        &format!(
            "{} is served {} archive for {}",
            remote,
            archive_type,
            req_p.display()
        ),
    );

    let allow_encoding = config.encoded_temp_dir.is_some();

    // Generate archive in memory
    let archive_data = match archive_type {
        ArchiveType::Tar => write_tar_body_to_vec(&req_p),
        ArchiveType::Zip => write_zip_body_to_vec(&req_p, allow_encoding),
    };

    match archive_data {
        Ok(data) => {
            log_msg(
                config.log,
                &format!(
                    "{} was served {} archive for {}",
                    remote,
                    archive_type,
                    req_p.display()
                ),
            );
            res.status_code(StatusCode::OK);
            res.headers_mut().insert(
                salvo::http::header::CONTENT_TYPE,
                archive_type.default_mime().parse().unwrap(),
            );
            res.headers_mut().insert(
                salvo::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", attachment)
                    .parse()
                    .unwrap(),
            );
            res.headers_mut()
                .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
            res.write_body(data).ok();
        }
        Err(e) => {
            log_msg(
                config.log,
                &format!("{} archive error for {}: {}", remote, req_p.display(), e),
            );
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
}

fn write_tar_body_to_vec(path: &Path) -> IoResult<Vec<u8>> {
    let buf = Vec::with_capacity(128 * 1024);
    let mut tar = TarBuilder::new(buf);
    tar.follow_symlinks(false);

    fn ignorable(err: io::Error) -> IoResult<()> {
        match err.kind() {
            IoErrorKind::ConnectionRefused
            | IoErrorKind::ConnectionReset
            | IoErrorKind::ConnectionAborted
            | IoErrorKind::BrokenPipe
            | IoErrorKind::WriteZero => Err(err),
            _ => Ok(()),
        }
    }

    #[cfg(unix)]
    let mut links = BTreeMap::<(u64, u64), PathBuf>::new();

    for entry in WalkDir::new(path)
        .follow_links(false)
        .follow_root_links(false)
        .into_iter()
        .flatten()
    {
        if entry.depth() == 0 && entry.file_type().is_dir() {
            continue;
        }

        let relative_path = if entry.depth() == 0 {
            entry
                .path()
                .file_name()
                .or_else(|| path.file_name())
                .map(Path::new)
                .unwrap_or(path)
        } else {
            entry
                .path()
                .strip_prefix(path)
                .expect("strip_prefix failed")
        };

        #[cfg(unix)]
        if !entry.file_type().is_dir()
            && let Ok(metadata) = entry.metadata()
            && metadata.nlink() > 1
        {
            match links.entry((metadata.dev(), metadata.ino())) {
                BTreeMapEntry::Occupied(previous) => {
                    let mut header = TarHeader::new_gnu();
                    header.set_metadata(&metadata);
                    header.set_size(0);
                    header.set_entry_type(TarEntryType::Link);
                    tar.append_link(&mut header, relative_path, previous.get())?;
                    continue;
                }
                BTreeMapEntry::Vacant(v) => {
                    v.insert(relative_path.to_path_buf());
                }
            }
        }

        tar.append_path_with_name(entry.path(), relative_path)
            .or_else(ignorable)?;
    }

    tar.into_inner()
}

fn write_zip_body_to_vec(path: &Path, allow_encoding: bool) -> IoResult<Vec<u8>> {
    let buf = Vec::with_capacity(128 * 1024);
    let mut zip = ZipWriter::new_stream(buf);

    for entry in WalkDir::new(path)
        .follow_links(false)
        .follow_root_links(false)
        .into_iter()
        .flatten()
    {
        if entry.depth() == 0 && entry.file_type().is_dir() {
            continue;
        }

        let relative_path = if entry.depth() == 0 {
            entry
                .path()
                .file_name()
                .or_else(|| path.file_name())
                .map(Path::new)
                .unwrap_or(path)
        } else {
            entry
                .path()
                .strip_prefix(path)
                .expect("strip_prefix failed")
        };

        let Ok(metadata) = entry.metadata() else {
            continue;
        };

        let mut options = ZipFileOptions::default()
            .compression_method(ZipCompressionMethod::Stored)
            .large_file(metadata.len() >= 2 * 1024 * 1024 * 1024);

        if let Some(zdt) = metadata
            .modified()
            .ok()
            .and_then(|mtime| mtime.duration_since(SystemTime::UNIX_EPOCH).ok())
            .and_then(|mdur| {
                let secs = mdur.as_secs();
                let tm = chrono::DateTime::<chrono::Utc>::from_timestamp(secs as i64, 0)?;
                let naive = tm.naive_utc();
                // Convert to zip DateTime manually via (date, time) u16 pair
                let year = naive.format("%Y").to_string().parse::<u16>().ok()?;
                let month = naive.format("%-m").to_string().parse::<u16>().ok()?;
                let day = naive.format("%-d").to_string().parse::<u16>().ok()?;
                let hour = naive.format("%-H").to_string().parse::<u16>().ok()?;
                let min = naive.format("%-M").to_string().parse::<u16>().ok()?;
                let sec = naive.format("%-S").to_string().parse::<u16>().ok()?;
                let date_part = ((year.saturating_sub(1980)) << 9) | (month << 5) | day;
                let time_part = (hour << 11) | (min << 5) | (sec / 2);
                zip::DateTime::try_from((date_part, time_part)).ok()
            })
        {
            options = options.last_modified_time(zdt);
        }

        #[cfg(unix)]
        {
            options = options.unix_permissions(metadata.mode());
        }
        #[cfg(not(unix))]
        {
            options = options.unix_permissions(
                (0o644 | (metadata.is_dir() as u32 * 0o111))
                    & !(metadata.permissions().readonly() as u32 * 0o444),
            );
        }

        match metadata.file_type() {
            e if e.is_symlink() => {
                if let Ok(target) = entry.path().read_link() {
                    zip.add_symlink_from_path(relative_path, target, options)?;
                }
            }
            e if e.is_dir() => {
                zip.add_directory_from_path(relative_path, options)?;
            }
            e if e.is_file() => {
                if let Ok(mut opened) = std::fs::File::open(entry.path()) {
                    let Ok(opened_metadata) = opened.metadata() else {
                        continue;
                    };

                    #[cfg(unix)]
                    if opened_metadata.dev() != metadata.dev()
                        || opened_metadata.ino() != metadata.ino()
                    {
                        continue;
                    }

                    if allow_encoding
                        && opened_metadata.len() > MIN_ENCODING_SIZE
                        && opened_metadata.len() < MAX_ENCODING_SIZE
                        && relative_path
                            .extension()
                            .map(|s| !extension_is_blacklisted(s))
                            .unwrap_or(true)
                    {
                        options = options.compression_method(ZipCompressionMethod::Deflated);
                    }

                    zip.start_file_from_path(relative_path, options)?;
                    io::copy(&mut opened, &mut zip)?;
                }
            }
            _ => (),
        }
    }

    let stream_writer = zip.finish()?;
    Ok(stream_writer.into_inner())
}
