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
//! deleted and copies of cells a rebalance left in the unused middle of a page, goes when the
//! archive's background removal compacts the index and writes the file anew
//! ([`SearchIndex::compact`]).

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};
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
    steps: &[TEXT, MARKED_DELETED, MARKED],
};

/// Text taken out of the index is marked deleted, not taken out at once (the module docs).
const MARKED_DELETED: &str = "
INSERT INTO span_text (span_text, rank) VALUES ('secure-delete', 0);
";

/// How many spans' entries are marked deleted, until the index is compacted
/// ([`SearchIndex::compact`]). It adds up across indexes, and is counted in the transactions
/// that mark them, so a crash or a cancel loses none of it. What was marked before it was
/// counted is merged away as the index merges its parts.
const MARKED: &str = "
CREATE TABLE marked (spans INTEGER NOT NULL) STRICT;
INSERT INTO marked (spans) VALUES (0);
";

/// The share of the spans there are that marked ones have to come to for the index to be
/// compacted ([`SearchIndex::compaction_due`]): one in four.
const COMPACT_SHARE: i64 = 4;

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
pub(crate) struct SearchIndex {
    db: Database,
    log: Log,
    /// Held by whoever writes a session's text and checks first that the catalogue still has
    /// its row as it was made from, until the row says its text is searchable; by whoever says
    /// that of text already written; by whoever forgets a session; and by a compaction. So no
    /// text is written after the session it is of was deleted for good, and no row says it is
    /// searchable when its text is not there.
    writing: Mutex<()>,
    /// Held by the compaction under way ([`SearchIndex::compact`]), one at a time.
    compaction: Mutex<()>,
    /// A compaction runs: text that would wait for it may be left to the index's backlog
    /// instead ([`SearchIndex::record`]), so rows never wait on it.
    compacting: AtomicBool,
    /// What stops the compaction's statement under way, while one runs.
    interrupt: Mutex<Option<rusqlite::InterruptHandle>>,
}

/// What [`SearchIndex::record`] did with text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Recorded {
    /// It wrote so many sessions' text: text the index had already is not counted.
    Written(usize),
    /// The index is being compacted, and the text was left for the backlog, as asked.
    Deferred,
}

impl std::fmt::Debug for SearchIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchIndex")
            .field("db", &self.db)
            .finish_non_exhaustive()
    }
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
                compaction: Mutex::default(),
                compacting: AtomicBool::new(false),
                interrupt: Mutex::default(),
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
     * rows to say so.
     *
     * While the index is compacted, with `wait` it waits for the compaction to end; without,
     * none is written and none of the rows says so ([`Recorded::Deferred`]): what is being
     * indexed doesn't wait for it, and the text is the backlog's (PLAN §10).
     */
    pub(crate) fn record(
        &self,
        texts: &[Text],
        wait: bool,
        still: impl Fn(&Text) -> Result<bool, SearchError>,
        written: impl FnOnce(&[(SessionId, String)]) -> Result<(), SearchError>,
    ) -> Result<Recorded, SearchError> {
        let recorded = (|| {
            let writing = if wait {
                Some(self.writing.lock().unwrap_or_else(PoisonError::into_inner))
            } else {
                self.writing_unless_compacting()
            };
            let Some(_writing) = writing else {
                return Ok(Recorded::Deferred);
            };
            let (done, wrote) = self.write_texts(texts, still)?;
            written(&done)?;
            Ok(Recorded::Written(wrote))
        })();
        self.log.retry();
        recorded
    }

    /// [`SearchIndex::writing`], waited for while another write has it, and not while the
    /// index is compacted.
    fn writing_unless_compacting(&self) -> Option<MutexGuard<'_, ()>> {
        loop {
            match self.writing.try_lock() {
                Ok(held) => return Some(held),
                Err(TryLockError::Poisoned(held)) => return Some(held.into_inner()),
                Err(TryLockError::WouldBlock) if self.compacting.load(Ordering::Relaxed) => {
                    return None;
                }
                Err(TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(2)),
            }
        }
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
            let (mut wrote, mut marked) = (0, 0);
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
                    marked += forget_range(tx, first, first)?;
                }
                let places = i64::try_from(text.spans.len()).map_or(SPANS, |spans| spans + 1);
                marked += forget_range(tx, first + places.min(SPANS), first + SPANS - 1)?;
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
                            marked += 1;
                        }
                        None => {}
                    }
                    span.execute(row)?;
                    index.execute(params![id, kept])?;
                }
            }
            mark(tx, marked)?;
            let done = (kept.iter())
                .map(|text| (text.id.clone(), text.made.clone()))
                .collect();
            Ok((done, wrote))
        })
    }

    /// Takes out the text of `sessions`, and leaves none of it in the file or its log
    /// ([`Log::empty`]) but what [`SearchIndex::compact`] takes.
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
                /* A session given twice is one doc. */
                let mut docs = BTreeSet::new();
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
                    /* Every session's that has text: all of it at once, and nothing is left
                    marked. */
                    tx.execute_batch(
                        "INSERT INTO span_text (span_text) VALUES ('delete-all');
                         DELETE FROM span;
                         DELETE FROM doc;
                         UPDATE marked SET spans = 0;",
                    )?;
                } else {
                    let mut marked = 0;
                    for doc in &docs {
                        marked += forget_spans(tx, *doc)?;
                        tx.execute("DELETE FROM doc WHERE id = ?1", [doc])?;
                    }
                    mark(tx, marked)?;
                }
                Ok::<_, SearchError>(!docs.is_empty())
            })
        };
        /* A write that failed (the disk full, say) is emptied out of the log too, so its
        frames don't hold the room. */
        if !matches!(forgot, Ok(false)) {
            self.log.empty();
        }
        forgot.map(drop)
    }

    /// Whether the spans marked deleted have come to a share of those there are that the
    /// index is to be compacted for ([`COMPACT_SHARE`]).
    pub(crate) fn compaction_due(&self) -> Result<bool, SearchError> {
        self.db.read(|c| {
            let marked: i64 = c.query_row("SELECT spans FROM marked", [], |row| row.get(0))?;
            if marked == 0 {
                return Ok(false);
            }
            let spans: i64 = c.query_row("SELECT count(*) FROM span", [], |row| row.get(0))?;
            Ok(marked * COMPACT_SHARE >= spans)
        })
    }

    /**
     * How much room a compaction takes at most ([`SearchIndex::compact`]). On the archive's
     * disk, the index's entries three times over (a new copy of them, in the log and then in
     * the database, and the pages of the old one overwritten, as `secure_delete` does, in the
     * log); and with `vacuum`, the file once more, its pages in the log. With `vacuum`, the
     * file again in SQLite's temporary folder, where `VACUUM` writes its copy.
     */
    pub(crate) fn compaction_room(&self, vacuum: bool) -> Result<disk::Room, SearchError> {
        self.db.read(|c| {
            let entries: i64 = c.query_row(
                "SELECT coalesce(sum(length(block)), 0) FROM span_text_data",
                [],
                |row| row.get(0),
            )?;
            let file: i64 = c.query_row(
                "SELECT page_count * page_size FROM pragma_page_count, pragma_page_size",
                [],
                |row| row.get(0),
            )?;
            let (entries, file) = (as_bytes(entries), as_bytes(file));
            let file = if vacuum { file } else { 0 };
            Ok(disk::Room {
                archive: entries.saturating_mul(3).saturating_add(file),
                temp: file,
            })
        })
    }

    /**
     * Compacts the index: FTS5's `rebuild` makes it again from the spans there are, so it
     * holds nothing that was marked deleted; the pages it lets go of SQLite's `secure_delete`
     * overwrites. (`optimize`, which merges the parts instead, took about a second less on a
     * few thousand sessions and left nearly three times the entries for the same text.) With
     * `vacuum` (a delete for good's, PLAN §4.5), it then writes the file anew (`VACUUM`), so
     * no page keeps a copy of a cell a rebalance left in it, which `secure_delete` doesn't
     * reach; `VACUUM` makes its copy in SQLite's temporary folder, the system's
     * ([`disk::sqlite_temp_dir`]), so the index's live text is there while it runs. Then
     * empties the log, so what was taken out is in no file. Searches go on
     * meanwhile, of the index as it was; text being written is left to the backlog, or waits,
     * as its writer asks ([`SearchIndex::record`]); a forget waits. One compaction runs at a
     * time.
     *
     * The disk is to have the room ([`SearchIndex::compaction_room`]), else it is
     * [`SearchError::Unavailable`], saying what it needs. `stop` is asked every few tens of
     * milliseconds and ends it at once, with nothing changed ([`SearchError::Cancelled`]). A
     * log a read held is [`SearchError::Unavailable`] too: compacted, but not yet out of every
     * file.
     */
    pub(crate) fn compact(
        &self,
        vacuum: bool,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), SearchError> {
        let _one = self
            .compaction
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        /* A delete for good's is for nothing while a read holds the log, which its pages go
        through: it is left for later, before any of it is done. */
        if vacuum && !self.log.empty_now() {
            return Err(SearchError::Unavailable(
                "a read holds the search index's log".to_string(),
            ));
        }
        let dir = self.log.path.parent().unwrap_or(Path::new("."));
        if let Some(lack) = disk::lacking(dir, self.compaction_room(vacuum)?) {
            let disk = match lack.disk {
                disk::Disk::Archive => "the archive's disk",
                disk::Disk::System => "the system disk, where SQLite keeps its temporary files",
            };
            return Err(SearchError::Unavailable(format!(
                "compacting the search index needs about {} MB free on {disk}",
                lack.needs >> 20
            )));
        }
        self.compacting.store(true, Ordering::Relaxed);
        let compacted = self.watched(stop, || {
            let _writing = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
            self.db.write(|tx| {
                self.interruptible(tx, || {
                    tx.execute_batch(
                        "INSERT INTO span_text (span_text) VALUES ('rebuild');
                         UPDATE marked SET spans = 0;",
                    )
                })
            })?;
            if vacuum {
                /* Outside a transaction, so on a connection of its own; nothing else writes
                while `writing` is held. */
                let connection =
                    Connection::open(disk::for_sqlite(&self.log.path, disk::SQLITE_FILE))?;
                connection.busy_timeout(Duration::from_secs(5))?;
                self.interruptible(&connection, || connection.execute_batch("VACUUM"))?;
            }
            Ok(())
        });
        self.compacting.store(false, Ordering::Relaxed);
        if compacted.is_err() {
            /* What the failed write put in the log was rolled back, but still takes its room
            on the disk (all of it, when the disk filled): the log is emptied all the same. */
            self.log.empty();
        }
        compacted?;
        if !self.log.empty_now() {
            return Err(SearchError::Unavailable(
                "a read held the search index's log".to_string(),
            ));
        }
        Ok(())
    }

    /// Runs `work` while a thread asks `stop` every few tens of milliseconds and interrupts
    /// what [`SearchIndex::interruptible`] runs when it says to. With no thread to be had, it
    /// is asked once, before.
    fn watched(
        &self,
        stop: &(dyn Fn() -> bool + Sync),
        work: impl FnOnce() -> Result<(), SearchError>,
    ) -> Result<(), SearchError> {
        if stop() {
            return Err(SearchError::Cancelled);
        }
        let ended = AtomicBool::new(false);
        thread::scope(|scope| {
            let watching = thread::Builder::new()
                .name("search-compact".to_string())
                .spawn_scoped(scope, || {
                    while !ended.load(Ordering::Relaxed) {
                        if stop() {
                            let handle = self
                                .interrupt
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner);
                            if let Some(handle) = &*handle {
                                handle.interrupt();
                            }
                        }
                        thread::sleep(WATCH);
                    }
                });
            if let Err(error) = watching {
                log::warn!("a compaction can't be stopped once it begins: {error}");
            }
            let worked = work();
            ended.store(true, Ordering::Relaxed);
            worked
        })
    }

    /// Runs `statement` on `connection` so that a stop ([`SearchIndex::watched`]) interrupts
    /// it, which is [`SearchError::Cancelled`].
    fn interruptible(
        &self,
        connection: &Connection,
        statement: impl FnOnce() -> rusqlite::Result<()>,
    ) -> Result<(), SearchError> {
        *self
            .interrupt
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(connection.get_interrupt_handle());
        let ran = statement();
        *self
            .interrupt
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        ran.map_err(|error| match error.sqlite_error_code() {
            Some(rusqlite::ErrorCode::OperationInterrupted) => SearchError::Cancelled,
            _ => error.into(),
        })
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

/// How often a compaction asks whether it is to stop.
const WATCH: Duration = Duration::from_millis(25);

/// A size SQLite gave, as bytes: none when it is below zero.
fn as_bytes(size: i64) -> u64 {
    u64::try_from(size).unwrap_or(0)
}

/// Counts `spans` more as marked deleted in the index ([`MARKED_DELETED`]).
fn mark(connection: &Connection, spans: i64) -> rusqlite::Result<()> {
    if spans > 0 {
        connection
            .prepare_cached("UPDATE marked SET spans = spans + ?1")?
            .execute([spans])?;
    }
    Ok(())
}

/// Takes out the spans of the doc `doc`, from the index and from `span`. Returns how many.
fn forget_spans(connection: &Connection, doc: i64) -> rusqlite::Result<i64> {
    forget_range(connection, doc * SPANS, doc * SPANS + SPANS - 1)
}

/// Takes out the spans whose ids are from `first` to `last`, from the index and from `span`.
/// Returns how many.
fn forget_range(connection: &Connection, first: i64, last: i64) -> rusqlite::Result<i64> {
    /* Any statement on the index has it write out what it holds in memory first, so one with
    nothing to take out is not made. */
    let none: bool = connection
        .prepare_cached("SELECT NOT EXISTS (SELECT 1 FROM span WHERE id BETWEEN ?1 AND ?2)")?
        .query_row([first, last], |row| row.get(0))?;
    if none {
        return Ok(0);
    }
    connection
        .prepare_cached(
            "INSERT INTO span_text (span_text, rowid, text)
             SELECT 'delete', id, text FROM span WHERE id BETWEEN ?1 AND ?2",
        )?
        .execute([first, last])?;
    let spans = connection
        .prepare_cached("DELETE FROM span WHERE id BETWEEN ?1 AND ?2")?
        .execute([first, last])?;
    Ok(i64::try_from(spans).unwrap_or(i64::MAX))
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
     * saying so in the log. Returns whether it emptied it.
     */
    pub(crate) fn empty(&self) -> bool {
        self.empty_trying(TRIES)
    }

    /**
     * [`Log::empty`], tried for a fifth of a second: what work that can wait does (the
     * background removal's scrub), so a read that holds the log costs it little, and it is
     * tried again later; a checkpoint of the app's own, a moment long, is waited out.
     */
    pub(crate) fn empty_now(&self) -> bool {
        self.empty_trying(SOON)
    }

    fn empty_trying(&self, tries: u32) -> bool {
        let emptied = (|| {
            let connection = Connection::open(disk::for_sqlite(&self.path, disk::SQLITE_FILE))?;
            connection.busy_timeout(Duration::ZERO)?;
            for tried in 1..=tries {
                let busy: i64 = connection
                    .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
                    .unwrap_or(1);
                if busy == 0 {
                    return Ok(true);
                }
                if tried < tries {
                    thread::sleep(APART);
                }
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
        emptied
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
/// How often [`Log::empty_now`] tries, as far apart: a fifth of a second in all.
const SOON: u32 = 3;
const APART: Duration = Duration::from_millis(100);

#[cfg(test)]
mod tests {
    use super::*;

    fn text(key: &str) -> Text {
        Text {
            id: SessionId {
                location: crate::location::LocationId::new(),
                provider: "fixture".to_string(),
                key: key.to_string(),
            },
            made: "made".to_string(),
            title: Some(format!("The title of {key}")),
            spans: Vec::new(),
        }
    }

    /// Text written while the index is compacted is left, at once, for the backlog: nothing
    /// that writes it waits for the compaction.
    #[test]
    fn text_written_while_the_index_is_compacted_is_left_and_not_waited_on() {
        let dir = tempfile::tempdir().unwrap();
        let (search, _) = SearchIndex::open(dir.path()).unwrap();
        let held = search.writing.lock().unwrap();
        search.compacting.store(true, Ordering::Relaxed);
        let said = Mutex::new(Vec::new());
        let (first, again) = (text("a"), text("b"));
        let written = search.record(
            &[first],
            false,
            |_| Ok(true),
            |done| {
                said.lock().unwrap().extend_from_slice(done);
                Ok(())
            },
        );
        assert_eq!(written.unwrap(), Recorded::Deferred);
        assert!(said.lock().unwrap().is_empty());
        drop(held);
        search.compacting.store(false, Ordering::Relaxed);
        assert_eq!(
            search
                .record(&[again], false, |_| Ok(true), |_| Ok(()))
                .unwrap(),
            Recorded::Written(1)
        );
    }

    /// A compaction that is to stop ends with nothing changed: what is marked stays counted.
    /// This stops it at the check before it begins; the interrupt of a statement under way is
    /// not reached here, as when it fires depends on how long the statement takes.
    #[test]
    fn a_compaction_stopped_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (search, _) = SearchIndex::open(dir.path()).unwrap();
        let first = text("a");
        let again = Text {
            id: first.id.clone(),
            made: "made again".to_string(),
            title: Some("Another title".to_string()),
            spans: Vec::new(),
        };
        search
            .record(&[first], false, |_| Ok(true), |_| Ok(()))
            .unwrap();
        search
            .record(&[again], false, |_| Ok(true), |_| Ok(()))
            .unwrap();
        let marked = || -> i64 {
            search
                .read(|c| Ok(c.query_row("SELECT spans FROM marked", [], |row| row.get(0))?))
                .unwrap()
        };
        assert_eq!(marked(), 1);
        assert!(matches!(
            search.compact(false, &|| true),
            Err(SearchError::Cancelled)
        ));
        assert_eq!(marked(), 1);
        search.compact(false, &|| false).unwrap();
        assert_eq!(marked(), 0);
    }
}
