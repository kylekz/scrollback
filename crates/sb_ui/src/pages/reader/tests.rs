//! The row diff ([`diff`]): what the list is told when the rows change.

use std::sync::Arc;

use pretty_assertions::assert_eq;
use sb_core::transcript::BlockId;

use super::diff;
use super::rows::{BodyPart, Row, RowKind, Space};
use crate::services::reader::BlockRef;

fn at(block: usize) -> BlockRef {
    BlockRef {
        conversation: Arc::from("main"),
        block: BlockId(block),
    }
}

fn row(kind: RowKind) -> Row {
    Row {
        kind,
        depth: 0,
        opens: false,
        closes: 0,
        space: Space::None,
    }
}

fn call(block: usize, open: bool) -> Row {
    row(RowKind::Call {
        at: at(block),
        open,
    })
}

fn body(block: usize, part: BodyPart) -> Row {
    row(RowKind::Body {
        at: at(block),
        part,
        first: false,
        last: false,
    })
}

/**
 * The old rows with `splices` made, the inserted ones taken from `new` (as the list measures
 * them anew): what the list holds once told. Every row kept must be the new row of its key.
 */
fn apply(old: &[Row], new: &[Row], splices: &[(usize, usize, usize)]) -> Vec<Option<RowKind>> {
    let mut list: Vec<Option<RowKind>> = old.iter().map(|row| Some(row.key())).collect();
    for &(at, removed, inserted) in splices {
        list.splice(at..at + removed, std::iter::repeat_n(None, inserted));
    }
    list.iter()
        .zip(new)
        .map(|(kept, row)| kept.clone().or_else(|| Some(row.key())))
        .collect()
}

fn check(old: &[Row], new: &[Row]) -> (Vec<(usize, usize, usize)>, Vec<usize>) {
    let (splices, changed) = diff(old, new);
    let applied = apply(old, new, &splices);
    assert_eq!(applied.len(), new.len(), "{splices:?}");
    let keys: Vec<Option<RowKind>> = new.iter().map(|row| Some(row.key())).collect();
    assert_eq!(applied, keys);
    (splices, changed)
}

#[test]
fn a_call_opened_and_closed_is_measured_again_in_place_and_its_body_spliced() {
    let closed = vec![call(1, false), call(2, false), call(3, false)];
    let open = vec![
        call(1, false),
        call(2, true),
        body(2, BodyPart::Loading),
        call(3, false),
    ];
    /* Opening: the call changed in place, its body inserted after it. */
    assert_eq!(check(&closed, &open), (vec![(2, 0, 1)], vec![1]));
    /* Closing: the other way. */
    assert_eq!(check(&open, &closed), (vec![(2, 1, 0)], vec![1]));
}

#[test]
fn a_body_that_loads_replaces_its_skeleton_and_keeps_what_is_around_it() {
    let loading = vec![call(1, true), body(1, BodyPart::Loading), call(2, false)];
    let loaded = vec![
        call(1, true),
        body(1, BodyPart::InputHead),
        body(
            1,
            BodyPart::Lines {
                part: None,
                chunk: 0,
            },
        ),
        body(1, BodyPart::Footer),
        call(2, false),
    ];
    assert_eq!(check(&loading, &loaded), (vec![(1, 1, 3)], vec![]));
    assert_eq!(check(&loaded, &loading), (vec![(1, 3, 1)], vec![]));
}

#[test]
fn nothing_changed_is_nothing_to_do_and_rows_moved_are_spliced_out_and_in() {
    let rows = vec![call(1, false), call(2, false)];
    assert_eq!(check(&rows, &rows), (vec![], vec![]));
    let swapped = vec![call(2, false), call(1, false)];
    check(&rows, &swapped);
    check(&[], &rows);
    check(&rows, &[]);
}
