//! S2, the one divisor: `share(program, pipeline, grant, width)` sizes both the reservation (`footprint_regions`) and every limit
//! the Node opens (a leader slot region, a worker's share or fixed buffer). Per breaker kind, at W1 and W4, a fresh Node's opened
//! limits sum to the reservation exactly, and each section's owners cut the leader region at `share(..).owner` (the HAVING
//! admission's owner bytes read the same field).

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::{footprint_regions, share, Call, Claimed, Entry, Grant, Merge, Step, REGIONS};
use gravity_program::cost::FALLBACK_RECORD;
use gravity_program::{Parallel, Program};

const NULLS: [u8; 1] = [0];
const BLOCKS: u64 = 24;
const MIB: u64 = 1 << 20;

/// The limits a fresh Node opens running `program` at `grant` over `width` (sections for eligible pipelines when `width` > 1).
fn opened(program: &Arc<Program>, grant: Grant, width: u16, merge: &dyn Merge) -> u64 {
    opened_under(program, lease(grant), width, merge)
}

fn opened_under(program: &Arc<Program>, res: std::rc::Rc<gravity_driver::Reservation>, width: u16, merge: &dyn Merge) -> u64 {
    let grant = res.grant;
    let mut derive = FakeDerive::ranges(RangeTable::new(BLOCKS, 10));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[8], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, program, &mut derive, &mut b, res).unwrap();
    for p in 0..program.pipelines.len() {
        if width > 1 && program.pipelines[p].parallel == Parallel::Eligible {
            let open = entry.open_section(&mut exec, &b, &mut derive, p, width).unwrap();
            let split = share(program, p, grant, width);
            assert_eq!(open.owner_bytes(), split.owner.bytes, "pipeline {p} W{width}: the owners' cut is share(..).owner");
            assert_eq!(split.owner.bytes, (split.breaker.bytes - gravity_abi::breaker::OWNED_HDR as u32) / width as u32 & !7, "behind the Owned header, width ways");
            let cursor = AtomicU64::new(0);
            for o in 0..width {
                loop {
                    let lo = cursor.fetch_add(5, Ordering::AcqRel);
                    if lo >= BLOCKS || open.run_claim(o, MorselRange::new(lo, (lo + 5).min(BLOCKS))).unwrap() == Claimed::Stopped {
                        break;
                    }
                }
            }
            entry.close_section(open, &mut b, merge, true).unwrap();
        } else {
            assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
        }
    }
    let limits = exec.node().accounting().limits;
    entry.end(exec, true);
    limits
}

#[test]
fn the_reservation_is_the_sum_of_the_opened_limits_for_every_breaker_kind_at_w1_and_w4() {
    let grant = Grant::of(64 * MIB);
    let cases: [(&str, Program, &dyn Merge); 6] = [
        ("plain agg, fixed", p_count(4), &CountMerge),
        ("two fixed slots", p_two_slots(4), &CountMerge),
        ("hash agg, share", p_group_share(4), &GroupMerge),
        ("chain of three shares", p_group_chain(4), &GroupMerge),
        ("unequal peaks", p_group_peaks(4), &GroupMerge),
        ("share with images", p_group_images(4), &GroupMerge),
    ];
    let mut bad = Vec::new();
    // D2587: with PostgreSQL's per-node budgets as floors the reservation still sums the opened limits (these 1000-row
    // breakers' models stop the floors at the even split, D2894; the floors above it: `d2894_a_floor_stops_at..`)
    let floored = grant.with_floors(24 * MIB, 48 * MIB);
    for (name, program, merge) in cases {
        let program = Arc::new(program);
        for (g, tag) in [(grant, ""), (floored, " floored")] {
            for width in [1u16, 4] {
                REGIONS.trim();
                let (reserved, got) = (footprint_regions(g, &program, width), opened(&program, g, width, merge));
                eprintln!("ONE-DIVISOR {name}{tag} W{width}: reserved {reserved} opened {got}");
                if reserved != got {
                    bad.push(format!("{name}{tag} W{width}: reserved {reserved}, opened {got}"));
                }
            }
        }
    }
    assert!(bad.is_empty(), "reservation != the opened limits: {bad:#?}");
}

/// D2894: a breaker's PostgreSQL floor stops at its modeled need (`cost::breaker::of`: 64-byte records, so 1000 rows ask
/// 64 KiB, 500,000 ask 32 MiB, 10 M ask 1 GiB). Under a 32 MiB grant with 24 / 48 MiB floors a chain of three hash
/// aggregates keeps the even split, its 32 MiB model, or the 48 MiB floor, and the reservation still sums the opened
/// limits. At the pgrc bar's posture (a 364 MiB grant, work_mem 512MB x hash_mem_multiplier 8) the 1000-row chain
/// reserves the grant's split, not three 4 GiB regions.
#[test]
fn d2894_a_floor_stops_at_the_modeled_need_and_the_reservation_sums_the_opened_limits() {
    assert_eq!(FALLBACK_RECORD, 64, "the chain's breakers have no owner layout: the fallback record");
    let floored = Grant::of(32 * MIB).with_floors(24 * MIB, 48 * MIB);
    for (rows, model) in [(1000u64, None), (500_000, Some(32 * MIB)), (10_000_000, Some(48 * MIB))] {
        let program = Arc::new(p_group_chain_rows(4, rows));
        for width in [1u16, 4] {
            let split = share(&program, 0, Grant::of(32 * MIB), width).breaker.bytes as u64;
            assert!(split < 32 * MIB, "{rows} rows W{width}: the even split {split} is under the model");
            // a floor's first FLOOR_CHARGED_MIN (8 MiB) is charged whatever the model; the rest of it is address space
            assert_eq!(share(&program, 0, floored, width).breaker.bytes as u64, model.unwrap_or(split.max(gravity_driver::FLOOR_CHARGED_MIN)), "{rows} rows W{width}");
            REGIONS.trim();
            let (reserved, got) = (footprint_regions(floored, &program, width), opened(&program, floored, width, &GroupMerge));
            assert_eq!(reserved, got, "{rows} rows W{width}: the reservation is the opened limits");
        }
    }
    let bar = Grant::of(364 * MIB).with_floors(512 * MIB, 4096 * MIB);
    let program = p_group_chain_rows(4, 1000);
    for width in [1u16, 8] {
        let fp = gravity_driver::footprint(bar, &program, width);
        assert!(fp <= gravity_driver::footprint(Grant::of(364 * MIB), &program, width), "W{width}: {fp} past the floorless grant's");
    }
}

/// Ruling 5 (main 09-27): the grant end's resident peak, noted at each section close and pipeline end, covers the largest sum
/// of live limits (every live slot's region is resident) at W1 and W4, from 0 on a fresh reservation.
#[test]
fn the_resident_peak_covers_the_live_limits_at_w1_and_w4() {
    let program = Arc::new(p_group_share(4));
    for width in [1u16, 4] {
        REGIONS.trim();
        let res = std::rc::Rc::new(gravity_driver::Reservation::unadmitted(Grant::of(64 * MIB)));
        assert_eq!(res.resident_peak(), 0);
        let limits = opened_under(&program, res.clone(), width, &GroupMerge);
        eprintln!("RESIDENT-PEAK W{width}: peak {} limits {limits}", res.resident_peak());
        assert!(limits > 0 && res.resident_peak() >= limits, "W{width}: peak {} under the live limits {limits}", res.resident_peak());
    }
}
