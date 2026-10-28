use std::ffi::CString;
use std::fs::{self, File, Metadata};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use libc::{AT_FDCWD, AT_SYMLINK_NOFOLLOW, UTIME_OMIT, futimens, timespec, utimensat};

use super::super::file::is_actually_file;

const FILE_ATTRIBUTE_READONLY: u32 = 0x01;
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x02;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;

/// Get windows-style attributes for the specified file (emulated on Unix)
pub fn win32_file_attributes(meta: &Metadata, path: &Path) -> u32 {
    let mut attr = 0;

    if meta.permissions().readonly() {
        attr |= FILE_ATTRIBUTE_READONLY;
    }

    if path
        .file_name()
        .map(|n| n.as_bytes().starts_with(b"."))
        .unwrap_or(false)
    {
        attr |= FILE_ATTRIBUTE_HIDDEN;
    }

    if !is_actually_file(&meta.file_type(), path) {
        attr |= FILE_ATTRIBUTE_DIRECTORY;
    } else {
        attr |= FILE_ATTRIBUTE_ARCHIVE;
    }

    attr
}

/// `st_dev`-`st_ino`-`st_mtime`
pub fn file_etag(m: &Metadata) -> String {
    format!("{:x}-{}-{}.{}", m.dev(), m.ino(), m.mtime(), m.mtime_nsec())
}

/// Check if file is marked executable
pub fn file_executable(meta: &Metadata) -> bool {
    (meta.permissions().mode() & 0o111) != 0
}

pub fn set_executable(f: &Path, ex: bool) -> io::Result<()> {
    let mut perm = fs::metadata(f)?.permissions();
    if ex {
        // Grant execute only to classes that can already read the file. Reading
        // the process umask by temporarily changing it races every file creator.
        perm.set_mode(perm.mode() | ((perm.mode() & 0o444) >> 2));
    } else {
        perm.set_mode(perm.mode() & !0o111);
    }
    fs::set_permissions(f, perm)
}

pub fn set_mtime(f: &Path, ms: u64) -> io::Result<()> {
    set_times(f, Some(ms), None, None)
}

pub fn set_mtime_f(f: &File, ms: u64) -> io::Result<()> {
    set_times_f(f, Some(ms), None, None)
}

const NO_TIMESPEC: timespec = timespec {
    tv_sec: 0,
    tv_nsec: UTIME_OMIT,
};

pub fn set_times_f(
    f: &File,
    mtime_ms: Option<u64>,
    atime_ms: Option<u64>,
    ctime_ms: Option<u64>,
) -> io::Result<()> {
    if ctime_ms.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "creation time is not writable on Unix",
        ));
    }
    if mtime_ms.is_some() || atime_ms.is_some() {
        let result = unsafe {
            futimens(
                f.as_raw_fd(),
                [
                    atime_ms.map(ms_to_timespec).unwrap_or(NO_TIMESPEC),
                    mtime_ms.map(ms_to_timespec).unwrap_or(NO_TIMESPEC),
                ]
                .as_ptr(),
            )
        };
        if result != 0 {
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
    if ctime_ms.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "creation time is not writable on Unix",
        ));
    }
    if mtime_ms.is_some() || atime_ms.is_some() {
        let path = CString::new(f.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        let result = unsafe {
            utimensat(
                AT_FDCWD,
                path.as_ptr(),
                [
                    atime_ms.map(ms_to_timespec).unwrap_or(NO_TIMESPEC),
                    mtime_ms.map(ms_to_timespec).unwrap_or(NO_TIMESPEC),
                ]
                .as_ptr(),
                AT_SYMLINK_NOFOLLOW,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn ms_to_timespec(ms: u64) -> timespec {
    timespec {
        tv_sec: (ms / 1000) as _,
        tv_nsec: ((ms % 1000) * 1_000_000) as _,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;

    #[test]
    fn timestamps_use_exact_path_and_propagate_errors() {
        let directory = TestDir::new();
        let path = directory.path().join("file-without-an-appended-nul");
        fs::write(&path, b"fixture").unwrap();
        set_mtime(&path, 1_600_000_000_123).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(metadata.mtime(), 1_600_000_000);
        assert_eq!(metadata.mtime_nsec(), 123_000_000);
        assert!(set_mtime(&directory.path().join("missing"), 1).is_err());
        assert!(set_mtime(Path::new("invalid\0path"), 1).is_err());
    }

    #[test]
    fn executable_permissions_follow_readable_classes_without_changing_umask() {
        let directory = TestDir::new();
        let path = directory.path().join("file");
        fs::write(&path, b"fixture").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        set_executable(&path, true).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o750
        );
        set_executable(&path, false).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(set_executable(&directory.path().join("missing"), true).is_err());
    }
}
