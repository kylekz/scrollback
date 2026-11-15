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
//! Commands, each replying with `state.appearance` after the change:
//! - `pin_appearance {appearance}` pins `light` or `dark` whatever the mode and the OS say, so
//!   a scenario looks the same on every machine; `null` lifts the pin.
//! - `set_appearance {mode?, material?}` changes what scrollback asks for, as Settings ›
//!   Appearance will (DESIGN §4.13).

use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow, bail};
use gpui::{App, AppContext as _, Context, Entity, Window};
use gpui_appearance::{Appearance, Material, Mode};
use gpui_automation::AutomationHost;
use sb_ui::Shell;
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
            "pin_appearance" => pin_appearance(&args, cx),
            "set_appearance" => set_appearance(&args, cx),
            _ => return None,
        };
        Some(result.map(|()| appearance(cx)))
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
fn pin_appearance(args: &Value, cx: &mut App) -> Result<()> {
    let appearance = match args.get("appearance") {
        None | Some(Value::Null) => None,
        Some(Value::String(key)) => Some(
            Appearance::from_key(key)
                .ok_or_else(|| anyhow!("appearance must be light, dark or null, not {key:?}"))?,
        ),
        Some(other) => bail!("appearance must be light, dark or null, not {other}"),
    };
    gpui_appearance::pin(appearance, cx);
    Ok(())
}

/// `set_appearance {mode?, material?}`: what is given replaces the current value.
fn set_appearance(args: &Value, cx: &mut App) -> Result<()> {
    let string = |name: &str| -> Result<Option<&str>> {
        match args.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_str()
                .map(Some)
                .with_context(|| format!("set_appearance's {name:?} must be a string")),
        }
    };
    let mut preferences = gpui_appearance::state(cx).preferences();
    if let Some(mode) = string("mode")? {
        preferences.mode = Mode::from_key(mode)
            .ok_or_else(|| anyhow!("mode must be system, light or dark, not {mode:?}"))?;
    }
    if let Some(material) = string("material")? {
        preferences.material = Material::from_key(material).ok_or_else(|| {
            anyhow!("material must be mica, acrylic, vibrancy or opaque, not {material:?}")
        })?;
    }
    gpui_appearance::set_preferences(preferences, cx);
    Ok(())
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
