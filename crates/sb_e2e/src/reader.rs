//! The library the reader's scenarios open sessions from (docs/DESIGN.md §4.2, §6), built as
//! an archive with the real engine from `sb_test_support::reader`'s sessions:
//!
//! - **This PC**, a folder fc with the boards' sessions (`E-Reader`, `E-Reader-Calls-1` and
//!   `-2`), the session of every construct, one of hostile content, one with no conversation of
//!   its own, and one whose
//!   later part was damaged in the archive after it was synced; a folder toolbox-rs with the
//!   board's audit (`E-Reader-Subagent`). Every construct's transcript ends inside a record,
//!   archived so by a second sync.
//! - **Ubuntu-24.04**, a WSL distro, with the garden session of `E-Edge-Source-Gone`, synced
//!   and then deleted at its source, so its main transcript is gone there.
//!
//! With [`Extra::Large`], This PC also holds the large session of the reader's budgets
//! (PLAN §14); with [`Extra::Versions`], toolbox-rs also holds the session of `E-Versions`,
//! whose transcript has three versions: the first extended by a second sync, a second that
//! rewrote it, and a third, shorter, held back (DESIGN §4.3); with [`Extra::Facts`], fc also
//! holds a session whose facts are as long as they come, which loads two memory files on its
//! way (DESIGN §4.2.8, §4.2.11).

use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use sb_core::CancelToken;
use sb_core::archive::Archive;
use sb_core::hash::Hash;
use sb_core::location::{LocationId, LocationKind};
use sb_core::provider::{self, Provider};
use sb_core::sync::sync_location;
use sb_test_support::ProjectsTree;
use sb_test_support::reader::{self, FC, Large, OPUS_55, Script, TOOLBOX};

/// What the reader's library holds besides its sessions (the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Extra {
    None,
    Large,
    Versions,
    Facts,
}

/// Syncs `tree` into `location`, all of it.
fn sync(
    archive: &Archive,
    location: &LocationId,
    tree: &ProjectsTree,
    providers: &[Box<dyn Provider>],
) -> Result<()> {
    for pass in sync_location(
        archive,
        location,
        &tree.transport(),
        providers,
        &CancelToken::new(),
        &mut |_| {},
    )? {
        let report = pass.outcome?;
        let index = pass.index?;
        ensure!(
            report.failed.is_empty() && index.failed.is_empty(),
            "the reader's library did not sync whole: {report:?}, {index:?}"
        );
    }
    Ok(())
}

/**
 * `E-Versions`' session, synced into `location` three times over (the module docs): extended,
 * rewritten, then shrunk to its first turn, which is held back.
 */
fn versions(
    archive: &Archive,
    location: &LocationId,
    pc: &mut ProjectsTree,
    providers: &[Box<dyn Provider>],
) -> Result<()> {
    let mut session = pc.session(TOOLBOX, 0);
    {
        let mut s = Script::new(&mut session.transcript, "2026-11-08", "v");
        s.at("21:58:00").prompt(
            "Changed lines in dv highlight the whole line. Add word-level highlighting inside \
             them, like GitHub does, without slowing down a 10k-line diff.",
        );
        s.at("21:58:20").reply(
            OPUS_55,
            "The row renderer already gets both sides of a changed pair, so the word diff can \
             run per pair, lazily, when a row first paints.",
        );
    }
    session.title("Word-level marks in dv's diff");
    sync(archive, location, pc, providers)?;
    {
        let mut s = Script::new(&mut session.transcript, "2026-11-08", "w");
        s.at("22:10:00").prompt("Cap it for minified lines.");
        s.at("22:10:30").reply(
            OPUS_55,
            "Pairs past 10,000 token products are left unmarked.",
        );
    }
    sync(archive, location, pc, providers)?;
    let file = pc.path(&session.key);
    let text = std::fs::read_to_string(&file)?;
    std::fs::write(
        &file,
        text.replacen("Cap it for minified", "Cap it for very long", 1),
    )?;
    sync(archive, location, pc, providers)?;
    let first: String = text
        .lines()
        .take(2)
        .map(|line| format!("{line}\n"))
        .collect();
    std::fs::write(&file, first)?;
    sync(archive, location, pc, providers)?;
    ensure!(
        archive
            .catalogue()
            .sessions(sb_core::catalogue::Shelf::All)?
            .iter()
            .any(|it| it.title.as_deref() == Some("Word-level marks in dv's diff") && it.held_back),
        "the session of E-Versions is not held back"
    );
    Ok(())
}

/// The segment files of the item `key`, in the order of their versions and offsets.
fn segments_of(archive_dir: &Path, key: &str) -> Result<Vec<std::path::PathBuf>> {
    /* An item's folder is named by the start of its key's hash (PLAN §4.1). */
    let folder = Hash::of(key.as_bytes()).to_hex()[..32].to_string();
    let mut found = Vec::new();
    let mut stack = vec![archive_dir.join("files")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|it| it == "zst")
                && path
                    .iter()
                    .any(|part| part.to_str() == Some(folder.as_str()))
            {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Builds the archive in `archive_dir` (the module docs), writing the locations' transcripts
/// under `scratch`, which is removed again.
pub fn build(archive_dir: &Path, scratch: &Path, extra: Extra) -> Result<()> {
    let providers = provider::builtin();
    let archive = Archive::create(archive_dir)?;
    let catalogue = archive.catalogue();

    let mut pc = ProjectsTree::in_home(&scratch.join("pc"));
    reader::playlist(&mut pc);
    reader::backfill(&mut pc);
    reader::constructs(&mut pc);
    reader::subagents_only(&mut pc);
    reader::hostile(&mut pc, 2 * 1024 * 1024);
    reader::audit(&mut pc);
    let mut damaged = pc.session(FC, 40);
    damaged.title("A session damaged in the archive");
    match extra {
        Extra::Large => {
            reader::large(&mut pc, Large::BUDGET);
        }
        Extra::Facts => {
            reader::facts(&mut pc);
        }
        Extra::None | Extra::Versions => {}
    }
    let here = catalogue.add_location(LocationKind::Local, "This PC")?;
    sync(&archive, &here.id, &pc, &providers)?;
    /* A second pass archives the tail of the transcript cut inside a record, and the damaged
    session's second segment, which is then broken. */
    damaged.append(40);
    sync(&archive, &here.id, &pc, &providers)?;
    let last = segments_of(archive_dir, &damaged.key)?
        .pop()
        .context("the damaged session has segments")?;
    std::fs::write(&last, b"not the segment that was archived")?;
    if extra == Extra::Versions {
        versions(&archive, &here.id, &mut pc, &providers)?;
    }

    let mut wsl = ProjectsTree::in_home(&scratch.join("wsl"));
    let garden = reader::garden(&mut wsl);
    let distro = catalogue.add_location(
        LocationKind::Wsl {
            distro: "Ubuntu-24.04".to_string(),
        },
        "Ubuntu-24.04",
    )?;
    sync(&archive, &distro.id, &wsl, &providers)?;
    std::fs::remove_file(wsl.path(&garden.key))?;
    sync(&archive, &distro.id, &wsl, &providers)?;

    for (name, path) in [("fc", FC), ("toolbox-rs", TOOLBOX)] {
        let folder = catalogue.create_folder(name, None)?;
        catalogue.attach_path(folder, path, None)?;
    }
    drop(archive);
    std::fs::remove_dir_all(scratch)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sb_core::catalogue::Shelf;
    use sb_core::provider::{ItemKind, SessionItems as _};

    use super::*;

    #[test]
    fn the_versions_session_has_three_versions_the_newest_held_back() {
        let dir = tempfile::tempdir().unwrap();
        let archive_dir = dir.path().join("archive");
        build(&archive_dir, &dir.path().join("locations"), Extra::Versions).unwrap();
        let archive = Archive::open(&archive_dir).unwrap();
        let session = archive
            .catalogue()
            .sessions(Shelf::All)
            .unwrap()
            .into_iter()
            .find(|it| it.title.as_deref() == Some("Word-level marks in dv's diff"))
            .unwrap();
        let key = archive
            .session(&session.id)
            .unwrap()
            .list()
            .iter()
            .find(|item| item.kind == ItemKind::Transcript)
            .unwrap()
            .key
            .clone();
        let history = archive
            .catalogue()
            .history(&session.id, &key)
            .unwrap()
            .unwrap();
        let numbers: Vec<(u32, usize, bool)> = history
            .versions
            .iter()
            .map(|(version, extended)| (version.number, *extended, version.held_back.is_some()))
            .collect();
        assert_eq!(numbers, [(1, 1, false), (2, 0, false), (3, 0, true)]);
        assert_eq!(history.reading, history.versions[1].0.id);
    }
}
