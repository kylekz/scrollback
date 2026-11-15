//! `scrollback`: the app (docs/PLAN.md §3). One process: it opens the window itself and never
//! detaches. Everything it shows is `sb_ui`'s.

mod app;
#[cfg(feature = "automation")]
mod automation;
mod cli;
mod data_dir;

use std::process::ExitCode;

use clap::Parser as _;

fn main() -> ExitCode {
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
