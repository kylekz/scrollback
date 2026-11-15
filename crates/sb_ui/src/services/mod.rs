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
pub mod library;
#[cfg(test)]
mod scale;
pub mod sync;
#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, AppContext as _, Entity, Global};
use sb_core::provider::Provider;
use sb_core::transport::Transport;

pub use archive::{ArchiveService, ArchiveState};
pub use library::{
    LibraryService, Month, Page, PathChip, SessionsPage, Stats, can_attach, group_key, group_path,
};
pub use sync::{Held, LocationStatus, PassProgress, SyncEvent, SyncService};

/// The providers of the agents scrollback archives, as the services and the views share them.
pub type Providers = Arc<Vec<Box<dyn Provider>>>;

/// What [`init`] needs from the binary.
pub struct ServicesConfig {
    /// The archive's folder (PLAN §4.4): `archive` in the data directory.
    pub archive_dir: PathBuf,
    /// The providers of the agents to archive (`sb_core::provider::builtin`).
    pub providers: Vec<Box<dyn Provider>>,
    /// This machine as a location: its real files, or a home of a test's choosing.
    pub local: Arc<dyn Transport>,
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
    let sync = cx.new(|cx| SyncService::new(archive.clone(), providers.clone(), config.local, cx));
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
