//! The sidebar (PLAN §11 `sidebar`, DESIGN §4.1): the library rows, the folder tree and the
//! locations at its foot.
//!
//! Nothing is archived yet, so it draws its empty state (DESIGN §4.10): the library rows
//! disabled with counts of 0, no folders, no locations, and New folder and Sync all disabled.
//! Phase 3 turns it into a view over the `Library` and `Sync` services (PLAN §13).

use gpui::{
    AnyElement, InteractiveElement as _, IntoElement, ParentElement as _, Role,
    StatefulInteractiveElement as _, Styled as _, div, px,
};

use crate::automation::AutomationTargetExt as _;
use crate::components::{DISABLED_OPACITY, count, icon, overline};
use crate::icons::SbIcon;
use crate::theme::{SbTheme, StyledText as _};

/// How the library rows are drawn while they can't be opened (DESIGN §4.10).
const DISABLED_ROWS_OPACITY: f32 = 0.55;

/// The library rows, top to bottom (DESIGN §4.1): icon, name and automation target.
const LIBRARY_ROWS: [(SbIcon, &str, &str); 4] = [
    (SbIcon::Layers, "All sessions", "sidebar/all-sessions"),
    (SbIcon::Star, "Starred", "sidebar/starred"),
    (SbIcon::Inbox, "Unsorted", "sidebar/unsorted"),
    (SbIcon::Trash, "Trash", "sidebar/trash"),
];

/// What the Folders and Locations sections say while they have no rows (DESIGN §4.10).
const NO_FOLDERS: &str = "Folders come after the first sync.";
const NO_LOCATIONS: &str = "None yet";

/// The sidebar on `background`: `bg-side` docked, its solid value as an overlay (DESIGN §2).
pub fn sidebar(background: gpui::Hsla, theme: &SbTheme) -> AnyElement {
    let c = &theme.colors;
    div()
        .id("sidebar")
        .role(Role::Navigation)
        .aria_label("Library")
        .flex_none()
        .flex()
        .flex_col()
        .w(theme.sizes.sidebar_width)
        .h_full()
        .pt(px(10.))
        .px(theme.spacing.md)
        .pb(theme.spacing.md)
        .bg(background)
        .border_r_1()
        .border_color(c.line_subtle)
        .debug_selector(|| "sidebar".into())
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(1.))
                .opacity(DISABLED_ROWS_OPACITY)
                .children(
                    LIBRARY_ROWS
                        .into_iter()
                        .map(|(glyph, name, target)| library_row(glyph, name, target, theme)),
                ),
        )
        .child(
            section_header(theme)
                .pt(theme.spacing.xl)
                .pb(theme.spacing.sm)
                .child(overline("Folders", theme))
                .child(header_button(
                    SbIcon::Plus,
                    "New folder",
                    "sidebar/new-folder",
                    theme,
                )),
        )
        .child(note(NO_FOLDERS, theme))
        .child(div().flex_1())
        .child(
            div()
                .flex_none()
                .h(px(1.))
                .mx(-theme.spacing.md)
                .mb(theme.spacing.md)
                .bg(c.line_subtle),
        )
        .child(
            section_header(theme)
                .pb(theme.spacing.xs)
                .child(overline("Locations", theme))
                .child(header_button(
                    SbIcon::RefreshCw,
                    "Sync all locations",
                    "sidebar/sync-all",
                    theme,
                )),
        )
        .child(
            div()
                .pt(theme.spacing.xs)
                .pb(theme.spacing.md)
                .child(note(NO_LOCATIONS, theme)),
        )
        .automation_target("shell/sidebar")
}

/// A library row (DESIGN §4.1): 32 high, radius 8, icon `fg3`, name `fg2`, its count at the
/// right.
fn library_row(
    glyph: SbIcon,
    name: &'static str,
    target: &'static str,
    theme: &SbTheme,
) -> AnyElement {
    let c = &theme.colors;
    div()
        .flex()
        .items_center()
        .gap(theme.spacing.md)
        .h(theme.sizes.sidebar_row_height)
        .px(theme.spacing.md)
        .rounded(theme.radii.row)
        .text_color(c.fg2)
        .whitespace_nowrap()
        .child(icon(glyph, px(16.), c.fg3))
        .child(name)
        .child(count(0, theme))
        .automation_target(target)
}

/// The row a section's overline and its button sit in.
fn section_header(theme: &SbTheme) -> gpui::Div {
    div().flex().items_center().px(theme.spacing.md)
}

/// A section's button at the right of its overline (22 square), disabled.
fn header_button(
    glyph: SbIcon,
    label: &'static str,
    target: &'static str,
    theme: &SbTheme,
) -> AnyElement {
    div()
        .id(target)
        .role(Role::Button)
        .aria_label(label)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .ml_auto()
        .size(px(22.))
        .rounded(theme.radii.control)
        .opacity(DISABLED_OPACITY)
        .child(icon(glyph, px(14.), theme.colors.fg2))
        .automation_target(target)
}

/// What an empty section says in place of its rows.
fn note(text: &'static str, theme: &SbTheme) -> impl IntoElement {
    div()
        .px(theme.spacing.md)
        .text_color(theme.colors.fg3)
        .text_spec(&theme.text.secondary)
        .child(text)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn the_rows_and_the_notes_read_as_design_writes_them() {
        // DESIGN §4.1: the library rows top to bottom, each with its icon (§1.5).
        assert_eq!(
            LIBRARY_ROWS,
            [
                (SbIcon::Layers, "All sessions", "sidebar/all-sessions"),
                (SbIcon::Star, "Starred", "sidebar/starred"),
                (SbIcon::Inbox, "Unsorted", "sidebar/unsorted"),
                (SbIcon::Trash, "Trash", "sidebar/trash"),
            ]
        );
        // DESIGN §4.10.
        assert_eq!(NO_FOLDERS, "Folders come after the first sync.");
        assert_eq!(NO_LOCATIONS, "None yet");
    }
}
