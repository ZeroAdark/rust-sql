//! The k-way merge over sorted runs (`breakers.md` "Runs") as a tree of losers, its state in the
//! kernel's own region.
use gravity_abi::limits::{MAXORDER, SORT_VAR_MIN_BUF};

pub use super::raw::runmerge::{drain, drain_by, drain_words, open, open_by, open_slices, open_var_by, open_var_in, open_words, open_words_in, EmitRow, KeyOf};

/// Bit 47 of an `EMIT` position: the merge is in flight (its state is in the region).
pub const MERGING: u64 = 1 << 47;
pub const RUNMERGE_KIND: u8 = 4;
/// `RunMerge.tag` of a merge a sort emit laid (host_abi `begin_in`): its `region` and `active` are set.
pub const EMIT_TAG: u8 = 0xa7;

#[repr(C)]
pub struct RunMerge {
    pub nruns: u32,
    pub rec: u32,
    pub buf: u32,
    pub heap_len: u32,
    pub emitted: u64,
    /// [key words low byte, own protocol, var, var offset (2, LE), key words high byte, 0]
    pub(crate) _pad: [u8; 7],
    pub kind: u8,
    /// D1061: the region's bytes the emit laid the merge over, for the next reader of the same run (its header is gone).
    pub region: u32,
    pub active: u8,
    pub tag: u8,
    pub(crate) _pad2: [u8; 2],
    /// D1327: the emit's stash for a record longer than the region's stash (host_abi `stash_big`), held until the
    /// statement's end: its address, its bytes, and whether the last row stashed is in it.
    pub big_stash: u64,
    pub big_cap: u32,
    pub big_last: u8,
    /// D1327: a run's buffer grew past the region (`grow`): the emit's stash checks each record's length.
    pub big_seen: u8,
    pub(crate) _pad3: [u8; 2],
    /// F3 (b): the reader whose emit laid the merge (host_abi `begin_in`), so that reader's next open knows a merge it
    /// left in flight as its own.
    pub reader: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct RunCur {
    pub(crate) handle: u32,
    pub(crate) len: u32,
    pub(crate) pos: u32,
    /// the buffer's bytes: `RunMerge.buf` in the region, a big buffer's own (D1327 `grow`)
    pub(crate) cap: u32,
    /// the buffer's offset from the region's base (D1327: a big buffer's address less the base's)
    pub(crate) off: i64,
    /// D1327: a big buffer's address (0: the buffer is the region's)
    pub(crate) big: u64,
}

pub type Cmp<'a> = &'a dyn Fn(*const u8, *const u8) -> core::cmp::Ordering;

/// The merge's fixed bytes over `nruns` runs: the header, the cursors, the tree.
pub fn fixed_bytes(nruns: usize) -> usize {
    core::mem::size_of::<RunMerge>() + nruns * core::mem::size_of::<RunCur>() + ((nruns * 4 + 7) & !7)
}

/// A var sort emit's stash over a `usable`-byte region merging `nruns` runs (host_abi `begin_in`): the hold word, a run
/// buffer's width, and 8 bytes for the buffers' floor to 8. D1328: without them `lay`'s buffer came out 8 bytes wider than
/// the stash's room in 1.4 % of layouts, and a record that long overran the region's end.
pub fn var_stash_bytes(usable: usize, nruns: usize) -> usize {
    16 + (usable - fixed_bytes(nruns) - 8) / (nruns + 1) / 8 * 8
}

/// D1328: the var merge `open_var_in` lays over `usable - sb` of a `usable`-byte region: (a run's buffer, the stash's room
/// behind the buffers and the hold word, `host_abi::stash_of`). A record fits the stash when the buffer does not pass it.
pub fn var_stash_room(usable: usize, nruns: usize, sb: usize) -> (usize, usize) {
    let fixed = fixed_bytes(nruns);
    let buf = (usable - sb).saturating_sub(fixed) / nruns / 8 * 8;
    (buf, usable.saturating_sub(fixed + nruns * buf + 8))
}

/// The merge's key words (`open_words`), low byte then high: a sort by up to PostgreSQL's 1664 columns (RUNS-27).
pub(crate) fn key_words(m: &RunMerge) -> usize {
    u16::from_le_bytes([m._pad[0], m._pad[5]]) as usize
}

/// D1327: a run's least buffer: a fixed record's width; a variable record's fixed head, at least `SORT_VAR_MIN_BUF` bytes
/// (the cost home's; a longer record grows its run's buffer past the region, `grow`).
pub(crate) fn min_buf(rec: usize, var: bool) -> usize {
    if var { rec.max(SORT_VAR_MIN_BUF) } else { rec }
}

/// D1327: the most runs one merge over `usable` bytes reads, each buffer at least `min` bytes, `out` bytes kept beside
/// them (a pass's output buffer, an emit's stash); at least 2, at most `nruns` and `MAXORDER` (D1494).
pub fn fan_in(usable: usize, min: usize, out: usize, nruns: usize) -> usize {
    let fits = |n: usize| usable >= fixed_bytes(n) + n * min + out;
    let mut n = 2;
    while n < nruns.min(MAXORDER) && fits(n + 1) {
        n += 1;
    }
    n.min(nruns)
}

/// D1327: the fan-in `begin_in`'s var emit lays its stash for (its runs beside a stash as wide as a run's buffer).
pub fn var_emit_fan_in(usable: usize, rec: usize, nruns: usize) -> usize {
    let min = min_buf(rec, true);
    let fits = |n: usize| usable >= fixed_bytes(n) + 8 + (n + 1) * min;
    if nruns <= MAXORDER && fits(nruns) {
        return nruns;
    }
    fan_in(usable, min, 8 + min, nruns)
}

/// D2572: the runs a pass of `n` merges away: `n - last` when one pass can end the merge (its groups of `fan` leave at
/// most `last`), else as many as it can (every run in a group).
pub(crate) fn pass_cut(n: usize, fan: usize, last: usize) -> usize {
    if n.div_ceil(fan) <= last { n - last } else { usize::MAX }
}

/// D2572: a pass's next group over `left` runs still unread: at most `fan`, and one more than the runs it still merges
/// away (a group of k merges k - 1 away).
pub(crate) fn group_len(left: usize, fan: usize, cut: usize) -> usize {
    left.min(fan).min(cut.saturating_add(1))
}

/// A signed key as an order-preserving unsigned word.
pub const fn norm_i64(k: i64) -> u64 {
    (k as u64) ^ (1u64 << 63)
}

#[cfg(test)]
mod pass_tests {
    use super::*;

    /// `merge_passes`' run counts: per pass the groups it merges and the runs it leaves; the final merge's runs.
    fn plan(nruns: usize, fan: usize, last: usize) -> (Vec<Vec<usize>>, usize) {
        let (mut n, mut passes) = (nruns, Vec::new()); // alloc: Cold
        while n > last {
            let (mut cut, mut i, mut groups) = (pass_cut(n, fan, last), 0, Vec::new()); // alloc: Cold
            while i < n && cut > 0 {
                let k = group_len(n - i, fan, cut);
                groups.push(k);
                i += k;
                cut = cut.saturating_sub(k - 1);
            }
            let left = n - i;
            n = groups.len() + left;
            passes.push(groups);
            if left > 0 {
                break;
            }
        }
        (passes, n)
    }

    /// D2572: every group merges 2..=fan runs (a full pass's last group may be one run, copied on as before), the final
    /// merge reads at most `last`, and the pass that ends the merge rewrites only the runs it must (the final merge reads
    /// exactly `last`).
    #[test]
    fn the_last_pass_merges_only_the_runs_the_final_merge_cannot_read() {
        for fan in 2..40 {
            for last in fan..fan + 3 {
                for nruns in 1..(fan * last * 3).min(5000) {
                    let (passes, n) = plan(nruns, fan, last);
                    assert!(n <= last, "{nruns} runs, fan {fan}, last {last}: {n} final runs");
                    if let Some((end, full)) = passes.split_last() {
                        assert!(end.iter().all(|&k| (2..=fan).contains(&k)), "{nruns}/{fan}/{last}: {passes:?}");
                        assert!(full.iter().all(|g| g.iter().rev().skip(1).all(|&k| k == fan) && (1..=fan).contains(g.last().unwrap())), "{nruns}/{fan}/{last}: {passes:?}");
                    }
                    if nruns > last {
                        assert_eq!(n, last, "{nruns}/{fan}/{last}: the final merge reads the most it can");
                    }
                    if nruns > last && nruns.div_ceil(fan) <= last {
                        let merged: usize = passes[0].iter().sum();
                        assert_eq!(passes.len(), 1);
                        assert_eq!(merged - passes[0].len(), nruns - last, "{nruns}/{fan}/{last}: only the excess merged away");
                    }
                }
            }
        }
        // 250 runs at a 64 kB region's fan (59, final 60): 194 runs rewritten, not all 250
        let (passes, n) = plan(250, 59, 60);
        assert_eq!((passes, n), (vec![vec![59, 59, 59, 17]], 60)); // alloc: Cold
    }
}

#[cfg(test)]
mod key_words_tests {
    use super::*;

    /// RUNS-27: more than 127 keys (255 key words) keep their count; the u8 the merge held them in wrapped 256 to 0.
    #[test]
    fn key_words_past_a_byte_round_trip() {
        for nkey in [0u16, 2, 254, 256, 282, 3328] {
            let m = RunMerge { nruns: 1, rec: 8, buf: 8, heap_len: 0, emitted: 0, _pad: [nkey as u8, 1, 0, 0, 0, (nkey >> 8) as u8, 0], kind: RUNMERGE_KIND, region: 0, active: 0, tag: 0, _pad2: [0; 2], big_stash: 0, big_cap: 0, big_last: 0, big_seen: 0, _pad3: [0; 2], reader: 0 };
            assert_eq!(key_words(&m), nkey as usize);
        }
    }
}
