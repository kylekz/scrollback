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

use super::{Catalogue, CatalogueError, Held, Result, VERSION, Version, folder, version_from};
use crate::filing::{FolderId, NormalPath};
use crate::hash::{Hash, Hasher};
use crate::location::LocationId;
use crate::provider::ItemKind;
use crate::transcript::{CopyWords, Ending, INDEX_VERSION, SessionIndex, Tokens, Totals};

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
     * pulled from, unless it is a copy, which ran at the location its record makes it, if any:
     * never the location that kept it. It is what every view that names a session's location
     * shows, filters and files by.
     */
    pub fn ran_at(&self) -> Option<&LocationId> {
        match &self.copy {
            None => Some(&self.id.location),
            Some(CopyOf {
                origin: Origin::Location { location, .. },
                ..
            }) => Some(location),
            Some(_) => None,
        }
    }
}

/// What a session is a copy as (PLAN §7.7): the provider's label, and where it ran as far as
/// anything says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyOf {
    /// What the copy is, shown where a location would be: "Some App's copy".
    pub label: String,
    pub origin: Origin,
}

impl CopyOf {
    /// What the tag of a copy that is from no location says: the host it ran on when that is
    /// known, else what the copy is.
    pub fn tag(&self) -> &str {
        match &self.origin {
            Origin::Host { host, .. } => host,
            Origin::Unsaid(_) | Origin::Location { .. } => &self.label,
        }
    }
}

/// Where a copy of a session ran (PLAN §7.7), each with the provider's words for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Nothing says which machine: "Ran over SSH; …".
    Unsaid(String),
    /// On this host, as the app that kept the copy recorded it, which this machine's `ssh`
    /// resolves and is none of the locations. `ran_on` is said before it: "Ran over SSH on".
    Host { ran_on: String, host: String },
    /// At this location: the SSH location this machine's `ssh` makes the recorded host.
    Location {
        ran_on: String,
        location: LocationId,
    },
}

/// How another session's main transcript compares with a session's, by the versions of each
/// that are read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compared {
    Same,
    /// The start of the session's, and no more: behind it.
    Shorter,
    /// The session's and more: ahead of it.
    Longer,
    /// Neither holds the other.
    Differs,
}

/// The versions of the main transcripts that are read of another session of a session's
/// conversation and of the session itself: each its id, length and hash.
pub(crate) struct KinRead {
    pub id: SessionId,
    pub copy: Option<CopyOf>,
    pub theirs: (Option<i64>, u64, Option<String>),
    pub ours: (Option<i64>, u64, Option<String>),
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
    /// Its row was made by another version of the parser or of the index.
    pub outdated: bool,
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

/// What became of a session's row ([`Catalogue::record_session`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowMade {
    /// It was made, and the rules moved the session into a folder or not.
    Made { filed: bool },
    /// It was not: the session is of a conversation the owner deleted for good (PLAN §4.5),
    /// and was deleted for good in its place, as a pass would have skipped it.
    Deleted,
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

/**
 * `$value` (SQL of the columns of `r`) of the copy record of the session `$x`'s conversation
 * where it was kept (PLAN §7.7), as an SQL value: NULL without one.
 */
macro_rules! recorded {
    ($value:literal, $x:literal) => {
        concat!(
            "(SELECT ",
            $value,
            " FROM copy_record r WHERE r.location_id = ",
            $x,
            ".location_id AND r.provider = ",
            $x,
            ".provider AND r.conversation = ",
            $x,
            ".conversation)"
        )
    };
}

/**
 * The condition, as SQL, that the record of the copy `$c` names the transcript item `$item`
 * (SQL) where it ran (PLAN §7.7): the item is at a WSL or SSH location, has the key the
 * provider made of the path recorded, and that path is under the root its provider last
 * reported there. Where a session another machine's app copied ran.
 */
macro_rules! names_transcript {
    ($c:literal, $item:expr) => {
        concat!(
            "EXISTS (SELECT 1 FROM copy_record r
              JOIN item i ON i.id = ",
            $item,
            " AND i.key = r.key
              JOIN location l ON l.id = i.location_id AND l.kind IN ('wsl', 'ssh')
              JOIN location_provider lp
                ON lp.location_id = i.location_id AND lp.provider = i.provider
              WHERE r.location_id = ",
            $c,
            ".location_id AND r.provider = ",
            $c,
            ".provider AND r.conversation = ",
            $c,
            ".conversation
                AND substr(r.path, 1, length(lp.root) + 1) = lp.root || '/')"
        )
    };
}

/**
 * What a session's row says of it being a copy, as [`copy_from`] reads them: the provider's
 * label and words, the host of its record when this machine's `ssh` resolved it, and the
 * location that host is.
 */
macro_rules! copy_columns {
    ($x:literal) => {
        concat!(
            $x,
            ".copy_label, ",
            $x,
            ".copy_origin, ",
            $x,
            ".copy_ran_on, ",
            recorded!("CASE WHEN r.resolved THEN r.host END", $x),
            ", ",
            recorded!("r.ran_at", $x)
        )
    };
}

/// A session's row with the owner's state of it, as [`session_from`] reads them.
pub(super) const SESSION: &str = concat!(
    "
    SELECT s.location_id, s.provider, s.session, s.main_item, s.reading_version,
           s.newest_version, s.title, s.first_prompt, s.project_path, s.project, s.branch,
           s.entrypoint, s.started_at, s.ended_at, s.models,
           s.own_calls, s.own_input, s.own_output, s.own_cache_read, s.own_cache_write,
           s.all_calls, s.all_input, s.all_output, s.all_cache_read, s.all_cache_write,
           s.ending, s.unreadable, s.unread, s.held_back, s.failure, s.parser_version,
           s.index_version, st.folder_id, COALESCE(st.by_hand, 0), COALESCE(st.starred, 0),
           st.trashed_at, ",
    copy_columns!("s"),
    "
    FROM session s
    LEFT JOIN session_state st
      ON st.location_id = s.location_id AND st.provider = s.provider AND st.session = s.session"
);

/// What a row was made from, in one word ([`Catalogue::made`]).
const MADE_FROM: &str = "s.indexed_from || ' ' || s.parser_version || ' ' || s.index_version";

/// What [`MADE_FROM`] says of a row made from the items whose [`fingerprint`] is `from`, by
/// the parser of version `parser_version` and this index.
pub(crate) fn made(from: Hash, parser_version: u32) -> String {
    format!("{} {parser_version} {INDEX_VERSION}", from.to_hex())
}

/**
 * The condition, as SQL, that the session `$x` is a copy its parser never read: no project
 * path to match by, its conversation the one its provider gives from its key.
 */
macro_rules! unparsed_copy {
    ($x:literal) => {
        concat!(
            "(",
            $x,
            ".copy_label IS NOT NULL AND ",
            $x,
            ".project_path IS NULL AND ",
            $x,
            ".failure IS NOT NULL)"
        )
    };
}

/**
 * The condition, as SQL, that the sessions `$o` and `$s` are two of one conversation (PLAN
 * §7.7): of one provider, with the agent's one id for the conversation, and either started in
 * one project path as it was recorded, or one a copy whose record names the other's transcript
 * as the path it had where it ran. Every query that matches copies to originals says it so.
 */
macro_rules! same_conversation {
    ($o:literal, $s:literal) => {
        concat!(
            $o,
            ".provider = ",
            $s,
            ".provider AND ",
            $o,
            ".conversation = ",
            $s,
            ".conversation AND (",
            $o,
            ".project_path IS ",
            $s,
            ".project_path OR ",
            unparsed_copy!($s),
            " OR ",
            unparsed_copy!($o),
            " OR (",
            $s,
            ".copy_label IS NOT NULL AND ",
            names_transcript!($s, concat!($o, ".main_item")),
            ") OR (",
            $o,
            ".copy_label IS NOT NULL AND ",
            names_transcript!($o, concat!($s, ".main_item")),
            ")) AND ",
            $o,
            ".id <> ",
            $s,
            ".id"
        )
    };
}

pub(super) use {names_transcript, same_conversation, unparsed_copy};

/**
 * Where the session `$x` stands among the copies of its conversation shown alone, as an SQL
 * row value, the least first: one the owner filed by hand, starred or put in Trash, then the
 * one archived first (by its own transcript's item), then by identity. It changes neither as copies grow
 * nor as they are synced, so the copy shown stays the one shown.
 */
macro_rules! copy_rank {
    ($x:literal) => {
        concat!(
            "(NOT EXISTS (SELECT 1 FROM session_state z WHERE z.location_id = ",
            $x,
            ".location_id AND z.provider = ",
            $x,
            ".provider AND z.session = ",
            $x,
            ".session AND (z.by_hand OR z.starred OR z.trashed_at IS NOT NULL)), COALESCE(",
            $x,
            ".main_item, 9223372036854775807), ",
            $x,
            ".location_id, ",
            $x,
            ".session)"
        )
    };
}

/**
 * The condition that the session `s` is one the library shows (PLAN §7.7): an original, or a
 * copy of a conversation none of whose originals is archived, and of several such copies the
 * first by [`copy_rank`]. The others are hidden everywhere.
 */
pub(super) const SHOWN: &str = concat!(
    "NOT (s.copy_label IS NOT NULL AND EXISTS (SELECT 1 FROM session o WHERE ",
    same_conversation!("o", "s"),
    " AND (o.copy_label IS NULL OR ",
    copy_rank!("o"),
    " < ",
    copy_rank!("s"),
    ")))"
);

/// The location `s` is attributed to (PLAN §7.7, [`Session::ran_at`]), as SQL: of a copy, the
/// location its record makes it, else NULL.
pub(super) const RAN_AT: &str = concat!(
    "CASE WHEN s.copy_label IS NULL THEN s.location_id ELSE ",
    recorded!("r.ran_at", "s"),
    " END"
);

/// What the tag of `s` says when it is a copy attributed to no location ([`CopyOf::tag`]), as
/// SQL: the host its record names when that resolved, else the provider's label. NULL for a
/// session attributed to a location.
pub(super) const COPY_TAG: &str = concat!(
    "CASE WHEN s.copy_label IS NOT NULL AND ",
    recorded!("r.ran_at", "s"),
    " IS NULL THEN COALESCE((SELECT r.host FROM copy_record r WHERE r.location_id = s.location_id
       AND r.provider = s.provider AND r.conversation = s.conversation AND r.resolved),
       s.copy_label) END"
);

/// The session that did something last comes first; those with no time come last.
pub(super) const BY_ACTIVITY: &str =
    "ORDER BY s.ended_at DESC NULLS LAST, s.location_id, s.provider, s.session";

/// What a row says of the session itself, in the order [`Catalogue::record_session`] gives it.
const DERIVED: [&str; 27] = [
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
];

/// What a row says of the session whatever came of its index, after [`DERIVED`]: whether it
/// is a copy, which its provider says from its id alone ([`crate::provider::Provider::copy_of`]).
const COPY: [&str; 3] = ["copy_label", "copy_origin", "copy_ran_on"];

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

    /// The items of `provider` at `location` that are of kind register (PLAN §7.7), by key,
    /// each at the version of it that is read.
    pub(crate) fn register_items(
        &self,
        location: &LocationId,
        provider: &str,
    ) -> Result<Vec<Reading>> {
        self.db.read(|c| {
            Ok(c.prepare(&format!(
                "{} WHERE i.location_id = ?1 AND i.provider = ?2 AND i.kind = 'register'
                 ORDER BY i.key",
                reading()
            ))?
            .query_map(params![location, provider], reading_from)?
            .collect::<rusqlite::Result<_>>()?)
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
                            outdated: row.as_ref().is_some_and(|made| !made.by_these),
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

    /**
     * Says the text of each session whose row says it is searchable is not, unless `docs`
     * (each session with text, with what its row was made from then) holds the text of its
     * row as it is now. Returns how many it said it of: rows that said so of text that is not
     * there, which a crash between the two databases' writes can leave.
     */
    pub(crate) fn unflag_without(&self, docs: &HashMap<SessionId, String>) -> Result<usize> {
        let flagged: Vec<(SessionId, String)> = self.db.read(|c| -> Result<_> {
            Ok(c.prepare(&format!(
                "SELECT s.location_id, s.provider, s.session, {MADE_FROM} FROM session s
                 WHERE s.searchable = 1"
            ))?
            .query_map([], |row| {
                let id = SessionId {
                    location: row.get(0)?,
                    provider: row.get(1)?,
                    key: row.get(2)?,
                };
                Ok((id, row.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?)
        })?;
        let wrong: Vec<SessionId> = (flagged.into_iter())
            .filter(|(id, made)| docs.get(id) != Some(made))
            .map(|(id, _)| id)
            .collect();
        if wrong.is_empty() {
            return Ok(0);
        }
        self.db.write(|tx| {
            let mut unflag = tx.prepare(
                "UPDATE session SET searchable = 0
                 WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
            )?;
            for id in &wrong {
                unflag.execute(params![id.location, id.provider, id.key])?;
            }
            Ok(wrong.len())
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
     * `copy` is what its provider says it is a copy as, if it is one, whatever came of the
     * index.
     *
     * A session of a conversation the owner deleted for good (PLAN §4.5, §7.7), arriving from
     * any location, is deleted for good in the same transaction, as its key would have been
     * skipped. An original whose row is new takes over what the owner decided about the copy
     * shown in its place until now ([`take_over`]). Returns what became of the row.
     *
     * `known` is the agent's id for the conversation as its provider tells it without a parse
     * ([`crate::provider::Provider::conversation_of`]): a row that could not be parsed keeps
     * the one a parse said, else takes it, so that a session its parser stops on is still of
     * its conversation ([`unparsed_row`]).
     */
    pub(crate) fn record_session(
        &self,
        id: &SessionId,
        parser_version: u32,
        from: Hash,
        indexed: Indexed<'_>,
        copy: Option<&CopyWords>,
        known: Option<&str>,
    ) -> Result<RowMade> {
        let nothing = SessionIndex::default();
        let (derived, failure, retry) = match indexed {
            Indexed::Parsed(derived) => (Some(derived), None, false),
            Indexed::Faulted(reason) => (None, Some(reason), false),
            Indexed::NotRead(reason) => (None, Some(reason), true),
        };
        let index = derived.as_ref().map_or(&nothing, |derived| derived.index);
        let main = derived.as_ref().and_then(|derived| derived.main);
        /* Under the location's own drive mount, where it has one (PLAN §6.3). A copy ran on a
        machine it doesn't name, whose mount the location that kept it doesn't know: its path
        is read with the usual one (PLAN §8). */
        let mount = match copy {
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
        let columns: Vec<&str> = DERIVED.into_iter().chain(COPY).chain(MADE).collect();
        /* Of a row that is there, a session that was not parsed changes only whether it is a
        copy, why, and what the row is made by and from. */
        let unparsed: Vec<&str> = COPY.into_iter().chain(MADE).collect();
        let set = if derived.is_some() {
            &columns[..]
        } else {
            &unparsed[..]
        };
        self.db.write(|tx| {
            /* A session the owner deleted for good while it was parsed gets no row: there is
            nothing of it left to make one from. */
            if !has_items(tx, id)? {
                return Ok(RowMade::Made { filed: false });
            }
            /* New to its conversation: no row of it said which conversation it is before (one
            made while its items could not be read says none). Only what was decided of a copy
            shown alone ever moves, so taking over again later moves nothing twice. */
            let new: bool = tx.query_row(
                "SELECT NOT EXISTS (
                     SELECT 1 FROM session
                     WHERE location_id = ?1 AND provider = ?2 AND session = ?3
                       AND conversation IS NOT NULL
                 )",
                params![id.location, id.provider, id.key],
                |row| row.get(0),
            )?;
            /* Before the row is made: the copy shown in its place until now. */
            let shown = match (&index.conversation, copy, new && derived.is_some()) {
                (Some(conversation), None, true) => shown_copy(
                    tx,
                    &id.provider,
                    conversation,
                    index.project_path.as_deref(),
                    main.map(|main| main.item),
                )?,
                _ => None,
            };
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
                    copy.map(|copy| &copy.label),
                    copy.map(|copy| &copy.unsaid),
                    copy.map(|copy| &copy.ran_on),
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
                return unparsed_row(tx, id, copy.is_some(), known);
            }
            if let Some(conversation) = &index.conversation
                && (tombstoned(tx, id, conversation, index.project_path.as_deref())?
                    || (copy.is_some() && path_tombstoned(tx, id, conversation)?))
            {
                super::trash::forget(tx, id)?;
                return Ok(RowMade::Deleted);
            }
            if let Some(shown) = shown {
                take_over(tx, &shown, id)?;
            }
            let (_, ran_at) = folder::filing_of(tx, id)?;
            Ok(RowMade::Made {
                filed: folder::file_by_rule(tx, id, project.as_ref(), ran_at.as_ref())?,
            })
        })
    }

    /**
     * The other sessions of the session `id`'s conversation (PLAN §7.7), originals first, each
     * with the version of its main transcript that is read, and `id`'s: what
     * [`crate::archive::Archive::kin`] compares.
     */
    pub(crate) fn kin_read(&self, id: &SessionId) -> Result<Vec<KinRead>> {
        self.db.read(|c| {
            Ok(c.prepare(concat!(
                "SELECT o.location_id, o.provider, o.session, ",
                copy_columns!("o"),
                ",
                        ov.id, COALESCE(ov.length, 0), ov.hash,
                        sv.id, COALESCE(sv.length, 0), sv.hash
                 FROM session s
                 JOIN session o ON ",
                same_conversation!("o", "s"),
                " LEFT JOIN version sv ON sv.item_id = s.main_item AND sv.number = s.reading_version
                 LEFT JOIN version ov ON ov.item_id = o.main_item AND ov.number = o.reading_version
                 WHERE s.location_id = ?1 AND s.provider = ?2 AND s.session = ?3
                 ORDER BY o.copy_label IS NOT NULL, o.location_id, o.session"
            ))?
            .query_map(params![id.location, id.provider, id.key], |row| {
                Ok(KinRead {
                    id: SessionId {
                        location: row.get(0)?,
                        provider: row.get(1)?,
                        key: row.get(2)?,
                    },
                    copy: copy_from(row, 3)?,
                    theirs: (row.get(8)?, row.get(9)?, row.get(10)?),
                    ours: (row.get(11)?, row.get(12)?, row.get(13)?),
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
                unrevived(tx, &location, provider, classifier)?;
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
 * Keeps a session deleted for good from coming back under another id after its items are
 * classified again (PLAN §4.5): when the transcript the owner deleted is now classified as the
 * own transcript of a session that has no other, the archived items that session now has (files
 * that belonged to no session before, say) are buried with the deleted one.
 */
fn unrevived(
    tx: &Connection,
    location: &LocationId,
    provider: &str,
    classifier: &dyn Fn(&str) -> (ItemKind, Option<String>),
) -> Result<()> {
    let deleted: Vec<(String, String)> = tx
        .prepare(
            "SELECT key, session FROM tombstone_item WHERE location_id = ?1 AND provider = ?2",
        )?
        .query_map(params![location, provider], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    /* Only a deleted transcript's key can make a session own transcript now. */
    let revived: Vec<(String, SessionId)> = (deleted.into_iter())
        .filter_map(|(key, tombstone)| match classifier(&key) {
            (ItemKind::Transcript, Some(now)) => Some((
                tombstone,
                SessionId {
                    location: location.clone(),
                    provider: provider.to_string(),
                    key: now,
                },
            )),
            _ => None,
        })
        .collect();
    if revived.is_empty() {
        return Ok(());
    }
    let mut select = tx.prepare(
        "SELECT id, key, item_id, kind FROM item
         WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
    )?;
    for (tombstone, now) in revived {
        let items: Vec<(i64, String, String, ItemKind)> = select
            .query_map(params![now.location, now.provider, now.key], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        if items.is_empty() || items.iter().any(|it| it.3 == ItemKind::Transcript) {
            continue;
        }
        let items: Vec<(i64, String, String)> = (items.into_iter())
            .map(|(item, key, folder, _)| (item, key, folder))
            .collect();
        let deleted = SessionId {
            key: tombstone,
            ..now.clone()
        };
        super::trash::bury(tx, &deleted, &items)?;
        tx.execute(
            "DELETE FROM session_state WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
            params![now.location, now.provider, now.key],
        )?;
    }
    Ok(())
}

/**
 * The copy of the conversation `conversation` (of `provider`, started in `project`, its main
 * transcript the item `main`) that the library shows now ([`SHOWN`]), if one is: none while an
 * original of it is archived. A copy is of it as [`same_conversation`] says.
 */
fn shown_copy(
    connection: &Connection,
    provider: &str,
    conversation: &str,
    project: Option<&str>,
    main: Option<i64>,
) -> rusqlite::Result<Option<SessionId>> {
    connection
        .query_row(
            &format!(
                "SELECT s.location_id, s.provider, s.session FROM session s
                 WHERE s.provider = ?1 AND s.conversation = ?2
                   AND (s.project_path IS ?3 OR {})
                   AND s.copy_label IS NOT NULL AND {SHOWN}",
                names_transcript!("s", "?4"),
            ),
            params![provider, conversation, project, main],
            |row| {
                Ok(SessionId {
                    location: row.get(0)?,
                    provider: row.get(1)?,
                    key: row.get(2)?,
                })
            },
        )
        .optional()
}

/**
 * Moves to the original `original`, whose row is new, what the owner decided about the copy
 * `copy` while it was shown alone in its place (PLAN §7.7): its folder, star and Trash. Only
 * what was decided of it as a copy moves, and only to an original with nothing decided of its
 * own; then the copy keeps none of it, so nothing moves twice.
 */
fn take_over(
    connection: &Connection,
    copy: &SessionId,
    original: &SessionId,
) -> rusqlite::Result<()> {
    let moved = connection.execute(
        "INSERT OR IGNORE INTO session_state
             (location_id, provider, session, folder_id, by_hand, starred, trashed_at)
         SELECT ?4, ?5, ?6, folder_id, by_hand, starred, trashed_at FROM session_state
         WHERE location_id = ?1 AND provider = ?2 AND session = ?3 AND as_copy",
        params![
            copy.location,
            copy.provider,
            copy.key,
            original.location,
            original.provider,
            original.key
        ],
    )?;
    if moved > 0 {
        connection.execute(
            "DELETE FROM session_state WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
            params![copy.location, copy.provider, copy.key],
        )?;
    }
    Ok(())
}

/**
 * Whether the conversation `conversation` (of `id`'s provider, started in `project`) was
 * deleted for good as another session than `id` (PLAN §4.5): then no session of it is archived
 * again, from any location.
 */
fn tombstoned(
    connection: &Connection,
    id: &SessionId,
    conversation: &str,
    project: Option<&str>,
) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM tombstone
             WHERE provider = ?2 AND conversation = ?4 AND project_path IS ?5
               AND NOT (location_id = ?1 AND session = ?3)
         )",
        params![id.location, id.provider, id.key, conversation, project],
        |row| row.get(0),
    )
}

/**
 * What becomes of the row of the session `id` its index could not parse, made or kept as it
 * was: it takes `known` for its conversation when no parse said one. A copy that never parsed
 * has no project path to match its conversation's by, so a tombstone of its conversation of
 * any path takes it (PLAN §4.5): a copy deleted with its conversation is not archived again
 * because its parser stops on it.
 */
fn unparsed_row(
    tx: &Connection,
    id: &SessionId,
    copy: bool,
    known: Option<&str>,
) -> Result<RowMade> {
    let identity = params![id.location, id.provider, id.key, known];
    tx.execute(
        "UPDATE session SET conversation = COALESCE(conversation, ?4)
         WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
        identity,
    )?;
    let (conversation, project): (Option<String>, Option<String>) = tx.query_row(
        "SELECT conversation, project_path FROM session
         WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
        params![id.location, id.provider, id.key],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let Some(conversation) = conversation else {
        return Ok(RowMade::Made { filed: false });
    };
    let deleted = match project.as_deref() {
        Some(project) => tombstoned(tx, id, &conversation, Some(project))?,
        None if copy => tx.query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM tombstone
                 WHERE provider = ?2 AND conversation = ?4
                   AND NOT (location_id = ?1 AND session = ?3)
             )",
            params![id.location, id.provider, id.key, conversation],
            |row| row.get(0),
        )?,
        None => false,
    };
    if deleted {
        super::trash::forget(tx, id)?;
        return Ok(RowMade::Deleted);
    }
    Ok(RowMade::Made { filed: false })
}

/**
 * Whether the record of the copy `id` (of the conversation `conversation`) names a transcript
 * the owner deleted for good where it ran, as [`names_transcript`] names one archived: a
 * deleted conversation does not come back as a copy matched by its path alone (PLAN §4.5).
 */
fn path_tombstoned(
    connection: &Connection,
    id: &SessionId,
    conversation: &str,
) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM copy_record r
             JOIN tombstone_item ti ON ti.provider = r.provider AND ti.key = r.key
             JOIN location l ON l.id = ti.location_id AND l.kind IN ('wsl', 'ssh')
             JOIN location_provider lp
               ON lp.location_id = ti.location_id AND lp.provider = ti.provider
             WHERE r.location_id = ?1 AND r.provider = ?2 AND r.conversation = ?3
               AND substr(r.path, 1, length(lp.root) + 1) = lp.root || '/'
         )",
        params![id.location, id.provider, conversation],
        |row| row.get(0),
    )
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
    let set = connection.execute(
        &upsert("session_state", &names, &names),
        params_from_iter(
            identity
                .into_iter()
                .chain(columns.iter().map(|(_, value)| *value)),
        ),
    )?;
    /* Whether it is decided of a copy shown alone, which its original takes over (PLAN §7.7):
    not of one hidden behind its original, nor of a session not yet known to be a copy. */
    connection.execute(
        &format!(
            "UPDATE session_state SET as_copy = EXISTS (
                 SELECT 1 FROM session s
                 WHERE s.location_id = ?1 AND s.provider = ?2 AND s.session = ?3
                   AND s.copy_label IS NOT NULL AND {SHOWN}
             )
             WHERE location_id = ?1 AND provider = ?2 AND session = ?3"
        ),
        identity,
    )?;
    Ok(set)
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

/// What a session is a copy as, from the columns [`copy_columns`] names, at `at` and after it.
fn copy_from(row: &Row<'_>, at: usize) -> rusqlite::Result<Option<CopyOf>> {
    let (Some(label), Some(unsaid)) = (row.get(at)?, row.get(at + 1)?) else {
        return Ok(None);
    };
    let ran_on: Option<String> = row.get(at + 2)?;
    let ran_on = || ran_on.clone().unwrap_or_default();
    let origin = match (row.get(at + 4)?, row.get(at + 3)?) {
        (Some(location), _) => Origin::Location {
            ran_on: ran_on(),
            location,
        },
        (None, Some(host)) => Origin::Host {
            ran_on: ran_on(),
            host,
        },
        (None, None) => Origin::Unsaid(unsaid),
    };
    Ok(Some(CopyOf { label, origin }))
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

    /// A copy its parser stopped on is a copy all the same: its provider says so from its id.
    #[test]
    fn a_copy_that_could_not_be_parsed_is_still_a_copy() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::create(dir.path()).unwrap();
        let catalogue = archive.catalogue();
        let location = catalogue
            .add_location(crate::location::LocationKind::Local, "fixture")
            .unwrap()
            .id;
        catalogue
            .db
            .write(|tx| {
                tx.execute(
                    "INSERT INTO item (location_id, provider, key, item_id, kind, session,
                                       seen_length, seen_stamp)
                     VALUES (?1, 'fixture', 'one', 'one', 'transcript', 'copied', 0, '')",
                    [&location],
                )?;
                Ok::<_, CatalogueError>(())
            })
            .unwrap();
        let id = SessionId {
            location,
            provider: "fixture".to_string(),
            key: "copied".to_string(),
        };
        let words = CopyWords {
            label: "A copy".to_string(),
            unsaid: "Ran elsewhere.".to_string(),
            ran_on: "Ran on".to_string(),
        };
        catalogue
            .record_session(
                &id,
                1,
                Hash::of(b""),
                Indexed::Faulted("no"),
                Some(&words),
                None,
            )
            .unwrap();
        let row = catalogue.session(&id).unwrap().unwrap();
        let copy = CopyOf {
            label: "A copy".to_string(),
            origin: Origin::Unsaid("Ran elsewhere.".to_string()),
        };
        assert_eq!((row.ran_at(), &row.copy), (None, &Some(copy)));
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
        assert_eq!(
            catalogue
                .record_session(&id, 1, Hash::of(b""), Indexed::Parsed(derived), None, None)
                .unwrap(),
            RowMade::Made { filed: false }
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
