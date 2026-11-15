//! scrollback's automation host (PLAN §12): the `state` document e2e scenarios assert on, the
//! `settled` predicate and scrollback's own commands. Built only with the `automation` feature
//! and started only with `--automation` / `--automation-pipe`. toolbox-rs's docs/automation.md
//! documents the protocol and the builtin commands; `gpui_automation` adds `window` and
//! `windows` to `state` itself.
//!
//! A scripted run never reaches a real WSL distro or SSH host ([`StandIns`]): a distro is
//! read from the home `--automation-wsl-home` names, or is unreachable without one, and a host
//! is unreachable.
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
//!   `hold_sync`'s hold), and `locations`, each with its `name`, `kind` (`local`, `wsl` or
//!   `ssh`) and `state` (`syncing` with `progress`: the `items` to fetch and `items_done`, the
//!   `sessions` to index and `sessions_done`; `synced`; `failed` with `error`; `unreachable`;
//!   or `never`).
//! - `library`: `shelf` (`all`, `starred`, `unsorted`, or `{"folder": name}`), `counts` (`all`,
//!   `starred`, `unsorted`), `folders` (each with its `name`, its `parent`'s name and its
//!   `sessions`, in the catalogue's order), `sessions` and `groups` (how many the shown page
//!   lists, `null` while it loads or is of the other kind), the titles of the `selected` and
//!   the `open` session, and `error` (why the library couldn't be read, or `null`).
//! - `reader`: `state` (`closed`, `opening`, `failed` with `error`, or `open`), `full_calls`,
//!   `place` (`[n, of]`: the open session among its page's), `rows` (the column's), `top` (the
//!   row at its top), `focused` (`row` and `kind` of the keyboard's row, or `null`),
//!   `open_calls`, `markdown_states` (how many parsed bodies are kept), `alone` (a subagent read
//!   on its own), `loads` (`bodies`, `subagents`, `summaries`, `images`: how many are `loading`
//!   and `loaded`) and `fault` (a read that failed for a reason other than the stored bytes).
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

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use gpui::{App, AppContext as _, Context, Entity, Window};
use gpui_appearance::{Appearance, Material, Mode};
use gpui_automation::AutomationHost;
use sb_core::CancelToken;
use sb_core::catalogue::{SessionId, Shelf};
use sb_core::location::LocationKind;
use sb_core::transport::{
    Env, FileStat, Hashed, Listing, LocalTransport, PrefixRequest, RangeFailure, RangeRequest,
    RangeSink, Transport, TransportError,
};
use sb_ui::Shell;
use sb_ui::services::{ActiveServices as _, ArchiveState, Held, LocationStatus, Page, ReaderState};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cli::Cli;

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
            "reader": reader(shell, cx),
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
                LocationStatus::Unreachable { .. } => ("unreachable", None, None),
                LocationStatus::NeverSynced => ("never", None, None),
            };
            json!({
                "name": location.name,
                "kind": match location.kind {
                    LocationKind::Local => "local",
                    LocationKind::Wsl { .. } => "wsl",
                    LocationKind::Ssh { .. } => "ssh",
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

/// `state.reader`.
fn reader(shell: &Shell, cx: &App) -> Value {
    let reader = cx.services().reader.read(cx);
    let view = shell.reader(cx);
    let open = reader.open_session();
    json!({
        "state": reader.state().key(),
        "error": match reader.state() {
            ReaderState::Failed { why, .. } => Some(why),
            _ => None,
        },
        "full_calls": reader.full_calls(),
        "place": cx.services().library.read(cx).open_place().map(|(at, of)| json!([at + 1, of])),
        "rows": view.rows,
        "top": view.top,
        "focused": view.focused.map(|(at, kind)| json!({ "row": at, "kind": kind })),
        "open_calls": view.open_calls,
        "markdown_states": view.states,
        "alone": view.alone,
        "loads": open.map(|open| {
            open.loads()
                .into_iter()
                .map(|(name, loading, loaded)| {
                    (name.to_string(), json!({ "loading": loading, "loaded": loaded }))
                })
                .collect::<serde_json::Map<_, _>>()
        }),
        "fault": open.and_then(|open| open.fault()),
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

/**
 * How a scripted run reaches its locations: never the distro or the host a location names,
 * which would be the machine's own and not the scenario's. This machine is read as it is, and
 * the runner points it at the scenario's home. A WSL distro is read from the home
 * `--automation-wsl-home` names, and from nowhere without one: read as this machine it would
 * archive this machine's transcripts under the distro's name. A host over SSH can't be
 * reached, which is how a scenario shows an unreachable location.
 */
pub struct StandIns {
    wsl_home: Option<PathBuf>,
}

/// The stand-ins of a run with the automation driver; `None` for any other run, which reaches
/// its locations as their kinds say.
pub fn stand_ins(cli: &Cli) -> Option<StandIns> {
    cli.automation_enabled().then(|| StandIns {
        wsl_home: cli.automation_wsl_home.clone(),
    })
}

impl StandIns {
    pub fn connect(&self, kind: &LocationKind, _cancel: &CancelToken) -> Box<dyn Transport> {
        match (kind, &self.wsl_home) {
            (LocationKind::Local, _) => Box::new(LocalTransport),
            (LocationKind::Wsl { .. }, Some(home)) => Box::new(StandInDistro {
                home: home.display().to_string(),
            }),
            (LocationKind::Wsl { .. }, None) => Box::new(Nowhere("no distro")),
            (LocationKind::Ssh { .. }, _) => Box::new(Nowhere("no host")),
        }
    }
}

/// A WSL distro played by a folder of this machine: its files are this machine's, its home
/// that folder, and none of this machine's environment is its.
struct StandInDistro {
    home: String,
}

impl Transport for StandInDistro {
    fn env(&self, _: &[&str]) -> Result<Env, TransportError> {
        Ok(Env {
            home: self.home.clone(),
            vars: HashMap::new(),
        })
    }

    fn stat(&self, paths: &[&str]) -> Result<Vec<Option<FileStat>>, TransportError> {
        LocalTransport.stat(paths)
    }

    fn list(&self, root: &str) -> Result<Listing, TransportError> {
        LocalTransport.list(root)
    }

    fn hash_prefixes(
        &self,
        requests: &[PrefixRequest],
    ) -> Result<Vec<Result<Hashed, RangeFailure>>, TransportError> {
        LocalTransport.hash_prefixes(requests)
    }

    fn read_ranges(
        &self,
        requests: &[RangeRequest],
        sink: &mut dyn RangeSink,
    ) -> Result<(), TransportError> {
        LocalTransport.read_ranges(requests, sink)
    }

    fn read_small(&self, path: &str) -> Result<Option<Vec<u8>>, TransportError> {
        LocalTransport.read_small(path)
    }
}

/// A location that is never there: what a scripted run reaches, named.
struct Nowhere(&'static str);

impl Nowhere {
    fn unreachable<T>(&self) -> Result<T, TransportError> {
        Err(TransportError::Unreachable(format!(
            "a scripted run reaches {}",
            self.0
        )))
    }
}

impl Transport for Nowhere {
    fn env(&self, _: &[&str]) -> Result<Env, TransportError> {
        self.unreachable()
    }

    fn stat(&self, _: &[&str]) -> Result<Vec<Option<FileStat>>, TransportError> {
        self.unreachable()
    }

    fn list(&self, _: &str) -> Result<Listing, TransportError> {
        self.unreachable()
    }

    fn hash_prefixes(
        &self,
        _: &[PrefixRequest],
    ) -> Result<Vec<Result<Hashed, RangeFailure>>, TransportError> {
        self.unreachable()
    }

    fn read_ranges(&self, _: &[RangeRequest], _: &mut dyn RangeSink) -> Result<(), TransportError> {
        self.unreachable()
    }

    fn read_small(&self, _: &str) -> Result<Option<Vec<u8>>, TransportError> {
        self.unreachable()
    }
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

    fn cli(args: &[&str]) -> Cli {
        use clap::Parser as _;
        Cli::try_parse_from(std::iter::once("scrollback").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn only_a_run_with_the_driver_reaches_its_locations_through_the_stand_ins() {
        assert!(stand_ins(&cli(&[])).is_none());
        assert!(stand_ins(&cli(&["--automation"])).is_some());
        assert!(stand_ins(&cli(&["--automation-pipe", "e2e"])).is_some());
    }

    #[test]
    fn a_distro_is_read_from_the_home_named_for_it_and_from_nowhere_else() {
        let distro = LocationKind::Wsl {
            distro: "Ubuntu-24.04".to_string(),
        };
        let cancel = CancelToken::new();
        let unnamed = stand_ins(&cli(&["--automation"])).unwrap();
        assert!(matches!(
            unnamed.connect(&distro, &cancel).env(&["HOME"]),
            Err(TransportError::Unreachable(_))
        ));
        let home = std::env::temp_dir().join("stand-in-home");
        let named = stand_ins(&cli(&[
            "--automation",
            "--automation-wsl-home",
            home.to_str().unwrap(),
        ]))
        .unwrap();
        let env = named.connect(&distro, &cancel).env(&["HOME"]).unwrap();
        assert_eq!(env.home, home.display().to_string());
        assert!(env.vars.is_empty(), "none of this machine's environment");
        // A host is never reached, home or not.
        let host = LocationKind::Ssh {
            host: "user@macbook".to_string(),
        };
        assert!(matches!(
            named.connect(&host, &cancel).env(&[]),
            Err(TransportError::Unreachable(_))
        ));
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
