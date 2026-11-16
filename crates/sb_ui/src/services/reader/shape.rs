//! A call's texts as its tool's kind draws them (DESIGN §4.2.4, PLAN §7.4): a terminal's output
//! with its colours ([`ansi`]), a file read with its lines' numbers and its syntax, what a content
//! search found with its files and matches, JSON printed again and coloured ([`json`]), an edit's
//! diff ([`diff`]), a web search's results. Shaped off the UI thread, as the body is read, and
//! again for the whole of a part when Show all reads it; Find reads the same text
//! ([`crate::services::reader::find`]), so a match is where the body draws it.
//!
//! Every shape is bounded whatever the text: each pass is linear or capped, and the marks a text
//! keeps are at most [`MARKS`].

pub mod ansi;
pub mod diff;
pub mod json;

use std::ops::Range;

use regex::RegexBuilder;
use sb_core::transcript::{ToolCall, ToolInput, ToolKind};

use super::text::{Mark, Text};
use crate::syntax::TokenKind;

/// The most marks of numbers, files and matches one text keeps.
pub const MARKS: usize = 64 * 1024;
/// The largest a content search's pattern may compile to before it is looked for as written.
const PATTERN_BYTES: usize = 1024 * 1024;
/// The most results a web search's body lists.
pub const RESULTS: usize = 50;

/// What a call's body shows its input as: printed again as JSON, MCP's coloured, a tool's that
/// no provider knows on one line (DESIGN §4.2.4).
pub fn input(call: &ToolCall, text: String) -> Text {
    match call.kind {
        ToolKind::Mcp => coloured(json::pretty(text)),
        ToolKind::Other => coloured(json::one_line(text)),
        _ => Text::new(json::pretty(text)),
    }
}

/// What a call's body shows a text it returned as, by the call's kind.
pub fn output(call: &ToolCall, text: String) -> Text {
    match (&call.input, call.kind) {
        (_, ToolKind::Shell) => {
            let (text, marks) = ansi::strip(&text);
            Text::marked(text, marks)
        }
        (ToolInput::Read { path, .. }, _) => {
            let marks = numbered(&text, extension(path), &|| true);
            Text::marked(text, marks)
        }
        (ToolInput::ContentSearch { pattern, .. }, _) => {
            let marks = found(&text, pattern);
            Text::marked(text, marks)
        }
        (_, ToolKind::Mcp) => coloured(json::pretty(text)),
        (_, ToolKind::Other) => coloured(json::one_line(text)),
        _ => Text::new(text),
    }
}

/// `text` with its tokens as JSON coloured.
fn coloured(text: String) -> Text {
    let marks = json::marks(&text);
    Text::marked(text, marks)
}

/// The extension of the file at `path`, which names its language for the highlighter.
pub fn extension(path: &str) -> Option<&str> {
    let name = path.rsplit(['/', '\\']).next()?;
    name.rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|extension| !extension.is_empty())
}

/**
 * The start of a line numbered as `cat -n` numbers it (`   36→` or `   36\t`, spaces, digits, a
 * tab or an arrow): the end of its number and where its content starts.
 */
fn number_of(line: &str) -> Option<(usize, usize)> {
    let digits = line.trim_start_matches(' ');
    let start = line.len() - digits.len();
    let count = digits.bytes().take_while(u8::is_ascii_digit).count();
    if count == 0 {
        return None;
    }
    let after = &digits[count..];
    let separator = ['\t', '→']
        .into_iter()
        .find(|separator| after.starts_with(*separator))?;
    Some((start + count, start + count + separator.len_utf8()))
}

/**
 * The marks of a file's lines as read: each line's number when every line is numbered as
 * `cat -n` numbers it, and the content's syntax by `language` (an extension), within the
 * highlighter's bounds ([`crate::syntax`]) and while `wanted` says.
 */
pub fn numbered(
    text: &str,
    language: Option<&str>,
    wanted: &dyn Fn() -> bool,
) -> Vec<(Range<usize>, Mark)> {
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let shown = line.trim_end_matches(['\n', '\r']);
        match number_of(shown) {
            Some((number, content)) => starts.push((offset, number, content, shown.len())),
            None if shown.is_empty() => {}
            None => return highlighted(text, language, wanted),
        }
        offset += line.len();
    }
    /* The contents alone, joined, are what is highlighted. */
    let mut code = String::with_capacity(text.len());
    let mut places = Vec::with_capacity(starts.len());
    for &(line, _, content, end) in &starts {
        places.push((code.len(), line + content));
        code.push_str(&text[line + content..line + end]);
        code.push('\n');
    }
    let tokens = highlighted(&code, language, wanted);
    let mut marks = Vec::with_capacity(starts.len() + tokens.len());
    let mut token = 0;
    for (index, &(line, number, _, _)) in starts.iter().enumerate() {
        if marks.len() >= MARKS {
            break;
        }
        marks.push((line..line + number, Mark::Number));
        let (from, to) = (
            places[index].0,
            places.get(index + 1).map_or(code.len(), |next| next.0),
        );
        while token < tokens.len() && tokens[token].0.start < to {
            let (range, mark) = &tokens[token];
            let (start, end) = (range.start.max(from), range.end.min(to - 1));
            if start < end {
                let shift = places[index].1 - from;
                marks.push((start + shift..end + shift, *mark));
            }
            if range.end > to {
                break;
            }
            token += 1;
        }
    }
    marks
}

/// The syntax of `code` in `language`, as marks; none past the highlighter's bounds.
fn highlighted(
    code: &str,
    language: Option<&str>,
    wanted: &dyn Fn() -> bool,
) -> Vec<(Range<usize>, Mark)> {
    language
        .and_then(|language| crate::syntax::highlight_while(language, code, wanted))
        .unwrap_or_default()
        .into_iter()
        .filter(|(range, kind)| *kind != TokenKind::Plain && code.get(range.clone()).is_some())
        .map(|(range, kind)| (range, Mark::Token(kind)))
        .collect()
}

/**
 * The marks of what a content search returned: the files named over their matches, each line's
 * number, and where in each line `pattern` matches (as a regular expression, linear in the text
 * whatever it is; as written when it isn't one).
 */
pub fn found(text: &str, pattern: &str) -> Vec<(Range<usize>, Mark)> {
    let regex = RegexBuilder::new(pattern)
        .size_limit(PATTERN_BYTES)
        .dfa_size_limit(PATTERN_BYTES)
        .build()
        .or_else(|_| {
            RegexBuilder::new(&regex::escape(pattern))
                .size_limit(PATTERN_BYTES)
                .build()
        })
        .ok();
    let mut marks = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if marks.len() >= MARKS {
            break;
        }
        let shown = line.trim_end_matches(['\n', '\r']);
        /* `path:12:text`, `  12  text` under its file, `12:text`, or a file's name alone. */
        let (number, content) = match shown.split_once(':') {
            Some((head, rest))
                if !head.is_empty() && !head.starts_with(' ') && digits_then(rest, ':') =>
            {
                marks.push((offset..offset + head.len(), Mark::File));
                let start = head.len() + 1;
                let count = rest.bytes().take_while(u8::is_ascii_digit).count();
                (Some(start..start + count), start + count + 1)
            }
            _ => {
                let digits = shown.trim_start_matches(' ');
                let start = shown.len() - digits.len();
                let count = digits.bytes().take_while(u8::is_ascii_digit).count();
                match digits[count..].chars().next() {
                    Some(':' | ' ' | '\t' | '-') if count > 0 => {
                        (Some(start..start + count), start + count + 1)
                    }
                    _ if start == 0 && count == 0 && is_path(shown) => {
                        marks.push((offset..offset + shown.len(), Mark::File));
                        (None, shown.len())
                    }
                    _ => (None, 0),
                }
            }
        };
        if let Some(number) = number {
            marks.push((offset + number.start..offset + number.end, Mark::Number));
        }
        if let (Some(regex), Some(content)) = (&regex, shown.get(content.min(shown.len())..)) {
            let base = offset + shown.len() - content.len();
            marks.extend(
                regex
                    .find_iter(content)
                    .filter(|found| !found.is_empty())
                    .take(MARKS)
                    .map(|found| (base + found.start()..base + found.end(), Mark::Found)),
            );
        }
        offset += line.len();
    }
    marks.truncate(MARKS);
    marks
}

/// Whether `line` reads as a file's path: no spaces, and a separator or an extension.
fn is_path(line: &str) -> bool {
    !line.is_empty() && !line.contains(char::is_whitespace) && line.contains(['/', '\\', '.'])
}

/// Whether `text` starts with digits and then `after`.
fn digits_then(text: &str, after: char) -> bool {
    let count = text.bytes().take_while(u8::is_ascii_digit).count();
    count > 0 && text[count..].starts_with(after)
}

/// One result of a web search: its title, and where it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebResult {
    pub title: String,
    pub url: String,
}

/**
 * The results a web search returned, from the first JSON list of results (objects with a
 * `title` and a `url`) in its text, at most [`RESULTS`]; none when there is no such list.
 */
pub fn results(text: &str) -> Vec<WebResult> {
    let result = |found: &serde_json::Value| {
        Some(WebResult {
            title: found.get("title")?.as_str()?.to_string(),
            url: found.get("url")?.as_str()?.to_string(),
        })
    };
    text.match_indices("[{")
        .take(8)
        .find_map(|(at, _)| {
            let list = serde_json::Deserializer::from_str(&text[at..])
                .into_iter::<Vec<serde_json::Value>>()
                .next()?
                .ok()?;
            let found: Vec<WebResult> = list.iter().filter_map(result).take(RESULTS).collect();
            (!found.is_empty()).then_some(found)
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use pretty_assertions::assert_eq;

    use super::*;

    fn shown(text: &str, marks: &[(Range<usize>, Mark)]) -> Vec<(String, Mark)> {
        marks
            .iter()
            .map(|(range, mark)| (text[range.clone()].to_string(), *mark))
            .collect()
    }

    #[test]
    fn a_file_read_is_numbered_and_highlighted_by_its_extension() {
        let text = "    28\tfn main() {\n    29\t    let x = 1;\n";
        let marks = numbered(text, Some("rs"), &|| true);
        let shown = shown(text, &marks);
        assert_eq!(shown[0], ("    28".to_string(), Mark::Number));
        assert!(shown.contains(&("fn".to_string(), Mark::Token(TokenKind::Keyword))));
        assert!(shown.contains(&("1".to_string(), Mark::Token(TokenKind::Number))));
        assert!(
            marks
                .windows(2)
                .all(|pair| pair[0].0.end <= pair[1].0.start)
        );
        /* Not numbered: highlighted as it is. */
        assert!(numbered("let x = 1;", Some("rs"), &|| true).len() > 1);
        assert_eq!(extension("/home/user/a.b/Makefile"), None);
        assert_eq!(extension("C:\\code\\main.rs"), Some("rs"));
    }

    #[test]
    fn a_content_search_marks_its_files_numbers_and_matches() {
        let text = "src/main.rs:10:fn main() {\nsrc/lib.rs\n  30  let main = 1;\n";
        assert_eq!(
            shown(text, &found(text, "ma.n")),
            [
                ("src/main.rs".to_string(), Mark::File),
                ("10".to_string(), Mark::Number),
                ("main".to_string(), Mark::Found),
                ("src/lib.rs".to_string(), Mark::File),
                ("30".to_string(), Mark::Number),
                ("main".to_string(), Mark::Found),
            ]
        );
        /* A pattern that isn't a regular expression is found as written. */
        assert_eq!(found("a (b c", "(b").len(), 1);
    }

    #[test]
    fn a_web_search_lists_its_results() {
        let text = r#"Results for "x"\n\nLinks: [{"title":"One","url":"https://a.example/x"},{"title":"Two","url":"https://b.example/"}]"#;
        assert_eq!(
            results(text),
            [
                WebResult {
                    title: "One".to_string(),
                    url: "https://a.example/x".to_string()
                },
                WebResult {
                    title: "Two".to_string(),
                    url: "https://b.example/".to_string()
                },
            ]
        );
        assert_eq!(results("[{not json"), []);
    }

    #[test]
    fn hostile_output_is_shaped_in_bounded_time() {
        let started = Instant::now();
        /* A pattern that matches everywhere, on a megabyte of one line. */
        let line = "a".repeat(1024 * 1024);
        assert_eq!(found(&line, "").len(), 0);
        assert_eq!(found(&line, "a").len(), MARKS);
        /* A pattern that would be exponential for a backtracking engine. */
        found(&"a".repeat(100_000), "(a*)*b");
        /* Numbered lines by the hundred thousand, of a language highlighted. */
        let lines: String = (1..100_000).map(|n| format!("{n:>6}\tx = {n}\n")).collect();
        numbered(&lines, Some("py"), &|| true);
        /* Brackets and quotes that open lists everywhere. */
        results(&"[{\"title\":".repeat(100_000));
        assert!(
            started.elapsed().as_secs_f64() < 10.0,
            "{:?}",
            started.elapsed()
        );
    }
}
