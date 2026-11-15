//! A temporary command-line trigger for the sync engine (PLAN §13), for use until the app has
//! locations in its UI. It syncs this machine's agents into the archive in a folder, creating
//! the archive if the folder has none, and indexes what the sync changed; or verifies that
//! archive; or sets aside what verify found under its `files/` that the catalogue doesn't
//! record; or empties the session index and builds it again from the archive, and says
//! whether it came back the same. It prints totals only.
//!
//! ```text
//! cargo run --release -p sb_core --example sync -- <archive folder>
//! cargo run --release -p sb_core --example sync -- <archive folder> verify
//! cargo run --release -p sb_core --example sync -- <archive folder> set-aside
//! cargo run --release -p sb_core --example sync -- <archive folder> reindex
//! ```

use std::error::Error;
use std::path::Path;
use std::time::Instant;

use sb_core::CancelToken;
use sb_core::archive::{Archive, ArchiveError};
use sb_core::catalogue::{Catalogue, Session, Shelf};
use sb_core::location::LocationKind;
use sb_core::provider;
use sb_core::sync::{IndexReport, Progress, index, sync_location};
use sb_core::transcript::Ending;
use sb_core::transport::LocalTransport;

/// What an index did, in a line.
fn indexed(report: &IndexReport) -> String {
    format!(
        "indexed {} sessions: {} could not be parsed, {} filed by a rule, {} versions held \
         back for how they parse",
        report.sessions,
        report.failed.len(),
        report.filed,
        report.held_back
    )
}

/// Prints what the index holds, as counts.
fn print_index(catalogue: &Catalogue) -> Result<(), Box<dyn Error>> {
    let sessions = catalogue.sessions(Shelf::All)?;
    let count =
        |of: &dyn Fn(&Session) -> bool| sessions.iter().filter(|session| of(session)).count();
    println!(
        "the index holds {} sessions: {} with a title, {} with a project path, {} without a \
         transcript of their own, {} that end inside a record, {} that could not be read \
         through, {} that could not be parsed, {} with a version held back",
        sessions.len(),
        count(&|session| session.title.is_some()),
        count(&|session| session.project.is_some()),
        count(&|session| session.main_item.is_none() && session.failure.is_none()),
        count(&|session| session.ending == Some(Ending::MidRecord)),
        count(&|session| matches!(session.ending, Some(Ending::Unreadable(_)))),
        count(&|session| session.failure.is_some()),
        count(&|session| session.held_back),
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (dir, action) = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [dir] => (dir, "sync"),
        [dir, action @ ("verify" | "set-aside" | "reindex")] => (dir, action),
        _ => return Err("usage: sync <archive folder> [verify | set-aside | reindex]".into()),
    };
    let archive = match Archive::open(Path::new(dir)) {
        Err(ArchiveError::NotAnArchive(_)) => Archive::create(Path::new(dir))?,
        opened => opened?,
    };
    let started = Instant::now();
    if action == "set-aside" {
        println!(
            "set aside {} files or folders the catalogue doesn't record, in orphans/",
            archive.set_aside_uncatalogued()?.len()
        );
        return Ok(());
    }
    if action == "verify" {
        let report = archive.verify(&CancelToken::new())?;
        println!(
            "verified {} versions in {} segments: {} bytes, stored in {} ({:.1}x), in {:.1?}",
            report.versions,
            report.segments,
            report.bytes,
            report.stored_bytes,
            report.bytes as f64 / report.stored_bytes.max(1) as f64,
            started.elapsed()
        );
        for damage in &report.damaged {
            println!(
                "damaged: version {} of an item: {:?}",
                damage.version, damage.problems
            );
        }
        for unread in &report.unread {
            println!(
                "not read: version {} of an item: {}",
                unread.version, unread.error
            );
        }
        if !(report.damaged.is_empty()
            && report.unread.is_empty()
            && report.uncatalogued.is_empty())
        {
            return Err(format!(
                "{} versions are damaged, {} could not be read, and the catalogue doesn't \
                 record {} files or folders under files/",
                report.damaged.len(),
                report.unread.len(),
                report.uncatalogued.len()
            )
            .into());
        }
        return Ok(());
    }

    let catalogue = archive.catalogue();
    if action == "reindex" {
        let before = catalogue.sessions(Shelf::All)?;
        catalogue.drop_index()?;
        let report = index(
            &archive,
            None,
            &provider::builtin(),
            &CancelToken::new(),
            &mut |progress| {
                if let Progress::Indexing { sessions, bytes } = progress {
                    println!("indexing {sessions} sessions, {bytes} bytes");
                }
            },
        )?;
        println!("{} in {:.1?}", indexed(&report), started.elapsed());
        let after = catalogue.sessions(Shelf::All)?;
        let differ = before
            .iter()
            .zip(&after)
            .filter(|(before, after)| before != after)
            .count();
        println!(
            "the index held {} sessions and holds {}, of which {differ} are not as they were",
            before.len(),
            after.len()
        );
        print_index(catalogue)?;
        return if before == after {
            Ok(())
        } else {
            Err("the index did not come back as it was".into())
        };
    }
    let location = match catalogue.locations()?.into_iter().next() {
        Some(location) => location,
        None => catalogue.add_location(LocationKind::Local, "This machine")?,
    };
    let mut planned = (0, 0);
    let mut fetched = 0;
    let mut indexing = None;
    let passes = sync_location(
        &archive,
        &location.id,
        &LocalTransport,
        &provider::builtin(),
        &CancelToken::new(),
        &mut |progress| match *progress {
            Progress::Scanned { items } => println!("scanned {items} items"),
            Progress::Planned { items, bytes } => {
                planned = (items, bytes);
                println!("planned {items} items, {bytes} bytes");
            }
            Progress::Fetched { bytes } => fetched = bytes,
            Progress::Stored { items } => println!(
                "fetched {items} of {} items, {fetched} of {} bytes",
                planned.0, planned.1
            ),
            Progress::Indexing { sessions, bytes } => {
                indexing = Some(Instant::now());
                println!("indexing {sessions} sessions, {bytes} bytes");
            }
            Progress::Indexed { .. } => {}
        },
    )?;
    for pass in passes {
        let report = match pass.outcome {
            Ok(report) => report,
            Err(error) => {
                println!("{}: failed: {error}", pass.provider);
                continue;
            }
        };
        println!(
            "{}: {} items: {} unchanged, {} new, {} appended, {} touched, {} new versions \
             ({} held back), {} gone, {} deferred, {} failed, {} unlisted, {} skipped; {} bytes \
             fetched, {} stored",
            pass.provider,
            report.scanned,
            report.unchanged,
            report.new_items,
            report.appended,
            report.touched,
            report.new_versions,
            report.held_back,
            report.gone,
            report.deferred.len(),
            report.failed.len(),
            report.unlisted.len(),
            report.skipped.len(),
            report.bytes_fetched,
            report.bytes_stored
        );
        println!("{}: {}", pass.provider, indexed(&report.index));
        let mut reasons: Vec<_> = report
            .failed
            .iter()
            .chain(&report.deferred)
            .map(|failure| &failure.reason)
            .chain(report.unlisted.iter().map(|unlisted| &unlisted.reason))
            .collect();
        reasons.sort();
        reasons.dedup();
        for reason in reasons {
            println!("  not archived: {reason}");
        }
    }
    println!(
        "synced in {:.1?}, of which the last index took {:.1?}",
        started.elapsed(),
        indexing
            .map(|indexing| indexing.elapsed())
            .unwrap_or_default()
    );
    print_index(catalogue)?;
    Ok(())
}
