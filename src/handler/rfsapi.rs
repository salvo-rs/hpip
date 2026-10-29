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
    let remote = req.remote_addr().to_string();
    match super::read_snapshot::run_metadata(req, config, move |request, work_config, guard| {
        let mut response = Response::new();
        handle_rfsapi_snapshot(request, &mut response, work_config, &remote, guard);
        response
    })
    .await
    {
        Ok(response) => *res = response,
        Err(status) => {
            res.status_code(status);
        }
    }
}

fn handle_rfsapi_snapshot(
    req: &mut Request,
    res: &mut Response,
    config: &AppConfig,
    remote: &str,
    guard: tokio::sync::MutexGuard<'static, ()>,
) {
    if let Some(response) = crate::hoops::auth::check_auth_path(req, config, req.uri().path()) {
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

    let data = if is_file {
        Some(raw_fs_file(config, &req_p, remote))
    } else {
        raw_fs_dir(res, config, &req_p, remote, &url_path_raw)
    };
    drop(guard);
    let Some(data) = data else { return };
    res.status_code(StatusCode::OK);
    res.headers_mut().insert(
        salvo::http::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );
    res.headers_mut()
        .insert(salvo::http::header::SERVER, USER_AGENT.parse().unwrap());
    render_snapshot(res, &data);
}

fn raw_fs_file(config: &AppConfig, req_p: &Path, remote: &str) -> FilesetData {
    log_msg(
        config.log,
        &format!(
            "{} was served metadata for file {}",
            remote,
            req_p.display()
        ),
    );

    FilesetData {
        writes_supported: config.writes_temp_dir.is_some(),
        is_root: false,
        is_file: true,
        files: vec![get_raw_fs_metadata(req_p)],
    }
}

fn raw_fs_dir(
    res: &mut Response,
    config: &AppConfig,
    req_p: &Path,
    remote: &str,
    url_path_raw: &str,
) -> Option<FilesetData> {
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
            return None;
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

    Some(FilesetData {
        writes_supported: config.writes_temp_dir.is_some(),
        is_root,
        is_file: false,
        files,
    })
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
    use salvo::test::{RequestBuilder, ResponseExt, TestClient};

    use super::*;

    #[tokio::test]
    async fn snapshot_worker_rechecks_authority_and_preserves_supplied_credentials() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("protected.txt"), "fixture").unwrap();
        let config =
            crate::test_support::config(dir.path(), &["--path-auth", "protected.txt=user:pass"]);
        for authorized in [false, true] {
            let mut request = TestClient::get("http://localhost/protected.txt").build();
            if authorized {
                request
                    .headers_mut()
                    .insert("authorization", "Basic dXNlcjpwYXNz".parse().unwrap());
            }
            let mut response = Response::new();
            handle_rfsapi_inner(&mut request, &mut response, &config).await;
            assert_eq!(
                response.status_code,
                Some(if authorized {
                    StatusCode::OK
                } else {
                    StatusCode::UNAUTHORIZED
                })
            );
            let body = response.take_string().await.unwrap();
            assert_eq!(body.contains("protected.txt"), authorized);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delayed_metadata_snapshot_uses_current_namespace_authority() {
        let dir = crate::test_support::TestDir::new();
        std::fs::create_dir(dir.path().join("public")).unwrap();
        std::fs::create_dir(dir.path().join("private")).unwrap();
        std::fs::write(dir.path().join("private/member.txt"), "fixture").unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink("public", &alias).unwrap();
        let config =
            crate::test_support::config(dir.path(), &["-l", "--path-auth", "private=user:pass"]);
        let mut request = TestClient::get("http://localhost/alias/").build();
        assert!(crate::hoops::auth::check_auth(&request, &config).is_none());
        {
            let _guard = crate::util::write_conditions::MUTATION_LOCK.lock().await;
            std::fs::remove_file(&alias).unwrap();
            std::os::unix::fs::symlink("private", &alias).unwrap();
        }
        let mut response = Response::new();
        handle_rfsapi_inner(&mut request, &mut response, &config).await;
        assert_eq!(response.status_code, Some(StatusCode::UNAUTHORIZED));
        assert!(!response.take_string().await.unwrap().contains("member.txt"));
    }

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
