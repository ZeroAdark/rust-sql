use gravity_abi::{word, PtrWord, Word};
use crate::host_abi::*;

// ---- the keyed fold's entries (group.rs h_open_sized / h_block / h_miss / h_finish) ----

/// The fold's claim state the IR reads by offset: the phase-1 table, its heads, the table's slots and the partition words.
#[repr(C)]
pub struct AggState {
    pub t: Word<*mut T1>,
    pub heads: Word<*mut u64>,
    pub table: Word<*mut u8>,
    pub mask: u64,
    pub thresh: u64,
    pub delta: u64,
    pub pmask: u64,
    pub pbits: u64,
    pub layout: Word<*const RecordLayout>,
    /// D1392 (a): the table's image arena BY REFERENCE: `arena_cur` = `T1::img_words(t)`, `arena_chunk` = `IMG_BY_REF`,
    /// set at every claim's open. The arena itself (cursor, end, chunk size: 64 KiB doubling to `IMAGE_CHUNK_MAX`, the
    /// pool bounded by the grant's bytes, D642) lives in the worker's T1 header, kept across its claims; this
    /// claim-lived state was zeroed at each claim, so every claim pinned a fresh chunk (a large grouped scan at width
    /// 8: 17 GB). The lowered programs still pass these words' address (`GroupRec::images`, no extra load per row); the
    /// copy hosts resolve it (`T1::img_resolve`). D3312: `arena_end` names the table itself (`T1::img_table`: its copies'
    /// dedupe), never a cursor's end.
    pub arena_cur: Word<*mut u8>,
    pub arena_end: Word<*mut u8>,
    pub arena_chunk: u64,
}

pub const AGG_STATE_BYTES: u32 = size_of::<AggState>() as u32;
/// The hashagg row loop's bypass word behind `AggState` (after the append count): read per row, written by `agg_open`'s IR and by
/// `locate` when the estimate's bypass is confirmed mid-claim. Only a `T1_EST` table (an `agg_open` of the hashagg tail) writes it.
pub const AGG_BYPASS_WORD: usize = AGG_STATE_BYTES as usize + 8;
/// Evictions that confirm the planner's estimate (`T1_EST`), with the table at least half full: a fold with fewer groups than
/// half the slots never confirms it, whatever its home collisions evict.
const EST_CONFIRM: u64 = 64;
pub use gravity_abi::breaker::T1_MIN_SLOTS as AGG_MIN_SLOTS;

pub fn t1_bytes(groups: u64, rec: u64) -> u32 {
    if groups == 0 {
        return t1_default_bytes();
    }
    // past 2^60 groups no power of two is above `groups × ADAPT_FACTOR` (D371): the largest table, the default's
    let slots = groups.saturating_mul(ADAPT_FACTOR).checked_next_power_of_two().unwrap_or(u64::MAX).max(AGG_MIN_SLOTS);
    slots.saturating_mul(rec).min(t1_open_bytes(groups, rec) as u64) as u32
}

/// The IR's radix bits (lowerv4 table/radix.rs: the bound's records over the reference slice), 0 = the breaker's default.
fn radix_bits(bits: u64) -> u8 {
    if bits as u8 == 0 { gravity_abi::breaker::PARTITION_DEFAULT_BITS } else { (bits as u8).min(T1_MAX_PARTS.trailing_zeros() as u8) }
}

/// A keyed table's partition bits from the IR's (`agg_open`, entries.rs `agg_open_grown`; lowerv4 table/radix.rs `bits_for`):
/// the bound only adds partitions past the default (radix.rs: past the runtime's floor of 16), so a bound of 2..15 tables'
/// worth (1..3 bits) opens at the default, as no bound does. Taken as they came, those bits fell under DR-6's least
/// (`PARTITION_MIN_BITS`), and a fold of 36k one-word pairs at W0 ran in 2 partitions where 32k ran in 16, 1.06-1.14x slower
/// (posture DEV t1e / log t1z, group). Only the partitions: the open's no-estimate bypass rule reads the IR's bits as they
/// are (`bypass_from_open`), and `pass1_open`'s DR-6 bits (3 and up, `params::spill_bits`) are not a bound's.
pub fn table_bits(bits: u64) -> u64 {
    if (bits as u8) < gravity_abi::breaker::PARTITION_DEFAULT_BITS { 0 } else { bits }
}

/// B3-16 / B3-17: the bits a fold's phase-1 table opens with (agg_open, entries.rs agg_open_grown, pass1_open), DR-6's rule at
/// the open: the radix bits (the breaker's default with none) re-cut to the machine slice (`machine_bits`, the identity on the
/// reference machine), capped by the region's share (`granted_bits`), below the wide width no further than each partition's
/// expected `groups` of `rec` bytes still fit the share (`fit_bits`; no cap without an estimate), then floored by width
/// (`opened_bits`, whose wide floor holds from `PARTITION_WIDE_WIDTH` workers).
/// The setop posture (option c'): a live `T1_RESERVED` table keeps the partitions it was laid with. Its pool grows in place
/// past the share (`T1::grow_pool` raises `limit`), so a reopen (a set operation side's UNION ALL arms folding on into the
/// one region, source/append.rs `continues`) re-cut at the grown limit would ask the live table for other partitions.
/// A region that started small (`lay` > 0, the driver's `RESERVE_LAYOUT` answer: `laid_cut`) is cut by its share, as a
/// share-committed region is by its limit (D3809): never by the start its `limit` holds, nor by its reservation, which
/// opened up to `T1_MAX_PARTS` partitions where the share opens a few hundred, each first eviction a fresh block.
/// D4033: a worker share the pool's room opened smaller is cut by its section's share, which the driver's answer carries
/// (`open_cut`), never by its own smaller limit: every phase-1 partial of one owner phase holds the same partitions.
/// # Safety
/// `ctx` is the claim's, its partial region armed with the share.
pub unsafe fn fold_bits(ctx: *const Ctx, bits: u64, arm_run: bool, groups: u64, rec: u64, lay: u64) -> u8 {
    use gravity_abi::breaker::TABLE_T1;
    let t = T1::view(partial(ctx as *mut Ctx));
    // a live table keeps the partitions it was laid with, whatever its limit now. Its shape is asserted at every reopen
    // (`T1::at_laid`), and only its first open asks the driver for a cut (`region_reserve_laid` at `cap == 0`): a reserved
    // table's limit grows in place, and D4033's table was cut by its section's share, not by its own limit
    if t.cap != 0 && t.kind == TABLE_T1 {
        return t.pbits;
    }
    cut_bits(bits, if lay != 0 { lay } else { t.limit as u64 }, (*ctx).width, arm_run, groups, rec)
}

/// `fold_bits`' rule over the bytes a table is cut by (`share`), the one rule for both roads (a share-committed region's
/// limit, a started-small region's share): the radix bits re-cut to the machine slice, capped by the share, below the wide
/// width no further than each partition's groups still fit it, then floored by width.
fn cut_bits(bits: u64, share: u64, width: u16, arm_run: bool, groups: u64, rec: u64) -> u8 {
    use gravity_abi::breaker::{fit_bits, granted_bits, machine_bits, machine_slice, opened_bits, PARTITION_WIDE_WIDTH};
    let b = machine_bits(radix_bits(bits), machine_slice());
    let granted = granted_bits(b, share, t1_default_bytes() as u64);
    let capped = if width < PARTITION_WIDE_WIDTH { granted.max(fit_bits(b, groups, rec, share)) } else { granted };
    opened_bits(capped, width, arm_run)
}

/// D3809: the bytes a started-small region's partitions are cut by, from the driver's `need` 0 answer: its share, else
/// (an answer without one) its reservation, as before.
pub fn laid_cut(reserve: u64, share: u64) -> u64 {
    if share != 0 { share } else { reserve }
}

/// The bytes `fold_bits` cuts a table's partitions by (its `lay`), from the driver's `need` 0 answer
/// (`region_reserve_laid`): a started-small region's `laid_cut`; D4033: any other region's carried share, which the driver
/// sends only for a worker share the pool's room opened smaller (`WorkerSlot::share_cut`). That worker then cuts by its
/// section's share, as every other worker of the section does by its limit, so every phase-1 partial of one owner phase
/// holds the same partitions. Cut by its own smaller limit, it opened fewer, and the owner phase's partitions past them read
/// past its head words. 0 (no share carried): the table's limit, as before.
pub fn open_cut(reserve: u64, laid: bool, share: u64) -> u64 {
    if laid { laid_cut(reserve, share) } else { share }
}

/// The bypass rule at open for a shape with no estimate: the partition space `2^bits × 512 KiB` past `ADAPT_FACTOR` times the
/// table's bytes (the default's, or twice a share-clipped table's power-of-two floor) — bytes against bytes, so a record size off
/// a power of two never flips a 23-group fold to a record per run.
fn bypass_from_open(t: &T1, bits: u8, groups: u64, rec: u64) -> bool {
    if groups != 0 {
        return false;
    }
    let table = (t1_default_bytes() as u64).min(2 * t.cap as u64 * rec);
    (1u64 << bits) * (t1_default_bytes() as u64) > ADAPT_FACTOR * table
}

/// V4R-375: a layout holding hosted aggregate states (`SlotKind::AggState`): its table never flips to the bypass at run time
/// (the open's rule, the estimate, the observation), so a state lives per group and partial, never per row.
fn hosted_layout(l: &RecordLayout) -> bool {
    l.slot_kinds().contains(&SlotKind::AggState)
}

/// The planner's estimate (or the groups the last execution observed) past `est_bypass_load` keys a slot of the table
/// asks for the bypass, confirmed by `EST_CONFIRM` evictions (`T1_EST`, `locate`) — never taken blind: a GROUP BY of 4
/// groups over two char(n) columns is estimated at 6 M on pgrc (no ndistinct for char(n)) and the blind bypass appended
/// every input row, 4 GB one owner had to fold at W0 (D089). The slots are the table's own (`cap`: its bytes over the
/// record's), so the rule reads the keys against what the table holds, whatever the record's width.
fn estimate_asks_bypass(t: &T1, groups: u64, width: u16) -> bool {
    groups > est_bypass_load(width) * t.cap as u64
}

/// Keys a slot past which the estimate asks for the bypass (`estimate_asks_bypass`), by the fold's width.
/// From `PARTITION_WIDE_WIDTH` workers: one, more keys than the table has slots. Every worker's table sees the keys, a row's
/// miss is a host locate, and `observe` never fires below 95 % evictions, while the appended rows fold on every worker in
/// the owner phase. CHOSEN (group, posture sweep, box 7, W8 vs trunk's 8 x): count(DISTINCT) of 36k..512k spread keys over
/// 2M rows 0.49-0.59x, GROUP BY of 20k..128k keys with a 24-byte record (16k slots) 0.53-0.54x, keys at or under the slots
/// 0.98-1.02x, and keys with locality (each opened once, evicted once done) 0.83-1.00x.
/// Below it: `ADAPT_FACTOR`, the rule before. One owner folds every appended row, and keys with locality fold in the
/// table at about a record a key: an early bypass appended every row, count(DISTINCT) of ~200k such keys at W0 1.62-1.70x
/// and a one-worker GROUP BY of 28.5k order keys over its 171k items 1.135x (posture DEV t1l / cliff rnd-2746), where
/// spread keys gained 0.73-0.84x (24-byte GROUP BY, 24k..128k keys). Telling the two apart needs a sign that an evicted
/// key came back (DESIGN-OPEN, HITS9-E5).
fn est_bypass_load(width: u16) -> u64 {
    if width >= gravity_abi::breaker::PARTITION_WIDE_WIDTH { 1 } else { ADAPT_FACTOR }
}

/// A bit on `agg_open`'s `groups`: the estimate sizes the table but never sets the bypass — the table is HELD (`T1_HOLD`: the floor form's
/// pass 1 under the code front, its inserts one per touched code per part, merged 5:1 by the derived key where the estimate counts groups;
/// the convert drains it per part, so the section-lived table never reads as thrashing to `observe()` — the ladder's per-claim table).
pub const AGG_GROUPS_OBSERVE: u64 = 1 << 62;
/// A bit on `agg_open`'s `groups`: the table bypassed by declaration (the floor form's pass 2 over pass 1's slot: one record per admitted
/// run, merged by the owners — the posture it inherited from pass 1's estimate bypass before the hold; held, its 21 k (key, part) records thrash 8192 slots).
pub const AGG_GROUPS_BYPASS: u64 = 1 << 61;
/// `agg_open`'s word at the section end: the worker's table drained, the call has nothing to scan.
pub const AGG_SECTION_DONE: i64 = 1;

crate::contract::host! {
    /// The claim's open: the T1 over `layout` at `fold_bits` (the IR's radix bits by DR-6's rule: machine slice, width) sized by the planner's group estimate, the bypass rule, the state filled. The table lives for the section: laid out at the worker's first claim, kept across its claims, drained once under the `SECTION_END` claim (`AGG_SECTION_DONE`; a full pool spilled).
    #[guard(ERROR)]
    pub unsafe fn agg_open(ctx: *mut Ctx, st: *mut u8, layout: *const RecordLayout, bits: u64, groups: u64) -> i64 {
        let st = st as *mut AggState;
        let rec = (*layout).size() as u64;
        let (observe_only, declared) = (groups & AGG_GROUPS_OBSERVE != 0, groups & AGG_GROUPS_BYPASS != 0);
        let groups = groups & !(AGG_GROUPS_OBSERVE | AGG_GROUPS_BYPASS);
        // the setop posture (option c'): a set operation's side may own a region reserved past its share (the driver's
        // answer, asked once, where the table is laid out; 0 = none): its pool then grows in place (`folded_in_memory`).
        // A region that started small (`laid`) is laid out at its reservation.
        let own = partial(ctx);
        let (reserve, floor, laid, cut) = if (*(own as *const T1)).cap == 0 { gravity_abi::program::region_reserve_laid(ctx) } else { (0, false, false, 0) };
        let pbits = fold_bits(ctx, table_bits(bits), (*ctx).flags & gravity_abi::ctx::FLAG_ARM_RUN != 0, groups, rec, open_cut(reserve, laid, cut));
        let bits = radix_bits(bits);
        let first = t1_bytes(groups, rec);
        let t = T1::at_laid(own, &*layout, pbits, first, first, SHARE, reserve, laid) as *mut T1;
        if floor && (*t).flags & gravity_abi::breaker::T1_RESERVED != 0 {
            (*t).flags |= gravity_abi::breaker::T1_FLOOR; // D2894: its pool commits before it compacts
        }
        // the hold is the claim's, not the region's: a live table reopened by a pipeline without the hold word (pass 2 over pass 1's slot and layout) drops it
        if observe_only {
            (*t).flags |= T1_HOLD;
        } else {
            (*t).flags &= !(T1_HOLD | T1_EST);
            // V4R-375: a hosted table (AggState slots) stays keyed at run time: a record per row keeps a state per row
            let hosted = hosted_layout(&*layout);
            if declared || (!hosted && bypass_from_open(&*t, bits, groups, rec)) {
                (*t).flags |= T1_BYPASS;
            } else if !hosted && (*t).flags & T1_BYPASS == 0 && estimate_asks_bypass(&*t, groups, (*ctx).width) {
                (*t).flags |= T1_EST;
            }
        }
        let base = t as *mut u8;
        *st = AggState { t: word(t), heads: word(base.add(T1_HDR) as *mut u64), table: word(base.add((*t).table_off as usize)), mask: (*t).cap as u64 - 1, thresh: rec << HEAD_LEFT, delta: rec.wrapping_sub(rec << HEAD_LEFT), pmask: (1u64 << pbits) - 1, pbits: pbits as u64, layout: word(layout), arena_cur: word(T1::img_words(t) as *mut u8), arena_end: word(t as *mut u8), arena_chunk: IMG_BY_REF };
        if (*ctx).claim.is_section_end() {
            return match agg_finish(ctx, st as *mut u8) {
                0 => AGG_SECTION_DONE,
                e => e,
            };
        }
        0
    }
}

/// The image hosts' failure (0), the error named for the raise (`raise::HOST` carries no text of its own).
#[cold]
fn no_pin(_what: &str, bytes: usize) -> i64 {
    pin_refused(bytes);
    0
}

/// D1392 (b): a `Program.pin` the grant pool refused, stashed as PG's "out of memory" (53200) for the raise that follows
/// (`raise::HOST` carries no text: without it the client read "an error without a message (raise code 7)"). Every
/// statement-lived image copy (a kept group key, a text extreme, a pulled run's copy) names it the same way.
#[cold]
pub fn pin_refused(bytes: usize) {
    gravity_fn::stash_error(crate::ops::agg::trans::entries::out_of_memory_in(bytes, "a Gravity pinned region (the statement's pinned pool is full)"));
}

crate::contract::host! {
    /// `agg_image_put` without a state: the image copied into a pinned region of its own (the plain fold's one record: a copy per improvement of its extreme, never per row).
    #[guard(0)]
    pub unsafe fn agg_image_pin(ctx: *mut Ctx, ptr: *const u8, len: u64) -> i64 {
        let need = (len as usize + 4 + 7) & !7;
        let d = gravity_abi::program::pin(ctx, need as u64);
        if d.is_null() {
            return no_pin("pin", need);
        }
        core::ptr::write_unaligned(d as *mut u32, ((len as u32 + 4) << 2).to_le());
        core::ptr::copy_nonoverlapping(ptr, d.add(4), len as usize);
        d as i64
    }
}

crate::contract::host! {
    /// The image of `len` bytes at `ptr` copied into the owner's arena as a 4-byte-header varlena, its datum address returned (0 when the
    /// arena cannot grow): once per new group, never per row; the arena's chunks are the driver's pinned pool chunks (`Program.pin`: the
    /// ctx bump arena is rewound at every Call), so a pointer merged into another worker's record or spilled with it stays valid to the statement's end.
    #[guard(0)]
    pub unsafe fn agg_image_put(ctx: *mut Ctx, st: *mut u8, ptr: *const u8, len: u64) -> i64 {
        // D1392 (a): the table's own words (`T1::img_words`: laid-out-lived, one per worker's table), not the claim's state
        let w = T1::img_words((*(st as *mut AggState)).t.ptr());
        let need = (len as usize + 4 + 7) & !7;
        // D3312: the arena's one bump (its chunk chain and start map)
        let d = gravity_abi::breaker::img_bump(w, need, |bytes| gravity_abi::program::pin(ctx, bytes));
        if d.is_null() {
            return no_pin("put", need);
        }
        core::ptr::write_unaligned(d as *mut u32, ((len as u32 + 4) << 2).to_le());
        core::ptr::copy_nonoverlapping(ptr, d.add(4), len as usize);
        d as i64
    }
}

crate::contract::host! {
    /// D3312: the claim's copy-slot word (lowerv4 `TableDecl::copies`: which record slots hold a key copy's address,
    /// `IMG_SLOTS_KNOWN`) set on its table (`T1::img_slots`), where an in-memory compact's reclaim reads it
    /// (table/img_reclaim.rs). Called after `agg_open` (or `agg_open_grown`) by a table whose records keep key copies.
    pub unsafe fn agg_copies(st: *mut u8, mask: u64) -> i64 {
        *T1::img_slots((*(st as *mut AggState)).t.ptr()) = mask;
        0
    }
}

pub(crate) unsafe fn agg_flush(ctx: *mut Ctx, st: *mut AggState) -> Result<(), i64> {
    // D3312: the image dedupe arms on the sparse evictions of one fill (`T1::img_cache`)
    T1::dedupe_flushed((*st).t.ptr());
    if folded_in_memory(ctx, st)? {
        return Ok(());
    }
    match spill(ctx, SPILL_SCATTER, (*st).pbits as i64) {
        n if n < 0 && matches!(gravity_abi::status::Status::decode(n), gravity_abi::status::Status::Bad(_)) => {
            let t = (*st).t.ptr();
            Err(bare(&format!("agg_flush: scatter returned {n:#x} (T1 buffered {} len {} cap {} parts {} block_bytes {} nblocks {} next_block {} flushes {} kind {} limit {}; worker {}/{} claim {}..{})", (*t).buffered, (*t).len, (*t).cap, (*t).parts(), (*t).block_bytes, (*t).nblocks, (*t).next_block, (*t).flushes, (*t).kind, (*t).limit, (*ctx).worker_ordinal, (*ctx).width, (*ctx).claim.lo, (*ctx).claim.hi))) // alloc: Cold
        }
        n if n < 0 => Err(n),
        _ => Ok(()),
    }
}

/// D2587 (CAND-M25): a flush done in memory, as PostgreSQL's HashAggregate keeps every group in hash_mem and spills only
/// once its groups pass it. A flush leaves the table empty and the pool with room (the driver's scatter drains the table
/// and writes both): here the table drains into the pool and the buffers fold by key (`T1::compact`), in turn, until
/// the table is empty with a quarter of the pool's blocks free, so a table whose groups fit its region writes no
/// temporary file. Past that the groups do not fit: `T1_NOFOLD` set, and this and every later flush spill. Not for a
/// held table, a hosted state (its combine is the owner fold's) or a float slot (a merge's overflow leaves its record
/// garbage, D244). The setop posture (option c'): a `T1_RESERVED` table (a set operation's side) whose compacted pool has
/// no room commits more of its reservation IN PLACE first (`pool_grown`), as PostgreSQL's hashed SetOp keeps every group
/// in memory past work_mem; only a spent reservation or a refused draw sets `T1_NOFOLD`. NEW-numeric-1 (P): a buffer's
/// records of a key are in row order, so `fold(into, from)` folds the newer into the older (`Rec::fold_newer`: an
/// order-key layout's tie takes the newer member, as PostgreSQL does).
/// The compact's block map and index (and the image reclaim's lists) are one scratch arena of this call on the table's
/// own breaker slot owner (`OpArena::own_owner_kind`: the allocator's road, charged to that owner under its kind) when
/// they may reach `ARENA_SCRATCH_MIN` (LEDGER2 step 5's rule: a smaller cut asks no owner and stays uncharged, where a
/// `Vec` was); a carve it refuses is the statement's out of memory (`Err` with the error stashed), where the allocator's
/// `Vec` aborted.
unsafe fn folded_in_memory(ctx: *mut Ctx, st: *mut AggState) -> Result<bool, i64> {
    use gravity_abi::breaker::{T1_FLOOR, T1_NOFOLD};
    use gravity_fn::arena::{AVec, Kind, OpArena};
    let (t, layout) = ((*st).t.ptr(), &*(*st).layout.ptr());
    // a never-compacted table that started small (`T1_GROWS`, pass 1's bare table) still grows its pool in place first
    let nofold = (*t).flags & (T1_NOFOLD | gravity_abi::breaker::T1_GROWS) == T1_NOFOLD;
    if (*t).flags & (T1_HOLD | T1_FLOOR) == T1_FLOOR && !nofold && (*t).buffered != 0 && floor_grown(ctx, t) {
        return Ok(true);
    }
    let raises = |k: SlotKind| matches!(k, SlotKind::SumF64 | SlotKind::SumF32 | SlotKind::F64Accum | SlotKind::AggState);
    if (*t).flags & (T1_HOLD | T1_NOFOLD) != 0 || (*t).buffered == 0 || layout.slot_kinds().iter().any(|&k| raises(k)) {
        return Ok(false);
    }
    let refused = |r: gravity_fn::arena::Refused| crate::ops::agg::trans::entries::failed(crate::ops::agg::trans::entries::out_of_memory(r.bytes as usize));
    // the slot's own owner and ledger kind (a HashAgg, a Distinct or a set-op side), as the host answers them, when the
    // lists may reach ARENA_SCRATCH_MIN: the map (4 bytes a block), the index (4 bytes a slot, at most four slots a
    // record of the longest chain: bounded by the records the pool's blocks can hold) and the reclaim's
    let recs = (*t).nblocks as usize * (gravity_abi::breaker::BLOCK_BYTES as usize / (layout.size() as usize).max(1));
    let most = 4 * (*t).nblocks as usize + 16 * recs + crate::table::img_reclaim::scratch_bound(t);
    let owner = match most >= gravity_abi::program::ARENA_SCRATCH_MIN { true => OpArena::own_owner_kind(ctx), false => None };
    let (o, k) = owner.map_or((None, Kind::HashAgg), |(o, k)| (Some(o), k));
    let a = OpArena::scratch_of(o, k);
    let mut map = AVec::<u32>::new();
    map.resize(&a, (*t).nblocks as usize, 0).map_err(refused)?;
    loop {
        let drained = T1::drain(t);
        let before = (*t).next_block;
        let mut index = AVec::<u32>::new();
        index.resize(&a, (2 * (*t).longest_chain()).max(2).next_power_of_two(), 0).map_err(refused)?;
        T1::compact(t, index.as_mut_slice(&a), map.as_mut_slice(&a), &mut |into, from| Rec::at(into, layout).fold_newer(&Rec::at(from as *mut u8, layout)).is_ok());
        // the newest carve back: the map's growth below extends it in place
        index.release(&a);
        let free = |t: *mut T1| 4 * ((*t).nblocks - (*t).next_block) >= (*t).nblocks;
        let (mut room, mut grew) = (free(t), false);
        while !room && pool_grown(ctx, t) {
            map.resize(&a, (*t).nblocks as usize, 0).map_err(refused)?;
            (room, grew) = (free(t), true);
        }
        if drained && room {
            // D3312: the table empty and every record in the pool: the images the folds left behind are reclaimed
            crate::table::img_reclaim::reclaim(t, &a).map_err(refused)?;
            return Ok(true);
        }
        // a grown pool drains on (its new blocks take the table's rest); else a compact that freed nothing gives up
        if !room || (!grew && (*t).next_block == before) {
            (*t).flags |= T1_NOFOLD;
            return Ok(false);
        }
    }
}

/// D2894: a `T1_FLOOR` table's flush done by its floor before any compact: the table drains into the pool, which doubles
/// in place (`pool_grown`) whenever it has no room or keeps under a quarter of its blocks free, as PostgreSQL's
/// HashAggregate fills hash_mem before it spills (a compact at a modeled share under it re-read the pool every time it
/// filled: a high-NDV group at W0 1.08x). Records are only moved, never folded, so a float slot or a hosted state rides
/// it too. False when the floor is spent or a commit refused (the pool has no bytes now): the compact's turn, then the
/// spill.
unsafe fn floor_grown(ctx: *mut Ctx, t: *mut T1) -> bool {
    loop {
        let drained = T1::drain(t);
        if drained && 4 * ((*t).nblocks - (*t).next_block) >= (*t).nblocks {
            return true;
        }
        if !pool_grown(ctx, t) {
            return false;
        }
    }
}

/// Option c': a `T1_RESERVED` table's pool doubled in place (at most to its reservation): the bytes drawn by the driver
/// (`SPILL_REGION_COMMIT`, past-share growth) and committed (`T1::grow_pool`); false when the reservation is spent, the
/// table has none, or the draw was refused.
unsafe fn pool_grown(ctx: *mut Ctx, t: *mut T1) -> bool {
    let need = (*t).pool_growth();
    if need == 0 || gravity_abi::program::region_commit(ctx, need) != need {
        return false;
    }
    let grown = T1::grow_pool(t, need);
    debug_assert!(grown, "a drawn growth fits the reservation the table was laid in");
    grown
}

/// A sort whose `RowCmp` compare failed (`CMP_ERR`): `ERROR` with the compare's own error (a collation's, a hosted btree
/// support function's, rep.hosted.order), PostgreSQL's (SQLSTATE and message), left in the stash for the statement to
/// raise (D1293: `bare` here overwrote it with "gravity: a sort compare failed"); `bare(what)` only when nothing is stashed.
#[cold]
pub(crate) fn cmp_failed(what: &str) -> i64 {
    match gravity_fn::take_error() {
        Some(e) => {
            gravity_fn::stash_error(e);
            ERROR
        }
        None => bare(what),
    }
}

/// A host's own `ERROR` (no driver error behind it): the reason stashed so the statement's error names it.
#[cold]
pub(crate) fn bare(what: &str) -> i64 {
    gravity_fn::stash_error(Box::new(gravity_fn::PgError::error(&format!("gravity: {what}")))); // alloc: Cold
    ERROR
}

crate::contract::host! {
    /// A block behind partition `part`'s head when it has no room (the `appended` records counted first): the head word, or a status below zero.
    #[guard(ERROR)]
    pub unsafe fn agg_block(ctx: *mut Ctx, st: *mut u8, part: u64, appended: u64) -> i64 {
        let st = st as *mut AggState;
        let t = (*st).t.ptr();
        (*t).buffered += appended;
        let mut ap = T1::appender(t);
        if !ap.ensure(part as u32) {
            ap.finish();
            if let Err(e) = agg_flush(ctx, st) {
                return e;
            }
            ap = T1::appender(t);
            if !ap.ensure(part as u32) {
                return unsupported(UNSUPPORTED_SCRATCH);
            }
        }
        ap.finish();
        let head = *(*st).heads.ptr().add(part as usize);
        if (head as i64) <= 0 || head & HEAD_POS & 7 != 0 || part > (*st).pmask {
            return bare(&format!("agg_block: a head word that is no head: part {part} of {} (pbits {}), head {head:#x}, t {:p} heads {:p} table {:p}, T1 parts {} cap {} block_bytes {} nblocks {} next_block {} kind {} limit {}", (*st).pmask + 1, (*st).pbits, t, (*st).heads.ptr(), (*st).table.ptr(), (*t).parts(), (*t).cap, (*t).block_bytes, (*t).nblocks, (*t).next_block, (*t).kind, (*t).limit)); // alloc: Cold
        }
        head as i64
    }
}

pub(crate) unsafe fn locate(ctx: *mut Ctx, st: *mut AggState, keys: &[u64]) -> Result<*mut u8, i64> {
    let t = (*st).t.ptr();
    let pbits = (*st).pbits as u8;
    let hash = KeyHash::of(keys);
    if (*t).flags & T1_BYPASS != 0 {
        // a pair-collapse record goes to its GROUP's partition (D154), as the IR's image road and the drain route it
        let part = ((*t).route(keys, hash)).part(pbits as u32);
        return match T1::append_new(t, part, keys) {
            Some(r) => Ok(r),
            None => {
                agg_flush(ctx, st)?;
                T1::append_new(t, part, keys).ok_or(unsupported(UNSUPPORTED_SCRATCH))
            }
        };
    }
    if let Some(p) = T1::probe(t, keys, hash) {
        return Ok(p);
    }
    if (*t).flags & T1_EST != 0 && (*t).evictions >= EST_CONFIRM && 2 * (*t).len as u64 >= (*t).cap as u64 {
        // the estimate's bypass, confirmed by evictions: the table drained, the claim's row loop told (its bypass word), this record appended
        while !T1::drain(t) {
            agg_flush(ctx, st)?;
        }
        (*t).flags = ((*t).flags & !T1_EST) | T1_BYPASS;
        *((st as *mut u8).add(AGG_BYPASS_WORD) as *mut u64) = T1_BYPASS as u64;
        return locate(ctx, st, keys);
    }
    let home = (*t).home(hash);
    if !T1::evict(t, home) {
        agg_flush(ctx, st)?;
        return T1::probe(t, keys, hash).ok_or(unsupported(UNSUPPORTED_SCRATCH));
    }
    let r = T1::open_at_hashed(t, home, keys, hash);
    let size = (*(*st).layout.ptr()).size() as usize;
    // a record wider than the copy below stays in table mode
    if size <= 256 && (*t).flags & T1_EST == 0 && (*t).observe() && !hosted_layout(&*(*st).layout.ptr()) {
        // the observation says bypass: the table drained, this record moved to its partition's buffer
        let mut tmp = [0u8; 256];
        core::ptr::copy_nonoverlapping(r, tmp.as_mut_ptr(), size);
        // D1448: the open is undone before the drain (the record leaves as `tmp`): left open, a layout without an implicit
        // count kept a keys-only slot the drain skips (sentinel 0) with `len` one high; an implicit count appended it twice
        core::ptr::write_bytes(r, 0, size);
        (*t).len -= 1;
        while !T1::drain(t) {
            agg_flush(ctx, st)?;
        }
        (*t).flags |= T1_BYPASS;
        let part = ((*t).route(keys, hash)).part(pbits as u32);
        let dst = match T1::reserve_for(t, part) {
            Some(d) => d,
            None => {
                agg_flush(ctx, st)?;
                T1::reserve_for(t, part).ok_or(unsupported(UNSUPPORTED_SCRATCH))?
            }
        };
        core::ptr::copy_nonoverlapping(tmp.as_ptr(), dst, size);
        return Ok(dst);
    }
    Ok(r)
}

/// A record merge's float overflow (D244) as the host's status.
pub(crate) fn o_status(_: gravity_abi::breaker::FloatOverflow) -> i64 {
    gravity_abi::breaker::FloatOverflow::STATUS
}

crate::contract::host! {
    /// The probe's miss: the run's record located (its home pair, the occupant evicted; appended in the bypass) and the image `img` (a record of the layout) folded into it.
    #[guard(ERROR)]
    pub unsafe fn agg_miss(ctx: *mut Ctx, st: *mut u8, img: *const u8) -> i64 {
        let st = st as *mut AggState;
        let layout = &*(*st).layout.ptr();
        let src = Rec::at(img as *mut u8, layout);
        match locate(ctx, st, src.keys()) {
            Ok(p) => match Rec::at(p, layout).fold_newer(&src) {
                Ok(()) => 0,
                Err(o) => o_status(o),
            },
            Err(e) => e,
        }
    }
}

crate::contract::host! {
    /// `agg_miss` for the pointer front: the record's address when it landed in the table (the front caches it; a probe-mode slot), 0 when it went to a partition buffer (the bypass: never cached), a status below zero on failure.
    #[guard(ERROR)]
    pub unsafe fn agg_miss_at(ctx: *mut Ctx, st: *mut u8, img: *const u8) -> i64 {
        let st = st as *mut AggState;
        let layout = &*(*st).layout.ptr();
        let src = Rec::at(img as *mut u8, layout);
        match locate(ctx, st, src.keys()) {
            Ok(p) => {
                if let Err(o) = Rec::at(p, layout).fold_newer(&src) {
                    return o_status(o);
                }
                let t = (*st).t.ptr();
                let (base, end) = ((*st).table.ptr() as usize, (*st).table.ptr() as usize + (*t).cap as usize * layout.size() as usize);
                if (*t).flags & T1_BYPASS == 0 && (base..end).contains(&(p as usize)) { p as i64 } else { 0 }
            }
            Err(e) => e,
        }
    }
}

crate::contract::host! {
    /// The table drained into the partition buffers (a full pool spilled): a serial producer's end, or the section end through `agg_open`.
    #[guard(ERROR)]
    pub unsafe fn agg_finish(ctx: *mut Ctx, st: *mut u8) -> i64 {
        let st = st as *mut AggState;
        while !T1::drain((*st).t.ptr()) {
            if let Err(e) = agg_flush(ctx, st) {
                return e;
            }
        }
        0
    }
}

crate::contract::host! {
    /// The seeded second fold: the records of consumed region `k` (the marking probe pipe's fold in its Materialize slot — the leader
    /// fold's plain `HashAgg` table, a serial worker's T1 with its partition buffers, or the owner phase's regions) folded into this call's table
    /// by key (`locate`, then `Rec::fold`: the merge's own transitions, one probe per group of the seed and never a read of its rows); with `plain`
    /// set `st` IS the plain fold's one record and every seed record folds into it.
    #[guard(ERROR)]
    pub unsafe fn agg_absorb(ctx: *mut Ctx, st: *mut u8, layout: *const RecordLayout, k: u64, plain: u64) -> i64 {
        let src = consumed(ctx, k as usize);
        if src.is_null() {
            return 0;
        }
        let layout = &*layout;
        let rec = layout.size() as usize;
        let fold = |r: Rec<'_>| -> Result<(), i64> {
            let dst = if plain != 0 { st } else { locate(ctx, st as *mut AggState, r.keys())? };
            Rec::at(dst, layout).fold(&r).map_err(o_status)
        };
        let mut r = Ok(());
        match table_kind(src) {
            TABLE_T1 => {
                let t = T1::view(src);
                for x in t.records() {
                    r = r.and_then(|_| fold(x));
                }
                for part in 0..t.parts() as u32 {
                    for b in t.chain(part) {
                        for i in 0..b.recs {
                            r = r.and_then(|_| fold(Rec::at(b.ptr.add(i * rec) as *mut u8, layout)));
                        }
                    }
                }
            }
            TABLE_OWNED => {
                // the owners' run records (`agg_gather`'s runs-form walk): per owner the (offset, count) directory of its groups
                let o = Owned::view(src);
                for j in 0..o.nowners as usize {
                    let h = OwnerTop::view(o.owner(j));
                    let dir = o.owner(j).add(OWNERTOP_HDR) as *const u32;
                    for g in 0..h.worst as usize {
                        let (off, m) = (*dir.add(2 * g) as usize, *dir.add(2 * g + 1) as usize);
                        for i in 0..m {
                            r = r.and_then(|_| fold(Rec::at(o.owner(j).add(off + i * rec), layout)));
                        }
                    }
                }
            }
            _ => {
                for x in HashAgg::view(src).records() {
                    r = r.and_then(|_| fold(x));
                }
            }
        }
        match r {
            Ok(()) => 0,
            Err(e) => e,
        }
    }
}

// ---- D4620: a cascaded grouping set's fold of its parent's owned records (lowerv4 ops/agg/cascade.rs) ----

/// `agg_cascade`'s descriptor (lowerv4 lays it on the stack): the header words, then `2 * nparents` words (each candidate
/// parent's consumed region and the tag word its records of the parent set carry), the key words' AND masks and OR masks
/// (`nkeys` each: the tag word's set to the set's own tag, a NULL key's image words to 0 and its flag set), then `nreps`
/// slot indexes (a NULL key's representative, cleared).
pub const CASCADE_NPARENTS: usize = 0;
pub const CASCADE_TAG_WORD: usize = 1;
pub const CASCADE_NREPS: usize = 2;
pub const CASCADE_HDR: usize = 3;

/// The descriptor's words for `nparents` parents, `nkeys` key words and `nreps` representatives.
pub const fn cascade_desc_words(nparents: usize, nkeys: usize, nreps: usize) -> usize {
    CASCADE_HDR + 2 * nparents + 2 * nkeys + nreps
}

crate::contract::host! {
    /// D4620: the running claim's share of a cascaded grouping set's input: of the candidate parents (`desc`'s consumed
    /// regions, each a finished owner region of the base group's record layout), the one holding the fewest records (all of
    /// them are sealed before this pipeline runs, so every claim reads the same one); of its records the claim's slices
    /// take (`claim_records`: cut by record, so a parent folded on one thread still splits), each record of the parent's
    /// set (its tag word) copied, its keys rewritten under the masks (the set's tag, the columns it does not group NULL,
    /// their representatives cleared) and folded into this claim's table by key
    /// (`locate`, then `Rec::fold`: the owner phase's merge, so a group's partial records meet as its owners' do). 0, or a
    /// status below zero (a float merge's overflow, a failed flush). A parent no owner phase laid (a width-1 producer its
    /// caller never finished: the server runner's `finish_single` owns every one, a crate rig does not) is the server's
    /// section's: `UNSUPPORTED_SCRATCH`, as the base's emit answers for its own region (`gather_in_x`), never a bare error.
    #[guard(ERROR)]
    pub unsafe fn agg_cascade(ctx: *mut Ctx, st: *mut u8, desc: *const u64) -> i64 {
        let st = st as *mut AggState;
        let layout = &*(*st).layout.ptr();
        let (np, tw, nreps) = (*desc.add(CASCADE_NPARENTS) as usize, *desc.add(CASCADE_TAG_WORD) as usize, *desc.add(CASCADE_NREPS) as usize);
        let nk = layout.nkeys();
        let parents = core::slice::from_raw_parts(desc.add(CASCADE_HDR), 2 * np);
        let and = core::slice::from_raw_parts(desc.add(CASCADE_HDR + 2 * np), nk);
        let or = core::slice::from_raw_parts(desc.add(CASCADE_HDR + 2 * np + nk), nk);
        let reps = core::slice::from_raw_parts(desc.add(CASCADE_HDR + 2 * np + 2 * nk), nreps);
        if tw >= nk {
            return bare("agg_cascade: a tag word past the record's keys");
        }
        let mut best: Option<(&Owned, u64, usize)> = None;
        for p in parents.chunks_exact(2) {
            let r = consumed(ctx, p[0] as usize);
            if r.is_null() || table_kind(r) != TABLE_OWNED || Owned::view(r).batched() {
                return unsupported(UNSUPPORTED_SCRATCH);
            }
            let o = Owned::view(r);
            let n = crate::table::emit::run_records(o);
            if best.is_none_or(|b| n < b.2) {
                best = Some((o, p[1], n));
            }
        }
        let Some((o, ptag, _)) = best else { return 0 };
        let size = layout.size() as usize;
        let mut stack = [0u64; 64];
        let buf = if size <= 8 * stack.len() { stack.as_mut_ptr() as *mut u8 } else { crate::util::scratch(ctx, size) };
        if buf.is_null() {
            return unsupported(UNSUPPORTED_SCRATCH);
        }
        let mut r = Ok(());
        crate::table::emit::claim_records(ctx, o, size, &mut |base, off, m| {
            for i in 0..m {
                if r.is_err() {
                    return;
                }
                let src = base.add(off + i * size);
                if *(src as *const u64).add(tw) != ptag {
                    continue;
                }
                core::ptr::copy_nonoverlapping(src, buf, size);
                let w = buf as *mut u64;
                for k in 0..nk {
                    *w.add(k) = (*w.add(k) & and[k]) | or[k];
                }
                for &j in reps {
                    let j = j as usize;
                    *(buf.add(layout.slot_off(j) as usize) as *mut u64) = if layout.mincode(j) { gravity_abi::breaker::MINCODE_NONE } else { 0 };
                    if layout.has_validity() {
                        let (b, sh) = layout.valid_bit(j);
                        *buf.add(b) &= !(1u8 << sh);
                    }
                }
                let rec = Rec::at(buf, layout);
                r = locate(ctx, st, rec.keys()).and_then(|d| Rec::at(d, layout).fold(&rec).map_err(o_status));
            }
        });
        match r {
            Ok(()) => 0,
            Err(e) => e,
        }
    }
}

// ---- the two-pass first pass (twopass.rs h_open / h_floor / h_close; `agg_block` is its block) and the second pass's admitted set (group.rs h_admit) ----

/// The first pass's claim state: the bare T1's `AggState` (the appender's words the IR reads), the floor's bound, the IR's append
/// count since the last `agg_block` (`app`: it lived past the struct before GRANT-TRIM and aliased the floor's first key — a flush's
/// record count went wild and read as a bare `ERROR` under a trimmed grant), and the floor's `n` words in the claim's arena (`floor`).
#[repr(C)]
pub struct Pass1State {
    pub agg: AggState,
    pub bound: i64,
    pub floor_n: u64,
    pub floor_len: u64,
    pub floor_worst: u64,
    pub app: u64,
    /// The top's tie order on the key is descending: the floor keeps the claim's `n` largest keys, the bound is their least.
    pub desc: u64,
    /// The key's type orders its words unsigned (typesem `word_order`: bool, "char", oid, xid8, pg_lsn), else signed.
    pub unsigned: u64,
    /// D3157: the statement's shared floor (`PASS1_SHARED_*` words, the `twopass.floor` protocol's), 0 when there is none.
    pub shared: u64,
    /// The floor's `floor_n` words, drawn from the claim's arena at `pass1_open` (the Call's scratch, rewound at the next
    /// claim's Call). D4441: they were the state slot's tail, a stack slot of `8 n` bytes in the pass-1 function's frame: a
    /// top of 200,000 laid a 1.6 MB frame the A64 backend refused ('invalid A64 reload frame slot'), and n = 1,000,000 an
    /// 8 MB one on the thread's stack.
    pub floor: u64,
}

/// `pass1_open`'s order word: bit 0 the tie order is descending, bit 1 the key's words order unsigned.
pub const PASS1_DESC: u64 = 1;
pub const PASS1_UNSIGNED: u64 = 2;

/// D3157: the shared floor's words (exec ops/sort/topn/proto.rs `TwoPassFloor` lays them zeroed per execution): a lock word
/// (0 free), the keys it holds, their `n`-th best once it holds `n` (the bound any claim may offer under), then its keys
/// in the top's tie order, best first, and `n` words a merge writes before it copies them back.
pub const PASS1_SHARED_LOCK: usize = 0;
pub const PASS1_SHARED_LEN: usize = 1;
pub const PASS1_SHARED_BOUND: usize = 2;
pub const PASS1_SHARED_KEYS: usize = 3;

/// The shared floor's words for a top of `n`.
pub const fn pass1_shared_words(n: u64) -> usize {
    PASS1_SHARED_KEYS + 2 * n as usize
}

pub const PASS1_STATE_BYTES: u32 = size_of::<Pass1State>() as u32;
pub const PASS1_BOUND: i32 = core::mem::offset_of!(Pass1State, bound) as i32;
pub const PASS1_APP: i32 = core::mem::offset_of!(Pass1State, app) as i32;
static BARE_LAYOUT: RecordLayout = crate::ops::sort::topn::t3::BARE;

crate::contract::host! {
    /// The claim's open: the bare table (`t3::BARE`, one key word) in the append mode at the declared bits, the floor empty; the IR's constants in the state
    /// (`order`: `PASS1_DESC` | `PASS1_UNSIGNED`; `shared`: the statement's shared floor or 0), the bound the shared floor's.
    #[guard(ERROR)]
    pub unsafe fn pass1_open(ctx: *mut Ctx, st: *mut u8, bits: u64, n: u64, order: u64, shared: u64) -> i64 {
        let st = st as *mut Pass1State;
        let rec = BARE_LAYOUT.size() as u64;
        // a region that started small is laid out at its reservation, its pool grown in place before it spills
        let own = partial(ctx);
        let (reserve, floor, laid, cut) = if (*(own as *const T1)).cap == 0 { gravity_abi::program::region_reserve_laid(ctx) } else { (0, false, false, 0) };
        // D4441: the floor's words from the claim's arena, before the bare table takes its region
        let floor_keys = if n == 0 { core::ptr::null_mut() } else { gravity_abi::ctx::arena_alloc(ctx, 8 * n as usize) };
        if n > 0 && floor_keys.is_null() {
            return unsupported(UNSUPPORTED_SCRATCH);
        }
        let pbits = fold_bits(ctx, bits, false, 0, rec, open_cut(reserve, laid, cut));
        let t = if laid { T1::at_laid(own, &BARE_LAYOUT, pbits, t1_default_bytes(), t1_default_bytes(), SHARE, reserve, true) } else { T1::at(own, &BARE_LAYOUT, pbits, t1_default_bytes(), SHARE) } as *mut T1;
        // D3187: never compacted. The bare table counts by observation (a key's records are its sightings, the owner lists
        // a key seen twice); D2587's in-memory compaction folds a chain's same-key records into one, so a key in two runs
        // or a floor key read once more lost its second sighting and was never admitted. A full pool spills instead, past
        // the pool a started-small region grows in place (`T1_GROWS`: moved records, never folded).
        (*t).flags |= T1_BYPASS | gravity_abi::breaker::T1_NOFOLD;
        if laid && floor && (*t).flags & gravity_abi::breaker::T1_RESERVED != 0 {
            (*t).flags |= gravity_abi::breaker::T1_FLOOR | gravity_abi::breaker::T1_GROWS;
        }
        let base = t as *mut u8;
        (*st).agg = AggState { t: word(t), heads: word(base.add(T1_HDR) as *mut u64), table: word(base.add((*t).table_off as usize)), mask: (*t).cap as u64 - 1, thresh: rec << HEAD_LEFT, delta: rec.wrapping_sub(rec << HEAD_LEFT), pmask: (1u64 << pbits) - 1, pbits: pbits as u64, layout: word(&BARE_LAYOUT as *const RecordLayout), arena_cur: word(T1::img_words(t) as *mut u8), arena_end: word(t as *mut u8), arena_chunk: IMG_BY_REF };
        (*st).desc = (order & PASS1_DESC != 0) as u64;
        (*st).unsigned = (order & PASS1_UNSIGNED != 0) as u64;
        (*st).floor_n = n;
        (*st).floor_len = 0;
        (*st).floor_worst = 0;
        (*st).app = 0;
        (*st).shared = shared;
        (*st).floor = floor_keys as u64;
        (*st).bound = tighter(st, empty_bound(st));
        0
    }
}

/// The shared floor's word `i`. # Safety — `(*st).shared` is the statement's shared floor (not 0) of `floor_n` keys.
unsafe fn shared_word<'a>(st: *mut Pass1State, i: usize) -> &'a core::sync::atomic::AtomicU64 {
    &*((*st).shared as *const core::sync::atomic::AtomicU64).add(i)
}

/// D3157: the shared floor's bound once it holds `n` keys: `n` distinct keys of the statement rank at or before it, so a
/// key of one row that ranks after it is never among the top's `n` groups (its run need not be offered, its floor key
/// not appended again). `None` without a shared floor or before it fills.
unsafe fn shared_bound(st: *mut Pass1State) -> Option<i64> {
    use core::sync::atomic::Ordering::Acquire;
    if (*st).shared == 0 || shared_word(st, PASS1_SHARED_LEN).load(Acquire) != (*st).floor_n {
        return None;
    }
    Some(shared_word(st, PASS1_SHARED_BOUND).load(Acquire) as i64)
}

/// `own` (the claim's bound) or the shared floor's, whichever ranks first: the claim offers fewer keys under it.
unsafe fn tighter(st: *mut Pass1State, own: i64) -> i64 {
    match shared_bound(st) {
        Some(b) if worse(st, own, b) => b,
        _ => own,
    }
}

/// D3157: the claim's floor merged into the shared floor under its lock: the shared floor becomes the `n` best distinct
/// keys of both, and the claim's floor keeps (best first) only its keys that entered, the ones the claim appends once
/// more; their count. A key that did not enter ranks after `n` distinct keys of the statement, or is a shared key already
/// (another claim saw it: two sightings). Without a shared floor the claim keeps its whole floor (each claim appends its
/// own `n`, as before D3157).
unsafe fn shared_merge(st: *mut Pass1State) -> usize {
    use core::sync::atomic::Ordering::{Acquire, Relaxed, Release};
    let (n, len) = ((*st).floor_n as usize, (*st).floor_len as usize);
    if (*st).shared == 0 || len == 0 {
        return len;
    }
    let f = floor_of(st);
    // the claim's best key does not rank before the shared bound: none of its keys enters, no lock taken
    if shared_bound(st).is_some_and(|b| !worse(st, b, f[0])) {
        return 0;
    }
    let lock = shared_word(st, PASS1_SHARED_LOCK);
    while lock.compare_exchange_weak(0, 1, Acquire, Relaxed).is_err() {
        core::hint::spin_loop();
    }
    let slen = shared_word(st, PASS1_SHARED_LEN).load(Relaxed) as usize;
    let key = |i: usize| shared_word(st, PASS1_SHARED_KEYS + i).load(Relaxed) as i64;
    let out = |o: usize, k: i64| shared_word(st, PASS1_SHARED_KEYS + n + o).store(k as u64, Relaxed);
    let (mut i, mut j, mut o, mut kept) = (0, 0, 0, 0);
    while o < n && (i < slen || j < len) {
        // the claim's key next when the shared one ranks after it (or the shared ones ran out); an equal key once, as shared
        if j < len && (i >= slen || worse(st, key(i), f[j])) {
            let k = f[j];
            f[kept] = k; // kept <= j: the keys not yet read stay where they are
            out(o, k);
            kept += 1;
            j += 1;
        } else {
            if j < len && key(i) == f[j] {
                j += 1;
            }
            out(o, key(i));
            i += 1;
        }
        o += 1;
    }
    if kept > 0 {
        for x in 0..o {
            shared_word(st, PASS1_SHARED_KEYS + x).store(shared_word(st, PASS1_SHARED_KEYS + n + x).load(Relaxed), Relaxed);
        }
        if o == n {
            shared_word(st, PASS1_SHARED_BOUND).store(key(n - 1) as u64, Release);
        }
        shared_word(st, PASS1_SHARED_LEN).store(o as u64, Release);
    }
    lock.store(0, Release);
    kept
}

unsafe fn floor_of<'a>(st: *mut Pass1State) -> &'a mut [i64] {
    core::slice::from_raw_parts_mut((*st).floor as *mut i64, (*st).floor_len as usize)
}

/// The bound while the floor is not full: every key is at or under it (the IR offers `k <= bound`, `k >= bound` descending,
/// compared as the key's words order: signed, or unsigned).
unsafe fn empty_bound(st: *mut Pass1State) -> i64 {
    match ((*st).desc != 0, (*st).unsigned != 0) {
        (false, false) => i64::MAX,
        (true, false) => i64::MIN,
        (false, true) => u64::MAX as i64,
        (true, true) => 0,
    }
}

/// `a` ranks after `b` in the top's tie order on the key (its words compared as its type orders them).
unsafe fn worse(st: *mut Pass1State, a: i64, b: i64) -> bool {
    let after = if (*st).unsigned != 0 { (a as u64) > (b as u64) } else { a > b };
    let before = if (*st).unsigned != 0 { (a as u64) < (b as u64) } else { a < b };
    if (*st).desc != 0 { before } else { after }
}

crate::contract::host! {
    /// `t3::Floor::offer` for a key the IR found at or under the bound: into the set (the worst replaced once it is full), the bound back into the state.
    /// The floor is kept in the top's tie order, best first (`floor_worst` is its last place), so a key is found and placed by
    /// binary search and the worst drops off the end: an offer costs O(log n) compares and one shift, where a scan for the key
    /// and a rescan for the new worst made pass 1 O(n) per offer (gtopn TOPN-N: a grouped top-N's pass 1 11 ms at n =
    /// 10, 681 ms at n = 1000, the same rows).
    pub unsafe fn pass1_floor(_ctx: *mut Ctx, st: *mut u8, k: u64) -> i64 {
        let st = st as *mut Pass1State;
        let k = k as i64;
        let (len, n) = ((*st).floor_len as usize, (*st).floor_n as usize);
        // SAFETY: `floor` is the claim's `floor_n` words (`pass1_open` drew them from the arena)
        let floor = core::slice::from_raw_parts_mut((*st).floor as *mut i64, n);
        // the keys that rank before `k` (the floor is in the tie order, its keys distinct)
        let at = floor[..len].partition_point(|&e| worse(st, k, e));
        if (at >= len || floor[at] != k) && at < n {
            let end = if len < n { len } else { n - 1 };
            floor.copy_within(at..end, at + 1);
            floor[at] = k;
            if len < n {
                (*st).floor_len += 1;
            }
            (*st).floor_worst = (*st).floor_len - 1;
        }
        // D3157: or the shared floor's bound, when it ranks first (`tighter`)
        (*st).bound = tighter(st, if (*st).floor_len < (*st).floor_n { empty_bound(st) } else { floor[n - 1] });
        0
    }
}

crate::contract::host! {
    /// The claim's end: the floor's keys appended once more (the dup rule exports them), the table drained. D3157: with
    /// a shared floor, only the keys that entered it (`shared_merge`): every claim appending its own `n` made pass 2's
    /// admitted set about `n` x claims keys (51,104 for a top of 100 over 511 claims, 5,114 for 10), its probe the cost.
    #[guard(ERROR)]
    pub unsafe fn pass1_close(ctx: *mut Ctx, st: *mut u8) -> i64 {
        let st = st as *mut Pass1State;
        let agg = &mut (*st).agg as *mut AggState;
        let t = (*agg).t.ptr();
        let pbits = (*agg).pbits as u8;
        let floor = (*st).floor as *mut i64;
        let mut ap = T1::appender(t);
        for i in 0..shared_merge(st) {
            let k = *floor.add(i) as u64;
            let part = KeyHash::of(&[k]).part(pbits as u32);
            let mut w = ap.reserve(part);
            if w.is_none() {
                ap.finish();
                if let Err(e) = agg_flush(ctx, agg) {
                    return e;
                }
                ap = T1::appender(t);
                w = ap.reserve(part);
            }
            match w {
                Some(w) => *(w as *mut u64) = k,
                None => return unsupported(UNSUPPORTED_SCRATCH),
            }
        }
        ap.finish();
        (*st).floor_len = 0;
        (*st).floor_worst = 0;
        (*st).bound = empty_bound(st);
        while !T1::drain(t) {
            if let Err(e) = agg_flush(ctx, agg) {
                return e;
            }
        }
        0
    }
}

#[cfg(test)]
mod d1392_pin_refused_tests {
    use super::*;

    /// D1392 (b): a group's text image the pool refuses (`Program.pin` null: this ctx's program binds none) is PostgreSQL's
    /// "out of memory" (ERRCODE_OUT_OF_MEMORY, MAKE_SQLSTATE('5','3','2','0','0') = 8389), never the message-less HOST raise.
    #[test]
    fn a_refused_image_pin_names_out_of_memory() {
        let mut program = Box::new(gravity_abi::ProgramBlock::NONE); // alloc: Cold
        let mut ctx = vec![0u64; gravity_abi::ctx::ctx_size(1) / 8]; // alloc: Cold
        // SAFETY: the words are a ctx image; `program` outlives every use.
        unsafe { (*(ctx.as_mut_ptr() as *mut Ctx)).program = &mut *program };
        drop(gravity_fn::take_error());
        let text = b"MAIL";
        // SAFETY: a bound ctx; `text` 4 readable bytes.
        let r = unsafe { agg_image_pin(ctx.as_mut_ptr() as *mut Ctx, text.as_ptr(), text.len() as u64) };
        assert_eq!(r, 0, "no room: the image host's failure");
        let e = gravity_fn::take_error().expect("a refused pin stashes its error for the HOST raise");
        assert_eq!((e.message(), e.sqlstate().0), ("out of memory", 8389));
        assert_eq!(e.detail(), Some("Failed on request of size 8 in a Gravity pinned region (the statement's pinned pool is full)."));
    }
}

#[cfg(test)]
mod pass1_floor_order_tests {
    use super::*;

    /// The floor of `n` keys after offering `keys` (each at or under the bound, as the IR offers), under `order`
    /// (`PASS1_DESC` | `PASS1_UNSIGNED`): its keys sorted, and the bound.
    fn floor(order: u64, n: usize, keys: &[u64]) -> (Vec<u64>, i64) {
        let mut buf = vec![0u64; size_of::<Pass1State>() / 8 + n]; // alloc: Cold
        let st = buf.as_mut_ptr() as *mut Pass1State;
        // SAFETY: `buf` holds the state and its `n` floor words (the claim arena's, as `pass1_open` draws them); pass1_floor
        // reads no ctx.
        unsafe {
            (*st).floor = buf.as_mut_ptr().add(size_of::<Pass1State>() / 8) as u64;
            (*st).desc = (order & PASS1_DESC != 0) as u64;
            (*st).unsigned = (order & PASS1_UNSIGNED != 0) as u64;
            (*st).floor_n = n as u64;
            (*st).bound = empty_bound(st);
            for &k in keys {
                let b = (*st).bound;
                let under = match ((*st).desc != 0, (*st).unsigned != 0) {
                    (false, false) => k as i64 <= b,
                    (true, false) => k as i64 >= b,
                    (false, true) => k <= b as u64,
                    (true, true) => k >= b as u64,
                };
                if under {
                    pass1_floor(core::ptr::null_mut(), st as *mut u8, k);
                }
            }
            let mut f: Vec<u64> = floor_of(st).iter().map(|&k| k as u64).collect(); // alloc: Cold
            f.sort_unstable();
            (f, (*st).bound)
        }
    }

    /// B3-15: the floor keeps the claim's `n` first keys in the key's own word order, signed or unsigned, either direction
    /// (an oid or xid8 past 2^63 / 2^31 sign-extended is a large unsigned word, never a small signed one).
    #[test]
    fn the_floor_orders_words_as_the_key_type_does() {
        let keys = [5u64, u64::MAX - 1, 3, 1u64 << 63, 1, 7];
        assert_eq!(floor(0, 2, &keys), (vec![1u64 << 63, u64::MAX - 1], -2)); // alloc: Cold
        assert_eq!(floor(PASS1_DESC, 2, &keys), (vec![5, 7], 5)); // alloc: Cold
        assert_eq!(floor(PASS1_UNSIGNED, 2, &keys), (vec![1, 3], 3)); // alloc: Cold
        assert_eq!(floor(PASS1_UNSIGNED | PASS1_DESC, 2, &keys), (vec![1u64 << 63, u64::MAX - 1], (1u64 << 63) as i64)); // alloc: Cold
        // not full: the bound admits every key
        assert_eq!(floor(PASS1_UNSIGNED, 8, &[9, 2]).1, u64::MAX as i64);
        assert_eq!(floor(PASS1_UNSIGNED | PASS1_DESC, 8, &[9, 2]).1, 0);
    }

    /// TOPN-N: the floor kept in order by binary search holds the `n` best distinct keys of any offer sequence (repeats
    /// offered again across runs, keys equal to the bound, every order), and its bound is the n-th best.
    #[test]
    fn the_floor_keeps_the_n_best_distinct_keys() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let keys: Vec<u64> = (0..5000).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x % 900 }).collect(); // alloc: Cold
        for (order, n) in [(0, 1), (0, 37), (PASS1_DESC, 37), (PASS1_UNSIGNED, 64), (PASS1_DESC, 899), (0, 1000)] {
            let mut want: Vec<u64> = keys.clone(); // alloc: Cold
            want.sort_unstable();
            want.dedup();
            if order & PASS1_DESC != 0 {
                want.reverse();
            }
            want.truncate(n);
            let bound = if want.len() == n { *want.last().unwrap() as i64 } else { floor(order, n, &[]).1 };
            want.sort_unstable();
            assert_eq!(floor(order, n, &keys), (want, bound), "order {order} n {n}");
        }
    }
}

#[cfg(test)]
mod d3157_shared_floor_tests {
    use super::*;

    /// One claim of the bare pass 1 over `rows` (a run per row) beside the shared floor `shared` (0: none), as the IR and
    /// the hosts run it: a row under the bound is offered to the claim's floor, the claim's end merges it (`shared_merge`).
    /// Each row's key is sighted once, and each floor key the claim keeps once more: `sighted` counts them.
    fn claim(order: u64, n: usize, rows: &[u64], shared: u64, sighted: &mut std::collections::BTreeMap<u64, u32>) -> usize {
        let mut buf = vec![0u64; size_of::<Pass1State>() / 8 + n]; // alloc: Cold
        let st = buf.as_mut_ptr() as *mut Pass1State;
        // SAFETY: `buf` holds the state and its `n` floor words (the claim arena's, as `pass1_open` draws them); `shared` is
        // a zeroed shared floor of `n` keys or 0; the floor hosts read no ctx.
        unsafe {
            (*st).floor = buf.as_mut_ptr().add(size_of::<Pass1State>() / 8) as u64;
            (*st).desc = (order & PASS1_DESC != 0) as u64;
            (*st).unsigned = (order & PASS1_UNSIGNED != 0) as u64;
            (*st).floor_n = n as u64;
            (*st).shared = shared;
            (*st).bound = tighter(st, empty_bound(st));
            for &k in rows {
                *sighted.entry(k).or_default() += 1;
                let b = (*st).bound;
                let under = match ((*st).desc != 0, (*st).unsigned != 0) {
                    (false, false) => k as i64 <= b,
                    (true, false) => k as i64 >= b,
                    (false, true) => k <= b as u64,
                    (true, true) => k >= b as u64,
                };
                if under {
                    pass1_floor(core::ptr::null_mut(), st as *mut u8, k);
                }
            }
            let kept = shared_merge(st);
            for &k in &floor_of(st)[..kept] {
                *sighted.entry(k as u64).or_default() += 1;
            }
            kept
        }
    }

    /// The top's `n` groups: count descending, then the key in the tie order (signed or unsigned, ascending or descending).
    fn top(order: u64, n: usize, counts: &std::collections::BTreeMap<u64, u32>) -> Vec<u64> {
        let rank = |k: u64| if order & PASS1_UNSIGNED != 0 { k as i128 } else { k as i64 as i128 };
        let mut all: Vec<(u32, u64)> = counts.iter().map(|(&k, &c)| (c, k)).collect(); // alloc: Cold
        all.sort_by(|a, b| b.0.cmp(&a.0).then(if order & PASS1_DESC != 0 { rank(b.1).cmp(&rank(a.1)) } else { rank(a.1).cmp(&rank(b.1)) }));
        all.into_iter().take(n).map(|(_, k)| k).collect() // alloc: Cold
    }

    /// D3157: claims beside one shared floor admit (a key sighted twice) every one of the top's `n` groups, as each claim's
    /// own floor did, for every order and every mix of one-row and repeated keys; and they append far fewer floor keys
    /// once the claims are many (the own floors append `n` a claim).
    #[test]
    fn a_shared_floor_admits_the_top_and_appends_few_keys() {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for (order, n, domain) in [(0, 1, 1u64 << 40), (0, 10, 1 << 40), (PASS1_DESC, 10, 1 << 40), (PASS1_UNSIGNED, 37, u64::MAX), (PASS1_UNSIGNED | PASS1_DESC, 37, u64::MAX), (0, 100, 5000), (PASS1_DESC, 7, 300)] {
            let claims: Vec<Vec<u64>> = (0..200).map(|_| (0..400).map(|_| rnd() % domain).map(|k| if order & PASS1_UNSIGNED != 0 { k } else { k.wrapping_sub(domain / 2) }).collect()).collect(); // alloc: Cold
            let mut words = vec![0u64; pass1_shared_words(n as u64)]; // alloc: Cold
            let (mut own, mut shared) = (std::collections::BTreeMap::new(), std::collections::BTreeMap::new());
            let (mut own_kept, mut shared_kept) = (0, 0);
            for c in &claims {
                own_kept += claim(order, n, c, 0, &mut own);
                shared_kept += claim(order, n, c, words.as_mut_ptr() as u64, &mut shared);
            }
            let rows: std::collections::BTreeMap<u64, u32> = claims.iter().flatten().fold(Default::default(), |mut m, &k| {
                *m.entry(k).or_default() += 1;
                m
            });
            for (name, sighted) in [("own", &own), ("shared", &shared)] {
                for k in top(order, n, &rows) {
                    assert!(sighted.get(&k).copied().unwrap_or(0) >= 2, "{name} floors: top key {k} not admitted (order {order} n {n})");
                }
            }
            assert_eq!(own_kept, claims.len() * n, "the own floors append n a claim");
            assert!(shared_kept * 3 < own_kept, "order {order} n {n}: the shared floor appended {shared_kept} of {own_kept}");
            // the shared floor holds the statement's n best distinct keys
            let mut best: Vec<u64> = rows.keys().copied().collect(); // alloc: Cold
            let rank = |k: &u64| if order & PASS1_UNSIGNED != 0 { *k as i128 } else { *k as i64 as i128 };
            best.sort_by_key(rank);
            if order & PASS1_DESC != 0 {
                best.reverse();
            }
            best.truncate(n);
            assert_eq!(&words[PASS1_SHARED_KEYS..PASS1_SHARED_KEYS + n], &best[..], "order {order} n {n}");
            assert_eq!((words[PASS1_SHARED_LOCK], words[PASS1_SHARED_LEN], words[PASS1_SHARED_BOUND]), (0, n as u64, best[n - 1]));
        }
    }
}

#[cfg(test)]
mod laid_cut_tests {
    use super::*;

    /// D3809: a started-small region is cut by its share exactly as a share-committed region of that share is (one rule,
    /// `cut_bits`), never by its reservation: at a 13.5 MB worker share (a grouping query at W16) the reservation's 2
    /// GB cut opened 8x the partitions. A share past the answer's 4 GiB saturation opens what the unsaturated one does.
    #[test]
    fn a_laid_region_is_cut_by_its_share() {
        let reserve = 2_056_486_912u64;
        for (width, groups, rec) in [(16u16, 0u64, 48u64), (4, 600_000, 48), (1, 5_000_000, 64)] {
            for share in [13_500_416u64, 64 << 20, 6 << 30] {
                let carried = gravity_abi::program::reserve_answer(gravity_abi::program::reserve_share_word(share)).3;
                let laid = cut_bits(11, laid_cut(reserve, carried), width, false, groups, rec);
                assert_eq!(laid, cut_bits(11, share, width, false, groups, rec), "width {width} share {share}: the committed road's bits");
            }
            // no share carried: the reservation, as before D3809
            assert_eq!(cut_bits(11, laid_cut(reserve, 0), width, false, groups, rec), cut_bits(11, reserve, width, false, groups, rec));
        }
        let (share, wide) = (13_500_416u64, 16u16);
        assert!(cut_bits(11, share, wide, false, 0, 48) < cut_bits(11, reserve, wide, false, 0, 48), "the share opens fewer partitions than the reservation");
    }

    /// D4033: a worker share the pool's room opened smaller (not laid) is cut by its section's share, which the driver's
    /// answer carries, as every other worker of the section is by its limit; nothing carried, the limit as before. Cut by
    /// its own smaller limit it opened fewer partitions (a W8 group-by under a starved pool: six partials at 1,024 and two
    /// at 128 and 64), and the owner phase, reading every partial at the most, read past their head words. Below the wide
    /// width a smaller limit can open more instead (each partition's groups must fit it): either way, not the section's.
    #[test]
    fn a_share_opened_smaller_is_cut_by_its_sections_share() {
        let share = 56_784_384u64;
        let carried = gravity_abi::program::reserve_answer(gravity_abi::program::reserve_share_word(share)).3;
        for (width, groups, rec, bits) in [(8u16, 0u64, 48u64, 10u64), (16, 0, 32, 11), (4, 600_000, 48, 11), (2, 5_000_000, 64, 9)] {
            let section = cut_bits(bits, share, width, false, groups, rec);
            assert_eq!(cut_bits(bits, open_cut(0, false, carried), width, false, groups, rec), section, "width {width}: the section's bits");
        }
        assert_eq!(open_cut(0, false, 0), 0, "no share carried: the table's limit");
        assert_eq!(open_cut(1 << 30, true, share), laid_cut(1 << 30, share), "a laid region: D3809's cut");
        for smaller in [7_094_272u64, 1_773_568] {
            assert!(cut_bits(10, smaller, 8, false, 0, 48) < cut_bits(10, share, 8, false, 0, 48), "cut by its own limit {smaller}, it opened fewer");
        }
    }
}
