#[cfg(unix)]
use std::collections::btree_map::{BTreeMap, Entry as BTreeMapEntry};
use std::io::{self, Result as IoResult, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::str;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use salvo::prelude::*;
use tar::{Builder as TarBuilder, EntryType as TarEntryType, Header as TarHeader};
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

const ARCHIVE_FORM_LIMIT: usize = 64 * 1024;

async fn archive_form_body(
    req: &mut Request,
    res: &mut Response,
    timeout: Duration,
) -> Option<Vec<u8>> {
    let status =
        match tokio::time::timeout(timeout, req.payload_with_max_size(ARCHIVE_FORM_LIMIT)).await {
            Ok(Ok(body)) if body.len() <= ARCHIVE_FORM_LIMIT => return Some(body.to_vec()),
            // Salvo can return a payload cached by middleware without applying the
            // new limit, so check its length even after a successful read.
            Ok(Ok(_)) | Ok(Err(salvo::http::ParseError::PayloadTooLarge)) => {
                StatusCode::PAYLOAD_TOO_LARGE
            }
            Ok(Err(_)) => StatusCode::BAD_REQUEST,
            Err(_) => StatusCode::REQUEST_TIMEOUT,
        };
    res.status_code(status);
    res.render(Text::Plain("Unable to read the archive request body.\n"));
    None
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

    let Some(body_bytes) = archive_form_body(req, res, Duration::from_secs(30)).await else {
        return;
    };
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
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll};

    use salvo::hyper::body::{Body, Bytes, Frame};
    use salvo::test::{ResponseExt, TestClient};
    use tokio::sync::oneshot;

    use super::*;
    use crate::test_support::{TestDir, config};

    struct FormBody {
        entered: Option<oneshot::Sender<()>>,
        dropped: Arc<AtomicBool>,
        fail: bool,
    }

    impl Body for FormBody {
        type Data = Bytes;
        type Error = salvo::BoxedError;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
            }
            if self.fail {
                Poll::Ready(Some(Err(
                    io::Error::other("fixture body read failure").into()
                )))
            } else {
                Poll::Pending
            }
        }
    }

    impl Drop for FormBody {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    fn form_request(fail: bool) -> (Request, oneshot::Receiver<()>, Arc<AtomicBool>) {
        let (entered, ready) = oneshot::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let mut request = TestClient::post("http://localhost/")
            .add_header("Content-Type", "text/plain", true)
            .build();
        *request.body_mut() = salvo::http::ReqBody::Boxed {
            inner: Box::pin(FormBody {
                entered: Some(entered),
                dropped: dropped.clone(),
                fail,
            }),
            fuse_config: None,
        };
        (request, ready, dropped)
    }

    #[tokio::test]
    async fn archive_form_failures_preserve_transport_and_size_statuses() {
        let directory = TestDir::new();
        let config = config(directory.path(), &["-A"]);
        let service = Service::new(crate::build_router(config.clone()));
        let response = TestClient::post("http://localhost/")
            .add_header("Content-Type", "text/plain", true)
            .body(vec![b'x'; ARCHIVE_FORM_LIMIT + 1])
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::PAYLOAD_TOO_LARGE));
        assert_eq!(config.archive_jobs.available_permits(), 4);
        let (request, _, dropped) = form_request(true);
        let response = service.handle(request).await;
        assert_eq!(response.status_code, Some(StatusCode::BAD_REQUEST));
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(config.archive_jobs.available_permits(), 4);

        // A caller may have cached a payload with a larger middleware limit.
        let mut cached = TestClient::post("http://localhost/")
            .body(vec![b'x'; ARCHIVE_FORM_LIMIT + 1])
            .build();
        cached
            .payload_with_max_size(ARCHIVE_FORM_LIMIT + 1)
            .await
            .unwrap();
        let mut response = Response::new();
        assert!(
            archive_form_body(&mut cached, &mut response, Duration::from_secs(1))
                .await
                .is_none()
        );
        assert_eq!(response.status_code, Some(StatusCode::PAYLOAD_TOO_LARGE));
    }

    #[tokio::test]
    async fn unfinished_archive_forms_time_out_and_release_their_body() {
        let (mut request, _, dropped) = form_request(false);
        let mut response = Response::new();
        assert!(
            archive_form_body(&mut request, &mut response, Duration::from_millis(10))
                .await
                .is_none()
        );
        assert_eq!(response.status_code, Some(StatusCode::REQUEST_TIMEOUT));
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancelling_an_archive_form_never_starts_a_producer() {
        let directory = TestDir::new();
        let config = config(directory.path(), &["-A"]);
        let service = Service::new(crate::build_router(config.clone()));
        let (request, ready, dropped) = form_request(false);
        let job = tokio::spawn(async move { service.handle(request).await });
        tokio::time::timeout(Duration::from_secs(2), ready)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(config.archive_jobs.available_permits(), 4);
        job.abort();
        assert!(job.await.unwrap_err().is_cancelled());
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(config.archive_jobs.available_permits(), 4);
    }

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

    let Some(permit) = config.reserve_archive_job() else {
        res.status_code(StatusCode::SERVICE_UNAVAILABLE);
        return;
    };

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

    if crate::http_cache::unversioned_preconditions(req, res) {
        return;
    }

    let sender = res.channel();
    let runtime = tokio::runtime::Handle::current();
    let mut snapshot = Request::new();
    *snapshot.uri_mut() = req.uri().clone();
    *snapshot.method_mut() = req.method().clone();
    *snapshot.headers_mut() = req.headers().clone();
    *snapshot.remote_addr_mut() = req.remote_addr().clone();
    let authority = ArchiveAuthority {
        request: snapshot,
        config: config.clone(),
        root: req_p.clone(),
        logical_root: decoded_url_path(req.uri().path()).unwrap(),
    };
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut writer = ArchiveWriter {
            sender,
            runtime,
            started: Instant::now(),
            stall_timeout: Duration::from_secs(30),
            total_timeout: Duration::from_secs(15 * 60),
            authority: SourceAuthority::Request(Box::new(authority)),
        };
        let result = match archive_type {
            ArchiveType::Tar => write_tar_body(&req_p, &mut writer),
            ArchiveType::Zip => write_zip_body(&req_p, allow_encoding, &mut writer),
        };
        if let Err(error) = result {
            writer.sender.send_error(error);
        }
    });
}

/// Bridge synchronous archive writers to a backpressured response channel.
/// A failed send stops traversal when the client disconnects.
struct ArchiveWriter {
    sender: salvo::http::body::BodySender,
    runtime: tokio::runtime::Handle,
    started: Instant,
    stall_timeout: Duration,
    total_timeout: Duration,
    authority: SourceAuthority,
}

enum SourceAuthority {
    Request(Box<ArchiveAuthority>),
    #[cfg(test)]
    Unchecked,
}

struct ArchiveAuthority {
    request: Request,
    config: Arc<AppConfig>,
    root: PathBuf,
    logical_root: String,
}

struct ArchiveMember {
    metadata: std::fs::Metadata,
    kind: MemberKind,
}

enum MemberKind {
    File(std::fs::File),
    Directory,
    Symlink(PathBuf),
    Special,
}

impl ArchiveAuthority {
    /// The caller holds MUTATION_LOCK only through authorization and opening.
    fn member(&self, path: &Path) -> IoResult<ArchiveMember> {
        let relative = path.strip_prefix(&self.root).map_err(io::Error::other)?;
        let mut logical = self.logical_root.clone();
        for component in relative.components() {
            let name = component.as_os_str().to_str().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "archive member has no exact UTF-8 authorization path",
                )
            })?;
            if name.contains('\\') {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "ambiguous archive member path",
                ));
            }
            if !logical.is_empty() {
                logical.push('/');
            }
            logical.push_str(name);
        }
        if crate::hoops::auth::check_decoded_auth(&self.request, &self.config, &logical).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "archive member authorization changed",
            ));
        }
        let root_segments: Vec<_> = self
            .request
            .uri()
            .path()
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        let (root, root_symlink, invalid) = resolve_path(
            &self.config.hosted_directory.1,
            &root_segments,
            self.config.follow_symlinks,
        );
        if invalid
            || root.canonicalize()? != self.root.canonicalize()?
            || self.config.is_symlink_denied(root_symlink, &root)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "archive root changed",
            ));
        }
        // Resolve parent aliases while preserving a member's final symlink as a
        // link entry. No-follow archives must not start traversing a new alias.
        if !relative.as_os_str().is_empty() {
            let parent = Path::new(&logical).parent().unwrap_or(Path::new(""));
            let encoded: Vec<_> = parent
                .components()
                .map(|part| {
                    percent_encoding::utf8_percent_encode(
                        part.as_os_str().to_str().unwrap(),
                        percent_encoding::NON_ALPHANUMERIC,
                    )
                    .to_string()
                })
                .collect();
            let segments: Vec<_> = encoded.iter().map(String::as_str).collect();
            let (parent, symlink, invalid) = resolve_path(
                &self.config.hosted_directory.1,
                &segments,
                self.config.follow_symlinks,
            );
            if invalid || self.config.is_symlink_denied(symlink, &parent) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "archive parent policy changed",
                ));
            }
            let actual = path.parent().unwrap().canonicalize()?;
            if actual != parent.canonicalize()? {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "archive member path changed",
                ));
            }
        }
        open_archive_member(path)
    }
}

fn open_archive_member(path: &Path) -> IoResult<ArchiveMember> {
    let metadata = std::fs::symlink_metadata(path)?;
    let kind = if metadata.is_file() {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = options.open(path)?;
        let opened = file.metadata()?;
        #[cfg(unix)]
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            return Err(io::Error::other("archive member changed while opening"));
        }
        if !opened.is_file() {
            return Err(io::Error::other(
                "archive member type changed while opening",
            ));
        }
        return Ok(ArchiveMember {
            metadata: opened,
            kind: MemberKind::File(file),
        });
    } else if metadata.is_dir() {
        MemberKind::Directory
    } else if metadata.file_type().is_symlink() {
        MemberKind::Symlink(path.read_link()?)
    } else {
        MemberKind::Special
    };
    Ok(ArchiveMember { metadata, kind })
}

trait ArchiveOutput: Write {
    fn member(&self, path: &Path) -> IoResult<ArchiveMember>;
}

impl<T: ArchiveOutput + ?Sized> ArchiveOutput for &mut T {
    fn member(&self, path: &Path) -> IoResult<ArchiveMember> {
        (**self).member(path)
    }
}

#[cfg(test)]
impl ArchiveOutput for Vec<u8> {
    fn member(&self, path: &Path) -> IoResult<ArchiveMember> {
        open_archive_member(path)
    }
}

impl ArchiveOutput for ArchiveWriter {
    fn member(&self, path: &Path) -> IoResult<ArchiveMember> {
        let remaining = self.total_timeout.saturating_sub(self.started.elapsed());
        let deadline = tokio::time::Instant::now() + self.stall_timeout.min(remaining);
        self.member_until(path, deadline)
    }
}

impl ArchiveWriter {
    fn member_until(&self, path: &Path, deadline: tokio::time::Instant) -> IoResult<ArchiveMember> {
        self.runtime.block_on(async {
            let wait = crate::util::write_conditions::MUTATION_LOCK.lock();
            tokio::pin!(wait);
            let guard = loop {
                if self.sender.is_closed() {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "archive receiver closed",
                    ));
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "archive source lock deadline exceeded",
                    ));
                }
                tokio::select! {
                    guard = &mut wait => break guard,
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
            };
            // Granting the lock may wake this worker before the periodic poll
            // runs again. Do not start synchronous source work after its waiter
            // was cancelled or expired while the lock was unavailable.
            if self.sender.is_closed() {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "archive receiver closed",
                ));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "archive source lock deadline exceeded",
                ));
            }
            let member = match &self.authority {
                SourceAuthority::Request(authority) => authority.member(path),
                #[cfg(test)]
                SourceAuthority::Unchecked => open_archive_member(path),
            };
            drop(guard);
            member
        })
    }
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

fn write_tar_body<W: ArchiveOutput>(path: &Path, output: W) -> IoResult<W> {
    output.member(path)?;
    let mut tar = TarBuilder::new(output);
    tar.follow_symlinks(false);

    #[cfg(unix)]
    let mut links = BTreeMap::<(u64, u64), PathBuf>::new();

    for entry in WalkDir::new(path)
        .follow_links(false)
        .follow_root_links(false)
        .into_iter()
        .filter_entry(|entry| entry.depth() == 0 || !is_internal_name(entry.file_name()))
    {
        let entry = entry.map_err(io::Error::other)?;
        let member = tar.get_mut().member(entry.path())?;
        if entry.depth() == 0 && member.metadata.is_dir() {
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
        {
            let metadata = &member.metadata;
            if !metadata.is_dir() && metadata.nlink() > 1 {
                match links.entry((metadata.dev(), metadata.ino())) {
                    BTreeMapEntry::Occupied(previous) => {
                        let mut header = TarHeader::new_gnu();
                        header.set_metadata(metadata);
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
        }

        append_tar_member(&mut tar, relative_path, member)?;
    }

    tar.into_inner()
}

fn append_tar_member<W: Write>(
    tar: &mut TarBuilder<W>,
    relative: &Path,
    member: ArchiveMember,
) -> IoResult<()> {
    if let MemberKind::File(mut file) = member.kind {
        return tar.append_file(relative, &mut file);
    }
    let mut header = TarHeader::new_gnu();
    header.set_metadata(&member.metadata);
    header.set_size(0);
    match member.kind {
        MemberKind::Symlink(target) => {
            header.set_entry_type(TarEntryType::Symlink);
            return tar.append_link(&mut header, relative, target);
        }
        MemberKind::Directory => header.set_entry_type(TarEntryType::Directory),
        MemberKind::Special => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileTypeExt;
                let kind = member.metadata.file_type();
                header.set_entry_type(if kind.is_fifo() {
                    TarEntryType::Fifo
                } else if kind.is_char_device() {
                    TarEntryType::Char
                } else if kind.is_block_device() {
                    TarEntryType::Block
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "unsupported TAR entry type",
                    ));
                });
                if kind.is_char_device() || kind.is_block_device() {
                    set_tar_device_numbers(&mut header, member.metadata.rdev())?;
                }
            }
            #[cfg(not(unix))]
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported TAR entry type",
            ));
        }
        MemberKind::File(_) => unreachable!(),
    }
    tar.append_data(&mut header, relative, io::empty())
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // dev_t and major/minor return types differ on Linux and macOS.
fn set_tar_device_numbers(header: &mut TarHeader, device: u64) -> IoResult<()> {
    let device = device as libc::dev_t;
    header.set_device_major(libc::major(device) as u32)?;
    header.set_device_minor(libc::minor(device) as u32)
}

fn write_zip_body<W: ArchiveOutput>(path: &Path, allow_encoding: bool, output: W) -> IoResult<W> {
    output.member(path)?;
    let mut zip = ZipWriter::new_stream(output);

    for entry in WalkDir::new(path)
        .follow_links(false)
        .follow_root_links(false)
        .into_iter()
        .filter_entry(|entry| entry.depth() == 0 || !is_internal_name(entry.file_name()))
    {
        let entry = entry.map_err(io::Error::other)?;
        let member = zip
            .get_ref()
            .ok_or_else(|| io::Error::other("archive writer closed"))?
            .get_ref()
            .member(entry.path())?;
        if entry.depth() == 0 && member.metadata.is_dir() {
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

        let metadata = &member.metadata;

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

        match member.kind {
            MemberKind::Symlink(target) => {
                zip.add_symlink_from_path(relative_path, target, options)?;
            }
            MemberKind::Directory => {
                zip.add_directory_from_path(relative_path, options)?;
            }
            MemberKind::File(mut opened) => {
                if allow_encoding
                    && metadata.len() > MIN_ENCODING_SIZE
                    && metadata.len() < MAX_ENCODING_SIZE
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
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "unsupported ZIP entry type",
                ));
            }
        }
    }

    let stream_writer = zip.finish()?;
    Ok(stream_writer.into_inner())
}

#[cfg(test)]
mod streaming_tests {
    use salvo::test::{ResponseExt, TestClient};

    use super::*;

    async fn late_namespace_grant(disconnect: bool) -> Option<io::ErrorKind> {
        let fixture = crate::test_support::TestDir::new();
        let path = fixture.path().join("file");
        std::fs::write(&path, b"fixture").unwrap();
        let guard = crate::util::write_conditions::MUTATION_LOCK.lock().await;
        let mut response = Response::new();
        let writer = ArchiveWriter {
            sender: response.channel(),
            runtime: tokio::runtime::Handle::current(),
            started: Instant::now(),
            stall_timeout: if disconnect {
                Duration::from_secs(2)
            } else {
                Duration::from_millis(10)
            },
            total_timeout: Duration::from_secs(10),
            authority: SourceAuthority::Unchecked,
        };
        let (entered, ready) = std::sync::mpsc::channel();
        // Anchor expiry in the caller so delayed worker scheduling cannot give
        // this test a fresh stall interval after the namespace was released.
        let deadline = tokio::time::Instant::now() + writer.stall_timeout;
        let job = tokio::task::spawn_blocking(move || {
            entered.send(()).unwrap();
            writer
                .member_until(&path, deadline)
                .err()
                .map(|error| error.kind())
        });
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        // Keep this current-thread timer driver paused while the blocking
        // producer parks on the namespace, then while the lock is granted.
        // This makes the lock wake win over the periodic cancellation poll.
        std::thread::sleep(Duration::from_millis(20));
        if disconnect {
            drop(response);
        }
        drop(guard);
        std::thread::sleep(Duration::from_millis(20));
        job.await.unwrap()
    }

    #[tokio::test]
    async fn a_closed_receiver_does_not_open_a_source_when_the_namespace_is_granted() {
        assert_eq!(
            late_namespace_grant(true).await,
            Some(io::ErrorKind::BrokenPipe)
        );
    }

    #[tokio::test]
    async fn an_expired_wait_does_not_open_a_source_when_the_namespace_is_granted() {
        assert_eq!(
            late_namespace_grant(false).await,
            Some(io::ErrorKind::TimedOut)
        );
    }

    fn authority(root: &Path, url: &str, config: Arc<AppConfig>) -> ArchiveAuthority {
        ArchiveAuthority {
            request: TestClient::get(format!("http://localhost{url}")).build(),
            config,
            root: root.to_path_buf(),
            logical_root: decoded_url_path(url).unwrap(),
        }
    }

    #[test]
    fn opened_tar_member_keeps_its_version_after_namespace_replacement() {
        let fixture = crate::test_support::TestDir::new();
        let path = fixture.path().join("file");
        let replacement = fixture.path().join("replacement");
        std::fs::write(&path, b"opened public version").unwrap();
        std::fs::write(&replacement, b"later version").unwrap();
        let authority = authority(
            &path,
            "/file",
            crate::test_support::config(fixture.path(), &[]),
        );
        let member = {
            let _guard = crate::util::write_conditions::MUTATION_LOCK.blocking_lock();
            let member = authority.member(&path).unwrap();
            crate::util::namespace::rename_no_replace(&path, &fixture.path().join("retired"))
                .unwrap();
            crate::util::namespace::rename_no_replace(&replacement, &path).unwrap();
            member
        };
        let mut tar = TarBuilder::new(Vec::new());
        append_tar_member(&mut tar, Path::new("file"), member).unwrap();
        let bytes = tar.into_inner().unwrap();
        let mut archive = tar::Archive::new(bytes.as_slice());
        let mut entries = archive.entries().unwrap();
        let mut entry = entries.next().unwrap().unwrap();
        let mut contents = String::new();
        std::io::Read::read_to_string(&mut entry, &mut contents).unwrap();
        assert_eq!(contents, "opened public version");
        assert_eq!(std::fs::read(&path).unwrap(), b"later version");
    }

    #[cfg(unix)]
    #[test]
    #[allow(clippy::unnecessary_cast)] // Match each platform's libc dev_t/major/minor ABI.
    fn owned_tar_metadata_preserves_links_and_platform_device_numbers() {
        use std::os::unix::fs::symlink;

        let fixture = crate::test_support::TestDir::new();
        std::fs::create_dir(fixture.path().join("directory")).unwrap();
        std::fs::write(fixture.path().join("first"), b"shared content").unwrap();
        std::fs::hard_link(fixture.path().join("first"), fixture.path().join("second")).unwrap();
        symlink("unresolved/target", fixture.path().join("symbolic")).unwrap();
        let bytes = write_tar_body(fixture.path(), Vec::new()).unwrap();
        let mut archive = tar::Archive::new(bytes.as_slice());
        let mut counts = [0; 4];
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.header().entry_type().is_file() {
                let mut contents = String::new();
                std::io::Read::read_to_string(&mut entry, &mut contents).unwrap();
                assert_eq!(contents, "shared content");
                counts[0] += 1;
            } else if entry.header().entry_type().is_hard_link() {
                counts[1] += 1;
            } else if entry.header().entry_type().is_dir() {
                counts[2] += 1;
            } else if entry.header().entry_type().is_symlink() {
                assert_eq!(
                    entry.link_name().unwrap().unwrap(),
                    Path::new("unresolved/target")
                );
                counts[3] += 1;
            }
        }
        assert_eq!(counts, [1, 1, 1, 1]);
        let bytes = write_tar_body(Path::new("/dev/null"), Vec::new()).unwrap();
        let mut archive = tar::Archive::new(bytes.as_slice());
        let entry = archive.entries().unwrap().next().unwrap().unwrap();
        let device = std::fs::symlink_metadata("/dev/null").unwrap().rdev() as libc::dev_t;
        assert_eq!(entry.header().entry_type(), TarEntryType::Char);
        assert_eq!(
            entry.header().device_major().unwrap(),
            Some(libc::major(device) as u32)
        );
        assert_eq!(
            entry.header().device_minor().unwrap(),
            Some(libc::minor(device) as u32)
        );
    }

    #[cfg(unix)]
    #[test]
    fn member_authority_is_rechecked_after_a_server_namespace_publication() {
        use std::cell::Cell;
        use std::os::unix::fs::symlink;

        struct ReboundOutput {
            bytes: Vec<u8>,
            authority: ArchiveAuthority,
            calls: Cell<usize>,
            active: PathBuf,
            prepared: PathBuf,
            retired: PathBuf,
        }
        impl Write for ReboundOutput {
            fn write(&mut self, bytes: &[u8]) -> IoResult<usize> {
                self.bytes.write(bytes)
            }
            fn flush(&mut self) -> IoResult<()> {
                Ok(())
            }
        }
        impl ArchiveOutput for ReboundOutput {
            fn member(&self, path: &Path) -> IoResult<ArchiveMember> {
                let _guard = crate::util::write_conditions::MUTATION_LOCK.blocking_lock();
                let calls = self.calls.get();
                self.calls.set(calls + 1);
                // The initial root inspection has passed. Publish using the same
                // namespace primitives and lock as a separately authorized writer.
                if calls == 1 {
                    crate::util::namespace::rename_no_replace(&self.active, &self.retired)?;
                    crate::util::namespace::copy_no_replace(&self.prepared, &self.active, true)?;
                }
                self.authority.member(path)
            }
        }
        for zip in [false, true] {
            let fixture = crate::test_support::TestDir::new();
            std::fs::create_dir_all(fixture.path().join("active/alias")).unwrap();
            std::fs::create_dir(fixture.path().join("private")).unwrap();
            std::fs::create_dir(fixture.path().join("prepared")).unwrap();
            std::fs::write(fixture.path().join("active/alias/file"), b"public version").unwrap();
            std::fs::write(fixture.path().join("private/file"), b"protected version").unwrap();
            symlink(
                fixture.path().join("private"),
                fixture.path().join("prepared/alias"),
            )
            .unwrap();
            let config =
                crate::test_support::config(fixture.path(), &["--path-auth", "private=user:pass"]);
            let path = fixture.path().join("active/alias/file");
            let authority = authority(&path, "/active/alias/file", config);
            assert!(
                crate::hoops::auth::check_tree_auth(
                    &authority.request,
                    &authority.config,
                    "/active/alias/file"
                )
                .is_none()
            );
            let output = ReboundOutput {
                bytes: Vec::new(),
                authority,
                calls: Cell::new(0),
                active: fixture.path().join("active"),
                prepared: fixture.path().join("prepared"),
                retired: fixture.path().join("retired"),
            };
            let result = if zip {
                write_zip_body(&path, false, output)
            } else {
                write_tar_body(&path, output)
            };
            assert_eq!(
                result.err().unwrap().kind(),
                io::ErrorKind::PermissionDenied
            );
            assert_eq!(
                std::fs::read(fixture.path().join("private/file")).unwrap(),
                b"protected version"
            );
        }
    }

    #[tokio::test]
    async fn member_lock_wait_stops_on_disconnect_or_deadline_and_releases_capacity() {
        let fixture = crate::test_support::TestDir::new();
        let path = fixture.path().join("file");
        std::fs::write(&path, b"fixture").unwrap();
        for disconnect in [false, true] {
            let config = crate::test_support::config(fixture.path(), &[]);
            let permit = config.reserve_archive_job().unwrap();
            let guard = crate::util::write_conditions::MUTATION_LOCK.lock().await;
            let mut response = Response::new();
            let writer = ArchiveWriter {
                sender: response.channel(),
                runtime: tokio::runtime::Handle::current(),
                started: Instant::now(),
                stall_timeout: Duration::from_millis(100),
                total_timeout: Duration::from_secs(2),
                authority: SourceAuthority::Unchecked,
            };
            let path = path.clone();
            let job = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                writer.member(&path).err().unwrap().kind()
            });
            if disconnect {
                drop(response);
            }
            let result = tokio::time::timeout(Duration::from_secs(2), job)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                result,
                if disconnect {
                    io::ErrorKind::BrokenPipe
                } else {
                    io::ErrorKind::TimedOut
                }
            );
            assert_eq!(config.archive_jobs.available_permits(), 4);
            drop(guard);
        }
    }

    #[tokio::test]
    async fn tar_and_zip_output_never_hold_the_namespace_lock() {
        struct CheckedOutput(ArchiveWriter);
        impl ArchiveOutput for CheckedOutput {
            fn member(&self, path: &Path) -> IoResult<ArchiveMember> {
                self.0.member(path)
            }
        }
        impl Write for CheckedOutput {
            fn write(&mut self, bytes: &[u8]) -> IoResult<usize> {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if let Ok(guard) = crate::util::write_conditions::MUTATION_LOCK.try_lock() {
                        drop(guard);
                        break;
                    }
                    if Instant::now() >= deadline {
                        return Err(io::Error::other("output held the namespace lock"));
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                self.0.write(bytes)
            }
            fn flush(&mut self) -> IoResult<()> {
                self.0.flush()
            }
        }
        let fixture = crate::test_support::TestDir::new();
        std::fs::create_dir(fixture.path().join("child")).unwrap();
        std::fs::write(fixture.path().join("child/file"), b"streamed member").unwrap();
        std::fs::write(
            fixture.path().join("child/中文%20.txt"),
            b"exact logical name",
        )
        .unwrap();
        for zip in [false, true] {
            let mut response = Response::new();
            let writer = ArchiveWriter {
                sender: response.channel(),
                runtime: tokio::runtime::Handle::current(),
                started: Instant::now(),
                stall_timeout: Duration::from_secs(5),
                total_timeout: Duration::from_secs(10),
                authority: SourceAuthority::Request(Box::new(authority(
                    fixture.path(),
                    "/",
                    crate::test_support::config(fixture.path(), &[]),
                ))),
            };
            let path = fixture.path().to_path_buf();
            let job = tokio::task::spawn_blocking(move || {
                if zip {
                    write_zip_body(&path, false, CheckedOutput(writer)).map(|_| ())
                } else {
                    write_tar_body(&path, CheckedOutput(writer)).map(|_| ())
                }
            });
            let bytes = tokio::time::timeout(Duration::from_secs(10), response.take_bytes(None))
                .await
                .unwrap()
                .unwrap();
            assert!(!bytes.is_empty());
            job.await.unwrap().unwrap();
            let mut contents = String::new();
            if zip {
                let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
                std::io::Read::read_to_string(
                    &mut archive.by_name("child/中文%20.txt").unwrap(),
                    &mut contents,
                )
                .unwrap();
            } else {
                let mut archive = tar::Archive::new(bytes.as_ref());
                for entry in archive.entries().unwrap() {
                    let mut entry = entry.unwrap();
                    if entry.path().unwrap() == Path::new("child/中文%20.txt") {
                        std::io::Read::read_to_string(&mut entry, &mut contents).unwrap();
                    }
                }
            }
            assert_eq!(contents, "exact logical name");
        }
    }

    #[test]
    fn traversal_errors_do_not_finish_valid_empty_archives() {
        let directory = crate::test_support::TestDir::new();
        let missing = directory.path().join("removed-before-generation");
        assert!(write_tar_body(&missing, Vec::new()).is_err());
        assert!(write_zip_body(&missing, true, Vec::new()).is_err());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn unreadable_zip_members_abort_the_download_instead_of_disappearing() {
        use std::os::windows::fs::OpenOptionsExt;

        let directory = crate::test_support::TestDir::new();
        std::fs::write(directory.path().join("visible.txt"), b"visible").unwrap();
        let blocked = directory.path().join("blocked.txt");
        std::fs::write(&blocked, b"important content").unwrap();
        let _lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&blocked)
            .unwrap();
        assert!(std::fs::File::open(&blocked).is_err());
        assert!(write_zip_body(directory.path(), true, Vec::new()).is_err());
        let service = Service::new(crate::build_router(crate::test_support::config(
            directory.path(),
            &["-A"],
        )));
        let mut response = TestClient::get("http://localhost/")
            .add_header("Accept", "application/zip", true)
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::OK));
        // Headers may already be sent, but the transfer must not finish as a valid ZIP.
        assert!(response.take_bytes(None).await.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_zip_special_files_are_errors_not_omissions() {
        assert_eq!(
            write_zip_body(Path::new("/dev/null"), false, Vec::new())
                .unwrap_err()
                .kind(),
            io::ErrorKind::Unsupported
        );
    }

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
                authority: SourceAuthority::Unchecked,
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
            authority: SourceAuthority::Unchecked,
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
