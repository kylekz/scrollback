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
//! the app is open.
//!
//! **The index.** A pass indexes what it changed itself. The start also brings the session
//! index up to date over every location (`sb_core::sync::index`): what an earlier index left,
//! and what a new parser version asks for. And a pass that was stopped, or that failed, is
//! followed by an index of its location, so that what it stored before it ended shows. These
//! run in the background like a pass, with their progress for the sidebar
//! ([`SyncService::indexing`]).
//!
//! Views observe the service for the locations' rows, and the library hears
//! [`SyncEvent::Archived`] when what it shows may have changed. When the app quits, every pass
//! and index is told to stop.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use gpui::{AppContext as _, Context, Entity, EventEmitter, Subscription, Task};
use sb_core::CancelToken;
use sb_core::location::{Location, LocationId, LocationKind};
use sb_core::sync::{Found, IndexReport, Progress, SyncError, index, survey, sync_location};
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
    /// The last sync failed: why, in the failing tool's words, each agent's part led by its
    /// provider's id (`crate::agents::named` writes it for the owner).
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

/// What a pass does when it reaches a [`Hold`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Held {
    /// Waits until it is released.
    Pause,
    /// Stops, as when its location is stopped.
    Stop,
}

#[derive(Default)]
struct HoldState {
    /// The pass that has stored this many items acts on the hold.
    after: Option<(usize, Held)>,
    /// A pass is waiting to be released.
    waiting: bool,
}

/// A point at which the next pass waits or stops, for the tests and the scenarios that need a
/// pass in a known state (`tests/e2e/first_sync.jsonl`). Nothing in the app sets one.
#[derive(Clone, Default)]
struct Hold(Arc<(Mutex<HoldState>, Condvar)>);

impl Hold {
    fn state(&self) -> MutexGuard<'_, HoldState> {
        self.0.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Called by a pass as it reports: acts on the hold once `after` items are stored.
    fn reached(&self, progress: &Progress, cancel: &CancelToken) {
        let Progress::Stored { items } = *progress else {
            return;
        };
        let mut state = self.state();
        match state.after {
            Some((after, Held::Stop)) if items >= after => {
                state.after = None;
                cancel.cancel();
            }
            Some((after, Held::Pause)) if items >= after => {
                state.waiting = true;
                while state.after.is_some() {
                    state = self.0.1.wait(state).unwrap_or_else(PoisonError::into_inner);
                }
                state.waiting = false;
            }
            _ => {}
        }
    }

    fn release(&self) {
        self.state().after = None;
        self.0.1.notify_all();
    }
}

/// What a background job reports its progress into.
type Reported = Arc<Mutex<PassProgress>>;

/// The jobs whose progress is read while they run.
#[derive(Clone)]
enum Work {
    /// A location's pass.
    Pass(LocationId),
    /// An index outside a pass: of one location, or of all.
    Index(Option<LocationId>),
}

struct Running {
    cancel: CancelToken,
    progress: PassProgress,
    first: bool,
    started: i64,
    /// Reads the pass's progress while it runs.
    _poll: Task<()>,
}

struct Indexing {
    progress: PassProgress,
    _poll: Task<()>,
}

pub struct SyncService {
    archive: Entity<ArchiveService>,
    providers: Providers,
    local: Arc<dyn Transport>,
    /// `None` until they are read from the catalogue.
    locations: Option<Vec<Location>>,
    /// Why the locations could not be read.
    failure: Option<String>,
    /// What is on this machine, looked for while there is no location (DESIGN §4.10).
    found: Option<Survey>,
    passes: HashMap<LocationId, Running>,
    /// The indexes running outside a pass (the module docs): of one location, or of all.
    indexes: HashMap<Option<LocationId>, Indexing>,
    /// Why a location's last pass stopped, where the catalogue could not record it.
    errors: HashMap<LocationId, String>,
    /// This machine is being added as a location.
    adding: bool,
    /// Background jobs other than passes and indexes: reading and adding locations, looking
    /// at this machine.
    jobs: usize,
    stop_index: CancelToken,
    hold: Hold,
    _observers: [Subscription; 2],
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
            _observers: [
                cx.observe(&archive, |this, _, cx| this.start(cx)),
                cx.on_app_quit(|this, _| {
                    this.stop_everything();
                    async {}
                }),
            ],
            archive,
            providers,
            local,
            locations: None,
            failure: None,
            found: None,
            passes: HashMap::new(),
            indexes: HashMap::new(),
            errors: HashMap::new(),
            adding: false,
            jobs: 0,
            stop_index: CancelToken::new(),
            hold: Hold::default(),
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

    /// Why the locations could not be read, when they could not.
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
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

    /// How far the index of `location` outside a pass is, or with `None` the index of every
    /// location, while one runs.
    pub fn indexing(&self, location: Option<&LocationId>) -> Option<PassProgress> {
        self.indexes
            .get(&location.cloned())
            .map(|indexing| indexing.progress)
    }

    pub fn is_syncing(&self) -> bool {
        !self.passes.is_empty()
    }

    /// Whether an index is running outside a pass.
    pub fn is_indexing(&self) -> bool {
        !self.indexes.is_empty()
    }

    /// Nothing is syncing or indexing, and no job of the service is in flight.
    pub fn is_settled(&self) -> bool {
        self.passes.is_empty() && self.indexes.is_empty() && self.jobs == 0
    }

    /// The start (the module docs), once the archive is open.
    fn start(&mut self, cx: &mut Context<Self>) {
        if self.archive.read(cx).archive().is_none() || self._interval.is_some() {
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
        self.read_locations(cx);
        self.index(None, cx);
    }

    /// Reads the locations, and syncs them all; with none, looks at this machine instead.
    fn read_locations(&mut self, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        self.run(
            cx,
            move || archive.catalogue().locations(),
            |this, locations, cx| match locations {
                Ok(locations) => {
                    let none = locations.is_empty();
                    this.failure = None;
                    this.locations = Some(locations);
                    if none {
                        this.look_here(cx);
                    } else {
                        this.sync_all(cx);
                    }
                }
                Err(error) => {
                    log::error!("the locations could not be read: {error}");
                    this.failure = Some(error.to_string());
                }
            },
        );
    }

    /// Reads the locations again after they could not be read.
    pub fn retry(&mut self, cx: &mut Context<Self>) {
        if self.locations.is_none() && self.jobs == 0 {
            self.read_locations(cx);
        }
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
                    .map(|found| {
                        found.map_err(|(id, error)| {
                            // An agent is named as its provider names it, never by its id.
                            let name = providers
                                .iter()
                                .find(|provider| provider.id() == id)
                                .map_or(id.as_str(), |provider| provider.display().name);
                            format!("{name}: {error}")
                        })
                    })
                    .collect()
            },
            |this, found, _| this.found = Some(found),
        );
    }

    /// Adds this machine as a location and starts its first sync (DESIGN §4.10). It is one
    /// location: nothing happens while it is being added, or once it is one.
    pub fn add_local(&mut self, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        let is_local = |location: &Location| matches!(location.kind, LocationKind::Local);
        if self.adding || self.locations.iter().flatten().any(is_local) {
            return;
        }
        self.adding = true;
        self.run(
            cx,
            move || {
                archive
                    .catalogue()
                    .add_location(LocationKind::Local, local_name())
            },
            |this, added, cx| {
                this.adding = false;
                match added {
                    Ok(location) => {
                        let id = location.id.clone();
                        this.locations.get_or_insert_default().push(location);
                        this.sync(&id, cx);
                    }
                    Err(error) => {
                        log::error!("this machine could not be added as a location: {error}");
                    }
                }
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
        let reported = Reported::default();
        let job = cx.background_spawn({
            let (id, providers, hold) = (id.clone(), self.providers.clone(), self.hold.clone());
            let (cancel, reported) = (cancel.clone(), reported.clone());
            async move {
                let synced = sync_location(
                    &archive,
                    &id,
                    transport.as_ref(),
                    &providers,
                    &cancel,
                    &mut |progress| {
                        reported
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .apply(progress);
                        hold.reached(progress, &cancel);
                    },
                );
                (synced, archive.catalogue().location(&id))
            }
        });
        cx.spawn({
            let id = id.clone();
            async move |this, cx| {
                let (synced, location) = job.await;
                this.update(cx, |this, cx| {
                    this.passes.remove(&id);
                    /* Only this location's facts are taken from what the job read: another
                    may have been added since. */
                    match location {
                        Ok(Some(location)) => {
                            for known in this.locations.iter_mut().flatten() {
                                if known.id == id {
                                    *known = location.clone();
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(error) => log::error!("a location could not be read: {error}"),
                    }
                    let (mut changed, mut whole) = (false, true);
                    match synced {
                        Ok(passes) => {
                            this.errors.remove(&id);
                            for pass in passes {
                                match pass.outcome {
                                    Ok(report) => {
                                        changed |=
                                            report.unchanged < report.scanned || report.gone > 0;
                                    }
                                    Err(error) => {
                                        changed = true;
                                        log::error!("{}'s pass stopped: {error}", pass.provider);
                                    }
                                }
                                changed |= this.indexed(pass.index);
                            }
                        }
                        Err(SyncError::Cancelled) => {
                            this.errors.remove(&id);
                            whole = false;
                        }
                        Err(error) => {
                            log::error!("a sync stopped: {error}");
                            this.errors.insert(id.clone(), error.to_string());
                            whole = false;
                        }
                    }
                    if !whole {
                        // What it stored before it ended is indexed, so that it shows.
                        this.index(Some(id), cx);
                    }
                    if changed || !whole {
                        cx.emit(SyncEvent::Archived);
                    }
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
        self.passes.insert(
            id.clone(),
            Running {
                cancel,
                progress: PassProgress::default(),
                first,
                started: format::now(),
                _poll: self.poll(Work::Pass(id.clone()), reported, cx),
            },
        );
        cx.notify();
    }

    /// Brings the session index up to date outside a pass: of `location`, or of every
    /// location. One of each runs at a time.
    fn index(&mut self, location: Option<LocationId>, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        if self.indexes.contains_key(&location) {
            return;
        }
        let reported = Reported::default();
        let job = cx.background_spawn({
            let (providers, stop) = (self.providers.clone(), self.stop_index.clone());
            let (location, reported) = (location.clone(), reported.clone());
            async move {
                index(
                    &archive,
                    location.as_ref(),
                    &providers,
                    &stop,
                    &mut |progress| {
                        reported
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .apply(progress);
                    },
                )
            }
        });
        cx.spawn({
            let location = location.clone();
            async move |this, cx| {
                let indexed = job.await;
                this.update(cx, |this, cx| {
                    this.indexes.remove(&location);
                    if this.indexed(indexed) {
                        cx.emit(SyncEvent::Archived);
                    }
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
        self.indexes.insert(
            location.clone(),
            Indexing {
                progress: PassProgress::default(),
                _poll: self.poll(Work::Index(location), reported, cx),
            },
        );
        cx.notify();
    }

    /// Logs what an index could not do. Returns whether it indexed anything.
    fn indexed(&self, indexed: Result<IndexReport, SyncError>) -> bool {
        match indexed {
            Ok(report) => {
                for (session, why) in &report.failed {
                    log::warn!(
                        "the session {} of {} could not be indexed: {why}",
                        session.key,
                        session.provider
                    );
                }
                report.sessions > 0
            }
            Err(SyncError::Cancelled) => false,
            Err(error) => {
                log::error!("the session index could not be brought up to date: {error}");
                false
            }
        }
    }

    /// Reads what `work` reports while it runs, and tells the library, at most once a second,
    /// when it has indexed more sessions.
    fn poll(&self, work: Work, reported: Reported, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let mut told = format::now();
            loop {
                cx.background_executor().timer(PROGRESS_TICK).await;
                let progress = *reported.lock().unwrap_or_else(PoisonError::into_inner);
                let alive = this.update(cx, |this, cx| {
                    let shown = match &work {
                        Work::Pass(id) => this.passes.get_mut(id).map(|it| &mut it.progress),
                        Work::Index(of) => this.indexes.get_mut(of).map(|it| &mut it.progress),
                    };
                    let Some(shown) = shown.filter(|shown| **shown != progress) else {
                        return;
                    };
                    let indexed = shown.sessions_done != progress.sessions_done;
                    *shown = progress;
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
        })
    }

    /// Stops every running pass at its next step, one waiting at a hold too. What each
    /// committed stays (PLAN §5).
    pub fn stop_all(&mut self) {
        for running in self.passes.values() {
            running.cancel.cancel();
        }
        self.hold.release();
    }

    /// Stops every pass and every index: the app is quitting.
    fn stop_everything(&mut self) {
        self.stop_all();
        self.stop_index.cancel();
    }

    /// Makes the next pass act on a hold once it has stored `after` items (see [`Held`]).
    /// For the tests and the scenarios.
    #[cfg(any(test, feature = "automation"))]
    pub fn hold_after(&mut self, after: usize, then: Held) {
        self.hold.state().after = Some((after, then));
    }

    /// Lets a pass that is waiting at a hold go on, and clears the hold.
    pub fn release(&mut self) {
        self.hold.release();
    }

    /// Whether a pass is waiting at a hold.
    pub fn is_held(&self) -> bool {
        self.hold.state().waiting
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
        self.stop_everything();
    }
}
