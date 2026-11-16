//! Velopack's start-up hooks, on Windows and macOS (PLAN §13, phase 7; dv's D9): [`run`] is the
//! first thing `main` does.
//!
//! On Windows the installer's `Update.exe` starts `scrollback.exe` with `--veloapp-install`,
//! `--veloapp-updated`, `--veloapp-obsolete` or `--veloapp-uninstall` as it installs, updates or
//! uninstalls the app; `VelopackApp` answers them and exits before anything else of scrollback
//! runs, with no window. scrollback has nothing of its own to do there: it puts nothing on PATH
//! and nothing in the registry, and an uninstall leaves the data folder, and the archive in it,
//! where they are: the install's folder is named by `script/package`'s pack id, never the data
//! folder's name (the tests hold it to that). macOS runs no hooks. An update
//! applies only from Restart to update (`crate::updates`), never by itself at start-up.
//!
//! Only a build `script/package` made hands its command line to Velopack: a development build
//! never does, so it never looks for an install or touches a file of Velopack's. A command line
//! that isn't all Unicode skips Velopack too, whose builder reads `env::args`, which panics on
//! one; the hooks' arguments are ASCII.

use std::ffi::OsString;

use velopack::VelopackApp;

/// Whether Velopack may read the command line `args` (module docs).
fn should_run(args: &[OsString]) -> bool {
    args.iter().all(|arg| arg.to_str().is_some())
}

/**
 * Hands this process's command line to a packaged build's Velopack first (module docs): a
 * hook runs and the process exits; anything else goes on.
 */
pub fn run() {
    if !sb_ui::version::is_packaged() || !should_run(&std::env::args_os().collect::<Vec<_>>()) {
        return;
    }
    VelopackApp::build().set_auto_apply_on_startup(false).run();
}

#[cfg(test)]
mod tests {
    use super::*;

    /**
     * On Windows the installer puts the app in `%LOCALAPPDATA%\<pack id>` and deletes that
     * whole folder when it uninstalls, so the pack id
     * `script/package` gives the release must never be the data folder's name, which sits
     * beside it.
     */
    #[test]
    fn the_install_folder_is_never_the_data_folder() {
        let package = include_str!("../../../script/package");
        let pack_id = package
            .lines()
            .find_map(|line| line.strip_prefix("pack_id="))
            .expect("script/package sets pack_id");
        assert!(!pack_id.is_empty());
        assert_ne!(
            pack_id.to_lowercase(),
            crate::data_dir::FOLDER.to_lowercase()
        );
    }

    #[test]
    fn a_unicode_command_line_reaches_velopack() {
        assert!(should_run(&[
            OsString::from("scrollback"),
            OsString::from("--veloapp-install"),
            OsString::from("0.1.0"),
        ]));
    }

    #[cfg(windows)]
    #[test]
    fn a_command_line_that_isnt_unicode_skips_velopack() {
        use std::os::windows::ffi::OsStringExt as _;
        /* A lone surrogate: a file name Windows allows and Rust's `env::args` panics on. */
        let odd = [
            OsString::from("scrollback.exe"),
            OsString::from_wide(&[0x0061, 0xD800]),
        ];
        assert!(!should_run(&odd));
    }
}
