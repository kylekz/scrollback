//! This build's version and, under it, a line on its updates (PLAN §13, phase 7), at the foot of
//! Settings: "scrollback 0.2.0", then what the updates service
//! ([`crate::services::updates`]) last found, with **Restart to update** once a newer release
//! is downloaded, and its question while other scrollback processes would end. A development
//! build shows the version alone, which says it is one; any other run that looks for no
//! updates says why.

use gpui::{
    AnyElement, App, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div,
};

use crate::automation::AutomationTargetExt as _;
use crate::components::{ButtonKind, button};
use crate::format;
use crate::services::{ActiveServices as _, CheckResult, Off, UpdatesService};
use crate::theme::{ActiveTheme as _, StyledText as _};
use crate::version;

/// `scrollback 0.2.0`, or `scrollback 0.1.0 (development build)`.
pub fn version_line() -> String {
    format!("scrollback {}", version::text())
}

/// What a button beside the line does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Restart to update, or Try again after a restart that couldn't be made.
    Restart,
    /// Restart anyway, while other processes would end.
    Confirm,
    /// Not now.
    Cancel,
}

impl Action {
    /// The button's automation target and debug selector.
    fn target(self) -> &'static str {
        match self {
            Self::Restart => "settings/restart-to-update",
            Self::Confirm => "settings/restart-anyway",
            Self::Cancel => "settings/not-now",
        }
    }
}

/// The line under the version, and the buttons beside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateLine {
    pub text: String,
    pub actions: Vec<(&'static str, Action)>,
}

impl UpdateLine {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            actions: Vec::new(),
        }
    }
}

/// What `updates` says under the version, seen at `now` (Unix ms); `None` for a development build.
pub fn update_line(updates: &UpdatesService, now: i64) -> Option<UpdateLine> {
    if let Some(off) = updates.off() {
        return match off {
            Off::Development => None,
            Off::NotInstalled => Some(UpdateLine::plain(
                "Updates are off: this copy wasn't installed by scrollback's installer.",
            )),
            Off::DataDir => Some(UpdateLine::plain(
                "Updates are off while --data-dir or SCROLLBACK_DATA_DIR names the data folder.",
            )),
            Off::Automation => Some(UpdateLine::plain("Updates are off in a scripted run.")),
        };
    }
    if let Some(ready) = updates.ready() {
        return Some(if updates.is_restarting() {
            UpdateLine::plain(format!("Restarting into scrollback {ready}…"))
        } else if let Some(others) = updates.confirm() {
            UpdateLine {
                text: format!(
                    "Restarting into scrollback {ready} closes {} running from this install.",
                    match others {
                        1 => "1 other scrollback".to_string(),
                        n => format!("{n} other scrollbacks"),
                    }
                ),
                actions: vec![
                    ("Restart anyway", Action::Confirm),
                    ("Not now", Action::Cancel),
                ],
            }
        } else if let Some(failure) = updates.failure() {
            UpdateLine {
                text: format!("Couldn't restart into scrollback {ready}: {failure}"),
                actions: vec![("Try again", Action::Restart)],
            }
        } else {
            UpdateLine {
                text: format!("scrollback {ready} is ready."),
                actions: vec![("Restart to update", Action::Restart)],
            }
        });
    }
    if updates.is_checking() {
        return Some(UpdateLine::plain("Checking for a newer scrollback…"));
    }
    let Some(last) = updates.last_check() else {
        return Some(UpdateLine::plain(
            "Checks for a newer release a few seconds after it starts, then every 4 hours.",
        ));
    };
    let day = format::day(last.at, now);
    let when = if day == format::day(now, now) {
        format!("today at {}", format::clock(last.at))
    } else {
        format!("on {day} at {}", format::clock(last.at))
    };
    Some(UpdateLine::plain(match &last.result {
        CheckResult::UpToDate | CheckResult::Ready(_) => format!("Up to date. Checked {when}."),
        CheckResult::Failed(error) => format!("Couldn't check for updates {when}: {error}"),
    }))
}

/**
 * The version, and the update line under it with its buttons (module docs). The view that
 * draws it observes the updates service, which notifies when the line changes.
 */
pub fn render(cx: &App) -> AnyElement {
    let theme = cx.sb_theme();
    let c = &theme.colors;
    let line = update_line(cx.services().updates.read(cx), format::now());
    let mut block = div().flex().flex_col().gap(theme.spacing.xs).child(
        div()
            .text_color(c.fg)
            .text_spec(&theme.text.control)
            .child(version_line())
            .automation_target("settings/version"),
    );
    if let Some(line) = line {
        block = block.child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(theme.spacing.md)
                .child(
                    div()
                        .text_color(c.fg3)
                        .text_spec(&theme.text.secondary)
                        .child(line.text)
                        .automation_target("settings/update"),
                )
                .children(line.actions.into_iter().map(|(label, action)| {
                    let kind = match action {
                        Action::Restart | Action::Confirm => ButtonKind::Primary,
                        Action::Cancel => ButtonKind::Plain,
                    };
                    button(action.target(), kind, true, true, theme)
                        .debug_selector(move || action.target().into())
                        .child(label)
                        .on_click(move |_, _, cx| {
                            let updates = cx.services().updates.clone();
                            updates.update(cx, |updates, cx| match action {
                                Action::Restart => updates.restart(cx),
                                Action::Confirm => updates.confirm_restart(cx),
                                Action::Cancel => updates.cancel_restart(cx),
                            });
                        })
                        .automation_target(action.target())
                })),
        );
    }
    block.into_any_element()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use gpui::{Entity, TestAppContext};
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::services::updates::{Checking, FIRST_CHECK_AFTER, Real, Updater};
    use crate::test_support::{init, machine};

    /// The line as it reads at the moment of the last check (or now, before one), so that
    /// "today" holds whatever the clock says.
    fn line(updates: &Entity<UpdatesService>, cx: &TestAppContext) -> Option<UpdateLine> {
        updates.read_with(cx, |updates, _| {
            let now = updates
                .last_check()
                .map_or_else(format::now, |check| check.at);
            update_line(updates, now)
        })
    }

    fn text(updates: &Entity<UpdatesService>, cx: &TestAppContext) -> Option<String> {
        line(updates, cx).map(|line| line.text)
    }

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

    #[test]
    fn a_development_build_says_so_in_its_version() {
        assert_eq!(
            version_line(),
            format!(
                "scrollback {} (development build)",
                env!("CARGO_PKG_VERSION")
            )
        );
    }

    #[gpui::test]
    fn a_run_without_updates_says_why_unless_it_is_a_development_build(cx: &mut TestAppContext) {
        let updates = service(Updater::Off(Off::Development), cx);
        assert_eq!(text(&updates, cx), None);
        let off = |off, cx: &mut TestAppContext| {
            updates.update(cx, |updates, cx| updates.set_updater(Updater::Off(off), cx));
            text(&updates, cx)
        };
        assert_eq!(
            off(Off::DataDir, cx).as_deref(),
            Some("Updates are off while --data-dir or SCROLLBACK_DATA_DIR names the data folder.")
        );
        assert!(off(Off::NotInstalled, cx).is_some());
        assert!(off(Off::Automation, cx).is_some());
    }

    #[gpui::test]
    fn the_line_follows_the_checks_and_offers_the_restart(cx: &mut TestAppContext) {
        let found = Arc::new(Mutex::new(Ok(None)));
        let answer = found.clone();
        let updates = service(
            Updater::Real(Real {
                check: Arc::new(move || answers(answer.lock().unwrap().clone())),
                restart: Box::new(|| Err("the installer isn't there".into())),
                others: Box::new(|| 0),
                then: |_| {},
            }),
            cx,
        );
        assert_eq!(
            text(&updates, cx).as_deref(),
            Some("Checks for a newer release a few seconds after it starts, then every 4 hours.")
        );
        cx.executor().advance_clock(FIRST_CHECK_AFTER);
        cx.run_until_parked();
        let checked = line(&updates, cx).expect("a line");
        assert!(
            checked.text.starts_with("Up to date. Checked today at "),
            "{checked:?}"
        );
        assert_eq!(checked.actions, []);

        *found.lock().unwrap() = Err("the feed is unreachable".into());
        updates.update(cx, |updates, cx| {
            updates.check_now(cx);
            assert_eq!(
                update_line(updates, format::now())
                    .map(|line| line.text)
                    .as_deref(),
                Some("Checking for a newer scrollback…")
            );
        });
        cx.run_until_parked();
        let failed = text(&updates, cx).expect("a line");
        assert!(
            failed.starts_with("Couldn't check for updates today at ")
                && failed.ends_with(": the feed is unreachable"),
            "{failed}"
        );

        *found.lock().unwrap() = Ok(Some("0.2.0".into()));
        updates.update(cx, |updates, cx| updates.check_now(cx));
        cx.run_until_parked();
        assert_eq!(
            line(&updates, cx),
            Some(UpdateLine {
                text: "scrollback 0.2.0 is ready.".into(),
                actions: vec![("Restart to update", Action::Restart)],
            })
        );
        updates.update(cx, |updates, cx| {
            updates.restart(cx);
            assert_eq!(
                update_line(updates, format::now())
                    .map(|line| line.text)
                    .as_deref(),
                Some("Restarting into scrollback 0.2.0…")
            );
        });
        cx.run_until_parked();
        assert_eq!(
            line(&updates, cx),
            Some(UpdateLine {
                text: "Couldn't restart into scrollback 0.2.0: the installer isn't there".into(),
                actions: vec![("Try again", Action::Restart)],
            })
        );
    }

    #[gpui::test]
    fn other_processes_are_named_before_a_restart(cx: &mut TestAppContext) {
        let updates = service(
            Updater::Real(Real {
                check: Arc::new(|| answers(Ok(Some("0.2.0".into())))),
                restart: Box::new(|| Ok(())),
                others: Box::new(|| 1),
                then: |_| {},
            }),
            cx,
        );
        cx.executor().advance_clock(FIRST_CHECK_AFTER);
        cx.run_until_parked();
        updates.update(cx, |updates, cx| updates.restart(cx));
        assert_eq!(
            line(&updates, cx),
            Some(UpdateLine {
                text: "Restarting into scrollback 0.2.0 closes 1 other scrollback running from \
                       this install."
                    .into(),
                actions: vec![
                    ("Restart anyway", Action::Confirm),
                    ("Not now", Action::Cancel)
                ],
            })
        );
    }
}
