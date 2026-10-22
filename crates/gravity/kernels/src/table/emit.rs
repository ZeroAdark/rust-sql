use gravity_abi::{word, PtrWord, Word};
use crate::host_abi::*;
use gravity_abi::limits::DESC_MAX_SORT;

// ---- the group emit's entries (group.rs h_gather / h_sort) over the emit descriptor ----

/// The emit descriptor's words: the layout (`DESC_LAYOUT_WORDS`), the form word (low half `FORM_*`, high half the NULL-flags key index + 1), the sort key count, the sort key words (`SK_*`).
pub const DESC_LAYOUT: usize = 0;
pub const DESC_FORM: usize = 8 * LAYOUT_WORDS;
pub const DESC_NSORT: usize = DESC_FORM + 8;
pub const DESC_SORT: usize = DESC_NSORT + 8;
/// M6-HOSTED (D1165): one word per sort word after them, its btree support function (`SK_CMP`), 0 for any other word;
/// read only for an `SK_CMP` word.
pub const DESC_PROCS: usize = DESC_SORT + 8 * DESC_MAX_SORT;
pub const DESC_BYTES: u32 = (DESC_PROCS + 8 * DESC_MAX_SORT) as u32;
/// W8-SETOP-WIDEKEYS: the `DESC_NSORT` word's high half, the comparator words' byte offset when they are not at
/// `DESC_PROCS`: a sorted group by more than `DESC_MAX_SORT` keys lays its sort words on past `DESC_PROCS` and its
/// comparator words after them (`DESC_SORT + 8 * nsort`); 0 for every other descriptor (the sort word count alone).
pub const DESC_PROCS_AT_SHIFT: u32 = 32;

/// The descriptor's sort word count (`DESC_NSORT`'s low half).
/// # Safety
/// `desc` is an emit descriptor.
unsafe fn nsort_of(desc: *const u8) -> usize {
    (*(desc.add(DESC_NSORT) as *const u64) & 0xffff_ffff) as usize
}
pub const FORM_NULLKEY_SHIFT: u32 = 32;
pub const FORM_RUNS: u64 = 0;
pub const FORM_COUNT_TOPK: u64 = 1;
/// V4R-190: the gather's form is the form word's low byte; bits `FORM_BOUND_SHIFT..32` carry `n + 1` for a top that takes
/// only the first `n` sorted records (a constant bound, no raising entry between the sort and the top; past a HAVING
/// between them the emit orders the next records with a bound of its own as its drain reaches them), 0 for every record
/// in order. `agg_sort` then orders the first `n` and leaves the rest in any order.
pub const FORM_KIND_MASK: u64 = 0xff;
pub const FORM_BOUND_SHIFT: u32 = 8;
pub const FORM_BOUND_MAX: u64 = (1 << 24) - 2;
/// RUNS-18: with a bound, a top WITH TIES: `agg_sort` orders the first `n`, every record whose sort words equal the `n`-th's
/// and the next record past them (the row its Limit stops at), and returns how many it ordered (the order stays total, so
/// they are the first ones of the one sort of every record). The bit sits above the key-flag word (`FORM_NULLKEY_SHIFT`).
pub const FORM_TIES: u64 = 1 << 63;

/// The key-flag word + 1 the form word carries above `FORM_NULLKEY_SHIFT` (0: none), `FORM_TIES` aside.
const fn nullkey_of(form: u64) -> u64 {
    (form & !FORM_TIES) >> FORM_NULLKEY_SHIFT
}
pub const SK_SLOT: u64 = 1 << 8;
pub const SK_DESC: u64 = 1 << 9;
pub const SK_NULLS_FIRST: u64 = 1 << 10;
/// A text key's sort word (M4-work §B.8): the key's representative slot at `SK_TEXT_SLOT_SHIFT` rendered through the dictionary of lane `SK_TEXT_LANE_SHIFT` of site `SK_TEXT_SITE_SHIFT`, or with `SK_HEAP` the slot's datum (an arena varlena); the collation at `SK_TEXT_COLL_SHIFT` (0 / C: bytes).
pub const SK_TEXT: u64 = 1 << 11;
pub const SK_HEAP: u64 = 1 << 12;
pub const SK_TEXT_SLOT_SHIFT: u32 = 16;
/// D456: a key word's sort word (neither `SK_SLOT` nor `SK_TEXT`) carries its NULL flag's bit + 1 here (the key's index among
/// the NULL-flags words, which a text key's extra words move off the key word's index: bit k % 64 of the flags word k / 64
/// past the form's first, CAP-28); 0 reads bit `w & 0xff`, the key word's own.
pub const SK_NULL_SHIFT: u32 = 16;
/// CAP-26: a key word's sort word (neither `SK_SLOT` nor `SK_TEXT`) carries its key word's index past 255 in the byte at
/// `SK_KEY_HI_SHIFT` (bits 8..15 of the index; the low byte bits 0..7) and its NULL flag's index + 1 past 255 in the byte at
/// `SK_NULL_HI_SHIFT`: the collation's bits, which only a text word reads. So a key word's index runs to 65535 and its
/// key's to 65534, past PostgreSQL's own 1664 target entries ([`sk_key_word`], [`sk_null_index`]).
pub const SK_KEY_HI_SHIFT: u32 = 40;
pub const SK_NULL_HI_SHIFT: u32 = 48;
/// The key word index and the NULL flag index + 1 a key word's sort word can carry (CAP-26).
/// Whether key word `last` (the key's last word) and NULL flag index + 1 `i1` fit a key word sort word: two bytes each,
/// the low byte and the high one at `SK_KEY_HI_SHIFT` / `SK_NULL_HI_SHIFT` (CAP-26).
pub const fn sk_fits(last: usize, i1: usize) -> bool {
    last <= u16::MAX as usize && i1 <= u16::MAX as usize
}

/// Key word sort word `w`'s key word index (CAP-26: the low byte, then `SK_KEY_HI_SHIFT`'s byte above it).
pub const fn sk_key_word(w: u64) -> usize {
    ((w & 0xff) | (w >> SK_KEY_HI_SHIFT & 0xff) << 8) as usize
}

/// Key word sort word `w`'s NULL flag index + 1 (0: the key word's own index; CAP-26: `SK_NULL_SHIFT`'s byte, then
/// `SK_NULL_HI_SHIFT`'s above it).
pub const fn sk_null_index(w: u64) -> usize {
    ((w >> SK_NULL_SHIFT & 0xff) | (w >> SK_NULL_HI_SHIFT & 0xff) << 8) as usize
}

/// The bits of a key word sort word naming key word `word` (one [`sk_fits`]).
pub const fn sk_key(word: usize) -> u64 {
    (word as u64 & 0xff) | (word as u64 >> 8 & 0xff) << SK_KEY_HI_SHIFT
}

/// The bits of a key word sort word naming NULL flag index + 1 `i1` (one [`sk_fits`]).
pub const fn sk_null(i1: usize) -> u64 {
    (i1 as u64 & 0xff) << SK_NULL_SHIFT | (i1 as u64 >> 8 & 0xff) << SK_NULL_HI_SHIFT
}
pub const SK_TEXT_LANE_SHIFT: u32 = 24;
pub const SK_TEXT_SITE_SHIFT: u32 = 32;
pub const SK_TEXT_COLL_SHIFT: u32 = 40;
/// A float sort word (V4R-111): the key word or slot is a float8's bits (a float key's canonical image, a `sum(float4 /
/// float8)` slot; an `F64Accum` slot orders by its average Sx / N), compared by its total-order word (`float_order`).
pub const SK_FLOAT: u64 = 1 << 13;
/// An unsigned key word (V4R-135, D466: pg_lsn / xid8's 8-byte image) compares unsigned: its sign bit flipped before any
/// DESC reversal.
pub const SK_UNSIGNED: u64 = 1 << 14;
/// D767: a bpchar text key (`SK_TEXT`): both images lose their trailing blanks before the compare, under every collation
/// (bpcharcmp: bcTruelen, then varstr_cmp), as `RowCmp`'s `KFLAG_BPCHAR` tails do.
pub const SK_BPCHAR: u64 = 1 << 15;
/// M6-HOSTED NM (D1165): with `SK_HEAP` only, the slot's datum is a `name`'s 64-byte image (NUL-padded, keyed_emit_key's
/// fixed copy), not a varlena. Its compared image is the bytes before the first NUL (namecmp's `strnlen(.., NAMEDATALEN)`),
/// under the word's collation as a text key's: C / 0 by bytes, else the collation's compare (namecmp's varstr_cmp). It
/// lives in the lane byte, which an `SK_HEAP` word never reads (no dictionary renders a heap datum).
pub const SK_NAME: u64 = 1 << SK_TEXT_LANE_SHIFT;
/// M6-HOSTED N1 / N2 / H (D1165): with `SK_HEAP` only, the slot's datum is a kept copy (a 4-byte-header varlena or a
/// fixed by-reference image, as PostgreSQL passes the Datum) ordered by its key's btree support function 1 (the
/// descriptor's `DESC_PROCS` word at this sort word's index) under the word's collation, through fmgr (`keysem::call::order`,
/// rep.hosted.order's). A comparator's ERROR (or a NULL result) is stashed and `agg_sort` returns ERROR: the emit raises it
/// before a row leaves (D1293's route); a 0 datum is NULL.
///
/// D1165 residual (M6-HOSTED): on a key word's sort word (neither `SK_SLOT` nor `SK_TEXT`: its lane byte is unused), the
/// key word is a by-value datum (an enum's oid) ordered by the same comparator word under collation 0 (enum_cmp: an even
/// oid by its value, an odd one by pg_enum.enumsortorder), never by the word itself; its NULL is the key's flag bit.
pub const SK_CMP: u64 = 2 << SK_TEXT_LANE_SHIFT;

/// Whether sort word `w` orders its key word by value through its comparator (`SK_CMP` on a key word, D1165 residual).
pub const fn sk_key_cmp(w: u64) -> bool {
    w & (SK_SLOT | SK_TEXT | SK_CMP) == SK_CMP
}
/// N6a: whether sort word `w` orders its slot's word (a by-value Datum: money, an enum, a date) through its comparator
/// (`SK_SLOT | SK_CMP`, the comparator word at this sort word's index, collation 0); its NULL is the slot's validity.
pub const fn sk_slot_cmp(w: u64) -> bool {
    w & (SK_SLOT | SK_TEXT | SK_CMP) == SK_SLOT | SK_CMP
}
/// NAMEDATALEN: a name's fixed image width.
const NAME_BYTES: usize = 64;

/// The emit's state in owner 0's table space: the record pointers, their count, the kept count.
#[repr(C)]
pub struct EmitState {
    pub ptrs: Word<*mut Word<*const u8>>,
    pub len: u64,
    pub kept: u64,
}

/// `EmitState.ptrs`' byte offset: the lowered emit's loads and stores (lowerv4 table/emit_group.rs, ops/sort/topn/lower.rs).
pub const EMIT_PTRS_OFF: i32 = core::mem::offset_of!(EmitState, ptrs) as i32;

/// `EmitState.kept`'s byte offset (as `EMIT_PTRS_OFF`).
pub const EMIT_KEPT_OFF: i32 = core::mem::offset_of!(EmitState, kept) as i32;

// C10 2b (D3582): the driver files a batched slot's emit state as three words (`breaker::OWNED_EMIT_BYTES`)
const _: () = assert!(size_of::<EmitState>() == gravity_abi::breaker::OWNED_EMIT_BYTES && EMIT_PTRS_OFF == 0 && core::mem::offset_of!(EmitState, len) == 8 && EMIT_KEPT_OFF == 16);

/// The gathered record array as the Rust sorts take it: `EmitState.ptrs` holds 8-byte words (the IR strides them at 8), the
/// identity on a 64-bit host. On wasm32 the words are narrowed in place to 4-byte pointers for the sort (`[0, 4n)`, read
/// forward so no word is overwritten before it is read) and widened back when the guard drops (read backward).
#[cfg_attr(target_pointer_width = "64", allow(dead_code))]
struct Narrow(*mut Word<*const u8>, usize);

impl Narrow {
    unsafe fn of<'a>(p: *mut Word<*const u8>, n: usize) -> (&'a mut [*const u8], Narrow) {
        #[cfg(not(target_pointer_width = "64"))]
        for i in 0..n {
            *(p as *mut *const u8).add(i) = (*p.add(i)).ptr();
        }
        (core::slice::from_raw_parts_mut(p as *mut *const u8, n), Narrow(p, n))
    }
}

impl Drop for Narrow {
    fn drop(&mut self) {
        #[cfg(not(target_pointer_width = "64"))]
        for i in (0..self.1).rev() {
            // SAFETY: `of`'s array, its first `4n` bytes the narrowed pointers.
            unsafe { *self.0.add(i) = word(*(self.0 as *const *const u8).add(i)) };
        }
    }
}

/// D3644 (b): one of a sort's per-record arrays (`len` values with no drop glue): a block of the statement's grant
/// (`program::work_take`: past the pool a mapped temp file, the statement's spill), given back at the drop, as PostgreSQL's
/// Sort keeps its tuples within work_mem and spills past it; the allocator's under the memory policy's floor (mempolicy
/// `work_floor`: the small sorts of hot emits keep their road), or on a ctx with no spill host (a kernel test's).
/// D4373: the road a `Work` block takes (the driver's spill host): `Take` = `SPILL_WORK_TAKE` (W, G, the pool past G, past
/// the pool a mapped temp file); a sort's blocks within its grant (`SPILL_WORK_WITHIN`: W, G, never the pool past G), past
/// it `Put` (a run being put: the statement's mapped temp file, it spills already) or `Seal` (a run sealed in memory: the
/// pool's room now, as PostgreSQL's in-memory sort holds its rows, then the mapped temp file).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Road {
    Take,
    Put,
    Seal,
}

pub(crate) struct Work<T> {
    ctx: *mut Ctx,
    taken: *mut T,
    // perf: the values' base on either road, so a compare's access is a plain slice's (no branch per access)
    ptr: *mut T,
    len: usize,
    heap: Vec<T>,
    // LEDGER2 step 5: the scratch arena `ptr` was carved from (`Work::scratch`), this array's alone: dropped with it, after
    // the fields above (nothing reads `ptr` past the drop)
    scratch: Option<gravity_fn::arena::OpArena>,
}

impl<T> Work<T> {
    /// The block for `len` values; null for the allocator's road; `Err(ERROR)` when the block is refused (the refusal held by
    /// the driver for the emit's raise).
    pub(crate) unsafe fn take(ctx: *mut Ctx, len: usize) -> Result<*mut T, i64> {
        Work::<T>::take_on(ctx, len, Road::Take)
    }

    /// `take` on `road` (D4373: a sort's index, cut scratch and copies take `Road::Put` / `Road::Seal`).
    unsafe fn take_on(ctx: *mut Ctx, len: usize, road: Road) -> Result<*mut T, i64> {
        debug_assert!(!core::mem::needs_drop::<T>() && core::mem::align_of::<T>() <= 8);
        let bytes = len.saturating_mul(core::mem::size_of::<T>());
        let taken = match road {
            _ if bytes == 0 => None,
            Road::Take => gravity_abi::program::work_take(ctx, bytes as u64),
            Road::Put => gravity_abi::program::work_within(ctx, bytes as u64, false),
            Road::Seal => gravity_abi::program::work_within(ctx, bytes as u64, true),
        };
        match taken {
            None => Ok(core::ptr::null_mut()),
            Some(p) if p.is_null() => Err(ERROR),
            Some(p) => Ok(p as *mut T),
        }
    }

    /// Value `i` is `f(i)`.
    pub(crate) unsafe fn from_fn(ctx: *mut Ctx, len: usize, f: impl FnMut(usize) -> T) -> Result<Work<T>, i64> {
        Work::from_fn_on(ctx, len, f, Road::Take)
    }

    /// D4373: `from_fn` on a sort's `road` (`Road::Put` / `Road::Seal`).
    pub(crate) unsafe fn from_fn_on(ctx: *mut Ctx, len: usize, mut f: impl FnMut(usize) -> T, road: Road) -> Result<Work<T>, i64> {
        let taken = Work::<T>::take_on(ctx, len, road)?;
        if taken.is_null() {
            let mut heap: Vec<T> = (0..len).map(f).collect(); // alloc: Node
            return Ok(Work { ctx, taken, ptr: heap.as_mut_ptr(), len, heap, scratch: None });
        }
        for i in 0..len {
            taken.add(i).write(f(i));
        }
        Ok(Work { ctx, taken, ptr: taken, len, heap: Vec::new(), scratch: None }) // alloc: Node
    }

    /// `len` values the caller writes before it reads any: the allocator's zeroed, or the block as it lies.
    pub(crate) unsafe fn block(ctx: *mut Ctx, len: usize) -> Result<Work<T>, i64>
    where
        T: Copy + Default,
    {
        Work::block_on(ctx, len, Road::Take)
    }

    /// D4373: `block` on a sort's `road`.
    pub(crate) unsafe fn block_on(ctx: *mut Ctx, len: usize, road: Road) -> Result<Work<T>, i64>
    where
        T: Copy + Default,
    {
        let taken = Work::<T>::take_on(ctx, len, road)?;
        let mut heap = if taken.is_null() { vec![T::default(); len] } else { Vec::new() }; // alloc: Node
        let ptr = if taken.is_null() { heap.as_mut_ptr() } else { taken };
        Ok(Work { ctx, taken, ptr, len, heap, scratch: None })
    }
}

impl<T> Work<T> {
    /// D4373: a block for `len` values the caller writes before it reads any (`take_on` on a sort's `road`), or `None` on
    /// the allocator's road (the caller keeps its own `Vec`); `Err(ERROR)` when the block is refused.
    pub(crate) unsafe fn granted(ctx: *mut Ctx, len: usize, road: Road) -> Result<Option<Work<T>>, i64> {
        let taken = Work::<T>::take_on(ctx, len, road)?;
        Ok((!taken.is_null()).then(|| Work { ctx, taken, ptr: taken, len, heap: Vec::new(), scratch: None })) // alloc: Node
    }

    /// D4373: values the allocator already holds (a heap top's order, built before any draw): the allocator's road.
    pub(crate) fn heap(mut heap: Vec<T>) -> Work<T> {
        let (ptr, len) = (heap.as_mut_ptr(), heap.len());
        Work { ctx: core::ptr::null_mut(), taken: core::ptr::null_mut(), ptr, len, heap, scratch: None }
    }

    /// LEDGER2 step 5: `len` values `f(i)` on the allocator's road (a sort's index, bitmap or kept list that never takes the
    /// grant): from `ARENA_SCRATCH_MIN` bytes on, carved from a scratch arena of this call charged to the breaker slot
    /// `ctx`'s pipeline OWNS (CONVERT wave 2: `OpArena::own_owner_kind`, the produced slot, else the one an emit reads: the
    /// owner this Work's grant road charges; the allocator's chunks, charged to it once, under its kind), so its bytes leave
    /// the account's unattributed row; else (smaller, no host, no slot owner, or the arena's carve refused) the allocator's
    /// `Vec` as before. `fallible`: the `Vec` reserved with `try_reserve_exact` (`None` when the allocator refuses, as the
    /// caller's own road did); else allocated as `Vec`s are (an abort on the allocator's refusal, as before): never `None`.
    /// # Safety
    /// `ctx` is null or bound.
    pub(crate) unsafe fn scratch(ctx: *mut Ctx, len: usize, mut f: impl FnMut(usize) -> T, fallible: bool) -> Option<Work<T>> {
        debug_assert!(!core::mem::needs_drop::<T>());
        let layout = core::alloc::Layout::array::<T>(len).ok().filter(|l| l.size() >= gravity_abi::program::ARENA_SCRATCH_MIN);
        if let (false, Some(layout)) = (ctx.is_null(), layout) {
            if let Some((o, k)) = gravity_fn::arena::OpArena::own_owner_kind(ctx) {
                let a = gravity_fn::arena::OpArena::scratch_of(Some(o), k);
                if let Ok(p) = a.alloc(layout) {
                    let ptr = p.as_ptr() as *mut T;
                    for i in 0..len {
                        ptr.add(i).write(f(i));
                    }
                    return Some(Work { ctx: core::ptr::null_mut(), taken: core::ptr::null_mut(), ptr, len, heap: Vec::new(), scratch: Some(a) }); // alloc: Node
                }
            }
        }
        let mut heap: Vec<T> = Vec::new(); // alloc: Node
        if fallible {
            heap.try_reserve_exact(len).ok()?;
        } else {
            heap.reserve_exact(len);
        }
        heap.extend((0..len).map(f));
        Some(Work::heap(heap))
    }
}

impl<T> core::ops::Deref for Work<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        // SAFETY: `ptr` is `heap`'s buffer (never grown) or the taken block, `len` values (`from_fn` wrote them, or `block`'s
        // caller does before reading)
        unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl<T> core::ops::DerefMut for Work<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        // SAFETY: as `deref`; the values are this array's alone until the drop
        unsafe { core::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl<T> Drop for Work<T> {
    fn drop(&mut self) {
        if !self.taken.is_null() {
            // SAFETY: `work_take`'s block of this owner, read by nothing past this drop
            unsafe { gravity_abi::program::work_give(self.ctx, self.taken as *mut u8) };
        }
    }
}

pub unsafe fn desc_layout(desc: *const u8) -> RecordLayout {
    let mut w = [0u64; LAYOUT_WORDS];
    core::ptr::copy_nonoverlapping(desc.add(DESC_LAYOUT) as *const u64, w.as_mut_ptr(), LAYOUT_WORDS);
    RecordLayout::from_words(w)
}

crate::contract::host! {
    /// A fresh call gathers every owner's records (the count-ordered form's kept records, else the runs) into a pointer array in scratch and files the state; a resumed call returns the state; null: refused. The region is the pipeline's consumed slot (`consumed[0]`), not its own: an emit that feeds a sort or top-k run owns that run's region.
    pub unsafe fn agg_gather(ctx: u64, desc: u64) -> u64 {
        // the host ABI takes and answers every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        gather_in(ctx as *mut Ctx, desc as *const u8, 0, None) as u64
    }
}

/// `agg_gather_at`'s fresh bit: a fresh gather whatever the resume word says.
pub const GATHER_FRESH: u64 = 1 << 63;

/// D1339 row C (the review's S1): `agg_gather_at`'s array, kept across the inner's re-opens behind its cache word (a frame
/// word no re-open resets; `[cap, ptrs..]` in a pinned chunk, which outlives the owner's call): grown by doubling, so an
/// inner holds O(its most groups) for the statement, never a new array per re-open; null: the pool refused.
unsafe fn kept_array(ctx: *mut Ctx, cache: *mut u64, n: usize) -> *mut Word<*const u8> {
    let mut a = *cache as *mut u64;
    if a.is_null() || (*a as usize) < n {
        let cap = n.max(if a.is_null() { 1 } else { 2 * *a as usize });
        // the statement's pin, never a rerun set's: the cache word outlives every run of a set that re-runs the owner
        a = gravity_abi::program::pin_statement(ctx, 8 * (cap as u64 + 1)) as *mut u64;
        if a.is_null() {
            return core::ptr::null_mut();
        }
        (*a, *cache) = (cap as u64, a as u64);
    }
    a.add(1) as *mut Word<*const u8>
}

#[inline(always)]
pub(crate) unsafe fn gather_in(ctx: *mut Ctx, desc: *const u8, k: usize, fresh: Option<(bool, *mut u64)>) -> *mut u8 {
    gather_in_x(ctx, desc, k, fresh, &[])
}

/// D4620: the finished, unbatched owner regions of consumed regions `extras` (a cascaded grouping set's groups, read
/// after the emit's own); `None` when one is no such region (the gather refuses).
unsafe fn extra_regions<'a>(ctx: *mut Ctx, extras: &[u64], out: &'a mut [Option<&'static Owned>; EMIT_EXTRAS_MAX]) -> Option<&'a [Option<&'static Owned>]> {
    if extras.len() > EMIT_EXTRAS_MAX {
        return None;
    }
    for (i, &k) in extras.iter().enumerate() {
        let r = consumed(ctx, k as usize);
        if r.is_null() || table_kind(r) != TABLE_OWNED || Owned::view(r).batched() {
            return None;
        }
        out[i] = Some(Owned::view(r));
    }
    Some(&out[..extras.len()])
}

/// D4620: the most extra regions a group emit gathers after its own (`limits::CASCADE_SETS_MAX`).
pub const EMIT_EXTRAS_MAX: usize = gravity_abi::limits::CASCADE_SETS_MAX;

/// The records of owner region `o` in the runs form: `f(record)` for each, owner by owner, run by run.
unsafe fn each_run_record(o: &Owned, rec: usize, f: &mut dyn FnMut(*const u8)) {
    for j in 0..o.nowners as usize {
        let h = OwnerTop::view(o.owner(j));
        let dir = o.owner(j).add(OWNERTOP_HDR) as *const u32;
        for g in 0..h.worst as usize {
            let (off, m) = (*dir.add(2 * g) as usize, *dir.add(2 * g + 1) as usize);
            for i in 0..m {
                f(o.owner(j).add(off + i * rec) as *const u8);
            }
        }
    }
}

/// `f(owner, run offset, records)` for each run of owner region `o` the running claim's slices take (`Owned::slice_runs`
/// over the runs the region holds), in run order: a serial run's one claim takes every run.
pub(crate) unsafe fn claim_runs(ctx: *const Ctx, o: &Owned, f: &mut dyn FnMut(*mut u8, usize, usize)) {
    let runs: u64 = (0..o.nowners as usize).map(|j| OwnerTop::view(o.owner(j)).worst as u64).sum();
    let (a, b) = Owned::slice_runs((*ctx).claim.lo, (*ctx).claim.hi, runs);
    let mut r = 0u64;
    for j in 0..o.nowners as usize {
        let w = OwnerTop::view(o.owner(j)).worst as u64;
        if r + w > a && r < b {
            let dir = o.owner(j).add(OWNERTOP_HDR) as *const u32;
            for g in a.max(r)..b.min(r + w) {
                let g = (g - r) as usize;
                f(o.owner(j), *dir.add(2 * g) as usize, *dir.add(2 * g + 1) as usize);
            }
        }
        r += w;
    }
}

/// D4620: `f(owner, record offset, records)` for the records of owner region `o` (`rec` bytes each, the runs form) the
/// running claim's slices take, cut by record over the flat record index (owner 0's runs' records, then owner 1's, ..):
/// a region of one run (an owner phase folded on one thread) still splits evenly over the claims.
pub(crate) unsafe fn claim_records(ctx: *const Ctx, o: &Owned, rec: usize, f: &mut dyn FnMut(*mut u8, usize, usize)) {
    let total = run_records(o) as u64;
    let (a, b) = Owned::slice_runs((*ctx).claim.lo, (*ctx).claim.hi, total);
    let mut r = 0u64;
    for j in 0..o.nowners as usize {
        let h = OwnerTop::view(o.owner(j));
        let dir = o.owner(j).add(OWNERTOP_HDR) as *const u32;
        for g in 0..h.worst as usize {
            if r >= b {
                return;
            }
            let (off, m) = (*dir.add(2 * g) as usize, *dir.add(2 * g + 1) as u64);
            let (s, e) = (a.max(r), b.min(r + m));
            if s < e {
                f(o.owner(j), off + (s - r) as usize * rec, (e - s) as usize);
            }
            r += m;
        }
    }
}

/// The records of owner region `o` in the runs form.
pub(crate) unsafe fn run_records(o: &Owned) -> usize {
    (0..o.nowners as usize).map(|j| {
        let h = OwnerTop::view(o.owner(j));
        (0..h.worst as usize).map(|g| *(o.owner(j).add(OWNERTOP_HDR) as *const u32).add(2 * g + 1) as usize).sum::<usize>()
    }).sum()
}

/// `gather_in` whose array holds, after region `k`'s records, the runs-form records of each consumed region of `extras`
/// (D4620: a cascaded grouping set's group, the base's record layout), in that order.
#[inline(always)]
pub(crate) unsafe fn gather_in_x(ctx: *mut Ctx, desc: *const u8, k: usize, fresh: Option<(bool, *mut u64)>, extras: &[u64]) -> *mut u8 {
    guard!(core::ptr::null_mut(), {
        let own = consumed(ctx, k);
        if own.is_null() || table_kind(own) != TABLE_OWNED {
            return core::ptr::null_mut();
        }
        let mut more = [None; EMIT_EXTRAS_MAX];
        let Some(more) = extra_regions(ctx, extras, &mut more) else { return core::ptr::null_mut() };
        let o = Owned::view(own);
        let h0 = OwnerTop::view(o.owner(0));
        let st = o.owner(0).add(h0.table_off as usize) as *mut EmitState;
        if h0.table_off as usize + size_of::<EmitState>() > o.owner_bytes as usize {
            return core::ptr::null_mut();
        }
        if !fresh.map_or_else(|| (*ctx).resume == RESUME_NONE, |f| f.0) {
            // D1132: the array the fresh call gathered lives in call scratch; a resumed call's allocations must not
            // reach it (a kernel that rewound the arena window below it would hand its words out again).
            let a = (*st).ptrs.ptr() as usize;
            let live = (a, a + 8 * ((*st).len as usize).max(1));
            let window = ((*ctx).arena_cur.ptr() as usize, (*ctx).arena_end.ptr() as usize);
            debug_assert!(window_clear(window, live), "D1132: the arena window reaches a live gather array");
            return st as *mut u8;
        }
        if o.batched() {
            // C10 2b (D3582): a batched slot's one owner holds one batch, gathered by the driver behind the state at the
            // owner's table space (the first batch, folded again when a batch past it was read: a rewind); the plain
            // emit's runs form only, over its first consumed slot (D4620: and no extra region)
            if k != 0 || !more.is_empty() || *(desc.add(DESC_FORM) as *const u64) & FORM_KIND_MASK != FORM_RUNS {
                return core::ptr::null_mut();
            }
            let r = gravity_abi::program::owned_next(ctx, desc_layout(desc).size() as u64, true);
            return if r > 0 { r as usize as *mut u8 } else { core::ptr::null_mut() };
        }
        let rec = desc_layout(desc).size() as usize;
        let form = *(desc.add(DESC_FORM) as *const u64) & FORM_KIND_MASK;
        if form == FORM_COUNT_TOPK && !more.is_empty() {
            return core::ptr::null_mut(); // D4620: a cascaded set's group is in the runs form alone
        }
        let mut n = 0usize;
        for j in 0..o.nowners as usize {
            let h = OwnerTop::view(o.owner(j));
            n += if form == FORM_COUNT_TOPK { h.len as usize } else { (0..h.worst as usize).map(|g| *(o.owner(j).add(OWNERTOP_HDR) as *const u32).add(2 * g + 1) as usize).sum() };
        }
        for x in more.iter().flatten() {
            n += run_records(x);
        }
        let ptrs = match fresh {
            Some((_, cache)) if !cache.is_null() => kept_array(ctx, cache, n),
            _ => util::scratch(ctx, 8 * n.max(1)) as *mut Word<*const u8>,
        };
        if ptrs.is_null() {
            return core::ptr::null_mut();
        }
        let mut i = 0usize;
        for j in 0..o.nowners as usize {
            let h = OwnerTop::view(o.owner(j));
            if form == FORM_COUNT_TOPK {
                for k in 0..h.len as usize {
                    *ptrs.add(i) = word(h.records().add(k * rec));
                    i += 1;
                }
            } else {
                let dir = o.owner(j).add(OWNERTOP_HDR) as *const u32;
                for g in 0..h.worst as usize {
                    let (off, m) = (*dir.add(2 * g) as usize, *dir.add(2 * g + 1) as usize);
                    for k in 0..m {
                        *ptrs.add(i) = word(o.owner(j).add(off + k * rec) as *const u8);
                        i += 1;
                    }
                }
            }
        }
        for x in more.iter().flatten() {
            each_run_record(x, rec, &mut |r| {
                *ptrs.add(i) = word(r);
                i += 1;
            });
        }
        *st = EmitState { ptrs: word(ptrs), len: n as u64, kept: n as u64 };
        st as *mut u8
    })
}

/// D4620: an emit's extra regions `[n, k1 .. kn]` (lowerv4 table/emit_group.rs `extras`) as a slice of their consumed
/// indices; a count past `EMIT_EXTRAS_MAX` reads as more than the gather takes (refused there).
unsafe fn extras_of<'a>(extras: u64) -> &'a [u64] {
    let p = extras as usize as *const u64;
    if p.is_null() {
        return &[];
    }
    let n = (*p as usize).min(EMIT_EXTRAS_MAX + 1);
    core::slice::from_raw_parts(p.add(1), n)
}

crate::contract::host! {
    /// D4620: `agg_gather` whose array also holds the records of the consumed regions `extras` names (`[n, k1 .. kn]`: a
    /// cascaded grouping set's groups, the base group's record layout), after the emit's own. Null: refused.
    pub unsafe fn agg_gather_x(ctx: u64, desc: u64, extras: u64) -> u64 {
        // the host ABI takes and answers every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        gather_in_x(ctx as *mut Ctx, desc as *const u8, 0, None, extras_of(extras)) as u64
    }
}

crate::contract::host! {
    /// D4620: `agg_gather_at` (an inner's gather at consumed region `at & 0xff`, fresh by `GATHER_FRESH`) with the records of
    /// `extras` after its own (`agg_gather_x`).
    pub unsafe fn agg_gather_at_x(ctx: u64, desc: u64, at: u64, cache: u64, extras: u64) -> u64 {
        // the host ABI takes and answers every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        gather_in_x(ctx as *mut Ctx, desc as *const u8, (at & 0xff) as usize, Some((at & GATHER_FRESH != 0, cache as *mut u64)), extras_of(extras)) as u64
    }
}

crate::contract::host! {
    /// D3024 (b): `agg_gather`'s form for a group emit whose claims are the owner runs (`ClaimKind::OwnerRuns`): the claim's
    /// slices of `OWNER_RUN_SLICES` name runs of the flat run index (owner 0's runs, then owner 1's, ..; `Owned::slice_runs`
    /// over the runs the region holds now), whose records are gathered into a pointer array behind an `EmitState`, both in
    /// call scratch (never owner 0's table space: each worker's claim has its own). A serial run's one claim
    /// (`MorselRange::ALL`, or every slice) is every run; the owner runs are disjoint by key hash, so no group is in two
    /// claims. The runs form only (`FORM_RUNS`); null: refused. The lowered emit calls it at a claim's fresh call only (a
    /// resumed call reads its frame words). Memory: the array is the claim's records only, in its thread's call arena,
    /// which a fresh call rewinds (driver node/raw.rs): a thread holds one claim's array at a time, its chunks drawn from
    /// that thread's share of the grant (driver hostimpl/arena `grow_in`: the worker's W, else the statement's G).
    pub unsafe fn agg_gather_claim(ctx: u64, desc: u64) -> u64 {
        // the host ABI takes and answers every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        gather_claim(ctx as *mut Ctx, desc as *const u8, false) as u64
    }
}

crate::contract::host! {
    /// `agg_gather_claim` whose pointer array is a statement pin (`program::pin_statement`), never call scratch: a sorted
    /// wide emit's claim (lowerv4 table/emit_group.rs) sorts its records there and its top files the array in the run table
    /// (`RUNS_*`), which the reader merges after the section (`agg_merge_runs`), past every claim's call. The state stays in
    /// call scratch. Memory: each claim's array (8 bytes a record) lives to the statement's end on its thread's chunks,
    /// drawn from that thread's share of the grant as a pin is. Null: refused.
    pub unsafe fn agg_gather_claim_pinned(ctx: u64, desc: u64) -> u64 {
        // the host ABI takes and answers every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        gather_claim(ctx as *mut Ctx, desc as *const u8, true) as u64
    }
}

/// A sorted wide emit's run table (`ProtoKind::EmitRuns`), laid zeroed by exec per execution behind the proto's handle word:
/// entry `i` (`RUNS_ENTRY_BYTES` each) of `RUNS_MAX` is the run of the claim that started at slice `i` of the owner runs'
/// `OWNER_RUN_SLICES`, filed by the claim's thread at its top's end (lowerv4 ops/sort/topn/lower.rs `TopEnd::finish`): its
/// pointer array (word 0) and its records' count (word 1), both 0 where no claim started. The claims are disjoint slices,
/// so no entry is filed twice. The reader's `EmitState` follows at `RUNS_STATE_OFF` (`agg_merge_runs`).
pub const RUNS_MAX: usize = gravity_abi::breaker::OWNER_RUN_SLICES as usize;
pub const RUNS_ENTRY_BYTES: usize = 16;
pub const RUNS_STATE_OFF: usize = RUNS_ENTRY_BYTES * RUNS_MAX;
/// D3336: the parallel merge's state (`ParState`, `agg_merge_runs_part`) follows the reader's `EmitState`.
pub const RUNS_PAR_OFF: usize = RUNS_STATE_OFF + size_of::<EmitState>();
pub const RUNS_TABLE_BYTES: usize = RUNS_PAR_OFF + size_of::<ParState>();
/// `RUNS_TABLE_BYTES` in words (exec lays the table as words).
pub const RUNS_TABLE_WORDS: usize = RUNS_TABLE_BYTES / 8;
const _: () = assert!(RUNS_TABLE_BYTES % 8 == 0);

crate::contract::host! {
    /// A sorted wide emit's reader (lowerv4 `TopDrain`): the first `n` records (`u64::MAX`: every one) of the runs its claims
    /// filed in run table `table`, each run in `agg_sort`'s order under the descriptor's sort words, as one array in that
    /// order behind the table's `EmitState` (its `kept` = how many): the state's address, 0 when the pool refused the merged
    /// array, or a failed compare's status (its error stashed, D1293 / D344). One run (a serial emit's one claim, or every
    /// other claim empty) is that run's own array, no copy; past one, a k-way merge into a statement pin (`merge_runs_into`).
    /// Once per execution: a later call (a resumed drain's open) answers the state the first one filed.
    pub unsafe fn agg_merge_runs(ctx: u64, desc: u64, table: u64, n: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        merge_runs(ctx as *mut Ctx, desc as *const u8, table as *mut u8, n)
    }
}

/// The non-empty runs of run table `table` (`RUNS_*`) in entry order, each its records' pointers, and how many there are.
/// # Safety
/// `table` is a run table whose filed entries name live arrays.
unsafe fn table_runs<'a>(table: *const u8) -> ([&'a [*const u8]; RUNS_MAX], usize) {
    let mut out: [&[*const u8]; RUNS_MAX] = [&[]; RUNS_MAX];
    let mut len = 0;
    for i in 0..RUNS_MAX {
        let e = table.add(i * RUNS_ENTRY_BYTES) as *const u64;
        let (p, n) = (*e as usize as *const *const u8, *e.add(1) as usize);
        if n > 0 && !p.is_null() {
            out[len] = core::slice::from_raw_parts(p, n);
            len += 1;
        }
    }
    (out, len)
}

#[inline(always)]
unsafe fn merge_runs(ctx: *mut Ctx, desc: *const u8, table: *mut u8, n: u64) -> i64 {
    guard!(ERROR, {
        let st = table.add(RUNS_STATE_OFF) as *mut EmitState;
        // merged once per execution (the table is laid zeroed): a resumed drain's open (a cursor's next FETCH) finds it
        if !(*st).ptrs.ptr().is_null() {
            return st as usize as i64;
        }
        let (runs, len) = table_runs(table);
        let runs = &runs[..len];
        let total: usize = runs.iter().map(|r| r.len()).sum();
        let n = total.min(usize::try_from(n).unwrap_or(usize::MAX));
        if runs.len() <= 1 {
            // one run is the merge: its own array (no run at all: an empty state, its array never read)
            let p = runs.first().map_or(table as *mut Word<*const u8>, |r| r.as_ptr() as *mut Word<*const u8>);
            *st = EmitState { ptrs: word(p), len: n as u64, kept: n as u64 };
            return st as usize as i64;
        }
        let out = gravity_abi::program::pin_statement(ctx, 8 * n.max(1) as u64) as *mut Word<*const u8>;
        if out.is_null() {
            return 0;
        }
        let r = merge_runs_into(ctx, desc, runs, core::slice::from_raw_parts_mut(out as *mut *const u8, n));
        if r != 0 {
            return r;
        }
        *st = EmitState { ptrs: word(out), len: n as u64, kept: n as u64 };
        st as usize as i64
    })
}

/// The first `out.len()` records of `runs` (at most `RUNS_MAX`, each in `agg_sort`'s order under `desc`) in that order: a
/// k-way merge over a min-heap of the runs' heads by `generic_cmp`, agg_sort's comparator for every key word kind. Its
/// order is total (the key words end every comparison) and both of agg_sort's roads sort by it, so the merge is the one
/// sort of the runs' union. A comparator that may answer an order that is not total (fmgr, TOTAL-ORDER) meets only these
/// compares, never a std sort that may panic. 0, or the first failed compare's status (its error stashed): the merge
/// stops there. The crate test seam (`agg_merge_runs` pins `out`).
/// # Safety
/// Every pointer of `runs` is a live record of `desc`'s layout; `out` holds at most the runs' records.
pub unsafe fn merge_runs_into(ctx: *mut Ctx, desc: *const u8, runs: &[&[*const u8]], out: &mut [*const u8]) -> i64 {
    let layout = desc_layout(desc);
    let nsort = nsort_of(desc);
    let keys = core::slice::from_raw_parts(desc.add(DESC_SORT) as *const u64, nsort);
    let procs = procs(desc, nsort);
    let nkw = nullkey_of(*(desc.add(DESC_FORM) as *const u64));
    let nk = (nkw != 0).then(|| nkw as usize - 1);
    let err = core::cell::Cell::new(0i64);
    // the heap holds the runs not yet drained, by index; `at[r]` is run r's next record; the top is the least head
    let mut at = [0usize; RUNS_MAX];
    let mut heap = [0u8; RUNS_MAX];
    let mut len = 0;
    for (r, run) in runs.iter().enumerate().take(RUNS_MAX) {
        if !run.is_empty() {
            heap[len] = r as u8;
            len += 1;
        }
    }
    let less = |a: u8, b: u8, at: &[usize; RUNS_MAX]| {
        let (a, b) = (a as usize, b as usize);
        generic_cmp(ctx, runs[a][at[a]], runs[b][at[b]], &layout, keys, procs, nk, &err) == Ordering::Less
    };
    let down = |heap: &mut [u8; RUNS_MAX], len: usize, mut i: usize, at: &[usize; RUNS_MAX]| loop {
        let (l, r) = (2 * i + 1, 2 * i + 2);
        let mut m = i;
        if l < len && less(heap[l], heap[m], at) {
            m = l;
        }
        if r < len && less(heap[r], heap[m], at) {
            m = r;
        }
        if m == i {
            break;
        }
        heap.swap(i, m);
        i = m;
    };
    for i in (0..len / 2).rev() {
        down(&mut heap, len, i, &at);
    }
    for slot in out.iter_mut() {
        if err.get() != 0 || len == 0 {
            break;
        }
        let r = heap[0] as usize;
        *slot = runs[r][at[r]];
        at[r] += 1;
        if at[r] == runs[r].len() {
            len -= 1;
            heap[0] = heap[len];
        }
        down(&mut heap, len, 0, &at);
    }
    err.get()
}

/// D3336 (lowerv4 road emit.parmerge): the parallel merge's bounds, homed in contract::layout (R11; kernels cannot reach cost).
pub use gravity_abi::limits::{PAR_MERGE_MIN_RECORDS as PAR_MIN_RECORDS, PAR_MERGE_PARTS_MAX as PAR_PARTS_MAX, PAR_MERGE_PART_RECORDS as PAR_PART_RECORDS};
/// The splitters are drawn from this many samples a part (each run's share of them at even steps, merged in order).
const PAR_OVERSAMPLE: usize = 4;
const PAR_SAMPLES_MAX: usize = PAR_OVERSAMPLE * PAR_PARTS_MAX + RUNS_MAX;

/// `ParState::phase`: no thread came yet; one thread lays the merged array and the splitters; the parts may be taken; the
/// merge is the reader's (too few records or runs, the pool refused the array, a failed compare).
const PAR_NONE: u64 = 0;
const PAR_SETUP: u64 = 1;
const PAR_READY: u64 = 2;
const PAR_DECLINED: u64 = 3;

/// D3336: a run table's parallel merge, past its `EmitState` (`RUNS_PAR_OFF`), laid zeroed with the table. The parts are key
/// ranges of the runs' union under the emit's total order: part `p` holds every record `r` with `split[p - 1] <= r <
/// split[p]` (the first part from the least record, the last to the greatest), each run cut at the splitters' lower bounds,
/// so the parts' merges laid one after another in `out` are the one merge of every run, as `agg_merge_runs` lays it.
#[repr(C)]
pub struct ParState {
    phase: u64,
    /// The next part a thread takes, and the parts merged so far (atomics).
    next: u64,
    done: u64,
    nparts: u64,
    /// The merged array (a statement pin of `total` words) and the runs' record count.
    out: u64,
    total: u64,
    /// The splitter records, `nparts - 1` of them in order.
    split: [u64; PAR_PARTS_MAX],
    /// D3336 (lowerv4 road win.partfill): 1 + the leading sort words the parts were cut on (`agg_merge_runs_part_k`): each
    /// part holds every record of its values of those words, so a window partitioned by them is whole in one part; 0:
    /// cut on the whole order. Filed by the setup thread before `PAR_READY`.
    kcut: u64,
    /// D3336 (win.partfill): part `p`'s end in the merged array (its start the part before's end, 0 for the first), each
    /// filed by the part's thread before it counts the part done.
    ends: [u64; PAR_PARTS_MAX],
}

/// `ParState` word `w` as an atomic.
/// # Safety
/// `w` points at a word of a live run table's `ParState`.
unsafe fn par_word<'a>(w: *mut u64) -> &'a core::sync::atomic::AtomicU64 {
    &*(w as *const core::sync::atomic::AtomicU64)
}

crate::contract::host! {
    /// D3336 (lowerv4 road emit.parmerge): a sorted wide emit's section end (`TopEnd::open`, once per claim thread after
    /// every run is filed) merges key ranges of `table`'s runs into one array in parallel: the first thread lays the array
    /// (a statement pin) and the splitters, every thread takes parts, the last part's thread files the array behind the
    /// table's `EmitState` for the reader's `agg_merge_runs` (a reader that takes every record). Under `PAR_MIN_RECORDS`,
    /// one run, or a refused array the reader merges as before. 0, or a failed compare's status (its error stashed).
    pub unsafe fn agg_merge_runs_part(ctx: u64, desc: u64, table: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        merge_runs_part(ctx as *mut Ctx, desc as *const u8, table as *mut u8, 0)
    }
}

crate::contract::host! {
    /// D3336 (lowerv4 road win.partfill): `agg_merge_runs_part` whose parts are cut on the descriptor's first `nk` sort words
    /// alone (each run cut at the lower bound of a splitter's values of those words), so no value of them straddles two
    /// parts: the drain's claims read whole parts (`agg_runs_claim`) and a window partitioned by those words finds each
    /// partition in one claim. The merged array is the same one merge of every run. `nk` 0: `agg_merge_runs_part`.
    pub unsafe fn agg_merge_runs_part_k(ctx: u64, desc: u64, table: u64, nk: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        merge_runs_part(ctx as *mut Ctx, desc as *const u8, table as *mut u8, nk as usize)
    }
}

/// D3336 (win.partfill): `agg_runs_claim`'s answer: the claim reads no record; part claims into one window object; every
/// record, the window dealing its partitions (the serial fill's form).
pub const RUNS_CLAIM_NONE: i64 = 0;
pub const RUNS_CLAIM_PARTS: i64 = 1;
pub const RUNS_CLAIM_ALL: i64 = 2;

crate::contract::host! {
    /// D3336 (lowerv4 road win.partfill): the records the running claim of a K-cut drain reads, for run table `table`
    /// and the window's object table `wtbl` (word 0 its `n` objects, then their handles): `out[0..2]` the claim's
    /// `[lo, hi)` of the merged array (`u64::MAX` past its end: the whole) and `out[2]` the object it fills (0: the window
    /// deals). A claim of one object `i` (the window's fill claims, one object each) reads parts `[i P / n, (i + 1) P / n)` of
    /// the `P` parts the section end cut on the window's keys (`agg_merge_runs_part_k`) into object `i`
    /// (`RUNS_CLAIM_PARTS`); when no such cut was made (too few records or runs, a refused array, the leader's one run)
    /// claim 0 reads every record and the window deals (`RUNS_CLAIM_ALL`) while the others read none
    /// (`RUNS_CLAIM_NONE`), so the drain's merge runs once. The section's end claim reads none; any other claim (the
    /// leader's whole read) every record, dealt.
    pub unsafe fn agg_runs_claim(ctx: u64, table: u64, wtbl: u64, out: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        runs_claim(ctx as *const Ctx, table as *const u8, wtbl as *const u64, out as *mut u64)
    }
}

/// `agg_runs_claim`'s body.
/// # Safety
/// `ctx` is the running claim's context, `table` a live run table, `wtbl` the window's object table, `out` three words.
unsafe fn runs_claim(ctx: *const Ctx, table: *const u8, wtbl: *const u64, out: *mut u64) -> i64 {
    use core::sync::atomic::Ordering::Acquire;
    let (lo, hi) = ((*ctx).claim.lo, (*ctx).claim.hi);
    let set = |a: u64, b: u64, h: u64| {
        (out.write(a), out.add(1).write(b), out.add(2).write(h));
    };
    if lo >= hi {
        set(u64::MAX, u64::MAX, 0);
        return RUNS_CLAIM_NONE;
    }
    let n = if wtbl.is_null() { 0 } else { *wtbl };
    if n == 0 || hi > n || hi - lo != 1 {
        set(0, u64::MAX, 0);
        return RUNS_CLAIM_ALL;
    }
    let par = table.add(RUNS_PAR_OFF) as *mut ParState;
    let cut = par_word(core::ptr::addr_of_mut!((*par).phase)).load(Acquire) == PAR_READY && (*par).kcut > 0;
    if !cut {
        return match lo {
            0 => {
                set(0, u64::MAX, 0);
                RUNS_CLAIM_ALL
            }
            _ => {
                set(u64::MAX, u64::MAX, 0);
                RUNS_CLAIM_NONE
            }
        };
    }
    let parts = (*par).nparts;
    let start = |p: u64| if p == 0 { 0 } else { (*par).ends[p as usize - 1] };
    let (a, b) = (lo * parts / n, hi * parts / n);
    set(start(a), start(b), *wtbl.add(1 + lo as usize));
    RUNS_CLAIM_PARTS
}

/// Sets a `ParState` declined unless disarmed: a setup that leaves early (a refusal, a failed compare, a panic the landing
/// pad parks) never leaves the other threads waiting on `PAR_SETUP`.
struct Decline(*mut u64);

impl Drop for Decline {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the table's phase word, live for the execution.
            unsafe { par_word(self.0) }.store(PAR_DECLINED, core::sync::atomic::Ordering::Release);
        }
    }
}

#[inline(always)]
unsafe fn merge_runs_part(ctx: *mut Ctx, desc: *const u8, table: *mut u8, kcut: usize) -> i64 {
    use core::sync::atomic::Ordering::{AcqRel, Acquire, Release};
    guard!(ERROR, {
        let st = table.add(RUNS_STATE_OFF) as *mut EmitState;
        let par = table.add(RUNS_PAR_OFF) as *mut ParState;
        let (runs, len) = table_runs(table);
        let runs = &runs[..len];
        let layout = desc_layout(desc);
        let nsort = nsort_of(desc);
        let keys = core::slice::from_raw_parts(desc.add(DESC_SORT) as *const u64, nsort);
        let procs = procs(desc, nsort);
        let nkw = nullkey_of(*(desc.add(DESC_FORM) as *const u64));
        let nk = (nkw != 0).then(|| nkw as usize - 1);
        let err = core::cell::Cell::new(0i64);
        let less = |a: *const u8, b: *const u8| generic_cmp(ctx, a, b, &layout, keys, procs, nk, &err) == Ordering::Less;
        // D3336 (win.partfill): the parts cut on the first `kcut` sort words alone (no key-word tie), so each value of them
        // lies in one part; 0 (or past the words): on the whole order
        let kn = kcut.min(nsort);
        let (kkeys, kprocs) = (&keys[..kn], &procs[..kn.min(procs.len())]);
        let below = |a: *const u8, b: *const u8| match kn {
            0 => less(a, b),
            _ => prefix_cmp(ctx, a, b, &layout, kkeys, kprocs, nk, &err) == Ordering::Less,
        };
        let phase = par_word(core::ptr::addr_of_mut!((*par).phase));
        loop {
            match phase.load(Acquire) {
                PAR_READY => break,
                PAR_DECLINED => return 0,
                PAR_SETUP => core::hint::spin_loop(),
                _ => {
                    if phase.compare_exchange(PAR_NONE, PAR_SETUP, AcqRel, Acquire).is_err() {
                        continue;
                    }
                    let mut decline = Decline(core::ptr::addr_of_mut!((*par).phase));
                    let total: usize = runs.iter().map(|r| r.len()).sum();
                    if runs.len() < 2 || total < PAR_MIN_RECORDS {
                        return 0;
                    }
                    let nparts = (total / PAR_PART_RECORDS).clamp(2, PAR_PARTS_MAX);
                    // each run's share of the samples at even steps (in its order), merged into the one order
                    let want = PAR_OVERSAMPLE * nparts;
                    let mut buf = [core::ptr::null::<u8>(); PAR_SAMPLES_MAX];
                    let mut at = 0usize;
                    let mut cuts = [0usize; RUNS_MAX + 1];
                    for (i, r) in runs.iter().enumerate() {
                        let m = (want * r.len()).div_ceil(total).clamp(1, r.len());
                        for j in 0..m {
                            buf[at] = r[(2 * j + 1) * r.len() / (2 * m)];
                            at += 1;
                        }
                        cuts[i + 1] = at;
                    }
                    let mut sruns: [&[*const u8]; RUNS_MAX] = [&[]; RUNS_MAX];
                    for i in 0..runs.len() {
                        sruns[i] = &buf[cuts[i]..cuts[i + 1]];
                    }
                    let mut sorted = [core::ptr::null::<u8>(); PAR_SAMPLES_MAX];
                    let rc = merge_runs_into(ctx, desc, &sruns[..runs.len()], &mut sorted[..at]);
                    if rc != 0 {
                        return rc;
                    }
                    let out = gravity_abi::program::pin_statement(ctx, 8 * total as u64);
                    if out.is_null() {
                        return 0;
                    }
                    for p in 1..nparts {
                        (*par).split[p - 1] = sorted[p * at / nparts] as usize as u64;
                    }
                    ((*par).nparts, (*par).out, (*par).total) = (nparts as u64, out as usize as u64, total as u64);
                    (*par).kcut = if kn > 0 { kn as u64 + 1 } else { 0 };
                    decline.0 = core::ptr::null_mut();
                    phase.store(PAR_READY, Release);
                    break;
                }
            }
        }
        let (nparts, total) = ((*par).nparts as usize, (*par).total);
        let out = (*par).out as usize as *mut *const u8;
        let (next, done) = (par_word(core::ptr::addr_of_mut!((*par).next)), par_word(core::ptr::addr_of_mut!((*par).done)));
        // the first record at or past `s` in run `r` (its length when none): the run's cut at a splitter
        let cut = |r: &[*const u8], s: *const u8| {
            let (mut lo, mut hi) = (0usize, r.len());
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if below(r[mid], s) {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            lo
        };
        loop {
            let p = next.fetch_add(1, AcqRel) as usize;
            if p >= nparts {
                return 0;
            }
            let mut subs: [&[*const u8]; RUNS_MAX] = [&[]; RUNS_MAX];
            let (mut off, mut n) = (0usize, 0usize);
            for (i, r) in runs.iter().enumerate() {
                let lo = if p == 0 { 0 } else { cut(r, (*par).split[p - 1] as usize as *const u8) };
                let hi = if p + 1 == nparts { r.len() } else { cut(r, (*par).split[p] as usize as *const u8) };
                off += lo;
                n += hi.max(lo) - lo;
                subs[i] = &r[lo..hi.max(lo)];
            }
            if err.get() != 0 {
                return err.get();
            }
            let rc = merge_runs_into(ctx, desc, &subs[..runs.len()], core::slice::from_raw_parts_mut(out.add(off), n));
            if rc != 0 {
                return rc;
            }
            (*par).ends[p] = (off + n) as u64;
            if done.fetch_add(1, AcqRel) as usize + 1 == nparts {
                *st = EmitState { ptrs: word(out as *mut Word<*const u8>), len: total, kept: total };
            }
        }
    })
}

#[inline(always)]
unsafe fn gather_claim(ctx: *mut Ctx, desc: *const u8, pinned: bool) -> *mut u8 {
    gather_claim_x(ctx, desc, pinned, &[])
}

/// `gather_claim` whose array holds, after the claim's runs of region 0, the claim's runs of each consumed region of
/// `extras` (D4620: a cascaded grouping set's group): the claim's slices name runs of each region alike, so every record
/// of every region goes to exactly one claim.
#[inline(always)]
unsafe fn gather_claim_x(ctx: *mut Ctx, desc: *const u8, pinned: bool, extras: &[u64]) -> *mut u8 {
    guard!(core::ptr::null_mut(), {
        let own = consumed(ctx, 0);
        if own.is_null() || table_kind(own) != TABLE_OWNED || *(desc.add(DESC_FORM) as *const u64) & FORM_KIND_MASK != FORM_RUNS {
            return core::ptr::null_mut();
        }
        let o = Owned::view(own);
        // C10 2b (D3582): a batched slot is its refilling serial emit's alone (driver mem/batch.rs `read_by_refill`)
        if o.batched() {
            return core::ptr::null_mut();
        }
        let mut more = [None; EMIT_EXTRAS_MAX];
        let Some(more) = extra_regions(ctx, extras, &mut more) else { return core::ptr::null_mut() };
        let rec = desc_layout(desc).size() as usize;
        let each = |o: &Owned, f: &mut dyn FnMut(*mut u8, usize, usize)| claim_runs(ctx, o, f);
        let regions = || core::iter::once(o).chain(more.iter().flatten().copied());
        let mut n = 0usize;
        for x in regions() {
            each(x, &mut |_, _, m| n += m);
        }
        let st = util::scratch(ctx, size_of::<EmitState>()) as *mut EmitState;
        let ptrs = if pinned { gravity_abi::program::pin_statement(ctx, 8 * n.max(1) as u64) } else { util::scratch(ctx, 8 * n.max(1)) } as *mut Word<*const u8>;
        if st.is_null() || ptrs.is_null() {
            return core::ptr::null_mut();
        }
        let mut i = 0usize;
        for x in regions() {
            each(x, &mut |base, off, m| {
                for k in 0..m {
                    *ptrs.add(i) = word(base.add(off + k * rec) as *const u8);
                    i += 1;
                }
            });
        }
        *st = EmitState { ptrs: word(ptrs), len: n as u64, kept: n as u64 };
        st as *mut u8
    })
}

crate::contract::host! {
    /// D4620: `agg_gather_claim` whose array also holds the claim's runs of the consumed regions `extras` names (`[n, k1 ..
    /// kn]`, `agg_gather_x`). Null: refused.
    pub unsafe fn agg_gather_claim_x(ctx: u64, desc: u64, extras: u64) -> u64 {
        // the host ABI takes and answers every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        gather_claim_x(ctx as *mut Ctx, desc as *const u8, false, extras_of(extras)) as u64
    }
}

crate::contract::host! {
    /// D4620: `agg_gather_claim_pinned` with the claim's runs of `extras` (`agg_gather_claim_x`). Null: refused.
    pub unsafe fn agg_gather_claim_pinned_x(ctx: u64, desc: u64, extras: u64) -> u64 {
        // the host ABI takes and answers every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        gather_claim_x(ctx as *mut Ctx, desc as *const u8, true, extras_of(extras)) as u64
    }
}

crate::contract::host! {
    /// C10 2b (D3582): a refilling emit's call at its records' end (lowerv4 table/emit_group.rs `refill`): its first consumed
    /// slot, when batched (`Owned::batched`), refilled with its next batch by the driver (`program::owned_next`, which
    /// folds it into the one owner region in place of the last and gathers its records behind an emit state at the owner's
    /// table space). The state's address, 0 for a slot not batched or past its last batch, or the driver's error status
    /// (negative; the runs form only, else refused).
    pub unsafe fn agg_gather_next(ctx: u64, desc: u64) -> i64 {
        gather_next(ctx as *mut Ctx, desc as *const u8)
    }
}

#[inline(always)]
unsafe fn gather_next(ctx: *mut Ctx, desc: *const u8) -> i64 {
    guard!(gravity_abi::status::unsupported(gravity_abi::status::UNSUPPORTED_SCRATCH), {
        let own = consumed(ctx, 0);
        if own.is_null() || table_kind(own) != TABLE_OWNED || !Owned::view(own).batched() {
            return 0;
        }
        if *(desc.add(DESC_FORM) as *const u64) & FORM_KIND_MASK != FORM_RUNS {
            return gravity_abi::status::unsupported(gravity_abi::status::UNSUPPORTED_SCRATCH);
        }
        gravity_abi::program::owned_next(ctx, desc_layout(desc).size() as u64, false)
    })
}

/// D1132: whether bump window `[cur, end)` lies wholly outside byte range `[a, b)`.
fn window_clear((cur, end): (usize, usize), (a, b): (usize, usize)) -> bool {
    end <= a || cur >= b
}

crate::contract::host! {
    /// The plain distinct set's gather (M5 §4.3, the old rung's `h_gather_plain`): a fresh call sums the owners' `OwnerTop.total`
    /// (`hosts::distinct_fold::plain_total`) and files ONE accumulator record `[sentinel = 1, pairs = total]` of the descriptor's layout in scratch behind an
    /// `EmitState`, so the emit reads it as any group's; a resumed call returns the state; null: refused.
    pub unsafe fn pairs_total(ctx: *mut Ctx, desc: *const u8) -> *mut u8 {
        pairs_total_in(ctx, desc, 0, None)
    }
}

/// `pairs_total` over consumed region `k`; `fresh`: `Some(true)` gathers whatever the resume word says (an embedded inner's
/// call, whose resume word is its owner's: `pairs_total_at`), `None` reads the resume word as `pairs_total` does.
#[inline(always)]
pub(crate) unsafe fn pairs_total_in(ctx: *mut Ctx, desc: *const u8, k: usize, fresh: Option<bool>) -> *mut u8 {
    guard!(core::ptr::null_mut(), {
        let own = consumed(ctx, k);
        if own.is_null() || table_kind(own) != TABLE_OWNED {
            return core::ptr::null_mut();
        }
        let o = Owned::view(own);
        let h0 = OwnerTop::view(o.owner(0));
        let st = o.owner(0).add(h0.table_off as usize) as *mut EmitState;
        if h0.table_off as usize + size_of::<EmitState>() > o.owner_bytes as usize {
            return core::ptr::null_mut();
        }
        if !fresh.unwrap_or((*ctx).resume == RESUME_NONE) {
            return st as *mut u8;
        }
        let acc = desc_layout(desc);
        let total: u64 = (0..o.nowners as usize).map(|i| OwnerTop::view(o.owner(i)).total).sum();
        let bytes = (acc.size() as usize + 7) & !7;
        let mem = util::scratch(ctx, 8 + bytes);
        if mem.is_null() {
            return core::ptr::null_mut();
        }
        let rec = mem.add(8);
        core::ptr::write_bytes(rec, 0, bytes);
        *(rec.add(acc.slot_off(0) as usize) as *mut u64) = 1;
        *(rec.add(acc.slot_off(ACC_PAIRS as usize) as usize) as *mut u64) = total;
        *(mem as *mut Word<*const u8>) = word(rec);
        *st = EmitState { ptrs: word(mem as *mut Word<*const u8>), len: 1, kept: 1 };
        st as *mut u8
    })
}

/// D819: two `avg(int)` finals `(sum, n)`, `n > 0`, in PostgreSQL's order: of the numerics `numeric_div` returns (the sum over
/// the count at `select_div_scale`'s scale, rounded half away from zero), never of the exact rationals, which two groups
/// with different sums can tie on (an average near 1e18 rounds to scale 0), leaving the order to the next sort key.
pub fn avg_cmp((sa, na): (i128, i64), (sb, nb): (i128, i64)) -> Ordering {
    let ((qa, ra), (qb, rb)) = (avg_rounded(sa, na), avg_rounded(sb, nb));
    // qa / 10^ra against qb / 10^rb: the fewer-decimals side scaled up; a product past i128 is past the other side (|q| < 2^127)
    let up = |q: i128, by: u32| 10i128.checked_pow(by).and_then(|p| q.checked_mul(p));
    match ra.cmp(&rb) {
        Ordering::Less => up(qa, rb - ra).map_or_else(|| qa.cmp(&0), |x| x.cmp(&qb)),
        Ordering::Greater => up(qb, ra - rb).map_or_else(|| 0.cmp(&qb), |y| qa.cmp(&y)),
        Ordering::Equal => qa.cmp(&qb),
    }
}

/// `numeric_div(sum, n)` (`n > 0`) as `(q, rscale)`: the quotient is `q / 10^rscale`. `select_div_scale` over the two base-10000
/// digit forms: qweight = weight(sum) - weight(n), less one when sum's first digit is <= n's; rscale = 16 - 4 * qweight, at
/// least 0 (both inputs have dscale 0) and at most 1000. `div_var` (exact) rounds half away from zero. |q| stays under 2^127:
/// with rscale > 0 the quotient is under 10000^4 and q under ~10^20; with rscale 0, |q| <= |sum|.
fn avg_rounded(sum: i128, n: i64) -> (i128, u32) {
    let digits = |x: u128| -> (i32, u128) {
        let (mut w, mut f) = (0, x);
        while f >= 10_000 {
            (w, f) = (w + 1, f / 10_000);
        }
        (w, f)
    };
    let (a, d) = (sum.unsigned_abs(), n as u128);
    let ((w1, f1), (w2, f2)) = (if a == 0 { (0, 0) } else { digits(a) }, digits(d));
    let qweight = w1 - w2 - (f1 <= f2) as i32;
    let rscale = (16 - 4 * qweight).clamp(0, 1000) as u32;
    let (mut q, mut rem) = (a / d, a % d);
    for _ in 0..rscale {
        rem *= 10; // rem < n < 2^63
        q = q * 10 + rem / d;
        rem %= d;
    }
    q += (2 * rem >= d) as u128;
    let q = q as i128;
    (if sum < 0 { -q } else { q }, rscale)
}

/// M6 L0-n: the numeric sum slots a top or a sort over groups orders (`num_order`): sum(numeric) at the program's fixed
/// scale (i128 / i256 mantissas, their specials a sentinel) and sum(int8) (an i128).
fn num_sum(k: SlotKind) -> bool {
    matches!(k, SlotKind::SumNum | SlotKind::SumNumWide | SlotKind::SumI128)
}

/// A numeric sum slot's order as numeric_cmp's (the final is the mantissa at one scale, so exact): (class, high, low) with
/// -Infinity 0, a finite value 1 (its i256 two's complement: high signed, low unsigned), +Infinity 2, NaN 3 (NaN above
/// every value); `None` for the OVERFLOW sentinel (D299), whose final raises instead.
fn num_order(r: &Rec<'_>, j: usize, k: SlotKind) -> Option<(u8, i128, u128)> {
    let v = r.i128(j) as u128;
    num_key(k, if k == SlotKind::SumNumWide { r.num256(j) } else { [v as u64, (v >> 64) as u64, 0, 0] })
}

/// D3141: `agg_sort`'s own mark on a numeric-sum slot word (never a descriptor's) whose records' sums all fit one word
/// (`num_fit`): the word rows order it (`num_pair`), not the comparator.
const SK_NUM_WORD: u64 = 1 << 62;

/// Whether `w` carries `SK_NUM_WORD` (a text word's collation bits may hold bit 62: never one).
const fn num_marked(w: u64) -> bool {
    w & (SK_NUM_WORD | SK_TEXT) == SK_NUM_WORD
}
/// A finite sum's mantissa fits one order word below this magnitude, so -Infinity / +Infinity / NaN keep words past it.
const NUM_WORD_MAX: i128 = 1 << 62;

/// D3141: numeric-sum order `o` (`num_order`) as one signed word in numeric_cmp's order: -Infinity `i64::MIN`, a finite
/// sum its mantissa (one scale per slot), +Infinity and NaN the two greatest; `None` for a mantissa past `NUM_WORD_MAX`.
fn num_fit(o: (u8, i128, u128)) -> Option<i64> {
    match o.0 {
        0 => Some(i64::MIN),
        2 => Some(i64::MAX - 1),
        3 => Some(i64::MAX),
        _ => {
            let v = o.2 as i128;
            (o.1 == v >> 127 && (-NUM_WORD_MAX..NUM_WORD_MAX).contains(&v)).then_some(v as i64)
        }
    }
}

/// Whether sort word `w` orders through word rows (`decorate`): no text, comparator or average word, and a numeric sum
/// only under `SK_NUM_WORD`.
fn row_word(w: u64, kinds: &[SlotKind]) -> bool {
    if num_marked(w) {
        return true;
    }
    w & SK_TEXT == 0 && !sk_key_cmp(w) && !sk_slot_cmp(w) && !(w & SK_SLOT != 0 && (kinds[(w & 0xff) as usize] == SlotKind::AvgI128 || num_sum(kinds[(w & 0xff) as usize])))
}

/// D3141 (a group emit ordered by a numeric sum compared every pair through `generic_cmp`'s numeric order, 0.6 us per
/// group): `keys` with `SK_NUM_WORD` on each numeric-sum slot word, where every other word is a row word and every
/// record's sum fits one word; `None` otherwise. E-kern: up to `DESC_MAX_SORT` sort words (every descriptor but a wide
/// sorted group's, W8-SETOP-WIDEKEYS) the marked words are a fixed array, no allocation; past it a `ChargedVec` on the
/// breaker slot `ctx`'s pipeline owns (`OpArena::own_owner_kind`, `Work`'s owner), asked only then.
unsafe fn num_words(ctx: *mut Ctx, ptrs: &[*const u8], layout: &RecordLayout, keys: &[u64]) -> Option<Marked> {
    let kinds = layout.slot_kinds();
    let num = |w: u64| w & (SK_SLOT | SK_TEXT | SK_CMP) == SK_SLOT && num_sum(kinds[(w & 0xff) as usize]);
    if !keys.iter().any(|&w| num(w)) || !keys.iter().all(|&w| num(w) || row_word(w, kinds)) {
        return None;
    }
    for &p in ptrs {
        let r = Rec::at(p as *mut u8, layout);
        for &w in keys.iter().filter(|&&w| num(w)) {
            let j = (w & 0xff) as usize;
            if r.valid(j) && num_order(&r, j, kinds[j]).is_some_and(|o| num_fit(o).is_none()) {
                return None;
            }
        }
    }
    let mark = |w: u64| if num(w) { w | SK_NUM_WORD } else { w };
    if keys.len() <= DESC_MAX_SORT {
        let mut few = [0u64; DESC_MAX_SORT];
        few.iter_mut().zip(keys).for_each(|(m, &w)| *m = mark(w));
        return Some(Marked::Few(few, keys.len()));
    }
    let o = gravity_fn::arena::OpArena::own_owner_kind(ctx);
    let mut many = gravity_fn::arena::ChargedVec::<u64>::new(o.map(|o| o.0), o.map_or(gravity_fn::arena::Kind::Sort, |o| o.1));
    many.with_vec(|v| v.extend(keys.iter().map(|&w| mark(w)))); // alloc: Node charged o
    Some(Marked::Many(many))
}

/// E-kern: `num_words`' marked sort words: a fixed array and its count up to `DESC_MAX_SORT`, else charged.
enum Marked {
    Few([u64; DESC_MAX_SORT], usize),
    Many(gravity_fn::arena::ChargedVec<u64>),
}

impl Marked {
    fn words(&self) -> &[u64] {
        match self {
            Marked::Few(w, n) => &w[..*n],
            Marked::Many(v) => v,
        }
    }
}

/// `num_order` over a slot's words (an i128 slot's two, a `SumNumWide`'s four): abi `num_sum_order`.
fn num_key(k: SlotKind, w: [u64; 4]) -> Option<(u8, i128, u128)> {
    gravity_abi::breaker::num_sum_order(k, w)
}

unsafe fn sort_cell(r: &Rec<'_>, layout: &RecordLayout, w: u64, nk: Option<usize>) -> (i64, bool) {
    let j = sk_key_word(w);
    if w & (SK_SLOT | SK_TEXT) == SK_TEXT {
        return (0, true); // D364: a text key word's NULL is its representative's (`text_image`), never key 0's flag bit
    }
    let float = w & SK_FLOAT != 0;
    if w & SK_SLOT == 0 {
        let bit = match sk_null_index(w) {
            0 => j,
            b => b - 1,
        };
        let v = if float { float_order(f64::from_bits(r.key(j))) } else if w & SK_UNSIGNED != 0 { (r.key(j) ^ 1 << 63) as i64 } else { r.key(j) as i64 };
        let (fw, fb) = gravity_abi::breaker::flag_at(bit); // CAP-28: one flags word per 64 keys from the first, `nk`
        return (v, nk.is_none_or(|n| r.key(n + fw) >> fb & 1 == 0));
    }
    slot_cell(r, layout, w)
}

/// The order cell of slot sort word `w` (`SK_SLOT`) over record `r`: its value as a signed word and whether it is not
/// NULL. Inlined into `sort_cell` always: a call per cell costs the emit's text sorts.
#[inline(always)]
unsafe fn slot_cell(r: &Rec<'_>, layout: &RecordLayout, w: u64) -> (i64, bool) {
    let j = (w & 0xff) as usize;
    let float = w & SK_FLOAT != 0;
    // D443: a slot's NULL here is exactly its final's (lowerv4 fold/class_word.rs `finish`, fold/slots.rs `slot_valid`): a
    // count is never NULL (0 over no row, even where a FILTER or a NULL argument left its validity bit unset), an average is
    // NULL exactly where its count word is 0, every other slot by its validity (the bit, or the kind's own test)
    match layout.slot_kinds()[j] {
        SlotKind::Count => (r.count(j), true),
        SlotKind::AvgI128 => (r.i64(j), r.avg(j).1 != 0),
        SlotKind::F64Accum if float => {
            let [n, sx, _] = r.f64_accum(j);
            (float_order(sx / n), r.valid(j)) // float8_avg's Sx / N (the slot is NULL while N = 0)
        }
        _ if float => (float_order(r.f64(j)), r.valid(j)),
        _ => (r.i64(j), r.valid(j)),
    }
}

/// A float8's total-order word, compared signed (lowerv4 rep/word/order.rs `float_order`): `-0` as `+0`, every NaN equal
/// and above every other value (`float8_cmp_internal`), a negative's magnitude bits flipped.
pub fn float_order(x: f64) -> i64 {
    if x.is_nan() {
        return i64::MAX;
    }
    let b = (x + 0.0).to_bits() as i64;
    b ^ ((b >> 63) & i64::MAX)
}

/// A text key's representative image of `r` under the sort word `w` (`SK_TEXT`): the arena datum of the slot (`SK_HEAP`) or the slot's code rendered through the site's dictionary; `None` is NULL. A failed render is NULL here with its status
/// kept in `err` (the first one): `agg_sort` returns it, never a sort that read it as a NULL (D344). A kept copy that is an
/// inline compressed datum (D2239: `key_copy` keeps it as it is) is `Img::Packed`, detoasted only for the compare.
unsafe fn text_image<'a>(ctx: *mut Ctx, r: &Rec<'_>, w: u64, err: &core::cell::Cell<i64>) -> Option<Img<'a>> {
    let slot = (w >> SK_TEXT_SLOT_SHIFT & 0xff) as usize;
    // D534: a representative never folded is NULL; under a validity word its word stays 0, which is a live code
    if !r.valid(slot) {
        return None;
    }
    let word = r.i64(slot) as u64;
    let p = if w & SK_HEAP != 0 {
        if word == 0 {
            return None;
        }
        if w & SK_NAME != 0 {
            return Some(Img::Bytes(name_image(word as *const u8)));
        }
        if w & SK_CMP != 0 {
            return Some(Img::Bytes(&[])); // a kept copy (a fixed image is no varlena): only its presence is read here (`copy_cmp`)
        }
        if *(word as *const u8) & 3 == 2 {
            return Some(Img::Packed(word as *const u8)); // VARATT_IS_4B_C
        }
        word as *const u8
    } else {
        match crate::dict::reps::render(gravity_abi::ctx::site_ptr(ctx, (w >> SK_TEXT_SITE_SHIFT & 0xff) as usize), (w >> SK_TEXT_LANE_SHIFT & 0xff) as u8, word) {
            Ok(p) => p?,
            Err(st) => {
                if err.get() == 0 {
                    err.set(st);
                }
                return None;
            }
        }
    };
    Some(Img::Bytes(crate::dict::reps::varlena_bytes(p)))
}

/// A text key's compared image: its bytes, or a kept compressed copy (D2239) detoasted only while it is compared.
#[derive(Clone, Copy)]
enum Img<'a> {
    Bytes(&'a [u8]),
    Packed(*const u8),
}

impl Img<'_> {
    /// `f` over the image's bytes; a compressed copy's detoast error is the `Err`. # Safety — a `Packed` image is a live varlena.
    unsafe fn with<R>(&self, f: impl FnOnce(&[u8]) -> R) -> Result<R, Box<gravity_fn::PgError>> {
        match *self {
            Img::Bytes(b) => Ok(f(b)),
            Img::Packed(p) => gravity_fn::with_payload(p, f),
        }
    }
}

/// A `name`'s compared image (`SK_NAME`): its bytes before the first NUL of its 64. # Safety — `p` is 64 readable bytes.
pub(crate) unsafe fn name_image<'a>(p: *const u8) -> &'a [u8] {
    let name = core::slice::from_raw_parts(p, NAME_BYTES);
    &name[..name.iter().position(|&c| c == 0).unwrap_or(NAME_BYTES)]
}

/// Two kept copies (`SK_CMP`) ordered by their key's btree support function `proc` under the word's collation. After a
/// failed compare (this one's or an earlier render's / collation's: `err` set) every pair is `Equal` with no call: the
/// first error stays the stash's and `agg_sort` returns it (D1293), so the sort's order is never used.
unsafe fn copy_cmp(a: &Rec<'_>, b: &Rec<'_>, w: u64, proc: u64, err: &core::cell::Cell<i64>) -> Ordering {
    if err.get() != 0 {
        return Ordering::Equal;
    }
    let slot = (w >> SK_TEXT_SLOT_SHIFT & 0xff) as usize;
    let (da, db) = (a.i64(slot) as u64, b.i64(slot) as u64);
    let coll = (w >> SK_TEXT_COLL_SHIFT) as u32;
    gravity_fn::typesem::keysem::call::order(proc as u32, coll, da, db).unwrap_or_else(|e| {
        gravity_fn::stash_error(e);
        err.set(ERROR);
        Ordering::Equal
    })
}

/// Two by-value words (`sk_key_cmp`: an enum's oid; `sk_slot_cmp`: a slot's Datum, as PostgreSQL passes it) ordered by
/// their comparator `proc` under collation 0, as `copy_cmp` orders two copies (a failed compare stashes its error and
/// every pair after it is `Equal`).
pub(crate) unsafe fn word_cmp(a: u64, b: u64, proc: u64, err: &core::cell::Cell<i64>) -> Ordering {
    if err.get() != 0 {
        return Ordering::Equal;
    }
    gravity_fn::typesem::keysem::call::order(proc as u32, 0, a, b).unwrap_or_else(|e| {
        gravity_fn::stash_error(e);
        err.set(ERROR);
        Ordering::Equal
    })
}

/// `bcTruelen`: the bytes without their trailing blanks (a bpchar's compared image).
fn bc_truelen(s: &[u8]) -> &[u8] {
    &s[..s.iter().rposition(|&c| c != b' ').map_or(0, |i| i + 1)]
}

/// CAND-25: a record's text word image as `generic` keeps it from its first comparison on, so a render through the
/// source (`dict_value`, a host call) or a kept copy's bytes is read once per record, not per comparison. `head`: only
/// where the word orders by bytes (`head_word`), the eight bytes past the images' shared prefix (`Memo::skip`; the first
/// word's only) big-endian, zero padded; where two differ they order the images (every image begins with the same `skip`
/// bytes, and a shorter image is the other's prefix there, and sorts first either way).
#[derive(Clone, Copy)]
struct Taken<'a> {
    img: Option<Img<'a>>,
    head: Option<u64>,
}

/// `generic`'s text words: per record and text word (`t` of them) its `Taken`; `skip`: the bytes every image of the first
/// word begins with (`shared_prefix`), which its heads start past.
struct Memo<'a> {
    t: usize,
    skip: usize,
    taken: Work<core::cell::Cell<Option<Taken<'a>>>>,
}

impl Memo<'_> {
    /// # Safety — `ctx` as `Work::from_fn`'s.
    unsafe fn new(ctx: *mut Ctx, keys: &[u64], m: usize, skip: usize) -> Result<Self, i64> {
        let t = keys.iter().filter(|&&w| w & SK_TEXT != 0).count();
        Ok(Memo { t, skip, taken: Work::from_fn(ctx, m * t, |_| core::cell::Cell::new(None))? })
    }
}

/// The length of the prefix every present image of heap text word `w` shares, when its heads order by bytes
/// (`head_word`); 0 for a dictionary word (its renders are its source's calls, taken later in code order) or where an
/// image is a compressed kept copy (it has no head). Keys that share a long prefix (zero-padded numbers, one URL scheme)
/// tie on their first eight bytes, so `heads_order` gave every compare to the comparator (61-byte keys sharing 55: 3x
/// the row executor's sort); the eight bytes past the prefix tell them apart. One pass, ended by a first-byte difference.
unsafe fn shared_prefix(ctx: *mut Ctx, ptrs: &[*const u8], layout: &RecordLayout, w: u64, nk: Option<usize>, err: &core::cell::Cell<i64>) -> usize {
    if w & SK_HEAP == 0 || !head_word(w) {
        return 0;
    }
    let mut first: Option<&[u8]> = None;
    for &p in ptrs {
        let r = Rec::at(p as *mut u8, layout);
        if !sort_cell(&r, layout, w, nk).1 {
            continue;
        }
        match text_image(ctx, &r, w, err) {
            None => {}
            Some(Img::Packed(_)) => return 0,
            Some(Img::Bytes(b)) => match first {
                None => first = Some(b),
                Some(f) => {
                    let l = common_len(f, b);
                    if l == 0 {
                        return 0;
                    }
                    first = Some(&f[..l]);
                }
            },
        }
    }
    first.map_or(0, |f| f.len())
}

/// Whether `shared_prefix`'s pass over every record can pay: on `heads_order`'s even sample of at most `HEAD_SAMPLE`
/// records, the images share a prefix and the eight bytes past it are at least half distinct. Where the sample shares
/// none, neither do all the records; where its heads past the prefix are a few values, `heads_order` declines the road
/// anyway, and the full pass was cost alone (100 shared bytes, then one of three bytes: 8 ms of a 59 ms top-N at W8).
/// Either way the sort is trunk's, skip 0.
unsafe fn prefix_spread(ctx: *mut Ctx, ptrs: &[*const u8], layout: &RecordLayout, w: u64, nk: Option<usize>, err: &core::cell::Cell<i64>) -> bool {
    if w & SK_HEAP == 0 || !head_word(w) {
        return false;
    }
    // E-kern: the sample (at most `SAMPLE_MAX` records) on the stack, no allocation
    let (mut imgs, mut ni): ([&[u8]; SAMPLE_MAX], usize) = ([&[]; SAMPLE_MAX], 0);
    let mut first: Option<&[u8]> = None;
    for i in (0..ptrs.len()).step_by((ptrs.len() / HEAD_SAMPLE).max(1)) {
        let r = Rec::at(ptrs[i] as *mut u8, layout);
        if !sort_cell(&r, layout, w, nk).1 {
            continue;
        }
        match text_image(ctx, &r, w, err) {
            None => {}
            Some(Img::Packed(_)) => return false,
            Some(Img::Bytes(b)) => {
                first = Some(match first {
                    None => b,
                    Some(f) => &f[..common_len(f, b)],
                });
                if let Some(slot) = imgs.get_mut(ni) {
                    (*slot, ni) = (b, ni + 1);
                }
            }
        }
    }
    let s = first.map_or(0, |f| f.len());
    if s == 0 {
        return false;
    }
    let mut heads = [0u64; SAMPLE_MAX];
    for (h, b) in heads.iter_mut().zip(&imgs[..ni]) {
        let b = &b[s.min(b.len())..];
        let mut w = [0u8; 8];
        let k = b.len().min(8);
        w[..k].copy_from_slice(&b[..k]);
        *h = u64::from_be_bytes(w);
    }
    let heads = &mut heads[..ni];
    heads.sort_unstable();
    2 * distinct(heads) >= ni
}

/// E-kern: the distinct values of a sorted slice (`Vec::dedup`'s length after, without the `Vec`).
fn distinct<T: PartialEq>(sorted: &[T]) -> usize {
    sorted.len().min(1) + sorted.windows(2).filter(|w| w[0] != w[1]).count()
}

/// The number of leading bytes `a` and `b` share, eight at a time.
fn common_len(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    while i + 8 <= n {
        let x = u64::from_le_bytes(a[i..i + 8].try_into().unwrap());
        let y = u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        if x != y {
            return i + ((x ^ y).trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

/// Record `r`'s image under text word `w` (`text_image`): from `memo`'s cell for record `i` and text word `col` once
/// taken (with its `head`), else read here.
unsafe fn take<'a>(ctx: *mut Ctx, r: &Rec<'_>, w: u64, (m, i): (&Memo<'a>, usize), col: usize, err: &core::cell::Cell<i64>) -> Taken<'a> {
    let cell = &m.taken[i * m.t + col];
    if let Some(t) = cell.get() {
        return t;
    }
    let img = text_image(ctx, r, w, err);
    let head = match img {
        Some(Img::Bytes(b)) if head_word(w) => {
            let b = &b[if col == 0 { m.skip.min(b.len()) } else { 0 }..];
            let mut h = [0u8; 8];
            let k = b.len().min(8);
            h[..k].copy_from_slice(&b[..k]);
            Some(u64::from_be_bytes(h))
        }
        _ => None,
    };
    let t = Taken { img, head };
    cell.set(Some(t));
    t
}

/// Two text representatives' images (`text_image`, rendered once by the caller) compared under the word's collation: bytes then length under C / 0, else the collation's compare (`SK_BPCHAR`: each without its trailing blanks first; a failing compare sorts as equal, its error stashed and `err` set to `ERROR` unless a render failed first: D1293).
unsafe fn text_cmp(x: &Img<'_>, y: &Img<'_>, w: u64, err: &core::cell::Cell<i64>) -> Ordering {
    let o = x.with(|x| y.with(|y| bytes_cmp(x, y, w)));
    match o {
        Ok(Ok(Ok(o))) => o,
        // D1293: PostgreSQL's comparator raises here (or a compressed copy's detoast failed). The error is stashed and
        // `err` set, so `agg_sort` returns it and the emit raises before a row leaves (it read `Equal` and returned 0:
        // the rows went out in the wrong order, no error).
        Ok(Ok(Err(e))) | Ok(Err(e)) | Err(e) => {
            if err.get() == 0 {
                gravity_fn::stash_error(e);
                err.set(ERROR);
            }
            Ordering::Equal
        }
    }
}

/// `text_cmp` over two taken images (`Taken`): by their heads where both have one and they differ.
unsafe fn taken_cmp(x: Taken<'_>, y: Taken<'_>, w: u64, err: &core::cell::Cell<i64>) -> Ordering {
    if let (Some(hx), Some(hy)) = (x.head, y.head) {
        if hx != hy {
            return hx.cmp(&hy);
        }
    }
    let (Some(x), Some(y)) = (x.img, y.img) else { return Ordering::Equal };
    text_cmp(&x, &y, w, err)
}

/// Two text images under the word's collation: bytes then length under 0 (the lowering clears a collation whose facts
/// order by bytes: topn lower.rs `byte_coll`), else the collation's compare, whose C fast path is varstr_cmp's own
/// (`SK_BPCHAR`: each without its trailing blanks first).
fn bytes_cmp(x: &[u8], y: &[u8], w: u64) -> Result<Ordering, Box<gravity_fn::PgError>> {
    let (x, y) = if w & SK_BPCHAR != 0 { (bc_truelen(x), bc_truelen(y)) } else { (x, y) };
    let coll = (w >> SK_TEXT_COLL_SHIFT) as u32;
    if coll == 0 {
        return Ok(x.cmp(y));
    }
    gravity_fn::typesem::collation::compare(x, y, coll).map(|c| c.cmp(&0))
}

/// CAND-25: whether text word `w`'s images order by their bytes as a property of its collation, so their first eight
/// bytes (`Taken::head`) may stand for the order where two differ: the word's collation is 0. The lowering leaves 0 only
/// where the key's collation orders and compares by bytes (topn lower.rs `byte_coll` clears it by facts `coll`'s
/// `bytes_order`: C, POSIX, a C database's default) or the key has none; every other collation (ICU, libc locales,
/// nondeterministic ones) keeps its oid here, takes no head, and is compared by `bytes_cmp`'s collation call from the
/// first compare on. Never a bpchar (its images lose their trailing blanks first) or a kept copy ordered by its
/// comparator (`copy_cmp`: its image here is only a presence mark).
fn head_word(w: u64) -> bool {
    (w >> SK_TEXT_COLL_SHIFT) as u32 == 0 && w & SK_BPCHAR == 0 && w & (SK_HEAP | SK_CMP) != SK_HEAP | SK_CMP
}

crate::contract::host! {
    /// The kept records sorted by the descriptor's keys (NULLS FIRST / DESC per key word; averages as exact rationals), ties by the key words ascending;
    /// 0, or the status of a text key's failed render (its error stashed in the node: the emit raises it, D344) or failed
    /// collation compare (`ERROR`, PostgreSQL's error stashed: D1293).
    ///
    /// V4R-190: without an average or text key word every record's order is a row of plain words (`decorate`: per sort word
    /// its NULL rank and its value, `!v` under DESC; then the key words), sorted as word rows, the same order as `generic`'s
    /// comparator. A bound in the form word (`FORM_BOUND_SHIFT`) orders only the first `n`: the rest is selected past them
    /// unordered. The order is total (the key words end every comparison), so both forms put the same records first.
    pub unsafe fn agg_sort(_ctx: u64, st: u64, desc: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let _ctx = _ctx as *mut Ctx;
        let st = st as *mut u8;
        let desc = desc as *const u8;
        guard!(ERROR, {
            let st = st as *mut EmitState;
            let layout = desc_layout(desc);
            let nsort = nsort_of(desc);
            let keys = core::slice::from_raw_parts(desc.add(DESC_SORT) as *const u64, nsort);
            let form = *(desc.add(DESC_FORM) as *const u64);
            let nkw = nullkey_of(form);
            let nk = (nkw != 0).then(|| nkw as usize - 1);
            let bound = ((form & 0xffff_ffff) >> FORM_BOUND_SHIFT) as usize;
            let (ptrs, _wide) = Narrow::of((*st).ptrs.ptr(), (*st).kept as usize);
            let n = if bound == 0 { ptrs.len() } else { (bound - 1).min(ptrs.len()) };
            let ties = form & FORM_TIES != 0;
            let marked = if ptrs.len() > 1 { num_words(_ctx, ptrs, &layout, keys) } else { None };
            let rows = marked.as_ref().map_or(keys, Marked::words);
            if ptrs.len() > 1 && rows.iter().all(|&w| row_word(w, layout.slot_kinds())) {
                let k = match sort_decorated(_ctx, ptrs, &layout, rows, nk, n, ties) {
                    Ok(k) => k,
                    Err(status) => return status,
                };
                return if ties { k as i64 } else { 0 };
            }
            // RUNS-18: the comparator's road reads no tie off a word row: a top WITH TIES orders every record there
            let n = if ties { ptrs.len() } else { n };
            let r = generic(_ctx, ptrs, &layout, keys, procs(desc, nsort), nk, n);
            if ties && r == 0 { ptrs.len() as i64 } else { r }
        })
    }
}

/// The descriptor's comparator words (`DESC_PROCS`, or where `DESC_PROCS_AT_SHIFT` places them), one per sort word.
unsafe fn procs<'a>(desc: *const u8, nsort: usize) -> &'a [u64] {
    let at = (*(desc.add(DESC_NSORT) as *const u64) >> DESC_PROCS_AT_SHIFT) as usize;
    core::slice::from_raw_parts(desc.add(if at == 0 { DESC_PROCS } else { at }) as *const u64, nsort)
}

/// `agg_sort`'s comparator over the records (every key word kind: averages, text representatives): the first `n` in
/// order, the rest after them in any order (`sort_decorated`'s bound).
#[allow(clippy::too_many_arguments)]
unsafe fn generic(ctx: *mut Ctx, ptrs: &mut [*const u8], layout: &RecordLayout, keys: &[u64], procs: &[u64], nk: Option<usize>, n: usize) -> i64 {
    generic_by(ctx, ptrs, layout, keys, procs, nk, n, small_on())
}

/// `generic` with the small road (`small_generic`) on or off.
#[allow(clippy::too_many_arguments)]
unsafe fn generic_by(ctx: *mut Ctx, ptrs: &mut [*const u8], layout: &RecordLayout, keys: &[u64], procs: &[u64], nk: Option<usize>, n: usize, small: bool) -> i64 {
    let err = core::cell::Cell::new(0i64);
    // spicmp (USEROP): an `SK_CMP` word whose comparator is SQL / PL code may raise mid-sort; its sorts stop at the first
    // failure (`bounded_sort`'s `stop`)
    let user = keys.iter().enumerate().any(|(i, &w)| w & SK_CMP != 0 && procs.get(i).is_some_and(|&p| gravity_fn::effects::user_call(p as u32)));
    // TOTAL-ORDER (user ruling 10-04): any word ordered through its comparator (fmgr, `DESC_PROCS` non-zero: a kept copy's
    // `copy_cmp`, a by-value word's `word_cmp`; a support function that may reach user code, as a record's or an array's
    // over a user type does) may answer an order that is not total, where std's select / sort panic; such sorts run
    // PostgreSQL's qsort too (any order is then valid, a panic never)
    let fmgr = keys.iter().enumerate().any(|(i, &w)| w & SK_CMP != 0 && procs.get(i).is_some_and(|&p| p != 0));
    let stop = (user || fmgr).then_some(&err);
    // agg_shop-0033 perf: a built-in comparator's bounded sort (no SQL / PL word) selects its first `n` (`top_sort`)
    let top = !user;
    if small && stop.is_none() && n == ptrs.len() {
        if let Some(st) = small_generic(ctx, ptrs, layout, keys, procs, nk, &err) {
            return st;
        }
    }
    // CAND-25: a first sort word of text that orders by bytes (`head_word`) and whose heads tell the records apart
    // (`heads_order`): each record's images are taken once (`Taken`), its (NULL rank, head) words decorate it, and the sort
    // runs over record indices with the full comparator only where heads tie (a later text word renders where a tie
    // reads it). Every other sort, under any other collation included, is trunk's comparator over the records.
    let road = keys.first().copied().filter(|&w| w & SK_TEXT != 0 && head_word(w) && ptrs.len() > 1);
    let memo = road.map(|w| Memo::new(ctx, keys, ptrs.len(), if prefix_spread(ctx, ptrs, layout, w, nk, &err) { shared_prefix(ctx, ptrs, layout, w, nk, &err) } else { 0 })).transpose();
    let memo = match memo {
        Ok(m) => m,
        Err(status) => return status,
    };
    let (Some(first), Some(memo)) = (road, memo) else {
        top_sort(ptrs, n, stop, top, |a, b| generic_cmp(ctx, *a, *b, layout, keys, procs, nk, &err));
        return err.get();
    };
    if !heads_order(ctx, ptrs, layout, first, nk, &memo, &err) {
        top_sort(ptrs, n, stop, top, |a, b| generic_cmp(ctx, *a, *b, layout, keys, procs, nk, &err));
        return err.get();
    }
    if first & SK_HEAP == 0 {
        // dictionary codes: taken in code order (by part, then code), so a part's renders run together and its source
        // memoises them, where the records' own order alternates parts at every render
        let slot = (first >> SK_TEXT_SLOT_SHIFT & 0xff) as usize;
        let mut order = match Work::from_fn(ctx, ptrs.len(), |i| (Rec::at(ptrs[i] as *mut u8, layout).i64(slot) as u64, i as u32)) {
            Ok(o) => o,
            Err(status) => return status,
        };
        order.sort_unstable();
        for &(_, i) in order.iter() {
            let r = Rec::at(ptrs[i as usize] as *mut u8, layout);
            if sort_cell(&r, layout, first, nk).1 {
                take(ctx, &r, first, (&memo, i as usize), 0, &err);
            }
        }
    }
    let mut idx = match Work::from_fn(ctx, ptrs.len(), |i| i as u32) {
        Ok(x) => x,
        Err(status) => return status,
    };
    {
        let ptrs = &*ptrs;
        let full = |a: &u32, b: &u32| generic_taken_cmp(ctx, ptrs[*a as usize], ptrs[*b as usize], layout, keys, procs, nk, (&memo, *a as usize, *b as usize), &err);
        // the first word's (NULL rank, head) words order two records wherever they differ, the full comparator the rest
        match heads(ctx, ptrs, layout, first, nk, &memo, &err) {
            Ok(Some(dec)) => top_sort(&mut idx, n, stop, top, |a, b| dec[*a as usize].cmp(&dec[*b as usize]).then_with(|| full(a, b))),
            Ok(None) => top_sort(&mut idx, n, stop, top, full),
            Err(status) => return status,
        }
    }
    drop(memo);
    let old = match Work::from_fn(ctx, ptrs.len(), |i| ptrs[i]) {
        Ok(o) => o,
        Err(status) => return status,
    };
    for (d, &i) in ptrs.iter_mut().zip(idx.iter()) {
        *d = old[i as usize];
    }
    err.get()
}

/// The small road of `generic` (`small_on`): 2 to `SMALL_SORT_ROWS` records, all ordered, no word ordered through fmgr, the
/// first sort word a text key whose images order by their bytes (collation 0: `head_word`'s, a bpchar's without its
/// trailing blanks). Each record's image is read once for its (NULL rank, head) words (`heads`'s, the head from the
/// image's first eight bytes), and the records sort over those with `generic_cmp` where two tie: the comparator orders
/// distinct records strictly (the key words end it) and the words agree with it where they differ, so the order is the
/// comparator's own. `None`, nothing moved: another word, or an image with no head (a compressed kept copy). Out of line:
/// its stack arrays stay off `generic`'s frame.
#[inline(never)]
unsafe fn small_generic(ctx: *mut Ctx, ptrs: &mut [*const u8], layout: &RecordLayout, keys: &[u64], procs: &[u64], nk: Option<usize>, err: &core::cell::Cell<i64>) -> Option<i64> {
    let (n, w) = (ptrs.len(), *keys.first()?);
    if n < 2 || n > SMALL_SORT_ROWS || w & (SK_TEXT | SK_SLOT) != SK_TEXT || (w >> SK_TEXT_COLL_SHIFT) as u32 != 0 || w & (SK_HEAP | SK_CMP) == SK_HEAP | SK_CMP {
        return None;
    }
    let mut dec = [(0u8, 0u64, 0u32); SMALL_SORT_ROWS];
    for (i, d) in dec[..n].iter_mut().enumerate() {
        let r = Rec::at(ptrs[i] as *mut u8, layout);
        let img = if sort_cell(&r, layout, w, nk).1 { text_image(ctx, &r, w, err) } else { None };
        *d = match img {
            Some(Img::Bytes(b)) => {
                let b = if w & SK_BPCHAR != 0 { bc_truelen(b) } else { b };
                let mut h = [0u8; 8];
                h[..b.len().min(8)].copy_from_slice(&b[..b.len().min(8)]);
                let h = u64::from_be_bytes(h);
                (1, if w & SK_DESC != 0 { !h } else { h }, i as u32)
            }
            Some(Img::Packed(_)) => return None,
            None => (if w & SK_NULLS_FIRST != 0 { 0 } else { 2 }, 0, i as u32),
        };
    }
    let dec = &mut dec[..n];
    dec.sort_unstable_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)).then_with(|| generic_cmp(ctx, ptrs[a.2 as usize], ptrs[b.2 as usize], layout, keys, procs, nk, err)));
    let mut old = [core::ptr::null::<u8>(); SMALL_SORT_ROWS];
    old[..n].copy_from_slice(ptrs);
    for (p, d) in ptrs.iter_mut().zip(dec.iter()) {
        *p = old[d.2 as usize];
    }
    Some(err.get())
}

/// CAND-25: per record, text word `w`'s place in `generic_taken_cmp`'s order as two words: its NULL rank (0 a NULL first,
/// 1 an image, 2 a NULL last) and its image's head (`Taken::head`; its complement under DESC). Where two records' words
/// differ they order the records as the comparator does (heads that differ order the images, `taken_cmp`); where they tie,
/// the comparator decides. `None`, so every compare takes the comparator from the first, unless the word orders by bytes
/// by its collation's property (`head_word`: collation 0, never a bpchar or a kept copy's `copy_cmp`), and when some image
/// has no head (a compressed kept copy): the words would not stand for the order.
unsafe fn heads(ctx: *mut Ctx, ptrs: &[*const u8], layout: &RecordLayout, w: u64, nk: Option<usize>, memo: &Memo<'_>, err: &core::cell::Cell<i64>) -> Result<Option<Work<(u8, u64)>>, i64> {
    if !head_word(w) {
        return Ok(None);
    }
    let mut whole = true;
    let out = Work::from_fn(ctx, ptrs.len(), |i| {
        if !whole {
            return (0, 0);
        }
        let r = Rec::at(ptrs[i] as *mut u8, layout);
        let t = if sort_cell(&r, layout, w, nk).1 { Some(take(ctx, &r, w, (memo, i), 0, err)) } else { None };
        match t.filter(|t| t.img.is_some()) {
            Some(Taken { head: Some(h), .. }) => (1u8, if w & SK_DESC != 0 { !h } else { h }),
            Some(_) => {
                whole = false;
                (0, 0)
            }
            None => (if w & SK_NULLS_FIRST != 0 { 0 } else { 2 }, 0),
        }
    })?;
    Ok(whole.then_some(out))
}

/// CAND-25: whether text word `w`'s heads tell the records apart, read from an even sample of at most `HEAD_SAMPLE` of
/// them (their images taken into `memo`): false when an image has no head (a compressed kept copy) or more than half of
/// the sampled neighbours tie on their (NULL rank, head) words. Keys that share a long prefix (or are mostly NULL) leave
/// nearly every compare to the comparator, so the decoration and the index sort are cost with no saving; PostgreSQL gives
/// up its abbreviated keys the same way (varlena.c `varstr_abbrev_abort`). Heads past a shared prefix (`Memo::skip`) must
/// also be at least half distinct in the sample: a few values there (ties of one in three neighbours) put the sort's
/// later compares on equal heads, where the road costs more than the comparator alone (W8: 1.09-1.15x).
unsafe fn heads_order(ctx: *mut Ctx, ptrs: &[*const u8], layout: &RecordLayout, w: u64, nk: Option<usize>, memo: &Memo<'_>, err: &core::cell::Cell<i64>) -> bool {
    let (mut prev, mut pairs, mut ties) = (None, 0usize, 0usize);
    // E-kern: the sample (at most `SAMPLE_MAX` records) on the stack, no allocation
    let (mut seen, mut ns) = ([(0u8, 0u64); SAMPLE_MAX], 0usize);
    for i in (0..ptrs.len()).step_by((ptrs.len() / HEAD_SAMPLE).max(1)) {
        let r = Rec::at(ptrs[i] as *mut u8, layout);
        let t = if sort_cell(&r, layout, w, nk).1 { Some(take(ctx, &r, w, (memo, i), 0, err)) } else { None };
        let word = match t.filter(|t| t.img.is_some()) {
            Some(Taken { head: Some(h), .. }) => (1u8, h),
            Some(_) => return false,
            None => (if w & SK_NULLS_FIRST != 0 { 0 } else { 2 }, 0),
        };
        if let Some(p) = prev {
            pairs += 1;
            ties += usize::from(p == word);
        }
        prev = Some(word);
        if memo.skip > 0 {
            if let Some(slot) = seen.get_mut(ns) {
                (*slot, ns) = (word, ns + 1);
            }
        }
    }
    if memo.skip > 0 {
        let seen = &mut seen[..ns];
        seen.sort_unstable();
        if 2 * distinct(seen) < ns {
            return false;
        }
    }
    2 * ties <= pairs
}

/// `heads_order`'s sample: enough neighbours to tell keys that mostly share a head from keys that mostly do not, few
/// enough to cost nothing beside the sort.
const HEAD_SAMPLE: usize = 256;

/// E-kern: the most records a `HEAD_SAMPLE` sample visits (a step of `len / HEAD_SAMPLE`, at least 1: fewer than
/// `2 * HEAD_SAMPLE`), the bound of `prefix_spread`'s and `heads_order`'s stack arrays.
const SAMPLE_MAX: usize = 2 * HEAD_SAMPLE;

/// agg_shop-0033 perf: `bounded_sort` where `top` (no SQL / PL comparator word: TOTAL-ORDER's `stop` is a built-in
/// comparator reached through fmgr) and `n` is short of `v.len()`: `v`'s first `n` in `cmp`'s order, the rest after them in
/// any order, never panicking on an order that is not total. A max-heap of the `n` least so far (`sift_down`: compares and
/// swaps alone, no std select, which may panic on such an order), each later element meeting its top once and entering
/// only where it orders before it; then PostgreSQL's qsort over the `n`. A failed compare (`stop`'s flag) ends it at once,
/// leaving a permutation. The whole-array sort where PostgreSQL ran its qsort over every row cost a group emit under a
/// counted Limit `n log n` fmgr compares (901 records for LIMIT 1 over an 8-key group: 0.84 ms) where this takes about one
/// per record.
fn top_sort<T: Copy>(v: &mut [T], n: usize, stop: Option<&core::cell::Cell<i64>>, top: bool, mut cmp: impl FnMut(&T, &T) -> Ordering) {
    let Some(err) = stop.filter(|_| top && n < v.len()) else {
        return bounded_sort(v, n, stop, cmp);
    };
    if n == 0 {
        return;
    }
    for i in (0..n / 2).rev() {
        sift_down(&mut v[..n], i, err, &mut cmp);
    }
    for j in n..v.len() {
        if err.get() != 0 {
            return;
        }
        if cmp(&v[j], &v[0]) == Ordering::Less {
            v.swap(0, j);
            sift_down(&mut v[..n], 0, err, &mut cmp);
        }
    }
    if err.get() != 0 {
        return;
    }
    let _ = gravity_fn::pg_qsort::pg_qsort_arg(&mut v[..n], |a, b| {
        let o = cmp(a, b);
        if err.get() != 0 { Err(()) } else { Ok(o as i32) }
    });
}

/// `top_sort`'s max-heap step: element `i` of `h` moves down past every child that orders after it.
fn sift_down<T: Copy>(h: &mut [T], mut i: usize, err: &core::cell::Cell<i64>, cmp: &mut impl FnMut(&T, &T) -> Ordering) {
    loop {
        let l = 2 * i + 1;
        if l >= h.len() || err.get() != 0 {
            return;
        }
        let c = if l + 1 < h.len() && cmp(&h[l + 1], &h[l]) == Ordering::Greater { l + 1 } else { l };
        if err.get() != 0 || cmp(&h[c], &h[i]) != Ordering::Greater {
            return;
        }
        h.swap(i, c);
        i = c;
    }
}

/// `v`'s first `n` in `cmp`'s order, the rest after them in any order (every element when `n` is `v.len()`). `stop`
/// (spicmp: a SQL / PL comparator's sort, its failure flag; TOTAL-ORDER: any sort through a comparator): the whole of `v`
/// by PostgreSQL's qsort, stopping at the first failed compare (the flag set, every later pair `Equal`), where std's
/// select / sort may panic on that order or on any order that is not total (XX000).
fn bounded_sort<T: Copy>(v: &mut [T], n: usize, stop: Option<&core::cell::Cell<i64>>, mut cmp: impl FnMut(&T, &T) -> Ordering) {
    if let Some(err) = stop {
        let _ = gravity_fn::pg_qsort::pg_qsort_arg(v, |a, b| {
            let o = cmp(a, b);
            if err.get() != 0 { Err(()) } else { Ok(o as i32) }
        });
    } else if n < v.len() {
        if n > 0 {
            v.select_nth_unstable_by(n - 1, &mut cmp);
            v[..n].sort_unstable_by(&mut cmp);
        }
    } else {
        v.sort_unstable_by(cmp);
    }
}

/// Two records under one slot sort word `w` (`SK_SLOT`, neither `SK_TEXT` nor `SK_CMP`), as `agg_sort` orders them: a
/// numeric sum in numeric_cmp's order (an OVERFLOW sum first in either direction), an average by its quotient, any other
/// slot by its cell; a NULL where `SK_NULLS_FIRST` puts it, `SK_DESC` reversing two values. The one order of a final
/// over groups: the emit's sort and the owner's kept top (exec table/raw/owner.rs) both read it.
/// # Safety
/// `ra`, `rb` are live records of `layout` and `w`'s low byte one of its slots.
#[inline]
pub unsafe fn slot_order(ra: &Rec<'_>, rb: &Rec<'_>, layout: &RecordLayout, w: u64) -> Ordering {
    let j = (w & 0xff) as usize;
    let ((va, oa), (vb, ob)) = (slot_cell(ra, layout, w), slot_cell(rb, layout, w));
    let kind = layout.slot_kinds()[j];
    let num = num_sum(kind);
    let (ka, kb) = if num { (oa.then(|| num_order(ra, j, kind)), ob.then(|| num_order(rb, j, kind))) } else { (None, None) };
    match (ka.is_some_and(|k| k.is_none()), kb.is_some_and(|k| k.is_none())) {
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }
    let ord = match (oa, ob) {
        (true, true) if num => ka.flatten().cmp(&kb.flatten()),
        (true, true) if kind == SlotKind::AvgI128 => avg_cmp(ra.avg(j), rb.avg(j)),
        (true, true) => va.cmp(&vb),
        (false, false) => Ordering::Equal,
        (false, true) => if w & SK_NULLS_FIRST != 0 { Ordering::Less } else { Ordering::Greater },
        (true, false) => if w & SK_NULLS_FIRST != 0 { Ordering::Greater } else { Ordering::Less },
    };
    if w & SK_DESC != 0 && oa && ob { ord.reverse() } else { ord }
}

/// Two records under the sort words, then the key words ascending (`agg_sort`'s order).
#[allow(clippy::too_many_arguments)]
unsafe fn generic_cmp(ctx: *mut Ctx, a: *const u8, b: *const u8, layout: &RecordLayout, keys: &[u64], procs: &[u64], nk: Option<usize>, err: &core::cell::Cell<i64>) -> Ordering {
    match prefix_cmp(ctx, a, b, layout, keys, procs, nk, err) {
        Ordering::Equal => {}
        o => return o,
    }
    let (ra, rb) = (Rec::at(a as *mut u8, layout), Rec::at(b as *mut u8, layout));
    for k in 0..layout.nkeys() {
        let ord = (ra.key(k) as i64).cmp(&(rb.key(k) as i64));
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// Two records under the sort words `keys` alone (their comparators `procs`), no key-word tie: `generic_cmp`'s first
/// step, and D3336's cut on a leading run of the words (`merge_runs_part`).
#[allow(clippy::too_many_arguments)]
unsafe fn prefix_cmp(ctx: *mut Ctx, a: *const u8, b: *const u8, layout: &RecordLayout, keys: &[u64], procs: &[u64], nk: Option<usize>, err: &core::cell::Cell<i64>) -> Ordering {
    let (ra, rb) = (Rec::at(a as *mut u8, layout), Rec::at(b as *mut u8, layout));
    for (i, &w) in keys.iter().enumerate() {
        if w & (SK_SLOT | SK_TEXT | SK_CMP) == SK_SLOT {
            let ord = slot_order(&ra, &rb, layout, w);
            if ord != Ordering::Equal {
                return ord;
            }
            continue;
        }
        let j = if w & SK_SLOT != 0 { (w & 0xff) as usize } else { sk_key_word(w) }; // CAP-26: a key word past 255
        let ((va, oa), (vb, ob)) = (sort_cell(&ra, layout, w, nk), sort_cell(&rb, layout, w, nk));
        let avg = w & SK_SLOT != 0 && layout.slot_kinds()[j] == SlotKind::AvgI128;
        let text = w & SK_TEXT != 0;
        // a text key's images, each rendered once per compare: its NULL test and its compared bytes (`text_cmp`)
        let img = |o: bool, r: &Rec<'_>| if text && o { text_image(ctx, r, w, err) } else { None };
        let (ia, ib) = (img(oa, &ra), img(ob, &rb));
        let (oa, ob) = if text { (ia.is_some(), ib.is_some()) } else { (oa, ob) };
        // M6 L0-n: a numeric sum in numeric_cmp's order; an OVERFLOW sum (its final raises 22003, D299) sorts before every
        // group in either direction, so a top keeps it and its emit raises, as the whole emit would
        let num = w & SK_SLOT != 0 && num_sum(layout.slot_kinds()[j]);
        let (ka, kb) = if num { (oa.then(|| num_order(&ra, j, layout.slot_kinds()[j])), ob.then(|| num_order(&rb, j, layout.slot_kinds()[j]))) } else { (None, None) };
        match (ka.is_some_and(|k| k.is_none()), kb.is_some_and(|k| k.is_none())) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        let ord = match (oa, ob) {
            (true, true) if num => ka.flatten().cmp(&kb.flatten()),
            (true, true) if avg => avg_cmp(ra.avg(j), rb.avg(j)),
            (true, true) if text && w & (SK_HEAP | SK_CMP) == SK_HEAP | SK_CMP => copy_cmp(&ra, &rb, w, procs[i], err),
            (true, true) if sk_key_cmp(w) => word_cmp(ra.key(j) as u32 as u64, rb.key(j) as u32 as u64, procs[i], err),
            (true, true) if sk_slot_cmp(w) => word_cmp(va as u64, vb as u64, procs[i], err),
            (true, true) if text => match (&ia, &ib) {
                (Some(x), Some(y)) => text_cmp(x, y, w, err),
                _ => Ordering::Equal,
            },
            (true, true) => va.cmp(&vb),
            (false, false) => Ordering::Equal,
            (false, true) => if w & SK_NULLS_FIRST != 0 { Ordering::Less } else { Ordering::Greater },
            (true, false) => if w & SK_NULLS_FIRST != 0 { Ordering::Greater } else { Ordering::Less },
        };
        let ord = if w & SK_DESC != 0 && oa && ob { ord.reverse() } else { ord };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// Two records under the sort words, then the key words ascending (`agg_sort`'s order). `memo`: `generic`'s, with the
/// two records' indices in it.
#[allow(clippy::too_many_arguments)]
unsafe fn generic_taken_cmp(ctx: *mut Ctx, a: *const u8, b: *const u8, layout: &RecordLayout, keys: &[u64], procs: &[u64], nk: Option<usize>, memo: (&Memo<'_>, usize, usize), err: &core::cell::Cell<i64>) -> Ordering {
    let (ra, rb) = (Rec::at(a as *mut u8, layout), Rec::at(b as *mut u8, layout));
    let mut col = 0usize;
    for (i, &w) in keys.iter().enumerate() {
        let j = if w & SK_SLOT != 0 { (w & 0xff) as usize } else { sk_key_word(w) }; // CAP-26: a key word past 255
        let ((va, oa), (vb, ob)) = (sort_cell(&ra, layout, w, nk), sort_cell(&rb, layout, w, nk));
        let avg = w & SK_SLOT != 0 && layout.slot_kinds()[j] == SlotKind::AvgI128;
        let text = w & SK_TEXT != 0;
        let (ta, tb) = if text {
            col += 1;
            (oa.then(|| take(ctx, &ra, w, (memo.0, memo.1), col - 1, err)), ob.then(|| take(ctx, &rb, w, (memo.0, memo.2), col - 1, err)))
        } else {
            (None, None)
        };
        let (oa, ob) = if text { (ta.is_some_and(|t| t.img.is_some()), tb.is_some_and(|t| t.img.is_some())) } else { (oa, ob) };
        // M6 L0-n: a numeric sum in numeric_cmp's order; an OVERFLOW sum (its final raises 22003, D299) sorts before every
        // group in either direction, so a top keeps it and its emit raises, as the whole emit would
        let num = w & SK_SLOT != 0 && num_sum(layout.slot_kinds()[j]);
        let (ka, kb) = if num { (oa.then(|| num_order(&ra, j, layout.slot_kinds()[j])), ob.then(|| num_order(&rb, j, layout.slot_kinds()[j]))) } else { (None, None) };
        match (ka.is_some_and(|k| k.is_none()), kb.is_some_and(|k| k.is_none())) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        let ord = match (oa, ob) {
            (true, true) if num => ka.flatten().cmp(&kb.flatten()),
            (true, true) if avg => avg_cmp(ra.avg(j), rb.avg(j)),
            (true, true) if text && w & (SK_HEAP | SK_CMP) == SK_HEAP | SK_CMP => copy_cmp(&ra, &rb, w, procs[i], err),
            (true, true) if sk_key_cmp(w) => word_cmp(ra.key(j) as u32 as u64, rb.key(j) as u32 as u64, procs[i], err),
            (true, true) if sk_slot_cmp(w) => word_cmp(va as u64, vb as u64, procs[i], err),
            (true, true) if text => ta.zip(tb).map_or(Ordering::Equal, |(x, y)| taken_cmp(x, y, w, err)),
            (true, true) => va.cmp(&vb),
            (false, false) => Ordering::Equal,
            (false, true) => if w & SK_NULLS_FIRST != 0 { Ordering::Less } else { Ordering::Greater },
            (true, false) => if w & SK_NULLS_FIRST != 0 { Ordering::Greater } else { Ordering::Less },
        };
        let ord = if w & SK_DESC != 0 && oa && ob { ord.reverse() } else { ord };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    for k in 0..layout.nkeys() {
        let ord = (ra.key(k) as i64).cmp(&(rb.key(k) as i64));
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// V4R-190: record `r`'s order as words: per sort word its NULL rank (0 a NULL first, 1 a value, 2 a NULL last) and its
/// value (`!v` under DESC: the reverse order, no overflow; 0 for a NULL, NULLs tie), then the key words.
unsafe fn decorate(out: &mut [i64], r: &Rec<'_>, layout: &RecordLayout, keys: &[u64], nk: Option<usize>) {
    for (i, &w) in keys.iter().enumerate() {
        (out[2 * i], out[2 * i + 1]) = word_pair(r, layout, w, nk);
    }
    for k in 0..layout.nkeys() {
        out[2 * keys.len() + k] = r.key(k) as i64;
    }
}

/// Sort word `w`'s two words of record `r`'s row (`decorate`).
#[inline(always)]
unsafe fn word_pair(r: &Rec<'_>, layout: &RecordLayout, w: u64, nk: Option<usize>) -> (i64, i64) {
    if num_marked(w) {
        return num_pair(r, layout, w);
    }
    let (v, ok) = sort_cell(r, layout, w, nk);
    match (ok, w & SK_NULLS_FIRST != 0) {
        (true, _) => (1, if w & SK_DESC != 0 { !v } else { v }),
        (false, true) => (0, 0),
        (false, false) => (2, 0),
    }
}

/// D3141: numeric-sum word `w`'s (`SK_NUM_WORD`) two words of record `r`'s row, `generic_cmp`'s order: the OVERFLOW
/// sentinel first in either direction (its final raises), a NULL by `SK_NULLS_FIRST`, a sum by `num_fit` (`!v` DESC).
#[inline]
unsafe fn num_pair(r: &Rec<'_>, layout: &RecordLayout, w: u64) -> (i64, i64) {
    let j = (w & 0xff) as usize;
    if !r.valid(j) {
        return if w & SK_NULLS_FIRST != 0 { (0, 0) } else { (2, 0) };
    }
    match num_order(r, j, layout.slot_kinds()[j]) {
        None => (-1, 0),
        Some(o) => {
            let v = num_fit(o).unwrap_or(0); // num_words marks a word only where every record's sum fits
            (1, if w & SK_DESC != 0 { !v } else { v })
        }
    }
}

/// NEW-35: record `r`'s row against row `t` in the rows' order, each word taken only until one differs (where the
/// leading sort word decides, one cell per record); `sort_only`: the sort words alone.
#[inline(always)]
unsafe fn row_cmp(r: &Rec<'_>, layout: &RecordLayout, keys: &[u64], nk: Option<usize>, t: &[i64], sort_only: bool) -> Ordering {
    for (i, &w) in keys.iter().enumerate() {
        let o = word_pair(r, layout, w, nk).cmp(&(t[2 * i], t[2 * i + 1]));
        if o != Ordering::Equal {
            return o;
        }
    }
    if sort_only {
        return Ordering::Equal;
    }
    for k in 0..layout.nkeys() {
        let o = (r.key(k) as i64).cmp(&t[2 * keys.len() + k]);
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

/// NEW-35: `buf`'s least `n` rows (`live`'s slots) kept, the other slots freed; the `n`-th's slot.
fn keep_least(live: &mut Vec<u32>, free: &mut Vec<u32>, rows: &[i64], stride: usize, n: usize) -> u32 {
    let row = |s: u32| &rows[s as usize * stride..(s as usize + 1) * stride];
    live.select_nth_unstable_by(n - 1, |a, b| row(*a).cmp(row(*b)));
    free.extend(live.drain(n..)); // alloc: Node charged lo (select_decorated's guards: within their reserved cap)
    live[n - 1]
}

/// NEW-35: `sort_decorated`'s bound over more than `3n` records, with no row per record. A buffer of `2n` rows holds the
/// least records met so far; full, it keeps its least `n` (`keep_least`), and a later record enters only where its row
/// orders before the kept `n`-th's (`row_cmp`). A record left out orders after `n` kept ones, so the buffer's least `n`
/// are the whole sort's. Per record one compare, read lazily; a selection over the buffer per `n` entries at most, so
/// records that arrive in the sort's order cost about today's decorate and select. WITH TIES (`tie_block`'s lead): one
/// more pass over the rest takes the records whose sort words equal the `n`-th's and the least record past them. The
/// lead first, in order, the rest after it in any order; returns the lead's length.
unsafe fn select_decorated(ctx: *mut Ctx, ptrs: &mut [*const u8], layout: &RecordLayout, keys: &[u64], nk: Option<usize>, n: usize, ties: bool) -> Result<usize, i64> {
    use gravity_fn::arena::{AVec, ChargedVec, Kind, OpArena, Refused};
    let stride = 2 * keys.len() + layout.nkeys();
    let cap = 2 * n;
    // a buffer row and its record index are written when the row is taken from `free`, before any read
    let mut rows = Work::<i64>::block(ctx, cap * stride)?;
    let mut of = Work::<u32>::block(ctx, cap)?;
    // the buffer's slot lists (2n u32 each, n the LIMIT: data-sized), charged as this call's scratch arena below is: from
    // ARENA_SCRATCH_MIN bytes on to the breaker the emit's pipeline owns, else uncharged (`Work::scratch`'s floor); filled
    // within their reserved `cap`, so the guards' drop is their one re-sync
    let (lo, lk) = match 2 * cap * core::mem::size_of::<u32>() >= gravity_abi::program::ARENA_SCRATCH_MIN { true => OpArena::own_owner_kind(ctx).map_or((None, Kind::Sort), |(o, k)| (Some(o), k)), false => (None, Kind::Sort) };
    let (mut live_cv, mut free_cv) = (ChargedVec::<u32>::new(lo, lk), ChargedVec::<u32>::new(lo, lk));
    let (mut live, mut free) = (live_cv.vec_mut(), free_cv.vec_mut());
    live.reserve_exact(cap); // alloc: Node charged lo
    free.reserve_exact(cap); // alloc: Node charged lo
    free.extend((0..cap as u32).rev()); // alloc: Node charged lo
    let mut cut: Option<u32> = None;
    for (i, &p) in ptrs.iter().enumerate() {
        let r = Rec::at(p as *mut u8, layout);
        if let Some(t) = cut {
            if row_cmp(&r, layout, keys, nk, &rows[t as usize * stride..(t as usize + 1) * stride], false) != Ordering::Less {
                continue;
            }
        }
        let s = free.pop().expect("NEW-35: a free buffer row") as usize;
        decorate(&mut rows[s * stride..(s + 1) * stride], &r, layout, keys, nk);
        of[s] = i as u32;
        live.push(s as u32); // alloc: Node charged lo (within the reserved cap)
        if live.len() == cap {
            cut = Some(keep_least(&mut live, &mut free, &rows, stride, n));
        }
    }
    if live.len() > n {
        keep_least(&mut live, &mut free, &rows, stride, n);
    }
    let row = |s: u32| &rows[s as usize * stride..(s as usize + 1) * stride];
    live.sort_unstable_by(|a, b| row(*a).cmp(row(*b)));
    // the per-record bitmaps and the tied records' lists: one scratch arena of this call under the breaker the emit's
    // pipeline feeds (the allocator's road, charged to it); a refused carve is the statement's out of memory. As LEDGER2
    // step 5's `Work::scratch`, a cut whose lists stay under `ARENA_SCRATCH_MIN` asks no owner (uncharged: where a `Vec` was)
    let most = if ties { ptrs.len().saturating_mul(14 + 8 * stride) } else { ptrs.len() / 8 + 12 * n };
    let (o, k) = match most >= gravity_abi::program::ARENA_SCRATCH_MIN { true => OpArena::own_owner_kind(ctx).map_or((None, Kind::Sort), |(o, k)| (Some(o), k)), false => (None, Kind::Sort) };
    let a = OpArena::scratch_of(o, k);
    let refused = |r: Refused| crate::ops::agg::trans::entries::failed(crate::ops::agg::trans::entries::out_of_memory(r.bytes as usize));
    let (mut tied, mut order, mut past) = (AVec::<u32>::new(), AVec::<u32>::new(), None::<(u32, ChargedVec<i64>)>);
    if ties {
        let last = row(live[n - 1]);
        let mut taken = AVec::<u64>::new();
        taken.resize(&a, ptrs.len().div_ceil(64), 0).map_err(refused)?;
        let bits = taken.as_mut_slice(&a);
        for &s in live.iter() {
            let i = of[s as usize];
            bits[i as usize / 64] |= 1 << (i % 64);
        }
        for (i, &p) in ptrs.iter().enumerate() {
            if bits[i / 64] >> (i % 64) & 1 != 0 {
                continue;
            }
            let r = Rec::at(p as *mut u8, layout);
            match row_cmp(&r, layout, keys, nk, last, true) {
                Ordering::Equal => tied.push(&a, i as u32).map_err(refused)?,
                Ordering::Greater if past.as_ref().is_none_or(|(_, t)| row_cmp(&r, layout, keys, nk, t, false) == Ordering::Less) => {
                    // E-kern: the record past the lead (one row, `stride` words), charged as the scratch arena `a` is: `(o, k)`
                    let mut t = past.take().map_or_else(
                        || {
                            let mut t = ChargedVec::<i64>::new(o, k);
                            t.with_vec(|v| v.resize(stride, 0)); // alloc: Node charged o
                            t
                        },
                        |(_, t)| t,
                    );
                    decorate(&mut t, &r, layout, keys, nk);
                    past = Some((i as u32, t));
                }
                _ => {}
            }
        }
        let tied = tied.as_slice(&a);
        let mut trows = AVec::<i64>::new();
        trows.resize(&a, tied.len() * stride, 0).map_err(refused)?;
        let trows = trows.as_mut_slice(&a);
        for (j, &i) in tied.iter().enumerate() {
            decorate(&mut trows[j * stride..(j + 1) * stride], &Rec::at(ptrs[i as usize] as *mut u8, layout), layout, keys, nk);
        }
        order.reserve(&a, tied.len()).map_err(refused)?;
        for j in 0..tied.len() as u32 {
            order.push(&a, j).map_err(refused)?;
        }
        let trow = |j: u32| &trows[j as usize * stride..(j as usize + 1) * stride];
        order.as_mut_slice(&a).sort_unstable_by(|x, y| trow(*x).cmp(trow(*y)));
    }
    // the lead: the first n in order, then (WITH TIES) the tied records in order and the least record past them
    let (tied, order) = (tied.as_slice(&a), order.as_slice(&a));
    let mut lead = AVec::<u32>::with_capacity(&a, live.len() + order.len() + 1).map_err(refused)?;
    for &s in live.iter() {
        lead.push(&a, of[s as usize]).map_err(refused)?;
    }
    for &j in order {
        lead.push(&a, tied[j as usize]).map_err(refused)?;
    }
    if let Some((i, _)) = past {
        lead.push(&a, i).map_err(refused)?;
    }
    let lead = lead.as_slice(&a);
    let mut firsts = AVec::<*const u8>::with_capacity(&a, lead.len()).map_err(refused)?;
    for &i in lead {
        firsts.push(&a, ptrs[i as usize]).map_err(refused)?;
    }
    let firsts = firsts.as_slice(&a);
    // the lead's records to the front: each front slot whose record the lead does not hold trades places with the next
    // record past the front that it does (a bitmap of the lead, read a word at a time: O(m / 64 + lead), no sort of the
    // lead's indices), then the front in the lead's order
    let l = firsts.len();
    let mut held = AVec::<u64>::new();
    held.resize(&a, ptrs.len().div_ceil(64), 0).map_err(refused)?;
    let held = held.as_mut_slice(&a);
    for &i in lead {
        held[i as usize / 64] |= 1 << (i % 64);
    }
    let mut src = l;
    for j in 0..l {
        if held[j / 64] >> (j % 64) & 1 == 0 {
            // as many lead records lie past the front as front slots it does not hold, so one is left at or past src
            loop {
                let w = held[src / 64] >> (src % 64);
                if w != 0 {
                    src += w.trailing_zeros() as usize;
                    break;
                }
                src = (src / 64 + 1) * 64;
            }
            ptrs.swap(j, src);
            src += 1;
        }
    }
    ptrs[..l].copy_from_slice(firsts);
    Ok(l)
}

/// The records ordered through their word rows; the first `n` in order, the rest after them in any order.
unsafe fn sort_decorated(ctx: *mut Ctx, ptrs: &mut [*const u8], layout: &RecordLayout, keys: &[u64], nk: Option<usize>, n: usize, ties: bool) -> Result<usize, i64> {
    let m = ptrs.len();
    // NEW-35: the buffer road's own cost past today's is its last selection, over n rows or more; it pays only where the
    // cut can leave out at least n records, past its 2n-row buffer: m > 3n (derived from the two roads' work, not fitted)
    if n > 0 && 3 * n < m {
        return select_decorated(ctx, ptrs, layout, keys, nk, n, ties);
    }
    let mut n = n;
    let stride = 2 * keys.len() + layout.nkeys();
    // The emit's one sort, sized by the records' own count; `decorate` writes each row whole before any compare reads it.
    let mut idx = Work::from_fn(ctx, m, |i| i as u32)?;
    {
        let mut words = Work::<i64>::block(ctx, m * stride)?;
        for (i, &p) in ptrs.iter().enumerate() {
            decorate(&mut words[i * stride..(i + 1) * stride], &Rec::at(p as *mut u8, layout), layout, keys, nk);
        }
        let row = |i: u32| &words[i as usize * stride..(i as usize + 1) * stride];
        let cmp = |a: &u32, b: &u32| row(*a).cmp(row(*b));
        if n < m {
            if n > 0 {
                idx.select_nth_unstable_by(n - 1, cmp);
                if ties {
                    n = tie_block(&mut idx, n, 2 * keys.len(), &row, cmp);
                }
                idx[..n].sort_unstable_by(cmp);
            }
        } else {
            idx.sort_unstable_by(cmp);
        }
    }
    let old = Work::from_fn(ctx, m, |i| ptrs[i])?;
    for (d, &i) in ptrs.iter_mut().zip(idx.iter()) {
        *d = old[i as usize];
    }
    Ok(n.min(m))
}

/// RUNS-18 (`FORM_TIES`): past the first `n` (selected: every record after them orders after the `n`-th), the records
/// whose sort words (the first `w` words of each row: a NULL rank and a value per sort word) equal the `n`-th's, then the
/// least of the rest; returns how many lead `idx`. The `n`-th's ties order right after it in the total order (only their
/// key words tell them apart) and before every other record, so the lead is the one sort's first records.
fn tie_block<'r>(idx: &mut [u32], n: usize, w: usize, row: &impl Fn(u32) -> &'r [i64], cmp: impl Fn(&u32, &u32) -> Ordering) -> usize {
    let last = &row(idx[n - 1])[..w];
    let mut t = n;
    for j in n..idx.len() {
        if &row(idx[j])[..w] == last {
            idx.swap(t, j);
            t += 1;
        }
    }
    if let Some(j) = (t..idx.len()).min_by(|&a, &b| cmp(&idx[a], &idx[b])) {
        idx.swap(t, j);
        t += 1;
    }
    t
}

/// V4R-190's crate test seam: the generic comparator's order of `ptrs`, for a test that pins the decorated order to it.
#[doc(hidden)]
pub unsafe fn agg_sort_generic(ctx: *mut Ctx, st: *mut u8, desc: *const u8) -> i64 {
    let st = st as *mut EmitState;
    let layout = desc_layout(desc);
    let nsort = nsort_of(desc);
    let keys = core::slice::from_raw_parts(desc.add(DESC_SORT) as *const u64, nsort);
    let nkw = nullkey_of(*(desc.add(DESC_FORM) as *const u64));
    let (ptrs, _wide) = Narrow::of((*st).ptrs.ptr(), (*st).kept as usize);
    let n = ptrs.len();
    generic(ctx, ptrs, &layout, keys, procs(desc, nsort), (nkw != 0).then(|| nkw as usize - 1), n)
}

#[cfg(test)]
mod l0n_num_order_tests {
    use super::*;

    fn i(v: i128) -> [u64; 4] {
        [v as u64, (v as u128 >> 64) as u64, 0, 0]
    }

    fn sent(c: u64) -> [u64; 4] {
        [c, NUM_SENT_HI as u64, 0, 0]
    }

    #[test]
    fn numeric_sums_order_as_numeric_cmp() {
        let k = |w| num_key(SlotKind::SumNum, w);
        let mut v = [k(sent(1)), k(i(5)), k(sent(3)), k(i(-7)), k(sent(2)), k(i(0)), k(i(i128::MAX >> 2)), k(i(-(i128::MAX >> 2)))];
        v.sort();
        assert_eq!(v, [k(sent(3)), k(i(-(i128::MAX >> 2))), k(i(-7)), k(i(0)), k(i(5)), k(i(i128::MAX >> 2)), k(sent(2)), k(sent(1))]);
        assert_eq!(k(sent(4)), None); // OVERFLOW: sorted first by agg_sort, its final raises
        // sum(int8): an i128 with no sentinel (its high word may be i64::MIN for a large negative sum)
        assert_eq!(num_key(SlotKind::SumI128, i(i128::MIN + 5)), Some((1, -1, (i128::MIN + 5) as u128)));
        assert!(num_key(SlotKind::SumI128, i(-1)) < num_key(SlotKind::SumI128, i(1)));
        // an i256 sum: -2^200 < -1 < 2^130 < +Infinity
        let w = |hi: i128, lo: u128| [lo as u64, (lo >> 64) as u64, hi as u64, (hi as u128 >> 64) as u64];
        let kw = |x| num_key(SlotKind::SumNumWide, x);
        assert!(kw(w(-(1 << 72), 0)) < kw(w(-1, u128::MAX)));
        assert!(kw(w(-1, u128::MAX)) < kw(w(0, 1 << 2)));
        assert!(kw(w(0, 1u128 << 127)) < kw(w(4, 0)));
        assert!(kw(w(4, 0)) < kw([2, 0, 0, NUM_SENT_HI as u64]));
    }
}

#[cfg(test)]
mod d819_avg_order_tests {
    use super::*;

    #[test]
    fn avg_rounds_as_numeric_div_does() {
        assert_eq!(avg_rounded(2_000_000_000_000_000_001, 2), (1_000_000_000_000_000_001, 0)); // 1e18 + 0.5 at scale 0
        assert_eq!(avg_rounded(1, 3), (33_333_333_333_333_333_333, 20)); // 0.33333333333333333333
        assert_eq!(avg_rounded(10, 4), (25_000_000_000_000_000, 16)); // 2.5000000000000000
        assert_eq!(avg_rounded(-10, 4), (-25_000_000_000_000_000, 16));
        assert_eq!(avg_rounded(0, 5), (0, 20));
        assert_eq!(avg_rounded(1, i64::MAX).1, 36);
    }

    #[test]
    fn averages_equal_after_rounding_tie() {
        let (a, b) = ((2_000_000_000_000_000_001i128, 2i64), (1_000_000_000_000_000_001i128, 1i64));
        assert_eq!(avg_cmp(a, b), Ordering::Equal);
        assert_eq!(avg_cmp((1, 3), (10, 4)), Ordering::Less);
        assert_eq!(avg_cmp((-1, 3), (0, 1)), Ordering::Less);
        assert_eq!(avg_cmp((10, 4), (5, 2)), Ordering::Equal); // 2.5000000000000000 = 2.5000000000000000
        assert_eq!(avg_cmp((i64::MAX as i128 * 1000, 1), (1, i64::MAX)), Ordering::Greater); // scale 0 against scale 36
        assert_eq!(avg_cmp((-(i64::MAX as i128) * 1000, 1), (1, i64::MAX)), Ordering::Less);
    }
}

#[cfg(test)]
mod cand25_head_word_tests {
    use super::*;

    #[test]
    fn a_head_stands_for_the_order_only_under_a_byte_ordered_collation() {
        let text = |coll: u64| SK_TEXT | 3 << SK_TEXT_SLOT_SHIFT | coll << SK_TEXT_COLL_SHIFT;
        assert!(head_word(text(0))); // C / POSIX / a C database's default, cleared by the lowering (byte_coll), or no collation
        assert!(head_word(text(0) | SK_HEAP | SK_DESC | SK_NULLS_FIRST)); // a heap copy, either direction
        assert!(head_word(text(0) | SK_HEAP | SK_NAME)); // a name's image, by bytes under C
        assert!(!head_word(text(950))); // an id is no property: a collation the lowering did not clear keeps the comparator
        assert!(!head_word(text(100))); // the database default under a non-C locale
        assert!(!head_word(text(12345))); // ICU / libc / nondeterministic collations
        assert!(!head_word(text(0) | SK_BPCHAR)); // bpchar: trailing blanks go before the compare
        assert!(!head_word(text(0) | SK_HEAP | SK_CMP)); // a kept copy ordered by its comparator (copy_cmp)
    }
}

#[cfg(test)]
mod d1132_gather_window_tests {
    use super::window_clear;

    #[test]
    fn a_window_over_the_gather_array_is_caught() {
        let live = (0x1000, 0x1040);
        assert!(window_clear((0x1040, 0x2000), live)); // the resumed call bumps past the array
        assert!(window_clear((0x8000, 0x9000), live)); // a grown chunk above
        assert!(window_clear((0x0100, 0x1000), live)); // a chunk below
        assert!(!window_clear((0x1000, 0x2000), live)); // the window rewound to the array's start
        assert!(!window_clear((0x0800, 0x2000), live)); // rewound below it
        assert!(!window_clear((0x1020, 0x1030), live)); // inside it
    }
}

#[cfg(test)]
mod spicmp_stop_tests {
    use super::*;

    /// spicmp (USEROP): a SQL / PL comparator's sort (`bounded_sort`'s `stop`) never panics on the order a failed compare
    /// leaves (every later pair `Equal`, the earlier ones true or random), makes no compare after the failure, and leaves
    /// a permutation; with no failure it orders, bound or not.
    #[test]
    fn a_stopping_sort_never_panics_and_stops_at_the_first_failure() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for len in [2usize, 7, 40, 300, 2000] {
            for fail_at in [0usize, 1, 5, 97, usize::MAX] {
                let err = core::cell::Cell::new(0i64);
                let (calls, after) = (core::cell::Cell::new(0usize), core::cell::Cell::new(0usize));
                let mut v: Vec<u32> = (0..len as u32).rev().collect(); // alloc: Exec
                bounded_sort(&mut v, len / 3, Some(&err), |a, b| {
                    if err.get() != 0 {
                        after.set(after.get() + 1);
                        return Ordering::Equal;
                    }
                    calls.set(calls.get() + 1);
                    if calls.get() > fail_at {
                        err.set(ERROR);
                        return Ordering::Equal;
                    }
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    if fail_at == usize::MAX { a.cmp(b) } else { [Ordering::Less, Ordering::Equal, Ordering::Greater][(seed % 3) as usize] }
                });
                assert_eq!(after.get(), 0, "no compare after the failure (len {len}, fail_at {fail_at})");
                let mut s = v.clone();
                s.sort_unstable();
                assert_eq!(s, (0..len as u32).collect::<Vec<u32>>(), "a permutation (len {len})");
                if fail_at == usize::MAX {
                    assert!(v.windows(2).all(|w| w[0] <= w[1]), "ordered with no failure (len {len})");
                }
            }
        }
    }
}

#[cfg(test)]
mod top_sort_tests {
    use super::*;

    /// agg_shop-0033 perf: `top_sort` under a built-in comparator's `stop` puts the first `n` in order and leaves a
    /// permutation, never panics on an order that is not total, and makes no compare after a failure.
    #[test]
    fn a_top_sort_orders_its_first_n_and_never_panics() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for len in [1usize, 2, 7, 40, 901, 3000] {
            for n in [0usize, 1, 2, 40, len.saturating_sub(1), len] {
                for fail_at in [0usize, 3, 500, usize::MAX] {
                    for total in [true, false] {
                        let err = core::cell::Cell::new(0i64);
                        let (calls, after) = (core::cell::Cell::new(0usize), core::cell::Cell::new(0usize));
                        let mut v: Vec<u32> = (0..len as u32).map(|i| i.wrapping_mul(2_654_435_761) % (len as u32).max(1)).collect(); // alloc: Exec
                        let mut want = v.clone();
                        top_sort(&mut v, n, Some(&err), true, |a, b| {
                            if err.get() != 0 {
                                after.set(after.get() + 1);
                                return Ordering::Equal;
                            }
                            calls.set(calls.get() + 1);
                            if calls.get() > fail_at {
                                err.set(ERROR);
                                return Ordering::Equal;
                            }
                            seed ^= seed << 13;
                            seed ^= seed >> 7;
                            seed ^= seed << 17;
                            if total { a.cmp(b) } else { [Ordering::Less, Ordering::Equal, Ordering::Greater][(seed % 3) as usize] }
                        });
                        assert_eq!(after.get(), 0, "no compare after the failure (len {len}, n {n}, fail_at {fail_at})");
                        let mut s = v.clone();
                        s.sort_unstable();
                        want.sort_unstable();
                        assert_eq!(s, want, "a permutation (len {len}, n {n})");
                        if total && fail_at == usize::MAX {
                            let k = n.min(len);
                            assert_eq!(&v[..k], &want[..k], "the first n in order (len {len}, n {n})");
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod cap26_wide_key_word_tests {
    use super::*;

    /// CAP-26: a key word sort word's two index bytes round-trip every key word index and NULL flag index + 1 below the
    /// maxima, keep off every flag a key word reads (slot, text, comparator, direction, NULL placement, float, unsigned,
    /// bpchar, name) and leave a word below 256 as it was (its high bytes 0).
    #[test]
    fn a_key_words_index_and_null_index_round_trip_past_255() {
        for i in [0usize, 1, 255, 256, 300, 4095, u16::MAX as usize] {
            assert_eq!(sk_key_word(sk_key(i)), i);
        }
        for i in [0usize, 1, 255, 256, 1664, u16::MAX as usize] {
            assert_eq!(sk_null_index(sk_null(i)), i);
        }
        let flags = SK_SLOT | SK_TEXT | SK_CMP | SK_NAME | SK_DESC | SK_NULLS_FIRST | SK_FLOAT | SK_UNSIGNED | SK_BPCHAR | SK_HEAP | 0xff << SK_TEXT_SITE_SHIFT;
        assert!(sk_fits(u16::MAX as usize, u16::MAX as usize) && !sk_fits(1 << 16, 0) && !sk_fits(0, 1 << 16));
        assert_eq!((sk_key(u16::MAX as usize) | sk_null(u16::MAX as usize)) & flags, 0);
        assert_eq!(sk_key(200) | sk_null(7), 200 | 7 << SK_NULL_SHIFT, "below 256 the word is as before");
        let w = sk_key(300) | sk_null(301) | SK_DESC | SK_CMP;
        assert!(sk_key_cmp(w) && sk_key_word(w) == 300 && sk_null_index(w) == 301);
    }
}

#[cfg(test)]
mod small_generic_tests {
    use super::*;
    use gravity_abi::breaker::KeyKind;

    /// Key word 0: the record's own word (the last tie-break); slots 0 and 1: two text keys' kept copies (0: NULL).
    const LAYOUT: RecordLayout = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::MinVar, SlotKind::MinVar]);
    const SIZES: [usize; 9] = [0, 1, 2, 3, 50, SMALL_SORT_ROWS - 1, SMALL_SORT_ROWS, SMALL_SORT_ROWS + 1, 300];

    type Key = (Option<Vec<u8>>, Option<Vec<u8>>);

    fn varlena(s: &[u8]) -> Vec<u8> {
        let mut v = (((s.len() + 4) as u32) << 2).to_le_bytes().to_vec();
        v.extend_from_slice(s);
        v
    }

    /// Record `i` (key word `1000 - i`, so the key words alone order them backwards) under the sort words `first` (slot 0)
    /// and a second text word (slot 1) as `generic_by` orders them: its status and the records' indices.
    fn sorted(keys: &[Key], first: u64, small: bool) -> (i64, Vec<usize>) {
        let words = LAYOUT.size() as usize / 8;
        let images: Vec<(Option<Vec<u8>>, Option<Vec<u8>>)> = keys.iter().map(|(a, b)| (a.as_deref().map(varlena), b.as_deref().map(varlena))).collect();
        let mut recs: Vec<Vec<u64>> = images
            .iter()
            .enumerate()
            .map(|(i, (a, b))| {
                let mut r = vec![0u64; words]; // alloc: Cold
                r[0] = 1000 - i as u64;
                r[LAYOUT.slot_off[0] as usize / 8] = a.as_ref().map_or(0, |v| v.as_ptr() as u64);
                r[LAYOUT.slot_off[1] as usize / 8] = b.as_ref().map_or(0, |v| v.as_ptr() as u64);
                r
            })
            .collect();
        let mut ptrs: Vec<*const u8> = recs.iter_mut().map(|r| r.as_ptr() as *const u8).collect();
        let sort = [first, SK_TEXT | SK_HEAP | 1 << SK_TEXT_SLOT_SHIFT];
        // SAFETY: a ctx is plain words; zero is its closed state (heap words render nothing)
        let mut ctx: Ctx = unsafe { core::mem::zeroed() };
        let n = ptrs.len();
        // SAFETY: `ptrs` are live records of `LAYOUT` whose slots point at live varlenas
        let st = unsafe { generic_by(&mut ctx, &mut ptrs, &LAYOUT, &sort, &[0, 0], None, n, small) };
        (st, ptrs.iter().map(|&p| 1000 - unsafe { *(p as *const u64) } as usize).collect())
    }

    /// The order `generic` must give: the first key by its bytes (a bpchar's without trailing blanks; NULLs by
    /// `SK_NULLS_FIRST`; DESC reverses two values), the second by its bytes (NULLs last), then the key word ascending.
    fn expected(keys: &[Key], first: u64) -> Vec<usize> {
        let trim = |v: &Vec<u8>| if first & SK_BPCHAR != 0 { bc_truelen(v).to_vec() } else { v.clone() };
        let rank = |v: &Option<Vec<u8>>, nf: bool| match v {
            Some(_) => 1,
            None if nf => 0,
            None => 2,
        };
        let mut idx: Vec<usize> = (0..keys.len()).collect();
        idx.sort_by(|&x, &y| {
            let (a, b) = (&keys[x], &keys[y]);
            rank(&a.0, first & SK_NULLS_FIRST != 0)
                .cmp(&rank(&b.0, first & SK_NULLS_FIRST != 0))
                .then_with(|| match (&a.0, &b.0) {
                    (Some(p), Some(q)) if first & SK_DESC != 0 => trim(q).cmp(&trim(p)),
                    (Some(p), Some(q)) => trim(p).cmp(&trim(q)),
                    _ => Ordering::Equal,
                })
                .then_with(|| rank(&a.1, false).cmp(&rank(&b.1, false)))
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| (1000 - x).cmp(&(1000 - y)))
        });
        idx
    }

    /// `n` keys: few values (whole ties of the first key, the second or the key word decides), a shared eight-byte prefix
    /// (the heads tie), trailing blanks, an empty text and NULLs.
    fn keys(n: usize, seed: u64) -> Vec<Key> {
        let mut s = seed | 1;
        (0..n)
            .map(|i| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                let first = match s % 6 {
                    0 => None,
                    1 => Some(format!("prefix-0{}", s % 3).into_bytes()), // alloc: Cold
                    2 => Some(format!("{}{}", s % 4, " ".repeat((s >> 8) as usize % 3)).into_bytes()), // alloc: Cold
                    3 => Some(Vec::new()), // alloc: Cold
                    _ => Some(format!("{:03}-{}", (s >> 4) % 40, i % 2).into_bytes()), // alloc: Cold
                };
                let second = if i % 5 == 0 { None } else { Some(format!("w{}", (s >> 16) % 3).into_bytes()) }; // alloc: Cold
                (first, second)
            })
            .collect()
    }

    #[test]
    fn a_small_group_sort_is_the_comparators_order() {
        let text = SK_TEXT | SK_HEAP;
        for n in SIZES {
            for first in [text, text | SK_BPCHAR, text | SK_DESC, text | SK_NULLS_FIRST, text | SK_BPCHAR | SK_DESC | SK_NULLS_FIRST] {
                let k = keys(n, 17 + n as u64);
                let want = expected(&k, first);
                let (off, on) = (sorted(&k, first, false), sorted(&k, first, true));
                assert_eq!(off, (0, want.clone()), "n {n} first {first:#x}: the comparator's road");
                assert_eq!(on, (0, want), "n {n} first {first:#x}: the small road");
            }
        }
    }

    #[test]
    fn a_word_the_heads_do_not_stand_for_keeps_the_comparators_road() {
        let k = keys(20, 5);
        let images: Vec<Option<Vec<u8>>> = k.iter().map(|(a, _)| a.as_deref().map(varlena)).collect();
        let mut recs: Vec<Vec<u64>> = images.iter().map(|a| {
            let mut r = vec![0u64; LAYOUT.size() as usize / 8]; // alloc: Cold
            r[LAYOUT.slot_off[0] as usize / 8] = a.as_ref().map_or(0, |v| v.as_ptr() as u64);
            r
        }).collect();
        let mut ptrs: Vec<*const u8> = recs.iter_mut().map(|r| r.as_ptr() as *const u8).collect();
        let before = ptrs.clone();
        // SAFETY: a ctx is plain words; zero is its closed state
        let mut ctx: Ctx = unsafe { core::mem::zeroed() };
        let err = core::cell::Cell::new(0i64);
        for w in [SK_TEXT | SK_HEAP | 100 << SK_TEXT_COLL_SHIFT, SK_TEXT | SK_HEAP | SK_CMP, SK_SLOT | SK_TEXT | SK_HEAP, SK_SLOT] {
            // SAFETY: as `sorted`'s
            assert_eq!(unsafe { small_generic(&mut ctx, &mut ptrs, &LAYOUT, &[w], &[0], None, &err) }, None, "word {w:#x}");
            assert!(ptrs == before, "word {w:#x}: nothing moved");
        }
    }
}
