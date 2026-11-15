//! Syncing (PLAN §5, §11 `Sync`): the locations, the passes that pull from them, and how far
//! each pass is.
//!
//! A pass runs on a background thread, one per location at a time, and passes for different
//! locations run side by side. It reads its location through the transport its kind takes
//! ([`Connect`]): this machine's files, or a shell in a WSL distro or on a host over SSH. It
//! reports into a [`PassProgress`] the service reads a few times a second, and it stops at its
//! next step when its location is stopped, the transport's running command with it; what it
//! committed stays (PLAN §5).
//!
//! **Unreachable** (PLAN §5): a location that could not be connected to (a machine asleep or
//! off the network) is in a state of its own, since when, which is not a failure: it has no
//! banner, and the next sync tries again.
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
//! ([`SyncService::indexing`]). A location's own index and its pass never run together: a
//! sync asked for while that index runs starts when it is over, so that the row says one thing
//! at a time and the pass does not index again what the index is indexing. The start's index
//! of every location is not waited for: passes start beside it, since a first sync must not
//! wait on a full re-index, and both are safe together: each indexes what the catalogue says
//! is stale, a session at a time, at worst one twice.
//!
//! **The owner's locations** ([`locations`]): adding, renaming and removing one are the
//! service's writes, each on a background job, as are the connection test and the listing of
//! WSL's distros, which views observe.
//!
//! Views observe the service for the locations' rows, and the library hears
//! [`SyncEvent::Archived`] when what it shows may have changed. When the app quits, every pass
//! and index is told to stop.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
#[cfg(any(test, feature = "automation"))]
use std::sync::{Condvar, MutexGuard};
use std::time::Duration;

use gpui::{AppContext as _, Context, Entity, EventEmitter, Subscription, Task};
use sb_core::CancelToken;
use sb_core::archive::Archive;
use sb_core::location::{Location, LocationId, LocationKind};
use sb_core::sync::{Found, IndexReport, Progress, SyncError, index, survey, sync_location};
use sb_core::transport::Transport;

use super::{ArchiveService, Connect, ListDistros, Providers, Tester};
use crate::format;

mod locations;

pub use locations::{
    ConnectionTest, Details, Distros, Expiring, STALE_AFTER_DAYS, Stale, TestFailure, TestState,
    host_problem, refusal, stale,
};

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

/// What a location's row shows (PLAN §5).
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
    /// The last sync could not reach it, nor any since the time given, in Unix milliseconds.
    /// Not a failure: what the location's last good sync archived is all there.
    Unreachable {
        since: i64,
    },
    /// When the last sync finished, in Unix milliseconds.
    Synced(i64),
    NeverSynced,
}

/// What each agent's provider found at a place, or why it could not look.
pub type Survey = Vec<Result<Found, String>>;

/// How each kind of location is reached and tested, and how WSL's distros are listed.
pub struct Reach {
    pub connect: Connect,
    pub tester: Tester,
    pub distros: ListDistros,
}

/// What each of `providers` finds at the place `transport` reads, an agent that could not
/// look named as its provider names it, never by its id.
fn surveyed(archive: &Archive, transport: &dyn Transport, providers: &Providers) -> Survey {
    survey(archive, transport, providers)
        .into_iter()
        .map(|found| {
            found.map_err(|(id, error)| {
                let name = providers
                    .iter()
                    .find(|provider| provider.id() == id)
                    .map_or(id.as_str(), |provider| provider.display().name);
                format!("{name}: {error}")
            })
        })
        .collect()
}

pub enum SyncEvent {
    /// The archive may hold sessions it didn't, or other facts about them.
    Archived,
}

/// What a pass does when it reaches a [`Hold`].
#[cfg(any(test, feature = "automation"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Held {
    /// Waits until it is released.
    Pause,
    /// Stops, as when its location is stopped.
    Stop,
}

#[cfg(any(test, feature = "automation"))]
#[derive(Default)]
struct HoldState {
    /// The pass that has stored this many items acts on the hold.
    after: Option<(usize, Held)>,
    /// A pass is waiting to be released.
    waiting: bool,
    /// How many times the hold was released: a pass waiting goes on at the next release,
    /// even when a hold is set again before its thread wakes.
    releases: u64,
}

/// A point at which the next pass waits or stops, for the tests and the scenarios that need a
/// pass in a known state (`tests/e2e/first_sync.jsonl`). The shipped app has none.
#[cfg(any(test, feature = "automation"))]
#[derive(Clone, Default)]
struct Hold(Arc<(Mutex<HoldState>, Condvar)>);

#[cfg(any(test, feature = "automation"))]
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
                let releases = state.releases;
                state.waiting = true;
                while state.releases == releases {
                    state = self.0.1.wait(state).unwrap_or_else(PoisonError::into_inner);
                }
            }
            _ => {}
        }
    }

    /// Clears the hold. A pass waiting at it is no longer held from here on, though its
    /// thread may wake a little later.
    fn release(&self) {
        let mut state = self.state();
        state.after = None;
        state.waiting = false;
        state.releases += 1;
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
    connect: Connect,
    tester: Tester,
    lister: ListDistros,
    /// The owner's locations; `None` until they are read from the catalogue.
    locations: Option<Vec<Location>>,
    /// The locations the owner removed, whose sessions are still in the archive.
    removed: Vec<Location>,
    /// What the archive holds from each location, and what was found at each.
    details: Details,
    /// Why the locations could not be read.
    failure: Option<String>,
    /// What is on this machine, looked for while there is no location (DESIGN §4.10).
    found: Option<Survey>,
    passes: HashMap<LocationId, Running>,
    /// The indexes running outside a pass (the module docs): of one location, or of all.
    indexes: HashMap<Option<LocationId>, Indexing>,
    /// The locations to sync once their index is over.
    waiting: HashSet<LocationId>,
    /// Why a location's last pass stopped, where the catalogue could not record it.
    errors: HashMap<LocationId, String>,
    /// The places being added as locations.
    adding: Vec<LocationKind>,
    /// The locations to remove once their pass has stopped.
    removing: HashSet<LocationId>,
    /// Why the last of the owner's changes to the locations could not be made.
    refused: Option<String>,
    /// The connection test running or last run.
    test: Option<ConnectionTest>,
    /// WSL's distros, once they have been listed, and what each running one has.
    distros: Option<Distros>,
    /// WSL's distros are being listed.
    listing: bool,
    /// When every location is synced next ([`SYNC_INTERVAL`]), in Unix milliseconds.
    next_round: Option<i64>,
    /// Background jobs other than passes and indexes: reading and adding locations, looking
    /// at this machine.
    jobs: usize,
    stop_index: CancelToken,
    #[cfg(any(test, feature = "automation"))]
    hold: Hold,
    _observers: [Subscription; 2],
    _interval: Option<Task<()>>,
}

impl EventEmitter<SyncEvent> for SyncService {}

impl SyncService {
    pub fn new(
        archive: Entity<ArchiveService>,
        providers: Providers,
        reach: Reach,
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
            connect: reach.connect,
            tester: reach.tester,
            lister: reach.distros,
            locations: None,
            removed: Vec::new(),
            details: Details::default(),
            failure: None,
            found: None,
            passes: HashMap::new(),
            indexes: HashMap::new(),
            waiting: HashSet::new(),
            errors: HashMap::new(),
            adding: Vec::new(),
            removing: HashSet::new(),
            refused: None,
            test: None,
            distros: None,
            listing: false,
            next_round: None,
            jobs: 0,
            stop_index: CancelToken::new(),
            #[cfg(any(test, feature = "automation"))]
            hold: Hold::default(),
            _interval: None,
        }
    }

    /// The owner's locations, oldest first; `None` until they are read.
    pub fn locations(&self) -> Option<&[Location]> {
        self.locations.as_deref()
    }

    /// A location, one the owner removed too: its sessions still name it.
    pub fn location(&self, id: &LocationId) -> Option<&Location> {
        self.locations
            .iter()
            .flatten()
            .chain(&self.removed)
            .find(|it| it.id == *id)
    }

    /// Whether the archive has never had a location: the first run (DESIGN §4.10).
    pub fn is_first_run(&self) -> bool {
        self.locations.as_ref().is_some_and(Vec::is_empty) && self.removed.is_empty()
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
        if let Some(error) = self.errors.get(&location.id) {
            return LocationStatus::Failed(error.clone());
        }
        // The last sync's word: what one that could not reach it knew is from before.
        match (
            &location.unreachable,
            &location.last_error,
            location.last_success,
        ) {
            (Some(unreachable), ..) => LocationStatus::Unreachable {
                since: unreachable.since,
            },
            (None, Some(error), _) => LocationStatus::Failed(error.clone()),
            (None, None, Some(at)) => LocationStatus::Synced(at),
            (None, None, None) => LocationStatus::NeverSynced,
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
        self.next_round = Some(format::now() + SYNC_INTERVAL.as_millis() as i64);
        self._interval = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(SYNC_INTERVAL).await;
                let synced = this.update(cx, |this, cx| {
                    this.next_round = Some(format::now() + SYNC_INTERVAL.as_millis() as i64);
                    this.sync_all(cx);
                });
                if synced.is_err() {
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
            move || {
                let locations = archive.catalogue().locations()?;
                Ok::<_, sb_core::catalogue::CatalogueError>((locations, Details::read(&archive)?))
            },
            |this, read, cx| match read {
                Ok((locations, details)) => {
                    let (removed, locations) =
                        locations.into_iter().partition(|it| it.removed.is_some());
                    this.failure = None;
                    this.locations = Some(locations);
                    this.removed = removed;
                    this.details = details;
                    if this.is_first_run() {
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

    /// Asks each provider what this machine has, and lists WSL's distros, surveying those
    /// that run (DESIGN §4.10).
    fn look_here(&mut self, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        let (providers, connect) = (self.providers.clone(), self.connect.clone());
        self.run(
            cx,
            move || {
                let here = connect(&LocationKind::Local, &CancelToken::new());
                surveyed(&archive, here.as_ref(), &providers)
            },
            |this, found, _| this.found = Some(found),
        );
        self.list_distros(cx);
    }

    /// Adds this machine as a location and starts its first sync (DESIGN §4.10). It is one
    /// location: nothing happens while it is being added, or once it is one.
    pub fn add_local(&mut self, cx: &mut Context<Self>) {
        self.add(LocationKind::Local, local_name().to_string(), true, cx);
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

    /// Syncs a location, unless it is syncing already; while its index runs, once that is
    /// over (the module docs).
    pub fn sync(&mut self, id: &LocationId, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        let Some(location) = self.locations.iter().flatten().find(|it| it.id == *id) else {
            return;
        };
        if self.passes.contains_key(id) || self.removing.contains(id) {
            return;
        }
        if self.indexes.contains_key(&Some(id.clone())) {
            self.waiting.insert(id.clone());
            return;
        }
        let first = location.last_success.is_none();
        let cancel = CancelToken::new();
        let reported = Reported::default();
        let job = cx.background_spawn({
            let (id, kind) = (id.clone(), location.kind.clone());
            let (providers, connect) = (self.providers.clone(), self.connect.clone());
            #[cfg(any(test, feature = "automation"))]
            let hold = self.hold.clone();
            let (cancel, reported) = (cancel.clone(), reported.clone());
            async move {
                // Stopping the pass stops what its transport runs at the location.
                let transport = connect(&kind, &cancel);
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
                        #[cfg(any(test, feature = "automation"))]
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
                                    // A normal state (the module docs), on the location's row.
                                    Err(SyncError::Unreachable(reason)) => {
                                        log::info!("{id} could not be reached: {reason}");
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
                        this.index(Some(id.clone()), cx);
                    }
                    // A location the owner removed while it synced goes once its pass is over.
                    if this.removing.contains(&id) {
                        this.write_removal(id, cx);
                    } else {
                        this.read_details(cx);
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
                        // What the locations hold counts the sessions just indexed.
                        this.read_details(cx);
                        cx.emit(SyncEvent::Archived);
                    }
                    if let Some(id) = location.filter(|id| this.waiting.remove(id)) {
                        this.sync(&id, cx);
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

    /// Stops every running pass at its next step, one waiting at a hold too, and drops the
    /// syncs waiting for an index. What each pass committed stays (PLAN §5).
    pub fn stop_all(&mut self) {
        for running in self.passes.values() {
            running.cancel.cancel();
        }
        self.waiting.clear();
        #[cfg(any(test, feature = "automation"))]
        self.hold.release();
    }

    /// Stops a location's pass at its next step. What it committed stays (PLAN §5).
    pub fn stop(&mut self, id: &LocationId) {
        if let Some(running) = self.passes.get(id) {
            running.cancel.cancel();
            #[cfg(any(test, feature = "automation"))]
            self.hold.release();
        }
        self.waiting.remove(id);
    }

    /// Stops every pass and every index, and the connection test: the app is quitting.
    fn stop_everything(&mut self) {
        self.stop_all();
        self.stop_index.cancel();
        self.cancel_test();
    }

    /// Makes the next pass act on a hold once it has stored `after` items (see [`Held`]).
    /// For the tests and the scenarios.
    #[cfg(any(test, feature = "automation"))]
    pub fn hold_after(&mut self, after: usize, then: Held) {
        self.hold.state().after = Some((after, then));
    }

    /// Lets a pass that is waiting at a hold go on, and clears the hold.
    #[cfg(any(test, feature = "automation"))]
    pub fn release(&mut self) {
        self.hold.release();
    }

    /// Whether a pass is waiting at a hold.
    #[cfg(any(test, feature = "automation"))]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hold_set_again_right_after_a_release_lets_the_waiting_pass_go() {
        let hold = Hold::default();
        hold.state().after = Some((1, Held::Pause));
        let pass = {
            let hold = hold.clone();
            std::thread::spawn(move || {
                hold.reached(&Progress::Stored { items: 1 }, &CancelToken::new());
            })
        };
        while !hold.state().waiting {
            std::thread::yield_now();
        }
        // Released, then held again before the pass's thread has woken.
        hold.release();
        hold.state().after = Some((1, Held::Pause));
        pass.join().expect("the pass went on");
        assert!(!hold.state().waiting);
    }
}
