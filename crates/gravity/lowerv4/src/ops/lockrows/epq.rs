//! V4R-316 (L0-h; the V4R-224 design, main 09-26): write.lock's EvalPlanQual in the IR, for a LockRows with a LIMIT /
//! OFFSET above it (write/lockrows.rs). The runner's recheck (exec epq.rs) sends the rechecked row to the client itself,
//! past any step above the lock, so a counting step would not count it; here the newer version goes on through the
//! pipeline's own steps above the lock. V4R-512: the same for a lock whose rows feed another pipeline's product (an Agg,
//! a Sort, a hash build, a CTE's store), where PG's LockRows hands its EPQ row to the parent. The recheck is
//! write/epq_ir.rs's `Epq` (V4R-512 moved it there); this file names the lock's refusals. D3271 / D3272: each lock's recheck
//! forces the pending InitPlans PostgreSQL's EvalPlanQualStart forces (lock_init.rs `forced`) and arms afresh those its
//! fresh estate re-arms (`armed`). Owner M4-LOCK.
use crate::frame::px::Px;
use crate::admit::refusal::ShapeWhy;

use crate::rerun::epq::epq_ir::Epq;

/// A scan reading a system column past ctid and tableoid (the site's cells hold the attributes and the position only;
/// tableoid is the scanned leaf's constant), or a projection below the lock whose form decompose did not record.
const SYS_EPQ: ShapeWhy = ShapeWhy("a LIMIT / OFFSET above LockRows over a system column or an unrecorded projection (L0-h)");
/// V4R-512: the same in a lock whose rows feed another pipeline's product.
const SYS_PRODUCT: ShapeWhy = ShapeWhy("a row lock feeding another pipeline's product over a system column or an unrecorded projection (V4R-512)");
/// lockrows 15b: the same in a lock over a Sort of its own relation's scan under a LIMIT / OFFSET (`breaker_limit`).
const SYS_BREAKER: ShapeWhy = ShapeWhy("a LIMIT / OFFSET above LockRows over a Sort over a system column or an unrecorded projection (lockrows 15b)");
/// lockrows REFFETCH: the same in a lock over a Sort of a join whose other side is REFERENCE-marked (`ref_limit`).
const SYS_REF: ShapeWhy = ShapeWhy("a LIMIT / OFFSET above LockRows over a Sort of a REFERENCE-marked join over a system column or an unrecorded projection (lockrows REFFETCH)");
/// lockrows REFBUILD: the same in a lock whose locked relation is on the build side of the join it probes (`ref_build`).
const SYS_BUILD: ShapeWhy = ShapeWhy("a row lock on the build side of a REFERENCE-marked join over a system column or an unrecorded projection (lockrows REFBUILD)");
/// D3565 REFDRAIN: the same in a lock over the Sort of a Hash Right Anti Join's drain of the locked relation (plan.rs `ref_drain`).
const SYS_DRAIN: ShapeWhy = ShapeWhy("a row lock over the Sort of a right anti join's drain of the locked relation over a system column or an unrecorded projection (D3565 REFDRAIN)");
/// lockrows REFNEST: the same in a lock over a nest whose inner is REFERENCE-marked (rerun/epq/ref_nest.rs).
const SYS_NEST: ShapeWhy = ShapeWhy("a row lock over a nest of a REFERENCE-marked relation over a system column or an unrecorded projection (lockrows REFNEST)");
/// D3379 LOCKNEST: the same in a lock on a nest's inner beside its REFERENCE-marked outer (rerun/epq/ref_nest.rs `lock_form`).
const SYS_LOCK_NEST: ShapeWhy = ShapeWhy("a row lock on a nest's inner relation over a system column or an unrecorded projection (lockrows LOCKNEST)");
/// D3379 LOCKPAIR: the same in a lock on both sides of a nest (rerun/epq/ref_nest.rs `pair_form`).
const SYS_LOCK_PAIR: ShapeWhy = ShapeWhy("a row lock on both sides of a nest over a system column or an unrecorded projection (D3379 LOCKPAIR)");
/// D3802 SORTCHAIN: the same in a lock over a Sort of a join chain whose locked relation is a nest's inner
/// (rerun/epq/sort_chain.rs).
const SYS_SORT_CHAIN: ShapeWhy = ShapeWhy("a row lock over a Sort of REFERENCE-marked joins locking a nest's inner over a system column or an unrecorded projection (D3802 SORTCHAIN)");
/// lockrows SUBLOCK: the same in a lock inside a SubPlan's inner (rerun/epq/sub_lock.rs).
const SYS_SUB: ShapeWhy = ShapeWhy("a row lock inside a SubPlan's inner over a system column or an unrecorded projection (lockrows SUBLOCK)");
/// D3607 COPYPROBE: the same in a lock over hash probes of COPY-marked rows (rerun/epq/ref_copy.rs `probe_form`).
const SYS_COPY_PROBE: ShapeWhy = ShapeWhy("a row lock over hash probes of COPY-marked rows over a system column or an unrecorded projection (D3607 COPYPROBE)");
/// D652-S1's lock half: the same in a lock beside per-row SubPlan steps (write/lockrows.rs `subs_in_ir`).
const SYS_SUBS: ShapeWhy = ShapeWhy("a row lock beside SubPlan steps over a system column or an unrecorded projection (D652-S1)");

/// The lock's recheck in the IR (write/epq_ir.rs): the source's recheck quals and the ops below op `j`; `limit`: a LIMIT /
/// OFFSET above the lock, else (V4R-512) the pipeline makes another's product.
pub(crate) fn epq_of<'g>(px: &mut Px<'g, '_>, k: usize, j: usize, node: i32, relid: u32, limit: bool) -> Epq<'g> {
    let e = crate::rerun::epq::epq_ir::epq_of(px, k, j, node, relid, if limit { SYS_EPQ } else { SYS_PRODUCT });
    let force = crate::ops::lockrows::lock_init::forced(px, node);
    e.forcing(force).arming(crate::ops::lockrows::lock_init::armed(px, node))
}

/// D652-S1's lock half (write/lockrows.rs `subs_in_ir`): the recheck in the IR, each per-row SubPlan step below the lock
/// evaluated again over the newest version (epq_ir.rs `epq_of_subs`).
pub(crate) fn epq_of_subs<'g>(px: &mut Px<'g, '_>, k: usize, j: usize, node: i32, relid: u32) -> Epq<'g> {
    let e = crate::rerun::epq::epq_ir::epq_of_subs(px, k, j, node, relid, SYS_SUBS);
    let force = crate::ops::lockrows::lock_init::forced(px, node);
    e.forcing(force).arming(crate::ops::lockrows::lock_init::armed(px, node))
}

/// D3607 COPYPROBE (rerun/epq/ref_copy.rs `probe_form`): the recheck in the IR over lock pipeline `k`'s ops before the
/// lock `j`, each INNER hash probe of COPY-marked rows matched again over the record it handed on (epq_ir.rs
/// `Redo::Probe`, the redo `epq_of_subs` makes).
pub(crate) fn epq_of_copy_probe<'g>(px: &mut Px<'g, '_>, k: usize, j: usize, node: i32, relid: u32) -> Epq<'g> {
    let e = crate::rerun::epq::epq_ir::epq_of_subs(px, k, j, node, relid, SYS_COPY_PROBE);
    let force = crate::ops::lockrows::lock_init::forced(px, node);
    e.forcing(force).arming(crate::ops::lockrows::lock_init::armed(px, node))
}

/// lockrows SUBLOCK (rerun/epq/sub_lock.rs): the recheck in the IR of a lock inside SubPlan inner `k`: its source's recheck quals and
/// its ops below op `j`, the outer row's parameters as the re-open read them.
pub(crate) fn epq_of_sub<'g>(px: &mut Px<'g, '_>, k: usize, j: usize, node: i32, relid: u32) -> Epq<'g> {
    let e = crate::rerun::epq::epq_ir::epq_of(px, k, j, node, relid, SYS_SUB);
    // D3274: the pending InitPlans the inner reads forced, and the subquery's own armed in the lock's state, as that
    // LockRows' EvalPlanQualStart does
    let force = crate::rerun::epq::sub_lock::forced(px, k, j);
    e.forcing(force).arming(crate::rerun::epq::sub_lock::armed(px, k)).keyed(node as u32 + 1)
}

/// lockrows 15b (write/lockrows.rs `breaker_limit`): the recheck in the IR over the Sort's producer `q`, its source's recheck
/// quals and its ops before the sort build (`jq`): the row the Sort hands on, over the newest version.
pub(crate) fn epq_of_breaker<'g>(px: &mut Px<'g, '_>, q: usize, jq: usize, node: i32, relid: u32) -> Epq<'g> {
    let e = crate::rerun::epq::epq_ir::epq_of(px, q, jq, node, relid, SYS_BREAKER);
    let force = crate::ops::lockrows::lock_init::forced(px, node);
    e.forcing(force).arming(crate::ops::lockrows::lock_init::armed(px, node))
}

/// D3563 (lower.rs `sort_tail`): `epq_of_breaker` whose lock pipeline `k` runs a Result's SubPlan steps and projections
/// past the Sort emit: the producer's part, then `k`'s ops before the lock `j` (epq_ir.rs `epq_of_tail`). D3777 MERGETAIL:
/// `fetch`, a merge's REFERENCE inner in `k`, its row refetched.
pub(crate) fn epq_of_tail<'g>(px: &mut Px<'g, '_>, (q, jq): (usize, usize), (k, j): (usize, usize), node: i32, relid: u32, fetch: &[(usize, i16, u32)]) -> Epq<'g> {
    let e = crate::rerun::epq::epq_ir::epq_of_tail(px, (q, jq), (k, j), node, relid, SYS_BREAKER, fetch);
    let force = crate::ops::lockrows::lock_init::forced(px, node);
    e.forcing(force).arming(crate::ops::lockrows::lock_init::armed(px, node))
}

/// lockrows REFFETCH (write/lockrows.rs `ref_limit`): `epq_of_breaker` whose producer `q` joins the locked relation's scan to
/// a REFERENCE-marked relation's own scan (`fetch`: its build pipeline, its junk ctid column, its relation) by an INNER
/// hash probe: that relation's row refetched at the junk ctid (epq_ir.rs `Fetch`). D3567 REFCHAIN: or several such joins
/// (hash probes of REFERENCE builds, REFNEST's nests), one fetch each in the order of their ops.
pub(crate) fn epq_of_ref<'g>(px: &mut Px<'g, '_>, q: usize, jq: usize, node: i32, relid: u32, fetch: &[(usize, i16, u32)]) -> Epq<'g> {
    let e = crate::rerun::epq::epq_ir::epq_of_ref(px, q, jq, node, relid, SYS_REF, fetch);
    let force = crate::ops::lockrows::lock_init::forced(px, node);
    e.forcing(force).arming(crate::ops::lockrows::lock_init::armed(px, node))
}

/// lockrows REFBUILD (write/lockrows.rs `ref_build`): the recheck of a lock in probe pipeline `k` whose locked leaf `relid`
/// is on the build side: that leaf's arm (and the union's emit) as the build row, the probe side's REFERENCE row refetched
/// (epq_ir.rs `epq_of_build`).
/// D3777 LOCKMERGE: `so`, a merge outer's Sort producer and its sort build op.
#[allow(clippy::too_many_arguments)]
pub(crate) fn epq_of_build<'g>(px: &mut Px<'g, '_>, arm: (usize, usize), union: Option<(usize, usize)>, at: (usize, usize, usize), fetch: (i16, u32), node: i32, relid: u32, so: Option<(usize, usize)>) -> Epq<'g> {
    let e = crate::rerun::epq::epq_ir::epq_of_build(px, arm, union, at, fetch, node, relid, SYS_BUILD, so);
    let force = crate::ops::lockrows::lock_init::forced(px, node);
    e.forcing(force).arming(crate::ops::lockrows::lock_init::armed(px, node))
}

/// D3565 REFDRAIN (plan.rs `ref_drain`): the recheck of a lock over the Sort of a Hash Right Anti Join's drain, `at` = (the
/// Sort's producer, its Drain op, its sort build), whose build `build` = (pipeline, its build op) is the locked relation's
/// own scan (epq_ir.rs `epq_of_drain`).
pub(crate) fn epq_of_drain<'g>(px: &mut Px<'g, '_>, build: (usize, usize), at: (usize, usize, usize), node: i32, relid: u32) -> Epq<'g> {
    let e = crate::rerun::epq::epq_ir::epq_of_drain(px, build, at, node, relid, SYS_DRAIN);
    let force = crate::ops::lockrows::lock_init::forced(px, node);
    e.forcing(force).arming(crate::ops::lockrows::lock_init::armed(px, node))
}

/// lockrows REFNEST (rerun/epq/ref_nest.rs): the recheck in the IR of a lock in pipeline `k` over an INNER nest whose inner
/// is a REFERENCE-marked relation's own scan (`fetch`: the inner's `Pipes` index, its junk ctid column, its relation): that
/// row refetched at the junk ctid, the inner's redo under the nest's parameters (epq_ir.rs `Fetch`). D3564:
/// one such fetch per nest, in op order (REFNEST-CHAIN).
pub(crate) fn epq_of_nest<'g>(px: &mut Px<'g, '_>, k: usize, j: usize, node: i32, relid: u32, fetch: &[(usize, i16, u32)]) -> Epq<'g> {
    let e = crate::rerun::epq::epq_ir::epq_of_ref(px, k, j, node, relid, SYS_NEST, fetch);
    let force = crate::ops::lockrows::lock_init::forced(px, node);
    e.forcing(force).arming(crate::ops::lockrows::lock_init::armed(px, node))
}

/// D3379 LOCKNEST (rerun/epq/ref_nest.rs `lock_form`): the recheck in the IR of lock op `j` in pipeline `k` whose locked
/// relation `relid` is the inner `ik` of its INNER nest op `jn`: the inner's redo over the newest version, joined to the
/// outer row the nest holds (epq_ir.rs `epq_of_inner`).
pub(crate) fn epq_of_lock_nest<'g>(px: &mut Px<'g, '_>, (k, j): (usize, usize), (jn, ik): (usize, usize), node: i32, relid: u32) -> Epq<'g> {
    let e = crate::rerun::epq::epq_ir::epq_of_inner(px, (k, j), (jn, ik), relid, node, SYS_LOCK_NEST);
    let force = crate::ops::lockrows::lock_init::forced(px, node);
    e.forcing(force).arming(crate::ops::lockrows::lock_init::armed(px, node))
}

/// D3379 LOCKPAIR (ops/lockrows/pair.rs): the recheck in the IR of lock op `j` in pipeline `k` over its INNER nest op `jn`
/// whose outer (relation `rels.0`, the main row's) and inner `ik` (`rels.1`, its lock site `isite`) are both locked
/// (epq_ir.rs `epq_of_pair`). D3777 PAIRMERGE: `so`, the merge outer's Sort producer and its sort build op.
pub(crate) fn epq_of_pair<'g>(px: &mut Px<'g, '_>, at: (usize, usize), nest: (usize, usize), rels: (u32, u32), isite: std::rc::Rc<core::cell::Cell<usize>>, node: i32, so: Option<(usize, usize)>) -> Epq<'g> {
    let e = crate::rerun::epq::epq_ir::epq_of_pair(px, at, nest, rels, isite, node, SYS_LOCK_PAIR, so);
    let force = crate::ops::lockrows::lock_init::forced(px, node);
    e.forcing(force).arming(crate::ops::lockrows::lock_init::armed(px, node))
}

/// D3802 SORTCHAIN (rerun/epq/sort_chain.rs): the recheck in the IR of a lock over a Sort whose producer `q` (up to its
/// sort build `jq`) reads a REFERENCE relation's scan, `from` = (its junk ctid column, its relation), and joins the locked
/// relation as a nest's inner (`fetch`: each join's in redo order, the locked inner's `epq_ir::HELD`, its row the lock
/// site `site`'s newest version; epq_ir.rs `epq_of_sort_chain`).
pub(crate) fn epq_of_sort_chain<'g>(px: &mut Px<'g, '_>, at: (usize, usize), node: i32, relid: u32, from: (i16, u32), fetch: &[(usize, i16, u32)], site: &std::rc::Rc<core::cell::Cell<usize>>) -> Epq<'g> {
    let e = crate::rerun::epq::epq_ir::epq_of_sort_chain(px, at, node, relid, SYS_SORT_CHAIN, from, fetch, site);
    let force = crate::ops::lockrows::lock_init::forced(px, node);
    e.forcing(force).arming(crate::ops::lockrows::lock_init::armed(px, node))
}
