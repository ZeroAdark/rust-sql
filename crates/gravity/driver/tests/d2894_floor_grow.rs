//! D2894: a breaker's PostgreSQL floor (D2587) is its region's CAPACITY, not its charge. The one divisor charges the share
//! up to the breaker's modeled need (`share_with`), and the region binds in an address-space reservation of the floor
//! (`grow::floor_reserve`, `reserved_region`): a build the plan under-counts grows IN PLACE up to the floor (`region_grow`
//! commits W, G, then pool bytes past both; no reopen, the base kept), as PostgreSQL's node fills its hash_mem, and a
//! build past the floor reopens as before (D254). `region_grow.rs`'s build, 32 MiB of rows modeled at 1000 rows, under a
//! 64 MiB floor at W0 and W2 (each past the 8 MiB charged), and under a floor charged whole; each against the exact totals.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::{Accounting, Call, Claimed, Entry, Grant, Step, SHARE_FLOOR};
use gravity_program::Program;

const NULLS: [u8; 1] = [0];
const PER_BLOCK: u64 = 64;
/// 512x the 64 KiB share floor in records (16 bytes a row): 32 MiB of rows.
const BLOCKS: u64 = (512 * SHARE_FLOOR as u64 / 16) / PER_BLOCK;
const CLAIM: u64 = 16;
const MIB: u64 = 1 << 20;

fn run(entry: &mut Entry, program: &Arc<Program>, derive: &mut FakeDerive, grant: Grant, width: u16) -> (Vec<Vec<u64>>, Accounting) {
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, program, derive, &mut b, lease(grant)).unwrap();
    if width == 0 {
        assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    } else {
        let open = entry.open_section(&mut exec, &b, derive, 0, width).unwrap();
        let cursor = AtomicU64::new(0);
        'claims: loop {
            for o in 0..width {
                let lo = cursor.fetch_add(CLAIM, Ordering::AcqRel);
                if lo >= BLOCKS {
                    break 'claims;
                }
                assert_eq!(open.run_claim(o, MorselRange::new(lo, (lo + CLAIM).min(BLOCKS))).unwrap(), Claimed::Done);
            }
        }
        entry.close_section(open, &mut b, &GrowMerge, true).unwrap();
    }
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    let acc = exec.node().accounting();
    entry.end(exec, true);
    (sink.sorted(), acc)
}

#[test]
fn d2894_an_undercounted_build_grows_in_place_up_to_its_floor_and_reopens_past_it() {
    let rows = BLOCKS * PER_BLOCK;
    let sum = rows * (rows - 1) / 2;
    let want = vec![vec![rows, sum, sum, sum]];
    let mut derive = FakeDerive::ranges(RangeTable::new(BLOCKS, PER_BLOCK));
    let program = Arc::new(p_grow_share(4));
    let mut entry = Entry::new(2);
    // a 64 MiB hash floor over a 1000-row model: the share charged at the grant's split (8 MiB at least), the floor reserved
    let roomy = Grant::of(SHARE_FLOOR as u64).with_floors(64 * MIB, 64 * MIB);
    let split = gravity_driver::share(&program, 0, roomy, 1).breaker.bytes as u64;
    assert_eq!(split, gravity_driver::FLOOR_CHARGED_MIN, "a 1000-row model charges the floor's first 8 MiB");
    for width in [0u16, 2] {
        let moves = GROW_MOVES.load(Ordering::Relaxed);
        let (got, acc) = run(&mut entry, &program, &mut derive, roomy, width);
        assert_eq!(got, want, "W{width} under a 64 MiB floor: every row, head and tail intact");
        // 32 MiB of rows (16 MiB a worker at W2) past the 8 MiB charged: grown, every grow in the floor's reservation
        assert!(acc.past_share > 0, "W{width}: the build grew past its charged share ({acc:?})");
        assert_eq!(GROW_MOVES.load(Ordering::Relaxed), moves, "W{width}: every grow in place, none reopened ({acc:?})");
    }
    // a floor charged whole (1 MiB: under FLOOR_CHARGED_MIN, nothing reserved): the build reopens past it (D254), as before
    let tight = Grant::of(SHARE_FLOOR as u64).with_floors(MIB, MIB);
    for width in [0u16, 2] {
        let moves = GROW_MOVES.load(Ordering::Relaxed);
        let (got, acc) = run(&mut entry, &program, &mut derive, tight, width);
        assert_eq!(got, want, "W{width} under a 1 MiB floor: every row");
        assert!(acc.past_share > 0 && GROW_MOVES.load(Ordering::Relaxed) > moves, "W{width}: the grows reopen ({acc:?})");
    }
}
