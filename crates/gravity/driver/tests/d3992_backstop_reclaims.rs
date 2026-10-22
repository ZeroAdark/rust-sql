//! D3992 (memtest tr3 step 3, the default config: broker advise): the backstop canceled 80 statements while ~10 GB of caches
//! stood, since nothing reclaimed outside the broker's `on`: the fast rungs reach managed caches only. The backstop's reclaim
//! takes every cache with a reclaim down to its floor, whatever the broker's mode, before it cancels.

use std::sync::atomic::{AtomicU64, Ordering};

use gravity_driver::mem::budget::{self, Consumer, Hooks};
use gravity_driver::mem::reclaim;

const MIB: u64 = 1 << 20;

static HELD: AtomicU64 = AtomicU64::new(0);

fn held() -> u64 {
    HELD.load(Ordering::Relaxed)
}

fn floor() -> u64 {
    64 * MIB
}

fn cap() -> u64 {
    1024 * MIB
}

fn trim_to(to: u64) -> u64 {
    let (cur, to) = (held(), to.max(floor()));
    if to >= cur {
        return 0;
    }
    HELD.store(to, Ordering::Relaxed);
    cur - to
}

#[test]
fn the_backstop_reclaims_an_unmanaged_cache_down_to_its_floor() {
    HELD.store(1000 * MIB, Ordering::Relaxed);
    assert!(budget::register(Consumer::CondCache, Hooks { current: held, floor, cap, reclaim: Some(trim_to) }));
    assert!(!reclaim::applies() && !budget::managed(Consumer::CondCache), "the default: no reclaimer, no managed cache");
    assert_eq!(reclaim::reclaim_fast(4096 * MIB), 0, "the fast rungs never reach it");
    assert_eq!(reclaim::backstop_bytes(), 936 * MIB, "what the backstop could free: down to the floor");
    assert_eq!(reclaim::reclaim_backstop(300 * MIB), 300 * MIB, "what it wants and no more");
    assert_eq!(held(), 700 * MIB);
    assert_eq!(reclaim::reclaim_backstop(4096 * MIB), 636 * MIB, "never under the floor");
    assert_eq!((held(), reclaim::backstop_bytes()), (floor(), 0));
}
