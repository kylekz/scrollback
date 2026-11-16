//! Velopack's start-up hooks, on Windows and macOS (PLAN §13, phase 7; dv's D9): [`run`] is the
//! first thing `main` does.
//!
//! On Windows the installer's `Update.exe` starts `scrollback.exe` with `--veloapp-install`,
//! `--veloapp-updated`, `--veloapp-obsolete` or `--veloapp-uninstall` as it installs, updates or
//! uninstalls the app; `VelopackApp` answers them and exits before anything else of scrollback
//! runs, with no window. scrollback has nothing of its own to do there: it puts nothing on PATH
//! and nothing in the registry, and an uninstall leaves the data folder, and the archive in it,
//! where they are. The install's folder is named by the pack id (`resources/pack-id`, which
//! `script/package` reads), never the data folder's name (the tests hold it to that), and no
//! data folder or archive may be inside it ([`root`]). macOS runs no hooks. An update applies
//! only from Restart to update (`crate::updates`), never by itself at start-up.
//!
//! Only a build `script/package` made hands its command line to Velopack: a development build
//! never does, so it never looks for an install or touches a file of Velopack's. A command line
//! that isn't all Unicode skips Velopack too, whose builder reads `env::args`, which panics on
//! one; the hooks' arguments are ASCII.

use std::ffi::OsString;
use std::path::PathBuf;

use velopack::VelopackApp;
use velopack::locator::{LocationContext, VelopackLocator, auto_locate_app_manifest};

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

/// The installer's record of this copy, when a packaged build is installed.
fn located() -> Option<VelopackLocator> {
    if !sb_ui::version::is_packaged() {
        return None;
    }
    auto_locate_app_manifest(LocationContext::FromCurrentExe).ok()
}

/**
 * The folder the installer owns and its uninstaller removes, in an installed copy:
 * `%LOCALAPPDATA%\<pack id>` on Windows, the app bundle on macOS.
 */
pub fn root() -> Option<PathBuf> {
    located().map(|locator| locator.get_root_dir())
}

/**
 * The folder the installed copy's executables run from, which an update replaces: Velopack's
 * `current` on Windows, the bundle's `Contents/MacOS` on macOS.
 */
pub fn binaries_dir() -> Option<PathBuf> {
    located().map(|locator| locator.get_current_bin_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pack id `script/package` gives the release.
    const PACK_ID: &str = include_str!("../resources/pack-id");

    /// A folder's name as Windows matches it: case aside, and without trailing dots or spaces.
    fn normalised(name: &str) -> String {
        name.trim()
            .trim_matches(['"', '\''])
            .trim_end_matches(['.', ' '])
            .to_lowercase()
    }

    /**
     * On Windows the installer puts the app in `%LOCALAPPDATA%\<pack id>` and deletes that
     * whole folder when it uninstalls, so the pack id must never be the data folder's name,
     * which sits beside it.
     */
    #[test]
    fn the_install_folder_is_never_the_data_folder() {
        let pack_id = normalised(PACK_ID);
        assert!(!pack_id.is_empty());
        assert_eq!(
            pack_id,
            PACK_ID.trim(),
            "resources/pack-id holds the id alone"
        );
        assert_ne!(pack_id, normalised(crate::data_dir::FOLDER));
        assert_eq!(normalised(" \"Scrollback.. \"\n"), "scrollback");
        /* script/package packs with the same file. */
        assert!(
            include_str!("../../../script/package")
                .contains("pack_id=\"$(tr -d '[:space:]' < crates/scrollback/resources/pack-id)\"")
        );
    }

    #[test]
    fn a_development_build_has_no_install() {
        assert_eq!(root(), None);
        assert_eq!(binaries_dir(), None);
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
