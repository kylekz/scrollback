//! The folder tree's rows (DESIGN §4.1): the folders that show, top to bottom, each with how
//! deep it is. A folder's children show while it is open.

use std::collections::{HashMap, HashSet};

use sb_core::catalogue::Folder;
use sb_core::filing::FolderId;

/// One row of the tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeRow {
    /// Which of the folders it is, as an index into them.
    pub folder: usize,
    /// How many folders it is inside.
    pub depth: usize,
    pub has_children: bool,
    pub open: bool,
    /**
     * Whether the row keeps room for a chevron before its icon: every top-level row does, and
     * a nested one does when a folder beside it has children, so that the names of the
     * folders in one folder line up (`E-Folder` draws a nested row without the room).
     */
    pub chevron_room: bool,
}

/// The rows that show of `folders`, which are in the catalogue's order (those with one parent
/// together, in their order), with the folders in `open` showing their children.
pub fn rows(folders: &[Folder], open: &HashSet<FolderId>) -> Vec<TreeRow> {
    let mut children: HashMap<Option<FolderId>, Vec<usize>> = HashMap::new();
    for (at, folder) in folders.iter().enumerate() {
        children.entry(folder.parent).or_default().push(at);
    }
    let mut rows = Vec::with_capacity(folders.len());
    // Depth first: the folders still to show, the next one last.
    let mut pending: Vec<(usize, usize, bool)> = Vec::new();
    let push = |pending: &mut Vec<(usize, usize, bool)>, parent, depth: usize| {
        let Some(inside) = children.get(&parent) else {
            return;
        };
        let room = depth == 0
            || inside
                .iter()
                .any(|at| children.contains_key(&Some(folders[*at].id)));
        pending.extend(inside.iter().rev().map(|at| (*at, depth, room)));
    };
    push(&mut pending, None, 0);
    while let Some((at, depth, chevron_room)) = pending.pop() {
        let id = folders[at].id;
        let has_children = children.contains_key(&Some(id));
        let is_open = has_children && open.contains(&id);
        rows.push(TreeRow {
            folder: at,
            depth,
            has_children,
            open: is_open,
            chevron_room,
        });
        if is_open {
            push(&mut pending, Some(id), depth + 1);
        }
    }
    rows
}

/// The folders `folder` is inside, the nearest first.
pub fn ancestors(folders: &[Folder], folder: FolderId) -> Vec<FolderId> {
    let parent_of = |id| {
        folders
            .iter()
            .find(|folder| folder.id == id)
            .and_then(|folder| folder.parent)
    };
    std::iter::successors(parent_of(folder), |id| parent_of(*id)).collect()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn folder(id: i64, parent: Option<i64>, name: &str) -> Folder {
        Folder {
            id: FolderId(id),
            parent: parent.map(FolderId),
            name: name.to_string(),
        }
    }

    /// The folders of `E-Folder`, as the catalogue lists them: by parent, then in order.
    fn folders() -> Vec<Folder> {
        vec![
            folder(1, None, "dv"),
            folder(2, None, "fc"),
            folder(3, None, "folio"),
            folder(4, Some(1), "dv-wsl-backend"),
            folder(5, Some(2), "player"),
            folder(6, Some(2), "import"),
            folder(7, Some(5), "hwdec"),
        ]
    }

    fn shown(folders: &[Folder], open: &[i64]) -> Vec<(String, usize, bool, bool, bool)> {
        rows(folders, &open.iter().copied().map(FolderId).collect())
            .into_iter()
            .map(|row| {
                (
                    folders[row.folder].name.clone(),
                    row.depth,
                    row.has_children,
                    row.open,
                    row.chevron_room,
                )
            })
            .collect()
    }

    #[test]
    fn closed_folders_hide_what_is_inside_them() {
        let folders = folders();
        assert_eq!(
            shown(&folders, &[]),
            [
                ("dv".to_string(), 0, true, false, true),
                ("fc".to_string(), 0, true, false, true),
                ("folio".to_string(), 0, false, false, true),
            ]
        );
    }

    #[test]
    fn an_open_folder_shows_its_children_under_it_one_level_in() {
        let folders = folders();
        assert_eq!(
            shown(&folders, &[2]),
            [
                ("dv".to_string(), 0, true, false, true),
                ("fc".to_string(), 0, true, true, true),
                // `player` has a folder in it, so both rows keep room for a chevron.
                ("player".to_string(), 1, true, false, true),
                ("import".to_string(), 1, false, false, true),
                ("folio".to_string(), 0, false, false, true),
            ]
        );
        assert_eq!(
            shown(&folders, &[1, 2, 5]),
            [
                ("dv".to_string(), 0, true, true, true),
                // None of `dv`'s folders has one in it: no room for a chevron.
                ("dv-wsl-backend".to_string(), 1, false, false, false),
                ("fc".to_string(), 0, true, true, true),
                ("player".to_string(), 1, true, true, true),
                ("hwdec".to_string(), 2, false, false, false),
                ("import".to_string(), 1, false, false, true),
                ("folio".to_string(), 0, false, false, true),
            ]
        );
    }

    #[test]
    fn a_folder_open_inside_a_closed_one_stays_hidden() {
        let folders = folders();
        assert_eq!(shown(&folders, &[5]), shown(&folders, &[]));
    }

    #[test]
    fn ancestors_run_from_the_nearest_to_the_top() {
        let folders = folders();
        assert_eq!(ancestors(&folders, FolderId(7)), [FolderId(5), FolderId(2)]);
        assert_eq!(ancestors(&folders, FolderId(1)), []);
    }
}
