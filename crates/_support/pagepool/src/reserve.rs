//! The one reservation and its zones: pool (chunks, each followed by a 2 MiB guard), lazy, frame, then the meta tables.

use crate::os;
use crate::run::RUN;
use crate::superpage::SUPER;
use crate::Config;
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, Ordering::*};


/// The central lists' block index grain (u32 offsets: a pool zone of 2 TiB at most).
pub(crate) const GRAIN_SHIFT: u32 = 9;
const MAX_POOL: usize = if usize::BITS >= 64 { 1 << (32 + GRAIN_SHIFT) as u64 } else { usize::MAX };
const TABLE_ALIGN: usize = 64 << 10;
/// The reservation's and the frame zone's alignment.
const ALIGN: usize = if cfg!(miri) { SUPER } else { 1 << 30 };

#[derive(Clone, Copy, Debug)]
pub(crate) struct Zone {
    pub lo: usize,
    pub len: usize,
}

impl Zone {
    #[inline]
    pub(crate) fn has(&self, a: usize) -> bool {
        a.wrapping_sub(self.lo) < self.len
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Zones {
    pub all: Zone,
    pub pool: Zone,
    pub lazy: Zone,
    pub frame: Zone,
    pub chunk: usize,
    pub stride: usize,
    pub chunks: u32,
    pub nsuper: u32,
    pub supers: usize,
    pub runs: usize,
    pub tags: usize,
    /// One byte per 64 KiB run of the pool and lazy zones.
    pub dead: usize,
    pub bits: [usize; 3],
    pub words: usize,
    pub chunk_locks: usize,
    pub lazy_slots: usize,
    pub nlazy: u32,
    /// One tag byte per 256 KiB of the frame zone (frames' table grain).
    pub frame_tags: usize,
    /// One u32 per 2 MiB of the frame zone: the pool superpage (+ 1) a moved piece came from.
    pub frame_origin: usize,
    pub cfg: Config,
}

struct Holder(UnsafeCell<MaybeUninit<Zones>>);
// SAFETY: written once by `reserve` before READY is set (Release); read only after READY reads true (Acquire) or
// after `on()` in a thread the initializing thread happens-before.
unsafe impl Sync for Holder {}

static ZONES: Holder = Holder(UnsafeCell::new(MaybeUninit::uninit()));
static READY: AtomicBool = AtomicBool::new(false);

#[inline]
pub(crate) fn zones() -> &'static Zones {
    debug_assert!(READY.load(Acquire));
    // SAFETY: see Holder: callers run after a true `on()`, which `reserve` set after writing ZONES.
    unsafe { (*ZONES.0.get()).assume_init_ref() }
}

fn table(cursor: &mut usize, bytes: usize) -> usize {
    let at = *cursor;
    *cursor += bytes.next_multiple_of(TABLE_ALIGN);
    at
}

pub(crate) fn reserve(cfg: &Config) -> Option<&'static Zones> {
    let page = os::page_size();
    if page > RUN || cfg.chunk < SUPER || cfg.chunk % SUPER != 0 || cfg.va < cfg.chunk || cfg.purge_batch < SUPER {
        return None;
    }
    let chunks = cfg.va.div_ceil(cfg.chunk);
    let stride = cfg.chunk + SUPER;
    let pool_len = chunks.checked_mul(stride)?;
    if pool_len > MAX_POOL {
        return None;
    }
    let lazy_len = cfg.lazy_va.next_multiple_of(SUPER);
    let frame_len = cfg.frame_va.next_multiple_of(SUPER);
    let nsuper = pool_len / SUPER;
    let nlazy = lazy_len / SUPER;
    let words = nsuper.div_ceil(64);
    let mut m = 0usize;
    let supers = table(&mut m, nsuper * core::mem::size_of::<crate::superpage::SuperMeta>());
    let runs = table(&mut m, pool_len / RUN * core::mem::size_of::<crate::run::RunMeta>());
    let tags = table(&mut m, pool_len >> GRAIN_SHIFT);
    let dead = table(&mut m, (pool_len + lazy_len) / RUN);
    let bits = [table(&mut m, words * 8), table(&mut m, words * 8), table(&mut m, words * 8)];
    let chunk_locks = table(&mut m, chunks * 4);
    let lazy_slots = table(&mut m, nlazy * core::mem::size_of::<crate::span::LazySlot>());
    let frame_tags = table(&mut m, frame_len >> 18);
    let frame_origin = table(&mut m, frame_len / SUPER * 4);
    let meta_len = m.next_multiple_of(SUPER);
    let frame_off = (pool_len + lazy_len).next_multiple_of(ALIGN);
    let total = frame_off.checked_add(frame_len)?.checked_add(meta_len)?;
    let (chunks32, nsuper32, nlazy32) = (u32::try_from(chunks).ok()?, u32::try_from(nsuper).ok()?, u32::try_from(nlazy).ok()?);
    // SAFETY: a fresh reservation, ours for the process's life.
    let lo = unsafe { os::reserve(total, ALIGN) }?;
    let meta = Zone { lo: lo + frame_off + frame_len, len: meta_len };
    // SAFETY: inside our fresh reservation, nothing live there.
    if !unsafe { os::open(meta.lo, meta.len, false, c"pgrust:pool-meta") } {
        return None;
    }
    for c in 0..chunks {
        // SAFETY: as above; the guard after each chunk stays PROT_NONE.
        if !unsafe { os::open(lo + c * stride, cfg.chunk, true, c"pgrust:pool") } {
            return None;
        }
    }
    let rel = |off: usize| meta.lo + off;
    let z = Zones {
        all: Zone { lo, len: total },
        pool: Zone { lo, len: pool_len },
        lazy: Zone { lo: lo + pool_len, len: lazy_len },
        frame: Zone { lo: lo + frame_off, len: frame_len },
        chunk: cfg.chunk,
        stride,
        chunks: chunks32,
        nsuper: nsuper32,
        supers: rel(supers),
        runs: rel(runs),
        tags: rel(tags),
        dead: rel(dead),
        bits: [rel(bits[0]), rel(bits[1]), rel(bits[2])],
        words,
        chunk_locks: rel(chunk_locks),
        lazy_slots: rel(lazy_slots),
        nlazy: nlazy32,
        frame_tags: rel(frame_tags),
        frame_origin: rel(frame_origin),
        cfg: *cfg,
    };
    // SAFETY: the only write, before READY.
    unsafe { (*ZONES.0.get()).write(z) };
    READY.store(true, Release);
    Some(zones())
}

#[inline]
pub(crate) fn in_pool(addr: usize) -> bool {
    let z = zones();
    z.pool.has(addr) && (addr - z.pool.lo) % z.stride < z.chunk
}
