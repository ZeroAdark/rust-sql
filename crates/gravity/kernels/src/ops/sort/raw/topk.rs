//! The bounded run's raw half (ops/sort/topk.rs keeps the spec and part-cut layout words and sizes): the top-k entries
//! over a worker's run, the text run's exact comparator and its ties, and the part cut's entries. Moved unchanged from
//! topk.rs (rule R6: the family's unsafe code lives in raw); topk.rs re-exports them under their old paths.
use crate::host_abi::*;

use super::super::topk::{cut_rest_calls, CUT_LEN_MASK, CUT_LEN_SEEDED, CUT_NO_SEED, CUT_OFF_CODES, CUT_OFF_LEN, CUT_OFF_LEAST, CUT_REST_MAX, CUT_REST_SHIFT, CUT_ROUND_MASK, CUT_ROUND_SHIFT};

// ---- the bounded run's entries (sort.rs h_open / h_insert / h_run / h_order) ----

crate::contract::host! {
    /// Once per claim: the worker's run at its shape (`cap > n` with `TOPK_TIES`: the ties run); null without a partial region.
    pub unsafe fn topk_open(ctx: *mut Ctx, n: u64, cap: u64, natts: u64, flags: u64) -> *mut u8 {
        guard!(core::ptr::null_mut(), {
            let own = partial(ctx);
            if own.is_null() {
                return core::ptr::null_mut();
            }
            TopK::at_cap(own, n as u32, cap as u32, natts as u32, flags as u32) as *mut TopK as *mut u8
        })
    }
}

crate::contract::host! {
    /// The gathered row into the run: 1 entered, 0 not, -1 when a ties run has no room for it.
    pub unsafe fn topk_insert(run: *mut u8, values: *const u64, nulls: *const u8, natts: u64) -> i64 {
        guard!(ERROR, {
            let run = &mut *(run as *mut TopK);
            let v = core::slice::from_raw_parts(values, natts as usize);
            let nl = core::slice::from_raw_parts(nulls, natts as usize);
            run.insert_ties(v, nl).map_or(-1, i64::from)
        })
    }
}

crate::contract::host! {
    /// The emit's run: the merged region (a zeroed region reads as an empty run).
    pub unsafe fn topk_run(ctx: *mut Ctx) -> *mut u8 {
        partial(ctx)
    }
}

crate::contract::host! {
    /// The merged run ordered by `(key image, second word)` — each a pair under `TOPK_PAIRS` / `TOPK_SECOND_PAIR` — and cut to `n`: the rows left.
    pub unsafe fn topk_order(run: *mut u8, n: u64) -> i64 {
        guard!(ERROR, {
            let run = &mut *(run as *mut TopK);
            let (flags, kw) = (run.flags, run.key_words());
            let order = |v: &[u64]| -> (i64, u64, i64, u64) {
                let (k0, k1) = (v[0] as i64, if flags & TOPK_PAIRS != 0 { v[1] } else { 0 });
                let (s0, s1) = if flags & TOPK_SECOND_PAIR != 0 { (v[kw] as i64, v[kw + 1]) } else { (0, v[kw]) };
                (k0, k1, s0, s1)
            };
            run.sort_by(|a, b| order(a.0) < order(b.0));
            run.truncate(n as u32);
            run.len as i64
        })
    }
}

crate::contract::host! {
    /// A text top-k key's image word (M4-L3T): the code word `part << 32 | code` of code lane `lane` of site `site` rendered through the
    /// dictionary, its first eight payload bytes big-endian as a signed word (`^ i64::MIN`: the bytes' unsigned order; the bytes past the
    /// length zero) into `out`; `ERROR` when the code renders to nothing.
    pub unsafe fn topk_text_word(ctx: *mut Ctx, site: u64, lane: u64, word: u64, out: *mut i64) -> i64 {
        guard!(ERROR, {
            let Ok(Some(p)) = crate::dict::reps::render(gravity_abi::ctx::site_ptr(ctx, site as usize), lane as u8, word) else { return bare("a top-k text code renders to nothing") };
            let bytes = crate::util::text_bytes(p as u64);
            let mut b8 = [0u8; 8];
            let m = bytes.len().min(8);
            b8[..m].copy_from_slice(&bytes[..m]);
            *out = (u64::from_be_bytes(b8) ^ (1u64 << 63)) as i64;
            0
        })
    }
}

/// `insert_by` keeping the rows tied with row `n − 1` under `less` (the exact comparator: equal = neither less): 1 entered, 0 past the bound, `UNSUPPORTED_SCRATCH` when the capacity holds no droppable row.
unsafe fn insert_text_ties(run: &mut TopK, v: &[u64], nl: &[u8], mut less: impl FnMut((&[u64], &[u8]), (&[u64], &[u8])) -> bool) -> i64 {
    let (w, len, n, cap) = (run.natts as usize, run.len as usize, run.n as usize, run.cap as usize);
    if len >= n && less(run.row(n - 1), (v, nl)) {
        return 0;
    }
    let mut i = len;
    while i > 0 && less((v, nl), run.row(i - 1)) {
        i -= 1;
    }
    let base = run as *mut TopK as *mut u8;
    let vals = base.add(size_of::<TopK>()) as *mut u64;
    let nulls = base.add(size_of::<TopK>() + cap * w * 8);
    let moved = if len == cap {
        if i >= n || (i < n - 1 && !less(run.row(n - 2), run.row(len - 1))) || (i == n - 1 && !less((v, nl), run.row(len - 1))) {
            return unsupported(UNSUPPORTED_SCRATCH);
        }
        len - 1 - i
    } else {
        len - i
    };
    core::ptr::copy(vals.add(i * w), vals.add((i + 1) * w), moved * w);
    core::ptr::copy(nulls.add(i * w), nulls.add((i + 1) * w), moved * w);
    core::ptr::copy_nonoverlapping(v.as_ptr(), vals.add(i * w), w);
    core::ptr::copy_nonoverlapping(nl.as_ptr(), nulls.add(i * w), w);
    if len < cap {
        run.len += 1;
    }
    run.len = tie_end(run, n, &mut less) as u32;
    1
}

/// The first row past `n − 1` that is not its exact tie (the run sorted under `less`).
unsafe fn tie_end(run: &TopK, n: usize, less: &mut impl FnMut((&[u64], &[u8]), (&[u64], &[u8])) -> bool) -> usize {
    let len = run.len as usize;
    if len <= n {
        return len;
    }
    let mut e = n;
    while e < len && !less(run.row(n - 1), run.row(e)) {
        e += 1;
    }
    e
}

pub(crate) unsafe fn code_bytes<'a>(site: *mut gravity_abi::Site, lane: u8, word: u64) -> Option<&'a [u8]> {
    crate::dict::reps::render(site, lane, word).ok().flatten().map(|p| crate::util::text_bytes(p as u64))
}

/// The exact comparator of a text top-k run (M4-L3T): the key image (the pair under `TOPK_PAIRS`), then a text first key's bytes through the dictionary on an image tie, then the second key (its word or pair, or its bytes, NULLs last); `failed` set when a code renders to nothing.
unsafe fn topk_text_less<'a>(site: *mut gravity_abi::Site, flags: u32, spec1: u64, spec2: u64, failed: &'a mut bool) -> impl FnMut((&[u64], &[u8]), (&[u64], &[u8])) -> bool + 'a {
    let (k1, l1) = (((spec1 >> 8) & 0xffff) as usize, (spec1 >> 24) as u8);
    let (kind2, c2, l2, at2) = ((spec2 & 0xff) as u8, ((spec2 >> 8) & 0xffff) as usize, ((spec2 >> 24) & 0xff) as u8, (spec2 >> 40) as usize);
    let pairs = flags & TOPK_PAIRS != 0;
    move |a: (&[u64], &[u8]), b: (&[u64], &[u8])| -> bool {
        let (ka, kb) = ((a.0[0] as i64, if pairs { a.0[1] } else { 0 }), (b.0[0] as i64, if pairs { b.0[1] } else { 0 }));
        if ka != kb {
            return ka < kb;
        }
        if k1 != TEXT_NONE as usize {
            match (code_bytes(site, l1, a.0[k1]), code_bytes(site, l1, b.0[k1])) {
                (Some(x), Some(y)) if x != y => return x < y,
                (Some(_), Some(_)) => {}
                _ => {
                    *failed = true;
                    return false;
                }
            }
        }
        match kind2 {
            1 => {
                let (sa, sb) = (a.0[at2], b.0[at2]);
                if sa != sb {
                    return sa < sb;
                }
            }
            2 => {
                let (sa, sb) = ((a.0[at2] as i64, a.0[at2 + 1]), (b.0[at2] as i64, b.0[at2 + 1]));
                if sa != sb {
                    return sa < sb;
                }
            }
            3 => match (a.1[c2] != 0, b.1[c2] != 0) {
                (true, true) => {}
                (true, false) => return false,
                (false, true) => return true,
                (false, false) => match (code_bytes(site, l2, a.0[c2]), code_bytes(site, l2, b.0[c2])) {
                    (Some(x), Some(y)) if x != y => return x < y,
                    (Some(_), Some(_)) => {}
                    _ => {
                        *failed = true;
                        return false;
                    }
                },
            },
            _ => {}
        }
        false
    }
}

crate::contract::host! {
    /// The gathered row into the run under the exact comparator: the run keeps its `n` best (and their exact ties under `TEXT_TIES`). 1 entered, 0 not, `ERROR` when a code renders to nothing.
    pub unsafe fn topk_insert_text(ctx: *mut Ctx, run: *mut u8, values: *const u64, nulls: *const u8, natts: u64, spec1: u64, spec2: u64) -> i64 {
        guard!(ERROR, {
            let run = &mut *(run as *mut TopK);
            let v = core::slice::from_raw_parts(values, natts as usize);
            let nl = core::slice::from_raw_parts(nulls, natts as usize);
            let site = gravity_abi::ctx::site_ptr(ctx, (spec1 & 0xff) as usize);
            let mut failed = false;
            let entered = if spec1 & TEXT_TIES != 0 {
                insert_text_ties(run, v, nl, topk_text_less(site, run.flags, spec1, spec2, &mut failed))
            } else {
                i64::from(run.insert_by(v, nl, topk_text_less(site, run.flags, spec1, spec2, &mut failed)))
            };
            if failed {
                return bare("a top-k text compare failed (a code renders to nothing)");
            }
            entered
        })
    }
}

crate::contract::host! {
    /// The merged run at the emit ordered under the exact comparator (the leader's merge kept the image ties) and cut to `n`: the rows left, `ERROR` when a code renders to nothing.
    pub unsafe fn topk_order_text(ctx: *mut Ctx, run: *mut u8, n: u64, spec1: u64, spec2: u64) -> i64 {
        guard!(ERROR, {
            let run = &mut *(run as *mut TopK);
            let site = gravity_abi::ctx::site_ptr(ctx, (spec1 & 0xff) as usize);
            let mut failed = false;
            let mut less = topk_text_less(site, run.flags, spec1, spec2, &mut failed);
            run.sort_by(&mut less);
            if spec1 & TEXT_TIES != 0 {
                run.len = tie_end(run, ((spec1 >> TEXT_N_SHIFT) & 0xffff) as usize, &mut less) as u32;
            } else {
                run.truncate(n as u32);
            }
            drop(less);
            if failed {
                return bare("a top-k text order failed (a code renders to nothing)");
            }
            run.len as i64
        })
    }
}

crate::contract::host! {
    /// The table at a statement's first open on this worker (the run empty: the driver zeroes a partial region's `ZERO_PREFIX` alone per statement, and a stale table cuts rows whose smaller siblings now belong to another worker).
    pub unsafe fn topk_cut_open(run: *mut u8, n: u64) -> i64 {
        let t = &*(run as *const TopK);
        if t.len == 0 {
            core::ptr::write_bytes(run.add(cut_base(t.cap, t.natts) as usize), 0, cut_bytes(n as u32) as usize);
        }
        0
    }
}

crate::contract::host! {
    /// A row past the inline check (`owner != tag` or `code <= cut`) claims its part's entry and enters its code among the part's `n` smallest; the entry's `cut` becomes the n-th once `n` codes are held.
    /// B3-12: a full entry is a max-heap (its root the cut), so a replacement costs log2(n) moves, not two scans of `n`;
    /// a replacement that takes a new least code rests the entry for `cut_rest_calls` of `n` and the run of
    /// such replacements (OF-TG2-2; code_cut's IR counts a resting entry's calls down itself and skips the call, the same
    /// step as the branch here); any other replacement ends the run. A claim with a `seed` other than `CUT_NO_SEED`
    /// takes it as the part's cut at once (NEW-45, `CUT_LEN_SEEDED`). A `n` of 0 or less (no top row) keeps no code.
    pub unsafe fn topk_cut(entry: u64, code: u64, tag: u64, n: u64, seed: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let entry = entry as *mut u8;
        if n as i64 <= 0 {
            return 0;
        }
        let e = entry as *mut u64;
        let word = |off: u64| e.wrapping_add(off as usize / 8);
        let n = n as usize;
        let codes = word(CUT_OFF_CODES);
        if *word(CUT_OFF_OWNER) != tag {
            *word(CUT_OFF_OWNER) = tag;
            if seed != CUT_NO_SEED {
                *word(CUT_OFF_CUT) = seed;
                *word(CUT_OFF_LEAST) = 0;
                *word(CUT_OFF_LEN) = CUT_LEN_SEEDED | CUT_REST_MAX << CUT_REST_SHIFT;
                return 0;
            }
            *word(CUT_OFF_LEN) = 1;
            *word(CUT_OFF_CUT) = if n == 1 { code } else { u64::MAX };
            *word(CUT_OFF_LEAST) = code;
            *codes = code;
            return 0;
        }
        let lw = *word(CUT_OFF_LEN);
        let len = (lw & CUT_LEN_MASK) as usize;
        if len < n {
            *codes.add(len) = code;
            *word(CUT_OFF_LEN) = lw + 1;
            *word(CUT_OFF_LEAST) = (*word(CUT_OFF_LEAST)).min(code);
            if len + 1 == n {
                let h = core::slice::from_raw_parts_mut(codes, n);
                for i in (0..n / 2).rev() {
                    sift_down(h, i);
                }
                *word(CUT_OFF_CUT) = h[0];
            }
            return 0;
        }
        if lw >> CUT_REST_SHIFT != 0 {
            *word(CUT_OFF_LEN) = lw - (1 << CUT_REST_SHIFT);
            return 0;
        }
        if code >= *word(CUT_OFF_CUT) {
            return 0;
        }
        if lw & CUT_LEN_SEEDED != 0 {
            *word(CUT_OFF_LEN) = lw | CUT_REST_MAX << CUT_REST_SHIFT;
            return 0;
        }
        let h = core::slice::from_raw_parts_mut(codes, n);
        h[0] = code;
        sift_down(h, 0);
        *word(CUT_OFF_CUT) = h[0];
        let len = lw & CUT_LEN_MASK;
        *word(CUT_OFF_LEN) = if code < *word(CUT_OFF_LEAST) {
            *word(CUT_OFF_LEAST) = code;
            let round = (lw >> CUT_ROUND_SHIFT) & CUT_ROUND_MASK;
            len | (round + 1).min(CUT_ROUND_MASK) << CUT_ROUND_SHIFT | cut_rest_calls(n as u64, round) << CUT_REST_SHIFT
        } else {
            len
        };
        0
    }
}

/// Restores the max-heap order below `i` (every parent at least its children).
fn sift_down(h: &mut [u64], mut i: usize) {
    let n = h.len();
    loop {
        let (l, r) = (2 * i + 1, 2 * i + 2);
        let mut m = i;
        if l < n && h[l] > h[m] {
            m = l;
        }
        if r < n && h[r] > h[m] {
            m = r;
        }
        if m == i {
            return;
        }
        h.swap(i, m);
        i = m;
    }
}

#[cfg(test)]
mod cut_tests {
    use super::super::super::topk::{cut_entry_bytes, cut_rest_calls as rest_calls, CUT_LEN_MASK, CUT_LEN_SEEDED, CUT_NO_SEED, CUT_OFF_CODES, CUT_OFF_CUT, CUT_OFF_LEN, CUT_OFF_OWNER, CUT_REST_MAX, CUT_REST_Q16, CUT_REST_SHIFT, CUT_ROUND_MASK, CUT_ROUND_SHIFT};

    /// The scan form the heap replaced (a full entry's cut is its largest code, a replacement takes the slot that holds
    /// it), with the same run of rests.
    #[derive(Default)]
    struct Scan {
        owner: u64,
        cut: u64,
        codes: Vec<u64>,
        min: u64,
        round: u64,
        rest: u64,
    }

    impl Scan {
        fn cut(&mut self, code: u64, tag: u64, n: usize) {
            if self.owner != tag {
                (self.owner, self.cut, self.min, self.round, self.rest) = (tag, if n == 1 { code } else { u64::MAX }, code, 0, 0);
                self.codes = vec![code]; // alloc: Cold
                return;
            }
            if self.codes.len() < n {
                self.codes.push(code);
                self.min = self.min.min(code);
                if self.codes.len() == n {
                    self.cut = *self.codes.iter().max().unwrap();
                }
                return;
            }
            if self.rest != 0 {
                self.rest -= 1;
                return;
            }
            if code >= self.cut {
                return;
            }
            let at = self.codes.iter().position(|&c| c == self.cut).unwrap();
            self.codes[at] = code;
            self.cut = *self.codes.iter().max().unwrap();
            if code < self.min {
                (self.min, self.rest) = (code, rest_calls(n as u64, self.round));
                self.round = (self.round + 1).min(CUT_ROUND_MASK);
            } else {
                self.round = 0;
            }
        }
    }

    fn rnd(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    const W_CUT: usize = CUT_OFF_CUT as usize / 8;
    const W_OWNER: usize = CUT_OFF_OWNER as usize / 8;
    const W_LEN: usize = CUT_OFF_LEN as usize / 8;
    const W_CODES: usize = CUT_OFF_CODES as usize / 8;

    /// A run of `(code, tag)` rows through the inline check as code_cut lowers it (a row of the entry's part strictly
    /// past its cut is dropped, any other calls `topk_cut` and reaches the top), against the scan form after every row,
    /// and every drop checked: `n` rows of its part that reached the top lie strictly before it (`least`: the part's `n`
    /// least codes that reached the top). Returns the rows dropped and the calls made while the entry rested.
    fn run(n: usize, rows: impl Iterator<Item = (u64, u64)>, what: &str) -> (usize, usize) {
        let mut e = vec![0u64; cut_entry_bytes(n as u32) as usize / 8]; // alloc: Cold
        let (mut scan, mut least, mut part) = (Scan::default(), std::collections::BinaryHeap::<u64>::new(), u64::MAX);
        let (mut dropped, mut rested) = (0, 0);
        for (i, (code, tag)) in rows.enumerate() {
            if tag != part {
                (part, least) = (tag, Default::default());
            }
            if e[W_OWNER] == tag && code > e[W_CUT] {
                let nth = (least.len() == n).then(|| *least.peek().unwrap());
                assert!(nth.is_some_and(|c| c < code), "{what} n {n} row {i}: code {code} dropped, the part's n-th least kept {nth:?}");
                dropped += 1;
                continue;
            }
            rested += (e[W_OWNER] == tag && e[W_LEN] >> CUT_REST_SHIFT != 0) as usize;
            unsafe { super::topk_cut((e.as_mut_ptr() as *mut u8) as u64, code, tag, n as u64, CUT_NO_SEED) };
            scan.cut(code, tag, n);
            least.push(code);
            if least.len() > n {
                least.pop();
            }
            let len = (e[W_LEN] & CUT_LEN_MASK) as usize;
            assert_eq!(e[W_CUT], scan.cut, "{what} n {n} row {i}: the cut");
            assert_eq!((e[W_LEN] >> CUT_ROUND_SHIFT) & CUT_ROUND_MASK, scan.round, "{what} n {n} row {i}: the run");
            assert_eq!(e[W_LEN] >> CUT_REST_SHIFT, scan.rest, "{what} n {n} row {i}: the rest");
            let mut held: Vec<u64> = e[W_CODES..W_CODES + len].to_vec();
            let mut want = scan.codes.clone();
            held.sort_unstable();
            want.sort_unstable();
            assert_eq!(held, want, "{what} n {n} row {i}: the codes");
        }
        (dropped, rested)
    }

    const NS: [usize; 48] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 63, 64, 65, 100, 128, 200, 251, 252];

    /// B3-12: over `n` from 1 to 252 and parts whose rows lie scattered, rise or repeat a few codes, the heap entry keeps
    /// the scan's cut and codes after every row; every drop is safe. A rising part never rests (no row reaches a full
    /// entry's call); the others rest only after their running records (OF-TG2-2), a small share of their calls.
    #[test]
    fn the_heap_entry_keeps_the_scan_cut() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for n in NS {
            for shape in [0, 2, 3] {
                let rows: Vec<(u64, u64)> = (0..3000u64)
                    .map(|i| {
                        let code = match shape {
                            0 => rnd(&mut seed) % 5000,
                            2 => i,
                            _ => rnd(&mut seed) % 7,
                        };
                        (code, if i % 997 == 996 { 3 << 32 | 1 } else { 2 << 32 | 1 })
                    })
                    .collect();
                let (_, rested) = run(n, rows.into_iter(), &format!("shape {shape}")); // alloc: Cold
                if shape == 2 {
                    assert_eq!(rested, 0, "shape {shape} n {n}: the entry rested");
                }
            }
        }
    }

    /// The calls a part of `rows` rows in descending order takes codes on: its first `n` (the fill), then one a rest, the
    /// rests growing as `rest_calls` of the run.
    fn descending_taken(n: usize, rows: usize) -> usize {
        let (mut at, mut taken, mut round) = (n, n, 0);
        while at < rows {
            let r = rest_calls(n as u64, round) as usize;
            (taken, at, round) = (taken + 1, at + 1 + r, (round + 1).min(CUT_ROUND_MASK));
        }
        taken.min(rows)
    }

    /// B3-12 / OF-TG2-2: a part whose rows descend makes every row a new least code; each replacement rests the entry,
    /// the rests growing while the part keeps descending, its drops safe throughout; a new part's claim starts the run
    /// over. The calls that take codes are the fill and one a rest.
    #[test]
    fn a_descending_part_rests_the_entry() {
        for n in NS {
            let rows = (0..20_000u64).map(|i| (1_000_000 - i, 2 << 32 | 1)).chain((0..2000u64).map(|i| (2000 - i, 3 << 32 | 1)));
            let (_, rested) = run(n, rows, "descending");
            let (taken, want) = (22_000 - rested, descending_taken(n, 20_000) + descending_taken(n, 2000));
            assert_eq!(taken, want, "descending n {n}: {taken} calls took codes");
            assert!(taken * 4 < 22_000, "descending n {n}: {taken} calls took codes, most should rest");
        }
    }

    /// B3-12: rows that descend for a while and then lie scattered: the rest ends and the cut tightens again (the codes
    /// let through while resting reached the top; the heap's are still seen codes), so most of the scattered tail drops.
    #[test]
    fn the_entry_takes_codes_again_after_a_descending_run() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for n in NS {
            let head = n + 400;
            let rows: Vec<(u64, u64)> = (0..head as u64).map(|i| 1_000_000 - i).chain((0..30_000).map(|_| rnd(&mut seed) % 1_000_000)).map(|c| (c, 2 << 32 | 1)).collect();
            let (dropped, rested) = run(n, rows.into_iter(), "descending then scattered");
            assert!(rested > 0, "n {n}: the descending head did not rest the entry");
            assert!(dropped > 30_000 * 7 / 10, "n {n}: {dropped} of the 30000 scattered rows dropped");
        }
    }

    /// OF-TG2-2: the kernels' copy of the rest rule is the cost home's (cost_profile cut.rs): the same table, and the same
    /// rests at every `n` the cut holds and past it, over a run's rounds and at the bound.
    #[test]
    fn the_rest_rule_is_the_cost_homes() {
        assert_eq!(CUT_REST_Q16, cost_profile::cut::REST_Q16);
        for n in 0..=300u64 {
            for round in (0..5000u64).chain([65535, u64::MAX / 4, u64::MAX]) {
                assert_eq!(rest_calls(n, round), cost_profile::cut::rest_calls(n, round), "n {n} round {round}");
            }
        }
    }

    /// A top of no rows (`n` 0, or a negative bound) keeps no code and claims nothing, seeded or not: every row reaches
    /// the top.
    #[test]
    fn a_top_of_no_rows_keeps_no_code() {
        for n in [0i64, -1, i64::MIN] {
            let mut e = vec![0u64; cut_entry_bytes(1) as usize / 8]; // alloc: Cold
            for code in [5, 3, 9] {
                unsafe { super::topk_cut((e.as_mut_ptr() as *mut u8) as u64, code, 2 << 32 | 1, n as u64, CUT_NO_SEED) };
                unsafe { super::topk_cut((e.as_mut_ptr() as *mut u8) as u64, code, 3 << 32 | 1, n as u64, 0) };
            }
            assert!(e.iter().all(|&w| w == 0), "n {n}: the entry changed");
        }
    }

    fn shuffle(v: &mut [u64], seed: &mut u64) {
        for i in (1..v.len()).rev() {
            v.swap(i, (rnd(seed) % (i as u64 + 1)) as usize);
        }
    }

    /// NEW-45: a part whose every code `0 .. ncodes` stands for at least one of its rows, its rows in an order of `shape`
    /// (descending, ascending, scattered, descending runs in scattered order), through the inline check as code_cut
    /// lowers it with the claim's seed (`n - 1`; a descending key, its codes complemented: the complement of `ncodes -
    /// n`) and the IR's own rest countdown. Returns the keys that reached the top and those dropped.
    fn seeded(n: usize, ncodes: u64, shape: u32, desc: bool, seed: &mut u64) -> (Vec<u64>, Vec<u64>, u64) {
        let mut codes: Vec<u64> = (0..ncodes).chain((0..2000).map(|_| rnd(seed) % ncodes)).collect(); // alloc: Cold
        match shape {
            0 => codes.sort_unstable_by(|a, b| b.cmp(a)),
            1 => codes.sort_unstable(),
            2 => shuffle(&mut codes, seed),
            _ => {
                codes.sort_unstable_by(|a, b| b.cmp(a));
                let mut runs: Vec<Vec<u64>> = codes.chunks(37).map(|c| c.to_vec()).collect(); // alloc: Cold
                for i in (1..runs.len()).rev() {
                    runs.swap(i, (rnd(seed) % (i as u64 + 1)) as usize);
                }
                codes = runs.concat();
            }
        }
        let cut = if desc { !(ncodes - n as u64) } else { n as u64 - 1 };
        let (tag, mut e) = (2 << 32 | 1, vec![0u64; cut_entry_bytes(n as u32) as usize / 8]); // alloc: Cold
        let (mut reached, mut dropped) = (Vec::new(), Vec::new()); // alloc: Cold
        for (i, key) in codes.into_iter().map(|c| if desc { !c } else { c }).enumerate() {
            if e[W_OWNER] == tag && key > e[W_CUT] {
                dropped.push(key);
                continue;
            }
            if e[W_OWNER] == tag && e[W_LEN] >> CUT_REST_SHIFT != 0 {
                e[W_LEN] -= 1 << CUT_REST_SHIFT;
            } else {
                unsafe { super::topk_cut((e.as_mut_ptr() as *mut u8) as u64, key, tag, n as u64, cut) };
            }
            reached.push(key);
            assert_eq!((e[W_CUT], e[W_LEN] & CUT_LEN_MASK), (cut, CUT_LEN_SEEDED), "n {n} ncodes {ncodes} shape {shape} desc {desc} row {i}: the seeded entry");
        }
        (reached, dropped, cut)
    }

    /// NEW-45: a seeded entry holds no codes and takes none; from the part's first row on, every row past the seed drops
    /// at the inline check (the first only claims, and reaches the top), and every row at or before it reaches the top. So
    /// every drop is safe: the part's `n` least keys that reached the top lie strictly before every dropped key.
    #[test]
    fn a_seeded_part_drops_past_its_nth_code_from_its_first_row() {
        let mut seed = 0x51ed_270b_27a3_f3c5u64;
        for n in NS {
            for ncodes in [n as u64 + 1, n as u64 + 7, 3 * n as u64 + 50, 5000] {
                for shape in 0..4 {
                    for desc in [false, true] {
                        let (mut reached, dropped, cut) = seeded(n, ncodes, shape, desc, &mut seed);
                        let what = format!("n {n} ncodes {ncodes} shape {shape} desc {desc}"); // alloc: Cold
                        let first_past = reached.first().is_some_and(|&k| k > cut) as usize;
                        assert_eq!(reached.iter().filter(|&&k| k > cut).count(), first_past, "{what}: a row past the seed reached the top");
                        reached.sort_unstable();
                        let nth = reached[n - 1];
                        assert!(nth <= cut && dropped.iter().all(|&k| k > nth), "{what}: a drop at or before the n-th least key {nth}");
                        assert_eq!(reached.len() + dropped.len(), ncodes as usize + 2000, "{what}: rows lost");
                    }
                }
            }
        }
    }

    /// NEW-45: a seeded entry's rest is renewed when it runs out, whatever code comes, and its cut never moves; the next
    /// part's claim without a seed starts the entry empty, as before.
    #[test]
    fn a_seeded_entry_rests_for_good_and_a_plain_claim_starts_empty() {
        let n = 10usize;
        let mut e = vec![0u64; cut_entry_bytes(n as u32) as usize / 8]; // alloc: Cold
        unsafe { super::topk_cut((e.as_mut_ptr() as *mut u8) as u64, 500, 2 << 32 | 1, n as u64, 9) };
        assert_eq!((e[W_CUT], e[W_LEN]), (9, CUT_LEN_SEEDED | CUT_REST_MAX << CUT_REST_SHIFT));
        for code in [3u64, 9, 0, 8] {
            e[W_LEN] &= CUT_LEN_MASK;
            unsafe { super::topk_cut((e.as_mut_ptr() as *mut u8) as u64, code, 2 << 32 | 1, n as u64, 9) };
            let want = if code < 9 { CUT_LEN_SEEDED | CUT_REST_MAX << CUT_REST_SHIFT } else { CUT_LEN_SEEDED };
            assert_eq!((e[W_CUT], e[W_LEN]), (9, want), "code {code}");
        }
        assert!(e[W_CODES..].iter().all(|&w| w == 0), "a seeded entry wrote a code");
        unsafe { super::topk_cut((e.as_mut_ptr() as *mut u8) as u64, 7, 3 << 32 | 1, n as u64, CUT_NO_SEED) };
        assert_eq!((e[W_CUT], e[W_LEN], e[W_CODES]), (u64::MAX, 1, 7), "the plain claim");
    }

    /// OF-TG2-2's fit (`#[ignore]`: run alone on the reference machine, `cargo test -p gravity_kernels --profile
    /// fast-profile --lib -- --ignored --nocapture the_cut_unit_costs`): what a call that replaces costs, by the heap's
    /// depth (cost_profile cut.rs `CUT_REPLACE`). A take sends `ROWS` strictly descending codes through a full entry of `n`
    /// codes, called through a function pointer as the program calls it: every call a replacement whose new least code
    /// sifts from the root to a leaf (`floor(log2 n)` levels); the length word is cut back to the codes held after each
    /// call so the entry never rests. Least of five takes a size; a depth's cost is the mean over its sizes. Not a line
    /// over the depth: a small heap's sift branches are learned by the predictor (about 5.7 ns at 1 to 7 codes on the
    /// reference machine), a deep one's are not (about 4.7 ns a level from 8 codes on).
    #[test]
    #[ignore]
    fn the_cut_unit_costs() {
        const ROWS: u64 = 1 << 21;
        let call: unsafe extern "C" fn(u64, u64, u64, u64, u64) -> i64 = std::hint::black_box(super::topk_cut);
        let (tag, mut depth) = (2 << 32 | 1, [(0.0f64, 0u32); 8]);
        for n in [1usize, 2, 3, 4, 5, 7, 8, 12, 15, 16, 24, 31, 32, 48, 63, 64, 100, 127, 128, 200, 251] {
            let mut e = vec![0u64; cut_entry_bytes(n as u32) as usize / 8]; // alloc: Cold
            let mut best = f64::MAX;
            for _ in 0..5 {
                e.iter_mut().for_each(|w| *w = 0);
                let mut code = u64::MAX / 2;
                for _ in 0..n {
                    unsafe { call((e.as_mut_ptr() as *mut u8) as u64, code, tag, n as u64, CUT_NO_SEED) };
                    code -= 1;
                }
                let t = std::time::Instant::now();
                for _ in 0..ROWS {
                    unsafe { call((e.as_mut_ptr() as *mut u8) as u64, code, tag, n as u64, CUT_NO_SEED) };
                    e[W_LEN] &= CUT_LEN_MASK;
                    code -= 1;
                }
                best = best.min(t.elapsed().as_nanos() as f64 / ROWS as f64);
                assert_eq!(e[W_LEN] & CUT_LEN_MASK, n as u64, "n {n}: the entry stays full");
            }
            let levels = (usize::BITS - 1 - n.leading_zeros()) as usize;
            println!("fit\tn\t{n}\tlevels\t{levels}\tns\t{best:.3}");
            depth[levels] = (depth[levels].0 + best, depth[levels].1 + 1);
        }
        let table: Vec<String> = depth.iter().map(|&(t, k)| format!("{:.2}", t / k as f64)).collect(); // alloc: Cold
        println!("fit\tCUT_REPLACE\t{}", table.join("\t"));
        assert!(depth.iter().all(|&(t, k)| k > 0 && t > 0.0), "every depth measured");
    }
}
