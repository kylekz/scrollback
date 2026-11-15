//! The locations at the sidebar's foot (DESIGN §4.1): one row per location with its sync
//! state, and the glyph that syncs them all. A location whose sessions are being indexed
//! outside a pass says so as a syncing one does, and so does the index of every location
//! that runs when the app starts, in a row of its own. Why a sync failed is the main area's
//! to say in full (`crate::pages`); the row says that it did. The rows open the locations
//! sheet once there is one (PLAN §13).

use gpui::{
    AnyElement, Context, Div, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Role,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, div,
    prelude::FluentBuilder as _, px,
};
use sb_core::location::{Location, LocationKind};

use super::{NO_LOCATIONS, Sidebar, note};
use crate::automation::AutomationTargetExt as _;
use crate::components::{icon, kind_name, progress_bar};
use crate::format;
use crate::icons::SbIcon;
use crate::services::{LocationStatus, PassProgress};
use crate::theme::{SbTheme, StyledText as _};

/// The gap between the lines of a row that shows a bar (`design/project/sb.css`, `.loc`).
const BUSY_GAP: gpui::Pixels = px(3.);

/// What a syncing location's row says at its right: files while they are fetched, sessions
/// while they are indexed, nothing while the location is scanned.
fn doing(progress: &PassProgress) -> Option<String> {
    if progress.is_indexing() {
        Some(format!(
            "{} / {}",
            format::count(progress.sessions_done as u64),
            format::count(progress.sessions as u64)
        ))
    } else if progress.planned {
        Some(format!(
            "{} / {}",
            format::count(progress.items_done as u64),
            format::count(progress.items as u64)
        ))
    } else {
        None
    }
}

/// What a syncing location's row says under its bar (DESIGN §4.1, `E-Edge-First-Sync`).
fn doing_line(kind: LocationKind, progress: &PassProgress, first: bool) -> String {
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
    pub(super) fn render_locations(&self, theme: &SbTheme, cx: &mut Context<Self>) -> AnyElement {
        let sync = self.sync.read(cx);
        let locations = sync.locations().unwrap_or_default();
        if locations.is_empty() {
            return div()
                .flex_none()
                .pt(theme.spacing.xs)
                .pb(theme.spacing.md)
                .children(sync.locations().map(|_| note(NO_LOCATIONS, theme)))
                .into_any_element();
        }
        let now = format::now();
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(2.))
            .children(locations.iter().map(|location| {
                location_row(
                    location,
                    sync.status(location),
                    sync.indexing(Some(&location.id)),
                    now,
                    theme,
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
        .flex()
        .items_center()
        .gap(theme.spacing.md)
        .px(theme.spacing.md)
        .rounded(theme.radii.row)
        .whitespace_nowrap()
        .child(icon(glyph, theme.sizes.icon, theme.colors.fg3))
}

/// A row's name (DESIGN §4.1: 12px at 500, `fg2`).
fn name(name: &str, theme: &SbTheme) -> Div {
    div()
        .text_color(theme.colors.fg2)
        .text_spec(&theme.text.control_small)
        .child(name.to_string())
}

/// A row's second line.
fn line(text: String, color: Hsla, theme: &SbTheme) -> Div {
    div()
        .min_w_0()
        .overflow_hidden()
        .text_ellipsis()
        .text_color(color)
        .text_spec(&theme.text.small)
        .child(text)
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
    now: i64,
    theme: &SbTheme,
) -> AnyElement {
    let c = &theme.colors;
    let kind = kind_name(location.kind);
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
    let row = row(
        format!("location-{}", location.id),
        match location.kind {
            LocationKind::Local => SbIcon::Monitor,
        },
        theme,
    );
    // A state that is one line under the name.
    let resting = |row: Stateful<Div>, state: Div| {
        row.h(theme.sizes.location_row_height).child(
            div()
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
            doing_line(location.kind, &progress, first),
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
            row,
            line(
                format!("{kind} · synced {}", format::ago(at, now)),
                c.fg3,
                theme,
            ),
        )
        .child(dot(Some(c.green))),
        (LocationStatus::NeverSynced, None) => {
            resting(row, line(format!("{kind} · not synced yet"), c.fg3, theme)).child(dot(None))
        }
        (LocationStatus::Failed(_), None) => resting(
            row,
            div()
                .flex()
                .items_center()
                .gap(theme.spacing.xs)
                .min_w_0()
                .child(icon(SbIcon::CircleAlert, theme.sizes.icon_xs, c.red))
                .child(line(format!("{kind} · last sync failed"), c.red, theme)),
        ),
    }
    .automation_target_with(move || target.clone().into())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use sb_core::sync::Progress;

    use super::*;

    #[test]
    fn a_syncing_row_says_what_its_pass_is_doing() {
        let mut progress = PassProgress::default();
        let local = LocationKind::Local;
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
        assert_eq!(doing(&progress).as_deref(), Some("38 / 212"));
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
        assert_eq!(doing(&progress).as_deref(), Some("362 / 1,418"));
        assert_eq!(doing_line(local, &progress, false), "Local · indexing");
    }
}
