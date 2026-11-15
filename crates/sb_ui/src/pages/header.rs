//! The header every library page starts with (DESIGN §2, §4.4–§4.6): the icon tile, the title
//! at 24/600, and under it the page's stats or a line about it.

use gpui::{
    AnyElement, InteractiveElement as _, IntoElement, ParentElement as _, Pixels, SharedString,
    Styled as _, div, px,
};

use crate::automation::AutomationTargetExt as _;
use crate::components::page_tile;
use crate::format;
use crate::icons::SbIcon;
use crate::services::Stats;
use crate::theme::{SbTheme, StyledText as _};

/// The gap between a header's stats (`design/project/sb.css`, `.stats`).
const STATS_GAP: Pixels = px(28.);

/// What a header shows under its title.
pub enum Under {
    /// Sessions, tool calls, span and locations (DESIGN §4.4).
    Stats(Stats),
    /// A sentence about the page.
    Line(SharedString),
}

/// A page's header. `accent` tints a folder's tile.
pub fn header(
    glyph: SbIcon,
    accent: bool,
    title: impl Into<SharedString>,
    under: Under,
    theme: &SbTheme,
) -> AnyElement {
    let c = &theme.colors;
    div()
        .flex_none()
        .flex()
        .items_start()
        .gap(theme.spacing.xl)
        .debug_selector(|| "page-header".into())
        .child(page_tile(glyph, accent, theme))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(theme.spacing.md)
                .child(
                    div()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_spec(&theme.text.page_title)
                        .child(title.into())
                        .automation_target("page/title"),
                )
                .child(match under {
                    Under::Stats(stats) => div()
                        .flex()
                        .gap(STATS_GAP)
                        .children(stat_values(&stats).into_iter().map(|(value, label)| {
                            div()
                                .flex()
                                .flex_col()
                                .child(div().text_spec(&theme.text.stat).child(value))
                                .child(
                                    div()
                                        .text_color(c.fg3)
                                        .text_spec(&theme.text.secondary)
                                        .child(label),
                                )
                        }))
                        .into_any_element(),
                    Under::Line(line) => div()
                        .max_w(px(760.))
                        .text_color(c.fg2)
                        .child(line)
                        .into_any_element(),
                }),
        )
        .automation_target("page/header")
}

/// The four stats as their values and labels, one of a thing in the singular. A page with no
/// sessions has no span and no locations to count (DESIGN §4.4, "Empty folder").
fn stat_values(stats: &Stats) -> [(String, &'static str); 4] {
    let dash = || "—".to_string();
    let one = |of: u64, one, several| if of == 1 { one } else { several };
    [
        (
            format::count(stats.sessions as u64),
            one(stats.sessions as u64, "session", "sessions"),
        ),
        (
            format::count(stats.calls),
            one(stats.calls, "tool call", "tool calls"),
        ),
        (
            stats
                .span
                .map_or_else(dash, |(from, to)| format::span(from, to)),
            "span",
        ),
        (
            if stats.sessions == 0 {
                dash()
            } else {
                format::count(stats.locations as u64)
            },
            one(stats.locations as u64, "location", "locations"),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn an_empty_page_has_dashes_for_what_it_cant_count() {
        let values = |stats| stat_values(&stats).map(|(value, _)| value);
        assert_eq!(values(Stats::default()), ["0", "0", "—", "—"]);
        assert_eq!(
            values(Stats {
                sessions: 488,
                calls: 52_914,
                span: None,
                locations: 3,
            }),
            ["488", "52,914", "—", "3"]
        );
        assert_eq!(
            stat_values(&Stats {
                sessions: 1,
                calls: 1,
                span: None,
                locations: 1,
            })
            .map(|(_, label)| label),
            ["session", "tool call", "span", "location"]
        );
        assert_eq!(
            stat_values(&Stats::default()).map(|(_, label)| label),
            ["sessions", "tool calls", "span", "locations"]
        );
    }
}
