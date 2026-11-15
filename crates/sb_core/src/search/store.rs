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
//! Deleting is meant to leave nothing: FTS5's `secure-delete` takes a span's entries out of the
//! index instead of marking them deleted, SQLite's `secure_delete` overwrites what is freed, and
//! the write-ahead log is emptied after a session is forgotten ([`SearchIndex::forget`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use toolbox_store::rusqlite::{self, Connection, OptionalExtension as _, params};
use toolbox_store::{Database, OpenOptions, Schema};

use super::{MatchKind, SearchError};
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
/// file again from the archive.
const SCHEMA: Schema = Schema {
    domain: "scrollback-search",
    steps: &[TEXT],
};

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

-- kind is title, prompt, reply, command or path; subagent: the text is a subagent's. A match
-- opens the session at turn and block (for a subagent's text, the call it ran under), or at
-- the subagent unattached that no call is known to have started; a title, at its start. cut:
-- only the start of the span's text is here.
CREATE TABLE span (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    subagent INTEGER NOT NULL,
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
    pub spans: Vec<Span>,
}

impl Text {
    /// About how many bytes the text holds.
    pub(crate) fn bytes(&self) -> usize {
        self.spans.iter().map(|span| span.text.len()).sum()
    }
}

/// The open `search.db`.
#[derive(Debug)]
pub(crate) struct SearchIndex {
    db: Database,
    log: Log,
    /// Held by whoever writes a session's text and checks first that the catalogue still has
    /// its row as it was made from, and by whoever forgets a session: so no text is written
    /// after the session it is of was deleted for good.
    writing: Mutex<()>,
}

impl SearchIndex {
    /// Opens `search.db` in the archive folder `dir`, making it anew when it is missing, or is
    /// not a file this build can read: the archive has everything it holds.
    pub(crate) fn open(dir: &Path) -> Result<Self, SearchError> {
        let path = dir.join(SEARCH_DATABASE);
        let options = OpenOptions::new(&SCHEMA);
        let db = match Database::open(&path, &options) {
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
                Database::open(&path, &options)?
            }
            opened => opened?,
        };
        /* What a write frees is overwritten, on the one connection that writes. */
        db.write(|tx| Ok::<_, SearchError>(tx.pragma_update(None, "secure_delete", true)?))?;
        Ok(Self {
            db,
            log: Log::new(path),
            writing: Mutex::default(),
        })
    }

    /// Empties the database's write-ahead log ([`Log::empty`]).
    pub(crate) fn empty_log(&self) {
        self.log.empty();
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
     * the text was made of. Returns how many were written.
     */
    pub(crate) fn record(
        &self,
        texts: &[Text],
        still: impl Fn(&Text) -> Result<bool, SearchError>,
    ) -> Result<usize, SearchError> {
        let written = self.write_texts(texts, still);
        self.log.retry();
        written
    }

    fn write_texts(
        &self,
        texts: &[Text],
        still: impl Fn(&Text) -> Result<bool, SearchError>,
    ) -> Result<usize, SearchError> {
        let _writing = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        let mut kept = Vec::new();
        for text in texts {
            if still(text)? {
                kept.push(text);
            }
        }
        if kept.is_empty() {
            return Ok(0);
        }
        self.db.write(|tx| {
            let mut doc = tx.prepare(
                "INSERT INTO doc (location_id, provider, session, made) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (location_id, provider, session) DO UPDATE SET made = excluded.made
                 RETURNING id",
            )?;
            let mut span = tx.prepare(
                "INSERT INTO span (id, kind, subagent, turn, block, unattached, cut, text)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            let mut index = tx.prepare("INSERT INTO span_text (rowid, text) VALUES (?1, ?2)")?;
            for text in &kept {
                let id = &text.id;
                let doc: i64 = doc.query_row(
                    params![id.location, id.provider, id.key, text.made],
                    |row| row.get(0),
                )?;
                forget_spans(tx, doc)?;
                let title = text
                    .title
                    .as_deref()
                    .map(|title| (0, MatchKind::Title, false, None, title));
                let spans = (1..SPANS).zip(&text.spans).map(|(place, span)| {
                    (
                        place,
                        span.kind.into(),
                        span.subagent,
                        Some(span.at),
                        &*span.text,
                    )
                });
                for (place, kind, subagent, at, body) in title.into_iter().chain(spans) {
                    let (turn, block, unattached) = match at {
                        None => (None, None, None),
                        Some(At::Block { turn, block }) => (Some(turn), Some(block.0), None),
                        Some(At::Unattached(place)) => (None, None, Some(place)),
                    };
                    let kept = start(body, SPAN_BYTES);
                    let id = doc * SPANS + place;
                    span.execute(params![
                        id,
                        kind.as_str(),
                        subagent,
                        turn,
                        block,
                        unattached,
                        kept.len() < body.len(),
                        kept
                    ])?;
                    index.execute(params![id, kept])?;
                }
            }
            Ok(kept.len())
        })
    }

    /// Takes out the text of `sessions`, and leaves none of it in the file or its log
    /// ([`checkpoint`]).
    pub(crate) fn forget(&self, sessions: &[SessionId]) -> Result<(), SearchError> {
        if sessions.is_empty() {
            return Ok(());
        }
        {
            let _writing = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
            self.db.write(|tx| {
                let mut find = tx.prepare(
                    "SELECT id FROM doc WHERE location_id = ?1 AND provider = ?2 AND session = ?3",
                )?;
                for id in sessions {
                    let doc: Option<i64> = find
                        .query_row(params![id.location, id.provider, id.key], |row| row.get(0))
                        .optional()?;
                    if let Some(doc) = doc {
                        forget_spans(tx, doc)?;
                        tx.execute("DELETE FROM doc WHERE id = ?1", [doc])?;
                    }
                }
                Ok::<_, SearchError>(())
            })?;
        }
        self.log.empty();
        Ok(())
    }

    /// Takes out every session's text.
    pub(crate) fn clear(&self) -> Result<(), SearchError> {
        {
            let _writing = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
            self.db.write(|tx| {
                tx.execute_batch(
                    "INSERT INTO span_text (span_text) VALUES ('delete-all');
                     DELETE FROM span;
                     DELETE FROM doc;",
                )?;
                Ok::<_, SearchError>(())
            })?;
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

/// Takes out the spans of the doc `doc`, from the index and from `span`.
fn forget_spans(connection: &Connection, doc: i64) -> rusqlite::Result<()> {
    let range = [doc * SPANS, doc * SPANS + SPANS - 1];
    connection.execute(
        "INSERT INTO span_text (span_text, rowid, text)
         SELECT 'delete', id, text FROM span WHERE id BETWEEN ?1 AND ?2",
        range,
    )?;
    connection.execute("DELETE FROM span WHERE id BETWEEN ?1 AND ?2", range)?;
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
            let connection = Connection::open(&self.path)?;
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
