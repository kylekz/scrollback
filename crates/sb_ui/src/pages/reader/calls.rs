//! The reader's tool calls (DESIGN §4.2.3, §4.2.4, §4.2.7): an activity row per call, the body
//! it opens into, and the panel of a subagent it started.
//!
//! **The body is the same for every kind of tool for now**: the call's input as it was sent
//! and what it returned, in a call body with a header over each, the first lines of each part
//! and "Show all", the images as images. The kinds' own renderers (DESIGN §4.2.4: a diff for an
//! edit, the shell's transcript, search results, todos, web results, MCP side by side) take a
//! kind's place in [`super::rows`]' body parts and in [`body`] here, by the call's kind, and
//! leave the list alone.

use std::sync::Arc;

use gpui::{
    AnyElement, ClickEvent, Context, ImageSource, InteractiveElement as _, IntoElement, ObjectFit,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    StyledImage as _, Window, div, img, prelude::FluentBuilder as _, px,
};
use sb_core::transcript::{
    Brief, Content, Conversation, TodoState, ToolCall, ToolInput, ToolKind, ToolStatus,
};

use super::ReaderView;
use super::blocks::{failed, on_toggle, skeleton, target};
use super::rows::{BodyPart, CHUNK_LINES, FIRST_LINES, PanelPart, Row, RowKind};
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{Tone, chip, focus_ring, icon, icon_button, tag};
use crate::format;
use crate::icons::SbIcon;
use crate::services::ActiveServices as _;
use crate::services::reader::{BlockRef, CallBody, Load, Part, Text, Toggle};
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _};

pub fn render(
    view: &mut ReaderView,
    ix: usize,
    row: &Row,
    focused: bool,
    _: &mut Window,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let theme = cx.sb_theme().clone();
    let element = match &row.kind {
        RowKind::Call { at, open } => activity(ix, row, at, *open, focused, &theme, cx),
        RowKind::Body {
            at,
            part,
            first,
            last,
        } => body(at, part, *first, *last, &theme, cx),
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

/**
 * The name an activity row gives a call: the tool's name as its agent calls it, lowercase
 * (DESIGN §4.2.3 draws `bash`, `read`, `grep`), except an MCP tool's, whose name holds its
 * server's: `mcp`, with the server and tool as the target.
 */
fn short_name(call: &ToolCall) -> String {
    match call.kind {
        ToolKind::Mcp => "mcp".to_string(),
        _ => call.name.to_lowercase(),
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
            path,
            first_line,
            lines,
        } => match (first_line, lines) {
            (Some(first), Some(lines)) => {
                format!(
                    "{}  {first}–{}",
                    relative(path, root),
                    first + lines.saturating_sub(1)
                )
            }
            (Some(first), None) => format!("{}  {first}–", relative(path, root)),
            _ => relative(path, root).to_string(),
        },
        ToolInput::Edit { path: file, .. } | ToolInput::Write { path: file, .. } => path(file),
        ToolInput::ContentSearch {
            pattern,
            path: within,
        } => match within {
            Some(within) => format!("\"{pattern}\" in {}", path(within)),
            None => format!("\"{pattern}\""),
        },
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

/// The short result a collapsed row shows (DESIGN §4.2.3), and its status chip. A result with
/// no brief is not taken for a success: a failed one says so.
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
            Some(chip("no result", Tone::Neutral, theme).into_any_element()),
            None,
        );
    };
    let took = took.map(format::duration);
    let text = |text: String| Some(div().child(text).into_any_element());
    let mut status = (result.status == ToolStatus::Failed)
        .then(|| chip("failed", Tone::Bad, theme).into_any_element());
    let said = match &result.brief {
        Some(Brief::Exit(code)) => {
            status = Some(chip(format!("exit {code}"), Tone::Bad, theme).into_any_element());
            took.and_then(text)
        }
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
                    .map(|model| agents::model_name(provider, model, cx)),
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
        None => took.and_then(text),
    };
    (status, said)
}

/// An activity row (DESIGN §4.2.3): glyph, name, target, status, short result, chevron.
fn activity(
    ix: usize,
    row: &Row,
    at: &BlockRef,
    open: bool,
    focused: bool,
    theme: &SbTheme,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
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
    let (status, said) = short_result(&call, took, &provider, theme, cx);
    let root = cx
        .services()
        .library
        .read(cx)
        .open_session()
        .and_then(|session| session.project_path.clone());
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
            this.bg(c.s1).shadow(focus_ring(true, theme))
        })
        .on_click(on_toggle(ix, row, cx))
        .child(icon(kind_icon(call.kind), theme.sizes.icon_sm, c.fg3))
        .child(
            div()
                .flex_none()
                .min_w(theme.sizes.activity_name_width)
                .text_color(c.fg)
                .text_spec(&theme.text.activity_name)
                .child(short_name(&call)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .text_color(c.fg2)
                .child(call_target(&call, root.as_deref())),
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

/// One row of a call's body (DESIGN §4.2.4 `callbody`): a piece of a box drawn across rows, its
/// top on the first and its bottom on the last.
fn body(
    at: &BlockRef,
    part: &BodyPart,
    first: bool,
    last: bool,
    theme: &SbTheme,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let c = &theme.colors;
    let reader = cx.services().reader.clone();
    let (loaded, call, name) = {
        let reader = reader.read(cx);
        let Some(session) = reader.open_session() else {
            return div().into_any_element();
        };
        (
            match session.body(at) {
                Some(Load::Loaded(body)) => Some(body.clone()),
                _ => None,
            },
            session.call(at).cloned(),
            target("body", at, session),
        )
    };
    if matches!(part, BodyPart::Loading) && loaded.is_none() {
        reader.update(cx, |reader, cx| reader.want_body(at, cx));
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
        (BodyPart::InputHead, Some(body), Some(call)) => head(
            &call.name,
            "input",
            None,
            body.input.as_ref().ok().map(|text| text.text.clone()),
            first,
            theme,
        ),
        (BodyPart::OutputHead, Some(body), Some(call)) => head(
            "",
            "output",
            call.result
                .as_ref()
                .filter(|result| result.status == ToolStatus::Failed)
                .map(|_| "failed"),
            Some(output_text(body)).filter(|text| !text.is_empty()),
            first,
            theme,
        ),
        (BodyPart::Lines { part, chunk }, Some(body), _) => match text_of(body, *part) {
            Some(text) => lines(text, *part, *chunk, at, theme, cx),
            None => div().into_any_element(),
        },
        (BodyPart::More { part, all }, Some(body), _) => {
            let count = text_of(body, *part).map_or(0, Text::line_count);
            more(at, *part, *all, count, theme, cx)
        }
        (BodyPart::PartFailed { part }, Some(body), _) => {
            let why = match part {
                None => body.input.as_ref().err(),
                Some(index) => match body.output.get(*index) {
                    Some(Part::Text(Err(why)) | Part::Image(Err(why))) => Some(why),
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
        (BodyPart::Image(index), Some(body), Some(call)) => match body.output.get(*index) {
            Some(Part::Image(Ok(picture))) => {
                let height = theme
                    .sizes
                    .tool_image_max_height
                    .min(px(picture.height as f32));
                div()
                    .p(px(10.))
                    .bg(c.s1)
                    .flex()
                    .flex_col()
                    .gap(theme.spacing.md)
                    .child(
                        img(ImageSource::Image(picture.image.clone()))
                            .w_full()
                            .h(height)
                            .object_fit(ObjectFit::Contain),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(theme.spacing.sm)
                            .text_color(c.fg3)
                            .text_spec(&theme.text.small)
                            .child(icon(SbIcon::Image, theme.sizes.icon_xs, c.fg3))
                            .child(format!(
                                "{} · returned by {} · {} × {} · {}",
                                picture.media_type,
                                call.name,
                                picture.width,
                                picture.height,
                                format::size(picture.bytes as u64)
                            )),
                    )
                    .into_any_element()
            }
            _ => div().into_any_element(),
        },
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

/// The text of the input (`None`) or of an output part.
fn text_of(body: &CallBody, part: Option<usize>) -> Option<&Text> {
    match part {
        None => body.input.as_ref().ok(),
        Some(index) => match body.output.get(index)? {
            Part::Text(Ok(text)) => Some(text),
            _ => None,
        },
    }
}

/// Every text part of the output, joined: what Copy output copies.
fn output_text(body: &CallBody) -> String {
    body.output
        .iter()
        .filter_map(|part| match part {
            Part::Text(Ok(text)) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A call body's header (`callhd`, 26): what follows, a status, and Copy.
fn head(
    name: &str,
    what: &'static str,
    status: Option<&'static str>,
    copy: Option<String>,
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
        .children(copy.map(|text| {
            icon_button(
                SharedString::from(format!("copy-{what}")),
                theme.sizes.small_button,
                theme,
            )
            .ml_auto()
            .aria_label(format!("Copy the {what}"))
            .on_click(move |_, _, cx| crate::system::copy_text(text.clone(), cx))
            .child(icon(SbIcon::Copy, theme.sizes.icon_xs, c.fg2))
        }))
        .into_any_element()
}

/// The `chunk`th row of a part's lines: its first [`FIRST_LINES`] until all are shown, then
/// [`CHUNK_LINES`] a row.
fn lines(
    text: &Text,
    part: Option<usize>,
    chunk: usize,
    at: &BlockRef,
    theme: &SbTheme,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let all = {
        let reader = cx.services().reader.read(cx);
        reader.open_session().is_some_and(|open| {
            open.is_open(&Toggle::AllLines(at.clone(), part), reader.full_calls())
        })
    };
    let count = text.line_count();
    let range = if all {
        chunk * CHUNK_LINES..((chunk + 1) * CHUNK_LINES).min(count)
    } else {
        0..FIRST_LINES.min(count)
    };
    let last = range.end == count || !all;
    div()
        .px(theme.spacing.lg)
        .when(chunk == 0, |this| this.pt(theme.spacing.md))
        .when(last, |this| this.pb(theme.spacing.md))
        .text_color(theme.colors.fg2)
        .text_spec(&theme.text.code)
        .whitespace_nowrap()
        .overflow_hidden()
        .child(text.lines(range))
        .into_any_element()
}

/// Under a part's lines when it has more than its first: how many more, and Show all; or Show
/// fewer once all are shown.
fn more(
    at: &BlockRef,
    part: Option<usize>,
    all: bool,
    count: usize,
    theme: &SbTheme,
    cx: &mut Context<ReaderView>,
) -> AnyElement {
    let c = &theme.colors;
    let toggle = Toggle::AllLines(at.clone(), part);
    let id = SharedString::from(format!(
        "more-{}-{}",
        at.block.0,
        part.map_or(0, |part| part + 1)
    ));
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
            this.child(format!(
                "+{} more lines",
                format::count(count.saturating_sub(FIRST_LINES) as u64)
            ))
        })
        .child(
            div()
                .id(id)
                .ml_auto()
                .text_color(c.accent_text)
                .cursor_pointer()
                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                    let reader = cx.services().reader.clone();
                    reader.update(cx, |reader, cx| reader.toggle(toggle.clone(), cx));
                }))
                .child(if all { "Show fewer" } else { "Show all" }),
        )
        .into_any_element()
}

/// A call body's footer (`callfoot`, 24): its status, how long it took, its output's lines.
fn footer(call: &ToolCall, body: &CallBody, theme: &SbTheme) -> AnyElement {
    let c = &theme.colors;
    let lines: usize = body
        .output
        .iter()
        .map(|part| match part {
            Part::Text(Ok(text)) => text.line_count(),
            _ => 0,
        })
        .sum();
    let (status, bad) = match (
        &call.result,
        call.result.as_ref().and_then(|r| r.brief.as_ref()),
    ) {
        (None, _) => ("not answered".to_string(), false),
        (Some(_), Some(Brief::Exit(code))) => (format!("exit {code}"), *code != 0),
        (Some(result), _) if result.status == ToolStatus::Failed => ("failed".to_string(), true),
        (Some(_), _) => ("ok".to_string(), false),
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
            |this| this.child(format::plural(lines, "line")),
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
    let stem = {
        let file = item.rsplit('/').next().unwrap_or(item);
        file.split('.').next().unwrap_or(file).to_string()
    };
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
                        .child(said),
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

/// A subagent no call is known to have started, as an `agent` row that opens its panel.
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
        let said = match session.subagent(item) {
            Some(Load::Loaded(read)) => [read.kind.clone(), read.description.clone()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · "),
            _ => stem.clone(),
        };
        (said, format!("reader/unattached/{stem}"))
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
                .child(said),
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
