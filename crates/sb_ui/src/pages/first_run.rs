//! The first run (PLAN §11 `first_run`; DESIGN §4.10, `E-First-Run`): "Add your first
//! location", shown while the archive has none. It offers what the providers found on this
//! machine, checked, and its button adds the location and starts its first sync.
//!
//! This machine is the one location it offers so far: the WSL distros found here, and adding a
//! host over SSH, come with the Add location flow (DESIGN §4.10–§4.12). The fixed copy names no
//! agent (PLAN §11); where the screen names one, the name is its provider's.

use gpui::{
    AnyElement, App, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    Pixels, Render, Role, StatefulInteractiveElement as _, Styled as _, StyledText, Subscription,
    TextRun, Window, div, font, prelude::FluentBuilder as _, px,
};

use sb_core::location::LocationKind;

use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{
    ButtonKind, button, checkbox, icon, kind_icon, kind_name, location_tag, overline,
};
use crate::format;
use crate::icons::SbIcon;
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

/// What this machine's row says at its right: how many sessions each agent has here.
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
    /// Whether this machine is checked, once the owner has said; until then it is when an
    /// agent was found here.
    checked: Option<bool>,
    /// The location is being added.
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
            checked: None,
            adding: false,
        }
    }

    fn is_checked(&self, found: Option<&Survey>) -> bool {
        self.checked
            .unwrap_or_else(|| found.is_some_and(|found| found.iter().any(Result::is_ok)))
    }

    /// Adds the checked location and starts its first sync.
    pub fn add(&mut self, cx: &mut Context<Self>) {
        let sync = self.sync.read(cx);
        if self.adding || sync.found().is_none() || !self.is_checked(sync.found()) {
            return;
        }
        self.adding = true;
        self.sync.update(cx, |sync, cx| sync.add_local(cx));
        cx.notify();
    }

    /// This machine's row: a checkbox, its icon, its name and kind over where its transcripts
    /// are, and what was found.
    fn machine_row(
        &self,
        found: Option<&Survey>,
        theme: &SbTheme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let checked = self.is_checked(found);
        let agents: Vec<(String, usize)> = found
            .into_iter()
            .flatten()
            .flatten()
            .map(|found| (agents::name(found.provider.as_str(), cx), found.sessions))
            .collect();
        let roots: Vec<String> = found
            .into_iter()
            .flatten()
            .flatten()
            .map(|found| format::path(&found.root))
            .collect();
        let failures: Vec<&String> = found
            .into_iter()
            .flatten()
            .filter_map(|found| found.as_ref().err())
            .collect();
        div()
            .id("first-run-machine")
            .role(Role::CheckBox)
            // `Tab` reaches the row, and `Space` checks it.
            .tab_index(0)
            .focus_visible(|style| style.bg(c.s2))
            .flex()
            .items_center()
            .gap(theme.spacing.lg)
            .py(theme.spacing.lg)
            .px(ROW_PADDING)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.checked = Some(!checked);
                cx.notify();
            }))
            .child(checkbox(checked, theme))
            .child(icon(
                kind_icon(&LocationKind::Local),
                theme.sizes.icon,
                c.fg2,
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
                            .child(div().text_spec(&theme.text.control).child(local_name()))
                            .child(location_tag(
                                kind_name(&LocationKind::Local),
                                &LocationKind::Local,
                                false,
                                theme,
                            )),
                    )
                    .children(roots.into_iter().map(|root| {
                        div()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_color(c.fg3)
                            .text_spec(&theme.text.path_small)
                            .child(root)
                    }))
                    .children(failures.into_iter().map(|failure| {
                        div()
                            .text_color(c.red)
                            .text_spec(&theme.text.secondary)
                            .child(failure.clone())
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(if agents.is_empty() { c.fg3 } else { c.fg2 })
                    .text_spec(&theme.text.number)
                    .child(match found {
                        Some(_) => found_line(&agents),
                        None => "looking…".to_string(),
                    }),
            )
            .automation_target("first-run/this-machine")
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.sb_theme().clone();
        let c = &theme.colors;
        let found = self.sync.read(cx).found();
        let ready = found.is_some() && self.is_checked(found) && !self.adding;
        let names: Vec<&str> = cx
            .services()
            .providers
            .iter()
            .map(|provider| provider.display().name)
            .collect();
        div()
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
                                    .child(self.machine_row(found, &theme, cx)),
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
                                    .child(if found.is_none() {
                                        "Looking for transcripts…"
                                    } else if self.is_checked(found) {
                                        "Add 1 location and sync"
                                    } else {
                                        "Add a location and sync"
                                    })
                                    .automation_target("first-run/add"),
                            ),
                    ),
            )
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
