//! The reader (PLAN §9, DESIGN §4.2): the page of an open session. Its header, then the reading
//! column: the session as a virtualised list of rows ([`rows`]), drawn from what the reader
//! service holds ([`crate::services::reader`]).
//!
//! **The list.** gpui's `list` lays out only the rows on screen and a screenful around them,
//! and keeps the height of each row it has measured. A collapsed activity row is a fixed
//! height; a message is as high as its Markdown lays out. When the service changes, the rows
//! are laid out again and compared with the ones before ([`ReaderView::set_rows`]): only the
//! rows that changed are measured again, and the list keeps what is on screen where it was
//! when rows open or close above it.
//!
//! **Markdown states** ([`crate::markdown`]) are kept for the rows near what is on screen only
//! (PLAN D11): each frame keeps the states of the rows within [`KEEP_ROWS`] of those drawn, and
//! drops the rest.
//!
//! **Loading** is asked for by the rows that need it as they are drawn: a call's body row, a
//! subagent's panel, a summary, a prompt's images. Until it lands they show a skeleton.
//!
//! **Keyboard** (DESIGN §5): `↑` / `↓` move between the rows that take the keyboard, `Enter`
//! opens or closes the one it is on, `→` / `←` open and close it (`←` on a row inside a body or
//! a panel goes to the call above), `]` / `[` go to the next and previous turn, `J` / `K` to
//! the next and previous session, `E` switches Activity / Full calls, `S` stars the session and
//! `Ctrl C` copies the message the keyboard is on. The keyboard's row is held by what it is,
//! not where, so it stays on that row when rows open or load above it, and goes to the row
//! before when its own goes away.

mod blocks;
mod calls;
pub mod rows;

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    ListAlignment, ListState, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, list,
    prelude::FluentBuilder as _, px,
};
use gpui_base::text::TextViewState;
use gpui_component::scroll::Scrollbar;
use sb_core::catalogue::{SessionId, Shelf};
use sb_core::transcript::{Content, ToolInput};

use crate::actions::reader as act;
use crate::automation::AutomationTargetExt as _;
use crate::components::{BANNER_GAP, ButtonKind, bad_banner, button, icon, icon_button};
use crate::format;
use crate::icons::SbIcon;
use crate::keymap;
use crate::markdown::Sanitized;
use crate::services::reader::{BlockRef, OpenSession, ReaderService, ReaderState};
use crate::services::{ActiveServices as _, LibraryService};
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _};
use rows::{Row, RowKind, Space};

/// How far past what is on screen the list lays rows out, so that a row is measured and its
/// Markdown parsed before it scrolls into view.
const OVERDRAW: f32 = 1200.;
/// How many rows either side of those drawn keep their Markdown states (the module docs).
const KEEP_ROWS: usize = 40;
/// How many skeleton rows stand in for the column while a session is parsed.
const SKELETON_ROWS: usize = 6;

/// Which of a row's Markdown bodies a state is: its prose, or a compaction's summary.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Body {
    Prose(BlockRef),
    Summary(BlockRef),
}

/// What [`ReaderView`] tells automation (`scrollback`'s `state.reader`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReaderSnapshot {
    pub rows: usize,
    /// The row the keyboard is on, and what kind of row it is.
    pub focused: Option<(usize, &'static str)>,
    /// The row at the top of the column.
    pub top: usize,
    /// The open activity rows.
    pub open_calls: usize,
    /// How many Markdown states are kept.
    pub states: usize,
    /// Whether a subagent is read on its own.
    pub alone: bool,
}

pub struct ReaderView {
    /// The main area's: where the keyboard rests.
    focus_handle: FocusHandle,
    list: ListState,
    rows: Rc<Vec<Row>>,
    /// The session the rows are of.
    session: Option<SessionId>,
    /// The subagent read on its own, and the row of the call it was opened from.
    alone: Option<(Arc<str>, Option<BlockRef>)>,
    /// Where the keyboard is: the row's place in `rows`, and what the row is.
    focused: Option<(usize, Row)>,
    states: HashMap<Body, (Entity<TextViewState>, usize)>,
    /// The first and last rows drawn in the last frame.
    drawn: Rc<Cell<Option<(usize, usize)>>>,
    _observers: [Subscription; 1],
}

impl ReaderView {
    pub fn new(focus_handle: FocusHandle, cx: &mut Context<Self>) -> Self {
        let reader = cx.services().reader.clone();
        let mut this = Self {
            focus_handle,
            list: ListState::new(0, ListAlignment::Top, px(OVERDRAW)),
            rows: Rc::default(),
            session: None,
            alone: None,
            focused: None,
            states: HashMap::new(),
            drawn: Rc::default(),
            _observers: [cx.observe(&reader, |this, _, cx| this.refresh(cx))],
        };
        this.refresh(cx);
        this
    }

    pub fn snapshot(&self) -> ReaderSnapshot {
        ReaderSnapshot {
            rows: self.rows.len(),
            focused: self
                .focused
                .as_ref()
                .map(|(at, row)| (*at, blocks::kind_name(&row.kind))),
            top: self.list.logical_scroll_top().item_ix,
            open_calls: self
                .rows
                .iter()
                .filter(|row| matches!(row.kind, RowKind::Call { open: true, .. }))
                .count(),
            states: self.states.len(),
            alone: self.alone.is_some(),
        }
    }

    /// Lays the rows out again from the service.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let reader = cx.services().reader.read(cx);
        let (id, rows) = match reader.state() {
            ReaderState::Open(open) => (
                Some(open.id.clone()),
                rows::rows(
                    open,
                    reader.full_calls(),
                    self.alone
                        .as_ref()
                        .map(|(item, call)| (item, call.as_ref())),
                ),
            ),
            ReaderState::Opening(id) | ReaderState::Failed { id, .. } => {
                (Some(id.clone()), Vec::new())
            }
            ReaderState::Closed => (None, Vec::new()),
        };
        if id != self.session {
            self.session = id;
            self.alone = None;
            self.focused = None;
            self.states.clear();
            self.rows = Rc::new(rows);
            self.list.reset(self.rows.len());
            self.focus_first();
        } else {
            self.set_rows(rows);
        }
        cx.notify();
    }

    /**
     * Puts `new` in the place of the rows, telling the list which rows changed: those equal
     * before and after are kept with their measured heights, the rest are measured again. A
     * change is looked for a short way ahead on either side; past that, rows are taken as
     * replaced one for one, which costs measuring them again and nothing else.
     */
    fn set_rows(&mut self, new: Vec<Row>) {
        const AHEAD: usize = 64;
        let old = self.rows.clone();
        let (mut i, mut j) = (0, 0);
        /* Splices in the list's current indices: everything before `j` is already `new`. */
        let mut splices: Vec<(usize, usize, usize)> = Vec::new();
        let mut splice = |at: usize, removed: usize, inserted: usize| match splices.last_mut() {
            Some(last) if last.0 + last.2 == at => {
                last.1 += removed;
                last.2 += inserted;
            }
            _ => splices.push((at, removed, inserted)),
        };
        while i < old.len() && j < new.len() {
            if old[i] == new[j] {
                i += 1;
                j += 1;
                continue;
            }
            let inserted = new[j..].iter().take(AHEAD).position(|row| *row == old[i]);
            let removed = old[i..].iter().take(AHEAD).position(|row| *row == new[j]);
            match (inserted, removed) {
                (Some(count), Some(other)) if other < count => {
                    splice(j, other, 0);
                    i += other;
                }
                (Some(count), _) => {
                    splice(j, 0, count);
                    j += count;
                }
                (None, Some(count)) => {
                    splice(j, count, 0);
                    i += count;
                }
                (None, None) => {
                    splice(j, 1, 1);
                    i += 1;
                    j += 1;
                }
            }
        }
        splice(j, old.len() - i, new.len() - j);
        for (at, removed, inserted) in splices {
            if removed + inserted > 0 {
                self.list.splice(at..at + removed, inserted);
            }
        }
        self.rows = Rc::new(new);
        if old.is_empty() {
            self.focus_first();
        } else {
            self.keep_focus();
        }
    }

    /// Puts the keyboard back on the row it was on, wherever that row is now; on the row
    /// before when it is gone.
    fn keep_focus(&mut self) {
        let Some((was, row)) = self.focused.take() else {
            return;
        };
        let key = blocks::focus_key(&row.kind);
        let found = self
            .rows
            .iter()
            .position(|it| key.is_some() && blocks::focus_key(&it.kind) == key);
        self.focused = match found {
            Some(at) => Some((at, self.rows[at].clone())),
            None => self.rows[..was.min(self.rows.len())]
                .iter()
                .rposition(Row::is_focusable)
                .map(|at| (at, self.rows[at].clone())),
        };
    }

    fn focus_first(&mut self) {
        self.focused = self
            .rows
            .iter()
            .position(Row::is_focusable)
            .map(|at| (at, self.rows[at].clone()));
    }

    fn focus(&mut self, at: usize, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get(at) {
            self.focused = Some((at, row.clone()));
            self.list.scroll_to_reveal_item(at);
            cx.notify();
        }
    }

    /// Moves the keyboard `by` focusable rows.
    fn step(&mut self, by: isize, cx: &mut Context<Self>) {
        let from = self.focused.as_ref().map(|(at, _)| *at);
        let next = match (from, by.is_negative()) {
            (None, _) => self.rows.iter().position(Row::is_focusable),
            (Some(at), false) => self.rows[at + 1..]
                .iter()
                .position(Row::is_focusable)
                .map(|found| at + 1 + found),
            (Some(at), true) => self.rows[..at].iter().rposition(Row::is_focusable),
        };
        if let Some(at) = next {
            self.focus(at, cx);
        }
    }

    /// Goes to the next turn's prompt (`by` 1) or the previous one (`by` -1).
    fn turn(&mut self, by: isize, cx: &mut Context<Self>) {
        let from = self
            .focused
            .as_ref()
            .map_or_else(|| self.list.logical_scroll_top().item_ix, |(at, _)| *at);
        let next = if by > 0 {
            self.rows[(from + 1).min(self.rows.len())..]
                .iter()
                .position(Row::starts_turn)
                .map(|found| from + 1 + found)
        } else {
            let current = self.rows[..=from.min(self.rows.len().saturating_sub(1))]
                .iter()
                .rposition(Row::starts_turn);
            match current {
                Some(at) if at < from => Some(at),
                Some(at) => self.rows[..at].iter().rposition(Row::starts_turn),
                None => None,
            }
        };
        if let Some(at) = next {
            self.focused = Some((at, self.rows[at].clone()));
            self.list.scroll_to(gpui::ListOffset {
                item_ix: at,
                offset_in_item: px(0.),
            });
            cx.notify();
        }
    }

    fn toggle(&mut self, row: &Row, cx: &mut Context<Self>) {
        if let Some(toggle) = row.toggle() {
            let reader = cx.services().reader.clone();
            reader.update(cx, |reader, cx| reader.toggle(toggle, cx));
        }
    }

    fn toggle_focused(&mut self, open: Option<bool>, cx: &mut Context<Self>) {
        let Some((at, row)) = self.focused.clone() else {
            return;
        };
        match open {
            Some(open) if row.toggle().is_some() && row.is_open() == open => {}
            /* `←` on a row that doesn't close goes to the call or panel it is under. */
            Some(false) if row.toggle().is_none() || !row.is_open() => {
                if let Some(above) = self.rows[..at]
                    .iter()
                    .rposition(|it| it.depth < row.depth && it.is_focusable())
                {
                    self.focus(above, cx);
                }
            }
            _ => self.toggle(&row, cx),
        }
    }

    /// Copies what the keyboard is on: a message's text, a call's command or path.
    fn copy(&mut self, cx: &mut Context<Self>) {
        let Some((_, row)) = &self.focused else {
            return;
        };
        let reader = cx.services().reader.read(cx);
        let Some(open) = reader.open_session() else {
            return;
        };
        let text = match &row.kind {
            RowKind::Prompt(at) | RowKind::Task { at, .. } | RowKind::Reply(at) => {
                match open.block(at).map(|block| &block.content) {
                    Some(Content::Prompt { text, .. } | Content::Reply { text }) => {
                        Some(text.clone())
                    }
                    _ => None,
                }
            }
            RowKind::Call { at, .. } => open.call(at).and_then(|call| match &call.input {
                ToolInput::Shell { command, .. } => Some(command.clone()),
                ToolInput::Read { path, .. }
                | ToolInput::Edit { path, .. }
                | ToolInput::Write { path, .. } => Some(path.clone()),
                _ => None,
            }),
            _ => None,
        };
        if let Some(text) = text {
            crate::system::copy_text(text, cx);
        }
    }

    fn scroll_page(&mut self, down: bool, cx: &mut Context<Self>) {
        let height = self.list.viewport_bounds().size.height * 0.9;
        self.list.scroll_by(if down { height } else { -height });
        cx.notify();
    }

    /// Reads the subagent `item` on its own, or goes back to the session with `None`.
    fn read_alone(&mut self, item: Option<(Arc<str>, Option<BlockRef>)>, cx: &mut Context<Self>) {
        let back_to = self.alone.as_ref().and_then(|(_, call)| call.clone());
        self.alone = item;
        self.rows = Rc::default();
        self.states.clear();
        self.focused = None;
        let reader = cx.services().reader.clone();
        let rows = match reader.read(cx).open_session() {
            Some(open) => rows::rows(
                open,
                reader.read(cx).full_calls(),
                self.alone
                    .as_ref()
                    .map(|(item, call)| (item, call.as_ref())),
            ),
            None => Vec::new(),
        };
        self.rows = Rc::new(rows);
        self.list.reset(self.rows.len());
        let call = back_to.filter(|_| self.alone.is_none()).and_then(|call| {
            self.rows
                .iter()
                .position(|row| matches!(&row.kind, RowKind::Call { at, .. } if *at == call))
        });
        match call {
            Some(at) => self.focus(at, cx),
            None => self.focus_first(),
        }
        cx.notify();
    }

    /// Keeps the Markdown state of the body `body` drawn at row `at`, made from `text` the
    /// first time.
    fn markdown_state(
        &mut self,
        body: Body,
        at: usize,
        text: impl FnOnce() -> Option<Sanitized>,
        cx: &mut App,
    ) -> Option<Entity<TextViewState>> {
        if let Some((state, row)) = self.states.get_mut(&body) {
            *row = at;
            return Some(state.clone());
        }
        let state = crate::markdown::state(&text()?, cx);
        self.states.insert(body, (state.clone(), at));
        Some(state)
    }

    /// Drops the Markdown states of the rows far from those drawn last.
    fn drop_far_states(&mut self) {
        let Some((first, last)) = self.drawn.get() else {
            return;
        };
        let keep = first.saturating_sub(KEEP_ROWS)..last + KEEP_ROWS + 1;
        self.states.retain(|_, (_, row)| keep.contains(row));
    }

    fn render_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(ix).cloned() else {
            return div().into_any_element();
        };
        self.drawn.set(Some(match self.drawn.get() {
            Some((first, last)) => (first.min(ix), last.max(ix)),
            None => (ix, ix),
        }));
        let theme = cx.sb_theme().clone();
        let focused = self.focused.as_ref().is_some_and(|(at, _)| *at == ix);
        let content = match &row.kind {
            RowKind::Call { .. }
            | RowKind::Body { .. }
            | RowKind::Panel { .. }
            | RowKind::Unattached { .. } => calls::render(self, ix, &row, focused, window, cx),
            _ => blocks::render(self, ix, &row, focused, window, cx),
        };
        frame(&row, content, &theme)
    }

    fn render_header(&self, theme: &SbTheme, cx: &mut Context<Self>) -> AnyElement {
        let c = &theme.colors;
        let services = cx.services();
        let library = services.library.read(cx);
        let reader = services.reader.read(cx);
        let from: SharedString = match library.shelf() {
            Shelf::All => "All sessions".into(),
            Shelf::Starred => "Starred".into(),
            Shelf::Unsorted => "Unsorted".into(),
            Shelf::Folder(folder) => library
                .folder(folder)
                .map_or_else(SharedString::default, |it| it.name.clone().into()),
        };
        let session = library.open_session();
        let title: SharedString = session
            .and_then(|session| session.title.clone())
            .unwrap_or_else(|| "Untitled session".to_string())
            .into();
        let starred = session.is_some_and(|session| session.starred);
        let place = library.open_place();
        let full_calls = reader.full_calls();
        let segment = |id: &'static str, label: &'static str, on: bool, full: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .h(theme.sizes.segment_height)
                .px(px(9.))
                .rounded(theme.radii.control - px(1.))
                .text_spec(&theme.text.control_small)
                .text_color(if on { c.fg } else { c.fg2 })
                .when(on, |this| this.bg(c.active))
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    let reader = cx.services().reader.clone();
                    reader.update(cx, |reader, cx| reader.set_full_calls(full, cx));
                })
                .child(label)
                .automation_target_with(move || format!("reader/calls/{id}").into())
        };
        let nav = |id: &'static str, glyph, by: isize, enabled: bool, keys: &keymap::Shortcut| {
            icon_button(id, theme.sizes.control_height, theme)
                .aria_label(format!(
                    "{} session ({})",
                    if by > 0 { "Next" } else { "Previous" },
                    keys.label()
                ))
                .when(!enabled, |this| {
                    this.opacity(crate::components::DISABLED_OPACITY)
                })
                .when(enabled, |this| {
                    this.on_click(move |_, _, cx| {
                        let library = cx.services().library.clone();
                        library.update(cx, |library, cx| library.open_step(by, cx));
                    })
                })
                .child(icon(glyph, theme.sizes.icon, c.fg2))
                .automation_target_with(move || format!("reader/{id}").into())
        };
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .h(theme.sizes.reader_header_height)
            .pl(theme.spacing.lg)
            .pr(px(14.))
            .border_b_1()
            .border_color(c.line_subtle)
            .whitespace_nowrap()
            .child(
                icon_button("session-back", theme.sizes.control_height, theme)
                    .aria_label(format!("Back to {from} ({})", keymap::BACK.label()))
                    .on_click(|_, _, cx| {
                        let library = cx.services().library.clone();
                        library.update(cx, LibraryService::close);
                    })
                    .child(icon(SbIcon::ArrowLeft, theme.sizes.icon, c.fg2))
                    .automation_target("session/back"),
            )
            .child(div().flex_none().text_color(c.fg3).child(from))
            .child(icon(SbIcon::ChevronRight, theme.sizes.icon_sm, c.fg3))
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_spec(&theme.text.title)
                    .child(title)
                    .automation_target("session/title"),
            )
            .child(
                icon_button("session-star", theme.sizes.star_button, theme)
                    .aria_label(format!(
                        "{} ({})",
                        if starred { "Unstar" } else { "Star" },
                        keymap::READER_STAR.label()
                    ))
                    .on_click(|_, _, cx| {
                        let library = cx.services().library.clone();
                        library.update(cx, LibraryService::toggle_open_star);
                    })
                    .child(if starred {
                        icon(SbIcon::StarFilled, theme.sizes.icon_sm, c.yellow).into_any_element()
                    } else {
                        icon(SbIcon::Star, theme.sizes.icon_sm, c.fg2).into_any_element()
                    })
                    .automation_target("reader/star"),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.xs)
                    .ml(theme.spacing.xs)
                    .child(nav(
                        "previous",
                        SbIcon::ChevronUp,
                        -1,
                        place.is_some_and(|(at, _)| at > 0),
                        &keymap::PREVIOUS_SESSION,
                    ))
                    .child(nav(
                        "next",
                        SbIcon::ChevronDown,
                        1,
                        place.is_some_and(|(at, of)| at + 1 < of),
                        &keymap::NEXT_SESSION,
                    ))
                    .children(place.map(|(at, of)| {
                        div()
                            .text_color(c.fg3)
                            .text_spec(&theme.text.count)
                            .child(format!(
                                "{} of {}",
                                format::count(at as u64 + 1),
                                format::count(of as u64)
                            ))
                            .automation_target("reader/place")
                    })),
            )
            .child(div().flex_1())
            .child(
                div()
                    .flex_none()
                    .flex()
                    .gap(px(2.))
                    .p(px(2.))
                    .rounded(theme.radii.input)
                    .bg(c.s1)
                    .border_1()
                    .border_color(c.line_subtle)
                    .child(segment("activity", "Activity", !full_calls, false))
                    .child(segment("full", "Full calls", full_calls, true))
                    .automation_target("reader/calls"),
            )
            .into_any_element()
    }

    /// What stands over the column: a source gone at its location, a read that failed.
    fn render_banners(&self, open: &OpenSession, theme: &SbTheme) -> Vec<AnyElement> {
        let c = &theme.colors;
        let mut banners = Vec::new();
        if let Some(gone) = open.gone() {
            banners.push(
                div()
                    .flex()
                    .items_center()
                    .gap(BANNER_GAP)
                    .py(theme.spacing.md)
                    .px(theme.spacing.lg)
                    .rounded(theme.radii.row)
                    .bg(c.s1)
                    .border_1()
                    .border_color(c.line_subtle)
                    .text_spec(&theme.text.row)
                    .child(icon(SbIcon::FileX, theme.sizes.icon_sm, c.fg3))
                    .child(div().flex_1().min_w_0().child(format!(
                        "Gone at {} since {}. This archive holds the only copy.",
                        gone.location,
                        format::day(gone.since, format::now())
                    )))
                    .automation_target("reader/gone")
                    .into_any_element(),
            );
        }
        if let Some(fault) = open.fault() {
            banners.push(
                bad_banner(theme)
                    .text_spec(&theme.text.row)
                    .child(div().flex_1().min_w_0().child(format!(
                        "Part of this session couldn't be read just now: {}. What follows may \
                         stop short.",
                        fault.trim_end_matches('.')
                    )))
                    .child(
                        button("reader-reopen", ButtonKind::Plain, true, true, theme)
                            .on_click(|_, _, cx| {
                                let reader = cx.services().reader.clone();
                                reader.update(cx, ReaderService::reopen);
                            })
                            .child("Try again"),
                    )
                    .automation_target("reader/fault")
                    .into_any_element(),
            );
        }
        banners
    }

    /// The column while the session is parsed: skeleton rows, no spinner (DESIGN §4.14).
    fn render_skeleton(theme: &SbTheme) -> AnyElement {
        column(theme)
            .pt(theme.sizes.reading_top)
            .flex()
            .flex_col()
            .gap(theme.spacing.md)
            .children((0..SKELETON_ROWS).map(|n| {
                div()
                    .h(theme.sizes.activity_row)
                    .w(gpui::relative(if n % 3 == 2 { 0.6 } else { 1. }))
                    .rounded(theme.radii.control)
                    .bg(theme.colors.s1)
            }))
            .automation_target("reader/skeleton")
    }

    fn render_failed(why: &str, theme: &SbTheme) -> AnyElement {
        column(theme)
            .pt(theme.sizes.reading_top)
            .child(
                bad_banner(theme)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(format!("This session can't be opened: {why}.")),
                    )
                    .child(
                        button("reader-retry", ButtonKind::Plain, true, true, theme)
                            .on_click(|_, _, cx| {
                                let reader = cx.services().reader.clone();
                                reader.update(cx, ReaderService::reopen);
                            })
                            .child("Try again"),
                    ),
            )
            .automation_target("reader/failed")
    }
}

/// The reading column: at most 720 wide with 32 at its sides, centred.
fn column(theme: &SbTheme) -> gpui::Div {
    div()
        .w_full()
        .max_w(theme.sizes.reading_width + theme.sizes.reading_side * 2.)
        .mx_auto()
        .px(theme.sizes.reading_side)
}

/**
 * `content` in its place in the column: the space above it, and the frame of each subagent
 * panel it is inside (DESIGN §4.2.7 `nest`: 30 in, 8 from the right, `bg-panel`, a
 * `line-subtle` border, radius 8, 10/12 padding). A panel is drawn a piece per row: its top on
 * the row it opens at, its bottom on the row it closes at.
 */
fn frame(row: &Row, content: AnyElement, theme: &SbTheme) -> AnyElement {
    let c = &theme.colors;
    let mut inner = content;
    for level in (1..=row.depth).rev() {
        let innermost = level == row.depth;
        let opens = innermost && row.opens;
        let closes = level > row.depth - row.closes.min(row.depth);
        inner = div()
            .ml(theme.sizes.call_indent)
            .mr(theme.spacing.md)
            .px(theme.spacing.lg)
            .bg(c.bg_panel)
            .border_l_1()
            .border_r_1()
            .border_color(c.line_subtle)
            .when(opens, |this| {
                this.mt(px(2.))
                    .pt(px(10.))
                    .border_t_1()
                    .rounded_t(theme.radii.row)
            })
            .when(closes, |this| {
                this.mb(px(6.))
                    .pb(px(10.))
                    .border_b_1()
                    .rounded_b(theme.radii.row)
            })
            .child(inner)
            .into_any_element();
    }
    let space = match row.space {
        Space::None => px(0.),
        Space::Group => px(1.),
        Space::Tight => theme.spacing.md,
        Space::Block => theme.sizes.block_gap,
        Space::Top => theme.sizes.reading_top,
    };
    let space = if row.depth > 0 && !row.opens {
        /* Inside a panel, blocks sit closer (`nest`: 6 between its lines). */
        space.min(theme.spacing.sm)
    } else {
        space
    };
    div()
        .w_full()
        .child(column(theme).pt(space).child(inner))
        .into_any_element()
}

impl Render for ReaderView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.sb_theme().clone();
        self.drop_far_states();
        self.drawn.set(None);
        let header = self.render_header(&theme, cx);
        let reader = cx.services().reader.read(cx);
        let body = match reader.state() {
            ReaderState::Closed => div().into_any_element(),
            ReaderState::Opening(_) => Self::render_skeleton(&theme),
            ReaderState::Failed { why, .. } => Self::render_failed(why, &theme),
            ReaderState::Open(open) => {
                let banners = self.render_banners(open, &theme);
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .when(!banners.is_empty(), |this| {
                        this.child(
                            column(&theme)
                                .flex_none()
                                .pt(theme.spacing.lg)
                                .flex()
                                .flex_col()
                                .gap(theme.spacing.md)
                                .children(banners),
                        )
                    })
                    .child(
                        div()
                            .id("reader-column")
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .child(
                                list(
                                    self.list.clone(),
                                    cx.processor(|this, ix, window, cx| {
                                        this.render_row(ix, window, cx)
                                    }),
                                )
                                .size_full()
                                .pb(px(48.)),
                            )
                            .child(Scrollbar::vertical(&self.list))
                            .automation_target("reader/column"),
                    )
                    .into_any_element()
            }
        };
        div()
            .id("reader")
            .size_full()
            // A click in the column leaves the keyboard on the reader, for its keys.
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| this.focus_handle.focus(window, cx)),
            )
            .flex()
            .flex_col()
            .child(header)
            .child(div().flex_1().min_h_0().child(body))
            .automation_target("session")
    }
}

/// `view`'s action on `element`: `run` with the view.
fn on<A: gpui::Action>(
    element: gpui::Stateful<gpui::Div>,
    view: &Entity<ReaderView>,
    run: impl Fn(&mut ReaderView, &mut Window, &mut Context<ReaderView>) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let view = view.clone();
    element.on_action(move |_: &A, window, cx| {
        view.update(cx, |this, cx| run(this, window, cx));
    })
}

/**
 * The reader's keys (the module docs), on `element`: the main area's root, where the keyboard
 * rests, since an action reaches only the elements on the way from the one with the keyboard
 * to the window's root.
 */
pub fn bind_keys(
    element: gpui::Stateful<gpui::Div>,
    view: &Entity<ReaderView>,
) -> gpui::Stateful<gpui::Div> {
    let element = on::<act::Up>(element, view, |this, _, cx| this.step(-1, cx));
    let element = on::<act::Down>(element, view, |this, _, cx| this.step(1, cx));
    let element = on::<act::NextTurn>(element, view, |this, _, cx| this.turn(1, cx));
    let element = on::<act::PreviousTurn>(element, view, |this, _, cx| this.turn(-1, cx));
    let element = on::<act::Toggle>(element, view, |this, window, cx| {
        // `Tab` put the keyboard on one of the page's buttons: `Enter` presses it.
        if this.focus_handle.is_focused(window) {
            this.toggle_focused(None, cx);
        } else {
            cx.propagate();
        }
    });
    let element = on::<act::Expand>(element, view, |this, _, cx| {
        this.toggle_focused(Some(true), cx)
    });
    let element = on::<act::Collapse>(element, view, |this, _, cx| {
        this.toggle_focused(Some(false), cx)
    });
    let element = on::<act::Copy>(element, view, |this, _, cx| this.copy(cx));
    let element = on::<act::PageUp>(element, view, |this, _, cx| this.scroll_page(false, cx));
    let element = on::<act::PageDown>(element, view, |this, _, cx| this.scroll_page(true, cx));
    let element = on::<act::Top>(element, view, |this, _, cx| {
        if let Some(first) = this.rows.iter().position(Row::is_focusable) {
            this.focus(first, cx);
        }
    });
    let element = on::<act::Bottom>(element, view, |this, _, cx| {
        if let Some(last) = this.rows.iter().rposition(Row::is_focusable) {
            this.focus(last, cx);
        }
        // What follows the last row the keyboard takes (a body, how the session ended) too.
        this.list.scroll_to_end();
    });
    element
        .on_action(|_: &act::SwitchCalls, _, cx| {
            let reader = cx.services().reader.clone();
            reader.update(cx, |reader, cx| {
                let full = !reader.full_calls();
                reader.set_full_calls(full, cx);
            });
        })
        .on_action(|_: &act::Star, _, cx| {
            let library = cx.services().library.clone();
            library.update(cx, LibraryService::toggle_open_star);
        })
        .on_action(|_: &act::NextSession, _, cx| {
            let library = cx.services().library.clone();
            library.update(cx, |library, cx| library.open_step(1, cx));
        })
        .on_action(|_: &act::PreviousSession, _, cx| {
            let library = cx.services().library.clone();
            library.update(cx, |library, cx| library.open_step(-1, cx));
        })
}
