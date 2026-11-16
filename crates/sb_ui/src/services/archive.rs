//! The open archive (PLAN §4.4, §11 `Archive`). It is opened when the app starts, in the
//! folder the app's settings name, and created there on the first run when that is the data
//! dir's (PLAN D3).
//!
//! The data dir's folder, holding no archive, gets one; anything else that keeps the archive
//! from opening (another scrollback has it, its catalogue is gone, the folder can't be
//! reached, a folder the owner chose holds none now) is a failure that the main area shows,
//! and no archive is ever made in the place of one that didn't open: [`Archive::create`]
//! itself refuses a folder that holds what is left of one. When the app's settings can't be
//! read, nothing is opened at all ([`ArchiveService::unavailable`]).
//!
//! Each archive that opens is remembered in the app's settings (its id, when, how many
//! sessions), for the page that says when it can't be reached (DESIGN §4.14).
//!
//! Settings › Use a different folder… opens the other archive itself, and the services over
//! it start with it open ([`ArchiveService::opened`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{AppContext as _, Context};
use sb_core::archive::{Archive, ArchiveError};

use super::ActiveServices as _;

/// Whether the archive is open.
pub enum ArchiveState {
    Opening,
    Open(Arc<Archive>),
    Failed(Failure),
}

/// Why the archive didn't open (DESIGN §4.14).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// A folder the owner chose holds no archive now: it isn't there (a drive unplugged, a
    /// letter changed), or it is (`there`) and holds none. None is made in its place.
    Missing { there: bool },
    /// Another scrollback has it open.
    Locked(String),
    /// The app's settings can't be read, so nothing was opened: why, in their words.
    Settings(String),
    /// Anything else, in the archive's words.
    Other(String),
}

impl Failure {
    /// Why, in the words of what failed.
    pub fn why(&self) -> &str {
        match self {
            Self::Missing { there: false } => "the folder isn't there",
            Self::Missing { there: true } => "the folder holds no archive",
            Self::Locked(why) | Self::Settings(why) | Self::Other(why) => why,
        }
    }
}

pub struct ArchiveService {
    dir: PathBuf,
    /// Whether an archive is made in `dir` when it holds none.
    create: bool,
    state: ArchiveState,
}

impl ArchiveService {
    /// Opens the archive in `dir`; with `create`, makes one there when it holds none.
    pub fn new(dir: PathBuf, create: bool, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            dir,
            create,
            state: ArchiveState::Opening,
        };
        this.open(cx);
        this
    }

    /// With no archive open, for the reason `failure` gives, and none opened by itself.
    pub fn unavailable(dir: PathBuf, failure: Failure) -> Self {
        Self {
            dir,
            create: false,
            state: ArchiveState::Failed(failure),
        }
    }

    /// Over `archive`, open already.
    pub fn opened(archive: Arc<Archive>, cx: &mut Context<Self>) -> Self {
        remember(&archive, cx);
        Self {
            dir: archive.dir().to_path_buf(),
            create: false,
            state: ArchiveState::Open(archive),
        }
    }

    /// The archive's folder.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn state(&self) -> &ArchiveState {
        &self.state
    }

    /// The archive, once it is open.
    pub fn archive(&self) -> Option<&Arc<Archive>> {
        match &self.state {
            ArchiveState::Open(archive) => Some(archive),
            _ => None,
        }
    }

    /// Gives up the archive, open no more here: the app has moved onto another
    /// (`super::reopen`), which drops it off the UI thread.
    pub(super) fn give_up(&mut self) -> Option<Arc<Archive>> {
        match std::mem::replace(&mut self.state, ArchiveState::Opening) {
            ArchiveState::Open(archive) => Some(archive),
            _ => None,
        }
    }

    /// Tries again to open an archive that failed to. Not when the settings couldn't be read:
    /// only the owner's choice of an archive settles that.
    pub fn retry(&mut self, cx: &mut Context<Self>) {
        let ArchiveState::Failed(failure) = &self.state else {
            return;
        };
        if !matches!(failure, Failure::Settings(_)) {
            self.state = ArchiveState::Opening;
            self.open(cx);
            cx.notify();
        }
    }

    fn open(&mut self, cx: &mut Context<Self>) {
        let (dir, create) = (self.dir.clone(), self.create);
        let job = cx.background_spawn(async move {
            let opened = match Archive::open(&dir) {
                Err(ArchiveError::NotAnArchive(_)) if create => Archive::create(&dir),
                opened => opened,
            };
            (opened, dir.is_dir())
        });
        cx.spawn(async move |this, cx| {
            let (opened, there) = job.await;
            this.update(cx, |this, cx| {
                this.state = match opened {
                    Ok(archive) => {
                        let archive = Arc::new(archive);
                        remember(&archive, cx);
                        ArchiveState::Open(archive)
                    }
                    Err(error) => {
                        log::error!("the archive didn't open: {error}");
                        ArchiveState::Failed(match error {
                            ArchiveError::NotAnArchive(_) => Failure::Missing { there },
                            ArchiveError::Locked(_) => Failure::Locked(error.to_string()),
                            error => Failure::Other(error.to_string()),
                        })
                    }
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

/// Has the app's settings keep which archive is open and what it holds (the module docs).
fn remember(archive: &Arc<Archive>, cx: &mut Context<ArchiveService>) {
    let job = cx.background_spawn({
        let archive = archive.clone();
        async move { archive.catalogue().totals() }
    });
    let id = archive.id().to_string();
    cx.spawn(async move |_, cx| match job.await {
        Ok(totals) => cx.update(|cx| {
            let settings = cx.services().settings.clone();
            settings.update(cx, |settings, cx| {
                settings.remember(id, totals.sessions, cx)
            });
        }),
        Err(error) => log::warn!("what the archive holds couldn't be read: {error}"),
    })
    .detach();
}
