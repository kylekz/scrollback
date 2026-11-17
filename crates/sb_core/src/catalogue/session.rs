//! Sessions in the catalogue (PLAN §2.1, §4.3): which version of each of a session's items is
//! read, the derived row an index writes for the session, and what tells that the row is no
//! longer of what the archive holds.
//!
//! The `session` table is derived (PLAN §2.2): [`Catalogue::drop_index`] empties it and the
//! next index fills it again from the archive alone. What the owner decided about a session is
//! in `session_state`, under the session's identity, and stays while its row here is missing.
//!
//! The reading copy of an item (PLAN §4.2) is the version the owner chose of it, while that
//! choice stands; else, of a transcript, the newest version that is not held back; else the
//! newest version. Only a transcript's version is ever a loss worth keeping from the reader:
//! of anything else an agent keeps, the newest is what it meant.

use std::cmp::Ordering;
use std::collections::HashMap;

use toolbox_store::rusqlite::types::Type;
use toolbox_store::rusqlite::{
    self, Connection, OptionalExtension as _, Row, ToSql, params, params_from_iter,
};

use super::{Catalogue, CatalogueError, Held, Result, VERSION, Version, folder, version_from};
use crate::filing::{FolderId, NormalPath};
use crate::hash::{Hash, Hasher};
use crate::location::LocationId;
use crate::provider::ItemKind;
use crate::transcript::{CopyOf, Ending, INDEX_VERSION, SessionIndex, Tokens, Totals};

/// A session's identity (PLAN §2.1): the location it was pulled from, its agent's provider,
/// and that provider's id for it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionId {
    pub location: LocationId,
    pub provider: String,
    pub key: String,
}

/// Where a session is filed (PLAN §8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filed {
    Unsorted,
    /// By its project path. The rules move it when the folders' paths change.
    ByRule(FolderId),
    /// By the owner. No rule moves it.
    ByHand(FolderId),
}

/// What one conversation or several used and did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Tool calls on the shown branches.
    pub calls: u32,
    pub tokens: Tokens,
}

/// A session as the library shows it: what its index derived from the archive, and what the
/// owner decided about it. Times are Unix milliseconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub id: SessionId,
    /// The item that is the session's own transcript; `None` when its items hold none, only
    /// what went with one.
    pub main_item: Option<i64>,
    /// The number of the version of the main transcript that is read, and of its newest.
    pub reading_version: Option<u32>,
    pub newest_version: Option<u32>,
    /// The title the agent stored, else the start of the first prompt.
    pub title: Option<String>,
    pub first_prompt: Option<String>,
    /// The directory the session started in, as the agent recorded it.
    pub project_path: Option<String>,
    /// The same, as filing compares paths.
    pub project: Option<NormalPath>,
    pub branch: Option<String>,
    pub entrypoint: Option<String>,
    pub started: Option<i64>,
    pub ended: Option<i64>,
    pub models: Vec<String>,
    /// The session's own conversation.
    pub own: Usage,
    /// The session's own conversation and its subagents'.
    pub all: Usage,
    /// How reading the main transcript ended; `None` without one.
    pub ending: Option<Ending>,
    /// Some part of the session could not be read: its own conversation, a subagent's, or
    /// something that goes with one.
    pub unread: bool,
    /// The newest version of one of the session's transcripts, its own or a subagent's, is
    /// held back as a loss (PLAN §4.2): what is read of it is an earlier version.
    pub held_back: bool,
    /// Why the last index of the session could not parse it. What else is here is then what
    /// the index before found, or nothing.
    pub failure: Option<String>,
    pub parser_version: u32,
    pub index_version: u32,
    pub filed: Filed,
    pub starred: bool,
    /// When the owner moved the session to Trash, while it is there (PLAN §4.5).
    pub trashed: Option<i64>,
    /// The session is a copy of one that ran on another machine (PLAN §7.7).
    pub copy: Option<CopyOf>,
}

impl Session {
    /**
     * The location the session ran at, as the library attributes it (PLAN §7.7): where it was
     * pulled from, unless it is a copy, which ran on a machine the copy doesn't name. It is
     * what every view that names a session's location shows, filters and files by.
     */
    pub fn ran_at(&self) -> Option<&LocationId> {
        self.copy.is_none().then_some(&self.id.location)
    }
}

/// How a session's main transcript compares with another's, by the versions of each that are
/// read: the same bytes, fewer, more, or as many but others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compared {
    Same,
    Shorter,
    Longer,
    Differs,
}

/// Another session of a session's conversation (PLAN §7.7): its original, or a copy of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kin {
    pub id: SessionId,
    /// What it is a copy as; `None` for an original.
    pub copy: Option<CopyOf>,
    /// How its main transcript compares with the session's.
    pub compared: Compared,
}

/// One item of a session with the version of it that is read (the module docs).
#[derive(Clone, Debug)]
pub(crate) struct Reading {
    pub item: i64,
    pub key: String,
    pub kind: ItemKind,
    pub version: i64,
    pub number: u32,
    pub length: u64,
    /// The version that is read is marked damaged.
    pub damaged: bool,
    /// The item's newest version, which is the one read unless it is held back or the owner
    /// chose another.
    pub newest: i64,
    pub newest_number: u32,
    pub newest_length: u64,
    pub newest_held: Option<Held>,
    /// The owner chose to read the newest version while it was held back.
    pub newest_accepted: bool,
}

/**
 * The versions of one of a session's items and the one of them that is read (PLAN §4.2): what
 * the reader's History shows ([`Catalogue::history`]).
 */
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct History {
    pub item: i64,
    /// Oldest first, each with how many syncs extended it: its segments after the first.
    pub versions: Vec<(Version, usize)>,
    /// The id of the version that is read: the owner's choice, else the rule's.
    pub reading: i64,
    /// Whether the owner chose it, in the rule's place.
    pub chosen: bool,
}

/// A session whose row is missing, or is not of what the archive holds of the session now.
pub(crate) struct Stale {
    pub id: SessionId,
    /// How much an index of it reads: its transcripts, the subagents' included.
    pub bytes: u64,
    /// What stale sessions are ordered by, the greatest first: a session with no row before
    /// any with one, and those by when they last did something.
    pub recency: i64,
}

/// What a session's row says of how it was made, and when the session ended.
struct Made {
    /// Whether it was made by the versions of the parser and of the index that there are now.
    by_these: bool,
    /// The [`fingerprint`] of the items it was made from.
    from: Hash,
    /// The archive could not be read when it was made: an index of a start tries again.
    retry: bool,
    ended: Option<i64>,
}

/// What an index derived from a session.
pub(crate) struct Derived<'a> {
    /// The item the session's own conversation was parsed from.
    pub main: Option<&'a Reading>,
    pub index: &'a SessionIndex,
    pub ending: Option<&'a Ending>,
    /// Whether the newest version of one of its transcripts is held back.
    pub held_back: bool,
}

/// What became of an index of a session.
pub(crate) enum Indexed<'a> {
    Parsed(Derived<'a>),
    /// Its parser stopped on it, for the reason given. Parsing it again as it is would end
    /// the same.
    Faulted(&'a str),
    /// The archive could not be read, for the reason given, which says nothing of what is
    /// stored: the index of a start tries again, until it can.
    NotRead(&'a str),
}

/// What tells one state of a session's items from another: what each item is, its version
/// that is read with its length and whether it is marked damaged, and its newest version with
/// its length. A row made from one state is stale in any other.
pub(crate) fn fingerprint(readings: &[Reading]) -> Hash {
    let mut hasher = Hasher::new();
    for reading in readings {
        hasher.update(
            format!(
                "{} {} {} {} {} {} {}\n",
                reading.item,
                reading.kind.as_str(),
                reading.version,
                reading.length,
                reading.damaged,
                reading.newest,
                reading.newest_length
            )
            .as_bytes(),
        );
    }
    hasher.finish()
}

/**
 * The copy the rule reads of a transcript (PLAN §4.2), as a subquery: `column` of the newest
 * version of the item `item` that is not held back, among those numbered below `before` when
 * one is given. `item` and `before` are SQL. Every place that asks what is read without a
 * later version, to say whether that version is a loss beside it, asks through this.
 */
pub(super) fn copy(column: &str, item: &str, before: Option<&str>) -> String {
    format!(
        "(SELECT {column} FROM version copy
          WHERE copy.item_id = {item} AND copy.held_back = 0 {}
          ORDER BY copy.number DESC LIMIT 1)",
        before.map_or_else(String::new, |before| format!("AND copy.number < {before}"))
    )
}

/// The number of the version the rule reads of the item `item`, whose kind is `kind` (both
/// SQL): of a transcript the [`copy`], of anything else the newest.
fn ruled(item: &str, kind: &str) -> String {
    format!(
        "COALESCE(
             CASE WHEN {kind} IN ('transcript', 'subagent') THEN {} END,
             (SELECT max(number) FROM version WHERE item_id = {item}))",
        copy("number", item, None)
    )
}

/// Every item that belongs to a session, as a [`Reading`], then its location and session.
/// What follows says which items, and is [`OF_SESSION`] or [`OF_LOCATION`].
fn reading() -> String {
    format!(
        "SELECT i.id, i.key, i.kind, r.id, r.number, r.length, r.damaged,
                n.id, n.number, n.length, n.held_back, n.held_by_parse, n.accepted,
                i.location_id, i.session
         FROM item i
         JOIN version n ON n.item_id = i.id
          AND n.number = (SELECT max(number) FROM version WHERE item_id = i.id)
         JOIN version r ON r.item_id = i.id AND r.number = COALESCE(
             (SELECT chosen.number FROM reading_choice choice
              JOIN version chosen ON chosen.id = choice.version_id
              WHERE choice.item_id = i.id),
             {})",
        ruled("i.id", "i.kind")
    )
}

/// A session's row with the owner's state of it, as [`session_from`] reads them.
pub(super) const SESSION: &str = "
    SELECT s.location_id, s.provider, s.session, s.main_item, s.reading_version,
           s.newest_version, s.title, s.first_prompt, s.project_path, s.project, s.branch,
           s.entrypoint, s.started_at, s.ended_at, s.models,
           s.own_calls, s.own_input, s.own_output, s.own_cache_read, s.own_cache_write,
           s.all_calls, s.all_input, s.all_output, s.all_cache_read, s.all_cache_write,
           s.ending, s.unreadable, s.unread, s.held_back, s.failure, s.parser_version,
           s.index_version, st.folder_id, COALESCE(st.by_hand, 0), COALESCE(st.starred, 0),
           st.trashed_at, s.copy_label, s.copy_origin
    FROM session s
    LEFT JOIN session_state st
      ON st.location_id = s.location_id AND st.provider = s.provider AND st.session = s.session";

/// What a row was made from, in one word ([`Catalogue::made`]).
const MADE_FROM: &str = "s.indexed_from || ' ' || s.parser_version || ' ' || s.index_version";

/**
 * The condition that the session `s` is one the library shows (PLAN §7.7): an original, or a
 * copy of a conversation none of whose originals is archived, and of several such copies the
 * one that did something last (then the greatest identity). The others are hidden everywhere.
 */
pub(super) const SHOWN: &str = "NOT (s.copy_label IS NOT NULL AND EXISTS (
    SELECT 1 FROM session o
    WHERE o.provider = s.provider AND o.conversation = s.conversation
      AND o.project_path IS s.project_path AND o.id <> s.id
      AND (o.copy_label IS NULL
           OR (COALESCE(o.ended_at, -9223372036854775808), o.location_id, o.session)
              > (COALESCE(s.ended_at, -9223372036854775808), s.location_id, s.session))))";

/// The location `s` is attributed to (PLAN §7.7, [`Session::ran_at`]), as SQL: NULL for a copy.
pub(super) const RAN_AT: &str = "CASE WHEN s.copy_label IS NULL THEN s.location_id END";

/// The session that did something last comes first; those with no time come last.
pub(super) const BY_ACTIVITY: &str =
    "ORDER BY s.ended_at DESC NULLS LAST, s.location_id, s.provider, s.session";

/// What a row says of the session itself, in the order [`Catalogue::record_session`] gives it.
const DERIVED: [&str; 29] = [
    "main_item",
    "reading_version",
    "newest_version",
    "title",
    "first_prompt",
    "project_path",
    "project",
    "branch",
    "entrypoint",
    "started_at",
    "ended_at",
    "models",
    "own_calls",
    "own_input",
    "own_output",
    "own_cache_read",
    "own_cache_write",
    "all_calls",
    "all_input",
    "all_output",
    "all_cache_read",
    "all_cache_write",
    "ending",
    "unreadable",
    "unread",
    "held_back",
    "conversation",
    "copy_label",
    "copy_origin",
];

/// What a row says of the index that made it, after [`DERIVED`]. These are all that an index
/// that could not parse the session sets of a row that is there.
const MADE: [&str; 6] = [
    "failure",
    "retry",
    "parser_version",
    "index_version",
    "indexed_from",
    "searchable",
];

impl Catalogue {
    /// The session `id`, once it is indexed.
    pub fn session(&self, id: &SessionId) -> Result<Option<Session>> {
        self.db.read(|c| {
            Ok(c.query_row(
                &format!(
                    "{SESSION} WHERE s.location_id = ?1 AND s.provider = ?2 AND s.session = ?3"
                ),
                params![id.location, id.provider, id.key],
                session_from,
            )
            .optional()?)
        })
    }

    /// The items of the session `id`, by key, each at the version of it that is read.
    pub(crate) fn session_items(&self, id: &SessionId) -> Result<Vec<Reading>> {
        self.db.read(|c| {
            Ok(c.prepare(&format!("{} {OF_SESSION}", reading()))?
                .query_map(params![id.location, id.provider, id.key], reading_from)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    /**
     * The sessions of `provider`, at `location` or at every location, that are to be indexed,
     * and the sessions that have a row and are none any more.
     *
     * A session is its items with at least one transcript among them, its own or a
     * subagent's. It is to be indexed when it has no row, or a row made from another state of
     * its items than there is now ([`fingerprint`]); and, with `backlog`, also when its row
     * was made by another version of the parser or of the index, or when the archive could
     * not be read.
     */
    pub(crate) fn stale(
        &self,
        location: Option<&LocationId>,
        provider: &str,
        parser_version: u32,
        backlog: bool,
    ) -> Result<(Vec<Stale>, Vec<SessionId>)> {
        self.db.read(|c| {
            let mut indexed: HashMap<(LocationId, String), Made> = c
                .prepare(
                    "SELECT location_id, session, parser_version = ?2 AND index_version = ?3,
                            indexed_from, retry, ended_at
                     FROM session WHERE provider = ?1 AND (?4 IS NULL OR location_id = ?4)",
                )?
                .query_map(
                    params![provider, parser_version, INDEX_VERSION, location],
                    |row| {
                        let made = Made {
                            by_these: row.get(2)?,
                            from: row.get(3)?,
                            retry: row.get(4)?,
                            ended: row.get(5)?,
                        };
                        Ok(((row.get(0)?, row.get(1)?), made))
                    },
                )?
                .collect::<rusqlite::Result<_>>()?;
            let mut stale = Vec::new();
            let mut close = |session: (LocationId, String), readings: &mut Vec<Reading>| {
                let read = |reading: &&Reading| reading.kind.is_transcript();
                if readings.iter().any(|reading| read(&reading)) {
                    let row = indexed.remove(&session);
                    let current = row.as_ref().is_some_and(|made| {
                        made.from == fingerprint(readings)
                            && (!backlog || (made.by_these && !made.retry))
                    });
                    if !current {
                        stale.push(Stale {
                            id: SessionId {
                                location: session.0,
                                provider: provider.to_string(),
                                key: session.1,
                            },
                            bytes: readings
                                .iter()
                                .filter(read)
                                .map(|reading| reading.length)
                                .sum(),
                            recency: row.map_or(i64::MAX, |made| made.ended.unwrap_or(i64::MIN)),
                        });
                    }
                }
                readings.clear();
            };
            /* A location at a time: its items are together in the index, session by session. */
            let locations: Vec<LocationId> = match location {
                Some(location) => vec![location.clone()],
                None => c
                    .prepare("SELECT id FROM location")?
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<_>>()?,
            };
            let mut select = c.prepare(&format!("{} {OF_LOCATION}", reading()))?;
            let mut readings = Vec::new();
            for location in locations {
                let mut rows = select.query(params![provider, location])?;
                let mut open: Option<String> = None;
                while let Some(row) = rows.next()? {
                    let session: String = row.get(14)?;
                    if let Some(last) = open.take_if(|last| *last != session) {
                        close((location.clone(), last), &mut readings);
                    }
                    open = Some(session);
                    readings.push(reading_from(row)?);
                }
                if let Some(last) = open {
                    close((location, last), &mut readings);
                }
            }
            /* The rows that are left are of sessions with no transcript among their items, or
            with no items: what they were made from is another session's now. */
            let gone = indexed
                .into_keys()
                .map(|(location, key)| SessionId {
                    location,
                    provider: provider.to_string(),
                    key,
                })
                .collect();
            Ok((stale, gone))
        })
    }

    /**
     * The sessions of `provider` (or of every provider) at `location` (or at every location)
     * whose text in the search index is not of their rows as they are now
     * (`session.searchable`), the one that did something last first, each with what its row
     * was made from: its items' fingerprint, the parser's version and the index's.
     */
    pub(crate) fn unsearchable(
        &self,
        location: Option<&LocationId>,
        provider: Option<&str>,
    ) -> Result<Vec<(SessionId, String)>> {
        self.db.read(|c| {
            Ok(c.prepare(&format!(
                "SELECT s.location_id, s.provider, s.session, {MADE_FROM} FROM session s
                 WHERE s.searchable = 0
                   AND (?1 IS NULL OR s.location_id = ?1) AND (?2 IS NULL OR s.provider = ?2)
                 {BY_ACTIVITY}"
            ))?
            .query_map(params![location, provider], |row| {
                let id = SessionId {
                    location: row.get(0)?,
                    provider: row.get(1)?,
                    key: row.get(2)?,
                };
                Ok((id, row.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// How many sessions' text is not of their rows as they are ([`Catalogue::unsearchable`]).
    pub(crate) fn count_unsearchable(&self) -> Result<usize> {
        self.db.read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM session INDEXED BY session_unsearchable
                 WHERE searchable = 0",
                [],
                |row| row.get(0),
            )?)
        })
    }

    /// Says the text of each of `made` is of its session's row, where that row is still made
    /// from what is given with it.
    pub(crate) fn set_searchable(&self, made: &[(SessionId, String)]) -> Result<()> {
        if made.is_empty() {
            return Ok(());
        }
        self.db.write(|tx| {
            let mut set = tx.prepare(&format!(
                "UPDATE session AS s SET searchable = 1
                 WHERE s.location_id = ?1 AND s.provider = ?2 AND s.session = ?3
                   AND {MADE_FROM} = ?4"
            ))?;
            for (id, made) in made {
                set.execute(params![id.location, id.provider, id.key, made])?;
            }
            Ok(())
        })
    }

    /// Says no session's text is of its row: the search index is new.
    pub(crate) fn none_searchable(&self) -> Result<()> {
        self.db.write(|tx| {
            tx.execute_batch("UPDATE session SET searchable = 0; DELETE FROM text_gone;")?;
            Ok(())
        })
    }

    /// The sessions whose text is to be taken out of the search index: their rows went, and
    /// have not come back (a row made again takes its session off the list).
    pub(crate) fn text_gone(&self) -> Result<Vec<SessionId>> {
        self.db.read(|c| {
            Ok(
                c.prepare("SELECT location_id, provider, session FROM text_gone")?
                    .query_map([], |row| {
                        Ok(SessionId {
                            location: row.get(0)?,
                            provider: row.get(1)?,
                            key: row.get(2)?,
                        })
                    })?
                    .collect::<rusqlite::Result<_>>()?,
            )
        })
    }

    /// The text of `sessions` is out of the search index.
    pub(crate) fn text_forgotten(&self, sessions: &[SessionId]) -> Result<()> {
        self.db.write(|tx| {
            let mut done = tx.prepare(
                "DELETE FROM text_gone WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
            )?;
            for id in sessions {
                done.execute(params![id.location, id.provider, id.key])?;
            }
            Ok(())
        })
    }

    /// What the row of the session `id` was made from ([`Catalogue::made`]), if it has one.
    pub(crate) fn made_of(&self, id: &SessionId) -> Result<Option<String>> {
        self.db.read(|c| {
            Ok(c.query_row(
                &format!(
                    "SELECT {MADE_FROM} FROM session s
                     WHERE s.location_id = ?1 AND s.provider = ?2 AND s.session = ?3"
                ),
                params![id.location, id.provider, id.key],
                |row| row.get(0),
            )
            .optional()?)
        })
    }

    /// Removes the rows of `sessions`, which are sessions no more ([`Catalogue::stale`]). What
    /// the owner decided about them stays, for the day they are sessions again.
    pub(crate) fn forget_sessions(&self, sessions: &[SessionId]) -> Result<()> {
        self.db.write(|tx| {
            let mut forget = tx.prepare(
                "DELETE FROM session WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
            )?;
            let mut text = tx.prepare(
                "INSERT OR IGNORE INTO text_gone (location_id, provider, session)
                 VALUES (?1, ?2, ?3)",
            )?;
            for id in sessions {
                forget.execute(params![id.location, id.provider, id.key])?;
                text.execute(params![id.location, id.provider, id.key])?;
            }
            Ok(())
        })
    }

    /**
     * Records what became of an index of the session `id`, with what the row is made by and
     * from: the parser's version, the index's, and `from`, the [`fingerprint`] of the items
     * that were read.
     *
     * A session that was parsed is then filed by the rules, unless the owner filed it by hand
     * (PLAN §8). One that was not keeps what its row said of it before, and where it is
     * filed; and when it was the archive that could not be read, the row says to try again.
     * Returns whether the rules moved the session into a folder.
     */
    pub(crate) fn record_session(
        &self,
        id: &SessionId,
        parser_version: u32,
        from: Hash,
        indexed: Indexed<'_>,
    ) -> Result<bool> {
        let nothing = SessionIndex::default();
        let (derived, failure, retry) = match indexed {
            Indexed::Parsed(derived) => (Some(derived), None, false),
            Indexed::Faulted(reason) => (None, Some(reason), false),
            Indexed::NotRead(reason) => (None, Some(reason), true),
        };
        let index = derived.as_ref().map_or(&nothing, |derived| derived.index);
        let main = derived.as_ref().and_then(|derived| derived.main);
        /* Under the location's own drive mount, where it has one (PLAN §6.3); a copy ran
        elsewhere, and is read under none (PLAN §8). */
        let mount = match index.copy {
            None => self.drive_mount(&id.location)?,
            Some(_) => None,
        };
        let project = index
            .project_path
            .as_deref()
            .and_then(|path| NormalPath::new(path, mount.as_deref()));
        let (ending, unreadable) = match derived.as_ref().and_then(|derived| derived.ending) {
            None => (None, None),
            Some(Ending::Whole) => (Some("whole"), None),
            Some(Ending::MidRecord) => (Some("mid_record"), None),
            Some(Ending::Unreadable(reason)) => (Some("unreadable"), Some(reason)),
        };
        let models =
            serde_json::to_string(&index.models).expect("a list of text always serializes");
        let (own, all) = (usage(&index.own), usage(&index.all));
        /* SQLite's integers are signed: a count past their range is recorded as the greatest. */
        let count = |count: u64| i64::try_from(count).unwrap_or(i64::MAX);
        let columns: Vec<&str> = DERIVED.into_iter().chain(MADE).collect();
        /* Of a row that is there, a session that was not parsed changes only why, and what
        the row is made by and from. */
        let set = if derived.is_some() {
            &columns[..]
        } else {
            &MADE[..]
        };
        self.db.write(|tx| {
            /* A session the owner deleted for good while it was parsed gets no row: there is
            nothing of it left to make one from. */
            if !has_items(tx, id)? {
                return Ok(false);
            }
            tx.execute(
                &upsert("session", &columns, set),
                params![
                    id.location,
                    id.provider,
                    id.key,
                    main.map(|main| main.item),
                    main.map(|main| main.number),
                    main.map(|main| main.newest_number),
                    index.title,
                    index.first_prompt,
                    index.project_path,
                    project,
                    index.branch,
                    index.entrypoint,
                    index.started,
                    index.ended,
                    models,
                    own.calls,
                    count(own.tokens.input),
                    count(own.tokens.output),
                    count(own.tokens.cache_read),
                    count(own.tokens.cache_write),
                    all.calls,
                    count(all.tokens.input),
                    count(all.tokens.output),
                    count(all.tokens.cache_read),
                    count(all.tokens.cache_write),
                    ending,
                    unreadable,
                    index.unread,
                    derived.as_ref().is_some_and(|derived| derived.held_back),
                    index.conversation,
                    index.copy.as_ref().map(|copy| &copy.label),
                    index.copy.as_ref().map(|copy| &copy.origin),
                    failure,
                    retry,
                    parser_version,
                    INDEX_VERSION,
                    from,
                    false
                ],
            )?;
            /* Its text is to be made again, not taken out. */
            tx.execute(
                "DELETE FROM text_gone WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
                params![id.location, id.provider, id.key],
            )?;
            if derived.is_none() {
                return Ok(false);
            }
            if index.copy.is_none() && index.conversation.is_some() {
                inherit(tx, id, index)?;
            }
            let ran_at = index.copy.is_none().then_some(&id.location);
            Ok(folder::file_by_rule(tx, id, project.as_ref(), ran_at)?)
        })
    }

    /**
     * The other sessions of the session `id`'s conversation (PLAN §7.7), originals first, each
     * with how its main transcript compares with `id`'s.
     */
    pub fn kin(&self, id: &SessionId) -> Result<Vec<Kin>> {
        self.db.read(|c| {
            Ok(c.prepare(
                "SELECT o.location_id, o.provider, o.session, o.copy_label, o.copy_origin,
                        COALESCE(ov.length, 0), ov.hash, COALESCE(sv.length, 0), sv.hash
                 FROM session s
                 JOIN session o
                   ON o.provider = s.provider AND o.conversation = s.conversation
                  AND o.project_path IS s.project_path AND o.id <> s.id
                 LEFT JOIN version sv ON sv.item_id = s.main_item AND sv.number = s.reading_version
                 LEFT JOIN version ov ON ov.item_id = o.main_item AND ov.number = o.reading_version
                 WHERE s.location_id = ?1 AND s.provider = ?2 AND s.session = ?3
                 ORDER BY o.copy_label IS NOT NULL, o.location_id, o.session",
            )?
            .query_map(params![id.location, id.provider, id.key], |row| {
                let theirs: (u64, Option<String>) = (row.get(5)?, row.get(6)?);
                let ours: (u64, Option<String>) = (row.get(7)?, row.get(8)?);
                Ok(Kin {
                    id: SessionId {
                        location: row.get(0)?,
                        provider: row.get(1)?,
                        key: row.get(2)?,
                    },
                    copy: copy_from(row, 3)?,
                    compared: match theirs.0.cmp(&ours.0) {
                        Ordering::Less => Compared::Shorter,
                        Ordering::Greater => Compared::Longer,
                        Ordering::Equal if theirs.1 == ours.1 => Compared::Same,
                        Ordering::Equal => Compared::Differs,
                    },
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// The copy the rule reads of the item `item` without its versions from `number` on
    /// ([`copy`]), if it has an earlier version that is not held back.
    pub(crate) fn copy_before(&self, item: i64, number: u32) -> Result<Option<Version>> {
        self.db.read(|c| {
            Ok(c.query_row(
                &format!(
                    "SELECT {VERSION} FROM version v WHERE v.id = {}",
                    copy("id", "?1", Some("?2"))
                ),
                params![item, number],
                |row| version_from(row, 0),
            )
            .optional()?)
        })
    }

    /**
     * Says whether the version `version` is held back, and why (PLAN §4.2), if it is still
     * `length` bytes long, as it was when that was decided: an append since may have changed
     * what it is. A version the owner accepted is never held back. Returns whether anything
     * changed.
     */
    pub(crate) fn set_hold(&self, version: i64, length: u64, held: Option<Held>) -> Result<bool> {
        let (held_back, by_parse) = (held.is_some(), held == Some(Held::ParsesWorse));
        self.db.write(|tx| {
            Ok(tx.execute(
                "UPDATE version SET held_back = ?3, held_by_parse = ?4
                 WHERE id = ?1 AND length = ?2 AND accepted = 0
                   AND (held_back, held_by_parse) IS NOT (?3, ?4)",
                params![version, length, held_back, by_parse],
            )? > 0)
        })
    }

    /**
     * Makes `version` the version that is read of `item`, one of the session's items (its
     * main transcript, in the history the reader shows); `None` goes back to the rule
     * (the module docs). The session's items read differently at once; its row follows when
     * the session is next indexed (`sync::index_session`).
     *
     * A version that is held back is accepted: it is no longer held back, nor will be, and
     * the rule itself then reads it when it is the newest. A version the rule does not read
     * is chosen in the rule's place, until the item gets a newer version than there is now,
     * or the chosen one is found damaged. A damaged version is not chosen.
     */
    pub fn set_reading_version(
        &self,
        session: &SessionId,
        item: i64,
        version: Option<i64>,
    ) -> Result<()> {
        self.db.write(|tx| {
            let of_session: bool = tx.query_row(
                "SELECT EXISTS (
                     SELECT 1 FROM item
                     WHERE id = ?4 AND location_id = ?1 AND provider = ?2 AND session = ?3
                 )",
                params![session.location, session.provider, session.key, item],
                |row| row.get(0),
            )?;
            if !of_session {
                return Err(CatalogueError::NotOfSession(item));
            }
            let Some(version) = version else {
                tx.execute("DELETE FROM reading_choice WHERE item_id = ?1", [item])?;
                return Ok(());
            };
            let (number, held, damaged): (u32, bool, bool) = tx
                .query_row(
                    "SELECT number, held_back, damaged FROM version
                     WHERE id = ?1 AND item_id = ?2",
                    params![version, item],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?
                .ok_or(CatalogueError::NotOfItem { version, item })?;
            if damaged {
                return Err(CatalogueError::Damaged(version));
            }
            tx.execute("DELETE FROM reading_choice WHERE item_id = ?1", [item])?;
            if held {
                tx.execute(
                    "UPDATE version SET held_back = 0, held_by_parse = 0, accepted = 1
                     WHERE id = ?1",
                    [version],
                )?;
                /* The versions after it were held back for being shorter than another copy
                than the rule reads before them now. Each is measured again, the earliest
                first; but not the newest, which the index measures and parses. */
                let later: Vec<i64> = tx
                    .prepare(
                        "SELECT id FROM version
                         WHERE item_id = ?1 AND number > ?2 AND held_back AND NOT held_by_parse
                           AND number < (SELECT max(number) FROM version WHERE item_id = ?1)
                         ORDER BY number",
                    )?
                    .query_map(params![item, number], |row| row.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                let mut measure = tx.prepare(&format!(
                    "UPDATE version SET held_back = COALESCE(length < {}, 0) WHERE id = ?1",
                    copy("length", "version.item_id", Some("version.number"))
                ))?;
                for later in later {
                    measure.execute([later])?;
                }
            }
            let read: i64 = tx.query_row(
                &format!(
                    "SELECT v.id FROM version v JOIN item i ON i.id = v.item_id
                     WHERE i.id = ?1 AND v.number = {}",
                    ruled("i.id", "i.kind")
                ),
                [item],
                |row| row.get(0),
            )?;
            if read != version {
                tx.execute(
                    "INSERT INTO reading_choice (item_id, version_id) VALUES (?1, ?2)",
                    params![item, version],
                )?;
            }
            Ok(())
        })
    }

    /// The history of the session `id`'s item `key`; `None` when it is not one of its items.
    pub fn history(&self, id: &SessionId, key: &str) -> Result<Option<History>> {
        let Some(reading) = self
            .session_items(id)?
            .into_iter()
            .find(|reading| reading.key == key)
        else {
            return Ok(None);
        };
        self.db.read(|c| {
            let versions = c
                .prepare(&format!(
                    "SELECT {VERSION}, (SELECT count(*) FROM segment WHERE version_id = v.id)
                     FROM version v WHERE v.item_id = ?1 ORDER BY v.number"
                ))?
                .query_map([reading.item], |row| {
                    let segments: usize = row.get(10)?;
                    Ok((version_from(row, 0)?, segments.saturating_sub(1)))
                })?
                .collect::<rusqlite::Result<_>>()?;
            let chosen = c
                .query_row(
                    "SELECT version_id FROM reading_choice WHERE item_id = ?1",
                    [reading.item],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            Ok(Some(History {
                item: reading.item,
                versions,
                reading: reading.version,
                chosen: chosen == Some(reading.version),
            }))
        })
    }

    pub fn set_starred(&self, session: &SessionId, starred: bool) -> Result<()> {
        self.db.write(|tx| {
            archived(tx, session)?;
            Ok(set_state(tx, session, &[("starred", &starred)]).map(drop)?)
        })
    }

    /// Empties the derived tables (PLAN §2.2). The next index fills them again from the
    /// archive; what the owner decided about each session stays.
    pub fn drop_index(&self) -> Result<()> {
        self.db.write(|tx| {
            /* Their text goes from the search index too, unless their rows come back. */
            tx.execute_batch(
                "INSERT OR IGNORE INTO text_gone (location_id, provider, session)
                     SELECT location_id, provider, session FROM session;
                 DELETE FROM session;",
            )?;
            Ok(())
        })
    }

    /**
     * Says again what each archived item of `provider` is, its kind and its session, at
     * `location` or at every location, where the items were last classified by another
     * version of the provider's parser than `parser_version`. A scan classifies what is at
     * the source; this reaches what is archived and gone from there too, so that a classifier
     * that changes changes every session alike ([`classify`]).
     */
    pub(crate) fn reclassify(
        &self,
        location: Option<&LocationId>,
        provider: &str,
        parser_version: u32,
        classifier: &dyn Fn(&str) -> (ItemKind, Option<String>),
    ) -> Result<()> {
        self.db.write(|tx| {
            let locations: Vec<LocationId> = tx
                .prepare(
                    "SELECT location_id FROM location_provider
                     WHERE provider = ?1 AND classified_by IS NOT ?2
                       AND (?3 IS NULL OR location_id = ?3)",
                )?
                .query_map(params![provider, parser_version, location], |row| {
                    row.get(0)
                })?
                .collect::<rusqlite::Result<_>>()?;
            for location in locations {
                /* By key: when one session becomes two, its own transcript that comes first
                is the one the owner's state goes with. */
                let items: Vec<(i64, String)> = tx
                    .prepare(
                        "SELECT id, key FROM item WHERE location_id = ?1 AND provider = ?2
                         ORDER BY key",
                    )?
                    .query_map(params![location, provider], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })?
                    .collect::<rusqlite::Result<_>>()?;
                for (item, key) in items {
                    let (kind, session) = classifier(&key);
                    classify(tx, item, kind, session.as_deref())?;
                }
                tx.execute(
                    "UPDATE location_provider SET classified_by = ?3
                     WHERE location_id = ?1 AND provider = ?2",
                    params![location, provider, parser_version],
                )?;
            }
            Ok(())
        })
    }
}

/**
 * Says what the archived item `item` is: its kind and its session. This is the one place an
 * item is given another session, by a scan or by its provider's classifier, so that what the
 * owner decided about a session (its folder, its star) is never left under an id no session
 * has:
 *
 * - when a session's own transcript is given another session, what was decided goes with it;
 * - when the last of a session's items leaves it, what was decided and is still there goes
 *   with that item;
 * - either way only to a session about which nothing is decided yet. What is then left under
 *   an id that no item has is dropped: two sessions that become one keep the decisions of
 *   the first to arrive, and those about a session whose items belong to none are gone;
 *   but the one kept is in Trash if either was.
 */
pub(super) fn classify(
    connection: &Connection,
    item: i64,
    kind: ItemKind,
    session: Option<&str>,
) -> rusqlite::Result<()> {
    let (location, provider, was_kind, was): (LocationId, String, ItemKind, Option<String>) =
        connection.query_row(
            "SELECT location_id, provider, kind, session FROM item WHERE id = ?1",
            [item],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
    if (was_kind, was.as_deref()) == (kind, session) {
        return Ok(());
    }
    connection.execute(
        "UPDATE item SET kind = ?2, session = ?3 WHERE id = ?1",
        params![item, kind, session],
    )?;
    let Some(was) = was.filter(|was| Some(was.as_str()) != session) else {
        return Ok(());
    };
    let left: bool = connection.query_row(
        "SELECT NOT EXISTS (
             SELECT 1 FROM item WHERE location_id = ?1 AND provider = ?2 AND session = ?3
         )",
        params![location, provider, was],
        |row| row.get(0),
    )?;
    let own = (was_kind, kind) == (ItemKind::Transcript, ItemKind::Transcript);
    if let Some(session) = session.filter(|_| own || left) {
        /* Where the session it goes to has state already, which this state does not replace,
        it is in Trash if either was: of the two, keeping a session out of sight until the
        owner says otherwise is the side that loses nothing. */
        connection.execute(
            "UPDATE session_state SET trashed_at = COALESCE(trashed_at, (
                 SELECT trashed_at FROM session_state
                 WHERE location_id = ?1 AND provider = ?2 AND session = ?3))
             WHERE location_id = ?1 AND provider = ?2 AND session = ?4",
            params![location, provider, was, session],
        )?;
        connection.execute(
            "UPDATE OR IGNORE session_state SET session = ?4
             WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
            params![location, provider, was, session],
        )?;
    }
    if left {
        connection.execute(
            "DELETE FROM session_state
             WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
            params![location, provider, was],
        )?;
    }
    Ok(())
}

/**
 * Gives the original `id`, of the conversation `index` names, what the owner decided about the
 * copy of it that was shown (its folder, star and Trash), when it has no state of its own
 * (PLAN §7.7): a session starred or filed as a copy stays so once its original is archived.
 */
fn inherit(connection: &Connection, id: &SessionId, index: &SessionIndex) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT OR IGNORE INTO session_state
             (location_id, provider, session, folder_id, by_hand, starred, trashed_at)
         SELECT ?1, ?2, ?3, st.folder_id, st.by_hand, st.starred, st.trashed_at
         FROM session s
         JOIN session_state st
           ON st.location_id = s.location_id AND st.provider = s.provider
          AND st.session = s.session
         WHERE s.provider = ?2 AND s.conversation = ?4 AND s.project_path IS ?5
           AND s.copy_label IS NOT NULL
         ORDER BY s.ended_at DESC NULLS LAST, s.location_id DESC, s.session DESC
         LIMIT 1",
        params![
            id.location,
            id.provider,
            id.key,
            index.conversation,
            index.project_path
        ],
    )?;
    Ok(())
}

/// The items of one session, by key: after [`reading`].
const OF_SESSION: &str =
    "WHERE i.location_id = ?1 AND i.provider = ?2 AND i.session = ?3 ORDER BY i.key";

/// The items of every session of a provider at one location, session by session and by key.
const OF_LOCATION: &str = "WHERE i.location_id = ?2 AND i.provider = ?1 AND i.session IS NOT NULL
                           ORDER BY i.session, i.key";

/// Fails unless something is archived of the session `id`: the owner decides nothing about
/// a session that is not there.
pub(super) fn archived(connection: &Connection, id: &SessionId) -> Result<()> {
    if has_items(connection, id)? {
        Ok(())
    } else {
        Err(CatalogueError::NoSession(id.clone()))
    }
}

/// Whether the catalogue has an item of the session `id`.
fn has_items(connection: &Connection, id: &SessionId) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM item WHERE location_id = ?1 AND provider = ?2 AND session = ?3
         )",
        params![id.location, id.provider, id.key],
        |row| row.get(0),
    )
}

/// Sets `columns` of the owner's state of the session `id`, whose row is made by the first
/// thing that is decided about it.
pub(super) fn set_state(
    connection: &Connection,
    id: &SessionId,
    columns: &[(&str, &dyn ToSql)],
) -> rusqlite::Result<usize> {
    let names: Vec<&str> = columns.iter().map(|(name, _)| *name).collect();
    let identity: [&dyn ToSql; 3] = [&id.location, &id.provider, &id.key];
    connection.execute(
        &upsert("session_state", &names, &names),
        params_from_iter(
            identity
                .into_iter()
                .chain(columns.iter().map(|(_, value)| *value)),
        ),
    )
}

/// The statement that makes the row `table` has for a session, with `columns`, or sets the
/// columns `set` of the row that is there: the session's identity is its first three
/// parameters, the columns the rest.
fn upsert(table: &str, columns: &[&str], set: &[&str]) -> String {
    format!(
        "INSERT INTO {table} (location_id, provider, session, {})
         VALUES (?1, ?2, ?3, {})
         ON CONFLICT (location_id, provider, session) DO UPDATE SET {}",
        columns.join(", "),
        (4..columns.len() + 4)
            .map(|at| format!("?{at}"))
            .collect::<Vec<_>>()
            .join(", "),
        set.iter()
            .map(|column| format!("{column} = excluded.{column}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn usage(totals: &Totals) -> Usage {
    Usage {
        calls: totals.calls.values().sum(),
        tokens: totals.tokens,
    }
}

/// A reading from the first columns of [`reading`].
fn reading_from(row: &Row<'_>) -> rusqlite::Result<Reading> {
    Ok(Reading {
        item: row.get(0)?,
        key: row.get(1)?,
        kind: row.get(2)?,
        version: row.get(3)?,
        number: row.get(4)?,
        length: row.get(5)?,
        damaged: row.get(6)?,
        newest: row.get(7)?,
        newest_number: row.get(8)?,
        newest_length: row.get(9)?,
        newest_held: match (row.get(10)?, row.get(11)?) {
            (false, _) => None,
            (true, false) => Some(Held::Shorter),
            (true, true) => Some(Held::ParsesWorse),
        },
        newest_accepted: row.get(12)?,
    })
}

/// A session from the columns of [`SESSION`].
pub(super) fn session_from(row: &Row<'_>) -> rusqlite::Result<Session> {
    let usage = |at: usize| {
        Ok::<_, rusqlite::Error>(Usage {
            calls: row.get(at)?,
            tokens: Tokens {
                input: row.get(at + 1)?,
                output: row.get(at + 2)?,
                cache_read: row.get(at + 3)?,
                cache_write: row.get(at + 4)?,
            },
        })
    };
    let invalid =
        |at: usize, name: &str| rusqlite::Error::InvalidColumnType(at, name.into(), Type::Text);
    Ok(Session {
        id: SessionId {
            location: row.get(0)?,
            provider: row.get(1)?,
            key: row.get(2)?,
        },
        main_item: row.get(3)?,
        reading_version: row.get(4)?,
        newest_version: row.get(5)?,
        title: row.get(6)?,
        first_prompt: row.get(7)?,
        project_path: row.get(8)?,
        project: row.get(9)?,
        branch: row.get(10)?,
        entrypoint: row.get(11)?,
        started: row.get(12)?,
        ended: row.get(13)?,
        models: serde_json::from_str(&row.get::<_, String>(14)?)
            .map_err(|_| invalid(14, "models"))?,
        own: usage(15)?,
        all: usage(20)?,
        ending: match row.get::<_, Option<String>>(25)?.as_deref() {
            None => None,
            Some("whole") => Some(Ending::Whole),
            Some("mid_record") => Some(Ending::MidRecord),
            Some("unreadable") => Some(Ending::Unreadable(row.get(26)?)),
            Some(_) => return Err(invalid(25, "ending")),
        },
        unread: row.get(27)?,
        held_back: row.get(28)?,
        failure: row.get(29)?,
        parser_version: row.get(30)?,
        index_version: row.get(31)?,
        filed: match (row.get::<_, Option<i64>>(32)?, row.get(33)?) {
            (None, _) => Filed::Unsorted,
            (Some(folder), false) => Filed::ByRule(FolderId(folder)),
            (Some(folder), true) => Filed::ByHand(FolderId(folder)),
        },
        starred: row.get(34)?,
        trashed: row.get(35)?,
        copy: copy_from(row, 36)?,
    })
}

/// A copy's words from the columns `copy_label` and `copy_origin`, at `at` and after it.
fn copy_from(row: &Row<'_>, at: usize) -> rusqlite::Result<Option<CopyOf>> {
    Ok(match (row.get(at)?, row.get(at + 1)?) {
        (Some(label), Some(origin)) => Some(CopyOf { label, origin }),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Archive;

    /// How SQLite goes about `sql`: the lines of its plan.
    fn plan(connection: &Connection, sql: &str) -> Vec<String> {
        connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map([], |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// With every parameter given as a literal, as the plan does not depend on their values.
    fn bound(sql: &str) -> String {
        (1..=4).fold(sql.to_string(), |sql, at| {
            sql.replace(&format!("?{at}"), "'x'")
        })
    }

    /// Whether a copy is shown asks for its conversation's other sessions by their index, not
    /// by going through every row once for each row a page lists.
    #[test]
    fn a_copys_conversation_is_found_by_its_index() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::create(dir.path()).unwrap();
        let plan = archive
            .catalogue()
            .db
            .read(|c| {
                Ok::<_, CatalogueError>(plan(c, &format!("SELECT 1 FROM session s WHERE {SHOWN}")))
            })
            .unwrap();
        assert!(
            plan.iter()
                .any(|step| step.contains("SEARCH o USING INDEX session_conversation")),
            "{plan:#?}"
        );
    }

    /// An index that parsed a session the owner then deleted for good leaves no row of it.
    #[test]
    fn a_session_with_no_items_gets_no_row() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::create(dir.path()).unwrap();
        let catalogue = archive.catalogue();
        let id = SessionId {
            location: catalogue
                .add_location(crate::location::LocationKind::Local, "fixture")
                .unwrap()
                .id,
            provider: "fixture".to_string(),
            key: "deleted".to_string(),
        };
        let index = SessionIndex {
            title: Some("Gone".to_string()),
            ..SessionIndex::default()
        };
        let derived = Derived {
            main: None,
            index: &index,
            ending: None,
            held_back: false,
        };
        assert!(
            !catalogue
                .record_session(&id, 1, Hash::of(b""), Indexed::Parsed(derived))
                .unwrap()
        );
        assert_eq!(catalogue.session(&id).unwrap(), None);
    }

    #[test]
    fn a_hold_is_set_on_the_version_as_it_was_when_it_was_measured() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::create(dir.path()).unwrap();
        let catalogue = archive.catalogue();
        let location = catalogue
            .add_location(crate::location::LocationKind::Local, "fixture")
            .unwrap()
            .id;
        let hash = Hash::of(b"").to_hex();
        catalogue
            .db
            .write(|tx| {
                tx.execute(
                    "INSERT INTO item (id, location_id, provider, key, item_id, kind, session,
                                       seen_length, seen_stamp)
                     VALUES (1, ?1, 'fixture', 'one', 'one', 'transcript', 's', 0, '')",
                    [&location],
                )?;
                for (id, length, accepted) in [(1, 50, 0), (2, 80, 0), (3, 80, 1)] {
                    tx.execute(
                        "INSERT INTO version (id, item_id, number, reason, started_at, length,
                                              hash, held_back, accepted)
                         VALUES (?1, 1, ?1, 'rewritten', 0, ?2, ?3, 0, ?4)",
                        params![id, length, hash, accepted],
                    )?;
                }
                Ok::<_, CatalogueError>(())
            })
            .unwrap();
        let held = |version: usize| catalogue.versions(1).unwrap()[version - 1].held_back;
        let worse = Some(Held::ParsesWorse);

        /* The version grew since it was parsed: what it parses as now is not known. */
        assert!(!catalogue.set_hold(2, 70, worse).unwrap());
        assert_eq!(held(2), None);
        /* As long as it was: the hold is set, once, and lifted the same way. */
        assert!(catalogue.set_hold(2, 80, worse).unwrap());
        assert_eq!(held(2), worse);
        assert!(!catalogue.set_hold(2, 80, worse).unwrap());
        assert!(catalogue.set_hold(2, 80, Some(Held::Shorter)).unwrap());
        assert!(!catalogue.set_hold(2, 90, None).unwrap());
        assert_eq!(held(2), Some(Held::Shorter));
        assert!(catalogue.set_hold(2, 80, None).unwrap());
        assert_eq!(held(2), None);
        /* A version the owner accepted is never held back. */
        assert!(!catalogue.set_hold(3, 80, worse).unwrap());
        assert_eq!(held(3), None);
        /* The copy before a version is the newest one before it that is not held back. */
        catalogue.set_hold(2, 80, worse).unwrap();
        let before = |number| {
            catalogue
                .copy_before(1, number)
                .unwrap()
                .map(|copy| copy.number)
        };
        assert_eq!(
            (before(1), before(2), before(3), before(4)),
            (None, Some(1), Some(1), Some(3))
        );
    }

    /**
     * An archive of twenty thousand sessions is slow to index and to open a session of if a
     * session's items are found by going through all of a location's. Each query that reads
     * items looks them up by session in `item_session`, in the order it wants them, and each
     * version by its item.
     */
    #[test]
    fn a_sessions_items_are_found_by_its_index_and_never_by_a_scan() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::create(dir.path()).unwrap();
        archive
            .catalogue()
            .db
            .read(|c| {
                for (what, sql) in [
                    ("a session's items", format!("{} {OF_SESSION}", reading())),
                    ("a location's items", format!("{} {OF_LOCATION}", reading())),
                    (
                        "whether a session is archived",
                        "SELECT EXISTS (
                             SELECT 1 FROM item
                             WHERE location_id = ?1 AND provider = ?2 AND session = ?3
                         )"
                        .to_string(),
                    ),
                ] {
                    let plan = plan(c, &bound(&sql));
                    let items: Vec<&String> = plan
                        .iter()
                        .filter(|step| step.contains(" i ") || step.contains(" item "))
                        .collect();
                    assert!(
                        !items.is_empty() && items.iter().all(|step| step.contains("item_session")),
                        "{what}: {plan:#?}"
                    );
                    assert!(
                        !plan
                            .iter()
                            .any(|step| step.starts_with("SCAN i") || step.contains("TEMP B-TREE")),
                        "{what}: {plan:#?}"
                    );
                }
                Ok::<_, CatalogueError>(())
            })
            .unwrap();
    }
}
