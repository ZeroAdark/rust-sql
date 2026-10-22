//! Shuttle model (suspect C): R1's ask (`return_warm`: LANDING, the R1 flag, ASK) races the purger's pass
//! (`serve_ask`). Whatever the order, the ask is served by R1's own step (`return_warm_now`, counted in
//! `r1_counts().calls`), and LANDING stays set until a pass served it. Real crate; every atomic of the shared state is
//! a scheduling point (src/sync.rs). Runs only with RUSTFLAGS="--cfg shuttle".
#![cfg(shuttle)]

use pagepool::{Class, Config, Hint, Tag};
use shuttle::thread;

const SUPER: usize = 2 << 20;

fn setup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let cfg = Config {
            va: 8 << 20,
            chunk: 8 << 20,
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
    });
}

fn one_round() {
    setup();
    // one WarmDirty superpage to give back
    let s = pagepool::span(SUPER, Hint::new(Class::Task, Tag::OTHER)).expect("span");
    // SAFETY: just taken, not used.
    unsafe { pagepool::give_span(s) };
    let before = pagepool::r1_counts().calls;
    let ladder = thread::spawn(pagepool::return_warm);
    let purger = thread::spawn(pagepool::serve_ask_now);
    let asked = ladder.join().unwrap();
    purger.join().unwrap();
    // the purger's next passes (it was woken)
    pagepool::serve_ask_now();
    pagepool::serve_ask_now();
    if asked != 0 {
        assert!(pagepool::r1_counts().calls > before, "R1's ask was served by a plain purge, not return_warm_now");
    }
    assert_eq!(pagepool::landing(u32::MAX), 0, "LANDING left set with no ask pending");
    if let Err(e) = pagepool::hooks::check() {
        panic!("{e}: {:?}", pagepool::stats());
    }
}

#[test]
fn r1_ask_races_the_purgers_pass() {
    let iters = std::env::var("SHUTTLE_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(2000);
    shuttle::check_pct(one_round, iters, 3);
}
