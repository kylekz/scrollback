//! An installed scrollback's updates (PLAN §13, phase 7; dv's D9 and `updates.rs`): Velopack's
//! `UpdateManager` over the feed the build names (`SCROLLBACK_UPDATE_FEED`, which
//! `script/package` bakes in), handed to `sb_ui`'s updates service, which checks a few seconds
//! after start and every 4 hours. A check downloads a newer release (its delta when there is one)
//! into the installer's own packages folder ([`check`]); Restart to update then has the
//! installer's updater (`Update.exe`; `UpdateMac` in the bundle) wait for this process to quit,
//! apply the update and start scrollback again (`Releases::apply`). Applying ends every process
//! running from the install, so the service asks first while others run (`crate::processes`).
//!
//! **The feed** is read through Velopack's `AutoSource`: a release from the release workflow
//! names the repository on GitHub, which Velopack reads as its published releases (never drafts
//! or prereleases); a local trial names a folder of `vpk pack` output.
//!
//! **Off** ([`updater`]), and so never reaching the network, in a development build, under
//! automation, when `--data-dir` or `SCROLLBACK_DATA_DIR` names the data folder, and in a
//! packaged build the installer didn't put where it runs: `UpdateManager::new` fails without an
//! `Update.exe` above it (an app bundle around it, on macOS). Velopack writes only in the
//! install's packages folder (`%LOCALAPPDATA%\<pack id>\packages`; on macOS
//! `~/Library/Caches/velopack/<pack id>`), and keeps no log file: its `file-logging` feature is
//! off. Automation stands in a scripted [`Releases`] for it (`update_ready`).

use std::sync::Arc;

use gpui::App;
use sb_ui::services::updates::{Others, Real, on_its_own_thread};
use sb_ui::services::{Off, Updater};

use crate::data_dir::Source;

/// What updating needs from Velopack, apart so the decisions are tested without an install.
pub trait Releases: Send + Sync {
    /// The version downloaded before and not applied yet.
    fn pending(&self) -> Option<String>;
    /**
     * The feed's newest release when it is newer than this copy (remembered for
     * [`Releases::download`]). Blocks.
     */
    fn newer(&self) -> Result<Option<String>, String>;
    /// Downloads what [`Releases::newer`] found. Blocks.
    fn download(&self) -> Result<(), String>;
    /**
     * Has the installer's updater wait for this process to exit, apply the pending update and
     * start scrollback again, as the Start menu or the Dock would.
     */
    fn apply(&self) -> Result<(), String>;
}

/**
 * The version ready to restart into: one downloaded before, else the feed's newest when it is
 * newer, downloaded now. Blocks.
 */
pub fn check(releases: &dyn Releases) -> Result<Option<String>, String> {
    if let Some(pending) = releases.pending() {
        return Ok(Some(pending));
    }
    match releases.newer()? {
        Some(version) => {
            releases.download()?;
            Ok(Some(version))
        }
        None => Ok(None),
    }
}

/**
 * The updater over `releases`, `others` counting the processes applying would end; `then`
 * follows once the installer took the update (quitting, so that it can apply it).
 */
pub fn real(releases: Arc<dyn Releases>, others: Others, then: fn(&mut App)) -> Updater {
    let checked = releases.clone();
    Updater::Real(Real {
        check: Arc::new(move || {
            let releases = checked.clone();
            on_its_own_thread(move || check(releases.as_ref()))
        }),
        restart: Box::new(move || releases.apply()),
        others,
        then,
    })
}

/// Whether this run looks for updates, and with what (module docs).
pub fn updater(automation: bool, data_dir: Source) -> Updater {
    match off(sb_ui::version::is_packaged(), automation, data_dir) {
        Some(off) => Updater::Off(off),
        None => installed(),
    }
}

/**
 * Why a run that is `packaged` or not, driven by `automation` or not, with its data folder
 * chosen as `data_dir` says, looks for no updates; `None` when it may, if it is installed.
 */
fn off(packaged: bool, automation: bool, data_dir: Source) -> Option<Off> {
    if !packaged {
        Some(Off::Development)
    } else if automation {
        Some(Off::Automation)
    } else if data_dir != Source::Platform {
        Some(Off::DataDir)
    } else {
        None
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn installed() -> Updater {
    Updater::Off(Off::NotInstalled)
}

#[cfg(any(windows, target_os = "macos"))]
use velopack_releases::installed;

/// Velopack's side, where the `velopack` crate is built: Windows and macOS.
#[cfg(any(windows, target_os = "macos"))]
mod velopack_releases {
    use std::ffi::OsString;
    use std::sync::{Arc, Mutex, MutexGuard};

    use sb_ui::services::{Off, Updater};
    use velopack::sources::AutoSource;
    use velopack::{UpdateCheck, UpdateInfo, UpdateManager};

    use super::Releases;

    /// The feed `script/package` baked into this build; `None` in a development build.
    const BUILT_FEED: Option<&str> = option_env!("SCROLLBACK_UPDATE_FEED");

    /// [`Releases`] through Velopack's `UpdateManager`.
    struct Velopack {
        manager: UpdateManager,
        found: Mutex<Option<UpdateInfo>>,
    }

    impl Releases for Velopack {
        fn pending(&self) -> Option<String> {
            self.manager
                .get_update_pending_restart()
                .map(|asset| asset.Version)
        }

        fn newer(&self) -> Result<Option<String>, String> {
            match self
                .manager
                .check_for_updates()
                .map_err(|error| error.to_string())?
            {
                UpdateCheck::UpdateAvailable(update) => {
                    let version = update.TargetFullRelease.Version.clone();
                    *lock(&self.found) = Some(*update);
                    Ok(Some(version))
                }
                UpdateCheck::NoUpdateAvailable | UpdateCheck::RemoteIsEmpty => Ok(None),
            }
        }

        fn download(&self) -> Result<(), String> {
            let found = lock(&self.found).clone().ok_or("nothing to download")?;
            self.manager
                .download_updates(&found, None)
                .map_err(|error| error.to_string())
        }

        fn apply(&self) -> Result<(), String> {
            let pending = self
                .manager
                .get_update_pending_restart()
                .ok_or("the update isn't downloaded any more")?;
            self.manager
                .wait_exit_then_apply_updates(&pending, false, true, Vec::<OsString>::new())
                .map_err(|error| error.to_string())
        }
    }

    /// `mutex`'s value, also after a panic elsewhere left it poisoned (it holds plain data).
    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The updater of an installed copy, or why this one has none (module docs).
    pub fn installed() -> Updater {
        let Some(feed) = BUILT_FEED.filter(|feed| !feed.trim().is_empty()) else {
            log::info!("updates: off (this build names no feed)");
            return Updater::Off(Off::NotInstalled);
        };
        let Some(binaries) = crate::installer::binaries_dir() else {
            log::info!("updates: off (not an installed copy)");
            return Updater::Off(Off::NotInstalled);
        };
        let manager = match UpdateManager::new(AutoSource::new(feed), None, None) {
            Ok(manager) => manager,
            Err(error) => {
                log::info!("updates: off ({error})");
                return Updater::Off(Off::NotInstalled);
            }
        };
        log::info!(
            "updates: scrollback {} checks {feed}",
            manager.get_current_version_as_string()
        );
        super::real(
            Arc::new(Velopack {
                manager,
                found: Mutex::new(None),
            }),
            Box::new(move || crate::processes::others_in(&binaries)),
            |cx| cx.quit(),
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A test binary isn't installed: the manager can't be made, which turns updates off.
        #[test]
        fn a_build_that_isnt_installed_has_no_manager() {
            assert!(UpdateManager::new(AutoSource::new("/no/such/feed"), None, None).is_err());
            assert!(crate::installer::binaries_dir().is_none());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard};

    use pretty_assertions::assert_eq;

    use super::*;

    /// A feed and an install that only say what they're asked to.
    #[derive(Default)]
    struct Fake {
        pending: Mutex<Option<String>>,
        newer: Option<String>,
        fail: Option<String>,
        calls: Mutex<Vec<&'static str>>,
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    impl Releases for Fake {
        fn pending(&self) -> Option<String> {
            lock(&self.calls).push("pending");
            lock(&self.pending).clone()
        }
        fn newer(&self) -> Result<Option<String>, String> {
            lock(&self.calls).push("newer");
            match &self.fail {
                Some(error) => Err(error.clone()),
                None => Ok(self.newer.clone()),
            }
        }
        fn download(&self) -> Result<(), String> {
            lock(&self.calls).push("download");
            *lock(&self.pending) = self.newer.clone();
            Ok(())
        }
        fn apply(&self) -> Result<(), String> {
            lock(&self.calls).push("apply");
            Ok(())
        }
    }

    #[test]
    fn a_check_downloads_a_newer_release_or_reports_the_one_waiting() {
        let fake = Fake {
            newer: Some("0.2.0".into()),
            ..Fake::default()
        };
        assert_eq!(check(&fake), Ok(Some("0.2.0".into())));
        assert_eq!(*lock(&fake.calls), ["pending", "newer", "download"]);
        lock(&fake.calls).clear();
        assert_eq!(check(&fake), Ok(Some("0.2.0".into())));
        assert_eq!(*lock(&fake.calls), ["pending"], "no second download");

        assert_eq!(check(&Fake::default()), Ok(None));
        let offline = Fake {
            fail: Some("the feed is unreachable".into()),
            ..Fake::default()
        };
        assert_eq!(check(&offline), Err("the feed is unreachable".into()));
    }

    #[test]
    fn only_an_installed_release_in_the_platforms_data_folder_looks_for_updates() {
        for source in [Source::Platform, Source::Explicit, Source::Automation] {
            for automation in [false, true] {
                assert_eq!(off(false, automation, source), Some(Off::Development));
            }
            assert_eq!(off(true, true, source), Some(Off::Automation));
        }
        assert_eq!(off(true, false, Source::Explicit), Some(Off::DataDir));
        assert_eq!(off(true, false, Source::Platform), None);
        /* A test is a development build. */
        assert!(matches!(
            updater(false, Source::Platform),
            Updater::Off(Off::Development)
        ));
    }
}
