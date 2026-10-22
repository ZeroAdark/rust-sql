//! `global_live()` against the good sizes handed out. A binary of its own: the counter is process-wide, and in a
//! shared binary the test harness's threads (spawning, capturing, exiting) move it during the count.
//! D4855: no libtest harness (`harness = false`, tests/one_thread): libtest's own threads allocated inside the count.

mod one_thread;

#[cfg(all(feature = "global", not(target_family = "wasm")))]
mod t {
    use core::alloc::{GlobalAlloc, Layout};
    use pagepool::PoolAlloc;

    #[global_allocator]
    static A: PoolAlloc = PoolAlloc;

    fn rng(x: &mut u64) -> u64 {
        *x ^= *x << 13;
        *x ^= *x >> 7;
        *x ^= *x << 17;
        *x
    }

    pub fn global_live_is_the_good_size_handed_out() {
        let mut blocks: Vec<(usize, Layout)> = Vec::with_capacity(3000);
        let g0 = pagepool::global_live();
        let mut x = 5u64;
        let mut want = 0usize;
        for i in 0..3000 {
            let size = if i % 10 == 0 { 1025 + (rng(&mut x) % 300_000) as usize } else { 1 + (rng(&mut x) % 1024) as usize };
            let l = Layout::from_size_align(size, [1, 8, 16, 64][i % 4]).unwrap();
            want += pagepool::good_size(l.size(), l.align());
            // SAFETY: a non-zero layout.
            blocks.push((unsafe { A.alloc(l) } as usize, l));
        }
        assert_eq!(pagepool::global_live() as isize - g0 as isize, want as isize);
        for &(p, l) in &blocks {
            // SAFETY: ours, freed once.
            unsafe { A.dealloc(p as *mut u8, l) };
        }
        assert_eq!(pagepool::global_live(), g0);
    }
}

fn main() {
    #[cfg(all(feature = "global", not(target_family = "wasm")))]
    let tests: &[(&str, fn())] = &[
        ("global_live_is_the_good_size_handed_out", t::global_live_is_the_good_size_handed_out),
    ];
    #[cfg(not(all(feature = "global", not(target_family = "wasm"))))]
    let tests: &[(&str, fn())] = &[];
    one_thread::main(tests);
}
