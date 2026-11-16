//! `e2e`: scrollback's scenarios (`tests/e2e`, docs/PLAN.md §12) through toolbox-rs's runner.
//! toolbox-rs's docs/automation.md documents the runner, the scenario format and the protocol;
//! the `scrollback` crate's `automation` module documents scrollback's `state` and commands.
//!
//! ```text
//! cargo build -p scrollback
//! cargo run -p sb_e2e --bin e2e -- tests/e2e
//! ```
//!
//! **No scenario reads this machine's own transcripts.** Every app the runner starts is pointed
//! at a home folder of the scenario's, under its output folder, and a header's `fixture` says
//! what that home and the app's data dir hold ([`Library`]):
//!
//! - `"no-agent"` (the default): nothing. The first run finds no agent on this machine.
//! - `"agent"`: an agent's generated transcripts ([`fixture::agent`]), for the first run to
//!   find and sync.
//! - `"agent-large"`: the boards' sessions as one agent's transcripts ([`boards::agent`]): a
//!   first sync that takes long enough to be seen.
//! - `"boards"`: an archive already in the data dir, holding the library the design boards
//!   draw ([`boards`]), so that the pages can be captured as the boards show them
//!   (docs/DESIGN.md §6).
//! - `"boards-held-back"`: the same, with one of the subfolder player's sessions held back
//!   and filed by hand ([`boards::build`]).
//! - `"search"`: the boards' library with the sessions `E-Search` finds, whose prompts,
//!   replies, commands, paths and subagent hold what it matches ([`extras::search`]).
//! - `"trash"`: the boards' library with the three sessions `E-Trash` lists in Trash, one of
//!   them gone at its location ([`extras::trash`]).
//! - `"reader"`: an archive in the data dir with the sessions the reader's scenarios open
//!   ([`reader`]): the boards' reader sessions, every construct the reader draws, and the
//!   edge states.
//! - `"reader-large"`: the same, with a session large enough to measure the reader's budgets
//!   against (docs/PLAN.md §14).
//! - `"reader-versions"`: the same, with `E-Versions`' session, whose newest version is held
//!   back (docs/DESIGN.md §4.3).
//! - `"locations"`: an archive in the data dir with one location of each kind, the host's
//!   last good sync twelve days ago, and this machine's agent in the home
//!   ([`fixture::locations`]): the locations sheet's states (docs/DESIGN.md §4.11).
//! - `"archive-remains"`: what is left of an archive in the data dir, its files without its
//!   `archive.json`: the app must say it can't open it, and make no new one in its place
//!   (docs/PLAN.md §4.4).
//! - `"settings"`: an archive in the data dir with a distro read from the home, two of whose
//!   transcripts are damaged (one still in the home, one gone from it), and a host whose two
//!   sessions were deleted for good ([`fixture::settings`]): Settings › Archive
//!   (docs/DESIGN.md §4.13).
//! - `"archive-gone"`: settings that name an archive in a folder that isn't there, as on a
//!   drive unplugged, and remember what it held ([`fixture::archive_gone`]): the
//!   archive-unavailable page (docs/DESIGN.md §4.14).
//! - `"settings-unreadable"`: a settings file that isn't JSON, naming an archive
//!   ([`fixture::settings_unreadable`]): nothing is opened or made until the owner chooses.
//!
//! **A time is held to a budget by a runner step**, `{"budget": {"ms": "${saved}", "within":
//! 500}}` ([`Action::Budget`]), never by an expectation's bound: a CI runner's budget is half as
//! long again.
//!
//! **No scenario starts `ssh` or `wsl.exe`.** The app reaches every distro and host through its
//! stand-ins and refuses any launcher under automation, logging the attempt as an error; a
//! scenario whose app logged one fails ([`Home::finish`]). On Unix, a tripwire for each comes
//! first on the app's `PATH` as well and notes being started, which fails the scenario too.

mod boards;
mod extras;
mod fixture;
mod reader;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail, ensure};
use gpui_automation_runner::{Fixture, FixtureCx, Profile, RunnerApp};
use sb_core::transport::LAUNCH_REFUSED;
use sb_test_support::ProjectsTree;
use serde::Deserialize;
use serde_json::{Value, json};

use extras::Extra;

/// What a scenario's app finds (the module docs).
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Library {
    #[default]
    NoAgent,
    Agent,
    AgentLarge,
    Boards,
    BoardsHeldBack,
    Search,
    Trash,
    Reader,
    ReaderLarge,
    ReaderVersions,
    Locations,
    ArchiveRemains,
    Settings,
    ArchiveGone,
    SettingsUnreadable,
}

/// scrollback's header keys: `{"scenario": {"fixture": "boards"}}`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    #[serde(default)]
    fixture: Library,
}

/// What every launch of a scenario's app is started with: the environment that points this
/// machine at the scenario's home, and that home as the stand-in for a WSL distro. `out_dir`
/// is where its logs and the tripwires' note are looked through once it is done.
struct Home {
    env: Vec<(OsString, Option<OsString>)>,
    home: PathBuf,
    out_dir: PathBuf,
}

/// The launchers no scenario may start.
#[cfg(unix)]
const LAUNCHERS: [&str; 2] = ["ssh", "wsl.exe"];

/// Where a tripwire notes that it was started, in the scenario's output folder.
const TRIPPED: &str = "launchers-started.log";

/**
 * Lays a tripwire for each of [`LAUNCHERS`] in `out_dir` and gives the `PATH` that finds them
 * first. Each notes what it was started with in [`TRIPPED`] and fails as an unreachable `ssh`
 * would. Unix only: Windows finds `wsl.exe` in its system folder before any on `PATH`.
 */
#[cfg(unix)]
fn tripwires(out_dir: &Path) -> Result<Vec<(OsString, Option<OsString>)>> {
    use std::os::unix::fs::PermissionsExt as _;

    let bin = out_dir.join("tripwires");
    std::fs::create_dir_all(&bin)?;
    for launcher in LAUNCHERS {
        let tripwire = bin.join(launcher);
        std::fs::write(
            &tripwire,
            format!(
                "#!/bin/sh\necho \"{launcher} $*\" >> '{}'\nexit 255\n",
                out_dir.join(TRIPPED).display()
            ),
        )?;
        std::fs::set_permissions(&tripwire, std::fs::Permissions::from_mode(0o755))?;
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&path)))?;
    Ok(vec![("PATH".into(), Some(path))])
}

#[cfg(not(unix))]
fn tripwires(_: &Path) -> Result<Vec<(OsString, Option<OsString>)>> {
    Ok(Vec::new())
}

/// The year the fixtures' newest sessions are dated in. The app writes its dates as seen
/// from it, in UTC, so that a capture is the same in any year and on any machine.
const CALENDAR: &str = "2026";

/// scrollback's runner steps, run here and never sent to the app.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Action {
    /**
     * A time an earlier step measured, saved from its result, held to a budget:
     * `{"budget": {"ms": "${opened_ms}", "within": 500}}`. The runner's expectations take only a
     * fixed bound, and the same work takes longer on a CI runner (a few shared cores, a software
     * renderer on Windows), so there the budget is [`CI_BUDGET`] times as long.
     */
    Budget { ms: String, within: f64 },
}

/// How much longer a budget is on a CI runner: when the runner's environment sets `CI`.
const CI_BUDGET: f64 = 1.5;

impl Fixture<Action> for Home {
    fn env(&self) -> Vec<(OsString, Option<OsString>)> {
        self.env.clone()
    }

    fn args(&self) -> Vec<String> {
        vec![
            "--automation-calendar".to_string(),
            CALENDAR.to_string(),
            "--automation-wsl-home".to_string(),
            self.home.display().to_string(),
        ]
    }

    fn run(&mut self, action: &Action) -> Result<Value> {
        match action {
            Action::Budget { ms, within } => {
                let took: f64 = ms
                    .parse()
                    .with_context(|| format!("budget: {ms:?} is not a time in milliseconds"))?;
                let ci = std::env::var_os("CI").is_some();
                let budget = if ci { within * CI_BUDGET } else { *within };
                ensure!(
                    took <= budget,
                    "{took} ms, past the budget of {budget} ms{}",
                    if ci { " (a CI runner's)" } else { "" }
                );
                Ok(json!({"ms": took, "budget": budget, "ci": ci}))
            }
        }
    }

    /// Fails the scenario when its app tried to start a launcher, or started one.
    fn finish(&mut self) -> Result<()> {
        if let Ok(started) = std::fs::read_to_string(self.out_dir.join(TRIPPED)) {
            bail!("the app started a launcher:\n{started}");
        }
        for entry in std::fs::read_dir(&self.out_dir)? {
            let path = entry?.path();
            let is_log = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("app.") && name.ends_with(".stderr.log"));
            if !is_log {
                continue;
            }
            let log = std::fs::read_to_string(&path)?;
            if let Some(refused) = log.lines().find(|line| line.contains(LAUNCH_REFUSED)) {
                bail!("the app tried to start a launcher: {refused}");
            }
        }
        Ok(())
    }
}

struct Scrollback;

impl RunnerApp for Scrollback {
    type Header = Header;
    type Action = Action;
    const HEADER_KEYS: &'static [&'static str] = &["fixture"];
    const ACTIONS: &'static [&'static str] = &["budget"];
    /// Where the agent's provider looks for its transcripts: the fixture's, so that no
    /// scenario reads the machine it runs on.
    const RESERVED_ENV: &'static [&'static str] = &["CLAUDE_CONFIG_DIR"];

    fn profile(&self) -> Profile {
        Profile {
            manifest_dir: env!("CARGO_MANIFEST_DIR"),
            name: "scrollback",
            build_hint: "cargo build -p scrollback",
            /* Every variable scrollback reads: inherited ones never reach a run, so the
             * owner's own data dir can't either. */
            env_prefix: "SCROLLBACK_",
            data_dir_var: "SCROLLBACK_DATA_DIR",
            /* The size the design boards are drawn at (DESIGN §2). */
            default_window_size: "1440x900",
            platforms: &["windows", "macos"],
        }
    }

    fn fixture(&self, header: &Header, cx: &FixtureCx) -> Result<Box<dyn Fixture<Action>>> {
        let home = cx.out_dir.join("home");
        let mut tree = ProjectsTree::in_home(&home);
        let env: Vec<(OsString, Option<OsString>)> = tree
            .vars()
            .iter()
            .map(|(name, value)| (name.into(), Some(value.into())))
            .chain(tripwires(cx.out_dir)?)
            .collect();
        match header.fixture {
            Library::Agent => fixture::agent(&mut tree),
            Library::AgentLarge => boards::agent(&mut tree),
            Library::NoAgent => std::fs::remove_dir_all(&home)?,
            Library::Boards | Library::BoardsHeldBack | Library::Search | Library::Trash => {
                // Its locations' trees are synced into the archive here, and gone by the
                // time the app starts: the app finds no agent, and syncs nothing away.
                std::fs::remove_dir_all(&home)?;
                boards::build(
                    &cx.data_dir.join("archive"),
                    &cx.out_dir.join("locations"),
                    match header.fixture {
                        Library::BoardsHeldBack => Extra::HeldBack,
                        Library::Search => Extra::Search,
                        Library::Trash => Extra::Trash,
                        _ => Extra::Nothing,
                    },
                )?;
            }
            Library::Reader | Library::ReaderLarge | Library::ReaderVersions => {
                std::fs::remove_dir_all(&home)?;
                reader::build(
                    &cx.data_dir.join("archive"),
                    &cx.out_dir.join("locations"),
                    match header.fixture {
                        Library::ReaderLarge => reader::Extra::Large,
                        Library::ReaderVersions => reader::Extra::Versions,
                        _ => reader::Extra::None,
                    },
                )?;
            }
            Library::Locations => fixture::locations(
                &mut tree,
                &cx.data_dir.join("archive"),
                &cx.out_dir.join("locations"),
            )?,
            Library::Settings => fixture::settings(
                &mut tree,
                &cx.data_dir.join("archive"),
                &cx.out_dir.join("locations"),
            )?,
            Library::ArchiveRemains => {
                std::fs::remove_dir_all(&home)?;
                std::fs::create_dir_all(cx.data_dir.join("archive/files/location"))?;
            }
            Library::ArchiveGone => {
                std::fs::remove_dir_all(&home)?;
                fixture::archive_gone(cx.data_dir, &cx.out_dir.join("drive"))?;
            }
            Library::SettingsUnreadable => {
                std::fs::remove_dir_all(&home)?;
                fixture::settings_unreadable(cx.data_dir, &cx.out_dir.join("drive"))?;
            }
        }
        Ok(Box::new(Home {
            env,
            home,
            out_dir: cx.out_dir.to_path_buf(),
        }))
    }
}

fn main() -> ExitCode {
    gpui_automation_runner::main(Scrollback)
}
