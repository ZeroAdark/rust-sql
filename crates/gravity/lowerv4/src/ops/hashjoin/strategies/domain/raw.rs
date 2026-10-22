//! domain.rs's unsafe region work (rule R6: a module's unsafe code lives in its raw.rs).
#![allow(unsafe_code)]
#[allow(unused_imports)]
use super::*;

/// The runner's reader over a finished build (`RegionFn`): each record's key part 0 image word as a one-word key
/// `(word, 0, 1)` (hosts m1 `admit_set`'s items; the bits form's keys from its exist bitmap, as rtfilter's `export` reads
/// them). `None`, so the end admits every key: a region that is not a laid-out build, op 14's held build error (FORM
/// `FAILED`: the probe's first row must still come to raise it), or a build of more records than the cap in `arg` (the
/// reader's last word, bits 0..32).
///
/// # Safety
/// `region` is a finished breaker region of at least the build header's bytes.
pub(super) unsafe fn domain_keys(region: *const u8, words: &[u64], _owner: Option<gravity_fn::arena::Owner>, emit: &mut dyn FnMut(u64, u64, u64)) -> Option<usize> {
    let d = region as *mut u8;
    if rd::<u8>(d, 15) != KIND || rd::<u8>(d, PHASE) != 2 || rd::<u8>(d, FORM) == FAILED {
        return None;
    }
    let t = Rt::of(d, usize::MAX);
    let cap = words.last().map_or(0, |a| *a as u32 as usize);
    if t.n > cap {
        return None;
    }
    if t.n == 0 {
        return Some(0);
    }
    if rd::<u8>(d, FORM) == DBITS {
        // the bits form's records may stay the workers' (build_part.rs): its unique keys from the exist bitmap
        let (dir, lo) = (rd::<u64>(d, DIR) as usize, rd::<i64>(d, LO));
        let set = (0..=(rd::<i64>(d, HI) - lo) as usize).filter(|&k| rd::<u64>(d, dir + 8 * (k >> 6)) >> (k & 63) & 1 != 0);
        set.for_each(|k| emit((lo + k as i64) as u64, 0, 1));
    } else {
        (0..t.n).for_each(|i| emit(t.key(i) as u64, 0, 1));
    }
    Some(t.n)
}

#[cfg(test)]
mod tests {
    use super::domain_keys;
    use crate::ops::hashjoin::build::{FAILED, FLAGS, FORM, F_COMPACT, F_RANGE0, HDR, HI, KIND, LO, N, PHASE, REC, WORDS};
    use crate::ops::hashjoin::rt::wr;

    /// A laid-out (phase 2) build region of `keys` as two-word records (`[key, 1000 + i]`: a key of two parts), flags
    /// `flags`.
    fn region(keys: &[i64], flags: u8) -> Vec<u64> {
        let mut buf = vec![0u64; (HDR + 24 * keys.len()) / 8 + 1];
        let d = buf.as_mut_ptr() as *mut u8;
        // SAFETY: `buf` holds the header and the 24-byte records.
        unsafe {
            (wr::<u8>(d, 15, KIND), wr::<u8>(d, PHASE, 2), wr::<u8>(d, FLAGS, flags), wr::<u8>(d, WORDS, 2), wr::<u32>(d, REC, 24), crate::ops::hashjoin::layout::set_pay(d, 0));
            (wr::<u64>(d, N, keys.len() as u64), wr::<i64>(d, LO, keys.iter().copied().min().unwrap_or(0)), wr::<i64>(d, HI, keys.iter().copied().max().unwrap_or(0)));
            for (i, k) in keys.iter().enumerate() {
                (wr::<i64>(d.add(HDR + 24 * i), 0, *k), wr::<u64>(d.add(HDR + 24 * i), 8, 1000 + i as u64));
            }
        }
        buf
    }

    fn items(buf: &[u64], cap: u64) -> (Option<usize>, Vec<(u64, u64, u64)>) {
        let mut v = Vec::new();
        // SAFETY: `buf` is a finished region of at least the header's bytes.
        let n = unsafe { domain_keys(buf.as_ptr() as *const u8, &[0, cap], None, &mut |a, b, c| v.push((a, b, c))) };
        (n, v)
    }

    /// Each record's first word, a negative key's too, as a one-word key; a key of several parts gives its first part
    /// only; an empty build is the empty set.
    #[test]
    fn a_build_gives_each_records_part_0_word() {
        let (n, v) = items(&region(&[7, -3, 0], F_RANGE0), 16);
        assert_eq!((n, v), (Some(3), vec![(7, 0, 1), ((-3i64) as u64, 0, 1), (0, 0, 1)]));
        assert_eq!(items(&region(&[], F_RANGE0), 16), (Some(0), vec![]), "an empty build");
    }

    /// Past the cap, op 14's held error and a region not laid out give no set (every key admitted), never an empty one.
    #[test]
    fn no_set_past_the_cap_over_a_held_error_or_a_region_not_laid_out() {
        assert_eq!(items(&region(&[1, 2, 3], 0), 2), (None, vec![]), "three records over a cap of two");
        let mut buf = region(&[], 0);
        // SAFETY: `buf` holds the header.
        unsafe { wr::<u8>(buf.as_mut_ptr() as *mut u8, FORM, FAILED) };
        assert_eq!(items(&buf, 16), (None, vec![]), "a FAILED region");
        let mut buf = region(&[1], 0);
        // SAFETY: `buf` holds the header.
        unsafe { wr::<u8>(buf.as_mut_ptr() as *mut u8, PHASE, 1) };
        assert_eq!(items(&buf, 16).0, None, "a region in phase 1");
    }

    /// A compact record (the bits form's 8-byte records) gives its low 32 bits sign-extended, as an int4 key's word.
    #[test]
    fn a_compact_record_gives_its_sign_extended_word() {
        let mut buf = vec![0u64; (HDR + 16) / 8 + 1];
        let d = buf.as_mut_ptr() as *mut u8;
        // SAFETY: `buf` holds the header and two 8-byte records.
        unsafe {
            (wr::<u8>(d, 15, KIND), wr::<u8>(d, PHASE, 2), wr::<u8>(d, FLAGS, F_COMPACT), wr::<u8>(d, WORDS, 1), wr::<u32>(d, REC, 8), crate::ops::hashjoin::layout::set_pay(d, 0));
            wr::<u64>(d, N, 2);
            (wr::<i32>(d.add(HDR), 0, -5), wr::<i32>(d.add(HDR + 8), 0, 9));
        }
        assert_eq!(items(&buf, 16), (Some(2), vec![((-5i64) as u64, 0, 1), (9, 0, 1)]));
    }
}
