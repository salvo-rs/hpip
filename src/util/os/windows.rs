use std::fs::{File, Metadata};
use std::io;
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, SetFileTime};

/// Get windows-style attributes for the specified file
pub fn win32_file_attributes(_: &Metadata, path: &Path) -> u32 {
    use std::os::windows::ffi::OsStrExt;
    let mut buf: Vec<u16> = path.as_os_str().encode_wide().collect();
    buf.push(0);
    unsafe { GetFileAttributesW(buf.as_ptr()) }
}

/// ETag based on file size, creation time, and last write time
pub fn file_etag(m: &Metadata) -> String {
    format!(
        "{:x}-{}-{}",
        m.file_size(),
        m.creation_time(),
        m.last_write_time()
    )
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
