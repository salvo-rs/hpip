use std::path::Path;

use salvo::prelude::*;

use super::archive::ArchiveType;
use crate::config::AppConfig;
use crate::util::guess_mime_type;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Representation {
    File,
    Json,
    Archive(ArchiveType),
    NotAcceptable,
}

struct Preference {
    media: mime::Mime,
    quality: u16,
    order: usize,
}

/// Parse HTTP qvalues without accepting NaN, excessive precision or out-of-range values.
fn quality(value: &str) -> u16 {
    let Some((integer, fraction)) = value.split_once('.') else {
        return match value {
            "0" => 0,
            "1" => 1000,
            _ => 0,
        };
    };
    if fraction.len() > 3 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return 0;
    }
    match integer {
        "0" => fraction.parse::<u16>().unwrap_or(0) * 10_u16.pow(3 - fraction.len() as u32),
        "1" if fraction.bytes().all(|byte| byte == b'0') => 1000,
        _ => 0,
    }
}

fn preferences(req: &Request) -> Vec<Preference> {
    req.headers()
        .get_all("accept")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .enumerate()
        .filter_map(|(order, value)| {
            let mut fields = value.split(';');
            let mut media = fields.next()?.trim().to_owned();
            let mut q = None;
            for field in fields {
                let (name, value) = field.trim().split_once('=')?;
                if name.trim().eq_ignore_ascii_case("q") {
                    if q.is_some() {
                        return None;
                    }
                    q = Some(quality(value.trim()));
                } else if q.is_none() {
                    media.push(';');
                    media.push_str(field.trim());
                }
            }
            Some(Preference {
                media: media.parse().ok()?,
                quality: q.unwrap_or(1000),
                order,
            })
        })
        .collect()
}

// The most specific matching media range controls an offered representation's qvalue.
// In particular, an explicit q=0 must beat an otherwise acceptable wildcard.
fn score(preferences: &[Preference], offered: &mime::Mime) -> Option<(u16, usize, usize)> {
    preferences
        .iter()
        .filter_map(|preference| {
            let range = &preference.media;
            let specificity = if range.type_() == mime::STAR {
                0
            } else if range.type_() != offered.type_() {
                return None;
            } else if range.subtype() == mime::STAR {
                1
            } else if range.subtype() == offered.subtype() {
                2
            } else {
                return None;
            };
            if !range
                .params()
                .all(|(name, value)| offered.get_param(name) == Some(value))
            {
                return None;
            }
            let specificity = specificity * 100 + range.params().count();
            Some((
                specificity,
                preference.quality,
                usize::MAX - preference.order,
            ))
        })
        .max()
        .map(|(specificity, quality, order)| (quality, specificity, order))
}

pub(super) fn select(req: &Request, config: &AppConfig, path: &Path) -> Representation {
    if !req.headers().contains_key("accept") {
        return Representation::File;
    }
    let preferences = preferences(req);
    let normal = if path.is_dir() {
        super::get::index_file(config, path)
            .map(|index| guess_mime_type(&index, &config.mime_type_overrides))
            .unwrap_or_else(|| "text/html; charset=utf-8".to_owned())
    } else {
        guess_mime_type(path, &config.mime_type_overrides)
    };
    let normal: mime::Mime = normal.parse().unwrap_or(mime::APPLICATION_OCTET_STREAM);
    let mut candidates = vec![(Representation::File, normal, false)];
    candidates.push((Representation::Json, mime::APPLICATION_JSON, true));
    if config.archives {
        for (kind, media) in [
            (ArchiveType::Tar, "application/x-tar"),
            (ArchiveType::Tar, "application/tar"),
            (ArchiveType::Zip, "application/zip"),
            (ArchiveType::Zip, "application/x-zip-compressed"),
        ] {
            candidates.push((Representation::Archive(kind), media.parse().unwrap(), true));
        }
    }
    let mut selected = Representation::NotAcceptable;
    let mut best = None;
    for (candidate, media, explicit) in candidates {
        // Wildcards alone must not turn ordinary navigation into metadata or an archive.
        if explicit
            && !preferences
                .iter()
                .any(|p| p.media.type_() == media.type_() && p.media.subtype() == media.subtype())
        {
            continue;
        }
        if let Some(score) = score(&preferences, &media)
            && score.0 > 0
            && best.is_none_or(|best| score >= best)
        {
            selected = candidate;
            best = Some(score);
        }
    }
    selected
}

#[cfg(test)]
mod tests {
    use salvo::test::{ResponseExt, TestClient};

    use super::*;

    #[tokio::test]
    async fn accept_preferences_select_the_same_get_and_head_representation() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("file.txt"), "fixture").unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &["-A"],
        )));
        for (accept, expected) in [
            ("application/json;q=0,text/plain;q=1", "text/plain"),
            ("application/json;q=0,*/*;q=1", "text/plain"),
            ("application/json;q=0.1,text/plain;q=1", "text/plain"),
            ("application/json;q=1,text/plain;q=0.1", "application/json"),
            ("APPLICATION/JSON; q=0.8,text/*;q=0.2", "application/json"),
            ("*/*", "text/plain"),
        ] {
            for request in [
                TestClient::get("http://localhost/file.txt"),
                TestClient::head("http://localhost/file.txt"),
            ] {
                let mut response = request
                    .add_header("Accept", accept, true)
                    .send(&service)
                    .await;
                assert_eq!(response.status_code, Some(StatusCode::OK), "{accept}");
                assert_eq!(response.headers()["content-type"], expected, "{accept}");
                response.take_bytes(None).await.unwrap();
            }
        }
        let mut ordinary = TestClient::get("http://localhost/file.txt")
            .add_header("Accept", "application/json;q=0,text/plain", true)
            .send(&service)
            .await;
        assert_eq!(ordinary.take_string().await.unwrap(), "fixture");
    }

    #[tokio::test]
    async fn archives_require_an_acceptable_preferred_explicit_media_type() {
        let dir = crate::test_support::TestDir::new();
        std::fs::write(dir.path().join("file.txt"), "fixture").unwrap();
        let service = Service::new(crate::build_router(crate::test_support::config(
            dir.path(),
            &["-A"],
        )));
        for (accept, expected) in [
            (
                "application/zip;q=0,text/html;q=1",
                "text/html; charset=utf-8",
            ),
            (
                "application/zip;q=0.1,text/html;q=1",
                "text/html; charset=utf-8",
            ),
            ("application/zip;q=0,*/*;q=1", "text/html; charset=utf-8"),
            ("application/zip;q=1,text/html;q=0.1", "application/zip"),
            (
                "application/json;q=0.1,application/x-tar;q=1",
                "application/x-tar",
            ),
            ("*/*", "text/html; charset=utf-8"),
        ] {
            for request in [
                TestClient::get("http://localhost/"),
                TestClient::head("http://localhost/"),
            ] {
                let mut response = request
                    .add_header("Accept", accept, true)
                    .send(&service)
                    .await;
                assert_eq!(response.status_code, Some(StatusCode::OK), "{accept}");
                assert_eq!(response.headers()["content-type"], expected, "{accept}");
                response.take_bytes(None).await.unwrap();
            }
        }
    }

    #[test]
    fn quality_and_specificity_do_not_resurrect_excluded_media_ranges() {
        for value in ["NaN", "-1", "1.1", "0.1234"] {
            assert_eq!(quality(value), 0);
        }
        assert_eq!(quality("0.5"), 500);
        let mut request = Request::new();
        request
            .headers_mut()
            .insert("accept", "text/*;q=1,text/plain;q=0".parse().unwrap());
        assert_eq!(
            score(&preferences(&request), &mime::TEXT_PLAIN).unwrap().0,
            0
        );
    }
}
