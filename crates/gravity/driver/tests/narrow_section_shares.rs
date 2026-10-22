//! A statement's sections at different widths stay inside its reservation. `footprint` reserves the widest section's
//! worker shares (W x the share at W); a narrower section's workers each take a larger share (the cut over fewer ways),
//! and the ordinals past its width sit idle. An idle ordinal kept its share region from the wider section, so a width-3
//! section, then a width-2 one, then a width-3 one held 2 x cut/2 + cut/3 of worker regions against the reserved
//! 3 x cut/3, and the next bind reserved the excess past G (ua's parallel union-arm sorts at W8: "reserve 33554432 B past
//! W and G" per statement). The section now hands an idle ordinal's share region back to the depot, and a share region an
//! eighth past the share it binds is swapped for one that fits. One test: it arms the process-wide pool.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use gravity_abi::{Datum, MorselRange};
use gravity_driver::{footprint, Claimed, Entry, Grant, POOL, REGIONS};
use gravity_program::Program;

const NULLS: [u8; 1] = [0];
const BLOCKS: u64 = 24;
const CLAIM: u64 = 3;
const WIDE: u16 = 3;

/// `program` admitted at `footprint(.., WIDE)`, each eligible pipeline a section at its width in `widths`: the rows and
/// the bytes reserved past G.
fn run(program: &Arc<Program>, grant: Grant, widths: &[u16]) -> (Vec<Vec<Datum>>, u64) {
    let res = std::rc::Rc::new(POOL.admit_need(grant, &|g| footprint(g, program, WIDE), Duration::from_millis(10)).expect("admitted"));
    let mut derive = FakeDerive::ranges(RangeTable::new(BLOCKS, 10));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[8], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, program, &mut derive, &mut b, res.clone()).expect("started");
    let grown = res.grown();
    for (p, &width) in widths.iter().enumerate() {
        let open = entry.open_section(&mut exec, &b, &mut derive, p, width).expect("the section binds");
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
        entry.close_section(open, &mut b, &GroupMerge, true).unwrap();
    }
    entry.run_to_end(&mut exec, &mut b).expect("the emit");
    entry.end(exec, true);
    let past = res.grown() - grown;
    drop(res);
    (sink.sorted(), past)
}

#[test]
fn a_narrower_section_between_wider_ones_binds_its_workers_inside_the_reservation() {
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    let program = Arc::new(p_group_chain(64));
    let grant = Grant::of(256 << 20);
    POOL.arm(footprint(grant, &program, WIDE) + (1 << 30));
    let (wide, wide_past) = run(&program, grant, &[WIDE, WIDE, WIDE]);
    assert_eq!(wide_past, 0, "sections at the reserved width bind inside G");
    let (mixed, mixed_past) = run(&program, grant, &[WIDE, 2, WIDE]);
    assert_eq!(mixed_past, 0, "a width-2 section between width-3 ones reserved {mixed_past} B past G");
    assert_eq!(mixed, wide, "the same groups at either widths");
    assert!(!wide.is_empty());

    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0, "every byte back at the statement's end");
    POOL.arm(0);
}
