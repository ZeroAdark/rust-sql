//! D4880, under `PoolAlloc` as this binary's global allocator: a big block the ready pool refuses for want of address space
//! (its lazy zone full of spans, as Gravity's regions fill it with 4 GiB reservations while little memory is claimed) comes
//! from the system, never null, zeroed when asked; it goes back to the system at its realloc and its free; `os_big` counts it.
#![cfg(all(feature = "global", target_os = "linux"))]

use core::alloc::{GlobalAlloc, Layout};
use pagepool::PoolAlloc;

#[global_allocator]
static A: PoolAlloc = PoolAlloc;

const MIB: usize = 1 << 20;
const RESERVE: usize = 4 << 30;
const HINT: pagepool::Hint = pagepool::Hint::new(pagepool::Class::Task, pagepool::Tag::REGION);

#[test]
fn a_big_block_the_full_lazy_zone_refuses_comes_from_the_system() {
    let warm = vec![1u8; 4096];
    assert!(pagepool::on() && pagepool::owns(warm.as_ptr()), "the pool serves this binary");
    // the lazy zone's address space taken by spans no one writes (no memory claimed for them)
    let mut spans = Vec::with_capacity(1 << 16);
    while let Some(s) = pagepool::span(RESERVE, HINT) {
        spans.push(s);
    }
    assert!(!spans.is_empty(), "the lazy zone held some 4 GiB spans before it filled");
    let big = spans.len();
    // its holes too, down to the lazy zone's least span (`pool_lazy_min`, 32 MiB)
    for size in [1 << 30, 256 * MIB, 64 * MIB, 32 * MIB] {
        while let Some(s) = pagepool::span(size, HINT) {
            spans.push(s);
        }
    }
    assert!(pagepool::span(64 * MIB, HINT).is_none(), "the lazy zone is full: the pool refuses a 64 MiB span");
    let (n0, b0) = pagepool::os_big();
    let l = Layout::from_size_align(64 * MIB, 64).unwrap();
    // SAFETY: a non-zero-size layout; every block below is freed with its layout.
    unsafe {
        let p = A.alloc(l);
        assert!(!p.is_null(), "a refused big block is the system's, not null");
        assert!(!pagepool::owns(p), "not the pool's");
        core::ptr::write_bytes(p, 0xa5, l.size());
        let q = A.realloc(p, l, 128 * MIB);
        assert!(!q.is_null() && !pagepool::owns(q), "its realloc stays the system's");
        assert_eq!((*q, *q.add(64 * MIB - 1)), (0xa5, 0xa5), "the realloc kept its bytes");
        A.dealloc(q, Layout::from_size_align(128 * MIB, 64).unwrap());
        let z = A.alloc_zeroed(l);
        assert!(!z.is_null() && !pagepool::owns(z), "a zeroed one too");
        assert!(core::slice::from_raw_parts(z, l.size()).iter().all(|&b| b == 0), "zeroed");
        A.dealloc(z, l);
    }
    let (n1, b1) = pagepool::os_big();
    // the two blocks the system served for the refused asks; the realloc of the system's block is the system's own
    // (System.realloc), no new ask of the pool
    assert_eq!((n1 - n0, b1 - b0), (2, 128 * MIB as u64), "os_big counted the alloc and the zeroed alloc");
    // D4885: some 4 GiB spans back, warm (a warm span keeps its address space for a take of its own size), the rest held:
    // a big block of another size takes a warm span's address space, the lazy zone otherwise full
    let rest = spans.split_off(big.min(8));
    for s in spans {
        // SAFETY: each span taken above, never written.
        unsafe { pagepool::give_span(s) };
    }
    let (d0, _) = pagepool::warm_va_drops();
    let v = vec![7u8; 64 * MIB];
    assert!(pagepool::owns(v.as_ptr()), "the pool's again once its lazy zone has room");
    let (d1, _) = pagepool::warm_va_drops();
    assert_eq!(d1 - d0, 1, "one warm 4 GiB span gave its address space");
    for s in rest {
        // SAFETY: each span taken above, never written.
        unsafe { pagepool::give_span(s) };
    }
}
