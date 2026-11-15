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

use std::collections::HashMap;

use toolbox_store::rusqlite::types::Type;
use toolbox_store::rusqlite::{
    self, Connection, OptionalExtension as _, Row, ToSql, params, params_from_iter,
};

use super::{Catalogue, CatalogueError, Held, Result, folder};
use crate::filing::{FolderId, NormalPath};
use crate::hash::{Hash, Hasher};
use crate::location::LocationId;
use crate::provider::ItemKind;
use crate::transcript::{Ending, INDEX_VERSION, SessionIndex, Tokens, Totals};

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
    /// The item's newest version, which is the one read unless it is held back or the owner
    /// chose another.
    pub newest: i64,
    pub newest_number: u32,
    pub newest_length: u64,
    pub newest_held: Option<Held>,
    /// The owner chose to read the newest version while it was held back.
    pub newest_accepted: bool,
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
    /// The [`fingerprint`] of the items it was made from; `None` when it is of no state of
    /// them.
    from: Option<Hash>,
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
    /// stored: the next index tries again.
    NotRead(&'a str),
}

/// What tells one state of a session's items from another: what each item is, its version
/// that is read and its newest, with their lengths, and whether the newest is held back. A row
/// made from one state is stale in any other.
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
                reading.newest,
                reading.newest_length,
                reading.newest_held.is_some()
            )
            .as_bytes(),
        );
    }
    hasher.finish()
}

/// Every item that belongs to a session, as a [`Reading`], then its location and session.
/// What follows says which items, and is [`OF_SESSION`] or [`OF_LOCATION`].
const READING: &str = "
    SELECT i.id, i.key, i.kind, r.id, r.number, r.length,
           n.id, n.number, n.length, n.held_back, n.held_by_parse, n.accepted,
           i.location_id, i.session
    FROM item i
    JOIN version n ON n.item_id = i.id
     AND n.number = (SELECT max(number) FROM version WHERE item_id = i.id)
    JOIN version r ON r.item_id = i.id AND r.number = COALESCE(
        (SELECT chosen.number FROM reading_choice choice
         JOIN version chosen ON chosen.id = choice.version_id
         WHERE choice.item_id = i.id AND chosen.damaged = 0),
        CASE WHEN i.kind IN ('transcript', 'subagent') THEN
            (SELECT max(number) FROM version WHERE item_id = i.id AND held_back = 0)
        END,
        n.number)";

/// A session's row with the owner's state of it, as [`session_from`] reads them.
pub(super) const SESSION: &str = "
    SELECT s.location_id, s.provider, s.session, s.main_item, s.reading_version,
           s.newest_version, s.title, s.first_prompt, s.project_path, s.project, s.branch,
           s.entrypoint, s.started_at, s.ended_at, s.models,
           s.own_calls, s.own_input, s.own_output, s.own_cache_read, s.own_cache_write,
           s.all_calls, s.all_input, s.all_output, s.all_cache_read, s.all_cache_write,
           s.ending, s.unreadable, s.unread, s.held_back, s.failure, s.parser_version,
           s.index_version, st.folder_id, COALESCE(st.by_hand, 0), COALESCE(st.starred, 0)
    FROM session s
    LEFT JOIN session_state st
      ON st.location_id = s.location_id AND st.provider = s.provider AND st.session = s.session";

/// The session that did something last comes first; those with no time come last.
pub(super) const BY_ACTIVITY: &str =
    "ORDER BY s.ended_at DESC NULLS LAST, s.location_id, s.provider, s.session";

/// What a row says of the session itself, in the order [`Catalogue::record_session`] gives it.
const DERIVED: [&str; 28] = [
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
    "failure",
    "parser_version",
];

/// What a row says of the index that made it, after [`DERIVED`]. These, with `failure` and
/// `parser_version` before them, are all that an index that could not parse the session sets
/// of a row that is there.
const MADE: [&str; 2] = ["index_version", "indexed_from"];

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
            Ok(c.prepare(&format!("{READING} {OF_SESSION}"))?
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
     * was made by another version of the parser or of the index.
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
                            indexed_from, ended_at
                     FROM session WHERE provider = ?1 AND (?4 IS NULL OR location_id = ?4)",
                )?
                .query_map(
                    params![provider, parser_version, INDEX_VERSION, location],
                    |row| {
                        let made = Made {
                            by_these: row.get(2)?,
                            from: row.get(3)?,
                            ended: row.get(4)?,
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
                        (made.by_these || !backlog) && made.from == Some(fingerprint(readings))
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
            let mut select = c.prepare(&format!("{READING} {OF_LOCATION}"))?;
            let mut readings = Vec::new();
            for location in locations {
                let mut rows = select.query(params![provider, location])?;
                let mut open: Option<String> = None;
                while let Some(row) = rows.next()? {
                    let session: String = row.get(13)?;
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

    /// Removes the rows of `sessions`, which are sessions no more ([`Catalogue::stale`]). What
    /// the owner decided about them stays, for the day they are sessions again.
    pub(crate) fn forget_sessions(&self, sessions: &[SessionId]) -> Result<()> {
        self.db.write(|tx| {
            let mut forget = tx.prepare(
                "DELETE FROM session WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
            )?;
            for id in sessions {
                forget.execute(params![id.location, id.provider, id.key])?;
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
     * filed; and when it was the archive that could not be read, the row is of no state of
     * the items, so that the next index tries again. Returns whether the rules moved the
     * session into a folder.
     */
    pub(crate) fn record_session(
        &self,
        id: &SessionId,
        parser_version: u32,
        from: Hash,
        indexed: Indexed<'_>,
    ) -> Result<bool> {
        let nothing = SessionIndex::default();
        let (derived, failure, from) = match indexed {
            Indexed::Parsed(derived) => (Some(derived), None, Some(from)),
            Indexed::Faulted(reason) => (None, Some(reason), Some(from)),
            Indexed::NotRead(reason) => (None, Some(reason), None),
        };
        let index = derived.as_ref().map_or(&nothing, |derived| derived.index);
        let main = derived.as_ref().and_then(|derived| derived.main);
        /* The drive mount is the default one for every location until a location can say its
        own (PLAN §6.3). */
        let project = index
            .project_path
            .as_deref()
            .and_then(|path| NormalPath::new(path, None));
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
            &columns[DERIVED.len() - 2..]
        };
        self.db.write(|tx| {
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
                    failure,
                    parser_version,
                    INDEX_VERSION,
                    from
                ],
            )?;
            Ok(derived.is_some() && folder::file_by_rule(tx, id, project.as_ref())?)
        })
    }

    /**
     * Holds the version `version` back for how its transcript parses, or stops holding it
     * back for that (PLAN §4.2), if it is still `length` bytes long, as it was when it was
     * parsed; an append since may have changed what it parses as. A version no longer held
     * for how it parses is still held back while it is shorter than the copy being read.
     * Returns whether anything changed.
     */
    pub(crate) fn hold_for_parse(&self, version: i64, length: u64, held: bool) -> Result<bool> {
        self.db.write(|tx| {
            Ok(tx.execute(
                "UPDATE version SET held_by_parse = ?3,
                     held_back = ?3 OR COALESCE(length < (
                         SELECT length FROM version reading
                         WHERE reading.item_id = version.item_id
                           AND reading.held_back = 0
                           AND reading.number < version.number
                         ORDER BY reading.number DESC LIMIT 1
                     ), 0)
                 WHERE id = ?1 AND length = ?2 AND accepted = 0 AND held_by_parse IS NOT ?3",
                params![version, length, held],
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
     * or the chosen one is found damaged.
     */
    pub fn set_reading_version(
        &self,
        session: &SessionId,
        item: i64,
        version: Option<i64>,
    ) -> Result<()> {
        self.db.write(|tx| {
            let kind: ItemKind = tx
                .query_row(
                    "SELECT kind FROM item
                     WHERE id = ?4 AND location_id = ?1 AND provider = ?2 AND session = ?3",
                    params![session.location, session.provider, session.key, item],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or(CatalogueError::NotOfSession(item))?;
            tx.execute("DELETE FROM reading_choice WHERE item_id = ?1", [item])?;
            let Some(version) = version else {
                return Ok(());
            };
            let accepted = tx.execute(
                "UPDATE version SET held_back = 0, held_by_parse = 0, accepted = accepted OR held_back
                 WHERE id = ?1 AND item_id = ?2",
                params![version, item],
            )?;
            if accepted == 0 {
                return Err(CatalogueError::NotOfItem { version, item });
            }
            let read: i64 = tx.query_row(
                "SELECT id FROM version WHERE item_id = ?1 AND (held_back = 0 OR NOT ?2)
                 ORDER BY number DESC LIMIT 1",
                params![item, kind.is_transcript()],
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

    pub fn set_starred(&self, session: &SessionId, starred: bool) -> Result<()> {
        self.db.write(|tx| {
            archived(tx, session)?;
            Ok(set_state(tx, session, &[("starred", &starred)]).map(drop)?)
        })
    }

    /// Empties the derived tables (PLAN §2.2). The next index fills them again from the
    /// archive; what the owner decided about each session stays.
    pub fn drop_index(&self) -> Result<()> {
        self.execute("DELETE FROM session", [])
    }

    /**
     * Says again what each archived item of `provider` is, its kind and its session, at the
     * locations whose items were last classified by another version of the provider's parser
     * than `parser_version`. A scan classifies what is at the source; this reaches what is
     * archived and gone from there too, so that a classifier that changes changes every
     * session alike. What the owner decided about a session goes with the session's own
     * transcript when that is given another session.
     */
    pub(crate) fn reclassify(
        &self,
        provider: &str,
        parser_version: u32,
        classify: &dyn Fn(&str) -> (ItemKind, Option<String>),
    ) -> Result<()> {
        self.db.write(|tx| {
            let locations: Vec<LocationId> = tx
                .prepare(
                    "SELECT location_id FROM location_provider
                     WHERE provider = ?1 AND classified_by IS NOT ?2",
                )?
                .query_map(params![provider, parser_version], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for location in locations {
                let items: Vec<(i64, String, ItemKind, Option<String>)> = tx
                    .prepare(
                        "SELECT id, key, kind, session FROM item
                         WHERE location_id = ?1 AND provider = ?2",
                    )?
                    .query_map(params![location, provider], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                    })?
                    .collect::<rusqlite::Result<_>>()?;
                for (item, key, kind, session) in items {
                    let (now, session_now) = classify(&key);
                    if (now, &session_now) == (kind, &session) {
                        continue;
                    }
                    tx.execute(
                        "UPDATE item SET kind = ?2, session = ?3 WHERE id = ?1",
                        params![item, now, session_now],
                    )?;
                    if (kind, now) == (ItemKind::Transcript, ItemKind::Transcript)
                        && let (Some(was), Some(is)) = (session, session_now)
                    {
                        tx.execute(
                            "UPDATE OR IGNORE session_state SET session = ?4
                             WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
                            params![location, provider, was, is],
                        )?;
                    }
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

/// The items of one session, by key: after [`READING`].
const OF_SESSION: &str =
    "WHERE i.location_id = ?1 AND i.provider = ?2 AND i.session = ?3 ORDER BY i.key";

/// The items of every session of a provider at one location, session by session and by key.
const OF_LOCATION: &str = "WHERE i.location_id = ?2 AND i.provider = ?1 AND i.session IS NOT NULL
                           ORDER BY i.session, i.key";

/// Fails unless something is archived of the session `id`: the owner decides nothing about
/// a session that is not there.
pub(super) fn archived(connection: &Connection, id: &SessionId) -> Result<()> {
    let archived: bool = connection.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM item WHERE location_id = ?1 AND provider = ?2 AND session = ?3
         )",
        params![id.location, id.provider, id.key],
        |row| row.get(0),
    )?;
    if archived {
        Ok(())
    } else {
        Err(CatalogueError::NoSession(id.clone()))
    }
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

/// A reading from the first columns of [`READING`].
fn reading_from(row: &Row<'_>) -> rusqlite::Result<Reading> {
    Ok(Reading {
        item: row.get(0)?,
        key: row.get(1)?,
        kind: row.get(2)?,
        version: row.get(3)?,
        number: row.get(4)?,
        length: row.get(5)?,
        newest: row.get(6)?,
        newest_number: row.get(7)?,
        newest_length: row.get(8)?,
        newest_held: match (row.get(9)?, row.get(10)?) {
            (false, _) => None,
            (true, false) => Some(Held::Shorter),
            (true, true) => Some(Held::ParsesWorse),
        },
        newest_accepted: row.get(11)?,
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
                    ("a session's items", format!("{READING} {OF_SESSION}")),
                    ("a location's items", format!("{READING} {OF_LOCATION}")),
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
