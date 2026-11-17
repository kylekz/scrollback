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
//! whatever Settings says; the owner ([`SyncService::sync`], [`SyncService::sync_all`]); and
//! while the app is open, every so often as Settings › Sync every says ([`SyncEvery`], every
//! [`SYNC_INTERVAL`] until the owner says otherwise). A new interval counts from when it is
//! chosen; "Only when I ask" leaves the start and the owner. When the next round is due is a
//! time of the wall clock, looked at every [`ROUND_TICK`], so a round the machine slept
//! through runs as soon as it wakes.
//!
//! **Stale** ([`SyncService::stale`]): after how many days without a good sync a location is
//! stale is the archive's, kept in its catalogue's `kv`, and read with the locations.
//!
//! **Fetch again** ([`refetch`]): an item verify found damaged is fetched again from its
//! location here, as a pass would, and the library hears of it.
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
//! Views observe the service for the locations' rows. The library and search hear
//! [`SyncEvent::Archived`] when what they show may have changed, and Settings › Archive hears
//! [`SyncEvent::Stored`] too, as a pass stores files before it indexes any. A running pass or
//! index tells each at most once a second, and once more after its last change. When the app
//! quits, every pass and index is told to stop.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};
#[cfg(any(test, feature = "automation"))]
use std::sync::{Condvar, MutexGuard};
use std::time::Duration;

use gpui::{AppContext as _, Context, Entity, EventEmitter, Subscription, Task};
use sb_core::CancelToken;
use sb_core::archive::Archive;
use sb_core::location::{Location, LocationId, LocationKind};
use sb_core::sync::{
    Found, IndexReport, Progress, SyncError, attribute, index, survey, sync_location,
};
use sb_core::transport::{ResolveHost, Transport};

use super::settings::{SettingsEvent, SettingsService, SyncEvery};
use super::{ArchiveService, Connect, Launch, ListDistros, Providers, Tester};
use crate::format;

mod locations;
mod refetch;

pub use locations::{
    ConnectionTest, Details, Distros, Expiring, STALE_AFTER_DAYS, STALE_AFTER_RANGE, Stale,
    TestFailure, TestState, name_problem, refusal, stale,
};
pub use refetch::Refetch;
pub use sb_core::location::host_problem;

/// How often every location is synced while the app is open until the owner chooses
/// otherwise in Settings ([`SyncEvery`]'s default).
pub const SYNC_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// How often the rounds look whether one is due (the module docs).
pub const ROUND_TICK: Duration = Duration::from_secs(60);

/// How long a pass that found its location held by something else waits to run again.
const BUSY_RETRY: Duration = Duration::from_secs(2);

/// How often a running pass's progress is read.
const PROGRESS_TICK: Duration = Duration::from_millis(100);

/// How often, at most, a running pass or index tells each kind of news ([`SyncEvent`]).
const NEWS_EVERY_MS: i64 = 1000;

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
    /// The sessions whose text the index has made searchable, as it goes.
    pub searchable_done: usize,
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
            Progress::Searchable { sessions } => self.searchable_done = sessions,
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
    /// The owner stopped the last pass with this much of its plan left to copy, in files and
    /// in bytes: what it copied is kept, and the next pass carries on (PLAN §5).
    Stopped {
        items_left: usize,
        bytes_left: u64,
    },
    NeverSynced,
}

/// What each agent's provider found at a place, or why it could not look.
pub type Survey = Vec<Result<Found, String>>;

/// How each kind of location is reached and tested, and how WSL's distros are listed.
#[derive(Clone)]
pub struct Reach {
    pub connect: Connect,
    pub tester: Tester,
    pub distros: ListDistros,
    /// What starts the shell of a WSL or SSH place: what its connection test runs and shows.
    pub launcher: Launch,
    /// The far side's home the development override reads every WSL and SSH location from,
    /// while it is on: the sheet says so (`scrollback`'s `--dev-location-home`).
    pub development_home: Option<String>,
    /// Which machine a host is to this machine's `ssh` (PLAN §6.4): where a copy whose app
    /// recorded the host it ran on is attributed (PLAN §7.7).
    pub resolve: Arc<dyn ResolveHost>,
}

/**
 * Says again where the copies kept here ran (PLAN §7.7), after a sync or an index. Nothing
 * about it stops what ran before it: an error is logged, and the next one tries again.
 * Returns how many copies' attribution changed.
 */
fn attributed(archive: &Archive, resolve: &dyn ResolveHost, cancel: &CancelToken) -> usize {
    match attribute(archive, resolve, cancel) {
        Ok(changed) => changed,
        Err(SyncError::Cancelled) => 0,
        Err(error) => {
            log::warn!("could not say where copies ran: {error}");
            0
        }
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncEvent {
    /// The archive may hold sessions it didn't, or other facts about them.
    Archived,
    /// A running pass stored more files: the archive grew, though no session it holds has
    /// changed yet (that is [`SyncEvent::Archived`], once they are indexed).
    Stored,
}

/// One kind of news a running pass or index has: told at most every [`NEWS_EVERY_MS`], and
/// owed until then, so that the last change is told however soon it came after the one before.
struct News {
    event: SyncEvent,
    told: i64,
    owed: bool,
}

impl News {
    fn new(event: SyncEvent) -> Self {
        Self {
            event,
            told: format::now(),
            owed: false,
        }
    }

    /// Owes the news if `changed`, and tells what is owed once it may.
    fn tell(&mut self, changed: bool, cx: &mut Context<SyncService>) {
        self.owed |= changed;
        if self.owed && format::now() - self.told >= NEWS_EVERY_MS {
            (self.told, self.owed) = (format::now(), false);
            cx.emit(self.event);
        }
    }
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
        self.release_then(None);
    }

    /// Lets a pass waiting at the hold go on, and acts on `then` from here on instead: the
    /// pass let go among those that reach it.
    fn release_then(&self, then: Option<(usize, Held)>) {
        let mut state = self.state();
        state.after = then;
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
    launch: Launch,
    development_home: Option<String>,
    resolve: Arc<dyn ResolveHost>,
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
    /// What the pass the owner stopped left to copy, files and bytes, until a pass completes.
    stopped: HashMap<LocationId, (usize, u64)>,
    /// The places being added as locations.
    adding: Vec<(LocationKind, String)>,
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
    /// How often every location is synced while the app is open.
    every: SyncEvery,
    /// When every location is synced next, in Unix milliseconds.
    next_round: Option<i64>,
    /// The start has run on this archive.
    started: bool,
    /// The app has moved onto another archive: nothing more starts on this one.
    closed: bool,
    /// The app is opening another archive to move onto: no pass starts meanwhile, so that
    /// the archive it may leave isn't worked ([`super::pause`]).
    paused: bool,
    /// After how many days without a good sync a location is stale (the module docs).
    stale_after: u32,
    /// The items being fetched again, or last fetched again, and how that went.
    refetches: HashMap<refetch::RefetchKey, Refetch>,
    /// The locations fetching an item again, with what stops it.
    refetching: HashMap<LocationId, CancelToken>,
    /// Fetch all again, while it runs.
    fetch_all: Option<refetch::FetchAll>,
    /**
     * Now, in Unix milliseconds, as the rounds count it: the wall clock, which goes on while
     * the machine sleeps. In tests, gpui's clock, which they move.
     */
    clock: Rc<dyn Fn() -> i64>,
    /// Background jobs other than passes and indexes: reading and adding locations, looking
    /// at this machine.
    jobs: usize,
    /// Where copies ran is being said ([`Self::attribute_copies`]), and is to be said again
    /// once that is done.
    attributing: bool,
    attribute_again: bool,
    stop_index: CancelToken,
    #[cfg(any(test, feature = "automation"))]
    hold: Hold,
    _observers: [Subscription; 3],
    _interval: Option<Task<()>>,
}

impl EventEmitter<SyncEvent> for SyncService {}

impl SyncService {
    pub fn new(
        archive: Entity<ArchiveService>,
        providers: Providers,
        reach: Reach,
        settings: &Entity<SettingsService>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            every: settings.read(cx).settings().sync_every,
            _observers: [
                cx.observe(&archive, |this, _, cx| this.start(cx)),
                cx.on_app_quit(|this, _| {
                    this.stop_everything();
                    async {}
                }),
                cx.subscribe(settings, |this, settings, _: &SettingsEvent, cx| {
                    let every = settings.read(cx).settings().sync_every;
                    this.set_every(every, cx);
                }),
            ],
            archive,
            providers,
            connect: reach.connect,
            tester: reach.tester,
            lister: reach.distros,
            launch: reach.launcher,
            development_home: reach.development_home,
            resolve: reach.resolve,
            locations: None,
            removed: Vec::new(),
            details: Details::default(),
            failure: None,
            found: None,
            passes: HashMap::new(),
            indexes: HashMap::new(),
            waiting: HashSet::new(),
            errors: HashMap::new(),
            stopped: HashMap::new(),
            adding: Vec::new(),
            removing: HashSet::new(),
            refused: None,
            test: None,
            distros: None,
            listing: false,
            next_round: None,
            started: false,
            closed: false,
            paused: false,
            stale_after: STALE_AFTER_DAYS,
            refetches: HashMap::new(),
            refetching: HashMap::new(),
            fetch_all: None,
            clock: clock(cx),
            jobs: 0,
            attributing: false,
            attribute_again: false,
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
        if let Some(&(items_left, bytes_left)) = self.stopped.get(&location.id) {
            return LocationStatus::Stopped {
                items_left,
                bytes_left,
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

    /// Whether a pass or a fetch again runs at the location `id`.
    pub fn is_busy(&self, id: &LocationId) -> bool {
        self.passes.contains_key(id) || self.refetching.contains_key(id)
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
        if self.archive.read(cx).archive().is_none() || self.started {
            return;
        }
        self.started = true;
        self.schedule(cx);
        self.read_locations(cx);
        self.index(None, cx);
    }

    /// How often every location is synced while the app is open.
    pub fn every(&self) -> SyncEvery {
        self.every
    }

    /// Syncs every location `every` so often from now on, the first round that long from now.
    fn set_every(&mut self, every: SyncEvery, cx: &mut Context<Self>) {
        if self.every != every && !self.closed {
            self.every = every;
            if self.started {
                self.schedule(cx);
            }
            cx.notify();
        }
    }

    /// Starts the rounds of [`Self::every`] over, the first that long from now; with only
    /// when asked, there are none.
    fn schedule(&mut self, cx: &mut Context<Self>) {
        let Some(interval) = self.every.interval() else {
            self.next_round = None;
            self._interval = None;
            return;
        };
        let every = interval.as_millis() as i64;
        self.next_round = Some((self.clock)() + every);
        self._interval = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(ROUND_TICK.min(interval))
                    .await;
                let alive = this.update(cx, |this, cx| {
                    let now = (this.clock)();
                    if this.next_round.is_some_and(|due| now >= due) {
                        this.next_round = Some(now + every);
                        this.sync_all(cx);
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
    }

    /// Reads the locations, and syncs them all; with none, looks at this machine instead.
    fn read_locations(&mut self, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        self.run(
            cx,
            move || {
                let catalogue = archive.catalogue();
                let locations = catalogue.locations()?;
                let stale_after = catalogue
                    .setting(locations::STALE_AFTER_KEY)?
                    .and_then(|days| days.parse().ok())
                    .filter(|days| STALE_AFTER_RANGE.contains(days));
                Ok::<_, sb_core::catalogue::CatalogueError>((
                    locations,
                    Details::read(&archive)?,
                    stale_after,
                ))
            },
            |this, read, cx| match read {
                Ok((locations, details, stale_after)) => {
                    let (removed, locations) =
                        locations.into_iter().partition(|it| it.removed.is_some());
                    this.stale_after = stale_after.unwrap_or(STALE_AFTER_DAYS);
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
        if self.passes.contains_key(id) || self.removing.contains(id) || self.closed || self.paused
        {
            return;
        }
        if self.indexes.contains_key(&Some(id.clone())) || self.refetching.contains_key(id) {
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
            let (id, stopped_at) = (id.clone(), reported.clone());
            async move |this, cx| {
                let (synced, location) = job.await;
                this.update(cx, |this, cx| {
                    this.passes.remove(&id);
                    this.attribute_copies(cx);
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
                    /* A stop is said until the next pass ends some other way: then that
                    pass's word stands, a failure or not reaching it among them. */
                    this.stopped.remove(&id);
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
                        /* Something else held the location a moment (a delete for good, an
                        undo): the pass didn't run, and the location is as it was. It waits,
                        and runs once that is likely over. */
                        Err(SyncError::Busy(_)) => {
                            this.waiting.insert(id.clone());
                            this.sync_after(id.clone(), BUSY_RETRY, cx);
                        }
                        /* A stop before the plan was made left nothing that can be counted:
                        the location is as it was before the pass. */
                        Err(SyncError::Cancelled) => {
                            this.errors.remove(&id);
                            let left = *stopped_at.lock().unwrap_or_else(PoisonError::into_inner);
                            let items_left = left.items.saturating_sub(left.items_done);
                            if left.planned && items_left > 0 {
                                this.stopped.insert(
                                    id.clone(),
                                    (items_left, left.bytes.saturating_sub(left.bytes_done)),
                                );
                            }
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
                    crate::system::give_back_memory(cx);
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

    /// Syncs `id` after `delay`, if it is still waiting then: a stop takes it out of waiting.
    fn sync_after(&mut self, id: LocationId, delay: Duration, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| {
                if this.waiting.remove(&id) {
                    this.sync(&id, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Brings the session index up to date outside a pass: of `location`, or of every
    /// location. One of each runs at a time.
    fn index(&mut self, location: Option<LocationId>, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        if self.indexes.contains_key(&location) || self.closed {
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
                    this.attribute_copies(cx);
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

    /// Logs what an index could not do. Returns whether it indexed anything, or made anything
    /// searchable: either is news for the library and search.
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
                report.sessions > 0 || report.searchable > 0
            }
            Err(SyncError::Cancelled) => false,
            Err(error) => {
                log::error!("the session index could not be brought up to date: {error}");
                false
            }
        }
    }

    /// Reads what `work` reports while it runs, and tells (the module docs) when it has
    /// indexed more sessions or made more searchable, and when it has stored more files.
    fn poll(&self, work: Work, reported: Reported, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let mut archived = News::new(SyncEvent::Archived);
            let mut stored = News::new(SyncEvent::Stored);
            loop {
                cx.background_executor().timer(PROGRESS_TICK).await;
                let progress = *reported.lock().unwrap_or_else(PoisonError::into_inner);
                let alive = this.update(cx, |this, cx| {
                    let shown = match &work {
                        Work::Pass(id) => this.passes.get_mut(id).map(|it| &mut it.progress),
                        Work::Index(of) => this.indexes.get_mut(of).map(|it| &mut it.progress),
                    };
                    let Some(shown) = shown else {
                        return;
                    };
                    let (indexed, grew) = (
                        shown.sessions_done != progress.sessions_done
                            || shown.searchable_done != progress.searchable_done,
                        shown.items_done != progress.items_done,
                    );
                    if *shown != progress {
                        *shown = progress;
                        cx.notify();
                    }
                    archived.tell(indexed, cx);
                    stored.tell(grew, cx);
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
        for cancel in self.refetching.values() {
            cancel.cancel();
        }
        self.fetch_all = None;
        self.waiting.clear();
        #[cfg(any(test, feature = "automation"))]
        self.hold.release();
    }

    /// Stops a location's pass at its next step. What it committed stays (PLAN §5).
    pub fn stop(&mut self, id: &LocationId) {
        self.stop_refetch(id);
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

    /// Starts no pass while `paused` (the field's docs).
    pub(super) fn pause(&mut self, paused: bool) {
        self.paused = paused;
    }

    /// Stops everything, and starts nothing more: the app is moving onto another archive
    /// (`super::reopen`).
    pub(super) fn close(&mut self, cx: &mut Context<Self>) {
        self.stop_everything();
        self.closed = true;
        self._interval = None;
        self.next_round = None;
        cx.notify();
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

    /// Lets a pass that is waiting at a hold go on, to wait again once it has stored `until`
    /// items: steps it through a sync.
    #[cfg(any(test, feature = "automation"))]
    pub fn release_until(&mut self, until: usize) {
        self.hold.release_then(Some((until, Held::Pause)));
    }

    /// Whether a pass is waiting at a hold.
    #[cfg(any(test, feature = "automation"))]
    pub fn is_held(&self) -> bool {
        self.hold.state().waiting
    }

    /**
     * Says again where the copies kept on this machine ran (PLAN §7.7), its own job after a
     * pass or an index, so that no location shows syncing while hosts are resolved. One runs
     * at a time; asked meanwhile, another follows it. A change refreshes what shows sessions.
     * It is stopped with the indexes, before it writes, when the app leaves the archive.
     */
    fn attribute_copies(&mut self, cx: &mut Context<Self>) {
        if self.attributing {
            self.attribute_again = true;
            return;
        }
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        self.attributing = true;
        let (resolve, cancel) = (self.resolve.clone(), self.stop_index.clone());
        self.run(
            cx,
            move || attributed(&archive, resolve.as_ref(), &cancel),
            |this, changed, cx| {
                this.attributing = false;
                if changed > 0 {
                    this.read_details(cx);
                    cx.emit(SyncEvent::Archived);
                }
                if std::mem::take(&mut this.attribute_again) {
                    this.attribute_copies(cx);
                }
            },
        );
    }

    /// Runs `work`, which blocks on the archive or on this machine's files, on the background
    /// executor, counted as a job in flight, then `done` with its result. Nothing runs once the
    /// service is closed: the archive is being left, or moved, and isn't to be written.
    fn run<T: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        work: impl FnOnce() -> T + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
    ) {
        if self.closed {
            return;
        }
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

/// What the rounds take for now (`SyncService::clock`).
#[cfg(not(test))]
fn clock(_: &mut Context<SyncService>) -> Rc<dyn Fn() -> i64> {
    Rc::new(format::now)
}

/// What the rounds take for now in tests: gpui's clock, from the wall clock's now.
#[cfg(test)]
fn clock(cx: &mut Context<SyncService>) -> Rc<dyn Fn() -> i64> {
    let executor = cx.background_executor().clone();
    let (start, base) = (executor.now(), format::now());
    Rc::new(move || base + (executor.now() - start).as_millis() as i64)
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
