//! Generated sessions for the scenarios' agent trees (docs/PLAN.md §12). Everything here is
//! made up: titles, prompts and paths in placeholder names.

use std::path::Path;

use anyhow::Result;
use sb_core::CancelToken;
use sb_core::archive::Archive;
use sb_core::location::LocationKind;
use sb_core::provider;
use sb_core::sync::sync_location;
use sb_test_support::{Outline, ProjectsTree};
use toolbox_store::rusqlite;

/// An agent's transcripts on this machine: two sessions of one project and one of another,
/// for the first run to find and Unsorted to group.
pub fn agent(tree: &mut ProjectsTree) {
    for outline in [
        Outline {
            project: "/home/user/code/app",
            title: "Fix the flaky upload test",
            prompt: "The upload test fails one run in five. Find the race and fix it.",
            model: "claude-opus-5-5",
            day: (2026, 11, 12),
            calls: 7,
        },
        Outline {
            project: "/home/user/code/app",
            title: "Add retries to the fetcher",
            prompt: "Retry a failed fetch three times with backoff before giving up.",
            model: "claude-sonnet-5",
            day: (2026, 11, 10),
            calls: 3,
        },
        Outline {
            project: "/home/user/notes",
            title: "Summarise this week's notes",
            prompt: "Read the notes from this week and write a summary for Friday.",
            model: "claude-opus-5-5",
            day: (2026, 10, 30),
            calls: 0,
        },
    ] {
        tree.outlined(&outline);
    }
}

/// How long the agent keeps transcripts at each location, as its settings say: the stale
/// banner's deadline is the last good sync plus this (DESIGN §4.11).
const RETENTION: &str = r#"{"cleanupPeriodDays": 30}"#;

/// How many days ago the host over SSH last synced: as `E-Locations` draws it, stale.
const HOST_SYNCED_DAYS_AGO: i64 = 12;

/**
 * The locations sheet's library (DESIGN §4.11): an archive in `archive_dir` with three
 * locations, this machine, the distro `Ubuntu-24.04` and the host `user@macbook`, each synced
 * from a tree of its own under `scratch` (removed again), the host's last good sync twelve
 * days ago; and this machine's agent in `tree`, which a scripted run also reads as the distro
 * (`scrollback`'s stand-ins), keeping its transcripts 30 days.
 */
pub fn locations(tree: &mut ProjectsTree, archive_dir: &Path, scratch: &Path) -> Result<()> {
    agent(tree);
    tree.write_settings(RETENTION);
    let providers = provider::builtin();
    let archive = Archive::create(archive_dir)?;
    let places = [
        (LocationKind::Local, "This PC", "/home/user/code/app"),
        (
            LocationKind::Wsl {
                distro: "Ubuntu-24.04".to_string(),
            },
            "Ubuntu-24.04",
            "/home/user/code/server",
        ),
        (
            LocationKind::Ssh {
                host: "user@macbook".to_string(),
            },
            "user@macbook",
            "/Users/user/code/blog",
        ),
    ];
    for (at, (kind, name, project)) in places.into_iter().enumerate() {
        let mut there = ProjectsTree::in_home(&scratch.join(at.to_string()));
        there.write_settings(RETENTION);
        for day in [3, 9] {
            there.outlined(&Outline {
                project,
                title: &format!("Notes from {name}, day {day}"),
                prompt: "Write down what changed today and what is left to do.",
                model: "claude-opus-5-5",
                day: (2026, 11, day),
                calls: 2,
            });
        }
        let location = archive.catalogue().add_location(kind, name)?;
        for pass in sync_location(
            &archive,
            &location.id,
            &there.transport(),
            &providers,
            &CancelToken::new(),
            &mut |_| {},
        )? {
            pass.outcome?;
        }
    }
    drop(archive);
    std::fs::remove_dir_all(scratch)?;
    /* Only the catalogue says when a location last synced, so it is told directly. */
    let ago = HOST_SYNCED_DAYS_AGO * 24 * 60 * 60 * 1000;
    rusqlite::Connection::open(archive_dir.join(CATALOGUE))?.execute(
        "UPDATE location SET last_success_at = last_success_at - ?1,
             last_attempt_at = last_attempt_at - ?1
         WHERE kind = 'ssh'",
        [ago],
    )?;
    Ok(())
}

/// The catalogue's file in an archive's folder (PLAN §4.1).
const CATALOGUE: &str = "scrollback.db";
