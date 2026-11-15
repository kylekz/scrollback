//! The first run (PLAN §11 `first_run`; DESIGN §4.10, `E-First-Run`): "Add your first
//! location", shown while the archive has never had one. It offers what was found here, each
//! place a checkbox row: this machine with what its providers found; each WSL distro where
//! there is WSL, a running one with what its providers found there and a stopped one listed as
//! it is, never started to look inside (PLAN §6.3); and the SSH hosts the owner chose through
//! "Add an SSH host…" (the add dialog's SSH step, `crate::add_location`). Its button adds
//! every checked place and starts each one's first sync. Everything it learns comes from the
//! sync service's background jobs.
//!
//! Checked at first: this machine when an agent was found here, every running distro, and
//! every host chosen. The fixed copy names no agent (PLAN §11); where the screen names one,
//! the name is its provider's.

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _,
    IntoElement, ParentElement as _, Pixels, Render, Role, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledText, Subscription, TextRun, Window, div,
    font, prelude::FluentBuilder as _, px,
};
use sb_core::location::LocationKind;

use crate::add_location::{AddLocation, AddLocationEvent, Kind, Purpose};
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{
    ButtonKind, button, checkbox, icon, kind_icon, kind_name, location_tag, overline, tab_stop,
};
use crate::format;
use crate::icons::SbIcon;
use crate::overlay::{self, Layer};
use crate::services::sync::{Survey, local_name};
use crate::services::{ActiveServices as _, ArchiveService, SyncService};
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _};

/// The gap between the screen's sections, and a found row's padding at its sides
/// (`design/project/E-First-Run`).
const SECTION_GAP: Pixels = px(22.);
const ROW_PADDING: Pixels = px(14.);

/// What the screen says about scrollback, with the agents it archives named by their
/// providers: one by its name, several as coding agents.
fn about(agents: &[&str]) -> String {
    let (whose, who) = match agents {
        [agent] => (
            (*agent).to_string(),
            format!("{agent} deletes old transcripts on its own"),
        ),
        _ => (
            "coding-agent".to_string(),
            "Agents delete old transcripts on their own".to_string(),
        ),
    };
    format!(
        "scrollback copies {whose} transcripts from each location into its own archive and \
         reads only the copies. {who}; what is copied here stays."
    )
}

/// What a place's row says at its right: how many sessions each agent has there.
fn found_line(found: &[(String, usize)]) -> String {
    match found {
        [] => "no transcripts found".to_string(),
        [(_, sessions)] => format::plural(*sessions, "session"),
        several => several
            .iter()
            .map(|(agent, sessions)| format!("{agent} · {}", format::plural(*sessions, "session")))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// A place offered, as its row shows it.
struct Offered {
    kind: LocationKind,
    name: String,
    /// What the row says under its name: its state, then its own lines.
    state: Option<&'static str>,
    lines: Vec<(String, bool)>,
    /// What the row says at its right.
    count: String,
    /// Whether it is checked until the owner says.
    checked: bool,
    /// Shown dimmed: a stopped distro, not looked inside.
    dim: bool,
}

/// A host chosen through the add dialog, and how many sessions its test found.
struct Host {
    kind: LocationKind,
    name: String,
    sessions: Option<usize>,
}

/**
 * `path` with a word joiner (U+2060, which draws nothing) between each two of its characters,
 * so that a line never breaks inside it (`D:` at a line's end, `\Software\…` on the next) and
 * wraps before it instead.
 */
fn unbroken(path: &str) -> String {
    let mut joined = String::with_capacity(path.len() * 4);
    for (at, character) in path.chars().enumerate() {
        if at > 0 {
            joined.push('\u{2060}');
        }
        joined.push(character);
    }
    joined
}

pub struct FirstRun {
    sync: Entity<SyncService>,
    archive: Entity<ArchiveService>,
    /// What the owner checked or unchecked, by place.
    said: Vec<(LocationKind, bool)>,
    hosts: Vec<Host>,
    /// The add dialog's SSH step, while it is open, and where the keyboard was before.
    dialog: Option<(Entity<AddLocation>, Option<FocusHandle>, Subscription)>,
    /// The locations are being added.
    adding: bool,
    _sync: Subscription,
}

impl FirstRun {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let services = cx.services();
        let (sync, archive) = (services.sync.clone(), services.archive.clone());
        Self {
            _sync: cx.observe(&sync, |this, sync, cx| {
                // An add that failed leaves the screen as it was, to be tried again.
                if sync.read(cx).is_settled() {
                    this.adding = false;
                }
                cx.notify();
            }),
            sync,
            archive,
            said: Vec::new(),
            hosts: Vec::new(),
            dialog: None,
            adding: false,
        }
    }

    /// Every place offered, this machine first, then the distros, then the hosts chosen.
    fn offered(&self, cx: &App) -> Vec<Offered> {
        let sync = self.sync.read(cx);
        let surveyed = |found: Option<&Survey>| {
            let agents: Vec<(String, usize)> = found
                .into_iter()
                .flatten()
                .flatten()
                .map(|found| (agents::name(found.provider.as_str(), cx), found.sessions))
                .collect();
            let lines: Vec<(String, bool)> = found
                .into_iter()
                .flatten()
                .map(|found| match found {
                    Ok(found) => (format::path(&found.root), false),
                    Err(why) => (why.clone(), true),
                })
                .collect();
            let count = match found {
                Some(_) => found_line(&agents),
                None => "looking…".to_string(),
            };
            (lines, count, !agents.is_empty())
        };
        let (lines, count, any) = surveyed(sync.found());
        let mut offered = vec![Offered {
            kind: LocationKind::Local,
            name: local_name().to_string(),
            state: None,
            lines,
            count,
            checked: any,
            dim: false,
        }];
        if let Some(distros) = sync.distros() {
            offered.extend(distros.offered().map(|distro| {
                let kind = LocationKind::Wsl {
                    distro: distro.name.clone(),
                };
                if distro.running {
                    let (lines, count, _) = surveyed(distros.surveys.get(&distro.name));
                    Offered {
                        kind,
                        name: distro.name.clone(),
                        state: Some(if distro.default {
                            "running · default"
                        } else {
                            "running"
                        }),
                        lines,
                        count,
                        checked: true,
                        dim: false,
                    }
                } else {
                    Offered {
                        kind,
                        name: distro.name.clone(),
                        state: Some("stopped"),
                        lines: vec![(
                            "Not started to look inside. If you add it, its first sync starts it."
                                .to_string(),
                            false,
                        )],
                        count: "not counted".to_string(),
                        checked: false,
                        dim: true,
                    }
                }
            }));
        }
        offered.extend(self.hosts.iter().map(|host| Offered {
            kind: host.kind.clone(),
            name: host.name.clone(),
            state: None,
            lines: Vec::new(),
            count: host.sessions.map_or_else(
                || "added unreachable".to_string(),
                |sessions| format::plural(sessions, "session"),
            ),
            checked: true,
            dim: false,
        }));
        offered
    }

    fn is_checked(&self, offered: &Offered) -> bool {
        self.said
            .iter()
            .find(|(kind, _)| *kind == offered.kind)
            .map_or(offered.checked, |(_, checked)| *checked)
    }

    fn toggle(&mut self, kind: LocationKind, checked: bool, cx: &mut Context<Self>) {
        self.said.retain(|(it, _)| *it != kind);
        self.said.push((kind, !checked));
        cx.notify();
    }

    /// The places checked, and what each is called.
    fn checked(&self, cx: &App) -> Vec<(LocationKind, String)> {
        let looked = self.sync.read(cx).found().is_some();
        self.offered(cx)
            .into_iter()
            .filter(|offered| self.is_checked(offered))
            // This machine is added once its providers have looked.
            .filter(|offered| looked || offered.kind != LocationKind::Local)
            .map(|offered| (offered.kind, offered.name))
            .collect()
    }

    /// Adds every checked place and starts each one's first sync.
    pub fn add(&mut self, cx: &mut Context<Self>) {
        let checked = self.checked(cx);
        if self.adding || checked.is_empty() {
            return;
        }
        self.adding = true;
        self.sync.update(cx, |sync, cx| {
            for (kind, name) in checked {
                sync.add(kind, name, true, cx);
            }
        });
        cx.notify();
    }

    /// Opens the add dialog's SSH step: a host chosen there joins the list, checked.
    fn add_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let back = window.focused(cx);
        let dialog = cx.new(|cx| AddLocation::new(Purpose::Choose, Kind::Ssh, None, window, cx));
        let heard = cx.subscribe_in(&dialog, window, |this, _, event, window, cx| {
            if let AddLocationEvent::Chosen {
                kind,
                name,
                sessions,
            } = event
            {
                this.hosts.retain(|host| !host.kind.same_place(kind));
                this.hosts.push(Host {
                    kind: kind.clone(),
                    name: name.clone(),
                    sessions: *sessions,
                });
            }
            this.close_dialog(window, cx);
        });
        self.dialog = Some((dialog, back, heard));
        cx.notify();
    }

    fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((_, back, _)) = self.dialog.take() {
            self.sync.update(cx, |sync, _| sync.cancel_test());
            if let Some(back) = back {
                back.focus(window, cx);
            }
            cx.notify();
        }
    }

    /// A place's row: a checkbox, its kind's icon, its name and kind over its own lines, and
    /// what was found there.
    fn row(&self, at: usize, offered: Offered, theme: &SbTheme, cx: &Context<Self>) -> AnyElement {
        let c = &theme.colors;
        let checked = self.is_checked(&offered);
        let target = match &offered.kind {
            LocationKind::Local => "first-run/this-machine".to_string(),
            LocationKind::Wsl { distro } => format!("first-run/distro/{distro}"),
            LocationKind::Ssh { host } => format!("first-run/host/{host}"),
        };
        let kind = offered.kind.clone();
        tab_stop(
            div()
                .id(SharedString::from(format!("first-run-row-{at}")))
                .role(Role::CheckBox)
                .flex()
                .items_center()
                .gap(theme.spacing.lg)
                .py(theme.spacing.lg)
                .px(ROW_PADDING)
                .border_1()
                .border_color(gpui::transparent_black())
                .when(at > 0, |this| this.border_t_1().border_color(c.line_subtle))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle(kind.clone(), checked, cx);
                })),
            theme,
        )
        .child(checkbox(checked, theme))
        .child(icon(
            kind_icon(&offered.kind),
            theme.sizes.icon,
            if offered.dim { c.fg3 } else { c.fg2 },
        ))
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
                                .text_color(if offered.dim { c.fg2 } else { c.fg })
                                .text_spec(&theme.text.control)
                                .child(offered.name.clone()),
                        )
                        .child(location_tag(
                            kind_name(&offered.kind),
                            &offered.kind,
                            false,
                            theme,
                        ))
                        .children(offered.state.map(|state| {
                            div()
                                .text_color(c.fg3)
                                .text_spec(&theme.text.small)
                                .child(state)
                        })),
                )
                .children(offered.lines.into_iter().map(|(line, failed)| {
                    div()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .map(|this| {
                            if failed {
                                this.text_color(c.red).text_spec(&theme.text.secondary)
                            } else if offered.dim {
                                this.text_color(c.fg3).text_spec(&theme.text.secondary)
                            } else {
                                this.text_color(c.fg3).text_spec(&theme.text.path_small)
                            }
                        })
                        .child(line)
                })),
        )
        .child(
            div()
                .flex_none()
                .text_color(if offered.dim { c.fg3 } else { c.fg2 })
                .text_spec(&theme.text.number)
                .child(offered.count),
        )
        .automation_target(target)
    }

    /// Where the archive goes, its folder in the code font inside the sentence.
    fn archive_line(&self, theme: &SbTheme, cx: &App) -> AnyElement {
        const BEFORE: &str = "The archive goes in scrollback's data folder, ";
        const AFTER: &str = ". Settings can point it somewhere else later.";
        let c = &theme.colors;
        let folder = unbroken(&self.archive.read(cx).dir().display().to_string());
        let run = |len: usize, family: &gpui::SharedString| TextRun {
            len,
            font: font(family.clone()),
            color: c.fg3,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = vec![
            run(BEFORE.len(), &theme.fonts.ui),
            run(folder.len(), &theme.fonts.mono),
            run(AFTER.len(), &theme.fonts.ui),
        ];
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_start()
            .gap(theme.spacing.sm)
            .text_spec(&theme.text.secondary)
            .child(icon(SbIcon::Archive, theme.sizes.icon_sm, c.fg3))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(StyledText::new(format!("{BEFORE}{folder}{AFTER}")).with_runs(runs)),
            )
            .into_any_element()
    }
}

impl Render for FirstRun {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.sb_theme().clone();
        let c = &theme.colors;
        let checked = self.checked(cx).len();
        let looked = self.sync.read(cx).found().is_some();
        let ready = checked > 0 && !self.adding;
        let names: Vec<&str> = cx
            .services()
            .providers
            .iter()
            .map(|provider| provider.display().name)
            .collect();
        let wsl_failed = self.sync.read(cx).distros().and_then(|distros| {
            distros
                .listed
                .as_ref()
                .err()
                .map(|said| format!("WSL's distros couldn't be listed: {said}"))
        });
        let rows: Vec<AnyElement> = self
            .offered(cx)
            .into_iter()
            .enumerate()
            .map(|(at, offered)| self.row(at, offered, &theme, cx))
            .collect();
        let dialog = self.dialog.as_ref().map(|(dialog, ..)| {
            let this = cx.entity().downgrade();
            overlay::modal(
                dialog.clone(),
                Layer::Dialog,
                window,
                &theme,
                move |window, cx| {
                    this.update(cx, |this, cx| this.close_dialog(window, cx))
                        .ok();
                },
            )
        });
        // The dialog's scrim covers the whole window: it is drawn beside the page, whose
        // edges would clip it.
        let page = div()
            .size_full()
            .flex()
            .justify_center()
            .items_start()
            .pt(theme.sizes.first_run_top)
            .overflow_hidden()
            .child(
                div()
                    .flex_none()
                    .w(theme.sizes.first_run_width)
                    .flex()
                    .flex_col()
                    .gap(SECTION_GAP)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(theme.spacing.md)
                            .child(
                                div()
                                    .text_spec(&theme.text.state_title)
                                    .child("Add your first location")
                                    .automation_target("first-run/title"),
                            )
                            .child(
                                div()
                                    .text_color(c.fg2)
                                    .text_spec(&theme.text.lead)
                                    .child(about(&names)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(theme.spacing.md)
                            .child(overline(&format!("Found on {}", local_name()), &theme))
                            .child(
                                div()
                                    .rounded(theme.radii.card)
                                    .border_1()
                                    .border_color(c.line)
                                    .bg(c.s1)
                                    .overflow_hidden()
                                    .children(rows),
                            )
                            .children(wsl_failed.map(|said| {
                                div()
                                    .text_color(c.red)
                                    .text_spec(&theme.text.secondary)
                                    .child(said)
                                    .automation_target("first-run/wsl-failed")
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(theme.spacing.md)
                            .child(overline("Another machine", &theme))
                            .child(
                                tab_stop(
                                    div()
                                        .id("first-run-add-host")
                                        .role(Role::Button)
                                        .flex()
                                        .items_center()
                                        .gap(theme.spacing.md)
                                        .h(px(40.))
                                        .px(ROW_PADDING)
                                        .rounded(theme.radii.card)
                                        .border_1()
                                        .border_dashed()
                                        .border_color(c.line_strong)
                                        .text_color(c.fg2)
                                        .text_spec(&theme.text.control)
                                        .cursor_pointer()
                                        .hover(|style| style.bg(c.s1))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.add_host(window, cx);
                                        })),
                                    &theme,
                                )
                                .child(icon(SbIcon::Plus, theme.sizes.icon_sm, c.fg2))
                                .child("Add an SSH host…")
                                .automation_target("first-run/add-host"),
                            ),
                    )
                    .child(div().flex_none().h(px(1.)).bg(c.line_subtle))
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap(theme.spacing.lg)
                            .child(self.archive_line(&theme, cx))
                            .child(
                                button("first-run-add", ButtonKind::Primary, false, ready, &theme)
                                    .when(ready, |this| {
                                        this.on_click(cx.listener(|this, _, _, cx| this.add(cx)))
                                    })
                                    // It counts what is checked; with nothing checked there
                                    // is nothing to count, and until this machine has been
                                    // looked at, nothing to check.
                                    .child(match checked {
                                        _ if !looked => "Looking for transcripts…".to_string(),
                                        0 => "Add a location and sync".to_string(),
                                        1 => "Add 1 location and sync".to_string(),
                                        many => format!("Add {many} locations and sync"),
                                    })
                                    .automation_target("first-run/add"),
                            ),
                    ),
            );
        div().size_full().child(page).children(dialog)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn the_copy_names_an_agent_only_from_its_provider() {
        assert_eq!(
            about(&["Some Agent"]),
            "scrollback copies Some Agent transcripts from each location into its own archive \
             and reads only the copies. Some Agent deletes old transcripts on its own; what is \
             copied here stays."
        );
        assert_eq!(
            about(&["One", "Other"]),
            "scrollback copies coding-agent transcripts from each location into its own \
             archive and reads only the copies. Agents delete old transcripts on their own; \
             what is copied here stays."
        );
    }

    #[test]
    fn the_row_counts_each_agents_sessions() {
        assert_eq!(found_line(&[]), "no transcripts found");
        assert_eq!(found_line(&[("One".to_string(), 418)]), "418 sessions");
        assert_eq!(
            found_line(&[("One".to_string(), 1414), ("Other".to_string(), 1)]),
            "One · 1,414 sessions, Other · 1 session"
        );
    }
}
