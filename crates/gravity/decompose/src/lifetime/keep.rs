//! The keep table: PostgreSQL 18.6's ExecReScan answer per plan node (nodeHashjoin.c, nodeAgg.c, nodeSetOp.c, nodeSort.c,
//! nodeIncrementalSort.c, nodeMaterial.c, nodeMemoize.c, nodeCtescan.c, nodeFunctionscan.c, nodeTableFuncscan.c), and
//! the executor flags each ExecInit* passes to its children, which decide a Sort's randomAccess and a Material's store.
use gravity_program::{ArenaRef, ArenaVec};
use types_nodes::plannodes::PlannedStmt;
use types_nodes::{JoinType, Node, NodeList, NodeTag};

use crate::effects::Effects;
use crate::ops::shared::agg::{AGG_HASHED, AGG_MIXED, AGG_PLAIN, AGG_SORTED};

/// PG's executor flags (executor.h EXEC_FLAG_*): what a node's parent asks of its rescans.
pub const REWIND: u8 = 0x04;
pub const BACKWARD: u8 = 0x08;
pub const MARK: u8 = 0x10;

const SETOP_HASHED: u32 = 1;

const MULTI_BATCH: &str = "lifetime: a kept hash table whose fill acts (PG rebuilds a multi-batch table, which the plan does not show)";
const SPILLED: &str = "lifetime: a kept hashed aggregate whose fill acts (PG rebuilds a table that spilled, which the plan does not show)";
const BOUND: &str = "lifetime: a top-k bound read from a changed param over a fill that acts (PG re-sorts only when the bound's value changes)";
const NO_JOIN: &str = "lifetime: a Hash with no HashJoin above it";
const NO_CTE: &str = "lifetime: a CteScan whose CTE plan the statement does not hold";
const PART_FN: &str = "lifetime: a FunctionScan whose rescan recomputes some functions and rewinds the others over a fill that acts (PG recomputes only the functions a changed param meets)";
const NEVER: &str = "lifetime: a ModifyTable, which PG never rescans";
const NO_RULE: &str = "lifetime: a plan node the keep table has no rule for";

/// What a rescan of one node changes, as PG's ExecReScan sees it there.
#[derive(Clone, Copy)]
pub struct Rescan<'a, 'p> {
    /// The statement: a CteScan's CTE plan is one of its subplans.
    pub stmt: Option<&'p PlannedStmt<'p>>,
    /// The params the owner's re-open changes. PG adds each owner param to chgParam whatever its value; a node's own
    /// chgParam is the part its `allParam` holds.
    pub changed: &'a [i32],
    /// The executor flags the node was initialized with (`eflags_at`).
    pub eflags: u8,
    /// The node's parent: a Hash's HashJoin (its join type decides the match flags), a Sort's Limit (its bound).
    pub parent: Option<Node<'p>>,
}

/// PG's answer when the run-time fact an `Unseen` names is false.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Usual {
    Keep,
    Rebuild,
}

/// PG's answer for one node at a rescan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keep {
    /// The state survives (a rewound store, a kept table); `marks`: the table's match flags are cleared first.
    Keep { marks: bool },
    /// A changed param reaches what the node holds: it is dropped and the node runs again.
    Rebuild,
    /// PG runs the node again though no changed param reaches it (a re-executing node, a Sort without randomAccess, an
    /// IncrementalSort): the same rows when its fill is quiet, so a site may keep it then (`Site::keep_rerun`).
    Rerun,
    /// PG's choice turns on a run-time fact the plan does not show (`why`); `usual` is its answer when the fact is false.
    /// Both answers return the same rows only over a quiet fill.
    Unseen { usual: Usual, marks: bool, why: &'static str },
    /// No rule: refused by this name.
    Refuse(&'static str),
}

/// A site's decision: PG's answer resolved against the effects of the code that fills the state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kept {
    Keep { marks: bool },
    Rebuild,
    Refuse(&'static str),
}

/// PG's answer for node `n` at a rescan `at` describes.
pub fn keep_rule<'p>(n: Node<'p>, at: &Rescan<'_, 'p>) -> Keep {
    crate::walk::stack_check(); // D3748
    let Some(p) = n.as_plan() else { return Keep::Refuse(NO_RULE) };
    let reads = |x: Option<Node<'p>>| x.and_then(|x| x.as_plan()).is_some_and(|c| c.allParam.overlap_list(at.changed));
    let (own, outer, inner) = (p.allParam.overlap_list(at.changed), reads(p.lefttree), reads(p.righttree));
    let again = if own { Keep::Rebuild } else { Keep::Rerun };
    let kept = Keep::Keep { marks: false };
    match n.node_tag() {
        NodeTag::T_Hash => match at.parent.and_then(|j| j.as_hash_join()) {
            None => Keep::Refuse(NO_JOIN),
            Some(_) if own => Keep::Rebuild,
            Some(j) => Keep::Unseen { usual: Usual::Keep, marks: fills_inner(j.join.jointype), why: MULTI_BATCH },
        },
        NodeTag::T_Agg => match n.as_agg().map_or(u32::MAX, |a| a.aggstrategy) {
            AGG_HASHED if outer || n.as_agg().is_some_and(|a| a.aggParams.overlap_list(at.changed)) => Keep::Rebuild,
            AGG_HASHED => Keep::Unseen { usual: Usual::Keep, marks: false, why: SPILLED },
            AGG_PLAIN | AGG_SORTED | AGG_MIXED => again,
            _ => Keep::Refuse(NO_RULE),
        },
        NodeTag::T_SetOp if n.as_set_op().is_some_and(|s| s.strategy == SETOP_HASHED) => {
            if outer || inner {
                Keep::Rebuild
            } else {
                kept
            }
        }
        NodeTag::T_Sort if outer => Keep::Rebuild,
        NodeTag::T_Sort if at.eflags & (REWIND | BACKWARD | MARK) == 0 => Keep::Rerun,
        NodeTag::T_Sort if bound_reads(at) => Keep::Unseen { usual: Usual::Rebuild, marks: false, why: BOUND },
        NodeTag::T_Sort => kept,
        NodeTag::T_IncrementalSort if outer => Keep::Rebuild,
        NodeTag::T_IncrementalSort => Keep::Rerun,
        NodeTag::T_Material if outer => Keep::Rebuild,
        NodeTag::T_Material if at.eflags & (REWIND | BACKWARD) == 0 => Keep::Rerun,
        NodeTag::T_Material => kept,
        NodeTag::T_Memoize => {
            let keys = n.as_memoize().map(|m| &m.keyparamids);
            let Some(c) = p.lefttree.and_then(|c| c.as_plan()) else { return Keep::Refuse(NO_RULE) };
            let nonkey = c.allParam.iter().any(|x| at.changed.contains(&x) && !keys.is_some_and(|k| k.is_member(x)));
            if nonkey {
                Keep::Rebuild
            } else {
                kept
            }
        }
        NodeTag::T_CteScan => match at.stmt.zip(n.as_cte_scan()).and_then(|(s, c)| crate::rerun::subplan_root(s, c.ctePlanId)).and_then(|r| r.as_plan()) {
            None => Keep::Refuse(NO_CTE),
            Some(c) if c.allParam.overlap_list(at.changed) => Keep::Rebuild,
            Some(_) => kept,
        },
        NodeTag::T_FunctionScan => {
            // per function (ExecReScanFunctionScan): a function whose funcparams a changed param meets is recomputed, the
            // others' tuplestores rewound; rebuilding the whole scan is PG's answer only when they all are, or the fill is quiet
            let fs = n.as_function_scan().map(|f| &f.functions);
            let again = |f: Node<'p>| f.as_range_tbl_function().is_none_or(|f| f.funcparams.overlap_list(at.changed));
            match fs.map_or((0, 0), |fs| (fs.iter().filter(|&f| again(f)).count(), fs.iter().count())) {
                (0, _) => kept,
                (a, all) if a == all => Keep::Rebuild,
                _ => Keep::Unseen { usual: Usual::Rebuild, marks: false, why: PART_FN },
            }
        }
        NodeTag::T_TableFuncScan if own => Keep::Rebuild,
        NodeTag::T_TableFuncScan | NodeTag::T_NamedTuplestoreScan => kept,
        NodeTag::T_Limit => p.lefttree.map_or(again, |c| match keep_rule(c, &Rescan { parent: Some(n), ..*at }) {
            Keep::Rerun if own => Keep::Rebuild,
            k => k,
        }),
        NodeTag::T_ModifyTable => Keep::Refuse(NEVER),
        NodeTag::T_SetOp
        | NodeTag::T_RecursiveUnion
        | NodeTag::T_WorkTableScan
        | NodeTag::T_Unique
        | NodeTag::T_Group
        | NodeTag::T_WindowAgg
        | NodeTag::T_Result
        | NodeTag::T_ProjectSet
        | NodeTag::T_ValuesScan
        | NodeTag::T_SubqueryScan
        | NodeTag::T_Append
        | NodeTag::T_MergeAppend
        | NodeTag::T_NestLoop
        | NodeTag::T_MergeJoin
        | NodeTag::T_HashJoin
        | NodeTag::T_LockRows
        | NodeTag::T_Gather
        | NodeTag::T_GatherMerge
        | NodeTag::T_SeqScan
        | NodeTag::T_SampleScan
        | NodeTag::T_IndexScan
        | NodeTag::T_IndexOnlyScan
        | NodeTag::T_BitmapIndexScan
        | NodeTag::T_BitmapHeapScan
        | NodeTag::T_BitmapAnd
        | NodeTag::T_BitmapOr
        | NodeTag::T_TidScan
        | NodeTag::T_TidRangeScan
        | NodeTag::T_ForeignScan => again,
        _ => Keep::Refuse(NO_RULE),
    }
}

/// ExecReScanFunctionScan's answer for one function of a FunctionScan, read from its expression `funcexpr` (a site that holds
/// the expression, not the RangeTblFunction): its tuplestore is dropped and the function called again iff a changed param
/// meets the PARAM_EXEC ids the expression reads (the planner's `funcparams`), else the store is rewound and the function
/// is not called again.
pub fn function_rescan(funcexpr: Node<'_>, changed: &[i32]) -> Keep {
    if crate::params::reads_any(funcexpr, changed) {
        Keep::Rebuild
    } else {
        Keep::Keep { marks: false }
    }
}

/// HJ_FILL_INNER or RIGHT SEMI: the joins whose kept table's match flags ExecReScanHashJoin clears.
fn fills_inner(j: JoinType) -> bool {
    matches!(j, JoinType::JOIN_RIGHT | JoinType::JOIN_FULL | JoinType::JOIN_RIGHT_ANTI | JoinType::JOIN_RIGHT_SEMI)
}

/// A Limit parent's count or offset reads a changed param: the bound it passes down (ExecSetTupleBound) may change.
fn bound_reads(at: &Rescan<'_, '_>) -> bool {
    at.parent.and_then(|l| l.as_limit()).is_some_and(|l| l.limitCount.into_iter().chain(l.limitOffset).any(|e| crate::params::reads_any(e, at.changed)))
}

/// What a site may do with PG's answer beyond following it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Site {
    /// Keep what PG re-runs when its fill is quiet: no call acts and a run returns the same rows, so a re-run would show
    /// nothing (a raise the first run makes is the one PG's re-run would make), and the plan-order run keeps it too.
    pub keep_rerun: bool,
    /// The site checks an `Unseen`'s run-time fact itself and leaves for PG's form when it turns out true (a kept hash
    /// table's one-batch cap: the hash join family's probe_kept.rs): an acting fill may take the usual answer.
    pub checks_unseen: bool,
}

/// PG's answer resolved against `fill`, the effects of the code that fills the node's state, at a site that may do `site`.
pub fn resolve(k: Keep, fill: Effects, site: Site) -> Kept {
    let same = !fill.acts && fill.same_rows;
    let quiet = same || site.checks_unseen;
    match k {
        Keep::Keep { marks } => Kept::Keep { marks },
        Keep::Rebuild => Kept::Rebuild,
        Keep::Rerun if site.keep_rerun && same => Kept::Keep { marks: false },
        Keep::Rerun => Kept::Rebuild,
        Keep::Unseen { usual: Usual::Keep, marks, .. } if quiet => Kept::Keep { marks },
        Keep::Unseen { usual: Usual::Rebuild, .. } if quiet => Kept::Rebuild,
        Keep::Unseen { why, .. } | Keep::Refuse(why) => Kept::Refuse(why),
    }
}

/// PG's answer for node `n` of `stmt` at the widest rescan an owner can make, a static fact of the plan: every param `n` or
/// one of its inputs reads from outside changes, except `fixed`, the params the statement sets once (extern params and the
/// uncorrelated InitPlans' outputs: rerun.rs `fixed_params`). An InitPlan attached inside `n`'s subtree is re-derived by its
/// rescan, so its output is in the subtree's allParam but in no extParam, and never counts as changed here (a Memoize over a
/// lateral CTE scan keeps its cache). A site that must hold for every rescan reads anything but `Keep::Keep` as "PG may drop
/// it" (fail closed). `arena`: decompose's, for the changed list (decompose allocates nothing after warmup).
pub fn at_any_rescan<'p>(stmt: &'p PlannedStmt<'p>, n: Node<'p>, fixed: &[i32], arena: ArenaRef) -> Keep {
    let Some(p) = n.as_plan() else { return Keep::Refuse(NO_RULE) };
    let ext = |x: Option<Node<'p>>| x.and_then(|x| x.as_plan()).into_iter().flat_map(|c| c.extParam.iter());
    let mut changed: ArenaVec<i32> = ArenaVec::new_in(arena); // alloc: Cold
    changed.extend(p.extParam.iter().chain(ext(p.lefttree)).chain(ext(p.righttree)).filter(|x| !fixed.contains(x)));
    let id = p.plan_node_id;
    keep_rule(n, &Rescan { stmt: Some(stmt), changed: &changed, eflags: eflags_in(stmt, id).unwrap_or(0), parent: node_of(stmt, id).and_then(|(_, up)| up) })
}

/// The plan node `id` (a plan_node_id) of statement `stmt` and its parent, in its own tree (the plan or a subplan).
pub fn node_of<'p>(stmt: &'p PlannedStmt<'p>, id: i32) -> Option<(Node<'p>, Option<Node<'p>>)> {
    fn walk<'p>(n: Node<'p>, up: Option<Node<'p>>, id: i32) -> Option<(Node<'p>, Option<Node<'p>>)> {
        crate::walk::stack_check(); // D3748
        if n.as_plan().is_some_and(|p| p.plan_node_id == id) {
            return Some((n, up));
        }
        let mut found = None;
        kids(n, &mut |k| {
            if found.is_none() {
                found = walk(k, Some(n), id);
            }
        });
        found
    }
    stmt.planTree.into_iter().chain(stmt.subplans.iter().flatten()).find_map(|r| walk(r, None, id))
}

/// The executor flags PG initializes node `id` of `stmt` with: the plan tree's root gets none (a scrollable cursor's
/// BACKWARD is delegated to the row executor), a subplan's root REWIND when rewindPlanIDs holds it (execMain.c:974-976).
pub fn eflags_in(stmt: &PlannedStmt<'_>, id: i32) -> Option<u8> {
    let subs = stmt.subplans.iter().enumerate().filter_map(|(i, r)| r.map(|r| (r, if stmt.rewindPlanIDs.is_member(i as i32 + 1) { REWIND } else { 0 })));
    stmt.planTree.map(|r| (r, 0)).into_iter().chain(subs).find_map(|(r, ef)| eflags_at(r, ef, id))
}

/// The flags PG's ExecInit* of `n`, initialized with `ef`, passes to a child (`inner`: its righttree).
pub fn child_eflags(n: Node<'_>, ef: u8, inner: bool) -> u8 {
    match n.node_tag() {
        NodeTag::T_NestLoop if inner && n.as_nest_loop().is_some_and(|l| l.nestParams.is_nil()) => ef | REWIND,
        NodeTag::T_NestLoop if inner => ef & !REWIND,
        NodeTag::T_MergeJoin if inner && !n.as_merge_join().is_some_and(|m| m.skip_mark_restore) => ef | MARK,
        NodeTag::T_Agg if n.as_agg().is_some_and(|a| a.aggstrategy == AGG_HASHED) => ef & !REWIND,
        NodeTag::T_SetOp if n.as_set_op().is_some_and(|s| s.strategy == SETOP_HASHED) => ef & !REWIND,
        NodeTag::T_Sort | NodeTag::T_Material => ef & !(REWIND | BACKWARD | MARK),
        _ => ef,
    }
}

/// The flags PG initializes node `target` (a plan_node_id) with, walking down from `root` initialized with `ef`: a nest's
/// inner gets `REWIND` from a nest with no NestLoopParam, a SubPlan's root from rewindPlanIDs. None: not under `root`.
pub fn eflags_at(root: Node<'_>, ef: u8, target: i32) -> Option<u8> {
    crate::walk::stack_check(); // D3748
    let p = root.as_plan()?;
    if p.plan_node_id == target {
        return Some(ef);
    }
    let right = p.righttree.and_then(|r| r.as_plan()).map(|r| r.plan_node_id);
    let mut found = None;
    kids(root, &mut |k| {
        if found.is_none() {
            let inner = k.as_plan().is_some_and(|c| Some(c.plan_node_id) == right);
            found = eflags_at(k, child_eflags(root, ef, inner), target);
        }
    });
    found
}

/// Where an owner's rescan of `root` (a nest's inner, a SubPlan's plan, a recursive union's term) reaches a node.
#[derive(Clone, Copy)]
pub enum Reach<'p> {
    /// Not in `root`'s tree: the rescan never runs it (an InitPlan's, a CTE's or a hashed SubPlan's plan is a separate tree,
    /// run again only through its own params).
    Outside,
    /// Under a node that keeps its state at this rescan, which does not rescan its children (`shields`).
    Shielded,
    /// Rescanned, initialized with `eflags` under `parent`.
    At { eflags: u8, parent: Option<Node<'p>> },
}

/// Where a rescan of `root` changing `changed` reaches node `target` (a plan_node_id): PG's ExecReScan runs top down, and a
/// node that keeps its state stops it there.
pub fn reach<'p>(stmt: &'p PlannedStmt<'p>, root: Node<'p>, changed: &[i32], target: i32) -> Reach<'p> {
    fn walk<'p>(stmt: &'p PlannedStmt<'p>, n: Node<'p>, ef: u8, up: Option<Node<'p>>, changed: &[i32], target: i32, shut: bool) -> Option<Reach<'p>> {
        crate::walk::stack_check(); // D3748
        let p = n.as_plan()?;
        if p.plan_node_id == target {
            return Some(if shut { Reach::Shielded } else { Reach::At { eflags: ef, parent: up } });
        }
        let shut = shut || shields(n, &Rescan { stmt: Some(stmt), changed, eflags: ef, parent: up });
        let right = p.righttree.and_then(|r| r.as_plan()).map(|r| r.plan_node_id);
        let mut found = None;
        kids(n, &mut |k| {
            if found.is_none() {
                let inner = k.as_plan().is_some_and(|c| Some(c.plan_node_id) == right);
                found = walk(stmt, k, child_eflags(n, ef, inner), Some(n), changed, target, shut);
            }
        });
        found
    }
    let id = root.as_plan().map_or(-1, |p| p.plan_node_id);
    let up = node_of(stmt, id).and_then(|(_, up)| up);
    walk(stmt, root, eflags_in(stmt, id).unwrap_or(0), up, changed, target, false).unwrap_or(Reach::Outside)
}

/// Whether node `n` keeps its state at rescan `at` and so does not rescan its children: a kept hash table
/// (ExecReScanHashJoin: "needn't rescan inner"), a rewound Sort or Material, a kept hashed aggregate's or SetOp's table.
/// A Memoize keeps its cache but rescans its child for the misses; a Limit always rescans its child.
pub fn shields<'p>(n: Node<'p>, at: &Rescan<'_, 'p>) -> bool {
    let holds = matches!(n.node_tag(), NodeTag::T_Hash | NodeTag::T_Sort | NodeTag::T_Material | NodeTag::T_Agg | NodeTag::T_SetOp);
    holds && matches!(keep_rule(n, at), Keep::Keep { .. } | Keep::Unseen { usual: Usual::Keep, .. })
}

/// `shields` for child `kid` of `parent` in `stmt`, with the flags PG initialized it with: where a rescan's run stops.
pub fn shields_in<'p>(stmt: &'p PlannedStmt<'p>, changed: &[i32], parent: Node<'p>, kid: Node<'p>) -> bool {
    let ef = kid.as_plan().and_then(|p| eflags_in(stmt, p.plan_node_id)).unwrap_or(0);
    shields(kid, &Rescan { stmt: Some(stmt), changed, eflags: ef, parent: Some(parent) })
}

/// D1920 (`rerun::of_rescan`): `shields_in` where PostgreSQL's answer is certain, the node keeping its state (`Keep::Keep`);
/// one whose choice turns on a run-time fact (`Keep::Unseen`: a hashed table that may spill) runs its fill again.
pub fn keeps_in<'p>(stmt: &'p PlannedStmt<'p>, changed: &[i32], parent: Node<'p>, kid: Node<'p>) -> bool {
    let ef = kid.as_plan().and_then(|p| eflags_in(stmt, p.plan_node_id)).unwrap_or(0);
    let holds = matches!(kid.node_tag(), NodeTag::T_Hash | NodeTag::T_Sort | NodeTag::T_Material | NodeTag::T_Agg | NodeTag::T_SetOp);
    holds && matches!(keep_rule(kid, &Rescan { stmt: Some(stmt), changed, eflags: ef, parent: Some(parent) }), Keep::Keep { .. })
}

/// Every CteScan of CTE `plan_id` in the plan tree under `root` (a SubPlan's or a CTE's own plan is a separate tree).
pub fn cte_scans<'p>(root: Node<'p>, plan_id: i32, f: &mut dyn FnMut(Node<'p>)) {
    crate::walk::stack_check(); // D3748
    if root.as_cte_scan().is_some_and(|c| c.ctePlanId == plan_id) {
        f(root);
    }
    kids(root, &mut |k| cte_scans(k, plan_id, &mut *f));
}

/// The plan children PG initializes under `n` (a SubPlan's or a CTE's plan is a separate tree).
fn kids<'p>(n: Node<'p>, f: &mut dyn FnMut(Node<'p>)) {
    let Some(p) = n.as_plan() else { return };
    p.lefttree.into_iter().chain(p.righttree).for_each(&mut *f);
    let lists: [Option<&'p NodeList<'p>>; 5] = [
        n.as_append().map(|a| &a.appendplans),
        n.as_merge_append().map(|a| &a.mergeplans),
        n.as_bitmap_and().map(|a| &a.bitmapplans),
        n.as_bitmap_or().map(|a| &a.bitmapplans),
        n.as_agg().map(|a| &a.chain),
    ];
    for l in lists.into_iter().flatten() {
        l.iter().for_each(&mut *f);
    }
    if let Some(s) = n.as_subquery_scan().and_then(|s| s.subplan) {
        f(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcx::Mcx;
    use types_nodes::plannodes::{Agg, Hash, HashJoin, IncrementalSort, Join, Limit, Material, Memoize, NestLoop, NestLoopParam, Plan, SetOp, Sort};
    use types_nodes::primnodes::{Param, ParamKind};
    use types_nodes::Bitmapset;

    fn bms<'m>(m: Mcx<'m>, ids: &[i32]) -> Bitmapset<'m> {
        let mut b = Bitmapset::empty();
        ids.iter().for_each(|&x| b.add_member(m, x).unwrap());
        b
    }

    fn plan<'m>(m: Mcx<'m>, id: i32, all: &[i32], left: Option<Node<'m>>, right: Option<Node<'m>>) -> Plan<'m> {
        Plan { plan_node_id: id, allParam: bms(m, all), lefttree: left, righttree: right, ..Default::default() }
    }

    fn scan<'m>(m: Mcx<'m>, id: i32, all: &[i32]) -> Node<'m> {
        Node::mk(m, types_nodes::plannodes::SeqScan { scan: types_nodes::plannodes::Scan { plan: plan(m, id, all, None, None), scanrelid: 1 }, ..Default::default() }).unwrap()
    }

    fn agg<'m>(m: Mcx<'m>, strategy: u32, all: &[i32], agg_params: &[i32], child: Node<'m>) -> Node<'m> {
        let p = plan(m, 10, all, Some(child), None);
        Node::mk(m, Agg { plan: p, aggstrategy: strategy, aggsplit: 0, numCols: 0, grpColIdx: &[], grpOperators: &[], grpCollations: &[], numGroups: 1, transitionSpace: 0, aggParams: bms(m, agg_params), groupingSets: NodeList::nil(), chain: NodeList::nil(), distinctHashed: false, gravity_rewrite: None, gravity_finals: &[], gravity_groups_default: false }).unwrap()
    }

    fn sort<'m>(m: Mcx<'m>, id: i32, child: Node<'m>) -> Sort<'m> {
        let all: Vec<i32> = child.as_plan().unwrap().allParam.iter().collect();
        Sort { plan: plan(m, id, &all, Some(child), None), numCols: 0, sortColIdx: &[], sortOperators: &[], collations: &[], nullsFirst: &[], gravity_rewrite: None }
    }

    fn param<'m>(m: Mcx<'m>, id: i32) -> Node<'m> {
        Node::mk(m, Param { paramkind: ParamKind::PARAM_EXEC, paramid: id, paramtype: types_core::INT4OID, paramtypmod: -1, paramcollid: 0, location: -1 }).unwrap()
    }

    const KEEP: Site = Site { keep_rerun: true, checks_unseen: false };

    fn at<'a, 'p>(changed: &'a [i32], eflags: u8, parent: Option<Node<'p>>) -> Rescan<'a, 'p> {
        Rescan { stmt: None, changed, eflags, parent }
    }

    /// ExecReScanAgg: a hashed table is kept unless its input or an aggregate argument reads a changed param (a param only
    /// in the HAVING or the target list keeps it, D2612), and unless it spilled, which the plan does not show; a plain,
    /// sorted or mixed Agg runs again whatever changed (D2624): kept only over a quiet fill, as a licensed departure.
    #[test]
    fn aggregates() {
        let ctx = mcx::MemoryContext::new("lifetime-agg");
        let m = ctx.mcx();
        let having = agg(m, AGG_HASHED, &[5], &[], scan(m, 1, &[]));
        let k = keep_rule(having, &at(&[5], 0, None));
        assert_eq!(k, Keep::Unseen { usual: Usual::Keep, marks: false, why: SPILLED });
        assert_eq!(resolve(k, Effects::NONE, Site::default()), Kept::Keep { marks: false }, "a quiet fill: the same rows either way");
        assert_eq!(resolve(k, Effects::ACTS, KEEP), Kept::Refuse(SPILLED), "an acting fill has no PG-equal answer");
        assert_eq!(resolve(k, Effects::ACTS, Site { checks_unseen: true, ..KEEP }), Kept::Keep { marks: false }, "a site that checks the spill itself");
        assert_eq!(keep_rule(agg(m, AGG_HASHED, &[5], &[5], scan(m, 1, &[])), &at(&[5], 0, None)), Keep::Rebuild, "aggParams");
        assert_eq!(keep_rule(agg(m, AGG_HASHED, &[5], &[], scan(m, 1, &[5])), &at(&[5], 0, None)), Keep::Rebuild, "the input");
        for s in [AGG_PLAIN, AGG_SORTED, AGG_MIXED] {
            let k = keep_rule(agg(m, s, &[], &[], scan(m, 1, &[])), &at(&[5], REWIND, None));
            assert_eq!(k, Keep::Rerun, "strategy {s}: re-aggregated at every rescan");
            assert_eq!(resolve(k, Effects::ACTS, KEEP), Kept::Rebuild, "strategy {s}: a VOLATILE fill runs again");
            assert_eq!(resolve(k, Effects::NONE, KEEP), Kept::Keep { marks: false }, "strategy {s}: a quiet fill may be kept");
            assert_eq!(resolve(k, Effects::NONE, Site::default()), Kept::Rebuild);
            assert_eq!(crate::effects::deny_all(|| resolve(k, Effects::NONE, KEEP)), Kept::Keep { marks: false }, "unobservable: the plan-order run keeps it too");
            assert_eq!(resolve(k, Effects::RAISES, KEEP), Kept::Keep { marks: false }, "a raise repeats the first run's");
            assert_eq!(keep_rule(agg(m, s, &[5], &[], scan(m, 1, &[5])), &at(&[5], 0, None)), Keep::Rebuild);
        }
    }

    /// ExecReScanSort / ExecReScanIncrementalSort / ExecReScanMaterial: a Sort rewinds only with randomAccess (REWIND,
    /// BACKWARD or MARK from its parent, D2616) and an unchanged bound; an IncrementalSort always re-sorts (D2626); a
    /// Material keeps its store only under REWIND (BACKWARD implies it).
    #[test]
    fn sorts_and_stores() {
        let ctx = mcx::MemoryContext::new("lifetime-sort");
        let m = ctx.mcx();
        let s = Node::mk(m, sort(m, 2, scan(m, 1, &[]))).unwrap();
        assert_eq!(keep_rule(s, &at(&[5], REWIND, None)), Keep::Keep { marks: false });
        assert_eq!(keep_rule(s, &at(&[5], MARK, None)), Keep::Keep { marks: false });
        assert_eq!(keep_rule(s, &at(&[5], 0, None)), Keep::Rerun, "no randomAccess: re-sorted");
        let s5 = Node::mk(m, sort(m, 2, scan(m, 1, &[5]))).unwrap();
        assert_eq!(keep_rule(s5, &at(&[5], REWIND, None)), Keep::Rebuild);
        let lim = |count| Limit { plan: plan(m, 3, &[5], Some(s), None), limitOffset: None, limitCount: Some(count), limitOption: Default::default(), uniqNumCols: 0, uniqColIdx: &[], uniqOperators: &[], uniqCollations: &[] };
        let l5 = Node::mk(m, lim(param(m, 5))).unwrap();
        assert_eq!(keep_rule(s, &at(&[5], REWIND, Some(l5))), Keep::Unseen { usual: Usual::Rebuild, marks: false, why: BOUND });
        assert_eq!(keep_rule(l5, &at(&[5], REWIND, None)), Keep::Unseen { usual: Usual::Rebuild, marks: false, why: BOUND }, "a Limit closes its child's sort");
        let l7 = Node::mk(m, lim(param(m, 7))).unwrap();
        assert_eq!(keep_rule(s, &at(&[5], REWIND, Some(l7))), Keep::Keep { marks: false }, "the bound reads no changed param");
        let inc = Node::mk(m, IncrementalSort { sort: sort(m, 2, scan(m, 1, &[])), nPresortedCols: 1 }).unwrap();
        assert_eq!(keep_rule(inc, &at(&[5], REWIND | BACKWARD, None)), Keep::Rerun, "re-executed even under REWIND");
        assert_eq!(resolve(Keep::Rerun, Effects::ACTS, KEEP), Kept::Rebuild);
        let mat = Node::mk(m, Material { plan: plan(m, 2, &[], Some(scan(m, 1, &[])), None) }).unwrap();
        assert_eq!(keep_rule(mat, &at(&[5], REWIND, None)), Keep::Keep { marks: false });
        assert_eq!(keep_rule(mat, &at(&[5], BACKWARD, None)), Keep::Keep { marks: false });
        assert_eq!(keep_rule(mat, &at(&[5], MARK, None)), Keep::Rerun);
        assert_eq!(keep_rule(mat, &at(&[5], 0, None)), Keep::Rerun);
    }

    /// ExecReScanHashJoin keeps a one-batch table its Hash's chgParam misses, clearing the match flags of an inner-filling
    /// or RIGHT SEMI join (D2625); ExecReScanSetOp keeps a hashed table neither input changes; ExecReScanMemoize purges on
    /// a non-key param only.
    #[test]
    fn tables() {
        let ctx = mcx::MemoryContext::new("lifetime-hash");
        let m = ctx.mcx();
        let hash = |all: &[i32]| Node::mk(m, Hash { plan: plan(m, 2, all, Some(scan(m, 1, all)), None), hashkeys: NodeList::nil(), skewTable: 0, skewColumn: 0, skewInherit: false, rows_total: 0.0 }).unwrap();
        let hj = |jt: JoinType, h| Node::mk(m, HashJoin { join: Join { plan: plan(m, 3, &[5], Some(scan(m, 4, &[5])), Some(h)), jointype: jt, ..Default::default() }, ..Default::default() }).unwrap();
        let h = hash(&[]);
        let q = |jt| keep_rule(h, &at(&[5], 0, Some(hj(jt, h))));
        assert_eq!(q(JoinType::JOIN_RIGHT_ANTI), Keep::Unseen { usual: Usual::Keep, marks: true, why: MULTI_BATCH });
        assert_eq!(q(JoinType::JOIN_RIGHT_SEMI), Keep::Unseen { usual: Usual::Keep, marks: true, why: MULTI_BATCH });
        assert_eq!(q(JoinType::JOIN_INNER), Keep::Unseen { usual: Usual::Keep, marks: false, why: MULTI_BATCH });
        assert_eq!(resolve(q(JoinType::JOIN_FULL), Effects::RAISES, Site::default()), Kept::Keep { marks: true });
        let h5 = hash(&[5]);
        assert_eq!(keep_rule(h5, &at(&[5], 0, Some(hj(JoinType::JOIN_INNER, h5)))), Keep::Rebuild);
        assert_eq!(keep_rule(h, &at(&[5], 0, None)), Keep::Refuse(NO_JOIN));
        let setop = |strategy: u32, r: &[i32]| Node::mk(m, SetOp { plan: plan(m, 3, r, Some(scan(m, 1, &[])), Some(scan(m, 2, r))), cmd: 0, strategy, numCols: 0, cmpColIdx: &[], cmpOperators: &[], cmpCollations: &[], cmpNullsFirst: &[], numGroups: 1 }).unwrap();
        assert_eq!(keep_rule(setop(SETOP_HASHED, &[]), &at(&[5], 0, None)), Keep::Keep { marks: false });
        assert_eq!(keep_rule(setop(SETOP_HASHED, &[5]), &at(&[5], 0, None)), Keep::Rebuild);
        assert_eq!(keep_rule(setop(0, &[]), &at(&[5], 0, None)), Keep::Rerun, "a sorted SetOp re-reads both inputs");
        let memo = |child_all: &[i32]| Node::mk(m, Memoize { plan: plan(m, 2, child_all, Some(scan(m, 1, child_all)), None), numKeys: 1, hashOperators: &[], collations: &[], param_exprs: NodeList::nil(), singlerow: false, binary_mode: false, est_entries: 0, keyparamids: bms(m, &[5]) }).unwrap();
        assert_eq!(keep_rule(memo(&[5]), &at(&[5], 0, None)), Keep::Keep { marks: false }, "a key param: the cache stays");
        assert_eq!(keep_rule(memo(&[5, 6]), &at(&[5, 6], 0, None)), Keep::Rebuild, "a non-key param purges it");
    }

    /// ExecReScanMemoize at the widest rescan (`at_any_rescan`; memoize item 5's matrix, moved here from join.rs
    /// `memo_purges`): PG may purge the cache iff the child reads a param outside `keyparamids` that the statement does not
    /// fix; a Memoize with no child plan refuses by name, which the site reads as a purge (fail closed).
    #[test]
    fn memoize_any_rescan() {
        let ctx = mcx::MemoryContext::new("lifetime-memo");
        let m = ctx.mcx();
        // (child extParam, keyparamids, statement-fixed, PG may purge)
        let cases: [(Option<&[i32]>, &[i32], &[i32], bool); 9] = [
            (Some(&[]), &[], &[], false),         // the child reads no param: nothing ever changes
            (Some(&[0]), &[0], &[], false),       // only its key (memoize#1's shape): a rescan changes keys only
            (Some(&[0, 1]), &[0, 1], &[], false), // two keys
            (Some(&[2]), &[0], &[], true),        // an outer column outside the keys (memoize#57's t0.ten)
            (Some(&[0, 2]), &[0], &[], true),     // a key and an outer column
            (Some(&[0, 3]), &[0], &[3], false),   // a key and an uncorrelated InitPlan's output (fixed per statement)
            (Some(&[0, 2, 3]), &[0], &[3], true), // fixed and an outer column: the outer column decides
            (Some(&[3]), &[], &[3], false),       // no key reads, only a fixed param
            (None, &[0], &[], true),              // no child plan: fail closed
        ];
        for (i, (child, ks, fixed, want)) in cases.into_iter().enumerate() {
            let kid = child.map(|c| Node::mk(m, types_nodes::plannodes::SeqScan { scan: types_nodes::plannodes::Scan { plan: Plan { plan_node_id: 1, allParam: bms(m, c), extParam: bms(m, c), ..Default::default() }, scanrelid: 1 }, ..Default::default() }).unwrap());
            let own: Vec<i32> = child.unwrap_or(&[]).iter().chain(ks).copied().collect();
            let p = Plan { plan_node_id: 2, allParam: bms(m, &own), extParam: bms(m, &own), lefttree: kid, ..Default::default() };
            let memo = Node::mk(m, Memoize { plan: p, numKeys: 1, hashOperators: &[], collations: &[], param_exprs: NodeList::nil(), singlerow: false, binary_mode: false, est_entries: 0, keyparamids: bms(m, ks) }).unwrap();
            let stmt = PlannedStmt { planTree: Some(memo), ..Default::default() };
            let arena = gravity_program::Arena::new(1 << 16);
            let k = at_any_rescan(&stmt, memo, fixed, arena.handle());
            assert_eq!(!matches!(k, Keep::Keep { .. }), want, "case {i}: child {child:?} keys {ks:?} fixed {fixed:?}: {k:?}");
        }
    }

    /// ExecReScanFunctionScan, per function: a function whose params a changed param meets is recomputed, the others rewound;
    /// a scan holding both answers `Unseen { usual: Rebuild }` (rebuilding everything is PG-equal only over a quiet fill).
    #[test]
    fn function_scans() {
        let ctx = mcx::MemoryContext::new("lifetime-fn");
        let m = ctx.mcx();
        let rtf = |ps: &[i32]| Node::mk(m, types_nodes::parsenodes::RangeTblFunction { funcexpr: Some(param(m, ps.first().copied().unwrap_or(99))), funcparams: bms(m, ps), ..Default::default() }).unwrap();
        let fscan = |fs| Node::mk(m, types_nodes::plannodes::FunctionScan { scan: types_nodes::plannodes::Scan { plan: plan(m, 1, &[5, 6], None, None), scanrelid: 1 }, functions: fs, funcordinality: false }).unwrap();
        let one = |ps: &[i32]| fscan(NodeList::make1(m, rtf(ps)).unwrap());
        assert_eq!(keep_rule(one(&[5]), &at(&[5], 0, None)), Keep::Rebuild, "its param changed: recomputed");
        assert_eq!(keep_rule(one(&[]), &at(&[5], 0, None)), Keep::Keep { marks: false }, "no param: its store rewound");
        assert_eq!(keep_rule(fscan(NodeList::make2(m, rtf(&[5]), rtf(&[6])).unwrap()), &at(&[5, 6], 0, None)), Keep::Rebuild, "every function recomputed");
        let mixed = keep_rule(fscan(NodeList::make2(m, rtf(&[5]), rtf(&[])).unwrap()), &at(&[5], 0, None));
        assert_eq!(mixed, Keep::Unseen { usual: Usual::Rebuild, marks: false, why: PART_FN });
        assert_eq!(resolve(mixed, Effects::NONE, KEEP), Kept::Rebuild, "a quiet fill: the same rows either way");
        assert_eq!(resolve(mixed, Effects::ACTS, KEEP), Kept::Refuse(PART_FN), "an acting rewound function would run again");
        assert_eq!(function_rescan(param(m, 5), &[5]), Keep::Rebuild);
        assert_eq!(function_rescan(param(m, 7), &[5]), Keep::Keep { marks: false });
    }

    /// No rule, no keep: a node the table does not know refuses by name.
    #[test]
    fn no_rule_refuses() {
        let ctx = mcx::MemoryContext::new("lifetime-none");
        let m = ctx.mcx();
        assert_eq!(keep_rule(param(m, 1), &at(&[], 0, None)), Keep::Refuse(NO_RULE));
        assert_eq!(resolve(Keep::Refuse(NO_RULE), Effects::NONE, KEEP), Kept::Refuse(NO_RULE));
        let cte = Node::mk(m, types_nodes::plannodes::CteScan { scan: types_nodes::plannodes::Scan { plan: plan(m, 1, &[], None, None), scanrelid: 1 }, ctePlanId: 1, cteParam: 0, gravity_fill: false }).unwrap();
        assert_eq!(keep_rule(cte, &at(&[5], 0, None)), Keep::Refuse(NO_CTE));
        assert_eq!(keep_rule(scan(m, 1, &[]), &at(&[5], 0, None)), Keep::Rerun);
        assert_eq!(keep_rule(scan(m, 1, &[5]), &at(&[5], 0, None)), Keep::Rebuild);
    }

    /// ExecInitNestLoop gives the inner REWIND only with no NestLoopParam; Sort and Material shield their children; a
    /// hashed Agg strips REWIND (nodeAgg.c), a plain one passes it on.
    #[test]
    fn flags() {
        let ctx = mcx::MemoryContext::new("lifetime-flags");
        let m = ctx.mcx();
        let s = Node::mk(m, sort(m, 3, scan(m, 4, &[]))).unwrap();
        let plain = agg(m, AGG_PLAIN, &[], &[], s);
        let nest = |params| Node::mk(m, NestLoop { join: Join { plan: plan(m, 1, &[], Some(scan(m, 2, &[])), Some(plain)), ..Default::default() }, nestParams: params }).unwrap();
        let free = nest(NodeList::nil());
        assert_eq!(eflags_at(free, 0, 3), Some(REWIND), "the inner's Sort under a plain Agg rewinds");
        assert_eq!(eflags_at(free, 0, 4), Some(0), "the Sort shields its input");
        assert_eq!(eflags_at(free, 0, 2), Some(0), "the outer gets the nest's own flags");
        assert_eq!(eflags_at(free, 0, 99), None);
        let nlp = Node::mk(m, NestLoopParam { paramno: 5, paramval: param(m, 9) }).unwrap();
        assert_eq!(eflags_at(nest(NodeList::make1(m, nlp).unwrap()), REWIND, 3), Some(0), "a NestLoopParam: no REWIND for the inner");
        let stmt = PlannedStmt { planTree: Some(free), ..Default::default() };
        assert_eq!(eflags_in(&stmt, 3), Some(REWIND));
        assert!(node_of(&stmt, 3).is_some_and(|(n, up)| n.node_tag() == NodeTag::T_Sort && up.is_some_and(|u| u.node_tag() == NodeTag::T_Agg)));
        assert!(node_of(&stmt, 1).is_some_and(|(_, up)| up.is_none()) && node_of(&stmt, 99).is_none());
        let hashed = agg(m, AGG_HASHED, &[], &[], scan(m, 4, &[]));
        assert_eq!(child_eflags(hashed, REWIND | MARK, false), MARK);
        assert_eq!(child_eflags(s, REWIND | BACKWARD | MARK, false), 0);
    }

    /// A rescan of a nest's inner runs top down: a rewound Sort stops it (its input is not rescanned), a Sort without
    /// REWIND passes it on, and the outer and the other trees are outside it.
    #[test]
    fn reaches() {
        let ctx = mcx::MemoryContext::new("lifetime-reach");
        let m = ctx.mcx();
        let s = Node::mk(m, sort(m, 3, scan(m, 4, &[]))).unwrap();
        let plain = agg(m, AGG_PLAIN, &[], &[], s);
        let nest = |params| Node::mk(m, NestLoop { join: Join { plan: plan(m, 1, &[], Some(scan(m, 2, &[])), Some(plain)), ..Default::default() }, nestParams: params }).unwrap();
        let free = PlannedStmt { planTree: Some(nest(NodeList::nil())), ..Default::default() };
        assert!(matches!(reach(&free, plain, &[], 10), Reach::At { eflags: REWIND, parent: Some(n) } if n.node_tag() == NodeTag::T_NestLoop));
        assert!(matches!(reach(&free, plain, &[], 3), Reach::At { eflags: REWIND, parent: Some(n) } if n.node_tag() == NodeTag::T_Agg));
        assert!(matches!(reach(&free, plain, &[], 4), Reach::Shielded), "the rewound Sort keeps its run");
        assert!(matches!(reach(&free, plain, &[], 2), Reach::Outside), "the outer");
        assert!(shields_in(&free, &[], plain, s) && !shields_in(&free, &[], s, scan(m, 4, &[])));
        let nlp = Node::mk(m, NestLoopParam { paramno: 5, paramval: param(m, 9) }).unwrap();
        let bound = PlannedStmt { planTree: Some(nest(NodeList::make1(m, nlp).unwrap())), ..Default::default() };
        assert!(matches!(reach(&bound, plain, &[5], 4), Reach::At { eflags: 0, .. }), "no REWIND: the Sort sorts again");
    }
}
