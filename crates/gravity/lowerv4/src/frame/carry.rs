//! Road `frame.carry` (H17, R8-L03 R3-L13): an end's accumulators (a fold's states, a keyed end's open run) stay in
//! registers for a window. The end loads them at the window's start (`EndMech::carried`), each row threads them through
//! the row loop's carry, and the window's end stores them once (`window_end`), instead of a load, update and store per row.
//! An exit that leaves the window early must store them too, or a resumed call would reload stale states and a finished
//! call would fold without the window's rows. The window's stop and count-reached exits (`CarryExits`) therefore store
//! before leaving. The frame installs them per window (frame.rs `row_loop`) and writes their bodies after the window's
//! bodies (D326), and a step that leaves mid-row passes its row's carry through `Env::stop` / `Env::full`. A raise or
//! cancel ends the statement, so their exits store nothing. Budget 160 lines.
use gravity_abi::status::{unsupported, UNSUPPORTED_SHAPE};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type};
use mcx::{CollectIn, Mcx};

use crate::face::end::EndMech;
use crate::face::step::{CarryExits, CarryImpl, CarryMech};
use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;

crate::strategies! {
    /// Fold accumulators and open run in registers per window.
    FRAME_CARRY = 8, "frame.carry", Fold;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(FRAME_CARRY, &CarryImpl(&STORE))], ..crate::ops::NONE };

struct Store;
static STORE: Store = Store;

impl CarryMech for Store {
    fn exits(&self, b: &mut FunctionBuilder<'_>, accs: &[Type], mcx: Mcx<'_>) -> CarryExits {
        let head = |n: usize| core::iter::repeat_n(Type::I64, n).chain(accs.iter().copied()).collect_in(mcx);
        CarryExits { l_stop: b.create_block(&head(3)[..]), l_full: b.create_block(&head(2)[..]) }
    }

    /// The bodies once the window's bodies are lowered (D326): a `window_end` written before its end's `lower` stored the
    /// build's default record shape (no words, 0-byte records) at a met LIMIT, so the build kept one zeroed record. The
    /// bodies close their blocks; the frame moves on to the window's end (no current block is left to return to).
    fn seal(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, end: &dyn EndMech, CarryExits { l_stop, l_full }: CarryExits, x: &Exits) {
        // D528: an exit no row leaves through is not emitted
        if crate::frame::frame::reached(b, l_stop) {
            b.switch_to(l_stop);
            let p = mcx::to_vec_in(&b.block_params(l_stop)[..], env.mcx);
            end.window_end(b, env, &Carry { out: p[0], accs: mcx::to_vec_in(&p[3..], env.mcx) });
            b.br(x.l_stop_in, &p[..3]);
        }
        if crate::frame::frame::reached(b, l_full) {
            b.switch_to(l_full);
            let p = mcx::to_vec_in(&b.block_params(l_full)[..], env.mcx);
            end.window_end(b, env, &Carry { out: p[0], accs: mcx::to_vec_in(&p[2..], env.mcx) });
            b.br(x.l_full, &p[..2]);
        }
    }
}

/// `Env::full`, for the steps that call it here.
pub(crate) fn full(b: &mut FunctionBuilder<'_>, env: &Env, c: &Carry, cur: gravity_ir::Value) {
    env.full(b, c, cur)
}

/// `next` for a row a step lets through, `count` storing the step's state for the row (a LIMIT's count) once the row is
/// through. A stop inside `next` leaves through `l_seen` (`env.x.l_stop_in` while `next` lowers): a cursor past the row
/// (`row.next`: the row stored, the sink's stop) stores it there, since `next` does not return; a cursor at the row
/// (`row.at`: an expanding step's stop, H32) stores nothing, as the resume re-enters the row and stores it when the row is
/// through. A cursor that is neither leaves with UNSUPPORTED_SHAPE, never a miscount (D357).
pub(crate) fn through<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>, count: &dyn Fn(&mut FunctionBuilder<'_>)) -> Carry<'e> {
    let l_seen = b.create_block(&[Type::I64, Type::I64, Type::I64]);
    b.mark_cold(l_seen);
    let stop_in = core::mem::replace(&mut env.x.l_stop_in, l_seen);
    let c = next(b, env, row);
    env.x.l_stop_in = stop_in;
    let here = b.current();
    b.switch_to(l_seen);
    let p = mcx::to_vec_in(&b.block_params(l_seen)[..], env.mcx);
    let past = b.icmp(IntCond::Eq, p[1], row.next);
    let at = b.icmp(IntCond::Eq, p[1], row.at);
    let known = b.int_bin(IntBinOp::Or, past, at);
    let (l_known, l_bad, l_count, l_on) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
    b.cond_br(known, (l_known, &[]), (l_bad, &[]));
    b.switch_to(l_bad);
    let uns = b.iconst(Type::I64, unsupported(UNSUPPORTED_SHAPE));
    b.br(env.x.l_exit, &[uns]);
    b.switch_to(l_known);
    b.cond_br(past, (l_count, &[]), (l_on, &[]));
    b.switch_to(l_count);
    count(b);
    b.br(l_on, &[]);
    b.switch_to(l_on);
    b.br(stop_in, &p[..]);
    b.switch_to(here);
    c
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use gravity_abi::ctx::{ctx_size, hdr, site_off};
    use gravity_abi::site::site;
    use gravity_abi::status;
    use gravity_interp::Interp;
    use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
    use gravity_ir::{Block, Func, IntBinOp, IntCond, Program, Type, Value};

    use crate::face::end::EndMech;
    use crate::face::source::SourceMech;
    use crate::face::step::StepMech;
    use crate::frame::env;
    use crate::frame::env::Env;
    use crate::frame::frame::{Mechs, Next, Win};
    use crate::frame::open::{Base, OpenWord, OpenWords};
    use crate::frame::row::{Carry, RowVals};
    use crate::frame::stop::Exits;

    const I64: Type = Type::I64;

    /// One window `[cursor, n)` from site 0's cursor (a per-open word), then status -5 at its end.
    struct Src(i64);

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
        fn row<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
            RowVals { row: crate::cell::Row::new_in(env.mcx), vals: mcx::PgVec::new_in(env.mcx), cols: mcx::PgVec::new_in(env.mcx), carry, next, at: j }
        }
    }

    /// Sums row indexes in a register accumulator: loaded from `ctx.budget_bytes` at a window's start, stored at its end.
    /// Its `window_end` reads a shape its `lower` decides (`.0`: the scale, 0 until lowered), as the hash build's record
    /// shape (D326), so an exit whose store is written before the window's bodies stores 0.
    struct Sum(Cell<i64>);

    impl EndMech for Sum {
        fn carried<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>) -> mcx::PgVec<'e, Value> {
            mcx::pg_vec![in env.mcx; b.load(I64, env.ctx, hdr::BUDGET_BYTES as i32)]
        }
        fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
            self.0.set(1);
            Carry { out: b.int_bin(IntBinOp::Add, row.carry.out, env.one), accs: mcx::pg_vec![in env.mcx; b.int_bin(IntBinOp::Add, row.carry.accs[0], row.at)] }
        }
        fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, c: &Carry) {
            let scale = b.iconst(I64, self.0.get());
            let v = b.int_bin(IntBinOp::Mul, c.accs[0], scale);
            b.store(env.ctx, hdr::BUDGET_BYTES as i32, v);
        }
    }

    /// Leaves at row `.0` before it reaches the end: the count reached (`Env::full`, a LIMIT met) or, once, a stop with
    /// status -7 (`Env::stop`; frame word 0 marks it, so the resumed call passes the row).
    struct Cut(i64, bool);

    impl StepMech for Cut {
        fn open_words(&self, w: &mut OpenWords) {
            if !self.1 {
                w.declare(OpenWord::open("stopped once", Base::Words, 0, I64, 0));
            }
        }
        fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
            let k = b.iconst(I64, self.0);
            let mut cut = b.icmp(IntCond::Eq, row.at, k);
            if !self.1 {
                let once = env.load_word(b, 0);
                let fresh = b.icmp(IntCond::Eq, once, env.zero);
                cut = b.int_bin(IntBinOp::And, cut, fresh);
            }
            let (l_cut, l_go) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(cut, (l_cut, &[]), (l_go, &[]));
            b.switch_to(l_cut);
            match self.1 {
                true => env.full(b, &row.carry, row.at),
                false => {
                    env.store_word(b, 0, env.one);
                    let st = b.iconst(I64, -7);
                    env.stop(b, &row.carry, row.at, st);
                }
            }
            b.switch_to(l_go);
            next(b, env, row)
        }
    }

    fn build(cut: Cut, n: i64) -> (Program, Func) {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let words = (!cut.1).then_some(1);
        let desc = gravity_program::Pipeline::eligible(0, gravity_program::End::Sink, 0);
        let c = rig.mcx();
        let mut m = Mechs { k: 0, source: mcx::pg_box_dyn!(c, Src(n) => dyn SourceMech), steps: mcx::pg_vec![in c; mcx::pg_box_dyn!(c, cut => dyn StepMech)], end: mcx::pg_box_dyn!(c, Sum(Cell::new(0)) => dyn EndMech), site: Some(0), desc, words, state_word: None };
        let mut pb = ProgramBuilder::new();
        let core = env::declare(&mut pb);
        let (f, bounded) = crate::frame::frame::pipeline_fn(&mut pb, core, crate::hir::Ir::none(), &[], "p0", 1, &mut m, rig.mcx());
        let p = pb.finish();
        gravity_ir::verify::verify(&p).expect("the frame's IR verifies");
        assert_eq!(crate::frame::cancel::verify(p.func(f), &bounded, rig.mcx()), Ok(()));
        (p, f)
    }

    fn run(p: &Program, f: Func, ctx: &mut [u64]) -> i64 {
        Interp::new(p).run(f, &[ctx.as_mut_ptr() as u64]).expect("runs")[0] as i64
    }

    fn cell(ctx: &[u64], off: usize) -> u64 {
        ctx[(site_off(0) + off) / 8]
    }

    #[test]
    fn a_met_limit_stores_the_window_carry_before_done() {
        let (p, f) = build(Cut(4, true), 10);
        let (mut ctx, mut out) = (vec![0u64; ctx_size(1) / 8], [0u64; 32]);
        ctx[hdr::OUT / 8] = out.as_mut_ptr() as u64;
        assert_eq!(run(&p, f, &mut ctx), status::DONE);
        assert_eq!((cell(&ctx, site::ROWS_OUT), ctx[hdr::BUDGET_BYTES / 8]), (4, 6), "rows 0..3 folded: 0+1+2+3 stored at the cut, in the lowered shape (D326)");
    }

    #[test]
    fn a_stop_mid_window_stores_the_carry_and_the_resume_reloads_it() {
        let (p, f) = build(Cut(3, false), 6);
        let (mut ctx, mut words) = (vec![0u64; ctx_size(2) / 8], [0u64; 2]);
        ctx[(site_off(1) + site::KEYS) / 8] = words.as_mut_ptr() as u64;
        assert_eq!(run(&p, f, &mut ctx), -7, "the stop's status");
        assert_eq!((cell(&ctx, site::CURSOR) as u32, ctx[hdr::BUDGET_BYTES / 8]), (3, 3), "rows 0+1+2 stored at the stop, cursor at row 3");
        assert_eq!(run(&p, f, &mut ctx), -5, "the resumed call finishes the window");
        assert_eq!((cell(&ctx, site::ROWS_OUT), ctx[hdr::BUDGET_BYTES / 8], words[0]), (6, 15, 1), "3 reloaded, then rows 3+4+5");
    }
}
