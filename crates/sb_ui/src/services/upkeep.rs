//! The archive's upkeep (DESIGN §4.13: Settings › Archive): how big the archive is, verify and
//! what the last one found, and the sessions deleted for good.
//!
//! **Reads.** What Settings shows is read in one background job ([`UpkeepService::refresh`]):
//! the archive's size, its last verify with what the archive holds now of each damaged item
//! ([`Problem`]), and the tombstones ([`Deleted`]). It is read when Settings is shown, after a
//! verify, and while Settings shows, whenever sync says the archive changed. Measuring walks
//! the whole archive, so nothing reads it while Settings is not shown.
//!
//! **Verify** runs on a background job with its progress read a few times a second, and
//! stops at the next version when asked; a verify stopped keeps nothing (`sb_core`).
//!
//! **Deleted for good.** Allowing a session to be archived again removes its tombstone at once;
//! the row stays, saying so, with Undo, until Settings is shown again. Undo puts the tombstone
//! back, unless a sync has archived the session again since.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use gpui::{AppContext as _, Context, Entity, Subscription, Task};
use sb_core::CancelToken;
use sb_core::archive::{Archive, ArchiveError, ArchiveStats, Damage, LastVerify, VerifyProgress};
use sb_core::catalogue::{
    AllowedAgain, CatalogueError, Filed, Reason, Session, SessionId, Tombstone,
};
use sb_core::location::Location;

use super::settings::SettingsService;
use super::{ArchiveService, SyncEvent, SyncService};

/// How often a running verify's progress is read.
const PROGRESS_TICK: Duration = Duration::from_millis(100);

/// What can be done for an item verify found damaged (DESIGN §4.13, decision 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fix {
    /// Its newest version is the damaged one and its location still has it: fetch it again.
    FetchAgain,
    /// It was fetched again since, as this version.
    Fetched(u32),
    /// Its location no longer has the damaged bytes: they are gone there, the source changed
    /// since, or the location was removed. The version stays marked, readable as far as it
    /// decodes.
    Damaged,
    /// The item is no longer in the archive: its session was deleted for good.
    Deleted,
}

/// One problem the last verify found, with what the archive holds of it now.
#[derive(Clone, Debug)]
pub struct Problem {
    pub damage: Damage,
    /// The session it is of, as the library shows it.
    pub session: Option<Session>,
    /// The name of the folder that session is in; `None` for Unsorted.
    pub folder: Option<String>,
    pub location: Option<Location>,
    pub fix: Fix,
}

/// A session deleted for good, with where it was from.
#[derive(Clone, Debug)]
pub struct Deleted {
    pub tombstone: Tombstone,
    pub location: Option<Location>,
    /// The owner allowed it to be archived again: what to put back on Undo.
    pub allowed: Option<AllowedAgain>,
    /// Why the owner's last change to it couldn't be made.
    pub refused: Option<String>,
}

/// What [`load`] reads.
struct Loaded {
    stats: ArchiveStats,
    last: Option<LastVerify>,
    problems: Vec<Problem>,
    deleted: Vec<Deleted>,
}

/// Reads what Settings › Archive shows (the module docs).
fn load(archive: &Archive) -> Result<Loaded, String> {
    let catalogue = archive.catalogue();
    let read = || -> Result<_, CatalogueError> {
        let last = catalogue.last_verify()?;
        let folders = catalogue.folders()?;
        let mut problems = Vec::new();
        for damage in last.iter().flat_map(|last| &last.damaged) {
            let location = catalogue.location(&damage.location)?;
            let Some(item) = catalogue.item(&damage.location, &damage.provider, &damage.key)?
            else {
                problems.push(Problem {
                    damage: damage.clone(),
                    session: None,
                    folder: None,
                    location,
                    fix: Fix::Deleted,
                });
                continue;
            };
            let versions = catalogue.versions(item.id)?;
            let refetched = versions
                .iter()
                .find(|it| it.number > damage.version && it.reason == Reason::Refetched);
            let newest = versions.last().map_or(damage.version, |it| it.number);
            let reachable = item.gone_since.is_none()
                && location.as_ref().is_some_and(|it| it.removed.is_none());
            let fix = match refetched {
                Some(version) => Fix::Fetched(version.number),
                None if newest == damage.version && reachable => Fix::FetchAgain,
                None => Fix::Damaged,
            };
            let session = match &item.session {
                Some(key) => catalogue.session(&SessionId {
                    location: damage.location.clone(),
                    provider: damage.provider.clone(),
                    key: key.clone(),
                })?,
                None => None,
            };
            let folder = session.as_ref().and_then(|session| match session.filed {
                Filed::Unsorted => None,
                Filed::ByRule(id) | Filed::ByHand(id) => folders
                    .iter()
                    .find(|folder| folder.id == id)
                    .map(|folder| folder.name.clone()),
            });
            problems.push(Problem {
                damage: damage.clone(),
                session,
                folder,
                location,
                fix,
            });
        }
        let mut deleted = Vec::new();
        for tombstone in catalogue.tombstones()? {
            deleted.push(Deleted {
                location: catalogue.location(&tombstone.session.location)?,
                tombstone,
                allowed: None,
                refused: None,
            });
        }
        Ok((last, problems, deleted))
    };
    let (last, problems, deleted) = read().map_err(|error| error.to_string())?;
    Ok(Loaded {
        stats: archive.stats().map_err(|error| error.to_string())?,
        last,
        problems,
        deleted,
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
    deleted: Vec<Deleted>,
    verifying: Option<Verifying>,
    /// Why the last read, or the last verify, failed.
    failure: Option<String>,
    loading: bool,
    /// A read was asked for while one ran.
    stale: bool,
    /// Writes in flight.
    jobs: usize,
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
            deleted: Vec::new(),
            verifying: None,
            failure: None,
            loading: false,
            stale: false,
            jobs: 0,
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

    pub fn problems(&self) -> &[Problem] {
        &self.problems
    }

    pub fn deleted(&self) -> &[Deleted] {
        &self.deleted
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

    /// Settings is shown: what was allowed before is history, and everything is read again.
    pub fn shown(&mut self, cx: &mut Context<Self>) {
        self.deleted.retain(|deleted| deleted.allowed.is_none());
        self.refresh(cx);
    }

    /// Reads what Settings › Archive shows again, now or once the read running is over.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        if self.loading {
            self.stale = true;
            return;
        }
        self.loading = true;
        let job = cx.background_spawn(async move { load(&archive) });
        cx.spawn(async move |this, cx| {
            let loaded = job.await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match loaded {
                    Ok(loaded) => {
                        this.stats = Some(loaded.stats);
                        this.last = loaded.last;
                        this.problems = loaded.problems;
                        /* The rows the owner allowed stay, with their Undo. */
                        let allowed: Vec<Deleted> = this
                            .deleted
                            .drain(..)
                            .filter(|deleted| deleted.allowed.is_some())
                            .collect();
                        this.deleted = loaded.deleted;
                        this.deleted.extend(allowed);
                        this.deleted
                            .sort_by_key(|deleted| std::cmp::Reverse(deleted.tombstone.deleted));
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

    /// Re-hashes the archive against its catalogue (PLAN §4.4), unless a verify runs.
    pub fn verify(&mut self, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        if self.verifying.is_some() {
            return;
        }
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

    /// Lets the session `id` deleted for good be archived again by the next sync of its
    /// location (PLAN §4.5).
    pub fn allow(&mut self, id: &SessionId, cx: &mut Context<Self>) {
        self.write(id, cx, |archive, deleted| {
            archive
                .catalogue()
                .remove_tombstone(&deleted.tombstone.session)
                .map(Some)
        });
    }

    /// Takes back allowing the session `id` to be archived again, unless a sync has archived it
    /// since.
    pub fn undo_allow(&mut self, id: &SessionId, cx: &mut Context<Self>) {
        self.write(id, cx, |archive, deleted| {
            let Some(allowed) = &deleted.allowed else {
                return Ok(None);
            };
            archive
                .catalogue()
                .restore_tombstone(allowed)
                .map(|()| None)
        });
    }

    /// Writes a change to the row of `id` off the UI thread: `change` returns what the row is
    /// allowed with after it.
    fn write(
        &mut self,
        id: &SessionId,
        cx: &mut Context<Self>,
        change: impl FnOnce(&Archive, &Deleted) -> Result<Option<AllowedAgain>, CatalogueError>
        + Send
        + 'static,
    ) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        let Some(row) = self
            .deleted
            .iter()
            .find(|deleted| deleted.tombstone.session == *id)
            .cloned()
        else {
            return;
        };
        self.jobs += 1;
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
                    match written {
                        Ok(allowed) => {
                            row.allowed = allowed;
                            row.refused = None;
                        }
                        Err(CatalogueError::ArchivedAgain(_)) => {
                            row.refused = Some(
                                "A sync has archived it again already. To remove it again, \
                                 move it to Trash and delete it for good."
                                    .to_string(),
                            );
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
