//! Where a lone copy ran (PLAN §7.7): the records an app keeps beside its copies name the host
//! a session ran on over SSH, which this machine's `ssh` resolves, and a copy whose host is an
//! SSH location's is that location's in the library until its original is archived. The
//! records and the hosts are generated; `ssh -G` is a stand-in answering from a table.

use std::collections::HashMap;

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::archive::Archive;
use sb_core::catalogue::{Catalogue, Session, SessionId, Shelf};
use sb_core::location::{LocationId, LocationKind};
use sb_core::provider;
use sb_core::search::{Filters, Limit, Query, Searched};
use sb_core::sync::{attribute, index, sync_location};
use sb_core::transcript::Origin;
use sb_core::transport::{ResolveHost, Resolved, Transport};
use sb_test_support::{DesktopCopy, ProjectsTree, Session as Written};
use tempfile::TempDir;

const PROJECT: &str = "/home/user/code/app";

/// `ssh -G` as a table: a host it doesn't hold fails to resolve.
struct Table(HashMap<&'static str, (&'static str, &'static str, u16)>);

impl ResolveHost for Table {
    fn resolve(&self, host: &str) -> Option<Resolved> {
        self.0.get(host).map(|(hostname, user, port)| Resolved {
            hostname: (*hostname).to_string(),
            user: (*user).to_string(),
            port: *port,
        })
    }
}

/// Where the alias `buildbox` ends, as `user@build.example` does.
fn table() -> Table {
    Table(HashMap::from([
        ("buildbox", ("build.example", "user", 22)),
        ("user@build.example", ("build.example", "user", 22)),
        ("other@build.example", ("build.example", "other", 22)),
        ("buildbox-alt-port", ("build.example", "user", 2222)),
        ("elsewhere", ("elsewhere.example", "user", 22)),
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
        attribute(&self.archive, resolve).unwrap();
    }

    fn sync_host(&self, resolve: &dyn ResolveHost) {
        self.sync_at(&self.host, &self.remote.transport());
        attribute(&self.archive, resolve).unwrap();
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
        (Some(host.clone()), Some(Origin::Location(host.clone())))
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
    for (n, host) in ["other@build.example", "buildbox-alt-port", "elsewhere"]
        .into_iter()
        .enumerate()
    {
        let mut machines = Machines::new();
        machines.copied(n, host);
        machines.sync_here(&table());
        assert_eq!(
            machines.origin(),
            (None, Some(Origin::Host(host.to_string()))),
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
    attribute(&machines.archive, &table()).unwrap();
    assert_eq!(machines.origin().0, Some(machines.host.clone()));
    attribute(&machines.archive, &Table(HashMap::new())).unwrap();
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
    attribute(&machines.archive, &table()).unwrap();
    assert_eq!(machines.origin().0, Some(machines.host.clone()));
}
