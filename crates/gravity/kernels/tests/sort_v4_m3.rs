//! M3's host entries (kernels host_abi/v4/m3.rs): `sort_open_res`'s section end seals a worker's run in place when the
//! slot's stream holds no run, and puts it behind the stream's when a run was put (a worker past its share).
#![cfg(feature = "hand-kernels")]
use gravity_kernels_rig as common;
use common::*;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use gravity_abi::breaker::SortRun;
use gravity_abi::{Ctx, MorselRange};
use gravity_kernels::ops::sort::entries::{sort_full_res, sort_full_res_var, sort_last_res, sort_last_res_var, sort_open_res, sort_open_res_top, sort_open_res_var, sort_open_res_var_top, RunShape, RESIDENT_MAX, RUN_KEYLANE, RUN_ORDERED, RUN_SEALED};
use gravity_kernels::host_abi::{small_on, sort_begin, sort_full, sort_full_var, sort_image, sort_next, sort_total, sort_total_at, SortSpec, SMALL_SORT_ROWS, SORT_MODE_MEM, SORT_MODE_MERGE, VAR_TBL, VAR_WIDTH};
use gravity_program::hand::{self as programs, Oids};

const ROW_BYTES: u32 = 40;
const SORT_TBL: [u32; 6] = [ROW_BYTES, 2, 2, 0, 0, 0];

unsafe fn push_row(run: *mut u8, key: i64, tag: u64) {
    let p = (*(run as *mut SortRun)).push() as *mut u64;
    *p = 1;
    *p.add(1) = (key ^ i64::MIN) as u64;
    *p.add(2) = key as u64;
    *p.add(3) = tag;
    *(p.add(4) as *mut u8) = 0;
    *(p.add(4) as *mut u8).add(1) = 0;
}

fn shuffled(n: i64) -> Vec<i64> {
    (0..n).map(|i| (i * 37 + 11) % n).collect()
}

unsafe fn keys(run: *const SortRun) -> Vec<i64> {
    (0..(*run).len as usize).map(|i| *((*run).row(i) as *const u64).add(2) as i64).collect()
}

#[test]
fn the_section_end_seals_the_run_in_place_when_no_run_was_put() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    let ctx = rig.ctx();
    unsafe {
        (*ctx).width = 8;
        let run = sort_open_res(ctx, ROW_BYTES as u64, 2, 0);
        for (i, k) in shuffled(100).into_iter().enumerate() {
            push_row(run, k, i as u64);
        }
        (*ctx).claim = MorselRange::SECTION_END;
        assert!(sort_open_res(ctx, ROW_BYTES as u64, 2, 0).is_null(), "the section end answers 0");
        assert_eq!(rig.host.runs.len(), 0, "no run put at width 8: the run stays in the worker's region");
        let r = run as *const SortRun;
        assert_eq!((*r).pad[0], RUN_SEALED);
        assert_eq!(keys(r), (0..100).collect::<Vec<i64>>(), "sorted in place");
        // the leader's merge leaves such a run in the slot; the emit reads it in memory
        (*ctx).claim = MorselRange::new(0, 0);
        assert_eq!(sort_begin(ctx, SORT_TBL.as_ptr()), SORT_MODE_MEM);
        assert_eq!(*(sort_next(ctx, SORT_TBL.as_ptr(), SORT_MODE_MEM as u64, 99) as *const u64).add(2), 99);
    }
}

#[test]
fn a_worker_past_its_share_puts_and_the_section_end_puts_behind_it() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    let ctx = rig.ctx();
    unsafe {
        (*ctx).width = 8;
        let run = sort_open_res(ctx, ROW_BYTES as u64, 2, 0);
        for (i, k) in shuffled(100).into_iter().enumerate() {
            if i == 50 {
                assert_eq!(sort_full(ctx, run, 2, 0), 0, "a full run put mid-section");
            }
            push_row(run, k, i as u64);
        }
        (*ctx).claim = MorselRange::SECTION_END;
        assert!(sort_open_res(ctx, ROW_BYTES as u64, 2, 0).is_null());
        assert_eq!(rig.host.runs.len(), 2, "the stream holds a run: the last run is put behind it, never sealed");
        assert_eq!((*(run as *const SortRun)).len, 0, "the region cleared by the put");
        assert_ne!((*(run as *const SortRun)).pad[0], RUN_SEALED);
    }
}

#[test]
fn an_empty_run_is_neither_sealed_nor_put() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    let ctx = rig.ctx();
    unsafe {
        (*ctx).width = 8;
        let run = sort_open_res(ctx, ROW_BYTES as u64, 2, 0);
        (*ctx).claim = MorselRange::SECTION_END;
        assert!(sort_open_res(ctx, ROW_BYTES as u64, 2, 0).is_null());
        assert_eq!((rig.host.runs.len(), (*(run as *const SortRun)).len), (0, 0));
        assert_ne!((*(run as *const SortRun)).pad[0], RUN_SEALED, "an empty run gives the merge nothing to read");
    }
}

/// D3259: shares past one region no longer put by themselves: with no run put during the section every worker seals (the
/// leader puts a merged run past a region, exec merge.rs `past_cap`), so a sort's grant no longer decides its merge.
#[test]
fn shares_past_one_region_seal_when_no_run_was_put() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    let ctx = rig.ctx();
    unsafe {
        (*ctx).width = 16;
        (*ctx).budget_bytes = RESIDENT_MAX / 16 + 1;
        let run = sort_open_res(ctx, ROW_BYTES as u64, 2, 0);
        for (i, k) in shuffled(10).into_iter().enumerate() {
            push_row(run, k, i as u64);
        }
        (*ctx).claim = MorselRange::SECTION_END;
        assert!(sort_open_res(ctx, ROW_BYTES as u64, 2, 0).is_null());
        assert_eq!(rig.host.runs.len(), 0, "16 shares past a region's bytes and 10 rows: sealed, nothing put");
        assert_eq!((*(run as *const SortRun)).pad[0], RUN_SEALED);
    }
}

/// D3259: the leader's answer that the section's runs merged pass a region (`SPILL_SEAL_OVER`) puts the run at the
/// section end though no run was put (every worker reads the same answer), and a top's section end does not take it
/// (the answer was read over its runs uncut): it seals its run cut to `n`.
#[test]
fn the_leaders_seal_answer_puts_a_run_and_a_top_seals_by_its_bound() {
    for top in [false, true] {
        let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
        rig.host.seal_over = true;
        let ctx = rig.ctx();
        unsafe {
            (*ctx).width = 16;
            let open = |ctx| if top { sort_open_res_top(ctx, ROW_BYTES as u64, 2, 0, 10, 0) } else { sort_open_res(ctx, ROW_BYTES as u64, 2, 0) };
            let run = open(ctx);
            for (i, k) in shuffled(100).into_iter().enumerate() {
                push_row(run, k, i as u64);
            }
            (*ctx).claim = MorselRange::SECTION_END;
            assert!(open(ctx).is_null(), "the section end answers 0");
            let r = run as *const SortRun;
            if top {
                assert_eq!(rig.host.runs.len(), 0, "a top seals by its bound, whatever the answer");
                assert_eq!((*r).pad[0], RUN_SEALED);
                assert_eq!(keys(r), (0..10).collect::<Vec<i64>>(), "the first 10 rows, in order");
            } else {
                assert_eq!(rig.host.runs.len(), 1, "past a region by the leader's answer: the run put");
                assert_ne!((*r).pad[0], RUN_SEALED);
            }
        }
    }
}

/// D3027 (b): a top's section end (`sort_open_res_top`) cuts its run to its first `n` rows and seals it in place, never
/// put, where 16 shares pass a region; D3259: with no bound (`n` = `i64::MAX`) it seals its whole run too (no share test).
#[test]
fn a_top_run_cut_to_n_seals_where_its_shares_would_put() {
    for (n, sealed) in [(10u64, true), (i64::MAX as u64, false)] {
        let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
        let ctx = rig.ctx();
        unsafe {
            (*ctx).width = 16;
            (*ctx).budget_bytes = RESIDENT_MAX / 16 + 1;
            let run = sort_open_res_top(ctx, ROW_BYTES as u64, 2, 0, n, 0);
            for (i, k) in shuffled(100).into_iter().enumerate() {
                push_row(run, k, i as u64);
            }
            (*ctx).claim = MorselRange::SECTION_END;
            assert!(sort_open_res_top(ctx, ROW_BYTES as u64, 2, 0, n, 0).is_null(), "the section end answers 0");
            let r = run as *const SortRun;
            if sealed {
                assert_eq!(rig.host.runs.len(), 0, "16 runs of 10 rows fit a region: nothing put");
                assert_eq!((*r).pad[0], RUN_SEALED);
                assert_eq!(keys(r), (0..10).collect::<Vec<i64>>(), "the first 10 rows, in order");
            } else {
                assert_eq!(rig.host.runs.len(), 0, "no bound: the whole run sealed, nothing put (D3259)");
                assert_eq!((*r).pad[0], RUN_SEALED);
                assert_eq!(keys(r), (0..100).collect::<Vec<i64>>(), "every row, in order");
            }
        }
    }
}

/// One text key (its image's slot at 16, bytes order), the tag at 24, the record word at 32.
fn vtbl() -> [u32; 12] {
    let spec = SortSpec::pack(&[(Some((16, VAR_WIDTH)), false)]).unwrap().0;
    [ROW_BYTES, 2, 3 | VAR_TBL, 0, spec as u32, (spec >> 32) as u32, 1, 32, 2, 16, 0, 0]
}

unsafe fn push_text(run: *mut u8, x: &[u8], tag: u64) {
    let r = run as *mut SortRun;
    let p = (*r).rows_ptr().add((*r).len as usize * ROW_BYTES as usize) as *mut u64;
    *p.add(4) = 0;
    assert_eq!(sort_image(run, x.as_ptr(), x.len() as u64, 16, 32), 0);
    let mut w = [0u8; 8];
    w[..x.len().min(8)].copy_from_slice(&x[..x.len().min(8)]);
    (*r).push();
    (*p, *p.add(1), *p.add(3)) = (1, u64::from_be_bytes(w), tag);
}

unsafe fn text_at(r: *const SortRun, i: usize) -> Vec<u8> {
    let slot = ((*r).row(i) as *const u8).add(16);
    let (size, off) = ((slot as *const u32).read_unaligned() as usize, (slot.add(4) as *const u32).read_unaligned() as usize);
    core::slice::from_raw_parts(slot.add(off + 4), size - 4).to_vec()
}

#[test]
fn a_var_run_keeps_its_table_copy_and_seals_sorted_with_its_copies() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    let ctx = rig.ctx();
    let tbl = vtbl();
    let words: Vec<Vec<u8>> = ["pear", "apple and more", "", "fig", "apple", "zucchini squash", "kiwi"].iter().map(|s| s.as_bytes().to_vec()).collect();
    unsafe {
        (*ctx).width = 8;
        let run = sort_open_res_var(ctx, tbl.as_ptr());
        assert!(RunShape::of_run(run as *const SortRun).is_some(), "the emptied run took its table copy");
        for (i, w) in words.iter().enumerate() {
            push_text(run, w, i as u64);
        }
        assert_eq!(sort_open_res_var(ctx, tbl.as_ptr()), run, "a later open keeps the rows and the copy");
        (*ctx).claim = MorselRange::SECTION_END;
        assert!(sort_open_res_var(ctx, tbl.as_ptr()).is_null());
        assert_eq!(rig.host.runs.len(), 0);
        let r = run as *const SortRun;
        assert_eq!((*r).pad[0], RUN_SEALED);
        let got: Vec<Vec<u8>> = (0..(*r).len as usize).map(|i| text_at(r, i)).collect();
        let mut want = words.clone();
        want.sort();
        assert_eq!(got, want, "sorted, each copy re-pointed with its row");
        let s = RunShape::of_run(r).expect("the copy survives the seal");
        assert_eq!(s.var_bytes(r), s.var_len((*r).row(0), (*r).len), "the run's copies are its rows' copies");
    }
}

/// D3027 (b): a var top's section end (`sort_open_res_var_top`) cuts to `n` rows, their copies re-laid with them, and
/// seals by `n` where 16 shares would put.
#[test]
fn a_var_top_run_cut_to_n_seals_with_its_copies() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    let ctx = rig.ctx();
    let tbl = vtbl();
    let words: Vec<Vec<u8>> = ["pear", "apple and more", "", "fig", "apple", "zucchini squash", "kiwi"].iter().map(|s| s.as_bytes().to_vec()).collect();
    unsafe {
        (*ctx).width = 16;
        (*ctx).budget_bytes = RESIDENT_MAX / 16 + 1;
        let run = sort_open_res_var_top(ctx, tbl.as_ptr(), 3, 0);
        for (i, w) in words.iter().enumerate() {
            push_text(run, w, i as u64);
        }
        (*ctx).claim = MorselRange::SECTION_END;
        assert!(sort_open_res_var_top(ctx, tbl.as_ptr(), 3, 0).is_null());
        assert_eq!(rig.host.runs.len(), 0, "16 runs of 3 rows fit a region: nothing put");
        let r = run as *const SortRun;
        assert_eq!((*r).pad[0], RUN_SEALED);
        let got: Vec<Vec<u8>> = (0..(*r).len as usize).map(|i| text_at(r, i)).collect();
        let mut want = words.clone();
        want.sort();
        want.truncate(3);
        assert_eq!(got, want, "the first 3 texts in order, each copy with its row");
        let s = RunShape::of_run(r).expect("the copy survives the cut and the seal");
        assert_eq!(s.var_bytes(r), s.var_len((*r).row(0), (*r).len), "the run's copies are its kept rows' copies");
    }
}

#[test]
fn a_var_run_after_a_put_is_put_and_its_next_open_lays_the_copy_again() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    let ctx = rig.ctx();
    let tbl = vtbl();
    unsafe {
        (*ctx).width = 8;
        let run = sort_open_res_var(ctx, tbl.as_ptr());
        push_text(run, b"b", 0);
        push_text(run, b"a", 1);
        assert_eq!(sort_full_var(ctx, run, tbl.as_ptr()), 0);
        assert_eq!(rig.host.runs.len(), 1);
        assert!(RunShape::of_run(run as *const SortRun).is_none(), "the put cleared the copy with the rows");
        assert_eq!(sort_open_res_var(ctx, tbl.as_ptr()), run);
        assert!(RunShape::of_run(run as *const SortRun).is_some(), "the next open lays it again");
        push_text(run, b"c", 2);
        (*ctx).claim = MorselRange::SECTION_END;
        assert!(sort_open_res_var(ctx, tbl.as_ptr()).is_null());
        assert_eq!(rig.host.runs.len(), 2, "a run in the stream: the section end puts");
    }
}

fn xorshift(seed: u64) -> impl FnMut() -> u64 {
    let mut x = seed.wrapping_mul(0x5851_f42d_4c95_7f2d) | 1;
    move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    }
}

/// Rows `[flags0, word0, flags1, word1, tag]` (two word keys) or `[flags0, word0, image0 (8 bytes), tag, 0]` (key 0 with
/// a fixed image): NULLs flagged 0 (first) and 2 (last), key words sharing their high bytes, many ties.
fn rows(n: usize, seed: u64, image: bool) -> Vec<[u64; 5]> {
    let mut r = xorshift(seed);
    (0..n).map(|i| {
        let f = [0, 1, 1, 1, 2][(r() % 5) as usize];
        let w = if f == 1 { 0xabcd_0000_0000_0000 | (r() % 37) << 8 } else { 0 };
        if image { [f, w, (r() % 3).to_be(), i as u64, 0] } else { [f, w, 1 + r() % 2, r() % 5, i as u64] }
    }).collect()
}

/// The row compare's order over `rows`: the key words, then key 0's image bytes (flag 1 only) or key 1's words.
fn by_compare(mut v: Vec<[u64; 5]>, image: bool) -> Vec<[u64; 5]> {
    v.sort_by(|a, b| (a[0], a[1]).cmp(&(b[0], b[1])).then_with(|| if image { if a[0] == 1 { a[2].to_ne_bytes().cmp(&b[2].to_ne_bytes()) } else { core::cmp::Ordering::Equal } } else { (a[2], a[3]).cmp(&(b[2], b[3])) }));
    v
}

#[test]
fn the_keylane_radix_orders_as_the_row_compare_does() {
    for (n, seed, image) in [(0, 1, false), (1, 2, false), (2, 3, true), (997, 4, false), (997, 5, true), (5000, 6, false)] {
        let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
        let ctx = rig.ctx();
        let (nkey, spec) = if image { (2, SortSpec::pack(&[(Some((16, 8)), false)]).unwrap().0) } else { (4, 0) };
        let v = rows(n, seed, image);
        unsafe {
            (*ctx).width = 1;
            let run = sort_open_res(ctx, ROW_BYTES as u64, nkey, spec) as *mut SortRun;
            assert!((*run).cap as usize >= n, "the rig's share holds {n} rows");
            (*run).pad[2] = RUN_KEYLANE;
            for x in v.iter() {
                *((*run).push() as *mut [u64; 5]) = *x;
            }
            assert_eq!(sort_last_res(ctx, run as *mut u8, nkey, spec), 0);
            assert_eq!((rig.host.runs.len(), (*run).pad[0] == RUN_SEALED || n == 0), (0, true), "width 1, nothing put: sealed in place");
            let got: Vec<[u64; 5]> = (0..n).map(|i| *((*run).row(i) as *const [u64; 5])).collect();
            let want = by_compare(v.clone(), image);
            let keys = |x: &[[u64; 5]]| x.iter().map(|r| if image { [r[0], r[1], if r[0] == 1 { r[2] } else { 0 }] } else { [r[0], r[1], r[2] << 32 | r[3]] }).collect::<Vec<_>>();
            assert_eq!(keys(&got), keys(&want), "n {n} image {image}: the radix's order is the compare's");
            let tags = |x: &[[u64; 5]]| { let mut t: Vec<u64> = x.iter().map(|r| if image { r[3] } else { r[4] }).collect(); t.sort(); t };
            assert_eq!(tags(&got), tags(&v), "every row once");
        }
    }
}

#[test]
fn a_run_the_ir_kept_ordered_is_not_sorted_and_a_full_run_is_put_in_order() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    let ctx = rig.ctx();
    unsafe {
        (*ctx).width = 8;
        let run = sort_open_res(ctx, ROW_BYTES as u64, 2, 0) as *mut SortRun;
        (*run).pad[2] = RUN_ORDERED;
        for k in [5i64, 3, 9] {
            push_row(run as *mut u8, k, 0);
        }
        assert_eq!(sort_full_res(ctx, run as *mut u8, 2, 0), 0);
        let put = |rig: &Rig, i: usize| rig.host.runs[i].chunks(ROW_BYTES as usize).map(|r| u64::from_ne_bytes(r[16..24].try_into().unwrap()) as i64).collect::<Vec<i64>>();
        assert_eq!(put(&rig, 0), vec![5, 3, 9], "the IR's ORDERED bit is trusted: the rows stay as they came");
        assert_eq!((*run).len, 0, "the put cleared the run");
        (*run).pad[2] = RUN_KEYLANE;
        for k in [5i64, -3, 9, 0] {
            push_row(run as *mut u8, k, 0);
        }
        assert_eq!(sort_full_res(ctx, run as *mut u8, 2, 0), 0);
        assert_eq!(put(&rig, 1), vec![-3, 0, 5, 9], "the radix over key 0");
    }
}

#[test]
fn a_var_run_puts_in_its_bits_order_as_sort_full_var_does_and_seals_serially() {
    let words: Vec<&[u8]> = vec![b"pear", b"apple and more", b"", b"fig", b"apple", b"zucchini squash", b"apple and less", b"kiwi"];
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    let ctx = rig.ctx();
    let tbl = vtbl();
    unsafe {
        (*ctx).width = 8;
        let run = sort_open_res_var(ctx, tbl.as_ptr());
        let fill = |bits: u8| {
            sort_open_res_var(ctx, tbl.as_ptr());
            (*(run as *mut SortRun)).pad[2] = bits;
            words.iter().enumerate().for_each(|(i, w)| push_text(run, w, i as u64));
        };
        fill(0);
        assert_eq!(sort_full_var(ctx, run, tbl.as_ptr()), 0);
        for bits in [0, RUN_KEYLANE, RUN_ORDERED] {
            fill(bits);
            assert_eq!(sort_full_res_var(ctx, run, tbl.as_ptr()), 0);
        }
        let hr = &rig.host.runs;
        assert_eq!(hr.len(), 4);
        assert_eq!(hr[1], hr[0], "unmarked: put_var's bytes");
        assert_eq!(hr[2], hr[0], "keylane: the radix and its ties by the image give the same bytes");
        assert_ne!(hr[3], hr[0], "ordered: the rows as they came");
        // a serial producer's last run with no run put: sealed in place, sorted with its copies
        let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
        let ctx = rig.ctx();
        (*ctx).width = 1;
        let run = sort_open_res_var(ctx, tbl.as_ptr());
        (*(run as *mut SortRun)).pad[2] = RUN_KEYLANE;
        words.iter().enumerate().for_each(|(i, w)| push_text(run, w, i as u64));
        assert_eq!(sort_last_res_var(ctx, run, tbl.as_ptr()), 0);
        let r = run as *const SortRun;
        assert_eq!(((*r).pad[0], rig.host.runs.len()), (RUN_SEALED, 0));
        let got: Vec<Vec<u8>> = (0..(*r).len as usize).map(|i| text_at(r, i)).collect();
        let mut want: Vec<Vec<u8>> = words.iter().map(|w| w.to_vec()).collect();
        want.sort();
        assert_eq!(got, want);
    }
}

static GROWS: AtomicU64 = AtomicU64::new(0);
static CHUNK: AtomicUsize = AtomicUsize::new(0);

/// `Program.arena_grow` granting a fresh chunk (leaked: the test's life), counted, its start kept in `CHUNK`.
unsafe extern "C" fn grant(ctx: *mut Ctx, bytes: u64) -> i64 {
    GROWS.fetch_add(1, Relaxed);
    let c: &'static mut [u64] = Box::leak(vec![0u64; bytes as usize / 8 + 1].into_boxed_slice());
    (*ctx).arena_cur = c.as_mut_ptr() as *mut u8;
    (*ctx).arena_end = (*ctx).arena_cur.add(c.len() * 8);
    CHUNK.store(c.as_ptr() as usize, Relaxed);
    (c.len() * 8) as i64
}

/// `Program.arena_grow` refusing (no memory left under the grant), counted.
unsafe extern "C" fn refuse(_ctx: *mut Ctx, _bytes: u64) -> i64 {
    GROWS.fetch_add(1, Relaxed);
    0
}

/// D1581 (G4 L1', V4R-517): the heads' does-not-apply witness (guardrails G1-G4). m3_sort.rs `index_by` orders a var run
/// whose key 0 is a by-reference key by heads in the arena; with no arena room (the bump window full and the driver's grow
/// refused) it orders by `index`, the rows' compare. Both put the same bytes; the arena is asked once either way, and a
/// granted chunk gets the heads' bytes back after the order. The heads of a run of at most `SMALL_SORT_ROWS` rows are
/// the small road's (`small_on`): on the stack, the arena never asked; so the run here is past that bound.
#[test]
fn a_var_run_with_no_arena_room_for_its_heads_puts_by_the_row_compare_the_same_bytes() {
    // distinct keys (an unstable sort's ties would compare nothing here), several tied on their 8-byte prefix word
    let base: [&[u8]; 10] = [b"pear", b"apple and more", b"", b"fig", b"apple", b"zucchini squash", b"apple and less", b"kiwi", b"apple and lesser", b"zucchini"];
    let words: Vec<Vec<u8>> = (0..SMALL_SORT_ROWS / base.len() + 1).flat_map(|k| base.iter().map(move |w| [*w, format!("#{k:02}").as_bytes()].concat())).collect();
    assert!(words.len() > SMALL_SORT_ROWS);
    let tbl = vtbl();
    let put = |words: &[Vec<u8>], grow: unsafe extern "C" fn(*mut Ctx, u64) -> i64| -> (Vec<u8>, u64, bool) {
        let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
        rig.prog.arena_grow = Some(grow);
        let ctx = rig.ctx();
        unsafe {
            (*ctx).width = 8;
            let run = sort_open_res_var(ctx, tbl.as_ptr());
            (*(run as *mut SortRun)).pad[2] = 0;
            words.iter().enumerate().for_each(|(i, w)| push_text(run, w, i as u64));
            (*ctx).arena_end = (*ctx).arena_cur; // the bump window full: the heads ask the driver's grow
            let before = GROWS.load(Relaxed);
            assert_eq!(sort_full_res_var(ctx, run, tbl.as_ptr()), 0, "the put succeeds either way");
            assert_eq!(rig.host.runs.len(), 1);
            (rig.host.runs[0].clone(), GROWS.load(Relaxed) - before, (*ctx).arena_cur as usize == CHUNK.load(Relaxed))
        }
    };
    let (heads, asked, given_back) = put(&words, grant);
    assert_eq!((asked, given_back), (1, true), "heads: one chunk asked for, their bytes given back after the order");
    let (rows, asked, _) = put(&words, refuse);
    assert_eq!(asked, 1, "no room: the heads asked and were refused, `index` ordered the rows");
    assert_eq!(rows, heads, "the rows' compare puts the heads' bytes");
    if small_on() {
        let (_, asked, _) = put(&words[..base.len()], refuse);
        assert_eq!(asked, 0, "a run within the small road's bound orders its heads on the stack");
    }
}

/// D1581 (G4 L1', V4R-517): a "C" / "POSIX" key compares its bytes (gravity_fn `sort_compare`), never the collation seam:
/// this binary installs no seam (an uninstalled seam panics, so a compare through it would fail the seal). Heads order the
/// run (key 0 a text key, no keylane); the serial seal keeps it in place, in byte order. Any other collation reaches the
/// seam: kernels d1293_compare_error.
#[test]
fn a_c_or_posix_key_orders_its_bytes_without_the_collation_seam() {
    let words: Vec<&[u8]> = vec![b"pear", b"Apple", b"apple and more", b"", b"_under", b"apple", b"zucchini squash", b"apple and less", b"10", b"9"];
    let mut want: Vec<Vec<u8>> = words.iter().map(|w| w.to_vec()).collect();
    want.sort();
    for coll in [950u32, 951] {
        let mut tbl = vtbl();
        tbl[10] = coll;
        let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
        let ctx = rig.ctx();
        unsafe {
            (*ctx).width = 1;
            let run = sort_open_res_var(ctx, tbl.as_ptr());
            (*(run as *mut SortRun)).pad[2] = 0;
            words.iter().enumerate().for_each(|(i, w)| push_text(run, w, i as u64));
            assert_eq!(sort_last_res_var(ctx, run, tbl.as_ptr()), 0, "collation {coll}: no compare failed");
            let r = run as *const SortRun;
            assert_eq!(((*r).pad[0], rig.host.runs.len()), (RUN_SEALED, 0));
            let got: Vec<Vec<u8>> = (0..(*r).len as usize).map(|i| text_at(r, i)).collect();
            assert_eq!(got, want, "collation {coll}: byte order");
        }
    }
}

/// V4R-136 (D463): `sort_total` is the emit's exact row count: the sealed run's length in memory, the put runs' rows (var
/// runs by their rows, not their bytes) in a merge, 0 for any other mode; `sort_total_at` the same over a consumed region.
#[test]
fn sort_total_counts_the_rows_the_emit_delivers() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    let ctx = rig.ctx();
    unsafe {
        (*ctx).width = 1;
        let run = sort_open_res(ctx, ROW_BYTES as u64, 2, 0);
        shuffled(37).into_iter().for_each(|k| push_row(run, k, 0));
        assert_eq!(sort_last_res(ctx, run, 2, 0), 0);
        assert_eq!(sort_total(ctx, SORT_TBL.as_ptr(), SORT_MODE_MEM as u64), 37, "the sealed run");
        assert_eq!(sort_total(ctx, SORT_TBL.as_ptr(), 99), 0);
        let p = &mut *(*(*ctx).program).partial;
        p.consumed[0] = p.own;
        assert_eq!(sort_total_at(ctx, SORT_TBL.as_ptr(), SORT_MODE_MEM as u64, 0, 3), 37, "the consumed region's run");
    }
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    let (ctx, tbl) = (rig.ctx(), vtbl());
    let words: [&[u8]; 5] = [b"pear", b"a much longer word than eight bytes", b"", b"fig", b"kiwi"];
    unsafe {
        (*ctx).width = 8;
        let run = sort_open_res_var(ctx, tbl.as_ptr());
        words.iter().enumerate().for_each(|(i, w)| push_text(run, w, i as u64));
        assert_eq!(sort_full_var(ctx, run, tbl.as_ptr()), 0);
        sort_open_res_var(ctx, tbl.as_ptr());
        words[..3].iter().enumerate().for_each(|(i, w)| push_text(run, w, 10 + i as u64));
        (*ctx).claim = MorselRange::SECTION_END;
        assert!(sort_open_res_var(ctx, tbl.as_ptr()).is_null());
        assert_eq!(rig.host.runs.len(), 2);
        assert_eq!(sort_total(ctx, tbl.as_ptr(), SORT_MODE_MERGE as u64), 8, "two var runs: 5 + 3 rows");
        assert_eq!(sort_total_at(ctx, tbl.as_ptr(), SORT_MODE_MERGE as u64, 0, 3), 8);
    }
}
