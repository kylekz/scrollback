//! Sessions in the catalogue (PLAN §2.1, §4.3): which version of each of a session's items is
//! read, the derived row an index writes for the session, and what tells that the row is no
//! longer of what the archive holds.
//!
//! The `session` table is derived (PLAN §2.2): [`Catalogue::drop_index`] empties it and the
//! next index fills it again from the archive alone. What the owner decided about a session is
//! in `session_state`, under the session's identity, and stays while its row here is missing.

use std::collections::HashMap;

use toolbox_store::rusqlite::types::Type;
use toolbox_store::rusqlite::{
    self, Connection, OptionalExtension as _, Row, ToSql, params, params_from_iter,
};

use super::{Catalogue, CatalogueError, Result, folder};
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
    /// Why the session could not be parsed at all. Nothing else is derived of it then.
    pub failure: Option<String>,
    pub parser_version: u32,
    pub index_version: u32,
    pub filed: Filed,
    pub starred: bool,
    /// The main transcript has a newer version than the one read, held back as a loss
    /// (PLAN §4.2), and the owner has not chosen it.
    pub held_back: bool,
}

/// One item of a session with the version of it that is read: the one the owner chose, else
/// the newest that is not held back (PLAN §4.2).
#[derive(Clone, Debug)]
pub(crate) struct Reading {
    pub item: i64,
    pub key: String,
    pub kind: ItemKind,
    pub version: i64,
    pub length: u64,
    /// The item's newest version, which is the one read unless it is held back or the owner
    /// chose another.
    pub newest: i64,
    pub newest_number: u32,
    pub newest_length: u64,
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

/// What an index derived from a session.
pub(crate) struct Derived<'a> {
    pub main_item: Option<i64>,
    pub index: &'a SessionIndex,
    pub ending: Option<&'a Ending>,
}

/// What tells one state of a session's items from another: each item's version that is read
/// and its newest, with their lengths. A row made from one state is stale in any other.
pub(crate) fn fingerprint(readings: &[Reading]) -> Hash {
    let mut hasher = Hasher::new();
    for reading in readings {
        hasher.update(
            format!(
                "{} {} {} {} {}\n",
                reading.item,
                reading.version,
                reading.length,
                reading.newest,
                reading.newest_length
            )
            .as_bytes(),
        );
    }
    hasher.finish()
}

/// Every item that belongs to a session, as a [`Reading`], then its location and session.
const READING: &str = "
    SELECT i.id, i.key, i.kind, r.id, r.length, n.id, n.number, n.length, i.location_id, i.session
    FROM item i
    JOIN version n ON n.item_id = i.id
     AND n.number = (SELECT max(number) FROM version WHERE item_id = i.id)
    LEFT JOIN session_state st
      ON st.location_id = i.location_id AND st.provider = i.provider AND st.session = i.session
    JOIN version r ON r.item_id = i.id AND r.number = COALESCE(
        (SELECT number FROM version WHERE id = st.reading_version AND item_id = i.id),
        (SELECT max(number) FROM version WHERE item_id = i.id AND held_back = 0))";

/// A session's row with the owner's state of it, as [`session_from`] reads them.
pub(super) const SESSION: &str = "
    SELECT s.location_id, s.provider, s.session, s.main_item, s.title, s.first_prompt,
           s.project_path, s.project, s.branch, s.entrypoint, s.started_at, s.ended_at, s.models,
           s.own_calls, s.own_input, s.own_output, s.own_cache_read, s.own_cache_write,
           s.all_calls, s.all_input, s.all_output, s.all_cache_read, s.all_cache_write,
           s.ending, s.unreadable, s.failure, s.parser_version, s.index_version,
           st.folder_id, COALESCE(st.by_hand, 0), COALESCE(st.starred, 0),
           COALESCE((SELECT v.held_back AND v.id IS NOT st.reading_version FROM version v
                     WHERE v.item_id = s.main_item ORDER BY v.number DESC LIMIT 1), 0)
    FROM session s
    LEFT JOIN session_state st
      ON st.location_id = s.location_id AND st.provider = s.provider AND st.session = s.session";

/// The session that did something last comes first; those with no time come last.
pub(super) const BY_ACTIVITY: &str =
    "ORDER BY s.ended_at DESC NULLS LAST, s.location_id, s.provider, s.session";

/// The derived columns of `session`, in the order [`Catalogue::record_session`] gives them.
const DERIVED: [&str; 26] = [
    "main_item",
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
    "failure",
    "parser_version",
    "index_version",
    "indexed_from",
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
            Ok(c.prepare(&format!(
                "{READING} WHERE i.location_id = ?1 AND i.provider = ?2 AND i.session = ?3
                 ORDER BY i.key"
            ))?
            .query_map(params![id.location, id.provider, id.key], reading_from)?
            .collect::<rusqlite::Result<_>>()?)
        })
    }

    /**
     * The sessions of `provider`, at `location` or at every location, that are to be indexed:
     * those with no row, with a row made by another version of the parser or of the index, or
     * with a row made from other versions of their items than are read now. A session is its
     * items with at least one transcript among them, its own or a subagent's.
     */
    pub(crate) fn stale(
        &self,
        location: Option<&LocationId>,
        provider: &str,
        parser_version: u32,
    ) -> Result<Vec<Stale>> {
        self.db.read(|c| {
            /* Each row: whether it is of these versions, what it was made from, and when its
            session ended. */
            let indexed: HashMap<(LocationId, String), (bool, Hash, Option<i64>)> = c
                .prepare(
                    "SELECT location_id, session, parser_version = ?2 AND index_version = ?3,
                            indexed_from, ended_at
                     FROM session WHERE provider = ?1",
                )?
                .query_map(params![provider, parser_version, INDEX_VERSION], |row| {
                    Ok((
                        (row.get(0)?, row.get(1)?),
                        (row.get(2)?, row.get(3)?, row.get(4)?),
                    ))
                })?
                .collect::<rusqlite::Result<_>>()?;
            let mut stale = Vec::new();
            let mut close = |session: (LocationId, String), readings: &mut Vec<Reading>| {
                let read = |reading: &&Reading| {
                    matches!(reading.kind, ItemKind::Transcript | ItemKind::Subagent)
                };
                let row = indexed.get(&session);
                let current = row
                    .is_some_and(|(current, from, _)| *current && *from == fingerprint(readings));
                if readings.iter().any(|reading| read(&reading)) && !current {
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
                        recency: row.map_or(i64::MAX, |(_, _, ended)| ended.unwrap_or(i64::MIN)),
                    });
                }
                readings.clear();
            };
            let mut select = c.prepare(&format!(
                "{READING} WHERE i.provider = ?1 AND i.session IS NOT NULL
                   AND (?2 IS NULL OR i.location_id = ?2)
                 ORDER BY i.location_id, i.session, i.key"
            ))?;
            let mut rows = select.query(params![provider, location])?;
            let mut open: Option<(LocationId, String)> = None;
            let mut readings = Vec::new();
            while let Some(row) = rows.next()? {
                let session = (row.get(8)?, row.get(9)?);
                if let Some(last) = open.take_if(|last| *last != session) {
                    close(last, &mut readings);
                }
                open = Some(session);
                readings.push(reading_from(row)?);
            }
            if let Some(last) = open {
                close(last, &mut readings);
            }
            Ok(stale)
        })
    }

    /**
     * Records what an index of the session `id` found, or why the session could not be parsed,
     * with what the row is made by and from: the parser's version, the index's, and
     * `indexed_from`, the [`fingerprint`] of the items that were read. A session that was
     * parsed is then filed by the rules, unless the owner filed it by hand (PLAN §8). Returns
     * whether the rules moved it into a folder.
     */
    pub(crate) fn record_session(
        &self,
        id: &SessionId,
        parser_version: u32,
        indexed_from: Hash,
        derived: std::result::Result<Derived<'_>, &str>,
    ) -> Result<bool> {
        let nothing = SessionIndex::default();
        let (derived, failure) = match derived {
            Ok(derived) => (derived, None),
            Err(reason) => (
                Derived {
                    main_item: None,
                    index: &nothing,
                    ending: None,
                },
                Some(reason),
            ),
        };
        let index = derived.index;
        /* Only this machine is a location so far. A WSL distro's drive mount comes with the
        distro (PLAN §6.3). */
        let project = index
            .project_path
            .as_deref()
            .and_then(|path| NormalPath::new(path, None));
        let (ending, unreadable) = match derived.ending {
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
        self.db.write(|tx| {
            tx.execute(
                &upsert("session", &DERIVED),
                params![
                    id.location,
                    id.provider,
                    id.key,
                    derived.main_item,
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
                    failure,
                    parser_version,
                    INDEX_VERSION,
                    indexed_from
                ],
            )?;
            /* A session that could not be parsed stays where it was filed. */
            Ok(failure.is_none() && folder::file_by_rule(tx, id, project.as_ref())?)
        })
    }

    /**
     * Holds the version `version` back for how its transcript parses, or stops holding it
     * back for that (PLAN §4.2). A version no longer held for how it parses is still held back
     * while it is shorter than the copy being read.
     */
    pub(crate) fn hold_for_parse(&self, version: i64, held: bool) -> Result<()> {
        self.execute(
            "UPDATE version SET held_by_parse = ?2,
                 held_back = ?2 OR COALESCE(length < (
                     SELECT length FROM version reading
                     WHERE reading.item_id = version.item_id
                       AND reading.held_back = 0
                       AND reading.number < version.number
                     ORDER BY reading.number DESC LIMIT 1
                 ), 0)
             WHERE id = ?1",
            params![version, held],
        )
    }

    /**
     * Makes `version`, a version of one of the session's items (its main transcript's, in the
     * history the reader shows), the one that is read of that item, held back or not; `None`
     * goes back to reading the newest version that is not held back (PLAN §4.2). The session's
     * items read differently at once; its row is of the other version until the next index.
     */
    pub fn set_reading_version(&self, session: &SessionId, version: Option<i64>) -> Result<()> {
        self.db.write(|tx| {
            if let Some(version) = version {
                let of_session: bool = tx.query_row(
                    "SELECT EXISTS (
                         SELECT 1 FROM version v JOIN item i ON i.id = v.item_id
                         WHERE v.id = ?4 AND i.location_id = ?1 AND i.provider = ?2
                           AND i.session = ?3
                     )",
                    params![session.location, session.provider, session.key, version],
                    |row| row.get(0),
                )?;
                if !of_session {
                    return Err(CatalogueError::NotOfSession(version));
                }
            }
            set_state(tx, session, &[("reading_version", &version)])?;
            Ok(())
        })
    }

    pub fn set_starred(&self, session: &SessionId, starred: bool) -> Result<()> {
        self.db
            .write(|tx| Ok(set_state(tx, session, &[("starred", &starred)]).map(drop)?))
    }

    /// Empties the derived tables (PLAN §2.2). The next index fills them again from the
    /// archive; what the owner decided about each session stays.
    pub fn drop_index(&self) -> Result<()> {
        self.execute("DELETE FROM session", [])
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
        &upsert("session_state", &names),
        params_from_iter(
            identity
                .into_iter()
                .chain(columns.iter().map(|(_, value)| *value)),
        ),
    )
}

/// The statement that sets `columns` of the row `table` has for a session, making the row if
/// there is none: the session's identity is its first three parameters, the columns the rest.
fn upsert(table: &str, columns: &[&str]) -> String {
    format!(
        "INSERT INTO {table} (location_id, provider, session, {})
         VALUES (?1, ?2, ?3, {})
         ON CONFLICT (location_id, provider, session) DO UPDATE SET {}",
        columns.join(", "),
        (4..columns.len() + 4)
            .map(|at| format!("?{at}"))
            .collect::<Vec<_>>()
            .join(", "),
        columns
            .iter()
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
        length: row.get(4)?,
        newest: row.get(5)?,
        newest_number: row.get(6)?,
        newest_length: row.get(7)?,
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
        title: row.get(4)?,
        first_prompt: row.get(5)?,
        project_path: row.get(6)?,
        project: row.get(7)?,
        branch: row.get(8)?,
        entrypoint: row.get(9)?,
        started: row.get(10)?,
        ended: row.get(11)?,
        models: serde_json::from_str(&row.get::<_, String>(12)?)
            .map_err(|_| invalid(12, "models"))?,
        own: usage(13)?,
        all: usage(18)?,
        ending: match row.get::<_, Option<String>>(23)?.as_deref() {
            None => None,
            Some("whole") => Some(Ending::Whole),
            Some("mid_record") => Some(Ending::MidRecord),
            Some("unreadable") => Some(Ending::Unreadable(row.get(24)?)),
            Some(_) => return Err(invalid(23, "ending")),
        },
        failure: row.get(25)?,
        parser_version: row.get(26)?,
        index_version: row.get(27)?,
        filed: match (row.get::<_, Option<i64>>(28)?, row.get(29)?) {
            (None, _) => Filed::Unsorted,
            (Some(folder), false) => Filed::ByRule(FolderId(folder)),
            (Some(folder), true) => Filed::ByHand(FolderId(folder)),
        },
        starred: row.get(30)?,
        held_back: row.get(31)?,
    })
}
