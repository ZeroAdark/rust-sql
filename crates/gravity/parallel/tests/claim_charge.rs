//! Wave 2: a claim cursor's vectors (an ordered walk's order and minima, its part edges, its dead ranges) are on the owner
//! `charge_to` names from then to the cursor's drop, re-synced when they grow.

use gravity_parallel::{ClaimSize, RangeCursor};

#[test]
fn a_cursors_vectors_are_on_its_owner_until_it_drops() {
    std::thread::spawn(|| {
        memledger::bind_backend(3968, 8168);
        memledger::statement_begin();
        let (o, _) = memledger::charge::context();
        let order: Vec<u32> = (0..10_000).collect(); // alloc: Cold
        let mins: Vec<i64> = (0..10_000).collect(); // alloc: Cold
        let mut c = RangeCursor::ordered(&order, &mins, |g| (g / 100) as usize, ClaimSize::Fixed(1));
        assert_eq!(memledger::charge::owner_row(o).committed, 0, "no owner named yet");
        c.charge_to(Some(o));
        let held = memledger::charge::owner_row(o).committed;
        assert!(held >= 10_000 * 12 + 99 * 8, "order + minima + part edges: {held}");
        c.charge_to(None);
        assert_eq!(memledger::charge::owner_row(o).committed, held, "a second call keeps the first owner");
        c.set_dead(&[(0, 5); 1000]);
        assert!(memledger::charge::owner_row(o).committed >= held + 16_000, "a growth re-syncs");
        assert_eq!(c.claim().map(|r| r.lo), Some(5), "the dead range is skipped as before");
        drop(c);
        assert_eq!(memledger::charge::owner_row(o).committed, 0);
        memledger::statement_end();
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}
