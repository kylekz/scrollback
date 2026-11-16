//! The keymap (DESIGN §5). One table, [`SHORTCUTS`], holds the gpui bindings, and a view draws
//! a bound action's keycap from its row ([`Shortcut::label`]), so the two can't drift. Settings
//! › Shortcuts lists what [`SHORTCUT_ROWS`] says each action does, with the keys these rows
//! bind it to.
//!
//! One keycap is ahead of the table: the title bar shows DESIGN §5's key for the palette, whose
//! action arrives with it in phase 6 (PLAN §13). Its keys are [`SEARCH_KEYS`] until then; it
//! becomes a row with its action.
//!
//! `secondary` is gpui's platform modifier: `Ctrl` on Windows, `Cmd` on macOS (DESIGN §5:
//! "macOS uses Cmd for every Ctrl").

use gpui::{Action, App, KeyBinding, KeyBindingContextPredicate, SharedString};

use crate::actions::{app, first_run, library, locations, menu, sessions, shell, sidebar};

/// Key context of the main area while it shows Settings (DESIGN §4.13).
pub const SETTINGS: &str = "Settings";

/// Key context of the shell's root element: everything in the main window.
pub const SHELL: &str = "Shell";
/// Key context of the sidebar, while it has the keyboard.
pub const SIDEBAR: &str = "Sidebar";
/// Key context of a folder's row while the folder is renamed in it.
pub const FOLDER_RENAME: &str = "FolderRename";
/// Key context of the main area while it shows session cards: All sessions, Starred, a folder.
pub const SESSIONS: &str = "Sessions";
/// Key context of the main area while it shows Unsorted.
pub const UNSORTED: &str = "Unsorted";
/// Key context of the main area while it shows the first run.
pub const FIRST_RUN: &str = "FirstRun";
/// Key context of the locations sheet (DESIGN §4.11), and of its list while it has the
/// keyboard.
pub const LOCATIONS_SHEET: &str = "LocationsSheet";
pub const LOCATIONS_LIST: &str = "LocationsList";
/// Key context of a dialog over the sheet or the first run: adding a location (DESIGN
/// §4.12), confirming a removal.
pub const LOCATION_DIALOG: &str = "LocationDialog";
/// Key context of an open menu.
pub const MENU: &str = "Menu";
/// Key context of a folder's paths editor (DESIGN §4.7).
pub const PATHS_EDITOR: &str = "PathsEditor";
/**
 * Key context of a dialog over a library page: Delete folder… and the question about the
 * paths editor's changes. It sits outside the page's context, so the page's keys don't reach
 * the page behind it; `Esc` is the shell's `Dismiss`, and `Enter` presses only the button
 * `Tab` put the keyboard on.
 */
pub const LIBRARY_DIALOG: &str = "LibraryDialog";

/// One row of the keymap.
#[derive(Clone, Copy)]
pub struct Shortcut {
    /// gpui keystroke syntax, e.g. `secondary-b`.
    pub keys: &'static str,
    /// The key context the row is bound in (one of the constants above).
    pub context: &'static str,
    pub action: fn() -> Box<dyn Action>,
}

impl std::fmt::Debug for Shortcut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shortcut")
            .field("keys", &self.keys)
            .field("context", &self.context)
            .field("action", &(self.action)().name())
            .finish()
    }
}

impl Shortcut {
    /// The gpui binding for this row, its keys parsed for the app's OS and keyboard.
    pub fn key_binding(&self, cx: &App) -> anyhow::Result<KeyBinding> {
        let predicate = KeyBindingContextPredicate::parse(self.context)?;
        Ok(KeyBinding::load(
            self.keys,
            (self.action)(),
            Some(predicate.into()),
            false,
            None,
            cx,
        )?)
    }

    /// The keycap text, e.g. `Ctrl B`.
    pub fn label(&self) -> SharedString {
        label(self.keys)
    }
}

macro_rules! shortcut {
    ($keys:literal, $context:expr, $action:expr) => {
        Shortcut {
            keys: $keys,
            context: $context,
            action: || Box::new($action),
        }
    };
}

/// `Ctrl B`: the sidebar of a narrow window (DESIGN §2). Named, because its button draws it.
pub const TOGGLE_SIDEBAR: Shortcut = shortcut!("secondary-b", SHELL, shell::ToggleSidebar);
/// `Ctrl Shift N` and `F5`. Named, because their buttons say them.
pub const NEW_FOLDER: Shortcut = shortcut!("secondary-shift-n", SHELL, library::NewFolder);
pub const SYNC_ALL: Shortcut = shortcut!("f5", SHELL, library::SyncAll);
/// `Alt ←`. Named, because the reader's Back button says it.
pub const BACK: Shortcut = shortcut!("alt-left", SHELL, library::Back);
/// `S`. Named, because a card's star button says it.
pub const STAR: Shortcut = shortcut!("s", SESSIONS, sessions::Star);
/// `Ctrl L`. Named, because the Locations overline's label says it.
pub const LOCATIONS: Shortcut = shortcut!("secondary-l", SHELL, shell::OpenLocations);
/// `Ctrl ,`. Named, because the title bar's Settings button says it.
pub const OPEN_SETTINGS: Shortcut = shortcut!("secondary-,", SHELL, shell::OpenSettings);
/// `Delete`, `M`, `Enter` and `Shift F10` on a page of sessions. Named, because the session
/// menu and the hover bar say them.
pub const TRASH: Shortcut = shortcut!("delete", SESSIONS, sessions::Trash);
pub const MOVE: Shortcut = shortcut!("m", SESSIONS, sessions::Move);
pub const OPEN: Shortcut = shortcut!("enter", SESSIONS, sessions::Open);
pub const MENU_KEY: Shortcut = shortcut!("shift-f10", SESSIONS, sessions::Menu);

/// Every binding scrollback defines.
pub const SHORTCUTS: &[Shortcut] = &[
    TOGGLE_SIDEBAR,
    shortcut!("f6", SHELL, shell::FocusOther),
    shortcut!("escape", SHELL, shell::Dismiss),
    NEW_FOLDER,
    shortcut!("f2", SHELL, sidebar::Rename),
    SYNC_ALL,
    BACK,
    shortcut!("up", SIDEBAR, sidebar::Previous),
    shortcut!("down", SIDEBAR, sidebar::Next),
    shortcut!("left", SIDEBAR, sidebar::Collapse),
    shortcut!("right", SIDEBAR, sidebar::Expand),
    shortcut!("enter", SIDEBAR, sidebar::Open),
    shortcut!("enter", FOLDER_RENAME, sidebar::KeepName),
    shortcut!("escape", FOLDER_RENAME, sidebar::DropName),
    shortcut!("left", SESSIONS, sessions::Left),
    shortcut!("right", SESSIONS, sessions::Right),
    shortcut!("up", SESSIONS, sessions::Up),
    shortcut!("down", SESSIONS, sessions::Down),
    OPEN,
    STAR,
    TRASH,
    MOVE,
    MENU_KEY,
    shortcut!("secondary-a", SESSIONS, sessions::SelectAll),
    shortcut!("up", UNSORTED, sessions::Up),
    shortcut!("down", UNSORTED, sessions::Down),
    shortcut!("enter", UNSORTED, sessions::Open),
    shortcut!("s", UNSORTED, sessions::Star),
    shortcut!("delete", UNSORTED, sessions::Trash),
    shortcut!("m", UNSORTED, sessions::Move),
    shortcut!("shift-f10", UNSORTED, sessions::Menu),
    shortcut!("up", MENU, menu::Previous),
    shortcut!("down", MENU, menu::Next),
    shortcut!("enter", MENU, menu::Choose),
    shortcut!("space", MENU, menu::Choose),
    shortcut!("right", MENU, menu::Open),
    shortcut!("left", MENU, menu::Back),
    shortcut!("escape", MENU, menu::Close),
    shortcut!("secondary-enter", PATHS_EDITOR, sessions::SavePaths),
    shortcut!("escape", PATHS_EDITOR, sessions::CancelPaths),
    shortcut!("enter", FIRST_RUN, first_run::Add),
    LOCATIONS,
    OPEN_SETTINGS,
    shortcut!("escape", LOCATIONS_SHEET, locations::Close),
    shortcut!("up", LOCATIONS_LIST, locations::Previous),
    shortcut!("down", LOCATIONS_LIST, locations::Next),
    shortcut!("enter", LOCATIONS_LIST, locations::Open),
    shortcut!("escape", LOCATION_DIALOG, locations::Close),
    shortcut!("enter", LOCATION_DIALOG, locations::Confirm),
];

/// `Ctrl K`, the palette's key (module docs).
pub const SEARCH_KEYS: &str = "secondary-k";

/// A row of Settings › Shortcuts (DESIGN §5, §4.13): what it does, and the actions whose keys
/// it shows, by their names (`sidebar::Rename`). The keys are whatever [`SHORTCUTS`] binds
/// those actions to.
#[derive(Clone, Copy, Debug)]
pub struct ShortcutRow {
    pub does: &'static str,
    pub actions: &'static [&'static str],
}

const fn row(does: &'static str, actions: &'static [&'static str]) -> ShortcutRow {
    ShortcutRow { does, actions }
}

/// Settings › Shortcuts, in DESIGN §5's order: every key that works wherever the keyboard is
/// in the window, then the keys of lists and pages. A key DESIGN §5 lists whose action isn't
/// built yet has no row until it is.
pub const SHORTCUT_ROWS: &[ShortcutRow] = &[
    row(
        "Back to the page the session was opened from, or leave Settings",
        &["library::Back"],
    ),
    row("Star or unstar the selected session", &["sessions::Star"]),
    row(
        "Move the focused or selected sessions to Trash",
        &["sessions::Trash"],
    ),
    row("New folder", &["library::NewFolder"]),
    row("Sync all", &["library::SyncAll"]),
    row("Locations", &["shell::OpenLocations"]),
    row("Settings", &["shell::OpenSettings"]),
    row(
        "Move and open in the sidebar and the locations list",
        &[
            "sidebar::Previous",
            "sidebar::Next",
            "sidebar::Open",
            "locations::Previous",
            "locations::Next",
            "locations::Open",
        ],
    ),
    row(
        "Move between sessions",
        &[
            "sessions::Left",
            "sessions::Right",
            "sessions::Up",
            "sessions::Down",
        ],
    ),
    row("Open the selected session", &["sessions::Open"]),
    row("Select every session shown", &["sessions::SelectAll"]),
    row("Move the focused session to a folder", &["sessions::Move"]),
    row("The focused session's menu", &["sessions::Menu"]),
    row(
        "Move and choose in a menu",
        &["menu::Previous", "menu::Next", "menu::Choose"],
    ),
    row("Save the paths editor's changes", &["sessions::SavePaths"]),
    row("Rename the folder shown", &["sidebar::Rename"]),
    row(
        "Narrow windows: show the sidebar; otherwise move between the sidebar and the page",
        &["shell::ToggleSidebar"],
    ),
    row(
        "Move between the sidebar and the page",
        &["shell::FocusOther"],
    ),
    row(
        "Close what is on top, or leave Settings",
        &["shell::Dismiss"],
    ),
];

impl ShortcutRow {
    /// The keycaps of its actions' bindings on this platform, each once, in the table's
    /// order.
    pub fn keycaps(&self) -> Vec<SharedString> {
        let macos: &[Shortcut] = if cfg!(target_os = "macos") {
            MACOS_SHORTCUTS
        } else {
            &[]
        };
        let mut keycaps: Vec<SharedString> = Vec::new();
        for name in self.actions {
            for shortcut in SHORTCUTS.iter().chain(macos) {
                let label = shortcut.label();
                if (shortcut.action)().name() == *name && !keycaps.contains(&label) {
                    keycaps.push(label);
                }
            }
        }
        keycaps
    }
}

/**
 * The keys every Mac app has (DESIGN §5, "macOS"), bound on macOS only: anywhere in the window
 * those the app menu shows (`crate::app_menu`), and `⌘⌫`, which moves to Trash as `Delete`
 * does. Plain `Backspace` moves nothing to Trash on any platform: it is too easily pressed
 * meaning to edit text.
 */
pub const MACOS_SHORTCUTS: &[Shortcut] = &[
    shortcut!("cmd-q", SHELL, app::Quit),
    shortcut!("cmd-h", SHELL, app::Hide),
    shortcut!("alt-cmd-h", SHELL, app::HideOthers),
    shortcut!("cmd-m", SHELL, app::Minimize),
    shortcut!("cmd-backspace", SESSIONS, sessions::Trash),
    shortcut!("cmd-backspace", UNSORTED, sessions::Trash),
];

/// Binds [`SHORTCUTS`], and on macOS [`MACOS_SHORTCUTS`]. Call after `gpui_component::init`, so
/// scrollback's rows are registered last and win within a context depth.
pub fn init(cx: &mut App) -> anyhow::Result<()> {
    let macos: &[Shortcut] = if cfg!(target_os = "macos") {
        MACOS_SHORTCUTS
    } else {
        &[]
    };
    let bindings = SHORTCUTS
        .iter()
        .chain(macos)
        .map(|shortcut| shortcut.key_binding(cx))
        .collect::<anyhow::Result<Vec<_>>>()?;
    cx.bind_keys(bindings);
    Ok(())
}

/// How keycaps spell modifiers (DESIGN §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keycaps {
    /// Windows: words, `Ctrl K`, `Ctrl Shift N`, `Alt ←`.
    Words,
    /// macOS: the symbols of its menus in its order (`⌃⌥⇧⌘`) against the key, `⌘K`, `⇧⌘N`,
    /// `⌥←`.
    Symbols,
}

impl Keycaps {
    /// This platform's.
    pub const NATIVE: Self = if cfg!(target_os = "macos") {
        Self::Symbols
    } else {
        Self::Words
    };
}

/// Keycap text for gpui keystroke syntax in this platform's spelling (DESIGN §5 writes
/// `Ctrl K`; macOS `⌘K`): `secondary` is `Ctrl`, or `⌘` on macOS; arrows are drawn as arrows.
pub fn label(keys: &str) -> SharedString {
    label_as(keys, Keycaps::NATIVE)
}

/// [`label`] in the spelling `caps`.
pub fn label_as(keys: &str, caps: Keycaps) -> SharedString {
    let (modifiers, key) = match keys.rsplit_once('-') {
        Some((modifiers, key)) if !key.is_empty() => (modifiers, key),
        _ => ("", keys),
    };
    let modifiers = modifiers.split('-').filter(|modifier| !modifier.is_empty());
    let key = match key {
        "left" => "←".to_string(),
        "right" => "→".to_string(),
        "up" => "↑".to_string(),
        "down" => "↓".to_string(),
        "escape" => "Esc".to_string(),
        "backspace" if caps == Keycaps::Symbols => "⌫".to_string(),
        other if other.chars().count() > 1 => capitalize(other),
        other => other.to_uppercase(),
    };
    match caps {
        Keycaps::Words => {
            let mut parts: Vec<String> = modifiers
                .map(|modifier| match modifier {
                    "secondary" | "ctrl" => "Ctrl".to_string(),
                    other => capitalize(other),
                })
                .collect();
            parts.push(key);
            parts.join(" ").into()
        }
        Keycaps::Symbols => {
            // macOS's order, whatever order the keystroke names them in.
            const ORDER: [&str; 4] = ["⌃", "⌥", "⇧", "⌘"];
            let mut symbols: Vec<String> = modifiers
                .map(|modifier| match modifier {
                    "ctrl" => "⌃".to_string(),
                    "alt" => "⌥".to_string(),
                    "shift" => "⇧".to_string(),
                    "secondary" | "cmd" => "⌘".to_string(),
                    other => capitalize(other),
                })
                .collect();
            symbols.sort_by_key(|symbol| ORDER.iter().position(|it| it == symbol));
            (symbols.concat() + &key).into()
        }
    }
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[gpui::test]
    fn every_row_binds_and_no_keystroke_is_bound_twice_in_one_context(
        cx: &mut gpui::TestAppContext,
    ) {
        let mut seen = HashSet::new();
        for shortcut in SHORTCUTS.iter().chain(MACOS_SHORTCUTS) {
            cx.update(|cx| shortcut.key_binding(cx))
                .unwrap_or_else(|error| panic!("{shortcut:?}: {error}"));
            assert!(
                seen.insert((shortcut.keys, shortcut.context)),
                "{shortcut:?} is bound twice"
            );
        }
    }

    #[test]
    fn every_row_of_settings_shows_keys_that_are_bound() {
        for row in SHORTCUT_ROWS {
            for name in row.actions {
                assert!(
                    SHORTCUTS
                        .iter()
                        .any(|shortcut| (shortcut.action)().name() == *name),
                    "{name} ({}) is bound to no key",
                    row.does
                );
            }
        }
        let caps = |does: &str| {
            SHORTCUT_ROWS
                .iter()
                .find(|row| row.does == does)
                .unwrap()
                .keycaps()
        };
        assert_eq!(caps("Settings"), [OPEN_SETTINGS.label()]);
        /* On macOS ⌘⌫ moves to Trash as Delete does; Backspace alone never does. */
        let trash = caps("Move the focused or selected sessions to Trash");
        assert_eq!(trash[0], TRASH.label());
        assert_eq!(trash.len(), if cfg!(target_os = "macos") { 2 } else { 1 });
        assert!(!trash.contains(&label("backspace")));
        /* ↑ is bound for the cards and for Unsorted's rows: shown once. */
        assert_eq!(
            caps("Move between sessions"),
            ["left", "right", "up", "down"].map(label)
        );
    }

    /// A key that works wherever the keyboard is in the window can't be added without a row
    /// in Settings › Shortcuts.
    #[test]
    fn every_key_of_the_whole_window_has_a_row_in_settings() {
        let listed: HashSet<&str> = SHORTCUT_ROWS
            .iter()
            .flat_map(|row| row.actions.iter().copied())
            .collect();
        for shortcut in SHORTCUTS.iter().filter(|it| it.context == SHELL) {
            let name = (shortcut.action)().name();
            assert!(
                listed.contains(name),
                "{name} has no row in Settings › Shortcuts"
            );
        }
    }

    #[test]
    fn labels_read_as_design_writes_them() {
        let words = |keys| label_as(keys, Keycaps::Words);
        assert_eq!(words("secondary-k"), "Ctrl K");
        assert_eq!(words("secondary-shift-n"), "Ctrl Shift N");
        assert_eq!(words("alt-left"), "Alt ←");
        assert_eq!(words("escape"), "Esc");
        assert_eq!(words("f5"), "F5");
        assert_eq!(words("secondary-,"), "Ctrl ,");

        let symbols = |keys| label_as(keys, Keycaps::Symbols);
        assert_eq!(symbols("secondary-k"), "⌘K");
        assert_eq!(symbols("secondary-shift-n"), "⇧⌘N");
        assert_eq!(symbols("alt-left"), "⌥←");
        assert_eq!(symbols("cmd-shift-alt-ctrl-x"), "⌃⌥⇧⌘X");
        assert_eq!(symbols("escape"), "Esc");
        assert_eq!(symbols("secondary-,"), "⌘,");
        assert_eq!(symbols("cmd-backspace"), "⌘⌫");
        assert_eq!(words("backspace"), "Backspace");
    }
}
