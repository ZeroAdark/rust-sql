use core::fmt::{self, Write};

use gravity_abi::breaker::{BreakerKind, Budget, Spill};
use gravity_program::{limits, ArenaRef, ArenaVec, End, Parallel, Pipeline};
use types_nodes::list::NodeList;
use types_nodes::plannodes::{Append, MergeAppend, SeqScan};
use types_nodes::Node;

use crate::breakers;
use crate::graph::{BreakerDecl, Col, Layout, NodeOps, Op, Order, OrderKey, PipelineGraph, Source};
use crate::ledger::Kind;
use crate::ops;
use crate::admit::Refusal;
use crate::ops::shared::sort::{column_type, keys_layout, SortKey};
use crate::walk::{Builder, Open, STMT_NODE};
use crate::ops::shared::append::{AppendDesc, PruneDesc, UnionRec};
use crate::ops::shared::union::{consume_all, settle, settle_arms, streams, BASE};

impl PruneDesc<'_> {
    /// Whether every level of the info is a partitioned relation's (`PartitionedRelPruneInfo`) and `pred` holds for each
    /// PARAM_EXEC id its exec steps read (`execparamids`, ascending), level by level: the lowering asks who sets them
    /// (lowerv4 ops/append/prune.rs, an InitPlan or a rerun set's producer) without reading the plan node itself.
    pub fn exec_params_all(&self, mut pred: impl FnMut(i32) -> bool) -> bool {
        for l in self.info.prune_infos.iter() {
            let Some(l) = l.as_list() else { continue };
            for r in l.iter() {
                let Some(r) = r.as_partitioned_rel_prune_info() else { return false };
                let mut x = r.execparamids.next_member(-1);
                while x >= 0 {
                    if !pred(x) {
                        return false;
                    }
                    x = r.execparamids.next_member(x);
                }
            }
        }
        true
    }
}

pub fn map_prune(b: &mut Builder<'_, '_>) -> Result<(), Refusal> {
    for n in b.stmt.partPruneInfos.iter() {
        let info = n.as_partition_prune_info().ok_or(Refusal::shape(Kind::PartitionPruneInfo, STMT_NODE, "a partPruneInfos entry that is not a PartitionPruneInfo"))?;
        for l in info.prune_infos.iter() {
            for r in l.as_list().map_or(NodeList::nil().iter(), |l| l.iter()) {
                if r.as_partitioned_rel_prune_info().is_none() {
                    return Err(Refusal::shape(Kind::PartitionPruneInfo, STMT_NODE, "a prune_infos entry that is not a PartitionedRelPruneInfo"));
                }
            }
        }
    }
    Ok(())
}

fn prune_of<'p>(b: &mut Builder<'p, '_>, index: i32, k: Kind, id: i32) -> Result<Option<PruneDesc<'p>>, Refusal> {
    if index < 0 {
        return Ok(None);
    }
    let n = (index as usize).min(b.stmt.partPruneInfos.len());
    if n == b.stmt.partPruneInfos.len() {
        return Err(Refusal::shape(k, id, "a part_prune_index outside partPruneInfos"));
    }
    let info = b.stmt.partPruneInfos.nth(n).as_partition_prune_info().unwrap();
    let (mut initial, mut exec) = (false, false);
    for l in info.prune_infos.iter() {
        for r in l.as_list().map_or(NodeList::nil().iter(), |l| l.iter()) {
            let r = r.as_partitioned_rel_prune_info().unwrap();
            exec |= !r.exec_pruning_steps.is_nil();
            initial |= !r.initial_pruning_steps.is_nil();
        }
    }
    b.graph.note(Kind::PartitionPruneInfo);
    Ok(Some(PruneDesc { info, index: n as u32, initial, exec }))
}

/// Push `c`, an open over union `u`: each arm gets the marker and a copy of `c`'s ops and ends in `c`'s end; when an
/// op cannot be copied or `c`'s desc is not the base one, the union keeps its Materialize form (`settle_arms`).
pub(crate) fn push_union<'p>(g: &mut PipelineGraph<'p>, mut c: Open<'p>, u: u32, ends: &mut ArenaVec<u16>) {
    let arena = *g.pipelines.allocator();
    c.union = None;
    let arms = core::mem::replace(&mut g.unions[u as usize].arms, ArenaVec::new_in(arena));
    let (order, prune) = (g.unions[u as usize].order, g.unions[u as usize].prune);
    // D1071: a streamed group reads its rows in the group keys' order, so its arms stream only in an ordered union's order
    let unordered_run = !order.is_known() && c.ops.iter().any(|op| matches!(op, Op::GroupRun(_)));
    let mut subs = crate::nested_arms::take_subs(g, u);
    if !streams(g, &c, &arms, order.is_known(), &|id| subs.iter().any(|s| s.plan_id == id)) || unordered_run {
        let emit = settle_arms(g, c, arms, order, prune);
        return g.push_into(emit, ends);
    }
    let n = arms.len() as u16;
    crate::arm_nests::fire_copies(g, &c.ops); // D4569
    // the parent's consumed slots are read once per arm (one read taken by `consuming`); a copied step's slots by arm 0 only
    let held = crate::nested_arms::held_slots(&c.ops, &subs);
    for &s in c.desc.consumes.iter().filter(|&&s| !crate::graph::slot_has(&held, s)) {
        g.reads[s as usize] = g.reads[s as usize].saturating_add(n - 1);
    }
    let base = crate::nested_arms::without(arena, c.desc, &held);
    // one buffer each for the n arms' pipelines (the fixed arena keeps every buffer a doubling leaves behind)
    g.pipelines.reserve(n as usize);
    ends.reserve(n as usize);
    let first = g.pipelines.len().min(u16::MAX as usize) as u16;
    for (i, mut a) in arms.into_iter().enumerate() {
        let m = a.ops.len() as u16;
        a.ops.reserve_exact(c.ops.len() + 1);
        let own = own_returning(&a, &c);
        a.push(Op::Append(AppendDesc::Arm { i: i as u16, n, ordered: order.is_known(), prune }));
        for op in c.ops.iter() {
            let x = match (op, own) {
                (Op::Project(l), Some((first, mine))) if core::ptr::eq(*l, first) => Op::Project(mine),
                _ => crate::nested_arms::dup_held(op, i, &mut subs, dup).unwrap_or_else(|| dup(op).expect("checked by streams")),
            };
            a.push(x);
        }
        // the Append's entry starts at the marker, the parent's nodes at their copied ops
        a.node_ops.extend(c.node_ops.iter().enumerate().map(|(k, x)| NodeOps { first_op: if k == 0 { m } else { x.first_op + m + 1 }, ..*x }));
        a.desc = arm_desc(arena, a.desc, &base);
        crate::nested_arms::embed_held(g, &mut a, &subs);
        g.push_into(a, ends);
    }
    g.unions[u as usize].pushed = first..g.pipelines.len().min(u16::MAX as usize) as u16;
}

/// V4R-214: arm `a` of a write of several result relations projects RETURNING through its own target's list
/// (`returningLists[i]`, in that relation's attnos; ExecProcessReturning's per-result-relation projection), the target
/// its source scans, in place of `targets[0]`'s the write carries. An arm over no target's scan keeps `targets[0]`'s:
/// write/delete.rs refuses it as a write over more than its target's own scan.
fn own_returning<'p>(a: &Open<'p>, c: &Open<'p>) -> Option<(&'p NodeList<'p>, &'p NodeList<'p>)> {
    let w = c.ops.iter().find_map(|o| if let Op::Write(w) = o { Some(w) } else { None }).filter(|w| w.targets.len() > 1)?;
    let rti = match &a.source {
        Source::Range { scanrelid, .. } | Source::Index { scanrelid, .. } | Source::Bitmap { scanrelid, .. } | Source::Tid { scanrelid, .. } => u32::from(*scanrelid),
        _ => return None,
    };
    Some((w.returning?, w.targets.iter().find(|t| t.rti == rti)?.returning?))
}

/// An arm's desc: its own site, parallelism and reads, then the parent's reads and what the parent's end set.
fn arm_desc(arena: ArenaRef, mut d: Pipeline, c: &Pipeline) -> Pipeline {
    d.ends_in = c.ends_in;
    // a row-owning op's arm (`owned`) is serial for it, as `serial_after` made the op's own open
    if let (false, Parallel::Ineligible { reason, .. }) = (c.parallel == BASE.parallel, c.parallel) {
        let site = if d.driving_site >= 0 { d.driving_site + 1 } else { 0 };
        d.parallel = Parallel::Ineligible { site, reason };
    }
    if c.claims != BASE.claims {
        d.claims = c.claims;
    }
    d.owner = c.owner.or(d.owner);
    d.admit = c.admit.or(d.admit);
    d.words = c.words.or(d.words);
    d.topk = c.topk.or(d.topk);
    if c.store_row_bytes != 0 {
        d.store_row_bytes = c.store_row_bytes;
    }
    consume_all(arena, d, c)
}

fn dup<'p>(op: &Op<'p>) -> Option<Op<'p>> {
    crate::walk::stack_check();
    use crate::join::JoinDesc;
    use crate::ops::shared::subplan::{SubForm, SubPlanDesc};
    Some(match op {
        Op::Filter(n) => Op::Filter(*n),
        Op::Gate(n) => Op::Gate(*n),
        Op::Project(l) => Op::Project(*l),
        Op::Limit { count, offset, ties } => Op::Limit { count: *count, offset: *offset, ties: ties.as_ref().map(|t| crate::ops::TieDesc { keys: t.keys.clone(), colls: t.colls.clone(), rows: t.rows.clone(), over: t.over }) },
        Op::Join(JoinDesc::Probe { slot, jointype, clauses, keys, qual, unique, marks, decls, inner_node, outer_first, catalog }) => Op::Join(JoinDesc::Probe { slot: *slot, jointype: *jointype, clauses: *clauses, keys: *keys, qual: *qual, unique: *unique, marks: *marks, decls: decls.clone(), inner_node: *inner_node, outer_first: *outer_first, catalog: *catalog }),
        Op::SubPlan(d) => {
            let form = match d.form {
                SubForm::Build => SubForm::Build,
                SubForm::Hashed { slot } => SubForm::Hashed { slot },
                SubForm::Nested { ref inner } => SubForm::Nested { inner: crate::nested_arms::dup_inner(inner, dup)? },
            };
            Op::SubPlan(SubPlanDesc { plan_id: d.plan_id, link: d.link, test: d.test, args: d.args, params: d.params, sets: d.sets, ids: d.ids, site: d.site, conditional: d.conditional, volatility: d.volatility, raise_free: d.raise_free, quiet_but_rows: d.quiet_but_rows, invariant: d.invariant, unknown_eq_false: d.unknown_eq_false, alternative: d.alternative, set_spool: d.set_spool, stops: d.stops, ahead: d.ahead, call: d.call, form })
        }
        Op::Append(AppendDesc::Arm { i, n, ordered, prune }) => Op::Append(AppendDesc::Arm { i: *i, n: *n, ordered: *ordered, prune: *prune }),
        Op::GroupRun(i) => Op::GroupRun(*i),
        Op::Srf(d) => Op::Srf(crate::ops::shared::srf::SrfDesc { place: d.place, functions: d.functions.clone(), tlist: d.tlist }),
        Op::Lock(d) => Op::Lock(crate::ops::shared::modifytable::LockDesc { marks: d.marks.clone(), plan: d.plan.clone(), epq: d.epq, tlist: d.tlist, members: d.members }),
        Op::Sort(d) => {
            let form = match d.form {
                crate::ops::shared::sort::SortForm::Runs => crate::ops::shared::sort::SortForm::Runs,
                crate::ops::shared::sort::SortForm::TopK { count, offset, ties } => crate::ops::shared::sort::SortForm::TopK { count, offset, ties },
                crate::ops::shared::sort::SortForm::Walk { .. } => return None,
            };
            Op::Sort(crate::ops::shared::sort::SortDesc { keys: d.keys.clone(), presorted: d.presorted, form, tlist: d.tlist })
        }
        Op::Write(w) => Op::Write(w.clone()),
        Op::Join(JoinDesc::Nest { .. }) => return crate::arm_nests::dup_nest(op, dup),
        _ => return None,
    })
}

/// A child feeds the shared slot; a merge's child must already come in the merge keys' order (`order`), which is then
/// the slot's declared order — a union's slot has none (feed order across children).
pub(crate) fn feed<'p>(b: &mut Builder<'p, '_>, open: Open<'p>, slot: Option<u8>, kind: BreakerKind, layout: Layout<'p>, order: Order, k: Kind, id: i32) -> Result<u8, Refusal> {
    let open = settle(&mut b.graph, open);
    let mut open = crate::ops::shared::breaker::respool(b, open, k, id)?;
    // USEROP: a merge key of another family's order is no `is_known` order (`Order::fams`), yet the children are in it
    let ordered = !order.keys().is_empty();
    if ordered {
        crate::ops::shared::order::ordered_as_order(&crate::ops::shared::order::order_of(b, &open), crate::ops::shared::order::projection(&open), &order, k, id, "a MergeAppend child with no known order", "a MergeAppend child not in the merge keys' order")?;
    }
    let Some(slot) = slot else {
        let slot = breakers::free_slot(&b.graph, &open).ok_or(Refusal::shape(k, id, "more than 256 live breaker slots (the u8 breaker slot id, limits::BREAKERS)"))?;
        open.push(Op::Append(AppendDesc::Feed { slot, ordered }));
        let slot = crate::ops::shared::breaker::close(b, open, kind, layout, Budget::Share, Spill::Runs, k, id)?;
        b.graph.breakers.last_mut().expect("just declared").order = order;
        return Ok(slot);
    };
    if b.graph.pipelines.len() + 2 > limits::PIPELINES {
        return Err(Refusal::shape(k, id, "more than 65535 pipelines (the u16 pipeline id)"));
    }
    let decl = *b.graph.breakers.iter().rev().find(|d| d.breaker.slot == slot).expect("the slot was declared by the first child");
    open.push(Op::Append(AppendDesc::Feed { slot, ordered }));
    open.desc.ends_in = End::Breaker(decl.breaker);
    let p = b.graph.pipelines.len() as u16;
    b.graph.push(open);
    b.graph.breakers.push(BreakerDecl { breaker: decl.breaker, layout: decl.layout, producer: p, order });
    Ok(slot)
}

fn map_append<'p>(b: &mut Builder<'p, '_>, a: &'p Append<'p>) -> Result<Open<'p>, Refusal> {
    let (plan, k) = (&a.plan, Kind::Append);
    let id = plan.plan_node_id;
    if a.appendplans.is_nil() {
        return Err(Refusal::shape(k, id, "an Append without children"));
    }
    let prune = prune_of(b, a.part_prune_index, k, id)?;
    // D3681: a write's twin result relations (modifytable/mod.rs `set_write`) group only whole, every child a twin of the
    // first and no pruning at run time: one set or every arm its own
    let write = written(b, a);
    // D3663: an Append pruned at the executor start alone (no exec step) groups too when every child is a twin of the
    // first: it is then one set site whose executions read the members its pruning keeps (exec append/proto.rs `Initial`)
    let pruned_set = !write && prune.as_ref().is_some_and(|p| !p.exec) && relsets(b, a) && all_twins(b, a);
    let group = (prune.is_none() && relsets(b, a) && (!write || all_twins(b, a))) || pruned_set;
    let whole = pruned_set || write && group;
    let mut opens = ArenaVec::with_capacity_in(a.appendplans.len(), b.arena);
    let mut starts = ArenaVec::with_capacity_in(a.appendplans.len() + 1, b.arena);
    // D3585: the open run of twin heap scans, its first scan's open (by index) and plan node
    let mut lead: Option<(usize, &'p SeqScan<'p>)> = None;
    for c in a.appendplans.iter() {
        if let Some((i, l)) = lead {
            if let Some(s) = c.as_seq_scan().filter(|s| whole || twin(b, l, s)) {
                join_twin(&mut opens[i], b.rte(s.scan.scanrelid).map_or(0, |r| r.relid), &s.scan.plan);
                continue;
            }
        }
        starts.push(b.graph.pipelines.len().min(u16::MAX as usize) as u16);
        let pushed = b.graph.pipelines.len();
        let open = b.plan(c)?;
        // D3663: a pruned Append's one run starts at its first child or not at all (its prune's arms are its subplans);
        // D3681: so does a write's
        lead = c.as_seq_scan().filter(|s| group && (!whole || opens.is_empty()) && b.graph.pipelines.len() == pushed && leads(b, &open, s)).map(|s| (opens.len(), s));
        opens.push(open);
    }
    starts.push(b.graph.pipelines.len().min(u16::MAX as usize) as u16);
    // every child one run of twins: the run's pipeline is the Append's rows, no union
    if opens.len() == 1 && opens[0].source.relation().is_some_and(|(_, _, site)| site.set.len() > 1) {
        let mut out = opens.pop().expect("one open");
        if let (Source::Range { site, .. }, Some(p)) = (&mut out.source, prune.filter(|_| pruned_set)) {
            site.set_prune = Some(p.index);
        }
        ops::filters(&mut out, &plan.qual);
        ops::project(&mut out, &plan.targetlist);
        b.graph.note(k);
        return Ok(out);
    }
    // the planner's ordered-Append mark (the partitions in bound order, each child pinned to the keys): the
    // union's feed order is the keys' order when every child's provenance says so, else no order is claimed
    let mut order = ordered_mark(b.facts, a);
    if order.is_known() && !opens.iter().all(|o| crate::ops::shared::order::leads_with(&crate::ops::shared::order::order_of(b, o), crate::ops::shared::order::projection(o), order.keys(), true)) {
        order = Order::NONE;
    }
    if opens.len() > u16::MAX as usize {
        return Err(Refusal::shape(k, id, "more than 65535 Append children"));
    }
    // the streamed union (V3-R1C): the children are held as arms; the open stands for the union's row until its
    // parent's end is pushed (`push_union`)
    let u = b.graph.unions.len() as u32;
    b.graph.unions.push(UnionRec { arms: opens, order, prune, starts, pushed: 0..0, sets: false });
    let mut out = Open::new(b.arena, id, Source::Const, BASE);
    out.union = Some(u);
    ops::filters(&mut out, &plan.qual);
    ops::project(&mut out, &plan.targetlist);
    b.graph.note(k);
    Ok(out)
}

/// D3585: whether Append `a` may read its runs of twin heap scans as relation sets (`group_twins`): the admission groups
/// (`admit::grouping_relsets`), the statement is a plain read (a write's or a lock's rows carry their own leaf), and the
/// Append keeps PostgreSQL's subplan order without an ordered mark (pruning keeps every arm its own: `prune_of`; D3663:
/// but an initial-steps-only pruning over all twins is one set site, `map_append`'s `pruned_set`).
/// D3681: or the Append a top-level UPDATE / DELETE over twin result relations reads (`written`).
fn relsets(b: &Builder<'_, '_>, a: &Append<'_>) -> bool {
    let read = b.stmt.commandType == types_nodes::nodes_enums::CmdType::CMD_SELECT && b.stmt.rowMarks.is_nil();
    crate::admit::relsets_on() && (read || written(b, a)) && b.carry.is_none() && a.gravity_sort_col_idx.len() == 0 && a.appendplans.len() > 1
}

/// D3681: Append `a` is the one under a top-level UPDATE / DELETE whose result relations are written alike (modifytable/mod.rs
/// `set_write`): its scans carry the row identity (`ctid`, `tableoid`), each row's own.
fn written(b: &Builder<'_, '_>, a: &Append<'_>) -> bool {
    b.write_set >= 0 && b.write_set == a.plan.plan_node_id
}

/// D3663: every child of `a` is a sequential scan, the first a bare one and each other its twin (`twin`), read on the plan
/// before any child is mapped.
fn all_twins(b: &Builder<'_, '_>, a: &Append<'_>) -> bool {
    let mut kids = a.appendplans.iter().map(|c| c.as_seq_scan());
    let Some(Some(l)) = kids.next() else { return false };
    bare_scan(b, l) && kids.all(|s| s.is_some_and(|s| twin(b, l, s)))
}

/// D3585: `l`'s scan of a plain table (no sample, no security quals, no InitPlan or subtree, no SubPlan in its clauses)
/// planned into `open`, a bare heap range with no row identity: its filters and projections over a site that reads no
/// system column (a ctid or a tableoid is its own relation's), a sequential scan's counts and one relation.
/// D3681: a write's scan (`written`) carries the row identity: its rowid lane (`ctid`) and a `tableoid` system lane, no other.
/// PGRCSERVE M0: a rows-road site (`rows_via_am`) maps as `Am::Heap` but reads one relation's slots: it leads no set.
fn leads(b: &Builder<'_, '_>, open: &Open<'_>, l: &SeqScan<'_>) -> bool {
    let Source::Range { site, am: crate::Am::Heap, .. } = &open.source else { return false };
    let write = b.write_set >= 0;
    let rowid = site.lanes().iter().any(|x| matches!(x.id, gravity_program::LaneId::RowId));
    let sys = b.graph.scan_lanes.iter().any(|x| x.node_id == l.scan.plan.plan_node_id && !(write && x.sys == TABLEOID_LANE));
    let plain = site.walk.is_none() && site.set.is_empty() && !site.rows_via_am && !(rowid && !write) && !sys;
    plain && open.union.is_none() && open.desc.consumes.is_empty() && open.ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_))) && bare_scan(b, l)
}

/// The `tableoid` system lane's bit alone (`ScanLanes::sys`, ops/scan/mod.rs `SYS_LANES`).
const TABLEOID_LANE: u8 = 1 << 5;

/// D3585: `s` scans a plain table with no sample and no security quals, holds no InitPlan, subtree or SubPlan, and its
/// clauses read no system column and no whole row (a ctid, a tableoid or a row value is its own relation's), every node
/// of a kind the expression walk knows.
/// D3681: a write's scan's target list may read its own `ctid` and `tableoid` too (`identity_columns`).
fn bare_scan(b: &Builder<'_, '_>, s: &SeqScan<'_>) -> bool {
    let p = &s.scan.plan;
    let tlist = |n: Node<'_>| if b.write_set >= 0 { identity_columns(n, s.scan.scanrelid as i32) } else { plain_columns(n) };
    bare_rel(b, s) && !p.qual.iter().chain(p.targetlist.iter()).any(crate::expr::has_subplan) && p.qual.iter().all(plain_columns) && p.targetlist.iter().all(tlist)
}

/// `bare_scan`'s checks but the clauses': a plain table with no sample and no security quals, no InitPlan and no subtree.
fn bare_rel(b: &Builder<'_, '_>, s: &SeqScan<'_>) -> bool {
    let p = &s.scan.plan;
    let rte = b.rte(s.scan.scanrelid).filter(|r| r.rtekind == types_nodes::parsenodes::RTEKind::RTE_RELATION && r.relid != 0 && r.relkind == b'r' && r.tablesample.is_none() && r.securityQuals.is_nil());
    rte.is_some() && p.initPlan.is_nil() && p.lefttree.is_none() && p.righttree.is_none()
}

/// No system column and no whole row in `n`, every node of a kind the expression walk knows.
fn plain_columns(n: Node<'_>) -> bool {
    crate::expr::walk(n, &mut |x| !x.as_var().is_some_and(|v| v.varattno <= 0))
}

/// D3681: `plain_columns`, but scan `varno`'s own `ctid` (-1) and `tableoid` (-6), a write's row identity, read too.
fn identity_columns(n: Node<'_>, varno: i32) -> bool {
    crate::expr::walk(n, &mut |x| !x.as_var().is_some_and(|v| v.varattno <= 0 && !(v.varlevelsup == 0 && v.varno == varno && matches!(v.varattno, -1 | -6))))
}

/// D3585: `s` reads its relation as `l` reads its own: the same plan fields but the relation (its clauses equal with each
/// one's range-table entry for the other's, `equal_as`), and a twin relation (`Facts::heap_twin`). D3615 (a): `l` passed
/// `bare_scan` (`leads`), and clauses equal to its own hold no SubPlan, system column or whole row either, so `s`'s clauses
/// are compared, never walked again (each arm's walk was a share of a fresh session's shared hit over 4000 partitions).
fn twin(b: &Builder<'_, '_>, l: &SeqScan<'_>, s: &SeqScan<'_>) -> bool {
    let (p, q) = (&l.scan.plan, &s.scan.plan);
    let lists = |x: &NodeList<'_>, y: &NodeList<'_>| x.len() == y.len() && x.iter().zip(y.iter()).all(|(m, n)| types_nodes::equal_as(m, n, l.scan.scanrelid as i32, s.scan.scanrelid as i32));
    let cols = |x: &Option<types_nodes::bitmapset::Bitmapset<'_>>, y: &Option<types_nodes::bitmapset::Bitmapset<'_>>| match (x, y) {
        (None, None) => true,
        (Some(x), Some(y)) => x.equal(y),
        _ => false,
    };
    let relid = |x: &SeqScan<'_>| b.rte(x.scan.scanrelid).map_or(0, |r| r.relid);
    bare_rel(b, s) && p.parallel_aware == q.parallel_aware && p.parallel_safe == q.parallel_safe && p.async_capable == q.async_capable && p.extParam.equal(&q.extParam) && p.allParam.equal(&q.allParam)
        && cols(&l.cb_scan_cols, &s.cb_scan_cols) && cols(&l.cb_target_cols, &s.cb_target_cols) && l.pgrc_merge == s.pgrc_merge && lists(&p.qual, &q.qual) && lists(&p.targetlist, &q.targetlist) && b.facts.heap_twin(relid(l), relid(s))
}

/// D3585: twin relation `relid` (plan `plan`'s rows) joins the run whose first scan planned `open`: the site reads it after
/// the run's others.
fn join_twin(open: &mut Open<'_>, relid: u32, plan: &types_nodes::plannodes::Plan<'_>) {
    let Source::Range { site, .. } = &mut open.source else { return };
    if site.set.is_empty() {
        let first = site.relation;
        site.set.push(first);
    }
    site.set.push(relid);
    let rows = Builder::rows(plan);
    open.desc.predicted_rows = open.desc.predicted_rows.saturating_add(rows);
    open.desc.end_rows = open.desc.end_rows.saturating_add(rows);
}

/// The order an Append's mark declares (`gravity_sort_col_idx` / `_sort_operators` / `_nulls_first`, set by
/// `create_append_plan` from the path's pathkeys); none without a mark or with an operator outside the table and the facts.
fn ordered_mark(facts: &dyn crate::catalog::Facts, a: &Append<'_>) -> Order {
    let mut order = Order::NONE;
    let n = a.gravity_sort_col_idx.len();
    if n == 0 || a.gravity_sort_operators.len() < n || a.gravity_nulls_first.len() < n {
        return order;
    }
    for i in 0..n {
        let Some(desc) = facts.key(crate::catalog::KeyReq::sort(a.gravity_sort_operators[i])).and_then(crate::catalog::KeyOrder::dir) else { return Order::NONE };
        order.push(OrderKey { col: Col::outer(a.gravity_sort_col_idx[i]), desc, nulls_first: a.gravity_nulls_first[i] });
    }
    order
}

fn merge_keys<'p>(b: &Builder<'p, '_>, m: &MergeAppend<'p>, k: Kind, id: i32) -> Result<ArenaVec<SortKey>, Refusal> {
    let n = m.numCols as usize;
    if n == 0 || m.sortColIdx.len() < n || m.sortOperators.len() < n || m.nullsFirst.len() < n {
        return Err(Refusal::shape(k, id, "a MergeAppend without keys"));
    }
    let mut keys = ArenaVec::with_capacity_in(n, b.arena);
    for i in 0..n {
        let opno = m.sortOperators[i];
        // USEROP (K10.y1 .h0): another btree family's ordering operator merges by that family's comparator, the CompareFn
        // key kind a Sort's key takes (sort/mod.rs `sort_keys`: lowerv4 gives it its comparator or refuses it by name);
        // a pattern family's (its order is the bytes' under "C", a Sort's own case), a family with no order proc for the
        // type (W8-EQUIVCLASS-SORTOP) or one whose family the catalog does not name (no child order to check) refuse
        let (desc, own_family) = match b.facts.key(crate::catalog::KeyReq::sort(opno)) {
            Some(o) if o.default_family => (o.desc, false),
            Some(o) if !crate::catalog::keys::key_order(opno).is_some_and(|p| p.bytes) && !o.no_proc => (o.desc, true),
            _ => return Err(Refusal::shape(k, id, "a sort operator outside the direction table")),
        };
        let t = column_type(&m.plan.targetlist, m.sortColIdx[i], k, id)?;
        let key = SortKey::of_type(b.facts, m.sortColIdx[i], opno, desc, m.nullsFirst[i], t, crate::ops::shared::sort::key_coll(b.facts, t, m.collations.get(i).copied().unwrap_or(0), k, id)?);
        // D3374 (e) (record keys E2): an anonymous record key whose arms disagree on its columns
        let per_row = !own_family && crate::catalog::anonymous_record(t) && crate::facts::record::node_per_row(b, id, m.sortColIdx[i]);
        keys.push(SortKey { own_family, per_row, ..key });
    }
    Ok(keys)
}

fn map_merge_append<'p>(b: &mut Builder<'p, '_>, m: &'p MergeAppend<'p>) -> Result<Open<'p>, Refusal> {
    let (plan, k) = (&m.plan, Kind::MergeAppend);
    let id = plan.plan_node_id;
    if m.mergeplans.is_nil() {
        return Err(Refusal::shape(k, id, "a MergeAppend without children"));
    }
    if !plan.qual.is_nil() {
        return Err(Refusal::shape(k, id, "a qual on MergeAppend"));
    }
    let prune = prune_of(b, m.part_prune_index, k, id)?;
    let keys = merge_keys(b, m, k, id)?;
    let layout = keys_layout(b.arena, keys.iter().map(|x| x.kind));
    // a key of another family's order is declared under its family (`Order::fams`, as a Sort's: sort/mod.rs `step`), so
    // each child is checked against that family's order and no default-family consumer reads the merge's as its own
    let mut order = Order::NONE;
    for key in keys.iter() {
        let fam = if key.own_family { b.facts.key(crate::catalog::KeyReq::sort(key.opno)).filter(|o| !o.default_family).map(|o| o.family).ok_or(Refusal::shape(k, id, "a sort operator outside the direction table"))? } else { 0 };
        order.push_fam(OrderKey { col: Col::outer(key.resno), desc: key.desc, nulls_first: key.nulls_first }, fam);
    }
    // D2369: a u8 count wrapped at 256 children (lowerv4 then found no merge for the arms: a Bug refusal); the Append's cap
    let n = u16::try_from(m.mergeplans.len()).map_err(|_| Refusal::shape(k, id, "more than 65535 MergeAppend children"))?;
    let mut slot = None;
    for c in m.mergeplans.iter() {
        let open = b.plan(c)?;
        slot = Some(feed(b, open, slot, BreakerKind::Sort, layout, order, k, id)?);
    }
    let slot = slot.unwrap();
    let mut out = crate::ops::shared::breaker::emit(b, id, slot);
    out.push(Op::Append(AppendDesc::Merge { slot, children: n, keys, prune }));
    ops::project(&mut out, &plan.targetlist);
    b.graph.note(k);
    Ok(out)
}

pub fn map<'p>(b: &mut Builder<'p, '_>, node: Node<'p>, k: Kind) -> Result<Open<'p>, Refusal> {
    match k {
        Kind::Append => map_append(b, node.as_append().unwrap()),
        Kind::MergeAppend => map_merge_append(b, node.as_merge_append().unwrap()),
        _ => Err(Refusal::shape(k, STMT_NODE, "a PartitionPruneInfo is attached to an Append, not a plan node")),
    }
}

const PRUNE_NAMES: [&str; 4] = ["static", "initial", "exec", "initial+exec"];

fn dump_prune(p: &Option<PruneDesc<'_>>, out: &mut dyn Write) -> fmt::Result {
    let Some(p) = p else { return Ok(()) };
    write!(out, " prune={}", PRUNE_NAMES[p.initial as usize + 2 * p.exec as usize])?;
    for l in p.info.prune_infos.iter() {
        for r in l.as_list().map_or(NodeList::nil().iter(), |l| l.iter()) {
            let r = r.as_partitioned_rel_prune_info().unwrap();
            write!(out, " [rti={} parts={} present=(", r.rtindex, r.nparts)?;
            for (i, m) in r.present_parts.iter().enumerate() {
                write!(out, "{}{m}", if i > 0 { "," } else { "" })?;
            }
            write!(out, ") steps={}", r.initial_pruning_steps.len())?;
            if !r.exec_pruning_steps.is_nil() {
                write!(out, " exec_steps={} params=(", r.exec_pruning_steps.len())?;
                for (i, m) in r.execparamids.iter().enumerate() {
                    write!(out, "{}$exec{m}", if i > 0 { "," } else { "" })?;
                }
                write!(out, ")")?;
            }
            write!(out, "]")?;
        }
    }
    Ok(())
}

impl AppendDesc<'_> {
    pub fn dump(&self, out: &mut dyn Write) -> fmt::Result {
        match self {
            AppendDesc::Feed { slot, ordered } => write!(out, "  feed slot={slot}{}", if *ordered { " ordered" } else { "" }),
            AppendDesc::Arm { i, n, ordered, prune } => {
                write!(out, "  append arm {i}/{n}{}", if *ordered { " ordered" } else { "" })?;
                dump_prune(prune, out)
            }
            AppendDesc::Union { slot, children, prune } => {
                write!(out, "  append union slot={slot} children={children}")?;
                dump_prune(prune, out)
            }
            AppendDesc::Merge { slot, children, keys, prune } => {
                write!(out, "  append merge slot={slot} children={children} keys=")?;
                crate::ops::shared::sort::dump_keys(keys, out)?;
                dump_prune(prune, out)
            }
        }
    }
}
