//! M3-F reader gates (chunk table §5 M3-F, verbatim):
//!
//! - [`open_faults`] — O(streams-touched) born-RED: opening one column of a
//!   wide shredded part faults only that column's streams (fault-counter
//!   witness, both teeth), + the per-face fault-policy pins.
//! - [`refusal`] — per-section CRC refusal on EVERY section kind, structural
//!   truncation/corruption battery, typed encoding/wrapper/expect refusals,
//!   and the seeded byte-flip no-panic battery (the M3-K fuzzer's PR-local
//!   tooth).
//! - [`cache`] — part cache: equal identity ⇒ identical bytes, pin + LRU
//!   janitor under budget pressure, no eviction of pinned parts.
//! - [`dict`] — lazy ensure-frame/ensure-code, generation-stable payload
//!   region pin (the StrView zero-copy dependency), varlena-shaped entry
//!   presentation pin (StrView §7b — this crate's chartered pinning test).
//! - [`decode_props`] — decode_sel ≡ decode_full ∘ select THROUGH the
//!   dispatch layer, full-decode correctness across classes/encodings/nulls/
//!   overflow/multi-extent/child streams, validity + meta faces.
//! - [`manifest`] — effective-generation walk: clog fence, absent-CURRENT
//!   emptiness, typed chain refusals, expectation facts.
//! - [`pins`] — `size_of` pins on the structs that price engagement.
//! - [`residency`] — D259: a `Residency::Private` cursor (the row scan's
//!   bulk-read posture) decodes what a shared one does, admits nothing to
//!   the part's residency plane and holds one extent per stream.
//!
//! Loom models for the shared segment-map/registry/dict states live in
//! `tests/loom.rs` (separate build cone, `--cfg loom`).

mod batch;
mod cache;
mod coldio;
mod decode_props;
mod dict;
mod evict_order;
mod manifest;
mod mem;
mod oom;
mod open_faults;
mod pins;
mod refusal;
mod residency;

std::thread_local! {
    static COUNTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static ALLOCS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// D4322: while nonzero, this thread's 8-aligned allocations of at least this many bytes are refused (null), as a
    /// hard arena or `ulimit -v` refuses them (`refusing`).
    static REFUSE_FROM: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// D4595: while set, this thread's allocations and frees are door 1's (`memledger::door_heap`, as the server's
    /// allocator counts them), and each free notes the lowest its account reads after it (`door1`).
    static DOOR1: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static DOOR1_LOW: std::cell::Cell<i64> = const { std::cell::Cell::new(i64::MAX) };
}

/// Counts the allocations a thread makes while its COUNTING is set (`counted`).
struct Counting;

// SAFETY: a pass-through to System plus a thread-local counter.
unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        if COUNTING.with(std::cell::Cell::get) {
            ALLOCS.with(|a| a.set(a.get() + 1));
        }
        let from = REFUSE_FROM.with(std::cell::Cell::get);
        if from != 0 && layout.align() == 8 && layout.size() >= from {
            return core::ptr::null_mut();
        }
        // SAFETY: forwarded verbatim.
        let p = unsafe { std::alloc::System.alloc(layout) };
        if !p.is_null() && DOOR1.try_with(std::cell::Cell::get).unwrap_or(false) {
            memledger::door_heap(layout.size() as isize);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        // SAFETY: forwarded verbatim.
        unsafe { std::alloc::System.dealloc(ptr, layout) };
        if DOOR1.try_with(std::cell::Cell::get).unwrap_or(false) {
            memledger::door_heap(-(layout.size() as isize));
            // the account as a fold left it (`account_row` reads atomics only)
            let now = memledger::account_row(memledger::current()).account_bytes;
            let _ = DOOR1_LOW.try_with(|l| l.set(l.get().min(now)));
        }
    }
}

/// D4595: `f` with this thread's allocations and frees on door 1, and the lowest its account read after a free.
fn door1<R>(f: impl FnOnce() -> R) -> (R, i64) {
    DOOR1_LOW.with(|l| l.set(i64::MAX));
    DOOR1.with(|d| d.set(true));
    let r = f();
    DOOR1.with(|d| d.set(false));
    (r, DOOR1_LOW.with(std::cell::Cell::get))
}

#[global_allocator]
static COUNTED: Counting = Counting;

/// `f`'s result and the allocations it made on this thread.
fn counted<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let before = ALLOCS.with(std::cell::Cell::get);
    COUNTING.with(|c| c.set(true));
    let r = f();
    COUNTING.with(|c| c.set(false));
    (r, ALLOCS.with(std::cell::Cell::get) - before)
}

/// `f`'s result with this thread's 8-aligned allocations of `from` bytes or more refused (D4322).
fn refusing<R>(from: usize, f: impl FnOnce() -> R) -> R {
    struct Off;
    impl Drop for Off {
        fn drop(&mut self) {
            REFUSE_FROM.with(|r| r.set(0));
        }
    }
    let _off = Off;
    REFUSE_FROM.with(|r| r.set(from.max(1)));
    f()
}

use std::sync::Arc;

use crate::openpart::{OpenPart, PartExpect};
use crate::testpart::BuiltPart;

/// Open a built part over MemPartIo with the given synthetic identity.
pub(crate) fn open_built(b: &BuiltPart, dev: u64, ino: u64) -> Arc<OpenPart> {
    Arc::new(
        OpenPart::open(Box::new(b.mem_io(dev, ino)), &PartExpect::none()).expect("open built part"),
    )
}

/// An 8-aligned decode arena backing buffer.
pub(crate) struct ArenaBuf {
    words: Vec<u64>,
}

impl ArenaBuf {
    pub(crate) fn new(bytes: usize) -> ArenaBuf {
        ArenaBuf {
            words: vec![0u64; bytes.div_ceil(8)],
        }
    }

    pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: u64 → u8 reinterpret of an exclusively borrowed buffer.
        unsafe {
            core::slice::from_raw_parts_mut(
                self.words.as_mut_ptr() as *mut u8,
                self.words.len() * 8,
            )
        }
    }
}

/// Deterministic xorshift64* (seeded batteries — no `rand`, no clocks).
pub(crate) struct XorShift(pub u64);

impl XorShift {
    pub(crate) fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub(crate) fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}
