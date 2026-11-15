//! Unsorted (DESIGN §4.6): the sessions no folder claims, grouped by the project path and the
//! location they started at. Each group can become a folder: **Create folder from this path**
//! makes one named after the path's last component and attaches the path to it, which files
//! the group there (PLAN §8).
//!
//! The groups are a virtual list: a group's height follows from how many of its sessions show
//! (five, until "Show all" is asked for; none while it is closed).
//!
//! **Keyboard**: `↑` and `↓` move over the sessions that show, `Enter` opens the selected one.
//!
//! Attaching a group's path to an existing folder, dragging its chip, the filter and the
//! sort come with later tasks.

use std::collections::HashSet;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AnyElement, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Render, Role, ScrollStrategy, SharedString, Size,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px, size,
};
use gpui_component::scroll::Scrollbar;
use gpui_component::{VirtualListScrollHandle, v_virtual_list};
use sb_core::catalogue::{PathGroup, Session};
use sb_core::filing::NormalPath;

use super::header::{Under, header};
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{ButtonKind, button, icon, location_tag};
use crate::format;
use crate::icons::SbIcon;
use crate::services::{ActiveServices as _, LibraryService, Page, SyncService};
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _};

/// How many of a group's sessions show until all are asked for (DESIGN §4.6).
const SHOWN: usize = 5;

const EVERYTHING_FILED: &str = "Everything is filed.";

/// What tells one group from another across loads: its path and its first location.
fn key(group: &PathGroup) -> String {
    format!(
        "{}\n{}",
        group.project.as_ref().map_or("", NormalPath::as_str),
        group
            .locations
            .first()
            .map_or("", |location| location.as_str())
    )
}

pub struct UnsortedView {
    library: Entity<LibraryService>,
    sync: Entity<SyncService>,
    scroll: VirtualListScrollHandle,
    /// The groups that show all their sessions, and those that show none, by [`key`].
    all: HashSet<String>,
    closed: HashSet<String>,
    sizes: Rc<Vec<Size<Pixels>>>,
    /// The sizes are of what the library held before it last changed.
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
            sizes: Rc::default(),
            stale: true,
        }
    }

    /// How many of a group's sessions show.
    fn shown(&self, group: &PathGroup) -> usize {
        let key = key(group);
        if self.closed.contains(&key) {
            0
        } else if self.all.contains(&key) {
            group.sessions.len()
        } else {
            group.sessions.len().min(SHOWN)
        }
    }

    fn groups<'a>(&self, cx: &'a Context<Self>) -> &'a [PathGroup] {
        match self.library.read(cx).page() {
            Some(Page::Unsorted(groups)) => groups,
            _ => &[],
        }
    }

    /// Each group's height with the gap under it: its 44px head, a 34px row per session that
    /// shows, one more for "Show all", inside its border.
    fn lay_out(&mut self, theme: &SbTheme, cx: &Context<Self>) {
        if !self.stale {
            return;
        }
        self.stale = false;
        let sizes = self
            .groups(cx)
            .iter()
            .map(|group| {
                let shown = self.shown(group);
                let rows = shown + usize::from(shown > 0 && shown < group.sessions.len());
                size(
                    px(0.),
                    theme.sizes.group_head_height
                        + theme.sizes.list_row_height * rows as f32
                        + px(2.)
                        + theme.spacing.lg,
                )
            })
            .collect();
        self.sizes = Rc::new(sizes);
    }

    fn toggle(&mut self, group: usize, cx: &mut Context<Self>) {
        let Some(key) = self.groups(cx).get(group).map(key) else {
            return;
        };
        if !self.closed.remove(&key) {
            self.closed.insert(key);
        }
        self.stale = true;
        cx.notify();
    }

    fn show_all(&mut self, group: usize, cx: &mut Context<Self>) {
        let Some(key) = self.groups(cx).get(group).map(key) else {
            return;
        };
        self.all.insert(key);
        self.stale = true;
        cx.notify();
    }

    fn create_folder(&mut self, group: usize, cx: &mut Context<Self>) {
        self.library.update(cx, |library, cx| {
            if let Some(Page::Unsorted(groups)) = library.page()
                && let Some(group) = groups.get(group).cloned()
            {
                library.create_folder_from(&group, cx);
            }
        });
    }

    /// The sessions that show, top to bottom, each as its group and its place in it.
    fn visible(&self, cx: &Context<Self>) -> Vec<(usize, usize)> {
        self.groups(cx)
            .iter()
            .enumerate()
            .flat_map(|(at, group)| (0..self.shown(group)).map(move |row| (at, row)))
            .collect()
    }

    /// Moves the selection over the sessions that show; with none selected, to the first.
    pub fn step(&mut self, by: isize, cx: &mut Context<Self>) {
        let visible = self.visible(cx);
        let groups = self.groups(cx);
        let selected = self.library.read(cx).selected();
        let at = selected.and_then(|selected| {
            visible
                .iter()
                .position(|(group, row)| groups[*group].sessions[*row].id == *selected)
        });
        let to = match at {
            Some(at) => at.checked_add_signed(by),
            None => Some(0),
        };
        let Some((group, row)) = to.and_then(|to| visible.get(to)).copied() else {
            return;
        };
        let id = groups[group].sessions[row].id.clone();
        self.scroll.scroll_to_item(group, ScrollStrategy::Nearest);
        self.library
            .update(cx, |library, cx| library.select(id, cx));
    }

    /// Opens the selected session.
    pub fn open_selected(&mut self, cx: &mut Context<Self>) {
        let Some(selected) = self.library.read(cx).selected() else {
            return;
        };
        let session = self
            .groups(cx)
            .iter()
            .flat_map(|group| &group.sessions)
            .find(|session| session.id == *selected)
            .cloned();
        if let Some(session) = session {
            self.library
                .update(cx, |library, cx| library.open(session, cx));
        }
    }

    fn render_groups(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = cx.sb_theme().clone();
        let now = format::now();
        range
            .filter_map(|at| Some((at, self.groups(cx).get(at)?)))
            .map(|(at, group)| self.group(at, group, now, &theme, cx))
            .collect()
    }

    /// One group (DESIGN §4.6): a bordered section whose head holds the path's chip, how many
    /// sessions it has and what can be done with it, over its sessions as list rows.
    fn group(
        &self,
        at: usize,
        group: &PathGroup,
        now: i64,
        theme: &SbTheme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let sync = self.sync.read(cx);
        let shown = self.shown(group);
        let open = shown > 0;
        let path = group.spellings.first().cloned();
        let head = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .h(theme.sizes.group_head_height)
            .pl(theme.spacing.sm)
            .pr(px(10.))
            .bg(c.s1)
            .border_b_1()
            .border_color(c.line_subtle)
            .child(
                div()
                    .id(("group-toggle", at))
                    .role(Role::Button)
                    .aria_label(if open {
                        "Hide sessions"
                    } else {
                        "Show sessions"
                    })
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(theme.sizes.control_small_height)
                    .rounded(theme.radii.control)
                    .cursor_pointer()
                    .hover(|style| style.bg(c.s2))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(at, cx)))
                    .child(icon(
                        if open {
                            SbIcon::ChevronDown
                        } else {
                            SbIcon::ChevronRight
                        },
                        px(14.),
                        c.fg2,
                    ))
                    .automation_target_with(move || format!("unsorted/group/{at}/toggle").into()),
            )
            .child(
                // The path's chip. It becomes what is dragged onto a folder.
                div()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.sm)
                    .h(theme.sizes.path_chip_height)
                    .pl(theme.spacing.xs)
                    .pr(px(10.))
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
                                location_tag(location.name.clone(), location.kind, false, theme)
                            }),
                    )
                    .child(match &path {
                        Some(path) => div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_spec(&theme.text.path)
                            .child(path.clone()),
                        None => div()
                            .text_color(c.fg2)
                            .text_spec(&theme.text.secondary)
                            .child("No project path"),
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(c.fg3)
                    .text_spec(&theme.text.number)
                    .child(format::plural(group.sessions.len(), "session")),
            )
            .when(group.spellings.len() > 1, |this| {
                this.child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_color(c.fg3)
                        .text_spec(&theme.text.secondary)
                        .child(format!("also as {}", group.spellings[1..].join(", "))),
                )
            })
            .child(div().flex_1())
            .when(path.is_some(), |this| {
                this.child(
                    button(("create-folder", at), ButtonKind::Plain, true, true, theme)
                        .on_click(cx.listener(move |this, _, _, cx| this.create_folder(at, cx)))
                        .child(icon(SbIcon::FolderPlus, px(14.), c.fg))
                        .child("Create folder from this path")
                        .automation_target_with(move || {
                            format!("unsorted/group/{at}/create-folder").into()
                        }),
                )
            });
        // The list gives the group its height: the section and the 12px gap under it.
        div()
            .w_full()
            .pb(theme.spacing.lg)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .rounded(theme.radii.card)
                    .border_1()
                    .border_color(c.line_subtle)
                    .overflow_hidden()
                    .child(head)
                    .children(
                        group.sessions[..shown]
                            .iter()
                            .enumerate()
                            .map(|(row, session)| self.row(at, row, session, now, theme, cx)),
                    )
                    .when(open && shown < group.sessions.len(), |this| {
                        this.child(
                            div()
                                .id(("show-all", at))
                                .role(Role::Button)
                                .flex()
                                .items_center()
                                .h(theme.sizes.list_row_height)
                                .px(px(10.))
                                .text_color(c.accent_text)
                                .text_spec(&theme.text.row)
                                .cursor_pointer()
                                .hover(|style| style.bg(c.s1))
                                .on_click(cx.listener(move |this, _, _, cx| this.show_all(at, cx)))
                                .child(format!("Show all {}", group.sessions.len()))
                                .automation_target_with(move || {
                                    format!("unsorted/group/{at}/show-all").into()
                                }),
                        )
                    })
                    .automation_target_with(move || format!("unsorted/group/{at}").into()),
            )
            .into_any_element()
    }

    /// A session's row (DESIGN §4.6): its title and the start of its prompt, its agent's mark
    /// and model, when it ended, and its tool calls.
    fn row(
        &self,
        group: usize,
        row: usize,
        session: &Session,
        now: i64,
        theme: &SbTheme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let library = self.library.read(cx);
        let selected = library.selected() == Some(&session.id);
        let title: SharedString = session
            .title
            .clone()
            .unwrap_or_else(|| "Untitled session".to_string())
            .into();
        let model = agents::model(&session.id.provider, &session.models, cx)
            .unwrap_or_else(|| agents::name(&session.id.provider, cx));
        let (id, opened) = (session.id.clone(), session.clone());
        div()
            .id(("row", group * 100_000 + row))
            .role(Role::ListItem)
            .flex()
            .items_center()
            .gap(theme.spacing.lg)
            .h(theme.sizes.list_row_height)
            .px(px(10.))
            .border_b_1()
            .border_color(c.line_subtle)
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
                let (id, opened) = (id.clone(), opened.clone());
                this.library.update(cx, |library, cx| {
                    library.select(id, cx);
                    if event.click_count() > 1 {
                        library.open(opened, cx);
                    }
                });
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
                    .w(px(96.))
                    .flex()
                    .items_center()
                    .gap(theme.spacing.sm)
                    .overflow_hidden()
                    .text_color(c.fg2)
                    .child(agents::mark(&session.id.provider, px(12.), cx))
                    .child(model),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(84.))
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
                    .w(px(64.))
                    .flex()
                    .justify_end()
                    .text_color(c.fg2)
                    .text_spec(&theme.text.number)
                    .child(format::plural(session.all.calls as usize, "call")),
            )
            .automation_target_with(move || format!("unsorted/row/{group}/{row}").into())
    }
}

impl Render for UnsortedView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.sb_theme().clone();
        self.lay_out(&theme, cx);
        let library = self.library.read(cx);
        let loaded = matches!(library.page(), Some(Page::Unsorted(_)));
        let groups = self.groups(cx);
        let sessions: usize = groups.iter().map(|group| group.sessions.len()).sum();
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(px(14.))
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
                    div()
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .child(
                            v_virtual_list(
                                cx.entity(),
                                "groups",
                                self.sizes.clone(),
                                Self::render_groups,
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
