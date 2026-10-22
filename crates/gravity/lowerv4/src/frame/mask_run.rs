//! Road `mask.run` (H01b, R1-L03 R2-L17): an all-ones block runs straight. The block path walks a block's passing rows by
//! ctz (find the lowest bit, clear it). A block whose mask is all ones (a covered window, a filter every row passes) has
//! its 64 rows in order, so the walk counts `i + 1` instead. One body serves both: the back edge branches on the block's
//! all-ones test (loop-invariant per block, so predicted), and the rows run in the same order either way. Budget 160 lines.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, IntBinOp, IntCond, IntUnOp, Type, Value};
use mcx::{CollectIn, GrowIn, Mcx, PgVec};

use crate::face::step::{MaskRunImpl, MaskRunMech, RowWalk};
use crate::frame::env::Env;
use crate::frame::frame::Mask;
use crate::frame::row::Carry;

crate::strategies! {
    /// All-ones straight run.
    MASK_RUN = 2, "mask.run", ScanFilter;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(MASK_RUN, &MaskRunImpl(&RUN))], ..crate::ops::NONE };

struct Run;
static RUN: Run = Run;

/// `l_next(next, carry…)`'s arguments.
fn to_next<'g>(next: Value, c: &Carry, mcx: Mcx<'g>) -> PgVec<'g, Value> {
    core::iter::once(next).chain(c.params()).collect_in(mcx)
}

impl MaskRunMech for Run {
    fn walk<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, base: Value, m: Mask, carry: &Carry, (l_next, next): (Block, Value), row: RowWalk<'_, 'e>) {
        let ctypes: PgVec<'_, Type> = carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let it_types: PgVec<'_, Type> = [Type::I64, Type::I64].into_iter().chain(ctypes.iter().copied()).collect_in(env.mcx);
        let l_first = b.create_block(&[]);
        let any = b.icmp(IntCond::Ne, m.0, env.zero);
        b.cond_br(any, (l_first, &[]), (l_next, &to_next(next, carry, env.mcx)[..]));
        b.switch_to(l_first);
        let all = b.iconst(Type::I64, -1);
        let full = b.icmp(IntCond::Eq, m.0, all);
        let sixty_four = b.iconst(Type::I64, 64);
        let end = b.int_bin(IntBinOp::Add, base, sixty_four);
        let (i0, rest0) = lowest(b, env, base, m.0);
        let l_it = b.create_block(&it_types);
        b.mark_loop_header(l_it);
        env.bounded.push_in(l_it);
        let mut first = mcx::pg_vec![in env.mcx; i0, rest0];
        first.extend_in(carry.params());
        b.br(l_it, &first[..]);
        b.switch_to(l_it);
        let p = mcx::to_vec_in(&b.block_params(l_it)[..], env.mcx);
        let (i, rest) = (p[0], p[1]);
        let c = row(b, env, i, Carry::from_params(&p[2..], env.mcx));
        let (l_inc, l_ctz, l_step) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(full, (l_inc, &[]), (l_ctz, &[]));
        // the straight run: the next row is `i + 1`, to the block's end
        b.switch_to(l_inc);
        let i1 = b.int_bin(IntBinOp::Add, i, env.one);
        let more = b.icmp(IntCond::Slt, i1, end);
        let mut on = mcx::pg_vec![in env.mcx; i1, rest];
        on.extend_in(c.params());
        b.cond_br(more, (l_it, &on[..]), (l_next, &to_next(next, &c, env.mcx)[..]));
        // the ctz walk: the next row is the lowest bit left
        b.switch_to(l_ctz);
        let empty = b.icmp(IntCond::Eq, rest, env.zero);
        b.cond_br(empty, (l_next, &to_next(next, &c, env.mcx)[..]), (l_step, &[]));
        b.switch_to(l_step);
        let (i2, rest2) = lowest(b, env, base, rest);
        let mut on = mcx::pg_vec![in env.mcx; i2, rest2];
        on.extend_in(c.params());
        b.br(l_it, &on[..]);
    }
}

/// The row of `bits`'s lowest set bit (`base + ctz`) and `bits` with it cleared.
fn lowest(b: &mut FunctionBuilder<'_>, env: &Env, base: Value, bits: Value) -> (Value, Value) {
    let t = b.int_un(IntUnOp::Ctz, bits);
    let i = b.int_bin(IntBinOp::Add, base, t);
    let low = b.int_bin(IntBinOp::Sub, bits, env.one);
    (i, b.int_bin(IntBinOp::And, bits, low))
}
