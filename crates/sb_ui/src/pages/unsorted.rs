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
//! `S`, `Delete`, `M` and `Shift F10` act on it as on a folder's page, and `Tab` reaches each
//! group's buttons.
//!
//! **A group's path** attaches to an existing folder from **Attach to folder ▾**, or by
//! dragging its chip onto a folder in the sidebar, limited to the group's location; its
//! sessions drag onto a folder as a card does. The filter row narrows the groups by their path
//! or their sessions' titles and prompts, by location and agent, and orders them
//! ([`crate::services::view::shown_groups`]).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, Bounds, ClickEvent, Context, Div, Entity, FocusHandle,
    Focusable as _, InteractiveElement as _, IntoElement, Modifiers, MouseButton, MouseDownEvent,
    ParentElement as _, Pixels, Point, Render, Role, ScrollStrategy, SharedString, Size,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, canvas, div, point,
    prelude::FluentBuilder as _, px, size,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::scroll::Scrollbar;
use gpui_component::{VirtualListScrollHandle, v_virtual_list};
use sb_core::catalogue::{PathGroup, Session, SessionId};
use sb_core::filing::{FolderId, NormalPath};
use sb_core::location::LocationId;

use super::header::{Under, header};
use super::menus::{self, MOVE_MENU_WIDTH, SESSION_MENU_WIDTH};
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{ButtonKind, button, focus_ring, icon, icon_button, location_tag};
use crate::drag;
use crate::format;
use crate::icons::SbIcon;
use crate::menu::{self, Entry, Item as MenuItem, NESTED_INDENT};
use crate::services::drag::verdict;
use crate::services::library::title_of;
use crate::services::view::{GroupSort, UnsortedView as GroupsView, shown_groups};
use crate::services::{
    ActiveServices as _, Dragged, DropTarget, LibraryService, SyncService, TypedPath, Verdict,
    can_attach, group_key, group_path,
};
use crate::sidebar::tree;
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _};

/// How much a group's rows dim while its path is dragged (DESIGN §4.6).
const DRAGGED_ROWS_OPACITY: f32 = 0.45;

/// The width of Attach to folder's menu.
const ATTACH_MENU_WIDTH: Pixels = px(220.);

/**
 * The path a group's chip carries and a folder takes: in its drive form for a drive's path,
 * limited to the group's location, or to none when several share it (DESIGN §4.6).
 */
fn attachable(group: &PathGroup) -> Option<(TypedPath, NormalPath)> {
    if !can_attach(group) {
        return None;
    }
    let (typed, _) = group_path(group)?;
    let location: Option<LocationId> = match &group.locations[..] {
        [only] => Some(only.clone()),
        _ => None,
    };
    Some(((typed, location), group.project.clone()?))
}

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
    /// "Show all" under a group's first sessions, and how many it has that the filters leave.
    ShowAll(usize, usize),
}

impl Item {
    fn group(self) -> usize {
        match self {
            Self::Head(group)
            | Self::Refused(group)
            | Self::Row(group, _)
            | Self::ShowAll(group, _) => group,
        }
    }
}

/**
 * The list's items for groups given as their index among the groups, the places of their
 * sessions the filters leave, how many of those show, and whether it has a refusal to show.
 */
pub fn items(groups: impl IntoIterator<Item = (usize, Vec<usize>, usize, bool)>) -> Vec<Item> {
    let mut items = Vec::new();
    for (at, sessions, shown, refused) in groups {
        items.push(Item::Head(at));
        if refused {
            items.push(Item::Refused(at));
        }
        items.extend(sessions[..shown].iter().map(|row| Item::Row(at, *row)));
        if shown > 0 && shown < sessions.len() {
            items.push(Item::ShowAll(at, sessions.len()));
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
    /// Where the keyboard rests in the main area: while it is there, the selected row has
    /// the keyboard's ring.
    focus: FocusHandle,
    /// How the page was set to show when it was laid out, and how many paths and sessions
    /// the filters left then.
    view: GroupsView,
    paths: usize,
    left: usize,
    /// The filter field, made with the first frame.
    filter: Option<(Entity<InputState>, Subscription)>,
    /// Where each session that shows was drawn last: where its menu opens from the keys.
    drawn: Rc<RefCell<HashMap<SessionId, Bounds<Pixels>>>>,
    _observers: [Subscription; 2],
}

impl UnsortedView {
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
            ],
            library,
            sync,
            scroll: VirtualListScrollHandle::new(),
            all: HashSet::new(),
            closed: HashSet::new(),
            items: Vec::new(),
            sizes: Rc::default(),
            stale: true,
            focus,
            view: GroupsView::default(),
            paths: 0,
            left: 0,
            filter: None,
            drawn: Rc::default(),
        }
    }

    /// How many of a group's sessions show.
    fn shown(&self, group: &PathGroup, left: usize) -> usize {
        let key = group_key(group);
        if self.closed.contains(&key) {
            0
        } else if self.all.contains(&key) {
            left
        } else {
            left.min(SHOWN)
        }
    }

    /**
     * Lists the items again when the groups or what shows of them changed. A group is a
     * bordered section: its head is 44 high under the section's top border, each row under it
     * 34, and the 12px gap to the next group is under its last item.
     */
    fn lay_out(&mut self, theme: &SbTheme, cx: &Context<Self>) {
        let library = self.library.read(cx);
        if !self.stale && *library.unsorted_view() == self.view {
            return;
        }
        self.stale = false;
        self.view = library.unsorted_view().clone();
        let groups = library.groups();
        let shown = shown_groups(groups, &self.view);
        self.paths = shown.len();
        self.left = shown.iter().map(|(_, sessions)| sessions.len()).sum();
        self.items = items(shown.into_iter().map(|(at, sessions)| {
            let group = &groups[at];
            let count = self.shown(group, sessions.len());
            let refused = library.refusal(&group_key(group)).is_some();
            (at, sessions, count, refused)
        }));
        let sizes = self
            .items
            .iter()
            .enumerate()
            .map(|(at, item)| {
                let last = self.is_last(at);
                let height = match item {
                    Item::Head(_) => theme.sizes.group_head_height + px(1.),
                    Item::Refused(_) | Item::Row(..) | Item::ShowAll(..) => {
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
        // Where it is in the list: its row, or the head of its closed group.
        let item = place.and_then(|(group, row)| {
            (self.items.iter())
                .position(|item| *item == Item::Row(group, row))
                .or_else(|| {
                    self.items
                        .iter()
                        .position(|item| *item == Item::Head(group))
                })
        });
        let to = match item {
            None => rows.next(),
            Some(item) if by > 0 => rows.find(|(at, _)| *at > item),
            Some(item) => rows.rfind(|(at, _)| *at < item),
        };
        let Some((item, (group, row))) = to else {
            return;
        };
        let id = groups[group].sessions[row].id.clone();
        self.scroll.scroll_to_item(item, ScrollStrategy::Nearest);
        self.library
            .update(cx, |library, cx| library.select(id, cx));
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

    /// Whether the filter field has the keyboard.
    pub fn is_typing(&self, window: &Window, cx: &App) -> bool {
        self.filter
            .as_ref()
            .is_some_and(|(input, _)| input.focus_handle(cx).is_focused(window))
    }

    /// Opens the selected session's menu, or with `moving` its Move to folder, under its row;
    /// when the row isn't drawn, in the middle of the window.
    pub fn menu_selected(&mut self, moving: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.library.read(cx).selected().cloned() else {
            return;
        };
        let at = self.drawn.borrow().get(&id).map_or_else(
            || {
                let viewport = window.viewport_size();
                point(viewport.width / 2., viewport.height / 3.)
            },
            |bounds| bounds.bottom_left() + point(px(12.), px(-8.)),
        );
        self.open_menu(&id, moving, at, window, cx);
    }

    fn open_menu(
        &mut self,
        id: &SessionId,
        moving: bool,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let library = self.library.read(cx);
        let Some(session) = library.session(id).cloned() else {
            return;
        };
        let ids = library.acted_on(id);
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
     * A click on a session's row: it alone is selected and becomes where a run starts; with
     * `Ctrl` it joins the selection or leaves it; with `Shift` the run of rows from where the
     * last plain or `Ctrl` click was to it is selected, and with `Ctrl Shift` added to the
     * selection. A second click opens it.
     */
    fn click(
        &mut self,
        id: &SessionId,
        (modifiers, count): (Modifiers, usize),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The page takes the keyboard, so that its keys act on what was clicked.
        self.focus.focus(window, cx);
        if count > 1 {
            return self.open(id, cx);
        }
        let library = self.library.read(cx);
        let secondary = modifiers.secondary(cx);
        let chosen = if modifiers.shift
            && let Some(from) = library.anchor().cloned()
        {
            let groups = library.groups();
            let ids: Vec<&SessionId> = (self.items.iter())
                .filter_map(|item| match item {
                    Item::Row(group, row) => Some(&groups.get(*group)?.sessions.get(*row)?.id),
                    _ => None,
                })
                .collect();
            let ends = (
                ids.iter().position(|it| **it == from),
                ids.iter().position(|it| *it == id),
            );
            match ends {
                (Some(a), Some(b)) => {
                    let mut chosen: HashSet<SessionId> = if secondary {
                        library.selection().into_iter().collect()
                    } else {
                        HashSet::new()
                    };
                    chosen.extend(ids[a.min(b)..=a.max(b)].iter().map(|it| (*it).clone()));
                    Some((chosen, false))
                }
                _ => None,
            }
        } else if secondary && library.is_selected(id) {
            let id = id.clone();
            self.library
                .update(cx, |library, cx| library.unchoose(&id, cx));
            return;
        } else if secondary {
            let mut chosen: HashSet<SessionId> = library.selection().into_iter().collect();
            chosen.insert(id.clone());
            Some((chosen, true))
        } else {
            None
        };
        let id = id.clone();
        self.library.update(cx, |library, cx| match chosen {
            Some((chosen, anchored)) => library.choose(chosen, (id, None), anchored, cx),
            None => library.select(id, cx),
        });
    }

    /// Attach to folder ▾ (DESIGN §4.6): every folder, those that would refuse the path
    /// dimmed with why.
    fn attach_menu(&self, group: &PathGroup, cx: &App) -> Vec<Entry> {
        let Some((path, normal)) = attachable(group) else {
            return Vec::new();
        };
        let library = self.library.read(cx);
        let dragged = Dragged::Path {
            path: path.clone(),
            normal,
            from: None,
            sessions: group.sessions.len(),
        };
        let folders = library.folders();
        let every: HashSet<FolderId> = folders.iter().map(|folder| folder.id).collect();
        tree::rows(folders, &every)
            .into_iter()
            .map(|row| {
                let folder = &folders[row.folder];
                let (id, path, sessions) = (folder.id, path.clone(), group.sessions.len());
                let item = MenuItem::new(
                    format!("folder/{}", menus::folder_key(library, id)),
                    folder.name.clone(),
                )
                .glyph(SbIcon::Folder)
                .indent(NESTED_INDENT * row.depth as f32);
                Entry::Item(match verdict(&dragged, DropTarget::Folder(id), library) {
                    Some(Verdict::Refuses(why)) => item.off(why),
                    _ => item.on(move |_, cx| {
                        let library = cx.services().library.clone();
                        library.update(cx, |library, cx| {
                            library.attach(id, path.clone(), sessions, cx);
                        });
                    }),
                })
            })
            .collect()
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

    /// The list's items in `range`. Each keeps the page's side padding at its right, so that
    /// the list, and its scrollbar, reach the main area's edge.
    fn render_items(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = cx.sb_theme().clone();
        let c = &theme.colors;
        let now = format::now();
        let keyboard = self.focus.is_focused(window) && window.last_input_was_keyboard();
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
                        (now, keyboard),
                        &theme,
                        cx,
                    ),
                    Item::ShowAll(of, left) => {
                        self.show_all_row(part, at, (of, left), group, &theme, cx)
                    }
                };
                Some(
                    div()
                        .w_full()
                        .pr(theme.sizes.page_side)
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
        let attach_key = key.clone();
        let open = !self.closed.contains(&key);
        let path = group_path(group);
        let creating = library.is_creating(&key);
        let dragged = attachable(group).map(|(path, normal)| Dragged::Path {
            path,
            normal,
            from: None,
            sessions: group.sessions.len(),
        });
        let lifted = dragged
            .as_ref()
            .is_some_and(|dragged| drag::lifts(library, |what| what == dragged));
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
                // The path's chip, which drags onto a folder in the sidebar.
                Some((shown, _)) => this.child({
                    let chip = div()
                        .id(("path-chip", at))
                        .debug_selector(move || format!("unsorted-chip-{index}"))
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
                        .when(lifted, |this| this.border_dashed().opacity(0.5))
                        .whitespace_nowrap()
                        .when(dragged.is_some(), |this| {
                            this.cursor_grab().child(icon(
                                SbIcon::GripVertical,
                                theme.sizes.icon_xs,
                                c.fg3,
                            ))
                        })
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
                        );
                    match dragged.clone() {
                        Some(dragged) => {
                            drag::source(chip, move |_| dragged.clone(), |_, _, _, _| {})
                                .automation_target_with(move || {
                                    format!("unsorted/group/{index}/chip").into()
                                })
                        }
                        None => chip.into_any_element(),
                    }
                }),
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
                .when(!library.folders().is_empty(), |this| {
                    this.child(
                        button(("attach", at), ButtonKind::Plain, true, true, theme)
                            .debug_selector(move || format!("unsorted-attach-{index}"))
                            // The group by its key: a load may have moved it since it was drawn.
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let library = this.library.read(cx);
                                let Some(group) = (library.groups().iter())
                                    .find(|it| group_key(it) == attach_key)
                                else {
                                    return;
                                };
                                let entries = this.attach_menu(group, cx);
                                menu::open(
                                    entries,
                                    window.mouse_position(),
                                    ATTACH_MENU_WIDTH,
                                    window,
                                    cx,
                                );
                            }))
                            .child(icon(SbIcon::Link, theme.sizes.icon_sm, c.fg))
                            .child("Attach to folder")
                            .child(icon(SbIcon::ChevronDown, theme.sizes.icon_xs, c.fg2))
                            .automation_target_with(move || {
                                format!("unsorted/group/{index}/attach").into()
                            }),
                    )
                })
            })
            .automation_target_with(move || format!("unsorted/group/{index}").into())
    }

    /// A session's row (DESIGN §4.6): its title and the start of its prompt, its agent's mark
    /// and model, when it ended, and its tool calls. `keyboard` says the keyboard is on the
    /// page, and so on the selected row.
    #[expect(clippy::too_many_arguments)]
    fn row(
        &self,
        part: Div,
        at: usize,
        (group, row): (usize, usize),
        session: &Session,
        (now, keyboard): (i64, bool),
        theme: &SbTheme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let library = self.library.read(cx);
        let selected = library.is_selected(&session.id);
        let ringed = keyboard && library.selected() == Some(&session.id);
        let lifted = drag::lifts_session(library, &session.id)
            || drag::lifts(library, |what| match what {
                Dragged::Sessions { .. } | Dragged::Folder(_) => false,
                Dragged::Path { path, .. } => library
                    .groups()
                    .get(group)
                    .and_then(attachable)
                    .is_some_and(|(it, _)| it == *path),
            });
        let drawn = self.drawn.clone();
        let title: SharedString = title_of(session).into();
        let model = agents::model(&session.id.provider, &session.models, cx)
            .unwrap_or_else(|| agents::name(&session.id.provider, cx));
        let id = session.id.clone();
        let element = part
            .id(("row", at))
            .debug_selector(move || format!("unsorted-row-{group}-{row}"))
            .role(Role::ListItem)
            .relative()
            .when(lifted, |this| this.opacity(DRAGGED_ROWS_OPACITY))
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
                        .when(ringed, |this| this.shadow(focus_ring(true, theme)))
                } else {
                    this.hover(|style| style.bg(c.s1))
                }
            })
            .on_click(cx.listener({
                let id = id.clone();
                move |this, event: &ClickEvent, window, cx| {
                    let pressed = (event.modifiers(), event.click_count());
                    this.click(&id, pressed, window, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener({
                    let id = id.clone();
                    move |this, event: &MouseDownEvent, window, cx| {
                        this.focus.focus(window, cx);
                        if !this.library.read(cx).is_selected(&id) {
                            let id = id.clone();
                            this.library
                                .update(cx, |library, cx| library.select(id, cx));
                        }
                        this.open_menu(&id, false, event.position, window, cx);
                    }
                }),
            )
            .child(
                canvas(
                    {
                        let id = id.clone();
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
            );
        let (library, view) = (self.library.clone(), cx.entity().downgrade());
        let title = title.to_string();
        drag::source(
            element,
            {
                let id = id.clone();
                move |cx| Dragged::Sessions {
                    ids: library.read(cx).acted_on(&id),
                    title: title.clone(),
                }
            },
            move |modifiers, _, window, cx| {
                view.update(cx, |view, cx| view.click(&id, (modifiers, 1), window, cx))
                    .ok();
            },
        )
        .automation_target_with(move || format!("unsorted/row/{group}/{row}").into())
    }

    /// The row that shows the rest of a group's sessions, `left` of them as the filters leave
    /// it.
    fn show_all_row(
        &self,
        part: Div,
        at: usize,
        (of, left): (usize, usize),
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
            .child(format!("Show all {}", format::count(left as u64)))
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

impl UnsortedView {
    /**
     * Unsorted's filter row (DESIGN §4.6): the filter field, "6 paths · 37 sessions", the
     * Location and Agent filters, and the sort.
     */
    fn filter_row(
        &self,
        summary: String,
        window: &Window,
        theme: &SbTheme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let c = &theme.colors;
        let library = self.library.read(cx);
        let sync = self.sync.read(cx);
        let view = library.unsorted_view().clone();
        let groups = library.groups();
        let mut locations: Vec<LocationId> = Vec::new();
        let mut agents_seen: Vec<String> = Vec::new();
        for group in groups {
            for location in &group.locations {
                if !locations.contains(location) {
                    locations.push(location.clone());
                }
            }
            for session in &group.sessions {
                if !agents_seen.contains(&session.id.provider) {
                    agents_seen.push(session.id.provider.clone());
                }
            }
        }
        let set = |change: Box<dyn Fn(&mut GroupsView)>| {
            move |_: &mut Window, cx: &mut App| {
                let library = cx.services().library.clone();
                library.update(cx, |library, cx| {
                    let mut view = library.unsorted_view().clone();
                    change(&mut view);
                    library.set_unsorted_view(view, cx);
                });
            }
        };
        let location_name = |id: &LocationId| {
            sync.location(id)
                .map_or_else(|| id.to_string(), |it| it.name.clone())
        };
        let location_entries: Vec<Entry> = locations
            .iter()
            .map(|location| {
                let id = location.clone();
                Entry::Item(
                    MenuItem::new(
                        format!("location/{}", location_name(location)),
                        location_name(location),
                    )
                    .on(set(Box::new(move |view| view.location = Some(id.clone())))),
                )
            })
            .collect();
        let agent_entries: Vec<Entry> = agents_seen
            .iter()
            .map(|agent| {
                let id = agent.clone();
                Entry::Item(
                    MenuItem::new(format!("agent/{agent}"), agents::name(agent, cx))
                        .on(set(Box::new(move |view| view.agent = Some(id.clone())))),
                )
            })
            .collect();
        let sort_entries: Vec<Entry> = GroupSort::ALL
            .iter()
            .map(|sort| {
                let sort = *sort;
                Entry::Item(
                    MenuItem::new(format!("{sort:?}").to_lowercase(), sort.label())
                        .checked(view.sort == sort)
                        .on(set(Box::new(move |view| view.sort = sort))),
                )
            })
            .collect();
        let chip = |name: &'static str,
                    label: String,
                    on: bool,
                    entries: Vec<Entry>,
                    clear: Box<dyn Fn(&mut GroupsView)>| {
            let clear = set(clear);
            div()
                .id(name)
                .flex_none()
                .flex()
                .items_center()
                .gap(px(5.))
                .h(px(22.))
                .px(theme.spacing.md)
                .rounded(theme.radii.pill)
                .border_1()
                .text_spec(&theme.text.secondary)
                .cursor_pointer()
                .map(|this| {
                    if on {
                        this.bg(c.accent_soft)
                            .border_color(gpui::transparent_black())
                            .text_color(c.accent_text)
                    } else {
                        this.bg(c.s2).border_color(c.line_subtle).text_color(c.fg2)
                    }
                })
                .on_click(move |_, window, cx| {
                    if on {
                        clear(window, cx);
                    } else if !entries.is_empty() {
                        menu::open(
                            entries.clone(),
                            window.mouse_position(),
                            px(220.),
                            window,
                            cx,
                        );
                    }
                })
                .child(label)
                .child(icon(
                    if on { SbIcon::X } else { SbIcon::ChevronDown },
                    theme.sizes.icon_xs,
                    if on { c.accent_text } else { c.fg2 },
                ))
                .automation_target(format!("unsorted/filter/{name}"))
        };
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(theme.spacing.sm)
            .children(self.filter.as_ref().map(|(input, _)| {
                div()
                    .flex_none()
                    .w(px(220.))
                    .h(px(26.))
                    .flex()
                    .items_center()
                    .gap(theme.spacing.md)
                    .px(theme.sizes.list_row_padding)
                    .rounded(theme.radii.input)
                    .border_1()
                    .border_color(if input.focus_handle(cx).is_focused(window) {
                        c.accent
                    } else {
                        c.line
                    })
                    .bg(c.s1)
                    .text_spec(&theme.text.secondary)
                    .child(icon(SbIcon::Filter, theme.sizes.icon_sm, c.fg3))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(input).appearance(false).px_0()),
                    )
                    .automation_target("unsorted/filter/field")
            }))
            .child(
                div()
                    .flex_none()
                    .mx(theme.spacing.xs)
                    .text_color(c.fg2)
                    .text_spec(&theme.text.number)
                    .child(summary)
                    .automation_target("unsorted/summary"),
            )
            .child(chip(
                "location",
                view.location.as_ref().map_or_else(
                    || "Location".to_string(),
                    |id| format!("Location: {}", location_name(id)),
                ),
                view.location.is_some(),
                location_entries,
                Box::new(|view| view.location = None),
            ))
            .child(chip(
                "agent",
                view.agent.as_ref().map_or_else(
                    || "Agent".to_string(),
                    |id| format!("Agent: {}", agents::name(id, cx)),
                ),
                view.agent.is_some(),
                agent_entries,
                Box::new(|view| view.agent = None),
            ))
            .child(div().flex_1())
            .child(chip(
                "sort",
                view.sort.label().to_string(),
                false,
                sort_entries,
                Box::new(|_| {}),
            ))
    }
}

impl Render for UnsortedView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.sb_theme().clone();
        // Where each row is drawn is learnt again as this frame draws it.
        self.drawn.borrow_mut().clear();
        if self.filter.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter in Unsorted"));
            let heard = cx.subscribe(&input, |this, input, event, cx| {
                if let InputEvent::Change = event {
                    let filter = input.read(cx).value().to_string();
                    this.library.update(cx, |library, cx| {
                        let view = GroupsView {
                            filter,
                            ..library.unsorted_view().clone()
                        };
                        library.set_unsorted_view(view, cx);
                    });
                }
            });
            self.filter = Some((input, heard));
        }
        self.lay_out(&theme, cx);
        let library = self.library.read(cx);
        let loaded = library.page().is_some();
        let groups = library.groups();
        let summary = format!(
            "{} · {}",
            format::plural(self.paths, "path"),
            format::plural(self.left, "session")
        );
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
                this.child(self.filter_row(summary, window, &theme, cx))
                    .child(
                        /* The list reaches the main area's edge, and its scrollbar sits in the
                        page's side padding, clear of the rows ([`Self::render_items`]). */
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .mr(-theme.sizes.page_side)
                            .child(
                                v_virtual_list(
                                    cx.entity(),
                                    "groups",
                                    self.sizes.clone(),
                                    Self::render_items,
                                )
                                .track_scroll(&self.scroll),
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
        /* Seven sessions with five showing, two with both, three closed, one refused; and a
        group sorted ahead whose filters leave two of its sessions. */
        assert_eq!(
            items([
                (0, (0..7).collect(), 5, false),
                (4, vec![1, 3], 2, false),
                (1, (0..2).collect(), 2, false),
                (2, (0..3).collect(), 0, false),
                (3, vec![0], 1, true),
            ]),
            [
                Item::Head(0),
                Item::Row(0, 0),
                Item::Row(0, 1),
                Item::Row(0, 2),
                Item::Row(0, 3),
                Item::Row(0, 4),
                Item::ShowAll(0, 7),
                Item::Head(4),
                Item::Row(4, 1),
                Item::Row(4, 3),
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
