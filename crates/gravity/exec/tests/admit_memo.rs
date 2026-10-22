//! The plan memo: 1,000 live keys cycle with zero misses after warm-up, a
//! full set displaces its LRU, entries die with their handle (allocation
//! counting is tests/memo_alloc.rs, its own binary).

use gravity_exec::admit::memo::{set_index, PlanMemo, Verdict, WAYS};
use gravity_program::{Generation, Handle, PlanKey};

fn key(handle: u64, stmt: u32) -> PlanKey {
    PlanKey { handle: Handle(handle), generation: Generation(7 | stmt << 24) }
}

#[test]
fn a_thousand_live_keys_cycle_without_a_second_walk() {
    let mut memo = PlanMemo::new();
    let keys: Vec<PlanKey> = (0..1000).map(|i| key(0x1000 + i * 0x38, (i % 5) as u32)).collect();
    for (i, k) in keys.iter().enumerate() {
        assert_eq!(memo.probe(*k), None, "cold key {i}");
        let v = if i % 50 == 0 { Verdict::Served((i / 50) as u16) } else { Verdict::Refused };
        assert_eq!(memo.insert(*k, v), None, "a live set overflowed at key {i}");
    }
    let warm = memo.stats();
    assert_eq!(warm.misses, 1000);
    for round in 0..10 {
        for (i, k) in keys.iter().enumerate() {
            let want = if i % 50 == 0 { Verdict::Served((i / 50) as u16) } else { Verdict::Refused };
            assert_eq!(memo.probe(*k), Some(want), "round {round} key {i}");
        }
    }
    let st = memo.stats();
    assert_eq!(st.misses, warm.misses, "no shape walk after warm-up");
    assert_eq!(st.probes, warm.probes + 10_000);
    assert_eq!(st.evictions, 0);
    assert_eq!(memo.len(), 1000);
}

#[test]
fn a_full_set_displaces_its_least_recently_used_entry() {
    let mut memo = PlanMemo::new();
    let base = key(0x5000, 0);
    let same_set: Vec<PlanKey> = (0x5000u64..).map(|h| key(h, 0)).filter(|k| set_index(*k) == set_index(base)).take(WAYS + 1).collect();
    for k in &same_set[..WAYS] {
        assert_eq!(memo.insert(*k, Verdict::Refused), None);
    }
    for k in &same_set[1..WAYS] {
        memo.probe(*k);
    }
    assert_eq!(memo.insert(same_set[WAYS], Verdict::Served(3)), Some((base, Verdict::Refused)));
    assert_eq!(memo.probe(base), None);
    assert_eq!(memo.probe(same_set[WAYS]), Some(Verdict::Served(3)));
    assert_eq!(memo.stats().evictions, 1);
}

#[test]
fn entries_die_with_their_handle_and_replan_walks_once() {
    let mut memo = PlanMemo::new();
    for s in 0..4 {
        memo.insert(key(0x77, s), Verdict::Refused);
    }
    memo.insert(key(0x78, 0), Verdict::Served(1));
    assert_eq!(memo.drop_for(Handle(0x77)), 4);
    assert_eq!(memo.len(), 1);
    for s in 0..4 {
        assert_eq!(memo.probe(key(0x77, s)), None, "statement {s} re-walks once");
    }
    assert_eq!(memo.probe(key(0x78, 0)), Some(Verdict::Served(1)));
    assert_eq!(memo.remove(key(0x78, 0)), Some(Verdict::Served(1)));
    assert!(memo.is_empty());
    memo.insert(key(0x79, 0), Verdict::Refused);
    memo.clear();
    assert!(memo.is_empty());
}

#[test]
fn the_three_verdicts_round_trip_and_the_served_index_stays_below_the_marks() {
    let mut memo = PlanMemo::new();
    memo.insert(key(0x90, 0), Verdict::EntryRefused);
    memo.insert(key(0x90, 1), Verdict::Refused);
    memo.insert(key(0x90, 2), Verdict::Served(u16::MAX));
    assert_eq!(memo.probe(key(0x90, 0)), Some(Verdict::EntryRefused));
    assert_eq!(memo.probe(key(0x90, 1)), Some(Verdict::Refused));
    assert_eq!(memo.probe(key(0x90, 2)), Some(Verdict::Served(u16::MAX - 3)));
}
