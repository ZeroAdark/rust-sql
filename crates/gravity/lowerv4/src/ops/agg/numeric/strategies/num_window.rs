//! Window-word mantissa fold, Smulh + AddHi, sentinels. Hooks / levers: H14 R2-L07. Budget 160 lines (files.tsv, +25 %
//! gate). Owner M1-FOLD.
//!
//! Registers: road `num.window` (`NUM_WINDOW`): a `class.num_packed` argument that is a tree of numeric columns,
//! constants, int casts, typmod casts and `+`, `-`, `*` folds as exact integer arithmetic on mantissas (lowerv2 agg/numeric.rs): a
//! node at scale `s` is its value × 10^s, `*` adds the scales (`mul_var`'s rscale), `+` / `-` rescale to the larger by a
//! compile-time power of ten; a node under 18 digits stays one word, a wider one is 128 bits (`mul` + `smulh`, `iadd` +
//! `addhi`). The finite values run first; the leaves' sentinel flags (NaN / ±Inf) are OR-ed and, only when one is set, a
//! cold block works the tree's code out as PG's numeric operators do (0 × Inf is NaN). Not a tree (a division, a
//! function, a CASE with an arm that is not one): the argument is an expression cell and class.num_packed reads its
//! image. A searched CASE of tree arms is a node (num_case.rs); `live` masks the raises of a not-taken arm's nodes.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use mcx::{GrowIn, PgVec};

use crate::frame::env::Env;
use crate::frame::row::RowVals;

use crate::ops::agg::numeric::strategies::num_tree::{digits_of, scale_of, Tree, NARROW};

crate::strategies! {
    /// Window-word mantissa fold, Smulh + AddHi, sentinels.
    NUM_WINDOW = 25, "num.window", NumPacked;
    /// D4567: a numeric sum / avg argument's decided scale from a mixed-scale CASE / COALESCE beside a `+` / `-` side of a
    /// larger fixed scale (num_tree.rs `fixed_beside`; decompose reads the road, on by default; named
    /// off plainly, those arguments fold hosted).
    NUM_SCALE = 148, "agg.num_scale", NumAny;
    /// D4567 (a): a numeric sum / avg of `x / k` (k a positive constant whose reciprocal is a finite decimal) folds x x c
    /// at a decided scale, its display scale by row in the dscale word (num_tree.rs `Tree::Div`; decompose reads the road
    /// through `Facts::road_on`: default off, on under `+agg.num_div`; off, the argument folds hosted).
    NUM_DIV = 159, "agg.num_div", NumAny;
    /// D4723: a min / max of a materialized CTE's numeric column whose every value has one display scale folds the
    /// values' order keys (class.num_packed's keyed extreme, num_side.rs; decompose agg mod.rs `cte_one_dscale` reads the
    /// road through `Facts::road_on`: default off, on under `+agg.num_cte`; off, the hosted numeric_smaller / larger).
    NUM_CTE = 162, "agg.num_cte", NumAny;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(NUM_WINDOW, &Road), (NUM_SCALE, &Road), (NUM_DIV, &Road), (NUM_CTE, &Road)], ..crate::ops::NONE };

/// The road's registration value (plain.rs folds through `term`).
struct Road;

/// One node's value for the row: `lo` (+ `hi` past 18 digits), a leaf's specials code, its validity, its kids; a CASE's
/// arm selectors (one per kid) and dscale word (num_case.rs).
pub(crate) struct Val<'v> {
    pub lo: Value,
    pub hi: Option<Value>,
    pub sp: Option<Value>,
    pub ok: Value,
    pub kids: PgVec<'v, Val<'v>>,
    pub sel: PgVec<'v, Value>,
    pub ds: Option<Value>,
    /// A column leaf's word and validity under its arm (`sp` its sentinel flag): the cold block works its code out.
    pub raw: Option<(Value, Value)>,
}

pub(crate) fn sext(b: &mut FunctionBuilder<'_>, v: Value) -> Value {
    let s63 = b.iconst(Type::I64, 63);
    b.int_bin(IntBinOp::Sshr, v, s63)
}

/// `hi:lo × t` signed, 128 × 64 → 128, with whether the true product fits: the 192-bit sum of `lo × t` (`lo`
/// unsigned: the signed high word plus `t` where `lo`'s top bit is set) and `hi × t` one word up; it fits when its top
/// word is the sign of the middle one (V4R-38: and that top word, the exact 192-bit product's).
pub(crate) fn mul128x64(b: &mut FunctionBuilder<'_>, lo: Value, hi: Value, t: Value) -> (Value, Value, Value, Value) {
    let (l0, sh) = (b.int_bin(IntBinOp::Mul, lo, t), b.int_bin(IntBinOp::Smulh, lo, t));
    let sl = sext(b, lo);
    let corr = b.int_bin(IntBinOp::And, sl, t);
    let a = b.int_bin(IntBinOp::Add, sh, corr);
    let (h0, h1) = (b.int_bin(IntBinOp::Mul, hi, t), b.int_bin(IntBinOp::Smulh, hi, t));
    let w1 = b.int_bin(IntBinOp::Add, a, h0);
    let cy = b.icmp(IntCond::Ult, w1, a);
    let cy = b.conv(gravity_ir::ConvOp::Zext, Type::I64, cy);
    let sa = sext(b, a);
    let w2 = b.int_bin(IntBinOp::Add, sa, h1);
    let w2 = b.int_bin(IntBinOp::Add, w2, cy);
    let sw = sext(b, w1);
    (l0, w1, b.icmp(IntCond::Eq, w2, sw), w2)
}

/// `mul128x64` whose lost bits raise by name where `ok` (D297: a product never wraps into a value).
fn mul_wide(b: &mut FunctionBuilder<'_>, env: &Env, lo: Value, hi: Value, t: Value, ok: Value) -> (Value, Value) {
    let (lo, hi, fits, _) = mul128x64(b, lo, hi, t);
    let yes = b.bconst(true);
    let lost = b.int_bin(IntBinOp::Xor, fits, yes);
    let bad = b.int_bin(IntBinOp::And, lost, ok);
    crate::ops::agg::numeric::num_word::raise(b, env, bad, 1);
    (lo, hi)
}

/// `v` × 10^k as a node of `digits` (one word when it stays under 18 digits); a lost bit raises where `ok`.
pub(crate) fn rescale(b: &mut FunctionBuilder<'_>, env: &Env, v: &Val, k: i16, narrow: bool, ok: Value) -> (Value, Option<Value>) {
    if k == 0 {
        return (v.lo, v.hi);
    }
    let f = b.iconst(Type::I64, 10i64.pow(k as u32));
    match (v.hi, narrow) {
        (None, true) => (b.int_bin(IntBinOp::Mul, v.lo, f), None),
        (None, false) => (b.int_bin(IntBinOp::Mul, v.lo, f), Some(b.int_bin(IntBinOp::Smulh, v.lo, f))),
        (Some(hi), _) => {
            let (lo, hi) = mul_wide(b, env, v.lo, hi, f, ok);
            (lo, Some(hi))
        }
    }
}

/// Tree `t`'s finite value for the row (`memo`: this row's subtrees already emitted, num.memo; `live`: under a CASE
/// arm, where it is taken, its raises masked by it).
pub(crate) fn finite<'t>(b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, t: &'t Tree, memo: &mut crate::ops::agg::numeric::strategies::memo_num::Memo<'t>, live: Option<Value>) -> Val<'t> {
    crate::stack_check();
    if let Some(v) = memo.get(t) {
        return v;
    }
    let (yes, mcx) = (b.bconst(true), memo.mcx);
    let leaf = |lo, sp, ok| Val { lo, hi: None, sp, ok, kids: PgVec::new_in(mcx), sel: PgVec::new_in(mcx), ds: None, raw: None };
    let lv = |b: &mut FunctionBuilder<'_>, ok| live.map_or(ok, |l| b.int_bin(IntBinOp::And, ok, l));
    let v = match t {
        Tree::Case(c) => crate::ops::agg::numeric::strategies::num_case::finite(b, env, row, c, memo, live),
        Tree::Const { m, .. } => leaf(b.iconst(Type::I64, *m), None, yes),
        // D2356: a constant past one word is a 128-bit leaf (its low and high words)
        Tree::PastWord { m: Some(m), .. } => {
            fires::fire("num.window.leaf128"); // shapecheck fire signal (REGISTRY FOLD-08: a constant past one word as a 128-bit leaf, D2356)
            Val { hi: Some(b.iconst(Type::I64, (*m >> 64) as i64)), ..leaf(b.iconst(Type::I64, *m as i64), None, yes) }
        }
        Tree::PastWord { m: None, .. } => crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a numeric constant past an i128 in a strict tree" }),
        Tree::Col { c, scale, .. } => {
            let Some(cell) = row.col(*c) else { crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a numeric leaf the row does not carry" }) };
            let (m, null, _) = crate::ops::agg::numeric::num_word::word(b, env, cell, *scale, false);
            let ok = b.int_bin(IntBinOp::Xor, null, yes);
            let (fin, special) = crate::ops::agg::numeric::num_word::sentinel(b, env, m, ok);
            Val { raw: Some((m, lv(b, ok))), ..leaf(fin, Some(special), ok) }
        }
        Tree::Int { c, typid, .. } => {
            let Some(cell) = row.col(*c) else { crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a numeric leaf the row does not carry" }) };
            let mut cx = env.lx(b, env.node);
            let (v, null) = crate::rep::datum(&mut cx, cell, &crate::step::expr::tf_datum(*typid));
            env.memo = cx.memo;
            leaf(v, None, b.int_bin(IntBinOp::Xor, null, yes))
        }
        Tree::Cast { x, scale, digits, typmod } => {
            let vx = finite(b, env, row, x, memo, live);
            let on = lv(b, vx.ok);
            let lo = cast(b, env, &vx, *scale - scale_of(x), *digits, *typmod, on);
            Val { lo, hi: None, sp: None, ok: vx.ok, kids: mcx::pg_vec![in mcx; vx], sel: PgVec::new_in(mcx), ds: None, raw: None }
        }
        // D4567 (a): x x c, exact in one word (decompose's bound); the display scale by |x|'s mantissa against the steps
        // (select_div_scale's, falling as |x| grows: one compare and select a step)
        Tree::Div { x, c, scales, .. } => {
            let vx = finite(b, env, row, x, memo, live);
            let k = b.iconst(Type::I64, *c);
            let lo = b.int_bin(IntBinOp::Mul, vx.lo, k);
            let neg = b.icmp(IntCond::Slt, vx.lo, env.zero);
            let flip = b.int_bin(IntBinOp::Sub, env.zero, vx.lo);
            let a = b.select(neg, flip, vx.lo);
            let mut ds = b.iconst(Type::I64, scales.zero as i64);
            for &(from, sc) in scales.steps() {
                let (f, s) = (b.iconst(Type::I64, from as i64), b.iconst(Type::I64, sc as i64));
                let past = b.icmp(IntCond::Uge, a, f);
                ds = b.select(past, s, ds);
            }
            Val { lo, hi: None, sp: None, ok: vx.ok, kids: mcx::pg_vec![in mcx; vx], sel: PgVec::new_in(mcx), ds: Some(ds), raw: None }
        }
        Tree::Mul(x, y) | Tree::Add(x, y) | Tree::Sub(x, y) => {
            let (vx, vy) = (finite(b, env, row, x, memo, live), finite(b, env, row, y, memo, live));
            let ok = b.int_bin(IntBinOp::And, vx.ok, vy.ok);
            let on = lv(b, ok);
            let narrow = digits_of(t) <= NARROW;
            let (lo, hi) = match t {
                Tree::Mul(..) if narrow => (b.int_bin(IntBinOp::Mul, vx.lo, vy.lo), None),
                Tree::Mul(..) => match (vx.hi, vy.hi) {
                    (None, None) => (b.int_bin(IntBinOp::Mul, vx.lo, vy.lo), Some(b.int_bin(IntBinOp::Smulh, vx.lo, vy.lo))),
                    (Some(h), None) | (None, Some(h)) => {
                        let (w, s) = if vx.hi.is_some() { (vx.lo, vy.lo) } else { (vy.lo, vx.lo) };
                        let (lo, hi) = mul_wide(b, env, w, h, s, on);
                        (lo, Some(hi))
                    }
                    (Some(_), Some(_)) => crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a numeric product of two wide nodes" }),
                },
                _ => {
                    let s = scale_of(t);
                    let (xl, xh) = rescale(b, env, &vx, s - scale_of(x), narrow, on);
                    let (yl, yh) = rescale(b, env, &vy, s - scale_of(y), narrow, on);
                    let sub = matches!(t, Tree::Sub(..));
                    match narrow {
                        true => (b.int_bin(if sub { IntBinOp::Sub } else { IntBinOp::Add }, xl, yl), None),
                        false => {
                            let (xh, yh) = (xh.unwrap_or_else(|| sext(b, xl)), yh.unwrap_or_else(|| sext(b, yl)));
                            let (yl, yh) = if sub { neg128(b, env, yl, yh) } else { (yl, yh) };
                            let (lo, hi) = crate::ops::agg::trans::class_word::add128(b, xl, xh, yl, yh);
                            (lo, Some(hi))
                        }
                    }
                }
            };
            Val { lo, hi, sp: None, ok, kids: mcx::pg_vec![in mcx; vx, vy], sel: PgVec::new_in(mcx), ds: None, raw: None }
        }
    };
    memo.put(t, v.clone());
    v
}

/// PS4-21: a one-word node `v` as numeric(p, s) holds it (`cast_word`, `k` = s less its scale); where `on` and no leaf
/// below is special (NaN passes a typmod), a value past p digits raises apply_typmod's numeric field overflow for `typmod`.
fn cast(b: &mut FunctionBuilder<'_>, env: &Env, v: &Val, k: i16, p: u32, typmod: i32, on: Value) -> Value {
    fires::fire("num.cast"); // shapecheck fire signal (REGISTRY PS4-21: a typmod cast as a num.window node)
    let (q, over) = cast_word(b, v.lo, k, p);
    let mut flags = mcx::PgVec::new_in(env.mcx);
    leaf_codes(v, &mut flags);
    let yes = b.bconst(true);
    let fin = flags.iter().fold(yes, |a, &c| {
        let n = b.int_bin(IntBinOp::Xor, c, yes);
        b.int_bin(IntBinOp::And, a, n)
    });
    let bad = b.int_bin(IntBinOp::And, over, on);
    let bad = b.int_bin(IntBinOp::And, bad, fin);
    crate::ops::agg::numeric::num_word::raise_typmod(b, env, bad, typmod);
    q
}

/// `lo` (under 10^18) times 10^k, or rounded half away from zero by 10^-k (round_var: |lo| + 10^-k / 2 over 10^-k, the
/// sign put back), and whether the result reaches 10^p; for k >= 0 the bound is tested on `lo` first, so the product it
/// passes stays in the word.
fn cast_word(b: &mut FunctionBuilder<'_>, lo: Value, k: i16, p: u32) -> (Value, Value) {
    let zero = b.iconst(Type::I64, 0);
    let neg = b.icmp(IntCond::Slt, lo, zero);
    let m = b.int_bin(IntBinOp::Sub, zero, lo);
    let a = b.select(neg, m, lo);
    if k >= 0 {
        let lim = b.iconst(Type::I64, p.checked_sub(k as u32).map_or(1, |d| 10i64.pow(d)));
        let f = b.iconst(Type::I64, 10i64.pow(k as u32));
        return (b.int_bin(IntBinOp::Mul, lo, f), b.icmp(IntCond::Uge, a, lim));
    }
    let d = 10i64.pow(k.unsigned_abs() as u32);
    let (dv, half) = (b.iconst(Type::I64, d), b.iconst(Type::I64, d / 2));
    let r = b.int_bin(IntBinOp::Add, a, half);
    let r = b.int_bin(IntBinOp::Udiv, r, dv);
    let lim = b.iconst(Type::I64, 10i64.pow(p));
    let nr = b.int_bin(IntBinOp::Sub, zero, r);
    (b.select(neg, nr, r), b.icmp(IntCond::Uge, r, lim))
}

fn neg128(b: &mut FunctionBuilder<'_>, env: &Env, lo: Value, hi: Value) -> (Value, Value) {
    let nlo = b.int_bin(IntBinOp::Sub, env.zero, lo);
    let nz = b.icmp(IntCond::Ne, lo, env.zero);
    let borrow = b.conv(gravity_ir::ConvOp::Zext, Type::I64, nz);
    let h = b.int_bin(IntBinOp::Sub, env.zero, hi);
    (nlo, b.int_bin(IntBinOp::Sub, h, borrow))
}

/// Tree `t`'s term for the row: its finite value and, when a leaf is special, the tree's specials code (the cold block).
pub(crate) fn term<'t>(b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, t: &'t Tree, memo: &mut crate::ops::agg::numeric::strategies::memo_num::Memo<'t>) -> crate::ops::agg::numeric::num_word::Term {
    let v = finite(b, env, row, t, memo, None);
    let mut flags = mcx::PgVec::new_in(env.mcx);
    leaf_codes(&v, &mut flags);
    let sp = match flags.as_slice() {
        [] => None,
        [c0, rest @ ..] => {
            let special = rest.iter().fold(*c0, |a, &c| b.int_bin(IntBinOp::Or, a, c));
            let (l_slow, l_join) = (b.create_block(&[]), b.create_block(&[Type::I64]));
            b.cond_br(special, (l_slow, &[]), (l_join, &[env.zero]));
            b.switch_to(l_slow);
            let code = crate::ops::agg::numeric::strategies::memo_num::code(b, env, t, &v);
            b.br(l_join, &[code]);
            b.switch_to(l_join);
            Some(b.block_params(l_join)[0])
        }
    };
    crate::ops::agg::numeric::num_word::Term { lo: v.lo, hi: v.hi, sp, ok: v.ok, ds: v.ds }
}

pub(crate) fn leaf_codes(v: &Val, out: &mut PgVec<'_, Value>) {
    crate::stack_check();
    if let Some(c) = v.sp {
        out.push_in(c);
    }
    v.kids.iter().for_each(|k| leaf_codes(k, out));
}

impl<'v> Val<'v> {
    pub(crate) fn parts(&self) -> (Value, Option<Value>, Option<Value>, &[Val<'v>]) {
        (self.lo, self.hi, self.sp, &self.kids)
    }

    pub(crate) fn case(lo: Value, hi: Option<Value>, ok: Value, kids: PgVec<'v, Val<'v>>, sel: PgVec<'v, Value>, ds: Option<Value>) -> Val<'v> {
        Val { lo, hi, sp: None, ok, kids, sel, ds, raw: None }
    }
}

impl Clone for Val<'_> {
    fn clone(&self) -> Self {
        Val { kids: mcx::to_vec_in(&self.kids[..], *self.kids.allocator()), sel: mcx::to_vec_in(&self.sel[..], *self.sel.allocator()), ..*self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D2356: a constant past one word is a 128-bit leaf: its words are its mantissa's; `c + k` rescales it exactly
    /// (mul_wide), `k - c` negates it, `c × k` against a one-word `k` is the exact i128.
    #[test]
    fn a_constant_past_one_word_folds_as_a_128_bit_leaf() {
        let c: i128 = 136_193_145_968_274_513_351; // 13619314596827451335.1 at scale 1
        let cx = mcx::MemoryContext::new("d2356");
        let wide = |m| mcx::box_in(Tree::PastWord { m: Some(m), scale: 1, digits: 21 }, cx.mcx());
        let k = |m, scale| mcx::box_in(Tree::Const { m, scale, digits: 3 }, cx.mcx());
        let run = |t: &Tree| -> i128 {
            let mut half = |hi: bool| {
                crate::frame::stop::tests::run1(&[], &mut |b, env| {
                    let row = RowVals { row: crate::cell::Row::new_in(env.mcx), vals: mcx::PgVec::new_in(env.mcx), cols: mcx::PgVec::new_in(env.mcx), carry: crate::frame::row::Carry::new(env.zero, env.mcx), next: env.zero, at: env.zero };
                    let mut memo = crate::ops::agg::numeric::strategies::memo_num::Memo::new(false, env.mcx);
                    let v = finite(b, env, &row, t, &mut memo, None);
                    if hi { v.hi.unwrap_or_else(|| sext(b, v.lo)) } else { v.lo }
                })
            };
            ((half(true) as i128) << 64) | (half(false) as u64 as i128)
        };
        assert_eq!(run(&Tree::PastWord { m: Some(-c), scale: 1, digits: 21 }), -c);
        assert_eq!(run(&Tree::Add(wide(c), k(125, 2))), c * 10 + 125);
        assert_eq!(run(&Tree::Sub(k(125, 2), wide(c))), 125 - c * 10);
        assert_eq!(run(&Tree::Mul(wide(-c), k(-7, 0))), c * 7);
    }

    /// PS4-21: a typmod cast's word is apply_typmod's (numeric_in of the value's text under the typmod): rounded half away
    /// from zero at both signs (a carry digit too), scaled up exactly, past p digits flagged (the node raises), a
    /// numeric(3, 4) holding only a zero integer.
    #[test]
    fn a_typmod_cast_word_is_apply_typmods_value() {
        let text = |m: i64, s: u32| {
            let a = format!("{:0>w$}", m.unsigned_abs(), w = s as usize + 1);
            let (i, f) = a.split_at(a.len() - s as usize);
            format!("{}{i}{}{f}", if m < 0 { "-" } else { "" }, if s > 0 { "." } else { "" })
        };
        let ms: [i64; 16] = [0, 1, -1, 49, 50, -50, 51, 99, 123_449, 123_450, -123_450, 999_999, -1_000_000, 99_999_999_995, 999_999_999_999_950, -999_999_999_999_999_999];
        for (sx, p, s) in [(4u32, 12u32, 2u32), (4, 6, 2), (4, 10, 0), (2, 8, 4), (0, 3, 4), (2, 12, 2), (0, 18, 0), (18, 18, 17)] {
            let typmod = ((p << 16) | s) as i32 + 4;
            let mut pb = gravity_ir::builder::ProgramBuilder::new();
            let f = pb.declare_func("t", &[Type::I64], &[Type::I64; 2]);
            let mut b = pb.define(f);
            let x = b.params()[0];
            let (q, over) = cast_word(&mut b, x, s as i16 - sx as i16, p);
            let over = b.conv(gravity_ir::ConvOp::Zext, Type::I64, over);
            b.ret(&[q, over]);
            let prog = pb.finish();
            for m in ms {
                let got = gravity_interp::Interp::new(&prog).run(f, &[m as u64]).expect("runs");
                let want = adt_numeric::numeric_in(&text(m, sx), typmod, None).map(|n| n.and_then(|n| gravity_fn::typesem::numeric::mantissa(&n.as_bytes()[4..])));
                match want {
                    Ok(Some(n)) => assert_eq!((got[0] as i64 as i128, got[1], n.dscale), (n.value.expect("a word"), 0, s as i32), "{} as numeric({p}, {s})", text(m, sx)),
                    Ok(None) => panic!("{}: no value", text(m, sx)),
                    Err(_) => assert_eq!(got[1], 1, "{} as numeric({p}, {s}) overflows", text(m, sx)),
                }
            }
        }
    }

    /// The 128 × 64 product is the exact i128 product's low 128 bits, `fits` exactly when nothing is lost.
    #[test]
    fn mul128x64_is_exact() {
        let xs: [i128; 7] = [0, 1, -1, 12_345_678_901_234_567_890_123, -98_765_432_109_876_543_210, i64::MAX as i128 + 7, i128::MAX / 3];
        let ts: [i64; 6] = [0, 1, -1, 10_000, -1_000_000_007, i64::MAX];
        for x in xs {
            for t in ts {
                let mut pb = gravity_ir::builder::ProgramBuilder::new();
                let f = pb.declare_func("t", &[Type::I64; 3], &[Type::I64; 3]);
                let mut b = pb.define(f);
                let p = b.params().to_vec();
                let (lo, hi, fits, _) = mul128x64(&mut b, p[0], p[1], p[2]);
                let fits = b.conv(gravity_ir::ConvOp::Zext, Type::I64, fits);
                b.ret(&[lo, hi, fits]);
                let prog = pb.finish();
                let got = gravity_interp::Interp::new(&prog).run(f, &[x as u64, (x >> 64) as u64, t as u64]).expect("runs");
                let want = x.checked_mul(t as i128);
                let w = x.wrapping_mul(t as i128);
                assert_eq!((got[0], got[1]), (w as u64, (w >> 64) as u64), "{x} * {t}");
                assert_eq!(got[2] == 1, want.is_some(), "{x} * {t} fits");
            }
        }
    }
}
