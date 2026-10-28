use std::fs::Metadata;
use std::io;
use std::path::Path;

use chrono::{DateTime, Utc};
use salvo::prelude::*;

use super::entity_tag::{EntityTagList, request_tag_field};
use super::http_date::request_date;

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
        let conditions = Self {
            if_match: request_tag_field(req, "if-match")?,
            if_none_match: request_tag_field(req, "if-none-match")?,
            // Invalid dates are ignored, as required for this conditional header.
            if_unmodified_since: request_date(req, "if-unmodified-since"),
        };
        for value in [&conditions.if_match, &conditions.if_none_match]
            .into_iter()
            .flatten()
        {
            if EntityTagList::parse(value).is_none() {
                return Err(StatusCode::BAD_REQUEST);
            }
        }
        Ok(conditions)
    }

    pub(crate) fn allows(&self, current: Option<&str>) -> bool {
        let current = current.map(|tag| format!("\"{tag}\""));
        let matches = |value: &str, weak: bool| {
            EntityTagList::parse(value).is_some_and(|tags| tags.matches(current.as_deref(), weak))
        };
        self.if_match
            .as_deref()
            .is_none_or(|value| matches(value, false))
            && self
                .if_none_match
                .as_deref()
                .is_none_or(|value| !matches(value, true))
    }

    pub(crate) fn allows_snapshot(&self, current: Option<&(Metadata, String)>) -> io::Result<bool> {
        if !self.allows(current.map(|(_, etag)| etag.as_str())) {
            return Ok(false);
        }
        // If-Match takes precedence over If-Unmodified-Since.
        if self.if_match.is_none()
            && let (Some(date), Some((metadata, _))) = (&self.if_unmodified_since, current)
        {
            let modified: DateTime<Utc> = metadata.modified()?.into();
            return Ok(modified.timestamp() <= date.timestamp());
        }
        Ok(true)
    }

    pub(crate) fn allows_path(&self, path: &Path) -> io::Result<bool> {
        let snapshot = match super::path_metadata_and_etag(path) {
            Ok(snapshot) => Some(snapshot),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        self.allows_snapshot(snapshot.as_ref())
    }

    pub(crate) fn create_only(&self) -> bool {
        self.if_none_match
            .as_deref()
            .is_some_and(|value| value.trim() == "*")
    }
}
