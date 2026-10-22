//! A hash probe among a nest inner's steps (M8-NEST-B D1018 row B0; a helper of nest/inner.rs, split for its budget). The
//! inner probes a table built once, before its owner: no rerun set holds the build's slot or pipeline, so PG's rescan keeps
//! it (one batch, no changed parameter) and every re-open reads it. The probe's region is read in the owner's program at the
//! index the owner's descriptor gives the slot (frame/tail.rs `Px::consumed` maps an embedded inner to its outermost owner,
//! Pipes::consumed_at's rule), so two inners of one owner each probing its own build read each at its own index (regress
//! join#113 / #117). No graph pipeline probes the build (the build's shape is then the inner's: every column of its last
//! projection); one that probes another life of the slot's number (heldout dsx-0051: a CTE's own hash join) reads another
//! table.
//! An empty build ends the re-open at the probe's window check (`WindowDone`: the walk's end, never the owner's), where
//! PG's later rescans scan the probe side against the empty table: its source (D1762: a scan, a Result or quiet VALUES),
//! the ops before the probe and its own hash keys (D1910: ExecHashJoinOuterGetTuple hashes every row) raise nothing (the
//! count-free ruling: how many times PG's order would call them is free; `ok`'s `calm`), and the build side, a scan, acts
//! nowhere and raises nothing (`still`: the table is reused across outer rows) (PG's outer-first path builds at the first probe-side row;
//! a heap, index or bitmap scan, item 9 (b): PG keeps the one-batch table over any of them), with index quals (D1765) and hash keys (D1764) read before any exec parameter is set (PG may not build: empty nest outer,
//! outer-first probe side). D1909: an INNER or SEMI probe not prefix-safe there (join/probe_inner.rs: PG's outer-first path
//! and a PARAM_EXEC before it) ends at its first row with `Env::full`: the inner then takes the LIMIT's ends (`needs_end`,
//! nest/inner.rs), so that exit ends this re-open's walk, never the owner's. D1337(H): the build may instead be a member of the embedding SubPlan's own rerun set, declared and re-run by
//! the inner's re-open (rerun/sets.rs `owner_set`) before its first window, so a probe of it that is not lazy is prefix-safe
//! (ops/hashjoin/probe.rs, the same `sub_set` test) and an empty build ends the walk there (PG rebuilds a Hash whose child's
//! chgParam is set at every call). A lazy one (a VOLATILE build side on PG's outer-first path, D824) stays not prefix-safe:
//! an empty build would take `Env::full`, the owner's exit, so it never serves (no_widen's NW_D2194 through `Px::lazy`, the
//! owner's inner made before that check, nest/subplan.rs; a build that cannot wait refuses first as 'not deferred'). The
//! rules above are the kept table's (PG reuses it); the calls PG's per-call build order skips are the volatile family
//! (DEFER-VOL). INNER, LEFT, SEMI, ANTI; no catalog probe, no Memoize; else D1018's name. Registers nothing. Owner M8-NEST.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::{Op, PipelineNode, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Block;
use gravity_program::ArenaRef;
use types_nodes::jointype::JoinType;
use types_nodes::Node;

use crate::frame::frame::reached;
use crate::frame::row::Carry;
use crate::frame::stop::Exits;
use crate::effects::visible::nestloop::{bitmap_quiet, probe_quiet, quiet_ops};
use crate::graph::Gx;

/// Whether op `i` of inner `ik` is a probe the inner serves (the module's rule). `arena`: the effects home's reader's.
/// The count-free rule: how many times a VOLATILE or user call runs is free
/// so long as every value a row uses has a call of its own and none is reused for another row; no new errors and CTE-once
/// still hold. So what PG's per-call order skips (an empty outer or build) is judged by its raises only (`calm`: the
/// effects home's may-raise, constant patterns and divisors included), where each re-open reads it again: the probe side's
/// source, steps before the probe and keys, and a nest-set build PG rebuilds at every rescan. A kept table, read once and
/// reused across outer rows as PG reuses its one-batch table, stays `still` (acts nowhere and raises nothing). The prefix
/// test (D1909) keeps the strict reading the probe's own prefix rule was written against; it gives the inner its ends
/// (`needs_end`) and no longer refuses the probe.
pub(crate) fn ok(gx: &Gx<'_, '_>, arena: ArenaRef, ik: usize, ip: &PipelineNode<'_>, i: usize) -> bool {
    crate::stack_check(); // D3832
    let Some(Op::Join(JoinDesc::Probe { slot, catalog: false, jointype, keys, .. })) = ip.ops.get(i) else { return false };
    let jt = matches!(jointype, JoinType::JOIN_INNER | JoinType::JOIN_UNIQUE_OUTER | JoinType::JOIN_UNIQUE_INNER | JoinType::JOIN_LEFT | JoinType::JOIN_SEMI | JoinType::JOIN_ANTI);
    let Some(b) = gx.life_of(ik, *slot).map(|l| l.0 as usize) else { return false };
    let ps = gx.pipelines();
    // D1018 (heldout dsx-0034): a set that also re-runs the inner's owner (an enclosing nest's: this nest's pipeline is its
    // member) lays the build out ahead of the owner at each of its runs, so within one run the table is kept across this
    // nest's re-opens as PG keeps its one-batch table (no chgParam); the build is still (`scan`, `built`), so a rebuild
    // at the enclosing re-open, where PG keeps or (a changed parameter) rebuilds it, reads the same rows
    let owner = top(gx, ik) as u16;
    let rerun = gx.graph.reruns.iter().any(|r| (gravity_decompose::graph::slot_has(&r.slots, *slot) || r.pipelines.iter().any(|&x| x as usize == b)) && !r.pipelines.contains(&owner));
    let probed = ps.iter().enumerate().any(|(x, p)| p.ops.iter().any(|op| matches!(op, Op::Join(JoinDesc::Probe { slot: s, .. }) if s == slot)) && gx.life_of(x, *slot).is_some_and(|l| l.0 as usize == b));
    let at = |k: usize| ps[k].desc.consumes.iter().position(|s| s == slot);
    let bp = &ps[b];
    let still = |n: Node<'_>| { let e = reads(gx, arena, n, false); !e.acts && !e.may_raise };
    let calm = |n: Node<'_>| !reads(gx, arena, n, true).may_raise;
    let scan = match &bp.source { Source::Range { .. } => true, Source::Index { keys, recheck, .. } => keys.iter().all(|k| still(k.value)) && recheck.iter().all(still), Source::Bitmap { plan, recheck, .. } => bitmap_quiet(plan, recheck, &still), _ => false }; // D1765; item 9 (b): a bitmap scan too
    // D1764: the build side's steps (`kept_side`: quiet steps, a quiet nest join, a hash join over a kept table)
    let built = kept_side(gx, arena, b, 0);
    let payload = bp.ops.iter().rev().find_map(|op| if let Op::Project(l) = op { Some(l.len()) } else { None }).is_some_and(|n| n > 0);
    // item 9 (b) part 4: an earlier probe of the inner (a hash join stacked under this one) is quiet when it is a probe
    // this rule serves and its keys, clauses and join qual are quiet: PG's rescans run it against its own kept table
    // a nest step before the probe (the inner's own nest join, e.g. a key lookup feeding the probe side) is quiet when its
    // inner raises nothing (effects.rs `nest_step_quiet`) and, for the prefix test, calls nothing VOLATILE
    // regress create_table_like#464's FROM-clause set function on the probe side too (scan-0017 / nl-0204: unnest(i.indkey)
    // ahead of a probe of the nest's own set): one effects.rs `srf_quiet` reads quiet acts nowhere and raises nothing
    let bound = |id: i32| gx.graph.params.iter().any(|x| x.0 == id);
    // D1018 (heldout rnd-0426): the probe side a union's emit streams (nest/inner_arms.rs: PG's Append, each arm pulled in
    // order at every re-open), its Union op the inner's op 0, every arm calm (`arm_calm`) as a scan source is below
    let arms = matches!(ip.source, Source::Breaker { .. }) && crate::pipes::inner_union(gx, ik).is_some_and(|u| u.0 == ik && u.2.iter().all(|&f| crate::effects::visible::rerun::arm_calm(&ps[f], &calm)));
    let union_op = |j: usize, op: &Op<'_>| arms && j == 0 && matches!(op, Op::Append(gravity_decompose::ops::shared::append::AppendDesc::Union { .. }));
    // D3854 (c-random rnd-2554): the probe side a RIGHT / FULL drain's spool hands on (INNER-SPOOL, rerun/inner.rs: the
    // spooled join filled once ahead of the owner, its spool re-read at every re-open, its Drain op the inner's op 0, then
    // the drained build records): reading the records and handing the drained ones on raise nothing and call nothing
    let drained = matches!(ip.source, Source::Breaker { .. }) && gx.source_kind(ik) == crate::face::kinds::KindId::JoinDrain && gx.spooled_inner(ik);
    let drain_op = |j: usize, op: &Op<'_>| drained && j == 0 && matches!(op, Op::Join(JoinDesc::Drain { .. }));
    let pre_op = |j: usize, op: &Op<'_>, q: &dyn Fn(Node<'_>) -> bool, acts: bool| union_op(j, op) || drain_op(j, op) || quiet_ops(std::slice::from_ref(op), q) || crate::effects::nest_step_quiet(op, q, acts) || crate::effects::srf_quiet(gx, op, &bound) || srf_project(&ip.ops[..i], j, q) || probe_quiet(op, q) && ok(gx, arena, ik, ip, j);
    let pre_by = |q: &dyn Fn(Node<'_>) -> bool, acts: bool| ip.ops[..i].iter().enumerate().all(|(j, op)| pre_op(j, op, q, acts));
    let pre = |q: &dyn Fn(Node<'_>) -> bool| pre_by(q, true);
    let src = arms || drained || match &ip.source { Source::Range { .. } | Source::Index { .. } | Source::Const => true, Source::Bitmap { plan, recheck, .. } => bitmap_quiet(plan, recheck, &calm), Source::Values { lists, .. } => lists.iter().all(|l| l.as_list().is_some_and(|r| r.iter().all(calm))), _ => false }; // D1762
    // D3378 C3 / C4 / C5 (D1910 / D1762): or a probe that walks every probe-side row past PG's build call, as PG's rescans
    // do: D1904's later-call words (probe_calls.rs `nest_rescans`: INNER / SEMI, on either of PG's paths), or a fill type
    // (LEFT / ANTI hand every row on; D1908's keys). Its keys, the filters, gates and projections before it and its VALUES
    // cells then run row by row in PG's order, so they may raise
    let walks = crate::ops::hashjoin::probe_calls::nest_rescans(gx, ik, i, b) || matches!(jointype, JoinType::JOIN_LEFT | JoinType::JOIN_ANTI);
    let any = |_: Node<'_>| true;
    let walked = walks && ip.ops[..i].iter().enumerate().all(|(j, op)| quiet_ops(std::slice::from_ref(op), &any) || pre_op(j, op, &calm, true));
    let cells = matches!(ip.source, Source::Values { .. });
    let kept = !rerun && scan && built && (pre(&calm) && keys.iter().all(calm) && src || walked && (src || cells));
    // D1337(H): a build of the embedding SubPlan's own set, laid by its re-open before the first window (prefix-safe by that)
    let own = super::sets::sub_set(gx, ik).is_some_and(|r| r.pipelines.contains(&(b as u16))) && matches!(bp.source, Source::Range { .. } | Source::Index { .. }) && matches!(bp.ops.last(), Some(Op::Join(JoinDesc::Build { .. })));
    // D1018 (a nest's own set): a build of the nest's own rerun set, re-run by the inner's re-open before its first window
    // (rerun/sets.rs `owner_set`), where PG rebuilds the Hash whose child's chgParam is set at every rescan: its side (a
    // scan or VALUES, its ops and keys) and the inner's steps before the probe and its keys calm with the bound params, so
    // PG's per-call order (outer-first or build-first, an empty outer or build) shows no raise
    // D2013 slice 1 (regress join#809 / #810): a build over a settled union the set refills at every re-open (rerun/union_arms.rs
    // `member_refill`: quiet arms, emit and pruning), its source the union's record, its Union op the row
    let union = matches!(bp.source, Source::Breaker { .. }) && crate::rerun::union_arms::refill_of(gx, b).is_some_and(|u| u.0 == b);
    let bsrc = union || match &bp.source { Source::Range { .. } | Source::Const => true, Source::Index { keys, recheck, .. } => keys.iter().all(|k| calm(k.value)) && recheck.iter().all(calm), Source::Bitmap { plan, recheck, .. } => bitmap_quiet(plan, recheck, &calm), Source::Values { lists, .. } => lists.iter().all(|l| l.as_list().is_some_and(|r| r.iter().all(calm))), _ => false };
    // regress create_table_like#464: a FROM-clause set function over the bound parameters (unnest(s.stxkeys::int2[]))
    let side = |rest: &[Op<'_>]| rest.iter().enumerate().all(|(x, op)| quiet_ops(std::slice::from_ref(op), &calm) || crate::effects::srf_quiet(gx, op, &bound) || union && x == 0 && matches!(op, Op::Append(gravity_decompose::ops::shared::append::AppendDesc::Union { .. })));
    // regress join#346 / #347: or a build side that may raise, which op 14 holds per re-open (ops/hashjoin/probe.rs `nest_held`)
    let nest = super::sets::nest_set(gx, ik).is_some_and(|r| r.pipelines.contains(&(b as u16))) && (bsrc && bp.ops.split_last().is_some_and(|(last, rest)| matches!(last, Op::Join(JoinDesc::Build { keys, .. }) if keys.iter().all(calm)) && side(rest)) || crate::ops::hashjoin::probe::nest_held(gx, ik, i, b)) && pre(&calm) && keys.iter().all(calm);
    // D3378 (D1764 / D1765): a kept build whose side may raise, op 14 holding its error for the probe's first row on PG's
    // outer-first path (probe.rs `kept_held`)
    let held = !rerun && crate::ops::hashjoin::probe::kept_held(gx, ik, i, b) && pre(&calm) && keys.iter().all(calm);
    jt && (kept || (own || nest || held) && src) && !probed && at(ik).is_some() && at(top(gx, ik)).is_some() && payload
}

/// D1909 / D2820: whether inner `ik` (`ip`) holds a probe that ends at its first row over an empty table: one no fill
/// type (LEFT, FULL, ANTI, a catalog probe: probe_fill.rs `Fill::of`) whose prefix rule fails (ops/hashjoin/probe.rs
/// `prefix_safe_of`, the probe's own reading, `lazy` when some build of the inner waits for its first row: `Px::lazy`).
/// That probe ends with `Env::full`, where PG's outer-first fetch ran that row before it built the empty table (and its
/// later rescans read every row against the kept one: `ok`'s `calm` terms say none raises), so the inner takes the LIMIT's
/// ends (nest/inner.rs `make`, rerun/inner_limit.rs `Ends`): that exit ends this re-open's walk, never the owner's
/// pipeline (a LEFT nest's NULL-extended row, an anti nest's kept row). A LIMIT already gives the inner its ends.
pub(crate) fn needs_end(gx: &Gx<'_, '_>, ik: usize, ip: &PipelineNode<'_>, lazy: bool) -> bool {
    ip.ops.iter().enumerate().any(|(i, op)| {
        let Op::Join(JoinDesc::Probe { slot, jointype, catalog: false, .. }) = op else { return false };
        !matches!(jointype, JoinType::JOIN_LEFT | JoinType::JOIN_FULL | JoinType::JOIN_ANTI)
            && gx.life_of(ik, *slot).is_none_or(|l| !crate::ops::hashjoin::probe::prefix_safe_of(gx, ik, i, l.0 as usize, lazy))
    })
}

/// The effects home's reading of `n` (`effects::of_expr` with no statement: a SubPlan in it is unknown, so it acts and may
/// raise), the values a nest or a per-row SubPlan binds (`PipelineGraph::params`) read as bound words under `exec`; a
/// list (a gate's implicit AND) member by member.
fn reads(gx: &Gx<'_, '_>, arena: ArenaRef, n: Node<'_>, exec: bool) -> gravity_decompose::effects::Effects {
    use gravity_decompose::effects::{of_expr, Cx, Effects, Scope};
    let bound = |id: i32| exec && gx.graph.params.iter().any(|x| x.0 == id);
    let (cx, sc) = (Cx::without_stmt(gx.facts, arena), Scope { bound: &bound, fetch: None });
    match n.as_list() {
        Some(l) => l.iter().fold(Effects::NONE, |e, m| e.and(of_expr(&cx, m, &sc))),
        None => of_expr(&cx, n, &sc),
    }
}

/// The Shape `own_order` names (D2227).
pub(crate) const OWN_BUILD_ORDER: &str = "a SubPlan inner's hash probe over a build its own rerun set re-runs, on PG's outer-first path, whose build or probe side can raise or call (PG decides per call whether it builds first: hj_OuterNotEmpty; D2227)";

/// D2227: probe `i` of inner `ik` reads a build of the
/// embedding SubPlan's own rerun set (`ok`'s D1337(H) road), which the inner's re-open lays before its first window at
/// every call, so an empty build ends the walk before the probe side runs and a build that raises raises before it. PG
/// 18.6's HJ_BUILD_HASHTABLE decides per call: LEFT / ANTI fetch the outer's first row before they build, and the cost
/// path (INNER / SEMI outer-first) does so while hj_OuterNotEmpty is clear (a first call, or one after a call whose outer
/// gave no row with non-NULL keys: PG clears it after each build), where an empty outer never builds and an empty table
/// ends the call after one outer row. So on the outer-first path a build side (its ops and keys) that can raise or call
/// refuses (an empty outer never runs it: 22012 where PG returns rows), and so does, on the cost path, the probe side
/// before the probe (its source and steps: the first row PG fetches before an empty table, VOLATILE calls counted; not
/// its outer hash keys, which PG computes only past a table with rows, as both orders do). A fill type scans its whole
/// outer at every call, whatever the table holds. `side_by` reads a parameter the SubPlan binds as quiet. The build-first
/// path (PG builds first at every call) shows nothing.
pub(crate) fn own_order(gx: &Gx<'_, '_>, ik: usize, ip: &PipelineNode<'_>, i: usize) -> Option<&'static str> {
    use crate::ops::hashjoin::build_class::{side_by, Side};
    let Some(Op::Join(JoinDesc::Probe { slot, jointype, outer_first: true, catalog: false, .. })) = ip.ops.get(i) else { return None };
    let b = gx.life_of(ik, *slot).map(|l| l.0 as usize)?;
    super::sets::sub_set(gx, ik).filter(|r| r.pipelines.contains(&(b as u16)))?;
    let fill = matches!(jointype, JoinType::JOIN_LEFT | JoinType::JOIN_ANTI | JoinType::JOIN_FULL);
    (side_by(gx, b, usize::MAX, false) != Side::Safe || !fill && side_by(gx, ik, i, false) != Side::Safe).then_some(OWN_BUILD_ORDER)
}

/// The graph pipeline whose program lowers inner `k` (its outermost owner).
fn top(gx: &Gx<'_, '_>, mut k: usize) -> usize {
    let (n, inners) = (gx.pipelines().len(), gx.pipelines().inners());
    while k >= n {
        k = inners[k - n].0 .0;
    }
    k
}

/// D1764: build pipeline `b` ends in a Build whose keys are still and its steps before it are still (acts nowhere, raises
/// nothing; a value the nest binds reads as bound): filters, gates and projections; regress create_function_sql#92, a
/// nest join on the build side (information_schema's routines joined to their return type) that effects.rs
/// `nest_step_quiet` reads so with no VOLATILE call; and gen nl-0202, a hash join on the build side (information_schema's
/// pg_class probing its pg_namespace table) whose keys, clauses and qual are still and whose own table is a kept one by
/// this same reading over a quiet scan, in no rerun set (PG builds the nested Hash once with the kept table, and an empty
/// outer that skips both shows nothing when neither acts nor raises). `depth` bounds the nesting.
fn kept_side(gx: &Gx<'_, '_>, arena: ArenaRef, b: usize, depth: u8) -> bool {
    crate::stack_check();
    let bp = &gx.pipelines()[b];
    let still = |n: Node<'_>| { let e = reads(gx, arena, n, false); !e.acts && !e.may_raise };
    let still_x = |n: Node<'_>| { let e = reads(gx, arena, n, true); !e.acts && !e.may_raise };
    let nested = |op: &Op<'_>| {
        // D1018 (heldout scan-0016): a catalog probe too (decompose join.rs `catalog_lookup`: psql's lookup nest in the hash
        // join's form, its inner read once with no parameters), read by the same keys, clauses and qual
        let Op::Join(JoinDesc::Probe { slot, keys, clauses, qual, .. }) = op else { return false };
        let Some(b2) = gx.life_of(b, *slot).map(|l| l.0 as usize) else { return false };
        let rerun = gx.graph.reruns.iter().any(|r| gravity_decompose::graph::slot_has(&r.slots, *slot) || r.pipelines.iter().any(|&x| x as usize == b2));
        let p2 = &gx.pipelines()[b2];
        let scan = match &p2.source { Source::Range { .. } => true, Source::Index { keys, recheck, .. } => keys.iter().all(|k| still(k.value)) && recheck.iter().all(still), Source::Bitmap { plan, recheck, .. } => bitmap_quiet(plan, recheck, &still), _ => false };
        depth < 3 && !rerun && scan && keys.iter().chain(clauses.iter()).chain(qual.iter()).all(still_x) && kept_side(gx, arena, b2, depth + 1)
    };
    // D1018 (heldout scan-0016): a set function that acts nowhere and raises nothing (effects.rs `srf_quiet`: information_schema
    // key_column_usage's _pg_expandarray, `quiet::sql_body`) in the kept build side
    let bound = |id: i32| gx.graph.params.iter().any(|x| x.0 == id);
    bp.ops.split_last().is_some_and(|(last, rest)| matches!(last, Op::Join(JoinDesc::Build { keys, .. }) if keys.iter().all(still)) && rest.iter().enumerate().all(|(j, op)| quiet_ops(std::slice::from_ref(op), &still_x) || crate::effects::nest_step_quiet(op, &still_x, false) || crate::effects::srf_quiet(gx, op, &bound) || srf_project(rest, j, &still_x) || nested(op)))
}

/// D1018 (create_function_sql#92): op `j` of `ops` is the projection after a target-list set function step, which lists
/// that step's own calls again (the step hands their values on: `effects::srf_call`, read by `srf_quiet`) beside entries
/// quiet by `q`.
fn srf_project(ops: &[Op<'_>], j: usize, q: &dyn Fn(Node<'_>) -> bool) -> bool {
    let (Some(Op::Project(l)), Some(Op::Srf(d))) = (ops.get(j), j.checked_sub(1).and_then(|p| ops.get(p))) else { return false };
    l.iter().all(|n| crate::effects::srf_call(d, n) || q(n))
}

/// The steps' window exits (`Exits::l_done`, a LIMIT's `ends` when there is one): with a probe, a block that ends this
/// re-open's walk with the window's carry (an empty build), never the owner's pipeline.
pub(crate) fn exits(b: &mut FunctionBuilder<'_>, x: &Exits, ends: Option<Exits>, probe: bool) -> (Option<WindowDone>, Exits) {
    match ends {
        Some(e) => (None, e),
        None if probe => {
            let w = b.create_block(&[]);
            (Some(WindowDone(w)), Exits { l_done: w, ..*x })
        }
        None => (None, *x),
    }
}

#[derive(Clone, Copy)]
pub(crate) struct WindowDone(Block);

impl WindowDone {
    /// After the steps' window checks: to the walk's end `l_done` with the window's carry `wc`.
    pub(crate) fn seal(self, b: &mut FunctionBuilder<'_>, l_done: Block, wc: &Carry) {
        if reached(b, self.0) {
            let here = b.current();
            b.switch_to(self.0);
            b.br(l_done, &wc.params());
            b.switch_to(here);
        }
    }
}
