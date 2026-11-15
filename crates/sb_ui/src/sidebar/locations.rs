//! The locations at the sidebar's foot (DESIGN §4.1): one row per location with its sync
//! state, and the glyph that syncs them all. The rows open the locations sheet once there is
//! one (PLAN §13).

use gpui::{
    AnyElement, Context, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Role,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _,
    px,
};
use sb_core::location::{Location, LocationKind};

use super::{NO_LOCATIONS, Sidebar, note};
use crate::automation::AutomationTargetExt as _;
use crate::components::{icon, kind_name, progress_bar};
use crate::format;
use crate::icons::SbIcon;
use crate::services::{LocationStatus, PassProgress};
use crate::theme::{SbTheme, StyledText as _};

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
            .children(
                locations
                    .iter()
                    .map(|location| location_row(location, sync.status(location), now, theme)),
            )
            .into_any_element()
    }
}

/// A location's row (DESIGN §4.1): 40 high, its kind's icon, its name at 500, and its state
/// on a second line; 48 high with a bar while it syncs.
fn location_row(
    location: &Location,
    status: LocationStatus,
    now: i64,
    theme: &SbTheme,
) -> AnyElement {
    let c = &theme.colors;
    let kind = kind_name(location.kind);
    let name = div()
        .text_color(c.fg2)
        .text_spec(&theme.text.control_small)
        .child(location.name.clone());
    let line = |text: String, color: Hsla| {
        div()
            .min_w_0()
            .overflow_hidden()
            .text_ellipsis()
            .text_color(color)
            .text_spec(&theme.text.small)
            .child(text)
    };
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
    let row = div()
        .id(SharedString::from(format!("location-{}", location.id)))
        .role(Role::ListItem)
        .flex()
        .items_center()
        .gap(theme.spacing.md)
        .px(theme.spacing.md)
        .rounded(theme.radii.row)
        .whitespace_nowrap()
        .child(icon(
            match location.kind {
                LocationKind::Local => SbIcon::Monitor,
            },
            px(16.),
            c.fg3,
        ));
    let target = format!("sidebar/location/{}", location.name);
    match status {
        LocationStatus::Syncing {
            progress, first, ..
        } => {
            row.h(theme.sizes.location_row_syncing_height)
                .aria_label(format!("{}: syncing", location.name))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(3.))
                        .child(div().flex().items_center().child(name).children(
                            doing(&progress).map(|doing| {
                                div()
                                    .ml_auto()
                                    .text_color(c.accent_text)
                                    .text_spec(&theme.text.count)
                                    .child(doing)
                            }),
                        ))
                        .child(progress_bar(progress.fraction(), theme))
                        .child(line(doing_line(location.kind, &progress, first), c.fg3)),
                )
        }
        LocationStatus::Synced(at) => row
            .h(theme.sizes.location_row_height)
            .child(div().flex().flex_col().min_w_0().child(name).child(line(
                format!("{kind} · synced {}", format::ago(at, now)),
                c.fg3,
            )))
            .child(dot(Some(c.green))),
        LocationStatus::NeverSynced => row
            .h(theme.sizes.location_row_height)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .child(name)
                    .child(line(format!("{kind} · not synced yet"), c.fg3)),
            )
            .child(dot(None)),
        LocationStatus::Failed(why) => row.h(theme.sizes.location_row_height).child(
            div().flex().flex_col().min_w_0().child(name).child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.xs)
                    .min_w_0()
                    .child(icon(SbIcon::CircleAlert, px(12.), c.red))
                    .child(line(format!("{kind} · failed · {why}"), c.red)),
            ),
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
