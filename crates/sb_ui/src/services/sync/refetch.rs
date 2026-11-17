//! Fetch again from its location (PLAN §4.4, DESIGN §4.13): an item verify found damaged, its
//! newest version, fetched again whole from its location as a new version, the damaged one
//! kept and marked (`sb_core::sync::refetch`). It runs on a background job like a pass, one
//! per location at a time, with a cancel of its own; the library hears when it is done.
//!
//! **Beside sync.** A location's pass and its refetch never run together. One asked for while
//! the location syncs is skipped, never failed: the pass fetches the item again itself. So is
//! one asked for while the location fetches another item, or is held a moment by a delete;
//! one asked for while it is being removed fails. A sync asked for while the location
//! refetches waits for it, as one waits for an index. Stopping the location, every location,
//! or removing it stops its refetch too.
//!
//! **Fetch all again** fetches a list of items again one after another, each as above; one
//! that is skipped or fails doesn't stop the rest, and one whose location is still fetching
//! another item (one stopped a moment ago) waits for it. Each item's last result is
//! forgotten as it starts. Stopping it stops the one fetching and drops those after.

use std::collections::VecDeque;

use gpui::Context;
use sb_core::CancelToken;
use sb_core::archive::DamageRecord;
use sb_core::location::LocationId;
use sb_core::provider::ProviderError;
use sb_core::sync::{SyncError, refetch};
use sb_core::transport::TransportError;

use super::{SyncEvent, SyncService};

/// How fetching an item again goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refetch {
    Running,
    /// Stored as this version.
    Done(u32),
    /// Not done, and nothing went wrong: why, for the owner (its location syncs, and its
    /// sync fetches it again; it is busy; or it was stopped).
    Skipped(String),
    /// Why it wasn't, for the owner.
    Failed(String),
}

/// What tells one damaged item from another: its location, its provider and its key.
pub(super) type RefetchKey = (LocationId, String, String);

/// Fetch all again, while it runs.
pub(super) struct FetchAll {
    /// The items still to fetch again.
    queue: VecDeque<DamageRecord>,
    /// How many there were.
    total: usize,
    /// The item fetching now; `None` while the next waits for its location to end a refetch
    /// that isn't this one's.
    current: Option<RefetchKey>,
}

impl SyncService {
    /// How fetching the item `damage` is of again goes, once it was asked for.
    pub fn refetch_of(&self, damage: &DamageRecord) -> Option<&Refetch> {
        self.refetches.get(&key(damage))
    }

    /// Whether `location` is fetching an item again.
    pub fn is_refetching(&self, location: &LocationId) -> bool {
        self.refetching.contains_key(location)
    }

    /// Fetches the item `damage` is of again from its location (the module docs).
    pub fn refetch(&mut self, damage: &DamageRecord, cx: &mut Context<Self>) {
        if self.closed || self.paused || self.waits(cx) {
            return;
        }
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        let Some(location) = self.location(&damage.location).cloned() else {
            return;
        };
        let key = key(damage);
        if self.refetches.get(&key) == Some(&Refetch::Running) {
            return;
        }
        let skipped = if location.removed.is_some() || self.removing.contains(&location.id) {
            Some(Refetch::Failed(format!(
                "{} was removed, and isn't reached any more.",
                location.name
            )))
        } else if self.passes.contains_key(&location.id) {
            Some(Refetch::Skipped(syncing(&location.name)))
        } else if self.refetching.contains_key(&location.id) {
            Some(Refetch::Skipped(busy(&location.name)))
        } else {
            None
        };
        if let Some(skipped) = skipped {
            self.refetches.insert(key, skipped);
            cx.notify();
            return;
        }
        let cancel = CancelToken::new();
        self.refetching.insert(location.id.clone(), cancel.clone());
        self.refetches.insert(key.clone(), Refetch::Running);
        cx.notify();
        let (providers, connect) = (self.providers.clone(), self.connect.clone());
        let (name, kind) = (location.name, location.kind);
        self.run(
            cx,
            move || {
                let (location, provider, item) = &key;
                let fetched = refetch(
                    &archive,
                    location,
                    provider,
                    item,
                    connect(&kind, &cancel).as_ref(),
                    &providers,
                    &cancel,
                );
                let stopped = cancel.is_cancelled();
                (key, fetched, stopped)
            },
            move |this, (key, fetched, stopped), cx| {
                this.refetching.remove(&key.0);
                let done = match fetched {
                    Ok(version) => {
                        cx.emit(SyncEvent::Archived);
                        Refetch::Done(version)
                    }
                    Err(_) if stopped => Refetch::Skipped("Stopped.".to_string()),
                    Err(SyncError::Busy(_)) => Refetch::Skipped(busy(&name)),
                    Err(error) => {
                        log::warn!("{} was not fetched again: {error}", key.2);
                        Refetch::Failed(why(&name, &error))
                    }
                };
                if this.removing.contains(&key.0) {
                    this.write_removal(key.0.clone(), cx);
                } else if this.waiting.remove(&key.0) {
                    this.sync(&key.0, cx);
                }
                /* The item Fetch all again fetched, or the refetch its next item waits for. */
                let next = this
                    .fetch_all
                    .as_ref()
                    .is_some_and(|all| all.current.is_none() || all.current.as_ref() == Some(&key));
                this.refetches.insert(key, done);
                if next {
                    this.fetch_next(cx);
                }
            },
        );
    }

    /// Stops the refetch of `location`, if one runs. Its job ends at its next step.
    pub(super) fn stop_refetch(&mut self, location: &LocationId) {
        if let Some(cancel) = self.refetching.get(location) {
            cancel.cancel();
        }
    }

    /// Fetches each of `damages` again, one after another (the module docs), unless Fetch all
    /// again runs already.
    pub fn refetch_all(&mut self, damages: Vec<DamageRecord>, cx: &mut Context<Self>) {
        if self.closed || self.fetch_all.is_some() || self.waits(cx) {
            return;
        }
        for damage in &damages {
            if self.refetch_of(damage) != Some(&Refetch::Running) {
                self.refetches.remove(&key(damage));
            }
        }
        self.fetch_all = Some(FetchAll {
            total: damages.len(),
            queue: damages.into(),
            current: None,
        });
        self.fetch_next(cx);
    }

    /// While Fetch all again runs: how many of its items are done, and of how many.
    pub fn fetch_all_progress(&self) -> Option<(usize, usize)> {
        self.fetch_all.as_ref().map(|all| {
            (
                all.total - all.queue.len() - usize::from(all.current.is_some()),
                all.total,
            )
        })
    }

    /// Stops Fetch all again: the item fetching now, and those after it.
    pub fn stop_fetch_all(&mut self, cx: &mut Context<Self>) {
        if let Some(FetchAll {
            current: Some((location, ..)),
            ..
        }) = self.fetch_all.take()
        {
            self.stop_refetch(&location);
        }
        cx.notify();
    }

    /// Starts the next item of Fetch all again; one that doesn't start is passed by, saying
    /// why. One whose location is still fetching another item waits for it. With none left,
    /// it is over.
    fn fetch_next(&mut self, cx: &mut Context<Self>) {
        while let Some(all) = self.fetch_all.as_mut() {
            all.current = None;
            let Some(damage) = all.queue.front().cloned() else {
                break;
            };
            if self.refetching.contains_key(&damage.location) {
                return;
            }
            all.queue.pop_front();
            self.refetch(&damage, cx);
            if self.refetch_of(&damage) == Some(&Refetch::Running) {
                if let Some(all) = self.fetch_all.as_mut() {
                    all.current = Some(key(&damage));
                }
                return;
            }
        }
        self.fetch_all = None;
        cx.notify();
    }
}

/// The key of the item `damage` is of.
fn key(damage: &DamageRecord) -> RefetchKey {
    (
        damage.location.clone(),
        damage.provider.clone(),
        damage.key.clone(),
    )
}

/// That `name` is held a moment by something else: a delete, an undo, another item.
fn busy(name: &str) -> String {
    format!("{name} is busy. Try again in a moment.")
}

/// That `name` is syncing, and its sync fetches the item again.
fn syncing(name: &str) -> String {
    format!("{name} is syncing now, and its sync fetches this again.")
}

/// Why fetching an item again from the location `name` failed, for the owner.
fn why(name: &str, error: &SyncError) -> String {
    match error {
        SyncError::Unreachable(_)
        | SyncError::Provider(ProviderError::Transport(TransportError::Unreachable(_))) => {
            format!("{name} can't be reached now. Try again when it is.")
        }
        SyncError::NotAtLocation(_) => format!("{name} no longer has it."),
        SyncError::NotDamaged(_) => "It isn't damaged any more.".to_string(),
        error => format!("It couldn't be fetched again: {error}."),
    }
}
