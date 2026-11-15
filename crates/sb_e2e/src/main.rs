//! `e2e`: scrollback's scenarios (`tests/e2e`, docs/PLAN.md §12) through toolbox-rs's runner.
//! toolbox-rs's docs/automation.md documents the runner, the scenario format and the protocol;
//! the `scrollback` crate's `automation` module documents scrollback's `state` and commands.
//!
//! ```text
//! cargo build -p scrollback
//! cargo run -p sb_e2e --bin e2e -- tests/e2e
//! ```

use std::process::ExitCode;

use gpui_automation_runner::{NoAction, NoHeader, Profile, RunnerApp};

struct Scrollback;

impl RunnerApp for Scrollback {
    type Header = NoHeader;
    type Action = NoAction;

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
}

fn main() -> ExitCode {
    gpui_automation_runner::main(Scrollback)
}
