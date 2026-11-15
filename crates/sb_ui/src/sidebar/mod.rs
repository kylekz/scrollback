//! The sidebar (PLAN §11 `sidebar`, DESIGN §4.1): the library rows with their counts, the
//! folder tree, and the locations with their sync state at its foot. A view over the
//! `Library` and `Sync` services.
//!
//! Until there is a location it draws its first-run state (DESIGN §4.10): the library rows
//! disabled with counts of 0, no folders, no locations and no section buttons.
//!
//! **The selected row** is the page shown: `↑` and `↓` show the page of the row above or
//! below, `←` and `→` close and open a folder, and `Enter` hands the keyboard to the page.
//! Trash has no page until the phase that builds it (PLAN §13), so its row shows its count of
//! nothing and can't be picked.
//!
//! **Renaming** (`F2`, and every new folder) happens in the folder's row: `Enter` keeps the
//! name, `Esc` leaves the folder as it was named.
//!
//! The tree is a `uniform_list`, so a thousand folders cost what the visible rows do. Drag
//! and drop arrives with the next task; [`tree::rows`] is where a drop finds its row.

mod locations;
pub mod tree;

use std::collections::HashSet;
use std::ops::Range;
use std::time::Duration;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Div, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Render, Role,
    ScrollStrategy, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task,
    UniformListScrollHandle, Window, div, prelude::FluentBuilder as _, px, uniform_list,
};
use gpui_component::input::{Input, InputEvent, InputState};
use sb_core::catalogue::Shelf;
use sb_core::filing::FolderId;

use crate::actions::sidebar::{Collapse, DropName, Expand, KeepName, Next, Open, Previous, Rename};
use crate::automation::AutomationTargetExt as _;
use crate::components::{count, icon, overline, sync_glyph};
use crate::icons::SbIcon;
use crate::keymap;
use crate::services::library::LibraryEvent;
use crate::services::{ActiveServices as _, LibraryService, SyncService};
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _};
use tree::TreeRow;

/// How the library rows are drawn while they can't be opened (DESIGN §4.10).
const DISABLED_ROWS_OPACITY: f32 = 0.55;

/// The library rows, top to bottom (DESIGN §4.1): icon, name, automation target and the page
/// each shows. Trash has none yet (module docs).
const LIBRARY_ROWS: [(SbIcon, &str, &str, Option<Shelf>); 4] = [
    (
        SbIcon::Layers,
        "All sessions",
        "sidebar/all-sessions",
        Some(Shelf::All),
    ),
    (
        SbIcon::Star,
        "Starred",
        "sidebar/starred",
        Some(Shelf::Starred),
    ),
    (
        SbIcon::Inbox,
        "Unsorted",
        "sidebar/unsorted",
        Some(Shelf::Unsorted),
    ),
    (SbIcon::Trash, "Trash", "sidebar/trash", None),
];

/// What the Folders and Locations sections say while they have no rows (DESIGN §4.10, §4.14).
const NO_FOLDERS_FIRST_RUN: &str = "Folders come after the first sync.";
const NO_FOLDERS: &str =
    "None yet. Unsorted groups sessions by project path; make folders from there.";
const NO_LOCATIONS: &str = "None yet";

/// What a folder is called until the owner names it.
const NEW_FOLDER_NAME: &str = "New folder";

/// How often the locations' "synced 2 min ago" is drawn again.
const CLOCK_TICK: Duration = Duration::from_secs(30);

pub enum SidebarEvent {
    /// A row was picked: a narrow window's overlay closes (DESIGN §2).
    Picked,
    /// The keyboard goes to the page shown.
    Open,
}

/// A folder being renamed in its row.
struct Renaming {
    folder: FolderId,
    input: Entity<InputState>,
    _input: Subscription,
}

pub struct Sidebar {
    library: Entity<LibraryService>,
    sync: Entity<SyncService>,
    focus_handle: FocusHandle,
    /// The folders that show their children.
    open: HashSet<FolderId>,
    /// The tree's rows, as of the library's folders and `open`.
    tree: Vec<TreeRow>,
    renaming: Option<Renaming>,
    scroll: UniformListScrollHandle,
    _observers: [Subscription; 3],
    _clock: Task<()>,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

impl Focusable for Sidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Sidebar {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let services = cx.services();
        let (library, sync) = (services.library.clone(), services.sync.clone());
        Self {
            _observers: [
                cx.observe(&library, |this, _, cx| {
                    this.rebuild(cx);
                    cx.notify();
                }),
                cx.observe(&sync, |_, _, cx| cx.notify()),
                cx.subscribe_in(&library, window, |this, _, event, window, cx| match event {
                    LibraryEvent::FolderCreated(folder) => this.rename(*folder, window, cx),
                }),
            ],
            _clock: cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(CLOCK_TICK).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }),
            library,
            sync,
            focus_handle: cx.focus_handle(),
            open: HashSet::new(),
            tree: Vec::new(),
            renaming: None,
            scroll: UniformListScrollHandle::new(),
        }
    }

    /// Whether there is a location, and so a library to show (DESIGN §4.10).
    fn has_library(&self, cx: &App) -> bool {
        self.sync
            .read(cx)
            .locations()
            .is_some_and(|locations| !locations.is_empty())
    }

    fn rebuild(&mut self, cx: &App) {
        self.tree = tree::rows(self.library.read(cx).folders(), &self.open);
    }

    /// The pages the rows show, top to bottom.
    fn shelves(&self, cx: &App) -> Vec<Shelf> {
        let folders = self.library.read(cx).folders();
        LIBRARY_ROWS
            .iter()
            .filter_map(|(_, _, _, shelf)| *shelf)
            .chain(
                self.tree
                    .iter()
                    .map(|row| Shelf::Folder(folders[row.folder].id)),
            )
            .collect()
    }

    fn show(&mut self, shelf: Shelf, cx: &mut Context<Self>) {
        self.library
            .update(cx, |library, cx| library.show(shelf, cx));
        if let Shelf::Folder(folder) = shelf
            && let Some(at) = self.row_of(folder, cx)
        {
            self.scroll.scroll_to_item(at, ScrollStrategy::Nearest);
        }
    }

    fn row_of(&self, folder: FolderId, cx: &App) -> Option<usize> {
        let folders = self.library.read(cx).folders();
        self.tree
            .iter()
            .position(|row| folders[row.folder].id == folder)
    }

    fn step(&mut self, by: isize, cx: &mut Context<Self>) {
        let shelves = self.shelves(cx);
        let shown = self.library.read(cx).shelf();
        let Some(at) = shelves.iter().position(|shelf| *shelf == shown) else {
            return;
        };
        if let Some(next) = at.checked_add_signed(by).and_then(|at| shelves.get(at)) {
            self.show(*next, cx);
        }
    }

    fn previous(&mut self, _: &Previous, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-1, cx);
    }

    fn next(&mut self, _: &Next, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, cx);
    }

    /// The row of the folder whose page is shown.
    fn shown_row(&self, cx: &App) -> Option<(FolderId, TreeRow)> {
        let Shelf::Folder(folder) = self.library.read(cx).shelf() else {
            return None;
        };
        Some((folder, self.tree[self.row_of(folder, cx)?]))
    }

    fn collapse(&mut self, _: &Collapse, _: &mut Window, cx: &mut Context<Self>) {
        let Some((folder, row)) = self.shown_row(cx) else {
            return;
        };
        if row.open {
            self.toggle(folder, cx);
        } else if let Some(parent) = self
            .library
            .read(cx)
            .folder(folder)
            .and_then(|it| it.parent)
        {
            self.show(Shelf::Folder(parent), cx);
        }
    }

    fn expand(&mut self, _: &Expand, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((folder, row)) = self.shown_row(cx)
            && row.has_children
            && !row.open
        {
            self.toggle(folder, cx);
        }
    }

    fn open(&mut self, _: &Open, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(SidebarEvent::Open);
    }

    /// Opens or closes a folder without selecting it.
    fn toggle(&mut self, folder: FolderId, cx: &mut Context<Self>) {
        if !self.open.remove(&folder) {
            self.open.insert(folder);
        }
        self.rebuild(cx);
        cx.notify();
    }

    /// Makes a folder and renames it in place once it is there: at the top, or with `inside`
    /// in the folder whose page is shown (DESIGN §5: `Shift` on the `+`).
    pub fn new_folder(&mut self, inside: bool, cx: &mut Context<Self>) {
        if !self.has_library(cx) {
            return;
        }
        let parent = match self.library.read(cx).shelf() {
            Shelf::Folder(folder) if inside => Some(folder),
            _ => None,
        };
        self.library.update(cx, |library, cx| {
            library.create_folder(NEW_FOLDER_NAME.to_string(), parent, cx);
        });
    }

    fn rename_shown(&mut self, _: &Rename, window: &mut Window, cx: &mut Context<Self>) {
        if let Shelf::Folder(folder) = self.library.read(cx).shelf() {
            self.rename(folder, window, cx);
        }
    }

    /// Starts renaming `folder` in its row, with its name selected.
    fn rename(&mut self, folder: FolderId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self
            .library
            .read(cx)
            .folder(folder)
            .map(|it| it.name.clone())
        else {
            return;
        };
        // A folder inside closed folders has no row to rename it in.
        self.open
            .extend(tree::ancestors(self.library.read(cx).folders(), folder));
        self.rebuild(cx);
        if let Some(at) = self.row_of(folder, cx) {
            self.scroll.scroll_to_item(at, ScrollStrategy::Nearest);
        }
        let input = cx.new(|cx| InputState::new(window, cx).default_value(name));
        input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        self.renaming = Some(Renaming {
            folder,
            // Enter keeps the name, and so does leaving the row.
            _input: cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                InputEvent::PressEnter { .. } | InputEvent::Blur => {
                    this.end_rename(true, window, cx);
                }
                InputEvent::Change | InputEvent::Focus => {}
            }),
            input,
        });
        cx.notify();
    }

    /// Ends a rename: with `keep`, the folder takes what was typed, unless that is nothing
    /// or its name already. The keyboard goes back to the sidebar if the input had it.
    fn end_rename(&mut self, keep: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(renaming) = self.renaming.take() else {
            return;
        };
        if renaming.input.focus_handle(cx).is_focused(window) {
            self.focus_handle.focus(window, cx);
        }
        let typed = renaming.input.read(cx).value();
        let name = typed.trim();
        let unchanged = self
            .library
            .read(cx)
            .folder(renaming.folder)
            .is_none_or(|folder| folder.name == name);
        if keep && !name.is_empty() && !unchanged {
            let name = name.to_string();
            self.library.update(cx, |library, cx| {
                library.rename_folder(renaming.folder, name, cx);
            });
        }
        cx.notify();
    }

    /// A library row (DESIGN §4.1): 32 high, radius 8, icon `fg3`, name `fg2`, its count at
    /// the right; `s3` with the name in `fg` at 500 while its page is shown.
    fn library_row(
        &self,
        (glyph, name, target, shelf): (SbIcon, &'static str, &'static str, Option<Shelf>),
        enabled: bool,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let library = self.library.read(cx);
        let counts = library.counts();
        let value = match shelf {
            Some(Shelf::All) => counts.all,
            Some(Shelf::Starred) => counts.starred,
            Some(Shelf::Unsorted) => counts.unsorted,
            Some(Shelf::Folder(_)) | None => 0,
        };
        let selected = enabled && shelf == Some(library.highlighted());
        let count_color = if selected {
            c.fg2
        } else if shelf == Some(Shelf::Unsorted) && value > 0 {
            c.accent_text
        } else {
            c.fg3
        };
        row_base(selected, theme)
            .id(target)
            .role(Role::ListItem)
            .when_some(shelf.filter(|_| enabled), |this, shelf| {
                this.cursor_pointer()
                    .when(!selected, |this| this.hover(|style| style.bg(c.s2)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.show(shelf, cx);
                        cx.emit(SidebarEvent::Picked);
                    }))
            })
            .child(icon(glyph, px(16.), if selected { c.fg2 } else { c.fg3 }))
            .child(name)
            .child(count(value, count_color, theme))
            .automation_target(target)
    }

    /// A folder's row: indented 18 per level with a 1px `line` guide, a chevron when it has
    /// folders in it (a click on it opens or closes the folder without selecting it), the
    /// folder glyph, its name and the count of its sessions and its folders' (DESIGN §4.1).
    fn folder_row(&self, row: TreeRow, theme: &SbTheme, cx: &mut Context<Self>) -> AnyElement {
        let c = &theme.colors;
        let library = self.library.read(cx);
        let folder = &library.folders()[row.folder];
        let id = folder.id;
        let name: SharedString = folder.name.clone().into();
        let selected = library.highlighted() == Shelf::Folder(id);
        let sessions = library.counts().folders.get(&id).copied().unwrap_or(0);
        let renaming = self
            .renaming
            .as_ref()
            .filter(|renaming| renaming.folder == id)
            .map(|renaming| renaming.input.clone());
        let chevron = row.has_children.then(|| {
            div()
                .id(("folder-chevron", row.folder))
                .role(Role::Button)
                .aria_label(if row.open {
                    "Close folder"
                } else {
                    "Open folder"
                })
                .flex()
                .items_center()
                .justify_center()
                .size_full()
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle(id, cx);
                }))
                .child(icon(
                    if row.open {
                        SbIcon::ChevronDown
                    } else {
                        SbIcon::ChevronRight
                    },
                    px(12.),
                    c.fg3,
                ))
                .automation_target_with({
                    let name = name.clone();
                    move || format!("sidebar/folder-chevron/{name}").into()
                })
        });
        // The list gives every row the same height: the row and the 1px gap under it.
        div()
            .h(theme.sizes.sidebar_row_height + px(1.))
            .child(
                row_base(selected, theme)
                    .id(("folder", row.folder))
                    .role(Role::ListItem)
                    .cursor_pointer()
                    .when(!selected, |this| this.hover(|style| style.bg(c.s2)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.show(Shelf::Folder(id), cx);
                        cx.emit(SidebarEvent::Picked);
                    }))
                    .children((0..row.depth).map(|_| {
                        div()
                            .flex_none()
                            .h_full()
                            .w(theme.sizes.folder_indent)
                            .pl(theme.spacing.md)
                            .child(div().h_full().w(px(1.)).bg(c.line))
                    }))
                    .when(row.chevron_room, |this| {
                        this.child(div().flex_none().size(px(12.)).children(chevron))
                    })
                    .child(icon(
                        SbIcon::Folder,
                        px(16.),
                        if selected { c.fg2 } else { c.fg3 },
                    ))
                    .map(|this| match renaming {
                        Some(input) => this.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                // The input passes Enter and Esc on. They end here, so
                                // that Enter isn't also the sidebar's key for the page.
                                .key_context(keymap::FOLDER_RENAME)
                                .on_action(cx.listener(|this, _: &KeepName, window, cx| {
                                    this.end_rename(true, window, cx);
                                }))
                                .on_action(cx.listener(|this, _: &DropName, window, cx| {
                                    this.end_rename(false, window, cx);
                                }))
                                .child(
                                    Input::new(&input)
                                        .appearance(false)
                                        .aria_label("Folder name")
                                        .px_0(),
                                )
                                .automation_target("sidebar/folder-rename"),
                        ),
                        None => this
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .child(name.clone()),
                            )
                            .child(count(sessions, if selected { c.fg2 } else { c.fg3 }, theme)),
                    })
                    .automation_target_with(move || format!("sidebar/folder/{name}").into()),
            )
            .into_any_element()
    }

    fn folder_rows(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = cx.sb_theme().clone();
        range
            .filter_map(|at| self.tree.get(at).copied())
            .map(|row| self.folder_row(row, &theme, cx))
            .collect()
    }
}

/// A row of the sidebar: 32 high with its 8px padding and radius, on `s3` while selected.
fn row_base(selected: bool, theme: &SbTheme) -> Div {
    let c = &theme.colors;
    div()
        .flex()
        .items_center()
        .gap(theme.spacing.md)
        .h(theme.sizes.sidebar_row_height)
        .px(theme.spacing.md)
        .rounded(theme.radii.row)
        .whitespace_nowrap()
        .map(|this| {
            if selected {
                this.bg(c.s3)
                    .text_color(c.fg)
                    .font_weight(theme.text.control.weight)
            } else {
                this.text_color(c.fg2)
            }
        })
}

/// The row a section's overline and its button sit in.
fn section_header(theme: &SbTheme) -> Div {
    div().flex().items_center().px(theme.spacing.md)
}

/// A section's button at the right of its overline (22 square).
fn header_button(
    glyph: impl IntoElement,
    label: String,
    target: &'static str,
    theme: &SbTheme,
) -> gpui::Stateful<Div> {
    let c = &theme.colors;
    div()
        .id(target)
        .role(Role::Button)
        .aria_label(label)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .ml_auto()
        .size(theme.sizes.section_button)
        .rounded(theme.radii.control)
        .cursor_pointer()
        .hover(|style| style.bg(c.s2))
        .child(glyph)
}

/// What an empty section says in place of its rows.
fn note(text: &'static str, theme: &SbTheme) -> impl IntoElement {
    div()
        .px(theme.spacing.md)
        .text_color(theme.colors.fg3)
        .text_spec(&theme.text.secondary)
        .child(text)
}

impl Render for Sidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.sb_theme().clone();
        let c = &theme.colors;
        let has_library = self.has_library(cx);
        let syncing = self.sync.read(cx).is_syncing();
        div()
            .id("sidebar-rows")
            .key_context(keymap::SIDEBAR)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::previous))
            .on_action(cx.listener(Self::next))
            .on_action(cx.listener(Self::collapse))
            .on_action(cx.listener(Self::expand))
            .on_action(cx.listener(Self::open))
            .on_action(cx.listener(Self::rename_shown))
            .flex()
            .flex_col()
            .size_full()
            .pt(px(10.))
            .px(theme.spacing.md)
            .pb(theme.spacing.md)
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .when(!has_library, |this| this.opacity(DISABLED_ROWS_OPACITY))
                    .children(
                        LIBRARY_ROWS
                            .into_iter()
                            .map(|row| self.library_row(row, has_library, &theme, cx)),
                    ),
            )
            .child(
                section_header(&theme)
                    .flex_none()
                    .pt(theme.spacing.xl)
                    .pb(theme.spacing.sm)
                    // The row is as high with its button as without.
                    .min_h(theme.spacing.xl + theme.spacing.sm + theme.sizes.section_button)
                    .child(overline("Folders", &theme))
                    .when(has_library, |this| {
                        this.child(
                            header_button(
                                icon(SbIcon::Plus, px(14.), c.fg2),
                                format!("New folder ({})", keymap::NEW_FOLDER.label()),
                                "sidebar/new-folder",
                                &theme,
                            )
                            .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                                this.new_folder(event.modifiers().shift, cx);
                            }))
                            .automation_target("sidebar/new-folder"),
                        )
                    }),
            )
            .map(|this| {
                if !has_library {
                    this.child(note(NO_FOLDERS_FIRST_RUN, &theme))
                        .child(div().flex_1())
                } else if self.tree.is_empty() {
                    this.child(note(NO_FOLDERS, &theme)).child(div().flex_1())
                } else {
                    this.child(
                        uniform_list(
                            "sidebar-folders",
                            self.tree.len(),
                            cx.processor(Self::folder_rows),
                        )
                        .track_scroll(&self.scroll)
                        .flex_1()
                        .min_h_0(),
                    )
                }
            })
            .child(
                div()
                    .flex_none()
                    .h(px(1.))
                    .mx(-theme.spacing.md)
                    .mb(theme.spacing.md)
                    .bg(c.line_subtle),
            )
            .child(
                section_header(&theme)
                    .flex_none()
                    .pb(theme.spacing.xs)
                    .min_h(theme.spacing.xs + theme.sizes.section_button)
                    .child(overline("Locations", &theme))
                    .when(has_library, |this| {
                        this.child(
                            header_button(
                                sync_glyph(syncing, c.fg2),
                                format!("Sync all locations ({})", keymap::SYNC_ALL.label()),
                                "sidebar/sync-all",
                                &theme,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sync.update(cx, |sync, cx| sync.sync_all(cx));
                            }))
                            .automation_target("sidebar/sync-all"),
                        )
                    }),
            )
            .child(self.render_locations(&theme, cx))
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn the_rows_and_the_notes_read_as_design_writes_them() {
        // DESIGN §4.1: the library rows top to bottom, each with its icon (§1.5).
        assert_eq!(
            LIBRARY_ROWS.map(|(glyph, name, target, _)| (glyph, name, target)),
            [
                (SbIcon::Layers, "All sessions", "sidebar/all-sessions"),
                (SbIcon::Star, "Starred", "sidebar/starred"),
                (SbIcon::Inbox, "Unsorted", "sidebar/unsorted"),
                (SbIcon::Trash, "Trash", "sidebar/trash"),
            ]
        );
        // DESIGN §4.10 and §4.14.
        assert_eq!(NO_FOLDERS_FIRST_RUN, "Folders come after the first sync.");
        assert_eq!(
            NO_FOLDERS,
            "None yet. Unsorted groups sessions by project path; make folders from there."
        );
        assert_eq!(NO_LOCATIONS, "None yet");
    }
}
