//! The pair pipeline of a spilled join.inner build (M7-SPILL SPILL-1, V4R-359; LEFT, SEMI, ANTI B4a; M7-plan §3.10). The probe `q` keeps each
//! row whose key's partition the build spilled (`Kept::exit`, after the probe image's width check): the row's cells go
//! to its owner's file of the build slot's stream at part `2^bits + p`, and the row goes no further. The pair pipeline
//! `P'` is added right after `q` (`Px::add_pass`) and runs `q`'s ops from the probe on into the same sink. Its claims are
//! the stream's partitions (`ClaimKind::SpillPartitions`); a serial run's one claim is every partition. Its source
//! (`KeptSource`) walks the claim's partitions in order and opens each `p` as follows: it reads the kept rows of `p`, then
//! partition `p`'s build records from every owner's file into a region of their own, and lays them out (build_rt.rs). It
//! points the probe's consumed build at that region and hands the kept rows on as the rows the probe met. A partition
//! with no kept row hands on nothing. One with no build record hands on nothing for INNER and SEMI (no match), and every
//! kept row for LEFT and ANTI (B4a: each misses over an empty HASH region, so the probe never meets an EMPTY build). A partition's region grows by name when it is past the grant (the D225 reserve,
//! `v4::take`). One level: B3; D2715: a partition past its region's length (a u32) joins in sub-passes, its
//! records and kept rows split by the KeyHash's next bits (spill/load.rs `split_bits`). Owner M7-SPILL (M1 scope). Budget 470 lines.
use core::cell::{Cell as VCell, RefCell};
use std::rc::Rc;

use gravity_expr::emit::hash::emit_key_hash;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::{ClaimKind, Parallel, Pipeline, Rep, Until};
use mcx::{CollectIn, GrowIn, Mcx, PgRc, PgVec};

use crate::cell::{At, Cell, CellHome, CellLife, Nullable, Row};
use crate::table::keyimg::ImageWords;
use crate::face::source::{SourceMech, WinConds};
use crate::frame::env::Env;
use crate::frame::frame::{EndMaker, StepMaker, Win};
use crate::frame::px::{ExtraPass, Px};
use crate::frame::row::{Carry, RowVals};
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::frame::iface::{breaker_of, kind_impl};
use crate::frame::resume::{resume_pos, write_pos};

use crate::ops::hashjoin::build::{region, D_CSTR, D_VARLENA};
use crate::ops::hashjoin::spill::build::{emit_route, raise_neg, SPILL, SPILLED};
use crate::ops::hashjoin::spill::load::{kept_put, pair_next, pair_open};

/// The most slots a pair pipeline consumes (its copy of the `more` table in the pair region's header).
pub(crate) const MORE: usize = 64;
/// The pair region's header before the partition's build region: `MAGIC`, then the original build region, the kept rows,
/// their offsets, their count, the original `more` table, the partition and the claim's end; D2715's split words (the
/// sub-partition, its bits, the grouped records and their bounds, the grouped kept rows' offsets and their bounds, a drained
/// build's drain records so far: buffer, capacity, bytes); then the copy of the table.
pub(crate) const META: usize = M_TABLE + 8 * MORE;
/// The hosts' `bits` argument's fill flag (`Kept::arg`).
pub(crate) const FILL: u64 = 1 << 8;
/// The hosts' `bits` argument's drain flags (B4c): a RIGHT / FULL / RIGHT ANTI / RIGHT SEMI build's partitions write their
/// drain parts at close (spill_load.rs `write_drain`), of the marked records (`DRAIN_MARKED`, RIGHT SEMI) or the rest.
pub(crate) const DRAIN: u64 = 1 << 9;
pub(crate) const DRAIN_MARKED: u64 = 1 << 10;
pub(crate) const MAGIC: u64 = 0x4d37_5350_4149_5231; // "M7SPAIR1"
pub(crate) const M_ORIG: usize = 8;
pub(crate) const M_KEPT: usize = 16;
pub(crate) const M_OFFS: usize = 24;
pub(crate) const M_NKEPT: usize = 32;
pub(crate) const M_MORE: usize = 40;
pub(crate) const M_PART: usize = 48;
pub(crate) const M_HI: usize = 56;
pub(crate) const M_SUB: usize = 64;
pub(crate) const M_SBITS: usize = 72;
pub(crate) const M_GRP: usize = 80;
pub(crate) const M_BND: usize = 88;
pub(crate) const M_KOFF: usize = 96;
pub(crate) const M_KBND: usize = 104;
pub(crate) const M_DP: usize = 112;
pub(crate) const M_DCAP: usize = 120;
pub(crate) const M_DLEN: usize = 128;
/// Cache lines apart, so the build region after the header keeps the alignment `v4::take` gives.
pub(crate) const M_TABLE: usize = 192;
/// The kept rows one window hands on: a batch size with no decline (the window refills), sized from the machine profile:
/// one kept row per 2 KiB of the core's L2 share (`partition_bytes`; a kept row is a reloaded spilled record, wider than a
/// resident one), 1024 on the reference machine (2 MiB, Graviton), where the program is the one the fixed span gave.
fn span() -> i64 {
    span_of(gravity_driver::machine::profile())
}

fn span_of(m: &gravity_driver::machine::MachineProfile) -> i64 {
    (m.partition_bytes >> 11) as i64
}
/// A reload buffer's first bytes (it doubles).
pub(crate) const FIRST: usize = 64 << 10;

/// A spill-eligible probe's kept rows: the build's slot, the probe's consumed index of it, the partition bits, and the
/// row layout `q`'s lowering records at the probe (`exit`) for `P'`'s source (lowered after it: program order).
pub(crate) struct Kept<'g> {
    mcx: Mcx<'g>,
    slot: u8,
    consumed: usize,
    bits: u8,
    ncons: usize,
    node: i32,
    /// LEFT / ANTI (a fill): a partition with no build record still hands its kept rows on, to miss (B4a).
    fill: bool,
    /// `drain_of` the probe's join type: its partitions' drain parts (B4c).
    drain: u64,
    /// `P'`'s virtual pipeline id (`Px::add_pass`): a drain follows it (M7-DRAIN, `Follows`).
    pass: usize,
    /// The kept row's layout, one per build: a union's arms (D2084) keep into one stream that one `P'` reads.
    layout: Rc<Rows<'g>>,
    /// D4054: `q` ends in a hash build that `P'` finishes (a pair run, build.rs `run_of`): a code or packed numeric cell is
    /// kept in its scan form (spill/forms.rs), so both arms hand the build one record form.
    scan_forms: bool,
}

/// The kept row layout of one build's probes (`JoinExt::pairs`), set by the first probe lowered.
pub(crate) type Rows<'g> = RefCell<Option<Layout<'g>>>;

/// The drain flags of a probe of join type `jt` (`Kept::arg`): a right-family build's drain sends what its marks leave.
pub(crate) fn drain_of(jt: types_nodes::jointype::JoinType) -> u64 {
    use types_nodes::jointype::JoinType;
    match jt {
        JoinType::JOIN_RIGHT | JoinType::JOIN_FULL | JoinType::JOIN_RIGHT_ANTI => DRAIN,
        JoinType::JOIN_RIGHT_SEMI => DRAIN | DRAIN_MARKED,
        _ => 0,
    }
}

/// The probe's row: each cell's reloaded form (a datum's storage rep, or D4054's scan form) and payload kind, its columns
/// and INNER_VAR map.
#[derive(Clone)]
pub(crate) struct Layout<'g> {
    cells: PgVec<'g, Cell>,
    kinds: PgVec<'g, u16>,
    cols: PgVec<'g, Option<u16>>,
    inner: Option<PgVec<'g, Option<u16>>>,
}

/// Probe `j` of pipeline `q` over build pipeline `build` (slot `slot`, `q`'s consumed index `consumed`): when the build may
/// spill (`build_bits`, the face's), `P'` is added after `q`, its steps `q`'s ops from `j` and its end `q`'s, made under
/// `P'`'s id. `None` inside a remade pipeline (`Px::ident`: `P'`'s own probe keeps nothing). D2084: a probe under a union's
/// arm (spill/build.rs `arm_run`) keeps its rows into the build's one stream; only the run's last arm adds `P'`, which reads every arm's
/// kept rows, and the union's `Arms` run grows by `P'`.
pub(crate) fn pair<'g>(px: &mut Px<'g, '_>, q: usize, j: usize, build: usize, slot: u8, consumed: usize, fill: bool, drain: u64) -> Option<PgRc<'g, Kept<'g>>> {
    if px.ident(q) != q {
        return None;
    }
    let bits = crate::ops::hashjoin::spill::build::build_bits(px, build, q, j)?;
    let gx = px.gx;
    let p = &gx.pipelines()[q];
    let node = p.node_id;
    let run = crate::ops::hashjoin::spill::build::arm_run(px.gx, q);
    let v = px.pass_id();
    let layout = rows_of(px, build);
    let ends = breaker_of(&p.desc).filter(|b| b.kind == gravity_abi::breaker::BreakerKind::HashBuild).map(|b| b.slot);
    let kept = PgRc::new_in(Kept { mcx: px.mcx(), slot, consumed, bits, ncons: p.desc.consumes.len(), node, fill, drain, pass: v, layout, scan_forms: ends.is_some() }, px.mcx());
    if run.is_some_and(|(f, n)| q != f + n - 1) {
        return Some(kept);
    }
    // D2717: the probe's copy reads the lazy build its original took (build_class.rs `lazy_or_copy`)
    use crate::ops::hashjoin::build_side::JoinExt;
    // D4054: a probe pipeline ending in a hash build makes a pair run of it: `P'`'s copy of the end, made next, finishes
    // the build; `q`'s own, made after this probe's step, continues it (build.rs `run_of`)
    if let Some(s) = ends {
        px.ext_mut::<JoinExt>(crate::face::kinds::Face::Join).pair_runs.push_in((q, v, s));
    }
    let outer = px.ext_mut::<JoinExt>(crate::face::kinds::Face::Join).pairing.replace((q, v));
    let mcx = px.mcx();
    let (steps, end, tabs) = px.making(q, v, |px| {
        // V4R-499: from the SubPlan steps right before the probe (a join qual's lend)
        let j0 = (0..j).rev().take_while(|&i| matches!(p.ops[i], gravity_decompose::Op::SubPlan(_))).last().unwrap_or(j);
        let steps = (j0..p.ops.len()).map(|i| (kind_impl::<StepMaker>(&node, gx.step_need(q, i)).0)(px, q, i)).collect_in(mcx);
        // D3876: the per-code tables those steps declared under `P'`'s id (a constant-arm key's image rows)
        (steps, (kind_impl::<EndMaker>(&node, gx.end_maker(q)).0)(px, q), crate::dict::percode::tables(px, q))
    });
    px.ext_mut::<JoinExt>(crate::face::kinds::Face::Join).pairing = outer;
    let source = mcx::pg_box_dyn!(mcx, KeptSource { kept: PgRc::clone(&kept), q, hv: VCell::new(None), tabs } => dyn SourceMech + '_);
    if run.is_none() {
        let j0 = (0..j).rev().take_while(|&i| matches!(p.ops[i], gravity_decompose::Op::SubPlan(_))).last().unwrap_or(j);
        crate::frame::instr::plan_pass(v, q, j0, j); // EXPLAIN ANALYZE: the kept rows' counts past the probe
    }
    if px.add_pass(q, ExtraPass { source, steps, end }) != v {
        crate::refuse!(node, Reason::Bug { payload: "a spill pair pass made under another pass's id" });
    }
    // B4b-1: a plain fold / full sort end (build_spill.rs `end_takes_pair`) takes `q` and `P'` as the two arms of its slot
    // (join/drain.rs's form): the driver keeps the slot open from `q` into `P'` and finishes it after `P'`
    // D2084: the arms of a union already continue the slot; their run takes `P'` as its last arm
    // D3512 / D3513: and every other run whose span holds `q` before its last pipeline (a Merge Append's or a union's
    // arms around this probe's arm) grows by `P'` too, which sits inside it in program order
    if let Some(b) = breaker_of(&p.desc) {
        let mut grown = core::mem::replace(&mut px.ext_mut::<JoinExt>(crate::face::kinds::Face::Join).grown, PgVec::new_in(mcx));
        let (ends, inside) = grows(&px.protos, &grown, q, b.slot, px.mcx());
        match ends {
            Some(i) => {
                px.protos[i].arg += 1;
                grown.push_in(i);
            }
            None => {
                let mut a = Proto::new(ProtoKind::Arms, q as u16);
                (a.arg, a.slot) = (2, b.slot);
                px.declare_proto(a);
                grown.push_in(px.protos.len() - 1);
            }
        }
        for i in inside {
            px.protos[i].arg += 1;
            grown.push_in(i);
        }
        px.ext_mut::<JoinExt>(crate::face::kinds::Face::Join).grown = grown;
    }
    Some(kept)
}

/// D3512 / D3513: the `Arms` runs (indices into `protos`) a pair pass `P'` spliced right after probe pipeline `q` (its end
/// on `slot`) grows: the run on `slot` whose last arm is `q` (`P'` its new last arm), and every run but a seeded drain's
/// whose span holds `q` before its last pipeline (`P'` then sits inside the span, whose last arm is one program index
/// later). A run's `arg` counts program pipelines: each `P'` it grew by already (`grown`, once per growth) moved its last
/// graph pipeline one before its end. A seeded drain's run is measured again from its drain (frame/tail.rs `seeded_arms`).
fn grows<'m>(protos: &[Proto], grown: &[usize], q: usize, slot: u8, mcx: Mcx<'m>) -> (Option<usize>, PgVec<'m, usize>) {
    let seeded = |a: &Proto| protos.iter().any(|f| f.kind == ProtoKind::Follows && f.peer == a.pipeline);
    let last = |i: usize, a: &Proto| run_last(grown, i, a);
    let arms = || protos.iter().enumerate().filter(|(_, a)| a.kind == ProtoKind::Arms);
    let ends = arms().find(|&(i, a)| a.slot == slot && last(i, a) == Some(q)).map(|(i, _)| i);
    let inside = arms().filter(|&(i, a)| !seeded(a) && a.pipeline as usize <= q && last(i, a).is_some_and(|l| q < l)).map(|(i, _)| i).collect_in(mcx);
    (ends, inside)
}

/// D3512 / D3513: the last graph pipeline of `Arms` run `a` (`protos[i]`): its span less each pass `P'` it grew by
/// (`grown`, once per growth), which sits inside it in program order; `None` for a run all passes.
pub(crate) fn run_last(grown: &[usize], i: usize, a: &Proto) -> Option<usize> {
    (a.pipeline as usize + a.arg as usize).checked_sub(1 + grown.iter().filter(|&&g| g == i).count())
}

/// Build `build`'s kept row layout, shared by its probes.
fn rows_of<'g>(px: &mut Px<'g, '_>, build: usize) -> Rc<Rows<'g>> {
    let e = px.ext_mut::<crate::ops::hashjoin::build_side::JoinExt>(crate::face::kinds::Face::Join);
    if let Some((_, r)) = e.pairs.iter().find(|r| r.0 == build) {
        return Rc::clone(r);
    }
    let r = Rc::new(RefCell::new(None)); // alloc: Program
    e.pairs.push_in((build, Rc::clone(&r)));
    r
}

fn host(b: &mut FunctionBuilder<'_>, name: &str, params: &[Type], f: usize) -> gravity_ir::HostFn {
    let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
    b.declare_host_once(name, params, Some(Type::I64), f, flags)
}

/// The payload kind a datum of `tf`'s storage takes in a kept row: 0 a word, else its image's (as build.rs's `DESC`).
fn kind_of(rep: Rep) -> u16 {
    match rep {
        Rep::VarlenaPtr => D_VARLENA,
        Rep::CstrPtr => D_CSTR,
        Rep::FixedPtr { len } => len,
        _ => 0,
    }
}

impl Kept<'_> {
    /// The stream's argument (the build's: the probe's consumed region `consumed`, which `P'` consumes at the same index)
    /// and the kept rows' first part. D3590: by its consumed index, never its number (`drain::spill::stream_of`).
    pub(crate) fn stream(&self) -> u64 {
        crate::ops::hashjoin::drain::spill::stream_of(self.consumed)
    }

    /// `P'`'s virtual pipeline id.
    #[allow(dead_code)] // B4c-2: Follows(d, P') comes from the drain end remade under P' (drain.rs), not from here
    pub(crate) fn pass(&self) -> usize {
        self.pass
    }

    /// The hosts' `bits` argument: the partition bits, `FILL` set for a fill probe, the drain flags (`open_pair`).
    fn arg(&self) -> u64 {
        self.bits as u64 | if self.fill { FILL } else { 0 } | self.drain
    }

    /// The window's spill words of the build (`SPILL` bits, `SPILLED` mask; `I64`), read by the probe's `window`.
    pub(crate) fn words(&self, b: &mut FunctionBuilder<'_>, r: Value) -> (Value, Value) {
        (b.load_zext(Type::I8, r, SPILL as i32), b.load(Type::I64, r, SPILLED as i32))
    }

    /// The probe row's exit to its partition's kept rows: under a spilled build (`sp` its bits, `mask` its partitions), a
    /// row whose image routes to a spilled partition has its cells and its KeyHash written (`kept_put`; D2715: a partition
    /// past its region's length splits its kept rows by the hash's next bits) and goes to `l_join` with its carry
    /// unchanged; any other row continues here. The branch's memo is its own (its values reach no later block).
    pub(crate) fn exit(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, img: &ImageWords, (sp, mask): (Value, Value), l_join: Block) {
        let on = b.icmp(IntCond::Ne, sp, env.zero);
        let (l_route, l_keep, l_go) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(on, (l_route, &[]), (l_go, &[]));
        b.switch_to(l_route);
        let h = emit_key_hash(b, &img.words);
        let p = emit_route(b, h, sp);
        let bits = b.int_bin(IntBinOp::Ushr, mask, p);
        let bit = b.int_bin(IntBinOp::And, bits, env.one);
        let out = b.icmp(IntCond::Ne, bit, env.zero);
        b.cond_br(out, (l_keep, &[]), (l_go, &[]));
        b.switch_to(l_keep);
        let memo = env.memo.set_aside();
        let n = row.row.cells.len();
        let slot = b.declare_stack_slot((24 * n.max(1)) as u32, 8);
        let at = b.stack_addr(slot);
        let mut lx = env.lx(b, self.node);
        let mut layout = Layout { cells: mcx::reserved_in(n, self.mcx), kinds: mcx::reserved_in(n, self.mcx), cols: mcx::to_vec_in(&row.cols[..], self.mcx), inner: env.inner.as_deref().map(|v| mcx::to_vec_in(v, self.mcx)) };
        for i in 0..n {
            let Some(cr) = row.cell(i) else { crate::refuse!(self.node, Reason::Bug { payload: "a probe row cell without its value" }) };
            if matches!(cr.cell.life.home, CellHome::ImageOnly) {
                crate::refuse!(self.node, Reason::Shape { why: ShapeWhy("a spilled build's kept row holds an image-only key cell") });
            }
            let (typid, num, nullable) = (cr.cell.typid, cr.cell.num, cr.cell.null);
            let tf = crate::step::expr::tf_datum(typid);
            let scan = (self.scan_forms || crate::ops::hashjoin::spill::forms::record_code(cr.cell)).then(|| crate::ops::hashjoin::spill::forms::scan_form(&mut lx, cr, &tf, i as u16)).flatten();
            let (d, null, kind, cell) = match scan {
                Some((w, cell)) => (w, cr.null.unwrap_or_else(|| lx.b.bconst(false)), 0, cell),
                None => {
                    // D1183: a type the type facts do not list (an enum, a domain) keeps the storage its rep names, never
                    // tf_datum's opaque varlena; a code (text / varchar / bpchar lanes) or packed (numeric) datum is a varlena
                    let rep = match cr.cell.rep {
                        _ if gravity_decompose::catalog::typefacts::row_of(typid).is_some() => Rep::of_storage(tf.typlen, tf.byval),
                        Rep::Code { .. } | Rep::PackedNum { .. } | Rep::PackedNumDyn => Rep::VarlenaPtr,
                        r => r,
                    };
                    let (d, null) = crate::rep::datum(&mut lx, cr, &tf);
                    let d = if lx.b.value_type(d) == Type::Ptr { lx.b.conv(ConvOp::PtrToInt, Type::I64, d) } else { d };
                    let life = CellLife { until: Until::Part, home: CellHome::FrameSlot };
                    (d, null, kind_of(rep), Cell { rep, typid, num, life, at: At::Slot(i as u16), null: nullable })
                }
            };
            let (kc, nz) = (lx.b.iconst(Type::I64, kind as i64), lx.b.conv(ConvOp::Zext, Type::I64, null));
            (lx.b.store(at, 24 * i as i32, kc), lx.b.store(at, 24 * i as i32 + 8, d), lx.b.store(at, 24 * i as i32 + 16, nz));
            layout.kinds.push_in(kind);
            layout.cells.push_in(cell);
        }
        env.memo = lx.memo;
        env.memo.restore(memo);
        // a union's arms (D2084) keep one layout; a cell one arm may leave NULL is read as maybe NULL from every arm's rows
        match &mut *self.layout.borrow_mut() {
            Some(l) if l.kinds != layout.kinds || l.cols != layout.cols || l.inner != layout.inner || !l.cells.iter().map(|c| (c.typid, c.rep)).eq(layout.cells.iter().map(|c| (c.typid, c.rep))) => {
                crate::refuse!(self.node, Reason::Shape { why: ShapeWhy("a spilled build's probes keep rows of two layouts (a union's arms)") })
            }
            Some(l) => l.cells.iter_mut().zip(&layout.cells).filter(|(_, c)| c.null == Nullable::Maybe).for_each(|(k, _)| k.null = Nullable::Maybe),
            l @ None => *l = Some(layout),
        }
        let one = b.iconst(Type::I64, 1);
        let parts = b.int_bin(IntBinOp::Shl, one, sp);
        let part = b.int_bin(IntBinOp::Add, parts, p);
        let (s, nc) = (b.iconst(Type::I64, self.stream() as i64), b.iconst(Type::I64, n as i64));
        let f = host(b, "m7_kept_put", &[Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64, Type::Ptr, Type::I64], gravity_abi::host_words!(kept_put(ctx, stream, bits, part, h, cells, n)));
        let st = b.call_host(f, &[env.ctx, s, sp, part, h, at, nc], Some(Type::I64)).expect("m7_kept_put returns its status");
        raise_neg(b, env, st);
        b.br(l_join, &row.carry.params());
        b.switch_to(l_go);
    }

    /// The probe's `epilogue`: the owner's staged kept rows written at the claim's close, when the build spilled.
    pub(crate) fn flush(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        let r = region(b, env, Some(self.consumed));
        crate::ops::hashjoin::spill::build::flush(b, env, r, self.stream());
    }
}

/// `P'`'s source: claim `p`'s kept rows, the probe's build pointed at partition `p`'s own laid-out region. It has no site:
/// its driving site is `q`'s, nominal (`nominal_site`), and its cursor is the resume word (source/result.rs).
struct KeptSource<'g> {
    kept: PgRc<'g, Kept<'g>>,
    q: usize,
    /// The window's kept rows (`I64`) and their offsets (`Ptr`).
    hv: VCell<Option<(Value, Value)>>,
    /// D3876: the per-code tables `q`'s steps declared under `P'`'s id: their row tables (a constant-arm group key's image,
    /// dict/strategies/key_consts.rs) are bound at each window, as `q`'s source binds them; `P'` has no site table.
    tabs: Rc<RefCell<crate::dict::percode::Tables>>,
}

impl SourceMech for KeptSource<'_> {
    fn nominal_site(&self, px: &Px<'_, '_>) -> Option<usize> {
        match px.pipeline_site(self.q) {
            Some(s) => Some(s),
            None => crate::refuse!(self.kept.node, Reason::Bug { payload: "a spill pair after a pipeline with no site" }),
        }
    }

    /// `q`'s graph descriptor over the partitions. D1182: `P'` runs `q`'s steps and end, so the rule of `q`'s source
    /// (source/heap.rs, pgrc.rs) keeps it on the leader too: a hosted call a pool thread cannot run (an fmgr site, a
    /// volatile or parallel-unsafe call) or a serial SRF.
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline {
        let d = Pipeline { claims: ClaimKind::SpillPartitions { slot: self.kept.slot }, ..px.gx.pipelines()[k].desc };
        match crate::hir::hosted_serial(px, k) {
            Some(reason) if d.is_eligible() => Pipeline { parallel: Parallel::Ineligible { site: d.driving_site, reason }, ..d },
            _ => d,
        }
    }

    /// The claim's first partition opened (`pair_open`), the cursor at its first kept row; a build that never spilled
    /// kept no row, and the call is done at once (nothing taken, nothing read: the never-spilling cost is this call).
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        let k = &self.kept;
        let f = host(b, "m7_pair_open", &[Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64], gravity_abi::host_words!(pair_open(ctx, stream, consumed, bits, ncons)));
        let args = [k.stream(), k.consumed as u64, k.arg(), k.ncons as u64].map(|a| b.iconst(Type::I64, a as i64));
        let st = b.call_host(f, &[env.ctx, args[0], args[1], args[2], args[3]], Some(Type::I64)).expect("m7_pair_open returns its status");
        raise_neg(b, env, st);
        let (none, l_go) = (b.icmp(IntCond::Ne, st, env.zero), b.create_block(&[]));
        b.cond_br(none, (env.x.l_done, &[]), (l_go, &[]));
        b.switch_to(l_go);
        env.zero
    }

    /// The next `span()` kept rows at the cursor. Past a partition's last, the claim's next partition opens (`pair_next`) and
    /// the window is empty at 0: the frame's window loop comes back through the steps' `window`, which read the new
    /// partition's build, and here at its first row. Past the claim's last partition, the pair closes and the claim ends.
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let r = region(b, env, Some(self.kept.consumed));
        let m = b.ptr_add(r, env.zero, 1, -(META as i32));
        let total = b.load(Type::I64, m, M_NKEPT as i32);
        let cur = resume_pos(b, env);
        let done = b.icmp(IntCond::Sge, cur, total);
        let (l_next, l_end, l_go) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64, Type::I64]));
        b.cond_br(done, (l_next, &[]), (l_go, &[cur, total]));
        b.switch_to(l_next);
        let k = &self.kept;
        let f = host(b, "m7_pair_next", &[Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64], gravity_abi::host_words!(pair_next(ctx, stream, consumed, bits, ncons)));
        let args = [k.stream(), k.consumed as u64, k.arg(), k.ncons as u64].map(|a| b.iconst(Type::I64, a as i64));
        let st = b.call_host(f, &[env.ctx, args[0], args[1], args[2], args[3]], Some(Type::I64)).expect("m7_pair_next returns its status");
        raise_neg(b, env, st);
        let more = b.icmp(IntCond::Ne, st, env.zero);
        b.cond_br(more, (l_go, &[env.zero, env.zero]), (l_end, &[]));
        b.switch_to(l_end);
        b.br(l_eof, &[env.zero]);
        b.switch_to(l_go);
        let (cur, total) = (b.block_params(l_go)[0], b.block_params(l_go)[1]);
        let span = b.iconst(Type::I64, span());
        let far = b.int_bin(IntBinOp::Add, cur, span);
        let lt = b.icmp(IntCond::Slt, far, total);
        let end = b.select(lt, far, total);
        // a new partition's empty window reads the pair it opened (its header: the region moved)
        let r = region(b, env, Some(self.kept.consumed));
        let m = b.ptr_add(r, env.zero, 1, -(META as i32));
        let (kb, offs) = (b.load(Type::I64, m, M_KEPT as i32), b.load(Type::Ptr, m, M_OFFS as i32));
        self.hv.set(Some((kb, offs)));
        crate::dict::percode_bind::bind_rows_only(b, env, &self.tabs);
        Win { cursor: cur, end, facts: WinConds::default() }
    }

    /// Kept row `j`: the probe row's cells as `q`'s lowering met them (their datums, or their scan forms), its columns and
    /// INNER_VAR map.
    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let (node, mcx) = (self.kept.node, env.mcx);
        let layout = self.kept.layout.borrow();
        let (Some((kb, offs)), Some(l)) = (self.hv.get(), layout.as_ref()) else {
            crate::refuse!(node, Reason::Bug { payload: "a spill pair row before its window or its probe's layout" })
        };
        let oa = b.ptr_add(offs, j, 8, 0);
        let o = b.load(Type::I64, oa, 0);
        let rp = b.int_bin(IntBinOp::Add, kb, o);
        let p = b.conv(ConvOp::IntToPtr, Type::Ptr, rp);
        let nw = l.cells.len().div_ceil(64);
        let (mut vals, mut nulls) = (mcx::reserved_in(l.cells.len(), mcx), None);
        for (i, c) in l.cells.iter().enumerate() {
            let w = cell_word(b, p, rp, nw, i, l.kinds[i]);
            let null = match c.null {
                Nullable::Never => None,
                Nullable::Maybe => {
                    let word = match nulls {
                        Some((k, word)) if k == i / 64 => word,
                        _ => b.load(Type::I64, p, (8 + 8 * (i / 64)) as i32),
                    };
                    nulls = Some((i / 64, word));
                    let s = b.iconst(Type::I64, (i % 64) as i64);
                    let bits = b.int_bin(IntBinOp::Ushr, word, s);
                    let bit = b.int_bin(IntBinOp::And, bits, env.one);
                    Some(b.icmp(IntCond::Ne, bit, env.zero))
                }
            };
            vals.push_in((w, null));
        }
        env.inner = l.inner.as_deref().map(|m| mcx::to_vec_in(m, mcx));
        RowVals { row: Row { cells: mcx::to_vec_in(&l.cells[..], mcx) }, vals, cols: mcx::to_vec_in(&l.cols[..], mcx), carry, next, at: j }
    }

    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, _out: Value) {
        write_pos(b, env, cur);
    }
}

/// Kept row cell `i`'s word (`I64`) at row `p` (`rp` the row's address as an `I64`, `nw` its null words): a word or a
/// scan form (`kind` 0) as stored; an image's offset from the row's start, made its address.
pub(crate) fn cell_word(b: &mut FunctionBuilder<'_>, p: Value, rp: Value, nw: usize, i: usize, kind: u16) -> Value {
    let w = b.load(Type::I64, p, (8 + 8 * nw + 8 * i) as i32);
    if kind != 0 { b.int_bin(IntBinOp::Add, rp, w) } else { w }
}

#[cfg(test)]
mod grow_tests {
    use gravity_program::protos::{Proto, ProtoKind};

    /// D3512 / D3513: a Merge Append's run 0..4 on slot 3 whose arm 1 is a probe with a pair: the run grows (P' inside it),
    /// a run ending at the probe on its slot takes P' as its last arm, and a seeded drain's run is left to `seeded_arms`.
    #[test]
    fn a_pair_inside_a_run_grows_it() {
        let cx = mcx::MemoryContext::new("test");
        let grows = |protos: &[Proto], grown: &[usize], q: usize, slot: u8| {
            let (ends, inside) = super::grows(protos, grown, q, slot, cx.mcx());
            (ends, inside.to_vec())
        };
        let protos = [Proto::new(ProtoKind::Arms, 0).slot(3).arg(4), Proto::new(ProtoKind::Arms, 1).slot(5).arg(1)];
        assert_eq!(grows(&protos, &[], 1, 3), (None, vec![0]));
        // grown once (arg 5 = four graph pipelines and P'): the probe at its last arm 3 ends it
        let protos = [Proto::new(ProtoKind::Arms, 0).slot(3).arg(5)];
        assert_eq!(grows(&protos, &[0], 3, 3), (Some(0), vec![]));
        assert_eq!(grows(&protos, &[0], 4, 3), (None, vec![]));
        // a run ending at the probe on another slot neither ends there nor holds it
        let protos = [Proto::new(ProtoKind::Arms, 0).slot(4).arg(2)];
        assert_eq!(grows(&protos, &[], 1, 3), (None, vec![]));
        // a seeded drain's run (Follows its first arm) is re-measured from its drain
        let protos = [Proto::new(ProtoKind::Arms, 0).slot(3).arg(4), Proto::new(ProtoKind::Follows, 3).peer(0)];
        assert_eq!(grows(&protos, &[], 1, 3), (None, vec![]));
    }
}

#[cfg(test)]
mod span_tests {
    use gravity_driver::machine::MachineProfile;

    #[test]
    fn the_window_span_follows_the_l2_share() {
        assert_eq!(super::span_of(&MachineProfile::reference()), 1024);
        assert_eq!(super::span_of(&MachineProfile { partition_bytes: 256 << 10, ..MachineProfile::reference() }), 128);
        assert_eq!(super::span_of(&MachineProfile { partition_bytes: 64 << 10, ..MachineProfile::reference() }), 32);
    }
}
