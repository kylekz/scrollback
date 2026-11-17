//! Where the copies kept on this machine ran (PLAN §7.7): the hosts their app's records name,
//! resolved by this machine's `ssh` and compared with the SSH locations' hosts.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use toolbox_process::CancelToken;

use super::SyncError;
use crate::archive::Archive;
use crate::location::{LocationId, LocationKind};
use crate::transport::ResolveHost;
use crate::transport::ssh_config::Resolution;

/// How many hosts are resolved at once: a slow `Match exec` in the owner's config holds up
/// one of these, not every host after it.
const AT_ONCE: usize = 4;

/**
 * Says where each copy shown alone at This PC's location ran, where its app's record names a
 * host (PLAN §7.7). Each such host is resolved through `resolve` (this machine's `ssh -G`, or a
 * stand-in), and only when one resolves are the SSH locations' hosts resolved too, to compare
 * with: what each ends at (host name, user, port and the way there) must be the same, and of
 * two locations alike the one added first is taken.
 *
 * A host `ssh` could not be asked about (missing, failing, slow) keeps what was said of it
 * last, as does one that resolved while a location's could not be asked; one it answered
 * nothing for is no machine. A location whose host is the very same string (`same_place`) is
 * the copy's whatever `ssh` says. Hidden copies are not looked at: nothing shows where they
 * ran. Run after every sync of any location and after a start's index, its own job; `cancel`
 * stops it before it writes. Returns how many records' attribution changed.
 */
pub fn attribute(
    archive: &Archive,
    resolve: &dyn ResolveHost,
    cancel: &CancelToken,
) -> Result<usize, SyncError> {
    let catalogue = archive.catalogue();
    let hosts = catalogue.copy_hosts()?;
    let recorded = resolve_all(resolve, &hosts.recorded);
    let locations = if recorded.iter().any(|it| matches!(it, Ok(Some(_)))) {
        let of: Vec<String> = hosts
            .locations
            .iter()
            .map(|(_, host)| host.clone())
            .collect();
        resolve_all(resolve, &of)
    } else {
        Vec::new()
    };
    let unasked = locations.iter().any(Result::is_err);
    let mut places: HashMap<String, Option<Option<LocationId>>> = HashMap::new();
    for (host, resolution) in hosts.recorded.iter().zip(recorded) {
        let named = LocationKind::Ssh { host: host.clone() };
        let same = (hosts.locations.iter())
            .find(|(_, at)| LocationKind::Ssh { host: at.clone() }.same_place(&named))
            .map(|(id, _)| id.clone());
        /* `None` keeps what was said last; else, as `attribute_copies` takes it: resolved or
        not, and the location it is. */
        let place = match resolution {
            Ok(Some(resolved)) => {
                let found = (hosts.locations.iter().zip(&locations))
                    .find(|(_, at)| matches!(at, Ok(Some(at)) if *at == resolved))
                    .map(|((id, _), _)| id.clone());
                match (found.or(same), unasked) {
                    (Some(id), _) => Some(Some(Some(id))),
                    (None, true) => None,
                    (None, false) => Some(Some(None)),
                }
            }
            Ok(None) => Some(same.map(Some)),
            Err(_) => same.map(|same| Some(Some(same))),
        };
        if let Some(place) = place {
            places.insert(host.clone(), place);
        }
    }
    if cancel.is_cancelled() {
        return Err(SyncError::Cancelled);
    }
    Ok(catalogue.attribute_copies(&places)?)
}

/// What `resolve` makes of each of `hosts`, in order, [`AT_ONCE`] at a time.
fn resolve_all(resolve: &dyn ResolveHost, hosts: &[String]) -> Vec<Resolution> {
    let next = AtomicUsize::new(0);
    let answers: Mutex<Vec<Option<Resolution>>> = Mutex::new(vec![None; hosts.len()]);
    std::thread::scope(|scope| {
        for _ in 0..AT_ONCE.min(hosts.len()) {
            scope.spawn(|| {
                loop {
                    let at = next.fetch_add(1, Ordering::Relaxed);
                    let Some(host) = hosts.get(at) else {
                        break;
                    };
                    let answer = resolve.resolve(host);
                    answers.lock().unwrap_or_else(PoisonError::into_inner)[at] = Some(answer);
                }
            });
        }
    });
    answers
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner)
        .into_iter()
        .map(|answer| answer.unwrap_or_else(|| Err("not asked".to_string())))
        .collect()
}
