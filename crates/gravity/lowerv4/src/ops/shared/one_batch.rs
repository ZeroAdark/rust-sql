//! USEROP (gate audit AGA-29): a sort keyed by another family's comparator in SQL or a procedural language over a hash
//! join in its pipeline (rep/keylane.rs `USER_BATCHES`) is served where PostgreSQL 18.6 certainly holds the join's table
//! in one batch, at any `work_mem` and `hash_mem_multiplier`. Only batching moves the join's rows (batch 0's, then each
//! later batch's): in one batch they come in the probe's order, and the probe's order is PostgreSQL's when each outer row
//! meets at most one inner row (an inner-unique INNER or LEFT join, a SEMI or ANTI join). A join with more matches per
//! outer row keeps `USER_BATCHES`, since its matches come in the build's chain order, which PostgreSQL reshuffles when it
//! resizes its buckets, unless no sort key tells them apart (D3710, `alike_past`).
//! PostgreSQL sizes its table at ExecHashTableCreate from the Hash's estimate and width (ExecChooseHashTableSize), then
//! adds a batch when a tuple's insert finds `spaceUsed + nbuckets_optimal x 8` past `spaceAllowed`. Both are checked
//! with PostgreSQL's tuple bound (decompose join.rs `pg_tuple`):
//! - At lowering, the estimate, against the least table any setting gives (`BUDGET`; the program is shared across
//!   sessions; a guard of the program, KEY-PREDICTED-ROWS). An estimate that fits it fits every session's table, so
//!   PostgreSQL starts in one batch with the same first bucket count. D3701: past it, the guard carries `DEFER` and the
//!   build's end judges the estimate against this statement's table as well: PostgreSQL starts in one batch exactly when
//!   that table holds the estimate's tuples and buckets, and its first bucket count is then the same at every such table
//!   (the estimate rounded up to a power of two, at least 1024: the pointer cap lies past it). The bound read there is the
//!   first bucket count itself, which no smaller than the estimate.
//! - At the build's end, the rows built, against this statement's table (`allowed`, D3625: the grant's hash ceiling,
//!   PostgreSQL's get_hash_memory_limit()): past it, the build leaves by a run-time refusal (`PAST`), before any row
//!   left the statement (both pipelines run ahead of rows: effects.rs `falls_at_run`); since the row executor's
//!   deletion that is the statement's 0A000, the one place PostgreSQL batches this join where the served run cannot.
//! A served PostgreSQL plan holds no Gather (decompose refuses one), so its Hash is never parallel-aware: the serial
//! batch rule is the only one. A home: sort's ends ask `served`, hashjoin's build end `guard_of` and `check`.
use std::borrow::Cow;

use gravity_decompose::join::JoinDesc;
use gravity_decompose::Op;
use gravity_driver::Error;
use gravity_program::cost::{PG_HASH_MIN_BUCKETS as MIN_BUCKETS, PG_HASH_ONE_BATCH_BYTES as BUDGET};
use gravity_program::LifeId;
use mcx::{CollectIn, GrowIn, Mcx, PgVec};
use types_nodes::jointype::JoinType;

use crate::frame::px::Px;
use crate::graph::Gx;

/// PostgreSQL's sizeof(HashJoinTuple): one bucket pointer.
const BUCKET: u64 = 8;

/// D3701: the bit of a guard's first bucket count (log2 below it) that defers the estimate's check to the build's end.
const DEFER: u8 = 0x80;

/// The run-time refusal of a guarded build whose rows passed `BUDGET`.
pub(crate) const PAST: &str = "a sort keyed by another family's comparator in SQL or a procedural language over a hash join whose build passed the least table PostgreSQL holds in one batch (it may run that join in batches, so a raise would name another pair)";

/// The most bytes PostgreSQL's accounting reaches for `n` tuples of at most `tup` bytes in a table first sized `first`
/// buckets: the tuples plus a pointer per bucket, the count doubling while the tuples outnumber it (NTUP_PER_BUCKET 1).
fn bytes(n: u64, tup: u64, first: u64) -> Option<u64> {
    let buckets = first.max(n.checked_next_power_of_two()?);
    n.checked_mul(tup)?.checked_add(BUCKET.checked_mul(buckets)?)
}

/// `guard_of`'s estimate decision over the build's `rows` (1000 when unset, as ExecChooseHashTableSize reads it) and the
/// tuple bound `a[0]`: log2 of PostgreSQL's first bucket count, with `DEFER` when the estimate passes `BUDGET` with
/// them (D3701); `u64::MAX` when no table holds it.
fn planned(rows: u64, a: &[u64; 4]) -> u64 {
    let est = if rows == 0 { 1000 } else { rows };
    let Some(first) = est.max(MIN_BUCKETS).checked_next_power_of_two() else { return u64::MAX };
    match bytes(est, a[0], first) {
        Some(b) if b <= BUDGET => first.trailing_zeros() as u64,
        Some(_) => first.trailing_zeros() as u64 | DEFER as u64,
        None => u64::MAX,
    }
}

/// Build pipeline `b`'s guard when a pipeline holding `USER_BATCHES`' shape probes it: PostgreSQL's tuple bound and log2
/// of its first bucket count, where the bound is known and the estimate fits `BUDGET` (`planned`). The build end writes
/// them to its header (hashjoin build.rs `GUARD`); `None` otherwise.
pub(crate) fn guard_of(gx: &Gx<'_, '_>, b: usize) -> Option<(u16, u8)> {
    let ps = gx.pipelines();
    let p = &ps[b];
    let Some(Op::Join(JoinDesc::Build { pg_tuple, .. })) = p.ops.last() else { return None };
    let ours = |q: usize| ps[q].ops.iter().any(|o| matches!(o, Op::Join(JoinDesc::Probe { slot, catalog: false, .. }) if gx.life_of(q, *slot) == Some(LifeId(b as u16))));
    if !(0..ps.len()).any(|q| q != b && ours(q) && crate::effects::visible::rep::user_batches(gx, q)) {
        return None;
    }
    let tup = u16::try_from(*pg_tuple).ok()?;
    let first = gravity_lower::inputs::key::decide(b, true, p.desc.build_rows, planned, [tup as u64, 0, 0, 0]);
    (first != u64::MAX).then_some((tup, first as u8))
}

/// Pipeline `k`, `USER_BATCHES`' shape, is served: its run-time refusal falls through (effects.rs `falls_at_run`), `k` runs ahead of rows,
/// there is no drain, and each probe meets at most one inner row per outer row (D3710: or rows the sort cannot tell
/// apart, `alike_past`) over a build that runs ahead of rows and carries a guard (`guard_of`).
pub(crate) fn served(px: &Px<'_, '_>, k: usize) -> bool {
    let gx = px.gx;
    if !px.ahead_of_rows(k) || !crate::effects::falls_at_run(gx) {
        return false;
    }
    let ops = &gx.pipelines()[k].ops[..];
    ops.iter().enumerate().all(|(j, o)| match o {
        Op::Join(JoinDesc::Drain { .. }) => false,
        Op::Join(JoinDesc::Probe { slot, jointype, unique, catalog: false, .. }) => {
            let one = match jointype {
                JoinType::JOIN_SEMI | JoinType::JOIN_ANTI => true,
                JoinType::JOIN_INNER | JoinType::JOIN_LEFT => *unique || alike_past(ops, j, px.mcx()),
                _ => false,
            };
            one && gx.life_of(k, *slot).map(|l| l.0 as usize).is_some_and(|b| px.ahead_of_rows(b) && guard_of(gx, b).is_some())
        }
        _ => true,
    })
}

/// D3710: no key of the sort ending `ops` tells apart the rows one outer row meets at join op `j`: down the projections
/// past `j` (the Sort's own target list first), each key reads only `OUTER_VAR` columns, through filters, gates and later
/// joins. Those rows reach the sort with equal keys, and PostgreSQL's sort reads only its keys (each compare's pair and
/// answer, a comparator's raise among them), so the order its bucket chain hands them on in (reverse insertion, reshuffled
/// by ExecHashIncreaseNumBuckets) moves only ORDER BY peers. A key that reads a column a join at or past `j` adds is not.
fn alike_past(ops: &[Op<'_>], j: usize, mcx: Mcx<'_>) -> bool {
    use types_nodes::primnodes::OUTER_VAR;
    let Some(s) = ops.iter().rposition(|o| matches!(o, Op::Sort(_))) else { return false };
    let Op::Sort(sd) = &ops[s] else { return false };
    let passes = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::Join(JoinDesc::Probe { .. } | JoinDesc::Nest { .. }));
    if s <= j || !ops[j + 1..s].iter().all(passes) {
        return false;
    }
    let (mut tlist, mut at) = (sd.tlist, s);
    let mut cols: PgVec<'_, i16> = sd.keys.iter().map(|key| key.resno).collect_in(mcx);
    loop {
        let mut next: PgVec<'_, i16> = PgVec::new_in(mcx);
        for &c in &cols {
            let Some(t) = (c as usize).checked_sub(1).filter(|&i| i < tlist.len()).and_then(|i| tlist.nth(i).as_target_entry()).filter(|t| t.resno == c) else { return false };
            let outer = gravity_decompose::expr::walk(t.expr, &mut |x| match x.as_var() {
                Some(v) if v.varlevelsup == 0 => {
                    let ok = v.varno == OUTER_VAR && v.varattno > 0;
                    if ok {
                        next.push_in(v.varattno);
                    }
                    ok
                }
                _ => true,
            });
            if !outer {
                return false;
            }
        }
        match ops[..at].iter().rposition(|o| matches!(o, Op::Project(_))) {
            Some(p) if p > j => {
                let Op::Project(l) = &ops[p] else { return false };
                (tlist, at, cols) = (*l, p, next);
            }
            _ => return at != s,
        }
    }
}

/// D3625: the bytes PostgreSQL's table holds in one batch at a statement's hash_mem `limit` (the grant's hash ceiling):
/// `limit` less a 16th, room for the skew table ExecChooseHashTableSize may carve (2 %) and the skew bucket array
/// ExecHashBuildSkewHash charges to spaceUsed (at most as much), as `BUDGET` is of the least hash_mem, 64 kB; never
/// below `BUDGET`, and `BUDGET` when the grant carries no ceiling (0).
fn allowed(limit: u64) -> u64 {
    if limit == 0 { BUDGET } else { BUDGET.max(limit - limit / 16) }
}

/// The build's end over its `n` records (hashjoin raw.rs `one_batch`, from `finish` and `prepare_parts`) at the
/// statement's hash_mem `limit`: a region whose header holds a guard (`tup` not 0, log2 of the first bucket count) and
/// whose records may pass `allowed(limit)` by PostgreSQL's accounting leaves by `PAST`; D3701: so does one whose guard
/// defers the estimate (`DEFER`) when as many tuples as its first bucket count may pass it.
pub(crate) fn check((tup, first): (u16, u8), n: u64, limit: u64) -> gravity_driver::Result<()> {
    let buckets = 1u64.checked_shl((first & !DEFER) as u32).unwrap_or(u64::MAX);
    let fits = |n: u64| bytes(n, tup as u64, buckets).is_some_and(|b| b <= allowed(limit));
    if tup == 0 || fits(n) && (first & DEFER == 0 || fits(buckets)) {
        return Ok(());
    }
    Err(Error::Refused(Cow::Borrowed(PAST)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One int4 column: 16 + MAXALIGN(15 + 1) + 4 + 7 = 43 bytes. 1024 buckets take 8 KiB of the 60.
    #[test]
    fn the_estimate_fits_the_least_one_batch_table() {
        assert_eq!(planned(100, &[43, 0, 0, 0]), 10);
        assert_eq!(planned(0, &[43, 0, 0, 0]), 10, "an unset estimate reads 1000 rows");
        assert_eq!(planned(1100, &[43, 0, 0, 0]), 11 | DEFER as u64, "2048 buckets (16 KiB) and 47 KiB of tuples pass 60 KiB");
        assert_eq!(planned(10, &[u16::MAX as u64, 0, 0, 0]), 10 | DEFER as u64);
        assert_eq!(planned(u64::MAX, &[43, 0, 0, 0]), u64::MAX);
    }

    #[test]
    fn the_rows_built_pass_by_pg_accounting() {
        let first = 1024;
        // 1024 tuples keep 1024 buckets: 1024 x 43 + 8 KiB = 52,224 bytes
        assert!(bytes(1024, 43, first).is_some_and(|b| b <= BUDGET));
        // the 1025th doubles the buckets: 1025 x 43 + 16 KiB = 60,459 bytes, still in; 1100 rows are not
        assert!(bytes(1025, 43, first).is_some_and(|b| b <= BUDGET));
        assert!(bytes(1100, 43, first).is_some_and(|b| b > BUDGET));
        // a first count past the rows keeps its buckets
        assert_eq!(bytes(10, 43, 4096), Some(430 + 8 * 4096));
        assert_eq!(bytes(u64::MAX, 43, first), None);
    }

    #[test]
    fn an_unguarded_region_never_leaves() {
        assert!(check((0, 0), u64::MAX, 0).is_ok());
        assert!(check((43, 10), 1024, 0).is_ok());
        assert!(matches!(check((43, 10), 1100, 0), Err(Error::Refused(_))));
        assert!(matches!(check((43, 63), 1, 0), Err(Error::Refused(_))), "a first count past the budget");
    }

    /// D3625: the rows built are judged at the statement's own hash_mem: PostgreSQL's defaults (4 MB x 2) hold 3,000
    /// int4 rows in one batch where the least table does not; the least hash_mem reads `BUDGET`, as no ceiling does.
    #[test]
    fn the_rows_built_meet_the_session_table() {
        assert_eq!(allowed(64 << 10), BUDGET);
        assert_eq!(allowed(0), BUDGET);
        assert_eq!(allowed(1 << 10), BUDGET, "never below the least table");
        assert!(matches!(check((48, 10), 3000, 0), Err(Error::Refused(_))));
        assert!(matches!(check((48, 10), 3000, 64 << 10), Err(Error::Refused(_))));
        assert!(check((48, 10), 3000, 8 << 20).is_ok());
        // 8 MiB less a 16th: 7.5 MiB. 160,000 rows of 48 bytes and 262,144 buckets (2 MiB) pass it
        assert!(matches!(check((48, 10), 160_000, 8 << 20), Err(Error::Refused(_))));
    }

    /// D3701: an estimate past the least table (sort_aga29_one_batch :101: 5000 int4 key + int4 rows, 8192 first buckets)
    /// is judged at the build's end against the statement's table, bounded by its first bucket count.
    #[test]
    fn a_deferred_estimate_meets_the_session_table() {
        let g = planned(5000, &[48, 0, 0, 0]);
        assert_eq!(g, 13 | DEFER as u64);
        let g = (48, g as u8);
        assert!(check(g, 5000, 8 << 20).is_ok());
        assert!(check(g, 10, 8 << 20).is_ok(), "few rows built over an estimate the table holds");
        assert!(matches!(check(g, 10, 64 << 10), Err(Error::Refused(_))), "the least table holds no such estimate");
        assert!(matches!(check(g, 10, 0), Err(Error::Refused(_))));
        // 400 KiB less a 16th: 375 KiB. 8192 x 48 + 64 KiB = 448 KiB passes it, though 10 rows built would not
        assert!(matches!(check(g, 10, 400 << 10), Err(Error::Refused(_))));
        assert!(check((48, 13), 10, 400 << 10).is_ok(), "the same first count from an estimate the least table holds");
    }
}
