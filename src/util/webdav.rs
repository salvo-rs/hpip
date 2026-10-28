use std::io::{Error as IoError, Result as IoResult};
use std::path::Path;
use std::{fmt, fs};

use walkdir::WalkDir;
use xml::name::Name as XmlName;

macro_rules! xml_name {
    ($ns:expr, $ln:expr) => {
        XmlName {
            local_name: $ln,
            namespace: Some($ns.1),
            prefix: Some($ns.0),
        }
    };
}

/// Prefix and namespace URI for generic WebDAV elements
pub const WEBDAV_XML_NAMESPACE_DAV: (&str, &str) = ("D", "DAV:");

/// Prefix and namespace URI for elements specific to Windows clients
pub const WEBDAV_XML_NAMESPACE_MICROSOFT: (&str, &str) = ("Z", "urn:schemas-microsoft-com:");

/// Prefix and namespace URI for elements for Apache emulation
pub const WEBDAV_XML_NAMESPACE_APACHE: (&str, &str) = ("A", "http://apache.org/dav/props/");

/// All first-class-recognised prefix/namespace pairs.
/// `WEBDAV_XML_NAMESPACE_DAV` needs to be the first here.
pub const WEBDAV_XML_NAMESPACES: &[(&str, &str)] = &[
    WEBDAV_XML_NAMESPACE_DAV,
    WEBDAV_XML_NAMESPACE_MICROSOFT,
    WEBDAV_XML_NAMESPACE_APACHE,
];

/// Properties to return on empty body or `<allprop />` for non-Windows clients
pub const WEBDAV_ALLPROP_PROPERTIES_NON_WINDOWS: &[&[XmlName<'_>]] = &[&[
    xml_name!(WEBDAV_XML_NAMESPACE_DAV, "creationdate"),
    xml_name!(WEBDAV_XML_NAMESPACE_DAV, "getcontentlength"),
    xml_name!(WEBDAV_XML_NAMESPACE_DAV, "getcontenttype"),
    xml_name!(WEBDAV_XML_NAMESPACE_DAV, "getlastmodified"),
    xml_name!(WEBDAV_XML_NAMESPACE_DAV, "resourcetype"),
]];

/// Properties to return on empty body or `<allprop />` for Windows clients
pub const WEBDAV_ALLPROP_PROPERTIES_WINDOWS: &[&[XmlName<'_>]] = &[
    WEBDAV_ALLPROP_PROPERTIES_NON_WINDOWS[0],
    &[
        xml_name!(WEBDAV_XML_NAMESPACE_MICROSOFT, "Win32CreationTime"),
        xml_name!(WEBDAV_XML_NAMESPACE_MICROSOFT, "Win32FileAttributes"),
        xml_name!(WEBDAV_XML_NAMESPACE_MICROSOFT, "Win32LastAccessTime"),
        xml_name!(WEBDAV_XML_NAMESPACE_MICROSOFT, "Win32LastModifiedTime"),
    ],
];

/// Properties listed for a `<propname />` request
pub const WEBDAV_PROPNAME_PROPERTIES: &[&[XmlName<'_>]] = &[
    WEBDAV_ALLPROP_PROPERTIES_NON_WINDOWS[0],
    &[
        xml_name!(WEBDAV_XML_NAMESPACE_APACHE, "executable"),
        xml_name!(WEBDAV_XML_NAMESPACE_MICROSOFT, "Win32LastAccessTime"),
    ],
];

/// The WebDAV Depth header
#[derive(Debug, Copy, Clone, Hash, PartialOrd, Ord, PartialEq, Eq)]
pub enum Depth {
    Zero,
    One,
    Infinity,
}

impl Depth {
    /// Get a depth lower than this one by one, if it exists
    pub fn lower(self) -> Option<Depth> {
        match self {
            Depth::Zero => None,
            Depth::One => Some(Depth::Zero),
            Depth::Infinity => Some(Depth::Infinity),
        }
    }

    /// Parse from a header value string
    pub fn parse(s: &str) -> Option<Depth> {
        match s.trim() {
            "0" => Some(Depth::Zero),
            "1" => Some(Depth::One),
            "infinity" | "Infinity" => Some(Depth::Infinity),
            _ => None,
        }
    }
}

impl fmt::Display for Depth {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Depth::Zero => f.write_str("0"),
            Depth::One => f.write_str("1"),
            Depth::Infinity => f.write_str("infinity"),
        }
    }
}

/// The WebDAV Overwrite header
#[derive(Debug, Copy, Clone, Hash, PartialOrd, Ord, PartialEq, Eq)]
pub struct Overwrite(pub bool);

impl Default for Overwrite {
    fn default() -> Overwrite {
        Overwrite(true)
    }
}

impl Overwrite {
    pub fn parse(s: &str) -> Option<Overwrite> {
        match s.trim() {
            "T" => Some(Overwrite(true)),
            "F" => Some(Overwrite(false)),
            _ => None,
        }
    }
}

/// Check if a User-Agent header indicates a Microsoft client
pub fn client_microsoft(user_agent: Option<&str>) -> bool {
    user_agent
        .map(|s| s.contains("Microsoft") || s.contains("microsoft"))
        .unwrap_or(false)
}

/// Copy a directory recursively from `from` to `to`.
pub fn copy_dir(from: &Path, to: &Path) -> IoResult<Vec<(IoError, String)>> {
    // Disallow copying a directory into itself
    let source = from.canonicalize()?;
    let target = to
        .parent()
        .ok_or_else(|| IoError::other("destination has no parent"))?
        .canonicalize()?
        .join(
            to.file_name()
                .ok_or_else(|| IoError::other("destination has no name"))?,
        );
    if target.starts_with(&source) {
        return Err(IoError::other(
            "cannot copy to a path prefixed by the source path",
        ));
    }
    fs::create_dir(to)?;

    let mut errors = Vec::new();
    for entry in WalkDir::new(from).min_depth(1) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                let path = error
                    .path()
                    .unwrap_or(from)
                    .strip_prefix(from)
                    .unwrap_or(from)
                    .to_string_lossy()
                    .into_owned();
                errors.push((error.into(), path));
                continue;
            }
        };
        let source_metadata = match entry.metadata() {
            Ok(md) => md,
            Err(err) => {
                errors.push((err.into(), entry.path().to_string_lossy().into_owned()));
                continue;
            }
        };

        let relative_path = entry
            .path()
            .strip_prefix(from)
            .expect("strip_prefix failed");
        let target_path = to.join(relative_path);

        if source_metadata.file_type().is_symlink() {
            if let Err(error) = copy_symlink(entry.path(), &target_path) {
                errors.push((error, relative_path.to_string_lossy().into_owned()));
            }
        } else if source_metadata.is_dir() {
            if let Err(e) = fs::create_dir(&target_path) {
                errors.push((e, relative_path.to_string_lossy().into_owned()));
            }
            if let Err(e) = fs::set_permissions(&target_path, source_metadata.permissions()) {
                errors.push((e, relative_path.to_string_lossy().into_owned()));
            }
        } else if source_metadata.is_file() {
            if let Err(error) = fs::copy(entry.path(), &target_path) {
                errors.push((error, relative_path.to_string_lossy().into_owned()));
            }
        } else {
            errors.push((
                IoError::new(std::io::ErrorKind::Unsupported, "cannot copy special file"),
                relative_path.to_string_lossy().into_owned(),
            ));
        }
    }

    Ok(errors)
}

#[cfg(unix)]
fn copy_symlink(from: &Path, to: &Path) -> IoResult<()> {
    std::os::unix::fs::symlink(fs::read_link(from)?, to)
}

#[cfg(windows)]
fn copy_symlink(from: &Path, to: &Path) -> IoResult<()> {
    use std::os::windows::fs::{FileTypeExt, symlink_dir, symlink_file};
    let target = fs::read_link(from)?;
    if fs::symlink_metadata(from)?.file_type().is_symlink_dir() {
        symlink_dir(target, to)
    } else {
        symlink_file(target, to)
    }
}

/// Parse a Win32 time string to ms since epoch
pub fn win32time(t: &str) -> Option<u64> {
    let tm = chrono::DateTime::parse_from_rfc2822(t)
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(t))
        .ok()?;
    tm.timestamp_millis().try_into().ok()
}

#[cfg(test)]
mod copy_tests {
    use super::*;
    use crate::test_support::TestDir;

    #[test]
    fn copies_nested_content_and_rejects_descendant_destination_without_side_effects() {
        let directory = TestDir::new();
        let from = directory.path().join("source");
        let to = directory.path().join("copy");
        fs::create_dir_all(from.join("nested")).unwrap();
        fs::write(from.join("nested/file"), b"fixture").unwrap();
        assert!(copy_dir(&from, &to).unwrap().is_empty());
        assert_eq!(fs::read(to.join("nested/file")).unwrap(), b"fixture");
        assert!(copy_dir(&from, &from.join("child")).is_err());
        assert!(!from.join("child").exists());
    }

    #[cfg(unix)]
    #[test]
    fn preserves_directory_and_dangling_symbolic_links() {
        let directory = TestDir::new();
        let from = directory.path().join("source");
        let to = directory.path().join("copy");
        fs::create_dir_all(from.join("nested")).unwrap();
        fs::write(from.join("nested/file"), b"fixture").unwrap();
        std::os::unix::fs::symlink("nested", from.join("link")).unwrap();
        std::os::unix::fs::symlink("missing", from.join("dangling")).unwrap();
        assert!(copy_dir(&from, &to).unwrap().is_empty());
        assert_eq!(fs::read_link(to.join("link")).unwrap(), Path::new("nested"));
        assert_eq!(
            fs::read_link(to.join("dangling")).unwrap(),
            Path::new("missing")
        );
        assert_eq!(fs::read(to.join("link/file")).unwrap(), b"fixture");
    }
}
