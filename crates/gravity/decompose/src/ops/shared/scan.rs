//! The scan descriptors other families read; their methods stay with ops/scan and ops/scan/closure.
use gravity_program::{ArenaVec, ZoneOp};

use crate::graph::IndexKey;

/// A scan's attributes in attno order (the plan arena's; `limits::LANES` of them at most).
pub struct Atts {
    pub attnos: ArenaVec<i16>,
    /// The scan's tuple position is read (`ctid`, a write's or lock's junk column): the site publishes it as the rowid lane (D4).
    pub rowid: bool,
    /// The system lanes read (`SYS_LANES` bits): whole-row and system column references.
    pub sys: u8,
}

pub const SELF_ITEM_POINTER: i16 = -1;

pub const TSM_BERNOULLI: u32 = 3313;
pub const TSM_SYSTEM: u32 = 3314;

/// The bitmap the heap scan drives over (D5): the index AM's bitmap build is the source's host (as the
/// incumbent's `tbm_*`), so a probe tree is a desc on the source, not a graph breaker (decision 38).
pub enum BitmapPlan<'p> {
    Index { node_id: i32, indexid: u32, keys: ArenaVec<IndexKey<'p>>, rows: u64, iam: crate::catalog::IndexAm },
    And { node_id: i32, plans: ArenaVec<BitmapPlan<'p>> },
    Or { node_id: i32, plans: ArenaVec<BitmapPlan<'p>> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TidForm {
    Points,
    Range,
    /// `WHERE CURRENT OF cursor`: the one point is the named cursor's recorded scan position over this relation (the
    /// clause is the `CurrentOfExpr`: `cursor_name` / `cursor_param`); the position's lookup is the source host's.
    Current,
}

/// The zone comparison of operator `opno`, when the lane's stored integer and the constant's compare as the operator does
/// (OIDW-85..90): a btree comparison (`effects::builtin_btree_member`, its strategy) or the negator of a btree `=` (btree
/// has no `<>` strategy), whose two input types store the same units ([`stored_units`]): the integers across widths,
/// every other signed-word type (date, time, timestamp, timestamptz, money) and oid with itself, text's `=` / `<>` (a
/// code lane's `<> ''`). Never a cross-type date vs timestamp(tz) operator (D257): days against microseconds, and a
/// timestamptz converts through the session TimeZone; normalize's `retype` turns such a comparison with a constant into
/// the same-type one first (SRC-07).
pub fn zone_op(opno: u32) -> Option<ZoneOp> {
    use crate::effects::{builtin_btree_member, builtin_oper};
    let (member, ne) = match builtin_btree_member(opno) {
        Some(m) => (m, false),
        None => (builtin_btree_member(builtin_oper(opno)?.negator)?, true),
    };
    let (_, left, right, strategy) = member;
    let units = stored_units(left).filter(|&u| Some(u) == stored_units(right))?;
    Some(match (ne, strategy) {
        (true, 3) => ZoneOp::Ne,
        (true, _) => return None,
        (false, 3) => ZoneOp::Eq,
        (false, _) if units == Units::Text => return None,
        (false, 1) => ZoneOp::Lt,
        (false, 2) => ZoneOp::Le,
        (false, 4) => ZoneOp::Ge,
        (false, 5) => ZoneOp::Gt,
        _ => return None,
    })
}

/// SRC-09: the zone comparison of a text operator `opno` (text's btree `<`, `<=`, `=`, `>=`, `>`, and `=`'s negator `<>`)
/// for a prefix-key predicate over a text lane: the operator's identity from the btree catalog
/// (`effects::builtin_btree_member`), never an OID list. The caller checks that the comparison's collation orders by bytes,
/// as the writer's prefix keys do (pgrc `MemcmpVarPrefix`, text only under a C-class collation).
pub fn text_zone_op(opno: u32) -> Option<ZoneOp> {
    use crate::effects::{builtin_btree_member, builtin_oper};
    let ((_, left, right, strategy), ne) = match builtin_btree_member(opno) {
        Some(m) => (m, false),
        None => (builtin_btree_member(builtin_oper(opno)?.negator)?, true),
    };
    if stored_units(left) != Some(Units::Text) || stored_units(right) != Some(Units::Text) {
        return None;
    }
    Some(match (ne, strategy) {
        (true, 3) => ZoneOp::Ne,
        (true, _) => return None,
        (false, 1) => ZoneOp::Lt,
        (false, 2) => ZoneOp::Le,
        (false, 3) => ZoneOp::Eq,
        (false, 4) => ZoneOp::Ge,
        (false, 5) => ZoneOp::Gt,
        _ => return None,
    })
}

/// P5-13: the zone comparison of a btree operator `opno` whose two inputs are one type, with that type: `<`, `<=`, `=`,
/// `>=`, `>` by their btree strategy and `=`'s negator as `<>`, from the btree catalog (`effects::builtin_btree_member`),
/// never an OID list. lowerv4's zone verdict keeps the memcmp-ordered fixed types (`gravity_fn::typesem::pgrc::memcmp_fixed`:
/// uuid, macaddr, macaddr8), whose order keys the writer derives from the image's bytes.
pub fn same_type_zone_op(opno: u32) -> Option<(ZoneOp, u32)> {
    use crate::effects::{builtin_btree_member, builtin_oper};
    let ((_, left, right, strategy), ne) = match builtin_btree_member(opno) {
        Some(m) => (m, false),
        None => (builtin_btree_member(builtin_oper(opno)?.negator)?, true),
    };
    if left != right {
        return None;
    }
    let op = match (ne, strategy) {
        (true, 3) => ZoneOp::Ne,
        (false, 1) => ZoneOp::Lt,
        (false, 2) => ZoneOp::Le,
        (false, 3) => ZoneOp::Eq,
        (false, 4) => ZoneOp::Ge,
        (false, 5) => ZoneOp::Gt,
        _ => return None,
    };
    Some((op, left))
}

/// PS1-07: whether `q` is an OR whose every arm is an AND (`x BETWEEN SYMMETRIC a AND b`, the parser's
/// `(x >= a AND x <= b) OR (x >= b AND x <= a)`; two ORed ranges). lowerv4's zone verdict bounds such a qual by the hull of
/// its arms' ranges (verdict_zone.rs `or_hull`), and the content key keys the arms' constants (lower inputs/key.rs
/// `zone_constants`): one shape test for both.
pub fn or_of_ands(q: types_nodes::Node<'_>) -> bool {
    use types_nodes::primnodes::BoolExprType;
    let and = |a: types_nodes::Node<'_>| a.as_bool_expr().is_some_and(|a| a.boolop == BoolExprType::AND_EXPR);
    q.as_bool_expr().is_some_and(|b| b.boolop == BoolExprType::OR_EXPR && b.args.iter().all(and))
}

/// What a zone record's integer counts for a type's lane ([`zone_op`]): two types compare by their stored integers only
/// in the same units. The integers are one value line across widths; any other type whose word is a signed integer
/// (typefacts `WordImage::Raw { signed: true }`: date, time, timestamp, timestamptz, money; pgrc keys their zones
/// sign-extended, `TypeSemantics::SignedInt`) counts its own units (days, microseconds, UTC microseconds, cents), so
/// it compares with itself only; so does an unsigned 4-byte word (oid: pgrc keys its zones zero-extended,
/// `KeyDerivation::UnsignedWordSmall`; its constants are zero-extended to match, [`zone_word`]). Text's lane is a
/// dictionary code: equality alone (the order is the collation's).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Units {
    Integer,
    Own(u32),
    Text,
}

fn stored_units(typid: u32) -> Option<Units> {
    use crate::catalog::typefacts::row_of;
    use types_core::{INT2OID, INT4OID, INT8OID, TEXTOID};
    match typid {
        INT2OID | INT4OID | INT8OID => Some(Units::Integer),
        TEXTOID => Some(Units::Text),
        t => row_of(t).filter(|r| r.image.is_signed_word() || unsigned_word(r)).map(|_| Units::Own(t)),
    }
}

fn unsigned_word(r: &crate::catalog::typefacts::TypeFacts) -> bool {
    r.image.is_unsigned_word() && r.byval && r.typlen == 4
}

/// Whether `typid`'s word meets a zone zero-extended ([`zone_word_of`]): an unsigned 4-byte word (oid, the reg* types).
pub fn zero_extended(typid: u32) -> bool {
    crate::catalog::typefacts::row_of(typid).is_some_and(unsigned_word)
}

/// A non-NULL by-value constant's word in its zone order ([`zone_word_of`]).
pub fn zone_word(c: &types_nodes::primnodes::Const) -> Option<i64> {
    if c.constisnull || !c.constbyval {
        return None;
    }
    zone_word_of(c.consttype, c.constlen, c.constvalue.as_u64())
}

/// Datum `word` of type `typid` (length `len`) in its zone order: an unsigned 4-byte word (oid) zero-extended, as pgrc
/// keys its zones; any other 4-byte or 2-byte word sign-extended; a 1-byte word zero-extended; none for another length.
pub fn zone_word_of(typid: u32, len: i32, word: u64) -> Option<i64> {
    Some(match len {
        1 => word as u8 as i64,
        2 => word as i16 as i64,
        4 if zero_extended(typid) => word as u32 as i64,
        4 => word as i32 as i64,
        8 => word as i64,
        _ => return None,
    })
}

/// NEW-72 (literal-free pgrc zone constants: a literal is never baked into a program): `q` as `col op c` whose constant every reader takes per
/// execution and none by value: `col` a column of range table entry `varno` (or its integer widening, which keeps the
/// word), `op` a [`zone_op`] comparison, `c` a non-NULL by-value constant of 1, 2, 4 or 8 bytes whose word meets the zone
/// as [`zone_word_of`] reads it without the type (never [`zero_extended`]), where `lane_word(attno)` holds. Gives the
/// attno, the comparison with the column on the left and the constant. The shape (`lane_word` always true) is the one
/// rule of four readers, none of which reads the constant: decompose's keyed site holds the predicate's shape
/// (`sources::zone`), the content key skips the constant (lower key.rs `zone_constants`), the front key keys it as a word
/// (exec front.rs), and lowerv4 binds it per execution over a by-value word lane (verdict_zone.rs `of`) and declares
/// nothing over any other.
pub fn bound_form<'p>(q: types_nodes::Node<'p>, varno: i32, lane_word: &dyn Fn(i16) -> bool) -> Option<(i16, ZoneOp, types_nodes::Node<'p>)> {
    let o = q.as_op_expr().filter(|o| o.args.len() == 2)?;
    let col = |n: types_nodes::Node<'_>| {
        let n = match n.as_func_expr() {
            Some(f) if f.args.len() == 1 && types_core::quiet::int_widen(f.funcid) => f.args.nth(0),
            _ => n,
        };
        n.as_var().filter(|v| v.varno == varno && v.varlevelsup == 0 && v.varattno > 0).map(|v| v.varattno)
    };
    let (a0, a1) = (o.args.nth(0), o.args.nth(1));
    let (attno, c, flipped) = match (col(a0), col(a1)) {
        (Some(a), None) => (a, a1, false),
        (None, Some(a)) => (a, a0, true),
        _ => return None,
    };
    c.as_const().filter(|k| k.constbyval && !k.constisnull && [1, 2, 4, 8].contains(&k.constlen) && !zero_extended(k.consttype))?;
    let op = match (zone_op(o.opno)?, flipped) {
        (ZoneOp::Lt, true) => ZoneOp::Gt,
        (ZoneOp::Le, true) => ZoneOp::Ge,
        (ZoneOp::Gt, true) => ZoneOp::Lt,
        (ZoneOp::Ge, true) => ZoneOp::Le,
        (op, _) => op,
    };
    lane_word(attno).then_some((attno, op, c))
}

/// N14 (D2859's lift): whether scan column `attno` of range table entry `varno` is read only as the argument of min /
/// max calls of the Agg right over scan `scan`, so its pgrcolumnar lane may carry the chunks' packed codes
/// (`Rep::PackedNumDyn`, whose only reader is the keyed extreme's step). The scan's quals do not read the column and
/// its target list carries it only as bare entries; the Agg reads those entries only as the one argument of
/// `min(numeric)` / `max(numeric)` with no DISTINCT, ORDER BY or FILTER: no key, no other call, nothing outside such a
/// call. The Agg's reads are the demand home's per-parent table (facts::demand `parent_reads`). Fail-closed: any node
/// the walk does not know, the scan under anything but an Agg's left side, an Agg that table does not hold (grouping
/// sets, a chain, a Gravity rewrite: a chained Agg groups by keys the head's `grpColIdx` does not list). D3139: in an
/// Agg with no group keys, the one argument of a numeric -> float4 / float8 cast within an aggregate's arguments (no
/// DISTINCT, ORDER BY or FILTER) reads the column too: the cast reads the code at its scale (lowerv4
/// rep/packednum/float_cast.rs), any other read of that argument renders the code's image at the row.
pub fn packed_minmax_only<'p>(b: &mut crate::walk::Builder<'p, '_>, scan: &'p types_nodes::plannodes::Plan<'p>, varno: i32, attno: i16) -> bool {
    use types_nodes::primnodes::OUTER_VAR;
    use types_nodes::Node;
    // whether `n` reads a node `hit` names, `None` past a node the walk does not know
    fn reads<'p>(n: Node<'p>, hit: &dyn Fn(Node<'p>) -> bool) -> Option<bool> {
        let mut found = false;
        let known = crate::expr::walk(n, &mut |x| {
            found |= hit(x);
            true
        });
        known.then_some(found)
    }
    let col = |n: Node<'_>| n.as_var().is_some_and(|v| v.varno == varno && v.varlevelsup == 0 && v.varattno == attno);
    if !scan.qual.iter().all(|q| reads(q, &col) == Some(false)) {
        return false;
    }
    let mut outs: ArenaVec<i16> = ArenaVec::new_in(b.arena); // alloc: Cold
    for (i, t) in scan.targetlist.iter().enumerate() {
        let Some(e) = t.as_target_entry().map(|t| t.expr) else { return false };
        if col(e) {
            outs.push(i as i16 + 1);
        } else if reads(e, &col) != Some(false) {
            return false;
        }
    }
    if outs.is_empty() {
        return false;
    }
    // the parent's reads, from the demand home's one per-parent table (an Agg with grouping sets, a chain or a rewrite is
    // not in it: fail closed)
    let Some(pr) = crate::facts::demand::parent_reads(b, scan) else { return false };
    if pr.side != OUTER_VAR || pr.parent.as_agg().is_none() {
        return false;
    }
    let out = |n: Node<'_>| n.as_var().is_some_and(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && outs.contains(&v.varattno));
    let keys = &pr.keys[..];
    if keys.iter().any(|k| outs.contains(k)) {
        return false;
    }
    // every Var of the column in the Agg's lists is the one argument of a min / max over numeric, plain, or (D3139, no
    // group keys) the one argument of a numeric -> float cast within the arguments of an aggregate with no DISTINCT,
    // ORDER BY or FILTER (evaluated at the row, as the extreme's)
    let cast = |e: Node<'_>| e.as_func_expr().is_some_and(|f| crate::catalog::numeric::float_cast(f.funcid).is_some() && f.args.len() == 1 && f.args.iter().next().is_some_and(out));
    let (mut seen, mut calls, mut known, mut deep) = (0usize, 0usize, true, true);
    for &n in pr.exprs.iter() {
        known &= crate::expr::walk(n, &mut |x| {
            seen += out(x) as usize;
            if let Some(r) = x.as_aggref() {
                let arg = r.args.iter().next().and_then(|t| t.as_target_entry()).map(|t| t.expr);
                let free = r.aggdistinct.is_nil() && r.aggorder.is_nil() && r.aggfilter.is_none();
                calls += (crate::catalog::agg::numeric_extreme(r.aggfnoid) && free && r.args.iter().count() == 1 && arg.is_some_and(out)) as usize;
                if free && keys.is_empty() {
                    for t in r.args.iter() {
                        deep &= crate::expr::walk(t, &mut |y| {
                            calls += cast(y) as usize;
                            true
                        });
                    }
                }
            }
            true
        });
    }
    known && deep && calls > 0 && seen == calls
}
