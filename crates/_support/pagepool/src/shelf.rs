//! A per-thread cache of free blocks and short runs, with no atomics. Local to a thread but not owned by it: any thread
//! frees onto its own shelf. Past `shelf_max` (or a key's count cap) half the key's list goes to the central lists as one
//! batch; at thread exit (a pthread key's destructor, armed at first use) the whole shelf does.

use crate::central::{self, Batch, Key, NONE, SIZES};
use crate::reserve::zones;
use crate::{stats, CLASSES};
use core::cell::Cell;
use crate::sync::{tls as thread_local, AtomicU32, AtomicU64, Ordering::*};

const SK: usize = CLASSES * SIZES;

struct Shelf {
    heads: [Cell<u32>; SK],
    counts: [Cell<u32>; SK],
    bytes: Cell<usize>,
    slot: Cell<u32>,
}

thread_local! {
    static SHELF: Shelf = const {
        Shelf { heads: [const { Cell::new(NONE) }; SK], counts: [const { Cell::new(0) }; SK], bytes: Cell::new(0), slot: Cell::new(0) }
    };
}

/// This thread's shelf. The closure only takes the address, so `LocalKey::with` inlines to the TLS offset.
#[inline(always)]
fn here() -> &'static Shelf {
    // SAFETY: the shelf lives as long as its thread, and the reference never leaves the call on that thread that took it.
    unsafe { &*SHELF.with(|s| s as *const Shelf) }
}

static CAPS: crate::Line<[AtomicU32; SIZES]> = crate::Line([const { AtomicU32::new(2) }; SIZES]);
static SHELF_MAX: crate::Line<core::sync::atomic::AtomicUsize> = crate::Line(core::sync::atomic::AtomicUsize::new(0));

pub(crate) fn init() {
    let max = zones().cfg.shelf_max;
    SHELF_MAX.store(max, Relaxed);
    for (s, cap) in CAPS.iter().enumerate() {
        let n = max / 8 / central::slot_bytes(s as u8);
        cap.store(n.clamp(2, 512) as u32, Relaxed);
    }
    exit::make_key();
}

/// This thread's stats slot (taken, and the exit destructor armed, at the first call).
#[inline]
pub(crate) fn slot_index() -> usize {
    SHELF.with(|s| {
        let v = s.slot.get();
        if v != 0 {
            return v as usize - 1;
        }
        let i = stats::take_slot();
        s.slot.set(i as u32 + 1);
        exit::arm();
        i
    })
}

#[cold]
fn first_use() -> usize {
    slot_index()
}

/// A shelf hit for a take: pop `k`'s top block and count `bytes` from shelved to `tag` in use, in one TLS access.
#[inline(always)]
pub(crate) fn pop_use(k: Key, tag: crate::Tag) -> Option<usize> {
    let s = here();
    let i = k.index();
    let h = s.heads[i].get();
    if h == NONE {
        return None;
    }
    let p = central::addr_of_idx(h);
    s.heads[i].set(central::link_next(p));
    s.counts[i].set(s.counts[i].get() - 1);
    let bytes = central::slot_bytes(k.size);
    s.bytes.set(s.bytes.get() - bytes);
    let slot = match s.slot.get() {
        0 => first_use(),
        v => v as usize - 1,
    };
    stats::shelf_move(slot, tag, bytes as isize);
    Some(p)
}

/// A give onto this thread's shelf: count `k`'s bytes from `tag` in use to shelved and push, in one TLS access; past
/// the caps half the list goes to central.
#[inline(always)]
pub(crate) fn push_free(k: Key, tag: crate::Tag, p: usize) {
    let s = here();
    let i = k.index();
    let bytes = central::slot_bytes(k.size);
    let slot = match s.slot.get() {
        0 => first_use(),
        v => v as usize - 1,
    };
    stats::shelf_move(slot, tag, -(bytes as isize));
    central::set_link_next(p, s.heads[i].get());
    s.heads[i].set(central::idx_of(p));
    let n = s.counts[i].get() + 1;
    s.counts[i].set(n);
    let b = s.bytes.get() + bytes;
    s.bytes.set(b);
    if n > CAPS[k.size as usize].load(Relaxed) || b > SHELF_MAX.load(Relaxed) {
        spill_half(k);
    }
}

/// A central batch onto `k`'s empty shelf list, its first block out (that block's count is the caller's).
pub(crate) fn refill(k: Key, b: Batch) -> usize {
    let s_bytes = b.bytes as usize;
    let first = central::addr_of_idx(b.head);
    let rest = central::link_next(first);
    if b.n > 1 {
        SHELF.with(|s| {
            let i = k.index();
            debug_assert_eq!(s.heads[i].get(), NONE);
            s.heads[i].set(rest);
            s.counts[i].set(b.n - 1);
            s.bytes.set(s.bytes.get() + (b.n as usize - 1) * s_bytes);
        });
        stats::shelved(((b.n as usize - 1) * s_bytes) as isize);
    }
    first
}

fn detach(s: &Shelf, k: Key, n: u32) -> Option<Batch> {
    let i = k.index();
    let head = s.heads[i].get();
    if head == NONE || n == 0 {
        return None;
    }
    let mut last = central::addr_of_idx(head);
    for _ in 1..n {
        last = central::addr_of_idx(central::link_next(last));
    }
    s.heads[i].set(central::link_next(last));
    central::set_link_next(last, NONE);
    s.counts[i].set(s.counts[i].get() - n);
    let bytes = central::slot_bytes(k.size);
    s.bytes.set(s.bytes.get() - n as usize * bytes);
    Some(Batch { head, n, bytes: bytes as u32, zero: false })
}

fn spill_half(k: Key) {
    let b = SHELF.with(|s| {
        let n = s.counts[k.index()].get();
        detach(s, k, n.div_ceil(2))
    });
    if let Some(b) = b {
        stats::shelved(-((b.n as usize * b.bytes as usize) as isize));
        central::push_capped(k, b);
    }
}

/// Give this thread's whole shelf to the central lists (idle, release_retained, thread exit).
pub fn flush_shelf() {
    if !crate::on() {
        return;
    }
    flush_where(None);
}

pub(crate) fn flush_where(class: Option<crate::Class>) {
    for i in 0..SK {
        let k = Key::from_index(i);
        if class.is_some_and(|c| k.class != c) {
            continue;
        }
        let b = SHELF.with(|s| detach(s, k, s.counts[i].get()));
        if let Some(b) = b {
            stats::shelved(-((b.n as usize * b.bytes as usize) as isize));
            central::push_capped(k, b);
        }
    }
}

/// The purger's "shelves, come home" flag (one relaxed load).
#[inline]
pub fn low_warm() -> bool {
    crate::state::low()
}

#[cfg_attr(any(target_family = "wasm", miri), allow(dead_code))]
pub(crate) fn mine() -> usize {
    SHELF.with(|s| s.bytes.get())
}

#[cfg(all(unix, not(shuttle)))]
mod exit {
    use super::*;

    static KEY: AtomicU64 = AtomicU64::new(0);

    extern "C" fn at_exit(_: *mut libc::c_void) {
        super::flush_shelf();
        let i = SHELF.with(|s| s.slot.replace(0));
        if i != 0 {
            stats::give_slot(i as usize - 1);
        }
    }

    pub(super) fn make_key() {
        if KEY.load(Acquire) != 0 {
            return;
        }
        let mut key: libc::pthread_key_t = 0;
        // SAFETY: `key` is a valid out pointer; `at_exit` stays valid for the process's life.
        if unsafe { libc::pthread_key_create(&mut key, Some(at_exit)) } == 0 {
            KEY.store(key as u64 + 1, Release);
        }
    }

    pub(super) fn arm() {
        let k = KEY.load(Acquire);
        if k != 0 {
            // SAFETY: a live key; any non-null value arms its destructor for this thread.
            unsafe { libc::pthread_setspecific((k - 1) as libc::pthread_key_t, core::ptr::NonNull::<u8>::dangling().as_ptr().cast()) };
        }
    }
}

#[cfg(any(not(unix), shuttle))]
mod exit {
    pub(super) fn make_key() {}
    pub(super) fn arm() {}
}
