//! Settings › Sync (DESIGN §4.13, `E-Settings`): Sync every, a menu of the intervals; Stale
//! after, a number of days, with what the locations' agents say of how long they keep
//! transcripts; and that any sync starts a stopped WSL distro.

use gpui::{
    Anchor, AnyElement, Context, Focusable as _, InteractiveElement as _, IntoElement,
    ParentElement as _, Role, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    anchored, deferred, div, prelude::FluentBuilder as _, px,
};
use gpui_component::input::Input;

use super::{SettingsPage, section_head, setting_row};
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{icon, info_banner, tab_stop};
use crate::icons::SbIcon;
use crate::services::ActiveServices as _;
use crate::services::SyncEvery;
use crate::services::settings::WAIT_FOR_THE_MOVE;
use crate::theme::{SbTheme, StyledText as _};

/// The menu button's width (`E-Settings`).
const MENU_BUTTON: f32 = 150.;
/// The Stale after field's width (`E-Settings`).
const DAYS_FIELD: f32 = 64.;

impl SettingsPage {
    pub(super) fn sync_section(
        &self,
        window: &mut Window,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let focused = self
            .stale_after
            .read(cx)
            .focus_handle(cx)
            .is_focused(window);
        div()
            .flex()
            .flex_col()
            .gap(theme.spacing.lg)
            .child(section_head(
                "Sync",
                Some("When locations sync, and when one counts as stale."),
                theme,
            ))
            .children(cx.services().settings.read(cx).is_moving().then(|| {
                div()
                    .text_spec(&theme.text.secondary)
                    .text_color(c.fg2)
                    .child(format!(
                        "{WAIT_FOR_THE_MOVE} Until then, these settings aren't changed."
                    ))
                    .automation_target("settings/sync/wait")
            }))
            .child(setting_row(
                "Sync every",
                "While the app is open. Every launch syncs too, and Sync all runs whenever you \
                 ask.",
                self.every_menu(theme, cx),
                theme,
            ))
            .child(setting_row(
                "Stale after",
                format!(
                    "A location that hasn't synced this long is flagged in the sidebar. {}",
                    cleanup(cx)
                ),
                div()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.md)
                    .child(
                        div()
                            .w(px(DAYS_FIELD))
                            .h(theme.sizes.input_height)
                            .flex()
                            .items_center()
                            .px(theme.sizes.list_row_padding)
                            .rounded(theme.radii.input)
                            .border_1()
                            .border_color(if focused { c.accent } else { c.line })
                            .bg(c.s1)
                            .text_color(c.fg)
                            .text_spec(&theme.text.number)
                            .child(Input::new(&self.stale_after).appearance(false).px_0())
                            .automation_target("settings/sync/stale-after"),
                    )
                    .child(
                        div()
                            .text_spec(&theme.text.secondary)
                            .text_color(c.fg2)
                            .child("days"),
                    ),
                theme,
            ))
            .child(info_banner(theme).child(div().flex_1().min_w_0().child(
                "Any sync, scheduled or manual, starts a stopped WSL distro. Listing \
                     distros never does.",
            )))
            .into_any_element()
    }

    /// The Sync every button and, while it is open, its menu under it.
    fn every_menu(&self, theme: &SbTheme, cx: &mut Context<Self>) -> AnyElement {
        let c = &theme.colors;
        let every = cx.services().sync.read(cx).every();
        let menu = self.menu.then(|| {
            div()
                .absolute()
                .top(theme.sizes.control_height + theme.spacing.xs)
                .right_0()
                .child(
                    deferred(
                        anchored().anchor(Anchor::TopRight).snap_to_window().child(
                            div()
                                .id("settings-every-menu")
                                .role(Role::Menu)
                                .flex()
                                .flex_col()
                                .p(theme.spacing.xs)
                                .min_w(px(MENU_BUTTON))
                                .rounded(theme.radii.row)
                                .border_1()
                                .border_color(c.line)
                                .bg(c.s2)
                                .shadow(theme.shadows.pop.clone())
                                .occlude()
                                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                    this.menu = false;
                                    cx.notify();
                                }))
                                .children(SyncEvery::ALL.map(|choice| {
                                    let on = choice == every;
                                    tab_stop(
                                        div().id(SharedString::from(format!(
                                            "settings-every-{}",
                                            choice.key()
                                        ))),
                                        theme,
                                    )
                                    .role(Role::MenuItemRadio)
                                    .debug_selector(move || {
                                        format!("settings-every-{}", choice.key())
                                    })
                                    .aria_label(choice.label())
                                    .flex()
                                    .items_center()
                                    .gap(theme.spacing.md)
                                    .h(theme.sizes.control_height)
                                    .px(theme.spacing.md)
                                    .rounded(px(5.))
                                    .border_1()
                                    .border_color(gpui::transparent_black())
                                    .text_spec(&theme.text.body)
                                    .text_color(c.fg)
                                    .whitespace_nowrap()
                                    .cursor_pointer()
                                    .when(on, |this| this.bg(c.s3))
                                    .hover(|style| style.bg(c.s3))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.menu = false;
                                        let settings = cx.services().settings.clone();
                                        settings.update(cx, |settings, cx| {
                                            settings.set_sync_every(choice, cx);
                                        });
                                    }))
                                    .child(div().flex_1().child(choice.label()))
                                    .when(on, |this| {
                                        this.child(icon(SbIcon::Check, theme.sizes.icon_sm, c.fg2))
                                    })
                                    .automation_target_with(move || {
                                        format!("settings/sync/every/{}", choice.key()).into()
                                    })
                                })),
                        ),
                    )
                    .with_priority(1),
                )
        });
        div()
            .relative()
            .child(
                tab_stop(div().id("settings-every"), theme)
                    .role(Role::Button)
                    .debug_selector(|| "settings-every".into())
                    .aria_label(format!("Sync every: {}", every.label()))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(theme.spacing.sm)
                    .w(px(MENU_BUTTON))
                    .h(theme.sizes.control_height)
                    .px(px(10.))
                    .rounded(theme.radii.control)
                    .border_1()
                    .border_color(c.line)
                    .bg(c.s2)
                    .text_color(c.fg)
                    .text_spec(&theme.text.control)
                    .whitespace_nowrap()
                    .cursor_pointer()
                    .hover(|style| style.bg(c.s3))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.menu = !this.menu;
                        cx.notify();
                    }))
                    .child(every.label())
                    .child(icon(SbIcon::ChevronDown, theme.sizes.icon_xs, c.fg2))
                    .automation_target("settings/sync/every"),
            )
            .children(menu)
            .into_any_element()
    }
}

/**
 * What the locations' agents say of how long they keep transcripts, as last read there
 * (DESIGN §4.13): one sentence for all when they agree, else the soonest with its location's
 * name. Nothing when no location has said.
 */
fn cleanup(cx: &gpui::App) -> String {
    let sync = cx.services().sync.read(cx);
    let locations = sync.locations().unwrap_or_default();
    let said: Vec<(&str, &str, u32)> = locations
        .iter()
        .flat_map(|location| {
            sync.details()
                .found(&location.id)
                .iter()
                .filter_map(move |found| {
                    Some((
                        location.name.as_str(),
                        found.provider.as_str(),
                        found.retention_days?,
                    ))
                })
        })
        .collect();
    let Some(&(name, provider, days)) = said.iter().min_by_key(|(_, _, days)| *days) else {
        return String::new();
    };
    let agent = agents::name(provider, cx);
    let everywhere = said
        .iter()
        .all(|(_, other, other_days)| *other == provider && *other_days == days)
        && locations
            .iter()
            .all(|location| said.iter().any(|(at, ..)| *at == location.name));
    let on = match locations.len() {
        _ if !everywhere => {
            return format!(
                "{agent} on {name} deletes transcripts after {days} days, the soonest of your \
                 locations, so keep this well under that."
            );
        }
        1 => format!("on {name}"),
        2 => "on both of your locations".to_string(),
        all => format!("on all {} of your locations", number(all)),
    };
    format!("{agent} deletes transcripts after {days} days {on}, so keep this well under that.")
}

/// `3` as `three`, up to ten, as the boards write a count in a sentence.
fn number(count: usize) -> String {
    const WORDS: [&str; 11] = [
        "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    ];
    WORDS
        .get(count)
        .map_or_else(|| count.to_string(), |word| (*word).to_string())
}
