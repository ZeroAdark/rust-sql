//! D4872: the host's memory pressure (`set_host_pressed`: memruntime's sample of the memory PSI / file refaults past
//! `pool_host_*`) gives the pool's free memory back with no ladder pressure at all, and the claimer prefaults nothing
//! while pressed; with `PGRUST_POOL_HOST_PRESSURE` off, a press does nothing. Under the ladder's line the pool otherwise
//! keeps what it freed while the kernel evicts the page cache (a large bulk load on a 32 GiB x86 box, 19 % slower than
//! under mimalloc). Born red: before D4872 nothing released the warm free spans here (no `set_pressure`).

mod common;
use common::*;
use pagepool::*;
use std::time::{Duration, Instant};

#[test]
fn host_pressure_releases_free_memory_and_stops_the_prefault() {
    setup();
    assert!(start(), "the pool's threads started");
    let size = 128 * MIB;
    let h = Hint::new(Class::Process, Tag::BUFFER);
    let spans: Vec<_> = (0..size / (16 * MIB)).map(|_| span(16 * MIB, h).expect("span")).collect();
    for s in &spans {
        touch(s.as_ptr() as usize, s.len());
    }
    for s in spans {
        // SAFETY: ours, not used again.
        unsafe { give_span(s) };
    }
    let r0 = stats().released_bytes;
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(stats().released_bytes, r0, "nothing released without pressure (pressure-only release, no ladder)");
    assert!(releasable() >= size / 10 * 9, "the given spans are free and releasable: {} of {size}", releasable());

    // the switch off: a press does nothing
    hooks::host_pressure_switch(false);
    set_host_pressed(true);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(stats().released_bytes, r0, "PGRUST_POOL_HOST_PRESSURE=0: the press asks nothing");
    assert_eq!(host_counts(), HostCounts { on: false, pressed: false, presses: 0, asks: 0, bytes: 0 });

    // on (the default): the press asks every releasable byte of the purger, which gives it back
    hooks::host_pressure_switch(true);
    set_host_pressed(true);
    let t0 = Instant::now();
    while stats().released_bytes - r0 < (size / 10 * 9) as u64 && t0.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(1));
    }
    let got = stats().released_bytes - r0;
    eprintln!("MEASURED host press: {} MiB released in {:.1} ms", got >> 20, t0.elapsed().as_secs_f64() * 1e3);
    assert!(got >= (size / 10 * 9) as u64, "the host press released {got} of {size}");
    let c = host_counts();
    assert!(c.on && c.pressed, "pressed: {c:?}");
    assert_eq!(c.presses, 1, "one press: {c:?}");
    assert!(c.asks >= 1, "asked: {c:?}");
    // the purger counts the host's bytes once its pass is over (served after the release counter moved)
    let t1 = Instant::now();
    while host_counts().bytes < (size / 10 * 9) as u64 && t1.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(host_counts().bytes >= (size / 10 * 9) as u64, "the host's asks' bytes: {:?}", host_counts());

    // while pressed the claimer prefaults nothing (its room is closed)
    assert_eq!(prefill(8 * MIB, 1), 0, "no prefault while the host is pressed");

    // a second pressed sample is the same press; a sample under the knobs lets go and the claimer fills again
    set_host_pressed(true);
    assert_eq!(host_counts().presses, 1, "still the one press");
    set_host_pressed(false);
    assert!(!host_counts().pressed, "let go");
    assert_eq!(prefill(8 * MIB, 1), 8 * MIB, "the claimer prefaults again once the host lets go");
    set_host_pressed(true);
    assert_eq!(host_counts().presses, 2, "a new press after a let-go");
    set_host_pressed(false);
}
