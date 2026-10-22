//! Shuttle model (suspect B): the claimer's fill (`prefill` on this thread: take a superpage of room, find no Cold
//! superpage, give the room back) races the ladder's sample `set_claim_room(0)`. The ladder's 0 is its last word, so
//! the room left is 0 whatever the order. Real crate; every atomic of the shared state is a scheduling point
//! (src/sync.rs). Runs only with RUSTFLAGS="--cfg shuttle".
#![cfg(shuttle)]

use pagepool::{Class, Config, Hint, Tag};
use shuttle::thread;

const SUPER: usize = 2 << 20;

fn setup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let cfg = Config {
            va: 4 << 20,
            chunk: 4 << 20,
            lazy_va: 0,
            frame_va: 0,
            shelf_max: 64 << 10,
            warm_zero: 0,
            warm_zero_max: 0,
            warm_cap: 0,
            decay_ms: 0,
            purge_batch: 2 << 20,
            lazy_min: 32 << 20,
            idle_drain_ms: 1000,
            block_min: 1024,
            release_mode: 2,
            lazy_decay_ms: 0,
            lazy_cap: 0,
            age_pass_max: 0,
            thp_partial: 0,
            host_age_ms: 0,
        };
        assert!(pagepool::init(&cfg));
        // every superpage held for good: a fill finds no Cold one and gives its room back
        while let Some(s) = pagepool::span(SUPER, Hint::new(Class::Task, Tag::OTHER)) {
            core::mem::forget(s);
        }
    });
}

fn one_round() {
    setup();
    pagepool::set_claim_room(SUPER);
    let claimer = thread::spawn(|| pagepool::prefill(SUPER, 1));
    let ladder = thread::spawn(|| pagepool::set_claim_room(0));
    assert_eq!(claimer.join().unwrap(), 0, "a fill with no Cold superpage");
    ladder.join().unwrap();
    assert_eq!(pagepool::hooks::claim_room_left(), 0, "the ladder's room 0 was undone by the claimer's refund");
}

#[test]
fn a_refund_races_a_sample() {
    let iters = std::env::var("SHUTTLE_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(2000);
    shuttle::check_pct(one_round, iters, 3);
}
