//! Syncing (PLAN §5, §11 `Sync`): the locations, the passes that pull from them, and how far
//! each pass is.
//!
//! A pass runs on a background thread, one per location at a time, and passes for different
//! locations run side by side. It reports into a [`PassProgress`] the service reads a few times
//! a second, and it stops at its next step when its location is stopped; what it committed
//! stays (PLAN §5).
//!
//! **Triggers** (PLAN §5): the start (once the archive is open and its locations are read),
//! the owner ([`SyncService::sync`], [`SyncService::sync_all`]), and [`SYNC_INTERVAL`] while
//! the app is open. The start also brings the session index up to date over every location
//! (`sb_core::sync::index`): what an earlier index left, and what a new parser version asks
//! for. A pass indexes what it changed itself.
//!
//! Views observe the service for the locations' rows, and the library hears
//! [`SyncEvent::Archived`] when what it shows may have changed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use gpui::{AppContext as _, Context, Entity, EventEmitter, Subscription, Task};
use sb_core::CancelToken;
use sb_core::location::{Location, LocationId, LocationKind};
use sb_core::sync::{Found, Progress, SyncError, index, survey, sync_location};
use sb_core::transport::Transport;

use super::{ArchiveService, Providers};
use crate::format;

/// How often every location is synced while the app is open (DESIGN §4.13 offers 5, 15, 30 and
/// 60 minutes; Settings arrive in a later phase).
pub const SYNC_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// How often a running pass's progress is read.
const PROGRESS_TICK: Duration = Duration::from_millis(100);

/// How often, at most, the library hears that a running pass indexed more sessions.
const ARCHIVED_EVERY_MS: i64 = 1000;

/// What this machine is called as a location (DESIGN §4.10).
pub fn local_name() -> &'static str {
    if cfg!(windows) {
        "This PC"
    } else if cfg!(target_os = "macos") {
        "This Mac"
    } else {
        "This machine"
    }
}

/// How far a location's sync is (PLAN §5): over every agent at the location, one after another.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PassProgress {
    /// Whether a plan is known. Until then the location is being scanned.
    pub planned: bool,
    /// The items to fetch and those dealt with, and the same in bytes.
    pub items: usize,
    pub items_done: usize,
    pub bytes: u64,
    pub bytes_done: u64,
    /// The sessions to index and those indexed, once the fetching is over.
    pub sessions: usize,
    pub sessions_done: usize,
    /// What the agents before the one being synced added to the counts above.
    items_before: usize,
    bytes_before: u64,
    sessions_before: usize,
}

impl PassProgress {
    /// Takes in what a pass reports.
    pub(crate) fn apply(&mut self, progress: &Progress) {
        match *progress {
            Progress::Scanned { .. } => {}
            Progress::Planned { items, bytes } => {
                self.planned = true;
                (self.items_before, self.bytes_before) = (self.items_done, self.bytes_done);
                self.items += items;
                self.bytes += bytes;
            }
            Progress::Fetched { bytes } => self.bytes_done = self.bytes_before + bytes,
            Progress::Stored { items } => self.items_done = self.items_before + items,
            Progress::Indexing { sessions, .. } => {
                self.sessions_before = self.sessions_done;
                self.sessions += sessions;
            }
            Progress::Indexed { sessions, .. } => {
                self.sessions_done = self.sessions_before + sessions;
            }
        }
    }

    /// Whether the fetching is over and the sessions it changed are being indexed.
    pub fn is_indexing(&self) -> bool {
        self.sessions > 0
    }

    /// How much of the work in hand is done, from 0 to 1: of the bytes to fetch, or of the
    /// sessions to index.
    pub fn fraction(&self) -> f32 {
        let (done, of) = if self.is_indexing() {
            (self.sessions_done as f64, self.sessions as f64)
        } else {
            (self.bytes_done as f64, self.bytes as f64)
        };
        if of > 0. {
            (done / of).min(1.) as f32
        } else if self.planned {
            1.
        } else {
            0.
        }
    }
}

/// What a location's row shows (PLAN §5). Unreachable arrives with the locations that can be
/// (PLAN §13, phase 5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocationStatus {
    Syncing {
        progress: PassProgress,
        /// Nothing was ever synced from here before.
        first: bool,
        /// When the pass started, in Unix milliseconds.
        started: i64,
    },
    /// The last sync failed: why, in the failing tool's words.
    Failed(String),
    /// When the last sync finished, in Unix milliseconds.
    Synced(i64),
    NeverSynced,
}

/// What an agent's provider found on this machine, or why it could not look.
pub type Survey = Vec<Result<Found, String>>;

pub enum SyncEvent {
    /// The archive may hold sessions it didn't, or other facts about them.
    Archived,
}

struct Running {
    cancel: CancelToken,
    progress: PassProgress,
    first: bool,
    started: i64,
    /// Reads the pass's progress while it runs.
    _poll: Task<()>,
}

pub struct SyncService {
    archive: Entity<ArchiveService>,
    providers: Providers,
    local: Arc<dyn Transport>,
    /// `None` until they are read from the catalogue.
    locations: Option<Vec<Location>>,
    /// What is on this machine, looked for while there is no location (DESIGN §4.10).
    found: Option<Survey>,
    passes: HashMap<LocationId, Running>,
    /// Why a location's last pass stopped, where the catalogue could not record it.
    errors: HashMap<LocationId, String>,
    /// Background jobs other than passes: reading and adding locations, looking at this
    /// machine, the start's index.
    jobs: usize,
    stop_index: CancelToken,
    _archive: Subscription,
    _interval: Option<Task<()>>,
}

impl EventEmitter<SyncEvent> for SyncService {}

impl SyncService {
    pub fn new(
        archive: Entity<ArchiveService>,
        providers: Providers,
        local: Arc<dyn Transport>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            _archive: cx.observe(&archive, |this, _, cx| this.start(cx)),
            archive,
            providers,
            local,
            locations: None,
            found: None,
            passes: HashMap::new(),
            errors: HashMap::new(),
            jobs: 0,
            stop_index: CancelToken::new(),
            _interval: None,
        }
    }

    /// The locations, oldest first; `None` until they are read.
    pub fn locations(&self) -> Option<&[Location]> {
        self.locations.as_deref()
    }

    pub fn location(&self, id: &LocationId) -> Option<&Location> {
        self.locations.as_ref()?.iter().find(|it| it.id == *id)
    }

    /// What this machine has to offer as a location; `None` until it has been looked at.
    pub fn found(&self) -> Option<&Survey> {
        self.found.as_ref()
    }

    pub fn status(&self, location: &Location) -> LocationStatus {
        if let Some(running) = self.passes.get(&location.id) {
            return LocationStatus::Syncing {
                progress: running.progress,
                first: running.first,
                started: running.started,
            };
        }
        let error = self
            .errors
            .get(&location.id)
            .or(location.last_error.as_ref());
        match (error, location.last_success) {
            (Some(error), _) => LocationStatus::Failed(error.clone()),
            (None, Some(at)) => LocationStatus::Synced(at),
            (None, None) => LocationStatus::NeverSynced,
        }
    }

    pub fn is_syncing(&self) -> bool {
        !self.passes.is_empty()
    }

    /// Nothing is syncing, and no job of the service is in flight.
    pub fn is_settled(&self) -> bool {
        self.passes.is_empty() && self.jobs == 0
    }

    /// The start (the module docs), once the archive is open.
    fn start(&mut self, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        if self._interval.is_some() {
            return;
        }
        self._interval = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(SYNC_INTERVAL).await;
                if this.update(cx, |this, cx| this.sync_all(cx)).is_err() {
                    break;
                }
            }
        }));
        let catalogue = archive.catalogue().clone();
        self.run(
            cx,
            move || catalogue.locations(),
            |this, locations, cx| match locations {
                Ok(locations) => {
                    let none = locations.is_empty();
                    this.locations = Some(locations);
                    if none {
                        this.look_here(cx);
                    } else {
                        this.sync_all(cx);
                    }
                }
                Err(error) => log::error!("the locations could not be read: {error}"),
            },
        );
        let (providers, stop) = (self.providers.clone(), self.stop_index.clone());
        self.run(
            cx,
            move || index(&archive, None, &providers, &stop, &mut |_| {}),
            |_, indexed, cx| match indexed {
                Ok(report) if report.sessions > 0 => cx.emit(SyncEvent::Archived),
                Ok(_) | Err(SyncError::Cancelled) => {}
                Err(error) => {
                    log::error!("the session index could not be brought up to date: {error}")
                }
            },
        );
    }

    /// Asks each provider what this machine has (DESIGN §4.10).
    fn look_here(&mut self, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        let (providers, local) = (self.providers.clone(), self.local.clone());
        self.run(
            cx,
            move || {
                survey(&archive, local.as_ref(), &providers)
                    .into_iter()
                    .map(|found| found.map_err(|(provider, error)| format!("{provider}: {error}")))
                    .collect()
            },
            |this, found, _| this.found = Some(found),
        );
    }

    /// Adds this machine as a location and starts its first sync (DESIGN §4.10).
    pub fn add_local(&mut self, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        self.run(
            cx,
            move || {
                archive
                    .catalogue()
                    .add_location(LocationKind::Local, local_name())
            },
            |this, added, cx| match added {
                Ok(location) => {
                    let id = location.id.clone();
                    this.locations.get_or_insert_default().push(location);
                    this.sync(&id, cx);
                }
                Err(error) => log::error!("this machine could not be added as a location: {error}"),
            },
        );
    }

    /// Syncs every location that isn't syncing already.
    pub fn sync_all(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<LocationId> = self
            .locations
            .iter()
            .flatten()
            .map(|location| location.id.clone())
            .collect();
        for id in ids {
            self.sync(&id, cx);
        }
    }

    /// Syncs a location, unless it is syncing already.
    pub fn sync(&mut self, id: &LocationId, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        let Some(location) = self.location(id) else {
            return;
        };
        if self.passes.contains_key(id) {
            return;
        }
        let first = location.last_success.is_none();
        // The one kind of location there is so far is this machine.
        let transport = match location.kind {
            LocationKind::Local => self.local.clone(),
        };
        let cancel = CancelToken::new();
        let shared = Arc::new(Mutex::new(PassProgress::default()));
        let job = cx.background_spawn({
            let (id, providers) = (id.clone(), self.providers.clone());
            let (cancel, shared) = (cancel.clone(), shared.clone());
            async move {
                let synced = sync_location(
                    &archive,
                    &id,
                    transport.as_ref(),
                    &providers,
                    &cancel,
                    &mut |progress| {
                        shared
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .apply(progress);
                    },
                );
                (synced.map(drop), archive.catalogue().locations())
            }
        });
        cx.spawn({
            let id = id.clone();
            async move |this, cx| {
                let (synced, locations) = job.await;
                this.update(cx, |this, cx| {
                    this.passes.remove(&id);
                    match synced {
                        Ok(()) | Err(SyncError::Cancelled) => {
                            this.errors.remove(&id);
                        }
                        Err(error) => {
                            log::error!("a sync stopped: {error}");
                            this.errors.insert(id, error.to_string());
                        }
                    }
                    match locations {
                        Ok(locations) => this.locations = Some(locations),
                        Err(error) => log::error!("the locations could not be read: {error}"),
                    }
                    cx.emit(SyncEvent::Archived);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
        let poll = cx.spawn({
            let id = id.clone();
            async move |this, cx| {
                let mut told = format::now();
                loop {
                    cx.background_executor().timer(PROGRESS_TICK).await;
                    let progress = *shared.lock().unwrap_or_else(PoisonError::into_inner);
                    let alive = this.update(cx, |this, cx| {
                        let Some(running) = this.passes.get_mut(&id) else {
                            return;
                        };
                        if running.progress == progress {
                            return;
                        }
                        let indexed = running.progress.sessions_done != progress.sessions_done;
                        running.progress = progress;
                        if indexed && format::now() - told >= ARCHIVED_EVERY_MS {
                            told = format::now();
                            cx.emit(SyncEvent::Archived);
                        }
                        cx.notify();
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            }
        });
        self.passes.insert(
            id.clone(),
            Running {
                cancel,
                progress: PassProgress::default(),
                first,
                started: format::now(),
                _poll: poll,
            },
        );
        cx.notify();
    }

    /// Stops every running pass at its next step. What each committed stays (PLAN §5).
    pub fn stop_all(&mut self) {
        for running in self.passes.values() {
            running.cancel.cancel();
        }
    }

    /// Runs `work`, which blocks on the archive or on this machine's files, on the background
    /// executor, counted as a job in flight, then `done` with its result.
    fn run<T: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        work: impl FnOnce() -> T + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
    ) {
        self.jobs += 1;
        let job = cx.background_spawn(async move { work() });
        cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                this.jobs -= 1;
                done(this, result, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

impl Drop for SyncService {
    fn drop(&mut self) {
        self.stop_index.cancel();
        self.stop_all();
    }
}
