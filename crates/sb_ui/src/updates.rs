//! This build's version and, under it, a line on its updates (PLAN §13, phase 7), for Settings:
//! "scrollback 0.2.0", then what the updates service ([`crate::services::updates`]) last found,
//! with **Restart to update** once a newer release is downloaded. A development build shows the
//! version alone, which says it is one; any other run that looks for no updates says why.

use gpui::{
    AnyElement, App, IntoElement, ParentElement as _, StatefulInteractiveElement as _, Styled as _,
    div, prelude::FluentBuilder as _,
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

/// The line under the version, and the label of the button beside it, when it has one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateLine {
    pub text: String,
    /// Restart to update, or Try again after a restart that couldn't be made.
    pub restart: Option<&'static str>,
}

impl UpdateLine {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            restart: None,
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
        } else if let Some(failure) = updates.failure() {
            UpdateLine {
                text: format!("Couldn't restart into scrollback {ready}: {failure}"),
                restart: Some("Try again"),
            }
        } else {
            UpdateLine {
                text: format!("scrollback {ready} is ready."),
                restart: Some("Restart to update"),
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
 * The version, and the update line under it with its button (module docs). The view that
 * draws it observes the updates service, which notifies when the line changes.
 */
pub fn render(cx: &App) -> AnyElement {
    let theme = cx.sb_theme();
    let c = &theme.colors;
    let line = update_line(cx.services().updates.read(cx), format::now());
    div()
        .flex()
        .flex_col()
        .gap(theme.spacing.xs)
        .child(
            div()
                .text_color(c.fg)
                .text_spec(&theme.text.control)
                .child(version_line())
                .automation_target("settings/version"),
        )
        .when_some(line, |this, line| {
            this.child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.md)
                    .child(
                        div()
                            .text_color(c.fg3)
                            .text_spec(&theme.text.secondary)
                            .child(line.text)
                            .automation_target("settings/update"),
                    )
                    .when_some(line.restart, |this, label| {
                        this.child(
                            button(
                                "settings-restart-to-update",
                                ButtonKind::Primary,
                                true,
                                true,
                                theme,
                            )
                            .child(label)
                            .on_click(|_, _, cx| {
                                cx.services()
                                    .updates
                                    .clone()
                                    .update(cx, |updates, cx| updates.restart(cx));
                            })
                            .automation_target("settings/restart-to-update"),
                        )
                    }),
            )
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::{AppContext as _, Entity, TestAppContext};
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::services::updates::{FIRST_CHECK_AFTER, Updater};

    fn line(updates: &Entity<UpdatesService>, cx: &TestAppContext) -> Option<UpdateLine> {
        updates.read_with(cx, |updates, _| update_line(updates, format::now()))
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
        let off = |off, cx: &mut TestAppContext| {
            let updates = cx.new(|cx| UpdatesService::new(Updater::Off(off), cx));
            line(&updates, cx).map(|line| line.text)
        };
        assert_eq!(off(Off::Development, cx), None);
        assert_eq!(
            off(Off::DataDir, cx).as_deref(),
            Some("Updates are off while --data-dir or SCROLLBACK_DATA_DIR names the data folder.")
        );
        assert!(off(Off::NotInstalled, cx).is_some());
        assert!(off(Off::Automation, cx).is_some());
    }

    #[gpui::test]
    fn the_line_follows_the_checks_and_offers_the_restart(cx: &mut TestAppContext) {
        let found = Arc::new(std::sync::Mutex::new(Ok(None)));
        let answer = found.clone();
        let updates = cx.new(|cx| {
            UpdatesService::new(
                Updater::Real {
                    check: Arc::new(move || {
                        answer
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .clone()
                    }),
                    restart: Box::new(|| Err("the installer isn't there".into())),
                },
                cx,
            )
        });
        assert_eq!(
            line(&updates, cx).map(|line| line.text).as_deref(),
            Some("Checks for a newer release a few seconds after it starts, then every 4 hours.")
        );
        cx.executor().advance_clock(FIRST_CHECK_AFTER);
        cx.run_until_parked();
        let checked = line(&updates, cx).expect("a line");
        assert!(
            checked.text.starts_with("Up to date. Checked today at "),
            "{checked:?}"
        );
        assert_eq!(checked.restart, None);

        *found.lock().unwrap() = Err("the feed is unreachable".into());
        updates.update(cx, |updates, cx| updates.check_now(cx));
        assert_eq!(
            line(&updates, cx).map(|line| line.text).as_deref(),
            Some("Checking for a newer scrollback…")
        );
        cx.run_until_parked();
        let failed = line(&updates, cx).expect("a line").text;
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
                restart: Some("Restart to update"),
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
                restart: Some("Try again"),
            })
        );
    }
}
