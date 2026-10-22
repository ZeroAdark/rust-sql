//! The num.window tree fold of a keyed end and class.num_wide's term (V4R-38), split from fold/plain.rs,
//! fold/num_window.rs and keyed/hashagg.rs (budget). Hooks / levers: road:num.window. Budget 150 lines (files-extra.tsv,
//! +25 % gate). Owner M1-FOLD. Registers nothing.
//!
//! A Hashed or Sorted group's end folds a packed numeric sum / avg whose tree passes one mantissa word (D297's wide case, which
//! class_of.rs `class_in` otherwise gives class.num_varlena), a narrow one whose argument is more than a column (a
//! product, a sum, a typmod cast, a CASE: A-24c (c), PS4-21, D3233) and a class.num_wide product as num.window trees, as
//! a plain end does: `compile` binds the tree in place of the
//! datum argument (group_rec.rs), `step` folds it (hashagg.rs).
//! class.num_wide's term is its wide tree (num_tree `compile_wide`) as its exact i256: a product the wide factor's i128
//! times the one-word factor, 192 bits sign-extended; a sum / difference (OID-04) its sides rescaled and added in 256
//! bits; never a raise; its specials code as num_window `term` works it out.
use gravity_decompose::ops::shared::agg::{AggDecl, GroupDecl, GroupForm, Register};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use mcx::{CollectIn, GrowIn, Mcx, PgVec};

use crate::face::fold::{Acc, FoldClass, WideTerm};
use crate::face::kinds::KindId;
use crate::face::strategy::NUM_WINDOW;
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::frame::row::RowVals;
use crate::admit::refusal::{Reason, Variant};

use crate::ops::agg::numeric::strategies::memo_num::Memo;
use crate::ops::agg::numeric::strategies::num_tree::{bounded, digits_of, scale_of, Tree, NARROW};
use crate::ops::agg::numeric::strategies::num_window::{finite, leaf_codes, mul128x64, sext, Val};
use crate::ops::agg::trans::plain::of_agg;
use crate::props::class_kind;
use crate::ops::agg::hashagg::{AccAt, Agg, GroupRec};

type Trees<'g, 'p> = Option<(Tree<'g>, PgVec<'g, types_nodes::Node<'p>>)>;

/// Whether `g`'s end folds num.window trees: a Hashed group's, the road on (a plain end's: fold/plain.rs).
pub(crate) fn on(px: &Px<'_, '_>, g: &GroupDecl<'_>) -> bool {
    forms(g, px.mcx()) && px.strategies.has(NUM_WINDOW) && crate::face::source::on(Variant::Strategy(NUM_WINDOW))
}

/// The keyed group forms whose end folds trees: Hashed; Sorted (runs.group's end is keyed.hashagg's over the same record,
/// ops/group/lower.rs) where a tree takes one of its aggregates, so a Sorted group without one keeps its record as it was
/// (code.front_light reads a record with no tree list).
pub(crate) fn forms(g: &GroupDecl<'_>, mcx: Mcx<'_>) -> bool {
    g.form == GroupForm::Hashed || g.form == GroupForm::Sorted && g.aggs.iter().any(|a| keyed_tree(a, mcx).is_some())
}

/// The tree a keyed end folds for `a`: a class.num_wide product, a packed argument past one mantissa word, or a narrow
/// packed argument that is more than a column. Its datum step ran the expression in PostgreSQL numeric a row and read the
/// result's datum back: a product or a sum of columns (A-24c (c): one narrow product's sum cost ~107 ns a row over the
/// tree's word, 4M heap rows on box gencoll), a typmod cast (PS4-21: through varlena), a CASE / COALESCE (D3233; the tree
/// takes num_case.rs's masked add, as a plain end does), a division by a constant (D4567 (a): x x c and a dscale word). A
/// bare column keeps its column step (num.lean's word).
pub(crate) fn keyed_tree<'g, 'p>(a: &AggDecl<'p>, mcx: Mcx<'g>) -> Trees<'g, 'p> {
    let e = a.node.as_aggref().and_then(|r| r.args.iter().next()).map(|t| t.as_target_entry().map_or(t, |t| t.expr))?;
    match class_kind(a) {
        KindId::ClassNumWide => of_wide(a, mcx),
        KindId::ClassNumPacked if crate::ops::agg::numeric::strategies::num_tree::wide(e, mcx) => of_agg(a, mcx),
        KindId::ClassNumPacked => of_agg(a, mcx).filter(|t| !matches!(t.0, Tree::Col { .. })),
        _ => None,
    }
}

/// A class.num_wide decl's argument as its num.window wide tree (num_tree `compile_wide`) with its WHEN nodes.
pub(crate) fn of_wide<'g, 'p>(a: &AggDecl<'p>, mcx: Mcx<'g>) -> Trees<'g, 'p> {
    let r = a.node.as_aggref().filter(|r| r.args.len() == 1 && matches!(a.reg, Register::SumNumWide | Register::AvgNumWide))?;
    let e = r.args.iter().next().map(|t| t.as_target_entry().map_or(t, |t| t.expr))?;
    let mut whens = PgVec::new_in(mcx);
    crate::ops::agg::numeric::strategies::num_tree::compile_wide(e, a.scale, &mut whens, mcx).map(|t| (t, whens))
}

/// group_rec.rs `compile_args`: `a`'s tree (`keyed_tree`) bound over the producer's row, pushed to `nums` with its FILTER
/// compiled; false (nothing compiled) where the end folds no trees or none takes `a`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compile<'g, 'p>(nums: &mut Option<PgVec<'g, (i16, Tree<'g>, bool)>>, a: &mut Agg<'p>, px: &mut Px<'g, 'p>, producer: usize, nops: usize, node: i32, image_only: bool) -> bool {
    let Some(nums) = nums.as_mut().filter(|_| !image_only) else { return false };
    let mcx = px.mcx();
    let Some((mut t, whens)) = keyed_tree(&a.d, mcx) else { return false };
    let ids: PgVec<'_, _> = whens.into_iter().map(|w| crate::step::expr::compile_at(px, producer, nops, w, None, node)).collect_in(mcx);
    crate::ops::agg::numeric::strategies::num_tree::bind(&mut t, &ids[..]);
    nums.push_in((a.d.aggno, t, class_kind(&a.d) == KindId::ClassNumWide));
    fires::fire("num.window.keyed"); // shapecheck fire signal (A-24c (c): a keyed end folds an aggregate's num.window tree)
    a.filter = a.d.filter.map(|f| crate::step::expr::compile_at(px, producer, nops, f, None, node));
    true
}

/// One row's tree term folded into `acc`: a wide product's i256 by its class (`fold_wide`), a packed one by
/// class.num_packed's fold under V4R-68's window guard `g` (`None`: add_sum per row; a wide sum is never guarded).
#[allow(clippy::too_many_arguments)]
pub(crate) fn fold_term<'t>(b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, a: &AggDecl<'_>, class: &dyn FoldClass, t: &'t Tree, wide: bool, memo: &mut Memo<'t>, acc: Acc<'_>, g: Option<Value>) {
    if wide {
        // D619: packed windows take the lean i256 add; the generic fold below runs for a verbatim one, its subtrees kept
        let (lean, mark) = (crate::ops::agg::numeric::strategies::num_lean::wide(b, env, row, a, class, t, &acc), memo.len());
        let term = wide_term(b, env, row, t, memo);
        class.fold_wide(b, env, a, acc, &term);
        if lean.is_some() {
            memo.truncate(mark);
        }
        crate::ops::agg::numeric::strategies::num_lean::join(b, env, lean);
    } else {
        // D3224 A1: likewise a packed window's tree term folds without the specials (num_lean.rs `tree`)
        let (lean, mark) = (crate::ops::agg::numeric::strategies::num_lean::tree(b, env, row, a, t, &acc, g), memo.len());
        let term = crate::ops::agg::numeric::strategies::num_window::term(b, env, row, t, memo);
        crate::ops::agg::numeric::class_num::fold(b, env, a, acc, &term, g);
        if lean.is_some() {
            memo.truncate(mark);
        }
        crate::ops::agg::numeric::strategies::num_lean::join(b, env, lean);
    }
}

/// hashagg.rs `step`: `a`'s tree (`compile`) folded into `acc` under its FILTER; `acc` back where `a` folds no tree.
pub(crate) fn step<'a>(gr: &GroupRec<'_, '_>, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, a: &Agg<'_>, acc: AccAt<'a>) -> Option<AccAt<'a>> {
    let Some((_, t, wide)) = gr.nums.iter().flatten().find(|n| n.0 == a.d.aggno) else { return Some(acc) };
    let fold = |b: &mut FunctionBuilder<'_>, env: &mut Env, acc: Acc<'_>| {
        let mcx = env.mcx;
        let mut memo = Memo::new(false, mcx);
        // D3233 (road case.select): a CASE with a NULL arm folds only the rows that take a non-NULL one
        match crate::ops::agg::numeric::strategies::num_case::select(b, env, row, t, mcx) {
            None => fold_term(b, env, row, &a.d, a.class, t, *wide, &mut memo, acc, None),
            Some((at, sel, on)) => {
                memo.sel = Some((at, sel));
                crate::ops::agg::trans::filter::acc_when(b, env, on, acc, |b, env, acc| fold_term(b, env, row, &a.d, a.class, t, *wide, &mut memo, acc, None));
            }
        }
    };
    let pred = |b: &mut FunctionBuilder<'_>, env: &mut Env| a.filter.map(|ex| crate::step::expr::pred(b, env, gr.node, row, ex));
    match acc {
        AccAt::Slot(base, slot, _) => {
            let acc = gr.slot(base, slot);
            let Some(p) = pred(b, env) else {
                fold(b, env, acc);
                return None;
            };
            let (l_do, l_join) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(p, (l_do, &[]), (l_join, &[]));
            b.switch_to(l_do);
            env.aside(|env| fold(b, env, acc));
            b.br(l_join, &[]);
            b.switch_to(l_join);
        }
        AccAt::Regs(r) => {
            let Some(p) = pred(b, env) else {
                fold(b, env, Acc::Regs(r));
                return None;
            };
            let types: PgVec<'_, Type> = r.iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
            let (l_do, l_join) = (b.create_block(&[]), b.create_block(&types[..]));
            b.cond_br(p, (l_do, &[]), (l_join, r));
            b.switch_to(l_do);
            let mut s = mcx::to_vec_in(&r[..], env.mcx);
            env.aside(|env| fold(b, env, Acc::Regs(&mut s[..])));
            b.br(l_join, &s[..]);
            b.switch_to(l_join);
            r.copy_from_slice(b.block_params(l_join));
        }
    }
    None
}

/// class.num_wide's term for the row: the root wide node (num_tree `compile_wide`) as its exact i256 (`wide_val`) and its
/// specials code (see the module doc).
pub(crate) fn wide_term<'t>(b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, t: &'t Tree, memo: &mut Memo<'t>) -> WideTerm {
    let (w, v) = wide_val(b, env, row, t, memo);
    let ok = v.ok;
    let mut flags: PgVec<'_, Value> = PgVec::new_in(env.mcx);
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
    WideTerm { w, sp, ok }
}

/// Node `t`'s exact value for the row as four words of two's complement at its scale, with its `Val` for the specials
/// code (memo_num `code` reads a product's factors and a sum's sides as its kids). A wide product (past `bounded`): the
/// wide factor's i128 times the one-word factor, 192 bits sign-extended; a wide sum / difference (OID-04): its sides'
/// words rescaled to its scale (`times10`) and added in 256 bits (`add256`), exact under the 55-digit bound
/// (num_tree `compile_wide`); any other node: its `finite` value sign-extended. Never a raise.
fn wide_val<'t>(b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, t: &'t Tree, memo: &mut Memo<'t>) -> ([Value; 4], Val<'t>) {
    let mcx = memo.mcx;
    let val = |w: [Value; 4], ok, kids| Val { lo: w[0], hi: Some(w[1]), sp: None, ok, kids, sel: PgVec::new_in(mcx), ds: None, raw: None };
    match t {
        Tree::Mul(x, y) if !bounded(t) => {
            let (vx, vy) = (finite(b, env, row, x, memo, None), finite(b, env, row, y, memo, None));
            let (w, s) = if digits_of(y) <= NARROW { (&vx, vy.lo) } else { (&vy, vx.lo) };
            let wh = w.hi.unwrap_or_else(|| sext(b, w.lo));
            let (p0, p1, _, p2) = mul128x64(b, w.lo, wh, s);
            let p3 = sext(b, p2);
            let ok = b.int_bin(IntBinOp::And, vx.ok, vy.ok);
            let w = [p0, p1, p2, p3];
            (w, val(w, ok, mcx::pg_vec![in mcx; vx, vy]))
        }
        Tree::Add(x, y) | Tree::Sub(x, y) if !bounded(t) => {
            let s = scale_of(t);
            let ((wx, vx), (wy, vy)) = (wide_val(b, env, row, x, memo), wide_val(b, env, row, y, memo));
            let (wx, wy) = (times10(b, wx, s - scale_of(x)), times10(b, wy, s - scale_of(y)));
            let wy = if matches!(t, Tree::Sub(..)) { neg256(b, env, wy) } else { wy };
            let w = add256(b, wx, wy);
            let ok = b.int_bin(IntBinOp::And, vx.ok, vy.ok);
            (w, val(w, ok, mcx::pg_vec![in mcx; vx, vy]))
        }
        _ => {
            let v = finite(b, env, row, t, memo, None);
            let hi = v.hi.unwrap_or_else(|| sext(b, v.lo));
            let top = sext(b, hi);
            ([v.lo, hi, top, top], v)
        }
    }
}

/// `w` x 10^k (k at most 18, so 10^k < 2^63) in 256 bits: each word's unsigned product with its carry, the two's
/// complement product modulo 2^256, exact where the true product fits (`wide_val`'s bound).
fn times10(b: &mut FunctionBuilder<'_>, w: [Value; 4], k: i16) -> [Value; 4] {
    if k == 0 {
        return w;
    }
    let f = b.iconst(Type::I64, 10i64.pow(k as u32));
    let mut out = w;
    let mut carry: Option<Value> = None;
    for (o, &x) in out.iter_mut().zip(w.iter()) {
        let (lo, hi) = (b.int_bin(IntBinOp::Mul, x, f), b.int_bin(IntBinOp::Umulh, x, f));
        *o = lo;
        let mut hi = hi;
        if let Some(c) = carry {
            *o = b.int_bin(IntBinOp::Add, lo, c);
            let cy = b.icmp(IntCond::Ult, *o, lo);
            let cy = b.conv(ConvOp::Zext, Type::I64, cy);
            hi = b.int_bin(IntBinOp::Add, hi, cy);
        }
        carry = Some(hi);
    }
    out
}

/// `x + y` in 256 bits (two's complement, the carries rippled; the caller's bound keeps it exact).
fn add256(b: &mut FunctionBuilder<'_>, x: [Value; 4], y: [Value; 4]) -> [Value; 4] {
    let mut out = x;
    let mut k: Option<Value> = None;
    for i in 0..4 {
        let s = b.int_bin(IntBinOp::Add, x[i], y[i]);
        let c1 = b.icmp(IntCond::Ult, s, x[i]);
        let (s, c) = match k {
            None => (s, c1),
            Some(k) => {
                let s2 = b.int_bin(IntBinOp::Add, s, k);
                let c2 = b.icmp(IntCond::Ult, s2, s);
                (s2, b.int_bin(IntBinOp::Or, c1, c2))
            }
        };
        out[i] = s;
        k = Some(b.conv(ConvOp::Zext, Type::I64, c));
    }
    out
}

/// `-w` in 256 bits: its complement plus one.
fn neg256(b: &mut FunctionBuilder<'_>, env: &Env, w: [Value; 4]) -> [Value; 4] {
    let m = b.iconst(Type::I64, -1);
    let mut inv = w;
    for x in inv.iter_mut() {
        *x = b.int_bin(IntBinOp::Xor, *x, m);
    }
    add256(b, inv, [env.one, env.zero, env.zero, env.zero])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(x: i128) -> [u64; 4] {
        let s = if x < 0 { u64::MAX } else { 0 };
        [x as u64, (x >> 64) as u64, s, s]
    }

    fn mul(a: [u64; 4], f: u64) -> [u64; 4] {
        let (mut out, mut c) = ([0u64; 4], 0u128);
        for i in 0..4 {
            let p = a[i] as u128 * f as u128 + c;
            (out[i], c) = (p as u64, p >> 64);
        }
        out
    }

    fn add(a: [u64; 4], b: [u64; 4]) -> [u64; 4] {
        let (mut out, mut c) = ([0u64; 4], 0u128);
        for i in 0..4 {
            let s = a[i] as u128 + b[i] as u128 + c;
            (out[i], c) = (s as u64, s >> 64);
        }
        out
    }

    fn neg(a: [u64; 4]) -> [u64; 4] {
        add(a.map(|x| !x), [1, 0, 0, 0])
    }

    /// `a * f` exactly in 256 bits.
    fn prod(a: i128, f: i64) -> [u64; 4] {
        let r = mul(ext(a), f.unsigned_abs());
        if f < 0 { neg(r) } else { r }
    }

    /// OID-04: a sum / difference over wide products (and a packed side) is its exact i256 at the larger scale
    /// (`wide_val`: each side rescaled by `times10`, added by `add256`, a subtrahend negated by `neg256`); `-x` is `0 - x`.
    #[test]
    fn a_sum_of_wide_products_is_its_exact_i256() {
        let (c1, c2): (i128, i128) = (123_456_789_012_345_678_901_234_567_890_123_456, -98_765_432_109_876_543_210_987_654_321_098_765);
        let (f1, f2): (i64, i64) = (987_654_321_098_765_432, -123_456_789_012_345_678);
        let cx = mcx::MemoryContext::new("oid04");
        let bx = |t| mcx::box_in(t, cx.mcx());
        let big = |m: i128, scale| bx(Tree::PastWord { m: Some(m), scale, digits: 36 });
        let k = |m: i64, scale| bx(Tree::Const { m, scale, digits: 18 });
        let run = |t: &Tree| -> [u64; 4] {
            let mut word = |i: usize| {
                crate::frame::stop::tests::run1(&[], &mut |b, env| {
                    let row = RowVals { row: crate::cell::Row::new_in(env.mcx), vals: mcx::PgVec::new_in(env.mcx), cols: mcx::PgVec::new_in(env.mcx), carry: crate::frame::row::Carry::new(env.zero, env.mcx), next: env.zero, at: env.zero };
                    let mut memo = Memo::new(false, env.mcx);
                    wide_term(b, env, &row, t, &mut memo).w[i]
                }) as u64
            };
            [word(0), word(1), word(2), word(3)]
        };
        // scales: c1 * f1 at 2 + 2 = 4, c2 * f2 at 3 + 0 = 3 (rescaled by 10 to 4)
        let (p1, p2) = (Tree::Mul(big(c1, 2), k(f1, 2)), Tree::Mul(big(c2, 3), k(f2, 0)));
        let (e1, e2) = (prod(c1, f1), mul(prod(c2, f2), 10));
        assert_eq!(run(&p1), e1, "a wide product");
        assert_eq!(run(&Tree::Add(bx(Tree::Mul(big(c1, 2), k(f1, 2))), bx(Tree::Mul(big(c2, 3), k(f2, 0))))), add(e1, e2), "x * y + z * w");
        assert_eq!(run(&Tree::Sub(bx(Tree::Mul(big(c1, 2), k(f1, 2))), bx(Tree::Mul(big(c2, 3), k(f2, 0))))), add(e1, neg(e2)), "x * y - z * w");
        assert_eq!(run(&Tree::Sub(bx(Tree::Mul(big(c2, 3), k(f2, 0))), bx(Tree::Mul(big(c1, 2), k(f1, 2))))), add(e2, neg(e1)), "z * w - x * y");
        assert_eq!(run(&Tree::Sub(k(0, 4), bx(p2))), neg(e2), "-(z * w) as 0 - z * w");
        // a packed side (a 36-digit constant at scale 1) rescaled by 10^3 and added to a wide product
        assert_eq!(run(&Tree::Add(bx(p1), big(c2, 1))), add(e1, mul(ext(c2), 1000)), "x * y + c");
    }
}
