//! The library's dialogs over its pages: Delete folder… (DESIGN §4.1) and the question leaving
//! the paths editor with changes to save asks (DESIGN §4.7). The library says when each is
//! asked ([`LibraryService::deleting`], [`LibraryService::asked`]); the shell draws it beside
//! the main area, not inside it, so that the page's keys (`Delete`, `Enter`, `S`) never reach
//! the page behind it. While one is open it has the keyboard, which `Tab` keeps inside it
//! (the overlay contract, `crate::overlay`), and when it closes the keyboard goes back to
//! where it was.

use gpui::{
    AnyElement, App, ClickEvent, Context, FocusHandle, Focusable as _, InteractiveElement as _,
    IntoElement as _, ParentElement as _, Role, StatefulInteractiveElement as _, Styled as _,
    Window, div,
};
use gpui_component::FocusTrapElement as _;
use sb_core::filing::FolderId;

use super::Shell;
use crate::automation::AutomationTargetExt as _;
use crate::components::{ButtonKind, button};
use crate::format;
use crate::keymap;
use crate::overlay::{self, Layer};
use crate::services::ActiveServices as _;
use crate::theme::{SbTheme, StyledText as _};

/// Which dialog is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Asking {
    Delete(FolderId),
    Save,
}

/// The dialog open, and where the keyboard was before it opened.
pub(super) struct Dialog {
    pub(super) asking: Asking,
    back: Option<FocusHandle>,
}

impl Shell {
    /**
     * Opens or closes the dialog the library asks for, the keyboard going to it, or back to
     * where it was. Called as the shell draws, so that what has the keyboard is drawn in the
     * same frame.
     */
    pub(super) fn follow_dialogs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let library = cx.services().library.read(cx);
        let asking = match (library.deleting(), library.asked()) {
            (Some(folder), _) => Some(Asking::Delete(folder)),
            (None, Some(_)) => Some(Asking::Save),
            (None, None) => None,
        };
        match (asking, &mut self.dialog) {
            (Some(asking), Some(open)) => open.asking = asking,
            (Some(asking), None) => {
                self.dialog = Some(Dialog {
                    asking,
                    back: window.focused(cx),
                });
                self.dialog_focus.focus(window, cx);
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

    /// The dialog open, over the scrim.
    pub(super) fn dialog(
        &self,
        window: &Window,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let content = match self.dialog.as_ref()?.asking {
            Asking::Delete(folder) => self.delete_folder(folder, theme, cx)?,
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
