//! The code fronts' size rule (gap-optoverfit §3.6: the estimate is the window's own, re-checked per row): a front
//! indexes a window's row by the mixed-radix index of its keys' codes,
//! `c0 + n0 * (c1 + n1 * (c2 + ..))`, `n_i` the size of key i's dictionary in the window's part. The index is one-to-one
//! on the tuples whose every code is under its dictionary's size, so a front of `cap` records holds every tuple of a
//! window whose dictionary-size product is at most `cap`, for any number of keys. The sizes are the window's own, known
//! exactly when its rows run, so the choice is made there, per row: a code at or past its size (a NULL's all ones) or an
//! index at or past the capacity takes the hashed table, and a window whose product is past the capacity keeps its low
//! tuples in the front.
//!
//! The planner's group estimate chooses whether three or more keys take the front at all (`takes`, mechanism R: the
//! estimate chooses, each row re-checks and degrades): a row past the front pays its index on top of the hashed table's
//! work, so a table whose groups are estimated past the capacity runs on the hashed table alone. A pair takes the front
//! at any estimate: its estimate without statistics is the default 200 x 200, past every capacity (at most 16 Ki records).
//!
//! The index is saturated at `cap + 1` (`index`): a stride or term past it is clamped there, so no product of dictionary
//! sizes wraps into a false in-front index, and an index past the capacity stays past it.

/// The front's capacity in records of `record` bytes, each with its touched-list word, in `bytes`.
pub const fn capacity(bytes: u64, record: u64) -> u64 {
    bytes / (record + 8)
}

/// The saturation of an index under a front of `cap` records: one past its last record.
pub const fn saturation(cap: u64) -> u64 {
    cap.saturating_add(1)
}

/// The product of `sizes`, saturated at `saturation(cap)`.
pub fn product(sizes: &[u64], cap: u64) -> u64 {
    let lim = saturation(cap);
    sizes.iter().fold(1u64, |p, &n| p.saturating_mul(n).min(lim))
}

/// A window of dictionaries `sizes` keeps every row of a non-NULL key in a front of `cap` records.
pub fn fits(sizes: &[u64], cap: u64) -> bool {
    product(sizes, cap) <= cap
}

/// Whether a front of `cap` records takes a table of `n` code keys whose planner group estimate is `groups`: a pair at any
/// estimate, more keys when the estimate fits. MEASURED (a 2 M-row table, box 10, W0, against the hashed table): three
/// keys of 25, 40 and ~2 M values, far past the front, ran 11 % slower on it; three keys of 5, 25 and 40 values (a
/// product of 5,000, past the record front) 9 % slower.
pub fn takes(n: u8, groups: u64, cap: u64) -> bool {
    n <= 2 || groups <= cap
}

/// The front index of a row's `codes` under its window's dictionary `sizes` (key order), or None when the row takes the
/// hashed table: a code at or past its size, or an index at or past `cap`. The emitted index (lowerv4
/// dict/codefront_dense.rs) gives the same answer: it clamps three or more keys' strides at `saturation(cap)` (a pair's
/// index never wraps), and its terms, a code under its size times a clamped stride, need no clamp.
pub fn index(codes: &[u64], sizes: &[u64], cap: u64) -> Option<u64> {
    let lim = saturation(cap);
    let (mut at, mut stride) = (0u64, 1u64);
    for (&c, &n) in codes.iter().zip(sizes) {
        if c >= n {
            return None;
        }
        at = at.saturating_add(c.min(lim).saturating_mul(stride).min(lim));
        stride = stride.saturating_mul(n).min(lim);
    }
    (at < cap).then_some(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_product_that_fits_indexes_every_tuple_once() {
        for sizes in [&[3u64, 2][..], &[100, 3], &[7, 5, 4], &[2, 2, 2, 2, 2, 2], &[1, 9, 1, 13]] {
            let cap = product(sizes, u64::MAX - 1);
            assert!(fits(sizes, cap) && !fits(sizes, cap - 1), "{sizes:?}");
            let mut seen = vec![false; cap as usize];
            let mut codes = vec![0u64; sizes.len()];
            loop {
                let i = index(&codes, sizes, cap).expect("a tuple of in-size codes under the product") as usize;
                assert!(!seen[i], "{sizes:?} {codes:?}");
                seen[i] = true;
                let Some(k) = (0..codes.len()).find(|&k| codes[k] + 1 < sizes[k]) else { break };
                codes[k] += 1;
                codes[..k].iter_mut().for_each(|c| *c = 0);
            }
            assert!(seen.iter().all(|&s| s), "{sizes:?}");
        }
    }

    #[test]
    fn a_code_past_its_size_or_an_index_past_the_front_takes_the_table() {
        assert_eq!(index(&[3, 0], &[3, 2], 6), None, "a code at its size");
        assert_eq!(index(&[u64::MAX, 1], &[3, 2], 6), None, "a NULL's all ones");
        assert_eq!(index(&[2, 1], &[3, 2], 5), None, "the last tuple past a front of 5");
        assert_eq!(index(&[1, 1], &[3, 2], 5), Some(4), "a low tuple of a window past the front stays in it");
        // dictionary sizes whose product wraps u64 never wrap into the front
        let big = [u32::MAX as u64; 4];
        assert_eq!(index(&[0, 0, 0, 1], &big, 1 << 15), None);
        assert_eq!(index(&[5, 0, 0, 0], &big, 1 << 15), Some(5));
        assert!(!fits(&big, 1 << 15));
        assert_eq!(product(&big, 1 << 15), saturation(1 << 15));
    }

    #[test]
    fn three_keys_take_the_front_by_their_estimate_and_a_pair_always() {
        assert!(takes(2, u64::MAX, 2048) && takes(3, 2048, 2048) && takes(6, 1, 2048));
        assert!(!takes(3, 2049, 2048) && !takes(4, u64::MAX, 2048));
    }

    #[test]
    fn the_capacity_counts_the_list_word() {
        assert_eq!((capacity(256 << 10, 40), capacity(256 << 10, 120)), (5461, 2048));
    }
}
