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

/**
 * Copies of sessions that ran elsewhere (docs/PLAN.md §7.7, DESIGN §4.2.8): an archive in
 * `archive_dir` with this machine and the distro `Ubuntu-24.04`, each synced from a tree of its
 * own under `scratch` (removed again), and the host `user@macbook`, which has never synced. The
 * distro ran a session of which this machine's desktop app keeps a copy that is further along
 * than the distro's, which synced before it; the Mac ran one of which this machine keeps the only
 * copy, and the app's record of it names the host by its alias, `macbook`, which `ssh` with no
 * config takes to the same machine, so the copy is the host's (PLAN §7.7); and this machine ran
 * one of its own.
 */
pub fn copies(archive_dir: &Path, scratch: &Path) -> Result<()> {
    let outline = |project, title, prompt, day| Outline {
        project,
        title,
        prompt,
        model: "claude-opus-5-5",
        day: (2026, 11, day),
        calls: 3,
    };
    let mut here = ProjectsTree::in_home(&scratch.join("here"));
    here.outlined(&outline(
        "/home/user/notes",
        "Summarise this week's notes",
        "Read the notes from this week and write a summary for Friday.",
        6,
    ));
    let mut distro = ProjectsTree::in_home(&scratch.join("distro"));
    let ran = distro.outline(&outline(
        "/home/user/code/server",
        "Add retries to the fetcher",
        "Retry a failed fetch three times with backoff before giving up.",
        12,
    ));
    let mut copy = here.desktop_copy(&ran);
    copy.transcript.set_time("2026-11-12T13:00:00.000Z");
    copy.transcript.prompt("And log each retry with its delay.");
    copy.transcript.reply("Each retry is logged now.");
    let mut mac = ProjectsTree::in_home(&scratch.join("mac"));
    let lone = mac.outline(&outline(
        "/Users/user/code/blog",
        "Rework the blog's header",
        "Make the header sticky and shrink it on scroll.",
        9,
    ));
    here.desktop_copy(&lone);
    here.desktop_ssh_record(
        1,
        &lone.id,
        "macbook",
        &format!("/Users/user/.claude/{}", lone.key),
    );

    let archive = Archive::create(archive_dir)?;
    for (kind, name, tree) in [
        (LocationKind::Local, "This PC", &here),
        (
            LocationKind::Wsl {
                distro: "Ubuntu-24.04".to_string(),
            },
            "Ubuntu-24.04",
            &distro,
        ),
    ] {
        let location = archive.catalogue().add_location(kind, name)?;
        synced(&archive, &location.id, &tree.transport())?;
    }
    archive.catalogue().add_location(
        LocationKind::Ssh {
            host: "user@macbook".to_string(),
        },
        "user@macbook",
    )?;
    drop(archive);
    std::fs::remove_dir_all(scratch)?;
    Ok(())
}

/// The catalogue's file in an archive's folder (PLAN §4.1).
const CATALOGUE: &str = "scrollback.db";

/// When `E-Settings-Archive` says its two sessions were deleted for good: 4 Oct and 2 Aug
/// 2026, at noon UTC.
const DELETED_AT: [i64; 2] = [1_791_115_200_000, 1_785_672_000_000];

/// Syncs `location` of `archive` through `transport`, every pass to its end.
fn synced(
    archive: &Archive,
    location: &sb_core::location::LocationId,
    transport: &dyn sb_core::transport::Transport,
) -> Result<()> {
    for pass in sync_location(
        archive,
        location,
        transport,
        &provider::builtin(),
        &CancelToken::new(),
        &mut |_| {},
    )? {
        pass.outcome?;
    }
    Ok(())
}

/// Overwrites the middle of the first stored segment of the item `key` in `archive_dir`: what
/// verify reports as a segment that doesn't hold the bytes archived.
fn damage(archive_dir: &Path, key: &str) -> Result<()> {
    /* An item's folder is named by the start of its key's hash (PLAN §4.1). */
    let folder = &sb_core::hash::Hash::of(key.as_bytes()).to_hex()[..32];
    let mut segments = Vec::new();
    let mut folders = vec![archive_dir.join("files")];
    while let Some(dir) = folders.pop() {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                folders.push(path);
            } else if path.extension().is_some_and(|it| it == "zst")
                && path.iter().any(|part| part == folder)
            {
                segments.push(path);
            }
        }
    }
    segments.sort();
    let segment = segments
        .first()
        .ok_or_else(|| anyhow::anyhow!("{key} has no segment"))?;
    let mut bytes = std::fs::read(segment)?;
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    std::fs::write(segment, bytes)?;
    Ok(())
}

/**
 * Settings › Archive's library (DESIGN §4.13, `E-Settings-Archive`): an archive in
 * `archive_dir` with the distro `Ubuntu-24.04`, synced from this machine's agent in `tree`
 * (which a scripted run reads as the distro), and the host `user@macbook`, synced from a tree of
 * its own under `scratch` (removed again), whose two sessions were deleted for good on the
 * board's days. Two of the distro's transcripts have a damaged segment: "Day 9: disk
 * compaction", which the distro still has, so it can be fetched again; and "Tidy the notes
 * folder", which the agent has deleted since, as the start's sync finds.
 */
pub fn settings(tree: &mut ProjectsTree, archive_dir: &Path, scratch: &Path) -> Result<()> {
    agent(tree);
    /* A session's transcript is its key's file under the agent's `projects/`. */
    let transcript = |session: String| format!("projects/{session}.jsonl");
    let compaction = transcript(tree.outlined(&Outline {
        project: "/home/user/code/server",
        title: "Day 9: disk compaction",
        prompt: "Compact the day's segment files into one and check the result.",
        model: "claude-opus-5-5",
        day: (2026, 11, 9),
        calls: 4,
    }));
    let notes = transcript(tree.outlined(&Outline {
        project: "/home/user/notes",
        title: "Tidy the notes folder",
        prompt: "Move last month's notes into an archive folder.",
        model: "claude-sonnet-5",
        day: (2026, 10, 20),
        calls: 1,
    }));
    tree.write_settings(RETENTION);
    let archive = Archive::create(archive_dir)?;
    let distro = archive.catalogue().add_location(
        LocationKind::Wsl {
            distro: "Ubuntu-24.04".to_string(),
        },
        "Ubuntu-24.04",
    )?;
    synced(&archive, &distro.id, &tree.transport())?;

    let mut host_tree = ProjectsTree::in_home(&scratch.join("host"));
    host_tree.write_settings(RETENTION);
    for (title, day) in [
        ("Print the whole .env to debug", (2026, 10, 3)),
        ("Benchmark: 80 MB transcript fixture", (2026, 8, 1)),
    ] {
        host_tree.outlined(&Outline {
            project: "/Users/user/code/blog",
            title,
            prompt: "Made up for the board.",
            model: "claude-opus-5-5",
            day,
            calls: 2,
        });
    }
    let host = archive.catalogue().add_location(
        LocationKind::Ssh {
            host: "user@macbook".to_string(),
        },
        "user@macbook",
    )?;
    synced(&archive, &host.id, &host_tree.transport())?;
    for session in archive
        .catalogue()
        .sessions(sb_core::catalogue::Shelf::All)?
    {
        if session.id.location == host.id {
            archive.catalogue().trash(&session.id)?;
            archive.delete_for_good(&archive.catalogue().deletion(&session.id)?)?;
        }
    }
    archive.wait_for_removals();
    drop(archive);
    std::fs::remove_dir_all(scratch)?;
    for key in [&compaction, &notes] {
        damage(archive_dir, key)?;
    }
    std::fs::remove_file(tree.path(&notes))?;
    /* The board's days for the two deletions, the latest first. */
    let catalogue = rusqlite::Connection::open(archive_dir.join(CATALOGUE))?;
    for (at, deleted) in DELETED_AT.iter().enumerate() {
        catalogue.execute(
            "UPDATE tombstone SET deleted_at = ?1 WHERE session = (
                 SELECT session FROM tombstone ORDER BY started_at DESC LIMIT 1 OFFSET ?2)",
            [*deleted, at as i64],
        )?;
    }
    Ok(())
}

/// The settings file in the data dir (`sb_ui::services::settings`).
const SETTINGS_FILE: &str = "settings.json";

/**
 * Settings that name the archive `Archive` under `drive`, which isn't there, as on a drive
 * unplugged; they remember the archive by its id, when it was last opened (the board's day)
 * and what it held.
 */
pub fn archive_gone(data_dir: &Path, drive: &Path) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let settings = serde_json::json!({
        "v": 1,
        "archive": std::path::absolute(drive.join("Archive"))?,
        "last_archive": {
            "id": "a3f061c2-7b1e-4c2a-9d4f-0e5b8c1d2f30",
            "opened": 1_794_758_400_000_i64,
            "sessions": 2318,
            "bytes": 4_400_000_000_u64,
        },
    });
    std::fs::write(
        data_dir.join(SETTINGS_FILE),
        serde_json::to_string_pretty(&settings)?,
    )?;
    Ok(())
}

/// A settings file cut short, so not JSON, that names the archive `Archive` under `drive`.
pub fn settings_unreadable(data_dir: &Path, drive: &Path) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let archive = serde_json::json!(std::path::absolute(drive.join("Archive"))?);
    std::fs::write(
        data_dir.join(SETTINGS_FILE),
        format!("{{\n  \"v\": 1,\n  \"archive\": {archive},\n  \"mode\": \"da"),
    )?;
    Ok(())
}
