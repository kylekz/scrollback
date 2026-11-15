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
//! - `"archive-remains"`: what is left of an archive in the data dir, its files without its
//!   `archive.json`: the app must say it can't open it, and make no new one in its place
//!   (docs/PLAN.md §4.4).

mod boards;
mod fixture;

use std::ffi::OsString;
use std::process::ExitCode;

use anyhow::Result;
use gpui_automation_runner::{Fixture, FixtureCx, NoAction, Profile, RunnerApp};
use sb_test_support::ProjectsTree;
use serde::Deserialize;
use serde_json::Value;

/// What a scenario's app finds (the module docs).
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Library {
    #[default]
    NoAgent,
    Agent,
    AgentLarge,
    Boards,
    ArchiveRemains,
}

/// scrollback's header keys: `{"scenario": {"fixture": "boards"}}`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    #[serde(default)]
    fixture: Library,
}

/// What every launch of a scenario's app is started with.
struct Home {
    env: Vec<(OsString, Option<OsString>)>,
}

/// The year the fixtures' newest sessions are dated in. The app writes its dates as seen
/// from it, in UTC, so that a capture is the same in any year and on any machine.
const CALENDAR: &str = "2026";

impl Fixture<NoAction> for Home {
    fn env(&self) -> Vec<(OsString, Option<OsString>)> {
        self.env.clone()
    }

    fn args(&self) -> Vec<String> {
        vec!["--automation-calendar".to_string(), CALENDAR.to_string()]
    }

    fn run(&mut self, action: &NoAction) -> Result<Value> {
        match *action {}
    }
}

struct Scrollback;

impl RunnerApp for Scrollback {
    type Header = Header;
    type Action = NoAction;
    const HEADER_KEYS: &'static [&'static str] = &["fixture"];
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

    fn fixture(&self, header: &Header, cx: &FixtureCx) -> Result<Box<dyn Fixture<NoAction>>> {
        let home = cx.out_dir.join("home");
        let mut tree = ProjectsTree::in_home(&home);
        let env: Vec<(OsString, Option<OsString>)> = tree
            .vars()
            .iter()
            .map(|(name, value)| (name.into(), Some(value.into())))
            .collect();
        match header.fixture {
            Library::Agent => fixture::agent(&mut tree),
            Library::AgentLarge => boards::agent(&mut tree),
            Library::NoAgent => std::fs::remove_dir_all(&home)?,
            Library::Boards => {
                // Its locations' trees are synced into the archive here, and gone by the
                // time the app starts: the app finds no agent, and syncs nothing away.
                std::fs::remove_dir_all(&home)?;
                boards::build(&cx.data_dir.join("archive"), &cx.out_dir.join("locations"))?;
            }
            Library::ArchiveRemains => {
                std::fs::remove_dir_all(&home)?;
                std::fs::create_dir_all(cx.data_dir.join("archive/files/location"))?;
            }
        }
        Ok(Box::new(Home { env }))
    }
}

fn main() -> ExitCode {
    gpui_automation_runner::main(Scrollback)
}
