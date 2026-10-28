use std::borrow::Cow;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use percent_encoding;

use super::MAX_SYMLINKS;

/// Percent-decode a string. Returns None if the result is not valid UTF-8.
pub fn percent_decode(s: &str) -> Option<Cow<'_, str>> {
    percent_encoding::percent_decode(s.as_bytes())
        .decode_utf8()
        .ok()
}

/// Decode one URL component without allowing it to change the path hierarchy.
pub fn decode_path_component(component: &str) -> Option<Cow<'_, str>> {
    let bytes = component.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit()
            {
                return None;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    let decoded = percent_decode(component)?;
    if decoded.is_empty()
        || matches!(decoded.as_ref(), "." | "..")
        || decoded.contains(['/', '\\', ':', '\0'])
        || decoded.to_ascii_lowercase().starts_with(".hpip-upload-")
        || decoded.to_ascii_lowercase().starts_with(".hpip-internal-")
    {
        return None;
    }
    #[cfg(windows)]
    {
        if decoded.ends_with(['.', ' ']) {
            return None;
        }
        let basename = decoded.split('.').next()?.to_ascii_uppercase();
        if matches!(basename.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (basename.len() == 4
                && (basename.starts_with("COM") || basename.starts_with("LPT"))
                && matches!(basename.as_bytes()[3], b'1'..=b'9'))
        {
            return None;
        }
    }
    Some(decoded)
}

/// Return the same decoded components used by filesystem resolution.
pub fn decoded_url_path(path: &str) -> Option<String> {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| decode_path_component(segment).map(Cow::into_owned))
        .collect::<Option<Vec<_>>>()
        .map(|segments| segments.join("/"))
}

/// Percent-encode the last character if it's white space
pub fn encode_tail_if_trimmed(mut s: Cow<str>) -> Cow<str> {
    if let Some(c) = s.as_bytes().last().copied()
        && c.is_ascii_whitespace()
    {
        let ed = unsafe { s.to_mut().as_mut_vec() };
        ed.pop();
        write!(ed, "%{:02X}", c).expect("Couldn't allocate two more characters?");
    }
    s
}

/// %-escape special characters in a URL
pub fn escape_specials(s: &str) -> Cow<'_, str> {
    let replacements = s
        .bytes()
        .filter(|b| matches!(b, b'%' | b'#' | b'?' | b'[' | b']' | b'"'))
        .count();
    if replacements == 0 {
        return s.into();
    }

    let mut ret = Vec::with_capacity(s.len() + replacements * 2);
    for &b in s.as_bytes() {
        match b {
            b'%' => ret.extend(b"%25"),
            b'#' => ret.extend(b"%23"),
            b'?' => ret.extend(b"%3F"),
            b'[' => ret.extend(b"%5B"),
            b']' => ret.extend(b"%5D"),
            b'"' => ret.extend(b"%22"),
            _ => ret.push(b),
        }
    }
    unsafe { String::from_utf8_unchecked(ret) }.into()
}

/// Check if a path refers to a symlink.
pub fn is_symlink<P: AsRef<Path>>(p: P) -> bool {
    p.as_ref().read_link().is_ok()
}

/// Check if a path is a descendant of (or equal to) another path.
pub fn is_descendant_of<Pw: AsRef<Path>, Po: AsRef<Path>>(who: Pw, of_whom: Po) -> bool {
    let (mut who, of_whom) = if let Ok(p) =
        fs::canonicalize(who).and_then(|w| fs::canonicalize(of_whom).map(|o| (w, o)))
    {
        p
    } else {
        return false;
    };

    if who == of_whom {
        return true;
    }

    while let Some(who_p) = who.parent().map(|p| p.to_path_buf()) {
        who = who_p;
        if who == of_whom {
            return true;
        }
    }

    false
}

/// Check if a path would be a descendant of another path, without requiring it to exist.
pub fn is_nonexistent_descendant_of<Pw: AsRef<Path>, Po: AsRef<Path>>(
    who: Pw,
    of_whom: Po,
) -> bool {
    let mut who = fs::canonicalize(&who).unwrap_or_else(|_| who.as_ref().to_path_buf());
    let of_whom = if let Ok(p) = fs::canonicalize(of_whom) {
        p
    } else {
        return false;
    };

    if who == of_whom {
        return true;
    }

    while let Some(who_p) = who.parent().map(|p| p.to_path_buf()) {
        who = if let Ok(p) = fs::canonicalize(&who_p) {
            p
        } else {
            who_p
        };

        if who == of_whom {
            return true;
        }
    }

    false
}

/// Check, whether, in any place of the path, a file is treated like a directory.
pub fn detect_file_as_dir(mut p: &Path) -> bool {
    while let Some(pnt) = p.parent() {
        if pnt.is_file() {
            return true;
        }
        p = pnt;
    }
    false
}

/// Resolve a URL path against a hosted directory, following symlinks with depth limit.
pub fn resolve_path(
    hosted_dir: &Path,
    url_segments: &[&str],
    follow_symlinks: bool,
) -> (PathBuf, bool, bool) {
    let mut depth_left = MAX_SYMLINKS;
    let mut cur = hosted_dir.to_path_buf();
    let mut sk = false;
    let mut err = false;
    let mut abs = true;

    for pp in url_segments.iter().filter(|p| !p.is_empty()) {
        if let Some(pp) = decode_path_component(pp) {
            cur.push(&*pp);
        } else {
            err = true;
            break;
        }
        while let Ok(newlink) = cur.read_link() {
            sk = true;
            if follow_symlinks && depth_left != 0 {
                if newlink.is_absolute() {
                    cur = newlink;
                } else {
                    abs = false;
                    cur.pop();
                    cur.push(newlink);
                }
                depth_left -= 1;
            } else {
                break;
            }
        }
    }

    if !abs && let Ok(full) = cur.canonicalize() {
        cur = full;
    }

    (cur, sk, err)
}

/// Names reserved for incomplete uploads and private instance state.
pub fn is_internal_name(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy().to_ascii_lowercase();
    name.starts_with(".hpip-upload-") || name.starts_with(".hpip-internal-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn literal_percent_names_address_the_same_file_for_all_representations() {
        use salvo::prelude::*;
        use salvo::test::{ResponseExt, TestClient};

        use crate::test_support::{TestDir, config};

        let fixture = TestDir::new();
        fs::write(fixture.path().join("a%20b.txt"), "percent").unwrap();
        fs::write(fixture.path().join("a b.txt"), "space").unwrap();
        let service = Service::new(crate::build_router(config(fixture.path(), &["-w", "-e"])));
        let url = "http://localhost/a%2520b.txt";
        let mut response = TestClient::get(url).send(&service).await;
        assert_eq!(response.take_string().await.unwrap(), "percent");
        let mut response = TestClient::get(url)
            .add_header("Accept", "application/json", true)
            .send(&service)
            .await;
        let metadata: serde_json::Value =
            serde_json::from_str(&response.take_string().await.unwrap()).unwrap();
        assert_eq!(metadata["files"][0]["name"], "a%20b.txt");
        let response = TestClient::put(url).text("updated").send(&service).await;
        assert_eq!(response.status_code, Some(StatusCode::NO_CONTENT));
        assert_eq!(
            fs::read_to_string(fixture.path().join("a%20b.txt")).unwrap(),
            "updated"
        );
        assert_eq!(
            fs::read_to_string(fixture.path().join("a b.txt")).unwrap(),
            "space"
        );
        let response = TestClient::delete(url).send(&service).await;
        assert_eq!(response.status_code, Some(StatusCode::NO_CONTENT));
        assert!(!fixture.path().join("a%20b.txt").exists());
        assert!(fixture.path().join("a b.txt").exists());
    }

    #[tokio::test]
    async fn malformed_utf8_is_rejected_without_rendering_the_root_directory() {
        use salvo::prelude::*;
        use salvo::test::TestClient;

        use crate::test_support::{TestDir, config};

        let fixture = TestDir::new();
        let service = Service::new(crate::build_router(config(fixture.path(), &["-w"])));
        for method in ["GET", "PUT", "DELETE"] {
            let mut request = TestClient::get("http://localhost/").build();
            *request.method_mut() = method.parse().unwrap();
            *request.uri_mut() = "/%ff".parse().unwrap();
            let response = service.handle(request).await;
            assert_eq!(response.status_code, Some(StatusCode::BAD_REQUEST));
        }
    }

    #[tokio::test]
    async fn all_filesystem_methods_reject_parent_components() {
        use salvo::prelude::*;
        use salvo::test::TestClient;

        use crate::test_support::{TestDir, config};

        let fixture = TestDir::new();
        let hosted = fixture.path().join("served");
        fs::create_dir(&hosted).unwrap();
        fs::write(fixture.path().join("outside.txt"), "outside").unwrap();
        let service = Service::new(crate::build_router(config(
            &hosted,
            &["-w", "-d", "-A", "-e"],
        )));
        for method in ["GET", "PUT", "DELETE", "PROPFIND", "MKCOL", "COPY", "MOVE"] {
            let mut request = TestClient::get("http://localhost/").build();
            *request.method_mut() = method.parse().unwrap();
            // Set the URI after URL construction so no client normalizes it.
            *request.uri_mut() = "/%2e%2e/outside.txt".parse().unwrap();
            let response = service.handle(request).await;
            assert_eq!(
                response.status_code,
                Some(StatusCode::BAD_REQUEST),
                "{method}"
            );
        }
        assert_eq!(
            fs::read_to_string(fixture.path().join("outside.txt")).unwrap(),
            "outside"
        );
    }

    #[test]
    fn rejects_components_that_escape_or_change_path_hierarchy() {
        let root = std::env::temp_dir().join("hpip-resolver-test-root");
        for segment in [
            "..",
            "%2e%2e",
            ".",
            "%2fetc",
            "a%2fb",
            "a%5cb",
            "C%3a",
            "a%00b",
            "%ff",
            "%",
            "%2",
            "%zz",
            ".hpip-upload-123",
            ".HPIP-UPLOAD-456",
            ".hpip-internal-123",
            ".HPIP-INTERNAL-456",
        ] {
            let (resolved, _, invalid) = resolve_path(&root, &[segment], true);
            assert!(invalid, "accepted invalid component {segment}");
            assert_eq!(resolved, root);
        }
    }

    #[test]
    fn retains_single_components_and_decodes_exactly_once() {
        let root = std::env::temp_dir().join("hpip-resolver-test-root");
        let (resolved, _, invalid) = resolve_path(&root, &["dir", "a%2520b.txt"], true);
        assert!(!invalid);
        assert_eq!(resolved, root.join("dir").join("a%20b.txt"));
        assert_eq!(
            decoded_url_path("//dir/a%20b.txt/"),
            Some("dir/a b.txt".into())
        );
    }

    #[cfg(windows)]
    #[test]
    fn rejects_windows_device_names_and_trimmed_aliases() {
        for component in [
            "NUL",
            "aux.txt",
            "COM1.log",
            "LPT9",
            "private.",
            "private%20",
        ] {
            assert!(decode_path_component(component).is_none(), "{component}");
        }
    }
}
