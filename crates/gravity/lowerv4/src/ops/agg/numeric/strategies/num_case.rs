//! Numeric CASE nodes of the num.window tree. Hooks / levers: H14 R2-L07. Budget 110 lines (files-extra.tsv, +25 % gate).
//! Owner M1-NUMERIC.
//!
//! Split from num_tree.rs / num_window.rs by responsibility (D267): a searched CASE of tree arms,
//! the kernel's masked add. Its WHENs compile through step_expr; the first is read on every row, a later one only where
//! no earlier one held (PostgreSQL's order). The arms run branch-free, the taken one selected at the node's scale, an
//! arm's raises masked by it taken; an arm another unfiltered fold reads whole (num.memo) runs unmasked, shared. A root
//! CASE's arms may differ in scale (D256): its dscale word is the taken arm's. Folds through road num.window.
//!
//! Registers: road `case.select` (`CASE_SELECT`, D3233, on by default): a root CASE with a NULL arm (a missing ELSE included)
//! is the rows it folds: the fold reads its selectors first (`select`) and runs only where a non-NULL arm is taken, as
//! nodeAgg skips a NULL input, so a row that takes a NULL arm reads no arm and touches no accumulator; the arm's term is
//! read inside. The same road has gravity_expr's emitter run a CASE arm that computes anything only where it is taken
//! (emit/scalar.rs `CaseSelect`, set over a statement's lowering: frame/program.rs).
//!
//! The form read is normalize's one searched CASE form (decompose normalize's `case` entry: PostgreSQL's own passes produce
//! it, so normalize needs no rule): the parser spells a missing ELSE as a NULL constant, so `CASE WHEN c THEN x END` and
//! `CASE WHEN c THEN x ELSE NULL END` are one node, and the planner (clauses.c eval_const_expressions, CaseExpr) drops a
//! constant-false or NULL WHEN and folds a constant-true one. A NULL arm is that constant under its relabels (`null`).
//!
//! PS4-17: a COALESCE is the CASE `WHEN a1 IS NOT NULL THEN a1 .. ELSE an` (clauses.c drops its NULL constants and every
//! argument past a non-NULL one): its arms are its arguments, none with a WHEN; arm i is taken where its value is valid
//! and no earlier one's is, and its raises are masked by no earlier one valid (PostgreSQL reads it only there).
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, Type, Value};
use mcx::{GrowIn, Mcx, PgVec};
use types_nodes::Node as N;

use crate::ops::agg::numeric::strategies::num_tree::{digits_of, scale_of, Tree, NARROW};
use crate::ops::agg::numeric::strategies::num_window::Val;
use crate::frame::env::Env;
use crate::frame::px::ExId;
use crate::frame::row::RowVals;

crate::strategies! {
    /// D3233: a CASE as the rows it selects: an aggregate over a CASE with a NULL arm folds only where a non-NULL arm is
    /// taken, and an expression's CASE runs an arm that computes anything only where it is taken. Default off
    /// (road_off.rs `DEFAULT_OFF`): on under `+case.select`.
    CASE_SELECT = 145, "case.select", All;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(CASE_SELECT, &())], ..crate::ops::NONE };

#[derive(PartialEq)]
pub(crate) struct Case<'g> {
    /// The WHENs (condition, result), then the ELSE (no condition); a `None` result is a NULL arm.
    pub arms: PgVec<'g, (Option<ExId>, Option<Tree<'g>>)>,
    pub scale: i16,
    pub digits: u32,
    /// Arms of different scales (only at a sum's root, where the dscale word tracks the taken arm's).
    pub mixed: bool,
    /// A COALESCE: the first valid arm is taken (no WHENs).
    pub coalesce: bool,
}

/// The searched CASE or the COALESCE `e` with every non-NULL result a tree (`arm`); WHEN nodes pushed on `whens`, their
/// ids bound later.
pub(crate) fn compile<'g, 'p>(e: N<'p>, mixed_ok: bool, whens: &mut PgVec<'_, N<'p>>, arm: &mut dyn FnMut(N<'p>, &mut PgVec<'_, N<'p>>) -> Option<Tree<'g>>, mcx: Mcx<'g>) -> Option<Case<'g>> {
    let mut result = |r: Option<N<'p>>, w: &mut PgVec<'_, N<'p>>| match r.filter(|r| !null(*r)) {
        Some(r) => arm(r, w).map(Some),
        None => Some(None),
    };
    let mut arms = PgVec::new_in(mcx);
    let coalesce = e.as_coalesce_expr();
    if let Some(c) = coalesce {
        for a in c.args.iter() {
            arms.push_in((None, result(Some(a), whens)?));
        }
    } else {
        let c = e.as_case_expr().filter(|c| c.arg.is_none())?;
        for w in c.args.iter() {
            let w = w.as_case_when()?;
            whens.push_in(w.expr?);
            arms.push_in((Some(ExId(whens.len() as u32 - 1)), result(w.result, whens)?));
        }
        arms.push_in((None, result(c.defresult, whens)?));
    }
    let trees = || arms.iter().filter_map(|a| a.1.as_ref());
    let scale = trees().map(scale_of).max()?;
    let digits = trees().map(|t| digits_of(t) + (scale - scale_of(t)) as u32).max()?;
    let mixed = trees().any(|t| scale_of(t) != scale);
    (mixed_ok || !mixed).then_some(Case { arms, scale, digits, mixed, coalesce: coalesce.is_some() })
}

/// The parser's NULL arm (a missing ELSE included), under its relabels.
fn null(r: N<'_>) -> bool {
    crate::stack_check();
    r.as_relabel_type().map_or_else(|| r.as_const().is_some_and(|k| k.constisnull), |x| null(x.arg))
}

/// Case `c`'s value for the row (`live`: the enclosing arm taken, `None` everywhere).
pub(crate) fn finite<'t>(b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, c: &'t Case, memo: &mut crate::ops::agg::numeric::strategies::memo_num::Memo<'t>, live: Option<Value>) -> Val<'t> {
    let (f, t) = (b.bconst(false), b.bconst(true));
    let mcx = memo.mcx;
    let mut sel = match c.coalesce {
        true => {
            fires::fire("num.coalesce"); // shapecheck fire signal (REGISTRY PS4-17: a COALESCE of tree arms, its first valid one)
            mcx::reserved_in(c.arms.len(), mcx)
        }
        false => {
            fires::fire("num.case"); // shapecheck fire signal (REGISTRY FOLD-19: a CASE of tree arms in normalize's one CASE form)
            match memo.sel.as_ref().filter(|s| s.0 == c as *const Case as usize) {
                Some(s) => mcx::to_vec_in(&s.1[..], mcx),
                None => whens(b, env, row, c, live, (f, t), mcx),
            }
        }
    };
    let wide = c.digits > NARROW;
    let (mut lo, mut hi, mut ok, mut ds) = (env.zero, wide.then_some(env.zero), f, env.zero);
    let mut kids = PgVec::new_in(mcx);
    for (i, (_, arm)) in c.arms.iter().enumerate() {
        // a COALESCE's arm is read where no earlier one is valid (`ok` so far false), taken where it is valid there
        let open = if c.coalesce { b.int_bin(IntBinOp::Xor, ok, t) } else { sel[i] };
        let Some(a) = arm else {
            if c.coalesce {
                sel.push_in(f);
            }
            continue;
        };
        let on = live.map_or(open, |l| b.int_bin(IntBinOp::And, l, open));
        // an arm another fold emits unmasked is shared; else its raises are masked by `on` and its nodes forgotten after
        let v = match memo.covers(a) {
            true => crate::ops::agg::numeric::strategies::num_window::finite(b, env, row, a, memo, live),
            false => {
                let mark = memo.len();
                let v = crate::ops::agg::numeric::strategies::num_window::finite(b, env, row, a, memo, Some(on));
                memo.truncate(mark);
                v
            }
        };
        let s = match c.coalesce {
            true => {
                let s = b.int_bin(IntBinOp::And, v.ok, open);
                sel.push_in(s);
                s
            }
            false => open,
        };
        let okl = b.int_bin(IntBinOp::And, v.ok, on);
        let (l, h) = crate::ops::agg::numeric::strategies::num_window::rescale(b, env, &v, c.scale - scale_of(a), !wide, okl);
        lo = b.select(s, l, lo);
        if let Some(hv) = hi {
            let h = h.unwrap_or_else(|| crate::ops::agg::numeric::strategies::num_window::sext(b, l));
            hi = Some(b.select(s, h, hv));
        }
        ok = b.select(s, v.ok, ok);
        if c.mixed {
            let d = b.iconst(Type::I64, scale_of(a) as i64);
            ds = b.select(s, d, ds);
        }
        kids.push_in(v);
    }
    Val::case(lo, hi, ok, kids, sel, c.mixed.then_some(ds))
}

/// D3233 (road case.select): where tree `t` is a searched CASE with a NULL arm, its arm selectors for the row (`whens`) and
/// whether a non-NULL arm is taken; the fold branches on the latter and hands the selectors to `finite` (`Memo::sel`).
pub(crate) fn select<'m>(b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, t: &Tree, mcx: Mcx<'m>) -> Option<(usize, PgVec<'m, Value>, Value)> {
    let Tree::Case(c) = t else { return None };
    if c.coalesce || !c.arms.iter().any(|a| a.1.is_none()) || !env.applies(CASE_SELECT) {
        return None;
    }
    let (f, tr) = (b.bconst(false), b.bconst(true));
    let sel = whens(b, env, row, c, None, (f, tr), mcx);
    let on = c.arms.iter().zip(sel.iter()).filter(|(a, _)| a.1.is_some()).fold(f, |acc, (_, &s)| b.int_bin(IntBinOp::Or, acc, s));
    fires::fire("case.select"); // shapecheck fire signal (case.select: a CASE aggregate folds only its taken rows)
    Some((&**c as *const Case as usize, sel, on))
}

/// Whether a fold of tree `t` branches on its CASE's selection (`select`) under the strategy set `on`: its subtrees are
/// read inside the branch, so a shared memo must not keep them (plain.rs).
pub(crate) fn selects(t: &Tree, on: bool) -> bool {
    on && matches!(t, Tree::Case(c) if !c.coalesce && c.arms.iter().any(|a| a.1.is_none()))
}

/// A searched CASE's arm selectors: each WHEN's predicate, the ELSE taken where none held.
fn whens<'m>(b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, c: &Case, live: Option<Value>, (f, t): (Value, Value), mcx: Mcx<'m>) -> PgVec<'m, Value> {
    let node = env.node;
    let skip0 = live.map(|l| b.int_bin(IntBinOp::Xor, l, t));
    let (mut taken, mut sel) = (f, mcx::reserved_in(c.arms.len(), mcx));
    for (i, (w, _)) in c.arms.iter().enumerate() {
        let Some(w) = *w else {
            sel.push_in(b.int_bin(IntBinOp::Xor, taken, t));
            break;
        };
        // a WHEN is read only where no earlier one held (and, under an arm, where that arm is taken): PG's order
        let skip = if i == 0 { skip0 } else { Some(skip0.map_or(taken, |s| b.int_bin(IntBinOp::Or, s, taken))) };
        let p = match skip {
            None => crate::step::expr::pred(b, env, node, row, w),
            Some(s) => lazy(b, env, row, w, s),
        };
        sel.push_in(p);
        taken = b.int_bin(IntBinOp::Or, taken, p);
    }
    sel
}

/// WHEN `w`'s predicate where `skip` is false, false where it is true (PostgreSQL never reads it there).
fn lazy(b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, w: ExId, skip: Value) -> Value {
    let ty = b.value_type(skip);
    let (l_eval, l_join, f) = (b.create_block(&[]), b.create_block(&[ty]), b.bconst(false));
    b.cond_br(skip, (l_join, &[f]), (l_eval, &[]));
    b.switch_to(l_eval);
    // a datum read in the branch does not dominate the join: the row's memo is set aside and restored around it
    let memo = env.memo.set_aside();
    let node = env.node;
    let p = crate::step::expr::pred(b, env, node, row, w);
    env.memo.restore(memo);
    b.br(l_join, &[p]);
    b.switch_to(l_join);
    b.block_params(l_join)[0]
}

/// Case `c`'s specials code for the row: the taken arm's (a NULL arm's and a finite one's 0).
pub(crate) fn code(b: &mut FunctionBuilder<'_>, env: &Env, c: &Case, v: &Val) -> Value {
    let arms = c.arms.iter().zip(&v.sel).filter_map(|(a, s)| a.1.as_ref().map(|t| (t, *s)));
    let mut r = env.zero;
    for ((a, s), kid) in arms.zip(v.parts().3) {
        let k = crate::ops::agg::numeric::strategies::memo_num::code(b, env, a, kid);
        r = b.select(s, k, r);
    }
    r
}

#[cfg(test)]
mod tests {
    use datum::Datum;
    use mcx::{Mcx, MemoryContext};
    use types_nodes::list::NodeList;
    use types_nodes::primnodes::{CoalesceExpr, OpExpr, Var};

    use super::*;
    use crate::ops::agg::numeric::strategies::num_tree::compile as tree;

    fn m() -> Mcx<'static> {
        Box::leak(Box::new(MemoryContext::new("ps417"))).mcx() // alloc: Cold
    }
    fn num(m: Mcx<'static>, att: i16, p: i32, s: i32) -> N<'static> {
        N::mk(m, Var { varno: 1, varattno: att, vartype: 1700, vartypmod: ((p << 16) | s) + 4, ..Default::default() }).unwrap()
    }
    fn i4(m: Mcx<'static>, v: i32) -> N<'static> {
        N::mk_const(m, 23, -1, 0, 4, Datum::from_i32(v), false, true).unwrap()
    }
    /// A numeric constant's node over its image (leaked: the node holds its address).
    fn nk(m: Mcx<'static>, t: &str) -> N<'static> {
        let img = Box::leak(Box::new(adt_numeric::numeric_in(t, -1, None).unwrap().unwrap())); // alloc: Cold
        N::mk_const(m, 1700, -1, 0, -1, Datum::from_usize(img.as_bytes().as_ptr() as usize), false, false).unwrap()
    }
    fn coalesce(m: Mcx<'static>, args: &[N<'static>]) -> N<'static> {
        N::mk(m, CoalesceExpr { coalescetype: 1700, coalescecollid: 0, args: NodeList::from_slice(m, args).unwrap(), location: -1 }).unwrap()
    }

    /// PS4-17: a COALESCE of tree arguments is a CASE node whose arms are its arguments, none with a WHEN: its scale the
    /// largest, its digits each arm's at that scale. Does not apply: arms of different scales under an operator (only a
    /// sum's root tracks the dscale) unless beside a `+` / `-` side of a scale at least every arm's (D4567), an argument
    /// no tree takes (a numeric with no typmod).
    #[test]
    fn a_coalesce_is_a_case_of_its_arguments() {
        let m = m();
        let mut whens = mcx::PgVec::new_in(m);
        let Some(Tree::Case(c)) = tree(coalesce(m, &[num(m, 1, 12, 2), i4(m, 0)]), 2, &mut whens, m) else { panic!("coalesce(n2, 0) is a node") };
        assert!(c.coalesce && c.mixed && whens.is_empty());
        assert_eq!((c.scale, c.digits, c.arms.len()), (2, 12, 2));
        assert!(c.arms.iter().all(|a| a.0.is_none() && a.1.is_some()));
        let Some(Tree::Case(c)) = tree(coalesce(m, &[num(m, 1, 12, 2), num(m, 2, 14, 4), i4(m, 7)]), 4, &mut whens, m) else { panic!("three arguments") };
        assert_eq!((c.scale, c.digits, c.mixed), (4, 14, true));
        let add = |x, y| N::mk(m, OpExpr { opno: 1758, opfuncid: 1724, opresulttype: 1700, args: NodeList::from_slice(m, &[x, y]).unwrap(), ..Default::default() }).unwrap();
        assert!(matches!(tree(add(coalesce(m, &[num(m, 1, 12, 2), num(m, 3, 12, 2)]), num(m, 2, 12, 2)), 2, &mut whens, m), Some(Tree::Add(..))), "one scale under +");
        // D4567: mixed arm scales under + beside a side of a scale at least every arm's: the sum's dscale is that side's
        assert!(matches!(tree(add(coalesce(m, &[num(m, 1, 12, 2), i4(m, 0)]), num(m, 2, 12, 2)), 2, &mut whens, m), Some(Tree::Add(..))), "mixed scales under + beside scale 2");
        assert!(matches!(tree(add(num(m, 2, 12, 3), coalesce(m, &[num(m, 1, 12, 2), i4(m, 0)])), 3, &mut whens, m), Some(Tree::Add(..))), "beside scale 3, on the left");
        assert!(tree(add(coalesce(m, &[num(m, 1, 12, 2), i4(m, 0)]), num(m, 2, 12, 1)), 2, &mut whens, m).is_none(), "mixed scales under + beside scale 1");
        assert!(tree(add(coalesce(m, &[num(m, 1, 12, 2), i4(m, 0)]), coalesce(m, &[num(m, 2, 12, 2), i4(m, 0)])), 2, &mut whens, m).is_none(), "two mixed sides");
        let bare = N::mk(m, Var { varno: 1, varattno: 4, vartype: 1700, vartypmod: -1, ..Default::default() }).unwrap();
        assert!(tree(coalesce(m, &[num(m, 1, 12, 2), bare]), 2, &mut whens, m).is_none(), "an unscaled argument");
    }

    /// PS4-17: the first valid arm is taken, at the node's scale, with its own dscale: `COALESCE(NULL, 0.5, 0.07)` is 0.5
    /// (50 at scale 2, dscale 1); a NULL arm is never taken.
    #[test]
    fn a_coalesce_takes_its_first_valid_arm() {
        let k = |m, scale, digits| Some(Tree::Const { m, scale, digits });
        let c = Case { arms: mcx::pg_vec![in m(); (None, None), (None, k(5, 1, 1)), (None, k(7, 2, 1))], scale: 2, digits: 3, mixed: true, coalesce: true };
        let run = |part: u8| {
            crate::frame::stop::tests::run1(&[], &mut |b, env| {
                let row = RowVals { row: crate::cell::Row::new_in(env.mcx), vals: mcx::PgVec::new_in(env.mcx), cols: mcx::PgVec::new_in(env.mcx), carry: crate::frame::row::Carry::new(env.zero, env.mcx), next: env.zero, at: env.zero };
                let mut memo = crate::ops::agg::numeric::strategies::memo_num::Memo::new(false, env.mcx);
                let v = finite(b, env, &row, &c, &mut memo, None);
                match part {
                    0 => v.lo,
                    1 => v.ds.expect("mixed: a dscale word"),
                    _ => b.conv(gravity_ir::ConvOp::Zext, Type::I64, v.ok),
                }
            })
        };
        assert_eq!((run(0), run(1), run(2)), (50, 1, 1));
    }

    /// D4567 (a): `x / k` at a sum's root is a Div node when 1 / k is a finite decimal (1 / 2 = 5 / 10, 1 / 8 = 125 / 1000):
    /// x's scale plus the places, x x c's digits. Not for k = 3, a negative k, a column divisor, nor under an operator.
    #[test]
    fn a_division_by_a_constant_is_a_root_div() {
        let m = m();
        let mut whens = mcx::PgVec::new_in(m);
        let div = |x, k| N::mk(m, OpExpr { opno: 1761, opfuncid: 1727, opresulttype: 1700, args: NodeList::from_slice(m, &[x, k]).unwrap(), ..Default::default() }).unwrap();
        let add = |x, y| N::mk(m, OpExpr { opno: 1758, opfuncid: 1724, opresulttype: 1700, args: NodeList::from_slice(m, &[x, y]).unwrap(), ..Default::default() }).unwrap();
        let t = tree(div(num(m, 1, 7, 2), nk(m, "2")), 3, &mut whens, m).expect("x / 2");
        assert!(matches!(t, Tree::Div { c: 5, e: 1, .. }) && crate::ops::agg::numeric::strategies::num_tree::digits_of(&t) == 8, "x / 2 at scale 3, 8 digits");
        assert!(matches!(tree(div(num(m, 1, 7, 2), nk(m, "8")), 5, &mut whens, m), Some(Tree::Div { c: 125, e: 3, .. })), "x / 8");
        assert!(tree(div(num(m, 1, 7, 2), nk(m, "3")), 3, &mut whens, m).is_none(), "1 / 3 is no finite decimal");
        assert!(tree(div(num(m, 1, 7, 2), nk(m, "-2")), 3, &mut whens, m).is_none(), "a negative divisor");
        assert!(tree(div(num(m, 1, 7, 2), num(m, 2, 7, 0)), 2, &mut whens, m).is_none(), "a column divisor");
        assert!(tree(div(num(m, 1, 7, 2), i4(m, 2)), 3, &mut whens, m).is_none(), "an int4 Const (the planner folds its cast into a numeric one)");
        assert!(matches!(tree(div(num(m, 1, 7, 2), nk(m, "0.25")), 2, &mut whens, m), Some(Tree::Div { c: 4, e: 0, .. })), "x / 0.25");
        assert!(tree(add(div(num(m, 1, 7, 2), nk(m, "2")), num(m, 2, 7, 3)), 3, &mut whens, m).is_none(), "a division under +");
        // numeric(18, 2) / 2: a value past 10^15 divides at scale 2 (< 3): PostgreSQL rounds it, no tree
        assert!(tree(div(num(m, 1, 18, 2), nk(m, "2")), 3, &mut whens, m).is_none(), "a dividend whose quotient rounds");
    }

    /// D4567 (a): `div_scales` is numeric.c's select_div_scale for every x of its scale and digits (each step's first
    /// mantissa, the one before it, zero, and a sweep), over divisors of one and of several base-10000 digits, below one,
    /// and with a display scale; `exact_reciprocal` is 1 / k exactly where k's mantissa has only 2s and 5s; and the Div
    /// node's row (x x c, its dscale word) agrees with numeric_div's quotient for each probe.
    #[test]
    fn a_division_row_is_numeric_div() {
        use gravity_decompose::catalog::numeric::{div_scales, exact_reciprocal};
        let img = |t: &str| adt_numeric::numeric_in(t, -1, None).unwrap().unwrap();
        let text = |m: u128, s: u32| format!("{m}e-{s}"); // alloc: Cold
        let pg = |m: u128, s: u32, k: &str| adt_numeric::arith::select_div_scale(img(&text(m, s)).num().view(), img(k).num().view()) as u32;
        for (k, km, ks) in [("2", 2i64, 0u32), ("2.0", 20, 1), ("3", 3, 0), ("0.5", 5, 1), ("12345", 12345, 0), ("9999", 9999, 0), ("10000", 10000, 0), ("0.004", 4, 3), ("7.25", 725, 2)] {
            for (s, digits) in [(0u32, 9u32), (2, 9), (2, 18), (4, 12), (5, 12), (18, 18)] {
                let d = div_scales(s, digits, km, ks).expect("in bounds");
                assert_eq!(d.zero, pg(0, s, k), "0 at {s} / {k}");
                let mut probes: Vec<u128> = d.steps().iter().flat_map(|&(f, _)| [f as u128, (f as u128).saturating_sub(1).max(1)]).collect(); // alloc: Cold
                probes.extend((0..digits).flat_map(|p| [10u128.pow(p), 10u128.pow(p) * 3 - 1, 10u128.pow(p + 1) - 1]));
                for m in probes.into_iter().filter(|&m| m >= 1 && m < 10u128.pow(digits)) {
                    assert_eq!(d.scale_of(m as u64), pg(m, s, k), "{} / {k}", text(m, s));
                }
                assert!(d.steps().windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 > w[1].1), "{s} {digits} / {k}: steps ascend, scales fall");
            }
        }
        assert_eq!(div_scales(2, 19, 2, 0), None);
        assert_eq!(div_scales(2, 9, -2, 0), None);
        for (km, ks, want) in [(2i64, 0u32, Some((5i64, 1u32))), (5, 1, Some((2, 0))), (25, 1, Some((4, 1))), (1, 2, Some((100, 0))), (10, 0, Some((1, 1))), (20, 1, Some((5, 1))), (3, 0, None), (6, 0, None), (0, 0, None), (-2, 0, None), (1024, 0, Some((9765625, 10)))] {
            assert_eq!(exact_reciprocal(km, ks), want, "{km} at {ks}");
        }
        // the IR row: x x c at s + e and select_div_scale's dscale, against numeric_div's own quotient
        for (k, km, ks, s, xs) in [("2", 2i64, 0u32, 2u32, &[0i64, 1, -1, 299, 300, -300, 2_999_999, 3_000_000, 123_456, -999_999_999][..]), ("8", 8, 0, 4, &[0, 1, 7, 10_000, 29_999, 30_000, -12_345_678][..]), ("0.25", 25, 2, 0, &[0, 1, 3, 4, 9_999, 40_000][..])] {
            let (c, e) = exact_reciprocal(km, ks).unwrap();
            let scales = div_scales(s, 10, km, ks).unwrap();
            for &x in xs {
                let t = Tree::Div { x: mcx::box_in(Tree::Const { m: x, scale: s as i16, digits: 10 }, m()), c, e: e as i16, scales };
                let run = |part: u8| {
                    crate::frame::stop::tests::run1(&[], &mut |b, env| {
                        let row = RowVals { row: crate::cell::Row::new_in(env.mcx), vals: mcx::PgVec::new_in(env.mcx), cols: mcx::PgVec::new_in(env.mcx), carry: crate::frame::row::Carry::new(env.zero, env.mcx), next: env.zero, at: env.zero };
                        let mut memo = crate::ops::agg::numeric::strategies::memo_num::Memo::new(false, env.mcx);
                        let v = crate::ops::agg::numeric::strategies::num_window::finite(b, env, &row, &t, &mut memo, None);
                        if part == 0 { v.lo } else { v.ds.expect("a dscale word") }
                    })
                };
                let q = adt_numeric::numeric_div_common(img(&format!("{x}e-{s}")).num(), img(k).num()).unwrap(); // alloc: Cold
                let want = adt_numeric::numeric_in(&format!("{}e-{}", x as i128 * c as i128, s + e), -1, None).unwrap().unwrap(); // alloc: Cold
                assert_eq!(adt_numeric::cmp_numerics(q.num(), want.num()), 0, "{x}e-{s} / {k} is exact at {}", s + e);
                assert_eq!((run(0), run(1)), (x * c, q.num().dscale() as i64), "{x}e-{s} / {k}");
            }
        }
    }
}
