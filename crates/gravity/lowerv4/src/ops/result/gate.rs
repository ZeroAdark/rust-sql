//! One-time gates, kind `step.gate` (KEEP lowerv3 scan/gate.rs): a Result's constant qual, evaluated once per call before
//! any row, in plan order and stopping at the first false or NULL (`ExecQual` over `resconstantqual`), its raise raised
//! there. A false or NULL gate passes no row and pulls none (its window is done at once, D760); the end still opens and
//! finishes, so an aggregate over no rows emits its empty result. Budget 267 lines. Owner M1-STEP.
use std::cell::Cell;

use gravity_abi::ctx::{hdr, RESUME_NONE};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::setop::SetDesc;
use gravity_decompose::ops::shared::subplan::SubLink;
use gravity_decompose::{Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_lower::rerun::words::has_exec;
use mcx::{CollectIn, GrowIn, PgBox, PgVec};

use crate::frame::iface::{limit_check_bound, limit_stopping};
use crate::effects::visible::result::qual_calls;
use crate::ops::shared::limit::{bind, soft, Above, Lim};
use crate::face::kinds::{Face, KindId};
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::{Next, StepMaker};
use crate::frame::mechs::FeederMaker;
use crate::frame::px::{ExId, Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::rerun::union_arms::feeds_alone;
use crate::admit::refusal::{Reason, ShapeWhy};

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::StepGate, &StepMaker(make)), (KindId::StepGate, &FeederMaker(feeder_gate)), (KindId::StepLimit, &FeederMaker(limit_feeder))], ..crate::ops::NONE };

pub(crate) struct Gate<'g> {
    quals: PgVec<'g, ExId>,
    /// D1914 / D1755: the checks of the nodes above the gate in its own pipeline (`Above`), made before its quals.
    above: Above<'g>,
    node: i32,
    pass: Cell<Option<Value>>,
    /// D2709: a calling qual's verdict (frame word byte offset), stored by the call that opens and read by a resumed one
    /// (a FETCH past a suspended portal): PG evaluates it once per scan (rs_checkqual). Not an open word: the frame writes
    /// those after the prologue.
    verdict: Option<i32>,
    /// D2571 (fed): a calling gate a pipeline that is not quiet feeds alone. That pipeline's feeder evaluates the quals
    /// before it runs, as PG does, and leaves the verdict in this word (1 true, 2 false; `true`: the feeder's check, which
    /// evaluates only while the word is 0); the gate's own step reads it instead of evaluating and clears it (`false`), so a
    /// call where no feeder ran evaluates here.
    fed: Option<(i32, bool)>,
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let g = gate(px, k, j);
    let calling = matches!(px.gx.pipelines()[k].ops[j], Op::Gate(n) if qual_calls(px.gx, n));
    let verdict = calling.then(|| verdict_word(px, k, j));
    let fed = (calling && fed_by(px.gx, k, j)).then(|| fed_word(px, g.node, k, j, false));
    mcx::pg_box_dyn!(px.mcx(), Gate { above: Above::of(px, k, j), verdict, fed, ..g } => dyn StepMech + 'g)
}

fn verdict_word(px: &mut Px<'_, '_>, q: usize, j: usize) -> i32 {
    px.frame_word(WordKey::Open { face: Face::Step, k: q, j: crate::frame::px::key_j(j), i: 0 }) as i32 * 8
}

/// The fed verdict's word of pipeline `q`'s gate at op `j`. A remade pipeline keys its words apart, so its gate would not
/// read the word the feeder writes: refused.
fn fed_word(px: &mut Px<'_, '_>, node: i32, q: usize, j: usize, writes: bool) -> (i32, bool) {
    if px.making.0 != px.making.1 {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy(CALLING_GATE) });
    }
    (px.frame_word(WordKey::Open { face: Face::Step, k: q, j: crate::frame::px::key_j(j), i: 1 }) as i32 * 8, writes)
}

/// Whether a pipeline that is not quiet feeds pipeline `q`'s calling gate at op `j` alone and makes its check
/// (`limit_feeder`): its feeder then evaluates the quals.
fn fed_by(gx: &crate::graph::Gx<'_, '_>, q: usize, j: usize) -> bool {
    (0..q).any(|k| feeds_alone(gx, k, q, j, false) && {
        let r = Reach::of(gx, k);
        !r.quiet && r.holds(q, j)
    })
}

/// Pipeline `k`'s gate at op `j`, its quals compiled at the gate's node.
fn gate<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> Gate<'g> {
    let mcx = px.mcx();
    let p = &px.gx.pipelines()[k];
    let node = p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(p.node_id, |n| n.node_id);
    let Op::Gate(n) = p.ops[j] else { crate::refuse!(node, Reason::Bug { payload: "a gate entry over another op" }) };
    let quals = match n.as_list() {
        Some(l) => l.iter().map(|q| crate::step::expr::compile(px, q, None, node)).collect_in(mcx),
        None => mcx::pg_vec![in mcx; crate::step::expr::compile(px, n, None, node)],
    };
    Gate { quals, above: Above::none(mcx), node, pass: Cell::new(None), verdict: None, fed: None }
}

impl<'g> StepMech for Gate<'g> {
    /// The gate's verdict, once per open (a resumed call reads the stored one): each qual evaluated only while every earlier
    /// one was true.
    fn prologue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _x: &Exits) {
        let stop = self.above.stop(b, env);
        let none = RowVals { row: crate::cell::Row::new_in(env.mcx), vals: PgVec::new_in(env.mcx), cols: PgVec::new_in(env.mcx), carry: Carry::new(env.zero, env.mcx), next: env.zero, at: env.zero };
        let l_done = b.create_block(&[Type::Bool]);
        // D2056 / D1914 / D1755: under an outer check that stopped (the feeder's, a node's above in this pipeline), the
        // quals are never evaluated
        if let Some(stop) = stop {
            let (l_eval, f) = (b.create_block(&[]), b.bconst(false));
            b.cond_br(stop, (l_done, &[f]), (l_eval, &[]));
            b.switch_to(l_eval);
        }
        let words = self.verdict.map(|off| {
            let Some(w) = env.words else { crate::refuse!(self.node, Reason::Bug { payload: "a gate verdict without the frame words" }) };
            let (r, none_w) = (b.load(Type::I64, env.ctx, hdr::RESUME as i32), b.iconst(Type::I64, RESUME_NONE));
            let fresh = b.icmp(IntCond::Eq, r, none_w);
            let (l_eval, l_kept) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(fresh, (l_eval, &[]), (l_kept, &[]));
            b.switch_to(l_kept);
            let v = b.load(Type::I64, w, off);
            let kept = b.icmp(IntCond::Ne, v, env.zero);
            b.br(l_done, &[kept]);
            b.switch_to(l_eval);
            (w, off)
        });
        let fed = self.fed.map(|(off, writes)| {
            let Some(w) = env.words else { crate::refuse!(self.node, Reason::Bug { payload: "a fed gate without the frame words" }) };
            let v = b.load(Type::I64, w, off);
            let (set, one) = (b.icmp(IntCond::Ne, v, env.zero), b.iconst(Type::I64, 1));
            let (l_read, l_eval) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(set, (l_read, &[]), (l_eval, &[]));
            b.switch_to(l_read);
            let p = b.icmp(IntCond::Eq, v, one);
            b.br(l_done, &[p]);
            b.switch_to(l_eval);
            (w, off, writes)
        });
        let l_q = if fed.is_some_and(|f| f.2) { b.create_block(&[Type::Bool]) } else { l_done };
        for &q in self.quals.iter() {
            let p = crate::step::expr::pred(b, env, self.node, &none, q);
            let l_more = b.create_block(&[]);
            b.cond_br(p, (l_more, &[]), (l_q, &[p]));
            b.switch_to(l_more);
        }
        let t = b.bconst(true);
        b.br(l_q, &[t]);
        if let Some((w, off, true)) = fed {
            b.switch_to(l_q);
            let p = b.block_params(l_q)[0];
            let (one, two) = (b.iconst(Type::I64, 1), b.iconst(Type::I64, 2));
            let v = b.select(p, one, two);
            b.store(w, off, v);
            b.br(l_done, &[p]);
        }
        b.switch_to(l_done);
        let pass = b.block_params(l_done)[0];
        if let Some((w, off, false)) = fed {
            b.store(w, off, env.zero);
        }
        if let Some((w, off)) = words {
            let v = b.conv(ConvOp::Zext, Type::I64, pass);
            b.store(w, off, v);
        }
        self.pass.set(Some(pass));
    }

    /// A false or NULL gate's window is done at once: its source is never pulled, as PG's Result never pulls its outer
    /// (a raising filter below it is not evaluated, a volatile call below it not made: D760).
    fn window(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env, x: &Exits) {
        let Some(pass) = self.pass.get() else { crate::refuse!(self.node, Reason::Bug { payload: "a gate's window before its prologue" }) };
        let l_go = b.create_block(&[]);
        b.cond_br(pass, (l_go, &[]), (x.l_done, &[]));
        b.switch_to(l_go);
    }

    /// D2152: a false or NULL gate closes the call: its source is never opened (an index key below it never evaluated).
    fn closes(&self, b: &mut FunctionBuilder<'_>, _env: &Env) -> Option<Value> {
        let (pass, t) = (self.pass.get()?, b.bconst(true));
        Some(b.int_bin(IntBinOp::Xor, pass, t))
    }

    /// D2877: PG's Result evaluates its one-time qual at its first call whether or not its outer holds a row.
    fn first_call(&self) -> bool {
        true
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let Some(pass) = self.pass.get() else { crate::refuse!(self.node, Reason::Bug { payload: "a gate lowered before its prologue" }) };
        crate::step::filter::when(b, env, row, pass, next)
    }
}

/// D1716: PG's Result evaluates its one-time qual before it pulls its outer and, false or NULL, never pulls it; PG's
/// ExecLimit evaluates its bounds (`recompute_limits`: OFFSET, then LIMIT, a negative one raising) before it pulls its
/// subplan and, at a zero count, never pulls it. So no node below runs, however many blocking children (a Sort, a build, an
/// aggregate, an InitPlan: here pipelines that run before the node's) feed it. Every graph pipeline that feeds such a
/// node's input alone (`feeds_alone`) gets this step: its prologue makes the node's check (a raise raised there, before any
/// child row, as PG's) and, stopped, its window is done at once (D760): it pulls no row and its end closes empty, so an
/// aggregate over it emits its empty result. A NULL LIMIT is none. D1751: under several such nodes it makes every check,
/// outermost first as PG pulls (a later pipeline's, then a later op's), and the first that stops ends them (PG never
/// reaches those below). D1755: a gate over an InitPlan's parameter is checked here too, the InitPlan running at that read
/// as PG's runs at the gate's first evaluation (in a pipeline a mask may skip it stays with its own step: running the
/// InitPlan there may raise where PG never reaches the gate). A check read per open (a per-open bound) is left to its own
/// step (read after these pipelines run); the checks below it are still made here.
struct Feeder<'g> {
    checks: PgVec<'g, Check<'g>>,
    stop: Cell<Option<Value>>,
}

/// One node's check: a Limit's (count, offset, soft: a negative bound stops and never raises), or a one-time gate's quals.
enum Check<'g> {
    Limit(Lim, Lim, bool),
    Gate(Gate<'g>),
    /// D1753 / D1752 (`full_check`): a streamed union's LIMIT whose rows are already read, from the union's words: the byte
    /// offsets of the union's flag and of the LIMIT's skipped and emitted counts, and the rows it reads (count plus offset).
    Full { flag: i32, skipped: i32, emitted: i32, rows: i64 },
}

/// D2054: a check in a pipeline a mask may skip, made by a `k` that is not quiet, must not raise (PG may never reach it).
const SKIPPED_GATE_RAISES: &str = "a one-time gate whose qual may raise in a pipeline an empty build or left skips, above a pipeline that runs anyway and is not quiet (D2054)";

/// D2571: PG's Result evaluates its one-time qual once per call (ExecResult's `rs_checkqual`), before it pulls its outer, and
/// the gate's own step does so here. A feeder's check is a second evaluation, which a qual that calls (a user function, a
/// VOLATILE builtin, a row-reading xml builtin: `calls`) shows; such a gate makes no check in a quiet feeder, and a feeder
/// that is not quiet (PG never runs it past a false gate) makes the one evaluation, which the gate's step reads (`fed`).
/// Refused where that feeder may run while a mask skips the gate's pipeline, over an InitPlan's parameter, or remade.
const CALLING_GATE: &str = "a one-time gate whose qual calls a user or VOLATILE function above a pipeline that feeds it alone and is not quiet (PG evaluates the qual once per call, before that pipeline runs; a check there would evaluate it once more: D2571)";

/// step.gate's feeder row: step.limit's (`limit_feeder`) makes the gates' checks with the Limits' in one step (the frame
/// takes one feeder per pipeline, frame/mechs.rs), so this row makes none.
fn feeder_gate<'g>(_px: &mut Px<'g, '_>, _k: usize) -> Option<PgBox<'g, dyn StepMech + 'g>> {
    None
}

// `feeds_alone`, the feeder checks' one predicate, lives in rerun/union_arms.rs (with `full_feed` / `full_fed`, main
// 2026-10-04).

/// D1688: the pulled sets (the CTE's pipeline and every one feeding it) of the data-modifying CTEs that hold pipeline `k`.
/// PG runs such a CTE's ModifyTable to its end at the statement's end whatever its readers read (ExecPostprocessPlan), so
/// only a node inside the set (the CTE's own plan) stops `k`. A CTE is data-modifying when its own pipeline writes (the
/// ModifyTable's, which it spools), not when it reads one: its readers gate it, as they gate any other.
fn drained<'g>(gx: &crate::graph::Gx<'g, '_>, k: usize) -> PgVec<'g, PgVec<'g, usize>> {
    let ps = gx.pipelines();
    let into = |p: usize| {
        let src = if let Source::Breaker { slot } = ps[p].source { Some(slot) } else { None };
        ps[p].desc.consumes.iter().copied().chain(src).filter_map(move |s| gx.life_of(p, s)).map(|l| l.0 as usize).filter(|&l| l < ps.len())
    };
    let writes = |p: usize| p < ps.len() && ps[p].ops.iter().any(|op| matches!(op, Op::Write(_)));
    let ctes = gx.graph.init_plans.iter().filter(|i| matches!(i.link, SubLink::Cte) && writes(i.pipeline as usize));
    let sets = ctes.map(|i| {
        let (mut set, mut todo): (PgVec<'_, usize>, PgVec<'_, usize>) = (PgVec::new_in(gx.mcx), mcx::pg_vec![in gx.mcx; i.pipeline as usize]);
        while let Some(p) = todo.pop() {
            if !set.contains(&p) {
                set.push_in(p);
                todo.extend_in(into(p));
            }
        }
        set
    });
    sets.filter(|s| s.contains(&k)).collect_in(gx.mcx)
}

/// One run_empty mask (bit p = graph pipeline p) and the op that makes it, op `j` of pipeline `q`: a build-first probe's
/// outer producers, skipped over an empty build (join_build_class::outer, D276; `q` itself when its own source runs only
/// after the build), and a set operation's right side (lp, rp] over an empty left (setop.emptyleft, hashed or grouped
/// sorted). PG never reaches a node there then. The runner skips only a masked pipeline with a driving site.
#[derive(Clone, Copy)]
struct Mask {
    bits: u64,
    q: usize,
    j: usize,
}

impl Mask {
    /// Whether the mask may skip op `j` of pipeline `q` where PG never reaches it. D2148: an op of the masking pipeline past
    /// its probe (a Limit, an OFFSET or a one-time gate above the join) PG reaches whatever the build holds, as its Limit or
    /// Result makes its check before it pulls the join; the runner skipping that pipeline whole over an empty build does not
    /// make PG skip it.
    fn skips(&self, q: usize, j: usize) -> bool {
        bit(self.bits, q) && !(q == self.q && j > self.j)
    }
}

fn masks<'g>(gx: &crate::graph::Gx<'g, '_>) -> PgVec<'g, Mask> {
    let walk = || {
        let life = |q: usize, s: u8| gx.life_of(q, s).map(|l| l.0 as usize);
        let of = |q: usize, j: usize, op: &Op<'_>| match op {
            Op::Join(JoinDesc::Probe { slot, .. }) => life(q, *slot).map_or(0, |b| crate::frame::iface::probe_skips(gx, b, q, j)),
            Op::SetOp(SetDesc::Set { left, right, .. }) => life(q, *left).zip(life(q, *right)).map_or(0, |(l, r)| (l + 1..=r.min(63)).fold(0, |m, x| m | 1u64 << x)),
            _ => 0,
        };
        let all = gx.pipelines().iter().enumerate().flat_map(|(q, p)| p.ops.iter().enumerate().map(move |(j, op)| Mask { bits: of(q, j, op), q, j }));
        all.filter(|m| m.bits != 0).collect_in(gx.mcx)
    };
    // D3585: in the graph's lowering the masks are found once (memo.rs `masks`), not by a walk of every op for each
    // feeder (an Append's every arm under a LIMIT asked: quadratic in the arms)
    let key = crate::memo::key_of(&gx.graph.pipelines);
    let over = gx.over.map_or(0, |o| o as *const _ as usize);
    let Some(kept) = crate::memo::read(key, |m| m.masks.clone()) else { return walk() };
    let kept = match kept {
        Some((o, v)) if o == over => v,
        _ => {
            let v: std::rc::Rc<[(u64, u32, u32)]> = walk().iter().map(|m| (m.bits, m.q as u32, m.j as u32)).collect(); // alloc: Program
            crate::memo::write(key, |m| m.masks = Some((over, v.clone())));
            v
        }
    };
    kept.iter().map(|&(bits, q, j)| Mask { bits, q: q as usize, j: j as usize }).collect_in(gx.mcx)
}

/// D2115 x D1911 (semantics): a grouping-sets phase's spool walk computes every input row's aggregate arguments, a
/// SubPlan's among them, as pipeline `k` runs (subplan/per_row.rs `order` reads such a SubPlan lazily there, in PG's
/// per-row order). Under a Limit whose count may be zero (a constant 0 or a parameter: `limit_stopping`) PG runs none of it, as
/// ExecLimit never pulls its subplan at a zero count; `k`'s feeder stops it before its first row only when it makes that
/// Limit's check: the Limit sits in a pipeline after `k` whose input `k` feeds alone, no mask may skip it, and its bounds
/// are read at bind (kit K18.x09 .l0 / .gl, two empty grouping sets with no Sort above: through every arm of the union
/// they stream into the sink, `feeds_alone`). A spool walk whose SubPlan acts or may raise refuses by this name when one
/// such Limit's check would not reach it.
const SPOOL_ZERO: &str = "an aggregate argument's VOLATILE or raising SubPlan in a grouping-sets spool walk below a LIMIT that may count 0 and whose zero check does not reach the walk (D2115)";

/// Pipeline `k` is a grouping-sets phase's spool walk: it ends in a key-less Sort the sets' groups read.
fn spool_walk(gx: &crate::graph::Gx<'_, '_>, k: usize) -> bool {
    let p = &gx.pipelines()[k];
    matches!(p.ops.last(), Some(Op::Sort(s)) if s.keys.is_empty()) && p.desc.produces().is_some()
}

/// The feeder of graph pipeline `k`: the check of every Limit whose count may be zero or a bound negative and of every
/// one-time gate whose input `k` feeds alone, outermost first; in a data-modifying CTE's pulled set, only the nodes inside
/// it (D1688). A `quiet` `k` that runs over an empty build or left makes no check in a pipeline the mask skips then
/// (`masks`): that node's own step makes it when its pipeline runs, as PG reaches it only then. A `k` that is not quiet
/// makes it (a zero count or a false gate stops it: PG never runs it then, empty build or not) but never raises there:
/// a negative bound stops it too (the node's own step raises when its pipeline runs), a gate whose qual may raise refuses.
fn limit_feeder<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> Option<PgBox<'g, dyn StepMech + 'g>> {
    let gx = px.gx;
    // D2115 x D1911: a spool walk's loud SubPlan runs only past every zero-capable Limit's check (`SPOOL_ZERO`)
    if spool_walk(gx, k) && crate::effects::arg_subplan_loud(gx, k) {
        let ps = gx.pipelines();
        for q in 0..ps.len() {
            for j in 0..ps[q].ops.len() {
                if limit_stopping(&ps[q].ops[j]).is_some() && !checks_limit(px, k, q, j) {
                    crate::refuse!(ps[k].node_id, Reason::Shape { why: ShapeWhy(SPOOL_ZERO) });
                }
            }
        }
    }
    // D2907: no Limit and no gate after `k`, no check: `Reach`'s readings (`quiet` reads every source cell, a long VALUES
    // list's too) are not taken
    let ps = gx.pipelines();
    let checked = |op: &Op<'_>| limit_stopping(op).is_some() || crate::graph::step_kind(op) == KindId::StepLimit || matches!(op, Op::Gate(_));
    // D3585: the ops that may make a check (only those can: the match below takes nothing else), in the graph's lowering
    // found once (memo.rs `check_ops`), not by a walk of every later pipeline's ops for each feeder (an Append's every arm
    // under a LIMIT: quadratic in the arms)
    let walk = || (0..ps.len()).flat_map(|q| (0..ps[q].ops.len()).filter(move |&j| checked(&ps[q].ops[j])).map(move |j| (q as u32, j as u32))).collect::<std::rc::Rc<[(u32, u32)]>>(); // alloc: Program
    let key = crate::memo::key_of(&gx.graph.pipelines);
    let ops = match crate::memo::read(key, |m| m.check_ops.clone()) {
        Some(Some(ops)) => ops,
        Some(None) => {
            let ops = walk();
            crate::memo::write(key, |m| m.check_ops = Some(ops.clone()));
            ops
        }
        None => walk(),
    };
    if !ops.last().is_some_and(|&(q, _)| q as usize > k) {
        return None;
    }
    let r = Reach::of(gx, k);
    let mut checks: PgVec<'g, Check<'g>> = PgVec::new_in(gx.mcx);
    let from = ops.partition_point(|&(q, _)| q as usize <= k);
    for &(q, j) in ops[from..].iter().rev() {
        let (q, j) = (q as usize, j as usize);
        let qp = &gx.pipelines()[q];
        if r.holds(q, j) {
            let masked = r.masked(q, j);
            let check = match &qp.ops[j] {
                op if limit_stopping(op).is_some() => limit_check(px, k, q, j, masked),
                op if crate::graph::step_kind(op) == KindId::StepLimit => full_check(px, k, q, j),
                // D2571: a qual that calls is evaluated once per call: by the gate's own step over a quiet `k`, else by this
                // check, whose verdict the gate's step reads (`Gate::fed`)
                Op::Gate(n) if feeds_alone(gx, k, q, j, false) && qual_calls(gx, *n) => {
                    let exec = n.as_list().map_or_else(|| has_exec(*n), |l| l.iter().any(has_exec));
                    match r.quiet {
                        true => None,
                        false => {
                            let mut g = gate(px, q, j);
                            if masked || exec {
                                crate::refuse!(g.node, Reason::Shape { why: ShapeWhy(CALLING_GATE) });
                            }
                            g.verdict = Some(verdict_word(px, q, j));
                            g.fed = Some(fed_word(px, g.node, q, j, true));
                            Some(Check::Gate(g))
                        }
                    }
                }
                // D1755: a qual over a per-execution parameter (an InitPlan's) too, but not where a mask may skip `q`
                Op::Gate(n) if feeds_alone(gx, k, q, j, false) => {
                    let (exec, rf) = n.as_list().map_or_else(|| (has_exec(*n), crate::effects::raise_free(gx, *n)), |l| (l.iter().any(has_exec), l.iter().all(|c| crate::effects::raise_free(gx, c))));
                    let g = (!exec).then(|| gate(px, q, j));
                    if let Some(g) = g.as_ref().filter(|_| masked && !rf) {
                        crate::refuse!(g.node, Reason::Shape { why: ShapeWhy(SKIPPED_GATE_RAISES) });
                    }
                    g.or_else(|| (!masked).then(|| gate(px, q, j))).map(Check::Gate)
                }
                _ => None,
            };
            checks.extend_in(check);
        }
    }
    (!checks.is_empty()).then(|| mcx::pg_box_dyn!(px.mcx(), Feeder { checks, stop: Cell::new(None) } => dyn StepMech + 'g))
}

/// The later pipelines whose checks the feeder of graph pipeline `k` makes (`limit_feeder`): inside every data-modifying
/// CTE's pulled set holding `k` (D1688) and, for a `quiet` `k`, none a mask skips (`skips`); `kept`, the masks that may skip
/// a pipeline whose check a `k` that is not quiet still makes.
struct Reach<'g> {
    sets: PgVec<'g, PgVec<'g, usize>>,
    skips: PgVec<'g, Mask>,
    kept: PgVec<'g, Mask>,
    quiet: bool,
}

impl<'g> Reach<'g> {
    fn of(gx: &crate::graph::Gx<'g, '_>, k: usize) -> Reach<'g> {
        let (runs, quiet) = (gx.pipelines()[k].desc.driving_site < 0, crate::effects::visible::result::quiet(gx, k, crate::frame::iface::end_breaker(gx, k)));
        let (mut skips, mut kept) = (PgVec::new_in(gx.mcx), PgVec::new_in(gx.mcx));
        for m in masks(gx).into_iter().filter(|m| runs || !bit(m.bits, k)) {
            if quiet { skips.push_in(m) } else { kept.push_in(m) }
        }
        Reach { sets: drained(gx, k), skips, kept, quiet }
    }

    /// Whether `k`'s feeder makes op `j` of pipeline `q`'s check at all.
    fn holds(&self, q: usize, j: usize) -> bool {
        self.sets.iter().all(|s| s.contains(&q)) && !self.skips.iter().any(|m| m.skips(q, j))
    }

    /// Whether a kept mask may skip op `j` of pipeline `q` (its check then never raises).
    fn masked(&self, q: usize, j: usize) -> bool {
        self.kept.iter().any(|m| m.skips(q, j))
    }
}

/// Bit `x` of a run_empty mask.
fn bit(m: u64, x: usize) -> bool {
    x < 64 && m >> x & 1 == 1
}

/// The check `k`'s feeder makes for op `j` of pipeline `q` when it is a Limit whose count may be zero or a bound negative
/// (`limit_stopping`) and `k` feeds its input alone, both bounds bound before the feeders run; `masked`: a kept mask may skip `q`.
fn limit_check<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, q: usize, j: usize, masked: bool) -> Option<Check<'g>> {
    let gx = px.gx;
    let (c, o) = limit_stopping(&gx.pipelines()[q].ops[j]).filter(|_| feeds_alone(gx, k, q, j, true))?;
    bind(px, q, c).zip(bind(px, q, o)).map(|(c, o)| Check::Limit(c, o, masked))
}

/// D1753 / D1752 (main's go 2026-10-04): PG's Append starts an arm only when its reader pulls past the previous one, so
/// under a LIMIT whose rows are already read it never runs a later arm's children (a later partition's Hash, a
/// HashAggregate's fill). Graph pipeline `k`'s check on op `j` of pipeline `q`, the LIMIT that reads a streamed union, when
/// rerun/union_arms.rs `full_feed` takes it (its conditions live there, one source with `full_fed`, which the fences
/// read): `k`'s prologue reads the union's words (limit/lower.rs: words 0 and 1, the skipped and emitted counts, written
/// under the union's flag) and stops `k` when the flag is set and the rows are read. The flag is clear until the first arm
/// opens in this execution, so a pipeline running before every arm (decompose's numbering, or arm 0's own feeders) never
/// stops; one running after an earlier arm (frame/mechs.rs `arm_order`) stops only where PG never starts its arm. The
/// arm's copies then pull nothing either (their `window`).
fn full_check<'g>(px: &mut Px<'g, '_>, k: usize, q: usize, j: usize) -> Option<Check<'g>> {
    let f = crate::rerun::union_arms::full_feed(px, k, q, j)?;
    let mut word = |i: u8| px.frame_word(WordKey::Union { union: f.union as u32, after: f.after, i }) as i32 * 8;
    let (skipped, emitted) = (word(0), word(1));
    let flag = px.frame_word(WordKey::UnionFlag { union: f.union as u32 }) as i32 * 8;
    Some(Check::Full { flag, skipped, emitted, rows: f.rows })
}

/// Whether the feeder of graph pipeline `k` makes op `j` of pipeline `q`'s check at all (`Reach::holds`: inside every
/// data-modifying CTE's pulled set holding `k`, D1688; for a quiet `k`, no mask skips it). rerun/union_arms.rs `full_fed`
/// reads it beside `full_feed` (D1753 / D1752).
pub(crate) fn reaches(gx: &crate::graph::Gx<'_, '_>, k: usize, q: usize, j: usize) -> bool {
    Reach::of(gx, k).holds(q, j)
}

/// Whether the feeder of graph pipeline `k` makes the check of Limit op `j` of pipeline `q` (`limit_feeder`'s verdict for
/// that op): a zero count there then stops `k` before it pulls a row and a negative bound raises or stops there, as PG's
/// Limit evaluates its bounds before it pulls its subplan (rerun/union_arms.rs: a union whose arms PG reads all or none).
pub(crate) fn checks_limit(px: &mut Px<'_, '_>, k: usize, q: usize, j: usize) -> bool {
    let r = Reach::of(px.gx, k);
    q > k && r.holds(q, j) && limit_check(px, k, q, j, r.masked(q, j)).is_some()
}

/// Whether the feeder of graph pipeline `k` makes the check of one-time gate op `j` of pipeline `q` whose qual neither
/// calls nor reads a per-execution parameter (`limit_feeder`'s verdict for that op): a false or NULL gate then stops `k`
/// before it pulls a row, as PG's Result evaluates its one-time qual before it pulls its outer.
pub(crate) fn checks_gate(px: &mut Px<'_, '_>, k: usize, q: usize, j: usize) -> bool {
    let gx = px.gx;
    let Op::Gate(n) = gx.pipelines()[q].ops[j] else { return false };
    let exec = n.as_list().map_or_else(|| has_exec(n), |l| l.iter().any(has_exec));
    let r = Reach::of(gx, k);
    q > k && !exec && !qual_calls(gx, n) && r.holds(q, j) && !r.masked(q, j) && feeds_alone(gx, k, q, j, false)
}

/// Construct C2 (b), the run-ahead tracer's stamp only (frame/program.rs `checks_results`): whether the feeder of graph
/// pipeline `k` makes the check of one-time gate op `j` of pipeline `q`, by `limit_feeder`'s own arms, wider than
/// `checks_gate`. A qual that calls is checked there by a `k` that is not quiet (D2571; a quiet `k` leaves it to the
/// gate's own step). A qual that does not call is checked there unless it reads a per-execution parameter in a
/// pipeline a mask may skip (D1755: over an InitPlan's parameter, the InitPlan runs at that read).
pub(crate) fn feeder_checks_gate(px: &mut Px<'_, '_>, k: usize, q: usize, j: usize) -> bool {
    let gx = px.gx;
    let Op::Gate(n) = gx.pipelines()[q].ops[j] else { return false };
    let r = Reach::of(gx, k);
    if q <= k || !r.holds(q, j) || !feeds_alone(gx, k, q, j, false) {
        return false;
    }
    let exec = n.as_list().map_or_else(|| has_exec(n), |l| l.iter().any(has_exec));
    if qual_calls(gx, n) {
        !r.quiet && !r.masked(q, j) && !exec
    } else {
        !exec || !r.masked(q, j)
    }
}

impl<'g> StepMech for Feeder<'g> {
    fn prologue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        let l_stop = b.create_block(&[Type::Bool]);
        for c in self.checks.iter() {
            // the check's verdict and whether it stops when true (a zero count) or when false (a false or NULL gate)
            let (v, on) = match c {
                Check::Limit(count, offset, false) => {
                    let _ = limit_check_bound(b, env, *offset, 0);
                    let Some(n) = limit_check_bound(b, env, *count, 1) else { continue };
                    (b.icmp(IntCond::Eq, n, env.zero), true)
                }
                Check::Limit(count, offset, true) => {
                    let (o, n) = (soft(b, env, *offset, IntCond::Slt), soft(b, env, *count, IntCond::Sle));
                    (b.int_bin(IntBinOp::Or, o, n), true)
                }
                Check::Gate(g) => {
                    g.prologue(b, env, x);
                    let Some(pass) = g.pass.get() else { continue };
                    (pass, false)
                }
                Check::Full { flag, skipped, emitted, rows } => {
                    let Some(w) = env.words else { continue };
                    let f = b.load(Type::I64, w, *flag);
                    let set = b.icmp(IntCond::Ne, f, env.zero);
                    let (sk, em) = (b.load(Type::I64, w, *skipped), b.load(Type::I64, w, *emitted));
                    let read = b.int_bin(IntBinOp::Add, sk, em);
                    let n = b.iconst(Type::I64, *rows);
                    let met = b.icmp(IntCond::Sge, read, n);
                    (b.int_bin(IntBinOp::And, set, met), true)
                }
            };
            let (l_more, t) = (b.create_block(&[]), b.bconst(true));
            match on {
                true => b.cond_br(v, (l_stop, &[t]), (l_more, &[])),
                false => b.cond_br(v, (l_more, &[]), (l_stop, &[t])),
            }
            b.switch_to(l_more);
        }
        let f = b.bconst(false);
        b.br(l_stop, &[f]);
        b.switch_to(l_stop);
        self.stop.set(Some(b.block_params(l_stop)[0]));
        env.stopped = self.stop.get();
    }

    fn outer(&self) -> bool {
        true
    }

    /// D2877: the checks of the nodes above, which PG makes at their first call (over an empty build, PG still reaches a
    /// gate above the join, whose check only this step makes then: D2148).
    fn first_call(&self) -> bool {
        true
    }

    fn window(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env, x: &Exits) {
        let Some(stop) = self.stop.get() else { return };
        let l_go = b.create_block(&[]);
        b.cond_br(stop, (x.l_done, &[]), (l_go, &[]));
        b.switch_to(l_go);
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        next(b, env, row)
    }

    fn keeps_rows(&self) -> bool {
        true
    }

    /// D1753 / D1752: a count read from the union's words is the leader's (the arms' copies count there)
    fn serial(&self) -> bool {
        self.checks.iter().any(|c| matches!(c, Check::Full { .. }))
    }

    /// D2152: a stopped feeder closes the call (no source open, no window: PG never pulls below the stopped node).
    fn closes(&self, _b: &mut FunctionBuilder<'_>, _env: &Env) -> Option<Value> {
        self.stop.get()
    }
}
