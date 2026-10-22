#![allow(unsafe_code)]
use gravity_abi::PtrWord;
use gravity_abi::program::{STREAM_CONSUMED, SPILL_MERGE_BEGIN, SPILL_MERGE_BIG, SPILL_MERGE_WRITE, SPILL_RUN_ROWS, SPILL_DROP, SPILL_OUT_GROW, SPILL_PARTS, SPILL_RAISE_HELD, SPILL_REFILL, SPILL_REGION_COMMIT, SPILL_REGION_GROW, SPILL_RUNS, SPILL_RUN_BASE, SPILL_RUN_ADOPT, SPILL_RUN_CLOSE, SPILL_RUN_FILL, SPILL_RUN_OPEN, SPILL_RUN_PUT, SPILL_RUN_SHAPE, SPILL_RUN_SLICE, SPILL_SCATTER, SPILL_SEAL_OVER, SPILL_SPLIT, SPILL_TARGET};
use gravity_abi::status::OK;
use gravity_abi::Ctx;

use crate::hostimpl::{fail, guarded, inner_of};
use crate::node::Inner;
use crate::mem::spill::{region_live, SpillTable};
use crate::{Error, Result};

impl Inner {
    /// The ctx owner's own produced table (`Partials.own`), live; `at` the refill target offset.
    unsafe fn own_region(&self, at: usize) -> Result<*mut u8> {
        let own = (*self.partials).own.ptr();
        if own.is_null() {
            return Err(Error::host("gravity: spill without a produced table"));
        }
        let own = own.add(at);
        if !region_live(own, usize::MAX) {
            return Err(Error::host("gravity: spill of an empty region"));
        }
        Ok(own)
    }

    fn spill_table(&mut self) -> &mut SpillTable {
        // SAFETY: the table is a heap box the Node owns for its whole life; workers write only
        // their own slot, the leader reads every slot after the section.
        unsafe { &mut *self.spill }
    }

    /// The stream a host argument names: 0 the owner's own slot, `STREAM_CONSUMED | k` the running pipeline's consumed
    /// slot `k` (D1324), else `slot + 1`.
    fn stream_of(&self, arg: u64) -> Result<u8> {
        // SAFETY: the table is a heap box the Node owns for its whole life.
        let nslots = unsafe { (*self.spill).streams.len() };
        let arg = match arg {
            a if a & STREAM_CONSUMED != 0 => match self.consumes.get((a & !STREAM_CONSUMED) as usize) {
                Some(&c) => c as u64 + 1,
                None => return Err(Error::host(format!("gravity: spill: consumed stream {} of a pipeline consuming {}", a & !STREAM_CONSUMED, self.consumes.len()))), // alloc: Cold
            },
            a => a,
        };
        // D1324 (debug): a slot named by number is the pipeline's own or one it consumes (S5: a spill-partition pipe reads
        // the slots before it); a number baked before lowerv4 renumbered a deferred slot would read another life's runs
        debug_assert!(arg == 0 || self.spill_parts || arg - 1 == self.own_slot as u64 || self.consumes.contains(&((arg - 1) as u8)), "gravity: spill: stream of slot {} the running pipeline does not consume (D1324)", arg.wrapping_sub(1));
        match arg {
            0 => Ok(self.own_slot),
            // a consumed stream: only its live life's (S5: never one whose life ended or never began)
            n if (n as usize) <= nslots => unsafe { (*self.spill).live_stream(n as u8 - 1) }.map(|_| n as u8 - 1),
            n => Err(Error::host(format!("gravity: spill: stream {n} of {nslots} slots"))), // alloc: Cold
        }
    }
}

/// The spill census (diagnostic): scatter ns / count, refill ns / count, summed over every thread that ran them.
pub static SPILL_CENSUS: [core::sync::atomic::AtomicU64; 4] = [const { core::sync::atomic::AtomicU64::new(0) }; 4];

/// The runs put (`SPILL_RUN_PUT`) and their bytes, summed over every thread; the section line reads and clears them.
pub static RUN_CENSUS: [core::sync::atomic::AtomicU64; 2] = [const { core::sync::atomic::AtomicU64::new(0) }; 2];

fn census(k: usize, t0: std::time::Instant) {
    use core::sync::atomic::Ordering::Relaxed;
    SPILL_CENSUS[k].fetch_add(t0.elapsed().as_nanos() as u64, Relaxed);
    SPILL_CENSUS[k + 1].fetch_add(1, Relaxed);
}

/// spillopt #5 (hybrid T1): a kernel's full phase-1 pool writes some partitions and keeps the rest
/// (`SpillTable::scatter_own_destaging`); `PGRUST_GRAVITY_T1_DESTAGE=0` writes whole pools as before. Read once.
fn destaging() -> bool {
    static ON: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *ON.get_or_init(|| crate::env::text("PGRUST_GRAVITY_T1_DESTAGE").is_none_or(|v| v.trim() != "0"))
}

unsafe fn spill_op(n: &mut Inner, op: u32, arg: u64) -> Result<i64> {
    match op {
        SPILL_SCATTER => {
            let bits = arg as u8;
            if arg == 0 || bits > 11 {
                return Err(Error::host(format!("gravity: spill: a scatter at {bits} partition bits"))); // alloc: Cold
            }
            let region = n.own_region(0)?;
            let (index, slot) = (n.spill_index, n.own_slot);
            let t0 = std::time::Instant::now();
            // spillopt #5 (hybrid T1): a phase-1 table's full pool keeps some partitions in memory; C5: a revoked one writes
            // them all, so its pool's growth can be given back
            let revoked = crate::mem::grow::home_cut(n);
            let destage = !revoked && destaging() && gravity_abi::breaker::table_kind(region) == gravity_abi::breaker::TABLE_T1;
            let r = if destage { n.spill_table().scatter_own_destaging(slot, index, region) } else { n.spill_table().scatter_own(slot, index, region, bits) };
            census(0, t0);
            if revoked && r.is_ok() {
                crate::mem::grow::give_back_scattered(n, region);
            }
            r.map(|c| c as i64)
        }
        SPILL_REFILL => {
            let token = arg as u32;
            let slot = n.stream_of(arg >> 32)?;
            let region = n.own_region(n.refill_off)?;
            let owner = n.spill_index;
            let t0 = std::time::Instant::now();
            let r = n.spill_table().refill(slot, owner, token, region).map(|c| c as i64);
            census(2, t0);
            r
        }
        SPILL_TARGET => {
            n.refill_off = arg as usize;
            Ok(OK)
        }
        SPILL_SPLIT => {
            let token = arg as u32;
            let slot = n.stream_of(arg >> 32)?;
            let region = n.own_region(n.refill_off)?;
            let owner = n.spill_index;
            n.spill_table().split_held(slot, owner, token, region).map(|c| c as i64)
        }
        SPILL_RUN_PUT => {
            // D1330: a non-zero arg is a var sort's big run (a buffer this owner drew, `merge_big`), else the own region
            let region = if arg == 0 {
                n.own_region(n.refill_off)?
            } else {
                debug_assert!(n.bigs.iter().any(|b| b.0 != 0 && b.1 == arg as usize), "a run put of a region this sort does not own");
                if !n.bigs.iter().any(|b| b.0 != 0 && b.1 == arg as usize) {
                    return Err(Error::host(format!("gravity: spill: a run put of region {arg:#x}, not a big run of this owner"))); // alloc: Cold
                }
                arg as usize as *mut u8
            };
            let (index, slot) = (n.spill_index, n.own_slot);
            let bytes = crate::mem::spill::dense_rows(region).map_or(0, |(_, b)| b as u64);
            let r = n.spill_table().run_put(slot, index, region).map(|r| r as i64);
            RUN_CENSUS[0].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            RUN_CENSUS[1].fetch_add(bytes, core::sync::atomic::Ordering::Relaxed);
            r
        }
        // D2572-c: a var sort's run rows' layout, before its puts (the words `[row bytes, rec_off, nvar, slot offsets]`)
        SPILL_RUN_SHAPE => {
            // advisory (the kernel ignores the answer): a shape the codec cannot take leaves the rows as they lie
            let w = arg as usize as *const u32;
            if w.is_null() || *w.add(2) > u16::MAX as u32 {
                return Ok(OK);
            }
            let (head, rec_off, nvar) = (*w, *w.add(1), *w.add(2) as usize);
            let slots = core::slice::from_raw_parts(w.add(3), nvar).to_vec(); // alloc: Exec
            if let Some(shape) = crate::mem::runcodec::RowShape::new(head, Some(crate::mem::runcodec::VarRows { rec_off, slots })) {
                let (index, slot) = (n.spill_index, n.own_slot);
                n.spill_table().run_shape(slot, index, shape);
            }
            Ok(OK)
        }
        SPILL_RUN_ADOPT => {
            let (from, to) = (n.stream_of(arg)?, n.own_slot);
            n.spill_table().adopt_runs(from, to).map(|r| r as i64)
        }
        SPILL_RUNS => {
            let slot = n.stream_of(arg)?;
            Ok(n.spill_table().stream(slot).total_runs() as i64)
        }
        // D3259: the leader's seal answer for this section end, the same for every worker
        SPILL_SEAL_OVER => {
            let slot = n.stream_of(arg)?;
            // D4373: with the rows the answer read (`Stream::seal_word`)
            Ok(n.spill_table().stream(slot).seal_word())
        }
        // D1327: a merge pass's run (runmerge `merge_pass`), kept apart from the put runs
        SPILL_MERGE_BEGIN => {
            let slot = n.stream_of(arg & 0xffff_ffff)?;
            n.spill_table().merge_begin(slot, ((arg >> 32) & 0x3fff_ffff) as u32, arg & (1 << 62) != 0).map(|i| i as i64)
        }
        SPILL_MERGE_WRITE => {
            let w = arg as usize as *const u64;
            let (stream, token, ptr, len) = (*w, *w.add(1) as u32, *w.add(2) as usize as *const u8, *w.add(3) as usize);
            let slot = n.stream_of(stream)?;
            n.spill_table().merge_write(slot, token, core::slice::from_raw_parts(ptr, len)).map(|()| OK)
        }
        SPILL_RUN_ROWS => {
            let slot = n.stream_of(arg)?;
            Ok(n.spill_table().stream(slot).total_rows() as i64)
        }
        SPILL_RUN_OPEN => {
            let run = arg as u32;
            let slot = n.stream_of(arg >> 32)?;
            let reader = n.spill_table().run_open(slot, run)?;
            let handle = n.readers.iter().position(Option::is_none).unwrap_or_else(|| {
                n.readers.push(None); // alloc: Exec
                n.readers.len() - 1
            });
            n.readers[handle] = reader;
            Ok(handle as i64)
        }
        // D3190: a co-split claim's slice of one put run (mem/fence.rs `slice_of`), a handle as `SPILL_RUN_OPEN`'s. The runs
        // are frozen past their producer and every worker reads them at once, so the table is borrowed shared.
        SPILL_RUN_SLICE => {
            let w = arg as usize as *const u64;
            let (stream, token, lo, hi) = (*w, *w.add(1) as u32, *w.add(2), *w.add(3));
            let slot = n.stream_of(stream)?;
            let t: &SpillTable = &*n.spill;
            let Some(fences) = t.run_fences(slot, token) else {
                return Err(Error::host(format!("gravity: spill: a slice of run {token:#x} of slot {slot}, put without fences"))); // alloc: Cold
            };
            let open = |f: usize| t.run_records(slot, token, Some(f))?.ok_or_else(|| Error::host("gravity: spill: a fenced run without its records"));
            let reader = match crate::mem::fence::slice_of(fences, &open, lo, hi)? {
                Some(r) => Some(Box::new(r) as Box<dyn crate::mem::spill::RunReader>), // alloc: Exec
                None => None,
            };
            let handle = n.readers.iter().position(Option::is_none).unwrap_or_else(|| {
                n.readers.push(None); // alloc: Exec
                n.readers.len() - 1
            });
            n.readers[handle] = reader;
            Ok(handle as i64)
        }
        SPILL_RUN_FILL => {
            let handle = (arg & 0xffff) as usize;
            let offset = ((arg >> 16) & 0xffff_ffff) as usize;
            let cap = ((arg >> 48) & 0xffff) as usize;
            let own = if n.fill_base.is_null() { (*n.partials).own.ptr() } else { n.fill_base };
            if own.is_null() || handle >= n.readers.len() {
                return Err(Error::host(format!("gravity: spill: a run fill into {} through reader handle {handle} of {} open (offset {offset}, cap {cap})", if own.is_null() { "no produced table" } else { "the own table" }, n.readers.len()))); // alloc: Cold
            }
            let Some(r) = n.readers[handle].as_mut() else { return Ok(0) };
            let buf = core::slice::from_raw_parts_mut(own.add(offset), cap);
            let got = r.read(buf)?;
            if got == 0 {
                n.readers[handle] = None;
            }
            Ok(got as i64)
        }
        SPILL_RUN_BASE => {
            if arg != 0 {
                fires::fire_shared("merge.inner_merged_run"); // shapecheck: merge.join's inner read through one merged run's window (kernels mergejoin window.rs), whichever fill put the run
            }
            n.fill_base = arg as usize as *mut u8;
            Ok(OK)
        }
        SPILL_RUN_CLOSE => {
            match n.readers.get_mut(arg as usize) {
                Some(r) => *r = None,
                None => return Err(Error::host(format!("gravity: spill: a run close of handle {arg} of {} open", n.readers.len()))), // alloc: Cold
            }
            Ok(OK)
        }
        // NEW-43: an INCSTREAM reader past its merge (kernels `SortStreamRearm`): the stream's runs dropped, its life kept
        gravity_abi::program::SPILL_RUN_DROP => {
            let slot = n.stream_of(arg)?;
            n.spill_table().drop_runs(slot);
            Ok(OK)
        }
        SPILL_PARTS => {
            let slot = n.stream_of(arg)?;
            let owner = n.spill_index;
            Ok(n.spill_table().stream(slot).parts(owner) as i64)
        }
        SPILL_DROP => {
            let (index, slot) = (n.spill_index, n.own_slot);
            let t = n.spill_table();
            if let Some(f) = t.streams[slot as usize].files.get_mut(index) {
                *f = None;
            }
            Ok(OK)
        }
        SPILL_REGION_GROW => {
            // SAFETY: `arg` is the address of the IR host's `[need, head, tail]` (`gravity_abi::program::region_grow`).
            let [need, head, tail] = *(arg as usize as *const [u64; 3]);
            // NR-6: a draw the debt lever refuses (`grow::debt_draw`) is the grow's error
            Ok(match crate::mem::grow::uncapped_op(n, need, head, tail, None).and_then(|()| crate::mem::grow::region_grow(n, need, head, tail)) {
                Ok(base) => base as i64,
                Err(e) => {
                    // a grow its wait refused for the statement's cancel is the cancel (`hostimpl::stopped`)
                    n.err = Some(crate::hostimpl::stopped(e));
                    0
                }
            })
        }
        // C10: `SPILL_REGION_GROW` on a spilling breaker's road that names its debt (`gravity_abi::program::region_grow_debt`)
        gravity_abi::program::SPILL_REGION_GROW_DEBT => {
            // SAFETY: `arg` is the address of the host's `[need, head, tail, why]` (`gravity_abi::program::region_grow_debt`).
            let [need, head, tail, why] = *(arg as usize as *const [u64; 4]);
            // a code no row has is an unnamed road
            let why = u8::try_from(why).ok().and_then(gravity_program::spill::NoSpill::from_code);
            Ok(match crate::mem::grow::uncapped_op(n, need, head, tail, why).and_then(|()| crate::mem::grow::region_grow(n, need, head, tail)) {
                Ok(base) => base as i64,
                Err(e) => {
                    // refused, the road's row names it (`grow::debt_refused`); for the statement's cancel, the cancel
                    n.err = Some(crate::hostimpl::stopped(crate::mem::grow::debt_refused(e, None, why)));
                    0
                }
            })
        }
        // D3105 (b): `SPILL_REGION_GROW` within the owner's limit (`grow::region_grow_in`): 1 when the limit refuses it
        gravity_abi::program::SPILL_REGION_GROW_IN => {
            // SAFETY: `arg` is the address of the IR host's `[need, head, tail]` (`gravity_abi::program::region_grow_in`).
            let [need, head, tail] = *(arg as usize as *const [u64; 3]);
            Ok(match crate::mem::grow::region_grow_within(n, need, head, tail) {
                Ok(Some(base)) => base as i64,
                Ok(None) => {
                    fires::fire_shared("merge.inner_merged_run"); // shapecheck: merge.join's inner run put past its owner's limit, read back as one merged run (D3105 (b))
                    1
                }
                Err(e) => {
                    n.err = Some(e);
                    0
                }
            })
        }
        // option c': the calling owner's reserved region asked (0) or committed in place (`grow::region_commit`); never an error
        SPILL_REGION_COMMIT => {
            let r = crate::mem::grow::region_commit(n, arg).unwrap_or(0);
            crate::mem::grow::note_spillable(n);
            Ok(r as i64)
        }
        // a sort run's started-small region grown in place (`grow::region_grow_place`): its new length, 0 refused; never an error
        gravity_abi::program::SPILL_REGION_PLACE => {
            // SAFETY: `arg` is the address of the IR host's `[need, head]` (`gravity_abi::program::region_grow_place`).
            let [need, head] = *(arg as usize as *const [u64; 2]);
            let r = crate::mem::grow::region_grow_place(n, need, head);
            crate::mem::grow::note_spillable(n);
            Ok(r as i64)
        }
        // op 14: the held (deferred) error raised on this thread, its host payload stashed again; 0 when nothing is held
        SPILL_RAISE_HELD => match n.spill_table().raise_held() {
            Some(e) => Err(e),
            None => Ok(0),
        },
        _ => Err(Error::host(format!("gravity: spill: unknown op {op}"))), // alloc: Cold
    }
}

/// D3887: the poll-gap probe's label of each spill op, by op number (`gravity_abi::program::SPILL_*`).
const OP_LABELS: [&str; 29] = [
    "spill scatter", "spill refill", "spill parts", "spill drop", "spill target", "spill run put", "spill runs",
    "spill run open", "spill run fill", "spill split", "spill run base", "spill run close", "spill run adopt",
    "spill region grow", "spill raise held", "spill run rows", "spill out grow", "spill merge begin", "spill merge write",
    "spill merge big", "spill run shape", "spill region commit", "spill run drop", "spill region grow in", "spill run slice",
    "spill seal over", "spill region place", "spill work take", "spill work give",
];
const _: () = assert!(gravity_abi::program::SPILL_WORK_GIVE as usize == OP_LABELS.len() - 1);

/// D3960: the spill ops that poll the statement's cancel at their start (each fails as its I/O would); the bookkeeping
/// ops (a drop, a close, a target, a count) never do.
const POLLS: [u32; 9] = [SPILL_SCATTER, SPILL_REFILL, SPILL_SPLIT, SPILL_RUN_PUT, SPILL_RUN_OPEN, SPILL_RUN_SLICE, SPILL_RUN_FILL, SPILL_MERGE_BEGIN, SPILL_MERGE_WRITE];

gravity_abi::host! {
    /// `Program.spill(ctx, op, arg)` (`breakers.md`): `SCATTER bits` the own table into the owner's
    /// file of its slot; `REFILL token | (slot + 1) << 32` refills that stream's token into the own
    /// table (a split appends tokens); `SPLIT token | (slot + 1) << 32` splits the partition a refill
    /// left in the target (the join pairs a build split with the outer's); `PARTS (slot + 1 | 0)` the
    /// stream's token count; `DROP`.
    /// # Safety — `ctx` is a bound ctx.
    pub unsafe fn spill(ctx: *mut Ctx, op: u32, arg: u64) -> i64 {
        guarded((*(*ctx).program.ptr()).env.ptr(), || {
            // C9: the op's step labels the probe for its whole call (its own steps nest under it)
            let mut step = cancelpoll::step(OP_LABELS.get(op as usize).copied().unwrap_or("spill op"));
            // D3960: an op that reads, writes or sorts spilled rows ticks at its start: a kernel's run of short ones (a
            // merge's run opens and fills, ~150 ms each under reclaim) polled nowhere between them (11.9 s)
            if POLLS.contains(&op) && step.tick().is_err() {
                return fail(inner_of(ctx), Error::Cancelled);
            }
            if op == SPILL_OUT_GROW {
                return out_grow(ctx, arg);
            }
            if op == SPILL_MERGE_BIG {
                return merge_big(ctx, arg);
            }
            if op == gravity_abi::program::SPILL_WORK_TAKE {
                return work_take(ctx, arg as usize);
            }
            if op == gravity_abi::program::SPILL_WORK_TRY {
                return work_within(ctx, arg as usize, false, false);
            }
            if op == gravity_abi::program::SPILL_WORK_WITHIN {
                return work_within(ctx, (arg & !gravity_abi::program::WORK_SEAL) as usize, true, arg & gravity_abi::program::WORK_SEAL != 0);
            }
            if op == gravity_abi::program::SPILL_WORK_GIVE {
                return work_give(ctx, arg as usize);
            }
            if op == gravity_abi::program::SPILL_OWNED_NEXT {
                return owned_next(ctx, arg);
            }
            if op == gravity_abi::program::SPILL_ARENA_CHUNK {
                return arena_chunk(ctx, arg);
            }
            if op == gravity_abi::program::SPILL_ARENA_GIVE {
                return arena_give(ctx, arg as usize);
            }
            let n = inner_of(ctx);
            match spill_op(n, op, arg) {
                Ok(v) => v,
                Err(e) => fail(n, e),
            }
        }, |status| status)
    }
}

/// D645 (`SPILL_OUT_GROW`, never a hot path: a row wider than the whole out region): the out block's region swapped for one
/// of at least `bytes` (twice the old at least), drawn as D641's result spill (`arena::spill_region`: W, else G, else D225's
/// reserve past both) and held until the statement's end, where `settle_regions` puts the block's own region back
/// (`Inner::rehome_out`). The sink pushed the rows before, so the swapped region holds no image. F5 (memlead's A5
/// re-audit, R2): past the grant pool the region is a session temp file mapped shared (`v4::take`, D3021's road: D491's
/// `map_owners`, temp_file_limit charged, held to the statement's end), as PostgreSQL's one row past work_mem is its own
/// palloc. The new capacity, or 0 with OutOfMemory held when the session has no file road (the file's own error, 53400 past
/// temp_file_limit, when it refuses; the error a refused draw stashed first stays).
/// # Safety — `ctx` is a bound ctx; `spill_region` takes its owner (no borrow of it is live across the call).
unsafe fn out_grow(ctx: *mut Ctx, bytes: u64) -> i64 {
    let o = (*ctx).out.ptr();
    let want = (bytes.max(2 * (*o).region_cap as u64) as usize).next_multiple_of(8);
    let p = v4::take(ctx, want);
    if p.is_null() {
        return 0;
    }
    (*o).region = gravity_abi::word(p);
    (*o).region_cap = want as i64;
    (*o).region_len = 0;
    want as i64
}

/// C10 2b (D3582, `SPILL_OWNED_NEXT`): the running emit's first consumed slot, batched, refilled (mem/batch.rs `refill`):
/// its batch's emit state, 0 past the last batch, or the error's status with the error held.
/// # Safety — `ctx` is a bound ctx; `refill` takes its owner (no borrow of it is live across the call).
unsafe fn owned_next(ctx: *mut Ctx, arg: u64) -> i64 {
    let at = {
        let n = inner_of(ctx);
        n.stream_of(STREAM_CONSUMED).map(|slot| (slot, n.spill))
    };
    match at.and_then(|(slot, spill)| crate::mem::batch::refill(ctx, spill, slot, arg)) {
        Ok(v) => v,
        Err(e) => fail(inner_of(ctx), e),
    }
}

/// D1327 (`SPILL_MERGE_BIG`, never a hot path: a sort record longer than its run's share of the merge region): `arg` the
/// address of `[merge region, bytes]`. `bytes` 0 frees the region's big buffers for reuse (a merge laid anew: its runs'
/// buffers are its own); else a free buffer of at least `bytes`, or one drawn as `out_grow` draws (F5: a mapped temp file
/// past the grant pool), held until the statement's end, so a sort run again (a rescan) reuses what its first run drew.
/// Its address, or 0 with OutOfMemory (or the file's own error) held.
/// # Safety — `ctx` is a bound ctx; `arg` points at two words.
unsafe fn merge_big(ctx: *mut Ctx, arg: u64) -> i64 {
    let w = arg as usize as *const u64;
    let (key, bytes) = (*w as usize, *w.add(1) as usize);
    let n = inner_of(ctx);
    if bytes == 0 {
        n.bigs.iter_mut().filter(|b| b.0 == key).for_each(|b| b.0 = 0);
        return OK;
    }
    // D1330: a lookup: the buffer `key` holds (a var sort's big run in flight), else 0
    if bytes == usize::MAX {
        return n.bigs.iter().find(|b| b.0 == key).map_or(0, |b| b.1 as i64);
    }
    if let Some(b) = n.bigs.iter_mut().filter(|b| b.0 == 0 && b.2 >= bytes).min_by_key(|b| b.2) {
        b.0 = key;
        let p = b.1;
        own_reused(n, p);
        return p as i64;
    }
    let want = bytes.next_multiple_of(8);
    let p = v4::take(ctx, want);
    if p.is_null() {
        return 0;
    }
    let n = inner_of(ctx);
    n.bigs.push((key, p as usize, want)); // alloc: Exec
    p as i64
}

/// D3644 (b) (`SPILL_WORK_TAKE`): as `merge_big`, keyed by the block's own address; 1 where mempolicy leaves it to the
/// allocator (`grow::work_from_grant`).
/// # Safety — `ctx` is a bound ctx.
unsafe fn work_take(ctx: *mut Ctx, bytes: usize) -> i64 {
    if !mempolicy::grow::work_from_grant(bytes as u64) {
        return 1;
    }
    let n = inner_of(ctx);
    if let Some(b) = n.bigs.iter_mut().filter(|b| b.0 == 0 && b.2 >= bytes).min_by_key(|b| b.2) {
        b.0 = b.1;
        let p = b.1;
        own_reused(n, p);
        return p as i64;
    }
    let want = bytes.next_multiple_of(8);
    let p = v4::take_unzeroed(ctx, want);
    if p.is_null() {
        return 0;
    }
    inner_of(ctx).bigs.push((p as usize, p as usize, want)); // alloc: Exec
    p as i64
}

/// LEDGER2 step 5: a free block of this owner's (`Inner::bigs`, a `spills` region or an `owner_maps` mapping) taken again
/// for the running pipeline: charged to its slot's owner now (a block an earlier pipeline drew stays with whom it is lent to).
fn own_reused(n: &mut crate::node::Inner, p: usize) {
    let Some(o) = n.op_owner else { return };
    if let Some(c) = n.spills.iter_mut().find(|c| c.as_ptr() as usize == p) {
        c.own_by(o);
    } else if let Some(m) = n.owner_maps.iter_mut().find(|m| m.as_mut_ptr() as usize == p) {
        m.own_by(o);
    }
}

/// LEDGER2 step 3 (`SPILL_ARENA_CHUNK`): an operator arena's chunk, drawn as `work_take`'s (`v4::take_unzeroed`: W, then G,
/// then past G a mapped temp file) and held as its region until `arena_give` or the statement's end; its charge moves to
/// the arena's owner (`arg`'s owner words) here, once per chunk, so every byte the arena carves from it is that owner's.
/// Its address, 0 when refused (the refusal held for the raise) or the owner words are no owner's.
/// # Safety — `ctx` is a bound ctx; `arg` points at three words.
unsafe fn arena_chunk(ctx: *mut Ctx, arg: u64) -> i64 {
    let [bytes, w0, w1] = *(arg as usize as *const [u64; 3]);
    // LEDGER2 step 5 (`bytes` 0, `gravity_abi::program::arena_slot_owner`): the owner words of the breaker slot this
    // program block's pipeline runs for, written into the arg; 1, or 0 when it has none. CONVERT wave 2 (R3): the slot the
    // pipeline PRODUCES into (`Inner::feed_owner`), else the statement's (or the portal's) owner the Node took at its bind:
    // a kernel emit feeding a sink is the statement's, never `own_slot_of`'s consumed slot or slot 0. Word 0 carries the
    // ledger kind its bytes count under (`arena_slot_owner_kind`): the slot's breaker kind, `Kind::Region` for the statement.
    // CONVERT wave 2 (commit 7): `w0` = `ARENA_ASK_OWN` asks the pipeline's OWN slot's owner and kind instead
    // (`arena_own_owner_kind`: `Inner::op_owner`, the owner its grant-road blocks charge; an emit's is the breaker it reads)
    if bytes == 0 {
        let n = inner_of(ctx);
        let ans = match w0 {
            gravity_abi::program::ARENA_ASK_OWN => n.op_owner.zip(n.own_kind),
            _ => n.feed_owner.or(n.stmt_owner.map(|s| (s.0, memledger::Kind::Region))),
        };
        let Some((o, kind)) = ans else { return 0 };
        let w = o.words();
        *(arg as usize as *mut [u64; 3]) = [kind as u64, w[0], w[1]];
        return 1;
    }
    let Some(owner) = memledger::Owner::from_words([w0, w1]) else { return 0 };
    let p = v4::take_unzeroed(ctx, (bytes as usize).next_multiple_of(8));
    if p.is_null() {
        return 0;
    }
    let n = inner_of(ctx);
    if let Some(i) = n.spills.iter().rposition(|c| c.as_ptr() == p as *const u8) {
        n.spills[i].own_by(owner);
    } else if let Some(i) = n.owner_maps.iter().rposition(|m| m.as_mut_ptr() == p) {
        n.owner_maps[i].own_by(owner);
    }
    p as i64
}

/// LEDGER2 step 3 (`SPILL_ARENA_GIVE`): an arena chunk back now (`v4::give`: a pool region dropped, its charge with it; a
/// mapped file unmapped); a pointer this owner never took (a chunk the statement's end already dropped): nothing.
/// # Safety — `ctx` is a bound ctx; nothing reads the chunk at `p` after.
unsafe fn arena_give(ctx: *mut Ctx, p: usize) -> i64 {
    if p != 0 && v4::taken(ctx, p as *const u8) {
        v4::give(ctx, p as *mut u8);
    }
    OK
}

/// D4373 (`SPILL_WORK_TRY`, `SPILL_WORK_WITHIN`): a sort's block within its grant: a free block of the statement's first,
/// else W, else G (`arena::spill_region_within`), never the grant pool past G; 1 where mempolicy leaves it to the
/// allocator. Past W and G, `SPILL_WORK_WITHIN` (`keep`: a block the sort cannot do without) takes the pool's room now when
/// `seal` (`WORK_SEAL`: a run sealed in memory, as PostgreSQL's in-memory sort holds its rows), then the statement's mapped
/// temp file (`map_owners`, the road `SPILL_WORK_TAKE` takes past the pool; with no temp-file road `SPILL_WORK_TAKE`'s own
/// road), its refusal held for the emit's raise; `SPILL_WORK_TRY` (the permutation's copy) answers 0, nothing held: the
/// rows are permuted in place.
/// # Safety — `ctx` is a bound ctx.
unsafe fn work_within(ctx: *mut Ctx, bytes: usize, keep: bool, seal: bool) -> i64 {
    if !mempolicy::grow::work_from_grant(bytes as u64) {
        return 1;
    }
    let n = inner_of(ctx);
    if let Some(b) = n.bigs.iter_mut().filter(|b| b.0 == 0 && b.2 >= bytes).min_by_key(|b| b.2) {
        b.0 = b.1;
        let p = b.1;
        own_reused(n, p);
        return p as i64;
    }
    let want = bytes.next_multiple_of(8);
    let mut p = crate::hostimpl::arena::spill_region_within(ctx, want, seal);
    // the permutation's copy past W and G: none, the rows are permuted in place (a temp file of the run's bytes would
    // be a spill PostgreSQL's in-memory sort never makes)
    if p.is_null() && keep {
        p = match crate::hostimpl::arena::map_owners(ctx, want) {
            Ok(m) if !m.is_null() => {
                v4::SPILL_MAPPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                inner_of(ctx).work_maps.push(m as usize); // alloc: Exec
                m
            }
            Ok(_) => v4::take_unzeroed(ctx, want),
            Err(e) => {
                inner_of(ctx).err.get_or_insert(e);
                core::ptr::null_mut()
            }
        };
    }
    if p.is_null() {
        return 0;
    }
    inner_of(ctx).bigs.push((p as usize, p as usize, want)); // alloc: Exec
    p as i64
}

/// D3644 (b): a pool block back to the pool now; a mapped file held free to the statement's end (the report counts it).
/// # Safety — `ctx` is a bound ctx; nothing reads the block at `p` after.
unsafe fn work_give(ctx: *mut Ctx, p: usize) -> i64 {
    let n = inner_of(ctx);
    let Some(i) = n.bigs.iter().position(|b| b.1 == p) else { return OK };
    // D4373: a sort's block mapped past W and G (`work_within`) is unmapped now: its file's pages go with it
    if let Some(w) = n.work_maps.iter().position(|&m| m == p) {
        n.work_maps.swap_remove(w);
        n.bigs.swap_remove(i);
        crate::hostimpl::arena::unmap_owners(ctx, p as *mut u8);
        return OK;
    }
    if n.owner_maps.iter().any(|m| m.as_mut_ptr() as usize == p) {
        n.bigs[i].0 = 0;
    } else {
        n.bigs.swap_remove(i);
        v4::give(ctx, p as *mut u8);
    }
    OK
}

/// M7-SPILL (V4R-359): the host side of a v4 kind-7 build's Grace spill over the S5 streams, called on the calling owner's
/// ctx by lowerv4 join/build_spill.rs's IR hosts. `stream` names a stream as the spill op's argument does (0 the owner's own
/// slot, else `slot + 1`, a live life's only); every file is the owner's stored file of that slot (`file_stored`: build
/// partitions `0 .. 1 << bits`, kept probe rows past them). A failure returns `FAILED` with the error held on the owner for
/// the IR's raise; a body that unwinds is caught (`guarded`).
pub mod v4 {
    use gravity_abi::{Ctx, PtrWord};

    use super::{guarded, inner_of};
    use crate::{Error, Result};

    pub const FAILED: i64 = -1;

    unsafe fn run(ctx: *mut Ctx, body: impl FnOnce(&mut crate::node::Inner) -> Result<i64>) -> i64 {
        guarded((*(*ctx).program.ptr()).env.ptr(), || {
            let n = inner_of(ctx);
            match body(n) {
                Ok(v) => v,
                Err(e) => {
                    super::fail(n, e);
                    FAILED
                }
            }
        }, |_| FAILED)
    }

    /// Decision 5 (a): the owner's produced region grown within its limit (grow.rs `region_grow_in`): the new base, 1 when
    /// the limit refused it (nothing drawn, no error), `FAILED` on a real failure.
    /// # Safety — `ctx` is bound, its running pipeline produces a region, head + tail <= its length.
    pub unsafe fn grow_in(ctx: *mut Ctx, need: u64, head: u64, tail: u64) -> i64 {
        run(ctx, |n| {
            let r = crate::mem::grow::region_grow_in(n, need, head, tail);
            crate::mem::grow::note_spillable(n);
            r.map(|b| b.map_or(1, |b| b as i64))
        })
    }

    /// D4265: the length a cut kind-7 build at `region` shrinks to before it gives its growth back (grow.rs `build_keep`):
    /// 0 when the owner's home is not cut, or the build cannot give back (counted there).
    /// # Safety — `ctx` is bound, `region` the build its running pipeline fills.
    pub unsafe fn cut_keep(ctx: *mut Ctx, region: *const u8) -> u64 {
        run(ctx, |n| Ok(crate::mem::grow::build_keep(n, region) as i64)).max(0) as u64
    }

    /// D4265: a cut kind-7 build at `region`, every kept record and image within `keep` bytes when `fits`, gives back what
    /// it committed past them (grow.rs `give_back_build`); the bytes given.
    /// # Safety — `ctx` is bound, `region` the build its running pipeline fills, nothing of it read past `keep` from now.
    pub unsafe fn give_back(ctx: *mut Ctx, region: *mut u8, keep: u64, fits: bool) -> u64 {
        run(ctx, |n| Ok(crate::mem::grow::give_back_build(n, region, keep, fits) as i64)).max(0) as u64
    }

    /// `bytes` staged for partition `part` of the owner's file of `stream` (`SpillTable::stage_put`); 0.
    /// # Safety — `ctx` is bound.
    pub unsafe fn put(ctx: *mut Ctx, stream: u64, bits: u8, part: u32, bytes: &[u8]) -> i64 {
        run(ctx, |n| {
            // D3967 / D3968 / D3972: a put, flush or read ticks at its start: a build's reload or stage loop of calls
            // that each fit one chunk polled nowhere between them (1.3 s on the stress run)
            cancelpoll::step("spill v4 put: a chunk staged").tick()?;
            let (slot, index) = (n.stream_of(stream)?, n.spill_index);
            n.spill_table().stage_put(slot, index, bits, part, bytes).map(|()| 0)
        })
    }

    /// The owner's staged bytes of `stream` written (`SpillTable::stage_flush`); 0.
    /// # Safety — `ctx` is bound.
    pub unsafe fn flush(ctx: *mut Ctx, stream: u64, bits: u8) -> i64 {
        run(ctx, |n| {
            cancelpoll::step("spill v4 flush: an owner's staged bytes written").tick()?;
            let (slot, index) = (n.stream_of(stream)?, n.spill_index);
            n.spill_table().stage_flush(slot, index, bits).map(|()| 0)
        })
    }

    /// The owners of `stream`'s files (a pair claim reads a partition from each).
    /// # Safety — `ctx` is bound.
    pub unsafe fn owners(ctx: *mut Ctx, stream: u64) -> i64 {
        run(ctx, |n| {
            let slot = n.stream_of(stream)?;
            Ok(n.spill_table().owners(slot) as i64)
        })
    }

    /// A reader over owner `owner`'s partition `part` of `stream`: its handle + 1, 0 when that owner wrote nothing there.
    /// A read must not miss the caller's own staged bytes: they are flushed first.
    /// # Safety — `ctx` is bound.
    pub unsafe fn open(ctx: *mut Ctx, stream: u64, owner: usize, part: u32) -> i64 {
        run(ctx, |n| {
            let (slot, index) = (n.stream_of(stream)?, n.spill_index);
            if n.spill_table().staged(slot, index) {
                return Err(Error::host("gravity: a kind-7 spill read before its owner's staged bytes were flushed"));
            }
            let Some(r) = n.spill_table().open_stored(slot, owner, part)? else { return Ok(0) };
            let h = n.readers.iter().position(Option::is_none).unwrap_or_else(|| {
                n.readers.push(None); // alloc: Exec
                n.readers.len() - 1
            });
            n.readers[h] = Some(r);
            Ok(h as i64 + 1)
        })
    }

    /// Up to `buf.len()` bytes of reader `handle` (open's value - 1); 0 at its end, where the reader is dropped.
    /// # Safety — `ctx` is bound.
    pub unsafe fn read(ctx: *mut Ctx, handle: usize, buf: &mut [u8]) -> i64 {
        run(ctx, |n| {
            cancelpoll::step("spill v4 read: a reader's chunk").tick()?;
            let Some(Some(r)) = n.readers.get_mut(handle) else { return Err(Error::host(format!("gravity: a kind-7 spill read of reader {handle}, not open"))) }; // alloc: Cold
            let got = r.read(buf)?;
            if got == 0 {
                n.readers[handle] = None;
            }
            Ok(got as i64)
        })
    }

    /// Reader `handle` dropped before its end (a cancelled or failed pair claim); 0.
    /// # Safety — `ctx` is bound.
    pub unsafe fn close(ctx: *mut Ctx, handle: usize) -> i64 {
        run(ctx, |n| {
            if let Some(r) = n.readers.get_mut(handle) {
                *r = None;
            }
            Ok(0)
        })
    }

    /// A region of `bytes` for a pair claim's reloaded partition or its kept rows, drawn as `arena::spill_region` draws (W,
    /// else G, else D225's reserve past both: named growth) and held by the owner until `give` or the statement's end; null
    /// with OutOfMemory held when the pool refuses and the session has no file road. D3021: past the pool the region is a
    /// session temp file mapped shared (D491's road, `arena::map_owners`: temp_file_limit charged, never the pool's), so a
    /// partition no split separates (one join key whose rows pass the grant: they share every route bit) reloads whole as
    /// PostgreSQL's batch file does, instead of raising; the page cache writes it back under memory pressure.
    /// # Safety — `ctx` is bound.
    pub unsafe fn take(ctx: *mut Ctx, bytes: usize) -> *mut u8 {
        take_road(ctx, bytes, true)
    }

    /// D3644 (b): `take` whose pool region is not zeroed (`spill_region_unzeroed`; a mapped file is zero as made).
    /// # Safety — `ctx` is bound; the taker writes every byte it reads.
    pub unsafe fn take_unzeroed(ctx: *mut Ctx, bytes: usize) -> *mut u8 {
        take_road(ctx, bytes, false)
    }

    /// D4425: a region of `bytes` for a spilled hash join's reload pass (lowerv4 hashjoin spill/raw/load.rs: a partition's
    /// reload buffer, its kept rows' offsets, the pair's laid-out region; spill/raw.rs: the split's drain records), zeroed,
    /// held as `take`'s until `give` or the statement's end. In a session with a temp-file road: W, else G, else the pool's
    /// room as it is now (`arena::spill_region_now`: no wait, no refusal kept), else the statement's mapped temp file, as
    /// D4373's sort block (`work_within`). On `take`'s road every reload past W and G waited out the pool's draw budget
    /// (`GrantPool::take_wait`, 2 s) while another statement held the pool, and then mapped the file anyway: four trimmed
    /// W4 joins in cell e2 (8G) ran 423-446 s, 85% of their workers' off-CPU time in that wait. With no file road, `take`.
    /// # Safety — `ctx` is bound.
    pub unsafe fn take_reload(ctx: *mut Ctx, bytes: usize) -> *mut u8 {
        if !crate::mem::spill::file_road() {
            return take(ctx, bytes);
        }
        let p = crate::hostimpl::arena::spill_region_now(ctx, bytes);
        if !p.is_null() {
            return p;
        }
        map_road(ctx, bytes)
    }

    unsafe fn take_road(ctx: *mut Ctx, bytes: usize, zeroed: bool) -> *mut u8 {
        // ladder step 1.1 (`gate_now`): with a temp-file road past the pool the draw is the gate's `Spills` asker (no wait
        // unless bytes are on their way, no refusal kept), then the mapped file
        let p = if mempolicy::admit::gate_now() && crate::mem::spill::file_road() {
            crate::hostimpl::arena::spill_region_spills(ctx, bytes, zeroed)
        } else if zeroed {
            crate::hostimpl::arena::spill_region(ctx, bytes)
        } else {
            crate::hostimpl::arena::spill_region_unzeroed(ctx, bytes)
        };
        if !p.is_null() {
            return p;
        }
        map_road(ctx, bytes)
    }

    /// D3021: `bytes` past the pool, the statement's temp file mapped shared (D491's `map_owners`); null with the error held
    /// (OutOfMemory when the session has no file road, the file's own error when it refuses).
    /// # Safety — `ctx` is bound.
    unsafe fn map_road(ctx: *mut Ctx, bytes: usize) -> *mut u8 {
        let n = inner_of(ctx);
        match crate::hostimpl::arena::map_owners(ctx, bytes) {
            Ok(m) if !m.is_null() => {
                SPILL_MAPPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                return m;
            }
            Ok(_) => {
                let pool = &crate::mem::grant::POOL;
                n.err.get_or_insert(Error::OutOfMemory { bytes: bytes as u64, held: pool.held(), capacity: pool.capacity(), why: None });
            }
            Err(e) => {
                n.err.get_or_insert(e);
            }
        }
        core::ptr::null_mut()
    }

    /// D3021: the `take`s the grant pool refused that a mapped temp file held instead (process-wide, for tests and reports).
    pub static SPILL_MAPPED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

    /// Whether `p` is the base of a region this owner's `take` gave and no `give` returned yet (an address compare: nothing
    /// at `p` is read).
    /// # Safety — `ctx` is bound.
    pub unsafe fn taken(ctx: *mut Ctx, p: *const u8) -> bool {
        let n = inner_of(ctx);
        n.spills.iter().any(|c| c.as_ptr() == p) || n.owner_maps.iter().any(|m| m.as_mut_ptr() as *const u8 == p)
    }

    /// pagepool P3 witness: whether the region a `take` of this owner gave at `p` is the page pool's (`RegionBuf::is_pool`:
    /// its give drops it back to the pool, never a give-back job); false for any other `p`.
    /// # Safety — `ctx` is bound.
    #[doc(hidden)]
    pub unsafe fn is_pool(ctx: *mut Ctx, p: *const u8) -> bool {
        let n = inner_of(ctx);
        n.spills.iter().any(|c| c.as_ptr() == p && c.is_pool())
    }

    /// A region `take` gave back before the statement's end: its pages returned, the buffer dropped. H3 step (4) C1-a
    /// (give_back_spill): through the give-back service as one job when it takes it (`mem::giveback::give`), else here.
    /// # Safety — `ctx` is bound; `p` is a `take` of this owner no reader holds.
    pub unsafe fn give(ctx: *mut Ctx, p: *mut u8) {
        let n = inner_of(ctx);
        if let Some(i) = n.spills.iter_mut().position(|c| c.as_mut_ptr() == p) {
            let c = n.spills.swap_remove(i);
            n.unmapped(c.len() as u64);
            crate::mem::giveback::give(c);
        } else {
            // D3021: a mapped temp file `take` drew past the pool: unmapped, its blocks given back
            crate::hostimpl::arena::unmap_owners(ctx, p);
        }
    }

    /// An error of the caller's own (a malformed spill record) held for the IR's raise; `FAILED`.
    /// # Safety — `ctx` is bound.
    pub unsafe fn fail(ctx: *mut Ctx, e: Error) -> i64 {
        super::fail(inner_of(ctx), e);
        FAILED
    }
}
