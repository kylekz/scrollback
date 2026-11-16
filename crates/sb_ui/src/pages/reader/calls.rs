//! The reader's tool calls (DESIGN §4.2.3, §4.2.4, §4.2.7): an activity row per call, the body
//! it opens into, and the panels of the subagents it started.
//!
//! **The body is the same for every kind of tool for now**: the call's input as it was sent
//! and what it returned, in a call body with a header over each, the first lines of each part
//! and "Show all", the images as images, the text wrapping. The kinds' own renderers (DESIGN
//! §4.2.4: a diff for an edit, the shell's transcript, search results, todos, web results, MCP
//! side by side) take a kind's place in [`super::rows`]' body parts and in [`body`] here, by the
//! call's kind, and leave the list alone.

use std::sync::Arc;

use gpui::{
    AnyElement, ClickEvent, Context, ImageSource, InteractiveElement as _, IntoElement, ObjectFit,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    StyledImage as _, Window, div, img, prelude::FluentBuilder as _, px,
};
use gpui_component::tooltip::Tooltip;
use sb_core::transcript::{
    Brief, Content, Conversation, TodoState, ToolCall, ToolInput, ToolKind, ToolStatus,
};

use super::ReaderView;
use super::blocks::{failed, more_lines, on_toggle, skeleton, target, text_rows};
use super::rows::{BodyPart, CHUNK_LINES, PanelPart, Row, RowKind};
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{Tone, focus_ring, icon, icon_button, low_chip, tag, text_width};
use crate::format;
use crate::hostile;
use crate::icons::SbIcon;
use crate::services::ActiveServices as _;
use crate::services::reader::text::expand_tabs;
use crate::services::reader::{BlockRef, CallBody, Held, ImageKey, Load, Part, Slot, Text, Toggle};
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _};

pub fn render(
    view: &mut ReaderView,
    ix: usize,
    row: &Row,
    focused: bool,
    window: &mut Window,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let theme = cx.sb_theme().clone();
    let element = match &row.kind {
        RowKind::Call { at, open } => activity(view, ix, row, at, *open, focused, window, cx),
        RowKind::CallIo { at, open } => call_io(ix, row, at, *open, focused, &theme, cx),
        RowKind::Body {
            at,
            part,
            first,
            last,
        } => body(view, ix, at, part, *first, *last, &theme, cx),
        RowKind::Panel { item, call, part } => {
            panel(view, ix, row, item, call, part, focused, &theme, cx)
        }
        RowKind::Unattached { item, open } => unattached(ix, row, item, *open, focused, &theme, cx),
        _ => div().into_any_element(),
    };
    /* At the top level an activity row and its body reach 8 past the column on either side
    (DESIGN §4.2.3); inside a panel they fill it. */
    if row.depth == 0 && !matches!(row.kind, RowKind::Panel { .. }) {
        div()
            .mx(-theme.spacing.md)
            .child(element)
            .into_any_element()
    } else {
        element
    }
}

/// A kind's glyph (DESIGN §1.5): told apart by glyph and name, never by colour.
pub fn kind_icon(kind: ToolKind) -> SbIcon {
    match kind {
        ToolKind::Shell => SbIcon::Terminal,
        ToolKind::Read => SbIcon::FileText,
        ToolKind::Edit => SbIcon::Pen,
        ToolKind::Write => SbIcon::FilePlus,
        ToolKind::ContentSearch => SbIcon::Search,
        ToolKind::FileSearch => SbIcon::FileSearch,
        ToolKind::Todos => SbIcon::ListTodo,
        ToolKind::Subagent => SbIcon::Bot,
        ToolKind::WebSearch | ToolKind::WebFetch => SbIcon::Globe,
        ToolKind::Mcp => SbIcon::Plug,
        ToolKind::Other => SbIcon::Puzzle,
    }
}

/// `path` under the session's project folder `root`, as the boards write it; else whole.
fn relative<'a>(path: &'a str, root: Option<&str>) -> &'a str {
    root.and_then(|root| path.strip_prefix(root))
        .and_then(|rest| rest.strip_prefix(['/', '\\']))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(path)
}

/// What the call was about, from its typed input: the command, the path, the pattern. Paths
/// under the session's project folder `root` are written from there.
fn call_target(call: &ToolCall, root: Option<&str>) -> String {
    let path = |path: &str| relative(path, root).to_string();
    match &call.input {
        ToolInput::Shell { command, .. } => command.lines().next().unwrap_or("").to_string(),
        ToolInput::Read {
            path: file,
            first_line,
            lines,
        } => match (first_line, lines) {
            (Some(first), Some(lines)) => {
                format!(
                    "{}  {first}–{}",
                    path(file),
                    first + lines.saturating_sub(1)
                )
            }
            (Some(first), None) => format!("{}  {first}–", path(file)),
            _ => path(file),
        },
        ToolInput::Edit { path: file, .. } | ToolInput::Write { path: file, .. } => path(file),
        ToolInput::ContentSearch {
            pattern,
            path: within,
            files,
        } => {
            let searched = match within {
                Some(within) => format!("\"{pattern}\" in {}", path(within)),
                None => format!("\"{pattern}\""),
            };
            match files {
                Some(files) => format!("{searched} · {files}"),
                None => searched,
            }
        }
        ToolInput::FileSearch {
            pattern,
            path: within,
        } => match within {
            Some(within) => format!("{pattern} in {}", path(within)),
            None => pattern.clone(),
        },
        ToolInput::Todos(todos) => {
            let done = todos
                .iter()
                .filter(|todo| todo.state == TodoState::Done)
                .count();
            let doing = todos
                .iter()
                .find(|todo| todo.state == TodoState::InProgress)
                .map(|todo| format!("{} · ", todo.text))
                .unwrap_or_default();
            format!("{doing}{done} of {} done", todos.len())
        }
        ToolInput::Subagent {
            kind, description, ..
        } => [kind.as_deref(), description.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · "),
        ToolInput::WebSearch { query } => query.clone(),
        ToolInput::WebFetch { url, .. } => url.clone(),
        ToolInput::Mcp { server, tool } => format!("{server} · {tool}"),
        ToolInput::Other => String::new(),
    }
}

/**
 * The status chip and the short result a collapsed row shows (DESIGN §4.2.3). A result with no
 * brief is not taken for a success, and a failed result reads as failed whatever its brief
 * says. A list of todos shows its progress in its target, not how long it took.
 */
fn short_result(
    call: &ToolCall,
    took: Option<i64>,
    provider: &str,
    theme: &SbTheme,
    cx: &gpui::App,
) -> (Option<AnyElement>, Option<AnyElement>) {
    let c = &theme.colors;
    let Some(result) = &call.result else {
        return (
            Some(low_chip("no result", Tone::Neutral, theme).into_any_element()),
            None,
        );
    };
    let took = took
        .filter(|_| call.kind != ToolKind::Todos)
        .map(format::duration);
    let text = |text: String| Some(div().child(text).into_any_element());
    let failed = result.status == ToolStatus::Failed;
    let status = match (&result.brief, failed) {
        (Some(Brief::Exit(code)), true) if *code != 0 => Some(format!("exit {code}")),
        (_, true) => Some("failed".to_string()),
        _ => None,
    }
    .map(|label| low_chip(label, Tone::Bad, theme).into_any_element());
    let said = match &result.brief {
        Some(Brief::Exit(_)) | None => took.and_then(text),
        Some(Brief::Lines(lines)) => text(format::plural(*lines as usize, "line")),
        Some(Brief::Changed { added, removed }) => Some(
            div()
                .flex()
                .gap(theme.spacing.xs)
                .child(div().text_color(c.green).child(format!("+{added}")))
                .child(div().text_color(c.red).child(format!("−{removed}")))
                .into_any_element(),
        ),
        Some(Brief::Written { new, lines }) => text(format!(
            "{}{}",
            if *new { "new file · " } else { "" },
            format::plural(*lines as usize, "line")
        )),
        Some(Brief::Matches(matches)) => text(format!(
            "{} {}",
            format::count(*matches),
            if *matches == 1 { "match" } else { "matches" }
        )),
        Some(Brief::Ran {
            model,
            calls,
            duration,
        }) => {
            let parts: Vec<String> = [
                model
                    .as_deref()
                    .map(|model| hostile::label(&agents::model_name(provider, model, cx))),
                calls.map(|calls| format::plural(calls as usize, "call")),
                duration.map(|duration| {
                    format::duration(i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
                }),
            ]
            .into_iter()
            .flatten()
            .collect();
            text(parts.join(" · "))
        }
    };
    (status, said)
}

/**
 * An activity row (DESIGN §4.2.3): glyph, label, target, status, short result, chevron. The
 * label column is as wide as the widest label drawn, up to a bound past which a label ends in
 * an ellipsis with the tool's name in a tooltip, so that the targets line up.
 */
#[allow(clippy::too_many_arguments)]
fn activity(
    view: &mut ReaderView,
    ix: usize,
    row: &Row,
    at: &BlockRef,
    open: bool,
    focused: bool,
    window: &mut Window,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let theme = cx.sb_theme().clone();
    let c = &theme.colors;
    let (call, took, provider, name) = {
        let reader = cx.services().reader.read(cx);
        let Some(session) = reader.open_session() else {
            return div().into_any_element();
        };
        let Some(block) = session.block(at) else {
            return div().into_any_element();
        };
        let Content::ToolCall(call) = &block.content else {
            return div().into_any_element();
        };
        let took = block
            .at
            .zip(call.result.as_ref().and_then(|result| result.at))
            .map(|(asked, answered)| answered - asked);
        (
            call.clone(),
            took,
            session.id.provider.clone(),
            target("call", at, session),
        )
    };
    let (status, said) = short_result(&call, took, &provider, &theme, cx);
    let root = cx
        .services()
        .library
        .read(cx)
        .open_session()
        .and_then(|session| session.project_path.clone());
    let label = hostile::label(&agents::tool_label(&provider, &call.name, cx));
    let label_width = text_width(&label, &theme.text.activity_name, window);
    view.label_width_next
        .set(view.label_width_next.get().max(label_width));
    let clipped = label_width > view.label_width;
    let tool = hostile::label(&call.name);
    let height = if row.depth > 0 {
        theme.sizes.activity_row_nested
    } else {
        theme.sizes.activity_row
    };
    div()
        .id(("call", ix))
        .flex()
        .items_center()
        .gap(theme.spacing.md)
        .h(height)
        .px(theme.spacing.md)
        .rounded(theme.radii.control)
        .text_spec(&theme.text.activity)
        .whitespace_nowrap()
        .cursor_pointer()
        .when(open, |this| this.bg(c.s1))
        .hover(|style| style.bg(c.s1))
        .when(focused, |this| {
            this.bg(c.s1).shadow(focus_ring(true, &theme))
        })
        .on_click(on_toggle(ix, row, cx))
        .child(icon(kind_icon(call.kind), theme.sizes.icon_sm, c.fg3))
        .child(
            div()
                .id(("label", ix))
                .flex_none()
                .w(view.label_width)
                .overflow_hidden()
                .text_ellipsis()
                .text_color(c.fg)
                .text_spec(&theme.text.activity_name)
                .when(clipped, |this| {
                    this.tooltip(move |window, cx| Tooltip::new(tool.clone()).build(window, cx))
                })
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .text_color(c.fg2)
                .child(hostile::label(&call_target(&call, root.as_deref()))),
        )
        .children(status)
        .children(said.map(|said| {
            div()
                .flex_none()
                .text_color(c.fg3)
                .text_spec(&theme.text.count)
                .child(said)
        }))
        .child(icon(
            if open {
                SbIcon::ChevronDown
            } else {
                SbIcon::ChevronRight
            },
            theme.sizes.icon_xs,
            c.fg3,
        ))
        .automation_target(name)
}

/// Under a call that started subagents: its own input and output, opened in a body under it.
fn call_io(
    ix: usize,
    row: &Row,
    at: &BlockRef,
    open: bool,
    focused: bool,
    theme: &SbTheme,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let c = &theme.colors;
    let name = {
        let reader = cx.services().reader.read(cx);
        reader
            .open_session()
            .map(|session| target("call-io", at, session))
    };
    div()
        .id(("call-io", ix))
        .flex()
        .items_center()
        .gap(theme.spacing.sm)
        .ml(theme.sizes.call_indent)
        .px(theme.spacing.sm)
        .h(theme.sizes.call_footer)
        .rounded(theme.radii.control)
        .text_color(c.accent_text)
        .text_spec(&theme.text.small)
        .cursor_pointer()
        .when(focused, |this| this.shadow(focus_ring(true, theme)))
        .on_click(on_toggle(ix, row, cx))
        .child(icon(
            if open {
                SbIcon::ChevronDown
            } else {
                SbIcon::ChevronRight
            },
            theme.sizes.icon_xs,
            c.fg3,
        ))
        .child("The call as it was sent and answered")
        .map(|this| match name {
            Some(name) => this.automation_target(name),
            None => this.into_any_element(),
        })
}

/// One row of a call's body (DESIGN §4.2.4 `callbody`): a piece of a box drawn across rows, its
/// top on the first and its bottom on the last.
#[allow(clippy::too_many_arguments)]
fn body(
    view: &mut ReaderView,
    ix: usize,
    at: &BlockRef,
    part: &BodyPart,
    first: bool,
    last: bool,
    theme: &SbTheme,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let c = &theme.colors;
    let reader = cx.services().reader.clone();
    let (loaded, call, image, all, whole, name) = {
        let reader = reader.read(cx);
        let Some(session) = reader.open_session() else {
            return div().into_any_element();
        };
        let image = match part {
            BodyPart::Image(index) => {
                let key = ImageKey {
                    at: at.clone(),
                    slot: Slot::Output(*index),
                };
                Some((session.image(&key).cloned(), key))
            }
            _ => None,
        };
        let all = match part {
            BodyPart::Lines { part, .. } | BodyPart::More { part, .. } => {
                session.is_open(&Toggle::AllLines(at.clone(), *part), reader.full_calls())
            }
            _ => false,
        };
        let whole = match part {
            BodyPart::Lines { part, .. }
            | BodyPart::Whole { part }
            | BodyPart::More { part, .. } => session.whole(at, *part).cloned(),
            _ => None,
        };
        (
            match session.body(at) {
                Some(Load::Loaded(body)) => Some(body.clone()),
                _ => None,
            },
            session.call(at).cloned(),
            image,
            all,
            whole,
            target("body", at, session),
        )
    };
    view.hold(Held::Body(at.clone()), ix);
    if matches!(part, BodyPart::Loading) && loaded.is_none() {
        reader.update(cx, |reader, cx| reader.want_body(at, cx));
    }
    match part {
        BodyPart::Whole { part: index } => {
            view.hold(Held::Whole(at.clone(), *index), ix);
            if whole.is_none() {
                reader.update(cx, |reader, cx| reader.want_whole(at, *index, cx));
            }
        }
        BodyPart::Lines { part: index, .. } if whole.is_some() => {
            view.hold(Held::Whole(at.clone(), *index), ix);
        }
        _ => {}
    }
    let content = match (part, &loaded, &call) {
        (BodyPart::Loading, _, _) => div()
            .py(theme.spacing.md)
            .px(theme.spacing.lg)
            .child(skeleton(3, theme))
            .into_any_element(),
        (BodyPart::Failed(why), _, _) => div()
            .py(theme.spacing.md)
            .px(theme.spacing.lg)
            .child(failed(&format!("The call couldn't be read: {why}"), theme))
            .into_any_element(),
        (BodyPart::InputHead, Some(_), Some(call)) => head(
            ix,
            &hostile::label(&call.name),
            "input",
            None,
            Copy::Input(at.clone()),
            first,
            theme,
        ),
        (BodyPart::OutputHead, Some(_), Some(call)) => head(
            ix,
            "",
            "output",
            call.result
                .as_ref()
                .filter(|result| result.status == ToolStatus::Failed)
                .map(|_| "failed"),
            Copy::Output(at.clone()),
            first,
            theme,
        ),
        (BodyPart::Whole { .. }, _, _) => div()
            .py(theme.spacing.md)
            .px(theme.spacing.lg)
            .child(match &whole {
                Some(Load::Failed(why)) => {
                    failed(&format!("The rest couldn't be read: {why}"), theme)
                }
                _ => skeleton(3, theme),
            })
            .into_any_element(),
        (BodyPart::Lines { part, chunk }, Some(body), _) => match match (&whole, all) {
            (Some(Load::Loaded(whole)), true) => Some(whole.as_ref()),
            _ => text_of(body, *part),
        } {
            /* Space above the first row and below the last only: the rows of one part read as
            one block, with no seam where one ends and the next begins. */
            Some(text) => div()
                .px(theme.spacing.lg)
                .when(*chunk == 0, |this| this.pt(theme.spacing.md))
                .when(
                    !all || (chunk + 1) * CHUNK_LINES >= text.line_count(),
                    |this| this.pb(theme.spacing.xs),
                )
                .text_color(c.fg2)
                .text_spec(&theme.text.code)
                .child(expand_tabs(&hostile::bidi(&text_rows(text, *chunk, all))).into_owned())
                .into_any_element(),
            None => div().into_any_element(),
        },
        (BodyPart::More { part, all }, Some(body), _) => {
            let text = match (&whole, *all) {
                (Some(Load::Loaded(whole)), true) => Some(whole.as_ref()),
                _ => text_of(body, *part),
            };
            div()
                .px(px(10.))
                .border_t_1()
                .border_color(c.line_subtle)
                .child(more_lines(
                    format!("more-{ix}").into(),
                    text,
                    *all,
                    Toggle::AllLines(at.clone(), *part),
                    theme,
                    cx,
                ))
                .into_any_element()
        }
        (BodyPart::PartFailed { part }, Some(body), _) => {
            let why = match part {
                None => body.input.as_ref().err(),
                Some(index) => match body.output.get(*index) {
                    Some(Part::Text(Err(why))) => Some(why),
                    _ => None,
                },
            };
            div()
                .py(theme.spacing.md)
                .px(theme.spacing.lg)
                .child(failed(
                    &format!(
                        "This part couldn't be read: {}",
                        why.map_or("", String::as_str)
                    ),
                    theme,
                ))
                .into_any_element()
        }
        (BodyPart::Image(_), _, Some(call)) => {
            let Some((load, key)) = image else {
                return div().into_any_element();
            };
            view.hold(Held::Image(key.clone()), ix);
            if load.is_none() {
                reader.update(cx, |reader, cx| reader.want_image(&key, cx));
            }
            tool_image(load, &hostile::label(&call.name), theme)
        }
        (BodyPart::NoOutput, _, Some(call)) => div()
            .py(theme.spacing.md)
            .px(theme.spacing.lg)
            .text_color(c.fg3)
            .text_spec(&theme.text.secondary)
            .child(if call.result.is_some() {
                "It returned nothing."
            } else {
                "No result: the session stopped before the call was answered."
            })
            .into_any_element(),
        (BodyPart::Footer, Some(body), Some(call)) => footer(call, body, theme),
        _ => div().into_any_element(),
    };
    let piece = div()
        .ml(theme.sizes.call_indent)
        .mr(theme.spacing.md)
        .bg(c.code)
        .border_l_1()
        .border_r_1()
        .border_color(c.line_subtle)
        .overflow_hidden()
        .when(first, |this| {
            this.mt(px(2.)).border_t_1().rounded_t(theme.radii.control)
        })
        .when(last, |this| {
            this.mb(px(6.)).border_b_1().rounded_b(theme.radii.control)
        })
        .child(content);
    if first {
        piece.automation_target(name)
    } else {
        piece.into_any_element()
    }
}

/// An image a tool returned (DESIGN §4.2.4): fitted to the column, at most 320 high, with a
/// caption; or why it can't be shown.
fn tool_image(
    load: Option<Load<crate::services::reader::Picture>>,
    tool: &str,
    theme: &SbTheme,
) -> AnyElement {
    let c = &theme.colors;
    let caption = |text: String, bad: bool| {
        div()
            .flex()
            .items_center()
            .gap(theme.spacing.sm)
            .text_color(if bad { c.red } else { c.fg3 })
            .text_spec(&theme.text.small)
            .child(icon(SbIcon::Image, theme.sizes.icon_xs, c.fg3))
            .child(hostile::label(&text))
    };
    let inner = match load {
        Some(Load::Loaded(picture)) => {
            let height = theme
                .sizes
                .tool_image_max_height
                .min(px(picture.height as f32));
            div()
                .flex()
                .flex_col()
                .gap(theme.spacing.md)
                .child(
                    img(ImageSource::Render(picture.image.clone()))
                        .w_full()
                        .h(height)
                        .object_fit(ObjectFit::Contain),
                )
                .child(caption(
                    format!(
                        "returned by {tool} · {} × {} · {}",
                        picture.width,
                        picture.height,
                        format::file_size(picture.bytes as u64)
                    ),
                    false,
                ))
                .into_any_element()
        }
        Some(Load::Failed(why)) => caption(why, true).into_any_element(),
        None | Some(Load::Loading) => skeleton(3, theme),
    };
    div().p(px(10.)).bg(c.s1).child(inner).into_any_element()
}

/// The text of the input (`None`) or of an output part.
fn text_of(body: &CallBody, part: Option<usize>) -> Option<&Text> {
    match part {
        None => body.input.as_ref().ok().map(Arc::as_ref),
        Some(index) => match body.output.get(index)? {
            Part::Text(Ok(text)) => Some(text),
            _ => None,
        },
    }
}

/// What a body header's Copy copies, read when it is clicked.
enum Copy {
    Input(BlockRef),
    Output(BlockRef),
}

/// A call body's header (`callhd`, 26): what follows, a status, and Copy.
fn head(
    ix: usize,
    name: &str,
    what: &'static str,
    status: Option<&'static str>,
    copy: Copy,
    first: bool,
    theme: &SbTheme,
) -> AnyElement {
    let c = &theme.colors;
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
        .when(!name.is_empty(), |this| {
            this.child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_spec(&theme.text.path_small)
                    .text_color(c.fg2)
                    .child(name.to_string()),
            )
        })
        .child(what)
        .children(status.map(|status| div().text_color(c.red).child(status)))
        .child(
            icon_button(
                SharedString::from(format!("copy-{what}-{ix}")),
                theme.sizes.small_button,
                theme,
            )
            .ml_auto()
            .aria_label(format!("Copy the {what}"))
            .on_click(move |_, _, cx| {
                let reader = cx.services().reader.clone();
                reader.update(cx, |reader, cx| match &copy {
                    Copy::Input(at) => reader.copy_call(at, false, cx),
                    Copy::Output(at) => reader.copy_call(at, true, cx),
                });
            })
            .child(icon(SbIcon::Copy, theme.sizes.icon_xs, c.fg2)),
        )
        .into_any_element()
}

/// A call body's footer (`callfoot`, 24): its status and its output's lines. A failed call
/// reads as failed whatever its exit status.
fn footer(call: &ToolCall, body: &CallBody, theme: &SbTheme) -> AnyElement {
    let c = &theme.colors;
    let texts = || {
        body.output.iter().filter_map(|part| match part {
            Part::Text(Ok(text)) => Some(text),
            _ => None,
        })
    };
    /* Of an output read only in part, the lines aren't known: its size is. */
    let size = if texts().any(|text| text.rest.is_some()) {
        let bytes = texts().map(|text| text.text.len() as u64 + text.rest.unwrap_or(0));
        format::file_size(bytes.sum())
    } else {
        format::plural(texts().map(|text| text.line_count()).sum(), "line")
    };
    let (status, bad) = match &call.result {
        None => ("not answered".to_string(), false),
        Some(result) if result.status == ToolStatus::Failed => match result.brief {
            Some(Brief::Exit(code)) if code != 0 => (format!("failed · exit {code}"), true),
            _ => ("failed".to_string(), true),
        },
        Some(_) => ("ok".to_string(), false),
    };
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
        .child(div().when(bad, |this| this.text_color(c.red)).child(status))
        .when(
            body.output.iter().any(|part| matches!(part, Part::Text(_))),
            |this| this.child(size),
        )
        .into_any_element()
}

/// A line of a subagent's panel (DESIGN §4.2.7).
#[allow(clippy::too_many_arguments)]
fn panel(
    view: &mut ReaderView,
    ix: usize,
    row: &Row,
    item: &Arc<str>,
    call: &Option<BlockRef>,
    part: &PanelPart,
    focused: bool,
    theme: &SbTheme,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let c = &theme.colors;
    let reader = cx.services().reader.clone();
    let open_alone = {
        let item = item.clone();
        let call = call.clone();
        cx.listener(move |this: &mut ReaderView, _: &ClickEvent, _, cx| {
            this.read_alone(Some((item.clone(), call.clone())), cx)
        })
    };
    let stem = super::blocks::file_stem(item).to_string();
    let alone_button = |id: SharedString| {
        div()
            .id(id)
            .ml_auto()
            .flex_none()
            .text_color(c.accent_text)
            .cursor_pointer()
            .on_click(open_alone)
            .child("Open on its own")
            .automation_target(format!("reader/alone/{stem}"))
    };
    let alone = view.alone.is_some();
    match part {
        PanelPart::Meta => {
            let (said, name) = {
                let reader = reader.read(cx);
                let Some(session) = reader.open_session() else {
                    return div().into_any_element();
                };
                let said = match session.subagent(item) {
                    Some(Load::Loaded(read)) => meta(
                        &read.conversation,
                        read.kind.as_deref(),
                        &session.id.provider,
                        cx,
                    ),
                    _ => "reading…".to_string(),
                };
                (
                    said,
                    format!(
                        "reader/panel/{}",
                        super::blocks::conversation_name(item, session)
                    ),
                )
            };
            div()
                .flex()
                .items_center()
                .gap(theme.spacing.md)
                .text_color(c.fg3)
                .text_spec(&theme.text.small)
                .whitespace_nowrap()
                .child(tag("subagent", Tone::Neutral, false, theme))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(hostile::label(&said)),
                )
                .when(!alone, |this| {
                    this.child(alone_button(format!("alone-{ix}").into()))
                })
                .automation_target(name)
        }
        PanelPart::Loading => {
            reader.update(cx, |reader, cx| reader.want_subagent(item, cx));
            skeleton(2, theme)
        }
        PanelPart::Failed(why) => failed(&format!("The subagent couldn't be read: {why}"), theme),
        PanelPart::More(count) => div()
            .id(("more-calls", ix))
            .flex()
            .px(theme.spacing.md)
            .py(theme.spacing.xs)
            .rounded(theme.radii.control)
            .text_color(c.accent_text)
            .text_spec(&theme.text.secondary)
            .cursor_pointer()
            .when(focused, |this| {
                this.bg(c.s1).shadow(focus_ring(true, theme))
            })
            .on_click(on_toggle(ix, row, cx))
            .child(format!(
                "Show {} more {}",
                count,
                if *count == 1 { "call" } else { "calls" }
            ))
            .into_any_element(),
        PanelPart::TooDeep => div()
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .text_color(c.fg3)
            .text_spec(&theme.text.small)
            .child("A subagent nested deeper than the reader draws.")
            .child(alone_button(format!("alone-{ix}").into()))
            .into_any_element(),
        PanelPart::Repeat => div()
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .text_color(c.fg3)
            .text_spec(&theme.text.small)
            .child("A subagent already shown where it was first started.")
            .child(alone_button(format!("alone-{ix}").into()))
            .into_any_element(),
    }
}

/// A subagent's meta line: its kind, when it started, its model, its calls, how long it ran
/// and the tokens it used.
fn meta(conversation: &Conversation, kind: Option<&str>, provider: &str, cx: &gpui::App) -> String {
    let span = conversation.span();
    let tokens = conversation.tokens;
    let used = tokens.input + tokens.output + tokens.cache_read + tokens.cache_write;
    [
        kind.map(str::to_string),
        span.map(|(start, _)| format!("started {}", format::clock(start))),
        conversation
            .models
            .first()
            .map(|model| agents::model_name(provider, model, cx)),
        Some(format::plural(conversation.calls().count(), "call")),
        span.map(|(start, end)| format::duration(end - start)),
        (used > 0).then(|| format!("{} tokens", format::tokens(used))),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ")
}

/// A subagent no call is known to have started, as an `agent` row that opens its panel, named
/// from what it says it is.
fn unattached(
    ix: usize,
    row: &Row,
    item: &Arc<str>,
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
        let stem = super::blocks::conversation_name(item, session).to_string();
        let names = session.names(item);
        let said = [names.kind, names.description]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        (
            if said.is_empty() {
                "a subagent".to_string()
            } else {
                said
            },
            format!("reader/unattached/{stem}"),
        )
    };
    div()
        .id(("unattached", ix))
        .flex()
        .items_center()
        .gap(theme.spacing.md)
        .h(theme.sizes.activity_row)
        .px(theme.spacing.md)
        .rounded(theme.radii.control)
        .text_spec(&theme.text.activity)
        .whitespace_nowrap()
        .cursor_pointer()
        .when(open, |this| this.bg(c.s1))
        .hover(|style| style.bg(c.s1))
        .when(focused, |this| {
            this.bg(c.s1).shadow(focus_ring(true, theme))
        })
        .on_click(on_toggle(ix, row, cx))
        .child(icon(SbIcon::Bot, theme.sizes.icon_sm, c.fg3))
        .child(
            div()
                .flex_none()
                .min_w(theme.sizes.activity_name_width)
                .text_color(c.fg)
                .text_spec(&theme.text.activity_name)
                .child("agent"),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .text_color(c.fg2)
                .child(hostile::label(&said)),
        )
        .child(icon(
            if open {
                SbIcon::ChevronDown
            } else {
                SbIcon::ChevronRight
            },
            theme.sizes.icon_xs,
            c.fg3,
        ))
        .automation_target(name)
}
