//! Text shown as it is, a bounded number of lines a row: a call's input and output, a record
//! as recorded, a thought, and prose too large to lay out as Markdown.

use std::borrow::Cow;
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
    /// When `text` is only the head of something longer: how many bytes more there are, as
    /// stored (an escape counts as written).
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

    /// The lines `range`, joined: what a row draws of them ([`expand_tabs`] last). The segments
    /// of one long line are joined without a break, up to [`JOINED_BYTES`] at a time.
    pub fn lines(&self, range: Range<usize>) -> String {
        let end = range.end.min(self.count);
        let start = range.start.min(end);
        let Some(&noted) = self.notes.get(start / NOTE_EVERY) else {
            return String::new();
        };
        let mut out = String::new();
        let mut at = noted;
        /* Whether the segment before was cut from the middle of its line: the next goes on
        from it with no break, so the row wraps the line where it is wide enough to, up to
        JOINED_BYTES of a line, where it breaks so that no line drawn is longer. */
        let mut cut = false;
        let mut joined = 0;
        for line in start / NOTE_EVERY * NOTE_EVERY..end {
            let (shown, next) = segment(&self.text, at);
            if line >= start {
                if line > start && (!cut || joined + shown.len() > JOINED_BYTES) {
                    out.push('\n');
                    joined = 0;
                }
                joined += shown.len();
                out.push_str(&self.text[shown.clone()]);
            }
            cut = next == shown.end && next < self.text.len();
            at = next;
        }
        out
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

/**
 * The most of one long line drawn as one: its segments are joined to this, then broken, so a
 * line of megabytes wraps in its row as one and no line laid out is longer than this.
 */
pub const JOINED_BYTES: usize = 4 * 1024;

/// The columns a tab stops at: a multiple of this. A tab is drawn as the spaces to the next.
pub const TAB_STOP: usize = 4;

/**
 * `lines` as drawn: each tab as the spaces to the next [`TAB_STOP`] of its line. Done last,
 * after anything else drawn into the lines (a bidirectional control's marker), so that the
 * stops count what is drawn.
 */
pub fn expand_tabs(lines: &str) -> Cow<'_, str> {
    if !lines.contains('\t') {
        return Cow::Borrowed(lines);
    }
    let mut out = String::with_capacity(lines.len() + 16);
    let mut column = 0;
    for c in lines.chars() {
        match c {
            '\t' => {
                let spaces = TAB_STOP - column % TAB_STOP;
                out.extend(std::iter::repeat_n(' ', spaces));
                column += spaces;
            }
            '\n' => {
                out.push(c);
                column = 0;
            }
            c => {
                out.push(c);
                column += 1;
            }
        }
    }
    Cow::Owned(out)
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
        /* A tab is drawn to the next stop, as a numbered line of a file reads. */
        let numbered = Text::new("    1\tfn main() {}\nab\tc".to_string());
        assert_eq!(
            expand_tabs(&numbered.lines(0..2)),
            "    1   fn main() {}\nab  c"
        );
        /* Counted after what else is drawn: a marker before a tab moves its stop. */
        assert_eq!(expand_tabs("⟦U+202E⟧\tx"), "⟦U+202E⟧    x");

        /* Ten megabytes of newlines: ten million lines, noted in a few megabytes. */
        let newlines = Text::new("\n".repeat(10_000_000));
        assert_eq!(newlines.line_count(), 10_000_000);
        assert!(newlines.notes.len() * size_of::<usize>() < 2 * 1024 * 1024);
        assert_eq!(newlines.lines(9_999_998..10_000_000), "\n");

        /* One line of five megabytes: each segment looks only as far as it can reach. */
        let start = std::time::Instant::now();
        let long = Text::new("word ".repeat(1_000_000));
        /* Its segments read on from each other, broken every JOINED_BYTES or so. */
        let drawn = long.lines(0..long.line_count());
        assert!(drawn.lines().all(|line| line.len() <= JOINED_BYTES));
        assert_eq!(drawn.replace('\n', "").len(), 5_000_000);
        assert!(start.elapsed().as_secs_f64() < 2.0, "{:?}", start.elapsed());
    }
}
