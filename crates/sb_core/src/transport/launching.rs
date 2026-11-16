//! Whether this process may start a launcher (`wsl.exe`, `ssh` or a command of its own). A
//! scripted run reaches its distros and hosts through stand-ins and must never reach the
//! machine's own (PLAN §12): it calls [`refuse_launchers`] first, and from then on an attempt
//! to start one starts nothing, fails, and is logged as an error that names it after
//! [`LAUNCH_REFUSED`], which the e2e runner fails a scenario on.

use std::sync::atomic::{AtomicBool, Ordering};

use super::TransportError;

/// What the error a refused launcher is logged and failed with begins with.
pub const LAUNCH_REFUSED: &str = "a launcher was refused in a run that starts none";

static REFUSED: AtomicBool = AtomicBool::new(false);

/// Refuses every launcher from now on, for the life of the process.
pub fn refuse_launchers() {
    REFUSED.store(true, Ordering::SeqCst);
}

/// Before `command` is started: an error, logged, once launchers are refused.
pub(super) fn may_start(command: &str) -> Result<(), TransportError> {
    if !REFUSED.load(Ordering::SeqCst) {
        return Ok(());
    }
    let refused = format!("{LAUNCH_REFUSED}: {command}");
    log::error!("{refused}");
    Err(TransportError::Failed(refused))
}
