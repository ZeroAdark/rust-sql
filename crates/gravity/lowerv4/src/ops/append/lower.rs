//! Append (cards/M2.md L6 `src.append`; the ladder's subplan.rs `lower_arms` and v1's `UnionSource` as the program forms,
//! not the code: here every arm is a pipeline of the one program). The decomposer streams a union (append.rs `push_union`):
//! each arm is its child's pipeline with an `AppendDesc::Arm { i, n, ordered, prune }` marker after the child's ops, then
//! the parent's ops copied, ending in the parent's end; the arms of an aggregate share the group the last arm declares
//! (graph.rs `group_at`), a build the arms read is freed after the last (decompose bumps its reads). The marker is this
//! file's step: the row goes on unchanged (the child's projection names the union's columns, which the parent's ops read
//! as `OUTER_VAR`), under
//! - the arm's skip bit when the Append prunes (source/prune.rs declares the words, the runtime's `src.prune` sets them
//!   once per execution): a pruned arm passes no row (and its claim source hands out nothing);
//! - the arms protocol (source/append_arms.rs, road `append.arms`), declared at arm 0 over the union's arms.
//! A union the parent's ops cannot be copied into keeps its Materialize form (`settle_arms`: Feed -> spool -> Union
//! emit); its spool is nest.spool's (M8): the feed end, the emit's source and its ops are made by that kind.
//! Registers kind `src.append` (step, source and end faces). Owner M2-SOURCES.
use std::cell::Cell;

use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::Op;
use gravity_abi::breaker::{Breaker, BreakerKind, Spill};
use gravity_program::protos::NO_SLOT;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use mcx::{CollectIn, PgBox, PgVec};

use crate::face::end::{EndMech, Parallel};
use crate::face::kinds::KindId;
use crate::face::source::SourceMech;
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::{EndMaker, Next, SourceMaker, StepMaker};
use crate::frame::frame::WordWin;
use crate::frame::px::Px;
use crate::frame::row::{Carry, Mask, RowVals};
use crate::frame::stop::Exits;
use crate::frame::iface::{breaker_of, kind_impl};
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;

pub(crate) const ROWS: Table = Table {
    kinds: &[(KindId::SrcAppend, &StepMaker(make_arm)), (KindId::SrcAppend, &SourceMaker(make_settled)), (KindId::SrcAppend, &EndMaker(make_feed))],
    ..crate::ops::NONE
};

/// The arm marker: its skip bit's word and bit when the Append prunes.
struct Arm {
    skip: Option<(u16, u32)>,
    pass: Cell<Option<Value>>,
    node: i32,
    /// F-ARMS: an arm of a resident keyed end's run: its pipeline runs on the leader. D2121 (rerun/inner_limit.rs
    /// `union_reset`): also the first arm of a union a rerun set re-runs (one open per run; the set's owner clears the
    /// union's written flag before each run).
    serial: bool,
    /// Construct C2: the whole-read licence of a feeder run before PostgreSQL starts the arm (later_arms.rs `fenced`,
    /// D2697); `None`: none runs early. Held, not read.
    #[allow(dead_code)]
    ahead: Option<gravity_decompose::effects::Licence>,
}

/// Op `j` of pipeline `k`'s plan node: the Append's for its marker.
fn node_of(px: &Px<'_, '_>, k: usize, j: usize) -> i32 {
    let p = &px.gx.pipelines()[k];
    p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(p.node_id, |n| n.node_id)
}

fn make_arm<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let gx = px.gx;
    let node = node_of(px, k, j);
    match &gx.pipelines()[k].ops[j] {
        Op::Append(AppendDesc::Arm { i, n, prune, .. }) => {
            // D393: the arms of a fold are one production: the Arms protocol names the breaker slot, the driver opens it at
            // the first arm and folds the rest on into it (node.rs `arms_of`). D393b: a keyed end with an owner phase and a
            // partitioned spill continues too (an arm that is not the last scatters its records into the slot's files, the
            // last arm's owner fold reads them all; its serial owner fold is perf debt D404); F-ARMS: a resident keyed end with an owner
            // phase (a set side's) continues too, handing its phase-1 tables on (D1238). V4R-159: a full sort's arms
            // continue its slot (runs/sort.rs holds each arm's run resident, exec `ArmSort` merges them once). A top-k,
            // build or owner-less keyed end, and arms that are not consecutive pipelines, refuse by name.
            // D1238 (NEW-53's union arms): a resident keyed end's arms (its records keep key copies, emit_key.rs `resident`,
            // or a set side's) run at the run's width: a continuing arm hands each worker's phase-1 table, and before a
            // section the leader's from an earlier serial arm, into the slot's stream (driver `Node::hand_off` /
            // `hand_leader`: in memory under the statement's grant, else in disk files at the run's partition count), and the
            // last arm's owner phase (or a serial last arm's own_single) folds them with its own partials.
            // D1387: a union nested in an arm (an inner Append whose arms stream through the outer arm's projection, a
            // coercion keeping the planner from flattening them) puts two markers in its pipelines. The run is the
            // pipeline's outermost (last) marker's: the consecutive pipelines walking that union's arms, one `Arms`
            // declaration over all of them; an inner marker declares none (two overlapping runs had the driver finish the
            // slot at the inner run's first arm, the outer run's last, and fold the rest into a finished table: kind 2).
            let top = top_arm(px, k).is_some_and(|t| t.0 == j);
            // D1753: a later arm whose feeders act, under a LIMIT or an end that may stop early (later_arms.rs); D2697:
            // one whose feeders run early for a whole reader holds the whole-read licence (`Arm.ahead`)
            let ahead = crate::ops::append::later_arms::fenced(px, k, j, node, *i).unwrap_or_else(|why| crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) }));
            let run = if top { top_run_once(px, k, node) } else { Some((k, 1)) };
            // arms that are not consecutive pipelines (an arm's build between two arms): the union's own count, as before
            // (a fold then refuses by name below, arms into the sink run as scheduled)
            let (first, len) = run.unwrap_or((k.saturating_sub(*i as usize), *n as usize));
            // V4R-533: the arms of a right-family join's probe end in its drain's spool: join/drain.rs makes each arm's end (the
            // drain's sink fused, or its continuing end seeded from the first arm through the drain), so they declare nothing
            let drained = gx.drained_arms(k).is_some();
            let slot = match breaker_of(&gx.pipelines()[k].desc) {
                Some(_) if drained => NO_SLOT,
                Some(b) if *n > 1 || len > 1 => {
                    // with#170 / #171: the arms of a recursive union's UNION ALL term append in turn to its intermediate
                    // table (setop.recursive's table end, each arm a member of the term's set: graph.rs `term_arm`)
                    let term = (first..first + len).all(|a| gx.term_arm(a).is_some());
                    if !term && (!continues(&gx.pipelines()[k].desc) || !Parallel::Arms.on() || !one_run_once(px, first, len, b)) {
                        crate::refuse!(node, Reason::Shape { why: ShapeWhy("a top-k / build end over a streamed union's arms (D393: only a fold's, a full sort's or an owned keyed end's arms continue one slot)") });
                    }
                    b.slot
                }
                _ => NO_SLOT,
            };
            // the ops above the marker (the parent's, copied into every arm) keep their per-open words per union, not per arm
            // (a LIMIT / OFFSET counts across the arms: step/limit.rs `Scope::Union`); the union is the Append's plan node
            px.set_union(k, node as u32, j);
            let reset = crate::rerun::inner_limit::union_reset(px, (k, j), node, run, top);
            // D1758: a fused drain's tail (join/drain.rs: the probe, or its spill pass, runs the drain's ops) makes this marker
            // again under the probe's identity; the run is the union's own pipelines, declared when the drain pipeline makes
            // it, so a tail declares none (its run would start at the probe, and seeded_arms would stretch it past the program)
            if top && k == first && Parallel::Arms.on() && !drained && !px.tailing() {
                crate::ops::append::strategies::arms::declare(px, k, len as u16, slot);
            }
            let skip = prune.as_ref().map(|p| crate::ops::append::prune::declare(px, k, *i, *n, p, node));
            mcx::pg_box_dyn!(px.mcx(), Arm { skip, pass: Cell::new(None), node, serial: reset, ahead } => dyn StepMech + 'g)
        }
        Op::Append(AppendDesc::Feed { .. } | AppendDesc::Union { .. }) => (kind_impl::<StepMaker>(&node, KindId::NestSpool).0)(px, k, j),
        _ => crate::refuse!(node, Reason::Bug { payload: "an append entry over another op" }),
    }
}

/// An end whose arms continue one slot: a fold (D393), a full sort's or a bounded incremental top's run (V4R-159: exec `ArmSort` holds each arm's run and
/// merges them at the last), or a keyed end with an owner phase that spills by partition (D393b) or stays resident (F-ARMS:
/// a set side's, SETOP row 2, or a key-copying group's, D1352; its continuing arms hand their phase-1 tables to the last
/// arm's owner phase, driver `Node::hand_off`, D1238).
fn continues(d: &gravity_program::Pipeline) -> bool {
    match breaker_of(d) {
        Some(b) => match b.kind {
            // D2077: a bounded incremental sort's top over an ordered union's arms (decompose append.rs `copies`): each arm's
            // run is cut to its own top (its stop per arm), described as a full sort's run, so exec `ArmSort` merges them
            BreakerKind::PlainAgg | BreakerKind::Sort | BreakerKind::TopK => true,
            BreakerKind::HashAgg | BreakerKind::Distinct => d.owner.is_some() && matches!(b.spill, Spill::Partitioned { .. } | Spill::None),
            _ => false,
        },
        None => false,
    }
}

/// The `len` pipelines from `first` (a run `top_run` found) each end in breaker `b` (its kind and slot) and continue it.
fn one_run(px: &Px<'_, '_>, first: usize, len: usize, b: Breaker) -> bool {
    let ps = px.gx.pipelines();
    (first..first + len).all(|q| ps.get(q).is_some_and(|p| matches!(breaker_of(&p.desc), Some(x) if x.kind == b.kind && x.slot == b.slot) && continues(&p.desc)))
}

/// Pipeline `q`'s outermost arm marker (its last `Arm` op: an inner union's marker comes first, after the inner child's
/// ops): the op index, the Append's plan node, the arm and the arm count.
fn top_arm(px: &Px<'_, '_>, q: usize) -> Option<(usize, i32, u16, u16)> {
    let p = px.gx.pipelines().get(q)?;
    let (j, i, n) = p.ops.iter().enumerate().rev().find_map(|(j, o)| match o {
        Op::Append(AppendDesc::Arm { i, n, .. }) => Some((j, *i, *n)),
        _ => None,
    })?;
    Some((j, node_of(px, q, j), i, n))
}

/// D1387: the run of pipeline `k`'s outermost union: the consecutive pipelines whose outermost marker is that union's
/// (its plan node and arm count), walking its arms in order (an arm's inner union gives it several pipelines), from arm 0
/// to arm `n - 1`: `(first, len)`, `None` when they do not.
fn top_run(px: &Px<'_, '_>, k: usize) -> Option<(usize, usize)> {
    let (_, node, _, n) = top_arm(px, k)?;
    let of = |q: usize| top_arm(px, q).filter(|t| t.1 == node && t.3 == n).map(|t| t.2);
    // pipeline `q + 1` continues `q`'s run: the same union at the same arm or the next
    let next = |q: usize| match (of(q), of(q + 1)) {
        (Some(a), Some(b)) => b == a || b == a + 1,
        _ => false,
    };
    let mut first = k;
    while first > 0 && next(first - 1) {
        first -= 1;
    }
    let mut last = k;
    while next(last) {
        last += 1;
    }
    (of(first) == Some(0) && of(last) == Some(n - 1)).then_some((first, last - first + 1))
}

/// `top_run` of pipeline `k` under union `node` (its outermost marker's plan node), walked once per union: the run's
/// other arms find it in the step face's state (step/expr.rs `StepExt::arm_runs`). A function of the graph alone, so the
/// one walk answers every arm in the run; walked per arm it made lowering O(arms^2) (m4_l20's 4100 partitions).
fn top_run_once(px: &mut Px<'_, '_>, k: usize, node: i32) -> Option<(usize, usize)> {
    if let Some(&(_, first, len)) = crate::step::expr::ext(px).arm_runs.iter().find(|r| r.0 == node && (r.1..r.1 + r.2).contains(&k)) {
        return Some((first, len));
    }
    let run = top_run(px, k)?;
    crate::step::expr::ext(px).arm_runs.push((node, run.0, run.1)); // alloc: Program
    Some(run)
}

/// `one_run`, checked once per run and breaker (step/expr.rs `StepExt::arm_one_runs`).
fn one_run_once(px: &mut Px<'_, '_>, first: usize, len: usize, b: Breaker) -> bool {
    let same = |r: &&(usize, usize, Breaker, bool)| r.0 == first && r.1 == len && r.2.kind == b.kind && r.2.slot == b.slot;
    if let Some(r) = crate::step::expr::ext(px).arm_one_runs.iter().find(same) {
        return r.3;
    }
    let holds = one_run(px, first, len, b);
    crate::step::expr::ext(px).arm_one_runs.push((first, len, b, holds)); // alloc: Program
    holds
}

/// The settled union's emit reads the arms' spool: nest.spool's (M8-NEST S2a, nest/union_arms.rs).
fn make_settled<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    (kind_impl::<SourceMaker>(&px.gx.pipelines()[k].node_id, KindId::NestSpool).0)(px, k)
}

/// A settled union's feed: its rows into the arms' spool, nest.spool's (M8-NEST S2a).
fn make_feed<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    (kind_impl::<EndMaker>(&px.gx.pipelines()[k].node_id, KindId::NestSpool).0)(px, k)
}

impl StepMech for Arm {
    /// The arm's verdict, once per call: its skip bit clear (no pruning: every row passes).
    fn prologue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _x: &Exits) {
        let Some((word, bit)) = self.skip else { return };
        let w = b.load(Type::I64, env.params, word as i32 * 8);
        let sh = b.iconst(Type::I64, bit as i64);
        let s = b.int_bin(IntBinOp::Ushr, w, sh);
        let s1 = b.int_bin(IntBinOp::And, s, env.one);
        self.pass.set(Some(b.icmp(IntCond::Eq, s1, env.zero)));
    }

    /// A pruned arm's block passes no row.
    fn block(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _w: &WordWin, _base: Value, m: Mask) -> Mask {
        match self.pass.get() {
            Some(p) => Mask(b.select(p, m.0, env.zero)),
            None => m,
        }
    }

    fn block_reads(&self, _env: &Env, _w: &WordWin) -> u64 {
        0
    }

    fn serial(&self) -> bool {
        self.serial
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let Some(pass) = self.pass.get() else {
            if self.skip.is_some() {
                crate::refuse!(self.node, Reason::Bug { payload: "an arm marker lowered before its prologue" });
            }
            return next(b, env, row);
        };
        let types: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let l_join = b.create_block(&types[..]);
        let l_yes = b.create_block(&[]);
        b.cond_br(pass, (l_yes, &[]), (l_join, &row.carry.params()));
        b.switch_to(l_yes);
        let c = next(b, env, row);
        b.br(l_join, &c.params());
        b.switch_to(l_join);
        Carry::from_params(b.block_params(l_join), env.mcx)
    }
}
