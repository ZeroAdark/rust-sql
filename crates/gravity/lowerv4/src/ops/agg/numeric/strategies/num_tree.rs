//! Numeric mantissa trees. Hooks / levers: H14 R2-L07. Budget 135 lines (files-extra.tsv, +25 % gate). Owner M1-FOLD.
//!
//! Split from num_window.rs by responsibility (D267): the compile of a `class.num_packed` argument into a tree of numeric
//! columns, int casts, constants, typmod casts and `+`, `-`, `*` (`Tree`), each node's scale (`*` adds, `+` / `-` take the larger) and its
//! digits bound (a node under 18 digits is one word, one under 37 is 128 bits, a wider one is not a tree); a searched
//! CASE of tree arms is a node (num_case.rs), a constant a leaf (num_const.rs). num_window.rs folds the tree; registers
//! nothing.
use mcx::{Mcx, PgBox, PgVec};
use types_nodes::Node as N;

use crate::ops::agg::numeric::strategies::num_const::const_of;

const WIDE: u32 = 37;
pub(crate) const NARROW: u32 = 18;

// fmgroids.h's names (the catalog home's): the numeric operators' procedures whose mantissa arithmetic the tree implements
use gravity_decompose::catalog::numeric::{is_div, negation, numeric_type, F_NUMERIC_ADD, F_NUMERIC_MUL, F_NUMERIC_SUB, F_NUMERIC_UMINUS};

#[derive(PartialEq)]
pub(crate) enum Tree<'g> {
    /// A numeric column of the row at its typmod scale, `digits` its precision.
    Col { c: i16, scale: i16, digits: u32 },
    /// An int2 / int4 / int8 column cast to numeric (scale 0).
    Int { c: i16, typid: u32, digits: u32 },
    Const { m: i64, scale: i16, digits: u32 },
    /// D2356: a numeric constant past one i64 mantissa word: its mantissa where it fits an i128 (`m`), display scale and
    /// digits. A strict tree holds it as a 128-bit leaf when `m` is known and the leaf is `bounded` (37 digits); the
    /// non-strict walk (`wide`'s shape) holds any.
    PastWord { m: Option<i128>, scale: i16, digits: u32 },
    Mul(PgBox<'g, Tree<'g>>, PgBox<'g, Tree<'g>>),
    Add(PgBox<'g, Tree<'g>>, PgBox<'g, Tree<'g>>),
    Sub(PgBox<'g, Tree<'g>>, PgBox<'g, Tree<'g>>),
    /// A searched CASE (or a COALESCE) over numeric arms (num_case.rs).
    Case(PgBox<'g, crate::ops::agg::numeric::strategies::num_case::Case<'g>>),
    /// PS4-21: `x::numeric(p, s)` (numeric_numeric_int4, a constant typmod `typmod`) of a one-word `x`: x at scale `scale`
    /// as apply_typmod rounds it, within `digits` (p) digits or PostgreSQL's numeric field overflow (num_window `cast`).
    Cast { x: PgBox<'g, Tree<'g>>, scale: i16, digits: u32, typmod: i32 },
    /// D4567 (a): `x / k` at a sum's root, x one word and k a positive constant whose reciprocal is a finite decimal
    /// (decompose agg numeric.rs `div_form`): x x `c` at x's scale + `e`, numeric_div's quotient exactly; its display scale,
    /// by row from x's mantissa (`scales`: select_div_scale), is the root's dscale word, at least the node's scale.
    Div { x: PgBox<'g, Tree<'g>>, c: i64, e: i16, scales: gravity_decompose::catalog::numeric::DivScales },
}

pub(crate) fn scale_of(t: &Tree) -> i16 {
    crate::stack_check();
    match t {
        Tree::Col { scale, .. } | Tree::Const { scale, .. } | Tree::PastWord { scale, .. } => *scale,
        Tree::Int { .. } => 0,
        Tree::Mul(x, y) => scale_of(x) + scale_of(y),
        Tree::Add(x, y) | Tree::Sub(x, y) => scale_of(x).max(scale_of(y)),
        Tree::Case(c) => c.scale,
        Tree::Cast { scale, .. } => *scale,
        Tree::Div { x, e, .. } => scale_of(x) + e,
    }
}

/// A bound on the decimal digits of the node's mantissa.
pub(crate) fn digits_of(t: &Tree) -> u32 {
    crate::stack_check();
    match t {
        Tree::Col { digits, .. } | Tree::Const { digits, .. } | Tree::Int { digits, .. } | Tree::PastWord { digits, .. } | Tree::Cast { digits, .. } => *digits,
        Tree::Mul(x, y) => digits_of(x) + digits_of(y),
        Tree::Add(x, y) | Tree::Sub(x, y) => {
            let s = scale_of(t);
            (digits_of(x) + (s - scale_of(x)) as u32).max(digits_of(y) + (s - scale_of(y)) as u32) + 1
        }
        Tree::Case(c) => c.digits,
        Tree::Div { x, c, .. } => digits_of(x) + c.unsigned_abs().checked_ilog10().unwrap_or(0) + 1,
    }
}

/// `e` (an aggregate's argument) as a tree at `scale`, when it is one; a CASE's WHEN nodes pushed on `whens` (`bind`).
pub(crate) fn compile<'g, 'p>(e: N<'p>, scale: i16, whens: &mut PgVec<'_, N<'p>>, mcx: Mcx<'g>) -> Option<Tree<'g>> {
    tree(e, true, true, whens, mcx).filter(|t| scale_of(t) == scale && (0..=18).contains(&scale))
}

/// V4R-38: `e` as class.num_wide's tree at `scale` (decompose `wide_tree`'s shape, OID-04): a wide node (`wide_node`)
/// past 37 digits and within 55 (`WIDE_TERM`), exempt from `bounded`.
pub(crate) fn compile_wide<'g, 'p>(e: N<'p>, scale: i16, whens: &mut PgVec<'_, N<'p>>, mcx: Mcx<'g>) -> Option<Tree<'g>> {
    wide_node(e, whens, mcx).filter(|t| digits_of(t) <= WIDE_TERM && scale_of(t) == scale && (0..=18).contains(&scale))
}

/// The most digits of a class.num_wide term: 10^55 < 2^183, 2^63 of them under 2^246.
const WIDE_TERM: u32 = 55;

/// A node class.num_wide evaluates exactly past 37 digits (num_keyed `wide_term`): `x * y`, one factor a tree (`bounded`,
/// 37 digits) and the other one word (18), its exact 192 bits; or `x + y` / `x - y`, each side a wide node or a tree,
/// added in 256 bits at the larger scale; `-x` of a wide node as `0 - x`. A side that is neither leaves `whens` as it
/// found it.
fn wide_node<'g, 'p>(e: N<'p>, whens: &mut PgVec<'_, N<'p>>, mcx: Mcx<'g>) -> Option<Tree<'g>> {
    crate::stack_check();
    let e = e.as_relabel_type().map_or(e, |r| r.arg);
    let op = e.as_op_expr()?;
    let (ex, ey) = (op.args.iter().next()?, op.args.iter().nth(1));
    let t = match (op.opfuncid, ey) {
        // -x as 0 - x (numeric_uminus), as `tree` reads it
        (F_NUMERIC_UMINUS, None) => {
            let x = wide_node(ex, whens, mcx)?;
            Tree::Sub(mcx::box_in(Tree::Const { m: 0, scale: scale_of(&x), digits: 1 }, mcx), mcx::box_in(x, mcx))
        }
        (_, Some(_)) if op.args.len() > 2 => return None,
        (F_NUMERIC_MUL, Some(ey)) => {
            let (x, y) = (tree(ex, true, false, whens, mcx)?, tree(ey, true, false, whens, mcx)?);
            if digits_of(&x) > NARROW && digits_of(&y) > NARROW {
                return None;
            }
            Tree::Mul(mcx::box_in(x, mcx), mcx::box_in(y, mcx))
        }
        (F_NUMERIC_ADD | F_NUMERIC_SUB, Some(ey)) => {
            let mut side = |e: N<'p>| {
                let n = whens.len();
                wide_node(e, whens, mcx).or_else(|| {
                    whens.truncate(n);
                    tree(e, true, false, whens, mcx)
                })
            };
            let (x, y) = (mcx::box_in(side(ex)?, mcx), mcx::box_in(side(ey)?, mcx));
            match op.opfuncid {
                F_NUMERIC_ADD => Tree::Add(x, y),
                _ => Tree::Sub(x, y),
            }
        }
        _ => return None,
    };
    Some(t).filter(|t| digits_of(t) > WIDE)
}

/// The WHENs' compiled ids (`compile`'s `whens` order) into their CASE nodes.
pub(crate) fn bind(t: &mut Tree<'_>, ids: &[crate::frame::px::ExId]) {
    crate::stack_check();
    match t {
        Tree::Mul(x, y) | Tree::Add(x, y) | Tree::Sub(x, y) => {
            bind(x, ids);
            bind(y, ids);
        }
        Tree::Case(c) => c.arms.iter_mut().for_each(|(w, r)| {
            w.iter_mut().for_each(|w| *w = ids[w.0 as usize]);
            r.iter_mut().for_each(|r| bind(r, ids));
        }),
        Tree::Cast { x, .. } | Tree::Div { x, .. } => bind(x, ids),
        _ => {}
    }
}

/// D297: `e` is a numeric expression of tree nodes past one mantissa word (19 digits or more), so the i64 word of the
/// generic path cannot hold it; a tree may (`compile`: nodes within 37 digits, columns of one word, a constant past one
/// word as a 128-bit leaf, D2356), else its datum folds (class_of.rs `class_in`).
pub(crate) fn wide(e: N<'_>, mcx: Mcx<'_>) -> bool {
    tree(e, false, true, &mut PgVec::new_in(mcx), mcx).is_some_and(|t| digits_of(&t) > NARROW)
}

/// `strict`: every node `bounded` and a column one word; else only the shape (for `wide`). `root`: a CASE there may mix
/// arm scales (D256's dscale word), one under an operator or an arm not.
fn tree<'g, 'p>(e: N<'p>, strict: bool, root: bool, whens: &mut PgVec<'_, N<'p>>, mcx: Mcx<'g>) -> Option<Tree<'g>> {
    crate::stack_check();
    if let Some(r) = e.as_relabel_type() {
        return tree(r.arg, strict, root, whens, mcx);
    }
    if e.as_case_expr().is_some() || e.as_coalesce_expr().is_some() {
        let c = crate::ops::agg::numeric::strategies::num_case::compile(e, root, whens, &mut |x, w| tree(x, strict, false, w, mcx), mcx)?;
        return Some(Tree::Case(mcx::box_in(c, mcx))).filter(|t| bounded(t) || !strict);
    }
    if let Some(v) = e.as_var() {
        let c = crate::step::project::bare_var(e)?;
        let t = u32::try_from(v.vartypmod.checked_sub(4)?).ok()?;
        let (scale, digits) = ((t & 0xffff) as i16, t >> 16);
        return (numeric_type(v.vartype) && scale <= 18 && (digits <= NARROW || !strict)).then_some(Tree::Col { c, scale, digits });
    }
    if let Some(c) = e.as_const() {
        return const_of(c, strict);
    }
    if let Some(t) = e.as_func_expr().and_then(|f| typmod_cast(f, strict, whens, mcx)) {
        return Some(t);
    }
    if let Some(f) = e.as_func_expr() {
        let a = f.args.iter().next().filter(|_| types_core::cast::exact_to_numeric(f.funcid).is_some() && f.args.len() == 1)?;
        if let Some(c) = a.as_const() {
            return const_of(c, strict);
        }
        let v = a.as_var()?;
        let digits = types_core::cast::int_digits(v.vartype)?;
        return Some(Tree::Int { c: crate::step::project::bare_var(a)?, typid: v.vartype, digits });
    }
    if let Some(op) = e.as_op_expr().filter(|o| negation(o.opfuncid) && o.args.len() == 1) {
        // D830: -x as 0 - x (numeric_uminus), a digit more by the Sub bound
        let x = tree(op.args.iter().next()?, strict, false, whens, mcx)?;
        let t = Tree::Sub(mcx::box_in(Tree::Const { m: 0, scale: scale_of(&x), digits: 1 }, mcx), mcx::box_in(x, mcx));
        return (bounded(&t) || !strict).then_some(t);
    }
    if let Some(op) = e.as_op_expr().filter(|o| is_div(o.opfuncid) && o.args.len() == 2) {
        return root.then(|| div(op, strict, whens, mcx)).flatten();
    }
    let op = e.as_op_expr().filter(|o| o.args.len() == 2)?;
    // D4567: under + / - one side may be a CASE of mixed arm scales when the other's scale is at least every arm's (the
    // node's dscale is then that side's on every row: decompose numeric.rs `form_at`)
    let add = gravity_decompose::catalog::numeric::sum_or_difference(op.opfuncid);
    let (x, y) = (mcx::box_in(tree(op.args.iter().next()?, strict, add, whens, mcx)?, mcx), mcx::box_in(tree(op.args.iter().nth(1)?, strict, add, whens, mcx)?, mcx));
    // D4567 (a): a division is a root's alone (its display scale varies by row)
    if (add && !fixed_beside(&x, &y)) || matches!(*x, Tree::Div { .. }) || matches!(*y, Tree::Div { .. }) {
        return None;
    }
    let t = match op.opfuncid {
        F_NUMERIC_ADD => Tree::Add(x, y),
        F_NUMERIC_SUB => Tree::Sub(x, y),
        F_NUMERIC_MUL => Tree::Mul(x, y),
        _ => return None,
    };
    (bounded(&t) || !strict).then_some(t)
}

/// D4567 (a): `x / k` as a `Tree::Div` (decompose agg numeric.rs `div_form`): x a one-word tree, k a positive constant
/// of a finite-decimal reciprocal (decompose catalog numeric `exact_reciprocal`), every row's display scale (`div_scales`
/// over x's digits) at least the node's scale, so the quotient is exact there.
fn div<'g, 'p>(op: &'p types_nodes::primnodes::OpExpr<'p>, strict: bool, whens: &mut PgVec<'_, N<'p>>, mcx: Mcx<'g>) -> Option<Tree<'g>> {
    use gravity_decompose::catalog::numeric::{div_scales, exact_reciprocal};
    let n = whens.len();
    let x = tree(op.args.iter().next()?, strict, false, whens, mcx).filter(|x| digits_of(x) <= NARROW);
    let Some(x) = x else {
        whens.truncate(n);
        return None;
    };
    let (km, ks) = gravity_decompose::ops::agg::div_const(op.args.iter().nth(1)?)?;
    let (c, e) = exact_reciprocal(km, ks)?;
    let s = u32::try_from(scale_of(&x)).ok()?;
    let scales = div_scales(s, digits_of(&x), km, ks)?;
    let at = s + e;
    let t = Tree::Div { x: mcx::box_in(x, mcx), c, e: e as i16, scales };
    (scales.min() >= at && at <= 18 && digits_of(&t) <= NARROW).then_some(t)
}

/// D4567: the two sides of a `+` / `-` give it one dscale on every row: neither a CASE of mixed arm scales, or one such
/// beside a side of a scale at least its every arm's.
fn fixed_beside(x: &Tree, y: &Tree) -> bool {
    let mixed = |t: &Tree| matches!(t, Tree::Case(c) if c.mixed);
    match (mixed(x), mixed(y)) {
        (true, true) => false,
        (true, false) => scale_of(y) >= scale_of(x),
        (false, true) => scale_of(x) >= scale_of(y),
        (false, false) => true,
    }
}

/// PS4-21: a typmod cast `x::numeric(p, s)` of a one-word tree `x` (its WHENs kept on `whens`), 1 <= p <= 18 and
/// 0 <= s <= 18: `Tree::Cast`.
fn typmod_cast<'g, 'p>(f: &'p types_nodes::primnodes::FuncExpr<'p>, strict: bool, whens: &mut PgVec<'_, N<'p>>, mcx: Mcx<'g>) -> Option<Tree<'g>> {
    let k = f.args.iter().nth(1).and_then(|k| k.as_const()).filter(|k| gravity_decompose::catalog::casts::numeric_typmod_cast(f.funcid) && f.args.len() == 2 && !k.constisnull && k.constbyval)?;
    let typmod = k.constvalue.as_i32();
    let (digits, scale) = gravity_fn::typesem::numeric::typmod_parts(typmod).filter(|&(p, s)| (1..=NARROW as i32).contains(&p) && (0..=18).contains(&s))?;
    let n = whens.len();
    let x = tree(f.args.iter().next()?, strict, false, whens, mcx).filter(|x| digits_of(x) <= NARROW);
    if x.is_none() {
        whens.truncate(n);
    }
    Some(Tree::Cast { x: mcx::box_in(x?, mcx), scale: scale as i16, digits: digits as u32, typmod })
}

/// D297: node `t` stays within the tree's words: at most 37 digits (i128 holds 38), a product with one side one word.
pub(crate) fn bounded(t: &Tree) -> bool {
    let one = match t {
        Tree::Mul(x, y) => digits_of(x) <= NARROW || digits_of(y) <= NARROW,
        _ => true,
    };
    one && digits_of(t) <= WIDE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D297: `a * b * c` over numeric(18, 0) is 54 digits, never a tree (it wrapped past 128 bits); 36 digits still are.
    #[test]
    fn no_node_passes_37_digits() {
        let cx = mcx::MemoryContext::new("d297");
        let bx = |t| mcx::box_in(t, cx.mcx());
        let col = |digits| bx(Tree::Col { c: 1, scale: 0, digits });
        assert!(bounded(&Tree::Mul(col(18), col(18))));
        assert!(bounded(&Tree::Mul(bx(Tree::Mul(col(9), col(9))), col(18))));
        assert!(!bounded(&Tree::Mul(bx(Tree::Mul(col(18), col(18))), col(18))));
        assert!(!bounded(&Tree::Mul(bx(Tree::Mul(col(18), col(18))), bx(Tree::Mul(col(1), col(1))))));
        assert!(bounded(&Tree::Mul(col(18), col(19))));
        assert!(!bounded(&Tree::Add(bx(Tree::Mul(col(18), col(19))), col(1))));
    }

    /// D2356: a numeric constant past one mantissa word is a `PastWord` leaf of its width (it read as no value and raised
    /// on every row): a strict tree's 128-bit leaf with its exact mantissa within 37 digits, past them only the non-strict
    /// walk's (`wide`: its argument folds as a datum). A one-word constant stays the strict tree's `Const`.
    #[test]
    fn a_constant_past_one_word_is_a_128_bit_leaf_within_the_bound() {
        let cx = mcx::MemoryContext::new("d2356");
        let mx = cx.mcx();
        let lits = ["13619314596827451335.1", "1e35", "1e40", "-922337203685477580.75", "1.5", "-92233720368547758.05"];
        let imgs: Vec<_> = lits.iter().map(|t| adt_numeric::numeric_in(t, -1, None).unwrap().unwrap()).collect();
        let k = |i: usize| N::mk_const(cx.mcx(), 1700, -1, 0, -1, datum::Datum::from_usize(imgs[i].as_bytes().as_ptr() as usize), false, false).expect("a const node");
        for (i, (past, scale, digits)) in [(true, 1, 21), (true, 0, 36), (true, 0, 41), (true, 2, 20), (false, 1, 2), (false, 2, 19)].into_iter().enumerate() {
            assert_eq!(wide(k(i), mx), digits > NARROW, "{}", lits[i]);
            let t = tree(k(i), false, true, &mut mcx::PgVec::new_in(mx), mx).expect(lits[i]);
            assert_eq!((scale_of(&t), digits_of(&t), matches!(t, Tree::PastWord { .. })), (scale, digits, past), "{}", lits[i]);
            assert_eq!(compile(k(i), scale, &mut mcx::PgVec::new_in(mx), mx).is_some(), digits <= WIDE, "{}", lits[i]);
        }
        let m = |i: usize| match tree(k(i), true, true, &mut mcx::PgVec::new_in(mx), mx) { Some(Tree::PastWord { m, .. }) => m, _ => None };
        assert_eq!((m(0), m(1), m(3)), (Some(136_193_145_968_274_513_351), Some(10i128.pow(35)), Some(-92_233_720_368_547_758_075)));
    }

    /// PS4-21: `x::numeric(p, s)` of a one-word tree is a Cast node of p digits at scale s, alone or under an operator.
    /// Does not apply: a precision past 18, a scale past 18 or below 0, a typmod argument that is not a constant int4, an
    /// `x` that is not a tree (a float8 column) or is past one word (numeric(30, 2)).
    #[test]
    fn a_typmod_cast_of_a_one_word_tree_is_a_node() {
        use gravity_decompose::catalog::numeric::F_NUMERIC_NUMERIC_INT4;
        use types_nodes::list::NodeList;
        use types_nodes::primnodes::{FuncExpr, OpExpr};
        let cx = mcx::MemoryContext::new("ps421");
        let mx = cx.mcx();
        let typmod = |p: i32, s: i32| ((p << 16) | (s & 0x7ff)) + 4;
        let var = |t: u32, tm: i32| N::mk_var(mx, 1, 1, t, tm, 0, 0).expect("a var");
        let i4 = |v: i32| N::mk_const(mx, 23, -1, 0, 4, datum::Datum::from_i32(v), false, true).expect("an int4 const");
        let call = |op: bool, f: u32, args: &[_]| {
            let mut l = NodeList::nil();
            for a in args {
                l.lappend(mx, *a).expect("args");
            }
            match op {
                true => N::mk(mx, OpExpr { opfuncid: f, args: l, ..Default::default() }),
                false => N::mk(mx, FuncExpr { funcid: f, args: l, ..Default::default() }),
            }
            .expect("a node")
        };
        let cast = |x, tm| call(false, F_NUMERIC_NUMERIC_INT4, &[x, tm]);
        let n14_4 = || var(1700, typmod(14, 4));
        assert!(matches!(tree(cast(n14_4(), i4(typmod(12, 2))), true, true, &mut mcx::PgVec::new_in(mx), mx), Some(Tree::Cast { scale: 2, digits: 12, .. })));
        assert!(compile(cast(n14_4(), i4(typmod(12, 2))), 2, &mut mcx::PgVec::new_in(mx), mx).is_some_and(|t| matches!(t, Tree::Cast { .. })));
        let mul = call(true, 1726, &[cast(n14_4(), i4(typmod(15, 2))), n14_4()]);
        assert!(matches!(tree(mul, true, true, &mut mcx::PgVec::new_in(mx), mx), Some(Tree::Mul(..))), "a cast under numeric_mul");
        for (x, tm, what) in [(n14_4(), i4(typmod(19, 2)), "numeric(19, 2)"), (n14_4(), i4(typmod(20, 19)), "a scale past 18"), (n14_4(), i4(typmod(5, -2)), "a negative scale"), (n14_4(), var(23, -1), "a typmod column"), (var(701, -1), i4(typmod(12, 2)), "a float8 argument"), (var(1700, typmod(30, 2)), i4(typmod(12, 2)), "a numeric(30, 2) argument")] {
            assert!(tree(cast(x, tm), true, true, &mut mcx::PgVec::new_in(mx), mx).is_none(), "{what}");
        }
    }

    /// Does not apply: the tree's gates are catalog names of what it implements (numeric_uminus, numeric_add / sub / mul,
    /// the numeric type's typmod, the exact casts int2 / int4 / int8 -> numeric, int2 / int4 / int8 / numeric constants);
    /// every near form declines, strict or not, so its aggregate's argument folds as a datum or takes the generic step:
    /// a one-argument numeric operator that is not the negation (numeric_uplus `+x`, numeric_abs `@x`), another type's
    /// negation (float8um, int4um), a binary operator the tree does not implement (numeric_div, numeric_mod), a column of
    /// another type at a numeric typmod (a domain over numeric is its own type), a numeric column with no typmod or a
    /// scale past 18, a cast that is not exact (float8 -> numeric) or not to numeric (numeric -> int4), and a constant
    /// of another type, a NULL, a NaN or one past scale 18. The named forms beside them compile.
    #[test]
    fn the_catalog_names_decline_every_near_form() {
        use types_nodes::list::NodeList;
        use types_nodes::primnodes::{FuncExpr, OpExpr};
        let cx = mcx::MemoryContext::new("numeric names");
        let mx = cx.mcx();
        let lits = ["12.5", "NaN", "0.0000000000000000001", "3"];
        let imgs: Vec<_> = lits.iter().map(|t| adt_numeric::numeric_in(t, -1, None).unwrap().unwrap()).collect();
        let num = |i: usize| N::mk_const(mx, 1700, -1, 0, -1, datum::Datum::from_usize(imgs[i].as_bytes().as_ptr() as usize), false, false).expect("a const");
        let typmod = |p: i32, s: i32| ((p << 16) | s) + 4;
        let var = |t: u32, tm: i32| N::mk_var(mx, 1, 1, t, tm, 0, 0).expect("a var");
        let op = |f: u32, args: &[_]| {
            let mut l = NodeList::nil();
            for a in args {
                l.lappend(mx, *a).expect("args");
            }
            N::mk(mx, OpExpr { opfuncid: f, args: l, ..Default::default() }).expect("an op")
        };
        let cast = |f: u32, x| N::mk(mx, FuncExpr { funcid: f, args: NodeList::make1(mx, x).expect("args"), ..Default::default() }).expect("a cast");
        let n10_2 = || var(1700, typmod(10, 2));
        let declines = |e, what: &str| {
            assert!(tree(e, false, true, &mut mcx::PgVec::new_in(mx), mx).is_none(), "{what}: the shape walk takes it");
            assert!(tree(e, true, true, &mut mcx::PgVec::new_in(mx), mx).is_none(), "{what}: the strict tree takes it");
        };
        // the named forms
        assert!(matches!(tree(op(1771, &[n10_2()]), true, true, &mut mcx::PgVec::new_in(mx), mx), Some(Tree::Sub(..))), "numeric_uminus");
        assert!(matches!(tree(cast(1740, var(23, -1)), true, true, &mut mcx::PgVec::new_in(mx), mx), Some(Tree::Int { digits: 10, .. })), "int4 -> numeric");
        assert!(matches!(tree(op(1726, &[n10_2(), num(0)]), true, true, &mut mcx::PgVec::new_in(mx), mx), Some(Tree::Mul(..))), "numeric_mul");
        let k4 = N::mk_const(mx, 23, -1, 0, 4, datum::Datum::from_i32(7), false, true).expect("an int4 const");
        assert!(matches!(tree(k4, true, true, &mut mcx::PgVec::new_in(mx), mx), Some(Tree::Const { m: 7, scale: 0, .. })), "an int4 constant");
        assert!(matches!(tree(num(3), true, true, &mut mcx::PgVec::new_in(mx), mx), Some(Tree::Const { m: 3, scale: 0, .. })), "a numeric constant");
        // their near forms
        declines(op(1915, &[n10_2()]), "numeric_uplus");
        declines(op(1704, &[n10_2()]), "numeric_abs");
        declines(op(220, &[var(701, -1)]), "float8um");
        declines(op(212, &[var(23, -1)]), "int4um");
        declines(op(1727, &[n10_2(), n10_2()]), "numeric_div");
        declines(op(1729, &[n10_2(), n10_2()]), "numeric_mod");
        declines(var(16_500, typmod(10, 2)), "a domain over numeric(10, 2)");
        declines(var(701, typmod(10, 2)), "a float8 column");
        declines(var(1700, -1), "numeric with no typmod");
        declines(var(1700, typmod(30, 20)), "numeric(30, 20)");
        declines(cast(1743, var(701, -1)), "float8 -> numeric");
        declines(cast(1744, n10_2()), "numeric -> int4");
        let k8 = N::mk_const(mx, 701, -1, 0, 8, datum::Datum::from_usize(1.5f64.to_bits() as usize), false, true).expect("a float8 const");
        declines(k8, "a float8 constant");
        declines(N::mk_const(mx, 1700, -1, 0, -1, datum::Datum::from_usize(0), true, false).expect("a NULL"), "a NULL numeric");
        declines(num(1), "NaN");
        declines(num(2), "a constant at scale 19");
    }
}
