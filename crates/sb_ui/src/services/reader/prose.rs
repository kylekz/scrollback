//! A prompt's or a reply's prose, prepared to draw (PLAN D11): cut into pieces of bounded size
//! the list lays out one a row, so that a long reply is virtualised like the rest of the
//! session, and so that nothing the stack's Markdown view can't lay out quickly, or at all,
//! reaches it.
//!
//! What keeps the Markdown parser's time bounded is size and time, not knowing Markdown: the
//! parser is at least quadratic in many things (blocks, list markers, table cells, entities,
//! delimiters, nesting), and a scan that tried to tell them all apart would be bypassed by the
//! next one. So:
//!
//! - **No parse is larger than [`REGION_BYTES`].** A body is scanned line by line ([`scan`]) and
//!   cut at blank lines into regions of at most that, each parsed on its own; a paragraph
//!   larger than that alone is plain text.
//! - **Fenced code is never parsed.** What the scan takes for a fenced block is a piece of its
//!   own ([`Piece::Code`]), drawn by the reader's code view. Where the scan is wrong, the result
//!   looks wrong, and costs nothing.
//! - **A body's parses stop after [`PARSE_BUDGET`].** The time is checked before each region
//!   and each piece; once spent, the rest is plain text. So is a body whose session closed, or
//!   whose row was let go, meanwhile.
//! - **The view is handed only what it parses quickly.** Each piece of Markdown is sanitised
//!   and parsed as the view will parse it, timed ([`Sanitized::measured`]): one that took long is
//!   plain text.
//! - **Definitions are carried as the parser read them, never as they were written.** A
//!   reference link's definition may be in another region, or another piece: each definition
//!   in a region's tree is written out anew on a line of its own ([`definition`]), and goes to
//!   the pieces, and the regions, that refer to it, up to [`DEFINITION_BYTES`]; a region that
//!   refers to one made in another is parsed again with it.
//! - The parser recurses once per level of nesting, so it runs on a thread with a stack of
//!   [`PARSE_STACK`], and its tree is taken apart without recursing.
//!
//! The scan also picks plain text early for a paragraph nested past [`MAX_NESTING`] or with
//! more delimiters than [`DELIMITER_COST`] allows: those spare the parser work that would end
//! in plain text anyway (nested past [`MAX_NESTING`], a list that would take the parser
//! hundreds of milliseconds); what they miss is still bounded by the rest.
//!
//! A parsed region is cut between its top-level blocks into pieces, each sanitised on its own:
//! blocks are grouped up to [`PIECE_BYTES`], a single block may be up to [`BLOCK_BYTES`]
//! ([`CODE_BYTES`] for code). What is past these is shown as its plain text instead, a bounded
//! number of lines a row: a code block past [`CODE_LINES`] or [`CODE_BYTES`], any other block
//! past [`BLOCK_BYTES`], with a line past [`LINE_BYTES`] or nested past [`MAX_NESTING`], and a
//! group whose definitions are past [`DEFINITION_BYTES`].
//!
//! The bounds come from D11's measurement of the view: about a millisecond a kilobyte to lay
//! out, so a piece costs a few milliseconds, once, as it scrolls in.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
/// The most of reference definitions carried into one piece, or into one region.
pub const DEFINITION_BYTES: usize = 8 * 1024;
/**
 * The most of a body handed to the parser at once (the module docs). At this size the slowest
 * inputs found (ordered lists or quotes nested to the region's end, entities after an
 * autolink) cost the parse and the sanitiser under 200 ms on a release build, the rest well
 * under 100 ms (the tests' table); at 16 KiB the nested ones took over 700 ms. A paragraph or a
 * table larger than this is plain text.
 */
pub const REGION_BYTES: usize = 8 * 1024;
/// The most time a body's parses may take, all told; past it, the rest is plain text.
pub const PARSE_BUDGET: Duration = Duration::from_millis(100);
/**
 * The deepest a body may nest (quotes in quotes, lists in lists, emphasis in emphasis): a tenth
 * of where the view's parser was seen to overflow a worker thread's stack (200 nested lists,
 * 300 nested quotes). The scan's use of it only saves work.
 */
pub const MAX_NESTING: usize = 32;
/**
 * A paragraph's delimiters, squared, past which the scan shows it as plain text without asking
 * the parser: each `*`, `_`, `~`, `[` and `<` and each run of backticks or `$`. Not what bounds
 * the parser's time ([`REGION_BYTES`] and [`PARSE_BUDGET`] do); it spares parses of paragraphs
 * that would come out too deep to draw anyway.
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
    /// Fenced code, drawn by the reader's code view: its language as its fence names it.
    Code {
        language: Option<String>,
        text: Arc<Text>,
    },
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
        let note = crate::hostile::nothing_shown(text);
        let text = note.as_deref().unwrap_or(text);
        Self {
            pieces: vec![Piece::Plain {
                text: Arc::new(Text::new(crate::hostile::bidi(text).into_owned())),
                oversized: false,
            }],
        }
    }

    /// `text`, a body of Markdown, prepared (the module docs).
    pub fn markdown(text: &str) -> Self {
        Self::markdown_while(text, &|| true)
    }

    /**
     * `text` prepared while `wanted` says it still is: asked before each region and each piece,
     * so that a body of a session closed, or of a row let go, meanwhile goes no further (the
     * rest of it is plain text, and dropped).
     */
    pub fn markdown_while(text: &str, wanted: &(dyn Fn() -> bool + Sync)) -> Self {
        if let Some(note) = crate::hostile::nothing_shown(text) {
            return Self::plain(&note);
        }
        let pieces = if text.len() > MARKDOWN_BYTES {
            None
        } else {
            std::thread::scope(|scope| {
                std::thread::Builder::new()
                    .name("prose".to_string())
                    .stack_size(PARSE_STACK)
                    .spawn_scoped(scope, || prepare(text, wanted))
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

/// The pieces of `text` (the module docs); `None` if a region doesn't parse.
fn prepare(text: &str, wanted: &(dyn Fn() -> bool + Sync)) -> Option<Vec<Piece>> {
    let started = Instant::now();
    let going = || started.elapsed() < PARSE_BUDGET && wanted();
    /* Each region parsed on its own, and the definitions the body makes: the first of a label
    is the one Markdown follows. */
    let mut definitions: HashMap<String, String> = HashMap::new();
    let mut spans = Vec::new();
    for span in regions(scan(text)) {
        spans.push(match span {
            Span::Code { language, body } => Prepared::Piece(code(language, body)),
            Span::Prose(range) if going() => {
                let region = markdown_pieces(&text[range.clone()], "", &going)?;
                for (label, definition) in &region.definitions {
                    definitions
                        .entry(label.clone())
                        .or_insert_with(|| definition.clone());
                }
                Prepared::Region(range, region)
            }
            Span::Prose(range) | Span::Plain(range) => Prepared::Plain(range),
        });
    }
    /* Then each region that refers to a definition made in another, again with it. */
    for prepared in &mut spans {
        let Prepared::Region(range, region) = prepared else {
            continue;
        };
        let source = &text[range.clone()];
        let carried = carried(source, &region.definitions, &definitions);
        if !carried.is_empty() && going() {
            *region = markdown_pieces(source, &carried, &going)?;
        }
    }
    let mut pieces = Vec::new();
    let mut plain: Option<Range<usize>> = None;
    let flush_plain = |plain: &mut Option<Range<usize>>, pieces: &mut Vec<Piece>| {
        if let Some(range) = plain.take() {
            let source = text[range].trim_end();
            if !source.is_empty() {
                pieces.push(oversized(source));
            }
        }
    };
    for prepared in spans {
        match prepared {
            Prepared::Plain(range) => {
                plain = Some(match plain {
                    Some(open) => open.start..range.end,
                    None => range,
                });
            }
            Prepared::Piece(piece) => {
                flush_plain(&mut plain, &mut pieces);
                pieces.push(piece);
            }
            Prepared::Region(_, region) => {
                flush_plain(&mut plain, &mut pieces);
                pieces.extend(region.pieces);
            }
        }
    }
    flush_plain(&mut plain, &mut pieces);
    Some(pieces)
}

/// A run of a body, prepared: fenced code, a region parsed, or plain text.
enum Prepared {
    Piece(Piece),
    Region(Range<usize>, Region),
    Plain(Range<usize>),
}

/// A region parsed: its pieces, and the definitions it makes, by label, written out to be
/// carried ([`definition`]).
struct Region {
    pieces: Vec<Piece>,
    definitions: HashMap<String, String>,
}

/// A piece of fenced code; plain text when it is longer than a code block may be.
fn code(language: Option<String>, body: String) -> Piece {
    if body.len() > CODE_BYTES || body.lines().count() > CODE_LINES {
        return oversized(&body);
    }
    Piece::Code {
        language,
        text: Arc::new(Text::new(crate::hostile::bidi(&body).into_owned())),
    }
}

/**
 * `text`, a region, parsed with the definitions `carried` from elsewhere in the body after it,
 * and cut into pieces while `going` says the body's parses may go on: past that, each group of
 * blocks left is plain text. `None` if it doesn't parse.
 *
 * A region whose parse took longer than the view's parse of its pieces may
 * ([`crate::markdown::view_parse_limit`]) is plain text without them: a piece is most often the
 * whole of its region, parsed by the same parser, and would take as long again three times.
 */
fn markdown_pieces(text: &str, carried: &str, going: &dyn Fn() -> bool) -> Option<Region> {
    let source = format!("{text}\n\n{carried}");
    let started = Instant::now();
    let root = markdown::to_mdast(&source, &options()).ok()?;
    let slow = started.elapsed() > crate::markdown::view_parse_limit(source.len());
    /* Every definition, wherever it is (in a quote, in a list item), where it starts and
    written out once: the first of a label is the one Markdown follows. */
    let mut definitions: HashMap<&str, (usize, String)> = HashMap::new();
    let mut stack = vec![&root];
    while let Some(node) = stack.pop() {
        if let Node::Definition(found) = node
            && let Some(at) = node.position()
        {
            definitions
                .entry(found.identifier.as_str())
                .or_insert_with(|| (at.start.offset, definition(found)));
        }
        /* Children in order: the first definition is found first. */
        stack.extend(node.children().into_iter().flatten().rev());
    }
    /* The region's own blocks; those of the definitions carried after it make no piece. */
    let blocks: Vec<&Node> = root
        .children()
        .into_iter()
        .flatten()
        .filter(|node| {
            node.position()
                .is_some_and(|at| at.start.offset < text.len())
        })
        .collect();
    let mut pieces = Vec::new();
    let flush = |group: &mut Option<Group>, pieces: &mut Vec<Piece>| {
        let Some(group) = group.take() else {
            return;
        };
        let source = &text[group.start..group.end];
        if slow || !going() {
            pieces.push(oversized(source));
            return;
        }
        /* Those the group refers to and doesn't hold. */
        let carried: Vec<&str> = group
            .references
            .iter()
            .filter_map(|identifier| definitions.get(identifier))
            .filter(|(at, _)| !(group.start..group.end).contains(at))
            .map(|(_, definition)| definition.as_str())
            .collect();
        if carried.iter().map(|it| it.len() + 2).sum::<usize>() > DEFINITION_BYTES {
            pieces.push(oversized(source));
            return;
        }
        let mut markdown = source.to_string();
        for definition in carried {
            markdown.push_str("\n\n");
            markdown.push_str(definition);
        }
        pieces.push(
            Sanitized::measured(&markdown).map_or_else(|| oversized(source), Piece::Markdown),
        );
    };
    let mut group: Option<Group> = None;
    for node in &blocks {
        let Some(position) = node.position() else {
            continue;
        };
        let (start, end) = (position.start.offset, position.end.offset.min(text.len()));
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
    let definitions = definitions
        .into_iter()
        .filter(|(_, (at, _))| *at < text.len())
        .map(|(label, (_, definition))| (label.to_string(), definition))
        .collect();
    dismantle(root);
    Some(Region {
        pieces,
        definitions,
    })
}

/// Top-level blocks being gathered into a piece: where they start and end in the body, and the
/// definitions they refer to.
struct Group<'a> {
    start: usize,
    end: usize,
    references: BTreeSet<&'a str>,
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
 * A label as the parser matches it, and names it in its tree: its whitespace collapsed, its
 * case folded (`markdown`'s `normalize_identifier`, then lower case).
 */
fn identifier(label: &str) -> String {
    label
        .split([' ', '\t', '\n', '\r'])
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .to_uppercase()
        .to_lowercase()
}

/**
 * `found` written out to be carried into another piece or region: on a line of its own, its
 * label as the parser names it, its destination in angle brackets and its title in quotes, each
 * escaped, so that it reads back as the same definition wherever it is put, at the cost of a
 * definition's parse. What it was written as in the body (a destination on the next line, in a
 * quote) is never carried: that could read back as something else, and cost far more.
 */
fn definition(found: &markdown::mdast::Definition) -> String {
    let mut out = format!("[{}]: <", found.identifier);
    for c in found.url.chars() {
        match c {
            '\\' | '<' | '>' | '&' => {
                out.push('\\');
                out.push(c);
            }
            /* A line break can't be in a destination: as the parser would have it in a URL. */
            c if c.is_control() => {
                for byte in c.encode_utf8(&mut [0; 4]).bytes() {
                    out.push_str(&format!("%{byte:02X}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('>');
    if let Some(title) = &found.title {
        out.push_str(" \"");
        for c in title.chars() {
            match c {
                '\\' | '"' | '&' => {
                    out.push('\\');
                    out.push(c);
                }
                c if c.is_control() => out.push(' '),
                c => out.push(c),
            }
        }
        out.push('"');
    }
    out
}

/**
 * The labels `text` holds in brackets, as the parser matches them ([`identifier`]), each once in
 * the order they first appear: the definitions it may refer to. In one pass: a bracket opened
 * inside another starts again from the inner one.
 */
fn labels(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        rest = &rest[open + 1..];
        let Some(close) = rest.find(']') else {
            break;
        };
        let name = &rest[..close];
        if let Some(inner) = name.rfind('[') {
            rest = &rest[inner..];
            continue;
        }
        rest = &rest[close + 1..];
        if !name.is_empty() && name.len() <= 999 {
            let label = identifier(name);
            if seen.insert(label.clone()) {
                found.push(label);
            }
        }
    }
    found
}

/**
 * The definitions of `definitions` the region `text` may refer to and doesn't make itself
 * (`own`): those whose label is in brackets in it, one after another. At most
 * [`DEFINITION_BYTES`] of them.
 */
fn carried(
    text: &str,
    own: &HashMap<String, String>,
    definitions: &HashMap<String, String>,
) -> String {
    let mut out = String::new();
    for label in labels(text) {
        let Some(definition) = definitions
            .get(&label)
            .filter(|_| !own.contains_key(&label))
        else {
            continue;
        };
        if out.len() + definition.len() + 2 > DEFINITION_BYTES {
            break;
        }
        out.push_str(definition);
        out.push_str("\n\n");
    }
    out
}

/// What the scan finds a run of a body to be.
enum Span {
    /// Paragraphs to parse.
    Prose(Range<usize>),
    /// Fenced code: its language, and its lines with the fence and its containers left out.
    Code {
        language: Option<String>,
        body: String,
    },
    /// Paragraphs shown as plain text.
    Plain(Range<usize>),
}

/**
 * `spans` with each run of paragraphs to parse cut, at the paragraphs' ends, into regions of at
 * most [`REGION_BYTES`].
 */
fn regions(spans: Vec<Span>) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    for span in spans {
        match (out.last_mut(), span) {
            (Some(Span::Prose(open)), Span::Prose(next))
                if next.end - open.start <= REGION_BYTES =>
            {
                open.end = next.end;
            }
            (Some(Span::Plain(open)), Span::Plain(next)) => open.end = next.end,
            (_, span) => out.push(span),
        }
    }
    out
}

/**
 * `text` as runs of paragraphs, by a scan line by line that never parses. A paragraph is the
 * lines up to a blank one, to parse unless it is longer than [`REGION_BYTES`], a line of it
 * nests past [`MAX_NESTING`] as the parsed tree would ([`Prefix::depth`]), or its delimiters
 * cost past [`DELIMITER_COST`].
 *
 * A fence opens only as CommonMark opens one, and as narrowly: three or more backticks or
 * tildes at most three columns in from the line's last container marker, or, on a line of a
 * list item's content, from the column that content starts at; a backtick fence's info string
 * holding no backtick. The list items a line may be in are those of the lines before whose
 * content starts at or before the line's own. It closes at a closing fence, or at the first
 * line that no longer carries the containers it opened in (fewer quote markers, or a list
 * item's content left). What it holds is a [`Span::Code`], never parsed: where the scan takes
 * for a fence what isn't one, that is only drawn wrong. And where it misses one, `TextView`
 * draws it, highlighted off the UI thread all the same
 * ([`crate::services::reader::highlights`]).
 */
fn scan(text: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    let finish = |range: Range<usize>, paragraph: &Paragraph, spans: &mut Vec<Span>| {
        if range.is_empty() {
            return;
        }
        let parse = !paragraph.deep
            && range.len() <= REGION_BYTES
            && paragraph.delimiters * paragraph.delimiters <= DELIMITER_COST;
        spans.push(if parse {
            Span::Prose(range)
        } else {
            Span::Plain(range)
        });
    };
    let mut fence: Option<(Fence, Option<String>, String)> = None;
    /* The columns the content of the list items the line may be in starts at, innermost last. */
    let mut items: Vec<usize> = Vec::new();
    let mut paragraph = Paragraph::default();
    let (mut start, mut at) = (0, 0);
    for line in text.split_inclusive('\n') {
        let line_start = at;
        at += line.len();
        let content = line.trim_end_matches(['\n', '\r']);
        let prefix = Prefix::of(content);
        if let Some((open, language, mut body)) = fence.take() {
            if open.closes(&prefix) {
                spans.push(Span::Code { language, body });
                start = at;
                continue;
            }
            if open.carries(&prefix) {
                body.push_str(open.strip(content));
                body.push('\n');
                fence = Some((open, language, body));
                continue;
            }
            spans.push(Span::Code { language, body });
            start = line_start;
        }
        paragraph.deep |= prefix.depth() > MAX_NESTING;
        if !prefix.blank {
            let start = if prefix.lists > 0 {
                prefix.indent
            } else {
                prefix.column
            };
            while items.last().is_some_and(|&item| item > start) {
                items.pop();
            }
            if prefix.lists > 0 {
                items.push(prefix.content);
            }
        }
        if let Some(open) = Fence::opens(&prefix, items.last().copied()) {
            finish(start..line_start, &paragraph, &mut spans);
            paragraph = Paragraph::default();
            let info = &prefix.rest[open.length * open.mark.len_utf8()..];
            let language = info.split_whitespace().next().map(str::to_string);
            fence = Some((open, language, String::new()));
            continue;
        }
        if prefix.blank {
            finish(start..at, &paragraph, &mut spans);
            (start, paragraph) = (at, Paragraph::default());
            continue;
        }
        let mut previous = '\0';
        for c in prefix.rest.chars() {
            match c {
                '[' | '<' | '*' | '_' | '~' => paragraph.delimiters += 1,
                '`' | '$' if c != previous => paragraph.delimiters += 1,
                _ => {}
            }
            previous = c;
        }
    }
    match fence {
        Some((_, language, body)) => spans.push(Span::Code { language, body }),
        None => finish(start..text.len(), &paragraph, &mut spans),
    }
    spans
}

/// What the scan knows of a paragraph so far.
#[derive(Default)]
struct Paragraph {
    deep: bool,
    delimiters: u64,
}

/// The containers a line carries: its quote and list markers, where its content starts.
struct Prefix<'a> {
    quotes: usize,
    lists: usize,
    /// The columns of whitespace before the first marker (or the content, with none).
    indent: usize,
    /// The column the last list marker's content starts at; 0 with none.
    content: usize,
    /// The columns of whitespace between the last marker and the content.
    inset: usize,
    /// The column the content starts at.
    column: usize,
    rest: &'a str,
    blank: bool,
}

impl<'a> Prefix<'a> {
    fn of(line: &'a str) -> Self {
        /* The columns of the whitespace `text` starts with, and what follows it. */
        fn space(text: &str) -> (usize, &str) {
            let columns = text
                .chars()
                .map_while(|c| match c {
                    ' ' => Some(1),
                    '\t' => Some(4),
                    _ => None,
                })
                .sum();
            (columns, text.trim_start_matches([' ', '\t']))
        }
        let (indent, mut rest) = space(line);
        let (mut quotes, mut lists, mut content) = (0, 0, 0);
        let mut column = indent;
        let mut inset = indent;
        loop {
            if let Some(after) = rest.strip_prefix('>') {
                quotes += 1;
                (inset, rest) = space(after);
                column += 1 + inset;
            } else if let Some(after) = list_marker(rest) {
                lists += 1;
                let marker = rest.len() - after.len();
                (inset, rest) = space(after);
                column += marker + inset;
                content = column - inset + inset.clamp(1, 4);
            } else {
                break;
            }
        }
        Self {
            quotes,
            lists,
            indent,
            content,
            inset,
            column,
            blank: rest.trim().is_empty(),
            rest,
        }
    }

    /**
     * How deep the line's content nests, at least, as the parsed tree counts it: two levels a
     * list (the list and its item), one a quote. A list item indented under another is taken
     * as nested in it, at two columns a level; indentation without a marker (code, a lazy
     * line) nests nothing of its own.
     */
    fn depth(&self) -> usize {
        let lists = match self.lists {
            0 => 0,
            lists => lists + self.indent / 2,
        };
        2 * lists + self.quotes
    }
}

/// An open fence: its mark and length, and the containers it opened in.
struct Fence {
    mark: char,
    length: usize,
    quotes: usize,
    /// The column its lines' content starts at: a list item's, or the fence's own indent.
    content: usize,
}

impl Fence {
    /**
     * The fence the line `prefix` opens, by CommonMark's rules, if it opens one: in the list
     * item whose content starts at the column `item`, when the line has no marker of its own.
     */
    fn opens(prefix: &Prefix, item: Option<usize>) -> Option<Self> {
        let markers = prefix.quotes + prefix.lists;
        /* After a marker, one column of the whitespace belongs to it; on a line of a list
        item's content, the columns up to where that starts do. */
        let (indent, content) = match item.filter(|&item| item <= prefix.column) {
            Some(item) if prefix.lists == 0 => (prefix.column - item, Some(prefix.column)),
            _ if markers == 0 => (prefix.indent, None),
            _ => (prefix.inset.saturating_sub(1), None),
        };
        if indent > 3 {
            return None;
        }
        ['`', '~'].into_iter().find_map(|mark| {
            let length = prefix.rest.chars().take_while(|c| *c == mark).count();
            let info = &prefix.rest[length * mark.len_utf8()..];
            (length >= 3 && !(mark == '`' && info.contains('`'))).then_some(Self {
                mark,
                length,
                quotes: prefix.quotes,
                content: match content {
                    Some(column) => column,
                    None if prefix.lists > 0 => prefix.content,
                    None => indent,
                },
            })
        })
    }

    /// Whether the line `prefix` still carries the containers the fence opened in.
    fn carries(&self, prefix: &Prefix) -> bool {
        prefix.quotes >= self.quotes
            && (self.content == 0
                || prefix.blank
                || prefix.quotes > 0
                || prefix.indent >= self.content)
    }

    /// Whether the line `prefix` closes the fence.
    fn closes(&self, prefix: &Prefix) -> bool {
        let run = prefix.rest.chars().take_while(|c| *c == self.mark).count();
        self.carries(prefix)
            && run >= self.length
            && prefix.rest[run * self.mark.len_utf8()..].trim().is_empty()
    }

    /// `line`, of the fence's code, without its containers' markers and indentation.
    fn strip<'a>(&self, line: &'a str) -> &'a str {
        let mut rest = line;
        for _ in 0..self.quotes {
            let trimmed = rest.trim_start_matches(' ');
            match trimmed.strip_prefix('>') {
                Some(after) => rest = after.strip_prefix(' ').unwrap_or(after),
                None => break,
            }
        }
        let spaces = rest.len() - rest.trim_start_matches(' ').len();
        &rest[spaces.min(self.content)..]
    }
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
                Piece::Code { .. } => "code",
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
                Piece::Plain { .. } | Piece::Code { .. } => None,
            })
            .collect();
        assert!(markdown.len() > 1);
        /* Written out anew where it is carried; as it is where it was written. */
        let [first, .., last] = markdown.as_slice() else {
            panic!("{markdown:?}");
        };
        assert!(
            first.ends_with("\n\n[docs]: <https://example.com/docs>"),
            "{first}"
        );
        assert!(
            last.ends_with("\n\n[docs]: https://example.com/docs"),
            "{last}"
        );
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
        for body in
            [
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
            ]
            .into_iter()
            /* Runs of emphasis whose parse time grows faster than their square. */
            .chain([20_000, 100_000, 250_000].into_iter().flat_map(|n| {
                ["*", "_"].map(|mark| format!("{}a{}", mark.repeat(n), mark.repeat(n)))
            }))
            /* What a fence could be mistaken for, to stop the scan looking: an info string with a
            backtick, an indented one, one in a list item that ends without closing it. */
            .chain(
                ["```x`\n", "para\n    ```\n", "- ```\nX\n\n"]
                    .into_iter()
                    .flat_map(|opener| {
                        [">".repeat(80_000), "*a".repeat(80_000)]
                            .map(|after| format!("{opener}{after}"))
                    }),
            )
        {
            let start: String = body.chars().take(12).collect();
            let (prose, took) = on_a_worker_stack(body);
            assert!(!prose.pieces.is_empty(), "{start:?}");
            /* Unoptimised, as tests run: about ten times what a release build takes. */
            assert!(took.as_secs_f64() < 2.0, "{start:?} took {took:?}");
        }
    }

    /// `unit` repeated to `size` bytes, its last whole repeat in.
    fn repeated(unit: &str, size: usize) -> String {
        unit.repeat((size / unit.len()).max(1))
    }

    /**
     * The inputs the parser was found slowest on, each a family whose cost grows with its size:
     * by name, made to `size` bytes. What a fence could be mistaken for comes before a payload
     * the scan would otherwise have counted.
     */
    fn families(size: usize) -> Vec<(&'static str, String)> {
        let table = |size: usize| {
            let row = format!("|{}\n", "a|".repeat(50));
            let head = format!("{row}|{}\n", "-|".repeat(50));
            format!("{head}{}", repeated(&row, size.saturating_sub(head.len())))
        };
        let mut families = vec![
            ("blocks", repeated("a\n\n", size)),
            ("list markers", repeated("*\n1.\n", size)),
            ("tables", table(size)),
            (
                "entities after an autolink",
                format!("www.a.b{}&am", repeated("&amp;", size - 10)),
            ),
            (
                "unknown entities after an autolink",
                format!("www.a.b{}&a", repeated("&a;", size - 9)),
            ),
            /* An autolink's trailing punctuation, looked at again for each mark. */
            (
                "autolink, then ]",
                format!("www.a.b{}a", "]".repeat(size - 8)),
            ),
            (
                "autolink, then .",
                format!("www.a.b{}a", ".".repeat(size - 8)),
            ),
            (
                "autolink, then !",
                format!("www.a.b{}a", "!".repeat(size - 8)),
            ),
            (
                "autolink, then )",
                format!("www.a.b{}a", ")".repeat(size - 8)),
            ),
            ("setext lines", repeated("a\n=\n", size)),
            ("hard breaks", repeated("a  \n", size)),
            ("emphasis runs", repeated("*a ", size)),
            (
                "emphasis, open and closed",
                repeated("*a ", size / 2) + &repeated("a*", size / 2),
            ),
            ("brackets", repeated("[a", size)),
            ("angles", repeated("<a ", size)),
            ("links", repeated("[a](", size)),
            ("quotes", repeated(">", size)),
            ("lists", repeated("- ", size)),
            ("ordered lists", repeated("1. ", size)),
            (
                "nested lists",
                (0..)
                    .map(|n| format!("{}- level\n", "  ".repeat(n)))
                    .scan(0, |total, line| {
                        *total += line.len();
                        (*total <= size).then_some(line)
                    })
                    .collect(),
            ),
        ];
        for opener in [
            "```x`\n",
            "para\n    ```\n",
            "- ```\nX\n\n",
            "<div>\n```\n\n",
            "<div>\n~~~\n",
            "$$\n```\n$$\n",
            "<!--\n```\n-->\n",
            "para\n2. ```\n",
            "para\n    - ```\n",
            "para\n    > ```\n",
        ] {
            for (name, payload) in [("quotes", ">"), ("emphasis", "*a")] {
                let body = format!("{opener}{}", repeated(payload, size - opener.len()));
                families.push((
                    if name == "quotes" {
                        "a fence's opener, then quotes"
                    } else {
                        "a fence's opener, then emphasis"
                    },
                    body,
                ));
            }
        }
        families
    }

    /**
     * Every family, at [`REGION_BYTES`], parsed and prepared as one region: the worst a parse
     * can cost. On a release build each is bounded at 250 ms; unoptimised, as tests run, ten
     * times that. `cargo test --release -p sb_ui --lib worst -- --nocapture` prints the table.
     */
    #[test]
    fn the_worst_a_region_can_cost_is_bounded() {
        let bound = if cfg!(debug_assertions) { 2.5 } else { 0.25 };
        let mut worst = (0.0, "");
        for (name, body) in families(REGION_BYTES) {
            assert!(body.len() <= REGION_BYTES, "{name}: {}", body.len());
            let start = std::time::Instant::now();
            let pieces = std::thread::Builder::new()
                .stack_size(PARSE_STACK)
                .spawn(move || markdown_pieces(&body, "", &|| true).map(|it| it.pieces.len()))
                .unwrap()
                .join()
                .unwrap();
            let took = start.elapsed().as_secs_f64();
            eprintln!("{name:<36} {:>8.1} ms  {pieces:?} pieces", took * 1000.);
            if took > worst.0 {
                worst = (took, name);
            }
        }
        eprintln!("worst: {} at {:.1} ms", worst.1, worst.0 * 1000.);
        assert!(
            worst.0 < bound,
            "{} took {:.0} ms",
            worst.1,
            worst.0 * 1000.
        );
    }

    /**
     * Every family at a whole body's [`MARKDOWN_BYTES`], prepared as the reader prepares a
     * body: bounded by [`PARSE_BUDGET`] and one region past it, on a worker thread's stack.
     */
    #[test]
    fn a_whole_body_of_any_family_is_prepared_in_bounded_time() {
        let bound = if cfg!(debug_assertions) { 4.0 } else { 0.5 };
        let bodies = families(MARKDOWN_BYTES)
            .into_iter()
            .chain(scanned(MARKDOWN_BYTES))
            .chain(carrying());
        let mut worst = (0.0, "");
        for (name, body) in bodies {
            let (prose, took) = on_a_worker_stack(body);
            let took = took.as_secs_f64();
            eprintln!("{name:<44} {:>8.1} ms", took * 1000.);
            assert!(!prose.pieces.is_empty(), "{name}");
            assert!(took < bound, "{name} took {:.0} ms", took * 1000.);
            if took > worst.0 {
                worst = (took, name);
            }
        }
        eprintln!("worst: {} at {:.1} ms", worst.1, worst.0 * 1000.);
    }

    /**
     * What only the scan keeps from the parser, each to `size` bytes: lazy lines in a list or a
     * quote nested to half the body, and a line of quote and list markers. A region of any
     * takes the parser past the region's bound (400 ms and more at [`REGION_BYTES`]).
     */
    fn scanned(size: usize) -> Vec<(&'static str, String)> {
        let half = size / 2;
        vec![
            (
                "lazy lines in a deep list",
                repeated("- ", half) + "a\n" + &repeated("b\n", half - 3),
            ),
            (
                "lazy lines in a deep quote",
                repeated(">", half) + "a\n" + &repeated("b\n", half - 3),
            ),
            (
                "quote, list and ordered list markers",
                repeated("> 1. - ", size),
            ),
        ]
    }

    /**
     * A block the scan lets through and the view can't be handed, nested too deep: it cuts a
     * region's blocks into pieces.
     */
    fn splitter() -> String {
        let splitter = format!("{}a", "- ".repeat(MAX_NESTING / 2));
        assert!(matches!(scan(&splitter).as_slice(), [Span::Prose(_)]));
        let pieces = markdown_pieces(&splitter, "", &|| true).unwrap().pieces;
        assert!(matches!(
            pieces.as_slice(),
            [Piece::Plain {
                oversized: true,
                ..
            }]
        ));
        splitter
    }

    /**
     * Bodies built to make what is carried between regions and pieces cost: a line that reads
     * like a definition and isn't, and a definition written in a quote, its destination on the
     * next line (an autolink with trailing brackets were it a paragraph), referred to by 64
     * pieces of the region it is in, of another region, and of 64 other regions.
     */
    fn carrying() -> Vec<(&'static str, String)> {
        let trail = |size: usize| format!("www.a.b{}a", "]".repeat(size - 8));
        let splitter = splitter();
        let quoted = format!("> [a]:\n> {}", trail(5_400));
        let small = format!("[a]\n\n{splitter}\n\n").repeat(64);
        let large = format!("[a] {}\n\n{splitter}\n\n", "x".repeat(40)).repeat(64);
        vec![
            (
                "a line like a definition, and a reference",
                format!(
                    "[a] {}\n\n[a]: x {}",
                    trail(REGION_BYTES - 4),
                    trail(DEFINITION_BYTES - 10)
                ),
            ),
            (
                "a quoted definition, in its region's 64 pieces",
                format!("{quoted}\n\n{small}"),
            ),
            (
                "a quoted definition, carried to 64 pieces",
                format!("{large}{quoted}"),
            ),
            (
                "and to 64 regions of 64",
                format!("{}{quoted}", large.repeat(64)),
            ),
        ]
    }

    #[test]
    fn what_is_carried_is_what_the_parser_read() {
        let trail = format!("www.a.b{}a", "]".repeat(2_000));
        let splitter = splitter();
        let body = format!(
            "[a]\n\n{splitter}\n\n[a]\n\n{}\n\n> [a]:\n> {trail}\n\n[b]: x {trail}\n\n[b]",
            "x".repeat(REGION_BYTES)
        );
        let prose = Prose::markdown(&body);
        let markdown: Vec<&str> = prose
            .pieces
            .iter()
            .filter_map(|piece| match piece {
                Piece::Markdown(markdown) => Some(markdown.as_str()),
                Piece::Plain { .. } | Piece::Code { .. } => None,
            })
            .collect();
        /* Each piece referring to the quoted definition carries it on a line of its own. */
        let carried = format!("[a]\n\n[a]: <{trail}>");
        assert_eq!(
            markdown.iter().filter(|it| **it == carried).count(),
            2,
            "{markdown:?}"
        );
        /* A line like a definition that isn't one is carried nowhere. */
        assert!(
            markdown.iter().all(|it| !it.contains("[b]: x")),
            "{markdown:?}"
        );
    }

    #[test]
    fn a_definition_carried_reads_back_as_the_same_definition() {
        for definition in [
            "[A  b]: <https://example.com/a b> \"t\"",
            "[a]: https://example.com/&amp;\\<\\>\\\\ 'a \"quoted\" title'",
            "[a]: https://example.com/&#10;x (a title\nover lines)",
            "> [a]:\n> https://example.com/x\n> \"t\"",
        ] {
            let read = |text: &str| -> Option<markdown::mdast::Definition> {
                let root = markdown::to_mdast(text, &options()).ok()?;
                let mut stack = vec![root];
                while let Some(node) = stack.pop() {
                    if let Node::Definition(found) = node {
                        return Some(found);
                    }
                    stack.extend(node.children().into_iter().flatten().cloned());
                }
                None
            };
            let found = read(definition).unwrap_or_else(|| panic!("{definition:?}"));
            let carried = super::definition(&found);
            assert!(!carried.contains('\n'), "{carried:?}");
            let again = read(&carried).unwrap_or_else(|| panic!("{carried:?}"));
            assert_eq!(again.identifier, found.identifier, "{carried:?}");
            assert_eq!(again.title, found.title.map(|it| it.replace('\n', " ")));
            assert_eq!(
                again.url,
                found.url.replace('\n', "%0A"),
                "{definition:?} as {carried:?}"
            );
        }
    }

    /**
     * Every piece of every family, at a region's size, and of every body built to carry: either
     * plain, or Markdown whose parse by the view itself takes under [`VIEW_PARSE_LIMIT`] for
     * each unit. Says how many fell back to plain text.
     */
    #[test]
    fn every_piece_the_view_is_handed_is_one_it_parses_quickly() {
        let bodies = families(REGION_BYTES)
            .into_iter()
            .chain(scanned(REGION_BYTES))
            .chain(carrying());
        let (mut markdown, mut plain) = (0, 0);
        /* The share of its limit the slowest piece of each family took the view. */
        let mut worst: HashMap<&str, f64> = HashMap::new();
        for (name, body) in bodies {
            let (prose, _) = on_a_worker_stack(body);
            for piece in prose.pieces {
                let Piece::Markdown(sanitized) = piece else {
                    plain += 1;
                    continue;
                };
                markdown += 1;
                /* The least of three, as the gate takes its time again: the tests run side by
                side, and one may be made to wait. */
                let took = std::thread::Builder::new()
                    .stack_size(PARSE_STACK)
                    .spawn(move || {
                        let took = (0..3).map(|_| {
                            let start = Instant::now();
                            let parsed = gpui_base::text::editor_host::parse_markdown(
                                sanitized.as_str(),
                                Default::default(),
                            );
                            assert!(parsed.is_ok());
                            start.elapsed()
                        });
                        (took.min().unwrap_or_default(), sanitized.as_str().len())
                    })
                    .unwrap()
                    .join()
                    .unwrap();
                let (took, bytes) = took;
                let limit = crate::markdown::view_parse_limit(bytes);
                assert!(
                    took < limit,
                    "{name}: a piece of {bytes} bytes took the view {took:?}"
                );
                let share = worst.entry(name).or_default();
                *share = share.max(took.as_secs_f64() / limit.as_secs_f64());
            }
        }
        let mut worst: Vec<(&str, f64)> = worst.into_iter().collect();
        worst.sort_by(|a, b| b.1.total_cmp(&a.1));
        for (name, share) in worst {
            eprintln!("{name:<44} {:>5.1}% of the limit", share * 100.);
        }
        eprintln!("{markdown} pieces of Markdown, {plain} plain");
    }

    #[test]
    fn a_body_no_longer_wanted_goes_no_further_than_its_next_piece() {
        /* One region, cut into pieces by blocks nested too deep to lay out. */
        let body = repeated(&format!("Paragraph.\n\n{}\n\n", splitter()), 2_048);
        assert_eq!(regions(scan(&body)).len(), 1);
        /* Wanted for the region and its first piece. */
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let prose = Prose::markdown_while(&body, &|| {
            asked.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 2
        });
        let paragraphs: Vec<&Piece> = prose
            .pieces
            .iter()
            .filter(|piece| match piece {
                Piece::Markdown(markdown) => markdown.as_str().starts_with("Paragraph"),
                Piece::Plain { text, .. } => text.text.starts_with("Paragraph"),
                Piece::Code { .. } => false,
            })
            .collect();
        assert!(paragraphs.len() > 10);
        assert!(matches!(paragraphs[0], Piece::Markdown(_)));
        assert!(
            paragraphs[1..]
                .iter()
                .all(|piece| matches!(piece, Piece::Plain { .. }))
        );
    }

    #[test]
    fn a_body_of_a_closed_session_goes_no_further_than_its_next_parse() {
        let body = repeated("Paragraph.\n\n", MARKDOWN_BYTES);
        let prose = Prose::markdown_while(&body, &|| false);
        assert!(prose.pieces.iter().all(|piece| matches!(
            piece,
            Piece::Plain {
                oversized: true,
                ..
            }
        )));
    }

    #[test]
    fn fenced_code_is_a_piece_of_its_own_and_never_parsed() {
        let body = "Before.\n\n```rust\nfn main() {}\n```\n\n> quoted\n> ```\n> <img src=x>\n> ```\n\nAfter.";
        let prose = Prose::markdown(body);
        let codes: Vec<(Option<&str>, &str)> = prose
            .pieces
            .iter()
            .filter_map(|piece| match piece {
                Piece::Code { language, text } => Some((language.as_deref(), text.text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            codes,
            [(Some("rust"), "fn main() {}\n"), (None, "<img src=x>\n")]
        );
        assert_eq!(
            kinds(&prose),
            ["markdown", "code", "markdown", "code", "markdown"]
        );
    }

    /**
     * A fence indented on a line of a list item's content, up to three columns past where that
     * content starts, as CommonMark has it: a piece of code, in a list, a nested list, a list
     * in a quote. Indented four past it, or after the list has ended, it is not.
     */
    #[test]
    fn a_fence_in_a_list_items_content_is_a_piece_of_its_own() {
        let codes = |body: &str| -> Vec<(Option<String>, String)> {
            Prose::markdown(body)
                .pieces
                .into_iter()
                .filter_map(|piece| match piece {
                    Piece::Code { language, text } => Some((language, text.text.clone())),
                    _ => None,
                })
                .collect()
        };
        let swift = |body: &str| vec![(Some("swift".to_string()), body.to_string())];
        assert_eq!(
            codes("- a\n\n    ```swift\n    /* x\n\n    y\n    ```\n"),
            swift("/* x\n\ny\n")
        );
        assert_eq!(
            codes("1. a\n\n   ```swift\n   z\n   ```\nAfter."),
            swift("z\n")
        );
        assert_eq!(
            codes("- a\n  - b\n\n      ```swift\n      z\n      ```\n"),
            swift("z\n")
        );
        assert_eq!(
            codes("- a\n  - b\n\n        ```swift\n        z\n        ```\n").len(),
            0
        );
        assert_eq!(
            codes("> - a\n>\n>     ```swift\n>     z\n>     ```\n"),
            swift("z\n")
        );
        assert_eq!(
            codes("- a\n\n      ```swift\n      z\n      ```\n").len(),
            0
        );
        assert_eq!(codes("- a\n\nb\n\n    ```swift\n    z\n    ```\n").len(), 0);
    }

    #[test]
    fn carried_definitions_are_bounded_and_found_in_one_pass() {
        let sizes = |prose: &Prose| -> (usize, usize) {
            let markdown = prose.pieces.iter().filter_map(|piece| match piece {
                Piece::Markdown(markdown) => Some(markdown.as_str().len()),
                Piece::Plain { .. } | Piece::Code { .. } => None,
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
    fn a_reference_carries_the_first_of_its_definitions() {
        let filler = "a".repeat(PIECE_BYTES - 10);
        let body = format!(
            "{filler}\n\nSee [the bank][b].\n\n[b]: https://bank.example\n\n{filler}\n\n[b]: https://evil.example"
        );
        let prose = Prose::markdown(&body);
        let carried: Vec<&str> = prose
            .pieces
            .iter()
            .filter_map(|piece| match piece {
                Piece::Markdown(markdown) if markdown.as_str().contains("[the bank][b]") => {
                    Some(markdown.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(carried.len(), 1);
        assert!(carried[0].contains("bank.example") && !carried[0].contains("evil.example"));
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
