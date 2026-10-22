
use gravity_program::{source_serial, step_serial, NestProbe, NestShape, Reason, SourceShape, StepShape};
use nodes_core::{expression_tree_walker, NodeWalker};
use types_error::PgResult;
use types_nodes::{Node, NodeTag};
use types_pathnodes::run::PlannerRun;
use types_pathnodes::{tag16, GwMemo, PathId, PathNode, PlannerInfo, RelId};

const PAR: u8 = 1;
const NEED_FREE_END: u8 = 2;
const NEED_ORDER_FREE: u8 = 4;
const RANGE: u8 = 8;
const RAISED: u8 = 32;
const FIRST_ROW: u8 = 64;
/// Continues a hash join that drains its build (RIGHT, FULL, RIGHT ANTI, RIGHT SEMI): a join step after it is a tail op the
/// probe cannot run as its own, so the probe spools its rows and both it and the drain run on the leader (lowerv4
/// hashjoin/drain/spool.rs, `Gx::spooled_drain`'s `m` rule).
const DRAIN: u8 = 128;

const WHY_NONE: u8 = 0;
const WHY_HOST: u8 = 1;
const WHY_INDEX: u8 = 2;
const WHY_NOSITE: u8 = 3;
const WHY_SINK: u8 = 4;

const BTREE_AM_OID: u32 = 403;

/// The most groups a hashed aggregate's table (or rows a hash join's build) holds within the caches: past it the pipeline's
/// class is `HashAggWide` (`Build`), within it `HashAgg` (`BuildNarrow`), each its own measured rows.
/// MEASURED: a hash table of up to 262,144 entries probes 6.5-8.1x faster at W8, one of 4 M entries 1.34x (cost_profile
/// prim.hash.probe rows, box 40); the hashagg rows are measured at 4,096 groups, the hashagg_wide rows at 0.5-8 M, the
/// build_narrow rows at 16 K - 256 K build rows, the build rows at 16 K - 4 M.
const WIDE_GROUPS: f64 = 262144.0;

fn hash_end(groups: f64) -> End {
    if groups > WIDE_GROUPS {
        End::HashAggWide
    } else {
        End::HashAgg
    }
}

fn build_end(rows: f64) -> End {
    if rows > WIDE_GROUPS {
        End::BuildWide
    } else {
        End::Build
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// A hash join's build of at most `WIDE_GROUPS` rows.
    Build,
    BuildWide,
    HashAgg,
    HashAggWide,
    Fold,
    Sort,
    Sink,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Count,
    HashAgg,
    HashAggWide,
    Sort,
    Build,
    BuildNarrow,
    Range,
    Nest,
}

const CLASSES: [(Class, &str); 8] = [
    (Class::Count, "prim.pipe.count"),
    (Class::HashAgg, "prim.pipe.hashagg"),
    (Class::HashAggWide, "prim.pipe.hashagg_wide"),
    (Class::Sort, "prim.pipe.sort"),
    (Class::Build, "prim.pipe.build"),
    (Class::BuildNarrow, "prim.pipe.build_narrow"),
    (Class::Range, "prim.pipe.range"),
    (Class::Nest, "prim.pipe.nest"),
];

const MAX_W: usize = 64;

pub fn speedup(class: Class, w: u16) -> f64 {
    static TABLE: pgsync::OnceLock<Vec<[f64; MAX_W + 1]>> = pgsync::OnceLock::new(); // alloc: Cold
    let t = TABLE.get_or_init(|| {
        CLASSES
            .iter()
            .map(|&(_, prim)| {
                let mut row = [1.0; MAX_W + 1];
                let base = cost_profile::measured::at(prim, 0).filter(|r| r.width == 0 && r.ns > 0.0);
                for (w, slot) in row.iter_mut().enumerate().skip(2) {
                    if let (Some(b), Some(r)) = (base, cost_profile::measured::at(prim, w as u32)) {
                        if r.width >= 2 && r.ns > 0.0 && (b.ns / r.ns).is_finite() {
                            *slot = b.ns / r.ns;
                        }
                    }
                }
                row
            })
            .collect() // alloc: Cold
    });
    let i = CLASSES.iter().position(|&(c, _)| c == class).unwrap_or(0);
    t[i][(w as usize).min(MAX_W)]
}

/// The greatest speedup any pipeline can have at a width of at most `w`: a path's width cost is at least its PostgreSQL cost
/// over this (`add_path_precheck`'s bound).
pub fn speedup_max(w: u16) -> f64 {
    let mut m: f64 = 1.0;
    for &(c, _) in CLASSES.iter() {
        for x in 2..=w.min(MAX_W as u16) {
            m = m.max(speedup(c, x));
        }
    }
    m
}

/// Whether the level compares width costs.
#[inline]
pub fn active(run: &PlannerRun<'_>) -> bool {
    run.root.gravity_width_cost.width > 1
}

/// One finished pipeline, for the trace (`trace`): its driving relation's OID (0: none, a leader emit), its end, the
/// descriptor's verdict (`None`: eligible; else the reason it runs on the leader), its run-time width, its cost and speedup.
#[derive(Clone, Copy, Debug)]
pub struct Pipe {
    pub oid: u32,
    pub end: End,
    pub serial: Option<Reason>,
    pub w: u16,
    pub cost: f64,
    pub s: f64,
}

#[derive(Clone, Copy, Debug, Default)]
struct GState {
    closed: f64,
    open: f64,
    /// The part of `open` that is widened nests' work (their inners and join steps): priced at the `Nest` class.
    nest: f64,
    flags: u8,
    why: u8,
    w: u16,
    driver: u32,
}

struct Cx<'r, 'mcx> {
    run: &'r PlannerRun<'mcx>,
    root: &'r PlannerInfo<'mcx>,
    width: u16,
    free_aggs: bool,
    morsel: bool,
}

impl<'r, 'mcx> Cx<'r, 'mcx> {
    fn of(run: &'r PlannerRun<'mcx>, root: &'r PlannerInfo<'mcx>) -> Self {
        let g = &root.gravity_width_cost;
        Cx { run, root, width: g.width, free_aggs: g.free_aggs, morsel: g.morsel }
    }

    fn total(&self, id: Option<PathId>) -> f64 {
        id.map_or(0.0, |i| self.root.path(i).base().total_cost)
    }

    /// The relation OID of RT index `relid` (0 when the entry is not a parse tree's relation).
    fn oid(&self, relid: u32) -> u32 {
        match self.root.simple_rte_array.get(relid as usize) {
            Some(types_pathnodes::RangeTblEntryId::Parse { query, index }) => self.run.queries.get(query.0 as usize).filter(|q| (*index as usize) < q.rtable.len()).and_then(|q| q.rtable.nth(*index as usize).as_range_tbl_entry()).map_or(0, |r| r.relid),
            _ => 0,
        }
    }
}

/// The verdict of a pipeline with `flags` ending at `end`: eligible (every worker) or the leader.
fn eligible(flags: u8, end: End, free_aggs: bool) -> bool {
    let free_end = matches!(end, End::Sort) || (matches!(end, End::HashAgg | End::HashAggWide | End::Fold) && free_aggs);
    let order_free = matches!(end, End::Build | End::BuildWide) || (matches!(end, End::HashAgg | End::HashAggWide | End::Fold) && free_aggs);
    flags & PAR != 0 && (flags & NEED_FREE_END == 0 || (free_end && flags & FIRST_ROW == 0)) && (flags & NEED_ORDER_FREE == 0 || order_free)
}

/// The class of an open pipeline's source and steps: an index or bitmap range's (`Range`), else a scan's (`Count`).
fn source_class(flags: u8) -> Class {
    if flags & RANGE != 0 {
        Class::Range
    } else {
        Class::Count
    }
}

/// The class of a breaker's own work at `end` (a fold's or the sink's: the scan's).
fn class(end: End) -> Class {
    match end {
        End::BuildWide => Class::Build,
        End::HashAgg => Class::HashAgg,
        End::HashAggWide => Class::HashAggWide,
        End::Sort => Class::Sort,
        End::Build => Class::BuildNarrow,
        End::Fold | End::Sink => Class::Count,
    }
}

/// Closes the pipeline of `st` (its open cost plus `extra`, a breaker's own work) at `end`. An eligible pipeline's parts
/// each over their own measured speedup: its source and steps over the source's class, its widened nests' work over
/// `Nest`'s, the breaker's work over the end's class; a serial pipeline costs PostgreSQL's cost.
fn close(cx: &Cx<'_, '_>, st: &GState, extra: f64, end: End, trace: &mut Option<&mut Vec<Pipe>>) -> f64 {
    let cost = st.open + extra;
    let par = eligible(st.flags, end, cx.free_aggs);
    let g = if par && st.w >= 2 {
        let nest = st.nest.clamp(0.0, st.open);
        (st.open - nest) / speedup(source_class(st.flags), st.w) + nest / speedup(Class::Nest, st.w) + extra / speedup(class(end), st.w)
    } else {
        cost
    };
    let s = if g > 0.0 { cost / g } else { 1.0 };
    if let Some(t) = trace.as_deref_mut() {
        let serial = match (par, st.why) {
            (true, _) => None,
            (false, WHY_INDEX) => Some(Reason::IndexSource),
            (false, WHY_NOSITE) => Some(Reason::NoDrivingSite),
            (false, WHY_SINK) => Some(Reason::Sink),
            (false, _) => Some(Reason::Host),
        };
        t.push(Pipe { oid: st.driver, end, serial, w: st.w, cost, s }); // alloc: Cold
    }
    g
}

/// A leader emit (a breaker's output, a source with no driving site) of cost `open`.
fn leader(closed: f64, open: f64) -> GState {
    GState { closed, open, nest: 0.0, flags: 0, why: WHY_NOSITE, w: 0, driver: 0 }
}

/// The state of a heap or column-store scan of `rel` with PostgreSQL cost `cost`.
fn range_scan(cx: &Cx<'_, '_>, rel: RelId, cost: f64) -> GState {
    let r = cx.root.rel(rel);
    let w = if r.amflags & types_pathnodes::AMFLAG_PGRCOLUMNAR != 0 {
        let granules = if r.pgrcolumnar_granules > 0 { r.pgrcolumnar_granules } else { (r.tuples / 8192.0).ceil() as u64 };
        match (granules / 16).clamp(1, cx.width as u64) {
            0 | 1 => 0,
            w => w as u16,
        }
    } else {
        gravity_program::heap_width(r.pages as u64, cx.width)
    };
    let par = source_serial(SourceShape::Range).is_none();
    GState { closed: 0.0, open: cost, nest: 0.0, flags: if par { PAR } else { 0 }, why: if par { WHY_NONE } else { WHY_HOST }, w, driver: cx.oid(r.relid) }
}

/// An index-driven scan: the morsel form (eligible, its pages' width) where decompose would take it, else the serial index
/// source.
fn index_scan(cx: &Cx<'_, '_>, rel: RelId, cost: f64, morsel: bool, order_bound: bool, pages: f64) -> GState {
    let r = cx.root.rel(rel);
    let shape = if morsel { SourceShape::Morsel } else { SourceShape::Index };
    match source_serial(shape) {
        None => {
            let w = gravity_program::heap_width(pages.max(1.0) as u64, cx.width);
            let flags = PAR | RANGE | if order_bound { NEED_ORDER_FREE } else { 0 };
            GState { closed: 0.0, open: cost, nest: 0.0, flags, why: WHY_NONE, w, driver: cx.oid(r.relid) }
        }
        Some(_) => GState { closed: 0.0, open: cost, nest: 0.0, flags: 0, why: WHY_INDEX, w: 0, driver: cx.oid(r.relid) },
    }
}

/// A plain heap relation (not a column store, a foreign table or anything but a base relation).
fn heap(cx: &Cx<'_, '_>, rel: RelId) -> bool {
    let r = cx.root.rel(rel);
    r.rtekind == types_pathnodes::RTE_RELATION && r.amflags & types_pathnodes::AMFLAG_PGRCOLUMNAR == 0 && r.serverid == 0
}

fn bare(n: types_nodes::Node<'_>) -> bool {
    let n = n.as_relabel_type().map_or(n, |r| r.arg);
    n.as_var().is_some() || n.as_param().is_some() || n.as_const().is_some()
}

fn nest_shape(cx: &Cx<'_, '_>, inner: Option<PathId>) -> NestShape {
    // D4532: a nest over an eager spool widens in the lowerer (`spool`), which these paths do not cost yet: the planner's
    // verdict, and so every plan and EXPLAIN, stays as before
    let mut shape = NestShape { probe: None, inner_plain: true, inner_volatile: false, end_order_visible: false, lazy_material: false, regroup: false, keeps_owner: false, spool: false };
    let Some(mut inner) = inner else { return shape };
    // a Memoize over the probe widens with it (each worker ordinal keeps its own table)
    if let PathNode::MemoizePath(mp) = cx.root.path(inner) {
        let Some(sub) = mp.subpath else { return shape };
        inner = sub;
    }
    let PathNode::IndexPath(ip) = cx.root.path(inner) else { return shape };
    let Some(info) = ip.indexinfo else { return shape };
    let rel = ip.path.parent;
    let keys = info.indexkeys.iter().take(info.nkeycolumns.max(0) as usize).all(|&a| a > 0);
    let mut bare_keys = true;
    let mut volatile = false;
    for c in ip.indexclauses.iter() {
        let Some(ri) = c.rinfo else { bare_keys = false; continue };
        let clause = cx.root.expr_node(cx.root.rinfo(ri).clause);
        match clause.as_op_expr() {
            Some(op) => bare_keys &= op.args.len() == 2 && op.args.iter().all(bare),
            None => bare_keys = false,
        }
        volatile |= clauses::contain_volatile_functions(*clause).unwrap_or(true);
    }
    for &ri in cx.root.rel(rel).baserestrictinfo.iter() {
        volatile |= clauses::contain_volatile_functions(*cx.root.expr_node(cx.root.rinfo(ri).clause)).unwrap_or(true);
    }
    shape.probe = Some(NestProbe { btree: info.relam == BTREE_AM_OID && keys, heap: heap(cx, rel), plain_cols: true, bare_keys });
    shape.inner_volatile = volatile;
    shape
}

/// Whether `n` raises on no input (the lowerer's raise-free walk, effects.rs `raise_free`, in its common forms): columns,
/// constants, extern parameters, relabels, AND / OR / NOT, IS [NOT] NULL / TRUE, and builtin calls PostgreSQL marks
/// leakproof or types_core's quiet list passes, over such arguments. Anything else may raise.
fn quiet(n: Node<'_>) -> bool {
    let all = |l: &types_nodes::NodeList<'_>| l.iter().all(quiet);
    let func = |f: u32| f < 16384 && (types_core::quiet::call(f) || lsyscache::get_func_leakproof(f).unwrap_or(false));
    match n.node_tag() {
        NodeTag::T_Var | NodeTag::T_Const => true,
        NodeTag::T_Param => n.as_param().is_some_and(|p| p.paramkind != types_nodes::primnodes::ParamKind::PARAM_EXEC),
        NodeTag::T_RelabelType => n.as_relabel_type().is_some_and(|r| quiet(r.arg)),
        NodeTag::T_BoolExpr => n.as_bool_expr().is_some_and(|b| all(&b.args)),
        NodeTag::T_NullTest => n.as_null_test().is_some_and(|t| t.arg.is_none_or(quiet)),
        NodeTag::T_BooleanTest => n.as_boolean_test().is_some_and(|t| t.arg.is_none_or(quiet)),
        NodeTag::T_FuncExpr => n.as_func_expr().is_some_and(|f| func(f.funcid) && all(&f.args)),
        NodeTag::T_OpExpr => n.as_op_expr().is_some_and(|o| func(if o.opfuncid != 0 { o.opfuncid } else { lsyscache::get_opcode(o.opno).unwrap_or(0) }) && all(&o.args)),
        NodeTag::T_ScalarArrayOpExpr => n.as_scalar_array_op_expr().is_some_and(|o| func(if o.opfuncid != 0 { o.opfuncid } else { lsyscache::get_opcode(o.opno).unwrap_or(0) }) && all(&o.args)),
        _ => false,
    }
}

struct Reruns;

impl<'mcx> NodeWalker<'mcx> for Reruns {
    fn visit(&mut self, node: Node<'mcx>) -> PgResult<bool> {
        match node.node_tag() {
            NodeTag::T_SubPlan if node.as_sub_plan().is_some_and(|sp| !sp.useHashTable) => Ok(step_serial(&StepShape::SubPlan { scan: true }).is_some()),
            NodeTag::T_AlternativeSubPlan => Ok(false),
            _ => expression_tree_walker(node, self),
        }
    }
}

/// Whether a step of path `id` (its relation's or join's quals, its target) evaluates a per-row SubPlan.
fn step_reruns(cx: &Cx<'_, '_>, id: PathId) -> bool {
    step_any(cx, id, &|n| Reruns.visit(n).unwrap_or(true))
}

/// A call a pool thread cannot run, a step that keeps its pipeline on the leader (`StepShape::HostedCall`; lowerv4 hir.rs
/// `pool_unsafe_site` in its catalog forms): user or extension code, or a function PostgreSQL does not mark parallel safe.
/// A parallel-safe builtin the lowerer calls through the function manager with no pool host is not seen here (the
/// census's drift check reports one).
struct Hosts;

impl<'mcx> NodeWalker<'mcx> for Hosts {
    fn visit(&mut self, node: Node<'mcx>) -> PgResult<bool> {
        let op = |opfuncid: u32, opno: u32| if opfuncid != 0 { opfuncid } else { lsyscache::get_opcode(opno).unwrap_or(0) };
        let f = match node.node_tag() {
            NodeTag::T_FuncExpr => node.as_func_expr().map_or(0, |f| f.funcid),
            NodeTag::T_OpExpr => node.as_op_expr().map_or(0, |o| op(o.opfuncid, o.opno)),
            NodeTag::T_ScalarArrayOpExpr => node.as_scalar_array_op_expr().map_or(0, |o| op(o.opfuncid, o.opno)),
            _ => 0,
        };
        let pool_safe = |f: u32| f < 16384 && lsyscache::func_parallel(f).is_ok_and(|p| p == b's' as i8);
        if f != 0 && step_serial(&StepShape::HostedCall { pool_safe: pool_safe(f) }).is_some() {
            return Ok(true);
        }
        expression_tree_walker(node, self)
    }
}

/// Whether a step of path `id` calls a function a pool thread cannot run.
fn step_hosts(cx: &Cx<'_, '_>, id: PathId) -> bool {
    step_any(cx, id, &|n| Hosts.visit(n).unwrap_or(true))
}

/// Whether a qual or projection of path `id` may raise (`quiet` fails).
fn step_raises(cx: &Cx<'_, '_>, id: PathId) -> bool {
    step_any(cx, id, &|n| !quiet(n))
}

/// Whether `hit` holds for an expression of path `id`'s step: its relation's or join's quals, its target.
fn step_any(cx: &Cx<'_, '_>, id: PathId, hit: &dyn Fn(Node<'_>) -> bool) -> bool {
    let node = cx.root.path(id);
    let quals: &[types_pathnodes::RinfoId] = match node {
        PathNode::Path(p) => &cx.root.rel(p.parent).baserestrictinfo,
        PathNode::IndexPath(p) => &cx.root.rel(p.path.parent).baserestrictinfo,
        PathNode::BitmapHeapPath(p) => &cx.root.rel(p.path.parent).baserestrictinfo,
        PathNode::NestPath(p) => &p.jpath.joinrestrictinfo,
        PathNode::MergePath(p) => &p.jpath.joinrestrictinfo,
        PathNode::HashPath(p) => &p.jpath.joinrestrictinfo,
        PathNode::ProjectionPath(_) => &[],
        _ => return false,
    };
    let hit = |e| hit(*cx.root.expr_node(e));
    quals.iter().any(|&ri| hit(cx.root.rinfo(ri).clause)) || node.base().pathtarget_id.is_some_and(|t| cx.root.pathtarget(t).exprs.iter().any(|&e| hit(e)))
}

/// A path's state: memoized per level when no trace is taken.
fn state(cx: &Cx<'_, '_>, id: PathId, trace: &mut Option<&mut Vec<Pipe>>) -> GState {
    let base = cx.root.path(id).base();
    let stamp = (base.startup_cost, base.total_cost);
    if trace.is_none() {
        if let Some(m) = cx.root.gravity_width_cost.memo.borrow().get(id.0 as usize) {
            if m.stamp == stamp {
                return GState { closed: m.closed, open: m.open, nest: m.nest, flags: m.flags, why: m.why, w: m.w, driver: m.driver };
            }
        }
    }
    let mut st = compute(cx, id, trace);
    if st.flags & PAR != 0 && (step_reruns(cx, id) || step_hosts(cx, id)) {
        st.flags = 0;
        st.why = WHY_HOST;
    }
    if st.flags & PAR != 0 && step_raises(cx, id) {
        st.flags |= RAISED;
    }
    if trace.is_none() {
        let mut memo = cx.root.gravity_width_cost.memo.borrow_mut();
        let i = id.0 as usize;
        if memo.len() <= i {
            let extra = i + 1 - memo.len();
            if memo.try_reserve(extra).is_err() {
                return st;
            }
            memo.resize(i + 1, GwMemo::default()); // alloc: Cold
        }
        memo[i] = GwMemo { stamp, closed: st.closed, open: st.open, nest: st.nest, flags: st.flags, why: st.why, w: st.w, driver: st.driver };
    }
    st
}

/// `st` with its open pipeline's cost scaled by `k` (a Limit's share of its input).
fn scaled(st: GState, k: f64) -> GState {
    GState { closed: st.closed * k, open: st.open * k, nest: st.nest * k, ..st }
}

fn ordered(st: GState, limit: bool) -> GState {
    let mut st = st;
    if st.flags & NEED_ORDER_FREE != 0 {
        st.flags = 0;
        st.why = WHY_INDEX;
    }
    if limit && st.flags & NEED_FREE_END != 0 {
        st.flags = 0;
        st.why = WHY_HOST;
    }
    st
}

fn compute(cx: &Cx<'_, '_>, id: PathId, trace: &mut Option<&mut Vec<Pipe>>) -> GState {
    let node = cx.root.path(id);
    let base = node.base();
    let (startup, total) = (base.startup_cost, base.total_cost);
    let pass = |cx: &Cx<'_, '_>, sub: Option<PathId>, trace: &mut Option<&mut Vec<Pipe>>| -> GState {
        match sub {
            Some(s) => {
                let st = state(cx, s, trace);
                GState { open: st.open + (total - cx.total(Some(s))), ..st }
            }
            None => leader(0.0, total),
        }
    };
    // a breaker over `sub`: its startup less the input's total is its build / transition / sort work (the input pipeline's)
    let breaker = |cx: &Cx<'_, '_>, sub: Option<PathId>, end: End, trace: &mut Option<&mut Vec<Pipe>>| -> GState {
        let Some(s) = sub else { return leader(0.0, total) };
        let st = state(cx, s, trace);
        let sub_total = cx.total(Some(s));
        let work = (startup - sub_total).max(0.0);
        let closed = st.closed + close(cx, &st, work, end, trace);
        leader(closed, (total - sub_total - work).max(0.0))
    };
    match node {
        PathNode::Path(p) => {
            let t = p.pathtype;
            if t == tag16(NodeTag::T_SeqScan) || t == tag16(NodeTag::T_SampleScan) {
                if cx.root.rel(p.parent).rtekind == types_pathnodes::RTE_RELATION && cx.root.rel(p.parent).serverid == 0 {
                    return range_scan(cx, p.parent, total);
                }
            }
            leader(0.0, total)
        }
        PathNode::IndexPath(ip) => {
            let rel = ip.path.parent;
            let btree = ip.indexinfo.is_some_and(|i| i.relam == BTREE_AM_OID);
            let morsel = cx.morsel && ip.path.pathtype == tag16(NodeTag::T_IndexScan) && btree && ip.indexorderbys.is_empty() && ip.path.param_info.is_none() && heap(cx, rel) && ip.path.rows >= gravity_program::cost::INDEX_MORSEL_MIN_ROWS_WIDE;
            let pages = cx.root.rel(rel).pages as f64 * ip.indexselectivity.clamp(0.0, 1.0);
            index_scan(cx, rel, total, morsel, true, pages)
        }
        PathNode::BitmapHeapPath(bp) => {
            let rel = bp.path.parent;
            let r = cx.root.rel(rel);
            let morsel = cx.morsel && bp.path.param_info.is_none() && heap(cx, rel) && bp.path.rows >= gravity_program::cost::BITMAP_MORSEL_MIN_ROWS;
            let per_page = if r.pages > 0 { (r.tuples / r.pages as f64).max(1.0) } else { 1.0 };
            let pages = (bp.path.rows / per_page).max(1.0).min(r.pages.max(1) as f64);
            index_scan(cx, rel, total, morsel, false, pages)
        }
        PathNode::SubqueryScanPath(sp) => {
            let rel = cx.root.rel(sp.path.parent);
            let sub = rel.subroot_idx.and_then(|i| cx.run.rel_subroots.get(i)).zip(sp.subroot_subpath);
            match sub {
                Some((ss, sub_id)) => {
                    let scx = Cx::of(cx.run, &ss.root);
                    let sub_total = ss.root.path(sub_id).base().total_cost;
                    let inner = if scx.width > 1 {
                        let st = state(&scx, sub_id, trace);
                        st.closed + close(&scx, &st, 0.0, End::Sink, trace)
                    } else {
                        sub_total
                    };
                    leader(inner, (total - sub_total).max(0.0))
                }
                None => leader(0.0, total),
            }
        }
        PathNode::NestPath(np) => {
            let j = &np.jpath;
            let so = ordered(j.outerjoinpath.map(|o| state(cx, o, trace)).unwrap_or_default(), false);
            let si = j.innerjoinpath.map(|i| state(cx, i, trace)).unwrap_or_default();
            let excl = total - cx.total(j.outerjoinpath) - cx.total(j.innerjoinpath);
            let shape = nest_shape(cx, j.innerjoinpath);
            // an inner (under its Memoize) that evaluates a per-row SubPlan or calls a function a pool thread cannot run does so
            // in the outer's pipeline, a step that keeps the pipeline on the leader
            let leader_step = |i: PathId| step_reruns(cx, i) || step_hosts(cx, i);
            let reruns = |i: PathId| leader_step(i) || matches!(cx.root.path(i), PathNode::MemoizePath(m) if m.subpath.is_some_and(leader_step));
            let wide = step_serial(&StepShape::Nest(shape)).is_none() && !j.innerjoinpath.is_some_and(reruns);
            let (flags, why) = match (wide, so.flags & DRAIN != 0) {
                (_, true) => (0, WHY_SINK),
                (true, false) => (so.flags | NEED_FREE_END | RAISED, so.why),
                (false, false) => (0, WHY_HOST),
            };
            GState { closed: so.closed + si.closed, open: so.open + si.open + excl, nest: so.nest + si.open + excl, flags, why, w: so.w, driver: so.driver }
        }
        PathNode::MergePath(mp) => {
            let j = &mp.jpath;
            let mut closed = 0.0;
            let mut open = total - cx.total(j.outerjoinpath) - cx.total(j.innerjoinpath);
            for (side, sorted, inner) in [(j.outerjoinpath, !mp.outersortkeys.is_empty(), false), (j.innerjoinpath, !mp.innersortkeys.is_empty(), true)] {
                let Some(s) = side else { continue };
                let mut st = ordered(state(cx, s, trace), false);
                closed += st.closed;
                // a sorted run the merge reads hands its rows to the merge's pipeline, whose end is not known here: a widened
                // nest under it keeps its order visible unless that end is the sink, a sort or an order-free group (lowerv4
                // visible.rs `run_visible`), taken as visible
                if sorted && st.flags & NEED_FREE_END != 0 {
                    st.flags = 0;
                    st.why = WHY_HOST;
                }
                let run = inner && step_serial(&StepShape::MergeInnerRun).is_some();
                if run {
                    closed += close(cx, &GState { flags: 0, why: WHY_SINK, ..st }, 0.0, if sorted { End::Sort } else { End::Sink }, trace);
                } else if sorted {
                    closed += close(cx, &st, 0.0, End::Sort, trace);
                } else {
                    open += st.open;
                }
            }
            let why = if step_serial(&StepShape::Merge).is_some() { WHY_HOST } else { WHY_NONE };
            // the merge runs in its unsorted outer's pipeline (a sorted outer's pipeline closed at its sort)
            let driver = match j.outerjoinpath {
                Some(o) if mp.outersortkeys.is_empty() => state(cx, o, &mut None).driver,
                _ => 0,
            };
            GState { closed, open, nest: 0.0, flags: 0, why, w: 0, driver }
        }
        PathNode::HashPath(hp) => {
            let j = &hp.jpath;
            let so = j.outerjoinpath.map(|o| state(cx, o, trace)).unwrap_or_default();
            let si = j.innerjoinpath.map(|i| state(cx, i, trace)).unwrap_or_default();
            let (outer_startup, inner_total) = (j.outerjoinpath.map_or(0.0, |o| cx.root.path(o).base().startup_cost), cx.total(j.innerjoinpath));
            let excl = total - cx.total(j.outerjoinpath) - inner_total;
            // final_cost_hashjoin: the startup is the outer's startup, the inner's total and the build's work
            let build = (startup - outer_startup - inner_total).clamp(0.0, excl.max(0.0));
            let built = si.closed + close(cx, &si, build, build_end(j.innerjoinpath.map_or(0.0, |i| cx.root.path(i).base().rows)), trace);
            let ordered = j.jointype == types_pathnodes::JOIN_RIGHT_SEMI && j.joinrestrictinfo.iter().any(|ri| !hp.path_hashclauses.contains(ri) && !quiet(*cx.root.expr_node(cx.root.rinfo(*ri).clause)));
            use types_pathnodes::{JOIN_ANTI, JOIN_FULL, JOIN_LEFT, JOIN_RIGHT, JOIN_RIGHT_ANTI};
            let fill_inner = matches!(j.jointype, JOIN_RIGHT | JOIN_RIGHT_ANTI | JOIN_FULL);
            let fill_outer = matches!(j.jointype, JOIN_LEFT | JOIN_ANTI | JOIN_FULL);
            let outer_first = !fill_inner && !j.path.parallel_aware && (fill_outer || outer_startup < inner_total);
            let serial = step_serial(&StepShape::HashProbe { lazy: false, clears_marks: false, ordered }).is_some();
            let first_row = outer_first && !fill_outer && so.flags & RAISED != 0;
            let drain = fill_inner || j.jointype == types_pathnodes::JOIN_RIGHT_SEMI;
            let (flags, why) = match (serial, so.flags & DRAIN != 0) {
                (_, true) => (0, WHY_SINK),
                (true, false) => (0, WHY_HOST),
                (false, false) => (so.flags | RAISED | if first_row { FIRST_ROW } else { 0 } | if drain { DRAIN } else { 0 }, so.why),
            };
            GState { closed: so.closed + built, open: so.open + (excl - build).max(0.0), nest: so.nest, flags, why, w: so.w, driver: so.driver }
        }
        PathNode::AppendPath(ap) => {
            let mut closed = 0.0;
            for &c in ap.subpaths.iter() {
                let st = state(cx, c, trace);
                closed += st.closed + close(cx, &st, 0.0, End::Sink, trace);
            }
            let kids: f64 = ap.subpaths.iter().map(|&c| cx.total(Some(c))).sum();
            leader(closed, (total - kids).max(0.0))
        }
        PathNode::MergeAppendPath(ap) => {
            let mut closed = 0.0;
            for &c in ap.subpaths.iter() {
                let st = state(cx, c, trace);
                closed += st.closed + close(cx, &st, 0.0, End::Sink, trace);
            }
            let kids: f64 = ap.subpaths.iter().map(|&c| cx.total(Some(c))).sum();
            leader(closed, (total - kids).max(0.0))
        }
        PathNode::MaterialPath(mp) => {
            let st = pass(cx, mp.subpath, trace);
            let why = if step_serial(&StepShape::MaterialFill).is_some() { WHY_SINK } else { st.why };
            GState { flags: 0, why, ..st }
        }
        PathNode::MemoizePath(mp) => pass(cx, mp.subpath, trace),
        PathNode::UniquePath(up) => match up.umethod {
            types_pathnodes::UNIQUE_PATH_HASH => {
                let Some(s) = up.subpath else { return leader(0.0, total) };
                let st = state(cx, s, trace);
                let sub_total = cx.total(Some(s));
                let work = (startup - sub_total).max(0.0);
                let closed = st.closed + close(&Cx { free_aggs: true, ..Cx::of(cx.run, cx.root) }, &st, work, hash_end(up.path.rows), trace);
                leader(closed, (total - sub_total - work).max(0.0))
            }
            types_pathnodes::UNIQUE_PATH_SORT => breaker(cx, up.subpath, End::Sort, trace),
            _ => pass(cx, up.subpath, trace),
        },
        PathNode::GatherPath(gp) => ordered(pass(cx, gp.subpath, trace), false),
        PathNode::GatherMergePath(gp) => ordered(pass(cx, gp.subpath, trace), false),
        PathNode::ProjectionPath(pp) => pass(cx, pp.subpath, trace),
        PathNode::ProjectSetPath(pp) => pass(cx, pp.subpath, trace),
        PathNode::SortPath(sp) => breaker(cx, sp.subpath, End::Sort, trace),
        PathNode::IncrementalSortPath(sp) => {
            let st = ordered(pass(cx, sp.spath.subpath, trace), false);
            GState { flags: 0, why: if st.flags & PAR != 0 { WHY_HOST } else { st.why }, ..st }
        }
        PathNode::GroupPath(gp) => ordered(pass(cx, gp.subpath, trace), false),
        PathNode::UpperUniquePath(up) => ordered(pass(cx, up.subpath, trace), false),
        PathNode::AggPath(ap) => match ap.aggstrategy {
            types_pathnodes::AGG_PLAIN => breaker(cx, ap.subpath, End::Fold, trace),
            types_pathnodes::AGG_HASHED | types_pathnodes::AGG_MIXED => breaker(cx, ap.subpath, hash_end(ap.numGroups), trace),
            _ => ordered(pass(cx, ap.subpath, trace), false),
        },
        PathNode::GroupingSetsPath(gp) => match gp.aggstrategy {
            types_pathnodes::AGG_HASHED | types_pathnodes::AGG_MIXED => breaker(cx, gp.subpath, End::HashAgg, trace),
            _ => ordered(pass(cx, gp.subpath, trace), false),
        },
        PathNode::WindowAggPath(wp) => ordered(pass(cx, wp.subpath, trace), false),
        PathNode::SetOpPath(sp) => {
            let hashed = sp.strategy == types_pathnodes::SETOP_HASHED;
            let mut closed = 0.0;
            for side in [sp.leftpath, sp.rightpath].into_iter().flatten() {
                let st = state(cx, side, trace);
                closed += st.closed + if hashed { close(&Cx { free_aggs: true, ..Cx::of(cx.run, cx.root) }, &st, 0.0, End::HashAgg, trace) } else { st.open };
            }
            leader(closed, (total - cx.total(sp.leftpath) - cx.total(sp.rightpath)).max(0.0))
        }
        PathNode::RecursiveUnionPath(rp) => {
            let mut closed = 0.0;
            for side in [rp.leftpath, rp.rightpath].into_iter().flatten() {
                let st = state(cx, side, trace);
                closed += st.closed + st.open;
            }
            leader(closed, (total - cx.total(rp.leftpath) - cx.total(rp.rightpath)).max(0.0))
        }
        PathNode::LockRowsPath(lp) => pass(cx, lp.subpath, trace),
        PathNode::ModifyTablePath(mp) => pass(cx, mp.subpath, trace),
        PathNode::LimitPath(lp) => match lp.subpath {
            Some(s) => {
                let sub_total = cx.total(Some(s));
                let st = ordered(state(cx, s, trace), true);
                if sub_total > 0.0 {
                    scaled(st, total / sub_total)
                } else {
                    st
                }
            }
            None => leader(0.0, total),
        },
        _ => leader(0.0, total),
    }
}

/// The width costs of path `id` at this level: `(startup, total)`, PostgreSQL's when the level does not compare width
/// costs. The open pipeline counts as ending in the sink (an end not yet known: a widened nest and an index morsel count
/// serial until a breaker closes them).
pub fn costs(run: &PlannerRun<'_>, id: PathId) -> (f64, f64) {
    let b = run.root.path(id).base();
    if !active(run) {
        return (b.startup_cost, b.total_cost);
    }
    let cx = Cx::of(run, &run.root);
    let st = state(&cx, id, &mut None);
    let end = match run.root.gravity_width_cost.end_hint {
        1 => End::HashAgg,
        2 => End::Sort,
        _ => End::Sink,
    };
    let g = st.closed + close(&cx, &st, 0.0, end, &mut None);
    let startup = if b.total_cost > 0.0 { b.startup_cost * (g / b.total_cost) } else { b.startup_cost };
    (startup, g)
}

/// A lower bound of the width costs of a path whose PostgreSQL costs are `(startup, total)` (no pipeline speeds up past
/// `speedup_max`).
pub fn bound(run: &PlannerRun<'_>, startup: f64, total: f64) -> (f64, f64) {
    if !active(run) {
        return (startup, total);
    }
    let m = speedup_max(run.root.gravity_width_cost.width);
    (startup / m, total / m)
}

pub fn trace(run: &PlannerRun<'_>, id: PathId) -> (Vec<Pipe>, f64) {
    let cx = Cx::of(run, &run.root);
    let mut pipes = Vec::new(); // alloc: Cold
    let st = state(&cx, id, &mut Some(&mut pipes));
    let g = st.closed + close(&cx, &st, 0.0, End::Sink, &mut Some(&mut pipes));
    (pipes, g)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_end_resolves_a_conditional_pipeline() {
        assert!(eligible(PAR, End::Sink, false));
        assert!(!eligible(0, End::Build, true));
        assert!(!eligible(PAR | NEED_FREE_END, End::Sink, true), "a widened nest into the sink shows its order");
        assert!(!eligible(PAR | NEED_FREE_END, End::Build, true), "a hash build is no fold");
        assert!(eligible(PAR | NEED_FREE_END, End::Sort, false));
        assert!(!eligible(PAR | NEED_FREE_END | FIRST_ROW, End::Sort, true), "which row comes first decides an error");
        assert!(eligible(PAR | FIRST_ROW, End::Sink, false), "a pipeline with no widened nest is not ordered by it");
        assert!(eligible(PAR | NEED_FREE_END, End::HashAgg, true));
        assert!(!eligible(PAR | NEED_FREE_END, End::HashAgg, false), "an order-sensitive aggregate");
        assert!(eligible(PAR | NEED_ORDER_FREE, End::Build, false), "a hash build reads its rows in any order");
        assert!(!eligible(PAR | NEED_ORDER_FREE, End::Sort, true), "a sort's reader is not order-free today");
        assert!(eligible(PAR | NEED_ORDER_FREE, End::Fold, true));
    }

    #[test]
    fn a_source_and_an_end_name_their_own_classes() {
        assert_eq!(source_class(PAR | RANGE), Class::Range);
        assert_eq!(source_class(PAR | NEED_FREE_END), Class::Count);
        assert_eq!(class(End::BuildWide), Class::Build);
        assert_eq!(class(End::Build), Class::BuildNarrow);
        assert_eq!((build_end(WIDE_GROUPS), build_end(WIDE_GROUPS + 1.0)), (End::Build, End::BuildWide));
        assert_eq!((hash_end(WIDE_GROUPS), hash_end(WIDE_GROUPS + 1.0)), (End::HashAgg, End::HashAggWide));
        assert_eq!(class(End::Fold), Class::Count);
        assert_eq!(class(End::Sink), Class::Count);
    }

    #[test]
    fn an_unmeasured_class_or_width_speeds_nothing_up() {
        for &(c, _) in CLASSES.iter() {
            assert_eq!(speedup(c, 0), 1.0);
            assert_eq!(speedup(c, 1), 1.0);
            assert!(speedup(c, 8) >= 0.0 && speedup(c, 8).is_finite());
        }
        assert!(speedup_max(8) >= 1.0);
        assert_eq!(speedup_max(1), 1.0);
    }

    /// Every class has its measured rows on this machine class: an eligible pipeline at W8 runs faster than one worker.
    #[test]
    fn every_class_is_measured_and_speeds_up_at_w8() {
        for &(c, prim) in CLASSES.iter() {
            assert!(cost_profile::measured::at(prim, 8).is_some_and(|r| r.width == 8), "{prim} has no W8 row");
            assert!(speedup(c, 8) > 1.0, "{prim} at W8: {}", speedup(c, 8));
        }
    }
}
