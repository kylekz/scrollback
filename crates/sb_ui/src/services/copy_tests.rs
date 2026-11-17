//! A copy of a session that ran elsewhere, kept on this machine with no original archived
//! (PLAN §7.7): it is shown, as a copy, and never as a session of this machine, wherever the
//! library names, filters, groups, counts or files by a location.

use gpui::TestAppContext;
use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::catalogue::{Filed, Session, Shelf};
use sb_core::location::{Location, LocationId, LocationKind};
use sb_core::search::{Filters, Limit, Query, Searched};
use sb_test_support::ProjectsTree;

use super::view::{Facets, PageView, UnsortedView, shown, shown_groups};
use super::*;
use crate::components::Place;
use crate::test_support::{init, init_library};

/// Where the copy's session ran: a Mac, as its records say, which is no location here.
const PROJECT: &str = "/Users/user/code/blog";

/// This machine's agent, holding only the desktop app's copy of a session that ran on a Mac.
fn machine_with_a_copy() -> ProjectsTree {
    let mut mac = ProjectsTree::new();
    let mut session = mac.session(PROJECT, 2);
    session
        .transcript
        .prompt("Draft the post about the archive");
    session.transcript.reply("Drafted.");
    let mut tree = ProjectsTree::new();
    tree.desktop_copy(&session);
    tree
}

/// The library over [`machine_with_a_copy`], synced, with this machine its one location.
fn library_of_a_copy(cx: &mut TestAppContext) -> (Session, Location) {
    init(machine_with_a_copy(), cx);
    cx.update(|cx| {
        let sync = cx.services().sync.clone();
        sync.update(cx, |sync, cx| sync.add_local(cx));
    });
    cx.run_until_parked();
    cx.update(|cx| {
        let services = cx.services();
        let here = services.sync.read(cx).locations().unwrap()[0].clone();
        assert_eq!(here.kind, LocationKind::Local);
        let page = services
            .library
            .read(cx)
            .sessions()
            .expect("a page of sessions");
        assert_eq!(page.sessions.len(), 1);
        (page.sessions[0].clone(), here)
    })
}

#[gpui::test]
fn a_lone_copy_is_shown_as_a_copy_and_never_as_this_machines(cx: &mut TestAppContext) {
    let (copy, here) = library_of_a_copy(cx);
    let label = copy.copy.clone().expect("the session is a copy").label;
    assert_eq!(copy.ran_at(), None);
    assert_eq!(copy.project_path.as_deref(), Some(PROJECT));

    /* Its tag is the copy's, never this machine's. */
    let place = Place::of(&copy, |id| (*id == here.id).then_some(&here)).unwrap();
    assert!(matches!(place, Place::Copy(_)));
    assert_eq!(place.name(), label);

    /* A page's Location filter offers no location for it, and this machine's hides it. */
    let sessions = [copy.clone()];
    assert!(Facets::of(&sessions).locations.is_empty());
    let from_here = PageView {
        location: Some(here.id.clone()),
        ..PageView::default()
    };
    assert!(shown(&sessions, &from_here, crate::format::now()).is_empty());
    assert_eq!(
        shown(&sessions, &PageView::default(), crate::format::now()),
        [0]
    );

    cx.update(|cx| {
        let services = cx.services();
        let catalogue = services
            .archive
            .read(cx)
            .archive()
            .unwrap()
            .catalogue()
            .clone();
        /* The library's stats name no location. */
        let page = services.library.read(cx).sessions().unwrap();
        assert_eq!(page.stats.locations, 0);
        /* Unsorted groups it by its path alone, under the copy's tag. */
        let groups = catalogue.unsorted().unwrap();
        assert_eq!(groups.len(), 1);
        assert!(groups[0].locations.is_empty());
        assert_eq!(groups[0].copy.as_deref(), Some(label.as_str()));
        let unsorted_here = UnsortedView {
            location: Some(here.id.clone()),
            ..UnsortedView::default()
        };
        assert!(shown_groups(&groups, &unsorted_here).is_empty());
        /* The paths editor offers its path for any location, under the copy's tag. */
        let seen = catalogue.project_paths().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            (seen[0].location.as_ref(), seen[0].copy.as_deref()),
            (None, Some(label.as_str()))
        );
        /* Search's `from:` this machine doesn't find it; with no filter it does. */
        let archive = services.archive.read(cx).archive().unwrap().clone();
        let found = |location: Option<LocationId>| {
            let query = Query {
                terms: vec!["archive".to_string()],
                filters: Filters {
                    location,
                    ..Filters::default()
                },
            };
            match archive.search(&query, Limit::default(), &CancelToken::new()) {
                Ok(Searched::Results(results)) => results.total_sessions,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!((found(Some(here.id.clone())), found(None)), (0, 1));
        /* This machine holds no session of its own. */
        assert!(
            catalogue
                .holdings()
                .unwrap()
                .iter()
                .all(|it| it.sessions == 0)
        );
        /* A rule for this machine doesn't file it; the same path for any location does. */
        let folder = catalogue.create_folder("blog", None).unwrap();
        catalogue
            .attach_path(folder, PROJECT, Some(&here.id))
            .unwrap();
        assert_eq!(
            catalogue.session(&copy.id).unwrap().unwrap().filed,
            Filed::Unsorted
        );
        assert_eq!(catalogue.sessions(Shelf::Unsorted).unwrap().len(), 1);
    });
}

#[gpui::test]
fn a_library_without_copies_is_as_it_was(cx: &mut TestAppContext) {
    init_library(cx);
    cx.update(|cx| {
        let page = cx.services().library.read(cx).sessions().unwrap();
        assert!(page.sessions.iter().all(
            |session| session.copy.is_none() && session.ran_at() == Some(&session.id.location)
        ));
        assert_eq!(page.stats.locations, 1);
    });
}
