//! The main area (DESIGN §2): one page at a time, chosen from what the services hold
//! ([`Showing`]). The first run while there is no location, then the library's pages (All
//! sessions, Starred and a folder as cards; Unsorted as groups), the page of an opened
//! session, and why the archive didn't open when it didn't.
//!
//! The main area has one focus handle, where the keyboard rests (`crate::shell`), and the key
//! context of the page shown, so a page's keys reach it without the page being clicked first.

pub mod first_run;
mod first_sync;
mod header;
pub mod sessions;
mod states;
#[cfg(test)]
mod tests;
pub mod unsorted;

use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _,
};
use sb_core::catalogue::Shelf;

use crate::actions::first_run::Add;
use crate::actions::sessions::{Down, Left, Open, Right, Star, Up};
use crate::keymap;
use crate::services::{ActiveServices as _, ArchiveState};
use crate::theme::ActiveTheme as _;
use first_run::FirstRun;
use sessions::SessionsView;
use unsorted::UnsortedView;

/// What the main area shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Showing {
    /// Nothing yet: the archive is opening, or its locations are being read.
    Starting,
    ArchiveFailed,
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
        match services.sync.read(cx).locations() {
            None => Self::Starting,
            Some([]) => Self::FirstRun,
            Some(_) if library.open_session().is_some() => Self::Session,
            Some(_) if library.shelf() == Shelf::Unsorted => Self::Unsorted,
            Some(_) => Self::Sessions,
        }
    }

    /// `"starting"`, `"archive-failed"`, `"first-run"`, `"session"`, `"unsorted"` or
    /// `"sessions"`, as automation's `state` spells it.
    pub fn key(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::ArchiveFailed => "archive-failed",
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
            Self::Starting | Self::ArchiveFailed | Self::Session => "Main",
        }
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
        Self {
            focus_handle: cx.focus_handle(),
            sessions: cx.new(SessionsView::new),
            unsorted: cx.new(UnsortedView::new),
            first_run: cx.new(FirstRun::new),
            _observers: [
                cx.observe(&archive, |_, _, cx| cx.notify()),
                cx.observe(&sync, |_, _, cx| cx.notify()),
                cx.observe(&library, |_, _, cx| cx.notify()),
            ],
        }
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

    fn open(&mut self, _: &Open, _: &mut Window, cx: &mut Context<Self>) {
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
}

impl Render for MainArea {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.sb_theme().clone();
        let showing = Showing::now(cx);
        let services = cx.services();
        div()
            .id("main-area")
            .key_context(showing.key_context())
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &Left, _, cx| this.step(-1, 0, cx)))
            .on_action(cx.listener(|this, _: &Right, _, cx| this.step(1, 0, cx)))
            .on_action(cx.listener(|this, _: &Up, _, cx| this.step(0, -1, cx)))
            .on_action(cx.listener(|this, _: &Down, _, cx| this.step(0, 1, cx)))
            .on_action(cx.listener(Self::open))
            .on_action(cx.listener(|this, _: &Star, _, cx| {
                this.sessions
                    .update(cx, |sessions, cx| sessions.star_selected(cx));
            }))
            .on_action(cx.listener(|this, _: &Add, _, cx| {
                this.first_run.update(cx, |first_run, cx| first_run.add(cx));
            }))
            .size_full()
            .map(|this| match showing {
                Showing::Starting => this,
                Showing::ArchiveFailed => {
                    let archive = services.archive.read(cx);
                    let why = match archive.state() {
                        ArchiveState::Failed(why) => why.clone(),
                        _ => String::new(),
                    };
                    this.child(states::archive_failed(
                        archive.dir().display().to_string(),
                        why,
                        &theme,
                        cx,
                    ))
                }
                Showing::FirstRun => this.child(self.first_run.clone()),
                Showing::Session => {
                    let library = services.library.read(cx);
                    let from: SharedString = match library.shelf() {
                        Shelf::All => "All sessions".into(),
                        Shelf::Starred => "Starred".into(),
                        Shelf::Unsorted => "Unsorted".into(),
                        Shelf::Folder(folder) => library
                            .folder(folder)
                            .map_or_else(SharedString::default, |it| it.name.clone().into()),
                    };
                    this.children(
                        library
                            .open_session()
                            .map(|session| states::session(session, from, &theme, cx)),
                    )
                }
                Showing::Unsorted => this.child(self.unsorted.clone()),
                Showing::Sessions => this.child(self.sessions.clone()),
            })
    }
}
