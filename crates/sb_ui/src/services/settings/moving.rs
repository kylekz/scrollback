//! Move… (PLAN §4.4 "Moving the archive", DESIGN §4.13). The folder is looked at and the
//! archive measured ([`SettingsService::check_move`]); once the owner says yes
//! ([`SettingsService::confirm_move`]) the settings file records the move, and every file is
//! copied while the app goes on (`sb_core::archive::Move::copy`). It finishes in the pause a
//! switch makes (`super::super::switch`): the services over the archive close, the owner's
//! changes wait, the finish runs and, as its last step, the settings file is pointed at the
//! copy. The services then come back over the copy, with the owner's waiting changes and a
//! Verify of it; or, stopped or failed, over the archive as it was, and what the move made is
//! removed. Nothing is ever changed in the old folder, and nothing here offers to delete it:
//! Settings says where it is ([`super::Moved`]).
//!
//! **At the start** a `move` in the settings file is a move the app was closed during, before
//! its last step: the archive is still where it was, the copy is removed if its folder is still
//! marked as the move's (`sb_core::archive::clear_move`), and Settings says so
//! ([`Ended::CutShort`]).

use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
#[cfg(any(test, feature = "automation"))]
use std::sync::{Condvar, MutexGuard};
use std::time::Duration;

use gpui::{AppContext as _, Context, Task};
use sb_core::CancelToken;
use sb_core::archive::{
    Archive, ArchiveError, Cleared, Move, MoveError, MovePlan, MoveProgress, MoveStage, Refusal,
    clear_move, resolved,
};

use super::{Moved, Moving, SettingsService, write_newer};
use crate::format;
#[cfg(any(test, feature = "automation"))]
use crate::services::sync::Held;
use crate::services::{ActiveServices as _, switch};

/// How often a running move's progress is read.
const PROGRESS_TICK: Duration = Duration::from_millis(100);

/// Where Move… stands.
pub enum MoveState {
    /// The folder is being looked at and the archive measured.
    Checking(PathBuf),
    /// The folder can't take the archive: why, in words.
    Refused {
        folder: PathBuf,
        why: String,
    },
    /// What the move copies and needs, until the owner says yes or no.
    Asking(MovePlan),
    Running(Running),
    /// A move cut short at the last run being cleared away, at the start.
    Clearing(Moving),
    Ended(Ended),
}

/// A move under way.
pub struct Running {
    pub plan: MovePlan,
    /// The archive's folder, which it leaves as it is.
    pub from: PathBuf,
    pub progress: MoveProgress,
    /// When it started, in Unix milliseconds.
    pub started: i64,
    cancel: CancelToken,
    _poll: Task<()>,
}

impl Running {
    /// Whether Stop can still stop it: not once the settings are being pointed at the copy.
    pub fn can_stop(&self) -> bool {
        self.progress.stage != MoveStage::Switching
    }

    /// Whether it was asked to stop.
    pub fn is_stopping(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

/// How a move that didn't happen ended. The archive is where it was, as it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ended {
    /// The owner stopped it.
    Stopped {
        from: PathBuf,
        to: PathBuf,
        cleared: Cleared,
    },
    /// It failed, for the reason `why` says.
    Failed {
        from: PathBuf,
        to: PathBuf,
        why: String,
        cleared: Cleared,
    },
    /// The app was closed, or the machine stopped, before its last step.
    CutShort {
        from: PathBuf,
        to: PathBuf,
        cleared: Cleared,
    },
}

impl Ended {
    /// How it ended, in a sentence or two (DESIGN §4.13).
    pub fn words(&self) -> String {
        let cleared = |cleared: &Cleared, to: &Path| match cleared {
            Cleared::Removed => format!("what had been copied to {} was removed.", to.display()),
            Cleared::Partly(_) => format!(
                "some of what had been copied is still in {}; delete it yourself.",
                to.display()
            ),
            Cleared::Left => format!(
                "what had been copied was left in {}; delete it yourself.",
                to.display()
            ),
        };
        match self {
            Self::Stopped {
                from,
                to,
                cleared: what,
            } => format!(
                "The move was stopped. The archive is still in {}, and {}",
                from.display(),
                cleared(what, to)
            ),
            Self::Failed {
                from,
                to,
                why,
                cleared: what,
            } => format!(
                "The move didn't finish: {}. The archive is still in {}, as it was, and {}",
                why.trim_end_matches('.'),
                from.display(),
                cleared(what, to)
            ),
            Self::CutShort {
                from,
                to,
                cleared: what,
            } => format!(
                "The move to {} didn't finish: scrollback closed before it was done. The \
                 archive stayed in {}, as it was, and {}",
                to.display(),
                from.display(),
                cleared(what, to)
            ),
        }
    }
}

/// What a move's copy came to.
enum Copied {
    Ready(Move),
    Ended(MoveError, Cleared),
}

impl SettingsService {
    pub fn move_state(&self) -> Option<&MoveState> {
        self.move_state.as_ref()
    }

    /// Whether a move is being checked, run or cleared away: nothing else is to be started.
    pub fn is_moving(&self) -> bool {
        matches!(
            self.move_state,
            Some(MoveState::Checking(_) | MoveState::Running(_) | MoveState::Clearing(_))
        )
    }

    /// The archive's last move, while the archive is still where it went (DESIGN §4.13): the
    /// old folder is the owner's to delete.
    pub fn moved_here(&self, dir: &Path) -> Option<&Moved> {
        self.settings
            .moved
            .as_ref()
            .filter(|moved| resolved(&moved.to) == resolved(dir))
    }

    /// Looks at `folder` as the place to move the archive to, and measures what goes: then
    /// asks, or says why not.
    pub fn check_move(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        if self.is_switching() {
            return;
        }
        if let Some(why) = self.inside_install(&folder) {
            self.move_state = Some(MoveState::Refused {
                folder,
                why: why.to_string(),
            });
            cx.notify();
            return;
        }
        let Some(archive) = cx.services().archive.read(cx).archive().cloned() else {
            return;
        };
        self.move_state = Some(MoveState::Checking(folder.clone()));
        cx.notify();
        let planned = cx.background_spawn({
            let folder = folder.clone();
            async move {
                archive.plan_move(&folder).map_err(|error| {
                    log::warn!("{} can't take the archive: {error}", folder.display());
                    why_not(&error)
                })
            }
        });
        cx.spawn(async move |this, cx| {
            let planned = planned.await;
            this.update(cx, |this, cx| {
                this.move_state = Some(match planned {
                    Ok(plan) => MoveState::Asking(plan),
                    Err(why) => MoveState::Refused { folder, why },
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The owner said no.
    pub fn cancel_move(&mut self, cx: &mut Context<Self>) {
        if matches!(self.move_state, Some(MoveState::Asking(_))) {
            self.move_state = None;
            cx.notify();
        }
    }

    /// Forgets how the last move ended, or why a folder was refused: the owner has read it.
    pub fn dismiss_move(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.move_state,
            Some(MoveState::Refused { .. } | MoveState::Ended(_))
        ) {
            self.move_state = None;
            cx.notify();
        }
    }

    /// Forgets where the old copy of the archive is (DESIGN §4.13: Dismiss).
    pub fn dismiss_moved(&mut self, cx: &mut Context<Self>) {
        if self.settings.moved.take().is_some() {
            self.changed(cx);
        }
    }

    /// Stops the move at its next file, unless the settings are being pointed at the copy.
    pub fn stop_move(&mut self) {
        if let Some(MoveState::Running(running)) = &self.move_state
            && running.can_stop()
        {
            running.cancel.cancel();
            #[cfg(any(test, feature = "automation"))]
            self.move_hold.release();
        }
    }

    /**
     * Moves the archive as the plan asked about says (the module docs): the move is recorded
     * in the settings file, then the archive's files are copied, then it finishes.
     */
    pub fn confirm_move(&mut self, cx: &mut Context<Self>) {
        let Some(MoveState::Asking(plan)) = self.move_state.take() else {
            return;
        };
        let Some(archive) = cx.services().archive.read(cx).archive().cloned() else {
            return;
        };
        let from = archive.dir().to_path_buf();
        let started = format::now();
        self.settings.moving = Some(Moving {
            id: plan.id.clone(),
            from: from.clone(),
            to: plan.target.clone(),
            started,
        });
        self.generation += 1;
        let (generation, text) = (self.generation, self.settings.to_json());
        let (on_disk, path) = (self.on_disk.clone(), self.path.clone());
        let cancel = CancelToken::new();
        let reported = Arc::new(Mutex::new(MoveProgress::default()));
        #[cfg(any(test, feature = "automation"))]
        let hold = self.move_hold.clone();
        let copied = cx.background_spawn({
            let (archive, plan, cancel, reported) = (
                archive.clone(),
                plan.clone(),
                cancel.clone(),
                reported.clone(),
            );
            async move {
                /* Recorded before anything is made there, so a start after a crash knows. */
                if let Err(error) = write_newer(&on_disk, &path, generation, &text) {
                    return Copied::Ended(MoveError::Commit(error), Cleared::Removed);
                }
                let mut moving = match archive.start_move(&plan) {
                    Ok(moving) => moving,
                    Err(error) => return Copied::Ended(error, Cleared::Removed),
                };
                let copied = moving.copy(&archive, &cancel, &mut |progress| {
                    *reported.lock().unwrap_or_else(PoisonError::into_inner) = *progress;
                    #[cfg(any(test, feature = "automation"))]
                    hold.reached(progress, &cancel);
                });
                match copied {
                    Ok(()) => Copied::Ready(moving),
                    Err(error) => Copied::Ended(error, moving.abandon()),
                }
            }
        });
        self.move_state = Some(MoveState::Running(Running {
            plan,
            from,
            progress: MoveProgress::default(),
            started,
            _poll: self.poll(reported.clone(), cx),
            cancel: cancel.clone(),
        }));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let copied = copied.await;
            this.update(cx, |this, cx| {
                this.written = this.written.max(generation);
                match copied {
                    Copied::Ready(moving) => this.finish_move(moving, archive, reported, cx),
                    Copied::Ended(error, cleared) => this.ended(error, cleared, cx),
                }
            })
            .ok();
        })
        .detach();
    }

    /// Reads the move's progress a few times a second while it runs.
    fn poll(&self, reported: Arc<Mutex<MoveProgress>>, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(PROGRESS_TICK).await;
                let progress = *reported.lock().unwrap_or_else(PoisonError::into_inner);
                let alive = this.update(cx, |this, cx| {
                    if let Some(MoveState::Running(running)) = &mut this.move_state
                        && running.progress != progress
                    {
                        running.progress = progress;
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
    }

    /**
     * The finish, in a switch's pause (the module docs): the settings file is written by it
     * alone until it ends, and its commit is a write of the settings as they then are, newer
     * than any before, naming the copy's folder and saying where the old copy is.
     */
    fn finish_move(
        &mut self,
        mut moving: Move,
        archive: Arc<Archive>,
        reported: Arc<Mutex<MoveProgress>>,
        cx: &mut Context<Self>,
    ) {
        let Some(MoveState::Running(running)) = &self.move_state else {
            return;
        };
        let (cancel, from, to) = (
            running.cancel.clone(),
            running.from.clone(),
            moving.target().to_path_buf(),
        );
        self.holding = true;
        self.generation += 1;
        let generation = self.generation;
        let mut after = self.settings.clone();
        after.archive = (!self.is_default(&to)).then(|| to.clone());
        after.moving = None;
        after.moved = Some(Moved {
            from: from.clone(),
            to: to.clone(),
            at: format::now(),
        });
        let (text, on_disk, path) = (after.to_json(), self.on_disk.clone(), self.path.clone());
        let switching = switch(true, cx, move |_| {
            let finished = moving.finish(
                &archive,
                &cancel,
                &mut |progress| {
                    *reported.lock().unwrap_or_else(PoisonError::into_inner) = *progress
                },
                |_| match write_newer(&on_disk, &path, generation, &text) {
                    Ok(true) => Ok(()),
                    Ok(false) => Err(io::Error::other("newer settings were written meanwhile")),
                    Err(error) => Err(error),
                },
            );
            match finished {
                Ok(moved) => (Arc::new(moved), Ok(())),
                Err(error) => {
                    let cleared = moving.abandon();
                    (archive, Err((error, cleared)))
                }
            }
        });
        cx.spawn(async move |this, cx| {
            let switched = switching.await;
            this.update(cx, |this, cx| {
                this.holding = false;
                match switched {
                    Ok(()) => {
                        this.written = this.written.max(generation);
                        this.settings.archive = after.archive;
                        this.settings.moving = None;
                        this.settings.moved = after.moved;
                        this.move_state = None;
                        this.changed(cx);
                        /* The copy was checked file by file; a verify of it says so in
                        Settings, as one the owner ran would. */
                        let upkeep = cx.services().upkeep.clone();
                        upkeep.update(cx, |upkeep, cx| upkeep.verify(cx));
                    }
                    Err((error, cleared)) => this.ended(error, cleared, cx),
                }
            })
            .ok();
        })
        .detach();
    }

    /// A move that didn't happen: why, and what became of what it made.
    fn ended(&mut self, error: MoveError, cleared: Cleared, cx: &mut Context<Self>) {
        let (from, to) = match self.move_state.take() {
            Some(MoveState::Running(running)) => (running.from, running.plan.target),
            _ => return,
        };
        self.move_state = Some(MoveState::Ended(match error {
            MoveError::Cancelled => Ended::Stopped { from, to, cleared },
            error => {
                log::error!("the archive wasn't moved to {}: {error}", to.display());
                Ended::Failed {
                    from,
                    to,
                    why: why_not(&error),
                    cleared,
                }
            }
        }));
        self.settings.moving = None;
        self.changed(cx);
    }

    /**
     * At the start: a move recorded in the settings file was cut short before its last step
     * (the module docs). Its partial copy is cleared away in the background, and Settings says
     * how that went.
     */
    pub(super) fn recover_move(&mut self, cx: &mut Context<Self>) {
        if self.unreadable.is_some() {
            return;
        }
        let Some(moving) = self.settings.moving.clone() else {
            return;
        };
        let in_use = self
            .settings
            .archive
            .clone()
            .unwrap_or_else(|| self.default_archive());
        /* Never so: the write that names the copy's folder takes the record out. */
        if resolved(&moving.to) == resolved(&in_use) {
            log::warn!("a move into the archive in use was still recorded; forgetting it");
            self.settings.moving = None;
            self.changed(cx);
            return;
        }
        self.move_state = Some(MoveState::Clearing(moving.clone()));
        let cleared = cx.background_spawn({
            let moving = moving.clone();
            async move { clear_move(&moving.to, &moving.id) }
        });
        cx.spawn(async move |this, cx| {
            let cleared = cleared.await;
            this.update(cx, |this, cx| {
                log::warn!(
                    "the move of the archive to {} was cut short; the archive stays in {} ({cleared:?})",
                    moving.to.display(),
                    moving.from.display()
                );
                this.move_state = Some(MoveState::Ended(Ended::CutShort {
                    from: moving.from,
                    to: moving.to,
                    cleared,
                }));
                this.settings.moving = None;
                this.changed(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Makes the next move act on `then` once it has copied `after` files. For the tests and
    /// the scenarios.
    #[cfg(any(test, feature = "automation"))]
    pub fn hold_move_after(&mut self, after: usize, then: Held) {
        self.move_hold.state().after = Some((after, then));
    }

    /// Lets a move waiting at the hold go on, and clears the hold.
    #[cfg(any(test, feature = "automation"))]
    pub fn release_move(&mut self) {
        self.move_hold.release();
    }

    /// Whether a move waits at the hold.
    #[cfg(any(test, feature = "automation"))]
    pub fn is_move_held(&self) -> bool {
        self.move_hold.state().waiting
    }
}

/// Why the archive can't be moved, in the owner's words: never the system's, which stay in the
/// log.
fn why_not(error: &MoveError) -> String {
    match error {
        MoveError::Refused(refusal) => match refusal {
            Refusal::Same => "That is the archive's own folder.".to_string(),
            Refusal::Inside => {
                "That folder is inside the archive. Choose one outside it.".to_string()
            }
            Refusal::Holds => "That folder holds the archive. Choose one outside it.".to_string(),
            Refusal::AnArchive => {
                "That folder holds another archive. Use a different folder… opens it.".to_string()
            }
            Refusal::Nested(other) => format!(
                "That folder is inside the archive in {}, or holds one.",
                other.display()
            ),
            Refusal::NotEmpty => {
                "That folder isn't empty. Choose an empty folder, or make a new one.".to_string()
            }
            Refusal::NotAFolder => "That's a file, not a folder.".to_string(),
            Refusal::Unreadable(_) => "scrollback can't read that folder.".to_string(),
            Refusal::Unwritable(_) => "scrollback can't write to that folder.".to_string(),
            Refusal::NoRoom {
                volume,
                free,
                needed,
            } => format!(
                "{} has {} free, and the archive needs about {}. Free some space there, or \
                 choose another drive.",
                volume_name(volume),
                format::size(*free),
                format::size(*needed)
            ),
        },
        MoveError::Held(path) => format!(
            "{} is held open by another program, an antivirus scan perhaps. Try again in a \
             moment.",
            path.display()
        ),
        MoveError::Io { source, path, .. } if source.kind() == io::ErrorKind::StorageFull => {
            format!("{} is full", volume_name(path))
        }
        MoveError::Io { action, path, .. } => {
            format!("scrollback couldn't {action} {}", path.display())
        }
        MoveError::Mismatch(path) => format!(
            "the copy of {} didn't hold what was read; the drive may be failing",
            path.display()
        ),
        MoveError::CatalogueMismatch(_) => "the catalogue's copy didn't match it".to_string(),
        MoveError::Commit(_) => "scrollback's settings couldn't be saved".to_string(),
        MoveError::Archive(ArchiveError::Locked(_)) => {
            "another scrollback has that folder open".to_string()
        }
        error => format!("{error}"),
    }
}

/**
 * The volume `path` is on, as the owner knows it: its drive (`D:\`) on Windows, else the
 * folder itself, whose volume is less plain to name.
 */
fn volume_name(path: &Path) -> String {
    match path.components().next() {
        Some(Component::Prefix(prefix)) => {
            format!(r"{}\", prefix.as_os_str().to_string_lossy())
        }
        _ => path.display().to_string(),
    }
}

#[cfg(any(test, feature = "automation"))]
#[derive(Default)]
pub(super) struct HoldState {
    /// The move that has copied this many files acts on the hold.
    pub(super) after: Option<(usize, Held)>,
    /// A move is waiting to be released.
    pub(super) waiting: bool,
    /// How many times the hold was released.
    releases: u64,
}

/// A point at which the next move's copy waits or stops, as sync's passes have one, for the
/// tests and the scenarios that need a move in a known state. The shipped app has none.
#[cfg(any(test, feature = "automation"))]
#[derive(Clone, Default)]
pub(super) struct Hold(Arc<(Mutex<HoldState>, Condvar)>);

#[cfg(any(test, feature = "automation"))]
impl Hold {
    pub(super) fn state(&self) -> MutexGuard<'_, HoldState> {
        self.0.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Called by the copy as it reports: acts on the hold once `after` files are copied.
    fn reached(&self, progress: &MoveProgress, cancel: &CancelToken) {
        let mut state = self.state();
        match state.after {
            Some((after, Held::Stop)) if progress.files_done >= after => {
                state.after = None;
                cancel.cancel();
            }
            Some((after, Held::Pause)) if progress.files_done >= after => {
                let releases = state.releases;
                state.waiting = true;
                while state.releases == releases {
                    state = self.0.1.wait(state).unwrap_or_else(PoisonError::into_inner);
                }
            }
            _ => {}
        }
    }

    pub(super) fn release(&self) {
        let mut state = self.state();
        state.after = None;
        state.waiting = false;
        state.releases += 1;
        self.0.1.notify_all();
    }
}
