//! rtfilter.rs's unsafe region work (rule R6: a module's unsafe code lives in its raw.rs), moved unchanged from it.
//! The parent re-exports these names, so its callers and the build region's raw.rs read them as before.
#![allow(unsafe_code)]
#[allow(unused_imports)]
use super::*;

/// The runner's reader over a finished build (`RegionFn`): the range, then the keys of a build of at most the cap
/// `declare` put in `arg` (the reader's last word) records, or with road join.rtfilter.multi on (no `ARG_ONE`) of at most
/// `RTFILTER_RUNS_READ` records, which the runner folds into runs and keeps within the cap (D3245); for a key of several
/// parts (`F_RANGE0`) both are of its first part, each record's first image word. PS1-22: a text fingerprint build
/// (`F_FP128`) has no range and emits no tag 0 / 1 item: its key set is `(2, lo, hi)` per record (the record's two image
/// words), and over the cap the count alone (no filter).
///
/// # Safety
/// `region` is a finished breaker region of at least the build header's bytes.
pub(super) unsafe fn export(region: *const u8, words: &[u64], _owner: Option<gravity_fn::arena::Owner>, emit: &mut dyn FnMut(u64, u64, u64)) -> Option<usize> {
    let d = region as *mut u8;
    let flags = rd::<u8>(d, FLAGS);
    // PS1-21: op 14's held build error (FORM `FAILED`, N 0) is no filter, never an empty key set: that would skip every
    // granule, and the probe's first row, which raises PG's error, would never come
    if rd::<u8>(d, 15) != KIND || rd::<u8>(d, PHASE) != 2 || rd::<u8>(d, FORM) == FAILED || flags & (F_RANGE | F_RANGE0 | F_FP128) == 0 {
        return None;
    }
    let t = Rt::of(d, usize::MAX);
    if t.n == 0 {
        return Some(0);
    }
    let cap = words.last().map_or(0, |a| (a >> ARG_SET_AT) as u32 as usize).max(gravity_program::cost::RTFILTER_SET_MAX);
    if flags & F_FP128 != 0 {
        // a reader that knows no tag 2 finds no range over a non-empty build: no filter (agg hosts.rs `rt_filter`)
        if t.n > cap || t.words != 2 || t.compact {
            return Some(1);
        }
        (0..t.n).for_each(|i| emit(2, rd::<u64>(t.rec(i), 0), rd::<u64>(t.rec(i), 8)));
        return Some(t.n);
    }
    emit(0, rd::<i64>(d, LO) as u64, rd::<i64>(d, HI) as u64);
    let one = words.last().is_some_and(|a| a & ARG_ONE != 0);
    if t.n > if one { cap } else { cap.max(gravity_program::cost::RTFILTER_RUNS_READ) } {
        return Some(1);
    }
    if rd::<u8>(d, FORM) == DBITS {
        // the bits form's records may stay the workers' (build_part.rs): its unique keys from the exist bitmap
        let (dir, lo) = (rd::<u64>(d, DIR) as usize, rd::<i64>(d, LO));
        let set = (0..=(rd::<i64>(d, HI) - lo) as usize).filter(|&k| rd::<u64>(d, dir + 8 * (k >> 6)) >> (k & 63) & 1 != 0);
        set.for_each(|k| emit(1, (lo + k as i64) as u64, 0));
    } else {
        (0..t.n).for_each(|i| emit(1, t.key(i) as u64, 0));
    }
    Some(1 + t.n)
}

#[cfg(test)]
mod tests {
    use super::export;
    use crate::ops::hashjoin::build::{FAILED, FLAGS, FORM, F_FP128, F_RANGE, F_RANGE0, HDR, HI, KIND, LO, N, PHASE, REC, WORDS};
    use crate::ops::hashjoin::rt::wr;

    /// A laid-out (phase 2) build region of `n` two-word records (`[i, 1000 + i]`), flags `flags`.
    fn region(n: usize, flags: u8) -> Vec<u64> {
        let mut buf = vec![0u64; (HDR + 24 * n) / 8 + 1];
        let d = buf.as_mut_ptr() as *mut u8;
        // SAFETY: `buf` holds the header and the `n` 24-byte records.
        unsafe {
            (wr::<u8>(d, 15, KIND), wr::<u8>(d, PHASE, 2), wr::<u8>(d, FLAGS, flags), wr::<u8>(d, WORDS, 2), wr::<u32>(d, REC, 24), crate::ops::hashjoin::layout::set_pay(d, 0));
            (wr::<u64>(d, N, n as u64), wr::<i64>(d, LO, 0), wr::<i64>(d, HI, n as i64));
            for i in 0..n {
                (wr::<u64>(d.add(HDR + 24 * i), 0, i as u64), wr::<u64>(d.add(HDR + 24 * i), 8, 1000 + i as u64));
            }
        }
        buf
    }

    fn items(buf: &[u64]) -> (Option<usize>, Vec<(u64, u64, u64)>) {
        let mut v = Vec::new();
        // SAFETY: `buf` is a finished region of at least the header's bytes.
        let n = unsafe { export(buf.as_ptr() as *const u8, &[0], None, &mut |t, a, b| v.push((t, a, b))) };
        (n, v)
    }

    /// PS1-22: a text build (F_FP128) exports `(2, word 0, word 8)` per record and no range within the cap, the count
    /// alone past it (no reader takes a filter from it), count 0 when empty; a build with neither flag exports nothing.
    #[test]
    fn a_text_build_exports_its_fingerprints_and_no_range() {
        let (n, v) = items(&region(3, F_FP128));
        assert_eq!((n, v), (Some(3), vec![(2, 0, 1000), (2, 1, 1001), (2, 2, 1002)]));
        let (n, v) = items(&region(gravity_program::cost::RTFILTER_SET_MAX + 1, F_FP128));
        assert_eq!((n, v.len()), (Some(1), 0), "past the cap: the count alone");
        assert_eq!(items(&region(0, F_FP128)), (Some(0), vec![]), "an empty build");
        assert_eq!(items(&region(3, 0)).0, None, "neither F_RANGE nor F_FP128");
        assert_eq!(items(&region(3, F_RANGE)).1.first().map(|i| i.0), Some(0), "an integer build still leads with its range");
    }

    /// PS1-21 / PS1-26: op 14's held build error (rt.rs `defer_error`: FORM `FAILED`, N 0, the flags as the build's last
    /// window stamped them) exports no filter at all, never the empty key set an empty build exports (which skips every
    /// granule, so the probe's first row could never raise the held error).
    #[test]
    fn a_held_build_error_exports_no_filter() {
        for flags in [F_RANGE, F_RANGE0, F_FP128, F_RANGE | F_RANGE0] {
            let mut buf = region(0, flags);
            // SAFETY: `buf` holds the header.
            unsafe { wr::<u8>(buf.as_mut_ptr() as *mut u8, FORM, FAILED) };
            assert_eq!(items(&buf), (None, vec![]), "flags {flags}: a FAILED region is no filter");
            assert_eq!(items(&region(0, flags)).0, Some(0), "flags {flags}: an empty build is the empty key set");
        }
    }
}
