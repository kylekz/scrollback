//! Settings (DESIGN §4.13, `E-Settings-Archive`, `E-Settings`), in the main area in place of
//! the page it was opened from, the sidebar beside it: a 44px header with Back, a list of the
//! sections, and the sections in one column that scrolls.
//!
//! **The list follows the column.** The section at the top of the column is the one picked in
//! the list; a click on one scrolls the column to it, and it stays picked until the column is
//! scrolled again, even where the column can't scroll so far that it is at the top.
//!
//! **Leaving.** Back, `Esc` and `Alt ←` go back to the page Settings was opened from, which is
//! as it was; so does picking a page in the sidebar, which shows that page. `Esc` first closes
//! what is open on the page: the confirmation of another folder, then a menu. Stale after keeps
//! what was typed in it when it loses the keyboard, on `Enter`, and when Settings is left.
//!
//! The sections are drawn in [`archive`], [`sync`], [`appearance`] and [`shortcuts`]. What they
//! show comes from the services: the app's settings ([`crate::services::SettingsService`]), the
//! archive's upkeep ([`crate::services::UpkeepService`]) and sync's own settings. Nothing here
//! opens anything outside scrollback but through [`crate::system`].
//!
//! **The version** sits at the column's foot, under the sections, with the line on
//! scrollback's own updates under it and Restart to update once one is ready
//! ([`crate::update_line`]).

mod appearance;
mod archive;
mod shortcuts;
mod sync;
#[cfg(test)]
mod tests;

use std::path::PathBuf;

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, Role, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::input::{InputEvent, InputState};
use sb_core::archive::Holds;
use sb_core::catalogue::Shelf;

use crate::automation::AutomationTargetExt as _;
use crate::components::{bad_banner, icon, icon_button};
use crate::icons::SbIcon;
use crate::services::{ActiveServices as _, SyncService};
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _};

/// Settings' sections, in their order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Archive,
    Sync,
    Appearance,
    Shortcuts,
}

impl Section {
    pub const ALL: [Self; 4] = [Self::Archive, Self::Sync, Self::Appearance, Self::Shortcuts];

    fn label(self) -> &'static str {
        match self {
            Self::Archive => "Archive",
            Self::Sync => "Sync",
            Self::Appearance => "Appearance",
            Self::Shortcuts => "Shortcuts",
        }
    }

    fn icon(self) -> SbIcon {
        match self {
            Self::Archive => SbIcon::Archive,
            Self::Sync => SbIcon::RefreshCw,
            Self::Appearance => SbIcon::Sun,
            Self::Shortcuts => SbIcon::Keyboard,
        }
    }

    /// `"archive"`, `"sync"`, `"appearance"` or `"shortcuts"`, as automation's targets and
    /// `state` spell it.
    pub fn key(self) -> &'static str {
        match self {
            Self::Archive => "archive",
            Self::Sync => "sync",
            Self::Appearance => "appearance",
            Self::Shortcuts => "shortcuts",
        }
    }
}

/// A folder chosen as the archive's, waiting for the owner to confirm (DESIGN §4.13).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Chosen {
    folder: PathBuf,
    /// What the folder holds: an archive to open, or nothing, to make one in.
    holds: Holds,
}

pub struct SettingsPage {
    scroll: ScrollHandle,
    /// The section picked in the list, until the column is scrolled.
    picked: Option<Section>,
    /// The Sync every menu is open.
    menu: bool,
    /// The Stale after field.
    stale_after: Entity<InputState>,
    /// The sync service the field was filled from, and keeps what is typed to: once the app
    /// has moved onto another archive, that one is closed and takes nothing.
    stale_sync: Entity<SyncService>,
    /// Whether Settings was shown when the app's settings last changed.
    was_shown: bool,
    /// A folder chosen as the archive's, being looked at or waiting to be confirmed.
    chosen: Option<Chosen>,
    /// Where the keyboard rests in the confirmation, and where it was before.
    confirm_focus: FocusHandle,
    /// The confirmation's Cancel, which has the keyboard as it opens.
    cancel_focus: FocusHandle,
    /// While the confirmation is open: where the keyboard was before it.
    confirm_back: Option<Option<FocusHandle>>,
    _subscriptions: [Subscription; 3],
}

impl SettingsPage {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let services = cx.services();
        let (stale_sync, settings, updates) = (
            services.sync.clone(),
            services.settings.clone(),
            services.updates.clone(),
        );
        let days = stale_sync.read(cx).stale_after_days();
        let stale_after = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(days.to_string())
                .validate(|text, _| text.len() <= 2 && text.chars().all(|c| c.is_ascii_digit()))
        });
        Self {
            _subscriptions: [
                cx.subscribe_in(
                    &stale_after,
                    window,
                    |this, _, event, window, cx| match event {
                        InputEvent::Blur | InputEvent::PressEnter { .. } => {
                            this.take_stale_after(window, cx);
                        }
                        _ => {}
                    },
                ),
                /* Leaving Settings takes the field off the screen without it losing the
                keyboard. */
                cx.observe_in(&settings, window, |this, settings, window, cx| {
                    let shown = settings.read(cx).is_shown();
                    if std::mem::replace(&mut this.was_shown, shown) && !shown {
                        this.take_stale_after(window, cx);
                    }
                }),
                /* The update line under the version. The service is the app's, kept when the
                app moves onto another archive. */
                cx.observe(&updates, |_, _, cx| cx.notify()),
            ],
            scroll: ScrollHandle::new(),
            picked: None,
            menu: false,
            stale_after,
            stale_sync,
            was_shown: false,
            chosen: None,
            confirm_focus: cx.focus_handle(),
            cancel_focus: cx.focus_handle(),
            confirm_back: None,
        }
    }

    /// Settings is shown: from its top, with nothing open, and what it shows read again.
    pub fn showing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.scroll.set_offset(gpui::point(px(0.), px(0.)));
        self.picked = None;
        self.menu = false;
        self.chosen = None;
        self.stale_sync = cx.services().sync.clone();
        let days = self.stale_sync.read(cx).stale_after_days();
        self.stale_after.update(cx, |input, cx| {
            input.set_value(days.to_string(), window, cx)
        });
        let upkeep = cx.services().upkeep.clone();
        upkeep.update(cx, |upkeep, cx| upkeep.shown(cx));
        cx.notify();
    }

    /// Whether the confirmation of another folder is open: a dialog over the window, which
    /// keeps the keyboard while it is (`crate::overlay`).
    pub fn is_confirming(&self) -> bool {
        self.chosen.is_some()
    }

    /**
     * Gives the keyboard to the confirmation's Cancel as it opens, and back to where it was as
     * it closes, as `crate::shell`'s dialogs do. Called as the page draws, so that what has the
     * keyboard is drawn in the same frame.
     */
    fn follow_confirmation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match (self.chosen.is_some(), self.confirm_back.is_some()) {
            (true, false) => {
                self.confirm_back = Some(window.focused(cx));
                self.cancel_focus.focus(window, cx);
            }
            (false, true) => {
                if let Some(Some(back)) = self.confirm_back.take() {
                    back.focus(window, cx);
                }
            }
            _ => {}
        }
    }

    /// `Esc`: closes what is open on the page, the confirmation first; `false` when nothing
    /// was, and Settings is left.
    pub fn dismiss(&mut self, cx: &mut Context<Self>) -> bool {
        if self.chosen.take().is_some() {
            cx.notify();
            return true;
        }
        if std::mem::take(&mut self.menu) {
            cx.notify();
            return true;
        }
        false
    }

    /// The section at the top of the column, or the one picked.
    pub fn current(&self) -> Section {
        if let Some(picked) = self.picked {
            return picked;
        }
        let at_end = self.scroll.max_offset().y > px(0.)
            && -self.scroll.offset().y >= self.scroll.max_offset().y - px(1.);
        let index = if at_end {
            Section::ALL.len() - 1
        } else {
            self.scroll.top_item()
        };
        Section::ALL[index.min(Section::ALL.len() - 1)]
    }

    /// Scrolls the column to `section`, which then sits below the column's top padding as the
    /// first does, or as near as the column scrolls.
    pub fn go_to(&mut self, section: Section, cx: &mut Context<Self>) {
        let index = Section::ALL.iter().position(|it| *it == section);
        if let (Some(first), Some(item)) = (
            self.scroll.bounds_for_item(0),
            index.and_then(|index| self.scroll.bounds_for_item(index)),
        ) {
            let below = (item.top() - first.top()).min(self.scroll.max_offset().y);
            self.scroll.set_offset(gpui::point(px(0.), -below));
        }
        self.picked = Some(section);
        cx.notify();
    }

    /// Keeps what was typed in Stale after, within its range; anything else is put back as it
    /// was.
    fn take_stale_after(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed: Option<u32> = self.stale_after.read(cx).value().trim().parse().ok();
        let sync = self.stale_sync.clone();
        if let Some(days) = typed {
            sync.update(cx, |sync, cx| sync.set_stale_after_days(days, cx));
        }
        let days = sync.read(cx).stale_after_days();
        if typed != Some(days) {
            self.stale_after.update(cx, |input, cx| {
                input.set_value(days.to_string(), window, cx)
            });
        }
    }

    /// Leaves Settings for the page it was opened from.
    fn back(&mut self, cx: &mut Context<Self>) {
        self.menu = false;
        self.chosen = None;
        let settings = cx.services().settings.clone();
        settings.update(cx, |settings, cx| settings.show(false, cx));
    }

    /// The 44px header (DESIGN §4.13): Back, "Settings", and where they are kept.
    fn header(&self, theme: &SbTheme, cx: &mut Context<Self>) -> AnyElement {
        let c = &theme.colors;
        let back_to = back_to(cx);
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .h(theme.sizes.settings_header_height)
            .pl(theme.spacing.lg)
            .pr(px(14.))
            .border_b_1()
            .border_color(c.line_subtle)
            .child(
                icon_button("settings-back", theme.sizes.control_height, theme)
                    .debug_selector(|| "settings-back".into())
                    .aria_label(format!("Back to {back_to}"))
                    .on_click(cx.listener(|this, _, _, cx| this.back(cx)))
                    .child(icon(SbIcon::ArrowLeft, theme.sizes.icon, c.fg2))
                    .automation_target("settings/back"),
            )
            .child(
                div()
                    .text_spec(&theme.text.title)
                    .text_color(c.fg)
                    .child("Settings"),
            )
            .child(
                div()
                    .text_spec(&theme.text.secondary)
                    .text_color(c.fg3)
                    .whitespace_nowrap()
                    .child(if cfg!(target_os = "macos") {
                        "Stored with this app on this Mac"
                    } else {
                        "Stored with this app on this PC"
                    }),
            )
            .into_any_element()
    }

    /// The list of sections (`srow`), the one at the top of the column picked.
    fn nav(&self, theme: &SbTheme, cx: &mut Context<Self>) -> AnyElement {
        let c = &theme.colors;
        let current = self.current();
        div()
            .id("settings-nav")
            .role(Role::List)
            .aria_label("Settings sections")
            .flex_none()
            .w(theme.sizes.settings_nav_width)
            .flex()
            .flex_col()
            .gap(px(2.))
            .py(px(14.))
            .px(px(10.))
            .border_r_1()
            .border_color(c.line_subtle)
            .children(Section::ALL.map(|section| {
                let on = section == current;
                crate::components::tab_stop(
                    div().id(SharedString::from(format!(
                        "settings-nav-{}",
                        section.key()
                    ))),
                    theme,
                )
                .role(Role::ListItem)
                .debug_selector(move || format!("settings-nav-{}", section.key()))
                .aria_label(section.label())
                .flex()
                .items_center()
                .gap(theme.spacing.md)
                .h(theme.sizes.sidebar_row_height)
                .px(theme.spacing.md)
                .rounded(theme.radii.row)
                .border_1()
                .border_color(gpui::transparent_black())
                .whitespace_nowrap()
                .cursor_pointer()
                .map(|this| {
                    if on {
                        this.bg(c.s3)
                            .text_color(c.fg)
                            .font_weight(theme.text.control.weight)
                    } else {
                        this.text_color(c.fg2).hover(|style| style.bg(c.s2))
                    }
                })
                .on_click(cx.listener(move |this, _, _, cx| this.go_to(section, cx)))
                .child(icon(
                    section.icon(),
                    theme.sizes.icon,
                    if on { c.fg2 } else { c.fg3 },
                ))
                .child(section.label())
                .automation_target_with(move || format!("settings/nav/{}", section.key()).into())
            }))
            .into_any_element()
    }
}

/// What Back says it goes back to: the session open under Settings, or the page.
fn back_to(cx: &App) -> String {
    let library = cx.services().library.read(cx);
    if let Some(session) = library.open_session() {
        return session
            .title
            .clone()
            .unwrap_or_else(|| "the session".to_string());
    }
    match library.shelf() {
        Shelf::All => "All sessions".to_string(),
        Shelf::Starred => "Starred".to_string(),
        Shelf::Unsorted => "Unsorted".to_string(),
        Shelf::Trash => "Trash".to_string(),
        Shelf::Folder(folder) => library
            .folder(folder)
            .map_or_else(|| "the folder".to_string(), |it| it.name.clone()),
    }
}

/// A section's title and what it is about (`h2` 17/24 at 600, and 12px `fg3`).
fn section_head(title: &str, about: Option<&str>, theme: &SbTheme) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(theme.spacing.xs)
        .child(
            div()
                .text_spec(&theme.text.section_title)
                .child(title.to_string()),
        )
        .when_some(about, |this, about| {
            this.child(
                div()
                    .text_spec(&theme.text.secondary)
                    .text_color(theme.colors.fg3)
                    .child(about.to_string()),
            )
        })
        .into_any_element()
}

/// A setting's row (`E-Settings`): its name and what it does on the left, its control on the
/// right, a hairline above.
fn setting_row(
    name: &str,
    about: impl IntoElement,
    control: impl IntoElement,
    theme: &SbTheme,
) -> gpui::Div {
    div()
        .flex()
        .items_start()
        .gap(theme.spacing.xl)
        .py(theme.spacing.lg)
        .border_t_1()
        .border_color(theme.colors.line_subtle)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(theme.spacing.xs)
                .child(div().text_spec(&theme.text.control).child(name.to_string()))
                .child(
                    div()
                        .text_spec(&theme.text.secondary)
                        .text_color(theme.colors.fg3)
                        .child(about),
                ),
        )
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(theme.spacing.md)
                .child(control),
        )
}

/**
 * A segmented control (`.seg`): one segment per choice, the one `on` in `active`. A choice
 * that is `None`'s reason is drawn disabled and takes no clicks.
 */
fn segments<T: Copy + PartialEq + 'static>(
    id: &str,
    choices: Vec<(T, &'static str, &'static str, bool)>,
    on: T,
    theme: &SbTheme,
    pick: impl Fn(T, &mut App) + Clone + 'static,
) -> AnyElement {
    let c = &theme.colors;
    div()
        .id(SharedString::from(id.to_string()))
        .role(Role::Group)
        .flex()
        .gap(px(2.))
        .p(px(2.))
        .rounded(theme.radii.input)
        .border_1()
        .border_color(c.line_subtle)
        .bg(c.s1)
        .children(choices.into_iter().map(|(value, label, target, enabled)| {
            let chosen = value == on;
            let pick = pick.clone();
            div()
                .id(target)
                .role(Role::Button)
                .debug_selector(move || target.to_string())
                .aria_label(label)
                .flex()
                .items_center()
                .h(theme.sizes.segment_height)
                .px(theme.sizes.segment_padding)
                .rounded(theme.radii.segment)
                .border_1()
                .border_color(gpui::transparent_black())
                .text_spec(&theme.text.control_small)
                .text_color(if chosen { c.fg } else { c.fg2 })
                .when(chosen, |this| this.bg(c.active).border_color(c.line_subtle))
                .map(|this| {
                    /* A choice that can't be made is no stop for Tab. */
                    if enabled {
                        crate::components::tab_stop(this, theme)
                            .cursor_pointer()
                            .on_click(move |_, _, cx| pick(value, cx))
                    } else {
                        this.opacity(crate::components::DISABLED_OPACITY)
                    }
                })
                .child(label)
                .automation_target(target)
        }))
        .into_any_element()
}

impl Render for SettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.follow_confirmation(window, cx);
        let theme = cx.sb_theme().clone();
        let column = theme.sizes.settings_column_width - px(64.);
        let sections = [
            self.archive_section(&theme, cx),
            self.sync_section(window, &theme, cx),
            self.appearance_section(&theme, cx),
            shortcuts::section(&theme),
        ];
        let unsaved = cx.services().settings.read(cx).save_failure().map(|why| {
            bad_banner(&theme)
                .mx(px(32.))
                .mt(theme.spacing.lg)
                .max_w(column)
                .child(div().flex_1().min_w_0().child(format!(
                    "Your last change to Settings wasn't saved: {}. scrollback tries again \
                     with your next change, and when it quits.",
                    why.trim_end_matches('.')
                )))
                .automation_target("settings/unsaved")
        });
        div()
            .id("settings")
            .size_full()
            .flex()
            .flex_col()
            .child(self.header(&theme, cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .child(self.nav(&theme, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .children(unsaved)
                            .child(
                                div()
                                    .id("settings-column")
                                    .debug_selector(|| "settings-column".into())
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .track_scroll(&self.scroll)
                                    .on_scroll(cx.listener(|this, _, _, cx| {
                                        this.picked = None;
                                        cx.notify();
                                    }))
                                    .flex()
                                    .flex_col()
                                    .gap(px(26.))
                                    .pt(theme.spacing.xxl)
                                    .px(px(32.))
                                    .pb(px(40.))
                                    .children(sections.into_iter().zip(Section::ALL).map(
                                        |(section, which)| {
                                            div()
                                                .flex_none()
                                                .w_full()
                                                .max_w(column)
                                                .debug_selector(move || {
                                                    format!("settings-section-{}", which.key())
                                                })
                                                .child(section)
                                                .automation_target_with(move || {
                                                    format!("settings/section/{}", which.key())
                                                        .into()
                                                })
                                        },
                                    ))
                                    .child(
                                        div()
                                            .flex_none()
                                            .w_full()
                                            .max_w(column)
                                            .pt(theme.spacing.lg)
                                            .border_t_1()
                                            .border_color(theme.colors.line_subtle)
                                            .debug_selector(|| "settings-version".into())
                                            .child(crate::update_line::render(cx)),
                                    )
                                    .automation_target("settings/column"),
                            ),
                    ),
            )
            .children(self.confirmation(window, &theme, cx))
            .automation_target("settings")
    }
}
