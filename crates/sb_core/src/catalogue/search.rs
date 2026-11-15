//! The search text in the catalogue (PLAN §4.3, §10): a session's spans, kept with its row by
//! the index step, and the search over them ([`crate::search`] says what is asked and answered).
//!
//! `span` and `session_text` are derived. A session's spans are written with its row, in the
//! same transaction, and go with it however the row goes (a trigger of the schema), so they
//! are always of the row there is. A span's id is its session's row id times [`SPANS`] plus
//! its place in the session, so the matches of a query say their sessions without a lookup,
//! and a session's spans, read by id, come in the order of its conversation.
//!
//! **Ranking.** Sessions whose title matches come first, then the one that did something last.
//! A match's relevance (FTS5's `bm25`) is not used: over substrings it says little, and it
//! costs as much again as finding the matches of a common word. Within a session, matches are
//! in the order of the conversation, the title first.

use std::collections::HashMap;

use toolbox_store::rusqlite::types::{Type, Value};
use toolbox_store::rusqlite::{self, Connection, params, params_from_iter};

use super::library::on_shelf;
use super::session::{BY_ACTIVITY, SESSION, session_from};
use super::{Catalogue, CatalogueError, Result};
use crate::CancelToken;
use crate::search::{Found, Limit, Match, MatchKind, Query, Results, Snippet};
use crate::transcript::{At, BlockId, Span};

/// How many spans a session has room for: a span's id is its session's row id times this,
/// plus its place. A session with more keeps the first of them in the index.
const SPANS: i64 = 1 << 24;

/// How long a term must be, in characters, to be looked up in the index: `trigram` indexes
/// runs of three.
const INDEXED_CHARS: usize = 3;

/// How many sessions' spans a query that reads the text itself reads between two looks at
/// whether it is cancelled.
const READ_SESSIONS: i64 = 500;

/// How many matches a query goes through between two looks at whether it is cancelled.
const CHECK_EVERY: usize = 8192;

/// What marks a match in a snippet. Control characters, which a span's text does not hold
/// unless its source wrote them; then the snippet marks a little more or less.
const OPEN: char = '\u{1}';
const CLOSE: char = '\u{2}';

/// How many characters a snippet holds about its matches (FTS5 counts tokens; a `trigram`
/// token is a character).
const SNIPPET_CHARS: usize = 64;

/// Replaces the spans of the session whose row is `session` with its title and `spans`.
pub(super) fn record_spans(
    connection: &Connection,
    session: i64,
    title: Option<&str>,
    spans: &[Span],
) -> rusqlite::Result<()> {
    let first = session * SPANS;
    connection.execute(
        "INSERT INTO session_text (session_text, rowid, text)
         SELECT 'delete', id, text FROM span WHERE id BETWEEN ?1 AND ?2",
        [first, first + SPANS - 1],
    )?;
    connection.execute(
        "DELETE FROM span WHERE id BETWEEN ?1 AND ?2",
        [first, first + SPANS - 1],
    )?;
    let mut span = connection.prepare(
        "INSERT INTO span (id, kind, subagent, turn, block, unattached, text)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    let mut text = connection.prepare("INSERT INTO session_text (rowid, text) VALUES (?1, ?2)")?;
    let title = title.map(|title| (MatchKind::Title, false, None, title));
    let spans = spans
        .iter()
        .map(|span| (span.kind.into(), span.subagent, Some(span.at), &*span.text));
    for (id, (kind, subagent, at, body)) in
        (first..first + SPANS).zip(title.into_iter().chain(spans))
    {
        let (turn, block, unattached) = match at {
            None => (None, None, None),
            Some(At::Block { turn, block }) => (Some(turn), Some(block.0), None),
            Some(At::Unattached(place)) => (None, None, Some(place)),
        };
        span.execute(params![
            id,
            kind.as_str(),
            subagent,
            turn,
            block,
            unattached,
            body
        ])?;
        text.execute(params![id, body])?;
    }
    Ok(())
}

/// What the matches of a query in one session come to.
#[derive(Default)]
struct Hits {
    count: usize,
    title: bool,
    /// The first matching spans, by id.
    first: Vec<i64>,
}

impl Catalogue {
    /**
     * Runs `query` (PLAN §10): the sessions with a span that holds every one of its terms and
     * that its filters let through, the best first, each with its first matches (`limit`).
     * A query with no terms finds nothing. `cancel` stops it between two steps, with
     * [`CatalogueError::Cancelled`]. Whatever the terms hold (quotes, FTS5's operators, any
     * length), they are text to look for.
     */
    pub fn search(&self, query: &Query, limit: Limit, cancel: &CancelToken) -> Result<Results> {
        let terms: Vec<&str> = query
            .terms
            .iter()
            .map(|term| term.trim())
            .filter(|term| !term.is_empty())
            .collect();
        if terms.is_empty() {
            return Ok(Results::default());
        }
        let (long, short): (Vec<&str>, Vec<&str>) = terms
            .into_iter()
            .partition(|term| term.chars().count() >= INDEXED_CHARS);
        /* Each term as one FTS5 string, which is text whatever it holds. */
        let matched = (!long.is_empty()).then(|| {
            long.iter()
                .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" ")
        });
        let likes: Vec<String> = short
            .iter()
            .map(|term| {
                let escaped = term
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_");
                format!("%{escaped}%")
            })
            .collect();
        let like = vec!["text LIKE ? ESCAPE '\\'"; likes.len()].join(" AND ");
        let stop = || {
            if cancel.is_cancelled() {
                Err(CatalogueError::Cancelled)
            } else {
                Ok(())
            }
        };
        self.db.read(|c| {
            let mut hits: HashMap<i64, Hits> = HashMap::new();
            let mut hit = |id: i64| {
                let found = hits.entry(id / SPANS).or_default();
                found.count += 1;
                found.title |= id % SPANS == 0;
                if found.first.len() < limit.per_session {
                    found.first.push(id);
                }
            };
            match &matched {
                Some(matched) => {
                    let sql = if likes.is_empty() {
                        "SELECT rowid FROM session_text WHERE session_text MATCH ? ORDER BY rowid"
                            .to_string()
                    } else {
                        format!(
                            "SELECT rowid FROM session_text WHERE session_text MATCH ? AND {like}
                             ORDER BY rowid"
                        )
                    };
                    let mut select = c.prepare(&sql)?;
                    let mut rows =
                        select.query(params_from_iter(std::iter::once(matched).chain(&likes)))?;
                    let mut seen = 0;
                    while let Some(row) = rows.next()? {
                        hit(row.get(0)?);
                        seen += 1;
                        if seen % CHECK_EVERY == 0 {
                            stop()?;
                        }
                    }
                }
                /* No term the index holds: the text is read, some sessions at a time. */
                None => {
                    let last: i64 =
                        c.query_row("SELECT COALESCE(max(id), 0) FROM session", [], |row| {
                            row.get(0)
                        })?;
                    let mut select = c.prepare(&format!(
                        "SELECT id FROM span WHERE id BETWEEN ? AND ? AND {like} ORDER BY id"
                    ))?;
                    let mut from = 0;
                    while from <= last {
                        stop()?;
                        let bounds = [from * SPANS, (from + READ_SESSIONS) * SPANS - 1];
                        let mut rows = select.query(params_from_iter(
                            bounds
                                .iter()
                                .map(|bound| Value::Integer(*bound))
                                .chain(likes.iter().map(|like| Value::Text(like.clone()))),
                        ))?;
                        while let Some(row) = rows.next()? {
                            hit(row.get(0)?);
                        }
                        from += READ_SESSIONS;
                    }
                }
            }
            stop()?;
            found(c, query, limit, matched.as_deref(), hits)
        })
    }
}

/// The sessions of `hits` that `query`'s filters let through, ranked (the module docs), with
/// the first `limit` of them read whole.
fn found(
    c: &Connection,
    query: &Query,
    limit: Limit,
    matched: Option<&str>,
    mut hits: HashMap<i64, Hits>,
) -> Result<Results> {
    let filters = &query.filters;
    let (shelf, folder) = on_shelf(filters.shelf, ":folder");
    /* Every session the filters let through, ranked; there are as many as are indexed. */
    let mut select = c.prepare(&format!(
        "SELECT s.id FROM session s
             LEFT JOIN session_state st
               ON st.location_id = s.location_id AND st.provider = s.provider
              AND st.session = s.session
             WHERE {shelf}
               AND (:location IS NULL OR s.location_id = :location)
               AND (:agent IS NULL OR s.provider = :agent)
               AND (:model IS NULL OR EXISTS (
                   SELECT 1 FROM json_each(s.models) WHERE instr(lower(value), lower(:model))))
               AND (:branch IS NULL OR s.branch = :branch COLLATE NOCASE)
               AND (:after IS NULL OR COALESCE(s.ended_at, s.started_at) >= :after)
               AND (:before IS NULL OR COALESCE(s.started_at, s.ended_at) < :before)
             {BY_ACTIVITY}"
    ))?;
    let location = filters.location.as_ref();
    let agent = filters.agent.map(|agent| agent.as_str());
    let mut named: Vec<(&str, &dyn rusqlite::ToSql)> = vec![
        (":location", &location),
        (":agent", &agent),
        (":model", &filters.model),
        (":branch", &filters.branch),
        (":after", &filters.after),
        (":before", &filters.before),
    ];
    if let Some(folder) = &folder {
        named.push((":folder", folder));
    }
    let mut ranked: Vec<(i64, Hits)> = Vec::new();
    let mut rows = select.query(named.as_slice())?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        if let Some(found) = hits.remove(&id) {
            ranked.push((id, found));
        }
    }
    /* Those whose title matches first, each lot in the order of activity, as the sort is
    stable. */
    ranked.sort_by_key(|(_, found)| !found.title);
    let mut results = Results {
        sessions: Vec::new(),
        total_sessions: ranked.len(),
        total_matches: ranked.iter().map(|(_, found)| found.count).sum(),
    };
    let mut session = c.prepare(&format!("{SESSION} WHERE s.id = ?1"))?;
    let mut span =
        c.prepare("SELECT kind, subagent, turn, block, unattached, text FROM span WHERE id = ?1")?;
    let mut snippet = c.prepare(&format!(
        "SELECT snippet(session_text, 0, '{OPEN}', '{CLOSE}', '…', {SNIPPET_CHARS})
             FROM session_text WHERE session_text MATCH ?1 AND rowid = ?2"
    ))?;
    for (id, found) in ranked.into_iter().take(limit.sessions) {
        let mut shown = Vec::new();
        for span_id in found.first {
            let (kind, subagent, turn, block, unattached, text): (
                MatchKind,
                bool,
                Option<usize>,
                Option<usize>,
                Option<usize>,
                String,
            ) = span.query_row([span_id], |row| {
                let kind: String = row.get(0)?;
                let kind = MatchKind::from_str(&kind)
                    .ok_or_else(|| rusqlite::Error::InvalidColumnType(0, kind, Type::Text))?;
                Ok((
                    kind,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })?;
            let marked = match matched {
                Some(matched) => snippet.query_row(params![matched, span_id], |row| row.get(0))?,
                None => mark(&text, &query.terms),
            };
            shown.push(Match {
                kind,
                subagent,
                at: match (turn, block, unattached) {
                    (Some(turn), Some(block), _) => Some(At::Block {
                        turn,
                        block: BlockId(block),
                    }),
                    (_, _, Some(place)) => Some(At::Unattached(place)),
                    _ => None,
                },
                snippet: unmark(&marked),
            });
        }
        results.sessions.push(Found {
            session: session.query_row([id], session_from)?,
            matches: found.count,
            shown,
        });
    }
    Ok(results)
}

/// `text` with the first match of any of `terms`, as LIKE finds them (ASCII letters in any
/// case), marked, and as much about it as a snippet holds.
fn mark(text: &str, terms: &[String]) -> String {
    let lower = text.to_ascii_lowercase();
    let Some((at, length)) = terms
        .iter()
        .filter_map(|term| {
            lower
                .find(&term.to_ascii_lowercase())
                .map(|at| (at, term.len()))
        })
        .min()
    else {
        return text.chars().take(SNIPPET_CHARS).collect();
    };
    let before: String = {
        let chars: Vec<char> = text[..at].chars().rev().take(SNIPPET_CHARS / 2).collect();
        chars.into_iter().rev().collect()
    };
    let after: String = text[at + length..]
        .chars()
        .take(SNIPPET_CHARS / 2)
        .collect();
    let mut marked = String::new();
    if before.len() < at {
        marked.push('…');
    }
    marked.push_str(&before);
    marked.push(OPEN);
    marked.push_str(&text[at..at + length]);
    marked.push(CLOSE);
    marked.push_str(&after);
    if at + length + after.len() < text.len() {
        marked.push('…');
    }
    marked
}

/// A snippet from text whose matches are between [`OPEN`] and [`CLOSE`].
fn unmark(marked: &str) -> Snippet {
    let mut snippet = Snippet {
        text: String::with_capacity(marked.len()),
        hits: Vec::new(),
    };
    let mut open = None;
    for char in marked.chars() {
        match char {
            OPEN => open = Some(snippet.text.len()),
            CLOSE => {
                if let Some(start) = open.take() {
                    snippet.hits.push(start..snippet.text.len());
                }
            }
            char => snippet.text.push(char),
        }
    }
    snippet
}
