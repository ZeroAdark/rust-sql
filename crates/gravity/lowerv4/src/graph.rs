//! Decompose's graph as v4 reads it (v3 graph.rs, kept with changes): each pipeline's source, steps and end named by the
//! kinds of `face/kinds.rs` (write ends included), node outputs, and the lives slots are read from. The structural view:
//! kinds a node's descriptor decides alone. Kinds that need expressions, types or facts (fold classes, Rep × accessor
//! arms, FILTER, HAVING, two-pass, array keys, PL functions) are `props.rs`'s.
use gravity_abi::breaker::BreakerKind;
use gravity_decompose::ops::shared::agg::{GroupDecl, GroupForm};
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::catalog::{Am, IndexAm};
use gravity_decompose::graph::KeyForm;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::setop::SetDesc;
use gravity_decompose::ops::shared::sort::SortForm;
use gravity_decompose::ops::shared::srf::SrfPlace;
use gravity_decompose::ops::shared::subplan::{SubForm, SubLink, SubPlanDesc};
use gravity_decompose::{Facts, Op, PipelineGraph, PipelineNode, Source};
use gravity_expr::Catalog;
use gravity_program::{Cmd, End, ExprOut, LifeId};
use types_nodes::jointype::JoinType;
use types_nodes::Node;

use crate::face::kinds::{KindId, KindSet};
use crate::frame::iface::breaker_of;
use gravity_driver::mem::Leases;
use mcx::{CollectIn, GrowIn, PgVec};

/// The end of a pipeline as the frame's end faces see it: decompose's `End`, or a row op when the pipeline writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EndKind {
    Sink,
    Breaker(BreakerKind),
    /// H27: the row-op end of a write pipeline (DELETE / INSERT / UPDATE / MERGE), whatever `End` it declares.
    RowOp(Cmd),
}

impl EndKind {
    /// The end pipeline description `d` declares, read through the interface home (`frame::iface::breaker_of`).
    pub fn of_desc(d: &gravity_program::Pipeline) -> EndKind {
        breaker_of(d).map_or(EndKind::Sink, |b| EndKind::Breaker(b.kind))
    }

    pub const fn of_end(e: End) -> EndKind {
        match e {
            End::Sink => EndKind::Sink,
            End::Breaker(b) => EndKind::Breaker(b.kind),
        }
    }

    /// The kind that builds this end (the producer's side of a breaker).
    pub const fn kind(self) -> KindId {
        match self {
            EndKind::Sink => KindId::EndSink,
            EndKind::Breaker(b) => breaker_kind(b),
            EndKind::RowOp(c) => write_kind(c),
        }
    }
}

/// A breaker's kind: the one that builds its producer side and emits it when a later pipeline drains it.
pub const fn breaker_kind(b: BreakerKind) -> KindId {
    match b {
        BreakerKind::PlainAgg => KindId::FoldPlain,
        BreakerKind::HashAgg => KindId::KeyedHashagg,
        BreakerKind::HashBuild => KindId::JoinInner,
        BreakerKind::Sort => KindId::RunsSort,
        BreakerKind::Materialize => KindId::NestSpool,
        BreakerKind::Distinct => KindId::KeyedDistinctPlain,
        BreakerKind::TopK => KindId::RunsTopkRows,
    }
}

/// An aggregate breaker's kind from its group's form: hashed, distinct and two-pass groups are keyed; a sorted / Group
/// group is `runs.group` (whatever breaker holds its records), and so is a group whose every key the planner dropped when
/// its output reads one; a Unique group `runs.unique`; a plain one `fold.plain`.
pub fn group_kind(g: &GroupDecl<'_>) -> KindId {
    match g.form {
        // every key dropped as redundant (`WHERE k = 0 GROUP BY k`): PostgreSQL's GroupAggregate over no key column, no
        // row over an empty input and the pinned columns from the group's first row (`empty_no_row`, `pinned`). The plain
        // fold keeps the first (its drain reads the record's row count) and, D345 (V4R-434), a pinned column decompose did
        // not key as the host's kept column (fold/kept.rs); a DISTINCT is the sorted group's, which names D345, unless
        // regroup.rs re-declared the group plain with its DISTINCT host-sorted (D3374)
        GroupForm::Distinct if g.empty_no_row && !grouped(g) => KindId::RunsGroup,
        GroupForm::Plain => KindId::FoldPlain,
        GroupForm::Hashed => KindId::KeyedHashagg,
        GroupForm::Distinct if !grouped(g) => KindId::KeyedDistinctPlain,
        GroupForm::Distinct => KindId::KeyedDistinctGrouped,
        GroupForm::Sorted | GroupForm::Group => KindId::RunsGroup,
        GroupForm::Unique => KindId::RunsUnique,
        GroupForm::Pass1 => KindId::KeyedTwopass,
    }
}

/// Whether a group has a group key (resno 0 is a DISTINCT aggregate's argument key, never a group key).
pub fn grouped(g: &GroupDecl<'_>) -> bool {
    g.keys.iter().any(|k| k.resno != 0)
}

/// D853 / D862 / OID-23 (constructs semantics-01: moved unchanged into the effects home, effects/fold.rs): whether a
/// group's aggregates are order-free (`order_free`), keep PostgreSQL's min / max member on any walk (`walk_free`), and
/// whether an opaque min / max is one image (`one_image_extremum`).
pub use crate::effects::fold::{one_image_extremum, order_free, walk_free};

/// Whether a group is keyed (its records live in a keyed table): hashed, distinct, two-pass or a grouping-sets arm.
pub fn keyed(g: &GroupDecl<'_>) -> bool {
    matches!(g.form, GroupForm::Hashed | GroupForm::Distinct | GroupForm::Pass1) || g.two_pass.is_some() || g.set.is_some()
}

/// Whether draining a group emits keyed groups (`keyed.emit_group`): a keyed group with a key or a grouping-sets arm, but
/// not the empty set's arm of a DISTINCT aggregate (D1263: keyed.distinct_plain's end and emit, one row as PG's plain agg).
pub fn emits_groups(g: &GroupDecl<'_>) -> bool {
    keyed(g) && (grouped(g) || (g.set.is_some() && g.form != GroupForm::Distinct))
}

/// Whether a group orders by group (a top-k over its finals is `runs.topk_groups`): every form but plain and Unique.
pub fn grouping(g: &GroupDecl<'_>) -> bool {
    !matches!(g.form, GroupForm::Plain | GroupForm::Unique) && (g.form != GroupForm::Distinct || grouped(g))
}

/// A write op's kind (decompose never builds a write op for `Cmd::Select`; it reads as DELETE's row-op end).
pub const fn write_kind(c: Cmd) -> KindId {
    match c {
        Cmd::Insert => KindId::WriteInsert,
        Cmd::Update => KindId::WriteUpdate,
        Cmd::Merge => KindId::WriteMerge,
        Cmd::Delete | Cmd::Select => KindId::WriteDelete,
    }
}

pub const fn join_kind(jt: JoinType) -> KindId {
    match jt {
        JoinType::JOIN_INNER | JoinType::JOIN_UNIQUE_OUTER | JoinType::JOIN_UNIQUE_INNER => KindId::JoinInner,
        JoinType::JOIN_LEFT => KindId::JoinLeft,
        JoinType::JOIN_SEMI => KindId::JoinSemi,
        JoinType::JOIN_ANTI => KindId::JoinAnti,
        JoinType::JOIN_RIGHT | JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT_ANTI => KindId::JoinRight,
        JoinType::JOIN_FULL => KindId::JoinFull,
    }
}

/// The kind of one op of a pipeline (its step, or for a write / lock / SRF op the kind that serves it).
pub fn step_kind(op: &Op<'_>) -> KindId {
    match op {
        Op::Filter(_) => KindId::StepFilter,
        Op::Gate(_) => KindId::StepGate,
        Op::Project(_) => KindId::StepProject,
        Op::Limit { .. } => KindId::StepLimit,
        // V4R-147 DISTINCT ON: runs.unique's step face (road run.pergroup); D1071 a streamed sorted group: runs.group's
        Op::FirstOfRun(_) => KindId::RunsUnique,
        Op::GroupRun(_) => KindId::RunsGroup,
        Op::Sort(d) => match d.form {
            SortForm::Runs if d.presorted > 0 => KindId::RunsIncremental,
            SortForm::Runs => KindId::RunsSort,
            SortForm::TopK { .. } | SortForm::Walk { .. } => KindId::RunsTopkRows,
        },
        Op::Join(JoinDesc::Build { .. }) => KindId::JoinInner,
        // psql \d's lookup nest as a catalog Probe (decompose `catalog_lookup`): nest.catalog, INNER or LEFT (V4R-142)
        Op::Join(JoinDesc::Probe { catalog: true, .. }) => KindId::NestCatalog,
        Op::Join(JoinDesc::Probe { jointype, .. }) => join_kind(*jointype),
        Op::Join(JoinDesc::Drain { .. }) => KindId::JoinDrain,
        Op::Join(JoinDesc::Merge { .. }) => KindId::MergeJoin,
        Op::Join(JoinDesc::Nest { .. }) => KindId::NestLoop,
        Op::Join(JoinDesc::Memo { .. }) => KindId::NestMemo,
        Op::Join(JoinDesc::Spool) => KindId::NestSpool,
        Op::Window(_) => KindId::WindowAgg,
        Op::SetOp(SetDesc::Iter(_)) => KindId::SetopRecursive,
        Op::SetOp(_) => KindId::SetopSetop,
        Op::SubPlan(_) => KindId::NestSubplan,
        Op::Append(AppendDesc::Merge { .. }) => KindId::RunsMergeappend,
        Op::Append(_) => KindId::SrcAppend,
        Op::Write(w) => write_kind(w.cmd),
        Op::Lock(_) => KindId::WriteLock,
        Op::Srf(s) => match s.place {
            SrfPlace::TargetList => KindId::SrfProjectset,
            // a FROM list of PL / SQL functions only is srf.pl's (props adds srf.pl beside a library function)
            SrfPlace::From { .. } if s.functions.iter().all(|f| gravity_decompose::effects::user_code(f.funcid)) => KindId::SrfPl,
            SrfPlace::From { .. } => KindId::SrfFunction,
        },
    }
}

/// An op's kind in a pipeline that drains a grouping group or not (`Gx::drains_grouping`): there a top-k sort orders
/// groups (`runs.topk_groups`), else `step_kind`.
fn op_need(op: &Op<'_>, groups: bool) -> KindId {
    match step_kind(op) {
        KindId::RunsTopkRows if groups => KindId::RunsTopkGroups,
        k => k,
    }
}

/// An op kind remapped by its pipeline: a MergeAppend Feed is the merge's (`merged`); by its `end`, a recursive union's
/// identity op the recursive union's, a Material's spool op filling a merge join's inner the merge's (`spool_kind`).
fn pipe_kind(kind: KindId, merged: bool, end: KindId) -> KindId {
    match kind {
        KindId::SrcAppend if merged => KindId::RunsMergeappend,
        KindId::SetopSetop if end == KindId::SetopRecursive => KindId::SetopRecursive,
        KindId::NestSpool if end == KindId::MergeJoin => KindId::MergeJoin,
        kind => kind,
    }
}

/// The graph, the facts and the catalog one lowering reads; nothing here holds a Postgres resource.
pub struct Gx<'g, 'p> {
    pub graph: &'g PipelineGraph<'p>,
    pub facts: &'g dyn Facts,
    pub catalog: &'g dyn Catalog<'p>,
    /// D1210: the groups re-declared before lowering (src/regroup.rs), read in place of the graph's.
    pub over: Option<&'g crate::ops::group::regroup::Overlay<'g, 'p>>,
    /// PLANMEM: the lowering's planning context (exec admit `PlanCtx`: the statement's plan owner), where its scratch and
    /// per-lowering values live; outside an admission, a context the caller owns.
    pub mcx: mcx::Mcx<'g>,
}

/// D3668: op `o` of pipeline `ops` is a SubPlan step only write `w`'s check options read (a WITH CHECK OPTION view's
/// qual, an RLS WITH CHECK): nest.subplan lends it host-fed and the host evaluates it per written row where
/// ExecWithCheckOptions reaches it (modifytable/rowop_sub.rs `fed`); no projection, filter, gate or RETURNING reads it.
pub(crate) fn check_sub(w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>, ops: &[Op<'_>], o: &Op<'_>) -> bool {
    use gravity_decompose::ops::shared::subplan::{refers as subplan_refers, refers_list as subplan_refers_list};
    let Op::SubPlan(s) = o else { return false };
    let id = s.plan_id;
    let read = |o: &Op<'_>| match o {
        Op::Filter(q) | Op::Gate(q) => subplan_refers(*q, id),
        Op::Project(l) => subplan_refers_list(l, id),
        _ => false,
    };
    w.checks.iter().any(|c| subplan_refers(c.qual, id)) && !ops.iter().any(read) && !w.returning.is_some_and(|r| subplan_refers_list(r, id))
}

/// D1112 (writesubplan): op `o` is a nested-form SubPlan step PostgreSQL may skip for a row (decompose `conditional`); its id.
/// Both callers ask that only MERGE actions read it; a MULTIEXPR (merge#340, an UPDATE action's `SET (b, c) = (SELECT ..)`
/// junk entry) is one too: write.merge reads it ahead of the SET entries that read its Params (merge/lower.rs `multi`), as
/// ExecBuildUpdateProjection runs each MULTIEXPR SubPlan of the action's list before any entry.
pub fn lazy_nested_sub(o: &Op<'_>) -> Option<i32> {
    match o {
        Op::SubPlan(d) if d.conditional && matches!(d.form, SubForm::Nested { .. }) => Some(d.plan_id),
        _ => None,
    }
}

/// An Append arm marker (V3-R1C): the op a streamed union's child continues past into the union's and the parent's ops
/// and end (a write of several result relations streams into its Append's arms, decompose append.rs `streamed_write`).
pub fn is_arm(o: &Op<'_>) -> bool {
    matches!(o, Op::Append(AppendDesc::Arm { .. }))
}

/// An Append arm marker's arm and arm count `(i, n)`; `None` for any other op (D2084: hashjoin spill/build.rs `arm_run`).
pub fn arm_at(o: &Op<'_>) -> Option<(u16, u16)> {
    if let Op::Append(AppendDesc::Arm { i, n, .. }) = o { Some((*i, *n)) } else { None }
}

/// Whether `o` is an Append op that holds no expression of its own: a feed, or an arm or union with no run-time pruning
/// (its arms' rows are made in their own pipelines). A structural fact, no effects question: mergecmd insert-only nest's
/// D2046 collation walk (nest_write.rs `loose`) has nothing to read in such an op.
pub fn append_without_exprs(o: &Op<'_>) -> bool {
    matches!(o, Op::Append(AppendDesc::Feed { .. } | AppendDesc::Arm { prune: None, .. } | AppendDesc::Union { prune: None, .. }))
}

impl<'g, 'p> Gx<'g, 'p> {
    pub fn pipelines(&self) -> &'g crate::pipes::Pipes<'p> {
        crate::pipes::Pipes::of(&self.graph.pipelines)
    }

    /// D652: pipeline `k`'s write reads another pipeline's product: another pipeline produces, or is no arm of k's Append.
    pub fn reads_product(&self, k: usize) -> bool {
        let ps = &self.graph.pipelines;
        let reads = |x: &PipelineNode<'_>| x.desc.produces().is_some() || !x.ops.iter().any(|o| matches!(o, Op::Append(AppendDesc::Arm { .. })));
        // D3681: in its lowering, the count of such pipelines is kept (memo.rs): each arm of a several-target write asks
        let key = crate::memo::key_of(ps);
        let kept = crate::memo::read(key, |m| m.products).map(|x| {
            x.unwrap_or_else(|| {
                let n = ps.iter().filter(|x| reads(x)).count();
                crate::memo::write(key, |m| m.products = Some(n));
                n
            })
        });
        match kept {
            Some(n) => n > usize::from(ps.get(k).is_some_and(reads)),
            None => ps.iter().enumerate().any(|(q, x)| q != k && reads(x)),
        }
    }

    /// D3553 (B) / D3776: pipeline `k` stacks `FILL_SHARE_MIN_PROBES` NULL-filling joins or more (LEFT nests, LEFT / FULL
    /// hash probes), each of which would lower the steps after it once more for its NULL-extended row (2^n copies). D3818: a
    /// statement that writes or locks rows shares too: the shared block keeps the inner row in reach of the EvalPlanQual
    /// redo with its NULL-extended flag as a run-time value (`Filled::At`), and the redo lowers its rest once past each
    /// LEFT join (epq_ir.rs `run_on`). Not a MERGE: its NOT MATCHED row is a LEFT join's NULL-extended one, and its
    /// MATCHED recheck reads the nest's outer row, which a fill does not hold (`FirstOuter::reaches`).
    /// D3810: counted over the whole lowered chain of `k`'s graph pipeline, its embedded inners at every depth included: a
    /// nest whose inner is itself a join (PG's bushy plan of stacked LEFT JOINs, Hibernate's eager fetch) runs the inner's
    /// fills inside the outer's continuation, so the copies multiply across the levels as down one pipeline.
    pub(crate) fn fills_share(&self, k: usize) -> bool {
        fn fills(p: &PipelineNode<'_>) -> usize {
            crate::stack_check();
            p.ops.iter().map(|op| match op {
                Op::Join(JoinDesc::Nest { jointype, inner, .. }) => usize::from(matches!(jointype, JoinType::JOIN_LEFT)) + fills(inner),
                Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_LEFT | JoinType::JOIN_FULL, .. }) => 1,
                Op::SubPlan(SubPlanDesc { form: SubForm::Nested { inner }, .. }) => fills(inner),
                _ => 0,
            }).sum()
        }
        let ps = self.pipelines();
        let mut root = k;
        while let Some(o) = ps.owner_of(root) {
            root = o;
        }
        fills(&ps[root]) >= gravity_program::cost::compile::FILL_SHARE_MIN_PROBES && !ps.iter().any(|p| p.ops.iter().any(|op| matches!(op, Op::Write(w) if w.cmd == Cmd::Merge)))
    }

    pub fn root_node(&self) -> i32 {
        self.graph.pipelines.get(self.graph.root as usize).map_or(0, |p| p.node_id)
    }

    pub fn out_of(&self, n: Node<'_>) -> Option<ExprOut> {
        self.graph.out_of(n)
    }

    /// The kind of pipeline `k`'s source: a breaker source is named by the kind of the life it drains.
    pub fn source_kind(&self, k: usize) -> KindId {
        let p = &self.pipelines()[k];
        match &p.source {
            // V4R-143 (D478): a pull-form MergeAppend arm reads nothing itself; its reader pulls the arm's rows (an index
            // arm's through its site, a Sort arm's run as a cursor)
            Source::Breaker { .. } | Source::Index { .. } if crate::props::Merge::of_arm(self, k).is_some_and(|m| crate::props::merge_pull(self, &m)) => KindId::RunsMergeappend,
            // D3789: a CteScan arm of a nest inner's streamed union reads nothing itself either (rerun/inner_arms.rs)
            Source::Breaker { .. } if crate::rerun::inner_arms::cte_arm(self, k) && crate::pipes::inner_union(self, k).is_some() => KindId::NestLoop,
            // D4620: a cascaded grouping set's producer reads its parents' owned records by claims (ops/agg/cascade.rs)
            Source::Breaker { .. } if crate::ops::agg::cascade::at(self, k).is_some() => KindId::KeyedGsets,
            Source::Breaker { slot } => self.drain_kind(k, *slot),
            // M8-NEST S1: a plan Material's fill its nest inner pulls reads nothing itself; the inner makes its scan again
            // D3498: or a Result's one row (rerun/spool/form.rs `result_child`)
            Source::Range { .. } | Source::Index { .. } | Source::Const if crate::pipes::spool_pull(self, k).is_ok() => KindId::NestSpool,
            _ if crate::pipes::inner_union(self, k).is_some() => KindId::NestLoop, // M8-NEST S2b: a nest inner's union streams this arm (nest/inner_arms.rs)
            _ if crate::rerun::union_arms::pulled_arm(self, k) => KindId::NestLoop, // G2(e) S1u: a Material's reader pulls this arm (inner_arms.rs `pulled`)
            s => source_kind_of(s, p.node_id),
        }
    }

    /// Draining a life is its producer's kind: a keyed group's emit, a run's merge, a spool's walk by its role
    /// (`spool_kind`); a hash build's unmatched rows are the join drain's.
    fn drain_kind(&self, k: usize, slot: u8) -> KindId {
        self.life_of(k, slot).map_or(KindId::NestSpool, |l| self.drain_life(l))
    }

    /// Draining `life` (`drain_kind`, and a nest inner's breaker source).
    fn drain_life(&self, life: LifeId) -> KindId {
        let Some(p) = self.graph.pipelines.get(life.0 as usize) else { return KindId::NestSpool };
        match breaker_of(&p.desc) {
            Some(b) if b.kind == BreakerKind::HashBuild => KindId::JoinDrain,
            Some(_) => match self.group_at(life.0 as usize) {
                Some(g) if emits_groups(g) => KindId::KeyedEmitGroup,
                _ => self.end_need(life.0 as usize),
            },
            None => EndKind::Sink.kind(),
        }
    }

    /// The group whose records pipeline `k` builds: its own, or, for an Append arm feeding one aggregate breaker with
    /// the arms after it, the group the last arm declares.
    pub fn group_at(&self, k: usize) -> Option<&'g GroupDecl<'p>> {
        let graph: &'g PipelineGraph<'p> = self.graph;
        let Some(b) = breaker_of(&self.pipelines()[k].desc) else { return None };
        // D3585: in its lowering, the graph's runs are walked once (an arm's walk to its run's end was quadratic in the arms)
        if let Some(next) = self.group_next() {
            return next.get(k).filter(|&&j| j != u32::MAX).and_then(|&j| self.group_of(LifeId(j as u16)));
        }
        let same = |j: &usize| breaker_of(&graph.pipelines[*j].desc).is_some_and(|c| c.slot == b.slot && c.kind == b.kind);
        (k..graph.pipelines.len()).take_while(same).find_map(|j| self.group_of(LifeId(j as u16)))
    }

    /// D3585: `group_at`'s answer for every graph pipeline, kept for the lowering of this graph under this overlay (memo.rs):
    /// the first pipeline from each one through the run of pipelines after it into the same breaker (slot and kind) that
    /// declares a group, `u32::MAX` for none. `None` outside the graph's lowering.
    fn group_next(&self) -> Option<std::rc::Rc<[u32]>> {
        let ps = &self.graph.pipelines;
        let key = crate::memo::key_of(ps);
        let over = self.over.map_or(0, |o| o as *const _ as usize);
        match crate::memo::read(key, |m| m.group_next.clone())? {
            Some((o, next)) if o == over => Some(next),
            _ => {
                let run = |j: usize| breaker_of(&ps[j].desc).map(|c| (c.slot, c.kind));
                let mut next = vec![u32::MAX; ps.len()]; // alloc: Program
                for j in (0..ps.len()).rev() {
                    let Some(r) = run(j) else { continue };
                    next[j] = if self.group_of(LifeId(j as u16)).is_some() {
                        j as u32
                    } else if j + 1 < ps.len() && run(j + 1) == Some(r) {
                        next[j + 1]
                    } else {
                        u32::MAX
                    };
                }
                let next: std::rc::Rc<[u32]> = next.into(); // alloc: Program
                crate::memo::write(key, |m| m.group_next = Some((over, next.clone())));
                Some(next)
            }
        }
    }

    /// The group `life` produces: the overlay's re-declaration (D1210), else the graph's.
    pub fn group_of(&self, life: LifeId) -> Option<&'g GroupDecl<'p>> {
        self.over.and_then(|o| o.group(life)).or_else(|| self.graph_group(life))
    }

    /// The graph's own group `life` produces (decompose `PipelineGraph::group_of`), from a table built once per lowering
    /// (memo.rs). D3703: a scan of every group per ask made the lowering of a chain of n nested EXCEPTs cubic (each level's
    /// key rep resolves through every level below it, `cell::key_cell_rep`): 2.7 s at 640 levels, where PostgreSQL 18.6
    /// answers in 40 ms.
    pub fn graph_group(&self, life: LifeId) -> Option<&'g GroupDecl<'p>> {
        let graph: &'g PipelineGraph<'p> = self.graph;
        let key = crate::memo::key_of(&graph.pipelines);
        let ix = match crate::memo::read(key, |m| m.group_ix.clone()) {
            None => return graph.group_of(life),
            Some(Some(ix)) => ix,
            Some(None) => {
                let mut ix = vec![u32::MAX; graph.pipelines.len()]; // alloc: Program
                for (i, g) in graph.groups.iter().enumerate() {
                    if let Some(e) = ix.get_mut(g.producer.0 as usize).filter(|e| **e == u32::MAX) {
                        *e = i as u32;
                    }
                }
                let ix: std::rc::Rc<[u32]> = ix.into(); // alloc: Program
                crate::memo::write(key, |m| m.group_ix = Some(ix.clone()));
                ix
            }
        };
        ix.get(life.0 as usize).filter(|&&i| i != u32::MAX).map(|&i| &graph.groups[i as usize])
    }

    /// Every breaker group, each as `group_of` reads it, then the streamed ones (D1071) as `stream` reads them.
    pub fn groups(&self) -> impl Iterator<Item = &'g GroupDecl<'p>> + '_ {
        self.graph.groups.iter().map(|g| self.group_of(g.producer).unwrap_or(g)).chain(self.streams())
    }

    /// The streamed group `Op::GroupRun(i)` runs (D1071): the overlay's re-declaration (G10), else the graph's.
    pub fn stream(&self, i: usize) -> Option<&'g GroupDecl<'p>> {
        let graph: &'g PipelineGraph<'p> = self.graph;
        self.over.and_then(|o| o.stream(i)).or_else(|| graph.streams.get(i))
    }

    /// Every streamed group by its `Op::GroupRun` index, each as `stream` reads it.
    pub fn streams(&self) -> impl Iterator<Item = &'g GroupDecl<'p>> + '_ {
        (0..self.graph.streams.len()).filter_map(|i| self.stream(i))
    }

    /// D3585: the pipelines (inners included) that drain group `g` (`drained_group`), in order. In a lowering the table is
    /// built once (memo.rs `drains`): a hashed group's every arm asked it, each walking every pipeline (a GROUP BY over
    /// 4000 partitions: 16 M `drained_group` calls in its lowering).
    pub fn drainers(&self, g: &GroupDecl<'_>) -> PgVec<'g, usize> {
        let a = (g as *const GroupDecl<'_>).cast::<u8>() as usize;
        let addr = |d: &GroupDecl<'_>| (d as *const GroupDecl<'_>).cast::<u8>() as usize;
        let walk = || (0..self.pipelines().len()).filter(|&q| self.drained_group(q).is_some_and(|d| addr(d) == a)).collect_in(self.mcx);
        let key = crate::memo::key_of(&self.graph.pipelines);
        let over = self.over.map_or(0, |o| o as *const _ as usize);
        let Some(kept) = crate::memo::read(key, |m| m.drains.clone()) else { return walk() };
        let table = match kept {
            Some((o, d)) if o == over => d,
            _ => {
                let mut d: Vec<(usize, u32)> = (0..self.pipelines().len()).filter_map(|q| self.drained_group(q).map(|g| (addr(g), q as u32))).collect(); // alloc: Program
                d.sort_unstable();
                let d: std::rc::Rc<[(usize, u32)]> = d.into(); // alloc: Program
                crate::memo::write(key, |m| m.drains = Some((over, d.clone())));
                d
            }
        };
        let lo = table.partition_point(|x| x.0 < a);
        table[lo..].iter().take_while(|x| x.0 == a).map(|x| x.1 as usize).collect_in(self.mcx)
    }

    /// D3585: whether a sorted set operation reads `life` as its right input (`SetDesc::Set { hashed: false, right }`); in
    /// a lowering the lives are gathered once (memo.rs `sorted_rights`): each hashed group's key asked it per arm.
    pub fn sorted_right(&self, life: LifeId) -> bool {
        let gather = || {
            let mut v: Vec<u16> = Vec::new(); // alloc: Program
            for (j, q) in self.pipelines().iter().enumerate() {
                for op in q.ops.iter() {
                    if let Op::SetOp(SetDesc::Set { hashed: false, right, .. }) = op {
                        if let Some(l) = self.life_of(j, *right) {
                            v.push(l.0);
                        }
                    }
                }
            }
            v.sort_unstable();
            v
        };
        let key = crate::memo::key_of(&self.graph.pipelines);
        let over = self.over.map_or(0, |o| o as *const _ as usize);
        let Some(kept) = crate::memo::read(key, |m| m.sorted_rights.clone()) else {
            return self.pipelines().iter().enumerate().any(|(j, q)| q.ops.iter().any(|op| matches!(op, Op::SetOp(SetDesc::Set { hashed: false, right, .. }) if self.life_of(j, *right).is_some_and(|l| l.0 == life.0))));
        };
        let lives = match kept {
            Some((o, v)) if o == over => v,
            _ => {
                let v: std::rc::Rc<[u16]> = gather().into(); // alloc: Program
                crate::memo::write(key, |m| m.sorted_rights = Some((over, v.clone())));
                v
            }
        };
        lives.binary_search(&life.0).is_ok()
    }

    /// D4620: whether group `g` is a grouping-set cascade's base (`PipelineGraph::cascades`): its cascaded sets read every
    /// group it folds, and its emit gathers theirs after its own.
    pub fn cascade_base(&self, g: &GroupDecl<'_>) -> bool {
        self.graph.cascades.iter().any(|c| c.base == g.producer)
    }

    /// The group pipeline `k` drains, when its source is a breaker holding a group's records.
    pub fn drained_group(&self, k: usize) -> Option<&'g GroupDecl<'p>> {
        let Source::Breaker { slot } = self.pipelines()[k].source else { return None };
        // D4620: a cascaded grouping set's producer folds its parent's records, it emits no group
        if !self.graph.cascades.is_empty() && crate::ops::agg::cascade::at(self, k).is_some() {
            return None;
        }
        self.life_of(k, slot).and_then(|l| self.group_at(l.0 as usize))
    }

    /// Whether pipeline `k` orders groups: it drains a grouping group (a top-k there is `runs.topk_groups`) and its sort
    /// reads the group's row through HAVING filters and projections alone (D1021): those the top resolves (topk_groups
    /// `sort_words`, order_slot `key_expr`); after any other op (a join, a window, an append, a set, a limit) its rows are
    /// not the group's, so its top orders rows by value (`runs.topk_rows`). So too when a projected column is an expression
    /// over a subquery scan's row (not a bare column of the projection before it): the drain projects the group's row once
    /// (topn `make_drain`), so that row is the top's only when every column resolves to it (A14 a).
    pub fn drains_grouping(&self, k: usize) -> bool {
        let ops = &self.pipelines()[k].ops;
        let sort = ops.iter().position(|op| matches!(op, Op::Sort(_))).unwrap_or(ops.len());
        self.drained_group(k).is_some_and(|g| grouping(g) && !crate::props::finals_once(self, g)) && ops[..sort].iter().all(|op| matches!(op, Op::Filter(_) | Op::Gate(_) | Op::Project(_))) && self.projects_group_row(k) // D1403
    }

    /// Whether every column of pipeline `k`'s nearest projection before its sort resolves to the drained group's row
    /// through the subquery scans' projections (order_slot `to_group_row`): the drain of a sort or top over the group's
    /// records projects that row once, so an expression over a subquery scan's row orders rows by value (A14 a; both
    /// `drains_grouping` and form G `group_sort` ask it).
    pub fn projects_group_row(&self, k: usize) -> bool {
        let ops = &self.pipelines()[k].ops;
        let sort = ops.iter().position(|op| matches!(op, Op::Sort(_))).unwrap_or(ops.len());
        let ls = crate::ops::sort::topn::order_slot::projections(&ops[..sort], self.mcx);
        ls.first().is_none_or(|l| l.iter().filter_map(|n| n.as_target_entry()).all(|te| crate::ops::sort::topn::order_slot::to_group_row(&ls[1..], te.expr).is_some()))
    }

    /// The kind that builds pipeline `k`'s end: an aggregate breaker's group's (`group_kind`), a Materialize's role's
    /// (`spool_kind`), a Sort a merge join reads the merge's, a top-k over drained groups `runs.topk_groups` (outside a rerun
    /// set), else `EndKind`'s.
    /// A write pipeline that ends in a breaker (RETURNING into a CTE's spool) needs its row op's kind as well.
    pub fn end_need(&self, k: usize) -> KindId {
        // M8 L0-g: a non-CTE InitPlan's pipeline ends in nest.initplan's cell end (its parameters), never the out sink
        if self.graph.init_plans.iter().any(|i| i.pipeline as usize == k && !matches!(i.link, SubLink::Cte)) {
            return KindId::NestInitplan;
        }
        let Some(b) = breaker_of(&self.pipelines()[k].desc) else { return self.end_kind(k).kind() };
        match b.kind {
            BreakerKind::Materialize => self.spool_kind(k),
            // D1339 row B: a member of a nest's or SubPlan's rerun set keeps its top's rows by value (`runs.topk_rows`, whose
            // run the owner's inner reads at its region, rerun/sets.rs `emit_ok`), as form G's full sort takes a row sort there
            BreakerKind::TopK if self.drains_grouping(k) && !self.top_reused(k) && !self.drained_twice(k) && !crate::rerun::sets::in_owner_set(self, k) => KindId::RunsTopkGroups,
            BreakerKind::Sort if self.merge_inner(k) => KindId::MergeJoin, // D1237: before `merged` (by slot: a reused slot's MergeAppend)
            BreakerKind::Sort if self.merged(k) => KindId::RunsMergeappend,
            kind => match self.group_at(k) {
                Some(g) => group_kind(g),
                None if kind == BreakerKind::Distinct => self.set_kind(k),
                None => breaker_kind(kind),
            },
        }
    }

    /// D295 (M3 L0-g): pipeline `k`, a top over drained groups, has its group's slot or its own slot produced again by a
    /// pipeline up to the top's reader, so the group's records cannot stay live through the drain (runs/topk_groups.rs
    /// `make_drain`'s `again`): the top keeps its rows by value, `runs.topk_rows` for its end and its drain.
    pub fn top_reused(&self, k: usize) -> bool {
        let ps = &self.graph.pipelines;
        let (Some(br), &Source::Breaker { slot }) = (breaker_of(&ps[k].desc), &ps[k].source) else { return false };
        let Some(r) = (k + 1..ps.len()).find(|&j| matches!(ps[j].source, Source::Breaker { slot: s } if s == br.slot)) else { return false };
        let group = self.life_of(k, slot).map_or(k, |l| l.0 as usize);
        let again = |s: u8, from: usize| (from + 1..=r).any(|m| ps[m].desc.produces() == Some(s));
        again(slot, group) || again(br.slot, k)
    }

    /// Whether the group pipeline `k` drains is drained by another pipeline too (a CTE scanned twice, each scan sorted or
    /// cut its own way). The group keeps ONE emit state (kernels table/emit.rs `EmitState`, in its owner 0's table space),
    /// which each drain's fresh gather refiles with its own pointer array; a top over the groups or form G's sort orders
    /// that array in place and its drain re-reads the state in a later pipeline, by when the other drain may have refiled
    /// it (a later scan's gather, or a merge's inner run lazily at its first outer row), so the drain would hand on the
    /// other's records in the other's order. Such a top or sort orders the rows by value (`runs.topk_rows`, `runs.sort`).
    pub fn drained_twice(&self, k: usize) -> bool {
        let Some(g) = self.drained_group(k) else { return false };
        self.drainers(g).iter().any(|&m| m != k)
    }

    /// Whether pipeline `k`'s runs are a MergeAppend's: the first later pipeline reading its slot merges it (`append
    /// merge`). Every child feeds the one slot, so the reader is found by slot, not by the life it names.
    pub fn merged(&self, k: usize) -> bool {
        let Some(b) = breaker_of(&self.pipelines()[k].desc) else { return false };
        let merges = |q: &PipelineNode<'p>| q.ops.iter().any(|op| matches!(op, Op::Append(AppendDesc::Merge { slot, .. }) if *slot == b.slot));
        let reads = |q: &PipelineNode<'p>| merges(q) || matches!(q.source, Source::Breaker { slot } if slot == b.slot);
        match self.later_reader(b.slot, k) {
            Some(Some(merging)) => return merging,
            Some(None) => {}
            None => {
                if let Some(q) = self.graph.pipelines.iter().skip(k + 1).find(|q| reads(q)) {
                    return merges(q);
                }
            }
        }
        // D1483: a SubPlan's or nest inner's MergeAppend (regress inherit#759): no later graph pipeline reads the slot; the
        // first inner a later pipeline embeds that reads it merges it
        let ins = self.pipelines().inners();
        let root = |mut o: usize| loop {
            match o.checked_sub(self.graph.pipelines.len()) {
                Some(i) => o = ins[i].0 .0,
                None => break o,
            }
        };
        ins.iter().find(|x| root(x.0 .0) > k && reads(x.1)).is_some_and(|x| merges(x.1))
    }

    /// D3585: in the graph's lowering (memo.rs), whether the first graph pipeline after `k` that reads slot `slot` (a
    /// MergeAppend's op over it, or its breaker as the source) merges it: `Some(None)` when none reads it, `None` outside
    /// the lowering. Every pipeline's reads are listed once, by slot then pipeline.
    fn later_reader(&self, slot: u8, k: usize) -> Option<Option<bool>> {
        let ps = &self.graph.pipelines;
        let key = crate::memo::key_of(ps);
        let readers = match crate::memo::read(key, |m| m.readers.clone())? {
            Some(r) => r,
            None => {
                let mut r: Vec<(u8, u32, bool)> = Vec::new(); // alloc: Program
                for (q, p) in ps.iter().enumerate() {
                    let at = r.len();
                    for op in p.ops.iter() {
                        if let Op::Append(AppendDesc::Merge { slot, .. }) = op {
                            if !r[at..].iter().any(|x| x.0 == *slot) {
                                r.push((*slot, q as u32, true)); // alloc: Program
                            }
                        }
                    }
                    if let Source::Breaker { slot } = p.source {
                        if !r[at..].iter().any(|x| x.0 == slot) {
                            r.push((slot, q as u32, false)); // alloc: Program
                        }
                    }
                }
                r.sort_unstable_by_key(|x| (x.0, x.1));
                let r: std::rc::Rc<[(u8, u32, bool)]> = r.into(); // alloc: Program
                crate::memo::write(key, |m| m.readers = Some(r.clone()));
                r
            }
        };
        let i = readers.partition_point(|x| (x.0, x.1 as usize) <= (slot, k));
        Some(readers.get(i).filter(|x| x.0 == slot).map(|x| x.2))
    }

    /// A Distinct breaker no group declares: a set operation's input identity set (`setop.setop`) or a hashed SubPlan's
    /// set (`nest.subplan`); else a plain distinct.
    fn set_kind(&self, k: usize) -> KindId {
        let ops = &self.pipelines()[k].ops;
        if let Some(r) = self.set_role(k) {
            r
        } else if ops.iter().any(|op| matches!(op, Op::SetOp(SetDesc::Identity { .. }))) {
            KindId::SetopSetop
        } else if ops.iter().any(|op| matches!(op, Op::SubPlan(_))) {
            KindId::NestSubplan
        } else {
            KindId::KeyedDistinctPlain
        }
    }

    /// A set operation's input (a recursive union's work or next table: `setop.recursive`; a SetOp's left or
    /// right input: `setop.setop`), when life `k` is one.
    fn set_role(&self, k: usize) -> Option<KindId> {
        let life = Some(LifeId(k as u16));
        // D3585: in its lowering, a graph with no set operation answers at once (memo.rs)
        let key = crate::memo::key_of(&self.graph.pipelines);
        let setops = crate::memo::read(key, |m| m.setops).map(|x| {
            x.unwrap_or_else(|| {
                let any = self.graph.pipelines.iter().any(|q| q.ops.iter().any(|op| matches!(op, Op::SetOp(_))));
                crate::memo::write(key, |m| m.setops = Some(any));
                any
            })
        });
        if setops == Some(false) {
            return None;
        }
        self.graph.pipelines.iter().enumerate().find_map(|(j, q)| {
            q.ops.iter().find_map(|op| match op {
                Op::SetOp(SetDesc::Iter(d)) if [d.work, d.next].into_iter().any(|s| self.life_of(j, s) == life) => Some(KindId::SetopRecursive),
                // with#170 / #171: an earlier arm of a UNION ALL term filling the intermediate table (`life_of` names the last)
                Op::SetOp(SetDesc::Iter(_)) if self.term_arm(k) == Some(j) => Some(KindId::SetopRecursive),
                Op::SetOp(SetDesc::Set { left, right, .. }) if self.life_of(j, *left) == life || self.life_of(j, *right) == life => Some(KindId::SetopSetop),
                _ => None,
            })
        })
    }

    /// A Materialize's kind by its role, first match: a CTE's spool (an InitPlan's `Cte` link fills it) `nest.cte`; a
    /// set operation's input (`set_role`); the union collect its producers' `Feed` ops fill `src.append`; a merge join's
    /// inner rows `merge.join`; a window's partition buffer (a window op drains it) `window.agg`; the matches set a hash
    /// join's `Drain` reads `join.drain`; any other (a plan Material) `nest.spool`.
    /// Pipeline `k`'s life is a merge join's inner: a Merge op reads its slot.
    /// rangefuncs#223 (D1109): a merge in a nest's embedded inner too (ops/mergejoin/nest_refill.rs).
    /// D2975: an earlier arm of a union whose slot a merge reads (a partitioned table mixing heap and pgrcolumnar leaves,
    /// a UNION ALL: `life_of` names the last arm) fills the same run, so it takes the merge's end too: serial, a full run
    /// grown resident, never put (as a parallel runs.sort end it put a run at W8, which the merge's view refuses).
    fn merge_inner(&self, k: usize) -> bool {
        let ps = self.pipelines();
        // D3585: in the graph's lowering the merge joins' lives are read once (memo.rs): a walk of every pipeline for `k`
        // and each arm of its run was cubic in an Append's arms
        let key = crate::memo::key_of(&self.graph.pipelines);
        if let Some(kept) = crate::memo::read(key, |m| m.merge_lives.clone()) {
            let lives = kept.unwrap_or_else(|| {
                let mut l: Vec<u16> = (0..ps.len() + ps.inners().len()) // alloc: Program
                    .flat_map(|j| ps[j].ops.iter().filter_map(move |op| match op {
                        Op::Join(JoinDesc::Merge { slot, .. }) => self.life_of(j, *slot).map(|l| l.0),
                        _ => None,
                    }))
                    .collect();
                l.sort_unstable();
                l.dedup();
                let l: std::rc::Rc<[u16]> = l.into(); // alloc: Program
                crate::memo::write(key, |m| m.merge_lives = Some(l.clone()));
                l
            });
            let read = |q: usize| lives.binary_search(&(q as u16)).is_ok();
            return !lives.is_empty() && (read(k) || (k < ps.len() && self.run_arms(k).into_iter().any(read)));
        }
        let read = |q: usize| (0..ps.len() + ps.inners().len()).any(|j| ps[j].ops.iter().any(|op| matches!(op, Op::Join(JoinDesc::Merge { slot, .. }) if self.life_of(j, *slot) == Some(LifeId(q as u16)))));
        read(k) || (k < ps.len() && self.run_arms(k).into_iter().any(read))
    }

    pub fn spool_kind(&self, k: usize) -> KindId {
        let life = Some(LifeId(k as u16));
        // D3054: a grouping-set input spool (ops/agg/gsets_spool.rs)
        if crate::ops::agg::gsets_spool::fills(self, k) {
            return KindId::KeyedGsets;
        }
        if self.graph.init_plans.iter().any(|i| i.pipeline as usize == k && matches!(i.link, SubLink::Cte)) {
            return KindId::NestCte;
        }
        if let Some(r) = self.set_role(k) {
            return r;
        }
        if self.pipelines()[k].ops.iter().any(|op| matches!(op, Op::Append(AppendDesc::Feed { .. }))) {
            return KindId::SrcAppend;
        }
        if self.merge_inner(k) {
            return KindId::MergeJoin;
        }
        // a window reader that is a nest's or a nested SubPlan's inner too (material W-INNER: the WindowAgg PG rescans per
        // outer row or call, its fill a member of that owner's rerun set; rerun/inner.rs, rerun/sets.rs `emit_ok`)
        let windows = |j: usize, q: &PipelineNode<'p>| matches!(q.source, Source::Breaker { slot } if self.life_of(j, slot) == life) && q.ops.iter().any(|op| matches!(op, Op::Window(_)));
        let ps = self.pipelines();
        if (0..ps.len() + ps.inners().len()).any(|j| windows(j, &ps[j])) {
            return KindId::WindowAgg;
        }
        let drains = |j: usize, q: &PipelineNode<'p>| q.ops.iter().any(|op| matches!(op, Op::Join(JoinDesc::Drain { matches, .. }) if self.life_of(j, *matches) == life));
        if (0..ps.len() + ps.inners().len()).any(|j| drains(j, &ps[j])) || self.drained_arms(k).is_some() {
            return KindId::JoinDrain;
        }
        KindId::NestSpool
    }

    /// Pipeline `k`'s end: a row op when the pipeline writes, else its declared `End`. A data-modifying CTE's write with
    /// RETURNING (`cte_write`) writes in its step and ends in its declared end, the CTE's store (`nest.cte`), not the client.
    pub fn end_kind(&self, k: usize) -> EndKind {
        let p = &self.pipelines()[k];
        match p.ops.iter().find_map(|op| if let Op::Write(w) = op { Some((w.cmd, w.returning.is_some())) } else { None }) {
            Some((c, ret)) if c != Cmd::Select && !(ret && self.cte_write(k)) => EndKind::RowOp(c),
            _ => EndKind::of_desc(&p.desc),
        }
    }

    /// Pipeline `k` is a CTE's producer (an InitPlan's `Cte` link fills its spool): a write there is a data-modifying
    /// CTE's (its ModifyTable's `canSetTag` false), whose tag and row count are the main statement's.
    pub fn cte_write(&self, k: usize) -> bool {
        self.graph.init_plans.iter().any(|i| i.pipeline as usize == k && matches!(i.link, SubLink::Cte))
    }

    /// Pipeline `k` runs in a recursive union's generation (a member of its `Iter`'s re-run set) and reads slot `s`, the
    /// union's working table: the table is settled when the generation reads it, since PG's RecursiveUnion runs the
    /// non-recursive term to its end before the first generation and each generation to its end before the next
    /// (nodeRecursiveunion.c), so nothing that filled it runs, raises or calls for that read (join/build_class.rs `feed`
    /// stops there). Read from outside the generation (the driver, the union's reader) the table is fed as before.
    /// with#170 / #171: the driver of the recursive union whose UNION ALL term pipeline `k` is an arm of, filling the
    /// intermediate table (`next`): a member of the term's set, before the driver, ending in `next` (`life_of` names only
    /// the last arm). The arms run in the set's order, each appending to the one table, as PG's Append returns its arms'
    /// rows in order.
    pub fn term_arm(&self, k: usize) -> Option<usize> {
        let ps = &self.graph.pipelines;
        // D3585: in its lowering, a graph with no recursive union's iteration answers at once (memo.rs)
        let key = crate::memo::key_of(ps);
        let iters = crate::memo::read(key, |m| m.iters).map(|x| {
            x.unwrap_or_else(|| {
                let any = ps.iter().any(|q| q.ops.iter().any(|op| matches!(op, Op::SetOp(SetDesc::Iter(_)))));
                crate::memo::write(key, |m| m.iters = Some(any));
                any
            })
        });
        if iters == Some(false) {
            return None;
        }
        let member = |d: &gravity_decompose::ops::shared::setop::IterDesc| self.graph.reruns.iter().any(|r| r.plan_id == gravity_decompose::ops::shared::setop::RECURSIVE_SET && r.pipelines.contains(&d.body) && r.pipelines.contains(&(k as u16)));
        (k + 1..ps.len()).find(|&q| ps[q].ops.iter().any(|op| matches!(op, Op::SetOp(SetDesc::Iter(d)) if ps[k].desc.produces() == Some(d.next) && member(d))))
    }

    pub fn settled_work(&self, k: usize, s: u8) -> bool {
        self.pipelines().iter().any(|p| p.ops.iter().any(|op| matches!(op, Op::SetOp(SetDesc::Iter(d)) if d.work == s && d.reruns.iter().any(|&r| r as usize == k))))
    }

    /// Every kind pipeline `k`'s descriptors decide (source, each op, end, the nest inner's own pipeline), each with the
    /// plan node that needs it first, in pipeline order.
    pub fn pipeline_kinds(&self, k: usize, out: &mut PgVec<'_, (KindId, i32)>) {
        let p = &self.pipelines()[k];
        push_kind_in(out, self.source_kind(k), p.node_id);
        if !matches!(p.source, Source::Breaker { .. }) && matches!(self.source_kind(k), KindId::NestSpool | KindId::NestLoop) {
            push_kind_in(out, source_kind_of(&p.source, p.node_id), p.node_id); // M8-NEST S1: the inner makes the scan again
        }
        // a Feed into a MergeAppend's runs is the merge's; an identity op filling a recursive union's dedup table is the
        // recursive union's; a Material's spool op filling a merge join's inner is the merge's
        let (merged, end) = (self.merged(k), self.end_need(k));
        let mut ks = PgVec::new_in(self.mcx);
        self.op_kinds(p, self.drains_grouping(k), &mut ks);
        for (kind, node) in ks {
            push_kind_in(out, pipe_kind(kind, merged, end), node);
        }
        if let EndKind::RowOp(c) = self.end_kind(k) {
            push_kind_in(out, write_kind(c), p.node_id);
        }
        push_kind_in(out, self.end_need(k), p.node_id);
    }

    /// The kind whose `StepMaker` builds op `j` of pipeline `k` in `frame::resolve`: the kind `pipeline_kinds` admits
    /// for that op (`op_need`, then `pipe_kind`), so admission and resolve cannot pick different makers.
    pub fn step_need(&self, k: usize, j: usize) -> KindId {
        let kind = op_need(&self.pipelines()[k].ops[j], self.drains_grouping(k));
        pipe_kind(kind, self.merged(k), self.end_need(k))
    }

    /// The kind whose `EndMaker` builds pipeline `k`'s end in `frame::resolve`: a write pipeline's row op (H27, whatever
    /// `End` it declares), else `end_need`, the kind `pipeline_kinds` admits for the end.
    pub fn end_maker(&self, k: usize) -> KindId {
        match self.end_kind(k) {
            EndKind::RowOp(c) => write_kind(c),
            _ => self.end_need(k),
        }
    }

    /// Each op's kind (a top-k sort over drained groups is `runs.topk_groups`), a nest inner's source and ops included.
    fn op_kinds(&self, p: &PipelineNode<'p>, groups: bool, out: &mut PgVec<'_, (KindId, i32)>) {
        crate::stack_check(); // D3832
        for (i, op) in p.ops.iter().enumerate() {
            let node = p.node_ops.iter().rev().find(|n| n.first_op as usize <= i).map_or(p.node_id, |n| n.node_id);
            push_kind_in(out, op_need(op, groups), node);
            if let Op::Sort(d) = op {
                if d.presorted > 0 && !matches!(d.form, SortForm::Runs) {
                    push_kind_in(out, KindId::RunsIncremental, node);
                }
            }
            if let Op::Join(JoinDesc::Nest { inner, inner_node, .. }) = op {
                push_kind_in(out, self.inner_source_kind(inner, *inner_node), *inner_node);
                self.op_kinds(inner, false, out);
            }
            if let Op::SubPlan(SubPlanDesc { form: SubForm::Nested { inner }, .. }) = op {
                push_kind_in(out, self.inner_source_kind(inner, inner.node_id), inner.node_id);
                self.op_kinds(inner, false, out);
            }
        }
    }

    /// A nest inner's source kind: a breaker source is the drain of the life its descriptor reads (a CTE's spool, a
    /// Material's), else the source's own kind.
    fn inner_source_kind(&self, inner: &PipelineNode<'p>, node: i32) -> KindId {
        let Source::Breaker { slot } = inner.source else { return source_kind_of(&inner.source, node) };
        let d = &inner.desc;
        let life = d.consumes.iter().position(|&s| s == slot).and_then(|j| d.consumes_from.get(j).copied());
        life.map_or(KindId::NestSpool, |l| self.drain_life(l))
    }

    /// Every kind the graph's descriptors decide, each with the first node that needs it.
    pub fn kinds(&self) -> PgVec<'g, (KindId, i32)> {
        let mut out = PgVec::new_in(self.mcx);
        for k in 0..self.graph.pipelines.len() {
            self.pipeline_kinds(k, &mut out);
        }
        out
    }

    /// The life pipeline `k` reads through `slot`: its declared `consumes_from`, else the nearest earlier producer.
    pub fn life_of(&self, k: usize, slot: u8) -> Option<LifeId> {
        let d = &self.pipelines()[k].desc;
        if let Some(j) = d.consumes.iter().position(|&s| s == slot) {
            if let Some(&l) = d.consumes_from.get(j) {
                return Some(l);
            }
        }
        (0..k.min(self.graph.pipelines.len())).rev().find(|&i| breaker_of(&self.graph.pipelines[i].desc).is_some_and(|b| b.slot == slot)).map(|i| LifeId(i as u16))
    }

    /// Pipelines `out` and, transitively, the producers of the slots they read, every arm of a union counted (D4153):
    /// `life_of` names one producer per slot, a streamed union's last arm (its other arms write the same slot: `run_arms`)
    /// or a settled union's last feed (its emit reads every feed: `union_feeds`), so each member's arms join the set too.
    pub fn with_producers(&self, seed: impl IntoIterator<Item = usize>) -> PgVec<'g, usize> {
        let mut out: PgVec<'g, usize> = PgVec::new_in(self.mcx);
        out.extend_in(seed);
        let mut i = 0;
        while let Some(&q) = out.get(i) {
            i += 1;
            let lives = self.pipelines()[q].desc.consumes.iter().filter_map(|&s| self.life_of(q, s)).map(|l| l.0 as usize);
            for l in lives.chain(self.run_arms(q)).chain(self.union_feeds(q)) {
                if !out.contains(&l) {
                    out.push_in(l);
                }
            }
        }
        out
    }

    /// D2115: the other arms of the streamed union whose arm is pipeline `x`. decompose's append.rs `push_union` gives each
    /// arm an `Arm` op and the parent's ops and end, so every arm writes one production of the end's slot, whose life is its
    /// last arm's (`life_of`): read alone, a zero LIMIT's feeder (result/gate.rs `feeds_alone`, through join/build_class.rs
    /// `feed`) stopped the last arm while the others (a grouping-sets spool run under them, UNION ALL arms into one
    /// aggregate) ran on, where PG's Limit pulls none. The run: the pipelines beside `x` producing its slot, each holding an
    /// `Arm` of the same arm count (the last `Arm` op: the outer union's), back to arm 0 and on to the last; none when `x`
    /// is no arm.
    pub fn run_arms(&self, x: usize) -> PgVec<'g, usize> {
        let ps = self.pipelines();
        let arm = |p: usize| ps[p].ops.iter().rev().find_map(|op| if let Op::Append(AppendDesc::Arm { i, n, .. }) = op { Some((*i, *n)) } else { None });
        let (Some((i, n)), Some(s)) = (arm(x), ps[x].desc.produces()) else { return PgVec::new_in(self.mcx) };
        let of = |p: usize| arm(p).filter(|a| a.1 == n && ps[p].desc.produces() == Some(s) && !ps[p].desc.consumes.contains(&s)).map(|a| a.0);
        let mut out = PgVec::new_in(self.mcx);
        let (mut p, mut at) = (x, i);
        while at > 0 && p > 0 {
            match of(p - 1) {
                Some(j) if j <= at => (p, at) = (p - 1, j),
                _ => break,
            }
            out.push_in(p);
        }
        (p, at) = (x, i);
        while at + 1 < n && p + 1 < ps.len() {
            match of(p + 1) {
                Some(j) if j >= at => (p, at) = (p + 1, j),
                _ => break,
            }
            out.push_in(p);
        }
        out
    }

    /// D2115 (K18.x09): the other arms of the streamed union whose arm is pipeline `x`, whatever their end: the pipelines
    /// holding an `Arm` of the same arm count (the last `Arm` op: the outer union's) whose plan nodes from that marker on
    /// are `x`'s (decompose append.rs `push_union` gives every arm the Append's entry at its marker and the parent's nodes
    /// at their copied ops, so the nodes name the union). `run_arms` ties a run into one slot by that slot; a run into the
    /// statement's sink (grouping sets with no Sort above, a UNION ALL under a bare LIMIT) has none. None when `x` is no arm.
    pub fn stream_arms(&self, x: usize) -> PgVec<'g, usize> {
        let ps = self.pipelines();
        let at = |p: usize| ps[p].ops.iter().rposition(is_arm);
        let n_of = |p: usize, j: usize| if let Op::Append(AppendDesc::Arm { n, .. }) = &ps[p].ops[j] { *n } else { 0 };
        let tail = |p: usize, j: usize| ps[p].node_ops.iter().filter(move |o| o.first_op as usize >= j).map(|o| o.node_id);
        let Some(jx) = at(x) else { return PgVec::new_in(self.mcx) };
        let n = n_of(x, jx);
        (0..ps.len()).filter(|&p| p != x && at(p).is_some_and(|j| n_of(p, j) == n && tail(p, j).eq(tail(x, jx)))).collect_in(self.mcx)
    }

    /// Pipeline `k`'s leases (S6): the life it produces and every life it reads.
    pub fn leases(&self, k: usize) -> Leases {
        let d = &self.pipelines()[k].desc;
        let produces = breaker_of(d).is_some().then_some(LifeId(k as u16));
        let reads = d.consumes.iter().filter_map(|&s| self.life_of(k, s)).collect(); // alloc: Program
        Leases { produces, reads }
    }
}

/// A source's kind from its descriptor alone (a breaker source, which only `Gx::source_kind` can name, reads as a spool).
/// A scan over a table AM that is neither heap nor pgrc refuses `UnsupportedAm` at `node`: the heap source reads heap
/// pages, so no kind serves another AM's format.
pub fn source_kind_of(s: &Source<'_>, node: i32) -> KindId {
    if let Source::Range { am: Am::Other, relid, .. } | Source::Tid { am: Am::Other, relid, .. } | Source::Sample { am: Am::Other, relid, .. } = s {
        crate::refuse!(node, crate::admit::refusal::Reason::UnsupportedAm { relid: *relid });
    }
    match s {
        Source::Range { am: Am::Pgrcolumnar, .. } => KindId::SrcPgrc,
        Source::Range { .. } => KindId::SrcHeap,
        // L0-h: a btree index-only walk is src.index_only's (a constant `= ANY` key too); a btree walk with an `= ANY` key
        // is the array-key source's, one with an AM operator or an ORDER BY operator the any-AM source's, as is every walk
        // of another AM, index-only or not (source/index.rs and source/index_only.rs keep their guards as bugs)
        Source::Index { only: true, iam: IndexAm::Btree, .. } => KindId::SrcIndexOnly,
        Source::Index { iam: IndexAm::Btree, keys, .. } if keys.iter().any(|k| matches!(k.form, KeyForm::Any | KeyForm::AmAny)) => KindId::SrcIndexArray,
        Source::Index { iam: IndexAm::Btree, keys, .. } if keys.iter().any(|k| matches!(k.form, KeyForm::AmOp | KeyForm::OrderBy)) => KindId::SrcIndexAm,
        Source::Index { iam: IndexAm::Btree, .. } => KindId::SrcIndex,
        Source::Index { .. } => KindId::SrcIndexAm,
        Source::Bitmap { .. } => KindId::SrcBitmap,
        Source::Tid { .. } => KindId::SrcTid,
        Source::Sample { .. } => KindId::SrcSample,
        Source::Const => KindId::SrcResult,
        Source::Values { .. } => KindId::SrcValues,
        // V4R-213 (main 09-25): a transition-table scan (a trigger body's `SELECT .. FROM new_table`) reads the AFTER-trigger
        // query's named tuplestore: srf.function's tuplestore source (materialize mode), not write.transition's trigger plan
        Source::Named { .. } => KindId::SrfFunction,
        Source::TableFunc { .. } => KindId::SrfTablefunc,
        Source::Breaker { .. } => KindId::NestSpool,
    }
}

fn push_kind_in(out: &mut PgVec<'_, (KindId, i32)>, k: KindId, node: i32) {
    if !out.iter().any(|(j, _)| *j == k) {
        out.push_in((k, node));
    }
}

/// The set of a kind list.
pub fn kind_set(ks: &[(KindId, i32)]) -> KindSet {
    let mut s = KindSet::EMPTY;
    for (k, _) in ks {
        s.add(*k);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admit::refusal::{Reason, Refusal};
    use gravity_program::{ArenaRef, SiteDesc};

    fn range(am: Am) -> Source<'static> {
        Source::Range { site: SiteDesc::none_in(ArenaRef::EMPTY), am, scanrelid: 1, relid: 16500 }
    }

    /// A scan over a table AM that is neither heap nor pgrc refuses by name, never reads as a heap scan.
    #[test]
    fn other_am_refuses() {
        assert_eq!(crate::catch(7, || source_kind_of(&range(Am::Other), 7)), Err(Refusal { reason: Reason::UnsupportedAm { relid: 16500 }, node: 7, native_rel: 0 }));
        assert_eq!(crate::catch(7, || source_kind_of(&range(Am::Heap), 7)), Ok(KindId::SrcHeap));
        assert_eq!(crate::catch(7, || source_kind_of(&range(Am::Pgrcolumnar), 7)), Ok(KindId::SrcPgrc));
        let seam = Refusal { reason: Reason::UnsupportedAm { relid: 16500 }, node: 7, native_rel: 0 }.to_seam();
        assert_eq!((seam.what, seam.detail, seam.oid), ("UnsupportedAm", "relation", 16500));
    }

    /// A Material's spool op is merge.join's only in a pipeline whose end is a merge join's inner (MERGE-1, main 17:59Z);
    /// under any other end it stays nest.spool's, and the recursive union's and MergeAppend's arms are unchanged.
    #[test]
    fn a_spool_op_is_merge_joins_only_under_a_merge_inner() {
        assert_eq!(pipe_kind(KindId::NestSpool, false, KindId::MergeJoin), KindId::MergeJoin);
        for end in [KindId::NestSpool, KindId::RunsSort, KindId::SetopRecursive, KindId::JoinDrain, KindId::WindowAgg, KindId::NestCte] {
            assert_eq!(pipe_kind(KindId::NestSpool, false, end), KindId::NestSpool, "{end:?}");
        }
        assert_eq!(pipe_kind(KindId::SetopSetop, false, KindId::SetopRecursive), KindId::SetopRecursive);
        for end in [KindId::MergeJoin, KindId::NestSpool, KindId::SetopSetop] {
            assert_eq!(pipe_kind(KindId::SetopSetop, false, end), KindId::SetopSetop, "{end:?}");
        }
        assert_eq!(pipe_kind(KindId::SrcAppend, true, KindId::MergeJoin), KindId::RunsMergeappend);
        assert_eq!(pipe_kind(KindId::SrcAppend, false, KindId::MergeJoin), KindId::SrcAppend);
        assert_eq!(pipe_kind(KindId::StepFilter, false, KindId::MergeJoin), KindId::StepFilter);
    }
}
