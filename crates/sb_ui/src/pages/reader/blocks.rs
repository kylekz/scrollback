//! The reader's rows of prose (DESIGN §4.2.2, §4.2.5, §4.2.6): prompts and replies with their
//! Markdown, the agent's header over what it wrote, thoughts, notes, compactions, records the
//! provider didn't know, and how a conversation ended.

use std::sync::Arc;

use gpui::{
    AnyElement, ClickEvent, Context, ElementId, ImageSource, InteractiveElement as _, IntoElement,
    ObjectFit, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    StyledImage as _, Window, div, img, prelude::FluentBuilder as _, px,
};
use sb_core::transcript::{Content, Ending};

use super::rows::{PanelPart, Row, RowKind};
use super::{Body, ReaderView};
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{
    BANNER_GAP, Tone, bad_banner, focus_ring, icon, icon_button, overline, tag,
};
use crate::format;
use crate::icons::SbIcon;
use crate::markdown::{self, Size};
use crate::services::ActiveServices as _;
use crate::services::reader::{BlockRef, Load, OpenSession, Picture};
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _};

/// What a row is, for automation and the keyboard's place.
pub fn kind_name(kind: &RowKind) -> &'static str {
    match kind {
        RowKind::Turn => "turn",
        RowKind::Prompt(_) => "prompt",
        RowKind::Task { .. } => "task",
        RowKind::Author(_) => "author",
        RowKind::Reply(_) => "reply",
        RowKind::Thought { .. } => "thought",
        RowKind::Call { .. } => "call",
        RowKind::Body { .. } => "body",
        RowKind::Panel { .. } => "panel",
        RowKind::Compaction { .. } => "compaction",
        RowKind::Summary { .. } => "summary",
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

/// What a row the keyboard can rest on is, whether it is open or not.
#[derive(Debug, PartialEq, Eq)]
pub enum FocusKey<'a> {
    Block(&'a BlockRef),
    More(&'a Arc<str>),
    Unattached(&'a Arc<str>),
}

pub fn focus_key(kind: &RowKind) -> Option<FocusKey<'_>> {
    match kind {
        RowKind::Prompt(at)
        | RowKind::Task { at, .. }
        | RowKind::Reply(at)
        | RowKind::Thought { at, .. }
        | RowKind::Call { at, .. }
        | RowKind::Compaction { at, .. }
        | RowKind::Unrecognised { at, .. } => Some(FocusKey::Block(at)),
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
    let file = item.rsplit('/').next().unwrap_or(item);
    file.split('.').next().unwrap_or(file)
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
    let size = if row.depth > 0 {
        Size::Nested
    } else {
        Size::Reading
    };
    match &row.kind {
        RowKind::Turn => div().h(px(1.)).bg(c.line_subtle).into_any_element(),
        RowKind::Prompt(at) => message(view, ix, at, None, size, focused, cx),
        RowKind::Task { at, from } => message(view, ix, at, Some(from.clone()), size, focused, cx),
        RowKind::Author(at) => author(at, &theme, cx),
        RowKind::Reply(at) => reply(view, ix, at, size, focused, cx),
        RowKind::Thought { at, open } => thought(ix, row, at, *open, focused, &theme, cx),
        RowKind::Compaction { at, open } => compaction(ix, row, at, *open, focused, &theme, cx),
        RowKind::Summary { at, .. } => summary(view, ix, at, &theme, cx),
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
            .automation_target("reader/no-conversation")
            .into_any_element(),
        RowKind::UnattachedHead(count) => overline(
            &format!("Subagents not placed under a call · {count}"),
            &theme,
        )
        .into_any_element(),
        RowKind::Alone(_) => alone(&theme, cx),
        RowKind::Call { .. }
        | RowKind::Body { .. }
        | RowKind::Panel { .. }
        | RowKind::Unattached { .. } => div().into_any_element(),
    }
}

/**
 * The ring of the keyboard's row, on `element`, which reaches 6 past its content on either
 * side so the ring doesn't touch it. A ring is a shadow, drawn under the element: the element
 * is filled so that only its edge shows.
 */
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

/// A Copy button that shows while the pointer is over its message or the keyboard is on it.
fn copy_button(
    id: ElementId,
    group: SharedString,
    shown: bool,
    text: String,
    theme: &SbTheme,
) -> AnyElement {
    icon_button(id, theme.sizes.small_button, theme)
        .ml_auto()
        .aria_label("Copy message")
        .when(!shown, |this| {
            this.opacity(0.)
                .group_hover(group, |style| style.opacity(1.))
        })
        .on_click(move |_, _, cx| crate::system::copy_text(text.clone(), cx))
        .child(icon(SbIcon::Copy, theme.sizes.icon_xs, theme.colors.fg2))
        .into_any_element()
}

/**
 * A prompt (DESIGN §4.2.2 "You"): who asked and when, the prompt as Markdown, and its images as
 * thumbnails. A prompt from another sender is labelled with that sender, never as You; a
 * subagent's task (`task` is the model that gave it) is drawn as the box it was handed in.
 */
#[allow(clippy::too_many_arguments)]
fn message(
    view: &mut ReaderView,
    ix: usize,
    at: &BlockRef,
    task: Option<Option<Arc<str>>>,
    size: Size,
    focused: bool,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let theme = cx.sb_theme().clone();
    let c = &theme.colors;
    let reader = cx.services().reader.clone();
    let (text, from, time, images, prose, name, provider) = {
        let reader = reader.read(cx);
        let Some(open) = reader.open_session() else {
            return div().into_any_element();
        };
        let Some(block) = open.block(at) else {
            return div().into_any_element();
        };
        let Content::Prompt { text, images, from } = &block.content else {
            return div().into_any_element();
        };
        let images: Vec<Option<Load<Picture>>> = (0..images.len())
            .map(|index| open.image(at, index).cloned())
            .collect();
        let prose =
            (!view.states.contains_key(&Body::Prose(at.clone()))).then(|| open.prose(at, text));
        (
            text.clone(),
            from.clone(),
            block.at,
            images,
            prose,
            target(if task.is_some() { "task" } else { "prompt" }, at, open),
            open.id.provider.clone(),
        )
    };
    for (index, image) in images.iter().enumerate() {
        if image.is_none() {
            reader.update(cx, |reader, cx| reader.want_image(at, index, cx));
        }
    }
    let state = view.markdown_state(Body::Prose(at.clone()), ix, || prose, cx);
    let body = state.map(|state| markdown::view(&state, size, cx).w_full());
    let group: SharedString = format!("message-{ix}").into();
    let thumbnails = (!images.is_empty()).then(|| {
        div()
            .flex()
            .flex_wrap()
            .gap(theme.spacing.md)
            .children(images.into_iter().map(|image| shot(image, &theme)))
    });
    if let Some(from) = task {
        let label = match from {
            Some(model) => format!("Task from {}", agents::model_name(&provider, &model, cx)),
            None => "Task".to_string(),
        };
        return div()
            .id(("task", ix))
            .flex()
            .flex_col()
            .gap(theme.spacing.xs)
            .py(theme.spacing.md)
            .px(px(10.))
            .rounded(theme.radii.row)
            .bg(c.s2)
            .border_1()
            .border_color(c.line_subtle)
            .when(focused, |this| this.shadow(focus_ring(false, &theme)))
            .child(
                div()
                    .text_color(c.fg3)
                    .text_spec(&theme.text.tag)
                    .child(label),
            )
            .children(body)
            .children(thumbnails)
            .automation_target(name);
    }
    ringed(
        div()
            .id(("prompt", ix))
            .group(group.clone())
            .flex()
            .flex_col()
            .gap(theme.spacing.md)
            .rounded(theme.radii.control),
        focused,
        &theme,
    )
    .child(
        div()
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .text_color(c.fg3)
            .text_spec(&theme.text.secondary)
            .child(
                div()
                    .text_color(c.fg)
                    .text_spec(&theme.text.author)
                    .child(from.clone().unwrap_or_else(|| "You".to_string())),
            )
            .when(from.is_some(), |this| {
                this.child(tag("not you", Tone::Neutral, true, &theme))
            })
            .children(time.map(|time| {
                div()
                    .text_spec(&theme.text.number)
                    .child(format::clock(time))
            }))
            .child(copy_button(
                ("copy-prompt", ix).into(),
                group,
                focused,
                text,
                &theme,
            )),
    )
    .children(body)
    .children(thumbnails)
    .automation_target(name)
}

/// A prompt's image as a thumbnail (DESIGN §4.2.2 `shot`: 200 × 116, a caption under it).
fn shot(image: Option<Load<Picture>>, theme: &SbTheme) -> AnyElement {
    let c = &theme.colors;
    let caption = match &image {
        Some(Load::Loaded(picture)) => format!(
            "{} · {} × {} · {}",
            picture.media_type,
            picture.width,
            picture.height,
            format::size(picture.bytes as u64)
        ),
        Some(Load::Failed(why)) => why.clone(),
        None | Some(Load::Loading) => "Loading…".to_string(),
    };
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
                .children(match image {
                    Some(Load::Loaded(picture)) => Some(
                        img(ImageSource::Image(picture.image))
                            .absolute()
                            .inset_0()
                            .size_full()
                            .object_fit(ObjectFit::Cover),
                    ),
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
                .text_color(c.fg3)
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
        .map(|model| agents::model_name(&provider, model, cx));
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

/// A reply as Markdown, with Copy on hover.
fn reply(
    view: &mut ReaderView,
    ix: usize,
    at: &BlockRef,
    size: Size,
    focused: bool,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let theme = cx.sb_theme().clone();
    let (text, prose, name) = {
        let reader = cx.services().reader.read(cx);
        let Some(open) = reader.open_session() else {
            return div().into_any_element();
        };
        let Some(Content::Reply { text }) = open.block(at).map(|block| &block.content) else {
            return div().into_any_element();
        };
        let prose =
            (!view.states.contains_key(&Body::Prose(at.clone()))).then(|| open.prose(at, text));
        (text.clone(), prose, target("reply", at, open))
    };
    let state = view.markdown_state(Body::Prose(at.clone()), ix, || prose, cx);
    let group: SharedString = format!("message-{ix}").into();
    ringed(
        div()
            .id(("reply", ix))
            .group(group.clone())
            .relative()
            .rounded(theme.radii.control),
        focused,
        &theme,
    )
    .children(state.map(|state| markdown::view(&state, size, cx).w_full()))
    .child(
        div()
            .absolute()
            .top(-theme.sizes.small_button)
            .right_0()
            .child(copy_button(
                ("copy-reply", ix).into(),
                group,
                focused,
                text,
                &theme,
            )),
    )
    .automation_target(name)
}

/// A thought (DESIGN §4.2.5): one line until opened, then all of it in italic.
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
    let (text, duration, name) = {
        let reader = cx.services().reader.read(cx);
        let Some(session) = reader.open_session() else {
            return div().into_any_element();
        };
        let Some(Content::Thinking { text, duration }) =
            session.block(at).map(|block| &block.content)
        else {
            return div().into_any_element();
        };
        (text.clone(), *duration, target("thought", at, session))
    };
    let lead = match duration {
        Some(duration) => format!(
            "Thought for {}",
            format::duration(i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        ),
        None => "Thought".to_string(),
    };
    let head = div()
        .id(("thought", ix))
        .flex()
        .items_center()
        .gap(theme.spacing.md)
        .h(theme.sizes.thought_row)
        .px(theme.spacing.xs)
        .rounded(theme.radii.control)
        .cursor_pointer()
        .hover(|style| style.bg(c.s1))
        .on_click(on_toggle(ix, row, cx))
        .child(icon(SbIcon::Lightbulb, theme.sizes.icon_sm, c.fg3));
    let head = ringed(head, focused, theme);
    if !open {
        let start: String = text
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
            .chars()
            .take(300)
            .collect();
        return head
            .child(
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
            .automation_target(name);
    }
    div()
        .flex()
        .flex_col()
        .gap(theme.spacing.xs)
        .child(
            head.child(
                div()
                    .text_color(c.fg2)
                    .text_spec(&theme.text.control_small)
                    .child(lead),
            )
            .child(icon(SbIcon::ChevronDown, theme.sizes.icon_xs, c.fg3)),
        )
        .child(
            div()
                .pl(px(22.))
                .text_color(c.fg2)
                .text_spec(&theme.text.thought_open)
                .italic()
                .child(text),
        )
        .automation_target(name)
}

/// A compaction boundary (DESIGN §4.2.6): rules either side of when, and the tokens before and
/// after; Show summary opens the summary the agent went on with.
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

/// A compaction's summary, as Markdown, once it is loaded.
fn summary(
    view: &mut ReaderView,
    ix: usize,
    at: &BlockRef,
    theme: &SbTheme,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let c = &theme.colors;
    let reader = cx.services().reader.clone();
    let loaded = reader
        .read(cx)
        .open_session()
        .and_then(|open| open.summary(at).cloned());
    let inner = match loaded {
        None => {
            reader.update(cx, |reader, cx| reader.want_summary(at, cx));
            skeleton(3, theme)
        }
        Some(Load::Loading) => skeleton(3, theme),
        Some(Load::Failed(why)) => failed(&format!("The summary couldn't be read: {why}"), theme),
        Some(Load::Loaded(text)) => view
            .markdown_state(Body::Summary(at.clone()), ix, || Some(text), cx)
            .map(|state| {
                markdown::view(&state, Size::Nested, cx)
                    .w_full()
                    .into_any_element()
            })
            .unwrap_or_else(|| div().into_any_element()),
    };
    div()
        .p(theme.spacing.lg)
        .rounded(theme.radii.row)
        .bg(c.s1)
        .border_1()
        .border_color(c.line_subtle)
        .child(inner)
        .into_any_element()
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
    let text: String = text.chars().take(2_000).collect();
    div()
        .flex()
        .items_start()
        .gap(theme.spacing.md)
        .text_color(c.fg3)
        .text_spec(&theme.text.secondary)
        .child(tag(kind.clone(), Tone::Neutral, true, theme))
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
            format!("A record scrollback doesn't know: {}", record.kind)
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
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .child(text.lines(0..RAW_LINES)),
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
    ending: &Ending,
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
        Ending::Whole => div().into_any_element(),
        Ending::MidRecord => div()
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
        Ending::Unreadable(why) => bad_banner(theme)
            .text_spec(&theme.text.row)
            .child(div().flex_1().min_w_0().child(format!(
                "What follows couldn't be read: {}.",
                why.trim_end_matches('.')
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
        .child(format!("{}.", why.trim_end_matches('.')))
        .into_any_element()
}
