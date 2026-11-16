//! The library's menus (DESIGN §4.1, §4.4, §4.5): a session's, its Move to folder, and a
//! folder's actions. Cards, list rows, Unsorted's rows, the hover bar, a folder page's `⋯` and
//! the sidebar's folder rows open them; [`crate::menu`] draws them.

use std::collections::HashSet;

use gpui::{App, ClipboardItem, Pixels, px};
use sb_core::catalogue::{Filed, Session, SessionId, Shelf};
use sb_core::filing::FolderId;

use crate::icons::SbIcon;
use crate::keymap;
use crate::menu::{Entry, Item, NESTED_INDENT};
use crate::services::library::NEW_FOLDER;
use crate::services::{ActiveServices as _, LibraryService};
use crate::sidebar::tree;

/// The session menu's width, and Move to folder's (DESIGN §4.5).
pub const SESSION_MENU_WIDTH: Pixels = px(230.);
pub const MOVE_MENU_WIDTH: Pixels = px(220.);
/// A folder's actions menu.
pub const FOLDER_MENU_WIDTH: Pixels = px(210.);

/// What History… says while the versions it shows are not built (DESIGN §4.3, PLAN §13).
const NO_HISTORY: &str = "History arrives with the versions, in a later phase";

/**
 * The menu of `session`, acting on `ids` (the selection it is in, or itself): Open, Star,
 * Move to folder ›, Copy project path, History…, File automatically while it is filed by
 * hand, and Move to Trash.
 */
pub fn session_menu(session: &Session, ids: Vec<SessionId>, cx: &App) -> Vec<Entry> {
    let library = cx.services().library.read(cx);
    let starred = ids
        .iter()
        .all(|id| library.session(id).is_some_and(|it| it.starred));
    let open = session.clone();
    let path = session.project_path.clone();
    let mut entries = vec![
        Entry::Item(
            Item::new("open", "Open")
                .glyph(SbIcon::Eye)
                .keys(keymap::OPEN.label())
                .on(move |_, cx| {
                    let library = cx.services().library.clone();
                    library.update(cx, |library, cx| library.open(open.clone(), cx));
                }),
        ),
        Entry::Item(
            Item::new("star", if starred { "Unstar" } else { "Star" })
                .glyph(SbIcon::Star)
                .keys(keymap::STAR.label())
                .on({
                    let ids = ids.clone();
                    move |_, cx| {
                        let library = cx.services().library.clone();
                        library.update(cx, |library, cx| library.star(ids.clone(), !starred, cx));
                    }
                }),
        ),
        Entry::Item(
            Item::new("move", "Move to folder")
                .glyph(SbIcon::FolderInput)
                .submenu(move_entries(ids.clone(), session.filed, library)),
        ),
        Entry::Item(match path {
            Some(path) => Item::new("copy-path", "Copy project path")
                .glyph(SbIcon::Copy)
                .on(move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(path.clone()))),
            None => Item::new("copy-path", "Copy project path")
                .glyph(SbIcon::Copy)
                .off("This session recorded no project path"),
        }),
        Entry::Item(
            Item::new("history", "History…")
                .glyph(SbIcon::History)
                .off(NO_HISTORY),
        ),
    ];
    if ids.iter().any(|id| {
        library
            .session(id)
            .is_some_and(|it| matches!(it.filed, Filed::ByHand(_)))
    }) {
        let ids = ids.clone();
        entries.push(Entry::Item(
            Item::new("file-automatically", "File automatically")
                .glyph(SbIcon::Hand)
                .on(move |_, cx| {
                    let library = cx.services().library.clone();
                    library.update(cx, |library, cx| {
                        library.file_automatically(ids.clone(), cx)
                    });
                }),
        ));
    }
    entries.push(Entry::Separator);
    entries.push(Entry::Item(
        Item::new("trash", "Move to Trash")
            .glyph(SbIcon::Trash)
            .keys(keymap::TRASH.label())
            .danger()
            .on(move |_, cx| {
                let library = cx.services().library.clone();
                library.update(cx, |library, cx| library.trash(ids.clone(), cx));
            }),
    ));
    entries
}

/**
 * Move to folder (DESIGN §4.5): how the session is filed now, every folder as a radio item
 * in the tree's order, the one it is in checked and those inside others indented, then New
 * folder…, which makes one and files `ids` in it.
 */
pub fn move_entries(ids: Vec<SessionId>, filed: Filed, library: &LibraryService) -> Vec<Entry> {
    let current = match filed {
        Filed::ByRule(folder) | Filed::ByHand(folder) => Some(folder),
        Filed::Unsorted => None,
    };
    let header = match filed {
        Filed::ByRule(folder) => format!("Filed in {} by its path", library.folder_path(folder)),
        Filed::ByHand(folder) => format!("Filed in {} by hand", library.folder_path(folder)),
        Filed::Unsorted => "In Unsorted".to_string(),
    };
    let folders = library.folders();
    let every: HashSet<FolderId> = folders.iter().map(|folder| folder.id).collect();
    let mut entries = vec![Entry::Header(header.into())];
    entries.extend(tree::rows(folders, &every).into_iter().map(|row| {
        let folder = &folders[row.folder];
        let (id, ids) = (folder.id, ids.clone());
        Entry::Item(
            Item::new(
                format!("folder/{}", folder_key(library, id)),
                folder.name.clone(),
            )
            .glyph(SbIcon::Folder)
            .checked(current == Some(id))
            .indent(NESTED_INDENT * row.depth as f32)
            .on(move |_, cx| {
                let library = cx.services().library.clone();
                library.update(cx, |library, cx| library.file_by_hand(ids.clone(), id, cx));
            }),
        )
    }));
    entries.push(Entry::Separator);
    entries.push(Entry::Item(
        Item::new("new-folder", "New folder…")
            .glyph(SbIcon::FolderPlus)
            .on(move |_, cx| {
                let library = cx.services().library.clone();
                library.update(cx, |library, cx| library.new_folder_with(ids.clone(), cx));
            }),
    ));
    entries
}

/**
 * How automation names a folder: its name with the folders it is in, as `fc/player`; or, in a
 * catalogue from before two folders in one place were kept from sharing a name, where another
 * folder's is the same, `#` and its id.
 */
pub fn folder_key(library: &LibraryService, folder: FolderId) -> String {
    let path = |folder: FolderId| {
        let mut names: Vec<&str> = std::iter::successors(library.folder(folder), |it| {
            it.parent.and_then(|parent| library.folder(parent))
        })
        .map(|it| it.name.as_str())
        .collect();
        names.reverse();
        names.join("/")
    };
    let key = path(folder);
    let shared =
        (library.folders().iter()).any(|other| other.id != folder && path(other.id) == key);
    if shared {
        format!("#{}", folder.0)
    } else {
        key
    }
}

/**
 * A folder's actions (DESIGN §4.1, §4.4): Open where it isn't its page (`open`), Rename in
 * its row, New subfolder, Edit paths… on its page, and Delete folder…, which asks first.
 */
pub fn folder_menu(folder: FolderId, open: bool) -> Vec<Entry> {
    let act = |act: fn(&mut LibraryService, FolderId, &mut gpui::Context<LibraryService>)| {
        move |_: &mut gpui::Window, cx: &mut App| {
            let library = cx.services().library.clone();
            library.update(cx, |library, cx| act(library, folder, cx));
        }
    };
    let mut entries = Vec::new();
    if open {
        entries.push(Entry::Item(
            Item::new("open", "Open")
                .glyph(SbIcon::Eye)
                .on(act(|library, folder, cx| {
                    library.show(Shelf::Folder(folder), cx);
                })),
        ));
    }
    entries.extend([
        Entry::Item(
            Item::new("rename", "Rename")
                .glyph(SbIcon::Pen)
                .keys(keymap::label("f2"))
                .on(act(LibraryService::ask_rename)),
        ),
        Entry::Item(
            Item::new("new-subfolder", "New subfolder")
                .glyph(SbIcon::FolderPlus)
                .on(act(|library, folder, cx| {
                    library.create_folder(NEW_FOLDER.to_string(), Some(folder), cx);
                })),
        ),
        Entry::Item(
            Item::new("edit-paths", "Edit paths…")
                .glyph(SbIcon::Link)
                .on(act(LibraryService::edit_paths)),
        ),
        Entry::Separator,
        Entry::Item(
            Item::new("delete", "Delete folder…")
                .glyph(SbIcon::Trash)
                .danger()
                .on(act(LibraryService::ask_delete)),
        ),
    ]);
    entries
}
