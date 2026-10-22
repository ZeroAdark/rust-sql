//! SORTRUNS-E0 L0 (pgrc-plans/SORTRUNS-E0-STEP1.md D2): a sorted run's reverse file (`crate::runrev`). Every form
//! (LABELS and FRAMEIX, ranks DENSE and SET) answers every ID of the run's ranges as built, over several pages of
//! every section; a page whose crc fails, a page read at another page's place, a page whose structure is wrong under
//! a good crc, a truncated file and a file that is not the one `EXT_RUNS` names are refused (an error, never a wrong
//! answer and never a panic).
//!
//! RUNREVFAST (main 10-08: L0's two damage tests made pgrc_format's debug unittests 15 s -> 490 s, the gate's pole):
//! lookups read pages through [`Cached`] (as pgrc_am's map cache serves them: a page's crc is checked once), and the
//! damage tests probe, per damaged page, the IDs whose lookup reads it on the intact file plus a sample of the rest.
//! Every damage class stays in the default run; the exhaustive forms (every probe ID through uncached pages) are
//! `#[ignore]`d with where they run.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::rowid::IDS_PER_BLOCK;
use crate::runrev::{encode, BytePages, Hit, Page, Pages, RunRev, Spec, Writer, PAGE, PAYLOAD};
use crate::sortrun::{RankForm, RunRevForm, SortRun};
use crate::{FormatError, FormatResult};

const B: u64 = IDS_PER_BLOCK;

/// What the test run's builder decided for one present ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    Group(u32),
    Exc(u16, u32),
    Frame(u32),
}

struct Built {
    run: SortRun,
    file: Vec<u8>,
    want: BTreeMap<u64, Hit>,
    probe: Vec<u64>,
}

fn crc_of(payload: &[u8], no: u64) -> u32 {
    let s = ::crc32c::pg_comp_crc32c(::crc32c::CRC32C_INIT, payload);
    ::crc32c::fin_crc32c(::crc32c::pg_comp_crc32c(s, &no.to_le_bytes()))
}

/// A run reverse over `ranges` (the run's homes) whose IDs `present` take `place`; `groups` (LABELS) or `prefix`
/// (FRAMEIX, the slots' first global frames and the total); ranks DENSE or SET.
fn build(form: RunRevForm, rank: RankForm, ranges: &[(u64, u64)], present: &[(u64, Place)], groups: &[(u16, u32, u32)], prefix: &[u32], members: u32) -> Built {
    let nsym = match form {
        RunRevForm::Labels => groups.len() as u32,
        _ => *prefix.last().unwrap(),
    };
    let label = |p: Place| match p {
        Place::Group(g) => g,
        Place::Frame(f) => f,
        Place::Exc(..) => nsym,
    };
    let by_id: BTreeMap<u64, Place> = present.iter().copied().collect();
    let mut labels = Vec::new();
    let mut s_runs: Vec<(u64, u64)> = Vec::new();
    match rank {
        RankForm::Dense => {
            for &(lo, hi) in ranges {
                for id in lo..hi {
                    labels.push(by_id.get(&id).map_or(nsym, |&p| label(p)));
                }
            }
        }
        RankForm::Set => {
            for (&id, &p) in &by_id {
                labels.push(label(p));
                match s_runs.last_mut() {
                    Some((lo, len)) if *lo + *len == id => *len += 1,
                    _ => s_runs.push((id, 1)),
                }
            }
        }
    }
    let exceptions: Vec<(u64, u16, u32)> = by_id.iter().filter_map(|(&id, &p)| if let Place::Exc(s, f) = p { Some((id, s, f)) } else { None }).collect();
    let span: u64 = ranges.iter().map(|r| r.1 - r.0).sum();
    let rows = present.len() as u64;
    let spec = Spec { run_id: 41, form, rank, ranks: if rank == RankForm::Dense { span } else { rows }, rows, nsym, members };
    let (file, fin) = encode(spec, &s_runs, &labels, groups, &exceptions, if form == RunRevForm::FrameIx { prefix } else { &[] }).expect("encode");
    assert_eq!(file.len() as u64, fin.rev_len);
    assert_eq!(&file[..PAGE], &fin.page0[..]);
    let run = SortRun { run_id: 41, level: 1, rev: form, rank, rows, rev_len: fin.rev_len, rev_crc: fin.rev_crc, members: (1..=members).collect(), ranges: ranges.to_vec() };
    let mut want = BTreeMap::new();
    for (&id, &p) in &by_id {
        let h = match p {
            Place::Group(g) => {
                let (slot, va, vb) = groups[g as usize];
                Hit::Group { slot, va, vb }
            }
            Place::Exc(slot, vframe) => Hit::Frame { slot, vframe },
            Place::Frame(f) => {
                let slot = prefix.partition_point(|&x| x <= f) - 1;
                Hit::Frame { slot: slot as u16, vframe: f - prefix[slot] }
            }
        };
        want.insert(id, h);
    }
    let mut probe: Vec<u64> = by_id.keys().copied().collect();
    for &(lo, hi) in ranges {
        probe.extend([lo.saturating_sub(1), lo, hi - 1, hi, hi + 1]);
    }
    probe.extend([0, 1, B - 1, u64::MAX >> 24]);
    probe.sort_unstable();
    probe.dedup();
    Built { run, file, want, probe }
}

fn expect(b: &Built, id: u64) -> Hit {
    b.want.get(&id).copied().unwrap_or(Hit::Absent)
}

/// A file's pages through a cache, as pgrc_am's map cache serves them: each page's crc is checked once when first read,
/// a refused page is never cached (every read of it refuses); `seen` records the pages read.
struct Cached<'a> {
    src: BytePages<'a>,
    pages: RefCell<BTreeMap<u64, Arc<Page>>>,
    seen: RefCell<BTreeSet<u64>>,
}

impl<'a> Cached<'a> {
    fn new(file: &'a [u8]) -> Cached<'a> {
        Cached { src: BytePages(file), pages: RefCell::new(BTreeMap::new()), seen: RefCell::new(BTreeSet::new()) }
    }
}

impl Pages for Cached<'_> {
    fn page(&self, no: u64) -> FormatResult<Arc<Page>> {
        self.seen.borrow_mut().insert(no);
        if let Some(p) = self.pages.borrow().get(&no) {
            return Ok(p.clone());
        }
        let p = self.src.page(no)?;
        self.pages.borrow_mut().insert(no, p.clone());
        Ok(p)
    }
}

fn check_all(b: &Built) {
    let src = Cached::new(&b.file);
    let rr = RunRev::open(&b.run, &src).expect("open");
    for &id in &b.probe {
        assert_eq!(rr.lookup(id, &src).expect("lookup"), expect(b, id), "id {id}");
    }
}

/// The probe IDs whose lookup reads each page of the intact file (the first `per` of them), and an even sample of
/// about 64 probe IDs: the IDs a damaged page's case looks up.
struct Probes {
    by_page: BTreeMap<u64, Vec<u64>>,
    sample: Vec<u64>,
}

fn probes(b: &Built, per: usize) -> Probes {
    let src = Cached::new(&b.file);
    let rr = RunRev::open(&b.run, &src).expect("open");
    let mut by_page: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    for &id in &b.probe {
        src.seen.borrow_mut().clear();
        rr.lookup(id, &src).expect("lookup");
        for &p in src.seen.borrow().iter() {
            let v = by_page.entry(p).or_default();
            if v.len() < per {
                v.push(id);
            }
        }
    }
    let sample = b.probe.iter().copied().step_by(b.probe.len() / 64 + 1).collect();
    Probes { by_page, sample }
}

/// How a damage case reads: the targeted IDs through [`Cached`] (the default), or every probe ID through uncached
/// [`BytePages`] (the exhaustive form).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    Targeted,
    Exhaustive,
}

/// The IDs case `p` looks up, and its page source over the damaged file `f`.
fn reach<'a>(b: &Built, pr: &Probes, how: Reach, p: usize, f: &'a [u8]) -> (Vec<u64>, Box<dyn Pages + 'a>) {
    match how {
        Reach::Exhaustive => (b.probe.clone(), Box::new(BytePages(f))),
        Reach::Targeted => {
            let mut ids: Vec<u64> = pr.by_page.get(&(p as u64)).cloned().unwrap_or_default();
            ids.extend(&pr.sample);
            ids.sort_unstable();
            ids.dedup();
            (ids, Box::new(Cached::new(f)))
        }
    }
}

/// Every page of the file damaged in each class in turn (a flipped bit at the payload's first byte, its middle, its
/// last byte, and in the page's crc): the open, or every lookup that reads the page, refuses with a crc mismatch; every
/// other lookup still answers right; nothing panics.
fn damaged_pages_are_refused(b: &Built, how: Reach) {
    let pr = probes(b, 8);
    let pages = b.file.len() / PAGE;
    for p in 0..pages {
        for at in [0usize, 1000, PAYLOAD - 1, PAYLOAD] {
            let mut f = b.file.clone();
            f[p * PAGE + at] ^= 0x10;
            let (ids, src) = reach(b, &pr, how, p, &f);
            let Ok(rr) = RunRev::open(&b.run, &*src) else { continue };
            let mut refused = 0;
            for &id in &ids {
                match rr.lookup(id, &*src) {
                    Ok(h) => assert_eq!(h, expect(b, id), "page {p} byte {at}: id {id} answered wrong"),
                    Err(e) => {
                        assert!(matches!(e, FormatError::CrcMismatch { .. }), "page {p} byte {at}: {e:?}");
                        refused += 1;
                    }
                }
            }
            assert!(refused > 0, "page {p} byte {at}: no lookup read it");
        }
    }
}

/// Every page but page 0 with its first half all ones (labels past the symbol, slots past the members, a run count past
/// the page, IDs past the space) and a stray byte in its tail, its crc made good again (a writer bug): refused as
/// `Corrupt` or answered right, never a wrong answer, never a panic.
fn bad_structure_is_refused(b: &Built, how: Reach) {
    let pr = probes(b, 8);
    let pages = b.file.len() / PAGE;
    let reseal = |f: &mut Vec<u8>, p: usize| {
        let crc = crc_of(&f[p * PAGE..p * PAGE + PAYLOAD], p as u64);
        f[p * PAGE + PAYLOAD..(p + 1) * PAGE].copy_from_slice(&crc.to_le_bytes());
    };
    for p in 1..pages {
        let mut f = b.file.clone();
        f[p * PAGE..p * PAGE + PAYLOAD / 2].iter_mut().for_each(|x| *x = 0xff);
        f[p * PAGE + PAYLOAD - 1] = 0x5a;
        reseal(&mut f, p);
        let (ids, src) = reach(b, &pr, how, p, &f);
        let Ok(rr2) = RunRev::open(&b.run, &*src) else { continue };
        for &id in &ids {
            if let Ok(h) = rr2.lookup(id, &*src) {
                assert_eq!(h, expect(b, id), "page {p}: id {id} answered wrong");
            }
        }
    }
}

/// A LABELS run over two ranges with holes: ~19k ranks (6 label pages at 10 bits), 1000 groups (3 group pages),
/// ~750 exceptions (3 pages).
fn labels_run(rank: RankForm) -> Built {
    let ranges = [(B, 61 * B), (100 * B, 105 * B)];
    let groups: Vec<(u16, u32, u32)> = (0..1000u32).map(|g| ((g % 7) as u16, g * 3, g * 3 + 1 + g % 4)).collect();
    let mut present = Vec::new();
    let mut k = 0u32;
    for &(lo, hi) in &ranges {
        for id in lo..hi {
            k += 1;
            if k % 11 == 0 {
                continue; // a hole: no row took this ID
            }
            present.push((id, if k % 25 == 7 { Place::Exc((k % 7) as u16, k % 97) } else { Place::Group(k / 17 % 1000) }));
        }
    }
    let b = build(RunRevForm::Labels, rank, &ranges, &present, &groups, &[], 7);
    assert!(b.want.values().filter(|h| matches!(h, Hit::Frame { .. })).count() > 2 * 255, "exceptions on several pages");
    b
}

#[test]
fn runrev_labels_dense_answers_every_id() {
    let b = labels_run(RankForm::Dense);
    assert!(b.file.len() / PAGE >= 13, "every section spans pages: {}", b.file.len() / PAGE);
    check_all(&b);
}

#[test]
fn runrev_labels_set_answers_every_id() {
    check_all(&labels_run(RankForm::Set));
}

/// SET over a sparse ID set of ~20k runs (several S pages): sealed IDs leave most of each block unused.
#[test]
fn runrev_set_over_many_runs_spans_pages() {
    let mut present = Vec::new();
    let mut id = 5 * B;
    for k in 0..20_000u64 {
        for j in 0..1 + k % 3 {
            present.push((id + j, Place::Frame(((present.len() as u64) / 128) as u32)));
        }
        id += 7 + k % 5;
    }
    let total = present.len() as u32 / 128 + 1;
    let prefix = vec![0, total / 3, total / 2, total];
    let ranges = [(5 * B, id.div_ceil(B) * B)];
    let b = build(RunRevForm::FrameIx, RankForm::Set, &ranges, &present, &[], &prefix, 3);
    check_all(&b);
    let src = BytePages(&b.file);
    let rr = RunRev::open(&b.run, &src).unwrap();
    assert_eq!(rr.rank_form(), RankForm::Set);
    assert_eq!(rr.ranks(), present.len() as u64);
    for (i, &(id, _)) in present.iter().enumerate().step_by(97) {
        assert_eq!(rr.rank_of(id, &src).unwrap(), Some(i as u64), "rank of {id}");
    }
}

#[test]
fn runrev_frameix_dense_answers_every_id() {
    let ranges = [(B, 41 * B)];
    let n = 40 * B;
    let present: Vec<(u64, Place)> = (B..B + n).filter(|id| id % 13 != 0).map(|id| (id, Place::Frame(((id * 7919) % n / 128) as u32))).collect();
    let frames = (n / 128 + 1) as u32;
    let prefix = vec![0, 10, frames / 2, frames - 3, frames];
    check_all(&build(RunRevForm::FrameIx, RankForm::Dense, &ranges, &present, &[], &prefix, 4));
}

#[test]
fn runrev_a_one_row_run() {
    let b = build(RunRevForm::Labels, RankForm::Dense, &[(B, 2 * B)], &[(B + 3, Place::Group(0))], &[(0, 0, 1)], &[], 1);
    assert_eq!(b.file.len(), 3 * PAGE, "page 0, one label page, one group page");
    check_all(&b);
}

/// Every page of the file (page 0, every section's pages: first, middle and last), damaged in each class: the IDs that
/// read the page (8 a page) and a sample of the rest, through the cache.
#[test]
fn runrev_every_damaged_page_is_refused() {
    damaged_pages_are_refused(&labels_run(RankForm::Set), Reach::Targeted);
}

/// The exhaustive form: every probe ID of every damage case through uncached pages (minutes in debug).
#[test]
#[ignore = "exhaustive (minutes in debug): runs in SORTRUNS-E0's lane PRECHECK on any pgrc_format change and in landq NIGHTLY (cargo test -p pgrc_format --lib -- --ignored _exhaustive)"]
fn runrev_every_damaged_page_is_refused_exhaustive() {
    damaged_pages_are_refused(&labels_run(RankForm::Set), Reach::Exhaustive);
}

#[test]
fn runrev_a_page_at_another_place_is_refused() {
    let b = labels_run(RankForm::Dense);
    let mut f = b.file.clone();
    let (x, y) = (2 * PAGE, 3 * PAGE);
    let tmp = f[x..x + PAGE].to_vec();
    f.copy_within(y..y + PAGE, x);
    f[y..y + PAGE].copy_from_slice(&tmp);
    let src = BytePages(&f);
    let rr = RunRev::open(&b.run, &src).expect("page 0 and the tops are intact");
    assert!(b.probe.iter().any(|&id| matches!(rr.lookup(id, &src), Err(FormatError::CrcMismatch { .. }))));
    assert!(Page::check(2, f[x..x + PAGE].into()).is_err(), "page 3's bytes at page 2");
    assert!(Page::check(3, f[x..x + PAGE].into()).is_ok());
    assert!(Page::check(0, vec![0u8; PAGE - 1].into()).is_err(), "a short page");
}

#[test]
fn runrev_a_file_other_than_the_named_one_is_refused() {
    let b = labels_run(RankForm::Dense);
    let src = BytePages(&b.file);
    let r = &b.run;
    for (why, run) in [
        ("page 0 crc", SortRun { rev_crc: r.rev_crc ^ 1, ..r.clone() }),
        ("length", SortRun { rev_len: r.rev_len + PAGE as u64, ..r.clone() }),
        ("form", SortRun { rev: RunRevForm::FrameIx, ..r.clone() }),
        ("rank", SortRun { rank: RankForm::Set, ..r.clone() }),
        ("run id", SortRun { run_id: 42, ..r.clone() }),
        ("members", SortRun { members: vec![1, 2], ..r.clone() }),
        ("rows", SortRun { rows: r.rows - 1, ..r.clone() }),
        ("ranges", SortRun { ranges: vec![(B, 61 * B)], ..r.clone() }),
        ("a self-homed run", SortRun { rev: RunRevForm::None, ..r.clone() }),
    ] {
        assert!(RunRev::open(&run, &src).is_err(), "{why}");
    }
    let short = &b.file[..b.file.len() - PAGE];
    assert!(RunRev::open(r, &BytePages(short)).is_err(), "truncated");
    assert!(RunRev::open(r, &BytePages(&b.file[..PAGE])).is_err(), "page 0 alone");
}

/// Pages whose crc holds but whose structure does not (a writer bug), every page but page 0: refused as `Corrupt`,
/// never a wrong answer. The IDs that read the page (8 a page) and a sample of the rest, through the cache.
#[test]
fn runrev_bad_structure_under_a_good_crc_is_refused() {
    bad_structure_is_refused(&labels_run(RankForm::Set), Reach::Targeted);
}

/// The exhaustive form: every probe ID of every page through uncached pages (minutes in debug).
#[test]
#[ignore = "exhaustive (minutes in debug): runs in SORTRUNS-E0's lane PRECHECK on any pgrc_format change and in landq NIGHTLY (cargo test -p pgrc_format --lib -- --ignored _exhaustive)"]
fn runrev_bad_structure_under_a_good_crc_is_refused_exhaustive() {
    bad_structure_is_refused(&labels_run(RankForm::Set), Reach::Exhaustive);
}

#[test]
fn runrev_writer_refuses_misuse() {
    let spec = Spec { run_id: 3, form: RunRevForm::Labels, rank: RankForm::Dense, ranks: 4, rows: 3, nsym: 2, members: 2 };
    assert!(Writer::new(Spec { nsym: 0, ..spec }).is_err(), "no symbol");
    assert!(Writer::new(Spec { members: 0, ..spec }).is_err(), "no member");
    assert!(Writer::new(Spec { rows: 5, ..spec }).is_err(), "more rows than ranks");
    assert!(Writer::new(Spec { form: RunRevForm::None, ..spec }).is_err(), "a NONE run has no reverse");
    assert!(Writer::new(Spec { rank: RankForm::Set, ..spec }).is_err(), "SET: rows are the ranks");
    let mut w = Writer::new(spec).unwrap();
    assert!(w.push_s_run(10, 1).is_err(), "DENSE has no ID set");
    assert!(w.push_label(3).is_err(), "past the exception symbol");
    for l in [0, 1, 2, 0] {
        w.push_label(l).unwrap();
    }
    assert!(w.push_label(0).is_err(), "more labels than ranks");
    assert!(w.push_group(2, 0, 1).is_err(), "slot past the members");
    assert!(w.push_group(0, 1, 1).is_err(), "an empty frame span");
    w.push_group(0, 0, 1).unwrap();
    w.push_exception(100, 1, 0).unwrap();
    assert!(w.push_group(1, 0, 2).is_err(), "groups after exceptions");
    assert!(w.push_exception(100, 1, 0).is_err(), "exceptions not ascending");
    assert!(w.finish(&[]).is_err(), "one group short");
    let mut s = Writer::new(Spec { rank: RankForm::Set, ranks: 3, rows: 3, ..spec }).unwrap();
    s.push_s_run(10, 2).unwrap();
    assert!(s.push_s_run(11, 1).is_err(), "overlapping runs");
    s.push_s_run(12, 1).unwrap();
    for l in [0, 1, 1] {
        s.push_label(l).unwrap();
    }
    assert!(s.push_s_run(20, 1).is_err(), "the ID set after the labels");
    s.push_group(0, 0, 1).unwrap();
    s.push_group(1, 0, 2).unwrap();
    let (tail, fin) = s.finish(&[]).unwrap();
    assert_eq!(tail.len() + fin.page0.len(), fin.rev_len as usize, "adjacent runs merged: one S page, one label page, one group page");
}
