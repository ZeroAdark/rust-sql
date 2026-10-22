//! The drain source (kind `join.drain`'s straight form, M7-plan §3.6; M7-DRAIN DRAIN-1, V4R-378): after the probe, one
//! serial walk over the build's records `[0, n + kept)` in record order. RIGHT / FULL / RIGHT ANTI hand on the records no
//! probe row marked (the NULL-key records past `n` included, L0-d), RIGHT SEMI the marked ones (M7-plan §3.4: its matches
//! are emitted here). Each goes on with the outer side NULL (every OUTER_VAR column the ops up to the join's projection
//! read, typed from its Var) and the record's payload as the INNER_VAR columns (probe_inner's `joined`). A window is one
//! run of wanted records: the unwanted ones before it are skipped, at most `span()` per window (an empty window moves the
//! cursor on), so both loops are bounded. The cursor (the record index) is the resume word (source/result.rs). The drain
//! runs after its probe pipeline, which ran the same ops into the same end (join/drain.rs: the sink, or the fold whose
//! arms continue one slot): it never reads the probe's spool `m`, so its descriptor drops that slot. D3024 (b): a drain
//! into a fold or build whose partials merge as any producer's runs wide (`wide_site`): its claims are fixed slices of the
//! build's record index (`ClaimKind::BuildRecords`), each claim walking its own records and spilled partitions. A
//! SourceMaker row under join.inner's kind (the module rule): join/drain.rs (M7) delegates to it. Owner M7-DRAIN (M1
//! scope). Budget 270 lines.
use core::cell::Cell as VCell;
use std::rc::Rc;

use gravity_abi::breaker::{BreakerKind, OWNER_RUN_SLICES, TABLE_KIND_OFF, TABLE_V4BUILD};
use gravity_abi::ctx::hdr;
use gravity_decompose::graph::{PipelineNode, Source};
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::Op;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_program::{ClaimKind, LifeId, NumTypmod, Parallel, Pipeline, Reason as Why, Rep, Until};
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};
use types_nodes::jointype::JoinType;
use types_nodes::primnodes::OUTER_VAR;
use types_nodes::{Node, NodeTag};

use crate::cell::{At, Cell, CellHome, CellLife, Nullable, Row};
use crate::face::kinds::{Face, KindId};
use crate::face::source::{SourceMech, WinConds};
use crate::frame::env::Env;
use crate::frame::frame::{SourceMaker, Win};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::admit::refusal::{Reason, ShapeWhy};

use crate::ops::hashjoin::build::{region, FAILED, FLAGS, FORM, F_SPLIT, KEPT, MARKS, N, PHASE, REC};
use crate::ops::hashjoin::build_side::Shape;
use crate::ops::hashjoin::drain::part::Spill;

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::JoinInner, &SourceMaker(make))], ..crate::ops::NONE };

/// The records one window looks at: the unwanted ones skipped before its run, and the run's length. A batch size with no
/// decline (the window refills), sized from the machine profile: one record per 512 bytes of the core's L2 share
/// (`partition_bytes`), 4096 on the reference machine (2 MiB, Graviton), where the program is the one the fixed span gave.
pub(crate) fn span() -> i64 {
    span_of(gravity_driver::machine::profile())
}

fn span_of(m: &gravity_driver::machine::MachineProfile) -> i64 {
    (m.partition_bytes >> 9) as i64
}

struct Drain<'g> {
    sh: Rc<Shape>,
    life: LifeId,
    consumed: usize,
    /// RIGHT SEMI: the marked records; the rest: the unmarked ones.
    marked: bool,
    /// `m`, the probe's spool, which the drain never reads (join/drain.rs).
    drop: Option<u8>,
    /// The OUTER_VAR columns read before the join's projection (attno, type), each a NULL cell.
    outer: PgVec<'g, (i16, TypeFacts)>,
    /// The window's region (`Ptr`), its records' address (`I64`) and its marks (`Ptr`).
    hv: VCell<Option<(Value, Value, Value)>>,
    /// B4c-2: phase 2 (the build may spill: a spill pair follows the probe), join/drain_part.rs.
    spill: Option<Spill>,
    /// D2625: a RIGHT SEMI / RIGHT ANTI drain over a table kept across a rerun set's calls (its build in no set): the
    /// marks are cleared at the nest inner's re-open (`reopen`), before the set's probe runs.
    reset: bool,
    /// The last run of wanted records `next_window` found, `[start, end)` (frame words, byte offsets; both 0 at an open):
    /// a reader that takes one record per window (the spooled form's fill, a stop) resumes inside it without scanning its
    /// marks again (held-out mj-0147 / mj-0149: a Hash Full Join's drain rescanned up to `span()` marks per record).
    run: [i32; 2],
    /// D3024 (b): the build's slot and the nominal driving site a wide drain claims under (its probe's: `wide_site`).
    slot: u8,
    site: Option<usize>,
    /// Whether the drain runs wide, decided once at its first `pipeline_desc` (`wide`), so the window walk reads its claim
    /// exactly when the runner cuts slices for it.
    wide: VCell<Option<bool>>,
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Source::Breaker { slot } = p.source else { crate::refuse!(node, Reason::Bug { payload: "a drain source over another source" }) };
    let Some(Op::Join(JoinDesc::Drain { matches, jointype })) = p.ops.first() else {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy("a build's slot read by a pipeline that does not open with a drain (join.emit_build)") })
    };
    let marked = match jointype {
        JoinType::JOIN_RIGHT | JoinType::JOIN_FULL | JoinType::JOIN_RIGHT_ANTI => false,
        JoinType::JOIN_RIGHT_SEMI => true,
        _ => crate::refuse!(node, Reason::Bug { payload: "a drain of a join type that sends no build row" }),
    };
    let Some(life) = gx.life_of(k, slot) else { crate::refuse!(node, Reason::Bug { payload: "a drain without its build" }) };
    let drop = Some(*matches);
    // D2632: a nest's embedded inner (join#212 / join#213) runs in its owner's call over the owner's regions, which list the
    // owner's own slots (a sorted outer's run) beside its inners' (pipes.rs `consumed_at`): it declares no list of its own
    // under the owner's ident (that list's indices read the run as the build) and reads the build at the owner's index
    let inner = k >= gx.pipelines().len();
    if !inner {
        px.set_consumes(p.desc.consumes.iter().copied().filter(|&s| Some(s) != drop).collect()); // alloc: Program
    }
    let consumed = if inner { gx.pipelines().consumed_at(k, slot) } else { px.consumed(k, slot) };
    let Some(consumed) = consumed else { crate::refuse!(node, Reason::Bug { payload: "a drain of a slot its pipeline does not consume" }) };
    let Some(vars) = outer_vars(p, px.mcx()) else { crate::refuse!(node, Reason::Shape { why: ShapeWhy("a drain whose outer columns the walker cannot bound (no projection, a whole-row or system column)") }) };
    let outer = vars.into_iter().map(|(a, t)| (a, crate::step::expr::tf_of(gx, t))).collect_in(px.mcx());
    let sh = crate::ops::hashjoin::build_side::shape(px, slot, life.0 as usize);
    // B4c-2: phase 2 when a spill pair follows the probe that spools `m` (join/spill_pair.rs `pair`: the build may spill).
    // D2173: the probe is the life of `m` this drain reads, not the first pipeline ending in `m`'s number: a slot number is
    // free again once its life ends, so an earlier producer (a lower join's build) may hold it, and phase 2 never ran.
    let probe = gx.life_of(k, *matches).map(|l| l.0 as usize);
    let spill = probe.filter(|&q| px.pass_after.contains(&q)).map(|_| crate::ops::hashjoin::drain::part::words(px, k, consumed));
    let reset = matches!(jointype, JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT_ANTI) && !gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(life.0 as u16)));
    let site = if inner { None } else { wide_site(px, k, *jointype, probe) };
    // words 0-2 are phase 2's (part.rs `words`)
    let mut w = |i: u8| px.frame_word(WordKey::Open { face: Face::Join, k, j: u64::MAX, i }) as i32 * 8;
    let run = [w(3), w(4)];
    mcx::pg_box_dyn!(px.mcx(), Drain { sh, life, consumed, marked, drop, outer, hv: VCell::new(None), spill, reset, run, slot, site, wide: VCell::new(None) } => dyn SourceMech + 'g)
}

/// D3024 (b): the nominal driving site of drain pipeline `k` when it may run wide over slices of its build's records: its
/// probe `q`'s site, when its end merges each worker's partial as any producer's (a hash aggregate or distinct with an
/// owner, the seeded run's last arm; a hash build, B4d; a plain fold) and nothing picks rows by their order or runs the
/// drain again: no LIMIT (D281) or Append arm (D1752) among its ops, neither it nor its probe a rerun set's member (D2594 /
/// D2625), no recheck, write or lock in the statement (EvalPlanQual reruns producers), and no RIGHT SEMI drain under a
/// partial reader (D1922). Every other order-visible form is spooled or fused, never this source's.
fn wide_site(px: &Px<'_, '_>, k: usize, jt: JoinType, probe: Option<usize>) -> Option<usize> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let q = probe?;
    let ends = crate::frame::iface::end_breaker_of(gx, k).is_some_and(|b| match b.kind {
        BreakerKind::HashAgg | BreakerKind::Distinct => p.desc.owner.is_some(),
        BreakerKind::HashBuild | BreakerKind::PlainAgg => true,
        _ => false,
    });
    let rerun = |x: usize| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(x as u16)));
    let acts = gx.graph.recheck || gx.pipelines().iter().any(|x| x.ops.iter().any(|op| matches!(op, Op::Write(_) | Op::Lock(_))));
    let arm = p.ops.iter().any(|op| matches!(op, Op::Append(AppendDesc::Arm { .. })));
    let semi = matches!(jt, JoinType::JOIN_RIGHT_SEMI) && gx.graph.partial;
    let ok = ends && !crate::frame::iface::holds_limit(&p.ops) && !arm && !rerun(k) && !rerun(q) && !acts && !semi;
    if ok { px.pipeline_site(q) } else { None }
}

/// D3024 (b): the bytes of the records a drain walks in laid-out build region `region` (its keyed records and the
/// NULL-key tail: `(N + KEPT) x REC`), which the runner weighs against the owner rule's leader bytes before it opens a
/// section for the drain (exec runner/plan.rs); `None` for a region that holds no laid-out build, or a held build error.
pub fn drain_bytes(region: &[u8]) -> Option<u64> {
    let byte = |at: usize| region.get(at).copied();
    let word = |at: usize| region.get(at..at + 8).and_then(|b| b.try_into().ok()).map(u64::from_ne_bytes);
    if byte(TABLE_KIND_OFF)? != TABLE_V4BUILD || byte(PHASE)? != 2 || byte(FORM)? == FAILED {
        return None;
    }
    let rec = region.get(REC..REC + 4).and_then(|b| b.try_into().ok()).map(u32::from_ne_bytes)?;
    let kept = if byte(FLAGS)? & F_SPLIT != 0 { word(KEPT)? } else { 0 };
    Some(word(N)?.saturating_add(kept).saturating_mul(rec as u64))
}

const _: () = assert!(OWNER_RUN_SLICES.is_power_of_two() && OWNER_RUN_SLICES <= 1 << 8);

/// D3024 (b): this call's claim as slices `[lo, hi)` of `OWNER_RUN_SLICES`, each clamped to the count: a run of every
/// slice (the whole-range claim `[0, u64::MAX)`) reads `[0, S)`, a section end's `[u64::MAX, u64::MAX)` reads `[S, S)`.
fn claim_slices(b: &mut FunctionBuilder<'_>, env: &Env) -> (Value, Value) {
    let s = b.iconst(Type::I64, OWNER_RUN_SLICES as i64);
    let lo = b.load(Type::I64, env.ctx, hdr::CLAIM_LO as i32);
    let hi = b.load(Type::I64, env.ctx, hdr::CLAIM_HI as i32);
    (b.int_bin(IntBinOp::Umin, lo, s), b.int_bin(IntBinOp::Umin, hi, s))
}

/// D3024 (b): the first record of slice `x` (of S) over a build of `total` records, `floor(total * x / S)` (abi
/// `Owned::slice_runs`' cut): the slices of any set of claims that cover `[0, S)` once tile `[0, total)` once.
fn records_at(b: &mut FunctionBuilder<'_>, total: Value, x: Value) -> Value {
    let t = b.int_bin(IntBinOp::Mul, total, x);
    let sh = b.iconst(Type::I64, OWNER_RUN_SLICES.trailing_zeros() as i64);
    b.int_bin(IntBinOp::Ushr, t, sh)
}

/// The OUTER_VAR columns (attno, type) the ops after the Drain read up to the join's projection; `None` when there is no
/// such projection, a node the walker cannot see through, or a whole-row / system column.
pub(crate) fn outer_vars<'m>(p: &PipelineNode<'_>, mcx: Mcx<'m>) -> Option<PgVec<'m, (i16, u32)>> {
    fn uses(n: Node<'_>, out: &mut PgVec<'_, (i16, u32)>) -> bool {
        crate::stack_check();
        // an identity column's nextval: its sequence only, no column (spool.rs `spool_columns` the same)
        if n.node_tag() == NodeTag::T_NextValueExpr {
            return true;
        }
        if let Some(v) = n.as_var() {
            if v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattno > 0 && !out.iter().any(|o| o.0 == v.varattno) {
                out.push_in((v.varattno, v.vartype));
            }
            return v.varno != OUTER_VAR || v.varattno > 0;
        }
        let mut ok = true;
        gravity_decompose::expr::kids(n, &mut |k| ok &= uses(k, out)) && ok
    }
    let mut out = PgVec::new_in(mcx);
    let mut ok = true;
    for op in p.ops[1..].iter() {
        match op {
            Op::Filter(n) | Op::Gate(n) => ok &= uses(*n, &mut out),
            // A1b: a SubPlan op before the projection only in a spooled drain (nest/spool_drain.rs spool_columns)
            Op::SubPlan(s) => ok &= s.args.iter().fold(true, |ok, n| uses(n, &mut out) && ok) && s.test.is_none_or(|t| uses(t, &mut out)),
            Op::Project(l) => return l.iter().fold(ok, |ok, n| uses(n, &mut out) && ok).then_some(out),
            _ if crate::frame::iface::limit_bounds(op).is_some() => {}
            _ => return None,
        }
    }
    None
}

impl<'g> Drain<'g> {
    /// Whether record `i` is one this drain sends: its mark byte against `marked`.
    fn wanted(&self, b: &mut FunctionBuilder<'_>, env: &Env, marks: Value, i: Value) -> Value {
        // SAFETY (the marks' visibility): the probe's workers store mark bytes with plain stores (build.rs `mark`: each
        // stores the same byte 1, so racing stores agree) into the marks array of this region, laid out and zeroed before
        // the probe opened. The drain opens only after the probe's section ended (`ProtoKind::Follows`, join/drain.rs:
        // the producer order every scheduler reads): the section's join (the workers' completion, the leader's wait on
        // it) orders every worker's stores before this load. The marks are read-only from then on: the drain only loads
        // them, on the leader or (D3024 (b), wide) on the claim threads of a section the leader starts after that join.
        let at = b.ptr_add(marks, i, 1, 0);
        let m = b.load_zext(Type::I8, at, 0);
        b.icmp(if self.marked { IntCond::Ne } else { IntCond::Eq }, m, env.zero)
    }

    /// A loop from `from` while `i < lim` and `wanted(i) == go`: the first index past it (bounded by `lim`).
    fn scan(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, marks: Value, from: Value, lim: Value, go: bool) -> Value {
        let (l_head, l_test, l_out) = (b.create_block(&[Type::I64]), b.create_block(&[]), b.create_block(&[Type::I64]));
        env.bounded.push_in(l_head);
        b.br(l_head, &[from]);
        b.switch_to(l_head);
        let i = b.block_params(l_head)[0];
        let past = b.icmp(IntCond::Sge, i, lim);
        b.cond_br(past, (l_out, &[i]), (l_test, &[]));
        b.switch_to(l_test);
        let w = self.wanted(b, env, marks, i);
        let i1 = b.int_bin(IntBinOp::Add, i, env.one);
        match go {
            true => b.cond_br(w, (l_head, &[i1]), (l_out, &[i])),
            false => b.cond_br(w, (l_out, &[i]), (l_head, &[i1])),
        }
        b.switch_to(l_out);
        b.block_params(l_out)[0]
    }
}

pub(crate) fn min(b: &mut FunctionBuilder<'_>, x: Value, y: Value) -> Value {
    let lt = b.icmp(IntCond::Slt, x, y);
    b.select(lt, x, y)
}

impl<'g> Drain<'g> {
    /// D3024 (b): whether the drain runs wide: a `wide_site` drain that the decomposer kept serial for no other reason
    /// than its missing driving site (a breaker emit's descriptor) and whose calls a pool thread may run
    /// (`kept_on_leader`, read once the pipeline's expressions are made), decided at the first `pipeline_desc` and kept.
    /// Road `drain.parts` (H25's partition-parallel drain) switched off (`gravity_road_off`) keeps every drain serial.
    fn wide(&self, px: &Px<'_, '_>, k: usize) -> bool {
        if let Some(w) = self.wide.get() {
            return w;
        }
        let sited = matches!(px.gx.pipelines()[k].desc.parallel, Parallel::Eligible | Parallel::Ineligible { reason: Why::NoDrivingSite, .. });
        let off = crate::face::strategy::off().has(crate::face::strategy::DRAIN_PARTS);
        let w = self.site.is_some() && sited && !off && !crate::table::emit_group::kept_on_leader(px, k);
        if w {
            fires::fire("join.drain.wide"); // shapecheck fire signal (D3024 (b)): a hash join build drain claims slices of its records
        }
        self.wide.set(Some(w));
        w
    }
}

impl<'g> SourceMech for Drain<'g> {
    /// The drain runs on the leader after its probe (the build's records are one region); `m` dropped when fused. D3024
    /// (b): a wide drain is eligible, its claims slices of the build's records, its driving site its probe's, nominal.
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline {
        let d = px.gx.pipelines()[k].desc;
        let wide = self.wide(px, k);
        let parallel = match d.parallel {
            _ if wide => Parallel::Eligible,
            Parallel::Eligible => Parallel::Ineligible { site: d.driving_site, reason: Why::NoDrivingSite },
            p => p,
        };
        let mut n = Pipeline { consumes: &[], consumes_from: &[], parallel, ..d };
        if let (true, Some(site)) = (wide, self.site) {
            n = Pipeline { driving_site: site as i16, claims: ClaimKind::BuildRecords { slot: self.slot }, ..n };
        }
        for (i, &s) in d.consumes.iter().enumerate().filter(|&(_, &s)| Some(s) != self.drop) {
            n = match d.consumes_from.get(i) {
                Some(&l) => n.consuming_from_in(px.arena, s, l),
                None => n.consuming_in(px.arena, s),
            };
        }
        n
    }

    /// D3024 (b): a wide drain's nominal driving site (exec reads no row through it).
    fn nominal_site(&self, _px: &Px<'_, '_>) -> Option<usize> {
        self.site.filter(|_| self.wide.get() == Some(true))
    }

    /// B4c-2: phase 2's words, zero (the resident phase, no region) at a fresh open.
    fn open_words(&self, w: &mut OpenWords) {
        if let Some(sp) = self.spill {
            crate::ops::hashjoin::drain::part::open_words(sp, w);
        }
        w.declare(OpenWord::open("drain run start", Base::Words, self.run[0], Type::I64, 0));
        w.declare(OpenWord::open("drain run end", Base::Words, self.run[1], Type::I64, 0));
    }

    /// D2625: PG's ExecReScanHashJoin resets a kept table's match flags at every rescan, whatever the outer then does; the
    /// probe's first window (probe_calls.rs `Calls::at_window`) never comes when a false one-time gate or a zero LIMIT closes
    /// its pipeline, so the nest inner's re-open clears them first. A slot not yet built (null, or EMPTY) has none.
    fn reopen(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        if !self.reset {
            return;
        }
        let r = region(b, env, Some(self.consumed));
        let a = b.conv(ConvOp::PtrToInt, Type::I64, r);
        let set = b.icmp(IntCond::Ne, a, env.zero);
        let (l_clear, l_done) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(set, (l_clear, &[]), (l_done, &[]));
        b.switch_to(l_clear);
        crate::lifetime::hashjoin::reset_at_reopen(b, env, r);
        b.br(l_done, &[]);
        b.switch_to(l_done);
    }

    fn open(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        env.zero
    }

    /// The next run of wanted records at or past the cursor, skipping at most `span()` unwanted ones first; past the last
    /// record, the end. B4c-2: past the resident records of a build that spilled, each spilled partition in turn (its
    /// drain part, else its build part: `drain_part`), every record wanted; a new partition opens with an empty window at
    /// 0, and the frame's window loop comes back here at its first record (join/spill_pair.rs's pattern).
    /// D3024 (b): a wide drain walks only its claim's records and spilled partitions (`claim_slices`).
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        // a window made before the descriptor chose (a drain lowered ahead of its pipeline's descriptor) walks every record,
        // so it keeps the drain serial: a later descriptor then cuts no claims this walk would read past
        if self.wide.get().is_none() {
            self.wide.set(Some(false));
        }
        let r = region(b, env, Some(self.consumed));
        let claim = (self.wide.get() == Some(true)).then(|| claim_slices(b, env));
        let l_win = b.create_block(&[Type::I64, Type::I64, Type::Ptr, Type::I64, Type::Ptr]);
        let l_res = b.create_block(&[]);
        let l_next = self.spill.map(|_| b.create_block(&[]));
        match (self.spill, l_next) {
            (Some(sp), Some(l_next)) => crate::ops::hashjoin::drain::part::entry(b, env, sp, self.sh.base(), l_res, l_eof, l_next, l_win),
            _ => b.br(l_res, &[]),
        }
        b.switch_to(l_res);
        // op 14: a held build error (FORM FAILED; its header past N unset). A fill-inner join (RIGHT / FULL / RIGHT ANTI)
        // builds before its outer in PG (HJ_FILL_INNER), so the error raises here whatever the outer held; RIGHT SEMI's
        // build was PG's outer-first one, never built when its outer was empty (a row would have raised at the probe)
        let form = b.load_zext(Type::I8, r, FORM as i32);
        match self.marked {
            true => {
                let ff = b.iconst(Type::I64, FAILED as i64);
                let failed = b.icmp(IntCond::Eq, form, ff);
                let l_built = b.create_block(&[]);
                b.cond_br(failed, (l_eof, &[env.zero]), (l_built, &[]));
                b.switch_to(l_built);
            }
            false => crate::ops::hashjoin::probe_wait::raise_failed(b, env, form),
        }
        let n = b.load(Type::I64, r, N as i32);
        let flags = b.load_zext(Type::I8, r, FLAGS as i32);
        let fs = b.iconst(Type::I64, F_SPLIT as i64);
        let split = b.int_bin(IntBinOp::And, flags, fs);
        let has = b.icmp(IntCond::Ne, split, env.zero);
        let kept = b.load(Type::I64, r, KEPT as i32);
        let kept = b.select(has, kept, env.zero);
        let total = b.int_bin(IntBinOp::Add, n, kept);
        // D3024 (b): a claim's records `[T*lo/S, T*hi/S)`, its cursor never below the first (a fresh call's position is 0)
        let (pos, total) = match claim {
            Some((lo, hi)) => {
                let (from, to) = (records_at(b, total, lo), records_at(b, total, hi));
                let at = crate::frame::resume::resume_pos(b, env);
                (b.int_bin(IntBinOp::Smax, at, from), to)
            }
            None => (crate::frame::resume::resume_pos(b, env), total),
        };
        let done = b.icmp(IntCond::Sge, pos, total);
        let l_go = b.create_block(&[]);
        match l_next {
            None => b.cond_br(done, (l_eof, &[env.zero]), (l_go, &[])),
            Some(l_next) => b.cond_br(done, (l_next, &[]), (l_go, &[])),
        }
        b.switch_to(l_go);
        let off = b.load(Type::I64, r, MARKS as i32);
        let marks = b.ptr_add(r, off, 1, 0);
        let ri = b.conv(ConvOp::PtrToInt, Type::I64, r);
        let hdr = b.iconst(Type::I64, self.sh.base() as i64);
        let recs = b.int_bin(IntBinOp::Add, ri, hdr);
        // inside the last run found (`run`: the probe ended before the drain opened, so no mark moves under it), the window
        // is the rest of that run; else the unwanted records skipped and the next run scanned, each bounded by span()
        let (r0, r1) = (env.load_word(b, self.run[0]), env.load_word(b, self.run[1]));
        let (l_end, l_keep, l_scan) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        let from = b.icmp(IntCond::Sge, pos, r0);
        b.cond_br(from, (l_end, &[]), (l_scan, &[]));
        b.switch_to(l_end);
        let before = b.icmp(IntCond::Slt, pos, r1);
        b.cond_br(before, (l_keep, &[]), (l_scan, &[]));
        b.switch_to(l_keep);
        b.br(l_win, &[pos, r1, r, recs, marks]);
        b.switch_to(l_scan);
        let span = b.iconst(Type::I64, span());
        let far = b.int_bin(IntBinOp::Add, pos, span);
        let lim = min(b, far, total);
        let s = self.scan(b, env, marks, pos, lim, false);
        let far = b.int_bin(IntBinOp::Add, s, span);
        let lim = min(b, far, total);
        let e = self.scan(b, env, marks, s, lim, true);
        env.store_word(b, self.run[0], s);
        env.store_word(b, self.run[1], e);
        b.br(l_win, &[s, e, r, recs, marks]);
        if let (Some(sp), Some(l_next)) = (self.spill, l_next) {
            crate::ops::hashjoin::drain::part::next(b, env, sp, self.sh.base(), r, self.marked, claim, l_next, l_eof, l_win);
        }
        b.switch_to(l_win);
        let v = b.block_params(l_win);
        self.hv.set(Some((v[2], v[3], v[4])));
        Win { cursor: v[0], end: v[1], facts: WinConds::default() }
    }

    /// Record `j`'s payload as the INNER_VAR columns (`env.inner`), every outer column read a NULL cell.
    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let Some((r, recs, _)) = self.hv.get() else { crate::refuse!(env.node, Reason::Bug { payload: "a drain row before its window" }) };
        let size = b.iconst(Type::I64, self.sh.rec.get() as i64);
        let off = b.int_bin(IntBinOp::Mul, j, size);
        let at = b.int_bin(IntBinOp::Add, recs, off);
        let p = b.conv(ConvOp::IntToPtr, Type::Ptr, at);
        let link = b.load(Type::I64, p, self.sh.link_off());
        let base = b.conv(ConvOp::PtrToInt, Type::I64, r);
        let mcx = env.mcx;
        let (mut row, mut vals) = (Row::new_in(mcx), mcx::PgVec::new_in(mcx));
        for (i, c) in self.sh.kept.borrow().iter().enumerate() {
            let mut w = b.load(Type::I64, p, self.sh.pay_off(i));
            if self.sh.is_rel(i) {
                w = b.int_bin(IntBinOp::Add, w, base);
            }
            // join.drain.fp: a code whose Fp128 the record carries goes on linked to its words, as the probe's joined row
            // does (walk.rs `joined`): the drain pipeline's ops are the probe's tail too, so both paths see one cell form
            if let Some(f) = self.sh.fp_at(i) {
                let fp = [b.load(Type::I64, p, self.sh.fp_off(f)), b.load(Type::I64, p, self.sh.fp_off(f) + 8)];
                env.fps.add(w, fp);
                fires::fire("join.drain.fp"); // shapecheck fire signal (join.drain.fp): a drained record's code keys by its stored Fp128 words
            }
            let (off, at) = self.sh.null_at(i);
            let word = if off == self.sh.link_off() { link } else { b.load(Type::I64, p, off) };
            let s = b.iconst(Type::I64, at as i64);
            let bits = b.int_bin(IntBinOp::Ushr, word, s);
            let bit = b.int_bin(IntBinOp::And, bits, env.one);
            let null = b.icmp(IntCond::Ne, bit, env.zero);
            row.cells.push_in(Cell::of_record(c.rep, c.typid, c.num, crate::ops::hashjoin::build_side::home_of(c, self.life), i as u16, Nullable::Maybe));
            vals.push_in((w, Some(null)));
        }
        let width = self.sh.payload.iter().copied().max().unwrap_or(0).max(0) as usize;
        let mut inner = mcx::pg_vec![in mcx; None; width];
        for (i, &a) in self.sh.payload.iter().enumerate() {
            inner[a as usize - 1] = Some(i as u16);
        }
        let np = row.cells.len();
        let life = CellLife { until: Until::Life, home: CellHome::Record(self.life) };
        let mut cols = mcx::pg_vec![in mcx; None; self.outer.iter().map(|o| o.0 as usize).max().unwrap_or(0)];
        let null = b.bconst(true);
        for (i, (a, tf)) in self.outer.iter().enumerate() {
            let rep = Rep::of_storage(tf.typlen, tf.byval);
            row.cells.push_in(Cell { rep, typid: tf.typid, num: NumTypmod::NONE, life, at: At::Slot((np + i) as u16), null: Nullable::Maybe });
            vals.push_in((env.zero, Some(null)));
            cols[*a as usize - 1] = Some((np + i) as u16);
        }
        env.inner = Some(inner);
        RowVals { row, vals, cols, carry, next, at: j }
    }

    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, _out: Value) {
        crate::frame::resume::write_pos(b, env, cur);
    }
}


#[cfg(test)]
mod tests {
    use gravity_driver::machine::MachineProfile;

    #[test]
    fn the_window_span_follows_the_l2_share() {
        assert_eq!(super::span_of(&MachineProfile::reference()), 4096);
        assert_eq!(super::span_of(&MachineProfile { partition_bytes: 256 << 10, ..MachineProfile::reference() }), 512);
        assert_eq!(super::span_of(&MachineProfile { partition_bytes: 64 << 10, ..MachineProfile::reference() }), 128);
    }
}
