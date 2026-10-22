//! `ops::material`'s raw half in exec (PLAN-v5 §3.2 R6): the Spool protocol's object (proto.rs), its regions, the copies'
//! arena and the cell's runtime (`Head::op`), all of its unsafe code behind the safe `new_obj` / `handle` / `restart` that
//! proto.rs lays with (construct bundle material-03: moved as is from proto.rs); and (setop_rec-0043) each spool a rerun
//! set's re-arm empties, found through its handle word in the caller's context params (exec rerun/mod.rs `run`, after
//! `rearm_ctes`).
#![allow(unsafe_code)]
use std::cell::RefCell;

use gravity_abi::status::ERROR;
use gravity_abi::Ctx;
use gravity_abi::limits::{MAPPED_ROAD_MAX_BYTES, MAPPED_ROAD_MIN_BYTES};
use gravity_driver::{RegionBuf, TempMap};
use gravity_kernels::ops::shared::object::entries as abi;
use gravity_program::cost::runtime::{SEAL_COPY_CHUNK as SEAL_CHUNK, SEAL_COPY_FLOOR as SEAL_FLOOR, SPOOL_COPY_CHUNK as COPY_CHUNK};
use types_error::PgResult;

use crate::glue;

/// Each spool whose handle word is in `spools` restarted (ops/material/proto.rs `restart`): a re-armed CTE's store and its
/// union's tables, whose count words the owner run reset. `ctx` is the caller's context, bound for this call (`m8_rerun`'s
/// first argument), its params holding the handle words, each one the cell proto.rs `lay` laid, live until the
/// execution's end. No spool: no read.
pub(crate) fn restart_spools(ctx: *mut Ctx, spools: &[usize]) -> PgResult<()> {
    if spools.is_empty() {
        return Ok(());
    }
    if ctx.is_null() {
        return Err(glue::internal("gravity: m8_rerun: a spool to restart with no context (setop_rec-0043)"));
    }
    for &h in spools {
        // SAFETY: the caller's bound context (above); its params hold the handle words, a nonzero one the spool cell's head,
        // its magic checked before any other read
        let o = unsafe { *(*(ctx as *const u8).add(gravity_abi::ctx::hdr::PARAMS).cast::<*const u64>()).add(h) } as *const abi::Head;
        // SAFETY: as above: a nonzero handle word is a laid cell's head
        if o.is_null() || unsafe { (*o).magic } != abi::SPOOL_MAGIC {
            return Err(glue::internal("gravity: m8_rerun: a re-armed CTE's spool handle word is not its spool (setop_rec-0043)"));
        }
        // SAFETY: the head is a spool cell's (magic checked), the first field of a live `SpoolObj` (repr(C))
        restart(unsafe { &*o.cast::<SpoolObj>() })?;
    }
    Ok(())
}

/// The first grow's records.
const FIRST_RECORDS: u64 = 64;
/// The widest record a spool takes (the proto's `arg`): PG's MaxHeapAttributeNumber cells and a null word per 64 cells,
/// derived from that limit (a record is a row a heap tuple can hold; overfit item NEW-kernels-exec-69).
const MAX_RECORD: u64 = {
    let cells = types_tuple::htup::MaxHeapAttributeNumber as u64;
    8 * cells + 8 * cells.div_ceil(64)
};

/// Where a region comes from: the execution's statement through the calling ctx (`object_region`), or a test's pool.
pub(crate) type Draw = unsafe fn(*mut Ctx, usize) -> Option<RegionBuf>;

/// The cell and the runtime state behind it (`h` points at the cell, the first field).
#[repr(C)]
pub(crate) struct SpoolObj {
    cell: abi::SpoolCell,
    rt: RefCell<SpoolRt>,
    /// D3040: one owner's spool of an owned spool (`OwnedObj`): only that owner fills it, at any width.
    owned: bool,
}

impl SpoolObj {
    /// The handle word proto.rs `lay` writes: the cell's address (its head first), live as long as the object.
    pub(crate) fn handle(&self) -> u64 {
        &self.cell as *const abi::SpoolCell as u64
    }
}

struct SpoolRt {
    draw: Draw,
    region: Option<Buf>,
    /// The copies' chunks, in order; the last one's first free byte is `used`.
    copies: Vec<Buf>,
    used: usize,
    /// The copies past a quarter chunk, a region each.
    large: Vec<Buf>,
    /// D3236: the temp-file road the spool takes once the pool refused it.
    road: Road,
}

/// A spool region: the pool's (`Draw`), or past the pool's refusal a temp file's (`Road`): a whole mapping (the record
/// region, an owned spool's merged records) or `len` bytes at an address carved from the road's mappings (a copies chunk,
/// a large copy), live as long as the road's mappings are (the spool's life, or until its restart).
enum Buf {
    Pool(RegionBuf),
    File(TempMap),
    Carved(usize, usize),
}

impl Buf {
    fn as_ptr(&self) -> *const u8 {
        match self {
            Buf::Pool(r) => r.as_ptr(),
            Buf::File(m) => m.as_mut_ptr(),
            Buf::Carved(p, _) => *p as *const u8,
        }
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        match self {
            Buf::Pool(r) => r.as_mut_ptr(),
            Buf::File(m) => m.as_mut_ptr(),
            Buf::Carved(p, _) => *p as *mut u8,
        }
    }

    fn len(&self) -> usize {
        match self {
            Buf::Pool(r) => r.len(),
            Buf::File(m) => m.len(),
            Buf::Carved(_, n) => *n,
        }
    }
}

/// D3236 (the re-audit's R2 spools row, F6): a spool's road past the pool, as PostgreSQL's tuplestore writes its rows
/// to a temp file past work_mem (a grouped CTE's spool under a lone statement whose held bytes reached the pool's
/// capacity was 53200 at a 64 KiB copies chunk). Every region still asks the pool first (`Draw`: W, then G; C10
/// (D4168): a session with a temp-file road is refused past them at once, no draw past G); a region the pool refuses
/// comes from temp files mapped shared (`gravity_driver::hostimpl::arena::object_map`, D491's road: no pool charge,
/// never parked, unmapped and gone with its holder; temp_file_limit charged and 53400 past it; the kernel writes the
/// pages back under memory pressure). The record region maps a file of its own (dropped at the next grow, as a pool
/// region is); copies are carved from the road's mappings, each the bytes mapped so far within `MAPPED_ROAD_MIN_BYTES
/// ..= MAPPED_ROAD_MAX_BYTES` (the hosts' mapped road's sizes, D3267), and a copy that fits the newest mapping's rest
/// is carved there before the pool is asked again, so a spool on the road asks the pool once a mapping, not once a
/// chunk. The mappings live as long as the spool, or until its restart. Their bytes are resident page cache, not
/// anonymous memory (the watchdog's RssAnon does not see them); the grant report's `mapped past the pool` field counts
/// them.
#[derive(Default)]
struct Road {
    maps: Vec<TempMap>,
    at: usize,
    end: usize,
    mapped: usize,
}

impl Road {
    /// A whole mapping of `bytes`; `None` without a file road.
    unsafe fn map(ctx: *mut Ctx, bytes: usize) -> PgResult<Option<TempMap>> {
        gravity_driver::hostimpl::arena::object_map(ctx, bytes).map_err(glue::pg_error)
    }

    /// The spool took the road (a mapping is held).
    fn on(&self) -> bool {
        !self.maps.is_empty()
    }

    /// `bytes` (rounded up to 8) carved from the newest mapping's rest; `None` when it has no room.
    fn carve_held(&mut self, bytes: usize) -> Option<usize> {
        let bytes = bytes.next_multiple_of(8);
        (self.end - self.at >= bytes).then(|| {
            self.at += bytes;
            self.at - bytes
        })
    }

    /// `bytes` (rounded up to 8) from the newest mapping, or a new one when it has no room; `None` without a file road.
    unsafe fn carve(&mut self, ctx: *mut Ctx, bytes: usize) -> PgResult<Option<usize>> {
        let bytes = bytes.next_multiple_of(8);
        if self.end - self.at < bytes {
            let want = self.mapped.clamp(MAPPED_ROAD_MIN_BYTES, MAPPED_ROAD_MAX_BYTES).max(bytes);
            let Some(m) = Road::map(ctx, want)? else { return Ok(None) };
            self.mapped += m.len();
            (self.at, self.end) = (m.as_mut_ptr() as usize, m.as_mut_ptr() as usize + m.len());
            self.maps.push(m); // alloc: Exec
        }
        let p = self.at;
        self.at += bytes;
        Ok(Some(p))
    }
}

/// setop_rec-0043: spool `o` emptied by its owner's resets, its count back at 0 (a re-armed CTE's store and its recursive
/// union's tables, ops/material/raw.rs `restart_spools`): its copies' arena restarts at its first chunk (kept) and its
/// large copies drop, as PG's tuplestore_clear frees the CTE's rows. Else every re-arm kept the last run's copies to the
/// execution's end (a 64 KiB chunk per few outer rows of a correlated SubPlan over by-reference rows). A spool on its
/// temp-file road (`Road`) drops every copy and the road's mappings and asks the pool again, as tuplestore_clear
/// truncates its file. The caller re-arms outside every read of the records.
pub(crate) fn restart(o: &SpoolObj) -> PgResult<()> {
    let Ok(mut rt) = o.rt.try_borrow_mut() else { return Err(bad("nest.spool: a spool restarted inside its own call")) };
    rt.restart();
    Ok(())
}

/// An empty spool of `rec`-byte records (8-aligned, at most `MAX_RECORD`), its regions drawn by `draw`.
pub(crate) fn new_obj(rec: u64, draw: Draw) -> PgResult<Box<SpoolObj>> {
    if rec == 0 || rec % 8 != 0 || rec > MAX_RECORD {
        return Err(glue::internal(&format!("gravity: nest.spool's record of {rec} bytes (8-aligned, 8 to {MAX_RECORD})"))); // alloc: Cold
    }
    let cell = abi::SpoolCell { head: abi::Head { magic: abi::SPOOL_MAGIC, op }, base: 0, cap: 0, rec };
    Ok(Box::new(SpoolObj { cell, rt: RefCell::new(SpoolRt { draw, region: None, copies: Vec::new(), used: 0, large: Vec::new(), road: Road::default() }), owned: false })) // alloc: Exec
}

/// D3040 (`abi::SPOOL_OWNED`): an owned spool: the merged cell its readers read, and one spool per owner ordinal, each
/// filled by its owner alone (a parallel fill), sealed into the merged cell by the leader after the fill (`seal`).
#[repr(C)]
pub(crate) struct OwnedObj {
    spools: core::cell::UnsafeCell<abi::OwnedSpools>,
    owners: Vec<Box<SpoolObj>>,
    merged: RefCell<Option<Buf>>,
    draw: Draw,
}

impl OwnedObj {
    /// The handle word: the merged cell's address (the object's first field), as a plain spool's.
    pub(crate) fn handle(&self) -> u64 {
        self.spools.get() as u64
    }
}

/// An empty owned spool of `rec`-byte records: a spool per owner ordinal, every count 0.
pub(crate) fn new_owned(rec: u64, draw: Draw) -> PgResult<Box<OwnedObj>> {
    let mut owners = Vec::with_capacity(gravity_abi::breaker::MAX_OWNERS); // alloc: Exec
    for _ in 0..gravity_abi::breaker::MAX_OWNERS {
        let mut o = new_obj(rec, draw)?;
        o.owned = true;
        owners.push(o); // alloc: Exec
    }
    let cell = abi::SpoolCell { head: abi::Head { magic: abi::SPOOL_MAGIC, op: op_merged }, base: 0, cap: 0, rec };
    let slots = core::array::from_fn(|i| abi::OwnedSlot { cell: owners[i].handle(), n: 0, pad: [0; 6] });
    let spools = core::cell::UnsafeCell::new(abi::OwnedSpools { cell, total: 0, pad: [0; 2], slots });
    Ok(Box::new(OwnedObj { spools, owners, merged: RefCell::new(None), draw })) // alloc: Exec
}

/// D3040: owned spool `o` sealed after its fill pipeline, on the leader with no worker running (`ctx` the leader's): every
/// owner's records copied in ordinal order into one region the merged cell names, `total` their count. The records'
/// by-reference datums keep pointing into their owners' copies, which live to the execution's end; each owner's record
/// region is given back.
/// D4628 (road `spool.seal`, `wide` = `Some(width)`): no copy on the leader. One owner holding every record (width 1, or one
/// worker's fill) lends its record region to the merged cell, which names its records where they lie; with several owners,
/// owner by owner in ordinal order, its records are copied into the merged region by `width` pool threads (`copy_wide`),
/// the leader parked until they are, and its region given back before the next owner's copy (the peak the leader's copy
/// holds). An owner's records under `SEAL_FLOOR` chunks, or with no pool, the leader copies as above.
pub(crate) fn seal(o: &OwnedObj, ctx: *mut Ctx, wide: Option<u16>) -> gravity_driver::Result<()> {
    use gravity_driver::Error;
    // SAFETY: no owner runs (the fill pipeline completed); the leader alone reads and writes the object here.
    let sp = unsafe { &mut *o.spools.get() };
    let rec = sp.cell.rec;
    let total: u64 = sp.slots.iter().map(|s| s.n).sum();
    let bytes = total.checked_mul(rec).filter(|&n| n <= isize::MAX as u64).ok_or(Error::Host("gravity: nest.spool: an owned spool past the address space".into()))? as usize;
    for (slot, owner) in sp.slots.iter().zip(o.owners.iter()) {
        if slot.n > 0 && (slot.n > owner.cell.cap || owner.cell.base == 0) {
            return Err(Error::Host("gravity: nest.spool: an owner's count past its spool's records".into()));
        }
    }
    let held = sp.slots.iter().filter(|s| s.n > 0).count();
    if wide.is_some() && held == 1 {
        // D4628: the one owner's region becomes the merged one, its records where they lie (its copies stay its own)
        let Some(i) = sp.slots.iter().position(|s| s.n > 0) else { return Err(Error::Host("gravity: nest.spool: no owner holds the records".into())) };
        let owner = &o.owners[i];
        let Ok(mut rt) = owner.rt.try_borrow_mut() else { return Err(Error::Host("gravity: nest.spool: an owned spool sealed inside its own call".into())) };
        let Some(region) = rt.region.take() else { return Err(Error::Host("gravity: nest.spool: an owner's records with no region".into())) };
        sp.cell.base = owner.cell.base;
        sp.cell.cap = total;
        sp.total = total;
        sp.slots[i].n = 0;
        *o.merged.borrow_mut() = Some(region);
        fires::fire("exec.spool.seal"); // shapecheck fire signal (D4628): an owned spool sealed with no copy
        return Ok(());
    }
    let oom = |n: usize| {
        let pool = &gravity_driver::POOL;
        Error::OutOfMemory { bytes: n as u64, held: pool.held(), capacity: pool.capacity(), why: None }
    };
    let mut merged = match bytes {
        0 => None,
        // SAFETY: the leader's bound ctx (the caller's), as any grow's; past the pool's refusal a temp file (D3236, `Road`)
        n => Some(match unsafe { (o.draw)(ctx, n) } {
            Some(r) => Buf::Pool(r),
            None => Buf::File(unsafe { gravity_driver::hostimpl::arena::object_map(ctx, n) }?.ok_or_else(|| oom(n))?),
        }),
    };
    let (mut at, mut pooled) = (0usize, false);
    for (slot, owner) in sp.slots.iter_mut().zip(o.owners.iter()) {
        if slot.n == 0 {
            continue;
        }
        let Ok(mut rt) = owner.rt.try_borrow_mut() else { return Err(Error::Host("gravity: nest.spool: an owned spool sealed inside its own call".into())) };
        let n = (slot.n * rec) as usize;
        if let Some(m) = merged.as_mut() {
            // SAFETY: the owner's region holds `cap >= n` records from `base` (checked above); the merged region `total * rec`
            // bytes, `at + n` at most `total * rec`
            let (src, dst) = (owner.cell.base as *const u8, unsafe { m.as_mut_ptr().add(at) });
            // D4628: by the pool's threads under the road, past the floor
            let wide = match wide {
                Some(width) => unsafe { copy_wide(src, dst, n, width) }?,
                None => false,
            };
            if !wide {
                // SAFETY: as above
                unsafe { core::ptr::copy_nonoverlapping(src, dst, n) };
            }
            pooled |= wide;
        }
        at += n;
        rt.region = None; // the owner's records are in the merged region; its copies stay
        slot.n = 0;
    }
    sp.cell.base = merged.as_mut().map_or(0, |m| m.as_mut_ptr() as u64);
    sp.cell.cap = total;
    sp.total = total;
    *o.merged.borrow_mut() = merged;
    if pooled {
        fires::fire("exec.spool.seal.wide"); // shapecheck fire signal (D4628): an owned spool's records copied by the pool's threads
    }
    Ok(())
}

/// D4628: `len` bytes from `src` copied to `dst` by `width` threads of the statement's pool, cut into `SEAL_CHUNK` chunks,
/// each copied whole by one claim, so the destination's pages are first touched on many threads. `Ok(false)`: not run
/// (under `SEAL_FLOOR` chunks, width 1, or no pool): the caller copies.
/// # Safety
/// `src` holds `len` readable bytes and `dst` `len` writable ones, disjoint, and nothing else writes either while it runs.
unsafe fn copy_wide(src: *const u8, dst: *mut u8, len: usize, width: u16) -> gravity_driver::Result<bool> {
    let chunks = len.div_ceil(SEAL_CHUNK);
    let Some(pool) = gravity_parallel::installed_pool().filter(|_| width >= 2 && chunks >= SEAL_FLOOR) else { return Ok(false) };
    let (src, dst) = (src as usize, dst as usize);
    let job = |_: u16, claim: gravity_abi::MorselRange| -> gravity_driver::Result<()> {
        for k in claim.lo as usize..(claim.hi as usize).min(chunks) {
            let (lo, hi) = (k * SEAL_CHUNK, ((k + 1) * SEAL_CHUNK).min(len));
            // SAFETY: bytes `lo..hi` of both ranges (the caller's); every byte is in one chunk, every chunk in one claim, so
            // no two threads write one byte
            unsafe { core::ptr::copy_nonoverlapping((src + lo) as *const u8, (dst + lo) as *mut u8, hi - lo) };
        }
        Ok(())
    };
    pool.run(width, &gravity_parallel::PartitionRange::new(chunks as u64, 1), &job).map_err(|f| f.error)?;
    Ok(true)
}

/// D3054 (`nest.spool.chunks`): the sealed record count of the owned spool whose handle word is `h` (`OwnedObj::handle`),
/// read on the leader after its fill pipeline (no worker runs).
pub(crate) fn sealed_total(h: u64) -> u64 {
    // SAFETY: `h` is an `OwnedObj`'s handle, its `OwnedSpools` (the object's first field), alive to the execution's end
    unsafe { (*(h as *const abi::OwnedSpools)).total }
}

/// The merged cell's runtime: nothing appends to it (each owner fills its own spool).
unsafe fn op_merged(_h: *mut abi::Head, _ctx: *mut Ctx, _code: u64, _a: u64, _b: u64) -> i64 {
    fail("nest.spool: an owned spool's merged cell appended to (each owner fills its own spool)")
}

/// The object's runtime (`Head::op`): `OP_SPOOL_GROW` → the new capacity in records; `OP_SPOOL_COPY` (`a` the datum,
/// `b` its bytes) → the copy's address; ERROR with the error stashed else.
unsafe fn op(h: *mut abi::Head, ctx: *mut Ctx, code: u64, a: u64, b: u64) -> i64 {
    // SAFETY: `h` is the first field of a live `SpoolObj` (`call` checked its magic), laid for this execution.
    let obj = &mut *h.cast::<SpoolObj>();
    if !obj.owned && !ctx.is_null() && (*ctx).width != 1 {
        return fail("nest.spool: a spool filled by more than one worker (its producer is serial by name)");
    }
    let Ok(mut rt) = obj.rt.try_borrow_mut() else { return fail("nest.spool: a spool entered inside its own call") };
    let r = match code {
        abi::OP_SPOOL_GROW => rt.grow(ctx, &mut obj.cell),
        abi::OP_SPOOL_COPY => rt.copy(ctx, a as *const u8, b as usize),
        _ => Err(bad(&format!("nest.spool: no operation {code}"))), // alloc: Cold
    };
    r.unwrap_or_else(|e| {
        gravity_fn::stash_error(e);
        ERROR
    })
}

impl SpoolRt {
    /// The region doubled (the first: `FIRST_RECORDS`), its `cap` records copied over; the cell names the new one.
    unsafe fn grow(&mut self, ctx: *mut Ctx, cell: &mut abi::SpoolCell) -> PgResult<i64> {
        let cap = if cell.cap == 0 { FIRST_RECORDS } else { cell.cap.checked_mul(2).ok_or_else(|| bad("nest.spool: a spool past 2^64 records"))? };
        let bytes = cap.checked_mul(cell.rec).filter(|&n| n <= isize::MAX as u64).ok_or_else(|| bad("nest.spool: a spool region past the address space"))? as usize;
        let mut next = self.take(ctx, bytes)?;
        if let Some(old) = self.region.as_ref() {
            let n = (cell.cap * cell.rec) as usize;
            core::ptr::copy_nonoverlapping(old.as_ptr(), next.as_mut_ptr(), n);
        }
        cell.base = next.as_mut_ptr() as u64;
        cell.cap = cap;
        self.region = Some(next); // the old region drops here, after the copy
        Ok(cap as i64)
    }

    /// setop_rec-0043 (`restart`): the arena back to its first chunk's start, the large copies dropped. The record region
    /// stays (the fill appends from record 0 again).
    fn restart(&mut self) {
        if self.road.on() {
            self.copies.clear();
            self.large.clear();
            self.road = Road::default();
        } else {
            self.copies.truncate(1);
            self.large.clear();
        }
        self.used = 0;
    }

    /// `len` bytes at `ptr` copied into the copies' arena (8-aligned; never moved, never reused while its fill's records
    /// live: `restart`).
    unsafe fn copy(&mut self, ctx: *mut Ctx, ptr: *const u8, len: usize) -> PgResult<i64> {
        if ptr.is_null() && len > 0 {
            return Err(bad("m8_spool_copy: a null datum of nonzero length"));
        }
        let n = len.div_ceil(8).max(1) * 8;
        if n > COPY_CHUNK / 4 {
            let mut c = self.take_copy(ctx, n)?;
            core::ptr::copy_nonoverlapping(ptr, c.as_mut_ptr(), len);
            let at = c.as_mut_ptr() as i64;
            self.large.push(c); // alloc: Exec
            return Ok(at);
        }
        if !self.copies.last().is_some_and(|c| c.len() - self.used >= n) {
            let c = self.take_copy(ctx, COPY_CHUNK)?;
            self.copies.push(c); // alloc: Exec
            self.used = 0;
        }
        let Some(c) = self.copies.last_mut() else { return Err(bad("m8_spool_copy: no open chunk")) };
        let at = c.as_mut_ptr().add(self.used);
        if len > 0 {
            core::ptr::copy_nonoverlapping(ptr, at, len);
        }
        self.used += n;
        Ok(at as i64)
    }

    /// The record region's `bytes`: from `draw`, else (the pool refused it) a temp file of its own (`Road`), else the pool's
    /// out of memory.
    unsafe fn take(&mut self, ctx: *mut Ctx, bytes: usize) -> PgResult<Buf> {
        if let Some(r) = (self.draw)(ctx, bytes) {
            return Ok(Buf::Pool(r));
        }
        Road::map(ctx, bytes)?.map(Buf::File).ok_or_else(|| oom(bytes))
    }

    /// A copies chunk or a large copy of `bytes`: the rest of the road's newest mapping when it holds them, else from
    /// `draw`, else (the pool refused it) carved from a new mapping of the road; else the pool's out of memory.
    unsafe fn take_copy(&mut self, ctx: *mut Ctx, bytes: usize) -> PgResult<Buf> {
        if let Some(p) = self.road.carve_held(bytes) {
            return Ok(Buf::Carved(p, bytes));
        }
        if let Some(r) = (self.draw)(ctx, bytes) {
            return Ok(Buf::Pool(r));
        }
        self.road.carve(ctx, bytes)?.map(|p| Buf::Carved(p, bytes)).ok_or_else(|| oom(bytes))
    }
}

/// The pool's out of memory for a spool region of `bytes` (no file road took it).
fn oom(bytes: usize) -> Box<types_error::PgError> {
    let pool = &gravity_driver::POOL;
    glue::pg_error(gravity_driver::Error::OutOfMemory { bytes: bytes as u64, held: pool.held(), capacity: pool.capacity(), why: None })
}

#[cold]
#[inline(never)]
fn fail(what: &str) -> i64 {
    gravity_fn::stash_error(bad(what));
    ERROR
}

#[cold]
#[inline(never)]
fn bad(what: &str) -> Box<types_error::PgError> {
    glue::internal(&format!("gravity: {what}")) // alloc: Cold
}

/// The crate rig's regions (proto.rs `prepare_bare`): the grant pool outside any Node.
pub(crate) unsafe fn pool(_ctx: *mut Ctx, bytes: usize) -> Option<RegionBuf> {
    gravity_driver::try_region(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test's regions: the grant pool outside any Node (`try_region`).
    unsafe fn pool(_ctx: *mut Ctx, bytes: usize) -> Option<RegionBuf> {
        gravity_driver::try_region(bytes).ok()
    }

    fn h(o: &SpoolObj) -> u64 {
        &o.cell as *const abi::SpoolCell as u64
    }

    fn message() -> String {
        gravity_fn::take_error().map(|e| e.message().to_owned()).unwrap_or_default()
    }

    /// The IR's append of record `n` (two words: a by-reference datum's address, its length): grow at `n == cap`, then
    /// store at `base + n * rec` read after the grow.
    unsafe fn append(o: &SpoolObj, n: u64, datum: &[u8]) {
        let ctx: *mut Ctx = core::ptr::null_mut();
        if n == o.cell.cap {
            assert!(abi::m8_spool_grow(ctx as u64, h(o)) > n as i64, "{}", message());
        }
        let at = abi::m8_spool_copy(ctx, h(o), datum.as_ptr(), datum.len() as u64);
        assert!(at > 0, "{}", message());
        let rec = (o.cell.base as *mut u64).add((n * o.cell.rec / 8) as usize);
        rec.write(at as u64);
        rec.add(1).write(datum.len() as u64);
    }

    /// Record `n` read by index, its base read now: the datum it points at.
    unsafe fn read(o: &SpoolObj, n: u64) -> Vec<u8> {
        let rec = (o.cell.base as *const u64).add((n * o.cell.rec / 8) as usize);
        std::slice::from_raw_parts(rec.read() as *const u8, rec.add(1).read() as usize).to_vec() // alloc: Cold
    }

    fn datum(i: u64) -> Vec<u8> {
        format!("datum {i} {}", "x".repeat((i % 37) as usize)).into_bytes() // alloc: Cold
    }

    #[test]
    fn a_reader_mid_scan_reads_every_record_by_index_across_grows_and_its_datums_never_move() {
        let o = new_obj(16, pool).expect("a spool");
        assert_eq!((o.cell.base, o.cell.cap), (0, 0), "an execution's spool starts empty");
        let mut seen = Vec::new(); // (record, the datum's address) the reader took before later grows // alloc: Cold
        // the lazy fill: the reader reads record k right after it is appended, so every grow happens mid-scan
        unsafe {
            for n in 0..1000u64 {
                append(&o, n, &datum(n));
                let rec = (o.cell.base as *const u64).add((n * 2) as usize);
                seen.push((n, rec.read())); // alloc: Cold
                assert_eq!(read(&o, n), datum(n));
            }
        }
        assert_eq!(o.cell.cap, 1024, "64 doubled four times");
        // after every grow: each record's words are the ones appended and its datum is where the reader first saw it
        for (n, addr) in seen {
            let rec = unsafe { (o.cell.base as *const u64).add((n * 2) as usize).read() };
            assert_eq!(rec, addr, "record {n}'s by-reference datum never moved");
            assert_eq!(unsafe { read(&o, n) }, datum(n));
        }
    }

    /// D4628: an owned spool sealed under road `spool.seal` (`wide`): one owner holding every record lends its region (the
    /// merged cell names its records where they lie), several owners' records are copied in ordinal order (no pool in a
    /// unit test: the leader's copy); off (`None`) the leader copies as before. Every record reads back by index.
    #[test]
    fn a_wide_seal_lends_one_owners_region_and_keeps_the_ordinal_order() {
        let cases: [(&[u64], Option<u16>); 5] = [(&[0, 300, 0], Some(8)), (&[300], Some(1)), (&[5, 0, 200, 1], Some(8)), (&[5, 0, 200, 1], None), (&[300], None)];
        for (counts, wide) in cases {
            let o = new_owned(16, pool).expect("an owned spool");
            let mut want = Vec::new(); // alloc: Cold
            for (i, &n) in counts.iter().enumerate() {
                for k in 0..n {
                    unsafe { append(&o.owners[i], k, &datum(i as u64 * 1000 + k)) };
                }
                unsafe { (*o.spools.get()).slots[i].n = n };
                want.extend((0..n).map(|k| datum(i as u64 * 1000 + k)));
            }
            let held: Vec<usize> = (0..counts.len()).filter(|&i| counts[i] > 0).collect(); // alloc: Cold
            let lent = (wide.is_some() && held.len() == 1).then(|| o.owners[held[0]].cell.base);
            seal(&o, core::ptr::null_mut(), wide).unwrap_or_else(|e| panic!("{counts:?} {wide:?}: {e:?}"));
            let sp = unsafe { &*o.spools.get() };
            assert_eq!((sp.total, sp.cell.cap), (want.len() as u64, want.len() as u64), "{counts:?} {wide:?}: the sealed count");
            assert!(sp.slots.iter().all(|s| s.n == 0), "{counts:?} {wide:?}: every owner's count taken");
            match lent {
                Some(b) => assert_eq!(sp.cell.base, b, "{counts:?} {wide:?}: the one owner's region lent, no copy"),
                None => assert!(o.owners.iter().all(|w| w.cell.base != sp.cell.base), "{counts:?} {wide:?}: a merged region of its own"),
            }
            for (k, w) in want.iter().enumerate() {
                let rec = unsafe { (sp.cell.base as *const u64).add(k * 2) };
                let got = unsafe { std::slice::from_raw_parts(rec.read() as *const u8, rec.add(1).read() as usize) };
                assert_eq!(got, &w[..], "{counts:?} {wide:?}: record {k} in owner ordinal order");
            }
        }
    }

    #[test]
    fn a_large_datum_takes_a_region_of_its_own_and_the_open_chunk_stays_open() {
        let o = new_obj(16, pool).expect("a spool");
        let (small, big) = (vec![7u8; 10], vec![9u8; COPY_CHUNK]); // alloc: Cold
        unsafe {
            append(&o, 0, &small);
            append(&o, 1, &big);
            append(&o, 2, &small);
            let (a, c) = ((o.cell.base as *const u64).read(), (o.cell.base as *const u64).add(4).read());
            assert_eq!(c, a + 16, "the small copies share the open chunk, 8-aligned");
            assert_eq!((read(&o, 0), read(&o, 1), read(&o, 2)), (small.clone(), big.clone(), small.clone()));
        }
    }

    #[test]
    fn a_restart_takes_the_copies_back_to_the_first_chunk() {
        let o = new_obj(16, pool).expect("a spool");
        unsafe {
            for n in 0..6000u64 {
                append(&o, n, &datum(n));
            }
            append(&o, 6000, &vec![5u8; COPY_CHUNK]); // alloc: Cold
        }
        let first = unsafe { (o.cell.base as *const u64).read() };
        let chunks = |o: &SpoolObj| (o.rt.borrow().copies.len(), o.rt.borrow().large.len());
        assert!(chunks(&o).0 > 1 && chunks(&o).1 == 1, "6000 datums (about 190 KiB of copies) fill more than one chunk, one large copy");
        restart(&o).expect("outside its calls");
        assert_eq!(chunks(&o), (1, 0), "the first chunk kept, the large copy dropped");
        unsafe {
            append(&o, 0, &datum(7));
            assert_eq!((o.cell.base as *const u64).read(), first, "the next fill's first copy is the first chunk's start");
            assert_eq!(read(&o, 0), datum(7));
        }
    }

    #[test]
    fn a_malformed_spool_or_copy_errors_by_name() {
        for rec in [0, 12, MAX_RECORD + 8] {
            let e = new_obj(rec, pool).err().expect("refused");
            assert!(e.message().starts_with("gravity: nest.spool's record of"), "{}", e.message());
        }
        let o = new_obj(8, pool).expect("a spool");
        let ctx: *mut Ctx = core::ptr::null_mut();
        assert_eq!(unsafe { abi::m8_spool_copy(ctx, h(&o), core::ptr::null(), 4) }, ERROR);
        assert_eq!(message(), "gravity: m8_spool_copy: a null datum of nonzero length");
        assert!(unsafe { abi::m8_spool_copy(ctx, h(&o), core::ptr::null(), 0) } > 0, "an empty datum has an address");
    }

    #[test]
    fn a_region_the_pool_refuses_is_the_pools_out_of_memory() {
        unsafe fn none(_ctx: *mut Ctx, _bytes: usize) -> Option<RegionBuf> {
            None
        }
        let o = new_obj(8, none).expect("a spool");
        assert_eq!(unsafe { abi::m8_spool_grow(0, h(&o)) }, ERROR);
        let e = gravity_fn::take_error().expect("stashed");
        assert_eq!((e.message(), e.sqlstate()), ("out of memory", types_error::ERRCODE_OUT_OF_MEMORY));
        assert_eq!((o.cell.base, o.cell.cap), (0, 0), "a failed grow leaves the spool as it was");
    }
}
