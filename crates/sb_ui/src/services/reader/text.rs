//! Text shown as it is, a bounded number of lines a row: a call's input and output, a record
//! as recorded, a thought, and prose too large to lay out as Markdown.

use std::ops::Range;

/**
 * The longest line drawn as one: a longer one is shown as several, each wrapping in its row,
 * so that no row has to lay out an unbounded line (a minified file is one line of megabytes).
 */
pub const SEGMENT_BYTES: usize = 512;

/**
 * How many lines drawn apart the text notes where one starts: a row's lines are found from the
 * note before them. Noting every line costs 16 bytes a line, as much as the text itself for
 * short lines; this costs a few bytes a hundred lines and a scan of at most this many.
 */
const NOTE_EVERY: usize = 64;

/// Text and the lines drawn of it: its own lines, each long one in segments of at most
/// [`SEGMENT_BYTES`], cut at a space near the end where there is one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Text {
    pub text: String,
    /// When `text` is only the head of something longer: about how many bytes more there are
    /// (as stored, so escapes count as written).
    pub rest: Option<u64>,
    /// Where every [`NOTE_EVERY`]th line drawn starts.
    notes: Vec<usize>,
    count: usize,
}

impl Text {
    pub fn new(text: String) -> Self {
        let mut notes = Vec::new();
        let mut count = 0;
        let mut at = Some(0);
        while let Some(start) = at.filter(|start| *start < text.len()) {
            if count % NOTE_EVERY == 0 {
                notes.push(start);
            }
            count += 1;
            at = Some(segment(&text, start).1);
        }
        Self {
            text,
            rest: None,
            notes,
            count,
        }
    }

    /// The first part of something `rest` bytes longer.
    pub fn head(text: String, rest: u64) -> Self {
        Self {
            rest: Some(rest),
            ..Self::new(text)
        }
    }

    pub fn line_count(&self) -> usize {
        self.count
    }

    /// The lines `range`, joined: what a row draws of them.
    pub fn lines(&self, range: Range<usize>) -> String {
        let end = range.end.min(self.count);
        let start = range.start.min(end);
        let Some(&noted) = self.notes.get(start / NOTE_EVERY) else {
            return String::new();
        };
        let mut lines = Vec::with_capacity(end - start);
        let mut at = noted;
        for line in start / NOTE_EVERY * NOTE_EVERY..end {
            let (shown, next) = segment(&self.text, at);
            if line >= start {
                lines.push(&self.text[shown]);
            }
            at = next;
        }
        lines.join("\n")
    }
}

/// The line drawn that starts at `start` in `text`, and where the next one starts.
fn segment(text: &str, start: usize) -> (Range<usize>, usize) {
    /* Only as far as a segment can reach: a line of megabytes is not searched to its end for
    each of its segments. */
    let reach = &text[start..boundary(text, (start + SEGMENT_BYTES + 2).min(text.len()))];
    let (end, next) = match reach.find('\n') {
        Some(newline) => (start + newline, start + newline + 1),
        None => (start + reach.len(), start + reach.len()),
    };
    let shown = text[start..end].trim_end_matches('\r');
    if shown.len() <= SEGMENT_BYTES {
        return (start..start + shown.len(), next);
    }
    let mut cut = boundary(shown, SEGMENT_BYTES);
    let half = boundary(shown, SEGMENT_BYTES / 2);
    if let Some(space) = shown[half..cut].rfind(' ') {
        cut = half + space + 1;
    }
    (start..start + cut, start + cut)
}

/// The start of the character at `at` in `text`, or `at` if one starts there.
fn boundary(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn a_long_line_is_drawn_as_segments_cut_at_a_character_and_a_space_where_there_is_one() {
        let long = "é".repeat(400);
        let text = Text::new(format!("one\r\ntwo\n{long}\n"));
        assert_eq!(text.lines(0..2), "one\ntwo");
        assert_eq!(text.line_count(), 4);
        assert_eq!(text.lines(2..4).replace('\n', ""), long);
        assert!(text.lines(2..3).len() <= SEGMENT_BYTES);

        let words = "word ".repeat(300);
        let text = Text::new(words.clone());
        assert!(text.lines(0..1).ends_with(' '));
        assert_eq!(text.lines(0..99).replace('\n', ""), words);
        assert_eq!(Text::new(String::new()).line_count(), 0);
        assert_eq!(text.lines(50..60), "");

        /* Characters of three bytes, the middle of a segment inside one of them. */
        for lead in 0..3 {
            let wide = format!("{}{}", "a".repeat(lead), "\u{200B}".repeat(400));
            assert_eq!(Text::new(wide.clone()).lines(0..9).replace('\n', ""), wide);
        }
    }

    #[test]
    fn any_lines_are_found_from_the_notes_and_cost_little_to_index() {
        let text: String = (0..1_000).map(|n| format!("line {n}\r\n\n")).collect();
        let text = Text::new(text);
        assert_eq!(text.line_count(), 2_000);
        assert_eq!(text.lines(130..133), "line 65\n\nline 66");
        assert_eq!(text.lines(1_998..2_005), "line 999\n");
        assert_eq!(text.lines(5..5), "");

        /* Ten megabytes of newlines: ten million lines, noted in a few megabytes. */
        let newlines = Text::new("\n".repeat(10_000_000));
        assert_eq!(newlines.line_count(), 10_000_000);
        assert!(newlines.notes.len() * size_of::<usize>() < 2 * 1024 * 1024);
        assert_eq!(newlines.lines(9_999_998..10_000_000), "\n");

        /* One line of five megabytes: each segment looks only as far as it can reach. */
        let start = std::time::Instant::now();
        let long = Text::new("word ".repeat(1_000_000));
        assert_eq!(
            long.lines(0..long.line_count()).len(),
            5_000_000 + long.line_count() - 1
        );
        assert!(start.elapsed().as_secs_f64() < 2.0, "{:?}", start.elapsed());
    }
}
