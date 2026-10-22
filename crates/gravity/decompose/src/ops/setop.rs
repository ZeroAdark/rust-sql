use core::fmt::{self, Write};

use gravity_abi::breaker::{BreakerKind, Budget, Spill};
use gravity_program::{ArenaVec, End, Pipeline, Reason};
use types_nodes::jointype::JoinType;
use types_nodes::list::{IntList, NodeList};
use types_nodes::plannodes::{Plan, RecursiveUnion, SetOp, WorkTableScan};
use types_nodes::primnodes::{ParamKind, SubLinkType, SubPlan};
use types_nodes::{Node, NodeTag};

use crate::graph::{Bound, Op, Source};
use crate::ledger::Kind;
use crate::ops;
use crate::admit::Refusal;
use crate::effects::setop::quiet;
use crate::ops::shared::sort::{column_kind, dump_eq_keys};
use crate::ops::shared::sort::{EqKey, ROW_LAYOUT};
use crate::walk::{Builder, Open, STMT_NODE};
use crate::ops::shared::setop::{IterDesc, IterUntil, SetCmd, SetDesc, SetForm, NO_LEFT, RECURSIVE_SEED_SET, RECURSIVE_SET};

mod gen_stream;
mod stream;

pub use stream::hosted_key;

fn eq_keys<'p>(b: &Builder<'p, '_>, tlist: &NodeList<'p>, cols: &[i16], ops: &[u32], n: i32, k: Kind, id: i32) -> Result<ArenaVec<EqKey>, Refusal> {
    let n = n.max(0) as usize;
    if cols.len() < n || ops.len() < n {
        return Err(Refusal::shape(k, id, "key arrays shorter than their count"));
    }
    let mut keys = ArenaVec::with_capacity_in(n, b.arena);
    for i in 0..n {
        keys.push(EqKey { resno: cols[i], opno: ops[i], kind: column_kind(b.facts, tlist, cols[i], k, id)?, typid: crate::ops::shared::sort::column_type(tlist, cols[i], k, id)? });
    }
    Ok(keys)
}

fn child_tlist<'p>(n: Option<Node<'p>>, k: Kind, id: i32) -> Result<&'p Plan<'p>, Refusal> {
    n.and_then(|n| n.as_plan()).ok_or(Refusal::shape(k, id, "a set operation without two inputs"))
}

/// D1292: a set compare under no collation over a collatable column raises in PG (texteq / hashtext's check).
fn loose_keys(b: &Builder<'_, '_>, tlist: &NodeList<'_>, cols: &[i16], colls: &[u32], n: i32, k: Kind, id: i32) -> Result<(), Refusal> {
    for (i, &c) in cols.iter().take(n.max(0) as usize).enumerate() {
        if crate::ops::shared::sort::indeterminate(b.facts, crate::ops::shared::sort::column_type(tlist, c, k, id)?, colls.get(i).copied().unwrap_or(0)) {
            return Err(Refusal::shape(k, id, crate::ops::shared::sort::INDETERMINATE_COLL));
        }
    }
    Ok(())
}

/// D3748: what `set_op_shape` decides before either input maps.
struct SetShape<'p> {
    cmd: SetCmd,
    hashed: bool,
    keys: ArenaVec<EqKey>,
    n: usize,
    lp: &'p Plan<'p>,
    rp: &'p Plan<'p>,
    ln: Node<'p>,
    rn: Node<'p>,
    /// The streamed form (`stream::form` answered one: `form` is it).
    stream: bool,
    whole: bool,
    form: SetForm,
    pull_loud: bool,
    /// The right side maps (and runs) before the left: the licence of its run ahead of PostgreSQL's left-first read.
    right_first: Option<crate::effects::Licence>,
}

/// D3748: a nest of set operations maps through here once per level, so the frames it keeps live while its inputs map are
/// small: the shape is decided in `set_op_shape`'s frame, each form maps its inputs in its own (`set_op_stream`,
/// `set_op_whole`, `set_op_spools`) and the node closes in `set_op_close`'s, none of them inlined here; the open the form
/// returns is this frame's own return value (no copy of it is held while the inputs map).
#[inline(never)]
fn map_set_op<'p>(b: &mut Builder<'p, '_>, so: &'p SetOp<'p>) -> Result<Open<'p>, Refusal> {
    let s = set_op_shape(b, so)?;
    let mut sides = (NO_LEFT, 0u8);
    let mut out = match (s.stream, s.whole) {
        (true, _) => set_op_stream(b, so, &s, &mut sides),
        (false, true) => set_op_whole(b, so, &s, &mut sides),
        (false, false) => set_op_spools(b, so, &s, &mut sides),
    };
    if out.is_ok() {
        set_op_close(b, so, s, &mut out, sides);
    }
    out
}

/// D3748: the open `r` holds, taken out to be rebuilt (`r` keeps an error meanwhile); None when `r` is an error.
fn take_open<'p>(r: &mut Result<Open<'p>, Refusal>, k: Kind, id: i32) -> Option<Open<'p>> {
    match core::mem::replace(r, Err(Refusal::kind(k, id))) {
        Ok(o) => Some(o),
        Err(e) => {
            *r = Err(e);
            None
        }
    }
}

/// D3748: the set operation's shape, decided before either input maps (in a frame of its own).
#[inline(never)]
fn set_op_shape<'p>(b: &mut Builder<'p, '_>, so: &'p SetOp<'p>) -> Result<SetShape<'p>, Refusal> {
    let (plan, k) = (&so.plan, Kind::SetOp);
    let id = plan.plan_node_id;
    let cmd = match so.cmd {
        0 => SetCmd::Intersect,
        1 => SetCmd::IntersectAll,
        2 => SetCmd::Except,
        3 => SetCmd::ExceptAll,
        _ => return Err(Refusal::shape(k, id, "an unknown set command")),
    };
    let hashed = so.strategy == 1;
    let lp = child_tlist(plan.lefttree, k, id)?;
    let rp = child_tlist(plan.righttree, k, id)?;
    let keys = eq_keys(b, &lp.targetlist, so.cmpColIdx, so.cmpOperators, so.numCols, k, id)?;
    loose_keys(b, &lp.targetlist, so.cmpColIdx, so.cmpCollations, so.numCols, k, id)?;
    let n = (so.numCols.max(0) as usize).min(so.cmpColIdx.len());
    // a sorted SetOp reads its right input only while the left has rows: over a Sort, or with no column (every row one
    // group: PG reads the right side whole once the left has a row), that is all of it or (an empty left) none, which the
    // grouped form keeps; over any other input PG may stop early. No column: each side is a plain group (agg.rs set_side)
    // F-SORTED (D1239): over any other right input PG may stop reading it early (the sorted retrieve stops at the left's
    // end); when the right subtree cannot raise and calls nothing volatile (F5, rescan::of_plan) that stop is unobservable
    // and the grouped form is exact. A Unique over a Sort (a DISTINCT side, D1239) is one: its Sort reads its input whole
    // before the Unique's first row, and the Unique's equality is F5-checked with the rest of the subtree.
    let quiet = |r: Node<'p>| crate::effects::setop::stop_unseen(b, r);
    let (ln, rn) = (plan.lefttree.unwrap(), plan.righttree.unwrap());
    let grouped = hashed || n == 0 || rn.as_sort().is_some() || quiet(rn);
    // D2801: any other right input streams where setop/stream.rs serves it (PG's walk: both inputs read as PG reads them);
    // a quiet one too, with a quiet left, run once on the statement's root path (`root_once`): the grouped form's whole
    // read of both inputs is exact there but costs every row PG never reads (past a LIMIT, or the right's rows past the
    // left's last group). No refusal and no counted-pull mark below depends on the choice then (a quiet left, no reader
    // that stops early)
    let x1 = grouped && !hashed && n > 0 && rn.as_sort().is_none() && root_once(b, id) && !stopped_early(b, id) && !crate::effects::setop::streamed_left_raises(b, ln);
    // D2805 (D3377): a left that may raise on the statement's root path, read by a partial reader (a named cursor's FETCH,
    // a counted portal: `PipelineGraph::partial`) whose count may stop the pull where PG has read only the left rows it
    // handed on: the grouped form reads the left whole at the first pull, so the streamed form (setop/stream.rs, where it
    // serves the shape) reads it as PG does, and no counted-pull mark is set below. A whole reader keeps the grouped form
    // (a quiet Sort over a relation scan as the right input too: `stream::sorted_scan`, pulled with its fill at the first pull)
    let x2 = grouped && b.graph.partial && !hashed && n > 0 && (rn.as_sort().is_none() || stream::sorted_scan(b, rn).is_some()) && !stopped_early(b, id) && on_root_path(b, id) && crate::effects::setop::streamed_left_raises(b, ln);
    // D2800 (D3376): a left that may raise under a sub-plan PG may stop reading early (`stopped_early`: an EXISTS, scalar,
    // ANY or ALL sublink, or a CTE's plan a LIMIT reads part of): the grouped form reads the left whole at the first pull,
    // so the streamed form reads it as PG does: the step hands each left group on as it reads it, and an EXISTS InitPlan
    // ends its pipeline at the first row handed on (lowerv4 ops/subplan/initplan `Init`, the count-reached exit), past
    // which no left row is read. Where the streamed form does not serve the shape, the refusal below stays
    let x3 = grouped && !hashed && n > 0 && (rn.as_sort().is_none() || stream::sorted_scan(b, rn).is_some()) && stopped_early(b, id) && crate::effects::setop::streamed_left_raises(b, ln);
    let stream = if grouped && !(x1 || x2 || x3) { None } else { stream::form(b, so, lp, &keys, rn, n) };
    // else one that may raise but acts nowhere and returns the same rows run to run is read whole too, ahead of PG's early
    // stop: the lowering asks a recheck licence for it, which a user function feeding it denies (`SetForm::Ahead`)
    let e = crate::effects::of_plan(&crate::effects::Cx::new(b.stmt, b.facts, b.arena), rn);
    let ahead = stream.is_none() && !grouped && e.may_raise && !e.acts && e.same_rows;
    // D2800: PG's sorted SetOp hands on each left group's rows as it reads the left input (nodeSetOp.c
    // setop_retrieve_sorted), so a sublink that stops at its first or second row leaves the rest of the left unread; the
    // grouped and licence forms below read the left whole first (the streamed form reads it as PG does), so a left that
    // may raise past those rows errors where PG answers
    let loud_left = stream.is_none() && !hashed && n > 0 && crate::effects::setop::streamed_left_raises(b, ln);
    if loud_left && stopped_early(b, id) {
        return Err(Refusal::shape(k, id, STOPPED_LEFT));
    }
    let pull_loud = loud_left && on_root_path(b, id);
    let whole = stream.is_none() && (grouped || ahead);
    // construct C2: a quiet right input (`stop_unseen`) the grouped form reads whole where PG's sorted walk may stop holds
    // the Ahead licence of code that neither acts nor raises (its rows may differ run to run: the read re-runs nothing)
    let quiet_right = (grouped && !hashed && n > 0 && rn.as_sort().is_none()).then(|| crate::effects::quiet_ahead(e)).flatten();
    let form = stream.unwrap_or(if ahead { SetForm::Ahead } else { SetForm::Grouped(quiet_right) });
    let right_first = (whole && held(rn) >= LEFT_FIRST_HELD && nodes(rn) > nodes(ln)).then(|| quiet_ahead(b, rn)).flatten();
    Ok(SetShape { cmd, hashed, keys, n, lp, rp, ln, rn, stream: stream.is_some(), whole, form, pull_loud, right_first })
}

/// The licence of subtree `n`'s run ahead of PostgreSQL's order (the right side mapped first, a Sort's fill at the
/// first pull): it shows nothing run ahead (`effects::quiet_ahead`: no act, no raise; the run re-runs nothing, so rows
/// that differ run to run are no difference) and calls no user function (`effects::setop::quiet`). `None` otherwise.
pub(super) fn quiet_ahead<'p>(b: &Builder<'p, '_>, n: Node<'p>) -> Option<crate::effects::Licence> {
    let licence = crate::effects::quiet_ahead(crate::effects::of_plan(&crate::effects::Cx::new(b.stmt, b.facts, b.arena), n))?;
    quiet(b, n).then_some(licence)
}

/// D3748: `map_set_op`'s streamed form's inputs, in a frame of their own: the left input's open is its return value.
#[inline(never)]
fn set_op_stream<'p>(b: &mut Builder<'p, '_>, so: &'p SetOp<'p>, s: &SetShape<'p>, sides: &mut (u8, u8)) -> Result<Open<'p>, Refusal> {
    // the emit is the left input's pipeline, serial (the step reads its order) and one pipeline (a streamed union's
    // arms settle into their slot); the right input is a spool its step pulls (lowerv4 ops/setop/stream.rs)
    let mut lo = b.plan(s.ln);
    if lo.is_ok() {
        stream_left(b, &mut lo, so.plan.plan_node_id);
    }
    if lo.is_ok() {
        match stream_right(b, so, s) {
            Ok(right) => *sides = (NO_LEFT, right),
            Err(e) => lo = Err(e),
        }
    }
    lo
}

/// D3748: the streamed form's left open, settled and made serial (`set_op_stream`).
#[inline(never)]
fn stream_left<'p>(b: &mut Builder<'p, '_>, r: &mut Result<Open<'p>, Refusal>, id: i32) {
    let Some(lo) = take_open(r, Kind::SetOp, id) else { return };
    let mut lo = crate::breakers::one_fill(&mut b.graph, lo);
    if lo.desc.is_eligible() {
        let site = lo.desc.driving_site.max(0);
        lo.desc = Pipeline { parallel: gravity_program::Parallel::Ineligible { site, reason: Reason::SortedSource }, ..lo.desc };
    }
    *r = Ok(lo);
}

/// D3748: the streamed form's right input, mapped and spooled: its slot (`set_op_stream`).
#[inline(never)]
fn stream_right<'p>(b: &mut Builder<'p, '_>, so: &'p SetOp<'p>, s: &SetShape<'p>) -> Result<u8, Refusal> {
    let (k, id) = (Kind::SetOp, so.plan.plan_node_id);
    let ro = b.plan(s.rn)?;
    // D2805 (D3377): a Sort's bare emit is spooled on its own (never the Sort's slot, which a pull cannot stop inside):
    // its fill is the pulled set's producer, run at the first pull (lowerv4 ops/setop/stream.rs `sorted_emit`)
    match s.rn.as_sort() {
        Some(_) => crate::ops::shared::breaker::close(b, ro, BreakerKind::Materialize, ROW_LAYOUT, Budget::Share, Spill::Runs, k, id),
        None => crate::ops::shared::breaker::spool(b, ro, k, id),
    }
}

/// D3748: `map_set_op`'s grouped forms' inputs (each side a group over the compared columns, PG 18's numLeft / numRight
/// per key: the emit walks the left groups, a sorted SetOp's in its Sort's order, and the right group is read whole by
/// the set step, never emitted), in a frame of their own: the left group's emit is its return value.
#[inline(never)]
fn set_op_whole<'p>(b: &mut Builder<'p, '_>, so: &'p SetOp<'p>, s: &SetShape<'p>, sides: &mut (u8, u8)) -> Result<Open<'p>, Refusal> {
    // CAP-36: a side's group stays live while the other side maps, so the larger side maps first and a nest of set
    // operations holds slots in the log of its size, not its depth (PG 18 nests INTERSECT ALL to the right: a
    // 300-arm one held 299 left groups at once). The right side goes first only when a left-first map of it would
    // hold `LEFT_FIRST_HELD` left groups or more at once (`held`: a deep nest, past a quarter of the breaker slots)
    // and nothing in it can raise or call anything volatile or a user function (F5), so its earlier run is
    // unobservable. Otherwise the left maps first, as PG reads it: an empty left then skips the right side
    // (setop.emptyleft; nodeSetOp.c reads no inner after an empty outer), which a right side run first cannot
    // (gen setop_core-0190: an empty t0 left of a two-arm nest, whose right side PG never runs).
    if s.right_first.is_some() {
        sides.1 = whole_right(b, so, s)?;
    }
    let mut out = whole_left(b, so, s, &mut sides.0);
    if out.is_ok() && s.right_first.is_none() {
        match whole_right(b, so, s) {
            Ok(right) => sides.1 = right,
            Err(e) => out = Err(e),
        }
    }
    // the right group's emit is dropped (the set step reads it whole); its read goes only now, after both sides
    // mapped: earlier, a right-first group with no reads would look free to the left side's breakers
    if out.is_ok() {
        crate::breakers::unread(&mut b.graph, sides.1);
    }
    out
}

/// D3748: the grouped form's left side, mapped and grouped: its group's emit, `slot` the group's slot.
#[inline(never)]
fn whole_left<'p>(b: &mut Builder<'p, '_>, so: &'p SetOp<'p>, s: &SetShape<'p>, slot: &mut u8) -> Result<Open<'p>, Refusal> {
    let mut r = b.plan(s.ln);
    if r.is_ok() {
        whole_group(b, so, s, true, &mut r, slot);
    }
    r
}

/// D3748: the grouped form's right side, mapped and grouped: its group's slot (its emit dropped).
#[inline(never)]
fn whole_right<'p>(b: &mut Builder<'p, '_>, so: &'p SetOp<'p>, s: &SetShape<'p>) -> Result<u8, Refusal> {
    let mut r = b.plan(s.rn);
    let mut slot = 0;
    if r.is_ok() {
        whole_group(b, so, s, false, &mut r, &mut slot);
    }
    r.map(|_| slot)
}

/// D3748: side `left`'s mapped open (in `r`) closed into its group (`agg::set_side`): `r` becomes the group's emit.
#[inline(never)]
fn whole_group<'p>(b: &mut Builder<'p, '_>, so: &'p SetOp<'p>, s: &SetShape<'p>, left: bool, r: &mut Result<Open<'p>, Refusal>, slot: &mut u8) {
    let (k, id) = (Kind::SetOp, so.plan.plan_node_id);
    let Some(o) = take_open(r, k, id) else { return };
    let (cols, colls) = (&so.cmpColIdx[..s.n], so.cmpCollations);
    let groups = so.numGroups.max(0) as u64;
    let grouped = match left {
        true => crate::ops::agg::set_side(b, o, cols, colls, true, !s.hashed && s.n > 0, if groups > 0 { groups } else { Builder::rows(s.lp) }, k, id),
        false => crate::ops::agg::set_side(b, o, cols, colls, false, false, Builder::rows(s.rp), k, id),
    };
    *r = grouped.map(|(out, at)| {
        *slot = at;
        out
    });
}

/// D3748: `map_set_op`'s spooled inputs, in a frame of their own: the emit over the left spool is its return value.
#[inline(never)]
fn set_op_spools<'p>(b: &mut Builder<'p, '_>, so: &'p SetOp<'p>, s: &SetShape<'p>, sides: &mut (u8, u8)) -> Result<Open<'p>, Refusal> {
    let id = so.plan.plan_node_id;
    let left = spooled(b, so, s.ln)?;
    let right = spooled(b, so, s.rn)?;
    *sides = (left, right);
    Ok(crate::ops::shared::breaker::emit(b, id, left))
}

/// D3748: input `n` of the set operation mapped and spooled: the spool's slot (`set_op_spools`).
#[inline(never)]
fn spooled<'p>(b: &mut Builder<'p, '_>, so: &'p SetOp<'p>, n: Node<'p>) -> Result<u8, Refusal> {
    let o = b.plan(n)?;
    crate::ops::shared::breaker::spool(b, o, Kind::SetOp, so.plan.plan_node_id)
}

/// D3748: the set operation's node over its mapped inputs (`r` holds the emit), in a frame of its own.
#[inline(never)]
fn set_op_close<'p>(b: &mut Builder<'p, '_>, so: &'p SetOp<'p>, s: SetShape<'p>, r: &mut Result<Open<'p>, Refusal>, (left, right): (u8, u8)) {
    let (plan, k) = (&so.plan, Kind::SetOp);
    let Some(mut out) = take_open(r, k, plan.plan_node_id) else { return };
    let SetShape { cmd, hashed, keys, form, pull_loud, .. } = s;
    crate::ops::shared::breaker::consume(b, &mut out, right);
    out.push(Op::SetOp(SetDesc::Set { cmd, hashed, left, right, keys, groups: so.numGroups.max(0) as u64, tlist: &plan.targetlist, form, pull_loud }));
    ops::filters(&mut out, &plan.qual);
    ops::project(&mut out, &plan.targetlist);
    b.graph.note(k);
    *r = Ok(out);
}

/// D2800's refusal name.
const STOPPED_LEFT: &str = "a sorted set operation a sublink may stop reading early (EXISTS, a scalar, ANY or ALL sublink) whose left input may raise past the rows PostgreSQL reads: the grouped form reads the left whole (D2800)";

/// PostgreSQL's AggStrategy AGG_SORTED (nodes.h): a GroupAggregate hands on each group as it reads its sorted input.
const AGG_SORTED: u32 = 1;
/// PostgreSQL's SetOpStrategy SETOP_HASHED (nodes.h): a SetOp that reads all of its input into a hash table first.
const SETOP_HASHED: u32 = 1;

/// D2800: plan node `id` sits in the plan of a sub-plan PostgreSQL may stop reading before its end, on a path of nodes
/// that hand rows on as they read them (`streams_to`). A sub-plan reads to its end when it is an ARRAY sublink's
/// (nodeSubplan.c ExecScanSubPlan reads every row) or a hashed SubPlan's (buildSubPlanHash fills its table whole); any
/// other, and one no SubPlan in a plan node's InitPlans, target list or qual names, may stop (fail closed). A CTE's plan
/// may stop when a CteScan of it is reached that way from a LIMIT with a count or from such a sub-plan (nodeCtescan.c
/// reads the CTE's plan only as far as its readers pull).
fn stopped_early<'p>(b: &Builder<'p, '_>, id: i32) -> bool {
    let stmt = b.stmt;
    if stmt.subplans.is_nil() {
        return false;
    }
    let (mut cte, mut whole, mut early): (ArenaVec<i32>, ArenaVec<i32>, ArenaVec<i32>) = (ArenaVec::new_in(b.arena), ArenaVec::new_in(b.arena), ArenaVec::new_in(b.arena)); // alloc: Cold
    let mut limits: ArenaVec<Node<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    crate::rerun::each_plan_node(stmt, &mut |n| {
        if let Some(c) = n.as_cte_scan() {
            cte.push(c.ctePlanId); // alloc: Cold
        }
        if let Some(l) = n.as_limit().filter(|l| !matches!(crate::ops::limit_bounds(l).0, Bound::All)) {
            if let Some(c) = l.plan.lefttree {
                limits.push(c); // alloc: Cold
            }
        }
        let Some(p) = n.as_plan() else { return };
        let mut see = |s: &'p SubPlan<'p>| match s.subLinkType {
            SubLinkType::CTE_SUBLINK => cte.push(s.plan_id), // alloc: Cold
            SubLinkType::ARRAY_SUBLINK => whole.push(s.plan_id), // alloc: Cold
            _ if s.useHashTable => whole.push(s.plan_id), // alloc: Cold
            _ => early.push(s.plan_id), // alloc: Cold
        };
        for s in p.initPlan.iter().filter_map(|i| i.as_sub_plan()) {
            see(s);
        }
        for e in p.targetlist.iter().chain(p.qual.iter()) {
            each_subplan(e, &mut see);
        }
    });
    let root = |pid: i32| crate::rerun::subplan_root(stmt, pid);
    let stops = |pid: i32| !cte.contains(&pid) && (early.contains(&pid) || !whole.contains(&pid));
    let scans = |pid: i32| move |n: Node<'_>| n.as_cte_scan().is_some_and(|c| c.ctePlanId == pid);
    let cte_stops = |pid: i32| {
        cte.contains(&pid)
            && (limits.iter().any(|&l| streams_to(l, &scans(pid)))
                || (1..=stmt.subplans.len() as i32).any(|q| stops(q) && root(q).is_some_and(|r| streams_to(r, &scans(pid)))))
    };
    let at = |n: Node<'_>| n.as_plan().is_some_and(|p| p.plan_node_id == id);
    (1..=stmt.subplans.len() as i32).any(|pid| (stops(pid) || cte_stops(pid)) && root(pid).is_some_and(|r| streams_to(r, &at)))
}

/// SetOp `id` on the statement's root path through nodes of one input PG runs a row at a time (a Limit, a Result, a
/// SubqueryScan, a sorted Unique or Agg): run once, and read only as far as the root's reader and the left input take it.
fn root_once<'p>(b: &Builder<'p, '_>, id: i32) -> bool {
    let mut at = b.stmt.planTree;
    while let Some(n) = at {
        let Some(p) = n.as_plan() else { return false };
        if p.plan_node_id == id {
            return true;
        }
        let sorted_agg = n.as_agg().is_none_or(|a| a.aggstrategy == AGG_SORTED);
        if p.righttree.is_some() || n.as_sort().is_some() || !sorted_agg || n.as_set_op().is_some() || n.as_append().is_some() || n.as_merge_append().is_some() {
            return false;
        }
        at = n.as_subquery_scan().and_then(|s| s.subplan).or(p.lefttree);
    }
    false
}

/// D2800: plan subtree `n` reaches a node `hit` names through nodes PG runs a row at a time, so a reader that stops early
/// stops that node's read too: never through a Sort, a hashed or plain Agg or a hashed SetOp, which read all of their
/// input before their first row. A SubqueryScan's plan and an Append's or MergeAppend's members are walked as children.
fn streams_to<'p>(n: Node<'p>, hit: &dyn Fn(Node<'p>) -> bool) -> bool {
    crate::walk::stack_check(); // D3748
    let Some(p) = n.as_plan() else { return false };
    if hit(n) {
        return true;
    }
    if n.as_sort().is_some() || n.as_agg().is_some_and(|a| a.aggstrategy != AGG_SORTED) || n.as_set_op().is_some_and(|s| s.strategy == SETOP_HASHED) {
        return false;
    }
    let arms = n.as_append().map(|a| &a.appendplans).or(n.as_merge_append().map(|a| &a.mergeplans));
    [p.lefttree, p.righttree, n.as_subquery_scan().and_then(|s| s.subplan)].into_iter().flatten().any(|c| streams_to(c, hit))
        || arms.is_some_and(|l| l.iter().any(|c| streams_to(c, hit)))
}

/// D3373 (D-a): `n` is a CteScan under only projections (a Result with no one-time qual, a SubqueryScan).
fn cte_below(n: Option<Node<'_>>) -> bool {
    crate::walk::stack_check(); // D3748
    let Some(n) = n else { return false };
    match n.node_tag() {
        NodeTag::T_Result if n.as_result().is_some_and(|r| r.resconstantqual.is_none()) => cte_below(n.as_plan().and_then(|p| p.lefttree)),
        NodeTag::T_SubqueryScan => cte_below(n.as_subquery_scan().and_then(|s| s.subplan)),
        k => k == NodeTag::T_CteScan,
    }
}

/// `n` is a Sort or an Agg under only projections (a Result with no one-time qual, a SubqueryScan).
fn whole_below(n: Option<Node<'_>>) -> bool {
    crate::walk::stack_check(); // D3748
    let Some(n) = n else { return false };
    match n.node_tag() {
        NodeTag::T_Result if n.as_result().is_some_and(|r| r.resconstantqual.is_none()) => whole_below(n.as_plan().and_then(|p| p.lefttree)),
        NodeTag::T_SubqueryScan => whole_below(n.as_subquery_scan().and_then(|s| s.subplan)),
        k => matches!(k, NodeTag::T_Sort | NodeTag::T_Agg),
    }
}

/// D2805: plan node `id` sits on the statement's own root path through nodes PG runs a row at a time (`streams_to`),
/// directly or through the plan of a CTE that a CteScan on that path reads (nodeCtescan.c pulls a CTE's plan only as far
/// as its readers pull), so a pull that may count its rows stops reading it where the client stops.
fn on_root_path<'p>(b: &Builder<'p, '_>, id: i32) -> bool {
    let stmt = b.stmt;
    let Some(top) = stmt.planTree else { return false };
    let at = |n: Node<'_>| n.as_plan().is_some_and(|p| p.plan_node_id == id);
    let (mut roots, mut entered): (ArenaVec<Node<'p>>, ArenaVec<i32>) = (ArenaVec::new_in(b.arena), ArenaVec::new_in(b.arena)); // alloc: Cold
    roots.push(top); // alloc: Cold
    let mut i = 0;
    while let Some(&r) = roots.get(i) {
        i += 1;
        if streams_to(r, &at) {
            return true;
        }
        for pid in 1..=stmt.subplans.len() as i32 {
            if entered.contains(&pid) || !streams_to(r, &|n: Node<'p>| n.as_cte_scan().is_some_and(|c| c.ctePlanId == pid)) {
                continue;
            }
            entered.push(pid); // alloc: Cold
            if let Some(cr) = crate::rerun::subplan_root(stmt, pid) {
                roots.push(cr); // alloc: Cold
            }
        }
    }
    false
}

/// The plan nodes in subtree `n` (its children, a SubqueryScan's plan, an Append's arms): the size `map_set_op` maps
/// first.
fn nodes(n: Node<'_>) -> usize {
    crate::walk::stack_check();
    let Some(p) = n.as_plan() else { return 1 };
    let arms = n.as_append().map(|a| &a.appendplans).or(n.as_merge_append().map(|a| &a.mergeplans));
    1 + [p.lefttree, p.righttree, n.as_subquery_scan().and_then(|s| s.subplan)].into_iter().flatten().map(nodes).sum::<usize>()
        + arms.map_or(0, |l| l.iter().map(nodes).sum())
}

/// CAP-36: the left groups a left-first map of a nest past which `map_set_op` maps a quiet right side first: a quarter of
/// the breaker slots (`limits::BREAKERS`), so the groups a left-first nest holds leave the rest of the plan its slots.
const LEFT_FIRST_HELD: usize = gravity_program::limits::BREAKERS / 4;

/// CAP-36: the left groups a left-first map of plan subtree `n` holds at once (its children, a SubqueryScan's plan, an
/// Append's arms): a set operation's left group stays live while its right side maps, so it holds one more than its right
/// side, or what its left side holds (that side's groups close before its own group opens).
fn held(n: Node<'_>) -> usize {
    crate::walk::stack_check();
    let Some(p) = n.as_plan() else { return 0 };
    if n.as_set_op().is_some() {
        return p.lefttree.map_or(0, held).max(1 + p.righttree.map_or(0, held));
    }
    let arms = n.as_append().map(|a| &a.appendplans).or(n.as_merge_append().map(|a| &a.mergeplans));
    let kids = [p.lefttree, p.righttree, n.as_subquery_scan().and_then(|s| s.subplan)].into_iter().flatten().map(held).max().unwrap_or(0);
    kids.max(arms.map_or(0, |l| l.iter().map(held).max().unwrap_or(0)))
}

/// S6: a Hash Join, a hashed Agg or a hashed SetOp in the rows of plan subtree `n` (its children, a SubqueryScan's plan, an
/// Append's arms): each hands its rows on in an order of its own table; a node that is no plan counts as one (fail closed).
fn hashes(n: Node<'_>) -> bool {
    crate::walk::stack_check();
    let Some(p) = n.as_plan() else { return true };
    let arms = n.as_append().map(|a| &a.appendplans).or(n.as_merge_append().map(|a| &a.mergeplans));
    let hashed = n.as_agg().is_some_and(|a| a.aggstrategy == crate::ops::shared::agg::AGG_HASHED || a.aggstrategy == crate::ops::shared::agg::AGG_MIXED);
    n.node_tag() == types_nodes::NodeTag::T_HashJoin
        || hashed
        || n.as_set_op().is_some_and(|s| s.strategy == 1)
        || [p.lefttree, p.righttree, n.as_subquery_scan().and_then(|s| s.subplan)].into_iter().flatten().any(hashes)
        || arms.is_some_and(|l| l.iter().any(hashes))
}

/// Whether PG surely reads a recursive union no further than its lowering does, decided on the plan as PG runs
/// it. The lowering runs a generation whole and the CTE's readers pull it, so a union that never ends returns only where
/// each breaker over it is one PG surely runs whole too. Served: every CteScan of the CTE sits in the statement's tree
/// on a path from the root through only a LIMIT, a projection (a Result with no gate, a SubqueryScan), a Unique, a row
/// lock (a LockRows), a Sort, an Agg, a ModifyTable, a window over or under one of the last three (PG's window streams a partition the
/// lowering buffers; with such a breaker on its path both read the union whole; one with a run condition, which may end
/// PG's read, only over one; a window PG reads whole before its first row, `facts::window::reads_whole`, is one of
/// those breakers itself), a hash join (`hash_join`), a merge join (`merge_join`), a SetOp whose outer reads the union
/// itself (`set_op`, D3373), a ProjectSet under such a breaker (D3373), a nested loop's outer, a nested
/// loop's inner and a Memoize in it (read lazily at each rescan, as a SubPlan's tree is), an Append nothing above may
/// stop and
/// another CTE's plan (`enter`), where a Sort, an Agg, a ModifyTable, such a window, a hash build or a merge join is
/// under no LIMIT whose count is not a constant of at least one (PG may never pull it: D2223), but a Sort or an Agg under one
/// LIMIT that may count 0 (K11.rec.l0 / .gl: PG reads the union whole or, at a zero count, not at all; `IterDesc::zero`,
/// served where the lowering's zero check stops every reader first). No CteScan at all reads nothing (D3373: a
/// constant-false gate over the only reader). Every other reader
/// refuses by name (a build or a merge PG may skip, a window over the union itself, a Material under no whole read, an
/// Append under a LIMIT or a probe an empty build may end, a gate, a SubPlan's tree but an InitPlan the root's one row
/// reads: `root_reads`). D2212: a CteScan in another CTE's plan (a second recursive union's seed or term, a plain CTE)
/// is served where PG runs that plan to its end: a data-modifying CTE (PG's ExecPostprocessPlan runs it whole, read or
/// not), or one a reader reads whole (`At::whole`) or to its end (no LIMIT, no probe an empty build may end on its
/// path: `At::early`); the lowering runs the other union's seed and its once-run builds whole where PG streams the
/// seed, so a stopping reader of it would read this union to its end where PG stops. Such a union is read through (a
/// counted pull refuses, D2230: the pull may stop where the lowering runs the other union's whole seed first) unless a
/// breaker reads that plan whole. `Ok((thin, through, zero, whole, top))`: `thin`, a qual or a join drops the union's
/// rows on their way to the client with no breaker over it, or the union is read through a nested loop's inner, a
/// SubPlan, an InitPlan or another CTE's plan (`IterDesc::thin`, D2230: a counted pull asks for the rows it owes only);
/// `through`, read through another CTE's plan no breaker reads whole, or streamed by another union's generations
/// (`IterDesc::through`, D2230: a counted pull refuses); `zero`, the Limit a Sort or Agg reading the union whole sits under
/// (`IterDesc::zero`); `whole`, no reader may stop before the union's end (`IterDesc::read_whole`, with#114); `top`, a
/// CteScan of the union is read on the root path with no breaker reading it whole (`IterDesc::pull_loud`, D2732); the
/// recursive unions whose seed must be pulled (`Reach::seed_pulls`, D3373); the window that must take the pulled fill
/// (`IterDesc::window`, D3373); an Append under a stop reads it (`IterDesc::arms`, D3373); `raised`, every reader that
/// may stop before the union's end stops only by raising (an EXPR, ROWCOMPARE or MULTIEXPR InitPlan reading it whole
/// otherwise: D2732's seed wall then holds for calls, not for raises); `Err`: the wall.
fn readers<'p>(b: &Builder<'p, '_>, ru: &'p RecursiveUnion<'p>) -> Result<(bool, bool, Option<i32>, bool, bool, u64, Option<i32>, bool, bool, bool, bool), &'static str> {
    let stmt = b.stmt;
    let other = "a recursive union read other than by CteScans under a LIMIT, a projection, a Unique, a Sort, an Agg, a ModifyTable, a hash or merge join PG runs as far or a CTE PG runs to its end (a nested loop's inner, a build or a merge PG may skip, a window not over a sort, a Material, an Append PG may stop, a gate, a SubPlan's tree but an InitPlan the root's one row reads, a CTE read under a LIMIT): not built yet (D2223)";
    let at = stmt.subplans.iter().position(|r| r.and_then(|r| r.as_plan()).is_some_and(|p| p.plan_node_id == ru.plan.plan_node_id));
    let Some(plan_id) = at.map(|i| i as i32 + 1) else { return Err(other) };
    let mut scans = 0;
    crate::rerun::each_plan_node(stmt, &mut |n| {
        if n.node_tag() == NodeTag::T_CteScan && n.as_cte_scan().is_some_and(|c| c.ctePlanId == plan_id) {
            scans += 1;
        }
    });
    // a CteScan in a SubPlan's tree is never reached from the root (but in an InitPlan `root_reads` names), so a count short
    // of `scans` refuses
    let mut r = Reach { stmt, facts: b.facts, arena: b.arena, partial: b.graph.partial, plan_id, found: 0, thin: false, through: false, entered: 0, stopped: false, zero: None, early: false, top: false, root: false, seed_pulls: 0, window: None, lazy_inner: false, arms: false, arms_later: false };
    // D2212: PG runs a data-modifying CTE to its end whether a reader reads it or not (ExecPostprocessPlan), so its plan is
    // walked first, and the statement's tree then passes over its CteScans
    let mut walked = Ok(());
    for (i, n) in stmt.subplans.iter().enumerate() {
        let Some(n) = n.filter(|n| n.node_tag() == NodeTag::T_ModifyTable) else { continue };
        if walked.is_ok() && r.holds(Some(n)) {
            walked = r.enter(i as i32 + 1, At { whole: true, ..At::default() });
        }
    }
    if walked.is_ok() && r.holds(stmt.planTree) {
        walked = r.walk(stmt.planTree, At { top: true, ..At::default() });
    }
    // D2732 (`raised`): a reader that may stop before the union's end other than an EXPR / ROWCOMPARE / MULTIEXPR
    // InitPlan whose own plan reads the union whole (ExecSetParamPlan reads such a plan to its end or raises 21000 at a
    // second row: it stops only by raising)
    let mut stops = r.early;
    // an InitPlan the root's one row reads (`root_reads`), walked as PG's ExecSetParamPlan reads it: EXISTS to its first
    // row, EXPR / ROWCOMPARE / MULTIEXPR to a second (21000), ARRAY to its end; the union read through it is thin.
    // with#32: an InitPlan attached anywhere else (a qual's or a target list's param of a node below the root) likewise:
    // PG runs it once, at the first evaluation that reads its param (ExecSetParamPlan from ExecEvalParamExec), and the
    // lowering's InitPlan runs at its first read too (nest.initplan; never run ahead when it holds a recursive union,
    // lowerv4 frame/on_demand.rs `eager_reader`), so both read the union as far as the InitPlan's plan reads it, or neither
    // (walked in place, in plan order: decompose allocates nothing once warm, core_zero_alloc)
    crate::rerun::each_plan_node(stmt, &mut |n| {
        let Some(p) = n.as_plan() else { return };
        for s in p.initPlan.iter().filter_map(|i| i.as_sub_plan()) {
            if walked.is_err() {
                return;
            }
            let Some(sr) = crate::rerun::subplan_root(stmt, s.plan_id).filter(|&n| s.subLinkType != SubLinkType::CTE_SUBLINK && r.holds(Some(n))) else { continue };
            let root = stmt.planTree.is_some_and(|root| root_reads(b, root, s));
            if !root && !read_at_first_eval(b, s) {
                walked = Err(());
                return;
            }
            r.thin = true;
            let raising = matches!(s.subLinkType, SubLinkType::EXPR_SUBLINK | SubLinkType::ROWCOMPARE_SUBLINK | SubLinkType::MULTIEXPR_SUBLINK);
            if raising {
                // its plan walked as an ARRAY sublink's (to its end) on a copy: no CteScan of the union found early there
                let mut whole = r;
                whole.early = false;
                stops |= whole.walk(Some(sr), At::default()).is_err() || whole.early;
            }
            let was = r.early;
            walked = r.walk(Some(sr), At { early: s.subLinkType != SubLinkType::ARRAY_SUBLINK, ..At::default() });
            stops |= !raising && r.early && !was;
        }
    });
    // setop_rec-0043: a SubPlan's tree (a SubPlan in a plan node's target list or qual), read at each call as a stopping
    // reader (an EXPR or EXISTS reads to its first or second row): PG re-runs a union its parameters reach at every call
    // (ExecReScanCteScan's tuplestore_clear) and reads it lazily, as the lowering's pull re-armed by the SubPlan's rerun set
    // does (lowerv4 ops/ctescan/lower.rs `rearms`); a union no parameter reaches keeps its store across calls in both
    if walked.is_ok() && r.found < scans {
        crate::rerun::each_plan_node(stmt, &mut |n| {
            let Some(p) = n.as_plan() else { return };
            for e in p.targetlist.iter().chain(p.qual.iter()) {
                each_subplan(e, &mut |s| {
                    let Some(sr) = crate::rerun::subplan_root(stmt, s.plan_id).filter(|&n| walked.is_ok() && r.holds(Some(n))) else { return };
                    r.thin = true;
                    walked = r.walk(Some(sr), At { early: true, ..At::default() });
                    stops |= r.early;
                });
            }
        });
    }
    match walked {
        // D3373: no CteScan reads the union at all (a constant-false gate the planner put over the reader: PG's plan holds the
        // CTE's InitPlan and no scan of it), so neither PG nor the lowering ever runs it (a CTE's plan runs only at its
        // readers' pulls, ops/ctescan)
        Ok(()) if r.found == scans => Ok((r.thin, r.through, r.zero, !r.early, r.top, r.seed_pulls, r.window, r.arms, r.arms_later, r.early && !stops, r.root)),
        _ if r.lazy_inner => Err(LAZY_INNER),
        _ if r.stopped => Err("a recursive union read through another CTE's plan PG may stop before its end (a reader under a LIMIT, a probe an empty build may end, a window): the lowering runs that plan's seed and its once-run builds whole where PG streams the seed and skips a build over an empty work table, so a stopping reader of a non-terminating one never returns (D2212)"),
        _ => Err(other),
    }
}

/// D2223 (setop P6, an InitPlan reader): the statement's root is a Result with no input, no gate and no qual whose target
/// list raises nothing (F5) and reads InitPlan `s`'s parameter. PG projects that one row, so it runs `s` at the read, once
/// (ExecSetParamPlan), as the lowering's InitPlan runs at its first read (nest.initplan; a run ahead, D1035, starts with
/// its one reader, which reads it): the union in `s` is read as far as `s` reads it in both, never skipped by one alone.
fn root_reads<'p>(b: &Builder<'p, '_>, root: Node<'p>, s: &SubPlan<'p>) -> bool {
    let Some(res) = root.as_result() else { return false };
    let p = &res.plan;
    if p.lefttree.is_some() || res.resconstantqual.is_some() || !p.qual.is_nil() {
        return false;
    }
    let quiet = p.targetlist.iter().all(|te| crate::rerun::of_expr(b.stmt, b.facts, b.arena, te).raise_free);
    quiet && p.targetlist.iter().any(|te| reads_param(te, &s.setParam))
}

/// with#32 (setop P6, an InitPlan reader below the root, as `root_reads` for the root's one row): InitPlan `s` is run at
/// the first evaluation of a plan node's target list or qual reading its param, in PG (ExecSetParamPlan from
/// ExecEvalParamExec) and in the lowering alike: some node's target list or qual reads it, and every such node's target
/// list and qual raise nothing (the effects reader's F5 fact, `of_expr`), so neither takes a raise the other runs `s`
/// after. A param read elsewhere (a join's, an index's or a gate's qual) finds no reader here and refuses.
fn read_at_first_eval<'p>(b: &Builder<'p, '_>, s: &SubPlan<'p>) -> bool {
    let (mut found, mut free) = (false, true);
    crate::rerun::each_plan_node(b.stmt, &mut |n| {
        let Some(p) = n.as_plan() else { return };
        if p.targetlist.iter().chain(p.qual.iter()).any(|e| reads_param(e, &s.setParam)) {
            found = true;
            free &= p.targetlist.iter().chain(p.qual.iter()).all(|e| crate::rerun::of_expr(b.stmt, b.facts, b.arena, e).raise_free);
        }
    });
    found && free
}

/// Each SubPlan in expression tree `n` (`readers`' SubPlan trees).
fn each_subplan<'p>(n: Node<'p>, f: &mut dyn FnMut(&'p SubPlan<'p>)) {
    crate::walk::stack_check(); // D3748
    if let Some(s) = n.as_sub_plan() {
        f(s);
    }
    crate::expr::kids(n, &mut |c| each_subplan(c, f));
}

/// Expression `n` reads one of exec parameters `ids`.
fn reads_param(n: Node<'_>, ids: &IntList<'_>) -> bool {
    crate::walk::stack_check(); // D3748
    if n.as_param().is_some_and(|p| p.paramkind == ParamKind::PARAM_EXEC && ids.iter().any(|i| i == p.paramid)) {
        return true;
    }
    let mut hit = false;
    crate::expr::kids(n, &mut |c| hit |= reads_param(c, ids));
    hit
}

/// `readers`' path state from the statement's root down: `stop`, a LIMIT above may stop before its first row; `whole`, a
/// breaker above reads the rows whole; `window`, a window above with no such breaker on its path to the root or the CteScan;
/// `early`, a LIMIT or a probe an empty build ends (an INNER or SEMI hash join's outer) above with no such breaker under it:
/// PG may stop before the rows end.
#[derive(Clone, Copy, Default)]
struct At {
    stop: bool,
    whole: bool,
    window: bool,
    early: bool,
    /// K11.rec.l0 / .gl: the one Limit that set `stop` (its count may be 0), when no other did
    limit: Option<i32>,
    /// D2732: under a Limit whose count is a constant of at most 0: nothing below is read (PG's ExecLimit returns before
    /// its first pull, or raises a negative count first; the lowering's Limit check stops the reader before its pull)
    none: bool,
    /// D2732 (cursor form): on the statement's own root path (not an InitPlan's or a data-modifying CTE's), where a pull that
    /// may count its rows stops the read
    top: bool,
    /// D3373 (:99): `early` holds only for INNER or SEMI hash probes above (`hash_join`), each ended by its empty table alone
    probe: bool,
}

/// `readers`' walk over the CteScans of the union `plan_id`: `found` counts those reached on a served path, `thin` notes a
/// qual or a join that drops rows with no breaker over it, or a read through another CTE's plan; `through`, a read through
/// another CTE's plan no breaker reads whole (or streamed by another union's generations); `entered`, the CTE plans
/// (bit = plan id) walked once (PG runs a CTE's plan once for all its readers); `stopped`, the walk refused at a CteScan of
/// another CTE holding the union that PG may stop reading before its end (D2212); `early`, a CteScan of the union found
/// where PG may stop before the union's end (`At::stop` or `At::early` on its path).
#[derive(Clone, Copy)]
struct Reach<'p, 'f> {
    stmt: &'p types_nodes::plannodes::PlannedStmt<'p>,
    facts: &'f dyn crate::catalog::Facts,
    arena: gravity_program::ArenaRef,
    /// D3373 (E-3): the statement runs under a partial pull (`PipelineGraph::partial`: a cursor's FETCH, a counted portal)
    partial: bool,
    plan_id: i32,
    found: usize,
    thin: bool,
    through: bool,
    entered: u64,
    stopped: bool,
    /// K11.rec.l0 / .gl: the Limit a Sort or Agg reading the union whole sits under (`IterDesc::zero`)
    zero: Option<i32>,
    early: bool,
    /// D2732 (cursor form): a CteScan of the union was found on the root path with no breaker reading it whole over it
    /// (`IterDesc::pull_loud`)
    top: bool,
    /// D4295: a CteScan of the union was found on the root path, a breaker reading it whole over it or not (`IterDesc::seed_exact`)
    root: bool,
    /// D3373 (E-1): the recursive unions (bit = plan id) whose seed reads this union as far as their reader reads
    /// (`enter_seeded`): each must pull its seed (`Builder::seed_pulls`)
    seed_pulls: u64,
    /// D3373 (D-a): the WindowAgg straight over a CteScan of the union that reads it as PG's spool_tuples does
    /// (`IterDesc::window`)
    window: Option<i32>,
    /// D3373 (D-d): the walk refused at a merge join whose outer is another input over the union on its inner (`LAZY_INNER`)
    lazy_inner: bool,
    /// D3373 (D-e): an Append under a reader that may stop reads the union in one of its arms (`append_stops`, `IterDesc::arms`)
    arms: bool,
    /// D3373 (:99): a later arm of such an Append reads it too (`append_stops`, `IterDesc::arms_later`)
    arms_later: bool,
}

impl<'p> Reach<'p, '_> {
    fn scan(&self, n: Node<'_>) -> bool {
        n.node_tag() == NodeTag::T_CteScan && n.as_cte_scan().is_some_and(|c| c.ctePlanId == self.plan_id)
    }

    /// The plan of another CTE `n` reads (a CteScan of a CTE other than the union).
    fn other_cte(&self, n: Node<'p>) -> Option<(i32, Node<'p>)> {
        let id = n.as_cte_scan().map(|c| c.ctePlanId).filter(|&id| id != self.plan_id)?;
        crate::rerun::subplan_root(self.stmt, id).map(|r| (id, r))
    }

    /// Plan subtree `n` (its children, a SubqueryScan's plan, the plan of a CTE it reads not yet walked) holds a CteScan of
    /// the union.
    fn holds(&self, n: Option<Node<'p>>) -> bool {
        crate::walk::stack_check(); // D3748
        let Some(n) = n else { return false };
        let Some(p) = n.as_plan() else { return false };
        self.scan(n)
            || self.other_cte(n).is_some_and(|(id, r)| !self.entered(id) && self.holds(Some(r)))
            || [p.lefttree, p.righttree, n.as_subquery_scan().and_then(|s| s.subplan)].into_iter().any(|c| self.holds(c))
            || n.as_append().is_some_and(|a| a.appendplans.iter().any(|c| self.holds(Some(c))))
    }

    /// `n` sorts the union's stream: its first row comes only once the union ended.
    fn sorts(&self, n: Option<Node<'_>>) -> bool {
        n.is_some_and(|n| n.node_tag() == NodeTag::T_Sort && self.streams(n.as_plan().and_then(|p| p.lefttree)))
    }

    /// `n` is a CteScan of the union under only projections: PG pulls it row by row, the union's rows in its order.
    fn streams(&self, n: Option<Node<'_>>) -> bool {
        crate::walk::stack_check(); // D3748
        let Some(n) = n else { return false };
        match n.node_tag() {
            NodeTag::T_Result if n.as_result().is_some_and(|r| r.resconstantqual.is_none()) => self.streams(n.as_plan().and_then(|p| p.lefttree)),
            NodeTag::T_SubqueryScan => self.streams(n.as_subquery_scan().and_then(|s| s.subplan)),
            _ => self.scan(n),
        }
    }

    fn entered(&self, id: i32) -> bool {
        u32::try_from(id).ok().and_then(|i| 1u64.checked_shl(i)).is_some_and(|bit| self.entered & bit != 0)
    }

    /// D2212: walk CTE plan `id` once, PG running it to its end (`readers`); its root reads as the statement's does, under a
    /// breaker where `at` has one. The union read through it is thin, and read through (`through`) unless that breaker
    /// reads the plan whole: a pull that may stop runs the plan's seed whole first, where PG stops with its reader.
    fn enter(&mut self, id: i32, at: At) -> Result<(), ()> {
        let bit = 1u64.checked_shl(u32::try_from(id).map_err(|_| ())?).ok_or(())?;
        if self.entered & bit != 0 {
            return Ok(());
        }
        self.entered |= bit;
        self.thin = true;
        self.through |= !at.whole;
        let root = crate::rerun::subplan_root(self.stmt, id).ok_or(())?;
        self.walk(Some(root), At { whole: at.whole, top: at.top, ..At::default() })
    }

    /// D2034: walk the plan of CTE `id`, recursive union `outer` whose generations stream this union's CTE (`gen_stream`),
    /// under a reader that may stop (`at`): its term reads this union as far as that reader reads (`At::early`); a seed
    /// holding it is not the shape.
    fn enter_streamed(&mut self, id: i32, outer: &'p RecursiveUnion<'p>, at: At) -> Result<(), ()> {
        let bit = 1u64.checked_shl(u32::try_from(id).map_err(|_| ())?).ok_or(())?;
        if self.entered & bit != 0 {
            return Ok(());
        }
        if self.holds(outer.plan.lefttree) {
            return Err(());
        }
        self.entered |= bit;
        self.thin = true;
        self.through = true;
        self.walk(outer.plan.righttree, At { early: true, none: at.none, top: at.top, ..At::default() })
    }

    /// D3373 (E-1): walk the seed of recursive union `outer` (CTE `id`), which alone holds this union's CteScan, under a
    /// reader that may stop (`at`): read as far as that reader reads (`At::early`), the union read through it. `outer`'s
    /// own mapping refuses by D2212's name unless it pulls its seed (`seed_pulls`, `Builder::seed_pulls`): run whole, the
    /// seed would read this union to its end where PG stops.
    fn enter_seeded(&mut self, id: i32, outer: &'p RecursiveUnion<'p>, at: At) -> Result<(), ()> {
        let bit = 1u64.checked_shl(u32::try_from(id).map_err(|_| ())?).ok_or(())?;
        if self.entered & bit != 0 {
            return Ok(());
        }
        self.entered |= bit;
        self.seed_pulls |= bit;
        // no `through`: `outer` pulls its seed (or refuses), so a pull that counts its rows reads this union no further
        self.thin = true;
        self.walk(outer.plan.lefttree, At { early: true, none: at.none, top: at.top, ..At::default() })
    }

    /// D2212 (Hibernate's jsonb_mergepatch emulation: three recursive unions, `res` reading `val0` and `val1` in its seed and
    /// its term, read by an EXPR SubLink's InitPlan): walk recursive union `outer` (CTE `id`), whose reader surely pulls it
    /// at least once but may stop after (an EXPR / EXISTS InitPlan, a LIMIT of a constant count of at least one; the caller's
    /// `sure`). PG's first pull runs the seed to its first row (ExecRecursiveUnion pulls its non-recursive term before
    /// anything else), and where that row comes only once a Sort or a plain or hashed Agg has read its whole input
    /// (`first_pull_whole`) holding every CteScan of this union in the seed, read whole there (the walk finds none `early`),
    /// that first pull reads this union to its end. Its store is then complete before `outer` hands on a row, so the term
    /// reads a finished store (walked whole), and the lowering's run of the seed and its once-run builds reads this union no
    /// further than PG does: neither returns over a union that never ends (a stop on the term's path reads no less than
    /// the seed did). The union is read through `outer`'s plan
    /// (`thin`) with a breaker reading it whole (no `through`). `Ok(false)`: not that shape, nothing recorded (the caller's
    /// wall); an InitPlan in the seed reading this union is `readers`' own walk.
    fn enter_first_whole(&mut self, id: i32, outer: &'p RecursiveUnion<'p>) -> Result<bool, ()> {
        let bit = 1u64.checked_shl(u32::try_from(id).map_err(|_| ())?).ok_or(())?;
        if self.entered & bit != 0 {
            return Ok(true);
        }
        if !self.first_pull_whole(outer.plan.lefttree) {
            return Ok(false);
        }
        let saved = *self;
        self.entered |= bit;
        self.thin = true;
        self.early = false;
        let whole = At { whole: true, ..At::default() };
        // the seed: every CteScan of this union read whole (none `early`); the term then reads the finished store, so a
        // stop on its path (an INNER probe's empty table, a merge join ending at one side) reads no less of the union than
        // PG's seed already did: its CteScans are counted (`count_finished`)
        let seed = self.walk(outer.plan.lefttree, whole).is_ok() && !self.early;
        let term = seed && self.count_finished(outer.plan.righttree).is_ok();
        if !term || self.window.is_some() != saved.window.is_some() {
            *self = saved;
            return Ok(false);
        }
        self.early = saved.early;
        Ok(true)
    }

    /// D2212 (`enter_first_whole`): count the CteScans of the union under plan `n`, a recursive union's term read once the
    /// union's store is complete: its children, a SubqueryScan's plan, an Append's or a MergeAppend's members. Another CTE's
    /// plan holding the union is not this shape (`Err`: the caller's wall).
    fn count_finished(&mut self, n: Option<Node<'p>>) -> Result<(), ()> {
        crate::walk::stack_check(); // D3748
        let Some(n) = n else { return Ok(()) };
        if self.scan(n) {
            self.found += 1;
            return Ok(());
        }
        if self.other_cte(n).is_some_and(|(id, r)| !self.entered(id) && self.holds(Some(r))) {
            return Err(());
        }
        let p = n.as_plan().ok_or(())?;
        for c in [p.lefttree, p.righttree, n.as_subquery_scan().and_then(|s| s.subplan)] {
            self.count_finished(c)?;
        }
        if let Some(arms) = n.as_append().map(|a| &a.appendplans).or(n.as_merge_append().map(|a| &a.mergeplans)) {
            for c in arms.iter() {
                self.count_finished(Some(c))?;
            }
        }
        Ok(())
    }

    /// D2212 (`enter_first_whole`): PG's first pull of plan `n` reaches a node that reads its whole input before its first
    /// row through projections alone: a Sort, a plain or hashed Agg, or a GroupAggregate over one (it hands on its first
    /// group only once its sorted input gave the next group's first row; a mixed Agg's sorted phase runs first, the same).
    fn first_pull_whole(&self, n: Option<Node<'p>>) -> bool {
        crate::walk::stack_check(); // D3748
        let Some(n) = n else { return false };
        let Some(p) = n.as_plan() else { return false };
        match n.node_tag() {
            NodeTag::T_Result if n.as_result().is_some_and(|r| r.resconstantqual.is_none()) => self.first_pull_whole(p.lefttree),
            NodeTag::T_SubqueryScan => self.first_pull_whole(n.as_subquery_scan().and_then(|s| s.subplan)),
            NodeTag::T_Agg if n.as_agg().is_some_and(|a| a.aggstrategy == AGG_SORTED || a.aggstrategy == crate::ops::shared::agg::AGG_MIXED) => {
                self.first_pull_whole(p.lefttree)
            }
            NodeTag::T_Sort | NodeTag::T_Agg => true,
            _ => false,
        }
    }

    /// D3373 (E-2): walk plain CTE plan `root` (CTE `id`), a pipeline its readers pull, under a reader that may stop (`at`):
    /// read as far as that reader reads (`At::early`), the union read through it.
    fn enter_lazy(&mut self, id: i32, root: Node<'p>, at: At) -> Result<(), ()> {
        let bit = 1u64.checked_shl(u32::try_from(id).map_err(|_| ())?).ok_or(())?;
        if self.entered & bit != 0 {
            return Ok(());
        }
        self.entered |= bit;
        self.thin = true;
        self.through = true;
        self.walk(Some(root), At { early: true, none: at.none, top: at.top, ..At::default() })
    }

    fn walk(&mut self, n: Option<Node<'p>>, mut at: At) -> Result<(), ()> {
        crate::walk::stack_check(); // D3748
        let node = n.ok_or(())?;
        let p = node.as_plan().ok_or(())?;
        self.thin |= !at.whole && !p.qual.is_nil();
        let next = match node.node_tag() {
            NodeTag::T_CteScan if self.scan(node) => {
                self.found += 1;
                self.early |= !at.none && (at.stop || at.early);
                self.top |= at.top && !at.whole && !at.none;
                self.root |= at.top && !at.none;
                return if at.window { Err(()) } else { Ok(()) };
            }
            // D2212: another CTE's plan holding a CteScan of the union, walked once where PG runs it to its end (`readers`)
            NodeTag::T_CteScan => {
                let (id, root) = self.other_cte(node).ok_or(())?;
                if self.entered(id) {
                    return Ok(());
                }
                if !self.holds(Some(root)) {
                    return Err(());
                }
                let dml = root.node_tag() == NodeTag::T_ModifyTable;
                // D2034: a recursive union whose term streams this union's CTE (ops/setop/gen_stream.rs) pulls it one
                // budget at a time as its own reader reads, as PG's term pulls the CteScan one row per call: read as far
                // as that reader reads, never to its end
                let streams = root.as_recursive_union().filter(|o| gen_stream::source(self.stmt, o) == Some(self.plan_id));
                let stops = !dml && !at.window && (at.stop || (at.early && !at.whole));
                if let Some(outer) = streams.filter(|_| stops) {
                    return self.enter_streamed(id, outer, at);
                }
                // D2212 (Hibernate's jsonb_mergepatch): a recursive union whose seed reads this union whole before its first
                // row, under a reader that surely pulls it once: read whole there, so its seed needs no pull (E-1 below)
                let sure = at.early && !at.stop && !at.window && !at.probe && !at.none && !(self.partial && at.top);
                if let Some(outer) = root.as_recursive_union().filter(|_| !dml && sure && !at.whole) {
                    if self.enter_first_whole(id, outer)? {
                        return Ok(());
                    }
                }
                // D3373 (E-1): a recursive union whose seed alone holds this union's CteScan pulls that seed a budget at a
                // time under a reader that may stop (D2034's seed cliff, `IterDesc::seed`), as PG's ExecRecursiveUnion pulls
                // its non-recursive term row by row: read as far as that reader reads
                let seeded = root.as_recursive_union().filter(|o| self.holds(o.plan.lefttree) && !self.holds(o.plan.righttree));
                // D3373 (E-3): a partial pull on the root path (a cursor's FETCH) stops there too: `outer` pulls its seed
                // under it (map_recursive_union `must_pull`)
                let counted = !dml && !at.window && self.partial && at.top && !at.whole && !at.none;
                if let Some(outer) = seeded.filter(|_| stops || counted) {
                    return self.enter_seeded(id, outer, at);
                }
                // D3373 (E-2): a plain CTE whose plan reaches this union's CteScan through projections and quals only
                // (`streams`) is one pipeline its readers pull (ops/ctescan's pull form), nesting this union's pull: read
                // as far as its reader reads
                if stops && root.as_recursive_union().is_none() && self.streams(Some(root)) {
                    return self.enter_lazy(id, root, at);
                }
                if !dml && (at.stop || at.window || (at.early && !at.whole)) {
                    self.stopped = true;
                    return Err(());
                }
                return self.enter(id, if dml { At { whole: true, ..at } } else { at });
            }
            // another recursive union's plan (`enter`): PG runs its seed to its end, then its term at least once, each
            // generation's to its end (nodeRecursiveunion.c); the lowering runs the seed and each generation whole
            NodeTag::T_RecursiveUnion => {
                for c in [p.lefttree, p.righttree] {
                    if self.holds(c) {
                        self.walk(c, at)?;
                    }
                }
                return Ok(());
            }
            NodeTag::T_Limit => {
                (at.early, at.probe) = (true, false);
                let may_stop = node.as_limit().is_none_or(|l| !matches!(crate::ops::limit_bounds(l).0, Bound::All) && !matches!(crate::ops::limit_bounds(l).0, Bound::Const(c) if c >= 1));
                if may_stop {
                    at.limit = (!at.stop).then_some(p.plan_node_id);
                }
                at.none |= node.as_limit().is_some_and(|l| matches!(crate::ops::limit_bounds(l).0, Bound::Const(c) if c <= 0));
                at.stop |= may_stop;
                p.lefttree
            }
            NodeTag::T_Result if node.as_result().is_some_and(|r| r.resconstantqual.is_none()) => p.lefttree,
            // a one-time gate over a Sort or an Agg: read whole or not at all, as under a LIMIT that may count 0
            NodeTag::T_Result if !at.stop && !at.whole && !at.window && whole_below(p.lefttree) => {
                (at.early, at.probe, at.stop, at.limit) = (true, false, true, Some(p.plan_node_id));
                p.lefttree
            }
            NodeTag::T_SubqueryScan => node.as_subquery_scan().and_then(|s| s.subplan),
            NodeTag::T_Unique => p.lefttree,
            // D3373: a ProjectSet (nodeProjectSet.c) pulls its input a row at a time, as its parent pulls, and ends a row's set
            // before the next; under a breaker that reads it whole both read the union to its end
            NodeTag::T_ProjectSet if at.whole => p.lefttree,
            // D3374: a Material (nodeMaterial.c) pulls its input a row at a time as its parent pulls, storing each, never
            // ahead; where the union is read whole (a merge join's inner the planner materialized over a Sort) so is it
            NodeTag::T_Material if at.whole => p.lefttree,
            // D3270: a LockRows (nodeLockRows.c) pulls its subplan one row per call as its parent pulls and never stops it on
            // its own, as the lowering's lock op does; a row whose lock finds it gone, moved away or (SKIP LOCKED) held is
            // dropped, as by a qual. Its EvalPlanQual recheck reads the CteScan's ROW_MARK_COPY row, never the union again.
            NodeTag::T_LockRows => {
                self.thin |= !at.whole;
                p.lefttree
            }
            // setop_rec-0060: a window PG surely reads whole before its first row (`facts::window::reads_whole`) is such a breaker
            NodeTag::T_WindowAgg if !at.stop && node.as_window_agg().is_some_and(crate::facts::window::reads_whole) => {
                (at.whole, at.window, at.early, at.probe) = (true, false, false, false);
                p.lefttree
            }
            // D3373 (D-a): a window straight over a CteScan (projections and quals between: `cte_below`) of the union or of
            // another CTE holding it, that PG does not read whole before its first row, reads it as PG's spool_tuples does,
            // row by row as its frames need, wherever a reader or its run condition stops it: lowerv4's W10 pulled fill
            // (ops/window/lower.rs `streaming_fill` over a recursive union's pulled store) reads no further; lowerv4 refuses
            // by D2223's name where the window does not take that fill (`IterDesc::window`). One such window per union.
            NodeTag::T_WindowAgg if !at.whole && !at.window && self.window.is_none() && cte_below(p.lefttree) => {
                self.window = Some(p.plan_node_id);
                (at.early, at.probe) = (true, false);
                p.lefttree
            }
            // a run condition may end PG's window before it reads the union whole, wherever the window sits
            NodeTag::T_WindowAgg => {
                at.window |= !at.whole || node.as_window_agg().is_none_or(|w| !w.runCondition.is_nil());
                p.lefttree
            }
            // D3373 (G-2): an Incremental Sort (nodeIncrementalSort.c) reads its input a presorted group at a time, ahead
            // of its reader by at most one group; decompose maps it as a Sort (ops/sort), read whole before its first row.
            // Where its reader may stop before its end, it is served only over a breaker that reads the union whole anyway
            // (`At::window`: no CteScan before a Sort, an Agg or a whole window), as PG's then reads it whole too
            NodeTag::T_IncrementalSort => {
                at.window |= !at.whole;
                p.lefttree
            }
            NodeTag::T_Sort | NodeTag::T_Agg | NodeTag::T_ModifyTable if !at.stop => {
                (at.whole, at.window, at.early, at.probe) = (true, false, false, false);
                p.lefttree
            }
            // K11.rec.l0 / .gl: a Sort or an Agg under one LIMIT that may count 0 reads the union whole once PG's Limit
            // pulls it, and PG pulls it only at a count above 0 (ExecLimit evaluates its bounds first and never pulls its
            // subplan at a zero count): whole or not at all, as the lowering's where the Limit's zero check stops the
            // union's reader before its first pull (`IterDesc::zero`, checked there)
            NodeTag::T_Sort | NodeTag::T_Agg if at.limit.is_some() && self.zero.is_none_or(|z| Some(z) == at.limit) => {
                self.zero = at.limit;
                (at.whole, at.window, at.early, at.stop, at.limit, at.probe) = (true, false, false, false, None, false);
                p.lefttree
            }
            NodeTag::T_HashJoin => return self.hash_join(node, at),
            // a nested loop (nodeNestloop.c) reads its outer row by row as its parent pulls, whatever its inner holds; the
            // union on its outer side only (an inner over it is rescanned per outer row)
            NodeTag::T_NestLoop if !self.holds(p.righttree) => p.lefttree,
            // subselect#208/209: the union in a nested loop's inner, rescanned at each outer row (ExecReScan): PG re-runs a
            // union the nest's parameters reach at each rescan (ExecReScanCteScan's tuplestore_clear) and reads it lazily as
            // far as the join reads its inner (to its end for an inner or left join, to a first match for a semi or anti
            // join, never past where a parent stops), as the lowering's pull re-armed by the inner's rerun set does
            // (setop_rec-0043's form); a union no parameter reaches keeps its store across rescans in both. The inner is such
            // a stopping reader; the outer reads as above.
            NodeTag::T_NestLoop => {
                if self.holds(p.lefttree) {
                    self.walk(p.lefttree, at)?;
                }
                self.thin = true;
                return self.walk(p.righttree, At { early: true, whole: false, probe: false, ..at });
            }
            // a Memoize in that inner (nodeMemoize.c): a hit replays the entry and never rescans its child, a miss reads the
            // child as far as the consumer reads and stores each row as it goes; the lowering's nest.memo is that (lowerv4
            // ops/memoize: the fill stores a row as it is handed on, never ahead), so the union runs at a miss only, read as
            // far as PG reads it
            NodeTag::T_Memoize if at.early => p.lefttree,
            NodeTag::T_MergeJoin => return self.merge_join(node, at),
            NodeTag::T_SetOp => return self.set_op(node, at),
            // an Append (nodeAppend.c) reads its arms in order, each to its end before the next: where nothing above may
            // stop it, PG reads every arm to its end; run-time pruning may skip one (refused)
            NodeTag::T_Append if (at.stop || at.early) && node.as_append().is_some_and(|a| a.part_prune_index < 0) => return self.append_stops(node, at),
            NodeTag::T_Append => {
                let a = node.as_append().ok_or(())?;
                if at.stop || at.early || a.part_prune_index >= 0 {
                    return Err(());
                }
                for c in a.appendplans.iter() {
                    if self.holds(Some(c)) {
                        self.walk(Some(c), at)?;
                    }
                }
                return Ok(());
            }
            _ => return Err(()),
        };
        self.walk(next, at)
    }

    /// A hash join over the union (nodeHashjoin.c). The probe side: PG reads the outer as its parent pulls rows, and an
    /// INNER or SEMI join's empty table ends it before the rest of the outer; the lowering's probe ends its pipeline at an
    /// empty build too (hashjoin D237), and LEFT and ANTI read every outer row in both. The build side: the Hash reads its
    /// input whole once PG builds, at the join's first call unless PG fetches the outer's first row before it builds
    /// (`join::outer_first`), and then only if one comes; the lowering builds there lazily too (hashjoin's lazy build,
    /// D824: on PG's outer-first path the build waits for the outer's first row and an empty outer never builds), so a
    /// build over the union reads it as far as PG on either path, under no LIMIT PG may stop before its first row (a build
    /// that reads a recursive union takes the lazy road whatever the join type: hashjoin D2635).
    fn hash_join(&mut self, node: Node<'p>, at: At) -> Result<(), ()> {
        let hj = node.as_hash_join().ok_or(())?;
        let (outer, hash) = (hj.join.plan.lefttree, hj.join.plan.righttree.and_then(|n| n.as_hash()).ok_or(())?);
        self.thin |= !at.whole;
        if self.holds(outer) {
            if !matches!(hj.join.jointype, JoinType::JOIN_INNER | JoinType::JOIN_SEMI | JoinType::JOIN_LEFT | JoinType::JOIN_ANTI | JoinType::JOIN_RIGHT | JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT_ANTI) {
                return Err(());
            }
            // a RIGHT join hands a matched inner row on as its probe meets it, and the rest at the end (a Right Semi /
            // Right Anti join's drain, D1922): under a LIMIT PG may stop the outer at a row the lowering's drain reads past
            let right = matches!(hj.join.jointype, JoinType::JOIN_RIGHT | JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT_ANTI);
            if right && (at.early || at.stop) {
                return Err(());
            }
            // an INNER, SEMI or RIGHT join's empty table ends PG's read of the outer before its end (a RIGHT join fills the
            // inner side only: PG builds first, and an empty table never reads the outer)
            let early = at.early || matches!(hj.join.jointype, JoinType::JOIN_INNER | JoinType::JOIN_SEMI | JoinType::JOIN_RIGHT | JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT_ANTI);
            // D3373 (:99): an INNER or SEMI probe's own stop (its empty table) keeps `probe` where nothing else stops above;
            // a LEFT or ANTI join stops nothing of its own
            let probe = match hj.join.jointype {
                JoinType::JOIN_INNER | JoinType::JOIN_SEMI => !at.early || at.probe,
                JoinType::JOIN_LEFT | JoinType::JOIN_ANTI => at.probe,
                _ => false,
            };
            self.walk(outer, At { early, probe, ..at })?;
        }
        if self.holds(hash.plan.lefttree) {
            if at.stop {
                return Err(());
            }
            self.walk(hash.plan.lefttree, At { whole: true, ..At::default() })?;
        }
        Ok(())
    }

    /// A merge join over the union (nodeMergejoin.c): PG fetches the outer's first row before any inner one and may end at
    /// either side's end, so it reads the union as far as the lowering (which sorts both sides) only where that first fetch
    /// reads the union whole: an outer that sorts the union itself. Each side then reads stored rows only.
    fn merge_join(&mut self, node: Node<'p>, at: At) -> Result<(), ()> {
        let p = node.as_plan().ok_or(())?;
        // D2212: a FULL merge join read whole (under a breaker PG reads to its end, nothing stopping between) reads both its
        // sides to their ends, as the lowering does: ExecMergeJoin hands on each side's unmatched rows, so it ends only once
        // both sides ended (Hibernate's jsonb_mergepatch: full joins of the argument unions, nested two deep)
        if at.whole && !at.stop && !at.early && node.as_merge_join().is_some_and(|m| m.join.jointype == JoinType::JOIN_FULL) {
            let whole = At { whole: true, ..At::default() };
            for c in [p.lefttree, p.righttree] {
                if self.holds(c) {
                    self.walk(c, whole)?;
                }
            }
            return Ok(());
        }
        // D3373 (D-d): the union on the inner side only, under a Sort, the outer another input: named (`LAZY_INNER`)
        self.lazy_inner |= !at.stop && !self.holds(p.lefttree) && self.sorts(p.righttree);
        if at.stop || !self.sorts(p.lefttree) {
            return Err(());
        }
        self.thin |= !at.whole;
        let whole = At { whole: true, ..At::default() };
        self.walk(p.lefttree, whole)?;
        if self.holds(p.righttree) {
            self.walk(p.righttree, whole)?;
        }
        Ok(())
    }

    /// D3373 (D-e): an Append under a reader that may stop (a LIMIT, a probe an empty build may end): PG reads its arms in
    /// order, each to its end before the next, and may never start a later one. The streamed union (decompose append.rs
    /// `push_union`: each arm its own pipeline ending in the parent's ops) reads them in that order too, so the union read in
    /// one arm is read as far as that reader reads (`at`). Every arm before it PG reads to its end, as the lowering does;
    /// every arm after it holds no CteScan of the union and is quiet (the lowering may open one PG never starts and read
    /// its first rows). lowerv4 refuses by name where the union settled into its spool instead (`IterDesc::arms`: a
    /// feed reads the store whole first). D3373 (:99): a later arm holding the union too, where the only stop is an INNER
    /// or SEMI probe's empty table (`At::probe`: no LIMIT, no partial pull on the root path) and the arm streams the union
    /// (a CteScan under projections): PG starts it only after the earlier arm read the union to its end, so it reads
    /// stored rows only, and an empty table ends the join before it starts. The lowering's later arm closes its call at
    /// the open over a built empty table (lowerv4 frame/iface `later_arm_probe`, checked by name in ops/setop/recursive.rs).
    fn append_stops(&mut self, node: Node<'p>, at: At) -> Result<(), ()> {
        let a = node.as_append().ok_or(())?;
        let mut held = false;
        let later = at.probe && !at.stop && !(self.partial && at.top);
        for c in a.appendplans.iter() {
            match (held, self.holds(Some(c))) {
                (false, true) => {
                    held = true;
                    self.walk(Some(c), at)?;
                }
                (true, true) if later && self.streams(Some(c)) => {
                    self.arms_later = true;
                    self.walk(Some(c), at)?;
                }
                (true, true) => return Err(()),
                (true, false) if !crate::effects::setop::quiet_in(self.stmt, self.facts, self.arena, c) => return Err(()),
                _ => {}
            }
        }
        self.arms = true;
        Ok(())
    }

    /// D3373: a SetOp (INTERSECT / EXCEPT, nodeSetOp.c) reads its outer input first: to its end into its hash table when
    /// hashed (`setop_fill_hash_table`), or its first row when sorted, so a Sort over the union there reads it whole at
    /// that first fetch. Its inner it reads only when the outer had a row (both forms: "If the outer relation is empty ...
    /// we don't need to read the inner relation at all"), so a union there is served only where the outer reads the
    /// union itself (its rows stored by then), as `merge_join`'s. Nothing above may stop or skip the SetOp's first fetch
    /// (no LIMIT, no probe an empty build may end: the lowering's Sort over the union runs ahead of its reader).
    fn set_op(&mut self, node: Node<'p>, at: At) -> Result<(), ()> {
        let p = node.as_plan().ok_or(())?;
        let hashed = node.as_set_op().ok_or(())?.strategy == SETOP_HASHED;
        if at.stop || at.early || !(self.sorts(p.lefttree) || (hashed && self.streams(p.lefttree))) {
            return Err(());
        }
        self.thin |= !at.whole;
        let whole = At { whole: true, ..At::default() };
        self.walk(p.lefttree, whole)?;
        if self.holds(p.righttree) {
            self.walk(p.righttree, whole)?;
        }
        Ok(())
    }
}

/// D3373 (D-d, a named residual): a merge join whose outer is another input reads the union on its inner side through a
/// Sort. PG fetches the outer first (ExecMergeJoin's EXEC_MJ_INITIALIZE_OUTER) and an empty outer ends the join before the
/// inner Sort's first fetch; the lowering sorts both sides ahead of the join, so over a union that never ends it never
/// returns. Served once the merge join's inner Sort starts at the outer's first row (a lazy inner, as hashjoin's D824).
const LAZY_INNER: &str = "a recursive union on a merge join's inner side whose outer is another input (PG never sorts the inner over an empty outer; the lowering sorts it ahead: a lazy merge-join inner is not built, D2223)";

/// D2312's refusal: decompose's wall, and the lowering's where a literal `srf_ends` names is not small.
pub const SRF_WALL: &str = "a recursive union whose seed or term holds a target-list set-returning function (PG streams it; the union fills each generation whole, so one whose rows never end never returns: D2312)";

/// D2312: a target-list set-returning function (PG's ProjectSet) in plan tree `n`: PG streams it, the union fills each
/// generation whole, so one whose rows never end never returns. Refused unless `srf_ends` knows each one ends.
fn project_set(n: Node<'_>) -> bool {
    crate::walk::stack_check();
    n.node_tag() == NodeTag::T_ProjectSet || plan_kids(n).any(project_set)
}

/// The plan children `project_set` and `srf_ends` walk: the outer and inner plans, a SubqueryScan's subplan, an Append's or
/// MergeAppend's arms.
fn plan_kids<'p>(n: Node<'p>) -> impl Iterator<Item = Node<'p>> {
    let kids = n.as_plan().map(|p| p.lefttree.into_iter().chain(p.righttree)).into_iter().flatten();
    let arms = n.as_append().map(|a| &a.appendplans).or(n.as_merge_append().map(|a| &a.mergeplans));
    kids.chain(n.as_subquery_scan().and_then(|s| s.subplan)).chain(arms.into_iter().flat_map(|l| l.iter()))
}

/// D3373 (D2312's finite form): whether every target-list set-returning function in plan tree `n` ends, its
/// rows bounded by constants the content key holds as words, each named to `lits` (the lowering files a literal guard that
/// each is at most `gravity_program::cost::expr::SRF_SMALL`: lowerv4 ops/setop/recursive.rs, a statement of the key with a larger one lowers again).
/// A ProjectSet ends when it holds no ProjectSet below it and each set-returning call is a builtin whose rows its
/// arguments' size bounds (unnest, generate_subscripts, the regexp, string_to_table and JSON ones: `kind_of`), a directory
/// listing (pg_ls_dir and the pg_ls_*dir family: one directory's entries, `lists_a_directory`; D4546) or an int4 /
/// int8 generate_series whose start and stop are non-NULL by-value constants (named; its step a constant: zero raises);
/// or when it sits straight under a LIMIT whose count is such a constant and whose offset is none or one (both named) over
/// builtin calls only (the LIMIT ends that pipeline after so many rows, as PG's ExecLimit stops pulling). A user, numeric
/// or time series call, a series bound that is not a constant, or a ProjectSet under another is not known to end.
fn srf_ends<'p>(n: Node<'p>, lits: &mut dyn FnMut(Node<'p>)) -> bool {
    crate::walk::stack_check(); // D3748
    use crate::ops::shared::srf::SrfKind;
    let lit = |c: Option<Node<'p>>| c.and_then(|c| c.as_const()).is_some_and(|c| c.constbyval && !c.constisnull);
    let calls = |ps: Node<'p>, lits: &mut dyn FnMut(Node<'p>), series: bool| {
        ps.as_plan().is_some_and(|p| {
            p.targetlist.iter().filter_map(|t| t.as_target_entry()).all(|t| {
                let Some((funcid, args)) = t.expr.as_func_expr().filter(|f| f.funcretset).map(|f| (f.funcid, &f.args)) else {
                    // a plain entry, or a set-returning operator (kept out)
                    return t.expr.as_op_expr().is_none_or(|o| !o.opretset);
                };
                match crate::catalog::srf::kind_of(funcid, true) {
                    SrfKind::SeriesInt { step, .. } if series => {
                        let (a, z, s) = (args.iter().next(), args.iter().nth(1), args.iter().nth(2));
                        let ok = lit(a) && lit(z) && (!step || s.is_some_and(|s| s.as_const().is_some()));
                        if ok {
                            a.into_iter().chain(z).for_each(&mut *lits);
                        }
                        ok
                    }
                    SrfKind::SeriesInt { .. } => true,
                    SrfKind::Subscripts | SrfKind::Unnest | SrfKind::RegexpMatches | SrfKind::RegexpSplit | SrfKind::StringToTable => true,
                    SrfKind::JsonbArrayElements { .. } | SrfKind::JsonbEach { .. } | SrfKind::JsonbObjectKeys => true,
                    SrfKind::JsonArrayElements { .. } | SrfKind::JsonEach { .. } | SrfKind::JsonObjectKeys | SrfKind::JsonbPathQuery => true,
                    // D4546: a directory listing ends with its directory's entries (pg_rewind's seed, pg_ls_dir('.'))
                    _ => crate::catalog::srf::lists_a_directory(funcid),
                }
            })
        })
    };
    if let Some(l) = n.as_limit() {
        if let Some(ps) = l.plan.lefttree.filter(|k| k.node_tag() == NodeTag::T_ProjectSet) {
            if lit(l.limitCount) && (l.limitOffset.is_none() || lit(l.limitOffset)) && !plan_kids(ps).any(project_set) && calls(ps, &mut |_| {}, false) {
                l.limitCount.into_iter().chain(l.limitOffset).for_each(&mut *lits);
                return true;
            }
        }
    }
    if n.node_tag() == NodeTag::T_ProjectSet {
        return !plan_kids(n).any(project_set) && calls(n, lits, true);
    }
    plan_kids(n).all(|k| srf_ends(k, lits))
}

#[inline(never)] // D3748: never inlined into `map`, whose frame a set-operation nest keeps live per level
fn map_recursive_union<'p>(b: &mut Builder<'p, '_>, ru: &'p RecursiveUnion<'p>) -> Result<Open<'p>, Refusal> {
    let (plan, k) = (&ru.plan, Kind::RecursiveUnion);
    let id = plan.plan_node_id;
    let seed = plan.lefttree.ok_or(Refusal::shape(k, id, "no non-recursive term"))?;
    let term = plan.righttree.ok_or(Refusal::shape(k, id, "no recursive term"))?;
    if !plan.qual.is_nil() {
        return Err(Refusal::shape(k, id, "a qual on RecursiveUnion"));
    }
    // the walls PG's plan decides (the lowering refuses with the first; `IterDesc::wall`)
    let mut bounds = ArenaVec::new_in(b.arena);
    let ends = srf_ends(seed, &mut |c| bounds.push(c)) && srf_ends(term, &mut |c| bounds.push(c));
    let (seed_srf, term_srf) = (project_set(seed), project_set(term));
    let srf = (!ends && (seed_srf || term_srf)).then_some(SRF_WALL);
    let (thin, through, zero, read_whole, top, root, window, arms, arms_later, raised, read_wall) = match readers(b, ru) {
        Ok((thin, through, zero, whole, top, pulls, window, arms, later, raised, root)) => {
            b.seed_pulls |= pulls;
            (thin, through, zero, whole, top, root, window, arms, later, raised, None)
        }
        Err(why) => (false, false, None, false, false, false, None, false, false, false, Some(why)),
    };
    // with#114 (append): a UNION ALL seed keeps its union's Materialize form (`sort::spool` settles it: its arms feed the
    // union's own spool, whose one emit fills the working table): no table end continues over a streamed union's arms
    // (lowerv4 ops/append/lower.rs, D393)
    let seed_start = b.graph.pipelines.len();
    let seed_open = b.plan(seed)?;
    let seed_union = seed_open.union.is_some();
    let work = match seed_open.union {
        Some(_) => crate::ops::shared::breaker::spool(b, seed_open, k, id)?,
        None => crate::ops::shared::breaker::close(b, seed_open, BreakerKind::Materialize, ROW_LAYOUT, Budget::Share, Spill::Runs, k, id)?,
    };
    // the working table's fill is the seed's (not the first term node's): a rescan that reaches the union re-runs it
    // (rerun.rs `union_seed`, PG's ExecReScanRecursiveUnion)
    b.closed_by(seed);
    let seed_end = b.graph.pipelines.len();
    // The driver opens first: its read of `work` leases it while the term is mapped.
    let mut out = crate::ops::shared::breaker::emit(b, id, work);
    // D1378: a UNION's keys; the lowering's two table fills drop a row the union already holds (no slot: the
    // set is the generation object's, not a breaker)
    loose_keys(b, &plan.targetlist, ru.dupColIdx, ru.dupCollations, ru.numCols, k, id)?;
    let dedup = eq_keys(b, &plan.targetlist, ru.dupColIdx, ru.dupOperators, ru.numCols, k, id)?;
    let dedup_rows = crate::facts::record_union::dedup_rows(b, ru, &dedup);
    let mut dedup_colls = ArenaVec::with_capacity_in(dedup.len(), b.arena);
    (0..dedup.len()).for_each(|i| dedup_colls.push(ru.dupCollations.get(i).copied().unwrap_or(0)));
    b.work.push((ru.wtParam, work));
    let term_start = b.graph.pipelines.len();
    let outer_term = b.graph.term_from; // an enclosing term's hold, from its earlier start, covers ours
    b.graph.term_from = Some(outer_term.map_or(term_start as u16, |t| t.min(term_start as u16)));
    let body = b.plan(term);
    b.work.pop();
    let body = crate::ops::shared::breaker::respool(b, body?, k, id)?;
    let next = crate::ops::shared::breaker::close(b, body, BreakerKind::Materialize, ROW_LAYOUT, Budget::Share, Spill::Runs, k, id)?;
    let body_ix = (b.graph.pipelines.len() - 1) as u16;
    b.graph.term_from = outer_term; // the driver's lease (`lease_iter`) holds the range from here
    // the body runs the term's nodes: its closer is the term (the WorkTableScan's wtParam is in its allParam, not
    // the RecursiveUnion's)
    b.closed_by(term);
    let (mut reruns, mut slots, kept_by) = crate::lifetime::rerun_set(b, term_start, &[ru.wtParam], k, id)?;
    // D3373: a term the planner proved empty (a constant-false qual: a Result with no input whose one-time filter is false,
    // and no WorkTableScan) reads no wtParam, so no pipeline joins the set; PG still runs it once after the seed
    // (ExecRecursiveUnion), finds no row and ends the union: its body alone is the set, each run empty
    if reruns.is_empty() && dummy(term) {
        reruns.push(body_ix);
        let d = b.graph.pipelines[body_ix as usize].desc;
        for s in d.consumes.iter().chain(d.produces().iter()) {
            crate::graph::slot_set(&mut slots, *s);
        }
    }
    b.graph.kept_by.extend(kept_by.iter().map(|r| crate::lifetime::KeptRec { plan_id: RECURSIVE_SET, ..*r }));
    // D4104: a hashed table PG keeps across generations whose fill may raise is a kept member (lowerv4 RERUN_KEPT: run at
    // the term's first run only, where PG fills it), never run again at every generation
    let (keep, outer) = crate::lifetime::kept_hashed(b, term_start, &reruns, &[ru.wtParam], true).map_err(|why| Refusal::shape(k, id, why))?;
    b.graph.kept.extend(keep.iter().copied());
    let rerun = |p: usize| reruns.iter().any(|&r| r as usize == p);
    // F3: a once-run input of a member that is not kept whole across generations (the rerun home's keep list)
    let unwhole = crate::lifetime::term_unwhole(b, &reruns, work);
    let f3 = unwhole.then_some("a recursive term's once-run input kept across generations that is not kept whole (an owner-folded hash table, a Materialize, a TopK, ...; W8-RECURSE F3)");
    let once_quiet = (term_start..=body_ix as usize).filter(|&p| !rerun(p)).all(|p| b.closers.get(p).copied().is_some_and(|n| quiet(b, n)));
    let early_quiet = quiet(b, seed) && once_quiet;
    // D2732 (Hibernate's json_mergepatch: res read by an EXPR SubLink's InitPlan): readers that stop only by raising
    // (`raised`: an EXPR SubLink's 21000 at a second row) read the union to its end or fail the statement, so the
    // lowering's whole run of its seed and once-run term inputs at the first pull departs from PG as under a whole reader
    // or where PG fails too. Where they call no user or VOLATILE function (nothing but a raise shows), the seed wall does
    // not apply (they still run whole: they are no quiet ones)
    let calm = |n: Node<'p>| !crate::effects::setop::acts(&crate::rerun::of_plan(b.stmt, b.facts, b.arena, n));
    let once_calm = (term_start..=body_ix as usize).filter(|&p| !rerun(p)).all(|p| b.closers.get(p).copied().is_some_and(calm));
    let raise_ok = raised && calm(seed) && once_calm;
    // D4470 (early work that can only raise an error PG never hits is the accepted class, licensed by name:
    // `effects::accepted`): a seed and once-run term inputs that call no user or VOLATILE
    // function (`calm`: nothing but a raise shows) run whole at the first pull under any reader. Their rows are PG's; a
    // seed row PG never reads may raise there, an error PG never hits. A seed or input that calls keeps the seed wall
    // (the web demo's Mandelbrot: a float4 division in the seed's CTE, read by an EXISTS that stops at its first match)
    let calm_early = calm(seed) && once_calm;
    // D2034: an inner recursive union whose seed reads none of this union's params runs again at every generation only
    // because its union is rescanned (rerun.rs `union_seed`); its rows come back each generation, and such a term may
    // never end (with#45's inner union). PG streams each generation to the reader; the lowering runs a generation whole,
    // so where a reader may stop before this union's end it would not return: the union must be read whole
    let inner_seed = reruns.iter().any(|&m| {
        let Some(&c) = b.closers.get(m as usize) else { return false };
        let reads_wt = c.as_plan().is_some_and(|p| p.allParam.is_member(ru.wtParam));
        let id = c.as_plan().map(|p| p.plan_node_id);
        !reads_wt && id.and_then(|id| crate::lifetime::node_of(b.stmt, id)).and_then(|(_, up)| up).and_then(|u| u.as_recursive_union()).is_some_and(|r| r.plan.plan_node_id != plan.plan_node_id && r.plan.lefttree.and_then(|l| l.as_plan()).map(|l| l.plan_node_id) == id)
    });
    // the streamed form (ops/setop/gen_stream.rs) pulls such a generation as PG does, for the term shape whose set runs only
    // the body; readers() lifted the inner union's D2212 wall for exactly that shape, so a mismatch refuses
    let stream_src = gen_stream::source(b.stmt, ru);
    // D3373 (F-2, D2312's streamed form): a term whose target-list SRF is not known to end (the seed holds none) under a
    // reader that may stop: the streamed form pulls the term's body a budget at a time, its end stopping after the row that
    // reaches the want and the ProjectSet resuming inside its set, as PG's RecursiveUnion pulls the term one row per call
    let term_rows = srf.is_some() && term_srf && !seed_srf && !read_whole && stream_src.is_none() && !inner_seed;
    let stream = (inner_seed && !read_whole && stream_src.is_some()) || term_rows;
    let streamed = (inner_seed && !read_whole && !stream).then_some("a recursive term holding another recursive union whose seed runs again at every generation, read by a reader that may stop before the union's end: the lowering runs each generation whole where PG streams it, so an inner union that never ends never returns (D2034)");
    let unmatched = (stream_src.is_some() && !read_whole && !inner_seed).then_some("a recursive term streaming another recursive union's CTE whose seed does not run again at every generation (D2034: not the streamed shape)");
    // D3373 (F-1, D2312's pulled form): a seed whose target-list SRF may not end (the term holds none), one pipeline
    // reading no param, read by a reader that may stop (a LIMIT, a cursor's FETCH): the pulled seed's end stops after the
    // row that reaches the driver's want (`IterDesc::seed_row`) and the ProjectSet resumes inside its set at the next pull,
    // as PG's ExecRecursiveUnion pulls the non-recursive term once per row it returns. A seed under a UNION ALL (its own
    // spool), a term SRF and a union read whole keep the wall
    let seed_free = seed.as_plan().is_some_and(|p| p.allParam.is_empty() && p.extParam.is_empty());
    let seed_rows = seed_srf && !term_srf && (!read_whole || (top && b.graph.partial)) && !seed_union && seed_free && outer_term.is_none() && seed_end == seed_start + 1 && !inner_seed && stream_src.is_none();
    let srf = srf.filter(|_| !seed_rows && !term_rows);
    // `srf_ends`' literal guard where a set is run whole (a pulled seed or a streamed term reads it as far as PG does,
    // whatever its bounds, so it takes none)
    if ends && !seed_rows && !term_rows {
        b.graph.srf_lits.extend(bounds.iter().map(|&c| (id, c)));
    }
    let walled = srf.or(read_wall).or(f3).or(streamed).or(unmatched);
    // D2034, the seed cliff: under a reader that may stop, a quiet seed of one pipeline reading no param (so no other rerun
    // set runs it) is pulled as PG pulls it, a budget at a time (`IterDesc::seed`); a seed read whole, a UNION ALL seed's
    // spool, a multi-pipeline seed, a union inside another's term and a streamed term keep the whole seed
    let seed_param_free = seed.as_plan().is_some_and(|p| p.allParam.is_empty() && p.extParam.is_empty());
    let seed_form = walled.is_none() && !stream && !seed_union && seed_param_free && outer_term.is_none() && seed_end == seed_start + 1;
    // D2732 (D3373): one CteScan on the root path under a reader that may stop pulls the union's CTE no further than its
    // bound, a row a pull where the plan calls (lowerv4 ops/ctescan/lower.rs `Pull`), and the CTE object holds an error
    // behind the rows a larger pull stored; a seed that may raise or call of that shape is then pulled a row at a time
    // (`IterDesc::seed_exact`), where its once-run term inputs are quiet
    let stop_exact = seed_form && !read_whole && top && !through && !quiet(b, seed) && once_quiet && one_scan(b, ru);
    // D2732: PG reads the seed row by row as the CTE is pulled (and runs a once-run term input only at the first
    // generation); the lowering's first pull runs both whole, so where a reader may stop before the union's end a seed row
    // PG never reads could raise (or a user / VOLATILE call show) here. D4470: a calm seed's raise is the accepted class
    // (`calm_early`, which `raise_ok` implies), so the wall stands only for a seed or once-run input that calls or acts
    let seed_wall = (!read_whole && !early_quiet && !stop_exact && !calm_early).then_some("a recursive union whose seed or once-run term input can raise or calls a user or VOLATILE function, read by a reader that may stop before the union's end (PG reads the seed row by row; the lowering runs it whole at the first pull: D2732)");
    let wall = walled.or(seed_wall);
    let seed_shape = wall.is_none() && seed_form;
    // D2732 (D3377, the cursor form): a seed that may raise or call read by one CteScan on the root path under a partial
    // reader (`IterDesc::seed_exact`) is pulled a row at a time, where its once-run term inputs are quiet (they run before
    // the driver's first pull, PG's at its first generation); else a pull that may count its rows refuses (`pull_loud`).
    // D4295: under a whole reader too, read on the root path (a breaker over the CteScan or not, `Reach::root`), where the
    // seed acts past call-local builtins (`effects::setop::acts_past_local`: a user VOLATILE function, which may write what
    // a later call in the statement reads): PG hands each seed row on as it reads it (a Sort over the CteScan pulls it a row
    // at a time too), so a call between the union and its reader sees only the seed rows read so far, and a seed read whole
    // would show it every row's act. The CteScan pulls such a CTE a row per miss (lowerv4 ops/ctescan/lower.rs `Pull::One`)
    let seed_exact = seed_shape && (stop_exact || (read_whole && !through && ((top && b.graph.partial) || (root && crate::effects::setop::acts_past_local(b, seed))) && !quiet(b, seed) && once_quiet && one_scan(b, ru)));
    let pull_loud = top && !early_quiet && !seed_exact;
    // D3373 (E-1, E-3): another recursive union's readers() read it through this seed as far as this union's reader reads
    // (`Reach::enter_seeded`): a seed run whole would read that union to its end where PG stops; under a partial pull on
    // the root path (a cursor's FETCH) too
    let must_pull = plan_id_of(b, ru).and_then(|i| 1u64.checked_shl(i as u32)).is_some_and(|bit| b.seed_pulls & bit != 0);
    let counted = must_pull && top && read_whole && b.graph.partial && early_quiet;
    let seed_set = (seed_shape && ((!read_whole && early_quiet) || seed_exact || seed_rows || counted)).then_some(seed_start as u16);
    // D3373 (F-1): a seed SRF that is not pulled is walled as before
    let seed_row = seed_rows && seed_set.is_some();
    let wall = wall.or((seed_rows && !seed_row).then_some(SRF_WALL));
    let unpulled = (must_pull && seed_set.is_none()).then_some(SEED_UNPULLED);
    let wall = wall.or(unpulled);
    if let Some(sp) = seed_set {
        let mut pipelines = ArenaVec::with_capacity_in(1, b.arena);
        pipelines.push(sp);
        let mut slots: crate::graph::SlotSet = [0; 4];
        let d = b.graph.pipelines[sp as usize].desc;
        for s in d.consumes.iter().chain(d.produces().iter()) {
            crate::graph::slot_set(&mut slots, *s);
        }
        b.graph.reruns.push(crate::graph::Rerun { node_id: id, plan_id: RECURSIVE_SEED_SET, pipelines, slots });
    }
    // construct C2: the licence the seed and the once-run term inputs run under ahead of PG's row-by-row pull (the seed
    // whole at the first pull or a budget past the reader's row, a once-run input before the first generation): quiet
    // code's Ahead licence (`effects::quiet_ahead`; a seed reading an outer recursion's work table re-runs nothing ahead);
    // past the seed wall, a seed that may raise or calls is read whole by every reader (`read_whole`) or by readers that
    // stop only by raising (`raise_ok`): the whole-read licence, granted to acting code only through call-local builtins
    // (`call_local`'s proof over the seed and those inputs); a seed pulled a row at a time (`seed_exact`) runs only its
    // quiet once-run inputs ahead; D4470: a calm seed under a reader that may stop (neither of those) holds the accepted
    // class (`effects::accepted`: an error PG never hits, from early work; rows never differ). Walled: none (the lowering
    // refuses)
    let seed_ahead = {
        use crate::effects::{accepted, call_local, quiet_ahead, whole_read_local, Cx, Effects};
        let fx = |n: Node<'p>| Effects::of_found(crate::rerun::of_plan(b.stmt, b.facts, b.arena, n));
        let once = || (term_start..=body_ix as usize).filter(|&p| !rerun(p));
        let once_fx = once().fold(Effects::NONE, |e, p| e.and(b.closers.get(p).copied().map_or(Effects::UNKNOWN, fx)));
        if wall.is_some() {
            None
        } else if seed_exact {
            quiet_ahead(once_fx)
        } else if early_quiet {
            quiet_ahead(fx(seed).and(once_fx))
        } else if !read_whole && !raise_ok {
            // past the wall, so `calm_early`: no act and no user call (`calm`), and every once-run pipeline has a closer
            accepted(fx(seed).and(once_fx), false).ok()
        } else {
            // a once-run pipeline with no closer is unknown code: no proof covers it
            let early = fx(seed).and(once_fx);
            let closed = once().all(|p| b.closers.get(p).is_some());
            let units = || core::iter::once(seed).chain(once().filter_map(|p| b.closers.get(p).copied()));
            let local = (early.acts && closed).then(|| call_local(&Cx::new(b.stmt, b.facts, b.arena), units())).flatten();
            whole_read_local(early, local).ok()
        }
    };
    let tf = crate::rerun::of_plan(b.stmt, b.facts, b.arena, term);
    let term_volatile = crate::effects::setop::acts(&tf);
    let hash_loud = !tf.raise_free && hashes(term);
    // the generation's rerun set is declared as a nest's is (`rerun::declare_rerun`): its owner (the driver, lowerv4
    // setop/recursive.rs) declares its protos through nest/rerun.rs; `RECURSIVE_SET` names it in `plan_id`
    if !reruns.is_empty() {
        let mut set = ArenaVec::with_capacity_in(reruns.len(), b.arena);
        reruns.iter().for_each(|&p| set.push(p));
        let changed = crate::lifetime::changed_params(b, &set, &[ru.wtParam]);
        b.graph.reruns.push(crate::graph::Rerun { node_id: id, plan_id: RECURSIVE_SET, pipelines: set, slots });
        let at = (b.graph.reruns.len() - 1) as u16;
        crate::lifetime::claim_refills(b, term_start, at, &changed);
        // D4120: a kept fill reading an enclosing owner's params waits for that owner's set to claim them
        b.graph.stale_reads.extend(outer.iter().map(|&x| (at, x, id)));
    }
    crate::ops::shared::breaker::consume(b, &mut out, next); // the driver swaps each generation's rows into `work`
    crate::breakers::lease_iter(b, &mut out, IterDesc { body: body_ix, reruns, work, next, until: IterUntil::Empty, dedup, dedup_colls, dedup_rows, wt_param: ru.wtParam, early_quiet, read_whole, term_volatile, hash_loud, thin, through, wall, zero, pull_loud, stream, seed: seed_set, seed_exact, seed_ahead, calm: calm_early, window, seed_row, arms, arms_later });
    ops::project(&mut out, &plan.targetlist);
    b.graph.note(k);
    Ok(out)
}

/// D3373: plan node `n` is the planner's empty relation: a Result with no input and a constant false or NULL one-time filter.
fn dummy(n: Node<'_>) -> bool {
    let Some(r) = n.as_result() else { return false };
    r.plan.lefttree.is_none() && r.resconstantqual.is_some_and(crate::ops::shared::gate::never)
}

/// D3373 (E-1): the wall of a recursive union another union's readers() read through its seed, when its seed is not pulled.
const SEED_UNPULLED: &str = "a recursive union read through another recursive union's seed that runs whole (not the pulled seed), under a reader that may stop before its end: the lowering would read it to its end where PG stops (D2212)";

/// The CTE plan id of recursive union `ru` (its place in the statement's subplans, plus one).
fn plan_id_of<'p>(b: &Builder<'p, '_>, ru: &'p RecursiveUnion<'p>) -> Option<i32> {
    let at = b.stmt.subplans.iter().position(|r| r.and_then(|r| r.as_plan()).is_some_and(|p| p.plan_node_id == ru.plan.plan_node_id));
    at.map(|i| i as i32 + 1)
}

/// D2732 (D3377): the statement reads recursive union `ru` through exactly one CteScan.
fn one_scan<'p>(b: &Builder<'p, '_>, ru: &'p RecursiveUnion<'p>) -> bool {
    let stmt = b.stmt;
    let Some(plan_id) = plan_id_of(b, ru) else { return false };
    let mut scans = 0;
    crate::rerun::each_plan_node(stmt, &mut |n| {
        if n.node_tag() == NodeTag::T_CteScan && n.as_cte_scan().is_some_and(|c| c.ctePlanId == plan_id) {
            scans += 1;
        }
    });
    scans == 1
}

#[inline(never)] // D3748: never inlined into `map`, whose frame a set-operation nest keeps live per level
fn map_work_table_scan<'p>(b: &mut Builder<'p, '_>, w: &'p WorkTableScan<'p>) -> Result<Open<'p>, Refusal> {
    let (plan, k) = (&w.scan.plan, Kind::WorkTableScan);
    let id = plan.plan_node_id;
    let slot = b.work.iter().rev().find(|(p, _)| *p == w.wtParam).map(|(_, s)| *s).ok_or(Refusal::shape(k, id, "a WorkTableScan outside its RecursiveUnion"))?;
    let mut open = Open::new(b.arena, id, Source::Breaker { slot }, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite));
    open.desc = crate::breakers::consuming(b, open.desc, slot);
    ops::filters(&mut open, &plan.qual);
    ops::project(&mut open, &plan.targetlist);
    b.graph.note(k);
    Ok(open)
}

pub fn map<'p>(b: &mut Builder<'p, '_>, node: Node<'p>, k: Kind) -> Result<Open<'p>, Refusal> {
    match k {
        Kind::SetOp => map_set_op(b, node.as_set_op().unwrap()),
        Kind::RecursiveUnion => map_recursive_union(b, node.as_recursive_union().unwrap()),
        Kind::WorkTableScan => map_work_table_scan(b, node.as_work_table_scan().unwrap()),
        _ => Err(Refusal::kind(k, node.as_plan().map_or(STMT_NODE, |p| p.plan_node_id))),
    }
}

impl SetDesc<'_> {
    pub fn dump(&self, out: &mut dyn Write) -> fmt::Result {
        match self {
            SetDesc::Identity { keys } => {
                write!(out, "  identity keys=")?;
                dump_eq_keys(keys, out)
            }
            SetDesc::Set { cmd, right, keys, groups, form: SetForm::Stream { .. }, .. } => {
                write!(out, "  setop {cmd:?} sorted stream right=slot{right} groups={groups} keys=")?;
                dump_eq_keys(keys, out)
            }
            SetDesc::Set { cmd, hashed, left, right, keys, groups, form, pull_loud, .. } => {
                write!(out, "  setop {cmd:?} {}{}{} left=slot{left} right=slot{right} groups={groups} keys=", if *hashed { "hashed" } else { "sorted" }, if *form == SetForm::Ahead { " ahead" } else { "" }, if *pull_loud { " pull_loud" } else { "" })?;
                dump_eq_keys(keys, out)
            }
            SetDesc::Iter(d) => {
                write!(out, "  iterate body=p{} work=slot{} next=slot{} until={:?} wtparam={}", d.body, d.work, d.next, d.until, d.wt_param)?;
                if !d.dedup.is_empty() {
                    write!(out, " dedup keys=")?;
                    dump_eq_keys(&d.dedup, out)?;
                }
                if !d.early_quiet {
                    write!(out, " early-loud")?;
                }
                if d.term_volatile {
                    write!(out, " term-volatile")?;
                }
                if d.hash_loud {
                    write!(out, " hash-loud")?;
                }
                if d.stream {
                    write!(out, " stream")?;
                }
                // the pipelines re-run per generation when they are more than the body (the term's others run once)
                if d.reruns.as_slice() != [d.body] {
                    write!(out, " reruns=")?;
                    for (i, p) in d.reruns.iter().enumerate() {
                        write!(out, "{}p{p}", if i > 0 { "," } else { "" })?;
                    }
                }
                Ok(())
            }
        }
    }
}
