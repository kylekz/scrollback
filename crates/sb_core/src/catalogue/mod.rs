//! The catalogue (PLAN §4.3): the SQLite database, through `toolbox_store`, that records which
//! stored bytes belong to which item and version. It is `scrollback.db` in the archive's
//! folder, with `toolbox_store`'s checked backups beside it in `backups/`.
//!
//! Sync only ever adds rows and moves an item's or a version's own fields forward (PLAN §2.3):
//! nothing here deletes what sync wrote.
//!
//! The library is here too (PLAN §2.2): the owner's folders, attached paths and what they
//! decided about a session ([`folder`]), and the derived index of each session ([`session`]),
//! which is read through [`library`].

mod folder;
mod library;
mod schema;
mod session;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;
use toolbox_store::rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef};
use toolbox_store::rusqlite::{self, Connection, OptionalExtension as _, Row, ToSql, params};
use toolbox_store::{Backups, Database, OpenOptions};

use crate::archive::item_folder;
use crate::filing::FolderId;
use crate::hash::Hash;
use crate::location::{Location, LocationId, LocationKind};
use crate::provider::{ItemKind, ScannedItem};

pub use folder::{Folder, FolderPath};
pub use library::{LibraryCounts, PathGroup, Shelf};
pub(crate) use session::{Derived, Indexed, Reading, fingerprint};
pub use session::{Filed, Session, SessionId, Usage};

#[derive(Debug, Error)]
pub enum CatalogueError {
    #[error(transparent)]
    Database(#[from] toolbox_store::Error),
    /// A range was to be recorded as the continuation of a version that is not as long as the
    /// pass that fetched the range found it. Nothing was recorded.
    #[error("version {version} is no longer {length} bytes long, so nothing is appended to it")]
    VersionMoved { version: i64, length: u64 },
    /// A path that names no directory (an empty one, `/`) would claim every session.
    #[error("{0:?} names no directory, so it is not attached to a folder")]
    NoPath(String),
    /// The path is attached already for the same locations (PLAN §8): to `folder`, as `typed`.
    #[error("{typed} is already attached to the folder {name}")]
    PathAttached {
        folder: FolderId,
        name: String,
        typed: String,
    },
    #[error("a folder is not moved into itself or into a folder inside it")]
    FolderInItself,
    #[error("there is no folder {}", .0.0)]
    NoFolder(FolderId),
    /// A folder's name is some text, of at most [`FOLDER_NAME_CHARS`] characters.
    #[error("a folder's name is not empty and not longer than {FOLDER_NAME_CHARS} characters")]
    FolderName,
    /// Nothing is archived of a session of this identity.
    #[error("nothing is archived of a session {} of {} at location {}", .0.key, .0.provider, .0.location)]
    NoSession(SessionId),
    #[error("item {0} is not one of the session's items")]
    NotOfSession(i64),
    #[error("version {version} is not a version of item {item}")]
    NotOfItem { version: i64, item: i64 },
}

/// The most characters a folder's name has.
pub const FOLDER_NAME_CHARS: usize = 200;

impl From<rusqlite::Error> for CatalogueError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error.into())
    }
}

type Result<T, E = CatalogueError> = std::result::Result<T, E>;

/// Why a version started (PLAN §4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The item's first version.
    FirstSync,
    /// Earlier bytes changed at the source.
    Rewritten,
    /// The source became shorter than what was archived.
    Shrank,
    /// The item was gone from the source and came back as something else, or its provider's
    /// export format changed (PLAN §7.2).
    Replaced,
    /// The archived bytes of the version before it were found damaged, so the source was
    /// fetched again whole (PLAN §4.4).
    Refetched,
}

impl Reason {
    fn as_str(self) -> &'static str {
        match self {
            Self::FirstSync => "first_sync",
            Self::Rewritten => "rewritten",
            Self::Shrank => "shrank",
            Self::Replaced => "replaced",
            Self::Refetched => "refetched",
        }
    }
}

impl ToSql for Reason {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for Reason {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        [
            Self::FirstSync,
            Self::Rewritten,
            Self::Shrank,
            Self::Replaced,
            Self::Refetched,
        ]
        .into_iter()
        .find(|reason| reason.as_str() == text)
        .ok_or(FromSqlError::InvalidType)
    }
}

impl ItemKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Transcript => "transcript",
            Self::Subagent => "subagent",
            Self::ToolResult => "tool_result",
            Self::Sidecar => "sidecar",
        }
    }
}

impl ToSql for ItemKind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for ItemKind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        [
            Self::Transcript,
            Self::Subagent,
            Self::ToolResult,
            Self::Sidecar,
        ]
        .into_iter()
        .find(|kind| kind.as_str() == text)
        .ok_or(FromSqlError::InvalidType)
    }
}

/// An archived item (PLAN §2.1). Times are Unix milliseconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub id: i64,
    pub key: String,
    pub kind: ItemKind,
    pub session: Option<String>,
    /// The source's length and stamp the last time a pass fully dealt with the item.
    pub seen_length: u64,
    pub seen_stamp: String,
    /// When the item was first found missing at its source, while it still is.
    pub gone_since: Option<i64>,
}

/// One continuous history of an item's bytes (PLAN §4.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub id: i64,
    /// From 1.
    pub number: u32,
    pub reason: Reason,
    pub started_at: i64,
    /// The length and SHA-256 of the content so far.
    pub length: u64,
    pub hash: Hash,
    /// Why the version is not the reading copy unless the owner chooses it, when it is a loss
    /// beside the copy being read (PLAN §4.2). Only a transcript's version is held back.
    pub held_back: Option<Held>,
    /// Verify, or a sync extending it, found its stored bytes missing or changed.
    pub damaged: bool,
    pub export_version: Option<u32>,
}

/// Why a version is held back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Held {
    /// It is shorter than the copy being read. It stops being held back once it has grown to
    /// that copy's length.
    Shorter,
    /// Its transcript no longer parses where the copy being read does. It stays held back
    /// however long it grows, until an index finds it parses as well again.
    ParsesWorse,
}

/// One stored byte range of a version (PLAN §4.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub offset: u64,
    pub length: u64,
    /// The SHA-256 of the segment's uncompressed bytes.
    pub hash: Hash,
    pub stored_at: i64,
}

/// An item with its newest version: what a pass compares the source with.
pub(crate) struct Archived {
    pub item: Item,
    pub current: Version,
}

/// A version and where its segments are, for verify and for reading it back.
pub(crate) struct Stored {
    pub location: LocationId,
    pub provider: String,
    pub key: String,
    /// The item's folder name under `files/`.
    pub folder: String,
    pub version: Version,
    pub segments: Vec<Segment>,
}

/// Which version a commit writes to.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Target {
    /// Version 1 of an item the catalogue doesn't have.
    NewItem,
    NewVersion {
        item: i64,
        reason: Reason,
    },
    Append {
        item: i64,
        version: i64,
    },
}

/// What storing one range of an item changes in the catalogue (PLAN §4.1). The segments the
/// range was stored as are recorded with it.
pub(crate) struct Commit<'a> {
    pub location: &'a LocationId,
    pub provider: &'a str,
    /// The item as the pass's scan found it. Its length and stamp become the item's seen ones.
    pub scanned: &'a ScannedItem,
    pub target: Target,
    /// The number of the version the commit writes to.
    pub number: u32,
    /// The version's length before the range, and its length and hash with it.
    pub from: u64,
    pub length: u64,
    pub hash: Hash,
}

/// A commit with the segments its range was stored as, now durable on disk: for each, its
/// offset in the version, its length and its hash.
pub(crate) struct Recorded<'a> {
    pub commit: Commit<'a>,
    pub segments: Vec<(u64, u64, Hash)>,
}

/// An item a pass could not archive, and since when it has been waiting (PLAN §5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Waiting {
    pub key: String,
    /// When a pass first could not archive it, in Unix milliseconds.
    pub since: i64,
    /// Why the last pass could not.
    pub reason: String,
}

/// The catalogue's file in the archive's folder.
pub(crate) const DATABASE: &str = "scrollback.db";

const ITEM: &str = "i.id, i.key, i.kind, i.session, i.seen_length, i.seen_stamp, i.gone_since";
const VERSION: &str = "v.id, v.number, v.reason, v.started_at, v.length, v.hash, v.held_back, \
                       v.held_by_parse, v.damaged, v.export_version";
const LOCATION: &str = "id, kind, name, last_attempt_at, last_success_at, last_error";

/// The open catalogue: a cheap-to-clone handle. Every method blocks on disk I/O.
#[derive(Clone, Debug)]
pub struct Catalogue {
    db: Database,
}

impl Catalogue {
    /// Opens the catalogue in the archive folder `dir`, creating it if missing, with today's
    /// backup and one before any migration.
    pub(crate) fn open(dir: &Path) -> Result<Self> {
        let backups = backups(dir);
        let options = OpenOptions::new(&schema::SCHEMA)
            .backups(&backups)
            .daily_backup(chrono::Local::now().date_naive());
        Ok(Self {
            db: Database::open(&dir.join(DATABASE), &options)?,
        })
    }

    /// Whether anything is archived.
    pub(crate) fn has_items(&self) -> Result<bool> {
        self.db
            .read(|c| Ok(c.query_row("SELECT EXISTS (SELECT 1 FROM item)", [], |row| row.get(0))?))
    }

    /// Runs one statement as a write of its own.
    fn execute(&self, sql: &str, params: impl rusqlite::Params) -> Result<()> {
        self.db.write(|tx| Ok(tx.execute(sql, params).map(drop)?))
    }

    /// Adds a location to pull from.
    pub fn add_location(&self, kind: LocationKind, name: &str) -> Result<Location> {
        let id = LocationId::new();
        self.execute(
            "INSERT INTO location (id, kind, name, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, kind, name, now_ms()],
        )?;
        Ok(Location {
            id,
            kind,
            name: name.to_string(),
            last_attempt: None,
            last_success: None,
            last_error: None,
        })
    }

    /// Every location, oldest first.
    pub fn locations(&self) -> Result<Vec<Location>> {
        self.db.read(|c| {
            Ok(c.prepare(&format!(
                "SELECT {LOCATION} FROM location ORDER BY created_at, id"
            ))?
            .query_map([], location_from)?
            .collect::<rusqlite::Result<_>>()?)
        })
    }

    pub fn location(&self, id: &LocationId) -> Result<Option<Location>> {
        self.db.read(|c| {
            Ok(c.query_row(
                &format!("SELECT {LOCATION} FROM location WHERE id = ?1"),
                [id],
                location_from,
            )
            .optional()?)
        })
    }

    /// The providers whose agents were found at `location`, each with the retention last read.
    pub fn providers_found(&self, location: &LocationId) -> Result<Vec<(String, Option<u32>)>> {
        self.db.read(|c| {
            Ok(c.prepare(
                "SELECT provider, retention_days FROM location_provider
                 WHERE location_id = ?1 ORDER BY provider",
            )?
            .query_map([location], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
        })
    }

    pub(crate) fn record_attempt(&self, location: &LocationId) -> Result<()> {
        self.execute(
            "UPDATE location SET last_attempt_at = ?2 WHERE id = ?1",
            params![location, now_ms()],
        )
    }

    /// A sync of `location` ended: without an error, or with `error`.
    pub(crate) fn record_outcome(&self, location: &LocationId, error: Option<&str>) -> Result<()> {
        match error {
            None => self.execute(
                "UPDATE location SET last_success_at = ?2, last_error = NULL WHERE id = ?1",
                params![location, now_ms()],
            ),
            Some(error) => self.execute(
                "UPDATE location SET last_error = ?2 WHERE id = ?1",
                params![location, error],
            ),
        }
    }

    pub(crate) fn record_provider(
        &self,
        location: &LocationId,
        provider: &str,
        retention_days: Option<u32>,
    ) -> Result<()> {
        self.execute(
            "INSERT INTO location_provider (location_id, provider, retention_days)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (location_id, provider)
             DO UPDATE SET retention_days = excluded.retention_days",
            params![location, provider, retention_days],
        )
    }

    pub fn item(&self, location: &LocationId, provider: &str, key: &str) -> Result<Option<Item>> {
        self.db.read(|c| {
            Ok(c.query_row(
                &format!(
                    "SELECT {ITEM} FROM item i
                     WHERE i.location_id = ?1 AND i.provider = ?2 AND i.key = ?3"
                ),
                params![location, provider, key],
                |row| item_from(row, 0),
            )
            .optional()?)
        })
    }

    /// Every item archived from `provider` at `location`, by key.
    pub fn items(&self, location: &LocationId, provider: &str) -> Result<Vec<Item>> {
        self.db.read(|c| {
            Ok(c.prepare(&format!(
                "SELECT {ITEM} FROM item i
                 WHERE i.location_id = ?1 AND i.provider = ?2 ORDER BY i.key"
            ))?
            .query_map(params![location, provider], |row| item_from(row, 0))?
            .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// An item's versions, oldest first.
    pub fn versions(&self, item: i64) -> Result<Vec<Version>> {
        self.db.read(|c| {
            Ok(c.prepare(&format!(
                "SELECT {VERSION} FROM version v WHERE v.item_id = ?1 ORDER BY v.number"
            ))?
            .query_map([item], |row| version_from(row, 0))?
            .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// A version's segments, in order.
    pub fn segments(&self, version: i64) -> Result<Vec<Segment>> {
        self.db.read(|c| segments_of(c, version))
    }

    /// Every item of `provider` at `location` with its newest version.
    pub(crate) fn archived(&self, location: &LocationId, provider: &str) -> Result<Vec<Archived>> {
        self.db.read(|c| {
            Ok(c.prepare(&format!(
                "SELECT {ITEM}, {VERSION} FROM item i
                 JOIN version v ON v.item_id = i.id
                  AND v.number = (SELECT max(number) FROM version WHERE item_id = i.id)
                 WHERE i.location_id = ?1 AND i.provider = ?2"
            ))?
            .query_map(params![location, provider], |row| {
                Ok(Archived {
                    item: item_from(row, 0)?,
                    current: version_from(row, 7)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Every version in the archive with its segments, in one snapshot.
    pub(crate) fn stored(&self) -> Result<Vec<Stored>> {
        self.db.read(|c| {
            let mut segments: HashMap<i64, Vec<Segment>> = HashMap::new();
            let mut select = c.prepare(
                "SELECT version_id, offset, length, hash, stored_at FROM segment
                 ORDER BY version_id, offset",
            )?;
            let mut rows = select.query([])?;
            while let Some(row) = rows.next()? {
                segments
                    .entry(row.get(0)?)
                    .or_default()
                    .push(segment_from(row, 1)?);
            }
            Ok(c.prepare(&format!(
                "SELECT i.location_id, i.provider, i.key, i.item_id, {VERSION}
                 FROM version v JOIN item i ON i.id = v.item_id ORDER BY v.id"
            ))?
            .query_map([], |row| {
                let stored = stored_from(row)?;
                Ok(Stored {
                    segments: segments.remove(&stored.version.id).unwrap_or_default(),
                    ..stored
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// One version and where its segments are.
    pub(crate) fn stored_version(&self, version: i64) -> Result<Option<Stored>> {
        self.db.read(|c| {
            let stored = c
                .query_row(
                    &format!(
                        "SELECT i.location_id, i.provider, i.key, i.item_id, {VERSION}
                         FROM version v JOIN item i ON i.id = v.item_id WHERE v.id = ?1"
                    ),
                    [version],
                    stored_from,
                )
                .optional()?;
            Ok(match stored {
                Some(stored) => Some(Stored {
                    segments: segments_of(c, version)?,
                    ..stored
                }),
                None => None,
            })
        })
    }

    /// Whether the catalogue records a segment at `offset` of version `number` of the item in
    /// `folder`.
    pub(crate) fn has_segment(
        &self,
        location: &LocationId,
        provider: &str,
        folder: &str,
        number: u32,
        offset: u64,
    ) -> Result<bool> {
        self.db.read(|c| {
            Ok(c.query_row(
                "SELECT EXISTS (
                     SELECT 1 FROM segment s
                     JOIN version v ON v.id = s.version_id
                     JOIN item i ON i.id = v.item_id
                     WHERE i.location_id = ?1 AND i.provider = ?2 AND i.item_id = ?3
                       AND v.number = ?4 AND s.offset = ?5
                 )",
                params![location, provider, folder, number, offset],
                |row| row.get(0),
            )?)
        })
    }

    /**
     * Records stored ranges, each with its segments and the source state it brings its item up
     * to, all in one transaction.
     * Returns for each whether a version it starts is held back (PLAN §4.2): it is a
     * transcript's and is shorter than the copy being read, which is the newest version not
     * itself held back. A held-back version that grows to that copy's length is no longer a
     * loss and stops being held back, unless it is held back for how it parses
     * ([`Catalogue::hold_for_parse`]). A version that starts also ends the owner's choice of
     * an earlier one to read ([`Catalogue::set_reading_version`]).
     */
    pub(crate) fn commit(&self, commits: &[Recorded<'_>]) -> Result<Vec<bool>> {
        let now = now_ms();
        self.db.write(|tx| {
            let mut held_back = Vec::new();
            for Recorded { commit, segments } in commits {
                let scanned = commit.scanned;
                /* It is archived: whatever a pass later leaves it waiting for starts anew. */
                tx.execute(
                    "DELETE FROM waiting WHERE location_id = ?1 AND provider = ?2 AND key = ?3",
                    params![commit.location, commit.provider, scanned.key],
                )?;
                let start = |item: i64, number: u32, reason: Reason, held_back: bool| {
                    tx.execute(
                        "INSERT INTO version (item_id, number, reason, started_at, length, hash,
                                              held_back, export_version)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![
                            item,
                            number,
                            reason,
                            now,
                            commit.length,
                            commit.hash,
                            held_back,
                            scanned.export_version
                        ],
                    )?;
                    Ok::<_, rusqlite::Error>(tx.last_insert_rowid())
                };
                let (version, held) = match commit.target {
                    Target::NewItem => {
                        tx.execute(
                            "INSERT INTO item (location_id, provider, key, item_id, kind, session,
                                               seen_length, seen_stamp)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                            params![
                                commit.location,
                                commit.provider,
                                scanned.key,
                                item_folder(&scanned.key),
                                scanned.kind,
                                scanned.session,
                                scanned.length,
                                scanned.stamp
                            ],
                        )?;
                        let item = tx.last_insert_rowid();
                        (start(item, 1, Reason::FirstSync, false)?, false)
                    }
                    Target::NewVersion { item, reason } => {
                        seen(tx, item, scanned)?;
                        let reading: u64 = tx.query_row(
                            "SELECT length FROM version WHERE item_id = ?1 AND held_back = 0
                             ORDER BY number DESC LIMIT 1",
                            [item],
                            |row| row.get(0),
                        )?;
                        let held = scanned.kind.is_transcript() && commit.length < reading;
                        tx.execute("DELETE FROM reading_choice WHERE item_id = ?1", [item])?;
                        (start(item, commit.number, reason, held)?, held)
                    }
                    Target::Append { item, version } => {
                        seen(tx, item, scanned)?;
                        let extended = tx.execute(
                            "UPDATE version SET length = ?2, hash = ?3,
                                 held_back = held_by_parse OR COALESCE(held_back AND ?2 < (
                                     SELECT length FROM version reading
                                     WHERE reading.item_id = version.item_id
                                       AND reading.held_back = 0
                                       AND reading.number < version.number
                                     ORDER BY reading.number DESC LIMIT 1
                                 ), 0)
                             WHERE id = ?1 AND length = ?4",
                            params![version, commit.length, commit.hash, commit.from],
                        )?;
                        if extended == 0 {
                            return Err(CatalogueError::VersionMoved {
                                version,
                                length: commit.from,
                            });
                        }
                        (version, false)
                    }
                };
                for (offset, length, hash) in segments {
                    tx.execute(
                        "INSERT INTO segment (version_id, offset, length, hash, stored_at)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![version, offset, length, hash, now],
                    )?;
                }
                held_back.push(held);
            }
            Ok(held_back)
        })
    }

    /// What a pass found without storing anything: `seen` items whose source holds what is
    /// archived (their stamps or what they are may have changed, and any that were gone are
    /// back), and `gone` items that are missing at the source, which keep everything
    /// (PLAN §2.3).
    pub(crate) fn record_scan(&self, seen: &[(i64, &ScannedItem)], gone: &[i64]) -> Result<()> {
        let now = now_ms();
        self.db.write(|tx| {
            for (item, scanned) in seen {
                self::seen(tx, *item, scanned)?;
            }
            let mut mark =
                tx.prepare("UPDATE item SET gone_since = ?2 WHERE id = ?1 AND gone_since IS NULL")?;
            for item in gone {
                mark.execute(params![item, now])?;
            }
            Ok(())
        })
    }

    /// Replaces what is recorded as waiting to be archived from `provider` at `location` with
    /// `waiting`, each a key and why. An item that was waiting already keeps since when. One
    /// that is stored stops waiting with its commit ([`Catalogue::commit`]), whatever becomes
    /// of the pass.
    pub(crate) fn record_waiting(
        &self,
        location: &LocationId,
        provider: &str,
        waiting: &[(&str, &str)],
    ) -> Result<()> {
        let now = now_ms();
        self.db.write(|tx| {
            let mut since: HashMap<String, i64> = tx
                .prepare("SELECT key, since FROM waiting WHERE location_id = ?1 AND provider = ?2")?
                .query_map(params![location, provider], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<rusqlite::Result<_>>()?;
            tx.execute(
                "DELETE FROM waiting WHERE location_id = ?1 AND provider = ?2",
                params![location, provider],
            )?;
            let mut insert = tx.prepare(
                "INSERT OR REPLACE INTO waiting (location_id, provider, key, since, reason)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (key, reason) in waiting {
                /* Not from later than now: the clock may have been ahead when it was kept. */
                let since = since.remove(*key).map_or(now, |since| since.min(now));
                insert.execute(params![location, provider, key, since, reason])?;
            }
            Ok(())
        })
    }

    /// The items of `provider` at `location` that its last pass could not archive, by key.
    pub fn waiting(&self, location: &LocationId, provider: &str) -> Result<Vec<Waiting>> {
        self.db.read(|c| {
            Ok(c.prepare(
                "SELECT key, since, reason FROM waiting
                 WHERE location_id = ?1 AND provider = ?2 ORDER BY key",
            )?
            .query_map(params![location, provider], |row| {
                Ok(Waiting {
                    key: row.get(0)?,
                    since: row.get(1)?,
                    reason: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// The id of the archive this catalogue belongs to, once it has been given one.
    pub(crate) fn archive_id(&self) -> Result<Option<String>> {
        self.db.read(|c| {
            Ok(c.query_row("SELECT id FROM archive", [], |row| row.get(0))
                .optional()?)
        })
    }

    pub(crate) fn set_archive_id(&self, id: &str) -> Result<()> {
        self.execute("INSERT INTO archive (id) VALUES (?1)", [id])
    }

    /// Whether any location was ever added.
    pub(crate) fn has_locations(&self) -> Result<bool> {
        self.db.read(|c| {
            Ok(
                c.query_row("SELECT EXISTS (SELECT 1 FROM location)", [], |row| {
                    row.get(0)
                })?,
            )
        })
    }

    pub(crate) fn set_damaged(&self, versions: &[i64], damaged: bool) -> Result<()> {
        self.db.write(|tx| {
            let mut update = tx.prepare("UPDATE version SET damaged = ?2 WHERE id = ?1")?;
            for version in versions {
                update.execute(params![version, damaged])?;
            }
            Ok(())
        })
    }
}

/// Records that `item` was found at its source as `scanned`: what it is, and its length and
/// stamp. An item that was gone is back.
fn seen(connection: &Connection, item: i64, scanned: &ScannedItem) -> rusqlite::Result<usize> {
    connection.execute(
        "UPDATE item SET kind = ?2, session = ?3, seen_length = ?4, seen_stamp = ?5,
                         gone_since = NULL
         WHERE id = ?1",
        params![
            item,
            scanned.kind,
            scanned.session,
            scanned.length,
            scanned.stamp
        ],
    )
}

fn segments_of(connection: &Connection, version: i64) -> Result<Vec<Segment>> {
    Ok(connection
        .prepare(
            "SELECT offset, length, hash, stored_at FROM segment
             WHERE version_id = ?1 ORDER BY offset",
        )?
        .query_map([version], |row| segment_from(row, 0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn location_from(row: &Row<'_>) -> rusqlite::Result<Location> {
    Ok(Location {
        id: row.get(0)?,
        kind: row.get(1)?,
        name: row.get(2)?,
        last_attempt: row.get(3)?,
        last_success: row.get(4)?,
        last_error: row.get(5)?,
    })
}

/// An item from the [`ITEM`] columns starting at column `at`.
fn item_from(row: &Row<'_>, at: usize) -> rusqlite::Result<Item> {
    Ok(Item {
        id: row.get(at)?,
        key: row.get(at + 1)?,
        kind: row.get(at + 2)?,
        session: row.get(at + 3)?,
        seen_length: row.get(at + 4)?,
        seen_stamp: row.get(at + 5)?,
        gone_since: row.get(at + 6)?,
    })
}

/// A version from the [`VERSION`] columns starting at column `at`.
fn version_from(row: &Row<'_>, at: usize) -> rusqlite::Result<Version> {
    Ok(Version {
        id: row.get(at)?,
        number: row.get(at + 1)?,
        reason: row.get(at + 2)?,
        started_at: row.get(at + 3)?,
        length: row.get(at + 4)?,
        hash: row.get(at + 5)?,
        held_back: match (row.get(at + 6)?, row.get(at + 7)?) {
            (false, _) => None,
            (true, false) => Some(Held::Shorter),
            (true, true) => Some(Held::ParsesWorse),
        },
        damaged: row.get(at + 8)?,
        export_version: row.get(at + 9)?,
    })
}

fn segment_from(row: &Row<'_>, at: usize) -> rusqlite::Result<Segment> {
    Ok(Segment {
        offset: row.get(at)?,
        length: row.get(at + 1)?,
        hash: row.get(at + 2)?,
        stored_at: row.get(at + 3)?,
    })
}

fn stored_from(row: &Row<'_>) -> rusqlite::Result<Stored> {
    Ok(Stored {
        location: row.get(0)?,
        provider: row.get(1)?,
        key: row.get(2)?,
        folder: row.get(3)?,
        version: version_from(row, 4)?,
        segments: Vec::new(),
    })
}

/// The archive's backups of its catalogue.
fn backups(dir: &Path) -> Backups {
    Backups::new(dir.join("backups"), "scrollback")
}

/// The most recent backup of the catalogue of the archive in `dir`, if there is one.
pub(crate) fn newest_backup(dir: &Path) -> Option<PathBuf> {
    backups(dir).newest()
}

/// Now, in Unix milliseconds.
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}
