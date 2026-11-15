//! Text as search compares it: without regard to case or to the accents of Latin letters, as
//! the index's `trigram` tokenizer folds it (`case_sensitive 0 remove_diacritics 1`). The index
//! finds the spans that may hold a term; this says whether one does, and where.

use std::ops::Range;

use super::Snippet;

/// How many characters a snippet shows on each side of its first match.
const AROUND: usize = 32;

/// `char` as it is compared: in lower case, and a Latin letter without its accent.
fn fold(char: char) -> char {
    let lower = char.to_lowercase().next().unwrap_or(char);
    match lower {
        'à'..='å' | 'ā' | 'ă' | 'ą' => 'a',
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => 'c',
        'ď' => 'd',
        'è'..='ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => 'g',
        'ĥ' => 'h',
        'ì'..='ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' => 'i',
        'ĵ' => 'j',
        'ķ' => 'k',
        'ĺ' | 'ļ' | 'ľ' => 'l',
        'ñ' | 'ń' | 'ņ' | 'ň' => 'n',
        'ò'..='ö' | 'ō' | 'ŏ' | 'ő' => 'o',
        'ŕ' | 'ŗ' | 'ř' => 'r',
        'ś' | 'ŝ' | 'ş' | 'š' => 's',
        'ţ' | 'ť' => 't',
        'ù'..='ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => 'u',
        'ŵ' => 'w',
        'ý' | 'ÿ' | 'ŷ' => 'y',
        'ź' | 'ż' | 'ž' => 'z',
        other => other,
    }
}

/// `text` folded ([`fold`]), a character for each of its own.
pub(super) fn folded(text: &str) -> String {
    text.chars().map(fold).collect()
}

/// Whether `text` holds `term`, which is [`folded`] already.
pub(super) fn holds(text: &str, term: &str) -> bool {
    if text.is_ascii() && term.is_ascii() {
        return text.to_ascii_lowercase().contains(term);
    }
    folded(text).contains(term)
}

/// The part of `text` about its first match of any of `terms` ([`folded`] already), with every
/// match in that part marked.
pub(super) fn snippet(text: &str, terms: &[String]) -> Snippet {
    /* Where each character of the folded text starts in `text`, and where `text` ends. */
    let starts: Vec<usize> = text
        .char_indices()
        .map(|(at, _)| at)
        .chain([text.len()])
        .collect();
    let chars: Vec<char> = text.chars().map(fold).collect();
    let mut hits: Vec<Range<usize>> = Vec::new();
    /* Where the term found least often is first found: what is rare is what was looked for. */
    let mut rarest: Option<(usize, usize)> = None;
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
                first.get_or_insert(at);
                at += term.len();
            } else {
                at += 1;
            }
        }
        if let Some(first) = first
            && rarest.is_none_or(|(least, _)| found < least)
        {
            rarest = Some((found, first));
        }
    }
    hits.sort_by_key(|hit| (hit.start, hit.end));
    let first = rarest.map_or(0, |(_, at)| at);
    let from = first.saturating_sub(AROUND);
    let to = (first + AROUND).min(chars.len());
    let mut snippet = Snippet {
        text: String::new(),
        hits: Vec::new(),
    };
    if from > 0 {
        snippet.text.push('…');
    }
    let offset = snippet.text.len();
    snippet.text.push_str(&text[starts[from]..starts[to]]);
    let mut shown_to = 0;
    for hit in hits {
        /* A match that runs past the part shown, or into one marked already, is not marked. */
        if hit.start < from || hit.end > to || hit.start < shown_to {
            continue;
        }
        snippet.hits.push(
            offset + starts[hit.start] - starts[from]..offset + starts[hit.end] - starts[from],
        );
        shown_to = hit.end;
    }
    if to < chars.len() {
        snippet.text.push('…');
    }
    snippet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_compared_without_case_or_accents_and_marked_where_it_is() {
        assert_eq!(folded("Café ÉTÉ Юг"), "cafe ete юг");
        assert!(holds("Rename the Café edition", "cafe"));
        assert!(holds("Юг и север", &folded("ЮГ")));
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
    }
}
