//! Tier 0's break-even (gap-optoverfit EX-18): a code-cache miss interprets its first execution when the rows it reads,
//! at the interpreter's extra cost per row, cost at most half the compile it puts off (the next execution compiles: a
//! one-shot statement saves at least half the compile, a repeated one pays at most half of it once). Both costs grow with
//! the program's IR (`lines`: its instructions and block terminators): the compile per line, the interpreter's extra per
//! row with the code a row runs through, which the program's lines stand for.
//!
//! FITTED (`fit::least_squares`) on box 59 (c8g, Neoverse V2), W0, dev sweep lanes/inputs/work/fit_tier0.py: synthetic
//! seed-1001 tables of 64 .. 65,536 rows, 0 .. 32 aggregate terms over a filtered scan (585 .. 9,197 lines), every sample
//! a program never seen before, its first execution with tier 0 off against tier 0 forced (the difference is the compile
//! less the rows' extra), 7 programs a cell, the median; per family the intercept over the rows is the compile and the
//! slope the extra per row (the IR dump that counts the lines is a pass of its own, outside the timings). Measured
//! (lines: compile ms, extra ns a row, break-even rows, the rule's): 585: 0.55, 294, 937, 954; 1,069: 0.70, 577, 605,
//! 677; 1,480: 0.86, 872, 492, 471; 2,052: 1.15, 1,401, 408, 367; 3,131: 1.54, 2,485, 309, 294; 5,153: 2.40, 4,566, 263,
//! 249; 9,197: 3.75, 8,632, 217, 222. The rule is a ratio of two costs of one machine's CPU, so it carries to another
//! machine to first order.

use crate::cost::anchor::Anchor;
use crate::cost::fit::Fitted;

const BOX: &str = "box 59: c8g (Neoverse V2), 64 threads";
const SWEEP: &str = "lanes/inputs/work/fit_tier0.py: seed-1001 tables, 0..32 aggregate terms, 64..65,536 rows, W0";

/// The compile and the interpreter are both branchy work of the core (the sort anchor): one anchor, so the rule holds on
/// every host, as the module says.
///
/// A compile's fixed cost.
pub const COMPILE_FIXED: Fitted = Fitted { ns: 340_200.0, machine: BOX, corpus: SWEEP, anchor: Anchor::Sort };
/// A compile's cost per IR line.
pub const COMPILE_PER_LINE: Fitted = Fitted { ns: 378.06, machine: BOX, corpus: SWEEP, anchor: Anchor::Sort };
/// The interpreter's extra cost per row, per IR line, and its fixed part (negative: a program's setup lines run once).
pub const INTERP_ROW_PER_LINE: Fitted = Fitted { ns: 0.9866, machine: BOX, corpus: SWEEP, anchor: Anchor::Sort };
pub const INTERP_ROW_FIXED: Fitted = Fitted { ns: -505.3, machine: BOX, corpus: SWEEP, anchor: Anchor::Sort };
/// The least extra per row: the smallest program measured (no aggregate term, 585 lines).
pub const INTERP_ROW_FLOOR: Fitted = Fitted { ns: 294.0, machine: BOX, corpus: SWEEP, anchor: Anchor::Sort };

/// The rows at most a program of `lines` IR lines may read for its first execution to run interpreted.
pub fn tier0_rows(lines: u64) -> u64 {
    tier0_rows_running(lines, lines as f64)
}

/// D4005: `tier0_rows` for a program whose rows each run through `per_row` of its `lines` (the rest run on fewer rows than
/// the statement reads: a projection over the rows a filter kept, gravity_backend `mapped_cached_keyed`'s sites): the
/// compile is every line's, the interpreter's extra per row is `per_row` lines'.
pub fn tier0_rows_running(lines: u64, per_row: f64) -> u64 {
    let compile = COMPILE_FIXED.ns + COMPILE_PER_LINE.ns * lines as f64;
    let row = (INTERP_ROW_FIXED.ns + INTERP_ROW_PER_LINE.ns * per_row.clamp(0.0, lines as f64)).max(INTERP_ROW_FLOOR.ns);
    (compile / (2.0 * row)) as u64
}


/// The fewest LEFT / FULL hash probes one pipeline stacks for them to lower the steps after them once (lowerv4
/// ops/hashjoin/probe_share.rs) instead of once for a probe's hits and once for its miss (2^n copies at n stacked probes).
/// MEASURED on box 45, the server's instructions:u per 2,000,000 outer rows LEFT JOINed to n 1,020-row lookups (misses, a
/// NULL key, duplicate matches), shared over copied, median of 3 warm runs, two runs on fresh tables (a fresh ANALYZE
/// sample each, so the plans may differ): n = 1: 1.054; 2: 1.089; 4: 1.091; 5: 1.102; 6: 0.967 / 1.091; 7: 0.961 / 1.24;
/// 8: 0.791 / 0.772 (W8 0.770). The first n where sharing runs no slower in both; the copies' compile is the cost above it
/// (9 stacked probes: 8.73e9 instructions copied, 7.6e7 shared).
pub const FILL_SHARE_MIN_PROBES: usize = 8;
#[cfg(test)]
mod tests {
    use super::tier0_rows;

    #[test]
    fn the_cap_is_half_the_compile_over_the_extra_per_row() {
        // within 15 % of every family measured (lines, break-even rows)
        for (lines, measured) in [(585, 937), (1069, 605), (1480, 492), (2052, 408), (3131, 309), (5153, 263), (9197, 217)] {
            let n = tier0_rows(lines) as f64;
            assert!((n / measured as f64 - 1.0).abs() < 0.15, "{lines} lines: {n} rows against {measured}");
        }
        assert!(tier0_rows(20_000) < tier0_rows(2_000), "a bigger program runs more code a row");
        assert!(tier0_rows(0) > 0);
    }

    #[test]
    fn lines_on_fewer_rows_raise_the_cap() {
        use super::tier0_rows_running;
        // every line on every row is the plain cap; fewer lines a row raise it, never past the floor's
        assert_eq!(tier0_rows_running(20_000, 20_000.0), tier0_rows(20_000));
        assert!(tier0_rows_running(20_000, 2_000.0) > tier0_rows(20_000));
        assert_eq!(tier0_rows_running(20_000, 0.0), tier0_rows_running(20_000, 1.0));
        assert_eq!(tier0_rows_running(20_000, 40_000.0), tier0_rows(20_000));
    }
}
