//! The open archive (PLAN §4.4, §11 `Archive`). It is opened when the app starts, in the
//! folder the app's settings name, and created there on the first run when that is the data
//! dir's (PLAN D3).
//!
//! The data dir's folder, holding no archive, gets one; anything else that keeps the archive
//! from opening (another scrollback has it, its catalogue is gone, the folder can't be
//! reached, a folder the owner chose holds none now) is a failure that the main area shows,
//! and no archive is ever made in the place of one that didn't open: [`Archive::create`]
//! itself refuses a folder that holds what is left of one.
//!
//! Settings › Use a different folder… opens the other archive itself, and the services over
//! it start with it open ([`ArchiveService::opened`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{AppContext as _, Context};
use sb_core::archive::{Archive, ArchiveError};

/// Whether the archive is open.
pub enum ArchiveState {
    Opening,
    Open(Arc<Archive>),
    Failed(Failure),
}

/// Why the archive didn't open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// In the archive's words.
    pub why: String,
    /// Another scrollback has it open.
    pub locked: bool,
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

    /// Over `archive`, open already.
    pub fn opened(archive: Arc<Archive>, _: &mut Context<Self>) -> Self {
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

    /// Tries again to open an archive that failed to.
    pub fn retry(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, ArchiveState::Failed(_)) {
            self.state = ArchiveState::Opening;
            self.open(cx);
            cx.notify();
        }
    }

    fn open(&mut self, cx: &mut Context<Self>) {
        let (dir, create) = (self.dir.clone(), self.create);
        let job = cx.background_spawn(async move {
            match Archive::open(&dir) {
                Err(ArchiveError::NotAnArchive(_)) if create => Archive::create(&dir),
                opened => opened,
            }
        });
        cx.spawn(async move |this, cx| {
            let opened = job.await;
            this.update(cx, |this, cx| {
                this.state = match opened {
                    Ok(archive) => ArchiveState::Open(Arc::new(archive)),
                    Err(error) => {
                        log::error!("the archive didn't open: {error}");
                        ArchiveState::Failed(Failure {
                            why: error.to_string(),
                            locked: matches!(error, ArchiveError::Locked(_)),
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
