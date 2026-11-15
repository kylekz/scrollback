//! scrollback's automation host (PLAN §12): the `state` document e2e scenarios assert on, the
//! `settled` predicate and scrollback's own commands. Built only with the `automation` feature
//! and started only with `--automation` / `--automation-pipe`. toolbox-rs's docs/automation.md
//! documents the protocol and the builtin commands; `gpui_automation` adds `window` and
//! `windows` to `state` itself.
//!
//! `state`:
//! - `app`: `"scrollback"`.
//! - `data_dir`: where this run keeps its files (`crate::data_dir`).
//! - `appearance`: `system` (the OS's `light` or `dark`), `mode` (`system`, `light`, `dark`)
//!   and `material` (`mica`, `acrylic`, `vibrancy`, `opaque`) as asked for, `pinned` (the
//!   `pin_appearance` override, or `null`), `available` (the materials the platform can show
//!   now) and `resolved` (`appearance` and `material`: what the theme draws).
//! - `shell`: `sidebar` (`docked`, `collapsed` or `overlay`, DESIGN §2), `window_material`
//!   (what was applied to the window) and `window_native` (macOS: what AppKit reports, `forced`
//!   light or dark or `null`, `opaque` and `vibrancy_view`; `null` elsewhere).
//!
//! Commands, each replying with `state.appearance` after the change. An argument a command
//! doesn't have is an error, so a misspelt one never passes for a change:
//! - `pin_appearance {appearance}` pins `light` or `dark` whatever the mode and the OS say, so
//!   a scenario looks the same on every machine; `null` lifts the pin.
//! - `set_appearance {mode?, material?}` changes what scrollback asks for, as Settings ›
//!   Appearance will (DESIGN §4.13).

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use gpui::{App, AppContext as _, Context, Entity, Window};
use gpui_appearance::{Appearance, Material, Mode};
use gpui_automation::AutomationHost;
use sb_ui::Shell;
use serde::Deserialize;
use serde_json::{Value, json};

/// The entity `gpui_automation` drives: the shell it describes.
pub struct ScrollbackAutomation {
    shell: Entity<Shell>,
    data_dir: PathBuf,
}

impl AutomationHost for ScrollbackAutomation {
    fn state(&self, window: &Window, cx: &App) -> Value {
        let shell = self.shell.read(cx);
        json!({
            "app": "scrollback",
            "data_dir": self.data_dir.display().to_string(),
            "appearance": appearance(cx),
            "shell": {
                "sidebar": shell.sidebar_placement(window.viewport_size().width, cx).key(),
                "window_material": shell.applied_material().key(),
                "window_native": gpui_appearance::native(window).map(|native| json!({
                    "forced": native.forced.map(Appearance::key),
                    "opaque": native.opaque,
                    "vibrancy_view": native.vibrancy_view,
                })),
            },
        })
    }

    /// Nothing in the shell runs in the background yet.
    fn settled(&self, _cx: &App) -> bool {
        true
    }

    fn commands(&self) -> &'static [&'static str] {
        &["pin_appearance", "set_appearance"]
    }

    fn custom(
        &mut self,
        cmd: &str,
        args: Value,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Result<Value>> {
        let result = match cmd {
            "pin_appearance" => serde_json::from_value(args)
                .map(|PinAppearance { appearance }| gpui_appearance::pin(appearance, cx)),
            "set_appearance" => {
                serde_json::from_value(args).map(|SetAppearance { mode, material }| {
                    let mut preferences = gpui_appearance::state(cx).preferences();
                    preferences.mode = mode.unwrap_or(preferences.mode);
                    preferences.material = material.unwrap_or(preferences.material);
                    gpui_appearance::set_preferences(preferences, cx);
                })
            }
            _ => return None,
        };
        Some(
            result
                .map(|()| appearance(cx))
                .with_context(|| format!("{cmd}'s arguments")),
        )
    }
}

/// `state.appearance`.
fn appearance(cx: &App) -> Value {
    let state = gpui_appearance::state(cx);
    let preferences = state.preferences();
    let resolved = state.resolved();
    json!({
        "system": state.system().key(),
        "mode": preferences.mode.key(),
        "material": preferences.material.key(),
        "pinned": state.pinned().map(Appearance::key),
        "available": state
            .support()
            .available()
            .into_iter()
            .map(Material::key)
            .collect::<Vec<_>>(),
        "resolved": {
            "appearance": resolved.appearance.key(),
            "material": resolved.material.key(),
        },
    })
}

/// `pin_appearance {"appearance": "light" | "dark" | null}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PinAppearance {
    /// `null` lifts the pin. `deserialize_with` makes the key required, where serde would read
    /// a missing `Option` as `None`: a command without it must not lift the pin.
    #[serde(deserialize_with = "Option::deserialize")]
    appearance: Option<Appearance>,
}

/// `set_appearance {mode?, material?}`: what is given replaces the current value.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetAppearance {
    mode: Option<Mode>,
    material: Option<Material>,
}

/// Starts automation on the main window.
pub fn start(
    window: gpui::AnyWindowHandle,
    shell: Entity<Shell>,
    data_dir: PathBuf,
    pipe: Option<String>,
    cx: &mut App,
) -> Result<()> {
    let host = cx.new(|_| ScrollbackAutomation { shell, data_dir });
    let options = gpui_automation::Options {
        pipe,
        ..Default::default()
    };
    gpui_automation::start(window, host, cx, options)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn pin_appearance_takes_one_required_argument() {
        let parse = |args: Value| serde_json::from_value::<PinAppearance>(args);
        assert_eq!(
            parse(json!({"appearance": "dark"})).unwrap().appearance,
            Some(Appearance::Dark)
        );
        assert_eq!(parse(json!({"appearance": null})).unwrap().appearance, None);
        // A misspelt or missing argument must not lift the pin.
        let misspelt = parse(json!({"apperance": "dark"})).unwrap_err().to_string();
        assert!(misspelt.contains("unknown field `apperance`"), "{misspelt}");
        assert!(parse(json!({})).is_err());
        assert!(parse(json!({"appearance": "sepia"})).is_err());
    }

    #[test]
    fn set_appearance_takes_only_a_mode_and_a_material() {
        let parse = |args: Value| serde_json::from_value::<SetAppearance>(args);
        let both = parse(json!({"mode": "light", "material": "mica"})).unwrap();
        assert_eq!(
            (both.mode, both.material),
            (Some(Mode::Light), Some(Material::Mica))
        );
        let neither = parse(json!({})).unwrap();
        assert_eq!((neither.mode, neither.material), (None, None));
        assert!(parse(json!({"materail": "mica"})).is_err());
        assert!(parse(json!({"mode": "sepia"})).is_err());
    }
}
