//! Frame emission (lowerv4 plan §2.1, P8; KWC lowerv3 core_emit): the cancel check and its back-edge lint, per-open words
//! (open and union scope), one pipeline through windows under the interpreter with its EXPLAIN ANALYZE cells, the 64-row
//! block path (`env.masked` for the first step only), and a stop inside a row resumed at its offset word. D231's cancel
//! between two pipelines' emits is in core_px (the program).
use std::cell::RefCell;

use gravity_abi::ctx::{ctx_size, hdr, site_off, RESUME_NONE};
use gravity_abi::{program::prog, site::site, status};
use gravity_interp::Interp;
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, Func, IntBinOp, IntCond, Program, Type, Value};
use gravity_lowerv4::face::{end::EndMech, source::SourceMech, step::StepMech};
use gravity_lowerv4::frame::frame::{self, Mask, Mechs, Next, Win, WordWin};
use gravity_lowerv4::frame::instr::{self, Ctr, InstrCell};
use gravity_lowerv4::frame::open::{Base, OpenWord, OpenWords};
use gravity_lowerv4::frame::row::{Carry, RowVals};
use gravity_lowerv4::frame::{cancel, env, env::Env, stop::Exits};
use gravity_lowerv4::{catch, hir::Ir, Reason};

const I64: Type = Type::I64;

fn run(p: &Program, f: Func, args: &[u64]) -> i64 {
    Interp::new(p).run(f, args).expect("runs")[0] as i64
}

/// `f(ctx)`: a loop with no exit but cancel; `checked`: its header checks the cancel byte (`cancel::header`).
fn spin(checked: bool) -> (Program, Func, Block) {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("spin", &[Type::Ptr], &[I64]);
    let mut b = pb.define(f);
    let (ctx, l_cancel) = (b.params()[0], b.create_block(&[]));
    let (h, body) = if checked { let m = mcx::MemoryContext::new("test"); cancel::header(&mut b, ctx, l_cancel, &[], m.mcx()) } else { let h = b.create_block(&[]); (h, h) };
    b.br(h, &[]);
    b.switch_to(body);
    b.br(h, &[]);
    b.switch_to(l_cancel);
    let c = b.iconst(I64, status::CANCELLED);
    b.ret(&[c]);
    (pb.finish(), f, h)
}

#[test]
fn the_cancel_check_and_its_lint() {
    let (p, f, _) = spin(true);
    let (mut ctx, byte) = (vec![0u64; ctx_size(1) / 8], Box::new(1u8));
    ctx[hdr::CANCEL / 8] = &*byte as *const u8 as u64;
    assert_eq!(run(&p, f, &[ctx.as_mut_ptr() as u64]), status::CANCELLED, "a set cancel byte leaves the loop");
    let rig = mcx::MemoryContext::new("test");
    assert_eq!(cancel::verify(p.func(f), &[], rig.mcx()), Ok(()));
    let (p, f, h) = spin(false);
    assert_eq!(cancel::verify(p.func(f), &[], rig.mcx()), Err(h), "an unchecked back edge is found");
    assert_eq!(cancel::verify(p.func(f), &[h], rig.mcx()), Ok(()), "a bounded row loop is accepted");
}

#[test]
fn open_words_are_written_at_the_open_before_any_read() {
    let rig = mcx::MemoryContext::new("test");
    let mut w = OpenWords::new_in(rig.mcx());
    [(0, 0i64), (1, -1), (2, 7)].into_iter().for_each(|(i, init)| w.declare(OpenWord::open("w", Base::Ctx, (hdr::SIZE + i * 8) as i32, I64, init)));
    w.declare(OpenWord::union("u", 8, I64, 9, 0)); // frame word 1, under the union flag in frame word 0
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("open", &[Type::Ptr, Type::Ptr], &[I64]);
    let mut b = pb.define(f);
    let (ctx, words) = (b.params()[0], b.params()[1]);
    w.write_all(&mut b, 1, ctx, None, Some(words));
    let z = b.iconst(I64, 0);
    b.ret(&[z]);
    let p = pb.finish();
    let (mut ctx, mut words) = (vec![0x7E7E_7E7E_7E7E_7E7Eu64; ctx_size(1) / 8], [0u64; 2]);
    for (arm, left) in [(0, 9), (1, 5)] {
        if arm == 1 {
            (words[1], ctx[hdr::SIZE / 8]) = (5, 3);
        }
        run(&p, f, &[ctx.as_mut_ptr() as u64, words.as_mut_ptr() as u64]);
        assert_eq!(ctx[hdr::SIZE / 8..hdr::SIZE / 8 + 3], [0, u64::MAX, 7], "every open writes its words");
        assert_eq!(words, [1, left], "a union's words are written by the first arm to open, never again");
    }
    let r = catch(1, || {
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("state", &[Type::Ptr], &[I64]);
        let mut b = pb.define(f);
        let ctx = b.params()[0];
        let m = mcx::MemoryContext::new("test");
        let mut w = OpenWords::new_in(m.mcx());
        w.declare(OpenWord::open("s", Base::State, 0, I64, 0));
        w.write_all(&mut b, 1, ctx, None, None);
    });
    assert!(matches!(r.map_err(|r| r.reason), Err(Reason::Bug { .. })), "a state word without a state slot is a Bug");
}

/// Windows `[cursor, n)` read from site 0's cursor (a per-open word, 0 at the open) until it reaches `n`, then status -5
/// (the call returns without the `done` host). `.1`: the window has 64-row block values.
struct Src(i64, bool);

impl SourceMech for Src {
    fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("cursor", Base::Ctx, (site_off(0) + site::CURSOR) as i32, Type::I32, 0));
    }
    fn open(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Value {
        b.iconst(I64, 0)
    }
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let cursor = b.load_zext(Type::I32, env.ctx, (site_off(0) + site::CURSOR) as i32);
        let end = b.iconst(I64, self.0);
        let past = b.icmp(IntCond::Sge, cursor, end);
        let (l_end, l_win) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(past, (l_end, &[]), (l_win, &[]));
        b.switch_to(l_end);
        let st = b.iconst(I64, -5);
        b.br(l_eof, &[st]);
        b.switch_to(l_win);
        Win { cursor, end, facts: Default::default() }
    }
    fn word_win<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win) -> Option<WordWin<'e>> {
        self.1.then(|| WordWin { lanes: mcx::PgVec::new_in(env.mcx), valid: mcx::PgVec::new_in(env.mcx), plain: env.one })
    }
    fn row<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        RowVals { row: gravity_lowerv4::cell::Row::new_in(env.mcx), vals: mcx::PgVec::new_in(env.mcx), cols: mcx::PgVec::new_in(env.mcx), carry, next, at: j }
    }
}

/// Counts rows into `out` and sums their indexes into one accumulator, stored to `ctx.budget_bytes` at each window's end.
struct SumEnd;

impl EndMech for SumEnd {
    fn carried<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>) -> mcx::PgVec<'e, Value> {
        mcx::pg_vec![in env.mcx; b.iconst(I64, 0)]
    }
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        Carry { out: b.int_bin(IntBinOp::Add, row.carry.out, env.one), accs: mcx::pg_vec![in env.mcx; b.int_bin(IntBinOp::Add, row.carry.accs[0], row.at)] }
    }
    fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, c: &Carry) {
        b.store(env.ctx, hdr::BUDGET_BYTES as i32, c.accs[0]);
    }
}

/// `f` under `cond`, the carry unchanged otherwise (every carry value an `I64`).
fn when<'e>(b: &mut FunctionBuilder<'_>, cond: Value, c: &Carry, f: impl FnOnce(&mut FunctionBuilder<'_>) -> Carry<'e>) -> Carry<'e> {
    let (l_do, l_join) = (b.create_block(&[]), b.create_block(&vec![I64; c.params().len()]));
    b.cond_br(cond, (l_do, &[]), (l_join, &c.params()));
    b.switch_to(l_do);
    let d = f(b);
    b.br(l_join, &d.params());
    b.switch_to(l_join);
    Carry::from_params(b.block_params(l_join), *d.accs.allocator())
}

/// Records `(env.step, env.masked)` at each lowering. `.1` (a first step): rows with an even index, tested per row; its block
/// form passes only bit 1 of each block (odd rows), so a masked row that ran the per-row test anyway would be lost.
struct Rec<'a>(&'a RefCell<Vec<(Option<usize>, bool)>>, bool);

impl StepMech for Rec<'_> {
    fn block_open(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env, _w: &WordWin) -> Option<Mask> {
        self.1.then_some(Mask(env.one))
    }
    fn block(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env, _w: &WordWin, _base: Value, _m: Mask) -> Mask {
        Mask(b.iconst(I64, 2))
    }
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        self.0.borrow_mut().push((env.step, env.masked));
        if env.masked || !self.1 {
            return next(b, env, row);
        }
        let odd = b.int_bin(IntBinOp::And, row.at, env.one);
        let even = b.icmp(IntCond::Eq, odd, env.zero);
        when(b, even, &row.carry, |b| next(b, env, row))
    }
}

/// An expanding step: two outputs per row, from output `off` (frame word 0, per-open, 0) on. It stops inside row 1 after its
/// first output, the offset stored and the cursor at the row (`RowVals::at`); the resumed call enters row 1 at offset 1.
struct Twice;

impl StepMech for Twice {
    fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("expansion offset", Base::Words, 0, I64, 0));
    }
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let off = env.load_word(b, 0);
        let first = b.icmp(IntCond::Eq, off, env.zero);
        let c0 = when(b, first, &row.carry, |b| next(b, env, row));
        let at_row1 = b.int_bin(IntBinOp::Xor, row.at, env.one);
        let any = b.int_bin(IntBinOp::Or, at_row1, off);
        let stop = b.icmp(IntCond::Eq, any, env.zero);
        let (l_stop, l_go) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(stop, (l_stop, &[]), (l_go, &[]));
        b.switch_to(l_stop);
        env.store_word(b, 0, env.one);
        let st = b.iconst(I64, -7);
        b.br(env.x.l_stop_in, &[c0.out, row.at, st]);
        b.switch_to(l_go);
        let c1 = next(b, env, &RowVals { carry: c0, ..clone_row(row) });
        env.store_word(b, 0, env.zero);
        c1
    }
}

fn clone_row<'r>(r: &RowVals<'r>) -> RowVals<'r> {
    RowVals { row: r.row.clone(), vals: r.vals.clone(), cols: r.cols.clone(), carry: r.carry.copy_in(*r.vals.allocator()), next: r.next, at: r.at }
}

fn mechs<'g>(mcx: mcx::Mcx<'g>, source: Src, steps: mcx::PgVec<'g, mcx::PgBox<'g, dyn StepMech + 'g>>, words: Option<usize>) -> Mechs<'g> {
    let desc = gravity_program::Pipeline::eligible(0, gravity_program::End::Sink, 0);
    Mechs { k: 0, source: mcx::pg_box_dyn!(mcx, source => dyn SourceMech + 'g), steps, end: mcx::pg_box_dyn!(mcx, SumEnd => dyn EndMech + 'g), site: Some(0), desc, words, state_word: None }
}

/// `m` as `p0(ctx)`, verified (IR and cancel back edges), with the counter cells an instrumented lowering allocated.
fn build(mut m: Mechs<'_>, instrument: bool) -> (Program, Func, Vec<InstrCell>) {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    let mut pb = ProgramBuilder::new();
    let core = env::declare(&mut pb);
    let ((f, bounded), cells) = instr::scope(instrument, || frame::pipeline_fn(&mut pb, core, Ir::none(), &[], "p0", 1, &mut m, rig.mcx()));
    let p = pb.finish();
    gravity_ir::verify::verify(&p).expect("the frame's IR verifies");
    assert_eq!(cancel::verify(p.func(f), &bounded, rig.mcx()), Ok(()), "every back edge checked or bounded");
    (p, f, cells)
}

#[test]
fn one_pipeline_through_the_frame() {
    let rig = mcx::MemoryContext::new("test");
    let (p, f, cells) = build(mechs(rig.mcx(), Src(3, false), mcx::PgVec::new_in(rig.mcx()), None), true);
    assert_eq!(cells, [InstrCell { node: 1, ctr: Ctr::Loops }, InstrCell { node: 1, ctr: Ctr::Rows }]);
    let (mut ctx, mut block, mut counters) = (vec![0u64; ctx_size(1) / 8], [0u64; 16], [0u64; 2]);
    block[prog::INSTR / 8] = counters.as_mut_ptr() as u64;
    ctx[hdr::PROGRAM / 8] = block.as_mut_ptr() as u64;
    ctx[(site_off(0) + site::CURSOR) / 8] = 2; // garbage the open overwrites
    assert_eq!(run(&p, f, &[ctx.as_mut_ptr() as u64]), -5);
    assert_eq!((ctx[(site_off(0) + site::ROWS_OUT) / 8], ctx[(site_off(0) + site::CURSOR) / 8] as u32), (3, 3), "written back at the window's end");
    assert_eq!((ctx[hdr::BUDGET_BYTES / 8], counters), (3, [1, 3]), "rows 0+1+2; one open, three rows (EXPLAIN ANALYZE cells)");
    let (mut ctx, byte) = (vec![0u64; ctx_size(1) / 8], Box::new(1u8));
    (ctx[hdr::CANCEL / 8], ctx[hdr::PROGRAM / 8]) = (&*byte as *const u8 as u64, block.as_mut_ptr() as u64);
    assert_eq!(run(&p, f, &[ctx.as_mut_ptr() as u64]), status::CANCELLED, "the window loop's check exits before the first window");
    assert_eq!(counters, [2, 3], "opened, then cancelled before a row");
    // D2881: a later claim of the pipeline's run opens it again but counts no loop; its rows count
    let mut ctx = vec![0u64; ctx_size(1) / 8];
    (ctx[hdr::PROGRAM / 8], ctx[hdr::FLAGS / 8]) = (block.as_mut_ptr() as u64, gravity_abi::ctx::FLAG_LATER_CLAIM as u64);
    assert_eq!(run(&p, f, &[ctx.as_mut_ptr() as u64]), -5);
    assert_eq!(counters, [2, 6], "a later claim: no loop, three more rows");
}

#[test]
fn whole_blocks_run_by_mask_and_the_tail_by_rows() {
    let (a, b) = (RefCell::new(Vec::new()), RefCell::new(Vec::new()));
    let rig = mcx::MemoryContext::new("test");
    let c = rig.mcx();
    let steps = mcx::pg_vec![in c; mcx::pg_box_dyn!(c, Rec(&a, true) => dyn StepMech + '_), mcx::pg_box_dyn!(c, Rec(&b, false) => dyn StepMech + '_)];
    let (p, f, cells) = build(mechs(c, Src(130, true), steps, None), false);
    assert!(cells.is_empty(), "an uninstrumented lowering allocates no cell");
    let mut ctx = vec![0u64; ctx_size(1) / 8];
    assert_eq!(run(&p, f, &[ctx.as_mut_ptr() as u64]), -5);
    // blocks [0, 64) and [64, 128) pass rows 1 and 65 by mask, untested; the tail [128, 130) tests each row: 128
    assert_eq!((ctx[(site_off(0) + site::ROWS_OUT) / 8], ctx[hdr::BUDGET_BYTES / 8]), (3, 1 + 65 + 128));
    let (a, b) = (a.into_inner(), b.into_inner());
    assert!(a.contains(&(Some(0), true)) && a.contains(&(Some(0), false)), "step 0 lowered in both bodies: {a:?}");
    assert!(!b.is_empty() && b.iter().all(|&s| s == (Some(1), false)), "env.masked holds only for the first step: {b:?}");
}

#[test]
fn a_stop_inside_a_row_resumes_at_its_offset() {
    let rig = mcx::MemoryContext::new("test");
    let (p, f, _) = build(mechs(rig.mcx(), Src(3, false), mcx::pg_vec![in rig.mcx(); mcx::pg_box_dyn!(rig.mcx(), Twice => dyn StepMech)], Some(1)), false);
    let (mut ctx, mut words) = (vec![0u64; ctx_size(2) / 8], [7u64; 2]);
    ctx[(site_off(1) + site::KEYS) / 8] = words.as_mut_ptr() as u64;
    let cell = |ctx: &[u64], off: usize| ctx[(site_off(0) + off) / 8];
    assert_eq!(run(&p, f, &[ctx.as_mut_ptr() as u64]), -7, "the stop's status");
    assert_eq!((cell(&ctx, site::ROWS_OUT), cell(&ctx, site::CURSOR) as u32, words[0]), (3, 1, 1), "rows 0, 0, 1 out; cursor at row 1; offset 1");
    assert_ne!(ctx[hdr::RESUME / 8] as i64, RESUME_NONE, "the open marked the call opened");
    assert_eq!(run(&p, f, &[ctx.as_mut_ptr() as u64]), -5, "the resumed call skips the open and re-enters row 1");
    assert_eq!((cell(&ctx, site::ROWS_OUT), cell(&ctx, site::CURSOR) as u32, words[0]), (6, 3, 0), "row 1's second output, row 2's two");
}
