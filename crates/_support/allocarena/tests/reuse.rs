//! REUSE BEFORE RELEASE (memacct2 2026-10-16, Broker's shape (A)): with the switch on, a general heap that needs fresh
//! slices first takes the cache arena's committed free slices: a freed cache image's pages, still resident, serve the
//! next large general allocation (no fresh faults), the census counts them as general bytes while held, and their free
//! puts them back in the cache arena; a general arena's own committed free run comes before the donor's; the cache heap's
//! own new page takes the cache arena's committed free run before fresh slices. Its own test binary: the donor and the
//! purge delay are process-wide.

#[global_allocator]
static GLOBAL: init_small::oom::Rescue<mimalloc::MiMalloc> = init_small::oom::Rescue(mimalloc::MiMalloc);

use std::sync::OnceLock;

// The arenas before mimalloc's constructor, as the server's; a 60 s purge delay (v3 mi_option_purge_delay = 15), so a freed
// image's slices stay committed and pending for the whole test.
extern "C" fn before_mimalloc() {
    // SAFETY: a plain option store, before any thread exists.
    unsafe { libmimalloc_sys::mi_option_set(15, 60_000) };
    let _ = allocarena::install();
}
#[used]
#[unsafe(link_section = ".init_array.00100")]
static BEFORE_MIMALLOC: extern "C" fn() = before_mimalloc;

fn setup() {
    static DONE: OnceLock<bool> = OnceLock::new();
    DONE.get_or_init(|| {
        init_small::oom::set_cache_alloc(allocarena::cache_alloc, allocarena::cache_realloc);
        init_small::oom::set_own_pages(allocarena::own::range(), allocarena::own_free, allocarena::own_resize);
        allocarena::start_cache_heap()
    });
}

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

const IMAGE: usize = 24 << 20;
const BLOCK: usize = 1 << 20;
/// Past one bitmap field (64 slices = 4 MiB): a claim whose run crosses fields (wide scans' >4 MiB claims carry most of their bytes).
const BIG: usize = 8 << 20;

/// A cache image of IMAGE written and freed, its pages back in the cache arena (this thread's collect, no purge).
fn freed_cache_image() {
    let img = {
        let _scope = memledger::arena::cache_scope_for(memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache)), memledger::Kind::Region);
        vec![3u8; IMAGE]
    };
    assert!(allocarena::in_cache_arena(img.as_ptr()), "the image is on the cache arena");
    drop(img);
    allocarena::collect_cache_local();
}

#[test]
fn a_general_allocation_reuses_a_freed_images_committed_slices_before_fresh_ones() {
    setup();
    // off: a large general block comes from a general arena
    let off = std::hint::black_box(vec![0u8; BLOCK]);
    assert!(!allocarena::in_cache_arena(off.as_ptr()) && allocarena::in_arenas(off.as_ptr()), "off: a general arena's block");
    // kept to the end: freed, its general run would serve the next block before the donor (a general arena's own committed
    // free runs come first)
    allocarena::set_reuse(true);
    freed_cache_image();
    // the supply: the freed image is one committed free run of at least 256 slices (16 MiB) in the histogram
    let (runs_c, _) = allocarena::cache_free_runs();
    assert!(runs_c[8..].iter().sum::<u64>() >= IMAGE as u64, "the freed image's committed run: {runs_c:?}");
    let (live0, claims0, _, _) = allocarena::reuse_stats();
    assert_eq!(live0, 0);
    let (g0, c0) = memledger::arena::taken().expect("arenas installed");
    // on: the next large general block is served from the image's committed free slices, resident before its first write
    let mut b = std::hint::black_box(Vec::<u8>::with_capacity(BLOCK));
    let p = b.as_ptr() as usize;
    assert!(allocarena::in_cache_arena(b.as_ptr()), "on: the block reuses the cache arena's committed free slices");
    assert!(resident(p, BLOCK) >= BLOCK * 9 / 10, "its pages are resident before any write: {} of {BLOCK}", resident(p, BLOCK));
    b.resize(BLOCK, 7);
    let (live1, claims1, claimed1, _) = allocarena::reuse_stats();
    assert!(claims1 > claims0 && live1 >= BLOCK as u64 && claimed1 >= BLOCK as u64, "counted: live {live1}, claims {claims1}");
    // the census: the held slices count as general bytes, not cache
    let (g1, c1) = memledger::arena::taken().unwrap();
    assert!(g1 >= g0 + live1 && c1 + live1 <= c0 + (1 << 20), "taken moved {live1} from cache to general: ({g0}, {c0}) -> ({g1}, {c1})");
    drop(b);
    // SAFETY: mimalloc's collect on this thread (the block's page back to its arena, no forced purge).
    unsafe { libmimalloc_sys::mi_collect(false) };
    assert_eq!(allocarena::reuse_stats().0, 0, "freed: the slices are the cache arena's again");
    // past one field: an 8 MiB block (a singleton page of 129 slices) takes a run that crosses bitmap fields
    let mut big = std::hint::black_box(Vec::<u8>::with_capacity(BIG));
    let q = big.as_ptr() as usize;
    assert!(allocarena::in_cache_arena(big.as_ptr()), "on: the 8 MiB block reuses a committed free run across fields");
    assert!(resident(q, BIG) >= BIG * 9 / 10, "resident before any write: {} of {BIG}", resident(q, BIG));
    big.resize(BIG, 9);
    let held = allocarena::reuse_stats().0;
    assert!(held >= BIG as u64, "counted: {:?}", allocarena::reuse_stats());
    drop(big);
    // SAFETY: as above.
    unsafe { libmimalloc_sys::mi_collect(false) };
    // its slices are the cache arena's again (a small page this thread took meanwhile, e.g. for resident()'s vector, may
    // itself be a reused slice and stay held: 64 KiB seen)
    let left = allocarena::reuse_stats().0;
    assert!(left + BIG as u64 <= held, "freed again: held {held}, left {left}");
    // a general arena's own committed free run comes first: a freed 8 MiB general block (taken with reuse off) serves the
    // next 8 MiB block, counted, resident before its first write; and the reusable bytes count it
    allocarena::set_reuse(false);
    let fresh0 = allocarena::alloc_sources().2;
    let g1 = std::hint::black_box(vec![5u8; BIG]);
    assert!(!allocarena::in_cache_arena(g1.as_ptr()) && allocarena::in_arenas(g1.as_ptr()), "reuse off: a general arena's block");
    assert!(allocarena::alloc_sources().2 >= fresh0 + BIG as u64 / 2, "its slices committed afresh are counted: {:?}", allocarena::alloc_sources());
    drop(g1);
    // SAFETY: as above.
    unsafe { libmimalloc_sys::mi_collect(false) };
    allocarena::set_reuse(true);
    assert!(allocarena::reusable_bytes() >= BIG as u64, "the freed general run is reusable: {}", allocarena::reusable_bytes());
    let self0 = allocarena::alloc_sources().0;
    let g2 = std::hint::black_box(Vec::<u8>::with_capacity(BIG));
    let r = g2.as_ptr() as usize;
    assert!(!allocarena::in_cache_arena(g2.as_ptr()) && allocarena::in_arenas(g2.as_ptr()), "the general arena's own run, not the donor's");
    assert!(allocarena::alloc_sources().0 >= self0 + BIG as u64, "counted as a general self-claim: {:?}", allocarena::alloc_sources());
    assert!(resident(r, BIG) >= BIG * 9 / 10, "resident before any write: {} of {BIG}", resident(r, BIG));
    drop(g2);
    // the cache heap's own new page (a part reload) takes the cache arena's committed free run before fresh slices,
    // counted apart from the general heaps' claims, resident before its first write
    freed_cache_image();
    let cs0 = allocarena::alloc_sources().4;
    let reload = {
        let _scope = memledger::arena::cache_scope_for(memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache)), memledger::Kind::Region);
        std::hint::black_box(Vec::<u8>::with_capacity(BIG))
    };
    let c = reload.as_ptr() as usize;
    assert!(allocarena::in_cache_arena(reload.as_ptr()), "a cache image, on the cache arena");
    assert!(allocarena::alloc_sources().4 >= cs0 + BIG as u64, "counted as the cache heap's own claim: {:?}", allocarena::alloc_sources());
    assert!(resident(c, BIG) >= BIG * 9 / 10, "resident before any write: {} of {BIG}", resident(c, BIG));
    drop(reload);
    allocarena::set_reuse(false);
    drop(off);
}
