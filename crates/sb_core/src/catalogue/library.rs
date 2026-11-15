//! What the library reads (PLAN §8, DESIGN §4.1, §4.4–§4.6): the counts beside the sidebar's
//! rows, and the sessions of a page. Only indexed sessions are counted and listed; one whose
//! row is missing appears once it is indexed.

use std::collections::HashMap;

use toolbox_store::rusqlite::{self, params_from_iter};

use super::session::{BY_ACTIVITY, SESSION, session_from};
use super::{Catalogue, Result, Session};
use crate::filing::{FolderId, NormalPath};
use crate::location::LocationId;

/// The sessions one page of the library lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shelf {
    All,
    Starred,
    /// The sessions in no folder.
    Unsorted,
    /// The sessions in a folder and in the folders inside it (PLAN §8).
    Folder(FolderId),
}

/// The counts beside the sidebar's rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LibraryCounts {
    pub all: usize,
    pub starred: usize,
    pub unsorted: usize,
    /// Each folder's sessions, with those of the folders inside it.
    pub folders: HashMap<FolderId, usize>,
}

/// The Unsorted sessions that started in one place: a project path at a location. The same
/// path at two locations is two places, unless it is a drive's, which is one place from every
/// location that sees the drive (a WSL distro and the Windows it runs on).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathGroup {
    /// The path, as filing compares paths; `None` for the sessions that recorded none.
    pub project: Option<NormalPath>,
    /// The locations its sessions are from: one, unless the path is a drive's.
    pub locations: Vec<LocationId>,
    /// The path as its sessions recorded it, each spelling once.
    pub spellings: Vec<String>,
    pub sessions: Vec<Session>,
}

impl Catalogue {
    pub fn library_counts(&self) -> Result<LibraryCounts> {
        self.db.read(|c| {
            let (all, starred, unsorted) = c.query_row(
                "SELECT count(*), COALESCE(sum(COALESCE(st.starred, 0)), 0),
                        COALESCE(sum(st.folder_id IS NULL), 0)
                 FROM session s
                 LEFT JOIN session_state st
                   ON st.location_id = s.location_id AND st.provider = s.provider
                  AND st.session = s.session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
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
                     LEFT JOIN session_state st ON st.folder_id = inside.id
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
                folders,
            })
        })
    }

    /// The sessions of a page, the one that did something last first.
    pub fn sessions(&self, shelf: Shelf) -> Result<Vec<Session>> {
        let (of, folder) = match shelf {
            Shelf::All => ("", None),
            Shelf::Starred => ("WHERE st.starred", None),
            Shelf::Unsorted => ("WHERE st.folder_id IS NULL", None),
            Shelf::Folder(folder) => (
                "WHERE st.folder_id IN (
                     WITH RECURSIVE inside (id) AS (
                         SELECT ?1
                         UNION ALL
                         SELECT f.id FROM folder f JOIN inside ON f.parent_id = inside.id
                     )
                     SELECT id FROM inside
                 )",
                Some(folder.0),
            ),
        };
        self.db.read(|c| {
            Ok(c.prepare(&format!("{SESSION} {of} {BY_ACTIVITY}"))?
                .query_map(params_from_iter(folder), session_from)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Unsorted, grouped by where the sessions started (PLAN §8): the group with the session
    /// that did something last first, and so the sessions in each.
    pub fn unsorted(&self) -> Result<Vec<PathGroup>> {
        let mut groups: Vec<PathGroup> = Vec::new();
        /* Where each place's group is: a place is a path, and its location unless the path is
        a drive's. */
        let mut places: HashMap<(Option<NormalPath>, Option<LocationId>), usize> = HashMap::new();
        for session in self.sessions(Shelf::Unsorted)? {
            let on_drive = session.project.as_ref().is_some_and(NormalPath::is_drive);
            let location = (!on_drive).then(|| session.id.location.clone());
            let at = *places
                .entry((session.project.clone(), location))
                .or_insert_with(|| {
                    groups.push(PathGroup {
                        project: session.project.clone(),
                        locations: Vec::new(),
                        spellings: Vec::new(),
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
            }
            group.sessions.push(session);
        }
        Ok(groups)
    }
}
