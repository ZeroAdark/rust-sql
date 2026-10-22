//! M3-WALK (cards/M3.md L7): road topk.walk's group walk on a top-k over a grouped int8 key (`Limit -> HashAgg[ZoneTopK]`,
//! fieldobs-034). The pipeline that builds the group declares the walked run (`TopKDesc::run(n).walking(attno)`) and the
//! walk protocol over its site, the key's attribute and the group's slot (a group walk; width.walk's bit when that road
//! is on). G4: the shapes the walk leaves whole declare neither: a nullable key, a LIMIT with a parameter, the road off.
//! D508, the row walk (`Limit -> Sort[ZoneWalk]`, a timestamp ORDER BY LIMIT, fieldobs-039): the top's pipeline declares the
//! same runner walk on observed_at (attno 3) and the protocol without the group bit; DESC walks the key maxima downward
//! (PM13). V4R-186, `SELECT *` under a LIKE ordered by observed_at (fieldobs-038) under topk.fetch: the walked scan's site
//! carries source_url (the filter) and observed_at (the key) alone; the road off, all 37.
use gravity_lowerv4_it as common;

use gravity_lowerv4::face::strategy::{self, StrategySet};
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::{End, LaneId, TopKDesc};

const WALK_GROUP: u64 = 1 << 16;
const WALK_WIDTH: u64 = 1 << 17;
const WALK_RUNNER: u64 = 1 << 18;

fn plan(name: &str) -> String {
    std::fs::read_to_string(common::plans_dir().join(format!("{name}.plan"))).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn group_top() -> String {
    plan("fieldobs-034")
}

/// The walk protocols and each pipeline's declared top of fixture `text`, lowered under `off`.
fn lowered(text: &str, off: &str) -> (Vec<Proto>, Vec<(Option<TopKDesc>, Option<u8>)>) {
    strategy::set_off(StrategySet::parse(off).unwrap_or_else(|e| panic!("{off}: {e}")));
    let l = match common::lower_fixture(text) {
        Some(Ok(l)) => l,
        Some(Err(e)) => panic!("lowers: {e:?}"),
        None => panic!("decompose refused"),
    };
    strategy::set_off(StrategySet::EMPTY);
    let walks = l.descs.v4.protos.iter().filter(|p| p.kind == ProtoKind::Walk).copied().collect();
    let pipes = l.descs.pipelines.iter().map(|p| (p.topk, match p.ends_in { End::Breaker(b) => Some(b.slot), _ => None })).collect();
    (walks, pipes)
}

#[test]
fn group_top_k_walks_its_int8_key() {
    let (walks, pipes) = lowered(&group_top(), "");
    let [w] = walks[..] else { panic!("one walk protocol: {walks:?}") };
    let (topk, slot) = pipes[w.pipeline as usize];
    assert_eq!(topk, Some(TopKDesc::run(10).walking(8)), "the group's pipeline declares n = 10 walked on observer_id (the runner orders the claims, D504)");
    assert_eq!((w.arg & 0xffff, w.arg & WALK_GROUP, w.arg & WALK_RUNNER, w.arg & WALK_WIDTH), (8, WALK_GROUP, WALK_RUNNER, WALK_WIDTH), "observer_id (attno 8), a group walk on the runner's claims, width.walk on");
    assert_eq!(Some(w.slot), slot, "the walk names the group's slot");
    assert!(w.site >= 0, "the walk names the pipeline's site");
    assert_eq!(pipes.iter().filter(|p| p.0.is_some()).count(), 1, "only the group's pipeline declares a top");
    let (walks, _) = lowered(&group_top(), "width.walk");
    assert_eq!(walks.iter().map(|w| w.arg & WALK_WIDTH).collect::<Vec<_>>(), [0], "width.walk off: the walk without its bit");
}

#[test]
fn g4_the_shapes_the_walk_leaves_whole() {
    let nullable = group_top().replace("observer_id typid=20 typmod=-1 len=8 byval=t notnull=t", "observer_id typid=20 typmod=-1 len=8 byval=t notnull=f");
    assert_ne!(nullable, group_top());
    for (why, text, off) in [("a nullable key", nullable, ""), ("the road off", group_top(), "topk.walk")] {
        let (walks, pipes) = lowered(&text, off);
        assert!(walks.is_empty() && pipes.iter().all(|p| p.0.is_none()), "{why}: no walk, no top ({walks:?})");
    }
}

#[test]
fn row_top_k_walks_its_timestamp_key() {
    let (walks, pipes) = lowered(&plan("fieldobs-039"), "");
    let [w] = walks[..] else { panic!("one walk protocol: {walks:?}") };
    assert_eq!(pipes[w.pipeline as usize].0, Some(TopKDesc::run(10).walking(3)), "the top's pipeline: n = 10 walked on observed_at");
    assert_eq!((w.arg & 0xffff, w.arg & WALK_GROUP, w.arg & WALK_RUNNER), (3, 0, WALK_RUNNER), "a row walk on the runner's claims");
    assert!(w.site >= 0);
    let (walks, pipes) = lowered(&plan("fieldobs-039"), "topk.walk");
    assert!(walks.is_empty() && pipes.iter().all(|p| p.0.is_none()), "the road off: the top runs whole");
}

/// PM13: the row top as `ORDER BY observed_at DESC` (NOT NULL, so NULLS FIRST walks) declares the same row walk, descending; a
/// nullable key's DESC NULLS FIRST walks too, its granules holding a NULL first (PL-06, `TopKDesc::nulls_first`), and so
/// does a nullable ASC NULLS FIRST.
#[test]
fn row_top_k_desc_walks_down_the_maxima() {
    let asc = plan("fieldobs-039");
    let desc = asc.replace(":sortOperators ( 2062) :collations ( 0) :nullsFirst ( false) :gravity_rewrite ZoneWalk { attno: 3, desc: false, nulls_first: false }", ":sortOperators ( 2064) :collations ( 0) :nullsFirst ( true) :gravity_rewrite ZoneWalk { attno: 3, desc: true, nulls_first: true }");
    assert_ne!(desc, asc, "the fixture's ascending sort");
    let (walks, pipes) = lowered(&desc, "");
    let [w] = walks[..] else { panic!("one walk protocol: {walks:?}") };
    assert_eq!(pipes[w.pipeline as usize].0, Some(TopKDesc::run(10).walking(3).descending()), "n = 10 walked down observed_at");
    assert_eq!((w.arg & 0xffff, w.arg & WALK_GROUP, w.arg & WALK_RUNNER), (3, 0, WALK_RUNNER));
    let nullable = desc.replace("observed_at typid=1114 typmod=-1 len=8 byval=t notnull=t", "observed_at typid=1114 typmod=-1 len=8 byval=t notnull=f");
    assert_ne!(nullable, desc);
    let (walks, pipes) = lowered(&nullable, "");
    let [w] = walks[..] else { panic!("a nullable DESC NULLS FIRST walks: {walks:?}") };
    assert_eq!(pipes[w.pipeline as usize].0, Some(TopKDesc::run(10).walking(3).descending().nulls_first()), "the granules holding a NULL first");
    let asc_nf = nullable.replace(":sortOperators ( 2064) :collations ( 0) :nullsFirst ( true) :gravity_rewrite ZoneWalk { attno: 3, desc: true, nulls_first: true }", ":sortOperators ( 2062) :collations ( 0) :nullsFirst ( true) :gravity_rewrite ZoneWalk { attno: 3, desc: false, nulls_first: true }");
    assert_ne!(asc_nf, nullable);
    let (walks, pipes) = lowered(&asc_nf, "");
    let [w] = walks[..] else { panic!("a nullable ASC NULLS FIRST walks: {walks:?}") };
    assert_eq!(pipes[w.pipeline as usize].0, Some(TopKDesc::run(10).walking(3).nulls_first()), "ascending, the granules holding a NULL first");
    let nn_asc_nf = asc_nf.replace("observed_at typid=1114 typmod=-1 len=8 byval=t notnull=f", "observed_at typid=1114 typmod=-1 len=8 byval=t notnull=t");
    let (walks, pipes) = lowered(&nn_asc_nf, "");
    let [w] = walks[..] else { panic!("a NOT NULL ASC NULLS FIRST walks: {walks:?}") };
    assert_eq!(pipes[w.pipeline as usize].0, Some(TopKDesc::run(10).walking(3)), "NOT NULL: the resident zone order, no NULL granules");
}

#[test]
fn row_top_k_walks_a_scan_of_its_filter_and_key_lanes() {
    for (off, want) in [("", Some(vec![3, 30])), ("topk.fetch", None)] {
        strategy::set_off(StrategySet::parse(off).unwrap_or_else(|e| panic!("{off}: {e}")));
        let l = match common::lower_fixture(&plan("fieldobs-038")) {
            Some(Ok(l)) => l,
            Some(Err(e)) => panic!("lowers: {e:?}"),
            None => panic!("decompose refused"),
        };
        strategy::set_off(StrategySet::EMPTY);
        let walks: Vec<Proto> = l.descs.v4.protos.iter().filter(|p| p.kind == ProtoKind::Walk).copied().collect();
        let [w] = walks[..] else { panic!("one walk protocol: {walks:?}") };
        let mut attnos: Vec<i16> = l.descs.sites[w.site as usize].lanes().iter().filter_map(|x| if let LaneId::Attr(a) = x.id { Some(a) } else { None }).collect();
        attnos.sort_unstable();
        match want {
            Some(want) => assert_eq!(attnos, want, "topk.fetch: the walked scan reads observed_at (the key) and source_url (the filter) alone"),
            None => assert_eq!(attnos.len(), 37, "the road off: the top keeps every column by value"),
        }
    }
}
