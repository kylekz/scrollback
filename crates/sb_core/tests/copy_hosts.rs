//! Where a lone copy ran (PLAN §7.7): the records an app keeps beside its copies name the host
//! a session ran on over SSH, which this machine's `ssh` resolves, and a copy whose host is an
//! SSH location's is that location's in the library until its original is archived. The
//! records and the hosts are generated; `ssh -G` is a stand-in answering from a table.

use std::collections::HashMap;

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::archive::Archive;
use sb_core::catalogue::{Catalogue, Filed, Origin, Session, SessionId, Shelf};
use sb_core::location::{LocationId, LocationKind};
use sb_core::provider;
use sb_core::search::{Filters, Limit, Query, Searched};
use sb_core::sync::{attribute, index, sync_location};
use sb_core::transport::ssh_config::Resolution;
use sb_core::transport::{ResolveHost, Resolved, Transport};
use sb_test_support::{DesktopCopy, ProjectsTree, Session as Written};
use tempfile::TempDir;

const PROJECT: &str = "/home/user/code/app";

/// Where a host ends: its name, user, port and jump host.
type End = (&'static str, &'static str, u16, Option<&'static str>);

/// `ssh -G` as a table: a host it doesn't hold it answers nothing for.
struct Table(HashMap<&'static str, End>);

impl ResolveHost for Table {
    fn resolve(&self, host: &str) -> Resolution {
        Ok(self
            .0
            .get(host)
            .map(|(hostname, user, port, jump)| Resolved {
                hostname: (*hostname).to_string(),
                user: (*user).to_string(),
                port: *port,
                proxy_jump: jump.map(str::to_string),
                proxy_command: None,
            }))
    }
}

/// `ssh -G` that can't be asked at all: missing, failing or too slow.
struct Unasked;

impl ResolveHost for Unasked {
    fn resolve(&self, _: &str) -> Resolution {
        Err("ssh is not there".to_string())
    }
}

/// Where the alias `buildbox` ends, as `user@build.example` does.
fn table() -> Table {
    Table(HashMap::from([
        ("buildbox", ("build.example", "user", 22, None)),
        ("user@build.example", ("build.example", "user", 22, None)),
        ("other@build.example", ("build.example", "other", 22, None)),
        ("buildbox-alt-port", ("build.example", "user", 2222, None)),
        ("elsewhere", ("elsewhere.example", "user", 22, None)),
        ("behind-a", ("build.example", "user", 22, Some("bastion-a"))),
        ("localhost", ("localhost", "user", 22, None)),
        ("wsl-alias", ("127.0.0.1", "user", 2222, None)),
    ]))
}

/// This machine, whose desktop app keeps copies and its records of them, and the host the
/// sessions ran on, added as the SSH location `user@build.example`.
struct Machines {
    _dir: TempDir,
    archive: Archive,
    here: LocationId,
    host: LocationId,
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
        let host = catalogue
            .add_location(
                LocationKind::Ssh {
                    host: "user@build.example".to_string(),
                },
                "user@build.example",
            )
            .unwrap()
            .id;
        Self {
            _dir: dir,
            archive,
            here,
            host,
            pc: ProjectsTree::new(),
            remote: ProjectsTree::new(),
        }
    }

    fn catalogue(&self) -> &Catalogue {
        self.archive.catalogue()
    }

    /// A session that ran on the host, and this machine's copy of it, recorded as run on
    /// `host`: record number `n`.
    fn copied(&mut self, n: usize, host: &str) -> (Written, DesktopCopy) {
        let mut session = self.remote.session(PROJECT, 2);
        session.transcript.prompt("Look into the fetcher");
        session.transcript.reply("Looking.");
        let copy = self.pc.desktop_copy(&session);

        let path = self.remote.location_path(&session.key);
        self.pc.desktop_ssh_record(n, &session.id, host, &path);
        (session, copy)
    }

    fn sync_at(&self, location: &LocationId, transport: &dyn Transport) {
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
            pass.index.unwrap();
        }
    }

    /// Syncs this machine and attributes its copies through `resolve`.
    fn sync_here(&self, resolve: &dyn ResolveHost) {
        self.sync_at(&self.here, &self.pc.transport());
        attribute(&self.archive, resolve, &CancelToken::new()).unwrap();
    }

    fn sync_host(&self, resolve: &dyn ResolveHost) {
        self.sync_at(&self.host, &self.remote.transport());
        attribute(&self.archive, resolve, &CancelToken::new()).unwrap();
    }

    fn the_one(&self) -> Session {
        let mut shown = self.catalogue().sessions(Shelf::All).unwrap();
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

    /// Where the one session shown ran, and what it is a copy as.
    fn origin(&self) -> (Option<LocationId>, Option<Origin>) {
        let shown = self.the_one();
        (shown.ran_at().cloned(), shown.copy.map(|copy| copy.origin))
    }

    /// The sessions a search for `fetcher` finds from `location`.
    fn found_from(&self, location: &LocationId) -> Vec<SessionId> {
        let query = Query {
            terms: vec!["fetcher".to_string()],
            filters: Filters {
                location: Some(location.clone()),
                ..Filters::default()
            },
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
            Searched::KeepTyping => panic!("long enough to look for"),
        }
    }
}

fn unsaid() -> Option<Origin> {
    Some(Origin::Unsaid(
        "Ran over SSH; the copy doesn't record which machine.".to_string(),
    ))
}

#[test]
fn an_alias_and_user_at_host_that_end_alike_are_one_machine() {
    let mut machines = Machines::new();
    let (_, copy) = machines.copied(1, "buildbox");
    machines.sync_here(&table());

    let host = machines.host.clone();
    assert_eq!(
        machines.origin(),
        (
            Some(host.clone()),
            Some(Origin::Location {
                ran_on: "Ran over SSH on".to_string(),
                location: host.clone()
            })
        )
    );
    let shown = machines.the_one();
    assert_eq!(shown.id, machines.id(&machines.here, &copy.folder));
    /* It is the host's everywhere a location is named: filters, search, Unsorted. */
    assert_eq!(machines.found_from(&host), std::slice::from_ref(&shown.id));
    assert_eq!(machines.found_from(&machines.here), []);
    let groups = machines.catalogue().unsorted().unwrap();
    assert_eq!(
        groups
            .iter()
            .map(|group| (group.locations.clone(), group.copy.clone()))
            .collect::<Vec<_>>(),
        [(vec![host], None)]
    );
}

#[test]
fn another_user_or_port_is_another_machine_named_by_its_host() {
    for (n, host) in [
        "other@build.example",
        "buildbox-alt-port",
        "elsewhere",
        "behind-a",
    ]
    .into_iter()
    .enumerate()
    {
        let mut machines = Machines::new();
        machines.copied(n, host);
        machines.sync_here(&table());
        assert_eq!(
            machines.origin(),
            (
                None,
                Some(Origin::Host {
                    ran_on: "Ran over SSH on".to_string(),
                    host: host.to_string()
                })
            ),
            "{host}"
        );
        let shown = machines.the_one();
        assert_eq!(shown.copy.as_ref().map(|copy| copy.tag()), Some(host));
        assert_eq!(machines.found_from(&machines.host), []);
        let groups = machines.catalogue().unsorted().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(
            (groups[0].locations.is_empty(), groups[0].copy.as_deref()),
            (true, Some(host))
        );
    }
}

#[test]
fn a_host_ssh_can_t_resolve_leaves_the_copy_as_the_app_s_copy() {
    let mut machines = Machines::new();
    machines.copied(1, "unknown-to-ssh");
    machines.sync_here(&table());
    assert_eq!(machines.origin(), (None, unsaid()));

    /* An ssh that answers nothing for any host resolves the location's host none either. */
    let mut machines = Machines::new();
    machines.copied(1, "buildbox");
    machines.sync_here(&Table(HashMap::new()));
    assert_eq!(machines.origin(), (None, unsaid()));
    /* Once it does, the next attribution says so; and when it no longer does, it unsays it. */
    attribute(&machines.archive, &table(), &CancelToken::new()).unwrap();
    assert_eq!(machines.origin().0, Some(machines.host.clone()));
    attribute(
        &machines.archive,
        &Table(HashMap::new()),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(machines.origin(), (None, unsaid()));
}

#[test]
fn a_record_that_can_t_be_read_says_nothing_and_fails_nothing() {
    let mut machines = Machines::new();
    let session = machines.remote.session(PROJECT, 2);
    machines.pc.desktop_copy(&session);
    machines
        .pc
        .desktop_record("a/local_a.json", b"{ \"cliSessionId\": ");
    machines.pc.desktop_record(
        "b/local_b.json",
        format!(
            r#"{{"cliSessionId": "{}", "sshConfig": ["buildbox"]}}"#,
            session.id
        )
        .as_bytes(),
    );
    machines.sync_here(&table());
    assert_eq!(machines.origin(), (None, unsaid()));
}

#[test]
fn a_prior_cli_session_id_joins_the_record_to_the_copy() {
    let mut machines = Machines::new();
    let session = machines.remote.session(PROJECT, 2);
    machines.pc.desktop_copy(&session);
    machines.pc.desktop_record(
        "d/local_d.json",
        serde_json::json!({
            "cliSessionId": "00000000-0000-4000-8000-0000000000ff",
            "priorCliSessionIds": [session.id],
            "sshConfig": { "sshHost": "buildbox" },
        })
        .to_string()
        .as_bytes(),
    );
    machines.sync_here(&table());
    assert_eq!(machines.origin().0, Some(machines.host.clone()));
}

#[test]
fn the_original_arriving_takes_over_from_the_attributed_copy() {
    let mut machines = Machines::new();
    let (session, copy) = machines.copied(1, "buildbox");
    machines.sync_here(&table());
    let copy_id = machines.id(&machines.here, &copy.folder);
    machines.catalogue().set_starred(&copy_id, true).unwrap();

    machines.sync_host(&table());
    let shown = machines.the_one();
    assert_eq!(shown.id, machines.id(&machines.host, session.session_key()));
    assert_eq!((shown.copy, shown.starred), (None, true));
    assert_eq!(machines.found_from(&machines.host), [shown.id]);
}

#[test]
fn the_recorded_path_matches_a_copy_whose_first_cwd_is_not_its_original_s() {
    let mut machines = Machines::new();
    let (session, copy) = machines.copied(1, "elsewhere");
    /* The copy's records start in another directory than the original's. */
    let path = machines.pc.path(&copy.key);
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace(PROJECT, "/home/user/code/elsewhere");
    std::fs::write(&path, text).unwrap();
    machines.pc.set_modified(
        &copy.key,
        std::time::SystemTime::now() - std::time::Duration::from_secs(3600),
    );
    machines.sync_here(&table());
    machines.sync_host(&table());

    let shown = machines.the_one();
    assert_eq!(shown.id, machines.id(&machines.host, session.session_key()));
    let kin = machines.archive.kin(&shown.id).unwrap();
    assert_eq!(
        kin.iter().map(|kin| kin.id.clone()).collect::<Vec<_>>(),
        [machines.id(&machines.here, &copy.folder)]
    );
}

#[test]
fn a_recorded_path_under_another_root_matches_nothing() {
    let mut machines = Machines::new();
    let mut session = machines.remote.session(PROJECT, 2);
    session.transcript.prompt("Look into the fetcher");
    let copy = machines.pc.desktop_copy(&session);
    /* The same key, under a root that is not the host's. */
    let path = format!("/srv/other/agent/{}", session.key);
    machines
        .pc
        .desktop_ssh_record(1, &session.id, "elsewhere", &path);
    let file = machines.pc.path(&copy.key);
    let text = std::fs::read_to_string(&file)
        .unwrap()
        .replace(PROJECT, "/home/user/code/elsewhere");
    std::fs::write(&file, text).unwrap();
    machines.pc.set_modified(
        &copy.key,
        std::time::SystemTime::now() - std::time::Duration::from_secs(3600),
    );
    machines.sync_here(&table());
    machines.sync_host(&table());
    assert_eq!(machines.catalogue().sessions(Shelf::All).unwrap().len(), 2);
}

#[test]
fn a_reindex_makes_the_attribution_again() {
    let mut machines = Machines::new();
    machines.copied(1, "buildbox");
    machines.sync_here(&table());
    machines.catalogue().drop_index().unwrap();
    index(
        &machines.archive,
        None,
        &provider::builtin(),
        &CancelToken::new(),
        &mut |_| {},
    )
    .unwrap();
    attribute(&machines.archive, &table(), &CancelToken::new()).unwrap();
    assert_eq!(machines.origin().0, Some(machines.host.clone()));
}

/// Of the app's folder, only the `.json` files directly in each session's folder are archived:
/// whatever else it keeps there is left alone.
#[test]
fn only_the_app_s_session_records_are_archived() {
    let machines = Machines::new();
    /* Both layouts: a record one folder down, and records together two folders down. */
    let mut kept = vec![
        machines
            .pc
            .desktop_record("a/local_a.json", b"{\"cliSessionId\": 1}"),
        machines
            .pc
            .desktop_record("u/o/local_b.json", b"{\"cliSessionId\": 2}"),
        machines
            .pc
            .desktop_record("u/o/local_c.json", b"{\"cliSessionId\": 3}"),
    ];
    kept.sort();
    for other in [
        "a/notes.txt",
        "u/o/deeper/local_d.json",
        "top.json",
        "a/log.jsonl",
    ] {
        machines.pc.desktop_record(other, b"{}");
    }
    machines.sync_here(&table());
    let archived: Vec<String> = (machines.catalogue())
        .items(&machines.here, provider::builtin()[0].id().as_str())
        .unwrap()
        .into_iter()
        .map(|item| item.key)
        .filter(|key| !key.starts_with("projects/"))
        .collect();
    assert_eq!(archived, kept);
}

/// The app's records together two folders down, as on Windows, join their copies by their
/// fields alone: the folders are named by nothing of the session's.
#[test]
fn records_kept_together_two_folders_down_are_joined_by_their_fields() {
    let mut machines = Machines::new();
    let session = machines.remote.session(PROJECT, 2);
    machines.pc.desktop_copy(&session);
    let other = machines.remote.session(PROJECT, 2);
    let path = machines.remote.location_path(&session.key);
    for (name, id, host) in [
        ("local_1.json", &session.id, "buildbox"),
        ("local_2.json", &other.id, "elsewhere"),
    ] {
        machines.pc.desktop_record(
            &format!("user-folder/org-folder/{name}"),
            serde_json::json!({
                "cliSessionId": id,
                "sshConfig": { "sshHost": host },
                "sshRemoteTranscriptPath": path,
            })
            .to_string()
            .as_bytes(),
        );
    }
    machines.sync_here(&table());
    assert_eq!(machines.origin().0, Some(machines.host.clone()));
}

/// `ssh` that can't be asked keeps what was said last, of a copy's host and of a location's;
/// one whose string is the location's own is that location even then.
#[test]
fn ssh_that_can_t_be_asked_keeps_the_last_attribution() {
    let mut machines = Machines::new();
    machines.copied(1, "buildbox");
    machines.sync_here(&table());
    let host = machines.host.clone();
    assert_eq!(machines.origin().0, Some(host.clone()));
    attribute(&machines.archive, &Unasked, &CancelToken::new()).unwrap();
    assert_eq!(machines.origin().0, Some(host.clone()));

    let mut machines = Machines::new();
    machines.copied(1, "user@build.example");
    machines.sync_here(&Unasked);
    assert_eq!(machines.origin().0, Some(machines.host.clone()));
}

/// A copy's location moving refiles it: by the path attached for the location it ran at, never
/// one it was filed in by hand.
#[test]
fn a_copy_is_filed_by_the_location_it_ran_at_and_refiled_as_that_moves() {
    let mut machines = Machines::new();
    machines.copied(1, "buildbox");
    let catalogue = machines.catalogue();
    let folder = catalogue.create_folder("app", None).unwrap();
    catalogue
        .attach_path(folder, PROJECT, Some(&machines.host))
        .unwrap();
    machines.sync_here(&table());
    assert_eq!(machines.the_one().filed, Filed::ByRule(folder));
    /* ssh no longer makes it that machine: it is from no location, and unfiled. */
    attribute(
        &machines.archive,
        &Table(HashMap::new()),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(machines.the_one().filed, Filed::Unsorted);
    /* Filed by hand, it stays wherever its location goes. */
    let id = machines.the_one().id;
    let other = machines.catalogue().create_folder("mine", None).unwrap();
    machines.catalogue().file_by_hand(&id, other).unwrap();
    attribute(&machines.archive, &table(), &CancelToken::new()).unwrap();
    assert_eq!(machines.the_one().filed, Filed::ByHand(other));
}

/// Of two locations that are one machine the one added first is taken; one removed is none.
#[test]
fn the_location_added_first_wins_and_a_removed_one_is_none() {
    let mut machines = Machines::new();
    machines.copied(1, "buildbox");
    let alias = machines
        .catalogue()
        .add_location(
            LocationKind::Ssh {
                host: "buildbox".to_string(),
            },
            "buildbox",
        )
        .unwrap()
        .id;
    machines.sync_here(&table());
    assert_eq!(machines.origin().0, Some(machines.host.clone()));
    machines
        .catalogue()
        .remove_location(&machines.host)
        .unwrap();
    attribute(&machines.archive, &table(), &CancelToken::new()).unwrap();
    assert_eq!(machines.origin().0, Some(alias.clone()));
    machines.catalogue().remove_location(&alias).unwrap();
    attribute(&machines.archive, &table(), &CancelToken::new()).unwrap();
    assert!(matches!(machines.origin().1, Some(Origin::Host { .. })));
}

/// Two records that disagree on a conversation's host say none: the copy is the app's.
#[test]
fn records_that_disagree_say_no_host() {
    let mut machines = Machines::new();
    let (session, _) = machines.copied(1, "buildbox");
    let path = machines.remote.location_path(&session.key);
    machines
        .pc
        .desktop_ssh_record(2, &session.id, "elsewhere", &path);
    machines.sync_here(&table());
    assert_eq!(machines.origin(), (None, unsaid()));
}

/// Without the desktop app's folder the agent syncs as before, and copies are the app's.
#[test]
fn an_agent_without_the_app_s_folder_syncs_as_before() {
    let mut machines = Machines::new();
    let session = machines.remote.session(PROJECT, 2);
    machines.pc.desktop_copy(&session);
    machines.sync_here(&table());
    assert_eq!(machines.origin(), (None, unsaid()));
}

/// A copy matched to a deleted original only by its recorded path is deleted with it: it does
/// not come back as a copy.
#[test]
fn a_copy_named_by_path_of_a_deleted_transcript_is_deleted_too() {
    let mut machines = Machines::new();
    let (session, copy) = machines.copied(1, "elsewhere");
    let file = machines.pc.path(&copy.key);
    let text = std::fs::read_to_string(&file)
        .unwrap()
        .replace(PROJECT, "/home/user/code/elsewhere");
    std::fs::write(&file, text).unwrap();
    machines.pc.set_modified(
        &copy.key,
        std::time::SystemTime::now() - std::time::Duration::from_secs(3600),
    );
    /* The original is deleted for good before the copy is archived. */
    machines.sync_host(&table());
    let original = machines.id(&machines.host, session.session_key());
    machines.catalogue().trash(&original).unwrap();
    let deletion = machines.catalogue().deletion(&original).unwrap();
    machines.archive.delete_for_good(&deletion).unwrap();
    machines.sync_here(&table());
    assert_eq!(
        machines
            .catalogue()
            .session(&machines.id(&machines.here, &copy.folder))
            .unwrap(),
        None
    );
}

/// Deleting a copy for good takes the app's record of its session with it.
#[test]
fn deleting_a_copy_takes_its_record() {
    let mut machines = Machines::new();
    machines.copied(1, "buildbox");
    machines.sync_here(&table());
    let id = machines.the_one().id;
    machines.catalogue().trash(&id).unwrap();
    let deletion = machines.catalogue().deletion(&id).unwrap();
    machines.archive.delete_for_good(&deletion).unwrap();
    let registers = (machines.catalogue())
        .items(&machines.here, provider::builtin()[0].id().as_str())
        .unwrap()
        .into_iter()
        .filter(|item| !item.key.starts_with("projects/"))
        .count();
    assert_eq!(registers, 0);
    machines.sync_here(&table());
    assert!(
        machines
            .catalogue()
            .sessions(Shelf::All)
            .unwrap()
            .is_empty()
    );
}

/// A WSL distro, as a location, synced from `tree` when one is given.
fn add_distro(machines: &Machines, name: &str, tree: Option<&ProjectsTree>) -> LocationId {
    let id = machines
        .catalogue()
        .add_location(
            LocationKind::Wsl {
                distro: name.to_string(),
            },
            name,
        )
        .unwrap()
        .id;
    if let Some(tree) = tree {
        machines.sync_at(&id, &tree.transport());
    }
    id
}

/// A copy the desktop app ran over SSH at localhost (its "SSH into WSL") ran in a WSL distro:
/// the only one there is, never an SSH location, whatever ssh says of the host.
#[test]
fn a_copy_that_ran_at_localhost_is_the_wsl_distro_s() {
    for host in ["localhost", "wsl-alias"] {
        let mut machines = Machines::new();
        machines.copied(1, host);
        let distro = add_distro(&machines, "Ubuntu-24.04", None);
        machines.sync_here(&table());
        assert_eq!(machines.origin().0, Some(distro), "{host}");
    }
}

/// Of several distros, the one whose home holds where the session started; of none or of
/// several, no distro, and no host either: "localhost" says nothing.
#[test]
fn of_several_distros_the_one_whose_home_holds_the_cwd() {
    let mut machines = Machines::new();
    let session = machines.remote.session(PROJECT, 2);
    machines.pc.desktop_copy(&session);
    let (first, second) = (ProjectsTree::new(), ProjectsTree::new());
    let cwd = format!("{}/code/app", second.home().display());
    machines.pc.desktop_record(
        "w/local_w.json",
        serde_json::json!({
            "cliSessionId": session.id,
            "cwd": cwd,
            "sshConfig": { "sshHost": "localhost" },
        })
        .to_string()
        .as_bytes(),
    );
    add_distro(&machines, "Debian", Some(&first));
    let wanted = add_distro(&machines, "Ubuntu-24.04", Some(&second));
    machines.sync_here(&table());
    assert_eq!(machines.origin().0, Some(wanted));

    /* Without any distro it is the app's copy, with no host. */
    let mut machines = Machines::new();
    machines.copied(1, "localhost");
    machines.sync_here(&table());
    assert_eq!(machines.origin(), (None, unsaid()));
}
