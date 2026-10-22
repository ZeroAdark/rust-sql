//! ppround (card 12, `PGRUST_POOL_FIT_TAIL`) under `PoolAlloc` as this binary's global allocator: a fitted span's size
//! for door 1 (`good_size`) is its ask to the page, the pool's HEAP in_use moves by exactly that, a realloc inside its
//! superpages stays in place (the bytes kept) and commits the new size, one past them moves with the bytes, and the frees
//! bring in_use back.
//! D4855: no libtest harness (`harness = false`, tests/one_thread): another thread's HEAP pages moved in_use inside the
//! window (one test's thread waiting on SER, libtest's own).

mod one_thread;

#[cfg(all(feature = "global", target_os = "linux"))]
mod t {
    use core::alloc::{GlobalAlloc, Layout};
    use pagepool::{PoolAlloc, Tag};

    #[global_allocator]
    static A: PoolAlloc = PoolAlloc;

    const MIB: usize = 1 << 20;

    /// The two tests flip the process-wide switch and read in_use: one at a time.
    static SER: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn heap() -> usize {
        pagepool::stats().in_use[Tag::HEAP.0 as usize]
    }

    pub fn a_fitted_span_charges_its_pages_and_grows_in_place() {
        let _g = SER.lock().unwrap_or_else(|e| e.into_inner());
        pagepool::hooks::span_fit_switch(true);
        pagepool::hooks::fit_tail_switch(true);
        let page = pagepool::hooks::page_size();
        let (n0, n1, n2) = (5 * MIB + 1, 5 * MIB + 400_000, 9 * MIB + 1);
        let l0 = Layout::from_size_align(n0, 8).unwrap();
        assert_eq!(pagepool::good_size(n0, 8), n0.next_multiple_of(page), "door 1's size: the ask to the page");
        let h0 = heap();
        // SAFETY: a valid layout; the block is ours until the frees below.
        let p = unsafe { A.alloc(l0) };
        assert!(!p.is_null() && pagepool::owns(p));
        assert_eq!(heap() - h0, n0.next_multiple_of(page), "in_use: the commit");
        // SAFETY: `n0` bytes of our block.
        unsafe { core::ptr::write_bytes(p, 0x5a, n0) };
        // SAFETY: our live block of layout l0.
        let q = unsafe { A.realloc(p, l0, n1) };
        assert_eq!(q, p, "in place inside its 3 superpages");
        assert_eq!(heap() - h0, n1.next_multiple_of(page), "in_use follows the grow");
        // SAFETY: the first n0 bytes are the ones written above.
        assert!(unsafe { core::slice::from_raw_parts(q, n0) }.iter().all(|&b| b == 0x5a));
        // SAFETY: `n1` bytes of our block now.
        unsafe { core::ptr::write_bytes(q.add(n0), 0x33, n1 - n0) };
        let l1 = Layout::from_size_align(n1, 8).unwrap();
        // SAFETY: our live block of layout l1.
        let r = unsafe { A.realloc(q, l1, n2) };
        assert!(!r.is_null() && r != q, "past its superpages: moved");
        // SAFETY: the first n1 bytes were copied from the old block.
        let s = unsafe { core::slice::from_raw_parts(r, n1) };
        assert!(s[..n0].iter().all(|&b| b == 0x5a) && s[n0..].iter().all(|&b| b == 0x33), "the bytes moved with it");
        assert_eq!(heap() - h0, n2.next_multiple_of(page), "the old span gone, the new one committed to the page");
        // SAFETY: our live block of `n2` bytes, freed once.
        unsafe { A.dealloc(r, Layout::from_size_align(n2, 8).unwrap()) };
        assert_eq!(heap(), h0, "freed: in_use back");
        let (on, misses, miss_bytes, unnamed) = pagepool::fit_tail();
        assert!(on && misses == 0 && miss_bytes == 0 && unnamed == 0, "every fitted ask got a base span");
    }

    /// The realloc guard (b6): one Vec grown by `reserve_exact` steps of ~700 KB from 2.1 to 24 MiB, then shrunk in four steps,
    /// with the switch off and on (each mode's blocks freed before the switch moves). With it on a grow inside the span's
    /// superpages stays in place through `resize` (a commit, no copy) and a shrink copies; off, both stay in place. ns per
    /// cycle is printed (MEASURED), never judged; the bytes are checked in both modes. D4855: the fitted spans are base spans
    /// (C.23), dirty once given and carved again only after the purger's `clean_base`, and this binary starts no purger
    /// (`pagepool::start` is the server's): every cycle's spans stayed resident (one quiet run 7.0 GB max RSS, 16 at once
    /// OOM-looped a 32 GiB box). After each cycle, outside its timing, `clean_base` here as the server's purger does at its
    /// wake; the bytes it gave back per cycle are printed too (MEASURED).
    pub fn realloc_guard() {
        let _g = SER.lock().unwrap_or_else(|e| e.into_inner());
        pagepool::hooks::span_fit_switch(true);
        let cycles = if std::env::var("PAGEPOOL_BIG_TESTS").is_ok_and(|v| v == "1") { 200 } else { 10 };
        let (mut ns, mut purged) = ([0f64; 2], [0usize; 2]);
        for (i, on) in [false, true, false, true].into_iter().enumerate() {
            pagepool::hooks::fit_tail_switch(on);
            let mut t = 0u128;
            for c in 0..cycles {
                let t0 = std::time::Instant::now();
                {
                    let mut v: Vec<u8> = Vec::with_capacity(2 * MIB + 100_000);
                    v.resize(2 * MIB + 100_000, c as u8);
                    while v.len() < 24 * MIB {
                        let add = 700_000.min(24 * MIB - v.len());
                        v.reserve_exact(add);
                        let len = v.len();
                        v.resize(len + add, c as u8);
                    }
                    for keep in [20 * MIB + 3, 14 * MIB + 5, 9 * MIB + 7, 3 * MIB + 11] {
                        v.truncate(keep);
                        v.shrink_to_fit();
                        assert!(v.iter().all(|&b| b == c as u8), "the bytes kept (on {on})");
                    }
                }
                t += t0.elapsed().as_nanos();
                let got = pagepool::hooks::clean_base();
                if i >= 2 {
                    purged[on as usize] += got;
                }
            }
            if i >= 2 {
                ns[on as usize] = t as f64 / cycles as f64;
            }
        }
        let per = |b: usize| b as f64 / cycles as f64 / MIB as f64;
        println!(
            "MEASURED realloc_guard ns/cycle: FIT_TAIL off {:.0}, on {:.0} ({:.3}x); given back per cycle: off {:.0} MiB, on {:.0} MiB",
            ns[0],
            ns[1],
            ns[1] / ns[0].max(1.0),
            per(purged[0]),
            per(purged[1])
        );
    }
}

fn main() {
    #[cfg(all(feature = "global", target_os = "linux"))]
    let tests: &[(&str, fn())] = &[
        ("a_fitted_span_charges_its_pages_and_grows_in_place", t::a_fitted_span_charges_its_pages_and_grows_in_place),
        ("realloc_guard", t::realloc_guard),
    ];
    #[cfg(not(all(feature = "global", target_os = "linux")))]
    let tests: &[(&str, fn())] = &[];
    one_thread::main(tests);
}
