//! App-wide services (PLAN §11), each a gpui entity that is the only writer of its state,
//! installed once as the [`Services`] global by [`init`] before the first window opens. Views
//! reach them through [`ActiveServices::services`] and observe them.
//!
//! - [`ArchiveService`] (PLAN §11's `Archive`): the open archive, or why it isn't open.
//! - [`SyncService`] (`Sync`): the locations, the passes that sync them and their progress.
//! - [`LibraryService`] (`Library`): the page shown, what is selected on it, and what the
//!   sidebar and the pages read from the catalogue.
//! - [`ReaderService`] (the reader, PLAN §9): the session open in the main area, parsed, and
//!   what of it has been loaded.
//! - [`SettingsService`] (`Settings`): the app's own settings, which outlive the archive the
//!   others are over, and whether Settings is shown.
//! - [`UpkeepService`]: the archive's size, verify and the sessions deleted for good, for
//!   Settings › Archive.
//! - [`SearchService`]: the palette's searches, and its recent sessions and searches.
//! - [`UpdatesService`]: scrollback's own updates, in an installed copy.
//!
//! **Another archive** ([`reopen`]): the services over an archive are made anew over the
//! other one, once nothing runs on the first, and the views follow the new [`Services`]. A
//! move of the archive ([`settings`]'s Move…) finishes in that same pause ([`switch`]).
//!
//! Nothing here blocks the UI thread: the archive is opened, synced and read in
//! `background_spawn` jobs (in tests, gpui's deterministic scheduler), and a service's state
//! changes when a job's result comes back.

pub mod archive;
#[cfg(test)]
mod copy_tests;
pub mod drag;
pub mod library;
#[cfg(test)]
mod location_tests;
#[cfg(test)]
mod move_tests;
#[cfg(test)]
mod owner_tests;
pub mod read_at;
pub mod reader;
#[cfg(test)]
mod scale;
pub mod search;
pub mod settings;
#[cfg(test)]
mod settings_tests;
pub mod sync;
#[cfg(test)]
mod tests;
pub mod updates;
pub mod upkeep;
pub mod view;

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use gpui::{App, AppContext as _, Entity, Global, Task};
use sb_core::CancelToken;
use sb_core::archive::Archive;
use sb_core::location::LocationKind;
use sb_core::provider::Provider;
use sb_core::transport::wsl::Distro;
use sb_core::transport::{Launcher, Probe, ShellTransport, Transport, TransportError};

pub use archive::{ArchiveService, ArchiveState, Failure};
pub use drag::{DragState, Dragged, DropTarget, Verdict};
pub use library::{
    ForGood, Leave, LibraryService, Month, Page, PathChip, SessionsPage, Stats, Toast, TypedPath,
    can_attach, group_key, group_path,
};
pub use read_at::ReadAt;
pub use reader::{ReaderService, ReaderState};
pub use search::{Answer, Answered, SearchService};
pub use settings::{SettingsService, SyncEvery};
#[cfg(any(test, feature = "automation"))]
pub use sync::Held;
pub use sync::{LocationStatus, PassProgress, Reach, SyncEvent, SyncService};
pub use updates::{CheckResult, Off, Updater, UpdatesService};
pub use upkeep::UpkeepService;
pub use view::{DateRange, Facets, GroupSort, PageView, UnsortedView};

/// The providers of the agents scrollback archives, as the services and the views share them.
pub type Providers = Arc<Vec<Box<dyn Provider>>>;

/// The transport that reads a location of a kind, stopped by the token: what
/// [`LocationKind::transport`] gives, or in tests homes of their choosing in place of this
/// machine's, a distro's and a host's.
pub type Connect = Arc<dyn Fn(&LocationKind, &CancelToken) -> Box<dyn Transport> + Send + Sync>;

/// What starts the shell of a place of a kind: [`LocationKind::launcher`], or under the
/// development override the same reading another home.
pub type Launch = Arc<dyn Fn(&LocationKind) -> Option<Launcher> + Send + Sync>;

/// The connection test of the WSL distro or the SSH host a launcher starts a shell in, stopped
/// by the token (DESIGN §4.12): what [`probe`] finds, or in tests a stand-in's answer.
pub type Tester =
    Arc<dyn Fn(&Launcher, &CancelToken) -> Result<Probe, TransportError> + Send + Sync>;

/// WSL's installed distros, without starting any: what `sb_core::transport::wsl::distros`
/// lists, or in tests a stand-in's list.
pub type ListDistros =
    Arc<dyn Fn(&CancelToken) -> Result<Option<Vec<Distro>>, TransportError> + Send + Sync>;

/// The connection test through `launcher` (PLAN §6.4): [`ShellTransport::probe`].
pub fn probe(launcher: &Launcher, cancel: &CancelToken) -> Result<Probe, TransportError> {
    ShellTransport::new(launcher.clone())
        .cancelled_by(cancel)
        .probe()
}

/// What [`init`] needs from the binary.
pub struct ServicesConfig {
    /// The app's settings, read from its data dir ([`settings::load`]): they say where the
    /// archive is (PLAN §4.4).
    pub settings: settings::Loaded,
    /// Light or dark for this run whatever the settings say (`--theme`), if asked for.
    pub mode_override: Option<crate::appearance::Mode>,
    /// The providers of the agents to archive (`sb_core::provider::builtin`).
    pub providers: Vec<Box<dyn Provider>>,
    /// How each kind of location is reached and tested, and how WSL's distros are listed.
    pub reach: Reach,
    /// What looks for scrollback's own updates: an installed copy's, or why there is none.
    pub updater: Updater,
    /// The installer's folder, in an installed copy: no archive may be chosen inside it.
    pub install_root: Option<std::path::PathBuf>,
}

/// The app's services.
pub struct Services {
    pub settings: Entity<SettingsService>,
    pub archive: Entity<ArchiveService>,
    pub sync: Entity<SyncService>,
    pub library: Entity<LibraryService>,
    pub reader: Entity<ReaderService>,
    pub upkeep: Entity<UpkeepService>,
    pub search: Entity<SearchService>,
    /// scrollback's own updates: the app's, kept when the app moves onto another archive.
    pub updates: Entity<UpdatesService>,
    pub providers: Providers,
    /// What the sync service was made with, for the services over another archive.
    reach: Reach,
}

impl Global for Services {}

impl Services {
    /// Nothing is opening, syncing, indexing, loading, being read, being saved or checking for
    /// updates: automation's `settled`.
    pub fn is_settled(&self, cx: &App) -> bool {
        self.is_quiet(cx)
            && self.settings.read(cx).is_settled()
            && !self.updates.read(cx).is_checking()
    }

    /// Nothing runs on the archive: it opened or failed to, and nothing syncs, indexes,
    /// loads, writes, verifies or measures it.
    fn is_quiet(&self, cx: &App) -> bool {
        !matches!(self.archive.read(cx).state(), ArchiveState::Opening)
            && self.sync.read(cx).is_settled()
            && self.library.read(cx).is_settled()
            && self.reader.read(cx).is_settled()
            && self.upkeep.read(cx).is_settled()
            && self.search.read(cx).is_settled()
    }
}

/// Builds the services, which starts opening the archive, and installs them as a global.
pub fn init(config: ServicesConfig, cx: &mut App) {
    let (chosen, default, unreadable) = (
        config.settings.settings.archive.clone(),
        config.settings.data_dir.join(settings::ARCHIVE_FOLDER),
        config.settings.unreadable.clone(),
    );
    let settings = cx.new(|cx| {
        SettingsService::new(
            config.settings,
            config.mode_override,
            config.install_root,
            cx,
        )
    });
    /* An archive is made without asking only in its default place (PLAN D3): a folder the
    owner chose that holds none now is one that can't be reached (PLAN §4.4). Settings that
    can't be read say neither, so nothing is opened. */
    let archive = cx.new(|cx| match (unreadable, chosen) {
        (Some(why), _) => ArchiveService::unavailable(default, Failure::Settings(why)),
        (None, Some(dir)) => ArchiveService::new(dir, false, cx),
        (None, None) => ArchiveService::new(default, true, cx),
    });
    let updates = cx.new(|cx| UpdatesService::new(config.updater, cx));
    let services = build(
        settings,
        updates,
        archive,
        Arc::new(config.providers),
        config.reach,
        cx,
    );
    cx.set_global(services);
}

/// The services over `archive`, with the app's `settings` and `updates`.
fn build(
    settings: Entity<SettingsService>,
    updates: Entity<UpdatesService>,
    archive: Entity<ArchiveService>,
    providers: Providers,
    reach: Reach,
    cx: &mut App,
) -> Services {
    let sync = cx.new(|cx| {
        SyncService::new(
            archive.clone(),
            providers.clone(),
            reach.clone(),
            &settings,
            cx,
        )
    });
    let library = cx.new(|cx| LibraryService::new(archive.clone(), sync.clone(), cx));
    let reader =
        cx.new(|cx| ReaderService::new(archive.clone(), library.clone(), providers.clone(), cx));
    let upkeep = cx.new(|cx| UpkeepService::new(archive.clone(), sync.clone(), &settings, cx));
    let search = cx.new(|cx| SearchService::new(archive.clone(), library.clone(), cx));
    Services {
        settings,
        archive,
        sync,
        library,
        reader,
        upkeep,
        search,
        updates,
        providers,
        reach,
    }
}

/// How often [`reopen`] looks whether anything still runs on the archive it leaves.
const QUIET_TICK: Duration = Duration::from_millis(20);

/**
 * Moves the app onto `archive`, open already (Settings › Use a different folder…, PLAN §4.4):
 * every pass, index, refetch and verify on the archive open now is stopped and nothing new
 * starts on it; once nothing runs on it, the services are made anew over `archive` and
 * installed, which the views follow. The archive left is closed off the UI thread, after its
 * background removal of deleted files (the wait closing it is), and nothing writes to it after.
 * The task ends once it is closed.
 */
pub fn reopen(archive: Arc<Archive>, cx: &mut App) -> Task<()> {
    let switching = switch(false, cx, move |_| (archive, Ok::<(), Infallible>(())));
    cx.spawn(async move |_| {
        let Ok(()) = switching.await;
    })
}

/**
 * What [`reopen`] does, with the archive to move onto decided off the UI thread, once nothing
 * runs on the archive open now: `onto` is given it (if one is open) and returns the archive to
 * make the services over and how it went. A move (Settings › Move…) copies the archive then,
 * and returns the copy, or the archive it left when it failed or was stopped; the switch's
 * outcome is then the move's. With `carry`, the owner's changes made meanwhile wait in the
 * library and are written to whichever archive the services come back over: the archive open
 * now is not to be written once it has been copied. Without, they are written to it before the
 * switch, as it is left behind.
 */
pub(crate) fn switch<E: Send + 'static>(
    carry: bool,
    cx: &mut App,
    onto: impl FnOnce(Option<Arc<Archive>>) -> (Arc<Archive>, Result<(), E>) + Send + 'static,
) -> Task<Result<(), E>> {
    let services = cx.services();
    let (settings, updates, providers, reach) = (
        services.settings.clone(),
        services.updates.clone(),
        services.providers.clone(),
        services.reach.clone(),
    );
    let (sync, upkeep, search, library) = (
        services.sync.clone(),
        services.upkeep.clone(),
        services.search.clone(),
        services.library.clone(),
    );
    let left = services.archive.clone();
    sync.update(cx, |sync, cx| sync.close(cx));
    upkeep.update(cx, |upkeep, cx| upkeep.close(cx));
    search.update(cx, |search, cx| search.close(cx));
    if carry {
        library.update(cx, |library, _| library.hold_changes());
    }
    cx.spawn(async move |cx| {
        while !cx.update(|cx| cx.services().is_quiet(cx)) {
            cx.background_executor().timer(QUIET_TICK).await;
        }
        let open = cx.update(|cx| left.read(cx).archive().cloned());
        let (archive, outcome) = cx.background_spawn(async move { onto(open) }).await;
        /* The service left gives up its archive as the new ones take over, so that nothing
        on the UI thread holds it: its last handle, and the wait for its removals that
        dropping it is, go to a background job. That ends before the switch does, so the
        folder is free to be opened again. */
        let given_up = cx.update(|cx| {
            let carried = carry.then(|| library.update(cx, |library, _| library.take_held()));
            let given_up = left.update(cx, |left, _| left.give_up());
            let archive = cx.new(|cx| ArchiveService::opened(archive, cx));
            let services = build(settings, updates, archive.clone(), providers, reach, cx);
            let library = services.library.clone();
            cx.set_global(services);
            /* The new services start as they do on an archive that has just opened. */
            archive.update(cx, |_, cx| cx.notify());
            if let Some(carried) = carried {
                library.update(cx, |library, cx| library.adopt(carried, cx));
            }
            given_up
        });
        if let Some(given_up) = given_up {
            cx.background_spawn(async move { drop(given_up) }).await;
        }
        outcome
    })
}

/// Holds the work on the archive open now while the app opens another to move onto: no pass,
/// round or refresh starts until the other opens, and the switch closes these services, or
/// it is refused, and they go on.
pub fn pause(paused: bool, cx: &mut App) {
    let services = cx.services();
    let (sync, upkeep) = (services.sync.clone(), services.upkeep.clone());
    sync.update(cx, |sync, _| sync.pause(paused));
    upkeep.update(cx, |upkeep, _| upkeep.pause(paused));
}

/// Access to the installed [`Services`].
pub trait ActiveServices {
    /// The app's services. Panics if [`init`] hasn't run, which is a startup bug.
    fn services(&self) -> &Services;
}

impl ActiveServices for App {
    fn services(&self) -> &Services {
        self.global::<Services>()
    }
}
