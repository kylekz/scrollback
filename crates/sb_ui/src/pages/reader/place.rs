//! Opening the session at a search match (`crate::services::read_at`, whose module docs are the
//! contract): the reader takes the palette's request once the session is parsed, opens the
//! rows the match is under, scrolls to its block and flashes it there.
//!
//! **Where it is.** A match in the session's own conversation is its block. One in a
//! subagent's text is under the call at `at` (or the subagent no call started at that place):
//! that call is opened, then each subagent on the way down to the one the match is in, each
//! with all its calls shown and the call that starts the next opened, the subagents read as
//! they are needed. A subagent nested deeper than panels are drawn is read on its own. Each
//! step waits for what it opened to be read, and goes on as the rows are laid out again.
//!
//! **What flashes** is the request's `flash`, in `hl.cur` for [`FLASH`] once drawn, where the
//! block at the place holds it as search matches it; when it doesn't (the session changed
//! since it was indexed), at the first block of the column that does, as Find's first match of
//! it would be. It flashes in a message's prose and in an activity row's command or path.
//!
//! **Find in session** isn't built yet (PLAN §13, phase 4b): the request's `find` is kept with
//! the request taken ([`ReaderView::read_at`]), for Find to be filled with when it comes.

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use gpui::{App, Context, Entity, ListOffset, Task, px};
use gpui_base::text::{RangeHighlight, TextViewState};
use sb_core::search::first_match;
use sb_core::transcript::{At, BlockId, searchable};

use super::rows::{MAX_DEPTH, Row};
use super::{Piece, ReaderView};
use crate::services::reader::{BlockRef, Load, OpenSession, ReaderService, Toggle};
use crate::services::{ActiveServices as _, ReadAt};
use crate::theme::ActiveTheme as _;

/// How long a match flashes once it is drawn.
const FLASH: Duration = Duration::from_secs(1);
/// How long a flash waits to be drawn before it is given up: its block's prose is prepared
/// meanwhile.
const FLASH_WAIT: Duration = Duration::from_secs(5);
/// How often a flash looks whether it has been drawn, and redraws until it has.
const FLASH_TICK: Duration = Duration::from_millis(50);

/// The text flashing in a block, while it does.
pub(super) struct Flash {
    pub(super) at: BlockRef,
    pub(super) text: String,
    /// The piece of prose it is lit in, once it is.
    lit: Option<Piece>,
    /// Whether it has been drawn: it shows for [`FLASH`] from then.
    drawn: bool,
    _ticks: Task<()>,
}

/// What opening a session at a match does next.
enum Step {
    /// Open these rows, and read these subagents; then look again.
    Open(Vec<Toggle>, Vec<Arc<str>>),
    /// Read this subagent on its own, from the call that started it.
    Alone(Arc<str>, Option<BlockRef>),
    /// Work out which subagent started which: those no call started are laid out once it is.
    Placement,
    /// Go to this block.
    Arrive(BlockRef),
    /// Wait for what was opened to be read.
    Wait,
    /// Nowhere in particular: the session stays at its start.
    Start,
}

/// The subagents from the outermost down to the one a match is in, each with the call of
/// its own that starts the next.
type Chain = Vec<(Arc<str>, Option<BlockId>)>;

impl ReaderView {
    /// The last request to open the session at a match the reader took, while that session
    /// is open.
    pub fn read_at(&self) -> Option<&ReadAt> {
        self.read_at.as_ref()
    }

    /// What flashes, while something does: its block and its text.
    pub fn flash(&self) -> Option<(&BlockRef, &str, bool)> {
        (self.flash.as_ref()).map(|flash| (&flash.at, flash.text.as_str(), flash.drawn))
    }

    /**
     * Takes the library's request to open the session at a match, once the session it is for
     * is parsed and laid out here, and starts going there. A subagent read on its own gives
     * way to the session first.
     */
    pub(super) fn take_read_at(&mut self, cx: &mut Context<Self>) {
        let services = cx.services();
        let (library, reader) = (services.library.clone(), services.reader.clone());
        let open = reader.read(cx).open_session().map(|open| open.id.clone());
        let asked = library.read(cx).read_at().map(|at| at.session.clone());
        if open.is_none() || open != self.session || asked != open {
            return;
        }
        let Some(read_at) = library.update(cx, |library, _| library.take_read_at()) else {
            return;
        };
        if self.alone.is_some() {
            self.read_alone(None, cx);
        }
        self.end_flash(cx);
        self.placing = Some(read_at.clone());
        self.read_at = Some(read_at);
        self.place(cx);
    }

    /// Takes the next step towards the match being opened at, if one is.
    pub(super) fn place(&mut self, cx: &mut Context<Self>) {
        let Some(read_at) = self.placing.clone() else {
            return;
        };
        let reader = cx.services().reader.clone();
        let step = {
            let reader = reader.read(cx);
            let Some(open) = reader.open_session() else {
                return;
            };
            let alone = self.alone.as_ref().map(|(item, _)| item);
            step(open, &read_at, reader.full_calls(), alone)
        };
        match step {
            Step::Open(toggles, read) => reader.update(cx, |reader, cx| {
                for toggle in toggles {
                    reader.toggle(toggle, cx);
                }
                for item in &read {
                    reader.want_subagent(item, cx);
                }
            }),
            Step::Alone(item, call) => {
                self.read_alone(Some((item, call)), cx);
                self.place(cx);
            }
            Step::Placement => reader.update(cx, ReaderService::want_placement),
            Step::Arrive(at) => {
                self.placing = None;
                self.arrive(&at, read_at.flash.as_deref(), cx);
            }
            Step::Wait => {}
            Step::Start => self.placing = None,
        }
    }

    /**
     * Scrolls to the block `at` and puts the keyboard on it, and flashes `flash` there; or,
     * when the block doesn't hold it or isn't laid out, at the first block of the column that
     * holds it (module docs).
     */
    fn arrive(&mut self, at: &BlockRef, flash: Option<&str>, cx: &mut Context<Self>) {
        let reader = cx.services().reader.clone();
        let laid = |at: &BlockRef| self.rows.iter().any(|row| row.block() == Some(at));
        let at = {
            let reader = reader.read(cx);
            let Some(open) = reader.open_session() else {
                return;
            };
            match flash {
                Some(flash) if !(laid(at) && holds(open, at, flash)) => self
                    .rows
                    .iter()
                    .filter_map(Row::block)
                    .find(|block| holds(open, block, flash))
                    .unwrap_or(at)
                    .clone(),
                _ => at.clone(),
            }
        };
        let Some(row) = self.rows.iter().position(|row| row.block() == Some(&at)) else {
            return;
        };
        self.list.set_follow_mode(gpui::FollowMode::Normal);
        self.list.scroll_to(ListOffset {
            item_ix: row,
            offset_in_item: px(0.),
        });
        if let Some(focus) = (self.rows.iter().enumerate().skip(row))
            .find(|(_, it)| it.block() == Some(&at) && it.is_focusable())
            .map(|(focus, _)| focus)
        {
            self.focused = Some((focus, self.rows[focus].clone()));
        }
        if let Some(text) = flash {
            self.start_flash(at, text.to_string(), cx);
        }
        cx.notify();
    }

    fn start_flash(&mut self, at: BlockRef, text: String, cx: &mut Context<Self>) {
        let ticks = cx.spawn(async move |this, cx| {
            let mut waited = Duration::ZERO;
            let mut shown = Duration::ZERO;
            loop {
                cx.background_executor().timer(FLASH_TICK).await;
                let going = this.update(cx, |this, cx| {
                    let Some(flash) = &this.flash else {
                        return false;
                    };
                    if flash.drawn {
                        shown += FLASH_TICK;
                    } else {
                        waited += FLASH_TICK;
                    }
                    if shown >= FLASH || waited >= FLASH_WAIT {
                        this.end_flash(cx);
                        return false;
                    }
                    /* Drawn again, so that prose prepared meanwhile is lit. */
                    cx.notify();
                    true
                });
                if !matches!(going, Ok(true)) {
                    break;
                }
            }
        });
        self.flash = Some(Flash {
            at,
            text,
            lit: None,
            drawn: false,
            _ticks: ticks,
        });
    }

    /// Stops the flash, and takes its highlight off the prose it lit.
    pub(super) fn end_flash(&mut self, cx: &mut Context<Self>) {
        let Some(flash) = self.flash.take() else {
            return;
        };
        if let Some((state, _)) = flash.lit.and_then(|piece| self.states.get(&piece)) {
            state.update(cx, |state, cx| state.clear_range_highlights(cx));
        }
        cx.notify();
    }

    /// Lights the flash in the piece of prose `piece`, drawn with `state`, if it is in that
    /// piece and isn't lit elsewhere.
    pub(super) fn light_prose(
        &mut self,
        piece: &Piece,
        state: &Entity<TextViewState>,
        cx: &mut App,
    ) {
        let Some(flash) = self.flash.as_mut().filter(|flash| flash.at == piece.at) else {
            return;
        };
        match &flash.lit {
            Some(lit) => flash.drawn |= lit == piece,
            None => {
                let rendered = state.read(cx).rendered_text();
                let Some(range) = first_match(rendered.as_str(), &flash.text) else {
                    return;
                };
                let accent = cx.sb_theme().colors.accent;
                let lit = state.update(cx, |state, cx| {
                    state.set_range_highlights([RangeHighlight::new(range, accent)], cx)
                });
                if lit.is_ok() {
                    flash.lit = Some(piece.clone());
                    flash.drawn = true;
                }
            }
        }
    }

    /// Where the flash is in `text`, drawn for the block `at` outside prose (an activity
    /// row's command or path), if it is there.
    pub(super) fn flash_in(&mut self, at: &BlockRef, text: &str) -> Option<Range<usize>> {
        let flash = self.flash.as_mut().filter(|flash| flash.at == *at)?;
        let range = first_match(text, &flash.text)?;
        flash.drawn = true;
        Some(range)
    }
}

/// Whether the block `at` holds `text` where search looks, as search matches it.
fn holds(open: &OpenSession, at: &BlockRef, text: &str) -> bool {
    open.block(at).is_some_and(|block| {
        searchable(&block.content)
            .iter()
            .any(|(_, said)| first_match(said, text).is_some())
    })
}

/**
 * What opening `open` at `read_at` does next, with Full calls on or off and the subagent read
 * on its own, if one is (the module docs).
 */
fn step(open: &OpenSession, read_at: &ReadAt, full_calls: bool, alone: Option<&Arc<str>>) -> Step {
    let Some(at) = read_at.at else {
        return Step::Start;
    };
    let (outer, roots, call) = match at {
        At::Block { block, .. } => {
            let Some(main) = open.main() else {
                return Step::Start;
            };
            let at = BlockRef {
                conversation: main.item.clone(),
                block,
            };
            if read_at.subagent.is_none() {
                return Step::Arrive(at);
            }
            let roots = open.call(&at).map_or_else(Vec::new, |call| {
                (call.subagents.iter())
                    .map(|subagent| subagent.item().clone())
                    .collect()
            });
            (Toggle::Call(at.clone()), roots, Some(at))
        }
        /* The place is among those no call is known to start once every subagent is read,
        as the index read them: known here once the placement is. */
        At::Unattached(place) => match open.unattached() {
            None => return Step::Placement,
            Some(unattached) => match unattached.get(place) {
                Some(item) => (Toggle::Subagent(item.clone()), vec![item.clone()], None),
                None => return Step::Start,
            },
        },
    };
    let Some(inside) = &read_at.subagent else {
        return Step::Start;
    };
    let target = BlockRef {
        conversation: Arc::from(inside.item.as_str()),
        block: inside.block,
    };
    if alone == Some(&target.conversation) {
        return Step::Arrive(target);
    }
    let mut unread = Vec::new();
    let chain = roots
        .iter()
        .find_map(|root| walk(open, root, &inside.item, &mut HashSet::new(), &mut unread));
    let Some(chain) = chain else {
        let outer_closed = !open.is_open(&outer, full_calls);
        return match (unread.is_empty(), call) {
            /* Read what may hold it, under the call opened. */
            (false, _) => Step::Open(outer_closed.then_some(outer).into_iter().collect(), unread),
            /* It isn't under the call any more: the call, and the first match from there. */
            (true, Some(call)) => Step::Arrive(call),
            (true, None) => Step::Start,
        };
    };
    if chain.len() > MAX_DEPTH {
        let starter = chain.len().checked_sub(2).and_then(|parent| {
            let (item, call) = &chain[parent];
            call.map(|block| BlockRef {
                conversation: item.clone(),
                block,
            })
        });
        return Step::Alone(target.conversation, starter.or(call));
    }
    let mut toggles = vec![outer];
    for (item, starts) in &chain {
        toggles.push(Toggle::AllCalls(item.clone()));
        if let Some(block) = starts {
            toggles.push(Toggle::Call(BlockRef {
                conversation: item.clone(),
                block: *block,
            }));
        }
    }
    toggles.retain(|toggle| !open.is_open(toggle, full_calls));
    let read: Vec<Arc<str>> = (chain.iter())
        .map(|(item, _)| item.clone())
        .filter(|item| open.subagent(item).is_none())
        .collect();
    match (
        toggles.is_empty() && read.is_empty(),
        open.subagent(&target.conversation),
    ) {
        (false, _) => Step::Open(toggles, read),
        (true, Some(Load::Loaded(_))) => Step::Arrive(target),
        (true, Some(Load::Loading)) => Step::Wait,
        /* It couldn't be read: the call, and the first match from there. */
        (true, _) => call.map_or(Step::Start, Step::Arrive),
    }
}

/**
 * The way from the subagent `item` down to the subagent `target`, through the calls that
 * start each next one; `None` when it isn't under `item`, or not as far as what is read shows,
 * the subagents not read yet on the way added to `unread`.
 */
fn walk(
    open: &OpenSession,
    item: &Arc<str>,
    target: &str,
    seen: &mut HashSet<Arc<str>>,
    unread: &mut Vec<Arc<str>>,
) -> Option<Chain> {
    if !seen.insert(item.clone()) {
        return None;
    }
    if **item == *target {
        return Some(vec![(item.clone(), None)]);
    }
    match open.subagent(item) {
        Some(Load::Loaded(read)) => {
            for (block, call) in read.conversation.calls() {
                for subagent in &call.subagents {
                    if let Some(mut chain) = walk(open, subagent.item(), target, seen, unread) {
                        chain.insert(0, (item.clone(), Some(block)));
                        return Some(chain);
                    }
                }
            }
        }
        Some(Load::Failed(_)) => {}
        Some(Load::Loading) | None => unread.push(item.clone()),
    }
    None
}
