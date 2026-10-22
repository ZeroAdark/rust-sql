//! D830: class.num_packed's spilled sum (a term of 19 to 37 digits: 2^63 of them pass 2^126). Hooks / levers: -. Budget
//! 150 lines (files-extra.tsv, +25 % gate). Owner M1-FOLD. Registers nothing. Its slot is the i256 one (`spilled`), its
//! registers `[lo, hi, carry]` (carry × 2^126 + the i128): a checked add (`add_carry`) moves the i128's multiples of 2^126
//! into the carry branch-free, so a finite sum never overflows or raises (|sum| < 2^190); the finals are the i128 ones
//! inside D299's range, the exact i256 ones past it (`num_carry_final`).
use gravity_abi::breaker::SlotKind;
use gravity_decompose::ops::shared::agg::{AggDecl, Register, NONE};
use gravity_decompose::ops::shared::agg_class::Trans;
use gravity_fn::agg_final::SENT_HI;
use gravity_expr::emit::CellValid;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, Type, Value};

use crate::face::fold::{Acc, AccShape, FoldClass};
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::admit::refusal::Site;
use crate::rep::CellRef;

use crate::ops::agg::numeric::num_word::{add_sum, kept};
pub(crate) use crate::ops::agg::numeric::raw::carry::num_carry_final;

/// D830: `a`'s sum is in the i256 slot, its registers `[lo, hi, carry]` (see the module doc).
pub(crate) fn spilled(a: &AggDecl<'_>) -> bool {
    matches!(a.trans, Trans::Abi(SlotKind::SumNumWide | SlotKind::AvgNumWide))
}

/// D830: a spilled sum / avg no num.window tree takes (a keyed end's product, a column past one word) folds its datum into
/// the i256 slot as class.num_wide; with a D256 dscale word as class.num_packed (V4R-549, num_datum.rs); else `varlena`.
pub(crate) fn datum(cx: &impl Site, a: &AggDecl<'_>, varlena: &'static dyn FoldClass) -> &'static dyn FoldClass {
    let (wide, ds) = (spilled(a) && a.dscale_slot == NONE, spilled(a) && a.dscale_slot != NONE);
    crate::face::fold::class(cx, if wide { KindId::ClassNumWide } else if ds { KindId::ClassNumPacked } else { KindId::ClassNumVarlena });
    if wide { &DATUM } else if ds { &crate::ops::agg::numeric::num_datum::DATUM_DS } else { varlena }
}

struct Datum;
static DATUM: Datum = Datum;

fn at<'p>(a: &AggDecl<'p>) -> (&'static dyn FoldClass, AggDecl<'p>) {
    (crate::face::fold::class(&0, KindId::ClassNumWide), AggDecl { reg: if a.reg == Register::AvgNum { Register::AvgNumWide } else { Register::SumNumWide }, ..*a })
}

impl FoldClass for Datum {
    /// num.partial's question: its registers are class.num_wide's (`at`).
    fn class_kind(&self) -> Option<KindId> { Some(KindId::ClassNumWide) }
    fn shape(&self, a: &AggDecl<'_>) -> AccShape { at(a).0.shape(&at(a).1) }
    fn init(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) { at(a).0.init(b, env, &at(a).1, acc) }
    /// A value whose word the decode takes (fold/num_word.rs `word`) adds lean (num_lean.rs `spilled`); any other row its
    /// datum's term.
    fn step(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, args: &[CellRef<'_>]) {
        let (class, w) = at(a);
        let lean = match args {
            [c] => {
                let (m, null, _) = crate::ops::agg::numeric::num_word::word(b, env, *c, a.scale, false);
                crate::ops::agg::numeric::strategies::num_lean::spilled(b, env, &w, class, &acc, m, null)
            }
            _ => None,
        };
        class.step(b, env, &w, acc, args);
        crate::ops::agg::numeric::strategies::num_lean::join(b, env, lean);
    }
    fn merge(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, into: Acc<'_>, from: Acc<'_>) { at(a).0.merge(b, env, &at(a).1, into, from) }
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) -> (Value, Value) { at(a).0.finish(b, env, &at(a).1, acc) }
}

/// D830: a grouped row `[lo, hi, 0, n]` into the i256 slot while it fits i128 and the term is finite (the i128 add; its
/// signed overflow, a sentinel, a slot past i128 to the cold block the builder continues in: the caller's generic fold).
/// D3224 A1: a zeroed record's slot (`Env::rec_zeroed`) is read untested: an invalid one holds the identity's zero words.
pub(crate) fn row(b: &mut FunctionBuilder<'_>, env: &Env, acc: &Acc<'_>, on: bool, avg: bool, row: &[Value]) -> Option<Block> {
    let (true, &Acc::Slot { base, off, valid }) = (on, acc) else { return None };
    let ok = (!env.rec_zeroed).then(|| crate::ops::shared::slots::slot_ok(b, env, base, valid));
    if ok.is_none() {
        fires::fire("num.lean.zeroed"); // fire signal (D3224 A1: a zeroed record's slot added untested)
    }
    let w = [0, 1, 2, 3].map(|i| b.load(Type::I64, base, off + 8 * i));
    let w = w.map(|v| ok.map_or(v, |ok| b.select(ok, v, env.zero)));
    let (s63, sent) = (b.iconst(Type::I64, 63), b.iconst(Type::I64, SENT_HI));
    let sx = b.int_bin(IntBinOp::Sshr, w[1], s63);
    let (f2, f3, fin) = (b.icmp(IntCond::Eq, w[2], sx), b.icmp(IntCond::Eq, w[3], sx), b.icmp(IntCond::Ne, row[1], sent));
    let (lo, hi) = crate::ops::agg::trans::class_word::add128(b, w[0], w[1], row[0], row[1]);
    let (x, y) = (b.int_bin(IntBinOp::Xor, w[1], hi), b.int_bin(IntBinOp::Xor, row[1], hi));
    let ovf = b.int_bin(IntBinOp::And, x, y);
    let kept = b.icmp(IntCond::Sge, ovf, env.zero);
    let fast = [f3, fin, kept].into_iter().fold(f2, |a, c| b.int_bin(IntBinOp::And, a, c));
    let (l_fast, l_cold, l_join) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
    b.mark_cold(l_cold);
    b.cond_br(fast, (l_fast, &[]), (l_cold, &[]));
    b.switch_to(l_fast);
    let top = b.int_bin(IntBinOp::Sshr, hi, s63);
    for (i, v) in [lo, hi, top, top].into_iter().enumerate() {
        b.store(base, off + 8 * i as i32, v);
    }
    let n = row[row.len() - 1];
    if avg {
        let m = b.load(Type::I64, base, off + 32);
        let m = ok.map_or(m, |ok| b.select(ok, m, env.zero));
        let m = b.int_bin(IntBinOp::Add, m, n);
        b.store(base, off + 32, m);
    }
    let some = b.icmp(IntCond::Ne, n, env.zero);
    let any = ok.map_or(some, |ok| b.int_bin(IntBinOp::Or, ok, some));
    crate::ops::shared::slots::mark(b, env, base, valid, any);
    b.br(l_join, &[]);
    b.switch_to(l_cold);
    Some(l_join)
}

/// `row`'s join after the generic fold.
pub(crate) fn join(b: &mut FunctionBuilder<'_>, l_join: Option<Block>) {
    if let Some(l) = l_join {
        b.br(l, &[]);
        b.switch_to(l);
    }
}

/// D830: `a + c` in carry form: the i128's multiples of 2^126 into the carry (the i128 left in [0, 2^126), so no later add
/// wraps it; never OVERFLOW); a sentinel on either side is `add_sum`'s code.
pub(crate) fn add_carry(b: &mut FunctionBuilder<'_>, env: &Env, a: &[Value], c: &[Value]) -> [Value; 3] {
    let (sent, m) = (b.iconst(Type::I64, SENT_HI), b.int_bin(IntBinOp::Smin, a[1], c[1]));
    let slow = b.icmp(IntCond::Eq, m, sent);
    let (l_slow, l_fast, l_join) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64; 3]));
    b.mark_cold(l_slow);
    b.cond_br(slow, (l_slow, &[]), (l_fast, &[]));
    b.switch_to(l_fast);
    let (lo, hi) = crate::ops::agg::trans::class_word::add128(b, a[0], a[1], c[0], c[1]);
    let (s62, mask) = (b.iconst(Type::I64, 62), b.iconst(Type::I64, (1 << 62) - 1));
    let k = b.int_bin(IntBinOp::Sshr, hi, s62);
    let hi = b.int_bin(IntBinOp::And, hi, mask);
    let cy = b.int_bin(IntBinOp::Add, a[2], c[2]);
    let cy = b.int_bin(IntBinOp::Add, cy, k);
    b.br(l_join, &[lo, hi, cy]);
    b.switch_to(l_slow);
    let (lo, hi) = add_sum(b, env, a[0], a[1], c[0], c[1]);
    b.br(l_join, &[lo, hi, env.zero]);
    b.switch_to(l_join);
    let p = b.block_params(l_join);
    [p[0], p[1], p[2]]
}

/// D830: an i256 slot's words as the carry form `[lo, hi mod 2^62, value >> 126]`, a sentinel `[code, SENT_HI, 0]`.
pub(crate) fn from256(b: &mut FunctionBuilder<'_>, env: &Env, w: [Value; 4]) -> [Value; 3] {
    let (s62, s2, mask, sent) = (b.iconst(Type::I64, 62), b.iconst(Type::I64, 2), b.iconst(Type::I64, (1 << 62) - 1), b.iconst(Type::I64, SENT_HI));
    let (top, low) = (b.int_bin(IntBinOp::Ushr, w[1], s62), b.int_bin(IntBinOp::Shl, w[2], s2));
    let cy = b.int_bin(IntBinOp::Or, top, low);
    let hi = b.int_bin(IntBinOp::And, w[1], mask);
    let sp = b.icmp(IntCond::Eq, w[3], sent);
    [w[0], b.select(sp, sent, hi), b.select(sp, env.zero, cy)]
}

/// D830: the carry form as the i256 slot's words (sext(hi:lo) + carry × 2^126), a sentinel `[code, 0, 0, SENT_HI]`.
pub(crate) fn to256(b: &mut FunctionBuilder<'_>, env: &Env, [lo, hi, cy]: [Value; 3]) -> [Value; 4] {
    let (s62, s2, s63, sent) = (b.iconst(Type::I64, 62), b.iconst(Type::I64, 2), b.iconst(Type::I64, 63), b.iconst(Type::I64, SENT_HI));
    let (sx, c1, c2, c3) = (b.int_bin(IntBinOp::Sshr, hi, s63), b.int_bin(IntBinOp::Shl, cy, s62), b.int_bin(IntBinOp::Sshr, cy, s2), b.int_bin(IntBinOp::Sshr, cy, s63));
    let w1 = b.int_bin(IntBinOp::Add, hi, c1);
    let k1 = b.icmp(IntCond::Ult, w1, hi);
    let k1 = b.conv(ConvOp::Zext, Type::I64, k1);
    let (w2, k2) = add_with(b, sx, c2, k1);
    let (w3, _) = add_with(b, sx, c3, k2);
    let sp = b.icmp(IntCond::Eq, hi, sent);
    [lo, b.select(sp, env.zero, w1), b.select(sp, env.zero, w2), b.select(sp, sent, w3)]
}

/// `x + y + k` (k 0 or 1) and its carry out.
fn add_with(b: &mut FunctionBuilder<'_>, x: Value, y: Value, k: Value) -> (Value, Value) {
    let s = b.int_bin(IntBinOp::Add, x, y);
    let t = b.int_bin(IntBinOp::Add, s, k);
    let (c1, c2) = (b.icmp(IntCond::Ult, s, x), b.icmp(IntCond::Ult, t, s));
    let c = b.int_bin(IntBinOp::Or, c1, c2);
    (t, b.conv(ConvOp::Zext, Type::I64, c))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D830: carry-form adds are exact past 2^126 (the i128 kept in [0, 2^126), the rest in the carry), a sentinel is
    /// add_sum's code; the i256 slot round-trips through `to256` / `from256` (the finals: e2e v4/m6_d830_packed_sum).
    #[test]
    fn a_spilled_sum_adds_exactly_past_the_packed_range() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use gravity_fn::agg_final::{SP_NAN, SUM_HI_LIMIT};
        use gravity_ir::builder::ProgramBuilder;
        // f(a lo hi cy, c lo hi cy) = from256(to256(add_carry(a, c)))
        let run = |a: [i64; 3], c: [i64; 3]| -> [i64; 3] {
            let mut pb = ProgramBuilder::new();
            let core = crate::frame::env::declare(&mut pb);
            let f = pb.declare_func("t", &[Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64, Type::I64, Type::I64], &[Type::I64; 3]);
            let mut b = pb.define(f);
            let p = b.params().to_vec();
            let x = crate::frame::stop::exits(&mut b);
            let env = Env::new(&mut b, core, crate::hir::Ir::none(), &[], x, rig.mcx());
            let s = add_carry(&mut b, &env, &p[1..4], &p[4..7]);
            let w = to256(&mut b, &env, s);
            let r = from256(&mut b, &env, w);
            b.ret(&r);
            for blk in [x.l_finish, x.l_stop_in, x.l_full, x.l_status, x.l_done, x.l_raise, x.l_cancel, x.l_exit] {
                b.switch_to(blk);
                let z = b.iconst(Type::I64, 0);
                b.ret(&[z, z, z]);
            }
            let prog = pb.finish();
            let mut ctx = vec![0u64; gravity_abi::ctx::ctx_size(1) / 8];
            let mut args = vec![ctx.as_mut_ptr() as u64];
            args.extend(a.iter().chain(c.iter()).map(|&v| v as u64));
            let got = gravity_interp::Interp::new(&prog).run(f, &args).expect("runs");
            [got[0] as i64, got[1] as i64, got[2] as i64]
        };
        let val = |w: [i64; 3]| (((w[1] as i128) << 64) | w[0] as u64 as i128) + ((w[2] as i128) << 126);
        let big = SUM_HI_LIMIT - 1; // just under 2^126
        for (a, c) in [([5, 0, 0], [7, 0, 0]), ([-3, -1, 0], [2, 0, 0]), ([0, big, 0], [0, big, 0]), ([0, -big, 0], [0, -big, 1]), ([9, big, -1], [1, big, 0])] {
            let got = run(a, c);
            assert_eq!(val(got), val(a) + val(c), "{a:?} + {c:?}");
            assert!((0..SUM_HI_LIMIT).contains(&got[1]), "the i128 left in [0, 2^126): {got:?}");
        }
        assert_eq!(run([SP_NAN, SENT_HI, 0], [4, 0, 1]), [SP_NAN, SENT_HI, 0], "a sentinel is sticky");
    }
}
