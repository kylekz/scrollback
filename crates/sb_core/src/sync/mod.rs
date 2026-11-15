//! The sync engine (PLAN §5). A pass syncs one location with one provider: the provider scans
//! and fetches through the location's transport, and the engine plans ([`plan`]) and stores
//! ([`store`]) without knowing the agent. Indexing joins with phase 2 (PLAN §13).
//!
//! A pass can stop anywhere, by cancel or by failure. Everything committed stays, and the next
//! pass continues from the catalogue. A sync only adds (PLAN §2.3): it never changes or deletes
//! a stored segment, and nothing is ever written to a location.

mod plan;
mod store;

use thiserror::Error;
use toolbox_process::CancelToken;

use crate::archive::{Archive, ArchiveError};
use crate::catalogue::CatalogueError;
use crate::location::LocationId;
use crate::provider::{ItemRange, Provider, ProviderError, ProviderId, Source, Unlisted};
use crate::transport::Transport;

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("the sync was cancelled")]
    Cancelled,
    #[error("location {0} is already being synced")]
    Busy(LocationId),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// The provider's fetch did not keep to the order ranges are streamed in.
    #[error("the provider broke the fetch protocol: {0}")]
    Protocol(String),
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    #[error(transparent)]
    Catalogue(#[from] CatalogueError),
}

/// How far a pass is, as it reports it (PLAN §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// The scan found this many items at the location.
    Scanned { items: usize },
    /// The plan: this many items to fetch, of this many bytes together.
    Planned { items: usize, bytes: u64 },
    /// So many of the planned items are dealt with, and so many of the bytes read.
    Fetched { items: usize, bytes: u64 },
}

/// An item a pass could not store. The pass goes on; the next one tries the item again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub key: String,
    pub reason: String,
}

/// What one pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PassReport {
    /// The items the agent has at the location.
    pub scanned: usize,
    /// Skipped: the same length and stamp as last time.
    pub unchanged: usize,
    /// Archived for the first time.
    pub new_items: usize,
    /// Grew at the source; the new bytes extended the current version.
    pub appended: usize,
    /// The stamp changed and the bytes did not.
    pub touched: usize,
    /// Rewritten, shrunk or replaced at the source, or damaged in the archive: fetched whole as
    /// a new version.
    pub new_versions: usize,
    /// Of `new_versions`, those shorter than the copy being read, flagged for the owner.
    pub held_back: usize,
    /// Newly missing at the source. Everything archived of them stays.
    pub gone: usize,
    /// Changed while the pass ran, so nothing of them was stored; left for the next pass.
    pub deferred: usize,
    /// Items that could not be read or stored.
    pub failed: Vec<Failure>,
    /// What the agent has at the location that could not even be scanned.
    pub unlisted: Vec<Unlisted>,
    /// Bytes read from the location.
    pub bytes_fetched: u64,
    /// Of those, the bytes that went into segments.
    pub bytes_stored: u64,
}

impl PassReport {
    /// Why the pass did not archive everything the agent has, if it did not: how many items it
    /// missed and the first reason.
    fn shortfall(&self) -> Option<String> {
        let first = self
            .failed
            .first()
            .map(|failure| &failure.reason)
            .or(self.unlisted.first().map(|unlisted| &unlisted.reason))?;
        Some(format!(
            "{} could not be archived; first: {first}",
            self.failed.len() + self.unlisted.len()
        ))
    }
}

/// One provider's pass over a location.
#[derive(Debug)]
pub struct Pass {
    pub provider: ProviderId,
    pub outcome: Result<PassReport, SyncError>,
}

/**
 * Syncs `location`, reached through `transport`, with each of `providers` whose agent is there,
 * one after another (PLAN §5). One provider's failure does not keep the others from their
 * passes. `progress` is called as each pass advances; `cancel` stops the sync at the next step,
 * with [`SyncError::Cancelled`].
 *
 * The attempt is recorded with the location, and so is how it ended: as a success only when
 * every pass archived everything its agent has there, and otherwise with what went wrong.
 */
pub fn sync_location(
    archive: &Archive,
    location: &LocationId,
    transport: &dyn Transport,
    providers: &[Box<dyn Provider + '_>],
    cancel: &CancelToken,
    progress: &mut dyn FnMut(&Progress),
) -> Result<Vec<Pass>, SyncError> {
    let _syncing = archive
        .begin_sync(location)
        .ok_or_else(|| SyncError::Busy(location.clone()))?;
    let catalogue = archive.catalogue();
    catalogue.record_attempt(location)?;
    let scratch = archive.tmp();
    let mut passes = Vec::new();
    let mut problems = Vec::new();
    for provider in providers {
        let id = provider.id();
        let outcome = match provider.detect(transport, &scratch) {
            Ok(None) => continue,
            Ok(Some(mut source)) => pass(archive, location, id, source.as_mut(), cancel, progress),
            Err(error) => Err(error.into()),
        };
        match &outcome {
            Err(SyncError::Cancelled) => return Err(SyncError::Cancelled),
            Err(error) => problems.push(format!("{id}: {error}")),
            Ok(report) => problems.extend(report.shortfall().map(|short| format!("{id}: {short}"))),
        }
        passes.push(Pass {
            provider: id,
            outcome,
        });
    }
    if problems.is_empty() {
        catalogue.record_outcome(location, None)?;
    } else {
        catalogue.record_outcome(location, Some(&problems.join("; ")))?;
    }
    Ok(passes)
}

/// One pass: scan, plan, fetch and store (PLAN §5).
fn pass(
    archive: &Archive,
    location: &LocationId,
    provider: ProviderId,
    source: &mut dyn Source,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(&Progress),
) -> Result<PassReport, SyncError> {
    let stop_if_cancelled = || {
        if cancel.is_cancelled() {
            Err(SyncError::Cancelled)
        } else {
            Ok(())
        }
    };
    let catalogue = archive.catalogue();
    /* How long the agent keeps transcripts is only for the staleness warning: a settings file
    that can't be read must not stop the backup. */
    let retention_days = source.retention_days().unwrap_or_else(|error| {
        log::warn!("could not read how long {provider} keeps transcripts: {error}");
        None
    });
    catalogue.record_provider(location, provider.as_str(), retention_days)?;

    let scan = source.scan()?;
    progress(&Progress::Scanned {
        items: scan.items.len(),
    });
    stop_if_cancelled()?;

    /* What is under a folder that could not be listed is not missing at the source. */
    let whole = !scan.unlisted.iter().any(|unlisted| unlisted.folder);
    let plan = plan::plan(
        &scan.items,
        whole,
        catalogue.archived(location, provider.as_str())?,
        source,
    )?;
    let (empty, fetches): (Vec<_>, Vec<_>) =
        plan.fetches.iter().partition(|planned| planned.length == 0);
    progress(&Progress::Planned {
        items: fetches.len(),
        bytes: fetches.iter().map(|planned| planned.length).sum(),
    });
    stop_if_cancelled()?;

    catalogue.record_scan(&plan.seen, &plan.gone)?;
    let mut report = PassReport {
        scanned: scan.items.len(),
        unchanged: plan.unchanged,
        touched: plan.seen.len(),
        gone: plan.gone.len(),
        deferred: plan.deferred,
        failed: plan.failed,
        unlisted: scan.unlisted,
        ..PassReport::default()
    };
    let mut store = store::Store {
        archive,
        location,
        provider: provider.as_str(),
        fetches: &fetches,
        cancel,
        progress,
        report: &mut report,
        open: None,
        ended: 0,
        unproven: Vec::new(),
        stopped: None,
    };
    for planned in empty {
        store.store_empty(planned)?;
    }
    if !fetches.is_empty() {
        let requests: Vec<_> = fetches
            .iter()
            .map(|planned| ItemRange {
                key: planned.scanned.key.clone(),
                offset: planned.offset,
                length: planned.length,
            })
            .collect();
        let fetched = source.fetch(&requests, &mut store);
        store.finish(fetched, source)?;
    }
    Ok(report)
}
