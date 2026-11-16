//! The `updates` service: scrollback's own updates as the app shows them (PLAN §13, phase 7;
//! dv's D9). An installed copy's binary hands it an [`Updater`] (Velopack, the `scrollback`
//! crate's `updates`); the service checks a few seconds after start and then every 4 hours. A
//! check downloads a newer release in the background, and once one is ready **Restart to
//! update** ([`UpdatesService::restart`]) has the installer apply it and start scrollback again,
//! then quits as any quit does: passes stop and the owner's queued changes are written (the
//! paths editor's too), and the next start carries on where the passes left off.
//!
//! Every other run has [`Updater::Off`] with the reason: a development build, a packaged build
//! that isn't installed, a run whose data folder `--data-dir` names, or an automation run.
//! Nothing is checked and nothing reaches the network there.

use std::sync::Arc;
use std::time::Duration;

use gpui::{AppContext as _, Context, Task};

use crate::format;

/// How long after startup the first check waits, so it doesn't compete with opening the archive.
pub const FIRST_CHECK_AFTER: Duration = Duration::from_secs(10);

/// How often the feed is checked again.
pub const CHECK_EVERY: Duration = Duration::from_secs(4 * 60 * 60);

/**
 * A check of the feed, blocking: the version ready to restart into (downloaded now if it
 * wasn't), `None` when this copy is the newest, or why it couldn't tell.
 */
pub type Check = Arc<dyn Fn() -> Result<Option<String>, String> + Send + Sync>;

/**
 * Has the installer apply the downloaded update once this process has quit, and start
 * scrollback again; an error says why it couldn't. It doesn't quit the app itself.
 */
pub type Restart = Box<dyn Fn() -> Result<(), String>>;

/// Why a run looks for no updates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Off {
    /// Built by cargo, not by `script/package`: no release and no feed.
    Development,
    /// A packaged build that the installer didn't put where it is.
    NotInstalled,
    /// `--data-dir` or `SCROLLBACK_DATA_DIR` names the data folder: a scratch run.
    DataDir,
    /// Driven by automation (PLAN §12).
    Automation,
}

impl Off {
    /// The key automation's `state` reports.
    pub fn key(self) -> &'static str {
        match self {
            Self::Development => "development",
            Self::NotInstalled => "not_installed",
            Self::DataDir => "data_dir",
            Self::Automation => "automation",
        }
    }
}

/// What checks for updates and restarts into one.
pub enum Updater {
    Off(Off),
    /// An installed copy's: the binary's Velopack.
    Real {
        check: Check,
        restart: Restart,
    },
}

/// What a check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckResult {
    /// This copy is the newest.
    UpToDate,
    /// The version downloaded and ready.
    Ready(String),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastCheck {
    /// When it finished, in Unix milliseconds.
    pub at: i64,
    pub result: CheckResult,
}

/// The update service (module docs).
pub struct UpdatesService {
    updater: Updater,
    /// The version downloaded and ready to restart into.
    ready: Option<String>,
    last_check: Option<LastCheck>,
    /// The check in flight.
    checking: Option<Task<()>>,
    /// Restart to update was clicked and the installer took the update: the app is quitting.
    restarting: bool,
    /// Why the last restart couldn't be made, until another is tried.
    failure: Option<String>,
    /// The timer behind the checks.
    _schedule: Option<Task<()>>,
}

impl UpdatesService {
    /**
     * The service over `updater`: a real one is checked [`FIRST_CHECK_AFTER`] from now, then
     * every [`CHECK_EVERY`].
     */
    pub fn new(updater: Updater, cx: &mut Context<Self>) -> Self {
        let schedule = matches!(updater, Updater::Real { .. }).then(|| {
            cx.spawn(async move |this, cx| {
                let mut wait = FIRST_CHECK_AFTER;
                loop {
                    cx.background_executor().timer(wait).await;
                    wait = CHECK_EVERY;
                    if this.update(cx, |this, cx| this.check_now(cx)).is_err() {
                        return;
                    }
                }
            })
        });
        Self {
            updater,
            ready: None,
            last_check: None,
            checking: None,
            restarting: false,
            failure: None,
            _schedule: schedule,
        }
    }

    /// Why this run looks for no updates, or `None` when it does.
    pub fn off(&self) -> Option<Off> {
        match self.updater {
            Updater::Off(off) => Some(off),
            Updater::Real { .. } => None,
        }
    }

    /// The version a restart applies, when one is ready.
    pub fn ready(&self) -> Option<&str> {
        self.ready.as_deref()
    }

    pub fn last_check(&self) -> Option<&LastCheck> {
        self.last_check.as_ref()
    }

    pub fn is_checking(&self) -> bool {
        self.checking.is_some()
    }

    pub fn is_restarting(&self) -> bool {
        self.restarting
    }

    /// Why the last restart couldn't be made.
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// Checks the feed now, unless a check is going or this run has no updater.
    pub fn check_now(&mut self, cx: &mut Context<Self>) {
        let Updater::Real { check, .. } = &self.updater else {
            return;
        };
        if self.checking.is_some() {
            return;
        }
        let check = check.clone();
        let checking = cx.background_spawn(async move { check() });
        self.checking = Some(cx.spawn(async move |this, cx| {
            let result = checking.await;
            this.update(cx, |this, cx| {
                this.checking = None;
                this.checked(result, cx);
            })
            .ok();
        }));
        cx.notify();
    }

    fn checked(&mut self, result: Result<Option<String>, String>, cx: &mut Context<Self>) {
        let result = match result {
            Ok(Some(version)) => {
                log::info!("updates: scrollback {version} is ready");
                self.ready = Some(version.clone());
                CheckResult::Ready(version)
            }
            Ok(None) => CheckResult::UpToDate,
            Err(error) => {
                log::warn!("updates: {error}");
                CheckResult::Failed(error)
            }
        };
        self.last_check = Some(LastCheck {
            at: format::now(),
            result,
        });
        cx.notify();
    }

    /**
     * Restart to update, or its Try again: once the click is handled (gpui can't quit from
     * inside an update), the installer takes the update and the app quits. Nothing without an
     * update ready, or while a restart is under way.
     */
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        if self.ready.is_none() || self.restarting {
            return;
        }
        self.restarting = true;
        self.failure = None;
        cx.notify();
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            if let Ok(Ok(())) = this.update(cx, |this, cx| this.hand_over(cx)) {
                cx.quit();
            }
        });
    }

    /// Hands the update to the installer; a failure keeps the app running and says why.
    fn hand_over(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let result = match &self.updater {
            Updater::Off(_) => Err("this scrollback has no updater".to_string()),
            Updater::Real { restart, .. } => restart(),
        };
        if let Err(error) = &result {
            log::error!(
                "updates: couldn't restart into scrollback {}: {error}",
                self.ready.as_deref().unwrap_or("?")
            );
            self.restarting = false;
            self.failure = Some(error.clone());
            cx.notify();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gpui::{Entity, TestAppContext};
    use pretty_assertions::assert_eq;

    use super::*;

    fn real(cx: &mut TestAppContext, check: Check, restart: Restart) -> Entity<UpdatesService> {
        cx.new(|cx| UpdatesService::new(Updater::Real { check, restart }, cx))
    }

    #[gpui::test]
    fn a_run_without_an_updater_never_checks(cx: &mut TestAppContext) {
        for off in [
            Off::Development,
            Off::NotInstalled,
            Off::DataDir,
            Off::Automation,
        ] {
            let updates = cx.new(|cx| UpdatesService::new(Updater::Off(off), cx));
            cx.executor().advance_clock(FIRST_CHECK_AFTER + CHECK_EVERY);
            cx.run_until_parked();
            updates.update(cx, |updates, cx| {
                updates.check_now(cx);
                assert!(!updates.is_checking());
                updates.restart(cx);
                assert!(!updates.is_restarting(), "nothing is ready");
            });
            updates.read_with(cx, |updates, _| {
                assert_eq!(updates.off(), Some(off));
                assert_eq!(updates.last_check(), None);
            });
        }
    }

    #[gpui::test]
    fn checks_run_after_start_then_on_schedule(cx: &mut TestAppContext) {
        let checks = Arc::new(AtomicUsize::new(0));
        let counted = checks.clone();
        let updates = real(
            cx,
            Arc::new(move || match counted.fetch_add(1, Ordering::SeqCst) {
                0 => Ok(None),
                1 => Err("the feed is unreachable".into()),
                _ => Ok(Some("0.2.0".into())),
            }),
            Box::new(|| Ok(())),
        );
        cx.run_until_parked();
        assert_eq!(checks.load(Ordering::SeqCst), 0, "not at once");
        cx.executor().advance_clock(FIRST_CHECK_AFTER);
        cx.run_until_parked();
        updates.read_with(cx, |updates, _| {
            assert_eq!(updates.off(), None);
            assert_eq!(
                updates.last_check().map(|check| &check.result),
                Some(&CheckResult::UpToDate)
            );
            assert_eq!(updates.ready(), None);
        });
        cx.executor().advance_clock(CHECK_EVERY);
        cx.run_until_parked();
        updates.read_with(cx, |updates, _| {
            assert_eq!(
                updates.last_check().map(|check| &check.result),
                Some(&CheckResult::Failed("the feed is unreachable".into()))
            );
        });
        cx.executor().advance_clock(CHECK_EVERY);
        cx.run_until_parked();
        updates.read_with(cx, |updates, _| {
            assert_eq!(updates.ready(), Some("0.2.0"));
            assert!(!updates.is_checking());
        });
        assert_eq!(checks.load(Ordering::SeqCst), 3);
    }

    #[gpui::test]
    fn a_restart_hands_the_update_over_once(cx: &mut TestAppContext) {
        let handed = Rc::new(Cell::new(0));
        let counted = handed.clone();
        let updates = real(
            cx,
            Arc::new(|| Ok(Some("0.2.0".into()))),
            Box::new(move || {
                counted.set(counted.get() + 1);
                Ok(())
            }),
        );
        cx.executor().advance_clock(FIRST_CHECK_AFTER);
        cx.run_until_parked();
        updates.update(cx, |updates, cx| {
            updates.restart(cx);
            updates.restart(cx);
            assert!(updates.is_restarting());
        });
        cx.run_until_parked();
        assert_eq!(handed.get(), 1);
        updates.read_with(cx, |updates, _| assert_eq!(updates.failure(), None));
    }

    #[gpui::test]
    fn a_restart_that_fails_says_why_and_can_be_tried_again(cx: &mut TestAppContext) {
        let tries = Rc::new(Cell::new(0));
        let counted = tries.clone();
        let updates = real(
            cx,
            Arc::new(|| Ok(Some("0.2.0".into()))),
            Box::new(move || {
                counted.set(counted.get() + 1);
                match counted.get() {
                    1 => Err("the installer isn't there".into()),
                    _ => Ok(()),
                }
            }),
        );
        cx.executor().advance_clock(FIRST_CHECK_AFTER);
        cx.run_until_parked();
        updates.update(cx, |updates, cx| updates.restart(cx));
        cx.run_until_parked();
        updates.update(cx, |updates, cx| {
            assert_eq!(updates.failure(), Some("the installer isn't there"));
            assert!(!updates.is_restarting());
            assert_eq!(updates.ready(), Some("0.2.0"), "still ready");
            updates.restart(cx);
        });
        cx.run_until_parked();
        assert_eq!(tries.get(), 2);
        updates.read_with(cx, |updates, _| assert_eq!(updates.failure(), None));
    }
}
