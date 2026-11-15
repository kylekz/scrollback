//! A temporary command-line trigger for the sync engine (PLAN §13), for use until the app has
//! locations in its UI. It syncs this machine's agents into the archive in a folder, creating
//! the archive if the folder has none, or verifies that archive. It prints totals only.
//!
//! ```text
//! cargo run --release -p sb_core --example sync -- <archive folder>
//! cargo run --release -p sb_core --example sync -- <archive folder> verify
//! ```

use std::error::Error;
use std::path::Path;
use std::time::Instant;

use sb_core::CancelToken;
use sb_core::archive::{Archive, ArchiveError};
use sb_core::location::LocationKind;
use sb_core::provider;
use sb_core::sync::{Progress, sync_location};
use sb_core::transport::LocalTransport;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (dir, verify) = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [dir] => (dir, false),
        [dir, "verify"] => (dir, true),
        _ => return Err("usage: sync <archive folder> [verify]".into()),
    };
    let archive = match Archive::open(Path::new(dir)) {
        Err(ArchiveError::NotAnArchive(_)) => Archive::create(Path::new(dir))?,
        opened => opened?,
    };
    let started = Instant::now();
    if verify {
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
        if !report.damaged.is_empty() || !report.unread.is_empty() {
            return Err(format!(
                "{} versions are damaged, {} could not be read",
                report.damaged.len(),
                report.unread.len()
            )
            .into());
        }
        return Ok(());
    }

    let catalogue = archive.catalogue();
    let location = match catalogue.locations()?.into_iter().next() {
        Some(location) => location,
        None => catalogue.add_location(LocationKind::Local, "This machine")?,
    };
    let mut planned = (0, 0);
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
            Progress::Fetched { items, bytes } if items % 100 == 0 || items == planned.0 => {
                println!(
                    "fetched {items} of {} items, {bytes} of {} bytes",
                    planned.0, planned.1
                );
            }
            Progress::Fetched { .. } => {}
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
             ({} held back), {} gone, {} deferred, {} failed, {} unlisted; {} bytes fetched, \
             {} stored",
            pass.provider,
            report.scanned,
            report.unchanged,
            report.new_items,
            report.appended,
            report.touched,
            report.new_versions,
            report.held_back,
            report.gone,
            report.deferred,
            report.failed.len(),
            report.unlisted.len(),
            report.bytes_fetched,
            report.bytes_stored
        );
        let mut reasons: Vec<_> = report
            .failed
            .iter()
            .map(|failure| &failure.reason)
            .chain(report.unlisted.iter().map(|unlisted| &unlisted.reason))
            .collect();
        reasons.sort();
        reasons.dedup();
        for reason in reasons {
            println!("  not archived: {reason}");
        }
    }
    println!("synced in {:.1?}", started.elapsed());
    Ok(())
}
