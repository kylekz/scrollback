//! Settings › Archive's Move… (DESIGN §4.13): the folder picker, the line under the folder's
//! buttons while the folder is looked at, its refusal or the question, the move's progress
//! with Stop, how a move that didn't happen ended, and after one that did, where the old copy
//! is, with Show in Explorer and Dismiss and never a way to delete it. The service behind it is
//! `crate::services::settings`'s.

use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _, Role,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _,
};
use gpui_component::FocusTrapElement as _;
use std::path::{Path, PathBuf};

use sb_core::archive::{self, MoveStage};

use super::SettingsPage;
use crate::automation::AutomationTargetExt as _;
use crate::components::{
    ButtonKind, button, info_banner, kv, path_in_words, paths_tooltip, progress_bar, warn_banner,
    with_numbers,
};
use crate::format;
use crate::overlay::{self, Layer};
use crate::pages::first_sync::ESTIMATE_AFTER_MS;
use crate::services::ActiveServices as _;
use crate::services::settings::{Ended, MoveState, Running, Said};
use crate::system;
use crate::theme::{SbTheme, StyledText as _};

/**
 * The warning a question about putting the archive in `folder` carries when a cloud service
 * keeps the folder in step (PLAN §4.4): allowed, but the archive changes often, and a service
 * that syncs it can make conflicting copies of its catalogue.
 */
pub(crate) fn synced_warning(folder: &Path, theme: &SbTheme) -> Option<AnyElement> {
    let service = archive::synced_by(folder)?;
    Some(
        warn_banner(theme)
            .child(div().flex_1().min_w_0().child(format!(
                "This folder is synced by {service}. The archive changes often, and {service} \
                 can make conflicting copies of it; a folder it doesn't sync is safer."
            )))
            .automation_target("synced-warning")
            .into_any_element(),
    )
}

/// How far a running move is, in words: its stage and its counts, and how long it has left
/// once it has copied for a minute.
fn doing(running: &Running, now: i64) -> String {
    let progress = running.progress;
    if running.is_stopping() {
        return "Stopping…".to_string();
    }
    match progress.stage {
        MoveStage::Copying => {
            let elapsed = now - running.started;
            let done = progress.fraction();
            let left = if elapsed >= ESTIMATE_AFTER_MS && done > 0. {
                format!(
                    "{} left",
                    format::about((elapsed as f64 * (1. - done) / done) as u64 / 1000)
                )
            } else {
                "estimating…".to_string()
            };
            format!(
                "Moving · {} of {} files · {} · {left}",
                format::count(progress.files_done as u64),
                format::count(progress.files as u64),
                format::bytes_of(progress.bytes_done, progress.bytes, " of ")
            )
        }
        MoveStage::Finishing if progress.files == 0 => {
            "Finishing · copying the catalogue".to_string()
        }
        MoveStage::Finishing => format!(
            "Finishing · copying the catalogue and {} written since",
            format::plural(progress.files, "file")
        ),
        MoveStage::Switching => {
            format!("Switching to {}…", path_in_words(&running.plan.target))
        }
    }
}

impl SettingsPage {
    /// Move…: the folder picker, then the folder looked at.
    pub(super) fn choose_move_folder(&mut self, cx: &mut Context<Self>) {
        let picked = system::pick_folder("Move here", cx);
        cx.spawn(async move |_, cx| {
            if let Ok(Some(folder)) = picked.await {
                cx.update(|cx| {
                    let settings = cx.services().settings.clone();
                    settings.update(cx, |settings, cx| settings.check_move(folder, cx));
                });
            }
        })
        .detach();
    }

    /// What the folder card shows of Move… under its buttons (the module docs).
    pub(super) fn move_lines(&self, theme: &SbTheme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let c = &theme.colors;
        let services = cx.services();
        let settings = services.settings.read(cx);
        let dir = services.archive.read(cx).dir().to_path_buf();
        let line = |text: String, color| {
            div()
                .flex_1()
                .min_w_0()
                .text_spec(&theme.text.secondary)
                .text_color(color)
                .child(text)
        };
        /* Words that name paths, cut, with the paths whole in a tooltip. */
        let said = |id: &'static str, said: &Said, color| {
            line(said.text.clone(), color)
                .id(id)
                .when(!said.paths.is_empty(), |this| {
                    this.tooltip(paths_tooltip(id, &said.paths))
                })
        };
        let dismiss = |id: &'static str, target: &'static str| {
            button(id, ButtonKind::Ghost, true, true, theme)
                .debug_selector(move || id.to_string())
                .on_click(cx.listener(|_, _, _, cx| {
                    let settings = cx.services().settings.clone();
                    settings.update(cx, |settings, cx| settings.dismiss_move(cx));
                }))
                .child("Dismiss")
                .automation_target(target)
        };
        let mut lines = Vec::new();
        match settings.move_state() {
            Some(MoveState::Checking(folder)) => lines.push(
                line(
                    format!("Checking {}…", path_in_words(folder)),
                    c.accent_text,
                )
                .id("settings-move-checking")
                .tooltip(paths_tooltip(
                    "settings-move-checking",
                    std::slice::from_ref(folder),
                ))
                .automation_target("settings/move/checking")
                .into_any_element(),
            ),
            Some(MoveState::Refused { why, .. }) => lines.push(
                div()
                    .flex()
                    .items_start()
                    .gap(theme.spacing.md)
                    .child(said("settings-move-refused-words", why, c.red))
                    .child(dismiss(
                        "settings-move-refused-dismiss",
                        "settings/move/dismiss",
                    ))
                    .automation_target("settings/move/refused")
                    .into_any_element(),
            ),
            Some(MoveState::Running(running)) => {
                let progress = running.progress;
                let fraction = match progress.stage {
                    MoveStage::Copying => progress.fraction() as f32,
                    MoveStage::Finishing | MoveStage::Switching => 1.,
                };
                let stop = running.can_stop() && !running.is_stopping();
                lines.push(
                    div()
                        .flex()
                        .flex_col()
                        .gap(theme.spacing.sm)
                        .child(progress_bar(fraction, theme))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(theme.spacing.md)
                                .child(
                                    div()
                                        .id("settings-move-doing")
                                        .flex_1()
                                        .min_w_0()
                                        .child(with_numbers(
                                            if settings.quits_after_finish() {
                                                format!(
                                                    "{} · scrollback quits once it is done",
                                                    doing(running, format::now())
                                                )
                                            } else {
                                                doing(running, format::now())
                                            },
                                            c.accent_text,
                                            &theme.text.secondary,
                                            theme,
                                        ))
                                        .when(progress.stage == MoveStage::Switching, |this| {
                                            this.tooltip(paths_tooltip(
                                                "settings-move-doing",
                                                std::slice::from_ref(&running.plan.target),
                                            ))
                                        }),
                                )
                                .when(stop, |this| {
                                    this.child(
                                        button(
                                            "settings-move-stop",
                                            ButtonKind::Plain,
                                            true,
                                            true,
                                            theme,
                                        )
                                        .debug_selector(|| "settings-move-stop".into())
                                        .on_click(cx.listener(|_, _, _, cx| {
                                            let settings = cx.services().settings.clone();
                                            settings.update(cx, |settings, cx| {
                                                settings.stop_move();
                                                cx.notify();
                                            });
                                        }))
                                        .child("Stop")
                                        .automation_target("settings/move/stop"),
                                    )
                                }),
                        )
                        .automation_target("settings/move/progress")
                        .into_any_element(),
                );
            }
            Some(MoveState::Ended(ended)) => {
                let color = match ended {
                    Ended::Stopped { .. } => c.fg2,
                    Ended::Failed { .. } => c.red,
                    Ended::CutShort { .. } => c.yellow,
                };
                lines.push(
                    div()
                        .flex()
                        .items_start()
                        .gap(theme.spacing.md)
                        .child(said("settings-move-ended-words", &ended.words(), color))
                        .child(dismiss(
                            "settings-move-ended-dismiss",
                            "settings/move/dismiss",
                        ))
                        .automation_target("settings/move/ended")
                        .into_any_element(),
                );
            }
            Some(MoveState::Asking(_) | MoveState::Clearing(_)) | None => {}
        }
        if let Some(moved) = settings.moved_here(&dir) {
            let old = moved.from.clone();
            let words = format!(
                "Moved from {} on {}. That folder is as it was. Delete it yourself once you're \
                 satisfied that everything is here.",
                path_in_words(&old),
                format::day(moved.at, format::now())
            );
            lines.push(
                info_banner(theme)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(theme.spacing.md)
                            .child(div().id("settings-moved-words").child(words).tooltip(
                                paths_tooltip("settings-moved-words", std::slice::from_ref(&old)),
                            ))
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .gap(theme.spacing.md)
                                    .child(
                                        button(
                                            "settings-moved-reveal",
                                            ButtonKind::Plain,
                                            true,
                                            true,
                                            theme,
                                        )
                                        .debug_selector(|| "settings-moved-reveal".into())
                                        .on_click(
                                            cx.listener(move |_, _, _, cx| {
                                                system::reveal(&old, cx)
                                            }),
                                        )
                                        .child(if cfg!(target_os = "macos") {
                                            "Show the old folder in Finder"
                                        } else {
                                            "Show the old folder in Explorer"
                                        })
                                        .automation_target("settings/moved/reveal"),
                                    )
                                    .child(
                                        button(
                                            "settings-moved-dismiss",
                                            ButtonKind::Ghost,
                                            true,
                                            true,
                                            theme,
                                        )
                                        .debug_selector(|| "settings-moved-dismiss".into())
                                        .on_click(cx.listener(|_, _, _, cx| {
                                            let settings = cx.services().settings.clone();
                                            settings.update(cx, |settings, cx| {
                                                settings.dismiss_moved(cx)
                                            });
                                        }))
                                        .child("Dismiss")
                                        .automation_target("settings/moved/dismiss"),
                                    ),
                            ),
                    )
                    .automation_target("settings/moved")
                    .into_any_element(),
            );
        }
        lines
    }

    /// The question before a move (DESIGN §4.13), over the scrim, while the service asks it.
    pub(super) fn move_confirmation(
        &self,
        window: &mut Window,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let Some(MoveState::Asking(plan)) = cx.services().settings.read(cx).move_state() else {
            return None;
        };
        let plan = plan.clone();
        let c = &theme.colors;
        let room = overlay::room(window.viewport_size(), theme);
        let title = "Move the archive to this folder?";
        let paragraph = |text: String| {
            div()
                .text_color(c.fg2)
                .text_spec(&theme.text.body)
                .child(text)
        };
        let free = match plan.free {
            Some(free) => format!("{} on {}", format::size(free), plan.volume.display()),
            None => "Not known".to_string(),
        };
        /* A path on a line of its own, cut in its middle: a path wraps anywhere, a Windows one
        straight after its drive. */
        let path_line = |id: &'static str, text: String, paths: &[PathBuf]| {
            div()
                .id(id)
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis_middle()
                .whitespace_nowrap()
                .child(text)
                .tooltip(paths_tooltip(id, paths))
        };
        let cancel = |cx: &mut gpui::App| {
            let settings = cx.services().settings.clone();
            settings.update(cx, |settings, cx| settings.cancel_move(cx));
        };
        let dialog = overlay::surface(theme)
            .id("settings-move-confirm")
            .role(Role::Dialog)
            .aria_label(title)
            .track_focus(&self.confirm_focus)
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
                        path_line(
                            "settings-move-confirm-folder",
                            plan.target.display().to_string(),
                            std::slice::from_ref(&plan.target),
                        )
                        .text_spec(&theme.text.path)
                        .text_color(c.fg)
                        .automation_target("settings/move-confirm/folder"),
                    )
                    .child(paragraph(format!(
                        "scrollback copies {} in {} there, reads each copy back to check it \
                         holds what was read, and checks every transcript against the \
                         catalogue. Syncing goes on \
                         while it copies; it pauses for the last moments, while the catalogue \
                         is copied and scrollback switches to the copy.",
                        format::size(plan.bytes),
                        format::plural(plan.files, "file")
                    )))
                    .child(paragraph(
                        "The archive stays where it is, as it is: nothing there is changed or \
                         deleted. Once you're satisfied, you can delete the old folder \
                         yourself."
                            .to_string(),
                    ))
                    .children(synced_warning(&plan.target, theme))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(theme.spacing.xs)
                            .child(kv(
                                "Needs",
                                div().child(format!("about {}", format::size(plan.needed))),
                                theme,
                            ))
                            .child(kv(
                                "Free",
                                path_line(
                                    "settings-move-confirm-free",
                                    free,
                                    std::slice::from_ref(&plan.volume),
                                ),
                                theme,
                            ))
                            .automation_target("settings/move-confirm/room"),
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
                            "settings-move-cancel",
                            ButtonKind::Plain,
                            false,
                            true,
                            theme,
                        )
                        .debug_selector(|| "settings-move-cancel".into())
                        .track_focus(&self.cancel_focus)
                        .on_click(move |_, _, cx| cancel(cx))
                        .child("Cancel")
                        .automation_target("settings/move-confirm/cancel"),
                    )
                    .child(
                        button("settings-move-ok", ButtonKind::Primary, false, true, theme)
                            .debug_selector(|| "settings-move-ok".into())
                            .on_click(|_, _, cx| {
                                let settings = cx.services().settings.clone();
                                settings.update(cx, |settings, cx| settings.confirm_move(cx));
                            })
                            .child("Move")
                            .automation_target("settings/move-confirm/ok"),
                    ),
            )
            .focus_trap("settings-move-confirm-trap", &self.confirm_focus)
            .automation_target("settings/move-confirm");
        Some(overlay::modal(
            dialog,
            Layer::Dialog,
            window,
            theme,
            move |_, cx| cancel(cx),
        ))
    }
}
