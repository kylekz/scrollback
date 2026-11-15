//! The library (PLAN §8, §11 `Library`): the page shown, what is selected on it, and what the
//! sidebar and the pages read from the catalogue.
//!
//! **Reads.** Everything is read in one background job per load ([`load`]): the counts beside
//! the sidebar's rows, the folders, and the shown page's sessions with what its header says of
//! them. Views draw from what the last load left here and never query. A load runs when the
//! archive opens, when another page is shown, after each of the owner's changes, and when sync
//! says the archive changed; a load asked for while one runs follows it, so loads never pile
//! up. All sessions is kept while another page shows, and shown again as it was when nothing
//! changed meanwhile.
//!
//! **The owner's changes** (a star, a new folder, a name, a path attached) go through one
//! queue and are written one at a time, in the order they were made, each in a background
//! job. The page is read again after every one, and a load that started before a change was
//! made or written is not taken in: what it read may be from before the change. A star is also
//! shown at once, in place. When the app quits, what is still queued is written within gpui's
//! shutdown window.

use std::cmp::Reverse;
use std::collections::{HashSet, VecDeque};
use std::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};

use gpui::{AppContext as _, Context, Entity, EventEmitter, Subscription};
use sb_core::archive::Archive;
use sb_core::catalogue::{
    CatalogueError, Filed, Folder, LibraryCounts, PathGroup, Session, SessionId, Shelf,
};
use sb_core::filing::{self, AttachedPath, FolderId, NormalPath};
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

    /// Where the session `id` is: at `hint`, where it was when the page was last drawn, or
    /// wherever a load has moved it since.
    fn find(&self, id: &SessionId, hint: usize) -> Option<usize> {
        match self.sessions.get(hint) {
            Some(session) if session.id == *id => Some(hint),
            _ => self.sessions.iter().position(|session| session.id == *id),
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

/// What tells one group of Unsorted from another across loads: its path, as filing compares
/// it or as it was recorded when filing can't, and its location unless it is a drive's.
pub fn group_key(group: &PathGroup) -> String {
    let project = group.project.as_ref();
    format!(
        "{}\n{}\n{}",
        project.map_or("", NormalPath::as_str),
        match project {
            Some(_) => "",
            None => group.spellings.first().map_or("", String::as_str),
        },
        match group.locations.first() {
            Some(location) if !project.is_some_and(NormalPath::is_drive) => location.as_str(),
            _ => "",
        }
    )
}

/**
 * How a group's path is written on its chip, and the other ways its sessions recorded it. A
 * drive's path is shown in its drive form (DESIGN §4.6: `D:\Projects\fc`, "also as
 * /mnt/d/Projects/fc"), which is made from a mount path when no session recorded it so.
 * `None` for the sessions that recorded no path.
 */
pub fn group_path(group: &PathGroup) -> Option<(String, Vec<String>)> {
    let on_drive = |path: &str| {
        let bytes = path.as_bytes();
        bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
    };
    let first = group.spellings.first()?;
    if !group.project.as_ref().is_some_and(NormalPath::is_drive) {
        return Some((first.clone(), group.spellings[1..].to_vec()));
    }
    let shown = match group.spellings.iter().find(|spelling| on_drive(spelling)) {
        Some(drive) => drive.clone(),
        None => {
            let below = first.strip_prefix(filing::WSL_DRIVE_MOUNT).unwrap_or(first);
            let (letter, rest) = below.split_at(below.len().min(1));
            format!("{}:{}", letter.to_uppercase(), rest.replace('/', "\\"))
        }
    };
    let others = group
        .spellings
        .iter()
        .filter(|spelling| **spelling != shown)
        .cloned()
        .collect();
    Some((shown, others))
}

/// Whether a folder can be made from a group's path: only a path that says where it is from
/// the top can be attached (PLAN §8).
pub fn can_attach(group: &PathGroup) -> bool {
    group.project.as_ref().is_some_and(NormalPath::is_absolute)
}

/// What one load reads.
pub(super) struct Loaded {
    shelf: Shelf,
    counts: LibraryCounts,
    folders: Vec<Folder>,
    agents: usize,
    pub(super) page: Page,
}

/// Reads what the sidebar and the page of `shelf` show.
pub(super) fn load(archive: &Archive, shelf: Shelf) -> Result<Loaded, CatalogueError> {
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

/// What the service does on the UI thread with what a change returned.
type Applied = Box<dyn FnOnce(&mut LibraryService, &mut Context<LibraryService>) + Send>;
/// One of the owner's changes, waiting its turn: what it does to the archive, off the UI
/// thread.
type Change = Box<dyn FnOnce(&Archive) -> Applied + Send>;

pub struct LibraryService {
    archive: Entity<ArchiveService>,
    shelf: Shelf,
    counts: LibraryCounts,
    folders: Vec<Folder>,
    /// How many agents the archive holds sessions of (DESIGN §4.2.10).
    agents: usize,
    /// The page last loaded, and the shelf it is of.
    page: Option<(Shelf, Page)>,
    /// All sessions as it was last loaded, while another page shows, and what [`Self::state`]
    /// was then.
    all: Option<(Page, u64)>,
    /// Counts what may have changed the archive: the owner's changes as they are made and as
    /// they are written, and sync's news. A load is of the state it started in.
    state: u64,
    /// The state the shown page was loaded in.
    page_state: u64,
    /// The selected session, and where it is among a sessions page's.
    selected: Option<(SessionId, Option<usize>)>,
    /// The session whose reader is open.
    open: Option<Session>,
    loading: bool,
    /// A load was asked for while one ran.
    stale: bool,
    /// Why the last load failed.
    failure: Option<String>,
    /// The owner's changes still to write, the next first. Whoever writes takes the first,
    /// while holding `turn`: the job of each change, and at quit one job for all that is left.
    queue: Arc<Mutex<VecDeque<Change>>>,
    turn: Arc<Mutex<()>>,
    /// How many changes are queued or being written.
    queued: usize,
    /// A job is writing the next change.
    writing: bool,
    /// A folder to announce once a load has it.
    created: Option<FolderId>,
    /// The groups of Unsorted a folder is being made from, by [`group_key`].
    creating: HashSet<String>,
    /// The group whose folder the catalogue refused to make, and why.
    refused: Option<(String, String)>,
    _observers: [Subscription; 3],
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
                cx.subscribe(&sync, |this, _, _: &SyncEvent, cx| {
                    this.state += 1;
                    this.reload(cx);
                }),
                cx.on_app_quit(Self::write_what_is_queued),
            ],
            archive,
            shelf: Shelf::All,
            counts: LibraryCounts::default(),
            folders: Vec::new(),
            agents: 0,
            page: None,
            all: None,
            state: 0,
            page_state: 0,
            selected: None,
            open: None,
            loading: false,
            stale: false,
            failure: None,
            queue: Arc::default(),
            turn: Arc::default(),
            queued: 0,
            writing: false,
            created: None,
            creating: HashSet::new(),
            refused: None,
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

    /// Unsorted's groups, when Unsorted is the page shown and loaded.
    pub fn groups(&self) -> &[PathGroup] {
        match self.page() {
            Some(Page::Unsorted(groups)) => groups,
            _ => &[],
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

    /// Why the library could not be read, when the last load failed.
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// Whether a folder is being made from the group of Unsorted with the key `group`.
    pub fn is_creating(&self, group: &str) -> bool {
        self.creating.contains(group)
    }

    /// Why the catalogue refused to make a folder from the group with the key `group`.
    pub fn refusal(&self, group: &str) -> Option<&str> {
        self.refused
            .as_ref()
            .filter(|(refused, _)| refused == group)
            .map(|(_, why)| why.as_str())
    }

    /// Nothing is being read or written.
    pub fn is_settled(&self) -> bool {
        !self.loading && self.queued == 0
    }

    /// Shows the page of `shelf`.
    pub fn show(&mut self, shelf: Shelf, cx: &mut Context<Self>) {
        if self.shelf == shelf && self.open.is_none() {
            return;
        }
        self.open = None;
        if self.shelf != shelf {
            if let Some((Shelf::All, page)) = self.page.take_if(|(of, _)| *of == Shelf::All) {
                self.all = Some((page, self.page_state));
            }
            self.shelf = shelf;
            self.selected = None;
            self.refused = None;
            match self.all.take_if(|_| shelf == Shelf::All) {
                // Nothing changed since it was loaded: it is shown as it was.
                Some((page, state)) if state == self.state && !self.loading => {
                    self.set_page(Shelf::All, page, state, cx);
                }
                kept => {
                    self.discard(kept, cx);
                    self.reload(cx);
                }
            }
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

    /// Selects the session `id`, which was at `hint` of the shown page's sessions when it was
    /// last drawn.
    pub fn select_session(&mut self, id: &SessionId, hint: usize, cx: &mut Context<Self>) {
        if let Some(at) = self.sessions().and_then(|page| page.find(id, hint)) {
            self.selected = Some((id.clone(), Some(at)));
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

    /// Selects and opens the session `id` of the shown page's sessions, last drawn at `hint`.
    pub fn open_session_at(&mut self, id: &SessionId, hint: usize, cx: &mut Context<Self>) {
        let Some(page) = self.sessions() else {
            return;
        };
        if let Some(at) = page.find(id, hint) {
            self.open = Some(page.sessions[at].clone());
            self.selected = Some((id.clone(), Some(at)));
            cx.notify();
        }
    }

    /// Goes back from the reader to the page it was opened from.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if self.open.take().is_some() {
            cx.notify();
        }
    }

    /// Stars the session `id` of the shown page's sessions, last drawn at `hint`, or takes its
    /// star away.
    pub fn toggle_star(&mut self, id: &SessionId, hint: usize, cx: &mut Context<Self>) {
        let Some((_, Page::Sessions(page))) = &mut self.page else {
            return;
        };
        let Some(at) = page.find(id, hint) else {
            return;
        };
        let session = &mut page.sessions[at];
        let starred = !session.starred;
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
            |_, changed, _| {
                if let Err(error) = changed {
                    log::error!("a star was refused: {error}");
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
            |this, created, _| match created {
                Ok(folder) => this.created = Some(folder),
                Err(error) => log::error!("a folder was refused: {error}"),
            },
        );
    }

    pub fn rename_folder(&mut self, folder: FolderId, name: String, cx: &mut Context<Self>) {
        self.write(
            cx,
            move |archive| archive.catalogue().rename_folder(folder, &name),
            |_, renamed, _| {
                if let Err(error) = renamed {
                    log::error!("a folder's new name was refused: {error}");
                }
            },
        );
    }

    /**
     * Makes a top-level folder named after the last component of a group of Unsorted's path
     * with the path attached to it, which files the group there (DESIGN §4.6); `group` is the
     * group's [`group_key`]. The path is attached for the group's location; a group that
     * several locations share, which only a drive's path is, has it attached for any. The
     * folder and its path are one change: when the catalogue refuses, there is no folder, and
     * [`Self::refusal`] says why.
     */
    pub fn create_folder_from(&mut self, group: &str, cx: &mut Context<Self>) {
        if self.creating.contains(group) {
            return;
        }
        let Some(found) = self
            .groups()
            .iter()
            .find(|it| group_key(it) == group && can_attach(it))
        else {
            return;
        };
        let Some((typed, _)) = group_path(found) else {
            return;
        };
        let Some(name) = format::last_component(&typed).map(str::to_string) else {
            return;
        };
        let location = match &found.locations[..] {
            [only] => Some(only.clone()),
            _ => None,
        };
        let key = group.to_string();
        self.creating.insert(key.clone());
        self.refused = None;
        self.write(
            cx,
            move |archive| {
                archive
                    .catalogue()
                    .create_folder_with_path(&name, None, &typed, location.as_ref())
            },
            move |this, created, _| match created {
                Ok(folder) => this.created = Some(folder),
                Err(error) => this.refused = Some((key, error.to_string())),
            },
        );
        cx.notify();
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
        let (shelf, state) = (self.shelf, self.state);
        let job = cx.background_spawn(async move { load(&archive, shelf) });
        cx.spawn(async move |this, cx| {
            let loaded = job.await;
            this.update(cx, |this, cx| {
                this.loading = false;
                let current = state == this.state && shelf == this.shelf;
                match loaded {
                    // What it read may be from before a change: the load that follows is not.
                    Ok(loaded) if current => this.loaded(loaded, state, cx),
                    Ok(_) => {}
                    Err(error) => {
                        log::error!("the library could not be read: {error}");
                        this.failure = Some(error.to_string());
                    }
                }
                if std::mem::take(&mut this.stale) || !current {
                    this.reload(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn loaded(&mut self, loaded: Loaded, state: u64, cx: &mut Context<Self>) {
        self.failure = None;
        self.counts = loaded.counts;
        self.folders = loaded.folders;
        self.agents = loaded.agents;
        if let Some((id, at)) = &mut self.selected
            && let Page::Sessions(page) = &loaded.page
        {
            // The selected session may have moved: sessions arrive while a sync runs.
            *at = page.find(id, at.unwrap_or(0));
        }
        self.set_page(loaded.shelf, loaded.page, state, cx);
        // The folders being made are made, or refused, by now.
        self.creating.clear();
        if let Some(folder) = self.created
            && self.folder(folder).is_some()
        {
            self.created = None;
            cx.emit(LibraryEvent::FolderCreated(folder));
        }
    }

    fn set_page(&mut self, shelf: Shelf, page: Page, state: u64, cx: &mut Context<Self>) {
        self.page_state = state;
        let before = self.page.replace((shelf, page));
        self.discard(before, cx);
    }

    /// Frees a page off the UI thread: tens of thousands of sessions take a while.
    fn discard<T: Send + 'static>(&self, page: Option<T>, cx: &mut Context<Self>) {
        if let Some(page) = page {
            cx.background_spawn(async move { drop(page) }).detach();
        }
    }

    /// Queues `change`, one of the owner's, to be written after those made before it, and
    /// `done` to hear how it went. The page is read again once it is written.
    fn write<T: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&Archive) -> Result<T, CatalogueError> + Send + 'static,
        done: impl FnOnce(&mut Self, Result<T, CatalogueError>, &mut Context<Self>) + Send + 'static,
    ) {
        self.state += 1;
        self.queued += 1;
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(Box::new(move |archive| {
                let changed = change(archive);
                Box::new(move |this, cx| done(this, changed, cx))
            }));
        self.write_next(cx);
    }

    /// Starts the job that writes the next queued change, unless one is running.
    fn write_next(&mut self, cx: &mut Context<Self>) {
        if self.writing || self.queued == 0 {
            return;
        }
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        self.writing = true;
        let (queue, turn) = (self.queue.clone(), self.turn.clone());
        let job = cx.background_spawn(async move {
            let _turn = turn.lock().unwrap_or_else(PoisonError::into_inner);
            let next = queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front();
            next.map(|change| change(&archive))
        });
        cx.spawn(async move |this, cx| {
            let applied = job.await;
            this.update(cx, |this, cx| {
                this.writing = false;
                this.queued -= 1;
                this.state += 1;
                if let Some(applied) = applied {
                    applied(this, cx);
                }
                this.reload(cx);
                this.write_next(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// At quit: writes what is still queued, in its order, for as long as gpui waits.
    fn write_what_is_queued(&mut self, cx: &mut Context<Self>) -> impl Future<Output = ()> + use<> {
        let archive = self.archive.read(cx).archive().cloned();
        let (queue, turn) = (self.queue.clone(), self.turn.clone());
        cx.background_spawn(async move {
            let Some(archive) = archive else {
                return;
            };
            let _turn = turn.lock().unwrap_or_else(PoisonError::into_inner);
            let next = || {
                queue
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .pop_front()
            };
            while let Some(change) = next() {
                drop(change(&archive));
            }
        })
    }
}
