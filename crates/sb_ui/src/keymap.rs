//! The keymap (DESIGN §5). One table, [`SHORTCUTS`], drives both the gpui bindings and the
//! keycaps views draw ([`label`]), so the two can't drift.
//!
//! `secondary` is gpui's platform modifier: `Ctrl` on Windows, `Cmd` on macOS (DESIGN §5:
//! "macOS uses Cmd for every Ctrl").

use gpui::{Action, App, KeyBinding, KeyBindingContextPredicate, SharedString};

use crate::actions::{app, shell};

/// Key context of the shell's root element: everything in the main window.
pub const SHELL: &str = "Shell";

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

/// Every binding scrollback defines.
pub const SHORTCUTS: &[Shortcut] = &[
    shortcut!("secondary-b", SHELL, shell::ToggleSidebar),
    shortcut!("escape", SHELL, shell::Dismiss),
];

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
