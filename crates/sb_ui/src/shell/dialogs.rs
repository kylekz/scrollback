//! The library's dialogs over its pages: Delete folder… (DESIGN §4.1), Delete for good…
//! (DESIGN §4.8) and the question leaving the paths editor with changes to save asks (DESIGN
//! §4.7). The library says when each is asked ([`LibraryService::deleting`],
//! [`LibraryService::for_good`], [`LibraryService::asked`]); the shell draws it beside the main
//! area, not inside it, so that the page's keys (`Delete`, `Enter`, `S`) never reach the page
//! behind it. While one is open it has the keyboard, which `Tab` keeps inside it (the overlay
//! contract, `crate::overlay`), and when it closes the keyboard goes back to where it was.
//!
//! **Delete for good** is the one act that can't be undone, so its `alertdialog` puts the
//! keyboard on Cancel, and `Enter` presses only the button that has it: Delete for good is
//! clicked, or reached with `Tab`.

use gpui::{
    AnyElement, App, ClickEvent, Context, FocusHandle, Focusable as _, FontWeight,
    InteractiveElement as _, IntoElement as _, ParentElement as _, Pixels, Role,
    StatefulInteractiveElement as _, Styled as _, StyledText, TextRun, Window, div, font,
    prelude::FluentBuilder as _, px,
};
use gpui_component::FocusTrapElement as _;
use sb_core::filing::FolderId;

use super::Shell;
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{ButtonKind, button, icon, location_tag};
use crate::format;
use crate::icons::SbIcon;
use crate::keymap;
use crate::overlay::{self, Layer};
use crate::services::ActiveServices as _;
use crate::services::ForGood;
use crate::services::library::title_of;
use crate::theme::{SbTheme, StyledText as _};

/// Which dialog is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Asking {
    Delete(FolderId),
    DeleteForGood,
    Save,
}

/// The dialog open, and where the keyboard was before it opened.
pub(super) struct Dialog {
    pub(super) asking: Asking,
    back: Option<FocusHandle>,
    /// Where the keyboard goes first in it: Delete for good's Cancel.
    pub(super) cancel: FocusHandle,
}

/// Delete for good's width (DESIGN §4.8), its tile and its title.
const FOR_GOOD_WIDTH: Pixels = px(540.);
const FOR_GOOD_TILE: Pixels = px(32.);
const FOR_GOOD_TITLE: Pixels = px(16.);

impl Shell {
    /**
     * Opens or closes the dialog the library asks for, the keyboard going to it, or back to
     * where it was. Called as the shell draws, so that what has the keyboard is drawn in the
     * same frame.
     */
    pub(super) fn follow_dialogs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let library = cx.services().library.read(cx);
        let asking = match (library.deleting(), library.for_good(), library.asked()) {
            (Some(folder), ..) => Some(Asking::Delete(folder)),
            (None, Some(_), _) => Some(Asking::DeleteForGood),
            (None, None, Some(_)) => Some(Asking::Save),
            (None, None, None) => None,
        };
        match (asking, &mut self.dialog) {
            (Some(asking), Some(open)) => open.asking = asking,
            (Some(asking), None) => {
                let (back, cancel) = (window.focused(cx), cx.focus_handle());
                match asking {
                    Asking::DeleteForGood => cancel.focus(window, cx),
                    Asking::Delete(_) | Asking::Save => self.dialog_focus.focus(window, cx),
                }
                self.dialog = Some(Dialog {
                    asking,
                    back,
                    cancel,
                });
            }
            (None, Some(_)) => {
                if let Some(Dialog { back, .. }) = self.dialog.take() {
                    back.unwrap_or_else(|| self.main.focus_handle(cx))
                        .focus(window, cx);
                }
            }
            (None, None) => {}
        }
    }

    /// Delete for good's Cancel, while its dialog is open.
    #[cfg(test)]
    pub(crate) fn for_good_cancel(&self) -> Option<&FocusHandle> {
        (self.dialog.as_ref())
            .filter(|dialog| dialog.asking == Asking::DeleteForGood)
            .map(|dialog| &dialog.cancel)
    }

    /// The dialog open, over the scrim.
    pub(super) fn dialog(
        &self,
        window: &Window,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let dialog = self.dialog.as_ref()?;
        let content = match dialog.asking {
            Asking::Delete(folder) => self.delete_folder(folder, theme, cx)?,
            Asking::DeleteForGood => self.delete_for_good(&dialog.cancel, theme, cx)?,
            Asking::Save => self.save_paths(theme, cx)?,
        };
        Some(overlay::modal(
            content,
            Layer::Dialog,
            window,
            theme,
            |_, cx| {
                let library = cx.services().library.clone();
                library.update(cx, |library, cx| {
                    library.answer_delete(false, cx);
                    library.answer_delete_for_good(false, cx);
                    library.answer(false, cx);
                });
            },
        ))
    }

    /// The surface of a dialog: it has the keyboard, and `Tab` stays inside it.
    fn surface(&self, label: &'static str, theme: &SbTheme) -> gpui::Stateful<gpui::Div> {
        overlay::surface(theme)
            .id(label)
            .role(Role::Dialog)
            .aria_label(label)
            .key_context(keymap::LIBRARY_DIALOG)
            .track_focus(&self.dialog_focus)
            .w(theme.sizes.dialog_width)
    }

    /**
     * Delete folder… (DESIGN §4.1): "Delete fc? Its 447 sessions go to Unsorted and are filed
     * again by the rules; its subfolders move up to its parent. Nothing is deleted from the
     * archive." with Cancel and Delete folder. `Enter` doesn't confirm.
     */
    fn delete_folder(
        &self,
        folder: FolderId,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let c = &theme.colors;
        let library = cx.services().library.read(cx);
        let folder = library.folder(folder)?;
        let counts = &library.counts().folders;
        let inside: usize = (library.folders().iter())
            .filter(|it| it.parent == Some(folder.id))
            .filter_map(|it| counts.get(&it.id))
            .sum();
        let own = counts
            .get(&folder.id)
            .copied()
            .unwrap_or(0)
            .saturating_sub(inside);
        let answer = |delete: bool| {
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                let library = cx.services().library.clone();
                library.update(cx, |library, cx| library.answer_delete(delete, cx));
            }
        };
        Some(
            self.surface("Delete folder", theme)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(theme.spacing.md)
                        .p(theme.sizes.dialog_padding)
                        .child(
                            div()
                                .flex()
                                .gap(theme.spacing.xs)
                                .text_spec(&theme.text.sheet_title)
                                .child("Delete")
                                .child(
                                    div()
                                        .flex()
                                        .child(div().italic().child(folder.name.clone()))
                                        .child("?"),
                                ),
                        )
                        .child(div().text_color(c.fg2).child(format!(
                            "Its {} go to Unsorted and are filed again by the rules; its \
                             subfolders move up to its parent. Nothing is deleted from the \
                             archive.",
                            format::plural(own, "session")
                        ))),
                )
                .child(
                    footer(theme)
                        .child(
                            button("delete-cancel", ButtonKind::Plain, false, true, theme)
                                .on_click(answer(false))
                                .child("Cancel")
                                .automation_target("delete-folder/cancel"),
                        )
                        .child(
                            button("delete-confirm", ButtonKind::Danger, false, true, theme)
                                .on_click(answer(true))
                                .child("Delete folder")
                                .automation_target("delete-folder/confirm"),
                        ),
                )
                .focus_trap("delete-folder-trap", &self.dialog_focus)
                .debug_selector(|| "delete-folder".into())
                .automation_target("delete-folder")
                .into_any_element(),
        )
    }

    /**
     * Delete for good… (DESIGN §4.8, `E-Trash`): the session's title, what deleting it
     * removes (its files, versions and size) and whether its location still has it, then its
     * project path, model, date and calls. Cancel has the keyboard first; Delete for good is
     * the app's only filled red, and waits until what it removes is read.
     */
    fn delete_for_good(
        &self,
        cancel: &FocusHandle,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let c = &theme.colors;
        let ForGood {
            session,
            deletion,
            refused,
        } = cx.services().library.read(cx).for_good()?.clone();
        let location = cx
            .services()
            .sync
            .read(cx)
            .location(&session.id.location)
            .cloned();
        let answer = |delete: bool| {
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                let library = cx.services().library.clone();
                library.update(cx, |library, cx| library.answer_delete_for_good(delete, cx));
            }
        };
        let removes = match &deletion {
            Some(deletion) => format!(
                "This removes the session from the archive: its {}, {} and {}. It can't be \
                 restored.",
                format::plural(deletion.files, "file"),
                format::plural(deletion.versions, "version"),
                format::fine_size(deletion.bytes)
            ),
            None => "Reading what it holds…".to_string(),
        };
        let tag = location
            .as_ref()
            .map(|location| location_tag(location.name.clone(), &location.kind, false, theme));
        let elsewhere = deletion.as_ref().map(|deletion| {
            let line = div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(theme.spacing.xs);
            if deletion.at_location {
                let rest = "Nothing is deleted there. Later syncs will skip it, so it won't come \
                            back; you can allow it again in Settings › Archive.";
                let bold = "Nothing is deleted there.".len();
                let run = |len, weight, color| TextRun {
                    len,
                    font: gpui::Font {
                        weight,
                        ..font(theme.fonts.ui.clone())
                    },
                    color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                line.child("The session still exists on")
                    .children(tag)
                    .child(".")
                    .child(StyledText::new(rest).with_runs(vec![
                        run(bold, FontWeight::SEMIBOLD, c.fg),
                        run(rest.len() - bold, FontWeight::NORMAL, c.fg2),
                    ]))
            } else {
                line.child("It is no longer on")
                    .children(tag)
                    .child("either, so this removes the last copy.")
            }
        });
        let summary = [
            session.project_path.clone().map(|path| format::path(&path)),
            agents::model(&session.id.provider, &session.models, cx),
            session
                .ended
                .or(session.started)
                .map(|at| format::day(at, format::now())),
            Some(format::plural(session.all.calls as usize, "call")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        let ready = deletion.is_some();
        Some(
            self.surface("Delete for good", theme)
                .role(Role::AlertDialog)
                .w(FOR_GOOD_WIDTH)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(theme.spacing.lg)
                        .pt(px(18.))
                        .pb(theme.spacing.xl)
                        .px(px(20.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(10.))
                                .child(
                                    div()
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .size(FOR_GOOD_TILE)
                                        .rounded(px(9.))
                                        .bg(c.red_soft)
                                        .child(icon(SbIcon::Trash, theme.sizes.icon, c.red)),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .text_size(FOR_GOOD_TITLE)
                                        .line_height(px(22.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(format!("Delete “{}” for good?", title_of(&session)))
                                        .automation_target("for-good/title"),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(theme.spacing.md)
                                .text_color(c.fg2)
                                .line_height(px(20.))
                                .child(div().child(removes).automation_target("for-good/removes"))
                                .children(
                                    elsewhere.map(|it| it.automation_target("for-good/elsewhere")),
                                )
                                .children(refused.map(|why| {
                                    div()
                                        .text_color(c.yellow)
                                        .child(why)
                                        .automation_target("for-good/refused")
                                })),
                        )
                        .child(
                            div()
                                .py(px(10.))
                                .px(theme.spacing.lg)
                                .rounded(theme.radii.row)
                                .border_1()
                                .border_color(c.line_subtle)
                                .bg(c.s1)
                                .text_color(c.fg3)
                                .text_spec(&theme.text.path_small)
                                .child(summary),
                        ),
                )
                .child(
                    footer(theme)
                        .child(
                            button("for-good-cancel", ButtonKind::Plain, false, true, theme)
                                .track_focus(cancel)
                                .on_click(answer(false))
                                .child("Cancel")
                                .automation_target("for-good/cancel"),
                        )
                        .child(
                            button(
                                "for-good-confirm",
                                ButtonKind::DangerFill,
                                false,
                                ready,
                                theme,
                            )
                            .when(ready, |this| this.on_click(answer(true)))
                            .child("Delete for good")
                            .automation_target("for-good/confirm"),
                        ),
                )
                .focus_trap("for-good-trap", &self.dialog_focus)
                .debug_selector(|| "for-good".into())
                .automation_target("for-good")
                .into_any_element(),
        )
    }

    /**
     * The question leaving with changes to save asks (DESIGN §4.7): "Save the path changes to
     * fc?", with Don't save, Cancel and Save.
     */
    fn save_paths(&self, theme: &SbTheme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let c = &theme.colors;
        let library = cx.services().library.read(cx);
        let name = library.folder(library.held()?)?.name.clone();
        let sessions = self.main.read(cx).sessions().clone();
        let answer = |save: Option<bool>| {
            let sessions = sessions.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                sessions.update(cx, |sessions, cx| sessions.answer(save, window, cx));
            }
        };
        Some(
            self.surface("Save the path changes", theme)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(theme.spacing.md)
                        .p(theme.sizes.dialog_padding)
                        .child(
                            div()
                                .text_spec(&theme.text.sheet_title)
                                .child(format!("Save the path changes to {name}?")),
                        )
                        .child(
                            div()
                                .text_color(c.fg2)
                                .child("Leaving the page without saving drops them."),
                        ),
                )
                .child(
                    footer(theme)
                        .child(
                            button("ask-discard", ButtonKind::Plain, false, true, theme)
                                .on_click(answer(Some(false)))
                                .child("Don't save")
                                .automation_target("ask/discard"),
                        )
                        .child(
                            button("ask-cancel", ButtonKind::Plain, false, true, theme)
                                .on_click(answer(None))
                                .child("Cancel")
                                .automation_target("ask/cancel"),
                        )
                        .child(
                            button("ask-save", ButtonKind::Primary, false, true, theme)
                                .on_click(answer(Some(true)))
                                .child("Save")
                                .automation_target("ask/save"),
                        ),
                )
                .focus_trap("ask-trap", &self.dialog_focus)
                .automation_target("ask")
                .into_any_element(),
        )
    }
}

/// A dialog's buttons, at the right of its foot.
fn footer(theme: &SbTheme) -> gpui::Div {
    let c = &theme.colors;
    div()
        .flex()
        .justify_end()
        .gap(theme.spacing.md)
        .p(theme.sizes.dialog_padding)
        .border_t_1()
        .border_color(c.line_subtle)
        .bg(c.s1)
}
