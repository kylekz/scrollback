//! What the library reads (PLAN §8, DESIGN §4.1, §4.4–§4.6): the counts beside the sidebar's
//! rows, and the sessions of a page. Only indexed sessions are counted and listed; one whose
//! row is missing appears once it is indexed. A conversation archived more than once (an
//! original and copies of it kept elsewhere) is counted and listed once ([`SHOWN`], PLAN §7.7).

use std::collections::HashMap;

use toolbox_store::rusqlite::{self, OptionalExtension as _, ToSql, params_from_iter};

use super::session::{BY_ACTIVITY, RAN_AT, SESSION, SHOWN, session_from};
use super::{Catalogue, Result, Session, SessionId};
use crate::filing::{self, FolderId, NormalPath};
use crate::location::LocationId;
use crate::search::Filters;

/// The sessions one page of the library lists. Every page but Trash leaves out the sessions
/// in Trash (PLAN §4.5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Shelf {
    #[default]
    All,
    Starred,
    /// The sessions in no folder.
    Unsorted,
    /// The sessions in a folder and in the folders inside it (PLAN §8).
    Folder(FolderId),
    /// The sessions the owner deleted, until they are restored or deleted for good.
    Trash,
}

/// The counts beside the sidebar's rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LibraryCounts {
    pub all: usize,
    pub starred: usize,
    pub unsorted: usize,
    pub trash: usize,
    /// Each folder's sessions, with those of the folders inside it.
    pub folders: HashMap<FolderId, usize>,
}

/// The Unsorted sessions that started in one place: a project path at a location. The same
/// path at two locations is two places, unless it is a drive's, which is one place from every
/// location that sees the drive (a WSL distro and the Windows it runs on). The sessions that
/// recorded no path are a place of their own at each location, and so are those of each path
/// that names no directory (`/`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathGroup {
    /// The path, as filing compares paths; `None` for the sessions that recorded none, or one
    /// that names no directory, which no folder can claim.
    pub project: Option<NormalPath>,
    /// The locations its sessions are from: one, unless the path is a drive's; none for
    /// copies of sessions that ran elsewhere, which are a place of their own (PLAN §7.7).
    pub locations: Vec<LocationId>,
    /// What the tag of the group's sessions says when they are copies attributed to no location
    /// (PLAN §7.7): the host they ran on, else what they are copies as.
    pub copy: Option<String>,
    /// The path as its sessions recorded it, each spelling once.
    pub spellings: Vec<String>,
    /**
     * A drive's path as Windows writes it (`D:\Projects\fc`), to show and to attach: a
     * spelling a session recorded so, or else one under a WSL distro's drive mount, read by
     * that location's own mount. `None` for a path that is no drive's.
     */
    pub drive: Option<String>,
    pub sessions: Vec<Session>,
}

impl Catalogue {
    pub fn library_counts(&self) -> Result<LibraryCounts> {
        self.db.read(|c| {
            let (all, starred, unsorted, trash) = c.query_row(
                &format!(
                    "SELECT COALESCE(sum(st.trashed_at IS NULL), 0),
                            COALESCE(sum(st.trashed_at IS NULL AND COALESCE(st.starred, 0)), 0),
                            COALESCE(sum(st.trashed_at IS NULL AND st.folder_id IS NULL), 0),
                            COALESCE(sum(st.trashed_at IS NOT NULL), 0)
                     FROM session s
                     LEFT JOIN session_state st
                       ON st.location_id = s.location_id AND st.provider = s.provider
                      AND st.session = s.session
                     WHERE {SHOWN}"
                ),
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
            /* `inside` pairs each folder with itself and with every folder inside it. */
            let folders = c
                .prepare(&format!(
                    "WITH RECURSIVE inside (folder, id) AS (
                         SELECT id, id FROM folder
                         UNION ALL
                         SELECT inside.folder, f.id FROM folder f
                         JOIN inside ON f.parent_id = inside.id
                     )
                     SELECT inside.folder, count(s.id) FROM inside
                     LEFT JOIN session_state st
                       ON st.folder_id = inside.id AND st.trashed_at IS NULL
                     LEFT JOIN session s
                       ON s.location_id = st.location_id AND s.provider = st.provider
                      AND s.session = st.session AND {SHOWN}
                     GROUP BY inside.folder"
                ))?
                .query_map([], |row| Ok((FolderId(row.get(0)?), row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(LibraryCounts {
                all,
                starred,
                unsorted,
                trash,
                folders,
            })
        })
    }

    /// The session `id`, while the library shows it: not a copy hidden behind its original or
    /// another copy (PLAN §7.7).
    pub fn shown_session(&self, id: &SessionId) -> Result<Option<Session>> {
        self.db.read(|c| {
            Ok(c.query_row(
                &format!(
                    "{SESSION} WHERE s.location_id = ?1 AND s.provider = ?2 AND s.session = ?3
                       AND {SHOWN}"
                ),
                rusqlite::params![id.location, id.provider, id.key],
                session_from,
            )
            .optional()?)
        })
    }

    /// The sessions of a page, the one that did something last first.
    pub fn sessions(&self, shelf: Shelf) -> Result<Vec<Session>> {
        let (of, folder) = on_shelf(shelf, "?1");
        self.db.read(|c| {
            Ok(
                c.prepare(&format!("{SESSION} WHERE {of} AND {SHOWN} {BY_ACTIVITY}"))?
                    .query_map(params_from_iter(folder), session_from)?
                    .collect::<rusqlite::Result<_>>()?,
            )
        })
    }

    /**
     * The models and the branches of the sessions not in Trash, each once, the one most
     * sessions have first: what the palette offers for `model:` and `branch:` (DESIGN §4.9).
     */
    pub fn models_and_branches(&self) -> Result<(Vec<String>, Vec<String>)> {
        const LIVE: &str = "FROM session s LEFT JOIN session_state st
                              ON st.location_id = s.location_id AND st.provider = s.provider
                             AND st.session = s.session";
        let live = format!("st.trashed_at IS NULL AND {SHOWN}");
        self.db.read(|c| {
            let values = |sql: String| -> Result<Vec<String>> {
                Ok(c.prepare(&sql)?
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<_>>()?)
            };
            Ok((
                values(format!(
                    "SELECT m.value {LIVE}, json_each(s.models) m WHERE {live}
                     GROUP BY m.value ORDER BY count(*) DESC, m.value"
                ))?,
                values(format!(
                    "SELECT s.branch {LIVE} WHERE {live} AND s.branch IS NOT NULL
                     GROUP BY s.branch ORDER BY count(*) DESC, s.branch"
                ))?,
            ))
        })
    }

    /// Every session `filters` let through (PLAN §10), the one that did something last first.
    pub(crate) fn filtered(&self, filters: &Filters) -> Result<Vec<SessionId>> {
        let (shelf, folder) = on_shelf(filters.shelf, ":folder");
        let location = filters.location.as_ref();
        let agent = filters.agent.map(|agent| agent.as_str());
        let mut named: Vec<(&str, &dyn ToSql)> = vec![
            (":location", &location),
            (":agent", &agent),
            (":model", &filters.model),
            (":branch", &filters.branch),
            (":after", &filters.after),
            (":before", &filters.before),
        ];
        if let Some(folder) = &folder {
            named.push((":folder", folder));
        }
        self.db.read(|c| {
            Ok(c.prepare(&format!(
                "SELECT s.location_id, s.provider, s.session FROM session s
                 LEFT JOIN session_state st
                   ON st.location_id = s.location_id AND st.provider = s.provider
                  AND st.session = s.session
                 WHERE {shelf} AND {SHOWN}
                   AND (:location IS NULL OR {RAN_AT} = :location)
                   AND (:agent IS NULL OR s.provider = :agent)
                   AND (:model IS NULL OR EXISTS (
                       SELECT 1 FROM json_each(s.models)
                       WHERE instr(lower(value), lower(:model))))
                   AND (:branch IS NULL OR s.branch = :branch COLLATE NOCASE)
                   AND (:after IS NULL OR COALESCE(s.ended_at, s.started_at) >= :after)
                   AND (:before IS NULL OR COALESCE(s.started_at, s.ended_at) < :before)
                 {BY_ACTIVITY}"
            ))?
            .query_map(named.as_slice(), |row| {
                Ok(SessionId {
                    location: row.get(0)?,
                    provider: row.get(1)?,
                    key: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Unsorted, grouped by where the sessions started (PLAN §8): the group with the session
    /// that did something last first, and so the sessions in each.
    pub fn unsorted(&self) -> Result<Vec<PathGroup>> {
        let mut groups: Vec<PathGroup> = Vec::new();
        /* Where each place's group is: a place is a path, and its location unless the path is
        a drive's. A path filing can't compare is a place by how it was recorded, so that the
        sessions with no path are not taken for those at `/`. Copies of sessions that ran
        elsewhere are a place apart, by path alone (PLAN §7.7). */
        type Place = (
            Option<NormalPath>,
            Option<LocationId>,
            Option<String>,
            Option<String>,
        );
        let mut places: HashMap<Place, usize> = HashMap::new();
        let mounts: HashMap<LocationId, Option<String>> = self
            .locations()?
            .into_iter()
            .map(|location| (location.id, location.drive_mount))
            .collect();
        for session in self.sessions(Shelf::Unsorted)? {
            /* A copy attributed to the location it ran at is grouped as that location's. */
            let copy = (session.copy.as_ref())
                .filter(|_| session.ran_at().is_none())
                .map(|copy| copy.tag().to_string());
            let on_drive =
                copy.is_none() && session.project.as_ref().is_some_and(NormalPath::is_drive);
            let location = session.ran_at().filter(|_| !on_drive).cloned();
            let recorded = match session.project {
                Some(_) => None,
                None => session.project_path.clone(),
            };
            let at = *places
                .entry((session.project.clone(), location, recorded, copy.clone()))
                .or_insert_with(|| {
                    groups.push(PathGroup {
                        project: session.project.clone(),
                        locations: Vec::new(),
                        copy,
                        spellings: Vec::new(),
                        drive: None,
                        sessions: Vec::new(),
                    });
                    groups.len() - 1
                });
            let group = &mut groups[at];
            if let Some(location) = session.ran_at()
                && !group.locations.contains(location)
            {
                group.locations.push(location.clone());
            }
            if let Some(spelling) = &session.project_path
                && !group.spellings.contains(spelling)
            {
                group.spellings.push(spelling.clone());
                /* The first spelling on a drive leads; until there is one, the first under
                a mount stands in for it. */
                if on_drive && !group.drive.as_deref().is_some_and(on_windows_drive) {
                    group.drive = if on_windows_drive(spelling) {
                        Some(spelling.clone())
                    } else {
                        group.drive.take().or_else(|| {
                            let mount = mounts.get(&session.id.location).cloned().flatten();
                            filing::drive_form(spelling, mount.as_deref())
                        })
                    };
                }
            }
            group.sessions.push(session);
        }
        Ok(groups)
    }
}

/// Whether `path` is written as Windows writes a path on a drive: `D:\…` or `D:/…`.
fn on_windows_drive(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/**
 * The condition, on the owner's state of a session as `st` (joined to its row, so `st` may be
 * missing), that it is on `shelf`; and the folder it takes, if it takes one, as the parameter
 * named `folder`.
 */
pub(super) fn on_shelf(shelf: Shelf, folder: &str) -> (String, Option<i64>) {
    match shelf {
        Shelf::All => ("st.trashed_at IS NULL".to_string(), None),
        Shelf::Starred => ("st.trashed_at IS NULL AND st.starred".to_string(), None),
        Shelf::Unsorted => (
            "st.trashed_at IS NULL AND st.folder_id IS NULL".to_string(),
            None,
        ),
        Shelf::Folder(id) => (
            format!(
                "st.trashed_at IS NULL AND st.folder_id IN (
                     WITH RECURSIVE inside (id) AS (
                         SELECT {folder}
                         UNION ALL
                         SELECT f.id FROM folder f JOIN inside ON f.parent_id = inside.id
                     )
                     SELECT id FROM inside
                 )"
            ),
            Some(id.0),
        ),
        Shelf::Trash => ("st.trashed_at IS NOT NULL".to_string(), None),
    }
}
