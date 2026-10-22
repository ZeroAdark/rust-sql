//! M1's keyed-table host entries and layouts (kernels host_abi v4 m1, re-exported there): the hashed table's open at its
//! sized bound and its miss (grown in place), the code front's open and flush, and `keyed.twopass`'s admitted set.
//! Created and closed by M1 (owner M1-HOSTS).
use gravity_abi::{PtrWord, Word};
use gravity_abi::breaker::{FloatOverflow, Rec, RecordLayout, ADAPT_FACTOR, SHARE, T1, T1_BYPASS, T1_GROWN_MIN_SLOTS, t1_open_bytes};
use gravity_abi::hash::{Depth, KeyHash};
use gravity_abi::program::partial;
use gravity_abi::Ctx;

use crate::host_abi::{agg_open, fold_bits, t1_bytes, table_bits, AggState, AGG_GROUPS_BYPASS, AGG_GROUPS_OBSERVE};

crate::contract::host! {
    /// `table.sized`'s open (keyed/table_size.rs, M1-TABLES): the claim's T1 laid out with the default's slots reserved but
    /// opened at the bound `groups` sizes (`T1::at_sized`; a set operation side's in its region's reservation,
    /// `T1::at_reserved`, as `agg_open` lays one), then `agg_open` over it; `agg_open`'s own `T1::at` finds the live table and
    /// keeps it. The partition bits are `agg_open`'s rule, so both see the same `pbits`. D3515: under the landing pad as
    /// `agg_open` (a region holding another table's header is the statement's error, never an abort).
    #[guard(gravity_abi::status::ERROR)]
    pub unsafe fn agg_open_grown(ctx: *mut Ctx, st: *mut u8, layout: *const RecordLayout, bits: u64, groups: u64) -> i64 {
        let g = groups & !(AGG_GROUPS_OBSERVE | AGG_GROUPS_BYPASS);
        // the setop posture (option c'): a set operation side's reservation is asked here, where the table is laid out
        // (`agg_open` then finds it live); 0 = none, laid exactly as `T1::at_sized`. A region that started small (`laid`)
        // is laid out at its reservation.
        let own = partial(ctx);
        let (reserve, floor, laid, cut) = if (*(own as *const T1)).cap == 0 { gravity_abi::program::region_reserve_laid(ctx) } else { (0, false, false, 0) };
        let pbits = fold_bits(ctx, table_bits(bits), (*ctx).flags & gravity_abi::ctx::FLAG_ARM_RUN != 0, g, (*layout).size() as u64, crate::table::fold_entries::open_cut(reserve, laid, cut));
        let rec = (*layout).size() as u64;
        let t = T1::at_laid(own, &*layout, pbits, t1_open_bytes(g, rec), grown_first_bytes(g, rec), SHARE, reserve, laid);
        if floor && t.flags & gravity_abi::breaker::T1_RESERVED != 0 {
            t.flags |= gravity_abi::breaker::T1_FLOOR; // D2894: its pool commits before it compacts
        }
        agg_open(ctx, st, layout, bits, groups)
    }
}

/// D2988: the bytes a grown table (`agg_open_grown`) opens at for the bound `groups`: `ADAPT_FACTOR` slots a group as
/// `t1_bytes`, at least `T1_GROWN_MIN_SLOTS` (not `t1_bytes`' `T1_MIN_SLOTS`: the table grows in place past them), at
/// most the bound's table (`t1_open_bytes`, which it grows to); no bound (0) opens at `t1_bytes`' answer, the default.
pub fn grown_first_bytes(groups: u64, rec: u64) -> u32 {
    if groups == 0 {
        return t1_bytes(groups, rec);
    }
    let slots = groups.saturating_mul(ADAPT_FACTOR).checked_next_power_of_two().unwrap_or(u64::MAX).max(T1_GROWN_MIN_SLOTS);
    slots.saturating_mul(rec).min(t1_open_bytes(groups, rec) as u64) as u32
}

/// `table.sized`'s grow (R3-L02, `T1::grow`): past the sizing's fill (`T1::can_grow`: the keys the table opened) with room
/// to double and not bypassed, a key found or opened in its home pair is its record (the caller folds into it at once: an
/// opened record is unused until then); a full pair doubles the table first and the state's `mask` follows the new `cap`
/// (a declined grow changes nothing, and the locate evicts as before). None: locate as usual.
unsafe fn grown(st: *mut AggState, keys: &[u64]) -> Option<*mut u8> {
    let t = (*st).t.ptr();
    if (*t).flags & T1_BYPASS != 0 || !(*t).can_grow() {
        return None;
    }
    if let Some(p) = T1::probe(t, keys, KeyHash::of(keys)) {
        return Some(p);
    }
    if T1::grow(t) {
        (*st).mask = (*t).cap as u64 - 1;
    }
    None
}

crate::contract::host! {
    /// The hashed keyed table's miss (`table.bypass`, keyed/bypass.rs): the record of the image `keys` (the layout's key words)
    /// in the claim's T1, grown first when it may (`grown`), else through the kernel's `locate` (the occupant evicted, a full
    /// pool flushed, the bypass switched on observation), its address, or a negative status.
    pub unsafe fn agg_locate(ctx: *mut Ctx, st: *mut u8, keys: *const u64) -> i64 {
        guard!(gravity_abi::status::ERROR, agg_locate_in(ctx, st, keys))
    }
}

/// `agg_locate`'s body, run under `guard!` (V4R-16 / V4R-40).
#[inline(always)]
unsafe fn agg_locate_in(ctx: *mut Ctx, st: *mut u8, keys: *const u64) -> i64 {
    let st = st as *mut AggState;
    let keys = core::slice::from_raw_parts(keys, (*(*st).layout.ptr()).nkeys());
    if let Some(p) = grown(st, keys) {
        return p as i64;
    }
    match crate::host_abi::locate(ctx, st, keys) {
        Ok(p) => p as i64,
        Err(e) => e,
    }
}

crate::contract::host! {
    /// `code.front_part`'s front (keyed/codefront.rs, M1-TABLES): `bytes` of the claim's arena scratch (drawn from its grant,
    /// on the statement's ledger), zeroed; null when the arena refuses, and the claim runs on the hashed table instead.
    pub unsafe fn agg_front_open(ctx: *mut Ctx, bytes: u64) -> *mut u8 {
        let p = crate::util::scratch(ctx, bytes as usize);
        if !p.is_null() {
            core::ptr::write_bytes(p, 0, bytes as usize);
        }
        p
    }

    /// `code.front_part`'s flush: each touched code `c`'s front record (`front + c * size`, a
    /// full record of the claim's layout whose key words are the whole image) located in the claim's T1 and folded there as
    /// `agg_miss` folds an image (opened, found, or appended under the bypass; `Rec::fold`, the owner merge's), then zeroed;
    /// the table grows first when it may (`grown`). 0, or the first negative status (a float overflow as the owner merge
    /// raises it); the next record's home is prefetched.
    pub unsafe fn agg_front_flush(ctx: *mut Ctx, st: *mut u8, front: *mut u8, touched: *const u64, n: u64) -> i64 {
        guard!(gravity_abi::status::ERROR, agg_front_flush_in(ctx, st, front, touched, n))
    }
}

/// `agg_front_flush`'s body, run under `guard!` (V4R-16 / V4R-40).
#[inline(always)]
unsafe fn agg_front_flush_in(ctx: *mut Ctx, st: *mut u8, front: *mut u8, touched: *const u64, n: u64) -> i64 {
    let a = st as *mut AggState;
    let (layout, t) = (&*(*a).layout.ptr(), (*a).t.ptr());
    let (size, codes) = (layout.size() as usize, core::slice::from_raw_parts(touched, n as usize));
    for (i, &c) in codes.iter().enumerate() {
        if let Some(&next) = codes.get(i + 1) {
            (*t).prefetch_home(KeyHash::of(Rec::at(front.add(next as usize * size), layout).keys()));
        }
        let src = front.add(c as usize * size);
        // `grown`'s record is folded here: a record the probe opened is unused (its sentinel 0) until something folds into it
        // NEW-numeric-1 (P): the front holds the window's rows of its key, later than the table's (`Rec::fold_newer`)
        let r = match grown(a, Rec::at(src, layout).keys()) {
            Some(p) => Rec::at(p, layout).fold_newer(&Rec::at(src, layout)).map_or(FloatOverflow::STATUS, |()| 0),
            None => crate::host_abi::agg_miss(ctx, st, src),
        };
        match r {
            0 => core::ptr::write_bytes(src, 0, size),
            s => return s,
        }
    }
    0
}

/// `keyed.twopass`'s admitted set (`ProtoKind::TwoPass`): the protocol writes `words[base]` = this header's address at
/// prepare and fills it after pass 1; pass 2's IR reads it at its open. `set` holds `mask + 1` two-word slots `(w0, w1)`
/// (empty = both `ADMIT_EMPTY`), homed by `KeyHash::of(image).home(cap_bits, Depth::TOP)`, probed linearly; a real key
/// equal to the empty pair is `empty_key` = 1. `count` 0 = pass 2 folds nothing; `empty_key` = `ADMIT_ALL` (with `count`
/// 1 over one empty slot) = pass 2 folds every key: pass 1 spilled partitions and no owner phase finished its region, so
/// it names no whole set (D459; a finished owner result names its exact set after a spill too, D3158).
/// V4R-209: `filter` = 2^`filter_bits` bits (`admit_filter_bits`) that pass 2 tests before the exact set (`admit_filter_add`).
#[repr(C)]
pub struct AdmitHdr {
    pub set: Word<*const u64>,
    pub mask: u64,
    pub count: u64,
    pub empty_key: u64,
    pub filter: Word<*const u64>,
    pub filter_bits: u64,
}


pub const ADMIT_EMPTY: u64 = u64::MAX;
pub const ADMIT_SET: i32 = 0;
pub const ADMIT_MASK: i32 = 8;
pub const ADMIT_COUNT: i32 = 16;
pub const ADMIT_EMPTY_KEY: i32 = 24;
/// `AdmitHdr.empty_key`'s every-key value (D459).
pub const ADMIT_ALL: u64 = 2;
pub const ADMIT_FILTER: i32 = 32;
pub const ADMIT_FILTER_LEN: i32 = 40;

/// V4R-209: the admitted set's L1 prefilter (v2's t3::AdmitSet bitmap, lowerv2 tail/hashagg.rs `admit_test`): a bitmap of
/// 2^`filter_bits` bits, where each key of the exact set sets one bit per depth in `ADMIT_FILTER_AT`, and pass 2 goes on to
/// the exact probe only when both of its bits are set. There are no false negatives: every key's bits are set. The filter
/// reads the top `filter_bits` of the hash bits 32..47 and 48..63 (`home(filter_bits, Depth::part(32 / 48))`). The exact
/// set homes on the top `cap_bits` bits (`Depth::TOP`), under 32 for any set under 2^31 keys, so the two never share a bit;
/// pass 2's own table hashes another image (its whole group key), so no bit of it is consumed here.
/// B3-14: the filter is sized from the key count, `ADMIT_FILTER_PER_KEY` bits per key (two probes at 8 bits per key pass
/// about 5% of the absent keys: (1 - e^(-2/8))^2), at least one word, at most `ADMIT_FILTER_BITS` (the 16 bits each depth
/// reads; 8 KiB, L1-resident). A small set's filter is a few words, so its test costs one cached load per probe; past
/// 2^16 / 8 keys the filter fills and passes more keys to the exact set, which is still correct.
pub const ADMIT_FILTER_BITS: u32 = 16;
pub const ADMIT_FILTER_PER_KEY: usize = 8;
pub const ADMIT_FILTER_AT: [Depth; 2] = [filter_at(32), filter_at(48)];

const fn filter_at(used: u32) -> Depth {
    match Depth::part(used) {
        Some(d) => d,
        None => panic!("an admit filter depth past ROUTE_BITS"),
    }
}

/// The filter's bit count (log2) for an exact set of `keys` keys: `ADMIT_FILTER_PER_KEY` bits per key, 64 ..= 2^16 bits.
pub fn admit_filter_bits(keys: usize) -> u32 {
    (ADMIT_FILTER_PER_KEY * keys).next_power_of_two().trailing_zeros().clamp(6, ADMIT_FILTER_BITS)
}

/// Sets key hash `h`'s bits in a filter of 2^`bits` bits.
pub fn admit_filter_add(filter: &mut [u64], bits: u32, h: KeyHash) {
    for at in ADMIT_FILTER_AT {
        let p = h.home(bits, at);
        filter[p / 64] |= 1 << (p % 64);
    }
}
