//! Shuttle model (suspect A): the purger's shed (`reserve_ask` + a pass: warmres::release) races a frames commit that
//! moves the one WarmDirty superpage's pages away and leaves it Cold. Whatever the order, the pool's claim stays its
//! parts (`hooks::check`): a superpage the shed finds Cold was never claimed and is no release. Real crate, real
//! syscalls; every atomic of the shared state is a scheduling point (src/sync.rs). Runs only with
//! RUSTFLAGS="--cfg shuttle".
#![cfg(shuttle)]

use pagepool::frames::{self, Mode};
use pagepool::{Class, Config, Hint, Tag};
use shuttle::thread;

const SUPER: usize = 2 << 20;

fn setup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        pagepool::hooks::reserve_switch(true);
        let cfg = Config {
            va: 8 << 20,
            chunk: 8 << 20,
            lazy_va: 0,
            frame_va: 16 << 20,
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
        pagepool::set_reserve_budget(64 << 20);
        // one WarmDirty superpage: a span taken (Cold, claimed) and given back
        let s = pagepool::span(SUPER, Hint::new(Class::Task, Tag::OTHER)).expect("span");
        // SAFETY: just taken, not used.
        unsafe { pagepool::give_span(s) };
    });
}

fn balanced(at: &str) {
    if let Err(e) = pagepool::hooks::check() {
        panic!("{at}: {e}: {:?}", pagepool::stats());
    }
}

fn one_round() {
    setup();
    balanced("start");
    let purger = thread::spawn(|| {
        pagepool::reserve_ask(SUPER);
        pagepool::serve_ask_now();
    });
    let mover = thread::spawn(|| {
        let z = frames::zone().expect("frame zone");
        // SAFETY: the frame zone's first piece, PROT_NONE, this test's alone.
        let c = unsafe { frames::commit(z.lo(), SUPER, true, Mode::Move, Tag::OTHER) };
        c.is_some()
    });
    purger.join().unwrap();
    let committed = mover.join().unwrap();
    balanced("after the shed and the move");
    if committed {
        let z = frames::zone().unwrap();
        // SAFETY: committed above, nothing reads it after; its piece goes back warm (WarmDirty) for the next round.
        unsafe { frames::release(z.lo(), SUPER, true) };
    }
    // the next round starts with one WarmDirty superpage again
    balanced("end");
}

#[test]
fn shed_races_a_move() {
    let iters = std::env::var("SHUTTLE_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(2000);
    shuttle::check_pct(one_round, iters, 3);
}
