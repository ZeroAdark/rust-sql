//! D365's Defect-Test (main 09-24): a partitioned record table (`Spill::Partitioned` on a share-budgeted hash agg: the T1 each
//! worker opens, its evictions buffered per partition until the owner phase) is asked for the records its workers hold, so a
//! worker's T1 over its share of that grant keeps them instead of scattering them to files. TH17's p1 (`avg(l_quantity)` by
//! `l_partkey`: the planner's 59,986,052 rows over 1,964,287 groups, the groups outnumbering the T1's 8,192 slots 240 to 1, so
//! nearly every row is evicted) asked 384 MiB from its partition buffers alone, scattered 44 times at W8 and wrote 2.2 GB of
//! spill runs whose unlink took ~275 ms at the statement's end. The ask is bounded by `HELD_TABLES` of the bits' table (M1-
//! DRIVER's Owner-Ack condition), so a few-group scan (4 groups over 60 M rows) asks next to what it did; at TH17's
//! numbers the bound (2 GiB) sits under the all-rows worst case (2.7 GiB): a worker holds 93 % of its rows, past the ~90 %
//! TH17 evicts (0 scatters at W0 / W8 / W16 on box 18).

use gravity_driver_rig as common;

use gravity_abi::breaker::{opened_bits, Breaker, BreakerKind, Budget, KeyKind, RecordLayout, SlotKind, Spill, T1, t1_default_bytes};
use gravity_driver::{share, Grant};
use gravity_program::cost::breaker::POOL_PART_BYTES;
use gravity_program::cost::{grant_need, spill_bits, HELD_TABLES, PARTITION_BYTES};
use gravity_program::{End, OwnerDesc, Program};

/// `p_group_spill`'s program (a partitioned hash agg into slot 0, its emit) at `rows` predicted input rows over `layout`, its
/// partition bits the planner's for `groups`.
fn program(rows: u64, groups: u64, layout: RecordLayout) -> (Program, u8) {
    let mut p = common::p_group_spill(16);
    let b = p.pipelines[0].breaker().expect("the fixture's breaker");
    assert!(matches!(b, Breaker { kind: BreakerKind::HashAgg, budget: Budget::Share, spill: Spill::Partitioned { .. }, .. }), "{b:?}");
    let bits = spill_bits(groups, layout.size() as u32);
    p.pipelines[0].ends_in = End::Breaker(Breaker { spill: Spill::Partitioned { bits }, ..b });
    p.pipelines[0].predicted_rows = rows;
    p.pipelines[0].owner = Some(OwnerDesc::generic(layout, 0, 0));
    (p, bits)
}

/// The pool's ask (D365's parent, the partition buffers alone, as DR-6 (B) counts them): every worker's block pool at the
/// bits it opens (`POOL_PART_BYTES` a partition, ruling 12's 6 bits at W >= 8) and its T1, plus the owner's pool.
fn pool_ask(p: &Program, bits: u8, w: u64) -> u64 {
    let pool = POOL_PART_BYTES << opened_bits(bits, w as u16, false);
    (pool * (w + 1) + w * t1_default_bytes() as u64).max(pool * p.slots.max_live.max(1) as u64)
}

#[test]
fn a_partitioned_record_table_is_granted_the_records_its_workers_hold() {
    // TH17's p1 at the planner's numbers (EXPLAIN on the SF10 pgrc template): bits 8, the parent's 128 MiB table (384 MiB at 3 live)
    const ROWS: u64 = 59_986_052;
    const GROUPS: u64 = 1_964_287;
    let layout = RecordLayout::new(&[KeyKind::I32], &[SlotKind::AvgNum]);
    let rec = layout.size() as u64;
    let (p, bits) = program(ROWS, GROUPS, layout);
    assert_eq!(bits, 8, "a {rec} B record");
    // widths 4..16 (a W1 share is the whole 2.5 GiB; the ratio is the same at every width)
    for w in [4u16, 8, 16] {
        let ask = grant_need(&p, w).expect("a share breaker");
        assert!(ask < 8 << 30, "W{w}: {ask} B is under the 8 GiB grant cap");
        let worker = share(&p, 0, Grant::of(ask), w).worker.bytes;
        // the worker's T1 over its share (the kernels' layout): its pool's records against the rows its claims evict
        let mut region = vec![0u64; worker as usize / 8];
        // SAFETY: `region` is `worker` zeroed, 8-aligned bytes; the header prefix is zero, so `at` lays the table out.
        let t = unsafe { T1::at(region.as_mut_ptr() as *mut u8, &layout, bits, t1_default_bytes(), worker) };
        // the records a worker is asked for: its rows, up to `HELD_TABLES` of the bits' table
        let asked = ROWS.min(HELD_TABLES * (PARTITION_BYTES << bits) / rec).div_ceil(w as u64);
        let (held, rows) = (t.nblocks as u64 * t.block_recs as u64, ROWS.div_ceil(w as u64));
        eprintln!("W{w}: rec {rec} B, bits {bits}, ask {} MiB, worker {} MiB, holds {held} of {rows} rows ({asked} asked)", ask >> 20, worker >> 20);
        assert!(held >= asked, "W{w}: a {worker} B share (asked {ask} B) holds {held} of the {asked} records ({rec} B) a worker is asked to hold ({rows} rows): it scatters");
    }
}

#[test]
fn a_few_group_partitioned_record_table_asks_within_its_bound() {
    // A few-group scan: 4 groups over 60 M rows (bits floored at 3); the rows alone would ask the grant cap
    let layout = RecordLayout::new(&[KeyKind::I64, KeyKind::I64], &[SlotKind::SumI128, SlotKind::SumI128, SlotKind::Count]);
    let (p, bits) = program(60_000_000, 4, layout);
    let bound = (HELD_TABLES * (PARTITION_BYTES << bits) * 5 / 4) * p.slots.max_live.max(1) as u64;
    for w in [1u64, 8, 16] {
        let ask = grant_need(&p, w as u16).unwrap();
        assert!(ask <= pool_ask(&p, bits, w).max(bound), "W{w}: {ask} B past HELD_TABLES x the bits' table");
        // DR-6 (B): at W >= 8 a share is the pool of the 6 bits the kernels open (12 MiB), 17 of them at W16
        assert!(ask <= 256 << 20, "W{w}: {ask} B for 4 groups");
    }
    // under its partition buffers the rows add nothing: the ask is the pool's
    let (small, bits) = program(1_000, 4, layout);
    for w in [1u64, 8, 16] {
        assert_eq!(grant_need(&small, w as u16), Some(pool_ask(&small, bits, w)), "W{w}");
    }
}
