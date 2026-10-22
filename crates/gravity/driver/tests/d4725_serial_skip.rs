//! D4725: under `slots::serial_skip` a worker slot never binds the source of a site that only serial pipelines drive (a
//! pgrc merged sorted site's, whose bind is not at-claim). The open of a parallel section over another site then binds
//! no slot on the leader, each claimed slot binds that site's source alone at its claim, and the serial site's source is
//! bound by no slot. Off (the default) the serial site's source, which does not bind at claim, makes the open bind every
//! slot of the width, both sources, on the leader.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::{Claimed, Entry};
use gravity_program::{End, Pipeline, Program, Reason, SiteDesc, WritePlan};

const NULLS: [u8; 1] = [0];
const WIDTH: u16 = 4;
const CLAIMED: u16 = 2;

/// A parallel arm over range site 0 (binds at claim) and a serial arm over index site 1 (does not).
fn program() -> Program {
    let st = gravity_program::Arena::new(1 << 14);
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2]), SiteDesc::index_source(a, 1008, 1108, &[1, 2, 3], &[0, 1, 2], &[3, 4, 5])];
    let pipes = [(Pipeline::eligible(0, End::Sink, 1000), gravity_program::Code::hand(k_range_rows)), (Pipeline::serial(1, End::Sink, Reason::Host), gravity_program::Code::hand(k_range_rows_1))];
    let layout = gravity_program::Layout { nsites: 2, out_natts: 2, out_rows: 4, scratch_bytes: 0, exec_bytes: 0 };
    let cols: [gravity_program::OutCol; 2] = core::array::from_fn(|i| gravity_program::OutCol::new(["c0", "c1"][i], 20, -1, 8, true));
    Program::with_pipelines(layout, &sites, &cols, WritePlan::SELECT, pipes)
}

#[test]
fn a_slot_never_binds_a_site_only_serial_pipelines_drive() {
    gravity_driver::slots::set_bind_at_claim(true);
    let table = RangeTable::new(24, 10);
    table.at_claim.store(true, Ordering::Relaxed);
    let (mut derive, stats) = FakeDerive::ranges(table.clone()).with_stats();
    let program = Arc::new(program());
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    for skip in [false, true] {
        gravity_driver::slots::set_serial_skip(skip);
        let mut sink = LegSink::new();
        let mut b = binding(&[0], &NULLS, &mut sink, &flag);
        let mut exec = entry.start(None, &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
        let (ranges0, index0) = (table.bound_now.load(Ordering::Relaxed), stats.binds.get());
        let open = entry.open_section(&mut exec, &b, &mut derive, 0, WIDTH).unwrap();
        let (ranges, index) = (table.bound_now.load(Ordering::Relaxed) - ranges0, stats.binds.get() - index0);
        let want = if skip { (0, 0) } else { (WIDTH as i64, WIDTH as u64) };
        assert_eq!((ranges, index), want, "skip {skip}: the open's binds (range site, index site)");
        for o in 0..CLAIMED {
            assert_eq!(open.run_claim(o, MorselRange::new(o as u64 * 6, o as u64 * 6 + 6)), Ok(Claimed::Done), "skip {skip}: ordinal {o} claims");
        }
        let after = (table.bound_now.load(Ordering::Relaxed) - ranges0, stats.binds.get() - index0);
        let want = if skip { (CLAIMED as i64, 0) } else { (WIDTH as i64, WIDTH as u64) };
        assert_eq!(after, want, "skip {skip}: the claimed slots bind their range site alone");
        entry.close_section(open, &mut b, &NoMerge, true).unwrap();
        assert_eq!(table.bound_now.load(Ordering::Relaxed), ranges0, "skip {skip}: the close unbinds every bound slot");
        entry.end(exec, true);
    }
    gravity_driver::slots::set_serial_skip(false);
}
