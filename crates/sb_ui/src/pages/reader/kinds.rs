//! The tool renderers (DESIGN §4.2.4): what a call's body draws by its tool's kind, a row at a
//! time, from the body as the reader service shaped it ([`crate::services::reader::shape`]).
//! The renderer comes from the call's kind and the name shown from its provider (PLAN §7.4); no
//! agent is named here.
//!
//! Text is drawn with its marks (a terminal's colours, a file's syntax and numbers, what a
//! search found) and Find's matches over them ([`styled`]).

use std::ops::Range;

use gpui::{
    AnyElement, ClickEvent, Context, FontWeight, HighlightStyle, InteractiveElement as _,
    IntoElement, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    StyledText, UnderlineStyle, div, prelude::FluentBuilder as _, px,
};
use sb_core::transcript::{TodoState, ToolCall, ToolInput, ToolKind};

use super::rows::{CHUNK_LINES, FIRST_DIFF_LINES, FIRST_LINES, FIRST_WRITTEN_LINES};
use super::{Piece, ReaderView};
use crate::automation::AutomationTargetExt as _;
use crate::components::{icon, icon_button};
use crate::format;
use crate::hostile;
use crate::icons::SbIcon;
use crate::markdown::{self, Size};
use crate::services::ActiveServices as _;
use crate::services::reader::shape::diff::{Diff, LineKind};
use crate::services::reader::text::{Ansi, Mark, Text};
use crate::services::reader::{BlockRef, CallBody, Part, Piece as ProsePiece, Toggle};
use crate::theme::{SbTheme, StyledText as _};

/// The lines of the `chunk`th row of `count` lines: the first `first` until all are shown,
/// then [`CHUNK_LINES`] a row.
pub fn chunk(chunk: usize, all: bool, first: usize, count: usize) -> Range<usize> {
    if all {
        chunk * CHUNK_LINES..((chunk + 1) * CHUNK_LINES).min(count)
    } else {
        0..first.min(count)
    }
}

/// How a mark draws (DESIGN §1.1): a token's syntax colour, a terminal's colour as the theme's,
/// a line's number in `fg3`, a file in `fg`, what a search found in `hl`.
fn mark_style(mark: Mark, theme: &SbTheme) -> HighlightStyle {
    let c = &theme.colors;
    let color = |color| HighlightStyle {
        color: Some(color),
        ..HighlightStyle::default()
    };
    match mark {
        Mark::Token(kind) => kind
            .color(&c.syntax)
            .map_or_else(HighlightStyle::default, color),
        Mark::Ansi(ansi) => color(match ansi {
            Ansi::Red => c.red,
            Ansi::Green => c.green,
            Ansi::Yellow => c.yellow,
            Ansi::Blue => c.blue,
            Ansi::Dim => c.fg3,
            Ansi::Bright => c.fg,
        }),
        Mark::Number => color(c.fg3),
        Mark::File => HighlightStyle {
            color: Some(c.fg),
            font_weight: Some(FontWeight::MEDIUM),
            ..HighlightStyle::default()
        },
        Mark::Found => found_style(false, theme),
    }
}

/// A match (`hl`: `yellow-soft` with a 1px `yellow` underline), or the current one (`hl.cur`:
/// `accent` with `accent-ink` text).
pub fn found_style(current: bool, theme: &SbTheme) -> HighlightStyle {
    let c = &theme.colors;
    if current {
        HighlightStyle {
            background_color: Some(c.accent),
            color: Some(c.accent_ink),
            ..HighlightStyle::default()
        }
    } else {
        HighlightStyle {
            background_color: Some(c.yellow_soft),
            underline: Some(UnderlineStyle {
                thickness: px(1.),
                color: Some(c.yellow),
                wavy: false,
            }),
            ..HighlightStyle::default()
        }
    }
}

/**
 * The lines `lines` of `text` drawn with its marks and with `finds` over them: Find's matches,
 * ranges of `text`, in order, apart, each with whether it is the current one.
 */
pub fn styled(
    text: &Text,
    lines: Range<usize>,
    finds: &[(Range<usize>, bool)],
    theme: &SbTheme,
) -> StyledText {
    let (drawn, marks) = text.draw(lines.clone(), &text.marks);
    let marks = marks
        .into_iter()
        .map(|(range, mark)| (range, mark_style(mark, theme)));
    let highlights: Vec<(Range<usize>, HighlightStyle)> = if finds.is_empty() {
        marks.collect()
    } else {
        let (_, found) = text.draw(lines, finds);
        gpui::combine_highlights(
            marks,
            found
                .into_iter()
                .map(|(range, current)| (range, found_style(current, theme))),
        )
        .collect()
    };
    StyledText::new(drawn).with_highlights(highlights)
}

/// What the header over a tool of no kind scrollback knows says (DESIGN §4.2.4).
pub const UNKNOWN_TOOL: &str = "Tool scrollback doesn't know yet, shown as sent and received";

/// A call body's own header (DESIGN §4.2.4 `callhd`, 26): what the kind says of the call, and
/// Copy where it copies something.
pub fn head(ix: usize, at: &BlockRef, call: &ToolCall, first: bool, theme: &SbTheme) -> AnyElement {
    let c = &theme.colors;
    let mono = |text: String, color| {
        div()
            .min_w_0()
            .overflow_hidden()
            .text_ellipsis()
            .text_spec(&theme.text.path_small)
            .text_color(color)
            .child(hostile::label(&text))
    };
    let said = |text: String| div().flex_none().child(text);
    let mut copied: Option<String> = None;
    let parts: Vec<AnyElement> = match &call.input {
        ToolInput::Shell { command, .. } => {
            copied = Some(hostile::bidi(command).into_owned());
            vec![mono(format!("$ {command}"), c.fg2).into_any_element()]
        }
        ToolInput::Read {
            path,
            first_line,
            lines,
        } => {
            let total = match call
                .result
                .as_ref()
                .and_then(|result| result.brief.as_ref())
            {
                Some(sb_core::transcript::Brief::Lines { total, lines: read }) => {
                    total.map(|total| (total, *read))
                }
                _ => None,
            };
            let first = first_line.unwrap_or(1);
            /* The numbers are the transcript's: added without overflowing. */
            let last = |lines: u64| first.saturating_add(lines.saturating_sub(1));
            let range = match (lines, total) {
                (Some(lines), Some((total, _))) => Some(format!(
                    "lines {}–{} of {}",
                    format::count(first),
                    format::count(last(*lines).min(total)),
                    format::count(total)
                )),
                (None, Some((total, read))) => Some(format!(
                    "lines {}–{} of {}",
                    format::count(first),
                    format::count(last(read)),
                    format::count(total)
                )),
                (Some(lines), None) => Some(format!(
                    "lines {}–{}",
                    format::count(first),
                    format::count(last(*lines))
                )),
                (None, None) => None,
            };
            copied = Some(hostile::bidi(path).into_owned());
            std::iter::once(mono(path.clone(), c.fg2).into_any_element())
                .chain(range.map(|range| said(range).into_any_element()))
                .collect()
        }
        ToolInput::ContentSearch {
            pattern,
            path,
            mode,
            ..
        } => {
            let mut parts = vec![mono(pattern.clone(), c.fg2).into_any_element()];
            parts.extend([path, mode].into_iter().flatten().flat_map(|it| {
                [
                    said("·".to_string()).into_any_element(),
                    mono(it.clone(), c.fg3).into_any_element(),
                ]
            }));
            parts
        }
        ToolInput::WebFetch { prompt, .. } => vec![
            said("prompt".to_string()).into_any_element(),
            mono(prompt.clone().unwrap_or_default(), c.fg2).into_any_element(),
        ],
        ToolInput::Mcp { server, .. } => vec![
            mono(call.name.clone(), c.fg2).into_any_element(),
            said(format!("server {}", hostile::label(server))).into_any_element(),
        ],
        _ if call.kind == ToolKind::Other => vec![
            div()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .child(UNKNOWN_TOOL)
                .into_any_element(),
        ],
        _ => vec![mono(call.name.clone(), c.fg2).into_any_element()],
    };
    div()
        .flex()
        .items_center()
        .gap(theme.spacing.md)
        .h(theme.sizes.call_header)
        .px(px(10.))
        .bg(c.codehead)
        .border_b_1()
        .when(!first, |this| this.border_t_1())
        .border_color(c.line_subtle)
        .text_color(c.fg3)
        .text_spec(&theme.text.small)
        .whitespace_nowrap()
        .children(parts)
        .children(copied.map(|copied| {
            icon_button(
                SharedString::from(format!("copy-head-{ix}")),
                theme.sizes.small_button,
                theme,
            )
            .ml_auto()
            .aria_label("Copy")
            .on_click(move |_, _, cx| crate::system::copy_text(copied.clone(), cx))
            .child(icon(SbIcon::Copy, theme.sizes.icon_xs, c.fg2))
        }))
        .automation_target_with({
            let at = at.clone();
            move || format!("reader/head/{}", at.block.0).into()
        })
}

/**
 * What the `chunk`th row of an edit's diff draws, as lines of its text drawn
 * ([`DiffLine::drawn`]): its first lines, at most [`FIRST_DIFF_LINES`] (a write's
 * [`FIRST_WRITTEN_LINES`]) and [`CHUNK_LINES`] drawn, until all are shown; then
 * [`CHUNK_LINES`] drawn a row, so that a line of a megabyte is drawn over many rows.
 */
pub fn diff_chunk(diff: &Diff, chunk: usize, all: bool, written: bool) -> Range<usize> {
    if all {
        return chunk * CHUNK_LINES..((chunk + 1) * CHUNK_LINES).min(diff.drawn);
    }
    let first = if written {
        FIRST_WRITTEN_LINES
    } else {
        FIRST_DIFF_LINES
    };
    let end = first
        .min(diff.lines.len())
        .checked_sub(1)
        .map_or(0, |last| diff.lines[last].drawn.end);
    0..end.min(CHUNK_LINES)
}

/// The rows of a diff past its first: how many lines it has past them, and how many drawn.
pub fn diff_rest(diff: &Diff, written: bool) -> (usize, bool) {
    let first = diff_chunk(diff, 0, false, written);
    let lines = diff
        .lines
        .partition_point(|line| line.drawn.end <= first.end);
    (diff.lines.len() - lines, first.end < diff.drawn)
}

/// `marks` of a text in order and apart, as highlights of the drawn lines `lines` of `text`.
fn drawn<M: Clone>(
    text: &Text,
    lines: Range<usize>,
    marks: &[(Range<usize>, M)],
    style: impl Fn(M) -> HighlightStyle,
) -> (String, Vec<(Range<usize>, HighlightStyle)>) {
    let (drawn, marks) = text.draw(lines, marks);
    let styled = marks
        .into_iter()
        .map(|(range, mark)| (range, style(mark)))
        .collect();
    (drawn, styled)
}

/// The `chunk`th row of an edit's diff (DESIGN §4.2.4 "Edit", dv's diff rows): an edit's hunk
/// header first, then each line, 20 high, its number in a 36 gutter, a 16 `+` / `−` marker on
/// the gutter's fill, the line's fill, the words that changed marked, and its syntax over it.
/// A line drawn over several rows has its number and marker on its first.
pub fn diff_rows(
    diff: &Diff,
    chunk_index: usize,
    all: bool,
    written: bool,
    finds: &[(Range<usize>, bool)],
    theme: &SbTheme,
) -> AnyElement {
    let c = &theme.colors;
    let shown = diff_chunk(diff, chunk_index, all, written);
    let (removed, added) = diff.counts();
    let hunk = (chunk_index == 0 && !written).then(|| {
        hunk_row(
            format!(
                "@@ −{},{} +{},{} @@",
                diff.start.0,
                diff.lines.len() - added,
                diff.start.1,
                diff.lines.len() - removed
            ),
            theme,
        )
    });
    let from = diff
        .lines
        .partition_point(|line| line.drawn.end <= shown.start);
    let to = diff
        .lines
        .partition_point(|line| line.drawn.start < shown.end);
    let rows = diff.lines[from..to.max(from)].iter().map(|line| {
        let (fill, gutter, marker, word) = match line.kind {
            LineKind::Added => (Some(c.add_bg), Some(c.add_gut), "+", c.add_word),
            LineKind::Removed => (Some(c.del_bg), Some(c.del_gut), "−", c.del_word),
            LineKind::Context => (None, None, "", c.add_word),
        };
        let opens = line.drawn.start >= shown.start;
        let number = match line.kind {
            LineKind::Removed => line.old,
            _ => line.new,
        }
        .filter(|_| opens);
        /* The lines of the text this row draws of the line. */
        let lines = line.segments.start + shown.start.saturating_sub(line.drawn.start)
            ..line.segments.end - line.drawn.end.saturating_sub(shown.end);
        let words: Vec<(Range<usize>, ())> =
            line.words.iter().map(|word| (word.clone(), ())).collect();
        let (text, syntax) = drawn(&diff.text, lines.clone(), &diff.marks, |mark| {
            mark_style(mark, theme)
        });
        let (_, words) = drawn(&diff.text, lines.clone(), &words, |()| HighlightStyle {
            background_color: Some(word),
            ..HighlightStyle::default()
        });
        let (_, found) = drawn(&diff.text, lines, finds, |current| {
            found_style(current, theme)
        });
        let marked: Vec<_> = gpui::combine_highlights(syntax, words).collect();
        let highlights: Vec<_> = gpui::combine_highlights(marked, found).collect();
        div()
            .flex()
            .items_stretch()
            .min_h(px(20.))
            .when_some(fill, |this, fill| this.bg(fill))
            .child(
                div()
                    .flex_none()
                    .w(px(36.))
                    .pr(theme.spacing.sm)
                    .flex()
                    .justify_end()
                    .text_color(c.fg3)
                    .text_spec(&theme.text.code)
                    .when_some(gutter, |this, gutter| this.bg(gutter))
                    .children(number.map(|number| number.to_string())),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(16.))
                    .flex()
                    .justify_center()
                    .text_color(c.fg2)
                    .text_spec(&theme.text.code)
                    .when_some(gutter, |this, gutter| this.bg(gutter))
                    .children(opens.then_some(marker)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pl(px(4.))
                    .text_color(c.fg)
                    .text_spec(&theme.text.code)
                    .child(StyledText::new(text).with_highlights(highlights)),
            )
    });
    div()
        .flex()
        .flex_col()
        .font_family(theme.fonts.mono.clone())
        .children(hunk)
        .children(rows)
        .into_any_element()
}

/// A hunk row (`hunk` fill, its text in `blue`).
pub fn hunk_row(text: String, theme: &SbTheme) -> AnyElement {
    div()
        .flex()
        .items_center()
        .h(px(20.))
        .px(px(10.))
        .bg(theme.colors.hunk)
        .text_color(theme.colors.blue)
        .text_spec(&theme.text.code)
        .child(text)
        .into_any_element()
}

/// Under a diff with more lines: an edit's "+n more lines · Show all", a write's hunk row
/// "··· n more lines"; Show fewer once all are shown.
pub fn diff_more(
    ix: usize,
    at: &BlockRef,
    diff: &Diff,
    all: bool,
    written: bool,
    theme: &SbTheme,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let c = &theme.colors;
    let more = match diff_rest(diff, written) {
        (0, _) => "+ the rest of a long line".to_string(),
        (lines, _) => format!("+{} more lines", format::count(lines as u64)),
    };
    let toggle = Toggle::AllLines(at.clone(), None);
    let click = cx.listener(move |_, _: &ClickEvent, _, cx| {
        let reader = cx.services().reader.clone();
        let toggle = toggle.clone();
        reader.update(cx, |reader, cx| reader.toggle(toggle, cx));
    });
    if written {
        return div()
            .id(("diff-more", ix))
            .cursor_pointer()
            .on_click(click)
            .child(hunk_row(
                if all {
                    "Show fewer".to_string()
                } else {
                    format!("··· {}", more.trim_start_matches('+'))
                },
                theme,
            ))
            .into_any_element();
    }
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .h(theme.sizes.call_footer)
        .px(px(10.))
        .border_t_1()
        .border_color(c.line_subtle)
        .text_color(c.fg3)
        .text_spec(&theme.text.small)
        .when(!all, |this| this.child(more))
        .child(
            div()
                .id(("diff-more", ix))
                .ml_auto()
                .text_color(c.accent_text)
                .cursor_pointer()
                .on_click(click)
                .child(if all { "Show fewer" } else { "Show all" }),
        )
        .into_any_element()
}

/// One todo (DESIGN §4.2.4 "Todos"): on `s1`, a 24 row with a 14 checkbox: done (filled
/// `accent`, a check, struck in `fg3`), in progress (an `accent` outline round a 6 `accent`
/// square), open (an `fg3` outline).
pub fn todo(text: &str, state: TodoState, first: bool, last: bool, theme: &SbTheme) -> AnyElement {
    let c = &theme.colors;
    let box_ = div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(px(14.))
        .rounded(px(3.))
        .border(px(1.5));
    let check = match state {
        TodoState::Done => box_.border_color(c.accent).bg(c.accent).child(icon(
            SbIcon::Check,
            px(10.),
            c.accent_ink,
        )),
        TodoState::InProgress => box_
            .border_color(c.accent)
            .child(div().size(px(6.)).rounded(px(1.)).bg(c.accent)),
        TodoState::Open => box_.border_color(c.fg3),
    };
    div()
        .flex()
        .items_center()
        .gap(theme.spacing.md)
        .h(px(24.))
        .px(px(10.))
        .bg(c.s1)
        .when(first, |this| this.mt(theme.spacing.xs).pt(theme.spacing.xs))
        .when(last, |this| this.pb(theme.spacing.xs))
        .text_spec(&theme.text.secondary)
        .child(check)
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_color(if state == TodoState::Done {
                    c.fg3
                } else {
                    c.fg
                })
                .when(state == TodoState::Done, |this| this.line_through())
                .child(hostile::label(text)),
        )
        .into_any_element()
}

/// One of a web search's results (DESIGN §4.2.4): its title in `accent-text` with Find's
/// matches in it, where it is in mono 11 `fg3`.
pub fn result(
    title: &str,
    url: &str,
    finds: &[(Range<usize>, bool)],
    theme: &SbTheme,
) -> AnyElement {
    let c = &theme.colors;
    let place = url
        .split_once("://")
        .map_or(url, |(_, rest)| rest)
        .trim_end_matches('/');
    div()
        .flex()
        .flex_col()
        .py(theme.spacing.xs)
        .px(px(10.))
        .bg(c.s1)
        .child(
            div()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_color(c.accent_text)
                .text_spec(&theme.text.secondary)
                .child({
                    let ranges: Vec<Range<usize>> =
                        finds.iter().map(|(range, _)| range.clone()).collect();
                    let (label, ranges) = hostile::label_ranges(title, &ranges);
                    let highlights: Vec<_> = ranges
                        .into_iter()
                        .zip(finds)
                        .filter(|(range, _)| !range.is_empty())
                        .map(|(range, (_, current))| (range, found_style(*current, theme)))
                        .collect();
                    StyledText::new(label).with_highlights(highlights)
                }),
        )
        .child(
            div()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_color(c.fg3)
                .text_spec(&theme.text.path_small)
                .child(hostile::label(place)),
        )
        .into_any_element()
}

/// Under a web search's first results: how many more, and Show all; or Show fewer.
pub fn results_more(
    ix: usize,
    at: &BlockRef,
    more: usize,
    all: bool,
    theme: &SbTheme,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let c = &theme.colors;
    let toggle = Toggle::AllLines(at.clone(), None);
    div()
        .flex()
        .items_center()
        .h(theme.sizes.call_footer)
        .px(px(10.))
        .bg(c.s1)
        .text_color(c.fg3)
        .text_spec(&theme.text.small)
        .when(!all, |this| {
            this.child(format!("+{} more results", format::count(more as u64)))
        })
        .child(
            div()
                .id(("results-more", ix))
                .ml_auto()
                .text_color(c.accent_text)
                .cursor_pointer()
                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                    let reader = cx.services().reader.clone();
                    let toggle = toggle.clone();
                    reader.update(cx, |reader, cx| reader.toggle(toggle, cx));
                }))
                .child(if all { "Show fewer" } else { "Show all" }),
        )
        .into_any_element()
}

/**
 * A row of a web fetch's result (DESIGN §4.2.4): a piece of Markdown at 13/20, or the `chunk`th
 * row of a piece shown as text, [`CHUNK_LINES`] lines a row, with Find's matches `finds`.
 */
#[allow(clippy::too_many_arguments)]
pub fn fetched(
    view: &mut ReaderView,
    ix: usize,
    at: &BlockRef,
    body: &CallBody,
    index: usize,
    chunk_index: usize,
    finds: &[(Range<usize>, bool)],
    theme: &SbTheme,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let piece = body
        .fetched
        .as_ref()
        .and_then(|prose| prose.pieces.get(index));
    let inner = match piece {
        Some(ProsePiece::Markdown(sanitized)) => {
            let sanitized = sanitized.clone();
            view.markdown_state(
                Piece {
                    at: at.clone(),
                    piece: index,
                },
                ix,
                || Some(sanitized),
                cx,
            )
            .map(|state| {
                markdown::view(&state, Size::Nested, cx)
                    .w_full()
                    .into_any_element()
            })
            .unwrap_or_else(|| div().into_any_element())
        }
        Some(ProsePiece::Code { text, .. } | ProsePiece::Plain { text, .. }) => div()
            .text_color(theme.colors.fg2)
            .text_spec(&theme.text.code)
            .child(styled(
                text,
                chunk(chunk_index, true, CHUNK_LINES, text.line_count()),
                finds,
                theme,
            ))
            .into_any_element(),
        None => div().into_any_element(),
    };
    div()
        .px(theme.spacing.lg)
        .py(theme.spacing.sm)
        .child(inner)
        .into_any_element()
}

/**
 * The `chunk`th row of an MCP call's input and its output side by side (DESIGN §4.2.4), each
 * labelled on the first; below 560 of the column's width they stack, as each column is at
 * least half that.
 */
pub fn pair(
    body: &CallBody,
    chunk_index: usize,
    all: bool,
    finds: [&[(Range<usize>, bool)]; 2],
    theme: &SbTheme,
) -> AnyElement {
    let c = &theme.colors;
    let output = body.output.iter().find_map(|part| match part {
        Part::Text(Ok(text)) => Some(text.as_ref()),
        _ => None,
    });
    let column = |label: &'static str, text: Option<&Text>, finds: &[(Range<usize>, bool)]| {
        let lines = text.map_or(0..0, |text| {
            chunk(chunk_index, all, FIRST_LINES, text.line_count())
        });
        div()
            .flex_1()
            .flex_basis(px(270.))
            .min_w(px(270.))
            .flex()
            .flex_col()
            .gap(theme.spacing.xs)
            .px(theme.spacing.lg)
            .when(chunk_index == 0, |this| {
                this.pt(theme.spacing.md).child(
                    div()
                        .text_color(c.fg3)
                        .text_spec(&theme.text.small)
                        .child(label),
                )
            })
            .text_color(c.fg2)
            .text_spec(&theme.text.code)
            .children(text.map(|text| styled(text, lines, finds, theme)))
    };
    div()
        .flex()
        .flex_wrap()
        .pb(theme.spacing.xs)
        .child(column(
            "input",
            body.input.as_ref().ok().map(AsRef::as_ref),
            finds[0],
        ))
        .child(column("output", output, finds[1]))
        .into_any_element()
}
