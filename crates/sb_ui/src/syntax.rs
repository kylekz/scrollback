//! Syntax highlighting (PLAN §9): tree-sitter, with the grammars and highlight queries of
//! gpui-component's registry, producing **token kinds** rather than colours, as dv's `syntax`
//! does (lifted from it, PLAN D15).
//!
//! A kind resolves to a colour only when it paints ([`TokenKind::color`], from the theme's
//! [`SyntaxColors`]), so a theme change repaints without highlighting again. The queries'
//! capture names (`keyword.return`, `function.method`, `string.escape`, …) map to kinds through
//! one table ([`kind_for_capture`]).
//!
//! Code is untrusted, and tree-sitter is far slower than linear on some of it (a run of `/*` in
//! Swift took minutes at 8 KiB). So [`highlight`] is bounded whatever the code: it takes at most
//! [`MAX_HIGHLIGHT_BYTES`], and gives up past [`HIGHLIGHT_BUDGET`], tree-sitter's parses and
//! queries being cancelled at their next check of the deadline. Tree-sitter checks only between
//! its steps, and one step can take far longer than the budget, so it is handed the code a
//! little at a time and none past the deadline ([`parse`]), and parses it [`SEGMENT_BYTES`] of
//! whole lines at a time, which bounds its longest step; a construct that runs across two
//! segments (a long block comment) may be coloured wrong past the cut. What it gives up on is
//! drawn plain. It never runs on the UI thread: the reader asks for it in the background
//! ([`crate::services::reader`]) and draws the code plain until it lands.
//!
//! A grammar whose C code aborts the process on some input (an `assert`, which nothing can
//! catch) can't be bounded at all, so it is not built in, and its code is plain: Bash (nested
//! heredocs), Kotlin (nested string templates) and YAML (a few hundred levels of indentation),
//! whose scanners overflow the state tree-sitter keeps for them. The ignored test
//! `grammars_never_abort` runs every grammar over code built to find such a thing, each in a
//! process of its own.

use std::collections::HashMap;
use std::ops::{ControlFlow, Range};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use gpui::Hsla;
use gpui_component::highlighter::LanguageRegistry;
use tree_sitter::{
    Language, ParseOptions, Parser, Query, QueryCursor, QueryCursorOptions, StreamingIterator as _,
    Tree,
};

use crate::theme::SyntaxColors;

/// Code larger than this, in bytes, isn't highlighted.
pub const MAX_HIGHLIGHT_BYTES: usize = 16 * 1024;
/**
 * The most time one block's highlighting may take, parses and queries together; past it, the
 * block is plain. Ordinary code of [`MAX_HIGHLIGHT_BYTES`] takes a few milliseconds.
 */
pub const HIGHLIGHT_BUDGET: Duration = Duration::from_millis(50);
/// The most injected layers (a script in HTML, a fence in Markdown) a block is highlighted with.
const MAX_LAYERS: usize = 16;
/// The longest name of an injected language read from the code: longer names name none.
const MAX_LANGUAGE_NAME: usize = 64;
/// How much code tree-sitter is handed at a time ([`parse`]).
const READ_BYTES: usize = 256;
/**
 * The most code parsed as one (the module docs): the longest step between two of tree-sitter's
 * checks of its deadline grows faster than the code. Parsing C#'s `<a ` repeated took 25 ms at
 * 2 KiB, 100 ms at 4 KiB and 1.5 s at 16 KiB, nearly all of it one step.
 */
const SEGMENT_BYTES: usize = 2 * 1024;

/// What a piece of code is, as far as colouring goes (DESIGN §1.1, §1.2 "Syntax").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenKind {
    Keyword,
    Function,
    String,
    Number,
    Type,
    Property,
    Comment,
    Plain,
}

impl TokenKind {
    /// The colour this kind paints in; `None` for plain code, which keeps the text colour.
    pub fn color(self, syntax: &SyntaxColors) -> Option<Hsla> {
        match self {
            Self::Keyword => Some(syntax.keyword),
            Self::Function => Some(syntax.function),
            Self::String => Some(syntax.string),
            Self::Number => Some(syntax.number),
            Self::Type => Some(syntax.ty),
            Self::Property => Some(syntax.property),
            Self::Comment => Some(syntax.comment),
            Self::Plain => None,
        }
    }
}

/// The kind a tree-sitter capture name stands for; `None` for a name that says nothing about
/// colour (a query's private `_name` captures, unknown names), which leaves the enclosing
/// capture's kind in place.
///
/// A capture that names plain code explicitly (`variable`, `punctuation`, `embedded`, …) is
/// [`TokenKind::Plain`], so a variable interpolated into a string paints plain, not as the
/// string around it. By the name's first segment unless the full name is listed:
///
/// | Kind | Names |
/// |---|---|
/// | Keyword | `keyword*`, `conditional`, `repeat`, `include`, `import`, `exception`, `storageclass`, `preproc`, `variable.builtin`, `variable.special`, `tag*`, `title`, `boolean`, `label` (DESIGN §4.2.4: literals are `k`) |
/// | Function | `function*`, `method*`, `link_text` |
/// | String | `string*`, `character*`, `escape`, `text.literal`, `text.code.span`, `text.uri`, `link_uri` |
/// | Number | `number*`, `float`, `constant*` |
/// | Type | `type*`, `constructor` |
/// | Property | `property*`, `field`, `attribute`, `tag.attribute`, `variable.member` |
/// | Comment | `comment*` |
/// | Plain | `variable*` (others), `parameter`, `punctuation*`, `operator`, `delimiter`, `embedded`, `module*`, `namespace`, `emphasis*`, `text*` (others), `none`, `spell`, `nospell` |
pub fn kind_for_capture(name: &str) -> Option<TokenKind> {
    use TokenKind::*;
    if name.is_empty() || name.starts_with('_') {
        return None;
    }
    let exact = match name {
        "constructor" => Some(Type),
        "boolean" | "label" | "variable.builtin" | "variable.special" | "conditional"
        | "repeat" | "include" | "import" | "exception" | "storageclass" | "preproc" | "title" => {
            Some(Keyword)
        }
        "attribute" | "field" | "tag.attribute" | "variable.member" => Some(Property),
        "operator" | "delimiter" | "tag.delimiter" => Some(Plain),
        "escape" | "text.literal" | "text.code.span" | "text.uri" | "link_uri" => Some(String),
        "float" => Some(Number),
        "link_text" => Some(Function),
        _ => None,
    };
    if exact.is_some() {
        return exact;
    }
    match name.split('.').next().unwrap_or(name) {
        "keyword" | "tag" => Some(Keyword),
        "function" | "method" => Some(Function),
        "string" | "character" => Some(String),
        // Grammars capture number literals and other constants alike (`constant.builtin`).
        "number" | "constant" => Some(Number),
        "type" => Some(Type),
        "property" => Some(Property),
        "comment" => Some(Comment),
        "variable" | "parameter" | "punctuation" | "embedded" | "module" | "namespace"
        | "emphasis" | "text" | "none" | "spell" | "nospell" => Some(Plain),
        _ => None,
    }
}

/// A language's grammar and its queries, compiled once for the process.
struct Grammar {
    language: Language,
    highlights: Query,
    injections: Option<Query>,
}

/**
 * The grammar of `name`, a name or alias gpui-component's registry knows (`rust`, `ts`, `sh`,
 * …); `None` for a language without one, or whose queries don't compile.
 */
fn grammar(name: &str) -> Option<Arc<Grammar>> {
    static GRAMMARS: LazyLock<Mutex<HashMap<String, Option<Arc<Grammar>>>>> =
        LazyLock::new(Mutex::default);
    let mut grammars = GRAMMARS.lock().unwrap_or_else(PoisonError::into_inner);
    grammars
        .entry(name.to_string())
        .or_insert_with(|| {
            let config = LanguageRegistry::singleton().language(name)?;
            let language = config.language?;
            Some(Arc::new(Grammar {
                highlights: Query::new(&language, &config.highlights).ok()?,
                injections: Query::new(&language, &config.injections).ok(),
                language,
            }))
        })
        .clone()
}

/**
 * The non-plain tokens of `code` in `language`, a name or alias gpui-component's registry knows
 * (`rust`, `ts`, `sh`, …), as byte ranges of `code` in order: empty for a language without a
 * grammar. `None` when the code is longer than [`MAX_HIGHLIGHT_BYTES`] or took longer than
 * [`HIGHLIGHT_BUDGET`] (the module docs): it is drawn plain.
 *
 * Captures paint as gpui-component's highlighter paints them, so that a block looks as it does
 * in dv: where they nest, the one that starts last wins, and of two that start together the
 * later match; a capture of the same node as the one before it takes that one's name. The
 * layers injected into the code (a script in HTML) paint over it.
 */
pub fn highlight(language: &str, code: &str) -> Option<Vec<(Range<usize>, TokenKind)>> {
    highlight_within(language, code, HIGHLIGHT_BUDGET)
}

/// [`highlight`] with `budget` for the budget.
fn highlight_within(
    language: &str,
    code: &str,
    budget: Duration,
) -> Option<Vec<(Range<usize>, TokenKind)>> {
    if code.len() > MAX_HIGHLIGHT_BYTES {
        return None;
    }
    let Some(host) = grammar(language) else {
        return Some(Vec::new());
    };
    let mut deadline = Instant::now() + budget;
    let mut tokens = Vec::new();
    for (start, segment) in segments(code) {
        let painted = highlight_segment(&host, segment.as_bytes(), &mut deadline)?;
        tokens.extend(
            painted
                .into_iter()
                .map(|(range, kind)| (range.start + start..range.end + start, kind)),
        );
    }
    Some(tokens)
}

/**
 * `code` cut into runs of whole lines of at most [`SEGMENT_BYTES`], each with where it starts;
 * a line longer than that is cut where a character ends.
 */
fn segments(code: &str) -> Vec<(usize, &str)> {
    let mut segments = Vec::new();
    let mut start = 0;
    while start < code.len() {
        let mut end = code.len().min(start + SEGMENT_BYTES);
        if end < code.len() {
            match code[start..end].rfind('\n') {
                Some(at) => end = start + at + 1,
                None => {
                    while !code.is_char_boundary(end) {
                        end -= 1;
                    }
                }
            }
        }
        segments.push((start, &code[start..end]));
        start = end;
    }
    segments
}

/**
 * The tokens of `code`, a segment, by `host`'s grammar and those it injects, by `deadline`;
 * moved on by the time taken to compile an injected grammar the first time.
 */
fn highlight_segment(
    host: &Grammar,
    code: &[u8],
    deadline: &mut Instant,
) -> Option<Vec<(Range<usize>, TokenKind)>> {
    let tree = parse(&host.language, code, &[], *deadline)?;
    let mut captures = Vec::new();
    let mut last: Option<Range<usize>> = None;
    for_each_capture(
        &host.highlights,
        &tree,
        code,
        *deadline,
        |range, name, _| {
            let kind = match &last {
                Some(previous) if *previous == range => captures
                    .last()
                    .and_then(|(_, kind): &(Range<usize>, Option<TokenKind>)| *kind),
                _ => kind_for_capture(name),
            };
            last = Some(range.clone());
            captures.push((range, kind));
        },
    )?;
    let injected = injections(host, &tree, code, *deadline)?;
    /* A grammar compiled the first time it is asked for doesn't count against the code. */
    let compiling = Instant::now();
    let layers: Vec<(Arc<Grammar>, Vec<tree_sitter::Range>)> = injected
        .into_iter()
        .filter_map(|(name, ranges)| Some((grammar(&name)?, ranges)))
        .take(MAX_LAYERS)
        .collect();
    *deadline += compiling.elapsed();
    let deadline = *deadline;
    for (layer, ranges) in layers {
        let tree = parse(&layer.language, code, &ranges, deadline)?;
        let mut end = 0;
        for_each_capture(
            &layer.highlights,
            &tree,
            code,
            deadline,
            |range, name, overlap| {
                if overlap || range.start >= end {
                    if !overlap {
                        end = range.end;
                    }
                    captures.push((range, kind_for_capture(name)));
                }
            },
        )?;
    }
    paint(captures, deadline)
}

/// `code` parsed with `language`, only within `ranges` when there are any; `None` past
/// `deadline`.
fn parse(
    language: &Language,
    code: &[u8],
    ranges: &[tree_sitter::Range],
    deadline: Instant,
) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(language).ok()?;
    if !ranges.is_empty() {
        parser.set_included_ranges(ranges).ok()?;
    }
    let mut progress = |_: &tree_sitter::ParseState| in_time(deadline);
    parser
        .parse_with_options(
            /* The code a little at a time, and none past the deadline: tree-sitter asks for
            its progress only between tokens, and one token can take it a quadratic scan of
            the rest (a run of `#` in Swift, of `<a ` in C#); asked for more as it scans, this
            ends that scan at the deadline. Past the end, and for an offset past it (the
            callback must not panic), there is no more code. */
            &mut |offset, _| {
                let end = code.len().min(offset.saturating_add(READ_BYTES));
                match code.get(offset..end) {
                    Some(chunk) if Instant::now() < deadline => chunk,
                    _ => &[],
                }
            },
            None,
            Some(ParseOptions::new().progress_callback(&mut progress)),
        )
        .filter(|_| Instant::now() < deadline)
}

/**
 * Calls `capture` with each capture of `query` over `tree`, in the order of its matches: its
 * byte range, its name, and whether its pattern lets it overlap others. `None` past
 * `deadline`.
 */
fn for_each_capture(
    query: &Query,
    tree: &Tree,
    code: &[u8],
    deadline: Instant,
    mut capture: impl FnMut(Range<usize>, &str, bool),
) -> Option<()> {
    let mut cursor = QueryCursor::new();
    let mut progress = |_: &tree_sitter::QueryCursorState| in_time(deadline);
    let mut matches = cursor.matches_with_options(
        query,
        tree.root_node(),
        code,
        QueryCursorOptions::new().progress_callback(&mut progress),
    );
    while let Some(found) = matches.next() {
        /* A match's predicates are checked outside tree-sitter's own checks. */
        if Instant::now() >= deadline {
            return None;
        }
        let overlap = query
            .property_settings(found.pattern_index)
            .iter()
            .any(|property| &*property.key == "highlight.allow-overlap");
        for each in found.captures {
            if let Some(name) = query.capture_names().get(each.index as usize) {
                capture(each.node.byte_range(), name, overlap);
            }
        }
    }
    (Instant::now() < deadline).then_some(())
}

fn in_time(deadline: Instant) -> ControlFlow<()> {
    if Instant::now() < deadline {
        ControlFlow::Continue(())
    } else {
        ControlFlow::Break(())
    }
}

/**
 * The names of the languages injected into `code` by `host`'s injection query, with the ranges
 * of `code` each is parsed over, in the order they start: one layer per match, or one per
 * language for the patterns that combine their matches. `None` past `deadline`.
 */
fn injections(
    host: &Grammar,
    tree: &Tree,
    code: &[u8],
    deadline: Instant,
) -> Option<Vec<(String, Vec<tree_sitter::Range>)>> {
    let Some(query) = &host.injections else {
        return Some(Vec::new());
    };
    let index = |name| query.capture_index_for_name(name);
    let (content, named) = (index("injection.content"), index("injection.language"));
    let mut layers: Vec<(String, Vec<tree_sitter::Range>)> = Vec::new();
    let mut combined: HashMap<String, Vec<tree_sitter::Range>> = HashMap::new();
    let mut cursor = QueryCursor::new();
    let mut progress = |_: &tree_sitter::QueryCursorState| in_time(deadline);
    let mut matches = cursor.matches_with_options(
        query,
        tree.root_node(),
        code,
        QueryCursorOptions::new().progress_callback(&mut progress),
    );
    while let Some(found) = matches.next() {
        if Instant::now() >= deadline {
            return None;
        }
        let settings = query.property_settings(found.pattern_index);
        let setting = |key: &str| settings.iter().find(|it| &*it.key == key);
        let name = match setting("injection.language").and_then(|it| it.value.as_deref()) {
            Some(name) => Some(name.to_string()),
            None => found
                .captures
                .iter()
                .find(|it| Some(it.index) == named)
                .map(|it| it.node.byte_range())
                .filter(|range| range.len() <= MAX_LANGUAGE_NAME)
                .and_then(|range| std::str::from_utf8(&code[range]).ok())
                .map(|name| name.trim().to_string()),
        };
        let Some(name) = name.filter(|name| !name.is_empty()) else {
            continue;
        };
        let ranges = found
            .captures
            .iter()
            .filter(|it| Some(it.index) == content)
            .map(|it| it.node.range());
        if setting("injection.combined").is_some() {
            combined.entry(name).or_default().extend(ranges);
        } else {
            layers.push((name, ranges.collect()));
        }
    }
    if Instant::now() >= deadline {
        return None;
    }
    layers.extend(combined);
    for (_, ranges) in &mut layers {
        /* Tree-sitter takes included ranges in order and apart. */
        ranges.sort_by_key(|range| (range.start_byte, range.end_byte));
        ranges.dedup_by(|next, previous| next.start_byte < previous.end_byte);
    }
    layers.retain(|(_, ranges)| !ranges.is_empty());
    layers.sort_by_key(|(_, ranges)| ranges[0].start_byte);
    Some(layers)
}

/**
 * The tokens `captures` paint, in order, merged where alike and without plain ones: at each
 * byte, of the captures over it with a kind, the one that starts last, and of those the last.
 * `None` past `deadline`.
 */
fn paint(
    captures: Vec<(Range<usize>, Option<TokenKind>)>,
    deadline: Instant,
) -> Option<Vec<(Range<usize>, TokenKind)>> {
    use std::collections::BinaryHeap;

    let mut by_start: Vec<(usize, Range<usize>, TokenKind)> = captures
        .into_iter()
        .enumerate()
        .filter_map(|(at, (range, kind))| Some((at, range.clone(), kind?)))
        .filter(|(_, range, _)| !range.is_empty())
        .collect();
    by_start.sort_by_key(|(at, range, _)| (range.start, *at));
    let mut edges: Vec<usize> = by_start
        .iter()
        .flat_map(|(_, range, _)| [range.start, range.end])
        .collect();
    edges.sort_unstable();
    edges.dedup();
    /* The captures over the run being painted, by where they start and their place among the
    captures, the one on top first, each with its place in `by_start`; one that has ended is
    dropped once it is on top. */
    let mut over: BinaryHeap<(usize, usize, usize)> = BinaryHeap::new();
    let mut opened = 0;
    let mut tokens: Vec<(Range<usize>, TokenKind)> = Vec::new();
    for (step, pair) in edges.windows(2).enumerate() {
        if step % 1024 == 0 && Instant::now() >= deadline {
            return None;
        }
        let (start, end) = (pair[0], pair[1]);
        while let Some((at, range, _)) = by_start.get(opened).filter(|it| it.1.start <= start) {
            over.push((range.start, *at, opened));
            opened += 1;
        }
        while over
            .peek()
            .is_some_and(|&(_, _, place)| by_start[place].1.end <= start)
        {
            over.pop();
        }
        match over.peek().map(|&(_, _, place)| by_start[place].2) {
            Some(kind) if kind != TokenKind::Plain => match tokens.last_mut() {
                Some((last, same)) if last.end == start && *same == kind => last.end = end,
                _ => tokens.push((start..end, kind)),
            },
            _ => {}
        }
    }
    Some(tokens)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn kinds(language: &str, code: &str) -> Vec<(&'static str, TokenKind)> {
        // Leaked so the pairs can borrow the code for the assertion's lifetime.
        let code: &'static str = code.to_string().leak();
        highlight(language, code)
            .unwrap()
            .into_iter()
            .map(|(range, kind)| (&code[range], kind))
            .collect()
    }

    #[test]
    fn rust_is_highlighted_into_kinds() {
        let tokens = kinds("rust", "fn add(a: u32) -> u32 { \"s\"; 12 } // c");
        for expected in [
            ("fn", TokenKind::Keyword),
            ("add", TokenKind::Function),
            ("u32", TokenKind::Type),
            ("\"s\"", TokenKind::String),
            ("12", TokenKind::Number),
            ("// c", TokenKind::Comment),
        ] {
            assert!(tokens.contains(&expected), "{expected:?} in {tokens:?}");
        }
    }

    #[test]
    fn an_alias_names_its_language() {
        assert!(!kinds("ts", "const a: number = 1;").is_empty());
        assert!(!kinds("py", "def a(): return \"b\"  # c").is_empty());
    }

    #[test]
    fn an_unknown_language_and_no_code_are_plain() {
        assert_eq!(highlight("brainfudge", "++++[>++<-]"), Some(Vec::new()));
        assert_eq!(highlight("rust", ""), Some(Vec::new()));
    }

    #[test]
    fn the_grammars_that_abort_are_not_built_in_and_their_code_is_plain() {
        for (name, code) in [
            ("yaml", "a:\n  - b\n  - c\n"),
            ("yml", "a: b\n"),
            ("bash", "echo \"$HOME\" # c\n"),
            ("sh", "cat <<EOF\nx\nEOF\n"),
            ("kotlin", "val a = \"${b}\"\n"),
        ] {
            assert_eq!(highlight(name, code), Some(Vec::new()), "{name}");
        }
    }

    #[test]
    fn a_script_injected_into_html_is_highlighted_as_its_language() {
        let tokens = kinds("html", "<p>x</p><script>const a = 1;</script>");
        assert!(
            tokens.contains(&("const", TokenKind::Keyword)),
            "{tokens:?}"
        );
        assert!(tokens.contains(&("1", TokenKind::Number)), "{tokens:?}");
    }

    #[test]
    fn code_past_the_limit_is_plain() {
        assert_eq!(highlight("rust", &"a\n".repeat(MAX_HIGHLIGHT_BYTES)), None);
    }

    /// `unit` repeated to at most `size` bytes.
    fn repeated(unit: &str, size: usize) -> String {
        unit.repeat((size / unit.len()).max(1))
    }

    /// The lines `line(n)` makes for n = 0, 1, …, to at most `size` bytes.
    fn growing(size: usize, line: impl Fn(usize) -> String) -> String {
        let mut out = String::new();
        for n in 0.. {
            let next = line(n);
            if out.len() + next.len() > size {
                break;
            }
            out.push_str(&next);
        }
        out
    }

    /**
     * Code built to find what a grammar is slow on, or aborts on, each `size` bytes at most:
     * deep nesting of each bracket, long runs of each operator and quote, growing indentation,
     * heredocs, string interpolation, raw strings, and long comments of each style.
     */
    fn hostile(size: usize) -> Vec<(String, String)> {
        let mut inputs: Vec<(String, String)> = Vec::new();
        for (open, close) in [("(", ")"), ("[", "]"), ("{", "}"), ("<", ">")] {
            inputs.push((format!("{open} open"), open.repeat(size)));
            inputs.push((
                format!("{open}{close} nested"),
                open.repeat(size / 2) + &close.repeat(size / 2),
            ));
            inputs.push((format!("a{open} open"), repeated(&format!("a{open}"), size)));
        }
        for unit in [
            "+", "-", "*", "/", "%", "=", "!", "&", "|", "^", "~", "?", ":", ";", ",", ".", "@",
            "#", "$", "\\", "'", "\"", "`", "=>", "->", "::", "<<", ">>", "..", "a.", "\n", "\t",
            "\r\n", "{}", "()", "[]", "- ", "* ", "> ", "1. ", "a: ", "? ", "| ", "&a ",
        ] {
            inputs.push((format!("{unit:?} run"), repeated(unit, size)));
        }
        for (name, line) in [
            ("indent a:", "a:"),
            ("indent - a", "- a"),
            ("indent if a:", "if a:"),
            ("indent x", "x"),
            ("indent - ", "- "),
            ("indent {", "{"),
        ] {
            inputs.push((
                format!("{name} spaces"),
                growing(size, |n| format!("{}{line}\n", " ".repeat(n))),
            ));
            inputs.push((
                format!("{name} tabs"),
                growing(size, |n| format!("{}{line}\n", "\t".repeat(n))),
            ));
            inputs.push((
                format!("{name} markers"),
                growing(size, |n| format!("{}{line}\n", "- ".repeat(n))),
            ));
        }
        for (name, unit) in [
            ("heredoc", "cat <<EOF\n"),
            ("heredoc -", "cat <<-EOF\n"),
            ("heredoc quoted", "cat <<'A'\n"),
            ("heredoc ruby", "a(<<~A)\n"),
            ("heredoc php", "<<<EOT\n"),
            ("heredocs nested", "$(cat <<A\n"),
            ("interpolation ${", "\"${"),
            ("interpolation #{", "\"#{"),
            ("interpolation `${", "`${"),
            ("interpolation f{", "f\"{"),
            ("interpolation ${ bare", "${"),
            ("interpolation $(", "$("),
            ("interpolation \\(", "\"\\("),
            ("interpolation s$", "s\"$a${"),
            ("interpolation $\"{", "$\"{"),
            ("interpolation @\"{", "@$\"{"),
            ("raw r#\"", "r#\""),
            ("raw r##\"", "r##\""),
            ("raw R\"(", "R\"("),
            ("raw @\"", "@\""),
            ("raw \"\"\"", "\"\"\""),
            ("raw '''", "'''"),
            ("raw [[", "[["),
            ("raw [==[", "[==["),
            ("comment /*", "/*"),
            ("comment /**", "/**"),
            ("comment /* */", "/* a */"),
            ("comment //", "//\n"),
            ("comment #", "#\n"),
            ("comment --", "--"),
            ("comment --[[", "--[["),
            ("comment <!--", "<!--"),
            ("comment {-", "{-"),
            ("comment (*", "(*"),
            ("comment ;", ";\n"),
            ("comment %", "%\n"),
            ("comment =begin", "=begin\n"),
            ("comment #|", "#|"),
            ("comment {#", "{#"),
            ("comment {{!--", "{{!--"),
            ("template {{", "{{"),
            ("template {%", "{%"),
            ("template <%", "<%"),
            ("template <?php", "<?php "),
            ("tag <a", "<a "),
            ("tag <a>", "<a>"),
            ("tag <script>", "<script>"),
            ("words", "fn a() { let b = c(d, \"e\"); } "),
        ] {
            inputs.push((name.to_string(), repeated(unit, size)));
        }
        inputs
    }

    /**
     * A pathological block of every language's at the largest size highlighted, abandoned or
     * finished within the budget: the inputs found slowest (a run of block comments' openers,
     * of `'`, of `{}`, nested brackets, interpolation). Over the deadline only by the longest
     * step tree-sitter takes between two checks of it in a segment, and by what an unoptimised
     * build or a busy machine adds.
     */
    #[test]
    fn a_pathological_block_of_any_language_is_given_up_on_in_time() {
        let slack = if cfg!(debug_assertions) {
            Duration::from_millis(150)
        } else {
            Duration::from_millis(30)
        };
        let languages = LanguageRegistry::singleton().languages();
        /* Compiled first, those injected into others too: once a process, not the code's. */
        for language in &languages {
            grammar(language);
        }
        let inputs: Vec<(String, String)> = hostile(MAX_HIGHLIGHT_BYTES)
            .into_iter()
            .filter(|(name, _)| {
                [
                    "comment /*",
                    "\"'\" run",
                    "\"{}\" run",
                    "{} nested",
                    "interpolation ${",
                    "words",
                ]
                .contains(&name.as_str())
            })
            .collect();
        let mut worst = (Duration::ZERO, String::new());
        for language in languages {
            if grammar(&language).is_none() {
                continue;
            }
            for (name, code) in &inputs {
                let start = Instant::now();
                highlight(&language, code);
                let took = start.elapsed();
                assert!(
                    took <= HIGHLIGHT_BUDGET + slack,
                    "{language} on {name} took {took:?}"
                );
                if took > worst.0 {
                    worst = (took, format!("{language} on {name}"));
                }
            }
        }
        eprintln!("worst: {} at {:?}", worst.1, worst.0);
    }

    /**
     * Every grammar over [`hostile`] code of a few sizes, each language in a process of its own,
     * so that one that aborts (an `assert` in a grammar's C code, which nothing can catch) is
     * found and named rather than taking the tests down. Minutes, so run by hand:
     * `cargo test --release -p sb_ui --lib grammars_never_abort -- --ignored --nocapture`.
     */
    #[test]
    #[ignore = "minutes: every grammar over hostile code, each in a process of its own"]
    fn grammars_never_abort() {
        const LANGUAGE: &str = "SB_SYNTAX_FUZZ_LANGUAGE";
        if let Ok(language) = std::env::var(LANGUAGE) {
            for size in [512, 4 * 1024, MAX_HIGHLIGHT_BYTES] {
                for (name, code) in hostile(size) {
                    eprintln!("case {name} at {size}");
                    highlight_within(&language, &code, Duration::from_secs(2));
                }
            }
            return;
        }
        let mut aborted = Vec::new();
        let mut languages = LanguageRegistry::singleton().languages();
        languages.sort();
        for language in languages {
            if grammar(&language).is_none() {
                continue;
            }
            let start = Instant::now();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "syntax::tests::grammars_never_abort",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(LANGUAGE, language.as_ref())
                .output()
                .unwrap();
            let log = String::from_utf8_lossy(&output.stderr);
            let cases = log.lines().filter(|line| line.starts_with("case ")).count();
            eprintln!(
                "{language:<16} {cases:>4} cases in {:>6.1}s: {}",
                start.elapsed().as_secs_f64(),
                output.status
            );
            if !output.status.success() {
                let last = log
                    .lines()
                    .rfind(|line| line.starts_with("case "))
                    .unwrap_or("");
                aborted.push(format!("{language} ({}) at {last}", output.status));
            }
        }
        assert!(aborted.is_empty(), "{aborted:#?}");
    }

    #[test]
    fn the_capture_that_starts_last_paints() {
        let deadline = Instant::now() + HIGHLIGHT_BUDGET;
        let painted = paint(
            vec![
                (0..10, Some(TokenKind::String)),
                (2..4, Some(TokenKind::Plain)),
                (2..4, None),
                (6..8, Some(TokenKind::Keyword)),
                (6..8, Some(TokenKind::Number)),
            ],
            deadline,
        );
        assert_eq!(
            painted,
            Some(vec![
                (0..2, TokenKind::String),
                (4..6, TokenKind::String),
                (6..8, TokenKind::Number),
                (8..10, TokenKind::String),
            ])
        );
    }
}
