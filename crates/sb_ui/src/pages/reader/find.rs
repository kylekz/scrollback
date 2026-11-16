//! Find in session (DESIGN §4.2.9), the view's half: the header's field, what the search the
//! reader service ran found ([`crate::services::reader::find`]), the current match, and going
//! to it.
//!
//! **Searching** starts as the query changes, off the UI thread; the search before is told to
//! stop, and what it found, if it lands, is dropped. Until the first lands nothing is lit.
//!
//! **Lighting.** A body's lines draw their matches with their marks
//! ([`super::kinds::styled`]), from where the search found them: a body's text is shaped as
//! the search read it. Prose is lit in what Markdown made of it, which the search can't see:
//! each piece drawn is searched again when it is drawn, at most [`PIECE_MATCHES`], and the
//! current match is the one at its place among its block's.
//!
//! **Going to a match** opens what holds it: its call, its thought, its summary, Show all on a
//! body whose match is past its first lines, each subagent on the way down to one inside a
//! subagent (as opening at a search match does, [`super::place`]); then, as the rows it opened
//! are laid out, scrolls to the row that draws it.

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{
    App, AppContext as _, Context, Entity, Focusable as _, ListOffset, Subscription, Task, Window,
    px,
};
use gpui_base::text::{RangeHighlight, TextViewState};
use gpui_component::input::{InputEvent, InputState};
use sb_core::search::find_all;
use sb_core::transcript::{At, InSubagent};

use super::rows::{BodyPart, CHUNK_LINES, FIRST_LINES, FIRST_RESULTS, RowKind};
use super::{Piece, ReaderView};
use crate::services::reader::find::{Findings, Found, SUBAGENTS, Spot};
use crate::services::reader::shape::diff::Diff;
use crate::services::reader::shape::{Renderer, renderer};
use crate::services::reader::{
    BlockRef, CallBody, Load, OpenSession, Part, Piece as ProsePiece, ReaderService, Toggle,
};
use crate::services::{ActiveServices as _, ReadAt};
use crate::theme::ActiveTheme as _;

/// Where the matches are in each piece of a web fetch's result.
type PieceFinds = Arc<Vec<Vec<Range<usize>>>>;

/// The most matches lit in one piece of prose.
pub const PIECE_MATCHES: usize = 1_000;

pub(super) struct Find {
    pub(super) input: Entity<InputState>,
    _heard: Subscription,
    /// What is being searched for; empty while Find is closed.
    pub(super) query: String,
    /// The search running, and its flag, cleared when the next one supersedes it.
    going: Option<(Arc<AtomicBool>, Task<()>)>,
    /// What the last search that landed found, with its matches by where they are and by turn.
    pub(super) findings: Option<Arc<Findings>>,
    by_spot: HashMap<Spot, Range<usize>>,
    /// How many matches each turn of the session's own conversation has.
    pub(super) turns: Arc<BTreeMap<usize, usize>>,
    pub(super) current: Option<usize>,
    /// A search was asked for while no session was open: it runs once one is.
    waiting: bool,
    /// Where in each piece of a web fetch's result its matches are, worked out once a count
    /// ([`ReaderView::fetched_finds`]).
    fetched: HashMap<BlockRef, (u64, PieceFinds)>,
    /// Going to the current match, until its row is laid out.
    revealing: bool,
    /// When the search lands, the match to make current: where a search match opened the
    /// session (its block and its text), rather than going to the first.
    at_flash: Option<(BlockRef, String)>,
    /// Counts what changes what prose lights: each piece is lit once a count.
    generation: u64,
    /// The pieces lit, with the count they were lit at and how many matches each has.
    pub(super) lit: HashMap<Piece, (u64, usize)>,
}

impl Find {
    pub(super) fn new(window: &mut Window, cx: &mut Context<ReaderView>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find"));
        let heard = cx.subscribe(&input, |this, input, event, cx| match event {
            /* A change that leaves the text as it was (Enter) is no new search. */
            InputEvent::Change => {
                let query = input.read(cx).value().to_string();
                if this.find.as_ref().is_none_or(|find| find.query != query) {
                    this.search(query, None, cx);
                }
            }
            InputEvent::PressEnter { shift, .. } => {
                this.step_match(if *shift { -1 } else { 1 }, cx)
            }
            InputEvent::Blur | InputEvent::Focus => cx.notify(),
        });
        Self {
            input,
            _heard: heard,
            query: String::new(),
            going: None,
            findings: None,
            by_spot: HashMap::new(),
            turns: Arc::default(),
            current: None,
            waiting: false,
            fetched: HashMap::new(),
            revealing: false,
            at_flash: None,
            generation: 0,
            lit: HashMap::new(),
        }
    }

    /// Whether Find has a query: its matches are lit.
    pub(super) fn is_active(&self) -> bool {
        !self.query.is_empty()
    }

    /// "3 of 14", "No matches", or nothing while the first search runs.
    pub(super) fn count(&self) -> Option<String> {
        let findings = self.findings.as_ref()?;
        if findings.matches.is_empty() {
            return Some("No matches".to_string());
        }
        let more = if findings.capped { "+" } else { "" };
        let left = if findings.subagents_left {
            format!(
                " · first {} subagents",
                crate::format::count(SUBAGENTS as u64)
            )
        } else {
            String::new()
        };
        Some(format!(
            "{} of {}{more}{left}",
            crate::format::count(self.current.map_or(0, |at| at + 1) as u64),
            crate::format::count(findings.matches.len() as u64)
        ))
    }

    fn current(&self) -> Option<&Found> {
        self.findings.as_ref()?.matches.get(self.current?)
    }

    /// Stops the search running, if one is: Find is let go of.
    pub(super) fn stop(self) {
        if let Some((going, _)) = self.going {
            going.store(false, Ordering::Relaxed);
        }
    }
}

/// Which of a block's texts a match is in, for automation.
pub fn spot_kind(spot: &Spot) -> String {
    match spot {
        Spot::Prose(_) => "prose".to_string(),
        Spot::Target(_) => "target".to_string(),
        Spot::Part(_, None) => "input".to_string(),
        Spot::Part(_, Some(part)) => format!("output/{part}"),
        Spot::Diff(_) => "diff".to_string(),
        Spot::Fetched(_) => "fetched".to_string(),
        Spot::Result(_, index) => format!("result/{index}"),
    }
}

impl ReaderView {
    /// The matches at `spot` to draw, each with whether it is the current one.
    pub(super) fn finds(&self, spot: &Spot) -> Vec<(Range<usize>, bool)> {
        let Some(find) = self.find.as_ref().filter(|find| find.is_active()) else {
            return Vec::new();
        };
        let (Some(findings), Some(range)) = (&find.findings, find.by_spot.get(spot)) else {
            return Vec::new();
        };
        findings.matches[range.clone()]
            .iter()
            .enumerate()
            .map(|(index, found)| {
                (
                    found.range.clone(),
                    find.current == Some(range.start + index),
                )
            })
            .collect()
    }

    /// Searches for `query`, superseding the search before; with `flash`, the match there is
    /// made current when it lands, else the first from the turn at the column's top.
    pub(super) fn search(
        &mut self,
        query: String,
        flash: Option<(BlockRef, String)>,
        cx: &mut Context<Self>,
    ) {
        let root = cx
            .services()
            .library
            .read(cx)
            .open_session()
            .and_then(|session| session.project_path.clone());
        let reader = cx.services().reader.clone();
        let Some(find) = self.find.as_mut() else {
            return;
        };
        if let Some((going, _)) = find.going.take() {
            going.store(false, Ordering::Relaxed);
        }
        find.query = query.clone();
        find.at_flash = flash;
        find.revealing = false;
        if query.trim().is_empty() {
            find.query.clear();
            self.clear_found(cx);
            return;
        }
        let going = Arc::new(AtomicBool::new(true));
        let Some(task) =
            reader.update(cx, |reader, cx| reader.find(query, root, going.clone(), cx))
        else {
            /* Nothing open to search: it runs once the session is ([`Self::reopened`]). */
            find.waiting = true;
            self.clear_found(cx);
            return;
        };
        find.waiting = false;
        let wait = cx.spawn(async move |this, cx| {
            let findings = task.await;
            this.update(cx, |this, cx| {
                if let Some(findings) = findings {
                    this.found(findings, cx);
                }
            })
            .ok();
        });
        if let Some(find) = self.find.as_mut() {
            find.going = Some((going, wait));
        }
    }

    /// Takes in what a search found.
    fn found(&mut self, findings: Findings, cx: &mut Context<Self>) {
        let top_turn = self.top_turn(cx);
        let Some(find) = self.find.as_mut() else {
            return;
        };
        find.going = None;
        find.by_spot.clear();
        find.fetched.clear();
        let mut turns = BTreeMap::new();
        for (index, found) in findings.matches.iter().enumerate() {
            find.by_spot
                .entry(found.spot.clone())
                .and_modify(|range| range.end = index + 1)
                .or_insert(index..index + 1);
            if let Some(turn) = found.turn {
                *turns.entry(turn).or_default() += 1;
            }
        }
        find.turns = Arc::new(turns);
        let flashed = find.at_flash.take().and_then(|(at, _)| {
            findings
                .matches
                .iter()
                .position(|found| found.spot.block() == &at)
        });
        let reveal = flashed.is_none();
        find.current = flashed.or_else(|| {
            (!findings.matches.is_empty()).then(|| {
                findings
                    .matches
                    .iter()
                    .position(|found| found.turn.is_some_and(|turn| Some(turn) >= top_turn))
                    .unwrap_or(0)
            })
        });
        find.findings = Some(Arc::new(findings));
        find.generation += 1;
        if reveal {
            self.reveal(cx);
        }
        cx.notify();
    }

    /// Takes every highlight of Find off.
    fn clear_found(&mut self, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        find.findings = None;
        find.by_spot.clear();
        find.turns = Arc::default();
        find.fetched.clear();
        find.current = None;
        find.generation += 1;
        for piece in std::mem::take(&mut find.lit).into_keys() {
            if let Some((state, _)) = self.states.get(&piece) {
                state.update(cx, |state, cx| state.clear_range_highlights(cx));
            }
        }
        cx.notify();
    }

    /**
     * The open session was parsed again (another version read, the reading copy changed), or
     * opened while a search waited for it: what was found is of another text, so it goes, and
     * an active Find searches again.
     */
    pub(super) fn reopened(&mut self, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        if let Some((going, _)) = find.going.take() {
            going.store(false, Ordering::Relaxed);
        }
        let query = find.query.clone();
        self.clear_found(cx);
        if !query.trim().is_empty() {
            self.search(query, None, cx);
        }
    }

    /// Whether a search waits for a session to open.
    pub(super) fn find_waits(&self) -> bool {
        self.find.as_ref().is_some_and(|find| find.waiting)
    }

    /// Fills Find with `query` and searches, without going to a match (a search match the
    /// session was opened at, [`crate::services::read_at`]): the match at `flash` is current.
    pub(super) fn fill(
        &mut self,
        query: &str,
        flash: Option<(BlockRef, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ensure_find(window, cx);
        if let Some(find) = &self.find {
            let input = find.input.clone();
            input.update(cx, |input, cx| {
                input.set_value(query.to_string(), window, cx)
            });
        }
        self.search(query.to_string(), flash, cx);
    }

    /// Makes Find's field, once there is a window to make it in.
    pub(super) fn ensure_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.is_none() {
            self.find = Some(Find::new(window, cx));
        }
    }

    /// `Ctrl F`: the field takes the keyboard, its text selected.
    pub(super) fn focus_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_find(window, cx);
        if let Some(find) = &self.find {
            find.input.update(cx, |input, cx| {
                input.focus(window, cx);
                input.select_all(window, cx);
            });
        }
        cx.notify();
    }

    /// `Esc` or the close button: Find is emptied and its highlights go; the keyboard goes
    /// back to the column.
    pub(super) fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(find) = self.find.as_mut().filter(|find| {
            find.is_active() || find.input.read(cx).focus_handle(cx).is_focused(window)
        }) else {
            return false;
        };
        if let Some((going, _)) = find.going.take() {
            going.store(false, Ordering::Relaxed);
        }
        find.query.clear();
        let input = find.input.clone();
        input.update(cx, |input, cx| input.set_value("", window, cx));
        self.clear_found(cx);
        self.focus_handle.focus(window, cx);
        true
    }

    /// Whether Find's field has the keyboard: the reader's own keys are then the field's.
    pub(super) fn finding(&self, window: &Window, cx: &App) -> bool {
        self.find
            .as_ref()
            .is_some_and(|find| find.input.read(cx).focus_handle(cx).is_focused(window))
    }

    /// `Enter` / `Shift Enter`, the up / down buttons: the next or previous match, wrapping.
    pub(super) fn step_match(&mut self, by: isize, cx: &mut Context<Self>) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        let Some(count) = find
            .findings
            .as_ref()
            .map(|it| it.matches.len())
            .filter(|it| *it > 0)
        else {
            return;
        };
        find.current = Some(match find.current {
            Some(at) => (at as isize + by).rem_euclid(count as isize) as usize,
            None => 0,
        });
        find.generation += 1;
        self.reveal(cx);
        cx.notify();
    }

    /// The turn of the session's own conversation at the column's top.
    fn top_turn(&self, cx: &App) -> Option<usize> {
        let reader = cx.services().reader.read(cx);
        let open = reader.open_session()?;
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

    /**
     * Goes to the current match (the module docs): opens what holds it, then, once its row is
     * laid out, scrolls there. Asked again as rows are laid out, while it is under way.
     */
    pub(super) fn reveal(&mut self, cx: &mut Context<Self>) {
        let Some(found) = self.find.as_ref().and_then(Find::current).cloned() else {
            return;
        };
        if let Some(find) = self.find.as_mut() {
            find.revealing = true;
        }
        let reader = cx.services().reader.clone();
        let (step, chain) = {
            let reader = reader.read(cx);
            let Some(open) = reader.open_session() else {
                return;
            };
            (
                opening(open, &found, reader.full_calls()),
                inside(open, found.spot.block()),
            )
        };
        /* Inside a subagent whose way down isn't known yet: its placement is worked out, and
        this goes on once it lands. */
        if chain.is_none()
            && self
                .rows
                .iter()
                .all(|row| row.block() != Some(found.spot.block()))
            && cx
                .services()
                .reader
                .read(cx)
                .open_session()
                .is_some_and(|open| {
                    open.main()
                        .is_none_or(|main| main.item != found.spot.block().conversation)
                        && open.placement().is_none()
                })
        {
            reader.update(cx, ReaderService::want_placement);
            return;
        }
        /* Inside a subagent: the way down to it is opened as opening at a match opens it. */
        if let Some(read_at) = chain
            && self
                .alone
                .as_ref()
                .is_none_or(|(item, _)| *item != found.spot.block().conversation)
            && !self
                .rows
                .iter()
                .any(|row| row.block() == Some(found.spot.block()))
        {
            if self.placing.is_none() {
                self.placing = Some(read_at);
                self.place(cx);
            }
            return;
        }
        if let Some((toggles, load)) = step {
            reader.update(cx, |reader, cx| {
                for toggle in toggles {
                    reader.toggle(toggle, cx);
                }
                if let Some(at) = load {
                    reader.want_body(&at, cx);
                }
            });
            return;
        }
        let Some((row, line)) = self.row_of(&found, cx) else {
            return;
        };
        if let Some(find) = self.find.as_mut() {
            find.revealing = false;
        }
        self.list.set_follow_mode(gpui::FollowMode::Normal);
        /* The match's line a little way down the column: about where it is in its row. */
        let line_height = cx.sb_theme().text.code.line_height;
        let into = (line_height * line as f32 - px(120.)).max(px(0.));
        self.list.scroll_to(ListOffset {
            item_ix: row,
            offset_in_item: into,
        });
        cx.notify();
    }

    /// Goes on to the current match, if it is being gone to: rows were laid out again.
    pub(super) fn reveal_pending(&mut self, cx: &mut Context<Self>) {
        if self.find.as_ref().is_some_and(|find| find.revealing) && self.placing.is_none() {
            self.reveal(cx);
        }
    }

    /// The row that draws `found` and the line of it the match is on, once it is laid out.
    fn row_of(&self, found: &Found, cx: &App) -> Option<(usize, usize)> {
        let reader = cx.services().reader.read(cx);
        let open = reader.open_session()?;
        let at = found.spot.block();
        let body = match open.body(at) {
            Some(Load::Loaded(body)) => Some(body.clone()),
            _ => None,
        };
        let line_of = |part: Option<usize>| -> Option<usize> {
            let body = body.as_ref()?;
            let text = match part {
                None => body.input.as_ref().ok()?,
                Some(index) => match body.output.get(index)? {
                    Part::Text(Ok(text)) => text,
                    _ => return None,
                },
            };
            Some(text.line_at(found.range.start))
        };
        /* A web fetch's match: the piece it is in, and its line there when it is text. */
        let fetched = match &found.spot {
            Spot::Fetched(_) => {
                let body = body.as_ref()?;
                let ordinal = self.ordinal(found)?;
                let (piece, index) = locate(&self.fetched_finds(at, body)?, ordinal)?;
                let line = match body.fetched.as_ref()?.pieces.get(piece)? {
                    ProsePiece::Code { text, .. } | ProsePiece::Plain { text, .. } => {
                        let ranges = &self.fetched_finds(at, body)?[piece];
                        text.line_at(ranges.get(index)?.start)
                    }
                    ProsePiece::Markdown(_) => 0,
                };
                Some((piece, line))
            }
            _ => None,
        };
        let wanted = |row: &RowKind| -> Option<usize> {
            match (&found.spot, row) {
                (Spot::Prose(it), RowKind::Prose { at, .. })
                | (Spot::Prose(it), RowKind::Thought { at, .. })
                | (Spot::Prose(it), RowKind::Compaction { at, .. })
                | (Spot::Prose(it), RowKind::Prompt(at))
                | (Spot::Target(it), RowKind::Call { at, .. })
                    if it == at =>
                {
                    Some(0)
                }
                (
                    Spot::Result(it, index),
                    RowKind::Body {
                        at,
                        part: BodyPart::Result(row),
                        ..
                    },
                ) if it == at && index == row => Some(0),
                (
                    Spot::Fetched(it),
                    RowKind::Body {
                        at,
                        part: BodyPart::Fetched { piece, chunk },
                        ..
                    },
                ) if it == at => {
                    let (want, line) = fetched?;
                    (want == *piece && line / CHUNK_LINES == *chunk).then_some(line % CHUNK_LINES)
                }
                (
                    Spot::Part(it, part),
                    RowKind::Body {
                        at,
                        part: BodyPart::Lines { part: lines, chunk },
                        ..
                    },
                ) if it == at && part == lines => {
                    let line = line_of(*part)?;
                    (line / CHUNK_LINES == *chunk || *chunk == 0 && line < FIRST_LINES)
                        .then_some(line % CHUNK_LINES)
                }
                (
                    Spot::Part(it, _),
                    RowKind::Body {
                        at,
                        part: BodyPart::Pair { chunk },
                        ..
                    },
                ) if it == at => {
                    let line = line_of(match found.spot {
                        Spot::Part(_, part) => part,
                        _ => None,
                    })?;
                    (line / CHUNK_LINES == *chunk || *chunk == 0 && line < FIRST_LINES)
                        .then_some(line % CHUNK_LINES)
                }
                (
                    Spot::Diff(it),
                    RowKind::Body {
                        at,
                        part: BodyPart::Diff { chunk },
                        ..
                    },
                ) if it == at => {
                    let diff = match &body.as_ref()?.diff {
                        Some(Ok(diff)) => diff.clone(),
                        _ => return None,
                    };
                    let line = &diff.lines[diff_line(&diff, found)?];
                    let drawn = line.drawn.start
                        + (diff.text.line_at(found.range.start))
                            .saturating_sub(line.segments.start);
                    (drawn / CHUNK_LINES == *chunk).then_some(drawn % CHUNK_LINES + 1)
                }
                _ => None,
            }
        };
        self.rows
            .iter()
            .enumerate()
            .find_map(|(index, row)| wanted(&row.kind).map(|line| (index, line)))
    }

    /**
     * Lights Find's matches in the piece of prose `piece`, drawn with `state`, once for each
     * change of what is found or current (the module docs).
     */
    pub(super) fn light_found(
        &mut self,
        piece: &Piece,
        state: &Entity<TextViewState>,
        cx: &mut App,
    ) {
        let colors = cx.sb_theme().colors;
        let Some(find) = self.find.as_ref().filter(|find| find.is_active()) else {
            return;
        };
        if find
            .lit
            .get(piece)
            .is_some_and(|(generation, _)| *generation == find.generation)
        {
            return;
        }
        let fetched = Spot::Fetched(piece.at.clone());
        /* In a web fetch's result, the matches of the pieces before this one. */
        let earlier = find.by_spot.contains_key(&fetched).then(|| {
            let reader = cx.services().reader.read(cx);
            let body = match reader.open_session()?.body(&piece.at)? {
                Load::Loaded(body) => body.clone(),
                _ => return None,
            };
            Some(
                self.fetched_finds(&piece.at, &body)?[..piece.piece]
                    .iter()
                    .map(Vec::len)
                    .sum::<usize>(),
            )
        });
        let Some(find) = self.find.as_mut() else {
            return;
        };
        let spots = [Spot::Prose(piece.at.clone()), fetched];
        let current = find
            .current()
            .filter(|found| spots.contains(&found.spot))
            .map(|found| {
                let range = find.by_spot.get(&found.spot).cloned().unwrap_or(0..0);
                find.current.unwrap_or(0) - range.start
            });
        if find.findings.is_none() || !spots.iter().any(|spot| find.by_spot.contains_key(spot)) {
            if find.lit.remove(piece).is_some() {
                state.update(cx, |state, cx| state.clear_range_highlights(cx));
            }
            return;
        }
        /* The current match's place among its block's: this piece's start is how many the
        pieces before it have. */
        let before: usize = earlier.flatten().unwrap_or_else(|| {
            find.lit
                .iter()
                .filter(|(lit, _)| lit.at == piece.at && lit.piece < piece.piece)
                .map(|(_, (_, count))| count)
                .sum()
        });
        let rendered = state.read(cx).rendered_text();
        let ranges = find_all(rendered.as_str(), &find.query, PIECE_MATCHES);
        let highlights = ranges.iter().enumerate().map(|(index, range)| {
            let color = if current == Some(before + index) {
                colors.accent
            } else {
                colors.yellow_soft
            };
            RangeHighlight::new(range.clone(), color)
        });
        let set = state.update(cx, |state, cx| state.set_range_highlights(highlights, cx));
        if set.is_ok() {
            find.lit
                .insert(piece.clone(), (find.generation, ranges.len()));
        }
    }
}

/// The line of `diff` that holds the match `found`.
fn diff_line(diff: &Diff, found: &Found) -> Option<usize> {
    let line = diff
        .lines
        .partition_point(|line| line.range.end < found.range.start);
    (line < diff.lines.len()).then_some(line)
}

/// Which piece's matches the `ordinal`th of a block's is, and its place among them.
fn locate(pieces: &[Vec<Range<usize>>], mut ordinal: usize) -> Option<(usize, usize)> {
    for (piece, ranges) in pieces.iter().enumerate() {
        if ordinal < ranges.len() {
            return Some((piece, ordinal));
        }
        ordinal -= ranges.len();
    }
    None
}

impl ReaderView {
    /// The place of `found` among the matches of its spot.
    fn ordinal(&self, found: &Found) -> Option<usize> {
        let find = self.find.as_ref()?;
        let range = find.by_spot.get(&found.spot)?;
        let at = find.current?;
        range.contains(&at).then(|| at - range.start)
    }

    /**
     * Where Find's matches are in each piece of the web fetch's result at `at`, found again in
     * each piece's text (a Markdown piece's as written), once for what Find found.
     */
    fn fetched_finds(&self, at: &BlockRef, body: &CallBody) -> Option<PieceFinds> {
        let find = self.find.as_ref().filter(|find| find.is_active())?;
        if let Some((generation, found)) = find.fetched.get(at)
            && *generation == find.generation
        {
            return Some(found.clone());
        }
        let found = Arc::new(
            body.fetched
                .iter()
                .flat_map(|prose| prose.pieces.iter())
                .map(|piece| {
                    let text = match piece {
                        ProsePiece::Markdown(sanitized) => sanitized.as_str(),
                        ProsePiece::Code { text, .. } | ProsePiece::Plain { text, .. } => {
                            text.text.as_str()
                        }
                    };
                    find_all(text, &find.query, PIECE_MATCHES)
                })
                .collect::<Vec<_>>(),
        );
        Some(found)
    }

    /// Find's matches in the `piece`th piece of the web fetch's result at `at`, when it is
    /// shown as text, with whether each is the current one.
    pub(super) fn fetched_piece_finds(
        &mut self,
        at: &BlockRef,
        body: &CallBody,
        piece: usize,
    ) -> Vec<(Range<usize>, bool)> {
        let Some(found) = self.fetched_finds(at, body) else {
            return Vec::new();
        };
        if let Some(find) = self.find.as_mut() {
            find.fetched
                .insert(at.clone(), (find.generation, found.clone()));
        }
        let current = self
            .find
            .as_ref()
            .and_then(Find::current)
            .filter(|it| it.spot == Spot::Fetched(at.clone()))
            .cloned()
            .and_then(|it| self.ordinal(&it))
            .and_then(|ordinal| locate(&found, ordinal));
        found
            .get(piece)
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, range)| (range.clone(), current == Some((piece, index))))
            .collect()
    }
}

/**
 * What to open so that `found` is drawn, with Full calls on or off: the rows that hold it, and
 * a body to read first when what is open depends on it; `None` when nothing is left to open.
 */
#[allow(clippy::type_complexity)]
fn opening(
    open: &OpenSession,
    found: &Found,
    full_calls: bool,
) -> Option<(Vec<Toggle>, Option<BlockRef>)> {
    let at = found.spot.block();
    let mut toggles = Vec::new();
    let block = open.block(at)?;
    match (&found.spot, &block.content) {
        (Spot::Prose(_), sb_core::transcript::Content::Thinking { .. }) => {
            toggles.push(Toggle::Thought(at.clone()))
        }
        (Spot::Prose(_), sb_core::transcript::Content::Compaction { .. }) => {
            toggles.push(Toggle::Summary(at.clone()))
        }
        (Spot::Part(_, part), _) => {
            let call = open.call(at)?;
            toggles.push(Toggle::Call(at.clone()));
            if !call.subagents.is_empty() {
                toggles.push(Toggle::CallIo(at.clone()));
            }
            let body = match open.body(at) {
                Some(Load::Loaded(body)) => body.clone(),
                Some(Load::Loading) => return Some((Vec::new(), None)),
                _ => {
                    toggles.retain(|toggle| !open.is_open(toggle, full_calls));
                    return Some((toggles, Some(at.clone())));
                }
            };
            let text = match part {
                None => body.input.as_ref().ok(),
                Some(index) => match body.output.get(*index) {
                    Some(Part::Text(Ok(text))) => Some(text),
                    _ => None,
                },
            };
            let mcp = renderer(call) == Renderer::Mcp;
            if let Some(text) = text
                && (text.rest.is_some() || text.line_at(found.range.start) >= FIRST_LINES)
            {
                toggles.push(Toggle::AllLines(at.clone(), if mcp { None } else { *part }));
            }
        }
        (Spot::Diff(_), _) => {
            let call = open.call(at)?;
            toggles.push(Toggle::Call(at.clone()));
            if let Some(Load::Loaded(body)) = open.body(at)
                && let Some(Ok(diff)) = &body.diff
            {
                let written = call.kind == sb_core::transcript::ToolKind::Write;
                let shown = super::kinds::diff_chunk(diff, 0, false, written);
                let drawn = diff_line(diff, found).map_or(0, |line| diff.lines[line].drawn.start);
                if drawn >= shown.end {
                    toggles.push(Toggle::AllLines(at.clone(), None));
                }
            }
        }
        (Spot::Fetched(_), _) => toggles.push(Toggle::Call(at.clone())),
        (Spot::Result(_, index), _) => {
            toggles.push(Toggle::Call(at.clone()));
            if *index >= FIRST_RESULTS {
                toggles.push(Toggle::AllLines(at.clone(), None));
            }
        }
        _ => {}
    }
    toggles.retain(|toggle| !open.is_open(toggle, full_calls));
    (!toggles.is_empty()).then_some((toggles, None))
}

/**
 * The way down to the block `at` when it is in a subagent's conversation, as a request to open
 * the session there ([`super::place`]): the call of the session's own conversation that
 * started the outermost subagent, or the place among those no call started, and the subagent
 * and block it is in.
 */
fn inside(open: &OpenSession, at: &BlockRef) -> Option<ReadAt> {
    let main = open.main();
    if main.is_some_and(|main| main.item == at.conversation) {
        return None;
    }
    /* The subagent's outermost ancestor: the one a call of the session's own started, or one
    no call started. */
    let mut item = at.conversation.clone();
    let mut seen = std::collections::HashSet::new();
    let started_by = |item: &Arc<str>| -> Option<(Arc<str>, sb_core::transcript::BlockId)> {
        let conversations = main
            .into_iter()
            .map(|main| (main.item.clone(), main))
            .chain(
                open.subagents_read()
                    .map(|(key, read)| (key.clone(), &read.conversation)),
            );
        for (key, conversation) in conversations {
            if let Some((block, _)) = conversation
                .calls()
                .find(|(_, call)| call.subagents.iter().any(|it| it.item() == item))
            {
                return Some((key, block));
            }
        }
        /* A subagent Find read for itself is under one the reader hasn't: its placement
        says which (its call is found as the way down is opened). */
        match open.placement() {
            Some(Load::Loaded(placement)) => placement
                .parents
                .get(item)
                .map(|parent| (parent.clone(), sb_core::transcript::BlockId(0))),
            _ => None,
        }
    };
    let place = loop {
        if !seen.insert(item.clone()) {
            return None;
        }
        match started_by(&item) {
            Some((parent, block)) if main.is_some_and(|main| main.item == parent) => {
                let turn = main?
                    .turns()
                    .iter()
                    .position(|turn| turn.blocks.contains(&block))?;
                break At::Block { turn, block };
            }
            Some((parent, _)) => item = parent,
            None => {
                let unattached = open.unattached()?;
                break At::Unattached(unattached.iter().position(|it| *it == item)?);
            }
        }
    };
    let conversation = open.conversation(&at.conversation)?;
    let turn = conversation
        .turns()
        .iter()
        .position(|turn| turn.blocks.contains(&at.block))?;
    let id = open.id.clone();
    Some(ReadAt {
        request: 0,
        session: id,
        at: Some(place),
        subagent: Some(InSubagent {
            item: at.conversation.to_string(),
            turn,
            block: at.block,
        }),
        kind: sb_core::search::MatchKind::Reply,
        find: String::new(),
        flash: None,
    })
}
