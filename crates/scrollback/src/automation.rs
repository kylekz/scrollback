//! scrollback's automation host (PLAN §12): the `state` document e2e scenarios assert on, the
//! `settled` predicate and scrollback's own commands. Built only with the `automation` feature
//! and started only with `--automation` / `--automation-pipe`. toolbox-rs's docs/automation.md
//! documents the protocol and the builtin commands; `gpui_automation` adds `window` and
//! `windows` to `state` itself.
//!
//! A scripted run never reaches a real WSL distro or SSH host, nor runs `wsl.exe` or `ssh`
//! ([`stand_ins`]): a distro or a host that answers is read from the folder
//! `--automation-wsl-home` names (and can't be reached without one), a host the scenario has
//! not named is unreachable, and WSL is not there until a scenario lists distros. Should
//! anything still try to start a launcher, it is refused and logged as an error
//! (`sb_core::transport::refuse_launchers`), and the e2e runner fails the scenario.
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
//!   `first-run`, `settings`, `sessions` (All sessions, Starred or a folder), `unsorted`, `trash`
//!   or `session` (an opened session).
//! - `settings` (DESIGN §4.13): `section` (the one picked in Settings' list: `archive`, `sync`,
//!   `appearance` or `shortcuts`), `archive` (its folder), `default` (whether that is the data
//!   dir's), `switch` (`null`, `{"opening": folder}` or `{"refused": why}`), `sync_every`
//!   (`5m`, `15m`, `30m`, `60m` or `asked`), `next_round` (whether a round is due),
//!   `stale_after` (days), `stats` (`sessions`, `items`, `versions`, or `null` before they are
//!   measured), `verify` (`running` with its `progress`, the `last` one's `segments` and
//!   `problems`, each with its `file` and `fix`: `fetch_again`, `fetched`, `damaged` or
//!   `deleted`, and `refetch`: `null`, `running`, `done`, `skipped: …` or the failure's words;
//!   and `fetch_all`, Fetch all again's `done` and `total` while it runs, or `null`), `deleted`
//!   (each session deleted for good's `location`, whether it is `allowed` again, and its
//!   `standing`: `deleted`, `allowed` or `archived_again`; `null` before the archive is read),
//!   and `saved` (why the settings
//!   aren't being saved, or `null`).
//! - `system`: what the app asked of the operating system (`sb_ui::system`), never done in a
//!   scripted run, oldest first: each request's `kind` (`reveal` with its `path`,
//!   `pick_folder`, `open_url` with its `url`, `copy` with the `text` copied).
//! - `archive`: `dir`, and `state` (`opening`, `open` or `failed`, then with `error` and
//!   `failure`: `missing`, `empty`, `locked`, `settings` or `other`).
//! - `sync`: `syncing`, `indexing` (an index runs outside a pass), `held` (a pass waits at
//!   `hold_sync`'s hold), and `locations`, each with its `name`, `kind` (`local`, `wsl` or
//!   `ssh`) and `state` (`syncing` with `progress`: the `items` to fetch and `items_done`, the
//!   `sessions` to index and `sessions_done`; `synced`; `failed` with `error`; `unreachable`;
//!   or `never`), `stale` and `removing`; `removed` (the names of the locations the owner
//!   removed); `refused` (why the owner's last change to them could not be made, or `null`);
//!   `distros` (WSL's distros' names as listed, `null` without WSL or before they are
//!   listed, or `{"error": …}`); and `test`, the connection test (`null`, or its `place` and
//!   `state`: `running`, `passed`, or the failure: `unreachable`, `refused`, `host_key`,
//!   `not_installed`, `missing`, `login_hung`, `other`). A location's `state` may also be
//!   `stopped`: the owner stopped its last pass before it was done.
//! - `sheet`: the locations sheet while it is open (`null` otherwise): the name of its
//!   `selected` location, and its `dialog` (`add`, `remove` or `null`).
//! - `library`: `shelf` (`all`, `starred`, `unsorted`, `trash`, or `{"folder": name}`), `counts`
//!   (`all`, `starred`, `unsorted`, `trash`), `folders` (each with its `name`, its `parent`'s name and its
//!   `sessions`, in the catalogue's order), `sessions` and `groups` (how many the shown page
//!   lists, `null` while it loads or is of the other kind), the titles of the `selected` and
//!   the `open` session, what the tag of where the open session is from says (`open_from`: the
//!   location's name, or a copy's tag, PLAN §7.7), and `error` (why the library couldn't be
//!   read, or `null`).
//!   It also has the titles of the `selection` (the sessions selected together, or the
//!   selected one); the `drag` under way (`null`, or `what` is dragged: `{"sessions": n}`,
//!   `{"path": typed}` or `{"folder": name}`; whether it has `moved` past the 4px threshold;
//!   what it is `over`: `{"folder": name}`, `{"empty_folder": name}` (a folder's empty page),
//!   `"trash"`, `{"between": {parent, index}}` or `"sidebar"` (a narrow window's sidebar
//!   button, which shows the sidebar after 500 ms); and the `verdict` there: `takes`, with the
//!   ghost's `result` and `note`, or the `note` saying why not); the `toast` (`text` and
//!   whether it can `undo`); whether leaving, closing the window or quitting `asked` to save
//!   the paths editor's changes, and the folder `held` by them; the folder whose deletion
//!   is being asked about (`deleting`); the session being asked about deleting for good
//!   (`for_good`: `null`, or its `title`, and once read what it removes, `files`, `versions`,
//!   `bytes` and whether its location still has it, `at_location`, with why the last
//!   confirmation was `refused`); and where the open session was opened at (`read_at`: `null`,
//!   or its `request` number, the match's `kind`, its `turn` from 0, the `subagent` item it is
//!   in, the `find` text and the hit to `flash`), until the reader takes it.
//! - `palette`: the search palette while it is open (`null` otherwise): its query's `text`, the
//!   labels of its `chips`, its `completions`, how many `rows` the keys move over and the one
//!   `highlighted`, the `commands` it lists, the finished token that names nothing and so
//!   holds the results (`unresolved`), whether it is `searching`, how many sessions are
//!   `unsearchable` yet, and the `answer` to the last search: `null` with no terms, `"keep-typing"`,
//!   `{"unavailable": why}`, or the `sessions` found (their titles, the best first),
//!   `total_sessions`, `total_matches`, the query's `terms` and how long it `took_ms`.
//!
//!   A drag is driven with `drag` (`release: false` to hold it), `mouse_up`, and keys between:
//!   no key but `Esc` acts while one is held. Where it is over is read from what the last frame
//!   drew, so a press that jumps straight to a target drops there; a drag that never moved 4px
//!   is a click on what was pressed.
//! - `page_view`: how the page of sessions shows: `list`, `oldest_first`, the `filter` text,
//!   whether it is `filtered` at all, how many
//!   sessions are `shown`, and its paths `editor` (`null`, or whether it is the `short` form
//!   and the paths `added` and `removed`, as typed).
//! - `menu`: the open menu (`null`, or the names of its `items` and of its open `submenu`'s;
//!   each item is the target `menu/<name>`, a submenu's `menu/<submenu>/<name>`).
//! - `cards`: what the page of cards last decided its cards' last lines from (DESIGN §4.4):
//!   `shows_calls`, `tags_past_caps`, the window's `viewport` width and `scale` factor, `inner`
//!   (a card's width inside its padding and border), `line` (the width of the line the counts
//!   were decided on, `with_calls`, which shows them while it is no wider than `inner` less
//!   `spare`; and `without_calls`, past `inner` the tags give way past their caps), `tags`
//!   (each location's and subfolder's tag: `name`, `whole` and `capped` widths), the widest
//!   `day` and the largest `calls` (each `text` and `width`), in logical pixels.
//! - `reader`: `state` (`closed`, `opening`, `failed` with `error`, or `open`), `full_calls`,
//!   `place` (`[n, of]`: the open session among its page's), `rows` (the column's), `top` and
//!   `top_name` (the row at its top, and what it is: `kind conversation/block`) and
//!   `top_offset` (how far into that row it is scrolled), `at_end` (whether the last row is
//!   drawn in full), `following` (whether the column keeps to its end, after End), `focused`
//!   (`row`, `kind` and the start of the `text` of the keyboard's row, or `null`),
//!   `open_calls`, `markdown_states` and `held` (how many parsed bodies, and how many prepared
//!   things of the service's, are kept), `alone` (a subagent read on its own), `loads`
//!   (`bodies`, `subagents`, `prose`, `images`, `raws`, `wholes`: how many are `loading` and
//!   `loaded`), `fault` (a read that failed for a reason other than the stored bytes), the
//!   last request to open the session at a search match it took (`read_at`, as the library's),
//!   and what `flash`es: its `block` (`conversation/block`), its `text` and whether it was
//!   `drawn`; `find` (Find in session's `query`, its `count` as the field shows it, and its
//!   `current` match: `conversation/block` and which text, `prose`, `target`, `input`,
//!   `output/<n>`, `diff` or `fetched`; `null` while Find is closed), whether the `facts` show,
//!   whose `history` is open (a file's key), the version read for a look (`viewing`, its
//!   number) and whether an image shows full size (`full_image`).
//! - `update`: scrollback's own updates. `off`, with the reason, under automation
//!   (`development`, or `automation` for a packaged build): a scripted run never looks for one
//!   (`crate::updates`) until `update_ready` stands one in; then `null`. With the stand-in:
//!   the version `ready`, whether it is `checking`, the last check (`checked`: `up_to_date`,
//!   `ready` with its `version` or `failed` with its `error`), how many others Restart to
//!   update asks about (`confirm`), whether it is `restarting`, whether the stand-in took the
//!   update (`restarted`) and why a restart failed (`failure`).
//!
//! Commands. An argument a command doesn't have is an error, so a misspelt one never passes
//! for a change. These reply with `state.appearance` after the change:
//! - `pin_appearance {appearance}` pins `light` or `dark` whatever the mode and the OS say, so
//!   a scenario looks the same on every machine; `null` lifts the pin.
//! - `set_appearance {mode?, material?}` changes what scrollback asks for, as Settings ›
//!   Appearance does (DESIGN §4.13), and keeps it in the app's settings; `--theme` still wins
//!   for the run.
//!
//! And this with `state.system`:
//! - `answer_picker {folder}` answers the next folder picker with `folder`, or cancels it
//!   with `null`.
//!
//! And this with `state.update`:
//! - `update_ready {version, fail?, others?}` stands a scripted feed in for Velopack (never
//!   the network) and checks it at once: `version` is downloaded and ready; Restart to update
//!   then records the restart in `restarted`, or fails with `fail`, and asks first about
//!   `others` processes running from the install (none unless given).
//!
//! And these with `state.sync`, for a scenario that needs a pass in a known state whatever
//! the machine's speed:
//! - `hold_sync {after}` makes the next pass wait once it has stored `after` items. The app
//!   is not settled while a pass waits: wait for `/sync/held` instead.
//! - `release_sync {until?}` lets the waiting pass go on, and clears a hold no pass has
//!   reached. Stopping the pass does too. With `until`, the pass waits again once it has
//!   stored that many items, which steps it through a sync.
//! - `stand_in {distros?, places?}` sets what the stand-ins answer from then on: `distros`,
//!   WSL's installed distros (each `{name, running?, default?, utility?}`; `[]` is WSL with
//!   none) or `{"error": …}` for a `wsl.exe` that fails, which are listed again at once; and
//!   `places`, how each distro or host by its name answers a connection test and a sync:
//!   `passes` (it is read from the stand-in home), `unreachable`, `refused`, `host_key` (a key
//!   that changed), `new_host_key` (one not known yet), `missing_tools`, `no_ssh`,
//!   `login_hung` or `fails`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Context as _, Result};
use gpui::{App, AppContext as _, Context, Entity, Window};
use gpui_appearance::{Appearance, Material, Mode};
use gpui_automation::AutomationHost;
use sb_core::catalogue::{SessionId, Shelf};
use sb_core::location::LocationKind;
use sb_core::transcript::At;
use sb_core::transport::wsl::Distro;
use sb_core::transport::{
    Env, FileStat, Hashed, Launcher, Listing, LocalTransport, PrefixRequest, Probe, RangeFailure,
    RangeRequest, RangeSink, ShellTransport, Transport, TransportError,
};
use sb_ui::Shell;
use sb_ui::pages::sessions::LineMeasure;
use sb_ui::search::Palette;
use sb_ui::services::settings::Switch;
use sb_ui::services::sync::{Refetch, TestFailure, TestState};
use sb_ui::services::upkeep::{Fix, Standing};
use sb_ui::services::{
    ActiveServices as _, Answer as Searched, ArchiveState, CheckResult, Dragged, DropTarget,
    Failure, Held, LocationStatus, Page, Reach, ReadAt, ReaderState, Verdict,
};
use sb_ui::system::{self, SystemRequest};
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
            "settings": settings(shell, cx),
            "system": system::requests(cx).iter().map(|request| match request {
                SystemRequest::Reveal(path) => json!({
                    "kind": request.kind(),
                    "path": path.display().to_string(),
                }),
                SystemRequest::PickFolder { .. } => json!({ "kind": request.kind() }),
                SystemRequest::OpenUrl(url) => json!({ "kind": request.kind(), "url": url }),
                SystemRequest::Copy(text) => json!({ "kind": request.kind(), "text": text }),
            }).collect::<Vec<_>>(),
            "cards": cards(shell.main().read(cx).sessions().read(cx).line_measure()),
            "page_view": page_view(shell, cx),
            "palette": shell.palette().map(|palette| self::palette(palette.read(cx), cx)),
            "menu": sb_ui::menu::items(cx).map(|(items, submenu)| json!({
                "items": items,
                "submenu": submenu,
            })),
            "reader": reader(shell, cx),
            "sheet": shell.locations_sheet().map(|sheet| {
                let sheet = sheet.read(cx);
                let sync = cx.services().sync.read(cx);
                json!({
                    "selected": sheet.selected().and_then(|id| sync.location(id)).map(|it| &it.name),
                    "dialog": sheet.dialog(),
                })
            }),
            "update": update(cx),
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
            "stand_in",
            "answer_picker",
            "update_ready",
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
                    let settings = cx.services().settings.clone();
                    settings.update(cx, |settings, cx| {
                        if let Some(mode) = mode {
                            settings.set_mode(mode, cx);
                        }
                        if let Some(material) = material {
                            settings.set_material(material, cx);
                        }
                    });
                    self::appearance(cx)
                })
            }
            "answer_picker" => serde_json::from_value(args)
                .map_err(anyhow::Error::from)
                .and_then(|AnswerPicker { folder }| {
                    system::queue_answer(folder, cx)?;
                    Ok(json!(system::requests(cx).len()))
                })
                .map_err(|error| serde_json::Error::io(std::io::Error::other(error.to_string()))),
            "update_ready" => serde_json::from_value(args).map(
                |UpdateReady {
                     version,
                     fail,
                     others,
                 }| {
                    let updater = crate::updates::real(
                        Arc::new(ScriptedFeed {
                            version,
                            fail,
                            downloaded: Mutex::new(false),
                        }),
                        Box::new(move || others),
                        |_| {},
                    );
                    let updates = cx.services().updates.clone();
                    updates.update(cx, |updates, cx| {
                        updates.set_updater(updater, cx);
                        updates.check_now(cx);
                    });
                    update(cx)
                },
            ),
            "hold_sync" => serde_json::from_value(args).map(|HoldSync { after }| {
                service.update(cx, |sync, _| sync.hold_after(after, Held::Pause));
                sync(cx)
            }),
            "release_sync" => serde_json::from_value(args).map(|ReleaseSync { until }| {
                service.update(cx, |sync, _| match until {
                    Some(until) => sync.release_until(until),
                    None => sync.release(),
                });
                sync(cx)
            }),
            "stand_in" => serde_json::from_value(args).map(|StandIn { distros, places }| {
                stage().places.extend(places);
                if let Some(distros) = distros {
                    stage().distros = match distros {
                        Distros::Listed(listed) => Ok(Some(
                            listed
                                .into_iter()
                                .map(|it| Distro {
                                    name: it.name,
                                    running: it.running,
                                    default: it.default,
                                    utility: it.utility,
                                })
                                .collect(),
                        )),
                        Distros::Failed { error } => Err(error),
                    };
                    service.update(cx, |sync, cx| sync.list_distros(cx));
                }
                sync(cx)
            }),
            _ => return None,
        };
        Some(result.with_context(|| format!("{cmd}'s arguments")))
    }
}

/// `state.update`.
fn update(cx: &App) -> Value {
    let updates = cx.services().updates.read(cx);
    json!({
        "off": updates.off().map(|off| off.key()),
        "ready": updates.ready(),
        "checking": updates.is_checking(),
        "checked": updates.last_check().map(|check| match &check.result {
            CheckResult::UpToDate => json!({"result": "up_to_date"}),
            CheckResult::Ready(version) => json!({"result": "ready", "version": version}),
            CheckResult::Failed(error) => json!({"result": "failed", "error": error}),
        }),
        "confirm": updates.confirm(),
        "restarting": updates.is_restarting(),
        "restarted": updates.is_restarted(),
        "failure": updates.failure(),
    })
}

/**
 * `update_ready`'s feed: `version` is the newest, downloaded when asked; applying it records
 * nothing of its own (the service keeps `restarted`) or fails with `fail`. Nothing of it
 * reaches the network or the disk.
 */
struct ScriptedFeed {
    version: String,
    fail: Option<String>,
    downloaded: Mutex<bool>,
}

impl crate::updates::Releases for ScriptedFeed {
    fn pending(&self) -> Option<String> {
        (*self
            .downloaded
            .lock()
            .unwrap_or_else(PoisonError::into_inner))
        .then(|| self.version.clone())
    }

    fn newer(&self) -> Result<Option<String>, String> {
        Ok(Some(self.version.clone()))
    }

    fn download(&self) -> Result<(), String> {
        *self
            .downloaded
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
        Ok(())
    }

    fn apply(&self) -> Result<(), String> {
        self.fail.clone().map_or(Ok(()), Err)
    }
}

/// `state.archive`.
fn archive(cx: &App) -> Value {
    let archive = cx.services().archive.read(cx);
    let (state, error) = match archive.state() {
        ArchiveState::Opening => ("opening", None),
        ArchiveState::Open(_) => ("open", None),
        ArchiveState::Failed(failure) => ("failed", Some(failure)),
    };
    json!({
        "dir": archive.dir().display().to_string(),
        "state": state,
        "error": error.map(Failure::why),
        "failure": error.map(|failure| match failure {
            Failure::Missing { there: false } => "missing",
            Failure::Missing { there: true } => "empty",
            Failure::Locked(_) => "locked",
            Failure::Settings(_) => "settings",
            Failure::Other(_) => "other",
        }),
    })
}

/// `state.sync`.
fn sync(cx: &App) -> Value {
    let sync = cx.services().sync.read(cx);
    let now = sb_ui::format::now();
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
                LocationStatus::Stopped { .. } => ("stopped", None, None),
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
                "stale": sync.stale(location, now).is_some(),
                "removing": sync.is_removing(&location.id),
            })
        })
        .collect();
    let test = sync.test().map(|test| {
        let state = match &test.state {
            TestState::Running(_) => "running",
            TestState::Passed { .. } => "passed",
            TestState::Failed(failure) => match failure {
                TestFailure::Unreachable(_) => "unreachable",
                TestFailure::Refused(_) => "refused",
                TestFailure::HostKey(_) => "host_key",
                TestFailure::NotInstalled(_) => "not_installed",
                TestFailure::Missing(_) => "missing",
                TestFailure::LoginHung(_) => "login_hung",
                TestFailure::Other(_) => "other",
            },
        };
        let place = match &test.kind {
            LocationKind::Local => "local",
            LocationKind::Wsl { distro } => distro,
            LocationKind::Ssh { host } => host,
        };
        json!({ "place": place, "state": state })
    });
    let distros = sync.distros().map(|distros| match &distros.listed {
        Ok(listed) => json!(listed.as_ref().map(|listed| {
            listed
                .iter()
                .map(|distro| distro.name.clone())
                .collect::<Vec<_>>()
        })),
        Err(error) => json!({ "error": error }),
    });
    json!({
        "syncing": sync.is_syncing(),
        "indexing": sync.is_indexing(),
        "held": sync.is_held(),
        "locations": locations,
        "removed": sync.removed().iter().map(|it| it.name.clone()).collect::<Vec<_>>(),
        "refused": sync.refused(),
        "distros": distros,
        "test": test,
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
    let title = |id: &SessionId| library.session(id).and_then(|it| it.title.clone());
    let drag = library.drag().map(|drag| {
        let over = drag.over.map(|over| match over {
            DropTarget::Folder(folder) => json!({ "folder": name(folder) }),
            DropTarget::EmptyFolder(folder) => json!({ "empty_folder": name(folder) }),
            DropTarget::Trash => json!("trash"),
            DropTarget::Sidebar => json!("sidebar"),
            DropTarget::Between { parent, index, .. } => {
                json!({ "between": { "parent": parent.and_then(name), "index": index } })
            }
        });
        let verdict = drag.verdict(library).map(|verdict| match verdict {
            Verdict::Takes { result, note } => {
                json!({ "takes": true, "result": result, "note": note })
            }
            Verdict::Refuses(why) => json!({ "takes": false, "note": why }),
        });
        json!({
            "what": match &drag.what {
                Dragged::Sessions { ids, .. } => json!({ "sessions": ids.len() }),
                Dragged::Path { path, .. } => json!({ "path": path.0 }),
                Dragged::Folder(folder) => json!({ "folder": name(*folder) }),
            },
            "moved": drag.moved,
            "over": over,
            "verdict": verdict,
        })
    });
    json!({
        "selection": library.selection().iter().map(title).collect::<Vec<_>>(),
        "drag": drag,
        "toast": library.toast().map(|toast| json!({ "text": toast.text, "undo": toast.can_undo() })),
        "asked": library.asked().is_some(),
        "deleting": library.deleting().and_then(name),
        "shelf": match library.shelf() {
            Shelf::All => json!("all"),
            Shelf::Starred => json!("starred"),
            Shelf::Unsorted => json!("unsorted"),
            Shelf::Trash => json!("trash"),
            Shelf::Folder(folder) => json!({ "folder": name(folder) }),
        },
        "counts": {
            "all": counts.all,
            "starred": counts.starred,
            "unsorted": counts.unsorted,
            "trash": counts.trash,
        },
        "for_good": library.for_good().map(|asked| json!({
            "title": asked.session.title,
            "files": asked.deletion.as_ref().map(|it| it.files),
            "versions": asked.deletion.as_ref().map(|it| it.versions),
            "bytes": asked.deletion.as_ref().map(|it| it.bytes),
            "at_location": asked.deletion.as_ref().map(|it| it.at_location),
            "refused": asked.refused.as_ref().map(sb_ui::services::library::Refusal::said),
            "also": asked.deletion.as_ref().map(|it| it.also.len()),
        })),
        "read_at": library.read_at().map(read_at),
        "folders": folders,
        "sessions": sessions,
        "groups": groups,
        "selected": library.selected().and_then(title),
        "held": library.held().and_then(name),
        "open": library.open_session().and_then(|session| session.title.clone()),
        "open_from": library.open_session().and_then(|session| {
            let sync = cx.services().sync.read(cx);
            sb_ui::components::Place::of(session, |id| sync.location(id))
                .map(|place| place.name().to_string())
        }),
        "error": library.failure(),
    })
}

/// `state.settings`.
fn settings(shell: &Shell, cx: &App) -> Value {
    let services = cx.services();
    let settings = services.settings.read(cx);
    let (sync, upkeep) = (services.sync.read(cx), services.upkeep.read(cx));
    let archive = services.archive.read(cx).dir().to_path_buf();
    let problems: Vec<Value> = upkeep
        .problems()
        .iter()
        .map(|problem| {
            json!({
                "file": problem.damage.file,
                "fix": match problem.fix {
                    Fix::FetchAgain => "fetch_again",
                    Fix::Fetched { .. } => "fetched",
                    Fix::Damaged => "damaged",
                    Fix::Deleted => "deleted",
                },
                "refetch": sync.refetch_of(&problem.damage).map(|refetch| match refetch {
                    Refetch::Running => "running".to_string(),
                    Refetch::Done(_) => "done".to_string(),
                    Refetch::Skipped(why) => format!("skipped: {why}"),
                    Refetch::Failed(why) => why.clone(),
                }),
            })
        })
        .collect();
    json!({
        "section": shell.settings_section(cx).key(),
        "archive": archive.display().to_string(),
        "default": settings.is_default(&archive),
        "switch": settings.switch().map(|switch| match switch {
            Switch::Opening(folder) => json!({ "opening": folder.display().to_string() }),
            Switch::Refused { why, .. } => json!({ "refused": why }),
        }),
        "sync_every": sync.every().key(),
        "next_round": sync.next_round().is_some(),
        "stale_after": sync.stale_after_days(),
        "stats": upkeep.stats().map(|stats| json!({
            "sessions": stats.totals.sessions,
            "items": stats.totals.items,
            "versions": stats.totals.versions,
        })),
        "verify": {
            "running": upkeep.verifying().is_some(),
            "progress": upkeep.verifying().map(|verifying| json!({
                "segments": verifying.progress.segments,
                "segments_done": verifying.progress.segments_done,
            })),
            "last": upkeep.last().map(|last| json!({
                "segments": last.segments,
                "problems": problems,
            })),
            "fetch_all": sync.fetch_all_progress().map(|(done, total)| json!({
                "done": done,
                "total": total,
            })),
        },
        "deleted": upkeep.deleted().map(|deleted| deleted.iter().map(|deleted| json!({
            "location": deleted.location.as_ref().map(|location| &location.name),
            "allowed": matches!(deleted.standing, Standing::Allowed(_)),
            "standing": match deleted.standing {
                Standing::Deleted => "deleted",
                Standing::Allowed(_) => "allowed",
                Standing::ArchivedAgain { .. } => "archived_again",
            },
        })).collect::<Vec<_>>()),
        "saved": settings.save_failure(),
    })
}

/// `state.palette`: the search palette, while it is open.
fn palette(palette: &Palette, cx: &App) -> Value {
    let search = cx.services().search.read(cx);
    let answer = search.answer().map(|answered| match &answered.answer {
        Searched::KeepTyping => json!("keep-typing"),
        Searched::Unavailable(why) => json!({ "unavailable": why }),
        Searched::Found(results) => json!({
            "sessions": results
                .sessions
                .iter()
                .map(|found| found.session.title.clone())
                .collect::<Vec<_>>(),
            "total_sessions": results.total_sessions,
            "total_matches": results.total_matches,
            "terms": answered.query.terms,
            "took_ms": answered.took.as_millis() as u64,
        }),
    });
    json!({
        "text": palette.text(cx),
        "chips": palette.chips().iter().map(|chip| &chip.label).collect::<Vec<_>>(),
        "completions": palette.completion_labels(cx),
        "rows": palette.rows(cx).len(),
        "highlighted": palette.highlight(),
        "commands": palette.command_names(cx),
        "unresolved": palette.unresolved(cx),
        "searching": search.is_searching(),
        "unsearchable": search.unsearchable(),
        "answer": answer,
    })
}

/// `state.page_view`: how the page of sessions shows, and its paths editor.
fn page_view(shell: &Shell, cx: &App) -> Value {
    let page = shell.main().read(cx).sessions().read(cx);
    let view = cx.services().library.read(cx).view();
    json!({
        "list": view.list,
        "oldest_first": view.oldest_first,
        "filter": view.filter,
        "filtered": view.is_filtered(),
        "shown": page.shown(),
        "editor": page.editor().map(|editor| {
            let editor = editor.read(cx);
            json!({
                "short": editor.is_short(),
                "added": editor.added().collect::<Vec<_>>(),
                "removed": editor.removed().collect::<Vec<_>>(),
            })
        }),
    })
}

/// `state.cards`: what the page of cards last decided its cards' last lines from.
fn cards(measure: &LineMeasure) -> Value {
    let px = |width: gpui::Pixels| (f32::from(width) * 100.).round() / 100.;
    json!({
        "shows_calls": measure.shows_calls,
        "tags_past_caps": measure.tags_past_caps,
        "viewport": px(measure.viewport),
        "scale": measure.scale,
        "inner": px(measure.inner),
        "line": {
            "with_calls": px(measure.with_calls),
            "without_calls": px(measure.without_calls),
            "spare": px(measure.spare),
        },
        "tags": measure.tags.iter().map(|(name, whole, capped)| json!({
            "name": name,
            "whole": px(*whole),
            "capped": px(*capped),
        })).collect::<Vec<_>>(),
        "day": { "text": measure.day.0, "width": px(measure.day.1) },
        "calls": { "text": measure.calls.0, "width": px(measure.calls.1) },
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
        "top_name": view.top_name,
        "top_offset": view.top_offset,
        "at_end": view.at_end,
        "following": view.following,
        "focused": view.focused.map(|(at, kind, text)| json!({ "row": at, "kind": kind, "text": text })),
        "open_calls": view.open_calls,
        "markdown_states": view.states,
        "held": view.held,
        "alone": view.alone,
        "read_at": view.read_at.as_ref().map(read_at),
        "flash": view.flash.map(|(block, text, drawn)| json!({
            "block": block,
            "text": text,
            "drawn": drawn,
        })),
        "find": view.find.map(|(query, count, current)| json!({
            "query": query,
            "count": count,
            "current": current,
        })),
        "facts": view.facts,
        "history": view.history,
        "viewing": view.viewing,
        "full_image": view.full_image,
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

/// A request to open a session at a search match, as `state.library` and `state.reader` show it.
fn read_at(at: &ReadAt) -> Value {
    json!({
        "kind": format!("{:?}", at.kind).to_lowercase(),
        "turn": match at.at {
            Some(At::Block { turn, .. }) => Some(turn),
            _ => None,
        },
        "request": at.request,
        "subagent": at.subagent.as_ref().map(|it| &it.item),
        "find": at.find,
        "flash": at.flash,
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

/// `answer_picker {"folder": path | null}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerPicker {
    /// `null` cancels the picker. Required, as `pin_appearance`'s argument is.
    #[serde(deserialize_with = "Option::deserialize")]
    folder: Option<PathBuf>,
}

/// `update_ready {"version": …, "fail"?: …, "others"?: n}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateReady {
    version: String,
    fail: Option<String>,
    #[serde(default)]
    others: usize,
}

/// `hold_sync {"after": items}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HoldSync {
    after: usize,
}

/// `release_sync {"until"?: items}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseSync {
    until: Option<usize>,
}

/// `stand_in {distros?, places?}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StandIn {
    distros: Option<Distros>,
    #[serde(default)]
    places: HashMap<String, Answer>,
}

/// WSL's distros as a scenario has them: listed, or `wsl.exe`'s failure.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Distros {
    Listed(Vec<DistroArg>),
    Failed { error: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DistroArg {
    name: String,
    #[serde(default)]
    running: bool,
    #[serde(default)]
    default: bool,
    #[serde(default)]
    utility: bool,
}

/// How a distro or a host answers the stand-ins (the module docs, `stand_in`).
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Answer {
    Passes,
    Unreachable,
    Refused,
    HostKey,
    NewHostKey,
    MissingTools,
    NoSsh,
    LoginHung,
    Fails,
}

impl Answer {
    /// What the place `name` fails with when it answers so, in the words its launcher would
    /// use. One that passes fails nothing; it is read as this machine.
    fn error(self, name: &str) -> TransportError {
        match self {
            Self::Passes => TransportError::Failed(format!("{name} passes")),
            Self::Unreachable => TransportError::Unreachable(format!(
                "ssh: connect to host {name} port 22: Operation timed out"
            )),
            Self::Refused => {
                TransportError::Refused(format!("{name}: Permission denied (publickey)."))
            }
            /* As sb_core reads ssh's words: its verdict first, then what it said before. */
            Self::HostKey => TransportError::HostKey(format!(
                "Host key verification failed.\n\
                 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
                 @    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n\
                 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
                 Host key for {name} has changed and you have requested strict checking."
            )),
            Self::NewHostKey => TransportError::HostKey(format!(
                "Host key verification failed.\n\
                 No ED25519 host key is known for {name} and you have requested strict checking."
            )),
            Self::MissingTools => {
                TransportError::MissingTools(vec!["sha256sum or shasum".to_string()])
            }
            Self::NoSsh => TransportError::Launch {
                program: "ssh".to_string(),
                source: std::io::ErrorKind::NotFound.into(),
            },
            Self::LoginHung => TransportError::LoginShellHung(5),
            Self::Fails => TransportError::Failed(
                "kex_exchange_identification: read: Connection reset by peer".to_string(),
            ),
        }
    }
}

/// What the stand-ins answer, as the scenario set it.
struct Stage {
    distros: Result<Option<Vec<Distro>>, String>,
    places: HashMap<String, Answer>,
}

static STAGE: LazyLock<Mutex<Stage>> = LazyLock::new(|| {
    Mutex::new(Stage {
        distros: Ok(None),
        places: HashMap::new(),
    })
});

fn stage() -> MutexGuard<'static, Stage> {
    STAGE.lock().unwrap_or_else(PoisonError::into_inner)
}

/**
 * The name a place of `kind` goes by and how it answers: as the scenario said, or else a
 * distro passes when there is a stand-in home to read it from and a host is asleep. A place
 * that would pass with no stand-in home can't be reached: it is never read as this machine.
 * This machine is `None`: it is read as it is.
 */
fn answer<'a>(kind: &'a LocationKind, home: Option<&str>) -> Option<(&'a str, Answer)> {
    let (name, otherwise) = match kind {
        LocationKind::Local => return None,
        LocationKind::Wsl { distro } => (distro, Answer::Passes),
        LocationKind::Ssh { host } => (host, Answer::Unreachable),
    };
    let answered = stage()
        .places
        .get(name.as_str())
        .copied()
        .unwrap_or(otherwise);
    Some((
        name,
        match (answered, home) {
            (Answer::Passes, None) => Answer::Unreachable,
            (answered, _) => answered,
        },
    ))
}

/**
 * How a scripted run reaches its locations, or `None` for a run without the driver, which
 * reaches them as their kinds say. Never the distro or the host a location names, which would
 * be the machine's own and not the scenario's, and never through `wsl.exe` or `ssh`. This
 * machine is read with the scenario's home as its home, where the runner points its agent;
 * hosts a copy's records name resolve as `ssh` with no config would make them
 * (`sb_core::transport::ssh_config::NoConfig`), never through `ssh -G`. A place that
 * passes is read from the folder `--automation-wsl-home` names ([`StandInHome`]); without one
 * it can't be reached, so a distro or a host is never read as this machine. Any other place
 * fails every call as the scenario said ([`Answer`]). The connection test answers the same
 * way, and WSL's distros are the scenario's. Every launcher is refused from then on, so a
 * path that bypasses the stand-ins starts nothing either.
 */
pub fn stand_ins(cli: &Cli) -> Option<Reach> {
    if !cli.automation_enabled() {
        return None;
    }
    sb_core::transport::refuse_launchers();
    let home = cli
        .automation_wsl_home
        .as_ref()
        .map(|home| home.display().to_string());
    let tested = home.clone();
    Some(Reach {
        connect: Arc::new(
            move |kind, _| match (answer(kind, home.as_deref()), &home) {
                /* This machine as the scenario's home, so that nothing kept beside the home's
                agent (the desktop app's folder, PLAN §7.3) is read from the person's own. */
                (None, Some(home)) => Box::new(StandInHome { home: home.clone() }),
                (None, None) => Box::new(LocalTransport),
                (Some((_, Answer::Passes)), _) => Box::new(StandInHome {
                    home: home.clone().unwrap_or_default(),
                }),
                (Some((name, answered)), _) => Box::new(Failing {
                    name: name.to_string(),
                    answered,
                }),
            },
        ),
        tester: Arc::new(move |launcher, _| {
            let kind = match launcher {
                Launcher::Wsl { distro } => LocationKind::Wsl {
                    distro: distro.clone(),
                },
                Launcher::Ssh { host } => LocationKind::Ssh { host: host.clone() },
                Launcher::Command { .. } => {
                    return Err(TransportError::Failed("no command is run".to_string()));
                }
            };
            match answer(&kind, tested.as_deref()) {
                Some((_, answered @ (Answer::Passes | Answer::MissingTools))) => Ok(Probe {
                    home: tested.clone().unwrap_or_default(),
                    clock_ns: 0,
                    clock_ahead_ns: 0,
                    missing: if answered == Answer::MissingTools {
                        vec!["sha256sum or shasum".to_string()]
                    } else {
                        Vec::new()
                    },
                    whole_seconds: false,
                    shell: "/bin/sh".to_string(),
                    login_shell: Some("/bin/bash".to_string()),
                    drive_mount: None,
                    took: Duration::from_millis(1400),
                }),
                Some((name, answered)) => Err(answered.error(name)),
                None => Err(TransportError::Failed(
                    "this machine has no shell".to_string(),
                )),
            }
        }),
        distros: Arc::new(|_| stage().distros.clone().map_err(TransportError::Failed)),
        launcher: Arc::new(LocationKind::launcher),
        development_home: None,
        resolve: Arc::new(sb_core::transport::ssh_config::NoConfig),
    })
}

/**
 * A distro or a host played by a folder of this machine: its files are this machine's and its
 * home is that folder. Of this machine's environment it sees only the variables that point
 * inside that folder (the runner's, which point an agent at the scenario's fixture), so
 * nothing of the person's own setup is ever read through it.
 */
struct StandInHome {
    home: String,
}

impl Transport for StandInHome {
    fn env(&self, names: &[&str]) -> Result<Env, TransportError> {
        let vars = names
            .iter()
            .filter_map(|name| {
                let value = std::env::var(name).ok()?;
                std::path::Path::new(&value)
                    .starts_with(&self.home)
                    .then(|| ((*name).to_string(), value))
            })
            .collect();
        Ok(Env {
            home: self.home.clone(),
            vars,
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

/**
 * The development override (`--dev-location-home`): every WSL and SSH location is reached
 * through its own launcher, `wsl.exe` or `ssh`, with the far side's shell started with
 * `home` for its home and nothing else of its environment ([`Launcher::reading_home`]), for
 * its connection test, its survey and every pass. This machine is read as it is. Logged at
 * the start, and said on the locations sheet, so that a build is never left in it unnoticed.
 */
pub fn development_override(home: &str) -> Reach {
    log::warn!("development override: every WSL and SSH location is read with {home} for its home");
    let launch = {
        let home = home.to_string();
        move |kind: &LocationKind| kind.launcher().map(|it| it.reading_home(&home))
    };
    let connect = launch.clone();
    Reach {
        connect: Arc::new(move |kind, cancel| match connect(kind) {
            Some(launcher) => Box::new(ShellTransport::new(launcher).cancelled_by(cancel)),
            None => Box::new(LocalTransport),
        }),
        tester: Arc::new(sb_ui::services::probe),
        distros: Arc::new(sb_core::transport::wsl::distros),
        launcher: Arc::new(launch),
        development_home: Some(home.to_string()),
        resolve: Arc::new(sb_core::transport::ssh_config::NoConfig),
    }
}

/// A place that fails every call as it answers.
struct Failing {
    name: String,
    answered: Answer,
}

impl Failing {
    fn fail<T>(&self) -> Result<T, TransportError> {
        Err(self.answered.error(&self.name))
    }
}

impl Transport for Failing {
    fn env(&self, _: &[&str]) -> Result<Env, TransportError> {
        self.fail()
    }

    fn stat(&self, _: &[&str]) -> Result<Vec<Option<FileStat>>, TransportError> {
        self.fail()
    }

    fn list(&self, _: &str) -> Result<Listing, TransportError> {
        self.fail()
    }

    fn hash_prefixes(
        &self,
        _: &[PrefixRequest],
    ) -> Result<Vec<Result<Hashed, RangeFailure>>, TransportError> {
        self.fail()
    }

    fn read_ranges(&self, _: &[RangeRequest], _: &mut dyn RangeSink) -> Result<(), TransportError> {
        self.fail()
    }

    fn read_small(&self, _: &str) -> Result<Option<Vec<u8>>, TransportError> {
        self.fail()
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

    use sb_core::CancelToken;
    use sb_core::transport::LAUNCH_REFUSED;

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
            (unnamed.connect)(&distro, &cancel).env(&["HOME"]),
            Err(TransportError::Unreachable(_))
        ));
        let home = std::env::temp_dir().join("stand-in-home");
        let named = stand_ins(&cli(&[
            "--automation",
            "--automation-wsl-home",
            home.to_str().unwrap(),
        ]))
        .unwrap();
        let env = (named.connect)(&distro, &cancel).env(&["HOME"]).unwrap();
        assert_eq!(env.home, home.display().to_string());
        assert!(env.vars.is_empty(), "none of this machine's environment");
        // A host is never reached, home or not.
        let host = LocationKind::Ssh {
            host: "user@macbook".to_string(),
        };
        assert!(matches!(
            (named.connect)(&host, &cancel).env(&[]),
            Err(TransportError::Unreachable(_))
        ));
    }

    #[test]
    fn a_run_with_the_driver_starts_no_launcher_even_past_its_stand_ins() {
        let reach = stand_ins(&cli(&["--automation"])).unwrap();
        let cancel = CancelToken::new();
        let host = LocationKind::Ssh {
            host: "user@macbook.invalid".to_string(),
        };
        let distro = LocationKind::Wsl {
            distro: "Ubuntu-24.04".to_string(),
        };
        let refused = |result: Result<Env, TransportError>| match result {
            Err(TransportError::Failed(said)) => said.starts_with(LAUNCH_REFUSED),
            _ => false,
        };
        for kind in [&host, &distro] {
            /* The stand-ins answer without a launcher… */
            assert!(!refused((reach.connect)(kind, &cancel).env(&[])));
            let launcher = (reach.launcher)(kind).unwrap();
            assert!((reach.tester)(&launcher, &cancel).is_err());
            /* …and the real transport, reached past them, is refused before it starts. */
            assert!(refused(kind.transport(&cancel).env(&[])), "{kind:?}");
        }
        assert_eq!((reach.distros)(&cancel).unwrap(), None);
        /* A host a copy's record names resolves through the stand-in, never `ssh -G`, which
        is refused before it starts. */
        assert!(reach.resolve.resolve("user@macbook.invalid").is_some());
        assert!(matches!(
            sb_core::transport::SshConfig::run("user@macbook.invalid"),
            Err(TransportError::Failed(said)) if said.starts_with(LAUNCH_REFUSED)
        ));
        assert_eq!(
            sb_core::transport::ResolveHost::resolve(
                &sb_core::transport::SshConfig,
                "user@macbook.invalid"
            ),
            None
        );
    }

    #[test]
    fn the_development_override_reads_distros_and_hosts_from_its_home_and_nothing_else() {
        let reach = development_override("/home/user/fixture");
        assert_eq!(
            reach.development_home.as_deref(),
            Some("/home/user/fixture")
        );
        let distro = LocationKind::Wsl {
            distro: "Ubuntu-24.04".to_string(),
        };
        let wsl = (reach.launcher)(&distro).unwrap().command_line();
        assert!(
            wsl.starts_with("wsl.exe -d Ubuntu-24.04 --cd / --exec env -i PATH=")
                && wsl.ends_with(" HOME=/home/user/fixture sh -s"),
            "{wsl}"
        );
        let host = LocationKind::Ssh {
            host: "user@macbook".to_string(),
        };
        let ssh = (reach.launcher)(&host).unwrap().command_line();
        assert!(
            ssh.starts_with("ssh -T -o BatchMode=yes")
                && ssh.contains(" -- user@macbook env -i PATH=")
                && ssh.ends_with(" HOME=/home/user/fixture sh -s"),
            "{ssh}"
        );
        /* This machine has no launcher, and is read as it is. */
        assert!((reach.launcher)(&LocationKind::Local).is_none());
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
