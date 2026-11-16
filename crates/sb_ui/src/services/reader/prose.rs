//! A prompt's or a reply's prose, prepared to draw (PLAN D11): cut into pieces of bounded size
//! the list lays out one a row, so that a long reply is virtualised like the rest of the
//! session, and so that nothing the stack's Markdown view can't lay out quickly, or at all,
//! reaches it.
//!
//! The body is parsed as the view parses it and cut between its top-level blocks into pieces
//! of at most [`PIECE_BYTES`], each sanitised on its own; a reference link's definition goes
//! with each piece that uses it. A block the view must not be handed is shown as its plain text
//! instead, a bounded number of lines a row: a code block past [`CODE_LINES`] or
//! [`CODE_BYTES`], any other block past [`BLOCK_BYTES`] or with a line past [`LINE_BYTES`], and
//! one nested past [`MAX_NESTING`] (the view's parser recurses once per level, and overflows a
//! thread's stack at a few hundred). A body past [`MARKDOWN_BYTES`] isn't parsed at all.
//!
//! The bounds come from D11's measurement of the view: about a millisecond a kilobyte to lay
//! out, so a piece costs a few milliseconds, once, as it scrolls in.

use std::sync::Arc;

use markdown::mdast::Node;

use super::text::Text;
use crate::markdown::{Sanitized, options};

/// The most of a body one row lays out as Markdown.
pub const PIECE_BYTES: usize = 8 * 1024;
/// A fenced or indented code block longer than these is shown as plain text.
pub const CODE_LINES: usize = 1_000;
pub const CODE_BYTES: usize = 64 * 1024;
/// Any other block longer than this, or with a line longer than [`LINE_BYTES`], likewise.
pub const BLOCK_BYTES: usize = 16 * 1024;
pub const LINE_BYTES: usize = 8 * 1024;
/// The deepest a block's nodes may nest (quotes in quotes, lists in lists): a tenth of where
/// the view's parser was seen to overflow a worker thread's stack (200 nested lists, 300
/// nested quotes), so that it holds on the smallest stack it runs on.
pub const MAX_NESTING: usize = 32;
/// A body longer than this is shown as plain text without being parsed.
pub const MARKDOWN_BYTES: usize = 1024 * 1024;

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
        let oversized = |source: &str| Piece::Plain {
            text: Arc::new(Text::new(crate::hostile::bidi(source).into_owned())),
            oversized: true,
        };
        if text.len() > MARKDOWN_BYTES {
            return Self {
                pieces: vec![oversized(text)],
            };
        }
        let Ok(root) = markdown::to_mdast(text, &options()) else {
            return Self {
                pieces: vec![oversized(text)],
            };
        };
        let blocks: Vec<&Node> = root.children().into_iter().flatten().collect();
        let definitions: Vec<(&str, &str)> = blocks
            .iter()
            .filter_map(|node| match node {
                Node::Definition(definition) => {
                    Some((definition.identifier.as_str(), span(node, text)?))
                }
                _ => None,
            })
            .collect();
        let mut pieces = Vec::new();
        let mut group: Option<(usize, usize, Vec<&Node>)> = None;
        let flush = |group: &mut Option<(usize, usize, Vec<&Node>)>, pieces: &mut Vec<Piece>| {
            if let Some((start, end, nodes)) = group.take() {
                let mut source = text[start..end].to_string();
                for (identifier, definition) in &definitions {
                    if nodes.iter().any(|node| references(node, identifier))
                        && !source.contains(definition)
                    {
                        source.push_str("\n\n");
                        source.push_str(definition);
                    }
                }
                pieces.push(Piece::Markdown(Sanitized::new(&source)));
            }
        };
        for node in blocks {
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
                Some((first, last, nodes)) if end - *first <= PIECE_BYTES => {
                    *last = end;
                    nodes.push(node);
                }
                _ => {
                    flush(&mut group, &mut pieces);
                    group = Some((start, end, vec![node]));
                }
            }
        }
        flush(&mut group, &mut pieces);
        Self { pieces }
    }
}

fn span<'a>(node: &Node, text: &'a str) -> Option<&'a str> {
    let at = node.position()?;
    text.get(at.start.offset..at.end.offset)
}

/// Whether the view must not be handed the top-level block `node`, whose source is `source`.
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

/// Whether a link or image under `node` refers to the definition `identifier`.
fn references(node: &Node, identifier: &str) -> bool {
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        match node {
            Node::LinkReference(link) if link.identifier == identifier => return true,
            Node::ImageReference(image) if image.identifier == identifier => return true,
            other => stack.extend(other.children().into_iter().flatten()),
        }
    }
    false
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
