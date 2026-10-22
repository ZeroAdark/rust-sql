//! PRESSURE ONLY (mimalloc's purge delay -1): no collect on any thread purges a freed range (a forced one included), the
//! freed pages stay committed and count as reusable, a new allocation takes them (the reuse claim reads free AND
//! committed slices, purge-pending or not), and `purge_unused` (ladder pressure's purge) decommits every free committed
//! run at once, counted as a non-purger thread's bytes, and timed as one event of the calling thread's phase (here: a
//! session after its ReadyForQuery). Its own test binary: the purge delay is process-wide.

#[global_allocator]
static GLOBAL: init_small::oom::Rescue<mimalloc::MiMalloc> = init_small::oom::Rescue(mimalloc::MiMalloc);

// The arenas before mimalloc's constructor, as the server's, with the purge delay at -1 (v3 mi_option_purge_delay = 15).
extern "C" fn before_mimalloc() {
    let _ = allocarena::install();
    allocarena::set_purge_pressure_only();
}
#[used]
#[unsafe(link_section = ".init_array.00100")]
static BEFORE_MIMALLOC: extern "C" fn() = before_mimalloc;

/// Resident bytes of `[p, p + len)` (mincore over the pages the range touches).
fn resident(p: usize, len: usize) -> usize {
    let page = 4096usize;
    let lo = p & !(page - 1);
    let hi = (p + len).next_multiple_of(page);
    let mut v = vec![0u8; (hi - lo) / page];
    // SAFETY: mincore over a range of this process's arenas (mapped, read-write) into a vector of the right length.
    let rc = unsafe { libc::mincore(lo as *mut libc::c_void, hi - lo, v.as_mut_ptr()) };
    assert_eq!(rc, 0, "mincore");
    v.iter().filter(|b| **b & 1 != 0).count() * page
}

const SIZE: usize = 16 << 20;
const BIG: usize = 8 << 20;

#[test]
fn under_pressure_only_freed_pages_stay_until_the_pressure_purge() {
    assert!(allocarena::general_arenas() >= 1, "the general arenas are installed");
    let b = std::hint::black_box(vec![3u8; SIZE]);
    let p = b.as_ptr() as usize;
    assert!(allocarena::in_arenas(b.as_ptr()) && !allocarena::in_cache_arena(b.as_ptr()), "a general arena's block");
    drop(b);
    // a forced collect on another thread: with the delay at -1 it purges nothing
    std::thread::spawn(|| {
        let warm = std::hint::black_box(vec![1u8; 4096]);
        drop(warm);
        // SAFETY: mimalloc's collect on this thread.
        unsafe { libmimalloc_sys::mi_collect(true) };
    })
    .join()
    .unwrap();
    // SAFETY: as above, on this thread.
    unsafe { libmimalloc_sys::mi_collect(true) };
    assert!(resident(p, SIZE) >= SIZE * 9 / 10, "no purge at -1, forced collects included: {} resident", resident(p, SIZE));
    allocarena::set_reuse(true);
    assert!(allocarena::reusable_bytes() >= SIZE as u64 / 2, "the freed range is reusable: {}", allocarena::reusable_bytes());
    // the reuse claim takes it with nothing marked purge-pending
    let self0 = allocarena::alloc_sources().0;
    let g = std::hint::black_box(Vec::<u8>::with_capacity(BIG));
    let q = g.as_ptr() as usize;
    assert!(allocarena::alloc_sources().0 >= self0 + BIG as u64, "a self-claim of the committed free run: {:?}", allocarena::alloc_sources());
    assert!(resident(q, BIG) >= BIG * 9 / 10, "resident before its first write: {} of {BIG}", resident(q, BIG));
    drop(g);
    // SAFETY: as above.
    unsafe { libmimalloc_sys::mi_collect(false) };
    // ladder pressure's purge: every free committed run goes, counted as a non-purger thread's bytes
    let others0 = allocarena::purged_bytes().1;
    let t0 = allocarena::purge_times();
    allocarena::set_purge_phase(2);
    let n = allocarena::purge_unused(false);
    allocarena::set_purge_phase(0);
    let t1 = allocarena::purge_times();
    assert_eq!(t1[2][0], t0[2][0] + 1, "one event after ReadyForQuery: {t0:?} -> {t1:?}");
    assert!(t1[2][1] > t0[2][1] && t1[2][2] >= t1[2][1] - t0[2][1], "timed, the longest at least this one: {t1:?}");
    assert_eq!(t1[2][3], t0[2][3] + n, "its bytes: {t1:?}");
    assert_eq!((t1[0], t1[1], t1[3], t1[4]), (t0[0], t0[1], t0[3], t0[4]), "no other class moved");
    assert!(n >= SIZE as u64 / 2, "decommitted {n}");
    assert!(resident(p, SIZE) <= SIZE / 16, "the freed range went back: {} resident", resident(p, SIZE));
    assert!(allocarena::purged_bytes().1 >= others0 + SIZE as u64 / 2, "counted: {:?}", allocarena::purged_bytes());
    assert!(allocarena::reusable_bytes() < SIZE as u64 / 4, "nothing left to reuse: {}", allocarena::reusable_bytes());
    allocarena::set_reuse(false);
}
