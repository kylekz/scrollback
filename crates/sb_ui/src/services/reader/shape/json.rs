//! JSON as a body draws it (DESIGN §4.2.4 "MCP", "Unknown tool"): pretty-printed, or on one
//! line, and coloured: keys `p`, strings `s`, numbers `n`, literals `k`.
//!
//! It is a transcript's, so hostile. Printing it again parses it, which holds several copies:
//! only up to [`PRETTY_BYTES`], and serde's own bound on nesting stops a parse that goes too
//! deep. Indentation grows with depth, so what is printed is bounded too: past
//! [`PRINTED_BYTES`] the printing stops and the text is shown as it was sent. Colouring is one
//! pass that never looks back and needs no nesting, whatever the text is, JSON or not.

use std::ops::Range;

use crate::services::reader::text::Mark;
use crate::syntax::TokenKind;

/// The most of a text printed again as JSON: past it, it is shown as it was sent.
pub const PRETTY_BYTES: usize = 64 * 1024;
/// The most a text printed again may come to: past it, it is shown as it was sent.
pub const PRINTED_BYTES: usize = 3 * PRETTY_BYTES;

/// Bytes written up to [`PRINTED_BYTES`]; a write past them fails, which ends the printing.
struct Bounded(Vec<u8>);

impl std::io::Write for Bounded {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.0.len() + buf.len() > PRINTED_BYTES {
            return Err(std::io::Error::other("printed past its bound"));
        }
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `value` printed by `print` within [`PRINTED_BYTES`].
fn printed(
    value: &serde_json::Value,
    print: impl FnOnce(&mut Bounded, &serde_json::Value) -> serde_json::Result<()>,
) -> Option<String> {
    let mut out = Bounded(Vec::new());
    print(&mut out, value).ok()?;
    String::from_utf8(out.0).ok()
}
/// The most marks one text keeps; past them the rest is drawn plain.
pub const MARKS: usize = 64 * 1024;

fn value(text: &str) -> Option<serde_json::Value> {
    (text.len() <= PRETTY_BYTES)
        .then(|| serde_json::from_str(text).ok())
        .flatten()
}

/// `text` pretty-printed when it is JSON of at most [`PRETTY_BYTES`] whose printing comes to
/// at most [`PRINTED_BYTES`], else as it is.
pub fn pretty(text: String) -> String {
    value(&text)
        .and_then(|value| {
            printed(&value, |out, value| {
                serde_json::to_writer_pretty(out, value)
            })
        })
        .unwrap_or(text)
}

/// `text` on one line when it is JSON of at most [`PRETTY_BYTES`], else as it is.
pub fn one_line(text: String) -> String {
    value(&text)
        .and_then(|value| printed(&value, |out, value| serde_json::to_writer(out, value)))
        .unwrap_or(text)
}

/**
 * The tokens of `text` read as JSON: a string followed by `:` is a key. What isn't JSON is
 * coloured as far as it looks like it, and costs no more.
 */
pub fn marks(text: &str) -> Vec<(Range<usize>, Mark)> {
    let bytes = text.as_bytes();
    let mut marks = Vec::new();
    let mut at = 0;
    while at < bytes.len() && marks.len() < MARKS {
        let start = at;
        let kind = match bytes[at] {
            b'"' => {
                at += 1;
                while at < bytes.len() && bytes[at] != b'"' && bytes[at] != b'\n' {
                    at += if bytes[at] == b'\\' { 2 } else { 1 };
                }
                at = (at + 1).min(bytes.len());
                let after = bytes[at..]
                    .iter()
                    .position(|byte| !matches!(byte, b' ' | b'\t'));
                if after.is_some_and(|after| bytes[at + after] == b':') {
                    TokenKind::Property
                } else {
                    TokenKind::String
                }
            }
            b'-' | b'0'..=b'9' => {
                at += 1;
                while at < bytes.len()
                    && matches!(bytes[at], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
                {
                    at += 1;
                }
                TokenKind::Number
            }
            b'a'..=b'z' => {
                while at < bytes.len() && bytes[at].is_ascii_lowercase() {
                    at += 1;
                }
                if matches!(&text[start..at], "true" | "false" | "null") {
                    TokenKind::Keyword
                } else {
                    continue;
                }
            }
            _ => {
                at += 1;
                continue;
            }
        };
        /* An escape cut by the text's end, or a string cut inside a character, ends where a
        character does. */
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        if start < at {
            marks.push((start..at, Mark::Token(kind)));
        } else {
            at = start + 1;
        }
    }
    marks
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use sb_test_support::growth::assert_linear;

    use super::*;

    fn tokens(text: &str) -> Vec<(&str, TokenKind)> {
        marks(text)
            .into_iter()
            .map(|(range, mark)| match mark {
                Mark::Token(kind) => (&text[range], kind),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn keys_strings_numbers_and_literals_are_told_apart() {
        let text =
            pretty(r#"{"q":"a \"b\"","n":-1.5e3,"ok":true,"none":null,"list":[1]}"#.to_string());
        assert_eq!(
            tokens(&text),
            [
                ("\"q\"", TokenKind::Property),
                ("\"a \\\"b\\\"\"", TokenKind::String),
                ("\"n\"", TokenKind::Property),
                ("-1500.0", TokenKind::Number),
                ("\"ok\"", TokenKind::Property),
                ("true", TokenKind::Keyword),
                ("\"none\"", TokenKind::Property),
                ("null", TokenKind::Keyword),
                ("\"list\"", TokenKind::Property),
                ("1", TokenKind::Number),
            ]
        );
        assert_eq!(one_line("{ \"a\" : [1, 2] }".to_string()), r#"{"a":[1,2]}"#);
        assert_eq!(pretty("not json".to_string()), "not json");
        /* Not JSON, cut in a string or an escape or a character: coloured as far as it goes. */
        assert_eq!(tokens("\"open"), [("\"open", TokenKind::String)]);
        assert_eq!(tokens("\"a\\"), [("\"a\\", TokenKind::String)]);
        assert_eq!(tokens("\"é"), [("\"é", TokenKind::String)]);
        assert_eq!(tokens("word truex"), []);
    }

    #[test]
    fn deep_and_large_json_is_shown_as_sent_and_coloured_in_one_pass() {
        /* Nested past serde's bound: shown as it was sent, never parsed to the bottom. */
        let deep = |depth: usize| format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        assert_eq!(pretty(deep(100_000)), deep(100_000));
        assert_eq!(one_line(deep(100_000)), deep(100_000));
        let objects = |depth: usize| format!("{}1{}", "{\"a\":".repeat(depth), "}".repeat(depth));
        assert_eq!(pretty(objects(50_000)), objects(50_000));
        assert_eq!(marks(&objects(50_000)).len(), 50_001);
        /* Past the bound: not parsed at all. */
        let large = format!("[{}0]", "0,".repeat(PRETTY_BYTES));
        assert_eq!(pretty(large.clone()).len(), large.len());
        /* Within it, nested deep enough that its indentation would come to megabytes: shown as
        sent (the reviewer's 1 MiB at 120 deep printed 127 MB). */
        for depth in [32usize, 64, 120] {
            let n = (PRETTY_BYTES - 2 * depth - 2) / 2;
            let deep = format!(
                "{}{}0{}",
                "[".repeat(depth),
                "0,".repeat(n),
                "]".repeat(depth)
            );
            let printed = pretty(deep.clone());
            assert!(printed.len() <= PRINTED_BYTES, "{depth}: {}", printed.len());
            assert_eq!(printed, deep);
        }
        assert!(pretty(r#"{"a":[1,2]}"#.to_string()).contains('\n'));
        /* A text of nothing but tokens: the marks stop at their bound. */
        let numbers = |count: usize| "1,".repeat(count);
        assert_eq!(marks(&numbers(1_000_000)).len(), MARKS);

        /* Each is coloured at about its bytes' cost: eight times the text, about eight times the
        time. Quotes that never close, and escapes everywhere, too. */
        let linear = |name: &str, size: usize, make: &dyn Fn(usize) -> String| {
            assert_linear(name, size, make, |text| drop(marks(text)));
        };
        linear("nested lists", 12_500, &deep);
        linear("nested objects", 6_250, &objects);
        linear("numbers", 125_000, &numbers);
        linear("escapes everywhere", 125_000, &|count| "\"\\".repeat(count));
        linear("quotes that never close", 250_000, &|count| {
            "\"".repeat(count)
        });
    }
}
