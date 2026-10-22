//! Purge after idle's claim count (the purger's hold reads it, `PGRUST_PURGE_AFTER_IDLE_S` above 1, default off) is kept
//! only once the hold has ticked: with the hold off, an arena's page claims add nothing to it, so the allocation path makes
//! no add on a line every thread shares. Its own test binary: the count and its switch are process-wide.

#[global_allocator]
static GLOBAL: init_small::oom::Rescue<mimalloc::MiMalloc> = init_small::oom::Rescue(mimalloc::MiMalloc);

// The arenas before mimalloc's constructor, as the server's.
extern "C" fn before_mimalloc() {
    let _ = allocarena::install();
}
#[used]
#[unsafe(link_section = ".init_array.00100")]
static BEFORE_MIMALLOC: extern "C" fn() = before_mimalloc;

/// Eight 4 MiB blocks: fresh pages each, claimed from this thread's general arena.
fn blocks(fill: u8) -> Vec<Vec<u8>> {
    let v: Vec<Vec<u8>> = (0..8).map(|_| std::hint::black_box(vec![fill; 4 << 20])).collect();
    assert!(v.iter().all(|b| allocarena::in_arenas(b.as_ptr())), "the blocks are on the arenas");
    v
}

#[test]
fn the_claim_count_moves_only_once_the_hold_ticks() {
    assert!(allocarena::general_arenas() >= 1, "the general arenas are installed");
    let off = blocks(1);
    assert_eq!(allocarena::claims_noted(), 0, "the hold off: no claim counted");
    // the hold's tick (no arena is owned here, so it only starts the count)
    let _ = allocarena::purge_after_idle(true, true, 60_000);
    let on = blocks(2);
    assert!(allocarena::claims_noted() > 0, "the hold ticked: claims counted");
    drop((off, on));
}
