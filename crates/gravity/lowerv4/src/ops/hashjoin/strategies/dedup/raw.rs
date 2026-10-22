//! dedup.rs's unsafe region work (rule R6: a module's unsafe code lives in its raw.rs): the record compares the lay-outs
//! drop a copy by, and D4904's compaction of a keys-only build (`compact`).
#![allow(unsafe_code)]
use gravity_fn::arena::{AVec, OpArena};

use crate::ops::hashjoin::build::{FLAGS, F_KEYS, LOW, N};
use crate::ops::hashjoin::rt::{rd, wr, Rt};
use crate::ops::hashjoin::spill::build::KEYS_DONE;

/// The kept records a chain walk compares a new record with before it chains it anyway (a copy past them stays).
pub(crate) const SEEN: usize = 8;

/// Records `i` and `j` of `t` hold the same bytes the probe reads: the image words, the payload NULL bits (the link word's
/// high half) and the payload words (a by-reference payload's offset, so equal values in two images differ).
///
/// # Safety
/// `t` is a build region with records `i` and `j` in, laid in the full record form.
unsafe fn same(t: &Rt, i: usize, j: usize) -> bool {
    let (a, b) = (t.rec(i), t.rec(j));
    (0..t.words).all(|w| rd::<u64>(a, 8 * w) == rd::<u64>(b, 8 * w)) && *t.link(i) >> 32 == *t.link(j) >> 32 && tail_eq(a.add(8 * (t.words + 1)), b.add(8 * (t.words + 1)), t.rec - 8 * (t.words + 1))
}

unsafe fn tail_eq(a: *const u8, b: *const u8, len: usize) -> bool {
    core::slice::from_raw_parts(a, len) == core::slice::from_raw_parts(b, len)
}

/// Record `i` equals one of the first `SEEN` records of the chain from `head` (record index + 1, 0: none; the next in each
/// record's link word's low half): it is a copy the lay-out leaves unchained.
///
/// # Safety
/// `t` is a build region whose chains below `head` are laid; record `i` is in, not yet chained.
pub(crate) unsafe fn seen(t: &Rt, head: u64, i: usize) -> bool {
    let (mut j1, mut k) = (head, 0);
    while j1 != 0 && k < SEEN {
        let j = j1 as usize - 1;
        if same(t, i, j) {
            return true;
        }
        j1 = *t.link(j) & LOW;
        k += 1;
    }
    false
}

/// Record `i` equals the dense slot `s` (a copy of a record from its link word on, its link's low half cleared) past the
/// key the slot stands for: the NULL bits and the payload words.
///
/// # Safety
/// `s` is a DENSE slot of `t` holding a record of record `i`'s key.
pub(crate) unsafe fn same_slot(t: &Rt, s: *const u8, i: usize) -> bool {
    rd::<u64>(s, 0) >> 32 == *t.link(i) >> 32 && tail_eq(s.add(8), t.rec(i).add(8 * (t.words + 1)), t.rec - 8 * (t.words + 1))
}

/// D4904: build `d` (`len` bytes, its `n` records in, `F_KEYS`: no payload) with each image held by one record, the first of
/// each kept and the records packed in their order; `N` set, the records kept returned. The kept images are an open-addressing
/// table of record numbers carved from `a` (the hash build operator's arena). `None`: not a keys-only build, a pass this open
/// already found not worth it (`KEYS_DONE`), or a carve the allocator refused; nothing moved. A pass that keeps more than
/// three quarters of the records sets `KEYS_DONE`: the build then grows or spills as before, without another pass.
///
/// # Safety
/// `d` is a build region in phase 1 whose first `n` records are in, in the full record form.
pub(crate) unsafe fn compact(a: &OpArena, d: *mut u8, len: usize, n: usize) -> Option<usize> {
    if rd::<u8>(d, FLAGS) & F_KEYS == 0 || rd::<u8>(d, KEYS_DONE) != 0 || n < 2 || n >= u32::MAX as usize {
        return None;
    }
    wr::<u64>(d, N, n as u64);
    let t = Rt::of(d, len);
    let cap = (2 * n).next_power_of_two();
    let mut set = AVec::<u32>::with_capacity(a, cap).ok()?;
    if set.resize(a, cap, 0).is_err() {
        set.release(a);
        return None;
    }
    let (slots, mask) = (set.as_mut_slice(a), cap - 1);
    let same = |x: *mut u8, y: *mut u8| (0..t.words).all(|w| rd::<u64>(x, 8 * w) == rd::<u64>(y, 8 * w));
    let mut keep = 0;
    for i in 0..n {
        let mut s = t.hash(i).to_abi() as usize & mask;
        // a slot holds kept record number + 1 (0: free); the kept records sit below `keep <= i`, so none moves under the walk
        while slots[s] != 0 && !same(t.rec(slots[s] as usize - 1), t.rec(i)) {
            s = (s + 1) & mask;
        }
        if slots[s] != 0 {
            continue;
        }
        if keep != i {
            core::ptr::copy_nonoverlapping(t.rec(i), t.rec(keep), t.rec);
        }
        slots[s] = keep as u32 + 1;
        keep += 1;
    }
    set.release(a);
    wr::<u64>(d, N, keep as u64);
    if 4 * keep > 3 * n {
        wr::<u8>(d, KEYS_DONE, 1);
    }
    if keep < n {
        fires::fire_shared("exec.join.semikeys"); // shapecheck fire signal (D4904): a keys-only build dropped records
    }
    Some(keep)
}
