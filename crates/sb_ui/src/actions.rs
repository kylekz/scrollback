//! scrollback's actions (DESIGN §5), one module per key context; the bindings live in
//! [`crate::keymap`]. Each module is also the action namespace, so names read
//! `shell::ToggleSidebar`, and automation's `actions` command lists them all.

/// The app's own actions on macOS (`crate::app_menu`): its menu bar's and the keys every Mac
/// app has. Handled by the app, wherever the keyboard is.
pub mod app {
    use gpui::actions;

    actions!(
        app,
        [
            /// Quit scrollback (`⌘Q`).
            Quit,
            /// Hide scrollback (`⌘H`).
            Hide,
            /// Hide the other apps (`⌥⌘H`).
            HideOthers,
            /// Show every app again.
            ShowAll,
            /// Minimize the window to the Dock (`⌘M`).
            Minimize,
        ]
    );
}

/// The shell's own actions, dispatched in the `Shell` key context: wherever the keyboard is
/// in the main window.
pub mod shell {
    use gpui::actions;

    actions!(
        shell,
        [
            /// Move the keyboard between the sidebar and the main area; in a window too narrow
            /// to dock the sidebar, show or hide it (`Ctrl B`, DESIGN §2, §4.1).
            ToggleSidebar,
            /// Move the keyboard between the sidebar and the main area (`F6`, DESIGN §4.1).
            FocusOther,
            /// Close the top overlay (`Esc`, DESIGN §5).
            Dismiss,
            /// Open the locations sheet (`Ctrl L`, DESIGN §4.11).
            OpenLocations,
        ]
    );
}

/// The library's actions, which work wherever the keyboard is in the main window.
pub mod library {
    use gpui::actions;

    actions!(
        library,
        [
            /// Make a folder and name it in place (`Ctrl Shift N`).
            NewFolder,
            /// Sync every location (`F5`).
            SyncAll,
            /// Go back to the page a session was opened from (`Alt ←`).
            Back,
        ]
    );
}

/// The sidebar's actions, while it has the keyboard.
pub mod sidebar {
    use gpui::actions;

    actions!(
        sidebar,
        [
            /// Show the page of the row above (`↑`).
            Previous,
            /// Show the page of the row below (`↓`).
            Next,
            /// Close the selected folder, or go to the folder it is in (`←`).
            Collapse,
            /// Open the selected folder (`→`).
            Expand,
            /// Move the keyboard to the page shown (`Enter`).
            Open,
            /// Rename the folder whose page is shown, in its row (`F2`, wherever the keyboard
            /// is).
            Rename,
            /// Keep the name typed for the folder being renamed (`Enter`).
            KeepName,
            /// Leave the folder being renamed as it was named (`Esc`).
            DropName,
        ]
    );
}

/// The actions of a page of sessions, while the main area has the keyboard.
pub mod sessions {
    use gpui::actions;

    actions!(
        sessions,
        [
            /// Select the session before (`←`).
            Left,
            /// Select the session after (`→`).
            Right,
            /// Select the session above (`↑`).
            Up,
            /// Select the session below (`↓`).
            Down,
            /// Open the selected session (`Enter`).
            Open,
            /// Star or unstar the selected session (`S`).
            Star,
            /// Move the selected sessions to Trash (`Delete`).
            Trash,
            /// Open Move to folder for the selected sessions (`M`).
            Move,
            /// Select every session the page shows (`Ctrl A`).
            SelectAll,
            /// Open the selected session's menu (`Shift F10`).
            Menu,
            /// In the paths editor: save the changes (`Ctrl Enter`).
            SavePaths,
            /// In the paths editor: drop the changes and close it (`Esc`).
            CancelPaths,
        ]
    );
}

/// A menu's actions, while it has the keyboard (DESIGN §5).
pub mod menu {
    use gpui::actions;

    actions!(
        menu,
        [
            /// The item above (`↑`).
            Previous,
            /// The item below (`↓`).
            Next,
            /// Do what the item says, or open its submenu (`Enter`, `Space`).
            Choose,
            /// Open the item's submenu (`→`).
            Open,
            /// Close the submenu, back to the menu it is in (`←`).
            Back,
            /// Close the menu (`Esc`).
            Close,
        ]
    );
}

/// The reader's actions, while the main area shows a session and has the keyboard.
pub mod reader {
    use gpui::actions;

    actions!(
        reader,
        [
            /// Open the next session of the page the session was opened from (`J`).
            NextSession,
            /// Open the previous session of that page (`K`).
            PreviousSession,
            /// Go to the next turn (`]`).
            NextTurn,
            /// Go to the previous turn (`[`).
            PreviousTurn,
            /// Switch between Activity and Full calls (`E`).
            SwitchCalls,
            /// Star or unstar the session (`S`).
            Star,
            /// Move to the row above (`↑`).
            Up,
            /// Move to the row below (`↓`).
            Down,
            /// Open or close the row the keyboard is on (`Enter`).
            Toggle,
            /// Open it (`→`).
            Expand,
            /// Close it, or go to the row it is under (`←`).
            Collapse,
            /// Copy the message the keyboard is on, or a call's command or path (`Ctrl C`).
            Copy,
            /// Scroll a screen up (`Page Up`).
            PageUp,
            /// Scroll a screen down (`Page Down`).
            PageDown,
            /// Go to the first row (`Home`).
            Top,
            /// Go to the last row (`End`).
            Bottom,
        ]
    );
}

/// The first run's actions.
pub mod first_run {
    use gpui::actions;

    actions!(
        first_run,
        [
            /// Add the checked locations and sync them (`Enter`).
            Add,
        ]
    );
}

/// The locations sheet's and the add-location dialog's actions (DESIGN §4.11, §4.12).
pub mod locations {
    use gpui::actions;

    actions!(
        locations,
        [
            /// Close the dialog, or the sheet when no dialog is over it (`Esc`).
            Close,
            /// Select the location above (`↑`).
            Previous,
            /// Select the location below (`↓`).
            Next,
            /// Move the keyboard into the selected location's pane (`Enter`).
            Open,
            /// In a dialog: test the host typed, or do what the dialog is for when nothing
            /// can go wrong by it (`Enter`).
            Confirm,
        ]
    );
}
