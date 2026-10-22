//! count(DISTINCT x) alone over a one-word integer image (a plain distinct over a scan column, no filter before its end):
//! the plain distinct end's window host `agg_distinct_window` (lowerv4 ops/group/distinct_plain.rs: a window's adjacent
//! duplicates dropped and its keys upserted in one host loop) and the pair's insert-count owner `distinct_count_fns`
//! (lowerv4 ops/group/distinct_total.rs). Raw-pointer work over the site's lanes and the owner's table, so it sits in the
//! family's raw half; the family's mod.rs re-exports the module as `ops::group::entries_distinct`, the path its readers name.
use gravity_abi::PtrWord;
use gravity_abi::breaker::T1;
use gravity_abi::hash::KeyHash;
use gravity_abi::limits::DISTINCT_WINDOW_CHUNK;
use gravity_abi::Ctx;

use crate::host_abi::AggState;

/// `agg_distinct_window`'s decline (the window walks its rows).
pub const DW_DECLINED: i64 = 1;

/// KEYED-24: `arg`'s flag for an unsigned word image (`WordImage::Raw { signed: false }`): the key is the lane's bytes
/// zero-extended, not sign-extended.
pub const DW_UNSIGNED: u64 = 1 << 8;

/// NEW-40: the runs word `agg_distinct_window` writes at `st + (arg >> 32)` when that offset is not 0, over a taken window
/// with a non-NULL row: 1 when the keys it kept after dropping adjacent duplicates are at most half its non-NULL rows (the
/// survivors came in runs of equal keys, where the host beats the row loop's walk in table mode), else 0. lowerv4
/// ops/group/distinct_plain.rs `CountEnd::whole` reads it to take the host for the next windows of the same end.
pub fn runs_word(kept: u64, rows: u64) -> u64 {
    (kept * 2 <= rows) as u64
}

crate::contract::host! {
    /// The window phase: window `[cursor, end)` of site `at >> 8` (through its selection when it has one) over the byval
    /// lane `at & 0xff`, a `count(DISTINCT col)` argument of `arg & 0xff` bytes (signed unless `DW_UNSIGNED`), taken whole into the plain distinct
    /// end's table `st` (an `AggState` over the one-word pair `[key, n]`; `(arg >> 16) & 0xffff`: table.small's dense flag
    /// word in the state, 0: none; `arg >> 32`: the end's runs word in the state, 0: none, `runs_word`): the NULL rows
    /// skipped, the adjacent duplicates dropped branch-free per chunk of `DISTINCT_WINDOW_CHUNK` rows, and
    /// each surviving key upserted as the row loop's run close upserts it. Under the bypass the pair record `[key, 1]` is
    /// appended to its partition (the top `pbits` of `KeyHash::word(key)`, `Hashed::append`'s record and route); before it,
    /// the key is located in T1 (`locate`: found, opened, or evicted, the bypass switched on observation, after which the
    /// rest append), and the state's bypass word follows T1 (`sync_bypass`). A dropped duplicate is a record the owner's
    /// dedup drops, so the count is the row loop's. 0: taken; `DW_DECLINED`: not a decoded byval lane of 2 / 4 / 8 bytes, a
    /// dense table.small, or a table that is not the one-word pair's; below zero: a failed flush or locate; `ERROR` when the
    /// body panics. A taken window with a non-NULL row sets the runs word (`runs_word`).
    #[guard(gravity_abi::status::ERROR)]
    pub unsafe fn agg_distinct_window(ctx: *mut Ctx, st: *mut u8, at: u64, cursor: u64, end: u64, arg: u64) -> i64 {
        distinct_window_in(ctx, st, at, None, cursor, end, arg)
    }
}

crate::contract::host! {
    /// KEYED-24: `agg_distinct_window` over one block of a filtered window, the rows its steps passed: position
    /// `base + i` for each set bit `i` of `mask` (block `[base, base + 64)` of the window, through the site's selection when
    /// it has one), the same lane, `arg` and table, and the same statuses: 0 taken, `DW_DECLINED` (a dense table.small,
    /// an undecoded lane) for the caller to walk the block's rows (lowerv4 distinct_plain.rs `CountEnd::block_or_walk`).
    #[guard(gravity_abi::status::ERROR)]
    pub unsafe fn agg_distinct_block(ctx: *mut Ctx, st: *mut u8, at: u64, base: u64, mask: u64, arg: u64) -> i64 {
        distinct_block_in(ctx, st, at, base, mask, arg)
    }
}

/// `agg_distinct_block`'s body: the mask's positions as lane rows, then the window body over them.
#[inline(always)]
unsafe fn distinct_block_in(ctx: *mut Ctx, st: *mut u8, at: u64, base: u64, mask: u64, arg: u64) -> i64 {
    let sel = (*gravity_abi::ctx::site_ptr(ctx, (at >> 8) as usize)).sel.ptr();
    let (mut rows, mut m, mut n) = ([0u32; 64], mask, 0usize);
    while m != 0 {
        let p = base as usize + m.trailing_zeros() as usize;
        rows[n] = if sel.is_null() { p as u32 } else { *sel.add(p) };
        (n, m) = (n + 1, m & (m - 1));
    }
    distinct_window_in(ctx, st, at, Some(rows.as_ptr()), 0, n as u64, arg)
}

/// `agg_distinct_window`'s body, run under the entry's landing pad (a host entry's panic is an ERROR, never an abort);
/// `rows`: the lane rows of positions `[cursor, end)` in place of the site's selection (`agg_distinct_block`'s).
#[inline(always)]
unsafe fn distinct_window_in(ctx: *mut Ctx, st: *mut u8, at: u64, rows: Option<*const u32>, cursor: u64, end: u64, arg: u64) -> i64 {
    use gravity_abi::breaker::{LAYOUT_IMPLICIT_COUNT, T1_BYPASS};
    use gravity_abi::ctx::{lane, site_ptr, Lane};
    let (typlen, dense, runs_at) = (arg & 0xff, ((arg >> 16) & 0xffff) as usize, (arg >> 32) as usize);
    let a = st as *mut AggState;
    let (t, layout) = ((*a).t.ptr(), &*(*a).layout.ptr());
    let pair = layout.nkeys() == 1 && layout.size() == 16 && layout.flags & LAYOUT_IMPLICIT_COUNT != 0 && layout.sentinel_off() == 8;
    if !pair || !matches!(typlen, 2 | 4 | 8) || (dense != 0 && *(st.add(dense) as *const u64) != 0) {
        return DW_DECLINED;
    }
    let s = site_ptr(ctx, (at >> 8) as usize);
    let l = &*((*s).lanes.ptr() as *const Lane).add((at & 0xff) as usize);
    let w = l.width as usize;
    if l.kind != lane::KIND_BYVAL || l.ptr.ptr().is_null() || !matches!(w, 2 | 4 | 8) {
        return DW_DECLINED;
    }
    let valid = if l.valid.ptr().is_null() || l.flags & lane::FLAG_ALL_VALID != 0 { core::ptr::null() } else { l.valid.ptr() };
    // KEYED-24: `arg & DW_UNSIGNED`: an unsigned image (oid): the value's bytes zero-extended, as the row loop images them
    let unsigned = arg & DW_UNSIGNED != 0;
    let shift = if unsigned { 0 } else { 64 - typlen as u32 * 8 };
    let mask = if unsigned && typlen < 8 { (1u64 << (typlen * 8)) - 1 } else { u64::MAX };
    let lanes = DwLane { base: l.ptr.ptr(), sel: rows.unwrap_or((*s).sel.ptr()), valid, shift, mask };
    type Compact = unsafe fn(&DwLane, usize, usize, *mut u64, &mut u64, &mut bool, &mut u64) -> usize;
    macro_rules! pick {
        ($w:literal) => {
            match (!lanes.sel.is_null(), !valid.is_null(), lanes.shift != 0) {
                (false, false, false) => dw_compact::<$w, false, false, false> as Compact,
                (false, false, true) => dw_compact::<$w, false, false, true>,
                (false, true, false) => dw_compact::<$w, false, true, false>,
                (false, true, true) => dw_compact::<$w, false, true, true>,
                (true, false, false) => dw_compact::<$w, true, false, false>,
                (true, false, true) => dw_compact::<$w, true, false, true>,
                (true, true, false) => dw_compact::<$w, true, true, false>,
                (true, true, true) => dw_compact::<$w, true, true, true>,
            }
        };
    }
    let compact = match w {
        8 => pick!(8),
        4 => pick!(4),
        _ => pick!(2),
    };
    let pbits = (*a).pbits as u32;
    // never zeroed: the compact writes each candidate before the slice below reads it. A zeroed chunk cost every
    // `agg_distinct_block` call (one 64-row block) an 8 KiB memset, 31250 of them over a 2M-row filtered window
    let mut chunk = core::mem::MaybeUninit::<[u64; DISTINCT_WINDOW_CHUNK]>::uninit();
    let cand = chunk.as_mut_ptr() as *mut u64;
    let (mut prev, mut first, mut rows, mut kept) = (0u64, true, 0u64, 0u64);
    let mut j = cursor as usize;
    while j < end as usize {
        let n = (end as usize - j).min(DISTINCT_WINDOW_CHUNK);
        let k = compact(&lanes, j, n, cand, &mut prev, &mut first, &mut rows);
        kept += k as u64;
        // SAFETY: `compact` wrote `cand[..k]` (k <= n <= the chunk)
        let keys = core::slice::from_raw_parts(cand as *const u64, k);
        let mut i = 0usize;
        // the table's form: each key located until the bypass is on
        while i < k && (*t).flags & T1_BYPASS == 0 {
            if let Err(e) = crate::host_abi::locate(ctx, a, &keys[i..i + 1]) {
                return e;
            }
            i += 1;
        }
        if i < k {
            let r = dw_scatter(ctx, a, &keys[i..k], pbits);
            if r < 0 {
                return r;
            }
        }
        j += n;
    }
    if (*t).flags & T1_BYPASS != 0 {
        *(st.add(crate::host_abi::AGG_BYPASS_WORD) as *mut u64) = T1_BYPASS as u64;
    }
    if runs_at != 0 && rows != 0 {
        *(st.add(runs_at) as *mut u64) = runs_word(kept, rows);
    }
    0
}

/// The bypass: each key's pair record `[key, 1]` appended to its partition; a full pool flushed once (`agg_flush`). 0, or the
/// flush's status.
#[inline(never)]
unsafe fn dw_scatter(ctx: *mut Ctx, a: *mut AggState, keys: &[u64], pbits: u32) -> i64 {
    let t = (*a).t.ptr();
    let mut ap = T1::appender(t);
    for &v in keys {
        let part = KeyHash::word(v).part(pbits);
        let dst = match ap.reserve(part) {
            Some(d) => d,
            None => {
                ap.finish();
                if let Err(e) = crate::host_abi::agg_flush(ctx, a) {
                    return e;
                }
                ap = T1::appender(t);
                match ap.reserve(part) {
                    Some(d) => d,
                    None => return gravity_abi::status::unsupported(gravity_abi::status::UNSUPPORTED_SCRATCH),
                }
            }
        };
        (dst as *mut u64).write(v);
        (dst as *mut u64).add(1).write(1);
    }
    ap.finish();
    0
}

/// The window's argument lane as `dw_compact` reads it.
struct DwLane {
    base: *const u8,
    sel: *const u32,
    valid: *const u64,
    shift: u32,
    /// KEYED-24: the image's bytes (all ones but for an unsigned image narrower than the word: its zero extension).
    mask: u64,
}

/// Rows `[j, j + n)` (entries of the selection when `SEL`; the NULL rows skipped when `VALID`) of a `W`-byte lane, each key's
/// image word (`EXT`: its low bytes sign-extended past `shift`) written to `cand` and kept unless it repeats the previous
/// row's (`prev`, none before the window's first row); the kept count, the non-NULL rows added to `rows`. Branch-free: the
/// write is unconditional, the count steps by the compare.
#[inline(never)]
unsafe fn dw_compact<const W: usize, const SEL: bool, const VALID: bool, const EXT: bool>(l: &DwLane, j: usize, n: usize, cand: *mut u64, prev: &mut u64, first: &mut bool, rows: &mut u64) -> usize {
    let key = |r: usize| -> u64 {
        let v = match W {
            8 => core::ptr::read_unaligned(l.base.add(8 * r) as *const u64),
            4 => core::ptr::read_unaligned(l.base.add(4 * r) as *const i32) as i64 as u64,
            _ => core::ptr::read_unaligned(l.base.add(2 * r) as *const i16) as i64 as u64,
        };
        (if EXT { (((v << l.shift) as i64) >> l.shift) as u64 } else { v }) & l.mask
    };
    let (mut k, mut p, mut f, mut m) = (0usize, *prev, *first, 0u64);
    for i in j..j + n {
        let r = if SEL { *l.sel.add(i) as usize } else { i };
        if VALID && *l.valid.add(r >> 6) >> (r & 63) & 1 == 0 {
            continue;
        }
        let v = key(r);
        *cand.add(k) = v;
        k += (f | (v != p)) as usize;
        (f, p, m) = (false, v, m + 1);
    }
    (*prev, *first) = (p, f);
    *rows += m;
    k
}

/// The owner phase: the plain one-word distinct owner (`count(DISTINCT x)` alone over the pair `[x, n]`, lowerv4
/// ops/group/distinct_total.rs names it through `OwnerDesc::ctor_desc`) as an insert-count: each partition's table is
/// the region's `2 · cap` 8-byte words (the driver lays out and zeroes `cap` 16-byte records), holding `KeyHash::word(x)`,
/// a bijection, so a slot's word is the key and 0 is empty; the key 0 (hash 0) is flagged apart (`DC_ZERO`) and counted
/// once. A record's home is the hash's bits below its route bits (`KeyHash::home`), probed linearly. The table's `len` is
/// the partition's distinct count, summed into `OwnerTop.total` (what `pairs_total` reads), as the generic plain form's.
pub fn distinct_count_fns() -> gravity_abi::breaker::OwnerFns {
    use gravity_abi::breaker::{OwnerFns, ORDER_NONE, OWNER_PARAMS};
    OwnerFns { params: [0; OWNER_PARAMS], rounds: 1, order_slot: ORDER_NONE, order_slot2: ORDER_NONE, min_bytes: dc_min, open: dc_open, reduce: dc_reduce, end_round: dc_end, reduce2: dc_reduce, fold: Some(dc_fold) }
}

/// `HashAgg.flags`: the partition's key 0 was counted.
const DC_ZERO: u8 = 0x80;

fn dc_min(_params: *const u64, _owners: u32) -> u64 {
    (gravity_abi::breaker::OWNERTOP_HDR + gravity_abi::breaker::HASHAGG_HDR + 64 * 16) as u64
}

unsafe fn dc_open(_params: *const u64, owner: *mut u8, bytes: usize) -> i64 {
    use gravity_abi::breaker::{OwnerTop, HASHAGG_HDR, OWNERTOP_HDR};
    if OWNERTOP_HDR + HASHAGG_HDR + 2 * 16 > bytes {
        return gravity_abi::status::unsupported(gravity_abi::status::UNSUPPORTED_SCRATCH);
    }
    *(owner as *mut OwnerTop) = OwnerTop { len: 0, n: 0, table_off: OWNERTOP_HDR as u32, worst: 0, round2_off: 0, resume: 0, total: 0, groups: 0, records: 0, thr: 0, key_lo: gravity_abi::breaker::KEY_LO_NONE, key_hi: gravity_abi::breaker::KEY_HI_NONE, cell: core::cell::UnsafeCell::new([]) };
    OWNERTOP_HDR as i64
}

unsafe fn dc_reduce(_params: *const u64, owner: *mut u8, table: *const gravity_abi::breaker::HashAgg, _site: *mut gravity_abi::Site, _cand: *const u32, _ncand: i64) -> i64 {
    let h = &mut *(owner as *mut gravity_abi::breaker::OwnerTop);
    let n = (*table).len as u64;
    h.records += n;
    h.total += n;
    0
}

unsafe fn dc_end(_params: *const u64, _owner: *mut u8) -> i64 {
    0
}

/// `FoldFn` over the pair records `[x, n]` (16 bytes): the records folded, or `-1 - folded` past 7/8 of the words (the driver
/// refolds or splits the partition).
unsafe fn dc_fold(_params: *const u64, t: *mut gravity_abi::breaker::HashAgg, recs: *const u8, nrecs: usize, split: gravity_abi::breaker::SubSplit, _cand: *mut gravity_abi::breaker::Candidates) -> i64 {
    let words = 2 * (*t).cap as usize;
    let (cb, at) = (words.trailing_zeros(), (*t).home_bits().1);
    let (slots, mask) = ((*t).base_ptr() as *mut u64, words - 1);
    // at least one word stays empty, so a probe ends (D1029: a cap-2 table's 7/8 of 4 words rounded to all 4)
    let full_at = (words - words / 8).min(words - 1) as u32;
    let (mut len, mut folded) = ((*t).len, 0i64);
    for i in 0..nrecs {
        let kh = KeyHash::word(*(recs.add(16 * i) as *const u64));
        if split.depth != 0 && !split.keeps(kh) {
            continue;
        }
        folded += 1;
        let h = kh.to_abi();
        if h == 0 {
            len += ((*t).flags & DC_ZERO == 0) as u32;
            (*t).flags |= DC_ZERO;
            continue;
        }
        let mut j = kh.home(cb, at);
        loop {
            let s = *slots.add(j);
            if s == h {
                break;
            }
            if s == 0 {
                if len >= full_at {
                    (*t).len = len;
                    return -folded;
                }
                *slots.add(j) = h;
                len += 1;
                break;
            }
            j = (j + 1) & mask;
        }
    }
    (*t).len = len;
    folded
}
