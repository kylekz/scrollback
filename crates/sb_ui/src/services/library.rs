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
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Subscription, Task};
use sb_core::archive::{Archive, ArchiveError, Deleted};
use sb_core::catalogue::{
    Before, CatalogueError, Deletion, Filed, Folder, FolderPath, LibraryCounts, PathGroup,
    Restored, Session, SessionId, Shelf, Undone,
};
use sb_core::filing::{self, AttachedPath, FolderId, NormalPath};
use sb_core::location::LocationId;

use super::drag::DragState;
use super::read_at::ReadAt;
use super::view::{self, PageView, UnsortedView};
use super::{ArchiveService, SyncEvent, SyncService};
use crate::format;
use crate::hostile::label;

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
    /// The path as filing compares it.
    pub path: NormalPath,
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
    /// On Trash's page, what each session holds in the archive (DESIGN §4.8's Size); empty
    /// on the others.
    pub sizes: HashMap<SessionId, u64>,
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
            locations.extend(session.ran_at());
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
            sizes: HashMap::new(),
        }
    }

    /// Where the session `id` is: at `hint`, where it was when the page was last drawn, or
    /// wherever a load has moved it since.
    pub fn find(&self, id: &SessionId, hint: usize) -> Option<usize> {
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
/// it or as it was recorded when filing can't, and its location unless it is a drive's, or
/// what its sessions are copies as.
pub fn group_key(group: &PathGroup) -> String {
    let project = group.project.as_ref();
    format!(
        "{}\n{}\n{}\n{}",
        group.copy.as_deref().unwrap_or_default(),
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
    attached: Vec<FolderPath>,
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
        Shelf::All | Shelf::Starred | Shelf::Folder(_) | Shelf::Trash => {
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
                            && let Some(by) = filing::file(project, session.ran_at(), &attached)
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
                            path: path.attached.path,
                            location: path.attached.location,
                            sessions,
                        })
                        .collect()
                }
                _ => Vec::new(),
            };
            let mut page = SessionsPage::new(sessions, paths);
            if shelf == Shelf::Trash {
                page.sizes = catalogue.trash_sizes()?;
            }
            Page::Sessions(page)
        }
    };
    Ok(Loaded {
        shelf,
        counts: catalogue.library_counts()?,
        /* A folder may be named after a transcript's project path, and one named before names
        were cleaned may hold anything: each is drawn as it is read here. */
        folders: (catalogue.folders()?.into_iter())
            .map(|folder| Folder {
                name: label(&folder.name),
                ..folder
            })
            .collect(),
        attached: catalogue.attached_paths()?,
        agents: agents.len(),
        page,
    })
}

pub enum LibraryEvent {
    /// A folder the owner just made is among the folders now: the sidebar renames it in
    /// place (DESIGN §4.6).
    FolderCreated(FolderId),
    /// The owner asked to rename a folder from a menu: the sidebar renames it in place.
    Rename(FolderId),
    /// The owner asked to edit a folder's paths, whose page shows now (DESIGN §4.7).
    EditPaths(FolderId),
    /// The owner closed the window, or quit, and the paths editor's changes were saved or
    /// dropped: it closes, or the app quits, now.
    Leave(Leave),
    /// A session was deleted for good: whatever keeps it (the palette's recent sessions)
    /// forgets it.
    DeletedForGood(SessionId),
}

/// A project path on a folder as the owner typed it, and the one location it is for: how the
/// changes name a path, so that one is written as the catalogue reads it then.
pub type TypedPath = (String, Option<LocationId>);

/// One of the owner's changes, waiting its turn to be written.
#[derive(Clone, Debug)]
pub enum Change {
    Star {
        ids: Vec<SessionId>,
        starred: bool,
    },
    /// A folder, and the sessions filed in it by hand once it is made (Move to folder's New
    /// folder…, DESIGN §4.5).
    NewFolder {
        name: String,
        parent: Option<FolderId>,
        sessions: Vec<SessionId>,
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
    FileByHand {
        ids: Vec<SessionId>,
        folder: FolderId,
    },
    FileAutomatically {
        ids: Vec<SessionId>,
    },
    /// Puts sessions back as they were filed: an Undo.
    Refile {
        before: Vec<Before>,
    },
    Trash {
        ids: Vec<SessionId>,
    },
    /// Takes sessions out of Trash: an Undo.
    Restore {
        ids: Vec<SessionId>,
    },
    /// Takes sessions out of Trash from its page (DESIGN §4.8), saying where each went.
    RestoreFromTrash {
        ids: Vec<SessionId>,
    },
    /// Deletes a session in Trash for good: exactly what its confirmation showed.
    DeleteForGood {
        deletion: Deletion,
        session: Box<Session>,
    },
    /// A folder's paths taken off and put on in one change: the editor's Save, attaching a
    /// group's path, and their Undo.
    ChangePaths {
        folder: FolderId,
        detach: Vec<TypedPath>,
        attach: Vec<TypedPath>,
    },
    MovePath {
        from: FolderId,
        path: TypedPath,
        to: FolderId,
    },
    MoveFolder {
        folder: FolderId,
        parent: Option<FolderId>,
        position: usize,
    },
    DeleteFolder {
        folder: FolderId,
    },
}

/**
 * What writing a change did, as the catalogue said in the change's own transaction: what the
 * toast says and its Undo put back are made from it.
 */
enum Written {
    Done,
    /// A folder was made.
    Made(FolderId),
    /// A folder was deleted, and these of its subfolders renamed, from what to what, as they
    /// moved up beside folders of their name.
    Deleted(Vec<(String, String)>),
    /// Sessions were filed by hand in the folder; how they were before.
    Filed(FolderId, Vec<Before>),
    /// Sessions were handed back to the rules; how they were before, and where they are now.
    Handed(Vec<(Before, Option<FolderId>)>),
    /// These sessions were moved to Trash; any others were there already.
    Trashed(Vec<SessionId>),
    /// An Undo, and what it could not put back.
    Undone(Undone),
    /// Sessions were taken out of Trash: where each went.
    Restored(Restored),
    /// A session was deleted for good.
    DeletedForGood(Deleted),
}

impl Change {
    /// Writes it, off the UI thread, each in one transaction.
    fn write(&self, archive: &Archive) -> Result<Written, ArchiveError> {
        let catalogue = archive.catalogue();
        Ok(match self {
            Self::Star { ids, starred } => {
                catalogue.set_starred_all(ids, *starred)?;
                Written::Done
            }
            Self::NewFolder {
                name,
                parent,
                sessions,
            } => Written::Made(catalogue.create_folder_with(name, *parent, sessions)?),
            Self::Rename { folder, name } => {
                catalogue.rename_folder(*folder, name)?;
                Written::Done
            }
            Self::FolderFrom {
                name,
                typed,
                location,
                ..
            } => Written::Made(catalogue.create_folder_with_path(
                name,
                None,
                typed,
                location.as_ref(),
            )?),
            Self::FileByHand { ids, folder } => {
                Written::Filed(*folder, catalogue.file_by_hand_all(ids, *folder)?)
            }
            Self::FileAutomatically { ids } => {
                Written::Handed(catalogue.file_automatically_all(ids)?)
            }
            Self::Refile { before } => Written::Undone(catalogue.refile_as(before)?),
            Self::Trash { ids } => Written::Trashed(catalogue.trash_all(ids)?),
            Self::Restore { ids } => Written::Undone(Undone {
                gone: catalogue.restore_all(ids)?.gone,
                refiled: 0,
            }),
            Self::RestoreFromTrash { ids } => Written::Restored(catalogue.restore_all(ids)?),
            Self::DeleteForGood { deletion, .. } => {
                Written::DeletedForGood(archive.delete_for_good(deletion)?)
            }
            Self::ChangePaths {
                folder,
                detach,
                attach,
            } => {
                catalogue.change_paths(*folder, detach, attach)?;
                Written::Done
            }
            Self::MovePath {
                from,
                path: (typed, location),
                to,
            } => {
                catalogue.move_path(*from, (typed, location.as_ref()), *to)?;
                Written::Done
            }
            Self::MoveFolder {
                folder,
                parent,
                position,
            } => {
                catalogue.move_folder(*folder, *parent, *position)?;
                Written::Done
            }
            Self::DeleteFolder { folder } => Written::Deleted(catalogue.delete_folder(*folder)?),
        })
    }

    /// What it is, for the owner and the log: "The name “work”" and the like, what it names
    /// drawn as a label ([`label`]: a name or a path may be a transcript's).
    fn what(&self) -> String {
        match self {
            Self::Star { starred: true, .. } => "The star".to_string(),
            Self::Star { starred: false, .. } => "Removing the star".to_string(),
            Self::NewFolder { name, .. } => format!("The new folder “{}”", label(name)),
            Self::Rename { name, .. } => format!("The name “{}”", label(name)),
            Self::FolderFrom { name, typed, .. } => {
                format!("The folder “{}” for {}", label(name), label(typed))
            }
            Self::FileByHand { .. } | Self::FileAutomatically { .. } => "Filing".to_string(),
            Self::Refile { .. } | Self::Restore { .. } => "Undoing".to_string(),
            Self::Trash { .. } => "Moving to Trash".to_string(),
            Self::RestoreFromTrash { .. } => "Restoring".to_string(),
            Self::DeleteForGood { session, .. } => {
                format!("Deleting “{}” for good", title_of(session))
            }
            Self::ChangePaths { .. } => "The change of paths".to_string(),
            Self::MovePath { path, .. } => format!("Moving {}", label(&path.0)),
            Self::MoveFolder { .. } => "Moving the folder".to_string(),
            Self::DeleteFolder { .. } => "Deleting the folder".to_string(),
        }
    }
}

/// A note at the bottom of the main area that something was done, with a way to undo it
/// (DESIGN §4.14), shown once its change is written.
#[derive(Clone, Debug)]
pub struct Toast {
    /// What it says. What it names (a session's title, a project's path, a folder) is a label
    /// already ([`label`]).
    pub text: String,
    undo: Option<Change>,
}

impl Toast {
    pub fn can_undo(&self) -> bool {
        self.undo.is_some()
    }
}

/// A session the owner asked to delete for good (DESIGN §4.8), and what that would remove.
#[derive(Clone, Debug)]
pub struct ForGood {
    pub session: Session,
    /// What deleting it removes, once it is read.
    pub deletion: Option<Deletion>,
    /// Why the last confirmation was refused: the session changed since it was shown, and
    /// these are its numbers now, or its location is being synced (PLAN §4.5).
    pub refused: Option<Refusal>,
}

/// Why a confirmation to delete for good was refused, and nothing deleted (PLAN §4.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The session changed since it was shown: the confirmation shows its numbers now.
    Changed,
    /// The location of it or of a copy of it that goes with it was being synced; the
    /// confirmation forgets this once that location's sync is done.
    Syncing(LocationId),
}

impl Refusal {
    pub fn said(&self) -> &'static str {
        match self {
            Self::Changed => "It changed since this was shown. These are its numbers now.",
            Self::Syncing(_) => {
                "Where it, its copy or the same session from another location is kept is being \
                 synced. Try again once the sync is done."
            }
        }
    }
}

/// The most characters of a title a toast says: a longer one is cut in its middle.
const TOAST_TITLE: usize = 60;
/// The most a toast that says why something was refused says of a title: its reason, which
/// follows, is what matters.
const REFUSAL_TITLE: usize = 32;

/// A session's title as a toast says it.
fn toast_title(session: &Session) -> String {
    crate::components::middle_cut(&title_of(session), TOAST_TITLE)
}

/// What a session without a title is called.
pub const UNTITLED: &str = "Untitled session";

/// A session's title as it is drawn, one line with its controls shown
/// ([`crate::hostile::label`]: a title is a transcript's text), or [`UNTITLED`].
pub fn title_of(session: &Session) -> String {
    session
        .title
        .as_deref()
        .map_or_else(|| UNTITLED.to_string(), label)
}

/// What a new folder is called until it is named; the catalogue makes it "New folder 2" and on
/// where a folder there has that name.
pub const NEW_FOLDER: &str = "New folder";

/// How long a toast shows (DESIGN §4.14).
const TOAST_TIME: Duration = Duration::from_secs(5);

/// Where the owner was going when the paths editor had changes to save (DESIGN §4.7).
#[derive(Clone, Debug)]
pub enum Leave {
    Show(Shelf),
    Open(Box<Session>, Option<ReadAt>),
    /// A session found anywhere, opened from its own page ([`LibraryService::open_where`]).
    OpenWhere(Box<Session>, Option<ReadAt>),
    /// Another folder's paths editor: its page, with the editor open.
    Edit(FolderId),
    /// Closing the window.
    Close,
    Quit,
    /// Restart to update (`crate::services::updates`): the installer takes the update and the
    /// app quits.
    Restart,
}

/// The page a session is on: its folder's, Unsorted, or Trash where it is in Trash.
fn shelf_of(session: &Session) -> Shelf {
    match session.filed {
        _ if session.trashed.is_some() => Shelf::Trash,
        Filed::ByRule(folder) | Filed::ByHand(folder) => Shelf::Folder(folder),
        Filed::Unsorted => Shelf::Unsorted,
    }
}

/**
 * What an Undo that couldn't put everything back as it was says: "Undone, but 1 session was
 * deleted for good", "…; 2 sessions filed by their paths, as their folder is gone".
 */
fn undone_text(undone: Undone) -> String {
    let gone = (undone.gone > 0).then(|| match undone.gone {
        1 => "1 session was deleted for good".to_string(),
        n => format!("{} were deleted for good", format::plural(n, "session")),
    });
    let refiled = (undone.refiled > 0).then(|| match undone.refiled {
        1 => "1 session was filed by its path, as its folder is gone".to_string(),
        n => format!(
            "{} were filed by their paths, as their folder is gone",
            format::plural(n, "session")
        ),
    });
    format!(
        "Undone, but {}",
        gone.into_iter()
            .chain(refiled)
            .collect::<Vec<_>>()
            .join("; ")
    )
}

/// "Moved to Trash", "3 sessions moved to Trash": said of one, or of `n`.
fn of_sessions(n: usize, one: &str, several: &str) -> String {
    if n == 1 {
        one.to_string()
    } else {
        format!("{} {several}", format::plural(n, "session"))
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
    /// Whether a location is busy, for a confirmation refused while it was.
    sync: Entity<SyncService>,
    shelf: Shelf,
    counts: LibraryCounts,
    folders: Vec<Folder>,
    /// Every folder's attached paths.
    attached: Vec<FolderPath>,
    /// How many agents the archive holds sessions of (DESIGN §4.2.10).
    agents: usize,
    /// The page last loaded, and the shelf it is of.
    page: Option<(Shelf, Page)>,
    /// Where each of its sessions is: its group of Unsorted (0 on another page), and its place
    /// among the page's or the group's sessions.
    places: HashMap<SessionId, (usize, usize)>,
    /// All sessions as it was last loaded, while another page shows, and what its load saw.
    all: Option<(Page, Seen)>,
    /// What a load starting now would see.
    seen: Seen,
    /// What the load of the shown page saw.
    page_seen: Seen,
    /// The selected session, and where it is among a sessions page's.
    selected: Option<(SessionId, Option<usize>)>,
    /// Where a `Shift`-click's run starts ([`Self::anchor`]).
    anchor: Option<SessionId>,
    /// The session whose reader is open, and where it opened when it opened at a match.
    open: Option<Session>,
    /// The original a copy that is open was read from, which it stands for (DESIGN §4.2.8).
    copy_of: Option<Session>,
    read_at: Option<ReadAt>,
    /// How many requests to open at a match there were.
    read_requests: u64,
    loading: bool,
    /// A load was asked for while one ran or while changes were written.
    stale: bool,
    /// Why the last load failed.
    failure: Option<String>,
    /// The sessions selected with `Ctrl` or `Shift` or `Ctrl A`, the selected one among them;
    /// empty while only the selected one is.
    chosen: HashSet<SessionId>,
    /// How each page was set to show, while the app runs.
    views: HashMap<Shelf, PageView>,
    unsorted_view: UnsortedView,
    /// What is being dragged, and where.
    drag: Option<DragState>,
    /// The toast showing, and the one to show once the change being written is.
    toast: Option<Toast>,
    toast_in_hand: (u64, Option<Toast>),
    /// The numbers of the changes that are Undos: their toast says "Undone", with no Undo of
    /// its own.
    undos: HashSet<u64>,
    /// The changes queued before the last Undo, by their number ([`Seen::changes`]), show no
    /// toast when they are written.
    quiet_before: u64,
    toast_timer: Option<Task<()>>,
    /// The folder whose paths editor has changes to save, and the change saving them is:
    /// leaving its page asks first.
    hold: Option<(FolderId, Change)>,
    asked: Option<Leave>,
    /// The folder the owner asked to delete, until they confirm or cancel.
    deleting: Option<FolderId>,
    /// The session the owner asked to delete for good, until they confirm or cancel.
    for_good: Option<ForGood>,
    /// The owner's changes still to write, the next first, each with the toast to show once
    /// it is.
    queue: VecDeque<(Change, Option<Toast>, u64)>,
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
    _observers: [Subscription; 4],
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
                cx.subscribe(&sync, |this, _, event: &SyncEvent, cx| {
                    if *event != SyncEvent::Archived {
                        return;
                    }
                    this.seen.news += 1;
                    this.reload(cx);
                }),
                // A confirmation refused while a location synced forgets it once it doesn't.
                cx.observe(&sync, |this, _, cx| {
                    let waited = (this.for_good.as_ref()).and_then(|asked| match &asked.refused {
                        Some(Refusal::Syncing(location)) => Some(location.clone()),
                        _ => None,
                    });
                    if let Some(location) = waited
                        && !this.is_busy(&location, cx)
                        && let Some(asked) = this.for_good.as_mut()
                    {
                        asked.refused = None;
                        asked.deletion = None;
                        this.read_deletion(cx);
                        cx.notify();
                    }
                }),
                cx.on_app_quit(Self::write_what_is_queued),
            ],
            archive,
            sync,
            shelf: Shelf::All,
            counts: LibraryCounts::default(),
            folders: Vec::new(),
            attached: Vec::new(),
            agents: 0,
            page: None,
            places: HashMap::new(),
            all: None,
            seen: Seen::default(),
            page_seen: Seen::default(),
            selected: None,
            anchor: None,
            open: None,
            copy_of: None,
            read_at: None,
            read_requests: 0,
            loading: false,
            stale: false,
            failure: None,
            chosen: HashSet::new(),
            views: HashMap::new(),
            unsorted_view: UnsortedView::default(),
            drag: None,
            toast: None,
            toast_in_hand: (0, None),
            undos: HashSet::new(),
            quiet_before: 0,
            toast_timer: None,
            hold: None,
            asked: None,
            deleting: None,
            for_good: None,
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

    /// Every folder, those with the same parent together and in their order, each name a label
    /// ([`crate::hostile::label`]) wherever it is drawn.
    pub fn folders(&self) -> &[Folder] {
        &self.folders
    }

    pub fn folder(&self, id: FolderId) -> Option<&Folder> {
        self.folders.iter().find(|folder| folder.id == id)
    }

    /// Every folder's attached paths.
    pub fn attached(&self) -> &[FolderPath] {
        &self.attached
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

    /**
     * The session the open reader acts on and stands for on its page: the open one, or, while
     * a copy is read from its original ([`LibraryService::open_kin`]), that original. Star,
     * Trash and Move act on it, and Back and J / K go from its place.
     */
    pub fn reading_for(&self) -> Option<&Session> {
        self.copy_of.as_ref().or(self.open.as_ref())
    }

    /// The sidebar's selected row (DESIGN §4.1): the page shown, or the folder of the session
    /// being read.
    pub fn highlighted(&self) -> Shelf {
        match self.reading_for() {
            Some(session) if session.trashed.is_some() => Shelf::Trash,
            Some(Session {
                filed: Filed::ByRule(folder) | Filed::ByHand(folder),
                ..
            }) => Shelf::Folder(*folder),
            Some(_) | None => self.shelf,
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

    /**
     * Whether `location` is busy: a pass or a fetch again runs there, or a change to one of its
     * sessions deleted for good is being written. A delete for good there is refused then.
     */
    fn is_busy(&self, location: &LocationId, cx: &App) -> bool {
        self.sync.read(cx).is_busy(location)
            || super::ActiveServices::services(cx)
                .upkeep
                .read(cx)
                .is_writing_at(location)
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
        if self.holds(shelf) {
            self.asked = Some(Leave::Show(shelf));
            cx.notify();
            return;
        }
        self.open = None;
        self.read_at = None;
        if self.shelf != shelf {
            if let Some((Shelf::All, page)) = self.page.take_if(|(of, _)| *of == Shelf::All) {
                self.all = Some((page, self.page_seen));
            }
            self.shelf = shelf;
            self.selected = None;
            self.chosen.clear();
            self.anchor = None;
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
            self.chosen.clear();
            self.anchor = None;
            cx.notify();
        }
    }

    /// Selects the session `id`, which was at `hint` of the shown page's sessions when it was
    /// last drawn.
    pub fn select_session(&mut self, id: &SessionId, hint: usize, cx: &mut Context<Self>) {
        if let Some(at) = self.sessions().and_then(|page| page.find(id, hint)) {
            self.selected = Some((id.clone(), Some(at)));
            self.chosen.clear();
            self.anchor = None;
            cx.notify();
        }
    }

    /// Selects a session of Unsorted.
    pub fn select(&mut self, id: SessionId, cx: &mut Context<Self>) {
        self.selected = Some((id, None));
        self.chosen.clear();
        self.anchor = None;
        cx.notify();
    }

    /**
     * Selects the sessions `chosen` together (`Ctrl` and `Shift`-click, `Ctrl A`), with
     * `selected`, last drawn at its hint of the page's sessions, as the one the keys act on.
     * One alone is the selected one. `anchored`: `selected` is where the next `Shift`-click's
     * run starts, as it is after a plain or `Ctrl` click but not after a `Shift`-click.
     */
    pub fn choose(
        &mut self,
        chosen: HashSet<SessionId>,
        (selected, hint): (SessionId, Option<usize>),
        anchored: bool,
        cx: &mut Context<Self>,
    ) {
        let at = match (self.page(), hint) {
            (Some(Page::Sessions(page)), Some(hint)) => page.find(&selected, hint),
            _ => None,
        };
        self.chosen = if chosen.len() > 1 {
            chosen
        } else {
            HashSet::new()
        };
        if anchored {
            self.anchor = Some(selected.clone());
        }
        self.selected = Some((selected, at));
        self.prune();
        cx.notify();
    }

    /// Where a `Shift`-click's run starts: the session of the last plain or `Ctrl` click.
    pub fn anchor(&self) -> Option<&SessionId> {
        self.anchor.as_ref().or(self.selected())
    }

    /// The sessions selected together, or the selected one alone, of those the page shows.
    pub fn selection(&self) -> Vec<SessionId> {
        let visible = self.visible();
        if self.chosen.is_empty() {
            (self.selected().cloned().into_iter())
                .filter(|id| visible.contains(id))
                .collect()
        } else {
            (self.chosen.iter())
                .filter(|id| visible.contains(*id))
                .cloned()
                .collect()
        }
    }

    /// Whether the session `id` is selected, alone or with others.
    pub fn is_selected(&self, id: &SessionId) -> bool {
        if self.chosen.is_empty() {
            self.selected() == Some(id)
        } else {
            self.chosen.contains(id)
        }
    }

    /**
     * Takes `id` out of the selection (a `Ctrl`-click on a session in it). The keys then act on
     * where a run starts, while it is still selected, or on the first shown of those left; with
     * none left, nothing is selected.
     */
    pub fn unchoose(&mut self, id: &SessionId, cx: &mut Context<Self>) {
        let mut chosen: HashSet<SessionId> = self.selection().into_iter().collect();
        chosen.remove(id);
        let shown = self.shown();
        let next = (self.anchor.as_ref())
            .filter(|anchor| chosen.contains(*anchor))
            .and_then(|anchor| shown.iter().find(|(it, _)| it == anchor))
            .or_else(|| shown.iter().find(|(it, _)| chosen.contains(it)))
            .cloned();
        if self.anchor.as_ref() == Some(id) {
            self.anchor = next.as_ref().map(|(it, _)| it.clone());
        }
        self.chosen = if chosen.len() > 1 {
            chosen
        } else {
            HashSet::new()
        };
        self.selected = next;
        cx.notify();
    }

    /// The sessions an action on `id` is for: the selection when `id` is in it, else `id`.
    pub fn acted_on(&self, id: &SessionId) -> Vec<SessionId> {
        if self.chosen.contains(id) {
            self.selection()
        } else {
            vec![id.clone()]
        }
    }

    /**
     * The sessions the page shows, as its filters leave them: what the selection is kept to,
     * so that the keys never act on a session out of sight (DESIGN §4.4).
     */
    fn visible(&self) -> HashSet<SessionId> {
        self.shown().into_iter().map(|(id, _)| id).collect()
    }

    /// The sessions the page shows, in the order it shows them, each with where it is among a
    /// sessions page's.
    fn shown(&self) -> Vec<(SessionId, Option<usize>)> {
        match self.page() {
            Some(Page::Sessions(page)) => view::shown(&page.sessions, &self.view(), format::now())
                .into_iter()
                .filter_map(|at| Some((page.sessions.get(at)?.id.clone(), Some(at))))
                .collect(),
            Some(Page::Unsorted(groups)) => view::shown_groups(groups, &self.unsorted_view)
                .into_iter()
                .flat_map(|(at, places)| {
                    places
                        .into_iter()
                        .filter_map(move |place| groups.get(at)?.sessions.get(place))
                })
                .map(|session| (session.id.clone(), None))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Keeps the selection to what the page shows: after a load, and when its filters change.
    fn prune(&mut self) {
        let visible = self.visible();
        self.chosen.retain(|id| visible.contains(id));
        if self
            .selected
            .as_ref()
            .is_some_and(|(id, _)| !visible.contains(id))
        {
            self.selected = None;
        }
    }

    /// Opens a session's reader, at its start.
    pub fn open(&mut self, session: Session, cx: &mut Context<Self>) {
        self.open_at(session, None, cx);
    }

    /**
     * Opens a session's reader at `at`, a search match, or at its start. Each request is
     * numbered anew ([`ReadAt::request`]), so that opening the same match again moves the
     * reader again.
     */
    pub fn open_at(&mut self, session: Session, at: Option<ReadAt>, cx: &mut Context<Self>) {
        if self.hold.is_some() {
            self.asked = Some(Leave::Open(Box::new(session), at));
        } else {
            self.copy_of = None;
            self.open = Some(session);
            self.read_at = at.map(|at| {
                self.read_requests += 1;
                ReadAt {
                    request: self.read_requests,
                    ..at
                }
            });
        }
        cx.notify();
    }

    /**
     * Opens the session `id`, another of the open session's conversation (PLAN §7.7): a copy
     * the reader's facts offer to read, or the original a copy names. It is read off the UI
     * thread, and opened unless another was opened meanwhile.
     */
    pub fn open_kin(&mut self, id: SessionId, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        let from = self.open.as_ref().map(|open| open.id.clone());
        let job = cx.background_spawn(async move { archive.catalogue().session(&id) });
        cx.spawn(async move |this, cx| {
            let read = job.await;
            this.update(cx, |this, cx| match read {
                Ok(Some(session)) if this.open.as_ref().map(|open| &open.id) == from.as_ref() => {
                    /* A copy read from its original stands for it; the original read from its
                    copy is itself again. */
                    let original = this.copy_of.take().or_else(|| {
                        this.open
                            .clone()
                            .filter(|open| open.copy.is_none() && session.copy.is_some())
                    });
                    this.open(session, cx);
                    this.copy_of = original.filter(|original| {
                        this.open
                            .as_ref()
                            .is_some_and(|open| open.id != original.id)
                    });
                }
                Ok(_) => {}
                Err(error) => log::warn!("a copy of the session could not be read: {error}"),
            })
            .ok();
        })
        .detach();
    }

    /// The request to open the open session at a match, until the reader takes it (the docs
    /// of [`ReadAt`]): what automation and the tests look at.
    pub fn read_at(&self) -> Option<&ReadAt> {
        self.read_at.as_ref()
    }

    /// Takes the request to open the open session at a match: once, by the reader.
    pub fn take_read_at(&mut self) -> Option<ReadAt> {
        self.read_at.take()
    }

    /**
     * Shows the page of the folder `session` is in, or Unsorted, or Trash where it is in
     * Trash, with the session selected there once the page is read (the palette's
     * `Ctrl Enter`, DESIGN §4.9).
     */
    pub fn show_where(&mut self, session: &Session, cx: &mut Context<Self>) {
        let shelf = shelf_of(session);
        self.show(shelf, cx);
        if self.shelf == shelf && self.open.is_none() {
            let at = self.sessions().and_then(|page| page.find(&session.id, 0));
            self.selected = Some((session.id.clone(), at));
        }
    }

    /**
     * Opens a session found anywhere (the palette, DESIGN §4.9) at `at`, as though from the
     * page it is on: its folder's, Unsorted, or Trash where it is in Trash. That page is then
     * the one the reader names in its breadcrumb, goes Back to and steps through with J and K,
     * and the session is selected on it.
     */
    pub fn open_where(&mut self, session: Session, at: Option<ReadAt>, cx: &mut Context<Self>) {
        if self.hold.is_some() {
            self.asked = Some(Leave::OpenWhere(Box::new(session), at));
            cx.notify();
            return;
        }
        self.show(shelf_of(&session), cx);
        let place = self.sessions().and_then(|page| page.find(&session.id, 0));
        self.selected = Some((session.id.clone(), place));
        self.chosen.clear();
        self.anchor = None;
        self.open_at(session, at, cx);
    }

    /// Selects and opens the session `id` of the shown page's sessions, last drawn at `hint`.
    pub fn open_session_at(&mut self, id: &SessionId, hint: usize, cx: &mut Context<Self>) {
        let Some(page) = self.sessions() else {
            return;
        };
        if let Some((at, session)) = page
            .find(id, hint)
            .and_then(|at| Some((at, page.sessions.get(at)?.clone())))
        {
            self.selected = Some((id.clone(), Some(at)));
            self.chosen.clear();
            self.anchor = None;
            self.open(session, cx);
        }
    }

    /// Where the session `id` is among the shown page's sessions, on a page of sessions.
    pub fn index_of(&self, id: &SessionId) -> Option<usize> {
        self.sessions()?;
        self.session(id)?;
        self.places.get(id).map(|(_, at)| *at)
    }

    /// The session `id` of the shown page, wherever it is on it.
    pub fn session(&self, id: &SessionId) -> Option<&Session> {
        let (group, at) = self.places.get(id)?;
        match self.page()? {
            Page::Sessions(page) => page.sessions.get(*at),
            Page::Unsorted(groups) => groups.get(*group)?.sessions.get(*at),
        }
        .filter(|session| session.id == *id)
    }

    /// How a page shows: as the owner last set it, or as every page starts.
    pub fn view(&self) -> PageView {
        self.views.get(&self.shelf).cloned().unwrap_or_default()
    }

    pub fn set_view(&mut self, view: PageView, cx: &mut Context<Self>) {
        if self.view() != view {
            self.views.insert(self.shelf, view);
            self.prune();
            cx.notify();
        }
    }

    /// Where the open session is among the sessions the page it was opened from shows, from 0,
    /// and how many that page shows (DESIGN §4.2.1: "4 of 488"), as filtered and sorted.
    pub fn open_place(&self) -> Option<(usize, usize)> {
        let id = &self.reading_for()?.id;
        let shown = self.shown();
        let at = shown.iter().position(|(shown, _)| shown == id)?;
        Some((at, shown.len()))
    }

    /// Opens the session `by` places after the open one (before, for a negative `by`) among
    /// those the page it was opened from shows, and selects it there. Nothing happens past
    /// either end.
    pub fn open_step(&mut self, by: isize, cx: &mut Context<Self>) {
        let Some((at, _)) = self.open_place() else {
            return;
        };
        let Some((session, place)) = at
            .checked_add_signed(by)
            .and_then(|next| self.shown().get(next).cloned())
            .and_then(|(id, place)| Some((self.session(&id)?.clone(), place)))
        else {
            return;
        };
        self.selected = Some((session.id.clone(), place));
        self.chosen.clear();
        self.open(session, cx);
    }

    /// Stars the open session, or takes its star away: the original, while a copy of it is
    /// read ([`LibraryService::reading_for`]).
    pub fn toggle_open_star(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.copy_of.as_mut().or(self.open.as_mut()) else {
            return;
        };
        open.starred = !open.starred;
        let (id, starred) = (open.id.clone(), open.starred);
        if let Some((_, Page::Sessions(page))) = &mut self.page
            && let Some(listed) = page.sessions.iter_mut().find(|session| session.id == id)
        {
            listed.starred = starred;
        }
        if starred {
            self.counts.starred += 1;
        } else {
            self.counts.starred = self.counts.starred.saturating_sub(1);
        }
        self.write(
            Change::Star {
                ids: vec![id],
                starred,
            },
            None,
            cx,
        );
        cx.notify();
    }

    pub fn unsorted_view(&self) -> &UnsortedView {
        &self.unsorted_view
    }

    pub fn set_unsorted_view(&mut self, view: UnsortedView, cx: &mut Context<Self>) {
        if self.unsorted_view != view {
            self.unsorted_view = view;
            self.prune();
            cx.notify();
        }
    }

    /// What is being dragged, while something is.
    pub fn drag(&self) -> Option<&DragState> {
        self.drag.as_ref()
    }

    pub fn set_drag(&mut self, drag: Option<DragState>, cx: &mut Context<Self>) {
        if self.drag != drag {
            self.drag = drag;
            cx.notify();
        }
    }

    /**
     * Whether a folder in `parent`, or at the top, other than `except` has the name `name`,
     * whatever its case: two folders in one place don't (the catalogue refuses it).
     */
    pub fn name_taken(
        &self,
        name: &str,
        parent: Option<FolderId>,
        except: Option<FolderId>,
    ) -> bool {
        let name = name.trim().to_lowercase();
        (self.folders.iter()).any(|folder| {
            folder.parent == parent
                && Some(folder.id) != except
                && folder.name.to_lowercase() == name
        })
    }

    /// A folder's name with the folders it is in: "fc › player".
    pub fn folder_path(&self, folder: FolderId) -> String {
        let mut names: Vec<&str> = std::iter::successors(self.folder(folder), |it| {
            it.parent.and_then(|parent| self.folder(parent))
        })
        .map(|it| it.name.as_str())
        .collect();
        names.reverse();
        names.join(" › ")
    }

    /// The toast showing.
    pub fn toast(&self) -> Option<&Toast> {
        self.toast.as_ref()
    }

    /// Undoes what the toast showing says was done.
    pub fn undo(&mut self, cx: &mut Context<Self>) {
        if let Some(undo) = self.toast.take().and_then(|toast| toast.undo)
            && self.undoable(&undo)
        {
            self.write(undo, None, cx);
            self.undos.insert(self.seen.changes);
            /* A change queued before it may still be being written: its toast would undo into
            a state that never was. */
            self.quiet_before = self.seen.changes;
        }
        cx.notify();
    }

    pub fn dismiss_toast(&mut self, cx: &mut Context<Self>) {
        if self.toast.take().is_some() {
            cx.notify();
        }
    }

    fn show_toast(&mut self, toast: Toast, cx: &mut Context<Self>) {
        self.toast = Some(toast);
        self.pause_toast(false, cx);
        cx.notify();
    }

    /// Stops the toast's time running out while the pointer rests on it, and starts it again
    /// once the pointer leaves.
    pub fn pause_toast(&mut self, paused: bool, cx: &mut Context<Self>) {
        self.toast_timer = (!paused).then(|| {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(TOAST_TIME).await;
                this.update(cx, |this, cx| this.dismiss_toast(cx)).ok();
            })
        });
    }

    /// Whether leaving for `shelf` waits on the owner: the paths editor has changes to save.
    fn holds(&self, shelf: Shelf) -> bool {
        self.held()
            .is_some_and(|folder| Shelf::Folder(folder) != shelf)
    }

    /**
     * Marks the folder whose paths editor has changes to save, with what they would take off
     * and put on, or none. A quit that asks nothing saves them (the module docs).
     */
    pub fn hold(&mut self, held: Option<(FolderId, Vec<TypedPath>, Vec<TypedPath>)>) {
        self.hold = held.map(|(folder, detach, attach)| {
            (
                folder,
                Change::ChangePaths {
                    folder,
                    detach,
                    attach,
                },
            )
        });
    }

    /// The folder whose paths editor has changes to save.
    pub fn held(&self) -> Option<FolderId> {
        self.hold.as_ref().map(|(folder, _)| *folder)
    }

    /// Asks before closing the window, quitting or restarting to update, while the paths editor
    /// has changes to save.
    pub fn ask_before(&mut self, leave: Leave, cx: &mut Context<Self>) {
        self.asked = Some(leave);
        cx.notify();
    }

    /// Where the owner was going when leaving waited on them.
    pub fn asked(&self) -> Option<&Leave> {
        self.asked.as_ref()
    }

    /// Goes where the owner was going, or stays (`go` false). The paths editor has saved or
    /// dropped its changes by now, or keeps them.
    pub fn answer(&mut self, go: bool, cx: &mut Context<Self>) {
        let Some(asked) = self.asked.take() else {
            return;
        };
        if go {
            self.hold = None;
            match asked {
                Leave::Show(shelf) => self.show(shelf, cx),
                Leave::Open(session, at) => self.open_at(*session, at, cx),
                Leave::OpenWhere(session, at) => self.open_where(*session, at, cx),
                Leave::Edit(folder) => self.edit_paths(folder, cx),
                leave @ (Leave::Close | Leave::Quit | Leave::Restart) => {
                    cx.emit(LibraryEvent::Leave(leave));
                }
            }
        }
        cx.notify();
    }

    /// The folder the owner asked to delete, until they answer.
    pub fn deleting(&self) -> Option<FolderId> {
        self.deleting
    }

    pub fn ask_delete(&mut self, folder: FolderId, cx: &mut Context<Self>) {
        self.deleting = Some(folder);
        cx.notify();
    }

    /// Deletes the folder asked about, or leaves it.
    pub fn answer_delete(&mut self, delete: bool, cx: &mut Context<Self>) {
        let Some(folder) = self.deleting.take() else {
            return;
        };
        if delete {
            if self.shelf == Shelf::Folder(folder) {
                self.hold = None;
                self.show(Shelf::All, cx);
            }
            self.write(Change::DeleteFolder { folder }, None, cx);
        }
        cx.notify();
    }

    /// Asks the sidebar to rename `folder` in its row.
    pub fn ask_rename(&mut self, folder: FolderId, cx: &mut Context<Self>) {
        cx.emit(LibraryEvent::Rename(folder));
    }

    /// Shows `folder`'s page with its paths editor open (DESIGN §4.7); while another
    /// folder's editor has changes to save, asks first.
    pub fn edit_paths(&mut self, folder: FolderId, cx: &mut Context<Self>) {
        if self.holds(Shelf::Folder(folder)) {
            self.asked = Some(Leave::Edit(folder));
            cx.notify();
            return;
        }
        self.show(Shelf::Folder(folder), cx);
        cx.emit(LibraryEvent::EditPaths(folder));
    }

    /// Files `ids` in `folder` by hand (DESIGN §4.4), with a toast that undoes it.
    pub fn file_by_hand(&mut self, ids: Vec<SessionId>, folder: FolderId, cx: &mut Context<Self>) {
        self.write(Change::FileByHand { ids, folder }, None, cx);
    }

    /// Hands `ids` back to the rules (decision 2); the toast says where they went once they
    /// are there.
    pub fn file_automatically(&mut self, ids: Vec<SessionId>, cx: &mut Context<Self>) {
        self.write(Change::FileAutomatically { ids }, None, cx);
    }

    /**
     * Moves `ids` to Trash (PLAN §4.5), with a toast that restores them. They leave the
     * selection at once; when the selected one goes, the one shown after it, or else before
     * it, is selected, so that the keys go on from where it was. On Trash's page nothing
     * happens.
     */
    pub fn trash(&mut self, ids: Vec<SessionId>, cx: &mut Context<Self>) {
        // Trash's own page holds sessions there already: nothing moves, nor does the selection.
        if self.shelf == Shelf::Trash {
            return;
        }
        let trashed: HashSet<&SessionId> = ids.iter().collect();
        self.chosen.retain(|id| !trashed.contains(id));
        if let Some(selected) = self.selected().filter(|id| trashed.contains(id)) {
            let shown = self.shown();
            let at = shown.iter().position(|(id, _)| id == selected).unwrap_or(0);
            let (before, after) = shown.split_at(at);
            self.selected = (after.iter())
                .chain(before.iter().rev())
                .find(|(id, _)| !trashed.contains(id))
                .cloned();
        }
        // One left of several selected together is the selected one alone.
        if self.chosen.len() == 1 {
            let left = self.chosen.drain().next();
            self.selected = (self.shown().into_iter()).find(|(id, _)| Some(id) == left.as_ref());
        }
        self.write(Change::Trash { ids }, None, cx);
        cx.notify();
    }

    /**
     * Takes `ids` out of Trash, each back where it was filed; one whose folder was deleted
     * meanwhile goes where the rules file it (decision 10). They leave the selection at once,
     * and the toast says where they went and undoes it.
     */
    pub fn restore(&mut self, ids: Vec<SessionId>, cx: &mut Context<Self>) {
        let restored: HashSet<&SessionId> = ids.iter().collect();
        self.chosen.retain(|id| !restored.contains(id));
        if self.selected().is_some_and(|id| restored.contains(id)) {
            self.selected = None;
        }
        self.write(Change::RestoreFromTrash { ids }, None, cx);
        cx.notify();
    }

    /// The session the owner asked to delete for good, until they answer.
    pub fn for_good(&self) -> Option<&ForGood> {
        self.for_good.as_ref()
    }

    /**
     * Asks before deleting `session`, in Trash, for good (DESIGN §4.8): what it would remove
     * is read off the UI thread, and the confirmation shows it once it is.
     */
    pub fn ask_delete_for_good(&mut self, session: Session, cx: &mut Context<Self>) {
        self.for_good = Some(ForGood {
            session,
            deletion: None,
            refused: None,
        });
        self.read_deletion(cx);
        cx.notify();
    }

    /// Reads what deleting the session asked about for good would remove.
    fn read_deletion(&mut self, cx: &mut Context<Self>) {
        let (Some(archive), Some(asked)) = (
            self.archive.read(cx).archive().cloned(),
            self.for_good.as_ref(),
        ) else {
            return;
        };
        let id = asked.session.id.clone();
        let job = cx.background_spawn({
            let id = id.clone();
            async move { archive.catalogue().deletion(&id) }
        });
        cx.spawn(async move |this, cx| {
            let read = job.await;
            this.update(cx, |this, cx| {
                let Some(asked) = this.for_good.as_mut().filter(|it| it.session.id == id) else {
                    return;
                };
                match read {
                    Ok(deletion) => asked.deletion = Some(deletion),
                    Err(error) => {
                        log::warn!(
                            "what deleting a session would remove couldn't be read: {error}"
                        );
                        let session = asked.session.clone();
                        this.for_good = None;
                        let said = this.not_deleted(&session, &error.into(), cx);
                        this.show_toast(
                            Toast {
                                text: said,
                                undo: None,
                            },
                            cx,
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /**
     * Takes in a delete for good of `session` refused with `error`, as its write would: what
     * a test can't make the archive do at the moment it wants, such as a location syncing.
     */
    #[cfg(test)]
    pub(crate) fn refused_for_good(
        &mut self,
        deletion: Deletion,
        session: Session,
        error: ArchiveError,
        cx: &mut Context<Self>,
    ) {
        let change = Change::DeleteForGood {
            deletion,
            session: Box::new(session),
        };
        self.written(change, Err(error), cx);
    }

    /**
     * Deletes the session asked about for good (`delete`), or leaves it in Trash. Deleting
     * waits for what it removes to be read: until then the confirmation stays as it is.
     */
    pub fn answer_delete_for_good(&mut self, delete: bool, cx: &mut Context<Self>) {
        if delete
            && self
                .for_good
                .as_ref()
                .is_some_and(|it| it.deletion.is_none())
        {
            return;
        }
        let Some(asked) = self.for_good.take() else {
            return;
        };
        if let (true, Some(deletion)) = (delete, asked.deletion) {
            /* Every session it deletes: the one asked and the others of its conversation. */
            for id in deletion.sessions() {
                self.chosen.remove(id);
            }
            if self
                .selected()
                .is_some_and(|selected| deletion.sessions().any(|id| id == selected))
            {
                self.selected = None;
            }
            let session = Box::new(asked.session);
            self.write(Change::DeleteForGood { deletion, session }, None, cx);
        }
        cx.notify();
    }

    /// Stars `ids`, or takes their stars away, at once on the page.
    pub fn star(&mut self, ids: Vec<SessionId>, starred: bool, cx: &mut Context<Self>) {
        if let Some((_, Page::Sessions(page))) = &mut self.page {
            let starring: HashSet<&SessionId> = ids.iter().collect();
            for session in &mut page.sessions {
                if session.starred != starred && starring.contains(&session.id) {
                    session.starred = starred;
                    if starred {
                        self.counts.starred += 1;
                    } else {
                        self.counts.starred = self.counts.starred.saturating_sub(1);
                    }
                }
            }
        }
        self.write(Change::Star { ids, starred }, None, cx);
        cx.notify();
    }

    /// Stars the session `id`, or takes its star away.
    pub fn toggle_star(&mut self, id: &SessionId, cx: &mut Context<Self>) {
        if let Some(starred) = self.session(id).map(|session| !session.starred) {
            self.star(vec![id.clone()], starred, cx);
        }
    }

    /**
     * Attaches a path to `folder` for `location`, or for any, from Unsorted (DESIGN §4.6),
     * with a toast that says how many sessions it files and undoes it.
     */
    pub fn attach(
        &mut self,
        folder: FolderId,
        path: TypedPath,
        sessions: usize,
        cx: &mut Context<Self>,
    ) {
        let name = self.folder_path(folder);
        let toast = Toast {
            text: format!(
                "Attached to {name} · {} filed",
                format::plural(sessions, "session")
            ),
            undo: Some(Change::ChangePaths {
                folder,
                detach: vec![path.clone()],
                attach: Vec::new(),
            }),
        };
        let change = Change::ChangePaths {
            folder,
            detach: Vec::new(),
            attach: vec![path],
        };
        self.write(change, Some(toast), cx);
    }

    /// Moves a path from one folder to another (DESIGN §4.4, a chip dropped on a folder).
    pub fn move_path(
        &mut self,
        from: FolderId,
        path: TypedPath,
        to: FolderId,
        cx: &mut Context<Self>,
    ) {
        let toast = Toast {
            text: format!("Moved {} to {}", label(&path.0), self.folder_path(to)),
            undo: Some(Change::MovePath {
                from: to,
                path: path.clone(),
                to: from,
            }),
        };
        self.write(Change::MovePath { from, path, to }, Some(toast), cx);
    }

    /// Saves the paths editor's changes to `folder` in one change, with a toast saying what
    /// they did and that undoes them.
    pub fn change_paths(
        &mut self,
        folder: FolderId,
        detach: Vec<TypedPath>,
        attach: Vec<TypedPath>,
        said: String,
        cx: &mut Context<Self>,
    ) {
        let toast = Toast {
            text: said,
            undo: Some(Change::ChangePaths {
                folder,
                detach: attach.clone(),
                attach: detach.clone(),
            }),
        };
        self.write(
            Change::ChangePaths {
                folder,
                detach,
                attach,
            },
            Some(toast),
            cx,
        );
    }

    /// Nests `folder` in `parent`, or moves it to the top, as the one at `position` there.
    pub fn move_folder(
        &mut self,
        folder: FolderId,
        parent: Option<FolderId>,
        position: usize,
        cx: &mut Context<Self>,
    ) {
        self.write(
            Change::MoveFolder {
                folder,
                parent,
                position,
            },
            None,
            cx,
        );
    }

    /// Makes a top-level folder and files `ids` in it by hand; the sidebar names it in place.
    pub fn new_folder_with(&mut self, ids: Vec<SessionId>, cx: &mut Context<Self>) {
        self.write(
            Change::NewFolder {
                name: NEW_FOLDER.to_string(),
                parent: None,
                sessions: ids,
            },
            None,
            cx,
        );
    }

    /// Goes back from the reader to the page it was opened from; from a copy read from its
    /// original, back to the original (DESIGN §4.2.8).
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.read_at = None;
        if let Some(original) = self.copy_of.take() {
            self.open(original, cx);
        } else if self.open.take().is_some() {
            cx.notify();
        }
    }

    /// Makes a folder in `parent`, or at the top. The sidebar hears when it is there.
    pub fn create_folder(
        &mut self,
        name: String,
        parent: Option<FolderId>,
        cx: &mut Context<Self>,
    ) {
        self.write(
            Change::NewFolder {
                name,
                parent,
                sessions: Vec::new(),
            },
            None,
            cx,
        );
    }

    pub fn rename_folder(&mut self, folder: FolderId, name: String, cx: &mut Context<Self>) {
        self.write(Change::Rename { folder, name }, None, cx);
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
            None,
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
        self.attached = loaded.attached;
        self.agents = loaded.agents;
        if let Some((id, at)) = &mut self.selected
            && let Page::Sessions(page) = &loaded.page
        {
            // The selected session may have moved: sessions arrive while a sync runs.
            *at = page.find(id, at.unwrap_or(0));
        }
        self.set_page(loaded.shelf, loaded.page, seen, cx);
        self.prune();
        if (self.toast.as_ref())
            .and_then(|toast| toast.undo.as_ref())
            .is_some_and(|undo| !self.undoable(undo))
        {
            self.toast = None;
        }
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
        self.places = match &page {
            Page::Sessions(page) => (page.sessions.iter().enumerate())
                .map(|(at, session)| (session.id.clone(), (0, at)))
                .collect(),
            Page::Unsorted(groups) => (groups.iter().enumerate())
                .flat_map(|(group, it)| {
                    (it.sessions.iter().enumerate())
                        .map(move |(at, session)| (session.id.clone(), (group, at)))
                })
                .collect(),
        };
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
    fn write(&mut self, change: Change, toast: Option<Toast>, cx: &mut Context<Self>) {
        self.seen.changes += 1;
        self.queue.push_back((change, toast, self.seen.changes));
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
        let Some((change, toast, number)) = self.queue.pop_front() else {
            self.reload(cx);
            return;
        };
        self.writing = Some(change.what());
        self.toast_in_hand = (number, toast);
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
                this.written(change, written, cx);
                this.write_next(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Takes in how a change's write went. A refusal is said to the owner: under the group of
    /// Unsorted a folder was to be made from, or in [`Self::write_failure`].
    fn written(
        &mut self,
        change: Change,
        written: Result<Written, ArchiveError>,
        cx: &mut Context<Self>,
    ) {
        let (number, toast) = std::mem::take(&mut self.toast_in_hand);
        /* Taken whether or not it was written: a refused Undo leaves no number behind. */
        let undo = self.undos.remove(&number);
        let written = match (&change, written) {
            (_, Ok(written)) => written,
            (Change::FolderFrom { group, .. }, Err(error)) => {
                self.refused = Some((group.clone(), label(&error.to_string())));
                return;
            }
            /* Nothing was deleted (PLAN §4.5). The confirmation shows again, with why and the
            numbers as they are now; or, while another session's is open, a toast says it. */
            (
                Change::DeleteForGood { session, .. },
                Err(
                    error @ (ArchiveError::Syncing(_)
                    | ArchiveError::Catalogue(CatalogueError::DeletionChanged(_))),
                ),
            ) if self
                .for_good
                .as_ref()
                .is_none_or(|open| open.session.id == session.id) =>
            {
                /* Refused as a location was busy (its own, or one where a copy of it is kept), the
                confirmation says so only while it still is; otherwise it shows its numbers read
                again, Cancel taking the keyboard back, as for a session that changed. */
                let refused = match error {
                    ArchiveError::Syncing(location) if self.is_busy(&location, cx) => {
                        Refusal::Syncing(location.clone())
                    }
                    _ => Refusal::Changed,
                };
                self.for_good = Some(ForGood {
                    session: (**session).clone(),
                    deletion: None,
                    refused: Some(refused),
                });
                self.read_deletion(cx);
                return;
            }
            (Change::DeleteForGood { session, .. }, Err(error)) => {
                log::warn!("{} was refused: {error}", change.what());
                let said = self.not_deleted(session, &error, cx);
                self.show_toast(
                    Toast {
                        text: said,
                        undo: None,
                    },
                    cx,
                );
                return;
            }
            (change, Err(error)) => {
                log::warn!("{} was refused: {error}", change.what());
                self.write_failure = Some(format!(
                    "{} couldn't be saved: {}.",
                    change.what(),
                    self.why(&error)
                ));
                return;
            }
        };
        let toast = match written {
            Written::Done => toast,
            Written::Made(folder) => {
                self.created = Some(folder);
                toast
            }
            Written::Deleted(renamed) => renamed.first().map(|(from, to)| Toast {
                text: match renamed.len() {
                    1 => format!("Folder deleted; its subfolder {from} is {to} now"),
                    n => format!(
                        "Folder deleted; {n} of its subfolders were renamed beside folders \
                         of their names"
                    ),
                },
                undo: None,
            }),
            Written::Filed(folder, before) => {
                let place = self.folder_path(folder);
                Some(Toast {
                    text: of_sessions(
                        before.len(),
                        &format!("Filed in {place} by hand"),
                        &format!("filed in {place} by hand"),
                    ),
                    undo: Some(Change::Refile { before }),
                })
            }
            Written::Handed(handed) => Some(Toast {
                text: self.handed(&handed),
                undo: Some(Change::Refile {
                    before: handed.into_iter().map(|(before, _)| before).collect(),
                }),
            }),
            Written::Trashed(moved) if moved.is_empty() => None,
            Written::Trashed(moved) => Some(Toast {
                text: of_sessions(moved.len(), "Moved to Trash", "moved to Trash"),
                undo: Some(Change::Restore { ids: moved }),
            }),
            Written::Undone(undone) if undone.is_whole() => toast,
            Written::Undone(undone) => Some(Toast {
                text: undone_text(undone),
                undo: None,
            }),
            Written::Restored(restored) => Some(Toast {
                text: self.restored(&restored),
                undo: (!restored.moved.is_empty()).then(|| Change::Trash {
                    ids: restored.moved.into_iter().map(|(id, ..)| id).collect(),
                }),
            }),
            Written::DeletedForGood(deleted) => Some(Toast {
                text: match (&change, deleted.pending) {
                    (Change::DeleteForGood { session, .. }, 0) => {
                        format!("Deleted “{}” for good.", toast_title(session))
                    }
                    (Change::DeleteForGood { session, .. }, _) => format!(
                        "Deleted “{}” for good. Its files are being removed in the background.",
                        toast_title(session)
                    ),
                    _ => "Deleted for good.".to_string(),
                },
                undo: None,
            }),
        };
        if let Change::DeleteForGood { deletion, .. } = &change {
            cx.emit(LibraryEvent::DeletedForGood(deletion.session.clone()));
        }
        // An Undo says it is done, or what it couldn't put back; it has no Undo of its own.
        let toast = match toast {
            _ if !undo => toast,
            Some(Toast { undo: None, text }) => Some(Toast { text, undo: None }),
            _ => Some(Toast {
                text: "Undone".to_string(),
                undo: None,
            }),
        };
        // Toasts of changes queued before an Undo say nothing: the Undo has the last word.
        if let Some(toast) = toast.filter(|_| number >= self.quiet_before) {
            self.show_toast(toast, cx);
        }
    }

    /// Where a filing toast says sessions went: "Filed in fc by its path", or how many went to
    /// a folder and how many stayed in Unsorted.
    fn handed(&self, handed: &[(Before, Option<FolderId>)]) -> String {
        let places: HashSet<Option<FolderId>> = handed.iter().map(|(_, now)| *now).collect();
        let unsorted = handed.iter().filter(|(_, now)| now.is_none()).count();
        match places.into_iter().collect::<Vec<_>>()[..] {
            [Some(folder)] => of_sessions(
                handed.len(),
                &format!("Filed in {} by its path", self.folder_path(folder)),
                &format!("filed in {} by their paths", self.folder_path(folder)),
            ),
            [None] => of_sessions(
                handed.len(),
                "In Unsorted: no folder's path claims it",
                "in Unsorted: no folder's path claims them",
            ),
            _ if unsorted == 0 => format!(
                "{} filed by their paths",
                format::plural(handed.len(), "session")
            ),
            _ => format!(
                "{} filed by {}; {} in Unsorted",
                format::plural(handed.len() - unsorted, "session"),
                if handed.len() - unsorted == 1 {
                    "its path"
                } else {
                    "their paths"
                },
                unsorted
            ),
        }
    }

    /**
     * Where a restore put sessions (decision 10): "Restored to fc", "Restored; its folder is
     * gone, so its path filed it in work", "3 sessions restored", with those deleted for good
     * meanwhile counted.
     */
    fn restored(&self, restored: &Restored) -> String {
        let place = |folder: Option<FolderId>| match folder {
            Some(folder) => self.folder_path(folder),
            None => "Unsorted".to_string(),
        };
        let gone = restored.gone;
        let said = match &restored.moved[..] {
            [] => String::new(),
            [(_, Filed::ByRule(was) | Filed::ByHand(was), now)] if self.folder(*was).is_none() => {
                match now {
                    Some(now) => format!(
                        "Restored: its folder is gone, so its path filed it in {}",
                        place(Some(*now))
                    ),
                    None => {
                        "Restored to Unsorted: its folder is gone, and no folder's path claims it"
                            .to_string()
                    }
                }
            }
            [(_, _, now)] => format!("Restored to {}", place(*now)),
            several => format!("{} restored", format::plural(several.len(), "session")),
        };
        match (said.is_empty(), gone) {
            (_, 0) => said,
            (true, 1) => "The session was deleted for good meanwhile".to_string(),
            (true, n) => format!("{n} sessions were deleted for good meanwhile"),
            (false, n) => format!(
                "{said}; {} deleted for good meanwhile",
                format::plural(n, "session")
            ),
        }
    }

    /**
     * Why deleting `session` for good was refused, as the owner knows it. Its title is cut to
     * [`REFUSAL_TITLE`] characters, so that why still shows in a narrow window's toast.
     */
    fn not_deleted(&self, session: &Session, error: &ArchiveError, cx: &App) -> String {
        let title = crate::components::middle_cut(&title_of(session), REFUSAL_TITLE);
        match error {
            ArchiveError::Catalogue(CatalogueError::NotInTrash(_)) => {
                format!("“{title}” was restored, so it wasn't deleted")
            }
            ArchiveError::Catalogue(CatalogueError::NoSession(_)) => {
                format!("“{title}” is deleted already")
            }
            ArchiveError::Syncing(_) => format!(
                "“{title}” wasn't deleted: its location is being synced. Try again once the \
                 sync is done"
            ),
            ArchiveError::Catalogue(CatalogueError::DeletionChanged(_)) => format!(
                "“{title}” wasn't deleted: it changed since it was shown. Ask again to see what \
                 it holds now"
            ),
            ArchiveError::Catalogue(CatalogueError::StillIndexing(_)) => format!(
                "“{title}” wasn't deleted: this project is still being indexed. Try again once \
                 the index is done"
            ),
            ArchiveError::Catalogue(CatalogueError::AlsoShown(also)) => format!(
                "“{title}” wasn't deleted: the same session from {} is still in your library. \
                 Move it to Trash too, then delete for good",
                (self.sync.read(cx).location(&also.location))
                    .map_or_else(|| "another location".to_string(), |it| label(&it.name))
            ),
            ArchiveError::Catalogue(CatalogueError::SeveralTranscripts(_)) => format!(
                "“{title}” wasn't deleted: it, or a copy of it, holds more than one transcript, \
                 and only the first can be read"
            ),
            error => format!(
                "“{title}” couldn't be deleted for good: {}",
                self.why(error)
            ),
        }
    }

    /// Why a change was refused, said as the owner knows things: no folder ids.
    fn why(&self, error: &ArchiveError) -> String {
        let why = match error {
            ArchiveError::Catalogue(CatalogueError::NoFolder(_)) => {
                "the folder was deleted".to_string()
            }
            ArchiveError::Catalogue(CatalogueError::NoSession(_)) => {
                "a session was deleted for good".to_string()
            }
            ArchiveError::Catalogue(CatalogueError::NotAttached(typed)) => {
                format!("{typed} isn't attached to that folder any more")
            }
            ArchiveError::Catalogue(CatalogueError::PathAttached { folder, typed, .. }) => format!(
                "{typed} is already attached to {}",
                self.folder_path(*folder)
            ),
            error => error.to_string().trim_end_matches('.').to_string(),
        };
        /* It may name a path, or say what a file it couldn't read held. */
        label(&why)
    }

    /// Whether an Undo can still be done: the folders it names are still there.
    fn undoable(&self, undo: &Change) -> bool {
        match undo {
            Change::ChangePaths { folder, .. } => self.folder(*folder).is_some(),
            Change::MovePath { from, to, .. } => {
                self.folder(*from).is_some() && self.folder(*to).is_some()
            }
            _ => true,
        }
    }

    /// At quit: writes what is still queued, in its order, after the change in hand, for as
    /// long as gpui waits (the module docs).
    fn write_what_is_queued(&mut self, cx: &mut Context<Self>) -> impl Future<Output = ()> + use<> {
        let archive = self.archive.read(cx).archive().cloned();
        /* Changes the paths editor holds are deliberate edits: a quit that asked nothing, from
        the Dock or at logout, saves them rather than dropping them. */
        if let Some((folder, change)) = self.hold.take() {
            log::warn!(
                "the app quit with the paths editor's changes to {} unsaved: saving them",
                self.folder_path(folder)
            );
            self.queue.push_back((change, None, 0));
        }
        let changes: Vec<Change> = self.queue.drain(..).map(|(change, ..)| change).collect();
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
