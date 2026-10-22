//! A union a nest (or nested SubPlan) inner embeds, streamed as PostgreSQL's Append (M8-NEST S2b, D1483; a helper of
//! nest/union_arms.rs, which hands this form's feed ends and emit source here). PG re-opens the Append at every rescan and
//! pulls arm 0 to its end, then arm 1, each arm opened there, so an arm never reached (a SEMI / ANTI match, a LIMIT met, an
//! empty outer) never runs and a VOLATILE call in an arm runs as often as PG's. The inner's source here (`Arms`) makes every
//! arm's source and ops again inside the inner, S1's pull form per arm (nest/spool.rs), each over its own site numbered in
//! arm order after the owner's, which nest/inner.rs holds ahead as for any inner: a site a step after the Union makes (a
//! nested nest's or SubPlan's inner) follows the owner's, as exec admit's walk does, and an arm's pairs in any order
//! (`lone_range`). The feed pipelines read nothing (graph.rs `source_kind` names their source nest.loop: `Empty`) and leave
//! at their open (`Fused`). A window is one row. The per-open words, written at every re-open, say arm 0,
//! unopened, position = count = 0. At the position the count, the current arm reads on (opened, its per-open words written,
//! at its first read) until one row passes its ops: that row's union columns go to a one-row buffer in frame words, and the
//! count is bumped; an arm's end opens the next, the last one's ends the union. A by-reference datum whose bytes outlive the
//! arm's next read (a heap page's, a constant's) is borrowed; any other (a computed or set-function value) is copied into
//! the column's SubResult slot (ops/subplan/result.rs `keep`, PG's Append slot copy), valid until the column's next copy,
//! which is the arm's next row. An arm with a
//! ProjectSet leaves inside its row (its cursor at the row, the step's offset word set) and re-enters it at its next read,
//! where srf/project_set.rs resumes (its held words keep the row's volatile values). A union PG prunes at run time by the
//! nest's parameters over an arm or a pruning call that is not quiet (all quiet: every arm runs) prunes as
//! PG's at the first read after each re-open (`nest_prune`, `m8_prune`: exec ops/append/prune.rs `NestPrune`, D2013).
//! D3789: an arm may read a CTE's store (a CteScan arm, `cte_arm`) and probe a hashed SubPlan's set, each filled once
//! outside every rerun set: PG's ExecReScanCteScan rewinds the arm's read pointer over the same store and pulls the CTE
//! only past it, as the CteScan reader here does (ops/ctescan/lower.rs), and the hashed set is never rebuilt.
//! D4062: an arm's filter may hold a correlated SubPlan, which PG runs per row in the arm's qual (ExecScanSubPlan): its step
//! is made again with the arm, its inner over the site number the feed's own make placed (`hold_inners`), and its prologue
//! (an inner LIMIT's bounds) runs with the inner's (`SourceMech::prologue`); only over inners whose second make declares
//! nothing a fill holds once (`remade`: no Material spool, CTE store or union record).
//! Refused by name: such a pruning the nest's parameters alone do not decide (D2013), an arm over another breaker (D2014),
//! an arm other than filters, projections and ProjectSets over a heap scan, an index walk (nest/inner_arms_index.rs), a
//! Result or VALUES (D2015), a rerun set beyond the arms and the slots they read that no set fills (D2016, nest/inner.rs;
//! a union the set refills is the eager form's, S2r: union_arms.rs `refilled`), a cstring column whose bytes may not
//! outlive the next read (D2020). Registers nest.loop's source maker (the feeds' `Empty`). Owner M8-NEST.
use gravity_abi::ctx::site_off;
use gravity_abi::status::raise;
use gravity_abi::site::site;
use gravity_decompose::catalog::expr_type;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::append::{AppendDesc, PruneDesc};
use gravity_decompose::ops::shared::srf::SrfPlace;
use gravity_decompose::ops::shared::subplan::{SubForm, SubLink};
use gravity_decompose::catalog::typefacts::{facts_of, TypeFacts};
use gravity_decompose::{Am, Op, Source};
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::append::raw::NestPrune;
use gravity_kernels::util::RESUME_OPEN;
use gravity_program::protos::{Proto, ProtoKind, Words};
use gravity_program::{NumTypmod, Rep, Until};
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};
use types_nodes::list::NodeList;

use crate::cell::{At, Cell, CellHome, CellLife, Nullable, Row};
use crate::face::end::EndMech;
use crate::face::kinds::{Face, KindId};
use crate::face::source::{Body, SourceMech, WinConds};
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::{Next, SourceMaker, StepMaker, Win};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{ExId, Px, WordKey, DRAFT_SITE};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;
use crate::rerun::params::NestExt;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::NestLoop, &SourceMaker(make_empty))], ..crate::ops::NONE };

pub(crate) const UNION_PRUNE_LOUD: &str = "a union in a nest inner that PostgreSQL prunes at run time over an arm that can raise or calls a user or VOLATILE function, or with pruning that does (a user partition key support function) (D2013)";
pub(crate) const UNION_ARM_BREAKER: &str = "a union in a nest inner with an arm over a breaker (D2014)";
pub(crate) const UNION_ARM_BEYOND_FORMS: &str = "a union in a nest inner with an arm other than filters, projections and ProjectSets over one heap scan, a Result or VALUES, an ordered arm, or an arm scan exec admit would pair with another scan (D2015)";
const UNION_UNDER_RERUN_SET: &str = "a union in a nest inner under a rerun set beyond its arms (D2016)";
pub(crate) const UNION_CSTRING_ARM: &str = "a union in a nest inner with a cstring arm column whose bytes may not outlive the arm's next read (a computed or set-function value; no copy slot for a cstring) (D2020)";

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// The feed pipeline's source: it reads nothing (the inner makes its arm again). Its row, in dead code only, carries the
/// arm source's columns as NULL cells so its ops still lower.
struct Empty<'g> {
    cols: PgVec<'g, (u32, Rep)>,
}

fn make_empty<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    crate::rerun::inner_arms_index::hold(px, k); // D3493: an index arm's number, where exec admit walks its feed
    let (gx, mcx) = (px.gx, px.mcx());
    let p = &gx.pipelines()[k];
    let rep = |t: u32| facts_of(gx.facts, t).map_or(Rep::Word { bytes: 8 }, |f| Rep::of_storage(f.typlen, f.byval));
    let cols = match &p.source {
        Source::Range { relid, .. } => {
            let natts = gx.facts.rel(*relid).map_or(0, |r| r.natts.max(0) as usize);
            (1..=natts as i16).map(|a| gx.facts.att(*relid, a).map_or((types_core::INT8OID, Rep::Word { bytes: 8 }), |f| (f.typid, rep(f.typid)))).collect_in(mcx)
        }
        Source::Values { lists, .. } => lists.iter().next().and_then(|l| l.as_list()).map_or_else(|| PgVec::new_in(mcx), |l| l.iter().map(|n| expr_type::type_of(n).unwrap_or_else(|| expr_type::result_type(n))).map(|t| (t, rep(t))).collect_in(mcx)),
        // D3789: a CteScan feed's, its CTE's projection
        Source::Breaker { slot } => gx.life_of(k, *slot).and_then(|l| match gx.pipelines()[l.0 as usize].ops.last() {
            Some(Op::Project(cols)) => Some(cols.iter().filter_map(|n| n.as_target_entry()).map(|te| expr_type::type_of(te.expr).unwrap_or_else(|| expr_type::result_type(te.expr))).map(|t| (t, rep(t))).collect_in(mcx)),
            _ => None,
        }).unwrap_or_else(|| PgVec::new_in(mcx)),
        _ => crate::rerun::inner_arms_index::cols(gx, &p.source, &rep), // S2c: an index feed's
    };
    mcx::pg_box_dyn!(mcx, Empty { cols } => dyn SourceMech + 'g)
}

impl SourceMech for Empty<'_> {
    fn open(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        env.zero
    }

    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        b.br(l_eof, &[env.zero]);
        let dead = b.create_block(&[]);
        b.switch_to(dead);
        Win { cursor: env.zero, end: env.zero, facts: WinConds::default() }
    }

    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let (life, mcx) = (CellLife { until: Until::Row, home: CellHome::FrameSlot }, env.mcx);
        let cells = self.cols.iter().enumerate().map(|(c, &(typid, rep))| Cell { rep, typid, num: NumTypmod::NONE, life, at: At::Slot(c as u16), null: Nullable::Maybe }).collect_in(mcx);
        let t = b.bconst(true);
        let vals = self.cols.iter().map(|_| (env.zero, Some(t))).collect_in(mcx);
        RowVals { row: Row { cells }, cols: (0..self.cols.len() as u16).map(Some).collect_in(mcx), vals, carry, next, at: j }
    }
}

/// The feed pipeline's end: it reads nothing, so its function leaves at the open.
struct Fused;

impl EndMech for Fused {
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        b.br(env.x.l_done, &[]);
        let dead = b.create_block(&[]);
        b.switch_to(dead);
    }

    fn lower<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        row.carry.copy_in(env.mcx)
    }
}

pub(crate) fn fused<'g>(mcx: Mcx<'g>) -> PgBox<'g, dyn EndMech + 'g> {
    mcx::pg_box_dyn!(mcx, Fused => dyn EndMech + 'g)
}

/// One arm made inside the inner: its source and ops over its site (`None`: a Result's or VALUES' position in `pos`, the
/// arm's own word), the words written at its open, its expanding steps (step, offset word), its plan node and pipeline.
struct Arm<'g> {
    src: PgBox<'g, dyn SourceMech + 'g>,
    steps: PgVec<'g, PgBox<'g, dyn StepMech + 'g>>,
    gates: Vec<(i32, ExId)>,
    /// D3829: the hashed SubPlan probes only the gates read (inner_arms_gate.rs `gate_sub`), lent at the open, never steps.
    lenders: PgVec<'g, PgBox<'g, dyn StepMech + 'g>>,
    site: Option<usize>,
    pos: Option<i32>,
    words: OpenWords<'g>,
    expands: PgVec<'g, (usize, i32)>,
    node: i32,
    k: usize,
    /// G2(e), S1u (`pulled`): the child's skip bit `(word, bit)` of an initial pruning, read before its open.
    skip: Option<(u16, u32)>,
}

/// The inner's source: the arms, the union's column types, the per-open words (the arm, its open flag, the position, the
/// count), the buffer's (datum, null) frame words per column and a by-reference column's copy slot (handle word, typlen).
struct Arms<'g> {
    arms: PgVec<'g, Arm<'g>>,
    tfs: PgVec<'g, TypeFacts>,
    w: [i32; 4],
    buf: PgVec<'g, (i32, i32)>,
    keeps: PgVec<'g, Option<(u16, i16)>>,
    node: i32,
    /// D2013 (`nest_prune`): the pruning object's handle word, and the frame words of the pruned arms' mask and of its
    /// decided flag (both zero at each re-open: the first read after it decides).
    prune: Option<(u16, [i32; 2])>,
}

/// nest/inner.rs: the rerun set (graph.reruns entry `set`, rerun/sets.rs `owner_set`) of the inner op of pipeline `k` over
/// union `u`'s slot. The arms run again at every re-open, so a set of only the union's feeds and slot re-runs nothing more:
/// it is declared (its members run only through the owner's m8_rerun, program.rs, which the inner never calls: the feeds
/// never run) and none is kept. Another slot of the set is one an arm reads that no set fills (D3789: a CTE's store, a
/// hashed SubPlan's set), the same rows at every re-open. A set beyond them refuses D2016.
pub(crate) fn rerun<R>(px: &mut Px<'_, '_>, (k, set, slot): (usize, usize, u8), u: Option<&(usize, u8, Vec<usize>)>, node: i32) -> Option<R> {
    let gx = px.gx;
    let feeds = u.map_or(&[][..], |u| &u.2[..]);
    let r = &gx.graph.reruns[set];
    let read = |s: u8| feeds.iter().any(|&f| gx.pipelines()[f].desc.consumes.contains(&s) && gx.life_of(f, s).is_some_and(|l| once(gx, l.0 as usize)));
    if !(r.pipelines.iter().all(|&q| feeds.contains(&(q as usize))) && (0..=u8::MAX).all(|s| s == slot || !gravity_decompose::graph::slot_has(&r.slots, s) || read(s))) {
        shape(node, UNION_UNDER_RERUN_SET);
    }
    crate::rerun::sets::Rerun::declare(px, k, set);
    None
}

/// The emit pipeline `ik` (a nest or nested SubPlan inner whose first op is its union's `Union`): its arms admitted, then made.
pub(crate) fn source<'g>(px: &mut Px<'g, '_>, ik: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let mcx = px.mcx();
    let ps = gx.pipelines();
    let node = ps[ik].node_id;
    let Some((_, _, feeds)) = crate::pipes::inner_union(gx, ik) else { crate::refuse!(node, Reason::Bug { payload: "a nest inner's union without its feeds" }) };
    // A run-time pruned union whose arms and pruning calls are all quiet runs every arm unpruned (its pruned arms pass no row);
    // D2013: else its pruning runs as PostgreSQL's, at the first read after each re-open (`nest_prune`)
    let prune = match ps[ik].ops.first() { Some(Op::Append(AppendDesc::Union { prune: Some(d), .. })) => Some(d), _ => None };
    // EXPLAIN ANALYZE prunes as PostgreSQL's too: an arm its pruning removes never starts (`(never executed)`)
    let loud = prune.filter(|d| crate::frame::instr::on() || !crate::rerun::union_arms::quiet_prune(gx, d) || !feeds.iter().all(|&f| crate::rerun::union_arms::quiet_arm_in(gx, &ps[f], true, false)));
    let handle = loud.map(|d| nest_prune(px, ik, d, feeds.len(), node));
    let cols: PgVec<'_, &NodeList<'_>> = feeds.iter().map(|&f| admit_arm(gx, f, node, true)).collect_in(mcx);
    let tfs = types(gx, node, cols[0]);
    if cols[1..].iter().any(|c| types(gx, node, c).iter().map(|t| t.typid).ne(tfs.iter().map(|t| t.typid))) {
        shape(node, UNION_ARM_BEYOND_FORMS);
    }
    let mut word = |i: u8, j: usize| px.frame_word(WordKey::Open { face: Face::Nest, k: ik, j: crate::frame::px::key_j(j), i }) as i32 * 8;
    let w = [word(4, usize::MAX), word(5, usize::MAX), word(6, usize::MAX), word(7, usize::MAX)];
    let buf = (0..tfs.len()).map(|c| (word(1, usize::MAX - 1 - c), word(2, usize::MAX - 1 - c))).collect_in(mcx);
    let prune = handle.map(|h| (h, [word(8, usize::MAX), word(9, usize::MAX)]));
    // D2020: a by-reference column's copy slot (a cstring has none)
    let keeps = tfs.iter().map(|tf| (tf.typlen == -1 || tf.typlen > 0).then(|| crate::ops::subplan::result::copy_slot(px, ik, node, tf)).flatten()).collect_in(mcx);
    let arms = feeds.iter().enumerate().map(|(a, &f)| make_arm(px, ik, f, a)).collect_in(mcx);
    mcx::pg_box_dyn!(mcx, Arms { arms, tfs, w, buf, keeps, node, prune } => dyn SourceMech + 'g)
}

/// D2013: a union PostgreSQL prunes at run time over the parameters of the nest whose inner it is, where an arm or a pruning
/// call is not quiet. PostgreSQL's ExecReScanAppend clears the valid set at every rescan whose chgParam meets the prune
/// state's execparamids (a nest flags every parameter it sets at every outer row), and choose_next_subplan_locally runs
/// ExecFindMatchingSubPlans at the first read after it: so does `m8_prune` here (exec ops/append/prune.rs `NestPrune`,
/// PostgreSQL's own pruning code), and a pruned arm never opens. A correlated SubPlan's inner prunes the same way at every
/// call, over its parParams (D3378). Refused by D2013's name: a pruning with no exec steps (its initial steps only), over a
/// parameter neither the nest or SubPlan nor a once-run InitPlan sets (D4032), over InitPlans' parameters alone with steps
/// that are not quiet, over more than 63 arms, or whose steps decompose's hoist rewrote (D2013
/// slice 2: its info is no longer the statement's entry at its index, decompose facts/hoist.rs `HOISTED`). The nest.prune
/// protocol's handle word.
fn nest_prune(px: &mut Px<'_, '_>, ik: usize, d: &PruneDesc<'_>, n: usize, node: i32) -> u16 {
    let gx = px.gx;
    let mcx = px.mcx();
    let ps = gx.pipelines();
    let inners = ps.inners();
    let Some(&((k, j), _)) = ik.checked_sub(ps.len()).and_then(|i| inners.get(i)) else { shape(node, UNION_PRUNE_LOUD) };
    // the parameters the owner sets before each re-open: a nest's NestLoopParams, or a correlated SubPlan's parParams
    // (D3378: ExecScanSubPlan adds each to the plan's chgParam at every call before its ExecReScan, so ExecReScanAppend
    // clears the valid set there as at a nest's rescan)
    let set: PgVec<'_, i32> = match ps[k].ops.get(j) {
        Some(Op::Join(JoinDesc::Nest { params, .. })) => params.iter().filter_map(|p| p.as_nest_loop_param().map(|p| p.paramno)).collect_in(mcx),
        Some(Op::SubPlan(s)) => s.params.iter().collect_in(mcx),
        _ => shape(node, UNION_PRUNE_LOUD),
    };
    // D4032: a parameter an InitPlan sets once for the statement (rerun/spool/form.rs `init_once`) is read from that
    // InitPlan at the decision (exec ops/append/prune.rs `NestPruneRt::init_word`, D3612), as PostgreSQL's
    // ExecFindMatchingSubPlans runs it at the Append's first call. PostgreSQL clears the valid set only at a rescan whose
    // chgParam meets execparamids: with one of the owner's parameters among them it decides again at every re-open, as
    // here; over InitPlans' parameters alone it decides once, so deciding again at each re-open shows the same only when
    // the steps are quiet with those parameters bound.
    let init = |x: i32| crate::rerun::spool::form::init_once(gx, x);
    let mut owned = false;
    let over = d.exec_params_all(|x| {
        owned |= set.contains(&x);
        set.contains(&x) || init(x)
    });
    let nests = over && (owned || crate::effects::visible::nestloop::quiet_prune_bound(gx, d, &init));
    if !d.exec || !nests || n > 63 || d.index == gravity_decompose::facts::hoist::HOISTED || d.index > u16::MAX as u32 {
        shape(node, UNION_PRUNE_LOUD);
    }
    let mut root = k;
    while root >= ps.len() {
        root = inners[root - ps.len()].0 .0;
    }
    let base = px.proto_words(1);
    let mut q = Proto::new(ProtoKind::NestPrune, root as u16);
    q.arg = crate::ops::append::prune::arg(d.index, n as u16, 0, n as u16, true);
    q.words = Words { base, n: 1 };
    px.declare_proto(q);
    base
}

/// G2(e), S1u (material, rerun/spool/form.rs `union_pull`): the arms of the settled union `feeds` a plan Material's reader
/// `ik` pulls once per execution, as PostgreSQL's Material pulls its Append (ops/material/lower.rs). The same arms as
/// `source`'s, each with its child's skip bit of an initial pruning (`skips`, arm order): a pruned child is never opened,
/// its end the next arm's turn. The reader writes the per-open words once, at its first pull. D3612: `late`, the union's
/// run-time pruning over InitPlans that may raise or act (rerun/spool/form.rs `pull_late`): decided at the first pull by
/// the nest.prune object (`decide`), which runs those InitPlans there, as PostgreSQL's Append's first call does; an arm it
/// prunes never opens.
pub(crate) fn pulled<'g>(px: &mut Px<'g, '_>, ik: usize, feeds: &[usize], skips: PgVec<'_, Option<(u16, u32)>>, late: Option<&PruneDesc<'_>>, node: i32) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let mcx = px.mcx();
    let cols: PgVec<'_, &NodeList<'_>> = feeds.iter().map(|&f| admit_arm(gx, f, node, false)).collect_in(mcx);
    let tfs = types(gx, node, cols[0]);
    if cols[1..].iter().any(|c| types(gx, node, c).iter().map(|t| t.typid).ne(tfs.iter().map(|t| t.typid))) {
        shape(node, UNION_ARM_BEYOND_FORMS);
    }
    let mut word = |i: u8, j: usize| px.frame_word(WordKey::Open { face: Face::Nest, k: ik, j: crate::frame::px::key_j(j), i }) as i32 * 8;
    let w = [word(4, usize::MAX), word(5, usize::MAX), word(6, usize::MAX), word(7, usize::MAX)];
    let buf = (0..tfs.len()).map(|c| (word(1, usize::MAX - 1 - c), word(2, usize::MAX - 1 - c))).collect_in(mcx);
    let pw = late.is_some().then(|| [word(8, usize::MAX), word(9, usize::MAX)]);
    let keeps = tfs.iter().map(|tf| (tf.typlen == -1 || tf.typlen > 0).then(|| crate::ops::subplan::result::copy_slot(px, ik, node, tf)).flatten()).collect_in(mcx);
    let prune = late.zip(pw).map(|(d, pw)| (pull_prune(px, ik, d, feeds.len(), node), pw));
    let arms = feeds.iter().zip(skips).enumerate().map(|(a, (&f, skip))| Arm { skip, ..make_arm(px, ik, f, a) }).collect_in(mcx);
    mcx::pg_box_dyn!(mcx, Arms { arms, tfs, w, buf, keeps, node, prune } => dyn SourceMech + 'g)
}

/// D3612: the nest.prune object of a Material's pulled union pruned over InitPlans that may raise or act (`pulled`'s
/// `late`), decided once, at the first pull (the reader writes `pulled`'s words once); exec reads each parameter from its
/// InitPlan (exec ops/append/prune.rs `NestPruneRt::init_word`). Its handle word.
fn pull_prune(px: &mut Px<'_, '_>, ik: usize, d: &PruneDesc<'_>, n: usize, node: i32) -> u16 {
    let ps = px.gx.pipelines();
    if n > 63 || d.index > u16::MAX as u32 {
        shape(node, UNION_PRUNE_LOUD);
    }
    let mut root = ik;
    while root >= ps.len() {
        root = ps.inners()[root - ps.len()].0 .0;
    }
    let base = px.proto_words(1);
    let mut q = Proto::new(ProtoKind::NestPrune, root as u16);
    q.arg = crate::ops::append::prune::arg(d.index, n as u16, 0, n as u16, true);
    q.words = Words { base, n: 1 };
    px.declare_proto(q);
    base
}

/// D3789: pipeline `q` runs in no rerun set: what it fills is filled once for the statement.
fn once(gx: &Gx<'_, '_>, q: usize) -> bool {
    !gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(q as u16)))
}

/// D3789: feed `f` reads a CTE's store that no rerun set re-runs (a CteScan arm), read again from its start at every open.
pub(crate) fn cte_arm(gx: &Gx<'_, '_>, f: usize) -> bool {
    let Source::Breaker { slot } = gx.pipelines()[f].source else { return false };
    gx.life_of(f, slot).is_some_and(|l| gx.graph.init_plans.iter().any(|i| i.pipeline == l.0 && matches!(i.link, SubLink::Cte)) && once(gx, l.0 as usize))
}

/// D3789 x D3782: feed `f` is a CteScan arm (`cte_arm`) whose ops S2b streams (`arm_body`, with its hashed probes).
pub(crate) fn cte_streams(gx: &Gx<'_, '_>, f: usize) -> bool {
    let ops = &gx.pipelines()[f].ops;
    cte_arm(gx, f) && matches!(ops.split_last(), Some((Op::Append(AppendDesc::Feed { .. }), body)) if arm_body(gx, f, body, true).is_some())
}

/// Feed `f`'s arm served here, else the refusal's name: its union columns (the projection before its Feed). `streamed`:
/// the nest inner's own arms (`source`), which may read a CTE and probe a hashed SubPlan's set built once (D3789).
fn admit_arm<'p>(gx: &Gx<'_, 'p>, f: usize, node: i32, streamed: bool) -> &'p NodeList<'p> {
    let p = &gx.pipelines()[f];
    match p.source {
        Source::Breaker { .. } if streamed && cte_arm(gx, f) => {}
        Source::Breaker { .. } => shape(node, UNION_ARM_BREAKER),
        Source::Range { am: Am::Heap, .. } | Source::Const | Source::Values { .. } => {}
        _ => crate::rerun::inner_arms_index::admit(gx, p, node), // S2c: an index or one-index bitmap arm, else D2015
    }
    // PSP3-11: an ordered Append's feed (the planner's mark: the partitions in bound order, each in the keys' order) streams
    // as any other, arm 0 to its end and then the next, which is PostgreSQL's Append and its order
    let Some((Op::Append(AppendDesc::Feed { .. }), body)) = p.ops.split_last() else { shape(node, UNION_ARM_BEYOND_FORMS) };
    let cols = arm_body(gx, f, body, streamed).unwrap_or_else(|| shape(node, UNION_ARM_BEYOND_FORMS));
    if !crate::rerun::inner_arms_index::lone_range(gx, &p.source) {
        shape(node, UNION_ARM_BEYOND_FORMS);
    }
    crate::rerun::inner_arms_gate::admit(gx, body, node); // S2d: a gated arm in a writing statement
    cols
}

/// Feed `f`'s ops before its Feed (`body`) as an arm makes them: filters, projections, gates and target-list set functions,
/// the last a projection: its union columns, else none. A top window's row cap in the arm (`arm_cap`) is no op of it.
/// `sets`: a hashed SubPlan's probe of a set no rerun set builds (D3789) too.
fn arm_body<'p>(gx: &Gx<'_, 'p>, f: usize, body: &[Op<'p>], sets: bool) -> Option<&'p NodeList<'p>> {
    let probe = |op: &Op<'_>| matches!(op, Op::SubPlan(d) if matches!(d.form, SubForm::Hashed { slot } if gx.life_of(f, slot).is_some_and(|l| once(gx, l.0 as usize))));
    // D4062: a correlated SubPlan (its inner made again with the arm, run per row as PostgreSQL's ExecScanSubPlan in the
    // arm's qual) over an inner the arm may make again (`remade`)
    let nested = |j: usize, op: &Op<'_>| matches!(op, Op::SubPlan(d) if matches!(d.form, SubForm::Nested { .. })) && gx.pipelines().inner_of(f, j).is_some_and(|ik| remade(gx, ik));
    let ok = |(j, op): (usize, &Op<'_>)| matches!(op, Op::Filter(_) | Op::Project(_) | Op::Gate(_)) || matches!(op, Op::Srf(d) if d.place == SrfPlace::TargetList) || arm_cap(gx, f, op) || sets && (probe(op) || nested(j, op));
    match body.last() {
        Some(Op::Project(cols)) if body.iter().enumerate().all(ok) => Some(*cols),
        _ => None,
    }
}

/// D4062: inner `ik` and every inner below it read no breaker but a run (a sort's, a bounded sort's, a sorted group's), so a
/// second make of them (the arm's, after the feed's own) declares nothing a fill holds once: a Material's spool, a CTE's
/// store or a union's record is declared once per fill (spool/rec.rs: a second declaration is a Bug).
fn remade(gx: &Gx<'_, '_>, ik: usize) -> bool {
    crate::stack_check(); // D3832
    let ps = gx.pipelines();
    let p = &ps[ik];
    let runs = || matches!(gx.source_kind(ik), KindId::RunsSort | KindId::RunsTopkRows | KindId::RunsTopkGroups | KindId::RunsUnique | KindId::RunsGroup | KindId::RunsIncremental | KindId::RunsGroupDistinct);
    (!matches!(p.source, Source::Breaker { .. }) || runs()) && (0..p.ops.len()).all(|j| ps.inner_of(ik, j).is_none_or(|x| remade(gx, x)))
}

/// D3377 road (3) (b): op `op` of feed `f` is the row cap decompose put in each arm of a union a top window reads under its
/// LIMIT (window.rs `cap_arms`, union_arms.rs `capped_feed`). Made again by the window's pulled fill, the arm keeps no
/// cap: the window's spool asks the arms for the rows PostgreSQL's spool_tuples reads, and its LIMIT stops there.
fn arm_cap(gx: &Gx<'_, '_>, f: usize, op: &Op<'_>) -> bool {
    crate::ops::limit::lower::const_bound(op) && crate::rerun::union_arms::capped_feed(gx, f)
}

/// D3377 road (3) (union_arms.rs `arms_read`): feed `f` is an arm `admit_arm` takes, without its refusal: its union
/// columns' type oids in resno order (a union whose arms do not all fit, or differ in type, keeps its record form).
pub(crate) fn arm_fits<'g>(gx: &Gx<'g, '_>, f: usize) -> Option<PgVec<'g, u32>> {
    let p = &gx.pipelines()[f];
    let src = match p.source {
        Source::Breaker { .. } => false,
        Source::Range { am: Am::Heap, .. } | Source::Const | Source::Values { .. } => true,
        _ => crate::rerun::inner_arms_index::fits(gx, p),
    };
    let (Op::Append(AppendDesc::Feed { .. }), body) = p.ops.split_last()? else { return None };
    let cols = arm_body(gx, f, body, false)?;
    if !src || !crate::rerun::inner_arms_index::lone_range(gx, &p.source) || gx.pipelines().iter().any(crate::rerun::inner_arms_index::writes) {
        return None;
    }
    let mut out = PgVec::new_in(gx.mcx);
    for (i, n) in cols.iter().enumerate() {
        let te = n.as_target_entry().filter(|te| te.resno as usize == i + 1)?;
        out.push_in(facts_of(gx.facts, expr_type::type_of(te.expr).unwrap_or_else(|| expr_type::result_type(te.expr))).map(|tf| tf.typid)?);
    }
    Some(out)
}

/// The union's column types, `cols` in resno order.
fn types<'g>(gx: &Gx<'g, '_>, node: i32, cols: &NodeList<'_>) -> PgVec<'g, TypeFacts> {
    cols.iter().enumerate().map(|(i, n)| {
        let te = n.as_target_entry().filter(|te| te.resno as usize == i + 1).unwrap_or_else(|| shape(node, UNION_ARM_BEYOND_FORMS));
        facts_of(gx.facts, expr_type::type_of(te.expr).unwrap_or_else(|| expr_type::result_type(te.expr))).unwrap_or_else(|| shape(node, UNION_ARM_BEYOND_FORMS))
    }).collect_in(gx.mcx)
}

/// Arm `a`, feed `kf`: its source and ops made under the inner's identity, its site numbered next (S1's pull form).
fn make_arm<'g>(px: &mut Px<'g, '_>, ik: usize, kf: usize, a: usize) -> Arm<'g> {
    let (gx, mcx) = (px.gx, px.mcx());
    let fp = &gx.pipelines()[kf];
    let body = &fp.ops[..fp.ops.len() - 1];
    // D3789: a CteScan arm's source is the CTE's reader (ops/ctescan/lower.rs), its read pointer reset at every open
    let cte = cte_arm(gx, kf);
    let kind = if cte { KindId::NestCte } else { crate::graph::source_kind_of(&fp.source, fp.node_id) };
    let ident = px.ident(ik);
    let protos = px.protos.len();
    let lent = |j: usize| crate::rerun::inner_arms_gate::gate_sub(body, j);
    let held = hold_inners(px, kf); // D4062
    let (mut src, steps, gates, lenders) = px.making(kf, ident, |px| {
        let src = (crate::frame::iface::kind_impl::<SourceMaker>(&fp.node_id, kind).0)(px, kf);
        let gates = crate::rerun::inner_arms_gate::compile(px, fp, kf); // S2d: checked at the arm's open, never a step
        let lenders: PgVec<'g, PgBox<'g, dyn StepMech + 'g>> = (0..body.len()).filter(|&j| lent(j)).map(|j| {
            let n = crate::rerun::inner::op_node(fp, j);
            (crate::frame::iface::kind_impl::<StepMaker>(&n, gx.step_need(kf, j)).0)(px, kf, j)
        }).collect_in(mcx);
        let steps: PgVec<'g, PgBox<'g, dyn StepMech + 'g>> = (0..body.len()).filter(|&j| !matches!(body[j], Op::Gate(_)) && !arm_cap(gx, kf, &body[j]) && !lent(j)).map(|j| {
            let n = crate::rerun::inner::op_node(fp, j);
            (crate::frame::iface::kind_impl::<StepMaker>(&n, gx.step_need(kf, j)).0)(px, kf, j)
        }).collect_in(mcx);
        (src, steps, gates, lenders)
    });
    took_inners(px, held, fp.node_id);
    src.in_order();
    let draft = px.making(kf, ident, |px| {
        let mut d = px.take_site(kf).or_else(|| src.site_desc(px.arena));
        if let Some(d) = d.as_mut() {
            src.seal(px, kf, d);
        }
        d
    });
    let site = draft.map(|d| crate::rerun::inner_arms_index::site(px, kf, d)); // an index arm's where exec admit pairs it
    px.set_pipeline_site(kf, site); // a row mark's recheck finds the marked relation's producer here (marks.rs)
    for p in px.protos[protos..].iter_mut().filter(|p| p.site == DRAFT_SITE) {
        p.site = site.map_or(-1, |s| s as i16);
    }
    // D3789: a CteScan arm reads on the leader by its own read pointer, an open word
    if site.is_none() != (cte || matches!(fp.source, Source::Const | Source::Values { .. })) || src.covers() != 0 {
        shape(fp.node_id, UNION_ARM_BEYOND_FORMS);
    }
    // a Result's or VALUES' position: the arm's own word (`Env::pos`), written RESUME_OPEN at the arm's open
    let pos = (site.is_none() && !cte).then(|| px.frame_word(WordKey::Open { face: Face::Nest, k: ik, j: crate::frame::px::key_j(usize::MAX - 1 - a), i: 0 }) as i32 * 8);
    let mut words = OpenWords::new_in(mcx);
    if let Some(o) = pos {
        words.declare(OpenWord::open("nest inner union arm position", Base::Words, o, Type::I64, RESUME_OPEN));
    }
    src.open_words(&mut words);
    steps.iter().chain(lenders.iter()).for_each(|s| s.open_words(&mut words));
    // a copied op runs once per row: an op keeping state across rows in the end's slot is not an arm's
    if words.words.iter().any(|w| w.base == Base::State) {
        shape(fp.node_id, UNION_ARM_BEYOND_FORMS);
    }
    let expands = steps.iter().enumerate().filter_map(|(i, s)| s.expand().map(|e| (i, e.offset))).collect_in(mcx);
    Arm { src, steps, gates, lenders, site, pos, words, expands, node: fp.node_id, k: kf, skip: None }
}

/// D4062: the site numbers feed `kf`'s own make (`make_empty`'s pipeline, dead code made in graph order before the union's
/// owner) placed for the inners its ops embed (a correlated SubPlan's scan), held for the arm's remake of them (pipes.rs
/// `place_site`): each such scan keeps one number, which exec admit pairs with its own source. A second number of that
/// (relation, index) would take the first source of its key exec admit finds, another arm's SubPlan's (D1633's class).
/// Only the first remake takes them (a remade owner's arms number theirs anew, as any remade inner does). `(held, fresh)`:
/// each `(inner, number)` held, and the inners numbered nothing so far.
fn hold_inners<'g>(px: &mut Px<'g, '_>, kf: usize) -> (PgVec<'g, (usize, usize)>, PgVec<'g, usize>) {
    let mcx = px.mcx();
    let n = px.gx.pipelines().len();
    let w = crate::rerun::inner_arms_index::entries(px.gx);
    let Some(own) = w.iter().position(|e| e.0 == kf) else { return (PgVec::new_in(mcx), PgVec::new_in(mcx)) };
    let (mut held, mut fresh) = (PgVec::new_in(mcx), PgVec::new_in(mcx));
    for x in w[own + 1..].iter().take_while(|e| e.0 >= n).map(|e| e.0) {
        let Some(s) = px.pipeline_site(x) else {
            fresh.push_in(x);
            continue;
        };
        let ext = px.ext_mut::<NestExt>(Face::Nest);
        if !ext.arm_sites.contains(&x) && !ext.sites.iter().any(|h| h.0 == x) {
            ext.arm_sites.push(x); // alloc: Program
            ext.sites.push((x, s)); // alloc: Program
            held.push_in((x, s));
        }
    }
    (held, fresh)
}

/// D4062: every number `hold_inners` held was taken by its own inner's remake, and no inner it found unnumbered got one
/// (a feed made after the arm would number it again), else the arm refuses (D2015); a number left held is dropped.
fn took_inners<'g>(px: &mut Px<'g, '_>, (held, fresh): (PgVec<'g, (usize, usize)>, PgVec<'g, usize>), node: i32) {
    let ext = px.ext_mut::<NestExt>(Face::Nest);
    let left = ext.sites.iter().any(|h| held.contains(h));
    ext.sites.retain(|h| !held.contains(h));
    if left || held.iter().any(|&(x, s)| px.pipeline_site(x) != Some(s)) || fresh.iter().any(|&x| px.pipeline_site(x).is_some()) {
        shape(node, UNION_ARM_BEYOND_FORMS);
    }
}

/// The environment an arm lowers under, and what it replaced.
struct Saved<'e> {
    site: Option<usize>,
    k: usize,
    node: i32,
    step: Option<usize>,
    win: Option<Win>,
    expands: PgVec<'e, (usize, i32)>,
    pos: Option<i32>,
    single: Option<usize>,
    inner: Option<PgVec<'e, Option<u16>>>,
    srfs: Option<crate::frame::env::AggRow<'e>>,
    mult: Option<Value>,
    tables: Option<Value>,
    codes: PgVec<'e, gravity_expr::emit::CodeRef>,
    body: Body,
    escape: Option<Block>,
    loc: Option<crate::face::source::Locator>,
    memo: crate::lx::SetAside<'e>,
}

fn enter<'e>(env: &mut Env<'e, '_>, a: &Arm<'_>) -> Saved<'e> {
    use core::mem::{replace, take};
    Saved {
        site: replace(&mut env.site, a.site),
        k: replace(&mut env.k, a.k),
        node: replace(&mut env.node, a.node),
        step: env.step.take(),
        win: env.win.take(),
        expands: replace(&mut env.expands, mcx::to_vec_in(&a.expands[..], env.mcx)),
        pos: replace(&mut env.pos, a.pos),
        single: env.single.take(),
        inner: env.inner.take(),
        srfs: env.srfs.take(),
        mult: env.mult.take(),
        tables: env.tables.take(),
        codes: replace(&mut env.codes, PgVec::new_in(env.mcx)),
        body: take(&mut env.body),
        escape: env.escape.take(),
        loc: env.loc.take(),
        memo: env.memo.set_aside(),
    }
}

fn leave<'e>(env: &mut Env<'e, '_>, s: Saved<'e>) {
    (env.site, env.k, env.node, env.step, env.win, env.expands, env.pos) = (s.site, s.k, s.node, s.step, s.win, s.expands, s.pos);
    (env.single, env.inner, env.srfs, env.mult, env.tables, env.codes, env.body, env.escape, env.loc) = (s.single, s.inner, s.srfs, s.mult, s.tables, s.codes, s.body, s.escape, s.loc);
    env.memo.restore(s.memo);
}

/// The arm's ops from `j` on over `row`, the last into `last`, `Env::step` naming the lowering one (srf/project_set.rs and
/// `stop::held` read the arm's later expanding steps by it).
fn chain<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, steps: &[PgBox<'_, dyn StepMech + '_>], j: usize, row: &RowVals, last: Next<'_, 'e>) -> Carry<'e> {
    crate::stack_check(); // D3832
    crate::frame::frame::bound(b, env, j); // EXPLAIN ANALYZE: the arm's node boundaries (`Arms::pull` makes them current)
    let Some((s, rest)) = steps.split_first() else {
        env.step = None;
        return last(b, env, row);
    };
    env.step = Some(j);
    let c = s.lower(b, env, row, &mut |b, env, r| {
        let c = chain(b, env, rest, j + 1, r, &mut *last);
        env.step = Some(j);
        c
    });
    env.step = None;
    c
}

impl Arms<'_> {
    /// D2013: at the first read after a re-open, `m8_prune(ctx, h)` decides the pruned arms (PostgreSQL's
    /// choose_next_subplan_locally after ExecReScanAppend cleared the valid set); a status below zero leaves by the raise
    /// exit with the host's stashed error (the support function's raise, at PostgreSQL's point).
    fn decide(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        let Some((h, [mw, dw])) = self.prune else { return };
        let done = env.load_word(b, dw);
        let fresh = b.icmp(IntCond::Eq, done, env.zero);
        let (l_call, l_go) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(fresh, (l_call, &[]), (l_go, &[]));
        b.switch_to(l_call);
        let hv = b.load(Type::I64, env.params, h as i32 * 8);
        let f = b.declare_host_once("m8_prune", &[Type::Ptr, Type::I64], Some(Type::I64), gravity_kernels::contract::entry1::<NestPrune> as *const () as usize, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() });
        let st = b.call_host(f, &[env.ctx, hv], Some(Type::I64)).unwrap_or(env.zero);
        let bad = b.icmp(IntCond::Slt, st, env.zero);
        let (l_bad, l_ok) = (b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_bad);
        b.cond_br(bad, (l_bad, &[]), (l_ok, &[]));
        b.switch_to(l_bad);
        let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
        b.br(env.l_raise, &[e]);
        b.switch_to(l_ok);
        env.store_word(b, mw, st);
        env.store_word(b, dw, env.one);
        b.br(l_go, &[]);
        b.switch_to(l_go);
    }

    /// Arm `a`'s cursor: its site's, or its own position word's (the source's `write_back`).
    fn cursor(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &Arm<'_>, cur: Value) {
        match a.site {
            Some(s) => {
                let c32 = b.conv(ConvOp::Trunc, Type::I32, cur);
                b.store(env.ctx, site_off(s) as i32 + site::CURSOR as i32, c32);
            }
            None => a.src.write_back(b, env, cur, env.zero),
        }
    }

    /// Row `r`'s union columns into the buffer (a by-reference one borrowed when its bytes outlive the arm's next read, else
    /// copied into its slot), the count bumped.
    fn put(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, r: &RowVals) {
        let mcx = env.mcx;
        let mut cx = env.lx(b, self.node);
        let vals: PgVec<'_, (Value, Value, Option<(u16, i16)>)> = self.tfs.iter().enumerate().map(|(c, tf)| {
            let Some(cr) = r.col(c as i16 + 1) else { shape(self.node, UNION_ARM_BEYOND_FORMS) };
            let borrows = matches!(cr.cell.rep, Rep::VarlenaPtr | Rep::FixedPtr { .. } | Rep::CstrPtr) && cr.cell.life.reaches(Until::Window);
            let keep = if tf.byval || borrows { None } else { Some(self.keeps[c].unwrap_or_else(|| shape(self.node, UNION_CSTRING_ARM))) };
            let (v, null) = crate::rep::datum(&mut cx, cr, tf);
            (v, null, keep)
        }).collect_in(mcx);
        env.memo = cx.memo;
        for (&(d, nw), &(v, null, keep)) in self.buf.iter().zip(vals.iter()) {
            let v = keep.map_or(v, |k| crate::ops::subplan::result::keep(b, env, k, v, null));
            let word = b.select(null, env.zero, v);
            env.store_word(b, d, word);
            let nv = b.conv(ConvOp::Zext, Type::I64, null);
            env.store_word(b, nw, nv);
        }
        let n = env.load_word(b, self.w[3]);
        let n1 = b.int_bin(IntBinOp::Add, n, env.one);
        env.store_word(b, self.w[3], n1);
    }

    /// Arm `i` read on until one row passes its ops (into the buffer, then `l_ready`); at its end the next arm is current
    /// and unopened (`l_next`); a status below zero `l_eof(status)`. The arm opens at its first read of the re-open.
    fn pull(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, i: usize, l_eof: Block, (l_ready, l_next): (Block, Block)) {
        let a = &self.arms[i];
        let saved = enter(env, a);
        // EXPLAIN ANALYZE: the arm's own node boundaries while its rows lower (none for an arm not counted, so the inner's
        // own never count an arm's steps), the inner's back at `leave`
        let bounds = crate::frame::instr::replace_current(crate::frame::instr::bounds_of(crate::frame::instr::arm_key(a.k)));
        let (l_open, l_read) = (b.create_block(&[]), b.create_block(&[]));
        let opened = env.load_word(b, self.w[1]);
        let fresh = b.icmp(IntCond::Eq, opened, env.zero);
        b.cond_br(fresh, (l_open, &[]), (l_read, &[]));
        b.switch_to(l_open);
        // S1u: a child PostgreSQL pruned at its executor start is never opened (its end: the next arm's turn)
        let l_pruned = a.skip.map(|(word, bit)| {
            let w = b.load(Type::I64, env.params, word as i32 * 8);
            let sh = b.iconst(Type::I64, bit as i64);
            let s = b.int_bin(IntBinOp::Ushr, w, sh);
            let s1 = b.int_bin(IntBinOp::And, s, env.one);
            let pruned = b.icmp(IntCond::Ne, s1, env.zero);
            let (l_pruned, l_kept) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(pruned, (l_pruned, &[]), (l_kept, &[]));
            b.switch_to(l_kept);
            l_pruned
        });
        // D2013: an arm this re-open's pruning removed is never opened (its end: the next arm's turn)
        let l_cut = self.prune.map(|(_, [mw, _])| {
            let m = env.load_word(b, mw);
            let sh = b.iconst(Type::I64, i as i64);
            let s = b.int_bin(IntBinOp::Ushr, m, sh);
            let s1 = b.int_bin(IntBinOp::And, s, env.one);
            let cut = b.icmp(IntCond::Ne, s1, env.zero);
            let (l_cut, l_kept) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(cut, (l_cut, &[]), (l_kept, &[]));
            b.switch_to(l_kept);
            l_cut
        });
        a.words.write_all(b, a.node, env.ctx, env.state, env.words);
        let l_shut = crate::rerun::inner_arms_gate::check_lent(b, env, &a.lenders, &a.gates); // S2d: a false or NULL gate ends the arm unopened
        // EXPLAIN ANALYZE: a loop of each of the arm's nodes as PG's Append starts the arm (a pruned arm never starts)
        crate::frame::instr::count_loops(b, env.ctx, &|_| true, env.mcx);
        let r = a.src.open(b, env);
        env.store_word(b, self.w[1], env.one);
        let (bad, l_opened) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]));
        b.cond_br(bad, (l_eof, &[r]), (l_opened, &[]));
        b.switch_to(l_opened);
        b.br(l_read, &[]);
        b.switch_to(l_read);
        let l_end = b.create_block(&[Type::I64]);
        // a row the ops drop reads the next: the loop's back edge checks for cancel
        let (l_loop, l_body) = crate::frame::cancel::header(b, env.ctx, env.l_cancel, &[], env.mcx);
        b.br(l_loop, &[]);
        b.switch_to(l_body);
        env.memo.clear();
        let win = a.src.next_window(b, env, l_end);
        env.win = Some(win);
        a.src.fill_late(b, env, &win, None);
        let past = b.int_bin(IntBinOp::Add, win.cursor, env.one);
        let row = a.src.row(b, env, &win, win.cursor, Carry::new(env.zero, env.mcx), past);
        // the cursor past the row, or at it while an expanding step may leave inside it (its offset word set)
        let inside = !a.expands.is_empty();
        self.cursor(b, env, a, if inside { win.cursor } else { past });
        chain(b, env, &a.steps, 0, &row, &mut |b, env, r| {
            self.put(b, env, r);
            b.br(l_ready, &[]);
            let dead = b.create_block(&[]);
            b.switch_to(dead);
            Carry::new(env.zero, env.mcx)
        });
        // here the row handed nothing on, or its expanding steps ended: it is through
        if inside {
            self.cursor(b, env, a, past);
        }
        b.br(l_loop, &[]);
        b.switch_to(l_end);
        let r = b.block_params(l_end)[0];
        let (bad, l_ended) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]));
        b.cond_br(bad, (l_eof, &[r]), (l_ended, &[]));
        b.switch_to(l_ended);
        crate::rerun::inner_arms_gate::shut(b, l_shut);
        let next = b.iconst(Type::I64, i as i64 + 1);
        env.store_word(b, self.w[0], next);
        env.store_word(b, self.w[1], env.zero);
        b.br(l_next, &[]);
        for l in l_pruned.into_iter().chain(l_cut) {
            b.switch_to(l);
            let next = b.iconst(Type::I64, i as i64 + 1);
            env.store_word(b, self.w[0], next);
            b.br(l_next, &[]);
        }
        crate::frame::instr::replace_current(bounds);
        leave(env, saved);
    }
}

impl SourceMech for Arms<'_> {
    fn declare(&mut self, pb: &mut ProgramBuilder) {
        for a in self.arms.iter_mut() {
            a.src.declare(pb);
            a.steps.iter_mut().chain(a.lenders.iter_mut()).for_each(|s| s.declare(pb)); // a step's own hosts, after its source (frame.rs's order)
        }
    }

    fn open_words(&self, w: &mut OpenWords) {
        let what = ["nest inner union arm", "nest inner union arm opened", "nest inner union position", "nest inner union count"];
        what.iter().zip(self.w.iter()).for_each(|(&s, &o)| w.declare(OpenWord::open(s, Base::Words, o, Type::I64, 0)));
        if let Some((_, [mw, dw])) = self.prune {
            w.declare(OpenWord::open("nest inner union pruned arms", Base::Words, mw, Type::I64, 0));
            w.declare(OpenWord::open("nest inner union pruning decided", Base::Words, dw, Type::I64, 0));
        }
    }

    /// D4062: each arm's steps' prologues under its environment (a correlated SubPlan's inner's: its LIMIT's bounds), as a
    /// pipeline's frame runs its steps' (the lenders run theirs at the arm's open, inner_arms_gate.rs `check_lent`).
    fn prologue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        for a in &self.arms {
            let saved = enter(env, a);
            a.steps.iter().for_each(|s| s.prologue(b, env, x));
            leave(env, saved);
        }
    }

    /// D4062: and their epilogues.
    fn epilogue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        for a in &self.arms {
            let saved = enter(env, a);
            a.steps.iter().for_each(|s| s.epilogue(b, env, x));
            leave(env, saved);
        }
    }

    /// The re-open writes the words (arm 0, unopened, nothing read); arm 0 opens at the first read.
    fn open(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        env.zero
    }

    /// The buffered row while the position is before the count, else the next row the arms hand on (module doc).
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let (l_pull, l_ready, l_union_end) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        let j = env.load_word(b, self.w[2]);
        let n = env.load_word(b, self.w[3]);
        let held = b.icmp(IntCond::Slt, j, n);
        b.cond_br(held, (l_ready, &[]), (l_pull, &[]));
        b.switch_to(l_pull);
        self.decide(b, env);
        let at = env.load_word(b, self.w[0]);
        let entry: PgVec<'_, Block> = self.arms.iter().map(|_| b.create_block(&[])).collect_in(env.mcx);
        for (i, &l) in entry.iter().enumerate() {
            let (ii, l_no) = (b.iconst(Type::I64, i as i64), b.create_block(&[]));
            let is = b.icmp(IntCond::Eq, at, ii);
            b.cond_br(is, (l, &[]), (l_no, &[]));
            b.switch_to(l_no);
        }
        b.br(l_union_end, &[]);
        for (i, &l) in entry.iter().enumerate() {
            b.switch_to(l);
            self.pull(b, env, i, l_eof, (l_ready, entry.get(i + 1).copied().unwrap_or(l_union_end)));
        }
        // past the last arm: the union's end for this re-open
        b.switch_to(l_union_end);
        b.br(l_eof, &[env.zero]);
        b.switch_to(l_ready);
        let cursor = env.load_word(b, self.w[2]);
        let end = env.load_word(b, self.w[3]);
        Win { cursor, end, facts: WinConds::default() }
    }

    /// The buffered row as datum cells (a cell lives for the row; the joined row caps it there too, nest/loop.rs).
    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let (life, mcx) = (CellLife { until: Until::Row, home: CellHome::FrameSlot }, env.mcx);
        let cells = self.tfs.iter().enumerate().map(|(c, tf)| Cell { rep: Rep::of_storage(tf.typlen, tf.byval), typid: tf.typid, num: NumTypmod::NONE, life, at: At::Slot(c as u16), null: Nullable::Maybe }).collect_in(mcx);
        let vals = self.buf.iter().map(|&(d, nw)| {
            let v = env.load_word(b, d);
            let nv = env.load_word(b, nw);
            (v, Some(b.icmp(IntCond::Ne, nv, env.zero)))
        }).collect_in(mcx);
        RowVals { row: Row { cells }, cols: (0..self.tfs.len() as u16).map(Some).collect_in(mcx), vals, carry, next, at: j }
    }

    /// The position: the row last handed on (nest/inner.rs `set_cursor`), or the window's end.
    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, _out: Value) {
        env.store_word(b, self.w[2], cur);
    }
}
