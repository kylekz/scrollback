//! Fetch again from its location (PLAN §4.4, DESIGN §4.13): an item verify found damaged, its
//! newest version, fetched again whole from its location as a new version, the damaged one
//! kept and marked (`sb_core::sync::refetch`). One runs per item at a time, on a background
//! job like a pass; the library hears when it is done. While the location syncs it is not
//! started: the pass fetches the item again itself.

use gpui::Context;
use sb_core::CancelToken;
use sb_core::archive::Damage;
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
    /// Why it wasn't, for the owner.
    Failed(String),
}

impl SyncService {
    /// How fetching the item `damage` is of again goes, once it was asked for.
    pub fn refetch_of(&self, damage: &Damage) -> Option<&Refetch> {
        self.refetches.get(&key(damage))
    }

    /// Fetches the item `damage` is of again from its location (the module docs).
    pub fn refetch(&mut self, damage: &Damage, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        let key = key(damage);
        if self.refetches.get(&key) == Some(&Refetch::Running) {
            return;
        }
        let Some(location) = self.location(&damage.location).cloned() else {
            return;
        };
        let refused = if location.removed.is_some() {
            Some(format!(
                "{} was removed, and isn't reached any more.",
                location.name
            ))
        } else if self.passes.contains_key(&location.id) {
            Some(format!(
                "{} is syncing now, and its sync fetches this again.",
                location.name
            ))
        } else {
            None
        };
        if let Some(why) = refused {
            self.refetches.insert(key, Refetch::Failed(why));
            cx.notify();
            return;
        }
        self.refetches.insert(key.clone(), Refetch::Running);
        cx.notify();
        let (providers, connect) = (self.providers.clone(), self.connect.clone());
        let (name, kind) = (location.name, location.kind);
        self.run(
            cx,
            move || {
                let (location, provider, item) = &key;
                let cancel = CancelToken::new();
                let fetched = refetch(
                    &archive,
                    location,
                    provider,
                    item,
                    connect(&kind, &cancel).as_ref(),
                    &providers,
                    &cancel,
                );
                (key, fetched)
            },
            move |this, (key, fetched), cx| {
                let done = match fetched {
                    Ok(version) => {
                        cx.emit(SyncEvent::Archived);
                        Refetch::Done(version)
                    }
                    Err(error) => {
                        log::warn!("{} was not fetched again: {error}", key.2);
                        Refetch::Failed(why(&name, &error))
                    }
                };
                this.refetches.insert(key, done);
            },
        );
    }
}

/// What tells one damaged item from another: its location, its provider and its key.
fn key(damage: &Damage) -> (LocationId, String, String) {
    (
        damage.location.clone(),
        damage.provider.clone(),
        damage.key.clone(),
    )
}

/// Why fetching an item again from the location `name` failed, for the owner.
fn why(name: &str, error: &SyncError) -> String {
    match error {
        SyncError::Unreachable(_)
        | SyncError::Provider(ProviderError::Transport(TransportError::Unreachable(_))) => {
            format!("{name} can't be reached now. Try again when it is.")
        }
        SyncError::Busy(_) => format!("{name} is syncing now, and its sync fetches this again."),
        SyncError::NotAtLocation(_) => format!("{name} no longer has it."),
        SyncError::NotDamaged(_) => "It isn't damaged any more.".to_string(),
        error => format!("It couldn't be fetched again: {error}."),
    }
}
