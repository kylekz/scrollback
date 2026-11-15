//! Add a location (DESIGN §4.12, `E-Add-Location`): a dialog over the locations sheet or the
//! first run, its header a segmented switch of the kinds: this machine (while it is not a
//! location), a WSL distro (where there is WSL) and an SSH host.
//!
//! - **WSL distro:** the distros WSL lists, never starting one (PLAN §6.3); those that are
//!   locations already can't be chosen. A running distro is tested as it is chosen; a
//!   stopped one is added without a test, as it is, and its first sync starts it.
//! - **SSH host:** the host exactly as the owner gives it to `ssh` (PLAN §6.4), checked only
//!   for what `ssh` can't take. It is tested on Test, on `Enter`, and 600 ms after typing
//!   stops; a new value cancels the test under way. A host that fails can still be added
//!   when nothing needs the owner's hand first ([`TestFailure::may_add_anyway`]); one whose
//!   key isn't accepted can't, and nothing here offers to accept it.
//! - **This machine:** what its providers find, and add.
//!
//! The name is the place's until the owner types one. A place that is a location already is
//! refused. **Add and sync now** adds and starts the first sync; **Add** only adds. In the
//! first run the dialog adds nothing itself: what was chosen joins the first run's list.
//!
//! Keyboard: `Esc` closes it, cancelling its test; `Enter` tests the host while the host field
//! has the keyboard, and otherwise adds and syncs once the test has passed. `Tab` stays inside.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyElement, App, AppContext as _, Context, Div, Entity,
    EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, Role, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task,
    Window, div, prelude::FluentBuilder as _, px, relative,
};
use gpui_component::FocusTrapElement as _;
use gpui_component::input::{Input, InputEvent, InputState};
use sb_core::location::LocationKind;

use crate::actions::locations::{Close, Confirm};
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{
    ButtonKind, DISABLED_OPACITY, bad_banner, button, card, icon, icon_button, info_banner, label,
    radio, said, tab_stop,
};
use crate::format;
use crate::icons::SbIcon;
use crate::keymap;
use crate::overlay;
use crate::services::sync::{
    ConnectionTest, Survey, TestFailure, TestState, host_problem, local_name, refusal,
};
use crate::services::{ActiveServices as _, SyncService};
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _};

/// How long typing must stop before the host typed is tested.
const TEST_AFTER: Duration = Duration::from_millis(600);

/// The width of a test result's labels (`E-Add-Location`).
const RESULT_LABEL: gpui::Pixels = px(110.);

/// What the dialog tells whoever opened it.
pub enum AddLocationEvent {
    Close,
    /// The place was given to the sync service to add.
    Added(LocationKind),
    /// The first run's choice: a place to add with the others, and how many sessions its
    /// test found there.
    Chosen {
        kind: LocationKind,
        name: String,
        sessions: Option<usize>,
    },
}

/// What adding does: add now, or choose for the first run's list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Add,
    Choose,
}

/// The kinds the switch offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    ThisMachine,
    Wsl,
    Ssh,
}

/// Whether the place shown can be added.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ready {
    No,
    /// Its test failed in a way that lets it be added all the same.
    Anyway,
    Yes,
}

pub struct AddLocation {
    sync: Entity<SyncService>,
    purpose: Purpose,
    kind: Kind,
    /// The distro chosen, by its name as WSL lists it.
    distro: Option<String>,
    host: Entity<InputState>,
    name: Entity<InputState>,
    /// The owner typed a name: the place's own is no longer put in.
    named: bool,
    focus_handle: FocusHandle,
    _typing: Option<Task<()>>,
    _subscriptions: [Subscription; 3],
}

impl EventEmitter<AddLocationEvent> for AddLocation {}

impl Focusable for AddLocation {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl AddLocation {
    /// The dialog on `kind`, on the distro `distro` for a distro, with the keyboard in it.
    pub fn new(
        purpose: Purpose,
        kind: Kind,
        distro: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let sync = cx.services().sync.clone();
        let host = cx.new(|cx| InputState::new(window, cx).placeholder("user@host"));
        let name = cx.new(|cx| InputState::new(window, cx));
        let mut this = Self {
            _subscriptions: [
                cx.observe(&sync, |_, _, cx| cx.notify()),
                cx.subscribe_in(&host, window, |this, _, event, window, cx| {
                    if let InputEvent::Change = event {
                        this.host_changed(window, cx);
                    }
                }),
                cx.subscribe(&name, |this, _, event, _| {
                    if let InputEvent::Change = event {
                        this.named = true;
                    }
                }),
            ],
            sync,
            purpose,
            kind,
            distro: None,
            host,
            name,
            named: false,
            focus_handle: cx.focus_handle(),
            _typing: None,
        };
        this.sync.update(cx, |sync, cx| sync.list_distros(cx));
        this.choose(kind, window, cx);
        if let Some(distro) = distro {
            this.choose_distro(distro, window, cx);
        }
        this
    }

    /// Shows `kind`, and tests what it has to test.
    fn choose(&mut self, kind: Kind, window: &mut Window, cx: &mut Context<Self>) {
        self.kind = kind;
        self._typing = None;
        match kind {
            Kind::ThisMachine => {
                self.prefill(local_name(), window, cx);
                self.sync
                    .update(cx, |sync, cx| sync.test_place(LocationKind::Local, cx));
                self.focus_handle.focus(window, cx);
            }
            Kind::Wsl => {
                self.sync.update(cx, |sync, _| sync.cancel_test());
                let first = self.distro.clone().or_else(|| {
                    let sync = self.sync.read(cx);
                    let locations = sync.locations().unwrap_or_default();
                    sync.distros()?
                        .offered()
                        .find(|distro| refusal(&wsl(&distro.name), locations.iter()).is_none())
                        .map(|distro| distro.name.clone())
                });
                if let Some(first) = first {
                    self.choose_distro(first, window, cx);
                }
                self.focus_handle.focus(window, cx);
            }
            Kind::Ssh => {
                self.host.update(cx, |host, cx| host.focus(window, cx));
                self.host_changed(window, cx);
            }
        }
        cx.notify();
    }

    /// Chooses the distro `name`: a running one is tested, a stopped one is not (PLAN §6.3).
    fn choose_distro(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.kind = Kind::Wsl;
        self.prefill(&name, window, cx);
        let running = self
            .sync
            .read(cx)
            .distros()
            .and_then(|distros| distros.offered().find(|it| it.name == name))
            .is_some_and(|distro| distro.running);
        let place = wsl(&name);
        self.distro = Some(name);
        self.sync.update(cx, |sync, cx| {
            if running {
                sync.test_place(place, cx);
            } else {
                sync.cancel_test();
            }
        });
        cx.notify();
    }

    /// The host typed changed: the test under way is for another host, and a new one runs
    /// once typing stops.
    fn host_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let host = self.host.read(cx).value().to_string();
        self.prefill(&host, window, cx);
        self.sync.update(cx, |sync, _| sync.cancel_test());
        self._typing = (self.kind == Kind::Ssh && self.testable(cx).is_some()).then(|| {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(TEST_AFTER).await;
                this.update(cx, |this, cx| this.test_host(cx)).ok();
            })
        });
        cx.notify();
    }

    /// The host typed, when it is one to test: well formed, and not a location already.
    fn testable(&self, cx: &App) -> Option<LocationKind> {
        let host = self.host.read(cx).value().to_string();
        if host_problem(&host).is_some() {
            return None;
        }
        let place = LocationKind::Ssh { host };
        let locations = self.sync.read(cx).locations().unwrap_or_default();
        refusal(&place, locations.iter()).is_none().then_some(place)
    }

    fn test_host(&mut self, cx: &mut Context<Self>) {
        self._typing = None;
        if let Some(place) = self.testable(cx) {
            self.sync.update(cx, |sync, cx| sync.test_place(place, cx));
        }
    }

    /// Puts the place's own name in the name field, unless the owner typed one.
    fn prefill(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.named {
            let name = name.to_string();
            self.name
                .update(cx, |input, cx| input.set_value(name, window, cx));
        }
    }

    /// The place shown, when there is one.
    fn place(&self, cx: &App) -> Option<LocationKind> {
        match self.kind {
            Kind::ThisMachine => Some(LocationKind::Local),
            Kind::Wsl => self.distro.as_deref().map(wsl),
            Kind::Ssh => {
                let host = self.host.read(cx).value().to_string();
                host_problem(&host)
                    .is_none()
                    .then_some(LocationKind::Ssh { host })
            }
        }
    }

    /// The connection test of the place shown, if it is the one the service has.
    fn test<'a>(&self, cx: &'a App) -> Option<&'a ConnectionTest> {
        let place = self.place(cx)?;
        self.sync
            .read(cx)
            .test()
            .filter(|test| test.kind.same_place(&place))
    }

    /// Whether a distro, by its name, is stopped as WSL listed it.
    fn stopped(&self, name: &str, cx: &App) -> bool {
        self.sync
            .read(cx)
            .distros()
            .and_then(|distros| distros.offered().find(|it| it.name == name))
            .is_some_and(|distro| !distro.running)
    }

    fn ready(&self, cx: &App) -> Ready {
        let Some(place) = self.place(cx) else {
            return Ready::No;
        };
        let sync = self.sync.read(cx);
        if sync.is_adding(&place)
            || refusal(&place, sync.locations().unwrap_or_default().iter()).is_some()
        {
            return Ready::No;
        }
        if let LocationKind::Wsl { distro } = &place
            && self.stopped(distro, cx)
        {
            return Ready::Yes;
        }
        match self.test(cx).map(|test| &test.state) {
            Some(TestState::Passed { .. }) => Ready::Yes,
            Some(TestState::Failed(failure)) if failure.may_add_anyway() => Ready::Anyway,
            _ => Ready::No,
        }
    }

    /// Adds the place shown, or chooses it for the first run, and with `sync_now` starts its
    /// first sync.
    fn add(&mut self, sync_now: bool, cx: &mut Context<Self>) {
        let Some(place) = self.place(cx) else {
            return;
        };
        let typed = self.name.read(cx).value().trim().to_string();
        let name = if typed.is_empty() {
            place_name(&place)
        } else {
            typed
        };
        match self.purpose {
            Purpose::Add => {
                self.sync.update(cx, |sync, cx| {
                    sync.cancel_test();
                    sync.add(place.clone(), name, sync_now, cx);
                });
                cx.emit(AddLocationEvent::Added(place));
            }
            Purpose::Choose => {
                let sessions = match self.test(cx).map(|test| &test.state) {
                    Some(TestState::Passed { found, .. }) => Some(sessions(found)),
                    _ => None,
                };
                self.sync.update(cx, |sync, _| sync.cancel_test());
                cx.emit(AddLocationEvent::Chosen {
                    kind: place,
                    name,
                    sessions,
                });
            }
        }
    }

    fn close(&mut self, _: &Close, _: &mut Window, cx: &mut Context<Self>) {
        self.sync.update(cx, |sync, _| sync.cancel_test());
        cx.emit(AddLocationEvent::Close);
    }

    /// `Enter` (module docs). On a button `Tab` put the keyboard on, it presses the button.
    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.host.focus_handle(cx).is_focused(window) {
            self.test_host(cx);
        } else if self.name.focus_handle(cx).is_focused(window)
            || self.focus_handle.is_focused(window)
        {
            if self.ready(cx) == Ready::Yes {
                self.add(true, cx);
            }
        } else {
            cx.propagate();
        }
    }

    /// The segmented switch of the kinds (`.seg`).
    fn kinds(&self, theme: &SbTheme, cx: &mut Context<Self>) -> AnyElement {
        let c = &theme.colors;
        let sync = self.sync.read(cx);
        let local_added = refusal(
            &LocationKind::Local,
            sync.locations().unwrap_or_default().iter(),
        )
        .is_some();
        let has_wsl = sync
            .distros()
            .is_some_and(|distros| !matches!(distros.listed, Ok(None)));
        let segment = |kind: Kind, text: &'static str, target: &'static str, enabled: bool| {
            let on = self.kind == kind;
            tab_stop(
                div()
                    .id(target)
                    .role(Role::Button)
                    .flex()
                    .items_center()
                    .h(theme.sizes.segment_height)
                    .px(px(9.))
                    .rounded(px(5.))
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .text_spec(&theme.text.control_small)
                    .text_color(if on { c.fg } else { c.fg2 })
                    .when(on, |this| this.bg(c.active).border_color(c.line_subtle))
                    .child(text),
                theme,
            )
            .map(|this| {
                if enabled {
                    this.cursor_pointer()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.choose(kind, window, cx);
                        }))
                } else {
                    this.opacity(DISABLED_OPACITY)
                }
            })
            .automation_target(target)
        };
        div()
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .child(
                div()
                    .flex()
                    .gap(px(2.))
                    .p(px(2.))
                    .rounded(theme.radii.input)
                    .border_1()
                    .border_color(c.line_subtle)
                    .bg(c.s1)
                    .child(segment(
                        Kind::ThisMachine,
                        local_name(),
                        "add/kind/local",
                        !local_added,
                    ))
                    .when(has_wsl, |this| {
                        this.child(segment(Kind::Wsl, "WSL distro", "add/kind/wsl", true))
                    })
                    .child(segment(Kind::Ssh, "SSH host", "add/kind/ssh", true)),
            )
            .when(local_added, |this| {
                this.child(
                    div()
                        .text_color(c.fg3)
                        .text_spec(&theme.text.small)
                        .child(format!("{} is already added", local_name())),
                )
            })
            .into_any_element()
    }

    /// The WSL step: the distros as a radio list, and what the chosen one needs said.
    fn wsl_step(&self, theme: &SbTheme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let c = &theme.colors;
        let sync = self.sync.read(cx);
        let locations = sync.locations().unwrap_or_default();
        let mut step = vec![
            div()
                .text_color(c.fg2)
                .text_spec(&theme.text.secondary)
                .child(format!(
                    "Distros installed on {}, from wsl.exe --list. Listing them never starts one.",
                    local_name()
                ))
                .into_any_element(),
        ];
        let Some(distros) = sync.distros() else {
            step.push(note("Listing the distros…", theme));
            return step;
        };
        if let Err(said_so) = &distros.listed {
            step.push(
                bad_banner(theme)
                    .child(
                        div()
                            .flex_1()
                            .text_spec(&theme.text.secondary)
                            .child(format!("wsl.exe couldn't list the distros: {said_so}")),
                    )
                    .into_any_element(),
            );
            return step;
        }
        let rows: Vec<AnyElement> = distros
            .offered()
            .enumerate()
            .map(|(at, distro)| {
                let added = refusal(&wsl(&distro.name), locations.iter()).is_some();
                let on = self.distro.as_deref() == Some(distro.name.as_str()) && !added;
                let state = match (distro.running, distro.default) {
                    (true, true) => "running · default",
                    (true, false) => "running",
                    (false, true) => "stopped · default",
                    (false, false) => "stopped",
                };
                let below = if added {
                    "Already a location".to_string()
                } else if !distro.running {
                    "Sessions: counted on its first sync".to_string()
                } else {
                    match self.test(cx).filter(|_| on).map(|test| &test.state) {
                        Some(TestState::Passed { found, .. }) => {
                            format::plural(sessions(found), "session")
                        }
                        Some(TestState::Failed(_)) => "Its test failed".to_string(),
                        Some(TestState::Running(_)) => "Testing…".to_string(),
                        None => "Tested when chosen".to_string(),
                    }
                };
                let name = distro.name.clone();
                tab_stop(
                    div()
                        .id(SharedString::from(format!("distro-{}", distro.name)))
                        .role(Role::RadioButton)
                        .flex()
                        .items_center()
                        .gap(theme.spacing.lg)
                        .p(theme.spacing.lg)
                        .border_1()
                        .border_color(gpui::transparent_black())
                        .when(at > 0, |this| this.border_t_1().border_color(c.line_subtle))
                        .when(on, |this| this.bg(c.s3))
                        .child(radio(on, theme))
                        .child(
                            div()
                                .flex_1()
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
                                                .text_spec(&theme.text.control)
                                                .child(distro.name.clone()),
                                        )
                                        .child(
                                            div()
                                                .text_color(if on { c.fg2 } else { c.fg3 })
                                                .text_spec(&theme.text.small)
                                                .child(state),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_color(if on { c.fg2 } else { c.fg3 })
                                        .text_spec(&theme.text.secondary)
                                        .child(below),
                                ),
                        ),
                    theme,
                )
                .map(|this| {
                    if added {
                        this.opacity(0.55)
                    } else {
                        this.cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.choose_distro(name.clone(), window, cx);
                            }))
                    }
                })
                .automation_target_with({
                    let name = distro.name.clone();
                    move || format!("add/distro/{name}").into()
                })
            })
            .collect();
        if rows.is_empty() {
            step.push(note("WSL has no distro installed.", theme));
            return step;
        }
        step.push(
            div()
                .flex()
                .flex_col()
                .rounded(theme.radii.card)
                .border_1()
                .border_color(c.line)
                .overflow_hidden()
                .children(rows)
                .into_any_element(),
        );
        match &self.distro {
            Some(distro) if self.stopped(distro, cx) => step.push(
                info_banner(theme)
                    .child(div().flex_1().child(format!(
                        "{distro} is stopped. Adding it doesn't start it. Its first sync will, \
                         and so will every sync after that, scheduled ones included."
                    )))
                    .automation_target("add/stopped"),
            ),
            Some(distro) => {
                if let Some(test) = self.test(cx) {
                    step.push(test_card(test, distro, theme, cx));
                }
            }
            None => {}
        }
        step
    }

    /// The SSH step: the host field and its test.
    fn ssh_step(
        &self,
        window: &Window,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let c = &theme.colors;
        let host = self.host.read(cx).value().to_string();
        let sync = self.sync.read(cx);
        let already = refusal(
            &LocationKind::Ssh { host: host.clone() },
            sync.locations().unwrap_or_default().iter(),
        )
        .map(|location| location.name.clone());
        let test = self.test(cx);
        let failed = matches!(test.map(|it| &it.state), Some(TestState::Failed(_)));
        let problem = (!host.is_empty()).then(|| host_problem(&host)).flatten();
        let mut step = vec![
            div()
                .flex()
                .flex_col()
                .gap(theme.spacing.xs)
                .child(label("Host", theme))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(theme.spacing.md)
                        .child(
                            field(
                                &self.host,
                                true,
                                failed || problem.is_some(),
                                window,
                                theme,
                                cx,
                            )
                            .flex_1()
                            .automation_target("add/host"),
                        )
                        .child(
                            button(
                                "add-test",
                                ButtonKind::Plain,
                                false,
                                self.testable(cx).is_some(),
                                theme,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.test_host(cx)))
                            .child(if test.is_some() { "Test again" } else { "Test" })
                            .automation_target("add/test"),
                        ),
                )
                .into_any_element(),
            help_line(theme),
        ];
        if let Some(problem) = problem {
            step.push(
                div()
                    .text_color(c.red)
                    .text_spec(&theme.text.secondary)
                    .child(problem)
                    .automation_target("add/host-problem"),
            );
        } else if let Some(name) = already {
            step.push(
                div()
                    .text_color(c.red)
                    .text_spec(&theme.text.secondary)
                    .child(if name == host {
                        format!("{host} is a location already.")
                    } else {
                        format!("{host} is a location already: {name}.")
                    })
                    .automation_target("add/already"),
            );
        }
        if let Some(test) = test {
            step.push(test_card(test, &host, theme, cx));
        }
        step
    }

    /// This machine's step: what its providers found.
    fn local_step(&self, theme: &SbTheme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.test(cx)
            .map(|test| test_card(test, local_name(), theme, cx))
            .into_iter()
            .collect()
    }

    /// The name field, and what the first sync will copy.
    fn name_step(
        &self,
        window: &Window,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let c = &theme.colors;
        let mut step = vec![
            div()
                .flex()
                .flex_col()
                .gap(theme.spacing.xs)
                .child(label("Name", theme))
                .child(
                    field(&self.name, false, false, window, theme, cx)
                        .automation_target("add/name"),
                )
                .into_any_element(),
        ];
        if let Some(TestState::Passed { found, .. }) = self.test(cx).map(|test| &test.state) {
            let bytes: u64 = found.iter().flatten().map(|found| found.bytes).sum();
            let asleep = if self.kind == Kind::Ssh {
                " A machine that sleeps is fine: it shows as not reachable and syncs when it wakes."
            } else {
                ""
            };
            step.push(
                div()
                    .text_color(c.fg3)
                    .text_spec(&theme.text.secondary)
                    .child(format!(
                        "The first sync copies {}.{asleep}",
                        format::size(bytes)
                    ))
                    .into_any_element(),
            );
        }
        step
    }

    fn footer(&self, theme: &SbTheme, cx: &mut Context<Self>) -> AnyElement {
        let ready = self.ready(cx);
        let choosing = self.purpose == Purpose::Choose;
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .py(theme.spacing.lg)
            .px(theme.spacing.xl)
            .border_t_1()
            .border_color(theme.colors.line_subtle)
            .bg(theme.colors.s1)
            .child(div().flex_1())
            .child(
                button("add-cancel", ButtonKind::Plain, false, true, theme)
                    .on_click(cx.listener(|this, _, window, cx| this.close(&Close, window, cx)))
                    .child("Cancel")
                    .automation_target("add/cancel"),
            )
            .when(ready == Ready::Anyway, |this| {
                this.child(
                    button("add-anyway", ButtonKind::Plain, false, true, theme)
                        // Its first sync is tried at once: it shows as not reachable until it is.
                        .on_click(cx.listener(|this, _, _, cx| this.add(true, cx)))
                        .child("Add anyway")
                        .automation_target("add/anyway"),
                )
            })
            .when(
                self.kind == Kind::Wsl && !choosing && ready == Ready::Yes,
                |this| {
                    this.child(
                        button("add-only", ButtonKind::Plain, false, true, theme)
                            .on_click(cx.listener(|this, _, _, cx| this.add(false, cx)))
                            .child("Add")
                            .automation_target("add/add"),
                    )
                },
            )
            .child(
                button(
                    "add-sync",
                    ButtonKind::Primary,
                    false,
                    ready == Ready::Yes,
                    theme,
                )
                .when(ready == Ready::Yes, |this| {
                    this.on_click(cx.listener(|this, _, _, cx| this.add(true, cx)))
                })
                .child(if choosing { "Add" } else { "Add and sync now" })
                .automation_target("add/add-sync"),
            )
            .into_any_element()
    }
}

impl Render for AddLocation {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.sb_theme().clone();
        let c = &theme.colors;
        let room = overlay::room(window.viewport_size(), &theme);
        let no_ssh = matches!(
            self.test(cx).map(|test| &test.state),
            Some(TestState::Failed(TestFailure::NotInstalled(_)))
        ) && self.kind == Kind::Ssh;
        let mut body = match self.kind {
            Kind::ThisMachine => self.local_step(&theme, cx),
            Kind::Wsl => self.wsl_step(&theme, cx),
            Kind::Ssh => self.ssh_step(window, &theme, cx),
        };
        let named = match self.kind {
            Kind::ThisMachine => true,
            Kind::Wsl => self.distro.is_some(),
            Kind::Ssh => self.ready(cx) != Ready::No,
        };
        if named && !no_ssh {
            body.extend(self.name_step(window, &theme, cx));
        }
        overlay::surface(&theme)
            .id("add-location")
            .key_context(keymap::LOCATION_DIALOG)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::close))
            .on_action(cx.listener(Self::confirm))
            .w(theme.sizes.dialog_width.min(room.width))
            .max_h(theme.sizes.dialog_max_height.min(room.height))
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(theme.spacing.sm)
                    .pt(px(14.))
                    .pb(theme.spacing.lg)
                    .px(theme.spacing.xl)
                    .border_b_1()
                    .border_color(c.line_subtle)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .text_spec(&theme.text.sheet_title)
                                    .child("Add a location"),
                            )
                            .child(
                                icon_button("add-close", theme.sizes.control_small_height, &theme)
                                    .aria_label("Close")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close(&Close, window, cx);
                                    }))
                                    .child(icon(SbIcon::X, theme.sizes.icon_sm, c.fg2))
                                    .automation_target("add/close"),
                            ),
                    )
                    .child(self.kinds(&theme, cx)),
            )
            .child(
                div()
                    .id("add-location-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(theme.spacing.lg)
                    .py(px(14.))
                    .px(theme.spacing.xl)
                    .children(body),
            )
            .child(self.footer(&theme, cx))
            .focus_trap("add-location-trap", &self.focus_handle)
            .automation_target("add")
    }
}

/// A WSL location's kind for the distro `name`.
fn wsl(name: &str) -> LocationKind {
    LocationKind::Wsl {
        distro: name.to_string(),
    }
}

/// What a place is called until the owner names it.
fn place_name(place: &LocationKind) -> String {
    match place {
        LocationKind::Local => local_name().to_string(),
        LocationKind::Wsl { distro } => distro.clone(),
        LocationKind::Ssh { host } => host.clone(),
    }
}

/// How many sessions every agent found has.
fn sessions(found: &Survey) -> usize {
    found.iter().flatten().map(|found| found.sessions).sum()
}

/// A line of `fg3` text in place of what isn't there yet.
fn note(text: &'static str, theme: &SbTheme) -> AnyElement {
    div()
        .text_color(theme.colors.fg3)
        .text_spec(&theme.text.secondary)
        .child(text)
        .into_any_element()
}

/// What the host field is for, and what scrollback does with it (DESIGN §4.11).
pub(crate) fn help_line(theme: &SbTheme) -> AnyElement {
    div()
        .text_color(theme.colors.fg3)
        .text_spec(&theme.text.secondary)
        .child(
            "As you'd type it after ssh: a ~/.ssh/config alias or user@host. scrollback runs your \
             ssh with your config, keys and agent, never prompts, and stores no credentials.",
        )
        .into_any_element()
}

/// A text field (`.inp`: 30 high, radius 7, `s1` with a `line` border; `accent` with the
/// keyboard, `red` when what it holds is wrong), in the code font with `mono`.
pub(crate) fn field(
    input: &Entity<InputState>,
    mono: bool,
    bad: bool,
    window: &Window,
    theme: &SbTheme,
    cx: &App,
) -> Div {
    let c = &theme.colors;
    let focused = input.focus_handle(cx).is_focused(window);
    div()
        .flex()
        .items_center()
        .h(theme.sizes.input_height)
        .px(px(10.))
        .rounded(theme.radii.input)
        .border_1()
        .border_color(if bad {
            c.red
        } else if focused {
            c.accent
        } else {
            c.line
        })
        .bg(c.s1)
        .text_color(c.fg)
        .map(|this| {
            if mono {
                this.text_spec(&theme.text.path)
            } else {
                this.text_spec(&theme.text.body)
            }
        })
        .child(Input::new(input).appearance(false).px_0())
}

/// One line of a passed test (`E-Add-Location`): what was checked, and what was found.
fn found_line(what: impl Into<SharedString>, value: impl IntoElement, theme: &SbTheme) -> Div {
    div()
        .flex()
        .items_start()
        .gap(theme.spacing.md)
        .text_spec(&theme.text.secondary)
        .child(
            div()
                .flex_none()
                .w(RESULT_LABEL)
                .text_color(theme.colors.fg3)
                .child(what.into()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_color(theme.colors.fg2)
                .child(value),
        )
}

/// A bar that moves while the length of the work is unknown (DESIGN §4.12).
fn busy_bar(theme: &SbTheme) -> impl IntoElement {
    let radius = theme.sizes.bar_height / 2.;
    div()
        .relative()
        .w_full()
        .h(theme.sizes.bar_height)
        .rounded(radius)
        .bg(theme.colors.s3)
        .overflow_hidden()
        .child(
            div()
                .absolute()
                .top_0()
                .h_full()
                .w(relative(0.3))
                .rounded(radius)
                .bg(theme.colors.accent)
                .with_animation(
                    "connecting",
                    Animation::new(Duration::from_millis(1200)).repeat(),
                    |bar, delta| bar.left(relative(delta * 0.7)),
                ),
        )
}

/**
 * What a connection test of the place called `name` says (DESIGN §4.12): connecting, what was
 * found when it passed, or why it failed in the launcher's own words, with the command that
 * was run and what to check.
 */
pub(crate) fn test_card(
    test: &ConnectionTest,
    name: &str,
    theme: &SbTheme,
    cx: &App,
) -> AnyElement {
    let c = &theme.colors;
    let card = card(theme).gap(theme.spacing.md).p(theme.spacing.lg);
    let tool = match &test.kind {
        LocationKind::Wsl { .. } => "wsl.exe",
        _ => "ssh",
    };
    match &test.state {
        TestState::Running(_) => card
            .child(
                div()
                    .text_color(c.fg2)
                    .text_spec(&theme.text.secondary)
                    .child(format!("Connecting to {name}…")),
            )
            .child(busy_bar(theme))
            .automation_target("add/result/running"),
        TestState::Passed { probe, found } => {
            let headline = match probe {
                Some(probe) => format!("Connected in {:.1} s", probe.took.as_secs_f64()),
                None => format!("Looked on {name}"),
            };
            let total: u64 = found.iter().flatten().map(|found| found.bytes).sum();
            card.child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.sm)
                    .text_color(c.green)
                    .text_spec(&theme.text.control)
                    .child(icon(SbIcon::CircleCheck, theme.sizes.icon_sm, c.green))
                    .child(headline),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(theme.spacing.sm)
                    .children(probe.as_ref().map(|probe| {
                        found_line(
                            "Shell",
                            match &probe.login_shell {
                                Some(login) => format!("{login}, as a login shell"),
                                None => format!(
                                    "{}; the login shell didn't answer, so only the bare \
                                     environment is seen",
                                    probe.shell
                                ),
                            },
                            theme,
                        )
                    }))
                    .children(
                        probe
                            .as_ref()
                            .map(|_| found_line("Tools", "All found", theme)),
                    )
                    .children(found.iter().map(|found| {
                        match found {
                            Ok(found) => found_line(
                                agents::name(found.provider.as_str(), cx),
                                div()
                                    .text_spec(&theme.text.path_small)
                                    .child(format::path(&found.root)),
                                theme,
                            ),
                            Err(why) => found_line(
                                "Couldn't look",
                                div().text_color(c.red).child(why.clone()),
                                theme,
                            ),
                        }
                    }))
                    .child(found_line(
                        "Found",
                        if found.iter().flatten().next().is_some() {
                            format!(
                                "{} · {}",
                                format::plural(sessions(found), "session"),
                                format::size(total)
                            )
                        } else {
                            "No transcripts found".to_string()
                        },
                        theme,
                    ))
                    .children(found.iter().flatten().filter_map(|found| {
                        let days = found.retention_days?;
                        Some(found_line(
                            "Cleanup",
                            format!(
                                "{} keeps {} here",
                                agents::name(found.provider.as_str(), cx),
                                format::plural(days as usize, "day")
                            ),
                            theme,
                        ))
                    })),
            )
            .automation_target("add/result/passed")
        }
        TestState::Failed(failure) => {
            let (headline, words, checks): (String, String, Vec<String>) = match failure {
                TestFailure::Unreachable(words) | TestFailure::Other(words) => (
                    "Couldn't connect".to_string(),
                    words.clone(),
                    vec![
                        format!("Is {name} spelled as in ~/.ssh/config?"),
                        format!(
                            "Does `{tool} {name}` work in a terminal without asking for a \
                             password?"
                        ),
                        "If the machine is only asleep, you can add it now; it syncs when \
                         it's back."
                            .to_string(),
                    ],
                ),
                TestFailure::Refused(words) => (
                    "The sign-in was refused".to_string(),
                    words.clone(),
                    vec![
                        format!(
                            "ssh may not ask for a password here: does `ssh {name}` sign in \
                             with a key from your agent or config?"
                        ),
                        "You can add it now and sync it once that works.".to_string(),
                    ],
                ),
                TestFailure::HostKey(words) => (
                    "The host's key isn't accepted".to_string(),
                    words.clone(),
                    vec![
                        "scrollback never accepts a host key for you: check why the key \
                         changed, update known_hosts in a terminal, then test again."
                            .to_string(),
                    ],
                ),
                TestFailure::Missing(tools) => (
                    "Tools are missing there".to_string(),
                    tools
                        .iter()
                        .map(|tool| format!("{tool} not found"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    vec![format!(
                        "{name} can be added once it has them: install them there, then test \
                         again."
                    )],
                ),
                TestFailure::NotInstalled(program) => (
                    format!("{program} isn't installed"),
                    if cfg!(windows) && program == "ssh" {
                        "ssh isn't installed. Add Windows' OpenSSH Client in Settings › \
                         Optional features, then try again."
                            .to_string()
                    } else {
                        format!(
                            "{program} isn't on this machine's PATH. Install it, then try again."
                        )
                    },
                    Vec::new(),
                ),
            };
            card.border_color(c.red_soft)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(theme.spacing.sm)
                        .text_color(c.red)
                        .text_spec(&theme.text.control)
                        .child(icon(SbIcon::CircleAlert, theme.sizes.icon_sm, c.red))
                        .child(headline),
                )
                .child(said(words, theme))
                .children(test.command.clone().map(|command| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(theme.spacing.xs)
                        .child(
                            div()
                                .text_color(c.fg3)
                                .text_spec(&theme.text.secondary)
                                .child(if matches!(failure, TestFailure::Missing(_)) {
                                    "scrollback ran:".to_string()
                                } else {
                                    format!("{tool}'s own words. scrollback ran:")
                                }),
                        )
                        .child(
                            div()
                                .text_color(c.fg3)
                                .text_spec(&theme.text.said)
                                .child(command),
                        )
                }))
                .when(!checks.is_empty(), |this| {
                    this.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(theme.spacing.xs)
                            .text_spec(&theme.text.secondary)
                            .text_color(c.fg2)
                            .child(
                                div()
                                    .text_color(c.fg)
                                    .text_spec(&theme.text.control_small)
                                    .child("Things to check"),
                            )
                            .children(checks.into_iter().map(|check| format!("· {check}"))),
                    )
                })
                .automation_target("add/result/failed")
        }
    }
}
