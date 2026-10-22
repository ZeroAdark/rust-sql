use gravity_abi::breaker::BreakerKind;
use gravity_program::{ArenaVec, End, Pipeline, Reason};
use types_nodes::list::NodeList;
use types_nodes::nodes_enums::LimitOption;
use types_nodes::plannodes::{Limit, Result as ResultPlan};
use types_nodes::{Node, NodeTag};

use crate::ops::shared::append::AppendDesc;
use crate::breakers;
use crate::catalog::Am;
use crate::graph::{Bound, Col, Op, Order, OrderKey, Source, MAX_ORDER_KEYS};
use crate::ledger::Kind;
use crate::admit::Refusal;
use crate::ops::shared::sort::EqKey;
use crate::walk::{Builder, Open};
pub mod agg;
pub mod append;
pub mod group;
pub mod limit;
pub mod mergejoin;
pub mod modifytable;
pub mod scan;
pub mod setop;
pub mod shared;
pub mod sort;
pub mod srf;
pub mod subplan;
pub mod window;

/// Whether `a` maps one arm per grouping set (agg's `map_sets`): its lists' SubPlans are each arm's
/// (`list_subplans`, before the arm's union), not the node's (`subplan::attach`).
pub fn per_set(a: &types_nodes::plannodes::Agg<'_>) -> bool {
    a.aggstrategy == crate::ops::shared::agg::AGG_MIXED || !a.groupingSets.is_nil() || !a.chain.is_nil()
}

/// The per-row SubPlans of `lists` placed before the first op of `open` that reads them (a grouping-sets arm's lists).
pub fn list_subplans<'p>(b: &mut Builder<'p, '_>, lists: &[&'p NodeList<'p>], k: Kind, id: i32, open: Open<'p>) -> Result<Open<'p>, Refusal> {
    subplan::attach_lists(b, lists, k, id, open)
}

/// The per-row SubPlans of window functions `funcs`' arguments past their D1415 step arguments (`window_step_arg`) and of
/// their FILTERs that the partition buffer's fill `open` evaluates (subplan's `attach_window_args`; a window's map).
pub fn window_subplans<'p>(b: &mut Builder<'p, '_>, funcs: &[Node<'p>], k: Kind, id: i32, open: Open<'p>) -> Result<Open<'p>, Refusal> {
    subplan::attach_window_args(b, funcs, k, id, open)
}

/// D1415: argument `a` of window function `f` is one the window object reads only at the current row, as int4, and holds a
/// SubPlan (window's `step_arg`; subplan's placement reads it).
pub fn window_step_arg(f: &types_nodes::primnodes::WindowFunc<'_>, a: usize) -> bool {
    window::step_arg(f, a)
}

pub fn filters<'p>(open: &mut Open<'p>, qual: &'p NodeList<'p>) {
    for q in qual.iter() {
        open.push(Op::Filter(q));
    }
}

pub fn project<'p>(open: &mut Open<'p>, tlist: &'p NodeList<'p>) {
    open.push(Op::Project(tlist));
}

/// D3005: a Limit's count and offset (`bound` of each), a generic plan's extern parameters taken as this execution's
/// constants together or not at all (`admit::limit_externs`; the admission's reuse checks the values).
pub fn limit_bounds<'p>(l: &Limit<'p>) -> (Bound<'p>, Bound<'p>) {
    let (c, o) = crate::admit::limit_externs(l.limitCount, l.limitOffset);
    (c.map_or_else(|| bound(l.limitCount), Bound::Const), o.map_or_else(|| bound(l.limitOffset), Bound::Const))
}

pub fn bound(n: Option<Node<'_>>) -> Bound<'_> {
    match n {
        None => Bound::All,
        Some(n) => match n.as_const() {
            Some(c) if c.constisnull => Bound::All,
            Some(c) if c.constbyval && c.constlen == 8 => Bound::Const(c.constvalue.as_i64()),
            _ => Bound::Expr(n),
        },
    }
}

/// The plan node the statement's first fetch reaches before any user code runs, fetched exactly once per execution:
/// walk from the root while the node fetches its only plan child first, unconditionally, never rescans it, and evaluates
/// nothing of its own before that fetch (a plain/sorted/hashed Agg, Sort, Incremental Sort, Unique, a WindowAgg whose
/// frame offsets are absent or constants, a Result without a one-time qual, a Limit whose count is absent or a constant
/// above zero); the first node off that list is the answer. A join, an Append, Gather or a Limit that may fetch nothing
/// ends the walk at itself. Used where the site does once at the bind what PG does at a node's first fetch (D1643's
/// TABLESAMPLE arguments, D1934's data-modifying CTE BEFORE STATEMENT triggers).
pub fn first_fetched(root: Option<Node<'_>>) -> Option<Node<'_>> {
    first_fetched_in(root, false)
}

/// D3095: `first_fetched`, the walk also taking a Nested Loop's outer child: ExecNestLoop fetches its outer plan first,
/// unconditionally, before its join quals or its inner run, and never rescans it (the loop itself is on the walk from
/// the root, fetched once). D3088 (3): and an Append's first arm when nothing prunes or reorders its arms (no run-time
/// pruning, no async arm, not parallel aware: a Parallel Append, under a Gather that ends the walk anyway, picks its arms
/// by worker): ExecAppend's first call fetches `appendplans[0]` before any other arm (a CTE read twice through UNION
/// ALL). Only where the answer names a CTE's first pull (`first_cte`): D1643 / D1934 keep the walk that ends at a join.
pub fn first_fetched_outer(root: Option<Node<'_>>) -> Option<Node<'_>> {
    first_fetched_in(root, true)
}

fn first_fetched_in(root: Option<Node<'_>>, nest_outer: bool) -> Option<Node<'_>> {
    let konst = |e: Option<Node<'_>>| e.is_none_or(|e| e.as_const().is_some());
    let mut n = root?;
    loop {
        let p = n.as_plan()?;
        if let Some(a) = n.as_append().filter(|a| nest_outer && !p.parallel_aware && a.part_prune_index < 0 && a.nasyncplans == 0) {
            match a.appendplans.iter().next() {
                Some(first) => {
                    n = first;
                    continue;
                }
                None => return Some(n),
            }
        }
        let once = match n.node_tag() {
            NodeTag::T_Agg | NodeTag::T_Sort | NodeTag::T_IncrementalSort | NodeTag::T_Unique => true,
            NodeTag::T_WindowAgg => n.as_window_agg().is_some_and(|w| konst(w.startOffset) && konst(w.endOffset)),
            NodeTag::T_Result => n.as_result().is_some_and(|r| r.resconstantqual.is_none()),
            NodeTag::T_Limit => n.as_limit().is_some_and(|l| match limit_bounds(l).0 {
                Bound::All => true,
                Bound::Const(c) => c > 0,
                Bound::Expr(_) => false,
            }),
            _ => false,
        };
        match (once, p.lefttree, p.righttree) {
            (true, Some(child), None) => n = child,
            (false, Some(outer), Some(_)) if nest_outer && n.node_tag() == NodeTag::T_NestLoop => n = outer,
            _ => return Some(n),
        }
    }
}

/// `WITH TIES`: the tie keys (the plan's `uniqColIdx` / `uniqOperators` over the input's projection — the sort
/// clause's columns) and the ordered source whose proven order leads with them (decision 56; `over` indexes
/// `TIE_SOURCES`). Past `count` the op stops at the first row whose keys differ from the last one emitted: over a
/// Sort's top-k the breaker holds the tie group (decision 20), over any other ordered source the op walks the source.
pub struct TieDesc {
    pub keys: ArenaVec<EqKey>,
    /// Each key's collation (the plan's `uniqCollations`; 0 when it names none): run.pergroup's key.coll image (D1517).
    pub colls: ArenaVec<u32>,
    /// D4126: each key's ROW() columns (type, collation) when it is an anonymous `record` whose columns the plan knows
    /// (`tie_row`), which run.pergroup images by their canonical form as a keyed group's record key; empty for every other
    /// key, and for every key of a Limit's ties (`tie_desc`).
    pub rows: ArenaVec<ArenaVec<(u32, u32)>>,
    pub over: u8,
}

/// One ordered-source kind a tie cut walks: its name and whether the open pipeline's order comes from it.
pub struct TieSource {
    pub name: &'static str,
    pub is: fn(&Builder<'_, '_>, &Open<'_>) -> bool,
}

fn over_breaker(b: &Builder<'_, '_>, p: &Open<'_>, f: fn(BreakerKind, Option<&Op<'_>>) -> bool) -> bool {
    let Source::Breaker { slot } = p.source else { return false };
    breakers::decl_of(&b.graph, slot).is_some_and(|d| d.order.is_known() && f(d.breaker.kind, b.graph.pipelines[d.producer as usize].ops.last()))
}

fn is_sort(b: &Builder<'_, '_>, p: &Open<'_>) -> bool {
    over_breaker(b, p, |kind, _| matches!(kind, BreakerKind::Sort | BreakerKind::TopK))
}

fn is_merge_append(b: &Builder<'_, '_>, p: &Open<'_>) -> bool {
    p.union.is_some_and(|u| b.graph.unions[u as usize].order.is_known()) || over_breaker(b, p, |kind, last| kind == BreakerKind::Materialize && matches!(last, Some(Op::Append(AppendDesc::Feed { ordered: true, .. }))))
}

fn is_materialized(b: &Builder<'_, '_>, p: &Open<'_>) -> bool {
    over_breaker(b, p, |kind, _| kind == BreakerKind::Materialize)
}

fn is_index(_: &Builder<'_, '_>, p: &Open<'_>) -> bool {
    matches!(p.source, Source::Index { backward: false, .. })
}

fn is_index_backward(_: &Builder<'_, '_>, p: &Open<'_>) -> bool {
    matches!(p.source, Source::Index { backward: true, .. })
}

pub(crate) fn is_sorted_scan(b: &Builder<'_, '_>, p: &Open<'_>) -> bool {
    matches!(&p.source, Source::Range { am: Am::Pgrcolumnar, relid, site, .. } if site.is_merged() || b.facts.rel(*relid).is_some_and(|r| r.nsorted > 0))
}

/// The ordered sources, one row each, in the order they are tried: a Sort's runs or top-k, a MergeAppend's merged
/// slot, an ordered materialised pipeline, an index scan forward / backward, a sorted pgrc scan. Anything else
/// has no proven order and refuses by name.
pub const TIE_SOURCES: &[TieSource] = &[
    TieSource { name: "sort", is: is_sort },
    TieSource { name: "merge-append", is: is_merge_append },
    TieSource { name: "materialized", is: is_materialized },
    TieSource { name: "index", is: is_index },
    TieSource { name: "index-backward", is: is_index_backward },
    TieSource { name: "sorted-scan", is: is_sorted_scan },
];

/// CAP-30's refusals: the tie / run keys pass the eight an `Order` holds and the input is not a Sort's emit (`order_past`).
pub const TIES_PAST: &str = "WITH TIES over more than eight keys whose input order past the eighth is not tracked (CAP-30: a Sort's emit only)";
/// USEROP: a Unique or Group over another btree family's order from a source other than a Sort (`first_of_run`'s `family`).
pub const FAMILY_RUNS_SOURCE: &str = "a Unique or Group over another family's order from a source other than a Sort (USEROP)";
pub const RUN_PAST: &str = "DISTINCT ON over more than eight keys whose input order past the eighth is not tracked (CAP-30: a Sort's emit only)";

/// CAP-30: the keys of the input's order past the `MAX_ORDER_KEYS` an `Order` holds, in the open's columns: over a Sort's
/// (or top-k's) emit through projections only, the Sort's further keys of their types' default families, moved through the
/// projections as `sort::order_of` moves the first ones (a key a projection drops ends the order there). A full or
/// Incremental Sort orders by every key (D2061's premise). D2061: over a btree index scan, its key columns past the eighth
/// (`order::index_past`). `None` over any other input: its order past the cut is not tracked.
fn order_past<'p>(b: &Builder<'p, '_>, open: &Open<'p>) -> Option<ArenaVec<OrderKey>> {
    if matches!(open.source, Source::Index { .. }) {
        return crate::ops::shared::order::index_past(b, open);
    }
    let Source::Breaker { slot } = open.source else { return None };
    if open.union.is_some() || b.work.iter().any(|w| w.1 == slot) || !open.ops.iter().all(|op| matches!(op, Op::Project(_))) {
        return None;
    }
    let d = breakers::decl_of(&b.graph, slot).filter(|d| matches!(d.breaker.kind, BreakerKind::Sort | BreakerKind::TopK))?;
    let Some(Op::Sort(s)) = b.graph.pipelines[d.producer as usize].ops.last() else { return None };
    if d.order.n as usize != MAX_ORDER_KEYS || s.keys.len() <= MAX_ORDER_KEYS {
        return None;
    }
    let mut out = ArenaVec::with_capacity_in(s.keys.len() - MAX_ORDER_KEYS, b.arena);
    for chunk in s.keys[MAX_ORDER_KEYS..].chunks(MAX_ORDER_KEYS) {
        // the producer's equalities ride along, so a projection finds a key through a column equal to it, as for the first keys
        let mut c = d.order.unordered();
        for key in chunk.iter().take_while(|k| !k.own_family) {
            c.push(OrderKey { col: Col::outer(key.resno), desc: key.desc, nulls_first: key.nulls_first });
        }
        let pushed = c.n as usize;
        for op in open.ops.iter() {
            if let Op::Project(t) = op {
                c = c.project(t);
            }
        }
        out.extend(c.keys().iter().copied());
        if c.n as usize != chunk.len() || pushed != chunk.len() {
            break;
        }
    }
    Some(out)
}

/// How a tie / run key list meets the input's order (`sort::leads_with`'s rule, directions free, the default family's
/// keys only): a wanted key that is a constant of the projection, repeats an earlier wanted key or equals a key already
/// passed needs no key of its own; the rest consume the order's keys, then `past`'s, in sequence.
#[derive(PartialEq, Eq)]
enum Leads {
    Yes,
    /// A wanted key met an order key that is not it.
    Differs,
    /// The tracked keys ran out first.
    Short,
}

fn leads_past(o: &Order, past: &[OrderKey], proj: Option<&NodeList<'_>>, want: &[OrderKey]) -> Leads {
    let n = o.n as usize;
    let key = |m: usize| if m < n { (o.fams[m] == 0).then_some(o.keys[m].col) } else { past.get(m - n).map(|k| k.col) };
    let mut j = 0;
    for (i, w) in want.iter().enumerate() {
        let passed = (0..j).any(|m| key(m).is_some_and(|c| o.same(c, w.col)));
        if crate::ops::shared::order::is_const(proj, w.col) || want[..i].iter().any(|x| o.same(x.col, w.col)) || passed {
            continue;
        }
        if j >= n + past.len() {
            return Leads::Short;
        }
        match key(j) {
            Some(c) if o.same(c, w.col) => j += 1,
            _ => return Leads::Differs,
        }
    }
    Leads::Yes
}

/// Whether `want` leads the open's order, reading a Sort's keys past the `Order`'s cut (CAP-30); `Err` is the refusal's
/// name: `wide` when the first keys agree and the order past the cut is not tracked (a merge, or an index whose keys past
/// the cut `order_past` does not read, with more keys than an `Order` holds), else `other` / `none` as `sort::ordered_as` names them.
fn tie_order<'p>(b: &Builder<'p, '_>, open: &Open<'p>, proj: &NodeList<'p>, want: &[OrderKey], wide: &'static str, other: &'static str, none: &'static str) -> Result<(), &'static str> {
    let o = crate::ops::shared::order::order_of(b, open);
    if crate::ops::shared::order::leads_with(&o, Some(proj), want, false) {
        return Ok(());
    }
    let full = o.n as usize == MAX_ORDER_KEYS;
    let past = if full { order_past(b, open) } else { None };
    match leads_past(&o, past.as_deref().unwrap_or(&[]), Some(proj), want) {
        Leads::Yes if past.is_some() => Ok(()),
        Leads::Short if full => Err(wide),
        _ if o.is_known() => Err(other),
        _ => Err(none),
    }
}

/// The tie cut over the Limit's input: its keys from the plan, the input's provenance checked to lead with them
/// (directions do not matter to a tie), the source named from `TIE_SOURCES`.
fn tie_desc<'p>(b: &Builder<'p, '_>, open: &Open<'p>, l: &'p Limit<'p>, k: Kind, id: i32) -> Result<TieDesc, Refusal> {
    let n = l.uniqNumCols as usize;
    if n == 0 || l.uniqColIdx.len() < n || l.uniqOperators.len() < n {
        return Err(Refusal::shape(k, id, "WITH TIES without tie keys"));
    }
    let tlist = crate::ops::shared::order::projection(open).ok_or(Refusal::shape(k, id, "WITH TIES over an input without a projection"))?;
    let mut keys = ArenaVec::with_capacity_in(n, b.arena);
    let (mut want, mut colls) = (ArenaVec::with_capacity_in(n, b.arena), ArenaVec::with_capacity_in(n, b.arena));
    for i in 0..n {
        let resno = l.uniqColIdx[i];
        // a tie is an equal key under the key's collation: the step ties a nondeterministic one by key.coll's image (colls)
        keys.push(EqKey { resno, opno: l.uniqOperators[i], kind: crate::ops::shared::sort::column_kind(b.facts, tlist, resno, k, id)?, typid: crate::ops::shared::sort::column_type(tlist, resno, k, id)? });
        colls.push(l.uniqCollations.get(i).copied().unwrap_or(0));
        want.push(OrderKey { col: Col::outer(resno), desc: false, nulls_first: false });
    }
    tie_order(b, open, tlist, &want, TIES_PAST, "WITH TIES over an input not in the tie keys' order", "WITH TIES over an input with no known order").map_err(|why| Refusal::shape(k, id, why))?;
    let over = TIE_SOURCES.iter().position(|s| (s.is)(b, open)).ok_or(Refusal::shape(k, id, "WITH TIES over an input with no known order"))?;
    Ok(TieDesc { keys, colls, rows: ArenaVec::new_in(b.arena), over: over as u8 })
}

/// V4R-147: DISTINCT ON's run keys over the Unique's input: the plan's `uniqColIdx` / `uniqOperators`, the input's
/// provenance checked to lead with them (directions do not matter to equal runs), the source named from `TIE_SOURCES`.
/// USEROP: `family`, the caller proved the input leads with the keys, some in another btree family's order
/// (`facts::stream::family_order`: a Sort by a user opclass's operator, where rows equal under its `=` are adjacent); only
/// a Sort's emit is taken then.
pub fn first_of_run<'p>(b: &Builder<'p, '_>, open: &Open<'p>, (cols, ops, cs): (&[i16], &[u32], &[u32]), family: bool, k: Kind, id: i32) -> Result<TieDesc, Refusal> {
    let n = cols.len();
    if n == 0 || ops.len() < n {
        return Err(Refusal::shape(k, id, "DISTINCT ON without its equality operators"));
    }
    let tlist = crate::ops::shared::order::projection(open).ok_or(Refusal::shape(k, id, "DISTINCT ON over an input without a projection"))?;
    let mut keys = ArenaVec::with_capacity_in(n, b.arena);
    let (mut want, mut colls, mut rows) = (ArenaVec::with_capacity_in(n, b.arena), ArenaVec::with_capacity_in(n, b.arena), ArenaVec::with_capacity_in(n, b.arena));
    for i in 0..n {
        let resno = cols[i];
        let typid = crate::ops::shared::sort::column_type(tlist, resno, k, id)?;
        keys.push(EqKey { resno, opno: ops[i], kind: crate::ops::shared::sort::column_kind(b.facts, tlist, resno, k, id)?, typid });
        colls.push(cs.get(i).copied().unwrap_or(0));
        rows.push(tie_row(b, tlist, resno, typid, k, id)?);
        want.push(OrderKey { col: Col::outer(resno), desc: false, nulls_first: false });
    }
    if family {
        // D2313 (the Agg form is ops/group/absorb.rs `family_order`): an index scan of the family's opclass hands
        // its rows on in that order too (Facts::index_family), equal rows under its `=` adjacent, forward or backward
        let sorted = open.union.is_none() && matches!(open.source, Source::Breaker { slot } if breakers::decl_of(&b.graph, slot).is_some_and(|d| matches!(d.breaker.kind, BreakerKind::Sort | BreakerKind::TopK)));
        let over = TIE_SOURCES.iter().position(|s| match s.name {
            "sort" => sorted,
            "index" | "index-backward" => open.union.is_none() && (s.is)(b, open),
            _ => false,
        });
        let over = over.ok_or(Refusal::shape(k, id, FAMILY_RUNS_SOURCE))?;
        return Ok(TieDesc { keys, colls, rows, over: over as u8 });
    }
    tie_order(b, open, tlist, &want, RUN_PAST, "DISTINCT ON over an input not in its keys' order", "DISTINCT ON over an input not in its keys' order").map_err(|why| Refusal::shape(k, id, why))?;
    let over = TIE_SOURCES.iter().position(|s| (s.is)(b, open)).ok_or(Refusal::shape(k, id, "DISTINCT ON over an input with no known order"))?;
    Ok(TieDesc { keys, colls, rows, over: over as u8 })
}

/// D4126: DISTINCT ON key `resno` (of type `typid`, over the Unique `id`'s input projection `tlist`)'s ROW() columns
/// (type, collation) when it is an anonymous `record` whose columns the plan knows (facts::record `key_row`, as a group
/// key's KeyDecl `rec`); empty for any other key and for a record known only per row (run.pergroup keeps hosted hash's
/// refusal for it). A column under no determinable collation refuses by sort's name, as a group key's ROW() does.
fn tie_row<'p>(b: &Builder<'p, '_>, tlist: &NodeList<'p>, resno: i16, typid: u32, k: Kind, id: i32) -> Result<ArenaVec<(u32, u32)>, Refusal> {
    let mut cols = ArenaVec::new_in(b.arena);
    if !crate::catalog::anonymous_record(typid) {
        return Ok(cols);
    }
    let e = (resno as usize).checked_sub(1).filter(|&i| i < tlist.len()).and_then(|i| tlist.nth(i).as_target_entry()).map(|t| t.expr);
    let Some(row) = e.and_then(|e| crate::facts::record::key_row(b, id, resno, e, false)) else { return Ok(cols) };
    if crate::facts::record::indeterminate(b.facts, row) {
        return Err(Refusal::shape(k, id, crate::ops::shared::sort::INDETERMINATE_COLL));
    }
    crate::facts::record::columns(b.facts, row).into_iter().for_each(|c| cols.push(c)); // alloc: Program
    Ok(cols)
}

/// The node a Limit's bound reaches (execProcnode.c `ExecSetTupleBound`): its child, through a Result over an outer plan
/// (the planner's projection of a postponed VOLATILE or costly target above a Sort), which hands every row it pulls on.
/// A constant qual stops the pass: the Gate this plan runs for it reads the child's rows.
fn bounded<'p>(mut n: Option<Node<'p>>) -> Option<&'p types_nodes::plannodes::Plan<'p>> {
    loop {
        let c = n?;
        match c.as_result() {
            Some(r) if r.resconstantqual.is_none() && r.plan.lefttree.is_some() && r.plan.qual.is_nil() => n = r.plan.lefttree,
            _ => return c.as_plan(),
        }
    }
}

pub fn map_limit<'p>(b: &mut Builder<'p, '_>, l: &'p Limit<'p>) -> Result<Open<'p>, Refusal> {
    let (id, k) = (l.plan.plan_node_id, Kind::Limit);
    let ties = l.limitOption == LimitOption::LIMIT_OPTION_WITH_TIES;
    let (count, offset) = limit_bounds(l);
    // WITH TIES hands no bound (nodeLimit.c `compute_tuples_needed`): its Limit reads one row past the ties through the Result.
    // D2645: a computed bound (an outer row's parameter under a LATERAL, a run-time value) reaches only its direct child:
    // a top's own copy of it is re-read at each rescan only where the Limit reads the top's breaker (rerun params_bound),
    // so through a Result the top kept the first outer row's bound for every rescan; a constant one passes through
    let computed = matches!(count, Bound::Expr(_)) || matches!(offset, Bound::Expr(_));
    let reach = if ties || computed { l.plan.lefttree.and_then(|c| c.as_plan()) } else { bounded(l.plan.lefttree) };
    b.limit = reach.map(|c| (c.plan_node_id, count, offset, ties));
    let mut open = b.child(&l.plan, k)?;
    b.limit = None;
    if !l.plan.qual.is_nil() {
        return Err(Refusal::shape(k, id, "a qual on Limit"));
    }
    // the stop handed down (facts/stream.rs, computed once here): the sort that keeps it runs no row PostgreSQL never reads
    let stop = crate::facts::stream::group_streams(b, l);
    if limit::ahead(b, l.plan.lefttree, &open, count, stop.map(|s| s.sort)) {
        return Err(Refusal::shape(k, id, limit::AHEAD));
    }
    if let Some(s) = stop {
        b.graph.stops.push(s);
    }
    let ties = if ties { Some(tie_desc(b, &open, l, k, id)?) } else { None };
    open.push(Op::Limit { count, offset, ties });
    project(&mut open, &l.plan.targetlist);
    b.graph.note(k);
    Ok(open)
}

pub fn map_result<'p>(b: &mut Builder<'p, '_>, r: &'p ResultPlan<'p>) -> Result<Open<'p>, Refusal> {
    let (id, k) = (r.plan.plan_node_id, Kind::Result);
    let mut open = match r.plan.lefttree {
        Some(_) => b.child(&r.plan, k)?,
        None => Open::new(b.arena, id, Source::Const, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite)),
    };
    if let Some(g) = r.resconstantqual {
        open.push(Op::Gate(g));
    }
    filters(&mut open, &r.plan.qual);
    project(&mut open, &r.plan.targetlist);
    b.graph.note(k);
    Ok(open)
}
