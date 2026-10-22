use std::collections::HashMap;

use gravity_abi::status::{UNSUPPORTED_DOMAIN, UNSUPPORTED_SCRATCH};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use gravity_abi::Datum;
use gravity_driver::{partition_plan, stream_runs, BatchFold, BorrowedBatch, Error, Flush, IovFn, Merge, Partial, PartitionPlan, Region, Result, SpillFile, State};
use gravity_abi::buildrows::RowStore;
use gravity_abi::breaker::{table_kind, Candidates, FloatOverflow, FOLD_HOST, FOLD_OVERFLOW, HashAgg, HashBuild, Owned, OwnerFns, OwnerTop, Rec, RecordLayout, SubSplit, TopK, T1, BUILD_TABLE, SELECT_MAX, TOPK_PAIRS, HASHAGG_HDR, HASHBUILD_ENTRY, HASHBUILD_HDR, MAX_BUILD_PARTS, ORDER_NONE, OWNED_HDR, REDUCE_FLUSH, TABLE_AGG, TABLE_OWNED, TABLE_T1};
pub use gravity_abi::breaker::TOPK_TIES;

/// An owner hook's failing status as the statement's error: a raise (a float merge's overflow, D244) keeps its sqlstate,
/// a key outside the owner's dense domain (D597) its kind, a stop for the statement's cancel (`CANCELLED`: a host step
/// under the byte the owner phase lends) is the cancel, never the scratch wall a grown region folds again; anything else
/// is the scratch wall.
fn reduce_error(v: i64) -> Error {
    match gravity_abi::status::Status::decode(v) {
        gravity_abi::status::Status::Raised { mask } => Error::Raised(mask),
        gravity_abi::status::Status::Cancelled => Error::Cancelled,
        gravity_abi::status::Status::Unsupported { kind: UNSUPPORTED_DOMAIN } => Error::Unsupported(UNSUPPORTED_DOMAIN),
        _ => Error::Unsupported(UNSUPPORTED_SCRATCH),
    }
}

/// A one-record table (the plain aggregate): the worker's record folds into the leader's.
pub struct PlainAggPartial;

impl Merge for PlainAggPartial {
    fn merge_into(&self, leader: &mut State<'_>, partial: Partial<'_>) -> Result<()> {
        let Some((layout, _, _)) = partial_shape(partial.bytes) else { return Ok(()) };
        let pt = unsafe { HashAgg::view(partial.bytes.as_ptr()) };
        // a record opened under SHARE (the window kernels' count) opens the leader under SHARE; a fixed-budget record (every
        // PlainAgg breaker: `Budget::Fixed(layout.size())`) keeps the fold's own limit — the `record` bytes the emit's
        // `record_consumed` re-opens with — whatever the leader's arm: a slot a hash build shared earlier is armed at its open
        // (the chain under a plain aggregate at W4: `HashAgg::at` "hash aggregate shape" at the emit)
        let limit = if pt.flags & gravity_abi::breaker::FLAG_SHARE_LIMIT != 0 { gravity_abi::breaker::SHARE } else { pt.limit };
        let l = unsafe { HashAgg::at(leader.bytes.as_mut_ptr(), &layout, 1, limit) };
        // NEW-numeric-1 (P): partials fold in after the leader's record (a width-1 arm's in plan order), each the newer
        each_record(partial.bytes, &mut |r: Rec<'_>| l.single().fold_newer(&r).map_err(|_| Error::Raised(FloatOverflow::MASK)))
    }
}

/// A worker's partial as records: a record table's used records, or a phase-1 table's table then
/// its partition buffers in partition order (`partitioned-record.md` §3.1); `(layout, held)`.
/// EX-13: the bytes of the records the partials hold together (tables and buffers, records x their layout's size): the
/// owner phase's input.
pub(crate) fn held_bytes(partials: &[Partial<'_>]) -> u64 {
    partials.iter().filter_map(|p| partial_shape(p.bytes)).map(|(l, held, _)| held * l.size() as u64).sum()
}

fn partial_shape(bytes: &[u8]) -> Option<(RecordLayout, u64, u8)> {
    if bytes.len() < HASHAGG_HDR {
        return None;
    }
    // SAFETY: a breaker region at least a header long; the kind byte names the table.
    unsafe {
        match table_kind(bytes.as_ptr()) {
            TABLE_T1 => {
                let t = T1::view(bytes.as_ptr());
                (t.cap != 0).then(|| (t.layout, t.held(), t.pbits))
            }
            TABLE_AGG => {
                let t = HashAgg::view(bytes.as_ptr());
                (t.cap != 0).then(|| (t.layout, t.len as u64, 0))
            }
            _ => None,
        }
    }
}

/// D273: a partial that holds no table of records: a region never laid out (shorter than a header, or its kind byte 0),
/// `Some(1)`; a T1 of no capacity, `Some` of its bits (at least 1). Any other table: `None`.
fn empty_t1(bytes: &[u8]) -> Option<u8> {
    if bytes.len() < HASHAGG_HDR {
        return Some(1);
    }
    // SAFETY: a breaker region at least a header long; the kind byte names the table.
    unsafe {
        match table_kind(bytes.as_ptr()) {
            0 => Some(1),
            TABLE_T1 if T1::view(bytes.as_ptr()).cap == 0 => Some(T1::view(bytes.as_ptr()).pbits.max(1)),
            _ => None,
        }
    }
}

/// C9: a leader fold's records, a tick per `BATCH` (cancelpoll::Step::batch) on the byte the fold's caller lends
/// (driver section.rs `close_section`'s `lend_cancel`).
fn each_record(bytes: &[u8], f: &mut dyn FnMut(Rec<'_>) -> Result<()>) -> Result<()> {
    let mut step = cancelpoll::step("merge merge_into: a partial's records folded");
    let mut k = 0usize;
    // SAFETY: `partial_shape` saw a live table of this kind.
    unsafe {
        if table_kind(bytes.as_ptr()) == TABLE_T1 {
            let t = T1::view(bytes.as_ptr());
            for r in t.records() {
                step.batch(k)?;
                k += 1;
                f(r)?;
            }
            for p in 0..t.parts() as u32 {
                each_chain_record(&mut step, &mut k, t, p, f)?;
            }
            return Ok(());
        }
        let t = HashAgg::view(bytes.as_ptr());
        for r in t.records() {
            step.batch(k)?;
            k += 1;
            f(r)?;
        }
    }
    Ok(())
}

unsafe fn each_chain_record(step: &mut cancelpoll::Step, k: &mut usize, t: &T1, part: u32, f: &mut dyn FnMut(Rec<'_>) -> Result<()>) -> Result<()> {
    let rec = t.layout.size() as usize;
    for b in t.chain(part) {
        for i in 0..b.recs {
            step.batch(*k)?;
            *k += 1;
            f(Rec::at(b.ptr.add(i * rec) as *mut u8, &t.layout))?;
        }
    }
    Ok(())
}

/// The record table's merge: the owner phase (partitioned-record.md §3.2) — `W` owners over the
/// leader region's cuts, or one owner over an arena region when those cannot hold the shape's
/// results (`own_single`, the width-1 arm too); a phase-1 table's partial is never folded serially. The kernels still on
/// a plain `HashAgg` partial (a one-key count group-by, a grouped average, the PGROUPT text group-by) keep the leader fold (`merge_into`).
pub struct HashAggPartial {
    /// The zone top-k's group limit: a worker's table with `limit` keys or more proves
    /// its `limit`-th smallest key as the walk's bound (0 = no walk).
    pub limit: u32,
    /// GP-1b: the key cell's XOR before the bound's select (`!0`: a descending walk, whose bound is the NOT of the
    /// `limit`-th largest key, in the walk's NOT-of-maximum order; 0: ascending).
    pub flip: i64,
    /// The owner phase's reduction (partitioned-record.md §3.2, §3.4): the kernel's functions over
    /// an owner region; `None` keeps the leader fold (the shapes not yet on the owner phase).
    pub owner: Option<OwnerFns>,
    /// D597: the owner's form without its dense domain (the hash reduce), taken from the first domain miss on.
    plain: Option<OwnerFns>,
    /// D597: an owner met a group key outside the domain `owner` was bound with: every open from then on is `plain`'s.
    missed: AtomicBool,
    /// D3075: `kept_run_memo`'s answers, one per form (`owner`, then `plain`): 0 not probed, 1 none, else the probe's word + 2.
    kept: [AtomicU64; 2],
}

impl HashAggPartial {
    pub const fn new(limit: u32, owner: Option<OwnerFns>) -> HashAggPartial {
        HashAggPartial { limit, flip: 0, owner, plain: None, missed: AtomicBool::new(false), kept: [AtomicU64::new(0), AtomicU64::new(0)] }
    }

    /// D597: `owner` bound over a dense domain, `plain` the same form without it. A key outside the domain ends the owner's
    /// round 1 (`UNSUPPORTED_DOMAIN`); the driver's round-1 rerun (the scratch road: section.rs `own_all`, parallel `own_single`)
    /// folds it again from the partials and files it only read, through `plain`.
    pub const fn with_plain(limit: u32, owner: Option<OwnerFns>, plain: Option<OwnerFns>) -> HashAggPartial {
        HashAggPartial { limit, flip: 0, owner, plain, missed: AtomicBool::new(false), kept: [AtomicU64::new(0), AtomicU64::new(0)] }
    }

    /// GP-1b: the walk's bound over the NOT of the key when `desc` (a descending group walk).
    pub const fn descending(mut self, desc: bool) -> HashAggPartial {
        self.flip = if desc { !0 } else { 0 };
        self
    }

    /// The owner functions in force: `plain` once a domain missed.
    fn fns(&self) -> Option<&OwnerFns> {
        if self.missed.load(AtomicOrdering::Acquire) { self.plain.as_ref() } else { self.owner.as_ref() }
    }
}


/// Partition `part` of every phase-1 partial (sub-partition `sub` at `depth`) folded into a table
/// laid out over `tp..tp + bytes` and reduced; a table that fills at its largest capacity splits
/// into eight sub-partitions read again with the finer filter (the second generation, §3.2).
/// `cancel` is checked per run of records (a source block, a residue record, a streamed run): D283.
#[allow(clippy::too_many_arguments)]
fn fold_sub(tp: *mut u8, bytes: usize, layout: &RecordLayout, part: u32, pbits: u8, depth: u8, reread: u8, sub: u64, est: u64, groups: u64, partials: &[Partial<'_>], files: &[&dyn SpillFile], site: *mut gravity_abi::Site, cancel: *const u8, owner: *mut u8, fns: &OwnerFns, reduce: gravity_abi::breaker::ReduceFn, order_slot: u8, flush: Flush<'_>) -> Result<()> {
    // C9: the owner phase's step on the statement's abort byte (`Ctx::cancel`; null: none), which its binding lends the
    // owner's ctxs, not a kernel call (`cancelpoll::step_on`): a set abort bit stops the fold at its next tick with
    // `Error::Cancelled`, which the statement raises as PG's error (D283); a tick is a poll for the poll-gap probe too
    // (D3969: the owner phase's folds read the byte, so the probe saw a gap from one host label to the next).
    // SAFETY: null or the live abort byte the statement's binding lends its ctxs, alive through the owner phase.
    let mut step = unsafe { cancelpoll::step_on(cancel, "merge fold_sub: a partition's runs folded") };
    step.tick()?;
    let split = SubSplit { pbits, depth, reread, sub };
    let cap = ((groups.saturating_mul(2)).next_power_of_two().max(64)).min(HashAgg::cap_of(layout, bytes as u32) as u64) as u32;
    if cap < 2 {
        return Err(Error::Unsupported(UNSUPPORTED_SCRATCH));
    }
    let census = crate::census_on();
    let t0 = census.then(std::time::Instant::now);
    // SAFETY: as `step`'s: the owner phase's abort byte
    let laid = unsafe { cancelpoll::step_on(cancel, "merge fold_sub: a partition's table laid") };
    // SAFETY: the owner's table space, this thread's alone; the header zeroed before the layout.
    let table = unsafe {
        core::ptr::write_bytes(tp, 0, HASHAGG_HDR);
        let t = HashAgg::at(tp, layout, cap, bytes as u32);
        t.set_route_bits(split.table_depth());
        t as *mut HashAgg
    };
    // K10a (D4222): the label covers the laying only; the runs below are `step`'s stretch, not the layout's
    drop(laid);
    let t1 = census.then(std::time::Instant::now);
    let n = core::cell::Cell::new(0u64);
    let full = core::cell::Cell::new(false);
    // the abort byte seen between two runs: the sources stop as at a full table, then the fold returns `Cancelled`
    let stop = core::cell::Cell::new(false);
    // a float merge's overflow (`FOLD_OVERFLOW`, D244): the statement's error, raised once the sources stop
    let ovf = core::cell::Cell::new(false);
    // V4R-375: a hosted combine raised (`FOLD_HOST`, the host's error stashed): the statement's error once the sources stop
    let host = core::cell::Cell::new(false);
    let hosted = layout.slot_kinds().contains(&gravity_abi::breaker::SlotKind::AggState);
    // interval kinds: a merge keeps the later of equal members, or raises as PG's combine in PG's order
    // (`SlotKind::folds_oldest_first`), and an order-key layout's owner fold takes the newer member of a tie
    // (`Rec::fold_newer`), so the partition is read oldest first as a hosted table's is: a W0 group's partials in input
    // order, the member PG's spilled HashAgg keeps, the sum PG's sequential fold reaches
    let oldest_first = hosted
        || layout.slot_kinds().iter().any(|k| k.folds_oldest_first())
        || layout.flags & gravity_abi::breaker::LAYOUT_KEY_TIES != 0;
    // the candidate list behind the table: the records whose count reached the owner's threshold
    // (§3.4: the sort never sees the partition), `ORDER_NONE`: the reduce sees the whole table
    let rec = layout.size() as usize;
    let mut cand = Candidates::none();
    if order_slot != ORDER_NONE {
        let used = HASHAGG_HDR + cap as usize * rec;
        // CR2-01: one index per table slot at most (`MOVING` notes a record once), so the list is as long as the
        // table space left allows, up to the table's slots; past that room the reduce scans the table.
        let room = bytes.saturating_sub(used) / 4;
        // SAFETY: the owner's table space past the table, this thread's alone; the owner header was opened.
        cand = unsafe { Candidates::at(tp.add(used), room.min(cap as usize), layout.slot_off(order_slot as usize) as usize, OwnerTop::view(owner).thr) };
    }
    {
        let t = unsafe { &mut *table };
        // one run of whole records through the kernel's per-shape fold or the generic one, or a file's own records through
        // the fold its writer named (`SpillFile::iov`, at their width); false when the table filled
        let mut run = |ptr: *const u8, recs: usize, own: Option<(IovFn, usize)>| -> bool {
            if step.tick().is_err() {
                stop.set(true);
                return false;
            }
            // SAFETY: `recs` whole records of the layout (or of the file's width) at `ptr`, read-only during phase 2; the
            // table is this thread's.
            let v = unsafe {
                match (own, fns.fold) {
                    (Some((g, width)), _) => g(t as *mut HashAgg, &[(ptr, recs)], width, split),
                    (None, Some(f)) => f(fns.params.as_ptr(), t as *mut HashAgg, ptr, recs, split, &mut cand),
                    (None, None) => t.fold_run(ptr, recs, split, &mut cand),
                }
            };
            if v == FOLD_OVERFLOW {
                ovf.set(true);
                return false;
            }
            if v == FOLD_HOST {
                host.set(true);
                return false;
            }
            n.set(n.get() + if v >= 0 { v } else { -1 - v } as u64);
            v >= 0
        };
        // every owner's epochs of the partition (the flushes), streamed in runs of whole records; a
        // full table stops the stream (the split re-reads). V4R-375: a table holding hosted states reads a file's records
        // through the owner's fold (its states combine; never a file's own fold)
        let files_pass = |run: &mut dyn FnMut(*const u8, usize, Option<(IovFn, usize)>) -> bool| -> Result<()> {
            // bounded("other: one spill file per worker and round, at most the section's 2 * SLOTS + 1 arrays; each run it streams goes through `run`, which ticks the fold's step first")
            for f in files {
                if full.get() {
                    break;
                }
                let own = f.iov().filter(|_| !hosted);
                let r = stream_runs(*f, part, own.map_or(rec, |o| o.1), &mut |ptr: *const u8, recs: usize| {
                    if run(ptr, recs, own) {
                        Ok(())
                    } else {
                        full.set(true);
                        Err(Error::Unsupported(UNSUPPORTED_SCRATCH))
                    }
                });
                match r {
                    Ok(_) => {}
                    Err(Error::Unsupported(_)) if full.get() => break,
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        };
        // V4R-375: a table holding hosted states reads the partition oldest first (the files' epochs, then the chains, then
        // the tables' records: a width-1 group's partials in input order), and so does one holding interval kinds or an
        // order-key layout; every other as before (chains, records, files)
        if oldest_first {
            files_pass(&mut run)?;
        }
        // the in-memory sources: every worker's chain of the partition, block by block, then its
        // table's records of it if any
        // bounded("small-fixed: one partial per worker, at most SLOTS (256)")
        'sources: for p in partials {
            if full.get() {
                break;
            }
            if partial_shape(p.bytes).is_none() || T1::of(p.bytes).is_none() {
                continue;
            }
            let t1 = unsafe { T1::view(p.bytes.as_ptr()) };
            // bounded("other: one call of `run` per chain block of the partition, and `run` ticks the fold's step first")
            for b in t1.chain(part) {
                if !run(b.ptr, b.recs, None) {
                    full.set(true);
                    break 'sources;
                }
            }
            if t1.len != 0 {
                // bounded("other: one call of `run` per record of the partition, and `run` ticks the fold's step first")
                for r in t1.records() {
                    if r.route().part(pbits as u32) != part {
                        continue;
                    }
                    if !run(r.as_ptr(), 1, None) {
                        full.set(true);
                        break 'sources;
                    }
                }
            }
        }
        if !oldest_first {
            files_pass(&mut run)?;
        }
    }
    // K10a (D4222): the runs are folded; the split's re-reads and the reduce below open their own steps, so this label
    // never covers them
    drop(step);
    if stop.get() {
        return Err(Error::Cancelled);
    }
    if ovf.get() {
        return Err(Error::Raised(FloatOverflow::MASK));
    }
    if host.get() {
        return Err(Error::Raised(gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST)));
    }
    if full.get() {
        if groups < est {
            // the estimate's table filled: once more over the partition at the records' size (the split re-reads eight times);
            // `reread`: the dense round noted the records before the fill at the insert — the fold keeps them quiet
            crate::TABLE_RETRY.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            return fold_sub(tp, bytes, layout, part, pbits, depth, 1, sub, est, est, partials, files, site, cancel, owner, fns, reduce, order_slot, flush);
        }
        // D445: a split that did not divide splits again on the next three route bits while the route has them (D446: a
        // chain of splits that do not divide goes on to the route's end, ROUTE_BITS); past them the partition is given back
        // (SCRATCH), which a two-round owner folds again in a grown region (driver own_all, D533)
        if gravity_abi::hash::Depth::new(pbits as u32, depth as u32 + 1).is_none() {
            return Err(Error::Unsupported(UNSUPPORTED_SCRATCH));
        }
        // D533: a child table starts at the records that filled this one (n / 8), retried once at the partition's share
        // (est / 8) if it fills below it, as the depth-0 table is at `est`: sized at n / 8 alone, each depth's table was a
        // quarter of its parent's while its records were an eighth, so a partition far past its table split ~7 deep
        let share = est / 8 + 1;
        for s in 0..8u32 {
            fold_sub(tp, bytes, layout, part, pbits, depth + 1, 0, sub << 3 | s as u64, share.max(n.get() / 8 + 1), n.get() / 8 + 1, partials, files, site, cancel, owner, fns, reduce, order_slot, flush)?;
        }
        return Ok(());
    }
    let t2 = census.then(std::time::Instant::now);
    // SAFETY: the folded table is live; the kernel's reduce reads it and writes the owner's result;
    // full round-2 buffers are flushed to the owner's round-2 file and the reduce resumes (§3.7)
    // SAFETY: as `step`'s: the owner phase's abort byte
    let mut reduced = unsafe { cancelpoll::step_on(cancel, "merge fold_sub: a partition's reduce") };
    let mut v = unsafe { reduce(fns.params.as_ptr(), owner, table, site, cand.ptr, cand.ncand()) };
    while v == REDUCE_FLUSH {
        reduced.tick()?;
        flush()?;
        v = unsafe { reduce(fns.params.as_ptr(), owner, table, site, cand.ptr, cand.ncand()) };
    }
    if v < 0 {
        return Err(reduce_error(v));
    }
    if let (Some(t0), Some(t1), Some(t2)) = (t0, t1, t2) {
        crate::OWN_CENSUS[0].fetch_add((t1 - t0).as_nanos() as u64, core::sync::atomic::Ordering::Relaxed);
        crate::OWN_CENSUS[1].fetch_add((t2 - t1).as_nanos() as u64, core::sync::atomic::Ordering::Relaxed);
        crate::OWN_CENSUS[2].fetch_add(t2.elapsed().as_nanos() as u64, core::sync::atomic::Ordering::Relaxed);
        crate::OWN_CENSUS[3].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        crate::OWN_CENSUS[4].fetch_add(n.get(), core::sync::atomic::Ordering::Relaxed);
    }
    Ok(())
}

/// A partition's group estimate for its table: the partition count was chosen for 512 KiB of groups per
/// partition (`spill_bits`, at most 2x under), so a partition's groups are within `1 MiB / rec` unless the
/// count was clamped at 11 bits; the records are the ceiling (a table that fills is retried at it).
fn table_groups(est: u64, pbits: u8, rec: u64) -> u64 {
    if pbits >= 11 {
        est
    } else {
        est.min((1u64 << 20) / rec.max(1))
    }
}

impl Merge for HashAggPartial {
    /// The leader's fold of a plain record-table partial (`TABLE_AGG`: the kernels not on the
    /// phase-1 table — a one-key count group-by, a grouped average, the PGROUPT text group-by); a phase-1 table's partials are the
    /// owners' and never reach it.
    fn merge_into(&self, leader: &mut State<'_>, partial: Partial<'_>) -> Result<()> {
        let Some((layout, held, _)) = partial_shape(partial.bytes) else { return Ok(()) };
        if T1::of(partial.bytes).is_some() {
            return Err(Error::Unsupported(UNSUPPORTED_SCRATCH));
        }
        let armed = unsafe { HashAgg::view(leader.bytes.as_ptr()) }.limit;
        let p_limit = unsafe { *(partial.bytes.as_ptr().add(8) as *const u32) };
        let limit = if armed != 0 { gravity_abi::breaker::SHARE } else { p_limit };
        assert!(leader.bytes.len() >= p_limit.min(armed.max(1)) as usize, "leader state below the partial's region");
        let cap = ((held * 8) / 7 + 1).next_power_of_two().clamp(256, u32::MAX as u64) as u32;
        let l = unsafe { HashAgg::at(leader.bytes.as_mut_ptr(), &layout, cap, limit) };
        each_record(partial.bytes, &mut |r: Rec<'_>| {
            let Some(dst) = (if layout.nkeys() == 0 { Some(l.single()) } else { l.find_or_insert(r.keys()) }) else {
                return Err(Error::Unsupported(UNSUPPORTED_SCRATCH));
            };
            dst.fold(&r).map_err(|_| Error::Raised(FloatOverflow::MASK))
        })
    }

    /// Room when the leader's largest capacity holds both at 7/8 load (no key overlap assumed).
    fn room_for(&self, leader: &[u8], partial: Partial<'_>) -> bool {
        if leader.len() < HASHAGG_HDR {
            return true;
        }
        let Some((layout, held, _)) = partial_shape(partial.bytes) else { return true };
        let l = unsafe { HashAgg::view(leader.as_ptr()) };
        let p_limit = unsafe { *(partial.bytes.as_ptr().add(8) as *const u32) };
        let limit = if l.limit != 0 { l.limit } else { p_limit };
        let max_cap = HashAgg::cap_of(&layout, limit) as u64;
        (l.len as u64 + held) <= max_cap - max_cap / 8
    }

    /// The owner phase runs when the shape brought its reduction, the owner regions hold what it
    /// needs and every partial is a phase-1 table (its buffers' partition count). A one-round unordered
    /// shape's floor names its whole result (the runs form: every group record); the pool's owners split it
    /// by partition, so it runs when the owners' run halves together hold the floor (`owner_open` halves a
    /// region; an owner that still overflows is redone by `own_single`) — the count-ordered and two-round forms' floors are per
    /// owner, a two-round form's for one of these owners (its second round splits the groups among them, KEYED-28).
    fn owned_partitions(&self, partials: &[Partial<'_>], owner_bytes: u32) -> Option<u8> {
        let fns = self.fns()?;
        let owners = partials.len().max(1) as u32;
        let pbits = self.partition_bits(partials)?;
        if (owner_bytes as u64) >= (fns.min_bytes)(fns.params.as_ptr(), owners) {
            return Some(pbits);
        }
        let floor = (fns.min_bytes)(fns.params.as_ptr(), 1);
        (fns.rounds == 1 && fns.order_slot == ORDER_NONE && owner_bytes as u64 * owners as u64 >= 2 * floor).then_some(pbits)
    }

    fn owner_floor(&self) -> Option<u64> {
        let fns = self.fns()?;
        Some((fns.min_bytes)(fns.params.as_ptr(), 1))
    }

    /// The phase-1 tables' partition count (every partial that holds records is one). D273: when no partial holds a table
    /// of records (the producer saw no row: a region never laid out, or a T1 of no capacity), still `Some`, the empty T1's
    /// own bits or 1: the owner phase runs over nothing and leaves an Owned leader of zero records, which every reader
    /// (the emit's gather, keyed.twopass, the distinct total) reads as it reads any other, never an unowned region.
    fn partition_bits(&self, partials: &[Partial<'_>]) -> Option<u8> {
        self.fns()?;
        let (mut pbits, mut held) = (0u8, false);
        // D273: the answer when no partial holds records, `None` once one partial is neither empty region nor empty T1
        let mut empty = Some(1u8);
        // bounded("small-fixed: one partial per worker, at most SLOTS (256)")
        for p in partials {
            match partial_shape(p.bytes) {
                Some((_, _, b)) if T1::of(p.bytes).is_some() => (pbits, held) = (pbits.max(b), true),
                Some(_) => return None,
                None => empty = empty.zip(empty_t1(p.bytes)).map(|(a, b)| a.max(b)),
            }
        }
        if held { (pbits != 0).then_some(pbits) } else { empty }
    }

    fn open_owner(&self, owner: Region, _ordinal: u16) -> Result<()> {
        let fns = self.fns().ok_or_else(gravity_driver::no_owner_phase)?;
        // SAFETY: the owner's region, this thread's alone (`Region`).
        let v = unsafe { (fns.open)(fns.params.as_ptr(), owner.ptr, owner.len) };
        if v < 0 {
            return Err(Error::Unsupported(UNSUPPORTED_SCRATCH));
        }
        Ok(())
    }

    /// D3075: the form's answer kept once probed (its params never change for the execution); a probe across a D597 switch
    /// to `plain` keeps nothing.
    fn kept_run_memo(&self, probe: &mut dyn FnMut() -> Option<u64>) -> Option<u64> {
        let form = self.missed.load(AtomicOrdering::Acquire) as usize;
        match self.kept[form].load(AtomicOrdering::Relaxed) {
            0 => {
                let r = probe();
                if self.missed.load(AtomicOrdering::Acquire) as usize == form {
                    self.kept[form].store(r.map_or(1, |o| o + 2), AtomicOrdering::Relaxed);
                }
                r
            }
            1 => None,
            v => Some(v - 2),
        }
    }

    /// D2988 (C): the partitions the partials' chains and tables hold a record of (`own_partition`'s sources), from memory
    /// alone: none with a file to read, or a partial that is not a phase-1 table.
    fn held_parts(&self, partials: &[Partial<'_>], files: &[&dyn SpillFile], pbits: u8, mask: &mut [u64]) -> bool {
        if !files.is_empty() || (1usize << pbits) > 64 * mask.len() {
            return false;
        }
        // C9: under the owner phase's lent byte (driver `own_all`); a cancel leaves the mask unknown (`false`), and the
        // fold's first partition then stops with it
        let mut step = cancelpoll::step("merge held_parts: the partials' records routed");
        for p in partials {
            if partial_shape(p.bytes).is_none() {
                continue;
            }
            if T1::of(p.bytes).is_none() {
                return false;
            }
            // SAFETY: a laid phase-1 table (`T1::of`), read-only during the owner phase.
            let t1 = unsafe { T1::view(p.bytes.as_ptr()) };
            if t1.pbits != pbits {
                return false;
            }
            // D3075: the chains are walked only when they buffer a record (`T1::buffered` counts every appended one): a
            // rescanned fold of a row or two walked every partition's empty chain per owner phase
            if t1.buffered != 0 {
                t1.held_chains(mask);
            }
            if t1.len != 0 {
                for (i, r) in t1.records().enumerate() {
                    if step.batch(i).is_err() {
                        return false;
                    }
                    let part = r.route().part(pbits as u32);
                    mask[part as usize / 64] |= 1 << (part % 64);
                }
            }
        }
        true
    }

    /// D3111: every partition's record bytes over `own_partition`'s sources, each partial's chain blocks and its table's
    /// records (walked, as `held_parts` does) at the partial's record size, and each file's partition bytes (a file that
    /// cannot say: its even share, as `own_partition` estimates it); `false` for a partial that is not a phase-1 table at
    /// `pbits`.
    fn part_record_bytes(&self, partials: &[Partial<'_>], files: &[&dyn SpillFile], pbits: u8, parts: &mut [u64]) -> bool {
        let n = 1usize << pbits;
        if n > parts.len() {
            return false;
        }
        let parts = &mut parts[..n];
        parts.fill(0);
        // C9: under the owner phase's lent byte (driver `own_all`); a cancel answers `false` (no count), and the fold's first
        // partition then stops with it
        let mut step = cancelpoll::step("merge part_record_bytes: the partials' records counted");
        for p in partials {
            let Some((layout, _, _)) = partial_shape(p.bytes) else { continue };
            if T1::of(p.bytes).is_none() {
                return false;
            }
            // SAFETY: a laid phase-1 table (`T1::of`), read-only during the owner phase.
            let t1 = unsafe { T1::view(p.bytes.as_ptr()) };
            if t1.pbits != pbits {
                return false;
            }
            let rec = layout.size() as u64;
            if t1.buffered != 0 {
                for (part, b) in parts.iter_mut().enumerate() {
                    if step.tick().is_err() {
                        return false;
                    }
                    *b += rec * t1.chain(part as u32).map(|c| c.recs as u64).sum::<u64>();
                }
            }
            if t1.len != 0 {
                for (i, r) in t1.records().enumerate() {
                    if step.batch(i).is_err() {
                        return false;
                    }
                    parts[r.route().part(pbits as u32) as usize] += rec;
                }
            }
        }
        for f in files {
            for (part, b) in parts.iter_mut().enumerate() {
                if step.tick().is_err() {
                    return false;
                }
                *b += match f.part_bytes(part as u32) {
                    0 => f.spilled_bytes() >> pbits,
                    b => b,
                };
            }
        }
        true
    }

    /// The generic fold: partition `part` of every phase-1 partial into the owner's table
    /// behind its result, sized from the partition's record count, then the kernel's reduce.
    fn owned_rounds(&self) -> u8 {
        self.fns().map_or(1, |f| f.rounds)
    }

    fn end_round(&self, owner: Region, flush: Flush<'_>) -> Result<()> {
        let fns = self.fns().ok_or_else(gravity_driver::no_owner_phase)?;
        // C9: under the byte its caller lends (driver section.rs: the owner phase's `lend_cancel`); a tick before each call,
        // as each REDUCE_FLUSH wrote the round-2 buffers out
        let mut step = cancelpoll::step("merge end_round: an owner's round closed");
        loop {
            step.tick()?;
            // SAFETY: the owner's region, opened.
            match unsafe { (fns.end_round)(fns.params.as_ptr(), owner.ptr) } {
                REDUCE_FLUSH => flush()?,
                v if v < 0 => return Err(reduce_error(v)),
                _ => return Ok(()),
            }
        }
    }

    fn round2_pbits(&self, owner: Region) -> u8 {
        self.round2_partial(owner).map_or(0, |r| unsafe { T1::view(r.ptr) }.pbits)
    }

    fn round2_partial(&self, owner: Region) -> Option<Region> {
        // SAFETY: the owner's region, opened: its header says where the round-2 buffers start.
        let off = unsafe { OwnerTop::view(owner.ptr) }.round2_off as usize;
        (off != 0 && off < owner.len).then(|| Region { ptr: unsafe { owner.ptr.add(off) }, len: owner.len - off })
    }

    fn own_partition(&self, owner: Region, part: u32, pbits: u8, partials: &[Partial<'_>], files: &[&dyn SpillFile], site: *mut gravity_abi::Site, cancel: *const u8, round: u8, flush: Flush<'_>) -> Result<()> {
        // SAFETY: as `fold_sub`'s step: the owner phase's abort byte
        unsafe { cancelpoll::step_on(cancel, "merge own_partition: a partition owned") }.tick()?;
        let fns = self.fns().ok_or_else(gravity_driver::no_owner_phase)?;
        let Some((layout, _, _)) = partials.iter().find_map(|p| partial_shape(p.bytes)) else { return Ok(()) };
        // SAFETY: the owner's region was opened (`open_owner`): its result header says where the table space starts.
        let off = unsafe { OwnerTop::view(owner.ptr) }.table_off as usize;
        if off >= owner.len {
            return Err(Error::Unsupported(UNSUPPORTED_SCRATCH));
        }
        // D4033: every phase-1 partial of one owner phase holds the owner phase's partitions (kernels `fold_bits` cuts a
        // worker share the pool's room opened smaller by its section's share). One at another count is refused by name and
        // never read: its words past its own partitions are no chains, and a chain below them holds other partitions' records
        if let Some(t) = partials.iter().filter(|p| partial_shape(p.bytes).is_some()).filter_map(|p| T1::of(p.bytes)).find(|t| t.pbits != pbits) {
            return Err(Error::host(format!("gravity: a phase-1 partial at {} partitions where its owner phase reads {}", 1u32 << t.pbits, 1u32 << pbits))); // alloc: Cold
        }
        let mut est = 0u64;
        // bounded("small-fixed: one partial per worker, at most SLOTS (256); a partial's chain of the partition summed by blocks")
        for p in partials {
            if partial_shape(p.bytes).is_none() || T1::of(p.bytes).is_none() {
                continue;
            }
            let t1 = unsafe { T1::view(p.bytes.as_ptr()) };
            est += t1.chain(part).map(|b| b.recs as u64).sum::<u64>() + if t1.len != 0 { t1.len as u64 / t1.parts() as u64 + 1 } else { 0 };
        }
        // a file that cannot say a partition's bytes is estimated at its even share (never skipped)
        let mut in_files = false;
        // bounded("small-fixed: one spill file per worker and round, at most the section's 2 * SLOTS + 1 arrays")
        for f in files {
            let b = match f.part_bytes(part) {
                0 => f.spilled_bytes() >> pbits,
                b => b,
            };
            in_files |= f.spilled_bytes() > 0;
            est += (b / layout.size() as u64).max((f.spilled_bytes() > 0) as u64);
        }
        if est == 0 && !in_files {
            return Ok(());
        }
        let (reduce, order_slot) = if round == 2 { (fns.reduce2, fns.order_slot2) } else { (fns.reduce, fns.order_slot) };
        let groups = match crate::owner_groups_cap() {
            0 => table_groups(est, pbits, layout.size() as u64),
            cap => table_groups(est, pbits, layout.size() as u64).min(cap),
        };
        match fold_sub(unsafe { owner.ptr.add(off) }, owner.len - off, &layout, part, pbits, 0, 0, 0, est, groups, partials, files, site, cancel, owner.ptr, fns, reduce, order_slot, flush) {
            // D597: the round-1 rerun (the scratch road) folds again through the plain form; without one, the error by name
            Err(Error::Unsupported(UNSUPPORTED_DOMAIN)) if self.plain.is_some() => {
                self.missed.store(true, AtomicOrdering::Release);
                Err(Error::Unsupported(UNSUPPORTED_SCRATCH))
            }
            Err(Error::Unsupported(UNSUPPORTED_DOMAIN)) => Err(Error::host("gravity: a group key outside the owner's dense domain")),
            r => r,
        }
    }

    fn finish_owners(&self, leader: &mut [u8], width: u16, owner_bytes: u32) -> Result<()> {
        if leader.len() < OWNED_HDR {
            return Err(Error::Unsupported(UNSUPPORTED_SCRATCH));
        }
        // SAFETY: the leader region's first 64 bytes, the owners' regions lie behind them.
        unsafe { *(leader.as_mut_ptr() as *mut Owned) = Owned { nowners: width as u32, owner_bytes, limit: leader.len() as u32, pad0: [0; 3], kind: TABLE_OWNED, base: 0, pad1: [0; 5], cell: core::cell::UnsafeCell::new([]) } };
        Ok(())
    }

    /// The `limit`-th smallest key of the worker's table, once it holds that many keys
    /// (a bounded insertion over the used records; `limit` is small).
    fn bound(&self, partial: Partial<'_>) -> Option<i64> {
        if self.limit == 0 {
            return None;
        }
        let (layout, _, _) = partial_shape(partial.bytes)?;
        if layout.nkeys() == 0 {
            return None;
        }
        // over the records held: the table's and the buffers' (the top-k kernel drains its survivors at the claim's end)
        // past the stack heap's `SELECT_MAX` the scratch is sized from the records held: any LIMIT gets its bound
        // fewer than `n` records hold fewer than `n` keys: no bound (the abi's own answer), with no scratch
        let n = self.limit as usize;
        T1::of(partial.bytes).and_then(|t| {
            if n <= SELECT_MAX {
                t.nth_smallest_key_held_by(0, n, self.flip)
            } else if t.held() < n as u64 {
                None
            } else {
                // memsig (UNOWNED runtime, parallel): the held keys' scratch (8 B per held record) on the statement the claim
                // serves: a worker's claim runs entered on its leader's account (pool Work::body's carry, hosts worker_arm
                // G1-B), the leader's bound on its own; no room is no bound, as a short scratch is
                let owner = gravity_driver::stmt_context().map(|(o, _)| o);
                let mut scratch = memledger::ChargedVec::new(owner, memledger::Kind::Region);
                scratch.resize(t.held() as usize, 0i64).ok()?;
                t.nth_smallest_key_held_in_by(0, n, &mut scratch, self.flip)
            }
        })
    }

    /// C10 2b (D3582): the runs form's fold over spill files alone (`HashAggBatch`): one round, no candidate list, no walk
    /// bound, every partial that holds records a phase-1 table (the layout the files' records are in).
    fn batch_fold(&self, partials: &[Partial<'_>]) -> Option<Box<dyn BatchFold>> {
        let fns = self.fns()?;
        if fns.rounds != 1 || fns.order_slot != ORDER_NONE || self.limit != 0 {
            return None;
        }
        let mut layout = None;
        // bounded("small-fixed: one partial per worker, at most SLOTS")
        for p in partials {
            if let Some((l, _, _)) = partial_shape(p.bytes) {
                T1::of(p.bytes)?;
                layout = layout.or(Some(l));
            }
        }
        let plain = if self.missed.load(AtomicOrdering::Acquire) { None } else { self.plain };
        Some(Box::new(HashAggBatch { owner: *fns, plain, missed: AtomicBool::new(false), layout: layout? })) // alloc: Exec
    }
}

/// C10 2b (D3582): `HashAggPartial`'s one-round owner fold kept for a batched slot's refills (driver mem/batch.rs): the
/// owner functions in force when the slot was laid, its plain form behind a dense one (D597), the phase-1 records' layout.
/// Each partition folds as `own_partition` folds it, from the slot's files alone (its producer scattered every partial).
struct HashAggBatch {
    owner: OwnerFns,
    plain: Option<OwnerFns>,
    /// D597: a batch met a key outside the dense domain: `plain` folds from then on.
    missed: AtomicBool,
    layout: RecordLayout,
}

impl HashAggBatch {
    fn fns(&self) -> &OwnerFns {
        match (&self.plain, self.missed.load(AtomicOrdering::Acquire)) {
            (Some(p), true) => p,
            _ => &self.owner,
        }
    }
}

impl BatchFold for HashAggBatch {
    fn rec(&self) -> usize {
        self.layout.size() as usize
    }

    fn floor(&self) -> u64 {
        let fns = self.fns();
        (fns.min_bytes)(fns.params.as_ptr(), 1)
    }

    fn open(&self, owner: Region) -> Result<()> {
        let fns = self.fns();
        // SAFETY: the batched slot's owner region, the leader's alone (`Region`).
        match unsafe { (fns.open)(fns.params.as_ptr(), owner.ptr, owner.len) } {
            v if v < 0 => Err(Error::Unsupported(UNSUPPORTED_SCRATCH)),
            _ => Ok(()),
        }
    }

    fn fold(&self, owner: Region, part: u32, pbits: u8, files: &[&dyn SpillFile], site: *mut gravity_abi::Site, cancel: *const u8) -> Result<()> {
        let fns = self.fns();
        let rec = self.layout.size() as u64;
        // SAFETY: the owner region was opened (`open`): its result header says where the table space starts.
        let off = unsafe { OwnerTop::view(owner.ptr) }.table_off as usize;
        if off >= owner.len {
            return Err(Error::Unsupported(UNSUPPORTED_SCRATCH));
        }
        // a file that cannot say a partition's bytes is estimated at its even share (never skipped), as `own_partition`'s
        let (mut est, mut in_files) = (0u64, false);
        // bounded("small-fixed: one file per owner of the slot's stream, at most 2 * SLOTS + 1")
        for f in files {
            let b = match f.part_bytes(part) {
                0 => f.spilled_bytes() >> pbits,
                b => b,
            };
            in_files |= f.spilled_bytes() > 0;
            est += (b / rec).max((f.spilled_bytes() > 0) as u64);
        }
        if est == 0 && !in_files {
            return Ok(());
        }
        let groups = match crate::owner_groups_cap() {
            0 => table_groups(est, pbits, rec),
            cap => table_groups(est, pbits, rec).min(cap),
        };
        // SAFETY: the table space behind the opened owner's header, inside its region.
        let tp = unsafe { owner.ptr.add(off) };
        match fold_sub(tp, owner.len - off, &self.layout, part, pbits, 0, 0, 0, est, groups, &[], files, site, cancel, owner.ptr, fns, fns.reduce, fns.order_slot, &mut || Err(Error::Unsupported(UNSUPPORTED_SCRATCH))) {
            // D597: the batch folds again through the plain form (the batches before it keep the dense one's runs)
            Err(Error::Unsupported(UNSUPPORTED_DOMAIN)) if self.plain.is_some() && !self.missed.swap(true, AtomicOrdering::AcqRel) => Err(Error::Unsupported(UNSUPPORTED_DOMAIN)),
            Err(Error::Unsupported(UNSUPPORTED_DOMAIN)) => Err(Error::host("gravity: a group key outside the owner's dense domain")),
            r => r,
        }
    }

    fn end(&self, owner: Region) -> Result<()> {
        let fns = self.fns();
        // SAFETY: the owner's region, opened.
        match unsafe { (fns.end_round)(fns.params.as_ptr(), owner.ptr) } {
            REDUCE_FLUSH => Err(Error::Unsupported(UNSUPPORTED_SCRATCH)),
            v if v < 0 => match reduce_error(v) {
                Error::Unsupported(UNSUPPORTED_DOMAIN) if self.plain.is_some() && !self.missed.swap(true, AtomicOrdering::AcqRel) => Err(Error::Unsupported(UNSUPPORTED_DOMAIN)),
                Error::Unsupported(UNSUPPORTED_DOMAIN) => Err(Error::host("gravity: a group key outside the owner's dense domain")),
                e => Err(e),
            },
            _ => Ok(()),
        }
    }
}

/// The zone walk's rows: each worker's bounded run inserted into the leader's.
pub struct TopKPartial {
    pub n: u32,
    pub natts: u32,
}

impl Merge for TopKPartial {
    fn merge_into(&self, leader: &mut State<'_>, partial: Partial<'_>) -> Result<()> {
        // D3024 (b): a top that keeps its records in its emit's array (lowerv4 topn `TopEnd`, DC-04: a sorted wide emit's
        // claims file their runs in the emit's run table) has a slot of no bytes: its partials hold no run to fold
        if partial.bytes.len() < core::mem::size_of::<TopK>() {
            return Ok(());
        }
        let p = unsafe { TopK::view(partial.bytes.as_ptr()) };
        if p.n == 0 {
            return Ok(());
        }
        // the run's columns are the partial's own (a code run carries more than the output)
        assert!(p.n == self.n, "top-k partial shape");
        let l = unsafe { TopK::at_cap(leader.bytes.as_mut_ptr(), p.n, p.cap, p.natts, p.flags) };
        // C9: under the byte the leader's fold lends (driver section.rs `close_section`)
        let mut step = cancelpoll::step("merge merge_into: a top-k partial's rows");
        for i in 0..p.len as usize {
            step.batch(i)?;
            let (v, nl) = p.row(i);
            if l.insert_ties(v, nl).is_none() {
                return Err(Error::Unsupported(UNSUPPORTED_SCRATCH));
            }
        }
        Ok(())
    }

    fn bound(&self, partial: Partial<'_>) -> Option<i64> {
        if partial.bytes.len() < core::mem::size_of::<TopK>() {
            return None;
        }
        let p = unsafe { TopK::view(partial.bytes.as_ptr()) };
        if p.n == 0 || (partial.bytes.len() as u32) < TopK::bytes_cap(p.cap, p.natts) || p.flags & TOPK_PAIRS != 0 {
            return None;
        }
        p.bound()
    }
}

/// The leader's finish of a top-k run keyed or ordered by dictionary codes
/// (a top-k returning a dict text column): every column in `codes` holds `part << 32 | code` words the
/// workers copied from a code lane; `resolve(part, lane, code)` renders one to
/// its text datum and payload length (the source's `dict_value`, the leader's
/// resident parts). Then, with a `secondary` column, the rows are ordered by
/// `(key, the secondary's bytes)` — codes order by bytes inside one part only,
/// which is why the workers kept ties (`TOPK_TIES`) — and cut to `n`. A code word is rendered
/// once per distinct `(column, part, code)` (a ties run repeats a few hundred words over
/// thousands of rows), and with a secondary key only that key's column before the cut: the
/// other code columns are rendered for the `n` survivors.
pub fn order_topk(stmt: &gravity_driver::Reservation, run: &mut TopK, codes: u32, secondary: Option<u16>, mut resolve: impl FnMut(u32, u16, u32) -> Result<(u64, u32)>) -> Result<()> {
    let natts = run.natts as usize;
    let census = crate::phases_on().then(std::time::Instant::now);
    // memsig (UNOWNED runtime, parallel): the render's lists on the statement this leader-only call serves (exec runner
    // plan.rs, the leader's drive: `stmt_context` is its statement or portal); uncharged outside one
    let owner = gravity_driver::stmt_context().map(|(o, _)| o);
    let oom = |bytes: usize| gravity_driver::Error::OutOfMemory { bytes: bytes as u64, held: gravity_driver::POOL.held(), capacity: gravity_driver::POOL.capacity(), why: None };
    // the rendered codes' table, its buckets on `owner` (`map_charge`, fitted to the table after each render pass; declared
    // after the table, so it drops first)
    let mut rendered: HashMap<(u16, u64), u64> = HashMap::new(); // alloc: Exec charged owner (map_charge, fitted after each render pass)
    let mut map_charge = owner.map(|o| memledger::Charge::open(o, memledger::charge::release_of(o), memledger::Kind::Region, memledger::charge::Door::Heap, 0));
    // C9: on the byte the running kernel call lends (outside one, a step that never stops); a tick per row rendered (a
    // dictionary value's render may read its part), a batch per row moved
    let mut step = cancelpoll::step("merge order_topk: a top-k run's codes rendered and cut");
    let mut render = |step: &mut cancelpoll::Step, run: &mut TopK, columns: u32| -> Result<()> {
        for i in 0..run.len as usize {
            step.tick()?;
            let (v, nl) = run.row_mut(i);
            // bounded("key-columns: one cell per output column of a row, at most MaxTupleAttributeNumber (1664) columns")
            for (c, cell) in v.iter_mut().enumerate().take(natts) {
                if columns >> c & 1 == 0 || nl[c] != 0 {
                    continue;
                }
                *cell = match rendered.entry((c as u16, *cell)) { // alloc: Exec charged owner (map_charge, fitted after the pass)
                    std::collections::hash_map::Entry::Occupied(e) => *e.get(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        let (part, code) = ((*cell >> 32) as u32, *cell as u32);
                        *e.insert(resolve(part, c as u16, code)?.0) // alloc: Exec charged owner (map_charge, fitted after the pass)
                    }
                }
            }
        }
        fit_map(&mut map_charge, &rendered);
        Ok(())
    };
    let first = secondary.map_or(codes, |sec| codes & (1 << sec | 1));
    if secondary.is_some() && run.len > run.n && run.n > 0 {
        // a row past the n-th key cannot survive the (key, bytes) cut: drop it before the render
        // so only the rows that can win are rendered (a ties run holds the walk's stragglers too,
        // and which ones it holds differs run to run)
        let (len, n) = (run.len as usize, run.n as usize);
        let mut keys = memledger::ChargedVec::try_from_iter(owner, memledger::Kind::Region, (0..len).map(|i| run.row(i).0[0] as i64)).map_err(|_| oom(len * 8))?;
        let (_, kth, _) = keys.select_nth_unstable(n - 1);
        let kth = *kth;
        drop(keys);
        // one row's copy, reused for every row moved down (was a copy per row)
        let (mut rv, mut rn) = (memledger::ChargedVec::new(owner, memledger::Kind::Region), memledger::ChargedVec::new(owner, memledger::Kind::Region));
        rv.resize(natts, 0u64).map_err(|_| oom(natts * 9))?;
        rn.resize(natts, 0u8).map_err(|_| oom(natts * 9))?;
        let mut kept = 0usize;
        for i in 0..len {
            step.batch(i)?;
            if (run.row(i).0[0] as i64) > kth {
                continue;
            }
            if kept != i {
                let (v, nl) = run.row(i);
                rv.copy_from_slice(v);
                rn.copy_from_slice(nl);
                let (dv, dn) = run.row_mut(kept);
                dv.copy_from_slice(&rv);
                dn.copy_from_slice(&rn);
            }
            kept += 1;
        }
        run.truncate(kept as u32);
    }
    render(&mut step, run, first)?;
    let (t_resolve, len0) = (census.map(|t| t.elapsed()), run.len);
    if let Some(sec) = secondary {
        let sec = sec as usize;
        assert!(sec < natts && codes >> sec & 1 != 0, "the secondary key is a rendered code column");
        // SAFETY: a rendered cell is a 4B-U varlena image resident with its part for the Node's life.
        let bytes = |v: &[u64], nl: &[u8]| -> Option<&'static [u8]> {
            if nl[sec] != 0 {
                return None;
            }
            let p = v[sec] as *const u8;
            let hdr = unsafe { u32::from_le_bytes([*p, *p.add(1), *p.add(2), *p.add(3)]) };
            let len = (hdr >> 2) as usize - 4;
            Some(unsafe { core::slice::from_raw_parts(p.add(4), len) })
        };
        let less = |a: (&[u64], &[u8]), b: (&[u64], &[u8])| -> bool {
            let (ka, kb) = (a.0[0] as i64, b.0[0] as i64);
            if ka != kb {
                return ka < kb;
            }
            // NULLs last, as ORDER BY … ASC does
            match (bytes(a.0, a.1), bytes(b.0, b.1)) {
                (Some(x), Some(y)) => x < y,
                (Some(_), None) => true,
                _ => false,
            }
        };
        let (len, n) = (run.len as usize, run.n as usize);
        if len > 2 * n {
            // the n winners selected in O(len) (a top-10 by a dict text column: ~5,100 ties rows for 10; the run's own sort is an insertion sort)
            let cmp = |&a: &u32, &b: &u32| {
                let (ra, rb) = (run.row(a as usize), run.row(b as usize));
                if less(ra, rb) {
                    core::cmp::Ordering::Less
                } else if less(rb, ra) {
                    core::cmp::Ordering::Greater
                } else {
                    a.cmp(&b)
                }
            };
            let mut idx = memledger::ChargedVec::try_from_iter(owner, memledger::Kind::Region, 0..len as u32).map_err(|_| oom(len * 4))?;
            idx.select_nth_unstable_by(n - 1, cmp);
            idx.with_vec(|v| v.truncate(n));
            // the polled sort (cancelpoll::sort_by): its merge passes need a scratch past one run
            let mut scratch = memledger::ChargedVec::new(owner, memledger::Kind::Region);
            if n > cancelpoll::SORT_RUN {
                scratch.resize(n, 0u32).map_err(|_| oom(n * 4))?;
            }
            cancelpoll::sort_by(&mut step, &mut idx, &mut scratch, cmp)?;
            drop(scratch);
            // the n winners' copy: on `owner`, within the statement's hold of their bytes (n x natts x 9)
            let (mut tv, mut tn) = (memledger::ChargedVec::new(owner, memledger::Kind::Region), memledger::ChargedVec::new(owner, memledger::Kind::Region));
            stmt.region_draw()?;
            let held = stmt.hold((n * natts * 9) as u64);
            if held.is_none() || tv.try_reserve(n * natts).is_err() || tn.try_reserve(n * natts).is_err() {
                return Err(oom(n * natts * 9));
            }
            for (k, &i) in idx.iter().enumerate() {
                step.batch(k)?;
                let (v, nl) = run.row(i as usize);
                tv.extend_from_slice(v).map_err(|_| oom(n * natts * 9))?;
                tn.extend_from_slice(nl).map_err(|_| oom(n * natts * 9))?;
            }
            for (k, _) in idx.iter().enumerate() {
                step.batch(k)?;
                let (v, nl) = run.row_mut(k);
                v.copy_from_slice(&tv[k * natts..(k + 1) * natts]);
                nl.copy_from_slice(&tn[k * natts..(k + 1) * natts]);
            }
            run.truncate(n as u32);
        } else {
            // bounded("other: TopK's in-place insertion sort over len <= 2 * n rows (this arm's test), n the top-k form's LIMIT; gravity_abi's sort takes no step")
            run.sort_by(less);
        }
    }
    run.truncate(run.n);
    render(&mut step, run, codes & !first)?;
    if let (Some(t), Some(r)) = (census, t_resolve) {
        eprintln!("PHASES topk rows={} n={} codes={:#b} rendered={} resolve={:.2}ms order={:.2}ms", len0, run.n, codes, rendered.len(), r.as_secs_f64() * 1e3, (t.elapsed() - r).as_secs_f64() * 1e3);
    }
    Ok(())
}

/// memsig: `order_topk`'s rendered-codes table's bytes on its charge: hashbrown's one block, `buckets` slots of the entry and
/// one control byte each plus a group of trailing control bytes (buckets = the power of two past capacity x 8 / 7; none
/// before the first insert).
fn fit_map<K, V>(charge: &mut Option<memledger::Charge>, map: &HashMap<K, V>) {
    let Some(c) = charge.as_mut() else { return };
    let cap = map.capacity();
    let bytes = if cap == 0 {
        0
    } else {
        let buckets = if cap < 8 { (cap + 1).next_power_of_two() } else { (cap * 8 / 7).next_power_of_two() };
        (buckets * (core::mem::size_of::<(K, V)>() + 1) + 16) as u64
    };
    c.fit_committed(bytes);
}

/// The build side's merge (§2 decision 5): below the gate the leader hashes every partial's
/// entries into a `BUILD_TABLE` over its share; above it the workers build disjoint slices of one
/// partitioned table (`merge_partition`); a leader table without room for a partial is scattered
/// first by the driver (`room_for`), the seal joining it to the workers' epochs.
pub struct HashBuildPartial;

impl Merge for HashBuildPartial {
    fn merge_into(&self, leader: &mut State<'_>, partial: Partial<'_>) -> Result<()> {
        let p = unsafe { HashBuild::view(partial.bytes.as_ptr()) };
        if partial.bytes.len() < HASHBUILD_HDR || p.cap == 0 {
            return Ok(());
        }
        // the table opens at the first partial's size and doubles ahead of each partial past `table_cap_for`
        // of the merged rows, up to the share (a table over the whole share would zero it all)
        let want = HashBuild::table_cap_for(p.len as u64);
        // a partial with a row store: the leader's carved alike before its table is laid out, the slices appended, every offset rebased
        let mut rebase = [0u32; MAX_BUILD_PARTS];
        let stored = match unsafe { RowStore::of(partial.bytes.as_ptr()) } {
            Some(ps) => {
                let ls = unsafe { RowStore::carve(leader.bytes.as_mut_ptr(), ps.row_hdr as u32, ps.parts) }.ok_or(Error::Unsupported(UNSUPPORTED_SCRATCH))?;
                if !ls.merge_from(ps, &mut rebase) {
                    return Err(Error::Unsupported(UNSUPPORTED_SCRATCH));
                }
                Some(ls.bits())
            }
            None => None,
        };
        let l = unsafe { HashBuild::at_share_sized(leader.bytes.as_mut_ptr(), BUILD_TABLE, want) };
        // bounded("other: the table doubles until it holds the merged rows, at most 32 doublings of a u32 capacity")
        while (l.cap as u64) < HashBuild::table_cap_for(l.len as u64 + p.len as u64) as u64 && l.grow() {}
        // C9: under the byte the leader's fold lends (driver section.rs `close_section`)
        let mut step = cancelpoll::step("merge merge_into: a build partial's entries");
        for (k, e) in p.entries().enumerate() {
            step.batch(k)?;
            let payload = match stored {
                Some(bits) => (e.payload as u32).wrapping_add(rebase[HashBuild::partition_of(e.key, bits) as usize]) as i64,
                None => e.payload,
            };
            if !l.insert_tagged(e.key, payload, e.used) {
                return Err(Error::Unsupported(UNSUPPORTED_SCRATCH));
            }
        }
        Ok(())
    }

    /// The width-1 arm built into the leader region as an append partition: hash it in place for the probe.
    fn finish(&self, leader: &mut State<'_>) -> Result<()> {
        if leader.bytes.len() < HASHBUILD_HDR {
            return Ok(());
        }
        let l = unsafe { HashBuild::view(leader.bytes.as_ptr()) };
        if l.cap == 0 || l.table == BUILD_TABLE {
            return Ok(());
        }
        let l = unsafe { &mut *(leader.bytes.as_mut_ptr() as *mut HashBuild) };
        if l.hash_in_place() {
            Ok(())
        } else {
            Err(Error::Unsupported(UNSUPPORTED_SCRATCH))
        }
    }

    /// Room when the leader's table holds both at 7/8 load.
    fn room_for(&self, leader: &[u8], partial: Partial<'_>) -> bool {
        if partial.bytes.len() < HASHBUILD_HDR || leader.len() < HASHBUILD_HDR {
            return true;
        }
        let p = unsafe { HashBuild::view(partial.bytes.as_ptr()) };
        if p.cap == 0 {
            return true;
        }
        let l = unsafe { HashBuild::view(leader.as_ptr()) };
        if let (Some(ps), Some(ls)) = (unsafe { RowStore::of(partial.bytes.as_ptr()) }, unsafe { RowStore::of(leader.as_ptr()) }) {
            if !ls.room_for(ps) {
                return false;
            }
        }
        let entries = match (unsafe { RowStore::of(leader.as_ptr()) }, unsafe { RowStore::of(partial.bytes.as_ptr()) }) {
            (Some(_), _) => l.pad2,
            (None, Some(ps)) => leader.len() as u32 - RowStore::split(leader.len() as u32, ps.row_hdr as u32),
            (None, None) => leader.len() as u32,
        };
        let (len, cap) = if l.cap == 0 { (0u64, HashBuild::table_cap_of(entries) as u64) } else { (l.len as u64, l.max_cap() as u64) };
        len + p.len as u64 <= cap - cap / 8
    }

    fn partition_plan(&self, partials: &[Partial<'_>]) -> Option<PartitionPlan> {
        let entries: u64 = partials.iter().filter(|p| p.bytes.len() >= HASHBUILD_HDR).map(|p| unsafe { HashBuild::view(p.bytes.as_ptr()) }).filter(|b| b.cap != 0).map(|b| b.len as u64).sum();
        partition_plan(entries, HASHBUILD_ENTRY as u32)
    }

    /// The destination: one partitioned `BUILD_TABLE` over the leader region, refused when the
    /// entries would pass 7/8 of it (the serial fold then spills as it must); its slices are
    /// zeroed by the jobs (`merge_partition`), not here on the leader.
    fn prepare_partitions(&self, leader: &mut [u8], plan: PartitionPlan, partials: &[Partial<'_>]) -> Result<()> {
        // the row store's slices must be the plan's partitions (one job appends one slice); else the serial fold
        let mut entries = leader.len() as u32;
        if let Some(ps) = partials.iter().filter(|p| p.bytes.len() >= HASHBUILD_HDR).find_map(|p| unsafe { RowStore::of(p.bytes.as_ptr()) }) {
            if ps.bits() != plan.pbits {
                return Err(Error::Unsupported(UNSUPPORTED_SCRATCH));
            }
            if unsafe { HashBuild::armed(leader.as_ptr()) } == 0 {
                unsafe { HashAgg::arm(leader.as_mut_ptr(), leader.len() as u32) };
            }
            unsafe { RowStore::carve(leader.as_mut_ptr(), ps.row_hdr as u32, ps.parts) }.ok_or(Error::Unsupported(UNSUPPORTED_SCRATCH))?;
            entries = unsafe { HashBuild::armed(leader.as_ptr()) };
        }
        let cap = HashBuild::table_cap_of(entries) as u64;
        if cap < 1 << plan.pbits || plan.groups > cap - cap / 8 {
            return Err(Error::Unsupported(UNSUPPORTED_SCRATCH));
        }
        let want = (plan.groups.saturating_mul(2) + 1).next_power_of_two().min(u32::MAX as u64) as u32;
        unsafe { HashBuild::at_partitioned_lazy(leader.as_mut_ptr(), entries, plan.pbits, want) };
        Ok(())
    }

    fn prepares_partials(&self) -> bool {
        true
    }

    /// Phase 2a on the worker's own partial: its entries sorted by partition (pass 3 item 3), so
    /// phase 2b reads each partial's slice of `part` and nothing else; no room for the directory
    /// refuses, and the serial fold takes over.
    fn prepare_partial(&self, partial: Region, plan: PartitionPlan) -> Result<()> {
        if partial.len < HASHBUILD_HDR {
            return Ok(());
        }
        // SAFETY: the partial is this job's alone (`Region`), a live build the worker laid out.
        let b = unsafe { &mut *(partial.ptr as *mut HashBuild) };
        if b.cap == 0 || b.table != gravity_abi::breaker::BUILD_PARTITION {
            return Ok(());
        }
        if b.sort_by_partition(plan.pbits) {
            Ok(())
        } else {
            Err(Error::Unsupported(UNSUPPORTED_SCRATCH))
        }
    }

    /// Slice `part` zeroed, then partition `part` of every partial (its sorted range; the whole
    /// partial scanned only when a partial was not sorted) inserted into it.
    fn merge_partition(&self, leader: Region, part: u32, plan: PartitionPlan, partials: &[Partial<'_>]) -> Result<()> {
        let dst = leader.ptr as *mut HashBuild;
        // SAFETY: slice `part` of the destination is this job's alone (`Region`); `len` is added
        // once per partition (a per-entry atomic on the shared header was 8 % of the workers' time).
        unsafe { HashBuild::zero_slice_at(dst, part) };
        let mut n = 0u32;
        let mut store = unsafe { RowStore::of_mut(leader.ptr) };
        let mut rows = 0u32;
        // C9: under the byte the phase-2 job lends (driver section.rs `merge_partitions`); a batch per entry
        let mut step = cancelpoll::step("merge merge_partition: a build partition's entries");
        for p in partials {
            if p.bytes.len() < HASHBUILD_HDR {
                continue;
            }
            let b = unsafe { HashBuild::view(p.bytes.as_ptr()) };
            if b.cap == 0 {
                continue;
            }
            // the partial's slice `part` lands in the leader's (this job's alone); its entries' offsets take the delta
            let delta = match (store.as_mut(), unsafe { RowStore::of(p.bytes.as_ptr()) }) {
                (Some(ls), Some(ps)) => {
                    rows += ps.len;
                    ls.append_slice(part as u16, ps.part_bytes(part as u16)).ok_or(Error::Unsupported(UNSUPPORTED_SCRATCH))?
                }
                _ => 0,
            };
            match b.partition_range(part, plan.pbits) {
                Some((lo, hi)) => {
                    for i in lo..hi {
                        step.batch(i)?;
                        let e = b.entry(i);
                        let payload = (e.payload as u32).wrapping_add(delta) as i64;
                        if unsafe { HashBuild::insert_partitioned_uncounted(dst, e.key, payload, e.used) }.is_none() {
                            for (k, v) in [part, b.len, b.cap, b.sorted as u32, lo as u32, hi as u32, n, e.key as u32].into_iter().enumerate() {
                                gravity_driver::P2_DIAG[k].store(v, core::sync::atomic::Ordering::Relaxed);
                            }
                            return Err(Error::Unsupported(UNSUPPORTED_SCRATCH));
                        }
                        n += 1;
                    }
                }
                None => {
                    for (i, e) in b.entries().enumerate() {
                        step.batch(i)?;
                        if HashBuild::partition_of(e.key, plan.pbits) != part {
                            continue;
                        }
                        let payload = (e.payload as u32).wrapping_add(delta) as i64;
                        if unsafe { HashBuild::insert_partitioned_uncounted(dst, e.key, payload, e.used) }.is_none() {
                            return Err(Error::Unsupported(UNSUPPORTED_SCRATCH));
                        }
                        n += 1;
                    }
                }
            }
        }
        unsafe { HashBuild::add_len(dst, n) };
        if let (Some(ls), 0) = (store, part) {
            unsafe { RowStore::add_len(ls as *mut RowStore, rows) };
        }
        Ok(())
    }
}

/// A k-way merge of sorted runs into `push` (a binary heap over the run heads: `O(n log k)`
/// comparisons where the repeated two-way merge does `O(n k)`), the tie order by run index.
pub fn kway_merge<'a>(runs: &[&'a SortedRun], cap_rows: u32, less: impl Fn((&[Datum], &[u8]), (&[Datum], &[u8])) -> bool, mut push: impl FnMut((&'a [Datum], &'a [u8])) -> Result<()>) -> Result<()> {
    const MAX_RUNS: usize = gravity_program::cost::runtime::KWAY_MERGE_RUNS;
    if runs.len() > MAX_RUNS {
        gravity_abi::limits::exceeded(format_args!("a k-way merge over {} runs: at most {MAX_RUNS}", runs.len()));
    }
    let mut pos = [0usize; MAX_RUNS];
    let mut heap = [0usize; MAX_RUNS];
    let mut n = 0usize;
    let before = |a: usize, b: usize, pos: &[usize; MAX_RUNS]| -> bool {
        let (ra, rb) = (runs[a].row(pos[a], cap_rows), runs[b].row(pos[b], cap_rows));
        less(ra, rb) || (!less(rb, ra) && a < b)
    };
    let sift_up = |heap: &mut [usize; MAX_RUNS], mut i: usize, pos: &[usize; MAX_RUNS]| {
        // bounded("small-fixed: a heap of at most MAX_RUNS (KWAY_MERGE_RUNS) runs, at most log2(MAX_RUNS) levels")
        while i > 0 {
            let parent = (i - 1) / 2;
            if before(heap[i], heap[parent], pos) {
                heap.swap(i, parent);
                i = parent;
            } else {
                break;
            }
        }
    };
    // bounded("small-fixed: a heap of at most MAX_RUNS (KWAY_MERGE_RUNS) runs, at most log2(MAX_RUNS) levels")
    let sift_down = |heap: &mut [usize; MAX_RUNS], n: usize, mut i: usize, pos: &[usize; MAX_RUNS]| loop {
        let (l, r) = (2 * i + 1, 2 * i + 2);
        let mut m = i;
        if l < n && before(heap[l], heap[m], pos) {
            m = l;
        }
        if r < n && before(heap[r], heap[m], pos) {
            m = r;
        }
        if m == i {
            break;
        }
        heap.swap(i, m);
        i = m;
    };
    // bounded("small-fixed: one run, at most MAX_RUNS (KWAY_MERGE_RUNS), each a heap insert")
    for (k, run) in runs.iter().enumerate() {
        if run.nrows > 0 {
            heap[n] = k;
            n += 1;
            sift_up(&mut heap, n - 1, &pos);
        }
    }
    // C9: on the byte the running kernel call lends (outside one, a step that never stops); a batch per row merged
    let mut step = cancelpoll::step("merge kway_merge: the runs' rows merged");
    let mut rows = 0usize;
    while n > 0 {
        step.batch(rows)?;
        rows += 1;
        let k = heap[0];
        push(runs[k].row(pos[k], cap_rows))?;
        pos[k] += 1;
        if pos[k] < runs[k].nrows as usize {
            sift_down(&mut heap, n, 0, &pos);
        } else {
            n -= 1;
            heap[0] = heap[n];
            sift_down(&mut heap, n, 0, &pos);
        }
    }
    Ok(())
}

pub struct RowsUnordered;

impl Merge for RowsUnordered {
    fn merge_into(&self, _leader: &mut State<'_>, _partial: Partial<'_>) -> Result<()> {
        Ok(())
    }
}

#[repr(C)]
pub struct SortedRun {
    pub natts: u32,
    pub nrows: u32,
}

impl SortedRun {
    pub const fn bytes(natts: u32, cap_rows: u32) -> u32 {
        (core::mem::size_of::<SortedRun>() as u32) + cap_rows * natts * 9
    }

    /// # Safety
    /// `p` points at `bytes(natts, cap)` bytes, 8-aligned.
    pub unsafe fn at<'a>(p: *mut u8) -> &'a mut SortedRun {
        &mut *(p as *mut SortedRun)
    }

    fn values(&self) -> *const Datum {
        unsafe { (self as *const SortedRun as *const u8).add(core::mem::size_of::<SortedRun>()) as *const Datum }
    }

    fn nulls(&self, cap_rows: u32) -> *const u8 {
        unsafe { (self.values() as *const u8).add(cap_rows as usize * self.natts as usize * 8) }
    }

    pub fn row(&self, i: usize, cap_rows: u32) -> (&[Datum], &[u8]) {
        let n = self.natts as usize;
        unsafe { (core::slice::from_raw_parts(self.values().add(i * n), n), core::slice::from_raw_parts(self.nulls(cap_rows).add(i * n), n)) }
    }

    pub fn push(&mut self, cap_rows: u32, v: &[Datum], nulls: &[u8]) -> bool {
        if self.nrows >= cap_rows {
            return false;
        }
        let n = self.natts as usize;
        let i = self.nrows as usize;
        unsafe {
            core::ptr::copy_nonoverlapping(v.as_ptr(), (self.values() as *mut Datum).add(i * n), n);
            core::ptr::copy_nonoverlapping(nulls.as_ptr(), (self.nulls(cap_rows) as *mut u8).add(i * n), n);
        }
        self.nrows += 1;
        true
    }
}

pub struct RowsOrdered {
    pub natts: u32,
    pub cap_rows: u32,
    pub keys: [i16; 4],
    pub nkeys: u8,
}

impl RowsOrdered {
    fn half(&self) -> usize {
        SortedRun::bytes(self.natts, self.cap_rows) as usize
    }

    fn less(&self, a: (&[Datum], &[u8]), b: (&[Datum], &[u8])) -> bool {
        // bounded("small-fixed: at most 4 sort keys (the keys array)")
        for &k in &self.keys[..self.nkeys as usize] {
            let k = k as usize;
            let (x, y) = (a.0[k] as i64, b.0[k] as i64);
            if x != y {
                return x < y;
            }
        }
        false
    }

    fn two_way(&self, leader: &mut State<'_>, partial: &[u8]) -> Result<()> {
        let half = self.half();
        let which = leader.bytes[2 * half] as usize;
        let (front_off, back_off) = if which == 0 { (0, half) } else { (half, 0) };
        let base = leader.bytes.as_mut_ptr();
        let front = unsafe { SortedRun::at(base.add(front_off)) };
        let back = unsafe { SortedRun::at(base.add(back_off)) };
        let run = unsafe { &*(partial.as_ptr() as *const SortedRun) };
        back.natts = self.natts;
        back.nrows = 0;
        let (mut i, mut j) = (0usize, 0usize);
        // C9: under the byte the leader's fold lends (driver section.rs `close_section`); a batch per row merged
        let mut step = cancelpoll::step("merge two_way: an ordered partial merged");
        while i < front.nrows as usize || j < run.nrows as usize {
            step.batch(i + j)?;
            let take_run = j < run.nrows as usize && (i >= front.nrows as usize || self.less(run.row(j, self.cap_rows), front.row(i, self.cap_rows)));
            let (v, n) = if take_run {
                j += 1;
                run.row(j - 1, self.cap_rows)
            } else {
                i += 1;
                front.row(i - 1, self.cap_rows)
            };
            if !back.push(self.cap_rows, v, n) {
                return Err(Error::Unsupported(UNSUPPORTED_SCRATCH));
            }
        }
        leader.bytes[2 * half] = (which ^ 1) as u8;
        Ok(())
    }
}

impl Merge for RowsOrdered {
    fn merge_into(&self, leader: &mut State<'_>, partial: Partial<'_>) -> Result<()> {
        assert!(leader.bytes.len() > 2 * self.half(), "ordered merge state = two runs and a flag byte");
        let run = unsafe { &*(partial.bytes.as_ptr() as *const SortedRun) };
        if run.nrows == 0 {
            return Ok(());
        }
        self.two_way(leader, partial.bytes)
    }

    fn finish(&self, leader: &mut State<'_>) -> Result<()> {
        let half = self.half();
        let which = leader.bytes[2 * half] as usize;
        let front = unsafe { SortedRun::at(leader.bytes.as_mut_ptr().add(if which == 0 { 0 } else { half })) };
        if front.natts == 0 {
            return Ok(());
        }
        let n = self.natts as usize;
        let rows = front.nrows as usize;
        let values = unsafe { core::slice::from_raw_parts(front.values(), rows * n) };
        let nulls = unsafe { core::slice::from_raw_parts(front.nulls(self.cap_rows), rows * n) };
        leader.sink.push(BorrowedBatch { natts: n, rows, values, isnull: nulls, region: &[] }).map(|_| ())
    }
}
