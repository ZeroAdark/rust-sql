//! D4298 (T4, main GO 2026-10-13): holders that registered no reclaim (the shared catalog L2, the compiled-code cache) were
//! invisible to the memory ladder: R2a never counted or freed them, so their bytes stayed while the ladder climbed to R6.
//! Each now registers a reclaim only the ladder runs (`budget::register_ladder_reclaim`), taken after every cache of
//! `reclaim::ORDER` (`mempolicy::ladder::TAIL`: the L2 first, then the code cache), down to its bytes in use. Their hooks
//! keep `reclaim: None`, so the watchdog's backstop (and the broker) see them as before: the ladder off is unchanged.

use std::sync::atomic::{AtomicU64, Ordering};

use gravity_driver::mem::budget::{self, Consumer, Hooks};
use gravity_driver::mem::reclaim;

const MIB: u64 = 1 << 20;

static L2: AtomicU64 = AtomicU64::new(0);
static CODE: AtomicU64 = AtomicU64::new(0);

fn l2() -> u64 {
    L2.load(Ordering::Relaxed)
}
fn code() -> u64 {
    CODE.load(Ordering::Relaxed)
}
fn none_in_use() -> u64 {
    0
}
fn code_in_use() -> u64 {
    50 * MIB
}
fn trim(cell: &AtomicU64, to: u64, in_use: u64) -> u64 {
    let (cur, to) = (cell.load(Ordering::Relaxed), to.max(in_use));
    if to >= cur {
        return 0;
    }
    cell.store(to, Ordering::Relaxed);
    cur - to
}
fn l2_trim(to: u64) -> u64 {
    trim(&L2, to, none_in_use())
}
fn code_trim(to: u64) -> u64 {
    trim(&CODE, to, code_in_use())
}

#[test]
fn the_ladder_takes_the_catalog_l2_then_the_code_cache_after_the_caches_and_the_backstop_does_not() {
    L2.store(300 * MIB, Ordering::Relaxed);
    CODE.store(200 * MIB, Ordering::Relaxed);
    // as production registers them: fixed hooks with no reclaim, and the ladder-only reclaim beside
    assert!(budget::register(Consumer::CatalogL2, Hooks { current: l2, floor: l2, cap: l2, reclaim: None }));
    assert!(budget::register(Consumer::ProgramCaches, Hooks { current: code, floor: code, cap: code, reclaim: None }));
    budget::register_ladder_reclaim(Consumer::CatalogL2, none_in_use, l2_trim);
    budget::register_ladder_reclaim(Consumer::ProgramCaches, code_in_use, code_trim);
    budget::note_b(4096 * MIB);
    // the ladder off: the watchdog's backstop neither counts nor frees them (unchanged)
    assert_eq!(reclaim::backstop_bytes(), 0, "the backstop's count has no tail holder");
    assert_eq!(reclaim::reclaim_backstop(4096 * MIB), 0, "the backstop's reclaim frees no tail holder");
    assert_eq!((l2(), code()), (300 * MIB, 200 * MIB));
    // R2a: the L2 down to 0 in use, the code cache down to its 50 MiB in use; none of it protected (outside 0b)
    assert_eq!(reclaim::ladder_elastic(), 450 * MIB, "R2a counts the tail holders above their bytes in use");
    assert_eq!(reclaim::ladder_protected(), 0, "the tail holders keep no protected floor");
    assert_eq!(reclaim::ladder_reclaim(350 * MIB), 350 * MIB, "R2a frees the L2 first, then the code cache");
    assert_eq!((l2(), code()), (0, 150 * MIB));
    assert_eq!(reclaim::ladder_reclaim(4096 * MIB), 100 * MIB, "never under the code cache's bytes in use");
    assert_eq!((code(), reclaim::ladder_elastic()), (code_in_use(), 0));
}
