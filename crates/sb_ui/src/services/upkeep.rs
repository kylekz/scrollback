//! The archive's upkeep (DESIGN §4.13: Settings › Archive): how big the archive is, verify and
//! what the last one found, and the sessions deleted for good.
//!
//! **Reads.** What Settings shows is read in one background job ([`UpkeepService::refresh`]):
//! the archive's size, its last verify with what the archive holds now of the damaged items
//! shown ([`Problem`], [`PROBLEMS_PAGE`] at a time, so a verify that found thousands costs no
//! more than one that found a few), and the tombstones ([`Deleted`]). It is read when Settings
//! is shown, after a verify, and while Settings shows, whenever sync says the archive changed.
//! Measuring walks the whole archive, so nothing reads it while Settings is not shown. The size
//! measured is kept in the app's settings, for the page that says when the archive can't be
//! reached (DESIGN §4.14).
//!
//! **Verify** runs on a background job with its progress read a few times a second, and
//! stops at the next version when asked; a verify stopped keeps nothing (`sb_core`).
//!
//! **Deleted for good.** Allowing a session to be archived again removes its tombstone at once;
//! the row stays, saying so, with Undo, until Settings is shown again. Undo puts the tombstone
//! back, unless a sync has archived the session again since, or its location is syncing now.
//! Once a sync has archived it, the row says so, and offers no Undo. A row takes one change at
//! a time.
//!
//! **Closing** ([`UpkeepService::close`]): when the app moves onto another archive, the
//! verify running stops and nothing more is read, verified or written on this one.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use gpui::{AppContext as _, Context, Entity, Subscription, Task};
use sb_core::CancelToken;
use sb_core::archive::{
    Archive, ArchiveError, ArchiveStats, DamageRecord, LastVerify, VerifyProgress,
};
use sb_core::catalogue::{
    AllowedAgain, CatalogueError, Filed, Folder, Held, Reason, Session, SessionId, Tombstone,
};
use sb_core::location::{Location, LocationId};

use super::settings::SettingsService;
use super::{ArchiveService, SyncEvent, SyncService};

/// How often a running verify's progress is read.
const PROGRESS_TICK: Duration = Duration::from_millis(100);

/// How many of the last verify's problems are shown at first, and how many more each time
/// the owner asks.
pub const PROBLEMS_PAGE: usize = 20;

/// What can be done for an item verify found damaged (DESIGN §4.13, decision 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fix {
    /// Its newest version is the damaged one and its location still has it: fetch it again.
    FetchAgain,
    /// It was fetched again since, as `version`, which may be held back (PLAN §4.2): the owner
    /// chooses which to read.
    Fetched {
        version: u32,
        held: Option<HeldBack>,
    },
    /// Its location no longer has the damaged bytes: they are gone there, the source changed
    /// since, or the location was removed. The version stays marked, readable as far as it
    /// decodes.
    Damaged,
    /// The item is no longer in the archive: its session was deleted for good.
    Deleted,
}

/// Why a version fetched again isn't the one read, and which is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeldBack {
    pub why: Held,
    /// The version read in its place.
    pub reading: u32,
}

/// One problem the last verify found, with what the archive holds of it now.
#[derive(Clone, Debug)]
pub struct Problem {
    pub damage: DamageRecord,
    /// The session it is of, as the library shows it.
    pub session: Option<Session>,
    /// The name of the folder that session is in; `None` for Unsorted.
    pub folder: Option<String>,
    pub location: Option<Location>,
    pub fix: Fix,
}

/// Where a session deleted for good stands now.
#[derive(Clone, Debug)]
pub enum Standing {
    /// Deleted for good: syncs skip it.
    Deleted,
    /// The owner allowed it to be archived again: what to put back on Undo.
    Allowed(AllowedAgain),
    /// A sync has archived it again, at `at` (Unix milliseconds) once that is read.
    ArchivedAgain { at: Option<i64> },
}

/// A session deleted for good, with where it was from.
#[derive(Clone, Debug)]
pub struct Deleted {
    pub tombstone: Tombstone,
    pub location: Option<Location>,
    pub standing: Standing,
    /// A change to it is being written: it takes no other until then.
    pub writing: bool,
    /// Why the owner's last change to it couldn't be made.
    pub refused: Option<String>,
}

/// What [`load`] reads.
struct Loaded {
    stats: ArchiveStats,
    last: Option<LastVerify>,
    problems: Vec<Problem>,
    deleted: Vec<Deleted>,
    /// Of the sessions the owner allowed, those a sync has archived again, with when that
    /// sync of their location started.
    again: Vec<(SessionId, Option<i64>)>,
}

/// What the archive holds now of the damaged item `damage`.
fn problem(
    archive: &Archive,
    damage: &DamageRecord,
    folders: &[Folder],
) -> Result<Problem, CatalogueError> {
    let catalogue = archive.catalogue();
    let location = catalogue.location(&damage.location)?;
    let Some(item) = catalogue.item(&damage.location, &damage.provider, &damage.key)? else {
        return Ok(Problem {
            damage: damage.clone(),
            session: None,
            folder: None,
            location,
            fix: Fix::Deleted,
        });
    };
    let versions = catalogue.versions(item.id)?;
    let refetched = versions
        .iter()
        .find(|it| it.number > damage.version && it.reason == Reason::Refetched);
    let newest = versions.last().map_or(damage.version, |it| it.number);
    let reachable =
        item.gone_since.is_none() && location.as_ref().is_some_and(|it| it.removed.is_none());
    let session = match &item.session {
        Some(key) => catalogue.session(&SessionId {
            location: damage.location.clone(),
            provider: damage.provider.clone(),
            key: key.clone(),
        })?,
        None => None,
    };
    /* The copy read: the one the session reads, for its own transcript, else the newest not
    held back. */
    let reading = session
        .as_ref()
        .filter(|session| session.main_item == Some(item.id))
        .and_then(|session| session.reading_version)
        .or_else(|| {
            versions
                .iter()
                .rev()
                .find(|it| it.held_back.is_none())
                .map(|it| it.number)
        })
        .unwrap_or(damage.version);
    let fix = match refetched {
        Some(version) => Fix::Fetched {
            version: version.number,
            held: version
                .held_back
                .filter(|_| reading != version.number)
                .map(|why| HeldBack { why, reading }),
        },
        None if newest == damage.version && reachable => Fix::FetchAgain,
        None => Fix::Damaged,
    };
    let folder = session.as_ref().and_then(|session| match session.filed {
        Filed::Unsorted => None,
        Filed::ByRule(id) | Filed::ByHand(id) => folders
            .iter()
            .find(|folder| folder.id == id)
            .map(|folder| folder.name.clone()),
    });
    Ok(Problem {
        damage: damage.clone(),
        session,
        folder,
        location,
        fix,
    })
}

/// Reads what Settings › Archive shows (the module docs): the first `shown` problems, and
/// which of the sessions `allowed` are archived again.
fn load(archive: &Archive, shown: usize, allowed: Vec<SessionId>) -> Result<Loaded, String> {
    let catalogue = archive.catalogue();
    let read = || -> Result<_, CatalogueError> {
        let last = catalogue.last_verify()?;
        let folders = catalogue.folders()?;
        let problems = last
            .iter()
            .flat_map(|last| &last.damaged)
            .take(shown)
            .map(|damage| problem(archive, damage, &folders))
            .collect::<Result<Vec<_>, _>>()?;
        let mut deleted = Vec::new();
        for tombstone in catalogue.tombstones()? {
            deleted.push(Deleted {
                location: catalogue.location(&tombstone.session.location)?,
                tombstone,
                standing: Standing::Deleted,
                writing: false,
                refused: None,
            });
        }
        /* When it was archived again: when its transcript's newest version was stored. */
        let mut again = Vec::new();
        for id in allowed {
            if let Some(session) = catalogue.session(&id)? {
                let at = match session.main_item {
                    Some(item) => catalogue
                        .versions(item)?
                        .last()
                        .map(|version| version.started_at),
                    None => None,
                };
                again.push((id, at));
            }
        }
        Ok((last, problems, deleted, again))
    };
    let (last, problems, deleted, again) = read().map_err(|error| error.to_string())?;
    Ok(Loaded {
        stats: archive.stats().map_err(|error| error.to_string())?,
        last,
        problems,
        deleted,
        again,
    })
}

/// A verify while it runs.
pub struct Verifying {
    pub progress: VerifyProgress,
    cancel: CancelToken,
    _poll: Task<()>,
}

pub struct UpkeepService {
    archive: Entity<ArchiveService>,
    settings: Entity<SettingsService>,
    stats: Option<ArchiveStats>,
    last: Option<LastVerify>,
    problems: Vec<Problem>,
    /// How many of the last verify's problems are read and shown.
    shown: usize,
    deleted: Vec<Deleted>,
    verifying: Option<Verifying>,
    /// Why the last read, or the last verify, failed.
    failure: Option<String>,
    loading: bool,
    /// A read was asked for while one ran.
    stale: bool,
    /// Writes in flight.
    jobs: usize,
    /// The app has moved onto another archive: nothing more starts on this one.
    closed: bool,
    /// The app is opening another archive to move onto: the archive isn't read meanwhile
    /// ([`super::pause`]).
    paused: bool,
    _observers: [Subscription; 1],
}

impl UpkeepService {
    pub fn new(
        archive: Entity<ArchiveService>,
        sync: Entity<SyncService>,
        settings: &Entity<SettingsService>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            _observers: [cx.subscribe(&sync, |this, _, _: &SyncEvent, cx| {
                if this.settings.read(cx).is_shown() {
                    this.refresh(cx);
                }
            })],
            archive,
            settings: settings.clone(),
            stats: None,
            last: None,
            problems: Vec::new(),
            shown: PROBLEMS_PAGE,
            deleted: Vec::new(),
            verifying: None,
            failure: None,
            loading: false,
            stale: false,
            jobs: 0,
            closed: false,
            paused: false,
        }
    }

    /// How big the archive is, once it has been measured.
    pub fn stats(&self) -> Option<&ArchiveStats> {
        self.stats.as_ref()
    }

    /// The last verify that ran to its end.
    pub fn last(&self) -> Option<&LastVerify> {
        self.last.as_ref()
    }

    /// The first of the last verify's problems, as many as are shown.
    pub fn problems(&self) -> &[Problem] {
        &self.problems
    }

    /// How many problems the last verify found that aren't shown.
    pub fn more_problems(&self) -> usize {
        self.last.as_ref().map_or(0, |last| {
            last.damaged_total.saturating_sub(self.problems.len())
        })
    }

    /**
     * The sessions deleted for good, once the archive has been read: before that there is
     * nothing to say of them, not even that there are none. They are read with the stats,
     * which are there from the first read that lands.
     */
    pub fn deleted(&self) -> Option<&[Deleted]> {
        self.stats.as_ref().map(|_| self.deleted.as_slice())
    }

    /// Whether a change to one of the location `id`'s sessions deleted for good is being
    /// written.
    pub fn is_writing_at(&self, id: &LocationId) -> bool {
        (self.deleted.iter())
            .any(|deleted| deleted.writing && deleted.tombstone.session.location == *id)
    }

    pub fn verifying(&self) -> Option<&Verifying> {
        self.verifying.as_ref()
    }

    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// Nothing is read, written or verified.
    pub fn is_settled(&self) -> bool {
        !self.loading && self.verifying.is_none() && self.jobs == 0
    }

    /// How many sessions are deleted for good now: not those allowed again since.
    pub fn still_deleted(&self) -> usize {
        self.deleted
            .iter()
            .filter(|deleted| matches!(deleted.standing, Standing::Deleted))
            .count()
    }

    /// Settings is shown: what was allowed before is history, and everything is read again.
    pub fn shown(&mut self, cx: &mut Context<Self>) {
        self.deleted
            .retain(|deleted| matches!(deleted.standing, Standing::Deleted));
        self.shown = PROBLEMS_PAGE;
        self.refresh(cx);
    }

    /// Shows [`PROBLEMS_PAGE`] more of the last verify's problems.
    pub fn show_more_problems(&mut self, cx: &mut Context<Self>) {
        self.shown += PROBLEMS_PAGE;
        self.refresh(cx);
    }

    /// Reads what Settings › Archive shows again, now or once the read running is over.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.closed || self.paused {
            return;
        }
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        if self.loading {
            self.stale = true;
            return;
        }
        self.loading = true;
        let shown = self.shown;
        let id = archive.id().to_string();
        let allowed = self
            .deleted
            .iter()
            .filter(|deleted| !matches!(deleted.standing, Standing::Deleted))
            .map(|deleted| deleted.tombstone.session.clone())
            .collect();
        let job = cx.background_spawn(async move { load(&archive, shown, allowed) });
        cx.spawn(async move |this, cx| {
            let loaded = job.await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match loaded {
                    Ok(loaded) => {
                        let on_disk = loaded.stats.on_disk;
                        this.settings.update(cx, |settings, _| {
                            settings.remember_size(&id, on_disk);
                        });
                        this.stats = Some(loaded.stats);
                        this.last = loaded.last;
                        this.problems = loaded.problems;
                        this.take_deleted(loaded.deleted, loaded.again);
                        this.failure = None;
                    }
                    Err(why) => {
                        log::error!("Settings › Archive could not be read: {why}");
                        this.failure = Some(why);
                    }
                }
                if std::mem::take(&mut this.stale) {
                    this.refresh(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /**
     * Takes the tombstones read as the rows, keeping the rows the owner allowed (which have no
     * tombstone now) and any row being written as it is: one row per session, the latest
     * deleted first.
     */
    fn take_deleted(&mut self, read: Vec<Deleted>, again: Vec<(SessionId, Option<i64>)>) {
        let kept: Vec<Deleted> = self
            .deleted
            .drain(..)
            .filter(|deleted| !matches!(deleted.standing, Standing::Deleted) || deleted.writing)
            .map(|mut deleted| {
                if let Some((_, at)) = again
                    .iter()
                    .find(|(id, _)| *id == deleted.tombstone.session)
                {
                    deleted.standing = Standing::ArchivedAgain { at: *at };
                }
                deleted
            })
            .collect();
        let mut rows: Vec<Deleted> = read
            .into_iter()
            .filter(|row| {
                !kept
                    .iter()
                    .any(|it| it.tombstone.session == row.tombstone.session)
            })
            .collect();
        rows.extend(kept);
        self.deleted = rows;
        self.deleted
            .sort_by_key(|deleted| std::cmp::Reverse(deleted.tombstone.deleted));
    }

    /// Re-hashes the archive against its catalogue (PLAN §4.4), unless a verify runs.
    pub fn verify(&mut self, cx: &mut Context<Self>) {
        if self.closed || self.verifying.is_some() {
            return;
        }
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        let cancel = CancelToken::new();
        let reported = Arc::new(Mutex::new(VerifyProgress::default()));
        let job = cx.background_spawn({
            let (cancel, reported) = (cancel.clone(), reported.clone());
            async move {
                archive.verify_reporting(&cancel, &mut |progress| {
                    *reported.lock().unwrap_or_else(PoisonError::into_inner) = progress;
                })
            }
        });
        let poll = cx.spawn({
            let reported = reported.clone();
            async move |this, cx| {
                loop {
                    cx.background_executor().timer(PROGRESS_TICK).await;
                    let progress = *reported.lock().unwrap_or_else(PoisonError::into_inner);
                    let alive = this.update(cx, |this, cx| {
                        if let Some(verifying) = &mut this.verifying
                            && verifying.progress != progress
                        {
                            verifying.progress = progress;
                            cx.notify();
                        }
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            }
        });
        cx.spawn(async move |this, cx| {
            let verified = job.await;
            this.update(cx, |this, cx| {
                this.verifying = None;
                match verified {
                    Ok(_) | Err(ArchiveError::Cancelled) => {}
                    Err(error) => {
                        log::error!("verify stopped: {error}");
                        this.failure = Some(format!("Verify stopped: {error}."));
                    }
                }
                this.refresh(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        self.verifying = Some(Verifying {
            progress: VerifyProgress::default(),
            cancel,
            _poll: poll,
        });
        cx.notify();
    }

    /// Stops a running verify at its next version. What it found is not kept.
    pub fn stop(&mut self) {
        if let Some(verifying) = &self.verifying {
            verifying.cancel.cancel();
        }
    }

    /// Reads nothing of the archive while `paused` (the field's docs).
    pub(super) fn pause(&mut self, paused: bool) {
        self.paused = paused;
    }

    /// Stops the verify running, and starts nothing more (the module docs).
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.stop();
        self.closed = true;
        cx.notify();
    }

    /// Lets the session `id` deleted for good be archived again by the next sync of its
    /// location (PLAN §4.5).
    pub fn allow(&mut self, id: &SessionId, cx: &mut Context<Self>) {
        self.write(id, cx, |archive, deleted| {
            archive
                .catalogue()
                .remove_tombstone(&deleted.tombstone.session)
                .map(Standing::Allowed)
                .map_err(ArchiveError::from)
        });
    }

    /// Takes back allowing the session `id` to be archived again, unless a sync has archived it
    /// since, or its location is syncing now.
    pub fn undo_allow(&mut self, id: &SessionId, cx: &mut Context<Self>) {
        self.write(id, cx, |archive, deleted| match &deleted.standing {
            Standing::Allowed(allowed) => archive
                .restore_tombstone(allowed)
                .map(|()| Standing::Deleted),
            standing => Ok(standing.clone()),
        });
    }

    /// Writes a change to the row of `id` off the UI thread, unless one is being written:
    /// `change` returns where the row stands after it.
    fn write(
        &mut self,
        id: &SessionId,
        cx: &mut Context<Self>,
        change: impl FnOnce(&Archive, &Deleted) -> Result<Standing, ArchiveError> + Send + 'static,
    ) {
        if self.closed {
            return;
        }
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        let Some(row) = self
            .deleted
            .iter_mut()
            .find(|deleted| deleted.tombstone.session == *id && !deleted.writing)
        else {
            return;
        };
        row.writing = true;
        let (row, place) = (
            row.clone(),
            row.location
                .as_ref()
                .map_or_else(|| "its location".to_string(), |it| it.name.clone()),
        );
        self.jobs += 1;
        cx.notify();
        let job = cx.background_spawn(async move { change(&archive, &row) });
        let id = id.clone();
        cx.spawn(async move |this, cx| {
            let written = job.await;
            this.update(cx, |this, cx| {
                this.jobs -= 1;
                if let Some(row) = this
                    .deleted
                    .iter_mut()
                    .find(|deleted| deleted.tombstone.session == id)
                {
                    row.writing = false;
                    match written {
                        Ok(standing) => {
                            row.standing = standing;
                            row.refused = None;
                        }
                        Err(ArchiveError::Catalogue(CatalogueError::ArchivedAgain(_))) => {
                            /* When, the next read of the archive says. */
                            row.standing = Standing::ArchivedAgain { at: None };
                            row.refused = Some(
                                "To remove it again, move it to Trash and delete it for good."
                                    .to_string(),
                            );
                        }
                        Err(ArchiveError::Syncing(_)) => {
                            row.refused = Some(format!("Try again once the sync of {place} ends."));
                        }
                        Err(error) => {
                            log::warn!("a tombstone couldn't be changed: {error}");
                            row.refused = Some(format!("That couldn't be saved: {error}."));
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

impl Drop for UpkeepService {
    fn drop(&mut self) {
        self.stop();
    }
}
