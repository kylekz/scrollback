//! The owner's locations (PLAN §11: the sync service owns them; DESIGN §4.10–§4.12): adding,
//! renaming and removing one, each a write of its own on a background job; the connection test
//! of a place; and the listing of WSL's distros. Views observe the results. Nothing here blocks
//! the UI thread: `wsl.exe` and `ssh` can take ten seconds and more.
//!
//! **Adding** a place that is a location already ([`LocationKind::same_place`]) is refused.
//! Adding one the owner removed brings that location back, so what was archived from it
//! carries on rather than being archived twice.
//!
//! **Removing** a location (FEATURES: Locations) stops syncing it: a pass that runs is stopped
//! first, and the location is removed once it has. Everything archived from it stays, filed as
//! it was, and nothing reaches the location itself.
//!
//! **The connection test** (PLAN §6.4) runs the launcher's probe, then asks each provider what
//! it finds there. One runs at a time: a new one, or closing what showed it, cancels the last.
//! This machine has no launcher, and its test is the providers' survey alone.

use std::collections::HashMap;

use gpui::Context;
use sb_core::CancelToken;
use sb_core::archive::Archive;
use sb_core::catalogue::{CatalogueError, Holding, LastCheck, ProviderAt};
use sb_core::location::{Location, LocationId, LocationKind, clean_name};
use sb_core::transport::wsl::Distro;
use sb_core::transport::{Launcher, Probe, TransportError};

use super::{Survey, SyncService, surveyed};
use crate::format;

/// After how many days without a good sync a location is stale until the owner says otherwise
/// in Settings (DESIGN §4.11, whose board says "warning after 7 days"; §4.13), and the most and
/// fewest days they can say.
pub const STALE_AFTER_DAYS: u32 = 7;
pub const STALE_AFTER_RANGE: std::ops::RangeInclusive<u32> = 1..=90;

/// The archive's setting (`kv`) that holds the owner's threshold.
pub(super) const STALE_AFTER_KEY: &str = "sync.stale_after_days";

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/**
 * What the archive holds from each location, what each one's last passes found there, and
 * what its shell said the last time it was tested.
 */
#[derive(Clone, Debug, Default)]
pub struct Details {
    holdings: Vec<Holding>,
    found: HashMap<LocationId, Vec<ProviderAt>>,
    checks: HashMap<LocationId, LastCheck>,
}

impl Details {
    pub(super) fn read(archive: &Archive) -> Result<Self, CatalogueError> {
        let catalogue = archive.catalogue();
        let (mut found, mut checks) = (HashMap::new(), HashMap::new());
        for location in catalogue.locations()? {
            found.insert(location.id.clone(), catalogue.providers_at(&location.id)?);
            if let Some(check) = catalogue.last_check(&location.id)? {
                checks.insert(location.id.clone(), check);
            }
        }
        Ok(Self {
            holdings: catalogue.holdings()?,
            found,
            checks,
        })
    }

    /// What `location`'s shell said the last time it was tested, if it ever was.
    pub fn last_check(&self, location: &LocationId) -> Option<&LastCheck> {
        self.checks.get(location)
    }

    /// What the archive holds from each agent at `location`.
    pub fn holdings<'a>(&'a self, location: &'a LocationId) -> impl Iterator<Item = &'a Holding> {
        self.holdings
            .iter()
            .filter(move |holding| holding.location == *location)
    }

    /// How many sessions the archive holds from `location`.
    pub fn sessions(&self, location: &LocationId) -> usize {
        self.holdings(location)
            .map(|holding| holding.sessions)
            .sum()
    }

    /// How many of those sessions are in Trash.
    pub fn trashed(&self, location: &LocationId) -> usize {
        self.holdings(location).map(|holding| holding.trashed).sum()
    }

    /// How many bytes the archive holds from `location`, each item's newest version once.
    pub fn bytes(&self, location: &LocationId) -> u64 {
        self.holdings(location).map(|holding| holding.bytes).sum()
    }

    /// When the archive first held anything from `location`.
    pub fn since(&self, location: &LocationId) -> Option<i64> {
        self.holdings(location).filter_map(|it| it.since).min()
    }

    /// The agents the last passes found at `location`.
    pub fn found(&self, location: &LocationId) -> &[ProviderAt] {
        self.found.get(location).map_or(&[], Vec::as_slice)
    }
}

/// WSL's distros, as listed: `None` when this machine has no WSL, or what `wsl.exe` said when
/// it failed. With what each running distro has, for the first run (DESIGN §4.10).
#[derive(Clone, Debug)]
pub struct Distros {
    pub listed: Result<Option<Vec<Distro>>, String>,
    /// What the providers found in each running distro, by its name, once they have looked.
    pub surveys: HashMap<String, Survey>,
}

impl Distros {
    /// The distros a location can be: those nobody works in left out (PLAN §6.3).
    pub fn offered(&self) -> impl Iterator<Item = &Distro> {
        self.listed
            .iter()
            .flatten()
            .flatten()
            .filter(|distro| !distro.utility)
    }
}

/// A connection test (DESIGN §4.12).
pub struct ConnectionTest {
    /// The place tested.
    pub kind: LocationKind,
    /// The command that was run, as one would type it; `None` for this machine.
    pub command: Option<String>,
    pub state: TestState,
    cancel: CancelToken,
}

#[derive(Clone, Debug)]
pub enum TestState {
    /// Since when, in Unix milliseconds.
    Running(i64),
    /// The shell answered with every tool there (this machine has no shell to ask), and the
    /// providers looked.
    Passed {
        probe: Option<Probe>,
        found: Survey,
    },
    Failed(TestFailure),
}

/// Why a connection test did not pass (PLAN §6.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TestFailure {
    /// It could not be connected to: asleep, off the network, or a name that leads nowhere.
    /// Not an error (PLAN §5). In the launcher's words.
    Unreachable(String),
    /// The sign-in was refused.
    Refused(String),
    /// The host's key was not accepted; nothing here ever accepts one.
    HostKey(String),
    /// The program that reaches it (`ssh`, `wsl.exe`) is not installed on this machine.
    NotInstalled(String),
    /// The tools the location lacks, by name.
    Missing(Vec<String>),
    /// The user's login shell there was stopped before it answered, in scrollback's words.
    LoginHung(String),
    /// Any other failure, in the failing tool's words.
    Other(String),
}

impl TestFailure {
    /// The failure a connection test ended in; `None` for one that was cancelled.
    fn of(error: TransportError) -> Option<Self> {
        Some(match error {
            TransportError::Cancelled => return None,
            TransportError::Unreachable(said) => Self::Unreachable(said),
            TransportError::Refused(said) => Self::Refused(said),
            TransportError::HostKey(said) => Self::HostKey(said),
            TransportError::MissingTools(missing) => Self::Missing(missing),
            hung @ TransportError::LoginShellHung(_) => Self::LoginHung(hung.to_string()),
            TransportError::Launch { program, source }
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Self::NotInstalled(program)
            }
            other => Self::Other(other.to_string()),
        })
    }

    /// Whether the place may be added all the same, to be synced once it can be reached
    /// (PLAN §6.4). Not with a key that isn't accepted, tools it lacks, or no `ssh`: those
    /// need the owner's hand first.
    pub fn may_add_anyway(&self) -> bool {
        matches!(
            self,
            Self::Unreachable(_) | Self::Refused(_) | Self::LoginHung(_) | Self::Other(_)
        )
    }
}

/// How stale a location is (PLAN §5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stale {
    /// Whole days since its last good sync.
    pub days: i64,
    /// Since when it has been stale, in Unix milliseconds.
    pub since: i64,
    /// When the first sessions it has not synced start to be deleted there, by the agent that
    /// keeps them the shortest time, as last read there.
    pub expiring: Option<Expiring>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expiring {
    pub provider: String,
    pub retention_days: u32,
    /// The last good sync plus that agent's retention, in Unix milliseconds.
    pub at: i64,
}

/// Whether a location whose last good sync was at `last_success` and whose agents are `found`
/// has gone more than `after_days` without one, seen from `now`. One never synced is not
/// stale: it says so itself.
pub fn stale(
    last_success: Option<i64>,
    found: &[ProviderAt],
    after_days: u32,
    now: i64,
) -> Option<Stale> {
    let last = last_success?;
    let since = last + i64::from(after_days) * DAY_MS;
    if now < since {
        return None;
    }
    let expiring = found
        .iter()
        .filter_map(|it| Some((it, it.retention_days?)))
        .min_by_key(|(_, days)| *days)
        .map(|(it, days)| Expiring {
            provider: it.provider.clone(),
            retention_days: days,
            at: last + i64::from(days) * DAY_MS,
        });
    Some(Stale {
        days: (now - last) / DAY_MS,
        since,
        expiring,
    })
}

/// The location among `locations` that is the place `kind` is already, if one is.
pub fn refusal<'a>(
    kind: &LocationKind,
    mut locations: impl Iterator<Item = &'a Location>,
) -> Option<&'a Location> {
    locations.find(|location| location.kind.same_place(kind))
}

/**
 * What is wrong with `name` as a location's name, if anything: nothing left of it once what
 * can't be seen is taken out ([`clean_name`]), or one of the names `taken` by others,
 * whatever its case.
 */
pub fn name_problem<'a>(name: &str, taken: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let Some(name) = clean_name(name) else {
        return Some("A location needs a name.".to_string());
    };
    taken
        .into_iter()
        .find(|it| it.eq_ignore_ascii_case(&name))
        .map(|it| format!("{it} is another location's name."))
}

/**
 * Keeps what `probe` says as the last check of the location that is the place `kind`, if one
 * is or was. One that can't be kept is told the log: the test's answer stands without it.
 */
fn record_check(archive: &Archive, kind: &LocationKind, probe: &Probe) {
    let catalogue = archive.catalogue();
    let kept = catalogue
        .locations()
        .and_then(|locations| match refusal(kind, locations.iter()) {
            Some(location) => catalogue.record_check(&location.id, &LastCheck::of(probe)),
            None => Ok(()),
        });
    if let Err(error) = kept {
        log::error!("a connection test could not be kept: {error}");
    }
}

impl SyncService {
    /// The locations the owner removed, whose sessions are still in the archive.
    pub fn removed(&self) -> &[Location] {
        &self.removed
    }

    pub fn details(&self) -> &Details {
        &self.details
    }

    /// Why the last of the owner's changes to the locations could not be made.
    pub fn refused(&self) -> Option<&str> {
        self.refused.as_deref()
    }

    pub fn dismiss_refused(&mut self, cx: &mut Context<Self>) {
        self.refused = None;
        cx.notify();
    }

    /// The connection test running or last run.
    pub fn test(&self) -> Option<&ConnectionTest> {
        self.test.as_ref()
    }

    /// WSL's distros, once listed.
    pub fn distros(&self) -> Option<&Distros> {
        self.distros.as_ref()
    }

    /// The far side's home every WSL and SSH location is read from, while the development
    /// override is on.
    pub fn development_home(&self) -> Option<&str> {
        self.development_home.as_deref()
    }

    /// When every location is synced next, in Unix milliseconds; `None` when they are synced
    /// only when the owner asks.
    pub fn next_round(&self) -> Option<i64> {
        self.next_round
    }

    /// How stale `location` is, by the archive's threshold ([`stale`]).
    pub fn stale(&self, location: &Location, now: i64) -> Option<Stale> {
        stale(
            location.last_success,
            self.details.found(&location.id),
            self.stale_after,
            now,
        )
    }

    /// After how many days without a good sync a location is stale (DESIGN §4.13).
    pub fn stale_after_days(&self) -> u32 {
        self.stale_after
    }

    /**
     * Makes a location stale after `days` without a good sync, within [`STALE_AFTER_RANGE`],
     * for this archive: it is kept in its catalogue. Shown at once; a write the catalogue
     * refuses is said like any change to the locations.
     */
    pub fn set_stale_after_days(&mut self, days: u32, cx: &mut Context<Self>) {
        let days = days.clamp(*STALE_AFTER_RANGE.start(), *STALE_AFTER_RANGE.end());
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        if days == self.stale_after || self.closed {
            return;
        }
        self.stale_after = days;
        cx.notify();
        self.run(
            cx,
            move || {
                archive
                    .catalogue()
                    .set_setting(STALE_AFTER_KEY, &days.to_string())
            },
            |this, written, _| {
                if let Err(error) = written {
                    log::error!("the stale threshold could not be kept: {error}");
                    this.refused = Some(format!(
                        "The number of days couldn't be saved: {}.",
                        error.to_string().trim_end_matches('.')
                    ));
                }
            },
        );
    }

    /// Whether `location` is being removed: its pass is stopping, or the removal is written.
    pub fn is_removing(&self, location: &LocationId) -> bool {
        self.removing.contains(location)
    }

    /// Whether the place `kind` is being added.
    pub fn is_adding(&self, kind: &LocationKind) -> bool {
        self.adding.iter().any(|(it, _)| it.same_place(kind))
    }

    /// The names the locations have, but the location `except`, and those of adds under way.
    pub fn names_taken(&self, except: Option<&LocationId>) -> impl Iterator<Item = &str> {
        self.locations
            .iter()
            .flatten()
            .filter(move |location| Some(&location.id) != except)
            .map(|location| location.name.as_str())
            .chain(self.adding.iter().map(|(_, name)| name.as_str()))
    }

    /**
     * The removed location that is the place `kind`, if one is, and whether the name it had
     * is another's now: then it comes back under a new one.
     */
    pub fn restoring(&self, kind: &LocationKind) -> Option<(&Location, bool)> {
        let location = refusal(kind, self.removed.iter())?;
        Some((
            location,
            name_problem(&location.name, self.names_taken(None)).is_some(),
        ))
    }

    /// Reads what the archive holds from each location again.
    pub(super) fn read_details(&mut self, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        self.run(
            cx,
            move || Details::read(&archive),
            |this, details, _| match details {
                Ok(details) => this.details = details,
                Err(error) => log::error!("what the locations hold could not be read: {error}"),
            },
        );
    }

    /**
     * Adds the place `kind` as a location named `name`, and with `sync_now` starts its first
     * sync. A place that is a location already, or is being added, is not added again, nor
     * is a name another location or an add under way has. One the owner removed comes back
     * under the name it had (module docs), or under `name` when another has taken that
     * since. The locations are then as the catalogue orders them.
     */
    pub fn add(
        &mut self,
        kind: LocationKind,
        name: String,
        sync_now: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        if self.is_adding(&kind) || refusal(&kind, self.locations.iter().flatten()).is_some() {
            return;
        }
        let name = match self.restoring(&kind) {
            Some((location, false)) => location.name.clone(),
            _ => {
                if let Some(problem) = name_problem(&name, self.names_taken(None)) {
                    self.refused = Some(problem);
                    cx.notify();
                    return;
                }
                clean_name(&name).unwrap_or(name)
            }
        };
        self.adding.push((kind.clone(), name.clone()));
        // What the shell there said to the test the place was added after is its last check.
        let check = self.test.as_ref().and_then(|test| match &test.state {
            TestState::Passed {
                probe: Some(probe), ..
            } if test.kind.same_place(&kind) => Some(LastCheck::of(probe)),
            _ => None,
        });
        self.run(
            cx,
            {
                let kind = kind.clone();
                move || {
                    let catalogue = archive.catalogue();
                    let place = refusal(&kind, catalogue.locations()?.iter()).cloned();
                    let added = match place {
                        Some(location) if location.removed.is_none() => Err(location.name),
                        Some(location) => {
                            catalogue.restore_location(&location.id, &name)?;
                            Ok(location.id)
                        }
                        None => Ok(catalogue.add_location(kind, &name)?.id),
                    };
                    if let (Ok(id), Some(check)) = (&added, &check) {
                        catalogue.record_check(id, check)?;
                    }
                    Ok::<_, CatalogueError>((
                        added,
                        catalogue.locations()?,
                        Details::read(&archive)?,
                    ))
                }
            },
            move |this, added, cx| {
                this.adding.retain(|(it, _)| !it.same_place(&kind));
                match added {
                    Ok((Ok(id), locations, details)) => {
                        this.take_locations(locations, Some(&id));
                        this.details = details;
                        if sync_now {
                            this.sync(&id, cx);
                        }
                    }
                    Ok((Err(name), locations, details)) => {
                        this.take_locations(locations, None);
                        this.details = details;
                        this.refused = Some(format!("That place is a location already: {name}."));
                    }
                    Err(error) => {
                        log::error!("a location could not be added: {error}");
                        this.refused = Some(format!("The location could not be added: {error}"));
                    }
                }
            },
        );
    }

    /**
     * The locations in the order the catalogue gives them, the order they were added, from
     * what an add read (`read`) just after it wrote `added`. What the service knows of every
     * other location is kept: an add that ran beside this one may have read before it, and
     * one this read missed (added since) goes after the rest.
     */
    fn take_locations(&mut self, read: Vec<Location>, added: Option<&LocationId>) {
        let known: Vec<Location> = self
            .locations
            .take()
            .into_iter()
            .flatten()
            .chain(std::mem::take(&mut self.removed))
            .collect();
        let mut all: Vec<Location> = read
            .into_iter()
            .map(
                |location| match known.iter().find(|it| it.id == location.id) {
                    Some(known) if Some(&known.id) != added => known.clone(),
                    _ => location,
                },
            )
            .collect();
        for location in known {
            if !all.iter().any(|it| it.id == location.id) {
                all.push(location);
            }
        }
        let (removed, locations) = all.into_iter().partition(|it| it.removed.is_some());
        self.locations = Some(locations);
        self.removed = removed;
    }

    /**
     * Names a location `name`, as [`clean_name`] keeps it. A name that is nothing, or another
     * location's, is refused with why, for the field to say; the location keeps its name.
     */
    pub fn rename(
        &mut self,
        id: &LocationId,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if let Some(problem) = name_problem(name, self.names_taken(Some(id))) {
            return Err(problem);
        }
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return Ok(());
        };
        let name = clean_name(name).unwrap_or_default();
        if self.location(id).is_none_or(|it| it.name == name) {
            return Ok(());
        }
        let id = id.clone();
        self.run(
            cx,
            {
                let (id, name) = (id.clone(), name.clone());
                move || archive.catalogue().rename_location(&id, &name)
            },
            move |this, renamed, _| match renamed {
                /* A location removed meanwhile is renamed too, where it is now listed. */
                Ok(()) => {
                    for location in this.locations.iter_mut().flatten().chain(&mut this.removed) {
                        if location.id == id {
                            location.name = name.clone();
                        }
                    }
                }
                Err(error) => {
                    log::error!("a location could not be renamed: {error}");
                    this.refused = Some(format!("The location could not be renamed: {error}"));
                }
            },
        );
        Ok(())
    }

    /// Removes a location (module docs): its pass or its refetch, if one runs, is stopped
    /// first.
    pub fn remove(&mut self, id: &LocationId, cx: &mut Context<Self>) {
        if self.archive.read(cx).archive().is_none() || !self.removing.insert(id.clone()) {
            return;
        }
        if self.passes.contains_key(id) || self.refetching.contains_key(id) {
            self.stop(id);
        } else {
            self.waiting.remove(id);
            self.write_removal(id.clone(), cx);
        }
        cx.notify();
    }

    /// Records that the owner removed `id`, whose pass is over.
    pub(super) fn write_removal(&mut self, id: LocationId, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            self.removing.remove(&id);
            return;
        };
        self.run(
            cx,
            {
                let id = id.clone();
                move || archive.catalogue().remove_location(&id)
            },
            move |this, removed, cx| {
                this.removing.remove(&id);
                match removed {
                    Ok(()) => {
                        let locations = this.locations.get_or_insert_default();
                        if let Some(at) = locations.iter().position(|it| it.id == id) {
                            let mut location = locations.remove(at);
                            location.removed = Some(format::now());
                            this.removed.push(location);
                        }
                        this.errors.remove(&id);
                        this.stopped.remove(&id);
                        this.read_details(cx);
                    }
                    Err(error) => {
                        log::error!("a location could not be removed: {error}");
                        this.refused = Some(format!("The location could not be removed: {error}"));
                    }
                }
            },
        );
    }

    /// Runs the connection test of the place `kind` (module docs), cancelling the last.
    pub fn test_place(&mut self, kind: LocationKind, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        self.cancel_test();
        let launcher = (self.launch)(&kind);
        let cancel = CancelToken::new();
        self.test = Some(ConnectionTest {
            kind: kind.clone(),
            command: launcher.as_ref().map(Launcher::command_line),
            state: TestState::Running(format::now()),
            cancel: cancel.clone(),
        });
        let (tester, connect) = (self.tester.clone(), self.connect.clone());
        let providers = self.providers.clone();
        self.run(
            cx,
            {
                let cancel = cancel.clone();
                move || {
                    let probe = match &launcher {
                        Some(launcher) => match tester(launcher, &cancel) {
                            Ok(probe) => {
                                /* A place that is a location, or was, keeps what its shell
                                said, missing tools and all (DESIGN §4.11). */
                                record_check(&archive, &kind, &probe);
                                if !probe.missing.is_empty() {
                                    return Some(TestState::Failed(TestFailure::Missing(
                                        probe.missing,
                                    )));
                                }
                                Some(probe)
                            }
                            Err(error) => return TestFailure::of(error).map(TestState::Failed),
                        },
                        None => None,
                    };
                    let transport = connect(&kind, &cancel);
                    let found = surveyed(&archive, transport.as_ref(), &providers);
                    (!cancel.is_cancelled()).then_some(TestState::Passed { probe, found })
                }
            },
            move |this, state, cx| {
                // A test cancelled since, or replaced by another, has nothing to say.
                if let (Some(state), Some(test)) = (state, this.test.as_mut())
                    && !cancel.is_cancelled()
                {
                    test.state = state;
                }
                // What it recorded of a location shows in its detail.
                this.read_details(cx);
            },
        );
    }

    /// Stops the connection test, and forgets it.
    pub fn cancel_test(&mut self) {
        if let Some(test) = self.test.take() {
            test.cancel.cancel();
        }
    }

    /// Lists WSL's distros, without starting any, unless that is under way. On a first run
    /// the providers then look in each running one (DESIGN §4.10).
    pub fn list_distros(&mut self, cx: &mut Context<Self>) {
        if self.listing {
            return;
        }
        self.listing = true;
        let list = self.lister.clone();
        self.run(
            cx,
            move || list(&CancelToken::new()).map_err(|error| error.to_string()),
            |this, listed, cx| {
                this.listing = false;
                let distros = Distros {
                    listed,
                    surveys: HashMap::new(),
                };
                let running: Vec<String> = distros
                    .offered()
                    .filter(|distro| distro.running)
                    .map(|distro| distro.name.clone())
                    .collect();
                this.distros = Some(distros);
                if this.is_first_run() {
                    for name in running {
                        this.survey_distro(name, cx);
                    }
                }
            },
        );
    }

    /// Asks each provider what the running distro `name` has.
    fn survey_distro(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(archive) = self.archive.read(cx).archive().cloned() else {
            return;
        };
        let (connect, providers) = (self.connect.clone(), self.providers.clone());
        self.run(
            cx,
            {
                let name = name.clone();
                move || {
                    let distro = LocationKind::Wsl { distro: name };
                    let transport = connect(&distro, &CancelToken::new());
                    surveyed(&archive, transport.as_ref(), &providers)
                }
            },
            move |this, found, _| {
                if let Some(distros) = &mut this.distros {
                    distros.surveys.insert(name, found);
                }
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn a_location_is_stale_a_week_after_its_last_good_sync() {
        let last = 100 * DAY_MS;
        let found = [
            ProviderAt {
                provider: "keeps-long".to_string(),
                retention_days: Some(90),
                root: None,
            },
            ProviderAt {
                provider: "keeps-short".to_string(),
                retention_days: Some(30),
                root: None,
            },
        ];
        let synced = Some(last);
        let week = STALE_AFTER_DAYS;
        assert_eq!(stale(synced, &found, week, last + 6 * DAY_MS), None);
        assert_eq!(stale(None, &found, week, last + 60 * DAY_MS), None);
        /* DESIGN §4.11: 12 days without a sync, and the agent that keeps transcripts the
        shortest time starts deleting 30 days after the last sync. */
        assert_eq!(
            stale(synced, &found, week, last + 12 * DAY_MS + 1000),
            Some(Stale {
                days: 12,
                since: last + 7 * DAY_MS,
                expiring: Some(Expiring {
                    provider: "keeps-short".to_string(),
                    retention_days: 30,
                    at: last + 30 * DAY_MS,
                }),
            })
        );
        let unknown = stale(synced, &[], week, last + 8 * DAY_MS).unwrap();
        assert_eq!(unknown.expiring, None);
        /* The owner's threshold moves when it is stale from, not when the agent deletes. */
        assert_eq!(stale(synced, &found, 14, last + 12 * DAY_MS), None);
        let fortnight = stale(synced, &found, 14, last + 15 * DAY_MS).unwrap();
        assert_eq!(fortnight.since, last + 14 * DAY_MS);
        assert_eq!(fortnight.expiring.unwrap().at, last + 30 * DAY_MS);
    }

    #[test]
    fn a_failed_test_may_be_added_anyway_only_when_the_owner_needs_do_nothing_first() {
        let unreachable = TestFailure::of(TransportError::Unreachable("asleep".into())).unwrap();
        assert!(unreachable.may_add_anyway());
        let key = TestFailure::of(TransportError::HostKey("changed".into())).unwrap();
        assert!(!key.may_add_anyway());
        let missing = TestFailure::of(TransportError::MissingTools(vec!["dd".into()])).unwrap();
        assert_eq!(missing, TestFailure::Missing(vec!["dd".to_string()]));
        assert!(!missing.may_add_anyway());
        let no_ssh = TestFailure::of(TransportError::Launch {
            program: "ssh".to_string(),
            source: std::io::ErrorKind::NotFound.into(),
        })
        .unwrap();
        assert_eq!(no_ssh, TestFailure::NotInstalled("ssh".to_string()));
        assert_eq!(TestFailure::of(TransportError::Cancelled), None);
    }
}
