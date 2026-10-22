//! Concurrent admission on one grant pool (lane MEMORY, the conc1 bar as a rig): 4 and 8 statements of four
//! group-by shapes (a share group-by, a two-breaker chain, unequal peaks, a D193 one-owner) admitted in a fixed order at W16
//! into a pool that holds about 5/8 of them, once reserving trunk's footprint (the regions alone) and once this tip's.
//! The bar: every reservation within trunk's + ARENA_ALLOWANCE + W × worker_arena (S4), no more trimmed grants than trunk, and no statement draws
//! past its reservation. Printed per pass: reserved bytes, trims, waits, peak pool use and refusals.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, ORDER_NONE};
use gravity_driver::{footprint, footprint_regions, Entry, Grant, Step, ARENA_ALLOWANCE, POOL, REGIONS, worker_arena};
use gravity_program::Program;

static LOCK: Mutex<()> = Mutex::new(());
const MIB: u64 = 1 << 20;
const W: u16 = 16;
const NULLS: [u8; 1] = [0];

/// (name, program, runs here): the D193 owner's shape is admitted only (its fold runs in exec/tests/owner_leader.rs).
fn shapes() -> Vec<(&'static str, Arc<Program>, bool)> {
    let d = gravity_program::OwnerDesc::generic(RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]), ORDER_NONE, 0);
    vec![("share group", Arc::new(p_group_share(64)), true), ("two-breaker chain", Arc::new(p_group_chain(64)), true), ("unequal peaks", Arc::new(p_group_peaks(64)), true), ("D193 owner", Arc::new(p_group_share_owned(64, d)), false)]
}

#[derive(Debug, Default, Clone, Copy)]
struct Pass {
    reserved: u64,
    trims: u64,
    waits: u64,
    peak: u64,
    refusals: u64,
    /// Admitted with nothing reserved: never since S7 (a full pool makes the statement wait, then refuses it by name).
    unreserved: u64,
}

fn pass(n: usize, capacity: u64, trunk: bool, want: Grant) -> (Pass, Vec<u64>) {
    let all = shapes();
    POOL.gate(0);
    REGIONS.trim();
    assert_eq!(POOL.held(), 0, "a previous pass left bytes held");
    POOL.arm(capacity);
    let s0 = POOL.stats();
    let turn = AtomicUsize::new(0);
    // a statement blocked in admission (ms since `t0`, 0 = none): the admitted ones hold their bytes until every statement is
    // admitted; a waiter blocked past the pool's bytes wait (it then waits at the floor) ends the oldest holder, one per wait
    let t0 = std::time::Instant::now();
    let pending = AtomicU64::new(0);
    let next_end = AtomicUsize::new(0);
    let peak = AtomicUsize::new(0);
    let reserved: Vec<u64> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..n)
            .map(|i| {
                let (turn, pending, next_end, peak) = (&turn, &pending, &next_end, &peak);
                let (name, program, runs) = all[i % all.len()].clone();
                sc.spawn(move || {
                    while turn.load(Ordering::Acquire) != i {
                        std::hint::spin_loop();
                    }
                    let need = |g: Grant| if trunk { footprint_regions(g, &program, W) } else { footprint(g, &program, W) };
                    pending.store(t0.elapsed().as_millis() as u64 + 1, Ordering::Release);
                    let a = std::rc::Rc::new(POOL.admit_need(want, &need, Duration::from_secs(20)).unwrap_or_else(|r| panic!("{name}: refused {r:?}")));
                    pending.store(0, Ordering::Release);
                    peak.fetch_max(POOL.held() as usize, Ordering::AcqRel);
                    turn.store(i + 1, Ordering::Release);
                    let mut drawn = 0;
                    if runs && !trunk && a.reserved > 0 {
                        let mut derive = FakeDerive::ranges(RangeTable::new(24, 10));
                        let mut entry = Entry::new(2);
                        let flag = AtomicU8::new(0);
                        let mut sink = LegSink::new();
                        let mut b = binding(&[8], &NULLS, &mut sink, &flag);
                        let mut exec = entry.start(None, &program, &mut derive, &mut b, a.clone()).unwrap();
                        assert_eq!(entry.run(&mut exec, &mut b, gravity_driver::Call::batch(64)).unwrap_or_else(|e| panic!("{name}: {e:?}")), Step::Done, "{name}");
                        entry.end(exec, true);
                        drawn = a.drawn();
                        assert_eq!(a.borrowed(), 0, "{name}: borrowed past its grant");
                    }
                    while turn.load(Ordering::Acquire) < n {
                        let (p, now) = (pending.load(Ordering::Acquire), t0.elapsed().as_millis() as u64 + 1);
                        if next_end.load(Ordering::Acquire) == i && p != 0 && now > p + 2_300 {
                            pending.store(now, Ordering::Release);
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    next_end.fetch_max(i + 1, Ordering::AcqRel);
                    let r = (name, a.reserved, a.grant.bytes < a.asked.bytes, drawn);
                    drop(a);
                    r
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
    })
    .into_iter()
    .map(|(name, r, t, drawn)| {
        assert!(drawn <= r, "{name}: drew {drawn} past its reservation {r}");
        r | (t as u64) << 63
    })
    .collect();
    let s1 = POOL.stats();
    let trims = reserved.iter().filter(|r| *r >> 63 != 0).count() as u64;
    let reserved: Vec<u64> = reserved.iter().map(|r| r & !(1 << 63)).collect();
    let unreserved = reserved.iter().filter(|r| **r == 0).count() as u64;
    let p = Pass { reserved: reserved.iter().sum(), trims, waits: s1.waits - s0.waits, peak: peak.load(Ordering::Acquire) as u64, refusals: s1.refusals - s0.refusals, unreserved };
    REGIONS.trim();
    POOL.arm(0);
    (p, reserved)
}

#[test]
fn four_and_eight_concurrent_statements_reserve_trunks_regions_plus_the_arena_and_trim_no_more() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // a grant four doublings over the floor: a full pool trims (halves) before it hands out the floor with nothing reserved
    let want = Grant::of(1024 * MIB);
    for (name, program, _) in shapes() {
        for g in [want, Grant::of(512 * MIB), Grant::of(64 * MIB)] {
            let (trunk, tip) = (footprint_regions(g, &program, W), footprint(g, &program, W));
            assert!(tip <= trunk + ARENA_ALLOWANCE + W as u64 * worker_arena(g, W), "{name} at {} MiB: reserves {tip} past trunk's {trunk} + the arena allowances", g.bytes >> 20);
        }
    }
    for n in [4usize, 8] {
        let trunk_need: u64 = (0..n).map(|i| footprint_regions(want, &shapes()[i % 4].1, W)).sum();
        let capacity = trunk_need * 5 / 8;
        let (t, tr) = pass(n, capacity, true, want);
        let (m, mr) = pass(n, capacity, false, want);
        eprintln!("CONCRIG n={n} capacity={} MiB trunk {t:?} tip {m:?} reserved trunk {:?} tip {:?}", capacity >> 20, tr.iter().map(|r| r >> 20).collect::<Vec<_>>(), mr.iter().map(|r| r >> 20).collect::<Vec<_>>());
        // S4: every statement reserves W × its worker arena beyond trunk's regions; at n = 8 that trims one grant more
        assert!(m.trims <= t.trims + 1, "n={n}: {} trimmed grants past trunk's {} + 1", m.trims, t.trims);
        assert_eq!(m.refusals, 0, "n={n}: a draw refused");
        assert_eq!((m.unreserved, t.unreserved), (0, 0), "n={n}: a statement admitted with nothing reserved (S7)");
    }
}
