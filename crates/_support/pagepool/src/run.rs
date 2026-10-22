//! The run table (64 KiB grain over the pool zone), the bucket set, carving blocks out of runs, and lookup by pointer.

use crate::central::{self, Batch, Key};
use crate::reserve::{zones, GRAIN_SHIFT};
use crate::stats;
use crate::superpage::{self, State, SUPER};
use crate::{state, Class, Hint, Tag};
use crate::sync::{AtomicU64, AtomicU8, Ordering::*};

pub(crate) const RUN: usize = 64 << 10;
/// Four per doubling (mimalloc's bins). With `PGRUST_POOL_FINE_CLASSES` off only the even ones are taken: two per
/// doubling, 1 KiB / 1.5 KiB / 2 KiB / 3 KiB .., the sizes as before.
pub(crate) const BUCKETS: [u32; 29] = [
    1024, 1280, 1536, 1792, 2048, 2560, 3072, 3584, 4096, 5120, 6144, 7168, 8192, 10240, 12288, 14336, 16384, 20480, 24576,
    28672, 32768, 40960, 49152, 57344, 65536, 81920, 98304, 114688, 131072,
];
pub(crate) const NB: usize = BUCKETS.len();
/// The 64 KiB bucket (the small tier's runs).
pub(crate) const RUN_BUCKET: u8 = 24;
const _: () = assert!(BUCKETS[RUN_BUCKET as usize] as usize == RUN);

/// D4760 (pppress): `PGRUST_POOL_FINE_CLASSES=1`, read once (0: unread, 1: off, 2: on).
static FINE_SWITCH: AtomicU8 = AtomicU8::new(0);

/// D4760 (pppress): `PGRUST_POOL_FINE_CLASSES=1`, read ONCE (at the first size lookup, before the pool's first block:
/// `pagepool::init` reads it too): blocks of 1-128 KiB take the least of four buckets per doubling instead of two, so a
/// block's rounding is at most a quarter of its size class's step (1025 B: 1280 B, not 1536 B), as mimalloc's.
#[inline]
pub(crate) fn fine_on() -> bool {
    match FINE_SWITCH.load(Relaxed) {
        0 => {
            let v = if crate::os::env_is_one(c"PGRUST_POOL_FINE_CLASSES") { 2 } else { 1 };
            FINE_SWITCH.store(v, Relaxed);
            v == 2
        }
        v => v == 2,
    }
}

pub(crate) fn force_fine(on: bool) {
    FINE_SWITCH.store(if on { 2 } else { 1 }, Relaxed);
}

/// The least block alignment: blocks are named by 512 B grains (the central lists' indices, the tag bytes), so a bucket
/// whose size is no multiple of it (1280, 1792) is never taken, even with `PGRUST_POOL_FINE_CLASSES`.
const GRAIN: usize = 1 << GRAIN_SHIFT;

/// Whether bucket `b` is taken with the switch as it is.
#[inline]
pub(crate) fn selectable(b: u8) -> bool {
    (b % 2 == 0 || fine_on()) && natural_align(b) >= GRAIN
}
/// The runs road's longest take; past it, spans.
pub(crate) const MAX_RUNS: u32 = 31;
/// Runs this long and shorter sit on shelves and the central lists; longer go straight back to their superpage.
pub(crate) const LIST_RUNS: u32 = 8;

const KIND_FREE: u64 = 0;
const KIND_BLOCKS: u64 = 1;
const KIND_RUN: u64 = 2;
const KIND_TAIL: u64 = 4;

/// word: kind:3 | bucket:5 | runs:6 | class:3 | tag:8 | listed:1 | used:16 (32..48) | back:16 (48..64).
/// aux (a block run's head): parked head offset + 1 :16 | parked count :16 | next listed run :32.
#[repr(C, align(16))]
pub struct RunMeta {
    word: AtomicU64,
    aux: AtomicU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Free,
    Blocks { bucket: u8 },
    Run { runs: u8 },
    Span { superpages: u32 },
    Dead,
    Small,
}

#[derive(Clone, Copy, Debug)]
pub struct RunInfo {
    pub kind: Kind,
    pub base: usize,
    pub bytes: usize,
    pub class: Class,
    pub tag: Tag,
}

#[inline]
pub(crate) fn rmeta(r: usize) -> &'static RunMeta {
    // SAFETY: the table holds one entry per 64 KiB of the pool zone, in the meta zone, open for the process's life.
    unsafe { &*(zones().runs as *const RunMeta).add(r) }
}

#[inline]
pub(crate) fn run_index(addr: usize) -> usize {
    (addr - zones().pool.lo) / RUN
}

#[inline]
pub(crate) fn run_addr(r: usize) -> usize {
    zones().pool.lo + r * RUN
}

#[inline]
fn kind(w: u64) -> u64 {
    w & 7
}
#[inline]
fn w_bucket(w: u64) -> u8 {
    ((w >> 3) & 31) as u8
}
#[inline]
fn w_runs(w: u64) -> u32 {
    ((w >> 8) & 63) as u32
}
#[inline]
fn w_class(w: u64) -> Class {
    Class::from_u8(((w >> 14) & 7) as u8)
}
#[inline]
fn w_tag(w: u64) -> Tag {
    Tag(((w >> 17) & 0xff) as u8)
}
const LISTED: u64 = 1 << 25;
#[inline]
fn w_used(w: u64) -> u32 {
    ((w >> 32) & 0xffff) as u32
}
#[inline]
fn w_back(w: u64) -> u32 {
    (w >> 48) as u32
}
#[inline]
fn word(kind: u64, bucket: u8, runs: u32, class: Class, tag: Tag, used: u32) -> u64 {
    kind | (bucket as u64) << 3 | (runs as u64 & 63) << 8 | (class as u64) << 14 | (tag.0 as u64) << 17 | (used as u64) << 32
}

#[inline]
pub(crate) fn bucket_bytes(b: u8) -> usize {
    BUCKETS[b as usize] as usize
}

/// A bucket's blocks are aligned to the largest power of two dividing its size.
#[inline]
pub(crate) fn natural_align(b: u8) -> usize {
    1 << BUCKETS[b as usize].trailing_zeros()
}

/// A bucket's run length: the least k with a carve tail of at most 1/16.
pub(crate) const fn carve_runs(b: u8) -> u32 {
    let s = BUCKETS[b as usize] as usize;
    let mut k = 1;
    loop {
        let span = k * RUN;
        if span >= s && span % s <= span / 16 {
            return k as u32;
        }
        k += 1;
    }
}

/// A bucket's carved runs start aligned to this many runs (natural alignment past 64 KiB).
#[inline]
fn carve_align(b: u8) -> u32 {
    (natural_align(b) / RUN).max(1) as u32
}

#[inline]
pub(crate) fn blocks_per(b: u8) -> u32 {
    (carve_runs(b) as usize * RUN / bucket_bytes(b)) as u32
}

/// The least bucket holding `(i + 1) * 256` bytes (every bucket is a multiple of 256).
const BUCKET_AT: [u8; 512] = {
    let mut t = [0u8; 512];
    let (mut i, mut b) = (0, 0);
    while i < 512 {
        while (BUCKETS[b] as usize) < (i + 1) * 256 {
            b += 1;
        }
        t[i] = b as u8;
        i += 1;
    }
    t
};

#[inline]
pub(crate) fn bucket_of(bytes: usize, align: usize) -> Option<u8> {
    if bytes > BUCKETS[NB - 1] as usize || bytes == 0 {
        return None;
    }
    let mut b = BUCKET_AT[(bytes - 1) >> 8] as usize;
    let step = if fine_on() {
        1
    } else {
        b += b & 1;
        2
    };
    let align = align.max(GRAIN);
    while b < NB {
        if natural_align(b as u8) >= align {
            return Some(b as u8);
        }
        b += step;
    }
    None
}

#[inline]
pub(crate) fn tag_slot(p: usize) -> &'static AtomicU8 {
    // SAFETY: one byte per 512 B of the pool zone, in the meta zone, open for the process's life.
    unsafe { &*((zones().tags + ((p - zones().pool.lo) >> GRAIN_SHIFT)) as *const AtomicU8) }
}

#[inline]
fn head(r: usize) -> (usize, u64) {
    let w = rmeta(r).word.load(Acquire);
    if kind(w) == KIND_TAIL {
        let h = r - w_back(w) as usize;
        (h, rmeta(h).word.load(Acquire))
    } else {
        (r, w)
    }
}

/// What holds `p`: one superpage-table load, then one run-table load for runs and blocks. A dead run reads `Dead`.
pub fn run_of(p: *const u8) -> Option<RunInfo> {
    let i = lookup(p)?;
    if matches!(i.kind, Kind::Blocks { .. } | Kind::Run { .. }) && crate::api::dead_at(p as usize) {
        let a = p as usize & !(RUN - 1);
        return Some(RunInfo { kind: Kind::Dead, base: a, bytes: RUN, ..i });
    }
    Some(i)
}

/// As `run_of`, the allocation's own kind even where a run of it is dead.
pub(crate) fn lookup(p: *const u8) -> Option<RunInfo> {
    if !crate::on() {
        return None;
    }
    let a = p as usize;
    let z = zones();
    if z.lazy.has(a) {
        return crate::span::lazy_info(a);
    }
    if !crate::reserve::in_pool(a) {
        return None;
    }
    let i = superpage::index_of(a);
    let m = superpage::meta(i);
    match m.state() {
        State::Span => {
            let h = m.head();
            let hm = superpage::meta(h);
            let n = hm.span_n.load(Relaxed);
            return Some(RunInfo {
                kind: Kind::Span { superpages: n },
                base: superpage::addr_of(h),
                bytes: n as usize * SUPER,
                class: hm.class(),
                tag: Tag(hm.tag()),
            });
        }
        State::Partial | State::Full => {}
        _ => return Some(RunInfo { kind: Kind::Free, base: superpage::addr_of(i), bytes: SUPER, class: m.class(), tag: Tag::OTHER }),
    }
    let (h, w) = head(run_index(a));
    let rb = run_addr(h);
    let (class, tag) = (w_class(w), w_tag(w));
    Some(match kind(w) {
        KIND_BLOCKS => {
            let s = bucket_bytes(w_bucket(w));
            RunInfo { kind: Kind::Blocks { bucket: w_bucket(w) }, base: rb + (a - rb) / s * s, bytes: s, class, tag: Tag(tag_slot(a).load(Relaxed)) }
        }
        KIND_RUN => RunInfo { kind: Kind::Run { runs: w_runs(w) as u8 }, base: rb, bytes: w_runs(w) as usize * RUN, class, tag },
        _ => RunInfo { kind: Kind::Free, base: run_addr(run_index(a)), bytes: RUN, class, tag },
    })
}

/// pppress card V: the present sweep's read of run `r` (a run of a Partial / Full superpage): whether it holds no live
/// block (a free run; a blocks run whose blocks are all home, `used` 0), and its allocation's tag (the carve's for a
/// blocks run). One or two relaxed loads; a run changing under the read is read either way.
#[cfg_attr(not(all(target_os = "linux", not(miri))), allow(dead_code))]
#[inline]
pub(crate) fn sweep_kind(r: usize) -> (bool, Tag) {
    let (_, w) = head(r);
    match kind(w) {
        KIND_BLOCKS => (w_used(w) == 0, w_tag(w)),
        KIND_RUN => (false, w_tag(w)),
        _ => (true, Tag::OTHER),
    }
}

fn mark(addr: usize, k: u32, head_word: u64) {
    let r = run_index(addr);
    for j in 1..k as usize {
        rmeta(r + j).word.store(KIND_TAIL | (j as u64) << 48, Relaxed);
    }
    rmeta(r).aux.store(0, Relaxed);
    rmeta(r).word.store(head_word, Release);
}

fn unmark(addr: usize, k: u32) {
    let r = run_index(addr);
    for j in 0..k as usize {
        rmeta(r + j).word.store(KIND_FREE, Relaxed);
    }
}

/// Take `k` runs for a runs-road allocation (class lock taken here). Its address and whether its bytes are zero.
pub(crate) fn take_run(k: u32, align_runs: u32, hint: Hint) -> Option<(usize, bool)> {
    let g = state::lock(hint.class);
    let got = state::take_runs_locked(&g, k, align_runs, hint.zero);
    if let Some((addr, _)) = got {
        mark(addr, k, word(KIND_RUN, 0, k, hint.class, hint.tag, 0));
    }
    got
}

pub(crate) fn free_run(addr: usize, k: u32, class: Class) {
    let g = state::lock(class);
    unmark(addr, k);
    state::free_runs_locked(&g, addr, k, 0);
}

#[inline]
pub(crate) fn run_class_tag(addr: usize) -> (Class, Tag) {
    let (_, w) = head(run_index(addr));
    (w_class(w), w_tag(w))
}

/// Retag a runs-road allocation's head (a take from a shelf or the central list under another tag).
#[inline]
pub(crate) fn set_run_tag(addr: usize, tag: Tag) {
    let r = run_index(addr);
    let m = rmeta(r);
    let w = m.word.load(Relaxed);
    if w_tag(w) != tag {
        m.word.store((w & !(0xff << 17)) | (tag.0 as u64) << 17, Release);
    }
}

/// Grow a runs-road allocation in place by `more` runs (the class lock taken here).
pub(crate) fn grow_run(addr: usize, k: u32, more: u32) -> bool {
    let (class, tag) = run_class_tag(addr);
    let g = state::lock(class);
    if !state::take_runs_at_locked(&g, addr + k as usize * RUN, more) {
        return false;
    }
    mark(addr, k + more, word(KIND_RUN, 0, k + more, class, tag, 0));
    true
}

pub(crate) fn shrink_run(addr: usize, k: u32, keep: u32) {
    let (class, tag) = run_class_tag(addr);
    let g = state::lock(class);
    let tail = addr + keep as usize * RUN;
    unmark(tail, k - keep);
    mark(addr, keep, word(KIND_RUN, 0, keep, class, tag, 0));
    state::free_runs_locked(&g, tail, k - keep, 0);
}

/// One batch of `bucket` blocks for `hint.class`: a run with parked blocks first, else freshly carved runs.
pub(crate) fn carve(bucket: u8, hint: Hint) -> Option<Batch> {
    let key = Key::new(hint.class, central::bucket_slot(bucket));
    let g = state::lock(hint.class);
    if let Some(b) = unpark_locked(key) {
        return Some(b);
    }
    let k = carve_runs(bucket);
    let (addr, zero) = state::take_runs_locked(&g, k, carve_align(bucket), hint.zero)?;
    let s = bucket_bytes(bucket);
    let n = blocks_per(bucket);
    mark(addr, k, word(KIND_BLOCKS, bucket, k, hint.class, hint.tag, n));
    stats::WASTE.fetch_add((k as usize * RUN - n as usize * s) as isize, Relaxed);
    drop(g);
    Some(central::chain(addr, s, n, zero))
}

/// Pop a run with parked blocks off `key`'s list and take them all (class lock held).
fn unpark_locked(key: Key) -> Option<Batch> {
    let list = central::parked(key);
    loop {
        let r1 = list.load(Relaxed);
        if r1 == 0 {
            return None;
        }
        let r = r1 as usize - 1;
        let m = rmeta(r);
        let aux = m.aux.load(Relaxed);
        list.store((aux >> 32) as u32, Relaxed);
        let w = m.word.load(Relaxed);
        let off1 = (aux & 0xffff) as usize;
        let n = ((aux >> 16) & 0xffff) as u32;
        m.aux.store(0, Relaxed);
        m.word.store((w & !LISTED & !(0xffff << 32)) | ((w_used(w) + n) as u64) << 32, Release);
        if n == 0 || off1 == 0 {
            continue;
        }
        let s = bucket_bytes(w_bucket(w));
        stats::STRANDED.fetch_sub((n as usize * s) as isize, Relaxed);
        let head = run_addr(r) + ((off1 - 1) << GRAIN_SHIFT);
        return Some(Batch { head: central::idx_of(head), n, bytes: s as u32, zero: false });
    }
}

/// Return a chain of `n` blocks of `key`'s bucket to their carved runs (class lock taken here). A run whose blocks are
/// all home is freed at once unless it sits on the key's list (then the purger's sweep or the next carve takes it).
pub(crate) fn home(key: Key, head: u32, n: u32) {
    let s = central::slot_bytes(key.size);
    let g = state::lock(key.class);
    let mut cur = head;
    for _ in 0..n {
        let p = central::addr_of_idx(cur);
        let next = central::link_next(p);
        let (r, w) = self::head(run_index(p));
        let m = rmeta(r);
        let aux = m.aux.load(Relaxed);
        let off1 = (aux & 0xffff) as u32;
        let cnt = ((aux >> 16) & 0xffff) as u32;
        let prev = if off1 == 0 { central::NONE } else { central::idx_of(run_addr(r) + ((off1 as usize - 1) << GRAIN_SHIFT)) };
        central::set_link_next(p, prev);
        let my_off1 = (((p - run_addr(r)) >> GRAIN_SHIFT) + 1) as u64;
        let used = w_used(w) - 1;
        let mut nw = (w & !(0xffff << 32)) | (used as u64) << 32;
        let mut naux = (aux & !0xffff_ffff) | my_off1 | ((cnt + 1) as u64) << 16;
        stats::STRANDED.fetch_add(s as isize, Relaxed);
        if used == 0 && w & LISTED == 0 {
            free_blocks_run_locked(&g, r, nw, cnt + 1);
        } else {
            if w & LISTED == 0 {
                let list = central::parked(key);
                naux = (naux & 0xffff_ffff) | (list.load(Relaxed) as u64) << 32;
                list.store(r as u32 + 1, Relaxed);
                nw |= LISTED;
            }
            m.aux.store(naux, Relaxed);
            m.word.store(nw, Release);
        }
        cur = next;
    }
}

/// A block run with every block home: give its runs back to the superpage (class lock held).
fn free_blocks_run_locked(g: &state::Guard, r: usize, w: u64, parked: u32) {
    let k = w_runs(w);
    let s = bucket_bytes(w_bucket(w));
    let n = blocks_per(w_bucket(w));
    debug_assert_eq!(parked, n);
    stats::STRANDED.fetch_sub((parked as usize * s) as isize, Relaxed);
    stats::WASTE.fetch_sub((k as usize * RUN - n as usize * s) as isize, Relaxed);
    let addr = run_addr(r);
    rmeta(r).aux.store(0, Relaxed);
    unmark(addr, k);
    state::free_runs_locked(g, addr, k, 0);
}

/// The purger's sweep: free every listed run of `key` whose blocks are all home.
pub(crate) fn sweep(key: Key) -> usize {
    let list = central::parked(key);
    if list.load(Relaxed) == 0 {
        return 0;
    }
    let g = state::lock(key.class);
    let mut freed = 0;
    let mut prev: Option<usize> = None;
    let mut cur = list.load(Relaxed);
    while cur != 0 {
        let r = cur as usize - 1;
        let m = rmeta(r);
        let aux = m.aux.load(Relaxed);
        let next = (aux >> 32) as u32;
        let w = m.word.load(Relaxed);
        if w_used(w) == 0 {
            match prev {
                None => list.store(next, Relaxed),
                Some(p) => {
                    let pa = rmeta(p).aux.load(Relaxed);
                    rmeta(p).aux.store((pa & 0xffff_ffff) | (next as u64) << 32, Relaxed);
                }
            }
            free_blocks_run_locked(&g, r, w & !LISTED, ((aux >> 16) & 0xffff) as u32);
            freed += 1;
        } else {
            prev = Some(r);
        }
        cur = next;
    }
    freed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_carve_tail_is_at_most_a_sixteenth() {
        for b in 0..NB as u8 {
            let k = carve_runs(b) as usize;
            let span = k * RUN;
            assert!(span % bucket_bytes(b) <= span / 16, "bucket {}", bucket_bytes(b));
            assert!(k as u32 <= LIST_RUNS);
        }
    }

    #[test]
    fn buckets_route_by_size_and_alignment() {
        // the switch is off in this test binary (no test turns it on): two per doubling, as before
        assert!(!fine_on());
        let size = |b: Option<u8>| b.map(bucket_bytes);
        assert_eq!(size(bucket_of(1, 16)), Some(1024));
        assert_eq!(size(bucket_of(1024, 16)), Some(1024));
        assert_eq!(size(bucket_of(1025, 16)), Some(1536));
        assert_eq!(size(bucket_of(1025, 1024)), Some(2048));
        assert_eq!(size(bucket_of(2049, 16)), Some(3072));
        assert_eq!(size(bucket_of(8192, 8192)), Some(8192));
        assert_eq!(size(bucket_of(65537, 16)), Some(98304));
        assert_eq!(size(bucket_of(131072, 131072)), Some(131072));
        assert_eq!(bucket_of(131073, 16), None);
        assert_eq!(bucket_of(4096, 1 << 18), None);
        for b in 0..NB as u8 {
            assert!(natural_align(b) >= 256 && bucket_bytes(b) % natural_align(b) == 0);
            assert!(carve_align(b) as usize * RUN >= natural_align(b).min(RUN));
            assert_eq!(selectable(b), b % 2 == 0, "off: the old sizes only");
            // the old sizes are all whole grains: the grain floor changes no route with the switch off
            assert!(b % 2 != 0 || natural_align(b) >= GRAIN);
        }
    }

    #[test]
    fn fine_buckets_are_four_per_doubling() {
        for w in BUCKETS.windows(2) {
            let pow = 1u32 << (31 - w[0].leading_zeros());
            assert_eq!(w[1] - w[0], pow / 4, "{} after {}", w[1], w[0]);
        }
        for b in 0..NB as u8 {
            assert!(blocks_per(b) >= 1 && blocks_per(b) <= 64);
        }
        assert_eq!(BUCKETS.iter().step_by(2).copied().collect::<Vec<_>>(), [1024, 1536, 2048, 3072, 4096, 6144, 8192, 12288, 16384, 24576, 32768, 49152, 65536, 98304, 131072]);
    }

    #[test]
    fn words_round_trip() {
        let w = word(KIND_BLOCKS, 14, 3, Class::Frame, Tag(255), 0xffff) | LISTED;
        assert_eq!((kind(w), w_bucket(w), w_runs(w), w_class(w), w_tag(w), w_used(w)), (KIND_BLOCKS, 14, 3, Class::Frame, Tag(255), 0xffff));
        assert_eq!(w_back(KIND_TAIL | 31 << 48), 31);
    }
}
