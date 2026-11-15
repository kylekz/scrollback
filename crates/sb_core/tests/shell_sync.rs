//! Sync over the shell transport (PLAN §12): the archive a location read through each shell
//! this machine has ends up as, beside the one `std::fs` gives; and what a pass makes of a
//! transport that is cancelled or cut off under it. The sync tests proper run over the shell
//! transport too, through `sh` (`common::again_through_a_shell`).

#![cfg(unix)]

mod common;

use pretty_assertions::assert_eq;
use sb_core::CancelToken;
use sb_core::archive::Archive;
use sb_core::catalogue::Reason;
use sb_core::hash::Hash;
use sb_core::location::{LocationId, LocationKind};
use sb_core::provider::ItemKind;
use sb_core::sync::{SyncError, sync_location};
use sb_core::transport::{Launcher, ShellTransport, Transport};
use sb_test_support::{Agent, FileAgent, HomeTransport, loopback, shells};
use tempfile::TempDir;

use common::{Bench, Meddle, When};

/// An archive of its own for one way of reading the fixture.
struct Copy {
    dir: TempDir,
    archive: Archive,
    location: LocationId,
}

impl Copy {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::create(dir.path()).unwrap();
        let location = archive
            .catalogue()
            .add_location(LocationKind::Local, "fixture")
            .unwrap()
            .id;
        Self {
            dir,
            archive,
            location,
        }
    }

    fn sync(&self, agent: &FileAgent, transport: &dyn Transport) {
        let passes = sync_location(
            &self.archive,
            &self.location,
            transport,
            &[agent.provider()],
            &CancelToken::new(),
            &mut |_| {},
        )
        .unwrap();
        let report = passes[0].outcome.as_ref().unwrap();
        assert!(report.failed.is_empty() && report.unlisted.is_empty());
    }

    /// Everything the catalogue holds of the agent's items but when it was written: each
    /// item with its versions, and each version with its segments.
    fn picture(&self, agent: &FileAgent) -> Vec<Item> {
        let catalogue = self.archive.catalogue();
        catalogue
            .items(&self.location, agent.provider().id().as_str())
            .unwrap()
            .into_iter()
            .map(|item| Item {
                versions: catalogue
                    .versions(item.id)
                    .unwrap()
                    .into_iter()
                    .map(|version| Version {
                        segments: catalogue
                            .segments(version.id)
                            .unwrap()
                            .into_iter()
                            .map(|segment| (segment.offset, segment.length, segment.hash))
                            .collect(),
                        number: version.number,
                        reason: version.reason,
                        length: version.length,
                        hash: version.hash,
                        held_back: version.held_back,
                    })
                    .collect(),
                key: item.key,
                kind: item.kind,
                session: item.session,
                seen_length: item.seen_length,
                gone: item.gone_since.is_some(),
            })
            .collect()
    }

    fn assert_verifies(&self) {
        let report = self.archive.verify(&CancelToken::new()).unwrap();
        assert!(report.damaged.is_empty() && report.unread.is_empty());
        assert!(report.uncatalogued.is_empty(), "{:?}", self.dir.path());
    }
}

#[derive(Debug, PartialEq)]
struct Item {
    key: String,
    kind: ItemKind,
    session: Option<String>,
    seen_length: u64,
    gone: bool,
    versions: Vec<Version>,
}

#[derive(Debug, PartialEq)]
struct Version {
    number: u32,
    reason: Reason,
    length: u64,
    hash: Hash,
    held_back: bool,
    segments: Vec<(u64, u64, Hash)>,
}

#[test]
fn a_location_read_through_a_shell_is_archived_as_std_fs_archives_it() {
    let mut agent = FileAgent::new();
    let keys: Vec<_> = [8, 3, 40, 5, 12, 1]
        .into_iter()
        .map(|records| agent.add_session(records))
        .collect();
    let subagent = agent.session(&keys[0]).subagent(6);
    agent.session(&keys[0]).tool_result(70_000);
    agent.session(&keys[1]).title("a title");
    /* Files of no session, one with a name a script could trip over. */
    agent
        .tree()
        .memory("/home/user/code/app", "notes", "remember");
    agent.tree().project_file(
        "/home/user/code/app",
        "-odd/it's a \"file\"\nof $HOME `id`.txt",
        b"\0binary\0\0",
    );

    let here = Copy::new();
    let copies: Vec<_> = shells()
        .into_iter()
        .map(|shell| {
            let transport = HomeTransport::through(&shell, agent.tree().home(), Vec::new());
            (Copy::new(), transport, shell)
        })
        .collect();
    let all_sync_alike = |agent: &FileAgent, step: &str| {
        here.sync(agent, &agent.transport());
        let expected = here.picture(agent);
        for (copy, transport, shell) in &copies {
            copy.sync(agent, transport);
            assert_eq!(copy.picture(agent), expected, "{step}, through {shell}");
        }
        expected
    };

    assert_eq!(all_sync_alike(&agent, "the first sync").len(), 12);

    /* Every way a source changes, at once. */
    agent.append(&keys[0], 4);
    agent.rewrite(&keys[1]);
    agent.truncate(&keys[2], 30);
    agent.touch(&keys[3]);
    agent.delete(&keys[4]);
    agent.rewrite_same_size(&keys[5]);
    agent.append_raw(&subagent, "{\"type\":\"user\",\"mess");
    let grown = agent.add_session(9);
    let changed = all_sync_alike(&agent, "the second sync");
    let versions = |key: &str| {
        changed
            .iter()
            .find(|item| item.key == key)
            .unwrap()
            .versions
            .len()
    };
    assert_eq!(
        [&keys[0], &keys[1], &keys[2], &keys[3], &keys[5], &grown].map(|key| versions(key)),
        [1, 2, 2, 1, 2, 1]
    );
    assert!(changed.iter().any(|item| item.key == keys[4] && item.gone));

    /* The unfinished last line is taken once the file is found as it was. */
    let settled = all_sync_alike(&agent, "the third sync");
    assert_ne!(settled, changed);
    assert_eq!(all_sync_alike(&agent, "the fourth sync"), settled);
    here.assert_verifies();
    for (copy, ..) in &copies {
        copy.assert_verifies();
    }
}

/// A bench with three sessions, each larger than a cut-off answer gets through.
fn bench() -> (Bench<FileAgent>, Vec<String>) {
    let mut agent = FileAgent::new();
    let keys = (0..3).map(|_| agent.add_session(200)).collect();
    (Bench::new(agent), keys)
}

#[test]
fn a_transport_stopped_by_the_cancel_ends_the_sync_as_cancelled() {
    let (bench, keys) = bench();
    let cancel = CancelToken::new();
    let transport =
        ShellTransport::new(loopback("sh", bench.agent.tree().home(), &[])).cancelled_by(&cancel);
    /* Cancelled as the scan is about to list the location: the listing is what stops. */
    let cancelled = sync_location(
        bench.archive(),
        &bench.location,
        &transport,
        &[bench.meddling(Meddle::at(When::BeforeScan, || cancel.cancel()))],
        &cancel,
        &mut |_| {},
    );
    assert!(
        matches!(cancelled, Err(SyncError::Cancelled)),
        "{cancelled:?}"
    );
    let location = bench.location();
    assert_eq!((location.last_error, location.unreachable), (None, None));

    assert_eq!(bench.sync().new_items, 3);
    bench.assert_mirrors(&keys);
}

#[test]
fn a_connection_cut_in_the_middle_of_a_batch_stores_none_of_it() {
    let (bench, keys) = bench();
    let home = bench.agent.tree().home().to_str().unwrap();
    /* Enough of each answer gets through for the pass to reach its fetch, and not the fetch. */
    let cut = ShellTransport::new(Launcher::Command {
        program: "sh".into(),
        args: vec![
            "-c".to_string(),
            "env -i PATH=\"$PATH\" HOME=\"$1\" sh -s | head -c 20000; \
             echo 'Connection lost' >&2; exit 255"
                .to_string(),
            "sh".to_string(),
            home.to_string(),
        ],
    });
    let passes = sync_location(
        bench.archive(),
        &bench.location,
        &cut,
        &[bench.agent.provider()],
        &CancelToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert!(
        matches!(passes[0].outcome, Err(SyncError::Provider(_))),
        "{:?}",
        passes[0].outcome
    );
    let error = bench.location().last_error.unwrap();
    assert!(error.ends_with("Connection lost"), "{error}");
    assert!(bench.items().is_empty() && bench.segment_files().is_empty());
    bench.assert_verifies();

    assert_eq!(bench.sync().new_items, 3);
    bench.assert_mirrors(&keys);
    assert_eq!(bench.location().last_error, None);
}
