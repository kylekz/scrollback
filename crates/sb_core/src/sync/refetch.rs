//! Fetch again from its location (PLAN §4.4, DESIGN §4.13): one item whose newest version
//! verify found damaged is fetched whole, at once, as a new version that started because the
//! one before was damaged. The damaged version stays, marked. It is what the next sync of the
//! location would do for that item, done for that item alone: scanned, planned, fetched,
//! proven and stored as a pass does, then its session indexed. Nothing else at the location is
//! looked at or recorded, so the location's last sync is as it was.

use std::slice;

use toolbox_process::CancelToken;

use super::{PassReport, SyncError, index, plan, store};
use crate::archive::Archive;
use crate::location::LocationId;
use crate::provider::Provider;
use crate::transport::Transport;

/**
 * Fetches the item `key` of `provider` at `location`, reached through `transport`, again as
 * its next version, when its newest version is damaged ([`SyncError::NotDamaged`] otherwise)
 * and the location still has it ([`SyncError::NotAtLocation`] otherwise). Returns the new
 * version's number. Refused while the location is being synced ([`SyncError::Busy`]): its
 * pass fetches the item again itself.
 */
pub fn refetch(
    archive: &Archive,
    location: &LocationId,
    provider: &str,
    key: &str,
    transport: &dyn Transport,
    providers: &[Box<dyn Provider + '_>],
    cancel: &CancelToken,
) -> Result<u32, SyncError> {
    let _syncing = archive
        .begin_sync(location)
        .ok_or_else(|| SyncError::Busy(location.clone()))?;
    let catalogue = archive.catalogue();
    let not_damaged = || SyncError::NotDamaged(key.to_string());
    let archived = catalogue
        .archived(location, provider)?
        .into_iter()
        .find(|archived| archived.item.key == key)
        .filter(|archived| archived.current.damaged)
        .ok_or_else(not_damaged)?;
    let number = archived.current.number + 1;
    let not_there = || SyncError::NotAtLocation(key.to_string());
    let agent = providers
        .iter()
        .find(|it| it.id().as_str() == provider)
        .ok_or_else(not_there)?;
    let scratch = archive.tmp();
    let mut source = agent.detect(transport, &scratch)?.ok_or_else(not_there)?;
    let scanned = source
        .scan()?
        .items
        .into_iter()
        .find(|item| item.key == key)
        .ok_or_else(not_there)?;
    let plan = plan::plan(slice::from_ref(&scanned), false, vec![archived]);
    let mut report = PassReport::default();
    store::Storing {
        archive,
        location,
        provider,
        cancel,
        progress: &mut |_| {},
        report: &mut report,
        done: 0,
    }
    .all(plan.work, source.as_mut())?;
    if let Some(left) = report.failed.first().or(report.deferred.first()) {
        return Err(SyncError::NotFetched(left.reason.clone()));
    }
    index::changed(archive, location, agent.as_ref(), cancel, &mut |_| {})?;
    Ok(number)
}
