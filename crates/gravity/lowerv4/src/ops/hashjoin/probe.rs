//! Inner hash probe, kind `join.inner` (+ join/build.rs); one probe per pipeline. Budget 420 lines. Owner M1-JOIN. The
//! walk's helpers (the header, the first record, the joined row, the hand-on, join.mult, the single-row body) are in
//! join/walk.rs (M7 L0-b).
//!
//! Per row: the probe key's cells (a NULL key never matches: PG's strict `=`), its image, the first record from the
//! build's form (the KeyHash directory head, its bloom tags first; or the dense slot / chain head), then the chain: every
//! record whose image equals the row's (the dense forms are exact by index) runs the join qual (`ExecQual` order) and
//! hands the joined row on, the build's payload cells appended and mapped for INNER_VAR (`Env::inner`). The chain makes
//! the probe an expanding step (H32): the offset word counts the row's matches so a stop inside the row resumes past the
//! ones already handed on, or at the last one when a later expanding step stopped inside it (D688). An empty build ends the pipeline before its first row, as PG's hash join skips its outer
//! side. The plan's inner_unique stops at the first match that passes the join qual, as PG's single_match does. A build
//! laid `DBITS` or `DENSE` (each key once) runs the frame's single-row body (V4R-27, `single_guard`): the one record or
//! none, handed on as a filter would, no chain, no offset word, so no loop header carries the fold's carry.
//! The build op on the build's own pipeline is an identity step (the end stores the row).
use core::cell::Cell as VCell;
use std::rc::Rc;

use gravity_decompose::join::JoinDesc;
use gravity_decompose::{Op, Source};
use gravity_expr::emit::hash::emit_image_eq_at;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_program::LifeId;
use types_nodes::jointype::JoinType;

use mcx::{CollectIn, GrowIn, PgBox, PgRc, PgVec};

use crate::face::end::EndMech;
use crate::face::kinds::{Face, KindId};
use crate::ops::hashjoin::face::Swap;
use crate::face::step::{Expand, ExpandKind, StepMech, VolatileImpl};
use crate::frame::env::Env;
use crate::frame::frame::{Next, StepMaker};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{ExId, Px, WordKey};
use crate::frame::row::{Carry, Mask, RowVals};
use crate::frame::stop::{self, Exits, ResumeWord};
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::frame::iface::kind_impl;

use crate::ops::hashjoin::build::{region, A_RF, BITS, DBITS, DENSE, DIR, EMPTY, FORM, HI, LO, LOW, N, SLOTS};
use crate::ops::hashjoin::keys::{any_null, check_keys, image, key_cells, key_src, KeySrc};
use crate::ops::hashjoin::build_class::{feed, side_by, Side};
use crate::ops::hashjoin::build_side::Shape;
use crate::ops::hashjoin::walk::Rec;
pub(super) use crate::ops::hashjoin::walk::{later_set, rec_at, Hv};

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::JoinInner, &StepMaker(make))], ..crate::ops::NONE };

struct Pass;

impl StepMech for Pass {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        next(b, env, row)
    }

    fn keeps_rows(&self) -> bool {
        true
    }
}

pub(crate) struct Probe<'g> {
    pub(crate) sh: Rc<Shape>,
    pub(crate) keys: PgVec<'g, KeySrc>,
    pub(crate) qual: PgVec<'g, ExId>,
    pub(crate) node: i32,
    pub(crate) consumed: usize,
    pub(crate) unique: bool,
    pub(crate) offset: i32,
    pub(crate) life: LifeId,
    pub(crate) hv: VCell<Option<Hv>>,
    /// join.mult: the row is handed on once with its key's count (multiplicity.rs: a candidate at make, decided in
    /// `end_is`).
    pub(crate) mult: bool,
    /// D237: an empty build ends the pipeline before its first row on PG's build-first path, or when the ops before the
    /// probe raise nothing; otherwise rows run up to the probe and the first to reach it ends the pipeline, as PG's first
    /// outer fetch does.
    pub(crate) prefix_safe: bool,
    /// Op 14: the build's merge holds its error (join_build_class::defers): the first row to reach the probe raises it.
    pub(crate) defer: bool,
    /// The frame words its quals' volatile results take (`Ex::held`; `Expand::held`).
    pub(crate) held: u8,
    pub(crate) fill: Option<crate::ops::hashjoin::probe_fill::Fill>, // V4R-142: a catalog probe (probe_fill.rs)
    /// D824 (V4R-298): the build is lazy (`join_build_class::lazy`): while it is pending (ctx flag `FLAG_PENDING_SHIFT +
    /// consumed`) the first row to reach the probe stops the call with `BUILD_WAIT`, the offset word tagged `WAIT`.
    pub(crate) lazy: bool,
    /// M7 (R1-R6): INNER, LEFT, SEMI or ANTI (join.left / .semi / .anti delegate here). SEMI and ANTI stop at the first
    /// match (R2); LEFT and ANTI fill a miss (probe_fill.rs).
    pub(crate) jt: JoinType,
    /// lockrows lk-0100 (D2131): a RIGHT SEMI probe whose spooled drain a row lock reads (drain/spool_admit.rs `free_lock`)
    /// hands each build record on with the probe row at its first match, as PG's HJ_SCAN_BUCKET does, so the lock's
    /// REFERENCE junk of the probe side is that row's (EvalPlanQual refetches it); its drain reads no marked records.
    pub(crate) semi_hands: bool,
    /// M7-SPILL (V4R-359): the build may spill (join/spill_pair.rs): each window's spill words, and the rows of spilled
    /// partitions kept for the pair pipeline.
    kept: Option<PgRc<'g, crate::ops::hashjoin::spill::pair::Kept<'g>>>,
    kv: VCell<Option<(Value, Value)>>,
    /// D1904 (probe_calls.rs): a rerun set's probe over a kept table whose outer side is not quiet; `cm` the call's mode
    /// read at the window.
    calls: Option<crate::ops::hashjoin::probe_calls::Calls>,
    cm: VCell<Option<Value>>,
    /// D1902 (probe_calls.rs `Order`): a member probe over a member build on PG's cost path whose outer side is not quiet:
    /// each call's order follows hj_OuterNotEmpty.
    order: Option<crate::ops::hashjoin::probe_calls::Order>,
    /// D3373 (frame/iface `later_arm_probe`): a streamed union's later arm closes its call at the open over a built EMPTY
    /// table, as PG's join, ended by that table, never starts the arm.
    arm_close: bool,
    /// D2674 (lockrows rerun/epq/marks.rs `epq_probe`): a write's topmost outer-first INNER / SEMI probe stores 1 in this
    /// frame word (byte offset) at PG's points that set hj_OuterNotEmpty (`Order::at_row`'s); the driver reads it after an
    /// EvalPlanQual recheck's call.
    epq: Option<i32>,
    /// D2592 (probe_marks.rs): a rerun set's marking probe over a kept table clears the table's marks at each open.
    reset: Option<crate::ops::hashjoin::probe_marks::Reset>,
    /// D1899 / D2003: the leading entries of `qual` that are the hash clauses run per candidate (keys.rs `confirms`); the
    /// join qual follows them.
    pub(crate) confirmed: usize,
    /// D2696 (D2003 under a width): a RIGHT SEMI probe whose join qual may raise. PG skips that qual on a build row an
    /// earlier outer row matched (walk.rs `hand_on`); workers sharing the outer find the marks in no fixed order, so the
    /// qual would raise on pairs PG skips: the pipeline runs serial, in PG's outer order (`serial`). A raise-free qual's
    /// extra runs show only as calls, which are free (a VOLATILE call's count is free; D2703 (A)).
    ordered: bool,
    /// D3194 (join.rowfilter, strategies/rowfilter.rs): the frame word (byte offset) the probe's sample of its build's
    /// Bloom words counts in, when it tests them, and (D3323, join.rowfilter.cost) the one that counts its windows; the
    /// window's filter (`rowfilter::window`) in `rfl`.
    pub(crate) rf: Option<(i32, Option<i32>)>,
    pub(crate) rfl: VCell<Option<crate::ops::hashjoin::strategies::rowfilter::Live>>,
    /// D3553 (B, probe_share.rs): a LEFT / FULL probe lowers the steps after it once, for its hits and its miss, in a
    /// pipeline stacking `cost::compile::FILL_SHARE_MIN_PROBES` NULL-filling joins or more (`Gx::fills_share`).
    pub(crate) share: bool,
    /// D3168 (join.probe.staged, strategies/probe_stage.rs): a probe whose join drops its unmatched rows, keyed by its
    /// source's columns, with none of the per-call or per-row state a block's stages would run ahead of (a lazy or
    /// deferred build, a rerun call's mode or order, EvalPlanQual's word, a kept table's reset, a fill); `sg` its window's
    /// guard (`stage_window`).
    stages: bool,
    sg: VCell<Option<Value>>,
    /// Construct C2: the licences the probe's departures run under (`make`, `make_swapped`): the build's run in program
    /// order before the outer's first row (a Safe side's no-effects licence, op 14's held error, `Px::run_ahead`'s
    /// recheck licence or `Px::ahead_unchecked`'s), a rerun set member's build at every call (ahead_unchecked's), and
    /// each raising outer-side pipeline run before the build on PG's build-first path (run_ahead's, ahead_unchecked's
    /// or the cover's whole-read licence). Held, not read.
    #[allow(dead_code)]
    ahead: PgVec<'g, gravity_decompose::effects::Licence>,
}

/// V4R-512: build `b` or a pipeline feeding it locks rows (build_class.rs classes a lock as a VOLATILE call: PG runs the
/// Hash's subtree only after the outer's first row, HJ_BUILD_HASHTABLE, and a held error does not undo a lock).
fn locks(gx: &crate::graph::Gx<'_, '_>, b: usize) -> bool {
    let (p, lock) = (&gx.pipelines()[b], |x: usize| gx.pipelines()[x].ops.iter().any(|o| matches!(o, Op::Lock(_))));
    let src = if let Source::Breaker { slot } = p.source { Some(slot) } else { None };
    lock(b) || crate::ops::hashjoin::build_class::feed(gx, b, p.desc.consumes.iter().copied().chain(src)).into_iter().any(lock)
}

/// V4R-512, D1670: pipeline `k` reads a CTE (nest/cte.rs pulls it a row per miss) that locks, calls or may raise (its facts,
/// or its side as a build's is classed): PG's first outer fetch pulls a row of it before the join sees its empty build.
fn pulls(gx: &crate::graph::Gx<'_, '_>, k: usize) -> bool {
    let Source::Breaker { slot } = gx.pipelines()[k].source else { return false };
    let Some(q) = gx.life_of(k, slot).map(|l| l.0 as usize) else { return false };
    let cte = gx.graph.init_plans.iter().find(|i| i.pipeline as usize == q && matches!(i.link, gravity_decompose::ops::shared::subplan::SubLink::Cte));
    cte.is_some_and(|i| crate::effects::visible::hashjoin::init_loud(i.raise_free, i.volatility) || crate::ops::hashjoin::build_class::side(gx, q) != Side::Safe)
}

/// D237's prefix rule (`Probe::prefix_safe` before a fill type's, probe_fill.rs `Fill::of`) for probe `j` of pipeline `k`
/// over build `b`, `lazy` its build waiting for the first row: an empty build ends the pipeline at the window when this
/// holds, else at the first row that reaches the probe (`Env::full`). rerun/inner_probe.rs `needs_end` reads it for a nest
/// inner's probe, whose `Env::full` must end the re-open's walk (D2820).
pub(crate) fn prefix_safe_of(gx: &crate::graph::Gx<'_, '_>, k: usize, j: usize, b: usize, lazy: bool) -> bool {
    let p = &gx.pipelines()[k];
    let Some(Op::Join(JoinDesc::Probe { outer_first, .. })) = p.ops.get(j) else { return false };
    // the reads `make`'s `Calls` (D1904) and `Order` (D1902) are made by
    let calls = crate::ops::hashjoin::probe_kept::rescans(gx, k, j, b, lazy);
    let order = !lazy && crate::ops::hashjoin::probe_calls::orders(gx, k, j, b) || held_order(gx, k, j, b, lazy);
    // D824: a lazy build is pending until the first row reaches the probe: never an empty build before the first row;
    // D1337(H): an embedded inner's probe of a build its SubPlan's own rerun set holds, laid before the inner's window: rerun/sets.rs
    // `sub_set`, the test rerun/inner_probe.rs `ok` admits by, restated (the rerun home's files are private to it)
    // (`sub_set_index`: the copy whose member produces a slot the inner `k` reads, else the set at the SubPlan op's node,
    // else the plan's first); D1018: a nest's own set too (`nest_set_index`, restated: the copy at the nest's node, plan 0, the inner reads, else the node's first)
    let at = |q: usize, jq: usize| gx.pipelines()[q].node_ops.iter().rev().find(|n| n.first_op as usize <= jq).map_or(gx.pipelines()[q].node_id, |n| n.node_id);
    let reads = |r: &gravity_decompose::graph::Rerun| gx.pipelines()[k].desc.consumes.iter().any(|&s| gx.life_of(k, s).is_some_and(|l| r.pipelines.contains(&l.0)));
    let sub = |q: usize, jq: usize, plan_id: i32| {
        let of = |f: &dyn Fn(&gravity_decompose::graph::Rerun) -> bool| gx.graph.reruns.iter().find(|r| r.plan_id == plan_id && f(r));
        of(&reads).or_else(|| of(&|r| r.node_id == at(q, jq))).or_else(|| of(&|_| true))
    };
    let nest = |q: usize, jq: usize| {
        let of = |f: &dyn Fn(&gravity_decompose::graph::Rerun) -> bool| gx.graph.reruns.iter().find(|r| r.plan_id == 0 && r.node_id == at(q, jq) && f(r));
        of(&reads).or_else(|| of(&|_| true))
    };
    let own = k.checked_sub(gx.pipelines().len()).and_then(|i| gx.pipelines().inners().get(i).map(|x| x.0)).and_then(|(q, jq)| match gx.pipelines()[q].ops.get(jq) { Some(Op::SubPlan(d)) => sub(q, jq, d.plan_id), _ => nest(q, jq) });
    !lazy && !order && !(calls && *outer_first) && (!*outer_first || crate::effects::ops_raise_free(gx, &p.ops[..j]) && !pulls(gx, k) || own.is_some_and(|r| r.pipelines.contains(&(b as u16))))
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(p.node_id, |n| n.node_id);
    let Op::Join(JoinDesc::Probe { slot, clauses, keys, qual, unique, decls, outer_first, catalog, jointype, .. }) = &p.ops[j] else {
        return mcx::pg_box_dyn!(px.mcx(), Pass => dyn StepMech + 'g);
    };
    let jt = match *jointype {
        JoinType::JOIN_LEFT | JoinType::JOIN_SEMI | JoinType::JOIN_ANTI | JoinType::JOIN_RIGHT | JoinType::JOIN_FULL | JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT_ANTI => *jointype,
        JoinType::JOIN_INNER | JoinType::JOIN_UNIQUE_OUTER | JoinType::JOIN_UNIQUE_INNER => JoinType::JOIN_INNER,
    };
    check_keys(px.gx.facts, node, decls, keys.len());
    let Some(life) = gx.life_of(k, *slot) else { crate::refuse!(node, Reason::Bug { payload: "a probe without its build" }) };
    let Some(consumed) = px.consumed(k, *slot) else { crate::refuse!(node, Reason::Bug { payload: "a probe of a slot its pipeline does not consume" }) };
    let sh = crate::ops::hashjoin::build_side::shape(px, *slot, life.0 as usize);
    let mcx = px.mcx();
    let keys = keys.iter().zip(decls.iter()).map(|(n, d)| key_src(px, n, node, d)).collect_in(mcx);
    // D1899 (keys.rs `confirms`): the hash clauses run on each image-equal record ahead of the join qual, as PG's bucket scan
    let confirm = crate::ops::hashjoin::keys::confirms(gx, clauses);
    if let Some(why) = crate::ops::hashjoin::keys::bucket_refusal(p, j, clauses, decls) {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) });
    }
    // D2696: a RIGHT SEMI join qual that may raise depends on which outer row marks a build row first (a call does not:
    // the extra calls are free, D2703 (A))
    let ordered = jt == JoinType::JOIN_RIGHT_SEMI && qual.iter().any(|n| !crate::effects::raise_free(gx, n));
    let qual: PgVec<'g, ExId> = clauses.iter().filter(|_| confirm).chain(qual.iter()).map(|q| crate::step::expr::compile(px, q, None, node)).collect_in(mcx);
    let confirmed = if confirm { clauses.len() } else { 0 };
    if confirm {
        crate::ops::hashjoin::keys::pull_owed(px, k, clauses);
    }
    let offset = stop::resume_word(px, ResumeWord::StepOffset { k, j: crate::frame::px::key_j(j) });
    let keys = crate::ops::hashjoin::keys::held_by(px, keys, offset);
    // a nest inner's probe (k past the graph) hands its rows to the owner's continuation, never to an end: no `end_is`
    let mult = crate::ops::hashjoin::strategies::multiplicity::candidate(px, k, j, qual.is_empty(), &sh) && !*catalog && matches!(jt, JoinType::JOIN_INNER | JoinType::JOIN_LEFT) && k < gx.pipelines().len();
    crate::ops::hashjoin::strategies::dedup::candidate(px, k, j, jt, *catalog, &sh);
    crate::ops::hashjoin::strategies::dedup::keys(px, k, jt, qual.is_empty(), *catalog, &sh);
    let word = |px: &mut Px<'_, '_>, i: u8| px.frame_word(WordKey::Open { face: Face::Join, k, j: crate::frame::px::key_j(j), i }) as i32 * 8;
    // D3194: the build lays Bloom words for this probe (build_side.rs `A_RF`): its sample's frame word
    let rf = match sh.allow & A_RF != 0 && crate::ops::hashjoin::strategies::rowfilter::takes(gx, k, j) {
        true => Some((word(px, RF_WORD), crate::ops::hashjoin::strategies::rowfilter::costed().then(|| word(px, RF_WIN)))),
        false => None,
    };
    if rf.is_some() {
        fires::fire("join.rowfilter"); // shapecheck fire signal (D3194): a probe tests its build's Bloom words
    }
    // D237: on PG's outer-first path the build side runs only once a row reaches the probe
    // D2635: never op 14's eager deferral for a build over a recursive union `k` does not read (`endless`)
    let endless = *outer_first && crate::ops::hashjoin::build_class::endless(gx, life.0 as usize, k);
    let defer = *outer_first && !endless && crate::ops::hashjoin::build_class::defers(gx, life.0 as usize, k, j);
    let (mut lazy, mut group) = (false, None);
    // D1906 (probe_kept.rs): a member's probe over a kept build whose side calls waits for its first row, as PG's build call
    let kc = crate::lifetime::hashjoin::kept_road(gx, k, j, life.0 as usize);
    // D3014 build-once (decompose self_spool.rs `keyed`): a keyed nest whose inner is built from pipelines of its own (a
    // join's build, an aggregate's or a sort's emit) runs them only when the first outer row reaches the probe, as
    // PostgreSQL scans the Nest's inner only then: D824's lazy group, the producers first. A Safe side the group does not
    // admit builds in program order (it raises and acts nowhere), fired by name; any other side reads as below
    // D4051: a build that calls user code by the effects home's walk (`build_class::nonbuiltin_build`: a user function in its
    // keys, a domain whose CHECK body calls one) reads as one that calls on PG's outer-first path: D824's lazy build or a
    // refusal, never an eager build, which would run that code before the outer's first row
    let user = *outer_first && crate::ops::hashjoin::build_class::nonbuiltin_build(gx, k, j);
    let side = if user { Side::UserFn } else { crate::ops::hashjoin::build_class::side(gx, life.0 as usize) };
    // D4661: `side` leaves the build's keys to its probe (`own_side`); a key that may raise (`keyed_side`) runs in the build,
    // which PG's outer-first Hash runs only once a row reaches the probe. Op 14 holds its error where it defers; where it
    // does not (feeders read elsewhere too, a CTE's store), the side reads as a raising one: D824's lazy build, the recheck
    // licence or the named refusal below, never the Safe side's program order
    let side = if *outer_first && side == Side::Safe && !defer && crate::effects::visible::hashjoin::keyed_side(gx, life.0 as usize) == Side::Raises { Side::Raises } else { side };
    let once = *outer_first && *catalog && crate::ops::hashjoin::build_class::fed(gx, life.0 as usize);
    // D3227 B5: an eligible probe (decompose leaves a keyed nest wide only where no reader orders its rows) over a Safe
    // build builds in program order: a lazy group would pin the probe to the leader. PG never builds it under an empty
    // outer, which costs the build's work alone: a Safe side raises and acts nowhere
    let wide = matches!(p.desc.parallel, gravity_program::Parallel::Eligible) && side == Side::Safe;
    if once && !wide {
        group = crate::ops::hashjoin::build_class::lazy_group(px, life.0 as usize, k, consumed, *slot, &[]);
        lazy = group.is_some();
        fires::fire(if lazy { "nest.buildonce.lazy" } else { "nest.buildonce.eager" });
    } else if once {
        fires::fire("nest.buildonce.wide");
    }
    // Construct C2: the licence of each departure below (`Probe.ahead`)
    let mut ahead: PgVec<'g, gravity_decompose::effects::Licence> = PgVec::new_in(mcx);
    let bq = life.0 as usize;
    match (*outer_first, side) {
        // a Safe side built in program order raises and acts nowhere: it departs from nothing (the no-effects licence)
        _ if once && (lazy || side == Side::Safe) => {
            if !lazy {
                keep(px, bq, gravity_decompose::effects::unobservable(gravity_decompose::effects::Effects::NONE), &mut ahead);
            }
        }
        // expr.volatile's calls on a build side keep PG's outer-first order: the build waits for a row to reach the probe
        // (D824: a lazy build, run when the first row reaches the probe; otherwise op 14's deferral). D1906: a kept one
        // (`kc`) always takes the lazy build, or rerun_order refuses it
        (true, Side::UserFn) => {
            kind_impl::<VolatileImpl>(&node, KindId::ExprVolatile);
            group = if defer && !kc { None } else { crate::ops::hashjoin::build_class::lazy_or_copy(px, life.0 as usize, k, j, consumed, *slot) };
            lazy = group.is_some();
            // a self join of a CTE that calls: the CTE runs whole and once in either order (`shared_cte`), the build eager
            if !kc && !defer && !lazy && !crate::ops::hashjoin::build_class::shared_cte(gx, life.0 as usize, k, j) {
                let lock = "a row lock on a build side on PG's outer-first path whose build is not lazy (the Hash runs after the outer's first row; V4R-512)";
                let user_why = "a build side that calls user code on PG's outer-first path whose build is not lazy (D4051)";
                let why = if locks(gx, life.0 as usize) { lock } else if user { user_why } else { "a volatile build side on PG's outer-first path that is not deferred" };
                crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) });
            }
        }
        // D1906: a kept build whose calls the side class does not read (or reads as raising) waits for its first row too
        (true, _) if kc => {
            group = crate::ops::hashjoin::build_class::lazy_or_copy(px, life.0 as usize, k, j, consumed, *slot);
            lazy = group.is_some();
        }
        // D2635 (setop's blocker): a build over a recursive union `k` does not read may never end, and PG's empty outer never
        // runs its Hash: it builds lazily (D824) whatever its side, never eagerly (op 14's deferral, the recheck licence)
        (true, _) if endless => {
            group = crate::ops::hashjoin::build_class::lazy_or_copy(px, life.0 as usize, k, j, consumed, *slot);
            lazy = group.is_some();
            if !lazy {
                crate::refuse!(node, Reason::Shape { why: ShapeWhy("a hash build over a recursive union on PG's outer-first path that takes no lazy build (PG never builds it under an empty outer, and the union may never end; D2635)") })
            }
        }
        // gen ct-0084 / ct-0086: a build side that can raise and whose error op 14 cannot hold (a fed build: a CTE's pulled
        // set, an inner join's build) builds lazily too, D824's group: its error then raises at the first row that reaches
        // the probe, where PG's HJ_BUILD_HASHTABLE runs the Hash. v5 errorhold (worklist item 4, `Batch::Statement`): where
        // no lazy group admits it, it runs eagerly, where PG's empty outer may never run it, under a recheck licence
        // (`Px::run_ahead`): its error before any row left re-runs the statement in plan order, where the licence is denied.
        // There, or where no lazy group and no recheck takes it, `DECISIONS.md` 4 (D) b runs it eagerly unchecked
        // (`Px::ahead_unchecked`: no acting or user code, its error tagged); denied, this name answers.
        // D3717: a side PostgreSQL 18.6 runs without raising (`side_by`'s quiet reading: a Result's constant unnests into a
        // CTE store) raises and acts nowhere, so it builds in program order as a Safe side does: no error to hold
        (true, Side::Raises) if !defer && !crate::ops::hashjoin::build_class::shared_cte_raises(gx, life.0 as usize, k, j) && crate::ops::hashjoin::build_class::side_by(gx, life.0 as usize, usize::MAX, false) != Side::Safe => {
            group = crate::ops::hashjoin::build_class::lazy_or_copy(px, life.0 as usize, k, j, consumed, *slot);
            lazy = group.is_some();
            let recheck = if lazy { None } else { px.run_ahead(life.0 as usize, gravity_decompose::effects::Effects::RAISES) };
            if !lazy && recheck.is_none() {
                // D3296: a keyed nest's build (a catalog probe, decompose self_spool.rs `keyed`) that is not fed reads as
                // raising when its inner holds a nest step (`own_side`, D2107): D824's lazy group as a fed one takes it
                // above, PostgreSQL scanning the Nest's inner only at the first outer row
                group = if *catalog { crate::ops::hashjoin::build_class::lazy_group(px, life.0 as usize, k, consumed, *slot, &[]) } else { None };
                lazy = group.is_some();
                let tagged = if lazy { None } else { unchecked(px, life.0 as usize) };
                if !lazy && tagged.is_none() {
                    crate::refuse!(node, Reason::Shape { why: ShapeWhy("a build side that can raise on PG's outer-first path and whose error is not held (a fed build, an outer join)") })
                }
                ahead.extend_in(tagged);
                if lazy {
                    fires::fire("nest.buildonce.lazy_raises");
                }
            }
            ahead.extend_in(recheck);
        }
        // op 14's deferral (`build_class::defers`): the Raises side builds in program order, its error held for the first
        // row that reaches the probe, where PG's HJ_BUILD_HASHTABLE runs the Hash: the held error's licence (it acts
        // nowhere and calls no user code)
        (true, Side::Raises) if defer => {
            keep(px, bq, gravity_decompose::effects::held_error(gravity_decompose::effects::Effects::RAISES, false, None).ok(), &mut ahead);
        }
        // D3717: a side PostgreSQL runs without raising builds in program order as a Safe side does (the no-effects
        // licence); a shared CTE's raising build (`shared_cte_raises`, the CTE run whole in either order) holds D1's below
        (true, Side::Raises) if !crate::ops::hashjoin::build_class::shared_cte_raises(gx, bq, k, j) => {
            let l = gravity_decompose::effects::unobservable(gravity_decompose::effects::Effects::NONE);
            // Construct C2 (b): the quiet reading that brought the build here (`side_by`, the arm above) read it with what
            // feeds it, so the feeders run in program order under the same licence
            if let Some(l) = &l {
                hold_fed(px, bq, l);
            }
            keep(px, bq, l, &mut ahead);
        }
        (true, Side::Safe) => keep(px, bq, gravity_decompose::effects::unobservable(gravity_decompose::effects::Effects::NONE), &mut ahead),
        // D1 exact (PgFailsToo), PG 18.6 CteScanNext: the shared CTE computes each row once for whichever reader pulls it
        // first, and the outer-first join's pulls run it whole in either order: the build's first error is the CTE's, PG's.
        // The proof checks that the join's outer first-fetch chain reads the same CTE and that PG surely runs the join
        // (`Proof::pg_fails_too_shared`): under a one-time qual, or with the CTE on the build side alone, it holds none
        (true, Side::Raises) => {
            // what the build runs that may raise is the CTE's plan (`shared_cte`: its own ops and other feeders are quiet);
            // `pipeline_effects` reads a hash build's end as unknown
            let cte = match gx.pipelines()[bq].source {
                Source::Breaker { slot } => gx.life_of(bq, slot).map(|l| l.0 as usize),
                _ => None,
            };
            let plan = cte.and_then(|c| gx.graph.init_plans.iter().find(|i| i.pipeline as usize == c && matches!(i.link, gravity_decompose::ops::shared::subplan::SubLink::Cte)));
            let proof = |i: &gravity_decompose::graph::InitPlan<'_>| gx.graph.stmt.and_then(|s| gravity_decompose::effects::Proof::pg_fails_too_shared(s, node, i.plan_id, px.mcx()));
            let l = plan.and_then(|i| proof(i).and_then(|p| gravity_decompose::effects::exact(gravity_decompose::effects::Effects::of_init_plan(i), p).ok()));
            keep(px, bq, l, &mut ahead);
        }
        _ => {}
    }
    let defer = defer && !lazy;
    // on the build-first path an empty build means PG never runs the outer: the producers the BuildSide mask names are
    // skipped over an empty build (D276: HOSTS' after_pipeline, DRIVER's run_empty); the rest run, and must be quiet
    let outer = crate::ops::hashjoin::build_class::outer(gx, life.0 as usize, k, j);
    match (*outer_first, outer.rest) {
        (false, Side::UserFn) => {
            kind_impl::<VolatileImpl>(&node, KindId::ExprVolatile);
            crate::refuse!(node, Reason::Shape { why: ShapeWhy("an outer side run before the build with a volatile call (an empty build never runs it)") })
        }
        // v5 errorhold (`Batch::Statement`): each of the rest that may raise runs ahead of PG's order (an empty build never runs
        // it) under a recheck licence (`Px::run_ahead`): its error before any row left re-runs the statement in plan order,
        // where the licence is denied and `DECISIONS.md` 4 (D) b runs it unchecked (`Px::ahead_unchecked`), or where its
        // raises are PG's statement's too (D2212, effects/visible/cover.rs); denied, this name answers
        (false, Side::Raises) => {
            for &x in outer.raising.iter() {
                let l = match px.run_ahead(x, gravity_decompose::effects::Effects::RAISES) {
                    Some(l) => Some(l),
                    None => match unchecked(px, x) {
                        Some(l) => Some(l),
                        None => ahead_covered(px, k, x, node).unwrap_or_else(|| crate::refuse!(node, Reason::Shape { why: ShapeWhy("an outer side run before the build that can raise on PG's build-first path (an empty build never runs it)") })),
                    },
                };
                ahead.extend_in(l);
            }
        }
        _ => {}
    }
    // D1902 / D1904 / D1906 (M8-RERUN, fail closed): a rerun set's member runs one build order where PG decides per call
    if let Some(why) = crate::lifetime::hashjoin::rerun_order(gx, k, j, life.0 as usize, lazy) {
        // `DECISIONS.md` 4 (D) b: a member build PG skips at a call whose outer has no row runs at every call, ahead of PG,
        // when it may raise but neither acts nor calls user code and the outer side raises nothing (no error PG's outer
        // would raise first is lost): unchecked (`Px::ahead_unchecked`), its error tagged
        let b = life.0 as usize;
        let raises = why == crate::lifetime::hashjoin::OUTER_FIRST_RAISES && side_by(gx, b, usize::MAX, false) == Side::Raises && side_by(gx, k, j, false) == Side::Safe;
        let tagged = if raises && crate::ops::hashjoin::build_class::quiet_statement(gx) { unchecked(px, b) } else { None };
        let Some(tagged) = tagged else { crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) }) };
        ahead.push_in(tagged);
    }
    // D1904: a later call over the kept table scans its whole outer; on the outer-first path a call's first row says it built
    let calls = crate::ops::hashjoin::probe_kept::rescans(gx, k, j, life.0 as usize, lazy).then(|| crate::ops::hashjoin::probe_calls::Calls::make(px, k, j, *outer_first));
    // D1902: a member build's emptiness ends the call where PG's per-call order would
    // D1902 (held build): or a member build op 14 holds, under a quiet outer side reading a parameter (`held_order`)
    let held_build = held_order(gx, k, j, life.0 as usize, lazy);
    let order = (!lazy && crate::ops::hashjoin::probe_calls::orders(gx, k, j, life.0 as usize) || held_build).then(|| crate::ops::hashjoin::probe_calls::Order::make(px, k, j, held_build));
    let arm_close = order.is_none() && crate::frame::iface::later_arm_probe(gx, k, j);
    // D3609 (rerun/params_key.rs `input_cells`): a walk evaluating a computed key at its seek is served under `Order`'s
    // close only (a lazy build's walk seeks at every call, where PG's build-first call over an empty table never fetches)
    // D1016 row 2: or a nest inner's `Calls` (probe_calls.rs `nest_rescans`), its build-first call closing over an EMPTY
    // table at the open (`Calls::closes`), its outer-first call seeking first as PG's first outer fetch
    if order.is_none() && px.seek_keyed.contains(&k) && !(calls.is_some() && !lazy && crate::ops::hashjoin::probe_calls::nest_rescans(gx, k, j, life.0 as usize)) {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy(crate::rerun::params_key::SEEK_KEY_PROBE) });
    }
    let epq = (crate::rerun::epq::marks::epq_probe(gx, k) == Some(j)).then(|| px.frame_word(WordKey::Open { face: Face::Join, k, j: crate::frame::px::key_j(j), i: crate::rerun::epq::marks::EPQ_WORD }) as i32 * 8);
    let prefix_safe = prefix_safe_of(gx, k, j, life.0 as usize, lazy);
    if lazy {
        group.into_iter().flatten().for_each(|x| px.lazy.push((x, k))); // alloc: Program
    }
    let held = qual.iter().map(|&q| px.placed(q).held.len()).sum::<usize>() as u8;
    let (fill, prefix_safe) = crate::ops::hashjoin::probe_fill::Fill::of(*catalog, *jointype, prefix_safe, held, crate::ops::hashjoin::keys::keys_quiet(gx, &p.ops[j]));
    // M7-SPILL B4a (V4R-395): INNER, LEFT, SEMI and ANTI keep a spilled partition's rows for the pair pipeline;
    // B4c-2 (V4R-414): so do the drained forms (RIGHT, FULL, RIGHT SEMI, RIGHT ANTI)
    let spills = (matches!(jt, JoinType::JOIN_INNER | JoinType::JOIN_LEFT | JoinType::JOIN_SEMI | JoinType::JOIN_ANTI) || crate::ops::hashjoin::spill::pair::drain_of(jt) != 0) && !*catalog;
    let drain = crate::ops::hashjoin::spill::pair::drain_of(jt);
    // D2717: over a lazy build (it runs whole at the first row) and op 14's deferred one (its error raises at the first row,
    // before the kept exit) too
    let kept = spills.then(|| crate::ops::hashjoin::spill::pair::pair(px, k, j, life.0 as usize, *slot, consumed, fill.is_some(), drain)).flatten();
    // D2592 (probe_marks.rs): a member's marking probe over a table kept across the set's re-runs clears its marks at each
    // open, as PG's rescan resets the kept table's match flags; one that may spill refuses by name
    let reset = crate::ops::hashjoin::probe_marks::over_kept(gx, k, j, life.0 as usize).then(|| match kept {
        Some(_) => crate::refuse!(node, Reason::Shape { why: ShapeWhy(crate::ops::hashjoin::probe_marks::KEPT_SPILL) }),
        None => crate::ops::hashjoin::probe_marks::Reset,
    });
    let semi_hands = jt == JoinType::JOIN_RIGHT_SEMI && gx.spooled_drain(k).is_some_and(|(d, _)| gx.semi_hands(d));
    let share = fill.is_some_and(|f| f.left) && gx.fills_share(k);
    let quiet = !lazy && !defer && calls.is_none() && order.is_none() && epq.is_none() && reset.is_none() && fill.is_none() && !semi_hands && !*catalog;
    let stages = quiet && k < gx.pipelines().len() && keys.iter().all(|x| matches!(x, KeySrc::Col(..))) && crate::ops::hashjoin::strategies::probe_stage::takes(jt);
    mcx::pg_box_dyn!(px.mcx(), Probe { sh, keys, qual, node, consumed, unique: *unique, offset, life, hv: VCell::new(None), mult: mult && !lazy, prefix_safe, defer, held, fill, lazy, jt, semi_hands, kept, kv: VCell::new(None), calls, cm: VCell::new(None), order, arm_close, epq, reset, confirmed, ordered, rf, rfl: VCell::new(None), share, stages, sg: VCell::new(None), ahead } => dyn StepMech + 'g)
}

/// D3194: the Join face's frame word (by op) that a probe's row filter sample counts in (0 / 1 unused, 2 / 3
/// probe_calls.rs `Calls`, 4 / 5 its `Order`, 6 rerun/epq marks.rs `EPQ_WORD`, 8 `RF_WIN`).
const RF_WORD: u8 = 7;
/// D3323 (join.rowfilter.cost): the probe's window count, which windows sample its filter again.
const RF_WIN: u8 = 8;

/// D1018 (regress join#346 / #347): probe `i` of nest inner `ik` reads build `b` of the nest's own rerun set (plan 0 at the
/// nest's node: rerun/inner_probe.rs's `nest` road, restated here as `make`'s `own` is), whose side may raise. The set
/// re-runs the build at every re-open before the inner's first window, and op 14 holds its error for the probe's first
/// row (build_class.rs `defers`; the driver's rerun call scope, as D1902's held member build: a re-open whose probe side
/// has no row drops it). PG 18.6 rebuilds the Hash at every rescan (its child's chgParam is set): a fill type fetches its
/// outer's first row before it builds at every rescan; on the cost path (INNER / SEMI outer-first) hj_OuterNotEmpty
/// decides, which cannot show when the probe side reads no executor parameter (empty at every rescan or at none); the
/// build-first path builds at the inner's first pull, as the re-open does. So: the build unfed (no breaker source,
/// nothing consumed: its every error is the Hash's, which op 14 holds), its source a Result, a heap or index scan or
/// VALUES, its ops filters, projections, set functions and nests over such a scan, every node builtin code
/// (effects/user_fn.rs `held_node`; a nest's parameters are its outer row's columns); the probe side before the probe `side_by` Safe (a
/// LIMIT 0 over the set refuses first: rerun/inner_limit.rs D1903). A VOLATILE builtin's calls (random) are free in
/// number, so the eager build is read by its raises only.
/// D4101 (k2): or a fed build (`fed_held_side`: a semi join's unique-ified inner, a HashAggregate over a scan whose
/// projection may raise, both members of the set): op 14 holds a feeder's error under the build (driver
/// `Entry::defer_fed`), and the rerun call drops a member's held error at its next call (`spill.call_begin`), so each
/// re-open holds and drops its own, as PG builds again at every rescan whose probe side hands a row.
pub(crate) fn nest_held(gx: &crate::graph::Gx<'_, '_>, ik: usize, i: usize, b: usize) -> bool {
    let ps = gx.pipelines();
    let Some(((k, jk), ip)) = ik.checked_sub(ps.len()).and_then(|x| ps.inners().get(x).copied()) else { return false };
    let Some(Op::Join(JoinDesc::Probe { jointype, outer_first, catalog: false, .. })) = ip.ops.get(i) else { return false };
    let fill = matches!(jointype, JoinType::JOIN_LEFT | JoinType::JOIN_ANTI);
    let jt = fill || matches!(jointype, JoinType::JOIN_INNER | JoinType::JOIN_UNIQUE_OUTER | JoinType::JOIN_UNIQUE_INNER | JoinType::JOIN_SEMI);
    let node = ps[k].node_ops.iter().rev().find(|n| n.first_op as usize <= jk).map_or(ps[k].node_id, |n| n.node_id);
    let own = !matches!(ps[k].ops.get(jk), Some(Op::SubPlan(_))) && gx.graph.reruns.iter().find(|r| r.plan_id == 0 && r.node_id == node).is_some_and(|r| r.pipelines.contains(&(b as u16)));
    if !jt || !own {
        return false;
    }
    (held_side(gx, b) || fed_held_side(gx, b)) && side_by(gx, ik, i, false) == Side::Safe && (fill || !*outer_first || !reads_param(gx, ik, i)) && crate::ops::hashjoin::build_class::quiet_statement(gx)
}

/// D3378 (D1764 / D1765): probe `i` of nest inner `ik` reads a KEPT build `b` (no rerun set holds it or its slot: built
/// once, ahead of the owner) whose side may raise, on PG's outer-first path (a LEFT / ANTI probe always; an INNER / SEMI
/// one the planner costs that way). PG 18.6 builds the kept table at the first rescan whose probe side hands a row
/// (hj_OuterNotEmpty stays false until a build) and keeps it (one batch, no changed parameter): op 14 holds the build's
/// error in the table for the probe's first row, at whichever re-open first reaches it, and an empty nest outer or a
/// probe side empty at every re-open never raises it, as PG never builds. The build side as `nest_held`'s (`held_side`:
/// unfed, a Result / heap / index scan / VALUES, builtin code only), the probe side before the probe Safe. The eager
/// build's calls are free in number (count-free), so it is read by its raises only. Build-first INNER / SEMI probes
/// (PG builds at the re-open's first pull) are not taken.
pub(crate) fn kept_held(gx: &crate::graph::Gx<'_, '_>, ik: usize, i: usize, b: usize) -> bool {
    let ps = gx.pipelines();
    let Some((_, ip)) = ik.checked_sub(ps.len()).and_then(|x| ps.inners().get(x).copied()) else { return false };
    let Some(Op::Join(JoinDesc::Probe { jointype, outer_first: true, catalog: false, slot, .. })) = ip.ops.get(i) else { return false };
    let jt = matches!(jointype, JoinType::JOIN_LEFT | JoinType::JOIN_ANTI | JoinType::JOIN_INNER | JoinType::JOIN_SEMI);
    let free = !gx.graph.reruns.iter().any(|r| gravity_decompose::graph::slot_has(&r.slots, *slot) || r.pipelines.contains(&(b as u16)));
    jt && free && (held_side(gx, b) || fed_held_side(gx, b)) && side_by(gx, ik, i, false) == Side::Safe && crate::ops::hashjoin::build_class::quiet_statement(gx)
}

/// D3937 (c-random seed 7 rnd-2597: a nest inner probing a chain of inner hash joins whose middle build probes with the
/// float join filter `(i.i_disc - p2.p_weight) >= 1e3`): `kept_held`'s build side fed by other builds' tables. Op 14 holds
/// a fed build's error with its feeders' (strategies/rtfilter.rs `held_feeders`: the pipelines just before it, read by
/// nothing else, no user function; the driver's `Entry::defer_fed`), so the build's own probes of those tables may raise
/// as its filters may: each probe not a keeping one (no RIGHT / FULL drain), its keys, clauses and join filter, the
/// build's filters, projections and keys builtin code (`held_node`), its source a Result or a heap scan. PG 18.6 builds
/// the whole chain at the inner's first rescan whose probe side hands a row, as it does an unfed build.
/// D4101 (k2): or a build whose source is a held feeder's emit (a hashed group's: the build reads the group table its
/// feeder filled, the feeder just before it and read by it alone, `held_feeders`), its ops over the group's records.
fn fed_held_side(gx: &crate::graph::Gx<'_, '_>, b: usize) -> bool {
    let p = &gx.pipelines()[b];
    let Some((Op::Join(JoinDesc::Build { keys, .. }), rest)) = p.ops.split_last() else { return false };
    let held = |n: types_nodes::Node<'_>| crate::effects::user_fn::held_node(gx, n);
    let keeping = |t: &JoinType| matches!(t, JoinType::JOIN_RIGHT | JoinType::JOIN_FULL | JoinType::JOIN_RIGHT_ANTI);
    let step = |op: &Op<'_>| match op {
        Op::Filter(n) | Op::Gate(n) => held(*n),
        Op::Project(l) => l.iter().all(held),
        Op::Join(JoinDesc::Probe { keys, clauses, qual, jointype, catalog: false, .. }) => !keeping(jointype) && keys.iter().chain(clauses.iter()).chain(qual.iter()).all(held),
        _ => false,
    };
    let emit = |s: u8| gx.life_of(b, s).is_some_and(|l| (l.0 as usize) < b && matches!(gx.end_kind(l.0 as usize), crate::graph::EndKind::Breaker(gravity_abi::breaker::BreakerKind::HashAgg | gravity_abi::breaker::BreakerKind::Distinct)));
    let src = match p.source { Source::Const | Source::Range { .. } => true, Source::Breaker { slot } => emit(slot), _ => false };
    !p.desc.consumes.is_empty() && src && rest.iter().all(step) && keys.iter().all(held) && crate::ops::hashjoin::strategies::rtfilter::held_feeders(gx, b)
}

/// `nest_held`'s build side: unfed (no breaker source, nothing consumed: its every error is the Hash's, which op 14
/// holds), its source a Result, a heap or index scan or VALUES, its ops filters, projections, set functions and nests over
/// such a scan, every node builtin code (`held_node`).
fn held_side(gx: &crate::graph::Gx<'_, '_>, b: usize) -> bool {
    let p = &gx.pipelines()[b];
    let Some((Op::Join(JoinDesc::Build { keys, .. }), rest)) = p.ops.split_last() else { return false };
    let held = |n: types_nodes::Node<'_>| crate::effects::user_fn::held_node(gx, n);
    let src = |s: &Source<'_>| match s {
        Source::Const | Source::Range { .. } => true,
        Source::Index { keys, recheck, .. } => keys.iter().all(|k| held(k.value)) && recheck.iter().all(held),
        Source::Values { lists, .. } => lists.iter().all(|l| l.as_list().is_some_and(|r| r.iter().all(held))),
        _ => false,
    };
    let plain = |op: &Op<'_>| match op {
        Op::Filter(n) | Op::Gate(n) => held(*n),
        Op::Project(l) => l.iter().all(held),
        _ => false,
    };
    let step = |op: &Op<'_>| match op {
        Op::Srf(d) => d.tlist.iter().all(held) && d.functions.iter().all(|f| held(f.node) && f.args.iter().all(held)),
        Op::Join(JoinDesc::Nest { inner, qual, .. }) => src(&inner.source) && inner.ops.iter().all(|o| plain(o)) && qual.iter().all(held),
        op => plain(op),
    };
    let unfed = p.desc.consumes.is_empty() && !matches!(p.source, Source::Breaker { .. });
    unfed && src(&p.source) && rest.iter().all(step) && keys.iter().all(held)
}

/// `nest_held` where op 14 must hold: the build side is not raise-free with the bound params (a filter, projection or key
/// that may raise, a set function the effects home does not read quiet, a nest) or its source is no Result or plain
/// scan. A raise-free build (the calm road's: rerun/inner_probe.rs) keeps its reading, no deferral bit.
pub(crate) fn nest_holds(gx: &crate::graph::Gx<'_, '_>, ik: usize, i: usize, b: usize) -> bool {
    let bound = |id: i32| gx.graph.params.iter().any(|x| x.0 == id);
    let rf = |n: types_nodes::Node<'_>| crate::effects::raise_free_each(gx, n, &bound);
    let p = &gx.pipelines()[b];
    let calm = matches!(p.source, Source::Const | Source::Range { .. })
        && p.ops.iter().all(|op| match op {
            Op::Filter(n) | Op::Gate(n) => rf(*n),
            Op::Project(l) => l.iter().all(rf),
            Op::Join(JoinDesc::Build { keys, .. }) => keys.iter().all(rf),
            op => crate::effects::srf_quiet(gx, op, &bound),
        });
    !calm && (nest_held(gx, ik, i, b) || kept_held(gx, ik, i, b))
}

/// The nest inner probing build `b`'s slot `slot` (a build only a nest inner probes: build_side.rs's `probe` none), as
/// (inner, op).
pub(crate) fn nest_probe_of(gx: &crate::graph::Gx<'_, '_>, b: usize, slot: u8) -> Option<(usize, usize)> {
    let n = gx.pipelines().len();
    gx.pipelines().inners().into_iter().enumerate().find_map(|(e, (_, p))| {
        let j = p.ops.iter().position(|op| matches!(op, Op::Join(JoinDesc::Probe { slot: s, .. }) if *s == slot))?;
        (gx.life_of(n + e, slot) == Some(LifeId(b as u16))).then_some((n + e, j))
    })
}

/// D1902 (held build, probe_calls.rs `orders_held`): probe `j` of pipeline `q` over member build `b` takes `Order` with
/// the build's held error: the build is not lazy, the outer side reads an executor parameter (`reads_param`: without one
/// it is empty at every call or at none, the plain held reading), and `orders_held` holds.
pub(crate) fn held_order(gx: &crate::graph::Gx<'_, '_>, q: usize, j: usize, b: usize, lazy: bool) -> bool {
    !lazy && reads_param(gx, q, j) && crate::ops::hashjoin::probe_calls::orders_held(gx, q, j, b)
}

/// D1902 (errorhold, fail closed): whether probe `j` of pipeline `q`'s outer side, `side_by`'s reach (its source and ops
/// before `j`; its feeders but the builds of its probes from `j` on, their groups too), reads an executor parameter or
/// holds a node the walk cannot read (`exec_param`); a source but a scan, a breaker's read, a Result or a btree index's
/// keys, and an op but a filter, gate, projection, probe, build, spool write (D2731) or set function (a union's too), read as one.
pub(crate) fn reads_param(gx: &crate::graph::Gx<'_, '_>, q: usize, j: usize) -> bool {
    use crate::rerun::spool::form::exec_param as rp;
    let p = &gx.pipelines()[q];
    let src = if let Source::Breaker { slot } = p.source { Some(slot) } else { None };
    let mut all: PgVec<'_, (usize, usize)> = feed(gx, q, p.desc.consumes.iter().copied().chain(src).filter(|s| !p.ops.iter().skip(j).any(|op| matches!(op, Op::Join(JoinDesc::Probe { slot, .. }) if slot == s)))).into_iter().map(|x| (x, usize::MAX)).collect_in(gx.mcx);
    all.push_in((q, j));
    let op = |op: &Op<'_>| match op {
        Op::Filter(n) | Op::Gate(n) => rp(*n),
        Op::Project(l) => l.iter().any(rp),
        Op::Join(JoinDesc::Probe { keys, qual, .. }) => keys.iter().chain(qual.iter()).any(rp),
        Op::Join(JoinDesc::Build { keys, .. }) => keys.iter().any(rp),
        Op::Join(JoinDesc::Spool) => false, // D2731: a spool write reads no expression
        Op::Srf(d) => d.tlist.iter().any(rp) || d.functions.iter().any(|f| rp(f.node) || f.args.iter().any(rp)),
        _ => true,
    };
    all.into_iter().any(|(x, upto)| {
        let p = &gx.pipelines()[x];
        let source = match &p.source {
            Source::Range { .. } | Source::Breaker { .. } | Source::Const => false,
            Source::Index { keys, recheck, .. } => keys.iter().any(|k| rp(k.value)) || recheck.iter().any(rp),
            _ => true,
        };
        // a feeder's group is the outer's (`q`'s own folds the joined rows)
        let group = x != q && gx.group_at(x).is_some_and(|g| g.keys.iter().any(|k| rp(k.expr.node())) || g.aggs.iter().any(|a| rp(a.node) || a.filter.is_some_and(rp)));
        source || group || p.ops[..upto.min(p.ops.len())].iter().any(op)
    })
}

/// D1906 (fail closed): kept build `b`'s side holds what the side class reads as Raises without seeing into it
/// (`effects::hides_calls`, a window included).
pub(crate) fn opaque(gx: &crate::graph::Gx<'_, '_>, b: usize) -> bool {
    crate::effects::hides_calls(gx, b, false)
}

/// `DECISIONS.md` 4 (D) b: pipeline `x` runs ahead of PG unchecked (`Px::ahead_unchecked`, which reads user code by the side
/// class) only when nothing in it or feeding it hides a call from that class (`effects::hides_calls`, a WindowAgg read into):
/// the licence granted, `None` where denied.
fn unchecked(px: &mut Px<'_, '_>, x: usize) -> Option<gravity_decompose::effects::Licence> {
    if crate::effects::hides_calls(px.gx, x, true) {
        return None;
    }
    px.ahead_unchecked(x, gravity_decompose::effects::Effects::RAISES)
}

/// Construct C2 (b): licence `l`, asked for build `b` with what feeds it, stamped on those feeders for the tracer
/// (`Px::hold`; build_class.rs `feed`). Nothing while the tracer is off.
fn hold_fed(px: &mut Px<'_, '_>, b: usize, l: &gravity_decompose::effects::Licence) {
    if !crate::frame::program::stamps_on() {
        return;
    }
    let ps = px.gx.pipelines();
    let src = if let Source::Breaker { slot } = ps[b].source { Some(slot) } else { None };
    for q in crate::ops::hashjoin::build_class::feed(px.gx, b, ps[b].desc.consumes.iter().copied().chain(src)) {
        px.hold(q, l);
    }
}

/// Construct C2: licence `l`, minted here for build `b`'s run in program order, stamped for the tracer (`Px::hold`) and
/// kept in `ahead` (`Probe.ahead`).
fn keep(px: &mut Px<'_, '_>, b: usize, l: Option<gravity_decompose::effects::Licence>, ahead: &mut PgVec<'_, gravity_decompose::effects::Licence>) {
    if let Some(l) = l {
        px.hold(b, &l);
        ahead.push_in(l);
    }
}

/// D2212: outer-side pipeline `x` of probe pipeline `k` runs before the build where the effects home's cover takes it
/// (effects/visible/cover.rs): `None` where it does not, else the licence it runs under. A cover resting on the root's
/// client reading every row serves the program to such a reader alone, under the whole-read licence
/// (`shared::whole::read`, over `x`'s effects with its feeders'); one resting on PG's statement running `x` to its end
/// holds the exact licence of hash join `node` (D1, `Proof::pg_fails_too`), held on `x` for the tracer.
fn ahead_covered(px: &mut Px<'_, '_>, k: usize, x: usize, node: i32) -> Option<Option<gravity_decompose::effects::Licence>> {
    let root = crate::effects::visible::cover::raises_covered(px.gx, px.arena, x)?;
    if !root {
        // D1 exact (PgFailsToo), PG 18.6 ExecHashJoin: an empty build skips this outer, but the statement runs it to its end
        // elsewhere (a whole reader, a FULL merge, an InitPlan: cover.rs `whole`) or fails, so each raise is PG's too. The
        // cover holds only in a statement that neither acts nor calls user code, so what `x` runs at most raises
        let l = px.gx.graph.stmt.and_then(|s| gravity_decompose::effects::Proof::pg_fails_too(s, node)).and_then(|p| gravity_decompose::effects::exact(gravity_decompose::effects::Effects::RAISES, p).ok());
        if let Some(l) = &l {
            px.hold(x, l);
        }
        return Some(l);
    }
    let own = crate::effects::pipeline_effects(px.gx, px.arena, x, true);
    Some(crate::ops::shared::whole::read(px, k, own))
}

/// join.buildside's swapped probe: program pipeline `s.probe` probes graph pipeline `s.build`'s rows under its build op's
/// keys into the probe side's records, in place of op `s.at`. The joined row's OUTER_VAR columns are the record's cells
/// and its INNER_VAR columns the arriving row's (`Probe::lower`); the guard proved every op on both sides raise-free, so an
/// empty build ends the pipeline before its first row.
pub(crate) fn make_swapped<'g>(px: &mut Px<'g, '_>, s: &Swap) -> PgBox<'g, dyn StepMech + 'g> {
    let gx = px.gx;
    let (bp, p) = (&gx.pipelines()[s.build], &gx.pipelines()[s.probe]);
    let node = p.node_ops.iter().rev().find(|n| n.first_op as usize <= s.at).map_or(p.node_id, |n| n.node_id);
    let (Some(Op::Join(JoinDesc::Build { keys, decls, .. })), Op::Join(JoinDesc::Probe { slot, qual, .. })) = (bp.ops.last(), &p.ops[s.at]) else {
        crate::refuse!(node, Reason::Bug { payload: "a swap without its build and probe" })
    };
    let Some(consumed) = p.desc.consumes.iter().position(|x| x == slot) else { crate::refuse!(node, Reason::Bug { payload: "a probe of a slot its pipeline does not consume" }) };
    let sh = crate::ops::hashjoin::build_side::shape_swapped(px, s);
    let mcx = px.mcx();
    let keys = keys.iter().zip(decls.iter()).map(|(n, d)| key_src(px, n, node, d)).collect_in(mcx);
    let qual: PgVec<'g, ExId> = qual.iter().map(|q| crate::step::expr::compile(px, q, None, node)).collect_in(mcx);
    let offset = stop::resume_word(px, ResumeWord::StepOffset { k: s.probe, j: crate::frame::px::key_j(s.at) });
    let (life, held) = (LifeId(s.build as u16), qual.iter().map(|&q| px.placed(q).held.len()).sum::<usize>() as u8);
    // Construct C2: the swapped build reads PG's outer side (graph `s.probe`'s source and its ops before the probe) whole
    // before the first row; with no effects it departs from nothing (the no-effects licence). One that acts (a VOLATILE
    // call the swap's raise-free guard admits) holds none
    let swapped = {
        use gravity_decompose::effects::{of_ops, of_source, unobservable, Cx, PLAIN};
        let cx = Cx::without_stmt(gx.facts, px.arena);
        unobservable(of_source(&cx, &p.source, &PLAIN).and(of_ops(&cx, &p.ops[..s.at], &PLAIN)))
    };
    let mut ahead = PgVec::new_in(mcx);
    keep(px, s.build, swapped, &mut ahead);
    mcx::pg_box_dyn!(mcx, Probe { sh, keys, qual, node, consumed, unique: false, offset, life, hv: VCell::new(None), mult: false, prefix_safe: true, defer: false, held, fill: None, lazy: false, jt: JoinType::JOIN_INNER, semi_hands: false, kept: None, kv: VCell::new(None), calls: None, cm: VCell::new(None), order: None, arm_close: false, epq: None, reset: None, confirmed: 0, ordered: false, rf: None, rfl: VCell::new(None), share: false, stages: false, sg: VCell::new(None), ahead } => dyn StepMech + 'g)
}

impl Probe<'_> {
    /// D2674: a row reached the probe at one of PG's points that set hj_OuterNotEmpty (`epq`).
    fn epq_row(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        if let Some(w) = self.epq {
            let one = b.iconst(Type::I64, 1);
            env.store_word(b, w, one);
        }
    }

    /// The build header's loads from its region `r`, past its `form` byte (`window`'s, or the row's where its frame calls
    /// no `window`: D2361).
    fn view(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, r: Value, form: Value) -> Hv {
        let at = |b: &mut FunctionBuilder<'_>, o: usize| {
            let off = b.load(Type::I64, r, o as i32);
            b.ptr_add(r, off, 1, 0)
        };
        let (dir, slots) = (at(b, DIR), at(b, SLOTS));
        let bits = b.load_zext(Type::I32, r, BITS as i32);
        let (lo, hi) = (b.load(Type::I64, r, LO as i32), b.load(Type::I64, r, HI as i32));
        let d = b.int_bin(IntBinOp::Sub, hi, lo);
        let span = b.int_bin(IntBinOp::Add, d, env.one);
        let ri = b.conv(ConvOp::PtrToInt, Type::I64, r);
        let hdr = b.iconst(Type::I64, self.sh.base() as i64);
        let recs = b.int_bin(IntBinOp::Add, ri, hdr);
        let n = b.load(Type::I64, r, N as i32);
        let planes = (self.dense() && crate::ops::hashjoin::strategies::dense_bits::may(&self.sh)).then(|| crate::ops::hashjoin::strategies::dense_bits::planes(b, dir, slots, &self.sh));
        Hv { form, dir, bits, lo, span, slots, recs, base: ri, n, planes }
    }

    /// SEMI and ANTI: the walk ends at the first record that passes the join qual (R2).
    pub(crate) fn first_only(&self) -> bool {
        matches!(self.jt, JoinType::JOIN_SEMI | JoinType::JOIN_ANTI)
    }

    /// D3168: the staged body's guard this window over the header `hv`: the HASH form whose directory and records outgrow
    /// L2 (prefetch.rs `above_l2`), no partition of a spilling build out (its rows leave before the probe). `None`: the
    /// probe never stages here (an instrumented frame counts each row's nodes as it walks; S0: a plain function).
    fn stage_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, hv: &Hv) -> Option<Value> {
        if !self.stages || crate::frame::instr::bounded() || env.unstaged {
            return None;
        }
        Some(self.stage_guard(b, env.zero, (hv.form, hv.bits, hv.n), self.kv.get()))
    }

    /// D3168: the guard over the built table's form, directory bits and records, and the spilled partitions `kv` (none
    /// when the build never spills): the BUILT size, never an estimate.
    fn stage_guard(&self, b: &mut FunctionBuilder<'_>, zero: Value, (form, bits, n): (Value, Value, Value), kv: Option<(Value, Value)>) -> Value {
        let hash = b.iconst(Type::I64, crate::ops::hashjoin::build::HASH as i64);
        let is_h = b.icmp(IntCond::Eq, form, hash);
        let eight = b.iconst(Type::I64, 8);
        let mut bytes = b.int_bin(IntBinOp::Shl, eight, bits);
        if crate::ops::hashjoin::strategies::probe_stage::foot() {
            let rec = b.iconst(Type::I64, self.sh.rec.get() as i64);
            let recs = b.int_bin(IntBinOp::Mul, n, rec);
            bytes = b.int_bin(IntBinOp::Add, bytes, recs);
        }
        let big = crate::frame::prefetch::above_l2(b, bytes);
        let g = b.int_bin(IntBinOp::And, is_h, big);
        match kv {
            Some((sp, _)) => {
                let none = b.icmp(IntCond::Eq, sp, zero);
                b.int_bin(IntBinOp::And, g, none)
            }
            None => g,
        }
    }

    /// The carry at `l_join`. A kept exit reaches it past the walk: no memo entry made after the probe's start dominates it.
    fn at_join<'e>(&self, env: &mut Env<'e, '_>, c: Carry<'e>) -> Carry<'e> {
        if self.kept.is_some() {
            env.memo.clear();
        }
        c
    }
}

impl StepMech for Probe<'_> {
    /// D3168 S0: a window's cached values belong to one function; a pipeline lowered twice starts each with none.
    fn declare(&mut self, _pb: &mut gravity_ir::builder::ProgramBuilder) {
        self.hv.set(None);
        self.kv.set(None);
        self.cm.set(None);
        self.rfl.set(None);
        self.sg.set(None);
    }

    fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("join.inner offset", Base::Words, self.offset, Type::I64, 0));
        if let Some(c) = &self.calls {
            c.open_words(w);
        }
        if let Some(o) = &self.order {
            o.open_words(w);
        }
    }

    /// D2592: the call that opens clears a kept table's marks, before any window (an outer with no rows included).
    fn prologue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _x: &Exits) {
        if let Some(r) = &self.reset {
            r.prologue(b, env, self.consumed);
        }
        if let Some(c) = &self.calls {
            c.prologue(b, env, self.consumed);
        }
    }

    /// D3609 (probe_calls.rs `Order::closes`): a build-first call over an EMPTY table closes the call at the open, before
    /// the source opens. D3373 (`arm_close`): a later arm's call over a built EMPTY table closes there too.
    fn closes(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> Option<Value> {
        match &self.order {
            Some(o) => Some(o.closes(b, env, self.consumed)),
            None => self.calls.as_ref().and_then(|c| c.closes()).or_else(|| self.arm_close.then(|| crate::ops::hashjoin::probe_calls::built_empty(b, env, self.consumed))),
        }
    }

    /// D2592: a probe clearing a kept table's marks runs its run in one call (probe_marks.rs): each claim's call opens.
    /// D2696: a RIGHT SEMI probe whose join qual may raise (`ordered`) reads the outer in PG's order.
    fn serial(&self) -> bool {
        gravity_program::step_serial(&gravity_program::StepShape::HashProbe { lazy: false, clears_marks: self.reset.is_some(), ordered: self.ordered }).is_some()
    }

    /// The build's header; an empty build (no row with a non-NULL key) ends the pipeline here.
    fn window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        let r = region(b, env, Some(self.consumed));
        let form = b.load_zext(Type::I8, r, FORM as i32);
        debug_assert_eq!(EMPTY, 0);
        let mode = self.calls.as_ref().map(|c| c.at_window(b, env, r));
        self.cm.set(mode);
        if let Some(o) = &self.order {
            o.at_window(b, env, form, x.l_done);
        }
        if self.prefix_safe {
            let mut empty = b.icmp(IntCond::Eq, form, env.zero);
            if let (Some(c), Some(m)) = (&self.calls, mode) {
                let ends = c.ends_empty(b, m);
                empty = b.int_bin(IntBinOp::And, empty, ends);
            }
            let l_go = b.create_block(&[]);
            b.cond_br(empty, (x.l_done, &[]), (l_go, &[]));
            b.switch_to(l_go);
        }
        let hv = self.view(b, env, r, form);
        self.hv.set(Some(hv));
        self.kv.set(self.kept.as_ref().map(|k| k.words(b, r)));
        // D3194: the build's Bloom words this window, its sample folded (an instrumented frame counts the rows the block
        // test drops at the probe's boundary: frame instr.rs `count_dropped`)
        self.rfl.set(self.rf.map(|w| crate::ops::hashjoin::strategies::rowfilter::window(b, env, (r, form, hv.dir, hv.bits), w)));
        self.sg.set(self.stage_window(b, env, &hv));
    }

    /// M7-SPILL: the owner's kept rows written at the claim's close.
    fn epilogue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _x: &Exits) {
        if let Some(k) = &self.kept {
            k.flush(b, env);
        }
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        // D2361: a row lowered before the frame's window loop (frame.rs `copy_entry`: an EPQ recheck's stored COPY row, under
        // UPDATE / DELETE .. FROM a subquery) loads the header itself; its empty build takes the per-row exit below
        if self.lazy {
            crate::ops::hashjoin::probe_wait::wait(b, env, row, self.offset, self.consumed);
        }
        let (hv, kv, viewed) = match self.hv.get() {
            Some(hv) => (hv, self.kv.get(), true),
            None => {
                let r = region(b, env, Some(self.consumed));
                let form = b.load_zext(Type::I8, r, FORM as i32);
                (self.view(b, env, r, form), self.kept.as_ref().map(|k| k.words(b, r)), false)
            }
        };
        let mode = self.calls.as_ref().map(|c| match self.cm.get() {
            Some(m) if viewed => m,
            _ => c.mode_now(b, env),
        });
        if let Some(c) = &self.calls {
            c.at_row(b, env);
        }
        if (!self.prefix_safe || !viewed) && self.fill.is_none() {
            let mut empty = b.icmp(IntCond::Eq, hv.form, env.zero);
            if let (Some(c), Some(m)) = (&self.calls, mode) {
                let ends = c.ends_empty(b, m);
                empty = b.int_bin(IntBinOp::And, empty, ends);
            }
            let (l_first, l_go) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(empty, (l_first, &[]), (l_go, &[]));
            b.switch_to(l_first);
            if let Some(o) = &self.order {
                o.at_row(b, env);
            }
            self.epq_row(b, env);
            env.full(b, &row.carry, row.next);
            b.switch_to(l_go);
        }
        if self.defer {
            crate::ops::hashjoin::probe_wait::raise_failed(b, env, hv.form);
        }
        let mcx = env.mcx;
        let ctypes: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(mcx);
        let l_join = b.create_block(&ctypes[..]);
        // a fill's miss path lowers `next` again: no read below dominates it, and the image reads its keys as non-NULL
        let kept = self.fill.is_some_and(|f| f.left || f.anti).then(|| env.memo.set_aside());
        let (l_miss, empty) = crate::ops::hashjoin::probe_fill::entry(b, env, self.fill, (hv.form, self.offset), l_join, &ctypes[..], &row.carry.params());
        // D1898: an outer side that keeps no NULL key (PG's `keep_nulls` = HJ_FILL_OUTER: LEFT, FULL, ANTI) stops at its first
        // NULL key, as `ExecBuildHash32Expr`'s strict steps do
        let carry = row.carry.params();
        let stop = (!self.fill.is_some_and(|f| f.left || f.anti)).then_some((l_miss, &carry[..]));
        let cells = key_cells(b, env, self.node, row, &self.keys, stop);
        // D1904: a later call's EMPTY kept table (a build call's empty table left above): the row's keys ran, and it misses
        if self.calls.is_some() {
            let empty = b.icmp(IntCond::Eq, hv.form, env.zero);
            let l_go = b.create_block(&[]);
            b.cond_br(empty, (l_miss, &row.carry.params()), (l_go, &[]));
            b.switch_to(l_go);
        }
        // D1908 (`Fill::keys`): an EMPTY build misses after keys that may raise or call, as PG's HJ_FILL_OUTER scan hashes every
        // outer row once (`entry` sent a row re-entered after its fill went on, a stop inside it or a later step, to the fill)
        if let Some(miss) = [empty, any_null(b, &cells)].into_iter().flatten().reduce(|x, y| b.int_bin(IntBinOp::Or, x, y)) {
            let l_go = b.create_block(&[]);
            b.cond_br(miss, (l_miss, &row.carry.params()), (l_go, &[]));
            b.switch_to(l_go);
        }
        if let Some(o) = &self.order {
            o.at_row(b, env);
        }
        self.epq_row(b, env);
        let mut lx = env.lx(b, self.node);
        let img = image(&mut lx, self.node, &cells, &self.keys);
        env.memo = lx.memo;
        if img.words.words.len() != self.sh.words.get() as usize {
            crate::refuse!(self.node, Reason::Bug { payload: "a probe image wider than its build's" });
        }
        // producerchain D: a hit's record has this image, in reach of a write's EvalPlanQual redo (epq_ir.rs `Redo::Probe`)
        env.redo_probes.push_in((self.node, mcx::to_vec_in(&img.words.words[..], env.mcx)));
        // M7-SPILL: a row of a spilled partition leaves for the pair pipeline (join/spill_pair.rs), its carry unchanged
        // M7-PROBE (B4a): after the NULL-key miss, before the fill decision. Past `l_join` no read takes the image's memo
        // entries (the kept edge's): the memo is cleared there (`at_join`)
        if let (Some(k), Some(kv)) = (&self.kept, kv) {
            k.exit(b, env, row, &img, kv, l_join);
        }
        if self.mult {
            let c = self.lower_mult(b, env, &hv, &img, row, next, (l_join, l_miss), kept);
            env.redo_probes.pop();
            return self.at_join(env, c);
        }
        if env.single.is_some() && env.single == env.step {
            let c = self.lower_single(b, env, &hv, &img, row, next, l_join);
            env.redo_probes.pop();
            return self.at_join(env, c);
        }
        if self.share {
            let c = self.lower_shared(b, env, &hv, &img, row, next, (l_join, l_miss), kept);
            return self.at_join(env, c);
        }
        let mut wtypes: PgVec<'_, Type> = mcx::pg_vec![in mcx; Type::I64, Type::I64];
        wtypes.extend_from_slice_in(&ctypes[..]);
        let l_walk = b.create_block(&wtypes[..]);
        // DBITS / DENSE windows take the single-row body (`single_guard`), unless another probe's guard chose it
        let (p, exact) = self.first(b, env, &hv, &img, true);
        let start: PgVec<'_, Value> = [p, env.zero].into_iter().chain(row.carry.params()).collect_in(mcx);
        let back = later_set(b, env).map(|x| b.conv(ConvOp::Zext, Type::I64, x));
        let l_end = b.create_block(&ctypes[..]);
        if self.first_only() {
            // R2: a SEMI / ANTI row re-entered after its row went on, but not for a later step (D688), is done
            let (off, l_go) = (env.load_word(b, self.offset), b.create_block(&[]));
            let set = b.icmp(IntCond::Ne, off, env.zero);
            let done = back.map_or(set, |x| {
                let none = b.icmp(IntCond::Eq, x, env.zero);
                b.int_bin(IntBinOp::And, set, none)
            });
            b.cond_br(done, (l_end, &row.carry.params()), (l_go, &[]));
            b.switch_to(l_go);
        }
        b.br(l_walk, &start[..]);
        // the chain: (record address or 0, matches so far, carry)
        b.switch_to(l_walk);
        b.mark_loop_header(l_walk);
        env.bounded.push_in(l_walk);
        let ps = b.block_params(l_walk);
        let (p, m, carry) = (ps[0], ps[1], Carry::from_params(&ps[2..], env.mcx));
        let l_rec = b.create_block(&[]);
        let none = b.icmp(IntCond::Eq, p, env.zero);
        b.cond_br(none, (l_end, &carry.params()), (l_rec, &[]));
        b.switch_to(l_rec);
        let rec = b.conv(ConvOp::IntToPtr, Type::Ptr, p);
        let link = b.load(Type::I64, rec, self.sh.link_off());
        let low = b.iconst(Type::I64, LOW as i64);
        let n1 = b.int_bin(IntBinOp::And, link, low);
        let np = rec_at(b, env, &hv, n1, self.sh.rec.get());
        let nz = b.icmp(IntCond::Eq, n1, env.zero);
        let next_p = b.select(nz, env.zero, np);
        let eq = emit_image_eq_at(b, &img.words, rec, 0);
        let eq = b.int_bin(IntBinOp::Or, eq, exact);
        let m1 = b.int_bin(IntBinOp::Add, m, env.one);
        let on = |p: Value, m: Value, c: &Carry| [p, m].into_iter().chain(c.params()).collect_in(mcx);
        let (l_hit, l_emit) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(eq, (l_hit, &[]), (l_walk, &on(next_p, m, &carry)[..]));
        b.switch_to(l_hit);
        let off = env.load_word(b, self.offset);
        let off = back.map_or(off, |x| b.int_bin(IntBinOp::Sub, off, x));
        let resumed = b.icmp(IntCond::Slt, m, off);
        b.cond_br(resumed, (l_walk, &on(next_p, m1, &carry)[..]), (l_emit, &[]));
        b.switch_to(l_emit);
        // rec-0013 / 0014 / 0015: a chain walk is bounded by the table's records (`env.bounded`), but one probe row may meet
        // a whole generation's matches, each running the steps after the probe (a CYCLE path's ROW(..) = ANY(path)): the
        // frame's once-a-window check then answered a statement_timeout tens of seconds late. PG's ExecHashJoin checks for
        // interrupts at each match it returns; so does each match here, before it goes on
        crate::frame::cancel::check(b, env.ctx, env.l_cancel);
        let c2 = self.hand_on(b, env, &hv, row, (Rec::At(rec, link), &carry), Some(m1), (l_walk, &on(next_p, m1, &carry)[..]), &mut *next);
        match self.unique || self.first_only() {
            true => b.br(l_end, &c2.params()),
            false => b.br(l_walk, &on(next_p, m1, &c2)[..]),
        }
        b.switch_to(l_end);
        let ce = Carry::from_params(b.block_params(l_end), env.mcx);
        if let Some(m) = kept {
            env.memo.restore(m);
        }
        env.redo_probes.pop();
        let ce = crate::ops::hashjoin::probe_fill::miss(b, env, (self.fill.map(|f| (f, self.offset, &*self.sh, self.life)), self.node), (ce, l_miss), row, next);
        env.store_word(b, self.offset, env.zero);
        b.br(l_join, &ce.params());
        b.switch_to(l_join);
        self.at_join(env, Carry::from_params(b.block_params(l_join), env.mcx))
    }

    fn expand(&self) -> Option<Expand> {
        (!self.mult).then_some(Expand { kind: ExpandKind::Chain, offset: self.offset, held: self.held })
    }

    /// A lazy build's wait tags the offset word and stops at the row (probe_wait.rs `wait`).
    fn wait_offset(&self) -> Option<i32> {
        self.lazy.then_some(self.offset)
    }

    /// V4R-27: a build laid `DBITS` or `DENSE` holds each key once (a duplicate lays another form), so the window's rows
    /// take the single-row body (`lower_single`).
    fn single_guard(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Option<Value> {
        // a marking probe (the right family, M7-DRAIN) reads only the KeyHash form: a match marks its record's address
        let marks = matches!(self.jt, JoinType::JOIN_RIGHT | JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT_ANTI);
        let hv = self.hv.get().filter(|_| !self.mult && self.dense() && self.fill.is_none() && !marks)?;
        let (db, de) = (b.iconst(Type::I64, DBITS as i64), b.iconst(Type::I64, DENSE as i64));
        let (x, y) = (b.icmp(IntCond::Eq, hv.form, db), b.icmp(IntCond::Eq, hv.form, de));
        Some(b.int_bin(IntBinOp::Or, x, y))
    }

    /// D3194: the window's build filter is armed (rowfilter.rs `window`): the frame's filtered body (`Env::rowfilter`).
    fn rowfilter_guard(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Option<Value> {
        self.rfl.get().map(|f| f.armed)
    }

    /// D3168: the window's guard of the staged body (`stage_window`).
    fn batch_guard(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Option<Value> {
        self.sg.get()
    }

    /// D3168 S0: the probe stages its rows where its build outgrows L2 (`stage_window`), so its pipeline lowers a staged
    /// function beside its plain one (frame.rs `pipeline_fns_in`).
    fn stages(&self) -> bool {
        self.stages && !crate::frame::instr::bounded()
    }

    /// D3168 S0: at the plain function's entry, the staged body's guard over the sealed build's header (the driver runs a
    /// probe pipeline once its build is done: the built size, read where `window` reads it); no region yet runs on.
    fn stage_entry(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, (l_staged, l_plain): (Block, Block)) {
        let r = region(b, env, Some(self.consumed));
        let ri = b.conv(ConvOp::PtrToInt, Type::I64, r);
        let (none, l_hdr) = (b.icmp(IntCond::Eq, ri, env.zero), b.create_block(&[]));
        b.cond_br(none, (l_plain, &[]), (l_hdr, &[]));
        b.switch_to(l_hdr);
        let form = b.load_zext(Type::I8, r, FORM as i32);
        let bits = b.load_zext(Type::I32, r, BITS as i32);
        let n = b.load(Type::I64, r, N as i32);
        let kv = self.kept.as_ref().map(|k| k.words(b, r));
        let g = self.stage_guard(b, env.zero, (form, bits, n), kv);
        b.cond_br(g, (l_staged, &[]), (l_plain, &[]));
    }

    /// D3168: the block's stages (strategies/probe_stage.rs).
    #[allow(clippy::type_complexity)]
    fn block_batch(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: (&RowVals, &dyn Fn(i16) -> Option<i16>), rows: crate::frame::env::BatchRows, fill: &mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env, &[i16]) -> bool) -> Option<crate::frame::env::Batched> {
        crate::ops::hashjoin::strategies::probe_stage::block(self, b, env, row, rows, fill)
    }

    /// D3194: the block's rows that pass the filter before the late lanes decode (rowfilter.rs `block`).
    fn block_rowfilter(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, j: Value, mask: Mask, fill: &mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env, &[i16]) -> bool) -> Option<Value> {
        let ((w, _), f) = (self.rf?, self.rfl.get()?);
        crate::ops::hashjoin::strategies::rowfilter::block(self, b, env, (row, j, mask), (w, &f), fill)
    }

    fn end_is(&mut self, end: &dyn EndMech) {
        self.mult = crate::ops::hashjoin::strategies::multiplicity::decide(&self.sh, self.mult, end);
        crate::ops::hashjoin::strategies::dedup::decide(&self.sh, self.mult, end);
    }
}

#[cfg(test)]
mod tests {
    use gravity_abi::breaker::{Breaker, BreakerKind};
    use gravity_decompose::graph::{slot_set, Bound, Rerun as SetDecl};
    use gravity_decompose::join::JoinDesc;
    use gravity_decompose::lifetime::{KeptBy, KeptRec};
    use gravity_decompose::{Am, NoFacts, Op, PipelineGraph, PipelineNode, Source};
    use gravity_program::{Arena, ArenaVec, End, Pipeline, SiteDesc};
    use mcx::MemoryContext;
    use types_nodes::jointype::JoinType::{self, JOIN_ANTI, JOIN_INNER, JOIN_LEFT, JOIN_SEMI};
    use types_nodes::list::NodeList;
    use types_nodes::primnodes::{FuncExpr, ScalarArrayOpExpr};
    use types_nodes::Node;

    use crate::lifetime::hashjoin::{rerun_order, KEPT_BUILD_CALLS, KEPT_OUTER_RAISES, KEPT_UNKNOWN, OUTER_FIRST_RAISES};
    use crate::graph::Gx;

    /// The keep rule's answer decompose records for a Hash outside a nest's set (keep.rs: kept unless multi-batch).
    const HASH_KEPT: KeptBy = KeptBy::Usual { marks: false, why: "a kept hash table whose fill acts (PG rebuilds a multi-batch table)" };

    /// D1902 / D1904 / D1906: p0 builds a hash table (slot 0) from a scan, or from a VALUES (`build`: it may raise); p1 probes it
    /// (`jt`; `first`: PG's outer-first path) from a scan, or from a VALUES (`outer`), and folds a plain aggregate (slot 1);
    /// p2, the owner, reads it. The set of the nest at node 5 holds p1 and, unless `kept`, p0; `lazy`: p0 is p1's lazy
    /// build (D824). The Shape p1's probe names, if any.
    fn order(build: bool, outer: bool, jt: JoinType, first: bool, kept: bool, lazy: bool) -> Option<&'static str> {
        case(build, 0, outer, None, jt, first, kept, lazy, false)
    }

    /// `order` over a quiet outer source whose filter before the probe may raise (int4div: division by zero).
    fn filtered(build: bool, jt: JoinType, first: bool) -> Option<&'static str> {
        case(build, 0, false, None, jt, first, false, false, true)
    }

    /// `order` with p1's probe keyed (D1904): by an outer column (`Some(false)`: quiet), or by int4div of it by itself
    /// (`Some(true)`: it may raise, division by zero).
    fn keyed(build: bool, outer: bool, key: Option<bool>, jt: JoinType, first: bool, kept: bool, lazy: bool) -> Option<&'static str> {
        case(build, 0, outer, key, jt, first, kept, lazy, false)
    }

    /// `order` with `calls` on p0 (D1906): 1 a filter calling a user function; what the side class reads as Raises without
    /// seeing into it (main 09:54Z): 2 a LIMIT whose count is an expression, 3 a streamed group (D1071), 5 a filter whose
    /// user operator runs under ANY; seen into, no call: 4 a constant LIMIT, 6 int4eq under ANY.
    fn order_with(build: bool, calls: u8, outer: bool, jt: JoinType, first: bool, kept: bool, lazy: bool) -> Option<&'static str> {
        case(build, calls, outer, None, jt, first, kept, lazy, false)
    }

    /// `order` with `order_with`'s `calls`, `keyed`'s `key` and `filtered`'s filter (`filter`); a kept p0 has the keep
    /// rule's answer for a Hash (`HASH_KEPT`).
    #[allow(clippy::too_many_arguments)]
    fn case(build: bool, calls: u8, outer: bool, key: Option<bool>, jt: JoinType, first: bool, kept: bool, lazy: bool, filter: bool) -> Option<&'static str> {
        case_by(build, calls, outer, key, jt, first, kept, lazy, filter, kept.then_some(HASH_KEPT))
    }

    /// `case` with `by` the keep rule's answer for p0 at the set's re-open (None: no answer recorded).
    #[allow(clippy::too_many_arguments)]
    fn case_by(build: bool, calls: u8, outer: bool, key: Option<bool>, jt: JoinType, first: bool, kept: bool, lazy: bool, filter: bool, by: Option<KeptBy>) -> Option<&'static str> {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil()));
        let keys: &NodeList<'_> = key.map_or(nil, |raises| {
            let m = Box::leak(Box::new(mcx::MemoryContext::new("d1904"))).mcx();
            let v = types_nodes::Node::mk_var(m, 1, 1, 23, -1, 0, 0).unwrap();
            let div = types_nodes::primnodes::FuncExpr { funcid: 154, funcresulttype: 23, args: NodeList::make2(m, v, v).unwrap(), ..Default::default() };
            &*Box::leak(Box::new(NodeList::make1(m, if raises { types_nodes::Node::mk(m, div).unwrap() } else { v }).unwrap()))
        });
        let r = gravity_program::Reason::NoDrivingSite;
        let src = |raises: bool| if raises { Source::Values { scanrelid: 2, lists: nil } } else { Source::Range { site: SiteDesc::none_in(a), am: Am::Pgrcolumnar, scanrelid: 1, relid: 16384 } };
        let into = |kind, slot| Pipeline::serial(-1, End::Breaker(Breaker::sized(kind, 64).in_slot(slot)), r);
        let mut g = PipelineGraph::new(a);
        let mut p0 = PipelineNode::new(a, 7, src(build), into(BreakerKind::HashBuild, 0));
        if calls > 0 {
            let m = Box::leak(Box::new(MemoryContext::new("d1906"))).mcx();
            let v = Node::mk_var(m, 1, 1, 23, -1, 0, 0).unwrap();
            p0.push(match calls {
                1 => Op::Filter(Node::mk(m, FuncExpr { funcid: 16390, funcresulttype: 16, args: NodeList::make1(m, v).unwrap(), ..Default::default() }).unwrap()),
                2 => Op::Limit { count: Bound::Expr(v), offset: Bound::All, ties: None },
                3 => Op::GroupRun(0),
                4 => Op::Limit { count: Bound::Const(5), offset: Bound::All, ties: None },
                // `x op ANY (x)`: a user operator (5, D1871's kind), int4eq (6)
                c => Op::Filter(Node::mk(m, ScalarArrayOpExpr { opno: if c == 5 { 16400 } else { 96 }, opfuncid: if c == 5 { 16401 } else { 65 }, hashfuncid: 0, negfuncid: 0, useOr: true, inputcollid: 0, args: NodeList::make2(m, v, v).unwrap(), location: -1 }).unwrap()),
            });
        }
        g.pipelines.push(p0);
        let mut p = PipelineNode::new(a, 6, src(outer), into(BreakerKind::PlainAgg, 1).consuming_one(0));
        if filter {
            let m = Box::leak(Box::new(MemoryContext::new("d1902"))).mcx();
            let v = Node::mk_var(m, 1, 1, 23, -1, 0, 0).unwrap();
            p.push(Op::Filter(Node::mk(m, FuncExpr { funcid: 154, funcresulttype: 23, args: NodeList::make2(m, v, v).unwrap(), ..Default::default() }).unwrap()));
        }
        p.push(Op::Join(JoinDesc::Probe { slot: 0, jointype: jt, clauses: nil, keys, qual: nil, unique: false, marks: false, decls: ArenaVec::new_in(a), inner_node: 7, outer_first: first, catalog: false }));
        g.pipelines.push(p);
        g.pipelines.push(PipelineNode::new(a, 5, Source::Breaker { slot: 1 }, Pipeline::serial(-1, End::Sink, r).consuming_one(1)));
        let (mut pipelines, mut slots) = (ArenaVec::new_in(a), [0u64; 4]);
        if !kept {
            pipelines.push(0u16);
        }
        pipelines.push(1u16);
        slot_set(&mut slots, 0);
        slot_set(&mut slots, 1);
        g.reruns.push(SetDecl { node_id: 5, plan_id: 0, pipelines, slots });
        if let Some(by) = by {
            g.kept_by.push(KeptRec { node_id: 5, plan_id: 0, pipeline: 0, by });
        }
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        rerun_order(&gx, 1, filter as usize, 0, lazy)
    }

    /// The keep rule decides whether a build outside the set is kept: with no answer recorded (or the answer that the
    /// closing node has no plan node of its own) the probe refuses by KEPT_UNKNOWN before any timing rule, on every path;
    /// PG's, the usual, the quiet and the unreached answers go on to the timing rules (a quiet pair serves).
    #[test]
    fn a_build_outside_the_set_the_keep_rule_does_not_show_kept_refuses() {
        for (jt, first) in [(JOIN_INNER, true), (JOIN_INNER, false), (JOIN_LEFT, true), (JOIN_SEMI, true)] {
            for lazy in [false, true] {
                assert_eq!(case_by(false, 0, false, None, jt, first, true, lazy, false, None), Some(KEPT_UNKNOWN), "no answer, {jt:?}");
                assert_eq!(case_by(false, 0, false, None, jt, first, true, lazy, false, Some(KeptBy::Open)), Some(KEPT_UNKNOWN), "open, {jt:?}");
            }
        }
        for by in [KeptBy::Pg { marks: true }, HASH_KEPT, KeptBy::Quiet, KeptBy::Unreached] {
            assert_eq!(case_by(false, 0, false, None, JOIN_INNER, true, true, false, false, Some(by)), None, "{by:?}");
        }
    }

    #[test]
    fn a_member_build_or_outer_side_whose_order_pg_decides_per_call_refuses_and_a_quiet_one_does_not() {
        // does not apply: a quiet build and a quiet outer (PG's order shows nothing), a build-first probe (PG builds first at
        // every call), a fill type's outer side (scanned whatever the table holds), a lazy build (D1700 / D1654's road)
        assert_eq!(order(false, false, JOIN_INNER, true, false, false), None, "quiet sides");
        assert_eq!(order(true, true, JOIN_INNER, false, false, false), None, "build-first");
        assert_eq!(order(false, true, JOIN_LEFT, true, false, false), None, "LEFT scans its outer at every call");
        assert_eq!(order(true, false, JOIN_LEFT, true, false, true), None, "a lazy LEFT build runs at the probe's first row");
        // D1902: an outer side that can raise on the cost path (a raising build: errorhold's test below)
        assert_eq!(order(false, true, JOIN_INNER, true, false, false), Some(OUTER_FIRST_RAISES), "INNER, a raising outer side");
        assert_eq!(order(false, true, JOIN_SEMI, true, false, false), Some(OUTER_FIRST_RAISES), "SEMI, a raising outer side");
        // a table kept across re-runs (not a member) is not D1902's: PG does not rebuild it per call
        assert_eq!(order(true, false, JOIN_INNER, true, true, false), None, "kept, a raising build");
    }

    #[test]
    fn a_member_probe_over_a_quiet_member_build_serves_a_raising_outer_filter_on_the_cost_path() {
        // probe_calls.rs `Order`: the call's order follows hj_OuterNotEmpty; the build's emptiness alone shows
        assert_eq!(filtered(false, JOIN_INNER, true), None, "member, INNER outer-first");
        assert_eq!(filtered(false, JOIN_SEMI, true), None, "member, SEMI outer-first");
        // still D1902's: a build that can raise (its error, held or not, is the order's too), a fill type's raising build
        assert_eq!(filtered(true, JOIN_INNER, true), Some(OUTER_FIRST_RAISES), "member, a raising build");
        assert_eq!(filtered(true, JOIN_LEFT, true), Some(OUTER_FIRST_RAISES), "member, LEFT, a raising build");
        // a raising outer source runs before PG's build-first call would have stopped: D1902's (the test above)
        assert_eq!(order(false, true, JOIN_INNER, true, false, false), Some(OUTER_FIRST_RAISES), "member, a raising source");
    }

    #[test]
    fn a_kept_table_a_member_probes_refuses_when_its_outer_side_is_not_quiet_unless_a_fill_type() {
        assert_eq!(order(false, true, JOIN_INNER, true, true, false), Some(KEPT_OUTER_RAISES), "kept, outer-first");
        assert_eq!(order(false, true, JOIN_INNER, false, true, false), Some(KEPT_OUTER_RAISES), "kept, build-first");
        assert_eq!(order(false, true, JOIN_SEMI, true, true, false), Some(KEPT_OUTER_RAISES), "kept, SEMI");
        // does not apply: a quiet outer side (a raising build is built once, as PG's first build), a fill type
        assert_eq!(order(true, false, JOIN_INNER, false, true, false), None, "kept, a raising build, a quiet outer side");
        assert_eq!(order(false, true, JOIN_LEFT, true, true, false), None, "kept, LEFT");
        assert_eq!(order(false, true, JOIN_ANTI, true, true, false), None, "kept, ANTI");
    }

    #[test]
    fn a_kept_table_a_member_probes_serves_a_raising_outer_hash_key_over_a_quiet_source() {
        // PG hashes every outer row's key after a kept-table rescan, over an empty table too: probe_calls.rs's later call
        // does so (a quiet source and feeders: they run at every call); a raising source stays D1904's (the test above)
        assert_eq!(keyed(false, false, Some(true), JOIN_INNER, true, true, false), None, "kept, INNER outer-first");
        assert_eq!(keyed(false, false, Some(true), JOIN_INNER, false, true, false), None, "kept, INNER build-first");
        assert_eq!(keyed(false, false, Some(true), JOIN_SEMI, true, true, false), None, "kept, SEMI");
        assert_eq!(keyed(false, true, Some(true), JOIN_INNER, false, true, false), Some(KEPT_OUTER_RAISES), "kept, a raising source too");
        // does not apply: a quiet key; a fill type (its keys over an empty table are D1908's, at the probe); a member's
        // keys (PG computes them over a non-empty rebuilt table only, as the set's re-run does)
        assert_eq!(keyed(false, false, Some(false), JOIN_INNER, true, true, false), None, "kept, a quiet key");
        assert_eq!(keyed(false, false, Some(false), JOIN_INNER, false, true, false), None, "kept, a quiet key, build-first");
        assert_eq!(keyed(false, false, Some(true), JOIN_LEFT, true, true, false), None, "kept, LEFT");
        assert_eq!(keyed(false, false, Some(true), JOIN_INNER, true, false, false), None, "member, INNER outer-first");
    }

    #[test]
    fn a_kept_table_whose_build_side_calls_refuses_on_any_join_type_and_path() {
        assert_eq!(order_with(false, 1, false, JOIN_INNER, true, true, false), Some(KEPT_BUILD_CALLS), "kept, INNER outer-first");
        assert_eq!(order_with(false, 1, false, JOIN_INNER, false, true, false), Some(KEPT_BUILD_CALLS), "kept, build-first");
        assert_eq!(order_with(false, 1, false, JOIN_LEFT, true, true, false), Some(KEPT_BUILD_CALLS), "kept, LEFT");
        assert_eq!(order_with(false, 1, false, JOIN_LEFT, true, true, true), Some(KEPT_BUILD_CALLS), "kept, a lazy LEFT build");
        // D1906 / D2713 (probe_kept.rs): a lazy INNER build on the outer-first path is served at any size (no batch bound)
        assert_eq!(order_with(false, 1, false, JOIN_INNER, true, true, true), None, "kept, a lazy INNER build");
        // a member build that calls is D1902's (re-built at every call, as PG's chgParam rescan)
        assert_eq!(order_with(false, 1, false, JOIN_INNER, true, false, false), Some(OUTER_FIRST_RAISES), "member, INNER outer-first");
        assert_eq!(order_with(false, 1, false, JOIN_INNER, false, false, false), None, "member, build-first");
    }

    #[test]
    fn a_kept_table_whose_build_side_holds_what_the_side_class_does_not_see_into_refuses() {
        // main 09:54Z: a LIMIT expression (a VOLATILE count), a streamed group: read as a call, fail closed
        assert_eq!(order_with(false, 2, false, JOIN_INNER, true, true, false), Some(KEPT_BUILD_CALLS), "kept, a LIMIT expression");
        assert_eq!(order_with(false, 2, false, JOIN_INNER, false, true, false), Some(KEPT_BUILD_CALLS), "kept, a LIMIT expression, build-first");
        assert_eq!(order_with(false, 3, false, JOIN_LEFT, true, true, false), Some(KEPT_BUILD_CALLS), "kept, a streamed group, LEFT");
        assert_eq!(order_with(false, 5, false, JOIN_INNER, false, true, false), Some(KEPT_BUILD_CALLS), "kept, a user operator under ANY");
        // does not apply: a constant LIMIT, a builtin operator under ANY (seen into: no call); a member build (D1902's)
        assert_eq!(order_with(false, 4, false, JOIN_INNER, true, true, false), None, "kept, a constant LIMIT");
        assert_eq!(order_with(false, 6, false, JOIN_INNER, true, true, false), None, "kept, int4eq under ANY");
        assert_eq!(order_with(false, 2, false, JOIN_INNER, true, false, false), Some(OUTER_FIRST_RAISES), "member, a LIMIT expression");
    }

    /// errorhold: p0 builds slot 0 from a VALUES (it may raise; op 14 holds it), p1 probes it (`jt`, outer-first) from a
    /// scan filtered by the boolean PARAM_EXEC 0 the nest at node 5 binds (quiet: a bound word), p2 reads p1's aggregate;
    /// the nest's set holds p0 and p1.
    fn param_outer(jt: JoinType) -> Option<&'static str> {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil()));
        let m = Box::leak(Box::new(MemoryContext::new("d1902"))).mcx();
        let r = gravity_program::Reason::NoDrivingSite;
        let into = |kind, slot| Pipeline::serial(-1, End::Breaker(Breaker::sized(kind, 64).in_slot(slot)), r);
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(PipelineNode::new(a, 7, Source::Values { scanrelid: 2, lists: nil }, into(BreakerKind::HashBuild, 0)));
        let mut p = PipelineNode::new(a, 6, Source::Range { site: SiteDesc::none_in(a), am: Am::Pgrcolumnar, scanrelid: 1, relid: 16384 }, into(BreakerKind::PlainAgg, 1).consuming_one(0));
        let prm = types_nodes::primnodes::Param { paramkind: types_nodes::primnodes::ParamKind::PARAM_EXEC, paramid: 0, paramtype: 16, paramtypmod: -1, ..Default::default() };
        p.push(Op::Filter(Node::mk(m, prm).unwrap()));
        p.push(Op::Join(JoinDesc::Probe { slot: 0, jointype: jt, clauses: nil, keys: nil, qual: nil, unique: false, marks: false, decls: ArenaVec::new_in(a), inner_node: 7, outer_first: true, catalog: false }));
        g.pipelines.push(p);
        g.pipelines.push(PipelineNode::new(a, 5, Source::Breaker { slot: 1 }, Pipeline::serial(-1, End::Sink, r).consuming_one(1)));
        g.params.push((0, gravity_decompose::graph::ParamRep { owner: gravity_decompose::graph::ParamOwner::Nest { node: 5 }, typid: 16, typmod: -1, coll: 0, out: None }));
        let (mut pipelines, mut slots) = (ArenaVec::new_in(a), [0u64; 4]);
        pipelines.push(0u16);
        pipelines.push(1u16);
        slot_set(&mut slots, 0);
        slot_set(&mut slots, 1);
        g.reruns.push(SetDecl { node_id: 5, plan_id: 0, pipelines, slots });
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        rerun_order(&gx, 1, 1, 0, false)
    }

    #[test]
    fn a_raising_member_build_op_14_holds_per_call_serves_where_pg_per_call_order_cannot_show() {
        // a quiet outer side: a fill type fetches its outer first at every call; the cost path's outer reads no parameter
        assert_eq!(order(true, false, JOIN_INNER, true, false, false), None, "INNER, a raising build, an outer side reading no parameter");
        assert_eq!(order(true, false, JOIN_SEMI, true, false, false), None, "SEMI, a raising build, an outer side reading no parameter");
        assert_eq!(order(true, false, JOIN_LEFT, true, false, false), None, "LEFT, a raising build not lazy");
        assert_eq!(order(true, false, JOIN_ANTI, true, false, false), None, "ANTI, a raising build");
        assert_eq!(param_outer(JOIN_LEFT), None, "LEFT, an outer side reading the parameter");
        assert_eq!(param_outer(JOIN_ANTI), None, "ANTI, an outer side reading the parameter");
        // D3332: the cost path's outer reading the parameter (empty at one call, not at another) over a raising member
        // build op 14 holds: the probe keeps hj_OuterNotEmpty per call (probe_calls.rs `orders_held`, `Order`)
        assert_eq!(param_outer(JOIN_INNER), None, "INNER, an outer side reading the parameter");
        assert_eq!(param_outer(JOIN_SEMI), None, "SEMI, an outer side reading the parameter");
        // D1902's still: a raising outer side, a build that calls, a build side the class does not see into
        assert_eq!(order(true, true, JOIN_INNER, true, false, false), Some(OUTER_FIRST_RAISES), "INNER, a raising build and outer side");
        assert_eq!(order(true, true, JOIN_LEFT, true, false, false), Some(OUTER_FIRST_RAISES), "LEFT, a raising build and outer side");
        assert_eq!(order_with(false, 1, false, JOIN_LEFT, true, false, false), Some(OUTER_FIRST_RAISES), "LEFT, a build that calls");
    }
}
