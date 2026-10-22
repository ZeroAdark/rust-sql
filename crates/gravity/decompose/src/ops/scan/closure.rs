use gravity_program::{ArenaVec, End, Pipeline, Reason, SiteDesc};
use mcx::GrowIn;
use types_nodes::list::NodeList;
use types_nodes::parsenodes::TableSampleClause;
use types_nodes::plannodes::{BitmapHeapScan, Plan, SampleScan, TableFuncScan};
use types_nodes::primnodes::{ParamKind, TableFunc, TableFuncType};
use types_nodes::{Node, NodeTag};

use crate::expr;
use crate::catalog::scan::{tid_op, TidOp};
use crate::catalog::{Am, RelFact};
use crate::graph::Source;
use crate::ledger::Kind;
use crate::ops;
use crate::admit::Refusal;
use crate::ops::scan as sources;
use crate::ops::shared::scan::{Atts, SELF_ITEM_POINTER, TSM_BERNOULLI, TSM_SYSTEM};
use crate::walk::{Builder, Open, STMT_NODE};
use crate::ops::shared::scan::{BitmapPlan, TidForm};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleMethod {
    System,
    Bernoulli,
    Other,
}

#[derive(Clone, Copy)]
pub struct SampleDesc<'p> {
    pub method: SampleMethod,
    pub handler: u32,
    pub args: &'p NodeList<'p>,
    pub repeatable: Option<Node<'p>>,
    /// D1643: the values (bit `i` = the `i`th of the arguments then the REPEATABLE value) read at each open of the walk
    /// (lowerv4 source/sample.rs, the host's as a computed one) that read no executor parameter: the lifetime home's
    /// `sample_at` answers `Rescan` for them (VOLATILE or may raise, in a scan other than the statement's first fetch), as
    /// tablesample_init evaluates them at each (re)scan's first fetch and never for a scan that fetches nothing.
    pub per_open: u8,
}

impl<'p> SampleDesc<'p> {
    /// D1643: whether value `v` (an argument or the REPEATABLE value, by node) is evaluated at each open (`per_open`).
    pub fn opens(&self, v: Node<'_>) -> bool {
        self.args.iter().chain(self.repeatable).position(|a| a.ptr_eq(v)).is_some_and(|i| i < 8 && self.per_open & (1 << i) != 0)
    }
}

impl TidForm {
    pub const fn name(self) -> &'static str {
        match self {
            TidForm::Points => "points",
            TidForm::Range => "range",
            TidForm::Current => "current",
        }
    }
}

pub fn map<'p>(b: &mut Builder<'p, '_>, node: Node<'p>, k: Kind) -> Result<Open<'p>, Refusal> {
    match k {
        Kind::BitmapHeapScan => map_bitmap_heap(b, node.as_bitmap_heap_scan().unwrap()),
        Kind::TidScan => {
            let s = node.as_tid_scan().unwrap();
            map_tid(b, &s.scan.plan, s.scan.scanrelid, &s.tidquals, TidForm::Points, k)
        }
        Kind::TidRangeScan => {
            let s = node.as_tid_range_scan().unwrap();
            map_tid(b, &s.scan.plan, s.scan.scanrelid, &s.tidrangequals, TidForm::Range, k)
        }
        Kind::SampleScan => map_sample(b, node.as_sample_scan().unwrap()).map(|(open, _ahead)| open),
        Kind::TableFuncScan => map_table_func(b, node.as_table_func_scan().unwrap()),
        Kind::NamedTuplestoreScan => super::named::map_named(b, node.as_named_tuplestore_scan().unwrap()),
        _ => Err(Refusal::shape(k, node.as_plan().map_or(STMT_NODE, |p| p.plan_node_id), "a bitmap plan node outside a BitmapHeapScan")),
    }
}

fn heap_site<'p>(b: &mut Builder<'p, '_>, rel: RelFact, plan: &'p Plan<'p>, scanrelid: u32, extra: Option<&'p NodeList<'p>>, k: Kind) -> Result<(SiteDesc, Atts), Refusal> {
    let id = plan.plan_node_id;
    if !matches!(rel.am, Am::Heap | Am::Pgrcolumnar) {
        return Err(Refusal::shape(k, id, "table access method"));
    }
    let mut atts = Atts::new(b.arena);
    let extra = extra.unwrap_or(&plan.qual);
    sources::read_vars(b, plan, &[&plan.qual, extra], scanrelid as i32, k, id, &mut atts)?;
    if !crate::ops::shared::carry::atts_for(b, scanrelid, &mut atts) || !sources::whole_row_atts(b, rel.relid, &mut atts) {
        return Err(Refusal::shape(k, id, "more than 4096 lanes (the sanity limit)"));
    }
    let mut site = SiteDesc::range_source(b.arena, rel.relid, atts.as_slice());
    sources::lanes(b, &mut site, rel.relid, rel.am, atts.as_slice(), k, id)?;
    sources::rowid_lane(&mut site, &atts);
    sources::scan_lanes(b, id, scanrelid, rel.relid, &atts);
    if rel.am == Am::Pgrcolumnar {
        sources::zone(b, &mut site, scanrelid as i32, &atts, &plan.qual, &plan.targetlist, None);
        if !core::ptr::eq(extra, &plan.qual) {
            sources::zone(b, &mut site, scanrelid as i32, &atts, extra, &plan.targetlist, None);
        }
    }
    Ok((site, atts))
}

fn map_bitmap_heap<'p>(b: &mut Builder<'p, '_>, bs: &'p BitmapHeapScan<'p>) -> Result<Open<'p>, Refusal> {
    let (plan, k) = (&bs.scan.plan, Kind::BitmapHeapScan);
    let id = plan.plan_node_id;
    let rel = b.relation(bs.scan.scanrelid, k, id)?;
    if plan.righttree.is_some() {
        return Err(Refusal::shape(k, id, "an unexpected right subtree"));
    }
    let tree = plan.lefttree.ok_or(Refusal::shape(k, id, "no bitmap subtree"))?;
    let bitmap = bitmap_plan(b, tree, rel)?;
    // a heap bitmap's recheck is the host's, compiled over its own slot (exec sites_heap.rs `recheck`): a column only the
    // recheck reads gets no lane, so a page window deforms nothing for it
    // the morsel form (`morsel_form`) reads the recheck's columns too: its filter tests `bitmapqualorig` on a page that asks
    let morsel = morsel_form(b, rel, plan, &bitmap, &bs.bitmapqualorig);
    let lanes_of = if rel.am != Am::Heap && rel.index_fetch() { RelFact { am: Am::Heap, ..rel } } else { rel };
    let (site, _) = heap_site(b, lanes_of, plan, bs.scan.scanrelid, (rel.am != Am::Heap || morsel).then_some(&bs.bitmapqualorig), k)?;
    let source = Source::Bitmap { site, am: if rel.rows { Am::Heap } else { rel.am }, scanrelid: bs.scan.scanrelid as u16, relid: rel.relid, plan: bitmap, recheck: &bs.bitmapqualorig, morsel };
    let desc = if morsel { Pipeline::eligible(0, End::Sink, Builder::rows(plan)) } else { Pipeline::serial(0, End::Sink, Reason::IndexSource) };
    let mut open = Open::new(b.arena, id, source, desc);
    bitmap_nodes(tree, &mut open); // the bitmap subtree's nodes run inside the source: before the heap scan's entry
    if morsel {
        // the morsel form's heap walk has no host recheck: `bitmapqualorig` filters the rows of a page that asks (a lossy
        // page, or an exact page an AM marked: GIN, BRIN, a hash), before the plan's own qual
        ops::filters(&mut open, &bs.bitmapqualorig);
    }
    ops::filters(&mut open, &plan.qual);
    ops::project(&mut open, &plan.targetlist);
    b.graph.note(k);
    Ok(open)
}

/// The bitmap heap scan's morsel form. The leader builds the bitmap once per execution, as PostgreSQL's parallel leader
/// does, and the pool walks its frozen entries as a heap range site (hosts bitmap_morsel.rs) with `bitmapqualorig` as a
/// filter on every page whose entry asks for the recheck (lossy, or an AM's recheck flag; lowerv4 step/filter.rs `guard`).
/// Taken on a heap relation whose bitmap keys and recheck read no executor parameter (a nest inner's or SubPlan's per-open
/// key keeps the serial source and its per-seek bitmap, D2970), in a plain SELECT (no row marks, no data-modifying CTE: a
/// write's scan runs on its own driver, which starts no pipeline through D899's late words, where the bitmap is built),
/// and whose estimate reaches `gravity_program::cost::BITMAP_MORSEL_MIN_ROWS`: a smaller bitmap keeps today's serial
/// source.
fn morsel_form(b: &Builder<'_, '_>, rel: RelFact, plan: &Plan<'_>, bitmap: &BitmapPlan<'_>, recheck: &NodeList<'_>) -> bool {
    fn keys_free(p: &BitmapPlan<'_>) -> bool {
        crate::walk::stack_check();
        match p {
            BitmapPlan::Index { keys, .. } => keys.iter().all(|k| !reads_exec(k.value)),
            BitmapPlan::And { plans, .. } | BitmapPlan::Or { plans, .. } => plans.iter().all(keys_free),
        }
    }
    plain_read(b, rel) && crate::admit::plan_rows(plan) >= gravity_program::cost::BITMAP_MORSEL_MIN_ROWS && keys_free(bitmap) && recheck.iter().all(|q| !reads_exec(q))
}

/// A heap relation read by a plain SELECT: no row marks, no data-modifying CTE (a write's scan runs on its own driver).
fn plain_read(b: &Builder<'_, '_>, rel: RelFact) -> bool {
    b.stmt.commandType == types_nodes::nodes_enums::CmdType::CMD_SELECT && b.stmt.rowMarks.is_nil() && !b.stmt.hasModifyingCTE && rel.am == Am::Heap
}

/// An index scan as the bitmap morsel form (`morsel_form`): the leader walks the index into a bitmap of its TIDs once
/// per execution and the pool walks the bitmap's heap pages, each worker following a TID's HOT chain to the version its
/// snapshot sees (hosts bitmap_morsel.rs `keep_page`). Taken only where no reader depends on the scan's row order
/// (`Builder::order_free`: the form hands rows up in heap block order), for a forward or backward scan of a btree that
/// is not partial, with no ORDER BY operator, not index-only (its visibility-map answer stays the serial source's), under `morsel_form`'s own
/// tests. `indexqualorig` is the recheck a lossy page needs; a btree page is exact otherwise. The workers set no index
/// hint (LP_DEAD): a bitmap heap scan never does, a hint is only a hint; nor does the heap range form, which visits no
/// entry. Rows never differ, only a later scan's heap fetches. The ordered walk (hosts `IndexSource`, through indexam's
/// `index_fetch_heap`) keeps PostgreSQL's marks (`kill_prior_tuple`). `None`: today's serial index source.
#[allow(clippy::too_many_arguments)]
/// The estimate an order-free index scan needs for the morsel form by the session's class (D3047's `Facts::serial`, keyed
/// by exec's front, so each class gets its own graph): a serial session's (the serial index source is faster at W0
/// below it) or one with workers'. Off a backend (no class) the serial one.
fn index_morsel_min_rows(b: &Builder<'_, '_>) -> f64 {
    if b.facts.serial() == Some(false) { gravity_program::cost::INDEX_MORSEL_MIN_ROWS_WIDE } else { gravity_program::cost::INDEX_MORSEL_MIN_ROWS }
}

pub(crate) fn index_morsel<'p>(b: &mut Builder<'p, '_>, rel: RelFact, plan: &'p Plan<'p>, scanrelid: u32, indexid: u32, keys: &gravity_program::ArenaVec<crate::graph::IndexKey<'p>>, iam: crate::catalog::IndexAm, recheck: &'p NodeList<'p>, orderby: &'p NodeList<'p>, k: Kind) -> Result<Option<Open<'p>>, Refusal> {
    if !b.order_free || iam != crate::catalog::IndexAm::Btree || !orderby.is_nil() {
        return Ok(None);
    }
    let id = plan.plan_node_id;
    // A full walk (no index qual) of a btree that indexes every row (not partial: PostgreSQL drops the quals the
    // predicate implies, which a heap walk would need) reads the heap's rows in some order: the heap range source, as a
    // sequential scan, in a plain read (no row mark, no write: the walk takes no tuple lock). At any estimate: it reads
    // the relation once in block order where the index walk reads every leaf and the heap in key order (its estimate is
    // the filter's rows, not the walk's)
    if keys.is_empty() && plain_read(b, rel) && b.facts.index(indexid).is_some_and(|f| f.btree && !f.partial) {
        fires::fire("index.full.range"); // shapecheck fire signal: a full btree walk read as the heap range
        return full_range(b, rel, plan, scanrelid, (indexid, false), k).map(Some);
    }
    // A partial index's entries answer its predicate, which the scan's quals need not repeat (PostgreSQL drops the quals
    // the predicate implies, create_indexscan_plan): a lossy page of the bitmap returns every row of the page that passes
    // the recheck, rows outside the predicate too, so a partial index keeps the serial source (PostgreSQL's own bitmap
    // path rechecks the predicate, create_bitmap_subplan)
    if plan.plan_rows < index_morsel_min_rows(b) || b.facts.index(indexid).is_none_or(|f| f.partial) {
        return Ok(None);
    }
    let mut ks = ArenaVec::new_in(b.arena);
    keys.iter().for_each(|x| ks.push(*x));
    let bitmap = BitmapPlan::Index { node_id: id, indexid, keys: ks, rows: Builder::rows(plan), iam };
    if !morsel_form(b, rel, plan, &bitmap, recheck) {
        return Ok(None);
    }
    let (mut site, _) = heap_site(b, rel, plan, scanrelid, Some(recheck), k)?;
    // D3581: the plan's Index Scan counts its heap fetches on the index (`index_fetch_heap`), not as a bitmap heap scan's
    site.walk = Some((indexid, false));
    let source = Source::Bitmap { site, am: rel.am, scanrelid: scanrelid as u16, relid: rel.relid, plan: bitmap, recheck, morsel: true };
    let mut open = Open::new(b.arena, id, source, Pipeline::eligible(0, End::Sink, Builder::rows(plan)));
    ops::filters(&mut open, recheck);
    ops::filters(&mut open, &plan.qual);
    ops::project(&mut open, &plan.targetlist);
    b.graph.note(k);
    Ok(Some(open))
}

/// A full index walk's heap range form (`index_morsel`, `index_only_full`): the relation's rows in block order, the scan's
/// filter and projection over them; `plan`'s lists read the relation's own columns. `walk`: the plan's walk (its index, and
/// whether it is index-only), whose statistics the source counts (D3475, `SiteDesc::walk`).
fn full_range<'p>(b: &mut Builder<'p, '_>, rel: RelFact, plan: &'p Plan<'p>, scanrelid: u32, walk: (u32, bool), k: Kind) -> Result<Open<'p>, Refusal> {
    let (mut site, _) = heap_site(b, rel, plan, scanrelid, None, k)?;
    site.walk = Some(walk);
    let source = Source::Range { site, am: rel.am, scanrelid: scanrelid as u16, relid: rel.relid };
    let mut open = Open::new(b.arena, plan.plan_node_id, source, Pipeline::eligible(0, End::Sink, Builder::rows(plan)));
    ops::filters(&mut open, &plan.qual);
    ops::project(&mut open, &plan.targetlist);
    b.graph.note(k);
    Ok(open)
}

/// An order-free full Index Only Scan (no index qual) read as `index_morsel`'s
/// heap range form, under its tests (a btree that is not partial, no ORDER BY operator, a plain read), when
/// every index column is a plain attribute of the relation (an expression column has no heap lane), however wide the
/// heap is next to the index. The scan's own lists read index columns (`INDEX_VAR` column `n`, the index target list's `n`th
/// entry, a column of the relation); the form reads them as that column: an IndexScan node with no index qual over the
/// same index, its lists over the relation, made in the decomposition's arena with the scan's plan node id. Its rows are
/// the index-only scan's: every row of the relation the snapshot sees, its indexed columns. `None`: the index-only walk.
#[allow(clippy::too_many_arguments)]
pub(crate) fn index_only_full<'p>(b: &mut Builder<'p, '_>, rel: RelFact, plan: &'p Plan<'p>, scanrelid: u32, indexid: u32, keys: &gravity_program::ArenaVec<crate::graph::IndexKey<'p>>, iam: crate::catalog::IndexAm, orderby: &'p NodeList<'p>, indextlist: &'p NodeList<'p>, dir: i32, k: Kind) -> Result<Option<Open<'p>>, Refusal> {
    let Some(mcx) = b.alloc else { return Ok(None) };
    if !b.order_free || iam != crate::catalog::IndexAm::Btree || !orderby.is_nil() || !keys.is_empty() || !plain_read(b, rel) {
        return Ok(None);
    }
    if !b.facts.index(indexid).is_some_and(|f| f.btree && !f.partial && f.keys().iter().all(|&a| a > 0)) {
        return Ok(None);
    }
    // the index target list's entries, each a column of the relation (`INDEX_VAR` column n is entry n)
    let mut cols: mcx::PgVec<'_, &'p types_nodes::primnodes::Var> = mcx::reserved_in(indextlist.len(), b.mcx);
    for t in indextlist.iter() {
        match t.as_target_entry().and_then(|t| t.expr.as_var()).filter(|v| v.varno == scanrelid as i32 && v.varlevelsup == 0 && v.varattno > 0) {
            Some(v) => cols.push_in(v),
            None => return Ok(None),
        }
    }
    let made = (|| -> types_error::PgResult<Option<Node<'p>>> {
        let (Some(tl), Some(qual)) = (heap_list(mcx, &plan.targetlist, &cols[..])?, heap_list(mcx, &plan.qual, &cols[..])?) else { return Ok(None) };
        let p = Plan { disabled_nodes: plan.disabled_nodes, startup_cost: plan.startup_cost, total_cost: plan.total_cost, plan_rows: plan.plan_rows, plan_width: plan.plan_width, parallel_aware: false, parallel_safe: plan.parallel_safe, async_capable: false, plan_node_id: plan.plan_node_id, targetlist: tl, qual, lefttree: None, righttree: None, initPlan: plan.initPlan.clone_in(mcx)?, extParam: plan.extParam.clone_in(mcx)?, allParam: plan.allParam.clone_in(mcx)? };
        let scan = types_nodes::plannodes::IndexScan { scan: types_nodes::plannodes::Scan { plan: p, scanrelid }, indexid, indexorderdir: dir, ..Default::default() };
        Node::mk(mcx, scan).map(Some)
    })();
    let Some(is) = made.map_err(|_| Refusal::shape(k, plan.plan_node_id, "an index-only scan's lists over its relation: out of memory or stack"))?.and_then(|n| n.as_index_scan()) else { return Ok(None) };
    fires::fire("index.only.full.range"); // shapecheck fire signal: a full index-only walk read as the heap range
    full_range(b, rel, &is.scan.plan, scanrelid, (indexid, true), k).map(Some)
}

/// An order-free Index Only Scan the pool reads by leaf pages (hosts index_leaves.rs: every participant seizes leaves of
/// one shared parallel btree walk; an entry whose heap page the visibility map vouches for is read from the index tuple,
/// the others from the heap in block order): a btree, no ORDER BY operator, a plain read, in a session with workers
/// (`Facts::serial`, D3047: on the leader alone the serial walk is the same walk), an estimate past
/// `INDEX_MORSEL_MIN_ROWS_WIDE`, and no system column of the scan's own (`ctid`, `tableoid`: a CURRENT OF cursor's
/// identity). Any index qual and a partial index are the walk's own: its rows are the index-only scan's. The site keeps
/// `SiteKind::IndexSource { leaves }` and its pipeline is eligible; exec takes the form only past two leaf workers
/// (runner plan.rs `leaf_width`), else the leader walks it serially. EXPLAIN text unchanged.
/// D3522: and no key reads an executor parameter (`keys_bound`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn index_only_leaves(b: &Builder<'_, '_>, rel: RelFact, plan: &Plan<'_>, indexid: u32, iam: crate::catalog::IndexAm, orderby: &NodeList<'_>, own_sys: bool, keys: &[crate::graph::IndexKey<'_>]) -> bool {
    let leaves = b.order_free && iam == crate::catalog::IndexAm::Btree && orderby.is_nil() && plain_read(b, rel) && b.facts.serial() == Some(false) && plan.plan_rows >= gravity_program::cost::INDEX_MORSEL_MIN_ROWS_WIDE && !own_sys && b.facts.index(indexid).is_some_and(|f| f.btree) && keys_bound(keys);
    if leaves {
        fires::fire("index.only.leaves"); // shapecheck fire signal: an index-only walk the pool reads by leaf pages
    }
    leaves
}

/// An ordered btree scan the pool reads in key ranges (hosts index_ranges.rs: the claims are runs of key ranges cut at
/// the index's own separator keys, each read in index order by one participant; the emit hands the claims on in claim
/// order, so the rows leave in the index's order): the scan is the statement's top row source (`at_top`: the plan's root,
/// or under the root's Result), whose order the emit keeps, read forward over a btree with no ORDER BY operator, a plain
/// read in a session with workers (`Facts::serial`), an estimate past `INDEX_MORSEL_MIN_ROWS_WIDE`, and no system column
/// of the scan's own. Any index qual and a partial index are the ranges' own. The site keeps
/// `SiteKind::IndexSource { ranges }` and its pipeline is eligible; exec takes the form only when the run's ranges
/// hold (runner plan.rs), else the leader walks it serially. EXPLAIN text unchanged.
/// D3522: and no key reads an executor parameter (`keys_bound`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn index_ranges(b: &Builder<'_, '_>, rel: RelFact, plan: &Plan<'_>, indexid: u32, iam: crate::catalog::IndexAm, orderby: &NodeList<'_>, own_sys: bool, backward: bool, keys: &[crate::graph::IndexKey<'_>]) -> bool {
    let ranges = !b.order_free && !backward && iam == crate::catalog::IndexAm::Btree && orderby.is_nil() && plain_read(b, rel) && b.facts.serial() == Some(false) && plan.plan_rows >= gravity_program::cost::INDEX_MORSEL_MIN_ROWS_WIDE && !own_sys && b.facts.index(indexid).is_some_and(|f| f.btree) && at_top(b, plan) && keys_bound(keys);
    if ranges {
        fires::fire("index.ranges"); // shapecheck fire signal: an ordered index walk the pool reads in key ranges
    }
    ranges
}

/// D3522 (Workers IO sig-36918c42daa4): the leaf walk and the key ranges are cut once per run on the leader from the
/// statement's words (runner plan.rs `build_leaves` / `build_ranges`), where a key over an executor parameter (an
/// InitPlan's value, read through its handle at the open, or a rescan's) has no word: its slot is -1 and the cut bound 0,
/// so `a > (SELECT 150)` walked from 0 and a by-reference key read a null pointer. Such a scan keeps the serial source,
/// which binds the key at its open (lowerv4 frame/site.rs `store_keys`), as `morsel_form` keeps a bitmap's.
fn keys_bound(keys: &[crate::graph::IndexKey<'_>]) -> bool {
    keys.iter().all(|k| !reads_exec(k.value))
}

/// The scan node `plan` is the statement's top row source: the plan's root, or the child of a root Result (its
/// projection and one-time gate run in the scan's pipeline, which ends in the statement's emit).
fn at_top(b: &Builder<'_, '_>, plan: &Plan<'_>) -> bool {
    let Some(root) = b.stmt.planTree else { return false };
    let at = (plan as *const Plan<'_>).cast::<u8>();
    let same = |p: &Plan<'_>| core::ptr::eq((p as *const Plan<'_>).cast::<u8>(), at);
    if root.as_plan().is_some_and(same) {
        return true;
    }
    root.as_result().is_some_and(|r| r.plan.lefttree.and_then(|c| c.as_plan()).is_some_and(same))
}

/// `l` with each `INDEX_VAR` column `n` read as `cols[n - 1]`, the relation's column; `None` when one names no entry.
fn heap_list<'p>(mcx: mcx::Mcx<'p>, l: &NodeList<'p>, cols: &[&'p types_nodes::primnodes::Var]) -> types_error::PgResult<Option<NodeList<'p>>> {
    let mut bad = false;
    let mut out = NodeList::nil();
    for n in l.iter() {
        let m = heap_vars(mcx, n, cols, &mut bad)?.unwrap_or(n);
        out.lappend(mcx, m)?;
    }
    Ok((!bad).then_some(out))
}

fn heap_vars<'p>(mcx: mcx::Mcx<'p>, x: Node<'p>, cols: &[&'p types_nodes::primnodes::Var], bad: &mut bool) -> types_error::PgResult<Option<Node<'p>>> {
    crate::walk::stack_check(); // D3748
    if let Some(v) = x.as_var().filter(|v| v.varno == types_nodes::primnodes::INDEX_VAR) {
        let Some(c) = usize::try_from(v.varattno).ok().and_then(|a| a.checked_sub(1)).and_then(|i| cols.get(i)).filter(|_| v.varlevelsup == 0) else {
            *bad = true;
            return Ok(None);
        };
        return Node::mk(mcx, types_nodes::primnodes::Var { varnullingrels: c.varnullingrels.clone_in(mcx)?, ..**c }).map(Some);
    }
    nodes_core::expression_tree_mutator_dyn(mcx, x, &mut |y| heap_vars(mcx, y, cols, bad))
}

fn bitmap_nodes<'p>(node: Node<'p>, open: &mut Open<'p>) {
    crate::walk::stack_check(); // D3748
    let kids = node.as_bitmap_and().map(|a| &a.bitmapplans).or_else(|| node.as_bitmap_or().map(|o| &o.bitmapplans));
    for c in kids.into_iter().flat_map(|l| l.iter()) {
        bitmap_nodes(c, open);
    }
    if let Some(p) = node.as_plan() {
        open.node_ops.push(crate::graph::NodeOps { node_id: p.plan_node_id, first_op: 0, producer_top: false });
    }
}

fn bitmap_plan<'p>(b: &mut Builder<'p, '_>, node: Node<'p>, rel: RelFact) -> Result<BitmapPlan<'p>, Refusal> {
    crate::walk::stack_check(); // D3748
    let k = Kind::of(node.node_tag()).ok_or(Refusal::shape(Kind::BitmapHeapScan, STMT_NODE, "not a plan node"))?;
    let id = node.as_plan().map_or(STMT_NODE, |p| p.plan_node_id);
    match k {
        Kind::BitmapIndexScan => {
            let s = node.as_bitmap_index_scan().unwrap();
            if !s.indexqualorig.is_nil() && s.indexqualorig.len() != s.indexqual.len() {
                return Err(Refusal::shape(k, id, "an index qual list unlike its original"));
            }
            let idx = b.facts.index(s.indexid).ok_or(Refusal::shape(k, id, "no index facts"))?;
            if idx.relid != rel.relid {
                return Err(Refusal::shape(k, id, "the index is not on the scanned relation"));
            }
            let keys = sources::index_keys(b, &s.indexqual, &idx, k, id)?;
            b.graph.note(k);
            Ok(BitmapPlan::Index { node_id: id, indexid: s.indexid, keys, rows: Builder::rows(&s.scan.plan), iam: b.facts.index_am(s.indexid) })
        }
        Kind::BitmapAnd | Kind::BitmapOr => {
            let list = if k == Kind::BitmapAnd { &node.as_bitmap_and().unwrap().bitmapplans } else { &node.as_bitmap_or().unwrap().bitmapplans };
            if list.len() < 2 {
                return Err(Refusal::shape(k, id, "a bitmap combination of fewer than two plans"));
            }
            let mut plans = ArenaVec::new_in(b.arena);
            for p in list.iter() {
                plans.push(bitmap_plan(b, p, rel)?);
            }
            b.graph.note(k);
            Ok(if k == Kind::BitmapAnd { BitmapPlan::And { node_id: id, plans } } else { BitmapPlan::Or { node_id: id, plans } })
        }
        _ => Err(Refusal::shape(Kind::BitmapHeapScan, id, "a bitmap subtree that is not a bitmap plan")),
    }
}

fn map_tid<'p>(b: &mut Builder<'p, '_>, plan: &'p Plan<'p>, scanrelid: u32, quals: &'p NodeList<'p>, form: TidForm, k: Kind) -> Result<Open<'p>, Refusal> {
    let id = plan.plan_node_id;
    let rel = b.relation(scanrelid, k, id)?;
    if quals.is_nil() {
        return Err(Refusal::shape(k, id, "a tid scan without tid clauses"));
    }
    let form = if form == TidForm::Points && quals.len() == 1 && quals.nth(0).node_tag() == NodeTag::T_CurrentOfExpr { TidForm::Current } else { form };
    if form != TidForm::Current {
        for q in quals.iter() {
            tid_clause(q, scanrelid as i32, form, k, id)?;
        }
    }
    // PGRCSERVE M3 (L1): a TID scan's relation of another AM is on the rows road (hosts am_rows.rs `identity`): the heap
    // protocol over its AM's rows, each TID fetched (or a range walked) by its AM (hosts tid_sample.rs), serial on the
    // leader as the rows road is. A native-road one is a stale road answer: refused
    if rel.am == Am::Pgrcolumnar {
        return Err(Refusal::shape(k, id, "a tid scan over a table AM off its rows road"));
    }
    let rel = if rel.rows { RelFact { am: Am::Heap, ..rel } } else { rel };
    let (site, _) = heap_site(b, rel, plan, scanrelid, Some(quals), k)?;
    let desc = match form {
        TidForm::Points | TidForm::Current => Pipeline::serial(0, End::Sink, Reason::IndexSource),
        TidForm::Range if rel.rows => Pipeline { predicted_rows: Builder::rows(plan), ..Pipeline::serial(0, End::Sink, Reason::Host) },
        TidForm::Range => Pipeline::eligible(0, End::Sink, Builder::rows(plan)),
    };
    let mut open = Open::new(b.arena, id, Source::Tid { site, am: rel.am, scanrelid: scanrelid as u16, relid: rel.relid, form, quals }, desc);
    ops::filters(&mut open, &plan.qual);
    ops::project(&mut open, &plan.targetlist);
    b.graph.note(k);
    Ok(open)
}

/// `ctid = value`, `ctid = ANY(array)` (points), `ctid op value` (a range bound); the value never reads a column.
fn tid_clause(q: Node<'_>, varno: i32, form: TidForm, k: Kind, id: i32) -> Result<(), Refusal> {
    let is_ctid = |n: Node<'_>| n.as_var().is_some_and(|v| v.varno == varno && v.varlevelsup == 0 && v.varattno == SELF_ITEM_POINTER);
    let (opno, args) = if let Some(op) = q.as_op_expr() {
        (op.opno, &op.args)
    } else if let Some(sa) = q.as_scalar_array_op_expr() {
        if form != TidForm::Points || !sa.useOr {
            return Err(Refusal::shape(k, id, "a tid array clause outside a point scan"));
        }
        (sa.opno, &sa.args)
    } else if q.node_tag() == NodeTag::T_CurrentOfExpr {
        return Err(Refusal::shape(k, id, "WHERE CURRENT OF beside another tid clause"));
    } else {
        return Err(Refusal::shape(k, id, "a tid clause that is not a comparison"));
    };
    if args.len() != 2 {
        return Err(Refusal::shape(k, id, "a tid clause that is not binary"));
    }
    let value = if is_ctid(args.nth(0)) {
        args.nth(1)
    } else if is_ctid(args.nth(1)) {
        args.nth(0)
    } else {
        return Err(Refusal::shape(k, id, "a tid clause without ctid"));
    };
    let ok = match (form, tid_op(opno)) {
        (TidForm::Points | TidForm::Current, Some(TidOp::Eq)) => true,
        (TidForm::Range, Some(TidOp::Less { .. } | TidOp::Greater { .. })) => true,
        _ => false,
    };
    if !ok {
        return Err(Refusal::shape(k, id, "a tid clause outside the tid comparison set"));
    }
    let mut has_var = false;
    expr::walk(value, &mut |n| {
        has_var |= n.node_tag() == NodeTag::T_Var;
        !has_var
    });
    if has_var {
        return Err(Refusal::shape(k, id, "a tid value referencing a column"));
    }
    Ok(())
}

/// D1643: PG evaluates a TABLESAMPLE's arguments in tablesample_init, at each (re)scan's first fetch, never for a scan
/// that fetches nothing; the site reads a value over no executor parameter as a statement word, evaluated once at the
/// bind. A VOLATILE one, or one that may raise (F5, an unknown counts) and reads no executor parameter, would run there
/// where PG does not, so it is evaluated at each open of the walk instead (`SampleDesc::per_open`: the host's, as a value
/// computed from executor parameters is), under the computed keys' rules (lowerv4 rerun/params_key.rs `input_cells`). A
/// VOLATILE or raising one is equal at the bind when the scan is the statement's first fetch (`ops::first_fetched`): PG
/// then evaluates it once, before any row leaves and after nothing else acted, so one call at the bind (and its error)
/// is PG's. More than eight values (no method takes them) keep the refusal. Construct C2: the quiet values bound at the
/// bind run ahead of PG's first fetch (perhaps of none) under `effects::quiet_ahead`'s licence over their effects, which
/// `map_sample` returns beside its Open; a loud one bound at the bind is at PG's point (the first fetch) and needs none.
const D1643: &str = "a TABLESAMPLE argument or REPEATABLE value that is VOLATILE or may raise, evaluated once per statement (PG: at each scan's first fetch; D1643)";

fn reads_exec(n: Node<'_>) -> bool {
    !expr::walk(n, &mut |x| x.as_param().is_none_or(|p| p.paramkind != ParamKind::PARAM_EXEC))
}

fn map_sample<'p>(b: &mut Builder<'p, '_>, ss: &'p SampleScan<'p>) -> Result<(Open<'p>, Option<crate::effects::Licence>), Refusal> {
    let (plan, k) = (&ss.scan.plan, Kind::SampleScan);
    let id = plan.plan_node_id;
    let rel = b.relation(ss.scan.scanrelid, k, id)?;
    // D4735: a relation on the rows road samples by its AM's sample scan (hosts am_rows.rs): the heap protocol over the
    // AM's rows, as its seq and TID scans
    let rows = rel.rows;
    let rel = if rows { RelFact { am: Am::Heap, ..rel } } else { rel };
    if !matches!(rel.am, Am::Heap | Am::Pgrcolumnar) {
        return Err(Refusal::shape(k, id, "a sample over a relation that is not a heap or pgrc table (no block sampling face)"));
    }
    let ts: &'p TableSampleClause<'p> = ss.tablesample.and_then(|n| n.as_table_sample_clause()).ok_or(Refusal::shape(k, id, "no tablesample clause"))?;
    // pgrc: the host masks each granule by the rows the method picks by their ctids (hosts granule_source.rs
    // `sample_mask`), a pure function of the position for BERNOULLI / SYSTEM only (SYSTEM_ROWS / SYSTEM_TIME pick by the
    // scan's progress)
    if rel.am == Am::Pgrcolumnar && !matches!(ts.tsmhandler, TSM_SYSTEM | TSM_BERNOULLI) {
        return Err(Refusal::shape(k, id, "a sample over a pgrc relation by a method other than BERNOULLI / SYSTEM"));
    }
    let first = ops::first_fetched(b.stmt.planTree).and_then(|n| n.as_plan()).is_some_and(|p| p.plan_node_id == plan.plan_node_id);
    let mut per_open = 0u8;
    // C2: the effects of the quiet values bound at the bind (`None`: none is)
    let mut quiet: Option<crate::effects::Effects> = None;
    for (i, a) in ts.args.iter().chain(ts.repeatable).enumerate() {
        if expr::has_subplan(a) {
            return Err(Refusal::shape(k, id, "a tablesample argument with a SubPlan"));
        }
        let f = crate::rerun::of_expr(b.stmt, b.facts, b.arena, a);
        // when it is evaluated is the lifetime home's (`sample_at`); one reading an executor parameter is computed at each
        // open already (lowerv4 params_key.rs `computed`), so only the others are marked
        let exec = reads_exec(a);
        let loud = crate::effects::scan::sample_arg_loud(&f, exec);
        if !exec && crate::lifetime::sample_at(first, loud, exec) == crate::lifetime::BoundAt::Rescan {
            if i >= 8 {
                return Err(Refusal::shape(k, id, D1643));
            }
            per_open |= 1 << i;
        } else if !exec && !loud {
            quiet = Some(quiet.unwrap_or(crate::effects::Effects::NONE).and(crate::effects::Effects::of_found(f)));
        }
    }
    // the quiet values' run at the bind, ahead of PG's first fetch: quiet_ahead's licence (it grants every such value, as
    // `sample_arg_loud` reads none of them VOLATILE or raising; a denial keeps D1643's refusal)
    let ahead = match quiet {
        Some(e) => Some(crate::effects::quiet_ahead(e).ok_or(Refusal::shape(k, id, D1643))?),
        None => None,
    };
    let method = match ts.tsmhandler {
        TSM_SYSTEM => SampleMethod::System,
        TSM_BERNOULLI => SampleMethod::Bernoulli,
        _ => SampleMethod::Other,
    };
    let (site, _) = heap_site(b, rel, plan, ss.scan.scanrelid, None, k)?;
    let site = if rows { site.rows_via_am() } else { site };
    let sample = SampleDesc { method, handler: ts.tsmhandler, args: &ts.args, repeatable: ts.repeatable, per_open };
    let mut open = Open::new(b.arena, id, Source::Sample { site, am: rel.am, scanrelid: ss.scan.scanrelid as u16, relid: rel.relid, sample }, Pipeline::serial(0, End::Sink, Reason::Host));
    ops::filters(&mut open, &plan.qual);
    ops::project(&mut open, &plan.targetlist);
    b.graph.note(k);
    Ok((open, ahead))
}

fn map_table_func<'p>(b: &mut Builder<'p, '_>, ts: &'p TableFuncScan<'p>) -> Result<Open<'p>, Refusal> {
    let (plan, k) = (&ts.scan.plan, Kind::TableFuncScan);
    let id = plan.plan_node_id;
    let func: &'p TableFunc<'p> = ts.tablefunc.and_then(|n| n.as_table_func()).ok_or(Refusal::shape(k, id, "no table function"))?;
    if func.docexpr.is_none() {
        return Err(Refusal::shape(k, id, "a table function without a document expression"));
    }
    if matches!(func.functype, TableFuncType::TFT_JSON_TABLE) && func.plan.is_none() {
        return Err(Refusal::shape(k, id, "a JSON_TABLE without its path plan"));
    }
    let source = Source::TableFunc { scanrelid: ts.scan.scanrelid as u16, func, cols: func.colnames.len() as u16, ordinality: func.ordinalitycol };
    crate::ops::shared::record_row::tablefunc_record_row(b, ts.scan.scanrelid, plan, func);
    let mut open = Open::new(b.arena, id, source, Pipeline::serial(-1, End::Sink, Reason::Host));
    ops::filters(&mut open, &plan.qual);
    ops::project(&mut open, &plan.targetlist);
    b.graph.note(k);
    Ok(open)
}

pub const fn table_func_name(t: TableFuncType) -> &'static str {
    match t {
        TableFuncType::TFT_XMLTABLE => "xmltable",
        TableFuncType::TFT_JSON_TABLE => "json_table",
    }
}

pub const fn sample_name(m: SampleMethod) -> &'static str {
    match m {
        SampleMethod::System => "system",
        SampleMethod::Bernoulli => "bernoulli",
        SampleMethod::Other => "other",
    }
}

impl<'p> BitmapPlan<'p> {
    pub fn dump(&self, depth: usize, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        for _ in 0..depth {
            write!(out, "  ")?;
        }
        match self {
            BitmapPlan::Index { node_id, indexid, keys, rows, iam } => {
                writeln!(out, "    bitmap index node={node_id} index={indexid} rows={rows}{}{}", if *iam == crate::catalog::IndexAm::Btree { "" } else { " iam=" }, if *iam == crate::catalog::IndexAm::Btree { "" } else { iam.name() })?;
                for key in keys.iter() {
                    for _ in 0..depth {
                        write!(out, "  ")?;
                    }
                    write!(out, "      key ")?;
                    key.dump(out)?;
                    writeln!(out)?;
                }
                Ok(())
            }
            BitmapPlan::And { node_id, plans } | BitmapPlan::Or { node_id, plans } => {
                writeln!(out, "    bitmap {} node={node_id}", if matches!(self, BitmapPlan::And { .. }) { "and" } else { "or" })?;
                for p in plans.iter() {
                    p.dump(depth + 1, out)?;
                }
                Ok(())
            }
        }
    }
}
