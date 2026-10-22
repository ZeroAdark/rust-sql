//! A Limit op's position and bounds, read through the interface home (lint C30). A producer bounded by a consumer's Limit
//! reads its request's `bound` (iface.rs `Request`); these answer the questions a Limit's own position asks (whether ops
//! hold one, what its count and offset are as the plan gives them), so no family matches the op itself. `demand` is the
//! one reading of a Limit op; `limit_bounds`, `limit_with_ties` and the request's bound (iface.rs `pass_down`) read it.
use gravity_decompose::graph::Bound as PlanBound;
use gravity_decompose::Op;

/// A Limit's demand on the node under it, as the plan gives it: its count and offset (PG's limitCount / limitOffset, read
/// by nodeLimit.c `recompute_limits`, which passes `count + offset` down as ExecSetTupleBound's bound) and whether it
/// keeps the last row's ties (WITH TIES: it reads on past the count while rows tie).
#[derive(Clone, Copy, Debug)]
pub struct Demand<'o, 'p> {
    pub count: &'o PlanBound<'p>,
    pub offset: &'o PlanBound<'p>,
    pub ties: bool,
}

impl Demand<'_, '_> {
    /// The rows the Limit reads from the node under it at most, `count + offset` (saturating), when both are plan
    /// constants: a count of at least 0 and an offset of at least 0 or none. `None` for no count (ALL or NULL), a bound
    /// known only at run time (a parameter or a computed expression), or a negative constant (PostgreSQL raises at the
    /// Limit's open). WITH TIES may read past it; a consumer that needs a strict bound checks `ties`.
    pub fn rows(&self) -> Option<u64> {
        let off = match self.offset {
            PlanBound::All => 0,
            PlanBound::Const(o) => u64::try_from(*o).ok()?,
            PlanBound::Expr(_) => return None,
        };
        match self.count {
            PlanBound::Const(c) => u64::try_from(*c).ok().map(|c| c.saturating_add(off)),
            _ => None,
        }
    }
}

/// `op`'s demand when it is a Limit (`Demand`), `None` for any other op.
pub fn demand<'o, 'p>(op: &'o Op<'p>) -> Option<Demand<'o, 'p>> {
    match op {
        Op::Limit { count, offset, ties } => Some(Demand { count, offset, ties: ties.is_some() }),
        _ => None,
    }
}

/// A Limit op's count and offset as the plan gives them (its `demand`), `None` for any other op.
pub fn limit_bounds<'o, 'p>(op: &'o Op<'p>) -> Option<(&'o PlanBound<'p>, &'o PlanBound<'p>)> {
    demand(op).map(|d| (d.count, d.offset))
}

/// Whether `ops` hold a Limit op.
pub fn holds_limit(ops: &[Op<'_>]) -> bool {
    ops.iter().any(|o| limit_bounds(o).is_some())
}

/// D1921: whether a Limit in `ops` may hand on no row by a bound PostgreSQL reads only at run time (nodeLimit.c
/// `recompute_limits`): a count that is a parameter or a computed expression, or a negative or zero constant, or an offset
/// that is not a constant >= 0 (`limit_stopping`).
pub fn limit_may_skip(ops: &[Op<'_>]) -> bool {
    ops.iter().any(|o| limit_stopping(o).is_some())
}

/// A Limit op's (count, offset) when its check may stop its call: a count that may be zero or negative, an offset that may
/// be negative (`may`); `None` for every other op. The result gate's feeders and the limit step's `Above` checks ask it,
/// and rerun/union_arms.rs.
pub fn limit_stopping<'o, 'p>(op: &'o Op<'p>) -> Option<(&'o PlanBound<'p>, &'o PlanBound<'p>)> {
    limit_bounds(op).filter(|(count, offset)| may(count, true) || may(offset, false))
}

/// Whether a bound may stop a Limit's call: a value that may be zero (`zero`) or negative.
fn may(b: &PlanBound<'_>, zero: bool) -> bool {
    matches!(*b, PlanBound::Expr(_)) || matches!(*b, PlanBound::Const(v) if v < 0 || zero && v == 0)
}

/// Whether `op` is a Limit whose count and offset are each none (ALL) or a plan constant of at least 0: its step
/// evaluates nothing and raises nothing (nodeLimit.c `recompute_limits` raises only at a negative bound, and computes
/// only a parameter or an expression). `false` for any other op.
pub fn limit_settled(op: &Op<'_>) -> bool {
    let settled = |b: &PlanBound<'_>| matches!(*b, PlanBound::All) || matches!(*b, PlanBound::Const(v) if v >= 0);
    limit_bounds(op).is_some_and(|(count, offset)| settled(count) && settled(offset))
}

/// A Limit's bound as its own step evaluates it (ops/limit `Limit::bound`: its value this call, `None` when it is ALL or
/// NULL, a negative one raising `which`, 0 OFFSET and 1 LIMIT), for a feeder that makes the Limit's check before the
/// Limit runs (ops/result/gate.rs, as PostgreSQL's Limit evaluates its bounds before it pulls its subplan).
pub(crate) fn limit_check_bound(b: &mut gravity_ir::builder::FunctionBuilder<'_>, env: &mut crate::frame::env::Env, l: crate::ops::shared::limit::Lim, which: u64) -> Option<gravity_ir::Value> {
    crate::ops::limit::lower::Limit::bound(b, env, l, which)
}

/// Whether `op` is a Limit WITH TIES: it keeps the last emitted row's tie key images (ops/limit/lower.rs `Tie`) to compare
/// each row past its count with, so a step under it cannot hand it cells that a later fetch moves.
pub fn limit_with_ties(op: &Op<'_>) -> bool {
    demand(op).is_some_and(|d| d.ties)
}

#[cfg(test)]
mod tests {
    use gravity_decompose::graph::Bound as PlanBound;
    use gravity_decompose::Op;

    /// A Limit's count and offset come back as the plan holds them; any other op has none, and ops hold a Limit only when
    /// one of them is one.
    #[test]
    fn a_limits_bounds_and_position() {
        let limit = Op::Limit { count: PlanBound::Const(5), offset: PlanBound::All, ties: None };
        assert!(matches!(super::limit_bounds(&limit), Some((PlanBound::Const(5), PlanBound::All))));
        assert!(super::limit_bounds(&Op::GroupRun(0)).is_none());
        assert!(super::holds_limit(&[Op::GroupRun(0), limit]));
        assert!(!super::holds_limit(&[Op::GroupRun(0)]));
        assert!(!super::holds_limit(&[]));
    }

    /// A Limit may skip every row only by a bound read at run time: a zero or negative count, a negative offset, or either
    /// bound computed; a constant positive count, a zero offset and no bound at all may not, and ops with no Limit never do.
    #[test]
    fn a_limit_skips_by_run_time_bounds_only() {
        let limit = |count, offset| Op::Limit { count, offset, ties: None };
        assert!(super::limit_may_skip(&[limit(PlanBound::Const(0), PlanBound::All)]));
        assert!(super::limit_may_skip(&[limit(PlanBound::Const(-1), PlanBound::All)]));
        assert!(super::limit_may_skip(&[limit(PlanBound::All, PlanBound::Const(-1))]));
        assert!(!super::limit_may_skip(&[limit(PlanBound::Const(5), PlanBound::Const(0))]));
        assert!(!super::limit_may_skip(&[limit(PlanBound::All, PlanBound::All)]));
        assert!(!super::limit_may_skip(&[Op::GroupRun(0)]));
    }

    /// A Limit is settled when each bound is none or a constant of at least 0 (a 0 count too: it hands on no row, and
    /// raises nothing); a negative constant or a computed bound is not, and no other op is.
    #[test]
    fn a_limit_is_settled_by_constant_bounds_of_at_least_zero() {
        let limit = |count, offset| Op::Limit { count, offset, ties: None };
        assert!(super::limit_settled(&limit(PlanBound::Const(2), PlanBound::All)));
        assert!(super::limit_settled(&limit(PlanBound::Const(0), PlanBound::Const(0))));
        assert!(super::limit_settled(&limit(PlanBound::All, PlanBound::Const(3))));
        assert!(!super::limit_settled(&limit(PlanBound::Const(-1), PlanBound::All)));
        assert!(!super::limit_settled(&limit(PlanBound::Const(2), PlanBound::Const(-1))));
        assert!(!super::limit_settled(&Op::GroupRun(0)));
    }

    /// A Limit's demand: its bounds as the plan holds them, its ties, and `count + offset` only when both are plan
    /// constants (count >= 0, offset >= 0 or none); any other op has none.
    #[test]
    fn a_limits_demand_is_its_one_reading() {
        let limit = |count, offset| Op::Limit { count, offset, ties: None };
        let rows = |count, offset| super::demand(&limit(count, offset)).and_then(|d| d.rows());
        assert_eq!(rows(PlanBound::Const(5), PlanBound::All), Some(5));
        assert_eq!(rows(PlanBound::Const(5), PlanBound::Const(3)), Some(8));
        assert_eq!(rows(PlanBound::Const(0), PlanBound::Const(3)), Some(3));
        assert_eq!(rows(PlanBound::Const(i64::MAX), PlanBound::Const(i64::MAX)), Some(u64::MAX - 1));
        assert_eq!(rows(PlanBound::All, PlanBound::Const(3)), None);
        assert_eq!(rows(PlanBound::Const(-1), PlanBound::All), None);
        assert_eq!(rows(PlanBound::Const(5), PlanBound::Const(-1)), None);
        let five = limit(PlanBound::Const(5), PlanBound::All);
        let d = super::demand(&five).expect("a Limit has a demand");
        assert!(matches!((d.count, d.offset, d.ties), (PlanBound::Const(5), PlanBound::All, false)));
        assert!(super::demand(&Op::GroupRun(0)).is_none());
        assert!(!super::limit_with_ties(&limit(PlanBound::Const(5), PlanBound::All)));
    }
}
