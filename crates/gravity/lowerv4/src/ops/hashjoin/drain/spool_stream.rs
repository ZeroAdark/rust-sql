//! C7STREAM (W2-DRAIN-A1B-C7STREAM, V4R-565; the user's C7 ask 09-29; design wf/design-W2-DRAIN-A1B-C7STREAM.txt, review
//! r1): join.drain's spooled form (nest/spool_drain.rs) with a STREAMED joined phase. Where the drain's consumer may stop,
//! the probe pipeline `k` is a pulled set (nest.cte's object: `ProtoKind::Cte` with the STREAM bit, exec v4/m8_cte.rs,
//! RT-2f) whose one reader is the drain `d` (no Follows edge): at the spool's count the reader pulls `k` on for a budget of
//! joined rows (1, doubling to nest/cte.rs's `WINDOW`), `k`'s end stops resumably once it took it (nest/cte.rs's end
//! rule), and only once `k` ended (sticky) does the reader go on to the drained records. PostgreSQL 18.6: ExecHashJoin
//! builds first for HJ_FILL_INNER (nodeHashjoin.c:297), pulls the outer lazily (:421), fills the inner only after the
//! outer ends (:437-455, :624), and ExecLimit stops pulling at its window's end (nodeLimit.c:158): the streamed run is a
//! prefix of the spooled one, the same rows in the same order. Everything else keeps the spooled form.
//! Where (`Gx::drain_streams`, from the graph, fail closed): (1) the consumer may stop (a LIMIT in the tail, a sink end);
//! (2) F1: every life `k` reads, its source's included, `d` reads too, so `members_in(k) == [k]` (frame/program.rs
//! checks it again: `Px::streamed`, a mismatch a Bug, never its Shape refusal): `k` is no graph CTE, so nest/cte.rs's
//! D2081 / D2059 checks never run on it; (3) F2: `k` owns no on-demand group (a SubPlan, a nest, a merge join or an
//! outer-first probe among its ops: a rerun set, a LAZY SubHash, a lazy build), and neither `k`, `d` nor a pipeline `d`
//! feeds is a rerun set's member, an Init's or a CTE's pipeline, or a build a lazy probe, a merge join, a hashed SubPlan or
//! an embedded inner reads; F6: `k` is N1's quiet probe itself (join/drain_spool.rs `spool_quiet`), whatever N1's
//! exemptions become, or (D1922, D3377) under a partial reader a probe that is not, pulled a joined row at a time
//! (spool_admit.rs `drain_exact`, `pull`'s `exact`). InitPlan values `k` reads stay streamed (nest/initplan.rs copies
//! by-reference values into the cell's per-execution arena).
//! The pin (review r1 F5): a parked `k` holds its heap page's pin and window lanes that may be KIND_BYREF pointers into
//! it; HeapRangeSource::release (heap_source.rs:518-524) unpins and clears them. Its one caller is call_claim_at_raw
//! (driver node.rs:1922-1926), the width-1 claim arm of a SITED producer (parallel lib.rs:861 / 883). `d`'s source is its
//! spool (Source::Breaker, no driving site): `d` runs through the emit / site-less arm (run_pipeline, call_pipeline_raw),
//! `k` through pull_nested -> run_pipeline; none releases. A sited claim at nested == 0 runs only before `k` starts or
//! after `d` ended, and `k` never resumes then (no DRAIN bit); runs nested inside `d` keep their pins (n.nested != 0).
//! So no release runs while a streamed pull is parked. A helper file (no rows). Owner M7-DRAIN.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::subplan::SubForm;
use gravity_decompose::{Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::shared::object::entries as m8;
use gravity_program::protos::{producers_in, Members, Proto, ProtoKind};
use gravity_program::Pipeline;
use mcx::{CollectIn, GrowIn, PgVec};

use crate::ops::shared::cte_reads::{reads, WINDOW};
use crate::rerun::spool::rec::Rec;
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::frame::row::RowVals;
use crate::graph::{EndKind, Gx};
use crate::admit::refusal::Reason;

/// The streamed words after the spool's count (nest/cte.rs's three, in its order), then the next pull's budget.
const W_BUDGET: u16 = 0;
const W_GOT: u16 = 1;
const W_DONE: u16 = 2;
const W_NEXT: u16 = 3;
pub(crate) const WORDS: u16 = 4;
/// The Cte proto's `arg` bit naming a streamed drain's pulled probe (exec v4/m8_cte.rs reads only nest/cte.rs's DRAIN,
/// bit 32); `arg`'s low 16 bits are the first streamed word.
const STREAM: u64 = 1 << 33;

impl Gx<'_, '_> {
    /// Whether spooled drain `d` (its Drain op `jd`) of probe `k` streams its joined phase: see the file's head. D1922
    /// (D3377): or, under a partial reader, a probe N1 does not find quiet (F6's exception: spool_admit.rs
    /// `drain_exact`), which the reader pulls a joined row at a time (`pull`).
    pub(crate) fn drain_streams(&self, k: usize, d: usize, jd: usize) -> bool {
        (self.spool_quiet(k) && self.stream_shape(k, d, jd)) || self.drain_exact(k, d, jd)
    }

    /// (1), (2) and (3) of the file's head, F6 aside.
    pub(crate) fn stream_shape(&self, k: usize, d: usize, jd: usize) -> bool {
        let stops = crate::frame::iface::holds_limit(&self.pipelines()[d].ops[jd + 1..]) || self.end_kind(d) == EndKind::Sink;
        let rd = reads(self, d);
        // drain (updatable_views#865): a streamed probe union's arms fill one spool unstreamed (spool.rs `spool_drain_end`)
        // D3379 RDRAIN: a row lock straight over the drain keeps the spooled form whatever its probe's order (a RIGHT
        // drain's probe builds first; `drain_on_demand` keeps an outer-first one's): its recheck reruns the producers under
        // its own words and reads from the fill's start, where the streamed reader's position restarts at the statement's
        // first record
        stops && self.drained_arms(k).is_none() && reads(self, k).iter().all(|x| rd.contains(x)) && !self.drain_on_demand(k, d) && !self.held_slot_reused(k, d) && self.free_lock(d, d).is_none()
    }

    /// (4) F7 (D3387): a pipeline after drain `d` produces a slot the pulled probe `k` holds: one `k` consumes or its own
    /// store. A pulled set keeps the live slots its pipeline consumes and its store for the statement (driver node
    /// `defer_rerun`: a parked pull may resume, its store freed only once its producer ended, `free_pulled_after`), but the
    /// decomposer hands a slot's number to a later producer once the slot's last reader in plan order (`d`) is past: that
    /// producer's open finds the slot still live (the run-time ledger's L3, a fallthrough). The spooled form keeps none of
    /// them past `d`.
    fn held_slot_reused(&self, k: usize, d: usize) -> bool {
        let ps = self.pipelines();
        let own = crate::frame::iface::breaker_of(&ps[k].desc).map(|b| b.slot);
        let src = if let Source::Breaker { slot } = ps[k].source { Some(slot) } else { None };
        let held = |s: u8| ps[k].desc.consumes.contains(&s) || own == Some(s) || src == Some(s);
        (d + 1..ps.len()).any(|q| crate::frame::iface::breaker_of(&ps[q].desc).is_some_and(|b| held(b.slot)))
    }

    /// (3) with F2 (the file's head), fail closed. D1922 (D3377): an outer-first probe in `k` over a build `d` reads too
    /// (the drained build itself: a RIGHT SEMI drain's own probe is outer-first) owns no lazy build: hashjoin
    /// build_class.rs `lazy_group_by` defers only a build its probe's pipeline reads alone, so that build runs before `k`.
    /// Not in a statement that locks or writes rows (lockrows lk-0100, D2131): its drain keeps the spooled form
    /// (spool_admit.rs `free_lock` / `free_write`), whose recheck reruns the producers under its own words; the streamed
    /// shape's recheck handed on the row before the rechecked one. There the outer-first probe stays on demand.
    fn drain_on_demand(&self, k: usize, d: usize) -> bool {
        let ps = self.pipelines();
        let rd = reads(self, d);
        let lazy_build = |slot: u8| self.life_of(k, slot).is_none_or(|l| !rd.contains(&(l.0 as usize)));
        let marks = |ops: &[Op<'_>]| ops.iter().any(|o| matches!(o, Op::Lock(_) | Op::Write(_)));
        let rows = (0..ps.len()).any(|q| marks(&ps[q].ops)) || ps.inners().iter().any(|(_, p)| marks(&p.ops));
        let own = |o: &Op<'_>| match o {
            Op::SubPlan(_) | Op::Join(JoinDesc::Nest { .. } | JoinDesc::Merge { .. }) => true,
            Op::Join(JoinDesc::Probe { slot, outer_first: true, .. }) => rows || lazy_build(*slot),
            _ => false,
        };
        let mut fed = mcx::pg_vec![in self.mcx; k, d];
        let mut i = 1;
        while let Some(&x) = fed.get(i) {
            let more: PgVec<'_, usize> = (x + 1..ps.len()).filter(|&q| !fed.contains(&q) && reads(self, q).contains(&x)).collect_in(self.mcx);
            fed.extend_in(more);
            i += 1;
        }
        let slots: PgVec<'_, u8> = fed.iter().filter_map(|&x| crate::frame::iface::breaker_of(&ps[x].desc).map(|b| b.slot)).collect_in(self.mcx);
        let lazy = |o: &Op<'_>| match o {
            Op::Join(JoinDesc::Probe { slot, outer_first: true, .. } | JoinDesc::Merge { slot, .. }) => Some(*slot),
            Op::SubPlan(s) => if let SubForm::Hashed { slot } = &s.form { Some(*slot) } else { None },
            _ => None,
        };
        let graph = (0..ps.len()).any(|q| ps[q].ops.iter().filter_map(lazy).any(|s| self.life_of(q, s).is_some_and(|l| fed.contains(&(l.0 as usize)))));
        let inner = ps.inners().iter().any(|(_, p)| matches!(p.source, Source::Breaker { slot } if slots.contains(&slot)) || p.ops.iter().filter_map(lazy).any(|s| slots.contains(&s)));
        let member = self.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&p| fed.contains(&(p as usize))));
        let init = self.graph.init_plans.iter().any(|x| fed.contains(&(x.pipeline as usize)));
        ps[k].ops.iter().any(own) || graph || inner || member || init
    }
}

impl<'g> Px<'g, '_> {
    /// `k` a pulled set whose one reader is drain `d` (no Follows edge: `d` pulls it); `x` the first streamed word.
    pub(crate) fn stream_probe(&mut self, k: usize, d: usize, x: u16) {
        let h = self.proto_words(1);
        self.declare_proto(Proto::new(ProtoKind::Cte, k as u16).peer(d as u16).words(h, 1).arg(x as u64 | STREAM));
    }

    /// Streamed probe `k`'s object handle word and first streamed word.
    pub(crate) fn streamed_probe(&self, k: usize) -> Option<(u16, u16)> {
        self.protos.iter().find(|p| p.kind == ProtoKind::Cte && p.pipeline as usize == k && p.arg & STREAM != 0).map(|p| (p.words.base, p.arg as u16))
    }

    /// frame/program.rs's nest.cte check of pulled set `p` (F1): a streamed probe (`true`) runs no producer of its own
    /// (`members_in(p) == [p]`) and reads only what its drain reads, so the shared-producer Shape refusal is never its; a
    /// mismatch is a Bug (`drain_streams` decided it from the graph). `false`: not a streamed probe.
    pub(crate) fn streamed(pipelines: &[Pipeline], protos: &[Proto], p: usize, m: &Members, node: i32) -> bool {
        let Some(c) = protos.iter().find(|x| x.kind == ProtoKind::Cte && x.pipeline as usize == p && x.arg & STREAM != 0) else { return false };
        if m.members != [p as u16] || !producers_in(pipelines, protos, p).all(|q| producers_in(pipelines, protos, c.peer as usize).any(|r| r == q)) {
            crate::refuse!(node, Reason::Bug { payload: "a streamed drain's probe with a producer its drain does not read (C7STREAM F1)" })
        }
        true
    }
}

fn word(b: &mut FunctionBuilder<'_>, env: &Env, w: u16) -> Value {
    b.load(Type::I64, env.params, w as i32 * 8)
}

fn set(b: &mut FunctionBuilder<'_>, env: &Env, w: u16, v: Value) {
    b.store(env.params, w as i32 * 8, v);
}

/// DrainFill's stop (nest/cte.rs's end rule), after the row is stored: counted, and once the pull's budget is taken a
/// resumable stop after the row. A zero budget never stops.
pub(crate) fn fill_stop(b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, x: u16) {
    let got = word(b, env, x + W_GOT);
    let got = b.int_bin(IntBinOp::Add, got, env.one);
    set(b, env, x + W_GOT, got);
    let budget = word(b, env, x + W_BUDGET);
    let (taken, on) = (b.icmp(IntCond::Uge, got, budget), b.icmp(IntCond::Ne, budget, env.zero));
    let (full, l_stop, l_on) = (b.int_bin(IntBinOp::And, taken, on), b.create_block(&[]), b.create_block(&[]));
    b.cond_br(full, (l_stop, &[]), (l_on, &[]));
    b.switch_to(l_stop);
    let st = b.iconst(Type::I64, gravity_abi::status::suspend(false));
    env.stop(b, &row.carry, row.next, st);
    b.switch_to(l_on);
}

/// The reader at the count, before the drained records (nest/cte.rs's reader rule): `k` ended, `l_drain`; else a pull
/// for the next budget, then `l_ready(j, count)` over the rows it stored. F3: a stopped pull with no new row pulls
/// again (a cancel-checked retry), so the drained phase (the unmatched build rows) never starts before `k`'s end has
/// marked every match. `exact` (D1922, D3377: a probe N1 does not find quiet, spool_admit.rs `drain_exact`): every
/// budget is one joined row, so the probe runs no row past the one the reader takes, as PostgreSQL's Hash Join pulls
/// its outer only as its parent asks.
pub(crate) fn pull(b: &mut FunctionBuilder<'_>, env: &mut Env, rec: &Rec, (h, x): (u16, u16), exact: bool, (j, l_ready, l_drain): (Value, Block, Block)) {
    let (l_call, l_ok, l_more, l_bad) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
    let done = word(b, env, x + W_DONE);
    let ended = b.icmp(IntCond::Ne, done, env.zero);
    b.cond_br(ended, (l_drain, &[]), (l_call, &[]));
    b.switch_to(l_call);
    let budget = match exact {
        true => env.one,
        false => {
            let next = word(b, env, x + W_NEXT);
            let first = b.icmp(IntCond::Eq, next, env.zero);
            let budget = b.select(first, env.one, next);
            let (twice, max) = (b.int_bin(IntBinOp::Add, budget, budget), b.iconst(Type::I64, WINDOW));
            let over = b.icmp(IntCond::Sgt, twice, max);
            let next = b.select(over, max, twice);
            set(b, env, x + W_NEXT, next);
            budget
        }
    };
    set(b, env, x + W_BUDGET, budget);
    set(b, env, x + W_GOT, env.zero);
    let f = b.declare_host_once("m8_cte_pull", &[Type::Ptr, Type::I64, Type::I64], Some(Type::I64), m8::m8_cte_pull as *const () as usize, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() });
    let hv = word(b, env, h);
    let r = b.call_host(f, &[env.ctx, hv, budget], Some(Type::I64)).expect("m8_cte_pull returns its status");
    set(b, env, x + W_BUDGET, env.zero);
    // below zero: the pull's error (the host stashed it), raised inside the reader's read as PG raises it in its fetch
    let bad = b.icmp(IntCond::Slt, r, env.zero);
    b.mark_cold(l_bad);
    b.cond_br(bad, (l_bad, &[]), (l_ok, &[]));
    b.switch_to(l_bad);
    let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
    b.br(env.l_raise, &[e]);
    b.switch_to(l_ok);
    // 1: `k` ended (sticky: later reads go on to the drained records)
    let ended = b.icmp(IntCond::Ne, r, env.zero);
    let flag = b.select(ended, env.one, env.zero);
    set(b, env, x + W_DONE, flag);
    let n = rec.count(b, env);
    let more = b.icmp(IntCond::Slt, j, n);
    b.cond_br(more, (l_ready, &[j, n]), (l_more, &[]));
    b.switch_to(l_more);
    let l_again = b.create_block(&[]);
    b.cond_br(ended, (l_drain, &[]), (l_again, &[]));
    b.switch_to(l_again);
    crate::frame::cancel::check(b, env.ctx, env.l_cancel);
    b.br(l_call, &[]);
}

#[cfg(test)]
mod tests {
    use gravity_abi::breaker::{Breaker, BreakerKind};
    use gravity_decompose::effects::Volatility;
    use gravity_decompose::graph::{Bound, InitPlan, Rerun};
    use gravity_decompose::facts::nest::NestFacts;
    use gravity_decompose::ops::shared::subplan::SubLink;
    use gravity_decompose::{Am, NoFacts, PipelineGraph, PipelineNode};
    use gravity_program::{Arena, ArenaBox, ArenaRef, ArenaVec, End, Reason as Why, SiteDesc};
    use types_nodes::jointype::JoinType;
    use types_nodes::list::{IntList, NodeList};

    use super::*;

    /// A shape change from the one C7 streams (`Fx::Plain`): the spooled drain's fixture (join/drain_spool.rs's) with the
    /// build 0, an extra pipeline 1, the probe 2 (a quiet RIGHT probe of slot 0 into its spool 1) and the drain 3.
    #[derive(Clone, Copy, PartialEq)]
    enum Fx {
        Plain,
        FoldLimit,
        Fold,
        Sort,
        SourceBreaker,
        OwnBuild,
        OwnNest,
        OuterFirst,
        /// D1922 (D3377): the drain's own probe outer-first (a RIGHT SEMI drain, PG's Hash Right Semi Join).
        SemiOuterFirst,
        /// D3387: `FoldLimit` whose reader after the fold produces slot 0 again (the build the probe consumes).
        ReuseFold,
        NotQuiet,
        /// D1922 (D3377): `NotQuiet` under a partial reader.
        PartialLoud,
        DrainRerun,
        DrainInit,
        LazyReader,
        EagerReader,
        InnerReader,
    }

    fn with<R>(fx: Fx, f: impl FnOnce(&Gx<'_, '_>) -> R) -> R {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil()));
        let scan = || Source::Range { site: SiteDesc::none_in(a), am: Am::Pgrcolumnar, scanrelid: 1, relid: 16384 };
        let into = |kind, slot| Pipeline::serial(-1, End::Breaker(Breaker::sized(kind, 4096).in_slot(slot)), Why::NoDrivingSite);
        let sink = || Pipeline::serial(-1, End::Sink, Why::NoDrivingSite);
        let probe = |slot, jt, outer_first| Op::Join(JoinDesc::Probe { slot, jointype: jt, clauses: nil, keys: nil, qual: nil, unique: false, marks: true, decls: ArenaVec::new_in(a), inner_node: 1, outer_first, catalog: false });
        let nest = |node, inner| Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, params: nil, inner: ArenaBox::new_in(inner, a), qual: nil, unique: false, inner_node: 9, facts: NestFacts { node, invariant: true, volatility: Volatility::Immutable, raise_free: true, eq: None } });
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(PipelineNode::new(a, 1, scan(), into(BreakerKind::HashBuild, 0)));
        let one = if fx == Fx::OwnBuild { BreakerKind::HashBuild } else { BreakerKind::Materialize };
        g.pipelines.push(PipelineNode::new(a, 3, scan(), into(one, 3)));
        let src = if fx == Fx::SourceBreaker { Source::Breaker { slot: 3 } } else { scan() };
        let reads: &[u8] = if fx == Fx::OwnBuild { &[0, 3] } else { &[0] };
        let mut k = PipelineNode::new(a, 2, src, reads.iter().fold(into(BreakerKind::Materialize, 1), |p, &s| p.consuming_in(a, s)));
        match fx {
            Fx::OwnBuild => k.push(probe(3, JoinType::JOIN_INNER, false)),
            Fx::OwnNest => k.push(nest(78, PipelineNode::new(a, 9, scan(), sink()))),
            Fx::OuterFirst => k.push(probe(3, JoinType::JOIN_INNER, true)),
            _ => {}
        }
        let semi = fx == Fx::SemiOuterFirst;
        k.push(probe(0, if semi { JoinType::JOIN_RIGHT_SEMI } else { JoinType::JOIN_RIGHT }, semi));
        if matches!(fx, Fx::NotQuiet | Fx::PartialLoud) {
            k.push(Op::Limit { count: Bound::Const(10), offset: Bound::All, ties: None });
        }
        g.partial = fx == Fx::PartialLoud;
        g.pipelines.push(k);
        let end = match fx {
            Fx::FoldLimit | Fx::Fold | Fx::ReuseFold => into(BreakerKind::PlainAgg, 5),
            Fx::Sort => into(BreakerKind::Sort, 5),
            Fx::LazyReader | Fx::EagerReader | Fx::InnerReader => into(BreakerKind::HashBuild, 5),
            _ => sink(),
        };
        let mut d = PipelineNode::new(a, 4, Source::Breaker { slot: 0 }, end.consuming_in(a, 1));
        d.push(Op::Join(JoinDesc::Drain { matches: 1, jointype: if semi { JoinType::JOIN_RIGHT_SEMI } else { JoinType::JOIN_RIGHT } }));
        if !matches!(fx, Fx::Fold | Fx::Sort) && end.ends_in != End::Sink {
            d.push(Op::Limit { count: Bound::Const(10), offset: Bound::All, ties: None });
        }
        g.pipelines.push(d);
        let reuse = fx == Fx::ReuseFold;
        let mut q = PipelineNode::new(a, 6, scan(), if reuse { into(BreakerKind::PlainAgg, 0) } else { sink() }.consuming_in(a, 5));
        match fx {
            Fx::LazyReader => q.push(probe(5, JoinType::JOIN_INNER, true)),
            Fx::EagerReader => q.push(probe(5, JoinType::JOIN_INNER, false)),
            Fx::InnerReader => q.push(nest(79, PipelineNode::new(a, 9, Source::Breaker { slot: 5 }, sink()))),
            _ => {}
        }
        g.pipelines.push(q);
        if reuse {
            g.pipelines.push(PipelineNode::new(a, 7, scan(), sink().consuming_in(a, 0)));
        }
        if fx == Fx::DrainRerun {
            let mut pipelines = ArenaVec::new_in(a);
            pipelines.push(3);
            g.reruns.push(Rerun { node_id: 77, plan_id: 0, pipelines, slots: [0; 4] });
        }
        if fx == Fx::DrainInit {
            let params: &IntList<'_> = Box::leak(Box::new(IntList::nil()));
            g.init_plans.push(InitPlan { plan_id: 9, pipeline: 3, link: SubLink::Cte, param: -1, params, slot: Some(5), volatility: Volatility::Immutable, raise_free: true, parallel_safe: true, attach: -1 });
        }
        f(&Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() })
    }

    fn streams(fx: Fx) -> bool {
        with(fx, |gx| gx.drain_streams(2, 3, 0))
    }

    /// (1) both ways: a sink or a LIMIT in the tail streams; a plain fold or a sort with no LIMIT (the full reads C1 / C2
    /// / C3) keeps the spooled form.
    #[test]
    fn streams_only_where_the_consumer_may_stop() {
        assert!(streams(Fx::Plain), "a sink a cursor may stop");
        assert!(streams(Fx::FoldLimit), "a LIMIT in the tail over a fold");
        assert!(!streams(Fx::Fold), "a plain fold reads every row (C1)");
        assert!(!streams(Fx::Sort), "a sort reads every row (C2 / C3)");
    }

    /// (2) F1 both ways: a probe reading a life its drain does not (a subquery breaker as its source, its own inner hash
    /// join's build) keeps the spooled form; (the plain shape streams above). F6: a probe N1 does not find quiet does too.
    #[test]
    fn streams_only_a_probe_whose_producers_its_drain_reads() {
        assert!(!streams(Fx::SourceBreaker), "a probe over a subquery breaker");
        assert!(!streams(Fx::OwnBuild), "a probe with its own inner hash join");
        assert!(!streams(Fx::NotQuiet), "F6: N1's quiet probe only");
    }

    /// D1922 (D3377): F6's exception both ways: under a partial reader a probe N1 does not find quiet streams, pulled a
    /// joined row at a time (spool_admit.rs `drain_exact`); under a whole reader it keeps the spooled form, and a quiet
    /// probe's stream is not the exact one.
    #[test]
    fn a_loud_probe_streams_exactly_under_a_partial_reader_only() {
        assert!(!streams(Fx::NotQuiet), "a whole reader");
        assert!(streams(Fx::PartialLoud) && with(Fx::PartialLoud, |gx| gx.drain_exact(2, 3, 0)), "a partial reader");
        assert!(with(Fx::Plain, |gx| !gx.drain_exact(2, 3, 0)), "no partial reader");
    }

    /// (3) with F2 both ways: an on-demand group the probe owns (a nest's rerun set, an outer-first probe's lazy build), a
    /// drain in a rerun set or an Init's (a CTE's) pipeline, a drain feeding a lazy build or an embedded inner keeps the
    /// spooled form; a drain feeding a build-first probe streams.
    #[test]
    fn streams_outside_every_on_demand_group() {
        for fx in [Fx::OwnNest, Fx::OuterFirst, Fx::DrainRerun, Fx::DrainInit, Fx::LazyReader, Fx::InnerReader] {
            assert!(with(fx, |gx| gx.drain_on_demand(2, 3)) && !streams(fx), "on demand");
        }
        assert!(!with(Fx::EagerReader, |gx| gx.drain_on_demand(2, 3)) && streams(Fx::EagerReader), "a build-first reader");
        assert!(!with(Fx::Plain, |gx| gx.drain_on_demand(2, 3)));
    }

    /// D3387 (F7) both ways: a pipeline after the drain that produces a slot the pulled probe holds (here its build's slot 0,
    /// reused once the drain, its last reader, is past) keeps the spooled form; the same fold read by a sink streams.
    #[test]
    fn a_slot_the_pulled_probe_holds_produced_after_the_drain_keeps_the_spooled_form() {
        assert!(streams(Fx::FoldLimit) && !with(Fx::FoldLimit, |gx| gx.held_slot_reused(2, 3)));
        assert!(with(Fx::ReuseFold, |gx| gx.held_slot_reused(2, 3) && !gx.stream_shape(2, 3, 0)) && !streams(Fx::ReuseFold));
    }

    /// D1922 (D3377): the drain's own outer-first probe (a RIGHT SEMI drain) owns no lazy build (the drain reads that build
    /// too), so its shape streams; an outer-first probe over a build only the probe reads stays on demand (`OuterFirst`).
    #[test]
    fn a_drains_own_outer_first_probe_is_no_on_demand_group() {
        assert!(with(Fx::SemiOuterFirst, |gx| !gx.drain_on_demand(2, 3) && gx.stream_shape(2, 3, 0)));
    }

    /// C4: the STREAM bit is not nest/cte.rs's DRAIN (bit 32: exec's drain_at_end would pull the probe to its end), and the
    /// first streamed word (`arg`'s low 16 bits) never reaches it.
    #[test]
    fn stream_is_not_drain() {
        assert!(include_str!("../../ctescan/lower.rs").contains("const DRAIN: u64 = 1 << 32;"));
        assert_eq!(STREAM & (1 << 32), 0);
        assert!(STREAM > u64::from(u16::MAX));
    }

    fn program(a: ArenaRef, own: bool) -> Vec<Pipeline> {
        let into = |kind, slot| Pipeline::serial(-1, End::Breaker(Breaker::sized(kind, 4096).in_slot(slot)), Why::NoDrivingSite);
        let mut k = into(BreakerKind::Materialize, 1).consuming_in(a, 0);
        if own {
            k = k.consuming_in(a, 2);
        }
        vec![into(BreakerKind::HashBuild, 0), into(BreakerKind::HashBuild, 2), k, Pipeline::serial(-1, End::Sink, Why::NoDrivingSite).consuming_in(a, 0).consuming_in(a, 1)]
    }

    /// C4 / F1 at frame/program.rs: a streamed probe whose producers its drain reads is never the shared-producer Shape
    /// refusal's (`streamed` is true, members [k]); one with a producer of its own is a Bug; a graph CTE is not its.
    #[test]
    fn program_never_shape_refuses_a_stream_proto() {
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let proto = |arg| [Proto::new(ProtoKind::Cte, 2).peer(3).words(9, 1).arg(arg)];
        let ps = program(a, false);
        let m = gravity_program::protos::members_in(&ps, &proto(4 | STREAM), 2);
        assert_eq!((m.members.as_slice(), m.shared.as_slice()), (&[2u16][..], &[0u16][..]));
        assert!(Px::streamed(&ps, &proto(4 | STREAM), 2, &m, 7), "no Shape refusal");
        assert!(!Px::streamed(&ps, &proto(4), 2, &m, 7), "a graph CTE keeps frame/program.rs's rule");
        let ps = program(a, true);
        let m = gravity_program::protos::members_in(&ps, &proto(4 | STREAM), 2);
        let e = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Px::streamed(&ps, &proto(4 | STREAM), 2, &m, 7))).expect_err("a Bug");
        assert!(e.downcast_ref::<crate::admit::refusal::Refusal>().is_some_and(|r| matches!(r.reason, Reason::Bug { .. })));
    }
}
