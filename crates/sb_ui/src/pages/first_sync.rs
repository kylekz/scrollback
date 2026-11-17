//! What All sessions shows while a location's first sync runs (DESIGN §4.14,
//! `E-Edge-First-Sync`): how far each location is, a way to stop, and where the sessions land
//! while there are no folders.

use gpui::{
    AnyElement, Context, IntoElement, ParentElement as _, Pixels, StatefulInteractiveElement as _,
    Styled as _, div, px,
};
use sb_core::catalogue::Shelf;

use super::sessions::SessionsView;
use crate::automation::AutomationTargetExt as _;
use crate::components::{
    BANNER_GAP, ButtonKind, button, icon, kind_icon, progress_bar, with_numbers,
};
use crate::format;
use crate::icons::SbIcon;
use crate::services::{LocationStatus, PassProgress, SyncService};
use crate::theme::{SbTheme, StyledText as _};

/// The First sync card's padding above and below (`design/project/E-Edge-First-Sync`).
const CARD_PADDING: Pixels = px(14.);

/// How long a first sync runs before it says how long it has left (DESIGN §4.14).
pub(super) const ESTIMATE_AFTER_MS: i64 = 60_000;

/// The first syncs that are running.
pub struct FirstSync {
    /// When the earliest started, in Unix milliseconds.
    started: i64,
    /// Each location's name and its kind's glyph, how far its pass is and when it started.
    locations: Vec<(String, SbIcon, PassProgress, i64)>,
}

/// The first syncs running now, if any is.
pub fn running(sync: &SyncService) -> Option<FirstSync> {
    let locations: Vec<_> = sync
        .locations()?
        .iter()
        .filter_map(|location| match sync.status(location) {
            LocationStatus::Syncing {
                progress,
                first: true,
                started,
            } => Some((
                location.name.clone(),
                kind_icon(&location.kind),
                progress,
                started,
            )),
            _ => None,
        })
        .collect();
    Some(FirstSync {
        started: locations.iter().map(|(.., started)| *started).min()?,
        locations,
    })
}

/// How far one location's pass is, in words.
fn doing(progress: &PassProgress) -> String {
    if progress.is_indexing() {
        format!(
            "{} of {} sessions indexed",
            format::count(progress.sessions_done as u64),
            format::count(progress.sessions as u64)
        )
    } else if progress.planned {
        format!(
            "{} of {} files · {}",
            format::count(progress.items_done as u64),
            format::count(progress.items as u64),
            format::bytes_of(progress.bytes_done, progress.bytes, " of ")
        )
    } else {
        "scanning".to_string()
    }
}

/// How long the slowest pass has left, once it has fetched for a minute; "estimating…"
/// before that, and nothing once only indexing is left.
fn time_left(first_sync: &FirstSync, now: i64) -> Option<String> {
    let left = first_sync
        .locations
        .iter()
        .filter(|(_, _, progress, _)| !progress.is_indexing())
        .map(|(_, _, progress, started)| {
            let elapsed = now - started;
            let done = progress.fraction();
            (elapsed >= ESTIMATE_AFTER_MS && done > 0.)
                .then(|| (elapsed as f64 * f64::from(1. - done) / f64::from(done)) as u64 / 1000)
        })
        .collect::<Option<Vec<u64>>>();
    match left {
        Some(left) => left
            .into_iter()
            .max()
            .map(|seconds| format!("{} left", format::about(seconds))),
        None => Some("estimating…".to_string()),
    }
}

/// The First sync card.
pub fn card(first_sync: &FirstSync, theme: &SbTheme, cx: &Context<SessionsView>) -> AnyElement {
    let c = &theme.colors;
    let started = format!("started {}", format::clock(first_sync.started));
    div()
        .flex_none()
        .flex()
        .flex_col()
        .gap(theme.spacing.lg)
        .py(CARD_PADDING)
        .px(theme.spacing.xl)
        .rounded(theme.radii.card)
        .bg(c.s1)
        .border_1()
        .border_color(c.line_subtle)
        .child(
            div()
                .flex()
                .items_center()
                .gap(theme.spacing.md)
                .child(div().text_spec(&theme.text.title).child("First sync"))
                .child(div().text_spec(&theme.text.secondary).child(with_numbers(
                    match time_left(first_sync, format::now()) {
                        Some(left) => format!("{started} · {left}"),
                        None => started,
                    },
                    c.fg3,
                    &theme.text.secondary,
                    theme,
                )))
                .child(
                    button("first-sync-stop", ButtonKind::Plain, true, true, theme)
                        .ml_auto()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sync.update(cx, |sync, _| sync.stop_all());
                        }))
                        .child(icon(SbIcon::X, theme.sizes.icon_xs, c.fg))
                        .child("Stop")
                        .automation_target("first-sync/stop"),
                ),
        )
        .child(
            div()
                .flex()
                .gap(theme.spacing.xl)
                .children(
                    first_sync
                        .locations
                        .iter()
                        .map(|(name, glyph, progress, _)| {
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(theme.spacing.sm)
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(theme.spacing.md)
                                        .text_spec(&theme.text.secondary)
                                        .child(icon(*glyph, theme.sizes.icon_sm, c.fg3))
                                        .child(
                                            div()
                                                .font_weight(theme.text.control.weight)
                                                .child(name.clone()),
                                        )
                                        .child(
                                            div()
                                                .ml_auto()
                                                .text_color(c.fg3)
                                                .text_spec(&theme.text.number)
                                                .child(doing(progress)),
                                        ),
                                )
                                .child(progress_bar(progress.fraction(), theme))
                        }),
                ),
        )
        .child(
            div()
                .text_color(c.fg3)
                .text_spec(&theme.text.secondary)
                .child(
                    "Stopping keeps everything copied so far; the next sync carries on from \
                     there.",
                ),
        )
        .automation_target("first-sync")
}

/// The banner that says where sessions land while there are no folders.
pub fn no_folders_banner(theme: &SbTheme, cx: &Context<SessionsView>) -> AnyElement {
    let c = &theme.colors;
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(BANNER_GAP)
        .py(theme.spacing.md)
        .px(theme.spacing.lg)
        .rounded(theme.radii.row)
        .bg(c.s1)
        .border_1()
        .border_color(c.line_subtle)
        .child(icon(SbIcon::Inbox, theme.sizes.icon_sm, c.fg3))
        .child(div().flex_1().min_w_0().text_color(c.fg2).child(
            "No folders yet, so everything lands in Unsorted, grouped by project path. Make \
             folders from those paths there.",
        ))
        .child(
            button("open-unsorted", ButtonKind::Plain, true, true, theme)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.library
                        .update(cx, |library, cx| library.show(Shelf::Unsorted, cx));
                }))
                .child("Open Unsorted")
                .automation_target("first-sync/open-unsorted"),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use sb_core::sync::Progress;

    use super::*;

    fn fetched(done: u64, of: u64) -> PassProgress {
        let mut progress = PassProgress::default();
        progress.apply(&Progress::Planned {
            items: 418,
            bytes: of,
        });
        progress.apply(&Progress::Fetched { bytes: done });
        progress.apply(&Progress::Stored { items: 362 });
        progress
    }

    #[test]
    fn a_pass_says_how_far_it_is() {
        assert_eq!(doing(&PassProgress::default()), "scanning");
        assert_eq!(
            doing(&fetched(1_181_116_006, 1_503_238_553)),
            "362 of 418 files · 1.1 of 1.4 GB"
        );
        let mut indexing = fetched(10, 10);
        indexing.apply(&Progress::Indexing {
            sessions: 418,
            bytes: 0,
        });
        assert_eq!(doing(&indexing), "0 of 418 sessions indexed");
    }

    #[test]
    fn the_time_left_waits_for_a_minute_of_progress() {
        let first_sync = |progress, started| FirstSync {
            started,
            locations: vec![("This PC".to_string(), SbIcon::Monitor, progress, started)],
        };
        let minute = 60_000;
        // A quarter done after five minutes: fifteen to go.
        let quarter = fetched(250, 1000);
        assert_eq!(
            time_left(&first_sync(quarter, 0), 5 * minute).as_deref(),
            Some("about 15 min left")
        );
        assert_eq!(
            time_left(&first_sync(quarter, 0), minute - 1).as_deref(),
            Some("estimating…")
        );
        assert_eq!(
            time_left(&first_sync(PassProgress::default(), 0), 5 * minute).as_deref(),
            Some("estimating…")
        );
    }
}
