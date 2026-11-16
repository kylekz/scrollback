//! Text as search compares it: folded exactly as the index's `trigram` tokenizer folds it
//! (`case_sensitive 0 remove_diacritics 1`), by the table taken from it ([`super::fold`]). The
//! index finds the spans that may hold a term; this says whether one does, and where.

use std::ops::Range;

use super::Snippet;
use super::fold::FOLDS;

/// How many characters a snippet shows on each side of the match it is about.
const AROUND: usize = 32;

/// `char` as the index compares it; `None` for a mark the index drops.
fn fold(char: char) -> Option<char> {
    match FOLDS.binary_search_by_key(&u32::from(char), |(from, _)| *from) {
        Ok(at) => match FOLDS[at].1 {
            0 => None,
            to => char::from_u32(to),
        },
        Err(_) => Some(char),
    }
}

/// `text` folded ([`fold`]).
pub(super) fn folded(text: &str) -> String {
    text.chars().filter_map(fold).collect()
}

/// Whether `text` holds `term`, which is [`folded`] already.
pub(super) fn holds(text: &str, term: &str) -> bool {
    /* The index folds ASCII to lower case and nothing else. */
    if text.is_ascii() && term.is_ascii() {
        return text.to_ascii_lowercase().contains(term);
    }
    folded(text).contains(term)
}

/// The part of `text` about the first match of the rarest of `terms` ([`folded`] already) in
/// it, the whole of that match always, with every match in that part marked.
pub(super) fn snippet(text: &str, terms: &[String]) -> Snippet {
    /* Where each character of `text` starts in it, and where `text` ends. */
    let starts: Vec<usize> = text
        .char_indices()
        .map(|(at, _)| at)
        .chain([text.len()])
        .collect();
    /* The folded text, each character with the place of its own in `text`. */
    let (chars, places): (Vec<char>, Vec<usize>) = text
        .chars()
        .enumerate()
        .filter_map(|(place, char)| fold(char).map(|folded| (folded, place)))
        .unzip();
    /* Where in `text` the folded characters from `at` start, and where those up to `at` end. */
    let start = |at: usize| places.get(at).map_or(text.len(), |place| starts[*place]);
    let end = |at: usize| match at {
        0 => 0,
        at => starts[places[at - 1] + 1],
    };
    let mut hits: Vec<Range<usize>> = Vec::new();
    /* The first match of the term found least often: what is rare is what was looked for. */
    let mut rarest: Option<(usize, Range<usize>)> = None;
    for term in terms {
        let term: Vec<char> = term.chars().collect();
        if term.is_empty() || term.len() > chars.len() {
            continue;
        }
        let (mut at, mut found, mut first) = (0, 0, None);
        while at + term.len() <= chars.len() {
            if chars[at..at + term.len()] == term[..] {
                hits.push(at..at + term.len());
                found += 1;
                first.get_or_insert(at..at + term.len());
                at += term.len();
            } else {
                at += 1;
            }
        }
        if let Some(first) = first
            && rarest.as_ref().is_none_or(|(least, _)| found < *least)
        {
            rarest = Some((found, first));
        }
    }
    hits.sort_by_key(|hit| (hit.start, hit.end));
    let about = rarest.map_or(0..0, |(_, first)| first);
    let from = about.start.saturating_sub(AROUND);
    let to = (about.end + AROUND).min(chars.len());
    let mut snippet = Snippet {
        text: String::new(),
        hits: Vec::new(),
    };
    if from > 0 {
        snippet.text.push('…');
    }
    let offset = snippet.text.len();
    let (first, last) = (start(from), end(to).max(start(from)));
    snippet.text.push_str(&text[first..last]);
    let mut shown_to = 0;
    for hit in hits {
        /* A match that runs past the part shown, or into one marked already, is not marked. */
        if hit.start < from || hit.end > to || hit.start < shown_to {
            continue;
        }
        snippet
            .hits
            .push(offset + start(hit.start) - first..offset + end(hit.end) - first);
        shown_to = hit.end;
    }
    if to < chars.len() {
        snippet.text.push('…');
    }
    snippet
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use toolbox_store::rusqlite::{Connection, params};

    use super::*;

    /**
     * The fold is the index's own, for every code point: what the bundled SQLite's `trigram`
     * tokenizer makes of each character (a run of three of it is one token, of three folded
     * characters, or none for a mark it drops) is what [`fold`] says.
     */
    #[test]
    fn the_fold_is_the_indexs_for_every_code_point() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE VIRTUAL TABLE t USING fts5 (x,
                 tokenize = 'trigram case_sensitive 0 remove_diacritics 1', detail = none);
             CREATE VIRTUAL TABLE v USING fts5vocab (t, 'instance');",
        )
        .unwrap();
        let chars: Vec<char> = (1..=u32::from(char::MAX))
            .filter_map(char::from_u32)
            .collect();
        let tx = c.unchecked_transaction().unwrap();
        {
            let mut insert = tx
                .prepare("INSERT INTO t (rowid, x) VALUES (?1, ?2)")
                .unwrap();
            for char in &chars {
                insert
                    .execute(params![u32::from(*char), char.to_string().repeat(3)])
                    .unwrap();
            }
        }
        tx.commit().unwrap();
        let tokens: HashMap<u32, String> = c
            .prepare("SELECT doc, term FROM v")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let differ: Vec<(char, Option<&String>)> = chars
            .iter()
            .filter(|char| {
                let index = tokens.get(&u32::from(**char));
                index.cloned() != fold(**char).map(|folded| folded.to_string().repeat(3))
            })
            .map(|char| (*char, tokens.get(&u32::from(*char))))
            .take(10)
            .collect();
        assert_eq!(differ, []);
    }

    #[test]
    fn text_is_compared_folded_and_marked_where_it_is() {
        assert_eq!(folded("Café ÉTÉ Юг"), "cafe ete юг");
        assert!(holds("Rename the Café edition", "cafe"));
        assert!(holds("Юг и север", &folded("ЮГ")));
        assert!(holds("Tiếng Việt", &folded("viet")));
        assert!(holds("ΟΔΟΣ", &folded("οδος")));
        assert!(holds("ᏣᎳᎩ", &folded("ᏣᎳᎩ")));
        /* A mark the index drops is no part of either. */
        assert!(holds("re\u{301}sume\u{301}", "resume"));
        assert!(!holds("parse args", "parse_args"));
        let marked = snippet("Where is PARSE_ARGS? parse_args.", &[folded("parse_args")]);
        assert_eq!(marked.hits.len(), 2);
        for hit in marked.hits {
            assert!(marked.text[hit].eq_ignore_ascii_case("parse_args"));
        }
        let long = format!("{}needle{}", "x".repeat(100), "y".repeat(100));
        let marked = snippet(&long, &["needle".to_string()]);
        assert!(marked.text.starts_with('…') && marked.text.ends_with('…'));
        assert_eq!(&marked.text[marked.hits[0].clone()], "needle");
        let marked = snippet("Résumé of 解析器", &[folded("resume"), folded("解析")]);
        let shown: Vec<&str> = marked
            .hits
            .iter()
            .map(|hit| &marked.text[hit.clone()])
            .collect();
        assert_eq!(shown, ["Résumé", "解析"]);
        let marked = snippet("re\u{301}sume\u{301} x", &["resume".to_string()]);
        assert_eq!(&marked.text[marked.hits[0].clone()], "re\u{301}sume");
        /* A term longer than the part about it is shown and marked whole. */
        let identifier = "a_very_long_identifier_name_that_runs_past_thirty_two_characters";
        let text = format!("{} {identifier} {}", "x ".repeat(60), "y ".repeat(60));
        let marked = snippet(&text, &[identifier.to_string()]);
        assert_eq!(&marked.text[marked.hits[0].clone()], identifier);
    }
}
