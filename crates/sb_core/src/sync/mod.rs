//! The sync engine (PLAN §5). A pass syncs one location with one provider: the provider scans
//! and fetches through the location's transport, and the engine plans ([`plan`]), stores
//! ([`store`]) and indexes ([`index`]) without knowing the agent: it asks the provider to parse.
//!
//! A pass can stop anywhere, by cancel or by failure. Everything committed stays, and the next
//! pass continues from the catalogue. A sync only adds (PLAN §2.3): it never changes or deletes
//! a stored segment, and nothing is ever written to a location.

mod attribute;
mod index;
mod plan;
mod refetch;
mod store;

use std::collections::HashSet;

use thiserror::Error;
use toolbox_process::CancelToken;

use crate::archive::{Archive, ArchiveError};
use crate::catalogue::{self, Catalogue, CatalogueError, SessionId};
use crate::location::{LocationId, LocationKind};
use crate::provider::{Provider, ProviderError, ProviderId, ScannedItem, Source, Unlisted};
use crate::search::SearchError;
use crate::transport::{Transport, TransportError, wsl};

pub use attribute::attribute;
pub use index::{IndexReport, index, index_session};
pub use refetch::refetch;

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("the sync was cancelled")]
    Cancelled,
    /// The location could not be connected to: a machine that is asleep or off the network.
    /// Not a failure (PLAN §5); what was archived before the connection went stays. With what
    /// the launcher said, its verdict on the first line.
    #[error("the location could not be reached: {0}")]
    Unreachable(String),
    #[error("location {0} is already being synced")]
    Busy(LocationId),
    /// An item was to be fetched again whose newest version is not damaged.
    #[error("{0} is not damaged in the archive, so it is not fetched again")]
    NotDamaged(String),
    /// An item was to be fetched again that its location no longer has.
    #[error("{0} is no longer at its location")]
    NotAtLocation(String),
    /// An item fetched again could not be stored: why, in the words of the pass's report.
    #[error("it could not be fetched again: {0}")]
    NotFetched(String),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// The provider's fetch did not keep to the order ranges are streamed in.
    #[error("the provider broke the fetch protocol: {0}")]
    Protocol(String),
    /// A session was to be indexed by another agent's provider than its own.
    #[error("the session is {session}'s, and {provider} does not parse it")]
    NotItsProvider {
        session: String,
        provider: ProviderId,
    },
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    #[error(transparent)]
    Catalogue(#[from] CatalogueError),
    #[error(transparent)]
    Search(#[from] SearchError),
}

/// How long an item may keep changing under every pass that reads it before a pass counts it
/// among what it could not archive. Until then the next pass is expected to get it; after, a
/// sync that keeps leaving it out must not go on looking like one that archived everything.
pub const WAITING_HOURS: i64 = 24;

/// How far a pass is, as it reports it (PLAN §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// The scan found this many items at the location.
    Scanned { items: usize },
    /// The plan: this many items to deal with, and this many bytes to fetch for them.
    Planned { items: usize, bytes: u64 },
    /// So many bytes have been read from the location.
    Fetched { bytes: u64 },
    /// So many of the planned items are dealt with: stored, or left for another pass.
    Stored { items: usize },
    /// What there is to index: this many sessions, whose transcripts hold this many bytes.
    Indexing { sessions: usize, bytes: u64 },
    /// So many of those sessions are indexed, and so many of those bytes read.
    Indexed { sessions: usize, bytes: u64 },
    /// So many sessions' text is made searchable so far by the index's second step, counted
    /// from its start: said after each batch of text it writes, so search finds more.
    Searchable { sessions: usize },
}

/// An item a pass did not archive, and why.
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
    /// The stamp changed, or the item was gone and is back, and the bytes are as archived.
    pub touched: usize,
    /// Rewritten, shrunk or replaced at the source, or damaged in the archive: fetched whole as
    /// a new version.
    pub new_versions: usize,
    /// Of `new_versions`, those shorter than the copy being read, flagged for the owner.
    pub held_back: usize,
    /// Newly missing at the source. Everything archived of them stays.
    pub gone: usize,
    /// Items that changed while the pass read them, so that nothing of them was stored: the
    /// next pass tries again. The catalogue keeps since when each has been waiting.
    pub deferred: Vec<Failure>,
    /// Items that could not be read or stored, and those that have changed under every pass
    /// for more than [`WAITING_HOURS`].
    pub failed: Vec<Failure>,
    /// What the agent has at the location that could not even be scanned.
    pub unlisted: Vec<Unlisted>,
    /// What the provider does not archive, such as symbolic links.
    pub skipped: Vec<Unlisted>,
    /// Of `scanned`, the items of sessions the owner deleted for good, which are not archived
    /// again (PLAN §4.5).
    pub deleted: usize,
    /// The sessions deleted for good (their tombstones) that those items are of: sessions that
    /// still exist at the location, and may be going on there, unarchived.
    pub deleted_sessions: Vec<SessionId>,
    /// Bytes read from the location.
    pub bytes_fetched: u64,
    /// The bytes that went into segments.
    pub bytes_stored: u64,
}

impl PassReport {
    /// Why the pass did not archive everything the agent has, if it did not, as a clause the
    /// owner reads: how many items it could not read and the first reason. Items that only
    /// changed under the pass are not a reason, the next pass has them, until they have waited
    /// too long and are among `failed`.
    fn shortfall(&self) -> Option<String> {
        let first = self
            .failed
            .first()
            .map(|failure| &failure.reason)
            .or(self.unlisted.first().map(|unlisted| &unlisted.reason))?;
        Some(match self.failed.len() + self.unlisted.len() {
            1 => format!("1 item was not archived, because: {first}"),
            items => format!("{items} items were not archived; the first because: {first}"),
        })
    }
}

/// What a provider's agent has at a location, before any of it is archived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub provider: ProviderId,
    /// Where the agent keeps it there ([`Source::root`]).
    pub root: String,
    /// How many sessions its items belong to.
    pub sessions: usize,
    /// How long its items are together: what a first sync copies.
    pub bytes: u64,
    /// How long the agent keeps transcripts there, when it says; `None` too when that could
    /// not be read, which is no reason to doubt the rest.
    pub retention_days: Option<u32>,
}

/**
 * What each of `providers` finds at the location behind `transport`, by a scan alone: what a
 * location offers before it is added (PLAN §6.4; DESIGN §4.10). A provider whose agent is not
 * there is left out; one that could not look is there with why. Nothing is archived or
 * recorded.
 */
pub fn survey(
    archive: &Archive,
    transport: &dyn Transport,
    providers: &[Box<dyn Provider + '_>],
) -> Vec<Result<Found, (ProviderId, ProviderError)>> {
    let scratch = archive.tmp();
    providers
        .iter()
        .filter_map(|provider| {
            let found = provider
                .detect(transport, &scratch)
                .and_then(|source| {
                    let Some(mut source) = source else {
                        return Ok(None);
                    };
                    let items = source.scan()?.items;
                    let sessions: HashSet<_> = items
                        .iter()
                        .filter_map(|item| item.session.as_ref())
                        .collect();
                    Ok(Some(Found {
                        provider: provider.id(),
                        root: source.root(),
                        sessions: sessions.len(),
                        bytes: items.iter().map(|item| item.length).sum(),
                        retention_days: source.retention_days().ok().flatten(),
                    }))
                })
                .map_err(|error| (provider.id(), error));
            found.transpose()
        })
        .collect()
}

/// One provider's pass over a location.
#[derive(Debug)]
pub struct Pass {
    pub provider: ProviderId,
    /// What the pass archived, or why it stopped.
    pub outcome: Result<PassReport, SyncError>,
    /// What was indexed once every pass was over, however this one ended: the provider's
    /// sessions at the location whose items changed.
    pub index: Result<IndexReport, SyncError>,
}

/**
 * Syncs `location`, reached through `transport`, with each of `providers` whose agent is there,
 * one after another (PLAN §5). One provider's failure does not keep the others from their
 * passes. `progress` is called as each pass advances; `cancel` stops the sync at the next step,
 * with [`SyncError::Cancelled`].
 *
 * The attempt is recorded with the location, and so is how it ended: as a success only when
 * every pass could read everything its agent has there, and otherwise with what went wrong.
 * Only then are the sessions the passes changed indexed, those of a pass that failed too:
 * what it stored before it failed is archived, and the backup is on record whatever becomes
 * of the index.
 *
 * When the location itself fails, at the start or from some point on, the sync stops there:
 * the provider after would only meet the same. The pass it stopped in is the last of those
 * returned, after whatever the providers before it did, and its failure is recorded as the
 * location's, once, after what the providers before it fell short of. A location that can't
 * be connected to ends that pass with
 * [`SyncError::Unreachable`] and is recorded as unreachable, which is not an error; a
 * launcher or a shell that fails (a sign-in refused, a host key not accepted, a missing
 * tool) is recorded as the location's error.
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
    let mut stored = Vec::new();
    let mut problems = Vec::new();
    /* Whether it is the location that failed, and not a provider's pass over it; and why
    it could not be reached, when that is how. */
    let mut the_locations = false;
    let mut unreachable = None;
    for provider in providers {
        let id = provider.id();
        let outcome = match provider.detect(transport, &scratch) {
            Ok(None) => continue,
            Ok(Some(mut source)) => pass(archive, location, id, source.as_mut(), cancel, progress),
            Err(error) => Err(error.into()),
        };
        let outcome = match outcome {
            Err(SyncError::Provider(ProviderError::Transport(TransportError::Unreachable(
                reason,
            )))) => Err(SyncError::Unreachable(reason)),
            outcome => outcome,
        };
        the_locations = match &outcome {
            Err(SyncError::Cancelled) => return Err(SyncError::Cancelled),
            /* A transport stopped by the cancel fails its call: that is the cancel, too. */
            Err(_) if cancel.is_cancelled() => return Err(SyncError::Cancelled),
            Err(SyncError::Unreachable(reason)) => {
                unreachable = Some(reason.clone());
                true
            }
            Err(SyncError::Provider(ProviderError::Transport(error)))
                if error.is_the_locations() =>
            {
                problems.push(error.to_string());
                true
            }
            Err(error) => {
                problems.push(format!("{id}: {error}"));
                false
            }
            Ok(report) => {
                problems.extend(report.shortfall().map(|short| format!("{id}: {short}")));
                false
            }
        };
        stored.push((provider, outcome));
        if the_locations {
            break;
        }
    }
    let problems = (!problems.is_empty()).then(|| problems.join("; "));
    match unreachable {
        /* No error, and it leaves what the last sync that reached the location said; but
        what the providers before fell short of, this sync found when it did reach it. */
        Some(reason) => {
            if problems.is_some() {
                catalogue.record_outcome(location, problems.as_deref())?;
            }
            catalogue.record_unreachable(location, &reason)?;
        }
        None => catalogue.record_outcome(location, problems.as_deref())?,
    }
    if !the_locations {
        learn_drive_mount(catalogue, location, transport)?;
    }
    let mut passes = Vec::new();
    for (provider, outcome) in stored {
        let index = index::changed(archive, location, provider.as_ref(), cancel, progress);
        if matches!(index, Err(SyncError::Cancelled)) {
            return Err(SyncError::Cancelled);
        }
        passes.push(Pass {
            provider: provider.id(),
            outcome,
            index,
        });
    }
    /* A log a read held when a delete for good was to empty it is emptied now. */
    catalogue.retry_empty_log();
    Ok(passes)
}

/**
 * Has a WSL distro say where it mounts the Windows drives, and records it: the project paths
 * of its sessions are read by it (PLAN §6.3), so it is learnt before they are indexed, and
 * anew at every sync, since the owner may change it. A file that can't be read just now
 * leaves what is on record.
 */
fn learn_drive_mount(
    catalogue: &Catalogue,
    location: &LocationId,
    transport: &dyn Transport,
) -> Result<(), SyncError> {
    let kind = catalogue.location(location)?.map(|location| location.kind);
    if !matches!(kind, Some(LocationKind::Wsl { .. })) {
        return Ok(());
    }
    match transport.read_small(wsl::CONF) {
        Ok(conf) => {
            let mount = conf.and_then(|conf| wsl::automount_root(&String::from_utf8_lossy(&conf)));
            catalogue.set_drive_mount(location, mount.as_deref())?;
        }
        Err(error) => log::warn!("could not read {}: {error}", wsl::CONF),
    }
    Ok(())
}

/// One pass: scan, plan, fetch and store (PLAN §5). Its index follows the last pass.
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
    catalogue.record_provider(location, provider.as_str(), retention_days, &source.root())?;

    let scan = source.scan()?;
    let scanned = scan.items.len();
    progress(&Progress::Scanned { items: scanned });
    stop_if_cancelled()?;

    let archived = catalogue.archived(location, provider.as_str())?;
    /* What the owner deleted for good is not archived again (PLAN §4.5). */
    let (skips, met) = catalogue.tombstoned(location, provider.as_str())?.skips(
        &scan.items,
        archived
            .iter()
            .map(|archived| (archived.item.key.as_str(), archived.item.session.as_deref())),
    );
    let items: Vec<ScannedItem> = scan
        .items
        .into_iter()
        .zip(skips)
        .filter_map(|(item, skip)| (!skip).then_some(item))
        .collect();
    /* What could not be listed may hold items: none that is missing from the scan is taken
    for missing at the source. */
    let plan = plan::plan(&items, scan.unlisted.is_empty(), archived);
    progress(&Progress::Planned {
        items: plan.work.len(),
        bytes: plan.work.iter().map(|planned| planned.length).sum(),
    });
    stop_if_cancelled()?;

    catalogue.record_scan(&plan.seen, &plan.gone)?;
    let mut report = PassReport {
        scanned,
        deleted: scanned - items.len(),
        deleted_sessions: met
            .into_iter()
            .map(|key| SessionId {
                location: location.clone(),
                provider: provider.as_str().to_string(),
                key,
            })
            .collect(),
        unchanged: plan.unchanged,
        touched: plan.back,
        gone: plan.gone.len(),
        unlisted: scan.unlisted,
        skipped: scan.skipped,
        ..PassReport::default()
    };
    store::Storing {
        archive,
        location,
        provider: provider.as_str(),
        cancel,
        progress,
        report: &mut report,
        done: 0,
    }
    .all(plan.work, source)?;

    let waiting: Vec<_> = report
        .deferred
        .iter()
        .chain(&report.failed)
        .map(|failure| (failure.key.as_str(), failure.reason.as_str()))
        .collect();
    catalogue.record_waiting(location, provider.as_str(), &waiting)?;
    /* Left for the next pass, but only for so long. */
    let too_long = catalogue::now_ms() - WAITING_HOURS * 60 * 60 * 1000;
    let overdue: HashSet<String> = catalogue
        .waiting(location, provider.as_str())?
        .into_iter()
        .filter(|waiting| waiting.since < too_long)
        .map(|waiting| waiting.key)
        .collect();
    let (late, deferred) = std::mem::take(&mut report.deferred)
        .into_iter()
        .partition(|deferred| overdue.contains(&deferred.key));
    report.deferred = deferred;
    report
        .failed
        .extend(late.into_iter().map(|late: Failure| Failure {
            reason: format!(
                "{}, as on every pass for more than {WAITING_HOURS} hours",
                late.reason
            ),
            ..late
        }));
    Ok(report)
}
