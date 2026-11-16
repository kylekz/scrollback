//! The page of All sessions, Starred or a folder (PLAN §11 `folder_page`; DESIGN §4.4, §4.5):
//! the header, a folder's attached paths as chips, and the sessions as cards grouped by month.
//!
//! **The cards are a virtual list.** Its items are the month overlines and the rows of cards
//! ([`rows`]), each of a height known without drawing it, so a page of tens of thousands of
//! sessions costs what the visible rows do. The rows are laid out again only when the page's
//! sessions or the number of columns change.
//!
//! **The filter row** narrows and orders the page and switches it between cards and the list
//! (DESIGN §4.4, §4.5; [`crate::services::view`]); each page keeps how it was set while the app
//! runs. The list view ([`list`]) is the same sessions as virtual rows, with the same
//! selection, menu and drag.
//!
//! **Selection:** a click selects a session, `Ctrl` adds or takes one away, `Shift` selects the
//! run from the selected one, `Ctrl A` everything shown. Its menu (right-click, the hover bar's
//! `⋯`, `Shift F10`), `Delete`, `S`, `M` and a drag act on the selection when the session is
//! in it.
//!
//! **Keyboard** (DESIGN §4.4, §5): the arrows move the selection between cards in reading
//! order (in the list, `↑` and `↓`), `Enter` opens the selected session, `S` stars it,
//! `Delete` moves it to Trash, `M` opens Move to folder and `Shift F10` its menu. Single
//! letters are the filter field's while it has the keyboard.
//!
//! **A folder's paths** show as chips that drag onto another folder, or as the paths editor
//! ([`crate::pages::paths_editor`]), which an empty folder opens by default.

mod filter_row;
mod list;

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use gpui::Focusable as _;
use gpui::{
    AnyElement, App, AppContext as _, Bounds, ClickEvent, Context, Div, ElementId, Entity,
    FocusHandle, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent,
    ParentElement as _, Pixels, Point, Render, Role, ScrollStrategy, SharedString, Size,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, point,
    prelude::FluentBuilder as _, px, size,
};
use gpui_component::input::{InputEvent, InputState};
use gpui_component::scroll::Scrollbar;
use gpui_component::tooltip::Tooltip;
use gpui_component::{VirtualListScrollHandle, v_virtual_list};
use sb_core::catalogue::{Filed, Session, SessionId, Shelf};
use sb_core::filing::FolderId;
use sb_core::location::LocationId;

use super::first_sync;
use super::header::{Under, header};
use super::menus::{self, FOLDER_MENU_WIDTH, MOVE_MENU_WIDTH, SESSION_MENU_WIDTH};
use super::paths_editor::{PathsEditor, PathsEditorEvent};
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{
    ButtonKind, Tone, button, focus_ring, icon, icon_button, location_tag, overline, tag,
    tag_width, text_width, warn_chip,
};
use crate::drag;
use crate::format;
use crate::icons::SbIcon;
use crate::keymap;
use crate::menu;
use crate::overlay::{self, Layer};
use crate::services::library::LibraryEvent;
use crate::services::{
    ActiveServices as _, Dragged, DropTarget, Facets, LibraryService, Month, PageView, PathChip,
    SessionsPage, Stats, SyncService, view,
};
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _};

/// What the empty pages say (DESIGN §4.14).
const NOTHING_STARRED: &str = "Star a session to keep it here.";
const NOTHING_ARCHIVED: &str = "Nothing is archived yet. Sessions appear here once a sync has \
                                copied and indexed them.";
const INDEXING: &str = "Indexing what was copied. Sessions appear here as they are indexed.";
const EMPTY_FOLDER: &str = "Attach a project path above and its sessions file here now and after \
                            every sync. Or drag sessions here from any folder; sessions you place \
                            by hand stay put.";

/// A card's padding at its sides (`design/project/sb.css`, `.scard`: 12px 14px).
const CARD_PADDING: Pixels = px(14.);

/// What the chip of a session whose newer version is held back says.
const HELD_BACK: &str = "held back";

/// The least of a model's name a card's last line keeps room for when it decides whether the
/// tool calls fit: a letter and the ellipsis.
const MODEL_MIN: Pixels = px(12.);

/// The narrowest a tag gets on a page whose lines fit not even with the tags at their caps:
/// its padding, a letter and the ellipsis, so that it never shows the ellipsis alone.
const TAG_LEAST: Pixels = px(28.);

/// What a card's last line keeps spare when it decides whether the tool calls fit, so that
/// text measured a hair wider at a fractional display scale doesn't take them off.
const LINE_SPARE: Pixels = px(4.);

/**
 * The widest of each piece the last line of a page's cards can hold (DESIGN §4.4), but for
 * the agent's mark and the model's name: the mark is always there, and the model takes what
 * the rest leave it.
 */
#[derive(Clone, Debug, Default, PartialEq)]
struct LinePieces {
    /// The tag of each location the page's sessions are from, as wide as it is uncapped.
    locations: Vec<Pixels>,
    /// The tag of each subfolder the page's sessions are filed in, likewise.
    subfolders: Vec<Pixels>,
    /// The "held back" chip, when one of the page's sessions has it.
    held: Option<Pixels>,
    /// One of the page's sessions is filed by hand: its line ends in the hand glyph.
    by_hand: bool,
    /// The widest day.
    day: Pixels,
    /// The widest call count, without its glyph; `None` for a line without counts.
    calls: Option<Pixels>,
}

/**
 * Whether a card `inner` wide inside its padding and border fits a line holding every piece
 * the page's lines can hold, each as wide as the widest (`line`), the tags at their caps,
 * [`MODEL_MIN`] of a model's name and the gaps between them. With the counts in `line`, this
 * decides whether the page's cards show their tool calls (DESIGN §4.4): for the page and not
 * card by card, so that neighbours read alike. A line gives way in that order: the model's
 * name down to its least, then the tags down to their caps, then the counts; and a line that
 * fits not even without counts lets its tags give way past their caps, so that the day shows.
 */
fn line_fits(inner: Pixels, line: &LinePieces, theme: &SbTheme) -> bool {
    let sizes = &theme.sizes;
    let widest = |tags: &[Pixels], cap: Pixels| {
        tags.iter()
            .copied()
            .reduce(Pixels::max)
            .map(|widest| widest.min(cap))
    };
    let pieces: Vec<Pixels> = [
        Some(sizes.icon_xs),
        Some(MODEL_MIN),
        widest(&line.locations, sizes.location_tag_max_width),
        widest(&line.subfolders, sizes.tag_max_width),
        line.held,
        Some(line.day),
        line.calls
            .map(|calls| sizes.icon_xs + theme.spacing.xs + calls),
        line.by_hand.then_some(sizes.icon_xs),
    ]
    .into_iter()
    .flatten()
    .collect();
    line_width(&pieces, theme) <= inner
}

/// How wide a card's last line of `pieces` is, with the gaps between them.
fn line_width(pieces: &[Pixels], theme: &SbTheme) -> Pixels {
    pieces.iter().copied().sum::<Pixels>()
        + theme.spacing.sm * pieces.len().saturating_sub(1) as f32
}

/// What the last lines of a page's cards hold, from its sessions: what [`LinePieces`] measures.
#[derive(Clone, Debug, Default, PartialEq)]
struct PageLine {
    locations: Vec<LocationId>,
    subfolders: Vec<FolderId>,
    held: bool,
    by_hand: bool,
    /// A day as wide as the widest the page shows ([`widest_day`]).
    day: String,
    most_calls: u32,
}

impl PageLine {
    fn of(page: &SessionsPage, shelf: Shelf, now: i64) -> Self {
        let mut line = Self {
            day: widest_day(&page.stats, now),
            ..Self::default()
        };
        for session in &page.sessions {
            if !line.locations.contains(&session.id.location) {
                line.locations.push(session.id.location.clone());
            }
            if let Some(folder) = subfolder_of(session, shelf)
                && !line.subfolders.contains(&folder)
            {
                line.subfolders.push(folder);
            }
            line.held |= session.held_back;
            line.by_hand |= matches!(session.filed, Filed::ByHand(_));
            line.most_calls = line.most_calls.max(session.all.calls);
        }
        line
    }
}

/// The folder inside the one whose page `shelf` is that `session` is filed in, whose tag its
/// card carries (DESIGN §4.4).
fn subfolder_of(session: &Session, shelf: Shelf) -> Option<FolderId> {
    match (session.filed, shelf) {
        (Filed::ByRule(folder) | Filed::ByHand(folder), Shelf::Folder(shown))
            if folder != shown =>
        {
            Some(folder)
        }
        _ => None,
    }
}

/// A day as wide as the widest a page of `stats` shows: the day of its oldest session, which
/// carries its year if any does, with a two-digit day.
fn widest_day(stats: &Stats, now: i64) -> String {
    let day = stats
        .span
        .map(|(oldest, _)| format::day(oldest, now))
        .unwrap_or_default();
    // Geist Mono's digits are all as wide: the month and the year are what differ.
    let month = day.find(' ').map_or("", |at| &day[at..]);
    format!("28{month}")
}

/**
 * What the page's decision on its cards' last lines was made from (DESIGN §4.4), for
 * automation's `state` to report: a card's width inside its padding and border, each tag's
 * width whole and at its cap, the widest day and count, and what was decided.
 */
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LineMeasure {
    pub inner: Pixels,
    /// Each location's and subfolder's tag: its name, its width whole, and at its cap.
    pub tags: Vec<(String, Pixels, Pixels)>,
    pub day: (String, Pixels),
    pub calls: (String, Pixels),
    pub shows_calls: bool,
    pub tags_past_caps: bool,
}

/// One item of the list of cards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// The overline of a month, as an index into the page's months.
    Month(usize),
    /// Up to a row's worth of a month's sessions, as a range of the page's sessions.
    Cards(Range<usize>),
}

/// The list's items for `months`, `columns` cards to a row.
pub fn rows(months: &[Month], columns: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    for (at, month) in months.iter().enumerate() {
        rows.push(Row::Month(at));
        let mut start = month.sessions.start;
        while start < month.sessions.end {
            let end = (start + columns).min(month.sessions.end);
            rows.push(Row::Cards(start..end));
            start = end;
        }
    }
    rows
}

/**
 * How many cards a row `width` wide holds (DESIGN §1.4, §2): the fewest that keep a card no
 * wider than its maximum, which is four at 1440 and more as the window grows, and never fewer
 * than two. A card stays at least its minimum width, which the narrowest window allows.
 */
pub fn columns(width: Pixels, theme: &SbTheme) -> usize {
    let card = |columns: usize| (width - theme.spacing.lg * (columns as f32 - 1.)) / columns as f32;
    let mut columns = 2;
    while card(columns) > theme.sizes.card_max_width {
        columns += 1;
    }
    while columns > 2 && card(columns) < theme.sizes.card_min_width {
        columns -= 1;
    }
    columns
}

/// A card's height: its padding, the title's line, the prompt's two, the last line and the
/// gaps between them, inside its border (DESIGN §4.4).
fn card_height(theme: &SbTheme) -> Pixels {
    theme.spacing.lg * 2.
        + theme.text.title.line_height
        + theme.sizes.card_prompt_height
        + theme.sizes.tag_small_height
        + theme.spacing.md * 2.
        + px(2.)
}

/// Each row's height with the gap under it: 16 under an overline and under a month's last
/// cards, 12 between rows of cards, and the page's 24 under the last row.
pub(crate) fn row_sizes(rows: &[Row], theme: &SbTheme) -> Vec<Size<Pixels>> {
    rows.iter()
        .enumerate()
        .map(|(at, row)| {
            let height = match (row, rows.get(at + 1)) {
                (Row::Month(_), _) => theme.text.overline.line_height + theme.spacing.xl,
                (Row::Cards(_), Some(Row::Cards(_))) => card_height(theme) + theme.spacing.lg,
                (Row::Cards(_), Some(Row::Month(_))) => card_height(theme) + theme.spacing.xl,
                (Row::Cards(_), None) => card_height(theme) + theme.spacing.xxl,
            };
            size(px(0.), height)
        })
        .collect()
}

/// The month whose cards are at the top of a list scrolled `scrolled` down, as an index into
/// the page's months: the one whose overline stays at the top while its cards scroll
/// (DESIGN §4.4). `None` at the very top, where the first overline is in its own place.
fn month_at(
    rows: &[Row],
    sizes: &[Size<Pixels>],
    months: &[Month],
    scrolled: Pixels,
) -> Option<usize> {
    if scrolled <= px(0.) {
        return None;
    }
    let mut bottom = px(0.);
    let row = rows.iter().zip(sizes).find_map(|(row, size)| {
        bottom += size.height;
        (bottom > scrolled).then_some(row)
    })?;
    Some(match row {
        Row::Month(month) => *month,
        Row::Cards(sessions) => {
            months.partition_point(|month| month.sessions.end <= sessions.start)
        }
    })
}

/// What a month's overline says.
fn month_name(month: &Month) -> String {
    month
        .month
        .map_or_else(|| "No date".to_string(), format::month_label)
}

/// The session at `index` moved by a key: `by` cards along the reading order, or `lines` rows
/// up or down in the same column.
fn moved(rows: &[Row], index: usize, by: isize, lines: isize) -> Option<usize> {
    let cards = |row: &Row| match row {
        Row::Cards(sessions) => Some(sessions.clone()),
        Row::Month(_) => None,
    };
    if lines == 0 {
        let last = rows.iter().rev().find_map(cards)?.end - 1;
        return index.checked_add_signed(by).filter(|at| *at <= last);
    }
    let at = rows
        .iter()
        .position(|row| cards(row).is_some_and(|sessions| sessions.contains(&index)))?;
    let column = index - cards(&rows[at])?.start;
    let target = if lines > 0 {
        rows[at + 1..].iter().find_map(cards)
    } else {
        rows[..at].iter().rev().find_map(cards)
    }?;
    Some((target.start + column).min(target.end - 1))
}

pub struct SessionsView {
    pub(super) library: Entity<LibraryService>,
    pub(super) sync: Entity<SyncService>,
    scroll: VirtualListScrollHandle,
    /// The page the list was last laid out for.
    shelf: Shelf,
    rows: Vec<Row>,
    sizes: Rc<Vec<Size<Pixels>>>,
    columns: usize,
    /// How wide a card is, and the row of cards or the list, as far as the window's width
    /// says.
    card_width: Pixels,
    room: Pixels,
    /// What the last lines of the page's cards hold.
    line: PageLine,
    /// Whether the cards show their tool calls ([`line_fits`]).
    shows_calls: bool,
    /// Whether a card's tags may give way past their caps: the page's lines fit not even
    /// without counts ([`line_fits`]).
    tags_past_caps: bool,
    /// How wide the tag of each location and folder the cards name is, whole.
    tag_widths: HashMap<String, Pixels>,
    /// How wide the "held back" chip is.
    held_width: Pixels,
    measure: LineMeasure,
    /// The card under the pointer.
    hovered: Option<SessionId>,
    /// The rows are of what the library held before it last changed.
    stale: bool,
    /// Where the keyboard rests in the main area: while it is there, the selected card has
    /// the keyboard's ring.
    focus: FocusHandle,
    /// How the page was set to show when it was laid out.
    view: PageView,
    /// The places in the page's sessions of those that show, in their order, and their months
    /// as ranges of `shown`.
    shown: Vec<usize>,
    months: Vec<Month>,
    /// What the filter chips offer.
    facets: Facets,
    /// The list view's rows (DESIGN §4.5).
    list_rows: Vec<list::ListRow>,
    list_sizes: Rc<Vec<Size<Pixels>>>,
    list_scroll: VirtualListScrollHandle,
    /// Where each session that shows was drawn last, by its place in `shown`: where its menu
    /// opens from the keys.
    drawn: Rc<RefCell<HashMap<usize, Bounds<Pixels>>>>,
    /// The filter field, made with the first frame, and the page its placeholder names.
    filter: Option<(Entity<InputState>, Subscription)>,
    filter_for: Option<Shelf>,
    /// The paths editor while it is open, and the folder it was asked for before it is made.
    editor: Option<(Entity<PathsEditor>, Subscription)>,
    edit: Option<FolderId>,
    _observers: [Subscription; 3],
}

impl SessionsView {
    pub fn new(focus: FocusHandle, cx: &mut Context<Self>) -> Self {
        let services = cx.services();
        let (library, sync) = (services.library.clone(), services.sync.clone());
        Self {
            _observers: [
                cx.observe(&library, |this, _, cx| {
                    this.stale = true;
                    cx.notify();
                }),
                cx.observe(&sync, |_, _, cx| cx.notify()),
                cx.subscribe(&library, |this, _, event, cx| {
                    if let LibraryEvent::EditPaths(folder) = event {
                        this.edit = Some(*folder);
                        cx.notify();
                    }
                }),
            ],
            library,
            sync,
            view: PageView::default(),
            shown: Vec::new(),
            months: Vec::new(),
            facets: Facets::default(),
            list_rows: Vec::new(),
            list_sizes: Rc::default(),
            list_scroll: VirtualListScrollHandle::new(),
            drawn: Rc::default(),
            filter: None,
            filter_for: None,
            editor: None,
            edit: None,
            scroll: VirtualListScrollHandle::new(),
            shelf: Shelf::All,
            rows: Vec::new(),
            sizes: Rc::default(),
            columns: 0,
            card_width: px(0.),
            room: px(0.),
            line: PageLine::default(),
            shows_calls: false,
            tags_past_caps: false,
            tag_widths: HashMap::new(),
            held_width: px(0.),
            measure: LineMeasure::default(),
            hovered: None,
            stale: true,
            focus,
        }
    }

    /// Lays the rows out again when the sessions or the width changed. Another page than
    /// the last starts at its top.
    fn lay_out(&mut self, window: &Window, theme: &SbTheme, cx: &App) {
        let shelf = self.library.read(cx).shelf();
        if shelf != self.shelf {
            self.shelf = shelf;
            self.scroll.base_handle().set_offset(point(px(0.), px(0.)));
            self.list_scroll
                .base_handle()
                .set_offset(point(px(0.), px(0.)));
            self.editor = None;
        }
        let view = self.library.read(cx).view();
        let viewport = window.viewport_size().width;
        let sidebar = if viewport >= theme.sizes.sidebar_collapse_width {
            theme.sizes.sidebar_width
        } else {
            px(0.)
        };
        let room = viewport - sidebar - theme.sizes.page_side * 2.;
        self.room = room;
        let columns = columns(room, theme);
        self.card_width = (room - theme.spacing.lg * (columns as f32 - 1.)) / columns as f32;
        if self.stale || columns != self.columns || view != self.view {
            self.stale = false;
            self.columns = columns;
            self.view = view;
            let page = self.library.read(cx).sessions();
            let sessions = page.map_or(&[][..], |page| &page.sessions[..]);
            self.shown = view::shown(sessions, &self.view, format::now());
            self.months = view::months(sessions, &self.shown);
            self.facets = Facets::of(sessions);
            self.rows = rows(&self.months, columns);
            self.sizes = Rc::new(row_sizes(&self.rows, theme));
            self.list_rows = list::rows(&self.months);
            self.list_sizes = Rc::new(list::row_sizes(&self.list_rows, theme));
            self.line = page.map_or_else(PageLine::default, |page| {
                PageLine::of(page, shelf, format::now())
            });
        }
        // The locations may arrive, or be renamed, after the sessions; and so may the folders.
        let (sync, library) = (self.sync.read(cx), self.library.read(cx));
        self.tag_widths.clear();
        let mut tags = Vec::new();
        let mut measure = |name: &str, cap: Pixels| {
            let width = tag_width(name, theme, window);
            self.tag_widths.insert(name.to_string(), width);
            tags.push((name.to_string(), width, width.min(cap)));
            width
        };
        let locations = (self.line.locations.iter())
            .filter_map(|id| sync.location(id))
            .map(|location| measure(&location.name, theme.sizes.location_tag_max_width))
            .collect();
        let subfolders = (self.line.subfolders.iter())
            .filter_map(|id| library.folder(*id))
            .map(|folder| measure(&folder.name, theme.sizes.tag_max_width))
            .collect();
        self.held_width = text_width(HELD_BACK, &theme.text.small, window) + theme.spacing.sm * 2.;
        let calls = format::count(u64::from(self.line.most_calls));
        let day = text_width(&self.line.day, &theme.text.count, window);
        let calls_width = text_width(&calls, &theme.text.count, window);
        let pieces = LinePieces {
            locations,
            subfolders,
            held: self.line.held.then_some(self.held_width),
            by_hand: self.line.by_hand,
            day,
            calls: Some(calls_width),
        };
        let inner = self.card_width - CARD_PADDING * 2. - px(2.);
        /* The page's cards show their counts when an ordinary card's line has room for them:
        a subfolder's tag, the held-back chip and the hand are a card's own, and a card that
        carries one drops its own count when its line is short of room (`card`). */
        self.shows_calls = line_fits(
            inner - LINE_SPARE,
            &LinePieces {
                subfolders: Vec::new(),
                held: None,
                by_hand: false,
                ..pieces.clone()
            },
            theme,
        );
        self.tags_past_caps = !line_fits(
            inner,
            &LinePieces {
                calls: None,
                ..pieces
            },
            theme,
        );
        self.measure = LineMeasure {
            inner,
            tags,
            day: (self.line.day.clone(), day),
            calls: (calls, calls_width),
            shows_calls: self.shows_calls,
            tags_past_caps: self.tags_past_caps,
        };
    }

    /// The paths editor, while it is open.
    pub fn editor(&self) -> Option<&Entity<PathsEditor>> {
        self.editor.as_ref().map(|(editor, _)| editor)
    }

    /// How many sessions show.
    pub fn shown(&self) -> usize {
        self.shown.len()
    }

    /// What the cards' last lines were last decided from.
    pub fn line_measure(&self) -> &LineMeasure {
        &self.measure
    }

    /**
     * Moves the selection (see [`moved`]), in the list `lines` rows up or down; with nothing
     * selected, selects the first session that shows.
     */
    pub fn step(&mut self, by: isize, lines: isize, cx: &mut Context<Self>) {
        let place = self.selected_place(cx);
        let to = if self.view.list {
            match (place, lines) {
                (None, _) => Some(0),
                // The list has no columns to move between.
                (Some(_), 0) => return,
                (Some(place), lines) => place.checked_add_signed(lines),
            }
        } else {
            place.map_or(Some(0), |place| moved(&self.rows, place, by, lines))
        };
        let Some(to) = to.filter(|to| *to < self.shown.len()) else {
            return;
        };
        self.reveal(to);
        let index = self.shown[to];
        self.library
            .update(cx, |library, cx| library.select_index(index, cx));
    }

    /// Scrolls the session at the place `place` into view.
    fn reveal(&self, place: usize) {
        if self.view.list {
            if let Some(row) =
                (self.list_rows.iter()).position(|row| *row == list::ListRow::Session(place))
            {
                self.list_scroll
                    .scroll_to_item(row, ScrollStrategy::Nearest);
            }
        } else if let Some(row) = self
            .rows
            .iter()
            .position(|row| matches!(row, Row::Cards(sessions) if sessions.contains(&place)))
        {
            self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        }
    }

    /// Where the selected session is among those that show.
    fn selected_place(&self, cx: &App) -> Option<usize> {
        let index = self.library.read(cx).selected_index()?;
        self.shown.iter().position(|at| *at == index)
    }

    /// The selected session and where it is on the page.
    fn selected(&self, cx: &Context<Self>) -> Option<(SessionId, usize)> {
        let library = self.library.read(cx);
        Some((library.selected()?.clone(), library.selected_index()?))
    }

    /// Opens the selected session.
    pub fn open_selected(&mut self, cx: &mut Context<Self>) {
        if let Some((id, at)) = self.selected(cx) {
            self.library
                .update(cx, |library, cx| library.open_session_at(&id, at, cx));
        }
    }

    /// Stars the selected sessions, or takes their stars away when they all have one.
    pub fn star_selected(&mut self, cx: &mut Context<Self>) {
        self.library.update(cx, |library, cx| {
            let ids = library.selection();
            let starred = ids
                .iter()
                .all(|id| library.session(id).is_some_and(|it| it.starred));
            if !ids.is_empty() {
                library.star(ids, !starred, cx);
            }
        });
    }

    /// Moves the selected sessions to Trash.
    pub fn trash_selected(&mut self, cx: &mut Context<Self>) {
        self.library.update(cx, |library, cx| {
            let ids = library.selection();
            if !ids.is_empty() {
                library.trash(ids, cx);
            }
        });
    }

    /// Selects every session that shows.
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        let library = self.library.read(cx);
        let Some(page) = library.sessions() else {
            return;
        };
        let ids: Vec<SessionId> = (self.shown.iter())
            .map(|at| page.sessions[*at].id.clone())
            .collect();
        let selected = match (library.selected().cloned(), library.selected_index()) {
            (Some(id), at) if ids.contains(&id) => (id, at),
            _ => match self.shown.first() {
                Some(first) => (page.sessions[*first].id.clone(), Some(*first)),
                None => return,
            },
        };
        self.library
            .update(cx, |library, cx| library.choose(ids, selected, cx));
    }

    /**
     * Opens the selected session's menu, or with `moving` its Move to folder (`M`), under
     * where it was drawn.
     */
    pub fn menu_selected(&mut self, moving: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(place) = self.selected_place(cx) else {
            return;
        };
        let at = self.drawn.borrow().get(&place).map_or_else(
            || window.mouse_position(),
            |bounds| bounds.bottom_left() + point(px(12.), px(-8.)),
        );
        let index = self.shown[place];
        self.open_menu(index, moving, at, window, cx);
    }

    /// Opens the menu of the session at `index` of the page at `at`.
    fn open_menu(
        &mut self,
        index: usize,
        moving: bool,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let library = self.library.read(cx);
        let Some(session) = library
            .sessions()
            .and_then(|page| page.sessions.get(index))
            .cloned()
        else {
            return;
        };
        let ids = library.acted_on(&session.id);
        let (entries, width) = if moving {
            (
                menus::move_entries(ids, session.filed, library),
                MOVE_MENU_WIDTH,
            )
        } else {
            (menus::session_menu(&session, ids, cx), SESSION_MENU_WIDTH)
        };
        menu::open(entries, at, width, window, cx);
    }

    /**
     * A click on the session at the place `place`: it alone is selected; with `Ctrl` it joins
     * the selection or leaves it; with `Shift` the run from the selected one to it is.
     */
    fn click(
        &mut self,
        place: usize,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The page takes the keyboard, so that its keys act on what was clicked.
        self.focus.focus(window, cx);
        let library = self.library.read(cx);
        let Some(page) = library.sessions() else {
            return;
        };
        let index = self.shown[place];
        let id = page.sessions[index].id.clone();
        let modifiers = event.modifiers();
        if event.click_count() > 1 {
            self.library
                .update(cx, |library, cx| library.open_session_at(&id, index, cx));
        } else if modifiers.secondary(cx) {
            let mut chosen = library.selection();
            if let Some(at) = chosen.iter().position(|it| *it == id) {
                chosen.remove(at);
            } else {
                chosen.push(id.clone());
            }
            self.library.update(cx, |library, cx| {
                library.choose(chosen, (id, Some(index)), cx)
            });
        } else if modifiers.shift
            && let Some(from) = self.selected_place(cx)
        {
            let run = from.min(place)..=from.max(place);
            let chosen = run
                .map(|at| page.sessions[self.shown[at]].id.clone())
                .collect();
            self.library.update(cx, |library, cx| {
                library.choose(chosen, (id, Some(index)), cx)
            });
        } else {
            self.library
                .update(cx, |library, cx| library.select_session(&id, index, cx));
        }
    }

    /// A press with the right button on the session at `index`: it is selected unless it is
    /// in the selection, and its menu opens at the pointer.
    fn right_click(
        &mut self,
        index: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus.focus(window, cx);
        let library = self.library.read(cx);
        let Some(id) = library
            .sessions()
            .and_then(|page| page.sessions.get(index))
            .map(|it| it.id.clone())
        else {
            return;
        };
        if !library.is_selected(&id) {
            self.library
                .update(cx, |library, cx| library.select_session(&id, index, cx));
        }
        self.open_menu(index, false, event.position, window, cx);
    }

    /// Whether a text field of the page has the keyboard: single letters are its then.
    pub fn is_typing(&self, window: &Window, cx: &App) -> bool {
        self.filter
            .as_ref()
            .is_some_and(|(input, _)| input.focus_handle(cx).is_focused(window))
            || self
                .editor
                .as_ref()
                .is_some_and(|(editor, _)| editor.read(cx).is_typing(window, cx))
    }

    /// What a drag of the session at `index` carries: the selection when it is in it.
    fn dragged(&self, session: &Session, cx: &App) -> Dragged {
        let ids = self.library.read(cx).acted_on(&session.id);
        Dragged::Sessions {
            ids,
            title: session
                .title
                .clone()
                .unwrap_or_else(|| UNTITLED.to_string()),
        }
    }

    /// Opens the paths editor of the folder shown; `short` for an empty folder's.
    fn open_editor(
        &mut self,
        folder: FolderId,
        short: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = cx.new(|cx| PathsEditor::new(folder, short, window, cx));
        let heard = cx.subscribe(&editor, |this, _, event, cx| match event {
            PathsEditorEvent::Closed => {
                this.editor = None;
                cx.notify();
            }
        });
        self.editor = Some((editor, heard));
        cx.notify();
    }

    /// The list's rows in `range`. Each keeps the page's side padding at its right, so that the
    /// list, and its scrollbar, reach the main area's edge.
    fn render_rows(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = cx.sb_theme().clone();
        let library = self.library.read(cx);
        let Some(page) = library.sessions() else {
            return Vec::new();
        };
        let now = format::now();
        let keyboard = self.focus.is_focused(window) && window.last_input_was_keyboard();
        range
            .filter_map(|at| self.rows.get(at))
            .map(|row| match row {
                Row::Month(month) => div()
                    .pr(theme.sizes.page_side)
                    .child(overline(&month_name(&self.months[*month]), &theme))
                    .into_any_element(),
                Row::Cards(places) => div()
                    .w_full()
                    .flex()
                    .gap(theme.spacing.lg)
                    .pr(theme.sizes.page_side)
                    .on_children_prepainted({
                        let (drawn, start) = (self.drawn.clone(), places.start);
                        move |bounds, _, _| {
                            let mut drawn = drawn.borrow_mut();
                            for (at, bounds) in bounds.into_iter().enumerate() {
                                drawn.insert(start + at, bounds);
                            }
                        }
                    })
                    .children(places.clone().map(|place| {
                        let index = self.shown[place];
                        self.card(
                            (place, index),
                            &page.sessions[index],
                            (now, keyboard),
                            &theme,
                            window,
                            cx,
                        )
                    }))
                    // A short row's cards stay as wide as a full one's: what fills the row
                    // takes a card's padding and border too.
                    .children((places.len()..self.columns).map(|_| {
                        div()
                            .flex_1()
                            .flex_basis(px(0.))
                            .px(CARD_PADDING)
                            .border_1()
                            .border_color(gpui::transparent_black())
                    }))
                    .into_any_element(),
            })
            .collect()
    }

    /// How narrow a tag `natural` wide may get when its line is tight: down to its cap, or
    /// past it on a page whose lines fit not even then ([`line_fits`]).
    fn least(&self, natural: Pixels, cap: Pixels) -> Pixels {
        natural.min(if self.tags_past_caps { TAG_LEAST } else { cap })
    }

    /// A tag on a card's last line: whole while the line has room for it, else giving way down
    /// to its least width ([`Self::least`]). `cut` says it does give way: then it ends in an
    /// ellipsis and says its name in full on hover.
    fn line_tag(
        &self,
        id: ElementId,
        tag: Div,
        name: &str,
        least: Pixels,
        cut: bool,
    ) -> AnyElement {
        let tag = tag.flex_shrink(1.).min_w(least);
        if !cut {
            return tag.into_any_element();
        }
        // On a tight line a tag keeps to its cap and leaves the rest to the model's name.
        let tag = tag.max_w(least);
        let name = SharedString::from(name.to_string());
        tag.id(id)
            .tooltip(move |window, cx| Tooltip::new(name.clone()).build(window, cx))
            .into_any_element()
    }

    /// A session's card (DESIGN §4.4): its title, the start of its first prompt on two lines,
    /// then its agent's mark and model, where it is from, when it ended and how many tool
    /// calls it made. `keyboard` says the keyboard is on the page, and so on the selected
    /// card.
    fn card(
        &self,
        (index, at): (usize, usize),
        session: &Session,
        (now, keyboard): (i64, bool),
        theme: &SbTheme,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let library = self.library.read(cx);
        let selected = library.is_selected(&session.id);
        // With nothing selected, the keyboard on the page is on its first card, where the arrows
        // start.
        let ringed = keyboard
            && library
                .selected_index()
                .map_or(index == 0, |selected| selected == at);
        let lifted = drag::lifts(
            library,
            |what| matches!(what, Dragged::Sessions { ids, .. } if ids.contains(&session.id)),
        );
        // The hover bar shows on the selected card and on the one under the pointer.
        let barred = selected || self.hovered.as_ref() == Some(&session.id);
        let title: SharedString = session
            .title
            .clone()
            .unwrap_or_else(|| "Untitled session".to_string())
            .into();
        // With one agent in the archive a card names the model alone (DESIGN §4.2.10).
        let model = agents::model(&session.id.provider, &session.models, cx);
        let agent = (library.agents() > 1 || model.is_none())
            .then(|| agents::name(&session.id.provider, cx));
        let model = agent
            .into_iter()
            .chain(model)
            .collect::<Vec<_>>()
            .join(" · ");
        // On a folder's page a tag names the folder of a session from one inside it.
        let subfolder = subfolder_of(session, library.shelf()).and_then(|it| library.folder(it));
        let location = self.sync.read(cx).location(&session.id.location);
        let by_hand = matches!(session.filed, Filed::ByHand(_));
        // The day always shows, and the calls on a page whose cards have room for them, but
        // none where there were none.
        let calls = (self.shows_calls && session.all.calls > 0)
            .then(|| format::count(u64::from(session.all.calls)))
            .filter(|calls| {
                let extras = subfolder.is_some() || session.held_back || by_hand;
                let capped = |name: &str, cap: Pixels| {
                    self.tag_widths
                        .get(name)
                        .copied()
                        .unwrap_or_default()
                        .min(cap)
                };
                let own: Vec<Pixels> = [
                    Some(theme.sizes.icon_xs),
                    Some(MODEL_MIN),
                    location.map(|it| capped(&it.name, theme.sizes.location_tag_max_width)),
                    subfolder.map(|it| capped(&it.name, theme.sizes.tag_max_width)),
                    session.held_back.then_some(self.held_width),
                    Some(text_width(&self.line.day, &theme.text.count, window)),
                    Some(
                        theme.sizes.icon_xs
                            + theme.spacing.xs
                            + text_width(calls, &theme.text.count, window),
                    ),
                    by_hand.then_some(theme.sizes.icon_xs),
                ]
                .into_iter()
                .flatten()
                .collect();
                !extras
                    || line_width(&own, theme)
                        <= self.card_width - CARD_PADDING * 2. - px(2.) - LINE_SPARE
            });
        let day = session
            .ended
            .or(session.started)
            .map(|ended| format::day(ended, now));
        // The tags are whole unless the line, with the model's name at its least, has no room
        // for them; then they give way, down to their least.
        let natural = |name: &str| self.tag_widths.get(name).copied().unwrap_or_default();
        let tags: Vec<(Pixels, Pixels)> = (location.map(|it| {
            let natural = natural(&it.name);
            (
                natural,
                self.least(natural, theme.sizes.location_tag_max_width),
            )
        }))
        .into_iter()
        .chain(subfolder.map(|it| {
            let natural = natural(&it.name);
            (natural, self.least(natural, theme.sizes.tag_max_width))
        }))
        .collect();
        let count_width = |text: &str| text_width(text, &theme.text.count, window);
        let line: Vec<Pixels> = [
            Some(theme.sizes.icon_xs),
            Some(MODEL_MIN),
            session.held_back.then_some(self.held_width),
            day.as_deref().map(count_width),
            calls
                .as_deref()
                .map(|calls| theme.sizes.icon_xs + theme.spacing.xs + count_width(calls)),
            by_hand.then_some(theme.sizes.icon_xs),
        ]
        .into_iter()
        .flatten()
        .chain(tags.iter().map(|(natural, _)| *natural))
        .collect();
        let inner = self.card_width - CARD_PADDING * 2. - px(2.);
        /* With the model's name whole, the count and the tags whole: does it fit? If not, the
        tags keep to their caps, so that the name and the count keep their room; and the name,
        which gives way first, says itself in full on hover, as a cut tag does. */
        let whole =
            line_width(&line, theme) - MODEL_MIN + text_width(&model, &theme.text.small, window);
        let tight = whole > inner;
        let given: Pixels = tags
            .iter()
            .map(|(natural, least)| (*natural - *least).max(px(0.)))
            .sum();
        let model_cut = tight && whole - given > inner;
        let cut = |(natural, least): (Pixels, Pixels)| tight && natural > least;

        let id = session.id.clone();
        let star = hover_button(
            ("star", index),
            format!(
                "{} ({})",
                if session.starred { "Unstar" } else { "Star" },
                keymap::STAR.label()
            ),
            theme,
        )
        .on_click(cx.listener({
            let id = id.clone();
            move |this, _, _, cx| {
                cx.stop_propagation();
                this.library.update(cx, |library, cx| {
                    let ids = library.acted_on(&id);
                    if ids.len() > 1 {
                        let starred = !library.session(&id).is_some_and(|it| it.starred);
                        library.star(ids, starred, cx);
                    } else {
                        library.toggle_star(&id, at, cx);
                    }
                });
            }
        }))
        .child(if session.starred {
            icon(SbIcon::StarFilled, theme.sizes.icon_sm, c.yellow)
        } else {
            icon(SbIcon::Star, theme.sizes.icon_sm, c.fg2)
        })
        .automation_target_with(move || format!("card/{index}/star").into());
        let move_to = hover_button(
            ("move", index),
            format!("Move to folder ({})", keymap::MOVE.label()),
            theme,
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            cx.stop_propagation();
            this.open_menu(at, true, window.mouse_position(), window, cx);
        }))
        .child(icon(SbIcon::FolderInput, theme.sizes.icon_sm, c.fg2))
        .automation_target_with(move || format!("card/{index}/move").into());
        let trash = hover_button(
            ("trash", index),
            format!("Move to Trash ({})", keymap::TRASH.label()),
            theme,
        )
        .on_click(cx.listener({
            let id = id.clone();
            move |this, _, _, cx| {
                cx.stop_propagation();
                this.library.update(cx, |library, cx| {
                    let ids = library.acted_on(&id);
                    library.trash(ids, cx);
                });
            }
        }))
        .child(icon(SbIcon::Trash, theme.sizes.icon_sm, c.fg2))
        .automation_target_with(move || format!("card/{index}/trash").into());
        let more = hover_button(
            ("more", index),
            format!("More actions ({})", keymap::MENU_KEY.label()),
            theme,
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            cx.stop_propagation();
            this.open_menu(at, false, window.mouse_position(), window, cx);
        }))
        .child(icon(SbIcon::Ellipsis, theme.sizes.icon_sm, c.fg2))
        .automation_target_with(move || format!("card/{index}/more").into());
        let card = div()
            .id(("card", index))
            .debug_selector(move || format!("card-{index}"))
            .group("card")
            .role(Role::ListItem)
            .relative()
            .flex_1()
            .flex_basis(px(0.))
            .min_w_0()
            .h(card_height(theme))
            .flex()
            .flex_col()
            .gap(theme.spacing.md)
            .py(theme.spacing.lg)
            .px(CARD_PADDING)
            .rounded(theme.radii.card)
            .bg(c.s1)
            .border_1()
            .map(|this| {
                if lifted {
                    this.border_dashed()
                        .border_color(c.line_strong)
                        .opacity(LIFTED_OPACITY)
                } else if ringed {
                    this.border_color(c.accent).shadow(focus_ring(false, theme))
                } else if selected {
                    this.border_color(c.line_strong)
                        .shadow(theme.shadows.card.clone())
                } else {
                    this.border_color(c.line_subtle).hover(|style| {
                        style
                            .border_color(c.line_strong)
                            .shadow(theme.shadows.card.clone())
                    })
                }
            })
            .on_hover(cx.listener({
                let id = id.clone();
                move |this, hovered: &bool, _, cx| {
                    if *hovered {
                        this.hovered = Some(id.clone());
                    } else if this.hovered.as_ref() == Some(&id) {
                        this.hovered = None;
                    }
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.click(index, event, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.right_click(at, event, window, cx);
                }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.sm)
                    // The title ends short of the hover bar while the bar shows.
                    .when(barred, |this| {
                        this.pr(theme.sizes.hover_button * HOVER_BUTTONS + theme.spacing.md)
                    })
                    .child(
                        div()
                            .id(("title", index))
                            .role(Role::Link)
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_spec(&theme.text.title)
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.library.update(cx, |library, cx| {
                                    library.open_session_at(&id, at, cx);
                                });
                            }))
                            .child(title)
                            .automation_target_with(move || format!("card/{index}/title").into()),
                    )
                    // The bar's own star says so while it shows.
                    .when(session.starred && !barred, |this| {
                        this.child(icon(SbIcon::StarFilled, theme.sizes.icon_xs, c.yellow))
                    }),
            )
            .child(
                div()
                    .h(theme.sizes.card_prompt_height)
                    .overflow_hidden()
                    .line_clamp(2)
                    .text_ellipsis()
                    .text_color(c.fg2)
                    .text_spec(&theme.text.secondary)
                    .child(session.first_prompt.clone().unwrap_or_default()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.sm)
                    .h(theme.sizes.tag_small_height)
                    .text_color(c.fg3)
                    .text_spec(&theme.text.small)
                    .whitespace_nowrap()
                    .child(agents::mark(&session.id.provider, theme.sizes.icon_xs, cx))
                    // The model's name takes the room the rest leave it, and gives way first,
                    // down to its least: then the tags give way.
                    .child({
                        let name = SharedString::from(model);
                        div()
                            .id(("card-model", index))
                            .flex_1()
                            .flex_basis(px(0.))
                            .min_w(MODEL_MIN)
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(name.clone())
                            .when(model_cut, |this| {
                                this.tooltip(move |window, cx| {
                                    Tooltip::new(name.clone()).build(window, cx)
                                })
                            })
                    })
                    .children(location.map(|location| {
                        self.line_tag(
                            ("card-location", index).into(),
                            location_tag(location.name.clone(), &location.kind, true, theme),
                            &location.name,
                            tags[0].1,
                            cut(tags[0]),
                        )
                    }))
                    .children(subfolder.map(|folder| {
                        let at = tags.len() - 1;
                        self.line_tag(
                            ("card-subfolder", index).into(),
                            tag(folder.name.clone(), Tone::Neutral, true, theme),
                            &folder.name,
                            tags[at].1,
                            cut(tags[at]),
                        )
                    }))
                    .when(session.held_back, |this| {
                        this.child(warn_chip(HELD_BACK, theme))
                    })
                    .children(
                        day.map(|day| div().flex_none().text_spec(&theme.text.count).child(day)),
                    )
                    .children(calls.map(|calls| {
                        div()
                            .debug_selector(move || format!("card-calls-{index}"))
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(theme.spacing.xs)
                            .child(icon(SbIcon::Puzzle, theme.sizes.icon_xs, c.fg3))
                            .child(div().text_spec(&theme.text.count).child(calls))
                            .automation_target_with(move || format!("card/{index}/calls").into())
                    }))
                    .when(by_hand, |this| {
                        this.child(
                            div()
                                .id(("card-by-hand", index))
                                .flex_none()
                                .tooltip(|window, cx| {
                                    Tooltip::new("Filed by hand").build(window, cx)
                                })
                                .child(icon(SbIcon::Hand, theme.sizes.icon_xs, c.fg3)),
                        )
                    }),
            )
            .child(
                // The hover bar (DESIGN §4.4).
                div()
                    .absolute()
                    .top(theme.spacing.md)
                    .right(theme.spacing.md)
                    .flex()
                    .p(px(2.))
                    .rounded(theme.radii.input)
                    .bg(c.s2)
                    .border_1()
                    .border_color(c.line)
                    .shadow(theme.shadows.card.clone())
                    /* Under the pointer it shows as soon as it is drawn; `hovered` follows
                    with the pointer's next move. */
                    .when(!barred || lifted, |this| {
                        this.opacity(0.)
                            .group_hover("card", |style| style.opacity(1.))
                    })
                    .child(star)
                    .child(move_to)
                    .child(trash)
                    .child(more),
            );
        drag::source(card, self.dragged(session, cx))
            .automation_target_with(move || format!("card/{index}").into())
    }
}

/// The hover bar's buttons: Star, Move to folder, Move to Trash, More.
const HOVER_BUTTONS: f32 = 4.;

/// How a card or a row that is dragged stays in place (`.scard.lifted`).
const LIFTED_OPACITY: f32 = 0.45;

/// What a session is called when its agent stored no title and it has no prompt.
const UNTITLED: &str = "Untitled session";

/// A button of a card's hover bar (26 square), with its label for assistive technology.
fn hover_button(id: impl Into<ElementId>, label: String, theme: &SbTheme) -> gpui::Stateful<Div> {
    let hover = theme.colors.s3;
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label)
        .flex()
        .items_center()
        .justify_center()
        .size(theme.sizes.hover_button)
        .rounded(theme.radii.control)
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
}

/**
 * A folder's attached paths as chips (DESIGN §4.4), or what the line says while it has none;
 * then Edit paths, and at the right whether a subfolder claims a path under these first. It is
 * one line: where the chips don't fit, each gives way in its path. A chip drags onto another
 * folder in the sidebar, which takes the path.
 */
fn paths_line(
    paths: &[PathChip],
    folder: FolderId,
    library: &LibraryService,
    sync: &SyncService,
    theme: &SbTheme,
) -> AnyElement {
    let c = &theme.colors;
    let name = library.folder(folder).map_or("", |it| it.name.as_str());
    let first_claim = library.attached().iter().find_map(|inner| {
        let below = std::iter::successors(library.folder(inner.attached.folder), |it| {
            it.parent.and_then(|parent| library.folder(parent))
        })
        .skip(1)
        .any(|above| above.id == folder);
        (below
            && paths
                .iter()
                .any(|path| path.path.contains(&inner.attached.path)))
        .then(|| {
            (
                library
                    .folder(inner.attached.folder)
                    .map_or(String::new(), |it| it.name.clone()),
                inner.typed.clone(),
            )
        })
    });
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(theme.spacing.md)
        .h(theme.sizes.path_chip_height)
        .overflow_hidden()
        .child(icon(SbIcon::Link, theme.sizes.icon_sm, c.fg3))
        .child(
            div()
                .flex_none()
                .mr(theme.spacing.xs)
                .text_color(c.fg2)
                .text_spec(&theme.text.control_small)
                .child("Project paths"),
        )
        .when(paths.is_empty(), |this| {
            this.child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_color(c.fg3)
                    .text_spec(&theme.text.secondary)
                    .child(format!(
                        "None yet. Sessions started under a path you attach here file \
                         themselves into {name}."
                    )),
            )
        })
        .children(paths.iter().enumerate().map(|(at, path)| {
            let location = path
                .location
                .as_ref()
                .and_then(|location| sync.location(location));
            let dragged = Dragged::Path {
                path: (path.typed.clone(), path.location.clone()),
                normal: path.path.clone(),
                from: Some(folder),
                sessions: path.sessions,
            };
            let lifted = drag::lifts(library, |what| *what == dragged);
            // A long path gives way in its middle; its location and its count stay whole.
            let chip = div()
                .id(("path-chip", at))
                .debug_selector(move || format!("path-chip-{at}"))
                .cursor_grab()
                .when(lifted, |this| this.border_dashed().opacity(0.5))
                .min_w_0()
                .flex_shrink(1.)
                .flex()
                .items_center()
                .gap(theme.spacing.sm)
                .h(theme.sizes.path_chip_height)
                .pl(theme.spacing.xs)
                .pr(theme.spacing.md)
                .rounded(theme.radii.pill)
                .bg(c.s2)
                .border_1()
                .border_color(c.line_subtle)
                .whitespace_nowrap()
                .child(match location {
                    Some(location) => {
                        location_tag(location.name.clone(), &location.kind, false, theme)
                            .into_any_element()
                    }
                    None => tag("any location", Tone::Neutral, false, theme).into_any_element(),
                })
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis_middle()
                        .text_spec(&theme.text.path)
                        .child(path.typed.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(c.fg3)
                        .text_spec(&theme.text.count)
                        .child(format::count(path.sessions as u64)),
                );
            drag::source(chip, dragged)
                .automation_target_with(move || format!("page/path/{at}").into())
        }))
        .child(
            button("edit-paths", ButtonKind::Ghost, true, true, theme)
                .on_click(move |_, _, cx| {
                    let library = cx.services().library.clone();
                    library.update(cx, |library, cx| library.edit_paths(folder, cx));
                })
                .child(icon(SbIcon::Pen, theme.sizes.icon_xs, c.fg2))
                .child("Edit paths")
                .automation_target("page/edit-paths"),
        )
        .children(first_claim.map(|(subfolder, typed)| {
            // One line of text, which gives way before the chips do.
            div()
                .ml_auto()
                .min_w_0()
                .flex_shrink(16.)
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_color(c.fg3)
                .text_spec(&theme.text.secondary)
                .child(format!(
                    "Subfolder {subfolder} claims {} first",
                    crate::components::middle_cut(&typed, 32)
                ))
        }))
        .automation_target("page/paths")
}

/// The empty folder's drop area (`design/project/sb.css`, `.empty`): the gap between its
/// pieces, its tile and the width of its text.
const EMPTY_GAP: Pixels = px(10.);
const EMPTY_TILE: Pixels = px(48.);
const EMPTY_TEXT_WIDTH: Pixels = px(460.);

/**
 * What a page says in place of its cards while it has none. An empty folder's is a drop area
 * for sessions, drawn as the drop target while a drag over it would be taken (`targeted`).
 */
fn empty(
    shelf: Shelf,
    folder: &str,
    indexing: bool,
    targeted: bool,
    theme: &SbTheme,
) -> AnyElement {
    let c = &theme.colors;
    let centred = div()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(EMPTY_GAP);
    match shelf {
        Shelf::Folder(id) => drag::target(centred, DropTarget::Folder(id))
            .mb(theme.spacing.xxl)
            .rounded(theme.radii.sheet)
            .border_1()
            .map(|this| {
                if targeted {
                    this.bg(c.accent_soft).border_color(c.accent)
                } else {
                    this.border_dashed().border_color(c.line_strong)
                }
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(EMPTY_TILE)
                    .rounded(theme.radii.sheet)
                    .bg(c.s2)
                    .child(icon(SbIcon::Folder, theme.sizes.icon_lg, c.fg3)),
            )
            .child(
                div()
                    .text_spec(&theme.text.empty_title)
                    .child(format!("Nothing in {folder} yet")),
            )
            .child(
                div()
                    .max_w(EMPTY_TEXT_WIDTH)
                    .text_center()
                    .text_color(c.fg2)
                    .child(EMPTY_FOLDER),
            ),
        _ => centred.text_color(c.fg2).child(if shelf == Shelf::Starred {
            NOTHING_STARRED
        } else if indexing {
            INDEXING
        } else {
            NOTHING_ARCHIVED
        }),
    }
    .automation_target("page/empty")
}

impl Render for SessionsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.sb_theme().clone();
        self.lay_out(window, &theme, cx);
        self.prepare(window, cx);
        let library = self.library.read(cx);
        let sync = self.sync.read(cx);
        let shelf = library.shelf();
        let (glyph, title): (SbIcon, SharedString) = match shelf {
            Shelf::Starred => (SbIcon::Star, "Starred".into()),
            Shelf::Folder(folder) => (
                SbIcon::Folder,
                library
                    .folder(folder)
                    .map_or_else(SharedString::default, |it| it.name.clone().into()),
            ),
            Shelf::Trash => (SbIcon::Trash, "Trash".into()),
            Shelf::All | Shelf::Unsorted => (SbIcon::Layers, "All sessions".into()),
        };
        let page: Option<&SessionsPage> = library.sessions();
        let stats = page.map(|page| page.stats).unwrap_or_default();
        // While a first sync runs, All sessions says how far it is (DESIGN §4.14).
        let first_sync = (shelf == Shelf::All)
            .then(|| first_sync::running(sync))
            .flatten();
        let under = match &first_sync {
            Some(_) => Under::Line(
                format!(
                    "{} archived so far. Sessions appear here once their files are copied \
                     and indexed, and you can open each right away.",
                    format::count(stats.sessions as u64)
                )
                .into(),
            ),
            None => Under::Stats(stats),
        };
        let folder = match shelf {
            Shelf::Folder(folder) => Some(folder),
            _ => None,
        };
        let syncing_first = first_sync.is_some();
        let pinned = (!self.view.list).then_some(()).and_then(|()| {
            let scrolled = -self.scroll.base_handle().offset().y;
            let month = month_at(&self.rows, &self.sizes, &self.months, scrolled)?;
            Some(month_name(self.months.get(month)?))
        });
        let targeted = folder.is_some_and(|folder| {
            library
                .drag()
                .is_some_and(|drag| drag.targets(DropTarget::Folder(folder), library))
        });
        let editor = self.editor.as_ref().map(|(editor, _)| editor.clone());
        let asked = library
            .asked()
            .is_some()
            .then(|| self.ask_to_save(&title, window, &theme, cx));
        let header_row = div()
            .flex_none()
            .flex()
            .items_start()
            .gap(theme.spacing.lg)
            .child(
                div()
                    .id("page-header-row")
                    .flex_1()
                    .min_w_0()
                    .when_some(folder, |this, folder| {
                        // A double click on a folder's name renames it in its row.
                        this.on_click(move |event, _, cx| {
                            if event.click_count() > 1 {
                                let library = cx.services().library.clone();
                                library.update(cx, |library, cx| library.ask_rename(folder, cx));
                            }
                        })
                    })
                    .child(header(
                        glyph,
                        folder.is_some(),
                        title.clone(),
                        under,
                        &theme,
                    )),
            )
            .children(folder.map(|folder| {
                icon_button("folder-actions", theme.sizes.control_height, &theme)
                    .border_color(theme.colors.line)
                    .aria_label("Folder actions")
                    .on_click(move |_, window, cx| {
                        menu::open(
                            menus::folder_menu(folder, false),
                            window.mouse_position(),
                            FOLDER_MENU_WIDTH,
                            window,
                            cx,
                        );
                    })
                    .child(icon(SbIcon::Ellipsis, theme.sizes.icon, theme.colors.fg2))
                    .automation_target("page/folder-actions")
            }));
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(theme.spacing.xl)
            .pt(theme.sizes.page_top)
            .px(theme.sizes.page_side)
            .child(header_row)
            .map(|this| match (folder, page, editor) {
                (Some(_), Some(_), Some(editor)) => this.child(editor),
                (Some(folder), Some(page), None) => this
                    .child(paths_line(&page.paths, folder, library, sync, &theme))
                    .child(
                        div()
                            .flex_none()
                            .h(px(1.))
                            .mx(-theme.sizes.page_side)
                            .bg(theme.colors.line_subtle),
                    ),
                _ => this,
            })
            .when_some(first_sync, |this, first_sync| {
                this.child(first_sync::card(&first_sync, &theme, cx))
                    .when(library.folders().is_empty(), |this| {
                        this.child(first_sync::no_folders_banner(&theme, cx))
                    })
            })
            .when(page.is_some_and(|page| !page.sessions.is_empty()), |this| {
                this.child(self.filter_row(window, &theme, cx))
            })
            .map(|this| match page {
                None => this,
                // A first sync that has archived nothing yet says so in the header.
                Some(page) if page.sessions.is_empty() && syncing_first => this,
                Some(page) if page.sessions.is_empty() => {
                    this.child(empty(shelf, &title, sync.is_indexing(), targeted, &theme))
                }
                Some(_) if self.shown.is_empty() => this.child(no_match(&theme, cx)),
                Some(_) if self.view.list => this.child(self.list(folder, &theme, cx)),
                Some(_) => this.child(
                    // The list reaches the main area's edge, and its scrollbar sits in the
                    // page's side padding, clear of the cards ([`Self::render_rows`]).
                    div()
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .mr(-theme.sizes.page_side)
                        .child(
                            v_virtual_list(
                                cx.entity(),
                                "cards",
                                self.sizes.clone(),
                                Self::render_rows,
                            )
                            .track_scroll(&self.scroll),
                        )
                        // The overline of the month at the top, over the list: the cards
                        // under it take no clicks through it.
                        .children(pinned.map(|month| {
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .right(theme.sizes.page_side)
                                .h(theme.text.overline.line_height + theme.spacing.xl)
                                .bg(theme.colors.bg_app)
                                .occlude()
                                .child(overline(&month, &theme))
                                .automation_target("page/month")
                        }))
                        .child(Scrollbar::vertical(&self.scroll))
                        .automation_target("page/cards"),
                ),
            })
            .children(asked)
    }
}

impl SessionsView {
    /**
     * What each frame needs before it is laid out: the filter field, holding what the page
     * shown was filtered by; and the paths editor, when it was asked for or the folder shown
     * is empty.
     */
    fn prepare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let library = self.library.read(cx);
        let shelf = library.shelf();
        let filter = library.view().filter;
        let name = match shelf {
            Shelf::Folder(folder) => library.folder(folder).map(|it| it.name.clone()),
            Shelf::Starred => Some("Starred".to_string()),
            _ => None,
        };
        if self.filter.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx));
            let heard = cx.subscribe(&input, |this, input, event, cx| {
                if let InputEvent::Change = event {
                    let filter = input.read(cx).value().to_string();
                    this.library.update(cx, |library, cx| {
                        library.set_view(
                            PageView {
                                filter,
                                ..library.view()
                            },
                            cx,
                        );
                    });
                }
            });
            self.filter = Some((input, heard));
        }
        // Another page shows: the field holds what that page was filtered by.
        if let Some((input, _)) = &self.filter
            && self.filter_for != Some(shelf)
        {
            self.filter_for = Some(shelf);
            let placeholder = match name {
                Some(name) => format!("Filter in {name}"),
                None => "Filter all sessions".to_string(),
            };
            input.update(cx, |input, cx| {
                input.set_placeholder(placeholder, window, cx);
                input.set_value(filter, window, cx);
            });
        }
        if let Some(folder) = self.edit.take()
            && shelf == Shelf::Folder(folder)
            && self.editor.is_none()
        {
            self.open_editor(folder, false, window, cx);
        }
        let empty = self
            .library
            .read(cx)
            .sessions()
            .is_some_and(|page| page.sessions.is_empty() && page.paths.is_empty());
        if let Shelf::Folder(folder) = shelf
            && empty
            && self.editor.is_none()
        {
            self.open_editor(folder, true, window, cx);
        }
    }

    /**
     * The question leaving with changes to save asks (DESIGN §4.7): "Save the path changes to
     * fc?", with Don't save, Cancel and Save.
     */
    fn ask_to_save(
        &self,
        name: &str,
        window: &Window,
        theme: &SbTheme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let answer = |save: Option<bool>| {
            cx.listener(move |this: &mut Self, _: &ClickEvent, window, cx| {
                if let Some(save) = save
                    && let Some((editor, _)) = &this.editor
                {
                    editor.update(cx, |editor, cx| {
                        if save {
                            editor.save(window, cx);
                        } else {
                            editor.cancel(cx);
                        }
                    });
                }
                this.library
                    .update(cx, |library, cx| library.answer(save.is_some(), cx));
            })
        };
        let dialog = overlay::surface(theme)
            .w(theme.sizes.dialog_width)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(theme.spacing.md)
                    .p(theme.sizes.dialog_padding)
                    .child(
                        div()
                            .text_spec(&theme.text.sheet_title)
                            .child(format!("Save the path changes to {name}?")),
                    )
                    .child(
                        div()
                            .text_color(c.fg2)
                            .child("Leaving the page without saving drops them."),
                    ),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(theme.spacing.md)
                    .p(theme.sizes.dialog_padding)
                    .border_t_1()
                    .border_color(c.line_subtle)
                    .bg(c.s1)
                    .child(
                        button("ask-discard", ButtonKind::Plain, false, true, theme)
                            .on_click(answer(Some(false)))
                            .child("Don't save")
                            .automation_target("ask/discard"),
                    )
                    .child(
                        button("ask-cancel", ButtonKind::Plain, false, true, theme)
                            .on_click(answer(None))
                            .child("Cancel")
                            .automation_target("ask/cancel"),
                    )
                    .child(
                        button("ask-save", ButtonKind::Primary, false, true, theme)
                            .on_click(answer(Some(true)))
                            .child("Save")
                            .automation_target("ask/save"),
                    ),
            )
            .automation_target("ask");
        let library = self.library.clone();
        overlay::modal(dialog, Layer::Dialog, window, theme, move |_, cx| {
            library.update(cx, |library, cx| library.answer(false, cx));
        })
    }
}

/// "No sessions match these filters", and Clear filters (DESIGN §4.4, §4.14).
fn no_match(theme: &SbTheme, cx: &Context<SessionsView>) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(theme.spacing.lg)
        .text_color(theme.colors.fg2)
        .child("No sessions match these filters")
        .child(
            button("clear-filters-empty", ButtonKind::Plain, true, true, theme)
                .on_click(cx.listener(|this, _, window, cx| this.clear_filters(window, cx)))
                .child("Clear filters")
                .automation_target("page/no-match/clear"),
        )
        .automation_target("page/no-match")
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::theme::{Appearance, Material};

    fn months(sizes: &[usize]) -> Vec<Month> {
        let mut start = 0;
        sizes
            .iter()
            .enumerate()
            .map(|(at, size)| {
                let month = Month {
                    month: Some((2026, 12 - at as u32)),
                    sessions: start..start + size,
                };
                start += size;
                month
            })
            .collect()
    }

    #[test]
    fn a_row_has_as_many_cards_as_keep_each_between_its_least_and_most_width() {
        let theme = SbTheme::new(Appearance::Dark, Material::Opaque);
        // DESIGN §2: 1440 wide with the sidebar docked leaves 1144 for the cards: four of 277.
        assert_eq!(columns(px(1144.), &theme), 4);
        // Three cards of 320 and two gaps of 12; any wider and a fourth fits.
        assert_eq!(columns(px(984.), &theme), 3);
        assert_eq!(columns(px(985.), &theme), 4);
        // 1040 with the sidebar docked, and 960 with it collapsed: three.
        assert_eq!(columns(px(744.), &theme), 3);
        assert_eq!(columns(px(912.), &theme), 3);
        assert_eq!(columns(px(652.), &theme), 2);
        // A very wide window: seven of 313, not four of 563.
        assert_eq!(columns(px(2264.), &theme), 7);
        assert_eq!(columns(px(300.), &theme), 2);
    }

    #[test]
    fn the_calls_show_on_every_card_of_a_page_or_on_none() {
        let theme = SbTheme::new(Appearance::Dark, Material::Opaque);
        let line = |locations: &[f32]| LinePieces {
            locations: locations.iter().copied().map(px).collect(),
            day: px(40.),
            calls: Some(px(20.)),
            ..LinePieces::default()
        };
        // The mark (12), 12 of a model's name, a 70 tag, a 40 day, the glyph (12), 4 and a
        // 20 count, and four gaps of 6.
        assert!(line_fits(px(194.), &line(&[70.]), &theme));
        assert!(!line_fits(px(193.), &line(&[70.]), &theme));
        // The widest location decides for every card, whichever location a card is from...
        assert!(!line_fits(px(193.), &line(&[40., 70.]), &theme));
        assert!(!line_fits(px(193.), &line(&[70., 40.]), &theme));
        // ...at its cap of 80.
        assert!(line_fits(px(204.), &line(&[200., 70.]), &theme));
        assert!(!line_fits(px(203.), &line(&[200., 70.]), &theme));
        // Every piece a line can hold is counted: a subfolder's tag at its cap of 96, the
        // held-back chip, and the hand glyph, each with its gap.
        let full = LinePieces {
            subfolders: vec![px(50.), px(120.)],
            held: Some(px(55.)),
            by_hand: true,
            ..line(&[70., 200.])
        };
        assert!(line_fits(px(385.), &full, &theme));
        assert!(!line_fits(px(384.), &full, &theme));
        // Without counts, the glyph, the count and their gap leave the line: whether the tags
        // may give way past their caps is decided so.
        let uncounted = LinePieces {
            calls: None,
            ..line(&[70.])
        };
        assert!(line_fits(px(152.), &uncounted, &theme));
        assert!(!line_fits(px(151.), &uncounted, &theme));
    }

    #[test]
    fn the_widest_day_has_two_digits_and_the_oldest_sessions_year() {
        let at = |year, month, day| {
            chrono::NaiveDate::from_ymd_opt(year, month, day)
                .and_then(|date| date.and_hms_opt(12, 0, 0))
                .map(|time| time.and_utc().timestamp_millis())
                .expect("a day at noon")
        };
        let now = at(2026, 11, 16);
        let stats = |oldest| Stats {
            sessions: 2,
            calls: 0,
            span: Some((oldest, now)),
            locations: 1,
        };
        assert_eq!(widest_day(&stats(at(2026, 11, 2)), now), "28 Nov");
        assert_eq!(widest_day(&stats(at(2025, 12, 3)), now), "28 Dec 2025");
        assert_eq!(widest_day(&Stats::default(), now), "28");
    }

    #[test]
    fn each_month_is_an_overline_and_its_cards_in_rows() {
        assert_eq!(
            rows(&months(&[5, 1, 4]), 4),
            [
                Row::Month(0),
                Row::Cards(0..4),
                Row::Cards(4..5),
                Row::Month(1),
                Row::Cards(5..6),
                Row::Month(2),
                Row::Cards(6..10),
            ]
        );
        assert_eq!(rows(&[], 4), []);
    }

    #[test]
    fn rows_are_as_high_as_design_draws_them() {
        let theme = SbTheme::new(Appearance::Dark, Material::Opaque);
        // 12 + 18 + 8 + 36 + 8 + 16 + 12 inside a 1px border.
        assert_eq!(card_height(&theme), px(112.));
        let heights: Vec<Pixels> = row_sizes(&rows(&months(&[5, 1]), 4), &theme)
            .into_iter()
            .map(|size| size.height)
            .collect();
        assert_eq!(
            heights,
            [
                // An overline and the 16 under it.
                px(30.),
                // Cards and the 12 between rows.
                px(124.),
                // A month's last cards and the 16 before the next overline.
                px(128.),
                px(30.),
                // The last cards and the page's 24.
                px(136.),
            ]
        );
    }

    #[test]
    fn the_overline_of_the_month_at_the_top_stays_while_its_cards_scroll() {
        let theme = SbTheme::new(Appearance::Dark, Material::Opaque);
        let months = months(&[5, 1]);
        let rows = rows(&months, 4);
        let sizes = row_sizes(&rows, &theme);
        let at = |scrolled: f32| month_at(&rows, &sizes, &months, px(scrolled));
        // At the top the first overline is where it belongs.
        assert_eq!(at(0.), None);
        // The first month: its overline (30), its two rows of cards (124 and 128).
        assert_eq!(at(1.), Some(0));
        assert_eq!(at(281.), Some(0));
        // Then the second month's overline reaches the top.
        assert_eq!(at(282.), Some(1));
        assert_eq!(at(400.), Some(1));
        // Past the end there is nothing at the top.
        assert_eq!(at(5000.), None);
    }

    #[test]
    fn the_arrows_move_in_reading_order_and_keep_the_column() {
        let rows = rows(&months(&[6, 3, 1]), 4);
        // Left and right run through the months and stop at the ends.
        assert_eq!(moved(&rows, 3, 1, 0), Some(4));
        assert_eq!(moved(&rows, 6, -1, 0), Some(5));
        assert_eq!(moved(&rows, 0, -1, 0), None);
        assert_eq!(moved(&rows, 9, 1, 0), None);
        // Down keeps the column, or takes the last card of a shorter row.
        assert_eq!(moved(&rows, 1, 0, 1), Some(5));
        assert_eq!(moved(&rows, 3, 0, 1), Some(5));
        assert_eq!(moved(&rows, 5, 0, 1), Some(7));
        assert_eq!(moved(&rows, 8, 0, 1), Some(9));
        assert_eq!(moved(&rows, 9, 0, 1), None);
        // Up crosses a month's overline.
        assert_eq!(moved(&rows, 7, 0, -1), Some(5));
        assert_eq!(moved(&rows, 9, 0, -1), Some(6));
        assert_eq!(moved(&rows, 2, 0, -1), None);
    }
}
