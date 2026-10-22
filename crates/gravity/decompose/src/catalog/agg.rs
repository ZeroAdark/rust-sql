//! The aggregates' catalog names: fmgroids.h's names for the built-in aggregates whose fold the aggregate itself picks
//! (PostgreSQL 18.6 pg_aggregate.dat), with the tests the fold asks of an aggregate by name.

/// avg(int8): its transition (int8_avg_accum) keeps a 128-bit sum.
pub const F_AVG_INT8: u32 = 2100;

/// `fnoid` is avg(int8), whose sum needs the 128-bit average register; avg(int2) / avg(int4) sum in 64 bits.
pub const fn avg_int8(fnoid: u32) -> bool {
    fnoid == F_AVG_INT8
}

/// `fnoid` is min(interval) (2144) or max(interval) (2128) of pg_aggregate (`interval_smaller` / `interval_larger`): their
/// extremum folds natively in an `IntervalMin` / `IntervalMax` slot (PG's 16-byte Interval, ordered by
/// `interval_cmp_value`, a tie keeping the later member).
pub const fn interval_extreme(fnoid: u32) -> bool {
    matches!(fnoid, 2144 | 2128)
}

/// `fnoid` is min(numeric) (2146) or max(numeric) (2130) of pg_aggregate (`numeric_smaller` / `numeric_larger`): the
/// extreme whose keyed form folds the values' order keys (typesem numeric `order_key`), which a pgrcolumnar scan may hand
/// packed codes (decompose ops::shared::scan `packed_minmax_only`).
pub const fn numeric_extreme(fnoid: u32) -> bool {
    matches!(fnoid, 2146 | 2130)
}
