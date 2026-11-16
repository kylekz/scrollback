//! `scrollback`: the app (docs/PLAN.md §3). One process: it opens the window itself and never
//! detaches. Everything it shows is `sb_ui`'s.
//!
//! On Windows the shipped app (built without `automation`) is a windows-subsystem program, so
//! starting it from the Start menu opens no console; a development build keeps its console for
//! the log, and the e2e runner talks to it over stdio. Without a console, what stops it from
//! starting, and `--help` and `--version`, show in a message box (`fatal`), and a panic is
//! written to `crash.log` in the data folder (`crash`).

#![cfg_attr(
    all(windows, not(feature = "automation")),
    windows_subsystem = "windows"
)]

mod app;
#[cfg(feature = "automation")]
mod automation;
mod cli;
mod crash;
mod data_dir;
mod fatal;
#[cfg(any(windows, target_os = "macos"))]
mod installer;
#[cfg(any(windows, target_os = "macos"))]
mod processes;
mod updates;

use std::process::ExitCode;

use clap::Parser as _;
use clap::error::ErrorKind;

/* On macOS only: the system allocator keeps the large blocks a closed session freed resident
(Cargo.toml). */
#[cfg(target_os = "macos")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Hands back to the system what mimalloc holds free: run after a session closes and a sync
/// pass ends (`sb_ui::system::GiveBack`).
#[cfg(target_os = "macos")]
fn give_back() {
    // SAFETY: `mi_collect` takes no pointers; forced, it frees what the calling thread's
    // heap holds free and purges what the process does.
    unsafe { libmimalloc_sys::mi_collect(true) }
}

fn main() -> ExitCode {
    #[cfg(any(windows, target_os = "macos"))]
    installer::run();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .init();
    let cli = match cli::Cli::try_parse() {
        Ok(cli) => cli,
        /* `--help` and `--version` aren't errors: their text, and exit 0. */
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            fatal::show_info(&error.render().to_string());
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            fatal::show_parse_error(&error.render().to_string());
            return ExitCode::from(2);
        }
    };
    match app::run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            fatal::report(&error);
            ExitCode::FAILURE
        }
    }
}
