//! The per-code front's host entries (lowerv4 dict/strategies/codefront_percode.rs): a claim's row counts
//! per source code of a derived key, folded at each window's end (or a full touched list) into a map of one part's
//! per-key records, the map folded into the claim's T1 when it fills, when a window of another part comes, and at the
//! claim's end. Budget 260 lines (files-extra.tsv, +25 % gate). Written by M1-KEYED; owner M1-HOSTS.
//!
//! The front (`agg_percode_open`, the claim's arena scratch: its grant, on the statement's ledger) is one region: a
//! header (word 0: the map's records in use, word 1: the map's part), a `u32` row count per code (`PC_CODES`), the map's
//! `u32` slots (0 empty, else a record's index + 1; homed by `KeyHash`, probed linearly, at most half full), the window's
//! touched list (per code first seen in the window: `[lo, hi, f << 32 | code]`, its key image and its per-code value:
//! `PC_F_LOW` bits of a length or a verdict, and KEYED-13's FILTER verdict in bit `PC_F_VERDICT` beside a length),
//! the map's records (the claim's layout) and their index list for `agg_front_flush`, then the decline's sample (`SAMPLE`).
//! Only the header, the counts, the slots and the sample are zeroed at the open; a record is cleared as it opens. A min /
//! max slot holds its best code + 1 until the map folds: the fold renders it once through the map's part's dictionary
//! (resident for the statement, as a kept code word's render at the emit), so a window's end reads no dictionary at all.
use gravity_abi::PtrWord;
use gravity_abi::breaker::Rec;
use gravity_abi::ctx::site_ptr;
use gravity_abi::hash::{Depth, KeyHash};
use gravity_abi::status::{self, raise};
use gravity_abi::Ctx;

use crate::table::entries::agg_front_flush;
use crate::host_abi::AggState;


/// The front's capacity in codes (a code at or past it takes the per-row path), its touched list's entries (a full list
/// folds at once, mid-window), and the map's records.
pub const PC_CODES: usize = 128 << 10;
pub const PC_TOUCH: usize = 4096;
pub const PC_RECS: usize = 8192;
const SLOTS: usize = 2 * PC_RECS;
/// The front's regions: the counts, the map's slots, the touched list, the records (then `PC_RECS` index words).
pub const PC_COUNTS: usize = 64;
const PC_SLOTS: usize = PC_COUNTS + 4 * PC_CODES;
pub const PC_LIST: usize = PC_SLOTS + 4 * SLOTS;
const PC_RECORDS: usize = PC_LIST + 24 * PC_TOUCH;
// the flush sorts one slot's `PC_RECS` extremes in the map's slots
const _: () = assert!(4 * SLOTS >= 8 * PC_RECS);

/// A per-code value's bits below `PC_F_VERDICT` (a length, or B3-07's verdict 0 / 1 when no length rides): `f`.
pub const PC_F_LOW: u32 = (1 << PC_F_VERDICT) - 1;
/// KEYED-13: the bit of the per-code value that carries a FILTER's verdict beside a length (`Fold::CountFv`).
pub const PC_F_VERDICT: u32 = 31;

/// A slot's fold of a listed code's `n` rows (4 bits per slot in `ops`, 0: none): the count, `n x f` into an int sum or an
/// int average, the key's representative `part << 32 | code` (or a parted record's deferred minimum, V4R-105), the min /
/// max image.
#[repr(u64)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fold {
    Count = 1,
    Sum64 = 2,
    Sum128 = 3,
    Avg128 = 4,
    Rep = 5,
    Min = 6,
    Max = 7,
    /// B3-07: a count whose FILTER reads only the source column: `f` is the code's verdict (0 / 1), so `n x f` rows count.
    CountF = 8,
    /// KEYED-13: a count whose FILTER reads only the source column, beside a length: its verdict is the value's
    /// `PC_F_VERDICT` bit, so `n x bit` rows count.
    CountFv = 9,
}

impl Fold {
    const ALL: [Fold; 9] = [Fold::Count, Fold::Sum64, Fold::Sum128, Fold::Avg128, Fold::Rep, Fold::Min, Fold::Max, Fold::CountF, Fold::CountFv];

    /// The fold a slot's 4 bits name (`None`: 0, an unfolded slot).
    #[inline(always)]
    pub fn of(bits: u64) -> Option<Fold> {
        Self::ALL.get((bits as usize).wrapping_sub(1)).copied()
    }

    /// The `ops` bits that fold slot `j` by this fold.
    pub const fn at(self, j: usize) -> u64 {
        (self as u64) << (4 * j)
    }
}

const _: () = {
    let mut i = 0;
    while i < Fold::ALL.len() {
        assert!(Fold::ALL[i] as usize == i + 1);
        i += 1;
    }
};

/// The raise of an error stashed by a dictionary read or the pinned pool's refusal (`raise::HOST` names none of its own).
const HOST_RAISE: i64 = status::raised(raise::bit(raise::HOST));

/// The front's bytes for records of `rec` bytes.
pub const fn pc_bytes(rec: u64) -> u64 {
    (PC_RECORDS + PC_RECS * (rec as usize + 8) + 8 * SAMPLE) as u64
}

/// D2804: the decline's sample, words at the front's end: the listed codes, the rows and the opened records of the folds
/// since the last judged.
const SAMPLE: usize = 3;

/// The sample's words of a front whose records are `rec` bytes.
#[inline(always)]
unsafe fn sample(front: *mut u8, rec: usize) -> *mut u64 {
    front.add(PC_RECORDS + PC_RECS * (rec + 8)) as *mut u64
}

crate::contract::host! {
    /// `code.front_percode`'s open: the front from the claim's arena, its header, counts and slots zeroed; null when the arena
    /// refuses (every row of the claim takes the per-row path).
    pub unsafe fn agg_percode_open(ctx: *mut Ctx, rec: u64) -> *mut u8 {
        let p = crate::util::scratch(ctx, pc_bytes(rec) as usize);
        if !p.is_null() {
            core::ptr::write_bytes(p, 0, PC_LIST);
            core::ptr::write_bytes(sample(p, rec as usize), 0, SAMPLE);
        }
        p
    }
}

crate::contract::host! {
    /// The window's end (or a full list): each of the `n` listed codes' rows folded into its key's map record by `ops`, its
    /// count zeroed. The map holds one part's records: a window of another part folds the map into T1 first. A min / max
    /// slot holds its best code + 1 of the map's part (0: none) until the fold renders it. `at` = `site << 8 | lane`, the
    /// source lane the representative and the extremes name; `decline` = the code weight << 32 | the opened record's, the
    /// decline's (0: never). 0; 1, the front declines for the rest of the claim (`declines` of the sample this fold
    /// completes: the caller turns its capacity to 0, so every later row takes the per-row path into the same T1, and the
    /// map folds on at the claim's end); or a negative status (the map's fold).
    pub unsafe fn agg_percode_window(ctx: *mut Ctx, st: *mut u8, front: *mut u8, n: u64, ops: u64, at: u64, decline: u64) -> i64 {
        guard!(gravity_abi::status::ERROR, agg_percode_window_in(ctx, st, front, n, ops, at, decline))
    }
}

/// `agg_percode_window`'s body, run under `guard!` (V4R-16 / V4R-40).
#[inline(always)]
unsafe fn agg_percode_window_in(ctx: *mut Ctx, st: *mut u8, front: *mut u8, n: u64, ops: u64, at: u64, decline: u64) -> i64 {
    let a = st as *mut AggState;
    let layout = &*(*a).layout.ptr();
    let part = (*site_ptr(ctx, (at >> 8) as usize)).part_id as u64;
    let hdr = front as *mut u64;
    // a count-walk front's counted part walks first (m1_countwalk.rs; its counts are these)
    if *hdr.add(2) != part + 1 {
        if let Err(s) = crate::dict::strategies::countwalk::settle(ctx, a, front, ops, at) {
            return s;
        }
    }
    if *hdr.add(1) != part {
        if let Err(s) = flush(ctx, a, front, ops, at) {
            return s;
        }
        *hdr.add(1) = part;
    }
    let counts = front.add(PC_COUNTS) as *mut u32;
    let list = core::slice::from_raw_parts(front.add(PC_LIST) as *const [u64; 3], n as usize);
    let (mut total, mut opened) = (0u64, 0u64);
    for e in list {
        let (code, f) = (e[2] as u32, (e[2] >> 32) as u32);
        let (low, bit) = (i64::from(f & PC_F_LOW), i64::from(f >> PC_F_VERDICT));
        let c = counts.add(code as usize);
        let rows = *c as i64;
        total += rows as u64;
        *c = 0;
        let keys = crate::dict::strategies::countwalk::image(layout, [e[0], e[1]], part);
        match fold_code(ctx, a, front, &keys.0[..layout.nkeys()], ops, at, (rows, rows * low, rows * bit), part as u32, (code, code)) {
            Ok(o) => opened += o as u64,
            Err(s) => return s,
        }
    }
    // a full sample judges: folds of fewer rows than a full list (a part's tail, a window's rest after a full list, a
    // filtered window) add up until they hold as many
    let s = sample(front, layout.size() as usize);
    let (n, total, opened) = (*s + n, *s.add(1) + total, *s.add(2) + opened);
    if total < PC_TOUCH as u64 {
        (*s, *s.add(1), *s.add(2)) = (n, total, opened);
        return 0;
    }
    (*s, *s.add(1), *s.add(2)) = (0, 0, 0);
    i64::from(declines((*ctx).width, n, total, opened, decline))
}

/// D2804: whether a claim of `width` participants turns its front off after a sample of `n` listed codes over `total` rows
/// that opened `opened` map records, under `decline` = the code weight << 32 | the opened record's (the lowerer's, from the
/// cost home: `cost::PERCODE_DECLINE`; 0: never): the codes and records, weighed in 256ths of a row's per-row path, at
/// least the rows (cost_profile front.rs `percode_declines`, which the test below holds this to).
/// A serial claim whose rows open a record each or nearly (fresh codes of fresh keys) folds nothing, and its map only
/// relays those records to T1 at a cost above the per-row path's append; a key whose codes are fresh but whose records
/// repeat (a URL's host key over many URLs) keeps its front, as does a parallel claim, whose map cuts the records every
/// worker writes for the owner merge.
#[inline(always)]
pub(crate) fn declines(width: u16, n: u64, total: u64, opened: u64, decline: u64) -> bool {
    let (code_w, opened_w) = (decline >> 32, decline & 0xffff_ffff);
    width <= 1 && decline != 0 && total != 0 && code_w as u128 * n as u128 + opened_w as u128 * opened as u128 >= 256 * total as u128
}

/// Codes `lo ..= hi` of `part` (one code, or a count-walk run of one key: m1_countwalk.rs), `rows` rows, `n x f` summed
/// `nf` (the value's `PC_F_LOW` bits) and `n x bit` summed `nv` (its `PC_F_VERDICT` bit), folded into their key `keys`' map
/// record by `ops`. A min / max slot takes the run's first / last code. True when the record opened here.
pub(crate) unsafe fn fold_code(ctx: *mut Ctx, a: *mut AggState, front: *mut u8, keys: &[u64], ops: u64, at: u64, (rows, nf, nv): (i64, i64, i64), part: u32, (lo, hi): (u32, u32)) -> Result<bool, i64> {
    let layout = &*(*a).layout.ptr();
    let (r, opened) = record(ctx, a, front, keys, ops, at)?;
    let rec = Rec::at(r, layout);
    for j in 0..layout.nslots() {
        match Fold::of(ops >> (4 * j) & 15) {
            Some(Fold::Count) => rec.add_count(j, rows),
            Some(Fold::CountF) => rec.add_count(j, nf),
            Some(Fold::CountFv) => rec.add_count(j, nv),
            Some(Fold::Sum64) => rec.add_i64(j, nf),
            Some(Fold::Sum128) => rec.add_i128(j, i128::from(nf)),
            Some(Fold::Avg128) => rec.add_avg(j, i128::from(nf), rows),
            Some(Fold::Rep) => rec.min_code(j, part, lo),
            Some(op @ (Fold::Min | Fold::Max)) => {
                // one part's codes order as their bytes (a byte-order collation: the plan's gate)
                let (s, v) = (r.add(layout.slot_off(j) as usize) as *mut u64, u64::from(if op == Fold::Min { lo } else { hi }) + 1);
                let cur = s.read_unaligned();
                if cur == 0 || if op == Fold::Min { v < cur } else { v > cur } {
                    s.write_unaligned(v);
                }
            }
            None => {}
        }
    }
    Ok(opened)
}

crate::contract::host! {
    /// The claim's end: the map folded into T1. 0 or a negative status.
    pub unsafe fn agg_percode_flush(ctx: *mut Ctx, st: *mut u8, front: *mut u8, ops: u64, at: u64) -> i64 {
        guard!(gravity_abi::status::ERROR, agg_percode_flush_in(ctx, st, front, ops, at))
    }
}

/// `agg_percode_flush`'s body, run under `guard!` (V4R-16 / V4R-40).
#[inline(always)]
unsafe fn agg_percode_flush_in(ctx: *mut Ctx, st: *mut u8, front: *mut u8, ops: u64, at: u64) -> i64 {
    if front.is_null() {
        return 0;
    }
    match crate::dict::strategies::countwalk::settle(ctx, st as *mut AggState, front, ops, at).and_then(|()| flush(ctx, st as *mut AggState, front, ops, at)) {
        Ok(()) => 0,
        Err(s) => s,
    }
}

/// The map's record of image `keys`, opened (cleared, then `Rec::open`) when absent, and whether it opened; a full map is
/// folded first.
unsafe fn record(ctx: *mut Ctx, a: *mut AggState, front: *mut u8, keys: &[u64], ops: u64, at: u64) -> Result<(*mut u8, bool), i64> {
    let layout = &*(*a).layout.ptr();
    let (size, slots, recs) = (layout.size() as usize, front.add(PC_SLOTS) as *mut u32, front.add(PC_RECORDS));
    let home = KeyHash::of(keys).home(SLOTS.trailing_zeros(), Depth::TOP);
    let mut i = home;
    while *slots.add(i) != 0 {
        let r = recs.add((*slots.add(i) as usize - 1) * size);
        let w = r as *const u64;
        if keys.iter().enumerate().all(|(k, &x)| *w.add(k) == x) {
            return Ok((r, false));
        }
        i = (i + 1) & (SLOTS - 1);
    }
    let used = front as *mut u64;
    if *used == PC_RECS as u64 {
        flush(ctx, a, front, ops, at)?;
        i = home;
    }
    let r = recs.add(*used as usize * size);
    *used += 1;
    *slots.add(i) = *used as u32;
    let rec = Rec::at(r, layout);
    rec.clear();
    rec.open(keys);
    Ok((r, true))
}

/// The map's records folded into the claim's T1: each min / max's best code rendered through the map's part's dictionary
/// in code order and copied (code_copy.rs's copy, its header naming the code: `extreme`), then `agg_front_flush`
/// (`locate` and `Rec::fold`, the owner merge's, each record zeroed after); the map emptied.
pub(crate) unsafe fn flush(ctx: *mut Ctx, a: *mut AggState, front: *mut u8, ops: u64, at: u64) -> Result<(), i64> {
    let (n, part) = (*(front as *const u64), *(front as *const u64).add(1));
    if n == 0 {
        return Ok(());
    }
    let (layout, recs) = (&*(*a).layout.ptr(), front.add(PC_RECORDS));
    let size = layout.size() as usize;
    let ids = recs.add(PC_RECS * size) as *mut u64;
    for k in 0..n as usize {
        *ids.add(k) = k as u64;
    }
    // each min / max slot's best codes rendered in code order, the part's dictionary read forward, not at random; the
    // order is sorted in the map's slots, which the fold no longer reads (they are cleared below)
    let order = core::slice::from_raw_parts_mut(front.add(PC_SLOTS) as *mut u64, PC_RECS);
    for j in (0..layout.nslots()).filter(|&j| matches!(Fold::of(ops >> (4 * j) & 15), Some(Fold::Min | Fold::Max))) {
        let mut m = 0;
        for k in 0..n as usize {
            let v = (recs.add(k * size + layout.slot_off(j) as usize) as *const u64).read_unaligned();
            if v != 0 {
                (order[m], m) = ((v - 1) << 32 | k as u64, m + 1);
            }
        }
        order[..m].sort_unstable();
        for &e in &order[..m] {
            extreme(ctx, a, recs.add((e as u32) as usize * size), j, at, part)?;
        }
    }
    match agg_front_flush(ctx, a as *mut u8, recs, ids, n) {
        0 => {}
        s => return Err(s),
    }
    core::ptr::write_bytes(front.add(PC_SLOTS), 0, 4 * SLOTS);
    *(front as *mut u64) = 0;
    Ok(())
}

/// Slot `j` (`MinVar` / `MaxVar`) of the record at `r`, holding its best code + 1 of `part` (0: none): the code rendered
/// through lane `at` and copied, the slot then the copy's address, marked valid; a code with no image raises.
unsafe fn extreme(ctx: *mut Ctx, a: *mut AggState, r: *mut u8, j: usize, at: u64, part: u64) -> Result<(), i64> {
    let layout = &*(*a).layout.ptr();
    let slot = r.add(layout.slot_off(j) as usize) as *mut u64;
    let v = slot.read_unaligned();
    if v == 0 {
        return Ok(());
    }
    slot.write_unaligned(0);
    let word = part << 32 | (v - 1);
    let img = match crate::dict::reps::render(site_ptr(ctx, (at >> 8) as usize), at as u8, word) {
        Ok(Some(img)) => img,
        Ok(None) => {
            // a listed code is never a NULL row's: a code the dictionary has no image for disagrees with the lane (M1-FOLD)
            gravity_fn::stash_error(Box::new(gravity_fn::PgError::error("gravity: a listed code without an image"))); // alloc: Cold
            return Err(HOST_RAISE);
        }
        Err(_) => return Err(HOST_RAISE),
    };
    let new = crate::dict::reps::varlena_bytes(img);
    if copy(ctx, slot, core::ptr::addr_of_mut!((*a).arena_cur) as *mut u64, new, new.len() + 4, word, NO_THR) == 0 {
        return Err(HOST_RAISE);
    }
    if layout.has_validity() {
        let (o, sh) = layout.valid_bit(j);
        *r.add(o) |= 1 << sh;
    }
    Ok(())
}

// The copy: a twin of lowerv4 fold/code_copy.rs's (M1-FOLD), whose rule the same slots follow after the flush (one
// header, one cursor).

/// A copy's header before its varlena: its capacity, the winner's `part << 32 | code`, the losing threshold.
pub const HDR: usize = 24;
pub const NO_THR: u64 = u64::MAX;

/// code_copy.rs `copy`: a copy of capacity `cap` (rounded to 8) holding `new`, its header `(cap, win, thr)`.
unsafe fn copy(ctx: *mut Ctx, slot: *mut u64, images: *mut u64, new: &[u8], cap: usize, win: u64, thr: u64) -> i64 {
    let cap = (cap + 7) & !7;
    let d = alloc(ctx, images, HDR + cap);
    if d.is_null() {
        crate::host_abi::pin_refused(HDR + cap); // D1392 (b)
        return 0;
    }
    let h = d as *mut u64;
    (h.write(cap as u64), h.add(1).write(win), h.add(2).write(thr));
    write_image(d.add(HDR), new);
    slot.write_unaligned(d.add(HDR) as u64);
    1
}

/// `new` as a 4-byte-header varlena at `d`.
unsafe fn write_image(d: *mut u8, new: &[u8]) {
    core::ptr::write_unaligned(d as *mut u32, ((new.len() as u32 + 4) << 2).to_le());
    core::ptr::copy_nonoverlapping(new.as_ptr(), d.add(4), new.len());
}

/// code_copy.rs `alloc`: `need` bytes bumped from the end's cursor (`cur, end, chunk`: `AggState`'s image arena, chunks
/// from 64 KiB doubling to 32 MiB, pinned to the statement's end).
unsafe fn alloc(ctx: *mut Ctx, images: *mut u64, need: usize) -> *mut u8 {
    let images = gravity_abi::breaker::T1::img_resolve(images); // D1392 (a): a state's words name its table's
    gravity_abi::breaker::img_bump(images, need, |bytes| gravity_abi::program::pin(ctx, bytes)) // D3312: the arena's one bump
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_abi::breaker::T1_IMAGE_CHUNK;

    thread_local! {
        static PINS: core::cell::RefCell<Vec<Vec<u64>>> = const { core::cell::RefCell::new(Vec::new()) }; // alloc: Cold
    }

    /// The test's `Program.pin`: a zeroed region of its own, kept to the thread's end.
    unsafe extern "C" fn test_pin(_ctx: *mut Ctx, bytes: u64) -> *mut u8 {
        PINS.with(|p| {
            let mut v = vec![0u64; (bytes as usize).div_ceil(8)]; // alloc: Cold
            let d = v.as_mut_ptr() as *mut u8;
            p.borrow_mut().push(v);
            d
        })
    }

    /// A ctx whose program pins through `test_pin` (both kept alive by the caller).
    fn test_ctx() -> (Vec<u64>, Box<gravity_abi::ProgramBlock>) {
        let mut program = Box::new(gravity_abi::ProgramBlock::NONE); // alloc: Cold
        program.pin = Some(test_pin);
        let mut ctx = vec![0u64; gravity_abi::ctx::ctx_size(1) / 8]; // alloc: Cold
        // SAFETY: the words are a ctx image; `program` outlives every use.
        unsafe { (*(ctx.as_mut_ptr() as *mut Ctx)).program = &mut *program };
        (ctx, program)
    }

    /// FOLD's header (code_copy.rs, which reads a copy this file makes when a per-row fold meets the record): the capacity
    /// its varlena truly has, rounded to 8, then the winner's word and no threshold, the image after them; copies pack into
    /// the cursor's 64 KiB chunk, and a copy past its room opens the next chunk, doubled.
    /// D2804: a serial claim declines its front after a sample whose codes and opened records outweigh its rows (cost::
    /// PERCODE_DECLINE's fitted weights); a parallel claim and a sample whose codes or records repeat keep it.
    #[test]
    fn a_serial_claim_of_fresh_codes_declines_the_front() {
        // the cost home's word (cost::PERCODE_DECLINE: the code weight << 32 | the opened record's)
        let (cw, ow) = (cost_profile::front::PERCODE_CODE_W, cost_profile::front::PERCODE_OPENED_W);
        let d = cw << 32 | ow;
        assert!(declines(1, 4096, 4096, 4096, d), "every row a fresh record, serial");
        assert!(declines(1, 4096, 4096, 1024, d), "fresh codes, a record in four rows");
        assert!(!declines(9, 4096, 4096, 4096, d), "a parallel claim keeps its front");
        assert!(!declines(1, 4096, 4096, 300, d), "fresh codes whose records repeat (one host over many referers)");
        assert!(!declines(1, 1024, 4096, 1024, d), "codes and records repeating every four rows");
        assert!(!declines(1, 4096, 4096, 4096, 0), "no weights: never");
        for (n, t, o) in [(4096, 4096, 4096), (2048, 4096, 2048), (1024, 4096, 1024), (4096, 4096, 1024), (4096, 4096, 300), (4096, 8192, 512), (1, 1, 1)] {
            assert_eq!(declines(1, n, t, o, d), cost_profile::front::percode_declines(n, t, o, cw, ow), "the cost home's rule: {n} {t} {o}");
        }
    }

    #[test]
    fn a_copy_carries_code_copys_header() {
        let (mut ctx, _prog) = test_ctx();
        let cp = ctx.as_mut_ptr() as *mut Ctx;
        let mut cur = [0u64; 3];
        let (mut a, mut b) = (0u64, 0u64);
        let long = vec![b'z'; 70_000]; // alloc: Cold
        unsafe {
            assert_eq!(copy(cp, &mut a, cur.as_mut_ptr(), b"http://s.example/", 21, 3 << 32 | 9, NO_THR), 1);
            let h = a as *const u64;
            assert_eq!((*h.sub(3), *h.sub(2), *h.sub(1)), (24, 3 << 32 | 9, NO_THR), "cap, win, thr");
            assert_eq!(crate::dict::reps::varlena_bytes(a as usize as *const u8), b"http://s.example/");
            assert_eq!(cur[2], T1_IMAGE_CHUNK as u64, "the first chunk");
            assert_eq!(copy(cp, &mut b, cur.as_mut_ptr(), &long, long.len() + 4, 4 << 32, NO_THR), 1);
            assert_eq!((*(b as *const u64).sub(3), cur[2]), (70_008, 2 * T1_IMAGE_CHUNK as u64), "a copy past the chunk opens the next, doubled");
            assert_eq!(crate::dict::reps::varlena_bytes(b as usize as *const u8), long.as_slice());
            assert_eq!(crate::dict::reps::varlena_bytes(a as usize as *const u8), b"http://s.example/", "the earlier copy stays");
        }
    }
}
