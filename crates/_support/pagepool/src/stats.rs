//! Exact counters and the line wake. Per-thread slots (single writer: relaxed load + store) for the hot counts; process
//! atomics for what moves at superpage or batch grain. Sums are exact at quiescence.

use crate::superpage::{self, State, SUPER};
use crate::Tag;
use crate::sync::{AtomicIsize, AtomicPtr, AtomicU64, AtomicUsize, Ordering::*};

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// RAM the pool holds from the OS, not yet released.
    pub claimed: usize,
    /// Whole free superpages, prefaulted zero / dirty (dirty includes warm lazy spans' pages).
    pub warm_zero: usize,
    pub warm_dirty: usize,
    /// Free blocks and runs on the central lists / on shelves.
    pub central: usize,
    pub shelved: usize,
    /// Dead ranges inside live blocks, not yet released.
    pub dead: usize,
    /// Free runs in superpages not wholly free, and free blocks parked in runs not wholly free.
    pub stranded: usize,
    /// pppress (pool_free_listed): the part of `stranded` that is free blocks parked in runs not wholly free (`stranded`
    /// less its free runs).
    pub parked: usize,
    pub waste: usize,
    pub in_use: [usize; Tag::COUNT],
    pub lazy_committed: usize,
    /// Pool superpages moved into the frame zone and not yet back (each a mapping of its own there).
    pub moved_vmas: usize,
    /// The claimer's target now; takes that found the warm lists dry (a Cold claim on the caller's path).
    pub warm_target: usize,
    pub warm_misses: u64,
    pub claims: u64,
    pub releases: u64,
    pub released_bytes: u64,
    pub purge_ns: u64,
    pub warm_takes: u64,
    pub cold_takes: u64,
    /// The claimer's prefaulted bytes so far, and its fills the memory ladder's gate held (`set_claim_room`).
    pub prefaulted: u64,
    pub claim_held: u64,
    /// pppress card 13 (2), by tag: the live lazy spans' commit past what their takes asked for (`usable` of the ask), in
    /// in_use: a warm commit a take kept with `PGRUST_POOL_SPAN_TRIM` off. 0 with the trim on.
    pub commit_past_ask: [usize; Tag::COUNT],
}

impl Stats {
    pub fn warm_free(&self) -> usize {
        self.warm_zero + self.warm_dirty + self.central + self.shelved + self.dead
    }

    pub fn in_use_total(&self) -> usize {
        self.in_use.iter().sum()
    }
}

pub(crate) struct Slot {
    pub in_use: [AtomicIsize; Tag::COUNT],
    pub shelved: AtomicIsize,
}

const SLOT0: Slot = Slot { in_use: [const { AtomicIsize::new(0) }; Tag::COUNT], shelved: AtomicIsize::new(0) };
const NSLOTS: usize = 1024;
static SLOTS: [Slot; NSLOTS] = [SLOT0; NSLOTS];
/// Threads past NSLOTS share this one (read-modify-write) and the exited threads' residue folds here.
static SHARED: Slot = SLOT0;
static USED: [AtomicU64; NSLOTS / 64] = [const { AtomicU64::new(0) }; NSLOTS / 64];

pub(crate) static CLAIMED: crate::Line<AtomicIsize> = crate::Line(AtomicIsize::new(0));
/// D4880: big global blocks the ready pool refused that the system served (`os_big`), and their bytes.
static OS_BIG: AtomicU64 = AtomicU64::new(0);
static OS_BIG_BYTES: AtomicU64 = AtomicU64::new(0);

#[cfg_attr(not(feature = "global"), allow(dead_code))]
pub(crate) fn note_os_big(bytes: usize) {
    OS_BIG.fetch_add(1, Relaxed);
    OS_BIG_BYTES.fetch_add(bytes as u64, Relaxed);
}

/// D4880: (blocks, bytes) the system served for big global asks the ready pool refused (no lazy-zone address space).
pub fn os_big() -> (u64, u64) {
    (OS_BIG.load(Relaxed), OS_BIG_BYTES.load(Relaxed))
}
pub(crate) static STRANDED: crate::Line<AtomicIsize> = crate::Line(AtomicIsize::new(0));
/// The resident free runs of superpages not wholly free (part of STRANDED: its parked blocks are not).
pub(crate) static FREE_RUNS: crate::Line<AtomicIsize> = crate::Line(AtomicIsize::new(0));
pub(crate) static WASTE: crate::Line<AtomicIsize> = crate::Line(AtomicIsize::new(0));
pub(crate) static DEAD: crate::Line<AtomicIsize> = crate::Line(AtomicIsize::new(0));
pub(crate) static LAZY_COMMITTED: crate::Line<AtomicIsize> = crate::Line(AtomicIsize::new(0));
pub(crate) static LAZY_WARM: crate::Line<AtomicIsize> = crate::Line(AtomicIsize::new(0));
pub(crate) static CLAIMS: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static RELEASES: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static RELEASED_BYTES: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
/// pppress (present's overshoot fix): the parts of RELEASED_BYTES that the present sweep did not class free. LIVE: pages
/// of an allocation live at the sweep (a live lazy span's trimmed tail; a span given back and dropped before any sweep
/// saw it warm), counted present but not free. UNSWEPT: pages the sweep never reads (a frame's, outside the pool zone).
pub(crate) static RELEASED_LIVE: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static RELEASED_UNSWEPT: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static PURGE_NS: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static WARM_TAKES: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static COLD_TAKES: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static WARM_MISSES: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
/// pagepool P3 (d): Cold superpages of a span's run filled from scattered warm ones (`span::fill_cold`).
pub(crate) static MOVED_IN: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static PREFAULTED: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
/// pppress card 13 (2): `Stats::commit_past_ask`, by tag (`span::set_past` moves it).
pub(crate) static COMMIT_PAST_ASK: [AtomicIsize; Tag::COUNT] = [const { AtomicIsize::new(0) }; Tag::COUNT];
pub(crate) static CLAIM_HELD: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
/// D4760 (`pool_thp_partial`): partial give-backs of superpages advised huge kept whole (1) and their bytes (as asked:
/// a range a later pass asks again counts again), and the folio splits asked first (2) and their ns.
pub(crate) static THP_KEPT: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static THP_KEPT_BYTES: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static THP_SPLITS: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
/// pppress card 8 (`pool_thp_partial` 3): superpages and lazy span pieces an age pass left held because they were hot.
pub(crate) static THP_HOT_SKIPS: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static THP_SPLIT_NS: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));

/// pagepool P3 (d): the Cold superpages span asks filled from scattered warm ones so far (each a claim saved).
pub fn moved_in() -> u64 {
    MOVED_IN.load(Relaxed)
}

static LINE: AtomicUsize = AtomicUsize::new(usize::MAX);
static WAKE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Call `wake` once each time `claimed` crosses `bytes` upward (superpage grain).
pub fn set_line(bytes: usize, wake: fn()) {
    WAKE.store(wake as *mut (), Release);
    LINE.store(bytes, Release);
}

/// Claimed bytes moved by `delta` (a claim > 0, a release < 0): the door and the line.
pub(crate) fn claim(delta: isize) {
    let old = CLAIMED.fetch_add(delta, Relaxed);
    if delta > 0 {
        CLAIMS.fetch_add(1, Relaxed);
        // pppress card 6 (`PGRUST_POOL_GROW_GUARD`): owed back by this thread's next settle point
        crate::purger::owe(delta as usize);
        let line = LINE.load(Acquire);
        if (old.max(0) as usize) < line && (old + delta) as usize >= line {
            let f = WAKE.load(Acquire);
            if !f.is_null() {
                // SAFETY: only `set_line` stores here, always a `fn()`.
                let f: fn() = unsafe { core::mem::transmute::<*mut (), fn()>(f) };
                f();
            }
        }
    }
    crate::door(delta as i64);
}

pub(crate) fn claimed() -> usize {
    CLAIMED.load(Relaxed).max(0) as usize
}

/// Whether `bytes` more may be claimed under the cap: `set_cap`'s, else the memory budget B (0 = claim freely).
pub(crate) fn under_cap(bytes: usize) -> bool {
    let cap = cap();
    cap == 0 || claimed() + bytes <= cap || crate::global_uncapped()
}

/// The claim cap: `set_cap`'s, else the memory budget B; 0 = none.
pub(crate) fn cap() -> usize {
    match crate::CAP.load(Relaxed) {
        #[cfg(not(loom))]
        0 => memledger::b_at() as usize,
        c => c,
    }
}

/// pagepool P3 (c): a fingerprint of the counts that move with the pool's resident bytes: claims and releases, warm and
/// Cold takes, prefaults, lazy commits, dead and free runs, and each thread's in-use and shelved bytes (a block taken or
/// given inside memory already claimed). Equal across two reads: nothing counted moved between them. A first write to a
/// page of a block taken earlier moves no count; the present sweep's longest skip (`present::QUIET_MS`) bounds that.
pub(crate) fn activity() -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut mix = |v: u64| h = (h ^ v).wrapping_mul(0x0100_0000_01b3);
    for c in [&CLAIMS, &RELEASES, &RELEASED_BYTES, &WARM_TAKES, &COLD_TAKES, &WARM_MISSES, &MOVED_IN, &PREFAULTED] {
        mix(c.load(Relaxed));
    }
    for c in [&CLAIMED, &STRANDED, &FREE_RUNS, &WASTE, &DEAD, &LAZY_COMMITTED, &LAZY_WARM] {
        mix(c.load(Relaxed) as u64);
    }
    let mut slot = |s: &Slot| {
        for t in &s.in_use {
            mix(t.load(Relaxed) as u64);
        }
        mix(s.shelved.load(Relaxed) as u64);
    };
    for (w, word) in USED.iter().enumerate() {
        let mut v = word.load(Relaxed);
        while v != 0 {
            let bit = v.trailing_zeros() as usize;
            slot(&SLOTS[w * 64 + bit]);
            v &= v - 1;
        }
    }
    slot(&SHARED);
    h
}

pub(crate) fn take_slot() -> usize {
    for (w, word) in USED.iter().enumerate() {
        let mut v = word.load(Relaxed);
        while v != u64::MAX {
            let bit = (!v).trailing_zeros();
            match word.compare_exchange_weak(v, v | 1 << bit, Acquire, Relaxed) {
                Ok(_) => return w * 64 + bit as usize,
                Err(now) => v = now,
            }
        }
    }
    NSLOTS
}

pub(crate) fn give_slot(i: usize) {
    if i >= NSLOTS {
        return;
    }
    let s = &SLOTS[i];
    for t in 0..Tag::COUNT {
        SHARED.in_use[t].fetch_add(s.in_use[t].swap(0, Relaxed), Relaxed);
    }
    SHARED.shelved.fetch_add(s.shelved.swap(0, Relaxed), Relaxed);
    USED[i / 64].fetch_and(!(1 << (i % 64)), Release);
}

#[inline]
pub(crate) fn slot_at(i: usize) -> (&'static Slot, bool) {
    match SLOTS.get(i) {
        Some(s) => (s, false),
        None => (&SHARED, true),
    }
}

/// `d` bytes move from shelved to in use (a take) or back (a give, `d` < 0) on slot `i`.
#[inline(always)]
pub(crate) fn shelf_move(i: usize, tag: Tag, d: isize) {
    let (s, shared) = slot_at(i);
    bump(&s.shelved, -d, shared);
    bump(&s.in_use[tag.idx()], d, shared);
}

#[inline]
fn bump(c: &AtomicIsize, d: isize, shared: bool) {
    if shared {
        c.fetch_add(d, Relaxed);
    } else {
        c.store(c.load(Relaxed) + d, Relaxed);
    }
}

#[inline]
pub(crate) fn in_use(tag: Tag, d: isize) {
    let i = crate::shelf::slot_index();
    let s = SLOTS.get(i).unwrap_or(&SHARED);
    bump(&s.in_use[tag.idx()], d, i >= NSLOTS);
}

#[inline]
pub(crate) fn shelved(d: isize) {
    let i = crate::shelf::slot_index();
    let s = SLOTS.get(i).unwrap_or(&SHARED);
    bump(&s.shelved, d, i >= NSLOTS);
}

pub fn stats() -> Stats {
    let mut st = Stats::default();
    if !crate::on() {
        return st;
    }
    let mut in_use = [0isize; Tag::COUNT];
    let mut shelved = 0isize;
    for s in SLOTS.iter().chain(core::iter::once(&SHARED)) {
        for (t, v) in in_use.iter_mut().enumerate() {
            *v += s.in_use[t].load(Relaxed);
        }
        shelved += s.shelved.load(Relaxed);
    }
    let pos = |v: isize| v.max(0) as usize;
    for (t, v) in in_use.iter().enumerate() {
        st.in_use[t] = pos(*v);
    }
    st.claimed = pos(CLAIMED.load(Relaxed));
    st.warm_zero = superpage::count(State::WarmZero) * SUPER;
    st.warm_dirty = superpage::count(State::WarmDirty) * SUPER + pos(LAZY_WARM.load(Relaxed));
    st.central = crate::central::total();
    st.shelved = pos(shelved);
    st.dead = pos(DEAD.load(Relaxed));
    st.stranded = pos(STRANDED.load(Relaxed));
    st.parked = st.stranded.saturating_sub(pos(FREE_RUNS.load(Relaxed)));
    st.waste = pos(WASTE.load(Relaxed));
    st.lazy_committed = pos(LAZY_COMMITTED.load(Relaxed));
    st.moved_vmas = crate::frames::MOVED.load(Relaxed);
    st.warm_target = crate::claimer::target();
    st.warm_misses = WARM_MISSES.load(Relaxed);
    st.claims = CLAIMS.load(Relaxed);
    st.releases = RELEASES.load(Relaxed);
    st.released_bytes = RELEASED_BYTES.load(Relaxed);
    st.purge_ns = PURGE_NS.load(Relaxed);
    st.warm_takes = WARM_TAKES.load(Relaxed);
    st.cold_takes = COLD_TAKES.load(Relaxed);
    st.prefaulted = PREFAULTED.load(Relaxed);
    st.claim_held = CLAIM_HELD.load(Relaxed);
    for (t, v) in st.commit_past_ask.iter_mut().enumerate() {
        *v = pos(COMMIT_PAST_ASK[t].load(Relaxed));
    }
    st
}

#[cfg_attr(any(target_family = "wasm", miri), allow(dead_code))]
pub(crate) fn check() -> Result<(), &'static str> {
    let s = stats();
    let parts = s.warm_zero + s.warm_dirty + s.central + s.shelved + s.dead + s.stranded + s.waste + s.in_use_total();
    if s.claimed != parts {
        return Err("claimed != warm_zero + warm_dirty + central + shelved + dead + stranded + waste + in_use");
    }
    crate::state::check()
}
