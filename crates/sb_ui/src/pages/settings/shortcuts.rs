//! Settings › Shortcuts (DESIGN §4.13, §5): a read-only table of what each key does, with
//! keycaps in this platform's spelling, made from the keymap itself
//! ([`keymap::SHORTCUT_ROWS`]), so it can't say other keys than the ones bound.

use gpui::{AnyElement, IntoElement, ParentElement as _, Styled as _, div, px};

use super::section_head;
use crate::automation::AutomationTargetExt as _;
use crate::components::kbd;
use crate::keymap;
use crate::theme::{SbTheme, StyledText as _};

/// The table's header row and its rows (`E-Settings`).
const HEADER: f32 = 28.;
const ROW: f32 = 30.;

pub(super) fn section(theme: &SbTheme) -> AnyElement {
    let c = &theme.colors;
    div()
        .flex()
        .flex_col()
        .gap(theme.spacing.lg)
        .child(section_head("Shortcuts", Some("Read-only for now."), theme))
        .child(
            div()
                .flex()
                .flex_col()
                .rounded(theme.radii.row)
                .border_1()
                .border_color(c.line_subtle)
                .overflow_hidden()
                .text_spec(&theme.text.row)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .h(px(HEADER))
                        .px(theme.spacing.lg)
                        .bg(c.s1)
                        .text_spec(&theme.text.tag)
                        .text_color(c.fg3)
                        .child(div().flex_1().child("Action"))
                        .child("Keys"),
                )
                .children(keymap::SHORTCUT_ROWS.iter().enumerate().map(|(at, row)| {
                    div()
                        .flex()
                        .items_center()
                        .gap(theme.spacing.xs)
                        .min_h(px(ROW))
                        .py(px(4.))
                        .px(theme.spacing.lg)
                        .border_t_1()
                        .border_color(c.line_subtle)
                        .child(div().flex_1().min_w_0().text_color(c.fg2).child(row.does))
                        .children(row.keycaps().into_iter().map(|keys| kbd(keys, theme)))
                        .automation_target_with(move || format!("settings/shortcuts/{at}").into())
                }))
                .automation_target("settings/shortcuts"),
        )
        .into_any_element()
}
