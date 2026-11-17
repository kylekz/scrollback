//! The locations sheet (PLAN §11 `locations_sheet`; DESIGN §4.11, `E-Locations`,
//! `E-Locations-Failed`): every location with its kind, where its agents keep their data, its
//! state and the action that suits it; a stale banner over them for each that is stale; the
//! WSL distros found on this machine and not added; and Add location. The selected location
//! fills the pane beside the list ([`detail`]): its name, what connects to it, its status
//! card, what its agents have there, and Remove.
//!
//! The shell opens it from a sidebar location row (with that location selected), from the
//! Locations overline and with `Ctrl L`. `↑` and `↓` move through the list while it has the
//! keyboard, `Enter` takes the keyboard to the selected location's name, `Esc` closes the sheet
//! (or the dialog over it), and `Tab` stays inside. Closing it cancels a connection test it
//! started.
//!
//! **Removing** asks first (DESIGN §4.11) and goes through the sync service, which stops a
//! running pass before it removes the location; what was archived from it stays.

mod detail;
#[cfg(test)]
mod sheet_tests;

use std::time::Duration;

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, Role, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::FocusTrapElement as _;
use gpui_component::input::{InputEvent, InputState};
use sb_core::location::{Location, LocationId, LocationKind, NAME_CHARS};

use crate::actions::locations::{Close, Confirm, Next, Open, Previous};
use crate::add_location::{AddLocation, AddLocationEvent, Kind, Purpose};
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{
    ButtonKind, bad_banner, button, development_notice, dot, end_cut, focus_ring, icon,
    icon_button, kind_name, kind_tile, location_tag, overline, progress_bar, warn_banner,
    warn_chip_glyph, with_numbers,
};
use crate::format;
use crate::icons::SbIcon;
use crate::keymap;
use crate::overlay::{self, Layer};
use crate::services::sync::{Stale, local_name, refusal};
use crate::services::{
    ActiveServices as _, IndexProgress, IndexStep, LocationStatus, PassProgress, SyncService,
};
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _};

/// How often the sheet's "2 min ago" is drawn again.
const CLOCK_TICK: Duration = Duration::from_secs(30);

/// What the sheet's fixed copy says under its title. It names no agent (PLAN §11).
const ABOUT: &str = "scrollback copies transcripts from these into its archive and reads only \
                     the copies. Nothing is ever written or deleted at a location.";

pub enum LocationsSheetEvent {
    Close,
}

pub struct LocationsSheet {
    sync: Entity<SyncService>,
    focus_handle: FocusHandle,
    list_focus: FocusHandle,
    selected: Option<LocationId>,
    /// The selected location's name, as the owner edits it.
    name: Entity<InputState>,
    /// The owner typed in the name field since its name was last taken.
    name_edited: bool,
    /// Why the name typed was not taken, under the field.
    name_problem: Option<String>,
    /// The place the add dialog added, to select once it is a location.
    to_select: Option<LocationKind>,
    add: Option<Entity<AddLocation>>,
    /// The location whose removal is being confirmed, and the keyboard's place in the
    /// confirmation.
    removing: Option<LocationId>,
    remove_focus: FocusHandle,
    /// Where the keyboard was when a dialog opened over the sheet.
    return_focus: Option<FocusHandle>,
    _subscriptions: [Subscription; 2],
    _dialog: Option<Subscription>,
    _clock: Task<()>,
}

impl EventEmitter<LocationsSheetEvent> for LocationsSheet {}

impl Focusable for LocationsSheet {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl LocationsSheet {
    /// The sheet with `selected` selected, or the first location, the keyboard on its list.
    pub fn new(selected: Option<LocationId>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sync = cx.services().sync.clone();
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                // The field takes no more than a name keeps.
                .validate(|name, _| name.chars().count() <= NAME_CHARS)
        });
        let list_focus = cx.focus_handle().tab_stop(true);
        list_focus.focus(window, cx);
        sync.update(cx, |sync, cx| sync.list_distros(cx));
        let mut this = Self {
            _subscriptions: [
                cx.observe_in(&sync, window, |this, _, window, cx| this.follow(window, cx)),
                cx.subscribe_in(&name, window, |this, _, event, window, cx| match event {
                    InputEvent::Change => {
                        this.name_edited = true;
                        this.name_problem = None;
                    }
                    InputEvent::Blur => this.take_name(window, cx),
                    InputEvent::PressEnter { .. } => {
                        this.take_name(window, cx);
                        this.list_focus.focus(window, cx);
                    }
                    _ => {}
                }),
            ],
            _clock: cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(CLOCK_TICK).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }),
            sync,
            focus_handle: cx.focus_handle(),
            list_focus,
            selected: None,
            name,
            name_edited: false,
            name_problem: None,
            to_select: None,
            add: None,
            removing: None,
            remove_focus: cx.focus_handle(),
            return_focus: None,
            _dialog: None,
        };
        let first = selected.or_else(|| this.locations(cx).first().map(|it| it.id.clone()));
        if let Some(id) = first {
            this.select(id, window, cx);
        }
        this
    }

    /// The selected location.
    pub fn selected(&self) -> Option<&LocationId> {
        self.selected.as_ref()
    }

    /// The dialog over the sheet: `"add"`, `"remove"`, or `None`.
    pub fn dialog(&self) -> Option<&'static str> {
        if self.add.is_some() {
            Some("add")
        } else if self.removing.is_some() {
            Some("remove")
        } else {
            None
        }
    }

    fn locations<'a>(&self, cx: &'a App) -> &'a [Location] {
        self.sync.read(cx).locations().unwrap_or_default()
    }

    /// Selects `id`, its name in the name field; the name typed for the last is taken first.
    fn select(&mut self, id: LocationId, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.as_ref() != Some(&id) {
            self.take_name(window, cx);
        }
        let Some(name) = self.sync.read(cx).location(&id).map(|it| it.name.clone()) else {
            return;
        };
        self.name
            .update(cx, |input, cx| input.set_value(name, window, cx));
        self.selected = Some(id);
        cx.notify();
    }

    /// Keeps the selection on a location: the one just added once it is there, and another
    /// when the selected one is removed.
    fn follow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let locations = self.locations(cx);
        if let Some(added) = self
            .to_select
            .as_ref()
            .and_then(|kind| refusal(kind, locations.iter()))
        {
            let id = added.id.clone();
            self.to_select = None;
            self.select(id, window, cx);
        } else if self
            .selected
            .as_ref()
            .is_none_or(|id| !locations.iter().any(|it| it.id == *id))
            && let Some(first) = locations.first()
        {
            let id = first.id.clone();
            self.select(id, window, cx);
        }
        cx.notify();
    }

    /**
     * Takes the name typed for the selected location (DESIGN §4.11: it renames on blur). A
     * name that is refused (nothing, another location's) is said under the field, and the
     * field shows the location's name again.
     */
    fn take_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected.clone().filter(|_| self.name_edited) else {
            return;
        };
        self.name_edited = false;
        let typed = self.name.read(cx).value().to_string();
        let taken = self
            .sync
            .update(cx, |sync, cx| sync.rename(&id, &typed, cx));
        if let Err(problem) = taken {
            self.name_problem = Some(problem);
            self.restore_name(window, cx);
        }
        cx.notify();
    }

    /// Shows the selected location's name in the field again.
    fn restore_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self
            .selected
            .as_ref()
            .and_then(|id| self.sync.read(cx).location(id).map(|it| it.name.clone()));
        if let Some(name) = name {
            self.name
                .update(cx, |input, cx| input.set_value(name, window, cx));
        }
        self.name_edited = false;
    }

    /// Why the name typed was not taken, if it wasn't.
    pub(super) fn name_problem(&self) -> Option<&str> {
        self.name_problem.as_deref()
    }

    fn step(&mut self, by: isize, window: &mut Window, cx: &mut Context<Self>) {
        let locations = self.locations(cx);
        let at = self
            .selected
            .as_ref()
            .and_then(|id| locations.iter().position(|it| it.id == *id));
        let next = match at {
            Some(at) => at
                .saturating_add_signed(by)
                .min(locations.len().saturating_sub(1)),
            None => 0,
        };
        if let Some(location) = locations.get(next) {
            let id = location.id.clone();
            self.select(id, window, cx);
        }
    }

    /// `Esc`: in a name field that was typed in, the name it had; anywhere else, closes.
    fn close(&mut self, _: &Close, window: &mut Window, cx: &mut Context<Self>) {
        if self.name_edited && self.name.focus_handle(cx).is_focused(window) {
            self.restore_name(window, cx);
            self.name_problem = None;
            self.list_focus.focus(window, cx);
            cx.notify();
        } else {
            cx.emit(LocationsSheetEvent::Close);
        }
    }

    /// The sheet is closing: the name typed is taken, and the connection test the sheet or
    /// its dialog started is cancelled.
    pub fn closing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.take_name(window, cx);
        self.sync.update(cx, |sync, _| sync.cancel_test());
    }

    /**
     * Add again on a removed location, the place `kind` named `name`: back under that name,
     * or, when another location has it now, through the add dialog on that place, where a
     * new one is chosen.
     */
    fn add_again(
        &mut self,
        kind: LocationKind,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let renamed = self
            .sync
            .read(cx)
            .restoring(&kind)
            .is_some_and(|(_, renamed)| renamed);
        if renamed {
            let shown = match &kind {
                LocationKind::Local => Kind::ThisMachine,
                LocationKind::Wsl { .. } => Kind::Wsl,
                LocationKind::Ssh { .. } => Kind::Ssh,
            };
            self.open_add(shown, Some(kind), window, cx);
        } else {
            self.to_select = Some(kind.clone());
            self.sync
                .update(cx, |sync, cx| sync.add(kind, name, false, cx));
        }
    }

    /// Opens the add dialog on `kind`, on the place `place` if one is given.
    fn open_add(
        &mut self,
        kind: Kind,
        place: Option<LocationKind>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.return_focus = window.focused(cx);
        let dialog =
            cx.new(|cx| AddLocation::new(Purpose::Add, kind, place, Vec::new(), window, cx));
        self._dialog = Some(
            cx.subscribe_in(&dialog, window, |this, _, event, window, cx| {
                if let AddLocationEvent::Added(kind) = event {
                    this.to_select = Some(kind.clone());
                }
                this.close_dialog(window, cx);
            }),
        );
        self.add = Some(dialog);
        cx.notify();
    }

    /// Closes the dialog over the sheet, cancelling its connection test, and gives the
    /// keyboard back.
    fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.add.take().is_some() {
            self.sync.update(cx, |sync, _| sync.cancel_test());
        }
        self._dialog = None;
        self.removing = None;
        match self.return_focus.take() {
            Some(focus) => focus.focus(window, cx),
            None => self.list_focus.focus(window, cx),
        }
        cx.notify();
    }

    /// Asks to remove the selected location.
    fn ask_to_remove(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.return_focus = window.focused(cx);
        self.removing = self.selected.clone();
        self.remove_focus.focus(window, cx);
        cx.notify();
    }

    fn remove(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.removing.take() {
            self.sync.update(cx, |sync, cx| sync.remove(&id, cx));
        }
        self.return_focus = Some(self.list_focus.clone());
        self.close_dialog(window, cx);
    }

    /// The sheet's header (DESIGN §4.11): its title and what it is, Sync all and Close.
    fn header(&self, theme: &SbTheme, cx: &mut Context<Self>) -> AnyElement {
        let c = &theme.colors;
        div()
            .flex_none()
            .flex()
            .items_start()
            .gap(theme.spacing.lg)
            .pt(theme.spacing.xl)
            .pb(theme.spacing.lg)
            .px(theme.sizes.sheet_padding)
            .border_b_1()
            .border_color(c.line_subtle)
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(theme.spacing.xs)
                    .child(
                        div()
                            .text_spec(&theme.text.sheet_title)
                            .child("Locations")
                            .automation_target("sheet/title"),
                    )
                    .child(
                        div()
                            .text_color(c.fg3)
                            .text_spec(&theme.text.secondary)
                            .child(ABOUT),
                    ),
            )
            .child(
                button("sheet-sync-all", ButtonKind::Plain, true, true, theme)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sync.update(cx, |sync, cx| sync.sync_all(cx));
                    }))
                    .child(icon(SbIcon::RefreshCw, theme.sizes.icon_sm, c.fg2))
                    .child("Sync all")
                    .automation_target("sheet/sync-all"),
            )
            .child(
                icon_button("sheet-close", theme.sizes.control_height, theme)
                    .aria_label("Close locations (Esc)")
                    .on_click(cx.listener(|this, _, window, cx| this.close(&Close, window, cx)))
                    .child(icon(SbIcon::X, theme.sizes.icon, c.fg2))
                    .automation_target("sheet/close"),
            )
            .into_any_element()
    }

    /// The stale banner for `location` (DESIGN §4.11): how long since it synced, and when
    /// its agent there starts deleting what it has not synced.
    fn stale_banner(
        &self,
        location: &Location,
        stale: &Stale,
        theme: &SbTheme,
        cx: &App,
    ) -> AnyElement {
        let c = &theme.colors;
        let now = format::now();
        let more = match &stale.expiring {
            Some(expiring) if expiring.at <= now => format!(
                "{} there deletes transcripts after {} days, so sessions written since the last \
                 sync may already be expiring. Sync it as soon as you can.",
                agents::name(&expiring.provider, cx),
                expiring.retention_days,
            ),
            Some(expiring) => format!(
                "{} there deletes transcripts after {} days, so sessions written since the last \
                 sync start expiring around {}. Sync it before then.",
                agents::name(&expiring.provider, cx),
                expiring.retention_days,
                format::day(expiring.at, now)
            ),
            None => "Agents delete old transcripts on their own; sync it soon.".to_string(),
        };
        warn_banner(theme)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_color(c.fg)
                            .font_weight(theme.text.control.weight)
                            .child(format!(
                                "{} hasn't synced for {}.",
                                location.name,
                                format::plural(stale.days as usize, "day")
                            )),
                    )
                    .child(div().text_color(c.fg2).child(more)),
            )
            .automation_target_with({
                let name = location.name.clone();
                move || format!("sheet/stale/{name}").into()
            })
    }

    /// A location's row (DESIGN §4.11): its kind's tile, its name and kind, where its agents
    /// keep their data, its state, and the action that suits it.
    fn row(
        &self,
        location: &Location,
        keyboard: bool,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let sync = self.sync.read(cx);
        let now = format::now();
        let details = sync.details();
        let selected = self.selected.as_ref() == Some(&location.id);
        let status = sync.status(location);
        let removing = sync.is_removing(&location.id);
        let roots: Vec<String> = details
            .found(&location.id)
            .iter()
            .filter_map(|found| found.root.as_deref().map(format::path))
            .collect();
        // Numbers in the face for numbers (DESIGN §1.3).
        let line = |text: String, color| {
            div().text_spec(&theme.text.secondary).child(with_numbers(
                text,
                color,
                &theme.text.secondary,
                theme,
            ))
        };
        let archived = details.sessions(&location.id);
        let last_good = location.last_success.map(|at| {
            line(
                format!(
                    "Last good sync {}, {} · {} archived",
                    format::day(at, now),
                    format::clock(at),
                    format::plural(archived, "session")
                ),
                c.fg3,
            )
        });
        let state: Vec<AnyElement> = match (&status, removing) {
            (_, true) => vec![line("Removing…".to_string(), c.fg3).into_any_element()],
            (LocationStatus::Syncing { progress, .. }, _) => vec![
                progress_bar(progress.fraction(), theme).into_any_element(),
                line(syncing_line(progress), c.accent_text).into_any_element(),
            ],
            (LocationStatus::Synced(at), _) => vec![with_dot(
                dot(Some(c.green), theme),
                line(
                    format!(
                        "Synced {} · {}",
                        format::ago(*at, now),
                        format::plural(archived, "session")
                    ),
                    c.fg2,
                ),
                theme,
            )],
            (LocationStatus::Unreachable { since }, _) => {
                let mut lines = vec![with_dot(
                    dot(None, theme),
                    line(
                        format!(
                            "Not reachable since {} · asleep or off the network",
                            format::day(*since, now)
                        ),
                        c.fg2,
                    ),
                    theme,
                )];
                lines.extend(last_good.map(IntoElement::into_any_element));
                lines
            }
            (LocationStatus::Failed(why), _) => {
                let why = agents::named(why, cx);
                let last = why.lines().last().unwrap_or_default().to_string();
                let mut lines = vec![
                    div()
                        .flex()
                        .items_center()
                        .gap(theme.spacing.sm)
                        .min_w_0()
                        .child(icon(SbIcon::CircleAlert, theme.sizes.icon_xs, c.red))
                        .child(
                            line(format!("Failed · {last}"), c.red)
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap(),
                        )
                        .into_any_element(),
                ];
                lines.extend(last_good.map(IntoElement::into_any_element));
                lines
            }
            (
                LocationStatus::Stopped {
                    items_left,
                    bytes_left,
                },
                _,
            ) => {
                let mut lines = vec![with_dot(
                    dot(None, theme),
                    line(stopped_line(*items_left, *bytes_left), c.fg2),
                    theme,
                )];
                lines.extend(last_good.map(IntoElement::into_any_element));
                lines
            }
            (LocationStatus::NeverSynced, _) => vec![with_dot(
                dot(None, theme),
                line(never_line(location, sync), c.fg2),
                theme,
            )],
        };
        let stale = sync.stale(location, now);
        let (action, glyph) = match status {
            LocationStatus::Syncing { .. } => ("Stop", SbIcon::Pause),
            LocationStatus::Unreachable { .. } | LocationStatus::Failed(_) => {
                ("Retry", SbIcon::RefreshCw)
            }
            LocationStatus::Synced(_)
            | LocationStatus::Stopped { .. }
            | LocationStatus::NeverSynced => ("Sync now", SbIcon::RefreshCw),
        };
        let syncing = matches!(status, LocationStatus::Syncing { .. });
        let (id, name) = (location.id.clone(), location.name.clone());
        div()
            .id(SharedString::from(format!("sheet-row-{}", location.id)))
            .role(Role::ListItem)
            .flex()
            .items_start()
            .gap(theme.spacing.lg)
            .p(theme.sizes.list_row_padding)
            .rounded(theme.radii.row)
            .cursor_pointer()
            .map(|this| {
                if selected {
                    this.bg(c.s3)
                        // The list has the keyboard, from the keyboard (DESIGN §5).
                        .when(keyboard, |this| this.shadow(focus_ring(true, theme)))
                } else {
                    this.hover(|style| style.bg(c.s1))
                }
            })
            .on_click(cx.listener({
                let id = id.clone();
                move |this, _, window, cx| this.select(id.clone(), window, cx)
            }))
            .child(kind_tile(&location.kind, theme))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(theme.spacing.xs)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(theme.spacing.sm)
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
                    .children(roots.into_iter().map(|root| {
                        div()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_color(if selected { c.fg2 } else { c.fg3 })
                            .text_spec(&theme.text.path_small)
                            .child(root)
                    }))
                    .children(state)
                    .children(stale.map(|stale| {
                        div().flex().child(warn_chip_glyph(
                            format!(
                                "Stale · {} without a sync",
                                format::plural(stale.days as usize, "day")
                            ),
                            theme,
                        ))
                    })),
            )
            .when(!removing, |this| {
                this.child(
                    button(
                        SharedString::from(format!("sheet-action-{id}")),
                        ButtonKind::Ghost,
                        true,
                        true,
                        theme,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
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
                    .child(icon(glyph, theme.sizes.icon_xs, c.fg2))
                    .child(action)
                    .automation_target_with({
                        let name = name.clone();
                        move || format!("sheet/row/{name}/action").into()
                    }),
                )
            })
            .automation_target_with(move || format!("sheet/row/{name}").into())
    }

    /// The WSL distros found on this machine and not added (DESIGN §4.11), or what `wsl.exe`
    /// said when it could not list them. Nothing where there is no WSL.
    fn found(&self, theme: &SbTheme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let c = &theme.colors;
        let sync = self.sync.read(cx);
        let Some(distros) = sync.distros() else {
            return Vec::new();
        };
        let locations = sync.locations().unwrap_or_default();
        let mut found: Vec<AnyElement> = match &distros.listed {
            Ok(None) => return Vec::new(),
            Err(said) => vec![
                bad_banner(theme)
                    .mx(px(2.))
                    .child(
                        div()
                            .flex_1()
                            .text_spec(&theme.text.secondary)
                            .child(format!("wsl.exe couldn't list the distros: {said}")),
                    )
                    .automation_target("sheet/found/failed"),
            ],
            Ok(Some(_)) => distros
                .offered()
                .filter(|distro| refusal(&wsl(&distro.name), locations.iter()).is_none())
                .map(|distro| {
                    let name = distro.name.clone();
                    div()
                        .flex()
                        .items_start()
                        .gap(theme.spacing.lg)
                        .py(theme.spacing.md)
                        .px(theme.sizes.list_row_padding)
                        .child(
                            div()
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .size(theme.sizes.location_tile)
                                .rounded(theme.radii.input)
                                .bg(c.s1)
                                .child(icon(SbIcon::Box, theme.sizes.icon, c.fg3)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(theme.spacing.xs)
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(theme.spacing.sm)
                                        .child(
                                            div()
                                                .text_color(c.fg2)
                                                .text_spec(&theme.text.control)
                                                .child(distro.name.clone()),
                                        )
                                        .child(location_tag(
                                            "WSL",
                                            &wsl(&distro.name),
                                            false,
                                            theme,
                                        ))
                                        .child(
                                            div()
                                                .text_color(c.fg3)
                                                .text_spec(&theme.text.small)
                                                .child(if distro.running {
                                                    "running"
                                                } else {
                                                    "stopped"
                                                }),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_color(c.fg3)
                                        .text_spec(&theme.text.secondary)
                                        .child(if distro.running {
                                            "Running now. Adding it tests the connection first."
                                        } else {
                                            "Listed without starting it. Adding it doesn't start \
                                             it either; its first sync will."
                                        }),
                                ),
                        )
                        .child(
                            button(
                                SharedString::from(format!("sheet-found-{name}")),
                                ButtonKind::Plain,
                                true,
                                true,
                                theme,
                            )
                            .on_click(cx.listener({
                                let name = name.clone();
                                move |this, _, window, cx| {
                                    let distro = LocationKind::Wsl {
                                        distro: name.clone(),
                                    };
                                    this.open_add(Kind::Wsl, Some(distro), window, cx);
                                }
                            }))
                            .child(icon(SbIcon::Plus, theme.sizes.icon_xs, c.fg2))
                            .child("Add")
                            .automation_target_with({
                                let name = name.clone();
                                move || format!("sheet/found/{name}/add").into()
                            }),
                        )
                        .automation_target_with(move || format!("sheet/found/{name}").into())
                })
                .collect(),
        };
        if found.is_empty() {
            return found;
        }
        found.insert(
            0,
            div()
                .py(theme.spacing.sm)
                .px(theme.sizes.list_row_padding)
                .child(overline(
                    &format!("Found on {}, not added", local_name()),
                    theme,
                ))
                .into_any_element(),
        );
        found.insert(
            0,
            div()
                .flex_none()
                .h(px(1.))
                .my(theme.spacing.md)
                .mx(theme.sizes.list_row_padding)
                .bg(c.line_subtle)
                .into_any_element(),
        );
        found
    }

    /**
     * The locations the owner removed, under the rest: what the archive still holds from
     * each, and Add again, which brings it back under its name, with what was archived
     * from it (FEATURES: Locations). A name another location has taken since is chosen anew
     * in the add dialog, on that place.
     */
    fn removed(&self, theme: &SbTheme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let c = &theme.colors;
        let sync = self.sync.read(cx);
        let mut rows: Vec<AnyElement> = sync
            .removed()
            .iter()
            .map(|location| {
                let sessions = sync.details().sessions(&location.id);
                let (kind, name) = (location.kind.clone(), location.name.clone());
                div()
                    .flex()
                    .items_start()
                    .gap(theme.spacing.lg)
                    .py(theme.spacing.md)
                    .px(theme.sizes.list_row_padding)
                    .child(kind_tile(&location.kind, theme))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(theme.spacing.xs)
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(theme.spacing.sm)
                                    .child(
                                        div()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .whitespace_nowrap()
                                            .text_color(c.fg2)
                                            .text_spec(&theme.text.control)
                                            .child(location.name.clone()),
                                    )
                                    .child(location_tag(
                                        kind_name(&location.kind),
                                        &location.kind,
                                        false,
                                        theme,
                                    )),
                            )
                            .child(div().text_spec(&theme.text.secondary).child(with_numbers(
                                if sessions == 0 {
                                    "Not synced. Nothing was archived from it.".to_string()
                                } else {
                                    format!(
                                        "Not synced. The archive still holds its {}.",
                                        format::plural(sessions, "session")
                                    )
                                },
                                c.fg3,
                                &theme.text.secondary,
                                theme,
                            ))),
                    )
                    .child(
                        button(
                            SharedString::from(format!("sheet-removed-{}", location.id)),
                            ButtonKind::Ghost,
                            true,
                            true,
                            theme,
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.add_again(kind.clone(), name.clone(), window, cx);
                        }))
                        .child(icon(SbIcon::Plus, theme.sizes.icon_xs, c.fg2))
                        .child("Add again")
                        .automation_target_with({
                            let name = location.name.clone();
                            move || format!("sheet/removed/{name}/add").into()
                        }),
                    )
                    .automation_target_with({
                        let name = location.name.clone();
                        move || format!("sheet/removed/{name}").into()
                    })
            })
            .collect();
        if !rows.is_empty() {
            rows.insert(
                0,
                div()
                    .py(theme.spacing.sm)
                    .px(theme.sizes.list_row_padding)
                    .child(overline("Removed", theme))
                    .into_any_element(),
            );
            rows.insert(
                0,
                div()
                    .flex_none()
                    .h(px(1.))
                    .my(theme.spacing.md)
                    .mx(theme.sizes.list_row_padding)
                    .bg(c.line_subtle)
                    .into_any_element(),
            );
        }
        rows
    }

    /// The list (DESIGN §4.11): refusals and stale banners, the locations, what was found and
    /// not added, the locations removed, and Add location at its foot.
    fn list(
        &self,
        width: gpui::Pixels,
        window: &Window,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let keyboard = self.list_focus.is_focused(window) && window.last_input_was_keyboard();
        let c = &theme.colors;
        let now = format::now();
        let locations = self.locations(cx).to_vec();
        let sync = self.sync.read(cx);
        let refused = sync.refused().map(str::to_string);
        let development = sync.development_home().map(str::to_string);
        let kinds = add_kinds(sync);
        let stale: Vec<AnyElement> = locations
            .iter()
            .filter_map(|location| {
                let stale = self.sync.read(cx).stale(location, now)?;
                Some(self.stale_banner(location, &stale, theme, cx))
            })
            .collect();
        let rows: Vec<AnyElement> = locations
            .iter()
            .map(|location| self.row(location, keyboard, theme, cx))
            .collect();
        let found = self.found(theme, cx);
        let removed = self.removed(theme, cx);
        div()
            .id("sheet-list")
            .key_context(keymap::LOCATIONS_LIST)
            .track_focus(&self.list_focus)
            .on_action(cx.listener(|this, _: &Previous, window, cx| this.step(-1, window, cx)))
            .on_action(cx.listener(|this, _: &Next, window, cx| this.step(1, window, cx)))
            .on_action(cx.listener(|this, _: &Open, window, cx| {
                if this.list_focus.is_focused(window) {
                    this.name.update(cx, |name, cx| name.focus(window, cx));
                } else {
                    // A button `Tab` put the keyboard on: Enter presses it.
                    cx.propagate();
                }
            }))
            .role(Role::List)
            .aria_label("Locations")
            .flex_none()
            .w(width)
            .h_full()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(c.line_subtle)
            .child(
                div()
                    .id("sheet-list-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .pt(theme.sizes.list_row_padding)
                    .px(theme.spacing.md)
                    // A build with the development override says so, plainly, at the top.
                    .children(development.map(|home| {
                        development_notice(&home, theme)
                            .mx(px(2.))
                            .mb(theme.spacing.sm)
                            .automation_target("sheet/development")
                    }))
                    .children(refused.map(|refused| {
                        bad_banner(theme)
                            .mx(px(2.))
                            .mb(theme.spacing.sm)
                            .child(
                                div()
                                    .flex_1()
                                    .text_spec(&theme.text.secondary)
                                    .child(refused),
                            )
                            .child(
                                button(
                                    "sheet-refused-dismiss",
                                    ButtonKind::Plain,
                                    true,
                                    true,
                                    theme,
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.sync.update(cx, SyncService::dismiss_refused);
                                }))
                                .child("Dismiss"),
                            )
                            .automation_target("sheet/refused")
                    }))
                    .children(
                        stale
                            .into_iter()
                            .map(|banner| div().mx(px(2.)).mb(theme.spacing.sm).child(banner)),
                    )
                    .children(rows)
                    .children(found)
                    .children(removed),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.md)
                    .pt(theme.spacing.md)
                    .pb(theme.spacing.md)
                    .px(theme.sizes.sheet_padding)
                    .child(
                        button("sheet-add", ButtonKind::Plain, false, true, theme)
                            .on_click(cx.listener(|this, _, window, cx| {
                                let has_wsl = this
                                    .sync
                                    .read(cx)
                                    .distros()
                                    .is_some_and(|it| it.offered().next().is_some());
                                let kind = if has_wsl { Kind::Wsl } else { Kind::Ssh };
                                this.open_add(kind, None, window, cx);
                            }))
                            .child(icon(SbIcon::Plus, theme.sizes.icon_sm, c.fg2))
                            .child("Add location…")
                            .automation_target("sheet/add"),
                    )
                    .child(
                        div()
                            .text_color(c.fg3)
                            .text_spec(&theme.text.secondary)
                            .child(kinds),
                    ),
            )
            .into_any_element()
    }
}

impl Render for LocationsSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.sb_theme().clone();
        let room = overlay::room(window.viewport_size(), &theme);
        let width = theme.sizes.sheet_width.min(room.width);
        let list = theme.sizes.sheet_list_width.min(width * 0.45);
        let selected = self
            .selected
            .as_ref()
            .and_then(|id| self.locations(cx).iter().find(|it| it.id == *id))
            .cloned();
        // What is still synced; and the name typed but not taken where the field has gone.
        if self.name_edited && !self.name.focus_handle(cx).is_focused(window) {
            self.take_name(window, cx);
        }
        let none_left = !self.sync.read(cx).removed().is_empty();
        let detail = match &selected {
            Some(location) => self.detail(location, window, &theme, cx),
            None => div()
                .flex_1()
                .p(theme.spacing.xxl)
                .text_color(theme.colors.fg3)
                .text_spec(&theme.text.secondary)
                .child(if none_left {
                    "No location is synced now. The archive still holds what was copied from \
                     the locations removed; add one again, or another, to sync."
                } else {
                    "No location yet. Add one to start archiving."
                })
                .into_any_element(),
        };
        let header = self.header(&theme, cx);
        let list = self.list(list, window, &theme, cx);
        overlay::surface(&theme)
            .id("locations-sheet")
            .role(Role::Dialog)
            .aria_label("Locations")
            .key_context(keymap::LOCATIONS_SHEET)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::close))
            .w(width)
            .h(theme.sizes.sheet_height.min(room.height))
            .child(header)
            .child(div().flex_1().min_h_0().flex().child(list).child(detail))
            .focus_trap("locations-sheet-trap", &self.focus_handle)
            .automation_target("sheet")
    }
}

impl LocationsSheet {
    /**
     * The dialog over the sheet, if one is open, on its own scrim: the add dialog, or the
     * question before a removal. The shell draws it beside the sheet, each on a layer of its
     * own, so that its scrim covers the sheet too and takes the presses on it.
     */
    pub fn dialog_layer(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = cx.sb_theme().clone();
        let weak = cx.entity().downgrade();
        let dismiss = move |window: &mut Window, cx: &mut App| {
            weak.update(cx, |this, cx| this.close_dialog(window, cx))
                .ok();
        };
        /* A press on the dimmed sheet closes the add dialog, unless that would lose what was
        typed or a test under way: then it does nothing. */
        if let Some(dialog) = self.add.clone() {
            let holding = dialog.clone();
            return Some(overlay::modal(
                dialog,
                Layer::Dialog,
                window,
                &theme,
                move |window, cx| {
                    if !holding.read(cx).holds_input(cx) {
                        dismiss(window, cx);
                    }
                },
            ));
        }
        let location = self
            .removing
            .as_ref()
            .and_then(|id| self.locations(cx).iter().find(|it| it.id == *id))
            .cloned()?;
        let confirm = self.confirm_removal(&location, window, &theme, cx);
        Some(overlay::modal(
            confirm,
            Layer::Confirm,
            window,
            &theme,
            dismiss,
        ))
    }
}

impl LocationsSheet {
    /// "Remove user@macbook?" (DESIGN §4.11): Cancel or Remove; `Enter` presses only the
    /// button `Tab` put the keyboard on, since removing is not to be done by accident.
    fn confirm_removal(
        &self,
        location: &Location,
        window: &Window,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let room = overlay::room(window.viewport_size(), theme);
        let sessions = self.sync.read(cx).details().sessions(&location.id);
        overlay::surface(theme)
            .id("remove-location")
            .role(Role::Dialog)
            .aria_label("Remove location")
            .key_context(keymap::LOCATION_DIALOG)
            .track_focus(&self.remove_focus)
            .on_action(cx.listener(|this, _: &Close, window, cx| this.close_dialog(window, cx)))
            .on_action(cx.listener(|this, _: &Confirm, window, cx| {
                if !this.remove_focus.is_focused(window) {
                    cx.propagate();
                }
            }))
            .w(theme.sizes.dialog_width.min(room.width))
            .mt(theme.sizes.overlay_margin * 4.)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(theme.spacing.md)
                    .p(theme.spacing.xl)
                    .child(
                        div()
                            .text_spec(&theme.text.sheet_title)
                            .child(format!("Remove {}?", end_cut(&location.name, 40))),
                    )
                    .child(div().text_color(c.fg2).text_spec(&theme.text.body).child(
                        if sessions == 0 {
                            "Syncing stops. Nothing was archived from it. Nothing at the \
                                 location is touched."
                                .to_string()
                        } else {
                            format!(
                                "Syncing stops. Its {} archived {} stay in the archive and \
                                     keep their folders. Nothing at the location is touched.",
                                format::count(sessions as u64),
                                if sessions == 1 { "session" } else { "sessions" }
                            )
                        },
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.md)
                    .py(theme.spacing.lg)
                    .px(theme.spacing.xl)
                    .border_t_1()
                    .border_color(c.line_subtle)
                    .bg(c.s1)
                    .child(div().flex_1())
                    .child(
                        button("remove-cancel", ButtonKind::Plain, false, true, theme)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_dialog(window, cx);
                            }))
                            .child("Cancel")
                            .automation_target("remove/cancel"),
                    )
                    .child(
                        button("remove-confirm", ButtonKind::Danger, false, true, theme)
                            .on_click(cx.listener(|this, _, window, cx| this.remove(window, cx)))
                            .child("Remove")
                            .automation_target("remove/confirm"),
                    ),
            )
            .focus_trap("remove-location-trap", &self.remove_focus)
            .automation_target("remove")
    }
}

/// A WSL location's kind for the distro `name`.
fn wsl(name: &str) -> LocationKind {
    LocationKind::Wsl {
        distro: name.to_string(),
    }
}

/// A state's line after its dot.
fn with_dot(dot: gpui::Div, line: gpui::Div, theme: &SbTheme) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(theme.spacing.sm)
        .child(dot)
        .child(line)
        .into_any_element()
}

/// What a syncing location's row says under its bar (DESIGN §4.11): "Syncing · 38 of 212
/// files · 41.2 of 96.0 MB"; then each step of the index (PLAN §7.5).
fn syncing_line(progress: &PassProgress) -> String {
    let sessions = |done: usize, of: usize| {
        format!(
            "{} of {} sessions",
            format::count(done as u64),
            format::count(of as u64)
        )
    };
    match progress.indexing() {
        Some(IndexProgress {
            step: IndexStep::Rows,
            done,
            of,
        }) => format!("Indexing · {}", sessions(done, of)),
        Some(IndexProgress {
            step: IndexStep::Texts,
            done,
            of,
        }) => format!("Search index · {}", sessions(done, of)),
        None if !progress.planned => "Syncing · looking for changes".to_string(),
        None if progress.items == 0 => "Syncing · nothing new".to_string(),
        None => format!(
            "Syncing · {} of {} files · {}",
            format::count(progress.items_done as u64),
            format::count(progress.items as u64),
            format::bytes_of(progress.bytes_done, progress.bytes, " of ")
        ),
    }
}

/// What a location whose last pass the owner stopped says: what it left to copy.
fn stopped_line(items_left: usize, bytes_left: u64) -> String {
    format!(
        "Stopped · {} and {} left to copy",
        format::plural(items_left, "file"),
        format::size(bytes_left)
    )
}

/// What a location that never synced says: a stopped distro says its first sync starts it
/// (DESIGN §4.11).
fn never_line(location: &Location, sync: &SyncService) -> String {
    let stopped = match &location.kind {
        LocationKind::Wsl { distro } => sync
            .distros()
            .and_then(|distros| distros.offered().find(|it| it.name == *distro))
            .filter(|it| !it.running)
            .map(|it| it.name.clone()),
        _ => None,
    };
    match stopped {
        Some(distro) => format!("Not synced yet · starts {distro} on its first sync"),
        None => "Not synced yet".to_string(),
    }
}

/// What Add location offers, beside its button: "This PC · WSL distro · SSH host".
fn add_kinds(sync: &SyncService) -> String {
    let local = refusal(
        &LocationKind::Local,
        sync.locations().unwrap_or_default().iter(),
    )
    .is_none()
    .then(local_name);
    let wsl = sync
        .distros()
        .is_some_and(|distros| matches!(distros.listed, Ok(Some(_))))
        .then_some("WSL distro");
    local
        .into_iter()
        .chain(wsl)
        .chain(["SSH host"])
        .collect::<Vec<_>>()
        .join(" · ")
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use sb_core::sync::Progress;

    use super::*;

    #[test]
    fn a_syncing_row_counts_files_and_bytes() {
        let mut progress = PassProgress::default();
        assert_eq!(syncing_line(&progress), "Syncing · looking for changes");
        progress.apply(&Progress::Planned {
            items: 212,
            bytes: 96 << 20,
        });
        progress.apply(&Progress::Fetched { bytes: 41 << 20 });
        progress.apply(&Progress::Stored { items: 38 });
        // DESIGN §4.11: "Syncing · 38 of 212 files · 41.2 of 96.0 MB".
        assert_eq!(
            syncing_line(&progress),
            "Syncing · 38 of 212 files · 41 of 96 MB"
        );
        progress.apply(&Progress::Indexing {
            sessions: 12,
            bytes: 0,
            outdated: 0,
        });
        assert_eq!(syncing_line(&progress), "Indexing · 0 of 12 sessions");
        progress.apply(&Progress::Indexed {
            sessions: 12,
            bytes: 0,
            searchable: 0,
        });
        // The second step counts again, of what it makes searchable.
        progress.apply(&Progress::Searching { sessions: 12 });
        progress.apply(&Progress::Searched {
            sessions: 5,
            searchable: 0,
        });
        assert_eq!(syncing_line(&progress), "Search index · 5 of 12 sessions");
    }
}
