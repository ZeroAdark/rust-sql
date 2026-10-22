//! A breaker region's buffer and how one is drawn (the grant pool, the depot, the arena's chunks): `RegionBuf`,
//! `uninit_region`, `chunk_region` and the region debug knobs (split out of `node/mod.rs`, contract-runtime landing 3).
#![allow(unsafe_code)]

use gravity_abi::PtrWord;
use std::ptr::NonNull;

use mempolicy::admit::{Asker, Road};

use crate::{Error, Result};

/// A breaker region's buffer and what holds its bytes (refactor-ledger.md §5): a statement's life or grant context while one
/// uses it, the grant pool while parked (`settle`); only `uninit_region` / `zeroed_region` make a non-empty one.
/// D1704: one allocation of `len` bytes at `REGION_ALIGN` (freed with that `Layout`), never a `Vec<u8>`: a `Vec<u8>` asks
/// alignment 1, and the tables a region holds are viewed as 8-aligned (`HashAgg::at`'s `&mut HashAgg`). `len()` and
/// `as_mut_ptr()` read the fields (no slice reborrow of the bytes a kernel's table pointer writes through).
pub struct RegionBuf {
    /// C8 (area move 3): the region's bytes as its owner's charge: the owner (the statement, portal or session that made
    /// it, the depot while parked, its taker's after), the account they are on, and their kind in the ledger's per-kind
    /// split (its breaker's, `ledger::with_kind` at the bind). Its committed bytes are a mapping's writable pages, or an
    /// allocation's bytes less the pages `fit_to` gave back (a pool allocation's with its `round`); its growth, give-backs
    /// and drop book them whichever thread runs them.
    charge: memledger::Charge,
    ptr: NonNull<u8>,
    len: usize,
    held: crate::mem::grant::Held,
    /// The setop posture (option c'): the allocation's length, past `len` for a reserved region (`reserved_region`: a
    /// `Budget::Pool` side's address space, its share `len` charged), else `len`.
    reserve: usize,
    /// Option c': the bytes committed past `len` in place (`commit`) and what holds them, released with the region.
    committed: usize,
    extra: Vec<crate::mem::grant::Held>,
    /// D2894: a reservation mapped as address space alone (`map_reserved`): the bytes from `ptr` that are readable and
    /// writable (whole pages covering `len + committed`), the rest PROT_NONE and MAP_NORESERVE, so an untouched reservation
    /// is no commit charge and no RSS; 0 for an allocator's region (`alloc`, or a reservation the mapping refused).
    rw: usize,
    /// D2894: the reservation is a breaker's PostgreSQL floor (`grow::floor_reserve`), not a set operation side's rows bound.
    floor: bool,
    /// The region started small (`grant::start_bytes`): its `len` is a start that grows in place, and its table is laid out
    /// at the reservation (the `RESERVE_LAYOUT` answer, `grow::region_commit`).
    laid: bool,
    /// The reservation a laid region's taker asked for (`laid_region`), at most `reserve`: its table's layout and partition
    /// count read it (`lay_reserve`), never the larger reservation a parked region was mapped with, so every worker of one
    /// section lays the same partitions whichever regions they took (the owner phase reads them all at one count).
    lay: usize,
    /// D3809: the share a laid region's taker opened it at (`laid_region`, refreshed at every take): its partitions are cut
    /// by it (the `RESERVE_LAYOUT` answer carries it, kernels `fold_bits`), as a share-committed region's are by its share.
    cut: usize,
    /// D3074 / D3089: an allocator region taken for a smaller ask (`fit_to`): the allocation's bytes past `len`, their pages
    /// given back (MADV_DONTNEED) and charged to nobody until a later take fits the region to its own ask.
    slack: usize,
    /// G1-A: of an allocator region's bytes (door 1 counted them all), the ones `fit_to` gave back to the kernel: off the
    /// account until a later fit takes them again.
    given: usize,
    /// D3872: its entry in the residency registry (`mem::resident`; `resident::NONE` = not listed): the residency pass
    /// moves its account's not-resident bytes to what it finds of its pages.
    res: u32,
    /// pagepool P3 (`PGRUST_POOL_GRAVITY=1`): the bytes are the page pool's (`pool_alloc`, `pool_reserved`), charged by its
    /// door (`Door::Pool`): pages given back go dead in the pool (`pool_dead`, the purger releases them), a growth takes
    /// them live first (`pool_live`), the drop gives the block back whole; no protection change, no huge-page advice.
    pool: bool,
    /// pagepool P3: a pool region parked with its pages dead (`park_free`), taken live at its `unparked`.
    dead: bool,
    /// pppress card 13 (3): a `pool_alloc` region's bytes past its ask, `reserve`, that the pool handed out with it
    /// (`pagepool::take`'s slice: the ask's bucket, its runs, a span's commit) and counts in use: on the region's charge
    /// from the take to the drop, so its account is the pool's in-use bytes for it. Never written, never in `len`,
    /// `whole` or `reserve`; 0 for any other region.
    round: usize,
}

/// D1704: every region's base alignment, the widest word a table lays (`u64`). The server's allocator gives 16 at any ask
/// of 16 or less (mimalloc, glibc), so a region's native allocation is the same call as before.
pub const REGION_ALIGN: usize = 8;

/// pagepool P3: a region's pool memory, statement-lived (`Tag::REGION`); zeroed only where the allocator's road zeroes.
const POOL_HINT: pagepool::Hint = pagepool::Hint::new(pagepool::Class::Task, pagepool::Tag::REGION);
/// pagepool P3: a pool region's base alignment (a cache line; `REGION_ALIGN` and more).
const POOL_ALIGN: usize = 64;
/// pagepool P3: the pool's dead-page grain (`pagepool::give_pages` takes whole runs of it).
const POOL_RUN: usize = 64 << 10;

/// pagepool P3: regions from the page pool (the server's switch, read by the pool, with the pool reserved).
#[inline]
pub(crate) fn pooled() -> bool {
    pagepool::on() && pagepool::switches().gravity
}

/// pagepool P3 (C.11): a depot trim under PRESSURE freed `bytes` of parked regions or slot sets, whose pool
/// memory went back to the pool warm (a region's drop gives its block back): the pool releases up to as many bytes of its
/// warm memory to the OS. Inline (`pagepool::purge_now`: its remaps) only on the memory runtime's loop thread
/// (`memledger::ladder_pass::on_ladder_thread`); on any other thread (a draw's or an admission's, the inline R2a) the
/// pool's purger is asked (`set_pressure`). Called only where the trim's caller is pressure (the ladder's and the
/// reclaimer's takes, the backstop, an admission the memory governor refused), never from an idle or a statement-end trim:
/// there the warm bytes are the next allocation's first. Switch off: nothing.
pub(crate) fn pool_release(bytes: u64) {
    if bytes == 0 || !pooled() {
        return;
    }
    let bytes = usize::try_from(bytes).unwrap_or(usize::MAX);
    if memledger::ladder_pass::on_ladder_thread() {
        pagepool::purge_now(bytes, Some(pagepool::Class::Task));
    } else {
        pagepool::set_pressure(pagepool::Pressure::Release { bytes, class: Some(pagepool::Class::Task) });
    }
}

// SAFETY: a charged hold's `Rc`s are touched only on the leader's thread (bind, settle, drop); a depot or Park region is settled first.
unsafe impl Send for RegionBuf {}

impl Default for RegionBuf {
    fn default() -> RegionBuf {
        RegionBuf::empty()
    }
}

/// G1-A: the memory ledger's kind of a breaker's region (no breaker: `Region`).
pub(crate) fn ledger_kind(b: Option<gravity_abi::breaker::BreakerKind>) -> memledger::Kind {
    use gravity_abi::breaker::BreakerKind as B;
    use memledger::Kind as K;
    match b {
        Some(B::HashBuild) => K::HashBuild,
        Some(B::HashAgg) => K::HashAgg,
        Some(B::PlainAgg) => K::PlainAgg,
        Some(B::Distinct) => K::Distinct,
        Some(B::Sort) => K::Sort,
        Some(B::TopK) => K::TopK,
        Some(B::Materialize) => K::Spool,
        None => K::Region,
    }
}

/// G1-B: the ledger kind of the regions pipeline `p` of `program` makes: its breaker's (`ledger_kind`), whether the region is
/// a slot's (opened under it, `Node::open_slot`) or drawn while its code runs (a call or a claim of it: a table's growth, an
/// arena chunk, a build's rows).
pub(crate) fn pipeline_kind(program: &gravity_program::Program, p: usize) -> memledger::Kind {
    ledger_kind(program.pipelines.get(p).and_then(|pipe| pipe.breaker()).map(|b| b.kind))
}

/// G1-B: the regions the calling thread makes count under pipeline `p`'s breaker kind until the guard drops: a pipeline's
/// whole run on the leader (its calls, its claims, its owner phase: `gravity_parallel::run_with_late`), and through the
/// session carry a pool claim of it (hosts `SessionCarry`).
pub fn pipeline_kind_scope(program: &gravity_program::Program, p: usize) -> memledger::KindScope {
    memledger::with_kind(pipeline_kind(program, p))
}

impl Drop for RegionBuf {
    fn drop(&mut self) {
        if self.len != 0 {
            unguard(self.ptr.as_ptr(), self.len);
            if self.pool {
                // pagepool P3: off its owner first, then the block back to the pool whole (warm; its dead runs live again
                // there); never the reaper's
                crate::mem::resident::unregister(self.res);
                drop(core::mem::replace(&mut self.charge, memledger::Charge::NONE));
                // SAFETY: the pool block or span `pool_alloc` / `pool_reserved` took, given back once by its only owner
                unsafe { pagepool::give(self.ptr, self.reserve) };
                return;
            }
            if self.rw != 0 {
                // G1-A door 2 / C8: the mapping's writable pages go with it: to the reaper's holder (`reap::defer`), which
                // gives them back after its unmap, else off its account here before the unmap
                let charge = core::mem::replace(&mut self.charge, memledger::Charge::NONE);
                // D2894: a mapped reservation goes back whole (its touched pages with it): a large one through the reaper
                // (`reap::defer`: unmapped off this thread, its pool charge released after; D3872: its residency entry
                // moves to REAPING with its last numbers), else here
                if !crate::mem::reap::defer(self.ptr.as_ptr(), self.reserve, self.rw, &mut self.held, self.res, charge) {
                    crate::mem::resident::unregister(self.res);
                    unmap(self.ptr.as_ptr(), self.reserve);
                }
                return;
            }
            // D3872: off the residency registry before the allocator takes the block back
            crate::mem::resident::unregister(self.res);
            if self.is_reserved() {
                // option c': a reservation's touched pages (its share and what its fold committed) go back to the kernel
                // before the allocator takes the range: a range under the allocator's mmap threshold returns to an arena,
                // which keeps it resident, and each statement's reservations would hold every page their folds touched
                dontneed(self.ptr.as_ptr(), self.len + self.committed);
            }
            // G1-A / C8: door 1 takes the allocation's `reserve` bytes off the dropping thread's account: the charge's drop
            // moves them (its given-back pages with them) from the region's account to the dropping thread's, so both net
            // to zero
            drop(core::mem::replace(&mut self.charge, memledger::Charge::NONE));
            // SAFETY: `ptr` is a live allocation of exactly this layout (`RegionBuf::alloc`, `reserve` bytes); the held charges drop after it.
            unsafe { std::alloc::dealloc(self.ptr.as_ptr(), core::alloc::Layout::from_size_align_unchecked(self.reserve, REGION_ALIGN)) };
        }
    }
}

/// H3 step (4) (`mem::giveback`, give_back_spill): an allocator region given back whole, taken apart for a give-back job
/// (`RegionBuf::give_parts`): its block, the bytes of it whose pages go back, its charge (on `Cache(Reaping)` from the
/// enqueue), its residency entry and the grant pool's part of its hold. `give` is its drop's allocator path, run on the
/// thread that takes the job.
pub(crate) struct GiveParts {
    at: NonNull<u8>,
    reserve: usize,
    give: usize,
    charge: memledger::Charge,
    res: u32,
    pool: u64,
    kept: bool,
    /// Take-back: the region's shape as it left (`take_back` makes it again): a plain allocator region's `len`, its
    /// uncharged tail and the tail's given-back bytes (`fit_to`); a reserved one is never taken back.
    plain: bool,
    len: usize,
    slack: usize,
    given: usize,
}

// SAFETY: the block is the job's alone (its region is gone: `give_parts` left it empty); the charge is a ledger charge
// (no `Rc`), and the hold's statement-bound parts stayed with the region (`Held::take_pool`), as for the reaper's jobs.
unsafe impl Send for GiveParts {}

impl GiveParts {
    /// The bytes on its REAPING charge: the job lane's landing for it.
    pub(crate) fn bytes(&self) -> usize {
        self.charge.committed() as usize
    }

    /// TB4: the grant pool's bytes it holds, released after its give.
    pub(crate) fn pool_bytes(&self) -> u64 {
        self.pool
    }

    /// Take-back: a plain region's `len` and the bytes it spans (`RegionBuf::whole`); `None` for a reserved one.
    pub(crate) fn shape(&self) -> Option<(usize, usize)> {
        self.plain.then_some((self.len, self.len + self.slack))
    }

    /// Take-back (T2): the region made again for a draw of its shape, its pages untouched (still resident: nothing given
    /// back), its block, residency entry and pool hold its own again, its charge from REAPING to the depot's holder
    /// (PARKED), as a parked region's is when a take finds it (`RegionDepot::take_parked` moves it on to the taker).
    /// pagepool P3: never a pool region (`give_parts` refuses one), so an allocator region again.
    pub(crate) fn take_back(self) -> RegionBuf {
        let GiveParts { at, reserve, charge, res, pool, kept, plain, len, slack, given, .. } = self;
        debug_assert!(plain, "a plain allocator region");
        let mut b = RegionBuf { ptr: at, len, held: crate::mem::grant::Held::settled(pool, kept), reserve, committed: 0, extra: Vec::new(), rw: 0, floor: false, laid: false, charge, lay: 0, cut: 0, slack, given, res, pool: false, dead: false, round: 0 }; // alloc: Cold
        b.parked();
        b
    }

    /// Its drop's allocator path, on this thread: its pages given back (`len`, and a reserved region's committed growth, as
    /// its drop gives back), off the residency registry, its charge dropped (door 1: REAPING down by its bytes, this
    /// thread's account up by the block's) and the block freed (door 1: this thread's account down by it, so this thread
    /// nets 0), then the pool's bytes released: the pool's room never rises before the pages are given.
    pub(crate) fn give(self) {
        let GiveParts { at, reserve, give, charge, res, pool, kept, .. } = self;
        dontneed(at.as_ptr(), give);
        #[cfg(all(debug_assertions, target_os = "linux"))]
        {
            // the whole 2 MiB pages inside: no huge-page collapse can refill them (none of their pages is mapped)
            const PMD: usize = 2 << 20;
            let (lo, hi) = ((at.as_ptr() as usize).next_multiple_of(PMD), (at.as_ptr() as usize + give) & !(PMD - 1));
            let left = if hi > lo { pagegive::resident_bytes(lo, hi, &mut [0u8; 512]) } else { None };
            debug_assert!(left.is_none_or(|b| b == 0), "a give's pages given back before its pool bytes: {left:?} resident");
        }
        crate::mem::giveback::probe(at.as_ptr() as usize, give);
        crate::mem::resident::unregister(res);
        drop(charge);
        // SAFETY: `at` is a live allocation of exactly this layout (`RegionBuf::alloc`, `reserve` bytes), taken out of its
        // region by `give_parts` (which left the region empty, so its drop frees nothing) and freed once, here.
        unsafe { std::alloc::dealloc(at.as_ptr(), core::alloc::Layout::from_size_align_unchecked(reserve, REGION_ALIGN)) };
        crate::mem::grant::give_back(pool, kept);
    }
}

impl RegionBuf {
    /// H3 step (4): its writable bytes (`rw`: a mapped reservation's; 0 for an allocator region).
    pub fn writable(&self) -> usize {
        self.rw
    }

    /// H3 step (4) (`mem::giveback`): an allocator region its owner gives back whole, taken apart for a give-back job. The
    /// grant pool's part of its hold is taken over first (`Held::take_pool`, as `reap::defer`: a charge to G becomes the
    /// pool's, the statement's count past G given back now). Then its charge moves whole to `Cache(Reaping)` (parked()'s
    /// move, `Charge::transfer`; its residency entry with it, `listed`), so the owner's account goes down by the charge's
    /// bytes, as this region's drop on this thread takes them, and REAPING up by them. Then its block, its charge and its
    /// residency entry leave it: its own drop frees nothing (its holds' rest, `extra` and what `take_pool` left, drop with it
    /// on this thread, as reap::defer leaves them). `None` (nothing changed) for a mapped region, an empty one, or a pool
    /// with no room for the hold.
    pub(crate) fn give_parts(&mut self) -> Option<GiveParts> {
        // pagepool P3: never a pool region (its block is the pool's: `pagepool::give` at its drop, never `dealloc`)
        if self.rw != 0 || self.len == 0 || self.pool {
            return None;
        }
        let (pool, kept) = self.held.take_pool()?;
        self.move_to(memledger::Owner::Cache(memledger::Holder::Reaping), memledger::Release::Evict);
        unguard(self.ptr.as_ptr(), self.len);
        let plain = !self.is_reserved();
        let give = if plain { self.len } else { self.len + self.committed };
        let charge = core::mem::replace(&mut self.charge, memledger::Charge::NONE);
        let res = core::mem::replace(&mut self.res, crate::mem::resident::NONE);
        let len = core::mem::replace(&mut self.len, 0);
        Some(GiveParts { at: self.ptr, reserve: self.reserve, give, charge, res, pool, kept, plain: plain && !self.floor && !self.laid, len, slack: self.slack, given: self.given })
    }
}

/// H3 step (4) C1-c (`mem::giveback::park`): a region on its way into the depot whose committed pages past its share the
/// give-back service gives back first (`uncommit`'s page work, off the putting thread), then parks. The depot insertion is
/// the fence: until it the region is the job's alone, and no take sees it. `RegionBuf::into_park` makes it on the putting
/// thread with every account move done there: E1, a mapping's writable tail `[keep, keep + tail)` given off its charge
/// and opened on REAPING (the charged delta); E2, the region settled and on the depot's account (`parked`); E3, the
/// owner's residency entry shrunk to `keep` and the tail's own entry on REAPING. An allocator reservation's committed
/// pages leave no named charge (door 1 charges its whole block): no REAPING, its pages pending relief alone.
pub(crate) struct ParkParts {
    buf: RegionBuf,
    /// `[len, len + gone)`: the committed bytes whose pages go back.
    gone: usize,
    keep: usize,
    tail: usize,
    reaping: memledger::Charge,
    res: u32,
    /// The pool's part of the committed bytes' holds (`Held::take_pool`), the plain and the kept bytes, released after
    /// their pages are given.
    pool: u64,
    kept: u64,
}

// SAFETY: `into_park` settled the region's hold on the putting thread (as the depot's regions are: no statement `Rc` is
// left in it) and took only the pool's bytes of the committed bytes' holds; the region is the job's alone until its
// insertion.
unsafe impl Send for ParkParts {}

/// C1-c: mapping tails left writable because the kernel refused their PROT_NONE in a park job (given back all the same).
pub static PARK_PROTECT_REFUSED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

impl ParkParts {
    /// Its bytes on REAPING (a mapping's tail, E1) and its pending relief (an allocator reservation's committed bytes).
    pub(crate) fn bytes(&self) -> (usize, usize) {
        let charged = self.reaping.committed() as usize;
        (charged, if self.tail == 0 { self.gone } else { 0 })
    }

    /// TB4: the grant pool's bytes of the committed bytes' holds it holds (plain and kept), released after its give.
    pub(crate) fn pool_bytes(&self) -> u64 {
        self.pool + self.kept
    }

    /// `uncommit`'s page work, on this thread: the committed bytes' pages given back, a mapping's tail closed again
    /// (PROT_NONE: a taker writing past `len + committed` faults, D3163), the tail off REAPING and its residency entry, and
    /// then the committed bytes' pool bytes released: the pool's room never rises before the pages are given. The region,
    /// for the depot's insertion.
    pub(crate) fn give(self) -> RegionBuf {
        let ParkParts { buf, gone, keep, tail, reaping, res, pool, kept } = self;
        // SAFETY: `[ptr + len, ptr + len + gone)` is inside this allocation of `reserve` bytes; its contents are dead (the
        // region parks with its share only) and nothing else holds the region until its insertion
        let at = unsafe { buf.ptr.as_ptr().add(buf.len) };
        dontneed(at, gone);
        // SAFETY: `[ptr + keep, ptr + keep + tail)` is inside this region's mapping (`tail` != 0 only for one); its contents
        // are dead (given back above), and nothing reads or writes them until a commit opens them again
        if tail != 0 && !unsafe { protect_none(buf.ptr.as_ptr().add(keep), tail) } {
            PARK_PROTECT_REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        crate::mem::giveback::probe(at as usize, gone);
        crate::mem::resident::unregister(res);
        drop(reaping);
        crate::mem::grant::give_back(pool, false);
        crate::mem::grant::give_back(kept, true);
        buf
    }
}

impl RegionBuf {
    /// C1-c: this region, about to park with committed bytes past its share (not a laid one: it parks with them), taken
    /// apart for a park job (`ParkParts`, its account moves made here); `Err` (unchanged) when it has none to give back.
    pub(crate) fn into_park(mut self) -> core::result::Result<ParkParts, RegionBuf> {
        // pagepool P3: never a pool region: its committed pages go dead in the pool (`uncommit`'s `pool_dead`, no
        // syscall, no protection change), on the putting thread as before
        if self.laid || self.committed == 0 || self.len == 0 || self.pool {
            return Err(self);
        }
        let gone = core::mem::take(&mut self.committed);
        // at least a page: `rw != 0` is what marks the region a mapping (`Drop` unmaps it)
        let keep = self.len.next_multiple_of(page_sizes().0).max(page_sizes().0);
        let (mut tail, mut reaping, mut res) = (0, memledger::Charge::NONE, crate::mem::resident::NONE);
        if self.rw > keep {
            tail = self.rw - keep;
            let before = self.charge.committed();
            self.charge.give(tail as u64);
            let delta = before - self.charge.committed();
            reaping = memledger::Charge::open(memledger::Owner::Cache(memledger::Holder::Reaping), memledger::Release::Evict, memledger::Kind::Mapped, memledger::charge::Door::Mapped, delta);
            res = crate::mem::resident::register(self.ptr.as_ptr() as usize + keep, tail, delta as usize, memledger::Acct::REAPING, true, false, reaping.nr_home());
            self.rw = keep;
        }
        // the committed bytes' holds give the pool its part of them (a charge to G becomes the pool's, the statement's
        // count past G given back now, as `reap::defer` takes a region's), released by the job after the pages; their rest
        // drops here, on the putting thread, with the list (a hold the pool has no room for drops whole now, as before)
        let (mut pool, mut kept) = (0, 0);
        for h in self.extra.iter_mut() {
            match h.take_pool() {
                Some((n, true)) => kept += n,
                Some((n, false)) => pool += n,
                None => {}
            }
        }
        self.extra.clear();
        self.settle();
        self.parked();
        Ok(ParkParts { buf: self, gone, keep, tail, reaping, res, pool, kept })
    }
}

/// C1-c's witness: a floor reservation of `reserve` bytes holding `share`, `more` bytes committed past it in place and
/// written, as a breaker's region is before its put.
#[doc(hidden)]
pub fn park_witness_region(share: usize, reserve: usize, more: usize) -> RegionBuf {
    let mut r = reserved_region(share, reserve, None, None, true).expect("a reservation");
    let held = road_held_now(more, None, None, false).expect("an unadmitted draw holds pool bytes");
    assert!(r.commit(more, held), "a commit within the reservation");
    let (p, n) = (r.as_mut_ptr(), r.len() + more);
    // SAFETY: the share and the committed bytes are writable
    unsafe { core::ptr::write_bytes(p, 0x5c, n) };
    r
}

/// C1-c's witness: a take of the parked reservation of that shape (`RegionDepot::take_reserved`).
#[doc(hidden)]
pub fn park_witness_take(share: usize, reserve: usize) -> Option<RegionBuf> {
    crate::park::REGIONS.take_reserved(share, reserve)
}

impl RegionBuf {
    pub const fn empty() -> RegionBuf {
        RegionBuf { ptr: NonNull::<u64>::dangling().cast(), len: 0, held: crate::mem::grant::Held::NONE, reserve: 0, committed: 0, extra: Vec::new(), rw: 0, floor: false, laid: false, charge: memledger::Charge::NONE, lay: 0, cut: 0, slack: 0, given: 0, res: crate::mem::resident::NONE, pool: false, dead: false, round: 0 } // alloc: Cold
    }

    /// `bytes` (> 0) at `REGION_ALIGN`, zeroed or not, held by nothing yet; `None` when the allocator refuses.
    fn alloc(bytes: usize, zeroed: bool) -> Option<RegionBuf> {
        if pooled() {
            if let Some(b) = RegionBuf::pool_alloc(bytes, zeroed) {
                return Some(b);
            }
        }
        let layout = core::alloc::Layout::from_size_align(bytes, REGION_ALIGN).ok().filter(|l| l.size() != 0)?;
        // SAFETY: a non-zero-size layout; a null return is the allocator's refusal, handled here.
        let p = unsafe { if zeroed { std::alloc::alloc_zeroed(layout) } else { std::alloc::alloc(layout) } }; // alloc: Node
        let ptr = NonNull::new(p)?;
        // C8: the allocator counted the block on this thread's account: the charge books its kind for the context's owner
        let (owner, release) = memledger::charge::context();
        let charge = memledger::Charge::open(owner, release, memledger::kind(), memledger::charge::Door::Heap, bytes as u64);
        let res = crate::mem::resident::register(p as usize, bytes, bytes, charge.acct(), false, false, charge.nr_home());
        Some(RegionBuf { ptr, len: bytes, held: crate::mem::grant::Held::NONE, reserve: bytes, committed: 0, extra: Vec::new(), rw: 0, floor: false, laid: false, charge, lay: 0, cut: 0, slack: 0, given: 0, res, pool: false, dead: false, round: 0 }) // alloc: Cold
    }

    /// pagepool P3: `alloc` from the page pool (`Tag::REGION`, statement-lived; zeroed or not as asked): the block on the
    /// context owner's `Door::Pool` charge (no door counted it on this thread). `None`: the pool refused (the allocator's
    /// road then).
    fn pool_alloc(bytes: usize, zeroed: bool) -> Option<RegionBuf> {
        let hint = POOL_HINT.zero(if zeroed { pagepool::Zero::Dense } else { pagepool::Zero::No });
        let s = pagepool::take(bytes, POOL_ALIGN, hint)?;
        // pppress card 13 (3): the slice's length is what the pool counts in use for the block (`pagepool::usable`; a
        // span's commit): charged whole, so REGION's account is the pool's in-use bytes by construction. `len` and
        // `reserve` stay the ask (their readers unchanged); the bytes past it are `round`, charged until the drop.
        debug_assert!(s.len() >= bytes, "the pool hands out at least the ask");
        let have = s.len().max(bytes);
        let ptr = s.cast::<u8>();
        let (owner, release) = memledger::charge::context();
        let charge = memledger::Charge::open(owner, release, memledger::kind(), memledger::charge::Door::Pool, have as u64);
        let res = crate::mem::resident::register(ptr.as_ptr() as usize, have, have, charge.acct(), false, false, charge.nr_home());
        Some(RegionBuf { ptr, len: bytes, held: crate::mem::grant::Held::NONE, reserve: bytes, committed: 0, extra: Vec::new(), rw: 0, floor: false, laid: false, charge, lay: 0, cut: 0, slack: 0, given: 0, res, pool: true, dead: false, round: have - bytes }) // alloc: Cold
    }

    /// pagepool P3: `map_reserved` from the page pool: a span of the reservation's mapped length, its first `bytes` (whole
    /// pages; a warm lazy span's committed pages, a pool span whole) writable and on the context owner's `Door::Pool`
    /// charge, the rest committed in place as `commit` asks (`commit_to`). `None`: the pool refused.
    fn pool_reserved(bytes: usize, reserve: usize) -> Option<RegionBuf> {
        let total = mapped_len(reserve);
        let mut s = pagepool::span(total, POOL_HINT)?;
        let page = page_sizes().0;
        // the ask's pages alone writable and charged, as the mapped road's (at least a page: `rw != 0` marks a reservation):
        // the span's pages past them (a pool span's other superpages, a warm lazy span's commit from its last owner) go dead
        // in the pool below, where the purger may release them under pressure, and come live as `commit` opens them
        let rw = bytes.next_multiple_of(page).max(page).min(s.len());
        // SAFETY: the span just taken, its first `rw` bytes the region's from here
        unsafe { pagepool::commit_to(&mut s, rw) };
        // pppress card 13 (3): what the pool counts in use for the span now (its commit: at least `rw`, a warm lazy span's
        // last commit, a pool span whole) opens the charge, and what `pool_dead_off` takes off that count below leaves
        // it: the region's account is the pool's count for it (the ask's pages, and the dead grain's part-run past them)
        let had = s.committed().max(rw);
        let ptr = NonNull::new(s.as_ptr())?;
        let (owner, release) = memledger::charge::context();
        let charge = memledger::Charge::open(owner, release, memledger::kind(), memledger::charge::Door::Pool, had as u64);
        let mut b = RegionBuf { ptr, len: bytes, held: crate::mem::grant::Held::NONE, reserve, committed: 0, extra: Vec::new(), rw, floor: false, laid: false, charge, lay: 0, cut: 0, slack: 0, given: 0, res: crate::mem::resident::NONE, pool: true, dead: false, round: 0 }; // alloc: Cold
        b.pool_dead_off(rw, had - rw);
        let charged = b.charge.committed() as usize;
        b.res = crate::mem::resident::register(ptr.as_ptr() as usize, b.pool_extent(), charged, b.charge.acct(), false, false, b.charge.nr_home());
        Some(b)
    }

    /// pagepool P3: `[at, at + len)` of a pool region dead (its whole runs: the purger may release them; the region writes
    /// none of them before `pool_live`). The CVE-safe release is the purger's (`pagegive::give_back` by remap).
    /// pppress card 13 (3): the bytes the pool took off its in-use count (`give_pages`: its whole runs that were live),
    /// the bytes the region's charge gives (`pool_dead_off`), never the range's length.
    fn pool_dead(&self, at: usize, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        // SAFETY: inside this region's block, contents dead: nothing reads or writes them until `pool_live`.
        unsafe { pagepool::give_pages(self.ptr, at, len) }
    }

    /// pppress card 13 (3): `pool_dead`, its bytes off the region's charge: the charge moves by what the pool's in-use
    /// count moved, so the region's account stays the pool's count for it (its residency entry with it).
    fn pool_dead_off(&mut self, at: usize, len: usize) {
        let n = self.pool_dead(at, len);
        if n != 0 {
            self.charge.give(n as u64);
            self.listed();
        }
    }

    /// pppress card 13 (3): `pool_live`, its bytes onto the region's charge (as `pool_dead_off` gave them).
    fn pool_live_on(&mut self, at: usize, len: usize) {
        let n = self.pool_live(at, len);
        if n != 0 {
            self.charge.commit(n as u64);
            self.listed();
        }
    }

    /// pagepool P3: `[at, at + len)` of a pool region live again before it is written: every dead run touching it. `pool_dead`
    /// gives only whole runs inside the region's `reserve` bytes (`give_pages` rounds inward), so those are the runs from the
    /// range's first run to its last, clipped to the whole runs of `[ptr, ptr + reserve)` (a run the region shares with
    /// another block at either edge is never its to revive). Their contents are dirty or zero; no region reads a byte past
    /// its length unwritten.
    /// pppress card 13 (3): the bytes the pool put back on its in-use count (its dead or released runs made live).
    fn pool_live(&self, at: usize, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        let b = self.ptr.as_ptr() as usize;
        let lo = ((b + at) & !(POOL_RUN - 1)).max(b.next_multiple_of(POOL_RUN));
        let hi = (b + at + len).next_multiple_of(POOL_RUN).min((b + self.reserve) & !(POOL_RUN - 1));
        if hi <= lo {
            return 0;
        }
        // SAFETY: whole runs inside this region's block (`take_pages` takes its range's whole runs), before any write.
        unsafe { pagepool::take_pages_counted(self.ptr, lo - b, hi - lo) }.1
    }

    /// D2894: `reserve` bytes (> `bytes`) of address space alone (PROT_NONE, MAP_NORESERVE: no commit charge, no RSS), its
    /// first `bytes` (whole pages) readable and writable; `None` when the kernel refuses the mapping or the protection.
    #[cfg(target_os = "linux")]
    fn map_reserved(bytes: usize, reserve: usize) -> Option<RegionBuf> {
        if pooled() {
            if let Some(b) = RegionBuf::pool_reserved(bytes, reserve) {
                return Some(b);
            }
        }
        let page = page_sizes().0;
        reserve.checked_next_multiple_of(OPEN_STEP)?;
        let total = mapped_len(reserve);
        let rw = bytes.checked_next_multiple_of(page)?.min(total);
        let p = vm::map_none(total)?;
        mempolicy::maps::name(p, total, mempolicy::maps::region(memledger::kind()));
        // SAFETY: `[p, p + rw)` is inside the mapping just made; only its protection changes
        if rw != 0 && !unsafe { protect_rw(p, rw) } {
            unmap(p, total);
            return None;
        }
        // G1-A door 2 / C8: the writable pages are the mapping's committed bytes (the PROT_NONE rest is address space alone),
        // the context owner's charge
        let ptr = NonNull::new(p)?;
        let (owner, release) = memledger::charge::context();
        let charge = memledger::Charge::open(owner, release, memledger::kind(), memledger::charge::Door::Mapped, rw as u64);
        let res = crate::mem::resident::register(p as usize, rw, rw, charge.acct(), true, true, charge.nr_home());
        Some(RegionBuf { ptr, len: bytes, held: crate::mem::grant::Held::NONE, reserve, committed: 0, extra: Vec::new(), rw, floor: false, laid: false, charge, lay: 0, cut: 0, slack: 0, given: 0, res, pool: false, dead: false, round: 0 }) // alloc: Cold
    }

    #[cfg(not(target_os = "linux"))]
    fn map_reserved(bytes: usize, reserve: usize) -> Option<RegionBuf> {
        RegionBuf::alloc(reserve.max(bytes), false).map(|mut b| {
            b.len = bytes;
            b
        })
    }

    /// LEDGER2 step 3: the region's charge (its committed bytes with their door and kind, its reservation) moves to owner
    /// `o`, released as before: an operator arena's chunk (`spill_host::arena_chunk`) is its arena's owner's, once. Step 5:
    /// a breaker slot's region, block or partial at its draw or bind (the slot's owner). Nothing for an empty region or one
    /// that is `o`'s already.
    pub(crate) fn own_by(&mut self, o: memledger::Owner) {
        if self.len != 0 && self.charge.owner() != o {
            let release = self.charge.release();
            self.move_to(o, release);
        }
    }

    /// D4844: the region's charge moves to owner `o` with `o`'s release (`release_of`): a bound set the plan memo files with
    /// its admission is the session's from the filing on, not the portal's its Bind ran under. Nothing for an empty region
    /// or one that is `o`'s already.
    pub fn adopt(&mut self, o: memledger::Owner) {
        if self.len != 0 && self.charge.owner() != o {
            self.move_to(o, memledger::charge::release_of(o));
        }
    }

    /// D2894: the reservation is a breaker's PostgreSQL floor (`grow::floor_reserve`), not a set operation side's.
    pub fn is_floor(&self) -> bool {
        self.floor
    }

    /// The region started small: its table is laid out at the reservation (`RESERVE_LAYOUT`); false for any region that
    /// is no reservation.
    pub fn is_laid(&self) -> bool {
        self.laid
    }

    /// The reservation a laid region's table reads (`RESERVE_LAYOUT`): its taker's ask (`lay`), else the mapping's.
    pub fn lay_reserve(&self) -> usize {
        if self.laid && self.lay != 0 { self.lay.min(self.reserve) } else { self.reserve }
    }

    /// D3809: the share a laid region's partitions are cut by (its taker's), 0 for any other region.
    pub fn lay_share(&self) -> usize {
        if self.laid { self.cut } else { 0 }
    }

    /// Marks a reservation that started small (`grant::start_bytes`); an allocator region (the mapping refused) never is.
    pub(crate) fn set_laid(&mut self, on: bool) {
        self.laid = on && self.is_reserved();
    }

    /// Option c': an address-space reservation of `reserve` bytes (> `len`): the allocation's length.
    pub fn reserve(&self) -> usize {
        self.reserve
    }

    /// Option c': the region is a reservation past its charged `len` (a `Budget::Pool` side's, `reserved_region`). A mapped
    /// reservation (`rw != 0`, `map_reserved`) is one whatever its length: a region that started small parks with the
    /// pages its growth committed (`uncommit`), so one that grew through its whole reservation parks with `len` equal to
    /// it, and read as an allocator region a plain take would refit its `len` (`fit_to`) past the pages it has writable.
    pub fn is_reserved(&self) -> bool {
        self.rw != 0 || self.reserve > self.len + self.slack
    }

    /// D3074 / D3089: the bytes an allocator region spans, its `len` and its uncharged tail (`fit_to`); a take's fit and
    /// cover read this, the charges read `len`.
    pub fn whole(&self) -> usize {
        self.len + self.slack
    }

    /// D3074 / D3089: a parked allocator region fitted to a take's ask before it is charged: `len` becomes `bytes` (page-rounded,
    /// within the allocation), the bytes past it its tail; a tail past an eighth of the ask has its pages given back to the
    /// kernel as they leave `len` (MADV_DONTNEED: a lazily freed page would still count in the resident memory the governor
    /// and the watchdog read), one within an eighth keeps them, uncharged (D225's own slack),
    /// and the pool's settled hold follows `len` (a shrink gives the rest back, a growth is held first). False (nothing
    /// changed) when the pool has no room for the growth.
    pub(crate) fn fit_to(&mut self, bytes: usize) -> bool {
        debug_assert!(!self.is_reserved() && self.rw == 0, "an allocator region");
        let whole = self.whole();
        let len = bytes.next_multiple_of(page_sizes().0).min(whole).max(bytes.min(whole));
        if len == self.len {
            return true;
        }
        if !self.held.resize_pool(len as u64) {
            return false;
        }
        // a tail within an eighth of the ask keeps its pages (an arena or a share that shrinks and grows by a few pages a
        // statement would refault them every run); a larger one gives them back
        // pppress card 13 (3): a pool region's charge moves by what the pool's in-use count moved (`pool_dead_off`,
        // `pool_live_on`: whole runs), not by the length
        if len < self.len && whole - len > len / 8 {
            if self.pool {
                self.pool_dead_off(len, self.len - len);
            } else {
                dontneed(self.ptr.as_ptr().wrapping_add(len), self.len - len);
                self.charge.give((self.len - len) as u64);
            }
            self.given += self.len - len;
            self.listed();
        } else if len > self.len && self.given != 0 {
            // G1-A: a growth back into given-back pages takes them again (at most what was given)
            let back = self.given.min(len - self.len);
            if self.pool {
                self.pool_live_on(self.len, len - self.len);
            } else {
                self.charge.commit(back as u64);
            }
            self.given -= back;
            self.listed();
        }
        self.len = len;
        self.slack = whole - len;
        true
    }

    /// The bytes the pool holds for this region (its settled hold).
    pub fn pool_bytes(&self) -> u64 {
        self.held.pool_bytes()
    }

    /// Option c': the bytes committed past `len` (`commit`).
    pub fn committed(&self) -> usize {
        self.committed
    }

    /// The bytes this region holds read-write, the number the pool charges for it: a reservation's `len` and what `commit`
    /// added in place past it (`map_reserved`'s writable extent before page rounding); an allocator region's `len` (its
    /// uncharged tail, `fit_to`, left out).
    pub fn charged_bytes(&self) -> u64 {
        (if self.is_reserved() { self.len + self.committed } else { self.len }) as u64
    }

    /// C5 (revoke's give-back): what `commit` added past `len` given back mid-statement, its contents dead (a revoked
    /// breaker's emptied growth, `grow::give_back_scattered`): its pages to the kernel (MADV_DONTNEED; the mapping keeps its
    /// protection, so a later commit writes them again without a fault) and its holds released, a laid region's too (unlike
    /// `uncommit`, which keeps a laid region's growth for its next taker). The bytes given back.
    pub(crate) fn give_back_committed(&mut self) -> usize {
        let c = self.committed;
        if c == 0 {
            return 0;
        }
        if self.pool {
            // pppress card 13 (3): off its charge as they leave the pool's in-use count
            self.pool_dead_off(self.len, c);
        } else {
            // SAFETY: `committed` bytes past `len`, inside this allocation of `reserve` bytes; the caller proved them dead
            dontneed(unsafe { self.ptr.as_ptr().add(self.len) }, c);
        }
        self.committed = 0;
        self.extra.clear();
        // D3872: its pages left: the next residency pass reads it again
        self.listed();
        c
    }

    /// D4291: the bytes `give_back_past(keep)` would give back now (a kept hold's length aside).
    pub(crate) fn past_bytes(&self, keep: usize) -> usize {
        let keep = keep.next_multiple_of(page_sizes().0);
        let warm = if self.is_reserved() && self.len > keep && !self.held.is_charged() { self.len - keep } else { 0 };
        self.committed + warm
    }

    /// D4291: a reserved region's bytes past `keep` given back mid-statement, their contents dead (a finished sort's run put
    /// to its temp file, `Node::put_own_sorts`): what `commit` added (`give_back_committed`), and the length a warm laid take
    /// brought past `keep` (D3794: a parked laid region keeps its growth as its length) when its hold can shrink with it
    /// (`Held::resize_pool`: a pool hold, not a charge to G). The pages go to the kernel (MADV_DONTNEED; the mapping keeps
    /// its protection, so a later commit writes them again without a fault). The bytes given back.
    pub(crate) fn give_back_past(&mut self, keep: usize) -> usize {
        let mut given = self.give_back_committed();
        let keep = keep.next_multiple_of(page_sizes().0);
        if self.is_reserved() && self.len > keep && !self.held.is_charged() && self.held.resize_pool(keep as u64) {
            if self.pool {
                // pppress card 13 (3): off its charge as they leave the pool's in-use count
                self.pool_dead_off(keep, self.len - keep);
            } else {
                // SAFETY: `[ptr + keep, ptr + len)` is inside this allocation; the caller proved its contents dead
                dontneed(unsafe { self.ptr.as_ptr().add(keep) }, self.len - keep);
            }
            given += self.len - keep;
            self.len = keep;
            // D3872: its pages left: the next residency pass reads it again
            self.listed();
        }
        given
    }

    /// Option c' (a reservation about to park): what the fold committed past the share goes back, its pages to the kernel
    /// and its holds released; the share and the reservation's address space stay.
    /// D3163: a mapped reservation's pages past its share are closed again (PROT_NONE, `rw` back to the share's pages), so
    /// the parked region's next taker faults at a write past `len + committed` instead of re-faulting pages no hold charges;
    /// its next `commit` opens them.
    pub(crate) fn uncommit(&mut self) {
        // A region that started small (`laid`) parks with the pages its growth committed: they join its
        // length, which the depot counts and the pool holds as it holds any parked region (so the park budget, `trim_to`
        // and the idle release drop them), and its next laid taker (`RegionDepot::take_laid`) writes them without a fault
        if self.laid && self.committed != 0 {
            self.len += self.committed;
            self.committed = 0;
            self.extra.clear();
            return;
        }
        if self.committed != 0 {
            if self.pool {
                // pppress card 13 (3): off its charge as they leave the pool's in-use count
                self.pool_dead_off(self.len, self.committed);
            } else {
                // SAFETY: `committed` bytes past `len`, inside this allocation of `reserve` bytes; their contents are dead.
                dontneed(unsafe { self.ptr.as_ptr().add(self.len) }, self.committed);
            }
            self.committed = 0;
            // D3872: its pages left: the next residency pass reads it again
            self.listed();
        }
        // at least a page: `rw != 0` is what marks the region a mapping (`Drop` unmaps it)
        let keep = self.len.next_multiple_of(page_sizes().0).max(page_sizes().0);
        // pagepool P3: a pool region's pages past `keep` go dead in the pool instead (no protection change)
        if self.pool && self.rw > keep {
            // pppress card 13 (3): its charge gives what the pool's in-use count gave (whole runs), not `rw - keep`
            self.pool_dead_off(keep, self.rw - keep);
            self.rw = keep;
            self.listed();
        }
        // SAFETY: `[ptr + keep, ptr + rw)` is inside this region's mapping (`map_reserved`: rw != 0 only for one); its
        // contents are dead (given back above), and nothing reads or writes them until a commit opens them again
        if self.rw > keep && unsafe { protect_none(self.ptr.as_ptr().add(keep), self.rw - keep) } {
            self.charge.give((self.rw - keep) as u64);
            self.rw = keep;
            self.listed();
        }
        self.extra.clear();
    }

    /// Option c': `bytes` more of the reservation committed in place, held by `held` until the region drops. D2894: a mapped
    /// reservation's pages up to the new end made readable and writable first; false (nothing committed, `held` dropped:
    /// its charge released) when the kernel refuses the protection. The writable extent opens in whole 2 MiB steps at their
    /// address boundaries (`open_to`): one protection change per step, each step one huge page to the kernel; the charge
    /// stays the bytes committed.
    pub(crate) fn commit(&mut self, bytes: usize, held: crate::mem::grant::Held) -> bool {
        debug_assert!(self.len + self.committed + bytes <= self.reserve, "a commit within the reservation");
        let end = self.len + self.committed + bytes;
        if self.pool {
            // pagepool P3: what `give_back_committed` / `give_back_past` / `uncommit` gave dead, live before it is written
            // (pppress card 13 (3): onto its charge as the pool counts them in use again)
            self.pool_live_on(self.len + self.committed, bytes);
        }
        if self.rw != 0 && end > self.rw {
            let rw = open_to(self.ptr.as_ptr(), end, mapped_len(self.reserve));
            let opened = if self.pool {
                // pagepool P3: the span's pages touched to `rw` (the pool's claim; no protection change)
                // SAFETY: the region's own live span, starting at its base
                if let Some(mut s) = unsafe { pagepool::span_at(self.ptr) } {
                    let c0 = s.committed();
                    // SAFETY: as above; `rw` is within the span (`mapped_len` of the reservation it was taken for)
                    unsafe { pagepool::commit_to(&mut s, rw) };
                    // pppress card 13 (3): the span's new commit, as the pool counts it in use
                    self.charge.commit(s.committed().saturating_sub(c0) as u64);
                }
                // the opened pages charged from here: live first (`pool_reserved` left the span's pages past the ask dead)
                self.pool_live_on(self.rw, rw - self.rw);
                true
            } else {
                // SAFETY: `[ptr + self.rw, ptr + rw)` is inside this region's mapping (`map_reserved`); only its protection changes
                unsafe { protect_rw(self.ptr.as_ptr().add(self.rw), rw - self.rw) }
            };
            if !opened {
                return false;
            }
            if !self.pool {
                self.charge.commit((rw - self.rw) as u64);
            }
            self.rw = rw;
            self.listed();
        }
        self.committed += bytes;
        self.extra.push(held); // alloc: Cold
        true
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bytes the region holds for its taker: its length (D3074 / D3089: a parked region taken for a smaller ask is
    /// fitted to it, `fit_to`, so this is the ask, not the allocation; `whole` is the allocation).
    pub fn capacity(&self) -> usize {
        self.len
    }

    pub fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    pub(crate) fn settle(&mut self) {
        self.held.settle();
    }

    /// D2687 (debug builds): a region taken out of the depot has its pages accessible again (`guard_parked`). G1-A / C8: its
    /// charge moves from the depot to the taker's owner (the context's: the portal its backend executes, else its statement).
    pub(crate) fn unparked(&mut self) {
        unguard(self.ptr.as_ptr(), self.len);
        if self.dead {
            // pagepool P3: `park_free`'s dead pages live again before the taker writes them
            self.dead = false;
            // pppress card 13 (3): onto its charge (still the depot's, moved below) as the pool counts them again
            self.pool_live_on(0, self.len);
        }
        let (owner, release) = memledger::charge::context();
        self.move_to(owner, release);
    }

    /// D2687: a spill region parked at its statement's end hands its pages back lazily (`free_lazy`: MADV_FREE). pagepool P3:
    /// a pool region's go dead in the pool instead (`give_pages`: the purger releases them as its policy says), taken live
    /// at the region's next `unparked`. pppress card 13 (3): they leave its charge (PARKED's account), so the ladder's R2p
    /// reads ~0 here and R1 reaches them as dead runs of a live block (`pagepool::releasable`). Reach by
    /// `pool_thp_partial`: 0 / 2 / 3 = R1 releases them under pressure (2 and 3 split the folio first; 3 is cold-only in
    /// its age passes alone); 1 (keep) = a dead run in a superpage still in use is in neither R1's reach nor R2p's, a
    /// KNOWN LIMIT of 1 (evicting the region could still free a whole superpage for R1; nothing asks for it).
    pub(crate) fn park_free(&mut self) {
        if self.pool {
            // pppress card 13 (3): off its charge as they leave the pool's in-use count
            self.pool_dead_off(0, self.len);
            self.dead = true;
        } else {
            free_lazy(self.ptr.as_ptr(), self.len);
        }
    }

    /// D641: a region about to drop has its pages returned to the kernel first (`dontneed`). pagepool P3: a pool region's go
    /// back to the pool warm with its block at the drop (the purger's release, never a `pagegive` call on pool memory here).
    pub(crate) fn dontneed_all(&mut self) {
        if !self.pool {
            dontneed(self.ptr.as_ptr(), self.len);
        }
    }

    /// pagepool P3: the region is the page pool's (`pool_alloc` / `pool_reserved`): its block goes back by `pagepool::give`.
    pub fn is_pool(&self) -> bool {
        self.pool
    }

    /// G1-A / C8: a region put in the depot leaves its statement for the depot's holder (PARKED's account), released when
    /// the depot evicts it.
    pub(crate) fn parked(&mut self) {
        self.move_to(memledger::Owner::Cache(memledger::Holder::Parked), memledger::Release::Evict);
    }

    /// G1-A / C8: the bytes this region has on its account (a mapping's writable pages; an allocation less its given-back
    /// pages) moved with its charge to owner `to` (`Charge::transfer`).
    fn move_to(&mut self, to: memledger::Owner, release: memledger::Release) {
        self.charge.transfer(to, release);
        self.listed();
    }

    /// D3872: its residency entry set to what its account has for it now (a mapping's writable pages, an allocation less
    /// its given-back pages), on its account now.
    fn listed(&self) {
        // pppress card 13 (3): a pool region's charge is the pool's in-use count for it (whole runs, its `round`)
        let (span, charged) = if self.pool {
            (self.pool_extent(), self.charge.committed() as usize)
        } else if self.rw != 0 {
            (self.rw, self.rw)
        } else {
            (self.reserve, self.reserve - self.given)
        };
        crate::mem::resident::update_at(self.res, self.ptr.as_ptr() as usize, span, charged, self.charge.acct(), self.charge.nr_home());
    }

    /// pppress card 13 (3): the bytes of a pool region's block its residency entry reads: a reservation's writable pages
    /// to the dead grain (the part-run past `rw` stays live in the pool, on its charge), an allocation's slice (`round`
    /// with it). A warm lazy span's last commit past that, kept with PGRUST_POOL_SPAN_TRIM off, ends in a part-run the
    /// pool cannot give dead (under a run, outside this range: read as not resident).
    fn pool_extent(&self) -> usize {
        if self.rw != 0 { self.rw.next_multiple_of(POOL_RUN) } else { self.reserve + self.round }
    }

    pub(crate) fn to_pool(&mut self) -> bool {
        self.held.to_pool()
    }

    /// False: the region does not fit the statement's grant (the caller drops it and takes an exact one).
    #[must_use]
    pub(crate) fn recharge(&mut self, stmt: Option<&crate::mem::grant::Reservation>, life: Option<&std::rc::Rc<crate::mem::grant::StmtCtx>>) -> bool {
        let n = self.len;
        n == 0 || self.held.recharge(stmt, life, n)
    }

    pub fn is_charged(&self) -> bool {
        self.held.is_charged()
    }

    /// The tests' read of its charge's committed bytes (what its account holds for it, `Charge::committed`).
    #[cfg(test)]
    pub(crate) fn charge_committed(&self) -> u64 {
        self.charge.committed()
    }

    /// P3 (`parked_trim_rung`): the bytes its account holds resident for it now, what its drop takes off the ledger's
    /// resident bytes: its charge's committed bytes (a mapping's writable pages, a heap region's less its given-back
    /// pages), less what the residency pass last read as not resident when it is listed (`resident::resident_of`; a
    /// region under RESIDENCY_MIN counts whole, as its account counts it). Never its length.
    pub(crate) fn account_bytes(&self) -> usize {
        let c = self.charge.committed() as usize;
        crate::mem::resident::resident_of(self.res).map_or(c, |r| r.min(c))
    }

    /// The account its residency entry books on (`None`: not listed): a witness's read that the entry moved with its charge.
    #[doc(hidden)]
    pub fn residency_acct(&self) -> Option<memledger::Acct> {
        memledger::resident::acct_of(self.res)
    }
}

impl core::ops::Deref for RegionBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: `len` bytes at `ptr` (dangling and 0 when empty), owned by this buffer.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl core::ops::DerefMut for RegionBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as `deref`, borrowed mutably through `self`.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

/// A region of `bytes` through the grant pool, for a caller outside the Node (tests, probes).
pub fn try_region(bytes: usize) -> Result<RegionBuf> {
    uninit_region(bytes, None, None)
}

/// `try_region` drawn from `stmt`'s reservation first.
pub fn try_region_in(stmt: &crate::mem::grant::Reservation, bytes: usize) -> Result<RegionBuf> {
    uninit_region(bytes, Some(stmt), None)
}

/// An arena chunk: a parked region first (W holds it when it covers it, else the statement), else a zeroed one on lazy zero
/// pages (`alloc_zeroed`); past W and G it is reserved at its size (D225). None when the pool or the allocator refuses.
pub(crate) fn chunk_region(bytes: usize, w: Option<&mut u64>, stmt: Option<&crate::mem::grant::Reservation>, worker: bool) -> Option<RegionBuf> {
    road_region(bytes, w, stmt, worker, true, false, Asker::Refuses)
}

/// C10 (D4168): `chunk_region` with no road past W and G: W, else G (a leader's room left, a worker's claim purse, D4043);
/// `None` past them, where the caller writes to its temp-file road (a spool, `hostimpl::arena::object_region`).
pub(crate) fn chunk_region_within(bytes: usize, w: Option<&mut u64>, stmt: Option<&crate::mem::grant::Reservation>, worker: bool) -> Option<RegionBuf> {
    let n = bytes as u64;
    if w.as_deref().is_some_and(|c| *c >= n) || stmt.is_some_and(|s| s.left() >= n) {
        return chunk_region(bytes, w, stmt, worker);
    }
    held_region(bytes, worker_purse(worker, n)?, true)
}

/// `chunk_region` that takes the pool's room past W and G as it is now, with no wait and no refusal kept on the statement:
/// the reopen of a breaker under its ceiling (`grow::region_grow_in`), whose caller spills when the pool has no room.
pub(crate) fn chunk_region_now(bytes: usize, w: Option<&mut u64>, stmt: Option<&crate::mem::grant::Reservation>, worker: bool) -> Option<RegionBuf> {
    road_region(bytes, w, stmt, worker, true, true, Asker::Refuses)
}

/// `chunk_region` not zeroed, for a pin that zeroes what it asks for itself (`arena::pin_road`); `now`: a draw its caller can
/// do without (KN-02 (B)'s owners past G) takes the pool's room past W and G as it is now, with no wait and no refusal kept
/// on the statement. D3212: zeroed, the allocator memset reused memory on the drawing thread (a 1.2 GB owner chunk, 0.6-1.5 s
/// on the leader with every worker idle).
pub(crate) fn chunk_region_unzeroed(bytes: usize, w: Option<&mut u64>, stmt: Option<&crate::mem::grant::Reservation>, worker: bool, now: bool) -> Option<RegionBuf> {
    road_region(bytes, w, stmt, worker, false, now, Asker::Refuses)
}

/// Ladder step 1.1 (`gate_now`): `chunk_region` (`zeroed`) or `chunk_region_unzeroed` for a draw whose caller has a road past
/// the pool (`spill_host::v4::take_road` in a session with a temp-file road: the statement's mapped temp file): past W and G
/// the gate's `Spills` asker (`GrantPool::take_gate`: no wait unless bytes are on their way, no refusal kept). With
/// `gate_now` off, `chunk_region`'s road exactly.
pub(crate) fn chunk_region_spills(bytes: usize, w: Option<&mut u64>, stmt: Option<&crate::mem::grant::Reservation>, worker: bool, zeroed: bool) -> Option<RegionBuf> {
    road_region(bytes, w, stmt, worker, zeroed, false, if mempolicy::admit::gate_now() { Asker::Spills } else { Asker::Refuses })
}

/// `chunk_region`'s road for a slot's or a worker share's region G has no room for (`past_g`, `WorkerSlot::bind_region`),
/// its bytes not zeroed: `uninit_region`'s rule holds for such a region as for any the depot maps (the driver zeroes
/// `ZERO_PREFIX` at the slot's open or the worker's first claim, a table zeroes the extent it lays out, a parked region
/// comes back unzeroed). Zeroed, a worker's share past W and G was a memset of the whole share every statement: a W8
/// union-arm sort's 32 MiB (`alloc_zeroed` over the allocator's reused pages), 25% of a 1.7 ms top-N over a nested Append.
pub(crate) fn region_past(bytes: usize, w: Option<&mut u64>, stmt: Option<&crate::mem::grant::Reservation>) -> Option<RegionBuf> {
    road_region(bytes, w, stmt, false, false, false, Asker::Refuses)
}

/// The setop posture (option c'): a `Budget::Pool` side's region, an address-space reservation of `reserve` bytes (pages
/// are the allocator's mapping: untouched ones cost nothing) whose share `bytes` is charged as a slot's or a worker's share
/// is (G, else past G for an admitted statement: `region_past`'s road through `w`); the rest is committed in place as the
/// side's fold needs it (`commit`, `SPILL_REGION_COMMIT`). Parked uncommitted (`RegionBuf::uncommit`, `RegionDepot::put`)
/// and taken back here by a reservation ask only (`RegionDepot::take_reserved`).
///
/// D2894: also a floor-carrying breaker's region (`floor`: `grow::floor_reserve`, its PostgreSQL budget as the reservation,
/// its modeled share charged). The reservation is address space alone (`map_reserved`: PROT_NONE and MAP_NORESERVE until a
/// commit makes its pages writable), so N statements' untouched floors are no commit charge and no RSS.
/// The region of a breaker that starts small (`grant::starts_small`): a parked one whose grown pages stayed
/// resident first (`RegionDepot::take_laid`: the longest at most `reserve`, of a reservation of at least `reserve`), its
/// whole length its start (the taker arms at it), held now without waiting (G, else past G within the pool's room now;
/// refused, it parks again); else a fresh floor reservation of `bytes` (`reserved_region`). Either is marked laid, its
/// table laid at `reserve` (`lay_reserve`) whatever reservation the region was mapped with, and its partitions cut by its
/// taker's `share` (`lay_share`, D3809), whoever opened it before.
pub(crate) fn laid_region(bytes: usize, reserve: usize, share: usize, mut w: Option<&mut u64>, stmt: Option<&crate::mem::grant::Reservation>) -> Result<RegionBuf> {
    // the forced-spill and revoke test faults (`Grant::spill_every`, `revoke_every`) open every laid region cold, at its
    // start: a warm one holds what its last run grew to, so the run would ask for no room and the fault could refuse nothing
    let warm = stmt.is_none_or(|s| s.grant.spill_every == 0 && s.grant.revoke_every == 0);
    if let Some(b) = warm.then(|| crate::park::REGIONS.take_laid(bytes, reserve)).flatten() {
        if let Some(s) = stmt {
            s.region_draw()?;
        }
        if let Some(b) = hold_warm(b, reserve, share, |len| crate::mem::grant::POOL.hold(len, stmt, None).or_else(|| road_held_now(len, w.as_deref_mut(), stmt, false))) {
            return Ok(b);
        }
    }
    let mut b = reserved_region(bytes, reserve, w, stmt, true)?;
    b.set_laid(true);
    (b.lay, b.cut) = (reserve, share);
    Ok(b)
}

/// `laid_region`'s warm take: the parked laid region `b` held for its taker by `hold` (its whole length), laid at `reserve`
/// and cut by `share` (the taker's, not the opener's: a W16 retake of a region a W4 statement opened cuts at W16's share);
/// refused, it parks again and `None` sends the ask to a fresh floor reservation (a start-small ask is never refused
/// because the warm region it found is longer than its taker's room).
fn hold_warm(mut b: RegionBuf, reserve: usize, share: usize, hold: impl FnOnce(usize) -> Option<crate::mem::grant::Held>) -> Option<RegionBuf> {
    b.held = crate::mem::grant::Held::NONE;
    match hold(b.len()) {
        Some(h) => {
            b.held = h;
            b.floor = true;
            b.laid = true;
            (b.lay, b.cut) = (reserve, share);
            Some(b)
        }
        None => {
            crate::park::REGIONS.put(b);
            None
        }
    }
}

pub(crate) fn reserved_region(bytes: usize, reserve: usize, w: Option<&mut u64>, stmt: Option<&crate::mem::grant::Reservation>, floor: bool) -> Result<RegionBuf> {
    let pool = &crate::mem::grant::POOL;
    if let Some(s) = stmt {
        s.region_draw()?;
    }
    // a parked reservation first (`take_reserved`: its share's pages resident), its settled hold given back before the
    // charge below; else a fresh one
    let mut parked = crate::park::REGIONS.take_reserved(bytes, reserve);
    if let Some(b) = parked.as_mut() {
        b.held = crate::mem::grant::Held::NONE;
    }
    let len = parked.as_ref().map_or(bytes, RegionBuf::len);
    let oom = || Error::OutOfMemory { bytes: len as u64, held: pool.held(), capacity: pool.capacity(), why: None };
    let held = match pool.hold(len, stmt, None) {
        Some(h) => h,
        None if stmt.is_some_and(crate::mem::grant::Reservation::admitted) => road_held(len, w, stmt, false).ok_or_else(oom)?,
        None => return Err(oom()),
    };
    reserved_with(parked, bytes, reserve, held, floor).ok_or_else(oom)
}

/// D3226 (c1) for a breaker's reservation (`floor`: a leader's shared slot or a worker's share whose ceiling passes its
/// share, `grow::share_reserve`): `reserved_region`, and on an admitted statement's out of memory the share at the pool's
/// room now (at least `SHARE_FLOOR`, never past `bytes`) inside the same reservation. The caller arms the breaker at the
/// share it holds, so it spills there, or commits in place as the pool frees. Returns the region and that share.
pub(crate) fn reserved_share(bytes: u32, reserve: usize, w: Option<&mut u64>, stmt: Option<&crate::mem::grant::Reservation>, floor: bool) -> Result<(RegionBuf, u32)> {
    let e = match reserved_region(crate::park::region_bytes(bytes as usize), reserve, w, stmt, floor) {
        Ok(buf) => return Ok((buf, bytes)),
        Err(e) => e,
    };
    // D4291: a refusal on the own-spill road goes back to the open first (its own sorts are put before a smaller share)
    let Some(s) = stmt.filter(|s| floor && s.admitted() && !s.own_refused() && matches!(e, Error::OutOfMemory { .. })) else { return Err(e) };
    let pool = &crate::mem::grant::POOL;
    let mut share = room_share(bytes);
    while share >= crate::mem::grant::SHARE_FLOOR as u64 {
        let len = crate::park::region_bytes(share as usize);
        if s.grow_fits(len as u64) && pool.take_trimming(len as u64) {
            s.note_grown(len as u64);
            let Some(buf) = reserved_with(None, len, reserve, crate::mem::grant::Held::pool(len as u64), floor) else { return Err(e) };
            s.note_refused(0);
            OPENED_SMALLER.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            return Ok((buf, (share as u32) & !7));
        }
        share = (share / 2) / crate::park::REGION_MIN as u64 * crate::park::REGION_MIN as u64;
    }
    Err(e)
}

/// `reserved_region`'s region once `held` holds its share: `parked`, else a fresh reservation of `reserve` bytes whose first
/// `bytes` are committed; `None` when not even the share maps.
fn reserved_with(parked: Option<RegionBuf>, bytes: usize, reserve: usize, held: crate::mem::grant::Held, floor: bool) -> Option<RegionBuf> {
    let len = parked.as_ref().map_or(bytes, RegionBuf::len);
    let mut buf = match parked {
        Some(b) => b,
        // D2894: a fresh reservation is address space alone (`map_reserved`: PROT_NONE + MAP_NORESERVE past the share)
        None => match (reserve > bytes).then(|| RegionBuf::map_reserved(bytes, reserve)).flatten() {
            Some(mut b) => {
                advise_huge(b.as_mut_ptr(), b.reserve);
                b
            }
            // the reservation refused (a strict overcommit policy, a small address space): the share alone, as today (not
            // reserved: the fold lays out at the share and `region_commit` answers 0)
            None => RegionBuf::alloc(bytes, false)?, // alloc: Exec
        },
    };
    buf.held = held;
    buf.floor = floor && buf.is_reserved();
    buf.laid = false;
    if region_debug() & POISON != 0 {
        // SAFETY: `buf` holds `len` u8 (the share; the reservation's rest is laid by the fold before it is read)
        unsafe { core::ptr::write_bytes(buf.as_mut_ptr(), 0xA5, len) };
    }
    if buf.is_floor() {
        fires::fire_shared("breaker.floor_reserve"); // shapecheck: a breaker binds in a reservation of its PostgreSQL floor, its modeled share charged (D2894)
    } else if buf.is_reserved() {
        fires::fire_shared("setop.reserve"); // shapecheck: a set operation side binds in a reservation of its rows bound (option c')
    }
    Some(buf)
}

/// What holds `bytes` drawn past a region's share (option c''s `commit`; `road_region`'s charge with no allocation): W first,
/// else G, else for an admitted statement (or a worker) pool bytes reserved past both, counted as grown (D225, D254). `None`
/// when the pool refuses within its wait (the refusal noted on the statement, D2845).
pub(crate) fn road_held(bytes: usize, w: Option<&mut u64>, stmt: Option<&crate::mem::grant::Reservation>, worker: bool) -> Option<crate::mem::grant::Held> {
    road_held_with(bytes, w, stmt, worker, true)
}

/// D2894: `road_held` for a commit whose refusal spills (a kind-7 build's `grow_in`, a merge's `State::grow_in`): pool bytes
/// past W and G only if the pool has them now (no wait, no refusal noted on the statement); `None` spills at once, as a
/// refused `grow_in` did before.
pub(crate) fn road_held_now(bytes: usize, w: Option<&mut u64>, stmt: Option<&crate::mem::grant::Reservation>, worker: bool) -> Option<crate::mem::grant::Held> {
    road_held_with(bytes, w, stmt, worker, false)
}

/// An EXACT draw of counted bytes (a merge's owners know what they hold before they fold it), taken once: W, else G,
/// else charged to the pool directly, waiting for its room (F3, `take_wait`), within the statement's part of the pool
/// (`past_cap`, asked once: no retry follows a refusal). `None`: past that part, or the pool stayed short through its
/// wait; the caller takes the kind's road (the owner phase as before: its owners' mapped temp file), never an error.
pub(crate) fn road_held_exact(bytes: usize, mut w: Option<&mut u64>, stmt: Option<&crate::mem::grant::Reservation>) -> Option<crate::mem::grant::Held> {
    let w_fits = w.as_deref().is_some_and(|c| *c >= bytes as u64);
    let g_fits = stmt.is_some_and(|s| s.left() >= bytes as u64);
    match mempolicy::admit::road(stmt.is_some_and(crate::mem::grant::Reservation::admitted), w_fits, g_fits) {
        Road::W => {
            if let Some(c) = w.as_deref_mut() {
                *c -= bytes as u64;
            }
            return Some(crate::mem::grant::Held::pool(bytes as u64));
        }
        // D4043: a G road the purse decided and the section's workers drew before the take goes past G (`purse_missed`)
        Road::G => match crate::mem::grant::POOL.hold(bytes, stmt, None) {
            Some(h) => return Some(h),
            None if purse_missed(stmt, bytes as u64) => {}
            None => return None,
        },
        Road::PastG => {}
    }
    // no refusal is kept on the statement: its owner phase takes its road, and a kept one would name its next scratch
    // refusal out of memory (`Entry`'s `pool_refused`, as `own_past_grant`'s pin keeps none)
    // ladder step 1.1 (`gate_now`): the gate's `Spills` asker (its road is the owners' temp file); off, `take_wait`
    if past_cap(stmt, false, bytes as u64) {
        crate::mem::grant::note_refusal(mcx::ladder_stats::Why::PastCap, bytes as u64);
        return None;
    }
    if !crate::mem::grant::POOL.take_gate(bytes as u64, false, mcx::ladder::Gate::Draw, Asker::Spills) {
        return None;
    }
    let past = stmt.map(|s| s.note_grown(bytes as u64));
    Some(crate::mem::grant::Held::grown(bytes as u64, past))
}

fn road_held_with(bytes: usize, w: Option<&mut u64>, stmt: Option<&crate::mem::grant::Reservation>, worker: bool, wait: bool) -> Option<crate::mem::grant::Held> {
    let w_fits = w.as_deref().is_some_and(|c| *c >= bytes as u64);
    road_held_on(road_of(bytes as u64, w_fits, stmt, worker), bytes, w, stmt, worker, wait)
}

/// A draw's road: W, G (`left`: G's room and its purse's bytes), else past both for an admitted statement or a worker.
fn road_of(bytes: u64, w_fits: bool, stmt: Option<&crate::mem::grant::Reservation>, worker: bool) -> Road {
    let admitted = worker || stmt.is_some_and(crate::mem::grant::Reservation::admitted);
    mempolicy::admit::road(admitted, w_fits, stmt.is_some_and(|s| s.left() >= bytes))
}

/// `road_held_with` on `road` (`road_of`, read before the take). D4043: a G road the purse decided, which the section's
/// workers drew before the take, goes on past G (`purse_missed`).
fn road_held_on(road: Road, bytes: usize, w: Option<&mut u64>, stmt: Option<&crate::mem::grant::Reservation>, worker: bool, wait: bool) -> Option<crate::mem::grant::Held> {
    if road != Road::PastG {
        let held = held_from(bytes, w, stmt);
        if held.is_some() || !purse_missed(stmt, bytes as u64) {
            if held.is_none() {
                crate::mem::grant::note_refusal(mcx::ladder_stats::Why::Hold, bytes as u64);
            }
            return held;
        }
    }
    if let Some(h) = worker_purse(worker, bytes as u64) {
        return Some(h);
    }
    if past_cap(stmt, worker, bytes as u64) {
        crate::mem::grant::note_refusal(mcx::ladder_stats::Why::PastCap, bytes as u64);
        if wait {
            refused_or_own(stmt, worker, bytes);
        }
        return None;
    }
    let pool = &crate::mem::grant::POOL;
    // D4291: on an open's own-spill road the depots alone (no wait): the open puts its own sorts and retries the ladder
    let waits = wait && !own_first(stmt, worker);
    if !(if waits { pool.take_wait(bytes as u64, worker) } else { pool.take_trimming(bytes as u64) }) {
        // D4914: `take_wait` notes its own branch
        if !waits {
            crate::mem::grant::note_refusal(mcx::ladder_stats::Why::NoWait, bytes as u64);
        }
        if wait {
            refused_or_own(stmt, worker, bytes);
        }
        return None;
    }
    Some(grown_hold(bytes as u64, stmt, worker))
}

/// D4291 (D4287's fix: a finished sort spills before the open waits): a leader's draw past W and G inside a pipeline open whose
/// statement holds sealed sort runs no reader has begun (`Node::own_spill_armed`). Its ladder is the depots (no I/O), then
/// those runs put to their temp files (`Node::put_own_sorts`, at the open: the refusal ends the open, which retries once
/// through the whole ladder), then GOVWAIT's revoke and wait.
fn own_first(stmt: Option<&crate::mem::grant::Reservation>, worker: bool) -> bool {
    let own = stmt.is_some_and(crate::mem::grant::Reservation::own_try);
    mempolicy::admit::past_ladder(own, worker) == mempolicy::admit::PastLadder::DepotsThenOwnSpill
}

/// D4291: a refused draw on the own-spill road (`own_first`) is noted for the open's retry, with no refusal kept for a
/// kernel to name; any other is `refused`.
fn refused_or_own(stmt: Option<&crate::mem::grant::Reservation>, worker: bool, bytes: usize) {
    match stmt.filter(|_| own_first(stmt, worker)) {
        Some(s) => s.note_own_refused(),
        None => refused(stmt, worker, bytes),
    }
}

/// D4043 (the leader-road race): a draw whose road read G (`left`) and that `GrantPool::hold` refused, where G's room alone
/// does not hold `bytes`: `left` counted the purse's bytes and the section's workers drew them before the take
/// (`PURSE_MISSES`). Before the purse its road was past G (`admit::road` on G's room): it takes that road, so a purse drained
/// between the read and the take costs nothing over it.
fn purse_missed(stmt: Option<&crate::mem::grant::Reservation>, bytes: u64) -> bool {
    stmt.is_some_and(|s| s.admitted() && s.g_left() < bytes)
}

/// pagepool P3 (b) witness: a mapped reservation of `reserve` bytes with its first `bytes` writable (`map_reserved`: the
/// pool's span under PGRUST_POOL_GRAVITY=1), its charged writable bytes (`rw`) beside it.
#[doc(hidden)]
#[cfg(target_os = "linux")]
pub fn reservation_witness(bytes: usize, reserve: usize) -> Option<(RegionBuf, usize)> {
    RegionBuf::map_reserved(bytes, reserve).map(|r| {
        let rw = r.rw;
        (r, rw)
    })
}

/// pagepool P3 (b) witness: `bytes` more of `r`'s reservation committed in place (`commit`, no hold); its charged writable
/// bytes after, `None` when the commit was refused.
#[doc(hidden)]
pub fn commit_witness(r: &mut RegionBuf, bytes: usize) -> Option<usize> {
    r.commit(bytes, crate::mem::grant::Held::NONE).then_some(r.rw)
}

/// pppress card 13 (3) witness: a region of `bytes` from the allocator's road (`RegionBuf::alloc`: the page pool's under
/// PGRUST_POOL_GRAVITY=1, `pool_alloc`), held by nothing, not zeroed.
#[doc(hidden)]
pub fn alloc_witness(bytes: usize) -> Option<RegionBuf> {
    RegionBuf::alloc(bytes, false)
}

/// pppress card 13 (3) witness: a move of a region's pages, run by `region_move_witness`.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub enum RegionMove {
    /// `fit_to(bytes)` (an allocator region).
    Fit(usize),
    /// `park_free`, then `unparked` (`Unpark`).
    ParkFree,
    Unpark,
    /// `commit(bytes)` with no hold (a reservation).
    Commit(usize),
    /// `give_back_committed`, `give_back_past(keep)`, `uncommit` (a reservation).
    GiveBackCommitted,
    GiveBackPast(usize),
    Uncommit,
}

/// pppress card 13 (3) witness: `m` run on `r`; what its charge holds after (its account's bytes for it,
/// `Charge::committed`) and whether the move was taken (`fit_to`, `commit`; true for the rest).
#[doc(hidden)]
pub fn region_move_witness(r: &mut RegionBuf, m: RegionMove) -> (u64, bool) {
    let done = match m {
        RegionMove::Fit(b) => r.fit_to(b),
        RegionMove::ParkFree => {
            r.park_free();
            true
        }
        RegionMove::Unpark => {
            r.unparked();
            true
        }
        RegionMove::Commit(b) => r.commit(b, crate::mem::grant::Held::NONE),
        RegionMove::GiveBackCommitted => {
            r.give_back_committed();
            true
        }
        RegionMove::GiveBackPast(keep) => {
            r.give_back_past(keep);
            true
        }
        RegionMove::Uncommit => {
            r.uncommit();
            true
        }
    };
    (r.charge.committed(), done)
}

/// pppress card 13 (3) witness: what `r`'s charge holds (its account's bytes for it, `Charge::committed`).
#[doc(hidden)]
pub fn charge_witness(r: &RegionBuf) -> u64 {
    r.charge.committed()
}

/// D4043's leader-road race made to happen: a leader's `road_held` of `bytes` on `stmt` with `between` run after its road is
/// read and before the take (the section's workers drawing the purse). The draw, or `None` when it was refused.
#[doc(hidden)]
pub fn leader_road_race_witness(bytes: usize, stmt: &crate::mem::grant::Reservation, between: impl FnOnce()) -> Option<crate::mem::grant::Held> {
    let road = road_of(bytes as u64, false, Some(stmt), false);
    between();
    road_held_on(road, bytes, None, Some(stmt), false, true)
}

/// `asker` (ladder step 1.1, `gate_now`): `Spills` = the caller maps its temp file past the pool, so a miss there keeps no
/// refusal on the statement; always `Refuses` with the knob off.
fn road_region(bytes: usize, mut w: Option<&mut u64>, stmt: Option<&crate::mem::grant::Reservation>, worker: bool, zeroed: bool, now: bool, asker: Asker) -> Option<RegionBuf> {
    let admitted = worker || stmt.is_some_and(crate::mem::grant::Reservation::admitted);
    let fits_g = |n: u64| stmt.is_some_and(|s| s.left() >= n);
    if let Some(mut buf) = crate::park::REGIONS.take_parked(bytes, None, None) {
        let n = buf.capacity() as u64;
        let w_fits = w.as_deref().is_some_and(|c| *c >= n);
        match mempolicy::admit::parked_road(admitted, w_fits, fits_g(n), fits_g(bytes as u64)) {
            Road::W => {
                if let Some(c) = w.as_deref_mut() {
                    *c -= n;
                }
                crate::mem::grant::POOL.release(n as usize);
                return Some(buf);
            }
            // D3107: past G only when the ask itself does not fit G either (`parked_road`); an ask G holds draws fresh
            // from G below (a worker's slot has no statement: its road is unchanged)
            Road::PastG if !past_cap(stmt, worker, n) => {
                match stmt.filter(|_| !worker) {
                    Some(s) => buf.held.count_past(s.note_grown(n), n),
                    // G1-B: a worker's shows on its claim's memory ledger account
                    None if worker => buf.held.show_worker(n),
                    None => {}
                }
                return Some(buf);
            }
            _ if !worker && buf.recharge(stmt, None) => return Some(buf),
            _ => crate::park::REGIONS.put(buf),
        }
    }
    let w_fits = w.as_deref().is_some_and(|c| *c >= bytes as u64);
    if mempolicy::admit::road(admitted, w_fits, fits_g(bytes as u64)) != Road::PastG {
        match held_from(bytes, w.as_deref_mut(), stmt) {
            Some(h) => {
                return held_region(bytes, h, zeroed).or_else(|| {
                    if !now {
                        refused(stmt, worker, bytes);
                    }
                    None
                })
            }
            // D4043: the purse decided G's road and the section's workers drew it before the take
            None if purse_missed(stmt, bytes as u64) => {}
            None => {
                if !now {
                    refused(stmt, worker, bytes);
                }
                return None;
            }
        }
    }
    if let Some(h) = worker_purse(worker, bytes as u64) {
        return held_region(bytes, h, zeroed);
    }
    if past_cap(stmt, worker, bytes as u64) {
        if !now {
            refused_or_own(stmt, worker, bytes);
        }
        return None;
    }
    if region_debug() & ARENA != 0 {
        eprintln!("gravity: arena: reserve {bytes} B past W and G ({})", if worker { "worker" } else { "leader" });
    }
    if now {
        if !crate::mem::grant::POOL.take_trimming(bytes as u64) {
            return None;
        }
    } else if own_first(stmt, worker) {
        // D4291: the open's own-spill road: the depots, then the open puts its own sorts and retries (`own_first`)
        if !crate::mem::grant::POOL.take_trimming(bytes as u64) {
            refused_or_own(stmt, worker, bytes);
            return None;
        }
    } else if !crate::mem::grant::POOL.take_gate(bytes as u64, worker, mcx::ladder::Gate::Draw, asker) {
        if asker == Asker::Refuses {
            refused(stmt, worker, bytes);
        }
        return None;
    }
    held_region(bytes, grown_hold(bytes as u64, stmt, worker), zeroed)
}

/// D4043: a worker's draw of `bytes` past its W from its statement's reserved-but-unused bytes (the claim's purse,
/// `grant::worker_purse_draw`) before the pool's capacity: a worker has no statement (D187), so its draw never reached G's
/// room and a statement whose reservation filled the pool refused its own workers. A leader's road reads the purse in
/// `left` (G's room) and draws it in `GrantPool::hold`.
fn worker_purse(worker: bool, bytes: u64) -> Option<crate::mem::grant::Held> {
    if !worker {
        return None;
    }
    crate::mem::grant::worker_purse_draw(bytes)
}

/// A draw of `bytes` past W and G the pool granted: a leader's counted on its statement (`Reservation::note_grown`), a
/// worker's shown on its claim's memory ledger account (G1-B: the ledger's pool bytes alone; a worker has no statement count).
fn grown_hold(bytes: u64, stmt: Option<&crate::mem::grant::Reservation>, worker: bool) -> crate::mem::grant::Held {
    if worker {
        return crate::mem::grant::Held::grown_worker(bytes);
    }
    crate::mem::grant::Held::grown(bytes, stmt.map(|s| s.note_grown(bytes)))
}

/// D4329: a hosted build's draws (`scratch_draw`), their bytes, and the ones taken as an overdraft past the pool, this
/// process.
pub static SCRATCH_DRAWS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static SCRATCH_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static SCRATCH_PAST: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// D4329: `bytes` of a hosted build's working memory (a verbatim part's dictionary build: heap outside every region, freed
/// at the build's end) charged to the grant pool before the build grows, on any thread: the claim's purse (its statement's
/// reserved-but-unused bytes), else the pool's room (`take_trimming`: the idle depots trimmed first, no wait) while the
/// statement's part of the pool holds it, else an overdraft past the pool (`Overdraw::Scratch`). Never refused, and with
/// `pgrust.memory_ladder` off never waiting (on: no room climbs the ladder and waits, `take_wait`, and the overdraft comes
/// only once that wait's budget ends; LADDER-DESIGN section 3): PostgreSQL holds no such memory, so a refusal would be a
/// 53200 it never raises; the bytes held in the pool are
/// what admission and the statements beside it see. The hold gives the bytes back at its drop. Pool bytes only: the bytes
/// themselves are the allocator's, already on the building thread's account (door 1), so nothing is shown on the ledger
/// twice.
pub fn scratch_draw(bytes: u64) -> crate::mem::grant::Held {
    use core::sync::atomic::Ordering::Relaxed;
    SCRATCH_DRAWS.fetch_add(1, Relaxed);
    SCRATCH_BYTES.fetch_add(bytes, Relaxed);
    if let Some(h) = crate::mem::grant::worker_purse_draw(bytes) {
        return h;
    }
    // D4324 step 3: with the ladder on, no room climbs and waits (`take_wait`); the overdraft only once its budget ends
    let pool = &crate::mem::grant::POOL;
    if !past_cap(None, true, bytes) && (if mcx::ladder::on() { pool.take_wait_at(bytes, true, mcx::ladder::Gate::Scratch) } else { pool.take_trimming(bytes) }) {
        return crate::mem::grant::Held::pool(bytes);
    }
    SCRATCH_PAST.fetch_add(1, Relaxed);
    crate::mem::grant::POOL.overdraw_pool(bytes, crate::mem::grant::Overdraw::Scratch)
}

/// D4373 (b): `bytes` of a put's heap copy (a sort merge's put form, exec `put_over` / `put_form`; a spilled run's coded
/// copy, `runcodec::put_run`) held in the grant pool before the copy is allocated: the pool's room now (the idle depots
/// trimmed first, no wait), else `None`: the caller takes its road past the pool (the compact copy in the statement's
/// mapped temp file, `copy_map`; the run written uncoded), never a refusal. The hold gives the bytes back at its drop.
/// Pool bytes only, as `scratch_draw`'s: the copy is the allocator's, already on the thread's account.
pub fn scratch_take(bytes: u64) -> Option<crate::mem::grant::Held> {
    crate::mem::grant::POOL.take_trimming(bytes).then(|| crate::mem::grant::Held::pool(bytes))
}

/// D4373 (b): `scratch_take`'s road past the pool for a put's compact copy: `bytes` of a mapped temp file (the statement's
/// spill road, `spill::map_temp`); `None` with no temp-file road (the caller copies through the allocator, as before).
pub fn copy_map(bytes: u64) -> crate::Result<Option<crate::mem::spill::TempMap>> {
    crate::mem::spill::map_temp(bytes as usize)
}

/// D2990: a draw of `bytes` past W and G past one statement's part of the pool (`Reservation::grow_fits`; a worker's by its
/// leader's account, D3982): refused at once, never waited on (the cap does not move while the statement runs).
fn past_cap(stmt: Option<&crate::mem::grant::Reservation>, worker: bool, bytes: u64) -> bool {
    if worker {
        // D4373 (a): a worker's draw past W and G is charged on its statement's memory ledger (`Held::show_worker`), so it
        // meets that ledger's limit as the leader's does (`Reservation::grow_fits`): refused there, the kernel's road runs
        return crate::mem::grant::worker_past_cap(bytes) || !mcx::governor::Charge::new("gravity worker grown", 0).fits(bytes as usize);
    }
    stmt.is_some_and(|s| !s.grow_fits(bytes))
}

/// D4373's witness: a parallel worker's draw of `bytes` past W and G on this thread, on the road a sort run's in-place
/// growth takes (`grow_in_place` -> `road_held_now`); `None` refused.
#[doc(hidden)]
pub fn worker_grown_draw(bytes: u64) -> Option<crate::mem::grant::Held> {
    road_held_now(bytes as usize, None, None, true)
}

/// D3982's witness: `past_cap` for a parallel worker's draw of `bytes` on this thread.
#[doc(hidden)]
pub fn worker_draw_past_cap(bytes: u64) -> bool {
    past_cap(None, true, bytes)
}

/// D2845: a leader's draw of `bytes` the pool (or the allocator) refused, kept on its statement: the kernel that asked reads
/// the refusal as its scratch running out, and `Entry::run_pipeline` names it out of memory.
fn refused(stmt: Option<&crate::mem::grant::Reservation>, worker: bool, bytes: usize) {
    if let Some(s) = stmt.filter(|_| !worker) {
        s.note_refused(bytes as u64);
        crate::mem::feedback::note_refusal();
    }
}

/// D285: a slot region G has no room for (an admitted statement grown past it: an owner re-placed, T1 growth) takes D275's
/// road, reserved past G and counted as grown (`chunk_region`), with no life; `e` (the refusal) for any other statement.
pub(crate) fn past_g(bytes: u32, stmt: Option<&crate::mem::grant::Reservation>, e: Error) -> Result<RegionBuf> {
    match e {
        Error::OutOfMemory { .. } if stmt.is_some_and(crate::mem::grant::Reservation::admitted) => region_past(crate::park::region_bytes(bytes as usize), None, stmt).ok_or(e),
        e => Err(e),
    }
}

/// D3226 (c1): `past_g` for a share-budgeted breaker region (a leader's shared slot, a worker's share). Its refusal on an
/// admitted statement opens the region at the room the pool has now, at least `SHARE_FLOOR` (the least share `cut_share`
/// opens), never past `bytes`, and the caller arms the breaker at the region's length, so it spills at it as a cut share
/// does. The full pool at a region's open was the statement's 53200 (two W8 statements at once: the 8th worker share's
/// 874 MiB).
/// Returns the region and the share bytes it holds; the refusal when not even `SHARE_FLOOR` is free.
pub(crate) fn past_g_share(bytes: u32, stmt: Option<&crate::mem::grant::Reservation>, e: Error) -> Result<(RegionBuf, u32)> {
    match past_g(bytes, stmt, e) {
        Ok(buf) => Ok((buf, bytes)),
        Err(e) => share_at_room(bytes, stmt, e),
    }
}

/// `past_g_share`'s road once the past-G road refused `bytes` (`e`): the region at the pool's room now, or `e`.
pub(crate) fn share_at_room(bytes: u32, stmt: Option<&crate::mem::grant::Reservation>, e: Error) -> Result<(RegionBuf, u32)> {
    // D4291: as `reserved_share`: the own-spill road's refusal goes back to the open first
    let Some(s) = stmt.filter(|s| s.admitted() && !s.own_refused() && matches!(e, Error::OutOfMemory { .. })) else { return Err(e) };
    let pool = &crate::mem::grant::POOL;
    let mut share = room_share(bytes);
    while share >= crate::mem::grant::SHARE_FLOOR as u64 {
        let len = crate::park::region_bytes(share as usize);
        if s.grow_fits(len as u64) && pool.take_trimming(len as u64) {
            s.note_grown(len as u64);
            if let Some(buf) = held_region(len, crate::mem::grant::Held::pool(len as u64), false) {
                // the refused draw is served: a later scratch refusal is not named after it (D2845)
                s.note_refused(0);
                OPENED_SMALLER.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                return Ok((buf, (share as u32) & !7));
            }
            return Err(e);
        }
        share = (share / 2) / crate::park::REGION_MIN as u64 * crate::park::REGION_MIN as u64;
    }
    Err(e)
}

/// The pool's room now for a share of `bytes`, at most `bytes`, in whole `REGION_MIN`s.
fn room_share(bytes: u32) -> u64 {
    let pool = &crate::mem::grant::POOL;
    let limit = pool.capacity().saturating_sub(pool.reserve()).saturating_add(pool.small_held());
    (limit.saturating_sub(pool.held()).min(bytes as u64) as usize / crate::park::REGION_MIN * crate::park::REGION_MIN) as u64
}

/// D3226 (c1): the share-budgeted regions opened at the pool's room below their share (`past_g_share`, `reserved_share`),
/// this process.
pub static OPENED_SMALLER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// `bytes` from W, else from G (`GrantPool::hold`: G's room, past it the statement's purse).
fn held_from(bytes: usize, w: Option<&mut u64>, stmt: Option<&crate::mem::grant::Reservation>) -> Option<crate::mem::grant::Held> {
    match w {
        Some(c) if *c >= bytes as u64 => {
            *c -= bytes as u64;
            Some(crate::mem::grant::Held::pool(bytes as u64))
        }
        _ => crate::mem::grant::POOL.hold(bytes, stmt, None),
    }
}

/// `bytes` held by `held`: zeroed (an arena chunk, on lazy zero pages when fresh), else as `uninit_region` maps a region.
fn held_region(bytes: usize, held: crate::mem::grant::Held, zeroed: bool) -> Option<RegionBuf> {
    let mut buf = RegionBuf::alloc(bytes, zeroed)?; // alloc: Exec
    buf.held = held;
    if !zeroed {
        if region_debug() & POISON != 0 {
            // SAFETY: `buf` holds `bytes` u8; as `uninit_region`'s poison.
            unsafe { core::ptr::write_bytes(buf.as_mut_ptr(), 0xA5, bytes) };
        }
        if !buf.pool {
            advise_huge(buf.as_mut_ptr(), bytes);
        }
    }
    Some(buf)
}

/// The bytes of a share-budgeted region the driver zeroes before arming it (the table headers). M7-SPILL (V4R-359): 128
/// covers a hash build's header words through its spill words (lowerv4 ops/hashjoin/spill/build.rs `SPILL` / `PBITS` /
/// `SPILLED`, 112..128), which a build that never spills does not store: they read 0 at every life's start. A header
/// that grows past this prefix does not build: build.rs const-asserts `SPILLED + 8 <= ZERO_PREFIX`.
pub const ZERO_PREFIX: usize = 128;

/// A breaker region's buffer, not zeroed: the driver zeroes `ZERO_PREFIX` at every open and a table
/// zeroes the extent it lays out (a zeroed allocation re-memsets the whole share on every fresh backend —
/// 1 GB per statement of a 256 MB `work_mem` join at W = 16, the pass-3 fresh-session finding; the
/// frames' 40 MiB program-constant regions the same, 17 per statement). The allocation is the
/// allocator's mapping: pages are zero until touched, so a region costs what its table lays out.
/// A region of a huge page or more (`page_sizes`) asks for transparent huge pages (Graviton, §4b (d): THP is
/// `madvise` on the box, the fold's dependent probes pay the page walk otherwise).
pub(crate) fn uninit_region(bytes: usize, stmt: Option<&crate::mem::grant::Reservation>, life: Option<&std::rc::Rc<crate::mem::grant::StmtCtx>>) -> Result<RegionBuf> {
    region_held_by(bytes, |pool| pool.hold(bytes, stmt, life))
}

/// D4920: a hashed IN list's set the Bind builds before any statement exists (lowerv4 `saop_set`, `saop_bytes` and
/// `saop_extreme`, only they): mapped as `try_region` maps it and held on the pool as a draw with no statement (D4403's
/// small buffer first), but no L1 (`GrantPool::hold_in_list_set`): under `PGRUST_GRAVITY_LEDGER=abort` a statement with
/// such a list no longer panics its backend at admission. Its charge is the Bind's context owner's, the session's once the
/// plan memo files the admission (`RegionBuf::adopt`, D4844); the pool's bytes come back when the set drops.
pub fn in_list_set_region(bytes: usize) -> Result<RegionBuf> {
    region_held_by(bytes, |pool| pool.hold_in_list_set(bytes))
}

/// `uninit_region`'s mapping, its bytes held on the pool by `hold`.
fn region_held_by(bytes: usize, hold: impl FnOnce(&crate::mem::grant::GrantPool) -> Option<crate::mem::grant::Held>) -> Result<RegionBuf> {
    let pool = &crate::mem::grant::POOL;
    let oom = || Error::OutOfMemory { bytes: bytes as u64, held: pool.held(), capacity: pool.capacity(), why: None };
    // every byte is written (the prefix by the driver, the tables' extents by the tables) before a kernel or the merge reads it
    let mut v = if bytes == 0 { RegionBuf::empty() } else { RegionBuf::alloc(bytes, false).ok_or_else(oom)? };
    v.held = hold(pool).ok_or_else(oom)?;
    if region_debug() & POISON != 0 {
        // SAFETY: `v` holds `bytes` u8; a table that reads a byte it never wrote now sees 0xA5, not the mapping's zero.
        unsafe { core::ptr::write_bytes(v.as_mut_ptr(), 0xA5, bytes) };
    }
    if !v.pool {
        advise_huge(v.as_mut_ptr(), bytes);
    }
    Ok(v)
}

pub(crate) const POISON: u8 = 1;

pub(crate) const TRACE: u8 = 2;

/// `arena`: one line per statement end, the arena chunks the leader and each worker hold against G (the S4 census).
pub(crate) const ARENA: u8 = 4;

/// `PGRUST_GRAVITY_REGION_DEBUG` (unset in every server of record): `poison` fills fresh regions with 0xA5 (stale
/// reads made deterministic, D103), `trace` logs slot opens and depot takes/puts to stderr; read once per process.
pub(crate) fn region_debug() -> u8 {
    static ON: pgsync::OnceLock<u8> = pgsync::OnceLock::new();
    *ON.get_or_init(|| {
        let v = crate::env::text("PGRUST_GRAVITY_REGION_DEBUG").unwrap_or_default(); // alloc: Cold
        v.split(',').fold(0, |m, w| m | match w.trim() { "poison" => POISON, "trace" => TRACE, "arena" => ARENA, _ => 0 })
    })
}

/// The kernel's page and transparent-huge-page sizes, read once per process: (page, huge). `huge` is the
/// PMD size the kernel reports (2 MiB under a 4 KiB base page, 512 MiB under a 64 KiB one), 0 when the
/// kernel has no THP (no advice is given then). A region at least `huge` long asks for huge pages.
#[cfg(target_os = "linux")]
pub(crate) fn page_sizes() -> (usize, usize) {
    extern "C" {
        fn sysconf(name: core::ffi::c_int) -> core::ffi::c_long;
    }
    const SC_PAGESIZE: core::ffi::c_int = 30; // _SC_PAGESIZE on Linux (glibc and musl, every arch)
    static SIZES: pgsync::OnceLock<(usize, usize)> = pgsync::OnceLock::new();
    *SIZES.get_or_init(|| {
        // SAFETY: sysconf is a pure read.
        let page = usize::try_from(unsafe { sysconf(SC_PAGESIZE) }).ok().filter(|p| p.is_power_of_two()).unwrap_or(4096);
        let huge = std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/hpage_pmd_size") // alloc: Cold
            .ok()
            .and_then(|t| t.trim().parse::<usize>().ok())
            .filter(|h| h.is_power_of_two() && *h > page)
            .unwrap_or(0);
        (page, huge)
    })
}

/// Off Linux no region is a reserved mapping (`rw` stays 0, so `commit` never rounds to a page) and no huge-page advice
/// is given: the base page only names a rounding unit.
#[cfg(not(target_os = "linux"))]
pub(crate) fn page_sizes() -> (usize, usize) {
    (4096, 0)
}

/// Whether the advice was given (false: too small, no THP, or a page pool span).
#[cfg(target_os = "linux")]
pub(crate) fn advise_huge(ptr: *mut u8, bytes: usize) -> bool {
    extern "C" {
        fn madvise(addr: *mut core::ffi::c_void, len: usize, advice: core::ffi::c_int) -> core::ffi::c_int;
    }
    const MADV_HUGEPAGE: core::ffi::c_int = 14;
    let (page, huge) = page_sizes();
    // a page pool span carries the pool's own advice (it maps its superpages huge): none here, so no madvise on the
    // query thread that binds it (a pool reservation's, or a region of the pool as the global allocator)
    if huge == 0 || bytes < huge || pagepool::owns(ptr) {
        return false;
    }
    let lo = (ptr as usize).next_multiple_of(page);
    let hi = (ptr as usize + bytes) / page * page;
    if hi > lo {
        // SAFETY: a page-aligned range inside the allocation; the advice never changes the bytes.
        unsafe { madvise(lo as *mut core::ffi::c_void, hi - lo, MADV_HUGEPAGE) };
    }
    hi > lo
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn advise_huge(_ptr: *mut u8, _bytes: usize) -> bool {
    false
}

/// pagepool P3 witness: `advise_huge` over `[ptr, ptr + bytes)`, whether it advised.
#[doc(hidden)]
pub fn advise_huge_witness(ptr: *mut u8, bytes: usize) -> bool {
    advise_huge(ptr, bytes)
}

/// D641: a region's pages returned to the kernel (the range reads zero after, as after MADV_DONTNEED), the allocation kept.
/// Through `pagegive` (CVE-2026-74674: never one MADV_DONTNEED over a whole 2 MiB-aligned range). An allocator region and a
/// reservation are both NORESERVE mappings under overcommit, and a region big enough to hold a whole 2 MiB range was advised
/// huge at its allocation (`advise_huge`).
#[cfg(target_os = "linux")]
pub(crate) fn dontneed(ptr: *mut u8, bytes: usize) {
    const PAGE: usize = 1 << 16; // inward to 64 KiB: page-aligned under a 4 KiB or a 64 KiB kernel page
    let lo = (ptr as usize).next_multiple_of(PAGE);
    let hi = (ptr as usize + bytes) / PAGE * PAGE;
    if hi > lo {
        // SAFETY: an aligned range inside the caller's allocation, about to be freed; its bytes are dead. Named the pool's
        // when the pool owns it (the pool as the global allocator), else the global allocator's, as before.
        unsafe { pagepool::give_back_any(lo, hi - lo, true) };
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn dontneed(_ptr: *mut u8, _bytes: usize) {}

/// D2687: a region's pages handed back lazily (MADV_FREE: the kernel takes them only under memory pressure, and a write
/// before that keeps the page without a fault), the allocation kept for the depot. A kernel without MADV_FREE refuses the
/// advice and the pages stay resident: still within the park budget the depot holds them to.
#[cfg(target_os = "linux")]
pub(crate) fn free_lazy(ptr: *mut u8, bytes: usize) {
    extern "C" {
        fn madvise(addr: *mut core::ffi::c_void, len: usize, advice: core::ffi::c_int) -> core::ffi::c_int;
    }
    const MADV_FREE: core::ffi::c_int = 8;
    let (lo, hi) = inward(ptr, bytes);
    if hi > lo {
        // SAFETY: an aligned range inside the caller's allocation; its bytes are dead (a parked region is capacity only).
        unsafe { madvise(lo as *mut core::ffi::c_void, hi - lo, MADV_FREE) };
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn free_lazy(_ptr: *mut u8, _bytes: usize) {}

/// The whole pages inside `[ptr, ptr + bytes)`, inward to 64 KiB: page-aligned under a 4 KiB or a 64 KiB kernel page.
fn inward(ptr: *const u8, bytes: usize) -> (usize, usize) {
    const PAGE: usize = 1 << 16;
    ((ptr as usize).next_multiple_of(PAGE), (ptr as usize + bytes) / PAGE * PAGE)
}

/// D2687's debug-build check (no release code): the pages of a spill region parked at its statement's end are
/// inaccessible (PROT_NONE) while it sits in the depot, so a holder that kept its address past the statement's end
/// faults at its first read or write instead of seeing bytes the next taker owns. `unguard` restores them when the
/// region leaves the depot (a take) or is freed (an eviction, any drop).
#[cfg(all(debug_assertions, target_os = "linux"))]
static GUARDED: pgsync::Mutex<Vec<(usize, usize)>> = pgsync::Mutex::new(Vec::new()); // alloc: Cold

#[cfg(all(debug_assertions, target_os = "linux"))]
const PROT_NONE: core::ffi::c_int = 0;
#[cfg(all(debug_assertions, target_os = "linux"))]
const PROT_READ_WRITE: core::ffi::c_int = 3;

#[cfg(all(debug_assertions, target_os = "linux"))]
fn protect(lo: usize, hi: usize, prot: core::ffi::c_int) -> bool {
    extern "C" {
        fn mprotect(addr: *mut core::ffi::c_void, len: usize, prot: core::ffi::c_int) -> core::ffi::c_int;
    }
    // SAFETY: whole pages inside one live allocation (`inward`); only its protection changes, never its bytes.
    unsafe { mprotect(lo as *mut core::ffi::c_void, hi - lo, prot) == 0 }
}

/// D2687 (debug builds): `buf`'s pages made inaccessible while it is parked.
#[cfg(all(debug_assertions, target_os = "linux"))]
pub(crate) fn guard_parked(buf: &RegionBuf) {
    if buf.pool {
        // pagepool P3: no protection change inside the pool's zones (a split mapping per parked region; the purger remaps
        // the region's dead runs whatever their protection)
        return;
    }
    let (lo, hi) = inward(buf.as_ptr(), buf.len());
    if hi > lo && protect(lo, hi, PROT_NONE) {
        pgsync::lock(&GUARDED).push((lo, hi)); // alloc: Cold
    }
}

#[cfg(not(all(debug_assertions, target_os = "linux")))]
pub(crate) fn guard_parked(_buf: &RegionBuf) {}

/// D2687 (debug builds): `[ptr, ptr + bytes)`'s pages accessible again if `guard_parked` closed them.
#[cfg(all(debug_assertions, target_os = "linux"))]
fn unguard(ptr: *const u8, bytes: usize) {
    let (lo, hi) = inward(ptr, bytes);
    if hi <= lo {
        return;
    }
    let mut guarded = pgsync::lock(&GUARDED);
    if let Some(i) = guarded.iter().position(|&e| e == (lo, hi)) {
        guarded.swap_remove(i);
        assert!(protect(lo, hi, PROT_READ_WRITE), "D2687: a parked spill region's pages could not be made accessible again");
    }
}

#[cfg(not(all(debug_assertions, target_os = "linux")))]
fn unguard(_ptr: *const u8, _bytes: usize) {}

#[cfg(all(test, target_os = "linux"))]
mod page_size_tests {
    /// CR2-09: the huge-page threshold is the kernel's PMD size, not a fixed 2 MiB.
    #[test]
    fn the_huge_threshold_is_the_kernels_pmd_size() {
        let (page, huge) = super::page_sizes();
        assert!(page.is_power_of_two() && page >= 4096);
        let sys = std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/hpage_pmd_size").ok().and_then(|t| t.trim().parse::<usize>().ok());
        assert_eq!(huge, sys.filter(|h| *h > page).unwrap_or(0));
        // a region below the threshold, and any region when the kernel has no THP, takes no advice (never faults)
        let mut v = vec![0u8; 1 << 16]; // alloc: Cold
        super::advise_huge(v.as_mut_ptr(), v.len());
    }
}

#[cfg(test)]
mod reserved_tests {
    /// D4303: the tests that park a laid region and take it back hold this. The depot is process-wide and `take_laid` takes
    /// the longest fitting region, so two of them at once took (and dropped) each other's: the forced-spill test's region
    /// (8 MiB of a 22 MiB reservation) fits the grown test's take (2..8 MiB, reservation 8) and the grown-past-share test's
    /// short take (2..11 MiB, reservation 11).
    static LAID: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn laid_lock() -> std::sync::MutexGuard<'static, ()> {
        LAID.lock().unwrap_or_else(|p| p.into_inner())
    }
    /// D3872: a mapping's writable pages its fold never wrote are off its account's resident bytes after a residency pass,
    /// come back as they are written, and leave with it at its drop (the doors' counts stay what was asked for).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_mappings_unwritten_pages_are_not_resident_on_its_account() {
        const MIB: usize = 1 << 20;
        let a = memledger::Acct::of(memledger::Consumer::CondCache);
        let _on = memledger::charge::enter_as(memledger::Owner::Cache(memledger::Holder::of_acct(a)));
        let nr = || memledger::account_row(a).not_resident_mapped_bytes;
        let before = nr();
        // a shape no other test parks, so the region is a fresh mapping
        let (share, reserve) = (6 * MIB, 96 * MIB + 4096);
        let mut r = super::reserved_region(share, reserve, None, None, false).expect("a reserved region");
        assert!(super::super::resident::pass_all());
        let fresh = nr() - before;
        // at least half: making the region may touch its first huge page
        assert!(fresh >= (share / 2) as i64, "a fresh mapping's writable pages are not resident ({fresh})");
        // SAFETY: the share's bytes are writable (`reserved_region`)
        unsafe { core::ptr::write_bytes(r.as_mut_ptr(), 0x5a, share) };
        assert!(super::super::resident::pass_all());
        assert!(nr() - before < MIB as i64, "written pages are resident ({})", nr() - before);
        drop(r);
        assert_eq!(nr(), before, "its not-resident bytes leave with it");
    }

    /// D4404 (ii): the reaper's landing credit for a mapping is its resident bytes at the residency pass's last read: a
    /// reservation with a gigabyte writable and 100 MiB touched credits about 100 MiB, never its writable gigabyte; one no
    /// pass has read yet credits nothing.
    #[cfg(target_os = "linux")]
    #[test]
    fn d4404_a_mappings_landing_credit_is_its_touched_bytes_not_its_writable_ones() {
        const MIB: usize = 1 << 20;
        let mut r = super::RegionBuf::map_reserved(1024 * MIB, 1024 * MIB + 2 * MIB).expect("a mapped reservation");
        assert_eq!(r.rw, 1024 * MIB, "a gigabyte writable");
        assert_eq!(crate::mem::reap::owed_of(r.res), 0, "a fresh mapping no pass has read: no credit");
        // SAFETY: the first 100 MiB are writable (`map_reserved`)
        unsafe { core::ptr::write_bytes(r.as_mut_ptr(), 0x5a, 100 * MIB) };
        assert!(super::super::resident::pass_all());
        let owed = crate::mem::reap::owed_of(r.res);
        assert!((100 * MIB as u64..=104 * MIB as u64).contains(&owed), "credits the touched 100 MiB, not the writable GiB: {} MiB", owed >> 20);
        drop(r);
    }

    /// LEDGER2 design A (main 09:50:42Z): a mapping's not-resident bytes book on its charge's owner as on its account, so the
    /// owner's resident bytes are its held less them; a move to a breaker slot's operator (`own_by`) takes them along, and
    /// they leave the operator with the region.
    #[cfg(target_os = "linux")]
    #[test]
    fn ledger2_a_regions_not_resident_bytes_follow_its_owner() {
        const MIB: usize = 1 << 20;
        const M: i64 = MIB as i64;
        std::thread::spawn(|| {
            memledger::bind_backend(3995, 9995); // a backend slot no other test binds (4080 and up are UNBOUND)
            memledger::statement_begin();
            let st = memledger::charge::context().0;
            let op = memledger::charge::op_begin(st, memledger::Kind::Sort as u32, 0);
            let mut r = super::RegionBuf::map_reserved(64 * MIB, 64 * MIB + 2 * MIB).expect("a mapped reservation");
            assert_eq!(r.charge.owner(), st, "made on the running statement");
            let u = memledger::charge::usage(st);
            // another test's pass may have read it already: making the region may touch its first huge page
            assert!(u.held == 64 * M && (60 * M..=64 * M).contains(&u.not_resident), "a fresh mapping: (nearly) none of it in memory: {u:?}");
            // SAFETY: the first 16 MiB are writable (`map_reserved`)
            unsafe { core::ptr::write_bytes(r.as_mut_ptr(), 0x5a, 16 * MIB) };
            assert!(super::super::resident::pass_all());
            let u = memledger::charge::usage(st);
            assert!((16 * M..=20 * M).contains(&u.subtree_resident()), "resident = held - not resident: {} MiB", u.subtree_resident() >> 20);
            let nr = u.not_resident;
            r.own_by(op);
            let (us, uo) = (memledger::charge::usage(st), memledger::charge::usage(op));
            assert_eq!((uo.held, uo.not_resident), (64 * M, nr), "the operator's now, its not-resident bytes with it");
            assert_eq!((us.held, us.not_resident, us.subtree_not_resident), (0, 0, nr), "in the statement's subtree only");
            assert_eq!(us.subtree_resident(), uo.subtree_resident());
            drop(r);
            let uo = memledger::charge::usage(op);
            assert_eq!((uo.held, uo.not_resident), (0, 0), "gone with the region (unmapped, or handed to the reaper)");
            memledger::charge::op_end(op);
            let _ = memledger::statement_end();
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }

    /// The setop posture (option c'): a reserved region is charged its share, its whole reservation is writable in place,
    /// and a commit grows what it holds without moving it.
    #[test]
    fn a_reserved_region_commits_in_place() {
        let (share, reserve) = (1usize << 20, 32usize << 20);
        let mut r = super::reserved_region(share, reserve, None, None, false).expect("a reserved region");
        // a parked reservation of this shape may come back (`take_reserved`: its share within an eighth over the ask)
        assert!(r.is_reserved() && r.len() >= share && r.reserve() >= reserve && r.committed() == 0);
        let len = r.len();
        let p = r.as_mut_ptr();
        let held = super::road_held(share, None, None, false).expect("a commit's draw");
        assert!(r.commit(share, held), "a commit within the reservation");
        assert_eq!((r.committed(), r.as_mut_ptr()), (share, p), "committed in place");
        // SAFETY: the reservation's bytes are the allocation's (`reserved_region`); the committed ones are written here
        unsafe { core::ptr::write_bytes(p, 0x5a, share + r.committed()) };
        // the depot parks it with its share only (the committed tail back to the kernel, its hold released): a plain share
        // ask never takes it; a reservation ask of its shape does, at the same address, nothing committed
        crate::park::REGIONS.put(r);
        let plain = crate::park::REGIONS.take_parked(share, None, None);
        assert!(plain.as_ref().is_none_or(|b| !b.is_reserved()), "a plain take never gets a reservation");
        drop(plain);
        let again = crate::park::REGIONS.take_reserved(share, reserve).expect("the parked reservation");
        assert_eq!((again.as_ptr(), again.committed(), again.len(), again.is_reserved()), (p as *const u8, 0, len, true));
    }

    /// A region that started small and grew through its whole reservation parks with its length equal to the reservation
    /// (`uncommit` keeps a laid region's grown pages): it stays a reservation, so a plain take never refits its length
    /// past its writable pages, and a laid take gets it back whole.
    #[test]
    fn a_laid_region_grown_to_its_reservation_stays_a_reservation() {
        let _laid = laid_lock();
        let (start, reserve) = (2usize << 20, 8usize << 20);
        let mut r = super::reserved_region(start, reserve, None, None, true).expect("a floor reservation");
        r.set_laid(true);
        let (p, res, more) = (r.as_mut_ptr(), r.reserve(), r.reserve() - r.len());
        let held = super::road_held_now(more, None, None, false).expect("an unadmitted draw holds pool bytes");
        assert!(r.commit(more, held));
        // SAFETY: the start and the committed bytes are writable
        unsafe { core::ptr::write_bytes(p, 0x3c, r.len() + more) };
        crate::park::REGIONS.put(r);
        for ask in [start, start + 4096, 4usize << 20] {
            let plain = crate::park::REGIONS.take_parked(ask, None, None);
            assert!(plain.as_ref().is_none_or(|b| b.as_ptr() != p as *const u8), "a plain take never gets the full laid region");
        }
        let again = crate::park::REGIONS.take_laid(start, reserve).expect("the parked laid region");
        assert_eq!((again.as_ptr(), again.len(), again.is_reserved(), again.is_laid()), (p as *const u8, res, true, true));
        #[cfg(target_os = "linux")]
        assert!(again.rw >= again.len(), "its whole length is writable");
    }

    /// A laid region taken back for an ask of a smaller reservation answers the ask (`lay_reserve`: its table's layout and
    /// partition count), not the reservation it was mapped with, so the workers of one section lay one partition count
    /// whichever regions they took; a fresh laid region answers its own.
    #[test]
    fn a_laid_take_answers_the_reservation_it_asked() {
        let _laid = laid_lock();
        let (start, big, small) = (2usize << 20, 12usize << 20, 6usize << 20);
        let mut r = super::reserved_region(start, big, None, None, true).expect("a floor reservation");
        r.set_laid(true);
        let p = r.as_mut_ptr();
        crate::park::REGIONS.put(r);
        let again = super::laid_region(start, small, start, None, None).expect("a laid region");
        assert!(again.is_laid());
        if again.as_ptr() == p as *const u8 {
            assert_eq!((again.reserve(), again.lay_reserve()), (big, small), "the parked mapping, laid at the ask");
        }
        assert_eq!(again.lay_reserve(), small);
        let fresh = super::laid_region(start, small, start, None, None).expect("a laid region");
        assert_eq!((fresh.is_laid(), fresh.lay_reserve()), (true, small));
    }

    /// D3794: a laid region that grew past its share in place parks with its grown pages, and the next laid open of a
    /// reservation that holds it takes it back at the same address with its whole length (its share is no bound: a hash
    /// aggregate's worker grows past it, and a share bound mapped a fresh reservation every hot repeat); an ask whose
    /// reservation is shorter than the parked length never takes it (its taker could commit nothing past it).
    #[test]
    fn a_laid_region_grown_past_its_share_opens_warm() {
        let _laid = laid_lock();
        // a reservation no other test asks (the depot is process-wide; a laid take needs a reservation at least the ask's)
        let (start, share, reserve) = (2usize << 20, 4usize << 20, 26usize << 20);
        let mut r = super::reserved_region(start, reserve, None, None, true).expect("a floor reservation");
        r.set_laid(true);
        let (p, more) = (r.as_mut_ptr(), (12usize << 20) - r.len());
        let held = super::road_held_now(more, None, None, false).expect("an unadmitted draw holds pool bytes");
        assert!(r.commit(more, held));
        // SAFETY: the start and the committed bytes are writable
        unsafe { core::ptr::write_bytes(p, 0x4d, r.len() + more) };
        let grown = r.len() + more;
        assert!(grown > share, "grown past its share");
        crate::park::REGIONS.put(r);
        let short = crate::park::REGIONS.take_laid(start, grown - (1 << 20));
        assert!(short.as_ref().is_none_or(|b| b.as_ptr() != p as *const u8), "an ask whose reservation it passes never takes it");
        drop(short);
        let again = super::laid_region(start, reserve, share, None, None).expect("a laid region");
        assert_eq!((again.as_ptr(), again.len(), again.is_laid(), again.lay_reserve(), again.lay_share()), (p as *const u8, grown, true, reserve, share), "taken back warm, whole, cut by its taker's share");
        #[cfg(target_os = "linux")]
        assert!(again.rw >= again.len(), "its whole length is writable");
        // SAFETY: `grown` bytes at `p` are the region's length, writable; the pages it grew kept their bytes
        assert_eq!(unsafe { *p.add(grown - 1) }, 0x4d, "its grown pages stayed resident");
        // a taker whose room cannot hold the warm length: the region parks again (the next laid take finds it) and the
        // ask opens a fresh reservation
        crate::park::REGIONS.put(again);
        let warm = crate::park::REGIONS.take_laid(start, reserve).expect("the warm region, parked again");
        assert_eq!(warm.as_ptr(), p as *const u8);
        assert!(super::hold_warm(warm, reserve, share, |_| None).is_none(), "refused: no warm region");
        let back = crate::park::REGIONS.take_laid(start, reserve).expect("parked again after the refusal");
        assert_eq!((back.as_ptr(), back.len()), (p as *const u8, grown));
        // D3809: another statement's retake (a wider section's smaller share) cuts by its own share, not the opener's
        crate::park::REGIONS.put(back);
        let narrower = share / 4;
        let retaken = super::laid_region(start, reserve, narrower, None, None).expect("a laid region");
        assert_eq!((retaken.as_ptr(), retaken.lay_share()), (p as *const u8, narrower), "the retaker's share");
        drop(retaken);
    }

    /// The forced-spill test fault (`Grant::spill_every`) opens a laid region cold: the warm parked one (its last run's
    /// growth resident) stays in the depot, so the run starts at its start and asks for room as it fills.
    #[test]
    fn the_forced_spill_fault_opens_a_laid_region_cold() {
        let _laid = laid_lock();
        // a reservation no other test asks (the depot is process-wide; a laid take needs a reservation at least the ask's)
        let (start, share, reserve) = (2usize << 20, 4usize << 20, 22usize << 20);
        let mut r = super::reserved_region(start, reserve, None, None, true).expect("a floor reservation");
        r.set_laid(true);
        let (p, more) = (r.as_mut_ptr(), (8usize << 20) - r.len());
        let held = super::road_held_now(more, None, None, false).expect("an unadmitted draw holds pool bytes");
        assert!(r.commit(more, held));
        let grown = r.len() + more;
        crate::park::REGIONS.put(r);
        let faulted = crate::mem::grant::Reservation::unadmitted(crate::mem::grant::Grant::of(64 << 20).with_spill_every(1));
        let cold = super::laid_region(start, reserve, share, None, Some(&faulted)).expect("a laid region");
        assert!(cold.as_ptr() != p as *const u8 && cold.len() < grown, "under the fault: a fresh reservation at its start");
        assert!(cold.is_laid());
        drop(cold);
        let warm = crate::park::REGIONS.take_laid(start, reserve).expect("the warm region, still parked");
        assert_eq!((warm.as_ptr(), warm.len()), (p as *const u8, grown));
    }

    /// A dropped mapped reservation past `reap::REAP_MIN` writable bytes is handed to the reaper with its pool charge (the
    /// hold then releases nothing; the reaper releases the charge once it unmapped the region); one under it is not.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_large_dropped_reservation_goes_to_the_reaper_with_its_charge() {
        use crate::mem::reap::{drain, defer, REAP_MIN};
        let mut r = super::reserved_region(REAP_MIN, 4 * REAP_MIN, None, None, true).expect("a floor reservation");
        assert!(r.rw >= REAP_MIN);
        let p = r.as_mut_ptr();
        // SAFETY: the share's bytes are writable at once
        unsafe { core::ptr::write_bytes(p, 0x6b, REAP_MIN) };
        // what the drop does, step by step: the region's hold taken out, the region forgotten by its owner, then handed
        let mut held = core::mem::replace(&mut r.held, crate::mem::grant::Held::NONE);
        let charge = core::mem::replace(&mut r.charge, memledger::Charge::NONE);
        let (reserve, rw, res) = (r.reserve, r.rw, r.res);
        r.len = 0;
        drop(r);
        assert!(defer(p, reserve, rw, &mut held, res, charge), "handed to the reaper");
        assert_eq!(held.pool_bytes(), 0, "its pool charge went with it");
        drain();
        let mut small = super::reserved_region(64 << 10, REAP_MIN, None, None, true).expect("a small floor reservation");
        let mut kept = core::mem::replace(&mut small.held, crate::mem::grant::Held::NONE);
        let pool = kept.pool_bytes();
        assert!(small.rw < REAP_MIN);
        let charge = core::mem::replace(&mut small.charge, memledger::Charge::NONE);
        assert!(!defer(small.as_mut_ptr(), small.reserve, small.rw, &mut kept, crate::mem::resident::NONE, charge), "a small one is unmapped by its owner");
        assert_eq!(kept.pool_bytes(), pool, "its hold untouched");
        small.held = kept;
    }

    /// A commit in place opens its reservation up to the next 2 MiB address boundary (`open_to`: one protection change and
    /// one huge page per step) while the bytes it charges stay the bytes committed; never past the mapping.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_commit_opens_whole_steps_and_charges_its_bytes() {
        let page = super::page_sizes().0;
        let (start, reserve, more) = (2usize << 20, 16usize << 20, 3 * page + 1);
        let mut r = super::reserved_region(start, reserve, None, None, true).expect("a floor reservation");
        let (p, rw0, c0, base) = (r.as_mut_ptr() as usize, r.rw, r.committed(), r.len() + r.committed());
        let held = super::road_held_now(more, None, None, false).expect("an unadmitted draw holds pool bytes");
        assert!(r.commit(more, held));
        assert_eq!(r.committed(), c0 + more, "the charge is the bytes committed");
        let end = base + more;
        if rw0 < end {
            match super::open_step() {
                0 => assert_eq!(r.rw, end.next_multiple_of(page)),
                s => assert!((p + r.rw) % s == 0 && r.rw >= end && r.rw - end < s, "opened to the next step boundary"),
            }
        }
        assert!(r.rw >= end && r.rw <= super::mapped_len(reserve), "the committed bytes writable, within the mapping");
        // SAFETY: the committed bytes are writable
        unsafe { core::ptr::write_bytes((p + base) as *mut u8, 0x5a, more) };
    }

    /// D2894: a floor reservation is address space alone: the share's pages readable and writable, the rest made so by a
    /// commit (whole pages); the region never moves, and it parks (option c') with its share only.
    #[test]
    fn a_floor_reservation_is_address_space_until_committed() {
        let (share, reserve) = (64usize << 10, 1usize << 30);
        let mut r = super::reserved_region(share, reserve, None, None, true).expect("a floor reservation");
        assert!(r.is_reserved() && r.is_floor() && r.len() == share && r.reserve() == reserve && r.committed() == 0);
        let p = r.as_mut_ptr();
        // SAFETY: the share's bytes are writable at once
        unsafe { core::ptr::write_bytes(p, 0x11, share) };
        let more = (3usize << 20) + 5; // not a page multiple: the commit's last page is made writable whole
        let held = super::road_held_now(more, None, None, false).expect("an unadmitted draw holds pool bytes");
        assert!(r.commit(more, held));
        assert_eq!((r.committed(), r.as_mut_ptr()), (more, p), "committed in place");
        // SAFETY: the share and the committed bytes are writable; the bytes read back are the ones written
        unsafe {
            core::ptr::write_bytes(p.add(share), 0x22, more);
            assert_eq!((*p, *p.add(share + more - 1)), (0x11, 0x22));
        }
        #[cfg(target_os = "linux")]
        assert!(r.rw >= share + more && r.rw % super::page_sizes().0 == 0 && r.rw < reserve, "only the committed pages are writable");
        crate::park::REGIONS.put(r);
    }

    /// D3163: giving the commits back (a reservation about to park) closes their pages again: past the share nothing is
    /// writable until the next commit opens it, so a taker that writes past `len + committed` faults.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_uncommitted_reservation_is_closed_past_its_share() {
        let page = super::page_sizes().0;
        let (share, reserve, more) = (64usize << 10, 64usize << 20, 4 * page + 1);
        let mut r = super::reserved_region(share, reserve, None, None, true).expect("a floor reservation");
        let p = r.as_mut_ptr();
        let keep = share.next_multiple_of(page);
        let held = super::road_held_now(more, None, None, false).expect("an unadmitted draw holds pool bytes");
        assert!(r.commit(more, held));
        // SAFETY: the share and the committed bytes are writable
        unsafe { core::ptr::write_bytes(p.add(share), 0x33, more) };
        assert_eq!(prot_at(p as usize + keep), "rw-", "committed: writable");
        r.uncommit();
        assert_eq!((r.committed(), r.rw), (0, keep), "only the share's pages stay writable");
        assert_eq!(prot_at(p as usize + keep), "---", "past the share: no access until a commit");
        let held = super::road_held_now(more, None, None, false).expect("an unadmitted draw holds pool bytes");
        assert!(r.commit(more, held), "a later commit opens them again");
        // SAFETY: committed again: writable
        unsafe { core::ptr::write_bytes(p.add(share), 0x44, more) };
        assert_eq!(prot_at(p as usize + keep), "rw-");
        drop(r);
    }

    /// The protection (`/proc/self/maps` perms, read / write / execute) of the mapping that holds `addr`.
    #[cfg(target_os = "linux")]
    fn prot_at(addr: usize) -> String {
        let maps = std::fs::read_to_string("/proc/self/maps").expect("/proc/self/maps"); // alloc: Cold
        for line in maps.lines() {
            let mut f = line.split_whitespace();
            let (range, perms) = (f.next().unwrap_or(""), f.next().unwrap_or(""));
            let (lo, hi) = range.split_once('-').unwrap_or(("0", "0"));
            let (lo, hi) = (usize::from_str_radix(lo, 16).unwrap_or(0), usize::from_str_radix(hi, 16).unwrap_or(0));
            if (lo..hi).contains(&addr) {
                return perms.chars().take(3).collect(); // alloc: Cold
            }
        }
        String::new()
    }
}

/// D2894: the mapped reservations' system calls (`RegionBuf::map_reserved`, `commit`, `drop`).
#[cfg(target_os = "linux")]
mod vm {
    use core::ffi::{c_int, c_void};
    extern "C" {
        fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
        fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> c_int;
    }
    const PROT_NONE: c_int = 0;
    const PROT_READ_WRITE: c_int = 3;
    // the generic Linux values (x86-64 and arm64 alike)
    const MAP_PRIVATE: c_int = 0x02;
    const MAP_ANONYMOUS: c_int = 0x20;
    const MAP_NORESERVE: c_int = 0x4000;

    /// `len` bytes (whole pages) of address space: no access, no swap or commit charge, no page behind any of it.
    pub(super) fn map_none(len: usize) -> Option<*mut u8> {
        // SAFETY: an anonymous private mapping at an address the kernel picks; nothing else is touched.
        let p = unsafe { mmap(core::ptr::null_mut(), len, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0) };
        (p as usize != usize::MAX && !p.is_null()).then_some(p as *mut u8)
    }

    /// `[p, p + len)` (whole pages of one `map_none` mapping) made readable and writable: its pages are faulted in, zeroed,
    /// as they are first written. False when the kernel refuses (a strict overcommit limit, the mapping count).
    /// # Safety — the range is inside a live `map_none` mapping.
    pub(super) unsafe fn protect_rw(p: *mut u8, len: usize) -> bool {
        mprotect(p as *mut c_void, len, PROT_READ_WRITE) == 0
    }

    /// D3163: `[p, p + len)` (whole pages of one `map_none` mapping) made inaccessible again, its contents dead.
    /// # Safety — the range is inside a live `map_none` mapping nothing reads or writes until it is made writable again.
    pub(super) unsafe fn protect_none(p: *mut u8, len: usize) -> bool {
        mprotect(p as *mut c_void, len, PROT_NONE) == 0
    }

    /// # Safety — `[p, p + len)` is a whole `map_none` mapping nothing reads or writes again.
    pub(super) unsafe fn unmap(p: *mut u8, len: usize) {
        munmap(p as *mut c_void, len);
    }
}

/// D2894: a mapped reservation of `reserve` bytes at `p` given back whole.
#[cfg(target_os = "linux")]
pub(crate) fn unmap(p: *mut u8, reserve: usize) {
    // SAFETY: `p` is the base of a `map_reserved` mapping of `reserve` bytes (`mapped_len`), dropped by its only owner
    unsafe { vm::unmap(p, mapped_len(reserve)) };
}

/// The step a commit in place opens (`RegionBuf::commit`): 2 MiB, the transparent huge page of a 4 KiB-page kernel. A
/// kernel whose huge page is another size (512 MiB under 64 KiB pages) opens whole pages as before.
const OPEN_STEP: usize = 2 << 20;

fn open_step() -> usize {
    if page_sizes().1 == OPEN_STEP { OPEN_STEP } else { 0 }
}

/// The length of a mapped reservation of `reserve` bytes: whole 2 MiB steps when it holds one (`open_step`), so the kernel
/// places it at a step boundary and each step it opens is one huge page; else whole pages.
pub(crate) fn mapped_len(reserve: usize) -> usize {
    match open_step() {
        s if s != 0 && reserve >= s => reserve.next_multiple_of(s),
        _ => reserve.next_multiple_of(page_sizes().0),
    }
}

/// The writable extent from `ptr` that a commit to `end` bytes opens: up to the next 2 MiB address boundary (`open_step`),
/// else whole pages; never past the mapping (`mapped`).
fn open_to(ptr: *const u8, end: usize, mapped: usize) -> usize {
    let page = page_sizes().0;
    let to = match open_step() {
        0 => end.next_multiple_of(page),
        s => ((ptr as usize + end).next_multiple_of(s) - ptr as usize).max(end.next_multiple_of(page)),
    };
    to.min(mapped)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn unmap(_p: *mut u8, _reserve: usize) {}

/// D2894: `[p, p + len)` of a mapped reservation made readable and writable (`vm::protect_rw`).
/// # Safety — the range is inside a live `map_reserved` mapping.
#[cfg(target_os = "linux")]
unsafe fn protect_rw(p: *mut u8, len: usize) -> bool {
    vm::protect_rw(p, len)
}

#[cfg(not(target_os = "linux"))]
unsafe fn protect_rw(_p: *mut u8, _len: usize) -> bool {
    true
}

/// D3163: `[p, p + len)` of a mapped reservation made inaccessible again (`vm::protect_none`).
/// # Safety — the range is inside a live `map_reserved` mapping whose contents there are dead.
#[cfg(target_os = "linux")]
unsafe fn protect_none(p: *mut u8, len: usize) -> bool {
    vm::protect_none(p, len)
}

#[cfg(not(target_os = "linux"))]
unsafe fn protect_none(_p: *mut u8, _len: usize) -> bool {
    true
}

/// D2674: frame word `i` of `node` (the program's words site's key cells: the words a lowered program keeps across a
/// statement's calls, `Px::frame_word`). `None` for an unbound node, a program without a words site, or past its words.
pub(crate) fn frame_word(node: &crate::node::Node, i: u16) -> Option<u64> {
    // SAFETY: `frame_cell` names a live, aligned key cell of the node's ctx
    frame_cell(node, i).map(|c| unsafe { *c })
}

/// D2674: writes `v` into frame word `i` of `node` (`frame_word`); false where there is none.
pub(crate) fn set_frame_word(node: &crate::node::Node, i: u16, v: u64) -> bool {
    // SAFETY: as `frame_word`; the node is not running (no kernel writes the cell meanwhile)
    frame_cell(node, i).map(|c| unsafe { *c = v }).is_some()
}

fn frame_cell(node: &crate::node::Node, i: u16) -> Option<*mut u64> {
    let program = node.program();
    let (ws, n) = program.sites.iter().enumerate().find_map(|(x, s)| match s.kind {
        gravity_program::SiteKind::Words { n } => Some((x, n)),
        _ => None,
    })?;
    let ctx = node.ctx();
    if i >= n || ctx.is_null() {
        return None;
    }
    // SAFETY: a bound node's ctx lays site `ws` with `n` key cells (node/image.rs), alive while the node is; `i < n`
    let keys = unsafe { (*gravity_abi::ctx::site_ptr(ctx, ws)).keys.ptr() as *mut u64 };
    // SAFETY: `keys` points at `n` cells
    (!keys.is_null()).then(|| unsafe { keys.add(i as usize) })
}

#[cfg(test)]
mod p3_tests {
    const MIB: usize = 1 << 20;

    /// P3 (born red): R2p's reach over the parked regions and what its trim notes as freed are the regions' account bytes
    /// (each one's charge less what the residency pass last read as not resident: what the ledger holds resident for it),
    /// never their lengths. A 64 MiB mapping with 16 MiB written parks before a 1 MiB heap region (the newest, kept): R2p's
    /// reach and its trim are the mapping's ~16 MiB, where its length said 64. Born red while both read the lengths.
    #[cfg(target_os = "linux")]
    #[test]
    fn p3_r2p_reach_and_trim_are_the_parked_regions_account_bytes_not_their_lengths() {
        let d = crate::park::RegionDepot::fresh();
        let mut a = super::RegionBuf::map_reserved(64 * MIB, 64 * MIB + 2 * MIB).expect("a mapped reservation");
        // SAFETY: the first 64 MiB are writable (`map_reserved`)
        unsafe { core::ptr::write_bytes(a.as_mut_ptr(), 0x5a, 16 * MIB) };
        let (res, len) = (a.res, a.len());
        d.put(a);
        d.put(super::RegionBuf::alloc(MIB, true).expect("a heap region"));
        assert!(super::super::resident::pass_all());
        let resident = super::super::resident::resident_of(res).expect("listed");
        assert!((16 * MIB..=20 * MIB).contains(&resident), "the mapping's written pages are resident: {} MiB", resident >> 20);
        assert_eq!(len, 64 * MIB);
        let reach = d.trimmable_account_bytes();
        assert_eq!(reach, resident, "R2p's reach: the older region's account bytes, not its length ({} MiB)", reach >> 20);
        let freed = d.trim_account_but_newest(usize::MAX);
        assert_eq!(freed, resident, "the trim notes the bytes its drop takes off the account, not the length ({} MiB)", freed >> 20);
        assert_eq!(d.stats().entries, 1, "the newest region kept");
    }

    /// P3, the ledger's side: the PARKED account holds exactly the parked regions' charges (their committed bytes, as the
    /// move to PARKED booked them), a region whose tail was given back (`fit_to`) included, before and after R2p's trim.
    /// Run alone (other tests park regions on PARKED's account while they run):
    /// `cargo test -p gravity_driver --lib p3_the_ledgers_parked -- --ignored --test-threads=1`.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn p3_the_ledgers_parked_account_is_the_parked_regions_charges() {
        let acct = || memledger::account_row(memledger::Acct::PARKED).account_bytes;
        let before = acct();
        let d = crate::park::RegionDepot::fresh();
        let mut a = super::RegionBuf::map_reserved(64 * MIB, 64 * MIB + 2 * MIB).expect("a mapped reservation");
        // SAFETY: the first 64 MiB are writable (`map_reserved`)
        unsafe { core::ptr::write_bytes(a.as_mut_ptr(), 0x5a, 16 * MIB) };
        d.put(a);
        let mut c = super::RegionBuf::alloc(8 * MIB, true).expect("a heap region");
        assert!(c.fit_to(MIB), "fitted: its tail's pages given back");
        assert_eq!(c.charge_committed(), MIB as u64, "its tail is off its charge");
        d.put(c);
        d.put(super::RegionBuf::alloc(MIB, true).expect("a heap region"));
        let charged = d.charged_bytes() as i64;
        assert_eq!(acct() - before, charged, "PARKED holds the parked regions' charges");
        let _ = d.trim_account_but_newest(usize::MAX);
        assert_eq!(acct() - before, d.charged_bytes() as i64, "after the trim: the newest one's charge");
        assert_eq!(d.charged_bytes(), MIB as u64);
    }
}
