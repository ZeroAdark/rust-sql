//! `code.front_light`'s hosts (keyed/codefront_light.rs, V4R-184; a #[path] submodule of host_abi/v4/m3.rs): the ladder's
//! rung-3 text front (gravity_lower/text.rs at ec2df04e1fc: `h_bind`, `convert`). A count top keyed by one code counts each
//! row as `cnt[code] += 1` over the part the claim reads (the program's row loop, no key image, no hash); the part's counts
//! are added into the claim's T1, one record per touched code (V4R-191: counted in place), once per part: when a window of
//! another part binds, and at the claim's end. Created by M3 (owner M3-HOSTS; written by M3-LEVERS-A).
//!
//! Memory: the counts and the list are `u32` per code (8 bytes a code, at most the cost home's `LIGHT_MAX_CODES`: 8 MiB a
//! worker, plus the list's spare entry), from the claim's arena, which draws on the statement's grant (a refused draw
//! leaves the part on the hashed table) and is released with the claim's scratch; nothing outlives the statement.
use gravity_abi::{word, PtrWord, Word};
use gravity_abi::breaker::{light_claim_takes, light_dense, light_keeps, light_takes, light_ways, prefetch_read, Appender, KeyCell, Rec, RecordLayout, CODE_FRONT_MIN_CODES, LIGHT_KEYS_MAX_GROUPS, LIGHT_MAX_CODES, LIGHT_WAYS, T1, T1_BYPASS};
use gravity_abi::hash::KeyHash;
use gravity_abi::ctx::{lane, site_ptr};
use gravity_abi::Ctx;

use crate::host_abi::AggState;
use crate::table::image::AlignedImage;

/// The front's words in the end's state, after the hashed table's (the program reads `cnt`, `touched`, `n` and `cap` per
/// row, `ways` and `alloc` per row the list does not take, and `part` per window): `cap` = the codes the row loop counts
/// in one array and lists (the bound part's `ncodes`; 0: the part runs on the hashed table or counts in ways), `alloc` =
/// the arrays' capacity in codes (a way's stride in `cnt`), `part` = the bound part (`LW_NO_PART` before the claim's
/// first window), `fps` = its Fp128 table, `ways` = the codes a row counts in ways (D3159, `light_ways`: the bound part's
/// `ncodes`, row `at` counted into way `at % LIGHT_WAYS`, no list; 0: not in ways), `wcap` = the ways the count array
/// holds (1 or `LIGHT_WAYS`). A part in ways binds `cap` 0, so the list's row code is the same whether or not a part of
/// the claim counts in ways: the ways' test rides the row that missed the list (the hashed table's arm).
#[repr(C)]
pub struct LightWords {
    pub cnt: Word<*mut u32>,
    pub touched: Word<*mut u32>,
    pub n: u64,
    pub cap: u64,
    pub alloc: u64,
    pub part: u64,
    pub fps: Word<*const [u64; 2]>,
    pub ncodes: u64,
    pub ways: u64,
    pub wcap: u64,
}
/// The fields' byte offsets the IR reads and writes (lowerv4 dict/strategies/codefront_light.rs): `LightWords`' own layout.
pub const LW_CNT: i32 = core::mem::offset_of!(LightWords, cnt) as i32;
pub const LW_TOUCHED: i32 = core::mem::offset_of!(LightWords, touched) as i32;
pub const LW_N: i32 = core::mem::offset_of!(LightWords, n) as i32;
pub const LW_CAP: i32 = core::mem::offset_of!(LightWords, cap) as i32;
pub const LW_PART: i32 = core::mem::offset_of!(LightWords, part) as i32;
pub const LW_ALLOC: i32 = core::mem::offset_of!(LightWords, alloc) as i32;
pub const LW_WAYS: i32 = core::mem::offset_of!(LightWords, ways) as i32;
pub const LW_BYTES: u32 = core::mem::size_of::<LightWords>() as u32;
pub const LW_NO_PART: u64 = u64::MAX;

/// A key's entry after the words in a front of two or more code keys (`front_light_bind_keys`): the bound part's Fp128
/// table, the next part's (the program stores it before the bind, which converts the bound part first), the key's
/// dictionary size and its stride in the count index (the product of the earlier keys' sizes), set at each bind; its
/// code lane and representative slot (`LD_NONE`: none), set at the claim's open.
#[repr(C)]
pub struct LightKey {
    pub fps: Word<*const [u64; 2]>,
    pub next: Word<*const [u64; 2]>,
    pub size: u64,
    pub stride: u64,
    pub lane: u64,
    pub rep: u64,
}
pub const LK_NEXT: i32 = core::mem::offset_of!(LightKey, next) as i32;
pub const LK_SIZE: i32 = core::mem::offset_of!(LightKey, size) as i32;
pub const LK_STRIDE: i32 = core::mem::offset_of!(LightKey, stride) as i32;
pub const LK_LANE: i32 = core::mem::offset_of!(LightKey, lane) as i32;
pub const LK_REP: i32 = core::mem::offset_of!(LightKey, rep) as i32;
pub const LK_BYTES: u32 = core::mem::size_of::<LightKey>() as u32;
/// The word after a front's keys' entries (two or more keys): 1 once a part the front counted held fewer rows a touched
/// count than the cost home's break-even (`light_keeps`), so the later parts take the hashed table. The program copies it in
/// from its frame word at each claim's open and back at the claim's end (lowerv4 codefront_light.rs `with_declined`): a
/// claim never crosses a part's edge, so the decline is the statement's (the worker's), not the claim's.
pub const LK_DECLINED_BYTES: u32 = 8;
/// The most code keys a front counts by: keys of two or more codes each fit `LIGHT_KEYS_MAX_GROUPS` at most its log2 at a
/// time.
pub const LIGHT_KEYS_MAX: usize = LIGHT_KEYS_MAX_GROUPS.trailing_zeros() as usize;
/// A front of `LIGHT_KEYS_MAX` keys' image: two words a key (its Fp128 entry) and one flags word.
const IMG_MAX: usize = 2 * LIGHT_KEYS_MAX + 1;
/// The convert's prefetch distance, in codes (the ladder's `CONVERT_PF`).
const LIGHT_PF: u64 = 16;
/// `desc`'s fields: the count slot (bits 0..8), the representative slot (8..16, `LD_NONE`: none), the image words (16..24),
/// the code lane (24..40), the site (40..56), `LD_WAYS` (bit 56). A front of two or more keys (`front_light_bind_keys`):
/// the count slot, the image words, the keys (24..32) and the site; each key's lane and representative are its `LightKey`.
pub const LD_NONE: u64 = 0xff;
/// D3159: one key's program tests the ways on the rows its list misses, so its bind may count a part in ways (lowerv4 sets
/// it when the planner's group estimate fits `light_ways_max_codes()`; a key estimated past it has no ways' test in its row
/// loop, and its parts keep the one array and the list).
pub const LD_WAYS: u64 = 1 << 56;

crate::contract::host! {
    /// The row loop's words for part `part` (the window's `part_id`, bound when it is not the bound one): the bound part's
    /// counts converted first, then this part's Fp128 table `fps` and its dictionary's code count (the code lane's
    /// `ncodes_or_scale`), the arrays grown past it (a power of two, zeroed); a part past `LIGHT_MAX_CODES`, a lane
    /// that is not codes, a claim of too few rows to fill the part's arrays (the cost home's `light_claim_takes`) or a
    /// refused draw leaves `cap` 0. 0, or the convert's negative status.
    pub unsafe fn front_light_bind(ctx: *mut Ctx, st: *mut u8, w: *mut u8, desc: u64, part: u64, fps: *const u8) -> i64 {
        guard!(gravity_abi::status::ERROR, bind(ctx, st, &mut *(w as *mut LightWords), desc, part, fps as *const [u64; 2]))
    }
}

crate::contract::host! {
    /// The bound part's counts into the claim's T1 at the claim's end. 0, or a negative status.
    pub unsafe fn front_light_flush(ctx: *mut Ctx, st: *mut u8, w: *mut u8, desc: u64) -> i64 {
        guard!(gravity_abi::status::ERROR, convert(ctx, st, &mut *(w as *mut LightWords), desc))
    }
}

crate::contract::host! {
    /// `front_light_bind` for two or more code keys (`desc`'s keys; each key's `LightKey` after the words, its `next`
    /// table stored by the program): the bound part's counts converted, then each key's table, size and stride; the row
    /// loop counts the index `sum code_k * stride_k`, below the product of the sizes. A product past `LIGHT_MAX_CODES`
    /// or over half the bound window's rows (the cost home's `light_takes`: fewer than `LIGHT_ROWS_A_COUNT` rows a count),
    /// a statement the convert declined (a part it counted under the break-even's rows a touched count), a lane that is not codes or a
    /// refused draw leaves `cap` 0. 0, or the convert's negative status.
    pub unsafe fn front_light_bind_keys(ctx: *mut Ctx, st: *mut u8, w: *mut u8, desc: u64, part: u64) -> i64 {
        guard!(gravity_abi::status::ERROR, bind_keys(ctx, st, w as *mut LightWords, desc, part))
    }
}

crate::contract::host! {
    /// `front_light_flush` for two or more code keys.
    pub unsafe fn front_light_flush_keys(ctx: *mut Ctx, st: *mut u8, w: *mut u8, desc: u64) -> i64 {
        guard!(gravity_abi::status::ERROR, convert_keys(ctx, st, w as *mut LightWords, desc))
    }
}

/// Key `k`'s entry after the words.
#[inline(always)]
unsafe fn key(w: *mut LightWords, k: usize) -> *mut LightKey {
    (w.add(1) as *mut LightKey).add(k)
}

/// The declined word after `nkeys` keys' entries (`LK_DECLINED_BYTES`).
#[inline(always)]
unsafe fn declined(w: *mut LightWords, nkeys: usize) -> *mut u64 {
    key(w, nkeys) as *mut u64
}

#[inline(always)]
unsafe fn bind_keys(ctx: *mut Ctx, st: *mut u8, w: *mut LightWords, desc: u64, part: u64) -> i64 {
    if (*w).part != part {
        let r = convert_keys(ctx, st, w, desc);
        if r < 0 {
            return r;
        }
        (*w).part = part;
    }
    (*w).cap = 0;
    let (site, nkeys) = (site_ptr(ctx, (desc >> 40 & 0xffff) as usize), (desc >> 24 & 0xff) as usize);
    if *declined(w, nkeys) != 0 {
        return 0;
    }
    let mut product = 1u64;
    for k in 0..nkeys {
        let e = &mut *key(w, k);
        e.fps = word(e.next.ptr());
        let l = gravity_abi::site::lane(site, e.lane as usize);
        (e.size, e.stride) = (l.ncodes_or_scale as u64, product);
        product = product.saturating_mul(e.size);
        if l.kind != lane::KIND_CODES || product > LIGHT_MAX_CODES {
            return 0;
        }
    }
    // a product past half the bound window's rows (the part's first) counts too few rows a count: the hashed table
    if !light_takes(product, (*site).rows as u64) {
        return 0;
    }
    (*w).ncodes = product;
    (*w).ways = 0;
    grow(ctx, &mut *w)
}

/// The bound part's counts converted (`walk_by`); a part the front counted (`cap` not 0) at fewer rows a touched count
/// than the cost home's break-even (`light_keeps` of the touched counts and the counted rows: a filter thinned them, or
/// near uniform tuples at the bind's edge) declines the front for the statement's later parts (D2804's run-time decline:
/// the hashed table, the same records).
#[inline(always)]
unsafe fn convert_keys(ctx: *mut Ctx, st: *mut u8, w: *mut LightWords, desc: u64) -> i64 {
    let (ix, latch) = (Many { keys: key(w, 0), n: (desc >> 24 & 0xff) as usize }, declined(w, (desc >> 24 & 0xff) as usize));
    let w = &mut *w;
    let (touched, mut rows) = (if w.cap != 0 { w.n } else { 0 }, 0u64);
    let r = walk_by::<_, 1>(ctx, st, w, desc, w.n * light_dense() >= w.ncodes, ix, &mut rows);
    if touched != 0 && !light_keeps(touched, rows) {
        *latch = 1;
    }
    r
}

#[inline(always)]
unsafe fn bind(ctx: *mut Ctx, st: *mut u8, w: &mut LightWords, desc: u64, part: u64, fps: *const [u64; 2]) -> i64 {
    if w.part != part {
        let r = convert(ctx, st, w, desc);
        if r < 0 {
            return r;
        }
        w.part = part;
    }
    w.fps = word(fps);
    (w.cap, w.ways) = (0, 0);
    let site = site_ptr(ctx, (desc >> 40 & 0xffff) as usize);
    let l = gravity_abi::site::lane(site, (desc >> 24 & 0xffff) as usize);
    w.ncodes = l.ncodes_or_scale as u64;
    // the claim's rows: its granules' windows (the window's rows a granule); a claim of fewer rows than fill the part's
    // arrays counts each code it touches about once and walks its list at random (W8: 4 granules over 1 M codes)
    let rows = (*ctx).claim.len().saturating_mul((*site).rows as u64);
    if l.kind != lane::KIND_CODES || w.ncodes > LIGHT_MAX_CODES || !light_claim_takes(w.ncodes, rows) {
        return 0;
    }
    // D3159: a part of few codes a row counts in ways (the cost home's `light_ways` at the bound window's rows), when the
    // program tests the ways (`LD_WAYS`)
    if desc & LD_WAYS != 0 && light_ways(w.ncodes, (*site).rows as u64) {
        w.ways = w.ncodes;
    }
    grow(ctx, w)
}

/// The arrays grown past the bound part's `ncodes` (a power of two, zeroed; `LIGHT_WAYS` count arrays when the part counts
/// in ways), then `cap` set to it, or (a part in ways) `cap` 0 and `ways` kept; a refused draw leaves both 0. The counts
/// are all zero here (the bind converted the bound part first), so fresh arrays start zeroed (the old ones are the
/// claim's scratch, and a claim never crosses a part's edge). A part in ways marks `n` (it keeps no list: the convert
/// walks its codes in code order). 0.
#[inline(always)]
unsafe fn grow(ctx: *mut Ctx, w: &mut LightWords) -> i64 {
    let ways = if w.ways != 0 { LIGHT_WAYS } else { 1 };
    if w.ncodes > w.alloc || ways > w.wcap {
        // the list holds one entry past the codes: the row loop writes `touched[n]` on every row, `n` counting fresh codes
        let cap = w.ncodes.next_power_of_two().max(CODE_FRONT_MIN_CODES) as usize;
        let wcap = ways as usize;
        let (c, t) = (crate::util::scratch(ctx, cap * 4 * wcap), crate::util::scratch(ctx, cap * 4 + 8));
        if c.is_null() || t.is_null() {
            (w.alloc, w.wcap, w.ways) = (0, 0, 0);
            return 0;
        }
        core::ptr::write_bytes(c, 0, cap * 4 * wcap);
        (w.cnt, w.touched, w.alloc, w.wcap) = (word(c as *mut u32), word(t as *mut u32), cap as u64, wcap as u64);
    }
    if w.ways != 0 {
        (w.cap, w.n) = (0, 1);
    } else {
        w.cap = w.ncodes;
    }
    0
}

/// Each touched code of the bound part folded into its record in the claim's T1, counted in place (V4R-191; the ladder's
/// held `convert`): the record of the code's image (the part's Fp128 entry, a clear flags word) found by the table's probe,
/// or, under the bypass, opened in its partition's buffer through one appender (V4R-200: the ladder's bypass form), else
/// M1's `agg_locate` (grown in place, else `locate`: opened, found, or appended after a flush), then its count slot
/// plus the code's count and its representative `min_code(part, code)`, as `Rec::fold` of the row path's record would
/// (that record: a valid count and representative, every other slot clear); each count zeroed behind it, so the arrays
/// stay zero between parts. A part that touched at least one code in `light_dense()` (the cost home's break-even of the
/// two walks on this host) walks the counts in code order, another the list; the walk prefetches the Fp128 entry
/// `2 * LIGHT_PF` codes ahead and, in the table, that code's home pair `LIGHT_PF` ahead.
/// D3159: a part counted in ways walks in code order, each code's count the sum of its ways.
#[inline(always)]
unsafe fn convert(ctx: *mut Ctx, st: *mut u8, w: &mut LightWords, desc: u64) -> i64 {
    walk(ctx, st, w, desc, w.n * light_dense() >= w.ncodes)
}

/// `convert` in code order (`dense`) or over the touched list.
#[inline(always)]
unsafe fn walk(ctx: *mut Ctx, st: *mut u8, w: &mut LightWords, desc: u64, dense: bool) -> i64 {
    if w.n == 0 {
        return 0;
    }
    let (a_st, rep) = (st as *mut AggState, desc >> 8 & 0xff);
    let rep_off = if rep != LD_NONE { (*(*a_st).layout.ptr()).slot_off(rep as usize) as usize } else { 0 };
    let ix = One { fps: w.fps.ptr(), rep, rep_off };
    match w.ways {
        0 => walk_by::<_, 1>(ctx, st, w, desc, dense, ix, &mut 0),
        _ => walk_ways(ctx, st, w, desc, ix),
    }
}

/// D3159: a part counted in ways walks out of line, so the list's walk in `convert`'s two callers (the flush and the
/// bind) stays the size of trunk's (a key of many codes, hot-nc G026's title, never counts in ways). One call a convert.
#[inline(never)]
unsafe fn walk_ways(ctx: *mut Ctx, st: *mut u8, w: &mut LightWords, desc: u64, ix: One) -> i64 {
    walk_by::<_, { LIGHT_WAYS as usize }>(ctx, st, w, desc, true, ix, &mut 0)
}

/// How a walk reads a count index: one key's code (`One`), or two or more keys' codes under their strides (`Many`).
trait CountIndex: Copy {
    type Img: AsRef<[u64]>;
    /// Index `i`'s image: each key's Fp128 entry in key order, then a clear flags word.
    unsafe fn image(self, i: u32) -> Self::Img;
    /// Index `i`'s Fp128 entries prefetched.
    unsafe fn prefetch(self, i: u32);
    /// Index `i`'s representatives stored in a fresh record `d` (the bypass's record written once).
    unsafe fn store_reps(self, d: *mut u8, layout: &RecordLayout, part: u32, i: u32);
    /// Index `i`'s representatives folded into `r`.
    unsafe fn fold_reps(self, r: &Rec<'_>, part: u32, i: u32);
}

#[derive(Clone, Copy)]
struct One {
    fps: *const [u64; 2],
    rep: u64,
    rep_off: usize,
}

impl CountIndex for One {
    type Img = AlignedImage<3>;
    #[inline(always)]
    unsafe fn image(self, i: u32) -> AlignedImage<3> {
        let fp = *self.fps.add(i as usize);
        AlignedImage([fp[0], fp[1], 0])
    }
    #[inline(always)]
    unsafe fn prefetch(self, i: u32) {
        prefetch_read(self.fps.add(i as usize) as *const u8);
    }
    #[inline(always)]
    unsafe fn store_reps(self, d: *mut u8, _: &RecordLayout, part: u32, i: u32) {
        if self.rep != LD_NONE {
            *(d.add(self.rep_off) as *mut u64) = KeyCell::code_word(part, i);
        }
    }
    #[inline(always)]
    unsafe fn fold_reps(self, r: &Rec<'_>, part: u32, i: u32) {
        if self.rep != LD_NONE {
            r.min_code(self.rep as usize, part, i);
        }
    }
}

#[derive(Clone, Copy)]
struct Many {
    keys: *const LightKey,
    n: usize,
}

impl Many {
    /// Key `k`'s entry and its code in index `i`.
    #[inline(always)]
    unsafe fn code(self, k: usize, i: u32) -> (*const LightKey, u32) {
        let e = self.keys.add(k);
        (e, (i as u64 / (*e).stride % (*e).size) as u32)
    }
}

impl CountIndex for Many {
    type Img = AlignedImage<IMG_MAX>;
    #[inline(always)]
    unsafe fn image(self, i: u32) -> AlignedImage<IMG_MAX> {
        let mut img = AlignedImage([0u64; IMG_MAX]);
        for k in 0..self.n {
            let (e, c) = self.code(k, i);
            let fp = *(*e).fps.ptr().add(c as usize);
            (img.0[2 * k], img.0[2 * k + 1]) = (fp[0], fp[1]);
        }
        img
    }
    #[inline(always)]
    unsafe fn prefetch(self, i: u32) {
        for k in 0..self.n {
            let (e, c) = self.code(k, i);
            prefetch_read((*e).fps.ptr().add(c as usize) as *const u8);
        }
    }
    #[inline(always)]
    unsafe fn store_reps(self, d: *mut u8, layout: &RecordLayout, part: u32, i: u32) {
        for k in 0..self.n {
            let (e, c) = self.code(k, i);
            if (*e).rep != LD_NONE {
                *(d.add(layout.slot_off((*e).rep as usize) as usize) as *mut u64) = KeyCell::code_word(part, c);
            }
        }
    }
    #[inline(always)]
    unsafe fn fold_reps(self, r: &Rec<'_>, part: u32, i: u32) {
        for k in 0..self.n {
            let (e, c) = self.code(k, i);
            if (*e).rep != LD_NONE {
                r.min_code((*e).rep as usize, part, c);
            }
        }
    }
}

/// `walk` over index reader `ix` of a part counted in `WAYS` count arrays (stride `alloc`; `WAYS` > 1 walks in code order),
/// the counted rows added to `rows`.
#[inline(always)]
unsafe fn walk_by<I: CountIndex, const WAYS: usize>(ctx: *mut Ctx, st: *mut u8, w: &mut LightWords, desc: u64, dense: bool, ix: I, rows: &mut u64) -> i64 {
    if w.n == 0 {
        return 0;
    }
    let (a_st, part) = (st as *mut AggState, w.part as u32);
    let (layout, t) = (&*(*a_st).layout.ptr(), (*a_st).t.ptr());
    let size = layout.size() as usize;
    let mut ap: Option<Appender> = None;
    let (count, nk) = ((desc & 0xff) as usize, (desc >> 16 & 0xff) as usize);
    // a layout without the validity word (every slot valid by its value) takes the bypass's record written once
    let once = !layout.has_validity();
    let count_off = layout.slot_off(count) as usize;
    let n = if dense { w.ncodes } else { w.n };
    let code_at = |i: u64| if dense { i as u32 } else { *w.touched.ptr().add(i as usize) };
    let stride = w.alloc as usize;
    // the code's count over the ways, and the ways cleared behind it
    let count_of = |c: *mut u32| (0..WAYS).fold(0u64, |a, k| a + *c.add(k * stride) as u64);
    let clear = |c: *mut u32| (0..WAYS).for_each(|k| *c.add(k * stride) = 0);
    for i in 0..n {
        let held = (*t).flags & T1_BYPASS == 0;
        if i + 2 * LIGHT_PF < n {
            ix.prefetch(code_at(i + 2 * LIGHT_PF));
        }
        if held && i + LIGHT_PF < n {
            (*t).prefetch_home(KeyHash::of(&ix.image(code_at(i + LIGHT_PF)).as_ref()[..nk]));
        }
        let code = code_at(i);
        let c = w.cnt.ptr().add(code as usize);
        let v = count_of(c);
        if v == 0 {
            continue;
        }
        *rows += v;
        let img = ix.image(code);
        let keys = &img.as_ref()[..nk];
        let p = if held {
            if let Some(mut a) = ap.take() {
                a.finish();
            }
            // the probe finds or opens the record in its home pair as `locate`'s first step; a full pair, the estimate's
            // bypass and the bypass's append are M1's (a probe-opened record is folded into at once, before the next probe)
            T1::probe(t, keys, KeyHash::of(keys))
        } else {
            // the bypass (V4R-200, the ladder's `Front::convert` form): the record opened in place in its partition's
            // buffer through one appender (`locate`'s route and `append_new`'s zeroed open), its head words held across
            // the walk; a full pool finishes the appender and takes M1's locate below (its flush), then a fresh one
            let a = ap.get_or_insert_with(|| T1::appender(t));
            let at = a.reserve((*t).route(keys, KeyHash::of(keys)).part((*a_st).pbits as u32));
            if at.is_none() {
                ap.take().unwrap().finish();
            }
            if let Some(d) = at {
                core::ptr::write_bytes(d, 0, size);
                Rec::at(d, layout).open(keys);
                if once {
                    // count-at-append (V4R-210): the fresh record's count and representative stored, never read back (a read
                    // of the just-reserved line waited on its fill); `Rec::open` left the representative at MINCODE_NONE
                    *(d.add(count_off) as *mut u64) = v;
                    ix.store_reps(d, layout, part, code);
                    clear(c);
                    continue;
                }
            }
            at
        };
        let p = match p {
            Some(p) => p,
            None => match crate::table::entries::agg_locate(ctx, st, keys.as_ptr()) {
                s if s < 0 => return s,
                p => p as *mut u8,
            },
        };
        let r = Rec::at(p, layout);
        r.add_count(count, v as i64);
        ix.fold_reps(&r, part, code);
        clear(c);
    }
    if let Some(mut a) = ap {
        a.finish();
    }
    w.n = 0;
    0
}

#[cfg(test)]
mod fit {
    //! The convert's walks timed alone over the same counts: the cost home's unit costs (`LIGHT_WALK`, `LIGHT_LIST`,
    //! `LIGHT_WAY_WALK`, _support/cost_profile front.rs) on synthetic parts at the dev seed. Run on a quiet box, optimized:
    //!   cargo test --profile fast-profile -p gravity_kernels --lib dict::strategies::front_light::fit -- --ignored --nocapture
    use super::*;
    use gravity_abi::breaker::{HashAgg, KeyKind, RecordLayout, SlotKind};
    use gravity_abi::ctx::ctx_size;
    use gravity_abi::{Partials, ProgramBlock};
    use cost_profile::fit::{break_even_div, least_squares};
    use std::time::Instant;

    const DEV_SEED: u64 = 0x5eed_0de7;
    const KEYS: [KeyKind; 3] = [KeyKind::I64; 3];
    const SLOTS: [SlotKind; 2] = [SlotKind::Count, SlotKind::MinCode];
    const LAYOUT: RecordLayout = RecordLayout::new_nn(&KEYS, &SLOTS);
    /// The count slot 0, the representative slot 1, three image words.
    const DESC: u64 = 1 << 8 | 3 << 16;
    const SHARE: u32 = 64 << 20;

    /// The driver's pool scatter, its records dropped: a timed walk that fills the pool (the bypass appends a record per
    /// touched code each walk) drains it and goes on.
    unsafe extern "C" fn scatter(ctx: *mut Ctx, op: u32, _arg: u64) -> i64 {
        if op != gravity_abi::program::SPILL_SCATTER {
            return 0; // agg_open's reservation ask (SPILL_REGION_COMMIT, option c'): the rig's share is not reserved
        }
        let t = gravity_abi::program::partial(ctx) as *mut T1;
        while !T1::drain(t) {
            (*t).reset_pool();
        }
        (*t).reset_pool();
        0
    }

    fn next(s: &mut u64) -> u64 {
        *s ^= *s >> 12;
        *s ^= *s << 25;
        *s ^= *s >> 27;
        s.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Nanoseconds per walk (`dense` or the list; `ways`: the `LIGHT_WAYS` arrays in code order) of `touched` codes of an
    /// `ncodes`-code part, every touched code's record already in the claim's table: the least of five takes of at least
    /// ~20 ms each. Each walk is preceded by the same refill of its counts and list (every walk pays it).
    unsafe fn time(ncodes: u64, touched: &[u32], dense: bool, ways: bool) -> f64 {
        let mut own = vec![0u64; SHARE as usize / 8]; // alloc: Cold
        HashAgg::arm(own.as_mut_ptr() as *mut u8, SHARE);
        let mut partials = Box::new(Partials::two(own.as_mut_ptr() as *mut u8, [core::ptr::null_mut(); 2])); // alloc: Cold
        let mut program = Box::new(ProgramBlock::NONE); // alloc: Cold
        program.partial = &mut *partials;
        program.spill = Some(scatter);
        let mut cv = vec![0u64; ctx_size(0) / 8]; // alloc: Cold
        let ctx = cv.as_mut_ptr() as *mut Ctx;
        (*ctx).program = &mut *program;
        let mut st = [0u64; crate::host_abi::AGG_STATE_BYTES as usize / 8];
        let sp = st.as_mut_ptr() as *mut u8;
        assert_eq!(crate::host_abi::agg_open(ctx, sp, &LAYOUT, 1, 0), 0);
        let fps: Vec<[u64; 2]> = (0..ncodes).map(|c| [(c + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15), c ^ 0x5555]).collect();
        let wn = if ways { LIGHT_WAYS } else { 1 };
        let (mut cnt, mut list) = (vec![0u32; (ncodes * wn) as usize], vec![0u32; touched.len() + 1]); // alloc: Cold
        let mut w = LightWords { cnt: word(cnt.as_mut_ptr()), touched: word(list.as_mut_ptr()), n: 0, cap: if ways { 0 } else { ncodes }, alloc: ncodes, part: 7, fps: word(fps.as_ptr()), ncodes, ways: if ways { ncodes } else { 0 }, wcap: wn };
        let once = |w: &mut LightWords| {
            for (i, &c) in touched.iter().enumerate() {
                // a ways part's count is in the way of its row (any way: the walk adds the four)
                *w.cnt.ptr().add(c as usize + (i as u64 % wn * ncodes) as usize) = 1;
                *w.touched.ptr().add(i) = c;
            }
            w.n = if ways { 1 } else { touched.len() as u64 };
            assert_eq!(walk(ctx, sp, w, DESC, dense), 0);
        };
        once(&mut w);
        let mut best = f64::MAX;
        for _ in 0..5 {
            let mut reps = 1u64;
            loop {
                let t = Instant::now();
                for _ in 0..reps {
                    once(&mut w);
                }
                let el = t.elapsed();
                if el.as_millis() >= 20 {
                    best = best.min(el.as_nanos() as f64 / reps as f64);
                    break;
                }
                reps *= 2;
            }
        }
        best
    }

    #[test]
    #[ignore]
    fn the_walks_unit_costs() {
        let mut seed = DEV_SEED;
        let (mut walk_pts, mut list_ns) = (Vec::new(), Vec::new()); // alloc: Cold
        for ncodes in [4096u64, 32768, 131072, 262144] {
            let mut perm: Vec<u32> = (0..ncodes as u32).collect();
            let (mut d_pts, mut l_pts) = (Vec::new(), Vec::new()); // alloc: Cold
            for frac in [256u64, 128, 64, 32, 16, 8, 4, 2] {
                let n = (ncodes / frac) as usize;
                for i in 0..n {
                    let j = i + (next(&mut seed) % (ncodes - i as u64)) as usize;
                    perm.swap(i, j);
                }
                let touched = &perm[..n];
                let (d, l) = unsafe { (time(ncodes, touched, true, false), time(ncodes, touched, false, false)) };
                println!("fit\t{ncodes}\t{n}\tdense\t{d:.0}\tlist\t{l:.0}");
                d_pts.push((n as f64, d));
                l_pts.push((n as f64, l));
            }
            let ((ad, p), (al, q)) = (least_squares(&d_pts), least_squares(&l_pts));
            println!("fit\t{ncodes}\twalk-fixed\t{:.0}\tper-touched dense\t{p:.3}\tlist\t{q:.3}", ad - al);
            walk_pts.push((ncodes as f64, ad - al));
            list_ns.push(q - p);
        }
        let (_, walk) = least_squares(&walk_pts);
        let list = list_ns.iter().sum::<f64>() / list_ns.len() as f64;
        println!("fit\tLIGHT_WALK\t{walk:.4}\tLIGHT_LIST\t{list:.4}\tLIGHT_DENSE\t{}", break_even_div(list, walk));
        assert!(walk > 0.0 && list > 0.0, "both walks cost something");
    }

    /// D3159: the ways' walk per code of the part's dictionary (`LIGHT_WAY_WALK`): at each dictionary size within the ways'
    /// arrays (`LIGHT_WAYS_MAX_CODES`), the ways' walk and the list's over the same touched codes; least squares over the
    /// touched counts gives each walk's fixed part, and the slope of the ways' fixed part over the dictionary sizes is
    /// the per-code cost the ways pay that the list does not.
    #[test]
    #[ignore]
    fn the_ways_walk_unit_cost() {
        let mut seed = DEV_SEED;
        let mut fixed_pts = Vec::new(); // alloc: Cold
        for ncodes in [256u64, 512, 1024, 2048, 4096] {
            let mut perm: Vec<u32> = (0..ncodes as u32).collect();
            let (mut w_pts, mut l_pts) = (Vec::new(), Vec::new()); // alloc: Cold
            for frac in [64u64, 32, 16, 8, 4, 2, 1] {
                let n = (ncodes / frac) as usize;
                for i in 0..n {
                    let j = i + (next(&mut seed) % (ncodes - i as u64)) as usize;
                    perm.swap(i, j);
                }
                let touched = &perm[..n];
                let (wy, l) = unsafe { (time(ncodes, touched, true, true), time(ncodes, touched, false, false)) };
                println!("fitways\t{ncodes}\t{n}\tways\t{wy:.0}\tlist\t{l:.0}");
                w_pts.push((n as f64, wy));
                l_pts.push((n as f64, l));
            }
            let ((aw, pw), (al, pl)) = (least_squares(&w_pts), least_squares(&l_pts));
            println!("fitways\t{ncodes}\tfixed ways\t{aw:.0}\tlist\t{al:.0}\tper-touched ways\t{pw:.3}\tlist\t{pl:.3}");
            fixed_pts.push((ncodes as f64, aw - al));
        }
        let (_, way_walk) = least_squares(&fixed_pts);
        println!("fitways\tLIGHT_WAY_WALK\t{way_walk:.4}");
        assert!(way_walk > 0.0, "the ways' walk costs something a code");
    }
}
