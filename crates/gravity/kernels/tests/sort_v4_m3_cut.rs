//! M3-TOPS' cut (kernels host_abi/v4/m3_cut.rs): `sort_cut_res` / `sort_cut_res_var` order a live run by its `pad[2]`
//! bits, keep its first n rows (WITH TIES: the rows equal to row n-1 too), re-lay the kept var copies under the table copy,
//! and hand back row n-1's key-0 pair as the bound; the run stays live for the producer's next rows.
#![cfg(feature = "hand-kernels")]
use gravity_kernels_rig as common;
use common::*;
use gravity_abi::breaker::SortRun;
use gravity_abi::status::CANCELLED;
use gravity_kernels::ops::sort::entries::{sort_cut_res, sort_cut_res_var, sort_full_res, sort_open_res, sort_open_res_var, RunShape, CUT_NATURAL, CUT_TIES, RUN_CUT, RUN_KEYLANE, RUN_ORDERED};
use gravity_kernels::host_abi::{sort_image, SortSpec, VAR_TBL, VAR_WIDTH};
use gravity_program::hand::{self as programs, Oids};

const ROW_BYTES: u32 = 40;

unsafe fn push_row(run: *mut u8, key: i64, tag: u64) {
    let p = (*(run as *mut SortRun)).push() as *mut u64;
    (*p, *p.add(1), *p.add(2), *p.add(3), *p.add(4)) = (1, (key ^ i64::MIN) as u64, key as u64, tag, 0);
}

unsafe fn keys(run: *const SortRun) -> Vec<i64> {
    (0..(*run).len as usize).map(|i| *((*run).row(i) as *const u64).add(2) as i64).collect()
}

/// Keys 0,0,1,1,..,9,9 pushed shuffled into an emptied run with `bits` in `pad[2]`.
unsafe fn pairs(rig: &mut Rig, bits: u8) -> *mut u8 {
    let run = sort_open_res(rig.ctx(), ROW_BYTES as u64, 2, 0);
    (*(run as *mut SortRun)).pad[2] = bits;
    for i in 0..20i64 {
        push_row(run, (i * 7 + 3) % 20 / 2, i as u64);
    }
    run
}

#[test]
fn a_cut_keeps_the_first_n_rows_in_key_order_and_hands_back_the_bound() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    for bits in [0, RUN_KEYLANE] {
        unsafe {
            let (ctx, run) = (rig.ctx(), pairs(&mut rig, bits));
            let mut out = [7u64; 2];
            assert_eq!(sort_cut_res(ctx, run, 2, 0, 5, 0, out.as_mut_ptr()), 5);
            assert_eq!(keys(run as *const SortRun), [0, 0, 1, 1, 2]);
            assert_eq!(out, [1, (2i64 ^ i64::MIN) as u64], "row 4's key-0 pair");
            assert_eq!((*(run as *const SortRun)).pad[2] & RUN_ORDERED, 0);
            for k in [9, -1] {
                push_row(run, k, 99);
            }
            assert_eq!(sort_cut_res(ctx, run, 2, 0, 4, CUT_TIES | CUT_NATURAL, out.as_mut_ptr()), 5, "row 4 ties row 3");
            assert_eq!(keys(run as *const SortRun), [-1, 0, 0, 1, 1]);
            assert_eq!(out, [1, (1i64 ^ i64::MIN) as u64]);
            assert_ne!((*(run as *const SortRun)).pad[2] & RUN_ORDERED, 0, "NATURAL: the cut run is ordered");
            out = [7, 7];
            assert_eq!(sort_cut_res(ctx, run, 2, 0, 10, 0, out.as_mut_ptr()), 5, "fewer rows than n: all kept");
            assert_eq!(out, [7, 7], "no bound under n rows");
            assert_eq!(sort_cut_res(ctx, run, 2, 0, 0, CUT_TIES, out.as_mut_ptr()), 0);
        }
    }
}

/// D538 (Q24-B): a compared cut selects row n-1 and sorts only the rows it keeps: every n, WITH TIES or not, keeps the
/// same rows in the same order as a full sort would (the ties past row n-1 gathered wherever the select left them).
#[test]
fn a_compared_cut_selects_then_sorts_the_kept_rows() {
    let all: Vec<i64> = (0..20i64).map(|i| i / 2).collect(); // alloc: Cold
    for n in 1..=21u64 {
        for ties in [0, CUT_TIES as u64] {
            // a fresh rig per cut: the slot's run keeps the last cut's rows across opens
            let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
            unsafe {
                let (ctx, run) = (rig.ctx(), pairs(&mut rig, 0));
                let mut out = [0u64; 2];
                let kept = sort_cut_res(ctx, run, 2, 0, n, ties, out.as_mut_ptr()) as usize;
                let want = if ties != 0 && (n as usize) < all.len() { all.iter().filter(|&&k| k <= all[n as usize - 1]).count() } else { (n as usize).min(all.len()) };
                assert_eq!(kept, want, "n {n} ties {ties}");
                assert_eq!(keys(run as *const SortRun), all[..want], "n {n} ties {ties}");
            }
        }
    }
}

/// D508: a cut that keeps `n` rows marks the run `RUN_CUT` (row n-1 in place, the row walk's bound); a cut under `n`, a put
/// and an emptied run's open clear it, so a run refilled past `n` with no cut names no bound.
#[test]
fn run_cut_marks_a_cut_run_and_a_put_an_open_or_a_short_cut_clears_it() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    unsafe {
        (*rig.ctx()).width = 8;
        let (ctx, run) = (rig.ctx(), pairs(&mut rig, 0));
        let (r, mut out) = (run as *mut SortRun, [0u64; 2]);
        let mut cut = |run: *mut u8| sort_cut_res(ctx, run, 2, 0, 5, 0, out.as_mut_ptr());
        assert_eq!(cut(run), 5);
        assert_ne!((*r).pad[2] & RUN_CUT, 0, "n rows kept: marked");
        assert_eq!(sort_full_res(ctx, run, 2, 0), 0);
        assert_eq!(((*r).len, (*r).pad[2] & RUN_CUT), (0, 0), "the put clears the run and its mark");
        for k in 0..8 {
            push_row(run, 20 - k, 0);
        }
        assert_eq!((*r).pad[2] & RUN_CUT, 0, "refilled past n with no cut: no bound");
        assert_eq!(cut(run), 5);
        (*r).clear();
        assert_eq!(sort_open_res(ctx, ROW_BYTES as u64, 2, 0), run);
        assert_eq!((*r).pad[2] & RUN_CUT, 0, "an emptied run's open clears the mark");
        (*r).pad[2] |= RUN_CUT;
        for k in 0..3 {
            push_row(run, k, 0);
        }
        assert_eq!((cut(run), (*r).pad[2] & RUN_CUT), (3, 0), "a cut under n rows: no bound");
    }
}

/// One text key (its image's slot at 16, bytes order), the tag at 24, the record word at 32 (tests/v4_m3.rs's table).
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

unsafe fn texts(r: *const SortRun) -> Vec<String> {
    (0..(*r).len as usize)
        .map(|i| {
            let slot = ((*r).row(i) as *const u8).add(16);
            let (size, off) = ((slot as *const u32).read_unaligned() as usize, (slot.add(4) as *const u32).read_unaligned() as usize);
            String::from_utf8(core::slice::from_raw_parts(slot.add(off + 4), size - 4).to_vec()).unwrap()
        })
        .collect()
}

#[test]
fn a_var_cut_re_lays_the_kept_copies_and_the_run_takes_more_rows() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    let (ctx, tbl) = (rig.ctx(), vtbl());
    let words = ["pear", "apple and more", "zucchini squash", "fig", "apple and more", "kiwi", "a much longer word than eight"];
    unsafe {
        let run = sort_open_res_var(ctx, tbl.as_ptr());
        let r = run as *const SortRun;
        for (i, w) in words.iter().enumerate() {
            push_text(run, w.as_bytes(), i as u64);
        }
        let top = (*r).var_top();
        let mut out = [0u64; 2];
        assert_eq!(sort_cut_res_var(ctx, run, tbl.as_ptr(), 2, CUT_TIES, out.as_mut_ptr()), 3);
        assert_eq!(texts(r), ["a much longer word than eight", "apple and more", "apple and more"]);
        assert_eq!(out, [1, u64::from_be_bytes(*b"apple an")]);
        let s = RunShape::of_run(r).expect("the table copy stays");
        assert_eq!(s.var_bytes(r), s.var_len((*r).row(0), 3), "only the kept rows' copies stay");
        assert!((*r).var_top() > top, "the dropped copies' room is back");
        for (i, w) in ["banana", "aardvark"].iter().enumerate() {
            push_text(run, w.as_bytes(), 10 + i as u64);
        }
        assert_eq!(sort_cut_res_var(ctx, run, tbl.as_ptr(), 3, 0, out.as_mut_ptr()), 3);
        assert_eq!(texts(r), ["a much longer word than eight", "aardvark", "apple and more"]);
        assert_eq!(s.var_bytes(r), s.var_len((*r).row(0), 3));
    }
}

/// D1028: a cut whose `n` is past the run's rows keeps them all and frees no var room (runs/topk_rows.rs then puts the
/// run instead of retrying); with the abort byte set either cut returns `CANCELLED` and leaves the run as it lay.
#[test]
fn a_cut_past_the_rows_frees_no_room_and_a_set_abort_byte_cancels_either_cut() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    let (ctx, tbl) = (rig.ctx(), vtbl());
    unsafe {
        let run = sort_open_res_var(ctx, tbl.as_ptr());
        let r = run as *const SortRun;
        for (i, w) in ["pear", "fig", "a much longer word than eight"].iter().enumerate() {
            push_text(run, w.as_bytes(), i as u64);
        }
        let (mut out, top) = ([0u64; 2], (*r).var_top());
        assert_eq!(sort_cut_res_var(ctx, run, tbl.as_ptr(), 300_025, 0, out.as_mut_ptr()), 3, "n past the rows: all kept");
        assert_eq!((*r).var_top(), top, "no room freed");
        let lay = texts(r);
        *rig.interrupt = 1;
        assert_eq!(sort_cut_res_var(ctx, run, tbl.as_ptr(), 1, 0, out.as_mut_ptr()), CANCELLED);
        assert_eq!(((*r).len, texts(r)), (3, lay), "a cancelled cut leaves the run");
        *rig.interrupt = 0;
        assert_eq!(sort_cut_res_var(ctx, run, tbl.as_ptr(), 1, 0, out.as_mut_ptr()), 1);
    }
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    unsafe {
        let (ctx, run) = (rig.ctx(), pairs(&mut rig, 0));
        let (lay, mut out) = (keys(run as *const SortRun), [0u64; 2]);
        *rig.interrupt = 1;
        assert_eq!(sort_cut_res(ctx, run, 2, 0, 5, 0, out.as_mut_ptr()), CANCELLED);
        assert_eq!(keys(run as *const SortRun), lay, "a cancelled cut leaves the run");
    }
}
