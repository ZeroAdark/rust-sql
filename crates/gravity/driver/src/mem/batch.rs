//! C10 2b (D3582): a batched slot. A spilled one-round hash aggregate (or distinct) whose records pass the statement's
//! room, read by one plain serial group emit (`Pipeline::refills`), keeps one batch of its partitions in memory at a
//! time, as PostgreSQL's HashAgg refills one spilled batch at a time under hash_mem (`hashagg_refill`). Its producer's
//! end (`Entry::own_single_of`, `Open::own_batched`) scatters every phase-1 table to the slot's files and calls `lay`:
//! the batches are planned from the files' partition bytes, one owner region is pinned for the largest, the first batch
//! is folded into it, and the slot's region is laid as that one owner's `Owned` header, marked batched. The emit gathers
//! the batch (`SPILL_OWNED_NEXT` with `OWNED_REWIND`) and asks for the next one at its records' end (`refill`), folded
//! into the same region in place of the last. Every other form keeps the owner phase over every partition at once.
#![allow(unsafe_code)]

use gravity_abi::breaker::{Owned, OwnerTop, OWNED_BATCHED, OWNED_EMIT_BYTES, OWNED_HDR, OWNERTOP_HDR, TABLE_OWNED};
use gravity_abi::status::{UNSUPPORTED_DOMAIN, UNSUPPORTED_SCRATCH};
use gravity_abi::{Ctx, PtrWord};
use gravity_program::{LifeId, Program};

use crate::mem::spill::{SpillFile, SpillTable, Stream};
use crate::sink::{BatchFold, Merge, Partial, Region};
use crate::{Error, Result};

/// A batched slot's state, on its stream (`Stream::batched`) until the slot's life ends (the stream's `clear`).
pub struct Batched {
    fold: Box<dyn BatchFold>,
    /// Each batch's partitions `[lo, hi)`, in partition order.
    batches: Vec<(u32, u32)>,
    /// The batch the next refill folds: the region holds batch `next - 1` (an empty owner at 0).
    next: usize,
    /// The region holds batch 0 as `lay` folded it, not gathered yet: the emit's first gather folds nothing.
    unread: bool,
    pbits: u8,
    region: Region,
    /// The slot's region, whose `Owned` header names `region`, and its bytes.
    leader: *mut u8,
    leader_len: u32,
    /// The producer's driving site, whose window in the node ctx renders the owners' text keys (the one owner's site
    /// at W0); `None`: a program with no site.
    site: Option<usize>,
    slot: u8,
}

// SAFETY: a batched slot's state is read and written on the leader alone (its producer's end, then its emit's refills,
// both on the node ctx's thread); its pointers name the slot's region and the pinned owner region, which live as long as
// the slot's life, and the state is dropped with the life (`Stream::clear`).
unsafe impl Send for Batched {}
unsafe impl Sync for Batched {}

/// Whether producer `p`'s slot `slot` is read by one refilling emit alone: the one pipeline that reads its life is a
/// plain serial group emit (`Pipeline::refills`) that consumes the slot first (its refills name consumed region 0).
pub(crate) fn read_by_refill(program: &Program, p: usize, slot: u8) -> bool {
    let mut readers = program.consumers_of(LifeId(p as u16));
    match (readers.next(), readers.next()) {
        (Some(k), None) => program.pipelines[k].refills && program.pipelines[k].consumes.first() == Some(&slot),
        _ => false,
    }
}

/// The batched road's fold for producer `p`'s slot, when it takes it: the slot's stream spilled to disk (no hand-off,
/// no entry runs), one refilling emit reads it (`read_by_refill`), its merge has a one-round runs form
/// (`Merge::batch_fold`, no kept run), and the one owner's region the old road asks over every held and spilled record
/// (`section.rs own_all`: `one_owner_bytes`, at most the wall) passes both the producer's `spare` region (cut from it,
/// no pin) and `room`, what W and G hold now: the old road's pin past W and G (`AggOwnerAll`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn fold_for(program: &Program, p: usize, slot: u8, stream: &Stream, merge: &dyn Merge, partials: &[Partial<'_>], held: usize, rec: usize, room: u64, spare: usize, kept: &mut dyn FnMut() -> Option<u64>) -> Option<Box<dyn BatchFold>> {
    if !stream.spilled() || stream.handed() || !read_by_refill(program, p, slot) {
        return None;
    }
    let ask = crate::section::one_owner_bytes(held, rec).min(crate::section::owner_wall_at());
    if ask as u64 <= room || ask <= spare {
        return None;
    }
    if merge.kept_run_memo(kept).is_some() {
        return None;
    }
    merge.batch_fold(partials)
}

/// The record bytes of each of the `parts.len()` partitions over `files` (a file that cannot say: its even share, as the
/// owner phase estimates it).
fn part_bytes(files: &[&dyn SpillFile], pbits: u8, parts: &mut [u64]) {
    parts.fill(0);
    // bounded("small-fixed: one file per owner of the slot's stream, at most 2 * SLOTS + 1")
    for f in files {
        let even = f.spilled_bytes() >> pbits;
        // bounded("small-fixed: one per partition, at most T1_MAX_PARTS")
        for (part, b) in parts.iter_mut().enumerate() {
            *b += match f.part_bytes(part as u32) {
                0 => even,
                b => b,
            };
        }
    }
}

/// The region one batch of `held` record bytes folds and gathers in: the one owner's runs layout with a table half of
/// twice its records (`one_owner_whole_bytes`, as the one owner's), and the emit state's pointer array (`OWNED_EMIT_BYTES`,
/// a word per record), at least the form's `floor`. The runs form lays at most `SPLIT_TABLE` of table at the region's end
/// (exec `OWNER_TABLE_CAP`), so a batch whose runs and array pass it asks the runs, the array and that table alone.
fn batch_region(held: u64, rec: usize, floor: u64) -> usize {
    use gravity_abi::breaker::OWNER_RUN_OFF;
    let held = held as usize;
    let gather = OWNED_EMIT_BYTES + 8 * held.div_ceil(rec.max(1));
    let whole = crate::section::one_owner_whole_bytes(held, rec) + gather;
    let front = held + gather;
    let region = if front >= crate::section::SPLIT_TABLE { whole.min(OWNER_RUN_OFF + front + crate::section::SPLIT_TABLE) } else { whole };
    region.max(floor as usize).next_multiple_of(8)
}

/// The batches over `parts` (each partition's record bytes): consecutive partitions while the batch's region
/// (`batch_region`) stays within `budget`, a partition past it alone a batch of its own (`AggPartitionPastGrant`: one
/// partition level). Partitions that hold nothing join their neighbours; no records, no batch.
fn plan(parts: &[u64], rec: usize, floor: u64, budget: u64, out: &mut Vec<(u32, u32)>) {
    out.clear();
    let (mut lo, mut held) = (0u32, 0u64);
    // bounded("small-fixed: at most T1_MAX_PARTS partitions")
    for (part, &b) in parts.iter().enumerate() {
        let part = part as u32;
        if held != 0 && batch_region(held + b, rec, floor) as u64 > budget {
            out.push((lo, part)); // alloc: Exec
            (lo, held) = (part, 0);
        }
        held += b;
    }
    if held != 0 {
        out.push((lo, parts.len() as u32)); // alloc: Exec
    }
}

/// The records a producer's end holds for its owner phase: its phase-1 tables' (`partials`) and its stream's files'
/// bytes, and the widest record (`Open::held_records`' sum, the one owner's `own_all`).
pub(crate) fn held_of(partials: &[Partial<'_>], stream: &Stream) -> (usize, usize) {
    let (mut held, mut rec) = (0usize, 8usize);
    // bounded("small-fixed: one phase-1 table per worker slot, at most SLOTS")
    for p in partials {
        if let Some(t) = gravity_abi::breaker::T1::of(p.bytes) {
            held += t.held() as usize * t.layout.size() as usize;
            rec = rec.max(t.layout.size() as usize);
        }
    }
    held += stream.files.iter().flatten().map(|f| f.spilled_bytes() as usize).sum::<usize>();
    (held, rec)
}

/// What one batch of producer `p`'s slot folds within: its breaker's ceiling (`grant::ceiling_of`: hash_mem, PostgreSQL's
/// budget for the node whose spilled batches it refills), else W.
pub(crate) fn budget(ctx: *mut Ctx, program: &Program, p: usize) -> u64 {
    // SAFETY: the callers pass the node's ctx, bound for the statement, on the leader thread.
    let n = unsafe { crate::hostimpl::inner_of(ctx) };
    match n.stmt().map_or(0, |s| crate::mem::grant::ceiling_of(s.grant, &program.pipelines[p])) {
        0 => n.w,
        c => c,
    }
}

/// What a pin on the leader may take inside the statement's grant now (`arena::pin_room`: W and G's room), else (a pool
/// unarmed, a statement not admitted) what W and the reservation hold.
pub(crate) fn room(ctx: *mut Ctx) -> u64 {
    // SAFETY: the callers pass the node's ctx, bound for the statement, on the leader thread.
    unsafe {
        crate::hostimpl::arena::pin_room(ctx).unwrap_or_else(|| {
            let n = crate::hostimpl::inner_of(ctx);
            n.w + n.stmt().map_or(0, |s| s.left())
        })
    }
}

/// The producer's end of a batched slot: every phase-1 table already scattered to `slot`'s files, the batches planned
/// over the files' partitions at `pbits` within `budget` (`budget`), one owner region pinned for the largest
/// (`pin_region`), the first batch folded into it, and `leader` (the slot's region) laid as its one-owner header, marked
/// batched; the state filed on the slot's stream. Fires `owner.batched`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lay(ctx: *mut Ctx, spill: *mut SpillTable, slot: u8, fold: Box<dyn BatchFold>, pbits: u8, budget: u64, leader: &mut [u8], site: Option<usize>) -> Result<()> {
    // SAFETY: the callers (`Entry::own_single_of`, `Open::own_batched`) pass the node's ctx, bound for the statement, its
    // spill table and the produced slot's region, idle past its producer: no claim runs, and the leader alone reads them.
    unsafe { lay_raw(ctx, spill, slot, fold, pbits, budget, leader, site) }
}

#[allow(clippy::too_many_arguments)]
unsafe fn lay_raw(ctx: *mut Ctx, spill: *mut SpillTable, slot: u8, fold: Box<dyn BatchFold>, pbits: u8, budget: u64, leader: &mut [u8], site: Option<usize>) -> Result<()> {
    if leader.len() < OWNED_HDR {
        return Err(Error::Unsupported(UNSUPPORTED_SCRATCH));
    }
    fires::fire_shared("owner.batched"); // shapecheck C10 2b (D3582): a spilled one-round group folded one batch of partitions at a time
    let table = &mut *spill;
    let stream = &mut table.streams[slot as usize];
    let mut files: [&dyn SpillFile; FILES] = [&crate::section::NO_FILE; FILES];
    let nf = files_of(stream, &mut files);
    let rec = fold.rec();
    let mut parts = vec![0u64; 1 << pbits]; // alloc: Exec
    part_bytes(&files[..nf], pbits, &mut parts);
    let floor = fold.floor();
    let mut batches = Vec::new(); // alloc: Exec
    plan(&parts, rec, floor, budget, &mut batches);
    let most = batches.iter().map(|&(lo, hi)| parts[lo as usize..hi as usize].iter().sum::<u64>()).max().unwrap_or(0);
    let region = pin_region(ctx, slot, batch_region(most, rec, floor).min(crate::section::owner_wall_at()))?;
    let mut b = Batched { fold, batches, next: 0, unread: false, pbits, region, leader: leader.as_mut_ptr(), leader_len: leader.len().min(u32::MAX as usize) as u32, site, slot };
    lend(ctx, || -> Result<()> {
        b.fold.open(b.region)?;
        if !b.batches.is_empty() {
            fold_next(ctx, &mut b, &files[..nf], rec)?;
            b.unread = true;
        }
        Ok(())
    })?;
    header(&b);
    stream.batched = Some(Box::new(b)); // alloc: Exec
    Ok(())
}

/// `SPILL_OWNED_NEXT` over `slot` (the emit's first consumed slot): `rewind` (a fresh gather) the first batch, gathered
/// as `lay` folded it when nothing read it yet, else folded again; otherwise the batch after the region's. The emit state
/// of the first batch that gathers a record (a fresh gather's even when none does), or 0 past the last batch, or when the
/// slot is not batched. Runs inside the emit's kernel call, whose abort byte it polls.
/// # Safety — `ctx` is the emit's, bound on the leader; `spill` its node's spill table.
pub(crate) unsafe fn refill(ctx: *mut Ctx, spill: *mut SpillTable, slot: u8, arg: u64) -> Result<i64> {
    let table = &mut *spill;
    let stream = &mut table.streams[slot as usize];
    let Some(mut b) = stream.batched.take() else { return Ok(0) };
    let mut files: [&dyn SpillFile; FILES] = [&crate::section::NO_FILE; FILES];
    let nf = files_of(stream, &mut files);
    let (rec, rewind) = ((arg & 0xffff_ffff) as usize, arg & gravity_abi::program::OWNED_REWIND != 0);
    let r = next_state(ctx, &mut b, &files[..nf], rec, rewind);
    header(&b);
    stream.batched = Some(b);
    r
}

unsafe fn next_state(ctx: *mut Ctx, b: &mut Batched, files: &[&dyn SpillFile], rec: usize, rewind: bool) -> Result<i64> {
    if core::mem::take(&mut b.unread) && rewind {
        return gather(ctx, b, files, rec).map(|st| st as i64);
    }
    if rewind {
        b.next = 0;
    }
    let mut step = cancelpoll::step("batch refill: the batches past the emit's");
    while b.next < b.batches.len() {
        step.tick().map_err(|_| Error::Cancelled)?;
        fold_next(ctx, b, files, rec)?;
        let st = gather(ctx, b, files, rec)?;
        if (*(st as *const [u64; 3]))[1] != 0 {
            return Ok(st as i64);
        }
    }
    if rewind {
        // every batch reduced to no group (a HAVING floor's prune): the empty state, as a slot with no record
        return gather(ctx, b, files, rec).map(|st| st as i64);
    }
    Ok(0)
}

/// Batch `b.next` folded into the region (opened afresh), its round closed; `next` moves past it. A region the batch
/// overflows (`UNSUPPORTED_SCRATCH`: a one-round form laid otherwise than the runs layout) is pinned twice the size, up
/// to the wall, and the batch folded again (its files are only read); a dense form that met a key outside its domain
/// (`UNSUPPORTED_DOMAIN`, D597) folds it again through its plain form.
unsafe fn fold_next(ctx: *mut Ctx, b: &mut Batched, files: &[&dyn SpillFile], rec: usize) -> Result<()> {
    let (lo, hi) = b.batches[b.next];
    let site = b.site.map_or(core::ptr::null_mut(), |s| gravity_abi::ctx::site_ptr(ctx, s));
    let cancel = (*ctx).cancel.ptr();
    let wall = crate::section::owner_wall_at();
    let mut step = cancelpoll::step("batch fold: a batch's partitions");
    let mut missed = false;
    loop {
        let r = (|| {
            zero_prefix(b.region);
            b.fold.open(b.region)?;
            for part in lo..hi {
                step.tick().map_err(|_| Error::Cancelled)?;
                b.fold.fold(b.region, part, b.pbits, files, site, cancel)?;
            }
            b.fold.end(b.region)
        })();
        match r {
            Ok(()) => break,
            // the fold answers a domain miss once, its plain form taking over
            Err(Error::Unsupported(UNSUPPORTED_DOMAIN)) if !core::mem::replace(&mut missed, true) => continue,
            Err(Error::Unsupported(UNSUPPORTED_SCRATCH)) if b.region.len < wall => {
                b.region = pin_region(ctx, b.slot, (2 * b.region.len).min(wall) & !7)?;
                header(b);
            }
            Err(Error::Unsupported(UNSUPPORTED_SCRATCH)) => return Err(crate::section::owner_wall(b.region.len, rec)),
            Err(e) => return Err(e),
        }
    }
    b.next += 1;
    Ok(())
}

/// The region's batch gathered for the emit: the emit state (`OWNED_EMIT_BYTES`: the pointer array's address, its
/// length, the kept count) at the owner's table offset, where a resumed emit finds it (dead past the round's close), and
/// every run's record addresses, in run order, behind it when the table space holds them, else between the runs' end
/// and the table (the runs form lays its table at the region's end, at most `OWNER_TABLE_CAP`). A region with room for
/// neither is pinned at twice the runs and the array (its table at most half of it) and the batch folded again there
/// (never past the wall).
unsafe fn gather(ctx: *mut Ctx, b: &mut Batched, files: &[&dyn SpillFile], rec: usize) -> Result<*mut u8> {
    let mut step = cancelpoll::step("batch gather: a batch's records");
    loop {
        let (base, top) = (b.region.ptr, &*(b.region.ptr as *const OwnerTop));
        let (off, runs) = (top.table_off as usize, top.worst as usize);
        let dir = base.add(OWNERTOP_HDR) as *const u32;
        let (mut n, mut end) = (0usize, 0usize);
        for g in 0..runs {
            step.tick().map_err(|_| Error::Cancelled)?;
            let (at, m) = (*dir.add(2 * g) as usize, *dir.add(2 * g + 1) as usize);
            n += m;
            end = end.max(at + m * rec);
        }
        // kernels `gather_in` finds the state of a resumed call at the owner's table offset, 8-aligned
        if off % 8 != 0 || off + OWNED_EMIT_BYTES > b.region.len {
            return Err(Error::Unsupported(UNSUPPORTED_SCRATCH));
        }
        let array = 8 * n;
        let at = if b.region.len - off - OWNED_EMIT_BYTES >= array {
            off + OWNED_EMIT_BYTES
        } else if off.saturating_sub(end.next_multiple_of(8)) >= array {
            end.next_multiple_of(8)
        } else {
            let need = (2 * (end + array + OWNED_EMIT_BYTES)).next_multiple_of(8);
            if need > crate::section::owner_wall_at() || need <= b.region.len || b.next == 0 {
                return Err(crate::section::owner_wall(need, n * rec));
            }
            b.region = pin_region(ctx, b.slot, need)?;
            header(b);
            b.next -= 1;
            fold_next(ctx, b, files, rec)?;
            continue;
        };
        let st = base.add(off);
        let ptrs = base.add(at) as *mut u64;
        let mut i = 0usize;
        for g in 0..runs {
            let (at, m) = (*dir.add(2 * g) as usize, *dir.add(2 * g + 1) as usize);
            for k in 0..m {
                step.tick().map_err(|_| Error::Cancelled)?;
                *ptrs.add(i + k) = base.add(at + k * rec) as u64;
            }
            i += m;
        }
        *(st as *mut [u64; 3]) = [ptrs as u64, n as u64, n as u64];
        return Ok(st);
    }
}

/// The slot's region laid as the one owner's header over `b.region`, marked batched.
unsafe fn header(b: &Batched) {
    *(b.leader as *mut Owned) = Owned { nowners: 1, owner_bytes: b.region.len as u32, limit: b.leader_len, pad0: [0; 3], kind: TABLE_OWNED, base: b.region.ptr as u64, pad1: [OWNED_BATCHED, 0, 0, 0, 0], cell: core::cell::UnsafeCell::new([]) };
}

/// A region of `bytes` for a batched slot's one owner: a pinned pool chunk filed as `slot`'s (given back when the slot
/// frees), named `AggPartitionPastGrant` past W and G (a batch is planned within them, so only a partition past them
/// alone, or a region regrown for one, is drawn there); else, as the one owner's (D2990), a temp file mapped shared.
/// # Safety — `ctx` is the node's, bound.
unsafe fn pin_region(ctx: *mut Ctx, slot: u8, bytes: usize) -> Result<Region> {
    let zeroed = crate::node::ZERO_PREFIX.min(bytes);
    let p = crate::hostimpl::arena::pin_batch(ctx, bytes, zeroed);
    if !p.is_null() {
        crate::hostimpl::arena::file_slot_pin(ctx, Some(slot), p);
        return Ok(Region { ptr: p, len: bytes });
    }
    if cancelpoll::seen() {
        return Err(Error::Cancelled);
    }
    let m = crate::hostimpl::arena::map_owners(ctx, bytes)?;
    if m.is_null() {
        // NR-3: refused by the pool and by the temp-file road: `pin_batch`'s road, named
        let pool = &crate::mem::grant::POOL;
        return Err(Error::OutOfMemory { bytes: bytes as u64, held: pool.held(), capacity: pool.capacity(), why: Some(gravity_program::spill::NoSpill::AggPartitionPastGrant) });
    }
    Ok(Region { ptr: m, len: bytes })
}

/// A reused region's prefix zeroed before its open, as a fresh cut's (section.rs `cut_zeroed`, a one-round owner's).
unsafe fn zero_prefix(r: Region) {
    core::ptr::write_bytes(r.ptr, 0, crate::node::ZERO_PREFIX.min(r.len));
}

/// `f` with the statement's abort byte lent to this thread (the producer's end runs between kernel calls), so the
/// batch's steps poll it (`cancelpoll::step`).
/// # Safety — `ctx` is bound.
unsafe fn lend<R>(ctx: *mut Ctx, f: impl FnOnce() -> R) -> R {
    let cancel = (*ctx).cancel.ptr() as *const core::sync::atomic::AtomicU8;
    if cancel.is_null() {
        return f();
    }
    cancelpoll::scoped(&*cancel, f)
}

/// The most files a slot's stream holds (`section.rs own_with`'s array).
const FILES: usize = 2 * crate::section::SLOTS + 1;

/// `stream`'s files in owner order into `out`; their count.
fn files_of<'a>(stream: &'a Stream, out: &mut [&'a dyn SpillFile; FILES]) -> usize {
    let mut n = 0;
    for f in stream.files.iter().flatten().map(|f| f.as_ref()).take(FILES) {
        out[n] = f;
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_pack_partitions_within_the_budget_and_a_big_one_alone() {
        let rec = 16;
        let one = batch_region(1600, rec, 0) as u64;
        let mut out = Vec::new(); // alloc: Cold
        // four partitions of 1600 bytes, a budget for two of them: two batches
        plan(&[1600, 1600, 1600, 1600], rec, 0, batch_region(3200, rec, 0) as u64, &mut out);
        assert_eq!(out, [(0, 2), (2, 4)]);
        // a budget under one partition: one batch each (the partition past the grant, one level)
        plan(&[1600, 1600, 1600], rec, 0, one / 2, &mut out);
        assert_eq!(out, [(0, 1), (1, 2), (2, 3)]);
        // empty partitions join their neighbours; no record, no batch
        plan(&[0, 1600, 0, 0, 1600, 0], rec, 0, one, &mut out);
        assert_eq!(out, [(0, 4), (4, 6)]);
        plan(&[0, 0, 0], rec, 0, one, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn a_batch_region_holds_its_runs_table_and_pointer_array() {
        let rec = 24;
        let r = batch_region(24_000, rec, 0);
        assert!(r >= crate::section::one_owner_whole_bytes(24_000, rec) + OWNED_EMIT_BYTES + 8 * 1000);
        assert_eq!(r % 8, 0);
        assert_eq!(batch_region(0, rec, 1 << 20), 1 << 20, "the form's floor");
        // a batch past the table cap: its runs, its array and the capped table
        let big = 240 << 20;
        let r = batch_region(big, rec, 0);
        assert!(r < crate::section::one_owner_whole_bytes(big as usize, rec), "the capped table, not twice the records");
        assert!(r >= big as usize + 8 * (big as usize / rec) + crate::section::SPLIT_TABLE);
    }
}
