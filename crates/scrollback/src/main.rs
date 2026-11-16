//! `scrollback`: the app (docs/PLAN.md §3). One process: it opens the window itself and never
//! detaches. Everything it shows is `sb_ui`'s.
//!
//! On Windows the shipped app (built without `automation`) is a windows-subsystem program, so
//! starting it from the Start menu opens no console; a development build keeps its console for
//! the log, and the e2e runner talks to it over stdio.

#![cfg_attr(
    all(windows, not(feature = "automation")),
    windows_subsystem = "windows"
)]

mod app;
#[cfg(feature = "automation")]
mod automation;
mod cli;
mod data_dir;
#[cfg(any(windows, target_os = "macos"))]
mod installer;
mod updates;

use std::process::ExitCode;

use clap::Parser as _;

fn main() -> ExitCode {
    #[cfg(any(windows, target_os = "macos"))]
    installer::run();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .init();
    match app::run(cli::Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            log::error!("scrollback couldn't start: {error:#}");
            ExitCode::FAILURE
        }
    }
}
