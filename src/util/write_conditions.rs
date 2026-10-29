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

/// Queue a namespace transaction without keeping abandoned requests behind a
/// writer. Once started, the worker owns the guard and pool permit to completion.
pub(crate) async fn mutation_transaction<R: Send + 'static>(
    config: &std::sync::Arc<crate::config::AppConfig>,
    work: impl FnOnce(tokio::sync::MutexGuard<'static, ()>) -> R + Send + 'static,
) -> Option<Result<R, tokio::task::JoinError>> {
    let (cancel_on_drop, mut cancelled) = tokio::sync::oneshot::channel::<()>();
    let runtime = tokio::runtime::Handle::current();
    let work_config = config.clone();
    let job = config.spawn_blocking(move || {
        let guard = runtime.block_on(mutation_guard(
            &work_config,
            &mut cancelled,
            MUTATION_LOCK.lock(),
        ))?;
        Some(work(guard))
    })?;
    let result = job.await;
    drop(cancel_on_drop);
    match result {
        Ok(Some(result)) => Some(Ok(result)),
        Ok(None) => None,
        Err(error) => Some(Err(error)),
    }
}

async fn mutation_guard(
    config: &crate::config::AppConfig,
    cancelled: &mut tokio::sync::oneshot::Receiver<()>,
    acquire: impl std::future::Future<Output = tokio::sync::MutexGuard<'static, ()>>,
) -> Option<tokio::sync::MutexGuard<'static, ()>> {
    let guard = tokio::select! {
        biased;
        _ = &mut *cancelled => return None,
        _ = config.wait_for_shutdown() => return None,
        guard = acquire => guard,
    };
    // Cancellation or closure can become ready during the lock branch's poll.
    // Only Empty proves that the request still owns its cancellation sender.
    if !matches!(
        cancelled.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ) || config.is_shutting_down()
    {
        return None;
    }
    Some(guard)
}

#[cfg(test)]
mod mutation_grant_tests {
    use super::*;
    use crate::test_support::{TestDir, config};

    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn cancellation_and_shutdown_during_the_lock_poll_discard_the_grant() {
        for shutdown in [false, true] {
            let fixture = TestDir::new();
            let config = config(fixture.path(), &[]);
            let (sender, mut cancelled) = tokio::sync::oneshot::channel::<()>();
            let guard = LOCK.lock().await;
            let mut grant = Some(guard);
            let mut sender = Some(sender);
            // The cancellation branch is polled first and is still Pending.
            // Make it ready from the lock poll that returns the same grant.
            let acquire = std::future::poll_fn(|_| {
                if shutdown {
                    config.close_admission();
                } else {
                    drop(sender.take());
                }
                std::task::Poll::Ready(grant.take().unwrap())
            });
            assert!(
                mutation_guard(&config, &mut cancelled, acquire)
                    .await
                    .is_none()
            );
            assert!(LOCK.try_lock().is_ok());
        }
    }
}

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
