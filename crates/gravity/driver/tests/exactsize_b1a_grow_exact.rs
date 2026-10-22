//! exactsize B1a: `State::grow_exact` grows the leader region to the counted need where the count is final (a hash build's
//! end: its records, the directory laid next), page-rounded, never to twice its length; `State::grow` (D254) doubles. The
//! statement's grant carries `pgrust.gravity_exact_size` (`Grant::exact`): without `EXACT_BUILD` `grow_exact` IS `grow`,
//! byte for byte. `region_grow.rs`'s build at W2 (32 MiB of rows), its merge's finish growing the folded leader by a
//! little more than it holds, under a floor charged whole (reopens) and under a reserved floor (in place), each run
//! against the exact totals.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::{Merge, Partial, State};
use gravity_driver::{Call, Claimed, Entry, Grant, Step, EXACT_BUILD, SHARE_FLOOR};
use gravity_program::Program;

const NULLS: [u8; 1] = [0];
const PER_BLOCK: u64 = 64;
/// 512x the 64 KiB share floor in records (16 bytes a row): 32 MiB of rows.
const BLOCKS: u64 = (512 * SHARE_FLOOR as u64 / 16) / PER_BLOCK;
const CLAIM: u64 = 16;
const MIB: u64 = 1 << 20;
/// The finish's need past what the folded leader holds: three pages and a byte.
const PAST: usize = 3 * 4096 + 1;

/// The leader's length before and after the finish's grow, and the need it grew to.
static BEFORE: AtomicU64 = AtomicU64::new(0);
static AFTER: AtomicU64 = AtomicU64::new(0);
static NEED: AtomicU64 = AtomicU64::new(0);

/// `GrowMerge` whose finish grows the folded leader to its records + `PAST` through `grow_exact` (the count: its rows), the
/// tail moved to the new end and the limit re-armed as `grow_fold` does.
struct ExactMerge;

impl Merge for ExactMerge {
    fn merge_into(&self, leader: &mut State<'_>, partial: Partial<'_>) -> gravity_driver::Result<()> {
        grow_fold(leader, partial.bytes)
    }

    fn finish(&self, leader: &mut State<'_>) -> gravity_driver::Result<()> {
        unsafe {
            let lh = grow_hdr(leader.bytes.as_mut_ptr());
            let (old, tail, count) = (lh.limit as usize, lh.tail as usize, lh.count);
            let need = old + PAST;
            BEFORE.store(leader.bytes.len() as u64, Ordering::Relaxed);
            NEED.store(need as u64, Ordering::Relaxed);
            leader.grow_exact(need, count)?;
            let len = leader.bytes.len();
            leader.bytes.copy_within(old - tail..old, len - tail);
            grow_hdr(leader.bytes.as_mut_ptr()).limit = len as u32;
            AFTER.store(len as u64, Ordering::Relaxed);
        }
        Ok(())
    }
}

fn run(entry: &mut Entry, program: &Arc<Program>, derive: &mut FakeDerive, grant: Grant) -> Vec<Vec<u64>> {
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, program, derive, &mut b, lease(grant)).unwrap();
    let width = 2u16;
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
    entry.close_section(open, &mut b, &ExactMerge, true).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    entry.end(exec, true);
    sink.sorted()
}

#[test]
fn exactsize_b1a_the_counted_finish_grows_to_its_need_and_none_doubles() {
    let rows = BLOCKS * PER_BLOCK;
    let sum = rows * (rows - 1) / 2;
    let want = vec![vec![rows, sum, sum, sum]];
    let mut derive = FakeDerive::ranges(RangeTable::new(BLOCKS, PER_BLOCK));
    let program = Arc::new(p_grow_share(4));
    let mut entry = Entry::new(2);
    let page = |n: u64| gravity_driver::park::region_bytes(n as usize) as u64;
    // a floor charged whole (1 MiB, nothing reserved): every grow reopens; a 64 MiB floor: reserved, grows in place
    for floors in [MIB, 64 * MIB] {
        let none = Grant::of(SHARE_FLOOR as u64).with_floors(floors, floors);
        assert_eq!(none.trimmed(none.bytes), Grant { trimmed: true, ..Grant::of(none.bytes) }, "a trim drops the floors (marked a trim, D4043); none stays none");
        assert_eq!(run(&mut entry, &program, &mut derive, none), want, "none, floors {floors}: every row");
        let (len, need, after) = (BEFORE.load(Ordering::Relaxed), NEED.load(Ordering::Relaxed), AFTER.load(Ordering::Relaxed));
        assert!(need > len, "the finish's need is past the folded leader ({len} < {need})");
        if floors == MIB {
            assert_eq!(after, page((2 * len).max(need)), "none: a reopen at twice the length");
        } else {
            assert!(after >= need && after <= page((2 * len).max(need)), "none: doubled, at most, within the reservation ({after})");
        }
        let none_after = after;

        let exact = none.with_exact(EXACT_BUILD);
        assert!(exact.exact_build() && !none.exact_build());
        assert_eq!(exact.trimmed(SHARE_FLOOR as u64).exact, EXACT_BUILD, "a trim keeps the exact sizing");
        assert_eq!(run(&mut entry, &program, &mut derive, exact), want, "build, floors {floors}: every row");
        let (len, need, after) = (BEFORE.load(Ordering::Relaxed), NEED.load(Ordering::Relaxed), AFTER.load(Ordering::Relaxed));
        assert!(need > len);
        assert_eq!(after, page(need), "build: the counted need, page-rounded, not twice {len}");
        assert!(after <= none_after, "build never grows past none's ({after} vs {none_after})");
    }
}
