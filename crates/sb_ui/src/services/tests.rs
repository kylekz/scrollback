use std::cell::RefCell;
use std::rc::Rc;

use gpui::{Entity, TestAppContext};
use pretty_assertions::assert_eq;
use sb_core::archive::Archive;
use sb_core::catalogue::{Filed, SessionId, Shelf};
use sb_core::filing::NormalPath;
use sb_core::location::Location;
use sb_core::sync::Progress;
use sb_test_support::{Outline, ProjectsTree, break_segment_commits, mend_segment_commits};

use super::library::LibraryEvent;
use super::sync::SyncEvent;
use super::sync::{SYNC_INTERVAL, local_name};
use super::*;
use crate::test_support::{
    SESSIONS, archive_dir, init, init_in, init_library, machine, write_session,
};

fn services(cx: &mut TestAppContext) -> (Entity<SyncService>, Entity<LibraryService>) {
    cx.update(|cx| {
        let services = cx.services();
        (services.sync.clone(), services.library.clone())
    })
}

fn locations(sync: &Entity<SyncService>, cx: &mut TestAppContext) -> Vec<Location> {
    sync.read_with(cx, |sync, _| {
        sync.locations().expect("the locations are read").to_vec()
    })
}

/// The session at `at` of the shown page.
fn id_at(library: &Entity<LibraryService>, at: usize, cx: &mut TestAppContext) -> SessionId {
    library.read_with(cx, |library, _| {
        library.sessions().expect("a page of sessions").sessions[at]
            .id
            .clone()
    })
}

/// Whether the session `id` is starred: on the page shown, and in the catalogue.
fn starred(
    library: &Entity<LibraryService>,
    id: &SessionId,
    cx: &mut TestAppContext,
) -> (bool, bool) {
    let shown = library.read_with(cx, |library, _| {
        let page = library.sessions().expect("a page of sessions");
        page.sessions.iter().any(|it| it.id == *id && it.starred)
    });
    let kept = cx.update(|cx| {
        let archive = cx.services().archive.read(cx).archive().cloned();
        archive
            .expect("the archive is open")
            .catalogue()
            .sessions(Shelf::Starred)
            .unwrap()
            .iter()
            .any(|it| it.id == *id)
    });
    (shown, kept)
}

fn titles(library: &Entity<LibraryService>, cx: &mut TestAppContext) -> Vec<String> {
    library.read_with(cx, |library, _| {
        library
            .sessions()
            .expect("a page of sessions")
            .sessions
            .iter()
            .filter_map(|session| session.title.clone())
            .collect()
    })
}

#[gpui::test]
fn a_first_run_makes_the_archive_and_looks_at_this_machine(cx: &mut TestAppContext) {
    init(machine(), cx);
    let (sync, library) = services(cx);
    let dir = archive_dir(cx);
    assert!(dir.join("archive.json").is_file(), "the archive is created");
    cx.update(|cx| {
        let services = cx.services();
        assert!(services.archive.read(cx).archive().is_some());
        assert!(services.is_settled(cx));
    });
    // No location yet, and what the first run offers: the agent found here.
    assert_eq!(locations(&sync, cx), []);
    sync.read_with(cx, |sync, _| {
        let found = sync.found().expect("this machine was looked at");
        let [Ok(found)] = &found[..] else {
            panic!("one agent is here: {found:?}");
        };
        assert_eq!(found.sessions, SESSIONS.len());
        assert!(found.root.ends_with("/projects"), "{}", found.root);
    });
    library.read_with(cx, |library, _| assert_eq!(library.counts().all, 0));
}

#[gpui::test]
fn a_machine_without_an_agent_offers_nothing(cx: &mut TestAppContext) {
    let machine = ProjectsTree::new();
    std::fs::remove_dir_all(machine.path("")).unwrap();
    init(machine, cx);
    let (sync, _) = services(cx);
    sync.read_with(cx, |sync, _| assert_eq!(sync.found(), Some(&Vec::new())));
}

#[gpui::test]
fn adding_this_machine_syncs_it_and_the_library_fills(cx: &mut TestAppContext) {
    init(machine(), cx);
    let (sync, library) = services(cx);
    sync.update(cx, |sync, cx| sync.add_local(cx));
    sync.read_with(cx, |sync, _| assert!(!sync.is_settled()));
    cx.run_until_parked();

    let [location] = &locations(&sync, cx)[..] else {
        panic!("one location");
    };
    assert_eq!(location.name, local_name());
    sync.read_with(cx, |sync, _| {
        assert!(sync.is_settled());
        assert!(matches!(sync.status(location), LocationStatus::Synced(_)));
    });
    // Everything lands in Unsorted, the newest first.
    assert_eq!(titles(&library, cx), SESSIONS.map(|session| session.title));
    library.read_with(cx, |library, _| {
        let counts = library.counts();
        assert_eq!((counts.all, counts.starred, counts.unsorted), (3, 0, 3));
        assert_eq!(library.agents(), 1);
        let page = library.sessions().unwrap();
        assert_eq!(
            page.stats,
            Stats {
                sessions: 3,
                calls: 10,
                span: page.stats.span,
                locations: 1,
            }
        );
        // November's two sessions, then October's one.
        let months: Vec<_> = page.months.iter().map(|it| it.sessions.clone()).collect();
        assert_eq!(months, [0..2, 2..3]);
        assert!(library.is_settled());
    });
}

#[gpui::test]
fn this_machine_is_one_location_however_often_it_is_added(cx: &mut TestAppContext) {
    init(machine(), cx);
    let (sync, _) = services(cx);
    // Twice before the first has landed, and once more after.
    sync.update(cx, |sync, cx| {
        sync.add_local(cx);
        sync.add_local(cx);
    });
    cx.run_until_parked();
    sync.update(cx, |sync, cx| sync.add_local(cx));
    cx.run_until_parked();
    assert_eq!(locations(&sync, cx).len(), 1);
    let archived = cx.update(|cx| {
        let archive = cx.services().archive.read(cx).archive().cloned().unwrap();
        archive.catalogue().locations().unwrap().len()
    });
    assert_eq!(archived, 1);
}

#[gpui::test]
fn a_stopped_first_sync_shows_what_it_copied_and_the_next_carries_on(cx: &mut TestAppContext) {
    // More sessions than a pass stores in one batch, so that one stops between two.
    let mut machine = ProjectsTree::new();
    for at in 0..300 {
        machine.outlined(&Outline {
            project: "/home/user/code/app",
            title: &format!("Session {at}"),
            prompt: "One of many.",
            model: "model-a",
            day: (2026, 11, 1 + at % 28),
            calls: 1,
        });
    }
    init(machine, cx);
    let (sync, library) = services(cx);
    sync.update(cx, |sync, cx| {
        sync.hold_after(1, Held::Stop);
        sync.add_local(cx);
    });
    cx.run_until_parked();
    let location = locations(&sync, cx).remove(0);
    let copied = library.read_with(cx, |library, _| library.counts().all);
    sync.read_with(cx, |sync, _| {
        assert!(sync.is_settled());
        // Stopping is not failing: the location says it was stopped, and what it left.
        assert!(matches!(
            sync.status(&location),
            LocationStatus::Stopped { items_left, .. } if items_left > 0
        ));
    });
    // What the pass stored before it stopped was indexed, and shows.
    assert!(copied > 0 && copied < 300, "{copied} of 300 show");

    sync.update(cx, |sync, cx| sync.sync_all(cx));
    cx.run_until_parked();
    let location = locations(&sync, cx).remove(0);
    sync.read_with(cx, |sync, _| {
        assert!(matches!(sync.status(&location), LocationStatus::Synced(_)));
    });
    library.read_with(cx, |library, _| assert_eq!(library.counts().all, 300));
}

#[gpui::test]
fn a_failed_sync_says_why_and_the_next_one_clears_it(cx: &mut TestAppContext) {
    init(machine(), cx);
    let (sync, library) = services(cx);
    break_segment_commits(&archive_dir(cx));
    sync.update(cx, |sync, cx| sync.add_local(cx));
    cx.run_until_parked();
    let location = locations(&sync, cx).remove(0);
    let why = sync.read_with(cx, |sync, _| {
        assert!(sync.is_settled());
        let LocationStatus::Failed(why) = sync.status(&location) else {
            panic!("the pass could not store anything");
        };
        why
    });
    assert!(why.contains("the test stops this commit"), "{why}");
    // The owner reads the agent's name, not its provider's id.
    cx.update(|cx| {
        let provider = &cx.services().providers[0];
        assert!(why.starts_with(&format!("{}: ", provider.id())), "{why}");
        let named = crate::agents::named(&why, cx);
        assert!(
            named.starts_with(&format!("{}: ", provider.display().name)),
            "{named}"
        );
    });
    library.read_with(cx, |library, _| assert_eq!(library.counts().all, 0));

    mend_segment_commits(&archive_dir(cx));
    sync.update(cx, |sync, cx| sync.sync(&location.id, cx));
    cx.run_until_parked();
    let location = locations(&sync, cx).remove(0);
    sync.read_with(cx, |sync, _| {
        assert!(matches!(sync.status(&location), LocationStatus::Synced(_)));
    });
    library.read_with(cx, |library, _| assert_eq!(library.counts().all, 3));
}

#[gpui::test]
fn a_later_sync_brings_what_the_agent_wrote_since(cx: &mut TestAppContext) {
    init_library(cx);
    let (sync, library) = services(cx);
    write_session(
        &Outline {
            project: "/home/user/code/app",
            title: "Cache the fetcher's responses",
            prompt: "Cache responses for five minutes.",
            model: "model-a",
            day: (2026, 11, 14),
            calls: 2,
        },
        cx,
    );
    sync.update(cx, |sync, cx| sync.sync_all(cx));
    sync.read_with(cx, |sync, _| assert!(sync.is_syncing()));
    cx.run_until_parked();
    sync.read_with(cx, |sync, _| assert!(!sync.is_syncing()));
    assert_eq!(titles(&library, cx)[0], "Cache the fetcher's responses");
    library.read_with(cx, |library, _| assert_eq!(library.counts().all, 4));
}

#[gpui::test]
fn every_location_syncs_again_on_the_interval(cx: &mut TestAppContext) {
    init_library(cx);
    let (sync, library) = services(cx);
    write_session(
        &Outline {
            project: "/home/user/notes",
            title: "Written while the app was open",
            prompt: "Nobody asks for this sync.",
            model: "model-a",
            day: (2026, 11, 15),
            calls: 0,
        },
        cx,
    );
    // Short of the interval nothing happens; at it, a pass runs by itself.
    cx.executor()
        .advance_clock(SYNC_INTERVAL - std::time::Duration::from_secs(1));
    cx.run_until_parked();
    library.read_with(cx, |library, _| assert_eq!(library.counts().all, 3));
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(1));
    cx.run_until_parked();
    sync.read_with(cx, |sync, _| assert!(sync.is_settled()));
    assert_eq!(titles(&library, cx)[0], "Written while the app was open");
}

#[gpui::test]
fn an_archive_that_doesnt_open_is_a_failure_and_none_is_made_in_its_place(cx: &mut TestAppContext) {
    // Another scrollback has the archive open.
    let scratch = tempfile::tempdir().unwrap();
    let dir = scratch.path().join("archive");
    let held = Archive::create(&dir).unwrap();
    let id = held.id().to_string();
    init_in(scratch, machine(), cx);
    let archive = cx.update(|cx| cx.services().archive.clone());
    archive.read_with(cx, |archive, _| {
        let ArchiveState::Failed(failure) = archive.state() else {
            panic!("the archive is locked");
        };
        assert!(matches!(failure, super::Failure::Locked(_)));
        assert!(
            failure.why().contains("open in another scrollback"),
            "{failure:?}"
        );
    });
    cx.update(|cx| {
        assert_eq!(crate::pages::Showing::now(cx).key(), "archive-failed");
        // Nothing syncs or loads without an archive, and nothing waits for one.
        assert!(cx.services().is_settled(cx));
        assert_eq!(cx.services().sync.read(cx).locations(), None);
    });

    // Once the other one has let go, trying again opens the same archive.
    drop(held);
    archive.update(cx, |archive, cx| archive.retry(cx));
    cx.run_until_parked();
    archive.read_with(cx, |archive, _| {
        assert_eq!(archive.archive().expect("the archive opens").id(), id);
    });
    cx.update(|cx| assert_eq!(crate::pages::Showing::now(cx).key(), "first-run"));
}

#[gpui::test]
fn a_folder_that_holds_what_is_left_of_an_archive_gets_no_new_one(cx: &mut TestAppContext) {
    let scratch = tempfile::tempdir().unwrap();
    let dir = scratch.path().join("archive");
    std::fs::create_dir_all(dir.join("files/location")).unwrap();
    init_in(scratch, machine(), cx);
    cx.update(|cx| {
        let archive = cx.services().archive.read(cx);
        let ArchiveState::Failed(failure) = archive.state() else {
            panic!("no archive is made over what is left of one");
        };
        assert!(matches!(failure, super::Failure::Other(_)), "{failure:?}");
    });
    assert!(!dir.join("archive.json").exists());
}

#[gpui::test]
fn a_star_shows_at_once_and_is_kept(cx: &mut TestAppContext) {
    init_library(cx);
    let (_, library) = services(cx);
    let second = id_at(&library, 1, cx);
    library.update(cx, |library, cx| library.toggle_star(&second, cx));
    // Before the catalogue has it.
    library.read_with(cx, |library, _| {
        assert!(library.sessions().unwrap().sessions[1].starred);
        assert_eq!(library.counts().starred, 1);
        assert!(!library.is_settled());
    });
    cx.run_until_parked();

    // Starred lists it, and unstarring it there takes it off the page.
    library.update(cx, |library, cx| library.show(Shelf::Starred, cx));
    cx.run_until_parked();
    assert_eq!(titles(&library, cx), ["Add retries to the fetcher"]);
    // The place it was drawn at may be out of date: it is found by what it is.
    library.update(cx, |library, cx| library.toggle_star(&second, cx));
    cx.run_until_parked();
    assert_eq!(titles(&library, cx), [""; 0]);
    library.read_with(cx, |library, _| assert_eq!(library.counts().starred, 0));
}

/// What the owner sees of some stars: each on the page shown, and the count of starred sessions.
type Stars = (Vec<bool>, usize);

/// Every state the library passes through from now on, as [`Stars`] of `ids`.
fn watch_stars(
    library: &Entity<LibraryService>,
    ids: Vec<SessionId>,
    cx: &mut TestAppContext,
) -> (Rc<RefCell<Vec<Stars>>>, gpui::Subscription) {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let subscription = cx.update(|cx| {
        let seen = seen.clone();
        cx.observe(library, move |library, cx| {
            let library = library.read(cx);
            let Some(page) = library.sessions() else {
                return;
            };
            let stars = ids
                .iter()
                .map(|id| page.sessions.iter().any(|it| it.id == *id && it.starred))
                .collect();
            seen.borrow_mut().push((stars, library.counts().starred));
        })
    });
    (seen, subscription)
}

#[gpui::test(iterations = 200)]
fn a_star_made_while_the_page_loads_is_not_undone_by_the_load(cx: &mut TestAppContext) {
    init_library(cx);
    let (_, library) = services(cx);
    let first = id_at(&library, 0, cx);
    let (seen, _watching) = watch_stars(&library, vec![first.clone()], cx);
    // The load starts before the star is made, and may finish before or after it is written.
    library.update(cx, |library, cx| {
        library.reload(cx);
        library.toggle_star(&first, cx);
    });
    assert_eq!(starred(&library, &first, cx).0, true, "shown at once");
    cx.run_until_parked();
    assert_eq!(starred(&library, &first, cx), (true, true));
    let seen = seen.borrow();
    assert!(
        seen.iter().all(|state| *state == (vec![true], 1)),
        "the star never went: {seen:?}"
    );
    library.read_with(cx, |library, _| assert!(library.is_settled()));
}

#[gpui::test(iterations = 200)]
fn no_load_takes_a_star_away_while_it_waits_to_be_written(cx: &mut TestAppContext) {
    init_library(cx);
    let (_, library) = services(cx);
    let (a, b) = (id_at(&library, 0, cx), id_at(&library, 1, cx));
    let (seen, _watching) = watch_stars(&library, vec![a.clone(), b.clone()], cx);
    // The second star waits while the first is written; a load asked for now must not start
    // before both are, or it reads the catalogue without them.
    library.update(cx, |library, cx| {
        library.toggle_star(&a, cx);
        library.toggle_star(&b, cx);
        library.reload(cx);
    });
    cx.run_until_parked();
    let seen = seen.borrow();
    assert!(
        seen.iter().all(|state| *state == (vec![true, true], 2)),
        "a star went while it was written: {seen:?}"
    );
    assert_eq!(starred(&library, &b, cx), (true, true));
}

#[gpui::test]
fn sync_news_lets_the_load_in_hand_be_taken_in(cx: &mut TestAppContext) {
    init_library(cx);
    let (sync, library) = services(cx);
    // Whether a load was taken in while the one that has the news was still to come.
    let early = Rc::new(RefCell::new(false));
    let _watching = cx.update(|cx| {
        let early = early.clone();
        cx.observe(&library, move |library, cx| {
            let library = library.read(cx);
            if library.page().is_some() && !library.is_settled() {
                *early.borrow_mut() = true;
            }
        })
    });
    library.update(cx, |library, cx| library.show(Shelf::Starred, cx));
    // News arrives while Starred is read: the load in hand is still a whole picture.
    sync.update(cx, |_, cx| cx.emit(SyncEvent::Archived));
    cx.run_until_parked();
    assert!(*early.borrow(), "the load in hand was thrown away");
    library.read_with(cx, |library, _| assert!(library.is_settled()));
}

#[gpui::test]
fn all_sessions_kept_aside_is_read_again_after_sync_news(cx: &mut TestAppContext) {
    init_library(cx);
    let (sync, library) = services(cx);
    library.update(cx, |library, cx| library.show(Shelf::Starred, cx));
    cx.run_until_parked();
    write_session(
        &Outline {
            project: "/home/user/notes",
            title: "Synced while Starred showed",
            prompt: "All sessions must not come back as it was.",
            model: "model-a",
            day: (2026, 11, 15),
            calls: 0,
        },
        cx,
    );
    sync.update(cx, |sync, cx| sync.sync_all(cx));
    cx.run_until_parked();
    library.update(cx, |library, cx| library.show(Shelf::All, cx));
    cx.run_until_parked();
    assert_eq!(titles(&library, cx)[0], "Synced while Starred showed");
}

#[gpui::test]
fn a_change_the_catalogue_refuses_is_said_and_the_page_shows_what_is_kept(cx: &mut TestAppContext) {
    init_library(cx);
    let (_, library) = services(cx);
    library.update(cx, |library, cx| {
        library.create_folder("work".into(), None, cx)
    });
    cx.run_until_parked();
    let folder = library.read_with(cx, |library, _| library.folders()[0].id);
    // A name of nothing but white space is no name.
    library.update(cx, |library, cx| {
        library.rename_folder(folder, "   ".into(), cx)
    });
    cx.run_until_parked();
    library.read_with(cx, |library, _| {
        assert_eq!(library.folders()[0].name, "work");
        let why = library.write_failure().expect("the refusal is said");
        assert!(
            why.starts_with("The name “   ” couldn't be saved: "),
            "{why}"
        );
        assert!(why.ends_with('.') && !why.ends_with(".."), "{why}");
    });
    library.update(cx, |library, cx| library.dismiss_write_failure(cx));
    library.read_with(cx, |library, _| assert_eq!(library.write_failure(), None));
}

#[gpui::test(iterations = 30)]
fn quick_stars_are_written_in_the_order_they_were_made(cx: &mut TestAppContext) {
    init_library(cx);
    let (_, library) = services(cx);
    let first = id_at(&library, 0, cx);
    // On and off again before either is written.
    library.update(cx, |library, cx| {
        library.toggle_star(&first, cx);
        library.toggle_star(&first, cx);
    });
    cx.run_until_parked();
    assert_eq!(starred(&library, &first, cx), (false, false));
    // And three: on, off, on.
    library.update(cx, |library, cx| {
        for _ in 0..3 {
            library.toggle_star(&first, cx);
        }
    });
    cx.run_until_parked();
    assert_eq!(starred(&library, &first, cx), (true, true));
    library.read_with(cx, |library, _| assert_eq!(library.counts().starred, 1));
}

#[gpui::test(iterations = 30)]
fn quick_renames_leave_the_last_name(cx: &mut TestAppContext) {
    init_library(cx);
    let (_, library) = services(cx);
    library.update(cx, |library, cx| {
        library.create_folder("New folder".into(), None, cx)
    });
    cx.run_until_parked();
    let folder = library.read_with(cx, |library, _| library.folders()[0].id);
    library.update(cx, |library, cx| {
        library.rename_folder(folder, "first".into(), cx);
        library.rename_folder(folder, "second".into(), cx);
        library.reload(cx);
        library.rename_folder(folder, "third".into(), cx);
    });
    cx.run_until_parked();
    library.read_with(cx, |library, _| {
        assert_eq!(library.folders()[0].name, "third");
        assert!(library.is_settled());
    });
}

#[gpui::test]
fn what_the_owner_changed_is_written_when_the_app_quits(cx: &mut TestAppContext) {
    init_library(cx);
    let (sync, library) = services(cx);
    let first = id_at(&library, 0, cx);
    let archive = cx.update(|cx| cx.services().archive.read(cx).archive().cloned().unwrap());
    // Nothing of this has reached the catalogue when the app is told to quit, mid-sync.
    library.update(cx, |library, cx| {
        library.toggle_star(&first, cx);
        library.create_folder("made at the last moment".into(), None, cx);
    });
    sync.update(cx, |sync, cx| sync.sync_all(cx));
    cx.quit();
    let catalogue = archive.catalogue();
    let starred = catalogue.sessions(Shelf::Starred).unwrap();
    assert_eq!(
        starred.iter().map(|it| &it.id).collect::<Vec<_>>(),
        [&first]
    );
    let folders = catalogue.folders().unwrap();
    assert_eq!(
        folders
            .iter()
            .map(|it| it.name.as_str())
            .collect::<Vec<_>>(),
        ["made at the last moment"]
    );
}

#[gpui::test]
fn a_folder_made_from_an_unsorted_path_files_the_paths_sessions(cx: &mut TestAppContext) {
    init_library(cx);
    let (sync, library) = services(cx);
    library.update(cx, |library, cx| library.show(Shelf::Unsorted, cx));
    cx.run_until_parked();
    let group = library.read_with(cx, |library, _| {
        let Some(Page::Unsorted(groups)) = library.page() else {
            panic!("Unsorted is loaded");
        };
        // The path with the most sessions first.
        let sizes: Vec<_> = groups
            .iter()
            .map(|group| (group.spellings[0].as_str(), group.sessions.len()))
            .collect();
        assert_eq!(sizes, [("/home/user/code/app", 2), ("/home/user/notes", 1)]);
        assert!(groups.iter().all(can_attach));
        group_key(&groups[0])
    });

    let created = Rc::new(RefCell::new(Vec::new()));
    let _heard = cx.update(|cx| {
        let created = created.clone();
        cx.subscribe(&library, move |_, event, _| {
            if let LibraryEvent::FolderCreated(folder) = event {
                created.borrow_mut().push(*folder);
            }
        })
    });
    // A second click before the first has landed makes no second folder.
    library.update(cx, |library, cx| {
        library.create_folder_from(&group, cx);
        assert!(library.is_creating(&group));
        library.create_folder_from(&group, cx);
    });
    cx.run_until_parked();

    let location = locations(&sync, cx)[0].id.clone();
    let folder = library.read_with(cx, |library, _| {
        let [folder] = library.folders() else {
            panic!("one folder");
        };
        assert_eq!(folder.name, "app");
        assert_eq!(library.counts().folders[&folder.id], 2);
        assert_eq!(library.counts().unsorted, 1);
        let Some(Page::Unsorted(groups)) = library.page() else {
            panic!("Unsorted is loaded");
        };
        assert_eq!(groups.len(), 1);
        assert!(!library.is_creating(&group));
        assert_eq!(library.refusal(&group), None);
        folder.id
    });
    assert_eq!(*created.borrow(), [folder], "the sidebar hears of it once");

    // Its page: the path's chip, limited to the group's location, and the sessions it filed.
    library.update(cx, |library, cx| library.show(Shelf::Folder(folder), cx));
    cx.run_until_parked();
    library.read_with(cx, |library, _| {
        let page = library.sessions().unwrap();
        assert_eq!(
            page.paths,
            [PathChip {
                typed: "/home/user/code/app".to_string(),
                path: NormalPath::new("/home/user/code/app", None).unwrap(),
                location: Some(location),
                sessions: 2,
            }]
        );
        assert!(
            page.sessions
                .iter()
                .all(|session| session.filed == Filed::ByRule(folder))
        );
    });
}

#[gpui::test]
fn a_folder_the_catalogue_refuses_is_not_made_and_the_group_says_why(cx: &mut TestAppContext) {
    init_library(cx);
    let (sync, library) = services(cx);
    library.update(cx, |library, cx| library.show(Shelf::Unsorted, cx));
    cx.run_until_parked();
    let group = library.read_with(cx, |library, _| group_key(&library.groups()[0]));
    // Behind the library's back the path goes to another folder: the page still offers it.
    let location = locations(&sync, cx)[0].id.clone();
    cx.update(|cx| {
        let archive = cx.services().archive.read(cx).archive().cloned().unwrap();
        let catalogue = archive.catalogue();
        let elsewhere = catalogue.create_folder("elsewhere", None).unwrap();
        catalogue
            .attach_path(elsewhere, "/home/user/code/app", Some(&location))
            .unwrap();
    });
    library.update(cx, |library, cx| library.create_folder_from(&group, cx));
    cx.run_until_parked();
    library.read_with(cx, |library, _| {
        let names: Vec<_> = library.folders().iter().map(|it| &it.name).collect();
        assert_eq!(names, ["elsewhere"], "no folder is left without its path");
        let why = library.refusal(&group).expect("the refusal is kept");
        assert!(why.contains("elsewhere"), "{why}");
        assert!(!library.is_creating(&group));
    });
}

#[gpui::test]
fn sessions_with_no_path_and_those_at_the_root_offer_no_folder(cx: &mut TestAppContext) {
    let mut machine = machine();
    for project in ["/", "relative/path"] {
        machine.outlined(&Outline {
            project,
            title: "Started somewhere no folder can claim",
            prompt: "Where am I?",
            model: "model-a",
            day: (2026, 11, 1),
            calls: 0,
        });
    }
    init(machine, cx);
    let (sync, library) = services(cx);
    sync.update(cx, |sync, cx| sync.add_local(cx));
    cx.run_until_parked();
    library.update(cx, |library, cx| library.show(Shelf::Unsorted, cx));
    cx.run_until_parked();
    let unattachable: Vec<String> = library.read_with(cx, |library, _| {
        library
            .groups()
            .iter()
            .filter(|group| !can_attach(group))
            .map(group_key)
            .collect()
    });
    assert_eq!(unattachable.len(), 2);
    // Asking anyway makes nothing.
    library.update(cx, |library, cx| {
        for group in &unattachable {
            library.create_folder_from(group, cx);
        }
    });
    cx.run_until_parked();
    library.read_with(cx, |library, _| assert_eq!(library.folders(), []));
}

#[test]
fn a_drives_group_is_written_in_its_drive_form() {
    use sb_core::catalogue::PathGroup;
    use sb_core::filing::{NormalPath, WSL_DRIVE_MOUNT};
    let group = |project: &str, spellings: &[&str], drive: Option<&str>| PathGroup {
        project: NormalPath::new(project, Some(WSL_DRIVE_MOUNT)),
        locations: Vec::new(),
        copy: None,
        spellings: spellings.iter().map(|it| it.to_string()).collect(),
        drive: drive.map(str::to_string),
        sessions: Vec::new(),
    };
    let also = |others: &[&str]| others.iter().map(|it| it.to_string()).collect::<Vec<_>>();
    // DESIGN §4.6: the drive form leads, whichever spelling was recorded first.
    assert_eq!(
        group_path(&group(
            "D:\\Projects\\fc",
            &["/mnt/d/Projects/fc", "D:\\Projects\\fc"],
            Some("D:\\Projects\\fc"),
        )),
        Some((
            "D:\\Projects\\fc".to_string(),
            also(&["/mnt/d/Projects/fc"])
        ))
    );
    // Recorded only through a mount, it is shown as the catalogue read it by that mount.
    assert_eq!(
        group_path(&group(
            "/mnt/d/Projects/fc",
            &["/win/d/Projects/fc"],
            Some("D:\\Projects\\fc"),
        )),
        Some((
            "D:\\Projects\\fc".to_string(),
            also(&["/win/d/Projects/fc"])
        ))
    );
    assert_eq!(
        group_path(&group(
            "/home/user/code/app",
            &["/home/user/code/app"],
            None
        )),
        Some(("/home/user/code/app".to_string(), Vec::new()))
    );
    assert_eq!(group_path(&group("", &[], None)), None);
}

#[gpui::test]
fn a_folder_made_from_a_drive_under_a_locations_own_mount_takes_the_drive_form(
    cx: &mut TestAppContext,
) {
    let mut machine = machine();
    machine.outlined(&Outline {
        project: "/win/d/Projects/fc",
        title: "Started on a drive mounted at /win/",
        prompt: "Where am I?",
        model: "model-a",
        day: (2026, 11, 14),
        calls: 0,
    });
    init(machine, cx);
    let (sync, library) = services(cx);
    sync.update(cx, |sync, cx| sync.add_local(cx));
    cx.run_until_parked();
    // The location mounts the drives at /win/, as a distro's /etc/wsl.conf can say.
    let location = locations(&sync, cx)[0].id.clone();
    cx.update(|cx| {
        let archive = cx.services().archive.read(cx).archive().cloned().unwrap();
        archive
            .catalogue()
            .set_drive_mount(&location, Some("/win/"))
            .unwrap();
    });
    library.update(cx, |library, cx| library.show(Shelf::Unsorted, cx));
    cx.run_until_parked();
    let group = library.read_with(cx, |library, _| {
        let group = library
            .groups()
            .iter()
            .find(|group| group.spellings == ["/win/d/Projects/fc"])
            .expect("the drive's group");
        assert_eq!(
            group_path(group),
            Some((
                "D:\\Projects\\fc".to_string(),
                vec!["/win/d/Projects/fc".to_string()]
            ))
        );
        group_key(group)
    });
    library.update(cx, |library, cx| library.create_folder_from(&group, cx));
    cx.run_until_parked();
    library.read_with(cx, |library, _| {
        let names: Vec<_> = library.folders().iter().map(|it| &it.name).collect();
        assert_eq!(names, ["fc"]);
        assert!(
            library
                .groups()
                .iter()
                .all(|it| it.spellings != ["/win/d/Projects/fc"]),
            "the group is filed into its folder"
        );
    });
}

#[gpui::test]
fn folders_are_made_named_and_nested(cx: &mut TestAppContext) {
    init_library(cx);
    let (_, library) = services(cx);
    library.update(cx, |library, cx| {
        library.create_folder("New folder".into(), None, cx)
    });
    cx.run_until_parked();
    let outer = library.read_with(cx, |library, _| library.folders()[0].id);
    library.update(cx, |library, cx| {
        library.rename_folder(outer, "work".into(), cx);
        library.create_folder("inside".into(), Some(outer), cx);
    });
    cx.run_until_parked();
    library.read_with(cx, |library, _| {
        let folders: Vec<_> = library
            .folders()
            .iter()
            .map(|folder| (folder.name.as_str(), folder.parent))
            .collect();
        assert_eq!(folders, [("work", None), ("inside", Some(outer))]);
    });
}

#[gpui::test]
fn the_selection_follows_its_session_when_the_page_changes_under_it(cx: &mut TestAppContext) {
    init_library(cx);
    let (sync, library) = services(cx);
    library.update(cx, |library, cx| library.select_index(0, cx));
    let selected = library.read_with(cx, |library, _| library.selected().cloned());
    write_session(
        &Outline {
            project: "/home/user/notes",
            title: "A newer session",
            prompt: "This one sorts first.",
            model: "model-a",
            day: (2026, 11, 15),
            calls: 0,
        },
        cx,
    );
    sync.update(cx, |sync, cx| sync.sync_all(cx));
    cx.run_until_parked();
    library.read_with(cx, |library, _| {
        assert_eq!(library.selected().cloned(), selected);
        assert_eq!(library.selected_index(), Some(1));
    });
}

#[test]
fn a_locations_progress_runs_over_each_of_its_agents() {
    let mut progress = PassProgress::default();
    assert_eq!(progress.fraction(), 0.);
    progress.apply(&Progress::Scanned { items: 10 });
    assert!(!progress.planned);
    progress.apply(&Progress::Planned {
        items: 4,
        bytes: 400,
    });
    progress.apply(&Progress::Fetched { bytes: 100 });
    assert_eq!(progress.fraction(), 0.25);
    progress.apply(&Progress::Fetched { bytes: 400 });
    progress.apply(&Progress::Stored { items: 4 });
    // A second agent's pass adds to the first's.
    progress.apply(&Progress::Planned {
        items: 6,
        bytes: 600,
    });
    progress.apply(&Progress::Fetched { bytes: 100 });
    progress.apply(&Progress::Stored { items: 1 });
    assert_eq!(
        (
            progress.items_done,
            progress.items,
            progress.bytes_done,
            progress.bytes
        ),
        (5, 10, 500, 1000)
    );
    assert_eq!(progress.fraction(), 0.5);
    // Then the sessions they changed are indexed: their rows, the first half of the bar.
    let indexing = |sessions| Progress::Indexing {
        sessions,
        bytes: 0,
        outdated: 0,
    };
    let indexed = |sessions, searchable| Progress::Indexed {
        sessions,
        bytes: 0,
        searchable,
    };
    let searched = |sessions, searchable| Progress::Searched {
        sessions,
        searchable,
    };
    progress.apply(&indexing(8));
    progress.apply(&indexed(2, 0));
    assert!(progress.is_indexing());
    assert_eq!(at(&progress), (Some((IndexStep::Rows, 6)), 0.125));
    // Their text, made from the same parse, is written a batch at a time as they are.
    progress.apply(&indexed(8, 4));
    assert_eq!(progress.searchable_done, 4);
    assert_eq!(at(&progress), (Some((IndexStep::Rows, 0)), 0.5));
    // Then their text, the second half: what is left counts what it has to make searchable.
    progress.apply(&Progress::Searching { sessions: 4 });
    assert_eq!(at(&progress), (Some((IndexStep::Texts, 4)), 0.5));
    progress.apply(&searched(1, 0));
    assert_eq!(at(&progress), (Some((IndexStep::Texts, 3)), 0.625));
    // Text written is news of its own.
    progress.apply(&searched(2, 2));
    assert_eq!(progress.searchable_done, 6);
    assert_eq!(at(&progress), (Some((IndexStep::Texts, 2)), 0.75));
    /* The first agent's index fails there. The second's starts the bar again from its own
    start, owing nothing of the first, and counts only its own. */
    progress.apply(&indexing(2));
    assert_eq!(at(&progress), (Some((IndexStep::Rows, 2)), 0.));
    progress.apply(&indexed(2, 0));
    progress.apply(&Progress::Searching { sessions: 2 });
    progress.apply(&searched(1, 1));
    assert_eq!(at(&progress), (Some((IndexStep::Texts, 1)), 0.75));
    progress.apply(&searched(2, 2));
    assert_eq!(at(&progress), (Some((IndexStep::Texts, 0)), 1.));
    // The counts over every agent go on.
    assert_eq!(
        (
            progress.sessions_done,
            progress.sessions,
            progress.searchable_done
        ),
        (10, 10, 8)
    );
    // A pass with nothing to fetch is done once it is planned.
    let mut nothing = PassProgress::default();
    nothing.apply(&Progress::Planned { items: 0, bytes: 0 });
    assert_eq!(nothing.fraction(), 1.);
}

/// The step an index is at with how many sessions it has left, and how far the bar is.
fn at(progress: &PassProgress) -> (Option<(IndexStep, usize)>, f32) {
    (
        progress.indexing().map(|index| (index.step, index.left())),
        progress.fraction(),
    )
}

#[test]
fn an_index_with_only_text_to_make_has_the_whole_bar() {
    let mut progress = PassProgress::default();
    progress.apply(&Progress::Indexing {
        sessions: 0,
        bytes: 0,
        outdated: 0,
    });
    assert!(!progress.is_indexing());
    // With no text to make either, there is nothing to show.
    progress.apply(&Progress::Searching { sessions: 0 });
    assert!(!progress.is_indexing());
    progress.apply(&Progress::Searching { sessions: 10 });
    progress.apply(&Progress::Searched {
        sessions: 5,
        searchable: 0,
    });
    assert_eq!(at(&progress), (Some((IndexStep::Texts, 5)), 0.5));
}

#[test]
fn a_second_step_with_nothing_to_do_leaves_the_first_done() {
    let mut progress = PassProgress::default();
    progress.apply(&Progress::Indexing {
        sessions: 3,
        bytes: 0,
        outdated: 0,
    });
    progress.apply(&Progress::Indexed {
        sessions: 3,
        bytes: 0,
        searchable: 3,
    });
    progress.apply(&Progress::Searching { sessions: 0 });
    assert_eq!(at(&progress), (Some((IndexStep::Rows, 0)), 1.));
}

#[test]
fn an_agent_with_nothing_to_index_shows_the_index_before_it_done() {
    let indexing = |sessions| Progress::Indexing {
        sessions,
        bytes: 0,
        outdated: 0,
    };
    let mut progress = PassProgress::default();
    progress.apply(&Progress::Planned {
        items: 4,
        bytes: 400,
    });
    progress.apply(&Progress::Fetched { bytes: 400 });
    progress.apply(&Progress::Stored { items: 4 });
    // The first agent's index, cut short in its second step.
    progress.apply(&indexing(3));
    progress.apply(&Progress::Indexed {
        sessions: 3,
        bytes: 0,
    });
    progress.apply(&Progress::Searching { sessions: 3 });
    progress.apply(&Progress::Searched {
        sessions: 1,
        searchable: 0,
    });
    // The second agent's has no rows to make: the first's shows, done, not the fetching.
    progress.apply(&indexing(0));
    assert_eq!(at(&progress), (Some((IndexStep::Texts, 0)), 1.));
    progress.apply(&Progress::Searching { sessions: 0 });
    assert_eq!(at(&progress), (Some((IndexStep::Texts, 0)), 1.));
    // Nor text: the same. With text, its own.
    let mut text = progress;
    text.apply(&indexing(0));
    text.apply(&Progress::Searching { sessions: 4 });
    assert_eq!(at(&text), (Some((IndexStep::Texts, 4)), 0.));
}
