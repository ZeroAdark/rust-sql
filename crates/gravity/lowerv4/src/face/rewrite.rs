//! The rewrite face (plan §3.1; H34): facts the planner and FACTS hand the lowerer. v4 reads the facts and declares the
//! uses; their owners build them (cross-crate).
//! - The monotone argument rewrite uses FACTS' monotonicity per function and argument. MIN / MAX(f(x)) folds x and applies f
//!   once per group; MAX - MIN of an affine f is a span; ORDER BY f(x) orders by x.
//! - The planner's shifted-sum rewrite (A70) is consumed, never redone.
//!
//! A rewrite serves only where PG's result is unchanged. f must be strict and immutable, so a NULL x is skipped on both
//! sides, and monotone over the whole domain, including the collation of a text f.
crate::hook_variants! {
    /// The rewrites consumed (H34), and the planner's shifted sums.
    "H34 -" pub enum Rewrite {
        /// MIN / MAX(f(x)) folds x, then applies f once per group (MIN and MAX swap for a decreasing f).
        MinMax = Strategy(EXPR_MONOTONE),
        /// MAX - MIN of an affine f is the span of x.
        Span = Strategy(EXPR_MONOTONE),
        /// ORDER BY f(x) orders by x (DESC for a decreasing f).
        OrderBy = Strategy(EXPR_MONOTONE),
        /// The planner's shifted-sum rewrite (A70), consumed.
        ShiftedSums = Strategy(FINALS_SHIFT),
    }
}

/// A monotone fact: `fnoid` is monotone in argument `arg`, non-decreasing when `increasing` is set, and affine
/// (`a * x + b`) when `affine` is set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Monotone {
    pub fnoid: u32,
    pub arg: u8,
    pub increasing: bool,
    pub affine: bool,
}
