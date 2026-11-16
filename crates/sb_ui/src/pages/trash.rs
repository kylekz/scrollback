//! Trash (DESIGN §4.8, `E-Trash`): the sessions the owner moved there, the one moved last
//! first, in a table of what each was, where it was filed, its location, when it was moved and
//! how much it holds. **Restore** puts a session back where it was filed, or where the rules
//! file it when its folder is gone (decision 10), with a toast that undoes it; **Delete for
//! good…** asks first, in the shell's dialog ([`crate::services::LibraryService::for_good`]).
//!
//! **Selection** works as on a folder's page: a click selects a row, `Ctrl` adds or takes one
//! away, `Shift` selects a run, `Ctrl A` everything, and a row's Restore restores the
//! selection when the row is in it. Delete for good is one session's at a time: its dialog
//! names what it removes.
//!
//! **Keyboard:** `↑` and `↓` move the selection, `Enter` opens the selected session. No key
//! deletes anything here: `Delete` is not bound on this page, and the library moves nothing
//! from Trash to Trash.
//!
//! The table is a virtual list: Trash is never emptied on its own, so it can hold thousands.

use std::collections::HashSet;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, FocusHandle, InteractiveElement as _,
    IntoElement, Modifiers, ParentElement as _, Pixels, Render, ScrollStrategy, SharedString, Size,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px, size,
};
use gpui_component::scroll::Scrollbar;
use gpui_component::{VirtualListScrollHandle, v_virtual_list};
use sb_core::catalogue::{Filed, Session, SessionId};

use super::header::{Under, header};
use crate::automation::AutomationTargetExt as _;
use crate::components::{ButtonKind, button, focus_ring, icon, location_tag};
use crate::format;
use crate::hostile;
use crate::icons::SbIcon;
use crate::services::library::title_of;
use crate::services::{ActiveServices as _, LibraryService, SyncService};
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _};

/// What the page says (DESIGN §4.8, §4.14).
const ABOUT: &str = "They are hidden everywhere else but still archived, and syncs keep adding \
                     to them. Restore puts a session back in its folder. Trash is never emptied \
                     on its own.";
const NOTE: &str = "Deleting for good removes a session from this archive only. Nothing is ever \
                    deleted at a location.";
const EMPTY: &str = "Trash is empty.";

/// The table's measures (`E-Trash`): its header and rows, and the columns after Session.
const HEADER_HEIGHT: Pixels = px(30.);
const ROW_HEIGHT: Pixels = px(52.);
const WAS_IN_WIDTH: Pixels = px(120.);
const LOCATION_WIDTH: Pixels = px(104.);
const MOVED_WIDTH: Pixels = px(110.);
const SIZE_WIDTH: Pixels = px(64.);
const ACTIONS_WIDTH: Pixels = px(236.);
const COLUMN_GAP: Pixels = px(12.);
const ROW_PADDING: Pixels = px(12.);
/// The gap between the header, the table and the note.
const PAGE_GAP: Pixels = px(14.);

/// The order the sessions of a page of Trash show in: the one moved there last first, as
/// places among the page's sessions.
pub fn order(sessions: &[Session]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..sessions.len()).collect();
    order.sort_by_key(|at| std::cmp::Reverse(sessions[*at].trashed));
    order
}

pub struct TrashView {
    library: Entity<LibraryService>,
    sync: Entity<SyncService>,
    scroll: VirtualListScrollHandle,
    /// The page's sessions in the order they show, and each row's size.
    order: Vec<usize>,
    sizes: Rc<Vec<Size<Pixels>>>,
    /// Where the keyboard rests in the main area: while it is there, the selected row has the
    /// keyboard's ring.
    focus: FocusHandle,
    _observers: [Subscription; 2],
}

impl TrashView {
    pub fn new(focus: FocusHandle, cx: &mut Context<Self>) -> Self {
        let services = cx.services();
        let (library, sync) = (services.library.clone(), services.sync.clone());
        Self {
            _observers: [
                cx.observe(&library, |this, _, cx| {
                    this.lay_out(cx);
                    cx.notify();
                }),
                cx.observe(&sync, |_, _, cx| cx.notify()),
            ],
            library,
            sync,
            scroll: VirtualListScrollHandle::new(),
            order: Vec::new(),
            sizes: Rc::default(),
            focus,
        }
    }

    /// Orders the rows again from what the library holds.
    fn lay_out(&mut self, cx: &App) {
        let order = (self.library.read(cx).sessions())
            .map(|page| order(&page.sessions))
            .unwrap_or_default();
        if order != self.order {
            self.sizes = Rc::new(vec![size(px(0.), ROW_HEIGHT); order.len()]);
            self.order = order;
        }
    }

    /// The session that shows at `row`, with its place among the page's sessions.
    fn at<'a>(&self, row: usize, cx: &'a App) -> Option<(usize, &'a Session)> {
        let index = *self.order.get(row)?;
        Some((
            index,
            self.library.read(cx).sessions()?.sessions.get(index)?,
        ))
    }

    /// The row the selected session shows at.
    fn selected_row(&self, cx: &App) -> Option<usize> {
        let index = self.library.read(cx).selected_index()?;
        self.order.iter().position(|at| *at == index)
    }

    /// Moves the selection a row up or down (`↑`, `↓`); the first press selects the first row.
    pub fn step(&mut self, by: isize, cx: &mut Context<Self>) {
        let row = match self.selected_row(cx) {
            Some(row) => row
                .saturating_add_signed(by)
                .min(self.order.len().saturating_sub(1)),
            None => 0,
        };
        if let Some((index, _)) = self.at(row, cx) {
            self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
            self.library
                .update(cx, |library, cx| library.select_index(index, cx));
        }
    }

    /// Opens the selected session (`Enter`).
    pub fn open_selected(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row(cx) else {
            return;
        };
        self.open(row, cx);
    }

    fn open(&mut self, row: usize, cx: &mut Context<Self>) {
        if let Some((index, session)) = self.at(row, cx) {
            let id = session.id.clone();
            self.library
                .update(cx, |library, cx| library.open_session_at(&id, index, cx));
        }
    }

    /// Selects every session in Trash (`Ctrl A`).
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        let Some(page) = self.library.read(cx).sessions() else {
            return;
        };
        let chosen: HashSet<SessionId> = page.sessions.iter().map(|it| it.id.clone()).collect();
        let first = self
            .at(0, cx)
            .map(|(index, it)| (it.id.clone(), Some(index)));
        if let Some(first) = first {
            self.library
                .update(cx, |library, cx| library.choose(chosen, first, true, cx));
        }
    }

    /**
     * A click on the row at `row`: selects it, with `Ctrl` adds it or takes it away, with
     * `Shift` selects the run from where the last plain or `Ctrl` click was; a double click
     * opens it.
     */
    fn click(
        &mut self,
        row: usize,
        (modifiers, count): (Modifiers, usize),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The page takes the keyboard, so that its keys act on what was clicked.
        self.focus.focus(window, cx);
        if count > 1 {
            return self.open(row, cx);
        }
        let Some((index, session)) = self.at(row, cx) else {
            return;
        };
        let id = session.id.clone();
        let library = self.library.read(cx);
        let secondary = modifiers.secondary(cx);
        let anchor = (library.anchor())
            .and_then(|anchor| (0..self.order.len()).find(|at| self.id(*at, cx) == Some(anchor)));
        let chosen: Option<(HashSet<SessionId>, bool)> = match anchor {
            Some(from) if modifiers.shift => {
                let mut chosen: HashSet<SessionId> = if secondary {
                    library.selection().into_iter().collect()
                } else {
                    HashSet::new()
                };
                chosen.extend(
                    (from.min(row)..=from.max(row)).filter_map(|at| self.id(at, cx).cloned()),
                );
                Some((chosen, false))
            }
            _ if secondary && library.is_selected(&id) => {
                self.library
                    .update(cx, |library, cx| library.unchoose(&id, cx));
                return;
            }
            _ if secondary => {
                let mut chosen: HashSet<SessionId> = library.selection().into_iter().collect();
                chosen.insert(id.clone());
                Some((chosen, true))
            }
            _ => None,
        };
        self.library.update(cx, |library, cx| match chosen {
            Some((chosen, anchored)) => library.choose(chosen, (id, Some(index)), anchored, cx),
            None => library.select_index(index, cx),
        });
    }

    fn id<'a>(&self, row: usize, cx: &'a App) -> Option<&'a SessionId> {
        self.at(row, cx).map(|(_, session)| &session.id)
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = cx.sb_theme().clone();
        let keyboard = self.focus.is_focused(window) && window.last_input_was_keyboard();
        let now = format::now();
        range
            .filter_map(|row| {
                let (index, session) = self.at(row, cx)?;
                let last = row + 1 == self.order.len();
                Some(self.row(row, index, session, (last, keyboard, now), &theme, cx))
            })
            .collect()
    }

    /// A session's row (`E-Trash`): its title over its prompt's start, the folder it was in,
    /// its location, when it was moved here, its size, Restore and Delete for good….
    fn row(
        &self,
        row: usize,
        index: usize,
        session: &Session,
        (last, keyboard, now): (bool, bool, i64),
        theme: &SbTheme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let library = self.library.read(cx);
        let selected = library.is_selected(&session.id);
        let ringed = keyboard && library.selected_index() == Some(index);
        let was_in = match session.filed {
            Filed::ByRule(folder) | Filed::ByHand(folder) => library
                .folder(folder)
                .map_or_else(|| "Unsorted".to_string(), |it| hostile::label(&it.name)),
            Filed::Unsorted => "Unsorted".to_string(),
        };
        let size = library
            .sessions()
            .and_then(|page| page.sizes.get(&session.id))
            .copied()
            .unwrap_or(0);
        let location = self.sync.read(cx).location(&session.id.location);
        let id = session.id.clone();
        div()
            .id(("trash-row", row))
            .debug_selector(move || format!("trash-row-{row}"))
            .w_full()
            .flex()
            .items_center()
            .gap(COLUMN_GAP)
            .h(ROW_HEIGHT)
            .px(ROW_PADDING)
            .when(!last, |this| this.border_b_1().border_color(c.line_subtle))
            .whitespace_nowrap()
            .cursor_pointer()
            .map(|this| {
                if selected {
                    this.bg(c.s3)
                        .when(ringed, |this| this.shadow(focus_ring(true, theme)))
                } else {
                    this.hover(|style| style.bg(c.s1))
                }
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.click(row, (event.modifiers(), event.click_count()), window, cx);
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .overflow_hidden()
                            .text_ellipsis()
                            .font_weight(theme.text.control.weight)
                            .child(title_of(session))
                            .automation_target_with(move || {
                                format!("trash/row/{row}/title").into()
                            }),
                    )
                    .child(
                        div()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_color(c.fg3)
                            .text_spec(&theme.text.secondary)
                            .child(
                                (session.first_prompt.as_deref())
                                    .map(hostile::label)
                                    .unwrap_or_default(),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .w(WAS_IN_WIDTH)
                    .flex()
                    .items_center()
                    .gap(theme.spacing.sm)
                    .text_color(c.fg2)
                    .text_spec(&theme.text.secondary)
                    .child(icon(SbIcon::Folder, theme.sizes.icon_xs, c.fg3))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(was_in),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .w(LOCATION_WIDTH)
                    .flex()
                    .overflow_hidden()
                    .children(location.map(|location| {
                        location_tag(location.name.clone(), &location.kind, false, theme)
                            .flex_shrink(1.)
                            .min_w_0()
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .w(MOVED_WIDTH)
                    .text_color(c.fg3)
                    .text_spec(&theme.text.number)
                    .child(
                        session
                            .trashed
                            .map(|at| format::day(at, now))
                            .unwrap_or_default(),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .w(SIZE_WIDTH)
                    .flex()
                    .justify_end()
                    .text_color(c.fg3)
                    .text_spec(&theme.text.number)
                    .child(format::fine_size(size)),
            )
            .child(
                div()
                    .flex_none()
                    .w(ACTIONS_WIDTH)
                    .flex()
                    .justify_end()
                    .gap(theme.spacing.md)
                    .child(
                        button(("trash-restore", row), ButtonKind::Plain, true, true, theme)
                            .debug_selector(move || format!("trash-restore-{row}"))
                            .on_click(cx.listener({
                                let id = id.clone();
                                move |this, _, _, cx| {
                                    // The button's alone: the row neither selects nor opens.
                                    cx.stop_propagation();
                                    this.library.update(cx, |library, cx| {
                                        let ids = library.acted_on(&id);
                                        library.restore(ids, cx);
                                    });
                                }
                            }))
                            .child(icon(SbIcon::RotateCcw, theme.sizes.icon_xs, c.fg2))
                            .child("Restore")
                            .automation_target_with(move || {
                                format!("trash/row/{row}/restore").into()
                            }),
                    )
                    .child(
                        button(("trash-delete", row), ButtonKind::Danger, true, true, theme)
                            .debug_selector(move || format!("trash-delete-{row}"))
                            .on_click(cx.listener({
                                let session = session.clone();
                                move |this, _, _, cx| {
                                    // The button's alone: the row neither selects nor opens.
                                    cx.stop_propagation();
                                    let session = session.clone();
                                    this.library.update(cx, |library, cx| {
                                        library.ask_delete_for_good(session, cx);
                                    });
                                }
                            }))
                            .child("Delete for good…")
                            .automation_target_with(move || {
                                format!("trash/row/{row}/delete").into()
                            }),
                    ),
            )
            .automation_target_with(move || format!("trash/row/{row}").into())
            .into_any_element()
    }

    /// The table's header row (`E-Trash`): 30 high on `s1`, 11px 500 `fg3`.
    fn table_header(theme: &SbTheme) -> impl IntoElement {
        let c = &theme.colors;
        let cell = |width: Pixels, label: &'static str| div().flex_none().w(width).child(label);
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(COLUMN_GAP)
            .h(HEADER_HEIGHT)
            .px(ROW_PADDING)
            .bg(c.s1)
            .border_b_1()
            .border_color(c.line_subtle)
            .text_color(c.fg3)
            .text_spec(&theme.text.tag)
            .whitespace_nowrap()
            .child(div().flex_1().min_w_0().child("Session"))
            .child(cell(WAS_IN_WIDTH, "Was in"))
            .child(cell(LOCATION_WIDTH, "Location"))
            .child(cell(MOVED_WIDTH, "Moved here"))
            .child(cell(SIZE_WIDTH, "Size").flex().justify_end())
            .child(div().flex_none().w(ACTIONS_WIDTH))
    }
}

impl Render for TrashView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.sb_theme().clone();
        let c = &theme.colors;
        self.lay_out(cx);
        let library = self.library.read(cx);
        let page = library.sessions();
        let total: u64 = page.map_or(0, |page| page.sizes.values().sum());
        let count = page.map_or(library.counts().trash, |page| page.sessions.len());
        let line: SharedString = format!(
            "{} · {}. {ABOUT}",
            format::plural(count, "session"),
            format::fine_size(total)
        )
        .into();
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(PAGE_GAP)
            .pt(theme.sizes.page_top)
            .px(theme.sizes.page_side)
            .child(header(
                SbIcon::Trash,
                false,
                "Trash",
                Under::Line(line),
                &theme,
            ))
            .when(page.is_some_and(|page| !page.sessions.is_empty()), |this| {
                this.child(
                    div()
                        .flex_shrink(1.)
                        .min_h(HEADER_HEIGHT + ROW_HEIGHT)
                        .h(HEADER_HEIGHT + ROW_HEIGHT * self.order.len() as f32 + px(2.))
                        .flex()
                        .flex_col()
                        .rounded(theme.radii.card)
                        .border_1()
                        .border_color(c.line_subtle)
                        .overflow_hidden()
                        .child(Self::table_header(&theme))
                        .child(
                            div()
                                .relative()
                                .flex_1()
                                .min_h_0()
                                .child(
                                    v_virtual_list(
                                        cx.entity(),
                                        "trash",
                                        self.sizes.clone(),
                                        Self::render_rows,
                                    )
                                    .track_scroll(&self.scroll),
                                )
                                .child(Scrollbar::vertical(&self.scroll)),
                        )
                        .debug_selector(|| "trash-table".into())
                        .automation_target("trash/table"),
                )
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(theme.spacing.md)
                        .pb(theme.spacing.xl)
                        .text_color(c.fg3)
                        .text_spec(&theme.text.secondary)
                        .child(icon(SbIcon::CircleAlert, theme.sizes.icon_sm, c.fg3))
                        .child(NOTE)
                        .automation_target("trash/note"),
                )
            })
            .when(page.is_some_and(|page| page.sessions.is_empty()), |this| {
                this.child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(c.fg2)
                        .child(EMPTY)
                        .automation_target("page/empty"),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn the_page_says_what_design_writes() {
        assert_eq!(
            ABOUT,
            "They are hidden everywhere else but still archived, and syncs keep adding to \
             them. Restore puts a session back in its folder. Trash is never emptied on its own."
        );
        assert_eq!(
            NOTE,
            "Deleting for good removes a session from this archive only. Nothing is ever \
             deleted at a location."
        );
        assert_eq!(EMPTY, "Trash is empty.");
    }
}
