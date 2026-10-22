//! The operator interface home (PLAN-v5 §2.5; structure/interfaces.md §2.5 "Request, answer, continuation"): what a
//! consumer asks of the plan node below it (`Request`) and what that node delivers (`Answer`), by plan node id. Created by
//! the window lane for the exact request (contract-interface worklist item 10; main's VF21 ruling, 2026-10-03).
//! - The requests are filled once, before any make, top-down over the graph (PG's ExecSetTupleBound, execProcnode.c:848):
//!   each plan node a pipeline runs hands its request on to the node it reads (the node before it in the pipeline, or
//!   across a breaker source the top of the pipeline that fills it), through its `pass_down`. A node no consumer asks
//!   anything of has `Request::ALL`.
//! - `Delivery::Exact` comes from decompose's effects check alone (`PipelineGraph::exact`: a Limit that stops over a
//!   WindowAgg whose fill acts, or may raise with its read-ahead licence denied, as in errorhold's plan-order run).
//! - `pass_down`'s default hands `Request::ALL` on, except that `Delivery::Exact` stays exact. Its one other arm today is
//!   the Limit's: a constant count (plus a constant offset) bounds the node under it, `Bound::Rows`. A count known only at
//!   bind (LIMIT $1) passes no bound yet (`Bound::Word` names a word the binder places at the Limit's own make).
//! - `bound`, `order` and `cols` are hints: a producer that ignores them is correct, only slower. `delivery` is a promise:
//!   under `Delivery::Exact` a producer evaluates nothing ahead of its consumer's pull (its PostgreSQL form); one that cannot
//!   refuses by name, never runs ahead silently.
//! - An answer is recorded when a node's last part is made (`Px::set_answer`); a consumer reads it, never a producer's
//!   mechanisms.
use gravity_decompose::graph::{Bound as PlanBound, Order};
use gravity_decompose::{Op, Source};

use crate::admit::refusal::{Site, Variant};
use crate::face::kinds::KindId;
use crate::face::strategy::Strategy;
use crate::frame::px::Px;
use crate::graph::Gx;
use crate::ops::{kind, kind_as, need, strategy, strategy_rows};

mod answer;
mod ends;
mod group;
mod group_end;
mod sort;
mod kinds;
mod limit;
mod nest;
mod union_arm;
mod cte_plan;
pub(crate) use answer::{answer_count_only, answer_count_star_only, answer_counted_qual, answer_declare_site, summary_filter_tally, summary_tally, SummaryFilterTally, SummaryTally};
pub(crate) use sort::{held_fill, sort_cols, sort_cursor, sort_emit_over, sort_end_over, sort_resident, SortArm};
pub use ends::{breaker_of, breaker_of_mut, end_holds, with_end_of};
pub use group::{key_ordered, order_read};
pub(crate) use group::presorted_exact;
pub(crate) use group_end::{fold_class_in, group_end, group_end_of, join_domain, twopass_admitted_at, twopass_count_split, twopass_having_split, twopass_pass2};
pub use kinds::kind_impl;
pub use limit::{demand, holds_limit, limit_bounds, limit_may_skip, limit_settled, limit_stopping, limit_with_ties, Demand};
pub(crate) use limit::limit_check_bound;
pub use nest::{nest_param_val, nest_param_vals};
pub(crate) use union_arm::{arm_index, later_arm_closes, later_arm_probe};
pub(crate) use cte_plan::in_cte_plan_apart;

/// What a consumer asks of the node below it. `bound`, `order` and `cols` are hints; `delivery` is a promise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub bound: Bound,
    pub order: Option<Order>,
    pub delivery: Delivery,
    pub cols: ColSet,
    /// The consumer will mark and restore (PG's EXEC_FLAG_MARK: a merge join's inner unless skip_mark_restore).
    pub mark: bool,
}

impl Request {
    /// No bound, any order, any number of passes, every column.
    pub const ALL: Request = Request { bound: Bound::All, order: None, delivery: Delivery::Stream, cols: ColSet(Vec::new()), mark: false };
}

/// `Request::ALL`, for a node no consumer asks anything of.
static ALL: Request = Request::ALL;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bound {
    All,
    /// LIMIT + OFFSET, top-k, 1 for EXISTS or a semi join, a cursor's fetch size.
    Rows(u64),
    /// A bound known only at bind (LIMIT $1): the parameter word that holds it, filled before the node opens.
    Word(u16),
}

/// How rows must come. Ordered by strictness: a producer may deliver a stricter form than asked, never a looser one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Delivery {
    /// Rows may come in several passes and from several pipelines (a spill pair, a drain's arms).
    Stream,
    /// One pass from one pipeline (a merge join's sides, an ordered group's input).
    OnePass,
    /// Nothing evaluated ahead of the consumer's pull: the producer emits its PostgreSQL form. Only `effects` sets it.
    Exact,
}

/// Output columns by position (bit `i` of word `i / 64`); a producer may carry more than asked, never fewer. Empty: every
/// column (nothing narrowed).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ColSet(pub Vec<u64>);

/// What a producer delivers, recorded once when its node's last part is made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// The order rows leave in; `None`: unknown.
    pub order: Option<Order>,
    /// At least as strict as requested.
    pub delivery: Delivery,
    /// The columns carried; a superset of the request's.
    pub cols: ColSet,
    /// It stops at the request's bound itself (counts, or reads the stop word).
    pub bounded: bool,
    /// It can mark and restore (answers `Request.mark`).
    pub markable: bool,
}

/// The statement's requests and answers, by plan node id (one `Px` field).
#[derive(Default)]
pub struct Iface {
    requests: ByNode<Request>,
    answers: ByNode<Answer>,
}

/// D3585: values by plan node id, in arrival order, with an index by id (`at`: the place + 1 of id `i`, 0: none). A
/// linear search per node made the requests' walk and the answers quadratic in the plan's nodes (an Append over N
/// partitions has N scans). An id past `BY_NODE_INDEXED` or below 0 is searched.
struct ByNode<T> {
    items: Vec<(i32, T)>, // alloc: Program
    at: Vec<u32>, // alloc: Program
}

const BY_NODE_INDEXED: usize = 1 << 20;

impl<T> Default for ByNode<T> {
    fn default() -> Self {
        ByNode { items: Vec::new(), at: Vec::new() }
    }
}

impl<T> ByNode<T> {
    fn place(&self, node: i32) -> Option<usize> {
        match usize::try_from(node) {
            Ok(i) if i < BY_NODE_INDEXED => self.at.get(i).copied().filter(|&p| p > 0).map(|p| p as usize - 1),
            _ => self.items.iter().position(|x| x.0 == node),
        }
    }

    fn get(&self, node: i32) -> Option<&T> {
        self.place(node).map(|i| &self.items[i].1)
    }

    fn get_mut(&mut self, node: i32) -> Option<&mut T> {
        self.place(node).map(|i| &mut self.items[i].1)
    }

    /// `v` for `node`, which holds none yet.
    fn push(&mut self, node: i32, v: T) {
        if let Some(i) = usize::try_from(node).ok().filter(|&i| i < BY_NODE_INDEXED) {
            if self.at.len() <= i {
                self.at.resize(i + 1, 0); // alloc: Program
            }
            self.at[i] = self.items.len() as u32 + 1;
        }
        self.items.push((node, v)); // alloc: Program
    }
}

impl Iface {
    /// Every node's request, walked top-down: a pipeline is read after every pipeline reading it (a reader follows its
    /// producer in the graph), its nodes from the last to the source's.
    pub fn of(gx: &Gx<'_, '_>) -> Iface {
        let mut out = Iface::default();
        let ps = &gx.graph.pipelines;
        for k in (0..ps.len()).rev() {
            let p = &ps[k];
            for (i, n) in p.node_ops.iter().enumerate().rev() {
                let child = match i.checked_sub(1) {
                    Some(c) => Some(p.node_ops[c].node_id),
                    None => match p.source {
                        Source::Breaker { slot } => gx.life_of(k, slot).and_then(|l| ps.get(l.0 as usize)).and_then(|q| q.node_ops.last()).map(|t| t.node_id),
                        _ => None,
                    },
                };
                let Some(c) = child.filter(|&c| c != n.node_id) else { continue };
                let end = p.node_ops.get(i + 1).map_or(p.ops.len(), |x| x.first_op as usize);
                let ops = p.ops.get(n.first_op as usize..end).unwrap_or(&[]);
                let ask = pass_down(ops, &out.own(gx, n.node_id));
                out.ask(c, ask);
            }
        }
        for &id in gx.graph.exact.iter() {
            let r = out.own(gx, id);
            out.set(id, r);
        }
        out
    }

    /// `node`'s request, exact where decompose asked it.
    fn own(&self, gx: &Gx<'_, '_>, node: i32) -> Request {
        let r = self.request(node).clone();
        match gx.graph.exact.contains(&node) {
            true => Request { delivery: Delivery::Exact, ..r },
            false => r,
        }
    }

    fn set(&mut self, node: i32, r: Request) {
        match self.requests.get_mut(node) {
            Some(x) => *x = r,
            None => self.requests.push(node, r),
        }
    }

    fn request(&self, node: i32) -> &Request {
        self.requests.get(node).unwrap_or(&ALL)
    }

    /// `r` asked of `node`; a node two consumers read (a shared producer) keeps the hints both agree on and the stricter
    /// delivery.
    fn ask(&mut self, node: i32, r: Request) {
        match self.requests.get_mut(node) {
            None => self.requests.push(node, r),
            Some(have) => {
                let cols = if have.cols == r.cols { r.cols } else { ColSet::default() };
                *have = Request {
                    bound: if have.bound == r.bound { r.bound } else { Bound::All },
                    order: if have.order == r.order { r.order } else { None },
                    delivery: have.delivery.max(r.delivery),
                    cols,
                    mark: have.mark || r.mark,
                };
            }
        }
    }
}

/// The implementation registered for hook variant `v`, as its face's type `T` (`NotBuilt` when unbuilt): a kind's row or a
/// road's, the registry lookup every hook family's `need` makes (face/source.rs `hook_variants!`), here so no family
/// reads the registry raw (lint RB).
pub fn need_variant<T: 'static>(cx: &impl crate::admit::refusal::Site, v: crate::admit::refusal::Variant) -> &'static T {
    match v {
        crate::admit::refusal::Variant::Kind(k) => crate::frame::iface::need_kind::<T>(cx, k),
        crate::admit::refusal::Variant::Strategy(r) => crate::frame::iface::need_strategy::<T>(cx, r),
    }
}

/// What a node running `ops` (its own, in its pipeline) with request `own` asks of the node it reads. The default hands
/// `Request::ALL` on, an exact request staying exact; a Limit bounds its child by a constant count plus offset.
fn pass_down(ops: &[Op<'_>], own: &Request) -> Request {
    let delivery = if own.delivery == Delivery::Exact { Delivery::Exact } else { Delivery::Stream };
    let bound = ops.iter().find_map(|o| demand(o).filter(|d| !d.ties).map(|d| limit_bound(d.count, d.offset)));
    Request { bound: bound.unwrap_or(Bound::All), delivery, ..Request::ALL }
}

/// The arm a key's representation registers for kind `k` (rep/: its datum, hash or order face, `T`): the kind lookup
/// lives here, so rep reads no registry raw (lint RB; rep/mod.rs `arm`). `NotBuilt` when no file provides the kind, a
/// Bug when one registers it as another face.
pub fn rep_arm<T: 'static>(site: &impl crate::admit::refusal::Site, k: crate::face::kinds::KindId) -> &'static T {
    crate::frame::iface::need_kind::<T>(site, k)
}

/// D1903 (rerun/inner_limit.rs `may_zero`): whether a Limit in `ops` has a count known only past the plan (a parameter, a
/// computed or bind-time expression) or a negative constant: read per re-open (PG `recompute_limits`), it may be 0 there.
pub fn limit_count_late(ops: &[Op<'_>]) -> bool {
    ops.iter().any(|o| matches!(o, Op::Limit { count, .. } if matches!(count, PlanBound::Expr(_)) || matches!(count, PlanBound::Const(c) if *c < 0)))
}

/// Whether the run pipeline `k`'s sort ends in holds rows, which an embedded inner's emit may read again from its start at
/// every re-open (rerun/inner_once.rs `kept_run`): a row sort's run (runs.sort orders the rows by value), not form G's
/// (ops/sort/shapes.rs `group_sort`: the sort orders the records of the keyed group it drains in place, no row copied, and
/// the drain's resumed gather is the group's). The one question a consumer asks of a sort's run, so no consumer copies form
/// G's conditions (lint RB).
pub fn run_holds_rows(gx: &Gx<'_, '_>, k: usize) -> bool {
    !crate::ops::sort::shapes::group_sort(gx, k)
}

/// D3024 (b): whether pipeline `k` ends in a row sort or a row top that takes section ends (a full sort's or a top's run over
/// no presorted prefix, not a union's arm): the sort's answer (ops/sort/lower.rs `wide_row_end`), which a CTE's sealed
/// store asks of a reader it lets read by record claims (ops/ctescan/wholefill.rs `sort_reader`), so it names no other
/// family (lint RA).
pub(crate) fn wide_row_end(gx: &Gx<'_, '_>, k: usize) -> bool {
    crate::ops::sort::lower::wide_row_end(gx, k)
}

/// Whether pipeline `k` ends in the client's sink rather than a breaker: the one question a strategy asks of its own
/// pipeline's end (end/strategies/sink_ordered.rs `form`), so no strategy reads the end raw (lint RB).
pub fn ends_in_sink(gx: &Gx<'_, '_>, k: usize) -> bool {
    matches!(gx.pipelines()[k].desc.ends_in, gravity_program::End::Sink)
}

/// The breaker kind pipeline `k` ends in, or `None` when it ends in the client's sink: the end question the result
/// gate asks of a feeder (ops/result/gate.rs `Reach::of`, effects/visible/result.rs `quiet`), so it reads no end raw
/// (lint RB).
pub fn end_breaker(gx: &Gx<'_, '_>, k: usize) -> Option<gravity_abi::breaker::BreakerKind> {
    match gx.pipelines()[k].desc.ends_in {
        gravity_program::End::Breaker(b) => Some(b.kind),
        gravity_program::End::Sink => None,
    }
}

/// The breaker pipeline `k` ends in (its kind, slot, spill and the rest), or `None` when it ends in the client's sink or
/// `k` names no pipeline: the end a keyed table reads its observed slot from (table/size.rs `declare_facts`), so it reads no end
/// raw (lint RB).
pub fn end_breaker_of(gx: &Gx<'_, '_>, k: usize) -> Option<gravity_abi::breaker::Breaker> {
    match gx.pipelines().get(k)?.desc.ends_in {
        gravity_program::End::Breaker(b) => Some(b),
        gravity_program::End::Sink => None,
    }
}

/// Descriptor `d` with its end breaker's spill off, so the breaker grows its grant instead of spilling (a keyed table
/// whose records hold key copies, table/emit_key.rs `resident`), its answer the debt `why` names (C10); a sink end is
/// unchanged. The one way a lowering edits a pipeline's end (lint RB).
pub fn end_resident(d: gravity_program::Pipeline, why: gravity_program::spill::NoSpill) -> gravity_program::Pipeline {
    match d.ends_in {
        gravity_program::End::Breaker(br) => {
            let d = gravity_program::Pipeline { ends_in: gravity_program::End::Breaker(gravity_abi::breaker::Breaker { spill: gravity_abi::breaker::Spill::None, ..br }), ..d };
            end_holds(d, gravity_program::spill::Shape::Grows(why))
        }
        gravity_program::End::Sink => d,
    }
}

/// Whether pipeline `k`'s sort orders the records of the keyed group it drains in place (form G: ops/sort/shapes.rs
/// `group_sort`) rather than holding rows: the question a group's emit asks before it sorts its records
/// (table/emit_group.rs), so no lowering reads the sort's form raw (lint RB).
pub fn sorts_group_in_place(gx: &Gx<'_, '_>, k: usize) -> bool {
    crate::ops::sort::shapes::group_sort(gx, k)
}

/// D3800: whether drain pipeline `d`'s write rechecks in the IR through the target's nest past its RIGHT drain, the
/// drain's row held: modifytable's answer (ops/modifytable/rowop_epq.rs `drain_held`), which the spooled drain asks of
/// its write (ops/hashjoin/drain/spool_admit.rs `held_write`), so it names no other family (lint RA).
pub(crate) fn drain_held(gx: &Gx<'_, '_>, d: usize) -> bool {
    crate::ops::modifytable::rowop_epq::drain_held(gx, d)
}

/// Whether PostgreSQL surely runs pipeline `q`'s rows from op `from` on once the statement runs: mergejoin's answer
/// (ops/mergejoin/lazy_outer.rs `reached`), which the held Incremental Sort asks of its run's one reader (ops/sort/inc_held.rs),
/// so it names no other family (lint RA).
pub(crate) fn rows_reached(gx: &Gx<'_, '_>, q: usize, from: usize) -> bool {
    crate::ops::mergejoin::lazy_outer::reached(gx, q, from)
}

/// The pipelines the runner may skip whole when the build of hash probe `j` of pipeline `q` (build pipeline `b`) is empty:
/// hashjoin's answer for that probe (ops/hashjoin/build_class.rs `outer`'s `skip`), which the result gate reads for its
/// masks (ops/result/gate.rs), so it names no other family (lint RA).
pub(crate) fn probe_skips(gx: &Gx<'_, '_>, b: usize, q: usize, j: usize) -> u64 {
    crate::ops::hashjoin::build_class::outer(gx, b, q, j).skip
}

/// Pipeline `k` and every pipeline feeding it (its consumed slots and a breaker source, transitively), and whether any
/// of them calls a user or VOLATILE function or locks rows: hashjoin's answer (ops/hashjoin/build_class.rs `feed` and
/// `side`'s `UserFn`), which a merge join's nest inner asks of its fillers (ops/mergejoin/nest_inner.rs `raise_only`),
/// so it names no other family (lint RA).
pub(crate) fn fed_user_code<'g>(gx: &Gx<'g, '_>, k: usize) -> (mcx::PgVec<'g, usize>, bool) {
    use crate::ops::hashjoin::build_class::{feed, side, Side};
    use mcx::GrowIn;
    let ps = gx.pipelines();
    let src = if let gravity_decompose::Source::Breaker { slot } = ps[k].source { Some(slot) } else { None };
    let mut all = mcx::pg_vec![in gx.mcx; k];
    all.extend_in(feed(gx, k, ps[k].desc.consumes.iter().copied().chain(src)));
    (all, side(gx, k) == Side::UserFn)
}

/// join.payload_fp: the code lanes of pipeline `k`'s scan whose Fp128 its hash build stores in its records for a group past
/// the probe: hashjoin's answer (ops/hashjoin/strategies/payload_fp.rs `lanes`), which the pgrc source declares beside the
/// group's own tables (ops/scan/pgrc/lower.rs), so it names no other family (lint RA).
pub(crate) fn build_fp_lanes(gx: &Gx<'_, '_>, k: usize) -> crate::ops::hashjoin::strategies::payload_fp::FpCols {
    crate::ops::hashjoin::strategies::payload_fp::lanes(gx, k)
}

/// D3324 (b) (road key.dependent, ops/agg/strategies/dependent.rs): column `col` of the row entering pipeline `q`'s end as
/// an INNER probe's one build key (`unique_build_key`), another as that build row's kept code (`build_code_col`), and the
/// build's run-time uniqueness (`build_unique_now`): hashjoin's answers (ops/hashjoin/strategies/dep_key.rs), which the
/// group asks, so it names no other family (lint RA).
pub(crate) use crate::ops::hashjoin::strategies::dep_key::UniqueKey;
pub(crate) fn unique_build_key(px: &mut Px<'_, '_>, q: usize, col: i16) -> Option<UniqueKey> {
    crate::ops::hashjoin::strategies::dep_key::unique_key(px, q, col)
}
/// D3324 R3 (road key.carry): column `col` as a payload column of an INNER probe's build (hashjoin's `build_row`).
pub(crate) fn build_row(px: &mut Px<'_, '_>, q: usize, col: i16) -> Option<UniqueKey> {
    crate::ops::hashjoin::strategies::dep_key::build_row(px, q, col)
}
pub(crate) fn build_code_col(px: &mut Px<'_, '_>, q: usize, u: &UniqueKey, col: i16, bp: bool) -> Option<(i16, gravity_program::Rep)> {
    crate::ops::hashjoin::strategies::dep_key::code_col(px, q, u, col, bp)
}
pub(crate) fn build_unique_now(u: &UniqueKey, b: &mut gravity_ir::builder::FunctionBuilder<'_>, env: &crate::frame::env::Env) -> Option<gravity_ir::Value> {
    crate::ops::hashjoin::strategies::dep_key::unique_now(u, b, env)
}

/// Whether group `g`'s records stream into a top over the groups that sorts by finals alone: the sort's answer
/// (ops/sort/topn/order_slot.rs `top_reads_finals`), which road key.dependent asks, so it names no other family (lint RA).
pub(crate) fn top_reads_finals(gx: &Gx<'_, '_>, g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>) -> bool {
    crate::ops::sort::topn::order_slot::top_reads_finals(gx, g)
}
/// As `top_reads_finals`, where the sort may also read a group key column `keep` passes (order_slot.rs `top_reads_finals_and`).
pub(crate) fn top_reads_finals_and(gx: &Gx<'_, '_>, g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>, keep: impl Fn(i16) -> bool) -> bool {
    crate::ops::sort::topn::order_slot::top_reads_finals_and(gx, g, keep)
}

/// D2605 PULLED: whether data-modifying CTE pipeline `k` is pulled by its readers one row per miss and drained at the
/// statement's end (ctescan's answer, ops/ctescan/lower.rs `pulled_write`), which a write's statement-trigger order reads
/// (ops/modifytable/rowop.rs `pulled_ends`), so it names no other family (lint RA).
pub(crate) fn cte_pulled_write(gx: &Gx<'_, '_>, k: usize) -> bool {
    crate::ops::ctescan::lower::pulled_write(gx, k)
}

/// The expression face (kind `expr.core`) a family compiles its expressions through: the kind lookup lives here, so no
/// family reads the registry raw for it (lint RB; rerun/params_bound.rs).
pub fn expr_face(site: &impl crate::admit::refusal::Site) -> &'static crate::face::step::ExprFace {
    crate::frame::iface::need_kind(site, crate::face::kinds::KindId::ExprCore)
}

/// The store reader's maker (kind `srf.function`'s store row) a FunctionScan over a materialised set builds its reader
/// through (ops/srf/function_store.rs `store_step`), so the family reads no registry row raw for it (lint RB).
pub(crate) fn store_maker(site: &impl crate::admit::refusal::Site) -> &'static crate::ops::srf::face::StoreMaker {
    crate::frame::iface::need_kind(site, crate::face::kinds::KindId::SrfFunction)
}

/// nest.initplan's InitLoad (an Init's value into its frame pair, run at the first open that needs it), which a
/// re-opened store reads its InitPlan parameters through (rerun/store_read.rs), so it reads no registry row raw (lint RB).
pub(crate) fn init_load(site: &impl crate::admit::refusal::Site) -> &'static crate::rerun::params::InitLoad {
    crate::frame::iface::need_kind(site, crate::face::kinds::KindId::NestInitplan)
}

/// The extension row of kind `k` (expr.wholerow, expr.saop) an expression's claim is tried through (step/expr_ext.rs
/// `pre_claim`), so step reads no registry row raw (lint RB).
pub(crate) fn ext_impl(site: &impl crate::admit::refusal::Site, k: crate::face::kinds::KindId) -> &'static crate::face::step::ExprExtImpl {
    crate::frame::iface::need_kind(site, k)
}

/// expr.volatile's mechanism, which plans a VOLATILE call's held words (step/expr_ext.rs `held`), so step reads no
/// registry row raw (lint RB).
pub(crate) fn volatile_impl(site: &impl crate::admit::refusal::Site) -> &'static crate::face::step::VolatileImpl {
    crate::frame::iface::need_kind(site, crate::face::kinds::KindId::ExprVolatile)
}

/// nest.subplan's SubEval, which evaluates a lent SubPlan in the block that reads it (step/expr_ext_lazy.rs
/// `emit_subplan`), so step reads no registry row raw (lint RB).
pub(crate) fn sub_eval(site: &impl crate::admit::refusal::Site) -> &'static crate::rerun::face::SubEval {
    crate::frame::iface::need_kind(site, crate::face::kinds::KindId::NestSubplan)
}

/// D3374: nest.subplan's `SubHolds`.
pub(crate) fn sub_holds(site: &impl crate::admit::refusal::Site) -> &'static crate::rerun::face::SubHolds {
    crate::frame::iface::need_kind(site, crate::face::kinds::KindId::NestSubplan)
}

/// PG `recompute_limits`' rows a Limit reads from its child: its demand's count plus offset, both constants (a NULL or
/// negative constant, or one known only at bind, passes no bound).
fn limit_bound(count: &PlanBound<'_>, offset: &PlanBound<'_>) -> Bound {
    Demand { count, offset, ties: false }.rows().map_or(Bound::All, Bound::Rows)
}

impl<'g, 'p> Px<'g, 'p> {
    /// This node's request (filled by the walk before any make).
    pub fn request(&self, node: i32) -> &Request {
        self.iface.request(node)
    }

    /// A child's answer: `None` until the child's last part is made (producers are made first: frame/program.rs).
    pub fn answer(&self, node: i32) -> Option<&Answer> {
        self.iface.answers.get(node)
    }

    pub fn set_answer(&mut self, node: i32, a: Answer) {
        match self.iface.answers.get_mut(node) {
            Some(x) => *x = a,
            None => self.iface.answers.push(node, a),
        }
    }
}

/// The implementation of kind `k` (road `r`) as its face's type `T`: `NotBuilt` when no file provides it, a Bug when
/// files provide it but no row as a `T` (a face's maker missing from a kind that registered another face's).
pub fn need_kind<T: 'static>(cx: &impl Site, k: KindId) -> &'static T {
    need(cx, kind(k).is_some(), kind_as::<T>(k), Variant::Kind(k))
}

/// A road may have one row per face type (num.short: its decode and its compare extension); the typed row is found
/// whatever the registration order.
pub fn need_strategy<T: 'static>(cx: &impl Site, r: Strategy) -> &'static T {
    need(cx, strategy(r).is_some(), strategy_rows(r).iter().find_map(|m| m.downcast_ref()), Variant::Strategy(r))
}

#[cfg(test)]
mod tests {
    use gravity_abi::breaker::{Breaker, BreakerKind};
    use gravity_decompose::graph::{Bound as PlanBound, NodeOps};
    use gravity_decompose::{NoFacts, Op, PipelineGraph, PipelineNode, Source};
    use gravity_lower::Sequential;
    use gravity_program::{Arena, End, Pipeline, Reason};

    use super::{Answer, Bound, ColSet, Delivery, Iface, Request};
    use crate::frame::px::Px;
    use crate::graph::Gx;

    /// A stand-in op: the walk reads only a node's Limit.
    const OP: Op<'static> = Op::GroupRun(0);

    fn limit(count: PlanBound<'static>, offset: PlanBound<'static>) -> Op<'static> {
        Op::Limit { count, offset, ties: None }
    }

    /// A pipeline running `nodes` (node id, its ops), source first.
    fn pipe<'p>(a: gravity_program::ArenaRef, source: Source<'p>, end: End, nodes: Vec<(i32, Vec<Op<'p>>)>) -> PipelineNode<'p> {
        let mut p = PipelineNode::new(a, nodes[0].0, source, Pipeline::serial(-1, end, Reason::NoDrivingSite));
        for (id, ops) in nodes {
            p.node_ops.push(NodeOps { node_id: id, first_op: p.ops.len() as u16, producer_top: false });
            ops.into_iter().for_each(|o| p.push(o));
        }
        p
    }

    fn iface(g: &PipelineGraph<'_>) -> Iface {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        Iface::of(&Gx { graph: g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() })
    }

    /// A Limit bounds the node right under it by count + offset, as PG's ExecSetTupleBound; the Limit itself and the node
    /// below the bounded one are asked nothing (a bound stops at the node that takes it: the default hands `ALL` on).
    #[test]
    fn a_limit_bounds_the_node_under_it_by_count_plus_offset() {
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(pipe(a, Source::Const, End::Sink, vec![(3, vec![OP]), (2, vec![OP]), (1, vec![limit(PlanBound::Const(5), PlanBound::Const(2)), OP])]));
        let r = iface(&g);
        assert_eq!(r.request(2).bound, Bound::Rows(7));
        assert_eq!(r.request(2).delivery, Delivery::Stream);
        assert_eq!(*r.request(1), Request::ALL);
        assert_eq!(*r.request(3), Request::ALL);
    }

    /// No bound passes for LIMIT ALL, a negative count, a count or offset known only at bind, or WITH TIES.
    #[test]
    fn only_a_constant_count_and_offset_bound() {
        assert_eq!(super::limit_bound(&PlanBound::Const(4), &PlanBound::All), Bound::Rows(4));
        assert_eq!(super::limit_bound(&PlanBound::All, &PlanBound::Const(3)), Bound::All);
        assert_eq!(super::limit_bound(&PlanBound::Const(-1), &PlanBound::All), Bound::All);
        assert_eq!(super::limit_bound(&PlanBound::Const(i64::MAX), &PlanBound::Const(i64::MAX)), Bound::Rows(u64::MAX - 1));
    }

    /// Across a breaker source the request reaches the top node of the pipeline that fills it; an exact request stays exact
    /// down every node below, and a node two consumers read keeps the stricter delivery and the hints both agree on.
    #[test]
    fn requests_cross_a_breaker_and_exact_stays_exact() {
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let mut g = PipelineGraph::new(a);
        let b = Breaker::sized(BreakerKind::Materialize, 64).in_slot(1);
        g.pipelines.push(pipe(a, Source::Const, End::Breaker(b), vec![(5, vec![OP]), (4, vec![OP])]));
        g.pipelines.push(pipe(a, Source::Breaker { slot: 1 }, End::Sink, vec![(1, vec![limit(PlanBound::Const(3), PlanBound::All)])]));
        let mut r = iface(&g);
        assert_eq!(r.request(4).bound, Bound::Rows(3), "the Limit's child is the filling pipeline's top");
        r.ask(4, Request { delivery: Delivery::Exact, ..Request::ALL });
        assert_eq!(r.request(4).delivery, Delivery::Exact);
        assert_eq!(r.request(4).bound, Bound::All, "two consumers that disagree on the bound pass none");
        let below = super::pass_down(&[OP], r.request(4));
        assert_eq!((below.delivery, below.bound), (Delivery::Exact, Bound::All), "exact stays exact; the bound does not pass");
    }

    /// A node decompose asked Exact of (`PipelineGraph::exact`) keeps its consumer's bound, and every node below it is asked
    /// Exact too.
    #[test]
    fn decompose_sets_exact_and_it_reaches_below() {
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(pipe(a, Source::Const, End::Sink, vec![(4, vec![OP]), (3, vec![OP]), (2, vec![OP]), (1, vec![limit(PlanBound::Const(5), PlanBound::All)])]));
        g.exact.push(2);
        let r = iface(&g);
        assert_eq!((r.request(2).bound, r.request(2).delivery), (Bound::Rows(5), Delivery::Exact));
        assert_eq!((r.request(3).delivery, r.request(4).delivery), (Delivery::Exact, Delivery::Exact));
        assert_eq!(r.request(1).delivery, Delivery::Stream);
    }

    /// A node's answer is `None` until it is set, and the last set wins.
    #[test]
    fn answers_are_set_once_made() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(pipe(a, Source::Const, End::Sink, vec![(7, vec![OP])]));
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        let mut plan = Sequential::new(a);
        let mut px = Px::new(a, &gx, &mut plan);
        assert!(px.answer(7).is_none());
        let ans = |delivery| Answer { order: None, delivery, cols: ColSet::default(), bounded: false, markable: false };
        px.set_answer(7, ans(Delivery::Stream));
        px.set_answer(7, ans(Delivery::Exact));
        assert_eq!(px.answer(7).map(|x| x.delivery), Some(Delivery::Exact));
        assert_eq!(*px.request(7), Request::ALL);
    }

    /// D3585: `ByNode` finds by id whether indexed (0 up to `BY_NODE_INDEXED`) or searched (a negative id, one past it),
    /// each id's own value, in arrival order.
    #[test]
    fn by_node_finds_every_id() {
        let mut m = super::ByNode::<u32>::default();
        let ids = [5, -1, 0, super::BY_NODE_INDEXED as i32 + 3, 4000, 1];
        for (v, &id) in ids.iter().enumerate() {
            assert!(m.get(id).is_none(), "id {id} holds nothing before its push");
            m.push(id, v as u32);
        }
        for (v, &id) in ids.iter().enumerate() {
            assert_eq!(m.get(id), Some(&(v as u32)), "id {id}");
        }
        *m.get_mut(4000).expect("4000") = 9;
        assert_eq!((m.get(4000), m.get(2), m.get(-2)), (Some(&9), None, None));
        assert_eq!(m.items.iter().map(|x| x.0).collect::<Vec<_>>(), ids);
    }
}
