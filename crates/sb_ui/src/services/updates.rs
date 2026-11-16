//! The `updates` service: scrollback's own updates as the app shows them (PLAN §13, phase 7;
//! dv's D9). An installed copy's binary hands it an [`Updater::Real`] (Velopack, the
//! `scrollback` crate's `updates`); the service checks a few seconds after start and then every
//! 4 hours. A check downloads a newer release in the background; Settings then shows it ready
//! with **Restart to update** (`crate::update_line`).
//!
//! **A check that doesn't answer** (the feed's HTTP requests have no timeout of their own) is
//! given up after [`CHECK_TIMEOUT`]: it is recorded as failed and the next round tries again.
//! An installed copy's check runs on a thread of its own ([`on_its_own_thread`]), so one that
//! never returns holds nothing of the app's.
//!
//! **Restart to update** ([`UpdatesService::restart`]) asks before anything is lost. Applying
//! an update ends every scrollback process running from the install, so while others run it
//! says how many and waits for **Restart anyway** or **Not now**; then, while the paths editor
//! has changes to save, the library asks about them as Quit does (`Leave::Restart`). Only after
//! those answers does the installer take the update ([`UpdatesService::hand_over`]), and the
//! app quits as any quit does: passes stop and the owner's queued changes are written, and the
//! next start carries on where the passes left off.
//!
//! Every other run has [`Updater::Off`] with the reason: a development build, a packaged build
//! that isn't installed, a run whose data folder `--data-dir` names, or an automation run.
//! Nothing is checked and nothing reaches the network there.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use gpui::{App, Context, Task};

use crate::format;
use crate::services::{ActiveServices as _, Leave};

/// How long after startup the first check waits, so it doesn't compete with opening the archive.
pub const FIRST_CHECK_AFTER: Duration = Duration::from_secs(10);

/// How often the feed is checked again.
pub const CHECK_EVERY: Duration = Duration::from_secs(4 * 60 * 60);

/// How long a check may go without an answer before it is given up (module docs).
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(2 * 60);

/// What a check that was given up records.
pub const NO_ANSWER: &str = "no answer from the update feed";

/**
 * A check of the feed under way: the version ready to restart into (downloaded now if it
 * wasn't), `None` when this copy is the newest, or why it couldn't tell.
 */
pub type Checking = Pin<Box<dyn Future<Output = Result<Option<String>, String>> + Send>>;

/// Starts a check of the feed.
pub type Check = Arc<dyn Fn() -> Checking + Send + Sync>;

/**
 * The blocking `check` on a thread of its own, answered through a channel: one that never
 * returns holds no thread of the app's executor.
 */
pub fn on_its_own_thread(
    check: impl FnOnce() -> Result<Option<String>, String> + Send + 'static,
) -> Checking {
    let (answer, answered) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = answer.send_blocking(check());
    });
    Box::pin(async move {
        answered
            .recv()
            .await
            .unwrap_or_else(|_| Err("the check ended without an answer".to_string()))
    })
}

/**
 * Has the installer apply the downloaded update once this process has quit, and start
 * scrollback again; an error says why it couldn't. It doesn't quit the app itself.
 */
pub type Restart = Box<dyn Fn() -> Result<(), String>>;

/// How many other scrollback processes run from the install: applying the update ends them.
pub type Others = Box<dyn Fn() -> usize>;

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

/// An installed copy's updater: the binary's Velopack, or automation's stand-in for it.
pub struct Real {
    pub check: Check,
    pub restart: Restart,
    pub others: Others,
    /**
     * What follows once the installer took the update: quitting, so that it can apply it. A
     * scripted run's stand-in stays open.
     */
    pub then: fn(&mut App),
}

/// What checks for updates and restarts into one.
pub enum Updater {
    Off(Off),
    Real(Real),
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

/// An updater's state, which only an installed copy has.
struct On {
    updater: Real,
    /// The version downloaded and ready to restart into.
    ready: Option<String>,
    last_check: Option<LastCheck>,
    /// The check in flight, and the round it is.
    checking: Option<(u64, Task<()>)>,
    /// The rounds started, so that an answer after the watchdog gave up is dropped.
    rounds: u64,
    /// How many other processes Restart to update would end, while it asks about them.
    confirm: Option<usize>,
    /// The installer is taking the update: the app is about to quit.
    restarting: bool,
    /// The installer took the update (a scripted run's stand-in stays open after it).
    restarted: bool,
    /// Why the last restart couldn't be made, until another is tried.
    failure: Option<String>,
    /// The timer behind the checks.
    _schedule: Task<()>,
}

enum State {
    Off(Off),
    On(Box<On>),
}

/// The update service (module docs).
pub struct UpdatesService {
    state: State,
}

impl UpdatesService {
    /**
     * The service over `updater`: a real one is checked [`FIRST_CHECK_AFTER`] from now, then
     * every [`CHECK_EVERY`].
     */
    pub fn new(updater: Updater, cx: &mut Context<Self>) -> Self {
        Self {
            state: Self::state(updater, cx),
        }
    }

    fn state(updater: Updater, cx: &mut Context<Self>) -> State {
        match updater {
            Updater::Off(off) => State::Off(off),
            Updater::Real(updater) => State::On(Box::new(On {
                updater,
                ready: None,
                last_check: None,
                checking: None,
                rounds: 0,
                confirm: None,
                restarting: false,
                restarted: false,
                failure: None,
                _schedule: cx.spawn(async move |this, cx| {
                    let mut wait = FIRST_CHECK_AFTER;
                    loop {
                        cx.background_executor().timer(wait).await;
                        wait = CHECK_EVERY;
                        if this.update(cx, |this, cx| this.check_now(cx)).is_err() {
                            return;
                        }
                    }
                }),
            })),
        }
    }

    /**
     * Puts `updater` in place of this one, with nothing checked yet: automation's stand-in
     * (`update_ready`), and tests.
     */
    #[cfg(any(test, feature = "automation"))]
    pub fn set_updater(&mut self, updater: Updater, cx: &mut Context<Self>) {
        self.state = Self::state(updater, cx);
        cx.notify();
    }

    fn on(&self) -> Option<&On> {
        match &self.state {
            State::On(on) => Some(on),
            State::Off(_) => None,
        }
    }

    fn on_mut(&mut self) -> Option<&mut On> {
        match &mut self.state {
            State::On(on) => Some(on),
            State::Off(_) => None,
        }
    }

    /// Why this run looks for no updates, or `None` when it does.
    pub fn off(&self) -> Option<Off> {
        match self.state {
            State::Off(off) => Some(off),
            State::On(_) => None,
        }
    }

    /// The version a restart applies, when one is ready.
    pub fn ready(&self) -> Option<&str> {
        self.on().and_then(|on| on.ready.as_deref())
    }

    pub fn last_check(&self) -> Option<&LastCheck> {
        self.on().and_then(|on| on.last_check.as_ref())
    }

    pub fn is_checking(&self) -> bool {
        self.on().is_some_and(|on| on.checking.is_some())
    }

    /// How many other processes Restart to update would end, while it asks about them.
    pub fn confirm(&self) -> Option<usize> {
        self.on().and_then(|on| on.confirm)
    }

    pub fn is_restarting(&self) -> bool {
        self.on().is_some_and(|on| on.restarting)
    }

    /// The installer took the update.
    pub fn is_restarted(&self) -> bool {
        self.on().is_some_and(|on| on.restarted)
    }

    /// Why the last restart couldn't be made.
    pub fn failure(&self) -> Option<&str> {
        self.on().and_then(|on| on.failure.as_deref())
    }

    /// Checks the feed now, unless a check is going or this run has no updater.
    pub fn check_now(&mut self, cx: &mut Context<Self>) {
        let Some(on) = self.on_mut() else {
            return;
        };
        if on.checking.is_some() {
            return;
        }
        on.rounds += 1;
        let round = on.rounds;
        let checking = (on.updater.check)();
        let timer = cx.background_executor().timer(CHECK_TIMEOUT);
        let watched = cx.spawn(async move |this, cx| {
            let result = answer_or_timeout(checking, timer).await;
            this.update(cx, |this, cx| this.checked(round, result, cx))
                .ok();
        });
        on.checking = Some((round, watched));
        cx.notify();
    }

    /// The answer to the check of `round`, or `None` when it didn't come in time.
    fn checked(
        &mut self,
        round: u64,
        result: Option<Result<Option<String>, String>>,
        cx: &mut Context<Self>,
    ) {
        let Some(on) = self.on_mut() else {
            return;
        };
        if on.checking.as_ref().map(|(at, _)| *at) != Some(round) {
            return;
        }
        on.checking = None;
        let result = match result.unwrap_or_else(|| Err(NO_ANSWER.to_string())) {
            Ok(Some(version)) => {
                log::info!("updates: scrollback {version} is ready");
                on.ready = Some(version.clone());
                CheckResult::Ready(version)
            }
            Ok(None) => CheckResult::UpToDate,
            Err(error) => {
                log::warn!("updates: {error}");
                CheckResult::Failed(error)
            }
        };
        on.last_check = Some(LastCheck {
            at: format::now(),
            result,
        });
        cx.notify();
    }

    /**
     * Restart to update, or its Try again (module docs): asks first while other processes run
     * from the install, then goes on as [`Self::confirm_restart`] does. Nothing without an
     * update ready, or while a restart is under way.
     */
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        let Some(on) = self.on_mut() else {
            return;
        };
        if on.ready.is_none() || on.restarting || on.confirm.is_some() {
            return;
        }
        let others = (on.updater.others)();
        if others > 0 {
            on.confirm = Some(others);
            cx.notify();
            return;
        }
        self.leave(cx);
    }

    /// Restart anyway, while the others are asked about.
    pub fn confirm_restart(&mut self, cx: &mut Context<Self>) {
        if self.on_mut().and_then(|on| on.confirm.take()).is_some() {
            self.leave(cx);
        }
    }

    /// Not now: nothing ends, and the update stays ready.
    pub fn cancel_restart(&mut self, cx: &mut Context<Self>) {
        if self.on_mut().and_then(|on| on.confirm.take()).is_some() {
            cx.notify();
        }
    }

    /// The paths editor's changes are asked about first, as Quit does; otherwise on at once.
    fn leave(&mut self, cx: &mut Context<Self>) {
        let library = cx.services().library.clone();
        if library.read(cx).held().is_some() {
            library.update(cx, |library, cx| library.ask_before(Leave::Restart, cx));
            cx.notify();
        } else {
            self.hand_over(cx);
        }
    }

    /**
     * Hands the update to the installer once this update is done (gpui can't quit from inside
     * one), then quits: after Restart to update and whatever it asked (module docs).
     */
    pub fn hand_over(&mut self, cx: &mut Context<Self>) {
        let Some(on) = self.on_mut() else {
            return;
        };
        if on.ready.is_none() || on.restarting {
            return;
        }
        on.restarting = true;
        on.failure = None;
        cx.notify();
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            if let Ok(Some(then)) = this.update(cx, |this, cx| this.run_restart(cx)) {
                then(cx);
            }
        });
    }

    /// The installer takes the update: what follows it, or, when it can't, why, kept to show.
    fn run_restart(&mut self, cx: &mut Context<Self>) -> Option<fn(&mut App)> {
        let on = self.on_mut()?;
        let result = (on.updater.restart)();
        cx.notify();
        match result {
            Ok(()) => {
                on.restarted = true;
                Some(on.updater.then)
            }
            Err(error) => {
                log::error!(
                    "updates: couldn't restart into scrollback {}: {error}",
                    on.ready.as_deref().unwrap_or_default()
                );
                on.restarting = false;
                on.failure = Some(error);
                None
            }
        }
    }
}

/// `answer`'s value, or `None` once `timer` has fired first.
async fn answer_or_timeout<T>(
    answer: impl Future<Output = T>,
    timer: impl Future<Output = ()>,
) -> Option<T> {
    let mut answer = std::pin::pin!(answer);
    let mut timer = std::pin::pin!(timer);
    std::future::poll_fn(move |task| {
        if let std::task::Poll::Ready(answer) = answer.as_mut().poll(task) {
            return std::task::Poll::Ready(Some(answer));
        }
        timer.as_mut().poll(task).map(|()| None)
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gpui::{Entity, TestAppContext};
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::test_support::{init, machine};

    /// The app's updates service, its updater put in place.
    fn service(updater: Updater, cx: &mut TestAppContext) -> Entity<UpdatesService> {
        init(machine(), cx);
        let updates = cx.update(|cx| cx.services().updates.clone());
        updates.update(cx, |updates, cx| updates.set_updater(updater, cx));
        updates
    }

    /// A check that answers at once.
    fn answers(answer: Result<Option<String>, String>) -> Checking {
        Box::pin(async move { answer })
    }

    fn real(check: Check, restart: Restart, others: usize) -> Updater {
        Updater::Real(Real {
            check,
            restart,
            others: Box::new(move || others),
            then: |_| {},
        })
    }

    #[gpui::test]
    fn a_run_without_an_updater_never_checks(cx: &mut TestAppContext) {
        let updates = service(Updater::Off(Off::DataDir), cx);
        cx.executor().advance_clock(FIRST_CHECK_AFTER + CHECK_EVERY);
        cx.run_until_parked();
        updates.update(cx, |updates, cx| {
            updates.check_now(cx);
            assert!(!updates.is_checking());
            updates.restart(cx);
            assert!(!updates.is_restarting(), "nothing is ready");
            assert_eq!(updates.off(), Some(Off::DataDir));
            assert_eq!(updates.last_check(), None);
        });
    }

    #[gpui::test]
    fn checks_run_after_start_then_on_schedule(cx: &mut TestAppContext) {
        let checks = Arc::new(AtomicUsize::new(0));
        let counted = checks.clone();
        let updates = service(
            real(
                Arc::new(move || {
                    answers(match counted.fetch_add(1, Ordering::SeqCst) {
                        0 => Ok(None),
                        1 => Err("the feed is unreachable".into()),
                        _ => Ok(Some("0.2.0".into())),
                    })
                }),
                Box::new(|| Ok(())),
                0,
            ),
            cx,
        );
        cx.run_until_parked();
        assert_eq!(checks.load(Ordering::SeqCst), 0, "not at once");
        let result = |cx: &mut TestAppContext| {
            cx.run_until_parked();
            updates.read_with(cx, |updates, _| {
                updates.last_check().map(|check| check.result.clone())
            })
        };
        cx.executor().advance_clock(FIRST_CHECK_AFTER);
        assert_eq!(result(cx), Some(CheckResult::UpToDate));
        cx.executor().advance_clock(CHECK_EVERY);
        assert_eq!(
            result(cx),
            Some(CheckResult::Failed("the feed is unreachable".into()))
        );
        cx.executor().advance_clock(CHECK_EVERY);
        assert_eq!(result(cx), Some(CheckResult::Ready("0.2.0".into())));
        updates.read_with(cx, |updates, _| assert_eq!(updates.ready(), Some("0.2.0")));
        assert_eq!(checks.load(Ordering::SeqCst), 3);
    }

    #[gpui::test]
    fn a_check_without_an_answer_is_given_up_and_tried_again(cx: &mut TestAppContext) {
        let first = AtomicUsize::new(0);
        let updates = service(
            real(
                Arc::new(move || {
                    /* The first check never answers; the next does. */
                    if first.fetch_add(1, Ordering::SeqCst) == 0 {
                        Box::pin(std::future::pending())
                    } else {
                        answers(Ok(Some("0.2.0".into())))
                    }
                }),
                Box::new(|| Ok(())),
                0,
            ),
            cx,
        );
        cx.executor().advance_clock(FIRST_CHECK_AFTER);
        cx.run_until_parked();
        updates.read_with(cx, |updates, _| assert!(updates.is_checking()));
        cx.executor().advance_clock(CHECK_TIMEOUT);
        cx.run_until_parked();
        updates.read_with(cx, |updates, _| {
            assert!(!updates.is_checking());
            assert_eq!(
                updates.last_check().map(|check| check.result.clone()),
                Some(CheckResult::Failed(NO_ANSWER.into()))
            );
        });
        updates.update(cx, |updates, cx| updates.check_now(cx));
        cx.run_until_parked();
        updates.read_with(cx, |updates, _| assert_eq!(updates.ready(), Some("0.2.0")));
    }

    /// An updater whose update is ready, its restarts counted, `others` running beside it.
    fn ready(
        others: usize,
        fail_first: bool,
        cx: &mut TestAppContext,
    ) -> (Entity<UpdatesService>, Arc<AtomicUsize>) {
        let tries = Arc::new(AtomicUsize::new(0));
        let counted = tries.clone();
        let updates = service(
            real(
                Arc::new(|| answers(Ok(Some("0.2.0".into())))),
                Box::new(move || match counted.fetch_add(1, Ordering::SeqCst) {
                    0 if fail_first => Err("the installer isn't there".into()),
                    _ => Ok(()),
                }),
                others,
            ),
            cx,
        );
        cx.executor().advance_clock(FIRST_CHECK_AFTER);
        cx.run_until_parked();
        (updates, tries)
    }

    #[gpui::test]
    fn a_restart_hands_the_update_over_once(cx: &mut TestAppContext) {
        let (updates, tries) = ready(0, false, cx);
        updates.update(cx, |updates, cx| {
            updates.restart(cx);
            updates.restart(cx);
            assert!(updates.is_restarting());
        });
        cx.run_until_parked();
        assert_eq!(tries.load(Ordering::SeqCst), 1);
        updates.read_with(cx, |updates, _| {
            assert!(updates.is_restarted());
            assert_eq!(updates.failure(), None);
        });
    }

    #[gpui::test]
    fn a_restart_that_fails_says_why_and_can_be_tried_again(cx: &mut TestAppContext) {
        let (updates, tries) = ready(0, true, cx);
        updates.update(cx, |updates, cx| updates.restart(cx));
        cx.run_until_parked();
        updates.update(cx, |updates, cx| {
            assert_eq!(updates.failure(), Some("the installer isn't there"));
            assert!(!updates.is_restarting());
            assert_eq!(updates.ready(), Some("0.2.0"), "still ready");
            updates.restart(cx);
        });
        cx.run_until_parked();
        assert_eq!(tries.load(Ordering::SeqCst), 2);
        updates.read_with(cx, |updates, _| assert_eq!(updates.failure(), None));
    }

    #[gpui::test]
    fn other_processes_are_asked_about_first(cx: &mut TestAppContext) {
        let (updates, tries) = ready(2, false, cx);
        updates.update(cx, |updates, cx| {
            updates.restart(cx);
            assert_eq!(updates.confirm(), Some(2));
            updates.cancel_restart(cx);
            assert_eq!(updates.confirm(), None);
            assert_eq!(updates.ready(), Some("0.2.0"));
        });
        cx.run_until_parked();
        assert_eq!(tries.load(Ordering::SeqCst), 0, "Not now ends nothing");
        updates.update(cx, |updates, cx| {
            updates.restart(cx);
            updates.confirm_restart(cx);
        });
        cx.run_until_parked();
        assert_eq!(tries.load(Ordering::SeqCst), 1);
    }
}
