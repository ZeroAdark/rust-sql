//! The join class's admission bounds (lower.md rung 9 need 6): a lowered build's room in its share, refused by name at admission or named at the wall.

use gravity_abi::breaker::BreakerKind;

use crate::admit::{self, Refused};
use crate::glue;
use crate::inputs::knobs::room::{granted_memory, granted_width};
use crate::runner::refused;
use crate::shapes::Admitted;

/// The program's first pipeline when it is a lowered build, with this statement's estimates (KEY-PREDICTED-ROWS road (b):
/// exec admit `bound`).
pub(crate) fn lowered_build(a: &Admitted) -> Option<gravity_program::Pipeline> {
    let p0 = admit::bound(a, 0)?;
    p0.breaker().filter(|b| b.kind == BreakerKind::HashBuild && b.budget.is_share())?;
    Some(p0)
}

/// The entries a build table of `bytes` holds: `HashBuildPartial::room_for`'s 7/8 load over `table_cap_of`.
pub(crate) fn build_room(bytes: u32) -> u64 {
    let cap = gravity_abi::breaker::HashBuild::table_cap_of(bytes) as u64;
    cap - cap / 8
}

/// `build_room` once the share carries a row store of `row_bytes` a row (`RowStore::split`; 0 = the legacy inline payload); `store_row_bytes` is 0 until the build tail's `RowLayout.bytes` reaches the pipeline (M7 L1).
pub(crate) fn build_room_stored(bytes: u32, row_bytes: u32) -> u64 {
    build_room(bytes - gravity_abi::buildrows::RowStore::split(bytes, row_bytes))
}

pub(crate) fn store_row_bytes(p: &gravity_program::Pipeline) -> u32 {
    p.store_row_bytes
}

/// A lowered join without the kept-outer form (rung 9 need 6: its probe's breaker is not partitioned — an outer row wider than one
/// word) whose build estimate passes the table's room at the GUCs' grant (a worker's share when parallel) is refused by name at admission.
pub(crate) fn build_would_spill(a: &Admitted) -> Option<Refused> {
    if a.program.v4.is_some() {
        // D3355 FIX 2b: a v4 build or spooled drain is never refused by its estimate. A build that spills its own partitions
        // spills by count past what its region can grow to (`exact_gate_skip` fires where its estimate passed that); any
        // other build or spool runs, and only one that really passes one region meets the named 53200 at run time (driver
        // mem/grow.rs) instead of a sure refusal here. KEY-PREDICTED-ROWS road (b): the estimates are this statement's.
        // D4600 (`admit::lean`): the bound pipelines are listed only when a hash build's estimate passes its region (the one
        // case that reads them all, `exact_gate_skip`); `bound` changes no breaker, so the same builds are asked
        if admit::lean::on() && !a.program.pipelines.iter().enumerate().any(|(i, p)| p.breaker().is_some_and(|b| b.kind == BreakerKind::HashBuild) && admit::bound(a, i).is_some_and(|p| v4_build_refused(p.build_estimate(), p.store_row_bytes).is_some())) {
            return None;
        }
        let ps = admit::bound_all(a);
        for (p, slot) in ps.iter().filter_map(|p| Some((p, p.breaker().filter(|b| b.kind == BreakerKind::HashBuild)?.slot))) {
            if v4_build_refused(p.build_estimate(), p.store_row_bytes).is_some() {
                exact_gate_skip(&ps, slot);
            }
        }
        return None;
    }
    let p0 = lowered_build(a)?;
    if a.program.pipelines.get(1).and_then(|p| p.breaker()).is_some_and(|b| matches!(b.spill, gravity_abi::breaker::Spill::Partitioned { .. })) {
        return None;
    }
    let width = if a.is_parallel() && !admit::leader_only() { granted_width() } else { 1 };
    let split = gravity_driver::share(&a.program, 0, granted_memory(width), width);
    let share = if width > 1 { split.worker.bytes } else { split.breaker.bytes };
    (p0.predicted_rows > build_room_stored(share, store_row_bytes(&p0))).then(|| Refused { stage: "join", what: "a build that would spill under an outer row wider than one word (need 6): the estimate passes the hash grant".into() }) // alloc: Cold
}

/// D272 (main 09-24): a v4 build (`Program.v4`) grows past its share instead of spilling (D254), so the legacy table's 7/8
/// room does not bound it. Its memory is accounted three times: the admit reservation (`cost::grant_need` over the
/// `store_row_bytes` its end declares, lowerv4 join/build.rs), the grows past it at run time (W, then G, then pool bytes;
/// `Error::OutOfMemory` when the pool refuses), and here: `rows` records and their directory that no grow can hold (past one
/// region's `MAX_REGION`, or the armed pool's capacity) by the estimate. D3355 FIX 2b: no admission refuses on it any more
/// (`build_would_spill` reads it only for `exact_gate_skip`'s fire); the run meets a real wall as the named 53200.
pub(crate) fn v4_build_refused(rows: u64, row_bytes: u32) -> Option<Refused> {
    let need = v4_build_need(rows, row_bytes);
    let pool = gravity_driver::POOL.capacity();
    let most = gravity_driver::mem::grow::MAX_REGION.min(if pool == 0 { u64::MAX } else { pool });
    (need > most).then(|| Refused { stage: "join", what: format!("a build of {rows} estimated rows ({need} bytes) past what its region can grow to ({most} bytes: one region, the grant pool; D272)").into() }) // alloc: Cold
}

/// exactsize B1e, D3380: build slot `slot` of pipelines `ps` spills its own partitions (the lowering gave it spill bits,
/// M7-SPILL: some pipeline claims spill partitions over its slot, the pair pipeline that joins each spilled partition, the
/// predicate the runner's `producer_ran` reads for a spilled unread build), so its estimate alone does not refuse it at
/// admission (`v4_build_refused`): it spills by count past what its region can grow to, and a partition past one region
/// splits (D2715). This holds at every `pgrust.gravity_exact_size` (that switch sizes the build's grant from its counts,
/// it does not decide whether the build can spill). Fires `exact.build_gate_skip` for such a build whose estimate passed
/// one region (the caller's filter); D3355 FIX 2b: a build that cannot spill is not refused either, it only does not fire.
pub(crate) fn exact_gate_skip(ps: &[gravity_program::Pipeline], slot: u8) -> bool {
    let skip = spills_own(ps, slot);
    if skip {
        fires::fire("exact.build_gate_skip"); // shapecheck: a spillable v4 build past its region by estimate admitted (exactsize B1e)
    }
    skip
}

/// Build slot `slot` spills its own partitions: some pipeline of `ps` claims spill partitions over it (the pair pipeline).
fn spills_own(ps: &[gravity_program::Pipeline], slot: u8) -> bool {
    ps.iter().any(|q| matches!(q.claims, gravity_program::ClaimKind::SpillPartitions { slot: s } if s == slot || q.consumes.contains(&slot)))
}

/// A v4 build's records and directory (`v4_build_refused`'s need).
fn v4_build_need(rows: u64, row_bytes: u32) -> u64 {
    let dir = (rows.saturating_mul(8) / 7).saturating_add(1).checked_next_power_of_two().unwrap_or(u64::MAX).saturating_mul(8);
    rows.saturating_mul(row_bytes.max(16) as u64).saturating_add(dir).saturating_add(4096)
}

/// The same wall met at run time (a build past its estimate), named rather than `unsupported execution (kind 2)`: only a
/// kernel's own status in the build or its probe (pipelines 0 and 1). D1443: any other kind names the pipeline it ended.
pub(crate) fn spilled_build_error(a: &Admitted, e: Box<types_error::PgError>) -> Box<types_error::PgError> {
    let (at, kernel) = gravity_driver::take_unsupported();
    if lowered_build(a).is_some() && kernel && at.is_none_or(|p| p <= 1) {
        if e.message() == glue::unsupported_message(gravity_abi::status::UNSUPPORTED_SCRATCH) {
            return refused("graph: join: a build that spilled past its estimate under an outer row wider than one word, or a partition wider than the scratch (need 6)");
        }
        if e.message() == glue::unsupported_message(gravity_driver::UNSUPPORTED_SPILL) {
            return refused("graph: join: a spilled build partition of one key wider than the hash grant (need 6 skew: the grant overdraft, as PostgreSQL stops growing batches)");
        }
    }
    match at {
        Some(p) if e.message().starts_with("gravity: unsupported execution (kind ") => glue::internal(&format!("{} at pipeline {p}", e.message())), // alloc: Cold
        _ => e,
    }
}

#[cfg(test)]
mod tests {
    use super::{build_room, build_room_stored, exact_gate_skip, spills_own, v4_build_refused};
    use gravity_abi::breaker::{HASHBUILD_ENTRY, HASHBUILD_HDR};

    /// exactsize B1e: a build slot is its own spiller when a pipeline claims spill partitions over it (the pair pipeline
    /// claims the build slot's stream, where the probe kept its rows, spill/pair.rs `Kept`) or consumes it while claiming
    /// partitions; a build no such pipeline reads is not. D3380: the gate skips a build that spills its own partitions at
    /// none too (the switch unset: no GUC installed here), and never one that does not.
    #[test]
    fn a_build_spills_its_own_partitions_only_under_a_pair_pipeline() {
        use gravity_abi::breaker::{Breaker, BreakerKind};
        use gravity_program::{ClaimKind, End, Pipeline};
        let build = Pipeline::eligible(0, End::Breaker(Breaker::sized(BreakerKind::HashBuild, 0)), 1000);
        let probe = Pipeline { consumes: &[0], ..Pipeline::eligible(1, End::Breaker(Breaker::sized(BreakerKind::Materialize, 0).in_slot(1)), 1000) };
        let pair = Pipeline { consumes: &[0], claims: ClaimKind::SpillPartitions { slot: 0 }, ..Pipeline::eligible(-1, End::Sink, 0) };
        assert!(spills_own(&[build, probe, pair], 0), "the pair pipeline claims the build slot's partitions");
        assert!(spills_own(&[build, Pipeline { claims: ClaimKind::SpillPartitions { slot: 2 }, ..pair }], 0), "or consumes the build's slot while claiming partitions");
        assert!(!spills_own(&[build, probe], 0), "no pair pipeline: the build grows by name, never spills");
        assert!(!spills_own(&[build, probe, pair], 1), "a slot no pair reads");
        assert!(exact_gate_skip(&[build, probe, pair], 0), "at none a build that spills its own partitions skips the estimate gate");
        assert!(!exact_gate_skip(&[build, probe], 0), "a build that cannot spill keeps the gate");
        assert!(!exact_gate_skip(&[build, probe, pair], 1), "a slot no pair reads keeps the gate");
    }

    /// D272: a v4 build past the legacy room of any share still admits (it grows); only one no region can hold refuses.
    #[test]
    fn a_v4_build_refuses_only_past_what_a_region_can_grow_to() {
        assert!(build_room(4 << 20) < 300_000, "the W8 statements' 300,000-row builds pass a 4 MiB share's legacy room");
        assert!(v4_build_refused(300_000, 24).is_none());
        assert!(v4_build_refused(60_000_000, 40).is_none(), "2.4 GB of records and a 1 GB directory fit one region");
        assert!(v4_build_refused(200_000_000, 24).is_some(), "4.8 GB of records: past MAX_REGION");
        assert!(v4_build_refused(u64::MAX / 4, 24).is_some());
    }

    /// V4R-481 (pgrc TH09): a v4 build is bounded by the rows its Hash receives (`Pipeline::build_estimate`), not its driving scan's.
    #[test]
    fn a_v4_build_is_bounded_by_the_rows_its_hash_receives() {
        use gravity_program::{End, Pipeline};
        let scan = Pipeline { store_row_bytes: 80, ..Pipeline::eligible(0, End::Sink, 59_986_052) };
        assert!(v4_build_refused(scan.build_estimate(), scan.store_row_bytes).is_some(), "no Hash rows named: the scan's 60 M rows, 5.9 GB");
        let th09 = Pipeline { build_rows: 1_365_827, ..scan };
        assert!(v4_build_refused(th09.build_estimate(), th09.store_row_bytes).is_none(), "TH09's Hash: 1.37 M rows, ~0.13 GB");
        let big = Pipeline { build_rows: 59_986_052, ..scan };
        assert!(v4_build_refused(big.build_estimate(), big.store_row_bytes).is_some(), "a Hash estimate of 5.9 GB still refuses");
    }

    #[test]
    fn the_build_room_is_seven_eighths_of_the_share_table() {
        assert_eq!(build_room(128 << 10), 3584);
        assert_eq!(build_room((HASHBUILD_HDR + 1024 * HASHBUILD_ENTRY) as u32), 896);
        assert_eq!(build_room(4 << 20), 114_688);
        assert_eq!(build_room_stored(4 << 20, 0), 114_688);
        assert_eq!(build_room_stored(4 << 20, 48), build_room((4 << 20) - ((4u32 << 20) * 48 / 80) / 16 * 16));
        assert_eq!(build_room_stored(4 << 20, 48), 57_344);
    }
}
