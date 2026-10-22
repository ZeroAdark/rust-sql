//! L1 (main 09-24): `answered_whole`, the width rule's host check — a site runs on the leader only when a walk of whole
//! parts skips or answers every part from the catalog's part records. One Mixed part, a record the catalog does not hold,
//! an inexact answer lane or a NULL under an AllMatch keeps the site wide (the stake is its width, never its rows).

use std::sync::Arc;

use gravity_hosts::ops::scan::pgrc::granule_source::{answered_parts, answered_whole, GranuleRecipe, PartIoSpec, PartList, PartSpec};
use gravity_program::{ZoneOp, ZonePred};
use pgrc_am::answers::Answers;
use pgrc_format::answers::encode_column;
use pgrc_format::meta::{KeyKind, StatsRecord, STATSF_COMPUTED};
use pgrc_read::{PartExpect, PartRegistry};

const ROWS: u32 = 1000;

/// A part-grain record: its keys, non-null count and key kind.
fn rec(min: i64, max: i64, nonnull: u32, kind: KeyKind) -> Vec<u8> {
    let mut r = StatsRecord::absent();
    (r.min_key, r.max_key, r.nonnull, r.key_kind, r.flags) = (min, max, nonnull, kind.as_u8(), STATSF_COMPUTED);
    let mut out = Vec::new();
    r.encode_into(&mut out);
    out
}

/// A list of one part per `(filter record, answer record)` (`ino` apart per case: the verdict is kept per generation);
/// the catalog holds attno 1's (the filter lane) and attno 2's (the answer lane), an empty record as none.
fn list(ino: u64, recs: &[(Vec<u8>, Vec<u8>)]) -> PartList {
    let n = recs.len();
    let parts = (0..n).map(|i| PartSpec { io: PartIoSpec::Mem { bytes: Arc::from(&[][..]), dev: 7, ino: ino + i as u64 }, expect: PartExpect::none(), rows: ROWS as u64, granules: 1, deletes: None, part: None }).collect();
    let col = |f: fn(&(Vec<u8>, Vec<u8>)) -> &[u8]| encode_column(&recs.iter().map(f).collect::<Vec<_>>(), &vec![None; n]).unwrap();
    let answers = Answers::new(n, vec![(1, col(|r| &r.0[..])), (2, col(|r| &r.1[..]))]).expect("answers");
    PartList::with_answers(parts, Some(Arc::new(answers)))
}

#[test]
fn a_site_is_answered_whole_only_when_every_part_is_skipped_or_answered_exactly() {
    let recipe = GranuleRecipe::for_test(424_242, &[1, 2], Arc::new(PartRegistry::new(1 << 20)));
    let lt = [ZonePred::word(0, ZoneOp::Lt, 100)];
    let exact = || rec(1, 9, ROWS, KeyKind::Exact);
    let all = || (rec(0, 50, ROWS, KeyKind::Exact), exact());
    let none = || (rec(200, 300, ROWS, KeyKind::Exact), exact());
    let ask = |ino: u64, recs: &[(Vec<u8>, Vec<u8>)], preds: &[ZonePred]| answered_whole(&list(ino, recs), &recipe, preds, &[1]);
    // every part AllMatch (answered) or NoneMatch (skipped), the answer lane exact
    assert!(ask(100, &[all(), none(), all()], &lt));
    // one Mixed part is read granule by granule: wide
    assert!(!ask(200, &[all(), (rec(50, 150, ROWS, KeyKind::Exact), exact()), none()], &lt));
    // AllMatch needs every row non-null: a NULL makes the part Mixed
    assert!(!ask(300, &[all(), (rec(0, 50, ROWS - 1, KeyKind::Exact), exact())], &lt));
    // a part whose filter record the catalog does not hold is a read: wide
    assert!(!ask(400, &[all(), (Vec::new(), exact())], &lt));
    // an AllMatch part whose answer is inexact (or not held) decodes: wide; a skipped part needs no answer
    assert!(!ask(500, &[all(), (rec(0, 50, ROWS, KeyKind::Exact), rec(1, 9, ROWS, KeyKind::Coarse))], &lt));
    assert!(!ask(600, &[all(), (rec(0, 50, ROWS, KeyKind::Exact), Vec::new())], &lt));
    assert!(ask(700, &[all(), (rec(200, 300, ROWS, KeyKind::Exact), Vec::new())], &lt));
    // no static zone predicate: no AllMatch verdict, nothing answered
    assert!(!ask(800, &[all()], &[]));
    // the verdict is kept per generation: the same list asked again answers the same
    let l = list(900, &[all(), (rec(50, 150, ROWS, KeyKind::Exact), exact())]);
    assert!(!answered_whole(&l, &recipe, &lt, &[1]) && !answered_whole(&l, &recipe, &lt, &[1]));
    let l = list(1000, &[all(), none()]);
    assert!(answered_whole(&l, &recipe, &lt, &[1]) && answered_whole(&l, &recipe, &lt, &[1]));
}

/// The runner's whole-part claims for an answerable site: only when some part is skipped or answered from the records with
/// no read (`answered_parts` > 0) or the records cannot say (`None`); every part Mixed counts none.
#[test]
fn answered_parts_counts_the_parts_the_records_skip_or_answer() {
    let recipe = GranuleRecipe::for_test(434_343, &[1, 2], Arc::new(PartRegistry::new(1 << 20)));
    let lt = [ZonePred::word(0, ZoneOp::Lt, 100)];
    let exact = || rec(1, 9, ROWS, KeyKind::Exact);
    let all = || (rec(0, 50, ROWS, KeyKind::Exact), exact());
    let none = || (rec(200, 300, ROWS, KeyKind::Exact), exact());
    let mixed = || (rec(50, 150, ROWS, KeyKind::Exact), exact());
    let ask = |ino: u64, recs: &[(Vec<u8>, Vec<u8>)], preds: &[ZonePred]| answered_parts(&list(ino, recs), &recipe, preds, &[1]);
    assert_eq!(ask(2100, &[mixed(), mixed(), mixed()], &lt), Some(0), "every part Mixed: none");
    assert_eq!(ask(2200, &[all(), mixed(), none()], &lt), Some(2), "one answered, one skipped");
    assert_eq!(ask(2300, &[mixed(), none()], &lt), Some(1), "a skipped part needs no answer");
    assert_eq!(ask(2400, &[mixed(), (rec(0, 50, ROWS, KeyKind::Exact), rec(1, 9, ROWS, KeyKind::Coarse))], &lt), Some(0), "an inexact answer decodes");
    assert_eq!(ask(2500, &[mixed(), (rec(0, 50, ROWS - 1, KeyKind::Exact), exact())], &lt), Some(0), "a NULL makes the part Mixed");
    assert_eq!(ask(2600, &[all(), (Vec::new(), exact())], &lt), None, "a filter record not held: the part's own Stats body decides");
    assert_eq!(ask(2700, &[all()], &[]), Some(0), "no static zone predicate");
    // agrees with answered_whole, and is kept per generation: asked twice
    for (ino, recs, whole) in [(2800, vec![all(), none()], true), (2900, vec![all(), mixed()], false)] {
        let l = list(ino, &recs);
        let n = answered_parts(&l, &recipe, &lt, &[1]);
        assert_eq!(n, answered_parts(&l, &recipe, &lt, &[1]));
        assert_eq!(n == Some(recs.len() as u64), whole);
        assert_eq!(answered_whole(&l, &recipe, &lt, &[1]), whole);
    }
}

/// SRC-21: a tallied predicate over an answer lane answers a part it leaves Mixed (its passing rows are the answer lane's
/// tally), as `stage_part` does; NoneMatch still skips. Untallied, or over a lane that is not an answer lane, or beside
/// another Mixed predicate, the part is read.
#[test]
fn a_tallied_predicate_answers_a_part_it_leaves_mixed() {
    let recipe = GranuleRecipe::for_test(454_545, &[1, 2], Arc::new(PartRegistry::new(1 << 20)));
    let mut ne = ZonePred::word(1, ZoneOp::Ne, 0);
    let plain = [ZonePred::word(0, ZoneOp::Lt, 100), ne];
    ne.tallied = true;
    let tallied = [ZonePred::word(0, ZoneOp::Lt, 100), ne];
    // `x` Mixed under `x <> 0`: NULLs, and 0 inside its range
    let mixed_x = || (rec(0, 50, ROWS, KeyKind::Exact), rec(-5, 9, ROWS - 3, KeyKind::Exact));
    // `x` all zero: `x <> 0` matches no row
    let zeros = || (rec(0, 50, ROWS, KeyKind::Exact), rec(0, 0, ROWS, KeyKind::Exact));
    let lt_mixed = || (rec(50, 150, ROWS, KeyKind::Exact), rec(-5, 9, ROWS, KeyKind::Exact));
    let ask = |ino: u64, recs: &[(Vec<u8>, Vec<u8>)], preds: &[ZonePred], answer: &[usize]| answered_parts(&list(ino, recs), &recipe, preds, answer);
    assert_eq!(ask(3100, &[mixed_x(), mixed_x()], &plain, &[1]), Some(0), "untallied: the Mixed parts are read");
    assert_eq!(ask(3200, &[mixed_x(), mixed_x()], &tallied, &[1]), Some(2), "tallied over the answer lane: answered");
    assert_eq!(ask(3300, &[mixed_x(), zeros()], &tallied, &[1]), Some(2), "an all-zero part is skipped");
    assert_eq!(ask(3400, &[lt_mixed(), mixed_x()], &tallied, &[1]), Some(1), "another predicate Mixed: read");
    assert_eq!(ask(3500, &[mixed_x()], &tallied, &[]), Some(0), "x not an answer lane: read");
    assert!(answered_whole(&list(3600, &[mixed_x(), zeros()]), &recipe, &tallied, &[1]));
    assert!(!answered_whole(&list(3700, &[mixed_x(), zeros()]), &recipe, &plain, &[1]));
}
