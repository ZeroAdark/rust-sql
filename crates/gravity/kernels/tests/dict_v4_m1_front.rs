//! `agg_front_flush` (kernels host_abi v4 m1): each touched code's front record folds into the claim's T1 as `agg_miss`
//! folds an image, in the table and under the bypass, and is zeroed; an untouched code's record is left alone; a float
//! overflow stops the flush with the owner merge's status. `agg_front_open` hands out zeroed arena scratch, null when the
//! arena refuses. table.sized: `agg_open_grown` opens the T1 at the bound's slots, and `agg_locate` / `agg_front_flush`
//! double it in place on a full home pair past the fill instead of evicting (the state's mask following); a bypassed
//! table never grows.

use gravity_abi::breaker::{FloatOverflow, HashAgg, KeyKind, Rec, RecordLayout, SlotKind, T1};
use gravity_abi::hash::KeyHash;
use gravity_abi::ctx::ctx_size;
use gravity_abi::program::partial;
use gravity_abi::{Ctx, Partials, ProgramBlock};
use gravity_kernels::table::entries::{agg_front_flush, agg_front_open, agg_locate, agg_open_grown};
use gravity_kernels::host_abi::{agg_finish, agg_miss, agg_open, AggState, AGG_GROUPS_BYPASS, AGG_STATE_BYTES};

const LAYOUT: RecordLayout = RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count, SlotKind::SumI64]);
const F64: RecordLayout = RecordLayout::new(&[KeyKind::I64], &[SlotKind::SumF64]);

const SHARE: u32 = 1 << 20;

/// A bound Ctx over one armed share region (bypass_route.rs's rig: no `hand-kernels` feature).
struct Bound {
    ctx: Vec<u64>,
    _program: Box<ProgramBlock>,
    _partials: Box<Partials>,
    _own: Vec<u64>,
}

impl Bound {
    fn new() -> Bound {
        let mut own = vec![0u64; SHARE as usize / 8];
        unsafe { HashAgg::arm(own.as_mut_ptr() as *mut u8, SHARE) };
        let mut partials = Box::new(Partials::two(own.as_mut_ptr() as *mut u8, [core::ptr::null_mut(); 2]));
        let mut program = Box::new(ProgramBlock::NONE);
        program.partial = &mut *partials;
        let mut ctx = vec![0u64; ctx_size(0) / 8];
        unsafe { (*(ctx.as_mut_ptr() as *mut Ctx)).program = &mut *program };
        Bound { ctx, _program: program, _partials: partials, _own: own }
    }

    fn ctx(&mut self) -> *mut Ctx {
        self.ctx.as_mut_ptr() as *mut Ctx
    }
}

/// Every record of key `k` after the finish (the bypass appends one per located image): (count, sum) summed.
unsafe fn totals(ctx: *mut Ctx, k: u64) -> (i64, i64) {
    let t = T1::view(partial(ctx));
    let mut out = (0, 0);
    for b in t.chain(KeyHash::of(&[k]).part(t.pbits as u32)) {
        for i in 0..b.recs {
            let r = Rec::at(b.ptr.add(i * LAYOUT.size() as usize) as *mut u8, &LAYOUT);
            if r.keys() == [k] {
                out = (out.0 + r.count(0), out.1 + r.i64(1));
            }
        }
    }
    out
}

#[test]
fn touched_front_records_fold_into_the_table_and_are_zeroed_in_both_modes() {
    let size = LAYOUT.size() as usize;
    for bits in [1, 0] {
        let mut b = Bound::new();
        let ctx = b.ctx();
        let mut st = [0u64; AGG_STATE_BYTES as usize / 8];
        let mut img = [0u8; 256];
        let mut front = vec![0u64; 4 * size / 8];
        unsafe {
            assert_eq!(agg_open(ctx, st.as_mut_ptr() as *mut u8, &LAYOUT, bits, 0), 0);
            let r = Rec::at(img.as_mut_ptr(), &LAYOUT);
            r.open(&[7]);
            r.add_count(0, 1);
            r.add_i64(1, 100);
            assert_eq!(agg_miss(ctx, st.as_mut_ptr() as *mut u8, img.as_ptr()), 0);
            let fp = front.as_mut_ptr() as *mut u8;
            for (c, key, count, sum) in [(0usize, 7u64, 1i64, 5i64), (1, 9, 4, 4), (2, 42, 3, 10), (3, 42, 2, 1)] {
                let f = Rec::at(fp.add(c * size), &LAYOUT);
                f.open(&[key]);
                f.add_count(0, count);
                f.add_i64(1, sum);
            }
            let kept = Rec::at(fp.add(size), &LAYOUT).bytes().to_vec();
            let touched = [2u64, 0, 3];
            assert_eq!(agg_front_flush(ctx, st.as_mut_ptr() as *mut u8, fp, touched.as_ptr(), 3), 0);
            for c in touched {
                assert!(Rec::at(fp.add(c as usize * size), &LAYOUT).bytes().iter().all(|&b| b == 0), "code {c} zeroed");
            }
            assert_eq!(Rec::at(fp.add(size), &LAYOUT).bytes(), &kept[..], "the untouched code's record stays");
            assert_eq!(agg_finish(ctx, st.as_mut_ptr() as *mut u8), 0);
            assert_eq!(totals(ctx, 42), (5, 11), "bits {bits}: both codes of key 42");
            assert_eq!(totals(ctx, 7), (2, 105), "bits {bits}: the front record joined the table's");
            assert_eq!(totals(ctx, 9), (0, 0), "bits {bits}: the untouched code never flushed");
        }
    }
}

#[test]
fn a_float_overflow_stops_the_flush_with_the_owner_merge_status() {
    let size = F64.size() as usize;
    let mut b = Bound::new();
    let ctx = b.ctx();
    let mut st = [0u64; AGG_STATE_BYTES as usize / 8];
    let mut img = [0u8; 256];
    let mut front = vec![0u64; 2 * size / 8];
    unsafe {
        assert_eq!(agg_open(ctx, st.as_mut_ptr() as *mut u8, &F64, 1, 0), 0);
        let r = Rec::at(img.as_mut_ptr(), &F64);
        r.open(&[1]);
        r.add_f64(0, 1.5e308).unwrap();
        assert_eq!(agg_miss(ctx, st.as_mut_ptr() as *mut u8, img.as_ptr()), 0);
        let fp = front.as_mut_ptr() as *mut u8;
        for (c, key) in [(0usize, 1u64), (1, 2)] {
            let f = Rec::at(fp.add(c * size), &F64);
            f.open(&[key]);
            f.add_f64(0, 1.5e308).unwrap();
        }
        let second = Rec::at(fp.add(size), &F64).bytes().to_vec();
        assert_eq!(agg_front_flush(ctx, st.as_mut_ptr() as *mut u8, fp, [0u64, 1].as_ptr(), 2), FloatOverflow::STATUS);
        assert_eq!(Rec::at(fp.add(size), &F64).bytes(), &second[..], "the flush stopped at the overflow");
    }
}

unsafe extern "C" fn refuse(_ctx: *mut Ctx, _bytes: u64) -> i64 {
    0
}

#[test]
fn the_front_opens_zeroed_from_the_arena_and_is_null_when_the_arena_refuses() {
    let mut b = Bound::new();
    let ctx = b.ctx();
    let mut arena = vec![0xAAu8; 1024];
    unsafe {
        (*(*ctx).program).arena_grow = Some(refuse);
        (*ctx).arena_cur = arena.as_mut_ptr();
        (*ctx).arena_end = arena.as_mut_ptr().add(arena.len());
        let p = agg_front_open(ctx, 768);
        assert_eq!(p, arena.as_mut_ptr());
        assert!(core::slice::from_raw_parts(p, 768).iter().all(|&x| x == 0), "zeroed");
        assert!(agg_front_open(ctx, 512).is_null(), "the arena refused");
    }
    assert_eq!(arena[768], 0xAA, "past the front untouched");
}

/// Distinct keys per grow test: enough that a 1,024-slot open meets full home pairs (a plain table evicts).
const KEYS: u64 = 1500;

/// Keys 1..=`KEYS` through `agg_locate`, twice each (count 1, sum `k` per pass), over a table opened by `open`: the table's
/// (first cap, cap, evictions) before the finish, then every key's totals checked after it.
unsafe fn locate_all(open: unsafe extern "C" fn(*mut Ctx, *mut u8, *const RecordLayout, u64, u64) -> i64, groups: u64) -> (u32, u32, u64) {
    let mut b = Bound::new();
    let ctx = b.ctx();
    let mut st = [0u64; AGG_STATE_BYTES as usize / 8];
    let sp = st.as_mut_ptr() as *mut u8;
    assert_eq!(open(ctx, sp, &LAYOUT, 1, groups), 0);
    let first = T1::view(partial(ctx)).cap;
    for _ in 0..2 {
        for k in 1..=KEYS {
            let r = agg_locate(ctx, sp, [k].as_ptr());
            assert!(r > 0, "key {k}: status {r}");
            let rec = Rec::at(r as *mut u8, &LAYOUT);
            rec.add_count(0, 1);
            rec.add_i64(1, k as i64);
        }
    }
    let t = T1::view(partial(ctx));
    let (cap, evictions) = (t.cap, t.evictions);
    if t.flags & gravity_abi::breaker::T1_BYPASS == 0 {
        assert_eq!((*(sp as *mut AggState)).mask, cap as u64 - 1, "the state's mask follows the grown cap");
    }
    assert_eq!(agg_finish(ctx, sp), 0);
    for k in [1, 2, KEYS / 2, KEYS - 1, KEYS] {
        assert_eq!(totals(ctx, k), (2, 2 * k as i64), "key {k}");
    }
    (first, cap, evictions)
}

#[test]
fn a_sized_table_grows_in_place_where_a_plain_one_evicts() {
    unsafe {
        let (first, cap, ev) = locate_all(agg_open_grown, 100);
        let (pfirst, pcap, pev) = locate_all(agg_open, 100);
        assert_eq!((first, pfirst, pcap), (1024, 1024, 1024), "both open at the bound's 1,024 slots; the plain one stays there");
        assert!(cap >= 2 * first, "the sized table doubled on a full pair: {first} -> {cap}");
        assert!(pev > 0 && ev < pev, "growing instead of evicting: {ev} evictions sized, {pev} plain");
    }
}

#[test]
fn a_bypassed_sized_table_never_grows() {
    unsafe {
        let (first, cap, _) = locate_all(agg_open_grown, 100 | AGG_GROUPS_BYPASS);
        assert_eq!(cap, first, "the bypass appends; the table keeps its open size");
    }
}

#[test]
fn the_front_flush_grows_the_sized_table_before_it_folds() {
    let size = LAYOUT.size() as usize;
    let mut b = Bound::new();
    let ctx = b.ctx();
    let mut st = [0u64; AGG_STATE_BYTES as usize / 8];
    let sp = st.as_mut_ptr() as *mut u8;
    let mut front = vec![0u64; KEYS as usize * size / 8];
    unsafe {
        assert_eq!(agg_open_grown(ctx, sp, &LAYOUT, 1, 100), 0);
        let first = T1::view(partial(ctx)).cap;
        let fp = front.as_mut_ptr() as *mut u8;
        for c in 0..KEYS as usize {
            let f = Rec::at(fp.add(c * size), &LAYOUT);
            f.open(&[c as u64 + 1]);
            f.add_count(0, 3);
            f.add_i64(1, c as i64);
        }
        let touched: Vec<u64> = (0..KEYS).collect();
        assert_eq!(agg_front_flush(ctx, sp, fp, touched.as_ptr(), KEYS), 0);
        let t = T1::view(partial(ctx));
        assert!(t.cap >= 2 * first, "the flush grew the table: {first} -> {}", t.cap);
        assert_eq!((*(sp as *mut AggState)).mask, t.cap as u64 - 1);
        assert_eq!(agg_finish(ctx, sp), 0);
        for k in [1, KEYS / 2, KEYS] {
            assert_eq!(totals(ctx, k), (3, k as i64 - 1), "key {k}");
        }
    }
}
