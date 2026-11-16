//! The archive can't be reached (DESIGN §4.14, `E-Edge-Archive-Missing`): what the main area
//! shows when no archive is open. A 600-wide column under a `red-soft` tile: what happened, the
//! folder, what scrollback won't do, the ways on, and what is known of the archive last open.
//!
//! - **The folder isn't there, or holds no archive** (a folder the owner chose; a drive
//!   unplugged, a letter changed): **Try again**, **Locate the archive…** and **Use a different
//!   folder…**. Locating accepts a folder whose `archive.json` names the archive last open; one
//!   that holds another archive asks "That folder holds a different archive. Open it
//!   instead?". gpui says nothing when a drive is attached, so it is Try again that looks
//!   again.
//! - **Another scrollback has it open:** a lock, and **Try again**.
//! - **The app's settings can't be read** (`crate::services::settings`): why, and **Use the
//!   default archive** or **Use a different folder…**; nothing was opened, and the file is
//!   left as it is until the owner chooses.
//! - **Anything else** (its catalogue is gone, its folder holds what is left of one): the
//!   archive's words, and the same three ways on as a folder that isn't there.
//!
//! Every way on goes through `SettingsService::use_folder`, which needs no archive open. A
//! different folder asks first, as Settings does: whether its archive opens or a new one is
//! made there. The question takes the keyboard, and `Esc` (the shell's `Dismiss`) closes it.

use std::path::PathBuf;

use gpui::{
    AnyElement, AppContext as _, Context, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Role, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::FocusTrapElement as _;
use sb_core::archive::{self, Holds};

use super::states::{alert_tile, state, with_path};
use crate::automation::AutomationTargetExt as _;
use crate::components::{ButtonKind, button, icon, kv, with_numbers};
use crate::format;
use crate::icons::SbIcon;
use crate::overlay::{self, Layer};
use crate::services::archive::Failure;
use crate::services::settings::{Switch, why_refused};
use crate::services::{ActiveServices as _, ArchiveState};
use crate::system;
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _};

/// What the screen says when another scrollback has the archive.
const LOCKED: &str = "The archive is open in another scrollback window. Close it there, or \
                      carry on there, then try again.";
/// What it says when the archive is not there to open, or is not what it was (DESIGN §4.14).
const NOT_REPLACED: &str = "scrollback won't make a new archive in its place, and syncing is \
                            paused until the archive is back. Nothing at your locations is \
                            affected.";

/// A folder the owner picked, waiting for them to say they mean it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Asking {
    /// It holds another archive than the one last open: open it instead?
    Different(PathBuf),
    /// It holds an archive, which opens.
    Open(PathBuf),
    /// It holds nothing: a new archive is made there.
    Make(PathBuf),
}

pub struct UnavailablePage {
    asking: Option<Asking>,
    /// Why the folder located can't be taken, until another is.
    refused: Option<String>,
    dialog_focus: FocusHandle,
    cancel_focus: FocusHandle,
    /// While the question is open: where the keyboard was before it.
    back: Option<Option<FocusHandle>>,
}

impl UnavailablePage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            asking: None,
            refused: None,
            dialog_focus: cx.focus_handle(),
            cancel_focus: cx.focus_handle(),
            back: None,
        }
    }

    /// Whether a question is open over the window.
    pub fn is_asking(&self) -> bool {
        self.asking.is_some()
    }

    /// `Esc`: closes the question. `false` when none was open.
    pub fn dismiss(&mut self, cx: &mut Context<Self>) -> bool {
        let open = self.asking.take().is_some();
        cx.notify();
        open
    }

    /**
     * Locate the archive…: a folder whose `archive.json` names the archive last open opens at
     * once; one that holds another asks first; one that holds none is refused.
     */
    fn locate(&mut self, cx: &mut Context<Self>) {
        let picked = system::pick_folder("Locate the archive", cx);
        cx.spawn(async move |this, cx| {
            let Ok(Some(folder)) = picked.await else {
                return;
            };
            let id = cx
                .background_spawn({
                    let folder = folder.clone();
                    async move { archive::id_in(&folder) }
                })
                .await;
            this.update(cx, |this, cx| {
                let settings = cx.services().settings.clone();
                let last = settings
                    .read(cx)
                    .settings()
                    .last
                    .as_ref()
                    .map(|last| last.id.clone());
                this.refused = None;
                match id {
                    Ok(Some(id)) if Some(&id) == last.as_ref() => {
                        settings.update(cx, |settings, cx| settings.use_folder(folder, cx));
                    }
                    Ok(Some(_)) => this.asking = Some(Asking::Different(folder)),
                    Ok(None) => {
                        this.refused = Some(format!("{} holds no archive.", folder.display()));
                    }
                    Err(error) => {
                        log::warn!("{} wasn't looked into: {error}", folder.display());
                        this.refused = Some(why_refused(&folder, &error));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Use a different folder…: what it holds, then the question (the module docs).
    fn choose(&mut self, cx: &mut Context<Self>) {
        let picked = system::pick_folder("Use this folder", cx);
        cx.spawn(async move |this, cx| {
            let Ok(Some(folder)) = picked.await else {
                return;
            };
            let looked = cx
                .background_spawn({
                    let folder = folder.clone();
                    async move { archive::what_is_in(&folder) }
                })
                .await;
            this.update(cx, |this, cx| {
                this.refused = None;
                match looked {
                    Ok(Holds::Archive) => this.asking = Some(Asking::Open(folder)),
                    Ok(Holds::Nothing) => this.asking = Some(Asking::Make(folder)),
                    /* What can't be the archive's goes the way a refusal does, without asking. */
                    _ => {
                        let settings = cx.services().settings.clone();
                        settings.update(cx, |settings, cx| settings.use_folder(folder, cx));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Opens the folder asked about: the owner said they mean it.
    fn answer(&mut self, cx: &mut Context<Self>) {
        if let Some(Asking::Different(folder) | Asking::Open(folder) | Asking::Make(folder)) =
            self.asking.take()
        {
            let settings = cx.services().settings.clone();
            settings.update(cx, |settings, cx| settings.use_folder(folder, cx));
        }
        cx.notify();
    }

    /// Gives the keyboard to the question's Cancel as it opens, and back as it closes.
    fn follow_question(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match (self.asking.is_some(), self.back.is_some()) {
            (true, false) => {
                self.back = Some(window.focused(cx));
                self.cancel_focus.focus(window, cx);
            }
            (false, true) => {
                if let Some(Some(back)) = self.back.take() {
                    back.focus(window, cx);
                }
            }
            _ => {}
        }
    }

    /// The question over the scrim.
    fn question(
        &self,
        window: &Window,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let asking = self.asking.clone()?;
        let c = &theme.colors;
        let room = overlay::room(window.viewport_size(), theme);
        let (title, what, yes) = match &asking {
            Asking::Different(folder) => (
                "That folder holds a different archive. Open it instead?",
                format!(
                    "{} holds another archive than the one scrollback last had open. It opens \
                     with its own locations, folders and settings; the one that can't be \
                     reached stays as it is, wherever it is.",
                    folder.display()
                ),
                "Open it",
            ),
            Asking::Open(folder) => (
                "Open the archive in this folder?",
                format!(
                    "scrollback opens the archive in {}, with its own locations, folders and \
                     settings.",
                    folder.display()
                ),
                "Open it",
            ),
            Asking::Make(folder) => (
                "Make a new archive in this folder?",
                format!(
                    "{} is empty. scrollback makes a new archive there; the first run asks for \
                     its locations.",
                    folder.display()
                ),
                "Make it",
            ),
        };
        let weak = cx.entity().downgrade();
        let dialog = overlay::surface(theme)
            .id("archive-question")
            .role(Role::Dialog)
            .aria_label(title)
            .track_focus(&self.dialog_focus)
            .w(theme.sizes.dialog_width.min(room.width))
            .mt(theme.sizes.overlay_margin * 4.)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(theme.spacing.md)
                    .p(theme.spacing.xl)
                    .child(div().text_spec(&theme.text.sheet_title).child(title))
                    .child(
                        div()
                            .text_color(c.fg2)
                            .text_spec(&theme.text.body)
                            .child(what),
                    ),
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
                        button(
                            "archive-question-cancel",
                            ButtonKind::Plain,
                            false,
                            true,
                            theme,
                        )
                        .track_focus(&self.cancel_focus)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.dismiss(cx);
                        }))
                        .child("Cancel")
                        .automation_target("archive/question/cancel"),
                    )
                    .child(
                        button(
                            "archive-question-ok",
                            ButtonKind::Primary,
                            false,
                            true,
                            theme,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.answer(cx)))
                        .child(yes)
                        .automation_target("archive/question/ok"),
                    ),
            )
            .focus_trap("archive-question-trap", &self.dialog_focus)
            .automation_target("archive/question");
        Some(overlay::modal(
            dialog,
            Layer::Dialog,
            window,
            theme,
            move |_, cx| {
                weak.update(cx, |this, cx| {
                    this.dismiss(cx);
                })
                .ok();
            },
        ))
    }
}

/// `a3f0 61c2`: an archive's id as the page writes it, its first eight digits in two groups.
fn short_id(id: &str) -> String {
    let start: String = id.chars().take(8).collect();
    match start.split_at_checked(4) {
        Some((first, second)) if !second.is_empty() => format!("{first} {second}"),
        _ => start,
    }
}

impl Render for UnavailablePage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.follow_question(window, cx);
        let theme = cx.sb_theme().clone();
        let c = &theme.colors;
        let services = cx.services();
        let archive = services.archive.read(cx);
        let ArchiveState::Failed(failure) = archive.state() else {
            return div().into_any_element();
        };
        let failure = failure.clone();
        let folder = archive.dir().display().to_string();
        let settings = services.settings.read(cx);
        let last = settings.settings().last.clone();
        let switch = settings.switch().cloned();
        let opening = settings.is_switching();
        let (glyph, title) = match &failure {
            Failure::Locked(_) => (
                SbIcon::Lock,
                "The archive is open in another scrollback window",
            ),
            Failure::Settings(_) => (
                SbIcon::CircleAlert,
                "scrollback's settings couldn't be read",
            ),
            Failure::Missing { .. } => (SbIcon::HardDrive, "The archive can't be reached"),
            Failure::Other(_) => (SbIcon::HardDrive, "The archive can't be opened"),
        };
        let paragraph = |text: String| {
            div()
                .text_spec(&theme.text.lead)
                .text_color(c.fg2)
                .child(with_path(&text, &folder, c.fg2, &theme))
        };
        let said = match &failure {
            Failure::Locked(_) => LOCKED.to_string(),
            Failure::Settings(why) => format!(
                "{why} No archive was opened, and the file is left as it is until you choose \
                 one: the default archive in scrollback's data folder, or another folder."
            ),
            Failure::Missing { there: false } => format!(
                "The folder isn't there. If it is on a drive that's unplugged, plug it back in. \
                 If the drive's letter changed, locate the folder. {NOT_REPLACED}"
            ),
            Failure::Missing { there: true } => format!(
                "The folder is there, and holds no archive. If the archive was moved, locate \
                 it. {NOT_REPLACED}"
            ),
            Failure::Other(why) => format!("{}. {NOT_REPLACED}", why.trim_end_matches('.')),
        };
        let action = |id: &'static str, kind, label: &'static str, glyph: Option<SbIcon>| {
            button(id, kind, false, !opening, &theme)
                .when_some(glyph, |this, glyph| {
                    this.child(icon(
                        glyph,
                        theme.sizes.icon_sm,
                        match kind {
                            ButtonKind::Primary => c.accent_ink,
                            _ => c.fg2,
                        },
                    ))
                })
                .child(label)
        };
        let buttons = match &failure {
            Failure::Locked(_) => div().flex().gap(theme.spacing.md).child(
                action(
                    "archive-retry",
                    ButtonKind::Primary,
                    "Try again",
                    Some(SbIcon::RefreshCw),
                )
                .when(!opening, |this| {
                    this.on_click(cx.listener(|_, _, _, cx| {
                        let archive = cx.services().archive.clone();
                        archive.update(cx, |archive, cx| archive.retry(cx));
                    }))
                })
                .automation_target("archive/retry"),
            ),
            Failure::Settings(_) => div()
                .flex()
                .gap(theme.spacing.md)
                .child(
                    action(
                        "archive-use-default",
                        ButtonKind::Primary,
                        "Use the default archive",
                        None,
                    )
                    .when(!opening, |this| {
                        this.on_click(cx.listener(|_, _, _, cx| {
                            let settings = cx.services().settings.clone();
                            settings.update(cx, |settings, cx| {
                                let default = settings.default_archive();
                                settings.use_folder(default, cx);
                            });
                        }))
                    })
                    .automation_target("archive/use-default"),
                )
                .child(
                    action(
                        "archive-use-folder",
                        ButtonKind::Plain,
                        "Use a different folder…",
                        Some(SbIcon::Folder),
                    )
                    .when(!opening, |this| {
                        this.on_click(cx.listener(|this, _, _, cx| this.choose(cx)))
                    })
                    .automation_target("archive/use-folder"),
                ),
            Failure::Missing { .. } | Failure::Other(_) => div()
                .flex()
                .gap(theme.spacing.md)
                .child(
                    action(
                        "archive-retry",
                        ButtonKind::Primary,
                        "Try again",
                        Some(SbIcon::RefreshCw),
                    )
                    .when(!opening, |this| {
                        this.on_click(cx.listener(|_, _, _, cx| {
                            let archive = cx.services().archive.clone();
                            archive.update(cx, |archive, cx| archive.retry(cx));
                        }))
                    })
                    .automation_target("archive/retry"),
                )
                .child(
                    action(
                        "archive-locate",
                        ButtonKind::Plain,
                        "Locate the archive…",
                        Some(SbIcon::Folder),
                    )
                    .when(!opening, |this| {
                        this.on_click(cx.listener(|this, _, _, cx| this.locate(cx)))
                    })
                    .automation_target("archive/locate"),
                )
                .child(
                    action(
                        "archive-use-folder",
                        ButtonKind::Ghost,
                        "Use a different folder…",
                        None,
                    )
                    .when(!opening, |this| {
                        this.on_click(cx.listener(|this, _, _, cx| this.choose(cx)))
                    })
                    .automation_target("archive/use-folder"),
                ),
        };
        let note = match (&switch, &self.refused) {
            (Some(Switch::Opening(folder)), _) => {
                Some((c.accent_text, format!("Opening {}…", folder.display())))
            }
            (Some(Switch::Refused { why, .. }), _) => Some((c.red, why.clone())),
            (None, Some(why)) => Some((c.red, why.clone())),
            (None, None) => None,
        };
        let row = |key: &'static str, value: String| {
            kv(
                key,
                with_numbers(value, c.fg2, &theme.text.secondary, &theme),
                &theme,
            )
        };
        let known = last
            .filter(|_| !matches!(failure, Failure::Settings(_)))
            .map(|last| {
                let held = match last.bytes {
                    Some(bytes) => format!(
                        "{} · {}",
                        format::plural(last.sessions, "session"),
                        format::size(bytes)
                    ),
                    None => format::plural(last.sessions, "session"),
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(theme.spacing.xs)
                    .pt(px(14.))
                    .border_t_1()
                    .border_color(c.line_subtle)
                    .child(row(
                        "Last opened",
                        format!(
                            "{}, {}",
                            format::day(last.opened, format::now()),
                            format::clock(last.opened)
                        ),
                    ))
                    .child(row("Held then", held))
                    .child(kv(
                        "Archive id",
                        div().text_spec(&theme.text.path_small).child(format!(
                            "{} · checked when you locate it",
                            short_id(&last.id)
                        )),
                        &theme,
                    ))
                    .automation_target("archive/known")
            });
        div()
            .size_full()
            .flex()
            .justify_center()
            .pt(px(120.))
            .child(
                state(alert_tile(glyph, &theme), title, &theme)
                    .when(!matches!(failure, Failure::Settings(_)), |this| {
                        this.child(
                            div()
                                .text_spec(&theme.text.path)
                                .text_color(c.fg)
                                .child(folder.clone())
                                .automation_target("archive/folder"),
                        )
                    })
                    .child(paragraph(said))
                    .child(buttons)
                    .children(note.map(|(color, text)| {
                        div()
                            .text_spec(&theme.text.secondary)
                            .text_color(color)
                            .child(text)
                            .automation_target("archive/note")
                    }))
                    .children(known)
                    .automation_target("archive/failed"),
            )
            .children(self.question(window, &theme, cx))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn an_archive_id_is_written_as_the_board_does() {
        assert_eq!(short_id("a3f061c2d4e5f6a7"), "a3f0 61c2");
        assert_eq!(short_id("a3f"), "a3f");
    }
}
