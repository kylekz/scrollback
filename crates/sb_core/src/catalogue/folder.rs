//! The owner's folders, the project paths attached to them, and filing (PLAN §4.3, §8). These
//! tables are the owner's work and are only here (PLAN §2.2).
//!
//! [`crate::filing`] decides which attached path claims a session; this is where the rules
//! run. A session that is not filed by hand is always where the rules put it: they run on a
//! session each time it is indexed, and over every indexed session when the attached paths
//! change or a folder goes, by the project path its row has. A session whose row is missing
//! (the index is being rebuilt) stays where it is until it is indexed again, and so does one
//! that an index could not parse: its row still has what the index before found.

use toolbox_store::rusqlite::{self, Connection, OptionalExtension as _, params};

use super::session::{archived, set_state};
use super::{Catalogue, CatalogueError, FOLDER_NAME_CHARS, Result, SessionId};
use crate::filing::{self, AttachedPath, FolderId, NormalPath};
use crate::location::LocationId;

/// One of the owner's folders (PLAN §2.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Folder {
    pub id: FolderId,
    /// The folder it is in; `None` at the top.
    pub parent: Option<FolderId>,
    pub name: String,
}

/// A project path on a folder, with the path as the owner typed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderPath {
    pub attached: AttachedPath,
    pub typed: String,
}

impl Catalogue {
    /// Every folder, those with the same parent together and in their order.
    pub fn folders(&self) -> Result<Vec<Folder>> {
        self.db.read(|c| {
            Ok(c.prepare(
                "SELECT id, parent_id, name FROM folder ORDER BY parent_id, position, id",
            )?
            .query_map([], |row| {
                Ok(Folder {
                    id: FolderId(row.get(0)?),
                    parent: row.get::<_, Option<i64>>(1)?.map(FolderId),
                    name: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Makes a folder in `parent`, or at the top, after the folders already there. Two
    /// folders may have the same name.
    pub fn create_folder(&self, name: &str, parent: Option<FolderId>) -> Result<FolderId> {
        let name = named(name)?;
        self.db.write(|tx| {
            if let Some(parent) = parent {
                parent_of(tx, parent)?;
            }
            let parent = parent.map(|parent| parent.0);
            tx.execute(
                "INSERT INTO folder (parent_id, name, position)
                 SELECT ?1, ?2, COALESCE(max(position) + 1, 0) FROM folder WHERE parent_id IS ?1",
                params![parent, name],
            )?;
            Ok(FolderId(tx.last_insert_rowid()))
        })
    }

    pub fn rename_folder(&self, folder: FolderId, name: &str) -> Result<()> {
        let name = named(name)?;
        self.db.write(|tx| {
            parent_of(tx, folder)?;
            tx.execute(
                "UPDATE folder SET name = ?2 WHERE id = ?1",
                params![folder.0, name],
            )?;
            Ok(())
        })
    }

    /// Moves a folder into `parent`, or to the top, as the one at `position` among the
    /// folders there (counted from 0, without itself; past the end is the end). Nesting and
    /// reordering are both this.
    pub fn move_folder(
        &self,
        folder: FolderId,
        parent: Option<FolderId>,
        position: usize,
    ) -> Result<()> {
        self.db.write(|tx| {
            let from = parent_of(tx, folder)?;
            let mut above = parent;
            while let Some(id) = above {
                if id == folder {
                    return Err(CatalogueError::FolderInItself);
                }
                above = parent_of(tx, id)?.map(FolderId);
            }
            let parent = parent.map(|parent| parent.0);
            let mut siblings = children(tx, parent)?;
            siblings.retain(|id| *id != folder.0);
            siblings.insert(position.min(siblings.len()), folder.0);
            tx.execute(
                "UPDATE folder SET parent_id = ?2 WHERE id = ?1",
                params![folder.0, parent],
            )?;
            order(tx, &siblings)?;
            /* The folders it left close up. */
            Ok(order(tx, &children(tx, from)?)?)
        })
    }

    /// Deletes a folder (PLAN §8): its subfolders move up to its parent, in its place; its
    /// paths are dropped; and its sessions go to Unsorted, those filed by hand too, from where
    /// the rules file them again.
    pub fn delete_folder(&self, folder: FolderId) -> Result<()> {
        self.db.write(|tx| {
            let parent = parent_of(tx, folder)?;
            let inside = children(tx, Some(folder.0))?;
            let siblings: Vec<i64> = children(tx, parent)?
                .into_iter()
                .flat_map(|id| {
                    if id == folder.0 {
                        inside.clone()
                    } else {
                        vec![id]
                    }
                })
                .collect();
            tx.execute(
                "UPDATE folder SET parent_id = ?2 WHERE parent_id = ?1",
                params![folder.0, parent],
            )?;
            for change in [
                "UPDATE session_state SET folder_id = NULL, by_hand = 0 WHERE folder_id = ?1",
                "DELETE FROM folder_path WHERE folder_id = ?1",
                "DELETE FROM folder WHERE id = ?1",
            ] {
                tx.execute(change, [folder.0])?;
            }
            order(tx, &siblings)?;
            Ok(refile(tx)?)
        })
    }

    /// The paths attached to a folder, in the order they were attached.
    pub fn folder_paths(&self, folder: FolderId) -> Result<Vec<FolderPath>> {
        self.db.read(|c| {
            Ok(c.prepare(
                "SELECT path, location_id, typed FROM folder_path WHERE folder_id = ?1
                 ORDER BY rowid",
            )?
            .query_map([folder.0], |row| {
                Ok(FolderPath {
                    attached: AttachedPath {
                        folder,
                        path: row.get(0)?,
                        location: row.get(1)?,
                    },
                    typed: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
        })
    }

    /**
     * Attaches the project path `typed` to a folder, for the sessions of `location` or of
     * every location, and files by it: the sessions in Unsorted and those a rule filed go
     * where the rules now put them (PLAN §8). The white space around the path is not part of
     * it. A path that names no directory, or does not say where it is from the top (`~/code`,
     * `code/app`), is [`CatalogueError::NoPath`].
     *
     * A path is attached once for a scope. A second attach, to any folder, is refused with
     * [`CatalogueError::PathAttached`], which names the folder that has it.
     */
    pub fn attach_path(
        &self,
        folder: FolderId,
        typed: &str,
        location: Option<&LocationId>,
    ) -> Result<()> {
        let typed = typed.trim();
        /* The drive mount is the default one for every location until a location can say its
        own (PLAN §6.3). */
        let path = NormalPath::new(typed, None)
            .filter(NormalPath::is_absolute)
            .ok_or_else(|| CatalogueError::NoPath(typed.to_string()))?;
        self.db.write(|tx| {
            parent_of(tx, folder)?;
            let first = tx
                .query_row(
                    "SELECT f.id, f.name, p.typed FROM folder_path p
                     JOIN folder f ON f.id = p.folder_id
                     WHERE p.path = ?1 AND p.location_id IS ?2",
                    params![path, location],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            if let Some((id, name, typed)) = first {
                return Err(CatalogueError::PathAttached {
                    folder: FolderId(id),
                    name,
                    typed,
                });
            }
            tx.execute(
                "INSERT INTO folder_path (folder_id, path, typed, location_id)
                 VALUES (?1, ?2, ?3, ?4)",
                params![folder.0, path, typed, location],
            )?;
            Ok(refile(tx)?)
        })
    }

    /// Takes an attached path off its folder. The sessions it filed go where the rules now
    /// put them: to a folder with a shorter path that contains theirs, or to Unsorted.
    pub fn detach_path(&self, attached: &AttachedPath) -> Result<()> {
        self.db.write(|tx| {
            tx.execute(
                "DELETE FROM folder_path
                 WHERE folder_id = ?1 AND path = ?2 AND location_id IS ?3",
                params![attached.folder.0, attached.path, attached.location],
            )?;
            Ok(refile(tx)?)
        })
    }

    /// Files a session in a folder by hand. No rule moves it from there (PLAN §8).
    pub fn file_by_hand(&self, session: &SessionId, folder: FolderId) -> Result<()> {
        self.db.write(|tx| {
            archived(tx, session)?;
            parent_of(tx, folder)?;
            set_state(tx, session, &[("folder_id", &folder.0), ("by_hand", &true)])?;
            Ok(())
        })
    }

    /// Hands a session filed by hand back to the rules (PLAN §8), and returns the folder they
    /// file it in, if one's path claims it.
    pub fn file_automatically(&self, session: &SessionId) -> Result<Option<FolderId>> {
        self.db.write(|tx| {
            archived(tx, session)?;
            let project: Option<NormalPath> = tx
                .query_row(
                    "SELECT project FROM session
                     WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
                    params![session.location, session.provider, session.key],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            let folder = claimed(project.as_ref(), &session.location, &attached(tx)?);
            set_state(tx, session, &[("folder_id", &folder), ("by_hand", &false)])?;
            Ok(folder.map(FolderId))
        })
    }
}

/// A folder's name as it is kept: without the white space around it.
fn named(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > FOLDER_NAME_CHARS {
        return Err(CatalogueError::FolderName);
    }
    Ok(name)
}

/// The folder `folder` is in, `None` at the top. Fails when there is no such folder.
fn parent_of(connection: &Connection, folder: FolderId) -> Result<Option<i64>> {
    connection
        .query_row(
            "SELECT parent_id FROM folder WHERE id = ?1",
            [folder.0],
            |row| row.get(0),
        )
        .optional()?
        .ok_or(CatalogueError::NoFolder(folder))
}

/// Runs the rules on the session `id`, started in `project`, unless the owner filed it by
/// hand. Returns whether they moved it into a folder.
pub(super) fn file_by_rule(
    connection: &Connection,
    id: &SessionId,
    project: Option<&NormalPath>,
) -> rusqlite::Result<bool> {
    let (current, by_hand): (Option<i64>, bool) = connection
        .query_row(
            "SELECT folder_id, by_hand FROM session_state
             WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
            params![id.location, id.provider, id.key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .unwrap_or_default();
    if by_hand {
        return Ok(false);
    }
    let folder = claimed(project, &id.location, &attached(connection)?);
    if folder == current {
        return Ok(false);
    }
    set_state(connection, id, &[("folder_id", &folder)])?;
    Ok(folder.is_some())
}

/// Runs the rules over every indexed session that is not filed by hand, each by the project
/// path its row has.
fn refile(connection: &Connection) -> rusqlite::Result<()> {
    let attached = attached(connection)?;
    let filed: Vec<(SessionId, Option<NormalPath>, Option<i64>)> = connection
        .prepare(
            "SELECT s.location_id, s.provider, s.session, s.project, st.folder_id
             FROM session s
             LEFT JOIN session_state st
               ON st.location_id = s.location_id AND st.provider = s.provider
              AND st.session = s.session
             WHERE COALESCE(st.by_hand, 0) = 0",
        )?
        .query_map([], |row| {
            let id = SessionId {
                location: row.get(0)?,
                provider: row.get(1)?,
                key: row.get(2)?,
            };
            Ok((id, row.get(3)?, row.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (id, project, current) in filed {
        let folder = claimed(project.as_ref(), &id.location, &attached);
        if folder != current {
            set_state(connection, &id, &[("folder_id", &folder)])?;
        }
    }
    Ok(())
}

/// The folder whose attached path claims a session started in `project` at `location`.
fn claimed(
    project: Option<&NormalPath>,
    location: &LocationId,
    attached: &[AttachedPath],
) -> Option<i64> {
    filing::file(project?, location, attached).map(|path| path.folder.0)
}

/// Every attached path.
fn attached(connection: &Connection) -> rusqlite::Result<Vec<AttachedPath>> {
    connection
        .prepare("SELECT folder_id, path, location_id FROM folder_path")?
        .query_map([], |row| {
            Ok(AttachedPath {
                folder: FolderId(row.get(0)?),
                path: row.get(1)?,
                location: row.get(2)?,
            })
        })?
        .collect()
}

/// The folders in `parent`, or at the top, in their order.
fn children(connection: &Connection, parent: Option<i64>) -> rusqlite::Result<Vec<i64>> {
    connection
        .prepare("SELECT id FROM folder WHERE parent_id IS ?1 ORDER BY position, id")?
        .query_map([parent], |row| row.get(0))?
        .collect()
}

/// Puts the folders `siblings`, which have one parent, in this order.
fn order(connection: &Connection, siblings: &[i64]) -> rusqlite::Result<()> {
    let mut place = connection.prepare("UPDATE folder SET position = ?2 WHERE id = ?1")?;
    for (position, id) in siblings.iter().enumerate() {
        place.execute(params![id, position])?;
    }
    Ok(())
}
