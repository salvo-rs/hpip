use std::fs::Metadata;
use std::io;
use std::path::Path;

use chrono::{DateTime, Utc};
use salvo::prelude::*;

/// Coordinate conditional checks and namespace mutations made by this server.
/// Filesystem changes by other processes remain outside this process lock.
pub static MUTATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Default, Clone)]
pub(crate) struct WriteConditions {
    pub(crate) if_match: Option<String>,
    pub(crate) if_none_match: Option<String>,
    pub(crate) if_unmodified_since: Option<DateTime<Utc>>,
}

impl WriteConditions {
    pub(crate) fn from_request(req: &Request) -> Result<Self, StatusCode> {
        let header = |name| {
            req.headers()
                .get(name)
                .map(|value| {
                    value
                        .to_str()
                        .map(str::to_owned)
                        .map_err(|_| StatusCode::BAD_REQUEST)
                })
                .transpose()
        };
        Ok(Self {
            if_match: header("if-match")?,
            if_none_match: header("if-none-match")?,
            // Invalid dates are ignored, as required for this conditional header.
            if_unmodified_since: header("if-unmodified-since")?
                .and_then(|date| DateTime::parse_from_rfc2822(&date).ok())
                .map(|date| date.with_timezone(&Utc)),
        })
    }

    pub(crate) fn allows(&self, current: Option<&str>) -> bool {
        let matches = |value: &str, weak: bool| {
            if value.trim() == "*" {
                return current.is_some();
            }
            value.split(',').any(|candidate| {
                let candidate = candidate.trim();
                let candidate = if weak {
                    candidate.strip_prefix("W/").unwrap_or(candidate)
                } else {
                    candidate
                };
                current.is_some_and(|etag| candidate == format!("\"{etag}\""))
            })
        };
        self.if_match
            .as_deref()
            .is_none_or(|value| matches(value, false))
            && self
                .if_none_match
                .as_deref()
                .is_none_or(|value| !matches(value, true))
    }

    pub(crate) fn allows_metadata(&self, current: Option<&Metadata>) -> io::Result<bool> {
        let etag = current.map(super::file_etag);
        if !self.allows(etag.as_deref()) {
            return Ok(false);
        }
        // If-Match takes precedence over If-Unmodified-Since.
        if self.if_match.is_none()
            && let (Some(date), Some(metadata)) = (&self.if_unmodified_since, current)
        {
            let modified: DateTime<Utc> = metadata.modified()?.into();
            return Ok(modified.timestamp() <= date.timestamp());
        }
        Ok(true)
    }

    pub(crate) fn allows_path(&self, path: &Path) -> io::Result<bool> {
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        self.allows_metadata(metadata.as_ref())
    }

    pub(crate) fn create_only(&self) -> bool {
        self.if_none_match
            .as_deref()
            .is_some_and(|value| value.trim() == "*")
    }
}
