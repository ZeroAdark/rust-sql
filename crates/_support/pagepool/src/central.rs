//! The central lists: per (class, size) a tagged Treiber stack of batches. A batch is a chain of free blocks (or runs)
//! linked through a `Link` written into each; offsets are 512 B grains from the pool's start. A pop's read of the head's
//! `next_batch` may be stale: the pool zone is never unmapped or PROT_NONE'd, so the read is safe and the tag fails the CAS.

use crate::reserve::{zones, GRAIN_SHIFT};
use crate::run::{self, LIST_RUNS, NB, RUN};
use crate::{Class, CLASSES};
use crate::sync::{AtomicU32, AtomicU64, AtomicUsize, Ordering::*};

pub const NONE: u32 = u32::MAX;
pub(crate) const SIZES: usize = NB + LIST_RUNS as usize;
pub(crate) const KEYS: usize = CLASSES * SIZES;

/// A 64-bit atomic the stack runs on (core's; loom's in the model).
pub trait Atom64 {
    fn load(&self, o: core::sync::atomic::Ordering) -> u64;
    fn cas(&self, cur: u64, new: u64, ok: core::sync::atomic::Ordering, err: core::sync::atomic::Ordering) -> Result<u64, u64>;
    /// Store `new`, the old value back (a CAS loop unless the atomic has its own).
    #[cfg_attr(not(feature = "global"), allow(dead_code))]
    fn swap(&self, new: u64, o: core::sync::atomic::Ordering) -> u64 {
        let mut cur = self.load(core::sync::atomic::Ordering::Relaxed);
        loop {
            match self.cas(cur, new, o, core::sync::atomic::Ordering::Relaxed) {
                Ok(v) => return v,
                Err(now) => cur = now,
            }
        }
    }
}

impl Atom64 for AtomicU64 {
    #[inline]
    fn load(&self, o: core::sync::atomic::Ordering) -> u64 {
        AtomicU64::load(self, o)
    }
    #[inline]
    fn cas(&self, cur: u64, new: u64, ok: core::sync::atomic::Ordering, err: core::sync::atomic::Ordering) -> Result<u64, u64> {
        self.compare_exchange_weak(cur, new, ok, err)
    }
    #[inline]
    fn swap(&self, new: u64, o: core::sync::atomic::Ordering) -> u64 {
        AtomicU64::swap(self, new, o)
    }
}

pub trait Nodes {
    fn next_batch(&self, i: u32) -> u32;
    fn set_next_batch(&self, i: u32, v: u32);
}

/// A tagged Treiber stack of node indices: head = index:32 | tag:32, the tag bumped by every change.
pub struct Stack<A: Atom64 = AtomicU64> {
    head: A,
}

impl Stack<AtomicU64> {
    pub const fn new() -> Self {
        Stack { head: AtomicU64::new(NONE as u64) }
    }
}

impl<A: Atom64> Stack<A> {
    #[cfg(loom)]
    pub fn with(head: A) -> Self {
        Stack { head }
    }

    #[inline]
    fn next(h: u64, i: u32) -> u64 {
        i as u64 | ((h >> 32).wrapping_add(1) << 32)
    }

    pub fn push(&self, nodes: &impl Nodes, i: u32) {
        let mut h = self.head.load(Relaxed);
        loop {
            nodes.set_next_batch(i, h as u32);
            match self.head.cas(h, Self::next(h, i), Release, Relaxed) {
                Ok(_) => return,
                Err(now) => h = now,
            }
        }
    }

    pub fn pop(&self, nodes: &impl Nodes) -> Option<u32> {
        let mut h = self.head.load(Acquire);
        loop {
            let i = h as u32;
            if i == NONE {
                return None;
            }
            let next = nodes.next_batch(i);
            match self.head.cas(h, Self::next(h, next), Acquire, Acquire) {
                Ok(_) => return Some(i),
                Err(now) => h = now,
            }
        }
    }

    /// Empty the stack; the old top (its nodes chain through `next_batch`).
    pub fn take_all(&self) -> Option<u32> {
        let mut h = self.head.load(Relaxed);
        loop {
            if h as u32 == NONE {
                return None;
            }
            match self.head.cas(h, Self::next(h, NONE), Acquire, Relaxed) {
                Ok(_) => return Some(h as u32),
                Err(now) => h = now,
            }
        }
    }
}

/// next:u32 | n:u32 | next_batch:u32 | zero:u32, at the start of each free block.
#[repr(C)]
pub(crate) struct Link {
    pub next: AtomicU32,
    pub n: AtomicU32,
    pub next_batch: AtomicU32,
    pub zero: AtomicU32,
}

#[inline]
pub(crate) fn link(p: usize) -> &'static Link {
    // SAFETY: `p` is a free pool block of 1 KiB or more, 512 B aligned, in the pool zone, which stays mapped read-write
    // for the process's life; its first 16 bytes are the pool's while it is free.
    unsafe { &*(p as *const Link) }
}

#[inline]
pub(crate) fn idx_of(p: usize) -> u32 {
    ((p - zones().pool.lo) >> GRAIN_SHIFT) as u32
}

#[inline]
pub(crate) fn addr_of_idx(i: u32) -> usize {
    zones().pool.lo + ((i as usize) << GRAIN_SHIFT)
}

#[inline]
pub(crate) fn link_next(p: usize) -> u32 {
    link(p).next.load(Relaxed)
}

#[inline]
pub(crate) fn set_link_next(p: usize, v: u32) {
    link(p).next.store(v, Relaxed);
}

struct Pool;

impl Nodes for Pool {
    #[inline]
    fn next_batch(&self, i: u32) -> u32 {
        link(addr_of_idx(i)).next_batch.load(Relaxed)
    }
    #[inline]
    fn set_next_batch(&self, i: u32, v: u32) {
        link(addr_of_idx(i)).next_batch.store(v, Relaxed);
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Batch {
    pub head: u32,
    pub n: u32,
    pub bytes: u32,
    pub zero: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Key {
    pub class: Class,
    pub size: u8,
    pub node: u8,
}

impl Key {
    #[inline]
    pub(crate) fn new(class: Class, size: u8) -> Key {
        Key { class, size, node: 0 }
    }

    #[inline]
    pub(crate) fn index(self) -> usize {
        self.class as usize * SIZES + self.size as usize
    }

    #[inline]
    pub(crate) fn from_index(i: usize) -> Key {
        Key::new(Class::from_u8((i / SIZES) as u8), (i % SIZES) as u8)
    }

    #[inline]
    pub(crate) fn is_runs(self) -> bool {
        self.size as usize >= NB
    }
}

#[inline]
pub(crate) fn bucket_slot(b: u8) -> u8 {
    b
}

#[inline]
pub(crate) fn runs_slot(k: u32) -> u8 {
    (NB as u32 + k - 1) as u8
}

#[inline]
pub(crate) fn slot_bytes(size: u8) -> usize {
    if (size as usize) < NB {
        run::bucket_bytes(size)
    } else {
        (size as usize - NB + 1) * RUN
    }
}

#[repr(align(64))]
struct Head {
    stack: Stack,
    bytes: AtomicUsize,
    /// Runs with parked blocks (run index + 1; 0 = none), under the class lock.
    parked: AtomicU32,
    /// Pops so far (wrapping; a racy count is enough: the purger asks only whether it moved).
    pops: AtomicU32,
}

static HEADS: [Head; KEYS] =
    [const { Head { stack: Stack::new(), bytes: AtomicUsize::new(0), parked: AtomicU32::new(0), pops: AtomicU32::new(0) } }; KEYS];

pub(crate) fn chain(addr: usize, s: usize, n: u32, zero: bool) -> Batch {
    for j in 0..n as usize {
        let p = addr + j * s;
        let next = if j + 1 < n as usize { idx_of(p + s) } else { NONE };
        set_link_next(p, next);
    }
    Batch { head: idx_of(addr), n, bytes: s as u32, zero }
}

pub(crate) fn push(k: Key, b: Batch) {
    let h = &HEADS[k.index()];
    let l = link(addr_of_idx(b.head));
    l.n.store(b.n, Relaxed);
    l.zero.store(b.zero as u32, Relaxed);
    h.bytes.fetch_add(b.n as usize * b.bytes as usize, Relaxed);
    h.stack.push(&Pool, b.head);
}

pub(crate) fn pop(k: Key) -> Option<Batch> {
    let h = &HEADS[k.index()];
    let i = h.stack.pop(&Pool)?;
    h.pops.store(h.pops.load(Relaxed).wrapping_add(1), Relaxed);
    let l = link(addr_of_idx(i));
    let b = Batch { head: i, n: l.n.load(Relaxed), bytes: slot_bytes(k.size) as u32, zero: l.zero.load(Relaxed) != 0 };
    h.bytes.fetch_sub(b.n as usize * b.bytes as usize, Relaxed);
    Some(b)
}

/// Every batch on `k`'s list, emptied; call `f` on each.
pub(crate) fn take_all(k: Key, mut f: impl FnMut(Batch)) {
    let h = &HEADS[k.index()];
    let s = slot_bytes(k.size) as u32;
    let mut cur = h.stack.take_all().unwrap_or(NONE);
    while cur != NONE {
        let l = link(addr_of_idx(cur));
        let (next, n) = (l.next_batch.load(Relaxed), l.n.load(Relaxed));
        h.bytes.fetch_sub(n as usize * s as usize, Relaxed);
        f(Batch { head: cur, n, bytes: s, zero: false });
        cur = next;
    }
}

#[inline]
pub(crate) fn bytes(k: Key) -> usize {
    HEADS[k.index()].bytes.load(Relaxed)
}

pub(crate) fn total() -> usize {
    HEADS.iter().map(|h| h.bytes.load(Relaxed)).sum()
}

#[inline]
pub(crate) fn pops(k: Key) -> u32 {
    HEADS[k.index()].pops.load(Relaxed)
}

#[inline]
pub(crate) fn parked(k: Key) -> &'static AtomicU32 {
    &HEADS[k.index()].parked
}

pub(crate) fn keys() -> impl Iterator<Item = Key> {
    (0..KEYS).map(Key::from_index)
}

/// Push a batch; past the key's cap (4 shelves' worth) the top batch goes home to its runs.
pub(crate) fn push_capped(k: Key, b: Batch) {
    push(k, b);
    if bytes(k) > 4 * zones().cfg.shelf_max {
        if let Some(top) = pop(k) {
            home(k, top);
        }
    }
}

/// A batch back to its carved runs (blocks) or its superpages (runs).
pub(crate) fn home(k: Key, b: Batch) {
    if !k.is_runs() {
        run::home(k, b.head, b.n);
        return;
    }
    let runs = (k.size as usize - NB + 1) as u32;
    let mut cur = b.head;
    for _ in 0..b.n {
        let p = addr_of_idx(cur);
        cur = link_next(p);
        run::free_run(p, runs, k.class);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Arr(Vec<AtomicU32>);
    impl Nodes for Arr {
        fn next_batch(&self, i: u32) -> u32 {
            self.0[i as usize].load(Relaxed)
        }
        fn set_next_batch(&self, i: u32, v: u32) {
            self.0[i as usize].store(v, Relaxed)
        }
    }

    #[test]
    fn stack_is_lifo_and_take_all_chains() {
        let nodes = Arr((0..8).map(|_| AtomicU32::new(0)).collect());
        let s = Stack::new();
        assert_eq!(s.pop(&nodes), None);
        for i in 0..8 {
            s.push(&nodes, i);
        }
        assert_eq!(s.pop(&nodes), Some(7));
        assert_eq!(s.pop(&nodes), Some(6));
        let top = s.take_all().unwrap();
        assert_eq!(s.pop(&nodes), None);
        let mut seen = vec![];
        let mut c = top;
        while c != NONE {
            seen.push(c);
            c = nodes.next_batch(c);
        }
        assert_eq!(seen, vec![5, 4, 3, 2, 1, 0]);
    }

    #[test]
    fn stack_survives_a_threaded_storm() {
        let total = if cfg!(miri) { 64 } else { 4096 };
        let nodes = std::sync::Arc::new(Arr((0..total).map(|_| AtomicU32::new(0)).collect()));
        let s = std::sync::Arc::new(Stack::new());
        for i in 0..total {
            s.push(&*nodes, i);
        }
        let threads = if cfg!(miri) { 2 } else { 8 };
        let rounds = if cfg!(miri) { 50 } else { 20_000 };
        let hs: Vec<_> = (0..threads)
            .map(|_| {
                let (nodes, s) = (nodes.clone(), s.clone());
                std::thread::spawn(move || {
                    for _ in 0..rounds {
                        if let Some(i) = s.pop(&*nodes) {
                            s.push(&*nodes, i);
                        }
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        let mut seen = vec![false; total as usize];
        while let Some(i) = s.pop(&*nodes) {
            assert!(!seen[i as usize], "{i} twice");
            seen[i as usize] = true;
        }
        assert!(seen.iter().all(|&b| b));
    }

    #[test]
    fn slots_and_keys_round_trip() {
        for i in 0..KEYS {
            assert_eq!(Key::from_index(i).index(), i);
        }
        assert_eq!(slot_bytes(runs_slot(1)), RUN);
        assert_eq!(slot_bytes(runs_slot(LIST_RUNS)), LIST_RUNS as usize * RUN);
        assert_eq!(slot_bytes(bucket_slot(0)), 1024);
    }
}
