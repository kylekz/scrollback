//! Which subagent started which: the one rule the index ([`super::Transcript::read_subagents`])
//! and the reader both place subagents by, so that what the index counts as the n-th subagent no
//! call started ([`super::At::Unattached`]) is the reader's n-th too.
//!
//! **The rule.** The session's own calls claim first, in order, then each subagent no call of
//! the session's own started (a root), in order. A subagent claims, in the order of its calls,
//! the subagents those started, and each claim is followed through what the claimed one claims
//! before the next is looked at. The first claim of a subagent places it; a later one is passed
//! over. A subagent's claim on itself, or on one it is under, is passed over too, so no subagent
//! is under itself. A later root's claim places an earlier root, which then is no root.
//!
//! It is worked out without recursion, each subagent's claims asked for once, so a session
//! whose subagents nest thousands deep or start hundreds each costs what they number.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Where each subagent is placed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Placed {
    /// Each subagent another subagent's claim placed, and that other subagent. One the
    /// session's own calls started is not here.
    pub parents: HashMap<Arc<str>, Arc<str>>,
    /// The roots no claim placed, in their order: those no call is known to have started.
    pub unattached: Vec<Arc<str>>,
}

/**
 * Places the subagents by the rule (the module docs): `first` are those the session's own
 * calls started, in order; `roots` those none of its calls started, in order; `started` says
 * which subagents a subagent's calls started, in order. It is asked once of each subagent
 * reached.
 */
pub fn place(
    first: &[Arc<str>],
    roots: &[Arc<str>],
    mut started: impl FnMut(&Arc<str>) -> Vec<Arc<str>>,
) -> Placed {
    let mut walk = Walk::default();
    for item in first {
        if walk.placed.insert(item.clone()) && walk.visited.insert(item.clone()) {
            walk.follow(item.clone(), &mut started);
        }
    }
    for root in roots {
        if !walk.placed.contains(root) && walk.visited.insert(root.clone()) {
            walk.follow(root.clone(), &mut started);
        }
    }
    Placed {
        unattached: (roots.iter())
            .filter(|root| !walk.placed.contains(*root))
            .cloned()
            .collect(),
        parents: walk.parents,
    }
}

#[derive(Default)]
struct Walk {
    parents: HashMap<Arc<str>, Arc<str>>,
    /// Those a claim placed, and those the session's own calls started.
    placed: HashSet<Arc<str>>,
    /// Those whose claims were followed, or are being.
    visited: HashSet<Arc<str>>,
    /// Those whose claims are being followed: the subagent claiming and those it is under.
    chain: HashSet<Arc<str>>,
}

impl Walk {
    /// Follows the claims of `item`, and of those it places, depth first.
    fn follow(&mut self, item: Arc<str>, started: &mut impl FnMut(&Arc<str>) -> Vec<Arc<str>>) {
        let claims = started(&item);
        self.chain.insert(item.clone());
        let mut stack = vec![(item, claims.into_iter())];
        while let Some((claiming, claims)) = stack.last_mut() {
            let claiming = claiming.clone();
            let Some(claimed) = claims.next() else {
                self.chain.remove(&claiming);
                stack.pop();
                continue;
            };
            if self.chain.contains(&claimed) || !self.placed.insert(claimed.clone()) {
                continue;
            }
            self.parents.insert(claimed.clone(), claiming);
            if self.visited.insert(claimed.clone()) {
                let claims = started(&claimed);
                self.chain.insert(claimed.clone());
                stack.push((claimed, claims.into_iter()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn items(names: &[&str]) -> Vec<Arc<str>> {
        names.iter().map(|it| Arc::from(*it)).collect()
    }

    fn placed(first: &[&str], roots: &[&str], claims: &[(&str, &[&str])]) -> Placed {
        let claims: HashMap<&str, Vec<Arc<str>>> = claims
            .iter()
            .map(|(it, claims)| (*it, items(claims)))
            .collect();
        place(&items(first), &items(roots), |item| {
            claims.get(&**item).cloned().unwrap_or_default()
        })
    }

    fn parent(placed: &Placed, item: &str) -> Option<String> {
        placed.parents.get(item).map(|it| it.to_string())
    }

    #[test]
    fn a_later_roots_claim_places_an_earlier_root() {
        let placed = placed(&[], &["A", "B"], &[("B", &["A"])]);
        assert_eq!(placed.unattached, items(&["B"]));
        assert_eq!(parent(&placed, "A").as_deref(), Some("B"));
    }

    #[test]
    fn a_claim_on_itself_or_what_it_is_under_is_passed_over() {
        let placed = placed(&[], &["A", "B"], &[("A", &["A", "B"]), ("B", &["A"])]);
        assert_eq!(placed.unattached, items(&["A"]));
        assert_eq!(parent(&placed, "B").as_deref(), Some("A"));
        assert_eq!(parent(&placed, "A"), None);
    }

    #[test]
    fn the_first_claim_places_and_the_sessions_own_calls_claim_first() {
        let placed = placed(
            &["S"],
            &["C", "D"],
            &[("S", &["C"]), ("C", &["D"]), ("D", &["C"])],
        );
        assert_eq!(placed.unattached, Vec::<Arc<str>>::new());
        assert_eq!(parent(&placed, "C").as_deref(), Some("S"));
        assert_eq!(parent(&placed, "D").as_deref(), Some("C"));
        assert_eq!(parent(&placed, "S"), None);
    }

    #[test]
    fn thousands_deep_and_hundreds_wide_are_placed_without_recursion() {
        let deep: Vec<String> = (0..20_000).map(|at| format!("d{at}")).collect();
        let wide: Vec<String> = (0..500).map(|at| format!("w{at}")).collect();
        let mut asked = 0;
        let placed = place(&items(&["d0"]), &items(&["root"]), |item| {
            asked += 1;
            match &**item {
                "root" => wide.iter().map(|it| Arc::from(it.as_str())).collect(),
                item => match item
                    .strip_prefix('d')
                    .and_then(|at| at.parse::<usize>().ok())
                {
                    Some(at) => deep
                        .get(at + 1)
                        .map(|it| Arc::from(it.as_str()))
                        .into_iter()
                        .collect(),
                    None => Vec::new(),
                },
            }
        });
        assert_eq!(asked, 20_000 + 1 + 500, "each asked once");
        assert_eq!(parent(&placed, "d19999").as_deref(), Some("d19998"));
        assert_eq!(parent(&placed, "w499").as_deref(), Some("root"));
        assert_eq!(placed.unattached, items(&["root"]));
    }
}
