#[cfg(unix)]
use std::collections::btree_map::{BTreeMap, Entry as BTreeMapEntry};
use std::io::{self, Result as IoResult, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::str;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

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
    let config = depot.get_typed::<Arc<AppConfig>>().unwrap().clone();

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
        .and_then(|value| value.parse::<mime::Mime>().ok());

    if !content_type
        .is_some_and(|value| value.type_() == mime::TEXT && value.subtype() == mime::PLAIN)
    {
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

    serve_archive(req, res, &config, at).await;
}

#[cfg(test)]
mod post_content_type_tests {
    use std::io::Read;

    use salvo::test::{ResponseExt, TestClient};

    use super::*;
    use crate::test_support::{TestDir, config};

    #[tokio::test]
    async fn archive_forms_accept_equivalent_plain_text_media_types() {
        let directory = TestDir::new();
        std::fs::write(directory.path().join("file.txt"), "archive fixture").unwrap();
        let service = Service::new(crate::build_router(config(directory.path(), &["-A"])));
        let form = "vendor=http\narchive=yes-i-really-want-one\ntype=tar\n";
        for content_type in [
            "text/plain",
            "Text/Plain",
            "text/plain; charset=utf-8",
            "Text/Plain; Charset=\"UTF-8\"",
        ] {
            let mut response = TestClient::post("http://localhost/")
                .add_header("Content-Type", content_type, true)
                .body(form)
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::OK), "{content_type}");
            assert_eq!(response.headers()["content-type"], "application/x-tar");
            let bytes = response.take_bytes(None).await.unwrap();
            let mut archive = tar::Archive::new(bytes.as_ref());
            let mut entries = archive.entries().unwrap();
            let mut entry = entries.next().unwrap().unwrap();
            assert_eq!(entry.path().unwrap().to_str().unwrap(), "file.txt");
            let mut contents = String::new();
            entry.read_to_string(&mut contents).unwrap();
            assert_eq!(contents, "archive fixture");
            assert!(entries.next().is_none());
        }
        for content_type in [
            "application/json",
            "text/plain; charset",
            "not a media type",
        ] {
            let response = TestClient::post("http://localhost/")
                .add_header("Content-Type", content_type, true)
                .body(form)
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::BAD_REQUEST));
        }
    }
}

/// Serve an archive from a GET request with Accept header
pub async fn serve_archive_from_get(
    req: &Request,
    res: &mut Response,
    config: &Arc<AppConfig>,
    archive_type: ArchiveType,
) {
    serve_archive(req, res, config, archive_type).await;
}

async fn serve_archive(
    req: &Request,
    res: &mut Response,
    config: &Arc<AppConfig>,
    archive_type: ArchiveType,
) {
    if let Some(response) =
        crate::hoops::auth::check_tree_auth_async(req, config, req.uri().path()).await
    {
        *res = response;
        return;
    }
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

    let sender = res.channel();
    let runtime = tokio::runtime::Handle::current();
    if config
        .spawn_archive(move || {
            let mut writer = ArchiveWriter {
                sender,
                runtime,
                started: Instant::now(),
                stall_timeout: Duration::from_secs(30),
                total_timeout: Duration::from_secs(15 * 60),
            };
            let result = match archive_type {
                ArchiveType::Tar => write_tar_body(&req_p, &mut writer),
                ArchiveType::Zip => write_zip_body(&req_p, allow_encoding, &mut writer),
            };
            if let Err(error) = result {
                writer.sender.send_error(error);
            }
        })
        .is_none()
    {
        res.body(salvo::http::ResBody::None);
        res.status_code(StatusCode::SERVICE_UNAVAILABLE);
    }
}

/// Bridge synchronous archive writers to a backpressured response channel.
/// A failed send stops traversal when the client disconnects.
struct ArchiveWriter {
    sender: salvo::http::body::BodySender,
    runtime: tokio::runtime::Handle,
    started: Instant,
    stall_timeout: Duration,
    total_timeout: Duration,
}

impl Write for ArchiveWriter {
    fn write(&mut self, bytes: &[u8]) -> IoResult<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let count = bytes.len().min(64 * 1024);
        let remaining = self.total_timeout.saturating_sub(self.started.elapsed());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "archive output deadline exceeded",
            ));
        }
        let timeout = self.stall_timeout.min(remaining);
        self.runtime.block_on(async {
            tokio::time::timeout(timeout, self.sender.send_data(bytes[..count].to_vec()))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "archive output stalled"))?
        })?;
        Ok(count)
    }

    fn flush(&mut self) -> IoResult<()> {
        Ok(())
    }
}

fn write_tar_body<W: Write>(path: &Path, output: W) -> IoResult<W> {
    let mut tar = TarBuilder::new(output);
    tar.follow_symlinks(false);

    #[cfg(unix)]
    let mut links = BTreeMap::<(u64, u64), PathBuf>::new();

    for entry in WalkDir::new(path)
        .follow_links(false)
        .follow_root_links(false)
        .into_iter()
        .filter_entry(|entry| entry.depth() == 0 || !is_internal_name(entry.file_name()))
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

        tar.append_path_with_name(entry.path(), relative_path)?;
    }

    tar.into_inner()
}

fn write_zip_body<W: Write>(path: &Path, allow_encoding: bool, output: W) -> IoResult<W> {
    let mut zip = ZipWriter::new_stream(output);

    for entry in WalkDir::new(path)
        .follow_links(false)
        .follow_root_links(false)
        .into_iter()
        .filter_entry(|entry| entry.depth() == 0 || !is_internal_name(entry.file_name()))
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

#[cfg(test)]
mod streaming_tests {
    use salvo::test::{ResponseExt, TestClient};

    use super::*;

    #[tokio::test]
    async fn slow_archives_do_not_starve_short_metadata_work_and_disconnect_releases_slots() {
        let directory = crate::test_support::TestDir::new();
        std::fs::write(
            directory.path().join("large.bin"),
            vec![42; 4 * 1024 * 1024],
        )
        .unwrap();
        let config = crate::test_support::config(directory.path(), &["-A"]);
        let service = Service::new(crate::build_router(config.clone()));
        let mut responses = Vec::new();
        for _ in 0..4 {
            let response = TestClient::get("http://localhost/")
                .add_header("Accept", "application/x-tar", true)
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::OK));
            responses.push(response);
        }
        assert_eq!(config.archive_jobs.available_permits(), 0);
        let directory_response = TestClient::get("http://localhost/").send(&service).await;
        assert_eq!(directory_response.status_code, Some(StatusCode::OK));
        let rejected = TestClient::get("http://localhost/")
            .add_header("Accept", "application/x-tar", true)
            .send(&service)
            .await;
        assert_eq!(rejected.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
        drop(responses);
        tokio::time::timeout(Duration::from_secs(2), async {
            while config.archive_jobs.available_permits() != 4 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn stalled_archive_output_times_out_and_expired_deadlines_prevent_sending() {
        let mut response = Response::new();
        let sender = response.channel();
        let runtime = tokio::runtime::Handle::current();
        let job = tokio::task::spawn_blocking(move || {
            let mut writer = ArchiveWriter {
                sender,
                runtime,
                started: Instant::now(),
                stall_timeout: Duration::from_millis(20),
                total_timeout: Duration::from_secs(1),
            };
            writer.write_all(&vec![17; 1024 * 1024]).unwrap_err().kind()
        });
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), job)
                .await
                .unwrap()
                .unwrap(),
            io::ErrorKind::TimedOut
        );
        drop(response);

        let mut response = Response::new();
        let mut writer = ArchiveWriter {
            sender: response.channel(),
            runtime: tokio::runtime::Handle::current(),
            started: Instant::now() - Duration::from_secs(2),
            stall_timeout: Duration::from_secs(1),
            total_timeout: Duration::from_secs(1),
        };
        assert_eq!(
            writer.write(b"expired").unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[tokio::test]
    async fn archives_stream_valid_tar_and_zip_contents() {
        let dir = crate::test_support::TestDir::new();
        let data = vec![17; 256 * 1024 + 3];
        std::fs::write(dir.path().join("data.bin"), &data).unwrap();
        std::fs::write(dir.path().join(".HPIP-UPLOAD-secret"), b"private").unwrap();
        std::fs::create_dir(dir.path().join(".hpip-internal-secret")).unwrap();
        std::fs::write(dir.path().join(".hpip-internal-secret/key"), b"private").unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &["-A"],
        )));
        for mime in ["application/x-tar", "application/zip"] {
            let mut response = TestClient::get("http://localhost/")
                .add_header("Accept", mime, true)
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::OK));
            assert!(response.body.is_channel());
            let bytes = response.take_bytes(None).await.unwrap();
            let mut contents = Vec::new();
            if mime.ends_with("zip") {
                let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
                assert_eq!(archive.len(), 1);
                let mut file = archive.by_name("data.bin").unwrap();
                std::io::Read::read_to_end(&mut file, &mut contents).unwrap();
            } else {
                let mut archive = tar::Archive::new(bytes.as_ref());
                for file in archive.entries().unwrap() {
                    let mut file = file.unwrap();
                    assert!(!file.path().unwrap().to_string_lossy().contains("secret"));
                    if file.path().unwrap() == Path::new("data.bin") {
                        std::io::Read::read_to_end(&mut file, &mut contents).unwrap();
                    }
                }
            }
            assert_eq!(contents, data);
        }
    }
}
