use std::ffi::OsStr;
use std::fs::{File, FileType, Metadata};
use std::io::Read;
use std::path::Path;
use std::str;

/// Check if a path refers to a file (including Unix devices and Windows symlinks).
pub fn is_actually_file<P: AsRef<Path>>(tp: &FileType, p: P) -> bool {
    tp.is_file()
        || (tp.is_symlink()
            && std::fs::metadata(p)
                .map(|m| is_actually_file(&m.file_type(), ""))
                .unwrap_or(false))
        || is_device(tp)
}

/// Detect NUL or invalid UTF-8 in a bounded sample, completing a trailing character.
pub fn file_binary<P: AsRef<Path>>(path: P) -> bool {
    file_binary_impl(path.as_ref())
}

fn file_binary_impl(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return true;
    };
    // Do not open special files just to sniff their content: opening a FIFO can block.
    if is_device(&metadata.file_type()) {
        return true;
    }
    let Ok(mut file) = File::open(path) else {
        return true;
    };
    const SAMPLE: usize = 2048;
    let mut buffer = [0_u8; SAMPLE + 3];
    let mut length = 0;
    while length < SAMPLE {
        match file.read(&mut buffer[length..SAMPLE]) {
            Ok(0) => break,
            Ok(read) => length += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return true,
        }
    }
    if buffer[..length].contains(&0) {
        return true;
    }
    match str::from_utf8(&buffer[..length]) {
        Ok(_) => false,
        Err(error) if error.error_len().is_none() && length == SAMPLE => {
            // The sample may end inside one UTF-8 scalar. Read only its missing
            // continuation bytes, never the rest of an arbitrarily large file.
            let start = error.valid_up_to();
            let width = match buffer[start] {
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => return true,
            };
            let end = start + width;
            if file.read_exact(&mut buffer[length..end]).is_err() {
                return true;
            }
            str::from_utf8(&buffer[..end]).is_err()
        }
        Err(_) => true,
    }
}

/// Get the suffix for the icon to use to represent the given file.
pub fn file_icon_suffix<P: AsRef<Path>>(f: P, is_file: bool) -> &'static str {
    if is_file {
        let mime = mime_guess::from_path(&f).first();
        match mime {
            Some(ref m) if m.type_() == mime::IMAGE || m.type_() == mime::VIDEO => "_image",
            Some(ref m) if m.type_() == mime::TEXT => "_text",
            Some(ref m) if m.type_() == mime::APPLICATION => "_binary",
            None => {
                if file_binary(&f) {
                    ""
                } else {
                    "_text"
                }
            }
            _ => "",
        }
    } else {
        ""
    }
}

/// Guess MIME type for a file, with override support.
pub fn guess_mime_type(
    req_p: &Path,
    overrides: &std::collections::BTreeMap<std::ffi::OsString, String>,
) -> String {
    let ext = req_p.extension().unwrap_or(OsStr::new(""));

    if let Some(override_mime) = overrides.get(ext) {
        return override_mime.clone();
    }

    mime_guess::from_path(req_p)
        .first()
        .map(|m| m.to_string())
        .unwrap_or_else(|| {
            if file_binary(req_p) {
                "application/octet-stream".to_string()
            } else {
                "text/plain".to_string()
            }
        })
}

// OS-specific implementations
#[cfg(target_os = "windows")]
pub fn is_device(_: &FileType) -> bool {
    false
}

#[cfg(not(target_os = "windows"))]
pub fn is_device(tp: &FileType) -> bool {
    use std::os::unix::fs::FileTypeExt;
    tp.is_block_device() || tp.is_char_device() || tp.is_fifo() || tp.is_socket()
}

/// Check file length. On most platforms this is just meta.len().
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn file_length<P: AsRef<Path> + ?Sized>(meta: &Metadata, _: &P) -> u64 {
    meta.len()
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn file_length<P: AsRef<Path> + ?Sized>(meta: &Metadata, _path: &P) -> u64 {
    use std::os::unix::fs::FileTypeExt;
    if meta.file_type().is_block_device() || meta.file_type().is_char_device() {
        // For block devices, try ioctl to get size; fallback to meta.len()
        meta.len()
    } else {
        meta.len()
    }
}

#[cfg(test)]
mod mime_sample_tests {
    use salvo::prelude::*;
    use salvo::test::{RequestBuilder, ResponseExt, TestClient};

    use super::*;
    use crate::test_support::{TestDir, config};

    #[test]
    fn valid_utf8_without_a_newline_is_text_and_invalid_bytes_stay_binary() {
        let directory = TestDir::new();
        let path = directory.path().join("sample");
        for content in [b"".as_slice(), b"hello", "中文".as_bytes(), b"hello\n"] {
            std::fs::write(&path, content).unwrap();
            assert!(!file_binary(&path), "{content:?}");
        }
        for content in [
            b"hello\0".as_slice(),
            b"hello\xff",
            b"hello\n\xff",
            b"\xe4\xb8",
        ] {
            std::fs::write(&path, content).unwrap();
            assert!(file_binary(&path), "{content:?}");
        }
    }

    #[test]
    fn a_sample_boundary_completes_one_utf8_scalar_but_rejects_incomplete_files() {
        let directory = TestDir::new();
        let path = directory.path().join("sample");
        for (padding, character) in [(2047, "é"), (2047, "中"), (2045, "😀")] {
            let mut content = vec![b'a'; padding];
            content.extend(character.as_bytes());
            content.extend(b"tail beyond sample");
            std::fs::write(&path, content).unwrap();
            assert!(!file_binary(&path));
        }
        for tail in [b"\xe4".as_slice(), b"\xe4xy"] {
            let mut content = vec![b'a'; 2047];
            content.extend(tail);
            std::fs::write(&path, content).unwrap();
            assert!(file_binary(&path));
        }
    }

    #[tokio::test]
    async fn extensionless_text_negotiates_for_get_and_head_without_changing_known_types() {
        let directory = TestDir::new();
        std::fs::write(directory.path().join("plain"), "hello").unwrap();
        std::fs::write(directory.path().join("unicode"), "中文").unwrap();
        std::fs::write(
            directory.path().join("known.png"),
            "text does not override the suffix",
        )
        .unwrap();
        let service = Service::new(crate::build_router(config(directory.path(), &[])));
        for path in ["plain", "unicode"] {
            for method in ["GET", "HEAD"] {
                let mut response = RequestBuilder::new(
                    format!("http://localhost/{path}"),
                    method.parse().unwrap(),
                )
                .add_header("Accept", "text/plain", true)
                .send(&service)
                .await;
                assert_eq!(response.status_code, Some(StatusCode::OK));
                assert_eq!(response.headers()["content-type"], "text/plain");
                if method == "GET" {
                    assert_eq!(
                        response.take_string().await.unwrap(),
                        std::fs::read_to_string(directory.path().join(path)).unwrap()
                    );
                }
            }
        }
        let response = TestClient::get("http://localhost/known.png")
            .add_header("Accept", "image/png", true)
            .send(&service)
            .await;
        assert_eq!(response.status_code, Some(StatusCode::OK));
        assert_eq!(response.headers()["content-type"], "image/png");
    }
}
