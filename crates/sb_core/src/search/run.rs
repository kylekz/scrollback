//! Running a query (the module docs of [`super`]): the terms are found in `search.db`, the
//! sessions they are in are let through by the filters in the catalogue, and ranked.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use toolbox_store::rusqlite::{self, Connection, OptionalExtension as _, Row, params};

use super::store::SPANS;
use super::text::{folded, holds, snippet};
use super::{Found, Limit, Match, MatchKind, Query, Results, SearchError, Searched};
use crate::CancelToken;
use crate::archive::Archive;
use crate::catalogue::SessionId;
use crate::transcript::{At, BlockId};

/// The most terms a query is run with; the rest of a longer query is left out.
pub const MOST_TERMS: usize = 16;

/// The most characters of a term that are looked for; the rest are left out.
pub const TERM_CHARS: usize = 256;

/// How many characters a term needs for the index to hold it: `trigram` indexes runs of three.
const INDEXED_CHARS: usize = 3;

/// How many spans a query reads between two looks at whether it is cancelled.
const CHECK_EVERY: usize = 4096;

/// How many sessions' spans a query that reads the text itself reads at a time.
const READ_DOCS: i64 = 200;

/// A session's spans that hold a term, each with the terms it holds, one bit each.
type Spans = BTreeMap<i64, u32>;

impl Archive {
    /// How many sessions' text is not searchable yet, or is of what they were before: what
    /// the palette's "Still indexing" line counts (DESIGN §4.9). A session the archive could
    /// not be read for when its text was to be made is counted until an index of a start
    /// reads it.
    pub fn unsearchable(&self) -> Result<usize, SearchError> {
        let docs = self.search_index()?.docs()?;
        Ok(self
            .catalogue()
            .made(None, None)?
            .into_iter()
            .filter(|(id, made)| docs.get(id) != Some(made))
            .count())
    }

    /**
     * Runs `query` (PLAN §10, the module docs of [`super`]): the sessions that hold every one
     * of its terms and that its filters let through, the best first, each with its first
     * matches (`limit`). A query with no terms finds nothing. `cancel` stops it between two
     * steps, with [`SearchError::Cancelled`]. Whatever the terms hold (quotes, FTS5's
     * operators, NULs, any length), they are text to look for; only the first [`MOST_TERMS`]
     * different terms, and the first [`TERM_CHARS`] characters of each, are.
     */
    pub fn search(
        &self,
        query: &Query,
        limit: Limit,
        cancel: &CancelToken,
    ) -> Result<Searched, SearchError> {
        let mut terms: Vec<String> = Vec::new();
        for term in &query.terms {
            let term: String = folded(term.trim())
                .chars()
                .filter(|char| *char != '\0')
                .take(TERM_CHARS)
                .collect();
            if !term.is_empty() && !terms.contains(&term) && terms.len() < MOST_TERMS {
                terms.push(term);
            }
        }
        if terms.is_empty() {
            return Ok(Searched::Results(Results::default()));
        }
        let indexed = |term: &String| term.chars().count() >= INDEXED_CHARS;
        if !terms.iter().any(indexed) && terms.iter().all(|term| term.is_ascii()) {
            return Ok(Searched::KeepTyping);
        }
        /* Those the index holds first: the others then only narrow what they found. */
        terms.sort_by_key(|term| !indexed(term));
        let stop = || {
            if cancel.is_cancelled() {
                Err(SearchError::Cancelled)
            } else {
                Ok(())
            }
        };
        let every = (1u32 << terms.len()) - 1;
        let search = self.search_index()?;
        let (docs, sessions) = search.read(|c| {
            let mut docs: Option<HashMap<i64, Spans>> = None;
            for (at, term) in terms.iter().enumerate() {
                let found = if indexed(term) {
                    looked_up(c, term, docs.as_ref(), &stop)?
                } else {
                    read_for(c, term, docs.as_ref(), &stop)?
                };
                let bit = 1 << at;
                docs = Some(match docs {
                    None => found
                        .into_iter()
                        .map(|(doc, spans)| (doc, spans.into_iter().map(|id| (id, bit)).collect()))
                        .collect(),
                    Some(mut docs) => {
                        docs.retain(|doc, _| found.contains_key(doc));
                        for (doc, spans) in found {
                            if let Some(held) = docs.get_mut(&doc) {
                                for id in spans {
                                    *held.entry(id).or_default() |= bit;
                                }
                            }
                        }
                        docs
                    }
                });
            }
            let docs = docs.unwrap_or_default();
            /* The sessions the docs found are, in one read. */
            let mut sessions = HashMap::new();
            let mut select = c.prepare("SELECT id, location_id, provider, session FROM doc")?;
            let mut rows = select.query([])?;
            let mut read = 0;
            while let Some(row) = rows.next()? {
                read += 1;
                if read % CHECK_EVERY == 0 {
                    stop()?;
                }
                let doc: i64 = row.get(0)?;
                if docs.contains_key(&doc) {
                    let id = SessionId {
                        location: row.get(1)?,
                        provider: row.get(2)?,
                        key: row.get(3)?,
                    };
                    sessions.insert(id, doc);
                }
            }
            Ok((docs, sessions))
        })?;
        stop()?;

        let catalogue = self.catalogue();
        let mut ranked: Vec<(SessionId, i64, bool)> = catalogue
            .filtered(&query.filters)?
            .into_iter()
            .filter_map(|id| {
                let doc = *sessions.get(&id)?;
                let title = docs[&doc].get(&(doc * SPANS)) == Some(&every);
                Some((id, doc, title))
            })
            .collect();
        /* Those whose title holds every term first, each lot in the order of activity, as the
        sort is stable. */
        ranked.sort_by_key(|(_, _, title)| !title);
        let mut results = Results {
            sessions: Vec::new(),
            total_sessions: ranked.len(),
            total_matches: ranked.iter().map(|(_, doc, _)| docs[doc].len()).sum(),
        };
        for (id, doc, _) in ranked.into_iter().take(limit.sessions) {
            stop()?;
            let Some(session) = catalogue.session(&id)? else {
                continue;
            };
            let spans = &docs[&doc];
            let first: Vec<i64> = spans
                .iter()
                .filter(|(_, held)| **held == every)
                .chain(spans.iter().filter(|(_, held)| **held != every))
                .map(|(id, _)| *id)
                .take(limit.per_session)
                .collect();
            let shown = search.read(|c| {
                /* Of the session's doc still: a doc's id goes to another session's only once
                this one's is forgotten, between that read and this. */
                let mut span = c.prepare(
                    "SELECT s.kind, s.subagent, s.turn, s.block, s.unattached, s.cut, s.text
                     FROM span s JOIN doc d ON d.id = ?2
                     WHERE s.id = ?1 AND d.location_id = ?3 AND d.provider = ?4 AND d.session = ?5",
                )?;
                let mut shown = Vec::new();
                for span_id in first {
                    stop()?;
                    let found = span
                        .query_row(
                            params![span_id, doc, id.location, id.provider, id.key],
                            |row| {
                                let kind: String = row.get(0)?;
                                let at = match (row.get(2)?, row.get(3)?, row.get(4)?) {
                                    (Some(turn), Some(block), _) => Some(At::Block {
                                        turn,
                                        block: BlockId(block),
                                    }),
                                    (_, _, Some(place)) => Some(At::Unattached(place)),
                                    _ => None,
                                };
                                let text: String = row.get(6)?;
                                Ok(match MatchKind::from_str(&kind) {
                                    None => None,
                                    Some(kind) => Some(Match {
                                        kind,
                                        subagent: row.get(1)?,
                                        at,
                                        cut: row.get(5)?,
                                        snippet: snippet(&text, &terms),
                                    }),
                                })
                            },
                        )
                        .optional()?
                        .flatten()
                        .filter(|found| !found.snippet.hits.is_empty());
                    shown.extend(found);
                }
                Ok(shown)
            })?;
            results.sessions.push(Found {
                session,
                matches: spans.len(),
                shown,
            });
        }
        Ok(Searched::Results(results))
    }
}

/**
 * The spans that hold `term`, which the index holds, by session: of `among` only, when given.
 * The index says which spans have every run of three characters of the term (each one FTS5
 * string: text, whatever it holds). For a term of three characters that is the answer, as the
 * index folds text exactly as the term was ([`super::text`]); for a longer one, the text of
 * each of those spans says whether it holds the term.
 */
fn looked_up(
    c: &Connection,
    term: &str,
    among: Option<&HashMap<i64, Spans>>,
    stop: &dyn Fn() -> Result<(), SearchError>,
) -> Result<HashMap<i64, Vec<i64>>, SearchError> {
    let chars: Vec<char> = term.chars().collect();
    let runs: BTreeSet<String> = chars
        .windows(INDEXED_CHARS)
        .map(String::from_iter)
        .collect();
    let matched = runs
        .iter()
        .map(|run| format!("\"{}\"", run.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ");
    let mut select = c.prepare("SELECT rowid FROM span_text WHERE span_text MATCH ?1")?;
    let mut rows = select.query([matched])?;
    let mut candidates = Vec::new();
    while let Some(row) = rows.next()? {
        if candidates.len() % CHECK_EVERY == 0 {
            stop()?;
        }
        let id: i64 = row.get(0)?;
        if among.is_none_or(|among| among.contains_key(&(id / SPANS))) {
            candidates.push(id);
        }
    }
    let mut found: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut text = c.prepare("SELECT text FROM span WHERE id = ?1")?;
    for (read, id) in candidates.into_iter().enumerate() {
        if chars.len() > INDEXED_CHARS {
            if read % CHECK_EVERY == 0 {
                stop()?;
            }
            let held = text.query_row([id], |row| Ok(holds(row.get_ref(0)?.as_str()?, term)))?;
            if !held {
                continue;
            }
        }
        found.entry(id / SPANS).or_default().push(id);
    }
    Ok(found)
}

/// The spans that hold `term`, which the index does not hold, by session, read from their
/// text: of the sessions `among`, or of every session.
fn read_for(
    c: &Connection,
    term: &str,
    among: Option<&HashMap<i64, Spans>>,
    stop: &dyn Fn() -> Result<(), SearchError>,
) -> Result<HashMap<i64, Vec<i64>>, SearchError> {
    let mut select = c.prepare("SELECT id, text FROM span WHERE id BETWEEN ?1 AND ?2")?;
    let mut found: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut read = |first: i64, last: i64| {
        stop()?;
        let mut rows = select.query(params![first * SPANS, (last + 1) * SPANS - 1])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            if holds(text(row)?, term) {
                found.entry(id / SPANS).or_default().push(id);
            }
        }
        Ok::<_, SearchError>(())
    };
    match among {
        Some(among) => {
            for doc in among.keys() {
                read(*doc, *doc)?;
            }
        }
        None => {
            let last: i64 =
                c.query_row("SELECT COALESCE(max(id), 0) FROM doc", [], |row| row.get(0))?;
            let mut first = 0;
            while first <= last {
                read(first, first + READ_DOCS - 1)?;
                first += READ_DOCS;
            }
        }
    }
    Ok(found)
}

/// The text of a span, the second column of `row`.
fn text<'r>(row: &'r Row<'_>) -> Result<&'r str, SearchError> {
    Ok(row.get_ref(1)?.as_str().map_err(rusqlite::Error::from)?)
}
