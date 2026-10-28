//! Finite snapshots for JSON and WebDAV metadata responses.

use std::fs::{self, DirEntry};
use std::io::{self, Write};
use std::path::Path;

pub const MAX_METADATA_ENTRIES: usize = 10_000;
pub const MAX_METADATA_BYTES: usize = 16 * 1024 * 1024;

pub fn directory_entries(path: &Path) -> io::Result<Vec<DirEntry>> {
    collect_entries(path, MAX_METADATA_ENTRIES)
}

fn collect_entries(path: &Path, maximum: usize) -> io::Result<Vec<DirEntry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entries.len() == maximum {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "metadata entry limit exceeded",
            ));
        }
        entries.push(entry);
    }
    Ok(entries)
}

pub struct MetadataBuffer {
    bytes: Vec<u8>,
    maximum: usize,
}

impl MetadataBuffer {
    pub fn new(maximum: usize) -> Self {
        Self {
            bytes: Vec::new(),
            maximum,
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for MetadataBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.maximum.saturating_sub(self.bytes.len()) {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "metadata output limit exceeded",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_reject_wide_directories_and_propagate_read_errors() {
        let directory = crate::test_support::TestDir::new();
        for name in ["one", "two", "three"] {
            fs::write(directory.path().join(name), b"fixture").unwrap();
        }
        assert_eq!(collect_entries(directory.path(), 3).unwrap().len(), 3);
        assert_eq!(
            collect_entries(directory.path(), 2).unwrap_err().kind(),
            io::ErrorKind::FileTooLarge
        );
        assert_eq!(
            directory_entries(&directory.path().join("missing"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn serialization_cannot_allocate_past_the_output_budget() {
        let mut output = MetadataBuffer::new(8);
        let error = serde_json::to_writer(&mut output, &"too long for this budget").unwrap_err();
        assert_eq!(error.io_error_kind(), Some(io::ErrorKind::FileTooLarge));
        assert!(output.into_bytes().len() <= 8);
    }
}
