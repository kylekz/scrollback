//! The reader's rows of prose (DESIGN §4.2.2, §4.2.5, §4.2.6): a prompt's header, a task's
//! box, the pieces of a block's prose, a prompt's images, the agent's header over what it
//! wrote, thoughts, notes, compactions, records the provider didn't know, and how a
//! conversation ended.

use std::sync::Arc;

use gpui::{
    AnyElement, ClickEvent, Context, ElementId, ImageSource, InteractiveElement as _, IntoElement,
    ObjectFit, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    StyledImage as _, Window, div, img, prelude::FluentBuilder as _, px,
};
use sb_core::transcript::Content;

use super::rows::{CHUNK_LINES, Ended, FIRST_LINES, PanelPart, ProsePart, Row, RowKind};
use super::{Piece, ReaderView, message_text};
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{
    BANNER_GAP, Tone, bad_banner, focus_ring, icon, icon_button, overline, tag,
};
use crate::format;
use crate::hostile;
use crate::icons::SbIcon;
use crate::markdown::{self, Size};
use crate::services::ActiveServices as _;
use crate::services::reader::text::expand_tabs;
use crate::services::reader::{
    BlockRef, Held, ImageKey, Load, OpenSession, Picture, Piece as ProsePiece, Slot, Text,
};
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _};

/// What a row is, for automation and the keyboard's place.
pub fn kind_name(kind: &RowKind) -> &'static str {
    match kind {
        RowKind::Turn(_) => "turn",
        RowKind::Prompt(_) => "prompt",
        RowKind::Task { .. } => "task",
        RowKind::Prose { reply: true, .. } => "reply",
        RowKind::Prose { .. } => "prose",
        RowKind::Images(_) => "images",
        RowKind::Author(_) => "author",
        RowKind::Thought { .. } => "thought",
        RowKind::Call { .. } => "call",
        RowKind::CallIo { .. } => "call-io",
        RowKind::Body { .. } => "body",
        RowKind::Panel { .. } => "panel",
        RowKind::Compaction { .. } => "compaction",
        RowKind::Note(_) => "note",
        RowKind::Unrecognised { .. } => "unrecognised",
        RowKind::Raw { .. } => "raw",
        RowKind::Ending { .. } => "ending",
        RowKind::NoConversation => "no-conversation",
        RowKind::UnattachedHead(_) => "unattached-head",
        RowKind::Unattached { .. } => "unattached",
        RowKind::Alone(_) => "alone",
    }
}

/// The name automation knows a row by: `kind conversation/block`, for its `top_name`.
pub fn row_name(row: &Row, open: &OpenSession) -> String {
    match row.block() {
        Some(at) => format!(
            "{} {}/{}",
            kind_name(&row.kind),
            conversation_name(&at.conversation, open),
            at.block.0
        ),
        None => kind_name(&row.kind).to_string(),
    }
}

/// The text of a row's block, for automation to assert on: a message's, a call's tool.
pub fn row_text(row: &Row, open: &OpenSession) -> String {
    let Some(at) = row.block() else {
        return String::new();
    };
    match open.block(at).map(|block| &block.content) {
        Some(
            Content::Prompt { text, .. } | Content::Reply { text } | Content::Thinking { text, .. },
        ) => hostile::label(text),
        Some(Content::ToolCall(call)) => hostile::label(&call.name),
        Some(Content::Note { text, .. }) => hostile::label(text),
        _ => String::new(),
    }
}

/// What a row the keyboard can rest on is, whether it is open or not.
#[derive(Debug, PartialEq, Eq)]
pub enum FocusKey<'a> {
    Block(&'static str, &'a BlockRef),
    More(&'a Arc<str>),
    Unattached(&'a Arc<str>),
}

pub fn focus_key(kind: &RowKind) -> Option<FocusKey<'_>> {
    match kind {
        RowKind::Prompt(at)
        | RowKind::Task { at, .. }
        | RowKind::Prose {
            at, reply: true, ..
        }
        | RowKind::Thought { at, .. }
        | RowKind::Call { at, .. }
        | RowKind::CallIo { at, .. }
        | RowKind::Compaction { at, .. }
        | RowKind::Unrecognised { at, .. } => Some(FocusKey::Block(kind_name(kind), at)),
        RowKind::Panel {
            item,
            part: PanelPart::More(_),
            ..
        } => Some(FocusKey::More(item)),
        RowKind::Unattached { item, .. } => Some(FocusKey::Unattached(item)),
        _ => None,
    }
}

/**
 * The name automation knows a row of a block by: `reader/<kind>/<conversation>/<block>`, the
 * conversation `main` for the session's own and the subagent's item's file stem for one of its
 * subagents. Fixtures make those stems, so a scenario can name them.
 */
pub fn target(kind: &str, at: &BlockRef, open: &OpenSession) -> SharedString {
    format!(
        "reader/{kind}/{}/{}",
        conversation_name(&at.conversation, open),
        at.block.0
    )
    .into()
}

pub fn conversation_name<'a>(item: &'a str, open: &OpenSession) -> &'a str {
    if open.main().is_some_and(|main| *main.item == *item) {
        return "main";
    }
    file_stem(item)
}

/**
 * The name a conversation's item goes by in automation and in the controls built for it: the
 * last part of its path less an extension it ends with (`.jsonl`). Whatever else the name
 * holds stays, so that conversations kept in one file are named apart.
 */
pub fn file_stem(item: &str) -> &str {
    let file = item.rsplit('/').next().unwrap_or(item);
    match file.rsplit_once('.') {
        Some((stem, extension))
            if !stem.is_empty() && extension.bytes().all(|it| it.is_ascii_alphanumeric()) =>
        {
            stem
        }
        _ => file,
    }
}

/// A click on a row: the keyboard goes to it, and what it opens opens or closes.
pub fn on_toggle(
    ix: usize,
    row: &Row,
    cx: &mut Context<ReaderView>,
) -> impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static {
    let row = row.clone();
    cx.listener(move |this, _: &ClickEvent, _, cx| {
        this.focused = Some((ix, row.clone()));
        this.toggle(&row, cx);
    })
}

pub fn render(
    view: &mut ReaderView,
    ix: usize,
    row: &Row,
    focused: bool,
    _: &mut Window,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let theme = cx.sb_theme().clone();
    let c = &theme.colors;
    match &row.kind {
        RowKind::Turn(_) => div().h(px(1.)).bg(c.line_subtle).into_any_element(),
        RowKind::Prompt(at) => prompt_head(ix, at, focused, &theme, cx),
        RowKind::Task { at, from } => task_head(ix, at, from.as_ref(), focused, &theme, cx),
        RowKind::Prose {
            at,
            part,
            boxed,
            last,
            reply,
        } => {
            let size = if row.depth > 0 || *boxed {
                Size::Nested
            } else {
                Size::Reading
            };
            let content = prose(view, ix, at, part, size, *reply, focused, &theme, cx);
            if *boxed {
                div()
                    .px(px(10.))
                    .bg(c.s2)
                    .border_l_1()
                    .border_r_1()
                    .border_color(c.line_subtle)
                    .when(*last, |this| {
                        this.pb(theme.spacing.md)
                            .border_b_1()
                            .rounded_b(theme.radii.row)
                    })
                    .child(content)
                    .into_any_element()
            } else {
                content
            }
        }
        RowKind::Images(at) => images(view, ix, at, &theme, cx),
        RowKind::Author(at) => author(at, &theme, cx),
        RowKind::Thought { at, open } => thought(ix, row, at, *open, focused, &theme, cx),
        RowKind::Compaction { at, open } => compaction(ix, row, at, *open, focused, &theme, cx),
        RowKind::Note(at) => note(at, &theme, cx),
        RowKind::Unrecognised { at, open } => unrecognised(ix, row, at, *open, focused, &theme, cx),
        RowKind::Raw { at, .. } => raw(at, &theme, cx),
        RowKind::Ending {
            conversation,
            ending,
        } => ending_row(conversation, ending, &theme, cx),
        RowKind::NoConversation => div()
            .text_color(c.fg2)
            .child(
                "This session has no conversation of its own: its items hold no main \
                 transcript, only what went with one.",
            )
            .automation_target("reader/no-conversation"),
        RowKind::UnattachedHead(count) => match count {
            Some(count) => overline(
                &format!("Subagents not placed under a call · {count}"),
                &theme,
            )
            .into_any_element(),
            None => {
                let reader = cx.services().reader.clone();
                reader.update(cx, |reader, cx| reader.want_placement(cx));
                skeleton(2, &theme)
            }
        },
        RowKind::Alone(_) => alone(&theme, cx),
        RowKind::Call { .. }
        | RowKind::CallIo { .. }
        | RowKind::Body { .. }
        | RowKind::Panel { .. }
        | RowKind::Unattached { .. } => div().into_any_element(),
    }
}

/// The ring of the keyboard's row, on `element`, which reaches 6 past its content on either
/// side so the ring doesn't touch it. A ring is a shadow, drawn under the element: the element
/// is filled so that only its edge shows.
fn ringed<E: gpui::Styled>(element: E, focused: bool, theme: &SbTheme) -> E {
    let element = element.mx(-theme.spacing.sm).px(theme.spacing.sm);
    if focused {
        element
            .bg(theme.colors.bg_app)
            .shadow(focus_ring(true, theme))
    } else {
        element
    }
}

/// A Copy button for the message at `at`, which shows while the pointer is over the message
/// (the group `group`) or the keyboard is on it. The text is read when it is clicked.
fn copy_button(
    id: ElementId,
    group: SharedString,
    shown: bool,
    at: &BlockRef,
    theme: &SbTheme,
) -> AnyElement {
    let at = at.clone();
    icon_button(id, theme.sizes.small_button, theme)
        .aria_label("Copy message")
        .when(!shown, |this| {
            this.opacity(0.)
                .group_hover(group, |style| style.opacity(1.))
        })
        .on_click(move |_, _, cx| {
            let text = cx
                .services()
                .reader
                .read(cx)
                .open_session()
                .and_then(|open| message_text(open, &at));
            if let Some(text) = text {
                crate::system::copy_text(text, cx);
            }
        })
        .child(icon(SbIcon::Copy, theme.sizes.icon_xs, theme.colors.fg2))
        .into_any_element()
}

/**
 * Over a prompt (DESIGN §4.2.2 "You"): who asked and when, and Copy. A prompt from another
 * sender is labelled with that sender, never as You.
 */
fn prompt_head(
    ix: usize,
    at: &BlockRef,
    focused: bool,
    theme: &SbTheme,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let c = &theme.colors;
    let (from, time, name) = {
        let reader = cx.services().reader.read(cx);
        let Some(open) = reader.open_session() else {
            return div().into_any_element();
        };
        let Some(block) = open.block(at) else {
            return div().into_any_element();
        };
        let Content::Prompt { from, .. } = &block.content else {
            return div().into_any_element();
        };
        (
            from.as_deref().map(hostile::label),
            block.at,
            target("prompt", at, open),
        )
    };
    let group: SharedString = format!("message-{ix}").into();
    ringed(
        div()
            .id(("prompt", ix))
            .group(group.clone())
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .rounded(theme.radii.control)
            .text_color(c.fg3)
            .text_spec(&theme.text.secondary),
        focused,
        theme,
    )
    .child(
        div()
            .text_color(c.fg)
            .text_spec(&theme.text.author)
            .child(from.clone().unwrap_or_else(|| "You".to_string())),
    )
    .when(from.is_some(), |this| {
        this.child(tag("not you", Tone::Neutral, true, theme))
    })
    .children(time.map(|time| {
        div()
            .text_spec(&theme.text.number)
            .child(format::clock(time))
    }))
    .child(div().flex_1())
    .child(copy_button(
        ("copy-prompt", ix).into(),
        group,
        focused,
        at,
        theme,
    ))
    .automation_target(name)
}

/// The top of a subagent's task box (DESIGN §4.2.7 `userbox`): "Task from" the model that gave
/// it. Its prose rows go on in the box.
fn task_head(
    ix: usize,
    at: &BlockRef,
    from: Option<&Arc<str>>,
    focused: bool,
    theme: &SbTheme,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let c = &theme.colors;
    let (provider, name) = {
        let reader = cx.services().reader.read(cx);
        let Some(open) = reader.open_session() else {
            return div().into_any_element();
        };
        (open.id.provider.clone(), target("task", at, open))
    };
    let label = match from {
        Some(model) => hostile::label(&format!(
            "Task from {}",
            agents::model_name(&provider, model, cx)
        )),
        None => "Task".to_string(),
    };
    div()
        .id(("task", ix))
        .pt(theme.spacing.md)
        .px(px(10.))
        .bg(c.s2)
        .border_t_1()
        .border_l_1()
        .border_r_1()
        .border_color(c.line_subtle)
        .rounded_t(theme.radii.row)
        .when(focused, |this| this.shadow(focus_ring(false, theme)))
        .text_color(c.fg3)
        .text_spec(&theme.text.tag)
        .child(label)
        .automation_target(name)
}

/// A row of a block's prose (the module docs): a piece of Markdown, a note that a piece is
/// shown as plain text for its size, a chunk of plain text, and how many more lines it has.
#[allow(clippy::too_many_arguments)]
fn prose(
    view: &mut ReaderView,
    ix: usize,
    at: &BlockRef,
    part: &ProsePart,
    size: Size,
    reply: bool,
    focused: bool,
    theme: &SbTheme,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let c = &theme.colors;
    let reader = cx.services().reader.clone();
    let (prepared, thinking, of_reply, length, name) = {
        let reader = reader.read(cx);
        let Some(open) = reader.open_session() else {
            return div().into_any_element();
        };
        let block = open.block(at).map(|block| &block.content);
        (
            match open.prose(at) {
                Some(Load::Loaded(prose)) => Some(prose.clone()),
                _ => None,
            },
            matches!(block, Some(Content::Thinking { .. })),
            matches!(block, Some(Content::Reply { .. })),
            match block {
                Some(Content::Prompt { text, .. } | Content::Reply { text }) => text.len(),
                _ => 0,
            },
            target(if reply { "reply" } else { "prose" }, at, open),
        )
    };
    view.hold(Held::Prose(at.clone()), ix);
    let piece = |index: usize| prepared.as_ref().and_then(|prose| prose.pieces.get(index));
    let content = match part {
        ProsePart::Pending => {
            let rank = view.rank(ix);
            reader.update(cx, |reader, cx| reader.want_prose(at, rank, cx));
            /* About as high as the prose will be, so that little moves when it lands. */
            let lines = (length / 90).clamp(1, 16);
            div()
                .h(theme.text.reading.line_height * lines as f32)
                .child(skeleton(lines.min(3), theme))
                .into_any_element()
        }
        ProsePart::Failed(why) => failed(&format!("This couldn't be read: {why}"), theme),
        ProsePart::Markdown(index) => {
            let sanitized = match piece(*index) {
                Some(ProsePiece::Markdown(sanitized)) => Some(sanitized.clone()),
                _ => None,
            };
            view.markdown_state(
                Piece {
                    at: at.clone(),
                    piece: *index,
                },
                ix,
                || sanitized,
                cx,
            )
            .map(|state| markdown::view(&state, size, cx).w_full().into_any_element())
            .unwrap_or_else(|| div().into_any_element())
        }
        ProsePart::Code(index) => match piece(*index) {
            Some(ProsePiece::Code { language, text }) => {
                let code = expand_tabs(&text.lines(0..text.line_count())).into_owned();
                let highlights = view.highlights(
                    Piece {
                        at: at.clone(),
                        piece: *index,
                    },
                    ix,
                    language.as_deref(),
                    &code,
                    cx,
                );
                code_view(ix, language.as_deref(), code, &highlights, theme)
            }
            _ => div().into_any_element(),
        },
        ProsePart::Note(index) => {
            let lines = match piece(*index) {
                Some(ProsePiece::Plain { text, .. }) => text.line_count(),
                _ => 0,
            };
            div()
                .flex()
                .items_center()
                .gap(theme.spacing.sm)
                .text_color(c.fg3)
                .text_spec(&theme.text.secondary)
                .child(icon(SbIcon::FileText, theme.sizes.icon_xs, c.fg3))
                .child(format!(
                    "Shown as plain text, {}: too large or too deeply nested to lay out as \
                     Markdown.",
                    format::plural(lines, "line")
                ))
                .into_any_element()
        }
        ProsePart::Lines {
            piece: index,
            chunk,
        } => match piece(*index) {
            Some(ProsePiece::Plain { text, oversized }) => {
                let all = !oversized
                    || reader.read(cx).open_session().is_some_and(|open| {
                        open.is_open(
                            &crate::services::reader::Toggle::ProseLines(at.clone(), *index),
                            reader.read(cx).full_calls(),
                        )
                    });
                let lines = expand_tabs(&text_rows(text, *chunk, all)).into_owned();
                let spec = if thinking {
                    &theme.text.thought_open
                } else {
                    &theme.text.code
                };
                div()
                    .when(thinking, |this| this.pl(px(22.)).italic())
                    .text_color(c.fg2)
                    .text_spec(spec)
                    .child(lines)
                    .into_any_element()
            }
            _ => div().into_any_element(),
        },
        ProsePart::More { piece: index, all } => {
            let text = match piece(*index) {
                Some(ProsePiece::Plain { text, .. }) => Some(text.as_ref()),
                _ => None,
            };
            let toggle = crate::services::reader::Toggle::ProseLines(at.clone(), *index);
            more_lines(
                format!("prose-more-{ix}").into(),
                text,
                *all,
                toggle,
                theme,
                cx,
            )
        }
    };
    /* The Copy button's own column, at the right of every row of a reply: the reply's text and
    a code block's own Copy stop short of it, so that its pieces are all as wide. */
    let copy_column = theme.sizes.small_button + theme.spacing.xs;
    if !reply {
        return if of_reply {
            div().pr(copy_column).child(content).into_any_element()
        } else {
            content
        };
    }
    let group: SharedString = format!("message-{ix}").into();
    ringed(
        div()
            .id(("reply", ix))
            .group(group.clone())
            .relative()
            .rounded(theme.radii.control),
        focused,
        theme,
    )
    /* After `ringed`, whose padding it widens. */
    .pr(theme.spacing.sm + copy_column)
    .child(content)
    .child(
        div()
            .absolute()
            .top_0()
            .right(theme.spacing.sm)
            .child(copy_button(
                ("copy-reply", ix).into(),
                group,
                focused,
                at,
                theme,
            )),
    )
    .automation_target(name)
}

/**
 * A piece of fenced code as the reader draws it, not the Markdown view: a code box (DESIGN
 * §4.2.2 `codeblock`) with its language and Copy over its lines, coloured by `highlights`.
 */
fn code_view(
    ix: usize,
    language: Option<&str>,
    code: String,
    highlights: &[(std::ops::Range<usize>, crate::syntax::TokenKind)],
    theme: &SbTheme,
) -> AnyElement {
    let c = &theme.colors;
    let runs: Vec<(std::ops::Range<usize>, gpui::HighlightStyle)> = highlights
        .iter()
        .filter(|(range, _)| {
            code.is_char_boundary(range.start)
                && code.is_char_boundary(range.end)
                && range.end <= code.len()
        })
        .filter_map(|(range, kind)| {
            Some((
                range.clone(),
                gpui::HighlightStyle {
                    color: Some(kind.color(&theme.colors.syntax)?),
                    ..Default::default()
                },
            ))
        })
        .collect();
    let copied = code.clone();
    div()
        .flex()
        .flex_col()
        .my(theme.spacing.sm)
        .rounded(theme.radii.control)
        .border_1()
        .border_color(c.line)
        .bg(c.code)
        .overflow_hidden()
        .child(
            div()
                .flex()
                .items_center()
                .justify_end()
                .gap(px(8.))
                .h(theme.sizes.call_header)
                .px(px(10.))
                .bg(c.codehead)
                .border_b_1()
                .border_color(c.line_subtle)
                .text_color(c.fg3)
                .text_spec(&theme.text.small)
                .children(language.map(|language| {
                    div()
                        .font_family(theme.fonts.mono.clone())
                        .child(hostile::label(language))
                }))
                .child(
                    div()
                        .id(("code-copy", ix))
                        .cursor_pointer()
                        .hover(|this| this.text_color(c.fg2))
                        .on_click(move |_, _, cx| crate::system::copy_text(copied.clone(), cx))
                        .child("Copy"),
                ),
        )
        .child(
            div()
                .px(theme.spacing.lg)
                .py(theme.spacing.md)
                .text_color(c.fg)
                .text_spec(&theme.text.code_block)
                .child(gpui::StyledText::new(code).with_highlights(runs)),
        )
        .into_any_element()
}

/// The lines of the `chunk`th row of `text`: its first [`FIRST_LINES`] until all are shown,
/// then [`CHUNK_LINES`] a row.
pub fn text_rows(text: &Text, chunk: usize, all: bool) -> String {
    let count = text.line_count();
    if all {
        text.lines(chunk * CHUNK_LINES..((chunk + 1) * CHUNK_LINES).min(count))
    } else {
        text.lines(0..FIRST_LINES.min(count))
    }
}

/// Under lines that have more than their first: how many more (in bytes when `text` is only
/// the head of something longer), and Show all; or Show fewer once all are shown.
pub fn more_lines(
    id: SharedString,
    text: Option<&Text>,
    all: bool,
    toggle: crate::services::reader::Toggle,
    theme: &SbTheme,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let c = &theme.colors;
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .h(theme.sizes.call_footer)
        .text_color(c.fg3)
        .text_spec(&theme.text.small)
        .when(!all, |this| {
            this.child(match text {
                Some(Text {
                    rest: Some(rest), ..
                }) => format!("+{} more", format::file_size(*rest)),
                _ => format!(
                    "+{} more lines",
                    format::count(
                        text.map_or(0, Text::line_count).saturating_sub(FIRST_LINES) as u64
                    )
                ),
            })
        })
        .child(
            div()
                .id(id)
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

/// A prompt's images as thumbnails (DESIGN §4.2.2 `shot`: 200 × 116, a caption under it).
fn images(
    view: &mut ReaderView,
    ix: usize,
    at: &BlockRef,
    theme: &SbTheme,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let reader = cx.services().reader.clone();
    let shots: Vec<(ImageKey, Option<Load<Picture>>)> = {
        let reader = reader.read(cx);
        let Some(open) = reader.open_session() else {
            return div().into_any_element();
        };
        let count = match open.block(at).map(|block| &block.content) {
            Some(Content::Prompt { images, .. }) => images.len(),
            _ => 0,
        };
        (0..count)
            .map(|index| {
                let key = ImageKey {
                    at: at.clone(),
                    slot: Slot::Prompt(index),
                };
                let load = open.image(&key).cloned();
                (key, load)
            })
            .collect()
    };
    for (key, load) in &shots {
        view.hold(Held::Image(key.clone()), ix);
        if load.is_none() {
            reader.update(cx, |reader, cx| reader.want_image(key, cx));
        }
    }
    div()
        .flex()
        .flex_wrap()
        .gap(theme.spacing.md)
        .children(shots.into_iter().map(|(_, load)| shot(load, theme)))
        .into_any_element()
}

/// One thumbnail: the image, or why it can't be shown.
fn shot(image: Option<Load<Picture>>, theme: &SbTheme) -> AnyElement {
    let c = &theme.colors;
    let caption = match &image {
        Some(Load::Loaded(picture)) => format!(
            "{} × {} · {}",
            picture.width,
            picture.height,
            format::file_size(picture.bytes as u64)
        ),
        Some(Load::Failed(why)) => hostile::label(why),
        None | Some(Load::Loading) => "Loading…".to_string(),
    };
    let bad = matches!(image, Some(Load::Failed(_)));
    div()
        .flex_none()
        .flex()
        .flex_col()
        .w(theme.sizes.shot_width)
        .h(theme.sizes.shot_height)
        .rounded(theme.radii.row)
        .border_1()
        .border_color(c.line)
        .bg(c.s2)
        .overflow_hidden()
        .child(
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .bg(c.s3)
                .flex()
                .items_center()
                .justify_center()
                .children(match image {
                    Some(Load::Loaded(picture)) => Some(
                        img(ImageSource::Render(picture.image))
                            .absolute()
                            .inset_0()
                            .size_full()
                            .object_fit(ObjectFit::Cover)
                            .into_any_element(),
                    ),
                    Some(Load::Failed(_)) => {
                        Some(icon(SbIcon::CircleAlert, theme.sizes.icon, c.red).into_any_element())
                    }
                    _ => None,
                }),
        )
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(theme.spacing.sm)
                .h(theme.sizes.shot_caption)
                .px(theme.spacing.md)
                .border_t_1()
                .border_color(c.line_subtle)
                .text_color(if bad { c.red } else { c.fg3 })
                .text_spec(&theme.text.small)
                .child(icon(SbIcon::Image, theme.sizes.icon_xs, c.fg3))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(caption),
                ),
        )
        .into_any_element()
}

/// Over what the agent wrote: its mark, the model and the time (DESIGN §4.2.2 "Assistant").
fn author(at: &BlockRef, theme: &SbTheme, cx: &mut Context<ReaderView>) -> AnyElement {
    let c = &theme.colors;
    let reader = cx.services().reader.read(cx);
    let Some((open, block)) = reader
        .open_session()
        .and_then(|open| Some((open, open.block(at)?)))
    else {
        return div().into_any_element();
    };
    let provider = open.id.provider.clone();
    let model = block
        .model
        .as_deref()
        .map(|model| hostile::label(&agents::model_name(&provider, model, cx)));
    let time = block.at;
    div()
        .flex()
        .items_center()
        .gap(theme.spacing.md)
        .text_color(c.fg3)
        .text_spec(&theme.text.secondary)
        .child(agents::mark(&provider, theme.sizes.icon_sm, cx))
        .child(
            div()
                .text_color(c.fg)
                .text_spec(&theme.text.author)
                .child(model.unwrap_or_else(|| agents::name(&provider, cx))),
        )
        .children(time.map(|time| {
            div()
                .text_spec(&theme.text.number)
                .child(format::clock(time))
        }))
        .into_any_element()
}

/// A thought (DESIGN §4.2.5): one line until opened; opened, its text follows in rows.
fn thought(
    ix: usize,
    row: &Row,
    at: &BlockRef,
    open: bool,
    focused: bool,
    theme: &SbTheme,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let c = &theme.colors;
    let (start, duration, name) = {
        let reader = cx.services().reader.read(cx);
        let Some(session) = reader.open_session() else {
            return div().into_any_element();
        };
        let Some(Content::Thinking { text, duration }) =
            session.block(at).map(|block| &block.content)
        else {
            return div().into_any_element();
        };
        let start = text
            .lines()
            .find(|line| !line.trim().is_empty())
            .map(hostile::label)
            .unwrap_or_default();
        (start, *duration, target("thought", at, session))
    };
    let lead = match duration {
        Some(duration) => format!(
            "Thought for {}",
            format::duration(i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        ),
        None => "Thought".to_string(),
    };
    ringed(
        div()
            .id(("thought", ix))
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .h(theme.sizes.thought_row)
            .rounded(theme.radii.control)
            .cursor_pointer()
            .hover(|style| style.bg(c.s1)),
        focused,
        theme,
    )
    .on_click(on_toggle(ix, row, cx))
    .child(icon(SbIcon::Lightbulb, theme.sizes.icon_sm, c.fg3))
    .map(|this| {
        if open {
            this.child(
                div()
                    .text_color(c.fg2)
                    .text_spec(&theme.text.control_small)
                    .child(lead),
            )
            .child(icon(SbIcon::ChevronDown, theme.sizes.icon_xs, c.fg3))
        } else {
            this.child(
                div()
                    .min_w_0()
                    .flex_1()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_color(c.fg2)
                    .text_spec(&theme.text.thought)
                    .italic()
                    .child(format!("{lead} · {start}")),
            )
        }
    })
    .automation_target(name)
}

/// A compaction boundary (DESIGN §4.2.6): rules either side of when, and the tokens before and
/// after; Show summary opens the summary the agent went on with, in rows under it.
fn compaction(
    ix: usize,
    row: &Row,
    at: &BlockRef,
    open: bool,
    focused: bool,
    theme: &SbTheme,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let c = &theme.colors;
    let (time, before, after, has_summary, name) = {
        let reader = cx.services().reader.read(cx);
        let Some(session) = reader.open_session() else {
            return div().into_any_element();
        };
        let Some(block) = session.block(at) else {
            return div().into_any_element();
        };
        let Content::Compaction {
            tokens_before,
            tokens_after,
            summary,
        } = &block.content
        else {
            return div().into_any_element();
        };
        (
            block.at,
            *tokens_before,
            *tokens_after,
            summary.is_some(),
            target("compaction", at, session),
        )
    };
    let mut said = "Context compacted".to_string();
    if let Some(time) = time {
        said.push_str(&format!(" at {}", format::clock(time)));
    }
    if let (Some(before), Some(after)) = (before, after) {
        said.push_str(&format!(
            " · {} → {} tokens",
            format::tokens(before),
            format::tokens(after)
        ));
    }
    let rule = || div().flex_1().h(px(1.)).bg(c.line);
    ringed(
        div()
            .id(("compaction", ix))
            .flex()
            .items_center()
            .gap(px(10.))
            .rounded(theme.radii.control)
            .text_color(c.fg3)
            .text_spec(&theme.text.secondary),
        focused,
        theme,
    )
    .child(rule())
    .child(icon(SbIcon::FoldVertical, theme.sizes.icon_sm, c.fg3))
    .child(div().flex_none().child(said))
    .when(has_summary, |this| {
        this.child(
            div()
                .id(("summary-toggle", ix))
                .flex_none()
                .text_color(c.accent_text)
                .cursor_pointer()
                .on_click(on_toggle(ix, row, cx))
                .child(if open { "Hide summary" } else { "Show summary" })
                .automation_target_with({
                    let name = name.clone();
                    move || format!("{name}/summary").into()
                }),
        )
    })
    .child(rule())
    .automation_target(name)
}

/// Text the agent's harness wrote, quietly: its kind and the text, three lines at most.
fn note(at: &BlockRef, theme: &SbTheme, cx: &mut Context<ReaderView>) -> AnyElement {
    let c = &theme.colors;
    let reader = cx.services().reader.read(cx);
    let Some(session) = reader.open_session() else {
        return div().into_any_element();
    };
    let Some(Content::Note { kind, text }) = session.block(at).map(|block| &block.content) else {
        return div().into_any_element();
    };
    let text: String = hostile::bidi(&text.chars().take(1_000).collect::<String>()).into_owned();
    div()
        .flex()
        .items_start()
        .gap(theme.spacing.md)
        .text_color(c.fg3)
        .text_spec(&theme.text.secondary)
        .child(tag(hostile::label(kind), Tone::Neutral, true, theme).max_w(px(160.)))
        .child(div().flex_1().min_w_0().line_clamp(3).child(text))
        .automation_target(target("note", at, session))
}

/// A record its provider didn't know (PLAN D5): what it is, quietly, and Show as recorded.
fn unrecognised(
    ix: usize,
    row: &Row,
    at: &BlockRef,
    open: bool,
    focused: bool,
    theme: &SbTheme,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let c = &theme.colors;
    let (said, name) = {
        let reader = cx.services().reader.read(cx);
        let Some(session) = reader.open_session() else {
            return div().into_any_element();
        };
        let Some(Content::Unrecognised(record)) = session.block(at).map(|block| &block.content)
        else {
            return div().into_any_element();
        };
        let mut said = if record.kind.is_empty() {
            "A record that couldn't be read".to_string()
        } else {
            format!(
                "A record scrollback doesn't know: {}",
                hostile::label(&record.kind)
            )
        };
        if let Some(misfit) = record.misfit {
            said.push_str(&format!(" ({misfit})"));
        }
        (said, target("unrecognised", at, session))
    };
    ringed(
        div()
            .id(("unrecognised", ix))
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .rounded(theme.radii.control)
            .text_color(c.fg3)
            .text_spec(&theme.text.secondary),
        focused,
        theme,
    )
    .child(icon(SbIcon::Puzzle, theme.sizes.icon_xs, c.fg3))
    .child(
        div()
            .min_w_0()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .child(said),
    )
    .child(
        div()
            .id(("raw-toggle", ix))
            .flex_none()
            .text_color(c.accent_text)
            .cursor_pointer()
            .on_click(on_toggle(ix, row, cx))
            .child(if open { "Hide" } else { "Show as recorded" }),
    )
    .automation_target(name)
}

/// The most of a record shown as recorded.
const RAW_LINES: usize = 200;

/// A record as it was recorded, in a code box.
fn raw(at: &BlockRef, theme: &SbTheme, cx: &mut Context<ReaderView>) -> AnyElement {
    let c = &theme.colors;
    let reader = cx.services().reader.clone();
    let loaded = reader
        .read(cx)
        .open_session()
        .and_then(|open| open.raw(at).cloned());
    let inner = match loaded {
        None => {
            reader.update(cx, |reader, cx| reader.want_raw(at, cx));
            skeleton(3, theme)
        }
        Some(Load::Loading) => skeleton(3, theme),
        Some(Load::Failed(why)) => failed(&format!("The record couldn't be read: {why}"), theme),
        Some(Load::Loaded(text)) => {
            let more = text.line_count().saturating_sub(RAW_LINES);
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_color(c.fg2)
                        .text_spec(&theme.text.code)
                        .child(expand_tabs(&hostile::bidi(&text.lines(0..RAW_LINES))).into_owned()),
                )
                .when(more > 0, |this| {
                    this.child(
                        div()
                            .text_color(c.fg3)
                            .text_spec(&theme.text.small)
                            .child(format!("+{} more lines", format::count(more as u64))),
                    )
                })
                .into_any_element()
        }
    };
    div()
        .py(theme.spacing.md)
        .px(theme.spacing.lg)
        .rounded(theme.radii.control)
        .bg(c.code)
        .border_1()
        .border_color(c.line_subtle)
        .overflow_hidden()
        .child(inner)
        .into_any_element()
}

/// How a conversation ended when it isn't whole (PLAN §7.4).
fn ending_row(
    conversation: &Arc<str>,
    ending: &Ended,
    theme: &SbTheme,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let c = &theme.colors;
    let name = {
        let reader = cx.services().reader.read(cx);
        reader.open_session().map_or_else(
            || "reader/ending".to_string(),
            |open| format!("reader/ending/{}", conversation_name(conversation, open)),
        )
    };
    match ending {
        Ended::MidRecord => div()
            .flex()
            .items_center()
            .gap(BANNER_GAP)
            .py(theme.spacing.md)
            .px(theme.spacing.lg)
            .rounded(theme.radii.row)
            .bg(c.s1)
            .border_1()
            .border_color(c.line_subtle)
            .text_color(c.fg2)
            .text_spec(&theme.text.row)
            .child(icon(SbIcon::CircleAlert, theme.sizes.icon_sm, c.fg3))
            .child(div().flex_1().min_w_0().child(
                "The transcript stops in the middle of a record: it was still being written \
                 when it was synced. The rest comes with a later sync.",
            ))
            .automation_target(name),
        Ended::Unreadable(why) => bad_banner(theme)
            .text_spec(&theme.text.row)
            .child(div().flex_1().min_w_0().child(format!(
                "What follows couldn't be read: {}.",
                hostile::label(why.trim_end_matches('.'))
            )))
            .automation_target(name),
    }
}

/// Over a subagent read on its own: back to the session, at the call that started it.
fn alone(theme: &SbTheme, cx: &mut Context<ReaderView>) -> AnyElement {
    let c = &theme.colors;
    div()
        .id("alone-back")
        .flex()
        .items_center()
        .gap(theme.spacing.md)
        .text_color(c.fg2)
        .text_spec(&theme.text.control_small)
        .cursor_pointer()
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.read_alone(None, cx)))
        .child(icon(SbIcon::ArrowLeft, theme.sizes.icon_sm, c.fg2))
        .child("Back to the call that started this subagent")
        .automation_target("reader/alone/back")
}

/// `lines` skeleton bars (DESIGN §4.2.3: a body loading shows three lines).
pub fn skeleton(lines: usize, theme: &SbTheme) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(theme.spacing.sm)
        .children((0..lines).map(|n| {
            div()
                .h(px(10.))
                .w(gpui::relative(match n % 3 {
                    0 => 0.9,
                    1 => 0.7,
                    _ => 0.45,
                }))
                .rounded(theme.radii.small)
                .bg(theme.colors.s1)
        }))
        .into_any_element()
}

/// Why something couldn't be read, in `red`.
pub fn failed(why: &str, theme: &SbTheme) -> AnyElement {
    div()
        .text_color(theme.colors.red)
        .text_spec(&theme.text.secondary)
        .child(format!("{}.", hostile::label(why.trim_end_matches('.'))))
        .into_any_element()
}
