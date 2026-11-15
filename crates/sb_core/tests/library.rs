//! Folders, filing and what the library reads, through the catalogue (PLAN §4.3, §8, §12).
//! The sessions are mostly the made-up agent's, which keeps a database and says where each
//! started: filing knows no agent.

mod common;

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::catalogue::{Catalogue, CatalogueError, Filed, Folder, SessionId, Shelf};
use sb_core::filing::{AttachedPath, FolderId, NormalPath};
use sb_core::location::{LocationId, LocationKind};
use sb_core::provider::{Hashed, RangeFailure};
use sb_core::sync::sync_location;
use sb_core::transport::{
    Env, FileStat, Listing, PrefixRequest, RangeRequest, RangeSink, Transport, TransportError,
};
use sb_test_support::{Agent, DbAgent, FileAgent, HomeTransport, read_catalogue};

use common::{Bench, Meddle};

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
        Self::of(bench, LocationKind::Local)
    }

    /// A second location of the kind `kind`.
    fn of(bench: &Bench<DbAgent>, kind: LocationKind) -> Self {
        Self {
            agent: DbAgent::new(),
            location: bench
                .archive()
                .catalogue()
                .add_location(kind, "elsewhere")
                .unwrap()
                .id,
        }
    }

    fn sync(&self, bench: &Bench<DbAgent>) {
        self.sync_through(bench, &self.agent.transport());
    }

    fn sync_through(&self, bench: &Bench<DbAgent>, transport: &dyn Transport) {
        sync_location(
            bench.archive(),
            &self.location,
            transport,
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
fn a_folder_made_with_a_path_is_made_whole_or_not_at_all() {
    let mut bench = Bench::new(DbAgent::new());
    let key = started(&mut bench.agent, "/home/user/code/app");
    bench.sync();
    let catalogue = bench.archive().catalogue();
    let names = || -> Vec<String> {
        let folders = catalogue.folders().unwrap();
        folders.into_iter().map(|folder| folder.name).collect()
    };

    /* The folder, its path, and the sessions the path claims, in one change. */
    let location = Some(&bench.location);
    let app = catalogue
        .create_folder_with_path("app", None, " /home/user/code/app ", location)
        .unwrap();
    assert_eq!(names(), ["app"]);
    let paths = catalogue.folder_paths(app).unwrap();
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].typed, "/home/user/code/app");
    assert_eq!(paths[0].attached.location.as_ref(), location);
    assert_eq!(bench.session(&key).filed, Filed::ByRule(app));

    /* A path that is attached already, or names no directory, or does not say where it is
    from the top, or a name that is none, leaves no folder behind. */
    assert!(matches!(
        catalogue.create_folder_with_path("again", None, "/home/user/code/app", location),
        Err(CatalogueError::PathAttached { folder, .. }) if folder == app
    ));
    for refused in ["", "/", ".", "code/app", "~/code"] {
        assert!(matches!(
            catalogue.create_folder_with_path("stray", None, refused, None),
            Err(CatalogueError::NoPath(_))
        ));
    }
    assert!(matches!(
        catalogue.create_folder_with_path(" ", None, "/home/user/other", None),
        Err(CatalogueError::FolderName)
    ));
    assert!(matches!(
        catalogue.create_folder_with_path("inside", Some(FolderId(9999)), "/home/user/x", None),
        Err(CatalogueError::NoFolder(_))
    ));
    assert_eq!(names(), ["app"]);
}

#[test]
fn sessions_with_no_path_are_a_group_apart_from_those_at_the_root() {
    let mut bench = Bench::new(DbAgent::new());
    let nowhere = [bench.agent.add_session(1), bench.agent.add_session(1)];
    let at_root = started(&mut bench.agent, "/");
    let here = started(&mut bench.agent, ".");
    bench.sync();
    let groups = bench.archive().catalogue().unsorted().unwrap();
    let group = |key: &String| {
        let id = bench.session_id(key);
        let group = groups
            .iter()
            .find(|group| group.sessions.iter().any(|session| session.id == id))
            .unwrap();
        assert_eq!(group.project, None, "no folder can claim it");
        (group.spellings.clone(), group.sessions.len())
    };
    assert_eq!(groups.len(), 3);
    assert_eq!(group(&nowhere[0]), (Vec::new(), 2));
    assert_eq!(group(&nowhere[1]), (Vec::new(), 2));
    assert_eq!(group(&at_root), (vec!["/".to_string()], 1));
    assert_eq!(group(&here), (vec![".".to_string()], 1));
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
    assert!(matches!(
        catalogue.folder_paths(app),
        Err(CatalogueError::NoFolder(_))
    ));
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

#[test]
fn unsorted_is_grouped_by_where_its_sessions_started() {
    let mut bench = Bench::new(DbAgent::new());
    let mut elsewhere = Elsewhere::new(&bench);
    let keys = [
        started(&mut bench.agent, "/home/user/code/app"),
        started(&mut bench.agent, r"D:\Projects\App"),
        started(&mut bench.agent, "/home/user/code/app/"),
    ];
    let there = [
        started(&mut elsewhere.agent, "/home/user/code/app"),
        started(&mut elsewhere.agent, "/mnt/d/Projects/App"),
    ];
    bench.sync();
    elsewhere.sync(&bench);
    let here: Vec<_> = keys.iter().map(|key| bench.session_id(key)).collect();
    let there: Vec<_> = there.iter().map(|key| elsewhere.session_id(key)).collect();

    let groups = bench.archive().catalogue().unsorted().unwrap();
    let group = |project: &str, first: &SessionId| {
        let group = groups
            .iter()
            .find(|group| group.sessions.iter().any(|session| session.id == *first))
            .unwrap();
        assert_eq!(
            group.project.as_ref().map(NormalPath::as_str),
            Some(project)
        );
        (
            group.locations.clone(),
            group.spellings.clone(),
            group
                .sessions
                .iter()
                .map(|s| s.id.clone())
                .collect::<Vec<_>>(),
        )
    };
    let (ours, theirs) = (bench.location.clone(), elsewhere.location.clone());
    assert_eq!(groups.len(), 3);
    /* The same path on two machines is two places. */
    assert_eq!(
        group("/home/user/code/app", &here[0]),
        (
            vec![ours.clone()],
            vec![
                "/home/user/code/app".to_string(),
                "/home/user/code/app/".to_string()
            ],
            vec![here[0].clone(), here[2].clone()],
        )
    );
    assert_eq!(
        group("/home/user/code/app", &there[0]),
        (
            vec![theirs.clone()],
            vec!["/home/user/code/app".to_string()],
            vec![there[0].clone()],
        )
    );
    /* A drive is one place from wherever it is seen, however its path is written. Which
    location's session is listed first is by the locations' ids. */
    let (mut locations, mut spellings, sessions) = group("d:/projects/app", &here[1]);
    let mut both = [ours, theirs];
    for locations in [&mut locations[..], &mut both[..]] {
        locations.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    }
    spellings.sort();
    assert_eq!(locations, both);
    assert_eq!(spellings, ["/mnt/d/Projects/App", r"D:\Projects\App"]);
    assert_eq!(sessions.len(), 2);
    assert!(sessions.contains(&here[1]) && sessions.contains(&there[1]));
}

#[test]
fn a_location_that_mounts_its_drives_elsewhere_is_filed_by_its_own_mount() {
    /* A WSL distro whose /etc/wsl.conf puts the Windows drives under /windows/. */
    let mut bench = Bench::new(DbAgent::new());
    let catalogue = bench.archive().catalogue().clone();
    catalogue
        .set_drive_mount(&bench.location, Some("/windows/"))
        .unwrap();
    assert_eq!(bench.location().drive_mount.as_deref(), Some("/windows/"));
    let on_drive = started(&mut bench.agent, "/windows/d/Projects/app/src");
    let not_a_drive = started(&mut bench.agent, "/mnt/d/Projects/app/src");
    bench.sync();
    let folder = catalogue.create_folder("app", None).unwrap();
    catalogue
        .attach_path(folder, r"D:\Projects\app", None)
        .unwrap();
    assert_eq!(
        filed(&catalogue, &bench.session_id(&on_drive)),
        Filed::ByRule(folder)
    );
    /* There, /mnt/ is a folder like any other. */
    assert_eq!(
        filed(&catalogue, &bench.session_id(&not_a_drive)),
        Filed::Unsorted
    );

    /* A path attached for the location is read by its mount as well. */
    let other = catalogue.create_folder("other", None).unwrap();
    catalogue
        .attach_path(other, "/windows/e/Work", Some(&bench.location))
        .unwrap();
    assert_eq!(
        catalogue.folder_paths(other).unwrap()[0]
            .attached
            .path
            .as_str(),
        "e:/work"
    );
}

/// A WSL distro's side of a location: the fixture's files, and an `/etc/wsl.conf` of the
/// test's choosing.
struct Distro {
    files: HomeTransport,
    /// The file's text, `None` where there is no such file, or what goes wrong reading it.
    conf: Mutex<Result<Option<String>, String>>,
    /// How many times the file was read.
    read: AtomicUsize,
}

impl Transport for Distro {
    fn env(&self, names: &[&str]) -> Result<Env, TransportError> {
        self.files.env(names)
    }

    fn stat(&self, paths: &[&str]) -> Result<Vec<Option<FileStat>>, TransportError> {
        self.files.stat(paths)
    }

    fn list(&self, root: &str) -> Result<Listing, TransportError> {
        self.files.list(root)
    }

    fn hash_prefixes(
        &self,
        requests: &[PrefixRequest],
    ) -> Result<Vec<Result<Hashed, RangeFailure>>, TransportError> {
        self.files.hash_prefixes(requests)
    }

    fn read_ranges(
        &self,
        requests: &[RangeRequest],
        sink: &mut dyn RangeSink,
    ) -> Result<(), TransportError> {
        self.files.read_ranges(requests, sink)
    }

    fn read_small(&self, path: &str) -> Result<Option<Vec<u8>>, TransportError> {
        if path != "/etc/wsl.conf" {
            return self.files.read_small(path);
        }
        self.read.fetch_add(1, Ordering::Relaxed);
        match &*self.conf.lock().unwrap() {
            Ok(conf) => Ok(conf.clone().map(String::into_bytes)),
            Err(why) => Err(TransportError::Failed(why.clone())),
        }
    }
}

#[test]
fn a_distro_that_moves_its_drives_has_its_paths_read_again() {
    let bench = Bench::new(DbAgent::new());
    let catalogue = bench.archive().catalogue().clone();
    let mut distro = Elsewhere::of(
        &bench,
        LocationKind::Wsl {
            distro: "Ubuntu-24.04".to_string(),
        },
    );
    let sessions = [
        "/mnt/d/Projects/app/src",
        "/windows/d/Projects/app/src",
        "/d/Projects/app/src",
    ]
    .map(|project| started(&mut distro.agent, project));
    let app = catalogue.create_folder("app", None).unwrap();
    catalogue
        .attach_path(app, r"D:\Projects\app", None)
        .unwrap();
    /* Two paths for the distro alone: one under a folder that may come to be the drive
    mount, and the drive itself. */
    let [work, drive] = ["work", "drive"].map(|name| catalogue.create_folder(name, None).unwrap());
    for (folder, typed) in [(work, "/windows/e/Work"), (drive, r"E:\Work")] {
        catalogue
            .attach_path(folder, typed, Some(&distro.location))
            .unwrap();
    }

    let transport = Distro {
        files: distro.agent.transport(),
        conf: Mutex::new(Ok(None)),
        read: AtomicUsize::new(0),
    };
    /* After a sync that finds the file to be `conf`: the mount on record, which of the
    sessions the folder's path claims, and the paths attached for the distro. */
    let synced = |conf: Result<Option<&str>, &str>| {
        *transport.conf.lock().unwrap() = conf
            .map(|conf| conf.map(str::to_string))
            .map_err(str::to_string);
        distro.sync_through(&bench, &transport);
        let claimed = sessions
            .each_ref()
            .map(|key| filed(&catalogue, &distro.session_id(key)) == Filed::ByRule(app));
        let attached = [work, drive].map(|folder| {
            let paths = catalogue.folder_paths(folder).unwrap();
            paths
                .iter()
                .map(|path| path.attached.path.as_str().to_string())
                .collect::<Vec<_>>()
                .join(" ")
        });
        let location = catalogue.location(&distro.location).unwrap().unwrap();
        (location.drive_mount, claimed, attached)
    };
    let mount = |mount: &str| Some(mount.to_string());
    let paths = |paths: [&str; 2]| paths.map(str::to_string);

    /* No file: the drives are under /mnt/. */
    assert_eq!(
        synced(Ok(None)),
        (
            None,
            [true, false, false],
            paths(["/windows/e/Work", "e:/work"])
        )
    );
    /* The owner moves them. No session changed, and each is read by the new mount at once;
    the path that is now the drive's is attached once, where it was first. */
    assert_eq!(
        synced(Ok(Some("[automount] # moved\nroot = /windows # here\n"))),
        (
            mount("/windows/"),
            [false, true, false],
            paths(["e:/work", ""])
        )
    );
    /* The drives at /c, /d. */
    assert_eq!(
        synced(Ok(Some("[automount]\nroot = /\n"))),
        (
            mount("/"),
            [false, false, true],
            paths(["/windows/e/Work", ""])
        )
    );
    /* A file that can't be read just now changes nothing. */
    assert_eq!(
        synced(Err("no answer for 600 seconds")),
        (
            mount("/"),
            [false, false, true],
            paths(["/windows/e/Work", ""])
        )
    );
    assert_eq!(
        synced(Ok(Some("[boot]\nsystemd = true\n"))),
        (None, [true, false, false], paths(["/windows/e/Work", ""]))
    );
    assert_eq!(transport.read.load(Ordering::Relaxed), 5);

    /* Only a distro has the file read: anywhere else it would say nothing of drives. */
    let host = Elsewhere::new(&bench);
    host.sync_through(&bench, &transport);
    assert_eq!(transport.read.load(Ordering::Relaxed), 5);
}

#[test]
fn a_path_is_the_same_however_the_drive_is_written() {
    let mut bench = Bench::new(DbAgent::new());
    let under_mount = started(&mut bench.agent, "/mnt/d/Projects/app/src");
    bench.sync();
    let id = bench.session_id(&under_mount);
    let catalogue = bench.archive().catalogue();
    let first = catalogue.create_folder("first", None).unwrap();
    let second = catalogue.create_folder("second", None).unwrap();

    /* Typed with white space around it, and in the quotes a path copied from a file
    manager comes in: neither is part of the path. */
    catalogue
        .attach_path(first, "  \"D:\\Projects\\app\" \n", None)
        .unwrap();
    let paths = catalogue.folder_paths(first).unwrap();
    assert_eq!(
        (paths[0].attached.path.as_str(), paths[0].typed.as_str()),
        ("d:/projects/app", r"D:\Projects\app")
    );
    /* A session that recorded the drive's mount in a WSL distro is under it. */
    assert_eq!(filed(catalogue, &id), Filed::ByRule(first));

    /* The mount's form of the path, and the form Windows writes to have a path taken as it
    is, are the path that is attached already. */
    for same in [
        "/mnt/d/Projects/app",
        r"\\?\D:\Projects\app",
        "d:/projects/APP/",
    ] {
        assert!(
            matches!(
                catalogue.attach_path(second, same, None),
                Err(CatalogueError::PathAttached { folder, .. }) if folder == first
            ),
            "{same}"
        );
    }

    /* A path that does not say where it is from the top is no path to attach. */
    for relative in ["code/app", "~/code/app", "./app", "app"] {
        assert!(
            matches!(
                catalogue.attach_path(second, relative, None),
                Err(CatalogueError::NoPath(_))
            ),
            "{relative}"
        );
    }
    assert!(catalogue.folder_paths(second).unwrap().is_empty());
}

#[test]
fn what_is_not_there_is_said_not_passed_over() {
    let mut bench = Bench::new(DbAgent::new());
    let catalogue = bench.archive().catalogue();
    let folder = catalogue.create_folder("  work  ", None).unwrap();
    catalogue.create_folder("b", None).unwrap();
    catalogue.create_folder("c", None).unwrap();
    assert_eq!(outline(&catalogue.folders().unwrap(), None), "work b c");
    let missing = FolderId(folder.0 + 100);
    let no_folder = |result: Result<(), CatalogueError>| matches!(result, Err(CatalogueError::NoFolder(id)) if id == missing);

    /* A folder that is not there is not renamed, moved, deleted, put into or attached to. */
    assert!(no_folder(catalogue.rename_folder(missing, "x")));
    assert!(no_folder(catalogue.move_folder(missing, None, 0)));
    assert!(no_folder(catalogue.move_folder(folder, Some(missing), 0)));
    assert!(no_folder(catalogue.delete_folder(missing)));
    assert!(no_folder(catalogue.attach_path(
        missing,
        "/home/user/code",
        None
    )));
    assert!(no_folder(
        catalogue.create_folder("x", Some(missing)).map(drop)
    ));
    assert_eq!(outline(&catalogue.folders().unwrap(), None), "work b c");

    /* A name is some text, of no great length, and two folders may share one. */
    for name in ["", "   ", &"n".repeat(201)] {
        assert!(matches!(
            catalogue.create_folder(name, None),
            Err(CatalogueError::FolderName)
        ));
        assert!(matches!(
            catalogue.rename_folder(folder, name),
            Err(CatalogueError::FolderName)
        ));
    }
    let twin = catalogue.create_folder("work", Some(folder)).unwrap();
    catalogue.rename_folder(twin, &"n".repeat(200)).unwrap();
    catalogue.rename_folder(twin, " work ").unwrap();
    assert_eq!(
        outline(&catalogue.folders().unwrap(), None),
        "work(work) b c"
    );

    /* The folders a move leaves close up: the places among them have no gaps. */
    let top = "SELECT position FROM folder WHERE parent_id IS NULL ORDER BY position";
    let b = catalogue.folders().unwrap()[1].id;
    catalogue.move_folder(b, Some(folder), 0).unwrap();
    assert_eq!(
        outline(&catalogue.folders().unwrap(), None),
        "work(b work) c"
    );
    assert_eq!(read_catalogue(bench.dir(), top), ["0", "1"]);
    catalogue.move_folder(b, None, 1).unwrap();
    catalogue.move_folder(twin, None, 1).unwrap();
    assert_eq!(read_catalogue(bench.dir(), top), ["0", "1", "2", "3"]);
    catalogue.delete_folder(folder).unwrap();
    assert_eq!(outline(&catalogue.folders().unwrap(), None), "work b c");
    assert_eq!(read_catalogue(bench.dir(), top), ["0", "1", "2"]);

    /* Paths of a folder that is not there, and a path that is not attached. */
    assert!(matches!(
        catalogue.folder_paths(missing),
        Err(CatalogueError::NoFolder(_))
    ));
    catalogue
        .attach_path(twin, "/home/user/code/app", None)
        .unwrap();
    let attached = catalogue.folder_paths(twin).unwrap().remove(0).attached;
    catalogue.detach_path(&attached).unwrap();
    assert!(matches!(
        catalogue.detach_path(&attached),
        Err(CatalogueError::NotAttached(path)) if path == "/home/user/code/app"
    ));
    assert!(no_folder(catalogue.detach_path(&AttachedPath {
        folder: missing,
        ..attached
    })));

    /* Nothing is decided about a session of which nothing is archived, and nothing is kept
    of the attempt: the session that arrives under that identity starts as any other. */
    let key = started(&mut bench.agent, "/srv/x");
    let id = SessionId {
        location: bench.location.clone(),
        provider: bench.agent.provider().id().to_string(),
        key: key.strip_prefix("sessions/").unwrap().to_string(),
    };
    let catalogue = bench.archive().catalogue();
    let no_session = |result: Result<(), CatalogueError>| matches!(result, Err(CatalogueError::NoSession(not_there)) if not_there == id);
    assert!(no_session(catalogue.set_starred(&id, true)));
    assert!(no_session(catalogue.file_by_hand(&id, twin)));
    assert!(no_session(catalogue.file_automatically(&id).map(drop)));
    assert_eq!(
        read_catalogue(bench.dir(), "SELECT count(*) FROM session_state"),
        ["0"]
    );
    bench.sync();
    let session = bench.session(&key);
    assert_eq!((session.filed, session.starred), (Filed::Unsorted, false));
    assert!(matches!(
        bench.archive().catalogue().file_by_hand(&id, missing),
        Err(CatalogueError::NoFolder(_))
    ));
}

#[test]
fn a_session_the_parser_stops_on_stays_filed_and_is_filed_by_what_was_known() {
    let mut bench = Bench::new(DbAgent::new());
    let key = started(&mut bench.agent, "/home/user/code/app");
    let catalogue = bench.archive().catalogue();
    let app = catalogue.create_folder("app", None).unwrap();
    catalogue
        .attach_path(app, "/home/user/code/app", None)
        .unwrap();
    assert_eq!(bench.sync().index.filed, 1);
    let id = bench.session_id(&key);
    let before = bench.session(&key);
    assert_eq!(before.filed, Filed::ByRule(app));

    /* The session goes on, and the parser stops on it: it is where it was, with what was
    known of it. */
    bench.agent.say(&key, "user", "More.");
    let report = bench.synced_with(bench.meddling(Meddle::FaultyParser(key.clone())));
    assert_eq!((report.index.failed.len(), report.index.filed), (1, 0));
    let failed = bench.session(&key);
    assert!(failed.failure.is_some());
    assert_eq!(
        (&failed.title, &failed.project, failed.filed),
        (&before.title, &before.project, Filed::ByRule(app))
    );

    /* A change of paths files it like any other, by the path its row still has. */
    let catalogue = bench.archive().catalogue();
    let attached = catalogue.folder_paths(app).unwrap().remove(0).attached;
    catalogue.detach_path(&attached).unwrap();
    assert_eq!(filed(catalogue, &id), Filed::Unsorted);
    let code = catalogue.create_folder("code", None).unwrap();
    catalogue
        .attach_path(code, "/home/user/code", None)
        .unwrap();
    assert_eq!(filed(catalogue, &id), Filed::ByRule(code));
}
