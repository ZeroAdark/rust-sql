//! M3-TOPS' cut (runs.topk_rows, bound_first; cards/M3.md L3): a sort run ordered in place by its `pad[2]` bits (as
//! `sort_full_res` orders it), truncated to its first `n` rows (WITH TIES: and the rows after that equal row n-1 on the
//! whole key), its var copies re-laid under its table copy, the run kept a live run the producer pushes on. A #[path]
//! submodule of host_abi/v4/m3.rs (owner M3-HOSTS; M3-TOPS' fetch-hydrate hosts join it).
use core::cmp::Ordering;

use crate::host_abi::CMP_ERR;
use super::polled::{sort_by, Poll, Stopped, SORT_RUN};

/// `flags` bit 0: WITH TIES, the rows after row n-1 that equal it on every key word stay.
pub const CUT_TIES: u64 = 1;
/// `flags` bit 1: the cut run keeps `RUN_ORDERED` (road `sort.natural`: the producer's next rows extend a sorted run).
pub const CUT_NATURAL: u64 = 2;
/// spicmp (USEROP): `sort_cut_res_var`'s own bit, never the caller's: a key's comparator is the user's (`RowCmp::exact`)
/// under WITH TIES (no bounded heap), so the cut orders the whole run by PostgreSQL's qsort (`runs::index`'s exact road),
/// as PostgreSQL sorts a WITH TIES input in full, never by std's select / sort, which may panic on a raising comparator.
pub(super) const CUT_EXACT: u64 = 1 << 63;
/// TOTAL-ORDER (user ruling 10-04): a key ordered through fmgr (`RowCmp::order`'s `Lenient`) may answer an order that is not
/// total; the cut orders the run by PostgreSQL's qsort (`runs::index`'s lenient road), never by std's select / sort, which
/// panic on such an order (any order is then valid, a panic never).
pub(super) const CUT_LENIENT: u64 = 1 << 62;

/// D538 (the cut half): the cut's index without a full sort: row `n - 1` selected, the rows before it (and, WITH TIES, every row
/// equal to it) sorted, the rest left in any order past them (the cut drops them). D4140: the sort is a polled step on the
/// kernels' one poll (`polled.rs`; one large run's cut was a second with no cancel poll); `Err` when the statement's abort
/// is pending, the run untouched.
pub(super) fn first_n(poll: &mut Poll<'_>, idx: &mut [u32], n: usize, ties: bool, scratch: &mut [u32], row: &dyn Fn(u32) -> *const u8, cmp: &dyn Fn(*const u8, *const u8) -> Ordering) -> Result<(), Stopped> {
    // D4373 (b): `idx` (the run's rows `0..len`) and `scratch` (as many, used past `SORT_RUN` kept rows) are the caller's
    // blocks of the grant
    let len = idx.len();
    poll.tick()?;
    idx.select_nth_unstable_by(n - 1, |&a, &b| cmp(row(a), row(b)));
    let mut keep = n;
    if ties {
        let pivot = row(idx[n - 1]);
        for j in n..len {
            poll.batch(j)?;
            if cmp(row(idx[j]), pivot).is_eq() {
                idx.swap(keep, j);
                keep += 1;
            }
        }
    }
    let scratch = if keep > SORT_RUN { &mut scratch[..keep] } else { &mut scratch[..0] };
    sort_by(poll, &mut idx[..keep], scratch, |&a, &b| cmp(row(a), row(b)))?;
    Ok(())
}

/// USEROP K7 (`KFLAG_USER`, `RowCmp::exact`): PostgreSQL's bounded heapsort for a top-N whose key's comparator may be the
/// user's, so the comparator meets the pairs tuplesort's does, in its order, the first raise being its (K7: 'met 9 and 1'
/// at the third row of a LIMIT 1, before a later row's raise). tuplesort keeps every row until it holds `2n + 1`
/// (`puttuple_common`'s switch), then `make_bounded_heap` (the direction reversed: the largest row on top) and, per later
/// row, a compare with the top (`<= 0` discards it, else `tuplesort_heap_replace_top`); `sort_bounded_heap` orders the
/// heap at the end. The producer (lowerv4 ops/sort/topk_rows.rs) calls raw.rs `SortHeapStep` after each stored row; the rows
/// stay where they were stored and the heap is their indexes (`HEAPS`, keyed by the run, valid while `RUN_HEAP` holds and
/// the run grew by exactly the one row since), so a step is `O(log n)` compares and no move; the soft cap's cut
/// (raw.rs `heap_cut`) re-lays the run as the heap's rows in heap order. The seal (runs.rs `index`) takes raw.rs `heap_order`.
///
/// tuplesort's heap over row indexes under `rc` (the reversed compare), stopping at the first compare that raised.
pub(super) struct PgHeap<'a> {
    pub(super) rc: &'a dyn Fn(u32, u32) -> Ordering,
}

impl PgHeap<'_> {
    pub(super) fn failed() -> bool {
        CMP_ERR.with(|c| c.get())
    }

    /// `tuplesort_heap_insert`: sift-up (Knuth 5.2.3 exercise 16).
    pub(super) fn insert(&self, h: &mut Vec<u32>, t: u32) {
        let mut j = h.len();
        h.push(t);
        while j > 0 {
            let i = (j - 1) >> 1;
            if (self.rc)(t, h[i]).is_ge() || Self::failed() {
                break;
            }
            h[j] = h[i];
            j = i;
        }
        h[j] = t;
    }

    /// `tuplesort_heap_replace_top` over `h[..n]` (Knuth 5.2.3H, steps H3-H8).
    pub(super) fn replace_top(&self, h: &mut [u32], n: usize, t: u32) {
        let mut i = 0;
        loop {
            let mut j = 2 * i + 1;
            if j >= n {
                break;
            }
            if j + 1 < n && (self.rc)(h[j], h[j + 1]).is_gt() {
                j += 1;
            }
            if Self::failed() || (self.rc)(t, h[j]).is_le() || Self::failed() {
                break;
            }
            h[i] = h[j];
            i = j;
        }
        h[i] = t;
    }

    /// One row in `TSS_BOUNDED` (`puttuple_common`, `make_bounded_heap`'s full-heap arm): a compare with the top.
    pub(super) fn offer(&self, h: &mut Vec<u32>, n: usize, t: u32) {
        if h.len() < n {
            self.insert(h, t);
        } else if !(self.rc)(t, h[0]).is_le() && !Self::failed() {
            self.replace_top(h, n, t);
        }
    }

    /// `make_bounded_heap` over rows `0..len` in arrival order.
    pub(super) fn make(&self, len: usize, n: usize) -> Vec<u32> {
        let mut h = Vec::with_capacity(n); // alloc: Exec
        for t in 0..len as u32 {
            self.offer(&mut h, n, t);
            if Self::failed() {
                break;
            }
        }
        h
    }

    /// `sort_bounded_heap`: the heap's rows in key order (each delete-top's row to the freed slot at the end).
    pub(super) fn sort(&self, mut h: Vec<u32>) -> Vec<u32> {
        let mut n = h.len();
        while n > 1 && !Self::failed() {
            let top = h[0];
            n -= 1;
            let t = h[n];
            self.replace_top(&mut h, n, t);
            h[n] = top;
        }
        h
    }
}

#[cfg(test)]
mod heap_tests {
    use super::*;

    /// USEROP K7: a LIMIT 1 top (n = 1) holds three rows, then `make_bounded_heap`: row 0 goes in with no compare, row 1
    /// meets the top as (row 1, row 0) (K7: 'met 9 and 1'); with no raise, the heap keeps the least rows and its sort
    /// orders them.
    #[test]
    fn the_bounded_heap_meets_postgres_pairs() {
        let seen = core::cell::RefCell::new(Vec::new()); // alloc: Exec
        CMP_ERR.with(|c| c.set(false));
        let rc = |a: u32, b: u32| {
            seen.borrow_mut().push((a, b));
            CMP_ERR.with(|c| c.set(true));
            Ordering::Equal
        };
        let h = PgHeap { rc: &rc }.make(3, 1);
        assert_eq!(seen.borrow().as_slice(), &[(1, 0)]);
        assert_eq!(h, [0]);
        CMP_ERR.with(|c| c.set(false));
        let v = [5, 3, 9, 1, 7, 2, 8, 6, 4, 0];
        let rc = |a: u32, b: u32| v[a as usize].cmp(&v[b as usize]).reverse();
        let pg = PgHeap { rc: &rc };
        let mut h = pg.make(7, 3);
        for t in 7..10 {
            pg.offer(&mut h, 3, t);
        }
        let got: Vec<i32> = pg.sort(h).iter().map(|&i| v[i as usize]).collect();
        assert_eq!(got, [0, 1, 2]);
    }
}
