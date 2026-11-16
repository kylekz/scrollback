//! The app's commands (DESIGN §5: "every action is also a command in the palette"; PLAN §11):
//! what the palette's Commands group offers when its query names one (DESIGN §4.9). Each runs
//! a row of the keymap ([`crate::keymap`]) and shows that row's keycap, or shows a page of the
//! library, which has no key.
//!
//! **It can't drift from the keymap.** A command holds the keymap's own row, and a test checks
//! that every row of the shell's and the pages' contexts is a command or is listed as moving
//! the keyboard, which no command does ([`NOT_COMMANDS`]).

use gpui::{App, SharedString};
use sb_core::catalogue::Shelf;

use crate::keymap::{self, Shortcut};

/// What a command does.
#[derive(Clone, Copy, Debug)]
pub enum Run {
    /// What its keymap row's key does, where the keyboard rests in the main area.
    Key(Shortcut),
    /// Shows a page of the library.
    Show(Shelf),
}

/// One command: what the palette calls it, and what it does.
#[derive(Clone, Copy, Debug)]
pub struct Command {
    pub name: &'static str,
    pub run: Run,
}

impl Command {
    /// The keycap the palette shows beside it, for a command that has a key.
    pub fn keys(&self) -> Option<SharedString> {
        match self.run {
            Run::Key(shortcut) => Some(shortcut.label()),
            Run::Show(_) => None,
        }
    }

    /**
     * Whether it is what `terms` ask for: each term starts a word of its name, whatever the
     * case, so "sync", "trash" and "new fol" find "Sync all locations", "Move to Trash" and
     * "New folder".
     */
    pub fn matches(&self, terms: &[String]) -> bool {
        let words: Vec<String> = self
            .name
            .split(|char: char| !char.is_alphanumeric())
            .map(str::to_lowercase)
            .collect();
        !terms.is_empty()
            && terms.iter().all(|term| {
                let term = term.to_lowercase();
                words.iter().any(|word| word.starts_with(&term))
            })
    }
}

/// Every command, in the order the palette lists those that match.
pub const COMMANDS: &[Command] = &[
    Command {
        name: "Sync all locations",
        run: Run::Key(keymap::SYNC_ALL),
    },
    Command {
        name: "Show the locations",
        run: Run::Key(keymap::LOCATIONS),
    },
    Command {
        name: "New folder",
        run: Run::Key(keymap::NEW_FOLDER),
    },
    Command {
        name: "Rename the folder shown",
        run: Run::Key(keymap::RENAME),
    },
    Command {
        name: "Show All sessions",
        run: Run::Show(Shelf::All),
    },
    Command {
        name: "Show Starred",
        run: Run::Show(Shelf::Starred),
    },
    Command {
        name: "Show Unsorted",
        run: Run::Show(Shelf::Unsorted),
    },
    Command {
        name: "Show Trash",
        run: Run::Show(Shelf::Trash),
    },
    Command {
        name: "Open the selected session",
        run: Run::Key(keymap::OPEN),
    },
    Command {
        name: "Star or unstar the selected sessions",
        run: Run::Key(keymap::STAR),
    },
    Command {
        name: "Move the selected sessions to a folder",
        run: Run::Key(keymap::MOVE),
    },
    Command {
        name: "Move the selected sessions to Trash",
        run: Run::Key(keymap::TRASH),
    },
    Command {
        name: "Select every session shown",
        run: Run::Key(keymap::SELECT_ALL),
    },
    Command {
        name: "Open the selected session's menu",
        run: Run::Key(keymap::MENU_KEY),
    },
    Command {
        name: "Back to the page",
        run: Run::Key(keymap::BACK),
    },
    Command {
        name: "Show or hide the sidebar",
        run: Run::Key(keymap::TOGGLE_SIDEBAR),
    },
    Command {
        name: "Move the keyboard between the sidebar and the page",
        run: Run::Key(keymap::FOCUS_OTHER),
    },
];

/**
 * The rows of the shell's and the pages' contexts that are no command: the palette's own key,
 * `Esc`, and the arrows, which move over what has the keyboard and so mean nothing from the
 * palette. Each is its keys and its context.
 */
pub const NOT_COMMANDS: &[(&str, &str)] = &[
    ("secondary-k", keymap::SHELL),
    ("escape", keymap::SHELL),
    ("left", keymap::SESSIONS),
    ("right", keymap::SESSIONS),
    ("up", keymap::SESSIONS),
    ("down", keymap::SESSIONS),
];

/// The commands whose names `terms` ask for.
pub fn matching(terms: &[String]) -> Vec<&'static Command> {
    COMMANDS
        .iter()
        .filter(|command| command.matches(terms))
        .collect()
}

/// Runs what `run` does: the key's action where the keyboard rests in the window, which the
/// caller has put on the main area, or the page shown.
pub fn run(run: Run, window: &mut gpui::Window, cx: &mut App) {
    match run {
        Run::Key(shortcut) => window.dispatch_action((shortcut.action)(), cx),
        Run::Show(shelf) => {
            let library = crate::services::ActiveServices::services(cx)
                .library
                .clone();
            library.update(cx, |library, cx| library.show(shelf, cx));
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn terms(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_string).collect()
    }

    fn names(text: &str) -> Vec<&'static str> {
        matching(&terms(text)).iter().map(|it| it.name).collect()
    }

    #[test]
    fn every_key_of_the_shell_and_the_pages_is_a_command() {
        for shortcut in keymap::SHORTCUTS {
            if ![keymap::SHELL, keymap::SESSIONS].contains(&shortcut.context) {
                continue;
            }
            let named = COMMANDS.iter().any(|command| match command.run {
                Run::Key(it) => (it.keys, it.context) == (shortcut.keys, shortcut.context),
                Run::Show(_) => false,
            });
            let excused = NOT_COMMANDS.contains(&(shortcut.keys, shortcut.context));
            assert!(
                named != excused,
                "{shortcut:?} is {}",
                if named {
                    "a command and excused"
                } else {
                    "neither a command nor excused"
                }
            );
        }
    }

    #[test]
    fn a_command_is_found_by_the_starts_of_its_words() {
        assert_eq!(names("sync"), ["Sync all locations"]);
        assert_eq!(
            names("trash"),
            ["Show Trash", "Move the selected sessions to Trash"]
        );
        assert_eq!(names("NEW fol"), ["New folder"]);
        assert_eq!(names("rash"), Vec::<&str>::new());
        assert_eq!(names(""), Vec::<&str>::new());
    }
}
