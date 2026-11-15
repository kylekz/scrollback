//! The library (PLAN §8, §11 `Library`): the page shown, what is selected on it, and what the
//! sidebar and the pages read from the catalogue.
//!
//! **Reads.** Everything is read in one background job per load ([`load`]): the counts beside
//! the sidebar's rows, the folders, and the shown page's sessions with what its header says of
//! them. Views draw from what the last load left here and never query. A load runs when the
//! archive opens, when another page is shown, once the owner's changes are written, and when
//! sync says the archive changed; a load asked for while one runs follows it, so loads never
//! pile up. A load is a consistent picture of the catalogue whenever it started, so sync's news
//! never throws one away: the load that follows has the news. All sessions is kept while
//! another page shows, and shown again as it was when nothing changed meanwhile.
//!
//! **The owner's changes** (a star, a new folder, a name, a path attached) wait in one queue
//! on the UI thread and are written one at a time, in the order they were made, each in a
//! background job. No load starts while any waits or is being written, and one that started
//! before a change was made is not taken in: what it read is from before the change. The last
//! write's end reads the page again. A star is also shown at once, in place. A change the
//! catalogue refuses is said in a banner ([`LibraryService::write_failure`]), or for a folder
//! made from Unsorted under its group, and the page read again shows what is kept.
//!
//! **At quit** what is still queued is written in one background job, for as long as gpui
//! waits for it: [`gpui::SHUTDOWN_TIMEOUT`], 200 ms. A change queued behind a pass's last batch
//! may need longer to reach the database; what is not known to be written by then is logged as
//! what may not have been: the job writing the change in hand may still finish it.

use std::cmp::Reverse;
use std::collections::{HashSet, VecDeque};
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
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
 * /mnt/d/Projects/fc"), which the catalogue read by its location's drive mount when no
 * session recorded it so. `None` for the sessions that recorded no path.
 */
pub fn group_path(group: &PathGroup) -> Option<(String, Vec<String>)> {
    let first = group.spellings.first()?;
    let Some(shown) = group.drive.clone() else {
        return Some((first.clone(), group.spellings[1..].to_vec()));
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

/// One of the owner's changes, waiting its turn to be written.
enum Change {
    Star {
        id: SessionId,
        starred: bool,
    },
    NewFolder {
        name: String,
        parent: Option<FolderId>,
    },
    Rename {
        folder: FolderId,
        name: String,
    },
    /// A top-level folder with a path attached, made from the group of Unsorted whose
    /// [`group_key`] is `group`.
    FolderFrom {
        group: String,
        name: String,
        typed: String,
        location: Option<LocationId>,
    },
}

impl Change {
    /// Writes it, off the UI thread. A folder made is returned.
    fn write(&self, archive: &Archive) -> Result<Option<FolderId>, CatalogueError> {
        let catalogue = archive.catalogue();
        match self {
            Self::Star { id, starred } => catalogue.set_starred(id, *starred).map(|()| None),
            Self::NewFolder { name, parent } => catalogue.create_folder(name, *parent).map(Some),
            Self::Rename { folder, name } => catalogue.rename_folder(*folder, name).map(|()| None),
            Self::FolderFrom {
                name,
                typed,
                location,
                ..
            } => catalogue
                .create_folder_with_path(name, None, typed, location.as_ref())
                .map(Some),
        }
    }

    /// What it is, for the owner and the log: "The name “work”" and the like.
    fn what(&self) -> String {
        match self {
            Self::Star { starred: true, .. } => "The star".to_string(),
            Self::Star { starred: false, .. } => "Removing the star".to_string(),
            Self::NewFolder { name, .. } => format!("The new folder “{name}”"),
            Self::Rename { name, .. } => format!("The name “{name}”"),
            Self::FolderFrom { name, typed, .. } => format!("The folder “{name}” for {typed}"),
        }
    }
}

/// The changes a quit is writing, and how many of them are written: those that are not known
/// to be when gpui stops waiting, and this is dropped, are logged.
struct Unwritten {
    changes: Vec<String>,
    written: Arc<AtomicUsize>,
}

impl Drop for Unwritten {
    fn drop(&mut self) {
        let written = self.written.load(Ordering::Acquire).min(self.changes.len());
        let left = &self.changes[written..];
        if !left.is_empty() {
            log::error!(
                "the app quit before {} of the owner's changes were seen written; they may not \
                 have been written: {}",
                left.len(),
                left.join("; ")
            );
        }
    }
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
    /// All sessions as it was last loaded, while another page shows, and what its load saw.
    all: Option<(Page, Seen)>,
    /// What a load starting now would see.
    seen: Seen,
    /// What the load of the shown page saw.
    page_seen: Seen,
    /// The selected session, and where it is among a sessions page's.
    selected: Option<(SessionId, Option<usize>)>,
    /// The session whose reader is open.
    open: Option<Session>,
    loading: bool,
    /// A load was asked for while one ran or while changes were written.
    stale: bool,
    /// Why the last load failed.
    failure: Option<String>,
    /// The owner's changes still to write, the next first.
    queue: VecDeque<Change>,
    /// What the change being written is, while one is.
    writing: Option<String>,
    /// The change handed to a job to write, until the job takes it. Whoever writes holds the
    /// lock while it does: the job, or at quit the one job for all that is left, which so
    /// writes the change in hand before the rest, itself if the job has not started.
    in_hand: Arc<Mutex<Option<Change>>>,
    /// Why the last of the owner's changes the catalogue refused was refused, until the owner
    /// has read it.
    write_failure: Option<String>,
    /// A folder to announce once a load has it.
    created: Option<FolderId>,
    /// The groups of Unsorted a folder is being made from, by [`group_key`].
    creating: HashSet<String>,
    /// The group whose folder the catalogue refused to make, and why.
    refused: Option<(String, String)>,
    _observers: [Subscription; 3],
}

/// What a load saw when it started: how many of the owner's changes had been made, and how
/// many times sync had said the archive changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Seen {
    changes: u64,
    news: u64,
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
                    this.seen.news += 1;
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
            seen: Seen::default(),
            page_seen: Seen::default(),
            selected: None,
            open: None,
            loading: false,
            stale: false,
            failure: None,
            queue: VecDeque::new(),
            writing: None,
            in_hand: Arc::default(),
            write_failure: None,
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

    /// Why the last of the owner's changes the catalogue refused was refused, until
    /// [`Self::dismiss_write_failure`].
    pub fn write_failure(&self) -> Option<&str> {
        self.write_failure.as_deref()
    }

    pub fn dismiss_write_failure(&mut self, cx: &mut Context<Self>) {
        if self.write_failure.take().is_some() {
            cx.notify();
        }
    }

    /// Nothing is being read or written.
    pub fn is_settled(&self) -> bool {
        !self.loading && self.writing.is_none() && self.queue.is_empty()
    }

    /// Shows the page of `shelf`.
    pub fn show(&mut self, shelf: Shelf, cx: &mut Context<Self>) {
        if self.shelf == shelf && self.open.is_none() {
            return;
        }
        self.open = None;
        if self.shelf != shelf {
            if let Some((Shelf::All, page)) = self.page.take_if(|(of, _)| *of == Shelf::All) {
                self.all = Some((page, self.page_seen));
            }
            self.shelf = shelf;
            self.selected = None;
            self.refused = None;
            match self.all.take_if(|_| shelf == Shelf::All) {
                // Nothing changed since it was loaded: it is shown as it was.
                Some((page, seen)) if seen == self.seen && !self.loading => {
                    self.set_page(Shelf::All, page, seen, cx);
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
        self.write(
            Change::Star {
                id: id.clone(),
                starred,
            },
            cx,
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
        self.write(Change::NewFolder { name, parent }, cx);
    }

    pub fn rename_folder(&mut self, folder: FolderId, name: String, cx: &mut Context<Self>) {
        self.write(Change::Rename { folder, name }, cx);
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
        self.creating.insert(group.to_string());
        self.refused = None;
        self.write(
            Change::FolderFrom {
                group: group.to_string(),
                name,
                typed,
                location,
            },
            cx,
        );
        cx.notify();
    }

    /// Reads the shown page and the sidebar's counts again: now, or once the load running and
    /// the owner's changes waiting are over (the module docs).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        if self.loading || self.writing.is_some() || !self.queue.is_empty() {
            self.stale = true;
            return;
        }
        self.loading = true;
        let (shelf, seen) = (self.shelf, self.seen);
        let job = cx.background_spawn(async move { load(&archive, shelf) });
        cx.spawn(async move |this, cx| {
            let loaded = job.await;
            this.update(cx, |this, cx| {
                this.loading = false;
                // A change made since it started may be missing from it: one is still to come.
                let current = seen.changes == this.seen.changes && shelf == this.shelf;
                match loaded {
                    Ok(loaded) if current => this.loaded(loaded, seen, cx),
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

    fn loaded(&mut self, loaded: Loaded, seen: Seen, cx: &mut Context<Self>) {
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
        self.set_page(loaded.shelf, loaded.page, seen, cx);
        // The folders being made are made, or refused, by now.
        self.creating.clear();
        if let Some(folder) = self.created
            && self.folder(folder).is_some()
        {
            self.created = None;
            cx.emit(LibraryEvent::FolderCreated(folder));
        }
    }

    fn set_page(&mut self, shelf: Shelf, page: Page, seen: Seen, cx: &mut Context<Self>) {
        self.page_seen = seen;
        let before = self.page.replace((shelf, page));
        self.discard(before, cx);
    }

    /// Frees a page off the UI thread: tens of thousands of sessions take a while.
    fn discard<T: Send + 'static>(&self, page: Option<T>, cx: &mut Context<Self>) {
        if let Some(page) = page {
            cx.background_spawn(async move { drop(page) }).detach();
        }
    }

    /// Queues one of the owner's changes, to be written after those made before it.
    fn write(&mut self, change: Change, cx: &mut Context<Self>) {
        self.seen.changes += 1;
        self.queue.push_back(change);
        self.write_next(cx);
    }

    /// Writes the next queued change, unless one is being written; with none left, reads the
    /// page again.
    fn write_next(&mut self, cx: &mut Context<Self>) {
        if self.writing.is_some() {
            return;
        }
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        let Some(change) = self.queue.pop_front() else {
            self.reload(cx);
            return;
        };
        self.writing = Some(change.what());
        // Free: the last job let go of it before its end reached here.
        *self.in_hand.lock().unwrap_or_else(PoisonError::into_inner) = Some(change);
        let in_hand = self.in_hand.clone();
        let job = cx.background_spawn(async move {
            let mut in_hand = in_hand.lock().unwrap_or_else(PoisonError::into_inner);
            // Gone when the app quit before the job started: the quit wrote it.
            let change = in_hand.take()?;
            let written = change.write(&archive);
            Some((change, written))
        });
        cx.spawn(async move |this, cx| {
            let Some((change, written)) = job.await else {
                return;
            };
            this.update(cx, |this, cx| {
                this.writing = None;
                this.written(change, written);
                this.write_next(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Takes in how a change's write went. A refusal is said to the owner: under the group of
    /// Unsorted a folder was to be made from, or in [`Self::write_failure`].
    fn written(&mut self, change: Change, written: Result<Option<FolderId>, CatalogueError>) {
        match (change, written) {
            (_, Ok(folder)) => self.created = folder.or(self.created),
            (Change::FolderFrom { group, .. }, Err(error)) => {
                self.refused = Some((group, error.to_string()));
            }
            (change, Err(error)) => {
                log::warn!("{} was refused: {error}", change.what());
                self.write_failure = Some(format!(
                    "{} couldn't be saved: {}.",
                    change.what(),
                    error.to_string().trim_end_matches('.')
                ));
            }
        }
    }

    /// At quit: writes what is still queued, in its order, after the change in hand, for as
    /// long as gpui waits (the module docs).
    fn write_what_is_queued(&mut self, cx: &mut Context<Self>) -> impl Future<Output = ()> + use<> {
        let archive = self.archive.read(cx).archive().cloned();
        let changes: Vec<Change> = self.queue.drain(..).collect();
        let written = Arc::new(AtomicUsize::new(0));
        let unwritten = Unwritten {
            changes: self
                .writing
                .iter()
                .cloned()
                .chain(changes.iter().map(Change::what))
                .collect(),
            written: written.clone(),
        };
        let writing = self.writing.is_some();
        let in_hand = self.in_hand.clone();
        let job = cx.background_spawn(async move {
            let Some(archive) = archive else {
                return;
            };
            let mut in_hand = in_hand.lock().unwrap_or_else(PoisonError::into_inner);
            let waiting = in_hand.take();
            if writing && waiting.is_none() {
                // Its job took it, and has written it by now.
                written.fetch_add(1, Ordering::Release);
            }
            for change in waiting.into_iter().chain(changes) {
                if let Err(error) = change.write(&archive) {
                    log::warn!("{} was refused: {error}", change.what());
                }
                written.fetch_add(1, Ordering::Release);
            }
        });
        async move {
            job.await;
            drop(unwritten);
        }
    }
}
