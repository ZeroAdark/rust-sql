//! `order`'s unsafe steps (rule R6, the module's raw home): the pool's thread hooks, kept as words in `Wake` and called
//! back as the functions `arm` / `arm_park` stored.
#![allow(unsafe_code)]

use core::sync::atomic::Ordering;

use super::Wake;

impl Wake {
    pub(super) fn unpark_word(&self, w: u64) {
        let f = self.unpark.load(Ordering::Acquire);
        if w != 0 && f != 0 {
            // SAFETY: `f` was stored from a `fn(u64)` by `arm`.
            unsafe { core::mem::transmute::<usize, fn(u64)>(f)(w) };
        }
    }

    pub(super) fn current(&self) -> Option<u64> {
        let (c, p) = (self.current.load(Ordering::Acquire), self.park_ms.load(Ordering::Acquire));
        // SAFETY: `c` was stored from a `fn() -> u64` by `arm_park`.
        (c != 0 && p != 0).then(|| unsafe { core::mem::transmute::<usize, fn() -> u64>(c)() }).filter(|&w| w != 0)
    }

    pub(super) fn park(&self, ms: u64) {
        let p = self.park_ms.load(Ordering::Acquire);
        if p != 0 {
            // SAFETY: `p` was stored from a `fn(u64)` by `arm_park`.
            unsafe { core::mem::transmute::<usize, fn(u64)>(p)(ms) };
        }
    }
}
