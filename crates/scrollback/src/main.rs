//! `scrollback`: the app (docs/PLAN.md §3). One process: it opens the window itself and never
//! detaches. Everything it shows is `sb_ui`'s.

mod app;
#[cfg(feature = "automation")]
mod automation;
mod cli;
mod data_dir;

use std::process::ExitCode;

use clap::Parser as _;

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
