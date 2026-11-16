//! App-wide services (PLAN §11), each a gpui entity that is the only writer of its state,
//! installed once as the [`Services`] global by [`init`] before the first window opens. Views
//! reach them through [`ActiveServices::services`] and observe them.
//!
//! - [`ArchiveService`] (PLAN §11's `Archive`): the open archive, or why it isn't open.
//! - [`SyncService`] (`Sync`): the locations, the passes that sync them and their progress.
//! - [`LibraryService`] (`Library`): the page shown, what is selected on it, and what the
//!   sidebar and the pages read from the catalogue.
//! - [`SettingsService`] (`Settings`): the app's own settings, which outlive the archive the
//!   others are over, and whether Settings is shown.
//! - [`UpkeepService`]: the archive's size, verify and the sessions deleted for good, for
//!   Settings › Archive.
//!
//! **Another archive** ([`reopen`]): the services over an archive are made anew over the
//! other one, once nothing runs on the first, and the views follow the new [`Services`].
//!
//! Nothing here blocks the UI thread: the archive is opened, synced and read in
//! `background_spawn` jobs (in tests, gpui's deterministic scheduler), and a service's state
//! changes when a job's result comes back.

pub mod archive;
pub mod library;
#[cfg(test)]
mod location_tests;
#[cfg(test)]
mod owner_tests;
#[cfg(test)]
mod scale;
pub mod settings;
#[cfg(test)]
mod settings_tests;
pub mod sync;
#[cfg(test)]
mod tests;
pub mod upkeep;

use std::sync::Arc;
use std::time::Duration;

use gpui::{App, AppContext as _, Entity, Global, Task};
use sb_core::CancelToken;
use sb_core::archive::Archive;
use sb_core::location::LocationKind;
use sb_core::provider::Provider;
use sb_core::transport::wsl::Distro;
use sb_core::transport::{Launcher, Probe, ShellTransport, Transport, TransportError};

pub use archive::{ArchiveService, ArchiveState};
pub use library::{
    LibraryService, Month, Page, PathChip, SessionsPage, Stats, can_attach, group_key, group_path,
};
pub use settings::{SettingsService, SyncEvery};
#[cfg(any(test, feature = "automation"))]
pub use sync::Held;
pub use sync::{LocationStatus, PassProgress, Reach, SyncEvent, SyncService};
pub use upkeep::UpkeepService;

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
    /// The providers of the agents to archive (`sb_core::provider::builtin`).
    pub providers: Vec<Box<dyn Provider>>,
    /// How each kind of location is reached and tested, and how WSL's distros are listed.
    pub reach: Reach,
}

/// The app's services.
pub struct Services {
    pub settings: Entity<SettingsService>,
    pub archive: Entity<ArchiveService>,
    pub sync: Entity<SyncService>,
    pub library: Entity<LibraryService>,
    pub upkeep: Entity<UpkeepService>,
    pub providers: Providers,
    /// What the sync service was made with, for the services over another archive.
    reach: Reach,
}

impl Global for Services {}

impl Services {
    /// Nothing is opening, syncing, indexing, loading or being saved: automation's `settled`.
    pub fn is_settled(&self, cx: &App) -> bool {
        self.is_quiet(cx) && self.settings.read(cx).is_settled()
    }

    /// Nothing runs on the archive: it opened or failed to, and nothing syncs, indexes,
    /// loads, writes, verifies or measures it.
    fn is_quiet(&self, cx: &App) -> bool {
        !matches!(self.archive.read(cx).state(), ArchiveState::Opening)
            && self.sync.read(cx).is_settled()
            && self.library.read(cx).is_settled()
            && self.upkeep.read(cx).is_settled()
    }
}

/// Builds the services, which starts opening the archive, and installs them as a global.
pub fn init(config: ServicesConfig, cx: &mut App) {
    let (chosen, default) = (
        config.settings.settings.archive.clone(),
        config.settings.data_dir.join(settings::ARCHIVE_FOLDER),
    );
    let settings = cx.new(|cx| SettingsService::new(config.settings, cx));
    /* An archive is made without asking only in its default place (PLAN D3): a folder the
    owner chose that holds none now is one that can't be reached (PLAN §4.4). */
    let archive = cx.new(|cx| match chosen {
        Some(dir) => ArchiveService::new(dir, false, cx),
        None => ArchiveService::new(default, true, cx),
    });
    let services = build(
        settings,
        archive,
        Arc::new(config.providers),
        config.reach,
        cx,
    );
    cx.set_global(services);
}

/// The services over `archive`, with the app's `settings`.
fn build(
    settings: Entity<SettingsService>,
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
    let upkeep = cx.new(|cx| UpkeepService::new(archive.clone(), sync.clone(), &settings, cx));
    Services {
        settings,
        archive,
        sync,
        library,
        upkeep,
        providers,
        reach,
    }
}

/// How often [`reopen`] looks whether anything still runs on the archive it leaves.
const QUIET_TICK: Duration = Duration::from_millis(20);

/**
 * Moves the app onto `archive`, open already (Settings › Use a different folder…, PLAN §4.4):
 * every pass, index and verify on the archive open now is stopped, and once nothing runs on
 * it, the services are made anew over `archive` and installed, which the views follow. The
 * archive left closes as the last of the services over it goes, and nothing writes to it
 * after. The task ends once the new services are in place.
 */
pub fn reopen(archive: Arc<Archive>, cx: &mut App) -> Task<()> {
    let services = cx.services();
    let (settings, providers, reach) = (
        services.settings.clone(),
        services.providers.clone(),
        services.reach.clone(),
    );
    let (sync, upkeep) = (services.sync.clone(), services.upkeep.clone());
    sync.update(cx, |sync, cx| sync.close(cx));
    upkeep.update(cx, |upkeep, _| upkeep.stop());
    cx.spawn(async move |cx| {
        while !cx.update(|cx| cx.services().is_quiet(cx)) {
            cx.background_executor().timer(QUIET_TICK).await;
        }
        cx.update(|cx| {
            let archive = cx.new(|cx| ArchiveService::opened(archive, cx));
            let services = build(settings, archive.clone(), providers, reach, cx);
            cx.set_global(services);
            /* The new services start as they do on an archive that has just opened. */
            archive.update(cx, |_, cx| cx.notify());
        });
    })
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
