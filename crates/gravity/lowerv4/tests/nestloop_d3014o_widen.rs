//! D3014O nest.parallel (ops/nestloop/widen.rs): an embedded nest inner's plain btree probe whose outer reaches an
//! order-free end is marked `SiteKind::IndexSource { worker: true }` (a worker probes it through the frozen index image,
//! hosts index_probe.rs); a plain index scan, which no nest re-opens, is never marked.
use gravity_lowerv4_it as common;

use gravity_program::SiteKind;

fn text(rel: &str) -> String {
    let p = common::plans_dir().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// The program's index sites, each with its worker bit.
fn index_sites(rel: &str) -> Vec<(u32, bool)> {
    let l = common::lower_fixture(&text(rel)).expect("decomposes").unwrap_or_else(|r| panic!("{rel} refused: {r:?}"));
    l.descs.sites.iter().filter_map(|s| match s.kind {
        SiteKind::IndexSource { worker, .. } => Some((s.index, worker)),
        _ => None,
    }).collect()
}

/// tpch-003: the customer x orders rows probe lineitem by `l_orderkey = o_orderkey` (a nest whose inner is a bare btree
/// probe), the rows reaching a hash aggregate (the sort and LIMIT above it read its groups): the probe's site is marked.
#[test]
fn a_nest_inner_btree_probe_under_an_order_free_end_is_marked() {
    let sites = index_sites("tpch-003.plan");
    assert!(!sites.is_empty(), "tpch-003 has an index site");
    assert!(sites.iter().all(|s| s.1), "tpch-003's nest inner probe is a worker probe: {sites:?}");
}

/// bench-018: a range over the primary key read by an index scan that drives its pipeline (no nest): never marked.
#[test]
fn a_plain_index_scan_is_not_marked() {
    let sites = index_sites("bench-018.plan");
    assert!(!sites.is_empty(), "bench-018 has an index site");
    assert!(sites.iter().all(|s| !s.1), "bench-018's index scan is not a worker probe: {sites:?}");
}

/// bench-130: the inner probe sits under a Memoize of one int4 key (D3014 M2: each worker keeps its own memo table, exec
/// ops/memoize/proto.rs): marked, and the memo's Cache proto carries its key's hash equality operator (int4eq's `=`, 96)
/// and collation (none) for the leader to resolve the key functions a worker calls.
#[test]
fn a_memoized_inner_is_marked_with_its_key_for_the_leader() {
    let sites = index_sites("bench-130.plan");
    assert!(!sites.is_empty() && sites.iter().all(|s| s.1), "bench-130's memoized probe is a worker probe: {sites:?}");
    let l = common::lower_fixture(&text("bench-130.plan")).expect("decomposes").unwrap_or_else(|r| panic!("bench-130 refused: {r:?}"));
    let memo: Vec<u64> = l.descs.v4.protos.iter().filter(|p| p.kind == gravity_program::protos::ProtoKind::Cache).map(|p| p.arg).collect();
    assert_eq!(memo, vec![96], "one memo, its one key's operator in the proto's arg (collation 0 in the high half)");
}

/// tpch-008 (D3014 phase 3): the filtered orders rows probe one hash table, probe lineitem by a nest step, then probe a
/// second hash table outer-first (PostgreSQL reads the outer's first row before it builds that table) on their way to a
/// sort. The probe and the nest ahead of the second probe raise on no row (effects/visible.rs `joins_quiet`), so no row
/// order decides an error at any width: every nest inner probe of the pipeline is marked, the memoized ones too.
#[test]
fn a_nest_ahead_of_an_outer_first_probe_is_marked() {
    let sites = index_sites("tpch-008.plan");
    assert!(sites.len() >= 3, "tpch-008 has its three nest inner index sites: {sites:?}");
    assert!(sites.iter().all(|s| s.1), "tpch-008's nest inner probes are worker probes: {sites:?}");
}
