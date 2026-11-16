//! Settings › Archive (DESIGN §4.13, `E-Settings-Archive`): the archive's folder with Show in
//! Explorer or Finder and Use a different folder…, its size, Verify with the last result and
//! what can be done for each problem, and the sessions deleted for good.
//!
//! **Use a different folder…** opens the folder picker; a folder that holds anything but an
//! archive is refused at once, and any other asks to confirm, saying whether its archive opens
//! or a new one is made there. Confirmed, the app moves onto it (`SettingsService::use_folder`).

use gpui::{
    AnyElement, AppContext as _, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    Role, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::FocusTrapElement as _;
use sb_core::archive::{self, Holds, Problem};
use sb_core::catalogue::Tombstone;

use super::{Chosen, SettingsPage, section_head};
use crate::automation::AutomationTargetExt as _;
use crate::components::{
    ButtonKind, Tone, button, card, chip, icon, info_banner, kv, location_tag, middle_cut,
    progress_bar, tag, with_numbers,
};
use crate::format;
use crate::icons::SbIcon;
use crate::overlay::{self, Layer};
use crate::services::ActiveServices as _;
use crate::services::settings::Switch;
use crate::services::sync::Refetch;
use crate::services::upkeep::{Deleted, Fix};
use crate::system;
use crate::theme::{SbTheme, StyledText as _};

/// What a stored file's problem is, in the owner's words.
fn what_is_wrong(problem: &Problem) -> &'static str {
    match problem {
        Problem::SegmentMissing { .. } => "The file is missing",
        Problem::SegmentUnreadable { .. } => "The file can't be decompressed",
        Problem::SegmentChanged { .. } => "Hash doesn't match the catalogue",
        Problem::Gap { .. } => "Part of the version has no file",
        Problem::VersionChanged => "The version doesn't match the catalogue",
    }
}

/**
 * A stored file as a problem's row names it (`E-Settings-Archive`): under `files/`, without the
 * location's and the agent's folders, which the row's session names, and the item's folder cut
 * to its start: `files/2f/2f91c0…/v0001/0000000000184467.zst`.
 */
fn shown_file(file: &std::path::Path) -> String {
    let parts: Vec<String> = file
        .iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect();
    match parts.as_slice() {
        [files, _location, _agent, shard, item, rest @ ..] => {
            let item: String = item.chars().take(6).collect();
            let mut shown = vec![files.clone(), shard.clone(), format!("{item}\u{2026}")];
            shown.extend(rest.iter().cloned());
            shown.join("/")
        }
        _ => parts.join("/"),
    }
}

/// `3 min 12 s`, `41 s`, `under a second`.
fn took(ms: u64) -> String {
    match ms / 1000 {
        0 => "under a second".to_string(),
        seconds @ 1..60 => format!("{seconds} s"),
        seconds => format!("{} min {} s", seconds / 60, seconds % 60),
    }
}

impl SettingsPage {
    pub(super) fn archive_section(&self, theme: &SbTheme, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(theme.spacing.lg)
            .child(section_head(
                "Archive",
                Some("Where scrollback keeps its copies, how big they are, and whether they are intact."),
                theme,
            ))
            .child(self.folder_card(theme, cx))
            .children(self.stats(theme, cx))
            .child(self.verify(theme, cx))
            .child(self.deleted_for_good(theme, cx))
            .into_any_element()
    }

    /// The folder, with Show in Explorer or Finder and Use a different folder… (decision 4).
    fn folder_card(&self, theme: &SbTheme, cx: &mut Context<Self>) -> AnyElement {
        let c = &theme.colors;
        let services = cx.services();
        let dir = services.archive.read(cx).dir().to_path_buf();
        let settings = services.settings.read(cx);
        let default = settings.is_default(&dir);
        let switch = settings.switch().cloned();
        let opening = matches!(switch, Some(Switch::Opening(_)));
        card(theme)
            .gap(theme.spacing.md)
            .py(theme.spacing.lg)
            .px(px(14.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.md)
                    .child(icon(SbIcon::Archive, theme.sizes.icon_sm, c.fg3))
                    .child(
                        div()
                            .min_w_0()
                            .text_spec(&theme.text.path)
                            .text_color(c.fg)
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(middle_cut(&dir.display().to_string(), 96))
                            .automation_target("settings/archive/folder"),
                    )
                    .when(default, |this| {
                        this.child(
                            tag("default", Tone::Neutral, false, theme)
                                .automation_target("settings/archive/default"),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.md)
                    .child(
                        button("settings-reveal", ButtonKind::Plain, true, true, theme)
                            .debug_selector(|| "settings-reveal".into())
                            .on_click(cx.listener(move |_, _, _, cx| system::reveal(&dir, cx)))
                            .child(icon(SbIcon::ExternalLink, theme.sizes.icon_xs, c.fg2))
                            .child(system::REVEAL_LABEL)
                            .automation_target("settings/archive/reveal"),
                    )
                    .child(
                        button(
                            "settings-use-folder",
                            ButtonKind::Plain,
                            true,
                            !opening,
                            theme,
                        )
                        .debug_selector(|| "settings-use-folder".into())
                        .when(!opening, |this| {
                            this.on_click(cx.listener(|this, _, _, cx| this.choose_folder(cx)))
                        })
                        .child(icon(SbIcon::Folder, theme.sizes.icon_xs, c.fg2))
                        .child("Use a different folder…")
                        .automation_target("settings/archive/use-folder"),
                    ),
            )
            .child(
                div()
                    .text_spec(&theme.text.secondary)
                    .text_color(c.fg3)
                    .child(
                        "An archive already in that folder opens; an empty folder gets a new \
                         one. This archive stays where it is. The setting lives with the app, \
                         not in the archive.",
                    ),
            )
            .children(switch.map(|switch| {
                match switch {
                    Switch::Opening(folder) => div()
                        .text_spec(&theme.text.secondary)
                        .text_color(c.accent_text)
                        .child(format!("Opening {}…", folder.display()))
                        .automation_target("settings/archive/opening"),
                    Switch::Refused { why, .. } => div()
                        .flex()
                        .items_start()
                        .gap(theme.spacing.md)
                        .text_spec(&theme.text.secondary)
                        .text_color(c.red)
                        .child(div().flex_1().min_w_0().child(why))
                        .child(
                            button(
                                "settings-refused-dismiss",
                                ButtonKind::Ghost,
                                true,
                                true,
                                theme,
                            )
                            .on_click(cx.listener(|_, _, _, cx| {
                                let settings = cx.services().settings.clone();
                                settings.update(cx, |settings, cx| settings.dismiss_switch(cx));
                            }))
                            .child("Dismiss"),
                        )
                        .automation_target("settings/archive/refused"),
                }
            }))
            .into_any_element()
    }

    /// Asks for another folder, and what it holds: one that holds anything but an archive is
    /// refused at once; any other waits for the owner to confirm.
    fn choose_folder(&mut self, cx: &mut Context<Self>) {
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
            this.update(cx, |this, cx| match looked {
                Ok(holds @ (Holds::Archive | Holds::Nothing)) => {
                    this.chosen = Some(Chosen { folder, holds });
                    cx.notify();
                }
                /* What can't be made an archive's goes the way a refusal does. */
                _ => {
                    let settings = cx.services().settings.clone();
                    settings.update(cx, |settings, cx| settings.use_folder(folder, cx));
                }
            })
            .ok();
        })
        .detach();
    }

    /// The confirmation of the folder chosen (DESIGN §4.13: "choosing a folder asks to confirm
    /// and reopens the window on the other archive"), over the scrim.
    pub(super) fn confirmation(
        &self,
        window: &mut Window,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let chosen = self.chosen.clone()?;
        let c = &theme.colors;
        let room = overlay::room(window.viewport_size(), theme);
        let current = cx.services().archive.read(cx).dir().display().to_string();
        let folder = chosen.folder.display().to_string();
        let (title, what, confirm) = match chosen.holds {
            Holds::Archive => (
                "Open the archive in this folder?",
                format!(
                    "scrollback reopens on the archive in {folder}, with its own locations, \
                     folders and settings."
                ),
                "Open it",
            ),
            _ => (
                "Make a new archive in this folder?",
                format!(
                    "{folder} is empty. scrollback makes a new archive there and reopens on \
                     it; the first run asks for its locations."
                ),
                "Make it",
            ),
        };
        let weak = cx.entity().downgrade();
        let dialog = overlay::surface(theme)
            .id("settings-confirm")
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
                        div()
                            .text_color(c.fg2)
                            .text_spec(&theme.text.body)
                            .child(what),
                    )
                    .child(
                        div()
                            .text_color(c.fg2)
                            .text_spec(&theme.text.body)
                            .child(format!(
                                "The archive in {current} stays where it is, as it is. Nothing is \
                         moved, and you can come back to it the same way."
                            )),
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
                            "settings-confirm-cancel",
                            ButtonKind::Plain,
                            false,
                            true,
                            theme,
                        )
                        .debug_selector(|| "settings-confirm-cancel".into())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.chosen = None;
                            cx.notify();
                        }))
                        .child("Cancel")
                        .automation_target("settings/confirm/cancel"),
                    )
                    .child(
                        button(
                            "settings-confirm-ok",
                            ButtonKind::Primary,
                            false,
                            true,
                            theme,
                        )
                        .debug_selector(|| "settings-confirm-ok".into())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(chosen) = this.chosen.take() {
                                let settings = cx.services().settings.clone();
                                settings.update(cx, |settings, cx| {
                                    settings.use_folder(chosen.folder, cx);
                                });
                            }
                            cx.notify();
                        }))
                        .child(confirm)
                        .automation_target("settings/confirm/ok"),
                    ),
            )
            .focus_trap("settings-confirm-trap", &self.confirm_focus)
            .automation_target("settings/confirm");
        Some(overlay::modal(
            dialog,
            Layer::Dialog,
            window,
            theme,
            move |_, cx| {
                weak.update(cx, |this, cx| {
                    this.chosen = None;
                    cx.notify();
                })
                .ok();
            },
        ))
    }

    /// How big the archive is: four numbers, then where the bytes are (`kv` rows).
    fn stats(&self, theme: &SbTheme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let c = &theme.colors;
        let stats = *cx.services().upkeep.read(cx).stats()?;
        let stat = |value: String, label: &'static str| {
            div()
                .flex()
                .flex_col()
                .child(div().text_spec(&theme.text.stat).child(value))
                .child(
                    div()
                        .text_spec(&theme.text.secondary)
                        .text_color(c.fg3)
                        .child(label),
                )
        };
        let totals = stats.totals;
        let backups = if stats.backups == 0 {
            "None yet".to_string()
        } else {
            format!(
                "{} · {} checked {} of the catalogue",
                format::size(stats.backups_bytes),
                stats.backups,
                if stats.backups == 1 {
                    "backup"
                } else {
                    "backups"
                }
            )
        };
        let row = |key: &'static str, value: String| {
            kv(
                key,
                with_numbers(value, c.fg2, &theme.text.secondary, theme),
                theme,
            )
        };
        Some(
            div()
                .flex()
                .flex_col()
                .gap(theme.spacing.lg)
                .child(
                    div()
                        .flex()
                        .gap(px(28.))
                        .child(stat(format::size(stats.on_disk), "on disk"))
                        .child(stat(format::count(totals.sessions as u64), "sessions"))
                        .child(stat(format::count(totals.items as u64), "items"))
                        .child(stat(format::count(totals.versions as u64), "versions"))
                        .automation_target("settings/archive/stats"),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(theme.spacing.xs)
                        .child(row(
                            "Transcripts",
                            format!(
                                "{}, compressed from {}",
                                format::size(stats.stored),
                                format::size(totals.bytes)
                            ),
                        ))
                        .child(row("Catalogue and index", format::size(stats.databases)))
                        .child(row("Backups", backups)),
                )
                .into_any_element(),
        )
    }

    /// Verify (PLAN §4.4): Verify now, or its progress and Stop while it runs; the last
    /// result, with each problem's row.
    fn verify(&self, theme: &SbTheme, cx: &mut Context<Self>) -> AnyElement {
        let c = &theme.colors;
        let upkeep = cx.services().upkeep.read(cx);
        let running = upkeep.verifying().map(|verifying| verifying.progress);
        let failure = upkeep.failure().map(str::to_string);
        div()
            .flex()
            .flex_col()
            .gap(theme.spacing.md)
            .pt(theme.spacing.lg)
            .border_t_1()
            .border_color(c.line_subtle)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.md)
                    .child(
                        div()
                            .flex_1()
                            .text_spec(&theme.text.control)
                            .child("Verify"),
                    )
                    .child(match running {
                        Some(_) => {
                            button("settings-verify-stop", ButtonKind::Plain, true, true, theme)
                                .debug_selector(|| "settings-verify-stop".into())
                                .on_click(cx.listener(|_, _, _, cx| {
                                    let upkeep = cx.services().upkeep.clone();
                                    upkeep.update(cx, |upkeep, _| upkeep.stop());
                                }))
                                .child("Stop")
                                .automation_target("settings/verify/stop")
                        }
                        None => button("settings-verify-run", ButtonKind::Plain, true, true, theme)
                            .debug_selector(|| "settings-verify-run".into())
                            .on_click(cx.listener(|_, _, _, cx| {
                                let upkeep = cx.services().upkeep.clone();
                                upkeep.update(cx, |upkeep, cx| upkeep.verify(cx));
                            }))
                            .child(icon(SbIcon::ShieldCheck, theme.sizes.icon_xs, c.fg2))
                            .child("Verify now")
                            .automation_target("settings/verify/run"),
                    }),
            )
            .child(
                div()
                    .text_spec(&theme.text.secondary)
                    .text_color(c.fg3)
                    .child(
                        "Re-hashes every stored file against the catalogue and reports what \
                         doesn't match. It repairs nothing.",
                    ),
            )
            .children(running.map(|progress| {
                let fraction = if progress.segments == 0 {
                    0.
                } else {
                    progress.segments_done as f32 / progress.segments as f32
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(theme.spacing.sm)
                    .child(progress_bar(fraction, theme))
                    .child(with_numbers(
                        format!(
                            "Verifying · {} of {} segments",
                            format::count(progress.segments_done as u64),
                            format::count(progress.segments as u64)
                        ),
                        c.accent_text,
                        &theme.text.secondary,
                        theme,
                    ))
                    .automation_target("settings/verify/progress")
            }))
            .children(failure.map(|why| {
                div()
                    .text_spec(&theme.text.secondary)
                    .text_color(c.red)
                    .child(why)
            }))
            .children(self.last_result(theme, cx))
            .into_any_element()
    }

    /// The last verify's result in a card: its header, then one row per problem.
    fn last_result(&self, theme: &SbTheme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let c = &theme.colors;
        let upkeep = cx.services().upkeep.read(cx);
        let last = upkeep.last()?.clone();
        let problems = upkeep.problems().to_vec();
        let now = format::now();
        let count = problems.len();
        let (glyph, color, says) = if count == 0 {
            (SbIcon::CircleCheck, c.green, "No problems".to_string())
        } else {
            (
                SbIcon::TriangleAlert,
                c.yellow,
                format::plural(count, "problem"),
            )
        };
        let notes: Vec<String> = [
            (last.unread > 0).then(|| {
                format!(
                    "{} couldn't be read to the end for a reason that isn't its bytes; it isn't \
                     counted as damaged.",
                    format::plural(last.unread, "version")
                )
            }),
            (last.uncatalogued > 0).then(|| {
                format!(
                    "{} under files/ that the catalogue doesn't record.",
                    format::plural(last.uncatalogued, "path")
                )
            }),
        ]
        .into_iter()
        .flatten()
        .collect();
        Some(
            card(theme)
                .overflow_hidden()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(theme.spacing.md)
                        .py(theme.spacing.md)
                        .px(theme.spacing.lg)
                        .text_spec(&theme.text.secondary)
                        .when(count > 0 || !notes.is_empty(), |this| {
                            this.border_b_1().border_color(c.line_subtle)
                        })
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(theme.spacing.sm)
                                .text_color(color)
                                .font_weight(theme.text.control.weight)
                                .child(icon(glyph, theme.sizes.icon_sm, color))
                                .child(says),
                        )
                        .child(with_numbers(
                            format!(
                                "Last run {}, {} · {} segments · {}",
                                format::day(last.at, now),
                                format::clock(last.at),
                                format::count(last.segments as u64),
                                took(last.took_ms)
                            ),
                            c.fg3,
                            &theme.text.secondary,
                            theme,
                        ))
                        .automation_target("settings/verify/result"),
                )
                .children(
                    problems
                        .iter()
                        .enumerate()
                        .map(|(at, problem)| self.problem_row(at, problem, theme, cx)),
                )
                .children(notes.into_iter().map(|note| {
                    div()
                        .py(theme.spacing.md)
                        .px(theme.spacing.lg)
                        .border_t_1()
                        .border_color(c.line_subtle)
                        .text_spec(&theme.text.secondary)
                        .text_color(c.fg3)
                        .child(note)
                }))
                .into_any_element(),
        )
    }

    /// One problem: the stored file, what is wrong and the session it is in; then Fetch again
    /// from its location, or the damaged chip where its location no longer has it; and Open
    /// session (decision 5).
    fn problem_row(
        &self,
        at: usize,
        problem: &crate::services::upkeep::Problem,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let sync = cx.services().sync.read(cx);
        let refetch = sync.refetch_of(&problem.damage).cloned();
        let wrong = problem
            .damage
            .problems
            .first()
            .map_or("Damaged", what_is_wrong);
        let session = problem.session.as_ref().map(|session| {
            format!(
                " · in “{}” ({}{})",
                session.title.as_deref().unwrap_or("Untitled"),
                problem.folder.as_deref().unwrap_or("Unsorted"),
                problem
                    .location
                    .as_ref()
                    .map(|location| format!(", {}", location.name))
                    .unwrap_or_default()
            )
        });
        let target = move |what: &str| -> SharedString {
            format!("settings/verify/problem/{at}/{what}").into()
        };
        let file = shown_file(&problem.damage.file());
        let fix: AnyElement = match (problem.fix, refetch) {
            (Fix::FetchAgain, Some(Refetch::Running)) => div()
                .text_spec(&theme.text.secondary)
                .text_color(c.accent_text)
                .child("Fetching…")
                .into_any_element(),
            (Fix::FetchAgain, refetched) => {
                let damage = problem.damage.clone();
                div()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.md)
                    .children(match refetched {
                        Some(Refetch::Failed(why)) => Some(
                            div()
                                .max_w(px(220.))
                                .text_spec(&theme.text.small)
                                .text_color(c.red)
                                .child(why),
                        ),
                        _ => None,
                    })
                    .child(
                        button(
                            SharedString::from(format!("settings-fetch-{at}")),
                            ButtonKind::Plain,
                            true,
                            true,
                            theme,
                        )
                        .on_click(cx.listener(move |_, _, _, cx| {
                            let sync = cx.services().sync.clone();
                            sync.update(cx, |sync, cx| sync.refetch(&damage, cx));
                        }))
                        .child("Fetch again from its location")
                        .automation_target(target("fetch")),
                    )
                    .into_any_element()
            }
            (Fix::Fetched(version), _) => div()
                .text_spec(&theme.text.secondary)
                .text_color(c.green)
                .child(format!("Fetched again as v{version}"))
                .automation_target(target("fetched")),
            (Fix::Damaged, _) => div()
                .flex()
                .items_center()
                .gap(theme.spacing.md)
                .child(
                    div()
                        .text_spec(&theme.text.small)
                        .text_color(c.fg3)
                        .child("Its location no longer has it"),
                )
                .child(chip("damaged", Tone::Bad, theme).automation_target(target("damaged")))
                .into_any_element(),
            (Fix::Deleted, _) => div()
                .text_spec(&theme.text.small)
                .text_color(c.fg3)
                .child("Deleted for good since")
                .into_any_element(),
        };
        let open = problem.session.clone().map(|session| {
            button(
                SharedString::from(format!("settings-open-{at}")),
                ButtonKind::Plain,
                true,
                true,
                theme,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.back(cx);
                let library = cx.services().library.clone();
                library.update(cx, |library, cx| library.open(session.clone(), cx));
            }))
            .child("Open session")
            .automation_target(target("open"))
        });
        div()
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .py(theme.spacing.md)
            .px(theme.spacing.lg)
            .when(at > 0, |this| this.border_t_1().border_color(c.line_subtle))
            .text_spec(&theme.text.secondary)
            .child(icon(SbIcon::FileX, theme.sizes.icon_sm, c.fg3))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_spec(&theme.text.path_small)
                            .text_color(c.fg2)
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(file),
                    )
                    .child(
                        div()
                            .text_color(c.fg3)
                            .child(format!("{wrong}{}", session.unwrap_or_default())),
                    ),
            )
            .child(fix)
            .children(open)
            .automation_target(target("row"))
    }

    /// Deleted for good (PLAN §4.5): one row per tombstone with Allow archiving again, which
    /// then reads as allowed, with Undo.
    fn deleted_for_good(&self, theme: &SbTheme, cx: &mut Context<Self>) -> AnyElement {
        let c = &theme.colors;
        let deleted = cx.services().upkeep.read(cx).deleted().to_vec();
        let now = format::now();
        div()
            .flex()
            .flex_col()
            .gap(theme.spacing.md)
            .pt(theme.spacing.lg)
            .border_t_1()
            .border_color(c.line_subtle)
            .child(
                div()
                    .text_spec(&theme.text.control)
                    .child(if deleted.is_empty() {
                        "Deleted for good".to_string()
                    } else {
                        format!("Deleted for good · {}", deleted.len())
                    }),
            )
            .child(
                div()
                    .text_spec(&theme.text.secondary)
                    .text_color(c.fg3)
                    .child(
                        "Syncs skip these sessions even while their location still has them. \
                         Allowing one lets the next sync archive it again, if it is still there.",
                    ),
            )
            .child(if deleted.is_empty() {
                info_banner(theme)
                    .child(div().flex_1().child("No sessions deleted for good."))
                    .automation_target("settings/deleted/none")
            } else {
                div()
                    .flex()
                    .flex_col()
                    .rounded(theme.radii.row)
                    .border_1()
                    .border_color(c.line_subtle)
                    .overflow_hidden()
                    .children(
                        deleted
                            .iter()
                            .enumerate()
                            .map(|(at, row)| deleted_row(at, row, now, theme, cx)),
                    )
                    .automation_target("settings/deleted")
            })
            .into_any_element()
    }
}

/// What names a session deleted for good: where it ran, since its tombstone keeps nothing of
/// what was said in it (PLAN §4.3), not even its title.
fn named(tombstone: &Tombstone) -> String {
    tombstone.project_path.as_deref().map_or_else(
        || "A session with no project path".to_string(),
        format::path,
    )
}

/// One session deleted for good (`E-Settings-Archive`: a 44px row).
fn deleted_row(
    at: usize,
    row: &Deleted,
    now: i64,
    theme: &SbTheme,
    cx: &mut Context<SettingsPage>,
) -> AnyElement {
    let c = &theme.colors;
    let tombstone = &row.tombstone;
    let id = tombstone.session.clone();
    let location = row.location.as_ref();
    let ran = tombstone
        .started
        .map(|started| format!("ran {} · ", format::day(started, now)))
        .unwrap_or_default();
    let target =
        move |what: &str| -> SharedString { format!("settings/deleted/{at}/{what}").into() };
    let (line, action): (String, AnyElement) = match &row.allowed {
        Some(_) => (
            format!(
                "Allowed · archived again on the next sync of {}",
                location.map_or("its location", |it| it.name.as_str())
            ),
            button(
                SharedString::from(format!("settings-undo-{at}")),
                ButtonKind::Plain,
                true,
                true,
                theme,
            )
            .on_click(cx.listener(move |_, _, _, cx| {
                let upkeep = cx.services().upkeep.clone();
                upkeep.update(cx, |upkeep, cx| upkeep.undo_allow(&id, cx));
            }))
            .child("Undo")
            .automation_target(target("undo")),
        ),
        None => (
            format!(
                "{ran}deleted for good {}",
                format::day(tombstone.deleted, now)
            ),
            button(
                SharedString::from(format!("settings-allow-{at}")),
                ButtonKind::Plain,
                true,
                true,
                theme,
            )
            .on_click(cx.listener(move |_, _, _, cx| {
                let upkeep = cx.services().upkeep.clone();
                upkeep.update(cx, |upkeep, cx| upkeep.allow(&id, cx));
            }))
            .child("Allow archiving again")
            .automation_target(target("allow")),
        ),
    };
    div()
        .flex()
        .items_center()
        .gap(theme.spacing.lg)
        .min_h(px(44.))
        .py(theme.spacing.sm)
        .px(theme.spacing.lg)
        .when(at > 0, |this| this.border_t_1().border_color(c.line_subtle))
        .child(icon(SbIcon::Ban, theme.sizes.icon_sm, c.fg3))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_spec(&theme.text.path)
                        .text_color(c.fg)
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(named(tombstone)),
                )
                .child(
                    div()
                        .text_spec(&theme.text.small)
                        .text_color(if row.allowed.is_some() {
                            c.accent_text
                        } else {
                            c.fg3
                        })
                        .child(line),
                )
                .children(row.refused.clone().map(|why| {
                    div()
                        .text_spec(&theme.text.small)
                        .text_color(c.red)
                        .child(why)
                })),
        )
        .children(location.map(|it| location_tag(it.name.clone(), &it.kind, false, theme)))
        .child(action)
        .automation_target(target("row"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_file_is_named_as_the_board_does() {
        let file: std::path::PathBuf = [
            "files",
            "6f0c2b8e",
            "agent",
            "2f",
            "2f91c0aa55",
            "v0001",
            "0000000000184467.zst",
        ]
        .iter()
        .collect();
        assert_eq!(
            shown_file(&file),
            "files/2f/2f91c0\u{2026}/v0001/0000000000184467.zst"
        );
    }
}
