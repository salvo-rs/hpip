use std::fs::{File, Metadata};
use std::io;
#[cfg(test)]
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::{
    ERROR_CALL_NOT_IMPLEMENTED, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER,
    ERROR_NOT_SUPPORTED, FILETIME,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
    FileIdInfo, GetFileAttributesW, GetFileInformationByHandle, GetFileInformationByHandleEx,
    SetFileTime,
};

/// Get windows-style attributes for the specified file
pub fn win32_file_attributes(_: &Metadata, path: &Path) -> u32 {
    use std::os::windows::ffi::OsStrExt;
    let mut buf: Vec<u16> = path.as_os_str().encode_wide().collect();
    buf.push(0);
    unsafe { GetFileAttributesW(buf.as_ptr()) }
}

/// Query identity and version metadata from the opened object, never its path.
/// Creation and modification times alone can be restored by WebDAV clients.
pub fn file_etag(_: &Metadata, file: &impl AsRawHandle) -> io::Result<String> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // The borrowed file keeps this handle alive for the complete query.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut identity = FILE_ID_INFO::default();
    // ReFS requires the full 128-bit ID; the basic 64-bit index is not unique.
    let identity = if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as _,
            FileIdInfo,
            (&mut identity as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    } != 0
    {
        Ok(identity)
    } else {
        Err(io::Error::last_os_error())
    };
    etag_with_identity(&info, identity)
}

fn etag_with_identity(
    info: &BY_HANDLE_FILE_INFORMATION,
    identity: io::Result<FILE_ID_INFO>,
) -> io::Result<String> {
    let combine = |high: u32, low: u32| (u64::from(high) << 32) | u64::from(low);
    let basic_identity = || {
        format!(
            "{:x}-{:x}",
            info.dwVolumeSerialNumber,
            combine(info.nFileIndexHigh, info.nFileIndexLow)
        )
    };
    let identity = match identity {
        // MS-FSCC specifies zero for file systems without 128-bit IDs.
        Ok(identity) if identity.FileId.Identifier == [0; 16] => basic_identity(),
        Ok(identity) if identity.FileId.Identifier == [0xff; 16] => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file identity is unavailable",
            ));
        }
        Ok(identity) => format!(
            "{:x}-{:032x}",
            identity.VolumeSerialNumber,
            u128::from_le_bytes(identity.FileId.Identifier)
        ),
        // With a fixed valid class and buffer, legacy implementations can reject
        // FileIdInfo as an invalid parameter. Other query errors remain errors.
        Err(error)
            if matches!(
                error.raw_os_error().map(|code| code as u32),
                Some(
                    ERROR_INVALID_FUNCTION
                        | ERROR_NOT_SUPPORTED
                        | ERROR_INVALID_PARAMETER
                        | ERROR_CALL_NOT_IMPLEMENTED
                )
            ) =>
        {
            basic_identity()
        }
        Err(error) => return Err(error),
    };
    Ok(format!(
        "{}-{:x}-{}-{}",
        identity,
        combine(info.nFileSizeHigh, info.nFileSizeLow),
        combine(
            info.ftCreationTime.dwHighDateTime,
            info.ftCreationTime.dwLowDateTime
        ),
        combine(
            info.ftLastWriteTime.dwHighDateTime,
            info.ftLastWriteTime.dwLowDateTime
        ),
    ))
}

/// Open only for attributes, including directories, and keep metadata and tag on
/// that same object. This never requires permission to read the file contents.
pub fn path_metadata_and_etag(path: &Path) -> io::Result<(Metadata, String)> {
    use std::os::windows::fs::OpenOptionsExt;
    let file = File::options()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let metadata = file.metadata()?;
    let etag = file_etag(&metadata, &file)?;
    Ok((metadata, etag))
}

/// Check if file is marked executable
#[inline(always)]
pub fn file_executable(_: &Metadata) -> bool {
    true
}

pub fn set_executable(_: &Path, _: bool) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "executable attributes are not writable on Windows",
    ))
}

pub fn set_mtime(f: &Path, ms: u64) -> io::Result<()> {
    set_times(f, Some(ms), None, None)
}

pub fn set_mtime_f(f: &File, ms: u64) -> io::Result<()> {
    set_times_f(f, Some(ms), None, None)
}

pub fn set_times_f(
    f: &File,
    mtime_ms: Option<u64>,
    atime_ms: Option<u64>,
    ctime_ms: Option<u64>,
) -> io::Result<()> {
    if mtime_ms.is_some() || atime_ms.is_some() || ctime_ms.is_some() {
        let ctime = ctime_ms.map(ms_to_filetime).transpose()?;
        let atime = atime_ms.map(ms_to_filetime).transpose()?;
        let mtime = mtime_ms.map(ms_to_filetime).transpose()?;
        let result = unsafe {
            SetFileTime(
                f.as_raw_handle() as _,
                ctime.as_ref().map_or(std::ptr::null(), |value| value),
                atime.as_ref().map_or(std::ptr::null(), |value| value),
                mtime.as_ref().map_or(std::ptr::null(), |value| value),
            )
        };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub fn set_times(
    f: &Path,
    mtime_ms: Option<u64>,
    atime_ms: Option<u64>,
    ctime_ms: Option<u64>,
) -> io::Result<()> {
    if mtime_ms.is_some() || atime_ms.is_some() || ctime_ms.is_some() {
        let file = File::options().write(true).open(f)?;
        set_times_f(&file, mtime_ms, atime_ms, ctime_ms)?;
    }
    Ok(())
}

/// FILETIME is in increments of 100ns, and in the Win32 epoch
fn ms_to_filetime(ms: u64) -> io::Result<FILETIME> {
    let ft = ms
        .checked_mul(10_000)
        .and_then(|value| value.checked_add(116444736000000000))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "timestamp is out of range"))?;
    Ok(FILETIME {
        dwLowDateTime: (ft & 0xFFFFFFFF) as u32,
        dwHighDateTime: (ft >> 32) as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;

    #[test]
    fn full_file_ids_distinguish_identical_basic_indexes_and_preserve_volume_bits() {
        let basic = BY_HANDLE_FILE_INFORMATION {
            dwVolumeSerialNumber: 7,
            nFileIndexLow: 9,
            ..Default::default()
        };
        let mut first = FILE_ID_INFO {
            VolumeSerialNumber: 0x1122_3344_0000_0007,
            ..Default::default()
        };
        first.FileId.Identifier[0] = 9;
        let mut second = first;
        second.FileId.Identifier[15] = 1;
        let first_tag = etag_with_identity(&basic, Ok(first)).unwrap();
        let second_tag = etag_with_identity(&basic, Ok(second)).unwrap();
        assert_ne!(first_tag, second_tag);
        assert!(first_tag.starts_with("1122334400000007-00000000000000000000000000000009-"));
        second.VolumeSerialNumber ^= 1_u64 << 63;
        assert_ne!(second_tag, etag_with_identity(&basic, Ok(second)).unwrap());
    }

    #[test]
    fn legacy_id_queries_fall_back_without_hiding_unrelated_errors() {
        let basic = BY_HANDLE_FILE_INFORMATION {
            dwVolumeSerialNumber: 7,
            nFileIndexLow: 9,
            ..Default::default()
        };
        for code in [
            ERROR_INVALID_FUNCTION,
            ERROR_NOT_SUPPORTED,
            ERROR_INVALID_PARAMETER,
            ERROR_CALL_NOT_IMPLEMENTED,
        ] {
            assert_eq!(
                etag_with_identity(&basic, Err(io::Error::from_raw_os_error(code as i32))).unwrap(),
                "7-9-0-0-0"
            );
        }
        for code in [5, 6, 32, 1117] {
            assert_eq!(
                etag_with_identity(&basic, Err(io::Error::from_raw_os_error(code)))
                    .unwrap_err()
                    .raw_os_error(),
                Some(code)
            );
        }
        assert_eq!(
            etag_with_identity(&basic, Ok(FILE_ID_INFO::default())).unwrap(),
            "7-9-0-0-0"
        );
        let mut unknown = FILE_ID_INFO::default();
        unknown.FileId.Identifier = [0xff; 16];
        assert_eq!(
            etag_with_identity(&basic, Ok(unknown)).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[tokio::test]
    async fn restored_times_do_not_reuse_replaced_file_tags_or_encoded_content() {
        use std::io::Read;

        use salvo::prelude::*;
        use salvo::test::{ResponseExt, TestClient};

        let directory = TestDir::new();
        let path = directory.path().join("file.txt");
        let original = "original document content\n".repeat(2000);
        let replacement = "replaced document content\n".repeat(2000);
        assert_eq!(original.len(), replacement.len());
        std::fs::write(&path, &original).unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            directory.path(),
            &["-w", "-d"],
        )));
        let update = r#"<D:propertyupdate xmlns:D="DAV:" xmlns:Z="urn:schemas-microsoft-com:"><D:set><D:prop><Z:Win32CreationTime>2020-09-13T12:26:40Z</Z:Win32CreationTime><Z:Win32LastModifiedTime>2020-09-13T12:26:40Z</Z:Win32LastModifiedTime></D:prop></D:set></D:propertyupdate>"#;
        let patch = || {
            salvo::test::RequestBuilder::new(
                "http://localhost/file.txt",
                "PROPPATCH".parse().unwrap(),
            )
            .add_header("Content-Type", "application/xml", true)
            .body(update)
        };
        assert_eq!(
            patch().send(&service).await.status_code,
            Some(StatusCode::MULTI_STATUS)
        );
        let before = path.metadata().unwrap();
        let mut old_tags = Vec::new();
        for coding in ["identity", "gzip"] {
            let mut response = TestClient::get("http://localhost/file.txt")
                .add_header("Accept-Encoding", coding, true)
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::OK));
            old_tags.push(response.headers()["etag"].to_str().unwrap().to_owned());
            let bytes = response.take_bytes(None).await.unwrap();
            let decoded = if coding == "gzip" {
                let mut decoded = String::new();
                flate2::read::GzDecoder::new(bytes.as_ref())
                    .read_to_string(&mut decoded)
                    .unwrap();
                decoded
            } else {
                String::from_utf8(bytes.to_vec()).unwrap()
            };
            assert_eq!(decoded, original);
        }
        assert_eq!(
            TestClient::put("http://localhost/file.txt")
                .body(replacement.clone())
                .send(&service)
                .await
                .status_code,
            Some(StatusCode::NO_CONTENT)
        );
        assert_eq!(
            patch().send(&service).await.status_code,
            Some(StatusCode::MULTI_STATUS)
        );
        let after = path.metadata().unwrap();
        assert_eq!(before.creation_time(), after.creation_time());
        assert_eq!(before.last_write_time(), after.last_write_time());
        assert_eq!(before.file_size(), after.file_size());
        let mut new_tags = Vec::new();
        for (coding, old_tag) in ["identity", "gzip"].into_iter().zip(&old_tags) {
            let mut response = TestClient::get("http://localhost/file.txt")
                .add_header("Accept-Encoding", coding, true)
                .add_header("If-None-Match", old_tag, true)
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::OK));
            let new_tag = response.headers()["etag"].to_str().unwrap().to_owned();
            assert_ne!(&new_tag, old_tag);
            let bytes = response.take_bytes(None).await.unwrap();
            let decoded = if coding == "gzip" {
                let mut decoded = String::new();
                flate2::read::GzDecoder::new(bytes.as_ref())
                    .read_to_string(&mut decoded)
                    .unwrap();
                decoded
            } else {
                String::from_utf8(bytes.to_vec()).unwrap()
            };
            assert_eq!(decoded, replacement);
            let mut head = TestClient::head("http://localhost/file.txt")
                .add_header("Accept-Encoding", coding, true)
                .send(&service)
                .await;
            assert_eq!(head.status_code, Some(StatusCode::OK));
            assert_eq!(head.headers()["etag"], new_tag);
            assert!(head.take_bytes(None).await.unwrap().is_empty());
            let response = TestClient::get("http://localhost/file.txt")
                .add_header("Accept-Encoding", coding, true)
                .add_header("If-None-Match", &new_tag, true)
                .send(&service)
                .await;
            assert_eq!(response.status_code, Some(StatusCode::NOT_MODIFIED));
            new_tags.push(new_tag);
        }
        let mut range = TestClient::get("http://localhost/file.txt")
            .add_header("Accept-Encoding", "identity", true)
            .add_header("Range", "bytes=0-7", true)
            .add_header("If-Range", &new_tags[0], true)
            .send(&service)
            .await;
        assert_eq!(range.status_code, Some(StatusCode::PARTIAL_CONTENT));
        assert_eq!(range.headers()["etag"], new_tags[0]);
        assert_eq!(
            range.take_bytes(None).await.unwrap().as_ref(),
            &replacement.as_bytes()[..8]
        );
        let stale = TestClient::put("http://localhost/file.txt")
            .add_header("If-Match", &old_tags[0], true)
            .body(original)
            .send(&service)
            .await;
        assert_eq!(stale.status_code, Some(StatusCode::PRECONDITION_FAILED));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), replacement);
        let updated = TestClient::put("http://localhost/file.txt")
            .add_header("If-Match", &new_tags[0], true)
            .body(replacement.clone())
            .send(&service)
            .await;
        assert_eq!(updated.status_code, Some(StatusCode::NO_CONTENT));
        let current_tag = format!("\"{}\"", path_metadata_and_etag(&path).unwrap().1);
        let response = patch()
            .add_header("If-Match", &current_tag, true)
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::MULTI_STATUS));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), replacement);
    }

    #[test]
    fn timestamps_propagate_errors_and_reject_overflow() {
        let directory = TestDir::new();
        let path = directory.path().join("file");
        std::fs::write(&path, b"fixture").unwrap();
        set_mtime(&path, 1_600_000_000_123).unwrap();
        let expected = ms_to_filetime(1_600_000_000_123).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().last_write_time(),
            u64::from(expected.dwLowDateTime) | (u64::from(expected.dwHighDateTime) << 32)
        );
        assert!(set_mtime(&directory.path().join("missing"), 1).is_err());
        assert!(set_mtime(&path, u64::MAX).is_err());
    }
}
