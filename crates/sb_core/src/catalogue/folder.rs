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

use super::session::{RAN_AT, SHOWN, archived, set_state};
use super::{Catalogue, CatalogueError, FOLDER_NAME_CHARS, Result, SessionId};
use crate::filing::{self, AttachedPath, Fileable, FolderId, NormalPath};
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

/// A project path sessions at one location recorded, as they recorded it: what the folder
/// paths editor completes from (DESIGN §4.7). Sessions in Trash are not counted, nor those the
/// library doesn't show (PLAN §7.7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectPath {
    /// Where the sessions ran; `None` for copies of sessions that ran elsewhere, which say
    /// what they are as `copy`.
    pub location: Option<LocationId>,
    pub copy: Option<String>,
    pub recorded: String,
    pub project: NormalPath,
    pub sessions: usize,
    /// How many of them are in Unsorted.
    pub unsorted: usize,
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

    /**
     * Makes a folder in `parent`, or at the top, after the folders already there. Two folders
     * in one place don't have the same name, whatever its case ([`CatalogueError::NameTaken`]);
     * in different places they may.
     */
    pub fn create_folder(&self, name: &str, parent: Option<FolderId>) -> Result<FolderId> {
        let name = named(name)?;
        self.db.write(|tx| {
            refuse_taken(tx, &name, parent, None)?;
            create(tx, &name, parent)
        })
    }

    /**
     * Makes a folder with a project path attached to it, as [`Catalogue::create_folder`] and
     * [`Catalogue::attach_path`] do, in one change: a path that is refused leaves no folder.
     * Where a folder in `parent` has the name, it takes the next free one, "app 2" (Unsorted's
     * Create folder from this path, whose name the owner then changes in place).
     */
    pub fn create_folder_with_path(
        &self,
        name: &str,
        parent: Option<FolderId>,
        typed: &str,
        location: Option<&LocationId>,
    ) -> Result<FolderId> {
        let name = named(name)?;
        let (typed, path) = self.typed_path(typed, location)?;
        self.db.write(|tx| {
            let folder = create(tx, &free_name(tx, &name, parent, None)?, parent)?;
            attach(tx, folder, typed, &path, location)?;
            refile(tx)?;
            Ok(folder)
        })
    }

    /// Renames a folder; refused as [`Catalogue::create_folder`] refuses a name.
    pub fn rename_folder(&self, folder: FolderId, name: &str) -> Result<()> {
        let name = named(name)?;
        self.db.write(|tx| {
            let parent = parent_of(tx, folder)?.map(FolderId);
            refuse_taken(tx, &name, parent, Some(folder))?;
            tx.execute(
                "UPDATE folder SET name = ?2 WHERE id = ?1",
                params![folder.0, name],
            )?;
            Ok(())
        })
    }

    /**
     * Moves a folder into `parent`, or to the top, as the one at `position` among the folders
     * there (counted from 0, without itself; past the end is the end). Nesting and reordering
     * are both this. A move into a place where a folder has its name is refused
     * ([`CatalogueError::NameTaken`]); reordering among the folders it is with already is not.
     */
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
            if from != parent.map(|parent| parent.0) {
                refuse_taken(tx, &name_of(tx, folder)?, parent, Some(folder))?;
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

    /**
     * Deletes a folder (PLAN §8): its subfolders move up to its parent, in its place; its paths
     * are dropped; and its sessions go to Unsorted, those filed by hand too, from where the
     * rules file them again. A subfolder whose name a folder there has takes the next free one,
     * "work 2"; says which were renamed, from what to what.
     */
    pub fn delete_folder(&self, folder: FolderId) -> Result<Vec<(String, String)>> {
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
            let mut renamed = Vec::new();
            for child in inside.into_iter().map(FolderId) {
                let name = name_of(tx, child)?;
                let free = free_name(tx, &name, parent.map(FolderId), Some(child))?;
                if free != name {
                    tx.execute(
                        "UPDATE folder SET name = ?2 WHERE id = ?1",
                        params![child.0, free],
                    )?;
                    renamed.push((name, free));
                }
            }
            refile(tx)?;
            Ok(renamed)
        })
    }

    /// The paths attached to a folder, in the order they were attached.
    pub fn folder_paths(&self, folder: FolderId) -> Result<Vec<FolderPath>> {
        self.db.read(|c| {
            parent_of(c, folder)?;
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
     * it, nor is a pair of double quotes around it, which a path copied from a file manager
     * comes in. A path that names no directory, or does not say where it is from the top
     * (`~/code`, `code/app`), is [`CatalogueError::NoPath`].
     *
     * A path is attached once for a scope: a second attach for the same location, or for
     * any where it is attached for any, to any folder, is refused with
     * [`CatalogueError::PathAttached`], which names the folder that has it. One folder may
     * have it for two locations, for the same project on two machines, but not for one
     * location and for any: those overlap.
     */
    pub fn attach_path(
        &self,
        folder: FolderId,
        typed: &str,
        location: Option<&LocationId>,
    ) -> Result<()> {
        let (typed, path) = self.typed_path(typed, location)?;
        self.db.write(|tx| {
            attach(tx, folder, typed, &path, location)?;
            Ok(refile(tx)?)
        })
    }

    /// Takes an attached path off its folder. The sessions it filed go where the rules now
    /// put them: to a folder with a shorter path that contains theirs, or to Unsorted.
    pub fn detach_path(&self, attached: &AttachedPath) -> Result<()> {
        self.db.write(|tx| {
            detach(tx, attached)?;
            Ok(refile(tx)?)
        })
    }

    /**
     * Changes a folder's paths in one change (DESIGN §4.7's Save): takes `detach` off it, then
     * attaches each of `attach`, each a path as the owner typed it and the one location it is
     * for, and files by the paths it ends with. A path that is refused, as
     * [`Catalogue::attach_path`] refuses one, or one to detach that the folder doesn't have,
     * leaves every path as it was.
     */
    pub fn change_paths(
        &self,
        folder: FolderId,
        detach: &[(String, Option<LocationId>)],
        attach: &[(String, Option<LocationId>)],
    ) -> Result<()> {
        let attach = attach
            .iter()
            .map(|(typed, location)| {
                let (typed, path) = self.typed_path(typed, location.as_ref())?;
                Ok((typed, path, location.as_ref()))
            })
            .collect::<Result<Vec<_>>>()?;
        self.db.write(|tx| {
            parent_of(tx, folder)?;
            for (typed, location) in detach {
                let attached = typed_attached(tx, folder, typed, location.as_ref())?;
                self::detach(tx, &attached)?;
            }
            for (typed, path, location) in &attach {
                self::attach(tx, folder, typed, path, *location)?;
            }
            Ok(refile(tx)?)
        })
    }

    /**
     * Moves the path `typed` of `from`, for `location` or for any, to the folder `to`, as it
     * was typed and for the same locations, and files by it there (DESIGN §4.4: a path chip
     * dropped on another folder).
     */
    pub fn move_path(
        &self,
        from: FolderId,
        (typed, location): (&str, Option<&LocationId>),
        to: FolderId,
    ) -> Result<()> {
        self.db.write(|tx| {
            parent_of(tx, to)?;
            let attached = typed_attached(tx, from, typed, location)?;
            detach(tx, &attached)?;
            attach(tx, to, filing::as_typed(typed), &attached.path, location)?;
            Ok(refile(tx)?)
        })
    }

    /// Every folder's attached paths, each folder's in the order they were attached.
    pub fn attached_paths(&self) -> Result<Vec<FolderPath>> {
        self.db.read(|c| {
            Ok(c.prepare(
                "SELECT folder_id, path, location_id, typed FROM folder_path
                 ORDER BY folder_id, rowid",
            )?
            .query_map([], |row| {
                Ok(FolderPath {
                    attached: AttachedPath {
                        folder: FolderId(row.get(0)?),
                        path: row.get(1)?,
                        location: row.get(2)?,
                    },
                    typed: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// The sessions out of Trash that the rules may move and the library shows: what a change
    /// of paths refiles, as the paths editor counts it. Hidden copies are refiled too
    /// ([`refile`]), and count for nothing there.
    pub fn fileable(&self) -> Result<Vec<Fileable>> {
        self.db.read(|c| {
            Ok(c.prepare(&format!(
                "SELECT s.project, {RAN_AT}, st.folder_id FROM session s
                     LEFT JOIN session_state st
                       ON st.location_id = s.location_id AND st.provider = s.provider
                      AND st.session = s.session
                     WHERE s.project IS NOT NULL AND COALESCE(st.by_hand, 0) = 0
                       AND st.trashed_at IS NULL AND {SHOWN}"
            ))?
            .query_map([], |row| {
                Ok(Fileable {
                    project: row.get(0)?,
                    location: row.get(1)?,
                    folder: row.get::<_, Option<i64>>(2)?.map(FolderId),
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Every project path a location's sessions recorded that a folder can take, with how
    /// many sessions recorded it and how many of those are in Unsorted, the most first.
    pub fn project_paths(&self) -> Result<Vec<ProjectPath>> {
        let paths: Vec<ProjectPath> = self.db.read(|c| -> Result<_> {
            Ok(c.prepare(&format!(
                "SELECT {RAN_AT} AS ran_at, s.copy_label, s.project_path, s.project,
                        count(*), COALESCE(sum(st.folder_id IS NULL), 0)
                 FROM session s
                 LEFT JOIN session_state st
                   ON st.location_id = s.location_id AND st.provider = s.provider
                  AND st.session = s.session
                 WHERE s.project IS NOT NULL AND s.project_path IS NOT NULL
                   AND st.trashed_at IS NULL AND {SHOWN}
                 GROUP BY ran_at, s.copy_label, s.project_path
                 ORDER BY count(*) DESC, s.project_path"
            ))?
            .query_map([], |row| {
                Ok(ProjectPath {
                    location: row.get(0)?,
                    copy: row.get(1)?,
                    recorded: row.get(2)?,
                    project: row.get(3)?,
                    sessions: row.get(4)?,
                    unsorted: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
        })?;
        Ok(paths
            .into_iter()
            .filter(|path| path.project.is_absolute())
            .collect())
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
            let (project, ran_at) = filing_of(tx, session)?;
            let folder = claimed(project.as_ref(), ran_at.as_ref(), &attached(tx)?);
            set_state(tx, session, &[("folder_id", &folder), ("by_hand", &false)])?;
            Ok(folder.map(FolderId))
        })
    }

    /**
     * A project path as it is kept, and as filing compares it: without the white space around
     * it and the double quotes a path copied from a file manager comes in, and for one location
     * read under that location's drive mount, where it has one (PLAN §6.3). Refused when it
     * names no directory or does not say where it is from the top.
     */
    fn typed_path<'a>(
        &self,
        typed: &'a str,
        location: Option<&LocationId>,
    ) -> Result<(&'a str, NormalPath)> {
        let typed = filing::as_typed(typed);
        let mount = match location {
            Some(location) => self.drive_mount(location)?,
            None => None,
        };
        let path = NormalPath::new(typed, mount.as_deref())
            .filter(NormalPath::is_absolute)
            .ok_or_else(|| CatalogueError::NoPath(typed.to_string()))?;
        Ok((typed, path))
    }
}

/**
 * Makes the folder `name` in `parent`, or at the top, after the folders already there. Its
 * caller has made sure the name is free there.
 */
pub(super) fn create(
    connection: &Connection,
    name: &str,
    parent: Option<FolderId>,
) -> Result<FolderId> {
    if let Some(parent) = parent {
        parent_of(connection, parent)?;
    }
    let parent = parent.map(|parent| parent.0);
    connection.execute(
        "INSERT INTO folder (parent_id, name, position)
         SELECT ?1, ?2, COALESCE(max(position) + 1, 0) FROM folder WHERE parent_id IS ?1",
        params![parent, name],
    )?;
    Ok(FolderId(connection.last_insert_rowid()))
}

/**
 * Attaches `path`, typed as `typed`, to a folder for `location` or for every location, without
 * filing by it. Refused when the path is attached already for that scope, or to this folder for
 * a scope that overlaps it ([`Catalogue::attach_path`]).
 */
fn attach(
    connection: &Connection,
    folder: FolderId,
    typed: &str,
    path: &NormalPath,
    location: Option<&LocationId>,
) -> Result<()> {
    parent_of(connection, folder)?;
    let first = connection
        .query_row(
            "SELECT f.id, f.name, p.typed FROM folder_path p
             JOIN folder f ON f.id = p.folder_id
             WHERE p.path = ?1
               AND (p.location_id IS ?2
                    OR (p.folder_id = ?3 AND (p.location_id IS NULL OR ?2 IS NULL)))
             ORDER BY p.location_id IS ?2 DESC",
            params![path, location, folder.0],
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
    connection.execute(
        "INSERT INTO folder_path (folder_id, path, typed, location_id)
         VALUES (?1, ?2, ?3, ?4)",
        params![folder.0, path, typed, location],
    )?;
    Ok(())
}

/// The path of `folder` typed as `typed`, for `location` or for any.
fn typed_attached(
    connection: &Connection,
    folder: FolderId,
    typed: &str,
    location: Option<&LocationId>,
) -> Result<AttachedPath> {
    let typed = filing::as_typed(typed);
    parent_of(connection, folder)?;
    let path: NormalPath = connection
        .query_row(
            "SELECT path FROM folder_path
             WHERE folder_id = ?1 AND typed = ?2 AND location_id IS ?3",
            params![folder.0, typed, location],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| CatalogueError::NotAttached(typed.to_string()))?;
    Ok(AttachedPath {
        folder,
        path,
        location: location.cloned(),
    })
}

/// Takes an attached path off its folder, without filing by what is left.
fn detach(connection: &Connection, attached: &AttachedPath) -> Result<()> {
    parent_of(connection, attached.folder)?;
    let detached = connection.execute(
        "DELETE FROM folder_path WHERE folder_id = ?1 AND path = ?2 AND location_id IS ?3",
        params![attached.folder.0, attached.path, attached.location],
    )?;
    if detached == 0 {
        return Err(CatalogueError::NotAttached(
            attached.path.as_str().to_string(),
        ));
    }
    Ok(())
}

/**
 * The names of the folders in `parent`, or at the top, but `except`'s, in lower case: what a
 * name is compared with there. Two folders in one place don't share a name, whatever its case.
 */
fn names_in(
    connection: &Connection,
    parent: Option<FolderId>,
    except: Option<FolderId>,
) -> Result<Vec<String>> {
    Ok(connection
        .prepare("SELECT id, name FROM folder WHERE parent_id IS ?1")?
        .query_map([parent.map(|parent| parent.0)], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter(|(id, _)| Some(FolderId(*id)) != except)
        .map(|(_, name)| name.to_lowercase())
        .collect())
}

/// Refuses `name` in `parent` when a folder there other than `except` has it.
fn refuse_taken(
    connection: &Connection,
    name: &str,
    parent: Option<FolderId>,
    except: Option<FolderId>,
) -> Result<()> {
    if names_in(connection, parent, except)?.contains(&name.to_lowercase()) {
        return Err(CatalogueError::NameTaken(name.to_string()));
    }
    Ok(())
}

/**
 * `name`, or where a folder in `parent` other than `except` has it, the first of "name 2",
 * "name 3" and on that none has, cut to [`FOLDER_NAME_CHARS`].
 */
pub(super) fn free_name(
    connection: &Connection,
    name: &str,
    parent: Option<FolderId>,
    except: Option<FolderId>,
) -> Result<String> {
    let taken = names_in(connection, parent, except)?;
    Ok((1..)
        .map(|n| match n {
            1 => name.to_string(),
            n => {
                let suffix = format!(" {n}");
                let kept = FOLDER_NAME_CHARS - suffix.chars().count();
                format!("{}{suffix}", name.chars().take(kept).collect::<String>())
            }
        })
        .find(|it| !taken.contains(&it.to_lowercase()))
        .unwrap_or_default())
}

/// The name of `folder`.
fn name_of(connection: &Connection, folder: FolderId) -> Result<String> {
    connection
        .query_row("SELECT name FROM folder WHERE id = ?1", [folder.0], |row| {
            row.get(0)
        })
        .optional()?
        .ok_or(CatalogueError::NoFolder(folder))
}

/// Whether there is a folder `folder`.
pub(super) fn folder_exists(connection: &Connection, folder: FolderId) -> Result<bool> {
    match parent_of(connection, folder) {
        Ok(_) => Ok(true),
        Err(CatalogueError::NoFolder(_)) => Ok(false),
        Err(error) => Err(error),
    }
}

/**
 * A folder's name as it is kept: without control characters or the invisible ones that format
 * text ([`crate::location::visible`]: a folder may be named after a transcript's project
 * path), and without the white space around it.
 */
pub(super) fn named(name: &str) -> Result<String> {
    let name = crate::location::visible(name).trim().to_string();
    if name.is_empty() || name.chars().count() > FOLDER_NAME_CHARS {
        return Err(CatalogueError::FolderName);
    }
    Ok(name)
}

/// The folder `folder` is in, `None` at the top. Fails when there is no such folder.
pub(super) fn parent_of(connection: &Connection, folder: FolderId) -> Result<Option<i64>> {
    connection
        .query_row(
            "SELECT parent_id FROM folder WHERE id = ?1",
            [folder.0],
            |row| row.get(0),
        )
        .optional()?
        .ok_or(CatalogueError::NoFolder(folder))
}

/// Runs the rules on the session `id`, started in `project` at `ran_at` ([`super::Session::ran_at`]),
/// unless the owner filed it by hand. Returns whether they moved it into a folder.
pub(super) fn file_by_rule(
    connection: &Connection,
    id: &SessionId,
    project: Option<&NormalPath>,
    ran_at: Option<&LocationId>,
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
    let folder = claimed(project, ran_at, &attached(connection)?);
    if folder == current {
        return Ok(false);
    }
    set_state(connection, id, &[("folder_id", &folder)])?;
    Ok(folder.is_some())
}

/**
 * Runs the rules over every indexed session that is not filed by hand nor in Trash, each by
 * the project path its row has. A session in Trash is filed when it is restored.
 */
fn refile(connection: &Connection) -> rusqlite::Result<()> {
    let attached = attached(connection)?;
    type Filed = (
        SessionId,
        Option<NormalPath>,
        Option<LocationId>,
        Option<i64>,
    );
    let filed: Vec<Filed> = connection
        .prepare(&format!(
            "SELECT s.location_id, s.provider, s.session, s.project, {RAN_AT}, st.folder_id
             FROM session s
             LEFT JOIN session_state st
               ON st.location_id = s.location_id AND st.provider = s.provider
              AND st.session = s.session
             WHERE COALESCE(st.by_hand, 0) = 0 AND st.trashed_at IS NULL"
        ))?
        .query_map([], |row| {
            let id = SessionId {
                location: row.get(0)?,
                provider: row.get(1)?,
                key: row.get(2)?,
            };
            Ok((id, row.get(3)?, row.get(4)?, row.get(5)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (id, project, ran_at, current) in filed {
        let folder = claimed(project.as_ref(), ran_at.as_ref(), &attached);
        if folder != current {
            set_state(connection, &id, &[("folder_id", &folder)])?;
        }
    }
    Ok(())
}

/**
 * Reads again, under the drive mount `location` now has, what was read under the one it had:
 * the project path of each of its sessions and each path attached for it, from how they were
 * recorded. Then files by the rules.
 */
pub(super) fn remount(
    connection: &Connection,
    location: &LocationId,
    mount: Option<&str>,
) -> rusqlite::Result<()> {
    let sessions: Vec<(i64, String)> = connection
        .prepare(
            "SELECT id, project_path FROM session
             WHERE location_id = ?1 AND project_path IS NOT NULL AND copy_label IS NULL",
        )?
        .query_map([location], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut read = connection.prepare("UPDATE session SET project = ?2 WHERE id = ?1")?;
    for (id, path) in sessions {
        read.execute(params![id, NormalPath::new(&path, mount)])?;
    }
    let attached: Vec<(i64, String)> = connection
        .prepare("SELECT folder_id, typed FROM folder_path WHERE location_id = ?1 ORDER BY rowid")?
        .query_map([location], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    /* Taken off and attached again in the order they were: a path may now be what another
    was a moment ago. One that now names what an earlier one does is not attached twice. */
    connection.execute("DELETE FROM folder_path WHERE location_id = ?1", [location])?;
    let mut attach = connection.prepare(
        "INSERT OR IGNORE INTO folder_path (folder_id, path, typed, location_id)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (folder, typed) in attached {
        if let Some(path) = NormalPath::new(&typed, mount).filter(NormalPath::is_absolute) {
            attach.execute(params![folder, path, typed, location])?;
        }
    }
    refile(connection)
}

/// The folder whose attached path claims a session started in `project` at `location`.
fn claimed(
    project: Option<&NormalPath>,
    location: Option<&LocationId>,
    attached: &[AttachedPath],
) -> Option<i64> {
    filing::file(project?, location, attached).map(|path| path.folder.0)
}

/// The project path the session `id`'s row has, as filing compares paths, and the location it
/// ran at ([`super::Session::ran_at`]): what the rules file it by.
pub(super) fn filing_of(
    connection: &Connection,
    id: &SessionId,
) -> rusqlite::Result<(Option<NormalPath>, Option<LocationId>)> {
    Ok(connection
        .query_row(
            "SELECT project, copy_label IS NULL FROM session
             WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
            params![id.location, id.provider, id.key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .map_or(
            (None, Some(id.location.clone())),
            |(project, original): (_, bool)| (project, original.then(|| id.location.clone())),
        ))
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
