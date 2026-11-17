//! The owner's changes to several sessions at once (DESIGN §4.4–§4.6: a selection filed,
//! starred or moved to Trash), each in one transaction: all of them happen or none do. Each
//! says how the sessions were before it, read in the same transaction, which is what its Undo
//! puts back.
//!
//! An Undo puts back what it can: a session deleted for good since has nothing to put back
//! ([`Undone::gone`]), and a folder deleted since has no place to put a session back in, which
//! then goes where the rules file it ([`Undone::refiled`]). Both are counted, for the owner to
//! be told.

use toolbox_store::rusqlite::{Connection, OptionalExtension as _, params};

use super::folder::{create, file_by_rule, filing_of, folder_exists, free_name, named, parent_of};
use super::session::{archived, set_state};
use super::trash::trashed_at;
use super::{Catalogue, CatalogueError, Filed, Result, SessionId, now_ms};
use crate::filing::FolderId;

/// How a session was filed before a change.
pub type Before = (SessionId, Filed);

/// What an Undo could not put back as it was.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Undone {
    /// Sessions deleted for good since.
    pub gone: usize,
    /// Sessions whose folder was deleted since, filed by their paths instead.
    pub refiled: usize,
}

/// What taking sessions out of Trash did ([`Catalogue::restore_all`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Restored {
    /// Sessions deleted for good since, which nothing put back.
    pub gone: usize,
    /// Each session taken out of Trash: how it was filed while there, and the folder it is in
    /// now, read in the same transaction.
    pub moved: Vec<(SessionId, Filed, Option<FolderId>)>,
}

impl Undone {
    /// Whether everything was put back as it was.
    pub fn is_whole(&self) -> bool {
        *self == Self::default()
    }
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
                    refile(tx, id)?;
                    let now = match filed(tx, id)? {
                        Filed::ByRule(folder) | Filed::ByHand(folder) => Some(folder),
                        Filed::Unsorted => None,
                    };
                    Ok(((id.clone(), before), now))
                })
                .collect()
        })
    }

    /**
     * Puts sessions back as they were filed: the Undo of filing them. A session deleted for
     * good since is left out, and one whose folder is gone is filed by its path (the module
     * docs).
     */
    pub fn refile_as(&self, before: &[Before]) -> Result<Undone> {
        self.db.write(|tx| {
            let mut undone = Undone::default();
            for (id, was) in before {
                match archived(tx, id) {
                    Err(CatalogueError::NoSession(_)) => {
                        undone.gone += 1;
                        continue;
                    }
                    other => other?,
                }
                match was {
                    Filed::ByHand(folder) if folder_exists(tx, *folder)? => {
                        set_state(tx, id, &[("folder_id", &folder.0), ("by_hand", &true)])?;
                    }
                    Filed::ByHand(_) => {
                        undone.refiled += 1;
                        set_state(tx, id, &[("by_hand", &false)])?;
                        refile(tx, id)?;
                    }
                    Filed::ByRule(_) | Filed::Unsorted => {
                        set_state(tx, id, &[("by_hand", &false)])?;
                        refile(tx, id)?;
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
     * Takes `ids` out of Trash, as [`Catalogue::restore`] does each, in one change: Trash's
     * Restore, and the Undo of moving them there. A session deleted for good since is left out
     * and counted; one taken out already stays where it is. Says where each went.
     */
    pub fn restore_all(&self, ids: &[SessionId]) -> Result<Restored> {
        self.db.write(|tx| {
            let mut restored = Restored::default();
            for id in ids {
                match archived(tx, id) {
                    Err(CatalogueError::NoSession(_)) => restored.gone += 1,
                    other => {
                        other?;
                        if trashed_at(tx, id)?.is_some() {
                            let was = filed(tx, id)?;
                            restore(tx, id)?;
                            let now = match filed(tx, id)? {
                                Filed::ByRule(folder) | Filed::ByHand(folder) => Some(folder),
                                Filed::Unsorted => None,
                            };
                            restored.moved.push((id.clone(), was, now));
                        }
                    }
                }
            }
            Ok(restored)
        })
    }

    /**
     * Makes a folder in `parent`, or at the top, named `name` or, where a folder there has
     * that, the next free "name 2", "name 3" (New folder, Move to folder › New folder…), and
     * files `sessions` in it by hand, in one change.
     */
    pub fn create_folder_with(
        &self,
        name: &str,
        parent: Option<FolderId>,
        sessions: &[SessionId],
    ) -> Result<FolderId> {
        let name = named(name)?;
        self.db.write(|tx| {
            let folder = create(tx, &free_name(tx, &name, parent, None)?, parent)?;
            for id in sessions {
                archived(tx, id)?;
                set_state(tx, id, &[("folder_id", &folder.0), ("by_hand", &true)])?;
            }
            Ok(folder)
        })
    }
}

/**
 * Takes a session in Trash out, and runs the rules on it, which left it alone while it was
 * there: the folder it was in may have lost its path or gone.
 */
pub(super) fn restore(connection: &Connection, id: &SessionId) -> Result<()> {
    set_state(connection, id, &[("trashed_at", &None::<i64>)])?;
    refile(connection, id)?;
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

/// Runs the rules on the session `id`, by the project path and location its row has.
fn refile(connection: &Connection, id: &SessionId) -> Result<()> {
    let (project, ran_at) = filing_of(connection, id)?;
    file_by_rule(connection, id, project.as_ref(), ran_at.as_ref())?;
    Ok(())
}
