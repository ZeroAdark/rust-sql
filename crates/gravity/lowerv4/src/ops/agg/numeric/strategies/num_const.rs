//! Numeric tree constants. Hooks / levers: H14 R2-L07. Budget 50 lines (files-extra.tsv, +25 % gate). Owner M1-FOLD.
//!
//! Split from num_tree.rs by responsibility (D267): a plan constant as a `Tree::Const` leaf of a `class.num_packed`
//! argument (an int2 / int4 / int8 at scale 0; a finite numeric at its display scale, at most 18) with its digit count,
//! read through typesem::numeric. A numeric constant past one i64 mantissa word is a `Tree::PastWord` leaf (D2356): a
//! strict tree's 128-bit leaf within the tree's digit bound, else only the non-strict walk's (its argument folds as a
//! datum).
//! num_tree.rs compiles the tree; registers nothing.
use crate::ops::agg::numeric::strategies::num_tree::Tree;
use types_core::catalog::{INT2OID, INT4OID, INT8OID, NUMERICOID};

fn digits_i64(m: i64) -> u32 {
    (m.unsigned_abs().checked_ilog10().unwrap_or(0) + 1).max(1)
}

/// A constant's mantissa at its own display scale (the numeric header's dscale); past one word, a strict tree's leaf only
/// when it fits an i128 within the tree's digit bound.
pub(crate) fn const_of<'g>(c: &types_nodes::primnodes::Const, strict: bool) -> Option<Tree<'g>> {
    if c.constisnull {
        return None;
    }
    let m = match c.consttype {
        INT8OID => c.constvalue.as_usize() as i64,
        INT2OID | INT4OID => c.constvalue.as_i32() as i64,
        NUMERICOID => {
            let n = gravity_fn::typesem::numeric::mantissa(gravity_decompose::expr::const_bytes(c)?)?;
            let scale = i16::try_from(n.dscale).ok().filter(|&s| s <= 18)?;
            return match n.word() {
                Some(m) => Some(Tree::Const { m, scale, digits: digits_i64(m) }),
                None => Some(Tree::PastWord { m: n.value, scale, digits: n.digits }).filter(|t| !strict || (n.value.is_some() && crate::ops::agg::numeric::strategies::num_tree::bounded(t))),
            };
        }
        _ => return None,
    };
    Some(Tree::Const { m, scale: 0, digits: digits_i64(m) })
}
