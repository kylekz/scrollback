//! The selected location's pane (DESIGN §4.11): its name, which renames it when the field
//! loses the keyboard or on `Enter`; what connects to it (an SSH host as typed, a distro and
//! where it mounts the drives); its status card; what its agents have there and what is
//! archived from it; the last connection test run from here; and Remove location…, Test
//! connection and Sync now or Stop.

use gpui::{
    AnyElement, Context, Div, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _,
};
use sb_core::location::{Location, LocationKind};

use super::LocationsSheet;
use crate::add_location::{field, help_line, test_card};
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{
    ButtonKind, button, card, dot, end_cut, icon, kind_icon, kind_name, kv, label, location_tag,
    progress_bar, said, with_numbers,
};
use crate::format;
use crate::icons::SbIcon;
use crate::services::sync::{STALE_AFTER_DAYS, stale};
use crate::services::{LocationStatus, SyncService};
use crate::theme::{SbTheme, StyledText as _};

/// `13 Nov, 22:41`.
fn when(ms: i64, now: i64) -> String {
    format!("{}, {}", format::day(ms, now), format::clock(ms))
}

impl LocationsSheet {
    pub(super) fn detail(
        &self,
        location: &Location,
        window: &Window,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let sync = self.sync.read(cx);
        let sessions = sync.details().sessions(&location.id);
        let syncing = matches!(sync.status(location), LocationStatus::Syncing { .. });
        let removing = sync.is_removing(&location.id);
        /* A test would start a stopped distro, which only a sync may do (PLAN §6.3); this
        machine has nothing to connect to. */
        let testable = match &location.kind {
            LocationKind::Local => false,
            LocationKind::Wsl { distro } => sync
                .distros()
                .and_then(|distros| distros.offered().find(|it| it.name == *distro))
                .is_some_and(|it| it.running),
            LocationKind::Ssh { .. } => true,
        };
        let test = sync
            .test()
            .filter(|test| test.kind.same_place(&location.kind))
            .map(|test| test_card(test, &location.name, theme, cx));
        let connects = connects(location, sync, theme);
        let status = self.status_card(location, theme, cx);
        let found = found_card(location, sync, theme, cx);
        let id = location.id.clone();
        // What doesn't fit scrolls; the buttons stay at the foot.
        let body = div()
            .id("sheet-detail-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(theme.spacing.lg)
            .pt(theme.sizes.sheet_padding)
            .pb(theme.spacing.lg)
            .px(theme.sizes.pane_padding)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.md)
                    .child(icon(kind_icon(&location.kind), theme.sizes.icon, c.fg2))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_spec(&theme.text.title)
                            .child(location.name.clone()),
                    )
                    .child(location_tag(
                        kind_name(&location.kind),
                        &location.kind,
                        false,
                        theme,
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(theme.spacing.xs)
                    .child(label("Name", theme))
                    .child(
                        field(
                            &self.name,
                            false,
                            self.name_problem().is_some(),
                            window,
                            theme,
                            cx,
                        )
                        .automation_target("sheet/name"),
                    )
                    .children(self.name_problem().map(|problem| {
                        div()
                            .text_color(c.red)
                            .text_spec(&theme.text.secondary)
                            .child(problem.to_string())
                            .automation_target("sheet/name-problem")
                    })),
            )
            .children(connects)
            .child(status)
            .child(found)
            .children(test);
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(body)
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(theme.spacing.sm)
                    .pt(theme.spacing.sm)
                    .pb(theme.spacing.lg)
                    .px(theme.sizes.pane_padding)
                    .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.md)
                    .child(
                        button("sheet-remove", ButtonKind::Danger, true, !removing, theme)
                            .when(!removing, |this| {
                                this.on_click(cx.listener(|this, _, window, cx| {
                                    this.ask_to_remove(window, cx);
                                }))
                            })
                            .child("Remove location…")
                            .automation_target("sheet/remove"),
                    )
                    .child(div().flex_1())
                    .when(testable, |this| {
                        let kind = location.kind.clone();
                        this.child(
                            button("sheet-test", ButtonKind::Plain, true, !removing, theme)
                                .when(!removing, |this| {
                                    this.on_click(cx.listener(move |this, _, _, cx| {
                                        let kind = kind.clone();
                                        this.sync
                                            .update(cx, |sync, cx| sync.test_place(kind, cx));
                                    }))
                                })
                                .child("Test connection")
                                .automation_target("sheet/test"),
                        )
                    })
                    .child(
                        button("sheet-sync", ButtonKind::Primary, true, !removing, theme)
                            .when(!removing, |this| {
                                this.on_click(cx.listener(move |this, _, _, cx| {
                                    let id = id.clone();
                                    this.sync.update(cx, |sync, cx| {
                                        if syncing {
                                            sync.stop(&id);
                                            cx.notify();
                                        } else {
                                            sync.sync(&id, cx);
                                        }
                                    });
                                }))
                            })
                            .child(icon(
                                if syncing { SbIcon::Pause } else { SbIcon::RefreshCw },
                                theme.sizes.icon_xs,
                                c.accent_ink,
                            ))
                            .child(if syncing { "Stop" } else { "Sync now" })
                            .automation_target("sheet/sync"),
                    ),
            )
            .child(
                div()
                    .text_color(c.fg3)
                    .text_spec(&theme.text.small)
                    .overflow_hidden()
                    .child(if sessions == 0 {
                        format!(
                            "Removing stops syncing {}. Nothing was archived from it.",
                            end_cut(&location.name, 40)
                        )
                    } else {
                        format!(
                            "Removing stops syncing {}. Its {} archived {} stay in the archive.",
                            end_cut(&location.name, 40),
                            format::count(sessions as u64),
                            if sessions == 1 { "session" } else { "sessions" }
                        )
                    }),
            ))
            .automation_target("sheet/detail")
    }

    /// The status card (DESIGN §4.11): synced, syncing, not reachable (a state, not an
    /// error), failed with the tool's words in full, or not synced yet; stale with since
    /// when, over any of them.
    fn status_card(&self, location: &Location, theme: &SbTheme, cx: &Context<Self>) -> AnyElement {
        let c = &theme.colors;
        let sync = self.sync.read(cx);
        let now = format::now();
        let headline = |dot: Div, what: &str, more: String| {
            div()
                .flex()
                .items_center()
                .gap(theme.spacing.md)
                .text_spec(&theme.text.secondary)
                .child(dot)
                .child(
                    div()
                        .text_color(c.fg)
                        .font_weight(theme.text.control.weight)
                        .child(what.to_string()),
                )
                .child(div().text_color(c.fg3).child(more))
        };
        let last_good = kv(
            "Last good sync",
            with_numbers(
                match location.last_success {
                    Some(at) => format!(
                        "{} {} · {}",
                        format::day(at, now),
                        format::clock(at),
                        format::ago(at, now)
                    ),
                    None => "never".to_string(),
                },
                c.fg2,
                &theme.text.secondary,
                theme,
            ),
            theme,
        );
        let stale = stale(
            location.last_success,
            sync.details().found(&location.id),
            now,
        )
        .map(|stale| {
            kv(
                "Stale",
                div().text_color(c.yellow).child(format!(
                    "since {} (warning after {STALE_AFTER_DAYS} days)",
                    format::day(stale.since, now)
                )),
                theme,
            )
        });
        let next_try = sync.next_round().map(|at| {
            let minutes = (at - now) / 60_000;
            kv(
                "Next try",
                if minutes >= 1 {
                    format!("in {minutes} min, and on every launch")
                } else {
                    "within a minute, and on every launch".to_string()
                },
                theme,
            )
        });
        let card = card(theme)
            .gap(theme.spacing.sm)
            .py(theme.sizes.list_row_padding)
            .px(theme.spacing.lg);
        let (card, target) = match sync.status(location) {
            LocationStatus::Synced(at) => (
                card.child(headline(
                    dot(Some(c.green), theme),
                    "Synced",
                    format!("{}, {}", when(at, now), format::ago(at, now)),
                ))
                .children(stale),
                "sheet/status/synced",
            ),
            LocationStatus::Syncing { progress, .. } => (
                card.child(headline(
                    dot(Some(c.accent), theme),
                    "Syncing",
                    super::syncing_line(&progress)
                        .trim_start_matches("Syncing · ")
                        .to_string(),
                ))
                .child(progress_bar(progress.fraction(), theme))
                .child(
                    div()
                        .text_color(c.fg3)
                        .text_spec(&theme.text.secondary)
                        .child("Stopping keeps everything copied so far; the next sync carries on."),
                )
                .child(last_good),
                "sheet/status/syncing",
            ),
            LocationStatus::Unreachable { since } => (
                card.child(headline(
                    dot(None, theme),
                    "Not reachable",
                    format!("since {}", when(since, now)),
                ))
                .children(
                    location
                        .unreachable
                        .as_ref()
                        .map(|it| said(it.reason.clone(), theme)),
                )
                .child(
                    div()
                        .text_color(c.fg3)
                        .text_spec(&theme.text.secondary)
                        .child("Asleep or off the network, most likely. That's not an error: it syncs when it's back."),
                )
                .child(last_good)
                .children(stale)
                .children(next_try),
                "sheet/status/unreachable",
            ),
            LocationStatus::Failed(why) => (
                card.border_color(c.red_soft)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(theme.spacing.md)
                            .text_spec(&theme.text.secondary)
                            .child(icon(SbIcon::CircleAlert, theme.sizes.icon_sm, c.red))
                            .child(
                                div()
                                    .text_color(c.red)
                                    .font_weight(theme.text.control.weight)
                                    .child("Failed"),
                            )
                            .children(location.last_attempt.map(|at| {
                                div().text_color(c.fg3).child(format!("at {}", when(at, now)))
                            })),
                    )
                    .child(said(agents::named(&why, cx), theme))
                    .child(
                        div()
                            .text_color(c.fg2)
                            .text_spec(&theme.text.secondary)
                            .child(failed_note(&location.kind)),
                    )
                    .child(last_good)
                    .children(stale),
                "sheet/status/failed",
            ),
            LocationStatus::Stopped {
                items_left,
                bytes_left,
            } => (
                card.child(headline(
                    dot(None, theme),
                    "Stopped",
                    super::stopped_line(items_left, bytes_left)
                        .trim_start_matches("Stopped · ")
                        .to_string(),
                ))
                .child(
                    div()
                        .text_color(c.fg3)
                        .text_spec(&theme.text.secondary)
                        .child("What was copied is kept; the next sync carries on from there."),
                )
                .child(last_good)
                .children(stale),
                "sheet/status/stopped",
            ),
            LocationStatus::NeverSynced => (
                card.child(headline(
                    dot(None, theme),
                    "Not synced yet",
                    super::never_line(location, sync)
                        .trim_start_matches("Not synced yet")
                        .trim_start_matches(" · ")
                        .to_string(),
                ))
                .children(next_try),
                "sheet/status/never",
            ),
        };
        card.automation_target(target)
    }
}

/// What a failed sync's card says under the tool's words: whose words they are, and for a
/// host, that its key is never accepted on the owner's behalf (DESIGN §4.11).
fn failed_note(kind: &LocationKind) -> &'static str {
    match kind {
        LocationKind::Ssh { .. } => {
            "scrollback never accepts a host key for you. If the key changed, check why and \
             update known_hosts in a terminal; if the host is new, connect once with ssh in a \
             terminal and answer its question. Then Sync now."
        }
        LocationKind::Wsl { .. } | LocationKind::Local => {
            "What failed, as it was recorded. Sync now tries again."
        }
    }
}

/// What connects to the location: an SSH host as typed, which is not edited here; a distro,
/// whether it runs, and where it mounts the Windows drives (DESIGN §4.11).
fn connects(location: &Location, sync: &SyncService, theme: &SbTheme) -> Vec<AnyElement> {
    let c = &theme.colors;
    match &location.kind {
        LocationKind::Local => Vec::new(),
        LocationKind::Ssh { host } => vec![
            div()
                .flex()
                .flex_col()
                .gap(theme.spacing.xs)
                .child(label("Host", theme))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .h(theme.sizes.input_height)
                        .px(theme.sizes.list_row_padding)
                        .rounded(theme.radii.input)
                        .border_1()
                        .border_color(c.line_subtle)
                        .bg(c.s1)
                        .text_color(c.fg2)
                        .text_spec(&theme.text.path)
                        .child(host.clone())
                        .automation_target("sheet/host"),
                )
                .into_any_element(),
            help_line(theme),
        ],
        LocationKind::Wsl { distro } => {
            let state = match sync
                .distros()
                .and_then(|distros| distros.offered().find(|it| it.name == *distro))
            {
                Some(listed) if listed.running => "running",
                Some(_) => "stopped",
                None => "not listed by WSL now",
            };
            vec![
                card(theme)
                    .gap(theme.spacing.sm)
                    .py(theme.sizes.list_row_padding)
                    .px(theme.spacing.lg)
                    .child(kv(
                        "Distro",
                        div()
                            .text_spec(&theme.text.path)
                            .child(SharedString::from(distro.clone())),
                        theme,
                    ))
                    .child(kv("State", state, theme))
                    .child(kv(
                        "Windows drives",
                        format!(
                            "under {}, which files as the drive letters",
                            location.drive_mount.as_deref().unwrap_or("/mnt/")
                        ),
                        theme,
                    ))
                    .into_any_element(),
            ]
        }
    }
}

/// What the location's last passes found (DESIGN §4.11): each agent with where it keeps its
/// data and how long, and what is archived from here.
fn found_card(
    location: &Location,
    sync: &SyncService,
    theme: &SbTheme,
    cx: &Context<LocationsSheet>,
) -> AnyElement {
    let c = &theme.colors;
    let details = sync.details();
    let found = details.found(&location.id);
    let card = card(theme)
        .gap(theme.spacing.sm)
        .py(theme.sizes.list_row_padding)
        .px(theme.spacing.lg)
        .child(
            div()
                .text_color(c.fg)
                .text_spec(&theme.text.control_small)
                .child("Found by its last sync"),
        );
    if found.is_empty() && location.last_success.is_none() {
        return card
            .child(
                div()
                    .text_color(c.fg3)
                    .text_spec(&theme.text.secondary)
                    .child("Nothing yet: its first sync looks for the agents there."),
            )
            .automation_target("sheet/found-card");
    }
    let trashed = details.trashed(&location.id);
    let archived = format!(
        "{}{} · {}{}",
        format::plural(details.sessions(&location.id), "session"),
        if trashed > 0 {
            format!(" ({trashed} in Trash)")
        } else {
            String::new()
        },
        format::size(details.bytes(&location.id)),
        details
            .since(&location.id)
            .map(|since| format!(" · since {}", format::span(since, since)))
            .unwrap_or_default()
    );
    card.children(found.iter().map(|found| {
        kv(
            agents::name(&found.provider, cx),
            div()
                .text_spec(&theme.text.path_small)
                .child(found.root.as_deref().map_or_else(String::new, format::path)),
            theme,
        )
    }))
    .child(kv(
        "Archived from here",
        with_numbers(archived, c.fg2, &theme.text.secondary, theme),
        theme,
    ))
    .children(found.iter().filter_map(|found| {
        let days = found.retention_days?;
        Some(kv(
            "Agent's cleanup",
            format!(
                "{} keeps transcripts {}",
                agents::name(&found.provider, cx),
                format::plural(days as usize, "day")
            ),
            theme,
        ))
    }))
    .automation_target("sheet/found-card")
}
