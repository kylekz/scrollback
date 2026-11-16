//! A prompt's or a reply's prose, prepared to draw (PLAN D11): cut into pieces of bounded size
//! the list lays out one a row, so that a long reply is virtualised like the rest of the
//! session, and so that nothing the stack's Markdown view can't lay out quickly, or at all,
//! reaches it.
//!
//! A body is first scanned without parsing ([`regions`]): the Markdown parser recurses once per
//! level of nesting and takes time quadratic in nesting and in a paragraph's delimiters, so a
//! paragraph past [`MAX_NESTING`], and what follows once [`DELIMITER_COST`] is spent, is never
//! parsed and is shown as plain text, as is a body past [`MARKDOWN_BYTES`]. The rest is parsed
//! on a thread of its own with a stack of [`PARSE_STACK`], and its tree taken apart without
//! recursing.
//!
//! The parsed body is cut between its top-level blocks into pieces, each sanitised on its own:
//! blocks are grouped up to [`PIECE_BYTES`], a single block may be up to [`BLOCK_BYTES`]
//! ([`CODE_BYTES`] for code), and a reference link's definitions go with each piece that uses
//! them, up to [`DEFINITION_BYTES`]. What is past these is shown as its plain text instead, a
//! bounded number of lines a row: a code block past [`CODE_LINES`] or [`CODE_BYTES`], any other
//! block past [`BLOCK_BYTES`], with a line past [`LINE_BYTES`] or nested past [`MAX_NESTING`],
//! and a group whose definitions are past [`DEFINITION_BYTES`].
//!
//! The bounds come from D11's measurement of the view: about a millisecond a kilobyte to lay
//! out, so a piece costs a few milliseconds, once, as it scrolls in.

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;
use std::sync::Arc;

use markdown::mdast::Node;

use super::text::Text;
use crate::markdown::{Sanitized, options};

/// The most of a body grouped into one row as Markdown, when it is more than one block.
pub const PIECE_BYTES: usize = 8 * 1024;
/// A fenced or indented code block longer than these is shown as plain text.
pub const CODE_LINES: usize = 1_000;
pub const CODE_BYTES: usize = 64 * 1024;
/// Any other block longer than this, or with a line longer than [`LINE_BYTES`], likewise.
pub const BLOCK_BYTES: usize = 16 * 1024;
pub const LINE_BYTES: usize = 8 * 1024;
/// The most of reference definitions carried into one piece.
pub const DEFINITION_BYTES: usize = 8 * 1024;
/**
 * The deepest a body may nest (quotes in quotes, lists in lists, emphasis in emphasis): a tenth
 * of where the view's parser was seen to overflow a worker thread's stack (200 nested lists,
 * 300 nested quotes), and where the parser's time, quadratic in it, is still well under a
 * millisecond.
 */
pub const MAX_NESTING: usize = 32;
/**
 * The most a body's paragraphs may cost the parser, as the sum over its paragraphs of the
 * square of their delimiters (runs of `*`, `_`, `~`, backticks and `$`, and each `[` and `<`):
 * matching them is quadratic. 2,000 delimiters in one paragraph were measured at about 30 ms
 * on a release build.
 */
pub const DELIMITER_COST: u64 = 4_000_000;
/// A body longer than this is shown as plain text without being parsed.
pub const MARKDOWN_BYTES: usize = 1024 * 1024;
/// The stack the parser runs on: many times what [`MAX_NESTING`] needs.
pub const PARSE_STACK: usize = 16 * 1024 * 1024;

/// One row's worth of a body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    Markdown(Sanitized),
    /// Shown as plain text, a bounded number of lines a row; `oversized` when that is because
    /// the Markdown view can't be handed it, which the row says.
    Plain {
        text: Arc<Text>,
        oversized: bool,
    },
}

/// A body, prepared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prose {
    pub pieces: Vec<Piece>,
}

fn oversized(source: &str) -> Piece {
    Piece::Plain {
        text: Arc::new(Text::new(crate::hostile::bidi(source).into_owned())),
        oversized: true,
    }
}

impl Prose {
    /// Text that is shown as it is (a thought): plain, in one piece.
    pub fn plain(text: &str) -> Self {
        Self {
            pieces: vec![Piece::Plain {
                text: Arc::new(Text::new(crate::hostile::bidi(text).into_owned())),
                oversized: false,
            }],
        }
    }

    /// `text`, a body of Markdown, prepared (the module docs).
    pub fn markdown(text: &str) -> Self {
        let pieces = if text.len() > MARKDOWN_BYTES {
            None
        } else {
            std::thread::scope(|scope| {
                std::thread::Builder::new()
                    .name("prose".to_string())
                    .stack_size(PARSE_STACK)
                    .spawn_scoped(scope, || {
                        let mut pieces = Vec::new();
                        for (range, parse) in regions(text) {
                            let source = &text[range];
                            if !parse {
                                pieces.push(oversized(source.trim_end()));
                            } else {
                                pieces.extend(markdown_pieces(source)?);
                            }
                        }
                        Some(pieces)
                    })
                    .ok()?
                    .join()
                    .ok()?
            })
        };
        Self {
            pieces: pieces.unwrap_or_else(|| vec![oversized(text)]),
        }
    }
}

/// `text` parsed and cut into pieces; `None` if it doesn't parse.
fn markdown_pieces(text: &str) -> Option<Vec<Piece>> {
    let root = markdown::to_mdast(text, &options()).ok()?;
    let blocks: Vec<&Node> = root.children().into_iter().flatten().collect();
    let definitions: HashMap<&str, &str> = blocks
        .iter()
        .filter_map(|node| match node {
            Node::Definition(definition) => {
                Some((definition.identifier.as_str(), span(node, text)?))
            }
            _ => None,
        })
        .collect();
    let mut pieces = Vec::new();
    let flush = |group: &mut Option<Group>, pieces: &mut Vec<Piece>| {
        let Some(group) = group.take() else {
            return;
        };
        let source = &text[group.start..group.end];
        let carried: Vec<&str> = group
            .references
            .iter()
            .filter_map(|identifier| definitions.get(identifier).copied())
            .filter(|definition| !source.contains(definition))
            .collect();
        if carried.iter().map(|it| it.len() + 2).sum::<usize>() > DEFINITION_BYTES {
            pieces.push(oversized(source));
            return;
        }
        let mut source = source.to_string();
        for definition in carried {
            source.push_str("\n\n");
            source.push_str(definition);
        }
        pieces.push(Piece::Markdown(Sanitized::new(&source)));
    };
    let mut group: Option<Group> = None;
    for node in &blocks {
        let Some(position) = node.position() else {
            continue;
        };
        let (start, end) = (position.start.offset, position.end.offset);
        let Some(source) = text.get(start..end) else {
            continue;
        };
        if too_big(node, source) {
            flush(&mut group, &mut pieces);
            pieces.push(oversized(source));
            continue;
        }
        match &mut group {
            Some(open) if end - open.start <= PIECE_BYTES => {
                open.end = end;
                references(node, &mut open.references);
            }
            _ => {
                flush(&mut group, &mut pieces);
                let mut open = Group {
                    start,
                    end,
                    references: BTreeSet::new(),
                };
                references(node, &mut open.references);
                group = Some(open);
            }
        }
    }
    flush(&mut group, &mut pieces);
    dismantle(root);
    Some(pieces)
}

/// Top-level blocks being gathered into a piece: where they start and end in the body, and the
/// definitions they refer to.
struct Group<'a> {
    start: usize,
    end: usize,
    references: BTreeSet<&'a str>,
}

fn span<'a>(node: &Node, text: &'a str) -> Option<&'a str> {
    let at = node.position()?;
    text.get(at.start.offset..at.end.offset)
}

/// Drops a parsed tree a node at a time: dropping it whole recurses once per level.
fn dismantle(root: Node) {
    let mut stack = vec![root];
    while let Some(mut node) = stack.pop() {
        if let Some(children) = node.children_mut() {
            stack.append(children);
        }
    }
}

/**
 * `text` cut into the runs to parse and the runs not to, in order, by a scan line by line that
 * never parses: a paragraph (the lines up to a blank one; a fenced block is never cut) is not
 * parsed if a line of it nests past [`MAX_NESTING`] (its quote and list markers, and its
 * indentation before them at two columns a level), and once the paragraphs parsed would cost
 * past [`DELIMITER_COST`], nothing after is.
 */
fn regions(text: &str) -> Vec<(Range<usize>, bool)> {
    let mut regions: Vec<(Range<usize>, bool)> = Vec::new();
    let mut spent = false;
    let mut cost: u64 = 0;
    let mut finish = |range: Range<usize>, deep: bool, delimiters: u64| {
        let own = delimiters * delimiters;
        let parse = !deep && !spent && cost + own <= DELIMITER_COST;
        if parse {
            cost += own;
        } else if !deep {
            spent = true;
        }
        match regions.last_mut() {
            Some((last, was)) if *was == parse => last.end = range.end,
            _ => regions.push((range, parse)),
        }
    };
    let mut fence: Option<(char, usize)> = None;
    let (mut start, mut deep, mut delimiters) = (0, false, 0);
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        at += line.len();
        let indent: usize = line
            .chars()
            .map_while(|c| match c {
                ' ' => Some(1),
                '\t' => Some(4),
                _ => None,
            })
            .sum();
        let mut depth = indent / 2;
        let mut rest = line.trim_start_matches([' ', '\t']);
        while let Some(after) = rest.strip_prefix('>').or_else(|| list_marker(rest)) {
            depth += 1;
            rest = after.trim_start_matches([' ', '\t']);
        }
        if let Some((mark, length)) = fence {
            let run = rest.chars().take_while(|c| *c == mark).count();
            if run >= length && rest[run * mark.len_utf8()..].trim().is_empty() {
                fence = None;
            }
            continue;
        }
        deep |= depth > MAX_NESTING;
        for mark in ['`', '~'] {
            let run = rest.chars().take_while(|c| *c == mark).count();
            if run >= 3 {
                fence = Some((mark, run));
            }
        }
        if fence.is_some() {
            continue;
        }
        if rest.trim().is_empty() {
            finish(start..at, deep, delimiters);
            (start, deep, delimiters) = (at, false, 0);
            continue;
        }
        let mut previous = '\0';
        for c in rest.chars() {
            match c {
                '[' | '<' => delimiters += 1,
                '*' | '_' | '~' | '`' | '$' if c != previous => delimiters += 1,
                _ => {}
            }
            previous = c;
        }
    }
    if start < text.len() {
        finish(start..text.len(), deep, delimiters);
    }
    regions
}

/// What follows a list item's marker (`-`, `*`, `+`, `1.`, `1)`) at the start of `line`, if
/// it starts with one.
fn list_marker(line: &str) -> Option<&str> {
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    let after = match line.as_bytes().get(digits)? {
        b'.' | b')' if (1..=9).contains(&digits) => &line[digits + 1..],
        b'-' | b'*' | b'+' if digits == 0 => &line[1..],
        _ => return None,
    };
    (after.is_empty() || after.starts_with([' ', '\t'])).then_some(after)
}

/// Whether the top-level block `node`, whose source is `source`, must not be handed the view.
fn too_big(node: &Node, source: &str) -> bool {
    match node {
        Node::Code(_) | Node::Math(_) => {
            source.len() > CODE_BYTES || source.lines().count() > CODE_LINES
        }
        _ => {
            source.len() > BLOCK_BYTES
                || source.lines().any(|line| line.len() > LINE_BYTES)
                || nesting(node) > MAX_NESTING
        }
    }
}

/// How deep the nodes under `node` nest, counted without recursing.
fn nesting(node: &Node) -> usize {
    let mut deepest = 0;
    let mut stack = vec![(node, 1)];
    while let Some((node, depth)) = stack.pop() {
        deepest = deepest.max(depth);
        if depth > MAX_NESTING {
            break;
        }
        stack.extend(
            node.children()
                .into_iter()
                .flatten()
                .map(|child| (child, depth + 1)),
        );
    }
    deepest
}

/// Adds the identifiers of the definitions links and images under `node` refer to.
fn references<'a>(node: &'a Node, found: &mut BTreeSet<&'a str>) {
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        match node {
            Node::LinkReference(link) => {
                found.insert(&link.identifier);
            }
            Node::ImageReference(image) => {
                found.insert(&image.identifier);
            }
            _ => {}
        }
        stack.extend(node.children().into_iter().flatten());
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn kinds(prose: &Prose) -> Vec<&'static str> {
        prose
            .pieces
            .iter()
            .map(|piece| match piece {
                Piece::Markdown(_) => "markdown",
                Piece::Plain {
                    oversized: true, ..
                } => "plain",
                Piece::Plain { .. } => "text",
            })
            .collect()
    }

    #[test]
    fn a_long_reply_is_cut_between_its_blocks_and_nothing_is_lost() {
        let paragraphs: Vec<String> = (0..2_000)
            .map(|n| format!("Paragraph {n} of many."))
            .collect();
        let body = paragraphs.join("\n\n");
        let prose = Prose::markdown(&body);
        assert!(prose.pieces.len() > 1);
        let mut rejoined = String::new();
        for piece in &prose.pieces {
            let Piece::Markdown(markdown) = piece else {
                panic!("short paragraphs are Markdown");
            };
            assert!(markdown.as_str().len() <= PIECE_BYTES);
            rejoined.push_str(markdown.as_str());
            rejoined.push_str("\n\n");
        }
        assert_eq!(rejoined.trim_end(), body);
    }

    #[test]
    fn what_the_view_cant_be_handed_is_plain_text_and_the_rest_stays_markdown() {
        let deep = ">".repeat(300) + " deep";
        let lists: String = (0..200)
            .map(|n| format!("{}- level\n", "  ".repeat(n)))
            .collect();
        let fence = format!("```\n{}```", "x\n".repeat(CODE_LINES + 1));
        let line = "word".repeat(LINE_BYTES / 4 + 1);
        for (body, expected) in [
            (
                format!("Before.\n\n{deep}\n\nAfter."),
                vec!["markdown", "plain", "markdown"],
            ),
            (lists.clone(), vec!["plain"]),
            (format!("Intro.\n\n{fence}"), vec!["markdown", "plain"]),
            (line.clone(), vec!["plain"]),
            ("x".repeat(MARKDOWN_BYTES + 1), vec!["plain"]),
        ] {
            assert_eq!(kinds(&Prose::markdown(&body)), expected);
        }
        let Piece::Plain { text, .. } = &Prose::markdown(&line).pieces[0] else {
            panic!("a long line is plain");
        };
        assert_eq!(text.text, line);
        // A moderate nesting is Markdown.
        let nested: String = (0..8)
            .map(|n| format!("{}- level\n", "  ".repeat(n)))
            .collect();
        assert_eq!(kinds(&Prose::markdown(&nested)), ["markdown"]);
    }

    #[test]
    fn a_reference_link_takes_its_definition_to_the_piece_that_uses_it() {
        let filler = "a".repeat(PIECE_BYTES - 10);
        let body = format!(
            "See [the docs][docs].\n\n{filler}\n\nAgain [the docs][docs].\n\n[docs]: https://example.com/docs"
        );
        let prose = Prose::markdown(&body);
        let markdown: Vec<&str> = prose
            .pieces
            .iter()
            .filter_map(|piece| match piece {
                Piece::Markdown(markdown) => Some(markdown.as_str()),
                Piece::Plain { .. } => None,
            })
            .collect();
        assert!(markdown.len() > 1);
        for piece in markdown.iter().filter(|piece| piece.contains("[docs].")) {
            assert!(
                piece.contains("[docs]: https://example.com/docs"),
                "{piece}"
            );
        }
    }

    /// `prepare` run on a thread with the stack a worker thread has on macOS.
    fn on_a_worker_stack(body: String) -> (Prose, std::time::Duration) {
        std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(move || {
                let start = std::time::Instant::now();
                let prose = Prose::markdown(&body);
                (prose, start.elapsed())
            })
            .unwrap()
            .join()
            .unwrap()
    }

    #[test]
    fn no_body_overflows_a_worker_stack_or_keeps_the_parser_long() {
        for body in [
            "- ".repeat(2_000) + "x",
            ">".repeat(200_000),
            "- ".repeat(100_000) + "x",
            "1. ".repeat(100_000) + "x",
            "[".repeat(200_000),
            "*a".repeat(200_000),
            "[a](".repeat(50_000),
            "`".repeat(300_000),
            "<a ".repeat(100_000),
            "![".repeat(100_000),
            "*a ".repeat(16_000) + &"a*".repeat(16_000),
            (0..200)
                .map(|n| format!("{}- level\n", "  ".repeat(n)))
                .collect(),
        ] {
            let start: String = body.chars().take(12).collect();
            let (prose, took) = on_a_worker_stack(body);
            assert!(!prose.pieces.is_empty(), "{start:?}");
            /* Unoptimised, as tests run: about ten times what a release build takes. */
            assert!(took.as_secs_f64() < 2.0, "{start:?} took {took:?}");
        }
    }

    #[test]
    fn carried_definitions_are_bounded_and_found_in_one_pass() {
        let sizes = |prose: &Prose| -> (usize, usize) {
            let markdown = prose.pieces.iter().filter_map(|piece| match piece {
                Piece::Markdown(markdown) => Some(markdown.as_str().len()),
                Piece::Plain { .. } => None,
            });
            markdown.fold((0, 0), |(most, all), len| (most.max(len), all + len))
        };
        /* Forty paragraphs that each refer to 1,300 of 2,000 long definitions: past what the
        parser may spend, so plain text after the first few. */
        /* Each a paragraph of its own, as the parser may read all of them. */
        let definitions: String = (0..2_000)
            .map(|n| {
                format!(
                    "[d{n}]: https://example.com/{n} \"{}\"\n\n",
                    "t".repeat(200)
                )
            })
            .collect();
        let references: String = (0..1_300).map(|n| format!("[d{n}] ")).collect();
        let paragraph = &references[..references.len().min(7_900)];
        let body = format!("{}\n{definitions}", format!("{paragraph}\n\n").repeat(40));
        let (prose, took) = on_a_worker_stack(body.clone());
        let (most, all) = sizes(&prose);
        assert!(most <= BLOCK_BYTES + DEFINITION_BYTES, "{most}");
        assert!(all <= body.len() * 2, "{all} of {}", body.len());
        assert!(took.as_secs_f64() < 2.0, "{took:?}");
        /* Forty short paragraphs, each referring to forty definitions of its own: a piece of
        them would carry more definitions than it may, so it is plain text. */
        let body = format!(
            "{}\n{definitions}",
            (0..40)
                .map(|p| (0..40)
                    .map(|n| format!("[d{}] ", p * 40 + n))
                    .collect::<String>())
                .collect::<Vec<_>>()
                .join("\n\n")
        );
        let (prose, took) = on_a_worker_stack(body.clone());
        let (most, all) = sizes(&prose);
        assert!(most <= PIECE_BYTES + DEFINITION_BYTES, "{most}");
        assert!(all <= body.len() * 2, "{all} of {}", body.len());
        assert!(took.as_secs_f64() < 2.0, "{took:?}");

        /* Forty thousand definitions and one reference. */
        let definitions: String = (0..40_000).map(|n| format!("[d{n}]: /x\n")).collect();
        let (prose, took) = on_a_worker_stack(format!("para [d1]\n\n{definitions}"));
        assert!(!prose.pieces.is_empty());
        assert!(took.as_secs_f64() < 2.0, "{took:?}");
    }

    #[test]
    fn a_body_is_sanitised_and_its_controls_shown() {
        let prose = Prose::markdown("![x](https://example.com/p.png) \u{202E}txt.exe");
        let Piece::Markdown(markdown) = &prose.pieces[0] else {
            panic!("Markdown");
        };
        assert_eq!(
            markdown.as_str(),
            "[image: x](<https://example.com/p.png>) ⟦U+202E⟧txt.exe"
        );
    }
}
