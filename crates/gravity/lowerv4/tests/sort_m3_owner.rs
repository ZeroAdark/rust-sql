//! M3 road owner.count (cards/M3.md L5 `runs/owner_count.rs`, V4R-113): a count-ordered top over a hash group's finals takes
//! exec owner.rs's count form, exactly where props `count_cut` admits it (the predicate `applies::CountTopk` reads); every
//! other count top stays runs.topk_groups (the generic owner, every record gathered and sorted).
use gravity_lowerv4_it as common;

use gravity_abi::breaker::ORDER_NONE;
use gravity_lower::Lowered;
use gravity_lowerv4::face::strategy::{Strategy, StrategySet, OWNER_COUNT, set_off};
use gravity_program::{OwnerDesc, REP_NONE};

fn text(rel: &str) -> String {
    let p = common::plans_dir().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// The fixture lowered with `extra` roads off (code.front_light never applies: V4R-174 took it out of M3).
fn lowered_off(fx: &str, extra: &[Strategy]) -> Result<common::LoweredIn, String> {
    let mut off = StrategySet::EMPTY;
    extra.iter().for_each(|&r| off.add(r));
    set_off(off);
    let l = common::lower_fixture(&text(fx)).expect("decompose takes the fixture").map_err(|e| e.to_string());
    set_off(StrategySet::EMPTY);
    l
}

fn lowered(fx: &str) -> Result<common::LoweredIn, String> {
    lowered_off(fx, &[])
}

/// The owner phases the lowering declares (the group producers').
fn owners(l: &Lowered) -> Vec<OwnerDesc> {
    l.descs.pipelines.iter().filter_map(|p| p.owner).collect()
}

/// The count form: the count slot, the bound n = LIMIT + OFFSET.
fn count_form(fx: &str) -> OwnerDesc {
    let l = lowered(fx).unwrap_or_else(|e| panic!("{fx}: {e}"));
    let o = owners(&l);
    let c: Vec<OwnerDesc> = o.iter().copied().filter(|d| d.order_slot != ORDER_NONE && d.topk > 0).collect();
    assert_eq!(c.len(), 1, "{fx}: one count-ordered owner: {o:?}");
    c[0]
}

/// fieldobs-033 `GROUP BY observer_id, notes ORDER BY count(*) DESC LIMIT 10`: the count form over the record's Count slot 0, n 10;
/// the text key is unnamed by the sort, so a count tie orders it by its fingerprint words (no representative rendered).
/// fieldobs-032 (one int8 key), fieldobs-048 (a URL key), fieldobs-057 (`OFFSET 10000`) and regress's `ORDER BY count(*) DESC LIMIT 5` likewise.
#[test]
fn a_count_ordered_top_takes_the_count_form() {
    for (fx, n) in [("fieldobs-033.plan", 10), ("fieldobs-032.plan", 10), ("fieldobs-048.plan", 10), ("regress-027.plan", 5), ("fieldobs-057.plan", 10010)] {
        let o = count_form(fx);
        assert_eq!((o.order_slot, o.topk, o.pair), (0, n, 0), "{fx}: {o:?}");
        assert!(o.reps.iter().all(|r| r.0 == REP_NONE), "{fx}: no key named after the count: {o:?}");
    }
}

/// The bound is LIMIT + OFFSET: fieldobs-054 (`LIMIT 10 OFFSET 1000`) keeps 1,010 per owner, fieldobs-056 (`OFFSET 100`) 110.
#[test]
fn an_offset_window_keeps_limit_plus_offset() {
    assert_eq!(count_form("fieldobs-054.plan").topk, 1010);
    assert_eq!(count_form("fieldobs-056.plan").topk, 110);
}

/// A nullable text key the sort names after the count over a pgrc dictionary lane (bench-121 `ORDER BY c DESC, dt` with a
/// `WHERE dt <> 'zeta'` filter, bench-155 SVlongc): the count form with the key's representative named, so a count tie
/// renders it (a NULL last, ASC NULLS LAST). The heap twins (bench-145/147/151/152: no code lane to render) keep the
/// generic owner.
#[test]
fn a_named_text_key_names_its_representative() {
    for (fx, n) in [("bench-121.plan", 5), ("bench-155.plan", 10)] {
        let o = count_form(fx);
        assert_eq!(o.topk, n, "{fx}: {o:?}");
        assert!(o.reps[0].0 != REP_NONE, "{fx}: the named key's representative: {o:?}");
    }
}

/// G4: an expression over finals (bench-149), a top by a group key (bench-065) and a named heap text key (bench-145/147/151/152) never declare the count
/// form: their owners are the generic phase.
#[test]
fn a_top_outside_the_shape_keeps_the_generic_owner() {
    for fx in ["bench-149.plan", "bench-065.plan", "bench-145.plan", "bench-147.plan", "bench-151.plan", "bench-152.plan"] {
        let v = lowered(fx);
        if let Ok(l) = &v {
            assert!(owners(l).iter().all(|d| d.topk == 0), "{fx} declared a top-k owner: {:?}", owners(l));
        }
        assert!(v.as_ref().err().is_none_or(|e| !e.contains("owner.count")), "{fx} refused by owner.count: {:?}", v.err());
    }
}

/// The forced-off twin: with owner.count off, fieldobs-033 lowers through runs.topk_groups (the generic owner, every record sorted).
#[test]
fn the_forced_off_twin_sorts_every_record() {
    let l = lowered_off("fieldobs-033.plan", &[OWNER_COUNT]).unwrap_or_else(|e| panic!("fieldobs-033 with owner.count off: {e}"));
    assert!(owners(&l).iter().all(|d| d.topk == 0), "{:?}", owners(&l));
}
