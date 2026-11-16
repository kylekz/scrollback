//! The owner's changes to several sessions at once (DESIGN §4.4–§4.6: a selection filed,
//! starred or moved to Trash), each in one transaction: all of them happen or none do. Each
//! says how the sessions were before it, read in the same transaction, which is what its Undo
//! puts back.
//!
//! An Undo puts back what it can: a session deleted for good since has nothing to put back,
//! and a folder deleted since has no place to put a session back in, which then goes where the
//! rules file it. [`Undone::skipped`] counts them, for the owner to be told.

use toolbox_store::rusqlite::{Connection, OptionalExtension as _, params};

use super::folder::{file_by_rule, parent_of};
use super::session::{archived, set_state};
use super::trash::trashed_at;
use super::{Catalogue, Filed, Result, SessionId, now_ms};
use crate::filing::{FolderId, NormalPath};

/// How a session was filed before a change.
pub type Before = (SessionId, Filed);

/// What an Undo could not put back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Undone {
    pub skipped: usize,
}

impl Catalogue {
    /// Files `ids` in `folder` by hand (PLAN §8), and says how each was filed before.
    pub fn file_by_hand_all(&self, ids: &[SessionId], folder: FolderId) -> Result<Vec<Before>> {
        self.db.write(|tx| {
            parent_of(tx, folder)?;
            ids.iter()
                .map(|id| {
                    archived(tx, id)?;
                    let before = filed(tx, id)?;
                    set_state(tx, id, &[("folder_id", &folder.0), ("by_hand", &true)])?;
                    Ok((id.clone(), before))
                })
                .collect()
        })
    }

    /**
     * Hands `ids` back to the rules (PLAN §8): each goes where its project path files it, or
     * to Unsorted. Says how each was filed before, and the folder it is in now.
     */
    pub fn file_automatically_all(
        &self,
        ids: &[SessionId],
    ) -> Result<Vec<(Before, Option<FolderId>)>> {
        self.db.write(|tx| {
            ids.iter()
                .map(|id| {
                    archived(tx, id)?;
                    let before = filed(tx, id)?;
                    set_state(tx, id, &[("by_hand", &false)])?;
                    file_by_rule(tx, id, project(tx, id)?.as_ref())?;
                    let now = match filed(tx, id)? {
                        Filed::ByRule(folder) | Filed::ByHand(folder) => Some(folder),
                        Filed::Unsorted => None,
                    };
                    Ok(((id.clone(), before), now))
                })
                .collect()
        })
    }

    /// Puts sessions back as they were filed: the Undo of filing them. See the module docs
    /// for what it can't put back.
    pub fn refile_as(&self, before: &[Before]) -> Result<Undone> {
        self.db.write(|tx| {
            let mut undone = Undone::default();
            for (id, was) in before {
                if archived(tx, id).is_err() {
                    undone.skipped += 1;
                    continue;
                }
                match was {
                    Filed::ByHand(folder) if parent_of(tx, *folder).is_ok() => {
                        set_state(tx, id, &[("folder_id", &folder.0), ("by_hand", &true)])?;
                    }
                    Filed::ByHand(_) => {
                        undone.skipped += 1;
                        set_state(tx, id, &[("by_hand", &false)])?;
                        file_by_rule(tx, id, project(tx, id)?.as_ref())?;
                    }
                    Filed::ByRule(_) | Filed::Unsorted => {
                        set_state(tx, id, &[("by_hand", &false)])?;
                        file_by_rule(tx, id, project(tx, id)?.as_ref())?;
                    }
                }
            }
            Ok(undone)
        })
    }

    /// Stars `ids`, or takes their stars away.
    pub fn set_starred_all(&self, ids: &[SessionId], starred: bool) -> Result<()> {
        self.db.write(|tx| {
            for id in ids {
                archived(tx, id)?;
                set_state(tx, id, &[("starred", &starred)])?;
            }
            Ok(())
        })
    }

    /// Moves `ids` to Trash (PLAN §4.5), and says which of them this moved: those not there
    /// already.
    pub fn trash_all(&self, ids: &[SessionId]) -> Result<Vec<SessionId>> {
        self.db.write(|tx| {
            let mut moved = Vec::new();
            for id in ids {
                archived(tx, id)?;
                if trashed_at(tx, id)?.is_none() {
                    set_state(tx, id, &[("trashed_at", &now_ms())])?;
                    moved.push(id.clone());
                }
            }
            Ok(moved)
        })
    }

    /**
     * Takes `ids` out of Trash, as [`Catalogue::restore`] does each: the Undo of moving them
     * there. A session deleted for good since, or taken out already, is skipped.
     */
    pub fn restore_all(&self, ids: &[SessionId]) -> Result<Undone> {
        self.db.write(|tx| {
            let mut undone = Undone::default();
            for id in ids {
                if archived(tx, id).is_err() || trashed_at(tx, id)?.is_none() {
                    undone.skipped += 1;
                    continue;
                }
                restore(tx, id)?;
            }
            Ok(undone)
        })
    }
}

/// Takes a session in Trash out, and runs the rules on it, which left it alone while it was
/// there: the folder it was in may have lost its path or gone.
pub(super) fn restore(connection: &Connection, id: &SessionId) -> Result<()> {
    set_state(connection, id, &[("trashed_at", &None::<i64>)])?;
    file_by_rule(connection, id, project(connection, id)?.as_ref())?;
    Ok(())
}

/// How the session `id` is filed.
fn filed(connection: &Connection, id: &SessionId) -> Result<Filed> {
    let (folder, by_hand): (Option<i64>, bool) = connection
        .query_row(
            "SELECT folder_id, by_hand FROM session_state
             WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
            params![id.location, id.provider, id.key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .unwrap_or_default();
    Ok(match (folder.map(FolderId), by_hand) {
        (None, _) => Filed::Unsorted,
        (Some(folder), true) => Filed::ByHand(folder),
        (Some(folder), false) => Filed::ByRule(folder),
    })
}

/// The project path the session `id`'s row has, as filing compares paths.
fn project(connection: &Connection, id: &SessionId) -> Result<Option<NormalPath>> {
    Ok(connection
        .query_row(
            "SELECT project FROM session
             WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
            params![id.location, id.provider, id.key],
            |row| row.get(0),
        )
        .optional()?
        .flatten())
}
