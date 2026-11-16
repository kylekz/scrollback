//! A folder's paths editor, in place on its page (DESIGN §4.7, `E-Folder-Paths`): the paths
//! attached to it in a table, each with its location scope and the sessions it files; the
//! changes made since it opened, each with Undo and what it would move; an add row that
//! completes from the paths the locations have seen and refuses a path that belongs elsewhere;
//! up to five paths suggested from Unsorted; and a footer that says what Save will do.
//! `Ctrl Enter` saves and `Esc` drops the changes; leaving the page with changes asks first.
//!
//! An empty folder's page opens it by default in its short form (`E-Edge-Empty-Folder`): the
//! add row and the suggestions alone, where adding a path attaches it at once, with a toast
//! that undoes it.
//!
//! **What a change would move** is worked out here, from what the editor read when it opened
//! (`sb_core`'s `fileable` and `project_paths`) and the paths every folder has, by the rules
//! themselves ([`filing::file`]), so it is what Save does.

use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, Bounds, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, anchored, canvas, deferred,
    div, point, prelude::FluentBuilder as _, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use sb_core::catalogue::{FolderPath, ProjectPath};
use sb_core::filing::{self, AttachedPath, Fileable, FolderId, NormalPath};
use sb_core::location::LocationId;

use crate::actions::sessions::{CancelPaths, SavePaths};
use crate::automation::AutomationTargetExt as _;
use crate::components::{ButtonKind, Tone, button, icon, icon_button, location_tag, tag};
use crate::format;
use crate::icons::SbIcon;
use crate::keymap;
use crate::menu::{self, Entry, Item};
use crate::services::{ActiveServices as _, LibraryService, SyncService, TypedPath};
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _};

/// How many paths Suggested from Unsorted lists, and how many completions show.
const SUGGESTIONS: usize = 5;
const COMPLETIONS: usize = 6;

/// The table's column widths (`E-Folder-Paths`): the scope, the count, Remove.
const SCOPE_WIDTH: Pixels = px(150.);
const COUNT_WIDTH: Pixels = px(120.);
const REMOVE_WIDTH: Pixels = px(24.);
const ROW_HEIGHT: Pixels = px(40.);
const HEAD_HEIGHT: Pixels = px(28.);

pub enum PathsEditorEvent {
    /// The editor is done: saved, or its changes dropped.
    Closed,
}

/// What the editor read when it opened.
struct Read {
    fileable: Vec<Fileable>,
    seen: Vec<ProjectPath>,
}

/// One of the folder's paths, and whether it is to be taken off.
struct Kept {
    path: FolderPath,
    sessions: usize,
    removed: bool,
}

/// Why a path typed can't be added.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Refusal {
    /// It doesn't say where it is from the top.
    NoPath,
    /// This folder has it already, as typed so.
    Here(String),
    /// Another folder has it for the same locations.
    Elsewhere(FolderId),
}

pub struct PathsEditor {
    folder: FolderId,
    /// The short form of an empty folder (module docs).
    short: bool,
    kept: Vec<Kept>,
    added: Vec<TypedPath>,
    /// The location the add row's path is for; `None` for any.
    scope: Option<LocationId>,
    input: Entity<InputState>,
    read: Option<Rc<Read>>,
    /// Whether the add row takes the keyboard when it is first drawn: the full editor's does.
    focus_first: bool,
    /// Where the add row's input was drawn, for its completions.
    input_bounds: Rc<std::cell::Cell<Bounds<Pixels>>>,
    focus: FocusHandle,
    library: Entity<LibraryService>,
    sync: Entity<SyncService>,
    _subscriptions: [Subscription; 2],
}

impl EventEmitter<PathsEditorEvent> for PathsEditor {}

impl Focusable for PathsEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl PathsEditor {
    /// The editor of `folder`'s paths, as its page shows them now; `short` for an empty
    /// folder's.
    pub fn new(folder: FolderId, short: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let services = cx.services();
        let (library, sync) = (services.library.clone(), services.sync.clone());
        let archive = services.archive.read(cx).archive().cloned();
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Type a path, or pick one a location has seen")
        });
        let kept = library
            .read(cx)
            .attached()
            .iter()
            .filter(|path| path.attached.folder == folder)
            .map(|path| Kept {
                path: path.clone(),
                sessions: library
                    .read(cx)
                    .sessions()
                    .and_then(|page| {
                        page.paths.iter().find(|chip| {
                            chip.path == path.attached.path
                                && chip.location == path.attached.location
                        })
                    })
                    .map_or(0, |chip| chip.sessions),
                removed: false,
            })
            .collect();
        if let Some(archive) = archive {
            let job = cx.background_spawn(async move {
                let catalogue = archive.catalogue();
                Ok::<_, sb_core::catalogue::CatalogueError>(Read {
                    fileable: catalogue.fileable()?,
                    seen: catalogue.project_paths()?,
                })
            });
            cx.spawn(async move |this, cx| {
                if let Ok(read) = job.await {
                    this.update(cx, |this, cx| {
                        this.read = Some(Rc::new(read));
                        cx.notify();
                    })
                    .ok();
                }
            })
            .detach();
        }
        Self {
            _subscriptions: [
                cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                    InputEvent::PressEnter {
                        secondary: true, ..
                    } => this.save(window, cx),
                    InputEvent::PressEnter { .. } => this.add_typed(window, cx),
                    InputEvent::Change => cx.notify(),
                    _ => {}
                }),
                cx.observe(&library, |_, _, cx| cx.notify()),
            ],
            folder,
            short,
            kept,
            added: Vec::new(),
            scope: None,
            input,
            read: None,
            input_bounds: Rc::default(),
            focus_first: !short,
            focus: cx.focus_handle(),
            library,
            sync,
        }
    }

    /// The folder it edits.
    pub fn folder(&self) -> FolderId {
        self.folder
    }

    /// Whether it is an empty folder's short form.
    pub fn is_short(&self) -> bool {
        self.short
    }

    /// The paths to be added, as typed.
    pub fn added(&self) -> impl Iterator<Item = &str> {
        self.added.iter().map(|(typed, _)| typed.as_str())
    }

    /// The paths to be taken off, as typed.
    pub fn removed(&self) -> impl Iterator<Item = &str> {
        (self.kept.iter())
            .filter(|kept| kept.removed)
            .map(|kept| kept.path.typed.as_str())
    }

    /// Whether anything is to be saved.
    pub fn is_pending(&self) -> bool {
        !self.added.is_empty() || self.kept.iter().any(|kept| kept.removed)
    }

    /// The add row's input has the keyboard: single-letter keys are its.
    pub fn is_typing(&self, window: &Window, cx: &App) -> bool {
        self.input.focus_handle(cx).is_focused(window)
    }

    /// Puts the keyboard in the add row.
    pub fn focus_input(&self, window: &mut Window, cx: &mut App) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// The path the add row holds, as typed, when there is one.
    fn typed(&self, cx: &App) -> Option<String> {
        let value = self.input.read(cx).value();
        let typed = filing::as_typed(&value);
        (!typed.is_empty()).then(|| typed.to_string())
    }

    /// Where `location` mounts the Windows drives, for reading a path typed for it.
    fn mount(&self, location: Option<&LocationId>, cx: &App) -> Option<String> {
        self.sync.read(cx).location(location?)?.drive_mount.clone()
    }

    fn normal(&self, (typed, location): &TypedPath, cx: &App) -> Option<NormalPath> {
        NormalPath::new(
            filing::as_typed(typed),
            self.mount(location.as_ref(), cx).as_deref(),
        )
        .filter(NormalPath::is_absolute)
    }

    /// The folder's paths once the changes are saved, and every other folder's.
    fn after(&self, cx: &App) -> Vec<AttachedPath> {
        let library = self.library.read(cx);
        let mut after: Vec<AttachedPath> = (library.attached().iter())
            .filter(|path| path.attached.folder != self.folder)
            .map(|path| path.attached.clone())
            .collect();
        after.extend(
            (self.kept.iter())
                .filter(|kept| !kept.removed)
                .map(|kept| kept.path.attached.clone()),
        );
        after.extend(self.added.iter().filter_map(|added| {
            Some(AttachedPath {
                folder: self.folder,
                path: self.normal(added, cx)?,
                location: added.1.clone(),
            })
        }));
        after
    }

    /// Why `path` can't be added now, if it can't.
    fn refusal(&self, path: &TypedPath, cx: &App) -> Option<Refusal> {
        let Some(normal) = self.normal(path, cx) else {
            return Some(Refusal::NoPath);
        };
        let here = (self.kept.iter())
            .filter(|kept| !kept.removed)
            .map(|kept| (kept.path.attached.path.clone(), kept.path.typed.clone()))
            .chain(
                self.added
                    .iter()
                    .filter_map(|added| Some((self.normal(added, cx)?, added.0.clone()))),
            )
            .find(|(path, _)| *path == normal);
        if let Some((_, typed)) = here {
            return Some(Refusal::Here(typed));
        }
        (self.library.read(cx).attached().iter())
            .find(|it| {
                it.attached.folder != self.folder
                    && it.attached.path == normal
                    && it.attached.location == path.1
            })
            .map(|it| Refusal::Elsewhere(it.attached.folder))
    }

    /// Adds the path typed: as a change to save, or in the short form at once.
    fn add_typed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(typed) = self.typed(cx) else {
            return;
        };
        let path = (typed, self.scope.clone());
        if self.refusal(&path, cx).is_some() {
            return;
        }
        self.add(path, window, cx);
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
    }

    fn add(&mut self, path: TypedPath, _: &mut Window, cx: &mut Context<Self>) {
        if self.short {
            let sessions = self.moved_by(&path, cx).0;
            let folder = self.folder;
            self.library
                .update(cx, |library, cx| library.attach(folder, path, sessions, cx));
            cx.emit(PathsEditorEvent::Closed);
            return;
        }
        self.added.push(path);
        self.held(cx);
    }

    /// How many sessions `path`, added, would file here, and how many of them are in Unsorted.
    fn moved_by(&self, path: &TypedPath, cx: &App) -> (usize, usize) {
        let (Some(read), Some(normal)) = (&self.read, self.normal(path, cx)) else {
            return (0, 0);
        };
        let added = AttachedPath {
            folder: self.folder,
            path: normal,
            location: path.1.clone(),
        };
        let after = self.after(cx);
        let mut after = after;
        if !after.contains(&added) {
            after.push(added.clone());
        }
        let moved: Vec<&Fileable> = (read.fileable.iter())
            .filter(|session| session.folder != Some(self.folder))
            .filter(|session| {
                filing::file(&session.project, &session.location, &after) == Some(&added)
            })
            .collect();
        let unsorted = moved
            .iter()
            .filter(|session| session.folder.is_none())
            .count();
        (moved.len(), unsorted)
    }

    /// How many sessions the kept path `at` files now that would leave, and how many of those
    /// would go to Unsorted, once it is taken off.
    fn freed_by(&self, at: usize, cx: &App) -> (usize, usize) {
        let Some(read) = &self.read else {
            return (0, 0);
        };
        let before: Vec<AttachedPath> = (self.library.read(cx).attached().iter())
            .map(|path| path.attached.clone())
            .collect();
        let after = self.after(cx);
        let path = &self.kept[at].path.attached;
        let freed: Vec<Option<FolderId>> = (read.fileable.iter())
            .filter(|session| {
                filing::file(&session.project, &session.location, &before) == Some(path)
            })
            .map(|session| {
                filing::file(&session.project, &session.location, &after).map(|it| it.folder)
            })
            .filter(|to| *to != Some(self.folder))
            .collect();
        let unsorted = freed.iter().filter(|to| to.is_none()).count();
        (freed.len(), unsorted)
    }

    /**
     * What Save will do, in one line (DESIGN §4.7): "Saving files 3 sessions from Unsorted
     * into fc. Sessions you filed by hand stay where they are."
     */
    fn summary(&self, cx: &App) -> String {
        let name = self.library.read(cx).folder_path(self.folder);
        let Some(read) = &self.read else {
            return "Counting what saving moves…".to_string();
        };
        let before: Vec<AttachedPath> = (self.library.read(cx).attached().iter())
            .map(|path| path.attached.clone())
            .collect();
        let after = self.after(cx);
        let (mut into, mut from_unsorted, mut out, mut to_unsorted) = (0, 0, 0, 0);
        for session in &read.fileable {
            let to = filing::file(&session.project, &session.location, &after).map(|it| it.folder);
            let was =
                filing::file(&session.project, &session.location, &before).map(|it| it.folder);
            if to == Some(self.folder) && was != Some(self.folder) {
                into += 1;
                from_unsorted += usize::from(was.is_none());
            } else if was == Some(self.folder) && to != Some(self.folder) {
                out += 1;
                to_unsorted += usize::from(to.is_none());
            }
        }
        let into = match (into, from_unsorted) {
            (0, _) => None,
            (into, from) if from == into => Some(format!(
                "files {} from Unsorted into {name}",
                format::plural(into, "session")
            )),
            (into, _) => Some(format!(
                "files {} into {name}",
                format::plural(into, "session")
            )),
        };
        let out = match (out, to_unsorted) {
            (0, _) => None,
            (out, to) if to == out => Some(format!(
                "hands {} back to Unsorted",
                format::plural(out, "session")
            )),
            (out, _) => Some(format!(
                "moves {} to other folders",
                format::plural(out, "session")
            )),
        };
        let what = match (into, out) {
            (Some(into), Some(out)) => format!("Saving {into} and {out}."),
            (Some(one), None) | (None, Some(one)) => format!("Saving {one}."),
            (None, None) => "Saving moves no session.".to_string(),
        };
        format!("{what} Sessions you filed by hand stay where they are.")
    }

    /// Saves the changes in one change, with a toast that undoes them, and closes.
    pub fn save(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if !self.is_pending() {
            return self.cancel(cx);
        }
        let detach: Vec<TypedPath> = (self.kept.iter())
            .filter(|kept| kept.removed)
            .map(|kept| (kept.path.typed.clone(), kept.path.attached.location.clone()))
            .collect();
        let name = self.library.read(cx).folder_path(self.folder);
        let said = format!("Paths of {name} saved");
        let (folder, attach) = (self.folder, std::mem::take(&mut self.added));
        self.library.update(cx, |library, cx| {
            library.hold(None);
            library.change_paths(folder, detach, attach, said, cx);
        });
        cx.emit(PathsEditorEvent::Closed);
    }

    /// Drops the changes and closes.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        self.library.update(cx, |library, _| library.hold(None));
        cx.emit(PathsEditorEvent::Closed);
    }

    /// Tells the library whether leaving waits on the owner.
    fn held(&mut self, cx: &mut Context<Self>) {
        let folder = self.is_pending().then_some(self.folder);
        self.library.update(cx, |library, _| library.hold(folder));
        cx.notify();
    }

    /// The menu of scopes a path can have: any location, or one of them.
    fn scopes(
        &self,
        choose: impl Fn(Option<LocationId>, &mut Window, &mut App) + Clone + 'static,
        cx: &App,
    ) -> Vec<Entry> {
        let choose_any = choose.clone();
        std::iter::once(Entry::Item(
            Item::new("any", "Any location").on(move |window, cx| choose_any(None, window, cx)),
        ))
        .chain(
            (self.sync.read(cx).locations().unwrap_or_default().iter()).map(|location| {
                let (id, choose) = (location.id.clone(), choose.clone());
                Entry::Item(
                    Item::new(format!("location/{}", location.name), location.name.clone())
                        .on(move |window, cx| choose(Some(id.clone()), window, cx)),
                )
            }),
        )
        .collect()
    }

    /// A path's scope as a tag: its location's, or "any location".
    fn scope_tag(&self, location: Option<&LocationId>, theme: &SbTheme, cx: &App) -> AnyElement {
        match location.and_then(|it| self.sync.read(cx).location(it)) {
            Some(location) => {
                location_tag(location.name.clone(), &location.kind, false, theme).into_any_element()
            }
            None => tag("any location", Tone::Neutral, false, theme).into_any_element(),
        }
    }

    /// The paths a location has seen that hold what is typed, the most sessions first.
    fn completions(&self, cx: &App) -> Vec<ProjectPath> {
        let (Some(read), Some(typed)) = (&self.read, self.typed(cx)) else {
            return Vec::new();
        };
        let typed = typed.to_lowercase();
        (read.seen.iter())
            .filter(|seen| {
                seen.recorded.to_lowercase().contains(&typed)
                    && seen.recorded.to_lowercase() != typed
            })
            .take(COMPLETIONS)
            .cloned()
            .collect()
    }

    /**
     * Up to five paths of Unsorted for this folder (DESIGN §4.7): those whose last component
     * starts with the folder's name, or that are under one of its paths, and that no change
     * has added already.
     */
    fn suggestions(&self, cx: &App) -> Vec<ProjectPath> {
        let Some(read) = &self.read else {
            return Vec::new();
        };
        let library = self.library.read(cx);
        let name = library
            .folder(self.folder)
            .map(|folder| folder.name.to_lowercase())
            .unwrap_or_default();
        (read.seen.iter())
            .filter(|seen| seen.unsorted > 0)
            .filter(|seen| {
                format::last_component(&seen.recorded)
                    .is_some_and(|last| last.to_lowercase().starts_with(&name))
                    || (self.kept.iter())
                        .any(|kept| kept.path.attached.path.contains(&seen.project))
            })
            .filter(|seen| {
                self.refusal(&(seen.recorded.clone(), Some(seen.location.clone())), cx)
                    .is_none()
            })
            .take(SUGGESTIONS)
            .cloned()
            .collect()
    }

    /// The note beside a path: how many of its sessions recorded it another way, and where.
    fn equivalence(&self, kept: &Kept, cx: &App) -> Option<String> {
        let read = self.read.as_ref()?;
        let other = (read.seen.iter())
            .filter(|seen| {
                seen.project == kept.path.attached.path
                    && filing::as_typed(&seen.recorded) != kept.path.typed
            })
            .max_by_key(|seen| seen.sessions)?;
        let location = self.sync.read(cx).location(&other.location)?;
        Some(format!(
            "{} of them from {} as {}",
            format::count(other.sessions as u64),
            location.name,
            other.recorded
        ))
    }
}

impl Render for PathsEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The full editor opens with the keyboard in its add row, drawn in this frame.
        if std::mem::take(&mut self.focus_first) {
            self.focus_input(window, cx);
        }
        let theme = cx.sb_theme().clone();
        let c = &theme.colors;
        let name = self.library.read(cx).folder_path(self.folder);
        let typed = self.typed(cx);
        let refusal = typed
            .clone()
            .and_then(|typed| self.refusal(&(typed, self.scope.clone()), cx));
        // What is still being typed isn't a path yet: Add waits, and nothing is said of it.
        let said = refusal.clone().filter(|it| *it != Refusal::NoPath);
        let suggestions = self.suggestions(cx);
        let input_focused = self.input.focus_handle(cx).is_focused(window);
        let completions = if input_focused {
            self.completions(cx)
        } else {
            Vec::new()
        };

        let scope_button = {
            let editor = cx.entity().downgrade();
            button("paths-scope", ButtonKind::Plain, true, true, &theme)
                .w(SCOPE_WIDTH)
                .justify_between()
                .child(self.scope_tag(self.scope.as_ref(), &theme, cx))
                .child(icon(SbIcon::ChevronDown, theme.sizes.icon_xs, c.fg2))
                .on_click(cx.listener(move |this, _, window, cx| {
                    let editor = editor.clone();
                    let entries = this.scopes(
                        // The keyboard goes on to the path.
                        move |scope, window, cx| {
                            editor
                                .update(cx, |editor, cx| {
                                    editor.scope = scope;
                                    editor.focus_input(window, cx);
                                    cx.notify();
                                })
                                .ok();
                        },
                        cx,
                    );
                    menu::open(entries, window.mouse_position(), px(200.), window, cx);
                }))
                .automation_target("paths/scope")
        };
        let input_bounds = self.input_bounds.clone();
        let field = div()
            .flex_1()
            .min_w_0()
            .relative()
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .h(theme.sizes.input_height)
            .px(theme.sizes.list_row_padding)
            .rounded(theme.radii.input)
            .border_1()
            .map(|this| match (&said, input_focused) {
                (Some(_), _) => this.border_color(c.red).shadow(vec![ring(c.red_soft)]),
                (None, true) => this
                    .border_color(c.accent)
                    .shadow(vec![ring(c.accent_soft)]),
                (None, false) => this.border_color(c.line),
            })
            .bg(c.s1)
            .text_color(c.fg)
            .text_spec(&theme.text.path)
            .child(icon(SbIcon::Plus, theme.sizes.icon_sm, c.fg3))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.input).appearance(false).px_0()),
            )
            .child(
                canvas(
                    move |bounds, _, _| input_bounds.set(bounds),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .automation_target("paths/input");
        let add_row = div()
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .child(scope_button)
            .child(field)
            .when(!self.short, |this| {
                this.child(
                    button(
                        "paths-add",
                        ButtonKind::Plain,
                        true,
                        typed.is_some() && refusal.is_none(),
                        &theme,
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.add_typed(window, cx)))
                    .child("Add")
                    .automation_target("paths/add"),
                )
            });
        let refused = said.map(|refusal| {
            let text = match &refusal {
                Refusal::Here(typed) => format!("Already here as {typed}"),
                Refusal::NoPath | Refusal::Elsewhere(_) => String::new(),
            };
            let row = div()
                .flex()
                .items_center()
                .gap(theme.spacing.sm)
                .pl(SCOPE_WIDTH + theme.spacing.md)
                .text_color(c.red)
                .text_spec(&theme.text.secondary)
                .child(icon(SbIcon::CircleAlert, theme.sizes.icon_sm, c.red));
            match refusal {
                Refusal::Elsewhere(folder) => {
                    let holder = self.library.read(cx).folder_path(folder);
                    row.child("Already attached to")
                        .child(
                            div()
                                .id("paths-holder")
                                .flex()
                                .items_center()
                                .gap(theme.spacing.xs)
                                .text_color(c.fg)
                                .font_weight(theme.text.control.weight)
                                .underline()
                                .cursor_pointer()
                                .child(icon(SbIcon::Folder, theme.sizes.icon_xs, c.fg2))
                                .child(holder)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.library
                                        .update(cx, |library, cx| library.edit_paths(folder, cx));
                                }))
                                .automation_target("paths/holder"),
                        )
                        .child(". A path belongs to one folder; remove it there first.")
                }
                _ => row.child(text),
            }
            .automation_target("paths/refused")
        });
        let completion_list = (!completions.is_empty()).then(|| {
            let at = self.input_bounds.get();
            deferred(
                anchored()
                    .position(point(at.left(), at.bottom() + px(4.)))
                    .child(
                        div()
                            .id("paths-completions")
                            .w(at.size.width.max(px(320.)))
                            .p(px(4.))
                            .flex()
                            .flex_col()
                            .rounded(theme.radii.row)
                            .border_1()
                            .border_color(c.line)
                            .bg(c.s2)
                            .shadow(theme.shadows.pop.clone())
                            .occlude()
                            .children(completions.into_iter().enumerate().map(|(at, seen)| {
                                let path = (seen.recorded.clone(), Some(seen.location.clone()));
                                div()
                                    .id(("paths-completion", at))
                                    .flex()
                                    .items_center()
                                    .gap(theme.spacing.md)
                                    .h(px(28.))
                                    .px(theme.spacing.md)
                                    .rounded(theme.radii.control)
                                    .cursor_pointer()
                                    .hover(|style| style.bg(c.s3))
                                    .child(self.scope_tag(Some(&seen.location), &theme, cx))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .text_spec(&theme.text.path)
                                            .child(seen.recorded.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_color(c.fg3)
                                            .text_spec(&theme.text.number)
                                            .child(format::plural(seen.sessions, "session")),
                                    )
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.scope = path.1.clone();
                                            let typed = path.0.clone();
                                            this.input.update(cx, |input, cx| {
                                                input.set_value(typed, window, cx)
                                            });
                                            cx.notify();
                                        }),
                                    )
                                    .automation_target(format!("paths/completion/{at}"))
                            })),
                    ),
            )
            .with_priority(5)
        });

        let card = div()
            .id("paths-editor")
            .key_context(keymap::PATHS_EDITOR)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &SavePaths, window, cx| this.save(window, cx)))
            .on_action(cx.listener(|this, _: &CancelPaths, _, cx| {
                // While leaving asks about the changes, Esc is the question's.
                if this.library.read(cx).asked().is_some() {
                    cx.propagate();
                } else {
                    this.cancel(cx);
                }
            }))
            .flex_none()
            .flex()
            .flex_col()
            .rounded(theme.radii.card)
            .border_1()
            .border_color(c.line)
            .bg(c.s1)
            .overflow_hidden()
            .children(completion_list);

        if self.short {
            return card
                .gap(theme.spacing.md)
                .py(theme.spacing.lg)
                .px(px(14.))
                .child(title_line(
                    format!(
                        "None yet. Sessions started under a path you attach here file \
                         themselves into {name}."
                    ),
                    &theme,
                ))
                .child(add_row)
                .children(refused)
                .children(
                    suggestions
                        .into_iter()
                        .take(3)
                        .enumerate()
                        .map(|(at, seen)| {
                            let path = (seen.recorded.clone(), Some(seen.location.clone()));
                            div()
                                .flex()
                                .items_center()
                                .gap(theme.spacing.md)
                                .h(px(32.))
                                .px(theme.spacing.md)
                                .rounded(theme.radii.row)
                                .bg(c.accent_soft)
                                .text_spec(&theme.text.secondary)
                                .child(
                                    div()
                                        .text_color(c.accent_text)
                                        .font_weight(theme.text.control.weight)
                                        .child("Suggested from Unsorted"),
                                )
                                .child(self.scope_tag(Some(&seen.location), &theme, cx))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .text_color(c.fg2)
                                        .text_spec(&theme.text.path)
                                        .child(seen.recorded.clone()),
                                )
                                .child(
                                    div()
                                        .text_color(c.fg3)
                                        .text_spec(&theme.text.number)
                                        .child(format::plural(seen.unsorted, "session")),
                                )
                                .child(
                                    button(
                                        ("paths-attach", at),
                                        ButtonKind::Primary,
                                        true,
                                        true,
                                        &theme,
                                    )
                                    .debug_selector(move || format!("paths-attach-{at}"))
                                    .child(icon(SbIcon::Plus, theme.sizes.icon_xs, c.accent_ink))
                                    .child("Attach")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.add(path.clone(), window, cx);
                                    }))
                                    .automation_target(format!("paths/suggested/{at}")),
                                )
                        }),
                )
                .automation_target("paths/editor")
                .into_any_element();
        }

        let rows = self.kept.iter().enumerate().map(|(at, kept)| {
            let freed = kept.removed.then(|| self.freed_by(at, cx));
            let note = self.equivalence(kept, cx);
            table_row(&theme)
                .child(
                    div()
                        .w(SCOPE_WIDTH)
                        .flex()
                        .items_center()
                        .child(self.scope_tag(kept.path.attached.location.as_ref(), &theme, cx)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_spec(&theme.text.path)
                        .when(kept.removed, |this| this.line_through().text_color(c.fg3))
                        .child(kept.path.typed.clone()),
                )
                .children(note.map(|note| {
                    div()
                        .flex_none()
                        .max_w(px(320.))
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_color(c.fg3)
                        .text_spec(&theme.text.secondary)
                        .child(note)
                }))
                .child(count_cell(
                    match freed {
                        Some((freed, unsorted)) if freed > 0 && freed == unsorted => {
                            format!("−{} to Unsorted", format::count(freed as u64))
                        }
                        Some((freed, _)) => format!("−{}", format::count(freed as u64)),
                        None => format::count(kept.sessions as u64),
                    },
                    &theme,
                ))
                .child(if kept.removed {
                    button(
                        ("paths-undo-remove", at),
                        ButtonKind::Ghost,
                        true,
                        true,
                        &theme,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.kept[at].removed = false;
                        this.held(cx);
                    }))
                    .child("Undo")
                    .automation_target(format!("paths/row/{at}/undo"))
                } else {
                    icon_button(("paths-remove", at), REMOVE_WIDTH, &theme)
                        .aria_label(format!("Remove {}", kept.path.typed))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.kept[at].removed = true;
                            this.held(cx);
                        }))
                        .child(icon(SbIcon::X, theme.sizes.icon_sm, c.fg2))
                        .automation_target(format!("paths/row/{at}/remove"))
                })
                .automation_target(format!("paths/row/{at}"))
        });
        let added = self.added.iter().enumerate().map(|(at, added)| {
            let (moved, unsorted) = self.moved_by(added, cx);
            table_row(&theme)
                .bg(c.accent_soft)
                .child(
                    div()
                        .w(SCOPE_WIDTH)
                        .flex()
                        .items_center()
                        .child(self.scope_tag(added.1.as_ref(), &theme, cx)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_color(c.accent_text)
                        .text_spec(&theme.text.path)
                        .child(added.0.clone()),
                )
                .child(
                    div()
                        .text_color(c.fg3)
                        .text_spec(&theme.text.secondary)
                        .child("added"),
                )
                .child(count_cell(
                    if moved > 0 && moved == unsorted {
                        format!("+{} from Unsorted", format::count(moved as u64))
                    } else {
                        format!("+{}", format::count(moved as u64))
                    },
                    &theme,
                ))
                .child(
                    button(
                        ("paths-undo-add", at),
                        ButtonKind::Ghost,
                        true,
                        true,
                        &theme,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.added.remove(at);
                        this.held(cx);
                    }))
                    .child("Undo")
                    .automation_target(format!("paths/added/{at}/undo")),
                )
                .automation_target(format!("paths/added/{at}"))
        });
        let pending = self.is_pending();
        card.child(
            div()
                .flex()
                .items_center()
                .gap(theme.spacing.md)
                .pt(theme.spacing.lg)
                .pb(px(10.))
                .px(px(14.))
                .child(title_line(
                    format!(
                        "Sessions that started under one of these file themselves into {name}. \
                         The longest matching path wins, so subfolders claim first."
                    ),
                    &theme,
                )),
        )
        .child(
            div()
                .mx(px(14.))
                .flex()
                .flex_col()
                .rounded(theme.radii.row)
                .border_1()
                .border_color(c.line)
                .overflow_hidden()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(theme.spacing.lg)
                        .h(HEAD_HEIGHT)
                        .px(px(10.))
                        .bg(c.s1)
                        .text_color(c.fg3)
                        .text_spec(&theme.text.tag)
                        .child(div().w(SCOPE_WIDTH).child("Location"))
                        .child(div().flex_1().child("Path"))
                        .child(div().w(COUNT_WIDTH).flex().justify_end().child("Sessions"))
                        .child(div().w(REMOVE_WIDTH)),
                )
                .children(rows)
                .children(added)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(theme.spacing.sm)
                        .p(px(10.))
                        .border_t_1()
                        .border_color(c.line_subtle)
                        .bg(c.s1)
                        .child(add_row)
                        .children(refused),
                ),
        )
        .when(!suggestions.is_empty(), |this| {
            this.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .pt(px(10.))
                    .pb(theme.spacing.xs)
                    .px(px(14.))
                    .child(div().pt(px(2.)).pb(theme.spacing.sm).child(
                        crate::components::overline("Suggested from Unsorted", &theme),
                    ))
                    .children(suggestions.into_iter().enumerate().map(|(at, seen)| {
                        let path = (seen.recorded.clone(), Some(seen.location.clone()));
                        div()
                            .flex()
                            .items_center()
                            .gap(theme.spacing.md)
                            .h(px(30.))
                            .text_spec(&theme.text.secondary)
                            .child(
                                button(
                                    ("paths-suggest", at),
                                    ButtonKind::Plain,
                                    true,
                                    true,
                                    &theme,
                                )
                                .child(icon(SbIcon::Plus, theme.sizes.icon_xs, c.fg))
                                .child("Add")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.add(path.clone(), window, cx);
                                }))
                                .automation_target(format!("paths/suggested/{at}")),
                            )
                            .child(self.scope_tag(Some(&seen.location), &theme, cx))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_color(c.fg2)
                                    .text_spec(&theme.text.path)
                                    .child(seen.recorded.clone()),
                            )
                            .child(
                                div()
                                    .text_color(c.fg3)
                                    .text_spec(&theme.text.number)
                                    .child(format::plural(seen.unsorted, "session")),
                            )
                    })),
            )
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap(theme.spacing.md)
                .mt(theme.spacing.md)
                .py(px(10.))
                .px(px(14.))
                .border_t_1()
                .border_color(c.line_subtle)
                .bg(c.s1)
                .child(icon(SbIcon::CircleAlert, theme.sizes.icon_sm, c.fg3))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(c.fg2)
                        .text_spec(&theme.text.secondary)
                        .child(if pending {
                            self.summary(cx)
                        } else {
                            "No changes yet.".to_string()
                        })
                        .automation_target("paths/summary"),
                )
                .child(
                    button("paths-cancel", ButtonKind::Plain, true, true, &theme)
                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx)))
                        .child("Cancel")
                        .automation_target("paths/cancel"),
                )
                .child(
                    button("paths-save", ButtonKind::Primary, true, pending, &theme)
                        .on_click(cx.listener(|this, _, window, cx| this.save(window, cx)))
                        .child("Save paths")
                        .automation_target("paths/save"),
                ),
        )
        .automation_target("paths/editor")
        .into_any_element()
    }
}

/// "Project paths" at 600 with the link glyph, then `text` in `fg3`.
fn title_line(text: String, theme: &SbTheme) -> impl IntoElement {
    let c = &theme.colors;
    div()
        .flex()
        .items_center()
        .gap(theme.spacing.md)
        .child(icon(SbIcon::Link, theme.sizes.icon_sm, c.fg3))
        .child(
            div()
                .flex_none()
                .font_weight(theme.text.title.weight)
                .child("Project paths"),
        )
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_color(c.fg3)
                .text_spec(&theme.text.secondary)
                .child(text),
        )
}

/// A 40px row of the table.
fn table_row(theme: &SbTheme) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(theme.spacing.lg)
        .h(ROW_HEIGHT)
        .px(px(10.))
        .border_t_1()
        .border_color(theme.colors.line_subtle)
        .whitespace_nowrap()
}

fn count_cell(text: String, theme: &SbTheme) -> impl IntoElement {
    div()
        .w(COUNT_WIDTH)
        .flex()
        .justify_end()
        .text_color(theme.colors.fg2)
        .text_spec(&theme.text.number)
        .child(text)
}

/// The 3px soft ring around a field (`.inp.focus`, `.inp.bad`).
fn ring(color: gpui::Hsla) -> gpui::BoxShadow {
    gpui::BoxShadow {
        color,
        offset: point(px(0.), px(0.)),
        blur_radius: px(0.),
        spread_radius: px(3.),
        inset: false,
    }
}
