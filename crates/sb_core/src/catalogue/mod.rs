//! The catalogue (PLAN §4.3): the SQLite database, through `toolbox_store`, that records which
//! stored bytes belong to which item and version. It is `scrollback.db` in the archive's
//! folder, with `toolbox_store`'s checked backups beside it in `backups/`.
//!
//! Sync only ever adds rows and moves an item's or a version's own fields forward (PLAN §2.3):
//! what sync wrote is deleted only when the owner deletes a session for good ([`trash`]).
//!
//! The library is here too (PLAN §2.2): the owner's folders, attached paths and what they
//! decided about a session ([`folder`]), Trash and the tombstones of sessions deleted for good
//! ([`trash`]), and the derived index of each session ([`session`]), which is read through
//! [`library`].

mod batch;
mod copy_record;

pub(crate) use copy_record::Place;
mod folder;
mod kv;
mod library;
mod schema;
mod session;
mod trash;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;
use toolbox_store::rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef};
use toolbox_store::rusqlite::{self, Connection, OptionalExtension as _, Row, ToSql, params};
use toolbox_store::{Backups, Database, OpenOptions};

use crate::archive::{disk, item_folder};
use crate::filing::FolderId;
use crate::hash::Hash;
use crate::location::{Location, LocationId, LocationKind, Unreachable};
use crate::provider::{ItemKind, ScannedItem};
use crate::search::Log;
use crate::transport::Probe;

pub use batch::{Before, Restored, Undone};
pub use folder::{Folder, FolderPath, ProjectPath};
pub use library::{LibraryCounts, PathGroup, Shelf};
pub use session::{Compared, CopyOf, Filed, History, Kin, Origin, Session, SessionId, Usage};
pub(crate) use session::{Derived, Indexed, Reading, RowMade, fingerprint, made};
pub use trash::{AllowedAgain, AlsoDeleted, Deletion, Tombstone};
pub(crate) use trash::{ItemFile, Removal, Scrub};

#[derive(Debug, Error)]
pub enum CatalogueError {
    #[error(transparent)]
    Database(#[from] toolbox_store::Error),
    /// A host that is not one to hand ssh ([`crate::location::host_problem`]): nothing was
    /// added.
    #[error("{0}")]
    BadHost(String),
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
    /// Another folder in the same place has the name, whatever its case.
    #[error("a folder named {0} is there already")]
    NameTaken(String),
    /// Nothing is archived of a session of this identity.
    #[error("nothing is archived of a session {} of {} at location {}", .0.key, .0.provider, .0.location)]
    NoSession(SessionId),
    #[error("item {0} is not one of the session's items")]
    NotOfSession(i64),
    /// No item of the session has this key.
    #[error("the session has no item {0}")]
    NotAnItem(String),
    #[error("version {version} is not a version of item {item}")]
    NotOfItem { version: i64, item: i64 },
    /// A version whose stored bytes are damaged is not chosen to be read.
    #[error("version {0} is damaged")]
    Damaged(i64),
    /// The path is not attached to the folder for those locations.
    #[error("{0} is not attached to the folder")]
    NotAttached(String),
    /// Only a session in Trash is restored or deleted for good.
    #[error("the session {} of {} at location {} is not in Trash", .0.key, .0.provider, .0.location)]
    NotInTrash(SessionId),
    #[error("no session {} of {} at location {} was deleted for good", .0.key, .0.provider, .0.location)]
    NoTombstone(SessionId),
    /// What deleting the session for good removes is not what was shown to the owner: an
    /// item was added to it, or a version, since. Nothing was deleted.
    #[error("the session {} of {} at location {} changed since it was shown", .0.key, .0.provider, .0.location)]
    DeletionChanged(SessionId),
    /// A tombstone the owner removed is not put back once a sync has archived its session
    /// again: the session is live in the archive.
    #[error("the session {} of {} at location {} is archived again", .0.key, .0.provider, .0.location)]
    ArchivedAgain(SessionId),
    /// A session is deleted for good with its conversation (PLAN §4.5), which is known of every
    /// session of it once the index has made each one's row again: not yet.
    #[error("the sessions like {} of {} at location {} are still being indexed", .0.key, .0.provider, .0.location)]
    StillIndexing(SessionId),
    /// The same session archived from another location (this one's [`SessionId`]) is still
    /// in the library, out of Trash: only what is in Trash is deleted for good (D14), so it
    /// goes to Trash first.
    #[error("the same session at location {} is not in Trash", .0.location)]
    AlsoShown(SessionId),
    /// The session holds more than one transcript of its own, of which only the first is read:
    /// it is not deleted for good, as the owner could not see what that removes.
    #[error("the session {} of {} at location {} holds more than one transcript", .0.key, .0.provider, .0.location)]
    SeveralTranscripts(SessionId),
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
            Self::Register => "register",
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
            Self::Register,
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
    /// It was shorter than the copy being read when it started. It stops being held back once
    /// an index finds it as long as that copy and parsing no worse.
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
#[derive(Clone)]
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

/// How much the catalogue records ([`Catalogue::totals`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub sessions: usize,
    pub items: usize,
    pub versions: usize,
    /// What every version holds, uncompressed.
    pub bytes: u64,
}

/// A provider whose agent was found at a location (DESIGN §4.11), as its last pass there read
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderAt {
    pub provider: String,
    /// How long the agent keeps transcripts there, when it says.
    pub retention_days: Option<u32>,
    /// Where the agent keeps its data there ([`crate::provider::Source::root`]); `None` until
    /// a pass records it.
    pub root: Option<String>,
}

/// What a WSL or SSH location's shell said the last time it was tested (DESIGN §4.11).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastCheck {
    /// When, in Unix milliseconds.
    pub at: i64,
    /// The `sh` that answered ([`Probe::shell`]).
    pub shell: String,
    /// The login shell, when it answered ([`Probe::login_shell`]).
    pub login_shell: Option<String>,
    /// The tools it lacks ([`Probe::missing`]).
    pub missing: Vec<String>,
    /// How long the test took, in milliseconds.
    pub took_ms: u64,
}

impl LastCheck {
    /// What `probe` says, taken now.
    pub fn of(probe: &Probe) -> Self {
        Self {
            at: now_ms(),
            shell: probe.shell.clone(),
            login_shell: probe.login_shell.clone(),
            missing: probe.missing.clone(),
            took_ms: u64::try_from(probe.took.as_millis()).unwrap_or(u64::MAX),
        }
    }
}

/// What the archive holds from one agent at one location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holding {
    pub location: LocationId,
    pub provider: String,
    /// Its sessions in the index, those in Trash among them. Copies kept here of sessions that
    /// ran elsewhere are not its sessions (PLAN §7.7), and are not counted.
    pub sessions: usize,
    /// Of those, the sessions in Trash.
    pub trashed: usize,
    /// Its items' lengths, each item's newest version once.
    pub bytes: u64,
    /// When the first of its versions was archived, in Unix milliseconds.
    pub since: Option<i64>,
}

/// The catalogue's file in the archive's folder.
pub(crate) const DATABASE: &str = "scrollback.db";

const ITEM: &str = "i.id, i.key, i.kind, i.session, i.seen_length, i.seen_stamp, i.gone_since";
const VERSION: &str = "v.id, v.number, v.reason, v.started_at, v.length, v.hash, v.held_back, \
                       v.held_by_parse, v.damaged, v.export_version";
const LOCATION: &str = "id, kind, detail, name, last_attempt_at, last_success_at, last_error, \
                        unreachable_since, unreachable_reason, drive_mount, removed_at";

/// The open catalogue: a cheap-to-clone handle. Every method blocks on disk I/O.
#[derive(Clone, Debug)]
pub struct Catalogue {
    db: Database,
    /// Its write-ahead log, emptied after a session is deleted for good.
    log: Arc<Log>,
}

impl Catalogue {
    /// Opens the catalogue in the archive folder `dir`, creating it if missing, with a backup
    /// before any migration. Today's backup is [`Catalogue::daily_backup`]'s.
    pub(crate) fn open(dir: &Path) -> Result<Self> {
        let backups = backups(dir);
        let options = OpenOptions::new(&schema::SCHEMA).backups(&backups);
        let path = dir.join(DATABASE);
        let db = Database::open(&disk::for_sqlite(&path, disk::SQLITE_FILE), &options)?;
        /* What a write frees is overwritten (PLAN §4.5), on the one connection that writes. */
        db.write(|tx| Ok::<_, CatalogueError>(tx.pragma_update(None, "secure_delete", true)?))?;
        Ok(Self {
            db,
            log: Arc::new(Log::new(dir.join(DATABASE))),
        })
    }

    /// Empties the catalogue's write-ahead log, so what a write took out is in no file.
    /// Returns whether it did: a read may hold it ([`Log::empty`]).
    pub(crate) fn empty_log(&self) -> bool {
        self.log.empty()
    }

    /// Empties the log, if a read held it the last time it was to be emptied.
    pub(crate) fn retry_empty_log(&self) {
        self.log.retry();
    }

    /// Makes today's backup of the catalogue, unless there is one. One that can't be made is
    /// said in the log and is no reason not to go on.
    pub(crate) fn daily_backup(&self) {
        let made = backups(self.dir()).daily(self.db.path(), chrono::Local::now().date_naive());
        if let Err(error) = made {
            log::warn!("couldn't make today's backup of the catalogue: {error}");
        }
    }

    /// The archive folder the catalogue is in.
    fn dir(&self) -> &Path {
        self.db.path().parent().unwrap_or(Path::new("."))
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
        if let LocationKind::Ssh { host } = &kind
            && let Some(problem) = crate::location::host_problem(host)
        {
            return Err(CatalogueError::BadHost(problem));
        }
        let id = LocationId::new();
        let (kind_name, detail) = kind.columns();
        self.execute(
            "INSERT INTO location (id, kind, detail, name, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, kind_name, detail, name, now_ms()],
        )?;
        Ok(Location {
            id,
            kind,
            name: name.to_string(),
            last_attempt: None,
            last_success: None,
            last_error: None,
            unreachable: None,
            drive_mount: None,
            removed: None,
        })
    }

    /// Names a location `name`. Its id, and with it everything archived from it, stays.
    pub fn rename_location(&self, location: &LocationId, name: &str) -> Result<()> {
        self.execute(
            "UPDATE location SET name = ?2 WHERE id = ?1",
            params![location, name],
        )
    }

    /**
     * Removes a location (FEATURES: Locations): it is synced no more, and [`Self::locations`]
     * gives it with when it was removed. Nothing archived from it is touched: its items,
     * versions and sessions stay, filed as they were, and so does its row, which they name.
     * Nothing reaches the location itself.
     */
    pub fn remove_location(&self, location: &LocationId) -> Result<()> {
        self.execute(
            "UPDATE location SET removed_at = COALESCE(removed_at, ?2) WHERE id = ?1",
            params![location, now_ms()],
        )
    }

    /// Makes a removed location one of the owner's again, named `name`: adding its place
    /// again carries on from what was archived from it, rather than archiving it twice.
    pub fn restore_location(&self, location: &LocationId, name: &str) -> Result<()> {
        self.execute(
            "UPDATE location SET removed_at = NULL, name = ?2 WHERE id = ?1",
            params![location, name],
        )
    }

    /**
     * Records where `location` mounts the Windows drives, as a sync or the connection test
     * found it ([`crate::transport::Probe::drive_mount`]); `None` for the usual place. The
     * project paths of its sessions and the paths attached for it are read by it. When it
     * is another place than was on record, those that were read under the old one are read
     * again, from the paths as they were recorded, and the sessions are filed anew by the
     * rules (PLAN §8).
     */
    pub fn set_drive_mount(&self, location: &LocationId, mount: Option<&str>) -> Result<()> {
        self.db.write(|tx| {
            let before: Option<String> = tx
                .query_row(
                    "SELECT drive_mount FROM location WHERE id = ?1",
                    [location],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            if before.as_deref() != mount {
                tx.execute(
                    "UPDATE location SET drive_mount = ?2 WHERE id = ?1",
                    params![location, mount],
                )?;
                folder::remount(tx, location, mount)?;
            }
            Ok(())
        })
    }

    /// Where `location` mounts the Windows drives, when it is not the usual place.
    pub(crate) fn drive_mount(&self, location: &LocationId) -> Result<Option<String>> {
        self.db.read(|c| {
            Ok(c.query_row(
                "SELECT drive_mount FROM location WHERE id = ?1",
                [location],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
        })
    }

    /// Every location, oldest first, those the owner removed among them. Two added in the same
    /// millisecond keep the order they were added in (the rowid), not their ids'.
    pub fn locations(&self) -> Result<Vec<Location>> {
        self.db.read(|c| {
            Ok(c.prepare(&format!(
                "SELECT {LOCATION} FROM location ORDER BY created_at, rowid"
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

    /// A sync of `location` reached it and ended: without an error, or with `error`.
    pub(crate) fn record_outcome(&self, location: &LocationId, error: Option<&str>) -> Result<()> {
        match error {
            None => self.execute(
                "UPDATE location SET last_success_at = ?2, last_error = NULL,
                     unreachable_since = NULL, unreachable_reason = NULL
                 WHERE id = ?1",
                params![location, now_ms()],
            ),
            Some(error) => self.execute(
                "UPDATE location SET last_error = ?2,
                     unreachable_since = NULL, unreachable_reason = NULL
                 WHERE id = ?1",
                params![location, error],
            ),
        }
    }

    /// A sync could not connect to `location`, for `reason`. That is no error (PLAN §5): what
    /// the last sync that reached it left, its success or its error, stays.
    pub(crate) fn record_unreachable(&self, location: &LocationId, reason: &str) -> Result<()> {
        self.execute(
            "UPDATE location SET unreachable_since = COALESCE(unreachable_since, ?2),
                 unreachable_reason = ?3
             WHERE id = ?1",
            params![location, now_ms(), reason],
        )
    }

    /// Keeps `check` as what `location`'s shell last said, in place of what it said before.
    pub fn record_check(&self, location: &LocationId, check: &LastCheck) -> Result<()> {
        self.execute(
            "INSERT INTO location_check
                 (location_id, checked_at, shell, login_shell, missing, took_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (location_id) DO UPDATE SET checked_at = ?2, shell = ?3,
                 login_shell = ?4, missing = ?5, took_ms = ?6",
            params![
                location,
                check.at,
                check.shell,
                check.login_shell,
                check.missing.join("\n"),
                i64::try_from(check.took_ms).unwrap_or(i64::MAX)
            ],
        )
    }

    /// What `location`'s shell said the last time it was tested, if it ever was.
    pub fn last_check(&self, location: &LocationId) -> Result<Option<LastCheck>> {
        self.db.read(|c| {
            Ok(c.query_row(
                "SELECT checked_at, shell, login_shell, missing, took_ms FROM location_check
                 WHERE location_id = ?1",
                [location],
                |row| {
                    let missing: String = row.get(3)?;
                    let took: i64 = row.get(4)?;
                    Ok(LastCheck {
                        at: row.get(0)?,
                        shell: row.get(1)?,
                        login_shell: row.get(2)?,
                        missing: missing
                            .lines()
                            .filter(|tool| !tool.is_empty())
                            .map(str::to_string)
                            .collect(),
                        took_ms: u64::try_from(took).unwrap_or_default(),
                    })
                },
            )
            .optional()?)
        })
    }

    /// The providers whose agents were found at `location`, with where each keeps its data
    /// there and for how long, as the last pass of each read them.
    pub fn providers_at(&self, location: &LocationId) -> Result<Vec<ProviderAt>> {
        self.db.read(|c| {
            Ok(c.prepare(
                "SELECT provider, retention_days, root FROM location_provider
                 WHERE location_id = ?1 ORDER BY provider",
            )?
            .query_map([location], |row| {
                Ok(ProviderAt {
                    provider: row.get(0)?,
                    retention_days: row.get(1)?,
                    root: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
        })
    }

    /**
     * What the archive holds from each agent at each location it holds anything from
     * (DESIGN §4.11), by location and provider. Sessions in Trash are held, and counted, as
     * well as among those in Trash; a session deleted for good is held no more, and neither
     * its items nor its row are counted. Each item counts once, at the length of its newest
     * version: what its source held when it was last archived.
     */
    pub fn holdings(&self) -> Result<Vec<Holding>> {
        self.db.read(|c| {
            let mut holdings: Vec<Holding> = c
                .prepare(
                    "SELECT i.location_id, i.provider,
                         COALESCE(SUM((SELECT v.length FROM version v WHERE v.item_id = i.id
                                       ORDER BY v.number DESC LIMIT 1)), 0),
                         MIN((SELECT MIN(v.started_at) FROM version v WHERE v.item_id = i.id))
                     FROM item i
                     GROUP BY i.location_id, i.provider
                     ORDER BY i.location_id, i.provider",
                )?
                .query_map([], |row| {
                    Ok(Holding {
                        location: row.get(0)?,
                        provider: row.get(1)?,
                        sessions: 0,
                        trashed: 0,
                        bytes: row.get(2)?,
                        since: row.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            let mut counts = c.prepare(
                "SELECT s.location_id, s.provider, COUNT(*), COUNT(st.trashed_at) FROM session s
                 LEFT JOIN session_state st ON st.location_id = s.location_id
                     AND st.provider = s.provider AND st.session = s.session
                 WHERE s.copy_label IS NULL
                 GROUP BY s.location_id, s.provider",
            )?;
            for counted in counts.query_map([], |row| {
                Ok((
                    row.get::<_, LocationId>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, usize>(2)?,
                    row.get::<_, usize>(3)?,
                ))
            })? {
                let (location, provider, sessions, trashed) = counted?;
                if let Some(holding) = holdings
                    .iter_mut()
                    .find(|it| it.location == location && it.provider == provider)
                {
                    holding.sessions = sessions;
                    holding.trashed = trashed;
                }
            }
            Ok(holdings)
        })
    }

    /// How much the catalogue records (DESIGN §4.13): its sessions as the library shows them
    /// (PLAN §7.7), items and versions, and what the versions hold uncompressed.
    pub fn totals(&self) -> Result<Totals> {
        self.db.read(|c| {
            Ok(c.query_row(
                &format!(
                    "SELECT (SELECT COUNT(*) FROM session s WHERE {}), (SELECT COUNT(*) FROM item),
                            COUNT(*), COALESCE(SUM(length), 0)
                     FROM version",
                    session::SHOWN
                ),
                [],
                |row| {
                    Ok(Totals {
                        sessions: row.get(0)?,
                        items: row.get(1)?,
                        versions: row.get(2)?,
                        bytes: row.get(3)?,
                    })
                },
            )?)
        })
    }

    pub(crate) fn record_provider(
        &self,
        location: &LocationId,
        provider: &str,
        retention_days: Option<u32>,
        root: &str,
    ) -> Result<()> {
        self.execute(
            "INSERT INTO location_provider (location_id, provider, retention_days, root)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (location_id, provider)
             DO UPDATE SET retention_days = excluded.retention_days, root = excluded.root",
            params![location, provider, retention_days, root],
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
     * itself held back. It stays held back while it grows: the index, which parses it, says
     * when it is no loss any more ([`Catalogue::set_hold`]). A version that starts also ends
     * the owner's choice of an earlier one to read ([`Catalogue::set_reading_version`]).
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
                            &format!("SELECT {}", session::copy("length", "?1", None)),
                            [item],
                            |row| row.get(0),
                        )?;
                        let held = scanned.kind.is_transcript() && commit.length < reading;
                        tx.execute("DELETE FROM reading_choice WHERE item_id = ?1", [item])?;
                        (start(item, commit.number, reason, held)?, held)
                    }
                    Target::Append { item, version } => {
                        seen(tx, item, scanned)?;
                        /* A version that is held back stays so however it grows: whether it
                        is still a loss is for the index to say, which parses it. */
                        let extended = tx.execute(
                            "UPDATE version SET length = ?2, hash = ?3
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

    /**
     * Marks each of `versions` as damaged, or as damaged no more, if it is still the version
     * that was read: its id is still of the version with that number of the item with that
     * location, provider and key. One deleted since, whose id another version may have now,
     * is left alone. Returns, for each, whether it was still there.
     */
    pub(crate) fn mark_as_read(&self, versions: &[(&Stored, bool)]) -> Result<Vec<bool>> {
        self.db.write(|tx| {
            let mut update = tx.prepare(
                "UPDATE version SET damaged = ?2
                 WHERE id = ?1 AND number = ?3 AND item_id = (
                     SELECT id FROM item WHERE location_id = ?4 AND provider = ?5 AND key = ?6)",
            )?;
            let mut unchoose = tx.prepare("DELETE FROM reading_choice WHERE version_id = ?1")?;
            let mut there = Vec::new();
            for (stored, damaged) in versions {
                let id = stored.version.id;
                let marked = update.execute(params![
                    id,
                    damaged,
                    stored.version.number,
                    stored.location,
                    stored.provider,
                    stored.key
                ])? > 0;
                if marked && *damaged {
                    unchoose.execute([id])?;
                }
                there.push(marked);
            }
            Ok(there)
        })
    }

    /// Marks `versions` as damaged, or as damaged no more. A damaged version is not read by
    /// the owner's choice: a choice of one ends here.
    pub(crate) fn set_damaged(&self, versions: &[i64], damaged: bool) -> Result<()> {
        self.db.write(|tx| {
            let mut update = tx.prepare("UPDATE version SET damaged = ?2 WHERE id = ?1")?;
            let mut unchoose = tx.prepare("DELETE FROM reading_choice WHERE version_id = ?1")?;
            for version in versions {
                update.execute(params![version, damaged])?;
                if damaged {
                    unchoose.execute([version])?;
                }
            }
            Ok(())
        })
    }
}

/// Records that `item` was found at its source as `scanned`: what it is, and its length and
/// stamp. An item that was gone is back.
fn seen(connection: &Connection, item: i64, scanned: &ScannedItem) -> rusqlite::Result<usize> {
    session::classify(connection, item, scanned.kind, scanned.session.as_deref())?;
    connection.execute(
        "UPDATE item SET seen_length = ?2, seen_stamp = ?3, gone_since = NULL WHERE id = ?1",
        params![item, scanned.length, scanned.stamp],
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
    let kind: String = row.get(1)?;
    Ok(Location {
        id: row.get(0)?,
        kind: LocationKind::from_columns(&kind, row.get(2)?)
            .ok_or_else(|| FromSqlError::Other(format!("no location is a {kind}").into()))?,
        name: row.get(3)?,
        last_attempt: row.get(4)?,
        last_success: row.get(5)?,
        last_error: row.get(6)?,
        unreachable: match (row.get(7)?, row.get(8)?) {
            (Some(since), Some(reason)) => Some(Unreachable { since, reason }),
            _ => None,
        },
        drive_mount: row.get(9)?,
        removed: row.get(10)?,
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
pub(crate) fn backups(dir: &Path) -> Backups {
    let backups = dir.join("backups");
    Backups::new(
        disk::for_sqlite(&backups, disk::SQLITE_FOLDER).into_owned(),
        "scrollback",
    )
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
