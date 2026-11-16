//! App-wide services (PLAN §11), each a gpui entity that is the only writer of its state,
//! installed once as the [`Services`] global by [`init`] before the first window opens. Views
//! reach them through [`ActiveServices::services`] and observe them.
//!
//! - [`ArchiveService`] (PLAN §11's `Archive`): the open archive, or why it isn't open.
//! - [`SyncService`] (`Sync`): the locations, the passes that sync them and their progress.
//! - [`LibraryService`] (`Library`): the page shown, what is selected on it, and what the
//!   sidebar and the pages read from the catalogue.
//!
//! Nothing here blocks the UI thread: the archive is opened, synced and read in
//! `background_spawn` jobs (in tests, gpui's deterministic scheduler), and a service's state
//! changes when a job's result comes back.

pub mod archive;
pub mod drag;
pub mod library;
#[cfg(test)]
mod location_tests;
#[cfg(test)]
mod owner_tests;
#[cfg(test)]
mod scale;
pub mod sync;
#[cfg(test)]
mod tests;
pub mod view;

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, AppContext as _, Entity, Global};
use sb_core::CancelToken;
use sb_core::location::LocationKind;
use sb_core::provider::Provider;
use sb_core::transport::wsl::Distro;
use sb_core::transport::{Launcher, Probe, ShellTransport, Transport, TransportError};

pub use archive::{ArchiveService, ArchiveState};
pub use drag::{DragState, Dragged, DropTarget, Verdict};
pub use library::{
    Leave, LibraryService, Month, Page, PathChip, SessionsPage, Stats, Toast, TypedPath,
    can_attach, group_key, group_path,
};
#[cfg(any(test, feature = "automation"))]
pub use sync::Held;
pub use sync::{LocationStatus, PassProgress, Reach, SyncEvent, SyncService};
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
    /// The archive's folder (PLAN §4.4): `archive` in the data directory.
    pub archive_dir: PathBuf,
    /// The providers of the agents to archive (`sb_core::provider::builtin`).
    pub providers: Vec<Box<dyn Provider>>,
    /// How each kind of location is reached and tested, and how WSL's distros are listed.
    pub reach: Reach,
}

/// The app's services.
pub struct Services {
    pub archive: Entity<ArchiveService>,
    pub sync: Entity<SyncService>,
    pub library: Entity<LibraryService>,
    pub providers: Providers,
}

impl Global for Services {}

impl Services {
    /// Nothing is opening, syncing, indexing or loading: automation's `settled`.
    pub fn is_settled(&self, cx: &App) -> bool {
        !matches!(self.archive.read(cx).state(), ArchiveState::Opening)
            && self.sync.read(cx).is_settled()
            && self.library.read(cx).is_settled()
    }
}

/// Builds the services, which starts opening the archive, and installs them as a global.
pub fn init(config: ServicesConfig, cx: &mut App) {
    let providers: Providers = Arc::new(config.providers);
    let archive = cx.new(|cx| ArchiveService::new(config.archive_dir, cx));
    let sync = cx.new(|cx| SyncService::new(archive.clone(), providers.clone(), config.reach, cx));
    let library = cx.new(|cx| LibraryService::new(archive.clone(), sync.clone(), cx));
    cx.set_global(Services {
        archive,
        sync,
        library,
        providers,
    });
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
