//! What is done to every string from a transcript before it is drawn: a transcript holds
//! whatever the agent saw, and a string from it must not draw as something it isn't.
//!
//! - **Bidirectional controls** (the embeddings, overrides and isolates, U+202A–202E and
//!   U+2066–2069, and the marks, U+200E, U+200F and U+061C) reorder what is around them, so a
//!   path or a command could read reversed. They are shown as what they are, `⟦U+202E⟧`, all
//!   alike. Right-to-left text reads right without them.
//! - **Text with nothing to show** (only controls, invisible formatting, spaces and the
//!   characters that draw as nothing) says so
//!   ([`nothing_shown`]) rather than drawing as a blank.
//! - **A one-line label** ([`label`]: a title, a row's target, a tool's name, a model's, a
//!   note's kind, a caption) is one line: every line break Unicode has (U+2028, U+2029, NEL,
//!   vertical tab and form feed with the usual ones), tabs and other controls become spaces, a
//!   stack of combining marks is cut short, and it is cut at a bound, since a row clips it
//!   anyway. Every string from a transcript drawn outside prose goes through it. Shown whole
//!   on hover it is cut at a far larger bound and says how much more there is ([`whole`]);
//!   copied, it is not cut at all ([`one_line`]), so that it pastes as it reads.

use std::borrow::Cow;
use std::ops::Range;

use unicode_properties::{GeneralCategory, UnicodeGeneralCategory as _};

/// The longest label drawn, in characters: past what any row can show.
pub const LABEL_CHARS: usize = 300;
/// The longest label shown whole on hover ([`whole`]), in characters: a path or a branch
/// longer than any real one.
pub const WHOLE_CHARS: usize = 4096;
/// The most combining marks kept on one character: more than any script stacks, and few
/// enough that a stack can't spill over the rows above and below.
pub const COMBINING_MARKS: usize = 4;

pub fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}'
    )
}

/**
 * What to show for `text` when none of it would draw (it is all controls, invisible
 * formatting and spaces, and isn't empty): how many characters it is.
 */
pub fn nothing_shown(text: &str) -> Option<String> {
    /* Spaces, controls and formatting, and the letters and symbols that draw as nothing
    (the Hangul fillers, the blank Braille pattern). */
    let invisible = |c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '\u{115F}' | '\u{1160}' | '\u{3164}' | '\u{FFA0}' | '\u{2800}'
            )
            || matches!(
                c.general_category(),
                GeneralCategory::Control | GeneralCategory::Format
            )
    };
    (!text.is_empty() && text.chars().all(invisible)).then(|| {
        let count = text.chars().count();
        format!(
            "({count} blank {})",
            if count == 1 {
                "character"
            } else {
                "characters"
            }
        )
    })
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
    labelled(text, Some(LABEL_CHARS), |_, _| {}).0
}

/// `text` as one line, as [`label`] draws it but of any length: what Copy puts on the
/// clipboard for a value drawn as a label, so that it pastes as it reads.
pub fn one_line(text: &str) -> String {
    labelled(text, None, |_, _| {}).0
}

/// `text` as one line of at most [`WHOLE_CHARS`] characters, then how many more there are:
/// a label shown whole on hover.
pub fn whole(text: &str) -> String {
    let (mut out, cut) = labelled(text, Some(WHOLE_CHARS), |_, _| {});
    if let Some(at) = cut {
        out.push_str(&format!(" {} more", text[at..].chars().count()));
    }
    out
}

/**
 * [`label`] of `text`, and `ranges` of `text` (on its character boundaries) as they fall in the
 * label: what a hit marks is found in the label of the whole, with the combining marks and the
 * cut counted across it. A range in what the label cuts off runs to its end.
 */
pub fn label_ranges(text: &str, ranges: &[Range<usize>]) -> (String, Vec<Range<usize>>) {
    let mut places = Vec::new();
    let (label, _) = labelled(text, Some(LABEL_CHARS), |input, output| {
        places.push((input, output))
    });
    let place = |at: usize| {
        (places.binary_search_by_key(&at, |(input, _)| *input))
            .map_or(label.len(), |found| places[found].1)
    };
    let ranges = (ranges.iter())
        .map(|range| place(range.start)..place(range.end))
        .collect();
    (label, ranges)
}

/**
 * [`label`] with at most `cap` characters (any number without), telling `place` where each
 * character of `text` it reaches starts in the label; with where in `text` it was cut, if it
 * was.
 */
fn labelled(
    text: &str,
    cap: Option<usize>,
    mut place: impl FnMut(usize, usize),
) -> (String, Option<usize>) {
    let mut out = String::with_capacity(text.len().min(cap.unwrap_or(usize::MAX / 4) * 4));
    let (mut kept, mut marks) = (0, 0);
    for (at, c) in text.char_indices() {
        place(at, out.len());
        if Some(kept) == cap {
            out.push('…');
            return (out, Some(at));
        }
        if is_combining(c) {
            marks += 1;
            if marks > COMBINING_MARKS {
                continue;
            }
        } else if c.general_category() == GeneralCategory::Format {
            /* A joiner between marks doesn't end their stack; past the cap it goes with them. */
            if marks >= COMBINING_MARKS {
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
    (out, None)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn ranges_follow_their_text_into_the_label() {
        let text = "a\u{202E}b\nhit";
        let (label, ranges) = label_ranges(text, &[1..5, 6..9]);
        assert_eq!(label, "a⟦U+202E⟧b hit");
        assert_eq!(&label[ranges[0].clone()], "⟦U+202E⟧b");
        assert_eq!(&label[ranges[1].clone()], "hit");
        /* The stack of marks is counted across a hit's edge, as in the label of the whole. */
        let stacked = format!("e{}", "\u{301}".repeat(6));
        let hit = 3..stacked.len();
        let (label, ranges) = label_ranges(&stacked, std::slice::from_ref(&hit));
        assert_eq!(label.chars().count(), 1 + COMBINING_MARKS);
        assert_eq!(ranges[0].end, label.len());
    }

    #[test]
    fn a_label_copied_or_shown_whole_is_one_line_as_it_reads() {
        /* A branch that would paste as commands pastes as one line. */
        assert_eq!(
            one_line("x\ncurl https://example.com/a|sh\n\u{202E}"),
            "x curl https://example.com/a|sh ⟦U+202E⟧"
        );
        let long = "a".repeat(LABEL_CHARS * 2);
        assert_eq!(one_line(&long), long);
        assert_eq!(whole(&long), long);
        let longer = "b".repeat(WHOLE_CHARS + 7);
        assert_eq!(
            whole(&longer),
            format!("{}… 7 more", "b".repeat(WHOLE_CHARS))
        );
    }

    #[test]
    fn a_control_that_reorders_text_is_shown_and_right_to_left_text_is_left_alone() {
        assert_eq!(bidi("rm -rf \u{202E}gpj.exe"), "rm -rf ⟦U+202E⟧gpj.exe");
        assert_eq!(bidi("a\u{2066}b\u{2069}"), "a⟦U+2066⟧b⟦U+2069⟧");
        let hebrew = "שלום עולם";
        assert!(matches!(bidi(hebrew), Cow::Borrowed(text) if text == hebrew));
        assert_eq!(
            bidi("a\u{200F}b\u{200E}c\u{061C}"),
            "a⟦U+200F⟧b⟦U+200E⟧c⟦U+061C⟧"
        );
        assert_eq!(
            nothing_shown("\u{0}\u{1}\u{200B} \n").as_deref(),
            Some("(5 blank characters)")
        );
        assert_eq!(nothing_shown(""), None);
        assert_eq!(
            nothing_shown("\u{3164}\u{2800}").as_deref(),
            Some("(2 blank characters)")
        );
        assert_eq!(nothing_shown("\u{0}a"), None);
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
        let joined = label(&format!("Z{}a", "\u{0336}\u{200D}".repeat(100)));
        assert_eq!(
            joined.chars().filter(|c| *c == '\u{0336}').count(),
            COMBINING_MARKS
        );
        for mark in ['\u{0336}', '\u{0489}', '\u{20DD}'] {
            let stacked = label(&format!("Z{}a", mark.to_string().repeat(200)));
            assert_eq!(stacked.chars().count(), 1 + COMBINING_MARKS + 1, "{mark:?}");
        }
    }
}
