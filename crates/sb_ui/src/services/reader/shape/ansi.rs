//! A terminal's output as a body draws it (DESIGN §4.2.4 "Bash"): its colours kept as marks
//! ([`Ansi`]), every other escape and control dropped, and a carriage return taking the line
//! back to its start as a terminal's would (a progress bar drawn over itself is its last state).
//!
//! The output is a transcript's, so it is hostile. One pass over it, which looks ahead at most
//! [`ESCAPE_BYTES`] for an escape's end: an escape that hasn't ended by then is dropped up to
//! its introducer and what follows is read as text, so an unterminated or giant escape costs
//! no more than its bytes and hides nothing after it. The marks are at most [`MARKS`].

use std::ops::Range;

use crate::services::reader::text::{Ansi, Mark};

/// The furthest an escape is looked through for its end.
pub const ESCAPE_BYTES: usize = 256;
/// The most marks one output keeps; past them the rest is drawn in the text's own colour.
pub const MARKS: usize = 64 * 1024;

/// `raw`, a terminal's output, as drawn: the text, and its colours.
pub fn strip(raw: &str) -> (String, Vec<(Range<usize>, Mark)>) {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut marks: Vec<(Range<usize>, Mark)> = Vec::new();
    /* The colour asked for, and where in `out` it was asked for. */
    let mut colour: Option<(Ansi, usize)> = None;
    let mut line_start = 0;
    let mut at = 0;
    let close = |colour: &mut Option<(Ansi, usize)>, end: usize, marks: &mut Vec<_>| {
        if let Some((ansi, start)) = colour.take()
            && start < end
            && marks.len() < MARKS
        {
            marks.push((start..end, Mark::Ansi(ansi)));
        }
    };
    while at < bytes.len() {
        match bytes[at] {
            0x1b => {
                let (end, sgr) = escape(bytes, at);
                if let Some(params) = sgr {
                    let next = sgr_colour(params, colour.map(|(ansi, _)| ansi));
                    if next != colour.map(|(ansi, _)| ansi) {
                        close(&mut colour, out.len(), &mut marks);
                        colour = next.map(|ansi| (ansi, out.len()));
                    }
                }
                at = end;
            }
            b'\n' => {
                let carried = colour;
                close(&mut colour, out.len(), &mut marks);
                out.push('\n');
                line_start = out.len();
                colour = carried.map(|(ansi, _)| (ansi, out.len()));
                at += 1;
            }
            b'\r' if bytes.get(at + 1) == Some(&b'\n') => at += 1,
            b'\r' => {
                /* Back to the line's start: what was drawn on it goes, its colour stays. */
                out.truncate(line_start);
                while marks
                    .last()
                    .is_some_and(|(mark, _)| mark.start >= line_start)
                {
                    marks.pop();
                }
                if let Some((range, _)) = marks.last_mut() {
                    range.end = range.end.min(line_start);
                }
                colour = colour.map(|(ansi, _)| (ansi, line_start));
                at += 1;
            }
            b'\t' => {
                out.push('\t');
                at += 1;
            }
            byte if byte < 0x20 || byte == 0x7f => at += 1,
            _ => {
                let c = raw[at..].chars().next().unwrap_or('\u{fffd}');
                /* The eight-bit controls, C1, drawn as nothing. */
                if !('\u{80}'..='\u{9f}').contains(&c) {
                    out.push(c);
                }
                at += c.len_utf8();
            }
        }
    }
    close(&mut colour, out.len(), &mut marks);
    (out, marks)
}

/**
 * The escape at `at` (an ESC): where what follows it starts, and for a Select Graphic
 * Rendition (`ESC [ … m`) its parameters. An escape not ended within [`ESCAPE_BYTES`] is its
 * ESC and its introducer only.
 */
fn escape(bytes: &[u8], at: usize) -> (usize, Option<&[u8]>) {
    let reach = (at + ESCAPE_BYTES).min(bytes.len());
    match bytes.get(at + 1) {
        /* A control sequence: parameters and intermediates, then a final byte. */
        Some(b'[') => {
            let body = at + 2;
            match bytes[body.min(reach)..reach]
                .iter()
                .position(|byte| (0x40..=0x7e).contains(byte))
            {
                Some(end) => {
                    let end = body + end;
                    let sgr = (bytes[end] == b'm').then(|| &bytes[body..end]);
                    (end + 1, sgr)
                }
                None => (body.min(bytes.len()), None),
            }
        }
        /* An operating system command, and the strings like it: up to BEL or ST. */
        Some(b']' | b'P' | b'_' | b'^' | b'X') => {
            let body = at + 2;
            let found = bytes[body.min(reach)..reach]
                .iter()
                .enumerate()
                .find(|(offset, byte)| {
                    **byte == 0x07 || **byte == 0x1b && bytes.get(body + offset + 1) == Some(&b'\\')
                })
                .map(|(offset, byte)| body + offset + if *byte == 0x07 { 1 } else { 2 });
            (found.unwrap_or(body.min(bytes.len())), None)
        }
        /* Any other: ESC and the one byte after it, when that is ASCII. */
        Some(byte) if byte.is_ascii() => (at + 2, None),
        _ => (at + 1, None),
    }
}

/// The colour a Select Graphic Rendition with `params` leaves, from `colour`.
fn sgr_colour(params: &[u8], mut colour: Option<Ansi>) -> Option<Ansi> {
    let numbers: Vec<Option<u16>> = params
        .split(|byte| *byte == b';' || *byte == b':')
        .map(|digits| {
            std::str::from_utf8(digits)
                .ok()
                .and_then(|digits| digits.parse().ok())
        })
        .collect();
    let mut index = 0;
    while index < numbers.len() {
        match numbers[index].unwrap_or(0) {
            0 | 39 => colour = None,
            code @ (30..=37 | 90..=97) => colour = basic((code % 10) as u8),
            38 => match numbers.get(index + 1).copied().flatten() {
                /* One of 256: the first sixteen are the basic ones and the bright ones. */
                Some(5) => {
                    colour = numbers
                        .get(index + 2)
                        .copied()
                        .flatten()
                        .filter(|it| *it < 16)
                        .and_then(|it| basic((it % 8) as u8));
                    index += 2;
                }
                /* A colour of its own, which the theme has no place for. */
                Some(2) => {
                    colour = None;
                    index += 4;
                }
                _ => {}
            },
            /* Backgrounds and what else a rendition says are dropped, their arguments too. */
            48 | 58 => match numbers.get(index + 1).copied().flatten() {
                Some(5) => index += 2,
                Some(2) => index += 4,
                _ => {}
            },
            _ => {}
        }
        index += 1;
    }
    colour
}

/// The basic colour `index` (0 black … 7 white) as the theme draws it.
fn basic(index: u8) -> Option<Ansi> {
    match index {
        0 => Some(Ansi::Dim),
        1 => Some(Ansi::Red),
        2 => Some(Ansi::Green),
        3 => Some(Ansi::Yellow),
        4..=6 => Some(Ansi::Blue),
        7 => Some(Ansi::Bright),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use pretty_assertions::assert_eq;

    use super::*;

    fn coloured(raw: &str) -> (String, Vec<(String, Ansi)>) {
        let (text, marks) = strip(raw);
        let marks = marks
            .into_iter()
            .map(|(range, mark)| match mark {
                Mark::Ansi(ansi) => (text[range].to_string(), ansi),
                other => panic!("{other:?}"),
            })
            .collect();
        (text, marks)
    }

    #[test]
    fn colours_become_the_themes_and_everything_else_goes() {
        assert_eq!(
            coloured("\u{1b}[1;31merror\u{1b}[0m: \u{1b}[32mok\u{1b}[39m done"),
            (
                "error: ok done".to_string(),
                vec![
                    ("error".to_string(), Ansi::Red),
                    ("ok".to_string(), Ansi::Green)
                ]
            )
        );
        /* 256 colours: the first sixteen only; true colour and backgrounds dropped. */
        assert_eq!(
            coloured("\u{1b}[38;5;11mwarn\u{1b}[38;2;1;2;3mrgb\u{1b}[48;5;1mbg\u{1b}[m"),
            (
                "warnrgbbg".to_string(),
                vec![("warn".to_string(), Ansi::Yellow)]
            )
        );
        /* A colour goes on over a line's end. */
        assert_eq!(
            coloured("\u{1b}[34ma\nb\u{1b}[0m"),
            (
                "a\nb".to_string(),
                vec![("a".to_string(), Ansi::Blue), ("b".to_string(), Ansi::Blue)]
            )
        );
        /* Titles, hyperlinks, cursor moves, the bell and other controls are not drawn. */
        assert_eq!(
            coloured(
                "\u{1b}]0;title\u{7}\u{1b}]8;;https://x\u{1b}\\link\u{1b}]8;;\u{1b}\\\u{1b}[2K\u{1b}[1Gx\u{7}\u{0}\u{85}y"
            ),
            ("linkxy".to_string(), vec![])
        );
        /* A progress bar drawn over itself is its last state; CRLF is a line's end. */
        assert_eq!(
            coloured("10%\r\u{1b}[32m50%\r100%\u{1b}[0m\r\nnext"),
            (
                "100%\nnext".to_string(),
                vec![("100%".to_string(), Ansi::Green)]
            )
        );
    }

    #[test]
    fn an_escape_that_never_ends_hides_nothing_after_it_and_costs_its_bytes() {
        /* Unterminated: dropped to its introducer, the rest read as text. */
        assert_eq!(coloured("a\u{1b}[31"), ("a31".to_string(), vec![]));
        assert_eq!(
            coloured("a\u{1b}]0;title"),
            ("a0;title".to_string(), vec![])
        );
        assert_eq!(coloured("\u{1b}"), (String::new(), vec![]));
        assert_eq!(coloured("x\u{1b}é"), ("xé".to_string(), vec![]));
        /* A giant escape: parameters past the reach are text, read once. */
        let giant = format!("\u{1b}[{}mtail", "1;".repeat(4_000_000));
        let started = Instant::now();
        let (text, marks) = strip(&giant);
        assert!(text.ends_with("mtail") && marks.is_empty());
        /* Every byte an introducer that never ends. */
        let introducers = "\u{1b}[".repeat(2_000_000);
        let (text, _) = strip(&introducers);
        assert!(text.is_empty());
        let oscs = "\u{1b}]".repeat(2_000_000);
        strip(&oscs);
        /* Colours changed at every character: the marks stop at their bound. */
        let flicker = "\u{1b}[31ma\u{1b}[32mb".repeat(200_000);
        let (text, marks) = strip(&flicker);
        assert_eq!((text.len(), marks.len()), (400_000, MARKS));
        /* Carriage returns on one long line: each character goes once. */
        let returns = "abc\r".repeat(1_000_000);
        assert_eq!(strip(&returns).0, "");
        assert!(
            started.elapsed().as_secs_f64() < 5.0,
            "{:?}",
            started.elapsed()
        );
    }
}
