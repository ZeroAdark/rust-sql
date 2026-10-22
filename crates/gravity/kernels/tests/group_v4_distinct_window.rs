//! V4R-26 (a scalar distinct count; host_abi v4 m1_distinct.rs): `agg_distinct_window` takes a window of a byval lane whole into the
//! plain distinct end's table, as the row loop's run carry would (every non-NULL key of the window's rows, through the
//! selection, lands once per run: located in T1 before the bypass, appended as its pair record `[key, 1]` in its KeyHash
//! partition under it), and declines what it cannot read; `distinct_count_fns`'s insert-count owner counts a partition's
//! distinct keys (the key 0 once) and reports a full table as the driver's refold signal.

use std::collections::BTreeSet;

use gravity_abi::breaker::{HashAgg, KeyKind, RecordLayout, SubSplit, HASHAGG_HDR, T1, T1_BYPASS};
use gravity_abi::ctx::{ctx_size, lane, Lane};
use gravity_abi::hash::KeyHash;
use gravity_abi::program::partial;
use gravity_abi::site::Site;
use gravity_abi::{Ctx, Partials, ProgramBlock};
use gravity_kernels::ops::group::entries_distinct::{agg_distinct_block, agg_distinct_window, distinct_count_fns, runs_word, DW_DECLINED};
use gravity_kernels::host_abi::{agg_finish, agg_open, AGG_BYPASS_WORD, AGG_STATE_BYTES};

const PAIR: RecordLayout = RecordLayout::with_sentinel(&[KeyKind::I64], &[], false, None);
const SHARE: u32 = 4 << 20;

/// A bound ctx over one armed share region and one site (bypass_route.rs's rig, with the site the window reads).
struct Bound {
    ctx: Vec<u64>,
    site: Box<Site>,
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
        // SAFETY: a site is plain words; zero is its closed state.
        let mut site: Box<Site> = Box::new(unsafe { core::mem::zeroed() });
        let mut ctx = vec![0u64; ctx_size(0) / 8];
        unsafe {
            (*(ctx.as_mut_ptr() as *mut Ctx)).program = &mut *program;
            (*(ctx.as_mut_ptr() as *mut Ctx)).sites = &mut *site;
        }
        Bound { ctx, site, _program: program, _partials: partials, _own: own }
    }

    fn ctx(&mut self) -> *mut Ctx {
        self.ctx.as_mut_ptr() as *mut Ctx
    }
}

/// One window over `vals` (`width` bytes each, little-endian), NULL where `valid` says so, through `sel` when given.
struct Window<'a> {
    vals: &'a [i64],
    width: u8,
    typlen: u64,
    valid: Option<&'a [bool]>,
    sel: Option<&'a [u32]>,
    bypass: bool,
    kind: u8,
    /// KEYED-24: `agg_distinct_block` once per `(base, mask)` in place of the one window call (empty: the window).
    blocks: &'a [(u64, u64)],
}

/// The window taken over a fresh table: the host's status, the keys the finished table holds (every partition's records,
/// with their partitions) and the records it holds.
fn take(w: &Window<'_>) -> (i64, BTreeSet<i64>, usize) {
    let bytes: Vec<u8> = w.vals.iter().flat_map(|v| v.to_le_bytes()[..w.width as usize].to_vec()).collect();
    let valid: Vec<u64> = match w.valid {
        Some(v) => {
            let mut m = vec![0u64; v.len().div_ceil(64).max(1)];
            v.iter().enumerate().for_each(|(i, &b)| m[i / 64] |= (b as u64) << (i % 64));
            m
        }
        None => Vec::new(),
    };
    let mut lanes = [Lane { ptr: bytes.as_ptr(), valid: if w.valid.is_some() { valid.as_ptr() } else { core::ptr::null() }, width: w.width, kind: w.kind, flags: if w.valid.is_some() { 0 } else { lane::FLAG_ALL_VALID }, ..Lane::NONE }];
    let mut b = Bound::new();
    let rows = w.sel.map_or(w.vals.len(), |s| s.len());
    (b.site.rows, b.site.nlanes, b.site.lanes) = (w.vals.len() as u32, 1, lanes.as_mut_ptr());
    b.site.sel = w.sel.map_or(core::ptr::null(), |s| s.as_ptr());
    let ctx = b.ctx();
    let mut st = [0u64; AGG_STATE_BYTES as usize / 8 + 2];
    unsafe {
        // an estimate of a few groups opens T1 in the table's form (no estimate at all bypasses at the open)
        assert_eq!(agg_open(ctx, st.as_mut_ptr() as *mut u8, &PAIR, 0, if w.bypass { 0 } else { 200 }), 0);
        let t = partial(ctx) as *mut T1;
        if w.bypass {
            (*t).flags |= T1_BYPASS;
        }
        assert_eq!((*t).flags & T1_BYPASS != 0, w.bypass, "the table's form");
        let r = match w.blocks {
            [] => agg_distinct_window(ctx, st.as_mut_ptr() as *mut u8, 0, 0, rows as u64, w.typlen),
            bs => bs.iter().map(|&(base, mask)| agg_distinct_block(ctx, st.as_mut_ptr() as *mut u8, 0, base, mask, w.typlen)).find(|&r| r != 0).unwrap_or(0),
        };
        if w.bypass && r == 0 {
            assert_eq!(st[AGG_BYPASS_WORD / 8], T1_BYPASS as u64, "the state's bypass word follows T1");
        }
        assert_eq!(agg_finish(ctx, st.as_mut_ptr() as *mut u8), 0);
        let (mut keys, mut recs) = (BTreeSet::new(), 0usize);
        let t = &*t;
        for p in 0..t.parts() as u32 {
            for c in t.chain(p) {
                for i in 0..c.recs {
                    let q = (c.ptr as *const u64).add(2 * i);
                    assert_eq!(*q.add(1), 1, "a pair record's count");
                    assert_eq!(KeyHash::word(*q).part(t.pbits as u32), p, "key {} in its KeyHash partition", *q as i64);
                    keys.insert(*q as i64);
                    recs += 1;
                }
            }
        }
        (r, keys, recs)
    }
}

fn distinct(vals: &[i64], valid: Option<&[bool]>, sel: Option<&[u32]>) -> BTreeSet<i64> {
    let rows: Vec<usize> = sel.map_or((0..vals.len()).collect(), |s| s.iter().map(|&r| r as usize).collect());
    rows.into_iter().filter(|&r| valid.is_none_or(|v| v[r])).map(|r| vals[r]).collect()
}

/// Runs of equal keys (lengths 1..9) over 3000 rows, key 0 and negative keys among them: under the bypass each run is one
/// record (the adjacent duplicates dropped, across the host's 1 K chunks too), and every key lands.
#[test]
fn a_bypassed_window_appends_one_pair_record_per_run() {
    let vals: Vec<i64> = (0..3000i64).map(|i| ((i / (1 + i % 9)) % 211) - 50).collect();
    let runs = 1 + vals.windows(2).filter(|w| w[0] != w[1]).count();
    let (r, keys, recs) = take(&Window { vals: &vals, width: 8, typlen: 8, valid: None, sel: None, bypass: true, kind: lane::KIND_BYVAL, blocks: &[] });
    assert_eq!(r, 0);
    assert_eq!(keys, distinct(&vals, None, None));
    assert!(keys.contains(&0), "the key 0 is a key");
    assert_eq!(recs, runs, "one record per run");
}

/// All-equal keys: one record.
#[test]
fn an_all_equal_window_is_one_record() {
    let vals = vec![7i64; 5000];
    let (r, keys, recs) = take(&Window { vals: &vals, width: 8, typlen: 8, valid: None, sel: None, bypass: true, kind: lane::KIND_BYVAL, blocks: &[] });
    assert_eq!((r, recs), (0, 1));
    assert_eq!(keys, BTreeSet::from([7]));
}

/// Before the bypass the keys are located in T1 (the table's form), and still every key lands once.
#[test]
fn a_window_before_the_bypass_locates_its_keys_in_t1() {
    let vals: Vec<i64> = (0..2000i64).map(|i| (i * 7919) % 173).collect();
    let (r, keys, recs) = take(&Window { vals: &vals, width: 8, typlen: 8, valid: None, sel: None, bypass: false, kind: lane::KIND_BYVAL, blocks: &[] });
    assert_eq!(r, 0);
    assert_eq!(keys, distinct(&vals, None, None));
    assert_eq!(recs, keys.len(), "T1 held each key once");
}

/// NULL rows skip (a NULL between two equal keys leaves one run), rows come through the selection, and a 4-byte lane's
/// negative keys sign-extend to their int8 image (a 2-byte lane's too).
#[test]
fn nulls_the_selection_and_narrow_lanes_read_as_the_row_loop_reads_them() {
    let vals: Vec<i64> = (0..1500i64).map(|i| (i / 3) % 97 - 48).collect();
    let valid: Vec<bool> = (0..1500).map(|i| i % 5 != 2).collect();
    let sel: Vec<u32> = (0..1500u32).filter(|i| i % 3 != 1).collect();
    for (width, typlen) in [(8u8, 8u64), (4, 4), (2, 2), (8, 4)] {
        let w = Window { vals: &vals, width, typlen, valid: Some(&valid), sel: Some(&sel), bypass: true, kind: lane::KIND_BYVAL, blocks: &[] };
        let (r, keys, _) = take(&w);
        assert_eq!(r, 0, "width {width}");
        assert_eq!(keys, distinct(&vals, Some(&valid), Some(&sel)), "width {width} typlen {typlen}");
        assert!(keys.iter().any(|&k| k < 0), "negative keys kept their sign");
    }
}

/// NEW-40: the runs word at `st + (arg >> 32)` after one window in the table's form, preset to `pre`: the int8 lane over
/// `vals`, NULL where `valid` says so, through `sel` when given.
fn runs_after(vals: &[i64], valid: Option<&[bool]>, sel: Option<&[u32]>, pre: u64) -> (i64, u64) {
    let mut m = vec![0u64; vals.len().div_ceil(64).max(1)];
    if let Some(v) = valid {
        v.iter().enumerate().for_each(|(i, &b)| m[i / 64] |= (b as u64) << (i % 64));
    }
    let bytes: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
    let mut lanes = [Lane { ptr: bytes.as_ptr(), valid: if valid.is_some() { m.as_ptr() } else { core::ptr::null() }, width: 8, kind: lane::KIND_BYVAL, flags: if valid.is_some() { 0 } else { lane::FLAG_ALL_VALID }, ..Lane::NONE }];
    let mut b = Bound::new();
    (b.site.rows, b.site.nlanes, b.site.lanes) = (vals.len() as u32, 1, lanes.as_mut_ptr());
    b.site.sel = sel.map_or(core::ptr::null(), |s| s.as_ptr());
    let rows = sel.map_or(vals.len(), |s| s.len()) as u64;
    let ctx = b.ctx();
    let mut st = [0u64; AGG_STATE_BYTES as usize / 8 + 4];
    let off = AGG_STATE_BYTES as u64 + 16;
    st[off as usize / 8] = pre;
    unsafe {
        assert_eq!(agg_open(ctx, st.as_mut_ptr() as *mut u8, &PAIR, 0, 200), 0);
        let r = agg_distinct_window(ctx, st.as_mut_ptr() as *mut u8, 0, 0, rows, 8 | off << 32);
        assert_eq!(agg_finish(ctx, st.as_mut_ptr() as *mut u8), 0);
        (r, st[off as usize / 8])
    }
}

/// NEW-40: a taken window writes the runs word, 1 where the keys it kept after dropping adjacent duplicates are at most
/// half its non-NULL rows: runs of 2 and of 3 (across the host's 1 K chunks), runs through the selection and around NULL
/// rows (a NULL between two equal keys leaves one run) hold it; keys with no runs and runs of 1.5 (kept 2 of 3) clear it; a
/// window with no non-NULL row leaves it as it was.
#[test]
fn a_taken_window_writes_whether_its_keys_came_in_runs() {
    assert_eq!((runs_word(1, 2), runs_word(2, 3)), (1, 0), "kept x 2 <= rows");
    let runs_of = |n: i64| -> Vec<i64> { (0..3000i64).map(|i| (i / n * 7919) % 1009).collect() };
    let one_two: Vec<i64> = (0..3000i64).map(|i| (i / 3 * 2 + (i % 3 != 0) as i64) * 7919 % 1009).collect();
    assert_eq!(one_two.windows(2).filter(|w| w[0] != w[1]).count() + 1, 2000, "runs of 1 and 2 by turns: 2000 keys kept of 3000");
    for (what, vals, want) in [("runs of 2", runs_of(2), 1), ("runs of 3", runs_of(3), 1), ("no runs", runs_of(1), 0), ("runs of 1.5", one_two, 0)] {
        assert_eq!(runs_after(&vals, None, None, 7), (0, want), "{what}");
    }
    // through a selection that keeps whole runs of 4 (every other run): runs of 4 survive
    let sel: Vec<u32> = (0..3000u32).filter(|i| i / 4 % 2 == 0).collect();
    assert_eq!(runs_after(&runs_of(4), None, Some(&sel), 7), (0, 1), "runs through the selection");
    // a selection that keeps one row of each run of 4: no runs survive
    let one: Vec<u32> = (0..3000u32).filter(|i| i % 4 == 0).collect();
    assert_eq!(runs_after(&runs_of(4), None, Some(&one), 7), (0, 0), "one row a run through the selection");
    // NULL rows between equal keys: x NULL x is one run
    let vals: Vec<i64> = (0..3000i64).map(|i| (i / 3 * 7919) % 1009).collect();
    let valid: Vec<bool> = (0..3000).map(|i| i % 3 != 1).collect();
    assert_eq!(runs_after(&vals, Some(&valid), None, 7), (0, 1), "a NULL inside a run");
    let none = vec![false; 300];
    assert_eq!(runs_after(&vals[..300], Some(&none), None, 7), (0, 7), "no non-NULL row: the word as it was");
}

/// G4: what the host cannot read, it declines untouched (the rows walk): a code lane, an undecoded lane, a 1-byte lane, a
/// dense table.small (its flag word set).
#[test]
fn a_lane_the_host_cannot_read_declines() {
    let vals = vec![1i64, 2, 3];
    for (kind, width) in [(lane::KIND_CODES, 8u8), (lane::KIND_BYVAL, 1)] {
        let (r, keys, _) = take(&Window { vals: &vals, width, typlen: 8, valid: None, sel: None, bypass: true, kind, blocks: &[] });
        assert_eq!((r, keys.len()), (DW_DECLINED, 0), "kind {kind} width {width}");
    }
    let mut b = Bound::new();
    let mut lanes = [Lane { kind: lane::KIND_BYVAL, width: 8, ..Lane::NONE }];
    (b.site.rows, b.site.nlanes, b.site.lanes) = (3, 1, lanes.as_mut_ptr());
    let ctx = b.ctx();
    let mut st = [0u64; AGG_STATE_BYTES as usize / 8 + 4];
    unsafe {
        assert_eq!(agg_open(ctx, st.as_mut_ptr() as *mut u8, &PAIR, 0, 0), 0);
        assert_eq!(agg_distinct_window(ctx, st.as_mut_ptr() as *mut u8, 0, 0, 3, 8), DW_DECLINED, "an undecoded lane");
        let dense = AGG_STATE_BYTES as u64 + 16;
        st[dense as usize / 8] = 1;
        assert_eq!(agg_distinct_window(ctx, st.as_mut_ptr() as *mut u8, 0, 0, 3, 8 | dense << 16), DW_DECLINED, "a dense table.small");
        // KEYED-24: the block entry declines a dense table.small the same way (its rows walk)
        assert_eq!(agg_distinct_block(ctx, st.as_mut_ptr() as *mut u8, 0, 0, 0b101, 8 | dense << 16), DW_DECLINED, "a dense table.small's block");
    }
}

/// KEYED-24: `agg_distinct_block` takes one block's passing rows (the mask's positions, through the selection) as the
/// window host takes a window's, before the bypass and under it, NULLs skipped and narrow lanes sign-extended; an empty
/// mask adds nothing, and a lane it cannot read is declined (`DW_DECLINED`: the caller walks that block's rows).
#[test]
fn a_block_takes_the_rows_its_mask_passes() {
    let vals: Vec<i64> = (0..1500i64).map(|i| (i * 7919) % 389 - 190).collect();
    let valid: Vec<bool> = (0..1500).map(|i| i % 7 != 3).collect();
    let sel: Vec<u32> = (0..1500u32).filter(|i| i % 4 != 2).collect();
    let blocks: Vec<(u64, u64)> = (0..sel.len() as u64).step_by(64).map(|b| {
        let n = (sel.len() as u64 - b).min(64);
        let mask = (b.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5555_5555_5555_5555) & if n == 64 { u64::MAX } else { (1 << n) - 1 };
        (b, mask)
    }).chain([(0, 0)]).collect();
    let at = |p: u64| sel[p as usize] as usize;
    let want: BTreeSet<i64> = blocks.iter().flat_map(|&(b, m)| (0..64u64).filter(move |i| m >> i & 1 == 1).map(move |i| b + i)).map(at)
        .filter(|&r| valid[r]).map(|r| vals[r]).collect();
    for (width, typlen, bypass) in [(8u8, 8u64, true), (8, 8, false), (4, 4, true), (2, 2, true)] {
        let w = Window { vals: &vals, width, typlen, valid: Some(&valid), sel: Some(&sel), bypass, kind: lane::KIND_BYVAL, blocks: &blocks };
        let (r, keys, _) = take(&w);
        assert_eq!(r, 0, "width {width}");
        assert_eq!(keys, want, "width {width} bypass {bypass}");
    }
    let (r, keys, _) = take(&Window { vals: &vals, width: 1, typlen: 8, valid: None, sel: None, bypass: true, kind: lane::KIND_BYVAL, blocks: &[(0, 7)] });
    assert_eq!((r, keys.len()), (DW_DECLINED, 0));
}

/// The insert-count owner: a partition's pair records (repeats, the key 0 twice) counted once each into the table's `len`,
/// which the reduce sums into the owner's total; a table past 7/8 of its words refuses (`-folded`), the driver's refold.
#[test]
fn the_insert_count_owner_counts_a_partitions_distinct_keys() {
    let fns = distinct_count_fns();
    let fold = fns.fold.expect("the owner's fold");
    let keys: Vec<u64> = (0..3000u64).map(|i| (i * 2654435761) % 1100).chain([0, 0, 5, 5]).collect();
    let recs: Vec<u64> = keys.iter().flat_map(|&k| [k, 1]).collect();
    let want = keys.iter().collect::<BTreeSet<_>>().len() as u32;
    let cap = 2048u32;
    let mut region = vec![0u64; (HASHAGG_HDR + cap as usize * 16) / 8 + 1];
    unsafe {
        let t = HashAgg::at(region.as_mut_ptr() as *mut u8, &PAIR, cap, (region.len() * 8) as u32) as *mut HashAgg;
        let n = fold(fns.params.as_ptr(), t, recs.as_ptr() as *const u8, keys.len(), SubSplit::NONE, core::ptr::null_mut());
        assert_eq!(n, keys.len() as i64);
        assert_eq!((*t).len, want, "each key once, 0 among them");
        let mut owner = vec![0u64; 64];
        assert!((fns.open)(fns.params.as_ptr(), owner.as_mut_ptr() as *mut u8, 512) > 0);
        assert_eq!((fns.reduce)(fns.params.as_ptr(), owner.as_mut_ptr() as *mut u8, t, core::ptr::null_mut(), core::ptr::null(), 0), 0);
        assert_eq!(gravity_abi::breaker::OwnerTop::view(owner.as_ptr() as *const u8).total, want as u64);
        let mut region = vec![0u64; (HASHAGG_HDR + 256 * 16) / 8 + 1];
        let small = HashAgg::at(region.as_mut_ptr() as *mut u8, &PAIR, 256, (region.len() * 8) as u32) as *mut HashAgg;
        let n = fold(fns.params.as_ptr(), small, recs.as_ptr() as *const u8, keys.len(), SubSplit::NONE, core::ptr::null_mut());
        assert!(n < 0 && (*small).len as usize == 512 - 512 / 8, "a full table refuses ({n}, len {})", (*small).len);
    }
}

/// D1029 (hardening, main 07:10Z): a cap-2 table (4 words; 7/8 of them rounds to all 4) refuses at 3 keys and keeps a word
/// empty, so each probe ends; before, a fifth distinct key probed 4 full words forever.
#[test]
fn a_cap_2_table_refuses_before_its_last_word() {
    let fns = distinct_count_fns();
    let fold = fns.fold.expect("the owner's fold");
    let recs: Vec<u64> = (1..=8u64).flat_map(|k| [k, 1]).collect();
    let mut region = vec![0u64; (HASHAGG_HDR + 2 * 16) / 8 + 1];
    unsafe {
        let t = HashAgg::at(region.as_mut_ptr() as *mut u8, &PAIR, 2, (region.len() * 8) as u32) as *mut HashAgg;
        let n = fold(fns.params.as_ptr(), t, recs.as_ptr() as *const u8, 8, SubSplit::NONE, core::ptr::null_mut());
        assert!(n < 0 && (*t).len == 3, "a full cap-2 table refuses with a word empty ({n}, len {})", (*t).len);
    }
}
