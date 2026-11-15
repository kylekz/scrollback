//! A temporary command-line trigger for the sync engine (PLAN §13), for use until the app has
//! locations in its UI. It syncs a location's agents into the archive in a folder, creating
//! the archive if the folder has none, or verifies that archive, or sets aside what verify
//! found under its `files/` that the catalogue doesn't record. It prints totals only.
//!
//! ```text
//! cargo run --release -p sb_core --example sync -- <archive folder> [<location>]
//! cargo run --release -p sb_core --example sync -- <archive folder> verify
//! cargo run --release -p sb_core --example sync -- <archive folder> set-aside
//! cargo run --release -p sb_core --example sync -- test <location>
//! cargo run --release -p sb_core --example sync -- distros
//! ```
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

use std::error::Error;
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use sb_core::CancelToken;
use sb_core::archive::{Archive, ArchiveError};
use sb_core::location::LocationKind;
use sb_core::provider;
use sb_core::sync::{Progress, SyncError, sync_location};
use sb_core::transport::{Launcher, ShellTransport, Transport, wsl};

const USAGE: &str = "usage: sync <archive folder> [<location> | verify | set-aside]
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
                /* As the distro's shell is started, with `env` before the shell. */
                let mut words = Launcher::Wsl {
                    distro: distro.clone(),
                }
                .words();
                words.truncate(words.len() - 2);
                words.extend(["env".to_string(), format!("HOME={home}")]);
                (
                    wsl_distro(&distro),
                    format!("{distro} {home}"),
                    command(words),
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
    let ahead =
        probe.clock_ns as f64 / 1e9 - SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
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
        None => println!("variables: the login shell did not answer; a bare command's"),
    }
    if probe.missing.is_empty() {
        println!("tools: all there");
        Ok(())
    } else {
        Err(format!("tools missing: {}", probe.missing.join(", ")).into())
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
        [dir, action @ ("verify" | "set-aside")] => (dir, action, Place::from(&[])?),
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
        },
    )?;
    if passes.is_empty() {
        println!("no agent was found there");
    }
    for pass in passes {
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
    println!("synced in {:.1?}", started.elapsed());
    Ok(())
}
