//! Kind `runs.sort` (cards/M3.md L4, M3-plan §3): the full sort. The producer (`RunEnd`, pipeline A's end) writes each
//! row into its run: the key pairs (keylane.rs), the child projection's datum words and null bytes, the fixed images and
//! the varlena copies (resident.rs). A full run is put; the section end (a serial producer's `finish`) seals the last run
//! in its region when nothing was put, so a sort within its grant is read in place. The emit (`RunEmit`, pipeline B's
//! source) opens with `sort_begin` (the sealed run, or the merge of the runs put) and reads row `j` through `sort_next`,
//! each column a cell valid until the next row. Form G (props `group_sort`, V4R-119): a full sort over a keyed group's
//! drain orders the group's records through topk_groups' mechanisms (the registry's). Budget 400 lines. Owner M3-SORT.
use core::cell::Cell as Once;

use gravity_abi::breaker::SORTRUN_HDR;
use gravity_abi::ctx::{hdr, RESUME_NONE};
use gravity_decompose::ops::shared::sort::{SortDesc, SortKey};
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::{Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, StackSlot, Type, Value};
use gravity_kernels::host_abi as k;
use gravity_kernels::ops::sort::entries as se;
use gravity_kernels::util::RESUME_OPEN;
use gravity_program::{LifeId, NumTypmod, Pipeline, Rep, Until};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};
use types_nodes::list::NodeList;

use crate::cell::{At, Cell, CellHome, CellLife, Nullable, Row};
use crate::face::end::EndMech;
use crate::face::kinds::{Face, KindId};
use crate::face::source::{SourceMech, WinConds};
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::{EndMaker, Next, SourceMaker, StepMaker, Win};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{runs_word, Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::face::strategy::{SORT_KEYLANE, SORT_NATURAL, SORT_RESIDENT};
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::frame::iface::{breaker_of, kind_impl, run_holds_rows};
use crate::ops::Table;

use crate::rep::keylane::{ivl, ivl_tail};
use crate::rep::KeyWord;
use crate::face::runs::{Form, Shape};
use crate::ops::sort::strategies::resident as res;

pub(crate) const ROWS: Table = Table {
    kinds: &[(KindId::RunsSort, &StepMaker(make_step)), (KindId::RunsSort, &EndMaker(make_end)), (KindId::RunsSort, &SourceMaker(make_emit))],
    ..crate::ops::NONE
};

/// The rows one emit window reads (a window's end is a cancel check).
const WINDOW: i64 = 4096;

fn bug(node: i32, payload: &'static str) -> ! { crate::refuse!(node, Reason::Bug { payload }) }
fn shape(node: i32, why: &'static str) -> ! { crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) }) }

/// Pipeline `k`'s sort op and the projection before it (the run row's columns by resno), whatever its form (a top, an
/// incremental sort reuse the run: M3-TOPS, M3-MERGE); admission decides which form reaches which maker.
pub(crate) fn sort_of<'g, 'p>(px: &Px<'g, 'p>, k: usize) -> (&'g SortDesc<'p>, &'g NodeList<'p>) {
    let p = &px.gx.pipelines()[k];
    let Some(j) = p.ops.iter().position(|op| matches!(op, Op::Sort(_))) else { bug(p.node_id, "a full sort's pipeline without its sort") };
    let Op::Sort(sd) = &p.ops[j] else { unreachable!() };
    // D1486 (errorhold): right over a settled union that projects nothing, the row is the union record's
    let own = p.ops[..j].iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None });
    let Some(l) = own.or_else(|| crate::rerun::union_arms::record_cols(px.gx, k)) else {
        shape(p.node_id, "a full sort over a row with no projection")
    };
    (sd, l)
}

/// The sort op: the end takes the row as it is.
struct Pass;

impl StepMech for Pass {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        next(b, env, row)
    }
}

fn make_step<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    crate::ops::sort::sort_noproc::checked(px.gx, k); // W8-EQUIVCLASS-SORTOP: a key PG cannot begin, outside a root merge's inner
    // form G: the sort orders the records of the group it drains, so its run holds no rows (runs.topk_groups)
    if !run_holds_rows(px.gx, k) {
        crate::mechs_used::declined(crate::admit::refusal::Variant::Kind(KindId::RunsSort)); // MECHCORPUS: the node is another kind's form
        return (kind_impl::<StepMaker>(&px.gx.pipelines()[k].node_id, KindId::RunsTopkGroups).0)(px, k, j);
    }
    mcx::pg_box_dyn!(px.mcx(), Pass => dyn StepMech + 'g)
}

/// A producer's run roads as given (`make_end` ANDs in the roads) and `finish`: finishing though `p.desc` says not (M3-TOPS).
#[derive(Clone, Copy)]
pub(crate) struct Arm {
    pub(crate) keylane: bool,
    pub(crate) natural: bool,
    pub(crate) resident: bool,
    pub(crate) finish: bool,
}

/// The producer: the run (the open's, kept in frame word `w_run`) filled a row at a time.
pub(crate) struct RunEnd {
    pub(crate) shape: Shape,
    node: i32,
    /// The pipeline has section-end calls (`Pipeline::finish`): the last run is the section end's, never `finish`'s.
    finishing: bool,
    /// Roads sort.keylane, sort.natural (the run's `pad[2]` marks), sort.resident (the open, the merge's protocol).
    pub(crate) keylane: bool,
    pub(crate) natural: bool,
    pub(crate) resident: bool,
    w_run: i32,
    /// What a collated key's order word is for (`Lx::key_word`): `Plain` for a top over a presorted prefix (topk_rows.rs
    /// `plain_ok`), `Sort` otherwise.
    pub(crate) key_word: KeyWord,
}

fn make_end<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let (p, node) = crate::ops::sort::sort_raise::checked(px.gx, k); // D1292: a raising Sort outside the served scope refuses
    // USEROP: a SQL / PL comparator over a hash join PostgreSQL may run in batches (any form: a sorted group's, an
    // incremental sort's, a full sort's), unless PostgreSQL certainly holds it in one (ops/shared/one_batch.rs)
    if crate::effects::visible::rep::user_batches(px.gx, k) && !crate::ops::shared::one_batch::served(px, k) {
        shape(node, crate::rep::keylane::USER_BATCHES);
    }
    if !run_holds_rows(px.gx, k) {
        crate::mechs_used::declined(crate::admit::refusal::Variant::Kind(KindId::RunsSort)); // MECHCORPUS: the node is another kind's form
        return (kind_impl::<EndMaker>(&node, KindId::RunsTopkGroups).0)(px, k);
    }
    let (sd, cols) = sort_of(px, k);
    if sd.presorted > 0 {
        crate::mechs_used::declined(crate::admit::refusal::Variant::Kind(KindId::RunsSort)); // MECHCORPUS: the node is another kind's form
        return (kind_impl::<EndMaker>(&node, KindId::RunsIncremental).0)(px, k); // I-C (D479): runs/incremental.rs
    }
    // USEROP (K10.y1 .h0): a CompareFn key over another breaker's rows (decompose's full-sort lift) under a SQL / PL
    // comparator: the rows arrive in that breaker's order, not PostgreSQL's (a hashed group's, ties of a sort's)
    // unless they arrive from a feeding sort that keeps PostgreSQL's order (exact_feed.rs)
    // D3374 (e): and a per-row record key, whose record_cmp raises on the pairs of two layouts it meets
    if matches!(p.source, Source::Breaker { .. }) && sd.keys.iter().any(|key| key.per_row || key.compare_fn() && !crate::rep::keylane::c_level_cmp(px.gx, key.opno)) && !crate::ops::sort::exact_feed::fed_exact(px.gx, k) {
        shape(node, crate::rep::keylane::USER_ARRIVAL);
    }
    let union_arm = union_arm(p);
    // D3374 (e) (record keys E2): a per-row record key over a union's arms is served. Each arm sorts its run apart and the
    // reader merges the runs, where PostgreSQL's Sort over the Append runs one qsort over the rows in arrival order: the two
    // compare other pairs, but record_cmp raises only on two rows of other layouts equal up to their first column of other
    // types, and a complete sort (every row out, no bound) must compare such a pair or it cannot order them: both raise
    // 42804 on the same rows, only the operands it names may differ (the both-raise rule). A bound (a top's per-arm cut) is
    // another kind's end; a Merge Append over such arms keeps its refusal (ops/append/merge.rs PER_ROW_MERGE).
    // D3024 (b): a wide group emit's claims fill one run per claim thread, sealed at the thread's section end, as a
    // claim-driven source's (`wide_row_end`)
    // D3336: so does a reader of a window by claims (face/partruns.rs `reads_wide`)
    let wide = px.wide.contains(&k) || crate::face::partruns::reads_wide(px, k);
    let (keylane, resident) = (crate::ops::sort::shapes::lane_key(sd, None), p.desc.finish || wide);
    let arm = Arm { keylane: keylane && res::on(px, SORT_KEYLANE), natural: res::on(px, SORT_NATURAL), resident: union_arm.is_some() || resident && res::on(px, SORT_RESIDENT), finish: wide };
    let mut end = RunEnd::over(px, k, &sd.keys, Some(sd.tlist), cols, arm);
    // ... but a wide emit the leader keeps (table/emit_group.rs `kept_on_leader`, read now that this end's expressions are
    // made, as the emit's descriptor reads it) runs site-less in one call: this end seals the run, as a serial producer's
    if wide && crate::table::emit_group::kept_on_leader(px, k) {
        end.kept_serial(px, k, union_arm.is_some());
    }
    if let (true, Some(br), true) = (end.resident, breaker_of(&p.desc), union_arm.is_none_or(|i| i == 0)) {
        res::declare(px, k, br.slot, (!end.shape.var()).then_some((end.shape.spec, 2 * end.shape.keys.len() as u16)));
    }
    crate::ops::sort::sort_raise::wrap(px.gx, k, mcx::pg_box_dyn!(px.mcx(), end => dyn EndMech + 'g)) // D1292: PG's 42P22 at the Sort's start
}

/// D3024 (b): whether pipeline `k` ends in a row sort or a row top that takes section ends when its source is a wide keyed
/// group emit (table/emit_group.rs, which records the pipeline in `Px::wide` before this end is made): a full sort's run
/// (`runs.sort`, form G excluded: that sort is the emit's own) or a top's (`runs.topk_rows`), over no presorted prefix (an
/// incremental sort reads its descriptor's sections alone) and not a union's arm. Each claim thread fills its own run, sealed
/// at its section end, and the reader merges the runs, as for a claim-driven scan's sort.
pub(crate) fn wide_row_end(gx: &crate::graph::Gx<'_, '_>, k: usize) -> bool {
    let p = &gx.pipelines()[k];
    let Some(d) = p.ops.iter().find_map(|op| if let Op::Sort(d) = op { Some(d) } else { None }) else { return false };
    matches!(gx.end_need(k), KindId::RunsSort | KindId::RunsTopkRows) && d.presorted == 0 && union_arm(p).is_none()
}

/// The arm a sort's pipeline is of a union of two arms or more (each arm's run is resident: append.arms' `ArmSort`
/// merges the arms' runs, a var run's shape read from its table copy).
pub(crate) fn union_arm(p: &gravity_decompose::PipelineNode<'_>) -> Option<u16> {
    p.ops.iter().filter_map(|o| match o { Op::Append(AppendDesc::Arm { i, n, .. }) if *n > 1 => Some(*i), _ => None }).max()
}

/// `make_end` over explicit keys (M3-MERGE's arms): the caller declares any protocol (`resident::declare`).
pub(crate) fn make_end_over<'g>(px: &mut Px<'g, '_>, k: usize, keys: &[SortKey], cols: &NodeList<'_>, arm: Arm) -> PgBox<'g, dyn EndMech + 'g> {
    mcx::pg_box_dyn!(px.mcx(), RunEnd::over(px, k, keys, None, cols, arm) => dyn EndMech + 'g)
}

/// D3024 (b): a run left to its section end (a wide emit's) on a pipeline that runs none: its wide emit was kept on the leader
/// by an expression made after the end chose (`RunEnd::kept_serial`), so the run would never be sealed and its reader would
/// read no row. Refused by name, never an empty answer.
pub(crate) const WIDE_KEPT_LATE: &str = "a wide group emit kept on the leader after its row sort or top chose to take section ends (D3024 (b): a pool-unsafe call made past the end)";

impl RunEnd {
    /// D3024 (b): the run of a wide emit the leader keeps (table/emit_group.rs `kept_on_leader`): its pipeline runs site-less in
    /// one call with no section end, so this end puts and seals the last run itself (`finish`), as a serial producer's, and
    /// its residence is the descriptor's.
    pub(crate) fn kept_serial(&mut self, px: &Px<'_, '_>, k: usize, union_arm: bool) {
        let fin = px.gx.pipelines()[k].desc.finish;
        self.finishing = fin;
        self.resident = union_arm || fin && res::on(px, SORT_RESIDENT);
    }

    /// Pipeline `k`'s producer over `keys` (through `tlist` when a Sort's) and the child projection `cols`.
    pub(crate) fn over(px: &mut Px<'_, '_>, k: usize, keys: &[SortKey], tlist: Option<&NodeList<'_>>, cols: &NodeList<'_>, arm: Arm) -> RunEnd {
        let p = &px.gx.pipelines()[k];
        let (node, finishing) = (p.node_id, p.desc.finish || arm.finish);
        // a feeding sort's run orders by PostgreSQL's qsort in arrival order, its rows taken serially in heap order
        let (shape, exact) = crate::ops::sort::exact_feed::shape(px, k, keys, tlist, cols);
        if exact {
            px.serial.push(k); // alloc: Program
        }
        let Arm { keylane, natural, resident, .. } = arm;
        let keylane = keylane && !exact && shape.keys.len() == keys.len(); // lane_key judged the first key, which a Const key drops
        let w_run = px.frame_word(WordKey::Open { face: Face::Runs, k, j: u64::MAX, i: runs_word::END_RUN }) as i32 * 8; // D482
        RunEnd { shape, node, finishing, keylane, natural, resident, w_run, key_word: KeyWord::Sort }
    }

    pub(crate) fn run(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> Value {
        let r = env.load_word(b, self.w_run);
        b.conv(ConvOp::IntToPtr, Type::Ptr, r)
    }

    /// Whether the full run grew in place (`sort_grow`: its region started small and had room to double); a status below
    /// zero leaves through `l_exit`.
    fn grow(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> Value {
        let run = self.run(b, env);
        let tbl = if self.shape.var() { self.shape.table(b) } else { b.conv(ConvOp::IntToPtr, Type::Ptr, env.zero) };
        let h = res::grow(b);
        let r = b.call_host(h, &[env.ctx, run, tbl], Some(Type::I64)).expect("sort_grow returns its answer");
        let (bad, l_ok) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]));
        b.cond_br(bad, (env.x.l_exit, &[r]), (l_ok, &[]));
        b.switch_to(l_ok);
        b.icmp(IntCond::Sgt, r, env.zero)
    }

    /// The run put (`last`: a serial producer's last run); `l_exit(status)` below zero.
    pub(crate) fn put(&self, b: &mut FunctionBuilder<'_>, env: &Env, last: bool) {
        let run = self.run(b, env);
        let h = res::put(b, self.shape.var(), last, self.keylane || self.natural);
        let r = if self.shape.var() {
            let tbl = self.shape.table(b);
            b.call_host(h, &[env.ctx, run, tbl], Some(Type::I64))
        } else {
            let [_, nkey, spec] = self.shape.args(b);
            b.call_host(h, &[env.ctx, run, nkey, spec], Some(Type::I64))
        }
        .expect("a sort put returns its status");
        let (bad, l_ok) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]));
        b.cond_br(bad, (env.x.l_exit, &[r]), (l_ok, &[]));
        b.switch_to(l_ok);
        self.next_run(b, env, last, r);
    }

    /// D1330: a var put's status `r` above zero is the run the next row goes to (`res::next_run`; a serial producer's last
    /// put names none).
    pub(crate) fn next_run(&self, b: &mut FunctionBuilder<'_>, env: &Env, last: bool, r: Value) {
        if self.shape.var() && !last {
            res::next_run(b, env, self.w_run, r)
        }
    }

    /// `open`; `top` (D3027 (b), a resident top's: its bound `n` and cut flags) opens through `res::open_top`, whose section
    /// end cuts the run to `n` rows before it seals or puts.
    pub(crate) fn open_bounded(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, top: Option<(Value, i64)>) {
        let top = top.filter(|_| self.resident).map(|(n, flags)| (n, b.iconst(Type::I64, flags)));
        let h = match top {
            Some(_) => res::open_top(b, self.shape.var()),
            None => res::open(b, self.shape.var(), self.resident),
        };
        let run = if self.shape.var() {
            let tbl = self.shape.table(b);
            match top {
                Some((n, flags)) => b.call_host(h, &[env.ctx, tbl, n, flags], Some(Type::Ptr)),
                None => b.call_host(h, &[env.ctx, tbl], Some(Type::Ptr)),
            }
        } else {
            let [rb, nkey, spec] = self.shape.args(b);
            match top {
                Some((n, flags)) => b.call_host(h, &[env.ctx, rb, nkey, spec, n, flags], Some(Type::Ptr)),
                None => b.call_host(h, &[env.ctx, rb, nkey, spec], Some(Type::Ptr)),
            }
        }
        .expect("the sort open returns the run");
        let ri = b.conv(ConvOp::PtrToInt, Type::I64, run);
        let (l_end, l_go, l_bad) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        let live = b.icmp(IntCond::Sgt, ri, env.zero);
        b.cond_br(live, (l_go, &[]), (l_end, &[]));
        b.switch_to(l_end);
        let done = b.icmp(IntCond::Eq, ri, env.zero);
        b.cond_br(done, (env.x.l_done, &[]), (l_bad, &[]));
        b.switch_to(l_bad);
        b.br(env.x.l_exit, &[ri]);
        b.switch_to(l_go);
        env.store_word(b, self.w_run, ri);
    }
}

impl EndMech for RunEnd {
    fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("sort run", Base::Words, self.w_run, Type::I64, 0));
    }

    /// The claim's run; at the section end the host sealed or put the last run and returned null (done) or a status below
    /// zero: the call leaves through `l_done` / `l_exit`, its source never opened.
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        self.open_bounded(b, env, None);
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        self.append(b, env, row, &mut |b, env, _| self.put(b, env, false))
    }

    /// D3990: the append copies the row (its datums, var copies, key pairs) into the run.
    fn copies_row(&self) -> bool {
        true
    }

    /// A serial producer's last run (sealed in its region when nothing was put); a finishing pipeline's is its section end's.
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _x: &Exits) {
        if !self.finishing {
            self.put(b, env, true);
        }
    }

    /// RUNS-26: the run's row as the lowering laid it out (`Shape::row_bytes`: words, null bytes, key pairs; a var row's
    /// fixed part), which the breaker cost reads for a Sort (program cost::breaker `record`, the planner's width its floor).
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize, d: Pipeline) -> Pipeline {
        // D3024 (b): a run finishing though the decomposer's descriptor is serial (a wide emit's) keeps its section end, which
        // its emit's driving site runs; a wide emit kept on the leader after this end chose would run none (`WIDE_KEPT_LATE`)
        if self.finishing && !d.finish && d.driving_site < 0 && px.wide.contains(&k) {
            crate::refuse!(self.node, Reason::Shape { why: crate::admit::refusal::ShapeWhy(WIDE_KEPT_LATE) });
        }
        Pipeline { finish: d.finish || self.finishing, ..d }.storing_rows(self.shape.row_bytes)
    }
}

impl RunEnd {
    /// The row appended to the run: its datums, null bytes, images, var copies and key pairs, the `pad[2]` marks, `len`
    /// bumped. `on_full` runs when the run has no room (a put; M3-TOPS: its cut), then the row is written again; it sees the
    /// row's key pairs (D2432: whether the row continues the run's last prefix group).
    pub(crate) fn append<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, on_full: &mut dyn FnMut(&mut FunctionBuilder<'_>, &Env, &[[Value; 2]])) -> Carry<'e> {
        let s = &self.shape;
        let mcx = env.mcx;
        let t = b.bconst(true);
        let mut cx = env.lx(b, self.node);
        cx.key_word = self.key_word;
        let tfs: PgVec<'_, TypeFacts> = s.cols.iter().map(|c| c.tf).collect_in(env.mcx);
        let vals: PgVec<'_, (Value, Value)> = s.cols.iter().enumerate().map(|(c, col)| {
            let Some(cr) = row.col(c as i16 + 1) else { bug(self.node, "a sorted column past the row") };
            let Form::Code { dict, producer } = col.form else { return crate::rep::datum(&mut cx, cr, &col.tf) };
            if !matches!(cr.cell.rep, Rep::Code { dict: d } if d.lane == dict.lane) {
                bug(self.node, "a carried code column whose cell is not its lane's code");
            }
            let kept = crate::rep::retain(&mut cx, cr, &col.tf, Until::Statement, crate::rep::RetainHome::Run(LifeId(producer)));
            (kept.val, kept.null.unwrap_or_else(|| cx.b.bconst(false)))
        }).collect_in(env.mcx);
        let pairs = crate::rep::keylane::pairs(&mut cx, self.node, row, &s.keys, &tfs[..], mcx);
        let ivt: PgVec<'_, Option<[Value; 2]>> = s.cols.iter().enumerate().map(|(c, col)| ivl(&col.tf).then(|| ivl_tail(&mut cx, self.node, row, c, &col.tf))).collect_in(env.mcx);
        env.memo = cx.memo;
        let rb = b.iconst(Type::I64, s.row_bytes as i64);
        let (l_try, l_full, l_store) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.br(l_try, &[]);
        b.switch_to(l_try);
        let run = self.run(b, env);
        let len = b.load_zext(Type::I32, run, 4);
        let cap = b.load_zext(Type::I32, run, 0);
        let full = b.icmp(IntCond::Uge, len, cap);
        if s.var() {
            res::var_room(b, run, len, cap, rb, full, l_full, l_store);
        } else {
            b.cond_br(full, (l_full, &[]), (l_store, &[]));
        }
        // every retry passes the cancel check (D1028: an `on_full` that frees no room would spin); the row path takes none
        b.switch_to(l_full);
        crate::frame::cancel::check(b, env.ctx, env.l_cancel);
        // a run whose region started small grows in place first (`sort_grow`) and the row is written again into it; any
        // other run, or a refusal, runs `on_full` (a put, a cut) as before
        let l_on = b.create_block(&[]);
        let grew = self.grow(b, env);
        b.cond_br(grew, (l_try, &[]), (l_on, &[]));
        b.switch_to(l_on);
        on_full(b, env, &pairs);
        b.br(l_try, &[]);
        b.switch_to(l_store);
        let off = b.int_bin(IntBinOp::Mul, len, rb);
        let at = b.ptr_add(run, off, 1, SORTRUN_HDR as i32);
        if s.var() {
            res::store_vars(b, env, run, at, &vals[..], &s.vars(), &s.payloads(mcx), s.row_bytes, s.rec_off, l_full);
        }
        let (dv, dn) = (s.datums(), s.nulls());
        for (c, col) in s.cols.iter().enumerate() {
            let (v, null) = vals[c];
            let nb = b.conv(ConvOp::Zext, Type::I8, null);
            b.store(at, dn + c as i32, nb);
            let word = if matches!(col.form, Form::Word | Form::Code { .. }) { b.select(null, env.zero, v) } else { env.zero };
            b.store(at, dv + 8 * c as i32, word);
            if let Form::Image { off } = col.form {
                let valid = b.int_bin(IntBinOp::Xor, null, t);
                res::store_image(b, env, at, off as i32, col.tf.typlen, v, valid);
                ivt[c].iter().flatten().enumerate().for_each(|(i, &w)| b.store(at, off as i32 + 16 + 8 * i as i32, w));
            }
        }
        for (i, [flags, word]) in pairs.iter().enumerate() {
            b.store(at, 16 * i as i32, *flags);
            b.store(at, 16 * i as i32 + 8, *word);
        }
        crate::ops::sort::strategies::natural::mark(b, env, run, at, len, rb, &pairs, &s.tailed(mcx), self.keylane, self.natural);
        let len1 = b.int_bin(IntBinOp::Add, len, env.one);
        let len32 = b.conv(ConvOp::Trunc, Type::I32, len1);
        b.store(run, 4, len32);
        let out = b.int_bin(IntBinOp::Add, row.carry.out, env.one);
        Carry::new(out, env.mcx)
    }

}

/// The window's reader: the table and the mode.
#[derive(Clone, Copy)]
struct Reader {
    tbl: Value,
    mode: Value,
}

/// The emit: `sort_begin` at the fresh call (its mode filed), then row `j` of the sorted rows per `sort_next`.
pub(crate) struct RunEmit {
    shape: Shape,
    /// D457: a reader whose pipeline ends in a breaker reads the run at its consumed index `k` (and the sort's slot's stream) through
    /// the `_at` hosts, else its own region is the run; D1339 row B: an embedded inner, among its owner's regions (`Pipes::consumed_at`), `.2`.
    at: Option<(u64, u64, bool)>,
    w_mode: i32,
    w_cursor: i32,
    /// D463: the rows the emit sends (`sort_total`), so a window is exactly its rows (an end may take one whole)
    w_total: i32,
    reader: Once<Option<Reader>>,
    /// D2004: the open's emit table slot, its address the reader's identity (`SortFinish` past the last window).
    open_tbl: Once<Option<StackSlot>>,
    /// D2004: the run is read by more than this pipeline (a grouping set's arms over one sort), so a merge this reader
    /// left in flight is given up past its last window; a run read once makes no call.
    shared: bool,
    /// D3190: the outer of a co-split merge join (face/cosplit.rs): each claim reads its range of the run.
    cosplit: Option<crate::face::cosplit::At>,
    /// D3336 (A1a, face/partfill.rs `rows_form`): a parted window's fill over the run's K-cut parts (its `cosplit` the
    /// cut's range table): the frame word of the claim's window object, which its open files, and the window's object
    /// table word.
    cut: Option<(i32, u16)>,
    /// D2316: the reader of an Incremental Sort held at its rows (inc_held.rs): its rows end at those PostgreSQL's batches
    /// handed on, the held error raised past them.
    held: Option<crate::ops::sort::inc_held::Read>,
}

/// D3190: the co-split's range table (its handle word's address), as the kernels' `sort_cosplit_*` read it.
fn cosplit_table(b: &mut FunctionBuilder<'_>, env: &Env, c: crate::face::cosplit::At) -> Value {
    b.load(Type::Ptr, env.params, c.h as i32 * 8)
}

/// D3336 (A1a, face/partfill.rs `rows_form`): the window object a K-cut fill's claim stores into, filed in frame word `obj`
/// (the window's end reads it, ops/window/rows.rs `WinEnd`): a claim of one object `i` (the window's fill claims) over a
/// cut run (the range table's mode not `COSPLIT_MODE_WHOLE`) takes object `i`'s handle (word `WINDOW_PARTS_AT + i` of the
/// object table in param word `table`); any other claim 0, the window dealing the partitions in turn (uncut, claim 0 reads
/// every row; the others read none).
fn cut_object(b: &mut FunctionBuilder<'_>, env: &mut Env, c: crate::face::cosplit::At, obj: i32, table: u16) {
    use gravity_program::protos::{COSPLIT_MODE, COSPLIT_MODE_WHOLE, WINDOW_PARTS_AT, WINDOW_PARTS_COUNT};
    let lo = b.load(Type::I64, env.ctx, hdr::CLAIM_LO as i32);
    let hi = b.load(Type::I64, env.ctx, hdr::CLAIM_HI as i32);
    let ctab = cosplit_table(b, env, c);
    let mode = b.load(Type::I64, ctab, 8 * COSPLIT_MODE as i32);
    let wtbl = b.load(Type::Ptr, env.params, table as i32 * 8);
    let n = b.load(Type::I64, wtbl, 8 * WINDOW_PARTS_COUNT as i32);
    let width = b.int_bin(IntBinOp::Sub, hi, lo);
    let one = b.icmp(IntCond::Eq, width, env.one);
    let fits = b.icmp(IntCond::Ule, hi, n);
    let whole = b.iconst(Type::I64, COSPLIT_MODE_WHOLE as i64);
    let cut = b.icmp(IntCond::Ne, mode, whole);
    let ok = b.int_bin(IntBinOp::And, one, fits);
    let ok = b.int_bin(IntBinOp::And, ok, cut);
    let i = b.select(ok, lo, env.zero);
    let at = b.ptr_add(wtbl, i, 8, 8 * WINDOW_PARTS_AT as i32);
    let h = b.load(Type::I64, at, 0);
    let h = b.select(ok, h, env.zero);
    env.store_word(b, obj, h);
}

fn make_emit<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let p = &px.gx.pipelines()[k];
    let node = p.node_id;
    let Source::Breaker { slot } = p.source else { bug(node, "a sort's emit over a source that is not its breaker") };
    let Some(life) = px.gx.life_of(k, slot) else { bug(node, "a sort's emit with no producer") };
    let producer = life.0 as usize;
    if !run_holds_rows(px.gx, producer) {
        crate::mechs_used::declined(crate::admit::refusal::Variant::Kind(KindId::RunsSort)); // MECHCORPUS: the node is another kind's form
        return (kind_impl::<SourceMaker>(&node, KindId::RunsTopkGroups).0)(px, k);
    }
    let (sd, cols) = sort_of(px, producer);
    let (shape, _) = crate::ops::sort::exact_feed::shape(px, producer, &sd.keys, Some(sd.tlist), cols); // its producer's shape
    // INCSTREAM: an unbounded incremental sort under a LockRows a LIMIT stops, or a cursor's root, is read as it fills (inc_stream.rs)
    let pre = if sd.presorted > 0 && shape.keys.len() == sd.keys.len() { (sd.presorted as usize).min(sd.keys.len()) } else { 0 };
    let shape = match crate::ops::sort::inc_stream::reader(px, k, producer, shape, pre) {
        Ok(src) => return crate::ops::sort::sort_raise::wrap_emit(px.gx, k, src),
        Err(shape) => shape,
    };
    // D2316: a held Incremental Sort's reader reads on the leader, never co-split
    let held = crate::ops::sort::inc_held::reader(px.gx, producer) == Some(k);
    let cosplit = if held { None } else { crate::face::cosplit::declare(px, k, producer, &shape) }; // D3190: a co-split merge join's outer
    // D3336 (A1a): a parted window's fill reads its claim's K-cut part of the run as a co-split outer reads its range
    let rows = if held || cosplit.is_some() { None } else { crate::face::partfill::rows_form(px, k, producer, &shape) };
    let cut = rows.map(|(_, table)| (crate::face::partfill::object_word(px, k), table));
    let mut emit = RunEmit { cosplit: cosplit.or(rows.map(|(at, _)| at)), cut, ..RunEmit::over(px, k, shape) };
    if held {
        emit.held = crate::ops::sort::inc_held::read(px, k, producer, emit.at.map(|(i, ..)| i));
    }
    crate::ops::sort::sort_raise::wrap_emit(px.gx, k, mcx::pg_box_dyn!(px.mcx(), emit => dyn SourceMech + 'g)) // D1292: an empty producer's raise
}

/// `make_emit` over explicit keys (M3-MERGE): pipeline `k` reads the run of `keys` over `cols`.
pub(crate) fn make_emit_over<'g>(px: &mut Px<'g, '_>, k: usize, keys: &[SortKey], cols: &NodeList<'_>) -> PgBox<'g, dyn SourceMech + 'g> {
    let node = px.gx.pipelines()[k].node_id;
    let shape = Shape::plan(px, node, keys, None, cols);
    mcx::pg_box_dyn!(px.mcx(), RunEmit::over(px, k, shape) => dyn SourceMech + 'g)
}

impl RunEmit {
    /// Pipeline `k`'s reader of a run of `shape` (M3-TOPS wraps it with its window).
    pub(crate) fn over(px: &mut Px<'_, '_>, k: usize, shape: Shape) -> RunEmit {
        let word = |px: &mut Px<'_, '_>, i: u8| px.frame_word(WordKey::Open { face: Face::Runs, k, j: u64::MAX, i }) as i32 * 8;
        let (w_mode, w_cursor, w_total) = (word(px, runs_word::READER_MODE), word(px, runs_word::READER_CURSOR), word(px, runs_word::READER_TOTAL));
        let p = &px.gx.pipelines()[k];
        // a breaker end makes the pipeline's own region its slot's (driver open_pipeline): read the run where it is consumed;
        // an embedded inner where its running call declares it (D2331: a join drain's own list, rerun/sets.rs `run_region`)
        let inner = k >= px.gx.pipelines().len();
        let at = match (breaker_of(&p.desc), &p.source) {
            (e, &Source::Breaker { slot }) if inner || e.is_some() => match if inner { crate::rerun::sets::run_region(px, k, slot) } else { px.gx.pipelines().consumed_at(k, slot) } {
                Some(i) => Some((i as u64, slot as u64, k >= px.gx.pipelines().len())),
                None => bug(p.node_id, "a sort's reader that does not consume its slot"),
            },
            _ => None,
        };
        let shared = match (&at, &p.source) {
            (Some((.., inner)), &Source::Breaker { slot }) => *inner || {
                let (n, life) = (px.gx.pipelines().len(), px.gx.life_of(k, slot));
                (0..n).filter(|&j| px.gx.pipelines()[j].desc.consumes.contains(&slot) && px.gx.life_of(j, slot) == life).count() > 1
            },
            _ => false,
        };
        RunEmit { shape, at, w_mode, w_cursor, w_total, reader: Once::new(None), open_tbl: Once::new(None), shared, cosplit: None, cut: None, held: None }
    }

    /// A pull-form merge's Sort arm (ops/append/merge_pull.rs through frame/iface/sort.rs `sort_cursor`): the run full sort
    /// `producer` leaves in `slot`, read by another pipeline's code (the merge reader's) at that pipeline's consumed index
    /// `i`; its mode, cursor and total words keyed by `words_k` (the arm the reader pulls, a graph pipeline), the shape
    /// the producer's own emit reads (`make_emit`). It is `make_emit`'s Runs form only: a run that holds rows (form G's
    /// emit is another kind) and no presorted prefix (no INCSTREAM reader), both checked here. No 42P22 wrap
    /// (`sort_raise::wrap_emit`): that wrap answers an empty producer whose sort raised, and a Sort whose rows can raise
    /// refuses at its producer's end where the root does not read its slot, so this run never reaches the reader unraised.
    /// `shared: false` (no D2004 `SortFinish` past the last window): the merge reader is the run's one reader, and no
    /// rerun set re-opens that reader over a run its producer did not remake (merge_admit.rs `merge_pull` refuses a reader
    /// in an owner set, `rerun::sets::in_owner_set`); lifting that bar must revisit D2004's finish.
    pub(crate) fn over_at(px: &mut Px<'_, '_>, words_k: usize, producer: usize, slot: u8, i: usize) -> RunEmit {
        let node = px.gx.pipelines()[producer].node_id;
        if words_k >= px.gx.pipelines().len() {
            bug(node, "a Sort arm's run cursor keyed by an embedded inner");
        }
        if !run_holds_rows(px.gx, producer) {
            bug(node, "a Sort arm's run cursor over a run that holds no rows (form G)");
        }
        let (sd, cols) = sort_of(px, producer);
        if sd.presorted > 0 {
            bug(node, "a Sort arm's run cursor over an incremental sort");
        }
        let (shape, _) = crate::ops::sort::exact_feed::shape(px, producer, &sd.keys, Some(sd.tlist), cols);
        let word = |px: &mut Px<'_, '_>, x: u8| px.frame_word(WordKey::Open { face: Face::Runs, k: words_k, j: u64::MAX, i: x }) as i32 * 8;
        let (w_mode, w_cursor, w_total) = (word(px, runs_word::READER_MODE), word(px, runs_word::READER_CURSOR), word(px, runs_word::READER_TOTAL));
        RunEmit { shape, at: Some((i as u64, slot as u64, false)), w_mode, w_cursor, w_total, reader: Once::new(None), open_tbl: Once::new(None), shared: false, cosplit: None, cut: None, held: None }
    }

    /// INCSTREAM (inc_stream.rs): the reader's mode, cursor and total words.
    pub(crate) fn words(&self) -> (i32, i32, i32) {
        (self.w_mode, self.w_cursor, self.w_total)
    }

    /// `open`'s `sort_begin` (the mode and the total filed; a status below zero, else 0); `keep`: the call's resume word kept
    /// around it (NEW-43: INCSTREAM's reader opens the merge mid-read, after its producer fell back to the breaker form).
    fn begin(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, keep: bool) -> Value {
        let (tbl, slot) = self.shape.table_slot(b);
        self.open_tbl.set(Some(slot));
        let (h, at) = match (self.at, self.cosplit) {
            (None, _) => (res::host(b, "sort_begin", &[Type::Ptr, Type::Ptr], Type::I64, gravity_abi::host_words!(k::sort_begin(ctx, tbl))), mcx::pg_vec![in env.mcx]),
            // D3190: a co-split claim's open (its part's merge of the put runs' slices, or `sort_begin_at`'s)
            (Some((i, ..)), Some(c)) => (res::host(b, "sort_cosplit_begin", &[Type::Ptr, Type::Ptr, Type::I64, Type::Ptr], Type::I64, gravity_abi::host_words!(se::sort_cosplit_begin(ctx, tbl, k, ctab))), mcx::pg_vec![in env.mcx; b.iconst(Type::I64, i as i64), cosplit_table(b, env, c)]),
            (Some((i, s, _)), None) => (res::host(b, "sort_begin_at", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64], Type::I64, gravity_abi::host_words!(k::sort_begin_at(ctx, tbl, k, slot))), mcx::pg_vec![in env.mcx; b.iconst(Type::I64, i as i64), b.iconst(Type::I64, s as i64)]),
        };
        let saved = keep.then(|| b.load(Type::I64, env.ctx, hdr::RESUME as i32)).or_else(|| matches!(self.at, Some((.., true))).then(|| b.load(Type::I64, env.ctx, hdr::RESUME as i32))); // D1339 row B: the owner's resume word kept
        let none = b.iconst(Type::I64, RESUME_NONE);
        b.store(env.ctx, hdr::RESUME as i32, none);
        let mode = b.call_host(h, &mcx::concat_in(&[&[env.ctx, tbl][..], &at[..]], env.mcx), Some(Type::I64)).expect("sort_begin returns the mode");
        let open = saved.unwrap_or_else(|| b.iconst(Type::I64, RESUME_OPEN));
        b.store(env.ctx, hdr::RESUME as i32, open);
        let bad = b.icmp(IntCond::Slt, mode, env.zero);
        let kept = b.select(bad, env.zero, mode);
        env.store_word(b, self.w_mode, kept);
        let total = match self.cosplit {
            Some(_) => {
                let h = res::host(b, "sort_cosplit_total", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::Ptr], Type::I64, gravity_abi::host_words!(se::sort_cosplit_total(ctx, tbl, mode, k, ctab)));
                b.call_host(h, &mcx::concat_in(&[&[env.ctx, tbl, kept][..], &at[..]], env.mcx), Some(Type::I64)).expect("sort_cosplit_total returns the rows")
            }
            None => res::total(b, &mcx::concat_in(&[&[env.ctx, tbl, kept][..], &at[..]], env.mcx)),
        };
        env.store_word(b, self.w_total, total);
        let st = { let short = b.icmp(IntCond::Slt, total, env.zero); b.select(short, total, env.zero) };
        b.select(bad, mode, st)
    }

    /// NEW-43: the merge opened mid-read (`begin` with the call's resume word kept), then its first `n` rows read past
    /// (`l_eof(status)` on a status below zero, or on a merge shorter than `n`: UNSUPPORTED).
    pub(crate) fn begin_past(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, n: Value, l_eof: Block) {
        let st = self.begin(b, env, true);
        let (l_bad, l_go) = (b.create_block(&[]), b.create_block(&[]));
        let bad = b.icmp(IntCond::Slt, st, env.zero);
        b.cond_br(bad, (l_bad, &[]), (l_go, &[]));
        b.switch_to(l_bad);
        b.br(l_eof, &[st]);
        b.switch_to(l_go);
        let mode = env.load_word(b, self.w_mode);
        let tbl = self.shape.table(b);
        let [l_body, l_done, l_short] = [(); 3].map(|_| b.create_block(&[]));
        let l_head = b.create_block(&[Type::I64]);
        b.br(l_head, &[env.zero]);
        b.switch_to(l_head);
        let j = b.block_params(l_head)[0];
        let more = b.icmp(IntCond::Slt, j, n);
        b.cond_br(more, (l_body, &[]), (l_done, &[]));
        b.switch_to(l_body);
        crate::frame::cancel::check(b, env.ctx, env.l_cancel);
        let p = match self.at {
            None => {
                let h = res::host(b, "sort_next", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64], Type::Ptr, gravity_abi::host_words!(k::sort_next(ctx, tbl, mode, i)));
                b.call_host(h, &[env.ctx, tbl, mode, j], Some(Type::Ptr))
            }
            Some((i, s, _)) => {
                let h = res::host(b, "sort_next_at", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64], Type::Ptr, gravity_abi::host_words!(k::sort_next_at(ctx, tbl, mode, i, k, slot)));
                let (i, s) = (b.iconst(Type::I64, i as i64), b.iconst(Type::I64, s as i64));
                b.call_host(h, &[env.ctx, tbl, mode, j, i, s], Some(Type::Ptr))
            }
        }
        .expect("sort_next returns the row");
        let pi = b.conv(ConvOp::PtrToInt, Type::I64, p);
        let has = b.icmp(IntCond::Sgt, pi, env.zero);
        let j1 = b.int_bin(IntBinOp::Add, j, env.one);
        b.cond_br(has, (l_head, &[j1]), (l_short, &[]));
        b.switch_to(l_short);
        let err = b.icmp(IntCond::Slt, pi, env.zero);
        let uns = b.iconst(Type::I64, gravity_abi::status::unsupported(gravity_abi::status::UNSUPPORTED_SHAPE));
        let st = b.select(err, pi, uns);
        b.br(l_eof, &[st]);
        b.switch_to(l_done);
    }

    /// NEW-43: INCSTREAM's reader past the merge of its fallen-back stream's runs, the stream not ended: the merge given up,
    /// the runs dropped and the region armed again as an empty run (kernels `SortStreamRearm`); `l_eof(status)` below zero.
    pub(crate) fn rearm(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) {
        let tbl = self.shape.table(b);
        let h = res::host(b, "sort_stream_rearm", &[Type::Ptr, Type::I64, Type::Ptr], Type::I64, gravity_kernels::contract::entry2::<gravity_kernels::ops::sort::entries::SortStreamRearm> as *const () as usize);
        let at = b.iconst(Type::I64, self.at.map_or(-1, |(i, ..)| i as i64));
        let r = b.call_host(h, &[env.ctx, at, tbl], Some(Type::I64)).expect("sort_stream_rearm returns its status");
        let (bad, l_ok) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]));
        b.cond_br(bad, (l_eof, &[r]), (l_ok, &[]));
        b.switch_to(l_ok);
    }

    /// C1 part 2: an INCSTREAM cursor reader that handed on every closed row (`closed` of them) before it pulls again: the
    /// run emptied when it holds no other row (kernels sort raw.rs `SortStreamRecycle`); twice the run's room in rows, plus
    /// one when emptied (`l_eof(status)` below zero).
    pub(crate) fn recycle(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, closed: Value, l_eof: Block) -> Value {
        let tbl = self.shape.table(b);
        let h = res::host(b, "sort_stream_recycle", &[Type::Ptr, Type::I64, Type::Ptr, Type::I64], Type::I64, gravity_kernels::contract::entry3::<gravity_kernels::ops::sort::entries::SortStreamRecycle> as *const () as usize);
        let at = b.iconst(Type::I64, self.at.map_or(-1, |(i, ..)| i as i64));
        let r = b.call_host(h, &[env.ctx, at, tbl, closed], Some(Type::I64)).expect("sort_stream_recycle returns its answer");
        let (bad, l_ok) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]));
        b.cond_br(bad, (l_eof, &[r]), (l_ok, &[]));
        b.switch_to(l_ok);
        r
    }

    /// The window at `l_win` (entered with rows left): the reader filed, rows `cursor` to at most `WINDOW` on.
    pub(crate) fn window(&self, b: &mut FunctionBuilder<'_>, mode: Value, cursor: Value, total: Value, l_win: Block) -> Win {
        b.switch_to(l_win);
        let tbl = self.shape.table(b);
        self.reader.set(Some(Reader { tbl, mode }));
        let far = { let w = b.iconst(Type::I64, WINDOW); b.int_bin(IntBinOp::Add, cursor, w) };
        let end = { let past = b.icmp(IntCond::Sgt, far, total); b.select(past, total, far) }; // D463: exactly its rows
        Win { cursor, end, facts: WinConds::default() }
    }
}

impl crate::face::runs::RunReader for RunEmit {
    fn fetch(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, j: Value) -> Value {
        let r = self.reader.get().unwrap_or_else(|| bug(env.node, "a sort emit row before its window"));
        if let (Some((i, ..)), Some(c)) = (self.at, self.cosplit) {
            let h = res::host(b, "sort_cosplit_next", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::I64, Type::Ptr], Type::Ptr, gravity_abi::host_words!(se::sort_cosplit_next(ctx, tbl, mode, i, k, ctab)));
            let (i, ctab) = (b.iconst(Type::I64, i as i64), cosplit_table(b, env, c));
            return b.call_host(h, &[env.ctx, r.tbl, r.mode, j, i, ctab], Some(Type::Ptr)).expect("sort_cosplit_next returns the row");
        }
        match self.at {
            None => {
                let h = res::host(b, "sort_next", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64], Type::Ptr, gravity_abi::host_words!(k::sort_next(ctx, tbl, mode, i)));
                b.call_host(h, &[env.ctx, r.tbl, r.mode, j], Some(Type::Ptr))
            }
            Some((i, s, _)) => {
                let h = res::host(b, "sort_next_at", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64], Type::Ptr, gravity_abi::host_words!(k::sort_next_at(ctx, tbl, mode, i, k, slot)));
                let (i, s) = (b.iconst(Type::I64, i as i64), b.iconst(Type::I64, s as i64));
                b.call_host(h, &[env.ctx, r.tbl, r.mode, j, i, s], Some(Type::Ptr))
            }
        }.expect("sort_next returns the row")
    }

    fn vals<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, p: Value, carry: Carry<'e>, next: Value, j: Value) -> RowVals<'e> {
        let s = &self.shape;
        let n = s.cols.len();
        let mut row = RowVals { row: Row { cells: mcx::reserved_in(n, env.mcx) }, vals: mcx::reserved_in(n, env.mcx), cols: (0..n as u16).map(Some).collect_in(env.mcx), carry, next, at: j };
        for (c, col) in s.cols.iter().enumerate() {
            let rep = match col.form {
                // a NULL cstring constant (face/runs.rs `plan_with`): its null byte only
                Form::Word if col.tf.typlen == -2 => Rep::CstrPtr,
                Form::Word => Rep::Word { bytes: col.tf.typlen as u8 },
                Form::Image { .. } => Rep::FixedPtr { len: col.tf.typlen as u16 },
                Form::Var { .. } => Rep::VarlenaPtr,
                Form::Code { dict, .. } => Rep::Code { dict },
            };
            // valid until the next row: the merge's stash holds one; a code word renders through its producer's site
            let life = CellLife { until: Until::Row, home: CellHome::FrameSlot };
            let cell = match col.form {
                Form::Code { producer, .. } => Cell::of_record(rep, col.tf.typid, NumTypmod::NONE, LifeId(producer), c as u16, Nullable::Maybe),
                _ => Cell { rep, typid: col.tf.typid, num: NumTypmod::NONE, life, at: At::Slot(c as u16), null: Nullable::Maybe },
            };
            row.row.cells.push_in(cell);
            let v = b.load(Type::I64, p, s.datums() + 8 * c as i32);
            let nb = b.load_zext(Type::I8, p, s.nulls() + c as i32);
            let null = b.icmp(IntCond::Ne, nb, env.zero);
            row.vals.push_in((v, Some(null)));
        }
        row
    }
}

impl SourceMech for RunEmit {
    fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("sort mode", Base::Words, self.w_mode, Type::I64, 0));
        w.declare(OpenWord::open("sort cursor", Base::Words, self.w_cursor, Type::I64, 0));
        w.declare(OpenWord::open("sort total", Base::Words, self.w_total, Type::I64, 0));
    }

    /// The fresh call: `sort_begin` (it reads `RESUME_NONE` as the fresh call, so the open mark is undone around it) files
    /// the mode (0: nothing to send); a status below zero leaves. D3190: a co-split claim's cursor and total are its range
    /// of the run (a whole-run claim's, all of it).
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        let st = self.begin(b, env, false);
        if let Some(h) = &self.held {
            h.clamp(b, env, self.w_total);
        }
        if let Some(c) = self.cosplit {
            use crate::face::cosplit::{clamp, range, Side};
            let total = env.load_word(b, self.w_total);
            let (lo, hi) = range(b, env, c.h, Side::Outer);
            let (lo, hi) = (clamp(b, lo, total), clamp(b, hi, total));
            env.store_word(b, self.w_cursor, lo);
            env.store_word(b, self.w_total, hi);
            if let Some((obj, table)) = self.cut {
                cut_object(b, env, c, obj, table);
            }
        }
        st
    }

    /// D3190: a co-split outer's pipeline is wide, its driving site the outer producer's, nominal (no row is read through
    /// it: the claims are the co-split's ranges).
    fn nominal_site(&self, _px: &Px<'_, '_>) -> Option<usize> {
        self.cosplit.map(|c| c.site)
    }

    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline {
        let d = px.gx.pipelines()[k].desc;
        match self.cosplit {
            // D3336 (A1a): a K-cut fill's claims are the window's objects (exec answers them); a call a pool thread cannot
            // run keeps it on the leader, its one claim reading every row, dealt
            Some(c) if self.cut.is_some() => {
                let parallel = match crate::hir::hosted_serial(px, k) {
                    Some(reason) => gravity_program::Parallel::Ineligible { site: c.site as i16, reason },
                    None => gravity_program::Parallel::Eligible,
                };
                Pipeline { parallel, driving_site: c.site as i16, predicted_rows: c.rows, ..d }
            }
            Some(c) => Pipeline { parallel: gravity_program::Parallel::Eligible, driving_site: c.site as i16, predicted_rows: c.rows, ..d },
            None => d,
        }
    }

    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let (mode, cursor, total) = (env.load_word(b, self.w_mode), env.load_word(b, self.w_cursor), env.load_word(b, self.w_total));
        let (on, left) = (b.icmp(IntCond::Sgt, mode, env.zero), b.icmp(IntCond::Slt, cursor, total));
        let (go, l_win) = (b.int_bin(IntBinOp::And, on, left), b.create_block(&[]));
        if !self.shared {
            match &self.held {
                None => b.cond_br(go, (l_win, &[]), (l_eof, &[env.zero])),
                // D2316: past the rows PostgreSQL's batches handed on, a held fill's error (its next load's raising row)
                Some(h) => {
                    let l_end = b.create_block(&[]);
                    b.cond_br(go, (l_win, &[]), (l_end, &[]));
                    b.switch_to(l_end);
                    h.end(b, env);
                    b.br(l_eof, &[env.zero]);
                }
            }
            return self.window(b, mode, cursor, total, l_win);
        }
        // D2004: past the last window a merge this reader laid and left in flight is given up (`SortFinish`): an end that
        // takes each window whole (a count-only fold) never pulls a row, and the run's next reader met the merge in flight
        let (l_out, l_fin) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(go, (l_win, &[]), (l_out, &[]));
        b.switch_to(l_out);
        let merging = { let m = b.iconst(Type::I64, k::SORT_MODE_MERGE); b.icmp(IntCond::Eq, mode, m) };
        b.cond_br(merging, (l_fin, &[]), (l_eof, &[env.zero]));
        b.switch_to(l_fin);
        // the open's table address: the merge's reader is told by it (an emit lowered without its open makes no call)
        let tbl = match self.open_tbl.get() {
            Some(slot) => b.stack_addr(slot),
            None => b.const_raw(Type::Ptr, 0),
        };
        let h = res::host(b, "sort_finish", &[Type::Ptr, Type::I64, Type::Ptr], Type::I64, gravity_kernels::contract::entry2::<gravity_kernels::ops::sort::entries::SortFinish> as *const () as usize);
        let at = b.iconst(Type::I64, self.at.map_or(-1, |(i, ..)| i as i64));
        let r = b.call_host(h, &[env.ctx, at, tbl], Some(Type::I64)).expect("sort_finish returns its status");
        let st = { let bad = b.icmp(IntCond::Slt, r, env.zero); b.select(bad, r, env.zero) };
        b.br(l_eof, &[st]);
        self.window(b, mode, cursor, total, l_win)
    }

    /// Row `j`: `sort_next`'s row (null at the end, below zero a status), each column's datum word and null byte.
    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let p = crate::face::runs::RunReader::fetch(self, b, env, j);
        // D463: the end leaves through the carry exits (the end's accumulators stored), never `l_eof` (past the window's end)
        let pi = b.conv(ConvOp::PtrToInt, Type::I64, p);
        let [l_row, l_end, l_bad, l_eod] = [(); 4].map(|_| b.create_block(&[]));
        let (has, bad) = (b.icmp(IntCond::Sgt, pi, env.zero), b.icmp(IntCond::Slt, pi, env.zero));
        b.cond_br(has, (l_row, &[]), (l_end, &[]));
        b.switch_to(l_end);
        b.cond_br(bad, (l_bad, &[]), (l_eod, &[]));
        b.switch_to(l_bad);
        env.stop(b, &carry, j, pi);
        b.switch_to(l_eod);
        env.full(b, &carry, j);
        b.switch_to(l_row);
        crate::face::runs::RunReader::vals(self, b, env, p, carry, next, j)
    }

    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, _out: Value) {
        env.store_word(b, self.w_cursor, cur);
    }
}
