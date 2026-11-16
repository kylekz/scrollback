//! The sheet and its add dialog in a window (DESIGN §4.11, §4.12): every way of closing them
//! cancels the connection test they started, and the sheet's name field renames on blur and
//! on close, and gives the name back on `Esc`.

use gpui::{Modifiers, MouseButton, TestAppContext, VisualTestContext, point, px, size};
use pretty_assertions::assert_eq;

use super::*;
use crate::services::sync::TestState;
use crate::shell::Shell;
use crate::test_support::{bounds, init_answering, open_window};

/// A point on the scrim, below the sheet and beside the dialog, in a 1440x900 window.
const SCRIM: gpui::Point<gpui::Pixels> = point(px(40.), px(880.));

/**
 * A 1440x900 window over this machine, added and synced, with the locations sheet open on
 * it; the shell and the sheet.
 */
fn open_sheet(
    cx: &mut TestAppContext,
) -> (
    Entity<Shell>,
    Entity<LocationsSheet>,
    &mut VisualTestContext,
) {
    init_answering(cx);
    cx.update(|cx| {
        let sync = cx.services().sync.clone();
        sync.update(cx, |sync, cx| sync.add_local(cx));
    });
    cx.run_until_parked();
    let (shell, cx) = open_window(size(px(1440.), px(900.)), cx);
    let here = cx.update(|_, cx| {
        cx.services().sync.read(cx).locations().unwrap()[0]
            .id
            .clone()
    });
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.open_locations(Some(here), window, cx));
    });
    cx.run_until_parked();
    let sheet = cx.update(|_, cx| shell.read(cx).locations_sheet().cloned().unwrap());
    (shell, sheet, cx)
}

fn sync(cx: &mut VisualTestContext) -> Entity<SyncService> {
    cx.update(|_, cx| cx.services().sync.clone())
}

fn click(cx: &mut VisualTestContext, at: gpui::Point<gpui::Pixels>) {
    cx.simulate_click(at, Modifiers::none());
    cx.run_until_parked();
}

/**
 * Opens the add dialog on an SSH host with `user@macbook` typed, and starts its test, which
 * is running until the executor next runs.
 */
fn dialog_testing(sheet: &Entity<LocationsSheet>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        sheet.update(cx, |sheet, cx| sheet.open_add(Kind::Ssh, None, window, cx));
    });
    cx.simulate_input("user@macbook");
    let host = LocationKind::Ssh {
        host: "user@macbook".to_string(),
    };
    sync(cx).update(cx, |sync, cx| sync.test_place(host, cx));
    assert!(running(cx));
}

fn running(cx: &mut VisualTestContext) -> bool {
    sync(cx).read_with(cx, |sync, _| {
        matches!(
            sync.test().map(|test| &test.state),
            Some(TestState::Running(_))
        )
    })
}

fn dialog(sheet: &Entity<LocationsSheet>, cx: &mut VisualTestContext) -> Option<&'static str> {
    cx.update(|_, cx| sheet.read(cx).dialog())
}

#[gpui::test]
fn esc_in_the_add_dialog_closes_it_and_cancels_its_test(cx: &mut TestAppContext) {
    let (_, sheet, cx) = open_sheet(cx);
    dialog_testing(&sheet, cx);
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(dialog(&sheet, cx), None);
    assert!(sync(cx).read_with(cx, |sync, _| sync.test().is_none()));
}

#[gpui::test]
fn cancel_in_the_add_dialog_closes_it_and_cancels_its_test(cx: &mut TestAppContext) {
    let (_, sheet, cx) = open_sheet(cx);
    dialog_testing(&sheet, cx);
    let cancel = bounds(cx, "add-cancel").expect("the dialog draws Cancel");
    // The frame drawn to find it ran the executor: the test is started again.
    sync(cx).update(cx, |sync, cx| {
        let host = LocationKind::Ssh {
            host: "user@macbook".to_string(),
        };
        sync.test_place(host, cx);
    });
    cx.simulate_mouse_down(cancel.center(), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(cancel.center(), MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    assert_eq!(dialog(&sheet, cx), None);
    assert!(sync(cx).read_with(cx, |sync, _| sync.test().is_none()));
}

#[gpui::test]
fn a_press_beside_the_add_dialog_leaves_it_while_its_test_runs(cx: &mut TestAppContext) {
    let (_, sheet, cx) = open_sheet(cx);
    dialog_testing(&sheet, cx);
    click(cx, SCRIM);
    assert_eq!(dialog(&sheet, cx), Some("add"));
    assert!(sync(cx).read_with(cx, |sync, _| sync.test().is_some()));
}

#[gpui::test]
fn closing_the_sheet_cancels_the_test_it_started(cx: &mut TestAppContext) {
    let (shell, _, cx) = open_sheet(cx);
    sync(cx).update(cx, |sync, cx| sync.test_place(LocationKind::Local, cx));
    assert!(running(cx));
    click(cx, SCRIM);
    assert!(cx.update(|_, cx| shell.read(cx).locations_sheet().is_none()));
    assert!(sync(cx).read_with(cx, |sync, _| sync.test().is_none()));
}

/// Puts the keyboard in the sheet's name field, and types `typed` after the name there.
fn type_name(sheet: &Entity<LocationsSheet>, typed: &str, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        let name = sheet.read(cx).name.clone();
        name.update(cx, |input, cx| input.focus(window, cx));
    });
    cx.simulate_input(typed);
    cx.run_until_parked();
}

fn name(cx: &mut VisualTestContext) -> String {
    sync(cx).read_with(cx, |sync, _| sync.locations().unwrap()[0].name.clone())
}

fn field(sheet: &Entity<LocationsSheet>, cx: &mut VisualTestContext) -> String {
    cx.update(|_, cx| sheet.read(cx).name.read(cx).value().to_string())
}

#[gpui::test]
fn esc_in_an_edited_name_gives_the_name_back_and_keeps_the_sheet(cx: &mut TestAppContext) {
    let (shell, sheet, cx) = open_sheet(cx);
    let before = name(cx);
    type_name(&sheet, " two", cx);
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(name(cx), before);
    assert_eq!(field(&sheet, cx), before);
    assert!(cx.update(|_, cx| shell.read(cx).locations_sheet().is_some()));
}

#[gpui::test]
fn leaving_the_name_field_renames(cx: &mut TestAppContext) {
    let (_, sheet, cx) = open_sheet(cx);
    let before = name(cx);
    type_name(&sheet, " two", cx);
    cx.update(|window, cx| {
        let list = sheet.read(cx).list_focus.clone();
        list.focus(window, cx);
    });
    cx.run_until_parked();
    assert_eq!(name(cx), format!("{before} two"));
}

#[gpui::test]
fn closing_the_sheet_renames(cx: &mut TestAppContext) {
    let (shell, sheet, cx) = open_sheet(cx);
    let before = name(cx);
    type_name(&sheet, " two", cx);
    sheet.update(cx, |_, cx| cx.emit(LocationsSheetEvent::Close));
    cx.run_until_parked();
    assert!(cx.update(|_, cx| shell.read(cx).locations_sheet().is_none()));
    assert_eq!(name(cx), format!("{before} two"));
}
