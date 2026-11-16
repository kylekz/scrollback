//! The facts margin (DESIGN §4.2.8): the session's facts, its turns, its calls by kind; and the
//! History popover of a file's versions (DESIGN §4.3), from the facts' Version row or a call
//! body's footer.
//!
//! The turns are a `uniform_list`, so a session of thousands of turns costs what the rows in
//! view do; the list keeps the current turn (the one at the top of the column) in view.

use std::collections::BTreeMap;
use std::sync::Arc;

use gpui::{
    AnyElement, ClickEvent, Context, InteractiveElement as _, IntoElement, ListOffset,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, div,
    prelude::FluentBuilder as _, px, uniform_list,
};
use sb_core::catalogue::{Held, History, Reason};
use sb_core::transcript::{Content, ToolKind};

use super::ReaderView;
use super::calls::kind_icon;
use super::rows::RowKind;
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{
    ButtonKind, Tone, button, icon, icon_button, kv, location_tag, low_chip, overline, warn_chip,
};
use crate::format;
use crate::hostile;
use crate::icons::SbIcon;
use crate::services::ActiveServices as _;
use crate::services::reader::OpenSession;
use crate::theme::{ActiveTheme as _, StyledText as _};

/// The facts margin's width (DESIGN §1.4) and the History popover's.
pub const FACTS_WIDTH: f32 = 268.;
pub const HISTORY_WIDTH: f32 = 440.;
/// A turn's row in the facts.
const TURN_ROW: f32 = 22.;

/// The kinds of call the facts count, each under one label (DESIGN §4.2.8 "Calls").
const CALL_KINDS: &[(&str, &[ToolKind])] = &[
    ("Shell", &[ToolKind::Shell]),
    ("Read", &[ToolKind::Read]),
    ("Search", &[ToolKind::ContentSearch, ToolKind::FileSearch]),
    ("Edit / Write", &[ToolKind::Edit, ToolKind::Write]),
    ("Agents", &[ToolKind::Subagent]),
    ("Todos", &[ToolKind::Todos]),
    ("Web", &[ToolKind::WebSearch, ToolKind::WebFetch]),
    ("MCP", &[ToolKind::Mcp]),
    ("Other", &[ToolKind::Other]),
];

/// Whose History is open: the file's key, and whether it is the session's own transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryOf {
    pub key: Arc<str>,
    pub history: History,
    pub main: bool,
}

/// The number of the version with id `id` in `history`.
pub fn number(history: &History, id: i64) -> Option<u32> {
    history
        .versions
        .iter()
        .find(|(version, _)| version.id == id)
        .map(|(version, _)| version.number)
}

impl ReaderView {
    /// The turn of the session's own conversation at the top of the column.
    pub(super) fn current_turn(&self, open: &OpenSession) -> Option<usize> {
        let main = open.main()?;
        let top = self.list.logical_scroll_top().item_ix;
        let block = self.rows[..=top.min(self.rows.len().checked_sub(1)?)]
            .iter()
            .rev()
            .find_map(|row| row.block().filter(|at| at.conversation == main.item))?;
        main.turns()
            .iter()
            .position(|turn| turn.blocks.contains(&block.block))
    }

    /// Scrolls the column to the turn `turn`'s first row.
    pub(super) fn go_to_turn(&mut self, turn: usize, cx: &mut Context<Self>) {
        let reader = cx.services().reader.read(cx);
        let Some(first) = reader
            .open_session()
            .and_then(|open| open.main())
            .and_then(|main| Some((main.item.clone(), *main.turns().get(turn)?.blocks.first()?)))
        else {
            return;
        };
        let Some(row) = self.rows.iter().position(|row| {
            !matches!(row.kind, RowKind::Turn(_))
                && row
                    .block()
                    .is_some_and(|at| at.conversation == first.0 && at.block == first.1)
        }) else {
            return;
        };
        self.list.set_follow_mode(gpui::FollowMode::Normal);
        self.list.scroll_to(ListOffset {
            item_ix: row,
            offset_in_item: px(0.),
        });
        if self.rows[row].is_focusable() {
            self.focused = Some((row, self.rows[row].clone()));
        }
        cx.notify();
    }

    /// The facts margin (DESIGN §4.2.8): Session, Turns, Calls.
    pub(super) fn render_facts(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.sb_theme().clone();
        let c = &theme.colors;
        let services = cx.services();
        let (library, reader, sync) = (
            services.library.read(cx),
            services.reader.read(cx),
            services.sync.read(cx),
        );
        let (Some(session), Some(open)) = (library.open_session(), reader.open_session()) else {
            return div().into_any_element();
        };
        let provider = session.id.provider.clone();
        let location = sync
            .locations()
            .unwrap_or_default()
            .iter()
            .find(|location| location.id == session.id.location)
            .map(|location| location_tag(location.name.clone(), &location.kind, false, &theme));
        let value = |text: String| div().child(text);
        let numbers = |text: String| div().text_spec(&theme.text.number).child(text);
        let mut facts: Vec<AnyElement> = Vec::new();
        facts.extend(
            location.map(|tag| kv("From", div().flex().child(tag), &theme).into_any_element()),
        );
        if let Some(path) = &session.project_path {
            facts.push(
                kv(
                    "Path",
                    div()
                        .font_family(theme.fonts.mono.clone())
                        .text_size(px(11.5))
                        .child(hostile::label(path)),
                    &theme,
                )
                .into_any_element(),
            );
        }
        if let Some(branch) = &session.branch {
            facts.push(
                kv(
                    "Branch",
                    div()
                        .flex()
                        .items_center()
                        .gap(theme.spacing.xs)
                        .child(icon(SbIcon::GitBranch, theme.sizes.icon_xs, c.fg3))
                        .child(hostile::label(branch)),
                    &theme,
                )
                .into_any_element(),
            );
        }
        if library.agents() > 1 {
            facts.push(
                kv(
                    "Agent",
                    div()
                        .flex()
                        .items_center()
                        .gap(theme.spacing.xs)
                        .child(agents::mark(&provider, theme.sizes.icon_xs, cx))
                        .child(agents::name(&provider, cx)),
                    &theme,
                )
                .into_any_element(),
            );
        }
        if !session.models.is_empty() {
            let models: Vec<String> = session
                .models
                .iter()
                .map(|model| hostile::label(&agents::model_name(&provider, model, cx)))
                .collect();
            facts.push(kv("Models", value(models.join(", ")), &theme).into_any_element());
        }
        if let Some(started) = session.started {
            let mut when = format!(
                "{} {}",
                format::day(started, format::now()),
                format::clock(started)
            );
            if let Some(ended) = session.ended {
                when.push_str(&format!(" · {}", format::duration(ended - started)));
            }
            facts.push(kv("When", numbers(when), &theme).into_any_element());
        }
        let tokens = session.all.tokens;
        if tokens.input + tokens.output > 0 {
            facts.push(
                kv(
                    "Tokens",
                    numbers(format!(
                        "{} in · {} out",
                        format::tokens(tokens.input + tokens.cache_read + tokens.cache_write),
                        format::tokens(tokens.output)
                    )),
                    &theme,
                )
                .into_any_element(),
            );
        }
        if let (Some(history), Some(key)) = (open.history(), open.main_key()) {
            let viewing = open
                .viewing()
                .filter(|(viewed, _)| viewed == key)
                .map(|(_, version)| *version);
            let read = number(history, viewing.unwrap_or(history.reading)).unwrap_or(0);
            let of = history.versions.len();
            let older = viewing.is_some();
            let opened = HistoryOf {
                key: key.clone(),
                history: history.clone(),
                main: true,
            };
            facts.push(
                kv(
                    "Version",
                    div()
                        .flex()
                        .items_center()
                        .gap(theme.spacing.md)
                        .child(
                            div()
                                .text_color(if older { c.yellow } else { c.fg2 })
                                .child(if older {
                                    format!("v{read} of {of} · older")
                                } else {
                                    format!("v{read} of {of}")
                                }),
                        )
                        .child(
                            div()
                                .id("facts-history")
                                .flex()
                                .items_center()
                                .gap(theme.spacing.xs)
                                .text_color(c.accent_text)
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.history = Some(opened.clone());
                                    cx.notify();
                                }))
                                .child(icon(SbIcon::History, theme.sizes.icon_xs, c.accent_text))
                                .child("History")
                                .automation_target("reader/facts/history"),
                        ),
                    &theme,
                )
                .automation_target("reader/facts/version"),
            );
        }
        if let Some(gone) = open.gone() {
            facts.push(
                kv(
                    "Source",
                    div().child(format!(
                        "gone since {}",
                        format::day(gone.since, format::now())
                    )),
                    &theme,
                )
                .automation_target("reader/facts/source"),
            );
        }

        /* Turns: each prompt's time and start. */
        let turns: Vec<(Option<i64>, String)> = open
            .main()
            .map(|main| {
                main.turns()
                    .iter()
                    .map(|turn| {
                        let first = &main[turn.blocks[0]];
                        let said = match &first.content {
                            Content::Prompt { text, .. } => hostile::label(
                                text.lines()
                                    .find(|line| !line.trim().is_empty())
                                    .unwrap_or(""),
                            ),
                            _ => "(no prompt)".to_string(),
                        };
                        (first.at, said)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let current = self.current_turn(open);
        if current != self.facts_turn {
            self.facts_turn = current;
            if let Some(current) = current {
                self.turns_scroll
                    .scroll_to_item(current, gpui::ScrollStrategy::Nearest);
            }
        }
        let finding = self
            .find
            .as_ref()
            .filter(|find| find.is_active() && find.findings.is_some());
        let found: BTreeMap<usize, usize> =
            finding.map(|find| find.turns.clone()).unwrap_or_default();
        let matches_in = finding
            .and_then(|find| find.findings.as_ref())
            .map(|findings| {
                format!(
                    "{}{} in {}",
                    format::plural(findings.matches.len(), "match").replace("matchs", "matches"),
                    if findings.capped { "+" } else { "" },
                    format::count(found.len() as u64)
                )
            });
        let numbered = turns.len() > 9;
        let count = turns.len();
        let turns = Arc::new(turns);
        let found = Arc::new(found);
        let turn_list = uniform_list("facts-turns", count, {
            let turns = turns.clone();
            let found = found.clone();
            let theme = theme.clone();
            cx.processor(move |_, range: std::ops::Range<usize>, _, cx| {
                let c = &theme.colors;
                range
                    .map(|index| {
                        let (at, said) = &turns[index];
                        let on = current == Some(index);
                        div()
                            .id(("facts-turn", index))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .h(px(TURN_ROW))
                            .px(px(6.))
                            .rounded(theme.radii.control)
                            .text_spec(&theme.text.secondary)
                            .cursor_pointer()
                            .when(on, |this| this.bg(c.s3))
                            .hover(|style| style.bg(c.s2))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.go_to_turn(index, cx)
                            }))
                            .when(numbered, |this| {
                                this.child(
                                    div()
                                        .flex_none()
                                        .w(px(22.))
                                        .flex()
                                        .justify_end()
                                        .text_color(if on { c.fg2 } else { c.fg3 })
                                        .text_spec(&theme.text.number)
                                        .child((index + 1).to_string()),
                                )
                            })
                            .children(at.map(|at| {
                                div()
                                    .flex_none()
                                    .text_color(if on { c.fg2 } else { c.fg3 })
                                    .text_spec(&theme.text.number)
                                    .child(format::clock(at))
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .text_color(if on { c.fg } else { c.fg2 })
                                    .child(said.clone()),
                            )
                            .children(found.get(&index).map(|count| {
                                warn_chip(format::count(*count as u64), &theme)
                                    .h(px(16.))
                                    .px(px(5.))
                            }))
                            .automation_target(format!("reader/facts/turn/{}", index + 1))
                    })
                    .collect()
            })
        })
        .track_scroll(&self.turns_scroll)
        .flex_1()
        .min_h_0()
        .mr(px(-4.));

        /* Calls by kind, of the session's own conversation. */
        let mut calls: BTreeMap<usize, usize> = BTreeMap::new();
        for (_, call) in open.main().into_iter().flat_map(|main| main.calls()) {
            if let Some(at) = CALL_KINDS
                .iter()
                .position(|(_, kinds)| kinds.contains(&call.kind))
            {
                *calls.entry(at).or_default() += 1;
            }
        }
        let calls = calls.into_iter().map(|(at, count)| {
            let (label, kinds) = CALL_KINDS[at];
            div()
                .flex()
                .items_center()
                .gap(theme.spacing.md)
                .text_color(c.fg2)
                .text_spec(&theme.text.secondary)
                .child(icon(kind_icon(kinds[0]), theme.sizes.icon_xs, c.fg3))
                .child(label)
                .child(
                    div()
                        .ml_auto()
                        .text_color(c.fg3)
                        .text_spec(&theme.text.count)
                        .child(format::count(count as u64)),
                )
        });
        let hair = || div().flex_none().h(px(1.)).bg(c.line_subtle);
        div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .w(px(FACTS_WIDTH))
            .h_full()
            .pt(px(18.))
            .pb(px(12.))
            .pl(px(16.))
            .pr(px(16.))
            .bg(c.bg_panel)
            .border_l_1()
            .border_color(c.line_subtle)
            .overflow_hidden()
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(overline("Session", &theme))
                    .children(facts),
            )
            .child(hair())
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(overline(
                        &format!("Turns · {}", format::count(count as u64)),
                        &theme,
                    ))
                    .children(matches_in.map(|said| {
                        div()
                            .ml_auto()
                            .text_color(c.fg3)
                            .text_spec(&theme.text.small)
                            .child(said)
                            .automation_target("reader/facts/matches")
                    })),
            )
            .child(turn_list)
            .child(hair())
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(overline("Calls", &theme))
                    .children(calls),
            )
            .automation_target("reader/facts")
    }

    /**
     * The History popover (DESIGN §4.3): a file's versions, newest first, each with its state
     * (held back, the reading copy, the one being read for a look) and what can be done with
     * it: Read vN, Make vN the reading copy.
     */
    pub(super) fn render_history(&self, of: &HistoryOf, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.sb_theme().clone();
        let c = &theme.colors;
        let reader = cx.services().reader.read(cx);
        let Some(open) = reader.open_session() else {
            return div().into_any_element();
        };
        let provider = agents::name(&open.id.provider, cx);
        let history = &of.history;
        let viewing = open
            .viewing()
            .filter(|(key, _)| *key == of.key)
            .map(|(_, version)| *version);
        let shown = viewing.unwrap_or(history.reading);
        let reading = number(history, history.reading).unwrap_or(0);
        let size = |length: u64| format::file_size(length);
        let copy_length = history
            .versions
            .iter()
            .find(|(version, _)| version.id == history.reading)
            .map_or(0, |(version, _)| version.length);
        let what = if of.main {
            "main transcript".to_string()
        } else {
            hostile::label(super::blocks::file_stem(&of.key))
        };
        let blocks = history.versions.iter().rev().map(|(version, extended)| {
            let n = version.number;
            let is_reading = version.id == history.reading;
            let is_shown = version.id == shown && !is_reading;
            let held = version.held_back.filter(|_| !is_reading);
            let why = match version.reason {
                Reason::FirstSync => "First sync",
                Reason::Rewritten => "Source rewritten",
                Reason::Shrank => "Source shrank",
                Reason::Replaced => "Replaced at the source",
                Reason::Refetched => "Fetched again",
            };
            let mut said = format!("{why} · {}", size(version.length));
            if *extended > 0 {
                said.push_str(&format!(
                    " · extended by {}",
                    format::plural(*extended, "sync")
                ));
            }
            if let Some(held) = held {
                said = match held {
                    Held::Shorter => format!(
                        "{why}: {}, down from {} in v{reading}. {provider} may have rewritten the \
                         file. v{reading} stays the reading copy until you choose.",
                        size(version.length),
                        size(copy_length)
                    ),
                    Held::ParsesWorse => format!(
                        "{why}: it reads worse than v{reading}, fewer of its records could be \
                         read. v{reading} stays the reading copy until you choose."
                    ),
                };
            }
            let key = of.key.clone();
            let (item, id, copy) = (history.item, version.id, history.reading);
            let read = button(
                SharedString::from(format!("history-read-{n}")),
                ButtonKind::Plain,
                true,
                true,
                &theme,
            )
            .on_click(cx.listener({
                let key = key.clone();
                move |this, _: &ClickEvent, _, cx| {
                    this.history = None;
                    let viewing = (id != copy).then(|| (key.clone(), id));
                    let reader = cx.services().reader.clone();
                    reader.update(cx, |reader, cx| reader.view_version(viewing, cx));
                }
            }))
            .child(icon(SbIcon::Eye, theme.sizes.icon_xs, c.fg2))
            .child(format!("Read v{n}"))
            .automation_target(format!("reader/history/read/{n}"));
            let make = button(
                SharedString::from(format!("history-make-{n}")),
                ButtonKind::Plain,
                true,
                true,
                &theme,
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.history = None;
                let reader = cx.services().reader.clone();
                reader.update(cx, |reader, cx| reader.make_reading_copy(item, id, cx));
            }))
            .child(format!("Make v{n} the reading copy"))
            .automation_target(format!("reader/history/make/{n}"));
            let (glyph, chip, fill) = if held.is_some() {
                (
                    Some(
                        icon(SbIcon::TriangleAlert, theme.sizes.icon_sm, c.yellow)
                            .into_any_element(),
                    ),
                    Some(low_chip("held back", Tone::Warn, &theme)),
                    Some(c.yellow_soft),
                )
            } else if is_reading {
                (
                    Some(
                        icon(SbIcon::Check, theme.sizes.icon_sm, c.accent_text).into_any_element(),
                    ),
                    Some(low_chip("reading copy", Tone::Accent, &theme)),
                    None,
                )
            } else if is_shown {
                (
                    Some(icon(SbIcon::Eye, theme.sizes.icon_sm, c.fg2).into_any_element()),
                    Some(low_chip("viewing", Tone::Neutral, &theme)),
                    Some(c.s3),
                )
            } else {
                (None, None, None)
            };
            let damaged = version.damaged;
            let actions: Vec<AnyElement> = if damaged {
                Vec::new()
            } else if held.is_some() {
                vec![read.into_any_element(), make.into_any_element()]
            } else if is_shown {
                vec![make.into_any_element()]
            } else if is_reading && viewing.is_none() {
                Vec::new()
            } else {
                vec![read.into_any_element()]
            };
            div()
                .flex()
                .items_start()
                .gap(px(10.))
                .p(theme.spacing.md)
                .rounded(theme.radii.row)
                .when_some(fill, |this, fill| this.bg(fill))
                .child(
                    div()
                        .flex_none()
                        .w(theme.sizes.icon_sm)
                        .pt(px(1.))
                        .children(glyph),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(theme.spacing.xs)
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .child(
                                    div()
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .child(format!("v{n}")),
                                )
                                .children(chip)
                                .when(damaged, |this| {
                                    this.child(low_chip("damaged", Tone::Bad, &theme))
                                })
                                .child(
                                    div()
                                        .ml_auto()
                                        .text_color(c.fg3)
                                        .text_spec(&theme.text.number)
                                        .child(format!(
                                            "{}, {}",
                                            format::day(version.started_at, format::now()),
                                            format::clock(version.started_at)
                                        )),
                                ),
                        )
                        .child(
                            div()
                                .text_color(c.fg2)
                                .text_spec(&theme.text.secondary)
                                .child(said),
                        )
                        .when(!actions.is_empty(), |this| {
                            this.child(div().flex().gap(px(6.)).children(actions))
                        }),
                )
                .automation_target(format!("reader/history/v{n}"))
        });
        div()
            .id("history")
            .occlude()
            .w(px(HISTORY_WIDTH))
            .max_h(px(560.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .rounded(theme.radii.card)
            .border_1()
            .border_color(c.line)
            .bg(c.s2)
            .shadow(theme.shadows.pop.clone())
            .text_spec(&theme.text.row)
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(theme.spacing.xs)
                    .py(theme.spacing.md)
                    .px(px(14.))
                    .border_b_1()
                    .border_color(c.line_subtle)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(theme.spacing.md)
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("History"),
                            )
                            .child(
                                div()
                                    .text_color(c.fg3)
                                    .text_spec(&theme.text.secondary)
                                    .child(format!(
                                        "{what} · {}",
                                        format::plural(history.versions.len(), "version")
                                    )),
                            )
                            .child(
                                icon_button(
                                    "history-close",
                                    theme.sizes.control_small_height,
                                    &theme,
                                )
                                .ml_auto()
                                .aria_label("Close history")
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.history = None;
                                    cx.notify();
                                }))
                                .child(icon(SbIcon::X, theme.sizes.icon_sm, c.fg2))
                                .automation_target("reader/history/close"),
                            ),
                    )
                    .child(
                        div()
                            .text_color(c.fg3)
                            .text_spec(&theme.text.secondary)
                            .child(
                                "A sync never overwrites. Appends extend a version; any other \
                                 change to the source starts a new one.",
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .p(px(6.))
                    .children(blocks),
            )
            .when(of.main, |this| {
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(theme.spacing.md)
                        .py(px(10.))
                        .px(px(14.))
                        .border_t_1()
                        .border_color(c.line_subtle)
                        .bg(c.s1)
                        .text_color(c.fg3)
                        .text_spec(&theme.text.secondary)
                        .child(icon(SbIcon::Info, theme.sizes.icon_sm, c.fg3))
                        .child(
                            "Subagent transcripts and tool results have their own versions, \
                             listed with each call.",
                        ),
                )
            })
            .automation_target("reader/history")
            .into_any_element()
    }
}
