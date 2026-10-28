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

struct CopyStaging {
    path: PathBuf,
    retain_for_recovery: bool,
}

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
            let builder = fs::DirBuilder::new();
            #[cfg(unix)]
            let builder = {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = builder;
                builder.mode(0o700);
                builder
            };
            match builder.create(&directory) {
                Ok(()) => {
                    return Ok(Self {
                        path: directory,
                        retain_for_recovery: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for CopyStaging {
    fn drop(&mut self) {
        if !self.retain_for_recovery {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Copy privately and expose the complete file/collection only if the target
/// name is still absent. Failed publication never changes the source.
pub fn copy_no_replace(from: &Path, to: &Path, recursive: bool) -> io::Result<()> {
    copy_with(from, to, recursive, false, |source, target| {
        fs::copy(source, target).map(|_| ())
    })
}

/// Prepare the entire copy before replacing an existing destination. Ordinary
/// file replacement is atomic; directory/type replacements use a recoverable
/// backup while the caller holds the server's mutation lock.
pub fn copy_replace(from: &Path, to: &Path, recursive: bool) -> io::Result<()> {
    copy_with(from, to, recursive, true, |source, target| {
        fs::copy(source, target).map(|_| ())
    })
}

fn copy_with(
    from: &Path,
    to: &Path,
    recursive: bool,
    overwrite: bool,
    copy_file: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let metadata = fs::metadata(from)?;
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "only files and directories can be copied",
        ));
    }
    if metadata.is_dir() && recursive {
        let source = from.canonicalize()?;
        let target = to
            .parent()
            .ok_or_else(|| io::Error::other("destination has no parent"))?
            .canonicalize()?
            .join(
                to.file_name()
                    .ok_or_else(|| io::Error::other("destination has no name"))?,
            );
        if target.starts_with(&source) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot copy a collection into itself",
            ));
        }
    }
    let mut staging = CopyStaging::create(to)?;
    let entry = staging.path.join("entry");
    if metadata.is_file() {
        copy_file(from, &entry)?;
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
    if !overwrite {
        return rename_no_replace(&entry, to);
    }
    match fs::symlink_metadata(to) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => rename_no_replace(&entry, to),
        Err(error) => Err(error),
        Ok(existing) if metadata.is_file() && existing.is_file() => fs::rename(&entry, to),
        Ok(_) => publish_with_backup(&mut staging, &entry, to, rename_no_replace),
    }
}

fn publish_with_backup(
    staging: &mut CopyStaging,
    entry: &Path,
    destination: &Path,
    rename: impl Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let backup = staging.path.join("original");
    rename(destination, &backup)?;
    if let Err(publish_error) = rename(entry, destination) {
        if let Err(restore_error) = rename(&backup, destination) {
            // Never delete the only remaining copy of the original destination.
            staging.retain_for_recovery = true;
            return Err(io::Error::other(format!(
                "copy publication failed ({publish_error}); restoration failed ({restore_error}); original retained at {}",
                backup.display()
            )));
        }
        return Err(publish_error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacing_a_hardlink_never_truncates_the_source() {
        let fixture = crate::test_support::TestDir::new();
        let source = fixture.path().join("source");
        let target = fixture.path().join("target");
        fs::write(&source, b"important original data").unwrap();
        fs::hard_link(&source, &target).unwrap();
        copy_replace(&source, &target, false).unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"important original data");
        assert_eq!(fs::read(&target).unwrap(), b"important original data");
    }

    #[test]
    fn partial_copy_failure_preserves_existing_destination_and_cleans_staging() {
        let fixture = crate::test_support::TestDir::new();
        let source = fixture.path().join("source");
        let target = fixture.path().join("target");
        fs::write(&source, b"new data").unwrap();
        fs::write(&target, b"old destination").unwrap();
        let result = copy_with(&source, &target, true, true, |_, staged| {
            fs::write(staged, b"partial")?;
            Err(io::Error::other("simulated read or write failure"))
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old destination");
        assert_eq!(fs::read(&source).unwrap(), b"new data");
        assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 2);
    }

    #[test]
    fn failed_directory_publication_restores_the_complete_original() {
        let fixture = crate::test_support::TestDir::new();
        let target = fixture.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("original-file"), b"old collection").unwrap();
        let mut staging = CopyStaging::create(&target).unwrap();
        let entry = staging.path.join("entry");
        fs::create_dir(&entry).unwrap();
        fs::write(entry.join("new-file"), b"new collection").unwrap();
        let result = publish_with_backup(&mut staging, &entry, &target, |from, to| {
            if from == entry {
                Err(io::Error::other("publication failed"))
            } else {
                rename_no_replace(from, to)
            }
        });
        assert!(result.is_err());
        drop(staging);
        assert_eq!(
            fs::read(target.join("original-file")).unwrap(),
            b"old collection"
        );
        assert!(!target.join("new-file").exists());
        assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_restoration_retains_recoverable_original_instead_of_deleting_it() {
        let fixture = crate::test_support::TestDir::new();
        let target = fixture.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("original-file"), b"recover me").unwrap();
        let mut staging = CopyStaging::create(&target).unwrap();
        let retained = staging.path.clone();
        let entry = retained.join("entry");
        fs::write(&entry, b"new copy").unwrap();
        let result = publish_with_backup(&mut staging, &entry, &target, |from, to| {
            if from == entry {
                fs::write(to, b"concurrent entry")?;
                Err(io::Error::other("publication failed"))
            } else {
                rename_no_replace(from, to)
            }
        });
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("original retained at")
        );
        drop(staging);
        assert_eq!(
            fs::read(retained.join("original/original-file")).unwrap(),
            b"recover me"
        );
        assert_eq!(fs::read(&target).unwrap(), b"concurrent entry");
    }

    #[cfg(unix)]
    #[test]
    fn copy_staging_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = crate::test_support::TestDir::new();
        let staging = CopyStaging::create(&fixture.path().join("target")).unwrap();
        assert_eq!(
            fs::metadata(&staging.path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

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
