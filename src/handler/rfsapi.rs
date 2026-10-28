use std::path::Path;
use std::sync::Arc;

use salvo::prelude::*;
use serde::Serialize;

use crate::config::{AppConfig, log_msg};
use crate::util::*;

/// Raw filesystem metadata for a single file/directory
#[derive(Debug, Clone, Serialize)]
pub struct RawFileData {
    pub mime_type: String,
    pub name: String,
    pub last_modified: String,
    pub size: u64,
    pub is_file: bool,
}

/// A set of raw filesystem metadata entries
#[derive(Debug, Clone, Serialize)]
pub struct FilesetData {
    pub writes_supported: bool,
    pub is_root: bool,
    pub is_file: bool,
    pub files: Vec<RawFileData>,
}

fn get_raw_fs_metadata(path: &std::path::Path) -> RawFileData {
    let mime_type = guess_mime_type(path, &std::collections::BTreeMap::new());
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let modified = file_time_modified_p(path);
    let metadata = path.metadata().ok();
    let size = metadata
        .as_ref()
        .map(|m| file_length(m, &path))
        .unwrap_or(0);

    RawFileData {
        mime_type,
        name,
        last_modified: modified.to_rfc3339(),
        size,
        is_file: true,
    }
}

/// Handle a GET request with Accept: application/json for raw filesystem metadata
#[handler]
pub async fn handle_rfsapi(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let config = depot.get_typed::<Arc<AppConfig>>().unwrap().clone();

    if let Some(resp) = crate::hoops::auth::check_auth(req, &config) {
        *res = resp;
        return;
    }

    handle_rfsapi_inner(req, res, &config).await;
}

/// Inner implementation that can be called directly with a config reference.
pub async fn handle_rfsapi_inner(req: &mut Request, res: &mut Response, config: &Arc<AppConfig>) {
    let uri = req.uri().clone();
    let remote = req.remote_addr().to_string();
    let work_config = config.clone();
    let Some(job) = config.spawn_blocking(move || {
        let mut request = Request::new();
        *request.uri_mut() = uri;
        let mut response = Response::new();
        handle_rfsapi_snapshot(&mut request, &mut response, &work_config, &remote);
        response
    }) else {
        res.status_code(StatusCode::SERVICE_UNAVAILABLE);
        return;
    };
    match job.await {
        Ok(response) => *res = response,
        Err(_) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
}

fn handle_rfsapi_snapshot(req: &mut Request, res: &mut Response, config: &AppConfig, remote: &str) {
    let url_path_raw = req.uri().path().to_string();
    let segments: Vec<&str> = url_path_raw.split('/').filter(|s| !s.is_empty()).collect();
    let (req_p, symlink, url_err) = resolve_path(
        &config.hosted_directory.1,
        &segments,
        config.follow_symlinks,
    );

    if url_err {
        res.status_code(StatusCode::BAD_REQUEST);
        res.render(Text::Json(
            serde_json::json!({"error": "Percent-encoding decoded to invalid UTF-8."}).to_string(),
        ));
        return;
    }

    if !req_p.exists() || config.is_symlink_denied(symlink, &req_p) {
        res.status_code(StatusCode::NOT_FOUND);
        res.render(Text::Json(
            serde_json::json!({"error": "Not found"}).to_string(),
        ));
        return;
    }

    let metadata = match req_p.metadata() {
        Ok(m) => m,
        Err(_) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            return;
        }
    };

    let is_file = is_actually_file(&metadata.file_type(), &req_p);

    if is_file {
        handle_get_raw_fs_file(req, res, config, &req_p, remote);
    } else {
        handle_get_raw_fs_dir(req, res, config, &req_p, remote, &url_path_raw);
    }
}

fn handle_get_raw_fs_file(
    _req: &Request,
    res: &mut Response,
    config: &AppConfig,
    req_p: &Path,
    remote: &str,
) {
    log_msg(
        config.log,
        &format!(
            "{} was served metadata for file {}",
            remote,
            req_p.display()
        ),
    );

    let data = FilesetData {
        writes_supported: config.writes_temp_dir.is_some(),
        is_root: false,
        is_file: true,
        files: vec![get_raw_fs_metadata(req_p)],
    };

    res.status_code(StatusCode::OK);
    res.headers_mut().insert(
        salvo::http::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );
    res.headers_mut()
        .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
    render_snapshot(res, &data);
}

fn handle_get_raw_fs_dir(
    _req: &Request,
    res: &mut Response,
    config: &AppConfig,
    req_p: &Path,
    remote: &str,
    url_path_raw: &str,
) {
    log_msg(
        config.log,
        &format!(
            "{} was served metadata for directory {}",
            remote,
            req_p.display()
        ),
    );

    let segment_count = url_path_raw.split('/').filter(|s| !s.is_empty()).count();
    let is_root = segment_count + !url_path_raw.ends_with('/') as usize == 1
        || url_path_raw == "/"
        || url_path_raw.is_empty();

    let entries = match crate::util::metadata::directory_entries(req_p) {
        Ok(entries) => entries,
        Err(error) => {
            res.status_code(if error.kind() == std::io::ErrorKind::FileTooLarge {
                StatusCode::INSUFFICIENT_STORAGE
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            });
            return;
        }
    };
    let files: Vec<RawFileData> = entries
        .into_iter()
        .filter(|entry| !is_internal_name(&entry.file_name()))
        .filter(|f| {
            let fp = f.path();
            let symlink = is_symlink(&fp);
            !config.is_symlink_denied(symlink, &fp)
        })
        .map(|f| {
            let path = f.path();
            let is_file = f
                .file_type()
                .map(|ft| is_actually_file(&ft, &path))
                .unwrap_or(false);
            if is_file {
                get_raw_fs_metadata(&path)
            } else {
                RawFileData {
                    mime_type: "text/directory".to_string(),
                    name: f.file_name().to_string_lossy().into_owned(),
                    last_modified: file_time_modified_p(&path).to_rfc3339(),
                    size: 0,
                    is_file: false,
                }
            }
        })
        .collect();

    let data = FilesetData {
        writes_supported: config.writes_temp_dir.is_some(),
        is_root,
        is_file: false,
        files,
    };

    res.status_code(StatusCode::OK);
    res.headers_mut().insert(
        salvo::http::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );
    res.headers_mut()
        .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
    render_snapshot(res, &data);
}

fn render_snapshot(res: &mut Response, data: &FilesetData) {
    let mut output =
        crate::util::metadata::MetadataBuffer::new(crate::util::metadata::MAX_METADATA_BYTES);
    match serde_json::to_writer(&mut output, data) {
        Ok(()) => res.body(output.into_bytes()),
        Err(error) => res.status_code(
            if error.io_error_kind() == Some(std::io::ErrorKind::FileTooLarge) {
                StatusCode::INSUFFICIENT_STORAGE
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            },
        ),
    };
}
#[cfg(test)]
mod snapshot_tests {
    use salvo::test::{RequestBuilder, TestClient};

    use super::*;

    #[tokio::test]
    async fn metadata_routes_share_bounded_admission_and_resume_when_capacity_returns() {
        let directory = crate::test_support::TestDir::new();
        std::fs::write(directory.path().join("file.txt"), b"fixture").unwrap();
        let config = crate::test_support::config(directory.path(), &["-d"]);
        let permits = config
            .blocking_jobs
            .clone()
            .acquire_many_owned(4)
            .await
            .unwrap();
        let service = Service::new(crate::build_router(config));
        let json = TestClient::get("http://localhost/")
            .add_header("Accept", "application/json", true)
            .send(&service)
            .await;
        let xml = RequestBuilder::new(
            "http://localhost/",
            salvo::http::Method::from_bytes(b"PROPFIND").unwrap(),
        )
        .add_header("Depth", "1", true)
        .send(&service)
        .await;
        assert_eq!(json.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
        assert_eq!(xml.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
        drop(permits);
        let json = TestClient::get("http://localhost/")
            .add_header("Accept", "application/json", true)
            .send(&service)
            .await;
        let xml = RequestBuilder::new(
            "http://localhost/",
            salvo::http::Method::from_bytes(b"PROPFIND").unwrap(),
        )
        .add_header("Depth", "1", true)
        .send(&service)
        .await;
        assert_eq!(json.status_code, Some(StatusCode::OK));
        assert_eq!(xml.status_code, Some(StatusCode::MULTI_STATUS));
    }
}
