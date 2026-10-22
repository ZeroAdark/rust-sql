//! The rerun home's keep answers (decompose rerun.rs builds the sets): which members of a nest's or SubPlan's rerun set
//! PostgreSQL 18.6 keeps across the owner's re-opens (ExecReScanAgg's hashed table, D2612), which closers its rescan
//! re-executes with no changed param (ExecReScanAgg / Unique / Group / WindowAgg), and which kept breakers a re-executed
//! reader may read again (D1018 item 9 (b)). rerun.rs records these answers in `graph.kept` and the sets. Which pipelines
//! a re-open runs again (`rerun_set`, over each closer's `on_rescan` answer) is decided here too.
use gravity_program::ArenaVec;
use types_nodes::{Node, NodeTag};

use crate::admit::Refusal;
use crate::graph::{slot_has, slot_set, SlotSet};
use crate::ledger::Kind;
use crate::lifetime;
use crate::rerun::{fill_effects, hashed_closer, of_rerun, owner_inner, sub_build, tag_sets_closer, Owner, ParamSet};
use crate::walk::Builder;

/// D2612 (PG's ExecReScanAgg): the members of rerun set `set` (an owner's re-open changing `params`) PG keeps at every
/// re-open. A hashed Agg (AGG_HASHED, one hash table) whose input reads no changed param and whose aggregate arguments read
/// none (aggParams) keeps its filled table (ResetTupleHashIterator): the param only in its HAVING, its projection or a node
/// above. Changed: the owner's params and the setParams of an InitPlan the set re-decides (PG's ExecReScanSetParamPlan
/// adds them to the node's chgParam). The pipeline filling the table and every set member feeding it (closed over the
/// slots they read, as `declare_breaker_rerun` walks a fill) join `graph.kept`: they run at the set's first run only
/// (lowerv4 `RERUN_KEPT`, exec v4 rerun `again`), PG's first ExecAgg after the first re-open; the table's emit stays a
/// member re-run at every re-open over the kept table. A fill whose rows another member also reads, that holds a hashed
/// SubPlan's build (its own road) or an InitPlan's pipeline stays re-run. PG rebuilds a table that spilled
/// (hash_ever_spilled), which moves only how often a fill that acts (a VOLATILE call in its input or aggregate arguments)
/// runs its calls, and that count is free (at least one call per row whose value is
/// used), so a fill that acts is kept as a quiet one is, at any size, its table growing as it needs (D2711: a run-time
/// group cap checked at the emit raised XX000 where the planner underestimated). A fill the set cannot keep runs
/// again at every re-open, which a fill whose VOLATILE calls may raise must not (no new error where PG keeps the table):
/// it refuses by `KEPT_HASHED_RAISES`.
/// D4104: a hashed GROUPING SETS table in its tag form (`tag_sets_closer`), and with `loud_only` (a recursive union's
/// generation set) any fill, is kept only where running it again is what the set may not do: a fill that acts and may
/// raise. Elsewhere such a fill runs again at every re-open as before (keeping or rebuilding an acting fill is a speed
/// choice, D2711). Kept so, its every param is the set's own or one the statement sets once (`fixed_params`): exec runs a
/// kept member until a run fills it and never again, so a table over a param an enclosing owner changes would go stale
/// where PG rebuilds it; such a fill refuses by `KEPT_HASHED_RAISES`.
/// D4120: with `loud_only`, a kept fill may also read params an enclosing owner changes, returned as the second list: the
/// caller records them (`graph.stale_reads`), and the enclosing set that changes them refills the set at its every run
/// (`claim_refills`, exec `SetRearms::refills`), where PG rebuilds the table. Every kept fill of the set reads the same
/// such params, so a refill runs no fill again that PG keeps; else the set refuses by `KEPT_HASHED_RAISES`.
pub(crate) fn kept_hashed(b: &Builder<'_, '_>, p0: usize, set: &[u16], params: &[i32], loud_only: bool) -> Result<(ArenaVec<u16>, ArenaVec<i32>), &'static str> {
    use gravity_abi::breaker::BreakerKind;
    let ps = &b.graph.pipelines;
    let mut keep: ArenaVec<u16> = ArenaVec::new_in(b.arena); // alloc: Cold
    let mut outer: Option<ArenaVec<i32>> = None;
    let loud = |q: usize| {
        let e = fill_effects(b, q);
        e.acts && e.may_raise
    };
    let narrow = |q: usize| loud_only || !hashed_closer(b, q);
    let candidate = |q: usize| if narrow(q) { (hashed_closer(b, q) || tag_sets_closer(b, q)) && loud(q) } else { true };
    if set.iter().all(|&q| !candidate(q as usize)) {
        return Ok((keep, ArenaVec::new_in(b.arena)));
    }
    let fixed = crate::rerun::fixed_params(b.stmt, b.arena);
    let changed = changed_params(b, set, params);
    let init = |q: usize| b.graph.init_plans.iter().any(|i| i.pipeline as usize == q);
    let reads = |q: usize| b.closers.get(q).and_then(|c| c.as_plan()).is_none_or(|c| c.allParam.overlap_list(&changed));
    let produced = |q: usize| ps[q].desc.produces();
    // the pipeline whose rows reader `r` reads in slot `s`: the nearest producer before it
    let life = |r: usize, s: u8| (p0..r).rev().find(|&j| produced(j) == Some(s));
    for &q in set.iter().filter(|&&q| candidate(q as usize)) {
        let q = q as usize;
        let Some(agg) = b.closers[q].as_agg() else { continue };
        let input = agg.plan.lefttree;
        if input.and_then(|c| c.as_plan()).is_none_or(|c| c.allParam.overlap_list(&changed)) || agg.aggParams.overlap_list(&changed) {
            continue;
        }
        // another table form runs again at every re-open (PG's answer under a spill), unless its calls may raise
        if !matches!(ps[q].desc.ends_in, gravity_program::End::Breaker(br) if matches!(br.kind, BreakerKind::HashAgg | BreakerKind::Distinct)) {
            if { let e = fill_effects(b, q); e.acts && e.may_raise } {
                return Err(KEPT_HASHED_RAISES);
            }
            continue;
        }
        // the fill: `q` and the set members feeding it (a union's arms all feed one slot), up to the first reader before them
        let mut fill: ArenaVec<u16> = ArenaVec::new_in(b.arena); // alloc: Cold
        let mut todo: ArenaVec<(u8, usize)> = ArenaVec::new_in(b.arena); // alloc: Cold
        let mut ok = !sub_build(b, q) && !init(q);
        fill.push(q as u16);
        todo.extend(ps[q].desc.consumes.iter().map(|&s| (s, q)));
        while let Some((s, at)) = todo.pop() {
            let mut met = false;
            for j in (p0..at).rev() {
                if produced(j) == Some(s) {
                    met = true;
                    if !set.contains(&(j as u16)) || fill.contains(&(j as u16)) {
                        continue;
                    }
                    ok &= !reads(j) && !sub_build(b, j) && !init(j);
                    fill.push(j as u16);
                    todo.extend(ps[j].desc.consumes.iter().map(|&c| (c, j)));
                } else if met && ps[j].desc.consumes.contains(&s) {
                    break;
                }
            }
        }
        // a feeder's rows read only inside the fill (the table itself is read by its emit, a member: lowerv4 members_close)
        ok &= fill.iter().filter(|&&f| f as usize != q).all(|&f| {
            let s = produced(f as usize);
            (p0..ps.len()).filter(|&r| s.is_some_and(|s| ps[r].desc.consumes.contains(&s) && life(r, s) == Some(f as usize))).all(|r| fill.contains(&(r as u16)))
        });
        // D4104: a fill kept so reads no param but the set's and the statement's fixed ones (an enclosing owner's would go
        // stale in the kept table where PG rebuilds it); D4120: a generation set's fill may, refilled by that owner's set
        let mut reads_outer: ArenaVec<i32> = ArenaVec::new_in(b.arena); // alloc: Cold
        if narrow(q) {
            for &f in fill.iter() {
                let Some(c) = b.closers.get(f as usize).and_then(|c| c.as_plan()) else {
                    ok = false;
                    continue;
                };
                for x in c.allParam.iter().filter(|x| !fixed.contains(x) && !changed.contains(x)) {
                    ok &= loud_only;
                    if !reads_outer.contains(&x) {
                        reads_outer.push(x);
                    }
                }
            }
        }
        if !ok && { let e = fill_effects(b, q); e.acts && e.may_raise } {
            return Err(KEPT_HASHED_RAISES);
        }
        if ok {
            reads_outer.sort_unstable();
            if outer.as_ref().is_some_and(|o| o[..] != reads_outer[..]) {
                return Err(KEPT_HASHED_RAISES);
            }
            outer = Some(reads_outer);
            for f in fill {
                if !keep.contains(&f) {
                    keep.push(f);
                }
            }
        }
    }
    Ok((keep, outer.unwrap_or_else(|| ArenaVec::new_in(b.arena))))
}

/// PG's chgParam at a re-open of rerun set `set` whose owner changes `params`: those, and the setParams of each InitPlan
/// the set re-decides (ExecReScanSetParamPlan adds them to the node's chgParam).
pub(crate) fn changed_params(b: &Builder<'_, '_>, set: &[u16], params: &[i32]) -> ArenaVec<i32> {
    let mut changed: ArenaVec<i32> = ArenaVec::new_in(b.arena); // alloc: Cold
    changed.extend(params.iter().copied());
    for x in b.graph.init_plans.iter().filter(|x| x.link != crate::ops::shared::subplan::SubLink::Cte && set.contains(&x.pipeline)) {
        changed.extend(x.params.iter());
    }
    changed
}

/// D4120: rerun set `a` (its inner mapped since pipeline `p0`, a re-open changing `changed`) claims the params a kept fill
/// of a set nested in that inner reads (`graph.stale_reads`) that it changes: its every run marks that set stale
/// (`graph.refills`, exec `SetRearms::refills`), so the set fills its kept members again over the new params, as PG's
/// ExecReScanAgg rebuilds a kept table whose input's chgParam is set. A param no enclosing set claims stays recorded and
/// refuses the statement at the walk's end (`refills_left`).
pub(crate) fn claim_refills(b: &mut Builder<'_, '_>, p0: usize, a: u16, changed: &[i32]) {
    let g = &mut b.graph;
    let (reruns, refills) = (&g.reruns, &mut g.refills);
    let inside = |s: u16| s != a && reruns.get(s as usize).and_then(|r| r.pipelines.iter().min()).is_some_and(|&q| q as usize >= p0);
    g.stale_reads.retain(|&(s, x, _)| {
        let claim = inside(s) && changed.contains(&x);
        if claim && !refills.contains(&(a, s)) {
            refills.push((a, s));
        }
        !claim
    });
}

/// D4120: the refusal of a kept fill that reads a param no enclosing rerun set claimed (`claim_refills`): exec would keep
/// its table where PG rebuilds it.
pub(crate) fn refills_left(g: &crate::graph::PipelineGraph<'_>) -> Option<Refusal> {
    g.stale_reads.first().map(|&(_, _, node)| Refusal::shape(Kind::RecursiveUnion, node, KEPT_HASHED_RAISES))
}

/// D2711: the Shape of a hashed aggregate PG keeps across re-runs whose fill a rerun set cannot keep and whose VOLATILE calls
/// may raise: run again at every re-open, a call could raise where PG's kept table returns rows.
pub const KEPT_HASHED_RAISES: &str = "a rerun set's hashed aggregate PG keeps across re-runs whose fill the set cannot keep and whose VOLATILE calls may raise (run again, a call could raise where PG returns rows; D2711)";

/// Whether PG re-executes pipeline `q`'s closing node at a rescan that changes none of its params: ExecReScanAgg runs a
/// plain or sorted Agg again over its rescanned child, as ExecReScanUnique / Group / WindowAgg do. A hashed Agg keeps its
/// table, a Sort (a Limit's top-k too) rewinds, a Material, CteScan, Hash or hashed SetOp keeps its rows, and a hashed
/// SubPlan's set is built once (its last op the build).
pub(crate) fn reexecuted(b: &Builder<'_, '_>, q: usize) -> bool {
    use crate::ops::shared::agg::{AGG_PLAIN, AGG_SORTED};
    !sub_build(b, q) && b.closers.get(q).is_some_and(|c| match c.node_tag() {
        NodeTag::T_Agg => c.as_agg().is_some_and(|a| matches!(a.aggstrategy, AGG_PLAIN | AGG_SORTED)),
        NodeTag::T_Unique | NodeTag::T_Group | NodeTag::T_WindowAgg => true,
        _ => false,
    })
}

/// D4101 (S3 rkeep ra-hx2/n10-unique-nl0-vol): whether pipeline `q` closes at a Unique that only reads a Sort's run again
/// at a rescan changing none of its params: its child is a Sort the keep table keeps there (`shields_in`: the Sort has
/// EXEC_FLAG_REWIND, which a nest gives its inner only with no NestLoopParam, so ExecReScanSort rewinds). ExecReScanUnique
/// then re-reads the rewound run: nothing under the Sort runs again, and a Unique evaluates nothing but its equality
/// operators, so every rescan returns the first run's rows in the first run's order. The pipeline holds both nodes (the
/// Sort is its group's input, ops/agg `map_set`), so it is kept whole as a node PG keeps is, where `reexecuted` alone
/// would run its fill again and repeat a VOLATILE call PG makes once. The Unique's own code must show nothing: PG
/// compares at every rescan and only as far as its reader pulls, the kept set once and over the whole run, so an
/// equality operator that acts or may raise (user code) is not this. Nor is a sorted Agg, whose aggregates PG
/// evaluates again over the rewound run (its Sort is a pipeline of its own, kept by `kept_root`).
/// D1018: a Group (GROUP BY with no aggregate, a sorted plan) over such a Sort is read the
/// same way: ExecReScanGroup re-reads the rewound run and evaluates the Group's qual and target list again per group, so
/// with that code quiet (`own`: it acts nowhere and cannot raise, as the Unique's equality operators) every rescan
/// returns the first run's rows. A qual or target list that may raise or acts is not taken: D1018 still names it.
pub(crate) fn rereads_run(b: &Builder<'_, '_>, q: usize) -> bool {
    let Some(&c) = b.closers.get(q) else { return false };
    if !matches!(c.node_tag(), NodeTag::T_Unique | NodeTag::T_Group) || !c.as_plan().and_then(|p| p.lefttree).is_some_and(|s| s.node_tag() == NodeTag::T_Sort && lifetime::shields_in(b.stmt, &[], c, s)) {
        return false;
    }
    // the node alone, its child apart: its target list, its qual (a Group's) and its equality operators
    let own = crate::effects::Effects::of_found(of_rerun(b.stmt, b.facts, b.arena, c, &|_, _| true));
    !own.acts && !own.may_raise
}

/// Item 9 (b) part 2: whether kept member `q`, read by a re-executed one, ends in a breaker that reader may read again
/// without its refill: a Sort or a top-k (a rewound run) or a hashed group (a kept table).
pub(crate) fn keepable(b: &Builder<'_, '_>, q: usize) -> bool {
    use gravity_abi::breaker::BreakerKind;
    matches!(b.graph.pipelines[q].desc.ends_in, gravity_program::End::Breaker(br) if matches!(br.kind, BreakerKind::Sort | BreakerKind::TopK | BreakerKind::HashAgg | BreakerKind::Distinct))
}

/// What a pipeline does when its owner re-opens it, as its closing node's ExecReScan decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnRescan {
    /// Keep the state (a one-batch hash table, a Material, a rewound Sort with no changed parameter below it), for the
    /// reason `KeptBy` names.
    Keep(KeptBy),
    /// Drop and rebuild: the pipeline joins the owner's re-run set.
    Rebuild,
}

/// The keep rule's answer for a pipeline an owner's re-open does not run again: why its rows stay the ones PG's rescan
/// returns. Lowering asks it (lowerv4 lifetime `kept_input`) instead of a local test of the pipeline's shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeptBy {
    /// PG keeps the closing node's state at this re-open (the keep table's `Keep`: a rewound Sort or Material, a hashed
    /// SetOp's table, a Memoize's cache, a CteScan's store). `marks`: its match flags are cleared first.
    Pg { marks: bool },
    /// PG keeps it unless a run-time fact the plan does not show holds (`why`: a multi-batch hash table, a spilled hashed
    /// aggregate), and then rebuilds it; kept as PG usually does, over a quiet fill or at a site that checks the fact.
    Usual { marks: bool, why: &'static str },
    /// PG runs the closing node again (a plain or sorted aggregate, a Sort without REWIND), but nothing in its fill acts
    /// and a run returns the same rows, so the rows kept are the rows PG's re-run returns (`Site::keep_rerun`).
    Quiet,
    /// PG's rescan does not reach the closing node: it sits under a node PG keeps, or in another tree (an InitPlan's, a
    /// CTE's, a hashed SubPlan's), run again only through its own params.
    Unreached,
    /// The pipeline has no closing plan node (its rows go to a later op of the same node).
    Open,
}

/// One answer of the keep rule (`KeptBy`) for pipeline `pipeline` at a re-open of owner (`node_id`, `plan_id`, as
/// `graph.reruns` names it), recorded for every pipeline mapped under the owner's inner that its rerun set does not run
/// again (`graph.kept_by`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeptRec {
    pub node_id: i32,
    pub plan_id: i32,
    pub pipeline: u16,
    pub by: KeptBy,
}

/// Pipeline `i`'s answer to a re-open of the owner's inner `inner` that changes `changed`: `Rebuild` when its closing
/// node's `allParam` meets them (a node PG keeps under such a param, a hashed aggregate's HAVING, is `kept_hashed`'s road
/// inside the set), else the lifetime home's keep table for the closer where PG's rescan of `inner` reaches it (`keep_at`).
/// A closer it does not reach keeps its state: one in another tree (an InitPlan's, a CTE's, a hashed SubPlan's build) or
/// under a node PG keeps (a Sort under a kept hash table is never re-sorted). A hash build's one-batch fact is checked at
/// run time by the hash join family (probe_kept.rs), so its table is kept as PG usually keeps it. A hashed aggregate PG
/// keeps while it never spilled (the keep table's SPILLED over a fill that acts) joins the set: PG's spill moves only
/// how often the fill's VOLATILE calls run, and that count is free (at least one call per row whose value is used;
/// keeping or rebuilding is a speed choice). The set runs the fill at its first run, never before
/// PG would. A one-table fill in a nest's or SubPlan's set (`keeps`, `declare_rerun`) is kept there for the later runs
/// (`kept_hashed`, D2711), and so is (D4104) a GROUPING SETS table's tag form there or any fill of a recursive union's
/// generation set (`keeps` too, setop.rs `map_recursive_union`) whose fill may raise; any other runs again at every
/// re-open, which only a fill that cannot raise may do: a VOLATILE call run again could raise where PG's kept table
/// returns rows (`fill_effects`), so such a fill refuses by SPILLED.
pub fn on_rescan<'p>(b: &Builder<'p, '_>, i: usize, changed: &ParamSet<'_>, inner: Option<Node<'p>>, keeps: bool) -> Result<OnRescan, &'static str> {
    let Some(&c) = b.closers.get(i) else { return Ok(OnRescan::Keep(KeptBy::Open)) };
    if c.as_plan().is_some_and(|p| p.allParam.overlap_list(changed.0)) || union_seed(b, c, changed.0) {
        return Ok(OnRescan::Rebuild);
    }
    match keep_answer(b, c, changed.0, inner, lifetime::Site { keep_rerun: true, checks_unseen: c.node_tag() == NodeTag::T_Hash }) {
        None => Ok(OnRescan::Keep(KeptBy::Unreached)),
        Some((lifetime::Keep::Keep { .. }, lifetime::Kept::Keep { marks })) => Ok(OnRescan::Keep(KeptBy::Pg { marks })),
        Some((lifetime::Keep::Unseen { why, .. }, lifetime::Kept::Keep { marks })) => Ok(OnRescan::Keep(KeptBy::Usual { marks, why })),
        Some((_, lifetime::Kept::Keep { .. })) => Ok(OnRescan::Keep(KeptBy::Quiet)),
        Some((_, lifetime::Kept::Rebuild)) => Ok(OnRescan::Rebuild),
        // the keep table's SPILLED over an acting fill: the only refusal a hashed Agg's rule gives (D2711); D4104: a
        // GROUPING SETS table's tag form too, which the set keeps where its fill may raise (`kept_hashed`)
        Some((_, lifetime::Kept::Refuse(_))) if hashed_agg(b, i) && (keeps && (hashed_closer(b, i) || tag_sets_closer(b, i)) || { let e = fill_effects(b, i); !(e.acts && e.may_raise) }) => Ok(OnRescan::Rebuild),
        Some((_, lifetime::Kept::Refuse(why))) => Err(why),
    }
}

/// Whether closer `c` is a recursive union's seed (its outer plan) whose union reads a param in `changed`: PG's
/// ExecReScanRecursiveUnion re-runs the seed at every rescan of the union (ExecReScan(outerPlan) when the seed's own
/// chgParam is NULL), so a seed that reads no changed param runs again when its union's term does (an enclosing union's
/// WorkTableScan, a nest's or a SubPlan's param), refilling the working table the union's driver swapped away.
fn union_seed<'p>(b: &Builder<'p, '_>, c: Node<'p>, changed: &[i32]) -> bool {
    let Some(id) = c.as_plan().map(|p| p.plan_node_id) else { return false };
    let Some((_, Some(up))) = lifetime::node_of(b.stmt, id) else { return false };
    up.as_recursive_union().is_some_and(|r| r.plan.lefttree.and_then(|l| l.as_plan()).is_some_and(|l| l.plan_node_id == id) && r.plan.allParam.overlap_list(changed))
}

/// The keep table's answer for closer `c` at a rescan of `inner` changing `changed` (its PG flags and parent as the rescan
/// reaches it), resolved against the effects of the part of its subtree PG runs again (down to the nodes that keep their
/// state): what PG keeps stays kept, what PG re-runs is kept only over a quiet part, and a node with no rule, or whose
/// answer turns on a fact the plan does not show over an acting part, refuses by the table's name. None: the rescan does
/// not reach `c`.
pub(crate) fn keep_at<'p>(b: &Builder<'p, '_>, c: Node<'p>, changed: &[i32], inner: Option<Node<'p>>, site: lifetime::Site) -> Option<lifetime::Kept> {
    keep_answer(b, c, changed, inner, site).map(|(_, kept)| kept)
}

/// `keep_at` with the keep table's own answer beside the resolved one: why a kept closer is kept (`KeptBy`).
fn keep_answer<'p>(b: &Builder<'p, '_>, c: Node<'p>, changed: &[i32], inner: Option<Node<'p>>, site: lifetime::Site) -> Option<(lifetime::Keep, lifetime::Kept)> {
    let id = c.as_plan()?.plan_node_id;
    let lifetime::Reach::At { eflags, parent } = lifetime::reach(b.stmt, inner?, changed, id) else { return None };
    let at = lifetime::Rescan { stmt: Some(b.stmt), changed, eflags, parent };
    let cut = |up: Node<'p>, k: Node<'p>| lifetime::shields_in(b.stmt, changed, up, k);
    let fill = crate::effects::Effects::of_found(of_rerun(b.stmt, b.facts, b.arena, c, &cut));
    let rule = lifetime::keep_rule(c, &at);
    Some((rule, lifetime::resolve(rule, fill, site)))
}

/// D2711: whether pipeline `q` closes at a hashed Agg (AGG_HASHED, grouping sets or not), whose keep rule's only refusal is
/// SPILLED: `on_rescan` rebuilds it.
fn hashed_agg(b: &Builder<'_, '_>, q: usize) -> bool {
    b.closers.get(q).and_then(|c| c.as_agg()).is_some_and(|a| a.aggstrategy == crate::ops::shared::agg::AGG_HASHED)
}

/// The pipelines since `p0` PG re-executes when `params` change (chgParam, D4): each that answers `Rebuild`
/// (`on_rescan`), reads a slot a set pipeline filled last, or refills a slot the set uses; with the slots they fill or
/// read. The rest keep their rows (a one-batch Hash, a Material / Sort over input with no chgParam). Shared by a nest's or
/// nested SubPlan's rerun set and a recursive union's generation (`setops::IterDesc::reruns`, D241).
/// With the set, the keep rule's answer (`KeptRec`) for every pipeline since `p0` the set does not run again, and for the
/// pipelines of a CTE mapped before `p0` that a CteScan in the owner's inner reads (`kept_ctes`): what the declarations
/// record in `graph.kept_by` for lowering to ask.
pub fn rerun_set(b: &Builder<'_, '_>, p0: usize, params: &[i32], k: Kind, owner: impl Into<Owner>) -> Result<(ArenaVec<u16>, SlotSet, ArenaVec<KeptRec>), Refusal> {
    let owner = owner.into();
    let mut kept: ArenaVec<KeptRec> = ArenaVec::new_in(b.arena); // alloc: Cold
    let (set, slots) = rerun_set_in(b, p0, params, k, owner, &mut kept)?;
    kept_ctes(b, p0, params, owner, &mut kept);
    kept.retain(|r| !set.contains(&r.pipeline));
    Ok((set, slots, kept))
}

/// A CTE whose plan was mapped before the owner's inner (a separate tree, run once and pulled) and that a CteScan inside
/// the inner reads: PG's rescan of that CteScan keeps the CTE's store unless a changed param reaches the CTE plan
/// (ExecReScanCteScan), so the CTE plan's pipelines answer as its CteScans do: `Pg` when every CteScan the rescan reaches
/// keeps the store, `Unreached` when none is reached. Any other answer records nothing (not known to be kept).
fn kept_ctes(b: &Builder<'_, '_>, p0: usize, params: &[i32], owner: Owner, kept: &mut ArenaVec<KeptRec>) {
    let Some(inner) = owner_inner(b, owner.node_id, owner.plan_id) else { return };
    for i in b.graph.init_plans.iter().filter(|i| i.link == crate::ops::shared::subplan::SubLink::Cte && (i.pipeline as usize) < p0) {
        let (mut by, mut known) = (None, true);
        lifetime::cte_scans(inner, i.plan_id, &mut |cs| {
            let id = cs.as_plan().map_or(-1, |p| p.plan_node_id);
            match lifetime::reach(b.stmt, inner, params, id) {
                lifetime::Reach::Outside => {}
                lifetime::Reach::Shielded => by = by.or(Some(KeptBy::Unreached)),
                lifetime::Reach::At { eflags, parent } => match lifetime::keep_rule(cs, &lifetime::Rescan { stmt: Some(b.stmt), changed: params, eflags, parent }) {
                    lifetime::Keep::Keep { marks } => by = Some(KeptBy::Pg { marks }),
                    _ => known = false,
                },
            }
        });
        let (true, Some(by), Some(tree)) = (known, by, crate::rerun::subplan_root(b.stmt, i.plan_id)) else { continue };
        for q in 0..p0.min(b.graph.pipelines.len()) {
            if b.graph.pipelines[q].node_ops.iter().any(|n| lifetime::eflags_at(tree, 0, n.node_id).is_some()) {
                kept.push(KeptRec { node_id: owner.node_id, plan_id: owner.plan_id, pipeline: q as u16, by }); // alloc: Cold
            }
        }
    }
}

fn rerun_set_in(b: &Builder<'_, '_>, p0: usize, params: &[i32], k: Kind, owner: Owner, kept: &mut ArenaVec<KeptRec>) -> Result<(ArenaVec<u16>, SlotSet), Refusal> {
    let Owner { node_id, plan_id } = owner;
    let inner = owner_inner(b, node_id, plan_id);
    let n = b.graph.pipelines.len();
    let changed = ParamSet::of(params);
    let mut set: ArenaVec<u16> = ArenaVec::new_in(b.arena); // alloc: Cold
    let (mut slots, mut foreign): (SlotSet, SlotSet) = ([0; 4], [0; 4]);
    for i in p0..n {
        let answer = on_rescan(b, i, &changed, inner, matches!(k, Kind::NestLoop | Kind::SubPlan | Kind::RecursiveUnion)).map_err(|why| Refusal::shape(k, node_id, why))?;
        let rebuild = answer == OnRescan::Rebuild;
        let fed = || b.graph.pipelines[i].desc.consumes.iter().any(|s| (p0..i).rev().find(|&j| b.graph.pipelines[j].desc.produces() == Some(*s)).is_some_and(|j| set.contains(&(j as u16))));
        let d = b.graph.pipelines[i].desc;
        // D1262 (PG's ExecReScanSetParamPlan): an InitPlan the set re-decides adds its setParams to its parent's chgParam
        let init = || b.closers.get(i).and_then(|c| c.as_plan()).is_some_and(|c| b.graph.init_plans.iter().any(|x| x.link != crate::ops::shared::subplan::SubLink::Cte && set.contains(&x.pipeline) && x.params.iter().any(|id| c.allParam.is_member(id))));
        // a pipeline refilling a set slot reruns too: built once, the set would refill it under its reader
        let rerun = rebuild || fed() || init() || d.produces().is_some_and(|s| slot_has(&slots, s));
        if d.produces().is_some_and(|s| slot_has(&foreign, s)) {
            return Err(Refusal::shape(k, node_id, "a slot a rerun set reads refilled by a later pipeline"));
        }
        if rerun {
            for s in d.consumes.iter().chain(d.produces().iter()) {
                slot_set(&mut slots, *s);
            }
            for s in d.consumes.iter().filter(|s| (p0..i).rev().find(|&j| b.graph.pipelines[j].desc.produces() == Some(**s)).is_none_or(|j| !set.contains(&(j as u16)))) {
                slot_set(&mut foreign, *s);
            }
            set.push(i as u16);
        } else if let OnRescan::Keep(by) = answer {
            kept.push(KeptRec { node_id, plan_id, pipeline: i as u16, by }); // alloc: Cold
        }
    }
    regather(b, p0, &mut set, &mut slots).map_err(|why| Refusal::shape(k, node_id, why))?;
    Ok((set, slots))
}

/// D2117: the pipelines nested SubPlan `plan_id` (at node `node_id`, its plan mapped since pipeline `p0`, its embedded
/// inner `inner`) reads, through its own rerun set's members, that no rerun set runs again run once, at the statement's
/// start, where PostgreSQL 18.6 runs them at the SubPlan's first evaluation (its plan's first scan: a rescan with no changed
/// param below them re-executes a plain or sorted Agg over the same input, or keeps a Sort, a hashed Agg or SetOp), so
/// never under an empty outer or a CASE branch no row reaches. A fill whose code may raise and acts on nothing (the effects
/// home's `of_plan` over its closer) gives the same rows and the same first error at every evaluation, so it joins the
/// SubPlan's set as a member PG keeps (`graph.kept`: run at the set's first run, the first evaluation; lowerv4
/// `RERUN_KEPT`), with every pipeline since `p0` it reads; the SubPlan with no set gets one of these kept members alone.
/// A fill that acts stays as it is (D1337 / D2624: PG's re-execution repeats its calls), and so does a quiet one (no
/// evaluation of it shows; but D4031: one the inner reads as its own source whose emit lowering re-reads only as a set
/// member joins, `member_reread`), a hash build (hashjoin's lazy road), a hashed SubPlan's build, a Material (the spool
/// road), an InitPlan's or a CTE's pipeline and a pipeline another set holds: lowerv4 answers each of them as before.
pub(crate) fn sub_kept_fills(b: &Builder<'_, '_>, p0: usize, node_id: i32, plan_id: i32, inner: &crate::graph::PipelineNode<'_>) -> ArenaVec<u16> {
    use gravity_abi::breaker::BreakerKind;
    let n = b.graph.pipelines.len();
    let ps = &b.graph.pipelines;
    let own = b.graph.reruns.iter().position(|r| r.node_id == node_id && r.plan_id == plan_id);
    let held = |q: usize| b.graph.reruns.iter().any(|r| r.pipelines.contains(&(q as u16)));
    let init = |q: usize| b.graph.init_plans.iter().any(|i| i.pipeline as usize == q);
    let produces = |q: usize, s: u8| ps[q].desc.produces() == Some(s);
    // every pipeline since `p0` filling slot `s` for a reader at `at` (a union's arms all feed one slot), up to the first
    // reader before them (an earlier life of the slot number)
    let fills = |s: u8, at: usize| -> ArenaVec<usize> {
        let mut out: ArenaVec<usize> = ArenaVec::new_in(b.arena); // alloc: Cold
        for q in (p0..at.min(n)).rev() {
            if produces(q, s) {
                out.push(q);
            } else if !out.is_empty() && ps[q].desc.consumes.contains(&s) {
                break;
            }
        }
        out
    };
    let src = |p: &crate::graph::PipelineNode<'_>| if let crate::graph::Source::Breaker { slot } = p.source { Some(slot) } else { None };
    let mut reads: ArenaVec<(u8, usize)> = ArenaVec::new_in(b.arena); // alloc: Cold
    reads.extend(inner.desc.consumes.iter().copied().chain(src(inner)).map(|s| (s, n)));
    if let Some(i) = own {
        for &q in b.graph.reruns[i].pipelines.iter() {
            reads.extend(ps[q as usize].desc.consumes.iter().copied().chain(src(&ps[q as usize])).map(|s| (s, q as usize)));
        }
    }
    let cx = crate::effects::Cx::new(b.stmt, b.facts, b.arena);
    let mut keep: ArenaVec<u16> = ArenaVec::new_in(b.arena); // alloc: Cold
    for &(s, at) in reads.iter() {
        for q in fills(s, at) {
            if held(q) || keep.contains(&(q as u16)) {
                continue;
            }
            let end = match ps[q].desc.ends_in { gravity_program::End::Breaker(br) => Some(br.kind), _ => None };
            if init(q) || sub_build(b, q) || matches!(end, None | Some(BreakerKind::HashBuild | BreakerKind::Materialize)) {
                continue;
            }
            let Some(&closer) = b.closers.get(q) else { return ArenaVec::new_in(b.arena) }; // alloc: Cold
            let e = crate::effects::of_plan(&cx, closer);
            if e.acts {
                return ArenaVec::new_in(b.arena); // alloc: Cold
            }
            if !e.may_raise && !(at == n && member_reread(b, q, end)) {
                continue;
            }
            // the fill: `q` and every pipeline since `p0` it reads, none another set's or an InitPlan's
            let mut todo: ArenaVec<usize> = ArenaVec::new_in(b.arena); // alloc: Cold
            todo.push(q);
            while let Some(x) = todo.pop() {
                if keep.contains(&(x as u16)) {
                    continue;
                }
                if held(x) || init(x) || sub_build(b, x) {
                    return ArenaVec::new_in(b.arena); // alloc: Cold
                }
                keep.push(x as u16);
                for c in ps[x].desc.consumes.iter().copied().chain(src(&ps[x])) {
                    todo.extend(fills(c, x));
                }
            }
        }
    }
    keep
}

/// D4031 (c-random rnd-0234 / rnd-0281: a correlated SubPlan whose WHERE folded to 'One-Time Filter: false'; any
/// SubPlan whose parameters sit above its top-N or aggregate): a quiet fill (it acts on nothing and may not raise) that
/// the SubPlan's embedded inner reads as its own source, ending in an emit lowering re-reads at a re-open only as a
/// member of the SubPlan's set (lowerv4 rerun/sets.rs `emit_ok`): a top-N's run, or a plain aggregate's one record of
/// plain aggregates (`plain_finals`: an ORDER BY, DISTINCT or ordered-set aggregate folds at the drain). The fill reads
/// none of the SubPlan's parameters, so PostgreSQL 18.6 makes the same rows at every call (ExecReScanSort sorts the same
/// input again with no EXEC_FLAG_REWIND, ExecReScanAgg aggregates it again), and lowering's once road takes a row
/// sort's run only (rerun/inner_once.rs `kept_run`): such a fill joins the set as a member PG keeps, run at the first
/// evaluation and read again at every later one, as a fill that may raise does.
fn member_reread(b: &Builder<'_, '_>, q: usize, end: Option<gravity_abi::breaker::BreakerKind>) -> bool {
    use gravity_abi::breaker::BreakerKind;
    match end {
        Some(BreakerKind::TopK) => true,
        Some(BreakerKind::PlainAgg) => crate::rerun::plain_finals(b, q),
        _ => false,
    }
}

/// D2624's Shape: an acting re-executed fill a set reads that cannot join it whole.
pub const REEXEC_ACTS: &str = "a rerun set reading a plain, sorted or mixed aggregate (or a Unique / Group / WindowAgg) that reads no changed param and whose input or aggregates call a VOLATILE function, fed by another pipeline or read outside the set (PG re-executes it at every rescan: ExecReScanAgg; D2624)";

/// D2624 (P1; PG 18.6's ExecReScanAgg re-aggregates a plain, sorted or mixed Agg at every rescan over its rescanned child,
/// as ExecReScanUnique / Group / WindowAgg re-run theirs; only AGG_HASHED keeps its table): a pipeline `rerun_set` keeps
/// (its closer reads no changed param) whose closer PG re-executes and whose code acts (a VOLATILE call in its input or
/// aggregates: the effects home's `acts`), read by a set member, was aggregated once where PG aggregates per rescan
/// (vlog called 8 times where PG calls it 24). It joins the set with every pipeline filling the same life of its slot (a
/// union's arms under one aggregate), and the slot: they re-run before the reader at every run, as PG's rescan
/// re-executes them. A quiet one stays kept (the same rows either way). One fed by another pipeline (a grouping-sets chain,
/// an input breaker), a fill PG keeps beside it, or rows a pipeline outside the set also reads refuse by name
/// (REEXEC_ACTS). An InitPlan's pipeline (run once) and a hashed SubPlan's build (its own road) stay out.
fn regather(b: &Builder<'_, '_>, p0: usize, set: &mut ArenaVec<u16>, slots: &mut SlotSet) -> Result<(), &'static str> {
    use crate::ops::shared::agg::{AGG_MIXED, AGG_PLAIN, AGG_SORTED};
    let ps = &b.graph.pipelines;
    let n = ps.len();
    if set.is_empty() {
        return Ok(());
    }
    let cx = crate::effects::Cx::new(b.stmt, b.facts, b.arena);
    let init = |q: usize| b.graph.init_plans.iter().any(|i| i.pipeline as usize == q);
    let reexec = |q: usize| {
        !sub_build(b, q) && !init(q) && b.closers.get(q).is_some_and(|c| match c.node_tag() {
            NodeTag::T_Agg => c.as_agg().is_some_and(|a| matches!(a.aggstrategy, AGG_PLAIN | AGG_SORTED | AGG_MIXED)),
            NodeTag::T_Unique | NodeTag::T_Group | NodeTag::T_WindowAgg => true,
            _ => false,
        })
    };
    let produces = |j: usize| ps[j].desc.produces();
    let reads = |j: usize, s: u8| ps[j].desc.consumes.contains(&s);
    let acts = |j: usize| b.closers.get(j).is_some_and(|c| crate::effects::of_plan(&cx, *c).acts);
    let mut added = false;
    for q in p0..n {
        let Some(s) = produces(q) else { continue };
        // at a life's first producer only (a union's arms all fill one slot): no producer of `s` since its last reader
        if (p0..q).rev().take_while(|&j| !reads(j, s)).any(|j| produces(j) == Some(s)) {
            continue;
        }
        let Some(r0) = (q + 1..n).find(|&j| reads(j, s)) else { continue };
        let fills = || (q..r0).filter(|&j| produces(j) == Some(s));
        let end = (r0..n).find(|&j| produces(j) == Some(s)).unwrap_or(n);
        let readers = || (r0..end).filter(|&j| reads(j, s));
        let out = |j: usize| !set.contains(&(j as u16));
        if !fills().any(out) || !readers().any(|j| !out(j)) || !fills().any(|j| out(j) && reexec(j)) || !fills().any(acts) {
            continue;
        }
        if fills().any(|j| out(j) && (!reexec(j) || !ps[j].desc.consumes.is_empty())) || readers().any(out) {
            return Err(REEXEC_ACTS);
        }
        for j in q..r0 {
            if produces(j) == Some(s) && !set.contains(&(j as u16)) {
                set.push(j as u16); // alloc: Cold
            }
        }
        slot_set(slots, s);
        added = true;
    }
    if added {
        set.sort_unstable();
    }
    Ok(())
}

/// F3 (fail closed): a slot a member reads from a producer outside the set (run once, kept for the statement), other
/// than the union's own tables, is a Hash build, a plain aggregate or a Sort, kept whole across generations. A Sort
/// that spilled is kept too (setop P6, the Merge Join's once-run Sort of PG's plan): the driver keeps a rerun set's
/// kept slot's stream for the statement (D1638, `free_after`), so each generation's merge re-reads its put runs, and a
/// merge's first pass drops a merge before's passes (D1327), as PG's ExecReScanSort rewinds a sort whose input did not
/// change. with#17: a hashed grouping with no aggregate (a UNION's or a DISTINCT's hash table) is kept too: PG's
/// ExecReScanAgg re-walks its table when no parameter changed, and its emit gathers the table's records again at each
/// member run (the gather's pointer array is the call's scratch, released with each run: no growth per generation).
/// D3373 (H-1): so is one hashed Agg's grouping with aggregates (AGG_HASHED, no grouping set, one pass): PG re-walks
/// the kept table and finalizes each group again at each generation over the kept transition states, as the emit takes
/// each final again from the kept states (a VOLATILE HAVING call runs once per group per generation in both; 3000
/// generations over 5003 groups with array_agg / string_agg finals peak 6 % over the same term with no aggregate, at
/// W0). A fill that acts keeps lifetime's spill refusal (PG rebuilds a table that spilled). with#170 / #171: a CTE's
/// store (`Builder::ctes`) is kept too (`cte_once`): a CTE no rerun set re-runs fills it once, its pull only appends,
/// and each generation's reader reads it again from its first row, as PG's CteScan rewinds its read pointer (lowerv4
/// ops/ctescan/lower.rs `kept_whole` reads it so for a nest's or a SubPlan's set).
/// The rerun home's answer for recursive union term set `reruns` (`map_recursive_union`, its working table `work`): `true`
/// when a member reads such an input that is not kept whole (the union refuses by F3's name).
pub(crate) fn term_unwhole(b: &Builder<'_, '_>, reruns: &[u16], work: u8) -> bool {
    use crate::effects::setop::quiet;
    use crate::graph::Source;
    use gravity_abi::breaker::BreakerKind;
    use gravity_program::End;
    let rerun = |p: usize| reruns.iter().any(|&r| r as usize == p);
    let hashed_group = |p: usize| b.graph.groups.iter().any(|g| g.producer.0 as usize == p && (g.aggs.is_empty() || g.set.is_none() && g.two_pass.is_none() && b.closers.get(p).and_then(|c| c.as_agg()).is_some_and(|a| a.aggstrategy == crate::ops::shared::agg::AGG_HASHED)));
    let cte_once = |p: usize| b.graph.pipelines[p].desc.produces().is_some_and(|s| b.ctes.iter().any(|&(_, c)| c == s)) && !b.graph.reruns.iter().any(|r| r.pipelines.contains(&(p as u16)));
    let producer = |m: usize, s: u8| (0..m).rev().find(|&p| b.graph.pipelines[p].desc.produces() == Some(s));
    // D3373 (W3): a Materialize whose fill reads a Sort's or a top-k's run is kept whole too: filled once, its store (spilled
    // runs included, D1638) is read again from its first row at each generation, as PG's ExecReScanMaterial rewinds a store
    // whose child no parameter changed (probed equal at W0 + W8: a top-k, a 2 MB store under a 1 MB grant read over 30
    // generations, a LIMIT, a cursor, a semi join). D3373 (H-2): so is one whose fill reads a hashed group's emit (a nest
    // loop's inner over a DISTINCT or a hashed aggregate), where the fill is quiet: lowerv4 runs that fill in pipeline order
    // ahead of the union (rerun/spool/lazy.rs), where PG fills the store at the term's first inner fetch, unseen for a fill
    // that neither acts nor raises
    let group_fill = |p: usize| matches!(b.graph.pipelines[p].source, Source::Breaker { slot } if producer(p, slot).is_some_and(|q| matches!(b.graph.pipelines[q].desc.ends_in, End::Breaker(br) if br.kind == BreakerKind::HashAgg) && hashed_group(q))) && b.closers.get(p).copied().is_some_and(|n| quiet(b, n));
    let sorted_fill = |p: usize| matches!(b.graph.pipelines[p].source, Source::Breaker { slot } if producer(p, slot).is_some_and(|q| matches!(b.graph.pipelines[q].desc.ends_in, End::Breaker(br) if matches!(br.kind, BreakerKind::Sort | BreakerKind::TopK))));
    // D3373 (G-3): a hashed SubPlan's table (its Distinct build, rerun.rs `sub_build`) is built once by its own road and
    // only probed, row by row, by the SubPlan's test (ANY / NOT IN): PG builds it once (ExecHashSubPlan rebuilds only on a
    // changed parameter, and a hashed SubPlan has none) and probes it at every generation, as the lowering's kept build is
    let kept = |p: usize| matches!(b.graph.pipelines[p].desc.ends_in, End::Breaker(br) if matches!(br.kind, BreakerKind::HashBuild | BreakerKind::PlainAgg | BreakerKind::Sort) || br.kind == BreakerKind::HashAgg && hashed_group(p) || br.kind == BreakerKind::Materialize && (sorted_fill(p) || group_fill(p)) || br.kind == BreakerKind::Distinct && crate::rerun::sub_build(b, p));
    // an enclosing union's working table (its WorkTableScan in this term) is swapped only between that union's own
    // generations, so it holds still across every generation of this one, as PG's working table under a WorkTableScan
    let outer_work = |s: u8| b.work.iter().any(|&(_, w)| w == s);
    reruns.iter().map(|&m| m as usize).any(|m| {
        let pl = &b.graph.pipelines[m];
        let src = if let Source::Breaker { slot } = pl.source { Some(slot) } else { None };
        pl.desc.consumes.iter().copied().chain(src).filter(|&s| s != work && !outer_work(s)).filter_map(|s| producer(m, s)).any(|p| !rerun(p) && !kept(p) && !cte_once(p))
    })
}
