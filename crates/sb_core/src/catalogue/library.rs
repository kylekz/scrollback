//! What the library reads (PLAN §8, DESIGN §4.1, §4.4–§4.6): the counts beside the sidebar's
//! rows, and the sessions of a page. Only indexed sessions are counted and listed; one whose
//! row is missing appears once it is indexed.

use std::collections::HashMap;

use toolbox_store::rusqlite::{self, ToSql, params_from_iter};

use super::session::{BY_ACTIVITY, SESSION, session_from};
use super::{Catalogue, Result, Session, SessionId};
use crate::filing::{self, FolderId, NormalPath};
use crate::location::LocationId;
use crate::search::Filters;

/// The sessions one page of the library lists. Every page but Trash leaves out the sessions
/// in Trash (PLAN §4.5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
    /// The locations its sessions are from: one, unless the path is a drive's.
    pub locations: Vec<LocationId>,
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
                "SELECT COALESCE(sum(st.trashed_at IS NULL), 0),
                        COALESCE(sum(st.trashed_at IS NULL AND COALESCE(st.starred, 0)), 0),
                        COALESCE(sum(st.trashed_at IS NULL AND st.folder_id IS NULL), 0),
                        COALESCE(sum(st.trashed_at IS NOT NULL), 0)
                 FROM session s
                 LEFT JOIN session_state st
                   ON st.location_id = s.location_id AND st.provider = s.provider
                  AND st.session = s.session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
            /* `inside` pairs each folder with itself and with every folder inside it. */
            let folders = c
                .prepare(
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
                      AND s.session = st.session
                     GROUP BY inside.folder",
                )?
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

    /// The sessions of a page, the one that did something last first.
    pub fn sessions(&self, shelf: Shelf) -> Result<Vec<Session>> {
        let (of, folder) = on_shelf(shelf, "?1");
        self.db.read(|c| {
            Ok(c.prepare(&format!("{SESSION} WHERE {of} {BY_ACTIVITY}"))?
                .query_map(params_from_iter(folder), session_from)?
                .collect::<rusqlite::Result<_>>()?)
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
                 WHERE {shelf}
                   AND (:location IS NULL OR s.location_id = :location)
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
        sessions with no path are not taken for those at `/`. */
        type Place = (Option<NormalPath>, Option<LocationId>, Option<String>);
        let mut places: HashMap<Place, usize> = HashMap::new();
        let mounts: HashMap<LocationId, Option<String>> = self
            .locations()?
            .into_iter()
            .map(|location| (location.id, location.drive_mount))
            .collect();
        for session in self.sessions(Shelf::Unsorted)? {
            let on_drive = session.project.as_ref().is_some_and(NormalPath::is_drive);
            let location = (!on_drive).then(|| session.id.location.clone());
            let recorded = match session.project {
                Some(_) => None,
                None => session.project_path.clone(),
            };
            let at = *places
                .entry((session.project.clone(), location, recorded))
                .or_insert_with(|| {
                    groups.push(PathGroup {
                        project: session.project.clone(),
                        locations: Vec::new(),
                        spellings: Vec::new(),
                        drive: None,
                        sessions: Vec::new(),
                    });
                    groups.len() - 1
                });
            let group = &mut groups[at];
            if !group.locations.contains(&session.id.location) {
                group.locations.push(session.id.location.clone());
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
