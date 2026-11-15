//! Unsorted (DESIGN §4.6): the sessions no folder claims, grouped by the project path and the
//! location they started at. A group whose path says where it is from the top can become a
//! folder: **Create folder from this path** makes one named after the path's last component
//! with the path attached to it, which files the group there (PLAN §8). The sessions that
//! recorded no path are a group with nothing to make a folder from.
//!
//! **The list is virtual, by row.** Its items are the groups' heads and the sessions that show
//! under them ([`items`]): five of a group until "Show all" is asked for, none while it is
//! closed. So a group of thousands shown in full costs what the visible rows do.
//!
//! **Keyboard**: `↑` and `↓` move over the sessions that show, `Enter` opens the selected one,
//! and `Tab` reaches each group's buttons.
//!
//! Attaching a group's path to an existing folder, dragging its chip, the filter and the
//! sort come with later tasks.

use std::collections::HashSet;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AnyElement, ClickEvent, Context, Div, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Render, Role, ScrollStrategy, SharedString, Size,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px, size,
};
use gpui_component::scroll::Scrollbar;
use gpui_component::{VirtualListScrollHandle, v_virtual_list};
use sb_core::catalogue::{PathGroup, Session, SessionId};

use super::header::{Under, header};
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{ButtonKind, button, icon, icon_button, location_tag};
use crate::format;
use crate::icons::SbIcon;
use crate::services::{
    ActiveServices as _, LibraryService, SyncService, can_attach, group_key, group_path,
};
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _};

/// How many of a group's sessions show until all are asked for (DESIGN §4.6).
const SHOWN: usize = 5;

const EVERYTHING_FILED: &str = "Everything is filed.";

/// What the group of the sessions that recorded no project path is called.
const NO_PATH: &str = "No project path";

/// The gap between the page's header, its summary and its groups (`design/project/E-Unsorted`).
const PAGE_GAP: Pixels = px(14.);

/// A row's padding at its sides (`design/project/sb.css`, `.lrow`).
const ROW_PADDING: Pixels = px(10.);

/// The widths of a row's columns after its title: the model, the day, the tool calls.
const MODEL_WIDTH: Pixels = px(132.);
const DAY_WIDTH: Pixels = px(84.);
const CALLS_WIDTH: Pixels = px(64.);

/// One item of the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    /// A group's head, as an index into the groups.
    Head(usize),
    /// Why the group's folder could not be made, under its head.
    Refused(usize),
    /// One of a group's sessions: the group, and the session's place in it.
    Row(usize, usize),
    /// "Show all" under a group's first sessions.
    ShowAll(usize),
}

impl Item {
    fn group(self) -> usize {
        match self {
            Self::Head(group)
            | Self::Refused(group)
            | Self::Row(group, _)
            | Self::ShowAll(group) => group,
        }
    }
}

/// The list's items for groups given as how many sessions each has, how many of them show,
/// and whether it has a refusal to show.
pub fn items(groups: impl IntoIterator<Item = (usize, usize, bool)>) -> Vec<Item> {
    let mut items = Vec::new();
    for (at, (sessions, shown, refused)) in groups.into_iter().enumerate() {
        items.push(Item::Head(at));
        if refused {
            items.push(Item::Refused(at));
        }
        items.extend((0..shown).map(|row| Item::Row(at, row)));
        if shown > 0 && shown < sessions {
            items.push(Item::ShowAll(at));
        }
    }
    items
}

pub struct UnsortedView {
    library: Entity<LibraryService>,
    sync: Entity<SyncService>,
    scroll: VirtualListScrollHandle,
    /// The groups that show all their sessions, and those that show none, by
    /// [`group_key`].
    all: HashSet<String>,
    closed: HashSet<String>,
    items: Vec<Item>,
    sizes: Rc<Vec<Size<Pixels>>>,
    /// The items are of what the library held before it last changed.
    stale: bool,
    _observers: [Subscription; 2],
}

impl UnsortedView {
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
            all: HashSet::new(),
            closed: HashSet::new(),
            items: Vec::new(),
            sizes: Rc::default(),
            stale: true,
        }
    }

    /// How many of a group's sessions show.
    fn shown(&self, group: &PathGroup) -> usize {
        let key = group_key(group);
        if self.closed.contains(&key) {
            0
        } else if self.all.contains(&key) {
            group.sessions.len()
        } else {
            group.sessions.len().min(SHOWN)
        }
    }

    /**
     * Lists the items again when the groups or what shows of them changed. A group is a
     * bordered section: its head is 44 high under the section's top border, each row under it
     * 34, and the 12px gap to the next group is under its last item.
     */
    fn lay_out(&mut self, theme: &SbTheme, cx: &Context<Self>) {
        if !self.stale {
            return;
        }
        self.stale = false;
        let library = self.library.read(cx);
        self.items = items(library.groups().iter().map(|group| {
            (
                group.sessions.len(),
                self.shown(group),
                library.refusal(&group_key(group)).is_some(),
            )
        }));
        let sizes = self
            .items
            .iter()
            .enumerate()
            .map(|(at, item)| {
                let last = self.is_last(at);
                let height = match item {
                    Item::Head(_) => theme.sizes.group_head_height + px(1.),
                    Item::Refused(_) | Item::Row(..) | Item::ShowAll(_) => {
                        theme.sizes.list_row_height
                    }
                };
                size(
                    px(0.),
                    height + if last { theme.spacing.lg } else { px(0.) },
                )
            })
            .collect();
        self.sizes = Rc::new(sizes);
    }

    /// Whether the item at `at` is the last of its group.
    fn is_last(&self, at: usize) -> bool {
        self.items
            .get(at + 1)
            .is_none_or(|next| next.group() != self.items[at].group())
    }

    fn toggle(&mut self, group: String, cx: &mut Context<Self>) {
        if !self.closed.remove(&group) {
            self.closed.insert(group);
        }
        self.stale = true;
        cx.notify();
    }

    fn show_all(&mut self, group: String, cx: &mut Context<Self>) {
        self.all.insert(group);
        self.stale = true;
        cx.notify();
    }

    /// The session `id` of the groups, wherever a load has put it.
    fn session<'a>(&self, id: &SessionId, cx: &'a Context<Self>) -> Option<&'a Session> {
        self.library
            .read(cx)
            .groups()
            .iter()
            .flat_map(|group| &group.sessions)
            .find(|session| session.id == *id)
    }

    /**
     * Moves the selection down (`by` 1) or up (-1) to the next session that shows; with none
     * selected, to the first. A selected session that doesn't show, its group closed, is
     * stepped from where it is among the groups.
     */
    pub fn step(&mut self, by: isize, cx: &mut Context<Self>) {
        // A load since the last frame may have moved the groups.
        let theme = cx.sb_theme().clone();
        self.lay_out(&theme, cx);
        let groups = self.library.read(cx).groups();
        // Each session that shows: its item, and its group and place in it.
        let mut rows = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(at, item)| match *item {
                Item::Row(group, row) => Some((at, (group, row))),
                _ => None,
            });
        let place = self.library.read(cx).selected().and_then(|selected| {
            groups.iter().enumerate().find_map(|(group, it)| {
                let row = it
                    .sessions
                    .iter()
                    .position(|session| session.id == *selected)?;
                Some((group, row))
            })
        });
        let to = match place {
            None => rows.next(),
            Some(place) if by > 0 => rows.find(|(_, at)| *at > place),
            Some(place) => rows.rfind(|(_, at)| *at < place),
        };
        let Some((item, (group, row))) = to else {
            return;
        };
        let id = groups[group].sessions[row].id.clone();
        self.scroll.scroll_to_item(item, ScrollStrategy::Nearest);
        self.library
            .update(cx, |library, cx| library.select(id, cx));
    }

    /// Opens the selected session.
    pub fn open_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(selected) = self.library.read(cx).selected().cloned() {
            self.open(&selected, cx);
        }
    }

    fn open(&mut self, id: &SessionId, cx: &mut Context<Self>) {
        if let Some(session) = self.session(id, cx).cloned() {
            self.library.update(cx, |library, cx| {
                library.select(session.id.clone(), cx);
                library.open(session, cx);
            });
        }
    }

    fn render_items(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = cx.sb_theme().clone();
        let c = &theme.colors;
        let now = format::now();
        let groups = self.library.read(cx).groups();
        range
            .filter_map(|at| {
                let item = *self.items.get(at)?;
                let group = groups.get(item.group())?;
                let last = self.is_last(at);
                // Every item draws its part of the section's border.
                let part = div()
                    .border_x_1()
                    .border_b_1()
                    .border_color(c.line_subtle)
                    .overflow_hidden()
                    .when(last, |this| this.rounded_b(theme.radii.card));
                let part = match item {
                    Item::Head(of) => self.head(part, (at, of), group, &theme, cx),
                    Item::Refused(_) => refused(
                        part,
                        self.library
                            .read(cx)
                            .refusal(&group_key(group))
                            .unwrap_or_default(),
                        &theme,
                    ),
                    Item::Row(of, row) => self.row(
                        part,
                        at,
                        (of, row),
                        group.sessions.get(row)?,
                        now,
                        &theme,
                        cx,
                    ),
                    Item::ShowAll(of) => self.show_all_row(part, at, of, group, &theme, cx),
                };
                Some(
                    div()
                        .w_full()
                        .when(last, |this| this.pb(theme.spacing.lg))
                        .child(part)
                        .into_any_element(),
                )
            })
            .collect()
    }

    /// A group's head (DESIGN §4.6): what opens and closes it, the path's chip with where it
    /// is from, how many sessions it has, the other ways they wrote the path, and the button
    /// that makes a folder from it.
    fn head(
        &self,
        part: Div,
        (at, index): (usize, usize),
        group: &PathGroup,
        theme: &SbTheme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let sync = self.sync.read(cx);
        let library = self.library.read(cx);
        let key = group_key(group);
        let open = self.shown(group) > 0;
        let path = group_path(group);
        let creating = library.is_creating(&key);
        part.border_t_1()
            .rounded_t(theme.radii.card)
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .h(theme.sizes.group_head_height + px(1.))
            .pl(theme.spacing.sm)
            .pr(ROW_PADDING)
            .bg(c.s1)
            .child(
                icon_button(
                    ("group-toggle", at),
                    theme.sizes.control_small_height,
                    theme,
                )
                .aria_label(if open {
                    "Hide sessions"
                } else {
                    "Show sessions"
                })
                .on_click(cx.listener({
                    let key = key.clone();
                    move |this, _, _, cx| this.toggle(key.clone(), cx)
                }))
                .child(icon(
                    if open {
                        SbIcon::ChevronDown
                    } else {
                        SbIcon::ChevronRight
                    },
                    theme.sizes.icon_sm,
                    c.fg2,
                ))
                .automation_target_with(move || format!("unsorted/group/{index}/toggle").into()),
            )
            .map(|this| match &path {
                // The path's chip. It becomes what is dragged onto a folder.
                Some((shown, _)) => this.child(
                    div()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap(theme.spacing.sm)
                        .h(theme.sizes.path_chip_height)
                        .pl(theme.spacing.xs)
                        .pr(ROW_PADDING)
                        .rounded(theme.radii.pill)
                        .bg(c.s2)
                        .border_1()
                        .border_color(c.line_subtle)
                        .whitespace_nowrap()
                        .children(
                            group
                                .locations
                                .iter()
                                .filter_map(|location| sync.location(location))
                                .map(|location| {
                                    location_tag(
                                        location.name.clone(),
                                        &location.kind,
                                        false,
                                        theme,
                                    )
                                }),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis_middle()
                                .text_spec(&theme.text.path)
                                .child(shown.clone()),
                        ),
                ),
                None => this.child(
                    div()
                        .flex_none()
                        .pl(theme.spacing.xs)
                        .text_spec(&theme.text.row_title)
                        .child(NO_PATH),
                ),
            })
            .child(
                div()
                    .flex_none()
                    .text_color(c.fg3)
                    .text_spec(&theme.text.number)
                    .child(format::plural(group.sessions.len(), "session")),
            )
            .children(
                path.as_ref()
                    .filter(|(_, others)| !others.is_empty())
                    .map(|(_, others)| {
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_color(c.fg3)
                            .text_spec(&theme.text.secondary)
                            .child(format!("also as {}", others.join(", ")))
                    }),
            )
            .child(div().flex_1())
            .when(can_attach(group), |this| {
                this.child(
                    // Until the group has gone into its folder, a second click makes no second.
                    button(
                        ("create-folder", at),
                        ButtonKind::Plain,
                        true,
                        !creating,
                        theme,
                    )
                    .when(!creating, |this| {
                        this.on_click(cx.listener(move |this, _, _, cx| {
                            this.library
                                .update(cx, |library, cx| library.create_folder_from(&key, cx));
                        }))
                    })
                    .child(icon(SbIcon::FolderPlus, theme.sizes.icon_sm, c.fg))
                    .child("Create folder from this path")
                    .automation_target_with(move || {
                        format!("unsorted/group/{index}/create-folder").into()
                    }),
                )
            })
            .automation_target_with(move || format!("unsorted/group/{index}").into())
    }

    /// A session's row (DESIGN §4.6): its title and the start of its prompt, its agent's mark
    /// and model, when it ended, and its tool calls.
    #[expect(clippy::too_many_arguments)]
    fn row(
        &self,
        part: Div,
        at: usize,
        (group, row): (usize, usize),
        session: &Session,
        now: i64,
        theme: &SbTheme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let selected = self.library.read(cx).selected() == Some(&session.id);
        let title: SharedString = session
            .title
            .clone()
            .unwrap_or_else(|| "Untitled session".to_string())
            .into();
        let model = agents::model(&session.id.provider, &session.models, cx)
            .unwrap_or_else(|| agents::name(&session.id.provider, cx));
        let id = session.id.clone();
        part.id(("row", at))
            .role(Role::ListItem)
            .flex()
            .items_center()
            .gap(theme.spacing.lg)
            .h(theme.sizes.list_row_height)
            .px(ROW_PADDING)
            .text_spec(&theme.text.row)
            .whitespace_nowrap()
            .cursor_pointer()
            .map(|this| {
                if selected {
                    this.bg(c.s3)
                } else {
                    this.hover(|style| style.bg(c.s1))
                }
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                if event.click_count() > 1 {
                    this.open(&id, cx);
                } else {
                    let id = id.clone();
                    this.library
                        .update(cx, |library, cx| library.select(id, cx));
                }
            }))
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
                            .child(title),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_color(c.fg3)
                            .child(session.first_prompt.clone().unwrap_or_default()),
                    ),
            )
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
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(model),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .w(DAY_WIDTH)
                    .text_color(c.fg3)
                    .text_spec(&theme.text.number)
                    .child(
                        session
                            .ended
                            .or(session.started)
                            .map(|ended| format::day(ended, now))
                            .unwrap_or_default(),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .w(CALLS_WIDTH)
                    .flex()
                    .justify_end()
                    .text_color(c.fg2)
                    .text_spec(&theme.text.number)
                    .child(format::plural(session.all.calls as usize, "call")),
            )
            .automation_target_with(move || format!("unsorted/row/{group}/{row}").into())
    }

    /// The row that shows the rest of a group's sessions.
    fn show_all_row(
        &self,
        part: Div,
        at: usize,
        of: usize,
        group: &PathGroup,
        theme: &SbTheme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let key = group_key(group);
        part.id(("show-all", at))
            .role(Role::Button)
            .tab_index(0)
            .flex()
            .items_center()
            .h(theme.sizes.list_row_height)
            .px(ROW_PADDING)
            .text_color(c.accent_text)
            .text_spec(&theme.text.row)
            .cursor_pointer()
            .hover(|style| style.bg(c.s1))
            .focus_visible(|style| style.bg(c.s2))
            .on_click(cx.listener(move |this, _, _, cx| this.show_all(key.clone(), cx)))
            .child(format!(
                "Show all {}",
                format::count(group.sessions.len() as u64)
            ))
            .automation_target_with(move || format!("unsorted/group/{of}/show-all").into())
    }
}

/// The line under a group's head that says why its folder could not be made.
fn refused(part: Div, why: &str, theme: &SbTheme) -> AnyElement {
    let c = &theme.colors;
    part.flex()
        .items_center()
        .gap(theme.spacing.md)
        .h(theme.sizes.list_row_height)
        .px(ROW_PADDING)
        .bg(c.red_soft)
        .text_color(c.red)
        .text_spec(&theme.text.row)
        .whitespace_nowrap()
        .child(icon(SbIcon::CircleAlert, theme.sizes.icon_sm, c.red))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .child(format!("No folder was made: {why}.")),
        )
        .automation_target("unsorted/refused")
}

impl Render for UnsortedView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.sb_theme().clone();
        self.lay_out(&theme, cx);
        let library = self.library.read(cx);
        let loaded = library.page().is_some();
        let groups = library.groups();
        let sessions: usize = groups.iter().map(|group| group.sessions.len()).sum();
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(PAGE_GAP)
            .pt(theme.sizes.page_top)
            .px(theme.sizes.page_side)
            .child(header(
                SbIcon::Inbox,
                false,
                "Unsorted",
                Under::Line(
                    format!(
                        "{} whose project path no folder claims, grouped by that path. \
                         Attach a path to a folder and these sessions, and every later one \
                         from the same path, file themselves there.",
                        format::plural(library.counts().unsorted, "session")
                    )
                    .into(),
                ),
                &theme,
            ))
            .when(loaded && !groups.is_empty(), |this| {
                this.child(
                    div()
                        .flex_none()
                        .text_color(theme.colors.fg2)
                        .text_spec(&theme.text.number)
                        .child(format!(
                            "{} · {}",
                            format::plural(groups.len(), "path"),
                            format::plural(sessions, "session")
                        ))
                        .automation_target("unsorted/summary"),
                )
                .child(
                    // The scrollbar is in the page's side padding, clear of the rows.
                    div()
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .mr(-theme.sizes.page_side)
                        .child(
                            div().size_full().pr(theme.sizes.page_side).child(
                                v_virtual_list(
                                    cx.entity(),
                                    "groups",
                                    self.sizes.clone(),
                                    Self::render_items,
                                )
                                .track_scroll(&self.scroll),
                            ),
                        )
                        .child(Scrollbar::vertical(&self.scroll)),
                )
            })
            .when(loaded && groups.is_empty(), |this| {
                this.child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(theme.colors.fg2)
                        .child(EVERYTHING_FILED)
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
    fn a_group_is_its_head_and_the_sessions_that_show() {
        // Seven sessions with five showing, two with both, three closed, one refused.
        assert_eq!(
            items([(7, 5, false), (2, 2, false), (3, 0, false), (1, 1, true)]),
            [
                Item::Head(0),
                Item::Row(0, 0),
                Item::Row(0, 1),
                Item::Row(0, 2),
                Item::Row(0, 3),
                Item::Row(0, 4),
                Item::ShowAll(0),
                Item::Head(1),
                Item::Row(1, 0),
                Item::Row(1, 1),
                Item::Head(2),
                Item::Head(3),
                Item::Refused(3),
                Item::Row(3, 0),
            ]
        );
        assert_eq!(items([]), []);
    }
}
