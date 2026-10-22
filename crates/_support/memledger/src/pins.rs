//! PINS: what a scan holds of a cache's entries, counted per owner beside `held`.
//!
//! A cache entry that a running scan references stays resident after the cache evicts it, on the cache's account, in no
//! statement's `held`: a fair share over `held` finds nobody over its level while the memory is not the cache's to give
//! back (the 8 GiB stress: 2.6-3.3 GiB of fp tables evicted but pinned). Each holder keeps a [`PinGuard`] beside its
//! reference; the guard books the entry's bytes on the owner (the portal or statement the scan was opened under, not the
//! protocol message running when it is taken or dropped) as `pins`, and, once the cache has evicted the entry
//! ([`PinSet::evict`]), as `pins_evicted` and `pins_share` (the entry's bytes over its holders, so a sum over owners
//! counts the entry once). These are counts, not charges: the bytes stay on the cache's account until the last
//! reference drops. A guard follows its owner's redirect when the owner ends, as a `Charge` does.

use std::sync::Arc;

use pgsync::Mutex;

use crate::charge::{context, Charge};
use crate::Owner;

/// One holder's booking.
struct Slot {
    c: Charge,
    pins: i64,
    ev: i64,
    sh: i64,
}

impl Slot {
    fn set(&mut self, pins: i64, ev: i64, sh: i64) {
        let d = (pins - self.pins, ev - self.ev, sh - self.sh);
        if d != (0, 0, 0) {
            self.c.pin_book(d);
            (self.pins, self.ev, self.sh) = (pins, ev, sh);
        }
    }
}

/// Holders kept inline: a pin allocates nothing (the scan path takes one per table per scan). A holder past these goes to
/// `more`, which allocates (rare: this many concurrent scans of one entry).
const INLINE: usize = 8;

struct Held {
    id: u64,
    slot: Slot,
}

struct Inner {
    evicted: bool,
    next: u64,
    inline: [Option<Held>; INLINE],
    more: Vec<Held>, // alloc: Cold
}

impl Inner {
    fn each(&mut self, mut f: impl FnMut(&mut Held)) -> usize {
        let mut n = 0;
        for h in self.inline.iter_mut().flatten().chain(self.more.iter_mut()) {
            f(h);
            n += 1;
        }
        n
    }

    fn count(&self) -> usize {
        self.inline.iter().flatten().count() + self.more.len()
    }
}

/// An entry's holders: `bytes` of the entry, who references it, and whether the cache evicted it.
pub struct PinSet {
    bytes: i64,
    inner: Mutex<Inner>,
}

/// A holder's pin on an entry; dropping it releases the holder's counts.
pub struct PinGuard {
    set: Arc<PinSet>,
    id: u64,
}

fn lock<T>(m: &Mutex<T>) -> pgsync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl PinSet {
    pub fn new(bytes: u64) -> Arc<PinSet> {
        Arc::new(PinSet { bytes: bytes.min(i64::MAX as u64) as i64, inner: Mutex::new(Inner { evicted: false, next: 0, inline: [const { None }; INLINE], more: Vec::new() }) }) // alloc: Cold
    }

    /// The entry's holders re-shared: each evicted entry's bytes over its holders (the remainder to the first), so the
    /// shares sum to the entry's bytes.
    fn reshare(&self, g: &mut Inner) {
        let n = g.count() as i64;
        let (bytes, evicted) = (self.bytes, g.evicted);
        let mut i = 0i64;
        g.each(|h| {
            let sh = if evicted && n > 0 { bytes / n + if i < bytes % n { 1 } else { 0 } } else { 0 };
            h.slot.set(bytes, if evicted { bytes } else { 0 }, sh);
            i += 1;
        });
    }

    /// `owner` (the portal or statement a scan runs under: `charge::context`) takes a reference.
    pub fn pin_as(self: &Arc<PinSet>, owner: Owner) -> PinGuard {
        let mut g = lock(&self.inner);
        let id = g.next;
        g.next += 1;
        let held = Held { id, slot: Slot { c: Charge::handle(owner), pins: 0, ev: 0, sh: 0 } };
        match g.inline.iter_mut().find(|s| s.is_none()) {
            Some(s) => *s = Some(held),
            None => g.more.push(held), // alloc: Cold
        }
        self.reshare(&mut g);
        PinGuard { set: Arc::clone(self), id }
    }

    /// The calling thread's portal or statement takes a reference.
    pub fn pin(self: &Arc<PinSet>) -> PinGuard {
        self.pin_as(context().0)
    }

    /// The cache evicted the entry: its holders' counts move into evicted.
    pub fn evict(&self) {
        let mut g = lock(&self.inner);
        if !g.evicted {
            g.evicted = true;
            self.reshare(&mut g);
        }
    }

    /// Holders now.
    pub fn holders(&self) -> usize {
        lock(&self.inner).count()
    }
}

impl Drop for PinGuard {
    fn drop(&mut self) {
        let mut g = lock(&self.set.inner);
        let id = self.id;
        let mut mine = None;
        if let Some(s) = g.inline.iter_mut().find(|s| s.as_ref().is_some_and(|h| h.id == id)) {
            mine = s.take();
        } else if let Some(i) = g.more.iter().position(|h| h.id == id) {
            mine = Some(g.more.swap_remove(i));
        }
        if let Some(mut h) = mine {
            h.slot.set(0, 0, 0);
        }
        self.set.reshare(&mut g);
    }
}
