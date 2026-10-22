//! The pgrc granule summary's tally (SRC-21): the predicates whose passing rows in a granule (or a part) a lane's record
//! counts exactly, whatever the window's other rows, and that hold no constant: `x IS NOT NULL` is `nonnull`, over an
//! integral lane (the WHERE form), and as an aggregate FILTER `x IS NULL` (`rows - nonnull`) and a bool lane's `b` /
//! `NOT b` (`nonnull - false_count` / `false_count`: a bool lane's record tallies its false rows, so both of its values
//! are answered). A comparison with a constant (`x = c`, `x <> c`, any `c`) is no tally: it takes the zone verdict every
//! constant takes (an `AllMatch` window is answered from its record, a `NoneMatch` one skipped, a `Mixed` one decoded),
//! so no constant is answered in a window its zone leaves `Mixed`. The site rule (lower.rs) marks a tallied qual's zone
//! predicate `tallied`, so the host answers a window that predicate leaves Mixed; the plain fold's counted answer (ops/agg
//! trans strategies/answer_counted.rs) reads this through frame/iface and folds the record. One place learns a predicate.
//! `x IS NULL` stays out of the WHERE form: an all-NULL window is the zone's NoneMatch for the answer lane's other
//! readers. Owner M1-SOURCE.
use gravity_fn::typesem::integral::int_width;
use gravity_program::ZoneOp;
use types_nodes::primnodes::NullTestType;
use types_nodes::Node;

/// A WHERE predicate over an integral lane `x` whose passing rows a granule record counts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Tally {
    /// `x IS NOT NULL`: `nonnull`.
    NotNull,
}

impl Tally {
    /// The zone operator the site's zone road declares for the predicate (verdict_zone.rs).
    pub(crate) fn zone_op(self) -> ZoneOp {
        match self {
            Tally::NotNull => ZoneOp::NotNull,
        }
    }
}

/// Qual `q` as a tallied predicate over a scan column of an integral type (typesem's integral width: smallint, integer,
/// bigint): `x IS NOT NULL`; its attno and form. A column of a domain over one is never a bare operand here: the parser
/// relabels it to its base type (coerce_type).
pub(crate) fn tally(q: Node<'_>) -> Option<(i16, Tally)> {
    let t = q.as_null_test().filter(|t| !t.argisrow && t.nulltesttype == NullTestType::IS_NOT_NULL)?;
    let v = t.arg?.as_var().filter(|v| int_width(v.vartype).is_some() && v.varattno > 0)?;
    Some((v.varattno as i16, Tally::NotNull))
}

/// An aggregate FILTER's predicate whose passing rows a granule record counts (a count battery
/// `count(*) FILTER (WHERE user_id IS NULL), count(*) FILTER (WHERE is_bot)` with no WHERE): `x IS NULL` (`rows -
/// nonnull`), `x IS NOT NULL` over an integral column (`nonnull`), or a bool column `b` / `NOT b` (`nonnull -
/// false_count` / `false_count`). `col` maps a Var of the fold's row to its scan column (`Some((attno, type))`). No zone
/// predicate stands for it, so `IS NULL` is no harder than the rest here (of any type: its caller takes it over a lane
/// whose record is exact, answer_counted.rs `filter_at`). A comparison with a constant is no tally (module doc).
pub(crate) fn filter_tally(q: Node<'_>, col: &dyn Fn(Node<'_>) -> Option<(i16, u32)>) -> Option<(i16, FilterTally)> {
    let boolean = |v: Node<'_>| col(v).filter(|c| c.1 == types_core::catalog::BOOLOID).map(|c| c.0);
    if let Some(t) = q.as_null_test().filter(|t| !t.argisrow) {
        let c = t.arg.and_then(|v| col(v))?;
        return match t.nulltesttype {
            NullTestType::IS_NULL => Some((c.0, FilterTally::IsNull)),
            _ if int_width(c.1).is_some() => Some((c.0, FilterTally::NotNull)),
            _ => None,
        };
    }
    if let Some(a) = boolean(q) {
        return Some((a, FilterTally::True));
    }
    let e = q.as_bool_expr().filter(|e| e.boolop == types_nodes::primnodes::BoolExprType::NOT_EXPR && e.args.len() == 1)?;
    e.args.iter().next().and_then(boolean).map(|a| (a, FilterTally::False))
}

/// A FILTER's tallied predicate (`filter_tally`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FilterTally {
    /// `x IS NOT NULL` over an integral column: `nonnull`.
    NotNull,
    /// `x IS NULL`: the window's rows less `nonnull`.
    IsNull,
    /// A bool column `b`: `nonnull - false_count`.
    True,
    /// `NOT b`: `false_count`.
    False,
}
