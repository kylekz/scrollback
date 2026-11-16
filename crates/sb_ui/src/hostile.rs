//! What is done to every string from a transcript before it is drawn: a transcript holds
//! whatever the agent saw, and a string from it must not draw as something it isn't.
//!
//! - **Bidirectional controls** (the embeddings, overrides and isolates, U+202A–202E and
//!   U+2066–2069) reorder what follows them, so a path or a command could read reversed. They
//!   are shown as what they are, `⟦U+202E⟧`. Right-to-left text needs none of them to read
//!   right, and the marks (U+200E, U+200F) only nudge neutral characters, so both stay.
//! - **A one-line label** ([`label`]: a title, a row's target, a tool's name, a model's, a
//!   note's kind, a caption) is one line: every line break Unicode has (U+2028, U+2029, NEL,
//!   vertical tab and form feed with the usual ones), tabs and other controls become spaces, a
//!   stack of combining marks is cut short, and it is cut at a bound, since a row clips it
//!   anyway. Every string from a transcript drawn outside prose goes through it.

use std::borrow::Cow;

use unicode_properties::{GeneralCategory, UnicodeGeneralCategory as _};

/// The longest label drawn, in characters: past what any row can show.
pub const LABEL_CHARS: usize = 300;
/// The most combining marks kept on one character: more than any script stacks, and few
/// enough that a stack can't spill over the rows above and below.
pub const COMBINING_MARKS: usize = 4;

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// `text` with every bidirectional control shown as its code point.
pub fn bidi(text: &str) -> Cow<'_, str> {
    if !text.chars().any(is_bidi_control) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        if is_bidi_control(c) {
            out.push_str(&format!("⟦U+{:04X}⟧", c as u32));
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// Whether `c` is a combining mark: one drawn over or around the character before it.
fn is_combining(c: char) -> bool {
    matches!(
        c.general_category(),
        GeneralCategory::NonspacingMark | GeneralCategory::EnclosingMark
    )
}

/// `text` as one line of at most [`LABEL_CHARS`] characters, its controls shown (the module
/// docs): transcript text drawn outside prose.
pub fn label(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(LABEL_CHARS * 4));
    let (mut kept, mut marks) = (0, 0);
    for c in text.chars() {
        if kept == LABEL_CHARS {
            out.push('…');
            break;
        }
        if is_combining(c) {
            marks += 1;
            if marks > COMBINING_MARKS {
                continue;
            }
        } else {
            marks = 0;
        }
        kept += 1;
        match c {
            c if is_bidi_control(c) => out.push_str(&format!("⟦U+{:04X}⟧", c as u32)),
            '\u{2028}' | '\u{2029}' => out.push(' '),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn a_control_that_reorders_text_is_shown_and_right_to_left_text_is_left_alone() {
        assert_eq!(bidi("rm -rf \u{202E}gpj.exe"), "rm -rf ⟦U+202E⟧gpj.exe");
        assert_eq!(bidi("a\u{2066}b\u{2069}"), "a⟦U+2066⟧b⟦U+2069⟧");
        let hebrew = "שלום \u{200F}עולם";
        assert!(matches!(bidi(hebrew), Cow::Borrowed(text) if text == hebrew));
    }

    #[test]
    fn a_label_is_one_bounded_line() {
        assert_eq!(label("cargo test\nrm -rf /\tx"), "cargo test rm -rf / x");
        let long = label(&"é".repeat(1000));
        assert_eq!(long.chars().count(), LABEL_CHARS + 1);
        assert!(long.ends_with('…'));
        assert_eq!(label("\u{202E}x"), "⟦U+202E⟧x");
        assert_eq!(
            label("a\u{2028}b\u{2029}c\u{85}d\u{b}e\u{c}f\u{0}g"),
            "a b c d e f g"
        );
        for mark in ['\u{0336}', '\u{0489}', '\u{20DD}'] {
            let stacked = label(&format!("Z{}a", mark.to_string().repeat(200)));
            assert_eq!(stacked.chars().count(), 1 + COMBINING_MARKS + 1, "{mark:?}");
        }
    }
}
