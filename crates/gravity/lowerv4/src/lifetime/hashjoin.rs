//! hashjoin's lifetime decisions, which the family asks here: every point where a hash table's match marks start clear,
//! and whether a build stays resident across its rerun set's runs.
//! The marks are one byte per build record (keyed and kept NULL-key alike, ops/hashjoin build.rs `MARKS`), set by a RIGHT /
//! FULL / RIGHT SEMI / RIGHT ANTI probe's matches and read by the drain. They start clear at each build (`built`, as PG's
//! ExecHashTableInsert stores a tuple with its match bit clear), and a table kept across calls starts each call with them
//! clear again (`reset_at_first_window`, `reset_at_reopen`, as PG's ExecReScanHashJoin calls
//! ExecHashTableResetMatchFlags). They are not yet an owner-run object (`Place::Object` at `Scope::OwnerRun`, reset by the
//! owner at the start of each run, the D2625 class's end state); until then the two call points are named here, and a
//! new reset point joins this file (rule RL).
//! The rerun set guards over a hash table's lifetime are here too: whether a member probe's per-call build order or a
//! table kept across the set's calls could show where PG decides per call (`rerun_order`: D1902 / D1904 / D1906, the keep
//! rule's `kept_across`), and whether a probe over a kept build whose side calls (the effects home's answer:
//! effects/visible/hashjoin.rs `build_calls`) takes probe_kept.rs's lazy build (`kept_road`). They are named fences until
//! the keep rule answers these shapes itself.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};

use mcx::GrowIn;
use types_nodes::jointype::JoinType;

use crate::effects::visible::hashjoin::build_calls;
use crate::frame::env::Env;
use crate::graph::Gx;
use crate::ops::hashjoin::build::{FLAGS, FORM, F_SPLIT, KEPT, MARKS, N};
use crate::ops::hashjoin::build_class::{self, side_by, Side};
use crate::ops::hashjoin::{probe, probe_calls, probe_kept};

/// A laid-out table's mark bytes (`marks`: the array past its records, rt.rs `place_marks`) cleared: every lay-out, the
/// serial one (rt.rs `lay_out`) and the partitioned merge's prepare (build_part.rs), so a built or rebuilt table's records
/// start unmatched.
pub(crate) fn built(marks: &mut [u8]) {
    marks.fill(0);
}

/// A kept table's marks cleared at the first window of a call (probe_calls.rs `Calls::at_window`, a kept
/// RIGHT SEMI / ANTI build whose probe's call runs again; `r` its region).
pub(crate) fn reset_at_first_window(b: &mut FunctionBuilder<'_>, env: &mut Env, r: Value) {
    clear(b, env, r);
}

/// D2625: a kept RIGHT SEMI / ANTI table's marks cleared at the nest inner's re-open (drain/source.rs `reopen`; `r` its
/// region, built): PG resets them at every rescan whatever the outer then does, and the probe's first window never comes
/// when a false one-time gate or a zero LIMIT closes its pipeline.
pub(crate) fn reset_at_reopen(b: &mut FunctionBuilder<'_>, env: &mut Env, r: Value) {
    clear(b, env, r);
}

/// The reset itself (ExecHashTableResetMatchFlags): every record's byte, the NULL-key records' (`KEPT`, past the keyed
/// ones) too. An EMPTY table has no marks.
fn clear(b: &mut FunctionBuilder<'_>, env: &mut Env, r: Value) {
    let form = b.load_zext(Type::I8, r, FORM as i32);
    let has = b.icmp(IntCond::Ne, form, env.zero);
    let (l_clear, l_loop, l_body, l_done) = (b.create_block(&[]), b.create_block(&[Type::I64]), b.create_block(&[]), b.create_block(&[]));
    b.cond_br(has, (l_clear, &[]), (l_done, &[]));
    b.switch_to(l_clear);
    let n = b.load(Type::I64, r, N as i32);
    let flags = b.load_zext(Type::I8, r, FLAGS as i32);
    let fs = b.iconst(Type::I64, F_SPLIT as i64);
    let split = b.int_bin(IntBinOp::And, flags, fs);
    let split = b.icmp(IntCond::Ne, split, env.zero);
    let kept = b.load(Type::I64, r, KEPT as i32);
    let kept = b.select(split, kept, env.zero);
    let total = b.int_bin(IntBinOp::Add, n, kept);
    let off = b.load(Type::I64, r, MARKS as i32);
    let marks = b.ptr_add(r, off, 1, 0);
    b.br(l_loop, &[env.zero]);
    // bounded by the table's records (PG's reset checks no interrupt either)
    b.mark_loop_header(l_loop);
    env.bounded.push_in(l_loop);
    b.switch_to(l_loop);
    let i = b.block_params(l_loop)[0];
    let more = b.icmp(IntCond::Slt, i, total);
    b.cond_br(more, (l_body, &[]), (l_done, &[]));
    b.switch_to(l_body);
    let at = b.ptr_add(marks, i, 1, 0);
    let z = b.iconst(Type::I8, 0);
    b.store(at, 0, z);
    let one = b.iconst(Type::I64, 1);
    let next = b.int_bin(IntBinOp::Add, i, one);
    b.br(l_loop, &[next]);
    b.switch_to(l_done);
}

/// A build a rerun set runs or keeps (build pipeline `k` or its probe pipeline `q` a member of a set of
/// `graph.reruns`, or the build's slot `slot` in a set's slots) stays resident: a spill's partition files and pair pass
/// would be one run's, and a kept table serves the set's later runs. spill/build.rs's spill rule (`alone`) keeps such a
/// build in memory (named growth).
pub(crate) fn resident_in_rerun(gx: &Gx<'_, '_>, k: usize, q: usize, slot: u8) -> bool {
    gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&x| x as usize == k || x as usize == q) || gravity_decompose::graph::slot_has(&r.slots, slot))
}

/// D1902's Shape (the SubPlan and the nest roads alike): a rerun set's outer-first build whose sides can raise or call.
pub(crate) const OUTER_FIRST_RAISES: &str = "a rerun set's hash build on PG's outer-first path whose build or outer side can raise or call (PG decides per call whether it builds first: hj_OuterNotEmpty; D1902)";

/// D1904's Shape: a member's probe over a table kept across the set's re-runs, whose outer side can raise or call.
pub(crate) const KEPT_OUTER_RAISES: &str = "a rerun set's hash probe over a table kept across re-runs whose outer side can raise or call (PG's rescan scans the whole outer over the kept table, an empty one too; D1904)";

/// The keep rule's Shape: a member's probe over a build outside its rerun set that the lifetime home does not show kept at
/// the set's re-open (`lifetime::kept_input` has no answer), so PG may build it again at a call where Gravity reads it.
pub(crate) const KEPT_UNKNOWN: &str = "a rerun set's hash probe over a build outside the set that the keep rule does not show kept at the set's re-open (lifetime kept_input: no answer)";

/// D1906's Shape: a member's probe over a kept table whose build side calls.
pub(crate) const KEPT_BUILD_CALLS: &str = "a rerun set's hash probe over a table kept across re-runs whose build side calls a user or VOLATILE function (PG rebuilds a multi-batch table at every rescan; D1906)";

/// D1902 (M8-RERUN, fail closed): the Shape probe `j` of graph pipeline `q` over build pipeline `b` names when both are
/// members of one rerun set (graph.reruns: a nest's inner or a per-row SubPlan's, re-run at every call) and PG's per-call
/// order would show. nodeHashjoin.c's HJ_BUILD_HASHTABLE decides per call: LEFT / ANTI fetch the outer's first row before
/// they build; the cost path (INNER / SEMI / RIGHT SEMI outer-first) does so at a first call, but ExecReScanHashJoin keeps
/// hj_OuterNotEmpty over a rebuild: after a call whose outer had a row it builds first, and an empty table never runs the
/// outer. The set re-runs one order (op 14 holds the build's error for the first row), so on the outer-first path a build
/// side (its keys too) that can raise or call refuses, and on the cost path an outer side too (the ops before the probe,
/// its source and feeders). A lazy build (D824) runs at the probe's first row, as PG's LEFT / ANTI builds (D1700); on the
/// cost path rerun/sets.rs refuses it (D1654) unless its outer side holds the same rows at every call (`steady`).
/// D1904: over a build outside the set (a one-batch table no changed parameter reads: ExecReScanHashJoin reuses it and
/// goes to HJ_NEED_NEW_OUTER, no emptiness check) every rescan scans the whole
/// outer and computes each row's outer hash keys (ExecHashJoinOuterGetTuple), an empty table too, so an outer side or an
/// outer key (not the join qual: it runs on a bucket's matches) that can raise or call refuses on either path unless
/// LEFT / ANTI / FULL (they scan the outer whatever the table holds; their keys over an empty table are D1908's). D1906:
/// a kept build side (its ops, keys and feeders) that calls a user or VOLATILE function, or holds what the side class reads
/// as Raises without seeing into it (`opaque`), is served only over probe_kept.rs's lazy build (it runs at
/// PG's first build and is kept at any size: D2713, call counts are free) and refuses on any other join type and path.
/// A PARAM_EXEC a nest or a per-row SubPlan binds reads as quiet (`side_by`). Checked at the member's make, where the side
/// classification is: the members are made before their owner. D1902's outer side has no key term: over an empty rebuilt
/// table PG computes no outer key either.
pub(crate) fn rerun_order(gx: &Gx<'_, '_>, q: usize, j: usize, b: usize, lazy: bool) -> Option<&'static str> {
    let Some(gravity_decompose::Op::Join(gravity_decompose::join::JoinDesc::Probe { jointype, outer_first, catalog: false, .. })) = gx.pipelines()[q].ops.get(j) else { return None };
    let fill = matches!(jointype, JoinType::JOIN_LEFT | JoinType::JOIN_ANTI | JoinType::JOIN_FULL);
    // D1902 (errorhold): a raising build op 14 holds is held per call (the driver's rerun call scope: a call's member probe
    // raises its own build's error, a call whose outer has no row drops it) where PG's per-call order cannot show: under a
    // quiet outer side, a fill type fetches its outer's first row before it builds at every call, and the cost path's outer,
    // reading no executor parameter, is empty at every call or at none (PG builds at no call, or at every call before its
    // first row). A build side holding what the side class does not see into (`opaque`: a call it may hide) stays refused,
    // and so does a statement holding user code the class does not read (`quiet_statement`: a user HASHES operator in the
    // hash clause, which v4 compares by the key's type, D2036).
    // A cost-path outer that reads an executor parameter (empty at some calls only) takes `Order` over the held build
    // (`held_order`): a build-first call raises the held error at its first window, as PG's build does over an empty outer.
    let held = || side_by(gx, b, usize::MAX, false) == Side::Raises && !probe::opaque(gx, b) && build_class::quiet_statement(gx) && build_class::defers(gx, b, q, j) && side_by(gx, q, j, false) == Side::Safe && (fill || !probe::reads_param(gx, q, j) || probe::held_order(gx, q, j, b, lazy));
    let (build, outer) = (|| side_by(gx, b, usize::MAX, false) != Side::Safe && !held(), |key| side_by(gx, q, j, key) != Side::Safe);
    gx.graph.reruns.iter().filter(|r| r.pipelines.contains(&(q as u16))).find_map(|r| match r.pipelines.contains(&(b as u16)) {
        // D3217: a set kept whole runs the join once, at its first run, where PostgreSQL's first call decides as for a probe
        // in no set (inherit#404's reading, rerun/sets.rs): no later call decides again
        true if crate::rerun::sets::kept_whole(gx, r) => None,
        true => (*outer_first && !lazy && (build() || !fill && outer(false)) && !probe_calls::orders(gx, q, j, b)).then_some(OUTER_FIRST_RAISES),
        // the keep rule (`kept_across`): a build outside the set whose table it does not show kept
        false if !kept_across(gx, r, b) => Some(KEPT_UNKNOWN),
        // D1906 (probe_kept.rs): served over a lazy build, the table kept at any size (D2713: call counts are free)
        false if build_calls(gx, b, false) => (!probe_kept::kept(gx, q, j, b, lazy)).then_some(KEPT_BUILD_CALLS),
        // D1906: a kept build whose calls the side class does not read (probe_kept.rs `hidden`): served over a lazy build
        // under a quiet outer side; else refused by what blocks it: an outer side that may raise or call over the lazy build
        // (D1904's: PG's whole-outer rescan), a build that takes no lazy group (D1906's)
        false if kept_road(gx, q, j, b) && !probe_kept::kept(gx, q, j, b, lazy) => Some(if lazy { KEPT_OUTER_RAISES } else { KEPT_BUILD_CALLS }),
        false => (!fill && outer(true) && !probe_kept::rescans(gx, q, j, b, lazy)).then_some(KEPT_OUTER_RAISES),
    })
}

/// D1906: probe `j` of pipeline `q` over build `b` is probe_kept.rs's road's: an INNER or SEMI probe (not psql's catalog
/// form) on PG's outer-first path, a rerun set's member, over a build every set holding it keeps (`kept_across`) whose
/// side calls, visibly (D1906's refusal) or hiddenly (an eager build ran such a call over an outer empty at the first
/// call, where PG never builds). Whether the side calls is the effects home's answer (effects/visible/hashjoin.rs
/// `build_calls`); what the table's lifetime makes of it is decided here.
pub(crate) fn kept_road(gx: &Gx<'_, '_>, q: usize, j: usize, b: usize) -> bool {
    let Some(gravity_decompose::Op::Join(gravity_decompose::join::JoinDesc::Probe { jointype, outer_first: true, catalog: false, .. })) = gx.pipelines()[q].ops.get(j) else { return false };
    let sets = || gx.graph.reruns.iter();
    let holds_q = |r: &&gravity_decompose::graph::Rerun| r.pipelines.contains(&(q as u16));
    matches!(jointype, JoinType::JOIN_INNER | JoinType::JOIN_SEMI | JoinType::JOIN_UNIQUE_OUTER | JoinType::JOIN_UNIQUE_INNER)
        && sets().any(|r| holds_q(&r))
        && sets().filter(holds_q).all(|r| kept_across(gx, r, b))
        && build_calls(gx, b, true)
}

/// Build `b` keeps its table across a re-open of rerun set `r` (one that holds the probe), by the lifetime home's keep rule
/// (`lifetime::kept_input`): PG keeps it (`Pg`), keeps it unless a fact the plan does not show holds (`Usual`: a
/// multi-batch table PG rebuilds, which only repeats calls whose count is free, D2713), runs it again to the same rows
/// (`Quiet`), or never reaches it (`Unreached`). No answer (`r` runs `b` again, or the rule recorded none): not kept.
pub(crate) fn kept_across(gx: &Gx<'_, '_>, r: &gravity_decompose::graph::Rerun, b: usize) -> bool {
    use gravity_decompose::lifetime::KeptBy;
    matches!(crate::lifetime::kept_input(gx.graph, r.node_id, r.plan_id, b), Some(KeptBy::Pg { .. } | KeptBy::Usual { .. } | KeptBy::Quiet | KeptBy::Unreached))
}
