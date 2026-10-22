//! D3190, the co-split cut (one module for every user: runs.sort's phase 2, the co-split merge join, a sort's parallel
//! emit): splitters sampled from one input's sorted runs, and any sorted run cut at each splitter's lower bound, so every
//! row of part p sorts before every row of part p + 1 in every run and equal keys never straddle two parts. The compare
//! is the caller's: one input's run compare (runs.sort's phase 2, a sort's emit), or a merge join's clause compare across
//! its inputs (the inner cut at the outer's splitters, `cmp(inner row, outer splitter)`). A run is resident (rows in
//! memory) or put (rows in a spill file, cut through its fences: driver mem/fence.rs; one fence interval read per
//! splitter, none for a splitter between two fences). TOTAL-ORDER: under a compare that is not a total order each cut is
//! held at least its predecessor, so the parts stay disjoint ranges that cover every run (any order is then valid, a lost
//! or doubled row never).
use core::cmp::Ordering;

use gravity_driver::mem::fence::{Fences, Records};
use gravity_driver::{Error, Result};
use gravity_fn::arena::{AVec, OpArena, Refused};
use gravity_kernels::ops::sort::entries::RunShape;

use super::raw;

/// A row compare over row addresses: `cmp(a, b)`.
pub(crate) type Cmp<'a> = &'a dyn Fn(usize, usize) -> Ordering;

/// A put run's records from a fence on (`Some(f)`: its record first) or from its first row (`None`).
pub(crate) type Open<'a> = &'a dyn Fn(Option<usize>) -> Result<Records>;

/// A sorted run as the cut reads it.
pub(crate) enum Run<'a> {
    /// `rows` rows of `rb` bytes from `base`, in memory.
    Mem { base: usize, rows: u64, rb: usize },
    /// A put run: its fences (fence 0 its first row) and its records.
    Put { fences: &'a Fences, open: Open<'a> },
}

impl Run<'_> {
    pub(crate) fn rows(&self) -> u64 {
        match self {
            Run::Mem { rows, .. } => *rows,
            Run::Put { fences, .. } => fences.rows(),
        }
    }
}

/// `parts - 1` splitters of `runs` under `cmp`: `parts - 1` rows of each run with rows at even steps (a put run's at even
/// steps of its fences), ordered by PostgreSQL's qsort, every `len / parts`-th of them. The addresses stay valid while the
/// runs (a put run's fences) live. Empty under 2 parts or without a row.
#[cfg(test)]
pub(crate) fn sample(runs: &[Run<'_>], parts: usize, cmp: Cmp<'_>) -> Vec<usize> {
    if parts < 2 {
        return Vec::new(); // alloc: Exec
    }
    let mut s = Vec::with_capacity(runs.len() * (parts - 1)); // alloc: Exec
    s.extend(core::iter::repeat(0usize).take(runs.len() * (parts - 1)));
    let n = draw(runs, parts, &mut s);
    let k = pick(&mut s[..n], parts, cmp);
    s.truncate(k);
    s
}

/// `sample`'s splitters in `a` (CONVERT wave 2: the sort merge's, in its operator's scratch arena).
pub(crate) fn sample_in(a: &OpArena, runs: &[Run<'_>], parts: usize, cmp: Cmp<'_>) -> core::result::Result<AVec<usize>, Refused> {
    let mut s = AVec::default();
    if parts < 2 {
        return Ok(s);
    }
    s.resize(a, runs.len() * (parts - 1), 0)?;
    let n = draw(runs, parts, s.as_mut_slice(a));
    let k = pick(&mut s.as_mut_slice(a)[..n], parts, cmp);
    s.truncate(k);
    Ok(s)
}

/// The sample's rows into `out` (room for `runs.len() * (parts - 1)`): the count written.
fn draw(runs: &[Run<'_>], parts: usize, out: &mut [usize]) -> usize {
    let mut n = 0;
    for r in runs.iter() {
        for j in 1..parts {
            out[n] = match *r {
                Run::Mem { base, rows, rb } if rows > 0 => base + (j * rows as usize / parts) * rb,
                Run::Put { fences, .. } if !fences.is_empty() => fences.rec(j * fences.len() / parts).as_ptr() as usize,
                _ => break,
            };
            n += 1;
        }
    }
    n
}

/// The `n = s.len()` sampled rows ordered, then every `n / parts`-th of them moved to the front of `s` (each read at or
/// past where it is written); the splitters' count (0 without a row).
fn pick(s: &mut [usize], parts: usize, cmp: Cmp<'_>) -> usize {
    if s.is_empty() {
        return 0;
    }
    // TOTAL-ORDER: a key ordered through fmgr may answer an order that is not total, where std's sort panics;
    // PostgreSQL's qsort leaves a permutation whatever it answers (the sample is a few rows per run)
    gravity_fn::pg_qsort::pg_qsort(s, |&a, &b| cmp(a, b) as i32);
    let n = s.len();
    for p in 1..parts {
        s[p - 1] = s[p * n / parts];
    }
    parts - 1
}

/// Run `run` cut at each of `splitters`' lower bound under `cmp(row, splitter)`: `splitters.len() + 2` row positions,
/// 0 first and the run's rows last, each at least the one before (TOTAL-ORDER). Part p of the run is rows
/// `at[p]..at[p + 1]`.
#[cfg(test)]
pub(crate) fn cut(run: &Run<'_>, splitters: &[usize], cmp: Cmp<'_>) -> Result<Vec<u64>> {
    let mut at = Vec::with_capacity(splitters.len() + 2); // alloc: Exec
    at.extend(core::iter::repeat(0u64).take(splitters.len() + 2));
    cut_into(run, splitters, cmp, &mut at)?;
    Ok(at)
}

/// `cut`'s positions into `at` (`splitters.len() + 2` of them).
pub(crate) fn cut_into(run: &Run<'_>, splitters: &[usize], cmp: Cmp<'_>, at: &mut [u64]) -> Result<()> {
    let rows = run.rows();
    at[0] = 0;
    let mut floor = 0u64;
    match *run {
        Run::Mem { base, rb, .. } => {
            for (i, &k) in splitters.iter().enumerate() {
                let (mut lo, mut hi) = (floor, rows);
                while lo < hi {
                    let mid = lo + (hi - lo) / 2;
                    if cmp(base + mid as usize * rb, k).is_lt() { lo = mid + 1 } else { hi = mid }
                }
                floor = lo;
                at[i + 1] = lo;
            }
        }
        Run::Put { fences, open } => {
            let mut rd = None;
            for (i, &k) in splitters.iter().enumerate() {
                floor = put_bound(fences, open, &mut rd, floor, k, cmp)?.max(floor);
                at[i + 1] = floor;
            }
        }
    }
    at[splitters.len() + 1] = rows;
    Ok(())
}

/// A put run's lower bound of splitter `k` at or past row `floor`: the first fence not under `k` from `floor`'s fence on
/// (a binary search over the fences' copies), then the records of the interval before it from `floor` or that interval's
/// fence on, up to the first not under `k` (at most the next fence). `rd` the run's reader, kept across splitters: the
/// next splitter's interval is read on from it when it lies in the same one.
fn put_bound(fences: &Fences, open: Open<'_>, rd: &mut Option<Records>, floor: u64, k: usize, cmp: Cmp<'_>) -> Result<u64> {
    let n = fences.len();
    if n == 0 {
        return Ok(0);
    }
    let f0 = fences.before(floor);
    let (mut lo, mut hi) = (f0, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if cmp(fences.rec(mid).as_ptr() as usize, k).is_lt() { lo = mid + 1 } else { hi = mid }
    }
    if lo == f0 {
        // floor's fence is not under k: neither is any row from it on
        return Ok(floor);
    }
    let (f, end) = (lo - 1, if lo < n { fences.row(lo) } else { fences.rows() });
    let from = (fences.row(f) + 1).max(floor);
    if from >= end {
        return Ok(end);
    }
    // the reader on: read on when it stands inside fence f's interval at or before `from`, else opened at fence f
    let r = match rd.take() {
        Some(r) if r.row() >= fences.row(f) && r.row() <= from => rd.insert(r),
        _ => rd.insert(open(Some(f))?),
    };
    let skip = from - r.row();
    if r.skip(skip)? != skip {
        return Err(Error::host("gravity: cut: a put run ends before its fence's rows"));
    }
    while r.row() < end {
        let Some(rec) = r.peek()? else { break };
        if !cmp(rec.as_ptr() as usize, k).is_lt() {
            break;
        }
        r.next()?;
    }
    Ok(r.row())
}

/// The compare that cuts input B's runs (rows of shape `sb`) at input A's splitters (rows of shape `sa`) on their first
/// `nkey` keys, `cmp(B row, A splitter)` (the co-split merge join: its inner at its outer's splitters, the clauses' keys first in
/// both sorts); `None` when the two shapes do not order alike (`raw::cross_ok`), and the merge stays serial.
#[allow(dead_code)] // its first user is the co-split merge join (D3190), next on this branch
pub(crate) fn cross<'s>(sa: &'s RunShape, sb: &'s RunShape, nkey: usize) -> Option<impl Fn(usize, usize) -> Ordering + 's> {
    raw::cross_ok(sb, sa, nkey).then_some(move |b: usize, a: usize| raw::cmp_cross(sb, b, sa, a, nkey))
}

/// A put run's records from row `from` on: opened at the last fence at or before it, the rows before `from` passed (a
/// slice's reader; part p of a cut run reads `at[p + 1] - at[p]` records from `at[p]`).
pub(crate) fn records_from(fences: &Fences, open: Open<'_>, from: u64) -> Result<Records> {
    if fences.is_empty() {
        return open(None);
    }
    let f = fences.before(from);
    let mut r = open(Some(f))?;
    let skip = from - fences.row(f);
    if r.skip(skip)? != skip {
        return Err(Error::host("gravity: cut: a put run ends before a slice's first row"));
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_driver::mem::runcodec::{self, RowShape};
    use gravity_driver::{RunReader, SpillFile};

    struct Bytes {
        b: Vec<u8>,
        pos: usize,
    }

    impl RunReader for Bytes {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
            let n = (self.b.len() - self.pos).min(buf.len()).min(5000);
            buf[..n].copy_from_slice(&self.b[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    /// One part; `compress`: a file under a temp limit (its runs compacted); opened at an offset by slicing.
    #[derive(Default)]
    struct File {
        b: Vec<u8>,
        compress: bool,
    }

    impl SpillFile for File {
        fn write_epoch(&mut self, parts: &[(u32, &[u8])]) -> Result<()> {
            parts.iter().for_each(|(_, b)| self.b.extend_from_slice(b));
            Ok(())
        }
        fn read_part(&self, _part: u32, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
            sink(&self.b)
        }
        fn open_part(&self, part: u32) -> Result<Option<Box<dyn RunReader>>> {
            self.open_part_at(part, 0)
        }
        fn open_part_at(&self, _part: u32, offset: u64) -> Result<Option<Box<dyn RunReader>>> {
            Ok((offset < self.b.len() as u64).then(|| Box::new(Bytes { b: self.b[offset as usize..].to_vec(), pos: 0 }) as Box<dyn RunReader>)) // alloc: Cold
        }
        fn spilled_bytes(&self) -> u64 {
            self.b.len() as u64
        }
        fn compress_runs(&self) -> bool {
            self.compress
        }
    }

    const RB: usize = 24;

    fn keys(seed: u64, n: usize, dups: u64) -> Vec<u64> {
        let mut x = seed.wrapping_mul(0x5851_f42d_4c95_7f2d) | 1;
        let mut k: Vec<u64> = (0..n).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x % dups }).collect(); // alloc: Cold
        k.sort_unstable();
        k
    }

    /// A run's rows as words: `[key, tag, row index]` per row.
    fn rows_of(keys: &[u64], tag: u64) -> Vec<u64> {
        keys.iter().enumerate().flat_map(|(i, &k)| [k, tag, i as u64]).collect() // alloc: Cold
    }

    #[allow(unsafe_code)]
    fn key_at(a: usize) -> u64 {
        // SAFETY (test): every address the cut hands a compare is a row's or a fence copy's, 8-aligned, a key word first.
        unsafe { *(a as *const u64) }
    }

    fn by_key(a: usize, b: usize) -> Ordering {
        key_at(a).cmp(&key_at(b))
    }

    fn bytes_of(w: &[u64]) -> Vec<u8> {
        w.iter().flat_map(|x| x.to_ne_bytes()).collect() // alloc: Cold
    }

    /// The cut of run `v` before this module (runs.sort's phase 2 as it was): the reference the Mem cut keeps.
    fn old_cut(base: usize, rows: u32, splitters: &[usize]) -> Vec<u32> {
        let mut at = vec![0]; // alloc: Cold
        let mut floor = 0u32;
        for &k in splitters {
            let (mut lo, mut hi) = (floor, rows);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if by_key(base + mid as usize * RB, k).is_lt() { lo = mid + 1 } else { hi = mid }
            }
            floor = lo;
            at.push(lo);
        }
        at.push(rows);
        at
    }

    /// Resident runs cut as runs.sort's phase 2 cut them before (the same splitters, the same positions), and every row
    /// of part p is under every row of part p + 1 with equal keys in one part.
    #[test]
    fn resident_runs_cut_as_phase_2_did() {
        for (nruns, dups, parts) in [(1, 1 << 40, 2), (3, 5, 4), (8, 1 << 40, 16), (17, 1, 8), (5, 50, 64)] {
            let runs: Vec<Vec<u64>> = (0..nruns).map(|r| rows_of(&keys(r as u64 + 7, (r * 977 + 13) % 3001 + 1, dups), r as u64)).collect();
            let cr: Vec<Run<'_>> = runs.iter().map(|v| Run::Mem { base: v.as_ptr() as usize, rows: (v.len() / 3) as u64, rb: RB }).collect();
            let sp = sample(&cr, parts, &by_key);
            let old: Vec<usize> = {
                let mut s: Vec<usize> = runs.iter().flat_map(|v| (1..parts).map(move |j| v.as_ptr() as usize + (j * (v.len() / 3) / parts) * RB)).collect();
                gravity_fn::pg_qsort::pg_qsort(&mut s, |&a, &b| by_key(a, b) as i32);
                (1..parts).map(|p| s[p * s.len() / parts]).collect()
            };
            assert_eq!(sp, old, "the splitters");
            for (v, r) in runs.iter().zip(cr.iter()) {
                let at = cut(r, &sp, &by_key).unwrap();
                assert_eq!(at.iter().map(|&x| x as u32).collect::<Vec<_>>(), old_cut(v.as_ptr() as usize, (v.len() / 3) as u32, &sp));
                for p in 0..parts {
                    for i in at[p]..at[p + 1] {
                        let key = v[i as usize * 3];
                        assert!(p == 0 || key >= key_at(sp[p - 1]), "part {p} at or over its splitter");
                        assert!(p + 1 == parts || key < key_at(sp[p]), "part {p} under the next splitter");
                    }
                }
            }
        }
    }

    /// CONVERT wave 2: the arena's sample and the slice cut (the sort merge's) answer as `sample` / `cut`, empty runs
    /// among them.
    #[test]
    fn arena_sample_and_cut_into_match_the_vec_forms() {
        let a = OpArena::scratch_of(None, gravity_fn::arena::Kind::Sort);
        for (nruns, dups, parts) in [(1, 1 << 40, 2), (3, 5, 4), (8, 1 << 40, 16), (17, 1, 8), (5, 50, 64), (4, 9, 1), (6, 3, 512)] {
            let runs: Vec<Vec<u64>> = (0..nruns).map(|r| rows_of(&keys(r as u64 + 3, if r % 3 == 1 { 0 } else { (r * 613 + 7) % 2001 + 1 }, dups), r as u64)).collect();
            let cr: Vec<Run<'_>> = runs.iter().map(|v| Run::Mem { base: v.as_ptr() as usize, rows: (v.len() / 3) as u64, rb: RB }).collect();
            let sp = sample(&cr, parts, &by_key);
            let si = sample_in(&a, &cr, parts, &by_key).unwrap();
            assert_eq!(si.as_slice(&a), &sp[..], "the splitters ({nruns} runs, {parts} parts)");
            let mut at = vec![u64::MAX; sp.len() + 2]; // alloc: Cold
            for r in cr.iter() {
                cut_into(r, &sp, &by_key, &mut at).unwrap();
                assert_eq!(at, cut(r, &sp, &by_key).unwrap(), "the positions");
            }
        }
    }

    /// A put run (raw blocks or compacted) cuts where the same rows resident cut, at splitters from resident runs or from
    /// put runs' fences, and its slices read back as the rows of each part.
    #[test]
    fn a_put_run_cuts_where_the_same_rows_cut_resident() {
        let shape = RowShape::new(RB as u32, None).unwrap();
        for (case, (n, dups, parts)) in [(1usize, 3u64, 4usize), (700, 1 << 40, 8), (20_000, 1 << 40, 16), (20_000, 7, 32), (60_000, 900, 64), (9_000, 1, 8)].into_iter().enumerate() {
            for compress in [false, true] {
                let words = rows_of(&keys(case as u64 + 1, n, dups), 1);
                let raw = bytes_of(&words);
                let mut f = File { compress, ..File::default() };
                let fences = runcodec::put_run(&mut f, 0, &raw, Some(&shape), 0, None).unwrap().expect("fenced");
                let open = |x: Option<usize>| match x {
                    Some(i) => Records::at(&f, 0, &fences, i),
                    None => Records::first(&f, 0, &shape, None),
                };
                let mem = Run::Mem { base: words.as_ptr() as usize, rows: n as u64, rb: RB };
                let put = Run::Put { fences: &fences, open: &open };
                // splitters from other runs (some keys between the run's) and from the put run's own fences
                let other: Vec<Vec<u64>> = (0..3).map(|r| rows_of(&keys(case as u64 * 31 + r, 500, dups + 3), 2)).collect();
                let oruns: Vec<Run<'_>> = other.iter().map(|v| Run::Mem { base: v.as_ptr() as usize, rows: 500, rb: RB }).collect();
                for sp in [sample(&oruns, parts, &by_key), sample(core::slice::from_ref(&put), parts, &by_key)] {
                    let want = cut(&mem, &sp, &by_key).unwrap();
                    assert_eq!(cut(&put, &sp, &by_key).unwrap(), want, "case {case}, compress {compress}");
                    for p in 0..want.len() - 1 {
                        let mut r = records_from(&fences, &open, want[p]).unwrap();
                        for i in want[p]..want[p + 1] {
                            let rec = r.next().unwrap().expect("a row of the part");
                            assert_eq!(rec, &raw[i as usize * RB..(i as usize + 1) * RB], "case {case}: part {p}, row {i}");
                        }
                    }
                }
            }
        }
    }

    /// TOTAL-ORDER: under a compare that answers at random the cuts of resident and put runs stay monotone from 0 to the
    /// run's rows (every row in exactly one part), and nothing panics.
    #[test]
    fn a_compare_that_is_not_an_order_still_cuts_disjoint_ranges() {
        let shape = RowShape::new(RB as u32, None).unwrap();
        let words = rows_of(&keys(3, 30_000, 1 << 40), 1);
        let raw = bytes_of(&words);
        let mut f = File::default();
        let fences = runcodec::put_run(&mut f, 0, &raw, Some(&shape), 0, None).unwrap().expect("fenced");
        let open = |x: Option<usize>| match x {
            Some(i) => Records::at(&f, 0, &fences, i),
            None => Records::first(&f, 0, &shape, None),
        };
        let seed = core::cell::Cell::new(0x1234_5678_9abc_def1u64);
        let random = |_: usize, _: usize| {
            let mut x = seed.get();
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            seed.set(x);
            [Ordering::Less, Ordering::Equal, Ordering::Greater][(x % 3) as usize]
        };
        let runs = [Run::Mem { base: words.as_ptr() as usize, rows: 30_000, rb: RB }, Run::Put { fences: &fences, open: &open }];
        for parts in [2, 8, 64] {
            let sp = sample(&runs, parts, &random);
            assert_eq!(sp.len(), parts - 1);
            for r in runs.iter() {
                let at = cut(r, &sp, &random).unwrap();
                assert_eq!((at[0], at[at.len() - 1], at.len()), (0, 30_000, parts + 1));
                assert!(at.windows(2).all(|w| w[0] <= w[1]), "monotone: {at:?}");
            }
        }
    }

    /// Input B (its key's 4-byte image at 16) cut at input A's splitters (the same key's image at 24, another column
    /// before it) by `cross`: each cut is B's lower bound of the splitter's key; shapes that order unalike give none.
    #[test]
    fn a_run_cuts_at_another_inputs_splitters_through_the_cross_compare() {
        use gravity_kernels::host_abi::SortSpec;
        let (spec_a, spec_b) = (SortSpec::pack(&[(Some((24, 4)), false)]).unwrap(), SortSpec::pack(&[(Some((16, 4)), false)]).unwrap());
        let (sa, sb) = (RunShape::fixed(32, 2, spec_a.0), RunShape::fixed(24, 2, spec_b.0));
        // key (word, image): word = k / 4, image = k % 4 as a big-endian u32 (bytes order), flags 1
        let img = |k: u64| u64::from_ne_bytes([0, 0, 0, (k % 4) as u8, 0, 0, 0, 0]);
        let a: Vec<u64> = keys(41, 900, 300).iter().flat_map(|&k| [1, k / 4, 7, img(k)]).collect();
        let b: Vec<u64> = keys(42, 5000, 300).iter().flat_map(|&k| [1, k / 4, img(k)]).collect();
        let ra = Run::Mem { base: a.as_ptr() as usize, rows: 900, rb: 32 };
        let rb = Run::Mem { base: b.as_ptr() as usize, rows: 5000, rb: 24 };
        let own_a = |x: usize, y: usize| raw::cmp_rows(&sa, x, y);
        let sp = sample(core::slice::from_ref(&ra), 16, &own_a);
        let cmp = cross(&sa, &sb, 1).expect("one key laid alike");
        let at = cut(&rb, &sp, &cmp).unwrap();
        let key_b = |i: usize| (b[i * 3 + 1], b[i * 3 + 2]);
        let key_a = |p: usize| key_at_pair(sp[p], 24);
        for (p, &x) in at[1..at.len() - 1].iter().enumerate() {
            let want = (0..5000).position(|i| key_b(i) >= key_a(p)).unwrap_or(5000) as u64;
            assert_eq!(x, want, "splitter {p}");
        }
        let desc = RunShape::fixed(24, 2, SortSpec::pack(&[(Some((16, 4)), true)]).unwrap().0);
        assert!(cross(&sa, &desc, 1).is_none(), "a descending key orders unalike");
    }

    /// The `(word, image word)` of the row at `a` whose image lies at byte `img`.
    #[allow(unsafe_code)]
    fn key_at_pair(a: usize, img: usize) -> (u64, u64) {
        // SAFETY (test): a row of the test's run, 8-aligned, its image word inside it.
        unsafe { (*(a as *const u64).add(1), *((a + img) as *const u64)) }
    }

    /// An empty put run and a run of one row cut at 0 and at their rows; no splitter, one part.
    #[test]
    fn empty_and_one_row_runs() {
        let shape = RowShape::new(RB as u32, None).unwrap();
        for n in [0usize, 1] {
            let words = rows_of(&keys(9, n, 5), 1);
            let raw = bytes_of(&words);
            let mut f = File::default();
            let fences = runcodec::put_run(&mut f, 0, &raw, Some(&shape), 0, None).unwrap().expect("fenced");
            let open = |x: Option<usize>| match x {
                Some(i) => Records::at(&f, 0, &fences, i),
                None => Records::first(&f, 0, &shape, None),
            };
            let put = Run::Put { fences: &fences, open: &open };
            let k = [7u64, 0, 0];
            let ks = [k.as_ptr() as usize; 3];
            let at = cut(&put, &ks, &by_key).unwrap();
            assert_eq!((at.len(), at[0], at[4]), (5, 0, n as u64));
            assert_eq!(cut(&put, &[], &by_key).unwrap(), vec![0, n as u64]); // alloc: Cold
            assert!(sample(core::slice::from_ref(&put), 1, &by_key).is_empty());
            assert_eq!(records_from(&fences, &open, 0).unwrap().skip(9).unwrap(), n as u64);
        }
    }
}
