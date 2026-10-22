//! The superpage table (out-of-line, indexed by `(p - pool.lo) >> 21`) and the free superpages' bitmaps, one per free
//! state. Holding a superpage = having cleared its bit; a superpage in no bitmap belongs to whoever cleared it.

use crate::reserve::zones;
use crate::Class;
use crate::sync::{AtomicU32, AtomicU64, AtomicUsize, Ordering::*};

pub(crate) const SUPER: usize = 2 << 20;
pub(crate) const RUNS_PER: u32 = (SUPER / crate::run::RUN) as u32;
pub(crate) const ALL_RUNS: u32 = u32::MAX;
pub(crate) const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum State {
    Cold = 0,
    WarmZero = 1,
    WarmDirty = 2,
    Partial = 3,
    Full = 4,
    Span = 5,
    Lent = 6,
    Busy = 7,
}

impl State {
    pub(crate) fn from_bits(v: u64) -> State {
        match v & 0xf {
            0 => State::Cold,
            1 => State::WarmZero,
            2 => State::WarmDirty,
            3 => State::Partial,
            4 => State::Full,
            5 => State::Span,
            6 => State::Lent,
            _ => State::Busy,
        }
    }
}

/// word: state:4 | class:3 | (3 unused) | tag:8 | active:1 (bit 18, pppress card 8: `state::note_active`) | bin:5 (bits
/// 19..24, a partial superpage's list: `state`'s hole or used-run bin) | head:32 (a span's
/// first superpage).
/// pppress card 8: the word's activity bit (`state::note_active`; cleared at each age mark).
pub(crate) const ACTIVE: u64 = 1 << 18;
const BIN_SHIFT: u32 = 19;
const BIN_MASK: u64 = 31;

#[repr(C, align(32))]
pub(crate) struct SuperMeta {
    pub word: AtomicU64,
    pub free_runs: AtomicU32,
    pub zero_runs: AtomicU32,
    pub next: AtomicU32,
    pub prev: AtomicU32,
    pub dirty_at: AtomicU32,
    pub span_n: AtomicU32,
}

impl SuperMeta {
    #[inline]
    pub(crate) fn state(&self) -> State {
        State::from_bits(self.word.load(Acquire))
    }

    #[inline]
    pub(crate) fn class(&self) -> Class {
        Class::from_u8(((self.word.load(Relaxed) >> 4) & 7) as u8)
    }

    #[inline]
    pub(crate) fn bin(&self) -> u32 {
        ((self.word.load(Relaxed) >> BIN_SHIFT) & BIN_MASK) as u32
    }

    #[inline]
    pub(crate) fn tag(&self) -> u8 {
        (self.word.load(Relaxed) >> 10) as u8
    }

    /// pppress (B): the word's tag bits from now on, its other bits as they are (the activity bit may move meanwhile).
    pub(crate) fn set_tag(&self, tag: u8) {
        let mut w = self.word.load(Acquire);
        loop {
            let n = (w & !(0xff << 10)) | (tag as u64) << 10;
            match self.word.compare_exchange_weak(w, n, AcqRel, Acquire) {
                Ok(_) => return,
                Err(x) => w = x,
            }
        }
    }

    #[inline]
    pub(crate) fn head(&self) -> u32 {
        (self.word.load(Relaxed) >> 32) as u32
    }

    #[inline]
    pub(crate) fn set(&self, st: State, class: Class, bin: u32, tag: u8, head: u32) {
        let w = st as u64 | (class as u64) << 4 | (bin as u64 & BIN_MASK) << BIN_SHIFT | (tag as u64) << 10 | (head as u64) << 32;
        self.word.store(w, Release);
    }

    #[inline]
    pub(crate) fn set_bin(&self, bin: u32) {
        let w = self.word.load(Relaxed);
        self.word.store((w & !(BIN_MASK << BIN_SHIFT)) | (bin as u64 & BIN_MASK) << BIN_SHIFT, Release);
    }
}

#[inline]
pub(crate) fn index_of(addr: usize) -> u32 {
    ((addr - zones().pool.lo) / SUPER) as u32
}

#[inline]
pub(crate) fn addr_of(i: u32) -> usize {
    zones().pool.lo + i as usize * SUPER
}

#[inline]
pub(crate) fn meta(i: u32) -> &'static SuperMeta {
    let z = zones();
    debug_assert!(i < z.nsuper);
    // SAFETY: the table holds `nsuper` entries in the meta zone, open read-write for the process's life; all-zero is a
    // valid SuperMeta (atomics).
    unsafe { &*(z.supers as *const SuperMeta).add(i as usize) }
}

#[inline]
pub(crate) fn per_chunk() -> u32 {
    (zones().chunk / SUPER) as u32
}

#[inline]
pub(crate) fn stride_slots() -> u32 {
    (zones().stride / SUPER) as u32
}

#[inline]
pub(crate) fn is_guard(i: u32) -> bool {
    i % stride_slots() >= per_chunk()
}

#[inline]
fn bm(st: State) -> usize {
    match st {
        State::Cold => 0,
        State::WarmZero => 1,
        State::WarmDirty => 2,
        _ => unreachable!("not a free state"),
    }
}

const FREE: [State; 3] = [State::Cold, State::WarmZero, State::WarmDirty];

#[inline]
fn bitmap(b: usize) -> &'static [AtomicU64] {
    let z = zones();
    // SAFETY: `words` words in the meta zone, open for the process's life.
    unsafe { core::slice::from_raw_parts(z.bits[b] as *const AtomicU64, z.words) }
}

static COUNT: [crate::Line<AtomicUsize>; 3] = [const { crate::Line(AtomicUsize::new(0)) }; 3];
static HINT: [crate::Line<AtomicUsize>; 3] = [const { crate::Line(AtomicUsize::new(0)) }; 3];

#[inline]
pub(crate) fn count(st: State) -> usize {
    COUNT[bm(st)].load(Relaxed)
}

/// Make superpage `i` free in `st` (Cold / WarmZero / WarmDirty). The caller held it.
pub(crate) fn put(i: u32, st: State) {
    let m = meta(i);
    m.set(st, Class::Task, 0, 0, NONE);
    m.free_runs.store(ALL_RUNS, Relaxed);
    if st != State::Cold {
        m.dirty_at.store(crate::purger::tick(), Relaxed);
    }
    let b = bm(st);
    COUNT[b].fetch_add(1, Relaxed);
    crate::warmres::door(st, SUPER as i64);
    HINT[b].fetch_min(i as usize / 64, Relaxed);
    bitmap(b)[i as usize / 64].fetch_or(1 << (i % 64), Release);
}

#[inline]
fn took(i: u32, st: State) -> State {
    COUNT[bm(st)].fetch_sub(1, Relaxed);
    crate::warmres::door(st, -(SUPER as i64));
    let m = meta(i);
    m.zero_runs.store(if st == State::WarmDirty { 0 } else { ALL_RUNS }, Relaxed);
    m.set(State::Busy, Class::Task, 0, st as u8, NONE);
    st
}

/// Take any free superpage in `st`, lowest index first.
pub(crate) fn take_one(st: State) -> Option<u32> {
    let b = bm(st);
    if COUNT[b].load(Relaxed) == 0 {
        return None;
    }
    let map = bitmap(b);
    let n = map.len();
    let start = HINT[b].load(Relaxed).min(n.saturating_sub(1));
    for k in 0..n {
        let wi = (start + k) % n;
        let mut w = map[wi].load(Acquire);
        while w != 0 {
            let bit = w.trailing_zeros();
            match map[wi].compare_exchange_weak(w, w & !(1 << bit), AcqRel, Acquire) {
                Ok(_) => {
                    let i = (wi * 64) as u32 + bit;
                    took(i, st);
                    return Some(i);
                }
                Err(now) => w = now,
            }
        }
        if k == 0 && wi == start {
            let _ = HINT[b].compare_exchange(start, (start + 1).min(n - 1), Relaxed, Relaxed);
        }
    }
    None
}

/// Take superpage `i` if it is free; the free state it had.
pub(crate) fn take_bit(i: u32) -> Option<State> {
    let (wi, bit) = (i as usize / 64, 1u64 << (i % 64));
    for (b, st) in FREE.iter().enumerate() {
        let map = bitmap(b);
        if map[wi].load(Acquire) & bit != 0 && map[wi].fetch_and(!bit, AcqRel) & bit != 0 {
            return Some(took(i, *st));
        }
    }
    None
}

fn chunk_lock(c: u32) -> &'static AtomicU32 {
    // SAFETY: `chunks` words in the meta zone, open for the process's life.
    unsafe { &*(zones().chunk_locks as *const AtomicU32).add(c as usize) }
}

pub(crate) fn spin(l: &AtomicU32) {
    let mut n = 0u32;
    while l.compare_exchange_weak(0, 1, Acquire, Relaxed).is_err() {
        n += 1;
        if n < 64 {
            crate::sync::spin_loop();
        } else {
            std::thread::yield_now();
        }
    }
}

pub(crate) fn unspin(l: &AtomicU32) {
    l.store(0, Release);
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Taken {
    pub first: u32,
    pub cold: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Pref {
    Warm,
    Cold,
    Any,
}

#[inline]
fn is_pref(i: u32, pref: Pref) -> bool {
    let (wi, bit) = (i as usize / 64, 1u64 << (i % 64));
    let on = |b: usize| bitmap(b)[wi].load(Relaxed) & bit != 0;
    match pref {
        Pref::Warm => on(1) || on(2),
        Pref::Cold => on(0),
        Pref::Any => on(0) || on(1) || on(2),
    }
}

/// Take `n` contiguous free superpages in chunk `chunk` (pool spans); the chunk's spin bit is held meanwhile.
pub(crate) fn free_contig(chunk: u32, n: u32, pref: Pref) -> Option<Taken> {
    let per = per_chunk();
    if n == 0 || n > per {
        return None;
    }
    let lock = chunk_lock(chunk);
    spin(lock);
    let base = chunk * stride_slots();
    let mut got = None;
    let mut i = 0;
    'scan: while i + n <= per {
        let mut j = 0;
        while j < n {
            if !is_pref(base + i + j, pref) {
                i += j + 1;
                continue 'scan;
            }
            j += 1;
        }
        let mut cold = 0;
        for k in 0..n {
            match take_bit(base + i + k) {
                Some(st) => cold += (st == State::Cold) as u32,
                None => {
                    for r in 0..k {
                        put_back(base + i + r);
                    }
                    i += k + 1;
                    continue 'scan;
                }
            }
        }
        got = Some(Taken { first: base + i, cold });
        break;
    }
    unspin(lock);
    got
}

#[inline]
pub(crate) fn was(i: u32) -> State {
    State::from_bits(meta(i).tag() as u64)
}

/// Return a held superpage that was not used to the free state it had.
pub(crate) fn put_back(i: u32) {
    put(i, was(i));
}

/// Take the `n` superpages after `first + have` if free (a span growing in place): how many were Cold.
pub(crate) fn take_after(first: u32, have: u32, n: u32) -> Option<u32> {
    let c = first / stride_slots();
    let off = first % stride_slots();
    if off + have + n > per_chunk() {
        return None;
    }
    let lock = chunk_lock(c);
    spin(lock);
    let mut cold = 0;
    let mut ok = true;
    for k in 0..n {
        match take_bit(first + have + k) {
            Some(st) => cold += (st == State::Cold) as u32,
            None => {
                for r in 0..k {
                    put_back(first + have + r);
                }
                ok = false;
                break;
            }
        }
    }
    unspin(lock);
    ok.then_some(cold)
}

pub(crate) fn chunks() -> u32 {
    zones().chunks
}
