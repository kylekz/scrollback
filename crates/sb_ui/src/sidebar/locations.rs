//! The locations at the sidebar's foot (DESIGN §4.1): one row per location with its sync
//! state, and the glyph that syncs them all. A location whose sessions are being indexed
//! outside a pass says so as a syncing one does, and so does the index of every location
//! that runs when the app starts, in a row of its own. Why a sync failed is the main area's
//! to say in full (`crate::pages`); the row says that it did. A row opens the locations
//! sheet with its location selected (DESIGN §4.1).

use gpui::{
    AnyElement, Context, Div, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Role,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, div,
    prelude::FluentBuilder as _, px, relative,
};
use sb_core::location::{Location, LocationId, LocationKind};

use super::{NO_LOCATIONS, NONE_SYNCED, Sidebar, SidebarEvent, note};
use crate::automation::AutomationTargetExt as _;
use crate::components::{focus_ring, icon, kind_icon, kind_name, progress_bar, with_numbers};
use crate::format;
use crate::icons::SbIcon;
use crate::services::sync::stale;
use crate::services::{LocationStatus, PassProgress};
use crate::theme::{SbTheme, StyledText as _};

/// The gap between the lines of a row that shows a bar (`design/project/sb.css`, `.loc`).
const BUSY_GAP: gpui::Pixels = px(3.);

/// What a syncing location's row says at its right: files while they are fetched, sessions
/// while they are indexed, nothing while the location is scanned.
fn doing(progress: &PassProgress) -> Option<String> {
    if progress.is_indexing() {
        Some(format!(
            "{} / {} sessions",
            format::count(progress.sessions_done as u64),
            format::count(progress.sessions as u64)
        ))
    } else if progress.planned {
        Some(format!(
            "{} / {} files",
            format::count(progress.items_done as u64),
            format::count(progress.items as u64)
        ))
    } else {
        None
    }
}

/// What a syncing location's row says under its bar (DESIGN §4.1, `E-Edge-First-Sync`).
fn doing_line(kind: &LocationKind, progress: &PassProgress, first: bool) -> String {
    let what = if progress.is_indexing() {
        "indexing".to_string()
    } else if !progress.planned {
        "scanning".to_string()
    } else if progress.bytes == 0 {
        "nothing new".to_string()
    } else {
        format::bytes_of(progress.bytes_done, progress.bytes, " of ")
    };
    format!(
        "{} · {}{what}",
        kind_name(kind),
        if first { "first sync · " } else { "" }
    )
}

impl Sidebar {
    /// The locations' rows, or "None yet" (DESIGN §4.10).
    pub(super) fn render_locations(
        &self,
        cursor: Option<&LocationId>,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sync = self.sync.read(cx);
        let locations = sync.locations().unwrap_or_default();
        if locations.is_empty() {
            // Removed locations' sessions are still in the archive: say what is true.
            let none = if sync.removed().is_empty() {
                NO_LOCATIONS
            } else {
                NONE_SYNCED
            };
            return div()
                .flex_none()
                .pt(theme.spacing.xs)
                .pb(theme.spacing.md)
                .children(sync.locations().map(|_| note(none, theme)))
                .into_any_element();
        }
        let now = format::now();
        div()
            .id("sidebar-location-rows")
            .flex_none()
            // Many locations scroll in their own part of the sidebar.
            .max_h(relative(0.4))
            .overflow_y_scroll()
            .track_scroll(&self.location_scroll)
            .flex()
            .flex_col()
            .gap(px(2.))
            .children(locations.iter().map(|location| {
                let id = location.id.clone();
                let stale = stale(
                    location.last_success,
                    sync.details().found(&location.id),
                    now,
                )
                .is_some();
                location_row(
                    location,
                    sync.status(location),
                    sync.indexing(Some(&location.id)),
                    (now, stale, cursor == Some(&location.id)),
                    theme,
                    cx.listener(move |_, _, _, cx| {
                        cx.emit(SidebarEvent::Locations(Some(id.clone())));
                    }),
                )
            }))
            // The index of every location, while it has sessions to index.
            .children(
                sync.indexing(None)
                    .filter(PassProgress::is_indexing)
                    .map(|progress| {
                        busy(
                            row("index", SbIcon::Layers, theme).aria_label("Indexing sessions"),
                            "Session index",
                            &progress,
                            "indexing every location".to_string(),
                            theme,
                        )
                        .automation_target("sidebar/index")
                    }),
            )
            .into_any_element()
    }
}

/// What every row at the sidebar's foot starts as: its glyph, then what the caller adds.
fn row(id: impl Into<SharedString>, glyph: SbIcon, theme: &SbTheme) -> Stateful<Div> {
    div()
        .id(id.into())
        .role(Role::ListItem)
        .flex_none()
        .flex()
        .items_center()
        .gap(theme.spacing.md)
        .px(theme.spacing.md)
        .rounded(theme.radii.row)
        .whitespace_nowrap()
        .child(icon(glyph, theme.sizes.icon, theme.colors.fg3))
}

/// A row's name (DESIGN §4.1: 12px at 500, `fg2`), cut before what follows it.
fn name(name: &str, theme: &SbTheme) -> Div {
    div()
        .min_w_0()
        .overflow_hidden()
        .text_ellipsis()
        .whitespace_nowrap()
        .text_color(theme.colors.fg2)
        .text_spec(&theme.text.control_small)
        .child(name.to_string())
}

/// A row's second line, its numbers in the face for numbers (DESIGN §1.3).
fn line(text: String, color: Hsla, theme: &SbTheme) -> Div {
    div()
        .min_w_0()
        .overflow_hidden()
        .text_ellipsis()
        .text_spec(&theme.text.small)
        .child(with_numbers(text, color, &theme.text.small, theme))
}

/// A second line led by a glyph, both in `color`.
fn alerted(glyph: SbIcon, text: String, color: Hsla, theme: &SbTheme) -> Div {
    div()
        .flex()
        .items_center()
        .gap(theme.spacing.xs)
        .min_w_0()
        .child(icon(glyph, theme.sizes.icon_xs, color))
        .child(line(text, color, theme))
}

/**
 * What an unreachable location's row says (DESIGN §4.1). Stale, how long since it last
 * synced ("SSH · last sync 12 days ago"); otherwise since when it can't be reached, in the
 * short words the row has room for ("SSH · not reachable · 16 Nov").
 */
fn unreachable_line(
    kind: &LocationKind,
    last_success: Option<i64>,
    stale: bool,
    since: i64,
    now: i64,
) -> String {
    let kind = kind_name(kind);
    match last_success.filter(|_| stale) {
        Some(at) => format!("{kind} · last sync {}", format::ago(at, now)),
        None => format!("{kind} · not reachable · {}", format::day(since, now)),
    }
}

/// A row with work in hand (DESIGN §4.1): 48 high, how far the work is at the right of its
/// name, a bar, and what the work is under it.
fn busy(
    row: Stateful<Div>,
    called: &str,
    progress: &PassProgress,
    says: String,
    theme: &SbTheme,
) -> Stateful<Div> {
    let c = &theme.colors;
    row.h(theme.sizes.location_row_syncing_height).child(
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(BUSY_GAP)
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(name(called, theme))
                    .children(doing(progress).map(|doing| {
                        div()
                            .ml_auto()
                            .text_color(c.accent_text)
                            .text_spec(&theme.text.count)
                            .child(doing)
                    })),
            )
            .child(progress_bar(progress.fraction(), theme))
            .child(line(says, c.fg3, theme)),
    )
}

/// A location's row (DESIGN §4.1): 40 high, its kind's icon, its name at 500, and its state
/// on a second line; 48 high with a bar while it syncs or is indexed.
fn location_row(
    location: &Location,
    status: LocationStatus,
    indexing: Option<PassProgress>,
    (now, stale, keyboard): (i64, bool, bool),
    theme: &SbTheme,
    open: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> AnyElement {
    let c = &theme.colors;
    let kind = kind_name(&location.kind);
    let dot = |fill: Option<Hsla>| {
        div()
            .flex_none()
            .ml_auto()
            .size(theme.sizes.dot)
            .rounded_full()
            .map(|this| match fill {
                Some(fill) => this.bg(fill),
                None => this.border(px(1.5)).border_color(c.fg3),
            })
    };
    let hover = c.s2;
    let row = row(
        format!("location-{}", location.id),
        kind_icon(&location.kind),
        theme,
    )
    .cursor_pointer()
    .hover(move |style| style.bg(hover))
    // The arrows' row, from the keyboard (DESIGN §5).
    .when(keyboard, |this| {
        this.bg(c.s3).shadow(focus_ring(true, theme))
    })
    .on_click(open);
    // A state that is one line under the name.
    let resting = |row: Stateful<Div>, state: Div| {
        row.h(theme.sizes.location_row_height).child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .min_w_0()
                .child(name(&location.name, theme))
                .child(state),
        )
    };
    let target = format!("sidebar/location/{}", location.name);
    match (status, indexing) {
        (
            LocationStatus::Syncing {
                progress, first, ..
            },
            _,
        ) => busy(
            row.aria_label(format!("{}: syncing", location.name)),
            &location.name,
            &progress,
            doing_line(&location.kind, &progress, first),
            theme,
        ),
        // What a stopped or a failed pass stored is being indexed.
        (_, Some(progress)) => busy(
            row.aria_label(format!("{}: indexing", location.name)),
            &location.name,
            &progress,
            format!("{kind} · indexing"),
            theme,
        ),
        (LocationStatus::Synced(at), None) => resting(
            row.aria_label(format!("{}: synced", location.name)),
            line(
                format!("{kind} · synced {}", format::ago(at, now)),
                c.fg3,
                theme,
            ),
        )
        .child(dot(Some(c.green))),
        (LocationStatus::NeverSynced, None) => resting(
            row.aria_label(format!("{}: not synced yet", location.name)),
            line(format!("{kind} · not synced yet"), c.fg3, theme),
        )
        .child(dot(None)),
        (LocationStatus::Failed(_), None) => resting(
            row.aria_label(format!("{}: last sync failed", location.name)),
            alerted(
                SbIcon::CircleAlert,
                format!("{kind} · last sync failed"),
                c.red,
                theme,
            ),
        ),
        /* Not a failure: since when it is out of reach, neutral; with the alert only when it is
        stale as well (DESIGN §4.1, PLAN §5). */
        (LocationStatus::Unreachable { since }, None) => {
            let says = unreachable_line(&location.kind, location.last_success, stale, since, now);
            resting(
                row.aria_label(format!("{}: not reachable", location.name)),
                if stale {
                    alerted(SbIcon::TriangleAlert, says, c.yellow, theme)
                } else {
                    line(says, c.fg3, theme)
                },
            )
            .child(dot(None))
        }
        (
            LocationStatus::Stopped {
                items_left,
                bytes_left: _,
            },
            None,
        ) => resting(
            row.aria_label(format!("{}: stopped", location.name)),
            line(
                format!(
                    "{kind} · stopped · {} left",
                    format::plural(items_left, "file")
                ),
                c.fg3,
                theme,
            ),
        )
        .child(dot(None)),
    }
    .automation_target_with(move || target.clone().into())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use sb_core::sync::Progress;

    use super::*;

    #[test]
    fn each_kind_is_shown_by_its_own_name_and_glyph() {
        let wsl = LocationKind::Wsl {
            distro: "Ubuntu-24.04".to_string(),
        };
        let ssh = LocationKind::Ssh {
            host: "user@macbook".to_string(),
        };
        // DESIGN §1.5, §4.1.
        assert_eq!(
            [&LocationKind::Local, &wsl, &ssh].map(|kind| (kind_name(kind), kind_icon(kind))),
            [
                ("Local", SbIcon::Monitor),
                ("WSL", SbIcon::Box),
                ("SSH", SbIcon::Laptop)
            ]
        );
    }

    #[test]
    fn an_unreachable_row_says_when_it_last_synced() {
        let ssh = LocationKind::Ssh {
            host: "user@macbook".to_string(),
        };
        let day = 24 * 60 * 60 * 1000;
        let now = 40 * day;
        // DESIGN §4.1: stale, "SSH · last sync 12 days ago".
        assert_eq!(
            unreachable_line(&ssh, Some(now - 12 * day), true, now - day, now),
            "SSH · last sync 12 days ago"
        );
        // Not stale, or never synced: since when, in words short enough for the row.
        let since = format!("SSH · not reachable · {}", format::day(now - day, now));
        assert_eq!(
            unreachable_line(&ssh, Some(now - 2 * day), false, now - day, now),
            since
        );
        assert_eq!(unreachable_line(&ssh, None, false, now - day, now), since);
    }

    #[test]
    fn a_syncing_row_says_what_its_pass_is_doing() {
        let mut progress = PassProgress::default();
        let local = &LocationKind::Local;
        assert_eq!(doing(&progress), None);
        assert_eq!(
            doing_line(local, &progress, true),
            "Local · first sync · scanning"
        );

        progress.apply(&Progress::Planned {
            items: 212,
            bytes: 96 << 20,
        });
        progress.apply(&Progress::Fetched { bytes: 41 << 20 });
        progress.apply(&Progress::Stored { items: 38 });
        assert_eq!(doing(&progress).as_deref(), Some("38 / 212 files"));
        assert_eq!(doing_line(local, &progress, false), "Local · 41 of 96 MB");
        assert_eq!(
            doing_line(local, &progress, true),
            "Local · first sync · 41 of 96 MB"
        );

        progress.apply(&Progress::Indexing {
            sessions: 1418,
            bytes: 0,
        });
        progress.apply(&Progress::Indexed {
            sessions: 362,
            bytes: 0,
        });
        assert_eq!(doing(&progress).as_deref(), Some("362 / 1,418 sessions"));
        assert_eq!(doing_line(local, &progress, false), "Local · indexing");
    }
}
