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
        if let Some(pp) = percent_decode(pp) {
            cur.push(&*pp);
        } else {
            err = true;
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
