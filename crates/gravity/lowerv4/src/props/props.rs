//! Graph properties, computed once per statement (plan §3.1 step 1): the kinds the graph needs, each with the first plan
//! node that needs it, and the property tokens `applies.rs` reads (storage, keys, order, counts, widths). Every token comes
//! from the graph's descriptors and expressions; the census (`v4plan.py classify()`) reads the same graph lines for each
//! fixture, and the two agree exactly (M0 reconciliation). `UNSEEN` tokens no graph fact decides are taken to hold. A
//! function the catalog cannot describe is hosted and VOLATILE (it names no builtin shim). CORE: open until M1 exits.
use gravity_decompose::ops::shared::agg::{AggDecl, GroupDecl, KeyDecl, Register};
use gravity_decompose::ops::shared::agg_class::Class;
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::subplan::{SubForm, SubPlanDesc};
use gravity_decompose::ops::modifytable::trig_facts::cmd_bits;
use gravity_decompose::{Op, PipelineNode, Source};
use types_nodes::jointype::JoinType;
use mcx::{GrowIn, PgVec};
use types_nodes::list::NodeList;

use crate::admit::refusal::check_interrupts;
use crate::applies::{Prop, PropSet};
use crate::face::kinds::{KindId, KindSet};
use crate::graph::{kind_set, EndKind, Gx};
use crate::rep::datum_kind;

pub(crate) use crate::ops::sort::topn::count_cut::{count_cut, key_check, pair_partition, pair_routed, tie_key};

/// V4R-118: the sort roads' shapes and form G, read by the run files (runs/sort.rs, keylane.rs) and keyed/emit_group.rs.
pub(crate) use crate::effects::visible::sort::finals_once;
/// V4R-126: road topk.fetch's shape (props/topk.rs), which runs/fetch.rs reads too.
pub(crate) use crate::ops::sort::strategies::fetch_shape::topk_wide;


/// The tokens no graph fact decides yet: taken to hold. None since V4R-126 (`select_star` became `topk_wide`, props/topk.rs).
pub const UNSEEN: &[Prop] = &[];

/// A graph's properties: what admission reads, computed once.
#[derive(Clone, Debug)]
pub struct Props<'g> {
    /// Every kind the graph needs, in graph order, with the first plan node that needs it.
    pub need: PgVec<'g, (KindId, i32)>,
    pub kinds: KindSet,
    pub props: PropSet,
    /// The largest planner row estimate over the pipelines (width policy, table sizing).
    pub max_rows: u64,
    pub pipelines: u16,
}

impl<'g> Props<'g> {
    pub fn of(gx: &Gx<'g, '_>) -> Props<'g> {
        let mut w = Walk { gx, need: PgVec::new_in(gx.mcx), p: PropSet::default(), node: gx.root_node(), nests: (0, 0) };
        w.add(KindId::ExprCore);
        let mut max_rows = 0u64;
        // D3610: each pipeline's (and group's) kinds are a cancel point, as each pipeline's resolve and emit are (D231): a
        // pipeline's kinds ask graph-wide questions (a Sort arm's `merge_inner` walks every arm of its run), so a cursor over
        // 2000 partitions spent 15 min here, cancel and statement_timeout unread
        for k in 0..gx.pipelines().len() {
            check_interrupts(gx.pipelines()[k].node_id);
            max_rows = max_rows.max(gx.pipelines()[k].desc.predicted_rows);
            w.pipeline(k);
        }
        for g in gx.groups() { // D1210: the overlay's re-declared groups
            check_interrupts(gx.root_node());
            w.group(g);
        }
        w.node = gx.root_node();
        if !gx.graph.init_plans.is_empty() {
            w.add(KindId::NestInitplan);
        }
        if gx.graph.init_plans.iter().any(|i| matches!(i.link, gravity_decompose::ops::shared::subplan::SubLink::Cte)) {
            w.add(KindId::NestCte);
        }
        // a recursive union's term set (W8-RECURSE: `plan_id` RECURSIVE_SET) is setop.recursive's, which names it
        if gx.graph.reruns.iter().any(|r| r.plan_id != gravity_decompose::ops::shared::setop::RECURSIVE_SET) {
            w.add(KindId::NestRerun);
        }
        let Walk { need, mut p, nests, .. } = w;
        if nests == (1, 1) {
            p.add(Prop::CatalogNest);
        }
        UNSEEN.iter().for_each(|&t| p.add(t));
        let kinds = kind_set(&need);
        if kinds.any(KindSet::of(&[KindId::ClassNumPacked, KindId::ClassNumVarlena, KindId::ClassNumWide])) {
            p.add(Prop::Numeric);
        }
        let rows = KindSet::of(&[
            KindId::SrcHeap,
            KindId::SrcPgrc,
            KindId::SrcIndex,
            KindId::SrcBitmap,
            KindId::SrcIndexOnly,
            KindId::SrcIndexArray,
            KindId::SrcIndexAm,
            KindId::SrcTid,
            KindId::SrcSample,
            KindId::SrcAppend,
            KindId::SrfFunction,
        ]);
        if kinds.any(KindSet::of(&[KindId::SrcResult, KindId::SrcValues])) && !kinds.any(rows) {
            p.add(Prop::ValuesOnly);
        }
        Props { need, kinds, props: p, max_rows, pipelines: gx.pipelines().len() as u16 }
    }
}

pub(crate) struct Walk<'a, 'g, 'p> {
    pub(crate) gx: &'a Gx<'g, 'p>,
    pub(crate) need: PgVec<'g, (KindId, i32)>,
    pub(crate) p: PropSet,
    /// The plan node the kinds found now are charged to.
    pub(crate) node: i32,
    /// (nested loops, of which psql's lookup shape): `CatalogNest` when the plan's one nest is.
    pub(crate) nests: (u16, u16),
}

impl<'p> Walk<'_, '_, 'p> {
    pub(crate) fn add(&mut self, k: KindId) {
        if !self.need.iter().any(|(j, _)| *j == k) {
            self.need.push_in((k, self.node));
        }
    }

    pub(super) fn pipeline(&mut self, k: usize) {
        let gx = self.gx;
        let p = &gx.pipelines()[k];
        self.node = p.node_id;
        let group = gx.drained_group(k);
        let mut ks = PgVec::new_in(gx.mcx);
        gx.pipeline_kinds(k, &mut ks);
        for (kind, node) in ks {
            self.node = node;
            self.add(kind);
        }
        self.node = p.node_id;
        // D455: a settled union's feed / collect (not a MergeAppend's runs) fills the arms' spool, nest.spool's (M8):
        // source/append.rs refuses by that name, so the statement needs it beside src.append
        if !gx.merged(k) && p.ops.iter().any(|op| matches!(op, Op::Append(AppendDesc::Feed { .. } | AppendDesc::Union { .. }))) {
            self.add(KindId::NestSpool);
        }
        self.source(p);
        self.ops(p, group);
        if p.desc.admit.is_some() {
            self.add(KindId::KeyedTwopass);
        }
        if matches!(gx.end_kind(k), EndKind::Sink | EndKind::RowOp(_)) {
            self.emits(p);
        }
        if let EndKind::RowOp(cmd) = gx.end_kind(k) {
            self.triggers(p, cmd_bits(cmd));
        }
        self.marks(p);
    }

    /// A pipeline's ops, and a nest inner's; two or more join steps on one pipeline are a chain (`join.chain`).
    pub(super) fn ops(&mut self, p: &PipelineNode<'p>, group: Option<&GroupDecl<'p>>) {
        crate::stack_check(); // D3832
        let joins = p.ops.iter().filter(|op| matches!(op, Op::Join(JoinDesc::Probe { .. } | JoinDesc::Nest { .. } | JoinDesc::Memo { .. } | JoinDesc::Merge { .. }))).count();
        if joins >= 2 {
            self.add(KindId::JoinChain);
        }
        let mut input: Option<&'p NodeList<'p>> = None;
        for op in p.ops.iter() {
            self.op(op, p, group, input);
            if let Op::Project(l) = op {
                input = Some(*l);
            }
            // M3 decision 16: psql \d's lookup, the only nest of its plan (a catalog Probe: decompose join.rs `catalog_lookup`,
            // V4R-142; its name order keys stay rep.varlena.order until O3's census half)
            if let Op::Join(JoinDesc::Nest { .. } | JoinDesc::Probe { catalog: true, .. }) = op {
                self.nests.0 += 1;
                self.nests.1 += matches!(op, Op::Join(JoinDesc::Probe { catalog: true, .. })) as u16;
            }
            if let Op::Join(JoinDesc::Nest { inner, .. }) | Op::SubPlan(SubPlanDesc { form: SubForm::Nested { inner }, .. }) = op {
                self.source(inner);
                self.ops(inner, None);
            }
        }
    }

    /// The cells a sink or a write emits (its last projection's outputs) are read through their Reps' datum arms.
    pub(super) fn emits(&mut self, p: &PipelineNode<'p>) {
        let Some(l) = p.ops.iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None }) else { return };
        for e in l.iter() {
            if let Some(o) = self.gx.out_of(e) {
                self.add(datum_kind(o.rep));
            }
        }
    }
}

/// `tid` (TIDOID).
pub(crate) const TID: u32 = 27;

/// A keyed group's key that keys through the hosted arm (fmgr hash + eq over the kept datum; rep.hosted.hash, M6) rather
/// than its word image: a `tid` key (GROUP BY ctid, D387). A float key keys by its word image (it folds `-0` and NaNs, as
/// float8eq) and its record keeps one member's datum for the output (M6 L0-m, keyed/emit_key.rs; D386 before it).
pub(crate) fn hosted_group_key(_g: &GroupDecl<'_>, key: &KeyDecl<'_>) -> bool {
    key.tf == TID
}

/// The class kind that folds aggregate `a`: its catalog class, with numeric and text classes split by the register
/// decompose chose for it (`fold_of`): a decided scale (`SumNum`..`MaxNum`) folds packed (V4R-38: a wide product's
/// `SumNumWide` / `AvgNumWide` in class.num_wide's i256 slot), a code-lane extremum
/// (`MinCode` / `MaxCode`) by code, anything else in its `Trans::Var` slot (varlena numeric, text). Admission (the walk)
/// and the fold lowerings (fold.plain, fold.filter, keyed) all take it from here, so a record's slot and its admitted
/// class cannot disagree (D256).
pub fn class_kind(a: &AggDecl<'_>) -> KindId {
    use gravity_decompose::ops::shared::agg_class::Ext;
    let scaled = matches!(a.reg, Register::SumNum | Register::AvgNum | Register::MinNum | Register::MaxNum | Register::MinNumDyn | Register::MaxNumDyn);
    match a.class {
        Class::CountStar | Class::CountAny | Class::CountPair => KindId::ClassWord,
        Class::SumI64 | Class::SumI128 | Class::AvgI128 | Class::BoolAnd | Class::BoolOr | Class::BitI64(_) => KindId::ClassWord,
        Class::SumF32 | Class::SumF64 | Class::AvgF64 => KindId::ClassFloat,
        Class::SumNum | Class::AvgNum if scaled => KindId::ClassNumPacked,
        Class::SumNum | Class::AvgNum if matches!(a.reg, Register::SumNumWide | Register::AvgNumWide) => KindId::ClassNumWide,
        Class::SumNum | Class::AvgNum => KindId::ClassNumVarlena,
        Class::Min(x) | Class::Max(x) => match x {
            Ext::Int | Ext::U64 => KindId::ClassWord,
            Ext::F64 => KindId::ClassFloat,
            Ext::Num if scaled => KindId::ClassNumPacked,
            Ext::Num => KindId::ClassNumVarlena,
            Ext::Text | Ext::Bytes if matches!(a.reg, Register::MinCode | Register::MaxCode) => KindId::ClassMinmaxCode,
            Ext::Text | Ext::Bytes => KindId::ClassText,
            Ext::Tid | Ext::Opaque => KindId::ClassOther,
        },
        Class::Collect(_) => KindId::ClassCollect,
        Class::User => KindId::ClassUser,
        Class::Stat3F64(_) | Class::Stat3I128(_) | Class::Stat3Num(_) | Class::Stat6F64(_) => KindId::ClassStats,
        Class::OrderedSet(_) | Class::Hypothetical(_) => KindId::ClassOrdered,
        Class::SumMoney | Class::SumInterval | Class::AvgInterval | Class::BitVar(_) => KindId::ClassOther,
    }
}

#[cfg(test)]
mod tests {
    use gravity_decompose::catalog::NoFacts;
    use gravity_decompose::{PipelineGraph, PipelineNode, Source};
    use gravity_program::{Arena, End, Pipeline, Reason as Why};

    use crate::admit::refusal::Reason;
    use crate::graph::Gx;

    /// D3610: a pending interrupt (a cancel, a statement_timeout) stops the kinds pass at its first pipeline as the
    /// `Interrupted` refusal, before any pipeline's graph-wide questions run; with none pending it walks every pipeline.
    #[test]
    fn the_kinds_pass_takes_a_cancel() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let arena = Arena::new(1 << 22);
        let a = arena.handle();
        let mut g = PipelineGraph::new(a);
        for k in 0..3 {
            g.pipelines.push(PipelineNode::new(a, k + 1, Source::Const, Pipeline::serial(-1, End::Sink, Why::NoDrivingSite))); // alloc: Cold
        }
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        assert!(crate::catch(0, || drop(super::Props::of(&gx))).is_ok(), "nothing pending: the pass walks every pipeline");
        // the backend's seam (the server's is tcop's init_seams): the facade's `pending` reads the interrupt globals through it
        static BACKEND: std::sync::Once = std::sync::Once::new();
        BACKEND.call_once(|| postgres_seams::interrupts_pending::set(|| init_small::globals::InterruptPending() && init_small::globals::InterruptsCanBeProcessed()));
        init_small::globals::SetInterruptPending(true);
        let r = crate::catch(0, || drop(super::Props::of(&gx)));
        init_small::globals::SetInterruptPending(false);
        assert_eq!(r.map_err(|r| (r.reason, r.node)), Err((Reason::Interrupted, 1)), "stopped at the first pipeline's check");
    }
}
