//! `GranuleRangeSource` over an in-process pgrc table of several parts.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use gravity_abi::ctx::lane;

use gravity_abi::{Lane, ANSWER_MINMAX, ANSWER_SUM};
use gravity_driver::{blank_site, Source, Window};
use gravity_program::{ZoneOp, ZonePred, BOUND_TEXT_PREFIX};
use gravity_hosts::ops::scan::pgrc::granule_source::{granule_minima, granule_walk_keys, GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use gravity_parallel::{ClaimSize, GranuleRange, MorselSource};
use pgrc_format::abi::{ByteArena, DecodeOut, ValidityVerdict};
use pgrc_format::dirlayout::part_file_name;
use pgrc_format::class::{ColSchema, CollationClass, StorageClass, TypeSemantics};
use pgrc_qa::corpus::{build_fixture, int8_fixture, open_part_bytes, BuiltFixture, Fixture, OracleVal};
use pgrc_qa::{int8_col, text_col};
use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture, DictPolicy, ReferenceCandidates};
use pgrc_write::dict::TextSemantics;
use pgrc_write::wvfs::WriteVfs;
use std::collections::BTreeMap;
use pgrc_read::{PartExpect, PartRegistry, StreamCursor};
use pgrc_write::writer::PartCutPolicy;

#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;
use granule_fixtures::*;

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static TRACKED: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static TOKEN: Cell<u8> = const { Cell::new(0) };
}

fn token() -> usize {
    TOKEN.with(|t| t.as_ptr() as usize)
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if token() == TRACKED.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if token() == HOLD.load(Ordering::Relaxed) {
            let i = HELD_N.fetch_add(1, Ordering::Relaxed);
            if i < HELD_MAX {
                // SAFETY: the block is `l.size()` bytes, freed by its owner; it stays allocated until `held_back` checks it
                unsafe { p.write_bytes(POISON, l.size()) };
                HELD[i].store(p as usize, Ordering::Relaxed);
                HELD_LAYOUT[i].store(l.size() << 8 | l.align().trailing_zeros() as usize, Ordering::Relaxed);
                return;
            }
        }
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// D3250: the frees of the holding thread, kept allocated and poisoned so a write after the free shows.
static HOLD: AtomicUsize = AtomicUsize::new(0);
const HELD_MAX: usize = 1 << 14;
const POISON: u8 = 0xa5;
static HELD_N: AtomicUsize = AtomicUsize::new(0);
static HELD: [AtomicUsize; HELD_MAX] = [const { AtomicUsize::new(0) }; HELD_MAX];
static HELD_LAYOUT: [AtomicUsize; HELD_MAX] = [const { AtomicUsize::new(0) }; HELD_MAX];

/// The blocks `f` freed that something wrote after their free.
fn written_after_free(f: impl FnOnce()) -> usize {
    HELD_N.store(0, Ordering::Relaxed);
    HOLD.store(token(), Ordering::Relaxed);
    f();
    HOLD.store(0, Ordering::Relaxed);
    let n = HELD_N.load(Ordering::Relaxed);
    assert!(n <= HELD_MAX, "{n} frees: HELD_MAX holds the first {HELD_MAX} only");
    let mut written = 0;
    for i in 0..n {
        let (p, w) = (HELD[i].load(Ordering::Relaxed) as *mut u8, HELD_LAYOUT[i].load(Ordering::Relaxed));
        let l = Layout::from_size_align(w >> 8, 1 << (w & 0xff)).unwrap();
        // SAFETY: a block `dealloc` kept: `l.size()` bytes, still allocated
        written += unsafe { core::slice::from_raw_parts(p, l.size()) }.iter().any(|&b| b != POISON) as usize;
        unsafe { System.dealloc(p, l) };
    }
    written
}

fn tracked(f: impl FnOnce()) -> usize {
    TRACKED.store(token(), Ordering::Relaxed);
    ALLOCS.store(0, Ordering::Relaxed);
    f();
    TRACKED.store(0, Ordering::Relaxed);
    ALLOCS.load(Ordering::Relaxed)
}

const REL: u32 = 4242;

fn value(i: u64) -> Option<i64> {
    if i % 13 == 0 {
        None
    } else {
        Some((i as i64 * 7919) % 100_003 - 50_000)
    }
}

fn fixture() -> BuiltFixture {
    // 20 000-row cuts land on granule edges: parts of 24 576, 24 576 and 10 848 rows (3 + 3 + 2 granules).
    build_fixture(int8_fixture("gravity_granules", 7301, ROWS, vec![], PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() }, value))
}

fn serial_scan(bf: &BuiltFixture) -> Vec<Option<i64>> {
    let binding = pgrc_am::scan::codec_binding();
    let mut out = Vec::new();
    for (i, rec) in bf.manifest.parts.iter().enumerate() {
        let part = open_part_bytes(bf.files.get(&part_file_name(rec.part_no)).expect("part file"), 900 + i as u64).expect("open part");
        let mut cur = StreamCursor::open(part, binding, 1, 0).expect("cursor");
        for g in 0..cur.granule_count() {
            let rows = cur.rows_in_granule(g) as usize;
            let mut datums = vec![0u64; rows];
            let mut arena = vec![0u64; 8];
            let bytes = unsafe { core::slice::from_raw_parts_mut(arena.as_mut_ptr() as *mut u8, 64) };
            let n = cur.decode_full(g, &mut DecodeOut { datums: &mut datums, arena: ByteArena::new(bytes) }).expect("decode");
            assert_eq!(n as usize, rows);
            let mut valid = vec![u64::MAX; rows.div_ceil(64)];
            let all = !cur.has_validity_stream() || matches!(cur.validity(g, &mut valid).expect("validity"), ValidityVerdict::AllValid);
            for (r, d) in datums.iter().enumerate() {
                out.push(if all || (valid[r / 64] >> (r % 64)) & 1 == 1 { Some(*d as i64) } else { None });
            }
        }
    }
    out
}

fn drain(src: &mut GranuleRangeSource, lo: u64, hi: u64, out: &mut Vec<Option<i64>>) -> u64 {
    src.seek(&[lo, hi], &[0, 0]).unwrap();
    let (mut v, mut n) = ([0u64; 2], [0u8; 2]);
    let mut rows = 0;
    let mut last = (0u32, 0u16);
    while src.next(&mut v, &mut n).unwrap() {
        assert_eq!((v[0], n[0]), (v[1], n[1]), "the same lane twice reads the same");
        out.push(if n[0] == 0 { Some(v[0] as i64) } else { None });
        let p = src.position();
        assert!(p.is_valid() && p.rel == REL && (p.block as u64) >= lo && (p.block as u64) < hi, "position inside the claim: {p:?}");
        assert!((p.block, p.off) > last, "positions ascend within a claim");
        last = (p.block, p.off);
        rows += 1;
    }
    assert_eq!(src.pins(), 0, "a claim run to its end holds no part pin");
    rows
}

#[test]
fn granule_claims_match_the_serial_scan_and_never_cross_a_part() {
    let bf = fixture();
    let expected: Vec<Option<i64>> = (0..ROWS).map(value).collect();
    assert_eq!(serial_scan(&bf), expected, "the fixture reads back through pgrc");
    let list = part_list(&bf);
    assert!(list.len() >= 3 && list.granules() == 8, "{} parts, {} granules", list.len(), list.granules());
    let recipe = GranuleRecipe::for_test(REL, &[1, 1], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    let claims = GranuleRange::over_parts(list.granules(), ClaimSize::Fixed(2), list.starts());
    let mut all = Vec::new();
    let mut nclaims = 0;
    while let Some(c) = claims.claim() {
        assert!(c.hi <= claims.next_boundary_after(c.lo), "claim {c:?} crosses a part edge");
        assert_eq!(list.part_of(c.lo), list.part_of(c.hi - 1), "claim {c:?} spans two parts");
        drain(&mut src, c.lo, c.hi, &mut all);
        nclaims += 1;
    }
    assert_eq!(nclaims, 5, "3 + 3 + 2 granules in claims of 2 that stop at part edges");
    assert_eq!(all, expected, "every row once, in granule order");
    assert_eq!(src.granules_decoded(), 8);
    let mut tail = Vec::new();
    assert_eq!(drain(&mut src, list.granules() + 3, u64::MAX, &mut tail), 0, "a claim past the table is empty");
    src.seek(&[0, 0], &[1, 0]).unwrap();
    let (mut v, mut n) = ([0u64; 2], [0u8; 2]);
    assert!(!src.next(&mut v, &mut n).unwrap(), "a null claim key is an empty claim");
    src.unbind();
    assert_eq!(src.pins(), 0);
}

#[test]
fn a_claim_stopped_early_releases_its_pin_and_resumes_by_position() {
    let bf = fixture();
    let list = part_list(&bf);
    let recipe = GranuleRecipe::for_test(REL, &[1, 1], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    src.seek(&[3, 6], &[0, 0]).unwrap();
    let (mut v, mut n) = ([0u64; 2], [0u8; 2]);
    assert!(src.next(&mut v, &mut n).unwrap());
    assert_eq!(src.pins(), 1, "a staged granule pins its part");
    src.release();
    assert_eq!(src.pins(), 0, "a stop mid-claim drops the registry pin");
    assert!(src.next(&mut v, &mut n).unwrap(), "the staged rows stay readable after the pin is dropped");
    let mut rest = Vec::new();
    drain(&mut src, 3, 6, &mut rest);
    assert_eq!(rest.len() as u64, 24_576, "the second part re-read in full");
    src.unbind();
}

#[test]
fn steady_state_claims_allocate_nothing() {
    let bf = fixture();
    let list = part_list(&bf);
    let recipe = GranuleRecipe::for_test(REL, &[1, 1], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    let claims = GranuleRange::over_parts(list.granules(), ClaimSize::Fixed(1), list.starts());
    let (mut v, mut n) = ([0u64; 2], [0u8; 2]);
    let mut pass = |src: &mut GranuleRangeSource| {
        claims.reset();
        let mut rows = 0u64;
        while let Some(c) = claims.claim() {
            src.seek(&[c.lo, c.hi], &[0, 0]).unwrap();
            while src.next(&mut v, &mut n).unwrap() {
                rows += 1;
            }
            assert_eq!(src.pins(), 0);
        }
        rows
    };
    assert_eq!(pass(&mut src), ROWS, "warm-up: every part opened, every extent faulted");
    const PASSES: usize = 20;
    let allocs = tracked(|| {
        for _ in 0..PASSES {
            assert_eq!(pass(&mut src), ROWS);
        }
    });
    assert_eq!(allocs, 0, "{allocs} heap allocations in {PASSES} passes of per-granule claims across 3 parts");
    src.unbind();
}

fn drain_windows(src: &mut GranuleRangeSource, lo: u64, hi: u64, out: &mut Vec<Option<i64>>) -> (u64, u64) {
    src.seek(&[lo, hi], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let (mut windows, mut rows) = (0u64, 0u64);
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        assert_eq!(src.pins(), 1, "the part stays pinned while its window is current");
        assert_eq!((w.nlanes, w.rows, w.base().pos.rel, w.base().claim_id), (2, n, REL, lo as u32));
        assert!((w.base().pos.block as u64) >= lo && (w.base().pos.block as u64) < hi);
        assert!(w.sel.is_null(), "no delete mask: no selection");
        let l = &w.lanes()[0];
        assert_eq!((l.width, l.kind, l.region, l.id), (8, lane::KIND_BYVAL, lane::REGION_ARENA, lane::id(lane::ID_ATTR, 1)));
        assert_eq!(l.all_valid(), l.valid.is_null());
        for i in 0..n as usize {
            assert_eq!(unsafe { (l.datum(i), l.is_valid(i)) }, unsafe { (w.lanes()[1].datum(i), w.lanes()[1].is_valid(i)) }, "the same lane twice reads the same");
            out.push(unsafe { if l.is_valid(i) { Some(l.datum(i) as i64) } else { None } });
        }
        windows += 1;
        rows += n as u64;
    }
    assert_eq!(src.pins(), 0);
    (windows, rows)
}

#[test]
fn windows_match_the_row_protocol_and_a_rebind_repins_without_reopening() {
    let bf = fixture();
    let list = part_list(&bf);
    let recipe = GranuleRecipe::for_test(REL, &[1, 1], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    let (mut by_rows, mut by_windows) = (Vec::new(), Vec::new());
    let claims = GranuleRange::over_parts(list.granules(), ClaimSize::Fixed(2), list.starts());
    let mut windows = 0;
    while let Some(c) = claims.claim() {
        drain(&mut src, c.lo, c.hi, &mut by_rows);
        windows += drain_windows(&mut src, c.lo, c.hi, &mut by_windows).0;
    }
    assert_eq!(by_windows, by_rows, "the window protocol and the row protocol read the same rows");
    assert_eq!(by_rows, (0..ROWS).map(value).collect::<Vec<_>>());
    assert_eq!(windows, 8, "one window per granule");
    assert_eq!((src.parts_opened(), src.residency().parts), (3, 3));
    src.unbind();
    assert_eq!((src.pins(), src.residency().parts), (0, 3), "the parked Node keeps the cursors (and their parts) open");
    src.bind(&binding_ref(&resolved)).unwrap();
    let mut again = Vec::new();
    assert_eq!(drain_windows(&mut src, 0, list.granules(), &mut again).1, ROWS);
    assert_eq!(src.parts_opened(), 3, "a rebind under the same generation re-pins without opening");
    src.unbind();
    let other = Arc::new(PartList::new(list.parts().iter().enumerate().map(|(i, p)| PartSpec { io: match &p.io { PartIoSpec::Mem { bytes, dev, ino } => PartIoSpec::Mem { bytes: bytes.clone(), dev: *dev, ino: ino + 100 * (i as u64 + 1) }, PartIoSpec::Path(c) => PartIoSpec::Path(c.clone()) }, expect: PartExpect::none(), rows: p.rows, granules: p.granules, deletes: None, part: None }).collect()));
    assert_ne!(other.generation(), list.generation());
    let resolved2 = Resolved::for_recipe(src.recipe(), other.clone());
    src.bind(&binding_ref(&resolved2)).unwrap();
    let mut third = Vec::new();
    assert_eq!(drain_windows(&mut src, 0, other.granules(), &mut third).1, ROWS);
    assert_eq!(src.parts_opened(), 3, "a new generation drops the held cursors and the new statement opens its parts");
    assert_eq!(src.residency().parts, 3);
    src.unbind();
}

#[test]
fn steady_state_windows_allocate_nothing() {
    let bf = fixture();
    let list = part_list(&bf);
    let recipe = GranuleRecipe::for_test(REL, &[1, 1], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    let claims = GranuleRange::over_parts(list.granules(), ClaimSize::Fixed(1), list.starts());
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut pass = |src: &mut GranuleRangeSource| {
        claims.reset();
        let mut rows = 0u64;
        while let Some(c) = claims.claim() {
            src.seek(&[c.lo, c.hi], &[0, 0]).unwrap();
            loop {
                let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
                let n = src.window(&mut w).unwrap();
                if n == 0 {
                    break;
                }
                rows += n as u64;
            }
            assert_eq!(src.pins(), 0);
        }
        rows
    };
    assert_eq!(pass(&mut src), ROWS);
    const PASSES: usize = 20;
    let allocs = tracked(|| {
        for _ in 0..PASSES {
            assert_eq!(pass(&mut src), ROWS);
        }
    });
    assert_eq!(allocs, 0, "{allocs} heap allocations in {PASSES} passes of windows");
    src.unbind();
    let allocs = tracked(|| {
        src.bind(&binding_ref(&resolved)).unwrap();
        assert_eq!(pass(&mut src), ROWS);
        src.unbind();
    });
    assert_eq!(allocs, 0, "{allocs} heap allocations in a rebind under the held generation");
}

const REL_NARROW: u32 = 4243;

fn narrow_value(k: usize, i: u64) -> Option<i64> {
    match k {
        0 => (i % 17 != 5).then(|| (i as i64 * 7919) % 100_003 - 50_000),
        _ => Some((i as i64 % 60_000) - 30_000),
    }
}

/// An int4 + int2 table: two parts of 24 576 and 10 848 rows (3 + 2 granules).
fn narrow_fixture() -> BuiltFixture {
    let mut c4 = int8_col(1);
    c4.class = StorageClass::ByvalWord { width: 4, signed: true };
    c4.typlen = 4;
    c4.typalign = b'i';
    let mut c2 = int8_col(2);
    c2.class = StorageClass::ByvalWord { width: 2, signed: true };
    c2.typlen = 2;
    c2.typalign = b's';
    let rows = 35_424u64;
    let oracle = |k: usize| (0..rows).map(|i| narrow_value(k, i).map(|v| OracleVal::Word(v as u64))).collect();
    build_fixture(Fixture { name: "gravity_narrow", dir: "/qa/t7302".to_string(), spc: 1663, db: 5, relfilenumber: 7302, schema: vec![c4, c2], oracle: vec![oracle(0), oracle(1)], plans: vec![], policy: PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() } })
}

#[test]
fn int4_and_int2_lanes_are_published_narrow_and_read_as_the_datum_path_does() {
    let bf = narrow_fixture();
    let list = part_list(&bf);
    let rows = list.rows();
    let recipe = GranuleRecipe::for_test_widths(REL_NARROW, &[1, 2], &[4, 2], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut by_windows: [Vec<Option<i64>>; 2] = [Vec::new(), Vec::new()];
    let mut pass = |src: &mut GranuleRangeSource, mut out: Option<&mut [Vec<Option<i64>>; 2]>| {
        src.seek(&[0, list.granules()], &[0, 0]).unwrap();
        let mut windows = 0u64;
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
            let n = src.window(&mut w).unwrap();
            if n == 0 {
                break;
            }
            windows += 1;
            let (l4, l2) = (w.lanes()[0], w.lanes()[1]);
            assert_eq!((l4.width, l4.kind, l2.width, l2.kind), (4, lane::KIND_BYVAL, 2, lane::KIND_BYVAL));
            assert!(!l4.all_valid() || n < 17, "the int4 lane carries its validity words");
            assert!(l2.all_valid(), "the int2 lane has no nulls: a null `valid`");
            if let Some(out) = out.as_deref_mut() {
                let (p4, p2) = (l4.ptr as *const i32, l2.ptr as *const i16);
                for i in 0..n as usize {
                    // the typed element and the sign-extended datum agree
                    assert_eq!(unsafe { (*p4.add(i)) as i64 as u64 }, unsafe { l4.datum(i) });
                    assert_eq!(unsafe { (*p2.add(i)) as i64 as u64 }, unsafe { l2.datum(i) });
                    out[0].push(unsafe { l4.is_valid(i).then(|| *p4.add(i) as i64) });
                    out[1].push(Some(unsafe { *p2.add(i) as i64 }));
                }
            }
        }
        windows
    };
    assert_eq!(pass(&mut src, Some(&mut by_windows)), 5, "one window per granule");
    for k in 0..2 {
        assert_eq!(by_windows[k], (0..rows).map(|i| narrow_value(k, i)).collect::<Vec<_>>(), "lane {k} through the typed decode");
    }
    let mut by_rows: [Vec<Option<i64>>; 2] = [Vec::new(), Vec::new()];
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    let (mut v, mut n) = ([0u64; 2], [0u8; 2]);
    while src.next(&mut v, &mut n).unwrap() {
        for k in 0..2 {
            by_rows[k].push((n[k] == 0).then(|| v[k] as i64));
        }
    }
    assert_eq!(by_rows, by_windows, "the row protocol widens the narrow lanes to the same datums");
    const PASSES: usize = 10;
    let allocs = tracked(|| {
        for _ in 0..PASSES {
            assert_eq!(pass(&mut src, None), 5);
        }
    });
    assert_eq!(allocs, 0, "{allocs} heap allocations in {PASSES} passes of narrow windows");
    src.unbind();
}

/// `count(*) WHERE a < thr` through windows under the verdicts: (count, windows, all-match windows, decoded, skipped).
fn count_lt_by_windows(src: &mut GranuleRangeSource, total: u64, thr: i64) -> (u64, u64, u64, u64, u64) {
    src.seek(&[0, total], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let (mut count, mut windows, mut all) = (0u64, 0u64, 0u64);
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        windows += 1;
        if w.is_answered() {
            // every lane filter-only: an AllMatch granule arrives answered (rows, no lanes)
            assert_eq!(w.nlanes, 0, "an answered window publishes no lanes");
            all += 1;
            count += n as u64;
            continue;
        }
        let l = w.lanes()[0];
        match l.flags & lane::VERDICT_MASK {
            lane::VERDICT_ALL_MATCH => panic!("an AllMatch granule of a filter-only site is answered, not decoded"),
            lane::VERDICT_MIXED => {
                assert!(!l.ptr.is_null());
                count += (0..n as usize).filter(|&i| unsafe { l.is_valid(i) && (l.datum(i) as i64) < thr }).count() as u64;
            }
            v => panic!("a NoneMatch granule produced a window (verdict {v})"),
        }
    }
    assert_eq!(src.pins(), 0);
    (count, windows, all, src.granules_decoded(), src.granules_skipped())
}

#[test]
fn zone_verdicts_skip_none_match_granules_and_decode_only_the_mixed_ones() {
    let bf = sorted_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    let recipe = GranuleRecipe::for_test(REL_SORTED, &[1], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut plain = GranuleRangeSource::new(recipe.clone());
    plain.bind(&binding_ref(&resolved)).unwrap();
    let mut zoned = GranuleRangeSource::new(recipe);
    zoned.zone(&[ZonePred::word(0, ZoneOp::Lt, 20_000)], &[1], &[0]);
    zoned.bind(&binding_ref(&resolved)).unwrap();
    // the row protocol on the plain source is the reference
    let mut rows = Vec::new();
    plain.seek(&[0, total], &[0, 0]).unwrap();
    let (mut v, mut n) = ([0u64; 1], [0u8; 1]);
    while plain.next(&mut v, &mut n).unwrap() {
        rows.push((n[0] == 0).then_some(v[0] as i64));
    }
    let reference = rows.iter().filter(|v| v.is_some_and(|v| v < 20_000)).count() as u64;
    assert_eq!(reference, 20_000);
    assert_eq!(count_lt_by_windows(&mut zoned, total, 20_000), (20_000, 3, 2, 1, 5), "granules 0-1 AllMatch (answered, no decode), 2 Mixed (decoded), 3-7 NoneMatch (skipped)");
    assert_eq!(zoned.granules_answered(), 2);
    zoned.seek(&[0, total], &[0, 0]).unwrap();
    assert!(zoned.next(&mut [0u64], &mut [0u8]).is_err(), "the row protocol refuses an answered window");
    // every predicate value against the plain count
    for thr in [0i64, 1, 8_192, 8_193, 30_000, 59_999, 60_000, 1 << 40] {
        let want = rows.iter().filter(|v| v.is_some_and(|v| v < thr)).count() as u64;
        let mut z = GranuleRangeSource::new(plain.recipe().clone());
        z.zone(&[ZonePred::word(0, ZoneOp::Lt, thr)], &[1], &[0]);
        z.bind(&binding_ref(&resolved)).unwrap();
        let (count, windows, all, decoded, skipped) = count_lt_by_windows(&mut z, total, thr);
        assert_eq!(count, want, "thr {thr}");
        assert_eq!(windows, all + decoded, "thr {thr}: a window is AllMatch or decoded");
        assert_eq!(z.granules_answered() + decoded + skipped, 8, "thr {thr}: every granule is answered, decoded or skipped");
        assert!(decoded <= 1, "thr {thr}: at most one Mixed granule on a sorted column");
        // an AllMatch window is a whole part (from its part-grain record) or a single granule
        let whole = [24_576i64, 49_152, 60_000].iter().filter(|&&end| thr >= end).count() as u64;
        assert_eq!(z.parts_answered(), whole, "thr {thr}: every part below the threshold is answered whole");
        let singly = z.granules_answered() - [3u64, 3, 2][..whole as usize].iter().sum::<u64>();
        assert_eq!(all, whole + singly, "thr {thr}: {whole} part windows and {singly} granule windows");
        z.unbind();
    }
    // the other operators
    for (op, thr, want) in [(ZoneOp::Le, 8_191i64, 8_192u64), (ZoneOp::Gt, 49_151, 10_848), (ZoneOp::Ge, 49_152, 10_848), (ZoneOp::Eq, 12_345, 1), (ZoneOp::NotNull, 0, 60_000)] {
        let mut z = GranuleRangeSource::new(plain.recipe().clone());
        z.zone(&[ZonePred::word(0, op, thr)], &[1], &[0]);
        z.bind(&binding_ref(&resolved)).unwrap();
        z.seek(&[0, total], &[0, 0]).unwrap();
        let mut lanes = [Lane::NONE; MAX_LANES];
        let mut count = 0u64;
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
            let mut w = Window::over(&mut site);
            let n = src_window(&mut z, &mut w);
            if n == 0 {
                break;
            }
            let l = lanes[0];
            count += if w.is_answered() {
                n as u64
            } else {
                (0..n as usize).filter(|&i| unsafe { l.is_valid(i) } && { let v = unsafe { l.datum(i) } as i64; match op { ZoneOp::Le => v <= thr, ZoneOp::Gt => v > thr, ZoneOp::Ge => v >= thr, ZoneOp::Eq => v == thr, _ => true } }).count() as u64
            };
        }
        assert_eq!(count, want, "{op:?} {thr}");
        z.unbind();
    }
    // steady state: the verdicts allocate nothing
    const PASSES: usize = 10;
    let allocs = tracked(|| {
        for _ in 0..PASSES {
            assert_eq!(count_lt_by_windows(&mut zoned, total, 20_000).0, 20_000);
        }
    });
    assert_eq!(allocs, 0, "{allocs} heap allocations in {PASSES} passes under zone verdicts");
    zoned.unbind();
    plain.unbind();
}

fn src_window(src: &mut GranuleRangeSource, w: &mut Window<'_>) -> u32 {
    src.window(w).unwrap()
}

const REL_ANSWER: u32 = 4245;

fn av(i: u64) -> Option<i64> {
    (i % 19 != 7).then(|| (i as i64 * 7919) % 100_003 - 50_000)
}

/// `a = i` sorted, `v` with nulls: two int8 lanes over three parts.
fn answer_fixture() -> BuiltFixture {
    let rows = ROWS;
    build_fixture(Fixture {
        name: "gravity_answers",
        dir: "/qa/t7304".to_string(),
        spc: 1663,
        db: 5,
        relfilenumber: 7304,
        schema: vec![int8_col(1), int8_col(2)],
        oracle: vec![(0..rows).map(|i| Some(OracleVal::Word(i))).collect(), (0..rows).map(|i| av(i).map(|v| OracleVal::Word(v as u64))).collect()],
        plans: vec![],
        policy: PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() },
    })
}

#[test]
fn answered_windows_carry_the_granule_records_and_decode_nothing() {
    let bf = answer_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    let recipe = GranuleRecipe::for_test(REL_ANSWER, &[1, 2], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    // `WHERE a < 20000`, lane 0 filter-only, lane 1 (`v`) answered: sum(v), count(v), min(v), max(v)
    src.zone(&[ZonePred::word(0, ZoneOp::Lt, 20_000)], &[1], &[2]);
    src.bind(&binding_ref(&resolved)).unwrap();
    let run = |src: &mut GranuleRangeSource, lo: u64, hi: u64| {
        src.seek(&[lo, hi], &[0, 0]).unwrap();
        let mut lanes = [Lane::NONE; MAX_LANES];
        let (mut count, mut nonnull, mut sum, mut min, mut max, mut answered, mut decoded) = (0u64, 0u64, 0i128, i64::MAX, i64::MIN, 0u64, 0u64);
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
            let n = src.window(&mut w).unwrap();
            if n == 0 {
                break;
            }
            if w.is_answered() {
                assert!(w.nlanes == 0 && !w.answer.is_null(), "an answered window has records and no lanes");
                let a = unsafe { *w.answer.add(1) };
                assert_eq!(a.flags & (ANSWER_SUM | ANSWER_MINMAX), ANSWER_SUM | ANSWER_MINMAX);
                answered += 1;
                count += n as u64;
                nonnull += a.nonnull;
                sum += a.sum();
                min = min.min(a.min);
                max = max.max(a.max);
            } else {
                decoded += 1;
                let (la, lv) = (w.lanes()[0], w.lanes()[1]);
                assert!(!la.ptr.is_null() && !lv.ptr.is_null());
                for i in 0..n as usize {
                    if unsafe { la.datum(i) } as i64 >= 20_000 {
                        continue;
                    }
                    count += 1;
                    if unsafe { lv.is_valid(i) } {
                        let v = unsafe { lv.datum(i) } as i64;
                        nonnull += 1;
                        sum += v as i128;
                        min = min.min(v);
                        max = max.max(v);
                    }
                }
            }
        }
        (count, nonnull, sum, min, max, answered, decoded)
    };
    let vals: Vec<i64> = (0..20_000u64).filter_map(av).collect();
    let (nonnull, sum, min, max) = (vals.len() as u64, vals.iter().map(|&v| v as i128).sum::<i128>(), *vals.iter().min().unwrap(), *vals.iter().max().unwrap());
    // part 0 is 24,576 rows (three granules): Mixed at part grain under a < 20000, so it goes granule
    // by granule (0-1 answered, 2 decoded); parts 1 and 2 are NoneMatch at part grain and skipped whole
    let got = run(&mut src, 0, total);
    assert_eq!(got, (20_000u64, nonnull, sum, min, max, 2u64, 1u64), "two answered granules, one decoded, five skipped");
    assert_eq!((src.parts_answered(), src.granules_answered(), src.granules_decoded(), src.granules_skipped()), (0, 2, 1, 5));
    const PASSES: usize = 10;
    let allocs = tracked(|| {
        for _ in 0..PASSES {
            assert_eq!(run(&mut src, 0, total).0, 20_000);
        }
    });
    assert_eq!(allocs, 0, "{allocs} heap allocations in {PASSES} answered passes");
    src.unbind();
}

/// SRC-21: a tallied predicate (`v IS NOT NULL` over the answer lane `v`, a NULL in every granule) answers a window it
/// leaves Mixed when every other zone predicate matches the window whole, its passing rows the record's `nonnull`; a
/// window another predicate leaves Mixed decodes, and a whole part is answered by the same rule. Untallied, the NULLs keep
/// `v IS NOT NULL` Mixed and those windows decode. A record hands on no tally of a constant: an integer lane's answer
/// carries no false count (only a bool lane's record tallies its false rows). A granule under a delete vector is never answered (the records count its
/// deleted rows): it decodes, its live rows through the selection. Every run counts the rows PostgreSQL's filter passes.
#[test]
fn a_tallied_predicate_answers_its_mixed_windows_from_the_tally() {
    let bf = answer_fixture();
    let plain = part_list(&bf);
    // rows 3 and 10 of part 0's granule 0 deleted (both pass `a < 20000 AND v IS NOT NULL`)
    let deleted = Arc::new(PartList::new(bf.manifest.parts.iter().enumerate().map(|(i, rec)| {
        let bytes: Arc<[u8]> = Arc::from(bf.files.get(&part_file_name(rec.part_no)).expect("part file").as_slice());
        let d = (i == 0).then(|| pgrc_format::dml::encode_dv(rec.part_no, 1, &[(0, pgrc_format::dml::DvBlockKind::List, &[3u16, 10][..])]).unwrap());
        let deletes = d.map(|d| Arc::new(pgrc_scan::PartDeletes::from_dv_payload(&d, rec.part_no, 1, rec.granule_count).unwrap()));
        PartSpec { io: PartIoSpec::Mem { bytes, dev: 7, ino: 1960 + i as u64 }, expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes, part: None }
    }).collect()));
    let total = plain.granules();
    let run_on = |list: &Arc<PartList>, thr: i64, tallied: bool| {
        let recipe = GranuleRecipe::for_test(REL_ANSWER, &[1, 2], Arc::new(PartRegistry::new(64 << 20)));
        let resolved = Resolved::for_recipe(&recipe, Arc::clone(list));
        let mut src = GranuleRangeSource::new(recipe);
        let mut nn = ZonePred::word(1, ZoneOp::NotNull, 0);
        nn.tallied = tallied;
        // `WHERE a < thr AND v IS NOT NULL`: lane 0 filter-only, lane 1 (`v`) answered
        src.zone(&[ZonePred::word(0, ZoneOp::Lt, thr), nn], &[1], &[2]);
        src.bind(&binding_ref(&resolved)).unwrap();
        src.seek(&[0, total], &[0, 0]).unwrap();
        let mut lanes = [Lane::NONE; MAX_LANES];
        let mut count = 0u64;
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
            let mut w = Window::over(&mut site);
            let n = src.window(&mut w).unwrap();
            if n == 0 {
                break;
            }
            if w.is_answered() {
                let a = unsafe { *w.answer.add(1) };
                assert_eq!(a.false_count, 0, "an integer lane's answer carries no false count");
                count += a.nonnull;
                continue;
            }
            let (la, lv) = (w.lanes()[0], w.lanes()[1]);
            let rows: Vec<u32> = if w.sel.is_null() { (0..n).collect() } else { unsafe { core::slice::from_raw_parts(w.sel, w.sel_n as usize) }.to_vec() };
            // lane 0 left undecoded (null) is one whose own predicate matched the window whole (`lane_elide`)
            assert!(!lv.ptr.is_null(), "the answer lane is decoded in a window the tally does not answer");
            count += rows.iter().filter(|&&i| unsafe { (la.ptr.is_null() || (la.datum(i as usize) as i64) < thr) && lv.is_valid(i as usize) }).count() as u64;
        }
        let census = (src.parts_answered(), src.granules_answered(), src.granules_decoded(), src.granules_skipped());
        src.unbind();
        (count, census)
    };
    let run = |thr: i64, tallied: bool| run_on(&plain, thr, tallied);
    let want = |thr: u64| (0..thr).filter(|&i| av(i).is_some()).count() as u64;
    // part 0 Mixed at part grain on `a < 20000`: granules 0-1 answered from the tally, 2 decoded; parts 1-2 skipped whole
    assert_eq!(run(20_000, true), (want(20_000), (0, 2, 1, 5)));
    // untallied, `v IS NOT NULL` is Mixed in every granule (its NULLs): granules 0-2 decode
    assert_eq!(run(20_000, false), (want(20_000), (0, 0, 3, 5)));
    // every part AllMatch on `a < 60000`: each part answered whole from its part-grain tally
    assert_eq!(run(60_000, true), (want(ROWS), (3, 8, 0, 0)));
    assert_eq!(run(60_000, false).0, want(ROWS));
    // the delete vector's granule decodes (and its part goes granule by granule); the next granule is answered
    assert_eq!(run_on(&deleted, 20_000, true), (want(20_000) - 2, (0, 1, 2, 5)));
    assert_eq!(run_on(&deleted, 60_000, true), (want(ROWS) - 2, (2, 7, 1, 0)));
}

/// NEW-72: a bound static predicate (`ZonePred::bound`, its constant a literal's word) takes the execution's value from
/// the cell (`set_word_zone`'s `WORD_ZONE_AT` slot) and prunes and answers exactly as the same predicate with that
/// literal baked; a cell with no value for it (no protocol, a contested claim) leaves the site no zone verdict: every
/// window decodes and the filter runs per row, still the right count.
#[test]
fn a_bound_predicate_takes_the_executions_value_and_no_verdict_without_one() {
    let bf = answer_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    let run = |lt: ZonePred, value: Option<i64>| {
        let recipe = GranuleRecipe::for_test(REL_ANSWER, &[1, 2], Arc::new(PartRegistry::new(64 << 20)));
        if let Some(v) = value {
            let arg = gravity_program::protos::word_zone_at_arg(0, ZoneOp::Lt as u8, 8, 0);
            recipe.verdicts.set_word_zone([(arg, 0usize)].into_iter(), &[v as u64], &[0]);
        }
        let thr = value.unwrap_or(lt.value);
        let resolved = Resolved::for_recipe(&recipe, Arc::clone(&list));
        let mut src = GranuleRangeSource::new(recipe);
        // `WHERE a < thr AND v <> 0`: lane 0 filter-only, lane 1 (`v`) answered
        src.zone(&[lt, ZonePred::word(1, ZoneOp::Ne, 0)], &[1], &[2]);
        src.bind(&binding_ref(&resolved)).unwrap();
        src.seek(&[0, total], &[0, 0]).unwrap();
        let mut lanes = [Lane::NONE; MAX_LANES];
        let mut count = 0u64;
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
            let mut w = Window::over(&mut site);
            let n = src.window(&mut w).unwrap();
            if n == 0 {
                break;
            }
            if w.is_answered() {
                // an answered window passes `v <> 0` whole (AllMatch: no zero and no NULL in it)
                let a = unsafe { *w.answer.add(1) };
                count += a.nonnull;
                continue;
            }
            let (la, lv) = (w.lanes()[0], w.lanes()[1]);
            let rows: Vec<u32> = if w.sel.is_null() { (0..n).collect() } else { unsafe { core::slice::from_raw_parts(w.sel, w.sel_n as usize) }.to_vec() };
            count += rows.iter().filter(|&&i| unsafe { (la.ptr.is_null() || (la.datum(i as usize) as i64) < thr) && lv.is_valid(i as usize) && lv.datum(i as usize) != 0 }).count() as u64;
        }
        let census = (src.parts_answered(), src.granules_answered(), src.granules_decoded(), src.granules_skipped());
        src.unbind();
        (count, census)
    };
    let want = |thr: u64| (0..thr).filter(|&i| av(i).is_some_and(|v| v != 0)).count() as u64;
    let bound = ZonePred::bound(0, ZoneOp::Lt, 0, 8);
    for thr in [20_000i64, 60_000, 1] {
        assert_eq!(run(bound, Some(thr)), run(ZonePred::word(0, ZoneOp::Lt, thr), None), "a < {thr}");
        assert_eq!(run(bound, Some(thr)).0, want(thr.max(0) as u64), "a < {thr}");
    }
    // no value: nothing skipped, answered or covered; every granule decodes (the bound placeholder never prunes)
    let (count, census) = run(ZonePred { value: 20_000, ..bound }, None);
    assert_eq!((count, census), (want(20_000), (0, 0, total, 0)));
}

/// A claim that starts at a part's first granule and runs to its end decides the part from its
/// part-grain record: AllMatch answers the whole part as ONE window, NoneMatch skips it whole.
#[test]
fn whole_parts_answer_as_one_window_from_the_part_grain_record() {
    let bf = answer_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    assert_eq!((list.len(), total, list.starts()[1], list.starts()[2]), (3, 8, 3, 6), "three parts of 3 / 3 / 2 granules of 8192 rows");
    let expect = |rows: u64| {
        let vals: Vec<i64> = (0..rows).filter_map(av).collect();
        (rows, vals.len() as u64, vals.iter().map(|&v| v as i128).sum::<i128>(), *vals.iter().min().unwrap(), *vals.iter().max().unwrap())
    };
    let answered_run = |thr: i64, lo: u64, hi: u64| {
        let recipe = GranuleRecipe::for_test(REL_ANSWER, &[1, 2], Arc::new(PartRegistry::new(64 << 20)));
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        let mut src = GranuleRangeSource::new(recipe);
        src.zone(&[ZonePred::word(0, ZoneOp::Lt, thr)], &[1], &[2]);
        src.bind(&binding_ref(&resolved)).unwrap();
        src.seek(&[lo, hi], &[0, 0]).unwrap();
        let mut lanes = [Lane::NONE; MAX_LANES];
        let (mut count, mut nonnull, mut sum, mut min, mut max, mut windows, mut rows_per_window) = (0u64, 0u64, 0i128, i64::MAX, i64::MIN, 0u64, Vec::new());
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
            let n = src.window(&mut w).unwrap();
            if n == 0 {
                break;
            }
            assert!(w.is_answered(), "every window of an AllMatch claim is answered (thr {thr}, claim {lo}..{hi})");
            let a = unsafe { *w.answer.add(1) };
            windows += 1;
            rows_per_window.push(n);
            count += n as u64;
            nonnull += a.nonnull;
            sum += a.sum();
            min = min.min(a.min);
            max = max.max(a.max);
        }
        let census = (src.parts_answered(), src.granules_answered(), src.granules_decoded(), src.granules_skipped());
        src.unbind();
        ((count, nonnull, sum, min, max), windows, rows_per_window, census)
    };
    // a < 24576 = part 0 exactly: one window for the part, parts 1-2 skipped whole
    let (agg, windows, rows, census) = answered_run(24_576, 0, total);
    assert_eq!(agg, expect(24_576));
    assert_eq!((windows, rows, census), (1, vec![24_576], (1, 3, 0, 5)), "part 0 answered as one window of 24,576 rows; five granules skipped as two parts");
    // every part AllMatch: one window per part
    let (agg, windows, rows, census) = answered_run(60_000, 0, total);
    assert_eq!(agg, expect(60_000));
    assert_eq!((windows, rows, census), (3, vec![24_576, 24_576, 10_848], (3, 8, 0, 0)), "the footer-answered count produces one window per part");
    // a claim starting inside part 0 answers its granules 1-2 singly, then parts 1-2 whole
    let (agg, windows, rows, census) = answered_run(60_000, 1, total);
    assert_eq!(agg.0, 60_000 - 8192);
    assert_eq!((windows, rows, census), (4, vec![8192, 8192, 24_576, 10_848], (2, 7, 0, 0)));
    // a claim short of part 0's end stays at granule grain
    let (_, windows, rows, census) = answered_run(60_000, 0, 2);
    assert_eq!((windows, rows, census), (2, vec![8192, 8192], (0, 2, 0, 0)));
}

/// The serial answer: a list whose part-grain faces an earlier scan read (warm) answers or skips a whole part from its
/// records before any lookup or pin, so an answered window holds no pin; a cold list pins each part first. Both publish
/// the same windows and the same census.
#[test]
fn a_warm_lists_whole_part_is_answered_unpinned_and_counts_as_a_cold_one() {
    let bf = answer_fixture();
    let run = |list: &Arc<PartList>, thr: i64| {
        let recipe = GranuleRecipe::for_test(REL_ANSWER, &[1, 2], Arc::new(PartRegistry::new(64 << 20)));
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        let mut src = GranuleRangeSource::new(recipe);
        src.zone(&[ZonePred::word(0, ZoneOp::Lt, thr)], &[1], &[2]);
        src.bind(&binding_ref(&resolved)).unwrap();
        src.seek(&[0, list.granules()], &[0, 0]).unwrap();
        let mut lanes = [Lane::NONE; MAX_LANES];
        let (mut pins, mut rows) = (Vec::new(), Vec::new());
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
            let mut w = Window::over(&mut site);
            let n = src.window(&mut w).unwrap();
            if n == 0 {
                break;
            }
            assert!(w.is_answered(), "thr {thr}: every window of these claims is a whole part's answer");
            pins.push(src.pins());
            rows.push(n);
        }
        let census = (src.parts_answered(), src.granules_answered(), src.granules_decoded(), src.granules_skipped());
        src.unbind();
        (pins, rows, census)
    };
    // part 0 answered whole, parts 1-2 skipped whole; then every part answered
    for (thr, want_rows, want_census) in [(24_576i64, vec![24_576u32], (1u64, 3u64, 0u64, 5u64)), (60_000, vec![24_576, 24_576, 10_848], (3, 8, 0, 0))] {
        let list = part_list(&bf);
        let cold = run(&list, thr);
        let warm = run(&list, thr);
        assert_eq!(cold.0, vec![1; want_rows.len()], "thr {thr}: a cold list pins each part before its record answers it");
        assert_eq!(warm.0, vec![0; want_rows.len()], "thr {thr}: a warm list's answered part is never pinned");
        assert_eq!((&cold.1, cold.2), (&want_rows, want_census), "thr {thr}: the cold run");
        assert_eq!((&warm.1, warm.2), (&want_rows, want_census), "thr {thr}: the warm run counts as the cold one");
    }
}

/// A dictionary-encoded text column whose entries mix ASCII, two-, three- and four-byte characters, sealed
/// under the writer's `Utf8Chars` claim (the index's char field is the real count, the delta form).
fn text_fixture_utf8() -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    let rows = 35_424u64;
    let fx = Fixture {
        name: "gravity_text_utf8",
        dir: "/qa/t7306".to_string(),
        spc: 1663,
        db: 5,
        relfilenumber: 7306,
        schema: vec![text_col(1)],
        oracle: vec![(0..rows).map(|i| Some(OracleVal::Bytes(tv_utf8(i)))).collect()],
        plans: vec![],
        policy: PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() },
    };
    let posture = ColumnPosture { dict: Some(DictPolicy { ndv_cap: 100_000, exec_ok: true, sem: TextSemantics::Utf8Chars }), ..ColumnPosture::default() };
    let cands = CodecCandidates::new(ColumnPosture::default()).with_column(1, 0, posture);
    let sources: [&dyn CandidateSource; 2] = [&cands, &ReferenceCandidates];
    let mut vfs = pgrc_qa::simvfs::SimVfs::new();
    vfs.mkdir_path(&fx.dir).expect("mkdir");
    let fxid = 900 + fx.relfilenumber;
    let mut probe = Probe::new(TxnVerdict::Aborted);
    probe.mark(fxid, TxnVerdict::InProgress);
    let mut w = open_writer(&fx, fxid).expect("writer");
    append_rows_with_sources(&mut vfs, &mut w, &fx, 0, rows, &sources).expect("append");
    finish_with_sources(&mut vfs, &mut w, &sources).expect("finish");
    w.publish(&mut vfs, &probe).expect("publish");
    probe.mark(fxid, TxnVerdict::Committed);
    let files = vfs.snapshot_dir(&fx.dir);
    let manifest = effective_manifest(&mut vfs, &fx.dir, &probe, None).expect("effective").expect("committed generation");
    BuiltFixture { fx, files, manifest }
}

fn tv_utf8(i: u64) -> Vec<u8> {
    const WORDS: [&str; 6] = ["plain", "café", "naïve", "日本語", "🚀x", "Ωmega"];
    let w = if i < 24_576 { i % 23 } else { i % 23 + 10 };
    format!("{}{w:02}", WORDS[(w % 6) as usize]).into_bytes()
}

#[test]
fn a_text_column_is_published_as_its_code_lane_and_the_fold_crosses_parts_by_value() {
    let bf = text_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    assert_eq!(list.len(), 2);
    let recipe = GranuleRecipe::for_test_codes(REL_TEXT, &[1], &[1], Arc::new(PartRegistry::new(64 << 20)));
    assert!(recipe.has_codes());
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    // count by (part, code) through the code lane, as the kernel does
    let count = |src: &mut GranuleRangeSource| {
        src.seek(&[0, total], &[0, 0]).unwrap();
        let mut lanes = [Lane::NONE; MAX_LANES];
        let mut by_code: BTreeMap<(u32, u32), u64> = BTreeMap::new();
        let mut epochs = [0u32; 2];
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
            let n = src.window(&mut w).unwrap();
            if n == 0 {
                break;
            }
            let l = w.lanes()[0];
            assert_eq!((l.kind, l.width, l.region), (lane::KIND_CODES, 4, lane::REGION_DICTIONARY));
            assert_eq!(l.ncodes_or_scale, 23, "each part's dictionary holds its 23 words");
            assert!(w.base().dict_epoch != 0);
            epochs[w.base().part_id as usize] = w.base().dict_epoch;
            let codes = l.ptr as *const u32;
            for i in 0..n as usize {
                let c = unsafe { *codes.add(i) };
                assert!(c < 23);
                *by_code.entry((w.base().part_id, c)).or_default() += 1;
            }
        }
        assert_ne!(epochs[0], epochs[1], "two parts, two dictionary epochs");
        by_code
    };
    let by_code = count(&mut src);
    assert_eq!(by_code.len(), 46);
    // the second level: (part, code) -> value through the dictionary, folded by value
    let mut by_value: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
    for (&(p, c), &n) in &by_code {
        let (datum, len) = src.dict_value(p, 0, c).unwrap();
        let bytes = unsafe { core::slice::from_raw_parts((datum as *const u8).add(4), len as usize) }.to_vec();
        *by_value.entry(bytes).or_default() += n;
    }
    let mut want: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
    for i in 0..list.rows() {
        *want.entry(tv(i)).or_default() += 1;
    }
    assert_eq!(by_value, want, "the code counts folded by value are the value counts");
    assert_eq!(by_value.len(), 33);
    assert!(src.dict_value(0, 0, 23).is_err(), "a code past the dictionary refuses");
    assert!(src.dict_value(5, 0, 0).is_err(), "a part outside the list refuses");
    const PASSES: usize = 5;
    let allocs = tracked(|| {
        for _ in 0..PASSES {
            src.seek(&[0, total], &[0, 0]).unwrap();
            let mut lanes = [Lane::NONE; MAX_LANES];
            loop {
                let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
                if src.window(&mut w).unwrap() == 0 {
                    break;
                }
            }
        }
    });
    assert_eq!(allocs, 0, "{allocs} heap allocations in {PASSES} passes of code-lane windows");
    src.unbind();
}

/// The rows of claim `[lo, hi)` through the code lane's windows (each window's codes checked inside the dictionary).
fn code_rows(src: &mut GranuleRangeSource, lo: u64, hi: u64) -> u64 {
    src.seek(&[lo, hi], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut rows = 0;
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            return rows;
        }
        let l = w.lanes()[0];
        assert_eq!(l.kind, lane::KIND_CODES);
        let codes = l.ptr as *const u32;
        assert!((0..n as usize).all(|i| unsafe { *codes.add(i) } < l.ncodes_or_scale));
        rows += n as u64;
    }
}

/// D3963: a render past the memo polls its statement's cancel (an emit renders its winners back to back in one kernel
/// call); a memoised render does not, and with the byte clear every code renders as before.
#[test]
fn a_canceled_statements_render_past_the_memo_stops_and_a_memoised_one_does_not() {
    let bf = text_fixture();
    let list = part_list(&bf);
    let recipe = GranuleRecipe::for_test_codes(REL_TEXT, &[1], &[1], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    let byte = std::sync::atomic::AtomicU8::new(0);
    // the part's memo arms at its 64th render (text_hosts RENDER_MEMO_AFTER); code 3's later renders read it
    let memoised = cancelpoll::scoped(&byte, || (0..70).map(|_| src.dict_value(0, 0, 3).unwrap()).last().unwrap());
    byte.store(cancelpoll::ABORT_BIT, Ordering::Relaxed);
    let again = cancelpoll::scoped(&byte, || src.dict_value(0, 0, 3));
    assert_eq!(again.ok(), Some(memoised), "a memoised render answers under a cancel");
    let stopped = cancelpoll::scoped(&byte, || src.dict_value(0, 0, 4));
    assert!(matches!(stopped, Err(gravity_driver::Error::Cancelled)), "a render past the memo stops for the cancel: {stopped:?}");
    byte.store(0, Ordering::Relaxed);
    let all = cancelpoll::scoped(&byte, || (0..23u32).map(|c| src.dict_value(1, 0, c).map(|(_, n)| n)).collect::<Result<Vec<_>, _>>());
    assert_eq!(all.map(|v| v.len()).ok(), Some(23), "with the byte clear every code of the other part renders");
    src.unbind();
}

/// D2985: a statement that streams its rows (`Resolved::set_part_release`) past the part cache's budget (`set_part_share`
/// Some(0)) opens its dictionaries in its own cells and drops each part's once every claim left the part, one granule claim
/// at a time as the runner cuts them: the list's shared cells stay empty, so another statement over the list keeps its own.
/// A second pass opens them again (counted: a plain scan re-opens none); under the budget, and in a statement that does not
/// stream, they go to the list's cells, as before.
#[test]
fn a_streaming_statement_releases_each_parts_dictionaries_once_its_claims_left_it() {
    let bf = text_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    assert_eq!(list.len(), 2);
    let recipe = GranuleRecipe::for_test_codes(REL_TEXT, &[1], &[1], Arc::new(PartRegistry::new(64 << 20)));
    let mut resolved = Resolved::new();
    resolved.set_part_release(true);
    resolved.set_part_share(Some(0));
    resolved.install(REL_TEXT, list.clone());
    let mut src = GranuleRangeSource::new(recipe.clone());
    src.bind(&binding_ref(&resolved)).unwrap();
    let first_end = list.starts()[1];
    let mut rows = 0;
    for g in 0..total {
        rows += code_rows(&mut src, g, g + 1);
        let want = if g + 1 < first_end { 0 } else if g + 1 < total { 1 } else { 2 };
        assert_eq!(resolved.part_release_counts(), (want, 0), "after the claim of granule {g}");
    }
    assert_eq!(rows, list.rows());
    assert_eq!(list.shared_dicts(), 0, "the list's shared cells are untouched");
    // a claim over both parts releases each as it leaves it
    assert_eq!(code_rows(&mut src, 0, total), list.rows());
    assert_eq!(resolved.part_release_counts(), (4, 2), "a second pass re-opens each part's dictionary once");
    src.unbind();
    // under the budget: the list's cells, kept for the next statement (the parts still release the statement's use)
    let mut under = Resolved::new();
    under.set_part_release(true);
    under.set_part_share(Some(u64::MAX));
    under.install(REL_TEXT, list.clone());
    let mut src = GranuleRangeSource::new(recipe.clone());
    src.bind(&binding_ref(&under)).unwrap();
    assert_eq!(code_rows(&mut src, 0, total), list.rows());
    assert_eq!((under.part_release_counts(), list.shared_dicts()), ((2, 0), 2));
    src.unbind();
    // a statement that does not stream: the list's cells, kept
    let plain = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&plain)).unwrap();
    assert_eq!(code_rows(&mut src, 0, total), list.rows());
    assert_eq!((plain.part_release_counts(), list.shared_dicts()), ((0, 0), 2), "the cells the streaming statement left");
    src.unbind();
}

/// D3268: an aggregate's scan (`Resolved::set_part_uses_release`) releases each part's use once every claim left it, as the
/// streaming statement above does, but its dictionaries go to the list's cells (where its kept code words render at the
/// emit) even past the part cache's budget: both parts release, the list's cells hold both dictionaries, none is private.
#[test]
fn an_aggregates_scan_releases_its_parts_uses_and_keeps_its_dictionaries_in_the_lists_cells() {
    let bf = text_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    assert_eq!(list.len(), 2);
    let recipe = GranuleRecipe::for_test_codes(REL_TEXT, &[1], &[1], Arc::new(PartRegistry::new(64 << 20)));
    let mut resolved = Resolved::new();
    resolved.set_part_uses_release(true);
    resolved.set_part_release_at(Some(0)); // D3823 gate: release at once (the cache's level is the server's)
    resolved.set_part_share(Some(0));
    resolved.install(REL_TEXT, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    let first_end = list.starts()[1];
    let mut rows = 0;
    for g in 0..total {
        rows += code_rows(&mut src, g, g + 1);
        let want = if g + 1 < first_end { 0 } else if g + 1 < total { 1 } else { 2 };
        assert_eq!(resolved.part_release_counts(), (want, 0), "after the claim of granule {g}");
    }
    assert_eq!(rows, list.rows());
    assert_eq!(resolved.part_release_private(), 0, "no dictionary of its own past the budget");
    assert_eq!(list.shared_dicts(), 2, "the list's cells hold both parts' dictionaries");
    src.unbind();
}

/// D3823: an aggregate's scan whose breaker keeps code words, no dictionary image (`Resolved::set_part_own_dicts`,
/// `mempolicy::release::dicts_go_with_part`), past the part cache's level opens its dictionaries in its own cells and drops
/// each with its part, as the streaming statement does: the list's cells stay empty, nothing of the dictionaries outlives
/// the part's release. A code word kept past the release renders the same bytes through the dictionary opened again
/// (counted as a re-open); under the level they go to the list's cells, as the uses-only release does.
#[test]
fn an_aggregates_scan_gives_its_own_dictionaries_back_with_each_part() {
    let bf = text_fixture();
    let list = part_list_at(&bf, 38231);
    let total = list.granules();
    assert_eq!(list.len(), 2);
    let recipe = GranuleRecipe::for_test_codes(REL_TEXT, &[1], &[1], Arc::new(PartRegistry::new(64 << 20)));
    let mut resolved = Resolved::new();
    resolved.set_part_uses_release(true);
    resolved.set_part_own_dicts(true);
    resolved.set_part_release_at(Some(0)); // D3823 gate: release at once (the cache's level is the server's)
    resolved.set_part_share(Some(0));
    resolved.install(REL_TEXT, list.clone());
    let mut src = GranuleRangeSource::new(recipe.clone());
    src.bind(&binding_ref(&resolved)).unwrap();
    let first_end = list.starts()[1];
    let (before, n) = src.dict_value(0, 0, 0).unwrap();
    let want = unsafe { core::slice::from_raw_parts(before as *const u8, n as usize) }.to_vec();
    let mut rows = 0;
    for g in first_end.saturating_sub(1)..total {
        rows += code_rows(&mut src, g, g + 1);
    }
    rows += code_rows(&mut src, 0, first_end.saturating_sub(1));
    assert_eq!(rows, list.rows());
    assert_eq!(resolved.part_release_counts(), (2, 0), "both parts released");
    assert_eq!(resolved.part_release_private(), 2, "each part's dictionary was the statement's own");
    assert_eq!(list.shared_dicts(), 0, "the list's shared cells are untouched");
    let (after, m) = src.dict_value(0, 0, 0).unwrap();
    assert_eq!(unsafe { core::slice::from_raw_parts(after as *const u8, m as usize) }, &want[..], "a code word renders the same bytes after its part's release");
    assert_eq!(resolved.part_release_counts(), (2, 1), "the render opened part 0's dictionary again");
    src.unbind();
    // under the level: the list's cells, as a uses-only release keeps them
    let mut under = Resolved::new();
    under.set_part_uses_release(true);
    under.set_part_own_dicts(true);
    under.set_part_release_at(Some(0));
    under.set_part_share(Some(u64::MAX));
    under.install(REL_TEXT, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&under)).unwrap();
    assert_eq!(code_rows(&mut src, 0, total), list.rows());
    assert_eq!((under.part_release_counts(), under.part_release_private(), list.shared_dicts()), ((2, 0), 0, 2));
    src.unbind();
}

/// D4327: a source whose render memo armed on a part (64 renders, text_hosts RENDER_MEMO_AFTER) renders that part again
/// after another source's claim released it with its own dictionaries (`set_part_own_dicts` past the level: the W8 shape,
/// one worker's emit renders while another's claim completes the part) and the part cache evicted its images (no use of
/// the part left). The render opens the dictionary again; the part's dictionary epoch is unchanged (one per open part,
/// its registry entry stays), so a memo keyed by it alone handed out a pointer into the released, freed image. The freed
/// blocks stay allocated and poisoned (`written_after_free`'s hold), so such a pointer reads 0xa5 bytes.
#[test]
fn a_render_memo_never_hands_out_a_released_dictionarys_image() {
    let bf = text_fixture();
    let list = part_list_at(&bf, 4327);
    let total = list.granules();
    assert_eq!(list.len(), 2);
    let first_end = list.starts()[1];
    assert!(total - first_end >= 2, "part 1 has two granules to split");
    let reg = Arc::new(PartRegistry::new(64 << 20));
    let recipe = GranuleRecipe::for_test_codes(REL_TEXT, &[1], &[1], Arc::clone(&reg));
    let mut resolved = Resolved::new();
    resolved.set_part_uses_release(true);
    resolved.set_part_own_dicts(true);
    resolved.set_part_release_at(Some(0)); // D3823 gate: release at once (the cache's level is the server's)
    resolved.set_part_share(Some(0));
    resolved.install(REL_TEXT, list.clone());
    let mut a = GranuleRangeSource::new(recipe.clone());
    let mut b = GranuleRangeSource::new(recipe);
    a.bind(&binding_ref(&resolved)).unwrap();
    b.bind(&binding_ref(&resolved)).unwrap();
    // b's claim enters part 1: its dictionary opens in the statement's own cell, where a's renders read it (no cursor)
    let mut rows = code_rows(&mut b, 0, first_end + 1);
    assert_eq!(resolved.part_release_counts(), (1, 0), "part 0 released");
    // a render is the image's 4B-U varlena: its header and `n` bytes
    let bytes = |(d, n): (u64, u32)| unsafe { core::slice::from_raw_parts(d as *const u8, n as usize + 4) }.to_vec();
    let want: Vec<Vec<u8>> = (0..23u32).map(|c| bytes(a.dict_value(1, 0, c).unwrap())).collect();
    // the memo arms on part 1 and holds code 3's render
    for _ in 0..70 {
        assert_eq!(bytes(a.dict_value(1, 0, 3).unwrap()), want[3]);
    }
    let mut got = Vec::new();
    written_after_free(|| {
        // b's last claim completes part 1: its dictionary drops; the cache's trim frees the part's images
        rows += code_rows(&mut b, first_end + 1, total);
        assert_eq!(resolved.part_release_counts(), (2, 0), "both parts released");
        reg.trim_contents(0);
        got = (0..23u32).map(|c| bytes(a.dict_value(1, 0, c).unwrap())).collect();
    });
    assert_eq!(rows, list.rows());
    assert_eq!(resolved.part_release_counts(), (2, 1), "the render opened part 1's dictionary again");
    assert_eq!(got, want, "a render after the part's release reads the dictionary opened again, never the released image");
    a.unbind();
    b.unbind();
}

/// D3823: a breaker's scan in uses-only release (any breaker over one site: `drive::part_release`) gives each part back
/// whole. Two sources split part 0: the first's claim ends inside it (its cursors stay, a later claim of its own could read
/// on), the second's completes the part and releases the statement's use; the first source's cursors close at its unbind
/// (`close_released`), so no use of the released part outlives the statement in a parked Node. A render after the release
/// reads the dictionary from the list's cell and takes no use of the part again (`dict_value_pg`).
#[test]
fn a_breakers_scan_gives_each_released_part_back_whole() {
    let bf = text_fixture();
    let list = part_list_at(&bf, 3823);
    let total = list.granules();
    assert_eq!(list.len(), 2);
    let first_end = list.starts()[1];
    assert!(first_end >= 2, "part 0 has two granules to split");
    let recipe = GranuleRecipe::for_test_codes(REL_TEXT, &[1], &[1], Arc::new(PartRegistry::new(64 << 20)));
    let mut resolved = Resolved::new();
    resolved.set_part_uses_release(true);
    resolved.set_part_release_at(Some(0)); // D3823 gate: release at once (the cache's level is the server's)
    resolved.install(REL_TEXT, list.clone());
    let mut a = GranuleRangeSource::new(recipe.clone());
    let mut b = GranuleRangeSource::new(recipe);
    a.bind(&binding_ref(&resolved)).unwrap();
    b.bind(&binding_ref(&resolved)).unwrap();
    let rows = code_rows(&mut a, 0, first_end - 1) + code_rows(&mut b, first_end - 1, total);
    assert_eq!(rows, list.rows());
    assert_eq!(resolved.part_release_counts(), (2, 0), "both parts released");
    let rp = resolved.rel(REL_TEXT).expect("installed");
    assert!(rp.part_users(0) > 0, "the first source's cursors over part 0 still use it");
    assert_eq!(rp.part_users(1), 0, "the second source's claim ran to part 1's end: its cursors closed");
    let (datum, _) = b.dict_value(0, 0, 0).unwrap();
    assert!(datum != 0, "a render after the release reads the dictionary");
    let users = rp.part_users(0);
    b.dict_value(1, 0, 0).unwrap();
    assert_eq!((rp.part_users(0), rp.part_users(1)), (users, 0), "a render after the release takes no use of its part");
    a.unbind();
    assert_eq!(rp.part_users(0), 0, "the unbind closed the first source's cursors over the released part");
    b.unbind();
}

/// D3823 (R3b): a relation two sites read (a self-join's build and probe) releases its parts' uses only once BOTH sites'
/// claims left each part (`set_part_uses_rels`, readers 2): after the first site's whole pass nothing is released, after
/// the second's every part is; a relation not listed keeps its parts.
#[test]
fn a_relation_two_sites_read_releases_each_part_after_both_readers() {
    let bf = text_fixture();
    let list = part_list_at(&bf, 3824);
    let total = list.granules();
    assert_eq!(list.len(), 2);
    let recipe = GranuleRecipe::for_test_codes(REL_TEXT, &[1], &[1], Arc::new(PartRegistry::new(64 << 20)));
    let mut resolved = Resolved::new();
    resolved.set_part_uses_rels(Box::new([(REL_TEXT, 2)]));
    resolved.set_part_release_at(Some(0));
    resolved.install(REL_TEXT, list.clone());
    let mut build = GranuleRangeSource::new(recipe.clone());
    let mut probe = GranuleRangeSource::new(recipe.clone());
    build.bind(&binding_ref(&resolved)).unwrap();
    probe.bind(&binding_ref(&resolved)).unwrap();
    assert_eq!(code_rows(&mut build, 0, total), list.rows());
    assert_eq!(resolved.part_release_counts(), (0, 0), "one reader's pass releases nothing");
    assert_eq!(code_rows(&mut probe, 0, total), list.rows());
    assert_eq!(resolved.part_release_counts(), (2, 0), "the second reader's pass releases both parts");
    build.unbind();
    probe.unbind();
    let mut other = Resolved::new();
    other.set_part_uses_rels(Box::new([(REL_TEXT + 1, 1)]));
    other.set_part_release_at(Some(0));
    other.install(REL_TEXT, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&other)).unwrap();
    assert_eq!(code_rows(&mut src, 0, total), list.rows());
    assert_eq!(other.part_release_counts(), (0, 0), "a relation not listed keeps its parts");
    src.unbind();
}

/// D3823 (R3c): a FETCH-paced portal over one site releases uses only (`drive::pull_release`, `set_part_uses_release`): a
/// cursor abandoned after a few fetches, its claim reading part 0 whole and stopping inside part 1, gives part 0 back and
/// keeps only part 1 (the part its claim is in); a render of part 0's codes after the release reads its dictionary from
/// the list's cell without taking part 0 again.
#[test]
fn a_fetch_paced_portal_holds_only_the_part_its_claim_is_in() {
    let bf = text_fixture();
    let list = part_list_at(&bf, 3825);
    let total = list.granules();
    assert_eq!(list.len(), 2);
    let first_end = list.starts()[1];
    assert!(total - first_end >= 2, "part 1 has a granule past the abandoned claim");
    let recipe = GranuleRecipe::for_test_codes(REL_TEXT, &[1], &[1], Arc::new(PartRegistry::new(64 << 20)));
    let mut resolved = Resolved::new();
    resolved.set_part_uses_release(true);
    resolved.set_part_release_at(Some(0)); // D3823 gate: release at once (the cache's level is the server's)
    resolved.install(REL_TEXT, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    assert!(code_rows(&mut src, 0, first_end + 1) > 0);
    assert_eq!(resolved.part_release_counts(), (1, 0), "part 0's claims are done, part 1's are not");
    let rp = resolved.rel(REL_TEXT).expect("installed");
    assert_eq!(rp.part_users(0), 0, "the claim ran past part 0's end: its cursors closed");
    assert!(rp.part_users(1) > 0, "the abandoned claim inside part 1 keeps it");
    let (datum, _) = src.dict_value(0, 0, 0).unwrap();
    assert!(datum != 0, "a render after the release reads the dictionary");
    assert_eq!(rp.part_users(0), 0, "a render after the release takes no use of its part");
    src.unbind();
}

/// D3823 gate (mempolicy::release): under the level a uses-only release keeps the parts its claims are done with (no
/// release; their cursors stay open for the statement's next run, which a release while the cache has room only makes
/// open them again); once a later claim finds the cache past the level, the kept part goes back with the next one and
/// its cursors close.
#[test]
fn a_release_under_the_level_keeps_its_parts_until_the_cache_needs_the_room() {
    let bf = text_fixture();
    let list = part_list_at(&bf, 3826);
    let total = list.granules();
    assert_eq!(list.len(), 2);
    let first_end = list.starts()[1];
    assert!(total - first_end >= 2, "part 1 has a granule past the first claim");
    let recipe = GranuleRecipe::for_test_codes(REL_TEXT, &[1], &[1], Arc::new(PartRegistry::new(64 << 20)));
    let mut resolved = Resolved::new();
    resolved.set_part_uses_release(true);
    resolved.set_part_release_at(Some(u64::MAX));
    resolved.install(REL_TEXT, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    let rows = code_rows(&mut src, 0, first_end + 1);
    assert_eq!(resolved.part_release_counts(), (0, 0), "under the level nothing goes back");
    assert!(resolved.rel(REL_TEXT).expect("installed").part_users(0) > 0, "part 0's cursors stay open for the next run");
    resolved.set_part_release_at(Some(0));
    assert_eq!(rows + code_rows(&mut src, first_end + 1, total), list.rows());
    assert_eq!(resolved.part_release_counts(), (2, 0), "past the level the kept part goes back with the next");
    let rp = resolved.rel(REL_TEXT).expect("installed");
    assert_eq!((rp.part_users(0), rp.part_users(1)), (0, 0), "both parts' cursors closed");
    src.unbind();
}

const REL_FACTS: u32 = 4280;

/// NEW-45: a code lane's window says its part's dictionary entries each stand for a row the scan delivers
/// (`site::PART_ENTRIES_SAFE`, the code cut's seed reads it) exactly where the host's `entries_safe` holds: both parts of a
/// delete-free list outside a transaction snapshot; with a delete vector on the first part, never that part's windows and
/// always the other's. `Window::over` clears it for each window.
#[test]
fn a_code_lane_window_says_its_entries_stand_for_its_rows_only_without_a_delete_vector() {
    let bf = text_fixture();
    for dv in [false, true] {
        let list = Arc::new(PartList::new(bf.manifest.parts.iter().enumerate().map(|(i, rec)| {
            let bytes: Arc<[u8]> = Arc::from(bf.files.get(&part_file_name(rec.part_no)).expect("part file").as_slice());
            let d = (dv && i == 0).then(|| pgrc_format::dml::encode_dv(rec.part_no, 1, &[(0, pgrc_format::dml::DvBlockKind::List, &[3u16][..])]).unwrap());
            let deletes = d.map(|d| Arc::new(pgrc_scan::PartDeletes::from_dv_payload(&d, rec.part_no, 1, rec.granule_count).unwrap()));
            PartSpec { io: PartIoSpec::Mem { bytes, dev: 7, ino: 960 + 10 * dv as u64 + i as u64 }, expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes, part: None }
        }).collect()));
        let recipe = GranuleRecipe::for_test_codes(REL_FACTS + dv as u32, &[1], &[1], Arc::new(PartRegistry::new(64 << 20)));
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        let mut src = GranuleRangeSource::new(recipe);
        src.bind(&binding_ref(&resolved)).unwrap();
        src.seek(&[0, list.granules()], &[0, 0]).unwrap();
        let mut lanes = [Lane::NONE; MAX_LANES];
        let mut windows = [[0u32; 2]; 2];
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
            site.part_facts = 0xff;
            let mut w = Window::over(&mut site);
            if src.window(&mut w).unwrap() == 0 {
                assert_eq!(w.part_facts, 0, "dv {dv}: the empty window's facts");
                break;
            }
            assert_eq!(w.lanes()[0].kind, lane::KIND_CODES);
            let (part, facts) = (w.base().part_id as usize, w.part_facts);
            assert!(facts == 0 || facts == gravity_abi::site::site::PART_ENTRIES_SAFE, "dv {dv} part {part}: facts {facts:#x}");
            windows[part][(facts != 0) as usize] += 1;
        }
        let want = if dv { [[windows[0][0], 0], [0, windows[1][1]]] } else { [[0, windows[0][1]], [0, windows[1][1]]] };
        assert_eq!(windows, want, "dv {dv}: windows by part without / with the fact");
        assert!(windows[0][dv as usize ^ 1] > 0 && windows[1][1] > 0, "dv {dv}: each part's windows read: {windows:?}");
        src.unbind();
    }
}

/// The same two-part text column with NO dictionary policy: the writer stores it verbatim and the
/// reader builds a dictionary per part when it pins it (`text_hosts::LocalDict`).
fn verbatim_text_fixture(with_empty: bool) -> BuiltFixture {
    verbatim_fixture(with_empty, false)
}

/// `verbatim_text_fixture` with every value distinct (`distinct`): ~20 k entries per part, so a
/// part's table lanes overflow the source's first table chunk.
fn verbatim_fixture(with_empty: bool, distinct: bool) -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    let rows = 35_424u64;
    let relfilenumber = if distinct { 7308 } else if with_empty { 7307 } else { 7306 };
    let value = |i: u64| if with_empty && i % 7 == 0 { Vec::new() } else if distinct { format!("u{i:06}").into_bytes() } else { tv(i) };
    let fx = Fixture {
        name: "gravity_text_verbatim",
        dir: format!("/qa/t{relfilenumber}"),
        spc: 1663,
        db: 5,
        relfilenumber,
        schema: vec![text_col(1)],
        oracle: vec![(0..rows).map(|i| Some(OracleVal::Bytes(value(i)))).collect()],
        plans: vec![],
        policy: PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() },
    };
    let cands = CodecCandidates::new(ColumnPosture::default());
    let sources: [&dyn CandidateSource; 2] = [&cands, &ReferenceCandidates];
    let mut vfs = pgrc_qa::simvfs::SimVfs::new();
    vfs.mkdir_path(&fx.dir).expect("mkdir");
    let fxid = 900 + fx.relfilenumber;
    let mut probe = Probe::new(TxnVerdict::Aborted);
    probe.mark(fxid, TxnVerdict::InProgress);
    let mut w = open_writer(&fx, fxid).expect("writer");
    append_rows_with_sources(&mut vfs, &mut w, &fx, 0, rows, &sources).expect("append");
    finish_with_sources(&mut vfs, &mut w, &sources).expect("finish");
    w.publish(&mut vfs, &probe).expect("publish");
    probe.mark(fxid, TxnVerdict::Committed);
    let files = vfs.snapshot_dir(&fx.dir);
    let manifest = effective_manifest(&mut vfs, &fx.dir, &probe, None).expect("effective").expect("committed generation");
    BuiltFixture { fx, files, manifest }
}

/// The value behind `dict_value(part, 0, code)` as bytes.
fn rendered(src: &mut GranuleRangeSource, part: u32, code: u32) -> Vec<u8> {
    let (datum, len) = src.dict_value(part, 0, code).unwrap();
    unsafe { core::slice::from_raw_parts((datum as *const u8).add(4), len as usize) }.to_vec()
}

#[test]
fn a_verbatim_text_column_publishes_byte_ranked_codes_cached_with_its_fingerprint_table() {
    use gravity_hosts::dict::text_hosts::{TableFn, TableSpec};
    for with_empty in [false, true] {
        let bf = verbatim_text_fixture(with_empty);
        let list = part_list(&bf);
        let total = list.granules();
        assert_eq!(list.len(), 2);
        let value = |i: u64| if with_empty && i % 7 == 0 { Vec::new() } else { tv(i) };
        let recipe = GranuleRecipe::for_test_codes(if with_empty { REL_TEXT + 2 } else { REL_TEXT + 1 }, &[1], &[1], Arc::new(PartRegistry::new(64 << 20))).with_tables(vec![TableSpec { over: 0, func: TableFn::Fingerprint }]);
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        let mut src = GranuleRangeSource::new(recipe);
        src.bind(&binding_ref(&resolved)).unwrap();
        src.seek(&[0, total], &[0, 0]).unwrap();
        let mut lanes = [Lane::NONE; MAX_LANES];
        let mut by_code: BTreeMap<(u32, u32), u64> = BTreeMap::new();
        let mut ncodes = [0u32; 2];
        let mut rows_seen = 0u64;
        let mut row = 0u64;
        // the Front's pattern: the fingerprint lane filled at a part's FIRST window, read for every granule at the part's end
        let mut part_fps: Option<(u32, gravity_dict::Fp128)> = None;
        let mut seen: Vec<(u32, Vec<u8>)> = Vec::new();
        let check_part = |fps: &Option<(u32, gravity_dict::Fp128)>, seen: &Vec<(u32, Vec<u8>)>| {
            let (_, t) = fps.as_ref().expect("a part's table");
            for (c, v) in seen {
                assert_eq!(Some(unsafe { t.get(*c) }), gravity_dict::fp::fp128(v), "the fingerprint of code {c}");
            }
        };
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
            let mut w = Window::over(&mut site);
            let n = src.window(&mut w).unwrap();
            if n == 0 {
                break;
            }
            let l = w.lanes()[0];
            assert_eq!((l.kind, l.width, l.region), (lane::KIND_CODES, 4, lane::REGION_DICTIONARY));
            assert!(w.base().dict_epoch != 0);
            let p = w.base().part_id;
            ncodes[p as usize] = l.ncodes_or_scale;
            if part_fps.as_ref().is_none_or(|(pp, _)| *pp != p) {
                if part_fps.is_some() {
                    check_part(&part_fps, &seen);
                }
                let mut t = Lane::NONE;
                assert_eq!(src.lane(2, &[], &mut t).unwrap(), 1);
                part_fps = Some((p, unsafe { gravity_dict::Fp128::from_lane(&t) }.expect("fp128")));
                seen.clear();
            }
            let codes = l.ptr as *const u32;
            for i in 0..n as usize {
                let c = unsafe { *codes.add(i) };
                let v = value(row + i as u64);
                assert!(c < l.ncodes_or_scale, "code {c} of {}", l.ncodes_or_scale);
                assert_eq!(c == 0 && with_empty, v.is_empty(), "'' is code 0 when the part has one");
                seen.push((c, v));
                *by_code.entry((p, c)).or_default() += 1;
            }
            row += n as u64;
            rows_seen += n as u64;
        }
        check_part(&part_fps, &seen);
        assert_eq!(rows_seen, list.rows());
        assert_eq!(ncodes, if with_empty { [24, 24] } else { [23, 23] }, "the distinct values per part (+ the empty string)");
        // codes are byte-ranked within a part (a kernel's per-part minimum by code is the minimum value): the rendered values ascend with the code
        for p in 0..2u32 {
            let vals: Vec<Vec<u8>> = (0..ncodes[p as usize]).map(|c| rendered(&mut src, p, c)).collect();
            assert!(vals.windows(2).all(|w| w[0] < w[1]), "part {p}: byte-rank order");
        }
        let mut by_value: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
        for (&(p, c), &n) in &by_code {
            *by_value.entry(rendered(&mut src, p, c)).or_default() += n;
        }
        let mut want: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
        for i in 0..list.rows() {
            *want.entry(value(i)).or_default() += 1;
        }
        assert_eq!(by_value, want, "the code counts folded by the rendered value are the value counts");
        assert!(src.dict_value(0, 0, ncodes[0]).is_err(), "a code past the dictionary refuses");
        assert_eq!(src.dict_value(gravity_abi::ctx::lane::EMPTY_PART, 0, 0).unwrap().1, 0);
        // the ranks are the part's, cached with the fingerprint table: a second source of the statement renders the same code
        let mut src2 = GranuleRangeSource::new(src.recipe().clone());
        src2.bind(&binding_ref(&resolved)).unwrap();
        assert_eq!(rendered(&mut src2, 1, 3), rendered(&mut src, 1, 3), "rendered through a source that never staged the part");
        const PASSES: usize = 3;
        let allocs = tracked(|| {
            for _ in 0..PASSES {
                src.seek(&[0, total], &[0, 0]).unwrap();
                let mut lanes = [Lane::NONE; MAX_LANES];
                loop {
                    let mut site = blank_site(lanes.as_mut_ptr());
                    let mut w = Window::over(&mut site);
                    if src.window(&mut w).unwrap() == 0 {
                        break;
                    }
                    let mut t = Lane::NONE;
                    assert_eq!(src.lane(2, &[], &mut t).unwrap(), 1);
                }
            }
        });
        assert_eq!(allocs, 0, "{allocs} heap allocations in {PASSES} passes over ranked codes with the fingerprint lane filled per window");
        src.unbind();
    }
}

/// The same values sealed both ways — the dictionary-coded fixture and the verbatim one hold `tv(i)`
/// row for row — fingerprint identically through the `Fp128` lane: the cross-part fold's premise.
#[test]
fn a_value_fingerprints_the_same_whether_its_part_stored_it_coded_or_verbatim() {
    use gravity_hosts::dict::text_hosts::{TableFn, TableSpec};
    let mut maps: Vec<BTreeMap<Vec<u8>, [u64; 2]>> = Vec::new();
    for (bf, rel) in [(text_fixture(), REL_TEXT + 8), (verbatim_text_fixture(false), REL_TEXT + 9)] {
        let list = part_list(&bf);
        let total = list.granules();
        let recipe = GranuleRecipe::for_test_codes(rel, &[1], &[1], Arc::new(PartRegistry::new(64 << 20))).with_tables(vec![TableSpec { over: 0, func: TableFn::Fingerprint }]);
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        let mut src = GranuleRangeSource::new(recipe);
        src.bind(&binding_ref(&resolved)).unwrap();
        src.seek(&[0, total], &[0, 0]).unwrap();
        let mut lanes = [Lane::NONE; MAX_LANES];
        let mut map: BTreeMap<Vec<u8>, [u64; 2]> = BTreeMap::new();
        let mut row = 0u64;
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
            let mut w = Window::over(&mut site);
            let n = src.window(&mut w).unwrap();
            if n == 0 {
                break;
            }
            let l = w.lanes()[0];
            let mut t = Lane::NONE;
            assert_eq!(src.lane(2, &[], &mut t).unwrap(), 1);
            let fps = unsafe { gravity_dict::Fp128::from_lane(&t) }.expect("fp128");
            for i in 0..n as usize {
                let c = unsafe { *(l.ptr as *const u32).add(i) };
                let fp = unsafe { fps.get(c) };
                assert_eq!(map.insert(tv(row + i as u64), fp).unwrap_or(fp), fp, "one fingerprint per value");
            }
            row += n as u64;
        }
        assert_eq!(map.len(), 33, "the two parts' 23 + 23 words share 13");
        maps.push(map);
    }
    assert_eq!(maps[0], maps[1], "coded and verbatim parts fingerprint a value alike");
    for (v, fp) in &maps[0] {
        assert_eq!(gravity_dict::fp::fp128(v), Some(*fp));
    }
}

/// The resident walk order (`ZoneOrder::of`): granules by ascending minimum then granule, the
/// minima in that order, every granule's part — what `claims_of` sorted per statement before.
#[test]
fn a_zone_order_sorts_the_minima_once_and_maps_every_granule_to_its_part() {
    let bf = sorted_fixture();
    let list = part_list(&bf);
    let mins = [40i64, i64::MIN, 7, 7, 40, 9, i64::MIN, 3];
    let o = gravity_hosts::ops::scan::pgrc::granule_source::ZoneOrder::of(&mins, &list).unwrap();
    assert_eq!(&*o.order, &[1, 6, 7, 2, 3, 5, 0, 4]);
    assert_eq!(o.mins, vec![i64::MIN, i64::MIN, 3, 7, 7, 9, 40, 40]);
    assert_eq!(o.part_of.len(), list.granules() as usize);
    for p in 0..list.len() {
        assert!(o.part_of[list.starts()[p] as usize..list.end_of(p) as usize].iter().all(|&x| x as usize == p));
    }
}

/// PM13: a descending walk's keys are the NOT of each granule's exact maximum, so the ascending walk order visits the
/// largest maximum first and `beyond_bound` (key > bound) skips a granule whose maximum lies under a DESC run's bound
/// (the run's word is the NOT of the ascending one too). The ascending keys are the minima, unchanged.
#[test]
fn a_descending_walk_orders_by_the_not_of_each_maximum() {
    let bf = sorted_fixture();
    let list = part_list(&bf);
    assert_eq!(granule_walk_keys(&list, 1, false, false).unwrap(), granule_minima(&list, 1).unwrap());
    assert_eq!(granule_walk_keys(&list, 1, true, true).unwrap(), granule_walk_keys(&list, 1, true, false).unwrap(), "no NULL: NULLS FIRST keys alike");
    let keys = granule_walk_keys(&list, 1, true, false).unwrap();
    assert_eq!(keys, (0..8).map(|g| !((g * 8192 + 8191).min(59_999))).collect::<Vec<i64>>(), "the NOT of each granule's maximum (60 000 rows)");
    let o = gravity_hosts::ops::scan::pgrc::granule_source::ZoneOrder::of(&keys, &list).unwrap();
    assert_eq!(&*o.order, &[7, 6, 5, 4, 3, 2, 1, 0], "the largest maximum walks first");
    // a DESC bound at value v (row n - 1's key) reads back as !v: granules whose maximum is under v lie past it
    let v = 6 * 8192 + 100;
    assert_eq!(o.mins.iter().filter(|&&k| k > !v).count(), 6, "granules 0-5 end under v");
}

/// PL-06: a NULLS FIRST walk over a nullable key keys every granule holding a NULL `i64::MIN` (its stats record's
/// non-null count under its rows), either direction, so it walks first and a NULL row n - 1's bound (`i64::MIN`, exec
/// merge.rs `run_bound`) skips every other granule; a NULLS LAST walk keys those granules by their values, as before.
#[test]
fn a_nulls_first_walk_keys_the_granules_holding_a_null_first() {
    // `a = i` but NULL on ten rows of granule 5 and one row of granule 7 (not their first rows)
    let nulls = |i: u64| matches!(i, 41_000..=41_009 | 58_000);
    let bf = build_fixture(int8_fixture("gravity_nulls_walk", 7341, ROWS, vec![], PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() }, |i| (!nulls(i)).then_some(i as i64)));
    let list = part_list(&bf);
    let held = |g: i64| g == 5 || g == 7;
    let asc: Vec<i64> = (0..8).map(|g| if held(g) { i64::MIN } else { g * 8192 }).collect();
    let desc: Vec<i64> = (0..8).map(|g| if held(g) { i64::MIN } else { !(g * 8192 + 8191).min(59_999) }).collect();
    assert_eq!(granule_walk_keys(&list, 1, false, true).unwrap(), asc, "ASC NULLS FIRST");
    assert_eq!(granule_walk_keys(&list, 1, true, true).unwrap(), desc, "DESC NULLS FIRST");
    assert_eq!(granule_walk_keys(&list, 1, false, false).unwrap(), (0..8).map(|g| g * 8192).collect::<Vec<i64>>(), "NULLS LAST: the minima");
    let o = gravity_hosts::ops::scan::pgrc::granule_source::ZoneOrder::of(&desc, &list).unwrap();
    assert_eq!(&o.order[..3], &[5, 7, 6], "the granules holding a NULL first, then the largest maximum");
    assert_eq!(o.mins.iter().filter(|&&k| k > i64::MIN).count(), 6, "a NULL row n - 1 bounds at i64::MIN: six granules lie past it");
}

/// RD-width.walk over a nullable key (a NULLS LAST row walk): each granule counts its non-null values from its stats
/// record, so the census by rows never counts a granule's NULLs as rows of the top.
#[test]
fn a_nullable_keys_granules_count_their_non_null_values() {
    use gravity_hosts::ops::scan::pgrc::granule_source::granule_nonnull;
    // the PL-06 fixture's data: `a = i` but NULL on ten rows of granule 5 and one row of granule 7
    let nulls = |i: u64| matches!(i, 41_000..=41_009 | 58_000);
    let bf = build_fixture(int8_fixture("gravity_nonnull_counts", 7342, ROWS, vec![], PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() }, |i| (!nulls(i)).then_some(i as i64)));
    let list = part_list(&bf);
    let want: Vec<u64> = (0..8u64).map(|g| (ROWS - g * 8192).min(8192) - match g { 5 => 10, 7 => 1, _ => 0 }).collect();
    assert_eq!(granule_nonnull(&list, 1).unwrap(), want, "every granule's rows less its NULLs");
}

/// RD-width.walk: a row walk reaches the granules in walk order until their rows reach n, then every granule whose walk
/// key is at most their farthest edge; a keyless granule walks first and adds no row; under n rows, every granule.
#[test]
fn a_row_walk_reaches_the_granules_under_its_first_n_rows_farthest_edge() {
    use gravity_hosts::ops::scan::pgrc::granule_source::walk_reach;
    let ranges = [(0i64, 99i64), (100, 199), (50, 250), (300, 399), (i64::MAX, i64::MIN), (200, 299)];
    let walk = |desc: bool| {
        let keys: Vec<i64> = ranges.iter().map(|&(lo, hi)| if lo > hi { i64::MIN } else if desc { !hi } else { lo }).collect();
        let mut order: Vec<u32> = (0..ranges.len() as u32).collect();
        order.sort_unstable_by_key(|&g| (keys[g as usize], g));
        let mins: Vec<i64> = order.iter().map(|&g| keys[g as usize]).collect();
        (order, mins)
    };
    let (order, mins) = walk(false);
    let reach = |n| walk_reach(&order, &mins, &ranges, |_| 100, false, n);
    assert_eq!((reach(50), reach(150), reach(1000)), (3, 5, 6), "ASC: g0 alone (edge 99), then g0 + g2 (edge 250), all");
    let (order, mins) = walk(true);
    let reach = |n| walk_reach(&order, &mins, &ranges, |_| 100, true, n);
    assert_eq!((reach(50), reach(150)), (2, 4), "DESC: g3 alone (its minimum 300 bounds), then g3 + g5 (edge 200 reaches g2's maximum 250)");
}

#[test]
fn a_walk_order_maps_claim_units_to_granules_by_key_minimum() {
    let bf = sorted_fixture();
    let list = part_list(&bf);
    let mins = granule_minima(&list, 1).unwrap();
    assert_eq!(mins, (0..8).map(|g| g * 8192).collect::<Vec<i64>>(), "each granule's exact minimum of the sorted key");
    // walk the granules in reverse (a descending walk would): unit i is granule 7 - i
    let order: Arc<[u32]> = Arc::from((0..8u32).rev().collect::<Vec<_>>());
    let recipe = GranuleRecipe::for_test(REL_SORTED, &[1], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe_ordered(&recipe, list.clone(), Some(order.clone()));
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    src.seek(&[0, 3], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut firsts = Vec::new();
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        firsts.push((w.base().pos.block, unsafe { w.lanes()[0].datum(0) } as i64, w.base().claim_id));
    }
    assert_eq!(firsts, vec![(7, 7 * 8192, 0), (6, 6 * 8192, 0), (5, 5 * 8192, 0)], "units 0..3 are granules 7, 6, 5; positions name the real granule");
    assert_eq!(src.pins(), 0);
    src.seek(&[6, 100], &[0, 0]).unwrap();
    let mut rows = 0;
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        rows += n as u64;
    }
    assert_eq!(rows, 2 * 8192, "a claim past the order's end is clamped to it: units 6, 7 = granules 1, 0");
    src.unbind();
}

/// D2897: the walk order belongs to a site, not to its relation. An InitPlan's zone walk (site 0) and its parent's plain
/// scan of the same relation (site 1) install the one list twice; the second install erased the first's order, so the
/// walk read granules 0, 1, .. as if they held the lowest keys and its LIMIT stopped on a later value.
#[test]
fn two_sites_over_one_relation_keep_their_own_walk_orders() {
    let bf = sorted_fixture();
    let list = part_list(&bf);
    let order: Arc<[u32]> = Arc::from((0..8u32).rev().collect::<Vec<_>>());
    let recipe = GranuleRecipe::for_test(REL_SORTED, &[1], Arc::new(PartRegistry::new(64 << 20)));
    let mut resolved = Resolved::new();
    resolved.install(REL_SORTED, list.clone());
    resolved.set_order(0, Some(order));
    resolved.install(REL_SORTED, list.clone());
    resolved.set_order(1, None);
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut blocks = |site: usize| {
        let mut src = GranuleRangeSource::new(recipe.clone()).at_site(site);
        src.bind(&binding_ref(&resolved)).unwrap();
        src.seek(&[0, 3], &[0, 0]).unwrap();
        let mut seen = Vec::new();
        loop {
            let mut s = blank_site(lanes.as_mut_ptr());
            let mut w = Window::over(&mut s);
            if src.window(&mut w).unwrap() == 0 {
                break;
            }
            seen.push(w.base().pos.block);
        }
        src.unbind();
        seen
    };
    assert_eq!(blocks(0), vec![7, 6, 5], "the walk's units 0..3 are granules 7, 6, 5 after the parent's install");
    assert_eq!(blocks(1), vec![0, 1, 2], "the parent's units are its granules");
}

/// The table lanes (expr.md §2.2): over the writer's dictionary (`text_fixture`) and the reader's
/// (`verbatim_text_fixture`), lane 2 a LIKE bits table, lane 3 `octet_length`, lane 4 the fingerprint —
/// every window's fill agrees with the rendered entries; a part's tables build once per part.
/// The backend state a regexp table's build reads outside a server: its compile check (D354) is one `regexp_replace`
/// call through the fmgr, the open-coded program's too (the interrupt seam, the regex seams, the database encoding and
/// the forced RE2 engine, as the derived-fingerprint dictionary test does).
fn regexp_fmgr_setup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        postgres_seams::check_for_interrupts::set(|| Ok(()));
        regex_core::init_seams();
    });
    let _ = mbutils::SetDatabaseEncoding(wchar::PG_UTF8);
    regexp_alt::set_regex_engine(guc_tables::consts::REGEX_ENGINE_RE2);
}

#[test]
fn table_lanes_publish_like_length_and_fingerprint_tables_per_part() {
    use gravity_hosts::dict::text_hosts::{TableFn, TableSpec};
    regexp_fmgr_setup();
    for (name, bf, rel) in [("dict", text_fixture(), REL_TEXT + 3), ("verbatim", verbatim_text_fixture(true), REL_TEXT + 4), ("verbatim-distinct", verbatim_fixture(true, true), REL_TEXT + 5)] {
        let list = part_list(&bf);
        let total = list.granules();
        // B2-20's code sets: present in one part, in both, in none, the empty value, the verbatim parts' values
        let (eq, ne): ([&[u8]; 6], [&[u8]; 3]) = ([b"w05", b"w30", b"w12", b"absent", b"", b"u000007"], [b"w00", b"w22", b"u035000"]);
        let tables = vec![
            TableSpec { over: 0, func: TableFn::Like { pattern: b"%1%".to_vec(), ilike: false, negated: false } },
            TableSpec { over: 0, func: TableFn::OctetLength },
            TableSpec { over: 0, func: TableFn::Fingerprint },
            TableSpec { over: 0, func: TableFn::CharLength },
            TableSpec { over: 0, func: TableFn::RegexpReplace { pattern: b"^([a-z]+)[0-9]*$".to_vec(), replacement: b"\\1".to_vec(), collation: 950, encoding: 6, global: false } },
            TableSpec { over: 0, func: TableFn::Like { pattern: b"%0_".to_vec(), ilike: false, negated: true } },
            TableSpec { over: 0, func: TableFn::DerivedFingerprint { pattern: b"^([a-z]+)[0-9]*$".to_vec(), replacement: b"\\1".to_vec(), collation: 950, encoding: 6, global: false } },
            TableSpec { over: 0, func: TableFn::Eq { values: eq.iter().map(|v| v.to_vec()).collect(), negated: false } },
            TableSpec { over: 0, func: TableFn::Eq { values: ne.iter().map(|v| v.to_vec()).collect(), negated: true } },
            // P5-06 / SRC-17: byte intervals (both ends, open above, past every entry) and LIKE's prefix, exact and any
            // patterns as code runs (by binary search over the dictionary parts, swept over the verbatim ones)
            TableSpec { over: 0, func: TableFn::Range { from: b"w1".to_vec(), to: Some(b"w3".to_vec()) } },
            TableSpec { over: 0, func: TableFn::Range { from: b"w2\0".to_vec(), to: None } },
            TableSpec { over: 0, func: TableFn::Range { from: b"\xff".to_vec(), to: None } },
            TableSpec { over: 0, func: TableFn::Like { pattern: b"w1%".to_vec(), ilike: false, negated: false } },
            TableSpec { over: 0, func: TableFn::Like { pattern: b"w05".to_vec(), ilike: false, negated: true } },
            TableSpec { over: 0, func: TableFn::Like { pattern: b"u00000%".to_vec(), ilike: false, negated: false } },
            TableSpec { over: 0, func: TableFn::Like { pattern: b"%%".to_vec(), ilike: false, negated: true } },
            // D3288: Segments (runs between %s), scanned granule by granule on a verbatim part, streamed over a dictionary payload
            TableSpec { over: 0, func: TableFn::Like { pattern: b"%w%1%".to_vec(), ilike: false, negated: false } },
            TableSpec { over: 0, func: TableFn::Like { pattern: b"u0%0%7".to_vec(), ilike: false, negated: false } },
            TableSpec { over: 0, func: TableFn::Like { pattern: b"w%2".to_vec(), ilike: false, negated: true } },
        ];
        let recipe = GranuleRecipe::for_test_codes(rel, &[1], &[1], Arc::new(PartRegistry::new(64 << 20))).with_tables(tables);
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        let mut src = GranuleRangeSource::new(recipe);
        src.bind(&binding_ref(&resolved)).unwrap();
        src.seek(&[0, total], &[0, 0]).unwrap();
        let mut lanes = [Lane::NONE; MAX_LANES];
        let mut rows_seen = 0u64;
        let mut parts = std::collections::BTreeSet::new();
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
            let mut w = Window::over(&mut site);
            let n = src.window(&mut w).unwrap();
            if n == 0 {
                break;
            }
            let codes = w.lanes()[0];
            let p = w.base().part_id;
            parts.insert(p);
            let mut t = [Lane::NONE; 19];
            for (i, l) in t.iter_mut().enumerate() {
                assert_eq!(src.lane((2 + i) as u16, &[], l).unwrap(), 1, "{name}: lane {} fills", 2 + i);
            }
            let dfps = unsafe { gravity_dict::Fp128::from_lane(&t[6]) }.expect("fp128");
            let bits = unsafe { gravity_dict::Bits::from_lane(&t[0]) }.expect("bits");
            let lens = unsafe { gravity_dict::I64::from_lane(&t[1]) }.expect("length lane");
            let fps = unsafe { gravity_dict::Fp128::from_lane(&t[2]) }.expect("fp128");
            let chars = unsafe { gravity_dict::I64::from_lane(&t[3]) }.expect("length lane");
            let derived = unsafe { gravity_dict::Derived::from_lane(&t[4]) }.expect("derived");
            let nbits = unsafe { gravity_dict::Bits::from_lane(&t[5]) }.expect("bits");
            let (eqs, nes) = unsafe { (gravity_dict::Bits::from_lane(&t[7]).expect("bits"), gravity_dict::Bits::from_lane(&t[8]).expect("bits")) };
            let runs: Vec<gravity_dict::Bits> = t[9..16].iter().map(|l| unsafe { gravity_dict::Bits::from_lane(l) }.expect("bits")).collect();
            let segs: Vec<gravity_dict::Bits> = t[16..].iter().map(|l| unsafe { gravity_dict::Bits::from_lane(l) }.expect("bits")).collect();
            for i in 0..n as usize {
                if !unsafe { codes.is_valid(i) } {
                    continue;
                }
                let c = unsafe { *(codes.ptr as *const u32).add(i) };
                let (datum, len) = src.dict_value(p, 0, c).unwrap();
                let v = unsafe { core::slice::from_raw_parts((datum as *const u8).add(4), len as usize) };
                assert_eq!(unsafe { bits.get(c) }, v.contains(&b'1'), "{name}: LIKE '%1%' on {:?}", String::from_utf8_lossy(v));
                assert_eq!(unsafe { lens.get(c) }, v.len() as i64, "{name}: octet_length");
                assert_eq!(Some(unsafe { fps.get(c) }), gravity_dict::fp::fp128(v), "{name}: fingerprint (seeded SipHash-128)");
                assert_eq!(unsafe { chars.get(c) }, v.iter().filter(|&&b| b & 0xC0 != 0x80).count() as i64, "{name}: length");
                let letters = v.iter().take_while(|b| b.is_ascii_lowercase()).count();
                let want: &[u8] = if letters > 0 && v[letters..].iter().all(u8::is_ascii_digit) { &v[..letters] } else { v };
                assert_eq!(unsafe { derived.view(c).bytes() }, want, "{name}: regexp_replace('^([a-z]+)[0-9]*$', '\\1') on {:?}", String::from_utf8_lossy(v));
                assert_eq!(unsafe { nbits.get(c) }, !(v.len() >= 2 && v[v.len() - 2] == b'0'), "{name}: NOT LIKE '%0_' on {:?}", String::from_utf8_lossy(v));
                assert_eq!(Some(unsafe { dfps.get(c) }), gravity_dict::fp::fp128(want), "{name}: the derived value's fingerprint on {:?}", String::from_utf8_lossy(v));
                assert_eq!(unsafe { eqs.get(c) }, eq.contains(&v), "{name}: IN {eq:?} on {:?}", String::from_utf8_lossy(v));
                assert_eq!(unsafe { nes.get(c) }, !ne.contains(&v), "{name}: NOT IN {ne:?} on {:?}", String::from_utf8_lossy(v));
                let want = [v >= &b"w1"[..] && v < &b"w3"[..], v > &b"w2"[..], false, v.starts_with(b"w1"), v != b"w05", v.starts_with(b"u00000"), false];
                for (k, (r, want)) in runs.iter().zip(want).enumerate() {
                    assert_eq!(unsafe { r.get(c) }, want, "{name}: run table {k} on {:?}", String::from_utf8_lossy(v));
                }
                let w1 = v.iter().position(|&b| b == b'w').is_some_and(|p| v[p + 1..].contains(&b'1'));
                let u07 = v.len() >= 3 && v.starts_with(b"u0") && v.ends_with(b"7") && v[2..v.len() - 1].contains(&b'0');
                let w2 = v.len() >= 2 && v.starts_with(b"w") && v.ends_with(b"2");
                for (k, (r, want)) in segs.iter().zip([w1, u07, !w2]).enumerate() {
                    assert_eq!(unsafe { r.get(c) }, want, "{name}: segments table {k} on {:?}", String::from_utf8_lossy(v));
                }
            }
            rows_seen += n as u64;
        }
        assert_eq!(rows_seen, list.rows());
        assert_eq!(src.tables_built(), 19 * parts.len() as u64, "{name}: nineteen tables per part, built once");
    }
}

/// The by-rowid fetch (fetch.md §1.2) over the int8 fixture (three parts, NULLs), the dictionary
/// text fixture and the verbatim one: rowids in shuffled request order across parts and granules
/// come back in request order with the oracle's values and validity; each touched `(granule,
/// column)` is decoded once; a rowid past a granule's rows is an error, never a row.
#[test]
fn fetch_by_rowid_returns_the_requested_rows_in_request_order_across_parts() {
    use gravity_hosts::ops::scan::pgrc::fetch_site::{FetchRecipe, FetchSource, KIND_INT, KIND_TEXT};
    let int = fixture();
    let text = text_fixture();
    let verbatim = verbatim_text_fixture(true);
    let cases: [(&str, &BuiltFixture, u8, u32); 3] = [("int8", &int, KIND_INT, REL), ("dict-text", &text, KIND_TEXT, REL_TEXT + 6), ("verbatim-text", &verbatim, KIND_TEXT, REL_TEXT + 7)];
    for (name, bf, kind, rel) in cases {
        let list = part_list(bf);
        let recipe = FetchRecipe::for_test(rel, &[1], &[kind], Arc::new(PartRegistry::new(64 << 20)));
        let resolved = Resolved::for_rel(rel, list.clone());
        let mut src = FetchSource::new(recipe);
        src.bind(&binding_ref(&resolved)).unwrap();
        // global row i -> (part, granule, row) through the list's geometry (granules of 8192 rows, the last shorter)
        let locate = |i: u64| {
            let mut base = 0u64;
            for (p, spec) in list.parts().iter().enumerate() {
                if i < base + spec.rows {
                    let local = i - base;
                    return ((p as u64) << 32) | ((local / 8192) << 13) | (local % 8192);
                }
                base += spec.rows;
            }
            panic!("row {i} outside the fixture");
        };
        let total = list.rows();
        let picks: Vec<u64> = [total - 1, 0, 8191, 8192, 24_576, 24_577, 13, 26, 30_000, 1, 2, total / 2, 8193].into_iter().filter(|&i| i < total).collect();
        let rowids: Vec<u64> = picks.iter().map(|&i| locate(i)).collect();
        let mut lanes = [Lane::NONE; 4];
        let before = src.census();
        let n = src.fetch(&rowids, &mut lanes).unwrap_or_else(|e| panic!("{name}: fetch: {e:?} {:?}", gravity_hosts::take_error().map(|e| e.message().to_string())));
        assert_eq!(n as usize, picks.len(), "{name}: rows fetched");
        let l = lanes[0];
        assert_eq!((l.width, l.kind), (8, if kind == KIND_TEXT { lane::KIND_BYREF } else { lane::KIND_BYVAL }), "{name}: lane shape");
        for (j, &i) in picks.iter().enumerate() {
            let valid = unsafe { l.is_valid(j) };
            let d = unsafe { l.datum(j) };
            match name {
                "int8" => assert_eq!(valid.then_some(d as i64), value(i), "{name}: row {i} at request index {j}"),
                _ => {
                    assert!(valid, "{name}: text rows are NOT NULL");
                    let bytes = unsafe { datum::VarlenaRef::from_ptr(d as *const u8) }.data();
                    let want = if name == "verbatim-text" && i % 7 == 0 { Vec::new() } else { tv(i) };
                    assert_eq!(bytes, &want[..], "{name}: row {i} at request index {j}");
                }
            }
        }
        let (fetched, decodes) = src.census();
        let mut groups: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
        for id in &rowids {
            groups.insert(id >> 13);
        }
        assert_eq!((fetched - before.0, decodes - before.1), (picks.len() as u64, groups.len() as u64), "{name}: one decode per touched (granule, column)");
        // a second fetch reuses the open cursors and returns the same rows
        let n2 = src.fetch(&rowids[..3], &mut lanes).unwrap();
        assert_eq!(n2, 3);
        let bad = [locate(total - 1) | 0x1FFF];
        assert!(src.fetch(&bad, &mut lanes).is_err(), "{name}: a row past the granule is an error");
        src.unbind();
    }
}

/// The statement's shared tables (R4): a key's table is built once and every later reader of the
/// key — another source, pass 2 — gets the same table; past the cap the least recently used
/// leaves the store (its holders keep it) and the next reader builds again.
#[test]
fn a_statements_shared_table_is_built_once_and_read_by_every_source_under_the_cap() {
    use gravity_hosts::ops::scan::pgrc::granule_source::StmtKey;
    use gravity_hosts::dict::text_hosts::build_owned;
    struct Words(Vec<Vec<u8>>);
    impl gravity_dict::Dict for Words {
        fn ncodes(&self) -> u32 {
            self.0.len() as u32
        }
        fn entries(&self, f: &mut dyn FnMut(u32, gravity_dict::View)) {
            for (c, w) in self.0.iter().enumerate() {
                f(c as u32, gravity_dict::View::of(w));
            }
        }
    }
    let dict = Words((0..1000u32).map(|i| format!("entry-{i}").into_bytes()).collect()); // alloc: Cold
    let builds = Cell::new(0u32);
    let build = |key: StmtKey| {
        builds.set(builds.get() + 1);
        build_owned(1000, 0, &mut |region| Ok(gravity_dict::I64::build(&dict, region, &mut |v| Ok(unsafe { v.bytes() }.len() as i64 + key.part as i64))?.header())).map_err(|_| Box::new(types_error::PgError::error("build".to_string()))) // alloc: Cold
    };
    let key = |part: u32| StmtKey { rel: 0, part, attno: 1, fingerprint: 7 };
    let resolved = Resolved::new();
    resolved.set_table_cap(1 << 20);
    let t = resolved.tables().get_or_build(key(0), &mut || build(key(0))).unwrap();
    let again = resolved.tables().get_or_build(key(0), &mut || build(key(0))).unwrap();
    assert!(Arc::ptr_eq(&t, &again), "the second reader gets the first builder's table");
    assert_eq!(builds.get(), 1);
    let lens = unsafe { gravity_dict::I64::at(t.header()) };
    assert_eq!(unsafe { lens.get(999) }, "entry-999".len() as i64);
    assert_eq!(resolved.tables().held(), (1, t.bytes()));
    // a cap below two tables: the older leaves when the second lands; the held one stays readable
    let small = Resolved::new();
    small.set_table_cap(t.bytes() + t.bytes() / 2);
    let a = small.tables().get_or_build(key(1), &mut || build(key(1))).unwrap();
    let _b = small.tables().get_or_build(key(2), &mut || build(key(2))).unwrap();
    assert_eq!(small.tables().held().0, 1, "one table under the cap");
    assert_eq!(unsafe { gravity_dict::I64::at(a.header()).get(0) }, "entry-0".len() as i64 + 1, "the evicted table's holder still reads it");
    let a2 = small.tables().get_or_build(key(1), &mut || build(key(1))).unwrap();
    assert!(!Arc::ptr_eq(&a, &a2), "an evicted key builds again");
    assert_eq!(builds.get(), 4);
    // nothing shared without a cap: every reader builds
    let none = Resolved::new();
    let x = none.tables().get_or_build(key(3), &mut || build(key(3))).unwrap();
    let y = none.tables().get_or_build(key(3), &mut || build(key(3))).unwrap();
    assert!(!Arc::ptr_eq(&x, &y));
    assert_eq!(none.tables().held(), (0, 0));
}

const REL_SELECT: u32 = 4262;

/// A dictionary text column (`''` for every row but each ninth, and for all of granule 2) beside
/// an int8 column `a = i`, over two parts.
fn text_int_fixture() -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    let rows = 35_424u64;
    let value = |i: u64| if (16_384..24_576).contains(&i) || i % 9 != 0 { Vec::new() } else { tv(i) };
    let fx = Fixture {
        name: "gravity_text_int",
        dir: "/qa/t7311".to_string(),
        spc: 1663,
        db: 5,
        relfilenumber: 7311,
        schema: vec![text_col(1), int8_col(2)],
        oracle: vec![(0..rows).map(|i| Some(OracleVal::Bytes(value(i)))).collect(), (0..rows).map(|i| Some(OracleVal::Word(i))).collect()],
        plans: vec![],
        policy: PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() },
    };
    let posture = ColumnPosture { dict: Some(DictPolicy { ndv_cap: 100_000, exec_ok: true, sem: TextSemantics::BytesOnly }), ..ColumnPosture::default() };
    let cands = CodecCandidates::new(ColumnPosture::default()).with_column(1, 0, posture);
    let sources: [&dyn CandidateSource; 2] = [&cands, &ReferenceCandidates];
    let mut vfs = pgrc_qa::simvfs::SimVfs::new();
    vfs.mkdir_path(&fx.dir).expect("mkdir");
    let fxid = 900 + fx.relfilenumber;
    let mut probe = Probe::new(TxnVerdict::Aborted);
    probe.mark(fxid, TxnVerdict::InProgress);
    let mut w = open_writer(&fx, fxid).expect("writer");
    append_rows_with_sources(&mut vfs, &mut w, &fx, 0, rows, &sources).expect("append");
    finish_with_sources(&mut vfs, &mut w, &sources).expect("finish");
    w.publish(&mut vfs, &probe).expect("publish");
    probe.mark(fxid, TxnVerdict::Committed);
    let files = vfs.snapshot_dir(&fx.dir);
    let manifest = effective_manifest(&mut vfs, &fx.dir, &probe, None).expect("effective").expect("committed generation");
    BuiltFixture { fx, files, manifest }
}

/// `code lane 0 <> c` as lowerv4 declares a text comparison with any constant (verdict_zone.rs `byref_of`): a bound
/// prefix predicate whose constant `c` this execution binds, its image (a short varlena, leaked for the test) in
/// `recipe`'s verdict cell (its own: `with_verdict_cell`), under the zone road's protocol.
fn text_ne(recipe: &GranuleRecipe, c: &[u8]) -> ZonePred {
    let img: &'static [u8] = Box::leak([&[((c.len() as u8 + 1) << 1) | 1][..], c].concat().into_boxed_slice()); // alloc: Cold
    recipe.verdicts.set(0, gravity_program::protos::VERDICT_ZONE);
    let arg = gravity_program::protos::word_zone_at_arg(0, ZoneOp::Ne as u8, BOUND_TEXT_PREFIX, 0);
    recipe.verdicts.set_word_zone([(arg, 0usize)].into_iter(), &[img.as_ptr() as u64], &[0]);
    ZonePred::bound_prefix(0, ZoneOp::Ne, 0, BOUND_TEXT_PREFIX)
}

/// Every window of `src` over `total` granules: (windows, rows through the selection, selected windows,
/// dense windows), each selected row's int lane `k` checked against `a = i` and `check(window, i)`.
fn walk_selected(src: &mut GranuleRangeSource, total: u64, k: usize, check: impl Fn(&Window<'_>, usize) -> bool) -> (u64, u64, u64, u64) {
    src.seek(&[0, total], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let (mut windows, mut through, mut selected, mut dense) = (0u64, 0u64, 0u64, 0u64);
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        assert!(!w.is_answered());
        windows += 1;
        let base = w.base().pos.block as u64 * GRANULE;
        let la = w.lanes()[k];
        if w.sel.is_null() {
            dense += 1;
        } else {
            selected += 1;
            assert!(w.sel_n as usize * 8 <= n as usize, "a selection is at most an eighth of the window");
        }
        for j in 0..w.end() {
            let i = unsafe { w.index(j) };
            assert!(check(&w, i), "row {i} of window {windows} fails the predicate");
            assert_eq!(unsafe { la.datum(i) }, base + i as u64, "the selected row's int lane is in place");
            through += 1;
        }
    }
    (windows, through, selected, dense)
}

/// `text_int_fixture`'s text value of the row whose int lane is `a`.
fn text_int_value(a: u64) -> Vec<u8> {
    if (16_384..24_576).contains(&a) || a % 9 != 0 { Vec::new() } else { tv(a) }
}

/// A text comparison with any constant, `''` or another, prunes by the part's code zones and selects its rows on the
/// decoded codes (the constant's code in each part, `row_select`): `<> ''` skips granule 2 (all '') and selects the
/// ninth rows elsewhere; `<> c` for a value a few rows hold, and for one no row holds, skips nothing and keeps every
/// window dense (more than an eighth passes: the program's filter drops the rows that hold `c`).
#[test]
fn a_text_filter_selects_the_rows_the_other_lanes_decode_and_skips_the_empty_granule() {
    let bf = text_int_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    assert_eq!(total, 5);
    let run = |c: &[u8]| {
        let recipe = GranuleRecipe::for_test_codes(REL_SELECT, &[1, 2], &[0b1], Arc::new(PartRegistry::new(64 << 20))).with_verdict_cell();
        let z = text_ne(&recipe, c);
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        let mut src = GranuleRangeSource::new(recipe);
        src.zone(&[z], &[0], &[0]);
        src.bind(&binding_ref(&resolved)).unwrap();
        let c = c.to_vec(); // alloc: Cold
        let walk = walk_selected(&mut src, total, 1, |w, i| w.sel.is_null() || text_int_value(unsafe { w.lanes()[1].datum(i) } as u64) != c);
        let counts = (src.granules_selected(), src.granules_decoded(), src.granules_skipped());
        src.unbind();
        (walk, counts)
    };
    let nonempty = (0..35_424u64).filter(|&i| !(16_384..24_576).contains(&i) && i % 9 == 0).count() as u64;
    assert_eq!(run(b""), ((4, nonempty, 4, 0), (4, 4, 1)), "granule 2 (all empty) skipped, the rest selected");
    assert_eq!(run(&tv(9)), ((5, 35_424, 0, 5), (0, 5, 0)), "<> a value a few rows hold: nothing skipped, every window dense");
    assert_eq!(run(b"absent"), ((5, 35_424, 0, 5), (0, 5, 0)), "<> a value no row holds: every part AllMatch by its codes");
    // the same walk without the predicate: every window dense, every row through
    let recipe = GranuleRecipe::for_test_codes(REL_SELECT, &[1, 2], &[0b1], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut plain = GranuleRangeSource::new(recipe);
    plain.bind(&binding_ref(&resolved)).unwrap();
    assert_eq!(walk_selected(&mut plain, total, 1, |_, _| true), (5, 35_424, 0, 5));
    assert_eq!(plain.granules_selected(), 0);
}

/// D4554: D3160's part verdict asked once a part (the memo, `PGRUST_GRAVITY_PART_DECIDES_MEMO=1`) decides every granule
/// as asking it at each granule does: the same windows, rows, selections and skipped granules.
#[test]
fn the_part_verdict_memo_decides_each_granule_as_asking_at_each_granule_does() {
    let bf = text_int_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    let run = |memo: bool| {
        let recipe = GranuleRecipe::for_test_codes(REL_SELECT, &[1, 2], &[0b1], Arc::new(PartRegistry::new(64 << 20))).with_verdict_cell();
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        let z = text_ne(&recipe, b"");
        let mut src = GranuleRangeSource::new(recipe);
        src.set_part_decides_memo(memo);
        src.zone(&[z], &[0], &[0]);
        src.bind(&binding_ref(&resolved)).unwrap();
        let walk = walk_selected(&mut src, total, 1, |w, i| !text_int_value(unsafe { w.lanes()[1].datum(i) } as u64).is_empty());
        let counts = (src.granules_selected(), src.granules_decoded(), src.granules_skipped());
        src.unbind();
        (walk, counts)
    };
    let off = run(false);
    assert_eq!(off.1, (4, 4, 1), "granule 2 (all empty) skipped, the rest selected");
    assert_eq!(run(true), off, "the memo: the same verdicts");
}

#[test]
fn an_int_predicate_selects_the_mixed_granule_when_few_rows_survive() {
    let bf = sorted_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    // lane 1 reads the same column: the lane the predicate does not read decodes selectively
    let recipe = GranuleRecipe::for_test(REL_SORTED, &[1, 1], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    let thr = 2 * GRANULE as i64 + 100;
    src.zone(&[ZonePred::word(0, ZoneOp::Lt, thr)], &[0], &[0]);
    src.bind(&binding_ref(&resolved)).unwrap();
    // granules 0-1 AllMatch (decoded whole, no selection), granule 2 Mixed with 100 survivors (selected), 3-7 NoneMatch
    let (windows, through, selected, dense) = walk_selected(&mut src, total, 1, |w, i| unsafe { (w.lanes()[0].datum(i) as i64) < thr });
    assert_eq!((windows, through, selected, dense), (3, thr as u64, 1, 2));
    assert_eq!((src.granules_selected(), src.granules_decoded(), src.granules_skipped()), (1, 3, 5));
    // a wide survivor set (7/8 of the granule) keeps the dense window
    src.unbind();
    let recipe = GranuleRecipe::for_test(REL_SORTED, &[1, 1], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut wide = GranuleRangeSource::new(recipe);
    let thr = 3 * GRANULE as i64 - GRANULE as i64 / 8;
    wide.zone(&[ZonePred::word(0, ZoneOp::Lt, thr)], &[0], &[0]);
    wide.bind(&binding_ref(&resolved)).unwrap();
    let (windows, _, selected, dense) = walk_selected(&mut wide, total, 1, |_, _| true);
    assert_eq!((windows, selected, dense), (3, 0, 3));
}

/// A code lane's claim run brings the part's dictionary regions in with the codes (the writer lays them
/// right after the values): an unwrapped region into the cache, a wrapped index staged raw for the unwrap
/// that reads it first. The dictionary open that follows reads nothing more: an index it unwraps takes the
/// run's staged image.
#[test]
fn a_claim_run_on_a_code_lane_brings_the_dictionary_regions_and_the_dict_open_faults_nothing() {
    use pgrc_format::part::StreamRole;
    use pgrc_read::{FaultTag, OpenPart};
    let bf = text_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    let registry = Arc::new(PartRegistry::new(64 << 20));
    let recipe = GranuleRecipe::for_test_codes(REL_TEXT, &[1], &[1], Arc::clone(&registry));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    src.seek(&[0, total], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut site = blank_site(lanes.as_mut_ptr());
    let mut w = Window::over(&mut site);
    assert!(src.window(&mut w).unwrap() > 0);
    // part 0 through the registry: the fixture's (dev, ino, len)
    let len = bf.files.get(&part_file_name(bf.manifest.parts[0].part_no)).expect("part file").len() as u64;
    let part = registry.get((7, 500, len)).expect("part 0 is open");
    let roles = |p: &OpenPart| p.faults().iter().filter_map(|f| match f.tag { FaultTag::StreamExtent { role, .. } => Some(role), _ => None }).collect::<Vec<u8>>();
    let dir = part.stream_directory().unwrap();
    let payload_wrapped = dir.lookup(1, 0, StreamRole::DictPayload).unwrap().entry.wrapper != 0;
    let index_wrapped = dir.lookup(1, 0, StreamRole::DictIndex).unwrap().entry.wrapper != 0;
    let r0 = roles(&part);
    let staged = part.staged_extents();
    assert!(r0.contains(&(StreamRole::DictIndex as u8)) || (index_wrapped && staged >= 1), "the run brought the dictionary index: {r0:?}, {staged} staged");
    assert_eq!(r0.contains(&(StreamRole::DictPayload as u8)), !payload_wrapped, "an unwrapped payload rides the run; a wrapped one is the handle's: {r0:?}");
    let n = part.faults().len();
    let (_, vlen) = src.dict_value(0, 0, 1).unwrap();
    assert!(vlen > 0);
    let new_roles = roles(&part)[r0.len()..].to_vec();
    let index_now = new_roles.iter().filter(|r| **r == StreamRole::DictIndex as u8).count();
    let payload_now = new_roles.iter().filter(|r| **r == StreamRole::DictPayload as u8).count();
    assert!(index_now + payload_now == new_roles.len() && payload_now <= usize::from(payload_wrapped), "the dictionary open faulted at most the index and the wrapped payload: {new_roles:?} ({} → {} faults)", n, part.faults().len());
    assert!(index_now <= staged.saturating_sub(part.staged_extents()), "an index the open unwrapped took the run's stage (no read of its own): {index_now} unwrapped, {staged} → {} staged", part.staged_extents());
}

/// The length lanes read from the dictionary index (no table): `octet_length` from the index's byte
/// field for every dictionary part, `length` from its stored char field (absolute or delta — the
/// stored-lengths law the SQE oracle reads too); a verbatim part sweeps into an `I64` per statement;
/// both equal the rendered entries' lengths on ASCII and on multi-byte entries; a lane pointing into
/// the index is never a cached table (R4).
#[test]
fn the_length_lanes_read_the_dictionary_index_and_agree_with_the_rendered_entries() {
    use gravity_dict::{IdxLen, I64};
    use gravity_hosts::dict::text_hosts::{TableFn, TableSpec};
    for (name, bf, rel, utf8_claimed) in [("bytes-only", text_fixture(), REL_TEXT + 10, false), ("utf8", text_fixture_utf8(), REL_TEXT + 11, true), ("verbatim", verbatim_text_fixture(true), REL_TEXT + 12, false)] {
        let list = part_list(&bf);
        let total = list.granules();
        let tables = vec![TableSpec { over: 0, func: TableFn::OctetLength }, TableSpec { over: 0, func: TableFn::CharLength }];
        let recipe = GranuleRecipe::for_test_codes(rel, &[1], &[1], Arc::new(PartRegistry::new(64 << 20))).with_tables(tables);
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        let mut src = GranuleRangeSource::new(recipe);
        src.bind(&binding_ref(&resolved)).unwrap();
        for statement in 0..2 {
            src.seek(&[0, total], &[0, 0]).unwrap();
            let mut lanes = [Lane::NONE; MAX_LANES];
            let (mut rows_seen, mut multibyte) = (0u64, 0u64);
            loop {
                let mut site = blank_site(lanes.as_mut_ptr());
                let mut w = Window::over(&mut site);
                let n = src.window(&mut w).unwrap();
                if n == 0 {
                    break;
                }
                let codes = w.lanes()[0];
                let p = w.base().part_id;
                let mut t = [Lane::NONE; 2];
                for (i, l) in t.iter_mut().enumerate() {
                    assert_eq!(src.lane((2 + i) as u16, &[], l).unwrap(), 1, "{name}: lane {} fills", 2 + i);
                }
                let dict_part = name != "verbatim";
                assert_eq!(unsafe { IdxLen::<false>::from_lane(&t[0]) }.is_some(), dict_part, "{name}: octet_length is the index view exactly for a dictionary part");
                let chars_view = gravity_dict::lane::elem(&t[1]) != gravity_dict::ELEM_I64;
                assert_eq!(chars_view, dict_part, "{name}: length is the index view (the stored char fact) exactly for a dictionary part");
                let lens = unsafe { I64::from_lane(&t[0]) }.expect("octet lane");
                let chars = unsafe { I64::from_lane(&t[1]) }.expect("char lane");
                for i in 0..n as usize {
                    if !unsafe { codes.is_valid(i) } {
                        continue;
                    }
                    let c = unsafe { *(codes.ptr as *const u32).add(i) };
                    let (datum, len) = src.dict_value(p, 0, c).unwrap();
                    let v = unsafe { core::slice::from_raw_parts((datum as *const u8).add(4), len as usize) };
                    let want_chars = v.iter().filter(|&&b| b & 0xC0 != 0x80).count() as i64;
                    assert_eq!(unsafe { lens.get(c) }, v.len() as i64, "{name}: octet_length of {:?}", String::from_utf8_lossy(v));
                    assert_eq!(unsafe { chars.get(c) }, want_chars, "{name}: length of {:?}", String::from_utf8_lossy(v));
                    multibyte += (want_chars != v.len() as i64) as u64;
                    rows_seen += 1;
                }
            }
            assert_eq!(rows_seen, bf.fx.oracle[0].len() as u64, "{name}: statement {statement} saw every row");
            if utf8_claimed {
                assert!(multibyte > 0, "{name}: the fixture exercises multi-byte entries");
            }
        }
        src.unbind();
    }
}

const REL_CLASSES: u32 = 4263;

/// Every storage class's oracle value for row `i` (NULL every 13th row of the float and text columns).
fn class_value(k: usize, i: u64) -> Option<OracleVal> {
    match k {
        0 => (i % 13 != 4).then(|| OracleVal::Word((i as f64 * 0.25 - 100.0).to_bits())),
        1 => Some(OracleVal::Word((i % 3 == 0) as u64)),
        2 => Some(OracleVal::Bytes((i as u128).wrapping_mul(0x0101_0101_0101_0101_0101_0101_0101_0101).to_le_bytes().to_vec())),
        // one image past the 32 KiB inline ceiling (the overflow stream) and past the first arena
        3 => (i % 13 != 7).then(|| OracleVal::Bytes(if i == 20_001 { vec![b'z'; 40_000] } else { format!("t{}", i % 977).into_bytes() })),
        4 => Some(OracleVal::Word(i % 251)),
        _ => (i % 13 != 9).then(|| OracleVal::Word((i as f32 * 0.5).to_bits() as u64)),
    }
}

/// f64, bool, fixed16, verbatim text, an unsigned 1-byte word, f32: two parts of 24 576 and 10 848 rows.
fn classes_fixture() -> BuiltFixture {
    let mut c1 = int8_col(5);
    c1.class = StorageClass::ByvalWord { width: 1, signed: false };
    c1.typlen = 1;
    c1.typalign = b'c';
    c1.semantics = pgrc_format::class::TypeSemantics::UnsignedInt;
    let mut f32c = pgrc_qa::f64_col(6);
    f32c.class = StorageClass::F32;
    f32c.typlen = 4;
    f32c.typalign = b'i';
    let rows = 35_424u64;
    let oracle = |k: usize| (0..rows).map(|i| class_value(k, i)).collect();
    build_fixture(Fixture { name: "gravity_classes", dir: "/qa/t7312".to_string(), spc: 1663, db: 5, relfilenumber: 7312, schema: vec![pgrc_qa::f64_col(1), pgrc_qa::bool_col(2), pgrc_qa::fixed16_col(3), text_col(4), c1, f32c], oracle: (0..6).map(oracle).collect(), plans: vec![], policy: PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() } })
}

/// The bytes a by-reference lane's datum pointer addresses: a fixed image of `len`, or the varlena
/// payload behind its 4-byte header.
unsafe fn byref_bytes(datum: u64, fixed: Option<usize>) -> Vec<u8> {
    let p = datum as *const u8;
    match fixed {
        Some(len) => core::slice::from_raw_parts(p, len).to_vec(),
        None => {
            let hdr = u32::from_le_bytes(*(p as *const [u8; 4]));
            assert_eq!(hdr & 3, 0, "a 4-byte uncompressed varlena header");
            core::slice::from_raw_parts(p.add(4), (hdr >> 2) as usize - 4).to_vec()
        }
    }
}

#[test]
fn every_storage_class_is_published_as_a_lane_of_its_kind_and_reads_as_the_datum_path_does() {
    use gravity_hosts::ops::scan::pgrc::granule_source::lane_of_class;
    let bf = classes_fixture();
    let list = part_list(&bf);
    let rows = list.rows();
    let classes = [StorageClass::F64, StorageClass::Bool, StorageClass::Fixed { len: 16 }, StorageClass::VarlenaVerbatim, StorageClass::ByvalWord { width: 1, signed: false }, StorageClass::F32];
    let kinds: Vec<(u8, u8)> = classes.iter().map(|&c| lane_of_class(c)).collect();
    assert_eq!(kinds, [(lane::KIND_FIXED, 8), (lane::KIND_FIXED, 1), (lane::KIND_BYREF, 8), (lane::KIND_BYREF, 8), (lane::KIND_BYVAL, 1), (lane::KIND_FIXED, 4)]);
    let recipe = GranuleRecipe::for_test_classes(REL_CLASSES, &[1, 2, 3, 4, 5, 6], &classes, Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut seen: Vec<Vec<Option<OracleVal>>> = vec![Vec::new(); 6];
    let mut pass = |src: &mut GranuleRangeSource, mut out: Option<&mut Vec<Vec<Option<OracleVal>>>>| {
        src.seek(&[0, list.granules()], &[0, 0]).unwrap();
        let mut windows = 0u64;
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
            let mut w = Window::over(&mut site);
            let n = src.window(&mut w).unwrap();
            if n == 0 {
                break;
            }
            windows += 1;
            for (k, &(kind, width)) in kinds.iter().enumerate() {
                let l = w.lanes()[k];
                assert_eq!((l.kind, l.width), (kind, width), "lane {k}");
            }
            let Some(out) = out.as_deref_mut() else { continue };
            for i in 0..n as usize {
                for k in 0..6 {
                    let l = w.lanes()[k];
                    // SAFETY: `i < n` rows of the window; the lane's cells are at its width.
                    let v = unsafe {
                        if !l.is_valid(i) {
                            None
                        } else {
                            match k {
                                0 => Some(OracleVal::Word(*(l.ptr as *const u64).add(i))),
                                1 | 4 => Some(OracleVal::Word(*l.ptr.add(i) as u64)),
                                2 => Some(OracleVal::Bytes(byref_bytes(*(l.ptr as *const u64).add(i), Some(16)))),
                                3 => Some(OracleVal::Bytes(byref_bytes(*(l.ptr as *const u64).add(i), None))),
                                _ => Some(OracleVal::Word(*(l.ptr as *const u32).add(i) as u64)),
                            }
                        }
                    };
                    out[k].push(v);
                }
            }
        }
        windows
    };
    assert_eq!(pass(&mut src, Some(&mut seen)), 5, "one window per granule");
    for k in 0..6 {
        assert_eq!(seen[k].len() as u64, rows);
        assert!(seen[k].iter().enumerate().all(|(i, v)| *v == class_value(k, i as u64)), "lane {k} through the windows");
    }
    // the row protocol: the same datum words (the by-reference lanes' pointers deref to the same images)
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    let (mut v, mut n) = ([0u64; 6], [0u8; 6]);
    let mut i = 0u64;
    while src.next(&mut v, &mut n).unwrap() {
        for k in 0..6 {
            let got = (n[k] == 0).then(|| unsafe {
                match k {
                    2 => OracleVal::Bytes(byref_bytes(v[k], Some(16))),
                    3 => OracleVal::Bytes(byref_bytes(v[k], None)),
                    _ => OracleVal::Word(v[k]),
                }
            });
            assert!(got == class_value(k, i), "row {i} lane {k}");
        }
        i += 1;
    }
    assert_eq!(i, rows);
    const PASSES: usize = 10;
    let allocs = tracked(|| {
        for _ in 0..PASSES {
            assert_eq!(pass(&mut src, None), 5);
        }
    });
    assert_eq!(allocs, 0, "{allocs} heap allocations in {PASSES} passes: the arenas stay at their high-water mark");
    src.unbind();
}

const REL_SHRED: u32 = 4264;

/// Row `i`'s document: `k` an int, `name` a text, `flag` a bool, `score` a number of two decimals; NULL every 13th row.
fn shred_doc(i: u64) -> Option<String> {
    (i % 13 != 5).then(|| format!(r#"{{"k": {}, "name": "tok{:04}", "flag": {}, "score": {}.25}}"#, i % 1000, i % 97, i % 2 == 0, i % 1000))
}

/// A jsonb column of `shred_doc` documents sealed with the production shred source: two parts of
/// 24 576 and 10 848 rows, the image beside the four path lanes (`k` at scale 0, `score` at scale 2).
fn shred_fixture() -> BuiltFixture {
    shred_fixture_of(shred_doc, 7313)
}

/// `shred_fixture`'s parts over documents `doc` at relfilenumber `rfn`.
fn shred_fixture_of(doc: fn(u64) -> Option<String>, rfn: u64) -> BuiltFixture {
    shred_fixture_with(doc, rfn, &CodecCandidates::new(ColumnPosture::default()))
}

/// `shred_fixture_of` under candidates `cands`.
fn shred_fixture_with(doc: fn(u64) -> Option<String>, rfn: u64, cands: &CodecCandidates) -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_shred, finish_with_shred, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    use pgrc_write::shred_jsonb::JsonbShredSource;
    let _ = mbutils::SetDatabaseEncoding(wchar::PG_UTF8);
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(mbutils::init_seams);
    let ctx = mcx::MemoryContext::new("granule-shred-test");
    let rows = 35_424u64;
    let images: Vec<Option<OracleVal>> = (0..rows)
        .map(|i| {
            doc(i).map(|d| {
                let img = adt_jsonb::io::jsonb_in(ctx.mcx(), d.as_bytes(), None).unwrap_or_else(|e| panic!("jsonb_in: {}", e.message())).expect("jsonb");
                let hdr = u32::from_le_bytes([img[0], img[1], img[2], img[3]]);
                assert_eq!((hdr >> 2) as usize, img.len(), "a 4-byte varlena header");
                OracleVal::Bytes(img[4..].to_vec())
            })
        })
        .collect();
    let fx = Fixture { name: "gravity_shred", dir: format!("/qa/t{rfn}"), spc: 1663, db: 5, relfilenumber: rfn, schema: vec![text_col(1)], oracle: vec![images], plans: vec![], policy: PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() } };
    let sources: [&dyn CandidateSource; 2] = [cands, &ReferenceCandidates];
    let mut shred = JsonbShredSource::new().with_column(1);
    let mut vfs = pgrc_qa::simvfs::SimVfs::new();
    vfs.mkdir_path(&fx.dir).expect("mkdir");
    let fxid = 900 + fx.relfilenumber;
    let mut probe = Probe::new(TxnVerdict::Aborted);
    probe.mark(fxid, TxnVerdict::InProgress);
    let mut w = open_writer(&fx, fxid).expect("writer");
    append_rows_with_shred(&mut vfs, &mut w, &fx, 0, rows, &sources, &mut shred).expect("append");
    finish_with_shred(&mut vfs, &mut w, &sources, &mut shred).expect("finish");
    w.publish(&mut vfs, &probe).expect("publish");
    probe.mark(fxid, TxnVerdict::Committed);
    let files = vfs.snapshot_dir(&fx.dir);
    let manifest = effective_manifest(&mut vfs, &fx.dir, &probe, None).expect("effective").expect("committed generation");
    BuiltFixture { fx, files, manifest }
}

#[test]
fn a_shredded_jsonb_path_is_published_as_a_lane_of_its_kind_beside_the_image() {
    use gravity_hosts::ops::scan::pgrc::granule_source::{shred_lane_ord, ShredLane};
    use pgrc_format::shredlane::ShredLaneKind;
    let bf = shred_fixture();
    let list = part_list(&bf);
    let rows = list.rows();
    let registry = Arc::new(PartRegistry::new(64 << 20));
    // the part elected the four paths; an unelected path resolves to nothing, a wrong kind refuses
    {
        let part = open_part_bytes(bf.files.get(&part_file_name(0)).expect("part"), 950).expect("open part");
        assert_eq!(shred_lane_ord(&part, 1, "k", ShredLaneKind::NumericFs, 0).unwrap(), Some(1));
        assert_eq!(shred_lane_ord(&part, 1, "nope", ShredLaneKind::Text, 0).unwrap(), None);
        assert!(shred_lane_ord(&part, 1, "k", ShredLaneKind::Text, 0).is_err(), "sealed at another kind");
        assert!(shred_lane_ord(&part, 1, "score", ShredLaneKind::NumericFs, 0).is_err(), "sealed at another scale");
    }
    let classes = [StorageClass::VarlenaVerbatim; 5];
    let recipe = GranuleRecipe::for_test_classes(REL_SHRED, &[1, 1, 1, 1, 1], &classes, registry.clone())
        .with_shred_lane(1, "k", ShredLaneKind::NumericFs, 0)
        .with_shred_lane(2, "name", ShredLaneKind::Text, 0)
        .with_shred_lane(3, "flag", ShredLaneKind::Bool, 0)
        .with_shred_lane(4, "score", ShredLaneKind::NumericFs, 2);
    let mut shred = gravity_hosts::LaneBits::new(recipe.natts);
    recipe.shred_lanes(&mut shred);
    assert_eq!(shred.words()[0], 0b11110);
    assert_eq!((recipe.kinds[1], recipe.widths[1]), ShredLane::lane_of(ShredLaneKind::NumericFs, 0));
    assert_eq!(&recipe.kinds[..5], &[lane::KIND_BYREF, lane::KIND_BYVAL, lane::KIND_BYREF, lane::KIND_FIXED, lane::KIND_FIXED]);
    assert_eq!(&recipe.widths[..5], &[8, 8, 8, 1, 8]);
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut pass = |src: &mut GranuleRangeSource, check: bool| {
        src.seek(&[0, list.granules()], &[0, 0]).unwrap();
        let (mut windows, mut i) = (0u64, 0u64);
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
            let mut w = Window::over(&mut site);
            let n = src.window(&mut w).unwrap();
            if n == 0 {
                break;
            }
            windows += 1;
            assert_eq!(w.lanes()[4].ncodes_or_scale, 2, "the score lane carries its scale");
            assert_eq!(w.lanes()[1].ncodes_or_scale, 0);
            if !check {
                continue;
            }
            for r in 0..n as usize {
                let l = w.lanes();
                let valid: Vec<bool> = (0..5).map(|k| unsafe { l[k].is_valid(r) }).collect();
                match shred_doc(i) {
                    None => assert_eq!(valid, [false; 5], "row {i}: a NULL document has no path"),
                    Some(_) => {
                        assert_eq!(valid, [true; 5], "row {i}");
                        // SAFETY: `r < n`; the lanes' cells are at their widths.
                        unsafe {
                            assert_eq!(*(l[1].ptr as *const i64).add(r), (i % 1000) as i64, "row {i} k");
                            assert_eq!(byref_bytes(*(l[2].ptr as *const u64).add(r), None), format!("tok{:04}", i % 97).into_bytes(), "row {i} name");
                            assert_eq!(*l[3].ptr.add(r), (i % 2 == 0) as u8, "row {i} flag");
                            assert_eq!(*(l[4].ptr as *const i64).add(r), (i % 1000) as i64 * 100 + 25, "row {i} score at scale 2");
                            let img = byref_bytes(*(l[0].ptr as *const u64).add(r), None);
                            assert_eq!(Some(OracleVal::Bytes(img)), bf.fx.oracle[0][i as usize], "row {i} image");
                        }
                    }
                }
                i += 1;
            }
        }
        if check {
            assert_eq!(i, rows);
        }
        windows
    };
    assert_eq!(pass(&mut src, true), 5, "one window per granule");
    const PASSES: usize = 10;
    let allocs = tracked(|| {
        for _ in 0..PASSES {
            assert_eq!(pass(&mut src, false), 5);
        }
    });
    assert_eq!(allocs, 0, "{allocs} heap allocations in {PASSES} passes of shred windows");
    src.unbind();
    // a lane the parts did not elect (or elected at another kind or scale) is all-invalid in every window: each row walks
    // its image (M6 D2, main 09-27), never a refusal
    let recipe = GranuleRecipe::for_test_classes(REL_SHRED, &[1, 1, 1, 1], &classes[..4], registry)
        .with_shred_lane(1, "nope", ShredLaneKind::Text, 0)
        .with_shred_lane(2, "k", ShredLaneKind::Text, 0)
        .with_shred_lane(3, "score", ShredLaneKind::NumericFs, 0);
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    let mut i = 0u64;
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        let l = w.lanes();
        for r in 0..n as usize {
            // SAFETY: `r < n`.
            let valid: Vec<bool> = (0..4).map(|k| unsafe { l[k].is_valid(r) }).collect();
            assert_eq!(valid, [shred_doc(i).is_some(), false, false, false], "row {i}: unelected, another kind, another scale");
            i += 1;
        }
    }
    assert_eq!(i, rows);
    src.unbind();
}

/// `shred_doc`, but the second part's documents (rows from `MIXED_CUT`) hold no `name` and a text `flag`: that part does
/// not elect `name`, and elects `flag` as text.
const MIXED_CUT: u64 = 24_576;

fn mixed_doc(i: u64) -> Option<String> {
    if i < MIXED_CUT {
        return shred_doc(i);
    }
    (i % 13 != 5).then(|| format!(r#"{{"k": {}, "flag": "{}", "score": {}.25}}"#, i % 1000, if i % 2 == 0 { "yes" } else { "no" }, i % 1000))
}

#[test]
fn one_scan_over_a_part_that_elected_a_path_and_one_that_did_not_publishes_the_second_all_invalid() {
    use gravity_hosts::ops::scan::pgrc::granule_source::{shred_lane_elected, Elected};
    use pgrc_format::shredlane::ShredLaneKind;
    let bf = shred_fixture_of(mixed_doc, 7314);
    let list = part_list(&bf);
    assert_eq!(list.parts().len(), 2);
    assert_eq!(list.parts()[0].rows, MIXED_CUT, "the documents change at the part cut");
    {
        let p0 = open_part_bytes(bf.files.get(&part_file_name(0)).expect("part"), 950).expect("open part");
        let p1 = open_part_bytes(bf.files.get(&part_file_name(1)).expect("part"), 950).expect("open part");
        assert!(matches!(shred_lane_elected(&p0, 1, "name", ShredLaneKind::Text, 0).unwrap(), Elected::At(_)));
        assert_eq!(shred_lane_elected(&p1, 1, "name", ShredLaneKind::Text, 0).unwrap(), Elected::No);
        assert!(matches!(shred_lane_elected(&p0, 1, "flag", ShredLaneKind::Bool, 0).unwrap(), Elected::At(_)));
        assert_eq!(shred_lane_elected(&p1, 1, "flag", ShredLaneKind::Bool, 0).unwrap(), Elected::OtherKind);
    }
    let rows = list.rows();
    let registry = Arc::new(PartRegistry::new(64 << 20));
    let classes = [StorageClass::VarlenaVerbatim; 4];
    let recipe = GranuleRecipe::for_test_classes(REL_SHRED, &[1, 1, 1, 1], &classes, registry)
        .with_shred_lane(1, "k", ShredLaneKind::NumericFs, 0)
        .with_shred_lane(2, "name", ShredLaneKind::Text, 0)
        .with_shred_lane(3, "flag", ShredLaneKind::Bool, 0);
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    // twice: the second pass reads the parts' elections from the list's faces
    for _ in 0..2 {
        src.seek(&[0, list.granules()], &[0, 0]).unwrap();
        let mut i = 0u64;
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
            let mut w = Window::over(&mut site);
            let n = src.window(&mut w).unwrap();
            if n == 0 {
                break;
            }
            let l = w.lanes();
            for r in 0..n as usize {
                // SAFETY: `r < n`; the lanes' cells are at their widths.
                unsafe {
                    let valid: Vec<bool> = (0..4).map(|k| l[k].is_valid(r)).collect();
                    let doc = mixed_doc(i).is_some();
                    let first = i < MIXED_CUT;
                    assert_eq!(valid, [doc, doc, doc && first, doc && first], "row {i}");
                    if doc {
                        assert_eq!(*(l[1].ptr as *const i64).add(r), (i % 1000) as i64, "row {i} k");
                        let img = byref_bytes(*(l[0].ptr as *const u64).add(r), None);
                        assert_eq!(Some(OracleVal::Bytes(img)), bf.fx.oracle[0][i as usize], "row {i} image");
                    }
                    if doc && first {
                        assert_eq!(byref_bytes(*(l[2].ptr as *const u64).add(r), None), format!("tok{:04}", i % 97).into_bytes(), "row {i} name");
                        assert_eq!(*l[3].ptr.add(r), (i % 2 == 0) as u8, "row {i} flag");
                    }
                }
                i += 1;
            }
        }
        assert_eq!(i, rows);
    }
    src.unbind();
}

#[test]
fn a_shred_code_lane_a_part_did_not_elect_is_all_invalid_and_its_table_has_no_entries() {
    // M6 R3: `name` read as codes (jsonb.codes) over the part that elected it and the part that did not, one scan
    use gravity_hosts::dict::text_hosts::{TableFn, TableSpec};
    let bf = shred_fixture_of(mixed_doc, 7315);
    let list = part_list(&bf);
    let registry = Arc::new(PartRegistry::new(64 << 20));
    let recipe = GranuleRecipe::for_test_classes(REL_SHRED + 1, &[1, 1], &[StorageClass::VarlenaVerbatim; 2], registry)
        .with_shred_code_lane(1, "name")
        .with_tables(vec![TableSpec { over: 1, func: TableFn::Program { elem: gravity_dict::lane::ELEM_BITS, swept: false, fingerprint: 0x5eed_0340 } }]);
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let (mut i, mut empty_tables) = (0u64, 0);
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        let first = w.base().part_id == 0;
        let l = w.lanes();
        assert_eq!((l[1].kind, l[1].region), (lane::KIND_CODES, lane::REGION_DICTIONARY));
        assert_eq!(l[1].ncodes_or_scale == 0, !first, "the unelected part publishes no codes");
        for r in 0..n as usize {
            // SAFETY: `r < n`.
            assert_eq!(unsafe { l[1].is_valid(r) }, first && mixed_doc(i).is_some(), "row {i}");
            i += 1;
        }
        if !first {
            let mut t = Lane::NONE;
            assert_eq!(src.lane(3, &[], &mut t).unwrap(), 1, "the unelected part's table is filled, never swept");
            assert!(gravity_dict::lane::is_table(&t));
            // SAFETY: a published table lane points at its header, whose data holds code 0's word.
            unsafe {
                let h = &*(t.ptr as *const gravity_dict::table::Header);
                assert_eq!((h.ncodes, h.elem, *(h.data as *const u64)), (0, gravity_dict::lane::ELEM_BITS, 0));
            }
            empty_tables += 1;
        }
    }
    assert_eq!(i, list.rows());
    assert!(empty_tables > 0);
    src.unbind();
}

#[test]
fn a_late_jsonb_lane_is_filled_where_its_path_lane_is_invalid_and_the_rest_on_asking() {
    use gravity_program::protos::VERDICT_LATE;
    use gravity_program::protos::VERDICT_ZONE;
    use pgrc_format::shredlane::ShredLaneKind;
    let bf = shred_fixture_of(mixed_doc, 7315);
    let list = part_list(&bf);
    let rows = list.rows();
    let classes = [StorageClass::VarlenaVerbatim; 2];
    let recipe = GranuleRecipe::for_test_classes(REL_SHRED, &[1, 1], &classes, Arc::new(PartRegistry::new(64 << 20))).with_shred_lane(1, "name", ShredLaneKind::Text, 0).with_late_guard(0, 1).with_verdict_cell();
    recipe.verdicts.set(3, 0b1);
    recipe.verdicts.set(0, VERDICT_ZONE | VERDICT_LATE);
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.zone(&[], &[0], &[0]);
    src.bind(&binding_ref(&resolved)).unwrap();
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let (mut i0, mut walked) = (0u64, 0u64);
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap() as usize;
        if n == 0 {
            break;
        }
        let l = w.lanes();
        for r in 0..n {
            let i = i0 + r as u64;
            // SAFETY: `r < n`; lane 0's cells are image pointers, lane 1's the path's text.
            unsafe {
                assert_eq!(l[0].is_valid(r), mixed_doc(i).is_some(), "row {i}: the image's validity is exact before any fill");
                let img = l[0].is_valid(r).then(|| byref_bytes(*(l[0].ptr as *const u64).add(r), None));
                if l[1].is_valid(r) {
                    // jsonops: the waiting cell is the jsonb `{}` (an object container of 0 pairs), over which a chain is NULL
                    assert_eq!(img, Some(vec![0, 0, 0, 0x20]), "row {i}: a valid path row's image waits (the {{}} image)");
                } else if l[0].is_valid(r) {
                    walked += 1;
                    assert_eq!(img.map(OracleVal::Bytes), bf.fx.oracle[0][i as usize], "row {i}: an invalid path row's image is filled");
                }
            }
        }
        let mut out = l[0];
        assert_eq!(src.lane(0, &[], &mut out).unwrap(), 1);
        for r in 0..n {
            let i = i0 + r as u64;
            // SAFETY: as above, after the whole fill.
            unsafe {
                let img = l[0].is_valid(r).then(|| byref_bytes(*(l[0].ptr as *const u64).add(r), None));
                assert_eq!(img.map(OracleVal::Bytes), bf.fx.oracle[0][i as usize], "row {i} after the fill");
            }
        }
        i0 += n as u64;
    }
    assert_eq!(i0, rows);
    assert!(walked >= rows - MIXED_CUT - (rows - MIXED_CUT) / 13 - 1, "the second part's rows are walked: {walked}");
    src.unbind();
}

/// PS1-25: entries over the code lane of the path `name` (the jsonb column its late guard). The part that elected the path
/// publishes each entry of its sealed dictionary once, the column's cell the one-key document {"name": entry}, byte for
/// byte jsonb_in's; the part that did not elect it walks its rows. With a delete vector on the electing part, its rows.
#[test]
fn a_path_entries_window_puts_each_sealed_entry_in_a_one_key_document() {
    // the production lane posture (pgrc_am ingest `codec_candidates`, its json arm): the path seals its dictionary executable
    let dict = DictPolicy { ndv_cap: 1_000_000, exec_ok: true, sem: TextSemantics::BytesOnly };
    let bf = shred_fixture_with(mixed_doc, 7316, &CodecCandidates::new(ColumnPosture::default()).with_lane_default(1, ColumnPosture { dict: Some(dict), ..ColumnPosture::default() }));
    let ctx = mcx::MemoryContext::new("path-entries-test");
    let want: std::collections::BTreeSet<String> = (0..MIXED_CUT).filter(|&i| mixed_doc(i).is_some()).map(|i| format!("tok{:04}", i % 97)).collect();
    for dv in [false, true] {
        let list = Arc::new(PartList::new(bf.manifest.parts.iter().enumerate().map(|(i, rec)| {
            let bytes: Arc<[u8]> = Arc::from(bf.files.get(&part_file_name(rec.part_no)).expect("part file").as_slice());
            let d = (dv && i == 0).then(|| pgrc_format::dml::encode_dv(rec.part_no, 1, &[(0, pgrc_format::dml::DvBlockKind::List, &[3u16][..])]).unwrap());
            let deletes = d.map(|d| Arc::new(pgrc_scan::PartDeletes::from_dv_payload(&d, rec.part_no, 1, rec.granule_count).unwrap()));
            PartSpec { io: PartIoSpec::Mem { bytes, dev: 7, ino: 700 + 10 * dv as u64 + i as u64 }, expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes, part: None }
        }).collect()));
        let recipe = GranuleRecipe::for_test_classes(REL_SHRED + 3 + dv as u32, &[1, 1], &[StorageClass::VarlenaVerbatim; 2], Arc::new(PartRegistry::new(64 << 20))).with_shred_code_lane(1, "name").with_late_guard(0, 1).with_entries(1);
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        let mut src = GranuleRangeSource::new(recipe);
        src.bind(&binding_ref(&resolved)).unwrap();
        src.seek(&[0, list.granules()], &[0, 0]).unwrap();
        let mut lanes = [Lane::NONE; MAX_LANES];
        let (mut got, mut entries, mut rows) = (std::collections::BTreeSet::new(), 0u64, [0u64; 2]);
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
            let mut w = Window::over(&mut site);
            let n = src.window(&mut w).unwrap() as usize;
            if n == 0 {
                break;
            }
            let (l, part) = (w.lanes(), w.base().part_id as usize);
            if l[1].flags & lane::FLAG_ENTRIES == 0 {
                rows[part] += n as u64;
                continue;
            }
            assert!(!dv && part == 0, "only the electing part, with no delete vector, publishes entries");
            for r in 0..n {
                // SAFETY: `r < n`; lane 0's cells are image pointers, lane 1's the codes.
                let (img, code) = unsafe { (byref_bytes(*(l[0].ptr as *const u64).add(r), None), *(l[1].ptr as *const u32).add(r)) };
                assert_eq!(code as u64, entries, "the window's codes are the entries in order");
                let v = String::from_utf8(img[12 + 4..].to_vec()).expect("utf8");
                let doc = format!(r#"{{"name": "{v}"}}"#);
                let pg = adt_jsonb::io::jsonb_in(ctx.mcx(), doc.as_bytes(), None).unwrap_or_else(|e| panic!("jsonb_in: {}", e.message())).expect("jsonb");
                assert_eq!(img, pg[4..].to_vec(), "entry {v}: jsonb_in's image");
                assert!(got.insert(v), "each entry once");
                entries += 1;
            }
        }
        let first = bf.manifest.parts[0].rows;
        if dv {
            assert_eq!((entries, rows), (0, [first, list.rows() - first]), "a part with a delete vector walks its rows");
        } else {
            assert_eq!(got, want, "the electing part's entries are its non-NULL values");
            assert_eq!(rows, [0, list.rows() - first], "the part that did not elect the path walks its rows");
        }
        src.unbind();
    }
}

const REL_MONEY: u32 = 4271;

fn cents(i: u64) -> Option<i64> {
    if i % 101 == 0 {
        None
    } else {
        Some((i as i64 * 7919) % 200_003 - 100_000)
    }
}

/// `numeric(15,2)`'s schema: a verbatim varlena with the packed-numeric semantics of its typmod.
fn money_col(attno: u32) -> ColSchema {
    ColSchema { attno, class: StorageClass::VarlenaVerbatim, typlen: -1, typbyval: false, typalign: b'i', collation_class: CollationClass::C, semantics: TypeSemantics::PackedNumeric { scale: 2 } }
}

/// The canonical numeric image of `c` cents (the payload the AM stores and the packed codec rebuilds).
fn money_payload(c: i64) -> Vec<u8> {
    adt_numeric::fixed_scale_unpack(c, 2).expect("image").payload().to_vec()
}

/// The cents behind a by-reference lane's datum: a 4B-U varlena image.
unsafe fn cents_of(datum: u64) -> i64 {
    let p = datum as usize as *const u8;
    let len = ((p as *const u32).read_unaligned() >> 2) as usize - 4;
    adt_numeric::fixed_scale_fit(adt_numeric::Num::from_payload(core::slice::from_raw_parts(p.add(4), len)), 2).expect("cents")
}

/// An int8 `a = i` beside a `numeric(15,2)` column over two parts: packed (the AM's numeric posture) or stored verbatim.
fn money_fixture(packed: bool) -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    let rows = 35_424u64;
    let relfilenumber = if packed { 7312 } else { 7313 };
    let fx = Fixture {
        name: "gravity_money",
        dir: format!("/qa/t{relfilenumber}"),
        spc: 1663,
        db: 5,
        relfilenumber,
        schema: vec![int8_col(1), money_col(2)],
        oracle: vec![(0..rows).map(|i| Some(OracleVal::Word(i))).collect(), (0..rows).map(|i| cents(i).map(|c| OracleVal::Bytes(money_payload(c)))).collect()],
        plans: vec![],
        policy: PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() },
    };
    let cands = CodecCandidates::new(ColumnPosture::default()).with_column(2, 0, ColumnPosture { numeric: packed, ..ColumnPosture::default() });
    let sources: [&dyn CandidateSource; 2] = [&cands, &ReferenceCandidates];
    let mut vfs = pgrc_qa::simvfs::SimVfs::new();
    vfs.mkdir_path(&fx.dir).expect("mkdir");
    let fxid = 900 + fx.relfilenumber;
    let mut probe = Probe::new(TxnVerdict::Aborted);
    probe.mark(fxid, TxnVerdict::InProgress);
    let mut w = open_writer(&fx, fxid).expect("writer");
    append_rows_with_sources(&mut vfs, &mut w, &fx, 0, rows, &sources).expect("append");
    finish_with_sources(&mut vfs, &mut w, &sources).expect("finish");
    w.publish(&mut vfs, &probe).expect("publish");
    probe.mark(fxid, TxnVerdict::Committed);
    let files = vfs.snapshot_dir(&fx.dir);
    let manifest = effective_manifest(&mut vfs, &fx.dir, &probe, None).expect("effective").expect("committed generation");
    BuiltFixture { fx, files, manifest }
}

/// Every window of `src`: the money lane's kind and each row's cents (None: NULL) through the lane's face, into `out` (its capacity the caller's).
fn money_windows(src: &mut GranuleRangeSource, total: u64, want_kind: u8, out: &mut Vec<Option<i64>>) -> u64 {
    src.seek(&[0, total], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut windows = 0u64;
    out.clear();
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        windows += 1;
        let l = w.lanes()[1];
        assert_eq!((l.kind, l.width, l.region, l.ncodes_or_scale), (want_kind, 8, lane::REGION_ARENA, 2), "the money lane's shape");
        for j in 0..w.end() {
            let i = unsafe { w.index(j) };
            out.push(unsafe {
                if !l.is_valid(i) {
                    None
                } else if l.kind == lane::KIND_FIXED {
                    Some(*(l.ptr as *const i64).add(i))
                } else {
                    Some(cents_of(l.datum(i)))
                }
            });
        }
    }
    windows
}

#[test]
fn a_numeric_column_is_its_mantissa_lane_when_packed_and_a_by_reference_lane_when_stored_verbatim() {
    for packed in [true, false] {
        let bf = money_fixture(packed);
        let list = part_list(&bf);
        let want = if packed { lane::KIND_FIXED } else { lane::KIND_BYREF };
        let recipe = GranuleRecipe::for_test(REL_MONEY, &[1, 2], Arc::new(PartRegistry::new(64 << 20))).with_numeric_lane(1, 2);
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        let mut src = GranuleRangeSource::new(recipe);
        src.bind(&binding_ref(&resolved)).unwrap();
        let mut got = Vec::with_capacity(list.rows() as usize);
        let windows = money_windows(&mut src, list.granules(), want, &mut got);
        assert_eq!(windows, 5, "one window per granule (packed {packed})");
        assert_eq!(got, (0..list.rows()).map(cents).collect::<Vec<_>>(), "the cents through the lane (packed {packed})");
        const PASSES: usize = 5;
        let allocs = tracked(|| {
            for _ in 0..PASSES {
                assert_eq!(money_windows(&mut src, list.granules(), want, &mut got), 5);
            }
        });
        assert_eq!(allocs, 0, "{allocs} heap allocations in {PASSES} passes over money windows (packed {packed})");
        // a predicate on the int lane selects granule 2's survivors: the money lane decodes them alone (`mantissa_sel` / `decode_sel`) and scatters them
        src.unbind();
        let recipe = GranuleRecipe::for_test(REL_MONEY, &[1, 2], Arc::new(PartRegistry::new(64 << 20))).with_numeric_lane(1, 2);
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        let mut sel = GranuleRangeSource::new(recipe);
        let thr = 2 * GRANULE as i64 + 100;
        sel.zone(&[ZonePred::word(0, ZoneOp::Lt, thr)], &[0], &[0]);
        sel.bind(&binding_ref(&resolved)).unwrap();
        let windows = money_windows(&mut sel, list.granules(), want, &mut got);
        assert_eq!((windows, sel.granules_selected()), (3, 1));
        assert_eq!(got, (0..thr as u64).map(cents).collect::<Vec<_>>(), "the selected rows' cents (packed {packed})");
        sel.unbind();
    }
}

/// NUMZONE (V4R-28): a zone predicate on a `numeric(15,2)` lane compares the lane's mantissas (cents) with its constant at the
/// scale: a constant past every granule's max skips them all (the exact PackedNumeric keys, packed or verbatim), and a
/// selective one selects the granules' survivors from the packed mantissas (`row_select`); a verbatim part's lane holds image
/// pointers, so it is never selected on (every row reaches the kernel, which tests the qual), rows and cents the same.
#[test]
fn a_numeric_lane_takes_zone_predicates_on_its_mantissas() {
    for packed in [true, false] {
        let bf = money_fixture(packed);
        let list = part_list(&bf);
        let want = if packed { lane::KIND_FIXED } else { lane::KIND_BYREF };
        let recipe = || GranuleRecipe::for_test(REL_MONEY, &[1, 2], Arc::new(PartRegistry::new(64 << 20))).with_numeric_lane(1, 2);
        let resolved = Resolved::for_recipe(&recipe(), list.clone());
        let mut got = Vec::with_capacity(list.rows() as usize);
        // m > 100_003 cents: above every value, every granule NoneMatch
        let mut src = GranuleRangeSource::new(recipe());
        src.zone(&[ZonePred::word(1, ZoneOp::Gt, 100_003)], &[0], &[0]);
        src.bind(&binding_ref(&resolved)).unwrap();
        assert_eq!(money_windows(&mut src, list.granules(), want, &mut got), 0, "packed {packed}");
        assert_eq!((src.granules_skipped(), src.granules_decoded()), (list.granules(), 0), "packed {packed}");
        src.unbind();
        // m < -99_000: about 0.5 % of the rows; a packed granule selects them, a verbatim one hands every row on
        let mut src = GranuleRangeSource::new(recipe());
        src.zone(&[ZonePred::word(1, ZoneOp::Lt, -99_000)], &[0], &[0]);
        src.bind(&binding_ref(&resolved)).unwrap();
        money_windows(&mut src, list.granules(), want, &mut got);
        let pass: Vec<Option<i64>> = (0..list.rows()).map(cents).filter(|c| c.is_some_and(|c| c < -99_000)).collect();
        assert!(!pass.is_empty());
        if packed {
            assert_eq!(src.granules_selected(), list.granules(), "every granule a selection over the mantissas");
            assert_eq!(got, pass, "the selected rows are the qual's");
        } else {
            assert_eq!(src.granules_selected(), 0, "a verbatim lane is never selected on");
            assert_eq!(got.len() as u64, list.rows());
            assert_eq!(got.iter().copied().filter(|c| c.is_some_and(|c| c < -99_000)).collect::<Vec<_>>(), pass);
        }
        src.unbind();
    }
}

/// perfdiag3 fix 1: a 2-part table (2 × 64 granules) at width 8 — the runner's whole-part law would engage 2
/// workers; the sizer-cut cursor it builds below the width hands out ≥ 8 claims and the rows read equal the
/// serial scan, every granule decoded once.
#[test]
fn two_parts_at_width_eight_split_claims_inside_the_parts() {
    const TWO_ROWS: u64 = 1 << 20;
    let bf = build_fixture(int8_fixture("gravity_two_parts", 7302, TWO_ROWS, vec![], PartCutPolicy { max_rows: TWO_ROWS / 2, ..PartCutPolicy::default() }, value));
    let expected: Vec<Option<i64>> = (0..TWO_ROWS).map(value).collect();
    assert_eq!(serial_scan(&bf), expected);
    let list = part_list(&bf);
    assert_eq!((list.len(), list.granules()), (2, 128), "{} parts, {} granules", list.len(), list.granules());
    let width = 8u16;
    let c0 = gravity_parallel::startup_c0(list.granules(), width, 16, list.max_part_granules());
    let recipe = GranuleRecipe::for_test(REL, &[1, 1], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    let claims = GranuleRange::over_parts(list.granules(), ClaimSize::Adaptive { startup_c0: c0 }, list.starts());
    assert!(!claims.whole_boundary_claims());
    let mut all = Vec::new();
    let mut nclaims = 0;
    while let Some(c) = claims.claim() {
        assert_eq!(list.part_of(c.lo), list.part_of(c.hi - 1), "claim {c:?} spans two parts");
        drain(&mut src, c.lo, c.hi, &mut all);
        nclaims += 1;
    }
    assert!(nclaims >= width, "{nclaims} claims for {width} workers");
    assert_eq!(all, expected, "every row once, in granule order");
    assert_eq!(src.granules_decoded(), 128);
}

/// The tail rule's cursor over a real source: 10 parts of 4 granules, claimed whole up to part 8 and cut in the startup
/// claim's 2 granules from there (`set_tail_from`); the rows read equal the serial scan, every granule decoded once, no claim
/// across a part and no part pin left by a claim (whole and cut claims of one table, in claim order).
#[test]
fn a_tail_cursor_reads_whole_parts_then_cut_tail_parts_once() {
    const TAIL_ROWS: u64 = 10 * 4 * 8192;
    let bf = build_fixture(int8_fixture("gravity_tail_parts", 7303, TAIL_ROWS, vec![], PartCutPolicy { max_rows: TAIL_ROWS / 10, ..PartCutPolicy::default() }, value));
    let expected: Vec<Option<i64>> = (0..TAIL_ROWS).map(value).collect();
    assert_eq!(serial_scan(&bf), expected);
    let list = part_list(&bf);
    assert_eq!((list.len(), list.granules()), (10, 40), "{} parts, {} granules", list.len(), list.granules());
    let recipe = GranuleRecipe::for_test(REL, &[1, 1], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    let mut claims = GranuleRange::over_parts(list.granules(), ClaimSize::Adaptive { startup_c0: 2 }, list.starts());
    claims.set_whole_boundary_claims(true);
    claims.set_tail_from(list.starts()[8]);
    let (mut all, mut sizes) = (Vec::new(), Vec::new());
    while let Some(c) = claims.claim() {
        assert_eq!(list.part_of(c.lo), list.part_of(c.hi - 1), "claim {c:?} spans two parts");
        drain(&mut src, c.lo, c.hi, &mut all);
        sizes.push(c.hi - c.lo);
    }
    assert_eq!(sizes, [4, 4, 4, 4, 4, 4, 4, 4, 2, 2, 2, 2], "8 whole parts, then the 2 tail parts in 2-granule claims");
    assert_eq!(all, expected, "every row once, in granule order");
    assert_eq!(src.granules_decoded(), 40);
}

/// TABLESAMPLE and ctid clauses over pgrc are refused: pgrc's ctid is the row's stable ID (pgrc_format rowid IX0), not its
/// position, so a sampled recipe's seek and a seek with a TID scan's granule
/// cells (four or five keys: lowerv4 pgrc lower.rs `tid_keys`, which `make_with` refuses first) error, and nothing is
/// walked; a method other than BERNOULLI / SYSTEM still refuses at the recipe.
#[test]
fn a_sample_and_tid_keys_over_pgrc_are_refused_at_the_seek() {
    let bf = fixture();
    let list = part_list(&bf);
    let total = list.granules();
    for handler in [3313u32, 3314] {
        let recipe = GranuleRecipe::for_test(REL, &[1], Arc::new(PartRegistry::new(64 << 20))).with_sample(handler, true).unwrap();
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        let mut src = GranuleRangeSource::new(recipe);
        src.bind(&binding_ref(&resolved)).unwrap();
        assert!(src.seek(&[0, total, datum::Datum::from_f32(30.0).as_u64(), 7.5f64.to_bits()], &[0; 4]).is_err(), "handler {handler}: a sample");
        src.unbind();
    }
    assert!(GranuleRecipe::for_test(REL, &[1], Arc::new(PartRegistry::new(1 << 20))).with_sample(4000, false).is_err(), "a method other than BERNOULLI / SYSTEM");
    let recipe = GranuleRecipe::for_test(REL, &[1], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    assert!(src.seek(&[0, total, 3, 5], &[0; 4]).is_err(), "four keys: a TID scan's granule range");
    assert!(src.seek(&[0, total, 0, total, 0], &[0, 0, 0, 0, 1]).is_err(), "five keys: a TID array's");
    src.seek(&[0, total], &[0; 2]).unwrap();
    src.unbind();
}

/// D3115: two sources of one statement over part 0's `PROGRAM` table (`swept`): one table between them, chunks claimed
/// from one sweep, the waiting source answered `CLAIM_WAIT` until the last chunk is swept, each source's own header
/// carrying the table's whole fault count; a source that comes later finds the table filled.
fn swept_sources(n: usize) -> (Vec<GranuleRangeSource>, Resolved, Arc<PartList>, BuiltFixture) {
    use gravity_hosts::dict::text_hosts::{TableFn, TableSpec};
    let bf = shred_fixture_of(mixed_doc, 7315);
    let list = part_list(&bf);
    let registry = Arc::new(PartRegistry::new(64 << 20));
    let recipe = GranuleRecipe::for_test_classes(REL_SHRED + 2, &[1, 1], &[StorageClass::VarlenaVerbatim; 2], registry)
        .with_shred_code_lane(1, "name")
        .with_tables(vec![TableSpec { over: 1, func: TableFn::Program { elem: gravity_dict::lane::ELEM_BITS, swept: true, fingerprint: 0x5eed_3115 } }]);
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let srcs = (0..n).map(|_| GranuleRangeSource::new(recipe.clone())).collect();
    (srcs, resolved, list, bf)
}

fn first_window(src: &mut GranuleRangeSource, resolved: &Resolved, granules: u64, lanes: &mut [Lane; MAX_LANES]) {
    src.bind(&binding_ref(resolved)).unwrap();
    src.seek(&[0, granules], &[0, 0]).unwrap();
    let mut site = blank_site(lanes.as_mut_ptr());
    let mut w = Window::over(&mut site);
    assert!(src.window(&mut w).unwrap() > 0);
    assert_eq!(w.base().part_id, 0);
}

const T: u16 = 3;
const CLAIM: u16 = gravity_abi::site::LANE_CLAIM as u16;
const DONE: u16 = gravity_abi::site::LANE_DONE as u16;
const WAIT: u32 = gravity_abi::site::CLAIM_WAIT as u32;

fn header(t: &Lane) -> &gravity_dict::table::Header {
    // SAFETY: a published table lane points at its source's header, live for the part generation.
    unsafe { &*(t.ptr as *const gravity_dict::table::Header) }
}

#[test]
fn d3115_two_sources_share_one_sweep_of_a_parts_program_table() {
    let (mut srcs, resolved, list, _bf) = swept_sources(3);
    let (mut la, mut lb, mut lc) = ([Lane::NONE; MAX_LANES], [Lane::NONE; MAX_LANES], [Lane::NONE; MAX_LANES]);
    let (a, rest) = srcs.split_first_mut().unwrap();
    let (b, rest) = rest.split_first_mut().unwrap();
    let c = &mut rest[0];
    first_window(a, &resolved, list.granules(), &mut la);
    first_window(b, &resolved, list.granules(), &mut lb);
    let (mut ta, mut tb) = (Lane::NONE, Lane::NONE);
    assert_eq!(a.lane(T, &[], &mut ta).unwrap(), gravity_abi::site::LANE_UNFILLED as u32);
    assert_eq!(b.lane(T, &[], &mut tb).unwrap(), gravity_abi::site::LANE_UNFILLED as u32);
    let n = header(&ta).ncodes;
    assert!(n > 0 && (n as u64) <= gravity_abi::site::SWEEP_CHUNK, "one chunk: {n} codes");
    assert_ne!(ta.ptr, tb.ptr, "each source its own header");
    assert_eq!((header(&ta).data, header(&ta).faults), (header(&tb).data, header(&tb).faults), "one table");
    assert_eq!(a.lane(T | CLAIM, &[], &mut Lane::NONE).unwrap(), 0, "a sweeps the one chunk");
    assert_eq!(b.lane(T | CLAIM, &[], &mut Lane::NONE).unwrap(), WAIT, "b waits for a's chunk");
    // SAFETY: the shared fault bitmap holds `n` bits; a's chunk is codes 0..n.
    unsafe { *(header(&ta).faults as *mut u64) |= 0b10 };
    assert_eq!(a.lane(T | CLAIM, &[], &mut Lane::NONE).unwrap(), n, "a's chunk swept: every chunk is");
    assert_eq!(b.lane(T | CLAIM, &[], &mut Lane::NONE).unwrap(), n);
    // the kernel's done host writes the source's own count before LANE_DONE: b swept nothing, a one fault
    // SAFETY: each source's own header.
    unsafe {
        let hb = tb.ptr as *mut gravity_dict::table::Header;
        (*hb).faults = gravity_abi::word(core::ptr::null());
    }
    assert_eq!(a.lane(T | DONE, &[], &mut Lane::NONE).unwrap(), 1);
    assert_eq!(b.lane(T | DONE, &[], &mut Lane::NONE).unwrap(), 1);
    for t in [&ta, &tb] {
        assert_eq!((header(t).nfaults, header(t).faults.is_null()), (1, false), "the table's one fault, whoever swept it");
    }
    first_window(c, &resolved, list.granules(), &mut lc);
    let mut tc = Lane::NONE;
    assert_eq!(c.lane(T, &[], &mut tc).unwrap(), 1, "a later source finds the table swept");
    assert_eq!((header(&tc).data, header(&tc).nfaults), (header(&ta).data, 1));
    for s in srcs.iter_mut() {
        s.unbind();
    }
}

#[test]
fn d3115_a_chunk_left_by_a_failed_claim_goes_to_the_next_claimer() {
    let (mut srcs, resolved, list, _bf) = swept_sources(2);
    let (mut la, mut lb) = ([Lane::NONE; MAX_LANES], [Lane::NONE; MAX_LANES]);
    let (a, rest) = srcs.split_first_mut().unwrap();
    let b = &mut rest[0];
    first_window(a, &resolved, list.granules(), &mut la);
    first_window(b, &resolved, list.granules(), &mut lb);
    let (mut ta, mut tb) = (Lane::NONE, Lane::NONE);
    assert_eq!(a.lane(T, &[], &mut ta).unwrap(), gravity_abi::site::LANE_UNFILLED as u32);
    assert_eq!(b.lane(T, &[], &mut tb).unwrap(), gravity_abi::site::LANE_UNFILLED as u32);
    let n = header(&tb).ncodes;
    assert_eq!(a.lane(T | CLAIM, &[], &mut Lane::NONE).unwrap(), 0);
    a.release(); // a's claim ended mid-sweep (an error): its chunk is an orphan
    assert_eq!(b.lane(T | CLAIM, &[], &mut Lane::NONE).unwrap(), 0, "b takes a's chunk");
    assert_eq!(b.lane(T | CLAIM, &[], &mut Lane::NONE).unwrap(), n);
    assert_eq!(b.lane(T | DONE, &[], &mut Lane::NONE).unwrap(), 1);
    assert!(header(&tb).faults.is_null(), "no entry faulted: b's header drops the bitmap");
    for s in srcs.iter_mut() {
        s.unbind();
    }
}

/// D3038 (A): a text column over two parts under the dictionary policy (the writer cuts at granule grain: the first part
/// holds rows 0..24,576): the first part's rows hold `tv`'s 23 words (a dictionary part), the second's are distinct, past
/// the policy's cap (a verbatim part that seals the octet-length lane); an empty value every 7th row and NULL every 11th.
fn mixed_text_fixture() -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    let rows = 35_424u64;
    let value = |i: u64| if i % 7 == 0 { Vec::new() } else if i < 24_576 { tv(i) } else { format!("u{i:06}-{}", "x".repeat((i % 13) as usize)).into_bytes() };
    let fx = Fixture {
        name: "gravity_text_mixed",
        dir: "/qa/t7318".to_string(),
        spc: 1663,
        db: 5,
        relfilenumber: 7318,
        schema: vec![text_col(1)],
        oracle: vec![(0..rows).map(|i| (i % 11 != 5).then(|| OracleVal::Bytes(value(i)))).collect()],
        plans: vec![],
        policy: PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() },
    };
    let posture = ColumnPosture { dict: Some(DictPolicy { ndv_cap: 1_000, exec_ok: true, sem: TextSemantics::BytesOnly }), ..ColumnPosture::default() };
    let cands = CodecCandidates::new(ColumnPosture::default()).with_column(1, 0, posture);
    let sources: [&dyn CandidateSource; 2] = [&cands, &ReferenceCandidates];
    let mut vfs = pgrc_qa::simvfs::SimVfs::new();
    vfs.mkdir_path(&fx.dir).expect("mkdir");
    let fxid = 900 + fx.relfilenumber;
    let mut probe = Probe::new(TxnVerdict::Aborted);
    probe.mark(fxid, TxnVerdict::InProgress);
    let mut w = open_writer(&fx, fxid).expect("writer");
    append_rows_with_sources(&mut vfs, &mut w, &fx, 0, rows, &sources).expect("append");
    finish_with_sources(&mut vfs, &mut w, &sources).expect("finish");
    w.publish(&mut vfs, &probe).expect("publish");
    probe.mark(fxid, TxnVerdict::Committed);
    let files = vfs.snapshot_dir(&fx.dir);
    let manifest = effective_manifest(&mut vfs, &fx.dir, &probe, None).expect("effective").expect("committed generation");
    BuiltFixture { fx, files, manifest }
}

/// D3038 (A): a code lane read only through an `octet_length` table (`with_lens_only`, the lowering's `FLAG_SOLE`) over a
/// table that mixes a dictionary part and a verbatim part. With the site's own cells (lens mode) the verbatim part's
/// lane is coded by length from its sealed octet-length lane (`octet.lens_coded`): codes rank the part's distinct
/// lengths, code 0 the empty value's, and the table maps each code to its length; the dictionary part reads its index as
/// before; the claims read none of the verbatim part's text extents. Without lens mode the verbatim part ranks its bytes
/// (its text read). Every row's length agrees with its value both ways, and
/// the empty code is the empty value's.
#[test]
fn a_length_only_code_lane_codes_a_verbatim_part_by_its_sealed_octet_lane() {
    use gravity_dict::I64;
    use gravity_hosts::dict::text_hosts::{TableFn, TableSpec};
    use gravity_hosts::ops::scan::pgrc::granule_source::octet_lane_ord;
    use pgrc_format::part::StreamRole;
    use pgrc_read::FaultTag;
    let bf = mixed_text_fixture();
    assert_eq!(bf.manifest.parts.len(), 2, "two parts");
    for (no, (rec, stored)) in bf.manifest.parts.iter().zip([false, true]).enumerate() {
        let part = open_part_bytes(bf.files.get(&part_file_name(rec.part_no)).expect("part"), 950 + no as u64).expect("open part");
        assert_eq!(octet_lane_ord(&part, 1).unwrap().is_some(), stored, "part {no}: a dictionary part seals no octet lane; the verbatim one does");
    }
    let list = part_list(&bf);
    fires::set(true);
    for lens in [true, false] {
        let registry = Arc::new(PartRegistry::new(64 << 20));
        let recipe = GranuleRecipe::for_test_codes(REL_TEXT + 30, &[1], &[1], Arc::clone(&registry)).with_tables(vec![TableSpec { over: 0, func: TableFn::OctetLength }]);
        let recipe = if lens { recipe.with_lens_only(0) } else { recipe };
        let mut resolved = Resolved::for_recipe(&recipe, list.clone());
        resolved.site_cells(1);
        let mut src = GranuleRangeSource::new(recipe).at_site(0);
        src.bind(&binding_ref(&resolved)).unwrap();
        let _ = fires::drain();
        src.seek(&[0, list.granules()], &[0, 0]).unwrap();
        let mut lanes = [Lane::NONE; MAX_LANES];
        let (mut i, mut empties) = (0u64, 0u64);
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
            let mut w = Window::over(&mut site);
            let n = src.window(&mut w).unwrap();
            if n == 0 {
                break;
            }
            let codes = w.lanes()[0];
            let mut t = Lane::NONE;
            assert_eq!(src.lane(2, &[], &mut t).unwrap(), 1, "lens {lens}: the length table fills");
            let table = unsafe { I64::from_lane(&t) }.expect("octet_length table");
            for r in 0..n as usize {
                match &bf.fx.oracle[0][i as usize] {
                    Some(OracleVal::Bytes(v)) => {
                        assert!(unsafe { codes.is_valid(r) }, "lens {lens}: row {i} is valid");
                        let c = unsafe { *(codes.ptr as *const u32).add(r) };
                        assert_eq!(unsafe { table.get(c) }, v.len() as i64, "lens {lens}: row {i} {:?}", String::from_utf8_lossy(v));
                        empties += v.is_empty() as u64;
                    }
                    None => assert!(!unsafe { codes.is_valid(r) }, "lens {lens}: row {i} is NULL"),
                    Some(other) => panic!("row {i} {other:?}"),
                }
                i += 1;
            }
        }
        assert_eq!(i, list.rows(), "lens {lens}: every row");
        assert!(empties > 0, "the fixture holds empty values");
        let fired = fires::drain();
        assert_eq!(fired.split(',').any(|f| f == "octet.lens_coded"), lens, "lens {lens}: {fired}");
        // the verbatim part (the list's second, ino 501): coded by length, the claims read none of the column's stream
        let len = bf.files.get(&part_file_name(bf.manifest.parts[1].part_no)).expect("part file").len() as u64;
        let part = registry.get((7, 501, len)).expect("the verbatim part is open");
        let text = part.faults().iter().filter(|f| matches!(f.tag, FaultTag::StreamExtent { attno: 1, path_ord: 0, role, .. } if role == StreamRole::Values as u8)).count();
        assert_eq!(text == 0, lens, "lens {lens}: the verbatim part's text extents read: {text}");
        src.unbind();
    }
    fires::set(false);
}

#[test]
fn d3250_a_source_dropped_with_its_sweep_unfinished_writes_nothing_into_freed_memory() {
    let (mut srcs, resolved, list, _bf) = swept_sources(2);
    let (mut la, mut lb) = ([Lane::NONE; MAX_LANES], [Lane::NONE; MAX_LANES]);
    let (a, rest) = srcs.split_first_mut().unwrap();
    let b = &mut rest[0];
    first_window(a, &resolved, list.granules(), &mut la);
    first_window(b, &resolved, list.granules(), &mut lb);
    let (mut ta, mut tb) = (Lane::NONE, Lane::NONE);
    assert_eq!(a.lane(T, &[], &mut ta).unwrap(), gravity_abi::site::LANE_UNFILLED as u32);
    assert_eq!(b.lane(T, &[], &mut tb).unwrap(), gravity_abi::site::LANE_UNFILLED as u32);
    assert_eq!(a.lane(T | CLAIM, &[], &mut Lane::NONE).unwrap(), 0);
    a.release(); // a's claim ended mid-sweep; b never claimed: both still hold their unpublished tables' cells
    for s in srcs.iter_mut() {
        s.unbind();
    }
    assert_eq!(written_after_free(|| drop(srcs)), 0, "each unpublished table's cell is marked failed before the cells free");
}

/// `verbatim_fixture` over `tv_utf8` values (multi-byte in every granule), NULL every 11th row.
fn verbatim_utf8_fixture() -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    let rows = 35_424u64;
    let fx = Fixture {
        name: "gravity_text_verbatim_utf8",
        dir: "/qa/t7317".to_string(),
        spc: 1663,
        db: 5,
        relfilenumber: 7317,
        schema: vec![text_col(1)],
        oracle: vec![(0..rows).map(|i| (i % 11 != 3).then(|| OracleVal::Bytes(tv_utf8(i)))).collect()],
        plans: vec![],
        policy: PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() },
    };
    let cands = CodecCandidates::new(ColumnPosture::default());
    let sources: [&dyn CandidateSource; 2] = [&cands, &ReferenceCandidates];
    let mut vfs = pgrc_qa::simvfs::SimVfs::new();
    vfs.mkdir_path(&fx.dir).expect("mkdir");
    let fxid = 900 + fx.relfilenumber;
    let mut probe = Probe::new(TxnVerdict::Aborted);
    probe.mark(fxid, TxnVerdict::InProgress);
    let mut w = open_writer(&fx, fxid).expect("writer");
    append_rows_with_sources(&mut vfs, &mut w, &fx, 0, rows, &sources).expect("append");
    finish_with_sources(&mut vfs, &mut w, &sources).expect("finish");
    w.publish(&mut vfs, &probe).expect("publish");
    probe.mark(fxid, TxnVerdict::Committed);
    let files = vfs.snapshot_dir(&fx.dir);
    let manifest = effective_manifest(&mut vfs, &fx.dir, &probe, None).expect("effective").expect("committed generation");
    BuiltFixture { fx, files, manifest }
}

/// D3038: the per-row length lanes (`with_octet_lane`, `with_nonempty_lane`, `with_char_lane`) over one text column. A
/// verbatim part seals the column's octet-length lane and the reader takes the words from it; a dictionary part seals
/// none and answers from its index. `length` copies the octet lengths on all-ASCII granules, reads the index on a
/// dictionary part and counts the column's characters otherwise. Every row agrees with its value, NULL where it is.
#[test]
fn the_row_length_lanes_read_the_sealed_octet_lane_or_the_dictionary_index() {
    use gravity_hosts::ops::scan::pgrc::granule_source::octet_lane_ord;
    let fixtures = [
        ("dict", text_fixture(), REL_TEXT + 20, false, "octet.chars_coded"),
        ("dict-utf8", text_fixture_utf8(), REL_TEXT + 21, false, "octet.chars_coded"),
        ("verbatim-ascii", verbatim_text_fixture(true), REL_TEXT + 22, true, "octet.chars_ascii"),
        ("verbatim-utf8", verbatim_utf8_fixture(), REL_TEXT + 23, true, "octet.chars_decoded"),
    ];
    fires::set(true);
    for (name, bf, rel, stored, chars_fire) in fixtures {
        {
            let part = open_part_bytes(bf.files.get(&part_file_name(0)).expect("part"), 950).expect("open part");
            assert_eq!(octet_lane_ord(&part, 1).unwrap().is_some(), stored, "{name}: the part sealed an octet lane");
        }
        let list = part_list(&bf);
        let classes = [StorageClass::VarlenaVerbatim; 4];
        let recipe = GranuleRecipe::for_test_classes(rel, &[1, 1, 1, 1], &classes, Arc::new(PartRegistry::new(64 << 20)))
            .with_octet_lane(1)
            .with_nonempty_lane(2)
            .with_char_lane(3);
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        let mut src = GranuleRangeSource::new(recipe);
        src.bind(&binding_ref(&resolved)).unwrap();
        let _ = fires::drain();
        src.seek(&[0, list.granules()], &[0, 0]).unwrap();
        let mut lanes = [Lane::NONE; MAX_LANES];
        let (mut i, mut empty) = (0u64, 0u64);
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
            let mut w = Window::over(&mut site);
            let n = src.window(&mut w).unwrap();
            if n == 0 {
                break;
            }
            let l = w.lanes();
            for r in 0..n as usize {
                let valid: Vec<bool> = (1..4).map(|k| unsafe { l[k].is_valid(r) }).collect();
                match &bf.fx.oracle[0][i as usize] {
                    Some(OracleVal::Bytes(v)) => {
                        assert_eq!(valid, [true; 3], "{name}: row {i}");
                        let chars = v.iter().filter(|&&b| b & 0xC0 != 0x80).count() as i64;
                        // SAFETY: `r < n`; the byval lanes' cells are 8 bytes wide.
                        let got: Vec<i64> = (1..4).map(|k| unsafe { *(l[k].ptr as *const i64).add(r) }).collect();
                        assert_eq!(got, [v.len() as i64, !v.is_empty() as i64, chars], "{name}: row {i} {:?}", String::from_utf8_lossy(v));
                        empty += v.is_empty() as u64;
                    }
                    None => assert_eq!(valid, [false; 3], "{name}: row {i} is NULL"),
                    Some(other) => panic!("{name}: row {i} {other:?}"),
                }
                i += 1;
            }
        }
        assert_eq!(i, list.rows(), "{name}: every row");
        let fired = fires::drain();
        let fired: Vec<&str> = fired.split(',').collect();
        assert_eq!(fired.contains(&"octet.stored"), stored, "{name}: {fired:?}");
        assert!(fired.contains(&chars_fire), "{name}: {fired:?}");
        if name == "verbatim-ascii" {
            assert!(empty > 0, "{name}: the fixture holds empty values");
        }
        src.unbind();
    }
    fires::set(false);
}
