//! Trash and deleting for good (PLAN §4.5, D14): the owner's state of a session that is in
//! Trash, the tombstones of sessions deleted for good, and the files whose removal is pending.
//! All of it is the owner's and only here (PLAN §2.2): a rebuild of the index leaves it.
//!
//! **A tombstone** is the identity of the session that was deleted and the keys of the items it
//! had then. Sync skips an item by its key, whatever session its provider says it is of now,
//! so nothing that was deleted comes back after a reclassification gives its session another
//! key. It also skips the new items (a subagent that starts later, say) of a tombstoned
//! session, and of a session one of the deleted keys is now of, unless the archive holds that
//! session: then it is a live one that a reclassification merged with the deleted one, and its
//! backup goes on ([`Tombstoned::skips`]).
//!
//! **Deleting for good** removes exactly the session's items: their rows and the files the
//! catalogue records for them, by name, in each item's own folder, and nothing else. An item's
//! folder is its alone (it is named by its key); the folders above it are shared, and are
//! removed only when that leaves them empty ([`crate::archive::Archive::delete_for_good`]).

use std::collections::{HashMap, HashSet};

use toolbox_store::rusqlite::{self, OptionalExtension as _, params};

use super::session::{archived, set_state};
use super::{Catalogue, CatalogueError, Result, SessionId, now_ms};
use crate::location::LocationId;
use crate::provider::ScannedItem;

/// What deleting a session for good removes: what the confirmation names (DESIGN §4.8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deletion {
    pub session: SessionId,
    /// Its items: its transcript and every file that goes with it.
    pub files: usize,
    /// The versions of its own transcript, as its history shows them (FEATURES: Versions);
    /// 0 without one.
    pub versions: usize,
    /// What is archived of all of its items, every version, uncompressed.
    pub bytes: u64,
    /// Some item of it is still at its location, as far as the last sync there saw: when none
    /// is, this is the last copy.
    pub at_location: bool,
}

/// A session deleted for good, which sync does not archive again until this is removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tombstone {
    pub session: SessionId,
    /// When, in Unix milliseconds.
    pub deleted: i64,
    /// What its row said when it was deleted.
    pub title: Option<String>,
    pub project_path: Option<String>,
}

/// The files of one deleted item that are still to be removed, in its folder under `files/`.
pub(crate) struct Removal {
    pub location: LocationId,
    pub provider: String,
    pub folder: String,
    pub files: Vec<ItemFile>,
}

/// A file in an item's folder (PLAN §4.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ItemFile {
    /// Its `source.json`.
    Source,
    Segment {
        version: u32,
        offset: u64,
    },
}

/// What a pass of one provider at one location skips (PLAN §5, step 2): the module docs.
#[derive(Debug, Default)]
pub(crate) struct Tombstoned {
    keys: HashSet<String>,
    sessions: HashSet<String>,
}

impl Tombstoned {
    /// Whether each of `scanned` is skipped, beside `archived`, the key and session of each
    /// item the catalogue holds of the provider at the location.
    pub(crate) fn skips<'a>(
        &self,
        scanned: &[ScannedItem],
        archived: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
    ) -> Vec<bool> {
        if self.keys.is_empty() && self.sessions.is_empty() {
            return vec![false; scanned.len()];
        }
        let now: HashMap<&str, Option<&str>> = scanned
            .iter()
            .map(|item| (item.key.as_str(), item.session.as_deref()))
            .collect();
        let deleted: HashSet<&str> = self
            .sessions
            .iter()
            .map(String::as_str)
            .chain(
                scanned
                    .iter()
                    .filter(|item| self.keys.contains(&item.key))
                    .filter_map(|item| item.session.as_deref()),
            )
            .collect();
        let live: HashSet<&str> = archived
            .into_iter()
            .filter_map(|(key, session)| now.get(key).copied().unwrap_or(session))
            .collect();
        scanned
            .iter()
            .map(|item| {
                self.keys.contains(&item.key)
                    || item
                        .session
                        .as_deref()
                        .is_some_and(|session| deleted.contains(session) && !live.contains(session))
            })
            .collect()
    }
}

impl Catalogue {
    /// Moves a session to Trash (PLAN §4.5): it leaves every page of the library but Trash,
    /// and is synced and indexed as before. A session in Trash already stays as it was.
    pub fn trash(&self, id: &SessionId) -> Result<()> {
        self.db.write(|tx| {
            archived(tx, id)?;
            if trashed_at(tx, id)?.is_none() {
                set_state(tx, id, &[("trashed_at", &now_ms())])?;
            }
            Ok(())
        })
    }

    /**
     * Takes a session out of Trash, back to where it is filed. That is the folder it was in;
     * if that folder was deleted meanwhile, deleting it already handed the session to the
     * rules (PLAN §8), so it is where they file it, or in Unsorted.
     */
    pub fn restore(&self, id: &SessionId) -> Result<()> {
        self.db.write(|tx| {
            if trashed_at(tx, id)?.is_none() {
                return Err(CatalogueError::NotInTrash(id.clone()));
            }
            set_state(tx, id, &[("trashed_at", &None::<i64>)])?;
            Ok(())
        })
    }

    /// What deleting the session for good would remove (DESIGN §4.8). Only a session in
    /// Trash is deleted for good.
    pub fn deletion(&self, id: &SessionId) -> Result<Deletion> {
        self.db.read(|c| deletion(c, id))
    }

    /**
     * Deletes a session in Trash for good, in one transaction (PLAN §4.5): its items' rows and
     * its own, with its search text and what the owner decided about it; a tombstone in their
     * place; and, recorded as pending, the removal of every file the catalogue had for its
     * items, which the archive does once this is committed
     * ([`crate::archive::Archive::delete_for_good`]).
     */
    pub(crate) fn delete_for_good(&self, id: &SessionId) -> Result<Deletion> {
        self.db.write(|tx| {
            let deletion = deletion(tx, id)?;
            let identity = params![id.location, id.provider, id.key];
            let items: Vec<(i64, String, String)> = tx
                .prepare(
                    "SELECT id, key, item_id FROM item
                     WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
                )?
                .query_map(identity, |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let (title, project_path): (Option<String>, Option<String>) = tx
                .query_row(
                    "SELECT title, project_path FROM session
                     WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
                    identity,
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
                .unwrap_or_default();
            tx.execute(
                "INSERT INTO tombstone (location_id, provider, session, deleted_at, title,
                                        project_path)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (location_id, provider, session) DO UPDATE SET
                     deleted_at = excluded.deleted_at, title = excluded.title,
                     project_path = excluded.project_path",
                params![
                    id.location,
                    id.provider,
                    id.key,
                    now_ms(),
                    title,
                    project_path
                ],
            )?;
            tx.execute(
                "DELETE FROM session WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
                identity,
            )?;
            tx.execute(
                "DELETE FROM session_state
                 WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
                identity,
            )?;
            for (item, key, folder) in &items {
                tx.execute(
                    "INSERT INTO removal (location_id, provider, folder, version, offset)
                     SELECT ?1, ?2, ?3, NULL, NULL
                     UNION ALL
                     SELECT ?1, ?2, ?3, v.number, s.offset FROM version v
                     JOIN segment s ON s.version_id = v.id WHERE v.item_id = ?4",
                    params![id.location, id.provider, folder, item],
                )?;
                tx.execute(
                    "INSERT OR REPLACE INTO tombstone_item (location_id, provider, key, session)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![id.location, id.provider, key, id.key],
                )?;
                for forget in [
                    "DELETE FROM reading_choice WHERE item_id = ?1",
                    "DELETE FROM segment
                     WHERE version_id IN (SELECT id FROM version WHERE item_id = ?1)",
                    "DELETE FROM version WHERE item_id = ?1",
                    "DELETE FROM item WHERE id = ?1",
                ] {
                    tx.execute(forget, [item])?;
                }
                tx.execute(
                    "DELETE FROM waiting WHERE location_id = ?1 AND provider = ?2 AND key = ?3",
                    params![id.location, id.provider, key],
                )?;
            }
            Ok(deletion)
        })
    }

    /// Every tombstone, the latest first: what Settings lists (FEATURES: Settings).
    pub fn tombstones(&self) -> Result<Vec<Tombstone>> {
        self.db.read(|c| {
            Ok(c.prepare(
                "SELECT location_id, provider, session, deleted_at, title, project_path
                 FROM tombstone ORDER BY deleted_at DESC, location_id, provider, session",
            )?
            .query_map([], |row| {
                Ok(Tombstone {
                    session: SessionId {
                        location: row.get(0)?,
                        provider: row.get(1)?,
                        key: row.get(2)?,
                    },
                    deleted: row.get(3)?,
                    title: row.get(4)?,
                    project_path: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Removes a tombstone: the next sync of its location archives the session again, if the
    /// location still has it.
    pub fn remove_tombstone(&self, id: &SessionId) -> Result<()> {
        self.db.write(|tx| {
            let identity = params![id.location, id.provider, id.key];
            tx.execute(
                "DELETE FROM tombstone_item
                 WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
                identity,
            )?;
            if tx.execute(
                "DELETE FROM tombstone WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
                identity,
            )? == 0
            {
                return Err(CatalogueError::NoTombstone(id.clone()));
            }
            Ok(())
        })
    }

    /// What the tombstones at `location` say a pass of `provider` there skips.
    pub(crate) fn tombstoned(&self, location: &LocationId, provider: &str) -> Result<Tombstoned> {
        self.db.read(|c| {
            let mut tombstoned = Tombstoned::default();
            let mut select = c.prepare(
                "SELECT session, NULL FROM tombstone WHERE location_id = ?1 AND provider = ?2
                 UNION ALL
                 SELECT NULL, key FROM tombstone_item WHERE location_id = ?1 AND provider = ?2",
            )?;
            let mut rows = select.query(params![location, provider])?;
            while let Some(row) = rows.next()? {
                match (row.get(0)?, row.get(1)?) {
                    (Some(session), _) => tombstoned.sessions.insert(session),
                    (_, Some(key)) => tombstoned.keys.insert(key),
                    (None, None) => false,
                };
            }
            Ok(tombstoned)
        })
    }

    /// The files of deleted items still to be removed, item by item.
    pub(crate) fn removals(&self) -> Result<Vec<Removal>> {
        self.db.read(|c| {
            let mut removals: Vec<Removal> = Vec::new();
            let mut select = c.prepare(
                "SELECT location_id, provider, folder, version, offset FROM removal
                 ORDER BY location_id, provider, folder",
            )?;
            let mut rows = select.query([])?;
            while let Some(row) = rows.next()? {
                let (location, provider, folder): (LocationId, String, String) =
                    (row.get(0)?, row.get(1)?, row.get(2)?);
                let file = match (row.get(3)?, row.get(4)?) {
                    (Some(version), Some(offset)) => ItemFile::Segment { version, offset },
                    _ => ItemFile::Source,
                };
                match removals.last_mut() {
                    Some(last)
                        if (&last.location, &last.provider, &last.folder)
                            == (&location, &provider, &folder) =>
                    {
                        last.files.push(file);
                    }
                    _ => removals.push(Removal {
                        location,
                        provider,
                        folder,
                        files: vec![file],
                    }),
                }
            }
            Ok(removals)
        })
    }

    /// Whether the catalogue records an item in the folder `folder` of `provider` at
    /// `location`: one archived there since its files were to be removed.
    pub(crate) fn has_item_in(
        &self,
        location: &LocationId,
        provider: &str,
        folder: &str,
    ) -> Result<bool> {
        self.db.read(|c| {
            Ok(c.query_row(
                "SELECT EXISTS (
                     SELECT 1 FROM item WHERE location_id = ?1 AND provider = ?2 AND item_id = ?3
                 )",
                params![location, provider, folder],
                |row| row.get(0),
            )?)
        })
    }

    /// The removal of the item's files is done, or is not to be done.
    pub(crate) fn removed(&self, removal: &Removal) -> Result<()> {
        self.execute(
            "DELETE FROM removal WHERE location_id = ?1 AND provider = ?2 AND folder = ?3",
            params![removal.location, removal.provider, removal.folder],
        )
    }
}

/// When the session went to Trash, if it is there.
fn trashed_at(connection: &rusqlite::Connection, id: &SessionId) -> Result<Option<i64>> {
    Ok(connection
        .query_row(
            "SELECT trashed_at FROM session_state
             WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
            params![id.location, id.provider, id.key],
            |row| row.get(0),
        )
        .optional()?
        .flatten())
}

/// What deleting the session in Trash `id` for good removes.
fn deletion(connection: &rusqlite::Connection, id: &SessionId) -> Result<Deletion> {
    if trashed_at(connection, id)?.is_none() {
        return Err(CatalogueError::NotInTrash(id.clone()));
    }
    let (files, bytes, at_location): (usize, u64, bool) = connection.query_row(
        "SELECT count(*),
                COALESCE(sum((SELECT sum(length) FROM version WHERE item_id = i.id)), 0),
                COALESCE(max(i.gone_since IS NULL), 0)
         FROM item i WHERE i.location_id = ?1 AND i.provider = ?2 AND i.session = ?3",
        params![id.location, id.provider, id.key],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if files == 0 {
        return Err(CatalogueError::NoSession(id.clone()));
    }
    let versions = connection.query_row(
        "SELECT count(*) FROM version v JOIN session s ON s.main_item = v.item_id
         WHERE s.location_id = ?1 AND s.provider = ?2 AND s.session = ?3",
        params![id.location, id.provider, id.key],
        |row| row.get(0),
    )?;
    Ok(Deletion {
        session: id.clone(),
        files,
        versions,
        bytes,
        at_location,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ItemKind;

    fn scanned(key: &str, session: &str) -> ScannedItem {
        ScannedItem {
            key: key.to_string(),
            kind: ItemKind::Transcript,
            session: Some(session.to_string()),
            length: 1,
            stamp: String::new(),
            lines: true,
            export_version: None,
        }
    }

    /// Which of `scanned`, each a key and the session its provider says it is of, a pass
    /// skips beside `archived`, after the session `a` with the item `a1` was deleted.
    fn skipped(scanned: &[(&str, &str)], archived: &[(&str, &str)]) -> Vec<String> {
        let tombstoned = Tombstoned {
            keys: HashSet::from(["a1".to_string()]),
            sessions: HashSet::from(["a".to_string()]),
        };
        let scanned: Vec<ScannedItem> = scanned
            .iter()
            .map(|(key, session)| self::scanned(key, session))
            .collect();
        let skips = tombstoned.skips(
            &scanned,
            archived.iter().map(|(key, session)| (*key, Some(*session))),
        );
        scanned
            .into_iter()
            .zip(skips)
            .filter(|(_, skip)| *skip)
            .map(|(item, _)| item.key)
            .collect()
    }

    #[test]
    fn a_tombstone_skips_its_keys_and_new_items_of_its_session_unless_one_is_archived() {
        /* Its item, and a new one of its session; never another session's. */
        assert_eq!(
            skipped(&[("a1", "a"), ("a2", "a"), ("b1", "b")], &[("b1", "b")]),
            ["a1", "a2"]
        );
        /* A classifier gave it another id: its item is found by its key, and the new one by
        the session that item is of now. */
        assert_eq!(
            skipped(&[("a1", "a'"), ("a2", "a'"), ("b1", "b")], &[("b1", "b")]),
            ["a1", "a2"]
        );
        /* A classifier merged it with a session the archive holds: that session's backup goes
        on, and only the deleted item is skipped. */
        assert_eq!(
            skipped(&[("a1", "b"), ("b1", "b"), ("b2", "b")], &[("b1", "b")]),
            ["a1"]
        );
        /* A session the archive holds under the deleted id, as one merged into it is. */
        assert_eq!(
            skipped(&[("b1", "a"), ("b2", "a")], &[("b1", "a")]),
            Vec::<String>::new()
        );
        /* Nothing is skipped without a tombstone. */
        assert_eq!(
            Tombstoned::default().skips(&[scanned("a1", "a")], []),
            [false]
        );
    }

    /**
     * Deleting a session for good and searching read rows of tables that grow with the
     * archive. Each looks them up by an index, a session's items by `item_session` and its
     * spans by their range of ids, and none goes through a whole table of them.
     */
    #[test]
    fn deleting_and_searching_find_their_rows_by_an_index() {
        let dir = tempfile::tempdir().unwrap();
        let archive = crate::archive::Archive::create(dir.path()).unwrap();
        let statements = [
            "SELECT id, key, item_id FROM item
             WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
            "SELECT count(*),
                    COALESCE(sum((SELECT sum(length) FROM version WHERE item_id = i.id)), 0),
                    COALESCE(max(i.gone_since IS NULL), 0)
             FROM item i WHERE i.location_id = ?1 AND i.provider = ?2 AND i.session = ?3",
            "SELECT count(*) FROM version v JOIN session s ON s.main_item = v.item_id
             WHERE s.location_id = ?1 AND s.provider = ?2 AND s.session = ?3",
            "SELECT v.number, s.offset FROM version v
             JOIN segment s ON s.version_id = v.id WHERE v.item_id = ?1",
            "DELETE FROM segment WHERE version_id IN (SELECT id FROM version WHERE item_id = ?1)",
            "DELETE FROM version WHERE item_id = ?1",
            "DELETE FROM item WHERE id = ?1",
            "SELECT EXISTS (
                 SELECT 1 FROM item WHERE location_id = ?1 AND provider = ?2 AND item_id = ?3
             )",
            "SELECT session, NULL FROM tombstone WHERE location_id = ?1 AND provider = ?2
             UNION ALL
             SELECT NULL, key FROM tombstone_item WHERE location_id = ?1 AND provider = ?2",
            "DELETE FROM tombstone_item WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
            "SELECT 'delete', id, text FROM span WHERE id BETWEEN ?1 AND ?2",
            "DELETE FROM span WHERE id BETWEEN ?1 AND ?2",
            "SELECT kind, subagent, turn, block, unattached, text FROM span WHERE id = ?1",
            "SELECT rowid FROM session_text WHERE session_text MATCH ?1 ORDER BY rowid",
            "SELECT id FROM span WHERE id BETWEEN ?1 AND ?2 AND text LIKE ?3 ORDER BY id",
            "SELECT id FROM session WHERE main_item = ?1",
            "DELETE FROM removal WHERE location_id = ?1 AND provider = ?2 AND folder = ?3",
        ];
        archive
            .catalogue()
            .db
            .read(|c| {
                for sql in statements {
                    let bound = (1..=4).fold(sql.to_string(), |sql, at| {
                        sql.replace(&format!("?{at}"), "'1'")
                    });
                    let plan: Vec<String> = c
                        .prepare(&format!("EXPLAIN QUERY PLAN {bound}"))?
                        .query_map([], |row| row.get(3))?
                        .collect::<rusqlite::Result<_>>()?;
                    assert!(
                        plan.iter().all(|step| !step.starts_with("SCAN")
                            || step.contains("VIRTUAL TABLE")
                            || step == "SCAN CONSTANT ROW"),
                        "{sql}: {plan:#?}"
                    );
                    assert!(
                        !plan.iter().any(|step| step.contains("TEMP B-TREE")),
                        "{sql}: {plan:#?}"
                    );
                }
                Ok::<_, CatalogueError>(())
            })
            .unwrap();
    }
}
