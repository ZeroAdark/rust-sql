//! Per-(row, cell / ExId) memo of mantissas and subtrees. Hooks / levers: H14 R2-L08. Budget 160 lines (files.tsv, +25
//! % gate). Owner M1-FOLD.
//!
//! Registers: road `num.memo` (`NUM_MEMO`): the aggregates of one fold read their num.window trees against one
//! row's memo, so a subtree equal to one already emitted for the row reuses its words and validity (`sum(n), avg(n)`
//! decode `n` once; `sum(p * (1 - d))` under two sums is multiplied once). A subtree is found by its value, not its
//! spelling: numeric_add and numeric_mul commute, so `(1 - d) * p` reuses `p * (1 - d)` and `n + 1` reuses `1 + n`, at
//! any depth (`same`), the reused value's operand parts put in the reader's order (`fit`). The memo is the row's: plain.rs
//! makes one per row body and drops what a FILTER branch added once the branch joins (a value emitted inside it does not
//! dominate).
//! Also here: the tree's specials code, worked out in num_window.rs's cold block from the leaves' codes and the finite
//! signs, as PG's numeric operators treat NaN and ±Infinity.
use gravity_fn::agg_final::{SP_NAN, SP_NINF, SP_PINF};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use mcx::{GrowIn, Mcx, PgVec};

use crate::ops::agg::numeric::strategies::num_tree::Tree;
use crate::ops::agg::numeric::strategies::num_window::Val;
use crate::frame::env::Env;

crate::strategies! {
    /// Per-(row, cell / ExId) memo of mantissas and subtrees.
    NUM_MEMO = 26, "num.memo", NumAny;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(NUM_MEMO, &Road)], ..crate::ops::NONE };

/// The road's registration value (plain.rs keeps one `Memo` per row body when it is on).
struct Road;

/// One row's emitted subtrees (`on`: num.memo; off, nothing is kept); `cover`: the nodes every row emits unmasked.
pub(crate) struct Memo<'t> {
    pub on: bool,
    pub mcx: Mcx<'t>,
    done: PgVec<'t, (&'t Tree<'t>, Val<'t>)>,
    cover: PgVec<'t, &'t Tree<'t>>,
    /// D3233 (road case.select): a root CASE's arm selectors the fold read before branching on them (num_case.rs
    /// `select`), by the CASE's address: its `finite` takes them instead of reading the WHENs again.
    pub sel: Option<(usize, PgVec<'t, gravity_ir::Value>)>,
}

impl<'t> Memo<'t> {
    pub(crate) fn new(on: bool, mcx: Mcx<'t>) -> Memo<'t> {
        Memo { on, mcx, done: PgVec::new_in(mcx), cover: PgVec::new_in(mcx), sel: None }
    }

    /// The trees of the folds that read every row (no FILTER) and their nodes outside a CASE arm.
    pub(crate) fn cover(&mut self, roots: impl Iterator<Item = &'t Tree<'t>>) {
        let mut todo: PgVec<'_, &'t Tree<'t>> = PgVec::new_in(self.mcx);
        for r in roots {
            todo.push_in(r);
        }
        while let Some(t) = todo.pop().filter(|_| self.on) {
            if let Tree::Mul(x, y) | Tree::Add(x, y) | Tree::Sub(x, y) = t {
                todo.extend_in([&**x, &**y]);
            }
            if let Tree::Cast { x, .. } | Tree::Div { x, .. } = t {
                todo.push_in(&**x);
            }
            self.cover.push_in(t);
        }
    }

    /// A CASE arm `t` another fold emits unmasked anyway (num_case.rs runs it so, shared).
    pub(crate) fn covers(&self, t: &Tree) -> bool {
        self.on && self.cover.iter().any(|c| same(c, t))
    }

    /// The row's value of a subtree `same` as `t`, its operand parts in `t`'s order.
    pub(crate) fn get(&self, t: &Tree<'t>) -> Option<Val<'t>> {
        if let Some(d) = self.done.iter().find(|d| d.0 == t) {
            return Some(d.1.clone());
        }
        let d = self.done.iter().find(|d| same(d.0, t))?;
        fires::fire("num.memo.commuted"); // shapecheck fire signal (REGISTRY FOLD-18: a subtree reused across a commuted operator)
        Some(fit(t, d.0, &d.1, self.mcx))
    }

    pub(crate) fn put(&mut self, t: &'t Tree<'t>, v: Val<'t>) {
        if self.on {
            self.done.push_in((t, v));
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.done.len()
    }

    /// Forgets what a branch added (its values do not dominate the join).
    pub(crate) fn truncate(&mut self, n: usize) {
        self.done.truncate(n);
    }
}

/// One value: `t` and `u` are equal up to the operand order of numeric_add and numeric_mul (PG's numeric_add / numeric_mul
/// are commutative, exact and raise on the same operand pairs) at every node; a CASE, a leaf and numeric_sub compare as
/// written.
fn same<'a>(t: &Tree<'a>, u: &Tree<'a>) -> bool {
    crate::stack_check();
    match (t, u) {
        (Tree::Add(a, b), Tree::Add(c, d)) | (Tree::Mul(a, b), Tree::Mul(c, d)) => (same(a, c) && same(b, d)) || (same(a, d) && same(b, c)),
        (Tree::Sub(a, b), Tree::Sub(c, d)) => same(a, c) && same(b, d),
        _ => t == u,
    }
}

/// `v`, the row's value of `u` (`same` as `t`), with its operands' parts in `t`'s order: `code` reads a binary node's
/// first kid as its first operand's.
fn fit<'v>(t: &Tree, u: &Tree, v: &Val<'v>, mcx: Mcx<'v>) -> Val<'v> {
    crate::stack_check();
    let mut r = v.clone();
    if let ((Tree::Add(a, b), Tree::Add(c, d)) | (Tree::Mul(a, b), Tree::Mul(c, d)) | (Tree::Sub(a, b), Tree::Sub(c, d)), [k0, k1]) = ((t, u), &v.kids[..]) {
        r.kids = match same(a, c) && same(b, d) {
            true => mcx::pg_vec![in mcx; fit(a, c, k0, mcx), fit(b, d, k1, mcx)],
            false => mcx::pg_vec![in mcx; fit(a, d, k1, mcx), fit(b, c, k0, mcx)],
        };
    }
    r
}

/// A finite node's sign class `(negative, zero)`.
fn sign(b: &mut FunctionBuilder<'_>, env: &Env, v: &Val) -> (Value, Value) {
    let (lo, hi, ..) = v.parts();
    match hi {
        None => (b.icmp(IntCond::Slt, lo, env.zero), b.icmp(IntCond::Eq, lo, env.zero)),
        Some(hi) => {
            let neg = b.icmp(IntCond::Slt, hi, env.zero);
            let (z1, z0) = (b.icmp(IntCond::Eq, hi, env.zero), b.icmp(IntCond::Eq, lo, env.zero));
            (neg, b.int_bin(IntBinOp::And, z0, z1))
        }
    }
}

/// Tree `t`'s specials code for the row (`v` its finite values): `+` / `-` by `add_codes` (a subtrahend's infinity
/// flipped), `*` NaN over NaN or 0 × Inf, else an infinity signed by the operands' signs.
pub(crate) fn code(b: &mut FunctionBuilder<'_>, env: &Env, t: &Tree, v: &Val) -> Value {
    crate::stack_check();
    let (_, _, _, kids) = v.parts();
    let (x, y, sub) = match t {
        Tree::Col { .. } => {
            let (m, on) = v.raw.expect("a column leaf carries its word");
            return crate::ops::agg::numeric::num_word::specials(b, env, m, on).1;
        }
        Tree::Const { .. } | Tree::Int { .. } | Tree::PastWord { .. } => return env.zero,
        Tree::Case(c) => return crate::ops::agg::numeric::strategies::num_case::code(b, env, c, v),
        // PS4-21: a NaN passes a typmod; no leaf of a tree is infinite
        Tree::Cast { x, .. } => return code(b, env, x, &kids[0]),
        // D4567 (a): a positive divisor keeps x's NaN and its infinity's sign
        Tree::Div { x, .. } => return code(b, env, x, &kids[0]),
        Tree::Add(x, y) => (x, y, false),
        Tree::Sub(x, y) => (x, y, true),
        Tree::Mul(x, y) => {
            let (cx, cy) = (code(b, env, x, &kids[0]), code(b, env, y, &kids[1]));
            let (nan, pinf, ninf) = (b.iconst(Type::I64, SP_NAN), b.iconst(Type::I64, SP_PINF), b.iconst(Type::I64, SP_NINF));
            let (xn, yn) = (b.icmp(IntCond::Eq, cx, nan), b.icmp(IntCond::Eq, cy, nan));
            let any_nan = b.int_bin(IntBinOp::Or, xn, yn);
            let (xf, yf) = (b.icmp(IntCond::Eq, cx, env.zero), b.icmp(IntCond::Eq, cy, env.zero));
            let both_fin = b.int_bin(IntBinOp::And, xf, yf);
            let ((xneg_f, xzero_f), (yneg_f, yzero_f)) = (sign(b, env, &kids[0]), sign(b, env, &kids[1]));
            let (x_ninf, y_ninf) = (b.icmp(IntCond::Eq, cx, ninf), b.icmp(IntCond::Eq, cy, ninf));
            let (xneg, yneg) = (b.select(xf, xneg_f, x_ninf), b.select(yf, yneg_f, y_ninf));
            let (xz, yz) = (b.int_bin(IntBinOp::And, xf, xzero_f), b.int_bin(IntBinOp::And, yf, yzero_f));
            let zero = b.int_bin(IntBinOp::Or, xz, yz);
            let neg = b.int_bin(IntBinOp::Xor, xneg, yneg);
            let inf = b.select(neg, ninf, pinf);
            let r = b.select(zero, nan, inf);
            let r = b.select(both_fin, env.zero, r);
            return b.select(any_nan, nan, r);
        }
    };
    let (cx, cy) = (code(b, env, x, &kids[0]), code(b, env, y, &kids[1]));
    let cy = if sub {
        let (pinf, ninf) = (b.iconst(Type::I64, SP_PINF), b.iconst(Type::I64, SP_NINF));
        let (p, n) = (b.icmp(IntCond::Eq, cy, pinf), b.icmp(IntCond::Eq, cy, ninf));
        let r = b.select(p, ninf, cy);
        b.select(n, pinf, r)
    } else {
        cy
    };
    crate::ops::agg::numeric::num_word::add_codes(b, env, cx, cy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_ir::Value as V;

    fn val<'m>(m: Mcx<'m>, lo: u32, kids: PgVec<'m, Val<'m>>) -> Val<'m> {
        Val { lo: V(lo), hi: None, sp: None, ok: V(0), kids, sel: PgVec::new_in(m), ds: None, raw: None }
    }

    /// A subtree is found by its value: numeric_add and numeric_mul commute at any depth (`(1 - d) * p` is `p * (1 - d)`,
    /// `c + b * a` is `a * b + c`), and the reused value's operand parts follow the reader's order. Does not apply:
    /// numeric_sub's operands, another operator, another constant or another column are another value; a memo that is
    /// off keeps nothing.
    #[test]
    fn a_commuted_subtree_is_one_value_with_its_parts_in_the_readers_order() {
        let cx = mcx::MemoryContext::new("num.memo");
        let m = cx.mcx();
        let bx = |t| mcx::box_in(t, m);
        let col = |c| bx(Tree::Col { c, scale: 2, digits: 10 });
        let k = |v| bx(Tree::Const { m: v, scale: 0, digits: 1 });
        let leaf = |lo| val(m, lo, PgVec::new_in(m));
        let pd = Tree::Mul(col(1), bx(Tree::Sub(k(1), col(2))));
        let dp = Tree::Mul(bx(Tree::Sub(k(1), col(2))), col(1));
        assert!(same(&pd, &dp) && same(&dp, &pd));
        let abc = Tree::Add(bx(Tree::Mul(col(1), col(2))), col(3));
        let cba = Tree::Add(col(3), bx(Tree::Mul(col(2), col(1))));
        assert!(same(&abc, &cba));
        assert!(same(&Tree::Add(k(1), col(1)), &Tree::Add(col(1), k(1))), "1 + n is n + 1");
        assert!(!same(&Tree::Sub(col(1), col(2)), &Tree::Sub(col(2), col(1))), "numeric_sub does not commute");
        assert!(!same(&Tree::Add(col(1), col(2)), &Tree::Mul(col(1), col(2))), "another operator");
        assert!(!same(&Tree::Add(col(1), k(1)), &Tree::Add(col(1), k(2))), "another constant");
        assert!(!same(&Tree::Mul(col(1), col(2)), &Tree::Mul(col(1), col(3))), "another column");
        assert!(!same(&Tree::Mul(bx(Tree::Sub(col(1), col(2))), col(3)), &Tree::Mul(col(3), bx(Tree::Sub(col(2), col(1))))), "a commuted product of a commuted difference");
        // the value of `a * b + c` read as `c + b * a`: kids [c, b * a] and the product's [b, a]
        let have = val(m, 10, mcx::pg_vec![in m; val(m, 11, mcx::pg_vec![in m; leaf(12), leaf(13)]), leaf(14)]);
        let got = fit(&cba, &abc, &have, m);
        assert_eq!((got.lo, got.kids[0].lo, got.kids[1].lo), (V(10), V(14), V(11)));
        assert_eq!((got.kids[1].kids[0].lo, got.kids[1].kids[1].lo), (V(13), V(12)));
        let same_order = fit(&abc, &abc, &have, m);
        assert_eq!((same_order.kids[0].lo, same_order.kids[1].lo), (V(11), V(14)));
        let mut memo = Memo::new(true, m);
        memo.put(&abc, have.clone());
        assert!(memo.get(&cba).is_some_and(|v| v.kids[0].lo == V(14) && v.kids[1].kids[0].lo == V(13)));
        assert!(memo.get(&Tree::Sub(col(3), col(1))).is_none());
        memo.cover([&pd].into_iter());
        assert!(memo.covers(&dp) && !memo.covers(&Tree::Mul(col(2), col(1))));
        let mut off = Memo::new(false, m);
        off.put(&abc, have);
        off.cover([&pd].into_iter());
        assert!(off.get(&cba).is_none() && !off.covers(&dp));
    }
}
