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
//! - `page`: what the main area shows: `starting`, `archive-failed`, `locations-failed`,
//!   `first-run`, `sessions` (All sessions, Starred or a folder), `unsorted` or `session` (an
//!   opened session).
//! - `archive`: `dir`, and `state` (`opening`, `open` or `failed`, then with `error`).
//! - `sync`: `syncing`, `indexing` (an index runs outside a pass), `held` (a pass waits at
//!   `hold_sync`'s hold), and `locations`, each with its `name`, `kind` and `state` (`syncing`
//!   with `progress`: the `items` to fetch and `items_done`, the `sessions` to index and
//!   `sessions_done`; `synced`; `failed` with `error`; or `never`).
//! - `library`: `shelf` (`all`, `starred`, `unsorted`, or `{"folder": name}`), `counts` (`all`,
//!   `starred`, `unsorted`), `folders` (each with its `name`, its `parent`'s name and its
//!   `sessions`, in the catalogue's order), `sessions` and `groups` (how many the shown page
//!   lists, `null` while it loads or is of the other kind), the titles of the `selected` and
//!   the `open` session, and `error` (why the library couldn't be read, or `null`).
//!
//! Commands. An argument a command doesn't have is an error, so a misspelt one never passes
//! for a change. These reply with `state.appearance` after the change:
//! - `pin_appearance {appearance}` pins `light` or `dark` whatever the mode and the OS say, so
//!   a scenario looks the same on every machine; `null` lifts the pin.
//! - `set_appearance {mode?, material?}` changes what scrollback asks for, as Settings ›
//!   Appearance will (DESIGN §4.13).
//!
//! And these with `state.sync`, for a scenario that needs a pass in a known state whatever
//! the machine's speed:
//! - `hold_sync {after}` makes the next pass wait once it has stored `after` items. The app
//!   is not settled while a pass waits: wait for `/sync/held` instead.
//! - `release_sync {}` lets the waiting pass go on, and clears a hold no pass has reached.
//!   Stopping the pass does too.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use gpui::{App, AppContext as _, Context, Entity, Window};
use gpui_appearance::{Appearance, Material, Mode};
use gpui_automation::AutomationHost;
use sb_core::catalogue::{SessionId, Shelf};
use sb_core::location::LocationKind;
use sb_ui::Shell;
use sb_ui::services::{ActiveServices as _, ArchiveState, Held, LocationStatus, Page};
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
            "page": shell.showing(cx).key(),
            "archive": archive(cx),
            "sync": sync(cx),
            "library": library(cx),
        })
    }

    /// The archive is open or has failed to, and nothing is syncing, indexing or loading.
    fn settled(&self, cx: &App) -> bool {
        cx.services().is_settled(cx)
    }

    fn commands(&self) -> &'static [&'static str] {
        &[
            "pin_appearance",
            "set_appearance",
            "hold_sync",
            "release_sync",
        ]
    }

    fn custom(
        &mut self,
        cmd: &str,
        args: Value,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Result<Value>> {
        let service = cx.services().sync.clone();
        let result = match cmd {
            "pin_appearance" => serde_json::from_value(args).map(|PinAppearance { appearance }| {
                gpui_appearance::pin(appearance, cx);
                self::appearance(cx)
            }),
            "set_appearance" => {
                serde_json::from_value(args).map(|SetAppearance { mode, material }| {
                    let mut preferences = gpui_appearance::state(cx).preferences();
                    preferences.mode = mode.unwrap_or(preferences.mode);
                    preferences.material = material.unwrap_or(preferences.material);
                    gpui_appearance::set_preferences(preferences, cx);
                    self::appearance(cx)
                })
            }
            "hold_sync" => serde_json::from_value(args).map(|HoldSync { after }| {
                service.update(cx, |sync, _| sync.hold_after(after, Held::Pause));
                sync(cx)
            }),
            "release_sync" => serde_json::from_value(args).map(|ReleaseSync {}| {
                service.update(cx, |sync, _| sync.release());
                sync(cx)
            }),
            _ => return None,
        };
        Some(result.with_context(|| format!("{cmd}'s arguments")))
    }
}

/// `state.archive`.
fn archive(cx: &App) -> Value {
    let archive = cx.services().archive.read(cx);
    let (state, error) = match archive.state() {
        ArchiveState::Opening => ("opening", None),
        ArchiveState::Open(_) => ("open", None),
        ArchiveState::Failed(failure) => ("failed", Some(&failure.why)),
    };
    json!({ "dir": archive.dir().display().to_string(), "state": state, "error": error })
}

/// `state.sync`.
fn sync(cx: &App) -> Value {
    let sync = cx.services().sync.read(cx);
    let locations: Vec<Value> = sync
        .locations()
        .unwrap_or_default()
        .iter()
        .map(|location| {
            let (state, error, progress) = match sync.status(location) {
                LocationStatus::Syncing { progress, .. } => ("syncing", None, Some(progress)),
                LocationStatus::Synced(_) => ("synced", None, None),
                LocationStatus::Failed(error) => ("failed", Some(error), None),
                LocationStatus::NeverSynced => ("never", None, None),
            };
            json!({
                "name": location.name,
                "kind": match location.kind {
                    LocationKind::Local => "local",
                },
                "state": state,
                "error": error,
                "progress": progress.map(|progress| json!({
                    "items": progress.items,
                    "items_done": progress.items_done,
                    "sessions": progress.sessions,
                    "sessions_done": progress.sessions_done,
                })),
            })
        })
        .collect();
    json!({
        "syncing": sync.is_syncing(),
        "indexing": sync.is_indexing(),
        "held": sync.is_held(),
        "locations": locations,
    })
}

/// `state.library`.
fn library(cx: &App) -> Value {
    let library = cx.services().library.read(cx);
    let name = |folder| library.folder(folder).map(|folder| folder.name.clone());
    let counts = library.counts();
    let folders: Vec<Value> = library
        .folders()
        .iter()
        .map(|folder| {
            json!({
                "name": folder.name,
                "parent": folder.parent.and_then(name),
                "sessions": counts.folders.get(&folder.id),
            })
        })
        .collect();
    let (sessions, groups) = match library.page() {
        Some(Page::Sessions(page)) => (Some(page.sessions.len()), None),
        Some(Page::Unsorted(groups)) => (None, Some(groups.len())),
        None => (None, None),
    };
    // The selected session's title: on a page of sessions by its place, in Unsorted by its id.
    let title_of = |id: &SessionId| match library.page()? {
        Page::Sessions(page) => page.sessions.get(library.selected_index()?)?.title.clone(),
        Page::Unsorted(groups) => groups
            .iter()
            .flat_map(|group| &group.sessions)
            .find(|session| session.id == *id)?
            .title
            .clone(),
    };
    json!({
        "shelf": match library.shelf() {
            Shelf::All => json!("all"),
            Shelf::Starred => json!("starred"),
            Shelf::Unsorted => json!("unsorted"),
            Shelf::Folder(folder) => json!({ "folder": name(folder) }),
        },
        "counts": {
            "all": counts.all,
            "starred": counts.starred,
            "unsorted": counts.unsorted,
        },
        "folders": folders,
        "sessions": sessions,
        "groups": groups,
        "selected": library.selected().and_then(title_of),
        "open": library.open_session().and_then(|session| session.title.clone()),
        "error": library.failure(),
    })
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

/// `hold_sync {"after": items}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HoldSync {
    after: usize,
}

/// `release_sync {}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseSync {}

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

    #[test]
    fn a_hold_needs_how_many_items_and_a_release_takes_nothing() {
        let hold = |args: Value| serde_json::from_value::<HoldSync>(args);
        assert_eq!(hold(json!({"after": 40})).unwrap().after, 40);
        assert!(hold(json!({})).is_err());
        assert!(hold(json!({"after": -1})).is_err());
        assert!(hold(json!({"after": 40, "then": "stop"})).is_err());
        let release = |args: Value| serde_json::from_value::<ReleaseSync>(args);
        assert!(release(json!({})).is_ok());
        assert!(release(json!({"after": 1})).is_err());
    }
}
