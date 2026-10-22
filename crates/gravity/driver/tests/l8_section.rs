//! L8 at section open: a worker's site words left OPEN or mid probe chain by an earlier section of a parked Node are reset
//! structurally at the next bind (zero_regions clears each site's WINDOW_CELLS), so no section can see a stale word.

use gravity_driver_rig as common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::{Claimed, Entry};
use gravity_abi::site::site::{CONT, NKEYS, SEL, STATE, STATE_CLOSED, STATE_OPEN, WINDOW_CELLS};
use gravity_program::{Generation, Handle, PlanKey};

const KEY: PlanKey = PlanKey { handle: Handle(28), generation: Generation(1) };
const NULLS: [u8; 1] = [0];

#[test]
fn a_worker_site_left_open_by_an_earlier_section_is_reset_at_the_next_bind() {
    let (lo, n) = WINDOW_CELLS;
    assert!(lo == SEL && lo + n == NKEYS && (lo..lo + n).contains(&STATE) && (lo..lo + n).contains(&CONT), "state/cont inside WINDOW_CELLS");
    let table = RangeTable::new(24, 10);
    let mut derive = FakeDerive::ranges(table);
    let program = Arc::new(p_first_row(4));
    let d = program.pipelines[0].driving_site as usize;
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    for (round, stale) in [(0, false), (1, true)] {
        let mut sink = LegSink::new();
        let mut b = binding(&[0], &NULLS, &mut sink, &flag);
        let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
        let open = entry.open_section(&mut exec, &b, &mut derive, 0, 4);
        if !stale {
            let open = open.expect("a fresh Node's workers open clean");
            let claims: Vec<_> = std::thread::scope(|s| {
                let open = &open;
                let hs: Vec<_> = (0..4u16).map(|o| s.spawn(move || open.run_claim(o, MorselRange::new(o as u64 * 6, o as u64 * 6 + 6)))).collect();
                hs.into_iter().map(|h| h.join().unwrap()).collect()
            });
            assert!(claims.iter().all(|c| *c == Ok(Claimed::Done)), "{claims:?}");
            // the Node parks with worker 1's driving site OPEN mid chain, as a claim stopped mid-window leaves it
            unsafe {
                let s = gravity_abi::ctx::site_ptr(open.worker_ctx(1), d);
                ((*s).state, (*s).cont) = (STATE_OPEN, 7);
            }
            entry.close_section(open, &mut b, &NoMerge, true).unwrap();
            entry.end(exec, true);
            continue;
        }
        assert_eq!(round, 1);
        let open = open.expect("the parked Node's workers open clean");
        let words = unsafe {
            let s = gravity_abi::ctx::site_ptr(open.worker_ctx(1), d);
            ((*s).state, (*s).cont)
        };
        assert_eq!(words, (STATE_CLOSED, 0), "round {round}: worker 1's stale site words reset at bind");
        entry.close_section(open, &mut b, &NoMerge, true).unwrap();
        entry.end(exec, true);
    }
}
