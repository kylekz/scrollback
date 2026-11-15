//! Search (PLAN §10, DESIGN §4.9): what the palette asks and what it is answered with. The
//! query is free text and filters, typed as `key:value` tokens and parsed here, so the syntax is
//! the same wherever it is typed; [`crate::catalogue::Catalogue::search`] runs it over the index
//! the index step keeps of every session (`session_text`, PLAN §4.3).
//!
//! The text is matched as substrings, without regard to case or accents, through FTS5's
//! `trigram` tokenizer: a term of three characters or more is looked up in the index; a shorter
//! one (`ok`, a two-character CJK word) only narrows what longer terms found, or, alone, is
//! looked for by reading the text, which is slower and is cancelled with the query. A span
//! matches when it holds every term; results are grouped by session.

use std::ops::Range;

use chrono::{Local, NaiveDate, TimeZone as _};

use crate::catalogue::{Folder, Session, Shelf};
use crate::location::{Location, LocationId};
use crate::provider::ProviderId;
use crate::transcript::{At, SpanKind};

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
    /// `in:` a folder, or a page of the library: `starred`, `unsorted`, `trash`, `all`.
    In,
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
    const ALL: [(Self, &'static str); 7] = [
        (Self::In, "in"),
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
/// typed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    pub terms: Vec<String>,
    pub tokens: Vec<Token>,
}

/**
 * Splits what was typed into terms and filter tokens. Terms are separated by white space; text
 * in double quotes is one term, spaces and all, and a quote with no partner runs to the end. A
 * word `key:value` whose key is one of [`Key`]'s, in any case, with a value (which may be
 * quoted), is a token; any other word with a colon (`C:\Users`, `https://…`) is a term.
 * Nothing typed is an error: whatever it holds, it is text to look for.
 */
pub fn parse(input: &str) -> Parsed {
    let mut parsed = Parsed::default();
    let mut rest = input;
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
            let value = unquote(value);
            (!value.is_empty()).then(|| Token {
                key: *key,
                value: value.to_string(),
            })
        });
        match token {
            Some(token) => parsed.tokens.push(token),
            None => {
                let term = unquote(word);
                if !term.trim().is_empty() {
                    parsed.terms.push(term.to_string());
                }
            }
        }
    }
}

/// The first word of `text`, which starts with no white space, and what follows it. White
/// space inside double quotes is part of the word.
fn word(text: &str) -> (&str, &str) {
    let mut quoted = false;
    for (at, char) in text.char_indices() {
        match char {
            '"' => quoted = !quoted,
            char if char.is_whitespace() && !quoted => return (&text[..at], &text[at..]),
            _ => {}
        }
    }
    (text, "")
}

/// `text` without the quotes around it, or after the colon of a token: `"a b"` is `a b`, and
/// so is `"a b` that a quote opened and nothing closed.
fn unquote(text: &str) -> &str {
    let text = text.strip_prefix('"').unwrap_or(text);
    text.strip_suffix('"').unwrap_or(text)
}

/// What a token's value can name.
pub struct Names<'a> {
    pub folders: &'a [Folder],
    pub locations: &'a [Location],
    pub agents: &'a [ProviderId],
}

impl Token {
    /**
     * The filter the token names, if it names one. `in:` takes a folder's name first (the first
     * folder of that name, in the order [`crate::catalogue::Catalogue::folders`] gives them),
     * then a page's; `from:` a location's name; `agent:` a provider's id; names are matched in
     * any case. `model:` and `branch:` take any value. A date that is not one is no filter.
     */
    pub fn resolve(&self, names: &Names<'_>) -> Option<Filter> {
        let value = self.value.as_str();
        match self.key {
            Key::In => names
                .folders
                .iter()
                .find(|folder| folder.name.eq_ignore_ascii_case(value))
                .map(|folder| Filter::Shelf(Shelf::Folder(folder.id)))
                .or_else(|| {
                    [
                        ("all", Shelf::All),
                        ("starred", Shelf::Starred),
                        ("unsorted", Shelf::Unsorted),
                        ("trash", Shelf::Trash),
                    ]
                    .into_iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(value))
                    .map(|(_, shelf)| Filter::Shelf(shelf))
                }),
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
            Key::After => start_of(value).map(Filter::After),
            Key::Before => start_of(value).map(Filter::Before),
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

/// The start, in local time and Unix milliseconds, of the day, month or year `value` names:
/// `YYYY-MM-DD`, `YYYY-MM` or `YYYY`.
fn start_of(value: &str) -> Option<i64> {
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

/// What a search found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Results {
    /// The sessions with matches, the best first: those whose title matches, then by how well
    /// their best match matches, then the one that did something last.
    pub sessions: Vec<Found>,
    /// Every session with a match, and every match, of which `sessions` are the first.
    pub total_sessions: usize,
    pub total_matches: usize,
}

/// A session with matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub session: Session,
    /// How many of its spans match.
    pub matches: usize,
    /// Its first matches, title first, then in the order of the conversation.
    pub shown: Vec<Match>,
}

/// One matching span: what a result row shows, and where opening it goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub kind: MatchKind,
    /// The text is a subagent's: opening the match opens the session at the call that started
    /// it, expanded (DESIGN §4.9).
    pub subagent: bool,
    /// Where the match opens the session; `None` for its title, which opens it at its start.
    pub at: Option<At>,
    pub snippet: Snippet,
}

/// The part of a span around its matches, with each match marked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snippet {
    pub text: String,
    /// Where in `text` the matches are, in bytes.
    pub hits: Vec<Range<usize>>,
}
