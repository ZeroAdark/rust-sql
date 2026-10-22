//! The code of a value in a byte-ordered dictionary (B2-20): a part dictionary whose code order is its entries' byte
//! order (pgrc's contract, `DictHandle::byte_rank_sorted`) answers `x = c` and `x IN (...)` by binary search, about
//! `log2 n` entries read per value, never a sweep of its `n` entries. The one lookup every reader of a constant against
//! a code column takes; `table::Bits::of_codes` builds the code set's table from its answers. A byte interval (`x < c`,
//! `x >= c`, `LIKE 'c%'`: P5-06 / SRC-17) is one run of codes, its ends `lower_bound`'s (`table::Bits::of_run`).
use core::cmp::Ordering;

/// The code of the entry whose bytes are `v` in a dictionary of `ncodes` entries in ascending byte order (`entry(code)`:
/// that entry's bytes), `None` when no entry equals `v`; `entry`'s error ends the search.
pub fn code_of<'a, E>(ncodes: u32, v: &[u8], entry: &mut dyn FnMut(u32) -> Result<&'a [u8], E>) -> Result<Option<u32>, E> {
    let (mut lo, mut hi) = (0u32, ncodes);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match entry(mid)?.cmp(v) {
            Ordering::Less => lo = mid + 1,
            Ordering::Greater => hi = mid,
            Ordering::Equal => return Ok(Some(mid)),
        }
    }
    Ok(None)
}


/// The first code whose entry's bytes are at or after `v` in byte order, in a dictionary of `ncodes` entries in ascending
/// byte order (`entry` as `code_of`'s), `ncodes` when every entry is before `v`: the entries in `[a, b)` are the codes
/// `[lower_bound(a), lower_bound(b))`, about `log2 n` entries read each; `entry`'s error ends the search.
pub fn lower_bound<'a, E>(ncodes: u32, v: &[u8], entry: &mut dyn FnMut(u32) -> Result<&'a [u8], E>) -> Result<u32, E> {
    first_not(0, ncodes, entry, &|e: &[u8]| e < v)
}

/// The first code of `[lo, hi)` whose entry `before` does not hold (`hi` when it holds for all), `before` true on a
/// leading run of the codes (a byte order bound).
fn first_not<'a, E>(mut lo: u32, mut hi: u32, entry: &mut dyn FnMut(u32) -> Result<&'a [u8], E>, before: &dyn Fn(&[u8]) -> bool) -> Result<u32, E> {
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if before(entry(mid)?) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

/// The byte successor of `prefix`, the least byte string after every string that starts with it, as `(stem, byte)`: the
/// string `stem ++ [byte]`, its last byte below 0xff raised by one and the 0xff bytes after it dropped. `None` when every
/// byte is 0xff (or there are none): no string bounds them, and the strings with the prefix run to the dictionary's end.
pub fn successor(prefix: &[u8]) -> Option<(&[u8], u8)> {
    let last = prefix.iter().rposition(|&b| b != 0xff)?;
    Some((&prefix[..last], prefix[last] + 1))
}

/// The codes `[lo, hi)` of the entries that start with `prefix` (LIKE 'prefix%'): from `prefix`'s lower bound to its
/// `successor`'s, to `ncodes` when it has none.
pub fn prefix_run<'a, E>(ncodes: u32, prefix: &[u8], entry: &mut dyn FnMut(u32) -> Result<&'a [u8], E>) -> Result<(u32, u32), E> {
    let lo = lower_bound(ncodes, prefix, entry)?;
    let Some((stem, byte)) = successor(prefix) else { return Ok((lo, ncodes)) };
    // `e < stem ++ [byte]`: e's bytes before the stem's end decide, else a shorter e is before, else e's next byte
    let before = |e: &[u8]| {
        let k = e.len().min(stem.len());
        match e[..k].cmp(&stem[..k]) {
            Ordering::Less => true,
            Ordering::Greater => false,
            Ordering::Equal => e.get(stem.len()).map_or(true, |&x| x < byte),
        }
    };
    Ok((lo, first_not(lo, ncodes, entry, &before)?))
}
