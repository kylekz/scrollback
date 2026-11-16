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
use sb_core::transcript::{TodoState, ToolCall, ToolInput};

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
            let range = match (lines, total) {
                (Some(lines), Some((total, _))) => Some(format!(
                    "lines {}–{} of {}",
                    format::count(first),
                    format::count((first + lines.saturating_sub(1)).min(total)),
                    format::count(total)
                )),
                (None, Some((total, read))) => Some(format!(
                    "lines {}–{} of {}",
                    format::count(first),
                    format::count(first + read.saturating_sub(1)),
                    format::count(total)
                )),
                (Some(lines), None) => Some(format!(
                    "lines {}–{}",
                    format::count(first),
                    format::count(first + lines.saturating_sub(1))
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

/// The `chunk`th row of an edit's diff (DESIGN §4.2.4 "Edit", dv's diff rows): an edit's hunk
/// header first, then each line, 20 high, its number in a 36 gutter, a 16 `+` / `−` marker on
/// the gutter's fill, the line's fill, the words that changed marked, and its syntax over it.
pub fn diff_rows(
    diff: &Diff,
    chunk_index: usize,
    all: bool,
    written: bool,
    finds: &[(Range<usize>, bool)],
    theme: &SbTheme,
) -> AnyElement {
    let c = &theme.colors;
    let first = if written {
        FIRST_WRITTEN_LINES
    } else {
        FIRST_DIFF_LINES
    };
    let lines = chunk(chunk_index, all, first, diff.lines.len());
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
    div()
        .flex()
        .flex_col()
        .font_family(theme.fonts.mono.clone())
        .children(hunk)
        .children(diff.lines[lines].iter().map(|line| {
            let (fill, gutter, marker, word) = match line.kind {
                LineKind::Added => (Some(c.add_bg), Some(c.add_gut), "+", c.add_word),
                LineKind::Removed => (Some(c.del_bg), Some(c.del_gut), "−", c.del_word),
                LineKind::Context => (None, None, "", c.add_word),
            };
            let number = match line.kind {
                LineKind::Removed => line.old,
                _ => line.new,
            };
            let range = line.range.clone();
            let shown = &diff.text[range.clone()];
            let syntax = diff
                .marks
                .iter()
                .filter(|(it, _)| it.start < range.end && it.end > range.start)
                .map(|(it, mark)| {
                    (
                        it.start.max(range.start) - range.start
                            ..it.end.min(range.end) - range.start,
                        mark_style(*mark, theme),
                    )
                });
            let words = line.words.iter().map(|it| {
                (
                    it.start - range.start..it.end - range.start,
                    HighlightStyle {
                        background_color: Some(word),
                        ..HighlightStyle::default()
                    },
                )
            });
            let marked: Vec<_> = gpui::combine_highlights(syntax, words).collect();
            let found: Vec<(Range<usize>, HighlightStyle)> = finds
                .iter()
                .filter(|(it, _)| it.start >= range.start && it.end <= range.end)
                .map(|(it, current)| {
                    (
                        it.start - range.start..it.end - range.start,
                        found_style(*current, theme),
                    )
                })
                .collect();
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
                        .child(marker),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .pl(px(4.))
                        .text_color(c.fg)
                        .text_spec(&theme.text.code)
                        .child(
                            StyledText::new(clean(shown).into_owned()).with_highlights(
                                highlights
                                    .into_iter()
                                    .filter(|(it, _)| shown.get(it.clone()).is_some())
                                    .collect::<Vec<_>>(),
                            ),
                        ),
                )
        }))
        .into_any_element()
}

/**
 * A line of a diff as drawn: a tab as four spaces and a bidirectional control as `?`, each the
 * width of what it stands for in the line's bytes, so the marks found in the line still fall
 * where they were.
 */
fn clean(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.contains(|c: char| c == '\t' || crate::hostile::is_bidi_control(c)) {
        return std::borrow::Cow::Borrowed(line);
    }
    std::borrow::Cow::Owned(
        line.chars()
            .map(|c| match c {
                '\t' => ' '.to_string(),
                c if crate::hostile::is_bidi_control(c) => "?".repeat(c.len_utf8()),
                c => c.to_string(),
            })
            .collect(),
    )
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
    let first = if written {
        FIRST_WRITTEN_LINES
    } else {
        FIRST_DIFF_LINES
    };
    let more = diff.lines.len().saturating_sub(first);
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
                    format!("··· {} more lines", format::count(more as u64))
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
        .when(!all, |this| {
            this.child(format!("+{} more lines", format::count(more as u64)))
        })
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

/// One of a web search's results (DESIGN §4.2.4): its title in `accent-text`, where it is in
/// mono 11 `fg3`.
pub fn result(title: &str, url: &str, theme: &SbTheme) -> AnyElement {
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
                .child(hostile::label(title)),
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

/// A piece of a web fetch's result (DESIGN §4.2.4): Markdown at 13/20, or its text as it is.
pub fn fetched(
    view: &mut ReaderView,
    ix: usize,
    at: &BlockRef,
    body: &CallBody,
    index: usize,
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
            .child(styled(text, 0..text.line_count(), &[], theme))
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
#[allow(clippy::too_many_arguments)]
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
