//! The main area (DESIGN §2): one page at a time, chosen from what the services hold
//! ([`Showing`]). The first run while there is no location, then the library's pages (All
//! sessions, Starred and a folder as cards; Unsorted as groups), the page of an opened
//! session, and why the archive didn't open when it didn't. Over a library page, a banner for
//! each thing that went wrong: a location whose last sync failed and the library when it
//! couldn't be read, each with a way to try again, and a change of the owner's the catalogue
//! refused. An unreachable location is not among them (PLAN §5): its row says so.
//!
//! The main area has one focus handle, where the keyboard rests (`crate::shell`), and the key
//! context of the page shown, so a page's keys reach it without the page being clicked first.
//! `Tab` goes on from there to the page's buttons, and `Enter` presses the one that has the
//! keyboard instead of opening the selected session.

pub mod first_run;
mod first_sync;
mod header;
#[cfg(test)]
mod interaction_tests;
pub(crate) mod menus;
pub mod paths_editor;
pub mod sessions;
mod states;
#[cfg(test)]
mod tests;
pub mod unsorted;

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, Role, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _,
};
use sb_core::catalogue::Shelf;

use crate::actions::first_run::Add;
use crate::actions::sessions::{Down, Left, Menu, Move, Open, Right, SelectAll, Star, Trash, Up};
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{ButtonKind, bad_banner, button};
use crate::keymap;
use crate::services::{ActiveServices as _, ArchiveState, LibraryService, LocationStatus};
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _};
use first_run::FirstRun;
use sessions::SessionsView;
use unsorted::UnsortedView;

/// What the main area shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Showing {
    /// Nothing yet: the archive is opening, or its locations are being read.
    Starting,
    ArchiveFailed,
    /// The archive is open and its locations couldn't be read.
    LocationsFailed,
    FirstRun,
    /// An opened session.
    Session,
    Unsorted,
    /// All sessions, Starred or a folder.
    Sessions,
}

impl Showing {
    /// What is shown, from the services: the archive's state, then whether there is a
    /// location, then the library's page.
    pub fn now(cx: &App) -> Self {
        let services = cx.services();
        match services.archive.read(cx).state() {
            ArchiveState::Opening => return Self::Starting,
            ArchiveState::Failed(_) => return Self::ArchiveFailed,
            ArchiveState::Open(_) => {}
        }
        let library = services.library.read(cx);
        let sync = services.sync.read(cx);
        match sync.locations() {
            None if sync.failure().is_some() => Self::LocationsFailed,
            None => Self::Starting,
            Some(_) if sync.is_first_run() => Self::FirstRun,
            Some(_) if library.open_session().is_some() => Self::Session,
            Some(_) if library.shelf() == Shelf::Unsorted => Self::Unsorted,
            Some(_) => Self::Sessions,
        }
    }

    /// `"starting"`, `"archive-failed"`, `"locations-failed"`, `"first-run"`, `"session"`,
    /// `"unsorted"` or `"sessions"`, as automation's `state` spells it.
    pub fn key(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::ArchiveFailed => "archive-failed",
            Self::LocationsFailed => "locations-failed",
            Self::FirstRun => "first-run",
            Self::Session => "session",
            Self::Unsorted => "unsorted",
            Self::Sessions => "sessions",
        }
    }

    /// The key context of the main area while this shows.
    fn key_context(self) -> &'static str {
        match self {
            Self::Sessions => keymap::SESSIONS,
            Self::Unsorted => keymap::UNSORTED,
            Self::FirstRun => keymap::FIRST_RUN,
            Self::Starting | Self::ArchiveFailed | Self::LocationsFailed | Self::Session => "Main",
        }
    }

    /// Whether a page of the library shows.
    pub fn is_library(self) -> bool {
        matches!(self, Self::Sessions | Self::Unsorted | Self::Session)
    }
}

/// Why a sync failed, in the failing tool's words, as a sentence of its own: its first letter
/// a capital and one full stop at its end.
fn sentence(why: &str) -> String {
    let why = why.trim().trim_end_matches('.');
    let mut chars = why.chars();
    match chars.next() {
        Some(first) => format!("{}{}.", first.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

pub struct MainArea {
    focus_handle: FocusHandle,
    sessions: Entity<SessionsView>,
    unsorted: Entity<UnsortedView>,
    first_run: Entity<FirstRun>,
    _observers: [Subscription; 3],
}

impl Focusable for MainArea {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl MainArea {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let services = cx.services();
        let (archive, sync, library) = (
            services.archive.clone(),
            services.sync.clone(),
            services.library.clone(),
        );
        let focus_handle = cx.focus_handle().tab_stop(true);
        Self {
            sessions: cx.new(|cx| SessionsView::new(focus_handle.clone(), cx)),
            unsorted: cx.new(|cx| UnsortedView::new(focus_handle.clone(), cx)),
            focus_handle,
            first_run: cx.new(FirstRun::new),
            _observers: [
                cx.observe(&archive, |_, _, cx| cx.notify()),
                cx.observe(&sync, |_, _, cx| cx.notify()),
                cx.observe(&library, |_, _, cx| cx.notify()),
            ],
        }
    }

    /// The page of All sessions, Starred or a folder.
    pub fn sessions(&self) -> &Entity<SessionsView> {
        &self.sessions
    }

    /// Moves a page's selection: `by` along the reading order, or `lines` up or down.
    fn step(&mut self, by: isize, lines: isize, cx: &mut Context<Self>) {
        match Showing::now(cx) {
            Showing::Sessions => self
                .sessions
                .update(cx, |sessions, cx| sessions.step(by, lines, cx)),
            Showing::Unsorted => self
                .unsorted
                .update(cx, |unsorted, cx| unsorted.step(lines, cx)),
            _ => {}
        }
    }

    /**
     * Whether a text field of the page has the keyboard: a single-letter key or `Delete` is
     * then the field's (DESIGN §5), and goes on to it.
     */
    fn typing(&self, window: &Window, cx: &mut Context<Self>) -> bool {
        let typing = match Showing::now(cx) {
            Showing::Sessions => self.sessions.read(cx).is_typing(window, cx),
            Showing::Unsorted => self.unsorted.read(cx).is_typing(window, cx),
            _ => false,
        };
        if typing {
            cx.propagate();
        }
        typing
    }

    /// Does `sessions` on a page of sessions, or `unsorted` on Unsorted.
    fn each_page(
        &mut self,
        cx: &mut Context<Self>,
        sessions: impl FnOnce(&mut SessionsView, &mut Context<SessionsView>),
        unsorted: impl FnOnce(&mut UnsortedView, &mut Context<UnsortedView>),
    ) {
        match Showing::now(cx) {
            Showing::Sessions => self.sessions.update(cx, sessions),
            Showing::Unsorted => self.unsorted.update(cx, unsorted),
            _ => {}
        }
    }

    /// Opens the selected session's menu, or with `moving` its Move to folder.
    fn menu(&mut self, moving: bool, window: &mut Window, cx: &mut Context<Self>) {
        match Showing::now(cx) {
            Showing::Sessions => self.sessions.update(cx, |sessions, cx| {
                sessions.menu_selected(moving, window, cx)
            }),
            Showing::Unsorted => self.unsorted.update(cx, |unsorted, cx| {
                unsorted.menu_selected(moving, window, cx)
            }),
            _ => {}
        }
    }

    /// Whether `Enter` is for one of the page's buttons: `Tab` has put the keyboard on it,
    /// and the key is left to press it.
    fn enter_presses_a_button(&self, window: &Window, cx: &mut Context<Self>) -> bool {
        let on_a_button = !self.focus_handle.is_focused(window);
        if on_a_button {
            cx.propagate();
        }
        on_a_button
    }

    fn open(&mut self, _: &Open, window: &mut Window, cx: &mut Context<Self>) {
        if self.enter_presses_a_button(window, cx) {
            return;
        }
        match Showing::now(cx) {
            Showing::Sessions => self
                .sessions
                .update(cx, |sessions, cx| sessions.open_selected(cx)),
            Showing::Unsorted => self
                .unsorted
                .update(cx, |unsorted, cx| unsorted.open_selected(cx)),
            _ => {}
        }
    }

    fn add(&mut self, _: &Add, window: &mut Window, cx: &mut Context<Self>) {
        if !self.enter_presses_a_button(window, cx) {
            self.first_run.update(cx, |first_run, cx| first_run.add(cx));
        }
    }

    /// What went wrong and can be tried again, over a library page (DESIGN §4.14's `bad`
    /// banner): the library when it couldn't be read, and each location whose last sync
    /// failed, with why in full.
    fn banners(&self, theme: &SbTheme, cx: &Context<Self>) -> Vec<AnyElement> {
        let services = cx.services();
        let (sync, library) = (services.sync.read(cx), services.library.read(cx));
        let text = |text: String| div().flex_1().min_w_0().child(text);
        let mut banners = Vec::new();
        if let Some(why) = library.failure() {
            banners.push(
                bad_banner(theme)
                    .child(text(format!("The library couldn't be read: {why}.")))
                    .child(
                        button("library-retry", ButtonKind::Plain, true, true, theme)
                            .on_click(cx.listener(|_, _, _, cx| {
                                let library = cx.services().library.clone();
                                library.update(cx, |library, cx| library.reload(cx));
                            }))
                            .child("Try again")
                            .automation_target("main/library-failed/retry"),
                    )
                    .automation_target("main/library-failed"),
            );
        }
        if let Some(why) = library.write_failure() {
            banners.push(
                bad_banner(theme)
                    .child(text(why.to_string()))
                    .child(
                        button("write-failed-dismiss", ButtonKind::Plain, true, true, theme)
                            .on_click(cx.listener(|_, _, _, cx| {
                                let library = cx.services().library.clone();
                                library.update(cx, LibraryService::dismiss_write_failure);
                            }))
                            .child("Dismiss")
                            .automation_target("main/write-failed/dismiss"),
                    )
                    .automation_target("main/write-failed"),
            );
        }
        for location in sync.locations().unwrap_or_default() {
            let LocationStatus::Failed(why) = sync.status(location) else {
                continue;
            };
            let why = sentence(&agents::named(&why, cx));
            let (id, name) = (location.id.clone(), location.name.clone());
            banners.push(
                bad_banner(theme)
                    .debug_selector(|| "sync-failed".into())
                    .child(text(if location.last_success.is_none() {
                        format!(
                            "{name}'s first sync isn't complete. {why} What it copied is kept, \
                             and the next sync carries on from there."
                        )
                    } else {
                        format!("{name} couldn't be synced in full. {why}")
                    }))
                    .child(
                        button(
                            SharedString::from(format!("sync-retry-{id}")),
                            ButtonKind::Plain,
                            true,
                            true,
                            theme,
                        )
                        .on_click(cx.listener(move |_, _, _, cx| {
                            let sync = cx.services().sync.clone();
                            sync.update(cx, |sync, cx| sync.sync(&id, cx));
                        }))
                        .child("Try again")
                        .automation_target_with({
                            let name = name.clone();
                            move || format!("main/sync-failed/{name}/retry").into()
                        }),
                    )
                    .automation_target_with(move || format!("main/sync-failed/{name}").into()),
            );
        }
        banners
    }
}

impl Render for MainArea {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.sb_theme().clone();
        let showing = Showing::now(cx);
        let services = cx.services();
        let banners = if showing.is_library() {
            self.banners(&theme, cx)
        } else {
            Vec::new()
        };
        let page = match showing {
            Showing::Starting => None,
            Showing::ArchiveFailed => {
                let archive = services.archive.read(cx);
                match archive.state() {
                    ArchiveState::Failed(failure) => Some(states::archive_failed(
                        archive.dir().display().to_string(),
                        failure,
                        &theme,
                        cx,
                    )),
                    _ => None,
                }
            }
            Showing::LocationsFailed => services
                .sync
                .read(cx)
                .failure()
                .map(|why| states::locations_failed(why, &theme, cx)),
            Showing::FirstRun => Some(self.first_run.clone().into_any_element()),
            Showing::Session => {
                let library = services.library.read(cx);
                let from: SharedString = match library.shelf() {
                    Shelf::All => "All sessions".into(),
                    Shelf::Starred => "Starred".into(),
                    Shelf::Unsorted => "Unsorted".into(),
                    Shelf::Trash => "Trash".into(),
                    Shelf::Folder(folder) => library
                        .folder(folder)
                        .map_or_else(SharedString::default, |it| it.name.clone().into()),
                };
                library
                    .open_session()
                    .map(|session| states::session(session, from, &theme, cx))
            }
            Showing::Unsorted => Some(self.unsorted.clone().into_any_element()),
            Showing::Sessions => Some(self.sessions.clone().into_any_element()),
        };
        div()
            .id("main-area")
            // The window's `Main` landmark is the element the keyboard rests on.
            .role(Role::Main)
            .key_context(showing.key_context())
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &Left, _, cx| this.step(-1, 0, cx)))
            .on_action(cx.listener(|this, _: &Right, _, cx| this.step(1, 0, cx)))
            .on_action(cx.listener(|this, _: &Up, _, cx| this.step(0, -1, cx)))
            .on_action(cx.listener(|this, _: &Down, _, cx| this.step(0, 1, cx)))
            .on_action(cx.listener(Self::open))
            .on_action(cx.listener(|this, _: &Star, window, cx| {
                if !this.typing(window, cx) {
                    this.each_page(cx, SessionsView::star_selected, UnsortedView::star_selected);
                }
            }))
            .on_action(cx.listener(|this, _: &Trash, window, cx| {
                if !this.typing(window, cx) {
                    this.each_page(
                        cx,
                        SessionsView::trash_selected,
                        UnsortedView::trash_selected,
                    );
                }
            }))
            .on_action(cx.listener(|this, _: &Move, window, cx| {
                if !this.typing(window, cx) {
                    this.menu(true, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Menu, window, cx| this.menu(false, window, cx)))
            .on_action(cx.listener(|this, _: &SelectAll, window, cx| {
                if !this.typing(window, cx) {
                    this.each_page(cx, SessionsView::select_all, |_, _| {});
                }
            }))
            .on_action(cx.listener(Self::add))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .when(!banners.is_empty(), |this| {
                this.child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_col()
                        .gap(theme.spacing.md)
                        .pt(theme.spacing.lg)
                        .px(theme.sizes.page_side)
                        .children(banners),
                )
            })
            .child(div().flex_1().min_h_0().children(page))
            .children(
                showing
                    .is_library()
                    .then(|| self.toast(&theme, cx))
                    .flatten(),
            )
            .children(self.deleting(window, &theme, cx))
    }
}

/// How far above the main area's foot a toast sits (DESIGN §4.14).
const TOAST_BOTTOM: gpui::Pixels = gpui::px(24.);

impl MainArea {
    /**
     * The toast (DESIGN §4.14): at the bottom centre of the main area, `s2`, radius 8,
     * `shadow-pop`, what was done and Undo where it can be undone.
     */
    fn toast(&self, theme: &SbTheme, cx: &Context<Self>) -> Option<AnyElement> {
        let c = &theme.colors;
        let toast = cx.services().library.read(cx).toast()?;
        Some(
            div()
                .absolute()
                .bottom(TOAST_BOTTOM)
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(theme.spacing.lg)
                        .py(theme.spacing.md)
                        .pl(theme.spacing.lg)
                        .pr(theme.spacing.md)
                        .rounded(theme.radii.row)
                        .border_1()
                        .border_color(c.line)
                        .bg(c.s2)
                        .shadow(theme.shadows.pop.clone())
                        .child(
                            div()
                                .child(toast.text.clone())
                                .automation_target("toast/text"),
                        )
                        .when(toast.can_undo(), |this| {
                            this.child(
                                div()
                                    .id("toast-undo")
                                    .role(Role::Button)
                                    .text_color(c.accent_text)
                                    .font_weight(theme.text.control.weight)
                                    .cursor_pointer()
                                    .on_click(|_, _, cx| {
                                        let library = cx.services().library.clone();
                                        library.update(cx, LibraryService::undo);
                                    })
                                    .child("Undo")
                                    .automation_target("toast/undo"),
                            )
                        })
                        .child(
                            crate::components::icon_button(
                                "toast-dismiss",
                                theme.sizes.control_small_height,
                                theme,
                            )
                            .aria_label("Dismiss")
                            .on_click(|_, _, cx| {
                                let library = cx.services().library.clone();
                                library.update(cx, LibraryService::dismiss_toast);
                            })
                            .child(crate::components::icon(
                                crate::icons::SbIcon::X,
                                theme.sizes.icon_sm,
                                c.fg2,
                            )),
                        )
                        .automation_target("toast"),
                )
                .into_any_element(),
        )
    }

    /**
     * Delete folder… (DESIGN §4.1): "Delete fc? Its 447 sessions go to Unsorted and are filed
     * again by the rules; its subfolders move up to its parent. Nothing is deleted from the
     * archive." with Cancel and Delete folder. `Enter` doesn't confirm.
     */
    fn deleting(&self, window: &Window, theme: &SbTheme, cx: &Context<Self>) -> Option<AnyElement> {
        let c = &theme.colors;
        let library = cx.services().library.read(cx);
        let folder = library.folder(library.deleting()?)?;
        let counts = &library.counts().folders;
        let inside: usize = (library.folders().iter())
            .filter(|it| it.parent == Some(folder.id))
            .filter_map(|it| counts.get(&it.id))
            .sum();
        let own = counts
            .get(&folder.id)
            .copied()
            .unwrap_or(0)
            .saturating_sub(inside);
        let answer = |delete: bool| {
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                let library = cx.services().library.clone();
                library.update(cx, |library, cx| library.answer_delete(delete, cx));
            }
        };
        let dialog = crate::overlay::surface(theme)
            .w(theme.sizes.dialog_width)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(theme.spacing.md)
                    .p(theme.sizes.dialog_padding)
                    .child(
                        div()
                            .flex()
                            .gap(theme.spacing.xs)
                            .text_spec(&theme.text.sheet_title)
                            .child("Delete")
                            .child(
                                div()
                                    .flex()
                                    .child(div().italic().child(folder.name.clone()))
                                    .child("?"),
                            ),
                    )
                    .child(div().text_color(c.fg2).child(format!(
                        "Its {} go to Unsorted and are filed again by the rules; its \
                         subfolders move up to its parent. Nothing is deleted from the \
                         archive.",
                        crate::format::plural(own, "session")
                    ))),
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
                        button("delete-cancel", ButtonKind::Plain, false, true, theme)
                            .on_click(answer(false))
                            .child("Cancel")
                            .automation_target("delete-folder/cancel"),
                    )
                    .child(
                        button("delete-confirm", ButtonKind::Danger, false, true, theme)
                            .on_click(answer(true))
                            .child("Delete folder")
                            .automation_target("delete-folder/confirm"),
                    ),
            )
            .debug_selector(|| "delete-folder".into())
            .automation_target("delete-folder");
        Some(crate::overlay::modal(
            dialog,
            crate::overlay::Layer::Dialog,
            window,
            theme,
            |_, cx| {
                let library = cx.services().library.clone();
                library.update(cx, |library, cx| library.answer_delete(false, cx));
            },
        ))
    }
}
