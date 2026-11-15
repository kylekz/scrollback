//! The keymap (DESIGN §5). One table, [`SHORTCUTS`], holds the gpui bindings, and a view draws
//! a bound action's keycap from its row ([`Shortcut::label`]), so the two can't drift.
//!
//! Two keycaps are ahead of the table: the title bar shows DESIGN §5's keys for the palette and
//! for Settings, whose actions arrive with them in phases 6 and 7 (PLAN §13). Their keys are
//! [`SEARCH_KEYS`] and [`SETTINGS_KEYS`] until then; each becomes a row with its action.
//!
//! `secondary` is gpui's platform modifier: `Ctrl` on Windows, `Cmd` on macOS (DESIGN §5:
//! "macOS uses Cmd for every Ctrl").

use gpui::{Action, App, KeyBinding, KeyBindingContextPredicate, SharedString};

use crate::actions::{app, first_run, library, locations, sessions, shell, sidebar};

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
    shortcut!("enter", SESSIONS, sessions::Open),
    STAR,
    shortcut!("up", UNSORTED, sessions::Up),
    shortcut!("down", UNSORTED, sessions::Down),
    shortcut!("enter", UNSORTED, sessions::Open),
    shortcut!("enter", FIRST_RUN, first_run::Add),
    LOCATIONS,
    shortcut!("escape", LOCATIONS_SHEET, locations::Close),
    shortcut!("up", LOCATIONS_LIST, locations::Previous),
    shortcut!("down", LOCATIONS_LIST, locations::Next),
    shortcut!("enter", LOCATIONS_LIST, locations::Open),
    shortcut!("escape", LOCATION_DIALOG, locations::Close),
    shortcut!("enter", LOCATION_DIALOG, locations::Confirm),
];

/// `Ctrl K`, the palette's key (module docs).
pub const SEARCH_KEYS: &str = "secondary-k";
/// `Ctrl ,`, Settings' key (module docs).
pub const SETTINGS_KEYS: &str = "secondary-,";

/// The keys every Mac app has (DESIGN §5, "macOS"), bound on macOS only, anywhere in the window:
/// the app menu shows them (`crate::app_menu`).
pub const MACOS_SHORTCUTS: &[Shortcut] = &[
    shortcut!("cmd-q", SHELL, app::Quit),
    shortcut!("cmd-h", SHELL, app::Hide),
    shortcut!("alt-cmd-h", SHELL, app::HideOthers),
    shortcut!("cmd-m", SHELL, app::Minimize),
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
    }
}
