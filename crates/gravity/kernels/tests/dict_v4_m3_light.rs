//! `front_light_flush` (kernels host_abi v4 m3_light.rs, V4R-191; the bypass's appender arm V4R-200, its record written
//! once V4R-210): the count front's convert counts each touched code in place into its T1 record (under the bypass, opened
//! through one appender and, without the validity word, stored once), and the table it leaves is the generic flush's (V4R-184: one record per code built and folded
//! through M1's `agg_front_flush`) over the same codes: every key's count and MinCode representative, in the table, under
//! the bypass and over a sized table that grows; with and without the validity word; a NULL key's record (the row path's:
//! no representative) and a row-path record of a counted code already in the table; the dense walk over a dictionary
//! whose present codes include one with count 0 (no record); the sparse walk with a stale list entry past `n`. The counts
//! are zeroed behind the convert and `n` is 0.

use std::collections::BTreeMap;

use gravity_abi::breaker::{light_keeps, HashAgg, KeyKind, Rec, RecordLayout, SlotKind, LIGHT_WAYS, MINCODE_NONE, T1};
use gravity_abi::ctx::ctx_size;
use gravity_abi::program::partial;
use gravity_abi::{Ctx, Partials, ProgramBlock};
use gravity_kernels::table::entries::{agg_front_flush, agg_open_grown};
use gravity_kernels::dict::strategies::front_light::{front_light_flush_keys, LightKey, LD_NONE};
use gravity_kernels::ops::sort::entries::{front_light_flush, LightWords};
use gravity_kernels::host_abi::{agg_finish, agg_miss, agg_open, AGG_GROUPS_BYPASS, AGG_STATE_BYTES};

const KEYS: [KeyKind; 3] = [KeyKind::I64, KeyKind::I64, KeyKind::I64];
const SLOTS: [SlotKind; 2] = [SlotKind::Count, SlotKind::MinCode];
const VALID: RecordLayout = RecordLayout::new(&KEYS, &SLOTS);
const NN: RecordLayout = RecordLayout::new_nn(&KEYS, &SLOTS);
/// `desc`: the count slot 0, the representative slot 1, three image words (the lane and site are the bind's).
const DESC: u64 = 1 << 8 | 3 << 16;
const PART: u32 = 7;
/// The row path's NULL key image (not a dictionary entry's).
const NULL_KEY: [u64; 3] = [0, 0, 1];

const SHARE: u32 = 1 << 22;
/// A share whose partition pool fills under the sparse walk's 3,000 codes (the flush path: `agg_locate`'s scatter).
const SMALL: u32 = 1 << 16;

/// A bound Ctx over one armed share region (v4_m1_front.rs's rig).
struct Bound {
    ctx: Vec<u64>,
    _program: Box<ProgramBlock>,
    _partials: Box<Partials>,
    _own: Vec<u64>,
}

impl Bound {
    fn new(share: u32) -> Bound {
        let mut own = vec![0u64; share as usize / 8];
        unsafe { HashAgg::arm(own.as_mut_ptr() as *mut u8, share) };
        let mut partials = Box::new(Partials::two(own.as_mut_ptr() as *mut u8, [core::ptr::null_mut(); 2]));
        let mut program = Box::new(ProgramBlock::NONE);
        program.partial = &mut *partials;
        program.spill = Some(scatter);
        let mut ctx = vec![0u64; ctx_size(0) / 8];
        unsafe { (*(ctx.as_mut_ptr() as *mut Ctx)).program = &mut *program };
        Bound { ctx, _program: program, _partials: partials, _own: own }
    }

    fn ctx(&mut self) -> *mut Ctx {
        self.ctx.as_mut_ptr() as *mut Ctx
    }
}

/// The records the stub scatter took out of each claim's pool (the driver's spill file) and its calls, by the claim's Ctx
/// (tests run on threads of their own; a map, not a thread-local: the session crate pins the tree's TLS sources).
static SPILLED: std::sync::Mutex<BTreeMap<usize, (Vec<Vec<u8>>, usize)>> = std::sync::Mutex::new(BTreeMap::new());

/// The driver's `SPILL_SCATTER` over the claim's T1 (driver spill.rs `scatter_t1`): the table drained into its buffers,
/// every buffered record kept aside, the pool reset, until the drain completes.
unsafe extern "C" fn scatter(ctx: *mut Ctx, op: u32, _arg: u64) -> i64 {
    if op == gravity_abi::program::SPILL_REGION_COMMIT {
        return 0; // agg_open's reservation ask (option c'): the rig's share is not reserved
    }
    if op == gravity_abi::program::SPILL_ARENA_CHUNK {
        return 0; // the compact scratch's owner ask (0 bytes, `OpArena::own_owner_kind`): no slot owner, uncharged
    }
    assert_eq!(op, gravity_abi::program::SPILL_SCATTER, "the convert's only spill is the pool's scatter");
    let t = partial(ctx) as *mut T1;
    let rec = (*t).layout.size() as usize;
    let mut all = SPILLED.lock().unwrap();
    let s = all.entry(ctx as usize).or_default();
    s.1 += 1;
    loop {
        let drained = T1::drain(t);
        for p in 0..(*t).parts() as u32 {
            for b in (*t).chain(p) {
                for i in 0..b.recs {
                    s.0.push(core::slice::from_raw_parts(b.ptr.add(i * rec), rec).to_vec());
                }
            }
        }
        (*t).reset_pool();
        if drained {
            return 0;
        }
    }
}

/// Code `c`'s Fp128 entry.
fn fp(c: u32) -> [u64; 2] {
    let x = (c as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    [x ^ 0x5555, x.rotate_left(29)]
}

/// Every record after the finish by key, the stub scatter's included (the bypass appends several per key): the counts summed, the representative the
/// least valid one (None: none).
unsafe fn table(ctx: *mut Ctx, layout: &RecordLayout) -> BTreeMap<Vec<u64>, (i64, Option<(u32, u32)>)> {
    let t = T1::view(partial(ctx));
    let mut out: BTreeMap<Vec<u64>, (i64, Option<(u32, u32)>)> = BTreeMap::new();
    let mut spilled = SPILLED.lock().unwrap().get_mut(&(ctx as usize)).map(|s| core::mem::take(&mut s.0)).unwrap_or_default();
    let mut recs: Vec<*mut u8> = spilled.iter_mut().map(|r| r.as_mut_ptr()).collect();
    for p in 0..1u32 << t.pbits {
        for b in t.chain(p) {
            recs.extend((0..b.recs).map(|i| b.ptr.add(i * layout.size() as usize) as *mut u8));
        }
    }
    for &at in &recs {
        let r = Rec::at(at, layout);
        let set = (!layout.has_validity() || r.valid(1)) && r.code(1) != ((MINCODE_NONE >> 32) as u32, MINCODE_NONE as u32);
        let e = out.entry(r.keys().to_vec()).or_insert((0, None));
        e.0 += r.count(0);
        if set {
            e.1 = Some(e.1.map_or(r.code(1), |m| m.min(r.code(1))));
        }
    }
    out
}

type Open = unsafe extern "C" fn(*mut Ctx, *mut u8, *const RecordLayout, u64, u64) -> i64;

/// The claim's table over `layout` opened by `open` (groups `groups`), the row path's records folded first (the NULL key's
/// with no representative, count 2; code 3's with count 5), then the part's `counts` (code, count) converted by the light
/// convert (`light`; `ways`: D3159's four count arrays, each code's count spread over them, no list) or the generic flush;
/// the table after the finish.
#[allow(clippy::too_many_arguments)]
unsafe fn run(layout: &RecordLayout, open: Open, groups: u64, share: u32, ncodes: u64, counts: &[(u32, u32)], stale: u32, light: bool, ways: bool) -> (BTreeMap<Vec<u64>, (i64, Option<(u32, u32)>)>, usize) {
    let mut b = Bound::new(share);
    let ctx = b.ctx();
    let mut st = [0u64; AGG_STATE_BYTES as usize / 8];
    let sp = st.as_mut_ptr() as *mut u8;
    assert_eq!(open(ctx, sp, layout, 1, groups), 0);
    let mut img = [0u8; 256];
    let r = Rec::at(img.as_mut_ptr(), layout);
    r.open(&NULL_KEY);
    r.add_count(0, 2);
    assert_eq!(agg_miss(ctx, sp, img.as_ptr()), 0);
    r.clear();
    r.open(&[fp(3)[0], fp(3)[1], 0]);
    r.add_count(0, 5);
    r.min_code(1, PART, 3);
    assert_eq!(agg_miss(ctx, sp, img.as_ptr()), 0);
    let fps: Vec<[u64; 2]> = (0..ncodes as u32).map(fp).collect();
    if light {
        let wn = if ways { LIGHT_WAYS } else { 1 };
        let (mut cnt, mut list) = (vec![0u32; (ncodes * wn) as usize], vec![0u32; counts.len() + 1]);
        for (i, &(c, n)) in counts.iter().enumerate() {
            // a ways part's count is the sum of its ways: way k holds n / 4 plus one of the first n % 4
            for k in 0..wn {
                cnt[(k * ncodes) as usize + c as usize] = n / wn as u32 + u32::from((k as u32) < n % wn as u32);
            }
            list[i] = c;
        }
        list[counts.len()] = stale;
        let n = if ways { 1 } else { counts.len() as u64 };
        let mut w = LightWords { cnt: cnt.as_mut_ptr(), touched: list.as_mut_ptr(), n, cap: if ways { 0 } else { ncodes }, alloc: ncodes, part: PART as u64, fps: fps.as_ptr(), ncodes, ways: if ways { ncodes } else { 0 }, wcap: wn };
        assert_eq!(front_light_flush(ctx, sp, &mut w as *mut LightWords as *mut u8, DESC), 0);
        assert_eq!(w.n, 0, "the list is empty behind the convert");
        assert!(cnt.iter().all(|&c| c == 0), "the counts are zeroed behind the convert");
    } else {
        let size = layout.size() as usize;
        let mut front = vec![0u64; counts.len().max(1) * size / 8];
        let base = front.as_mut_ptr() as *mut u8;
        for (i, &(c, n)) in counts.iter().enumerate() {
            let r = Rec::at(base.add(i * size), layout);
            r.open(&[fps[c as usize][0], fps[c as usize][1], 0]);
            r.add_count(0, n as i64);
            r.min_code(1, PART, c);
        }
        let idx: Vec<u64> = (0..counts.len() as u64).collect();
        assert_eq!(agg_front_flush(ctx, sp, base, idx.as_ptr(), idx.len() as u64), 0);
    }
    assert_eq!(agg_finish(ctx, sp), 0);
    let out = table(ctx, layout);
    (out, SPILLED.lock().unwrap().remove(&(ctx as usize)).map_or(0, |s| s.1))
}

/// The light convert's table equals the generic flush's in every table mode, both layouts.
unsafe fn same(ncodes: u64, counts: &[(u32, u32)], stale: u32, absent: &[u32]) {
    same_in(SHARE, ncodes, counts, stale, absent)
}

/// `same` over a share of `share` bytes.
unsafe fn same_in(share: u32, ncodes: u64, counts: &[(u32, u32)], stale: u32, absent: &[u32]) {
    let modes: [(&str, Open, u64); 3] = [("table", agg_open, 0), ("bypass", agg_open, AGG_GROUPS_BYPASS), ("sized", agg_open_grown, 100)];
    for (name, layout) in [("validity", &VALID), ("nn", &NN)] {
        for (mode, open, groups) in modes {
            let (got, want) = (run(layout, open, groups, share, ncodes, counts, stale, true, false).0, run(layout, open, groups, share, ncodes, counts, stale, false, false).0);
            assert_eq!(got, want, "{name} {mode}: the light convert's table is the generic flush's");
            // D3159: the same counts spread over the four ways, walked in code order
            let ways = run(layout, open, groups, share, ncodes, counts, stale, true, true).0;
            assert_eq!(ways, want, "{name} {mode}: the ways' convert's table is the generic flush's");
            assert_eq!(got[&NULL_KEY.to_vec()], (2, None), "{name} {mode}: the NULL key's record keeps no representative");
            let three = counts.iter().find(|c| c.0 == 3).map_or(0, |c| c.1 as i64);
            assert_eq!(got[&vec![fp(3)[0], fp(3)[1], 0]], (5 + three, Some((PART, 3))), "{name} {mode}: code 3 joined the row path's record");
            for &c in counts {
                assert_eq!(got[&vec![fp(c.0)[0], fp(c.0)[1], 0]].1, Some((PART, c.0)), "{name} {mode}: code {}'s representative", c.0);
            }
            for &c in absent {
                assert!(!got.contains_key(&vec![fp(c)[0], fp(c)[1], 0]), "{name} {mode}: code {c} (count 0) has no record");
            }
            assert_eq!(got.len(), counts.len() + 1 + usize::from(!counts.iter().any(|c| c.0 == 3)), "{name} {mode}: one record per key");
        }
    }
}

#[test]
fn the_dense_walk_counts_in_place_as_the_generic_flush_and_skips_a_present_code_with_count_0() {
    // 8 codes, 6 touched: the dense walk (n * LIGHT_DENSE >= ncodes) visits codes 2 and 5, present in the dictionary with
    // count 0
    unsafe { same(8, &[(0, 1), (1, 4), (3, 2), (4, 9), (6, 1), (7, 3)], 0, &[2, 5]) };
}

#[test]
fn the_sparse_walk_counts_in_place_as_the_generic_flush_past_evictions() {
    // 3,000 of 65,536 codes: the list walk; past the 1,024-slot table's pairs (evictions, the sized table's grow); the
    // list's spare entry past `n` names an untouched code
    let counts: Vec<(u32, u32)> = (0..3000u32).map(|i| (i * 21 % 65536, 1 + i % 7)).collect();
    unsafe { same(65536, &counts, 65535, &[65535, 1]) };
}

#[test]
fn a_full_pool_under_the_bypass_flushes_through_locate_and_the_appender_goes_on() {
    // the sparse walk's 3,000 codes into a 64 KiB share: the bypass's appender runs out of blocks, finishes, and M1's
    // locate scatters the pool (the stub driver keeps the records) before a fresh appender takes the rest
    let counts: Vec<(u32, u32)> = (0..3000u32).map(|i| (i * 21 % 65536, 1 + i % 7)).collect();
    unsafe {
        let (got, calls) = run(&NN, agg_open, AGG_GROUPS_BYPASS, SMALL, 65536, &counts, 65535, true, false);
        assert!(calls > 0, "the pool filled and scattered");
        let want = run(&NN, agg_open, AGG_GROUPS_BYPASS, SHARE, 65536, &counts, 65535, false, false).0;
        assert_eq!(got, want, "the table after scatters is the generic flush's over a pool that never fills");
        same_in(SMALL, 65536, &counts, 65535, &[65535, 1]);
    }
}

const KEYS2: [KeyKind; 5] = [KeyKind::I64; 5];
const SLOTS2: [SlotKind; 3] = [SlotKind::Count, SlotKind::MinCode, SlotKind::MinCode];
const VALID2: RecordLayout = RecordLayout::new(&KEYS2, &SLOTS2);
const NN2: RecordLayout = RecordLayout::new_nn(&KEYS2, &SLOTS2);
/// Two keys: the count slot 0, five image words, two keys (each key's representative is its entry's: slots 1 and 2).
const DESC2: u64 = LD_NONE << 8 | 5 << 16 | 2 << 24;

/// A front of two keys' words: the head, one entry a key and the convert's decline word.
#[repr(C)]
struct Words2 {
    w: LightWords,
    k: [LightKey; 2],
    declined: u64,
}

/// Key 1's Fp128 entry (key 0's is `fp`).
fn fp1(c: u32) -> [u64; 2] {
    let [a, b] = fp(c ^ 0x8000);
    [b, a]
}

/// The image of the codes `(c0, c1)`: each key's entry, a clear flags word.
fn image2(c0: u32, c1: u32) -> Vec<u64> {
    vec![fp(c0)[0], fp(c0)[1], fp1(c1)[0], fp1(c1)[1], 0]
}

/// Every record after the finish by key: the counts summed, each key's least representative.
unsafe fn table2(ctx: *mut Ctx, layout: &RecordLayout) -> BTreeMap<Vec<u64>, (i64, Option<(u32, u32)>, Option<(u32, u32)>)> {
    let t = T1::view(partial(ctx));
    let mut out = BTreeMap::new();
    let mut spilled = SPILLED.lock().unwrap().get_mut(&(ctx as usize)).map(|s| core::mem::take(&mut s.0)).unwrap_or_default();
    let mut recs: Vec<*mut u8> = spilled.iter_mut().map(|r| r.as_mut_ptr()).collect();
    for p in 0..1u32 << t.pbits {
        for b in t.chain(p) {
            recs.extend((0..b.recs).map(|i| b.ptr.add(i * layout.size() as usize) as *mut u8));
        }
    }
    for &at in &recs {
        let r = Rec::at(at, layout);
        let rep = |j: usize| ((!layout.has_validity() || r.valid(j)) && r.code(j) != ((MINCODE_NONE >> 32) as u32, MINCODE_NONE as u32)).then(|| r.code(j));
        let e = out.entry(r.keys().to_vec()).or_insert((0, None, None));
        e.0 += r.count(0);
        for (j, m) in [(1, &mut e.1), (2, &mut e.2)] {
            if let Some(c) = rep(j) {
                *m = Some(m.map_or(c, |x: (u32, u32)| x.min(c)));
            }
        }
    }
    out
}

/// Two keys of `sizes` codes, the part's `counts` (index, count) by the light convert of two keys (`light`) or the
/// generic flush of the same tuples' records; the table after the finish.
unsafe fn run2(layout: &RecordLayout, open: Open, groups: u64, sizes: [u32; 2], counts: &[(u32, u32)], light: bool) -> BTreeMap<Vec<u64>, (i64, Option<(u32, u32)>, Option<(u32, u32)>)> {
    let mut b = Bound::new(SHARE);
    let ctx = b.ctx();
    let mut st = [0u64; AGG_STATE_BYTES as usize / 8];
    let sp = st.as_mut_ptr() as *mut u8;
    assert_eq!(open(ctx, sp, layout, 1, groups), 0);
    let ncodes = (sizes[0] * sizes[1]) as u64;
    let codes = |i: u32| (i % sizes[0], i / sizes[0]);
    let (fps0, fps1): (Vec<[u64; 2]>, Vec<[u64; 2]>) = ((0..sizes[0]).map(fp).collect(), (0..sizes[1]).map(fp1).collect());
    if light {
        let (mut cnt, mut list) = (vec![0u32; ncodes as usize], vec![0u32; counts.len() + 1]);
        for (i, &(c, n)) in counts.iter().enumerate() {
            (cnt[c as usize], list[i]) = (n, c);
        }
        let key = |fps: &Vec<[u64; 2]>, size: u32, stride: u64, rep: u64| LightKey { fps: fps.as_ptr(), next: fps.as_ptr(), size: size as u64, stride, lane: 0, rep };
        let mut w = Words2 {
            w: LightWords { cnt: cnt.as_mut_ptr(), touched: list.as_mut_ptr(), n: counts.len() as u64, cap: ncodes, alloc: ncodes, part: PART as u64, fps: core::ptr::null(), ncodes, ways: 0, wcap: 1 },
            k: [key(&fps0, sizes[0], 1, 1), key(&fps1, sizes[1], sizes[0] as u64, 2)],
            declined: 0,
        };
        assert_eq!(front_light_flush_keys(ctx, sp, &mut w as *mut Words2 as *mut u8, DESC2), 0);
        assert_eq!(w.w.n, 0, "the list is empty behind the convert");
        let rows: u64 = counts.iter().map(|&(_, n)| n as u64).sum();
        assert_eq!(w.declined, !light_keeps(counts.len() as u64, rows) as u64, "the convert declines the statement's later parts after a part of too few rows a touched count");
        assert!(cnt.iter().all(|&c| c == 0), "the counts are zeroed behind the convert");
    } else {
        let size = layout.size() as usize;
        let mut front = vec![0u64; counts.len().max(1) * size / 8];
        let base = front.as_mut_ptr() as *mut u8;
        for (i, &(c, n)) in counts.iter().enumerate() {
            let (c0, c1) = codes(c);
            let r = Rec::at(base.add(i * size), layout);
            r.open(&image2(c0, c1));
            r.add_count(0, n as i64);
            r.min_code(1, PART, c0);
            r.min_code(2, PART, c1);
        }
        let idx: Vec<u64> = (0..counts.len() as u64).collect();
        assert_eq!(agg_front_flush(ctx, sp, base, idx.as_ptr(), idx.len() as u64), 0);
    }
    assert_eq!(agg_finish(ctx, sp), 0);
    let out = table2(ctx, layout);
    SPILLED.lock().unwrap().remove(&(ctx as usize));
    out
}

/// Two keys' light convert (dense and list walks) equals the generic flush in every table mode, both layouts: one record
/// a touched tuple, keyed by both keys' entries, each key's representative its own code.
#[test]
fn two_keys_count_by_their_tuple_index_as_the_generic_flush() {
    let sizes = [5u32, 7];
    let dense: Vec<(u32, u32)> = (0..35u32).filter(|i| i % 6 != 4).map(|i| (i, 1 + i % 4)).collect();
    let list: Vec<(u32, u32)> = [3u32, 11, 34, 20].iter().map(|&i| (i, 2 + i % 3)).collect();
    // under the break-even's 2.4 rows a touched count: the convert declines the statement's later parts
    let thin: Vec<(u32, u32)> = vec![(2, 1), (9, 1), (17, 2), (30, 1)];
    let modes: [(&str, Open, u64); 3] = [("table", agg_open, 0), ("bypass", agg_open, AGG_GROUPS_BYPASS), ("sized", agg_open_grown, 100)];
    for counts in [&dense, &list, &thin] {
        for (name, layout) in [("validity", &VALID2), ("nn", &NN2)] {
            for (mode, open, groups) in modes {
                let got = unsafe { run2(layout, open, groups, sizes, counts, true) };
                assert_eq!(got, unsafe { run2(layout, open, groups, sizes, counts, false) }, "{name} {mode}: the light convert's table is the generic flush's");
                assert_eq!(got.len(), counts.len(), "{name} {mode}: one record a tuple");
                for &(i, n) in counts.iter() {
                    let (c0, c1) = (i % sizes[0], i / sizes[0]);
                    assert_eq!(got[&image2(c0, c1)], (n as i64, Some((PART, c0)), Some((PART, c1))), "{name} {mode}: tuple {i}");
                }
            }
        }
    }
}
