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
//! What is freed in the catalogue's file is overwritten, so of the session the catalogue keeps
//! only its tombstone: its identity, its items' keys, its project path and when it ran, and
//! nothing of what was said in it.

use std::collections::{BTreeSet, HashMap, HashSet};

use toolbox_store::rusqlite::{self, OptionalExtension as _, params};

use super::session::{archived, set_state};
use super::{Catalogue, CatalogueError, Result, SessionId, now_ms};
use crate::hash::Hash;
use crate::location::LocationId;
use crate::provider::ScannedItem;

/**
 * What deleting a session for good removes: what the confirmation names (DESIGN §4.8), and
 * what [`crate::archive::Archive::delete_for_good`] is given back, to delete only that.
 *
 * Its project path and when it ran stay in its tombstone, for Settings to name it, and nothing
 * of what was said in it. The catalogue's backups made before the delete hold its row as it
 * was (its title, which may be the start of its first prompt, and its project path) until
 * they age out of `backups/`. Its text is in no backup.
 */
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
    /// Where and when it ran, as its row said when it was deleted.
    pub project_path: Option<String>,
    pub started: Option<i64>,
    pub ended: Option<i64>,
}

/// The files of one deleted item that are still to be removed, in its folder under `files/`.
pub(crate) struct Removal {
    pub location: LocationId,
    pub provider: String,
    /// The item's folder: 32 hex digits, as the layout names it.
    pub folder: String,
    /// Each file with its row in `removal`.
    pub files: Vec<(i64, ItemFile)>,
}

/// A file in a deleted item's folder (PLAN §4.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ItemFile {
    /// Its `source.json`.
    Source,
    /// A segment, which is removed only while it holds the `length` bytes of SHA-256 `hash`
    /// that it held when it was deleted.
    Segment {
        version: u32,
        offset: u64,
        length: u64,
        hash: Hash,
    },
}

/// What a pass of one provider at one location skips (PLAN §5, step 2): the module docs.
#[derive(Debug, Default)]
pub(crate) struct Tombstoned {
    /// The deleted items' keys, each with its tombstone's session.
    keys: HashMap<String, String>,
    sessions: HashSet<String>,
}

impl Tombstoned {
    /**
     * Whether each of `scanned` is skipped, beside `archived`, the key and session of each item
     * the catalogue holds of the provider at the location; and the tombstones (their sessions)
     * of what is skipped, so the owner can be told that a session deleted for good goes on at
     * its location unarchived.
     */
    pub(crate) fn skips<'a>(
        &self,
        scanned: &[ScannedItem],
        archived: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
    ) -> (Vec<bool>, BTreeSet<String>) {
        let mut met = BTreeSet::new();
        if self.keys.is_empty() && self.sessions.is_empty() {
            return (vec![false; scanned.len()], met);
        }
        let now: HashMap<&str, Option<&str>> = scanned
            .iter()
            .map(|item| (item.key.as_str(), item.session.as_deref()))
            .collect();
        /* Each session whose new items are skipped, with its tombstone. */
        let mut deleted: HashMap<&str, &str> = self
            .sessions
            .iter()
            .map(|session| (session.as_str(), session.as_str()))
            .collect();
        for item in scanned {
            if let (Some(tombstone), Some(session)) =
                (self.keys.get(&item.key), item.session.as_deref())
            {
                deleted.entry(session).or_insert(tombstone);
            }
        }
        let live: HashSet<&str> = archived
            .into_iter()
            .filter_map(|(key, session)| now.get(key).copied().unwrap_or(session))
            .collect();
        let skips = scanned
            .iter()
            .map(|item| {
                let tombstone = self.keys.get(&item.key).map(String::as_str).or_else(|| {
                    let session = item.session.as_deref()?;
                    deleted
                        .get(session)
                        .filter(|_| !live.contains(session))
                        .copied()
                });
                met.extend(tombstone.map(str::to_string));
                tombstone.is_some()
            })
            .collect();
        (skips, met)
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
     * Takes a session out of Trash, back to where it is filed. That is the folder it was in,
     * by hand or by the rules, which left it alone while it was in Trash and file it now; if
     * that folder was deleted meanwhile, it is where they file it, or in Unsorted (PLAN §8).
     */
    pub fn restore(&self, id: &SessionId) -> Result<()> {
        self.db.write(|tx| {
            if trashed_at(tx, id)?.is_none() {
                return Err(CatalogueError::NotInTrash(id.clone()));
            }
            super::batch::restore(tx, id)
        })
    }

    /// What deleting the session for good would remove (DESIGN §4.8). Only a session in
    /// Trash is deleted for good.
    pub fn deletion(&self, id: &SessionId) -> Result<Deletion> {
        self.db.read(|c| deletion(c, id))
    }

    /**
     * Deletes the session in Trash that `shown` is of for good, in one transaction (PLAN §4.5):
     * its items' rows and its own, with what the owner decided about it; a tombstone in their
     * place; and, recorded as pending, the removal of every file the catalogue had for its
     * items, which the archive does once this is committed
     * ([`crate::archive::Archive::delete_for_good`]). What is deleted is what was shown:
     * if the session has another item, version or length now,
     * [`CatalogueError::DeletionChanged`], and nothing is deleted.
     */
    pub(crate) fn delete_for_good(&self, shown: &Deletion) -> Result<()> {
        let id = &shown.session;
        self.db.write(|tx| {
            if deletion(tx, id)? != *shown {
                return Err(CatalogueError::DeletionChanged(id.clone()));
            }
            let identity = params![id.location, id.provider, id.key];
            let items: Vec<(i64, String, String)> = tx
                .prepare(
                    "SELECT id, key, item_id FROM item
                     WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
                )?
                .query_map(identity, |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let (project_path, started, ended): (Option<String>, Option<i64>, Option<i64>) = tx
                .query_row(
                    "SELECT project_path, started_at, ended_at FROM session
                     WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
                    identity,
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?
                .unwrap_or_default();
            tx.execute(
                "INSERT INTO tombstone (location_id, provider, session, deleted_at, project_path,
                                        started_at, ended_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (location_id, provider, session) DO UPDATE SET
                     deleted_at = excluded.deleted_at, project_path = excluded.project_path,
                     started_at = excluded.started_at, ended_at = excluded.ended_at",
                params![
                    id.location,
                    id.provider,
                    id.key,
                    now_ms(),
                    project_path,
                    started,
                    ended
                ],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO text_gone (location_id, provider, session)
                 VALUES (?1, ?2, ?3)",
                identity,
            )?;
            for forget in [
                "DELETE FROM session WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
                "DELETE FROM session_state
                 WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
            ] {
                tx.execute(forget, identity)?;
            }
            for (item, key, folder) in &items {
                tx.execute(
                    "INSERT INTO removal (location_id, provider, folder, version, offset, length,
                                          hash)
                     SELECT ?1, ?2, ?3, NULL, NULL, NULL, NULL
                     UNION ALL
                     SELECT ?1, ?2, ?3, v.number, s.offset, s.length, s.hash FROM version v
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
            Ok(())
        })
    }

    /// Every tombstone, the latest first: what Settings lists (FEATURES: Settings).
    pub fn tombstones(&self) -> Result<Vec<Tombstone>> {
        self.db.read(|c| {
            Ok(c.prepare(
                "SELECT location_id, provider, session, deleted_at, project_path, started_at,
                        ended_at
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
                    project_path: row.get(4)?,
                    started: row.get(5)?,
                    ended: row.get(6)?,
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
                 SELECT session, key FROM tombstone_item WHERE location_id = ?1 AND provider = ?2",
            )?;
            let mut rows = select.query(params![location, provider])?;
            while let Some(row) = rows.next()? {
                let session: String = row.get(0)?;
                match row.get(1)? {
                    Some(key) => drop(tombstoned.keys.insert(key, session)),
                    None => drop(tombstoned.sessions.insert(session)),
                }
            }
            Ok(tombstoned)
        })
    }

    /**
     * The files of deleted items still to be removed, item by item. A row that does not name a
     * place under `files/` as the layout makes them (an item's folder is 32 hex digits, a
     * provider's id is lower-case letters, digits and `-`) is no file of this archive's: it is
     * dropped, and said so in the log.
     */
    pub(crate) fn removals(&self) -> Result<Vec<Removal>> {
        let mut unsound = Vec::new();
        let removals = self.db.read(|c| {
            let mut removals: Vec<Removal> = Vec::new();
            let mut select = c.prepare(
                "SELECT rowid, location_id, provider, folder, version, offset, length, hash
                 FROM removal ORDER BY location_id, provider, folder",
            )?;
            let mut rows = select.query([])?;
            while let Some(row) = rows.next()? {
                let (row_id, location, provider, folder): (i64, LocationId, String, String) =
                    (row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?);
                let named = |text: &str, allowed: fn(u8) -> bool| {
                    !text.is_empty() && text.bytes().all(allowed)
                };
                let sound = named(location.as_str(), |byte| byte.is_ascii_alphanumeric())
                    && named(&provider, |byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
                    && folder.len() == 32
                    && named(&folder, |byte| {
                        byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
                    });
                if !sound {
                    unsound.push(row_id);
                    continue;
                }
                let file = match (row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?) {
                    (Some(version), Some(offset), Some(length), Some(hash)) => ItemFile::Segment {
                        version,
                        offset,
                        length,
                        hash,
                    },
                    _ => ItemFile::Source,
                };
                match removals.last_mut() {
                    Some(last)
                        if (&last.location, &last.provider, &last.folder)
                            == (&location, &provider, &folder) =>
                    {
                        last.files.push((row_id, file));
                    }
                    _ => removals.push(Removal {
                        location,
                        provider,
                        folder,
                        files: vec![(row_id, file)],
                    }),
                }
            }
            Ok::<_, CatalogueError>(removals)
        })?;
        if !unsound.is_empty() {
            log::warn!(
                "{} pending removals name no file of the archive",
                unsound.len()
            );
            self.removed(&unsound)?;
        }
        Ok(removals)
    }

    /// How many deleted items' folders are still to be removed.
    pub(crate) fn pending_removals(&self) -> Result<usize> {
        self.db.read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM (SELECT DISTINCT location_id, provider, folder FROM removal)",
                [],
                |row| row.get(0),
            )?)
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

    /// The pending removals `rows` (of `removal`) are done, or are not to be done.
    pub(crate) fn removed(&self, rows: &[i64]) -> Result<()> {
        self.db.write(|tx| {
            let mut done = tx.prepare("DELETE FROM removal WHERE rowid = ?1")?;
            for row in rows {
                done.execute([row])?;
            }
            Ok(())
        })
    }
}

/// When the session went to Trash, if it is there.
pub(super) fn trashed_at(connection: &rusqlite::Connection, id: &SessionId) -> Result<Option<i64>> {
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

/// What deleting the session in Trash `id` for good removes, from its items alone: its index
/// row may be missing.
fn deletion(connection: &rusqlite::Connection, id: &SessionId) -> Result<Deletion> {
    if trashed_at(connection, id)?.is_none() {
        return Err(CatalogueError::NotInTrash(id.clone()));
    }
    let (files, bytes, at_location, versions): (usize, u64, bool, usize) = connection.query_row(
        "SELECT count(*),
                COALESCE(sum((SELECT sum(length) FROM version WHERE item_id = i.id)), 0),
                COALESCE(max(i.gone_since IS NULL), 0),
                COALESCE(sum(CASE WHEN i.kind = 'transcript'
                             THEN (SELECT count(*) FROM version WHERE item_id = i.id) END), 0)
         FROM item i WHERE i.location_id = ?1 AND i.provider = ?2 AND i.session = ?3",
        params![id.location, id.provider, id.key],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    if files == 0 {
        return Err(CatalogueError::NoSession(id.clone()));
    }
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
    /// skips beside `archived`, after the session `a` with the item `a1` was deleted; and the
    /// tombstones it met.
    fn skipped(scanned: &[(&str, &str)], archived: &[(&str, &str)]) -> (Vec<String>, usize) {
        let tombstoned = Tombstoned {
            keys: HashMap::from([("a1".to_string(), "a".to_string())]),
            sessions: HashSet::from(["a".to_string()]),
        };
        let scanned: Vec<ScannedItem> = scanned
            .iter()
            .map(|(key, session)| self::scanned(key, session))
            .collect();
        let (skips, met) = tombstoned.skips(
            &scanned,
            archived.iter().map(|(key, session)| (*key, Some(*session))),
        );
        let skipped = scanned
            .into_iter()
            .zip(skips)
            .filter(|(_, skip)| *skip)
            .map(|(item, _)| item.key)
            .collect();
        assert!(met.iter().all(|met| met == "a"));
        (skipped, met.len())
    }

    #[test]
    fn a_tombstone_skips_its_keys_and_new_items_of_its_session_unless_one_is_archived() {
        /* Its item, and a new one of its session; never another session's. */
        assert_eq!(
            skipped(&[("a1", "a"), ("a2", "a"), ("b1", "b")], &[("b1", "b")]),
            (vec!["a1".to_string(), "a2".to_string()], 1)
        );
        /* Its transcript is gone at the source, and a new item of it appears. */
        assert_eq!(
            skipped(&[("a2", "a"), ("b1", "b")], &[("b1", "b")]),
            (vec!["a2".to_string()], 1)
        );
        /* A classifier gave it another id: its item is found by its key, and the new one by
        the session that item is of now. */
        assert_eq!(
            skipped(&[("a1", "a'"), ("a2", "a'"), ("b1", "b")], &[("b1", "b")]),
            (vec!["a1".to_string(), "a2".to_string()], 1)
        );
        /* A classifier merged it with a session the archive holds: that session's backup goes
        on, and only the deleted item is skipped. */
        assert_eq!(
            skipped(&[("a1", "b"), ("b1", "b"), ("b2", "b")], &[("b1", "b")]),
            (vec!["a1".to_string()], 1)
        );
        /* A session the archive holds under the deleted id, as one merged into it is. */
        assert_eq!(
            skipped(&[("b1", "a"), ("b2", "a")], &[("b1", "a")]),
            (vec![], 0)
        );
        /* Nothing is skipped without a tombstone. */
        assert_eq!(
            Tombstoned::default().skips(&[scanned("a1", "a")], []),
            (vec![false], BTreeSet::new())
        );
    }

    /**
     * Deleting a session for good reads rows of tables that grow with the archive. Each looks
     * them up by an index, a session's items by `item_session`, and none goes through a whole
     * table of them.
     */
    #[test]
    fn deleting_finds_its_rows_by_an_index() {
        let dir = tempfile::tempdir().unwrap();
        let archive = crate::archive::Archive::create(dir.path()).unwrap();
        let statements = [
            "SELECT id, key, item_id FROM item
             WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
            "SELECT count(*),
                    COALESCE(sum((SELECT sum(length) FROM version WHERE item_id = i.id)), 0),
                    COALESCE(max(i.gone_since IS NULL), 0),
                    COALESCE(sum(CASE WHEN i.kind = 'transcript'
                                 THEN (SELECT count(*) FROM version WHERE item_id = i.id) END), 0)
             FROM item i WHERE i.location_id = ?1 AND i.provider = ?2 AND i.session = ?3",
            "SELECT v.number, s.offset, s.length, s.hash FROM version v
             JOIN segment s ON s.version_id = v.id WHERE v.item_id = ?1",
            "DELETE FROM segment WHERE version_id IN (SELECT id FROM version WHERE item_id = ?1)",
            "DELETE FROM version WHERE item_id = ?1",
            "DELETE FROM item WHERE id = ?1",
            "SELECT EXISTS (
                 SELECT 1 FROM item WHERE location_id = ?1 AND provider = ?2 AND item_id = ?3
             )",
            "SELECT session, NULL FROM tombstone WHERE location_id = ?1 AND provider = ?2
             UNION ALL
             SELECT session, key FROM tombstone_item WHERE location_id = ?1 AND provider = ?2",
            "DELETE FROM tombstone_item WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
            "SELECT id FROM session WHERE main_item = ?1",
            "DELETE FROM removal WHERE rowid = ?1",
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
                        plan.iter()
                            .all(|step| !step.starts_with("SCAN") || step == "SCAN CONSTANT ROW"),
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
