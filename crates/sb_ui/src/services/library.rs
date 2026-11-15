//! The library (PLAN §8, §11 `Library`): the page shown, what is selected on it, and what the
//! sidebar and the pages read from the catalogue.
//!
//! Everything is read in one background job per load ([`load`]): the counts beside the
//! sidebar's rows, the folders, and the shown page's sessions with what its header says of
//! them. Views draw from what the last load left here and never query. A load runs when the
//! archive opens, when another page is shown, after the owner changes something, and when
//! sync says the archive changed; a load asked for while one runs follows it, so loads never
//! pile up.
//!
//! The owner's changes (a star, a new folder, a path attached) are written in background jobs
//! too, and the page is loaded again after each. A star is also shown at once, in place.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::ops::Range;

use gpui::{AppContext as _, Context, Entity, EventEmitter, Subscription};
use sb_core::archive::Archive;
use sb_core::catalogue::{
    CatalogueError, Filed, Folder, LibraryCounts, PathGroup, Session, SessionId, Shelf,
};
use sb_core::filing::{self, AttachedPath, FolderId};
use sb_core::location::LocationId;

use super::{ArchiveService, SyncEvent, SyncService};
use crate::format;

/// The sessions of a page that ended in one calendar month, as a range of the page's sessions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Month {
    /// `(year, month)`; `None` for the sessions that recorded no time, which come last.
    pub month: Option<(i32, u32)>,
    pub sessions: Range<usize>,
}

/// What a page's header says of its sessions (DESIGN §4.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub sessions: usize,
    /// Tool calls, the subagents' included.
    pub calls: u64,
    /// When the first session started and the last ended, in Unix milliseconds.
    pub span: Option<(i64, i64)>,
    /// How many locations the sessions are from.
    pub locations: usize,
}

/// A project path attached to the folder shown (DESIGN §4.4), as its chip reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathChip {
    /// The path as the owner typed it.
    pub typed: String,
    /// The one location it is attached for; `None` for any.
    pub location: Option<LocationId>,
    /// How many of the page's sessions it filed here.
    pub sessions: usize,
}

/// The sessions of All sessions, Starred or a folder, the one that did something last first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionsPage {
    pub sessions: Vec<Session>,
    pub months: Vec<Month>,
    pub stats: Stats,
    /// The shown folder's attached paths; none on the other pages.
    pub paths: Vec<PathChip>,
}

impl SessionsPage {
    fn new(sessions: Vec<Session>, paths: Vec<PathChip>) -> Self {
        let mut months: Vec<Month> = Vec::new();
        let mut locations = HashSet::new();
        let mut stats = Stats {
            sessions: sessions.len(),
            ..Stats::default()
        };
        for (at, session) in sessions.iter().enumerate() {
            let month = session.ended.and_then(format::month_of);
            match months.last_mut() {
                Some(last) if last.month == month => last.sessions.end = at + 1,
                _ => months.push(Month {
                    month,
                    sessions: at..at + 1,
                }),
            }
            stats.calls += u64::from(session.all.calls);
            locations.insert(&session.id.location);
            for time in session.started.into_iter().chain(session.ended) {
                stats.span = Some(match stats.span {
                    Some((from, to)) => (from.min(time), to.max(time)),
                    None => (time, time),
                });
            }
        }
        stats.locations = locations.len();
        Self {
            sessions,
            months,
            stats,
            paths,
        }
    }
}

/// What a page shows under its header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Page {
    Sessions(SessionsPage),
    /// Unsorted, grouped by where the sessions started (DESIGN §4.6), the group with the most
    /// sessions first.
    Unsorted(Vec<PathGroup>),
}

/// What one load reads.
struct Loaded {
    shelf: Shelf,
    counts: LibraryCounts,
    folders: Vec<Folder>,
    agents: usize,
    page: Page,
}

/// Reads what the sidebar and the page of `shelf` show.
fn load(archive: &Archive, shelf: Shelf) -> Result<Loaded, CatalogueError> {
    let catalogue = archive.catalogue();
    let mut agents = HashSet::new();
    for location in catalogue.locations()? {
        agents.extend(
            catalogue
                .providers_found(&location.id)?
                .into_iter()
                .map(|(provider, _)| provider),
        );
    }
    let page = match shelf {
        Shelf::Unsorted => {
            let mut groups = catalogue.unsorted()?;
            groups.sort_by_key(|group| Reverse(group.sessions.len()));
            Page::Unsorted(groups)
        }
        Shelf::All | Shelf::Starred | Shelf::Folder(_) => {
            let sessions = catalogue.sessions(shelf)?;
            let paths = match shelf {
                Shelf::Folder(folder) => {
                    let paths = catalogue.folder_paths(folder)?;
                    let attached: Vec<AttachedPath> =
                        paths.iter().map(|path| path.attached.clone()).collect();
                    // A session a rule filed here was claimed by the longest of these.
                    let mut filed = vec![0; paths.len()];
                    for session in &sessions {
                        if session.filed == Filed::ByRule(folder)
                            && let Some(project) = &session.project
                            && let Some(by) = filing::file(project, &session.id.location, &attached)
                            && let Some(at) = attached.iter().position(|path| path == by)
                        {
                            filed[at] += 1;
                        }
                    }
                    paths
                        .into_iter()
                        .zip(filed)
                        .map(|(path, sessions)| PathChip {
                            typed: path.typed,
                            location: path.attached.location,
                            sessions,
                        })
                        .collect()
                }
                _ => Vec::new(),
            };
            Page::Sessions(SessionsPage::new(sessions, paths))
        }
    };
    Ok(Loaded {
        shelf,
        counts: catalogue.library_counts()?,
        folders: catalogue.folders()?,
        agents: agents.len(),
        page,
    })
}

pub enum LibraryEvent {
    /// A folder the owner just made is among the folders now: the sidebar renames it in
    /// place (DESIGN §4.6).
    FolderCreated(FolderId),
}

pub struct LibraryService {
    archive: Entity<ArchiveService>,
    shelf: Shelf,
    counts: LibraryCounts,
    folders: Vec<Folder>,
    /// How many agents the archive holds sessions of (DESIGN §4.2.10).
    agents: usize,
    /// The page last loaded, and the shelf it is of.
    page: Option<(Shelf, Page)>,
    /// The selected session, and where it is among a sessions page's.
    selected: Option<(SessionId, Option<usize>)>,
    /// The session whose reader is open.
    open: Option<Session>,
    loading: bool,
    /// A load was asked for while one ran.
    stale: bool,
    /// The owner's changes being written.
    writes: usize,
    /// A folder to announce once a load has it.
    created: Option<FolderId>,
    _observers: [Subscription; 2],
}

impl EventEmitter<LibraryEvent> for LibraryService {}

impl LibraryService {
    pub fn new(
        archive: Entity<ArchiveService>,
        sync: Entity<SyncService>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            _observers: [
                cx.observe(&archive, |this, _, cx| {
                    if this.page.is_none() {
                        this.reload(cx);
                    }
                }),
                cx.subscribe(&sync, |this, _, _: &SyncEvent, cx| this.reload(cx)),
            ],
            archive,
            shelf: Shelf::All,
            counts: LibraryCounts::default(),
            folders: Vec::new(),
            agents: 0,
            page: None,
            selected: None,
            open: None,
            loading: false,
            stale: false,
            writes: 0,
            created: None,
        }
    }

    /// The page shown.
    pub fn shelf(&self) -> Shelf {
        self.shelf
    }

    pub fn counts(&self) -> &LibraryCounts {
        &self.counts
    }

    /// Every folder, those with the same parent together and in their order.
    pub fn folders(&self) -> &[Folder] {
        &self.folders
    }

    pub fn folder(&self, id: FolderId) -> Option<&Folder> {
        self.folders.iter().find(|folder| folder.id == id)
    }

    pub fn agents(&self) -> usize {
        self.agents
    }

    /// What the shown page holds, once it is loaded.
    pub fn page(&self) -> Option<&Page> {
        self.page
            .as_ref()
            .filter(|(shelf, _)| *shelf == self.shelf)
            .map(|(_, page)| page)
    }

    /// The shown page's sessions, when it is one of sessions and loaded.
    pub fn sessions(&self) -> Option<&SessionsPage> {
        match self.page()? {
            Page::Sessions(page) => Some(page),
            Page::Unsorted(_) => None,
        }
    }

    pub fn selected(&self) -> Option<&SessionId> {
        self.selected.as_ref().map(|(id, _)| id)
    }

    /// Where the selected session is among the shown page's sessions.
    pub fn selected_index(&self) -> Option<usize> {
        self.selected.as_ref()?.1
    }

    /// The session whose reader is open.
    pub fn open_session(&self) -> Option<&Session> {
        self.open.as_ref()
    }

    /// The sidebar's selected row (DESIGN §4.1): the page shown, or the folder of the session
    /// being read.
    pub fn highlighted(&self) -> Shelf {
        match self.open.as_ref().map(|session| session.filed) {
            Some(Filed::ByRule(folder) | Filed::ByHand(folder)) => Shelf::Folder(folder),
            Some(Filed::Unsorted) | None => self.shelf,
        }
    }

    /// Nothing is being read or written.
    pub fn is_settled(&self) -> bool {
        !self.loading && self.writes == 0
    }

    /// Shows the page of `shelf`.
    pub fn show(&mut self, shelf: Shelf, cx: &mut Context<Self>) {
        if self.shelf == shelf && self.open.is_none() {
            return;
        }
        self.open = None;
        if self.shelf != shelf {
            self.shelf = shelf;
            self.selected = None;
            self.reload(cx);
        }
        cx.notify();
    }

    /// Selects the session at `index` of the shown page's sessions.
    pub fn select_index(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(session) = self.sessions().and_then(|page| page.sessions.get(index)) {
            self.selected = Some((session.id.clone(), Some(index)));
            cx.notify();
        }
    }

    /// Selects a session of Unsorted.
    pub fn select(&mut self, id: SessionId, cx: &mut Context<Self>) {
        self.selected = Some((id, None));
        cx.notify();
    }

    /// Opens a session's reader.
    pub fn open(&mut self, session: Session, cx: &mut Context<Self>) {
        self.open = Some(session);
        cx.notify();
    }

    /// Goes back from the reader to the page it was opened from.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if self.open.take().is_some() {
            cx.notify();
        }
    }

    /// Stars or unstars a session of the shown page.
    pub fn set_starred(&mut self, id: &SessionId, starred: bool, cx: &mut Context<Self>) {
        let shelf = self.shelf;
        let Some((_, Page::Sessions(page))) = &mut self.page else {
            return;
        };
        let at = match &self.selected {
            Some((selected, Some(at))) if selected == id => Some(*at),
            _ => page.sessions.iter().position(|session| session.id == *id),
        };
        let Some(session) = at.and_then(|at| page.sessions.get_mut(at)) else {
            return;
        };
        if session.starred == starred {
            return;
        }
        session.starred = starred;
        if starred {
            self.counts.starred += 1;
        } else {
            self.counts.starred = self.counts.starred.saturating_sub(1);
        }
        let id = id.clone();
        self.write(
            cx,
            move |archive| archive.catalogue().set_starred(&id, starred),
            // Starred lists only the starred; elsewhere the star is already shown.
            move |this, (), cx| {
                if shelf == Shelf::Starred {
                    this.reload(cx);
                }
            },
        );
        cx.notify();
    }

    /// Makes a folder in `parent`, or at the top. The sidebar hears when it is there.
    pub fn create_folder(
        &mut self,
        name: String,
        parent: Option<FolderId>,
        cx: &mut Context<Self>,
    ) {
        self.write(
            cx,
            move |archive| archive.catalogue().create_folder(&name, parent),
            |this, folder, cx| {
                this.created = Some(folder);
                this.reload(cx);
            },
        );
    }

    pub fn rename_folder(&mut self, folder: FolderId, name: String, cx: &mut Context<Self>) {
        self.write(
            cx,
            move |archive| archive.catalogue().rename_folder(folder, &name),
            |this, (), cx| this.reload(cx),
        );
    }

    /**
     * Makes a top-level folder named after the last component of a group of Unsorted's path
     * and attaches the path to it, which files the group there (DESIGN §4.6). The path is
     * attached for the group's location; a group that several locations share, which only a
     * drive's path is, has it attached for any.
     */
    pub fn create_folder_from(&mut self, group: &PathGroup, cx: &mut Context<Self>) {
        let Some(typed) = group.spellings.first().cloned() else {
            return;
        };
        let Some(name) = format::last_component(&typed).map(str::to_string) else {
            return;
        };
        let location = match &group.locations[..] {
            [only] => Some(only.clone()),
            _ => None,
        };
        self.write(
            cx,
            move |archive| {
                let catalogue = archive.catalogue();
                let folder = catalogue.create_folder(&name, None)?;
                catalogue.attach_path(folder, &typed, location.as_ref())?;
                Ok(folder)
            },
            |this, folder, cx| {
                this.created = Some(folder);
                this.reload(cx);
            },
        );
    }

    /// Reads the shown page and the sidebar's counts again.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        if self.loading {
            self.stale = true;
            return;
        }
        self.loading = true;
        let shelf = self.shelf;
        let job = cx.background_spawn(async move { load(&archive, shelf) });
        cx.spawn(async move |this, cx| {
            let loaded = job.await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match loaded {
                    Ok(loaded) => this.loaded(loaded, cx),
                    Err(error) => log::error!("the library could not be read: {error}"),
                }
                if std::mem::take(&mut this.stale) || this.shelf != shelf {
                    this.reload(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn loaded(&mut self, loaded: Loaded, cx: &mut Context<Self>) {
        self.counts = loaded.counts;
        self.folders = loaded.folders;
        self.agents = loaded.agents;
        if loaded.shelf == self.shelf
            && let Some((id, at)) = &mut self.selected
            && let Page::Sessions(page) = &loaded.page
        {
            // The selected session may have moved: sessions arrive while a sync runs.
            let still = at.is_some_and(|at| page.sessions.get(at).is_some_and(|it| it.id == *id));
            if !still {
                *at = page.sessions.iter().position(|session| session.id == *id);
            }
        }
        // A page of tens of thousands of sessions takes a while to free.
        let before = self.page.replace((loaded.shelf, loaded.page));
        cx.background_spawn(async move { drop(before) }).detach();
        if let Some(folder) = self.created
            && self.folder(folder).is_some()
        {
            self.created = None;
            cx.emit(LibraryEvent::FolderCreated(folder));
        }
    }

    /// Runs `change`, one of the owner's, on the background executor, then `done` with what
    /// it returned. A change the catalogue refuses is logged and the page read again.
    fn write<T: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&Archive) -> Result<T, CatalogueError> + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
    ) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        self.writes += 1;
        let job = cx.background_spawn(async move { change(&archive) });
        cx.spawn(async move |this, cx| {
            let changed = job.await;
            this.update(cx, |this, cx| {
                this.writes -= 1;
                match changed {
                    Ok(value) => done(this, value, cx),
                    Err(error) => {
                        log::error!("a change to the library was refused: {error}");
                        this.reload(cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}
