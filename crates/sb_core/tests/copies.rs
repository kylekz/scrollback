//! Copies of a session kept on another machine (PLAN §7.7): an app's copy of a session that
//! ran at another location is archived where it was found and shown once, as its original
//! when that is archived, as the copy until then, and never as a session of the machine that
//! kept it. The copies here are the first provider's desktop app's (PLAN §7.3), generated.

use std::fs;
use std::slice;
use std::time::{Duration, SystemTime};

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::archive::Archive;
use sb_core::catalogue::{
    Catalogue, Compared, Filed, Kin, LibraryCounts, Session, SessionId, Shelf,
};
use sb_core::filing::FolderId;
use sb_core::location::{LocationId, LocationKind};
use sb_core::provider;
use sb_core::search::{Filters, Limit, Query, Searched};
use sb_core::sync::sync_location;
use sb_core::transcript::Subagent;
use sb_test_support::{DesktopCopy, ProjectsTree, Session as Written};
use tempfile::TempDir;

const PROJECT: &str = "/home/user/code/app";

/// An archive with two locations: this machine, whose agent's app keeps copies of sessions it
/// ran elsewhere, and a distro, where those sessions ran.
struct Machines {
    _dir: TempDir,
    archive: Archive,
    here: LocationId,
    distro: LocationId,
    /// This machine's agent data, and the distro's.
    pc: ProjectsTree,
    remote: ProjectsTree,
}

impl Machines {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::create(dir.path()).unwrap();
        let catalogue = archive.catalogue();
        let here = catalogue
            .add_location(LocationKind::Local, "This PC")
            .unwrap()
            .id;
        let distro = catalogue
            .add_location(
                LocationKind::Wsl {
                    distro: "Ubuntu-24.04".to_string(),
                },
                "Ubuntu-24.04",
            )
            .unwrap()
            .id;
        Self {
            _dir: dir,
            archive,
            here,
            distro,
            pc: ProjectsTree::new(),
            remote: ProjectsTree::new(),
        }
    }

    fn catalogue(&self) -> &Catalogue {
        self.archive.catalogue()
    }

    /// A session that ran on the distro, with a prompt that names `word` and a subagent.
    fn ran_remotely(&mut self, word: &str) -> Written {
        let mut session = self.remote.session(PROJECT, 2);
        session
            .transcript
            .prompt(&format!("Look into the {word} module"));
        session.transcript.reply("Looking.");
        session.subagent(2);
        session
    }

    /// Syncs this machine, and with `distro` the distro too, each with every provider.
    fn sync(&self, distro: bool) {
        let mut places = vec![(&self.here, self.pc.transport())];
        if distro {
            places.push((&self.distro, self.remote.transport()));
        }
        for (location, transport) in places {
            for pass in sync_location(
                &self.archive,
                location,
                &transport,
                &provider::builtin(),
                &CancelToken::new(),
                &mut |_| {},
            )
            .unwrap()
            {
                let report = pass.outcome.unwrap();
                assert!(report.failed.is_empty(), "{:?}", report.failed);
                pass.index.unwrap();
            }
        }
    }

    fn shown(&self, shelf: Shelf) -> Vec<Session> {
        self.catalogue().sessions(shelf).unwrap()
    }

    /// The one session shown on All.
    fn the_one(&self) -> Session {
        let mut shown = self.shown(Shelf::All);
        assert_eq!(shown.len(), 1, "{shown:#?}");
        shown.remove(0)
    }

    fn id(&self, location: &LocationId, key: &str) -> SessionId {
        SessionId {
            location: location.clone(),
            provider: provider::builtin()[0].id().as_str().to_string(),
            key: key.to_string(),
        }
    }

    fn original(&self, session: &Written) -> SessionId {
        self.id(&self.distro, session.session_key())
    }

    fn copy(&self, copy: &DesktopCopy) -> SessionId {
        self.id(&self.here, &copy.folder)
    }

    fn counts(&self) -> LibraryCounts {
        self.catalogue().library_counts().unwrap()
    }

    /// How many sessions each location holds, as the locations sheet counts them.
    fn held(&self) -> Vec<(LocationId, usize)> {
        let mut held: Vec<(LocationId, usize)> = (self.catalogue().holdings().unwrap())
            .into_iter()
            .map(|holding| (holding.location, holding.sessions))
            .collect();
        held.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        held
    }

    /// The sessions a search for `term` finds, with `filters`.
    fn found(&self, term: &str, filters: Filters) -> Vec<SessionId> {
        let query = Query {
            terms: vec![term.to_string()],
            filters,
        };
        match self
            .archive
            .search(&query, Limit::default(), &CancelToken::new())
            .unwrap()
        {
            Searched::Results(results) => results
                .sessions
                .into_iter()
                .map(|found| found.session.id)
                .collect(),
            Searched::KeepTyping => panic!("{term} is long enough to look for"),
        }
    }
}

/// Rewrites the file at `key` of `tree` with `from` replaced by `to`, as written an hour ago.
fn rewrite(tree: &ProjectsTree, key: &str, from: &str, to: &str) {
    let path = tree.path(key);
    let text = fs::read_to_string(&path).unwrap().replace(from, to);
    fs::write(&path, text).unwrap();
    tree.set_modified(key, SystemTime::now() - Duration::from_secs(3600));
}

#[test]
fn an_identical_copy_is_shown_once_as_its_original() {
    let mut machines = Machines::new();
    let session = machines.ran_remotely("upload");
    let copy = machines.pc.desktop_copy(&session);
    machines.sync(true);

    let shown = machines.the_one();
    assert_eq!(shown.id, machines.original(&session));
    assert_eq!(
        (&shown.copy, shown.ran_at()),
        (&None, Some(&machines.distro))
    );
    /* The copy is archived, and hidden. */
    let copy_id = machines.copy(&copy);
    let hidden = machines.catalogue().session(&copy_id).unwrap().unwrap();
    assert!(hidden.copy.is_some() && hidden.ran_at().is_none());
    assert_eq!(
        machines.catalogue().kin(&shown.id).unwrap(),
        [Kin {
            id: copy_id.clone(),
            copy: hidden.copy.clone(),
            compared: Compared::Same,
        }]
    );
    assert_eq!(
        machines.catalogue().kin(&copy_id).unwrap(),
        [Kin {
            id: shown.id.clone(),
            copy: None,
            compared: Compared::Same,
        }]
    );
    /* Counted once, and only at the location it ran at. */
    let counts = machines.counts();
    assert_eq!((counts.all, counts.unsorted, counts.trash), (1, 1, 0));
    assert_eq!(machines.held(), {
        let mut held = vec![(machines.here.clone(), 0), (machines.distro.clone(), 1)];
        held.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        held
    });
    /* Found once. */
    assert_eq!(
        machines.found("upload", Filters::default()),
        slice::from_ref(&shown.id)
    );
    assert_eq!(machines.catalogue().unsorted().unwrap().len(), 1);
}

#[test]
fn a_copy_behind_or_ahead_of_its_original_is_compared_and_hidden() {
    let mut machines = Machines::new();
    let behind = machines.ran_remotely("upload");
    machines.pc.desktop_copy(&behind);
    let mut behind = behind;
    behind.append(3);
    let ahead = machines.ran_remotely("fetcher");
    let mut copy = machines.pc.desktop_copy(&ahead);
    copy.transcript.prompt("And the retries too");
    machines.sync(true);

    let shown = machines.shown(Shelf::All);
    assert_eq!(shown.len(), 2);
    assert!(shown.iter().all(|session| session.copy.is_none()));
    let compared = |session: &Written| {
        let kin = machines
            .catalogue()
            .kin(&machines.original(session))
            .unwrap();
        assert_eq!(kin.len(), 1);
        kin[0].compared
    };
    assert_eq!(compared(&behind), Compared::Shorter);
    assert_eq!(compared(&ahead), Compared::Longer);
}

#[test]
fn a_copy_with_no_original_is_shown_as_a_copy_from_no_location() {
    let mut machines = Machines::new();
    let session = machines.ran_remotely("upload");
    let copy = machines.pc.desktop_copy(&session);
    machines.sync(false);

    let shown = machines.the_one();
    assert_eq!(shown.id, machines.copy(&copy));
    let label = shown.copy.clone().unwrap().label;
    assert_eq!(shown.ran_at(), None);
    assert_eq!(shown.project_path.as_deref(), Some(PROJECT));
    /* This machine holds no session of its own. */
    assert!(machines.held().iter().all(|(_, sessions)| *sessions == 0));
    /* Unsorted groups it apart, with no location. */
    let groups = machines.catalogue().unsorted().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(
        (groups[0].locations.as_slice(), groups[0].copy.as_deref()),
        (&[][..], Some(label.as_str()))
    );
    /* The paths editor offers its path for no location. */
    let paths = machines.catalogue().project_paths().unwrap();
    assert_eq!(paths.len(), 1);
    assert_eq!(
        (paths[0].location.as_ref(), paths[0].copy.as_deref()),
        (None, Some(label.as_str()))
    );
    /* A location's filter never finds it; the search finds it unfiltered. */
    let from_here = Filters {
        location: Some(machines.here.clone()),
        ..Filters::default()
    };
    assert_eq!(machines.found("upload", from_here), []);
    assert_eq!(
        machines.found("upload", Filters::default()),
        slice::from_ref(&shown.id)
    );
    /* A path attached for this machine doesn't file it; one attached for every location does. */
    let catalogue = machines.catalogue();
    let folder = catalogue.create_folder("app", None).unwrap();
    catalogue
        .attach_path(folder, PROJECT, Some(&machines.here))
        .unwrap();
    assert_eq!(
        catalogue.session(&shown.id).unwrap().unwrap().filed,
        Filed::Unsorted
    );
    let other = catalogue.create_folder("everywhere", None).unwrap();
    catalogue.attach_path(other, PROJECT, None).unwrap();
    assert_eq!(
        catalogue.session(&shown.id).unwrap().unwrap().filed,
        Filed::ByRule(other)
    );
    assert_eq!(catalogue.fileable().unwrap()[0].location, None);
}

#[test]
fn the_original_arriving_later_takes_the_copys_place_and_what_was_decided() {
    let mut machines = Machines::new();
    let session = machines.ran_remotely("upload");
    let copy = machines.pc.desktop_copy(&session);
    machines.sync(false);
    let copy_id = machines.copy(&copy);
    let catalogue = machines.catalogue();
    let folder: FolderId = catalogue.create_folder("kept", None).unwrap();
    catalogue.file_by_hand(&copy_id, folder).unwrap();
    catalogue.set_starred(&copy_id, true).unwrap();

    machines.sync(true);
    let shown = machines.the_one();
    assert_eq!(shown.id, machines.original(&session));
    assert_eq!((shown.filed, shown.starred), (Filed::ByHand(folder), true));
    let counts = machines.counts();
    assert_eq!(
        (counts.all, counts.starred, counts.folders.get(&folder)),
        (1, 1, Some(&1))
    );
    assert_eq!(
        machines.found("upload", Filters::default()),
        slice::from_ref(&shown.id)
    );
}

#[test]
fn a_copys_loose_subagents_are_its_own_and_placed_under_their_calls() {
    let mut machines = Machines::new();
    let session = machines.ran_remotely("upload");
    let copy = machines.pc.desktop_copy(&session);
    assert_eq!(copy.subagents.len(), 1);
    machines.sync(false);

    let id = machines.copy(&copy);
    let items = machines.archive.session(&id).unwrap();
    let provider = &provider::builtin()[0];
    let mut transcript = provider.parse(&items);
    transcript.read_subagents(provider.as_ref(), &items);
    assert!(
        transcript.unattached.is_empty(),
        "{:?}",
        transcript.unattached
    );
    let placed: Vec<&Subagent> = transcript
        .conversation
        .as_ref()
        .unwrap()
        .calls()
        .flat_map(|(_, call)| &call.subagents)
        .collect();
    assert_eq!(placed.len(), 1);
    assert_eq!(&**placed[0].item(), copy.subagents[0].as_str());
    assert!(matches!(placed[0], Subagent::Read { .. }));
    /* What the subagent did is the session's. */
    let shown = machines.the_one();
    assert!(shown.all.calls > shown.own.calls);
}

#[test]
fn trash_hides_the_session_and_its_copy_and_delete_for_good_takes_both() {
    let mut machines = Machines::new();
    let session = machines.ran_remotely("upload");
    let copy = machines.pc.desktop_copy(&session);
    machines.sync(true);
    let (original, copy_id) = (machines.original(&session), machines.copy(&copy));
    let catalogue = machines.catalogue();

    catalogue.trash(&original).unwrap();
    assert!(machines.shown(Shelf::All).is_empty());
    assert_eq!(
        machines
            .shown(Shelf::Trash)
            .into_iter()
            .map(|session| session.id)
            .collect::<Vec<_>>(),
        slice::from_ref(&original)
    );
    let counts = machines.counts();
    assert_eq!((counts.all, counts.trash), (0, 1));

    let deletion = catalogue.deletion(&original).unwrap();
    assert_eq!(deletion.copies.len(), 1);
    assert_eq!(deletion.copies[0].session, copy_id);
    assert!(deletion.copies[0].files >= 2);
    machines.archive.delete_for_good(&deletion).unwrap();
    machines.archive.wait_for_removals();
    for id in [&original, &copy_id] {
        assert_eq!(catalogue.session(id).unwrap(), None);
    }
    assert_eq!(catalogue.tombstones().unwrap().len(), 2);
    assert!(machines.shown(Shelf::All).is_empty() && machines.shown(Shelf::Trash).is_empty());
    /* Neither comes back with the next syncs. */
    machines.sync(true);
    assert!(machines.shown(Shelf::All).is_empty());
    assert_eq!(machines.found("upload", Filters::default()), []);
}

#[test]
fn deleting_one_of_two_originals_leaves_the_copy_hidden_behind_the_other() {
    let mut machines = Machines::new();
    let session = machines.ran_remotely("upload");
    machines.pc.desktop_copy(&session);
    /* The same session archived from a second location: the same distro reached over SSH. */
    let host = machines
        .catalogue()
        .add_location(
            LocationKind::Ssh {
                host: "user@macbook".to_string(),
            },
            "user@macbook",
        )
        .unwrap()
        .id;
    machines.sync(true);
    sync_location(
        &machines.archive,
        &host,
        &machines.remote.transport(),
        &provider::builtin(),
        &CancelToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(machines.shown(Shelf::All).len(), 2);
    let original = machines.original(&session);
    machines.catalogue().trash(&original).unwrap();
    assert_eq!(machines.catalogue().deletion(&original).unwrap().copies, []);
}

#[test]
fn a_copy_of_another_conversation_under_the_same_id_is_not_merged() {
    let mut machines = Machines::new();
    let session = machines.ran_remotely("upload");
    let copy = machines.pc.desktop_copy(&session);
    /* The same id, started in another directory: another session. */
    rewrite(&machines.pc, &copy.key, PROJECT, "/home/user/code/other");
    machines.sync(true);

    let shown = machines.shown(Shelf::All);
    assert_eq!(shown.len(), 2, "{shown:#?}");
    assert!(
        machines
            .catalogue()
            .kin(&machines.original(&session))
            .unwrap()
            .is_empty()
    );
    assert_eq!(machines.counts().all, 2);
}

#[test]
fn of_two_copies_with_no_original_the_one_active_last_is_shown() {
    let mut machines = Machines::new();
    let session = machines.ran_remotely("upload");
    machines.pc.desktop_copy(&session);
    let mut later = machines.pc.desktop_copy(&session);
    later.transcript.prompt("One more thing");
    machines.sync(false);

    assert_eq!(machines.the_one().id, machines.copy(&later));
    assert_eq!(machines.counts().all, 1);
}
