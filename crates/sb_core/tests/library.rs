//! Folders, filing and what the library reads, through the catalogue (PLAN §4.3, §8, §12).
//! The sessions are mostly the made-up agent's, which keeps a database and says where each
//! started: filing knows no agent.

mod common;

use std::collections::HashMap;

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::catalogue::{Catalogue, CatalogueError, Filed, Folder, SessionId, Shelf};
use sb_core::filing::{FolderId, NormalPath};
use sb_core::location::{LocationId, LocationKind};
use sb_core::sync::sync_location;
use sb_test_support::{Agent, DbAgent, FileAgent};

use common::Bench;

/// A new session of the made-up agent that started in `project`. Returns its transcript's key.
fn started(agent: &mut DbAgent, project: &str) -> String {
    let key = agent.add_session(0);
    agent.say(&key, "start", project);
    agent.say(&key, "user", "Carry on.");
    key
}

/// A second location of a bench's archive, with an agent of its own.
struct Elsewhere {
    agent: DbAgent,
    location: LocationId,
}

impl Elsewhere {
    fn new(bench: &Bench<DbAgent>) -> Self {
        Self {
            agent: DbAgent::new(),
            location: bench
                .archive()
                .catalogue()
                .add_location(LocationKind::Local, "elsewhere")
                .unwrap()
                .id,
        }
    }

    fn sync(&self, bench: &Bench<DbAgent>) {
        sync_location(
            bench.archive(),
            &self.location,
            &self.agent.transport(),
            &[self.agent.provider()],
            &CancelToken::new(),
            &mut |_| {},
        )
        .unwrap();
    }

    /// The identity of the session here whose transcript is the item `key`.
    fn session_id(&self, key: &str) -> SessionId {
        SessionId {
            location: self.location.clone(),
            provider: self.agent.provider().id().to_string(),
            key: key.strip_prefix("sessions/").unwrap().to_string(),
        }
    }
}

fn filed(catalogue: &Catalogue, id: &SessionId) -> Filed {
    catalogue.session(id).unwrap().unwrap().filed
}

/// The sessions of a page, as their identities.
fn listed(catalogue: &Catalogue, shelf: Shelf) -> Vec<SessionId> {
    catalogue
        .sessions(shelf)
        .unwrap()
        .into_iter()
        .map(|session| session.id)
        .collect()
}

/// The folders in `parent` as `name(the folders in it) name …`, in their order.
fn outline(folders: &[Folder], parent: Option<FolderId>) -> String {
    folders
        .iter()
        .filter(|folder| folder.parent == parent)
        .map(|folder| match &outline(folders, Some(folder.id))[..] {
            "" => folder.name.clone(),
            inside => format!("{}({inside})", folder.name),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn folders_nest_and_reorder_and_move_up_when_their_parent_goes() {
    let bench = Bench::new(DbAgent::new());
    let catalogue = bench.archive().catalogue();
    let tree = || outline(&catalogue.folders().unwrap(), None);
    let create = |name, parent| catalogue.create_folder(name, parent).unwrap();
    let (a, b, c) = (create("a", None), create("b", None), create("c", None));
    let (a1, a2) = (create("a1", Some(a)), create("a2", Some(a)));
    assert_eq!(tree(), "a(a1 a2) b c");

    catalogue.rename_folder(b, "bee").unwrap();
    assert_eq!(tree(), "a(a1 a2) bee c");

    /* Nesting and reordering are one move: into a folder, at a place among those there. */
    catalogue.move_folder(b, Some(a), 1).unwrap();
    assert_eq!(tree(), "a(a1 bee a2) c");
    catalogue.move_folder(c, None, 0).unwrap();
    assert_eq!(tree(), "c a(a1 bee a2)");
    catalogue.move_folder(a2, Some(a), 0).unwrap();
    assert_eq!(tree(), "c a(a2 a1 bee)");
    catalogue.move_folder(b, None, 99).unwrap();
    assert_eq!(tree(), "c a(a2 a1) bee");

    /* A folder goes neither into itself nor into a folder inside it. */
    let deep = create("deep", Some(a1));
    for inside in [a, a1, deep] {
        assert!(matches!(
            catalogue.move_folder(a, Some(inside), 0),
            Err(CatalogueError::FolderInItself)
        ));
    }
    assert_eq!(tree(), "c a(a2 a1(deep)) bee");

    /* A deleted folder's subfolders take its place. */
    catalogue.delete_folder(a).unwrap();
    assert_eq!(tree(), "c a2 a1(deep) bee");
    catalogue.delete_folder(c).unwrap();
    assert_eq!(tree(), "a2 a1(deep) bee");
}

#[test]
fn a_new_session_is_filed_by_the_longest_attached_path() {
    let mut bench = Bench::new(DbAgent::new());
    let agent = &mut bench.agent;
    let keys = [
        started(agent, "/home/user/code/app/crates/core"),
        started(agent, "/home/user/code/other"),
        started(agent, "/srv/x/"),
        started(agent, "/srv/x"),
        agent.add_session(2),
    ];
    let catalogue = bench.archive().catalogue();
    let code = catalogue.create_folder("code", None).unwrap();
    let app = catalogue.create_folder("app", Some(code)).unwrap();
    catalogue
        .attach_path(code, "/home/user/code", None)
        .unwrap();
    catalogue
        .attach_path(app, "/home/user/code/app", None)
        .unwrap();

    assert_eq!(bench.sync().index.filed, 2);
    let ids: Vec<_> = keys.iter().map(|key| bench.session_id(key)).collect();
    assert_eq!(
        ids.iter()
            .map(|id| filed(catalogue, id))
            .collect::<Vec<_>>(),
        [
            /* Under a path counts, and the subfolder's longer path claims first. */
            Filed::ByRule(app),
            Filed::ByRule(code),
            Filed::Unsorted,
            Filed::Unsorted,
            Filed::Unsorted,
        ]
    );

    /* A folder holds its subfolders' sessions: its count and its page include them. */
    let counts = catalogue.library_counts().unwrap();
    assert_eq!((counts.all, counts.starred, counts.unsorted), (5, 0, 3));
    assert_eq!(counts.folders, HashMap::from([(code, 2), (app, 1)]));
    assert_eq!(listed(catalogue, Shelf::All), ids);
    assert_eq!(listed(catalogue, Shelf::Folder(code)), ids[..2]);
    assert_eq!(listed(catalogue, Shelf::Folder(app)), ids[..1]);
    assert_eq!(listed(catalogue, Shelf::Unsorted), ids[2..]);

    /* Unsorted is grouped by project path, however the path was written. */
    let groups: Vec<_> = catalogue
        .unsorted()
        .unwrap()
        .into_iter()
        .map(|group| {
            let sessions: Vec<_> = group.sessions.into_iter().map(|s| s.id).collect();
            (group.project, sessions)
        })
        .collect();
    assert_eq!(
        groups,
        [
            (NormalPath::new("/srv/x", None), ids[2..4].to_vec()),
            (None, ids[4..].to_vec()),
        ]
    );
}

#[test]
fn a_change_of_paths_files_what_is_unsorted_and_what_a_rule_filed() {
    let mut bench = Bench::new(DbAgent::new());
    let inner = started(&mut bench.agent, "/home/user/code/app");
    let outer = started(&mut bench.agent, "/home/user/code/other");
    assert_eq!(bench.sync().index.filed, 0);
    let (inner, outer) = (bench.session_id(&inner), bench.session_id(&outer));
    let catalogue = bench.archive().catalogue();
    let both = || (filed(catalogue, &inner), filed(catalogue, &outer));
    assert_eq!(both(), (Filed::Unsorted, Filed::Unsorted));

    /* A path attached: what is in Unsorted under it is filed. */
    let code = catalogue.create_folder("code", None).unwrap();
    catalogue
        .attach_path(code, "/home/user/code/", None)
        .unwrap();
    assert_eq!(both(), (Filed::ByRule(code), Filed::ByRule(code)));
    let paths = catalogue.folder_paths(code).unwrap();
    assert_eq!(
        (paths[0].attached.path.as_str(), paths[0].typed.as_str()),
        ("/home/user/code", "/home/user/code/")
    );

    /* A longer one attached elsewhere: what a rule filed moves. */
    let app = catalogue.create_folder("app", None).unwrap();
    catalogue
        .attach_path(app, "/home/user/code/app", None)
        .unwrap();
    assert_eq!(both(), (Filed::ByRule(app), Filed::ByRule(code)));

    /* Paths detached: back to the shorter path's folder, and then to Unsorted. */
    let attached = |folder| catalogue.folder_paths(folder).unwrap().remove(0).attached;
    catalogue.detach_path(&attached(app)).unwrap();
    assert_eq!(both(), (Filed::ByRule(code), Filed::ByRule(code)));
    catalogue.detach_path(&attached(code)).unwrap();
    assert_eq!(both(), (Filed::Unsorted, Filed::Unsorted));
    assert!(catalogue.folder_paths(code).unwrap().is_empty());
}

#[test]
fn a_path_limited_to_a_location_files_only_that_locations_sessions() {
    let mut bench = Bench::new(DbAgent::new());
    let mut elsewhere = Elsewhere::new(&bench);
    let here = started(&mut bench.agent, "/home/user/code/app");
    let there = started(&mut elsewhere.agent, "/home/user/code/app/docs");
    bench.sync();
    elsewhere.sync(&bench);
    let (here, there) = (bench.session_id(&here), elsewhere.session_id(&there));
    let catalogue = bench.archive().catalogue();
    let both = || (filed(catalogue, &here), filed(catalogue, &there));

    /* Limited to the other location, the path leaves this location's session where it is. */
    let theirs = catalogue.create_folder("theirs", None).unwrap();
    catalogue
        .attach_path(theirs, "/home/user/code/app", Some(&elsewhere.location))
        .unwrap();
    assert_eq!(both(), (Filed::Unsorted, Filed::ByRule(theirs)));

    /* The same path for every location, on another folder: the one for the session's own
    location wins. */
    let all = catalogue.create_folder("all", None).unwrap();
    catalogue
        .attach_path(all, "/home/user/code/app", None)
        .unwrap();
    assert_eq!(both(), (Filed::ByRule(all), Filed::ByRule(theirs)));

    /* A session that arrives later is filed the same way when it is indexed. */
    let later = started(&mut elsewhere.agent, "/home/user/code/app");
    elsewhere.sync(&bench);
    let catalogue = bench.archive().catalogue();
    assert_eq!(
        filed(catalogue, &elsewhere.session_id(&later)),
        Filed::ByRule(theirs)
    );
    let counts = catalogue.library_counts().unwrap();
    assert_eq!(counts.folders, HashMap::from([(all, 1), (theirs, 2)]));
}

#[test]
fn a_path_is_attached_once_for_each_scope() {
    let bench = Bench::new(DbAgent::new());
    let catalogue = bench.archive().catalogue();
    let first = catalogue.create_folder("first", None).unwrap();
    let second = catalogue.create_folder("second", None).unwrap();
    let refused = |folder, typed, location| match catalogue.attach_path(folder, typed, location) {
        Err(CatalogueError::PathAttached {
            folder,
            name,
            typed,
        }) => (folder, name, typed),
        other => panic!("{typed} was not refused: {other:?}"),
    };
    let has_it = |typed: &str| (first, "first".to_string(), typed.to_string());

    /* For every location: a second attach is refused and names the folder that has the path,
    whichever folder it is for and however the path is written. */
    catalogue
        .attach_path(first, "/home/user/code/app", None)
        .unwrap();
    assert_eq!(
        refused(second, "/home/user/code/app/", None),
        has_it("/home/user/code/app")
    );
    assert_eq!(
        refused(first, "/home/user//code/app", None),
        has_it("/home/user/code/app")
    );
    catalogue
        .attach_path(first, r"D:\Projects\App", None)
        .unwrap();
    assert_eq!(
        refused(second, "d:/projects/app", None),
        has_it(r"D:\Projects\App")
    );

    /* For one location it is another scope, and there too it is attached once. */
    let location = Some(&bench.location);
    catalogue
        .attach_path(second, "/home/user/code/app", location)
        .unwrap();
    assert_eq!(
        refused(first, "/home/user/code/app", location),
        (
            second,
            "second".to_string(),
            "/home/user/code/app".to_string()
        )
    );
    assert_eq!(catalogue.folder_paths(first).unwrap().len(), 2);
    assert_eq!(catalogue.folder_paths(second).unwrap().len(), 1);

    /* A path that names nothing would claim everything. */
    for nothing in ["", "/", "."] {
        assert!(matches!(
            catalogue.attach_path(first, nothing, None),
            Err(CatalogueError::NoPath(_))
        ));
    }
}

#[test]
fn a_session_filed_by_hand_stays_until_it_is_handed_back_to_the_rules() {
    let mut bench = Bench::new(DbAgent::new());
    let claimed = started(&mut bench.agent, "/home/user/code/app");
    let stray = started(&mut bench.agent, "/srv/x");
    bench.sync();
    let ids = (bench.session_id(&claimed), bench.session_id(&stray));
    let catalogue = bench.archive().catalogue();
    let kept = catalogue.create_folder("kept", None).unwrap();
    catalogue.file_by_hand(&ids.0, kept).unwrap();
    catalogue.file_by_hand(&ids.1, kept).unwrap();

    /* No rule moves it: not a path attached later, nor being indexed again. */
    let app = catalogue.create_folder("app", None).unwrap();
    catalogue
        .attach_path(app, "/home/user/code/app", None)
        .unwrap();
    assert_eq!(filed(catalogue, &ids.0), Filed::ByHand(kept));
    bench.agent.say(&claimed, "user", "More.");
    assert_eq!(bench.sync().index.sessions, 1);
    let catalogue = bench.archive().catalogue();
    assert_eq!(filed(catalogue, &ids.0), Filed::ByHand(kept));
    assert_eq!(catalogue.library_counts().unwrap().folders[&kept], 2);

    /* File automatically: the rules have it again, and put it where its path says. */
    assert_eq!(catalogue.file_automatically(&ids.0).unwrap(), Some(app));
    assert_eq!(filed(catalogue, &ids.0), Filed::ByRule(app));
    assert_eq!(catalogue.file_automatically(&ids.1).unwrap(), None);
    assert_eq!(filed(catalogue, &ids.1), Filed::Unsorted);
    let counts = catalogue.library_counts().unwrap();
    assert_eq!(counts.unsorted, 1);
    assert_eq!(counts.folders, HashMap::from([(kept, 0), (app, 1)]));
}

#[test]
fn a_deleted_folders_sessions_go_to_unsorted_and_are_filed_again() {
    let mut bench = Bench::new(DbAgent::new());
    let agent = &mut bench.agent;
    let keys = [
        started(agent, "/home/user/code/app"),
        started(agent, "/srv/x"),
        started(agent, "/srv/y"),
    ];
    bench.sync();
    let ids: Vec<_> = keys.iter().map(|key| bench.session_id(key)).collect();
    let catalogue = bench.archive().catalogue();
    let code = catalogue.create_folder("code", None).unwrap();
    let app = catalogue.create_folder("app", Some(code)).unwrap();
    let deep = catalogue.create_folder("deep", Some(app)).unwrap();
    catalogue
        .attach_path(code, "/home/user/code", None)
        .unwrap();
    catalogue
        .attach_path(app, "/home/user/code/app", None)
        .unwrap();
    catalogue.file_by_hand(&ids[1], app).unwrap();
    catalogue.file_by_hand(&ids[2], deep).unwrap();
    let all = || -> Vec<_> { ids.iter().map(|id| filed(catalogue, id)).collect() };
    assert_eq!(
        all(),
        [Filed::ByRule(app), Filed::ByHand(app), Filed::ByHand(deep)]
    );

    /* Its sessions go to Unsorted, the one filed by hand too, and the rules file them again:
    the parent's path claims the one that started under it. Its subfolder moves up with what
    is in it, and its paths are gone. */
    catalogue.delete_folder(app).unwrap();
    assert_eq!(
        all(),
        [Filed::ByRule(code), Filed::Unsorted, Filed::ByHand(deep)]
    );
    assert_eq!(outline(&catalogue.folders().unwrap(), None), "code(deep)");
    assert!(catalogue.folder_paths(app).unwrap().is_empty());
    let counts = catalogue.library_counts().unwrap();
    assert_eq!(counts.unsorted, 1);
    assert_eq!(counts.folders, HashMap::from([(code, 2), (deep, 1)]));

    /* The path is free to attach again. */
    catalogue
        .attach_path(deep, "/home/user/code/app", None)
        .unwrap();
    assert_eq!(all()[0], Filed::ByRule(deep));
}

#[test]
fn the_library_lists_sessions_by_when_they_last_did_something() {
    let mut agent = FileAgent::new();
    let keys = [
        agent.add_session(4),
        agent.add_session(12),
        agent.add_session(8),
    ];
    let bench = Bench::new(agent);
    bench.sync();
    let ids: Vec<_> = keys.iter().map(|key| bench.session_id(key)).collect();
    let catalogue = bench.archive().catalogue();

    /* The fixture's sessions start together, so the longest ended last. */
    let all = catalogue.sessions(Shelf::All).unwrap();
    assert_eq!(
        all.iter().map(|session| &session.id).collect::<Vec<_>>(),
        [&ids[1], &ids[2], &ids[0]]
    );
    assert!(all[0].ended > all[1].ended && all[1].ended > all[2].ended);
    assert_eq!(all[0].started, all[2].started);

    /* A row has what a card shows. */
    let card = &all[0];
    assert!(card.title.is_some() && card.first_prompt.is_some());
    assert_eq!(card.project_path.as_deref(), Some("/home/user/code/app"));
    assert_eq!(card.id.provider, bench.agent.provider().id().as_str());
    assert_eq!(card.models, ["model-1"]);
    assert_eq!((card.own.calls, card.all.calls), (3, 3));
    assert_eq!(
        (card.filed, card.starred, card.held_back),
        (Filed::Unsorted, false, false)
    );

    catalogue.set_starred(&ids[0], true).unwrap();
    catalogue.set_starred(&ids[2], true).unwrap();
    assert_eq!(
        listed(catalogue, Shelf::Starred),
        [ids[2].clone(), ids[0].clone()]
    );
    assert_eq!(catalogue.library_counts().unwrap().starred, 2);
    catalogue.set_starred(&ids[2], false).unwrap();
    assert_eq!(listed(catalogue, Shelf::Starred), ids[..1]);
    let counts = catalogue.library_counts().unwrap();
    assert_eq!((counts.all, counts.starred, counts.unsorted), (3, 1, 3));
}
