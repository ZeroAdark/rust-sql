//! A span resized to a size `take` serves as a bucket's block or runs moves (`resize` says None: the caller copies),
//! with `PGRUST_POOL_SPAN_FIT` on or off. Kept in place, the span would go back through `give_hinted`'s size-keyed shelf
//! as a bucket's or a run's block (mcx's pooled_bit test with SPAN_FIT off: a 40 MiB lazy span shrunk to 32 KiB in place,
//! then given as 32 KiB). A resize that stays a span still runs (SPAN_FIT off: in place, or the lazy span moved).
#![cfg(target_os = "linux")]

mod common;
use common::*;
use pagepool::hooks;
use pagepool::*;

const KIB: usize = 1 << 10;

#[test]
fn a_span_resized_to_a_small_size_moves() {
    hooks::fit_tail_switch(true);
    setup();
    let h = Hint::new(Class::Task, Tag::OTHER);
    for on in [false, true] {
        hooks::span_fit_switch(on);
        // a lazy span (past pool_lazy_min, 32 MiB here) and a pool span; to a bucket's size, to runs listed on the shelf
        // (8 x 64 KiB), to the most runs (31 x 64 KiB)
        for big in [40 * MIB, 3 * MIB + 5] {
            for small in [32 * KIB, 100 * KIB + 3, 512 * KIB, 31 * 64 * KIB] {
                let p = take(big, 64, h).expect("a span").cast::<u8>();
                // SAFETY: ours, `big` bytes at 64
                let r = unsafe { resize(p, big, small, 64) };
                assert!(r.is_none(), "SPAN_FIT {on}: a {big} B span resized to {small} B moves (a bucket's or runs' size)");
                // SAFETY: ours, freed once (still `big` bytes: the resize refused)
                unsafe { give(p, big) };
            }
        }
        if !on {
            // span to span: still served (the lazy span in place or moved, its result replaces it)
            let p = take(40 * MIB, 64, h).expect("a span").cast::<u8>();
            // SAFETY: ours, `40 MiB` at 64
            let q = unsafe { resize(p, 40 * MIB, 3 * MIB, 64) }.expect("a span shrunk to a span's size");
            // SAFETY: the resize's result, its new size
            unsafe { give(q.cast(), 3 * MIB) };
        }
    }
    flush_shelf();
    purge_now(usize::MAX, None);
    lazy_pass_now();
    balanced();
}
