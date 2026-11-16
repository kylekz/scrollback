//! What is dragged in the library, where it is over, and what a drop there would do (DESIGN
//! §4.1 "Folder tree drag and drop", §4.4, §4.6): sessions onto a folder or Trash, a project
//! path onto a folder, a folder into another or between two.
//!
//! The library holds the drag ([`super::LibraryService::drag`]) so that the sidebar's rows,
//! the pages and the ghost read one answer: a row draws itself as the drop target only when
//! [`verdict`] says it takes the drop, and the ghost says what it does or why not.

use std::collections::HashSet;

use sb_core::catalogue::{Filed, SessionId};
use sb_core::filing::{FolderId, NormalPath};

use super::LibraryService;
use super::library::TypedPath;
use crate::format;

/// How far the pointer moves before a press is a drag (DESIGN §4.4).
pub const DRAG_THRESHOLD: f32 = 4.;

/// What is dragged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dragged {
    /// Sessions, the first one's title to name one alone.
    Sessions {
        ids: Vec<SessionId>,
        title: String,
    },
    /// A project path: from a folder's paths line, or from a group of Unsorted (`from` none)
    /// whose sessions it files.
    Path {
        path: TypedPath,
        normal: NormalPath,
        from: Option<FolderId>,
        sessions: usize,
    },
    Folder(FolderId),
}

/// Where a drop would land.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropTarget {
    /// Into a folder: its row in the sidebar.
    Folder(FolderId),
    /// Into a folder: its empty page, which takes sessions alone (DESIGN §4.4).
    EmptyFolder(FolderId),
    Trash,
    /**
     * The button that shows a narrow window's sidebar: it takes no drop, but resting on it
     * shows the sidebar over the main area until the drag ends, so that a folder can be
     * reached (spring-loading, as a closed folder opens).
     */
    Sidebar,
    /// Between two rows of the folder tree: as the folder at `index` of `parent`'s, its line
    /// drawn at the top of the row `row` of the tree, or at its bottom (`below`), `depth` in.
    Between {
        parent: Option<FolderId>,
        index: usize,
        row: usize,
        below: bool,
        depth: usize,
    },
}

/// A drag under way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DragState {
    pub what: Dragged,
    pub over: Option<DropTarget>,
    /// The pointer has moved [`DRAG_THRESHOLD`] from where it was pressed: until it has, nothing
    /// shows and nothing takes a drop.
    pub moved: bool,
    /// The sessions dragged, to tell at once whether a card is among them.
    held: HashSet<SessionId>,
}

/// What a drop on a target would do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// It takes the drop: the ghost says the result, then what it does.
    Takes { result: String, note: String },
    /// It refuses, and the ghost says why. No drop target shows.
    Refuses(String),
}

impl Verdict {
    pub fn takes(&self) -> bool {
        matches!(self, Self::Takes { .. })
    }
}

/// What a drop of `what` on `target` would do; `None` where it is no target at all.
pub fn verdict(what: &Dragged, target: DropTarget, library: &LibraryService) -> Option<Verdict> {
    let name = |folder| library.folder_path(folder);
    Some(match (what, target) {
        (
            Dragged::Sessions { ids, .. },
            DropTarget::Folder(folder) | DropTarget::EmptyFolder(folder),
        ) => {
            let there = ids.iter().all(|id| {
                library
                    .session(id)
                    .is_some_and(|session| session.filed == Filed::ByHand(folder))
            });
            if there {
                Verdict::Refuses(format!("already in {}", name(folder)))
            } else {
                Verdict::Takes {
                    result: name(folder),
                    note: "files by hand".to_string(),
                }
            }
        }
        (Dragged::Sessions { .. }, DropTarget::Trash) => Verdict::Takes {
            result: "Trash".to_string(),
            note: "moves to Trash".to_string(),
        },
        (
            Dragged::Path {
                path: (_, location),
                normal,
                from,
                sessions,
            },
            DropTarget::Folder(folder),
        ) => {
            let holder = library.attached().iter().find(|it| {
                it.attached.path == *normal
                    && it.attached.location == *location
                    && (it.attached.folder == folder || Some(it.attached.folder) != *from)
            });
            match holder {
                Some(holder) => Verdict::Refuses(format!(
                    "already attached to {}",
                    name(holder.attached.folder)
                )),
                None if *from == Some(folder) => {
                    Verdict::Refuses(format!("already attached to {}", name(folder)))
                }
                None => Verdict::Takes {
                    result: format!(
                        "{} {}",
                        if from.is_some() {
                            "move to"
                        } else {
                            "attach to"
                        },
                        name(folder)
                    ),
                    note: format!("files {}", format::plural(*sessions, "session")),
                },
            }
        }
        (Dragged::Folder(dragged), DropTarget::Folder(folder)) => {
            if inside(library, folder, *dragged) {
                Verdict::Refuses("can't go inside itself".to_string())
            } else if library.folder(*dragged).and_then(|it| it.parent) == Some(folder) {
                Verdict::Refuses(format!("already in {}", name(folder)))
            } else {
                Verdict::Takes {
                    result: name(folder),
                    note: "nests it".to_string(),
                }
            }
        }
        (Dragged::Folder(dragged), DropTarget::Between { parent, .. }) => {
            if parent.is_some_and(|parent| inside(library, parent, *dragged)) {
                Verdict::Refuses("can't go inside itself".to_string())
            } else {
                Verdict::Takes {
                    result: parent.map_or_else(|| "the top".to_string(), name),
                    note: "moves it here".to_string(),
                }
            }
        }
        (Dragged::Sessions { .. } | Dragged::Path { .. }, DropTarget::Between { .. })
        | (
            Dragged::Path { .. } | Dragged::Folder(_),
            DropTarget::Trash | DropTarget::EmptyFolder(_),
        )
        | (_, DropTarget::Sidebar) => return None,
    })
}

/// Whether `folder` is `outer` or a folder inside it.
fn inside(library: &LibraryService, folder: FolderId, outer: FolderId) -> bool {
    std::iter::successors(Some(folder), |id| {
        library.folder(*id).and_then(|it| it.parent)
    })
    .any(|id| id == outer)
}

impl DragState {
    /// A drag of `what` that has not moved yet.
    pub fn new(what: Dragged) -> Self {
        Self {
            held: match &what {
                Dragged::Sessions { ids, .. } => ids.iter().cloned().collect(),
                Dragged::Path { .. } | Dragged::Folder(_) => HashSet::new(),
            },
            what,
            over: None,
            moved: false,
        }
    }

    /// The drag over `over`, moved far enough or not.
    pub fn at(self, over: Option<DropTarget>, moved: bool) -> Self {
        Self {
            over,
            moved,
            ..self
        }
    }

    /// Whether the session `id` is among those dragged.
    pub fn holds(&self, id: &SessionId) -> bool {
        self.held.contains(id)
    }

    /// What a drop where the pointer is would do, once the pointer has moved far enough.
    pub fn verdict(&self, library: &LibraryService) -> Option<Verdict> {
        self.over
            .filter(|_| self.moved)
            .and_then(|over| verdict(&self.what, over, library))
    }

    /// Whether `target` is where the pointer is and takes the drop: it draws itself so.
    pub fn targets(&self, target: DropTarget, library: &LibraryService) -> bool {
        self.over == Some(target) && self.verdict(library).is_some_and(|it| it.takes())
    }
}
