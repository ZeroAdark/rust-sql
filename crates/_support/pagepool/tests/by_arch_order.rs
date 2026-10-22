//! The server's per-architecture default (`by_arch_default!`, the user 2026-10-18) takes effect only when its constructor
//! runs before the process's first allocation. The server has another constructor that allocates (`tune_allocator`,
//! `.init_array.00100`: its env reads), so this binary has one in the same shape and checks that `by_arch_default!` ran
//! before it, on any architecture: a constructor moved or added ahead of it fails here on Graviton too, not only on x86
//! (where the default would silently stay Hearth: x86 v2 of the change, 2026-10-18).
#![cfg(all(feature = "global", target_os = "linux", not(miri), not(loom)))]

use core::sync::atomic::{AtomicBool, Ordering::Relaxed};
use pagepool::PoolAlloc;

#[global_allocator]
static A: PoolAlloc = PoolAlloc;
pagepool::by_arch_default!();

/// The allocating constructor ran, and whether `by_arch_default!` had run when it did.
static ALLOC_CTOR_RAN: AtomicBool = AtomicBool::new(false);
static BY_ARCH_BEFORE_ALLOC_CTOR: AtomicBool = AtomicBool::new(false);

extern "C" fn allocating_ctor() {
    BY_ARCH_BEFORE_ALLOC_CTOR.store(pagepool::by_arch(), Relaxed);
    // tune_allocator's shape: an env read, which allocates (the process's first allocation)
    let v = std::env::var_os("PATH").map(|p| p.len()).unwrap_or(0);
    let b = std::hint::black_box(vec![0u8; 64 + v % 7]);
    drop(b);
    ALLOC_CTOR_RAN.store(true, Relaxed);
}

#[used]
#[unsafe(link_section = ".init_array.00100")]
static ALLOCATING_CTOR: extern "C" fn() = allocating_ctor;

#[test]
fn by_arch_default_runs_before_an_allocating_constructor() {
    assert!(ALLOC_CTOR_RAN.load(Relaxed), "the .init_array.00100 constructor ran");
    assert!(BY_ARCH_BEFORE_ALLOC_CTOR.load(Relaxed), "by_arch_default! had run when the .init_array.00100 constructor allocated");
    assert!(pagepool::by_arch() && !pagepool::by_arch_late(), "by_arch_default!'s constructor ran before the first allocation");
    assert_eq!(pagepool::allocator_decides(), 1, "the choice was read once, at that first allocation");
    let want = std::env::var_os("PGRUST_ALLOCATOR").map_or(pagepool::HEARTH_BY_ARCH, |v| pagepool::hearth_chosen(v.to_str(), pagepool::HEARTH_BY_ARCH));
    assert_eq!(pagepool::pool_allocates(), want, "the per-architecture default (or PGRUST_ALLOCATOR) chose the allocator");
}
