//! An edit's diff as a body draws it (DESIGN §4.2.4 "Edit", "Write"; PLAN §9): dv's diff rows,
//! unified, lifted per PLAN D15 from dv's `diff::intraline` (word-level marks for each removed
//! line paired with an added one) and its line model, with the line diff made here from the
//! call's own text, before and after, by the same `imara-diff`.
//!
//! Both texts are a transcript's, so hostile, and a naive diff is quadratic, by lines and by
//! words. So: lines are diffed only up to [`DIFF_LINES`] a side, past which the old text reads
//! as removed and the new as added; a pair of lines is diffed by words only when each is at most
//! [`WORD_LINE_BYTES`] and their token counts' product at most [`MAX_TOKEN_PRODUCT`], and the
//! diff stops marking words once it has spent [`WORD_BUDGET`] of such products.

use std::ops::Range;

use imara_diff::{Algorithm, Diff as Lines, InternedInput, TokenSource};

use crate::services::reader::text::Mark;

/// The most lines on a side diffed by lines (the module docs).
pub const DIFF_LINES: usize = 20_000;
/// The longest line diffed by words.
pub const WORD_LINE_BYTES: usize = 2 * 1024;
/// Above this product of two lines' token counts the pair isn't marked by words (dv's bound).
pub const MAX_TOKEN_PRODUCT: usize = 10_000;
/// The most token products a diff spends on words, all its pairs together.
pub const WORD_BUDGET: usize = 4_000_000;
/**
 * A pair where more than this fraction of either line's bytes changed reads as rewritten:
 * word-level marks would speckle nearly the whole line and hide that, so neither side is
 * marked (the row tint still shows the change). dv's.
 */
pub const MAX_CHANGED_FRACTION: f32 = 0.7;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

/// One line of a [`Diff`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    /// Its number before and after, from the first line of what changed; `None` on the side
    /// it isn't on.
    pub old: Option<u32>,
    pub new: Option<u32>,
    /// Its text, a range of [`Diff::text`], without its line ending.
    pub range: Range<usize>,
    /// What differs from the line it pairs with, ranges of [`Diff::text`].
    pub words: Vec<Range<usize>>,
}

/// An edit's text before and after, and its lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diff {
    /// The text before, a line break, and the text after: what the lines are ranges of.
    pub text: String,
    pub lines: Vec<DiffLine>,
    /// Syntax marks over [`Diff::text`], when its file's language is known.
    pub marks: Vec<(Range<usize>, Mark)>,
    /// The lines before and after it starts at.
    pub start: (u32, u32),
}

impl Diff {
    /// How many lines it removes and adds.
    pub fn counts(&self) -> (usize, usize) {
        let count = |kind| self.lines.iter().filter(|line| line.kind == kind).count();
        (count(LineKind::Removed), count(LineKind::Added))
    }
}

/// The lines of `text` from `offset` in the text they are part of, without their endings.
fn lines_of(text: &str, offset: usize) -> Vec<Range<usize>> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let shown = line.trim_end_matches('\n').trim_end_matches('\r');
        lines.push(offset + start..offset + start + shown.len());
        start += line.len();
    }
    lines
}

/// The diff from `old` to `new`, numbered from the lines `start` (the module docs).
pub fn diff(old: &str, new: &str, start: (u32, u32)) -> Diff {
    let text = format!("{old}\n{new}");
    let before = lines_of(old, 0);
    let after = lines_of(new, old.len() + 1);
    let mut lines = Vec::with_capacity(before.len() + after.len());
    let line = |kind, old: Option<usize>, new: Option<usize>, range: &Range<usize>| DiffLine {
        kind,
        old: old.map(|at| start.0.saturating_add(at as u32)),
        new: new.map(|at| start.1.saturating_add(at as u32)),
        range: range.clone(),
        words: Vec::new(),
    };
    if before.len() > DIFF_LINES || after.len() > DIFF_LINES {
        lines.extend(
            before
                .iter()
                .enumerate()
                .map(|(at, range)| line(LineKind::Removed, Some(at), None, range)),
        );
        lines.extend(
            after
                .iter()
                .enumerate()
                .map(|(at, range)| line(LineKind::Added, None, Some(at), range)),
        );
    } else {
        let slices = |ranges: &[Range<usize>]| -> Vec<&str> {
            ranges.iter().map(|range| &text[range.clone()]).collect()
        };
        let (old_lines, new_lines) = (slices(&before), slices(&after));
        let input = InternedInput::new(Tokens(&old_lines), Tokens(&new_lines));
        let changes = Lines::compute(Algorithm::Histogram, &input);
        let (mut a, mut b) = (0usize, 0usize);
        for hunk in changes.hunks() {
            let (from, to) = (hunk.before.start as usize, hunk.after.start as usize);
            while a < from && b < to {
                lines.push(line(LineKind::Context, Some(a), Some(b), &before[a]));
                (a, b) = (a + 1, b + 1);
            }
            for at in hunk.before.clone() {
                lines.push(line(
                    LineKind::Removed,
                    Some(at as usize),
                    None,
                    &before[at as usize],
                ));
            }
            for at in hunk.after.clone() {
                lines.push(line(
                    LineKind::Added,
                    None,
                    Some(at as usize),
                    &after[at as usize],
                ));
            }
            (a, b) = (hunk.before.end as usize, hunk.after.end as usize);
        }
        while a < before.len() && b < after.len() {
            lines.push(line(LineKind::Context, Some(a), Some(b), &before[a]));
            (a, b) = (a + 1, b + 1);
        }
        mark_words(&text, &mut lines);
    }
    Diff {
        text,
        lines,
        marks: Vec::new(),
        start,
    }
}

/**
 * Fills [`DiffLine::words`] for every removed line paired with an added one: within each run of
 * changed lines the i-th removed pairs with the i-th added, as on GitHub (dv's
 * `mark_intraline`), within [`WORD_BUDGET`].
 */
fn mark_words(text: &str, lines: &mut [DiffLine]) {
    let mut budget = WORD_BUDGET;
    let mut start = 0;
    while start < lines.len() {
        if lines[start].kind == LineKind::Context {
            start += 1;
            continue;
        }
        let end = lines[start..]
            .iter()
            .position(|line| line.kind == LineKind::Context)
            .map_or(lines.len(), |offset| start + offset);
        let of = |kind| {
            (start..end)
                .filter(|&at| lines[at].kind == kind)
                .collect::<Vec<_>>()
        };
        for (r, a) in of(LineKind::Removed).into_iter().zip(of(LineKind::Added)) {
            let (removed, added) = (lines[r].range.clone(), lines[a].range.clone());
            if removed.len() > WORD_LINE_BYTES || added.len() > WORD_LINE_BYTES {
                continue;
            }
            let (removed_words, added_words) =
                diff_pair(&text[removed.clone()], &text[added.clone()], &mut budget);
            let shift = |words: Vec<Range<usize>>, by: usize| {
                words
                    .into_iter()
                    .map(|word| word.start + by..word.end + by)
                    .collect()
            };
            lines[r].words = shift(removed_words, removed.start);
            lines[a].words = shift(added_words, added.start);
        }
        start = end;
    }
}

/**
 * The byte ranges that differ between a removed line and the added line paired with it: (the
 * removed line's, the added line's). Both are empty when the lines are identical, too long to
 * diff cheaply, mostly rewritten, or `budget` is spent. dv's `diff_pair`, with the budget.
 */
pub fn diff_pair(
    removed: &str,
    added: &str,
    budget: &mut usize,
) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let removed_tokens = tokenize(removed);
    let added_tokens = tokenize(added);
    let product = removed_tokens.len().saturating_mul(added_tokens.len());
    if product > MAX_TOKEN_PRODUCT || product > *budget {
        return (Vec::new(), Vec::new());
    }
    *budget -= product;

    let removed_strs: Vec<&str> = removed_tokens.iter().map(|r| &removed[r.clone()]).collect();
    let added_strs: Vec<&str> = added_tokens.iter().map(|r| &added[r.clone()]).collect();
    let input = InternedInput::new(Tokens(&removed_strs), Tokens(&added_strs));
    let diff = Lines::compute(Algorithm::Histogram, &input);

    let mut removed_ranges = Vec::new();
    let mut added_ranges = Vec::new();
    for hunk in diff.hunks() {
        let span = |tokens: &[Range<usize>], range: Range<u32>| {
            tokens[range.start as usize].start..tokens[range.end as usize - 1].end
        };
        if !hunk.before.is_empty() {
            push_merged(&mut removed_ranges, span(&removed_tokens, hunk.before));
        }
        if !hunk.after.is_empty() {
            push_merged(&mut added_ranges, span(&added_tokens, hunk.after));
        }
    }

    /* Trailing whitespace doesn't count towards a line's length, so padding can't sneak a
    rewritten line under the threshold. Either side crossing it unmarks both: a one-sided
    mark reads worse than none. */
    let rewritten = |ranges: &[Range<usize>], len: usize| {
        len > 0
            && ranges.iter().map(|r| r.len()).sum::<usize>() as f32 / len as f32
                > MAX_CHANGED_FRACTION
    };
    if rewritten(&removed_ranges, removed.trim_end().len())
        || rewritten(&added_ranges, added.trim_end().len())
    {
        return (Vec::new(), Vec::new());
    }
    (removed_ranges, added_ranges)
}

/// Appends `range`, merging it into the previous one when they touch.
fn push_merged(ranges: &mut Vec<Range<usize>>, range: Range<usize>) {
    if let Some(last) = ranges.last_mut()
        && range.start <= last.end
    {
        last.end = last.end.max(range.end);
        return;
    }
    ranges.push(range);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Space,
    Other,
}

fn classify(c: char) -> Class {
    if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else if c.is_whitespace() {
        Class::Space
    } else {
        Class::Other
    }
}

/// Splits `text` into byte ranges: runs of word characters, runs of whitespace, and one token
/// per other character (punctuation never runs together, so `((` is two tokens).
fn tokenize(text: &str) -> Vec<Range<usize>> {
    let mut tokens = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        let class = classify(c);
        let mut end = start + c.len_utf8();
        if class != Class::Other {
            while let Some(&(_, next)) = chars.peek() {
                if classify(next) != class {
                    break;
                }
                end += next.len_utf8();
                chars.next();
            }
        }
        tokens.push(start..end);
    }
    tokens
}

/// A pre-split token list as imara-diff input: a line's words, or a text's lines.
struct Tokens<'a>(&'a [&'a str]);

impl<'a> TokenSource for Tokens<'a> {
    type Token = &'a str;
    type Tokenizer = std::iter::Copied<std::slice::Iter<'a, &'a str>>;

    fn tokenize(&self) -> Self::Tokenizer {
        self.0.iter().copied()
    }

    fn estimate_tokens(&self) -> u32 {
        u32::try_from(self.0.len()).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use pretty_assertions::assert_eq;

    use super::*;

    fn shown(diff: &Diff) -> Vec<String> {
        diff.lines
            .iter()
            .map(|line| {
                let mut text = diff.text[line.range.clone()].to_string();
                for word in line.words.iter().rev() {
                    text.insert(word.end - line.range.start, ']');
                    text.insert(word.start - line.range.start, '[');
                }
                let mark = match line.kind {
                    LineKind::Context => ' ',
                    LineKind::Added => '+',
                    LineKind::Removed => '-',
                };
                format!("{:?} {:?} {mark}{text}", line.old, line.new)
            })
            .collect()
    }

    #[test]
    fn an_edit_is_its_lines_and_the_words_that_changed() {
        let diff = diff(
            "let list = match id {\n    Some(id) if id.starts_with(\"PL\") =>\n}",
            "let list = match id {\n    Some(id) if PREFIXES.iter().any(|p| id.starts_with(p)) =>\n    // mixes too\n}",
            (39, 41),
        );
        assert_eq!(
            shown(&diff),
            [
                "Some(39) Some(41)  let list = match id {",
                "Some(40) None -    Some(id) if id.starts_with([\"PL\"]) =>",
                "None Some(42) +    Some(id) if [PREFIXES.iter().any(|p| ]id.starts_with([p)]) =>",
                "None Some(43) +    // mixes too",
                "Some(41) Some(44)  }",
            ]
        );
        assert_eq!(diff.counts(), (1, 2));
        /* A new file: every line added; CRLF ends a line. */
        let written = super::diff("", "a\r\nb\n", (1, 1));
        assert_eq!(shown(&written), ["None Some(1) +a", "None Some(2) +b"]);
    }

    #[test]
    fn hostile_edits_are_diffed_in_bounded_time() {
        let started = Instant::now();
        /* One line of a megabyte each side: a line diff, and no words. */
        let old = "x ".repeat(512 * 1024);
        let new = format!("{old}y");
        let long = diff(&old, &new, (1, 1));
        assert_eq!(long.counts(), (1, 1));
        assert!(long.lines.iter().all(|line| line.words.is_empty()));
        /* Past the lines diffed: the old removed and the new added, as they are. */
        let many = "a\n".repeat(DIFF_LINES + 1);
        let others = "b\n".repeat(DIFF_LINES + 1);
        assert_eq!(
            diff(&many, &others, (1, 1)).counts(),
            (DIFF_LINES + 1, DIFF_LINES + 1)
        );
        /* As many lines as are diffed, nothing alike, then all alike but one in each. */
        let lines = |seed: usize| -> String {
            (0..DIFF_LINES)
                .map(|n| format!("line {} of {seed}\n", n * 7919 % 1009))
                .collect()
        };
        diff(&lines(1), &lines(2), (1, 1));
        let same = "same line\n".repeat(DIFF_LINES);
        diff(&format!("{same}x"), &format!("y{same}"), (1, 1));
        /* Lines paired by the thousand, each of many short words: the budget ends the words. */
        let words = |c: char| -> String {
            (0..DIFF_LINES)
                .map(|n| format!("{}{c}{n}\n", "w ".repeat(40)))
                .collect()
        };
        let paired = diff(&words('a'), &words('b'), (1, 1));
        let marked = |line: &DiffLine| !line.words.is_empty();
        assert!(marked(&paired.lines[0]) && !marked(&paired.lines[DIFF_LINES - 1]));
        assert!(
            started.elapsed().as_secs_f64() < 20.0,
            "{:?}",
            started.elapsed()
        );
    }
}
