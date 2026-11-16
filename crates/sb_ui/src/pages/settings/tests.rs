//! Settings' sections (DESIGN §4.13), each through the shell as the owner uses it.

use gpui::{Entity, Focusable as _, Modifiers, TestAppContext, VisualTestContext, px, size};
use pretty_assertions::assert_eq;
use sb_core::catalogue::Shelf;

use super::*;
use crate::appearance::Mode;
use crate::pages::Showing;
use crate::services::SyncEvery;
use crate::shell::Shell;
use crate::system::{self, SystemRequest, SystemShell};
use crate::test_support::{
    archive_dir, bounds, init, machine, open_shell, open_shell_sized, open_window,
};

fn showing(cx: &mut VisualTestContext) -> Showing {
    cx.update(|_, cx| Showing::now(cx))
}

/// Clicks the middle of the element drawn with `name` for its debug selector.
fn click(cx: &mut VisualTestContext, name: &'static str) {
    let at = bounds(cx, name).unwrap_or_else(|| panic!("{name} is not drawn"));
    cx.simulate_click(at.center(), Modifiers::none());
    cx.run_until_parked();
}

fn page(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Entity<SettingsPage> {
    cx.update(|_, cx| shell.read(cx).main().read(cx).settings_page().clone())
}

/// The shell over a library, with a scripted system shell, and Settings open by `Ctrl ,`.
fn open_settings(cx: &mut TestAppContext) -> (Entity<Shell>, &mut VisualTestContext) {
    cx.update(|cx| system::install(SystemShell::scripted(), cx));
    let (shell, cx) = open_shell(cx);
    cx.simulate_keystrokes("secondary-,");
    cx.run_until_parked();
    assert_eq!(showing(cx), Showing::Settings);
    (shell, cx)
}

#[gpui::test]
fn settings_open_over_the_page_and_every_way_back_leads_to_it(cx: &mut TestAppContext) {
    cx.update(|cx| system::install(SystemShell::scripted(), cx));
    let (_shell, cx) = open_shell(cx);
    let library = cx.update(|_, cx| cx.services().library.clone());
    library.update(cx, |library, cx| library.show(Shelf::Unsorted, cx));
    cx.run_until_parked();

    /* The gear, then Back; Ctrl , then Esc; Ctrl , then Alt ←. */
    click(cx, "titlebar-settings");
    assert_eq!(showing(cx), Showing::Settings);
    click(cx, "settings-back");
    assert_eq!(showing(cx), Showing::Unsorted);
    for back in ["escape", "alt-left"] {
        cx.simulate_keystrokes("secondary-,");
        cx.run_until_parked();
        assert_eq!(showing(cx), Showing::Settings);
        cx.simulate_keystrokes(back);
        cx.run_until_parked();
        assert_eq!(showing(cx), Showing::Unsorted, "{back}");
    }

    /* Picking a page in the sidebar leaves Settings for it. */
    cx.simulate_keystrokes("secondary-,");
    cx.run_until_parked();
    cx.simulate_keystrokes("f6 up");
    cx.run_until_parked();
    assert_eq!(showing(cx), Showing::Sessions);
    library.read_with(cx, |library, _| assert_eq!(library.shelf(), Shelf::Starred));
}

#[gpui::test]
fn the_list_picks_the_section_at_the_top_of_the_column(cx: &mut TestAppContext) {
    let (shell, cx) = open_settings(cx);
    let page = page(&shell, cx);
    let current = |cx: &mut VisualTestContext| page.read_with(cx, |page, _| page.current());
    assert_eq!(current(cx), Section::Archive);

    /* A click scrolls the column to the section, which the list then picks. */
    click(cx, "settings-nav-appearance");
    assert_eq!(current(cx), Section::Appearance);
    let top = bounds(cx, "settings-nav-appearance").unwrap();
    assert!(top.size.height > px(0.));

    /* A section in the middle sits where the first one does, below the column's top padding,
    and the list still picks it once the column is scrolled by hand. */
    let column = bounds(cx, "settings-column").unwrap();
    click(cx, "settings-nav-sync");
    assert_eq!(current(cx), Section::Sync);
    let sync = bounds(cx, "settings-section-sync").unwrap();
    assert_eq!(sync.top() - column.top(), px(24.));
    page.update(cx, |page, cx| {
        page.picked = None;
        cx.notify();
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert_eq!(current(cx), Section::Sync);

    /* Scrolled by hand, the list follows the column: at its end, the last section. */
    page.update(cx, |page, cx| {
        page.picked = None;
        page.scroll.set_offset(gpui::point(px(0.), px(-100_000.)));
        cx.notify();
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert_eq!(current(cx), Section::Shortcuts);
    page.update(cx, |page, cx| {
        page.scroll.set_offset(gpui::point(px(0.), px(0.)));
        cx.notify();
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert_eq!(current(cx), Section::Archive);
}

#[gpui::test]
fn the_archive_folder_shows_in_the_file_manager_through_the_seam(cx: &mut TestAppContext) {
    let (_shell, cx) = open_settings(cx);
    click(cx, "settings-reveal");
    let requests = cx.update(|_, cx| system::requests(cx));
    assert_eq!(requests, [SystemRequest::Reveal(archive_dir(cx))]);
    /* Nothing was asked of the real system. */
    assert!(!cx.did_prompt_for_paths());
}

#[gpui::test]
fn a_different_folder_is_confirmed_before_the_app_moves_onto_it(cx: &mut TestAppContext) {
    let (shell, cx) = open_settings(cx);
    let elsewhere = tempfile::tempdir().unwrap();
    let new = elsewhere.path().join("archive");

    /* Cancelled: the app stays where it is. */
    cx.update(|_, cx| system::queue_answer(Some(new.clone()), cx).unwrap());
    click(cx, "settings-use-folder");
    assert!(page(&shell, cx).read_with(cx, |page, _| page.chosen.is_some()));
    click(cx, "settings-confirm-cancel");
    assert!(!new.exists());
    assert_eq!(showing(cx), Showing::Settings);

    /* Esc closes the confirmation, not Settings. */
    cx.update(|_, cx| system::queue_answer(Some(new.clone()), cx).unwrap());
    click(cx, "settings-use-folder");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(showing(cx), Showing::Settings);
    assert!(page(&shell, cx).read_with(cx, |page, _| page.chosen.is_none()));

    /* Confirmed: a new archive there, and the window shows it, from its first run. */
    cx.update(|_, cx| system::queue_answer(Some(new.clone()), cx).unwrap());
    click(cx, "settings-use-folder");
    click(cx, "settings-confirm-ok");
    for _ in 0..10 {
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(50));
        cx.run_until_parked();
    }
    assert!(new.join("archive.json").is_file());
    assert_eq!(showing(cx), Showing::FirstRun);
    cx.update(|_, cx| {
        assert_eq!(
            cx.services().archive.read(cx).dir(),
            std::path::absolute(&new).unwrap()
        );
    });

    /* The first run in a folder the owner chose keeps the way back to Settings, to choose
    another: the gear and Ctrl ,. */
    assert!(bounds(cx, "titlebar-settings").is_some());
    assert!(bounds(cx, "titlebar-sync-all").is_none());
    click(cx, "titlebar-settings");
    assert_eq!(showing(cx), Showing::Settings);
    /* Settings over it keeps the first run's title bar. */
    assert!(bounds(cx, "titlebar-sync-all").is_none());
    assert!(bounds(cx, "titlebar-search").is_none());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(showing(cx), Showing::FirstRun);
    cx.simulate_keystrokes("secondary-,");
    cx.run_until_parked();
    assert_eq!(showing(cx), Showing::Settings);
}

/// Another folder chosen while a sync and a verify run on the archive open: both are stopped,
/// the window's pages and sidebar move onto the new archive, and nothing keeps the one left
/// open.
#[gpui::test]
fn a_switch_while_a_sync_and_a_verify_run_lets_the_archive_left_go(cx: &mut TestAppContext) {
    let (_shell, cx) = open_settings(cx);
    let left = cx.update(|_, cx| {
        std::sync::Arc::downgrade(cx.services().archive.read(cx).archive().unwrap())
    });
    let elsewhere = tempfile::tempdir().unwrap();
    let new = elsewhere.path().join("archive");
    cx.update(|_, cx| {
        let services = cx.services();
        let (sync, upkeep, settings) = (
            services.sync.clone(),
            services.upkeep.clone(),
            services.settings.clone(),
        );
        sync.update(cx, |sync, cx| sync.sync_all(cx));
        upkeep.update(cx, |upkeep, cx| upkeep.verify(cx));
        assert!(sync.read(cx).is_syncing());
        assert!(upkeep.read(cx).verifying().is_some());
        settings.update(cx, |settings, cx| settings.use_folder(new.clone(), cx));
    });
    for _ in 0..20 {
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(50));
        cx.run_until_parked();
    }
    assert!(new.join("archive.json").is_file());
    assert_eq!(showing(cx), Showing::FirstRun);
    assert!(left.upgrade().is_none(), "the archive left is still open");
    /* The window still draws, over the new archive. */
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(bounds(cx, "titlebar-settings").is_some());
}

/// The first run in the data folder's archive offers no Settings: there is nothing to choose
/// yet, and the first run says where the archive goes.
#[gpui::test]
fn the_first_run_in_the_data_folder_has_no_settings(cx: &mut TestAppContext) {
    init(machine(), cx);
    let (_shell, cx) = open_window(size(px(1440.), px(900.)), cx);
    assert_eq!(showing(cx), Showing::FirstRun);
    assert!(bounds(cx, "titlebar-settings").is_none());
    cx.simulate_keystrokes("secondary-,");
    cx.run_until_parked();
    assert_eq!(showing(cx), Showing::FirstRun);
}

#[gpui::test]
fn a_folder_that_holds_something_else_is_refused_without_asking(cx: &mut TestAppContext) {
    let (shell, cx) = open_settings(cx);
    let documents = tempfile::tempdir().unwrap();
    std::fs::write(documents.path().join("notes.txt"), "mine").unwrap();
    cx.update(|_, cx| system::queue_answer(Some(documents.path().to_path_buf()), cx).unwrap());
    click(cx, "settings-use-folder");
    assert!(page(&shell, cx).read_with(cx, |page, _| page.chosen.is_none()));
    let switch = cx.update(|_, cx| cx.services().settings.read(cx).switch().cloned());
    assert!(
        matches!(
            switch,
            Some(crate::services::settings::Switch::Refused { .. })
        ),
        "{switch:?}"
    );
    assert_eq!(showing(cx), Showing::Settings);
}

#[gpui::test]
fn verify_now_runs_verify_and_shows_its_result(cx: &mut TestAppContext) {
    let (_shell, cx) = open_settings(cx);
    click(cx, "settings-verify-run");
    for _ in 0..10 {
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        cx.run_until_parked();
    }
    cx.update(|_, cx| {
        let upkeep = cx.services().upkeep.read(cx);
        assert!(upkeep.verifying().is_none());
        let last = upkeep.last().expect("the verify ran to its end");
        assert!(last.segments > 0 && last.damaged.is_empty());
        assert!(
            upkeep
                .stats()
                .is_some_and(|stats| stats.totals.sessions == 3)
        );
    });
    assert!(bounds(cx, "settings-verify-run").is_some());
}

#[gpui::test]
fn sync_every_is_a_menu_and_esc_closes_it_before_settings(cx: &mut TestAppContext) {
    let (shell, cx) = open_settings(cx);
    click(cx, "settings-nav-sync");
    click(cx, "settings-every");
    assert!(page(&shell, cx).read_with(cx, |page, _| page.menu));
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(page(&shell, cx).read_with(cx, |page, _| !page.menu));
    assert_eq!(showing(cx), Showing::Settings);

    click(cx, "settings-every");
    click(cx, "settings-every-60m");
    cx.update(|_, cx| {
        assert_eq!(cx.services().sync.read(cx).every(), SyncEvery::Sixty);
        assert_eq!(
            cx.services().settings.read(cx).settings().sync_every,
            SyncEvery::Sixty
        );
    });
    assert!(page(&shell, cx).read_with(cx, |page, _| !page.menu));
}

#[gpui::test]
fn stale_after_takes_whole_days_within_its_range(cx: &mut TestAppContext) {
    let (shell, cx) = open_settings(cx);
    let input = page(&shell, cx).read_with(cx, |page, _| page.stale_after.clone());
    let days = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| cx.services().sync.read(cx).stale_after_days())
    };
    for (typed, kept, shown) in [("14", 14, "14"), ("0", 1, "1"), ("99", 90, "90")] {
        cx.update(|window, cx| {
            input.update(cx, |input, cx| input.set_value(typed, window, cx));
            input.read(cx).focus_handle(cx).focus(window, cx);
        });
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(days(cx), kept, "{typed}");
        assert_eq!(
            input.read_with(cx, |input, _| input.value().to_string()),
            shown
        );
    }

    /* Typed and left without Enter: by Back and `Esc`, and by losing the keyboard. (`Alt ←`
    moves by a word in the field.) */
    let mut typed = 20;
    /* Focus is lost only in a window that is active. */
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    for leave in ["blur", "back", "escape"] {
        typed += 1;
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.set_value(typed.to_string(), window, cx)
            });
            input.read(cx).focus_handle(cx).focus(window, cx);
        });
        cx.run_until_parked();
        match leave {
            "back" => click(cx, "settings-back"),
            "blur" => {
                cx.update(|window, cx| window.blur(cx));
                cx.run_until_parked();
            }
            keys => {
                cx.simulate_keystrokes(keys);
                cx.run_until_parked();
            }
        }
        assert_eq!(days(cx), typed, "{leave}");
        if showing(cx) != Showing::Settings {
            cx.simulate_keystrokes("secondary-,");
            cx.run_until_parked();
        }
    }
}

#[gpui::test]
fn theme_and_material_change_what_is_drawn_and_are_kept(cx: &mut TestAppContext) {
    let (_shell, cx) = open_settings(cx);
    click(cx, "settings-nav-appearance");
    click(cx, "settings/appearance/theme/light");
    cx.update(|_, cx| {
        assert_eq!(gpui_appearance::state(cx).preferences().mode, Mode::Light);
        assert_eq!(cx.services().settings.read(cx).settings().mode, Mode::Light);
    });
    click(cx, "settings/appearance/material/opaque");
    cx.update(|_, cx| {
        assert_eq!(
            gpui_appearance::state(cx).preferences().material,
            crate::theme::Material::Opaque
        );
    });
}

#[test]
fn each_platform_offers_its_own_materials_and_says_why_one_cant_be_shown() {
    use super::appearance::{Platform, materials};
    use crate::theme::Material::{Acrylic, Mica, Opaque, Vibrancy};
    use gpui_appearance::MaterialSupport;

    let windows_11 = MaterialSupport::for_windows(Some(26200), true);
    assert_eq!(
        materials(Platform::Windows, windows_11),
        (vec![(Mica, true), (Acrylic, true), (Opaque, true)], None)
    );
    /* Windows 10, or transparency effects off: None alone, and why. */
    let (choices, why) = materials(
        Platform::Windows,
        MaterialSupport::for_windows(Some(19045), true),
    );
    assert_eq!(choices, [(Mica, false), (Acrylic, false), (Opaque, true)]);
    assert!(why.is_some_and(|why| why.contains("Transparency effects")));
    let (_, why) = materials(
        Platform::Windows,
        MaterialSupport::for_windows(Some(22000), true),
    );
    assert!(why.is_some_and(|why| why.contains("Mica")));
    assert_eq!(
        materials(Platform::MacOs, MaterialSupport::for_macos(false)),
        (vec![(Vibrancy, true), (Opaque, true)], None)
    );
    let (choices, why) = materials(Platform::MacOs, MaterialSupport::for_macos(true));
    assert_eq!(choices, [(Vibrancy, false), (Opaque, true)]);
    assert!(why.is_some_and(|why| why.contains("Reduce transparency")));
}

#[gpui::test]
fn settings_fit_a_narrow_window(cx: &mut TestAppContext) {
    cx.update(|cx| system::install(SystemShell::scripted(), cx));
    let (_shell, cx) = open_shell_sized(size(px(960.), px(600.)), cx);
    cx.simulate_keystrokes("secondary-,");
    cx.run_until_parked();
    assert_eq!(showing(cx), Showing::Settings);
    let back = bounds(cx, "settings-back").unwrap();
    let nav = bounds(cx, "settings-nav-shortcuts").unwrap();
    assert!(back.origin.x < px(60.), "the sidebar is hidden: {back:?}");
    assert!(nav.right() <= px(960.));
}

/// A dialog over Settings, the library's or Settings' own, keeps the keyboard: the shell's keys
/// do nothing behind it, and `Esc` answers it before it leaves Settings.
#[gpui::test]
fn a_dialog_over_settings_keeps_the_keyboard(cx: &mut TestAppContext) {
    let (shell, cx) = open_settings(cx);
    let sheet_open = |shell: &Entity<Shell>, cx: &mut VisualTestContext| {
        cx.update(|_, cx| shell.read(cx).locations_sheet().is_some())
    };

    /* Settings' confirmation of another folder. */
    let elsewhere = tempfile::tempdir().unwrap();
    cx.update(|_, cx| system::queue_answer(Some(elsewhere.path().join("archive")), cx).unwrap());
    click(cx, "settings-use-folder");
    let page = page(&shell, cx);
    cx.update(|window, cx| {
        assert!(
            page.read(cx).cancel_focus.is_focused(window),
            "the dialog has the keyboard, on Cancel"
        );
    });
    cx.simulate_keystrokes("secondary-l");
    cx.run_until_parked();
    assert!(!sheet_open(&shell, cx));
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(page.read_with(cx, |page, _| page.chosen.is_none()));
    assert_eq!(showing(cx), Showing::Settings);

    /* The library's Delete folder… over Settings. */
    let library = cx.update(|_, cx| cx.services().library.clone());
    library.update(cx, |library, cx| {
        library.create_folder("work".into(), None, cx);
    });
    cx.run_until_parked();
    let folder = library.read_with(cx, |library, _| library.folders()[0].id);
    library.update(cx, |library, cx| library.ask_delete(folder, cx));
    cx.run_until_parked();
    for key in ["secondary-l", "alt-left", "secondary-,"] {
        cx.simulate_keystrokes(key);
        cx.run_until_parked();
        assert!(!sheet_open(&shell, cx), "{key}");
        assert_eq!(showing(cx), Showing::Settings, "{key}");
    }
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    library.read_with(cx, |library, _| assert_eq!(library.deleting(), None));
    assert_eq!(
        showing(cx),
        Showing::Settings,
        "Esc answered the dialog only"
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_ne!(showing(cx), Showing::Settings);
}

/**
 * The version at the column's foot, and Restart to update under it once an update is ready:
 * clicked, the installer takes the update.
 */
#[gpui::test]
fn restart_to_update_shows_under_the_version_once_an_update_is_ready(cx: &mut TestAppContext) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::services::updates::{FIRST_CHECK_AFTER, Real, Updater};

    let (shell, cx) = open_settings(cx);
    let page = page(&shell, cx);
    let scroll_down = |cx: &mut VisualTestContext| {
        page.update(cx, |page, cx| {
            page.scroll.scroll_to_bottom();
            cx.notify();
        });
        cx.run_until_parked();
    };
    scroll_down(cx);
    assert!(bounds(cx, "settings-version").is_some());
    assert!(
        bounds(cx, "settings/restart-to-update").is_none(),
        "a development build has none"
    );

    let restarts = Arc::new(AtomicUsize::new(0));
    let counted = restarts.clone();
    let updates = cx.update(|_, cx| cx.services().updates.clone());
    updates.update(cx, |updates, cx| {
        updates.set_updater(
            Updater::Real(Real {
                check: Arc::new(|| Box::pin(async { Ok(Some("0.2.0".to_string())) })),
                restart: Box::new(move || {
                    counted.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }),
                others: Box::new(|| 0),
                then: |_| {},
            }),
            cx,
        )
    });
    cx.executor().advance_clock(FIRST_CHECK_AFTER);
    cx.run_until_parked();
    scroll_down(cx);
    click(cx, "settings/restart-to-update");
    assert_eq!(restarts.load(Ordering::SeqCst), 1);
    updates.read_with(cx, |updates, _| assert!(updates.is_restarted()));
}
