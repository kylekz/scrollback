//! What the palette's query says (DESIGN §4.9): its chips, the filters already applied, and
//! the text typed, whose finished `key:value` tokens turn into chips and whose last word, while
//! it is being typed, is completed. The syntax is `sb_core`'s ([`sb_core::search::parse`]):
//! this only says where in the text each word is, so a token can leave it for its chip.
//!
//! **Only finished tokens count.** A token is finished once white space follows it: one being
//! typed (`branch:ma`) is no filter and no term yet, so the results don't change under every
//! letter of it. A finished token that names nothing holds the results: the palette says so
//! and searches nothing until it is mended or taken away.

use std::ops::Range;

use sb_core::catalogue::{Folder, Shelf};
use sb_core::location::Location;
use sb_core::provider::ProviderId;
use sb_core::search::{Calendar, Filter, Filters, Key, Names, Query, Token, parse, word};

use crate::components::end_cut;

/// The pages `in:` names before folders, as typed and as their chips say them.
const PAGES: [(&str, Shelf, &str); 4] = [
    ("all", Shelf::All, "All sessions"),
    ("starred", Shelf::Starred, "Starred"),
    ("unsorted", Shelf::Unsorted, "Unsorted"),
    ("trash", Shelf::Trash, "Trash"),
];

/// Every key, in the order completions offer them.
const KEYS: [Key; 8] = [
    Key::In,
    Key::Folder,
    Key::From,
    Key::Agent,
    Key::Model,
    Key::Branch,
    Key::After,
    Key::Before,
];

/// How many completions show at once.
const COMPLETIONS: usize = 6;

/// The most characters of a value a chip says.
const CHIP_CHARS: usize = 40;

/// A filter the palette applies, as its chip says it: "Folder: fc", "Location: Ubuntu-24.04".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chip {
    pub filter: Filter,
    pub label: String,
}

impl Chip {
    fn new(filter: Filter, what: &str, value: &str) -> Self {
        Self {
            filter,
            label: format!("{what}: {}", end_cut(value, CHIP_CHARS)),
        }
    }

    /// Whether `other` sets the same filter, which one query has once.
    fn same_kind(&self, other: &Self) -> bool {
        std::mem::discriminant(&self.filter) == std::mem::discriminant(&other.filter)
    }
}

/// What the palette's tokens can name, and how their chips say it.
pub struct Known<'a> {
    pub folders: &'a [Folder],
    pub locations: &'a [Location],
    /// Each agent's provider, and the agent's name.
    pub agents: &'a [(ProviderId, String)],
    /// The same providers alone, as tokens name them.
    pub agent_ids: &'a [ProviderId],
    pub models: &'a [String],
    /// How a model is written: as its agent's provider writes it.
    pub model_name: &'a dyn Fn(&str) -> String,
    pub branches: &'a [String],
    /// The years and the month after: and before: offer, the latest first.
    pub dates: &'a [String],
    /// The calendar dates are shown in, and so read in.
    pub calendar: Calendar,
}

impl<'a> Known<'a> {
    fn chip(&self, filter: Filter, typed: &str) -> Chip {
        match &filter {
            Filter::Shelf(Shelf::Folder(id)) => {
                let name = (self.folders.iter())
                    .find(|folder| folder.id == *id)
                    .map_or(typed, |folder| folder.name.as_str());
                Chip::new(filter.clone(), "Folder", name)
            }
            Filter::Shelf(shelf) => {
                let (_, _, name) = PAGES
                    .iter()
                    .find(|(_, it, _)| it == shelf)
                    .copied()
                    .unwrap_or(PAGES[0]);
                Chip::new(filter.clone(), "In", name)
            }
            Filter::Location(id) => {
                let name = (self.locations.iter())
                    .find(|location| location.id == *id)
                    .map_or(typed, |location| location.name.as_str());
                Chip::new(filter.clone(), "Location", name)
            }
            Filter::Agent(id) => {
                let name = (self.agents.iter())
                    .find(|(agent, _)| agent == id)
                    .map_or(typed, |(_, name)| name.as_str());
                Chip::new(filter.clone(), "Agent", name)
            }
            Filter::Model(model) => Chip::new(filter.clone(), "Model", &(self.model_name)(model)),
            Filter::Branch(branch) => Chip::new(filter.clone(), "Branch", branch),
            Filter::After(_) => Chip::new(filter, "After", typed),
            Filter::Before(_) => Chip::new(filter, "Before", typed),
        }
    }

    /// The chip `token` makes, when it names something.
    pub fn resolve(&self, token: &Token) -> Option<Chip> {
        let names = Names {
            folders: self.folders,
            locations: self.locations,
            agents: self.agent_ids,
            calendar: self.calendar,
        };
        token
            .resolve(&names)
            .map(|filter| self.chip(filter, &token.value))
    }

    /// What `key:` can be followed by, as typed.
    fn values(&self, key: Key) -> Vec<&str> {
        let folders = || self.folders.iter().map(|folder| folder.name.as_str());
        let all = |values: &'a [String]| values.iter().map(String::as_str).collect();
        match key {
            Key::In => (PAGES.iter().map(|(name, ..)| *name))
                .chain(folders())
                .collect(),
            Key::Folder => folders().collect(),
            Key::From => self.locations.iter().map(|it| it.name.as_str()).collect(),
            Key::Agent => self.agent_ids.iter().map(|id| id.as_str()).collect(),
            Key::Model => all(self.models),
            Key::Branch => all(self.branches),
            Key::After | Key::Before => all(self.dates),
        }
    }
}

/// A completion of the last word being typed: `Tab` takes the first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Completion {
    /// The word is the start of a key's name: it becomes `key:`.
    Key(Key),
    /// The word is a key and the start of a value: it becomes a chip.
    Chip(Chip),
}

impl Completion {
    /// What it reads as among the completions.
    pub fn label(&self) -> String {
        match self {
            Self::Key(key) => format!("{}:", key.as_str()),
            Self::Chip(chip) => chip.label.clone(),
        }
    }
}

/// The words of `text` and where each is, in bytes, as `sb_core`'s parser reads them.
fn words(text: &str) -> Vec<Range<usize>> {
    let mut words = Vec::new();
    let mut at = 0;
    loop {
        let rest = &text[at..];
        let start = at + rest.len() - rest.trim_start().len();
        if start == text.len() {
            return words;
        }
        let (word, _) = word(&text[start..]);
        words.push(start..start + word.len());
        at = start + word.len();
    }
}

/// The token `word` is, when it is one with a value.
fn token(word: &str) -> Option<Token> {
    let parsed = parse(word);
    match (parsed.tokens.as_slice(), parsed.terms.is_empty()) {
        ([token], true) => Some(token.clone()),
        _ => None,
    }
}

/**
 * Takes the finished tokens of `text` that name something into chips: each word `key:value`
 * with white space after it. Returns what is left of the text, and the chips in the order
 * typed. A token that names nothing stays in the text, for the palette to say so.
 */
pub fn take_tokens(text: &str, known: &Known<'_>) -> (String, Vec<Chip>) {
    let mut chips = Vec::new();
    let mut left = String::new();
    let mut from = 0;
    for word in words(text) {
        let finished = text[word.end..].starts_with(char::is_whitespace);
        let chip = token(&text[word.clone()])
            .filter(|_| finished)
            .and_then(|token| known.resolve(&token));
        if let Some(chip) = chip {
            left.push_str(&text[from..word.start]);
            chips.push(chip);
            // The space after it goes with it.
            from = word.end + text[word.end..].chars().next().map_or(0, char::len_utf8);
        }
    }
    left.push_str(&text[from..]);
    (left, chips)
}

/// Adds `chip` to `chips`, in place of one that sets the same filter.
pub fn add(chips: &mut Vec<Chip>, chip: Chip) {
    match chips.iter().position(|it| it.same_kind(&chip)) {
        Some(at) => chips[at] = chip,
        None => chips.push(chip),
    }
}

/// What the chips and the query field ask for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asked {
    /// The terms of the text and the filters of the chips and its finished tokens.
    pub query: Query,
    /// The terms as Find in session is filled with them: no quotes, no tokens.
    pub find: String,
    /// The first finished token that names nothing, which holds the results (module docs).
    pub unresolved: Option<Token>,
}

/// What `chips` and the query field's `text` ask for (module docs).
pub fn asked(chips: &[Chip], text: &str, known: &Known<'_>) -> Asked {
    /* A token or a key being typed is left out until it is finished. */
    let end = match words(text).pop() {
        Some(last)
            if last.end == text.len()
                && (token(&text[last.clone()]).is_some()
                    || !parse(&text[last.clone()]).incomplete.is_empty()) =>
        {
            last.start
        }
        _ => text.len(),
    };
    let parsed = parse(&text[..end]);
    let mut filters = Filters::default();
    for chip in chips {
        filters.set(chip.filter.clone());
    }
    let mut unresolved = None;
    for token in parsed.tokens {
        match known.resolve(&token) {
            Some(chip) => filters.set(chip.filter),
            None => {
                unresolved.get_or_insert(token);
            }
        }
    }
    Asked {
        find: parsed.terms.join(" "),
        query: Query {
            terms: parsed.terms,
            filters,
        },
        unresolved,
    }
}

/// The completions of the last word of `text`, while it is being typed (no white space after
/// it), the best first.
pub fn completions(text: &str, known: &Known<'_>) -> Vec<Completion> {
    let Some(word) = words(text).pop().filter(|word| word.end == text.len()) else {
        return Vec::new();
    };
    let word = &text[word];
    match word.split_once(':') {
        Some((key, typed)) => {
            let Some(key) = KEYS.iter().find(|it| it.as_str().eq_ignore_ascii_case(key)) else {
                return Vec::new();
            };
            let typed = typed.trim_matches('"').to_lowercase();
            known
                .values(*key)
                .into_iter()
                .filter(|value| value.to_lowercase().contains(&typed))
                .filter_map(|value| {
                    known.resolve(&Token {
                        key: *key,
                        value: value.to_string(),
                    })
                })
                .take(COMPLETIONS)
                .map(Completion::Chip)
                .collect()
        }
        None if word.chars().count() >= 2 => KEYS
            .iter()
            .filter(|key| {
                key.as_str().len() > word.len()
                    && key.as_str().starts_with(&word.to_ascii_lowercase())
            })
            .map(|key| Completion::Key(*key))
            .collect(),
        None => Vec::new(),
    }
}

/// `text` with its last word taken by `completion`: `key:` for a key, nothing for a chip.
pub fn complete(text: &str, completion: &Completion) -> String {
    let start = words(text).pop().map_or(text.len(), |word| word.start);
    match completion {
        Completion::Key(key) => format!("{}{}:", &text[..start], key.as_str()),
        Completion::Chip(_) => text[..start].to_string(),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use sb_core::filing::FolderId;

    use super::*;

    fn known_with<T>(test: impl FnOnce(&Known<'_>) -> T) -> T {
        let folders = [Folder {
            id: FolderId(7),
            parent: None,
            name: "fc".to_string(),
        }];
        test(&Known {
            folders: &folders,
            locations: &[],
            agents: &[],
            agent_ids: &[],
            models: &["model-opus".to_string(), "model-haiku".to_string()],
            model_name: &|model| model.replace("model-", "Model "),
            branches: &["main".to_string()],
            dates: &["2026".to_string()],
            calendar: Calendar::Local,
        })
    }

    #[test]
    fn a_finished_token_turns_into_a_chip_and_leaves_the_text() {
        known_with(|known| {
            let (left, chips) = take_tokens("backoff folder:fc 429", known);
            assert_eq!(left, "backoff 429");
            assert_eq!(
                chips,
                [Chip {
                    filter: Filter::Shelf(Shelf::Folder(FolderId(7))),
                    label: "Folder: fc".to_string(),
                }]
            );
            /* One still being typed stays, and so does one that names nothing. */
            assert_eq!(
                take_tokens("x from:Ubu", known),
                ("x from:Ubu".to_string(), vec![])
            );
            assert_eq!(
                take_tokens("from:nowhere x", known),
                ("from:nowhere x".to_string(), vec![])
            );
            let (left, chips) = take_tokens("FOLDER:FC in:trash ", known);
            assert_eq!(left, "");
            assert_eq!(
                chips.iter().map(|it| it.label.as_str()).collect::<Vec<_>>(),
                ["Folder: fc", "In: Trash"]
            );
        });
    }

    #[test]
    fn only_finished_tokens_count_and_one_that_names_nothing_is_said() {
        known_with(|known| {
            let chips = [Chip {
                filter: Filter::Shelf(Shelf::Trash),
                label: "In: Trash".to_string(),
            }];
            /* The token being typed is no filter and no term yet. */
            let typing = asked(&chips, "backoff \"rate limit\" model:op", known);
            assert_eq!(typing.query.terms, ["backoff", "rate limit"]);
            assert_eq!(typing.query.filters.shelf, Shelf::Trash);
            assert_eq!(typing.query.filters.model, None);
            assert_eq!(typing.find, "backoff rate limit");
            assert_eq!(typing.unresolved, None);
            assert_eq!(asked(&chips, "backoff in:", known).query.terms, ["backoff"]);
            /* Once finished, it filters; one that names nothing is said. */
            let finished = asked(&chips, "backoff model:op from:x 429 ", known);
            assert_eq!(finished.query.filters.model.as_deref(), Some("op"));
            assert_eq!(finished.query.terms, ["backoff", "429"]);
            assert_eq!(
                finished.unresolved.map(|it| it.value),
                Some("x".to_string())
            );
        });
    }

    #[test]
    fn the_last_word_completes_to_a_key_or_a_chip() {
        known_with(|known| {
            assert_eq!(completions("x fr", known), [Completion::Key(Key::From)]);
            assert_eq!(complete("x fr", &Completion::Key(Key::From)), "x from:");
            assert_eq!(
                completions("model:hai", known)
                    .iter()
                    .map(Completion::label)
                    .collect::<Vec<_>>(),
                ["Model: Model haiku"]
            );
            assert_eq!(completions("in:", known).len(), 5);
            assert_eq!(completions("x ", known), []);
            assert_eq!(completions("x f", known), []);
            let mut chips = vec![Chip {
                filter: Filter::Model("a".to_string()),
                label: "Model: a".to_string(),
            }];
            add(
                &mut chips,
                Chip {
                    filter: Filter::Model("b".to_string()),
                    label: "Model: b".to_string(),
                },
            );
            assert_eq!(chips.len(), 1);
            assert_eq!(chips[0].label, "Model: b");
        });
    }
}
