//! Road `num.share` : num.partial's row terms (fold/num_partial.rs `terms`; split out
//! of it with P1's evaluator, main 09-29 03:3xZ). Hooks / levers: road:num.share; reads road:num.narrow (a column's decode,
//! fold/num_narrow.rs `term`). Budget 120 lines (files-extra.tsv, +25 % gate).
//!
//! A row's terms are its bare columns and its num.window trees (num_tree: columns, constants, +, -, *) as i64 words at
//! their scales; a leaf's NULL or special and every product, rescale, sum and difference past i64 set the row's bad bit
//! (its row takes the record fold). With road num.share: a column is read and gated once per row, not once per reference;
//! a subtree equal to one the row already evaluated is that value (e.g. `e * (1 - d)` under a sum and inside
//! `e * (1 - d) * (1 + t)`); a node whose digits bound (num_tree `digits_of`: its columns' typmod precisions, every column
//! of a tree has one) is at most 18 takes its operation and its sides' rescales unchecked (`1 - d`, `1 + t` over
//! numeric(15, 2): 16 digits). Not shared: subtrees that differ (by a constant, a column or a scale); checked: a node past
//! 18 digits, the block adds (num_partial.rs). Road off: every reference read and every node checked (P1's terms).
//! Kept as is: term equality and the typmod digit bound decide. Does not apply (labelled in e2e
//! v4/m6_th01_p3_levers.sql): no shared subtree, subtrees that differ by a constant, a volatile term, NULL-bearing columns.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntOvfOp, Type, Value};
use mcx::{CollectIn, GrowIn, Mcx, PgVec};

use crate::frame::env::Env;
use crate::frame::row::RowVals;
use crate::admit::refusal::Reason;

use crate::ops::agg::numeric::strategies::num_partial::checked;
use crate::ops::agg::numeric::strategies::num_tree::{digits_of, scale_of, Tree, NARROW};
use crate::ops::agg::hashagg::GroupRec;

crate::strategies! {
    /// Num.partial's terms share a subtree and a column's gate per row, and a node its typmod digits bound takes
    /// unchecked.
    NUM_SHARE = 124, "num.share", NumHeap;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(NUM_SHARE, &Road)], ..crate::ops::NONE };

/// Road num.share's registration value (the sharing is `terms`').
struct Road;

/// A partial's term as `terms` reads it: a bare column `(c, scale)`, a tree, or whether column `c` is not NULL (1 or 0,
/// never bad: count(x)).
pub(crate) enum Of<'t> {
    Col(i16, i16),
    Tree(&'t Tree<'t>),
    Present(i16),
}

/// The row's subtrees already evaluated (num.share): an equal subtree of another term is that value.
pub(crate) type Shared<'t> = Option<PgVec<'t, (&'t Tree<'t>, Value)>>;

/// Row `row`'s terms `ts` and their bad bit (a leaf NULL, special or past a word; a product, rescale or sum past i64);
/// `share`: road num.share; `narrow`: road num.narrow (a column's decode, fold/num_narrow.rs `term`).
pub(crate) fn terms<'m, 't>(b: &mut FunctionBuilder<'_>, env: &mut Env, rec: &GroupRec<'_, '_>, row: &RowVals, ts: &[Of<'t>], share: bool, narrow: bool, mcx: Mcx<'m>) -> (PgVec<'m, Value>, Value) {
    let (mut bad, scratch) = (b.bconst(false), env.mcx);
    // count(x)'s terms first: 1 where x is not NULL, else 0
    let present: PgVec<'_, Value> = ts.iter().filter_map(|t| match t { Of::Present(c) => Some(*c), _ => None }).map(|c| {
        let Some(cell) = row.col(c) else { crate::refuse!(rec.node, Reason::Bug { payload: "a partial's column the row lacks" }) };
        let (null, yes) = (crate::ops::agg::trans::args::null_of(b, env, cell), b.bconst(true));
        let ok = b.int_bin(IntBinOp::Xor, null, yes);
        b.conv(gravity_ir::ConvOp::Zext, Type::I64, ok)
    }).collect_in(scratch);
    let mut present = present.iter().copied();
    let mut seen: mcx::PgVec<'_, ((i16, i16), Value)> = mcx::PgVec::new_in(scratch);
    let mut leaf = |b: &mut FunctionBuilder<'_>, c: i16, scale: i16, bad: &mut Value| {
        if let Some(m) = seen.iter().find(|x| share && x.0 == (c, scale)).map(|x| x.1) {
            return m;
        }
        let Some(cell) = row.col(c) else { crate::refuse!(rec.node, Reason::Bug { payload: "a partial's column the row lacks" }) };
        let (m, x) = crate::ops::agg::numeric::strategies::num_narrow::term(b, env, cell, scale, narrow);
        *bad = b.int_bin(IntBinOp::Or, *bad, x);
        seen.push_in(((c, scale), m));
        m
    };
    let mut shared: Shared<'_> = share.then(|| PgVec::new_in(scratch));
    let vals = ts.iter().map(|t| match t {
        Of::Col(c, scale) => leaf(b, *c, *scale, &mut bad),
        Of::Tree(t) => eval(b, t, &mut bad, &mut leaf, &mut shared),
        Of::Present(_) => present.next().expect("a present term's word"),
    }).collect_in(mcx);
    (vals, bad)
}

/// `x op y`, checked into `bad` unless `exact` (its node's digits bound proves it fits).
fn op(b: &mut FunctionBuilder<'_>, op: IntOvfOp, x: Value, y: Value, bad: &mut Value, exact: bool) -> Value {
    if !exact {
        return checked(b, op, x, y, bad);
    }
    b.int_bin(match op { IntOvfOp::SaddOvf => IntBinOp::Add, IntOvfOp::SsubOvf => IntBinOp::Sub, IntOvfOp::SmulOvf => IntBinOp::Mul }, x, y)
}

/// Tree `t`'s i64 at its scale (num_tree `scale_of`), each leaf column `(c, scale)` read by `leaf`; every product, rescale,
/// sum and difference checked into `bad`. With `share` (num.share): a subtree the row evaluated is its value, and a node
/// whose digits bound is at most 18 takes its operation and its sides' rescales unchecked (the bound is below 2^63; a
/// special leaf is `bad` and its row's terms unread).
pub(crate) fn eval<'t>(b: &mut FunctionBuilder<'_>, t: &'t Tree<'t>, bad: &mut Value, leaf: &mut dyn FnMut(&mut FunctionBuilder<'_>, i16, i16, &mut Value) -> Value, share: &mut Shared<'t>) -> Value {
    crate::stack_check();
    if let Some(v) = share.as_ref().and_then(|m| m.iter().find(|x| x.0 == t)).map(|x| x.1) {
        return v;
    }
    // 18 digits (NARROW): i64 holds every 18-digit decimal (10^18 < 2^63), not every 19-digit one, an i64 limit
    let exact = share.is_some() && digits_of(t) <= NARROW;
    let v = match t {
        Tree::Col { c, scale, .. } => return leaf(b, *c, *scale, bad),
        Tree::Const { m, .. } => return b.iconst(Type::I64, *m),
        Tree::Mul(x, y) => {
            let (vx, vy) = (eval(b, x, bad, leaf, share), eval(b, y, bad, leaf, share));
            op(b, IntOvfOp::SmulOvf, vx, vy, bad, exact)
        }
        Tree::Add(x, y) | Tree::Sub(x, y) => {
            let s = scale_of(t);
            let (vx, vy) = (side(b, x, s - scale_of(x), bad, leaf, share, exact), side(b, y, s - scale_of(y), bad, leaf, share, exact));
            op(b, if matches!(t, Tree::Add(..)) { IntOvfOp::SaddOvf } else { IntOvfOp::SsubOvf }, vx, vy, bad, exact)
        }
        Tree::Int { .. } | Tree::Case(_) | Tree::PastWord { .. } | Tree::Cast { .. } | Tree::Div { .. } => unreachable!("plan takes plain trees alone"),
    };
    if let Some(m) = share {
        m.push_in((t, v));
    }
    v
}

/// Side `x` of a sum or difference at `k` more decimal places (a constant's rescale folded here); `exact`: the sum's.
#[allow(clippy::too_many_arguments)]
fn side<'t>(b: &mut FunctionBuilder<'_>, x: &'t Tree<'t>, k: i16, bad: &mut Value, leaf: &mut dyn FnMut(&mut FunctionBuilder<'_>, i16, i16, &mut Value) -> Value, share: &mut Shared<'t>, exact: bool) -> Value {
    let p = 10i64.pow(k as u32);
    if let Tree::Const { m, .. } = x {
        let v = m.checked_mul(p);
        *bad = if v.is_some() { *bad } else { b.bconst(true) };
        return b.iconst(Type::I64, v.unwrap_or(0));
    }
    let v = eval(b, x, bad, leaf, share);
    if k == 0 {
        return v;
    }
    let p = b.iconst(Type::I64, p);
    op(b, IntOvfOp::SmulOvf, v, p, bad, exact)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_ir::builder::ProgramBuilder;
    use gravity_ir::ConvOp;

    /// Tree `t` over two leaf columns (1 and 2: the words `x`, `y`) through the interpreter: its i64 and whether it is bad.
    fn eval_run(t: &Tree, x: i64, y: i64) -> (i64, bool) {
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("t", &[Type::I64, Type::I64], &[Type::I64, Type::I64]);
        let mut b = pb.define(f);
        let p = b.params().to_vec();
        let mut bad = b.bconst(false);
        let v = eval(&mut b, t, &mut bad, &mut |_, c, _, _| p[c as usize - 1], &mut None);
        let bad = b.conv(ConvOp::Zext, Type::I64, bad);
        b.ret(&[v, bad]);
        let prog = pb.finish();
        let got = gravity_interp::Interp::new(&prog).run(f, &[x as u64, y as u64]).expect("runs");
        (got[0] as i64, got[1] == 1)
    }

    /// TH01's `e * (1 - d)` at scale 4 over numeric(15, 2) words: exact, the constant rescaled at lowering; a factor past
    /// 2^31 still exact while the product fits; a product, a rescale or a difference past i64 is bad (the row fold's).
    #[test]
    fn a_term_is_exact_or_bad() {
        let cx = mcx::MemoryContext::new("num.share");
        let bx = |t| mcx::box_in(t, cx.mcx());
        let col = |c, scale| bx(Tree::Col { c, scale, digits: 15 });
        let one = bx(Tree::Const { m: 1, scale: 0, digits: 1 });
        let t = Tree::Mul(col(1, 2), bx(Tree::Sub(one, col(2, 2))));
        assert_eq!(eval_run(&t, 1_050_000, 4), (1_050_000 * 96, false));
        assert_eq!(eval_run(&t, -3_000_000_000, -7), (-3_000_000_000 * 107, false), "a factor past 2^31");
        assert!(eval_run(&t, i64::MAX / 50, 4).1, "the product past i64");
        assert!(eval_run(&t, 5, i64::MIN + 50).1, "the difference past i64");
        let s = Tree::Add(col(1, 0), col(2, 4));
        assert_eq!(eval_run(&s, -12, 34_567), (-120_000 + 34_567, false));
        assert!(eval_run(&s, i64::MAX / 1000, 0).1, "the rescale past i64");
    }

    /// TH01's `e * (1 - d)` and `e * (1 - d) * (1 + t)` over numeric(15, 2) (columns 1, 2, 3) with and without the share
    /// memo, and `e * (2 - d)` (a subtree that differs by a constant): `(terms, bad, leaf reads)` for the words `e`, `d`, `t`.
    fn share_run(share: bool, e: i64, d: i64, t: i64) -> ([i64; 3], bool, u32) {
        let cx = mcx::MemoryContext::new("num.share");
        let bx = |t| mcx::box_in(t, cx.mcx());
        let col = |c| bx(Tree::Col { c, scale: 2, digits: 15 });
        let k = |m| bx(Tree::Const { m, scale: 0, digits: 1 });
        let e1d = Tree::Mul(col(1), bx(Tree::Sub(k(1), col(2))));
        let e1d1t = Tree::Mul(bx(Tree::Mul(col(1), bx(Tree::Sub(k(1), col(2))))), bx(Tree::Add(k(1), col(3))));
        let e2d = Tree::Mul(col(1), bx(Tree::Sub(k(2), col(2))));
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("t", &[Type::I64, Type::I64, Type::I64], &[Type::I64, Type::I64, Type::I64, Type::I64]);
        let mut b = pb.define(f);
        let p = b.params().to_vec();
        let (mut bad, mut reads, mut shared) = (b.bconst(false), 0, share.then(|| PgVec::new_in(cx.mcx())));
        let mut leaf = |_: &mut FunctionBuilder<'_>, c: i16, _: i16, _: &mut Value| {
            reads += 1;
            p[c as usize - 1]
        };
        let v: Vec<Value> = [&e1d, &e1d1t, &e2d].into_iter().map(|t| eval(&mut b, t, &mut bad, &mut leaf, &mut shared)).collect(); // alloc: Program
        let bad = b.conv(ConvOp::Zext, Type::I64, bad);
        b.ret(&[v[0], v[1], v[2], bad]);
        let prog = pb.finish();
        let got = gravity_interp::Interp::new(&prog).run(f, &[e as u64, d as u64, t as u64]).expect("runs");
        ([got[0] as i64, got[1] as i64, got[2] as i64], got[3] == 1, reads)
    }

    /// num.share: the second term's `e * (1 - d)` is the first's value (only `t` read again); `e * (2 - d)` differs by a
    /// constant and is evaluated (its columns read again: the memo shares subtrees, `terms` shares columns); the terms are
    /// the unshared ones; `1 - d` and `1 + t` (16 digits) take no overflow check, the products (31, 47 digits) keep theirs.
    #[test]
    fn a_shared_subtree_is_read_once_and_a_bounded_sum_is_unchecked() {
        let (e, d, t) = (10_494_950, 7, 4);
        let want = [e * (100 - d), e * (100 - d) * (100 + t), e * (200 - d)];
        assert_eq!(share_run(false, e, d, t), (want, false, 7));
        assert_eq!(share_run(true, e, d, t), (want, false, 5));
        // a word past the column's typmod bound (never a heap value of numeric(15, 2)): unshared, 1 - d overflows and is
        // bad; shared, it is unchecked (the bound proves a column's value never reaches it), and the product 0 * x is 0
        assert!(share_run(false, 0, i64::MIN + 50, 0).1);
        assert!(!share_run(true, 0, i64::MIN + 50, 0).1);
        assert!(share_run(true, i64::MAX / 50, 4, 0).1, "a product past i64 is still bad");
    }
}
