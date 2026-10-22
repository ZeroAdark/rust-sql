//! M1-FRAME's frame tests under the interpreter (lowerv4 frame/frame.rs row_loop / block_loop): decode.late's fills
//! (`SourceMech::fill_late`, H01e) reach every lane a step or fold reads, and no qual runs early.
use gravity_abi::ctx::{ctx_size, site_off};
use gravity_abi::site::site;
use gravity_interp::Interp;
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, IntBinOp, IntCond, Type, Value};
use gravity_lowerv4::face::{end::EndMech, source::SourceMech, step::StepMech};
use gravity_lowerv4::frame::frame::{self, Mask, Mechs, Next, Win, WordWin};
use gravity_lowerv4::frame::open::{Base, OpenWord, OpenWords};
use gravity_lowerv4::frame::row::{Carry, RowVals};
use gravity_lowerv4::frame::{cancel, env, env::Env, stop::Exits};
use gravity_lowerv4::hir::Ir;

const I64: Type = Type::I64;

/// Word `LATE + r` of the test context is the late lane's row r: r + 1 once filled, 0 (unfilled) before.
const LATE: usize = ctx_size(1) / 8;
const LATE_WORDS: usize = 3 * 70 + 64;

/// Row r's late word.
fn late_at(b: &mut FunctionBuilder<'_>, env: &Env, r: Value) -> Value {
    b.ptr_add(env.ctx, r, 8, (LATE * 8) as i32)
}

/// Whether bit `k` of mask `m` is set.
fn bit_on(b: &mut FunctionBuilder<'_>, env: &Env, m: Value, k: Value) -> Value {
    let bit = b.int_bin(IntBinOp::Ushr, m, k);
    let bit = b.int_bin(IntBinOp::And, bit, env.one);
    b.icmp(IntCond::Ne, bit, env.zero)
}

/// Windows of 70 rows from site 0's cursor up to 210 (a whole block and a 6-row tail each), then status -5; block values
/// when `.0`. One late lane, which `fill_late` fills when `.1`: a block's survivors (`Some`) or the whole window (`None`).
struct Late(bool, bool);

impl SourceMech for Late {
    fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("cursor", Base::Ctx, (site_off(0) + site::CURSOR) as i32, Type::I32, 0));
    }
    fn open(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Value {
        b.iconst(I64, 0)
    }
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let cursor = b.load_zext(Type::I32, env.ctx, (site_off(0) + site::CURSOR) as i32);
        let n = b.iconst(I64, 210);
        let past = b.icmp(IntCond::Sge, cursor, n);
        let (l_end, l_win) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(past, (l_end, &[]), (l_win, &[]));
        b.switch_to(l_end);
        let st = b.iconst(I64, -5);
        b.br(l_eof, &[st]);
        b.switch_to(l_win);
        let w = b.iconst(I64, 70);
        let end = b.int_bin(IntBinOp::Add, cursor, w);
        Win { cursor, end, facts: Default::default() }
    }
    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let p = late_at(b, env, j);
        let v = b.load(I64, p, 0);
        RowVals { row: gravity_lowerv4::cell::Row::new_in(env.mcx), vals: mcx::pg_vec![in env.mcx; (v, None)], cols: mcx::PgVec::new_in(env.mcx), carry, next, at: j }
    }
    fn word_win<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win) -> Option<WordWin<'e>> {
        self.0.then(|| WordWin { lanes: mcx::PgVec::new_in(env.mcx), valid: mcx::PgVec::new_in(env.mcx), plain: env.one })
    }
    fn fill_late(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, win: &Win, at: Option<(Value, Mask)>) {
        let (base, m, n) = match at {
            Some((base, m)) => (base, Some(m.0), 64),
            None => (win.cursor, None, 70),
        };
        for i in (0..n).filter(|_| self.1) {
            let k = b.iconst(I64, i);
            let r = b.int_bin(IntBinOp::Add, base, k);
            let (p, v) = (late_at(b, env, r), b.int_bin(IntBinOp::Add, r, env.one));
            let v = match m {
                Some(m) => {
                    let (on, old) = (bit_on(b, env, m, k), b.load(I64, p, 0));
                    b.select(on, v, old)
                }
                None => v,
            };
            b.store(p, 0, v);
        }
    }
}

/// A filter passing even rows; with a block form (`.0`) its block mask is the even rows of the block.
struct Even(bool);

impl StepMech for Even {
    fn block_open(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env, _w: &WordWin) -> Option<Mask> {
        self.0.then_some(Mask(env.one))
    }
    fn block(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env, _w: &WordWin, _base: Value, m: Mask) -> Mask {
        let even = b.iconst(I64, 0x5555_5555_5555_5555);
        Mask(b.int_bin(IntBinOp::And, m.0, even))
    }
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        if env.masked {
            return next(b, env, row);
        }
        let odd = b.int_bin(IntBinOp::And, row.at, env.one);
        let even = b.icmp(IntCond::Eq, odd, env.zero);
        let (l_yes, l_join) = (b.create_block(&[]), b.create_block(&[I64]));
        b.cond_br(even, (l_yes, &[]), (l_join, &[row.carry.out]));
        b.switch_to(l_yes);
        let c = next(b, env, row);
        b.br(l_join, &[c.out]);
        b.switch_to(l_join);
        Carry::new(b.block_params(l_join)[0], env.mcx)
    }
}

/// A qual that raises on a row whose late value is still 0 (unfilled), as one dividing by it would.
struct Checked;

impl StepMech for Checked {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let zero = b.icmp(IntCond::Eq, row.vals[0].0, env.zero);
        let (l_bad, l_ok) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(zero, (l_bad, &[]), (l_ok, &[]));
        b.switch_to(l_bad);
        let e = b.iconst(I64, 22012);
        b.br(env.l_raise, &[e]);
        b.switch_to(l_ok);
        next(b, env, row)
    }
}

/// Sums the late values of the rows that reach it; with `.0`, also folds whole blocks from the late lane (mask.fold).
struct SumLate(bool);

impl EndMech for SumLate {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        Carry::new(b.int_bin(IntBinOp::Add, row.carry.out, row.vals[0].0), env.mcx)
    }
    fn block<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _w: &WordWin, base: Value, m: Mask, c: &Carry) -> Option<Carry<'e>> {
        if !self.0 {
            return None;
        }
        let mut out = c.out;
        for i in 0..64 {
            let k = b.iconst(I64, i);
            let r = b.int_bin(IntBinOp::Add, base, k);
            let p = late_at(b, env, r);
            let (v, on) = (b.load(I64, p, 0), bit_on(b, env, m.0, k));
            let v = b.select(on, v, env.zero);
            out = b.int_bin(IntBinOp::Add, out, v);
        }
        Some(Carry::new(out, env.mcx))
    }
}

/// The pipeline `source → steps → end` over a fresh context with the late lane after it: its status and the context.
fn run<'g>(c: mcx::Mcx<'g>, source: Late, steps: mcx::PgVec<'g, mcx::PgBox<'g, dyn StepMech>>, end: SumLate) -> (i64, Vec<u64>) {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    let desc = gravity_program::Pipeline::eligible(0, gravity_program::End::Sink, 0);
    let mut m = Mechs { k: 0, source: mcx::pg_box_dyn!(c, source => dyn SourceMech), steps, end: mcx::pg_box_dyn!(c, end => dyn EndMech), site: Some(0), desc, words: None, state_word: None };
    let mut pb = ProgramBuilder::new();
    let core = env::declare(&mut pb);
    let (f, bounded) = frame::pipeline_fn(&mut pb, core, Ir::none(), &[], "p0", 1, &mut m, rig.mcx());
    let p = pb.finish();
    gravity_ir::verify::verify(&p).expect("the frame's IR verifies");
    assert_eq!(cancel::verify(p.func(f), &bounded, rig.mcx()), Ok(()));
    let mut ctx = vec![0u64; LATE + LATE_WORDS];
    let st = Interp::new(&p).run(f, &[ctx.as_mut_ptr() as u64]).expect("runs")[0] as i64;
    (st, ctx)
}

#[test]
fn late_lanes_are_filled_for_survivors_before_any_step_or_fold_reads_them() {
    let out = |ctx: &[u64]| ctx[(site_off(0) + site::ROWS_OUT) / 8];
    let even_sum = 105 * 105; // r + 1 over the even r < 210
    let rig = mcx::MemoryContext::new("test");
    let c = rig.mcx();
    // row bodies: the whole window filled at the body's entry; the raising qual sees only filled rows
    let (st, ctx) = run(c, Late(false, true), mcx::pg_vec![in c; mcx::pg_box_dyn!(c, Even(false) => dyn StepMech), mcx::pg_box_dyn!(c, Checked => dyn StepMech)], SumLate(false));
    assert_eq!((st, out(&ctx)), (-5, even_sum));
    // block path: a block fills its survivors only (the odd rows of whole blocks stay unfilled, and the raising qual after
    // the first never sees them: no qual runs early); a tail fills every tail row
    let (st, ctx) = run(c, Late(true, true), mcx::pg_vec![in c; mcx::pg_box_dyn!(c, Even(true) => dyn StepMech), mcx::pg_box_dyn!(c, Checked => dyn StepMech)], SumLate(false));
    assert_eq!((st, out(&ctx)), (-5, even_sum));
    assert_eq!((ctx[LATE + 1], ctx[LATE + 70], ctx[LATE + 71], ctx[LATE + 65], ctx[LATE + 209]), (0, 71, 0, 66, 210));
    // mask.fold: the end folds each whole block from the late lane after the block's fill
    let (st, ctx) = run(c, Late(true, true), mcx::pg_vec![in c; mcx::pg_box_dyn!(c, Even(true) => dyn StepMech)], SumLate(true));
    assert_eq!((st, out(&ctx)), (-5, even_sum));
    // without the fill the raising qual does raise, so the checks above are not vacuous
    let (st, _) = run(c, Late(true, false), mcx::pg_vec![in c; mcx::pg_box_dyn!(c, Even(true) => dyn StepMech), mcx::pg_box_dyn!(c, Checked => dyn StepMech)], SumLate(false));
    assert_ne!(st, -5);
}
