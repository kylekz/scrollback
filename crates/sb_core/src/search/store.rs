//! The search text (PLAN §10): `search.db` beside the catalogue, a database of its own. It is
//! derived from the archive alone, so it is never backed up and is made again whenever it is
//! missing, unreadable or of another layout; and it is apart from the catalogue so that what a
//! session said is not copied into the catalogue's backups, nor left in its free pages, after
//! the owner deletes the session for good.
//!
//! A `doc` is a session whose text is indexed, by its identity, with what its row in the
//! catalogue was made from when the text was (`made`): the text is out of date when that row
//! is made from anything else. A span's id is its doc's id times [`SPANS`] plus its place: the
//! title, if the session has one, is at place 0, and its prompts, replies and tool calls follow
//! from place 1 in the order of its conversation.
//!
//! Text taken out of the index, written again ([`SearchIndex::record`]) or forgotten
//! ([`SearchIndex::forget`]), is marked deleted, and goes as the index merges its parts: FTS5's
//! `secure-delete`, which takes it out at once, costs a search of the index for each of a
//! span's trigrams, far more than writing the span, and an index after an update writes most
//! of the text again; nor can the two be mixed, since taking out securely what was marked
//! deleted before leaves the index damaged. Of a session made again as it was, nothing is
//! written; of one that grew, its new spans.
//!
//! Deleting for good is meant to leave nothing (PLAN §4.5). Its text is forgotten at once, so
//! search no longer finds it; SQLite's `secure_delete` overwrites what a write frees; and the
//! write-ahead log is emptied. What is still left of it then, its entries the index marked
//! deleted and copies of cells a page kept as it was rebalanced, goes when the archive's
//! background removal scrubs the file ([`SearchIndex::scrub`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use toolbox_store::rusqlite::{self, Connection, OptionalExtension as _, params};
use toolbox_store::{Database, OpenOptions, Schema};

use super::{MatchKind, SearchError};
use crate::archive::disk;
use crate::catalogue::SessionId;
use crate::transcript::{At, Span};

/// The search text's file in the archive's folder.
pub(crate) const SEARCH_DATABASE: &str = "search.db";

/// How many spans a session has room for: a span's id is its doc's id times this, plus its
/// place. A session with more has the first of them indexed.
pub(super) const SPANS: i64 = 1 << 24;

/// How much of one span's text is indexed: the start of a pasted log or a blob of base64 is
/// what is worth finding of it. Nine spans in ten are under a few kilobytes, and the longest
/// prompts and commands seen run to a few tens; a span cut here says so.
const SPAN_BYTES: usize = 64 * 1024;

/// The layout of `search.db`. A change to it is a new step, or a new domain, which makes the
/// file again from the archive: the second domain is the one whose spans say where in a
/// subagent's conversation they are.
const SCHEMA: Schema = Schema {
    domain: "scrollback-search-2",
    steps: &[TEXT, MARKED_DELETED],
};

/// Text taken out of the index is marked deleted, not taken out at once (the module docs).
const MARKED_DELETED: &str = "
INSERT INTO span_text (span_text, rank) VALUES ('secure-delete', 0);
";

/// `detail = none`: the index holds which spans have each run of three characters, not where,
/// which is a quarter of the size; whether a span holds a term is then read from its text
/// (`super::text`), so what is found is exact.
const TEXT: &str = "
CREATE TABLE doc (
    id INTEGER PRIMARY KEY,
    location_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    session TEXT NOT NULL,
    made TEXT NOT NULL,
    UNIQUE (location_id, provider, session)
) STRICT;

-- kind is title, prompt, reply, command or path. A match opens the session at turn and block
-- (for a subagent's text, the call it ran under), or at the subagent unattached that no call
-- is known to have started; a title, at its start. subagent: the item of the subagent's
-- conversation the text is in, and its turn and block there (sub_turn, sub_block). cut: only
-- the start of the span's text is here.
CREATE TABLE span (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    subagent TEXT,
    sub_turn INTEGER,
    sub_block INTEGER,
    turn INTEGER,
    block INTEGER,
    unattached INTEGER,
    cut INTEGER NOT NULL,
    text TEXT NOT NULL
) STRICT;

CREATE VIRTUAL TABLE span_text USING fts5 (
    text,
    content = 'span',
    content_rowid = 'id',
    tokenize = 'trigram case_sensitive 0 remove_diacritics 1',
    detail = none
);

INSERT INTO span_text (span_text, rank) VALUES ('secure-delete', 1);
";

/// A session's text, made of its row in the catalogue as `made` says
/// ([`crate::catalogue::Catalogue::made`]).
pub(crate) struct Text {
    pub id: SessionId,
    pub made: String,
    pub title: Option<String>,
    pub spans: Vec<Kept>,
}

impl Text {
    /// About how many bytes the text takes in memory.
    pub(crate) fn bytes(&self) -> usize {
        let title = self.title.as_ref().map_or(0, String::len);
        let spans = self.spans.iter().map(|kept| {
            let subagent = kept.span.subagent.as_ref().map_or(0, |it| it.item.len());
            std::mem::size_of::<Kept>() + kept.span.text.len() + subagent
        });
        title + spans.sum::<usize>()
    }
}

/// A span as the index keeps it: the start of its text, at most [`SPAN_BYTES`], and whether
/// that is all of it.
pub(crate) struct Kept {
    pub span: Span,
    pub cut: bool,
}

/// `spans` as the index keeps them ([`Kept`]): what is left out of each is let go at once, so
/// a session's text waiting to be written holds a few tens of kilobytes of each span at most.
pub(crate) fn kept(spans: Vec<Span>) -> Vec<Kept> {
    spans
        .into_iter()
        .map(|mut span| {
            let kept = start(&span.text, SPAN_BYTES).len();
            let cut = kept < span.text.len();
            if cut {
                span.text.truncate(kept);
                span.text.shrink_to_fit();
            }
            Kept { span, cut }
        })
        .collect()
}

/// The open `search.db`.
#[derive(Debug)]
pub(crate) struct SearchIndex {
    db: Database,
    log: Log,
    /// Held by whoever writes a session's text and checks first that the catalogue still has
    /// its row as it was made from, until the row says its text is searchable; by whoever says
    /// that of text already written; and by whoever forgets a session. So no text is written
    /// after the session it is of was deleted for good, and no row says it is searchable when
    /// its text is not there.
    writing: Mutex<()>,
}

impl SearchIndex {
    /**
     * Opens `search.db` in the archive folder `dir`, making it anew when it is missing, or is
     * not a file this build can read: the archive has everything it holds. Returns it, and
     * whether it is new, and so holds no session's text. A file another program holds is
     * [`SearchError::Unavailable`] after a second at most, and is left as it is.
     */
    pub(crate) fn open(dir: &Path) -> Result<(Self, bool), SearchError> {
        let path = dir.join(SEARCH_DATABASE);
        let mut anew = !path.exists();
        if !anew {
            in_use(&path)?;
        }
        let options = OpenOptions::new(&SCHEMA);
        let db = match Database::open(&disk::for_sqlite(&path, disk::SQLITE_FILE), &options) {
            Err(
                toolbox_store::Error::Corrupt { .. }
                | toolbox_store::Error::Foreign(_)
                | toolbox_store::Error::Newer { .. }
                | toolbox_store::Error::MigrationChanged { .. }
                | toolbox_store::Error::MigrationFailed { .. },
            ) => {
                log::warn!("{} is made again from the archive", path.display());
                for suffix in ["", "-wal", "-shm"] {
                    let mut file = path.clone().into_os_string();
                    file.push(suffix);
                    let _ = std::fs::remove_file(file);
                }
                anew = true;
                Database::open(&disk::for_sqlite(&path, disk::SQLITE_FILE), &options)?
            }
            opened => opened?,
        };
        /* What a write frees is overwritten, on the one connection that writes. */
        db.write(|tx| Ok::<_, SearchError>(tx.pragma_update(None, "secure_delete", true)?))?;
        Ok((
            Self {
                db,
                log: Log::new(path),
                writing: Mutex::default(),
            },
            anew,
        ))
    }

    /// Empties the log, if a read held it the last time it was to be emptied.
    pub(crate) fn retry_empty_log(&self) {
        self.log.retry();
    }

    pub(super) fn read<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, SearchError>,
    ) -> Result<T, SearchError> {
        self.db.read(f)
    }

    /// Every session whose text is indexed, with what its row was made from then.
    pub(crate) fn docs(&self) -> Result<HashMap<SessionId, String>, SearchError> {
        self.db.read(|c| {
            Ok(
                c.prepare("SELECT location_id, provider, session, made FROM doc")?
                    .query_map([], |row| {
                        let id = SessionId {
                            location: row.get(0)?,
                            provider: row.get(1)?,
                            key: row.get(2)?,
                        };
                        Ok((id, row.get(3)?))
                    })?
                    .collect::<rusqlite::Result<_>>()?,
            )
        })
    }

    /**
     * Replaces the text of each of `texts`' sessions with its own, in one transaction: of each
     * that `still` says, once nothing else writes or forgets, the catalogue still has the row
     * the text was made of. Then, before anything else writes or forgets, `written` hears
     * those whose text is now that, each session with what its text was made of, for their
     * rows to say so. Returns how many were written: text the index has already is not.
     */
    pub(crate) fn record(
        &self,
        texts: &[Text],
        still: impl Fn(&Text) -> Result<bool, SearchError>,
        written: impl FnOnce(&[(SessionId, String)]) -> Result<(), SearchError>,
    ) -> Result<usize, SearchError> {
        let recorded = (|| {
            let _writing = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
            let (done, wrote) = self.write_texts(texts, still)?;
            written(&done)?;
            Ok(wrote)
        })();
        self.log.retry();
        recorded
    }

    /**
     * Of `rows`, each a session with what its row is made from, those whose text is already
     * of that (an index stopped before it said so, say): `kept` hears them before anything
     * else writes or forgets, for their rows to say so. Returns them.
     */
    pub(crate) fn keep(
        &self,
        rows: Vec<(SessionId, String)>,
        kept: impl FnOnce(&[(SessionId, String)]) -> Result<(), SearchError>,
    ) -> Result<Vec<(SessionId, String)>, SearchError> {
        if rows.is_empty() {
            return Ok(rows);
        }
        let _writing = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        let same: Vec<_> = self.db.read(|c| {
            let mut made = c.prepare(
                "SELECT made FROM doc WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
            )?;
            let mut same = Vec::new();
            for (id, row) in rows {
                let text: Option<String> = made
                    .query_row(params![id.location, id.provider, id.key], |row| row.get(0))
                    .optional()?;
                if text.as_ref() == Some(&row) {
                    same.push((id, row));
                }
            }
            Ok::<_, SearchError>(same)
        })?;
        kept(&same)?;
        Ok(same)
    }

    /// Writes the text of those of `texts` that `still` says are of their rows, under
    /// [`SearchIndex::writing`]: [`SearchIndex::record`]. Returns them, and how many of them
    /// it wrote.
    fn write_texts(
        &self,
        texts: &[Text],
        still: impl Fn(&Text) -> Result<bool, SearchError>,
    ) -> Result<(Vec<(SessionId, String)>, usize), SearchError> {
        let mut kept = Vec::new();
        for text in texts {
            if still(text)? {
                kept.push(text);
            }
        }
        if kept.is_empty() {
            return Ok((Vec::new(), 0));
        }
        self.db.write(|tx| {
            let mut wrote = 0;
            let mut made = tx.prepare(
                "SELECT made FROM doc WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
            )?;
            let mut doc = tx.prepare(
                "INSERT INTO doc (location_id, provider, session, made) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (location_id, provider, session) DO UPDATE SET made = excluded.made
                 RETURNING id",
            )?;
            let mut span = tx.prepare(
                "INSERT INTO span (id, kind, subagent, sub_turn, sub_block, turn, block,
                                   unattached, cut, text)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            let mut index = tx.prepare("INSERT INTO span_text (rowid, text) VALUES (?1, ?2)")?;
            let mut same = tx.prepare(
                "SELECT kind = ?2 AND subagent IS ?3 AND sub_turn IS ?4 AND sub_block IS ?5
                        AND turn IS ?6 AND block IS ?7 AND unattached IS ?8 AND cut = ?9
                        AND text = ?10, text
                 FROM span WHERE id = ?1",
            )?;
            let mut unindex = tx.prepare(
                "INSERT INTO span_text (span_text, rowid, text) VALUES ('delete', ?1, ?2)",
            )?;
            let mut unspan = tx.prepare("DELETE FROM span WHERE id = ?1")?;
            for text in &kept {
                let id = &text.id;
                /* Made again as it was: the text is the same. */
                let before: Option<String> = made
                    .query_row(params![id.location, id.provider, id.key], |row| row.get(0))
                    .optional()?;
                if before.as_ref() == Some(&text.made) {
                    continue;
                }
                wrote += 1;
                let doc: i64 = doc.query_row(
                    params![id.location, id.provider, id.key, text.made],
                    |row| row.get(0),
                )?;
                /* The places the text has no span at any more: the title's, when it has none,
                and those after its last span. */
                let first = doc * SPANS;
                if text.title.is_none() {
                    forget_range(tx, first, first)?;
                }
                let places = i64::try_from(text.spans.len()).map_or(SPANS, |spans| spans + 1);
                forget_range(tx, first + places.min(SPANS), first + SPANS - 1)?;
                let title = text.title.as_deref().map(|title| {
                    let kept = start(title, SPAN_BYTES);
                    (
                        0,
                        MatchKind::Title,
                        None,
                        None,
                        kept,
                        kept.len() < title.len(),
                    )
                });
                let spans = (1..SPANS).zip(&text.spans).map(|(place, kept)| {
                    let span = &kept.span;
                    let at = Some(span.at);
                    (
                        place,
                        span.kind.into(),
                        span.subagent.as_ref(),
                        at,
                        &*span.text,
                        kept.cut,
                    )
                });
                for (place, kind, subagent, at, kept, cut) in title.into_iter().chain(spans) {
                    let (turn, block, unattached) = match at {
                        None => (None, None, None),
                        Some(At::Block { turn, block }) => (Some(turn), Some(block.0), None),
                        Some(At::Unattached(place)) => (None, None, Some(place)),
                    };
                    let id = first + place;
                    let row = params![
                        id,
                        kind.as_str(),
                        subagent.map(|it| &it.item),
                        subagent.map(|it| it.turn),
                        subagent.map(|it| it.block.0),
                        turn,
                        block,
                        unattached,
                        cut,
                        kept
                    ];
                    /* A span as it was is left: taking it out costs far more than writing
                    it (the module docs). */
                    let before = same
                        .query_row(row, |row| {
                            Ok((row.get::<_, bool>(0)?, row.get::<_, String>(1)?))
                        })
                        .optional()?;
                    match before {
                        Some((true, _)) => continue,
                        Some((false, before)) => {
                            unindex.execute(params![id, before])?;
                            unspan.execute([id])?;
                        }
                        None => {}
                    }
                    span.execute(row)?;
                    index.execute(params![id, kept])?;
                }
            }
            let done = (kept.iter())
                .map(|text| (text.id.clone(), text.made.clone()))
                .collect();
            Ok((done, wrote))
        })
    }

    /// Takes out the text of `sessions`, and leaves none of it in the file or its log
    /// ([`Log::empty`]) but what [`SearchIndex::scrub`] takes.
    pub(crate) fn forget(&self, sessions: &[SessionId]) -> Result<(), SearchError> {
        if sessions.is_empty() {
            return Ok(());
        }
        let forgot = {
            let _writing = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
            self.db.write(|tx| {
                let mut find = tx.prepare(
                    "SELECT id FROM doc WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
                )?;
                let mut docs = Vec::new();
                for id in sessions {
                    docs.extend(
                        find.query_row(params![id.location, id.provider, id.key], |row| {
                            row.get::<_, i64>(0)
                        })
                        .optional()?,
                    );
                }
                let all: usize = tx.query_row("SELECT count(*) FROM doc", [], |row| row.get(0))?;
                if !docs.is_empty() && docs.len() == all {
                    /* Every session's that has text: all of it at once. */
                    tx.execute_batch(
                        "INSERT INTO span_text (span_text) VALUES ('delete-all');
                         DELETE FROM span;
                         DELETE FROM doc;",
                    )?;
                } else {
                    for doc in &docs {
                        forget_spans(tx, *doc)?;
                        tx.execute("DELETE FROM doc WHERE id = ?1", [doc])?;
                    }
                }
                Ok::<_, SearchError>(!docs.is_empty())
            })?
        };
        if forgot {
            self.log.empty();
        }
        Ok(())
    }

    /// How many sessions have text.
    pub(crate) fn texts(&self) -> Result<usize, SearchError> {
        self.db
            .read(|c| Ok(c.query_row("SELECT count(*) FROM doc", [], |row| row.get(0))?))
    }

    /**
     * Takes out of the file what is left of text that went (PLAN §4.5): the entries of
     * replaced text the index has marked deleted and not yet merged away (FTS5's `rebuild`
     * makes the index again from the spans there are), and the copies of cells a page kept
     * after it was rebalanced, which `secure_delete` doesn't reach (`VACUUM` writes the file
     * anew). Then empties the log. Searches go on meanwhile, of the index as it was; writes
     * wait.
     */
    pub(crate) fn scrub(&self) -> Result<(), SearchError> {
        {
            let _writing = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
            self.db.write(|tx| {
                Ok::<_, SearchError>(
                    tx.execute("INSERT INTO span_text (span_text) VALUES ('rebuild')", [])
                        .map(drop)?,
                )
            })?;
            vacuum(&self.log.path)?;
        }
        self.log.empty();
        Ok(())
    }

    /**
     * Checks that the index holds an entry for each span and for nothing else, in a read, and
     * makes it again from the spans when it does not: the derived index of a derived table.
     * Returns whether it matched.
     */
    pub(crate) fn check(&self) -> Result<bool, SearchError> {
        let matches: bool = self.db.read(|c| {
            Ok::<_, SearchError>(c.query_row(
                "SELECT NOT EXISTS (
                     SELECT 1 FROM span s LEFT JOIN span_text_docsize d ON d.id = s.id
                     WHERE d.id IS NULL)
                 AND NOT EXISTS (
                     SELECT 1 FROM span_text_docsize d LEFT JOIN span s ON s.id = d.id
                     WHERE s.id IS NULL)",
                [],
                |row| row.get(0),
            )?)
        })?;
        if !matches {
            log::warn!("the search index does not match its text; it is made again");
            let _writing = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
            self.db.write(|tx| {
                Ok::<_, SearchError>(
                    tx.execute("INSERT INTO span_text (span_text) VALUES ('rebuild')", [])
                        .map(drop)?,
                )
            })?;
        }
        Ok(matches)
    }
}

/**
 * Fails when another program holds the database at `path`: it is tried for a second, without
 * the busy wait of a connection to it that would otherwise go on far longer, and is said to
 * be in use, which is not damage.
 */
fn in_use(path: &Path) -> Result<(), SearchError> {
    let tried = Connection::open_with_flags(
        disk::for_sqlite(path, disk::SQLITE_FILE),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .and_then(|connection| {
        connection.busy_timeout(Duration::from_secs(1))?;
        connection.query_row("SELECT count(*) FROM sqlite_schema", [], |row| {
            row.get::<_, i64>(0)
        })
    });
    match tried {
        Err(rusqlite::Error::SqliteFailure(failure, _))
            if matches!(
                failure.code,
                rusqlite::ErrorCode::DatabaseBusy
                    | rusqlite::ErrorCode::DatabaseLocked
                    | rusqlite::ErrorCode::CannotOpen
                    | rusqlite::ErrorCode::PermissionDenied
                    | rusqlite::ErrorCode::SystemIoFailure
            ) =>
        {
            Err(SearchError::Unavailable(format!(
                "{} is in use by another program, or can't be read: {failure}",
                path.display()
            )))
        }
        /* Anything else is for the open to judge: a file that is no database is made anew. */
        _ => Ok(()),
    }
}

/**
 * Writes the database at `path` anew (`VACUUM`), on a connection of its own, once a write
 * under way has ended. Nothing a delete freed is in any page of it after, nor in a page a
 * delete rebalanced.
 */
pub(crate) fn vacuum(path: &Path) -> rusqlite::Result<()> {
    let connection = Connection::open(disk::for_sqlite(path, disk::SQLITE_FILE))?;
    connection.busy_timeout(Duration::from_secs(30))?;
    connection.execute_batch("VACUUM")
}

/// Takes out the spans of the doc `doc`, from the index and from `span`.
fn forget_spans(connection: &Connection, doc: i64) -> rusqlite::Result<()> {
    forget_range(connection, doc * SPANS, doc * SPANS + SPANS - 1)
}

/// Takes out the spans whose ids are from `first` to `last`, from the index and from `span`.
fn forget_range(connection: &Connection, first: i64, last: i64) -> rusqlite::Result<()> {
    /* Any statement on the index has it write out what it holds in memory first, so one with
    nothing to take out is not made. */
    let none: bool = connection
        .prepare_cached("SELECT NOT EXISTS (SELECT 1 FROM span WHERE id BETWEEN ?1 AND ?2)")?
        .query_row([first, last], |row| row.get(0))?;
    if none {
        return Ok(());
    }
    connection
        .prepare_cached(
            "INSERT INTO span_text (span_text, rowid, text)
             SELECT 'delete', id, text FROM span WHERE id BETWEEN ?1 AND ?2",
        )?
        .execute([first, last])?;
    connection
        .prepare_cached("DELETE FROM span WHERE id BETWEEN ?1 AND ?2")?
        .execute([first, last])?;
    Ok(())
}

/// The start of `text`, at most `bytes` long, cut where a character starts.
fn start(text: &str, bytes: usize) -> &str {
    if text.len() <= bytes {
        return text;
    }
    let mut end = bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/**
 * The write-ahead log of a database whose writes take things out that must leave no trace:
 * emptied after such a write ([`Log::empty`]), and again after the next write if a read held it
 * then ([`Log::retry`]).
 */
#[derive(Debug)]
pub(crate) struct Log {
    path: PathBuf,
    /// The last time it was to be emptied, a read held it.
    held: AtomicBool,
}

impl Log {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            held: AtomicBool::new(false),
        }
    }

    /**
     * Moves everything in the log into the database and empties the log, so what a write took
     * out of the database is in no file any more. A read under way holds the log: this tries
     * again for a moment (never waiting on a lock), and then leaves it to [`Log::retry`],
     * saying so in the log.
     */
    pub(crate) fn empty(&self) {
        let emptied = (|| {
            let connection = Connection::open(disk::for_sqlite(&self.path, disk::SQLITE_FILE))?;
            connection.busy_timeout(Duration::ZERO)?;
            for _ in 0..TRIES {
                let busy: i64 = connection
                    .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
                    .unwrap_or(1);
                if busy == 0 {
                    return Ok(true);
                }
                thread::sleep(APART);
            }
            Ok::<_, rusqlite::Error>(false)
        })();
        let emptied = emptied.unwrap_or_else(|error| {
            log::warn!("{}: {error}", self.path.display());
            false
        });
        if !emptied {
            log::warn!(
                "{}: a read held its log; it is emptied after the next write",
                self.path.display()
            );
        }
        self.held.store(!emptied, Ordering::Relaxed);
    }

    /// Empties the log if the last time it was to be, a read held it.
    pub(crate) fn retry(&self) {
        if self.held.load(Ordering::Relaxed) {
            self.empty();
        }
    }
}

/// How often, and how far apart, emptying a log that a read holds is tried: a second in all.
const TRIES: u32 = 10;
const APART: Duration = Duration::from_millis(100);
