//! Kind `merge.join` (m9-m11 plan §3c; MERGE-1: INNER, LEFT, SEMI, ANTI; MERGE-2 / 3: RIGHT, FULL, RIGHT ANTI). Owner
//! M11-MERGE. Budget 450 lines.
//!
//! decompose's shape: the inner is a sealed run in a slot (a Sort's run reused, else a Materialize whose end is this kind's:
//! runs.sort's keyless run in the plan's order, runs/slot_run.rs), and the merge is a step on the outer pipeline. The step
//! runs PG's `ExecMergeJoin` state machine (nodeMergejoin.c; v1 b5e7abcc076 joinlower.rs:695) literally, one outer row per
//! call of `lower`: the states that fetch the next outer tuple (INITIALIZE_OUTER, NEXTOUTER, SKIPOUTER_ADVANCE, ENDINNER)
//! are where a row's work ends, and the next row enters there. The inner "fetch" is an index into the run (the current inner
//! tuple `W_IP`, the mark `W_MARK`), so mark / restore is a word (road merge.markable's position form). The compare is PG's
//! `MJCompare`: per clause the btree family's support function 1 under the clause's collation, reversed for DESC, never
//! over a NULL (MJEvalOuterValues / MJEvalInnerValues make such a tuple non-matchable, or end the join when the first key
//! sorts NULLS LAST and that side is not filled), with a constant-false join qual making every tie `> 0`. The input's order
//! is checked where PG checks it: TESTOUTER below the mark and NEXTINNER above the outer raise PG's "mergejoin input data is
//! out of order". A join that PG ends (the inner exhausted under INNER / SEMI) ends the pipeline at that outer row, as PG
//! fetches no further outer tuple.
//!
//! Stop and resume: the step is expanding. Its offset word says what the row handed on last (a joined row, or the row's
//! fill), so a resumed call continues the machine past it, or hands the same row on again when a later expanding step
//! stopped inside it (D688). RIGHT / FULL (MERGE-2, D1267) and RIGHT ANTI (MERGE-3, D1349: a match marks the inner tuple
//! and is never handed on) fill the inner where PG does, one word for its current tuple
//! (`mj_MatchedInner`: exact, as the planner makes every such join qual a merge clause), and ENDOUTER at the input's end
//! (merge/fill_inner.rs). The step runs serial (MERGE-3d: a clauseless join's unsorted outer too; with no clause every
//! tuple is matchable and ties, as MJCompare's). An inner side that can raise or calls a user or volatile function (PG runs
//! it only once an outer row needs it: D1236) and a RIGHT join's outer that may act past its first NULLS LAST NULL key
//! (D1289) refuse by name, except a Sort-ended inner, which runs lazily at INITIALIZE_INNER as PG's does (MERGE R3 (ii),
//! D824's lazy build; merge/machine.rs; an acting outer: merge/lazy_outer.rs; RIGHT / RIGHT ANTI also at ENDOUTER: `fills`).
pub(crate) use core::cell::{Cell as VCell, RefCell};

pub(crate) use gravity_abi::breaker::BreakerKind;
pub(crate) use gravity_decompose::graph::Bound;
pub(crate) use gravity_decompose::join::JoinDesc;
pub(crate) use gravity_decompose::{Op, Source};
pub(crate) use gravity_ir::builder::FunctionBuilder;
pub(crate) use gravity_ir::{Block, IntBinOp, IntCond, Type, Value};
pub(crate) use gravity_program::End;
pub(crate) use types_nodes::jointype::JoinType;

pub(crate) use crate::face::end::EndMech;
pub(crate) use crate::ops::shared::lazy::{LazyImpl, LazyMech};
pub(crate) use crate::face::kinds::{Face, KindId};
pub(crate) use crate::face::runs::{SlotRun, SlotRunImpl};
pub(crate) use crate::face::step::{Expand, ExpandKind, ExprFace, StepMech};
pub(crate) use crate::frame::env::Env;
pub(crate) use crate::frame::frame::{EndMaker, Next, StepMaker};
pub(crate) use crate::frame::open::{Base, OpenWord, OpenWords};
pub(crate) use crate::frame::px::{ExId, Px, WordKey};
pub(crate) use crate::frame::row::{Carry, RowVals};
pub(crate) use crate::frame::stop::{self, Exits, ResumeWord};
pub(crate) use crate::graph::Gx;
pub(crate) use crate::admit::refusal::{Reason, ShapeWhy};
pub(crate) use crate::ops::Table;
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::MergeJoin, &StepMaker(make)), (KindId::MergeJoin, &EndMaker(make_end))], ..crate::ops::NONE };

// PG's EXEC_MJ_* states (the word `W_ST`), plus DONE.
pub(crate) const INIT_OUTER: u64 = 0;
pub(crate) const INIT_INNER: u64 = 1;
pub(crate) const JOINTUPLES: u64 = 2;
pub(crate) const NEXTOUTER: u64 = 3;
pub(crate) const TESTOUTER: u64 = 4;
pub(crate) const NEXTINNER: u64 = 5;
pub(crate) const SKIP_TEST: u64 = 6;
pub(crate) const SKIPOUTER_ADVANCE: u64 = 7;
pub(crate) const SKIPINNER_ADVANCE: u64 = 8;
pub(crate) const ENDOUTER: u64 = 9;
pub(crate) const ENDINNER: u64 = 10;
pub(crate) const DONE: u64 = 11;
// MJEvalOuterValues / MJEvalInnerValues.
pub(crate) const MATCHABLE: i64 = 0;
pub(crate) const NONMATCHABLE: i64 = 1;
pub(crate) const ENDOFJOIN: i64 = 2;
// The offset word: what the row handed on last.
pub(crate) const JOINED: i64 = 1;
pub(crate) const FILLED: i64 = 2;
pub(crate) const FILLED_INNER: i64 = 3;
// The step's per-open words (`W_*`: `Merge::w`).
pub(crate) const W_ST: u8 = 0;
pub(crate) const W_IP: u8 = 1;
pub(crate) const W_MARK: u8 = 2;
pub(crate) const W_MO: u8 = 3;
pub(crate) const W_MI: u8 = 4;

pub(crate) fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// One merge clause: its outer and inner key expressions and its compare (`cmp_proc(outer, inner)`, int4).
pub(crate) struct Clause {
    outer: ExId,
    inner: ExId,
    cmp: ExId,
    desc: bool,
    nulls_first: bool,
}

pub(crate) struct Merge<'g> {
    /// The planning context the step's lowering-time shapes (`cells`, `outer`) live in.
    mcx: Mcx<'g>,
    pub(crate) node: i32,
    pub(crate) jt: JoinType,
    /// PG's single_match: SEMI, or an inner the planner proved unique.
    pub(crate) single: bool,
    clauses: PgVec<'g, Clause>,
    /// MJExamineQuals' constant-false join qual (`mj_ConstFalseJoin`): every tie compares `> 0`.
    const_false: bool,
    pub(crate) qual: PgVec<'g, ExId>,
    held: u8,
    pub(crate) ex: &'static ExprFace,
    pub(crate) run: PgBox<'g, dyn SlotRun + 'g>,
    pub(crate) offset: i32,
    pub(crate) w: [i32; 5],
    pub(crate) n: VCell<Option<Value>>,
    /// The inner's cells as the first joined row carried them (the NULL-extended row's shape), made while lowering; the
    /// outer's as its row came (RIGHT / FULL's inner fill).
    pub(crate) cells: RefCell<Option<crate::ops::mergejoin::strategies::markable::InnerShape<'g>>>,
    pub(crate) outer: RefCell<Option<crate::ops::mergejoin::strategies::markable::InnerShape<'g>>>,
    /// MERGE R3 (ii), V4R-494: the inner runs lazily (D824), pending until INITIALIZE_INNER: its consumed index and the wait.
    pub(crate) lazy: Option<(usize, &'static dyn LazyMech)>,
    /// jmerge-0012: the pulled inner's handle word and the merge's words over it (merge/exact_inner.rs over face/pull.rs;
    /// MJ-AHEAD's budget).
    pub(crate) pull: Option<crate::face::pull::PullWords>,
    /// D3190: the co-split form (merge/cosplit.rs): one machine per claim, its inputs the claim's key range of each run.
    cosplit: bool,
    /// Construct C2: the licences the inner's run whole before the outer's first row holds (`make`): a quiet inner's
    /// no-effects licence (a nest's inner merge's, nest_inner.rs `ahead`, or one whose fillers act nowhere after what the
    /// outer's first row ran), each filler's `Px::run_ahead` recheck licence or `Px::ahead_unchecked` tagged one, a refilled
    /// run's `Px::accept` licences (nest_refill.rs), the held inner's held-error licence (held_inner.rs) or a FULL merge's
    /// covered fillers' (full_whole.rs). Held, not read.
    #[allow(dead_code)]
    ahead: PgVec<'g, gravity_decompose::effects::Licence>,
}

/// The Materialize under a merge join: runs.sort's keyless run (runs/slot_run.rs).
pub(crate) fn make_end<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let node = px.gx.pipelines()[k].node_id;
    let (mech, pulled) = (crate::frame::iface::kind_impl::<SlotRunImpl>(&node, KindId::RunsSort).0, crate::ops::mergejoin::exact_inner::pulled(px, k));
    if pulled { mech.pulled_end(px, k) } else { mech.end(px, k) } // jmerge-0012: a pulled inner's end stops after each row
}

/// A Material's own spool op in the pipeline that fills this kind's inner (graph.rs `pipe_kind`): its rows pass as they
/// come, the end seals them.
pub(crate) struct Through;

impl StepMech for Through {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        next(b, env, row)
    }
}

pub(crate) fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let mcx = px.mcx();
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    if matches!(p.ops.get(j), Some(Op::Join(JoinDesc::Spool))) {
        return mcx::pg_box_dyn!(mcx, Through => dyn StepMech + 'g);
    }
    let node = p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(p.node_id, |n| n.node_id);
    let Some(Op::Join(JoinDesc::Merge { slot, jointype, clauses, reversals, nulls_first, families, collations, qual, unique, .. })) = p.ops.get(j) else {
        crate::refuse!(node, Reason::Bug { payload: "a merge step over another op" })
    };
    let jt = match *jointype {
        JoinType::JOIN_INNER | JoinType::JOIN_LEFT | JoinType::JOIN_SEMI | JoinType::JOIN_ANTI | JoinType::JOIN_RIGHT | JoinType::JOIN_FULL | JoinType::JOIN_RIGHT_ANTI => *jointype,
        _ => shape(node, "a merge join of a type nodeMergejoin never runs"),
    };
    let fill_inner = crate::ops::mergejoin::fill_inner::fills_inner(jt);
    if let Err(why) = crate::ops::mergejoin::fill_inner::qual_ok(fill_inner, qual.iter()) {
        crate::refuse!(node, why)
    }
    let Some(life) = gx.life_of(k, *slot) else { crate::refuse!(node, Reason::Bug { payload: "a merge without its inner" }) };
    // MERGE R3 (ii), V4R-494: PG's ExecSort reads its whole subtree at INITIALIZE_INNER's first fetch (the first matchable
    // outer tuple); an INNER / LEFT / SEMI / ANTI join over a Sort-ended inner that may act runs it there (D824's lazy group)
    let inner = life.0 as usize;
    let pull = crate::ops::mergejoin::exact_inner::declare(px, (k, j), inner); // jmerge-0012: pulled as PG fetches it
    // D2026: every arm of a UNION ALL inner fills the run (lazy_outer.rs `fillers`), not the last one alone
    let fillers = crate::ops::mergejoin::lazy_outer::fillers(gx, k, *slot);
    // D3634: a nest loop's inner merge over a run filled once ahead of the owner (nest_inner.rs) takes no lazy group
    let nested = crate::ops::mergejoin::nest_inner::serves(gx, k, j);
    // Construct C2: the licences of the inner's run whole before the outer's first row (`Merge.ahead`)
    let mut ahead: PgVec<'_, gravity_decompose::effects::Licence> = PgVec::new_in(mcx);
    if nested {
        let Some(l) = crate::ops::mergejoin::nest_inner::ahead(px, &fillers) else { shape(node, crate::rerun::inner::INNER_BEYOND_FORMS) };
        ahead.push_in(l);
    }
    let held_in = VCell::new(None); // D1236's held inner (held_inner.rs): its licence
    // D3799: an INNER / LEFT / SEMI / ANTI merge fetches its outer first (MJ_INITIALIZE_OUTER) and stops on an empty one,
    // so what the outer's first row ran to its end (lazy_outer.rs `first_whole`) never runs for the inner: River's
    // conflicts read jobs_with_rownum, whose Sort read the FOR UPDATE claim whole before the merge's first inner fetch.
    // Only where PG surely runs the merge: its pipeline writes (ModifyTable reads its plan to the end) and only filters
    // and projections come before it
    let sure = matches!(jt, JoinType::JOIN_INNER | JoinType::JOIN_LEFT | JoinType::JOIN_SEMI | JoinType::JOIN_ANTI)
        && p.ops.iter().any(|o| matches!(o, Op::Write(_)))
        && p.ops[..j].iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_)));
    let ran = match p.source {
        Source::Breaker { slot: s } if sure => crate::ops::mergejoin::lazy_outer::first_whole(gx, k, s),
        _ => PgVec::new_in(mcx),
    };
    let acts_after = pull.is_none() && !nested && fillers.iter().any(|&x| crate::effects::visible::merge::inner_may_act_after(gx, x, &ran));
    // a sealed inner whose fillers act nowhere after what PG ran first runs whole before the outer's first row: it departs
    // from nothing (the no-effects licence, as nest_inner.rs `ahead` reads a quiet inner)
    if pull.is_none() && !nested && !acts_after {
        if let Some(l) = gravity_decompose::effects::unobservable(gravity_decompose::effects::Effects::NONE) {
            px.hold(inner, &l);
            // Construct C2 (b): what fills and feeds the inner runs before it under the same licence, each one asked as the
            // inner was (its own walk finds nothing that acts after what PG ran first)
            if crate::frame::program::stamps_on() {
                let src = |x: usize| if let Source::Breaker { slot } = gx.pipelines()[x].source { Some(slot) } else { None };
                let fed = fillers.iter().flat_map(|&x| crate::ops::mergejoin::lazy_outer::lives(gx, x, gx.pipelines()[x].desc.consumes.iter().copied().chain(src(x)))).chain(fillers.iter().copied());
                for x in fed.filter(|x| !ran.contains(x)) {
                    if !crate::effects::visible::merge::inner_may_act_after(gx, x, &ran) {
                        px.hold(x, &l);
                    }
                }
            }
            ahead.push_in(l);
        }
    }
    let lazy = acts_after.then(|| {
        let sorted = matches!(crate::frame::iface::end_breaker(gx, inner), Some(BreakerKind::Sort)) || crate::ops::mergejoin::spool_inner::first_pull(px, (k, j), inner, *slot, jt);
        let lz: &'static dyn LazyMech = crate::frame::iface::kind_impl::<LazyImpl>(&node, KindId::JoinInner).0;
        // the outer's feeders (its breaker source, the slots its subtree reads: consumed before the inner's) PG runs first
        let src = |k: usize| if let Source::Breaker { slot } = gx.pipelines()[k].source { Some(slot) } else { None };
        let first: PgVec<'_, u8> = gx.pipelines()[k].desc.consumes.iter().take_while(|&&s| s != *slot).copied().chain(src(k)).collect_in(mcx);
        let before = crate::ops::mergejoin::lazy_outer::lives(gx, k, first.iter().copied());
        let mut all: PgVec<'_, usize> = fillers.iter().flat_map(|&x| crate::ops::mergejoin::lazy_outer::lives(gx, x, gx.pipelines()[x].desc.consumes.iter().copied().chain(src(x)))).collect_in(mcx);
        all.extend_in(fillers.iter().copied()); // (each arm's feeders, then the arms, the inner's own pipeline last)
        // an outer that may act (D1236's rule: its ops and feeders) keeps D1236's name unless PG surely runs the merge over
        // it whole (W8-D1236-RESIDUE, merge/lazy_outer.rs: D1716's LIMIT 0 / unreached merge stay refused)
        let acting = crate::effects::visible::merge::may_act_but(gx, k, j, Some(*slot));
        let fill = !acting && fills(gx, (k, j), jt, (&before, &all));
        let kinds = fill || matches!(jt, JoinType::JOIN_INNER | JoinType::JOIN_LEFT | JoinType::JOIN_SEMI | JoinType::JOIN_ANTI);
        let served = !acting || sorted && kinds && crate::ops::mergejoin::lazy_outer::acting_outer(gx, k, j, &before, &all);
        let group = px.consumed(k, *slot).filter(|_| sorted && kinds && served).and_then(|c| Some((c, lz.group(px, inner, k, c, *slot, &first)?)));
        let mut near = crate::ops::mergejoin::lazy_outer::lives_by(gx, inner, gx.pipelines()[inner].desc.consumes.iter().copied().chain(src(inner)), false);
        near.push_in(inner); // (D2026: the walk the group is built by, before the arms are read)
        let holds = |g: &[usize]| crate::ops::mergejoin::lazy_outer::leaves(gx, k, g, (&before, &all), acting) && crate::ops::mergejoin::lazy_outer::group_holds(gx, g, (&before, &all), &near);
        let Some((consumed, group)) = group.filter(|g| holds(&g.1)) else {
            // D3374: or a recursive term's body PG runs whole at every generation (fed_first.rs `term`), for this road alone
            let fed = served || kinds && crate::ops::mergejoin::fed_first::term(gx, (k, j), &before, &all);
            let own = if acting && fed { crate::ops::mergejoin::fed_first::fed_first(gx, inner, &before) } else { None }; // W8-D1236-CLASSD fed_first.rs: runs unwaited
            let unwaited = own.is_some();
            // Construct C2: the inner's own pipelines, quiet by that walk, run unwaited under the no-effects licence. The
            // outer's feeders they pull first (PG's INITIALIZE_OUTER runs those whole: fed_first.rs) hold no licence yet
            if let (Some(own), Some(l)) = (&own, gravity_decompose::effects::unobservable(gravity_decompose::effects::Effects::NONE)) {
                own.iter().for_each(|&x| px.hold(x, &l));
                ahead.push_in(l);
            }
            // D1236 (v5 errorhold, item 10): else an inner that may raise runs whole first under a recheck licence (`Px::run_ahead`),
            // each arm of a UNION ALL inner asked on its own side (D2026: an earlier arm's user call keeps D1236's name too)
            let recheck = if unwaited { None } else { fillers.iter().try_fold(PgVec::new_in(mcx), |mut v, &x| {
                v.push_in(px.run_ahead(x, gravity_decompose::effects::Effects::RAISES)?);
                Some(v)
            }) };
            let held_ahead = &mut ahead;
            held_ahead.extend_in(recheck.iter().flatten().copied());
            // D1236's refilled run (nest_refill.rs): served to a reader that takes every row; its fill, run whole at each
            // re-open past PG's pulls, holds the accepted class's licence where it grants one (`Px::accept`)
            let refill = if unwaited || recheck.is_some() { None } else { crate::ops::mergejoin::nest_refill::refill_raises(gx, k, j) }.map(|o| {
                px.whole_read(o);
                let arena = px.arena;
                let e = fillers.iter().fold(gravity_decompose::effects::Effects::NONE, |e, &x| e.and(crate::effects::pipeline_effects(gx, arena, x, true)));
                fillers.iter().filter_map(|&x| px.accept(x, e)).collect_in(mcx)
            });
            held_ahead.extend_in(refill.iter().flatten().copied());
            let ahead = unwaited || recheck.is_some() || refill.is_some();
            // D1236's held inner (held_inner.rs, op 14 for runs.sort): the inner's Sort runs in program order and its error
            // waits for the merge's first inner call (INITIALIZE_INNER's or ENDOUTER's `first_fetch`, any join type: the
            // raise falls where PG's first inner call does). An outer that may act is read as PG reads it (held_inner.rs
            // `outer_exact`: a full Sort's run or a scan this pipeline stops where the merge ends, the merge surely run)
            let full_sort = matches!(crate::frame::iface::end_breaker(gx, inner), Some(BreakerKind::Sort));
            let outer = served || crate::ops::mergejoin::held_inner::outer_exact(gx, k, j);
            if let Some(held) = (!ahead && full_sort && outer).then(|| crate::ops::mergejoin::held_inner::holds(px, (k, j), inner, &fillers)).flatten() {
                px.held_sort.push(inner); // alloc: Program
                px.hold(inner, &held); // the tracer's stamp: the inner's fill runs in program order, its error held
                held_in.set(Some(held));
                held_ahead.push_in(held);
                return None;
            }
            // `DECISIONS.md` 4 (D) b: else, where no plan-order run re-checks the inner and nothing in it acts or calls user
            // code, it runs whole first unchecked (`Px::ahead_unchecked`, tagged); never a CTE's store, which fills only at
            // its readers' pulls (D2178, spool_inner.rs: run whole first it reads the store empty), nor an inner fed by a CTE
            // the outer does not read (`all` past `before`: D3817's bare CteScan inner spooled through its reader, whose pull
            // would run the CTE where PG's merge, reaching no inner row, never runs it: v5_mergejoin_d2178_cte_store's empty
            // outer; an outer reading the same CTE runs it as PG's does, m11_merge_fed_first's self-join)
            let unread = all.iter().any(|&x| gx.cte_write(x) && !before.contains(&x));
            let tagged = if ahead || unread { None } else { fillers.iter().try_fold(PgVec::new_in(mcx), |mut v, &x| {
                if gx.cte_write(x) {
                    return None;
                }
                v.push_in(px.ahead_unchecked(x, gravity_decompose::effects::Effects::RAISES)?);
                Some(v)
            }) };
            held_ahead.extend_in(tagged.iter().flatten().copied());
            let ahead = ahead || tagged.is_some();
            // D2212: or a FULL merge's inner whose every raise PG's statement makes too or fails anyway (full_whole.rs)
            let full = if !ahead && jt == JoinType::JOIN_FULL { crate::ops::mergejoin::full_whole::full_whole(px, k, node, full_sort, &fillers) } else { None };
            held_ahead.extend_in(full.iter().flatten().copied());
            let ahead = ahead || full.is_some();
            // the accepted class (AGENTS.md "Errors do not constrain evaluation order"): an inner whose only departure is
            // an error PostgreSQL never hits runs whole first (`Px::accept`). Not one that acts: a nextval it runs past PG's
            // pulls is read by the next statement (v5_mergejoin_d2275_fedfirst E2: the sequence's last_value)
            let arena = px.arena;
            let e = fillers.iter().fold(gravity_decompose::effects::Effects::NONE, |e, &x| e.and(crate::effects::pipeline_effects(gx, arena, x, true)));
            let acts = e.acts || all.iter().any(|&x| crate::effects::pipeline_effects(gx, arena, x, true).acts);
            let accepted = if ahead || unread || acts { None } else {
                fillers.iter().try_fold(PgVec::new_in(mcx), |mut v, &x| {
                    if gx.cte_write(x) {
                        return None;
                    }
                    v.push_in(px.accept(x, e)?);
                    Some(v)
                })
            };
            held_ahead.extend_in(accepted.iter().flatten().copied());
            let ahead = ahead || accepted.is_some();
            return ahead.then_some(None).unwrap_or_else(|| shape(node, "a merge join whose inner side can raise or calls a user or volatile function (PG runs it only once an outer row needs it: D1236)"));
        };
        group.into_iter().for_each(|x| px.lazy.push((x, k))); // alloc: Program
        Some((consumed, lz))
    }).flatten();
    let ex: &'static ExprFace = crate::frame::iface::expr_face(&node);
    let cmps = crate::ops::mergejoin::strategies::markable::compares(px, node, clauses, families, collations);
    let mut cs = mcx::reserved_in(cmps.len(), mcx);
    for (i, (o, n, c)) in cmps.into_iter().enumerate() {
        let (outer, inner, cmp) = (ex.0.compile_at(px, (k, j), o, None, node), ex.0.compile_at(px, (k, j), n, None, node), ex.0.compile_at(px, (k, j), c, None, node));
        let (Some(&desc), Some(&nulls_first)) = (reversals.get(i), nulls_first.get(i)) else { shape(node, "merge arrays shorter than the clauses") };
        cs.push_in(Clause { outer, inner, cmp, desc, nulls_first });
    }
    let const_false = qual.iter().any(crate::ops::shared::dummy::never);
    let qual: PgVec<'_, ExId> = qual.iter().map(|q| ex.0.compile_at(px, (k, j), q, None, node)).collect_in(mcx);
    let held = qual.iter().map(|&q| px.placed(q).held.len()).sum::<usize>() as u8;
    let mech = crate::frame::iface::kind_impl::<SlotRunImpl>(&node, KindId::RunsSort).0;
    let run = if pull.is_some() { mech.pulled_reader(px, k, *slot) } else { mech.reader(px, k, *slot) };
    let run = if let Some(held) = held_in.get() { crate::ops::mergejoin::held_inner::wrap(px, k, *slot, run, held) } else { run };
    let offset = stop::resume_word(px, ResumeWord::StepOffset { k, j: crate::frame::px::key_j(j) });
    // (an outer whose first key is a NOT NULL scan column, through filters, projections and joins that keep the scan row, never reaches that key: D3494)
    let nn = matches!(&p.source, Source::Range { relid, .. } | Source::Index { relid, .. } if p.ops[..j].iter().all(crate::props::code::keeps_scan_row) &&clauses.iter().next().and_then(|c| c.as_op_expr()).and_then(|o| crate::props::code::scan_attno(p.ops[..j].iter(), o.args.nth(0))).and_then(|a| gx.facts.att(*relid, a)).is_some_and(|f| f.notnull));
    // (nor one with at most one NULL-keyed row, the last: a group keyed by that column alone, D1289's `one_null_row`)
    let one = || clauses.iter().next().and_then(|c| c.as_op_expr()).is_some_and(|o| crate::ops::mergejoin::lazy_outer::one_null_row(gx, k, j, o.args.nth(0)));
    // (D4106, S3 rkeep ra-hx8/m7-mj-left-in-lat: nor through what PG ran whole at the merge's first outer fetch: MJ_INITIALIZE_OUTER
    // pulls the outer's first tuple, and a full breaker's producer (a Sort's run: lazy_outer.rs `first_whole`) reads its
    // whole input there, before any key is seen, in both engines; only what runs per outer row after it may act past the key)
    let acts_past = || {
        let ran = match p.source {
            Source::Breaker { slot: s } => crate::ops::mergejoin::lazy_outer::first_whole(gx, k, s),
            _ => PgVec::new_in(mcx),
        };
        crate::effects::visible::merge::outer_may_act_after(gx, k, j, Some(*slot), &ran)
    };
    if matches!(jt, JoinType::JOIN_RIGHT | JoinType::JOIN_RIGHT_ANTI) && cs.first().is_some_and(|c| !c.nulls_first) && !nn && acts_past() && !one() { shape(node, "a RIGHT merge join whose outer may act past its first NULLS LAST NULL key (PG fetches no outer past it: D1289)") }
    let mut w = [0; 5];
    for i in [W_ST, W_IP, W_MARK, W_MO].into_iter().chain(fill_inner.then_some(W_MI)) {
        w[i as usize] = px.frame_word(WordKey::Open { face: Face::Merge, k, j: crate::frame::px::key_j(j), i }) as i32 * 8;
    }
    let single = jt == JoinType::JOIN_SEMI || *unique;
    let cosplit = crate::face::cosplit::of(px, k).is_some() && crate::face::cosplit::merge_at(&px.gx.pipelines()[px.ident(k)]) == Some(j);
    if cosplit && (lazy.is_some() || pull.is_some()) {
        crate::refuse!(node, Reason::Bug { payload: "a co-split merge join with a lazy or pulled inner (merge/cosplit.rs declares neither)" })
    }
    mcx::pg_box_dyn!(mcx, Merge { mcx, node, jt, single, clauses: cs, const_false, qual, held, ex, run, offset, w, n: VCell::new(None), cells: RefCell::new(None), outer: RefCell::new(None), lazy, pull, cosplit, ahead } => dyn StepMech + 'g)
}

/// W8-D1236-CLASSA: RIGHT / RIGHT ANTI over a quiet outer. PG's node, once called, reads its whole inner (INITIALIZE_INNER's
/// fetch, else ENDOUTER's over an empty or unmatchable outer) and hands nothing on before: so PG must surely call it and v4
/// reach its input's end, where `flush` waits (a LIMIT or gate before it ends the pipeline there). FULL fills outer first.
/// D2026: a UNION ALL's arms on either side are not this form (lower.rs's other roads take the merge).
pub(crate) fn fills(gx: &Gx<'_, '_>, (k, j): (usize, usize), jt: JoinType, (before, all): (&[usize], &[usize])) -> bool {
    let ops = &gx.pipelines()[k].ops[..j];
    let arms = crate::ops::mergejoin::lazy_outer::arms(gx, before) || crate::ops::mergejoin::lazy_outer::arms(gx, all) || ops.iter().any(|op| matches!(op, Op::Append(_)));
    matches!(jt, JoinType::JOIN_RIGHT | JoinType::JOIN_RIGHT_ANTI) && !arms
        && ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_) | Op::Join(JoinDesc::Nest { .. })) || crate::frame::iface::limit_bounds(op).is_some_and(|(count, _)| matches!(count, Bound::All))) && crate::ops::mergejoin::lazy_outer::reached(gx, k, j + 1)
}

impl Merge<'_> {
    pub(crate) fn fill_outer(&self) -> bool {
        matches!(self.jt, JoinType::JOIN_LEFT | JoinType::JOIN_ANTI | JoinType::JOIN_FULL)
    }

    pub(crate) fn fill_inner(&self) -> bool {
        crate::ops::mergejoin::fill_inner::fills_inner(self.jt)
    }

    pub(crate) fn word(&self, b: &mut FunctionBuilder<'_>, env: &Env, i: u8) -> Value {
        env.load_word(b, self.w[i as usize])
    }

    pub(crate) fn set(&self, b: &mut FunctionBuilder<'_>, env: &Env, i: u8, v: i64) {
        let c = b.iconst(Type::I64, v);
        env.store_word(b, self.w[i as usize], c);
    }

    /// The outer row joined to inner row `ip`, and the map of the inner's columns into it (`env.inner`).
    pub(crate) fn joined<'r, 'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals<'r>, carry: &Carry, ip: Value) -> (RowVals<'r>, PgVec<'e, Option<u16>>) {
        let r = self.inner_at(b, env, row, carry, ip);
        crate::ops::mergejoin::strategies::markable::join_rows(row, (&r.row.cells, &r.vals, &r.cols), carry, env.mcx)
    }

    /// Inner row `ip` as the join reads it.
    pub(crate) fn inner_at<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, carry: &Carry, ip: Value) -> RowVals<'e> {
        let p = self.run.fetch(b, env, ip);
        let r = self.run.vals(b, env, p, carry.copy_in(env.mcx), row.at, row.at);
        let types: PgVec<'_, Type> = r.vals.iter().map(|&(v, _)| b.value_type(v)).collect_in(self.mcx);
        self.cells.borrow_mut().get_or_insert_with(|| (mcx::to_vec_in(&r.row.cells[..], self.mcx), types, mcx::to_vec_in(&r.cols[..], self.mcx)));
        r
    }

    /// The expression `e` over `row` with the inner's columns mapped by `map`: `(value, valid)`.
    fn emit(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, map: Option<&[Option<u16>]>, e: ExId) -> (Value, Value) {
        // the machine's blocks do not dominate each other: no memoized read crosses them
        env.memo.clear();
        let mcx = env.mcx;
        let saved = core::mem::replace(&mut env.inner, map.map(|m| mcx::to_vec_in(m, mcx)));
        let r = self.ex.0.emit(b, env, self.node, row, e);
        env.inner = saved;
        r
    }

    /// MJEvalOuterValues (`outer`) / MJEvalInnerValues: MATCHABLE, NONMATCHABLE or ENDOFJOIN (`I64`). A NULL first key that
    /// sorts NULLS LAST ends the join unless that side is filled; any other NULL key makes the tuple non-matchable.
    pub(crate) fn eval(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, map: Option<&[Option<u16>]>, outer: bool) -> Value {
        let fill = if outer { self.fill_outer() } else { self.fill_inner() };
        let (nm, eoj) = (b.iconst(Type::I64, NONMATCHABLE), b.iconst(Type::I64, ENDOFJOIN));
        let mut res = env.zero; // MATCHABLE
        for (i, c) in self.clauses.iter().enumerate() {
            let (_, valid) = self.emit(b, env, row, map, if outer { c.outer } else { c.inner });
            let null = match i == 0 && !c.nulls_first && !fill {
                true => eoj,
                false => {
                    let m = b.icmp(IntCond::Eq, res, env.zero);
                    b.select(m, nm, res)
                }
            };
            res = b.select(valid, res, null);
        }
        res
    }

    /// MJCompare over the joined row: `< 0`, `0` or `> 0` (`I64`), the first clause that differs deciding (negated for
    /// DESC); a tie is `> 0` under a constant-false join qual.
    pub(crate) fn compare(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, map: &[Option<u16>]) -> Value {
        let l_end = b.create_block(&[Type::I64]);
        for c in self.clauses.iter() {
            let (v, _) = self.emit(b, env, row, Some(map), c.cmp);
            let v = match b.value_type(v) {
                Type::I64 => b.conv(gravity_ir::ConvOp::Trunc, Type::I32, v),
                _ => v,
            };
            let v = b.conv(gravity_ir::ConvOp::Sext, Type::I64, v);
            let v = if c.desc { b.int_bin(IntBinOp::Sub, env.zero, v) } else { v };
            let (ne, l_go) = (b.icmp(IntCond::Ne, v, env.zero), b.create_block(&[]));
            b.cond_br(ne, (l_end, &[v]), (l_go, &[]));
            b.switch_to(l_go);
        }
        let tie = if self.const_false { env.one } else { env.zero };
        b.br(l_end, &[tie]);
        b.switch_to(l_end);
        b.block_params(l_end)[0]
    }
}

/// D688: a later expanding step's offset word is set (it stopped inside the row handed on: re-enter it); `None` without one.
pub(crate) fn later_set(b: &mut FunctionBuilder<'_>, env: &Env) -> Option<Value> {
    let mut set = None;
    for &(_, o) in env.expands.iter().filter(|e| env.step.is_some_and(|j| e.0 > j)) {
        let w = env.load_word(b, o);
        set = Some(set.map_or(w, |a| b.int_bin(IntBinOp::Or, a, w)));
    }
    Some(b.icmp(IntCond::Ne, set?, env.zero))
}

/// A three-way branch on a verdict (`MATCHABLE` / `NONMATCHABLE` / else) or a compare (`== 0` / `< 0` / `> 0`).
pub(crate) fn three(b: &mut FunctionBuilder<'_>, v: Value, (a, n, e): (Block, Block, Block), cmp: bool, cp: &[Value]) {
    let z = b.iconst(Type::I64, MATCHABLE); // 0: also a compare's tie
    let first = b.icmp(IntCond::Eq, v, z);
    let l_rest = b.create_block(&[]);
    b.cond_br(first, (a, cp), (l_rest, &[]));
    b.switch_to(l_rest);
    let second = match cmp {
        true => b.icmp(IntCond::Slt, v, z),
        false => {
            let nm = b.iconst(Type::I64, NONMATCHABLE);
            b.icmp(IntCond::Eq, v, nm)
        }
    };
    b.cond_br(second, (n, cp), (e, cp));
}

impl StepMech for Merge<'_> {
    fn open_words(&self, w: &mut OpenWords) {
        self.pulled_words(w);
        w.declare(OpenWord::open("merge.join offset", Base::Words, self.offset, Type::I64, 0));
        w.declare(OpenWord::open("merge.join state", Base::Words, self.w[W_ST as usize], Type::I64, INIT_OUTER as i64));
        w.declare(OpenWord::open("merge.join inner tuple", Base::Words, self.w[W_IP as usize], Type::I64, -1));
        w.declare(OpenWord::open("merge.join mark", Base::Words, self.w[W_MARK as usize], Type::I64, -1));
        w.declare(OpenWord::open("merge.join matched outer", Base::Words, self.w[W_MO as usize], Type::I64, 0));
        if self.fill_inner() {
            // 1 while there is no current inner tuple (no fill before the first fetch)
            w.declare(OpenWord::open("merge.join matched inner", Base::Words, self.w[W_MI as usize], Type::I64, 1));
        }
    }

    /// ENDOUTER: the inner rows left unmatched at the outer's end (merge/fill_inner.rs).
    fn flushes(&self) -> bool {
        self.fill_inner()
    }

    /// W8-D1236-CLASSA: ENDOUTER fetches the inner (an empty outer's too): a lazy inner still pending runs first, the call
    /// stopping by the flush's exit; the resumed one reaches the input's end again, its window re-reading the run.
    fn flush<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, carry: Carry<'e>, next: Next<'_, 'e>) -> Carry<'e> {
        self.run.first_fetch(b, env); // W8-EQUIVCLASS-SORTOP: ENDOUTER's inner call raises first (INITIALIZE_INNER's raised before)
        if let Some((consumed, lz)) = self.lazy {
            let r = RowVals { row: crate::cell::Row::new_in(env.mcx), vals: PgVec::new_in(env.mcx), cols: PgVec::new_in(env.mcx), carry: carry.copy_in(env.mcx), next: env.zero, at: env.zero };
            lz.wait(b, env, &r, self.offset, consumed);
        }
        // mergecmd RIGHT_OWN (held-out subq-0184): the outer's end has no current outer row, so no driving locator (its
        // values sit in the loop body, which does not dominate the input's end): a write end over the fill rows reads none
        // (merge/lower.rs sends a fill row, NULL in the target's junk ctid, to the NOT MATCHED actions)
        let loc = env.loc.take();
        let c = self.end_outer(b, env, carry, next);
        env.loc = loc;
        c
    }

    /// The inner's sealed run at each window: its row count.
    fn window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        let n = self.run.open(b, env, x);
        self.keep(b, env, n);
        self.n.set(Some(n));
    }

    fn expand(&self) -> Option<Expand> {
        Some(Expand { kind: ExpandKind::Chain, offset: self.offset, held: self.held })
    }

    /// MERGE-3d: one machine per join (its state, mark and matched words are one scan's), as PG never runs a merge join
    /// over a partial outer's share: an outer that could run in parallel (a clauseless join's unsorted outer: FULL JOIN
    /// ON true / false) runs serial on the leader. D3190: the co-split form runs one machine per claim, a key range of
    /// both inputs that no equal key straddles, so it runs wide.
    fn serial(&self) -> bool {
        let shape = if self.cosplit { gravity_program::StepShape::MergeCoSplit } else { gravity_program::StepShape::Merge };
        gravity_program::step_serial(&shape).is_some()
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        // D3269 (a): a recheck's stored COPY row enters the chain at its SubqueryScan before any window start (frame.rs
        // `copy_entry`, the subquery the merge's outer): the merge opens its inner's run there itself, the run the recheck
        // reran; the window's own start sets it again for the source's rows
        let n = match self.n.take() {
            Some(n) => n,
            None => {
                let x = env.x;
                self.window(b, env, &x);
                self.n.take().unwrap_or_else(|| crate::refuse!(self.node, Reason::Bug { payload: "a merge row before its window" }))
            }
        };
        self.n.set(Some(n));
        let ctypes: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let [l_mach, l_out, l_fill, l_done, l_emit, l_odone, l_ieoj] = [(); 7].map(|_| b.create_block(&ctypes));
        let l_ifill = self.fill_inner().then(|| b.create_block(&ctypes));
        let types: PgVec<'_, Type> = row.vals.iter().map(|&(v, _)| b.value_type(v)).collect_in(self.mcx);
        self.outer.borrow_mut().get_or_insert_with(|| (mcx::to_vec_in(&row.row.cells[..], self.mcx), types, mcx::to_vec_in(&row.cols[..], self.mcx)));
        let cp = row.carry.params();
        let memo = env.memo.set_aside();
        let later = later_set(b, env);
        // a resumed row continues past what it handed on last, or hands it on again (D688)
        let off = env.load_word(b, self.offset);
        let (l_fresh, l_res) = (b.create_block(&[]), b.create_block(&[]));
        let fresh = b.icmp(IntCond::Eq, off, env.zero);
        b.cond_br(fresh, (l_fresh, &[]), (l_res, &[]));
        b.switch_to(l_res);
        // the last hand-on (the outer's fill, the inner's, else a joined row) again, or past it
        let arms = [(FILLED, l_fill, l_out)].into_iter().chain(l_ifill.map(|l| (FILLED_INNER, l, l_mach)));
        for (v, again, past) in arms {
            let (f, on, l_else) = (b.iconst(Type::I64, v), b.create_block(&[]), b.create_block(&[]));
            let is = b.icmp(IntCond::Eq, off, f);
            b.cond_br(is, (on, &[]), (l_else, &[]));
            b.switch_to(on);
            match later {
                Some(x) => b.cond_br(x, (again, &cp), (past, &cp)),
                None => b.br(past, &cp),
            }
            b.switch_to(l_else);
        }
        match later {
            Some(x) => b.cond_br(x, (l_emit, &cp), (l_mach, &cp)),
            None => b.br(l_mach, &cp),
        }
        // a fresh row enters where the machine fetches the next outer tuple
        b.switch_to(l_fresh);
        self.set(b, env, W_MO, 0);
        let st = self.word(b, env, W_ST);
        let l_eval = b.create_block(&ctypes);
        let cases = [(DONE, l_done), (ENDINNER, l_fill)].into_iter().chain(l_ifill.map(|_| (ENDOUTER, l_out))); // ENDOUTER: PG fetches no further outer tuple
        b.switch(st, cases.map(|(v, t)| (v, t, &cp[..])), (l_eval, &cp));
        b.switch_to(l_eval);
        let c = Carry::from_params(b.block_params(l_eval), env.mcx);
        let r = self.eval(b, env, row, None, true);
        let (l_m, l_nm, l_eoj) = (b.create_block(&ctypes), b.create_block(&ctypes), b.create_block(&ctypes));
        three(b, r, (l_m, l_nm, l_eoj), false, &c.params());
        b.switch_to(l_m);
        let st = self.word(b, env, W_ST);
        // INITIALIZE_OUTER -> INITIALIZE_INNER, NEXTOUTER -> TESTOUTER, SKIPOUTER_ADVANCE -> SKIP_TEST
        let [io_c, no_c, to_c, sk_c, ii_c] = [INIT_OUTER, NEXTOUTER, TESTOUTER, SKIP_TEST, INIT_INNER].map(|v| b.iconst(Type::I64, v as i64));
        let (io, no) = (b.icmp(IntCond::Eq, st, io_c), b.icmp(IntCond::Eq, st, no_c));
        let to = b.select(no, to_c, sk_c);
        let to = b.select(io, ii_c, to);
        env.store_word(b, self.w[W_ST as usize], to);
        let p = mcx::to_vec_in(&b.block_params(l_m)[..], env.mcx);
        b.br(l_mach, &p);
        b.switch_to(l_nm);
        let p = mcx::to_vec_in(&b.block_params(l_nm)[..], env.mcx);
        b.br(if self.fill_outer() { l_fill } else { l_out }, &p);
        b.switch_to(l_eoj);
        let p = mcx::to_vec_in(&b.block_params(l_eoj)[..], env.mcx);
        self.st(b, env, if self.fill_inner() { ENDOUTER } else { DONE });
        b.br(if self.fill_inner() { l_out } else { l_done }, &p);
        // D3379 (mergecmd_rowc_cte :48): the outer row stays in reach of a writing CTE's MERGE redo in the IR (merge/epq.rs
        // SORTED `held_source`: a merge clause's source column its projection does not carry), as nest.loop's outer row
        env.redo_outers.push_in((self.node, (mcx::to_vec_in(&row.row.cells[..], env.mcx), mcx::to_vec_in(&row.vals[..], env.mcx), mcx::to_vec_in(&row.cols[..], env.mcx), crate::frame::env::Filled::No)));
        self.machine(b, env, row, n, [l_mach, l_emit, l_odone, l_ieoj], l_ifill, &ctypes);
        self.tails(b, env, row, [l_emit, l_fill, l_odone, l_ieoj, l_done, l_mach, l_out], l_ifill, next);
        env.redo_outers.pop();
        b.switch_to(l_out);
        env.memo.restore(memo);
        env.store_word(b, self.offset, env.zero);
        Carry::from_params(b.block_params(l_out), env.mcx)
    }
}

impl Merge<'_> {
    pub(crate) fn st(&self, b: &mut FunctionBuilder<'_>, env: &Env, v: u64) {
        self.set(b, env, W_ST, v as i64);
    }

    /// Stores state `v` and loops back to the machine with carry `c`.
    pub(crate) fn to(&self, b: &mut FunctionBuilder<'_>, env: &Env, v: u64, l_mach: Block, c: &[Value]) {
        self.st(b, env, v);
        b.br(l_mach, c);
    }
}
