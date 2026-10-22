//! Numeric folds over PackedNum (sum / avg / min / max). Hooks / levers: -. Budget 220 lines (files.tsv, +25 % gate).
//! Owner M1-FOLD.
//!
//! Registers: kind `class.num_packed` (`KindId::ClassNumPacked`): a `numeric(p, s)` aggregate at the scale decompose
//! decided (`AggDecl::scale`) over mantissas. A row's term is its value × 10^scale as an i128 (num_window.rs builds it
//! from a +, -, * tree; a bare argument is read here: a packed lane's word, a verbatim or heap image through
//! `num_mantissa`), summed exactly (`iadd` + `addhi`); NaN / ±Infinity is a sticky sentinel (`hi == SENT_HI`, `lo` its
//! code) added as PG adds specials. Words: sum / avg `[lo, hi, n]` (slots `SumNum` / `AvgNum`), min / max `[m, n]` (the
//! mantissa word, its sentinels ordered as `cmp_numerics`: -Inf < finite < Inf < NaN). Finals: gravity_expr's num_sum /
//! num_avg / num_word at the scale. join.mult (H20): the term × w exactly, `n` += w; min / max ignore w.
//! D830: a sum / avg whose term passes one word folds in carry form into the i256 slot (num_carry.rs).
//! NEW-numeric-1: min / max of an unconstrained numeric column (`MinNumDyn` / `MaxNumDyn`) fold the value's order key
//! (gravity_fn typesem numeric `order_key`: exact at 16 significant digits and dscale 15), N13 a value past it into the
//! side companion's copy: num_side.rs, its words `[key, side, n]`.
use gravity_decompose::ops::shared::agg::{AggDecl, Register as R, NONE};
use gravity_fn::agg_final::{NO_MANTISSA, SENT_HI};
use gravity_ir::builder::FunctionBuilder;
use gravity_expr::emit::CellValid;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};

use crate::face::fold::{Acc, AccShape, AnswerForm, ClassImpl, FinalOrder, FoldClass};
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::admit::refusal::Reason;
use crate::rep::CellRef;
use mcx::{CollectIn, GrowIn, Mcx, PgVec};

use crate::ops::agg::numeric::num_carry::{add_carry, from256, num_carry_final, spilled, to256};
use crate::ops::agg::numeric::num_word::{add_sum, capped, num_avg_ds, num_sum_ds, raise, specials, word, Term};
use crate::ops::agg::numeric::num_side;

crate::strategies! {
    /// A verbatim / heap numeric image's finite word takes the lean exact add.
    NUM_LEAN = 121, "num.lean", NumHeap;
}

pub(crate) const ROWS: crate::ops::Table =
    crate::ops::Table { kinds: &[(KindId::ClassNumPacked, &ClassImpl(&NUM))], strategies: &[(crate::face::strategy::NUM_LEAN, &LeanRoad)], ..crate::ops::NONE };

/// Road num.lean's registration value: `step`'s lean add of a verbatim / heap image's finite word
/// (fold/num_lean.rs `word`).
struct LeanRoad;

struct Num;
static NUM: Num = Num;

#[derive(Clone, Copy, PartialEq, Eq)]
enum F {
    Sum,
    Avg,
    Min,
    Max,
}

fn form(node: i32, a: &AggDecl<'_>) -> F {
    if a.dscale_slot != NONE && (!matches!(a.reg, R::SumNum | R::AvgNum) || a.dscale_slot != a.slot + 1) {
        crate::refuse!(node, Reason::Bug { payload: "a D256 dscale slot that is not the slot after its sum's" });
    }
    match a.reg {
        R::SumNum => F::Sum,
        R::AvgNum => F::Avg,
        R::MinNum | R::MinNumDyn => F::Min,
        R::MaxNum | R::MaxNumDyn => F::Max,
        R::SumNumVar | R::AvgNumVar => crate::refuse!(node, Reason::Bug { payload: "a Var numeric register under class.num_packed" }),
        _ => crate::refuse!(node, Reason::Bug { payload: "class.num_packed over another register" }),
    }
}

/// NEW-numeric-1: an extreme over order keys (num_side.rs) rather than mantissas at the aggregate's scale.
fn keyed(a: &AggDecl<'_>) -> bool {
    crate::ops::shared::agg::order_keyed(a)
}

/// N13: a keyed extreme's step into a record slot with the end's image cursor `images` (its side copies packed there,
/// not a pinned region each: dict/minmax_code.rs `step_with`).
pub(crate) fn step_keyed(b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, args: &[CellRef<'_>], images: Option<Value>) {
    let [c] = args else { crate::refuse!(env.node, Reason::Bug { payload: "class.num_packed over other than one argument" }) };
    num_side::step(b, env, form(env.node, a) == F::Min, acc, *c, images);
}

/// D256: the dscale companion's word offset from the sum's (the slot right after it) and its validity (the next bit).
fn ds_at(f: F, wide: bool, valid: CellValid) -> (i32, CellValid) {
    let v = match valid {
        CellValid::Bit { off, bit: 7 } => CellValid::Bit { off: off + 1, bit: 0 },
        CellValid::Bit { off, bit } => CellValid::Bit { off, bit: bit + 1 },
        v => v,
    };
    (if f == F::Avg { 24 } else { 16 } + 16 * wide as i32, v)
}

/// The identity state: sum / avg `[lo, hi, (carry,) (dscale,) n]`, min / max `[m, n]`.
fn ident<'g>(b: &mut FunctionBuilder<'_>, mcx: Mcx<'g>, f: F, (ds, wide): (bool, bool)) -> PgVec<'g, Value> {
    let z = b.iconst(Type::I64, 0);
    match f {
        F::Sum | F::Avg => mcx::pg_vec![in mcx; z; 3 + ds as usize + wide as usize],
        F::Min => mcx::pg_vec![in mcx; b.iconst(Type::I64, i64::MAX), z],
        F::Max => mcx::pg_vec![in mcx; b.iconst(Type::I64, i64::MIN), z],
    }
}

/// The state `[…, n]`: registers as they are; a slot's words (an invalid slot the identity; an i256 one in carry form).
fn load<'g>(b: &mut FunctionBuilder<'_>, env: &Env<'g, '_>, f: F, (ds, wide): (bool, bool), acc: &Acc<'_>) -> PgVec<'g, Value> {
    let Acc::Slot { base, off, valid } = *acc else {
        let Acc::Regs(w) = acc else { unreachable!() };
        return mcx::to_vec_in(&w[..], env.mcx);
    };
    let ok = crate::ops::shared::slots::slot_ok(b, env, base, valid);
    let id = ident(b, env.mcx, f, (ds, wide));
    let main = if matches!(f, F::Min | F::Max) { 1 } else { 2 + 2 * wide as usize };
    let mut w = (0..main).map(|i| b.load(Type::I64, base, off + 8 * i as i32)).collect_in(env.mcx);
    if wide {
        let c = from256(b, env, [w[0], w[1], w[2], w[3]]);
        w = mcx::to_vec_in(&c[..], env.mcx);
    }
    if ds {
        w.push_in(b.load(Type::I64, base, off + ds_at(f, wide, valid).0));
    }
    for (v, &i) in w.iter_mut().zip(id.iter()) {
        *v = b.select(ok, *v, i);
    }
    let n = match f {
        F::Avg => b.load(Type::I64, base, off + 16 + 16 * wide as i32),
        _ => b.conv(ConvOp::Zext, Type::I64, ok),
    };
    w.push_in(b.select(ok, n, env.zero));
    w
}

fn store(b: &mut FunctionBuilder<'_>, env: &Env, f: F, (ds, wide): (bool, bool), acc: Acc<'_>, w: &[Value]) {
    match acc {
        Acc::Regs(r) => r.copy_from_slice(w),
        Acc::Slot { base, off, valid } => {
            let main = match f {
                F::Min | F::Max => mcx::pg_vec![in env.mcx; w[0]],
                _ if wide => mcx::to_vec_in(&to256(b, env, [w[0], w[1], w[2]])[..], env.mcx),
                _ => mcx::pg_vec![in env.mcx; w[0], w[1]],
            };
            for (i, &v) in main.iter().enumerate() {
                b.store(base, off + 8 * i as i32, v);
            }
            let n = w[w.len() - 1];
            if f == F::Avg {
                b.store(base, off + 16 + 16 * wide as i32, n);
            }
            let any = b.icmp(IntCond::Ne, n, env.zero);
            if ds {
                let (o, v) = ds_at(f, wide, valid);
                b.store(base, off + o, w[2 + wide as usize]);
                crate::ops::shared::slots::mark(b, env, base, v, any);
            }
            crate::ops::shared::slots::mark(b, env, base, valid, any);
        }
    }
}

/// Two states combined: sums by `add_sum`, extrema by the word order, the dscale the larger, `n` added.
fn combine<'g>(b: &mut FunctionBuilder<'_>, env: &Env<'g, '_>, f: F, wide: bool, a: &[Value], c: &[Value], g: Option<Value>) -> PgVec<'g, Value> {
    let k = a.len() - 1;
    let n = b.int_bin(IntBinOp::Add, a[k], c[k]);
    let mut w = match f {
        F::Sum | F::Avg => match g {
            Some(g) => add_window(b, env, wide, a, c, g),
            None if wide => mcx::to_vec_in(&add_carry(b, env, a, c)[..], env.mcx),
            None => {
                let (lo, hi) = add_sum(b, env, a[0], a[1], c[0], c[1]);
                mcx::pg_vec![in env.mcx; lo, hi]
            }
        },
        F::Min => mcx::pg_vec![in env.mcx; b.int_bin(IntBinOp::Smin, a[0], c[0])],
        F::Max => mcx::pg_vec![in env.mcx; b.int_bin(IntBinOp::Smax, a[0], c[0])],
    };
    let d = w.len();
    if k == d + 1 {
        w.push_in(b.int_bin(IntBinOp::Smax, a[d], c[d]));
    }
    w.push_in(n);
    w
}

/// `add_sum` under a window guard `g`: finite sums add unchecked (a `wide` carry added); a sentinel or a window past the
/// guard takes `add_sum` (`add_carry`).
fn add_window<'g>(b: &mut FunctionBuilder<'_>, env: &Env<'g, '_>, wide: bool, a: &[Value], c: &[Value], g: Value) -> PgVec<'g, Value> {
    let (sent, m) = (b.iconst(Type::I64, SENT_HI), b.int_bin(IntBinOp::Smin, a[1], c[1]));
    let fin = b.icmp(IntCond::Ne, m, sent);
    let fast = b.int_bin(IntBinOp::And, fin, g);
    let (l_fast, l_chk, l_join) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64; 3][..2 + wide as usize]));
    b.cond_br(fast, (l_fast, &[]), (l_chk, &[]));
    b.switch_to(l_fast);
    let (lo, hi) = crate::ops::agg::trans::class_word::add128(b, a[0], a[1], c[0], c[1]);
    let mut v = mcx::pg_vec![in env.mcx; lo, hi];
    if wide {
        v.push_in(b.int_bin(IntBinOp::Add, a[2], c[2]));
    }
    b.br(l_join, &v[..]);
    b.switch_to(l_chk);
    let v = if wide { mcx::to_vec_in(&add_carry(b, env, a, c)[..], env.mcx) } else { let (lo, hi) = add_sum(b, env, a[0], a[1], c[0], c[1]); mcx::pg_vec![in env.mcx; lo, hi] };
    b.br(l_join, &v[..]);
    b.switch_to(l_join);
    mcx::to_vec_in(&b.block_params(l_join)[..], env.mcx)
}


/// Term `t` into the state `acc` (join.mult: `w` copies of it, exactly).
pub(crate) fn fold(b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, t: &Term, g: Option<Value>) {
    let (f, ds, wide) = (form(env.node, a), a.dscale_slot != NONE, spilled(a));
    let one = b.conv(ConvOp::Zext, Type::I64, t.ok);
    let n = match env.mult {
        Some(m) => b.select(t.ok, m, env.zero),
        None => one,
    };
    let row = match f {
        F::Min | F::Max => {
            let id = ident(b, env.mcx, f, (false, false))[0];
            mcx::pg_vec![in env.mcx; b.select(t.ok, t.lo, id), n]
        }
        F::Sum | F::Avg => {
            let s63 = b.iconst(Type::I64, 63);
            let mut hi = t.hi.unwrap_or_else(|| b.int_bin(IntBinOp::Sshr, t.lo, s63));
            let (mut lo, mut cy) = (t.lo, env.zero);
            if let Some(m) = env.mult {
                // D299: w copies past the sum's range are the sticky OVERFLOW code (a NaN row still wins, as in PG);
                // D830: a spilled sum takes the product exactly in carry form (under 2^186: a term under 2^123, w 2^63)
                let (l, h, fits, top) = crate::ops::agg::numeric::strategies::num_window::mul128x64(b, lo, hi, m);
                if wide {
                    let c = from256(b, env, [l, h, top, top]);
                    (lo, hi, cy) = (c[0], c[1], c[2]);
                } else {
                    let (l, h) = capped(b, l, h);
                    let (code, sent) = (b.iconst(Type::I64, gravity_fn::agg_final::SP_OVERFLOW), b.iconst(Type::I64, SENT_HI));
                    (lo, hi) = (b.select(fits, l, code), b.select(fits, h, sent));
                }
            }
            if let Some(sp) = t.sp {
                let special = b.icmp(IntCond::Ne, sp, env.zero);
                let sent = b.iconst(Type::I64, SENT_HI);
                (lo, hi, cy) = (b.select(special, sp, lo), b.select(special, sent, hi), b.select(special, env.zero, cy));
            }
            let mut w = mcx::pg_vec![in env.mcx; b.select(t.ok, lo, env.zero), b.select(t.ok, hi, env.zero)];
            if wide {
                w.push_in(b.select(t.ok, cy, env.zero));
            }
            if ds {
                let d = t.ds.unwrap_or_else(|| b.iconst(Type::I64, a.scale as i64));
                w.push_in(b.select(t.ok, d, env.zero));
            }
            w.push_in(n);
            w
        }
    };
    // D830: a grouped row adds into the i256 slot directly while it fits i128 (num_carry.rs `row`), else the generic fold
    let fast = crate::ops::agg::numeric::num_carry::row(b, env, &acc, wide && !ds && env.mult.is_none(), f == F::Avg, &row[..]);
    let s = load(b, env, f, (ds, wide), &acc);
    let w = combine(b, env, f, wide, &s[..], &row[..], g.filter(|_| matches!(acc, Acc::Regs(_)) && env.mult.is_none()));
    store(b, env, f, (ds, wide), acc, &w[..]);
    crate::ops::agg::numeric::num_carry::join(b, fast);
}

impl FoldClass for Num {
    fn class_kind(&self) -> Option<KindId> {
        Some(KindId::ClassNumPacked)
    }

    fn shape(&self, a: &AggDecl<'_>) -> AccShape {
        match a.reg {
            R::MinNum | R::MaxNum => AccShape { words: 2, bytes: 8, align: 8 },
            // N13: `[key, side, n]`, the side companion its own slot (decompose agg `cell`)
            R::MinNumDyn | R::MaxNumDyn => AccShape { words: num_side::WORDS, bytes: 8, align: 8 },
            R::AvgNum => AccShape { words: 3 + (a.dscale_slot != NONE) as u8 + spilled(a) as u8, bytes: 24 + 16 * spilled(a) as u32, align: 8 },
            _ => AccShape { words: 3 + (a.dscale_slot != NONE) as u8 + spilled(a) as u8, bytes: 16 + 16 * spilled(a) as u32, align: 8 },
        }
    }

    fn init(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) {
        if keyed(a) {
            return num_side::init(b, env, form(env.node, a) == F::Min, acc);
        }
        let (f, ds) = (form(env.node, a), (a.dscale_slot != NONE, spilled(a)));
        let id = ident(b, env.mcx, f, ds);
        store(b, env, f, ds, acc, &id[..]);
    }

    /// A bare argument (a packed lane, a verbatim or heap image); a tree comes through num_window.rs to `fold`.
    fn step(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, args: &[CellRef<'_>]) {
        let f = form(env.node, a);
        let [c] = args else { crate::refuse!(env.node, Reason::Bug { payload: "class.num_packed over other than one argument" }) };
        if keyed(a) {
            return num_side::step(b, env, f == F::Min, acc, *c, None);
        }
        let lean = crate::ops::agg::numeric::strategies::num_lean::bare(b, env, a, &acc, *c); // D619: a packed window's lean add; the verbatim one here
        let (m, null, ds) = word(b, env, *c, a.scale, a.dscale_slot != NONE);
        // A verbatim / heap image's finite word takes the lean add; a NULL or a special the generic step below
        let lean = match lean {
            Some(l) => Some(l),
            None if matches!(f, F::Sum | F::Avg) => crate::ops::agg::numeric::strategies::num_lean::word(b, env, a, &acc, m, null),
            None => None,
        };
        let t1 = b.bconst(true);
        let ok = b.int_bin(IntBinOp::Xor, null, t1);
        let t = match f {
            F::Min | F::Max => {
                let none = b.iconst(Type::I64, NO_MANTISSA);
                let bad = b.icmp(IntCond::Eq, m, none);
                let bad = b.int_bin(IntBinOp::And, bad, ok);
                raise(b, env, bad, 0);
                Term { lo: m, hi: None, sp: None, ok, ds }
            }
            F::Sum | F::Avg => {
                let (lo, sp) = specials(b, env, m, ok);
                Term { lo, hi: None, sp: Some(sp), ok, ds }
            }
        };
        fold(b, env, a, acc, &t, None);
        crate::ops::agg::numeric::strategies::num_lean::join(b, env, lean);
    }

    fn merge(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, into: Acc<'_>, from: Acc<'_>) {
        if keyed(a) {
            return num_side::merge(b, env, form(env.node, a) == F::Min, into, from);
        }
        let (f, ds) = (form(env.node, a), (a.dscale_slot != NONE, spilled(a)));
        let (x, y) = (load(b, env, f, ds, &into), load(b, env, f, ds, &from));
        let w = combine(b, env, f, ds.1, &x[..], &y[..], None);
        store(b, env, f, ds, into, &w[..]);
    }

    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) -> (Value, Value) {
        if keyed(a) {
            return num_side::finish(b, env, form(env.node, a) == F::Min, acc);
        }
        let (f, (ds, wide)) = (form(env.node, a), (a.dscale_slot != NONE, spilled(a)));
        let w = load(b, env, f, (ds, wide), &acc);
        let n = w[w.len() - 1];
        let none = b.icmp(IntCond::Eq, n, env.zero);
        let sc = b.iconst(Type::I64, a.scale as i64);
        use gravity_expr::emit::agg as g;
        use crate::ops::agg::trans::plain::host_final as fin;
        // D830: the carry form's finals (the i128 ones inside D299's range, the exact i256 past it); scale << 32 | dscale
        let sd = if wide {
            let s32 = b.iconst(Type::I64, (a.scale as i64) << 32);
            b.int_bin(IntBinOp::Or, s32, if ds { w[3] } else { sc })
        } else {
            sc
        };
        let v = match f {
            F::Sum | F::Avg if wide => fin(b, env, "class_num::num_carry_final", num_carry_final as *const () as usize, &[w[0], w[1], w[2], if f == F::Avg { n } else { env.zero }, sd], none),
            // D299: through num_word.rs's finals, which keep the past-range error's name (22003)
            F::Sum => fin(b, env, "class_num::num_sum_ds", num_sum_ds as *const () as usize, &[w[0], w[1], sc, if ds { w[2] } else { sc }], none),
            F::Avg => fin(b, env, "class_num::num_avg_ds", num_avg_ds as *const () as usize, &[w[0], w[1], n, sc, if ds { w[2] } else { sc }], none),
            F::Min | F::Max => crate::ops::agg::trans::plain::host_final(b, env, "gravity_expr::num_word", gravity_abi::host_words!(g::num_word(region, m, scale)), &[w[0], sc], none),
        };
        (v, none)
    }

    fn order(&self, a: &AggDecl<'_>) -> Option<FinalOrder> {
        matches!(a.reg, R::MinNum | R::MaxNum).then_some(FinalOrder::Word)
    }

    /// D3024: a sum folded in one i128 at its typmod's scale (no dscale companion, no carry form) and a min / max
    /// mantissa word (not an order-keyed extreme): their finals are read lazily, and a compare reads the words.
    fn packed_num(&self, a: &AggDecl<'_>) -> Option<crate::face::fold::PackedNum> {
        if keyed(a) || a.dscale_slot != NONE || a.scale < 0 || !matches!(a.reg, R::SumNum | R::MinNum | R::MaxNum) || spilled(a) {
            return None;
        }
        let sum = a.reg == R::SumNum;
        Some(crate::face::fold::PackedNum { sum, scale: a.scale as u16, render: if sum { num_sum_ds as *const () as usize } else { 0 } })
    }

    fn answer(&self, a: &AggDecl<'_>) -> Option<AnswerForm> {
        matches!(a.reg, R::SumNum | R::AvgNum).then_some(AnswerForm::Numeric)
    }

    /// D3224 A3: a sum and an avg each an i128 at one fixed scale (no dscale companion, no carry form): the avg's `[lo, hi]`
    /// would be the sum's words, its `n` its own (a sum keeps no count, only its slot's validity).
    fn shares(&self, s: &AggDecl<'_>, a: &AggDecl<'_>) -> bool {
        let one = |d: &AggDecl<'_>| !keyed(d) && d.dscale_slot == NONE && !spilled(d) && d.scale >= 0;
        s.reg == R::SumNum && a.reg == R::AvgNum && s.scale == a.scale && one(s) && one(a)
    }

    /// D3224 A3: `n` += the row's weight (join.mult's w, else 1) for a non-NULL argument, a special too, as `fold` counts
    /// it. The slot's `[lo, hi]` stay the identity (a merge adds them; the final reads the sum's): in a record not zeroed
    /// (`Env::rec_zeroed`) a first step writes them so.
    fn count(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, args: &[CellRef<'_>]) {
        let [c] = args else { crate::refuse!(env.node, Reason::Bug { payload: "class.num_packed over other than one argument" }) };
        // `word`'s NULL: a packed lane at the scale its cell's own (no retain of the value), else the datum's (the sum's
        // step decoded it: the row memo answers)
        let lane = matches!(c.cell.rep, gravity_program::Rep::PackedNum { .. }) && crate::rep::num_of(c.cell.rep).scale == a.scale;
        let null = match (lane, c.null) {
            (true, Some(n)) => n,
            (true, None) => b.bconst(false),
            (false, _) => word(b, env, *c, a.scale, false).1,
        };
        let t1 = b.bconst(true);
        let ok = b.int_bin(IntBinOp::Xor, null, t1);
        let inc = match env.mult {
            Some(m) => b.select(ok, m, env.zero),
            None => b.conv(ConvOp::Zext, Type::I64, ok),
        };
        match acc {
            Acc::Regs(r) => {
                let k = r.len() - 1;
                r[k] = b.int_bin(IntBinOp::Add, r[k], inc);
            }
            Acc::Slot { base, off, valid } => {
                let mut n = b.load(Type::I64, base, off + 16);
                if !env.rec_zeroed {
                    let live = crate::ops::shared::slots::slot_ok(b, env, base, valid);
                    n = b.select(live, n, env.zero);
                    for i in 0..2 {
                        let w = b.load(Type::I64, base, off + 8 * i);
                        let w = b.select(live, w, env.zero);
                        b.store(base, off + 8 * i, w);
                    }
                }
                let n = b.int_bin(IntBinOp::Add, n, inc);
                b.store(base, off + 16, n);
                let any = b.icmp(IntCond::Ne, n, env.zero);
                crate::ops::shared::slots::mark(b, env, base, valid, any);
            }
        }
    }

    /// D3224 A3: the avg over the sum's `[lo, hi]` (the identity while the sum's slot is invalid: then `n` is 0 too) and its
    /// own `n`, through `finish`.
    fn finish_shared(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, sum: Acc<'_>, own: Acc<'_>) -> (Value, Value) {
        fires::fire("num.avg.share"); // fire signal (D3224 A3: an avg finished over its sum's slot)
        let s = load(b, env, F::Sum, (false, false), &sum);
        let o = load(b, env, F::Avg, (false, false), &own);
        let mut w = [s[0], s[1], o[2]];
        self.finish(b, env, a, Acc::Regs(&mut w))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind as S};

    /// D256: the companion is the slot after the sum's, so its word and validity bit are `ds_at`'s (abi's layout rule).
    #[test]
    fn the_dscale_companion_sits_after_its_sum() {
        for keys in [&[][..], &[KeyKind::I32][..], &[KeyKind::I64, KeyKind::I64][..]] {
            // D830: a spilled sum's companion after its i256 slot
            for (wide, sum, avg) in [(false, S::SumNum, S::AvgNum), (true, S::SumNumWide, S::AvgNumWide)] {
                let l = RecordLayout::new(keys, &[S::Count, sum, S::MaxI64, avg, S::MaxI64, S::SumI64, sum, S::MaxI64]);
                for (j, f) in [(1, F::Sum), (3, F::Avg), (6, F::Sum)] {
                    let (o, bit) = l.valid_bit(j);
                    let (d, v) = ds_at(f, wide, CellValid::Bit { off: o as u32, bit });
                    let (o2, bit2) = l.valid_bit(j + 1);
                    assert_eq!((l.slot_off(j) as i32 + d, v), (l.slot_off(j + 1) as i32, CellValid::Bit { off: o2 as u32, bit: bit2 }));
                }
            }
        }
    }

    /// TH06 lever a: under the window guard two finite sums add exactly as add_sum does inside D299's range; a sentinel
    /// on either side, or the guard off, is add_sum itself (a sum leaving the range is OVERFLOW there).
    #[test]
    fn a_guarded_window_add_is_add_sum_on_every_finite_and_sentinel_pair() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use gravity_fn::agg_final::{SP_NAN, SP_OVERFLOW, SUM_HI_LIMIT};
        use gravity_ir::builder::ProgramBuilder;
        let run = |guarded: Option<bool>, a: (i64, i64), t: (i64, i64)| -> (i64, i64) {
            let mut pb = ProgramBuilder::new();
            let core = crate::frame::env::declare(&mut pb);
            let f = pb.declare_func("t", &[Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64, Type::I64], &[Type::I64, Type::I64]);
            let mut b = pb.define(f);
            let p = b.params().to_vec();
            let x = crate::frame::stop::exits(&mut b);
            let env = Env::new(&mut b, core, crate::hir::Ir::none(), &[], x, rig.mcx());
            let one = b.iconst(Type::I64, 1);
            let g = b.icmp(IntCond::Eq, p[5], one);
            let (lo, hi) = match guarded {
                Some(_) => {
                    let v = add_window(&mut b, &env, false, &p[1..3], &p[3..5], g);
                    (v[0], v[1])
                }
                None => add_sum(&mut b, &env, p[1], p[2], p[3], p[4]),
            };
            b.ret(&[lo, hi]);
            for blk in [x.l_finish, x.l_stop_in, x.l_full, x.l_status, x.l_done, x.l_raise, x.l_cancel, x.l_exit] {
                b.switch_to(blk);
                let z = b.iconst(Type::I64, 0);
                b.ret(&[z, z]);
            }
            let prog = pb.finish();
            let mut ctx = vec![0u64; gravity_abi::ctx::ctx_size(1) / 8];
            let args = [ctx.as_mut_ptr() as u64, a.0 as u64, a.1 as u64, t.0 as u64, t.1 as u64, guarded.unwrap_or(false) as u64];
            let got = gravity_interp::Interp::new(&prog).run(f, &args).expect("runs");
            (got[0] as i64, got[1] as i64)
        };
        let big = SUM_HI_LIMIT - 1;
        let pairs = [((5, 0), (7, 0)), ((-3, -1), (2, 0)), ((u64::MAX as i64, 7), (1, 0)), ((SP_NAN, SENT_HI), (9, 0)), ((4, 0), (SP_NAN, SENT_HI)), ((0, big), (0, big))];
        for (a, t) in pairs {
            let want = run(None, a, t);
            assert_eq!(run(Some(false), a, t), want, "guard off: add_sum");
            let exact = (((a.1 as i128) << 64) | a.0 as u64 as i128) + (((t.1 as i128) << 64) | t.0 as u64 as i128);
            let fin = a.1 != SENT_HI && t.1 != SENT_HI;
            let on = run(Some(true), a, t);
            if fin {
                assert_eq!(on, (exact as i64, (exact >> 64) as i64), "guard on: the exact i128 sum");
            } else {
                assert_eq!(on, want, "a sentinel: add_sum");
            }
        }
        assert_eq!(run(None, (0, big), (0, big)), (SP_OVERFLOW, SENT_HI), "past the range add_sum is OVERFLOW");
    }
}
