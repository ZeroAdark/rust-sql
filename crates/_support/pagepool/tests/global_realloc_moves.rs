//! `PoolAlloc` as this binary's global allocator: a realloc whose lazy span cannot grow where it is moves it by mremap,
//! and the caller gets the new address (D4722: the old one, guarded after the move, faulted at the next grow).
#![cfg(all(feature = "global", target_os = "linux"))]

use pagepool::PoolAlloc;

#[global_allocator]
static A: PoolAlloc = PoolAlloc;

const MIB: usize = 1 << 20;

#[test]
fn a_grown_vec_whose_lazy_span_moved_reads_where_it_moved() {
    let n = 48 * MIB / 8;
    let mut v: Vec<u64> = Vec::with_capacity(n);
    v.extend(0..n as u64);
    let next = vec![0xa5u8; 48 * MIB];
    let (old, lazy) = (v.as_ptr() as usize, pagepool::owns(v.as_ptr().cast()));
    assert!(lazy && pagepool::owns(next.as_ptr()), "both lazy spans of the pool's");
    v.push(n as u64);
    let moved = v.as_ptr() as usize != old;
    eprintln!("moved {moved}: {old:#x} -> {:#x}", v.as_ptr() as usize);
    assert!(v.iter().enumerate().all(|(i, &x)| x == i as u64), "the grown vec reads what was written");
    v.extend(n as u64 + 1..2 * n as u64);
    assert!(v.iter().enumerate().all(|(i, &x)| x == i as u64), "and what is written past the old end");
    assert!(next.iter().all(|&b| b == 0xa5), "the neighbour's bytes are its own");
    v.reserve(4 * n);
    assert!(v.iter().enumerate().all(|(i, &x)| x == i as u64), "a second grow reads the moved block");
}
