//! Owner folds compiled per layout, per process: at most `SETS * WAYS` live (a full set evicts an unpinned slot and frees
//! its code); a miss compiles under a try-lock, never waited on (the other owners fold through the counted loops).
use super::owner_fold::{full_at, Fold};
use crate::contract::Host;
use gravity_abi::breaker::{Candidates, HashAgg, RecordLayout, FOLD_OVERFLOW, FOLD_PF, LAYOUT_WORDS};
use gravity_abi::hash::KeyHash;
use gravity_abi::program::Entry;
use gravity_abi::Ctx;
use core::any::Any;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

/// The code reads every field at its `offset_of!`. No sub-split: the code has no split filter.
#[repr(C)]
pub struct FoldCall {
    /// First, so a ring slot's address is one add.
    pub ring: [u64; FOLD_PF],
    pub base: *mut u8,
    pub mask: u64,
    pub full_at: u64,
    /// `KeyHash::home`'s shifts; `shr` is taken modulo 64 (a one-slot table's mask is 0).
    pub used: u64,
    pub shr: u64,
    /// In and out.
    pub len: u64,
    pub recs: *const u8,
    pub nrecs: u64,
    pub cand: *mut Candidates,
    pub layout: *const RecordLayout,
}

/// `entry` is mapped while `own` lives.
pub struct Compiled {
    pub entry: Entry,
    pub bytes: usize,
    pub own: Box<dyn Any + Send + Sync>,
}

/// `Later`: nothing kept; the next miss compiles again.
pub enum Made {
    Code(Compiled),
    Refused,
    Later,
}

pub type Compile = fn(&Fold) -> Made;

static COMPILE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

pub fn install(c: Compile) {
    COMPILE.store(c as *mut (), Ordering::Release);
}

pub const SETS: usize = 256;
pub const WAYS: usize = 4;

struct Slot {
    /// Even: stable (0: never filled); odd: being written.
    ver: AtomicU64,
    key: [AtomicU64; LAYOUT_WORDS],
    /// 0: refused.
    entry: AtomicUsize,
    bytes: AtomicUsize,
    /// A pinned slot is never evicted.
    pins: AtomicU32,
    hot: AtomicBool,
    /// Only under `LOCK`, with `ver` odd and `pins` 0.
    own: UnsafeCell<Option<Box<dyn Any + Send + Sync>>>,
}

// SAFETY: every field but `own` is atomic; `own` is touched only under `LOCK`, while `ver` is odd and `pins` is 0.
unsafe impl Sync for Slot {}

#[allow(clippy::declare_interior_mutable_const)]
const EMPTY: Slot = Slot {
    ver: AtomicU64::new(0),
    key: [const { AtomicU64::new(0) }; LAYOUT_WORDS],
    entry: AtomicUsize::new(0),
    bytes: AtomicUsize::new(0),
    pins: AtomicU32::new(0),
    hot: AtomicBool::new(false),
    own: UnsafeCell::new(None),
};

static SLOTS: [Slot; SETS * WAYS] = [EMPTY; SETS * WAYS];
static LOCK: AtomicBool = AtomicBool::new(false);
/// Each set's clock hand: the way its next eviction looks at first. Only under `LOCK`.
static HANDS: [AtomicU8; SETS] = [const { AtomicU8::new(0) }; SETS];

pub static COMPILED: AtomicU64 = AtomicU64::new(0);
pub static REFUSED: AtomicU64 = AtomicU64::new(0);
pub static EVICTED: AtomicU64 = AtomicU64::new(0);
pub static LIVE_BYTES: AtomicU64 = AtomicU64::new(0);

/// Key kinds cleared: the fold never reads them.
pub fn key(l: &RecordLayout) -> [u64; LAYOUT_WORDS] {
    let mut w = l.words();
    let at = core::mem::offset_of!(RecordLayout, keys);
    // SAFETY: the layout's bytes as words (`RecordLayout::words`); the key kinds' bytes lie inside them.
    let b = unsafe { core::slice::from_raw_parts_mut(w.as_mut_ptr().cast::<u8>(), 8 * LAYOUT_WORDS) };
    b[at..at + core::mem::size_of_val(&l.keys)].fill(0);
    w
}

fn set_index(key: &[u64; LAYOUT_WORDS]) -> usize {
    KeyHash::of(key).part(SETS.trailing_zeros()) as usize
}

fn set_of(key: &[u64; LAYOUT_WORDS]) -> &'static [Slot] {
    let s = set_index(key);
    &SLOTS[s * WAYS..(s + 1) * WAYS]
}

pub struct Pin(&'static Slot);

impl Pin {
    pub fn entry(&self) -> Option<Entry> {
        let e = self.0.entry.load(Ordering::Relaxed);
        // SAFETY: a non-zero entry is a `Compiled::entry` stored by `get`, live while the slot is pinned.
        (e != 0).then(|| unsafe { core::mem::transmute::<usize, Entry>(e) })
    }
}

impl Drop for Pin {
    fn drop(&mut self) {
        self.0.pins.fetch_sub(1, Ordering::Release);
    }
}

pub fn find(key: &[u64; LAYOUT_WORDS]) -> Option<Pin> {
    for s in set_of(key) {
        let v = s.ver.load(Ordering::Acquire);
        if v == 0 || v & 1 != 0 || !s.key.iter().zip(key).all(|(a, b)| a.load(Ordering::Relaxed) == *b) {
            continue;
        }
        // pin, then see the slot unchanged: an evictor marks the slot odd before it reads the pins, so either it sees this
        // pin and leaves the slot, or this sees its mark and lets go
        s.pins.fetch_add(1, Ordering::SeqCst);
        if s.ver.load(Ordering::SeqCst) != v {
            s.pins.fetch_sub(1, Ordering::Release);
            continue;
        }
        if !s.hot.load(Ordering::Relaxed) {
            s.hot.store(true, Ordering::Relaxed);
        }
        return Some(Pin(s));
    }
    None
}

struct Unlock;

impl Drop for Unlock {
    fn drop(&mut self) {
        LOCK.store(false, Ordering::Release);
    }
}

/// `None`: no compiler installed, another compile running, refused, or every way of the set pinned.
pub fn get(f: &Fold, key: &[u64; LAYOUT_WORDS]) -> Option<Pin> {
    if let Some(p) = find(key) {
        return Some(p);
    }
    let c = COMPILE.load(Ordering::Acquire);
    if c.is_null() || LOCK.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        return None;
    }
    let _unlock = Unlock;
    if let Some(p) = find(key) {
        return Some(p);
    }
    // SAFETY: only `install` stores here, always from a `Compile`.
    let compile: Compile = unsafe { core::mem::transmute::<*mut (), Compile>(c) };
    let code = match compile(f) {
        Made::Code(c) => Some(c),
        Made::Refused => None,
        Made::Later => return None,
    };
    let s = victim(set_index(key))?;
    // SAFETY: under `LOCK`, the slot odd and unpinned (`victim`): no reader holds it and none can pin it.
    let old = unsafe { core::mem::replace(&mut *s.own.get(), None) };
    if s.ver.load(Ordering::Relaxed) != 1 {
        EVICTED.fetch_add(1, Ordering::Relaxed);
        LIVE_BYTES.fetch_sub(s.bytes.load(Ordering::Relaxed) as u64, Ordering::Relaxed);
    }
    drop(old);
    let (entry, bytes) = match code {
        Some(c) => {
            COMPILED.fetch_add(1, Ordering::Relaxed);
            LIVE_BYTES.fetch_add(c.bytes as u64, Ordering::Relaxed);
            // SAFETY: as above
            unsafe { *s.own.get() = Some(c.own) };
            (c.entry as usize, c.bytes)
        }
        None => {
            REFUSED.fetch_add(1, Ordering::Relaxed);
            (0, 0)
        }
    };
    for (a, b) in s.key.iter().zip(key) {
        a.store(*b, Ordering::Relaxed);
    }
    s.entry.store(entry, Ordering::Relaxed);
    s.bytes.store(bytes, Ordering::Relaxed);
    s.hot.store(true, Ordering::Relaxed);
    s.pins.fetch_add(1, Ordering::SeqCst);
    let v = s.ver.load(Ordering::Relaxed);
    s.ver.store(v + 1, Ordering::Release);
    Some(Pin(s))
}

/// Returns the way marked odd and unpinned. CLOCK from the set's hand: the hand leaves a filed fold behind it, so a set
/// evicts it only after a full turn (a scan from way 0 would take a fold filed a moment ago, its mark just cleared).
fn victim(set: usize) -> Option<&'static Slot> {
    let ways = &SLOTS[set * WAYS..(set + 1) * WAYS];
    if let Some(s) = ways.iter().find(|s| s.ver.load(Ordering::Relaxed) == 0) {
        s.ver.store(1, Ordering::SeqCst);
        return Some(s);
    }
    let hand = &HANDS[set];
    for _ in 0..2 * WAYS {
        let w = hand.load(Ordering::Relaxed) as usize;
        hand.store(((w + 1) % WAYS) as u8, Ordering::Relaxed);
        let s = &ways[w];
        if s.hot.swap(false, Ordering::Relaxed) {
            continue;
        }
        let v = s.ver.load(Ordering::Relaxed);
        s.ver.store(v + 1, Ordering::SeqCst);
        if s.pins.load(Ordering::SeqCst) == 0 {
            return Some(s);
        }
        s.ver.store(v, Ordering::Release);
    }
    None
}

/// # Safety
/// `FoldFn`'s contract, no sub-split; `e` compiled for `(*t).layout`.
pub unsafe fn run(e: Entry, t: *mut HashAgg, recs: *const u8, nrecs: usize, cand: *mut Candidates) -> i64 {
    let (cb, at) = (*t).home_bits();
    let mut c = FoldCall {
        base: (*t).base_ptr(),
        mask: (*t).cap as u64 - 1,
        full_at: full_at((*t).cap) as u64,
        used: at.used() as u64,
        shr: 64 - cb as u64,
        len: (*t).len as u64,
        recs,
        nrecs: nrecs as u64,
        cand,
        layout: &(*t).layout,
        ring: [0; FOLD_PF],
    };
    let v = e(&mut c as *mut FoldCall as *mut Ctx);
    (*t).len = c.len as u32;
    if v == FOLD_OVERFLOW {
        gravity_abi::status::unwind::resume();
    }
    v
}

/// 0 where `fold_slot` refused or unwound (the pad parked the panic; `run` resumes it).
pub struct SlotHost;

impl Host for SlotHost {
    type Args = (u64, u64, u64);

    unsafe fn run(ctx: *mut Ctx, (j, dst, src): (u64, u64, u64)) -> u64 {
        let l = &*(*(ctx as *const FoldCall)).layout;
        gravity_abi::breaker::Rec::at(dst as *mut u8, l).fold_slot(j as usize, &gravity_abi::breaker::Rec::at(src as *mut u8, l)).is_ok() as u64
    }

    fn fail(_a: (u64, u64, u64)) -> u64 {
        0
    }
}
