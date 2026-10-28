use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{fs, io};

/// Publish a complete entry without replacing an existing destination.
/// Unsupported platforms/filesystems fail rather than emulating this with an
/// existence check followed by a replacing rename.
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
        let wide = |path: &Path| -> io::Result<Vec<u16>> {
            let value: Vec<_> = path.as_os_str().encode_wide().collect();
            if value.contains(&0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "path contains NUL",
                ));
            }
            Ok(value.into_iter().chain(Some(0)).collect())
        };
        let from = wide(from)?;
        let to = wide(to)?;
        // No MOVEFILE_REPLACE_EXISTING and no cross-volume copy fallback.
        if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) } != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let from = std::ffi::CString::new(from.as_os_str().as_bytes())?;
        let to = std::ffi::CString::new(to.as_os_str().as_bytes())?;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(not(any(
        windows,
        target_os = "linux",
        target_os = "android",
        target_os = "macos"
    )))]
    {
        let _ = (from, to);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic no-replace rename is unavailable",
        ))
    }
}

struct CopyStaging(PathBuf);

impl CopyStaging {
    fn create(destination: &Path) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = destination
            .parent()
            .ok_or_else(|| io::Error::other("destination has no parent"))?;
        loop {
            let directory = parent.join(format!(
                ".hpip-upload-copy-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&directory) {
                Ok(()) => return Ok(Self(directory)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for CopyStaging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Copy privately and expose the complete file/collection only if the target
/// name is still absent. Failed publication never changes the source.
pub fn copy_no_replace(from: &Path, to: &Path, recursive: bool) -> io::Result<()> {
    let metadata = fs::metadata(from)?;
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "only files and directories can be copied",
        ));
    }
    let staging = CopyStaging::create(to)?;
    let entry = staging.0.join("entry");
    if metadata.is_file() {
        fs::copy(from, &entry)?;
    } else if recursive {
        let errors = super::webdav::copy_dir(from, &entry)?;
        if !errors.is_empty() {
            return Err(io::Error::other(
                "collection copy failed before publication",
            ));
        }
    } else {
        fs::create_dir(&entry)?;
    }
    rename_no_replace(&entry, to)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn competing_publications(is_move: bool, directory: bool) {
        let fixture = crate::test_support::TestDir::new();
        let target = fixture.path().join("target");
        let barrier = std::sync::Barrier::new(8);
        let results = std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..8)
                .map(|index| {
                    let source = fixture.path().join(format!("source-{index}"));
                    if directory {
                        fs::create_dir(&source).unwrap();
                        fs::write(source.join("file"), index.to_string()).unwrap();
                    } else {
                        fs::write(&source, index.to_string()).unwrap();
                    }
                    let barrier = &barrier;
                    let target = &target;
                    scope.spawn(move || {
                        barrier.wait();
                        let result = if is_move {
                            rename_no_replace(&source, target)
                        } else {
                            copy_no_replace(&source, target, true)
                        };
                        (index, source, result)
                    })
                })
                .collect();
            jobs.into_iter()
                .map(|job| job.join().unwrap())
                .collect::<Vec<_>>()
        });
        let winners: Vec<_> = results
            .iter()
            .filter(|(_, _, result)| result.is_ok())
            .collect();
        assert_eq!(winners.len(), 1);
        let content = fs::read_to_string(if directory {
            target.join("file")
        } else {
            target.clone()
        })
        .unwrap();
        assert_eq!(content, winners[0].0.to_string());
        for (_, source, result) in results {
            if let Err(error) = result {
                assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
                assert!(source.exists(), "losing MOVE must retain its source");
            } else {
                assert_eq!(source.exists(), !is_move);
            }
        }
        assert!(
            fs::read_dir(fixture.path())
                .unwrap()
                .all(|entry| !super::super::path::is_internal_name(&entry.unwrap().file_name()))
        );
    }

    #[test]
    fn concurrent_file_and_directory_moves_have_exactly_one_winner() {
        competing_publications(true, false);
        competing_publications(true, true);
    }

    #[test]
    fn concurrent_file_and_directory_copies_publish_one_complete_winner() {
        competing_publications(false, false);
        competing_publications(false, true);
    }

    #[test]
    fn failed_copy_removes_staging_and_leaves_destination_absent() {
        let fixture = crate::test_support::TestDir::new();
        let target = fixture.path().join("target");
        for recursive in [false, true] {
            assert!(copy_no_replace(&fixture.path().join("missing"), &target, recursive).is_err());
            assert!(!target.exists());
            assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 0);
        }
    }
}
