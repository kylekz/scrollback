//! A temporary command-line trigger for the sync engine (PLAN §13), for use until the app has
//! locations in its UI. It syncs a location's agents into the archive in a folder, creating
//! the archive if the folder has none, and indexes what the sync changed; or verifies that
//! archive, listing what is under its `files/` that the catalogue doesn't record; or sets
//! that aside; or brings the session index up to date as a start does; or empties it and
//! builds it again from the archive, and says whether it came back the same; or searches it
//! (PLAN §10) with the palette's syntax (DESIGN §4.9), printing the results; or moves it into
//! another folder (PLAN §4.4), timing the copy. It prints totals, and of the archive's own
//! files their paths in it.
//!
//! ```text
//! cargo run --release -p sb_core --example sync -- <archive folder> [<location>]
//! cargo run --release -p sb_core --example sync -- <archive folder> verify
//! cargo run --release -p sb_core --example sync -- <archive folder> set-aside
//! cargo run --release -p sb_core --example sync -- test <location>
//! cargo run --release -p sb_core --example sync -- distros
//! cargo run --release -p sb_core --example sync -- <archive folder> index
//! cargo run --release -p sb_core --example sync -- <archive folder> reindex
//! cargo run --release -p sb_core --example sync -- <archive folder> search <query>
//! cargo run --release -p sb_core --example sync -- <archive folder> copies
//! cargo run --release -p sb_core --example sync -- <archive folder> move <empty folder>
//! ```
//!
//! `copies` counts, of the indexed archive, the sessions the library shows, the copies of
//! sessions that ran elsewhere shown alone, and the copies hidden behind their originals by
//! how each compares with its original (PLAN §7.7). Counts only.
//!
//! `move` copies the archive into the folder and finishes the move there as Settings › Move…
//! does, with no settings file to point at the copy: the old folder stays as it was, and the
//! copy is an archive of its own. It prints how many files a second the copy ran at.
//!
//! A search prints how many matches it found in how many sessions and how long it took, on
//! its first line, then each session it found and its first matches.
//!
//! `<location>` is this machine when left out, or one of:
//!
//! ```text
//! --wsl Ubuntu-24.04                             a WSL distro's default user
//! --wsl Ubuntu-24.04 --home /home/user/fixture   the distro, with that folder for a home
//! --ssh user@macbook                             a host, as `ssh` takes it
//! --loopback /home/user/fixture                  this machine through a local `sh`, with
//!                                                that folder for a home
//! ```
//!
//! The two with a home folder are for trying the shell transport on a generated tree instead
//! of the user's own data. `test` is the connection test of a location that is reached through
//! a shell; `distros` lists the WSL distros without starting any.

use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::path::Path;
use std::time::Instant;

use sb_core::CancelToken;
use sb_core::archive::{Archive, ArchiveError};
use sb_core::catalogue::{Catalogue, Compared, Session, Shelf};
use sb_core::location::LocationKind;
use sb_core::provider;
use sb_core::search::{self, Calendar, Limit, Names, Searched};
use sb_core::sync::{IndexReport, Progress, SyncError, attribute, index, sync_location};
use sb_core::transcript::Ending;
use sb_core::transport::{Launcher, ShellTransport, SshConfig, Transport, wsl};

const USAGE: &str = "usage: sync <archive folder> [<location>]
       sync <archive folder> verify | set-aside | index | reindex
       sync <archive folder> search <query>
       sync <archive folder> copies
       sync <archive folder> move <empty folder>
       sync test <location>
       sync distros
<location>: --wsl <distro> [--home <folder>] | --ssh <host> | --loopback <home folder>";

/// A location named on the command line.
struct Place {
    kind: LocationKind,
    name: String,
    /// What reaches it, when that is not what its kind is reached by: a shell that is given
    /// a home folder of the caller's choosing.
    launcher: Option<Launcher>,
}

impl Place {
    fn from(args: &[&str]) -> Result<Self, Box<dyn Error>> {
        /* A launcher that runs `words` and then a shell that reads its script on stdin. */
        let command = |mut words: Vec<String>| {
            words.extend(["sh", "-s"].map(str::to_string));
            Some(Launcher::Command {
                program: words.remove(0).into(),
                args: words,
            })
        };
        let (kind, name, launcher) = match *args {
            [] => (LocationKind::Local, "This machine".to_string(), None),
            ["--wsl", distro] => {
                let distro = spelled(distro)?;
                (wsl_distro(&distro), distro, None)
            }
            ["--wsl", distro, "--home", home] => {
                let distro = spelled(distro)?;
                let launcher = Launcher::Wsl {
                    distro: distro.clone(),
                }
                .reading_home(home);
                (
                    wsl_distro(&distro),
                    format!("{distro} {home}"),
                    Some(launcher),
                )
            }
            ["--ssh", host] => (
                LocationKind::Ssh {
                    host: host.to_string(),
                },
                host.to_string(),
                None,
            ),
            /* Nothing of this process's environment but its PATH reaches the shell. */
            ["--loopback", home] => (
                LocationKind::Local,
                format!("Loopback {home}"),
                command(vec![
                    "env".to_string(),
                    "-i".to_string(),
                    format!("PATH={}", std::env::var("PATH")?),
                    format!("HOME={home}"),
                ]),
            ),
            _ => return Err(USAGE.into()),
        };
        Ok(Self {
            kind,
            name,
            launcher,
        })
    }

    /// What starts the shell the location is read through, if it is read through one.
    fn launcher(&self) -> Option<Launcher> {
        self.launcher.clone().or(match &self.kind {
            LocationKind::Local => None,
            LocationKind::Wsl { distro } => Some(Launcher::Wsl {
                distro: distro.clone(),
            }),
            LocationKind::Ssh { host } => Some(Launcher::Ssh { host: host.clone() }),
        })
    }
}

fn wsl_distro(distro: &str) -> LocationKind {
    LocationKind::Wsl {
        distro: distro.to_string(),
    }
}

/// The distro `name` as WSL spells it: `wsl.exe` takes a name in any case, and a location is
/// kept under the one spelling. A name it does not list is left as it was given.
fn spelled(name: &str) -> Result<String, Box<dyn Error>> {
    let distros = wsl::distros(&CancelToken::new())?.unwrap_or_default();
    Ok(wsl::find(&distros, name)
        .map_or(name, |distro| &distro.name)
        .to_string())
}

/// The connection test (DESIGN §4.12).
fn test(place: &Place) -> Result<(), Box<dyn Error>> {
    let launcher = place
        .launcher()
        .ok_or("this machine is not reached through a shell: there is nothing to test")?;
    println!("running: {}", launcher.command_line());
    let probe = ShellTransport::new(launcher)
        .probe()
        .map_err(|error| error.to_string())?;
    let ahead = probe.clock_ahead_ns as f64 / 1e9;
    println!(
        "connected in {:.2?}: {} answered, its home is {}, its clock is {ahead:+.1} s from \
         this machine's, modification times come in {}",
        probe.took,
        probe.shell,
        probe.home,
        if probe.whole_seconds {
            "whole seconds"
        } else {
            "nanoseconds"
        }
    );
    match &probe.login_shell {
        Some(shell) => println!("variables: as the login shell {shell} has them"),
        None => println!(
            "variables: the login shell did not answer (csh and tcsh never do); a bare command's"
        ),
    }
    if let Some(mount) = &probe.drive_mount {
        println!("drives: mounted under {mount}");
    }
    if probe.missing.is_empty() {
        println!("tools: all there");
        Ok(())
    } else {
        Err(format!("tools missing: {}", probe.missing.join(", ")).into())
    }
}

/// What an index did, in a line.
fn indexed(report: &IndexReport) -> String {
    format!(
        "indexed {} sessions: {} could not be parsed, {} filed by a rule, {} versions held \
         back for how they parse, {} deleted for good as of a conversation deleted for good",
        report.sessions,
        report.failed.len(),
        report.filed,
        report.held_back,
        report.deleted.len()
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
         through, {} with a part that could not be read, {} that could not be parsed, {} \
         with a version held back",
        sessions.len(),
        count(&|session| session.title.is_some()),
        count(&|session| session.project.is_some()),
        count(&|session| session.main_item.is_none() && session.failure.is_none()),
        count(&|session| session.ending == Some(Ending::MidRecord)),
        count(&|session| matches!(session.ending, Some(Ending::Unreadable(_)))),
        count(&|session| session.unread),
        count(&|session| session.failure.is_some()),
        count(&|session| session.held_back),
    );
    Ok(())
}

/// Counts the archive's copies of sessions that ran elsewhere, as the library shows them.
fn copies(archive: &Archive) -> Result<(), Box<dyn Error>> {
    let catalogue = archive.catalogue();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut counted = HashSet::new();
    for shelf in [Shelf::All, Shelf::Trash] {
        for session in catalogue.sessions(shelf)? {
            *counts.entry("sessions shown").or_default() += 1;
            if session.copy.is_some() {
                *counts
                    .entry("copies shown alone, with no original")
                    .or_default() += 1;
            }
            /* A copy behind two originals is counted once. */
            for kin in archive.kin(&session.id)? {
                if kin.copy.is_none() || !counted.insert(kin.id) {
                    continue;
                }
                let what = match kin.compared {
                    Compared::Same => "copies hidden: the same as the session shown",
                    Compared::Shorter => "copies hidden: behind the session shown",
                    Compared::Longer => "copies hidden: ahead of the session shown",
                    Compared::Differs => "copies hidden: neither holds the other",
                };
                *counts.entry(what).or_default() += 1;
            }
        }
    }
    for (what, count) in counts {
        println!("{what}: {count}");
    }
    Ok(())
}

/// Searches the archive as the palette does, and prints what it found.
fn find(archive: &Archive, input: &str) -> Result<(), Box<dyn Error>> {
    let catalogue = archive.catalogue();
    let started = Instant::now();
    let agents: Vec<_> = provider::builtin()
        .iter()
        .map(|provider| provider.id())
        .collect();
    let (query, unresolved) = search::query(
        input,
        &Names {
            folders: &catalogue.folders()?,
            locations: &catalogue.locations()?,
            agents: &agents,
            calendar: Calendar::Local,
        },
    );
    let results = match archive.search(&query, Limit::default(), &CancelToken::new())? {
        Searched::KeepTyping => {
            println!("too short to look for: a letter or two of ASCII matches nearly everything");
            return Ok(());
        }
        Searched::Results(results) => results,
    };
    println!(
        "{} matches in {} sessions in {:.1?}",
        results.total_matches,
        results.total_sessions,
        started.elapsed()
    );
    let unsearchable = archive.unsearchable()?;
    if unsearchable > 0 {
        println!("{unsearchable} sessions are not searchable yet: an index makes them so");
    }
    for token in unresolved {
        println!(
            "{}:{} names nothing; it is left out",
            token.key.as_str(),
            token.value
        );
    }
    for found in &results.sessions {
        println!(
            "{} ({} matches)",
            found.session.title.as_deref().unwrap_or("untitled"),
            found.matches
        );
        for found in &found.shown {
            /* Each match between brackets. */
            let mut text = found.snippet.text.clone();
            for hit in found.snippet.hits.iter().rev() {
                text.insert(hit.end, ']');
                text.insert(hit.start, '[');
            }
            println!(
                "  {:?}{} at {:?}: {}",
                found.kind,
                if found.subagent.is_some() {
                    " in a subagent"
                } else {
                    ""
                },
                found.at,
                text.replace('\n', " ")
            );
        }
        if found.matches > found.shown.len() {
            println!(
                "  +{} more in this session",
                found.matches - found.shown.len()
            );
        }
    }
    Ok(())
}

/// Moves `archive` into the folder `to`, timing the copy and the finish.
fn move_to(archive: &Archive, to: &Path) -> Result<(), Box<dyn Error>> {
    let plan = archive.plan_move(to)?;
    println!("moving {} files, {} bytes", plan.files, plan.bytes);
    let mut moving = archive.start_move(&plan)?;
    let started = Instant::now();
    if let Err(error) = moving.copy(archive, &CancelToken::new(), &mut |_| {}) {
        moving.abandon();
        return Err(error.into());
    }
    let copied = started.elapsed();
    println!(
        "copied in {copied:.1?}: {:.0} files a second",
        plan.files as f64 / copied.as_secs_f64()
    );
    let started = Instant::now();
    match moving.finish(archive, &CancelToken::new(), &mut |_| {}, |_| Ok(())) {
        Ok(moved) => {
            moved.wait_for_removals();
            println!("finished in {:.1?}", started.elapsed());
            Ok(())
        }
        Err(error) => {
            moving.abandon();
            Err(error.into())
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (dir, action, place) = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["distros"] => {
            match wsl::distros(&CancelToken::new())? {
                None => println!("WSL is not installed here"),
                Some(distros) => {
                    for distro in &distros {
                        println!(
                            "{}: {}{}{}",
                            distro.name,
                            if distro.running {
                                "running"
                            } else {
                                "not running"
                            },
                            if distro.default { ", the default" } else { "" },
                            if distro.utility {
                                ", a container tool's own"
                            } else {
                                ""
                            }
                        );
                    }
                    println!("{} distros", distros.len());
                }
            }
            return Ok(());
        }
        ["test", ref place @ ..] => return test(&Place::from(place)?),
        [dir, action @ ("verify" | "set-aside" | "index" | "reindex")] => {
            (dir, action, Place::from(&[])?)
        }
        [dir, "copies"] => return copies(&Archive::open(Path::new(dir))?),
        [dir, "move", to] => return move_to(&Archive::open(Path::new(dir))?, Path::new(to)),
        [dir, "search", ref query @ ..] => {
            let archive = Archive::open(Path::new(dir))?;
            return find(&archive, &query.join(" "));
        }
        [dir, ref place @ ..] => (dir, "sync", Place::from(place)?),
        [] => return Err(USAGE.into()),
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
        archive.wait_for_removals();
        println!(
            "{} folders of sessions deleted for good are still to be removed; they are not set \
             aside",
            archive.pending_removals()?
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
        /* What the catalogue doesn't record is no damage: every archived byte is as it was. */
        for unrecorded in &report.uncatalogued {
            println!("not recorded by the catalogue: {}", unrecorded.display());
        }
        for removing in &report.removing {
            println!(
                "of a session deleted for good, still to be removed: {}",
                removing.display()
            );
        }
        if !(report.damaged.is_empty() && report.unread.is_empty()) {
            return Err(format!(
                "{} versions are damaged and {} could not be read",
                report.damaged.len(),
                report.unread.len()
            )
            .into());
        }
        return Ok(());
    }

    let catalogue = archive.catalogue();
    if action == "index" || action == "reindex" {
        let before = catalogue.sessions(Shelf::All)?;
        if action == "reindex" {
            catalogue.drop_index()?;
        }
        let report = index(
            &archive,
            None,
            &provider::builtin(),
            &CancelToken::new(),
            &mut |progress| match progress {
                Progress::Indexing {
                    sessions,
                    bytes,
                    outdated,
                } => println!(
                    "indexing {sessions} sessions, {bytes} bytes; {outdated} of them made by \
                     another version"
                ),
                Progress::Searching { sessions } => {
                    println!(
                        "making {sessions} sessions searchable after {:.1?}",
                        started.elapsed()
                    );
                }
                Progress::Compacting => {
                    println!(
                        "compacting the search index after {:.1?}",
                        started.elapsed()
                    );
                }
                Progress::Waiting => println!("waiting for another index"),
                _ => {}
            },
        )?;
        println!("{} in {:.1?}", indexed(&report), started.elapsed());
        attributed(&archive)?;
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
        return if action == "index" || before == after {
            Ok(())
        } else {
            Err("the index did not come back as it was".into())
        };
    }
    let known = catalogue
        .locations()?
        .into_iter()
        .find(|location| location.kind.same_place(&place.kind) && location.name == place.name);
    let location = match known {
        Some(location) => location,
        None => catalogue.add_location(place.kind.clone(), &place.name)?,
    };
    let cancel = CancelToken::new();
    let transport: Box<dyn Transport> = match place.launcher.clone() {
        Some(launcher) => Box::new(ShellTransport::new(launcher).cancelled_by(&cancel)),
        None => place.kind.transport(&cancel),
    };
    let mut planned = (0, 0);
    let mut fetched = 0;
    let mut indexing = None;
    let passes = sync_location(
        &archive,
        &location.id,
        transport.as_ref(),
        &provider::builtin(),
        &cancel,
        &mut |progress| match *progress {
            Progress::Scanned { items } => {
                println!("scanned {items} items after {:.1?}", started.elapsed());
            }
            Progress::Planned { items, bytes } => {
                planned = (items, bytes);
                println!(
                    "planned {items} items, {bytes} bytes after {:.1?}",
                    started.elapsed()
                );
            }
            Progress::Fetched { bytes } => fetched = bytes,
            Progress::Stored { items } => println!(
                "fetched {items} of {} items, {fetched} of {} bytes",
                planned.0, planned.1
            ),
            Progress::Indexing {
                sessions, bytes, ..
            } => {
                indexing = Some(Instant::now());
                println!("indexing {sessions} sessions, {bytes} bytes");
            }
            Progress::Searching { sessions } => {
                println!("making {sessions} sessions searchable");
            }
            Progress::Compacting => println!("compacting the search index"),
            Progress::Indexed { .. } | Progress::Searched { .. } | Progress::Waiting => {}
        },
    )?;
    if passes.is_empty() {
        println!("no agent was found there");
    }
    for pass in passes {
        match &pass.index {
            Ok(index) => println!("{}: {}", pass.provider, indexed(index)),
            Err(error) => println!("{}: the index failed: {error}", pass.provider),
        }
        let report = match pass.outcome {
            Ok(report) => report,
            /* Not an error to the app; here it is why the sync stopped. */
            Err(SyncError::Unreachable(reason)) => {
                return Err(
                    format!("unreachable, which is not counted as a failure: {reason}").into(),
                );
            }
            Err(error) => {
                println!("{}: failed: {error}", pass.provider);
                continue;
            }
        };
        println!(
            "{}: {} items: {} unchanged, {} new, {} appended, {} touched, {} new versions \
             ({} held back), {} gone, {} deferred, {} failed, {} unlisted, {} skipped, {} of \
             sessions deleted for good; {} bytes fetched, {} stored",
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
            report.deleted,
            report.bytes_fetched,
            report.bytes_stored
        );
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
    attributed(&archive)?;
    print_index(catalogue)?;
    Ok(())
}

/// Says where the copies kept on this machine ran, through this machine's `ssh -G`, as the app
/// does after a sync (PLAN §7.7), and how many copies' attribution that changed.
fn attributed(archive: &Archive) -> Result<(), Box<dyn Error>> {
    let changed = attribute(archive, &SshConfig::default(), &CancelToken::new())?;
    println!("where copies ran: {changed} changed");
    Ok(())
}
