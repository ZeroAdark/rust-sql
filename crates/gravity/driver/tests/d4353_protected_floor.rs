//! D4353 (C), LADDER-DESIGN section 0b (user ruling: a fixed size for now): the caches keep a PROTECTED floor, 25 % of B
//! split by their caps. The ladder's R2a (elastic bytes) takes a cache down to its protected floor only; R2b, past the
//! wait and just before a cancel, takes the rest down to the bytes in use. Without the floors R2a took everything and R2b
//! had nothing to evict: it could never fire.

use std::sync::atomic::{AtomicU64, Ordering};

use gravity_driver::mem::budget::{self, Consumer, Hooks};
use gravity_driver::mem::reclaim;

const MIB: u64 = 1 << 20;

static HELD: AtomicU64 = AtomicU64::new(0);

fn held() -> u64 {
    HELD.load(Ordering::Relaxed)
}

fn in_use() -> u64 {
    64 * MIB
}

fn cap() -> u64 {
    4096 * MIB
}

fn trim_to(to: u64) -> u64 {
    let (cur, to) = (held(), to.max(in_use()));
    if to >= cur {
        return 0;
    }
    HELD.store(to, Ordering::Relaxed);
    cur - to
}

#[test]
fn r2a_stops_at_the_protected_floor_and_r2b_takes_the_rest() {
    HELD.store(2000 * MIB, Ordering::Relaxed);
    assert!(budget::register(Consumer::CondCache, Hooks { current: held, floor: in_use, cap, reclaim: Some(trim_to) }));
    // B 4 GiB: 1 GiB protected, all of it this cache's (the only one with a cap)
    budget::note_b(4096 * MIB);
    assert_eq!(reclaim::ladder_elastic(), 976 * MIB, "R2a counts the bytes above the protected 1024 MiB");
    assert_eq!(reclaim::ladder_reclaim(4096 * MIB), 976 * MIB, "R2a frees down to the protected floor, no further");
    assert_eq!(held(), 1024 * MIB);
    assert_eq!((reclaim::ladder_elastic(), reclaim::ladder_protected()), (0, 960 * MIB), "R2b has the protected bytes not in use");
    assert_eq!(reclaim::reclaim_backstop(4096 * MIB), 960 * MIB, "R2b (the backstop's reclaim) takes them down to the bytes in use");
    assert_eq!((held(), reclaim::ladder_protected()), (in_use(), 0));
}
