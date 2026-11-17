//! Copies of a session kept on another machine (PLAN §7.7): an app's copy of a session that
//! ran at another location is archived where it was found and shown once, as its original
//! when that is archived, as the copy until then, and never as a session of the machine that
//! kept it. The copies here are the first provider's desktop app's (PLAN §7.3), generated.

use std::fs;
use std::path::Path;
use std::slice;
use std::time::{Duration, SystemTime};

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::archive::{Archive, ArchiveError};
use sb_core::catalogue::{
    Catalogue, CatalogueError, Compared, Filed, Kin, LibraryCounts, Session, SessionId, Shelf,
};
use sb_core::filing::FolderId;
use sb_core::location::{LocationId, LocationKind};
use sb_core::provider;
use sb_core::search::{Filters, Limit, Query, Searched};
use sb_core::sync::{IndexReport, index, sync_location};
use sb_core::transcript::Subagent;
use sb_core::transport::Transport;
use sb_test_support::{
    DesktopCopy, ProjectsTree, Session as Written, edit_catalogue, read_catalogue,
};
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

    /// The archive's folder.
    fn dir(&self) -> &Path {
        self._dir.path()
    }

    /// Syncs this machine, and with `distro` the distro too, each with every provider.
    fn sync(&self, distro: bool) {
        self.sync_at(&self.here, &self.pc.transport());
        if distro {
            self.sync_at(&self.distro, &self.remote.transport());
        }
    }

    /// Syncs `location` through `transport` with every provider. Returns what each pass's
    /// index did.
    fn sync_at(&self, location: &LocationId, transport: &dyn Transport) -> Vec<IndexReport> {
        let mut indexed = Vec::new();
        for pass in sync_location(
            &self.archive,
            location,
            transport,
            &provider::builtin(),
            &CancelToken::new(),
            &mut |_| {},
        )
        .unwrap()
        {
            let report = pass.outcome.unwrap();
            assert!(report.failed.is_empty(), "{:?}", report.failed);
            indexed.push(pass.index.unwrap());
        }
        indexed
    }

    /// Indexes what is out of date, as a start does.
    fn index(&self) {
        index(
            &self.archive,
            None,
            &provider::builtin(),
            &CancelToken::new(),
            &mut |_| {},
        )
        .unwrap();
    }

    /// A host over SSH, as a third location.
    fn add_host(&self) -> LocationId {
        self.catalogue()
            .add_location(
                LocationKind::Ssh {
                    host: "user@macbook".to_string(),
                },
                "user@macbook",
            )
            .unwrap()
            .id
    }

    /// Moves the session `id` to Trash and deletes it for good, with what goes with it.
    fn delete(&self, id: &SessionId) {
        self.catalogue().trash(id).unwrap();
        let deletion = self.catalogue().deletion(id).unwrap();
        self.archive.delete_for_good(&deletion).unwrap();
        self.archive.wait_for_removals();
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
        machines.archive.kin(&shown.id).unwrap(),
        [Kin {
            id: copy_id.clone(),
            copy: hidden.copy.clone(),
            compared: Compared::Same,
        }]
    );
    assert_eq!(
        machines.archive.kin(&copy_id).unwrap(),
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
        let kin = machines.archive.kin(&machines.original(session)).unwrap();
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
    assert_eq!(deletion.also.len(), 1);
    assert_eq!(deletion.also[0].session, copy_id);
    assert!(deletion.also[0].copy.is_some() && deletion.also[0].at_location);
    assert!(deletion.also[0].files >= 2);
    machines.archive.delete_for_good(&deletion).unwrap();
    machines.archive.wait_for_removals();
    for id in [&original, &copy_id] {
        assert_eq!(catalogue.session(id).unwrap(), None);
    }
    /* Settings lists the copy under its original, and allows both again at once. */
    let tombstones = catalogue.tombstones().unwrap();
    assert_eq!(tombstones.len(), 1);
    assert_eq!(
        (&tombstones[0].session, tombstones[0].copy.as_deref()),
        (&original, None)
    );
    assert_eq!(tombstones[0].with.len(), 1);
    assert_eq!(tombstones[0].with[0].session, copy_id);
    assert!(tombstones[0].with[0].copy.is_some());
    let allowed = catalogue.remove_tombstone(&original).unwrap();
    assert_eq!(catalogue.tombstones().unwrap(), []);
    machines.archive.restore_tombstone(&allowed).unwrap();
    assert_eq!(catalogue.tombstones().unwrap(), tombstones);
    assert!(machines.shown(Shelf::All).is_empty() && machines.shown(Shelf::Trash).is_empty());
    /* Neither comes back with the next syncs. */
    machines.sync(true);
    assert!(machines.shown(Shelf::All).is_empty());
    assert_eq!(machines.found("upload", Filters::default()), []);
}

#[test]
fn deleting_the_original_also_takes_the_same_session_from_another_location() {
    let mut machines = Machines::new();
    let session = machines.ran_remotely("upload");
    machines.pc.desktop_copy(&session);
    /* The same session archived from a second location: the same distro reached over SSH. */
    let host = machines.add_host();
    machines.sync(true);
    machines.sync_at(&host, &machines.remote.transport());
    assert_eq!(machines.shown(Shelf::All).len(), 2);
    let original = machines.original(&session);
    machines.catalogue().trash(&original).unwrap();
    /* Only what is in Trash is deleted for good (D14): the same session from the host is
    still in the library, so it goes to Trash first. */
    let from_host = machines.id(&host, session.session_key());
    assert!(matches!(
        machines.catalogue().deletion(&original),
        Err(CatalogueError::AlsoShown(also)) if also == from_host
    ));
    machines.catalogue().trash(&from_host).unwrap();
    let deletion = machines.catalogue().deletion(&original).unwrap();
    let also: Vec<(&LocationId, bool)> = (deletion.also.iter())
        .map(|also| (&also.session.location, also.copy.is_some()))
        .collect();
    assert_eq!(also, [(&host, false), (&machines.here, true)]);
    machines.archive.delete_for_good(&deletion).unwrap();
    assert!(machines.shown(Shelf::All).is_empty());
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
            .archive
            .kin(&machines.original(&session))
            .unwrap()
            .is_empty()
    );
    assert_eq!(machines.counts().all, 2);
}

/// Of two lone copies the one archived first is shown, and stays shown as the other grows.
#[test]
fn of_two_copies_with_no_original_the_one_shown_stays_shown() {
    let mut machines = Machines::new();
    let session = machines.ran_remotely("upload");
    let first = machines.pc.desktop_copy(&session);
    let mut later = machines.pc.desktop_copy(&session);
    later.transcript.prompt("One more thing");
    machines.sync(false);
    assert_eq!(machines.the_one().id, machines.copy(&first));
    assert_eq!(machines.counts().all, 1);

    later.transcript.prompt("And another");
    machines.sync(false);
    assert_eq!(machines.the_one().id, machines.copy(&first));

    /* What the owner decides of the one shown keeps it shown, in Trash too. */
    machines.catalogue().trash(&machines.copy(&first)).unwrap();
    later.transcript.prompt("And a third");
    machines.sync(false);
    assert!(machines.shown(Shelf::All).is_empty());
    assert_eq!(machines.shown(Shelf::Trash)[0].id, machines.copy(&first));
}

/// A copy in Trash whose original arrives: the original is in Trash, and the copy has nothing
/// decided of it left.
#[test]
fn an_original_takes_a_shown_copys_trash_and_the_copy_keeps_none() {
    let mut machines = Machines::new();
    let session = machines.ran_remotely("upload");
    let copy = machines.pc.desktop_copy(&session);
    machines.sync(false);
    let copy_id = machines.copy(&copy);
    machines.catalogue().trash(&copy_id).unwrap();

    machines.sync(true);
    let original = machines.original(&session);
    let trash = machines.shown(Shelf::Trash);
    assert_eq!(trash.len(), 1);
    assert_eq!(trash[0].id, original);
    assert!(machines.shown(Shelf::All).is_empty());
    let left = read_catalogue(
        machines.dir(),
        &format!(
            "SELECT count(*) FROM session_state WHERE session = '{}'",
            copy_id.key
        ),
    );
    assert_eq!(left, ["0"]);
}

/**
 * An archive from before copies were known: the owner trashed the duplicate a desktop copy made
 * and kept its original. Neither the index that knows copies now nor a rebuild of the index
 * moves that Trash onto the original.
 */
#[test]
fn a_duplicate_trashed_before_copies_were_known_leaves_its_original_alone() {
    let mut machines = Machines::new();
    let session = machines.ran_remotely("upload");
    let copy = machines.pc.desktop_copy(&session);
    machines.sync(true);
    let (original, copy_id) = (machines.original(&session), machines.copy(&copy));
    /* As 0.1.0 left it: the copy a session of this machine, in Trash; nothing decided of its
    original. */
    edit_catalogue(
        machines.dir(),
        &format!(
            "UPDATE session SET copy_label = NULL, copy_origin = NULL WHERE session = '{0}';
             INSERT INTO session_state (location_id, provider, session, trashed_at)
             VALUES ('{1}', '{2}', '{0}', 1);
             UPDATE session SET index_version = 2;
             UPDATE session SET ended_at = ended_at + 1000 WHERE session = '{0}';",
            copy_id.key,
            copy_id.location.as_str(),
            copy_id.provider
        ),
    );
    machines.index();
    assert_eq!(machines.the_one().id, original);
    machines.catalogue().drop_index().unwrap();
    machines.index();
    let shown = machines.the_one();
    assert_eq!((&shown.id, shown.trashed), (&original, None));
}

/**
 * An original whose first row was made without a parse (its items could not be read), then
 * read: it is new to its conversation then, and takes over what was decided of the copy shown
 * in its place.
 */
#[test]
fn an_original_read_after_a_first_row_without_a_conversation_takes_over() {
    let mut machines = Machines::new();
    let session = machines.ran_remotely("upload");
    let copy = machines.pc.desktop_copy(&session);
    machines.sync(true);
    let (original, copy_id) = (machines.original(&session), machines.copy(&copy));
    /* As if the original's first index could not read it: no conversation, to be read again;
    and the copy trashed while shown alone. */
    edit_catalogue(
        machines.dir(),
        &format!(
            "UPDATE session SET conversation = NULL, retry = 1, indexed_from = hex(zeroblob(32))
             WHERE location_id = '{0}';
             DELETE FROM session_state;
             INSERT INTO session_state (location_id, provider, session, trashed_at, as_copy)
             VALUES ('{1}', '{2}', '{3}', 1, 1);",
            original.location.as_str(),
            copy_id.location.as_str(),
            copy_id.provider,
            copy_id.key
        ),
    );
    assert_eq!(machines.shown(Shelf::Trash)[0].id, copy_id);
    machines.index();
    let trashed = machines.shown(Shelf::Trash);
    assert_eq!(
        trashed.iter().map(|it| &it.id).collect::<Vec<_>>(),
        [&original]
    );
    let copy_state = read_catalogue(
        machines.dir(),
        &format!(
            "SELECT count(*) FROM session_state WHERE session = '{}'",
            copy_id.key
        ),
    );
    assert_eq!(copy_state, ["0"]);
}

/// A session deleted for good before its copy was archived: the copy is not archived after.
#[test]
fn a_copy_arriving_after_its_original_was_deleted_for_good_is_deleted_too() {
    let mut machines = Machines::new();
    let session = machines.ran_remotely("upload");
    machines.sync(true);
    let original = machines.original(&session);
    machines.delete(&original);

    let copy = machines.pc.desktop_copy(&session);
    /* The index that met it deleted it for good, and says so. */
    let indexed = machines.sync_at(&machines.here, &machines.pc.transport());
    assert_eq!(indexed[0].deleted, [machines.copy(&copy)]);
    machines.archive.wait_for_removals();
    assert!(machines.shown(Shelf::All).is_empty() && machines.shown(Shelf::Trash).is_empty());
    assert_eq!(
        machines.catalogue().session(&machines.copy(&copy)).unwrap(),
        None
    );
    assert_eq!(machines.found("upload", Filters::default()), []);
    /* Its keys are tombstoned: the next sync takes nothing of it, nor of the original. */
    machines.sync(true);
    assert!(machines.shown(Shelf::All).is_empty());
    machines.sync(false);
    assert!(
        (machines
            .catalogue()
            .items(&machines.here, &original.provider)
            .unwrap())
        .iter()
        .all(|item| !item.key.contains(&copy.folder))
    );
}

/// A lone copy deleted for good, then its machine added: the original is not archived.
#[test]
fn an_original_arriving_after_its_copy_was_deleted_for_good_is_deleted_too() {
    let mut machines = Machines::new();
    let session = machines.ran_remotely("upload");
    let copy = machines.pc.desktop_copy(&session);
    machines.sync(false);
    machines.delete(&machines.copy(&copy));

    machines.sync(true);
    machines.archive.wait_for_removals();
    assert!(machines.shown(Shelf::All).is_empty());
    assert_eq!(
        machines
            .catalogue()
            .session(&machines.original(&session))
            .unwrap(),
        None
    );
}

/// While the index of an upgrade has not reached a session's copies, they don't say what they
/// are copies of: deleting the session for good waits for it.
#[test]
fn delete_for_good_waits_for_the_conversation_to_be_indexed() {
    let mut machines = Machines::new();
    let session = machines.ran_remotely("upload");
    let copy = machines.pc.desktop_copy(&session);
    machines.sync(true);
    let original = machines.original(&session);
    machines.catalogue().trash(&original).unwrap();
    /* As an older index left the copy's row. */
    edit_catalogue(
        machines.dir(),
        &format!(
            "UPDATE session SET conversation = NULL, copy_label = NULL, copy_origin = NULL,
                 index_version = 2
             WHERE session = '{}'",
            machines.copy(&copy).key
        ),
    );
    assert!(matches!(
        machines.catalogue().deletion(&original),
        Err(CatalogueError::StillIndexing(_))
    ));
    machines.index();
    assert_eq!(
        machines.catalogue().deletion(&original).unwrap().also.len(),
        1
    );
}

/// A copy folder holding two transcripts, of which only the first is read, is not deleted.
#[test]
fn a_copy_with_two_transcripts_is_not_deleted_for_good() {
    let mut machines = Machines::new();
    let session = machines.ran_remotely("upload");
    let copy = machines.pc.desktop_copy(&session);
    let other = machines.remote.session(PROJECT, 2);
    let second = format!("projects/{}/{}.jsonl", copy.folder, other.id);
    fs::copy(machines.remote.path(&other.key), machines.pc.path(&second)).unwrap();
    machines
        .pc
        .set_modified(&second, SystemTime::now() - Duration::from_secs(3600));
    machines.sync(false);
    let copy_id = machines.copy(&copy);
    machines.catalogue().trash(&copy_id).unwrap();
    assert!(matches!(
        machines.catalogue().deletion(&copy_id),
        Err(CatalogueError::SeveralTranscripts(_))
    ));
}

/**
 * A subagent started after where a copy that is behind its original stops: the desktop app
 * keeps its transcript all the same, and no call of the copy's started it. It is the copy's,
 * unattached, read on its own.
 */
#[test]
fn a_loose_subagent_past_where_a_copy_stops_is_unattached() {
    let mut machines = Machines::new();
    let mut session = machines.ran_remotely("upload");
    let mut copy = machines.pc.desktop_copy(&session);
    let later = session.subagent(2);
    machines.pc.copy_subagents(&mut copy, &session);
    machines.sync(false);

    let id = machines.copy(&copy);
    let items = machines.archive.session(&id).unwrap();
    let provider = &provider::builtin()[0];
    let mut transcript = provider.parse(&items);
    transcript.read_subagents(provider.as_ref(), &items);
    let unattached: Vec<&str> = (transcript.unattached.iter())
        .map(|subagent| &**subagent.item())
        .collect();
    let name = later.rsplit('/').next().unwrap();
    assert_eq!(unattached.len(), 1);
    assert!(unattached[0].ends_with(name), "{unattached:?}");
    assert!(matches!(transcript.unattached[0], Subagent::Read { .. }));
}

/// A copy as long as it is shorter than its original, but not its start: it differs.
#[test]
fn a_shorter_copy_that_is_not_the_originals_start_differs() {
    let mut machines = Machines::new();
    let mut session = machines.ran_remotely("upload");
    let copy = machines.pc.desktop_copy(&session);
    rewrite(&machines.pc, &copy.key, "Looking.", "Lookin'.");
    session.append(2);
    machines.sync(true);
    let kin = machines.archive.kin(&machines.original(&session)).unwrap();
    assert_eq!(kin[0].compared, Compared::Differs);
}

/// A copy's hidden twin counts nowhere: not in the paths editor's preview, nor in Settings.
#[test]
fn hidden_copies_are_counted_nowhere() {
    let mut machines = Machines::new();
    let session = machines.ran_remotely("upload");
    machines.pc.desktop_copy(&session);
    machines.sync(true);
    let catalogue = machines.catalogue();
    assert_eq!(catalogue.fileable().unwrap().len(), 1);
    assert_eq!(catalogue.totals().unwrap().sessions, 1);
    let original = machines.original(&session);
    assert!(catalogue.shown_session(&original).unwrap().is_some());
    let copy = &machines.archive.kin(&original).unwrap()[0].id;
    assert_eq!(catalogue.shown_session(copy).unwrap(), None);
}

/**
 * An archive from before copies were classified, whose desktop copy the owner deleted for
 * good then (its transcript under the session `ssh-…/<id>`), and whose loose subagent
 * transcripts, then no session's, stayed: classified again, they are not a session brought
 * back, and a sync brings back none of it.
 */
#[test]
fn a_copy_deleted_before_copies_were_known_is_not_brought_back_by_its_subagents() {
    let mut machines = Machines::new();
    let session = machines.ran_remotely("upload");
    let copy = machines.pc.desktop_copy(&session);
    machines.sync(false);
    let provider = provider::builtin()[0].id().as_str();
    let old = format!("{}/{}", copy.folder, session.id);
    let location = machines.here.as_str().to_string();
    /* As the first classification and a delete for good of 0.1.0 left it. */
    edit_catalogue(
        machines.dir(),
        &format!(
            "DELETE FROM session;
             DELETE FROM session_state;
             UPDATE item SET kind = 'sidecar', session = NULL
                 WHERE key LIKE 'projects/{folder}/agent-%';
             DELETE FROM segment WHERE version_id IN (
                 SELECT v.id FROM version v JOIN item i ON i.id = v.item_id WHERE i.key = '{key}');
             DELETE FROM version WHERE item_id IN (SELECT id FROM item WHERE key = '{key}');
             DELETE FROM item WHERE key = '{key}';
             INSERT INTO tombstone (location_id, provider, session, deleted_at)
                 VALUES ('{location}', '{provider}', '{old}', 1);
             INSERT INTO tombstone_item (location_id, provider, key, session)
                 VALUES ('{location}', '{provider}', '{key}', '{old}');
             UPDATE location_provider SET classified_by = 1;",
            folder = copy.folder,
            key = copy.key,
        ),
    );
    machines.index();
    machines.archive.wait_for_removals();
    assert!(machines.shown(Shelf::All).is_empty());
    assert!(
        (machines
            .catalogue()
            .items(&machines.here, provider)
            .unwrap())
        .iter()
        .all(|item| !item.key.contains(&copy.folder))
    );
    machines.sync(false);
    assert!(machines.shown(Shelf::All).is_empty());
}

/// Undoing Allow puts back the tombstones of every session of the conversation, so it waits
/// while any of their locations is syncing, not only the first one's.
#[test]
fn undoing_allow_waits_for_the_sync_of_any_location_of_the_conversation() {
    let mut machines = Machines::new();
    let session = machines.ran_remotely("upload");
    machines.pc.desktop_copy(&session);
    machines.sync(true);
    let original = machines.original(&session);
    machines.delete(&original);
    let allowed = machines.catalogue().remove_tombstone(&original).unwrap();
    assert_eq!(allowed.tombstone.with.len(), 1);

    /* This machine, which kept the copy, syncs while the owner undoes. */
    let mut refused = None;
    sync_location(
        &machines.archive,
        &machines.here,
        &machines.pc.transport(),
        &provider::builtin(),
        &CancelToken::new(),
        &mut |_| {
            refused.get_or_insert_with(|| machines.archive.restore_tombstone(&allowed));
        },
    )
    .unwrap();
    assert!(matches!(refused, Some(Err(ArchiveError::Syncing(_)))));
}

/// A row of the provider at the session's project path made by an older parser may not say
/// which conversation it is: deleting for good waits for it to be made again.
#[test]
fn delete_for_good_waits_for_rows_an_older_parser_made() {
    let mut machines = Machines::new();
    let session = machines.ran_remotely("upload");
    machines.pc.desktop_copy(&session);
    machines.sync(true);
    let original = machines.original(&session);
    machines.catalogue().trash(&original).unwrap();
    edit_catalogue(
        machines.dir(),
        &format!(
            "UPDATE session SET parser_version = parser_version - 1 WHERE location_id = '{}'",
            machines.here.as_str()
        ),
    );
    assert!(matches!(
        machines.catalogue().deletion(&original),
        Err(CatalogueError::StillIndexing(_))
    ));
}
