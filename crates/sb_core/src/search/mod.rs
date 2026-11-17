//! Search (PLAN §10, DESIGN §4.9): what the palette asks and what it is answered with. The
//! query is free text and filters, typed as `key:value` tokens and parsed here, so the syntax is
//! the same wherever it is typed; [`crate::archive::Archive::search`] runs it over the text the
//! index step keeps of every session, in `search.db` ([`store`]).
//!
//! **Matching.** A term is matched as a substring, without regard to case or to the accents of
//! Latin letters ([`text`]). A session is found when every term is somewhere in it: its title,
//! prompts, replies, tool calls' commands and paths, its subagents' included. A term of three
//! characters or more is looked up in the index; a shorter one beside it only narrows what it
//! found. A query of short terms alone is looked for by reading the text, which is slower and
//! is cancelled with the query, unless they are all ASCII (`ok`, `-p`): that is
//! [`Searched::KeepTyping`], since a letter or two of English matches nearly every session,
//! where two characters of Chinese or Japanese are often a whole word.
//!
//! **Ranking.** Sessions whose title holds every term come first, then the one that did
//! something last. A session's matches are its spans that hold a term, those that hold every
//! term first, each lot in the order of the conversation.

mod fold;
mod run;
mod store;
mod text;

use std::ops::Range;

use chrono::{Local, NaiveDate, TimeZone as _, Utc};
use thiserror::Error;
use toolbox_store::rusqlite;

use crate::catalogue::{CatalogueError, Folder, Session, Shelf};
use crate::location::{Location, LocationId};
use crate::provider::ProviderId;
use crate::transcript::{At, InSubagent, SpanKind};

pub use run::{MOST_TERMS, TERM_CHARS};
pub(crate) use store::{Kept, Log, SEARCH_DATABASE, SearchIndex, Text, kept};
pub use text::{find_all, first_match};

#[derive(Debug, Error)]
pub enum SearchError {
    #[error(transparent)]
    Database(#[from] toolbox_store::Error),
    #[error(transparent)]
    Catalogue(#[from] CatalogueError),
    #[error("the search was cancelled")]
    Cancelled,
    /// The search index could not be opened, for the reason given; the archive can, and the
    /// next index tries again.
    #[error("search is not available: {0}")]
    Unavailable(String),
}

impl From<rusqlite::Error> for SearchError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error.into())
    }
}

/// What a matched span is (DESIGN §4.9's kind column).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MatchKind {
    Title,
    Prompt,
    Reply,
    /// What a tool call was asked to do: a command, a search pattern or query, a task.
    Command,
    /// What a tool call was asked to work on: a path or a web address.
    Path,
}

impl MatchKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Prompt => "prompt",
            Self::Reply => "reply",
            Self::Command => "command",
            Self::Path => "path",
        }
    }

    pub(crate) fn from_str(text: &str) -> Option<Self> {
        [
            Self::Title,
            Self::Prompt,
            Self::Reply,
            Self::Command,
            Self::Path,
        ]
        .into_iter()
        .find(|kind| kind.as_str() == text)
    }
}

impl From<SpanKind> for MatchKind {
    fn from(kind: SpanKind) -> Self {
        match kind {
            SpanKind::Prompt => Self::Prompt,
            SpanKind::Reply => Self::Reply,
            SpanKind::Command => Self::Command,
            SpanKind::Path => Self::Path,
        }
    }
}

/// A query as the palette runs it: free text, every term of which a span must hold, and the
/// filters on the sessions it may be in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    pub terms: Vec<String>,
    pub filters: Filters,
}

/// What narrows a query to some sessions (PLAN §10). Times are Unix milliseconds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filters {
    /// The sessions of one page of the library: [`Shelf::All`] is every session not in Trash,
    /// and [`Shelf::Trash`] only those in it.
    pub shelf: Shelf,
    pub location: Option<LocationId>,
    /// The provider of the session's agent.
    pub agent: Option<ProviderId>,
    /// Part of the name of one of the session's models, in any case.
    pub model: Option<String>,
    /// The session's branch, in any case.
    pub branch: Option<String>,
    /// The session did something at or after this time, and before `before`.
    pub after: Option<i64>,
    pub before: Option<i64>,
}

impl Filters {
    /// Sets one filter, in place of any of the same kind.
    pub fn set(&mut self, filter: Filter) {
        match filter {
            Filter::Shelf(shelf) => self.shelf = shelf,
            Filter::Location(location) => self.location = Some(location),
            Filter::Agent(agent) => self.agent = Some(agent),
            Filter::Model(model) => self.model = Some(model),
            Filter::Branch(branch) => self.branch = Some(branch),
            Filter::After(after) => self.after = Some(after),
            Filter::Before(before) => self.before = Some(before),
        }
    }
}

/// One filter, as a token resolves to it: a chip of the palette.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Filter {
    Shelf(Shelf),
    Location(LocationId),
    Agent(ProviderId),
    Model(String),
    Branch(String),
    After(i64),
    Before(i64),
}

/// The keys of the filter tokens (DESIGN §4.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// `in:` a page of the library (`all`, `starred`, `unsorted`, `trash`), or else a folder.
    In,
    /// `folder:` a folder, whatever its name.
    Folder,
    /// `from:` a location, by its name.
    From,
    /// `agent:` a provider's id.
    Agent,
    /// `model:` part of a model's name.
    Model,
    Branch,
    /// `after:` and `before:` a day (`2026-03-05`), a month (`2026-03`) or a year (`2026`),
    /// in local time: `after:` from its start, `before:` up to its start.
    After,
    Before,
}

impl Key {
    const ALL: [(Self, &'static str); 8] = [
        (Self::In, "in"),
        (Self::Folder, "folder"),
        (Self::From, "from"),
        (Self::Agent, "agent"),
        (Self::Model, "model"),
        (Self::Branch, "branch"),
        (Self::After, "after"),
        (Self::Before, "before"),
    ];

    pub fn as_str(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(key, _)| *key == self)
            .map_or("", |(_, name)| name)
    }
}

/// A `key:value` token of a query, as typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub key: Key,
    pub value: String,
}

/// What a query's text says: its free text's terms and its filter tokens, each in the order
/// typed, and the keys typed with no value yet (`in:`), which are no filter and no text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    pub terms: Vec<String>,
    pub tokens: Vec<Token>,
    pub incomplete: Vec<Key>,
}

/**
 * Splits what was typed into terms and filter tokens. Terms are separated by white space; text
 * in double quotes is one term, spaces and all, and a quote with no partner runs to the end.
 * Inside quotes `\"` is a quote of the text (`branch:"say \"hi\""`). A
 * word `key:value` whose key is one of [`Key`]'s, in any case, is a token, and with no value
 * an incomplete one; any other word with a colon (`C:\Users`, `https://…`) is a term. A NUL is
 * no character of any term. Nothing typed is an error: whatever it holds, it is text to look
 * for.
 */
pub fn parse(input: &str) -> Parsed {
    let mut parsed = Parsed::default();
    let input = input.replace('\0', "");
    let mut rest = input.as_str();
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return parsed;
        }
        let (word, after) = word(rest);
        rest = after;
        let token = word.split_once(':').and_then(|(key, value)| {
            let (key, _) = Key::ALL
                .iter()
                .find(|(_, name)| name.eq_ignore_ascii_case(key))?;
            Some((*key, unquote(value)))
        });
        match token {
            Some((key, value)) if value.is_empty() => parsed.incomplete.push(key),
            Some((key, value)) => parsed.tokens.push(Token { key, value }),
            None => {
                let term = unquote(word);
                if !term.trim().is_empty() {
                    parsed.terms.push(term);
                }
            }
        }
    }
}

/// The first word of `text`, which starts with no white space, and what follows it. White
/// space inside double quotes is part of the word, read by [`quoted`]'s rules.
pub fn word(text: &str) -> (&str, &str) {
    let mut at = 0;
    while let Some(char) = text[at..].chars().next() {
        match char {
            '"' => match quoted(&text[at + 1..]).1 {
                Some(close) => at += 1 + close + 1,
                None => return (text, ""),
            },
            char if char.is_whitespace() => return (&text[..at], &text[at..]),
            char => at += char.len_utf8(),
        }
    }
    (text, "")
}

/**
 * What a quote opened before `inside` holds, and where in `inside` its closing quote is, if
 * one closes it. Inside quotes `\"` is a quote, and `\\"` a backslash that ends the quotes, so
 * a quoted path can end in one (`"D:\app\\"` is `D:\app\`); every other backslash is itself,
 * so a network path keeps its two (`"\\host\share"`).
 */
fn quoted(inside: &str) -> (String, Option<usize>) {
    let mut out = String::with_capacity(inside.len());
    let mut chars = inside.char_indices();
    while let Some((at, char)) = chars.next() {
        match char {
            '"' => return (out, Some(at)),
            '\\' if inside[at + 1..].starts_with("\\\"") => {
                out.push('\\');
                chars.next();
            }
            '\\' if inside[at + 1..].starts_with('"') => {
                out.push('"');
                chars.next();
            }
            char => out.push(char),
        }
    }
    (out, None)
}

/**
 * `text` without the quotes around it, or after the colon of a token: `"a b"` is `a b`, and
 * so is `"a b` that a quote opened and nothing closed, read by [`quoted`]'s rules. What
 * follows the closing quote is taken as it is.
 */
pub fn unquote(text: &str) -> String {
    let Some(inside) = text.strip_prefix('"') else {
        return text.to_string();
    };
    match quoted(inside) {
        (mut out, Some(close)) => {
            out.push_str(&inside[close + 1..]);
            out
        }
        (out, None) => out,
    }
}

/// Which calendar a date in a token is read in: the one the app shows dates in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Calendar {
    /// This machine's time zone.
    #[default]
    Local,
    Utc,
}

/// What a token's value can name, and how its dates are read.
pub struct Names<'a> {
    pub folders: &'a [Folder],
    pub locations: &'a [Location],
    pub agents: &'a [ProviderId],
    pub calendar: Calendar,
}

impl Token {
    /**
     * The filter the token names, if it names one. `in:` takes a page's name first, then a
     * folder's, and `folder:` a folder's: the first folder of that name, in the order
     * [`crate::catalogue::Catalogue::folders`] gives them, so a folder named like a page is
     * reached by `folder:`. `from:` takes a location's name, `agent:` a provider's id; names
     * are matched in any case. `model:` and `branch:` take any value. A date that is not one
     * is no filter.
     */
    pub fn resolve(&self, names: &Names<'_>) -> Option<Filter> {
        let value = self.value.as_str();
        let folder = || {
            names
                .folders
                .iter()
                .find(|folder| folder.name.eq_ignore_ascii_case(value))
                .map(|folder| Filter::Shelf(Shelf::Folder(folder.id)))
        };
        match self.key {
            Key::In => [
                ("all", Shelf::All),
                ("starred", Shelf::Starred),
                ("unsorted", Shelf::Unsorted),
                ("trash", Shelf::Trash),
            ]
            .into_iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(value))
            .map(|(_, shelf)| Filter::Shelf(shelf))
            .or_else(folder),
            Key::Folder => folder(),
            Key::From => names
                .locations
                .iter()
                .find(|location| location.name.eq_ignore_ascii_case(value))
                .map(|location| Filter::Location(location.id.clone())),
            Key::Agent => names
                .agents
                .iter()
                .find(|agent| agent.as_str().eq_ignore_ascii_case(value))
                .map(|agent| Filter::Agent(*agent)),
            Key::Model => Some(Filter::Model(value.to_string())),
            Key::Branch => Some(Filter::Branch(value.to_string())),
            Key::After => start_of(value, names.calendar).map(Filter::After),
            Key::Before => start_of(value, names.calendar).map(Filter::Before),
        }
    }
}

/// The query `input` says, with the tokens in it that name no filter.
pub fn query(input: &str, names: &Names<'_>) -> (Query, Vec<Token>) {
    let parsed = parse(input);
    let mut query = Query {
        terms: parsed.terms,
        filters: Filters::default(),
    };
    let mut unresolved = Vec::new();
    for token in parsed.tokens {
        match token.resolve(names) {
            Some(filter) => query.filters.set(filter),
            None => unresolved.push(token),
        }
    }
    (query, unresolved)
}

/// The start, in `calendar`'s time and Unix milliseconds, of the day, month or year `value`
/// names: `YYYY-MM-DD`, `YYYY-MM` or `YYYY`.
fn start_of(value: &str, calendar: Calendar) -> Option<i64> {
    let parts: Vec<&str> = value.split('-').collect();
    let widths = [4, 2, 2];
    if parts.len() > widths.len()
        || parts.iter().zip(widths).any(|(part, width)| {
            part.len() != width || !part.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return None;
    }
    let number = |at: usize| parts.get(at).map_or(Some(1), |part| part.parse().ok());
    let midnight = NaiveDate::from_ymd_opt(parts[0].parse().ok()?, number(1)?, number(2)?)?
        .and_hms_opt(0, 0, 0)?;
    if calendar == Calendar::Utc {
        return Some(Utc.from_utc_datetime(&midnight).timestamp_millis());
    }
    /* A midnight that a clock change skips is no time: the first instant after it is. */
    Local
        .from_local_datetime(&midnight)
        .earliest()
        .or_else(|| {
            Local
                .from_local_datetime(&(midnight + chrono::Duration::hours(1)))
                .earliest()
        })
        .map(|start| start.timestamp_millis())
}

/// How much a search returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit {
    /// The most sessions, the best first.
    pub sessions: usize,
    /// The most matches given of each session; the rest are counted.
    pub per_session: usize,
}

impl Default for Limit {
    /// What the palette shows (DESIGN §4.9): four matches a session.
    fn default() -> Self {
        Self {
            sessions: 50,
            per_session: 4,
        }
    }
}

/// What a search answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Searched {
    /// The query's terms are a letter or two of ASCII alone, which nearly every session holds:
    /// it was not run (the module docs).
    KeepTyping,
    Results(Results),
}

/// What a search found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Results {
    /// The sessions found, the best first: those whose title holds every term, then the one
    /// that did something last.
    pub sessions: Vec<Found>,
    /// Every session with a match, and every match, of which `sessions` are the first.
    pub total_sessions: usize,
    pub total_matches: usize,
}

/// A session with matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub session: Session,
    /// How many of its spans hold a term.
    pub matches: usize,
    /// Its first matches: those that hold every term, then the others, each lot in the order
    /// of the conversation (the title first).
    pub shown: Vec<Match>,
}

/// One matching span: what a result row shows, and where opening it goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub kind: MatchKind,
    /// The text is a subagent's: opening the match opens the session at the call that started
    /// it, expanded (DESIGN §4.9), at this place in the subagent's own conversation.
    pub subagent: Option<InSubagent>,
    /// Where the match opens the session; `None` for its title, which opens it at its start.
    pub at: Option<At>,
    /// Only the start of the span's text is searched: it is longer than search indexes.
    pub cut: bool,
    pub snippet: Snippet,
}

/// The part of a span around its matches, with each match marked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snippet {
    pub text: String,
    /// Where in `text` the matches are, in bytes.
    pub hits: Vec<Range<usize>>,
}
