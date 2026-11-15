use std::cell::RefCell;
use std::rc::Rc;

use gpui::{Entity, TestAppContext};
use pretty_assertions::assert_eq;
use sb_core::archive::Archive;
use sb_core::catalogue::{Filed, Shelf};
use sb_core::location::Location;
use sb_core::sync::Progress;
use sb_test_support::{Outline, ProjectsTree};

use super::library::LibraryEvent;
use super::sync::local_name;
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
fn an_archive_that_doesnt_open_is_a_failure_and_none_is_made_in_its_place(cx: &mut TestAppContext) {
    // Another scrollback has the archive open.
    let scratch = tempfile::tempdir().unwrap();
    let dir = scratch.path().join("archive");
    let held = Archive::create(&dir).unwrap();
    let id = held.id().to_string();
    init_in(scratch, machine(), cx);
    let archive = cx.update(|cx| cx.services().archive.clone());
    archive.read_with(cx, |archive, _| {
        let ArchiveState::Failed(why) = archive.state() else {
            panic!("the archive is locked");
        };
        assert!(why.contains("open in another scrollback"), "{why}");
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
        assert!(matches!(archive.state(), ArchiveState::Failed(_)));
    });
    assert!(!dir.join("archive.json").exists());
}

#[gpui::test]
fn a_star_shows_at_once_and_is_kept(cx: &mut TestAppContext) {
    init_library(cx);
    let (_, library) = services(cx);
    let id = library.read_with(cx, |library, _| {
        library.sessions().unwrap().sessions[1].id.clone()
    });
    library.update(cx, |library, cx| library.set_starred(&id, true, cx));
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
    library.update(cx, |library, cx| library.set_starred(&id, false, cx));
    cx.run_until_parked();
    assert_eq!(titles(&library, cx), [""; 0]);
    library.read_with(cx, |library, _| assert_eq!(library.counts().starred, 0));
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
        groups[0].clone()
    });

    let created = Rc::new(RefCell::new(Vec::new()));
    let _heard = cx.update(|cx| {
        let created = created.clone();
        cx.subscribe(&library, move |_, event, _| match event {
            LibraryEvent::FolderCreated(folder) => created.borrow_mut().push(*folder),
        })
    });
    library.update(cx, |library, cx| library.create_folder_from(&group, cx));
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
    // Then the sessions they changed are indexed.
    progress.apply(&Progress::Indexing {
        sessions: 8,
        bytes: 0,
    });
    progress.apply(&Progress::Indexed {
        sessions: 2,
        bytes: 0,
    });
    assert!(progress.is_indexing());
    assert_eq!(progress.fraction(), 0.25);
    // A pass with nothing to fetch is done once it is planned.
    let mut nothing = PassProgress::default();
    nothing.apply(&Progress::Planned { items: 0, bytes: 0 });
    assert_eq!(nothing.fraction(), 1.);
}
