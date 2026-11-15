//! The page of All sessions, Starred or a folder (PLAN §11 `folder_page`; DESIGN §4.4, §4.5):
//! the header, a folder's attached paths as chips, and the sessions as cards grouped by month.
//!
//! **The cards are a virtual list.** Its items are the month overlines and the rows of cards
//! ([`rows`]), each of a height known without drawing it, so a page of tens of thousands of
//! sessions costs what the visible rows do. The rows are laid out again only when the page's
//! sessions or the number of columns change.
//!
//! **Keyboard** (DESIGN §4.4): the arrows move the selection between cards in reading order,
//! `Enter` opens the selected session and `S` stars it.
//!
//! Filters, sort, the list view and the hover bar's other actions come with later tasks.

use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Render, Role, ScrollStrategy, SharedString, Size,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, point,
    prelude::FluentBuilder as _, px, size,
};
use gpui_component::scroll::Scrollbar;
use gpui_component::{VirtualListScrollHandle, v_virtual_list};
use sb_core::catalogue::{Filed, Session, SessionId, Shelf};

use super::first_sync;
use super::header::{Under, header};
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{
    Tone, icon, location_tag, overline, tag, tag_width, text_width, warn_chip, warn_chip_width,
};
use crate::format;
use crate::icons::SbIcon;
use crate::keymap;
use crate::services::{
    ActiveServices as _, LibraryService, Month, PathChip, SessionsPage, SyncService,
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

/// The narrowest a model's name is drawn, cut short; with less room it is left out.
const MODEL_MIN: Pixels = px(40.);

/// What of a card's last line shows where the line is short (DESIGN §4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LastLine {
    model: bool,
    calls: bool,
}

/**
 * What a card's last line has room for. The agent's mark, the location's tag, a subfolder's
 * tag and the day always show, whole, and leave `room`. The model's name is `model` wide
 * and the tool calls `calls`. When they don't both fit, the name is cut short, down to
 * [`MODEL_MIN`]; then the calls are left out; then the name.
 */
fn last_line(room: Pixels, model: Pixels, calls: Pixels) -> LastLine {
    let least = model.min(MODEL_MIN);
    LastLine {
        model: least <= room,
        calls: least + calls <= room,
    }
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

/// How many cards fit a row `width` wide (DESIGN §2): four, then three, then two, each at
/// least the card's minimum width.
pub fn columns(width: Pixels, theme: &SbTheme) -> usize {
    (2..=4)
        .rev()
        .find(|columns| {
            let gaps = theme.spacing.lg * (*columns as f32 - 1.);
            (width - gaps) / *columns as f32 >= theme.sizes.card_min_width
        })
        .unwrap_or(2)
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
    /// How wide a card is, as far as the window's width says.
    card_width: Pixels,
    /// The card under the pointer.
    hovered: Option<SessionId>,
    /// The rows are of what the library held before it last changed.
    stale: bool,
    _observers: [Subscription; 2],
}

impl SessionsView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let services = cx.services();
        let (library, sync) = (services.library.clone(), services.sync.clone());
        Self {
            _observers: [
                cx.observe(&library, |this, _, cx| {
                    this.stale = true;
                    cx.notify();
                }),
                cx.observe(&sync, |_, _, cx| cx.notify()),
            ],
            library,
            sync,
            scroll: VirtualListScrollHandle::new(),
            shelf: Shelf::All,
            rows: Vec::new(),
            sizes: Rc::default(),
            columns: 0,
            card_width: px(0.),
            hovered: None,
            stale: true,
        }
    }

    /// Lays the rows out again when the sessions or the width changed. Another page than
    /// the last starts at its top.
    fn lay_out(&mut self, window: &Window, theme: &SbTheme, cx: &App) {
        let shelf = self.library.read(cx).shelf();
        if shelf != self.shelf {
            self.shelf = shelf;
            self.scroll.base_handle().set_offset(point(px(0.), px(0.)));
        }
        let viewport = window.viewport_size().width;
        let sidebar = if viewport >= theme.sizes.sidebar_collapse_width {
            theme.sizes.sidebar_width
        } else {
            px(0.)
        };
        let room = viewport - sidebar - theme.sizes.page_side * 2.;
        let columns = columns(room, theme);
        self.card_width = (room - theme.spacing.lg * (columns as f32 - 1.)) / columns as f32;
        if !self.stale && columns == self.columns {
            return;
        }
        self.stale = false;
        self.columns = columns;
        self.rows = self
            .library
            .read(cx)
            .sessions()
            .map_or_else(Vec::new, |page| rows(&page.months, columns));
        self.sizes = Rc::new(row_sizes(&self.rows, theme));
    }

    /// Moves the selection (see [`moved`]); with nothing selected, selects the first session.
    pub fn step(&mut self, by: isize, lines: isize, cx: &mut Context<Self>) {
        let selected = self.library.read(cx).selected_index();
        let Some(to) = selected.map_or(Some(0), |index| moved(&self.rows, index, by, lines)) else {
            return;
        };
        if let Some(row) = self
            .rows
            .iter()
            .position(|row| matches!(row, Row::Cards(sessions) if sessions.contains(&to)))
        {
            self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        }
        self.library
            .update(cx, |library, cx| library.select_index(to, cx));
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

    /// Stars the selected session, or takes its star away.
    pub fn star_selected(&mut self, cx: &mut Context<Self>) {
        if let Some((id, at)) = self.selected(cx) {
            self.library
                .update(cx, |library, cx| library.toggle_star(&id, at, cx));
        }
    }

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
        range
            .filter_map(|at| self.rows.get(at))
            .map(|row| match row {
                Row::Month(month) => div()
                    .child(overline(&month_name(&page.months[*month]), &theme))
                    .into_any_element(),
                Row::Cards(sessions) => div()
                    .w_full()
                    .flex()
                    .gap(theme.spacing.lg)
                    .children(sessions.clone().map(|index| {
                        self.card(index, &page.sessions[index], now, &theme, window, cx)
                    }))
                    // A short row's cards stay as wide as a full one's: what fills the row
                    // takes a card's padding and border too.
                    .children((sessions.len()..self.columns).map(|_| {
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

    /// A session's card (DESIGN §4.4): its title, the start of its first prompt on two lines,
    /// then its agent's mark and model, where it is from, and when it ended and how many
    /// tool calls it made.
    fn card(
        &self,
        index: usize,
        session: &Session,
        now: i64,
        theme: &SbTheme,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let library = self.library.read(cx);
        let selected = library.selected_index() == Some(index);
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
        let subfolder = match (session.filed, library.shelf()) {
            (Filed::ByRule(folder) | Filed::ByHand(folder), Shelf::Folder(shown))
                if folder != shown =>
            {
                library.folder(folder)
            }
            _ => None,
        };
        let location = self.sync.read(cx).location(&session.id.location);
        let by_hand = matches!(session.filed, Filed::ByHand(_));
        let calls = format::plural(session.all.calls as usize, "call");
        // The day always shows; a session that recorded no time has its calls in its place.
        let (day, calls) = match session.ended.or(session.started) {
            Some(ended) => (format::day(ended, now), format!(" · {calls}")),
            None => (calls, String::new()),
        };

        /* What always shows on the last line, and the gaps between its pieces, leave this
        much for the model's name and the tool calls ([`last_line`]). */
        let gap = theme.spacing.sm;
        let always = [
            Some(theme.sizes.icon_xs),
            Some(text_width(&day, &theme.text.count, window)),
            location.map(|location| tag_width(&location.name, theme, window)),
            subfolder.map(|folder| {
                tag_width(&folder.name, theme, window).min(theme.sizes.tag_max_width)
            }),
            session
                .held_back
                .then(|| warn_chip_width(HELD_BACK, theme, window)),
            by_hand.then_some(theme.sizes.icon_xs),
        ];
        let pieces = always.iter().flatten().count();
        let room = self.card_width
            - CARD_PADDING * 2.
            - px(2.)
            - always
                .iter()
                .flatten()
                .fold(px(0.), |sum, width| sum + *width)
            - gap * (pieces - 1) as f32;
        let shows = last_line(
            room,
            text_width(&model, &theme.text.small, window) + gap,
            text_width(&calls, &theme.text.count, window),
        );

        let id = session.id.clone();
        let star = div()
            .id(("star", index))
            .role(Role::Button)
            .aria_label(format!(
                "{} ({})",
                if session.starred { "Unstar" } else { "Star" },
                keymap::STAR.label()
            ))
            .flex()
            .items_center()
            .justify_center()
            .size(theme.sizes.hover_button)
            .rounded(theme.radii.control)
            .cursor_pointer()
            .hover(|style| style.bg(c.s3))
            .on_click(cx.listener({
                let id = id.clone();
                move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.library
                        .update(cx, |library, cx| library.toggle_star(&id, index, cx));
                }
            }))
            .child(icon(
                SbIcon::Star,
                theme.sizes.icon_sm,
                if session.starred { c.yellow } else { c.fg2 },
            ))
            .automation_target_with(move || format!("card/{index}/star").into());
        div()
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
                if selected {
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
            .on_click(cx.listener({
                let id = id.clone();
                move |this, event: &ClickEvent, _, cx| {
                    this.library.update(cx, |library, cx| {
                        if event.click_count() > 1 {
                            library.open_session_at(&id, index, cx);
                        } else {
                            library.select_session(&id, index, cx);
                        }
                    });
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.sm)
                    // The title ends short of the hover bar while the bar shows.
                    .when(barred, |this| {
                        this.pr(theme.sizes.hover_button + theme.spacing.sm)
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
                                    library.open_session_at(&id, index, cx);
                                });
                            }))
                            .child(title)
                            .automation_target_with(move || format!("card/{index}/title").into()),
                    )
                    // The bar's own star says so while it shows.
                    .when(session.starred && !barred, |this| {
                        this.child(icon(SbIcon::Star, theme.sizes.icon_xs, c.yellow))
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
                    .gap(gap)
                    .h(theme.sizes.tag_small_height)
                    .text_color(c.fg3)
                    .text_spec(&theme.text.small)
                    .whitespace_nowrap()
                    .child(agents::mark(&session.id.provider, theme.sizes.icon_xs, cx))
                    // Only the model's name gives way: everything else keeps its width.
                    .when(shows.model, |this| {
                        this.child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(model),
                        )
                    })
                    .children(location.map(|location| {
                        location_tag(location.name.clone(), location.kind, true, theme)
                    }))
                    .children(subfolder.map(|folder| {
                        tag(folder.name.clone(), Tone::Neutral, true, theme)
                            .max_w(theme.sizes.tag_max_width)
                    }))
                    .when(session.held_back, |this| {
                        this.child(warn_chip(HELD_BACK, theme))
                    })
                    .child(
                        div()
                            .flex_none()
                            .ml_auto()
                            .text_spec(&theme.text.count)
                            .child(if shows.calls {
                                format!("{day}{calls}")
                            } else {
                                day
                            }),
                    )
                    .when(by_hand, |this| {
                        this.child(icon(SbIcon::Hand, theme.sizes.icon_xs, c.fg3))
                    }),
            )
            .child(
                // The hover bar (DESIGN §4.4), with the one action there is so far.
                div()
                    .absolute()
                    .top(theme.spacing.md)
                    .right(theme.spacing.md)
                    .p(px(2.))
                    .rounded(theme.radii.input)
                    .bg(c.s2)
                    .border_1()
                    .border_color(c.line)
                    .shadow(theme.shadows.card.clone())
                    /* Under the pointer it shows as soon as it is drawn; `hovered` follows
                    with the pointer's next move. */
                    .when(!barred, |this| {
                        this.opacity(0.)
                            .group_hover("card", |style| style.opacity(1.))
                    })
                    .child(star),
            )
            .automation_target_with(move || format!("card/{index}").into())
    }
}

/// A folder's attached paths as read-only chips (DESIGN §4.4), or what the line says while
/// it has none.
fn paths_line(paths: &[PathChip], folder: &str, sync: &SyncService, theme: &SbTheme) -> AnyElement {
    let c = &theme.colors;
    div()
        .flex_none()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(theme.spacing.md)
        .child(icon(SbIcon::Link, theme.sizes.icon_sm, c.fg3))
        .child(
            div()
                .mr(theme.spacing.xs)
                .text_color(c.fg2)
                .text_spec(&theme.text.control_small)
                .child("Project paths"),
        )
        .when(paths.is_empty(), |this| {
            this.child(
                div()
                    .text_color(c.fg3)
                    .text_spec(&theme.text.secondary)
                    .child(format!(
                        "None yet. Sessions started under a path you attach here file \
                         themselves into {folder}."
                    )),
            )
        })
        .children(paths.iter().map(|path| {
            let location = path
                .location
                .as_ref()
                .and_then(|location| sync.location(location));
            // A long path gives way in its middle; its location and its count stay whole.
            div()
                .min_w_0()
                .max_w_full()
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
                        location_tag(location.name.clone(), location.kind, false, theme)
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
                )
        }))
        .automation_target("page/paths")
}

/// The empty folder's drop area (`design/project/sb.css`, `.empty`): the gap between its
/// pieces, its tile and the width of its text.
const EMPTY_GAP: Pixels = px(10.);
const EMPTY_TILE: Pixels = px(48.);
const EMPTY_TEXT_WIDTH: Pixels = px(460.);

/// What a page says in place of its cards while it has none.
fn empty(shelf: Shelf, folder: &str, indexing: bool, theme: &SbTheme) -> AnyElement {
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
        Shelf::Folder(_) => centred
            .mb(theme.spacing.xxl)
            .rounded(theme.radii.sheet)
            .border_1()
            .border_dashed()
            .border_color(c.line_strong)
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
        let is_folder = matches!(shelf, Shelf::Folder(_));
        let syncing_first = first_sync.is_some();
        let pinned = page.and_then(|page| {
            let scrolled = -self.scroll.base_handle().offset().y;
            let month = month_at(&self.rows, &self.sizes, &page.months, scrolled)?;
            Some(month_name(page.months.get(month)?))
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(theme.spacing.xl)
            .pt(theme.sizes.page_top)
            .px(theme.sizes.page_side)
            .child(header(glyph, is_folder, title.clone(), under, &theme))
            .when_some(page.filter(|_| is_folder), |this, page| {
                this.child(paths_line(&page.paths, &title, sync, &theme))
                    .child(
                        div()
                            .flex_none()
                            .h(px(1.))
                            .mx(-theme.sizes.page_side)
                            .bg(theme.colors.line_subtle),
                    )
            })
            .when_some(first_sync, |this, first_sync| {
                this.child(first_sync::card(&first_sync, &theme, cx))
                    .when(library.folders().is_empty(), |this| {
                        this.child(first_sync::no_folders_banner(&theme, cx))
                    })
            })
            .map(|this| match page {
                None => this,
                // A first sync that has archived nothing yet says so in the header.
                Some(page) if page.sessions.is_empty() && syncing_first => this,
                Some(page) if page.sessions.is_empty() => {
                    this.child(empty(shelf, &title, sync.is_indexing(), &theme))
                }
                Some(_) => this.child(
                    // The scrollbar is in the page's side padding, clear of the cards.
                    div()
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .mr(-theme.sizes.page_side)
                        .child(
                            div().size_full().pr(theme.sizes.page_side).child(
                                v_virtual_list(
                                    cx.entity(),
                                    "cards",
                                    self.sizes.clone(),
                                    Self::render_rows,
                                )
                                .track_scroll(&self.scroll),
                            ),
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
    }
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
    fn cards_drop_from_four_columns_to_three_then_two() {
        let theme = SbTheme::new(Appearance::Dark, Material::Opaque);
        // DESIGN §2: 1440 wide with the sidebar docked leaves 1144 for the cards.
        assert_eq!(columns(px(1144.), &theme), 4);
        // Four cards of 200 and three gaps of 12.
        assert_eq!(columns(px(836.), &theme), 4);
        assert_eq!(columns(px(835.), &theme), 3);
        assert_eq!(columns(px(624.), &theme), 3);
        assert_eq!(columns(px(623.), &theme), 2);
        assert_eq!(columns(px(300.), &theme), 2);
    }

    #[test]
    fn a_short_last_line_cuts_the_model_then_drops_the_calls_then_the_model() {
        let line = |room: f32| last_line(px(room), px(90.), px(50.));
        let (both, model, neither) = (
            LastLine {
                model: true,
                calls: true,
            },
            LastLine {
                model: true,
                calls: false,
            },
            LastLine {
                model: false,
                calls: false,
            },
        );
        assert_eq!(line(140.), both);
        // The name is cut short for as long as 40 of it and the calls fit.
        assert_eq!(line(139.), both);
        assert_eq!(line(90.), both);
        assert_eq!(line(89.), model);
        assert_eq!(line(40.), model);
        assert_eq!(line(39.), neither);
        // A name shorter than that is never cut: it shows whole or not at all.
        assert_eq!(last_line(px(30.), px(24.), px(50.)), model);
        assert_eq!(last_line(px(20.), px(24.), px(50.)), neither);
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
