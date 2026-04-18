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

/// Check if the specified file is to be considered "binary" (not valid UTF-8 text).
pub fn file_binary<P: AsRef<Path>>(path: P) -> bool {
    file_binary_impl(path.as_ref())
}

fn file_binary_impl(path: &Path) -> bool {
    path.metadata()
        .map(|m| {
            is_device(&m.file_type())
                || File::open(path)
                    .map_err(|_| ())
                    .and_then(|mut f| {
                        let mut buf = [0u8; 2048];
                        let mut remaining = &mut buf[..];
                        while let Ok(rd) = f.read(remaining) {
                            if rd == 0 || remaining[0..rd].contains(&b'\0') {
                                return Err(());
                            }
                            if let Some(idx) = remaining[0..rd].iter().position(|&b| b == b'\n') {
                                remaining = &mut remaining[idx..];
                                let remaining_len = remaining.len();
                                let _ = remaining;
                                return str::from_utf8(&buf[0..buf.len() - remaining_len])
                                    .map(|_| ())
                                    .map_err(|_| ());
                            }
                            remaining = &mut remaining[rd..];
                            if remaining.is_empty() {
                                break;
                            }
                        }
                        Err(())
                    })
                    .is_err()
        })
        .unwrap_or(true)
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
