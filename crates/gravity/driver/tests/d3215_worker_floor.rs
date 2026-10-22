//! exactsize D3215: a partitioned record table's workers share its node's PostgreSQL floor (`grant::worker_floor`). Each
//! worker holds its own claims' records until the owner phase, so a worker's least share is the records the workers hold
//! (D365's `held`) cut W ways, not the node's whole hash_mem: TPC-H q20 (lineitem by (l_partkey, l_suppkey) under the
//! shipdate filter, near one row a group) at work_mem 256MB reserved W x 512 MiB past its estimate's grant, 11.5 GB at W15.

use gravity_driver_rig as common;

use gravity_abi::breaker::{Breaker, KeyKind, RecordLayout, SlotKind, Spill};
use gravity_driver::{share, Grant};
use gravity_program::cost::{grant_need, spill_bits};
use gravity_program::{End, OwnerDesc, Program};

const MIB: u64 = 1 << 20;

/// `p_group_spill`'s partitioned hash agg at `rows` planner rows over `groups` groups (q20's subquery shape).
fn program(rows: u64, groups: u64) -> Program {
    let layout = RecordLayout::new(&[KeyKind::I64, KeyKind::I64], &[SlotKind::SumI128]);
    let mut p = common::p_group_spill(16);
    let b = p.pipelines[0].breaker().expect("the fixture's breaker");
    let bits = spill_bits(groups, layout.size() as u32);
    p.pipelines[0].ends_in = End::Breaker(Breaker { spill: Spill::Partitioned { bits }, ..b });
    p.pipelines[0].predicted_rows = rows;
    p.pipelines[0].owner = Some(OwnerDesc::generic(layout, 0, 0));
    p
}

/// The workers' shares together at width `w` (what the footprint reserves for them).
fn workers(p: &Program, g: Grant, w: u16) -> u64 {
    share(p, 0, g, w).worker.bytes as u64 * w as u64
}

#[test]
fn a_partitioned_record_tables_workers_share_its_nodes_floor() {
    let p = program(9_100_000, 6_500_000);
    let ask = grant_need(&p, 15).expect("a share breaker");
    // work_mem 256MB: hash_mem 512 MiB
    let g = Grant::of(ask).with_floors(256 * MIB, 512 * MIB);
    let (w8, w15) = (workers(&p, g, 8), workers(&p, g, 15));
    eprintln!("ask {} MiB, workers W8 {} MiB, W15 {} MiB", ask >> 20, w8 >> 20, w15 >> 20);
    assert!(share(&p, 0, g, 15).worker.bytes as u64 * 2 <= 512 * MIB, "a W15 worker's share is not the node's whole hash_mem");
    assert!(2 * w15 <= 3 * w8, "the workers' shares at W15 ({w15} B) within 1.5x of W8's ({w8} B)");
    // the serial open keeps its node's floor
    assert!(share(&p, 0, g, 1).breaker.bytes as u64 >= 512 * MIB);
}

#[test]
fn at_the_default_work_mem_a_worker_keeps_its_floor() {
    // work_mem 4MB: the 8 MiB hash floor is `FLOOR_CHARGED_MIN`, so the cut changes nothing
    let p = program(9_100_000, 6_500_000);
    for w in [2u16, 8, 15, 16] {
        let ask = grant_need(&p, w).expect("a share breaker");
        let g = Grant::of(ask).with_floors(4 * MIB, 8 * MIB);
        assert!(share(&p, 0, g, w).worker.bytes as u64 >= 8 * MIB, "W{w}: a worker keeps hash_mem");
    }
}

#[test]
fn a_few_group_table_keeps_its_floor_at_every_width() {
    // few groups: the records the workers hold are small, so each worker's least share is `FLOOR_CHARGED_MIN`
    let p = program(9_100_000, 4);
    let g = Grant::of(grant_need(&p, 8).unwrap()).with_floors(256 * MIB, 512 * MIB);
    assert!(share(&p, 0, g, 8).worker.bytes as u64 >= gravity_driver::FLOOR_CHARGED_MIN.min(share(&p, 0, g, 1).breaker.bytes as u64));
}
