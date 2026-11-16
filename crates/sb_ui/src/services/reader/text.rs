//! Text shown as it is, a bounded number of lines a row: a call's input and output, a record
//! as recorded, a thought, and prose too large to lay out as Markdown.

use std::ops::Range;

/**
 * The longest line drawn as one: a longer one is shown as several, each wrapping in its row,
 * so that no row has to lay out an unbounded line (a minified file is one line of megabytes).
 */
pub const SEGMENT_BYTES: usize = 512;

/// Text, whole, and cut into the lines drawn: its own lines, each long one in segments of at
/// most [`SEGMENT_BYTES`], cut at a space near the end where there is one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Text {
    pub text: String,
    lines: Vec<Range<usize>>,
}

impl Text {
    pub fn new(text: String) -> Self {
        let mut lines = Vec::new();
        let mut start = 0;
        for line in text.split_inclusive('\n') {
            let shown = line.trim_end_matches(['\n', '\r']);
            let mut at = 0;
            loop {
                let rest = &shown[at..];
                if rest.len() <= SEGMENT_BYTES {
                    lines.push(start + at..start + shown.len());
                    break;
                }
                let mut end = boundary(rest, SEGMENT_BYTES);
                let half = boundary(rest, SEGMENT_BYTES / 2);
                if let Some(space) = rest[half..end].rfind(' ') {
                    end = half + space + 1;
                }
                lines.push(start + at..start + at + end);
                at += end;
            }
            start += line.len();
        }
        Self { text, lines }
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// The lines `range`, joined: what a row draws of them.
    pub fn lines(&self, range: Range<usize>) -> String {
        let end = range.end.min(self.lines.len());
        self.lines[range.start.min(end)..end]
            .iter()
            .map(|line| &self.text[line.clone()])
            .collect::<Vec<_>>()
            .join("\n")
    }
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
}
