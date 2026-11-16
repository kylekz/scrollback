//! The list view (DESIGN §4.5, `E-All-Sessions`): the page's sessions as 34px rows under a
//! header, grouped by month, a virtual list like the cards. Columns: star, Title with the
//! first prompt's start, Folder (on All sessions and Starred, and on a folder's page while
//! one of its subfolders has sessions), Agent · Model, Location, Branch, Date (whose header
//! flips the order) and Calls. A narrow window drops Branch, then Location, then Folder,
//! before Title would go below 240.

use std::ops::Range;

use gpui::{
    AnyElement, ClickEvent, Context, InteractiveElement as _, IntoElement, MouseButton,
    MouseDownEvent, ParentElement as _, Pixels, Size, StatefulInteractiveElement as _, Styled as _,
    Window, canvas, div, prelude::FluentBuilder as _, px, size,
};
use gpui_component::scroll::Scrollbar;
use gpui_component::v_virtual_list;
use sb_core::catalogue::{Filed, Session, Shelf};
use sb_core::filing::FolderId;

use super::{LIFTED_OPACITY, SessionsView, month_name, subfolder_of};
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{focus_ring, icon, location_tag, overline};
use crate::drag;
use crate::format;
use crate::icons::SbIcon;
use crate::services::Month;
use crate::services::library::title_of;
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _};

/// The columns' widths (DESIGN §4.5). The date has room for a year and the hand glyph.
const STAR_WIDTH: Pixels = px(16.);
const FOLDER_WIDTH: Pixels = px(120.);
const MODEL_WIDTH: Pixels = px(96.);
const LOCATION_WIDTH: Pixels = px(104.);
const BRANCH_WIDTH: Pixels = px(120.);
const DATE_WIDTH: Pixels = px(84.);
const CALLS_WIDTH: Pixels = px(44.);
/// The narrowest Title gets before a column goes.
const TITLE_MIN: Pixels = px(240.);
const HEADER_HEIGHT: Pixels = px(30.);
const MONTH_HEIGHT: Pixels = px(30.);
/// A row's padding at its sides, and the gap between its columns (`.lrow`).
const ROW_PADDING: Pixels = px(10.);
const COLUMN_GAP: Pixels = px(12.);

/// One row of the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListRow {
    /// A month's overline, as an index into the months.
    Month(usize),
    /// A session, by its place among those that show.
    Session(usize),
}

/// The list's rows for `months`.
pub fn rows(months: &[Month]) -> Vec<ListRow> {
    months
        .iter()
        .enumerate()
        .flat_map(|(at, month)| {
            std::iter::once(ListRow::Month(at)).chain(month.sessions.clone().map(ListRow::Session))
        })
        .collect()
}

/// Each row's height: 30 for a month, 34 for a session.
pub fn row_sizes(rows: &[ListRow], theme: &SbTheme) -> Vec<Size<Pixels>> {
    rows.iter()
        .map(|row| {
            size(
                px(0.),
                match row {
                    ListRow::Month(_) => MONTH_HEIGHT,
                    ListRow::Session(_) => theme.sizes.list_row_height,
                },
            )
        })
        .collect()
}

/// Which of the columns that can go show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Columns {
    pub folder: bool,
    pub location: bool,
    pub branch: bool,
}

/**
 * The columns a list `width` wide shows, Folder only where the page has one (`folder`):
 * Branch goes first, then Location, then Folder, so that Title keeps 240.
 */
pub fn columns(width: Pixels, folder: bool) -> Columns {
    let fixed = STAR_WIDTH + MODEL_WIDTH + DATE_WIDTH + CALLS_WIDTH + ROW_PADDING * 2.;
    let title = |columns: &Columns| {
        let shown = [
            (columns.folder, FOLDER_WIDTH),
            (columns.location, LOCATION_WIDTH),
            (columns.branch, BRANCH_WIDTH),
        ];
        let widths: Pixels = shown
            .iter()
            .filter(|(on, _)| *on)
            .map(|(_, width)| *width)
            .sum();
        let gaps = 4 + shown.iter().filter(|(on, _)| *on).count();
        width - fixed - widths - COLUMN_GAP * gaps as f32
    };
    let mut columns = Columns {
        folder,
        location: true,
        branch: true,
    };
    for drop in [
        |columns: &mut Columns| columns.branch = false,
        |columns: &mut Columns| columns.location = false,
        |columns: &mut Columns| columns.folder = false,
    ] {
        if title(&columns) >= TITLE_MIN {
            break;
        }
        drop(&mut columns);
    }
    columns
}

impl SessionsView {
    /// The list view, under its header; it reaches the main area's edge, its scrollbar in the
    /// page's side padding.
    pub(super) fn list(
        &self,
        folder: Option<FolderId>,
        theme: &SbTheme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let columns = self.list_columns(folder);
        let oldest = self.view.oldest_first;
        let cell = |width: Pixels, label: &'static str| div().flex_none().w(width).child(label);
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .mr(-theme.sizes.page_side)
            .child(
                div()
                    .mr(theme.sizes.page_side)
                    .flex()
                    .items_center()
                    .gap(COLUMN_GAP)
                    .h(HEADER_HEIGHT)
                    .px(ROW_PADDING)
                    .rounded_t(theme.radii.card)
                    .border_1()
                    .border_color(c.line_subtle)
                    .bg(c.s1)
                    .text_color(c.fg3)
                    .text_spec(&theme.text.tag)
                    .whitespace_nowrap()
                    .child(div().flex_none().w(STAR_WIDTH))
                    .child(div().flex_1().min_w_0().child("Title"))
                    .when(columns.folder, |this| {
                        this.child(cell(FOLDER_WIDTH, "Folder"))
                    })
                    .child(cell(MODEL_WIDTH, "Model"))
                    .when(columns.location, |this| {
                        this.child(cell(LOCATION_WIDTH, "Location"))
                    })
                    .when(columns.branch, |this| {
                        this.child(cell(BRANCH_WIDTH, "Branch"))
                    })
                    .child(
                        div()
                            .id("list-date")
                            .flex_none()
                            .w(DATE_WIDTH)
                            .flex()
                            .items_center()
                            .gap(theme.spacing.xs)
                            .text_color(c.fg)
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.library.update(cx, |library, cx| {
                                    let mut view = library.view();
                                    view.oldest_first = !oldest;
                                    library.set_view(view, cx);
                                });
                            }))
                            .child("Date")
                            .child(icon(
                                if oldest {
                                    SbIcon::ChevronUp
                                } else {
                                    SbIcon::ChevronDown
                                },
                                theme.sizes.icon_xs,
                                c.fg2,
                            ))
                            .automation_target("list/date"),
                    )
                    .child(cell(CALLS_WIDTH, "Calls").flex().justify_end()),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_virtual_list(
                            cx.entity(),
                            "list",
                            self.list_sizes.clone(),
                            Self::render_list,
                        )
                        .track_scroll(&self.list_scroll),
                    )
                    .child(Scrollbar::vertical(&self.list_scroll)),
            )
            .automation_target("page/list")
            .into_any_element()
    }

    fn list_columns(&self, folder: Option<FolderId>) -> Columns {
        columns(
            self.room,
            folder.is_none() || !self.line.subfolders.is_empty(),
        )
    }

    fn render_list(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = cx.sb_theme().clone();
        let c = &theme.colors;
        let library = self.library.read(cx);
        let Some(page) = library.sessions() else {
            return Vec::new();
        };
        let folder = match library.shelf() {
            Shelf::Folder(folder) => Some(folder),
            _ => None,
        };
        let columns = self.list_columns(folder);
        let keyboard = self.focus.is_focused(window) && window.last_input_was_keyboard();
        let now = format::now();
        range
            .filter_map(|at| self.list_rows.get(at).copied())
            .map(|row| {
                let part = div()
                    .w_full()
                    .border_x_1()
                    .border_b_1()
                    .border_color(c.line_subtle);
                /* Each row keeps the page's side padding at its right, so that the list, and
                its scrollbar, reach the main area's edge. */
                let row = match row {
                    ListRow::Month(month) => part
                        .flex()
                        .items_center()
                        .h(MONTH_HEIGHT)
                        .px(ROW_PADDING)
                        .child(overline(
                            &format!(
                                "{} · {}",
                                month_name(&self.months[month]),
                                format::count(self.months[month].sessions.len() as u64)
                            ),
                            &theme,
                        ))
                        .into_any_element(),
                    ListRow::Session(place) => {
                        let index = self.shown[place];
                        self.list_row(
                            part,
                            (place, index),
                            &page.sessions[index],
                            (columns, folder, keyboard, now),
                            &theme,
                            cx,
                        )
                    }
                };
                div()
                    .w_full()
                    .pr(theme.sizes.page_side)
                    .child(row)
                    .into_any_element()
            })
            .collect()
    }

    /// A session's row (DESIGN §4.5): its star, title and prompt's start, folder, agent's mark
    /// and model, location, branch, date (and the hand glyph when it was filed by hand) and
    /// calls.
    fn list_row(
        &self,
        part: gpui::Div,
        (place, index): (usize, usize),
        session: &Session,
        (columns, folder, keyboard, now): (Columns, Option<FolderId>, bool, i64),
        theme: &SbTheme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let title = title_of(session);
        let library = self.library.read(cx);
        let selected = library.is_selected(&session.id);
        let ringed = keyboard && library.selected_index() == Some(index);
        let lifted = drag::lifts_session(library, &session.id);
        let model = agents::model(&session.id.provider, &session.models, cx)
            .unwrap_or_else(|| agents::name(&session.id.provider, cx));
        let folder_name = match (folder, session.filed) {
            (Some(_), _) => subfolder_of(session, library.shelf()),
            (None, Filed::ByRule(it) | Filed::ByHand(it)) => Some(it),
            (None, Filed::Unsorted) => None,
        }
        .and_then(|it| library.folder(it))
        .map(|it| it.name.clone());
        let location = self.sync.read(cx).location(&session.id.location);
        let drawn = self.drawn.clone();
        let row = part
            .id(("list-row", place))
            .debug_selector(move || format!("list-row-{place}"))
            .relative()
            .flex()
            .items_center()
            .gap(COLUMN_GAP)
            .h(theme.sizes.list_row_height)
            .px(ROW_PADDING)
            .text_spec(&theme.text.row)
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
            .when(lifted, |this| this.opacity(LIFTED_OPACITY))
            .on_click(cx.listener({
                let id = session.id.clone();
                move |this, event: &ClickEvent, window, cx| {
                    let pressed = (event.modifiers(), event.click_count());
                    this.click((&id, index), pressed, window, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener({
                    let id = session.id.clone();
                    move |this, event: &MouseDownEvent, window, cx| {
                        this.right_click((&id, index), event, window, cx);
                    }
                }),
            )
            .child(
                canvas(
                    {
                        let id = session.id.clone();
                        move |bounds, _, _| {
                            drawn.borrow_mut().insert(id.clone(), bounds);
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(
                div()
                    .flex_none()
                    .w(STAR_WIDTH)
                    .when(session.starred, |this| {
                        this.child(icon(SbIcon::StarFilled, theme.sizes.icon_xs, c.yellow))
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.md)
                    .child(
                        div()
                            .flex_none()
                            .max_w_full()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_spec(&theme.text.row_title)
                            .child(title.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_color(c.fg3)
                            .child(crate::hostile::label(
                                session.first_prompt.as_deref().unwrap_or_default(),
                            )),
                    ),
            )
            .when(columns.folder, |this| {
                this.child(
                    div()
                        .flex_none()
                        .w(FOLDER_WIDTH)
                        .flex()
                        .items_center()
                        .gap(theme.spacing.sm)
                        .overflow_hidden()
                        .text_color(c.fg2)
                        .children(folder_name.map(|name| {
                            div()
                                .flex()
                                .items_center()
                                .gap(theme.spacing.sm)
                                .min_w_0()
                                .child(icon(SbIcon::Folder, theme.sizes.icon_xs, c.fg3))
                                .child(
                                    div()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .child(name),
                                )
                        })),
                )
            })
            .child(
                div()
                    .flex_none()
                    .w(MODEL_WIDTH)
                    .flex()
                    .items_center()
                    .gap(theme.spacing.sm)
                    .text_color(c.fg2)
                    .child(agents::mark(&session.id.provider, theme.sizes.icon_xs, cx))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(model),
                    ),
            )
            .when(columns.location, |this| {
                this.child(
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
            })
            .when(columns.branch, |this| {
                this.child(
                    div()
                        .flex_none()
                        .w(BRANCH_WIDTH)
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_color(c.fg2)
                        .text_spec(&theme.text.path_small)
                        .child(
                            (session.branch.as_deref())
                                .map(crate::hostile::label)
                                .unwrap_or_default(),
                        ),
                )
            })
            .child(
                div()
                    .id(("list-date", place))
                    .flex_none()
                    .w(DATE_WIDTH)
                    .flex()
                    .items_center()
                    .gap(theme.spacing.xs)
                    .text_color(c.fg3)
                    .text_spec(&theme.text.number)
                    .child(
                        session
                            .ended
                            .or(session.started)
                            .map(|time| format::day(time, now))
                            .unwrap_or_default(),
                    )
                    .when(matches!(session.filed, Filed::ByHand(_)), |this| {
                        this.child(icon(SbIcon::Hand, theme.sizes.icon_xs, c.fg3))
                            .tooltip(|window, cx| {
                                gpui_component::tooltip::Tooltip::new("Filed by hand")
                                    .build(window, cx)
                            })
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .w(CALLS_WIDTH)
                    .flex()
                    .justify_end()
                    .text_color(c.fg2)
                    .text_spec(&theme.text.number)
                    .child(format::count(u64::from(session.all.calls))),
            );
        self.draggable(row, (session.id.clone(), index), (title, None), cx)
            .automation_target_with(move || format!("list/row/{place}").into())
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn a_narrow_list_drops_branch_then_location_then_folder() {
        let all = Columns {
            folder: true,
            location: true,
            branch: true,
        };
        // 1440 with the sidebar docked: 1144, room for every column.
        assert_eq!(columns(px(1144.), true), all);
        // 960 with the sidebar collapsed: 912. Branch goes.
        assert_eq!(
            columns(px(912.), true),
            Columns {
                branch: false,
                ..all
            }
        );
        // 1040 with it docked: 744. Location goes too.
        assert_eq!(
            columns(px(744.), true),
            Columns {
                branch: false,
                location: false,
                folder: true,
            }
        );
        assert_eq!(
            columns(px(500.), true),
            Columns {
                branch: false,
                location: false,
                folder: false,
            }
        );
        // A page without a Folder column keeps the others longer.
        assert!(!columns(px(912.), false).folder);
        assert!(columns(px(912.), false).location);
    }

    #[test]
    fn each_month_heads_its_sessions() {
        let months = [
            Month {
                month: Some((2026, 11)),
                sessions: 0..2,
            },
            Month {
                month: Some((2026, 10)),
                sessions: 2..3,
            },
        ];
        assert_eq!(
            rows(&months),
            [
                ListRow::Month(0),
                ListRow::Session(0),
                ListRow::Session(1),
                ListRow::Month(1),
                ListRow::Session(2),
            ]
        );
    }
}
