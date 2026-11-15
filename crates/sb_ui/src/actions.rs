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

/// The shell's own actions, dispatched in the `Shell` key context.
pub mod shell {
    use gpui::actions;

    actions!(
        shell,
        [
            /// Show or hide the sidebar while the window is too narrow to dock it (`Ctrl B`,
            /// DESIGN §2).
            ToggleSidebar,
            /// Close the top overlay (`Esc`, DESIGN §5).
            Dismiss,
        ]
    );
}
