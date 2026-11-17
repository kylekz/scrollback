//! The reader (PLAN §9, DESIGN §4.2): the page of an open session. Its header, then the reading
//! column: the session as a virtualised list of rows ([`rows`]), drawn from what the reader
//! service holds ([`crate::services::reader`]).
//!
//! **The list.** gpui's `list` lays out only the rows on screen and a screenful around them,
//! and keeps the height of each row it has measured. Every row is bounded: a collapsed activity
//! row is a fixed height, prose comes a piece a row, text a bounded number of lines a row. When
//! the service changes, the rows are laid out again and matched with the ones before by what
//! they are ([`ReaderView::replace`]): rows that came are spliced in, rows that went are
//! spliced out, rows changed in place are measured again, and the row at the top of the
//! column stays where it is. Only what changes the rows' structure (a row opened or closed,
//! Full calls, a subagent read) lays out the whole session again; a block's prose prepared or a
//! call's body read lays out only its own rows ([`rows::relaid`]), and highlights or an image
//! that land only paint the rows again.
//!
//! **What is kept near the screen.** Markdown states ([`crate::markdown`]) are kept for the
//! rows within [`KEEP_STATES`] of those drawn, and the service's prepared prose, bodies and
//! images for those within [`KEEP_DATA`] (PLAN D11); the rest is let go each frame, and
//! prepared again if it comes back.
//!
//! **Loading** is asked for by the rows that need it as they are drawn. Until it lands they
//! show a skeleton.
//!
//! **Keyboard** (DESIGN §5): `↑` / `↓` move between the rows that take the keyboard, `Enter`
//! opens or closes the one it is on, `→` / `←` open and close it (`←` on a row inside a body or
//! a panel goes to the call above), `]` / `[` go to the next and previous turn, `J` / `K` to
//! the next and previous session, `E` switches Activity / Full calls, `S` stars the session and
//! `Ctrl C` copies the text selected, else the message the keyboard is on. The keyboard's row
//! is held by what it is, not where, so it stays on that row when rows open or load above it,
//! and goes to the row before when its own goes away. Its ring shows once a key is used.
//!
//! **Opened at a search match**, it goes to the match and flashes it ([`place`]).

pub(crate) mod blocks;
mod calls;
mod facts;
#[cfg(test)]
mod features_tests;
mod find;
#[cfg(test)]
mod hostile_tests;
mod kinds;
mod place;
#[cfg(test)]
mod place_tests;
pub mod rows;
#[cfg(test)]
mod tests;

use std::cell::Cell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable as _, InteractiveElement as _,
    IntoElement, ListAlignment, ListState, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Subscription,
    UniformListScrollHandle, Window, div, list, prelude::FluentBuilder as _, px,
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
use crate::services::reader::{
    BlockRef, Changed, Code, Held, Highlights, ImageKey, OpenSession, Rank, ReaderService,
    ReaderState,
};
use crate::services::{ActiveServices as _, LibraryService, ReadAt};
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _};
use rows::{Row, RowKind, Space};

/// How far past what is on screen the list lays rows out, so that a row is measured and its
/// Markdown parsed before it scrolls into view.
const OVERDRAW: f32 = 1200.;
/// How many rows either side of those drawn keep their Markdown states, and their prepared
/// prose, bodies and images (the module docs).
const KEEP_STATES: usize = 40;
const KEEP_DATA: usize = 160;
/// How many skeleton rows stand in for the column while a session is parsed.
const SKELETON_ROWS: usize = 6;

/// A Markdown state: of a piece of a block's prose.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Piece {
    at: BlockRef,
    piece: usize,
}

/// What [`ReaderView`] tells automation (`scrollback`'s `state.reader`).
#[derive(Clone, Debug, PartialEq)]
pub struct ReaderSnapshot {
    pub rows: usize,
    /// The row the keyboard is on, what kind of row it is, and the start of its text.
    pub focused: Option<(usize, &'static str, String)>,
    /// The row at the top of the column, its name ([`blocks::row_name`]), and how far into it
    /// the column is scrolled.
    pub top: usize,
    pub top_name: String,
    pub top_offset: f32,
    /// Whether the column's last row is drawn, its bottom within the column.
    pub at_end: bool,
    /// Whether the column keeps to its end (End) as the rows there change.
    pub following: bool,
    /// The open activity rows.
    pub open_calls: usize,
    /// How many Markdown states, and how many prepared things of the service's, are kept.
    pub states: usize,
    pub held: usize,
    /// Whether a subagent is read on its own.
    pub alone: bool,
    /// The last request to open the session at a search match the reader took.
    pub read_at: Option<ReadAt>,
    /// What flashes: the row name of its block ([`blocks::row_name`]'s, without the row's
    /// kind), its text, and whether it has been drawn.
    pub flash: Option<(String, String, bool)>,
    /// Find's query, its count ("3 of 14"), and where its current match is: the row name of
    /// its block and which of its texts.
    pub find: Option<(String, Option<String>, Option<String>)>,
    /// Whether the facts show: as the margin, or over the column in a narrow window.
    pub facts: bool,
    /// Whose History is open.
    pub history: Option<String>,
    /// The version read for a look, by its number, when one is.
    pub viewing: Option<u32>,
    /// The image shown full size, when one is.
    pub full_image: bool,
}

pub struct ReaderView {
    /// The main area's: where the keyboard rests.
    focus_handle: FocusHandle,
    list: ListState,
    rows: Vec<Row>,
    /// The session the rows are of.
    session: Option<SessionId>,
    /// The subagent read on its own, and the row of the call it was opened from.
    alone: Option<(Arc<str>, Option<BlockRef>)>,
    /// Where the keyboard is: the row's place in `rows`, and what the row is.
    focused: Option<(usize, Row)>,
    /// Whether a key has moved the keyboard in this session: until then no ring is drawn.
    keyboard: bool,
    /// Set when the wheel scrolls the column: the next frame lets go of its end (End keeps to
    /// it until then). Scrolling back down to the end doesn't take it up again.
    wheeled: Rc<Cell<bool>>,
    states: HashMap<Piece, (Entity<TextViewState>, usize)>,
    /// The highlights of the pieces of fenced code drawn, kept as the Markdown states are, once
    /// they are made.
    highlights: HashMap<Piece, (Highlights, usize)>,
    /// Counts the frames, and the row at the top of the column this frame: where a row is, for
    /// the jobs it asks for ([`Rank`]).
    frame: u64,
    top: usize,
    /// What of the service's the rows drawn hold, by the row last drawn with it.
    held: HashMap<Held, usize>,
    /// The first and last rows drawn in the last frame.
    drawn: Rc<Cell<Option<(usize, usize)>>>,
    /// The request to open the session at a search match being gone to, until it is there
    /// ([`place`]), and the last one taken.
    placing: Option<ReadAt>,
    read_at: Option<ReadAt>,
    flash: Option<place::Flash>,
    /// Find in session, once there was a window to make its field in ([`find`]); and what to
    /// fill it with when the view is next drawn.
    find: Option<find::Find>,
    filling: Option<(String, Option<(BlockRef, String)>)>,
    /// The History popover open ([`facts`]), and how many of its versions it lays out.
    history: Option<facts::HistoryOf>,
    history_shown: usize,
    /// The facts' turns, and the turn they last showed current.
    turns_scroll: UniformListScrollHandle,
    facts_turn: Option<usize>,
    /// Whether the facts show over the column, in a window too narrow for the margin.
    facts_open: bool,
    /// The image a tool returned shown full size, over the column.
    full_image: Option<ImageKey>,
    /// Whether the margin showed last frame: the window was wide enough.
    wide: bool,
    /// The reader service's count of opens when the rows were last laid out of an open session.
    generation: u64,
    _observers: [Subscription; 3],
}

impl ReaderView {
    pub fn new(focus_handle: FocusHandle, cx: &mut Context<Self>) -> Self {
        let (reader, library) = (cx.services().reader.clone(), cx.services().library.clone());
        let mut this = Self {
            focus_handle,
            list: ListState::new(0, ListAlignment::Top, px(OVERDRAW)),
            rows: Vec::new(),
            session: None,
            alone: None,
            focused: None,
            keyboard: false,
            wheeled: Rc::default(),
            states: HashMap::new(),
            highlights: HashMap::new(),
            frame: 0,
            top: 0,
            held: HashMap::new(),
            drawn: Rc::default(),
            placing: None,
            read_at: None,
            flash: None,
            find: None,
            filling: None,
            history: None,
            history_shown: facts::HISTORY_SHOWN,
            turns_scroll: UniformListScrollHandle::new(),
            facts_turn: None,
            facts_open: false,
            full_image: None,
            wide: true,
            generation: 0,
            _observers: [
                cx.observe(&reader, |this, _, cx| this.refresh(cx)),
                cx.subscribe(&reader, |this, _, changed, cx| this.changed(changed, cx)),
                // The same session opened at a match again: no parse, a new request.
                cx.observe(&library, |this, _, cx| this.take_read_at(cx)),
            ],
        };
        this.list.set_scroll_handler({
            let wheeled = this.wheeled.clone();
            move |_, _, _| wheeled.set(true)
        });
        this.refresh(cx);
        this
    }

    pub fn snapshot(&self, cx: &App) -> ReaderSnapshot {
        let reader = cx.services().reader.read(cx);
        let open = reader.open_session();
        let scroll_top = self.list.logical_scroll_top();
        let top = scroll_top.item_ix;
        ReaderSnapshot {
            rows: self.rows.len(),
            focused: self.focused.as_ref().map(|(at, row)| {
                (
                    *at,
                    blocks::kind_name(&row.kind),
                    open.map(|open| blocks::row_text(row, open))
                        .unwrap_or_default()
                        .chars()
                        .take(80)
                        .collect(),
                )
            }),
            top,
            top_name: match (self.rows.get(top), open) {
                (Some(row), Some(open)) => blocks::row_name(row, open),
                _ => String::new(),
            },
            top_offset: scroll_top.offset_in_item.into(),
            following: self.list.is_following_tail(),
            at_end: self
                .rows
                .len()
                .checked_sub(1)
                .and_then(|last| self.list.bounds_for_item(last))
                .is_some_and(|last| {
                    let column = self.list.viewport_bounds();
                    last.bottom() <= column.bottom() + px(1.)
                }),
            open_calls: self
                .rows
                .iter()
                .filter(|row| matches!(row.kind, RowKind::Call { open: true, .. }))
                .count(),
            states: self.states.len(),
            held: self.held.len(),
            alone: self.alone.is_some(),
            read_at: self.read_at.clone(),
            flash: self.flash().map(|(at, text, drawn)| {
                let name = open.map_or_else(String::new, |open| {
                    format!(
                        "{}/{}",
                        blocks::conversation_name(&at.conversation, open),
                        at.block.0
                    )
                });
                (name, text.to_string(), drawn)
            }),
            find: self
                .find
                .as_ref()
                .filter(|find| find.is_active())
                .map(|find| {
                    let current = find
                        .current
                        .and_then(|at| find.findings.as_ref()?.matches.get(at))
                        .and_then(|found| {
                            let at = found.spot.block();
                            let name = format!(
                                "{}/{}",
                                blocks::conversation_name(&at.conversation, open?),
                                at.block.0
                            );
                            Some(format!("{name} {}", find::spot_kind(&found.spot)))
                        });
                    (find.query.clone(), find.count(), current)
                }),
            facts: self.wide || self.facts_open,
            history: self.history.as_ref().map(|of| of.key.to_string()),
            viewing: open.and_then(|open| {
                let (_, version) = open.viewing()?;
                Some(facts::number(open.history()?, *version).unwrap_or(0))
            }),
            full_image: self.full_image.is_some(),
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
        let generation = reader.generation();
        let reopened = open_now(reader) && generation != self.generation && id == self.session;
        if open_now(reader) {
            self.generation = generation;
        }
        let waited = open_now(reader) && self.find_waits();
        if id != self.session {
            self.session = id;
            self.alone = None;
            self.focused = None;
            self.keyboard = false;
            self.placing = None;
            self.read_at = None;
            self.flash = None;
            self.history = None;
            self.facts_open = false;
            self.full_image = None;
            self.facts_turn = None;
            if let Some(find) = self.find.take() {
                find.stop();
            }
            self.states.clear();
            self.held.clear();
            self.rows = rows;
            self.list.set_follow_mode(gpui::FollowMode::Normal);
            self.list.reset(self.rows.len());
            self.focus_first();
        } else {
            /* Parsed again (another version read): a block at the same place may say
            something else now, so what was made from its text goes. */
            if reopened {
                self.states.clear();
                self.highlights.clear();
            }
            self.replace(0..self.rows.len(), rows);
        }
        if reopened || waited {
            self.reopened(cx);
        }
        self.take_read_at(cx);
        self.place(cx);
        self.reveal_pending(cx);
        cx.notify();
    }

    /**
     * Lays out again the rows of what `changed` in the service, or paints them again (the
     * module docs): its rows are looked for from where they were last drawn, outwards.
     */
    fn changed(&mut self, changed: &Changed, cx: &mut Context<Self>) {
        let (of, held) = match changed {
            Changed::Paint => return cx.notify(),
            Changed::Highlight(code) => {
                if self.held.contains_key(&Held::Highlight(code.clone())) {
                    cx.notify();
                }
                return;
            }
            Changed::Prose(at) => (at, Held::Prose(at.clone())),
            Changed::Body(at) => (at, Held::Body(at.clone())),
            Changed::Raw(at) => (at, Held::Raw(at.clone())),
        };
        let laid = |row: &Row| match (&row.kind, changed) {
            (RowKind::Prose { at, .. }, Changed::Prose(_))
            | (RowKind::Body { at, .. }, Changed::Body(_))
            | (RowKind::Raw { at, .. }, Changed::Raw(_)) => at == of,
            _ => false,
        };
        let reader = cx.services().reader.read(cx);
        let Some(open) = reader
            .open_session()
            .filter(|open| Some(&open.id) == self.session.as_ref())
        else {
            return;
        };
        let from = self.held.get(&held).copied().unwrap_or(self.top);
        let Some(found) = outwards(self.rows.len(), from).find(|&at| laid(&self.rows[at])) else {
            return;
        };
        let start = (0..found)
            .rev()
            .take_while(|&at| laid(&self.rows[at]))
            .last()
            .unwrap_or(found);
        let end = (found..self.rows.len())
            .take_while(|&at| laid(&self.rows[at]))
            .last()
            .unwrap_or(found)
            + 1;
        let rows = rows::relaid(open, reader.full_calls(), &self.rows[start..end]);
        self.replace(start..end, rows);
        self.reveal_pending(cx);
        cx.notify();
    }

    /**
     * Puts `new` in the place of the rows `range` (the module docs), by [`diff`]. The row at
     * the top of the column stays there, as far into it as it was; when it is gone (a body or
     * a panel closed under it), the row before it that is left takes its place, from its top:
     * the call whose body it was.
     */
    fn replace(&mut self, range: Range<usize>, new: Vec<Row>) {
        let top = self.list.logical_scroll_top();
        let old = &self.rows[range.clone()];
        let (splices, changed) = diff(old, &new);
        /* At the end, the list keeps itself there. */
        let anchor = (range.contains(&top.item_ix) && !self.list.is_following_tail())
            .then(|| {
                let in_new: HashMap<RowKind, usize> = new
                    .iter()
                    .enumerate()
                    .map(|(at, row)| (row.key(), at))
                    .collect();
                let at = top.item_ix - range.start;
                in_new
                    .get(&old[at].key())
                    .map(|&found| (range.start + found, top.offset_in_item))
                    .or_else(|| {
                        old[..at]
                            .iter()
                            .rev()
                            .find_map(|row| in_new.get(&row.key()))
                            .map(|&found| (range.start + found, px(0.)))
                    })
                    /* The rows before `range` are all kept. */
                    .or_else(|| range.start.checked_sub(1).map(|before| (before, px(0.))))
            })
            .flatten();
        for (at, removed, inserted) in splices {
            let at = range.start + at;
            self.list.splice(at..at + removed, inserted);
        }
        for at in changed {
            let at = range.start + at;
            self.list.remeasure_items(at..at + 1);
        }
        if let Some((item_ix, offset_in_item)) = anchor {
            self.list.scroll_to(gpui::ListOffset {
                item_ix,
                offset_in_item,
            });
        }
        let (removed, inserted) = (range.len(), new.len());
        let first = self.rows.is_empty();
        self.rows.splice(range.clone(), new);
        if first {
            self.focus_first();
        } else {
            self.keep_focus(range.start, removed, inserted);
        }
    }

    /**
     * Puts the keyboard back on the row it was on, wherever that row is now, once the `removed`
     * rows from `start` were replaced by `inserted` rows; on the row before when it is gone.
     */
    fn keep_focus(&mut self, start: usize, removed: usize, inserted: usize) {
        let Some((was, row)) = self.focused.take() else {
            return;
        };
        if was < start {
            self.focused = Some((was, row));
            return;
        }
        if was >= start + removed {
            let at = was - removed + inserted;
            self.focused = Some((at, self.rows[at].clone()));
            return;
        }
        let key = blocks::focus_key(&row.kind);
        let found = self.rows[start..start + inserted]
            .iter()
            .position(|it| key.is_some() && blocks::focus_key(&it.kind) == key)
            .map(|at| start + at);
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
        if self.rows.is_empty() {
            return;
        }
        let from = self
            .focused
            .as_ref()
            .map_or_else(|| self.list.logical_scroll_top().item_ix, |(at, _)| *at)
            .min(self.rows.len() - 1);
        let next = if by > 0 {
            self.rows[from + 1..]
                .iter()
                .position(Row::starts_turn)
                .map(|found| from + 1 + found)
        } else {
            match self.rows[..=from].iter().rposition(Row::starts_turn) {
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
        /* A row opened or closed stays where it is: the column no longer keeps to its end. */
        self.list.set_follow_mode(gpui::FollowMode::Normal);
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

    /// Copies the text selected in a message, else what the keyboard is on: a message's text,
    /// a call's command or path (DESIGN §4.2.2, §5).
    fn copy(&mut self, cx: &mut Context<Self>) {
        let selected = self
            .states
            .values()
            .map(|(state, _)| state.read(cx).selected_text())
            .find(|text| !text.is_empty());
        if let Some(text) = selected {
            crate::system::copy_text(text, cx);
            return;
        }
        let Some((_, row)) = &self.focused else {
            return;
        };
        let reader = cx.services().reader.read(cx);
        let Some(open) = reader.open_session() else {
            return;
        };
        let text = match &row.kind {
            RowKind::Call { at, .. } => open.call(at).and_then(|call| match &call.input {
                ToolInput::Shell { command, .. } => {
                    Some(crate::hostile::bidi(command).into_owned())
                }
                ToolInput::Read { path, .. }
                | ToolInput::Edit { path, .. }
                | ToolInput::Write { path, .. } => Some(crate::hostile::bidi(path).into_owned()),
                _ => None,
            }),
            _ => row.block().and_then(|at| message_text(open, at)),
        };
        if let Some(text) = text {
            crate::system::copy_text(text, cx);
        }
    }

    /// Scrolls a screen, and puts the keyboard on the first row of it that takes it.
    fn scroll_page(&mut self, down: bool, cx: &mut Context<Self>) {
        let height = self.list.viewport_bounds().size.height * 0.9;
        self.list.scroll_by(if down { height } else { -height });
        let top = self.list.logical_scroll_top().item_ix;
        if let Some(at) = self.rows.get(top..).and_then(|rows| {
            rows.iter()
                .position(Row::is_focusable)
                .map(|found| top + found)
        }) {
            self.focused = Some((at, self.rows[at].clone()));
        }
        cx.notify();
    }

    /// Reads the subagent `item` on its own, or goes back to the session with `None`.
    fn read_alone(&mut self, item: Option<(Arc<str>, Option<BlockRef>)>, cx: &mut Context<Self>) {
        let back_to = self.alone.as_ref().and_then(|(_, call)| call.clone());
        self.alone = item;
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
        self.rows = rows;
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

    /// Keeps the Markdown state of the piece `piece` drawn at row `at`, made from `text` the
    /// first time.
    fn markdown_state(
        &mut self,
        piece: Piece,
        at: usize,
        text: impl FnOnce() -> Option<Sanitized>,
        cx: &mut App,
    ) -> Option<Entity<TextViewState>> {
        let state = match self.states.get_mut(&piece) {
            Some((state, row)) => {
                *row = at;
                state.clone()
            }
            None => {
                let state = crate::markdown::state(&text()?, cx);
                self.states.insert(piece.clone(), (state.clone(), at));
                state
            }
        };
        self.light_prose(&piece, &state, cx);
        if self.flash.as_ref().is_none_or(|flash| flash.at != piece.at) {
            self.light_found(&piece, &state, cx);
        }
        Some(state)
    }

    /// Where the row `at` is drawn, for the jobs it asks for.
    fn rank(&self, at: usize) -> Rank {
        Rank {
            frame: self.frame,
            distance: at.abs_diff(self.top),
        }
    }

    /**
     * The highlights of the piece of code `piece` drawn at row `at`, `code` in `language`: kept
     * once they are made, and asked of the reader service until then, which makes them off
     * the UI thread. Until they are made the code is plain.
     */
    fn highlights(
        &mut self,
        piece: Piece,
        at: usize,
        language: Option<&str>,
        code: &str,
        cx: &mut Context<Self>,
    ) -> Highlights {
        if let Some((highlights, row)) = self.highlights.get_mut(&piece) {
            *row = at;
            return highlights.clone();
        }
        let Some(language) = language.filter(|_| code.len() <= crate::syntax::MAX_HIGHLIGHT_BYTES)
        else {
            self.highlights.insert(piece, (Highlights::default(), at));
            return Highlights::default();
        };
        let code = Code {
            language: language.into(),
            text: code.into(),
        };
        let rank = self.rank(at);
        let reader = cx.services().reader.clone();
        let made = reader.update(cx, |reader, cx| {
            let made = reader.highlighted(&code);
            if made.is_none() {
                reader.want_highlight(code.clone(), rank, cx);
            }
            made
        });
        match made {
            Some(highlights) => {
                self.highlights.insert(piece, (highlights.clone(), at));
                highlights
            }
            None => {
                self.hold(Held::Highlight(code), at);
                Highlights::default()
            }
        }
    }

    /// Notes that the row `at` drew with `held`, so it is kept while the row is near.
    fn hold(&mut self, held: Held, at: usize) {
        self.held.insert(held, at);
    }

    /// Lets go of the Markdown states, and of the service's prose, bodies and images, of the
    /// rows far from those drawn last (the module docs).
    fn let_go_of_far(&mut self, cx: &mut Context<Self>) {
        let Some((first, last)) = self.drawn.get() else {
            return;
        };
        let near = |keep: usize| first.saturating_sub(keep)..last + keep + 1;
        let states = near(KEEP_STATES);
        self.states.retain(|_, (_, row)| states.contains(row));
        self.highlights.retain(|_, (_, row)| states.contains(row));
        let data = near(KEEP_DATA);
        let far: Vec<Held> = self
            .held
            .iter()
            .filter(|(_, row)| !data.contains(row))
            .map(|(held, _)| held.clone())
            .collect();
        if far.is_empty() {
            return;
        }
        for held in &far {
            self.held.remove(held);
        }
        let reader = cx.services().reader.clone();
        cx.defer(move |cx| reader.update(cx, |reader, cx| reader.release(far, cx)));
    }

    fn render_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.rows.get(ix).cloned() else {
            return div().into_any_element();
        };
        self.drawn.set(Some(match self.drawn.get() {
            Some((first, last)) => (first.min(ix), last.max(ix)),
            None => (ix, ix),
        }));
        /* A subagent's conversation is kept while a row of it, or of its panel, is near. */
        match &row.kind {
            RowKind::Panel { item, .. } | RowKind::Unattached { item, .. } => {
                self.hold(Held::Subagent(item.clone()), ix)
            }
            RowKind::Raw { at, .. } => self.hold(Held::Raw(at.clone()), ix),
            _ => {}
        }
        if let Some(at) = row.block() {
            self.hold(Held::Subagent(at.conversation.clone()), ix);
        }
        let theme = cx.sb_theme().clone();
        let focused = self.keyboard && self.focused.as_ref().is_some_and(|(at, _)| *at == ix);
        let content = match &row.kind {
            RowKind::Call { .. }
            | RowKind::CallIo { .. }
            | RowKind::Body { .. }
            | RowKind::Panel { .. }
            | RowKind::Unattached { .. } => calls::render(self, ix, &row, focused, window, cx),
            _ => blocks::render(self, ix, &row, focused, window, cx),
        };
        let framed = frame(&row, content, &theme);
        if !row.is_focusable() {
            return framed;
        }
        /* A click on a row the keyboard can rest on puts it there, as an arrow would. */
        div()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if let Some(row) = this.rows.get(ix).cloned() {
                        this.focused = Some((ix, row));
                        cx.notify();
                    }
                }),
            )
            .child(framed)
            .into_any_element()
    }

    fn render_header(
        &self,
        window: &Window,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let services = cx.services();
        let library = services.library.read(cx);
        let reader = services.reader.read(cx);
        let from: SharedString = match library.shelf() {
            Shelf::All => "All sessions".into(),
            Shelf::Starred => "Starred".into(),
            Shelf::Unsorted => "Unsorted".into(),
            Shelf::Trash => "Trash".into(),
            Shelf::Folder(folder) => library
                .folder(folder)
                .map_or_else(SharedString::default, |it| it.name.clone().into()),
        };
        let session = library.open_session();
        let title: SharedString = session
            .and_then(|session| session.title.as_deref().map(crate::hostile::label))
            .unwrap_or_else(|| "Untitled session".to_string())
            .into();
        /* A copy read from its original stars the original (DESIGN §4.2.8). */
        let starred = library.reading_for().is_some_and(|session| session.starred);
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
                .on_click({
                    let list = self.list.clone();
                    move |_, _, cx| {
                        list.set_follow_mode(gpui::FollowMode::Normal);
                        let reader = cx.services().reader.clone();
                        reader.update(cx, |reader, cx| reader.set_full_calls(full, cx));
                    }
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
            .child(self.render_find(window, theme, cx))
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
            .when(!self.wide, |this| {
                this.child(
                    icon_button("facts-toggle", theme.sizes.control_height, theme)
                        .debug_selector(|| "facts-toggle".into())
                        .aria_label("Facts")
                        .when(self.facts_open, |this| this.bg(c.s3))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.facts_open = !this.facts_open;
                            cx.notify();
                        }))
                        .child(icon(SbIcon::PanelRight, theme.sizes.icon, c.fg2))
                        .automation_target("reader/facts-toggle"),
                )
            })
            .into_any_element()
    }

    /**
     * The header's Find field (DESIGN §4.2.9): 240 wide, 300 and ringed while it has the
     * keyboard; the count, and the previous, next and close buttons while it has a query.
     */
    fn render_find(&self, window: &Window, theme: &SbTheme, cx: &mut Context<Self>) -> AnyElement {
        let c = &theme.colors;
        let Some(find) = &self.find else {
            return div().into_any_element();
        };
        let focused = find.input.focus_handle(cx).is_focused(window);
        let active = find.is_active();
        let count = find.count();
        let none = find
            .findings
            .as_ref()
            .is_some_and(|findings| findings.matches.is_empty());
        let step = |id: &'static str, glyph, label: &'static str, by: isize| {
            icon_button(id, theme.sizes.control_height, theme)
                .aria_label(label)
                .on_click(cx.listener(move |this, _, _, cx| this.step_match(by, cx)))
                .child(icon(glyph, theme.sizes.icon, c.fg2))
                .automation_target_with(move || format!("reader/find/{id}").into())
        };
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(theme.spacing.xs)
            .child(
                div()
                    .id("find-field")
                    .flex_none()
                    .w(if focused || active {
                        px(300.)
                    } else {
                        px(240.)
                    })
                    .h(theme.sizes.control_height)
                    .flex()
                    .items_center()
                    .gap(theme.spacing.md)
                    .px(theme.sizes.list_row_padding)
                    .rounded(theme.radii.input)
                    .border_1()
                    .border_color(if focused || active { c.accent } else { c.line })
                    .when(focused || active, |this| {
                        this.shadow(crate::components::focus_ring(false, theme))
                    })
                    .bg(c.s1)
                    .text_spec(&theme.text.secondary)
                    .capture_action(cx.listener(
                        |this, _: &gpui_component::input::Escape, window, cx| {
                            this.close_find(window, cx);
                            cx.stop_propagation();
                        },
                    ))
                    .child(icon(SbIcon::Search, theme.sizes.icon_sm, c.fg3))
                    .child(
                        div().flex_1().min_w_0().child(
                            gpui_component::input::Input::new(&find.input)
                                .appearance(false)
                                .px_0(),
                        ),
                    )
                    .children(count.filter(|_| active).map(|count| {
                        div()
                            .flex_none()
                            .text_color(if none { c.fg3 } else { c.fg2 })
                            .text_spec(&theme.text.count)
                            .child(count)
                            .automation_target("reader/find/count")
                    }))
                    .when(!focused && !active, |this| {
                        this.child(crate::components::kbd(keymap::FIND.label(), theme))
                    })
                    .automation_target("reader/find/field"),
            )
            .when(active, |this| {
                this.child(step(
                    "previous-match",
                    SbIcon::ChevronUp,
                    "Previous match",
                    -1,
                ))
                .child(step("next-match", SbIcon::ChevronDown, "Next match", 1))
                .child(
                    icon_button("close-find", theme.sizes.control_height, theme)
                        .aria_label("Close find")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.close_find(window, cx);
                        }))
                        .child(icon(SbIcon::X, theme.sizes.icon, c.fg2))
                        .automation_target("reader/find/close"),
                )
            })
            .into_any_element()
    }

    /**
     * What stands over the column: an older version read for a look (DESIGN §4.3), a source
     * gone at its location, a version that failed verification, a read that failed (§4.14).
     */
    fn render_banners(&self, open: &OpenSession, theme: &SbTheme, cx: &App) -> Vec<AnyElement> {
        let c = &theme.colors;
        let mut banners = Vec::new();
        if let Some((key, version)) = open.viewing() {
            let main = open.main_key() == Some(key);
            let history = if main {
                open.history()
            } else {
                open.viewed_history()
            };
            let of = history.map_or(0, |history| history.versions.len());
            let shown = history.and_then(|history| {
                history
                    .versions
                    .iter()
                    .find(|(it, _)| it.id == *version)
                    .map(|(it, _)| it.clone())
            });
            let reading = history.and_then(|history| facts::number(history, history.reading));
            let n = shown.as_ref().map_or(0, |it| it.number);
            let synced = shown.as_ref().map_or_else(String::new, |it| {
                format!(
                    ", synced {} at {}",
                    format::day(it.started_at, format::now()),
                    format::clock(it.started_at)
                )
            });
            let what = if main {
                String::new()
            } else {
                format!(" of {}", crate::hostile::label(blocks::file_stem(key)))
            };
            let item = history.map(|history| history.item);
            let id = *version;
            banners.push(
                crate::components::warn_banner(theme)
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child(format!("Reading v{n} of {of}{what}{synced}.")),
                            )
                            .child(div().text_color(c.fg2).child(match reading {
                                Some(reading) => format!(
                                    "This is an older version. The reading copy is v{reading}."
                                ),
                                None => "This is an older version.".to_string(),
                            })),
                    )
                    .child(
                        button("reader-back-to-copy", ButtonKind::Plain, true, true, theme)
                            .on_click(|_, _, cx| {
                                let reader = cx.services().reader.clone();
                                reader.update(cx, |reader, cx| reader.view_version(None, cx));
                            })
                            .child("Back to the reading copy")
                            .automation_target("reader/older/back"),
                    )
                    .children(item.map(|item| {
                        button("reader-make-copy", ButtonKind::Plain, true, true, theme)
                            .on_click(move |_, _, cx| {
                                let reader = cx.services().reader.clone();
                                reader.update(cx, |reader, cx| {
                                    reader.make_reading_copy(item, id, cx)
                                });
                            })
                            .child(format!("Make v{n} the reading copy"))
                            .automation_target("reader/older/make")
                    }))
                    .automation_target("reader/older")
                    .into_any_element(),
            );
        }
        let damaged = open.history().is_some_and(|history| {
            history
                .versions
                .iter()
                .any(|(version, _)| version.id == history.reading && version.damaged)
        });
        if let Some(why) = cx.services().reader.read(cx).copy_failure() {
            banners.push(
                bad_banner(theme)
                    .text_spec(&theme.text.row)
                    .child(div().flex_1().min_w_0().child(format!(
                        "That version couldn't be made the reading copy: {}.",
                        crate::hostile::label(why.trim_end_matches('.'))
                    )))
                    .child(
                        button("reader-copy-dismiss", ButtonKind::Plain, true, true, theme)
                            .on_click(|_, _, cx| {
                                let reader = cx.services().reader.clone();
                                reader.update(cx, ReaderService::dismiss_copy_failure);
                            })
                            .child("Dismiss"),
                    )
                    .automation_target("reader/copy-failed")
                    .into_any_element(),
            );
        }
        if damaged && open.viewing().is_none() {
            /* DESIGN §4.14, as far as the archive knows: when verify found it; the turn the
            damage starts at isn't known. */
            let found = open.damaged_on().map_or_else(String::new, |on| {
                format!(" on {}", format::day(on, format::now()))
            });
            banners.push(
                bad_banner(theme)
                    .text_spec(&theme.text.row)
                    .child(div().flex_1().min_w_0().child(format!(
                        "Part of this session failed verification{found}. What follows the \
                         damage may be wrong."
                    )))
                    .child(
                        button(
                            "reader-damage-details",
                            ButtonKind::Plain,
                            true,
                            true,
                            theme,
                        )
                        .on_click(|_, _, cx| {
                            let settings = cx.services().settings.clone();
                            settings.update(cx, |settings, cx| settings.show(true, cx));
                        })
                        .child("Details"),
                    )
                    .automation_target("reader/damaged")
                    .into_any_element(),
            );
        }
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
                        crate::hostile::label(&gone.location),
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

    /**
     * `Esc` (DESIGN §5): closes the image shown full size, else History, else the facts over
     * the column, else Find. Whether it closed anything.
     */
    pub fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.full_image.take().is_some() || self.history.take().is_some() {
            cx.notify();
            return true;
        }
        if std::mem::take(&mut self.facts_open) {
            cx.notify();
            return true;
        }
        self.close_find(window, cx)
    }

    /// An image a tool returned, full size over the column, while one is shown: a click or
    /// `Esc` closes it.
    fn render_full_image(&self, theme: &SbTheme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let key = self.full_image.as_ref()?;
        let reader = cx.services().reader.read(cx);
        let Some(crate::services::reader::Load::Loaded(picture)) =
            reader.open_session()?.image(key).cloned()
        else {
            return None;
        };
        Some(
            div()
                .id("full-image")
                .occlude()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .p(theme.spacing.xl)
                .bg(theme.colors.scrim)
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.full_image = None;
                    cx.notify();
                }))
                .child(
                    gpui::img(gpui::ImageSource::Render(picture.image))
                        .max_w_full()
                        .max_h_full()
                        .w(px(picture.width as f32))
                        .h(px(picture.height as f32))
                        .object_fit(gpui::ObjectFit::Contain)
                        .into_any_element(),
                )
                .automation_target("reader/full-image")
                .into_any_element(),
        )
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

/**
 * The text of the message at `at`, a prompt or a reply, to copy: as it was written, its
 * bidirectional controls as they were shown (`⟦U+202E⟧`), never the controls themselves.
 */
fn message_text(open: &OpenSession, at: &BlockRef) -> Option<String> {
    match &open.block(at)?.content {
        Content::Prompt { text, .. } | Content::Reply { text } => {
            Some(crate::hostile::bidi(text).into_owned())
        }
        _ => None,
    }
}

/// Whether `reader` has a session open.
fn open_now(reader: &ReaderService) -> bool {
    matches!(reader.state(), ReaderState::Open(_))
}

/// The places of `len` rows from `from` outwards: `from`, the one after it, the one before, …
fn outwards(len: usize, from: usize) -> impl Iterator<Item = usize> {
    let from = from.min(len);
    (0..len)
        .flat_map(move |step| [from.checked_add(step), from.checked_sub(step + 1)])
        .flatten()
        .filter(move |&at| at < len)
}

/**
 * How to turn the rows `old` into `new`: the splices to make, in order (where, in the list as it
 * is being changed, how many rows go and how many come), and the places in `new` of the rows
 * kept but changed, to measure again. Rows are matched by [`Row::key`]: one in both, in order,
 * is kept; one only in the old rows is spliced out, one only in the new spliced in. What is
 * matched out of order is spliced out and in again.
 */
fn diff(old: &[Row], new: &[Row]) -> (Vec<(usize, usize, usize)>, Vec<usize>) {
    let old_keys: Vec<RowKind> = old.iter().map(Row::key).collect();
    let new_keys: Vec<RowKind> = new.iter().map(Row::key).collect();
    let in_new: HashMap<&RowKind, usize> = new_keys
        .iter()
        .enumerate()
        .map(|(at, key)| (key, at))
        .collect();
    let in_old: HashMap<&RowKind, usize> = old_keys
        .iter()
        .enumerate()
        .map(|(at, key)| (key, at))
        .collect();
    /* In the list's indices as it is being changed: everything before `j` is already new. */
    let mut splices: Vec<(usize, usize, usize)> = Vec::new();
    let mut changed: Vec<usize> = Vec::new();
    let mut splice = |at: usize, removed: usize, inserted: usize| match splices.last_mut() {
        Some(last) if last.0 + last.2 == at => {
            last.1 += removed;
            last.2 += inserted;
        }
        _ => splices.push((at, removed, inserted)),
    };
    let (mut i, mut j) = (0, 0);
    while i < old.len() && j < new.len() {
        if old_keys[i] == new_keys[j] {
            if old[i] != new[j] {
                changed.push(j);
            }
            i += 1;
            j += 1;
        } else if in_new.get(&old_keys[i]).is_none_or(|&at| at < j) {
            splice(j, 1, 0);
            i += 1;
        } else if in_old.get(&new_keys[j]).is_none_or(|&at| at < i) {
            splice(j, 0, 1);
            j += 1;
        } else {
            splice(j, 1, 0);
            i += 1;
        }
    }
    splice(j, old.len() - i, new.len() - j);
    splices.retain(|&(_, removed, inserted)| removed + inserted > 0);
    (splices, changed)
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
    /* Painted within its own bounds: a stack of combining marks in its text, as tall as it
    may be, can't paint over the rows around it. Focus rings are drawn inside what they ring. */
    div()
        .w_full()
        .overflow_hidden()
        .child(column(theme).pt(space).child(inner))
        .into_any_element()
}

impl Render for ReaderView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.sb_theme().clone();
        if self.wheeled.take() {
            self.list.set_follow_mode(gpui::FollowMode::Normal);
        }
        self.frame += 1;
        self.top = self.list.logical_scroll_top().item_ix;
        self.let_go_of_far(cx);
        self.drawn.set(None);
        self.ensure_find(window, cx);
        if let Some((query, flash)) = self.filling.take() {
            self.fill(&query, flash, window, cx);
        }
        /* Below 1280 the margin gives way to the header's Facts toggle (DESIGN §2). */
        self.wide = window.viewport_size().width >= theme.sizes.facts_collapse_width;
        if self.wide {
            self.facts_open = false;
        }
        let header = self.render_header(window, &theme, cx);
        let open = matches!(cx.services().reader.read(cx).state(), ReaderState::Open(_));
        let facts = (open && (self.wide || self.facts_open)).then(|| self.render_facts(cx));
        let (margin, over) = if self.wide {
            (facts, None)
        } else {
            (None, facts)
        };
        let history = self
            .history
            .clone()
            .filter(|_| open)
            .map(|of| self.render_history(&of, cx));
        let full_image = self.render_full_image(&theme, cx);
        let reader = cx.services().reader.read(cx);
        let body = match reader.state() {
            ReaderState::Closed => div().into_any_element(),
            ReaderState::Opening(_) => Self::render_skeleton(&theme),
            ReaderState::Failed { why, .. } => Self::render_failed(why, &theme),
            ReaderState::Open(open) => {
                let banners = self.render_banners(open, &theme, cx);
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
                            /* A body's own Copy, with text selected in it, is the reader's
                            instead, through the system's seam: caught on the way down to the
                            body, in the column only, so that any text input elsewhere keeps
                            its own. */
                            .capture_action(cx.listener(
                                |this, _: &gpui_base::input::Copy, _, cx| {
                                    this.copy(cx);
                                    cx.stop_propagation();
                                },
                            ))
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
        let wide = self.wide;
        let below_header = theme.sizes.reader_header_height;
        div()
            .id("reader")
            .relative()
            .size_full()
            // A click in the column leaves the keyboard on the reader, for its keys.
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| this.focus_handle.focus(window, cx)),
            )
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(div().flex_1().min_w_0().h_full().child(body))
                    .children(margin),
            )
            /* Over the column: what a click outside closes, under the overlay it closes. */
            .when(
                self.history.is_some() || (!wide && self.facts_open),
                |this| {
                    this.child(
                        div()
                            .absolute()
                            .top(below_header)
                            .left_0()
                            .right_0()
                            .bottom_0()
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    if this.history.take().is_none() {
                                        this.facts_open = false;
                                    }
                                    cx.notify();
                                }),
                            ),
                    )
                },
            )
            .children(over.map(|facts| {
                div()
                    .id("facts-overlay")
                    .occlude()
                    .absolute()
                    .top(below_header)
                    .right_0()
                    .bottom_0()
                    .shadow(theme.shadows.pop.clone())
                    .child(facts)
            }))
            .children(history.map(|history| {
                div()
                    .absolute()
                    .top(below_header + px(120.))
                    .right(px(24.))
                    .child(history)
            }))
            .children(full_image)
            .automation_target("session")
    }
}

/**
 * `view`'s action on `element`: `run` with the view. While Find's field has the keyboard, the
 * reader's keys are the field's: the action goes on, and the key is typed.
 */
fn on<A: gpui::Action>(
    element: gpui::Stateful<gpui::Div>,
    view: &Entity<ReaderView>,
    run: impl Fn(&mut ReaderView, &mut Window, &mut Context<ReaderView>) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let view = view.clone();
    element.on_action(move |_: &A, window, cx| {
        if view.read(cx).finding(window, cx) {
            cx.propagate();
            return;
        }
        view.update(cx, |this, cx| {
            this.keyboard = true;
            /* Every key but End lets go of the end (below). */
            this.list.set_follow_mode(gpui::FollowMode::Normal);
            run(this, window, cx);
        });
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
        /* What follows the last row the keyboard takes (a body, how the session ended) too;
        and the column stays at its end while the rows there are measured and those still
        loading land, until a key or the wheel moves it. */
        this.list.set_follow_mode(gpui::FollowMode::Tail);
    });
    /* Through `on`, as every key but End: each lets go of the column's end. */
    let element = on::<act::SwitchCalls>(element, view, |_, _, cx| {
        let reader = cx.services().reader.clone();
        reader.update(cx, |reader, cx| {
            let full = !reader.full_calls();
            reader.set_full_calls(full, cx);
        });
    });
    let element = on::<act::Star>(element, view, |_, _, cx| {
        let library = cx.services().library.clone();
        library.update(cx, LibraryService::toggle_open_star);
    });
    let element = on::<act::NextSession>(element, view, |_, _, cx| {
        let library = cx.services().library.clone();
        library.update(cx, |library, cx| library.open_step(1, cx));
    });
    let element = on::<act::PreviousSession>(element, view, |_, _, cx| {
        let library = cx.services().library.clone();
        library.update(cx, |library, cx| library.open_step(-1, cx));
    });
    let view = view.clone();
    /* Ctrl F works in the field too: its text is selected again. */
    element.on_action(move |_: &act::Find, window, cx| {
        view.update(cx, |this, cx| this.focus_find(window, cx));
    })
}
