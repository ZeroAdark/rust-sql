//! Road `dict.entries`' runs (a count(DISTINCT x) over a dictionary-coded column; the fingerprint ruling and the kernel's
//! layout amendment): a dictionary-coded part's entry fingerprints laid out by bucket, resident with the part like its `Fp128`
//! table (text_hosts `entry_runs`: the same cache, key, cap, LRU and retirement), read by the owner phase in place of
//! the part's phase-1 rows. Owner M1-TABLES (hosts' Owner-Ack: M1-HOSTS).
//!
//! A run record is the plain distinct's pair record's key words, `[fp lo, fp hi]` (the two Fp128 image words
//! rep/code/hash.rs `table_fp` loads: 16 B, no count: an identity counts once), which the file names its fold for
//! (`SpillFile::iov`: kernels fold.rs `ident_iov`, the pair table's `run_plain` at the runs' width; the runs fold
//! in the pair table's own fold); its sizes stay the pair layout's 24 B a record (`part_bytes`, `spilled_bytes`), so the owner's estimates do not
//! move. Bucket `b` of `2^BUCKET_BITS` holds the entries whose route `KeyHash::of([lo, hi])` has top
//! bits `b` (the phase-1 route, `Rec::route`), so the owner phase's partition `p` at `P <= BUCKET_BITS` bits reads
//! buckets `[p << (B - P), (p + 1) << (B - P))`, and a row of a part that publishes rows lands in the partition that holds
//! its copy in another part's runs.
//!
//! The handshake: the consumer's table open offers its layout and radix bits (kernels host_abi
//! v4 `entry_runs_offer`, lowerv4 keyed/table_size.rs); a source takes a part's runs only after an accepted offer
//! (`accepts`), so any other layout or bit count leaves every part publishing its rows.
use core::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use gravity_abi::breaker::{KeyKind, RecordLayout, LAYOUT_IMPLICIT_COUNT};
use gravity_abi::hash::KeyHash;
use gravity_driver::{Error, IovFn, Result, RunReader, SpillFile};

use super::raw::{words_as_bytes, Gather, RunSpan};
use crate::dict::text_hosts::FpTable;

/// The layout constant: `2^11` buckets, `T1_MAX_PARTS` (no phase-1 table has more partitions).
pub const BUCKET_BITS: u32 = 11;
const _: () = assert!(1 << BUCKET_BITS == gravity_abi::breaker::T1_MAX_PARTS);

/// The fingerprint cache's third key word for a part's runs (no table spec fingerprints to it: the spec keys are hashes).
pub const RUNS_KEY: u64 = u64::from_le_bytes(*b"RUNS.B11");

/// Words per run record: the two key words.
const WORDS: usize = 2;

/// The pair layout's bytes a record, which the runs' sizes are reported in (the owner's estimates and region).
const LAYOUT_BYTES: usize = 24;

/// Parts ahead the owner's read prefetches a partition's slice (its first two lines).
const PF: usize = 8;

/// Parts whose bounds the owner's read loads before their slices.
const CHUNK: usize = gravity_program::cost::pgrc::ENTRY_RUN_BOUNDS_BATCH;

/// The owner's gathered run: 4095 records (64 KiB on the reader's stack); a slice of `IN_PLACE` words goes in place.
const GATHER: usize = WORDS * 4095;
const IN_PLACE: usize = WORDS * 512;

/// The offer a source accepts: the plain pair of one Fp128 image (two `Fp128` keys, both the route, no slot but the
/// implicit count sentinel, no validity, 24 B, the kernels' identity fold) at no more radix bits than the
/// layout's buckets. Only an identity fold may
/// take runs: two sources whose claims cover different granules of one part both publish that part's whole runs, so an
/// entry can arrive once per such source (the rows road published each code once); the plain distinct ignores the
/// sentinel's count, and a layout with a real count slot must never be accepted.
pub fn accepts(l: &RecordLayout, bits: u8) -> bool {
    !l.is_wide()
        && l.key_kinds() == [KeyKind::Fp128, KeyKind::Fp128]
        && l.partition_keys() == 2
        && l.nslots() == 1
        && l.flags & LAYOUT_IMPLICIT_COUNT != 0
        && !l.has_validity()
        && l.size() as usize == LAYOUT_BYTES
        && gravity_kernels::table::owner_fold::ident_folds(l)
        && bits as u32 <= BUCKET_BITS
}

/// The bucket of an image `[lo, hi]`: its route's top `BUCKET_BITS` bits.
#[inline(always)]
fn bucket(lo: u64, hi: u64) -> usize {
    KeyHash::of(&[lo, hi]).part(BUCKET_BITS) as usize
}

/// The runs of a part's `Fp128` entries (16 B per code, `lo` then `hi`, little-endian): the records in bucket order and the
/// `2^BUCKET_BITS + 1` bucket offsets (bucket `b` is records `off[b]..off[b + 1]`). One counting pass, one scatter, each a
/// tick per `BATCH` entries.
pub fn build(entries: &[u8]) -> core::result::Result<(Vec<u64>, Box<[u32]>), cancelpoll::Canceled> {
    let mut step = cancelpoll::step("entry_runs build: a part's entries in bucket order");
    let n = entries.len() / 16;
    let word = |i: usize, w: usize| u64::from_le_bytes(entries[16 * i + 8 * w..16 * i + 8 * w + 8].try_into().expect("8 bytes"));
    let mut off = vec![0u32; (1 << BUCKET_BITS) + 1]; // alloc: Cold
    for i in 0..n {
        step.batch(i)?;
        off[bucket(word(i, 0), word(i, 1)) + 1] += 1;
    }
    for b in 0..1 << BUCKET_BITS {
        off[b + 1] += off[b];
    }
    let mut at = [0u32; 1 << BUCKET_BITS];
    at.copy_from_slice(&off[..1 << BUCKET_BITS]);
    let mut recs = vec![0u64; WORDS * n]; // alloc: Cold
    for i in 0..n {
        step.batch(i)?;
        let (lo, hi) = (word(i, 0), word(i, 1));
        let b = bucket(lo, hi);
        let r = WORDS * at[b] as usize;
        recs[r..r + WORDS].copy_from_slice(&[lo, hi]);
        at[b] += 1;
    }
    Ok((recs, off.into_boxed_slice()))
}

/// Owner partition `part` of `2^pbits` as a bucket range (`pbits <= BUCKET_BITS`).
pub fn buckets_of(part: u32, pbits: u8) -> (usize, usize) {
    let s = BUCKET_BITS - pbits as u32;
    ((part as usize) << s, (part as usize + 1) << s)
}

/// A source's pinned runs, read as a file by the owner phase (`Source::owner_input`): partition `p` is every table's
/// buckets of `p`. The phase-1 claims fill it; the owner phase only reads it.
#[derive(Default)]
pub struct Runs {
    tables: Vec<crate::dict::text_hosts::FpPin>,
    /// Each table's records and offsets, flat: a partition's walk over the parts reads these, not each table's header.
    spans: Vec<RunSpan>,
    /// The runs' bytes at the pair layout's 24 B a record (`spilled_bytes`).
    bytes: u64,
    pbits: AtomicU8,
}

impl Runs {
    pub fn push(&mut self, t: Arc<FpTable>) {
        let (recs, off) = t.runs().expect("an entry-runs table");
        debug_assert!(off.len() == (1 << BUCKET_BITS) + 1, "a table of 2^BUCKET_BITS buckets");
        self.spans.push(RunSpan::new(recs, off)); // alloc: Node
        self.bytes += (LAYOUT_BYTES * (recs.len() / WORDS)) as u64;
        self.tables.push(crate::dict::text_hosts::FpPin::of(&t)); // alloc: Node
    }

    /// D3213: the lists a parked source keeps (the tables are the fp cache's).
    pub fn kept_bytes(&self) -> usize {
        gravity_driver::vec_bytes(&self.tables) + gravity_driver::vec_bytes(&self.spans)
    }

    pub fn clear(&mut self) {
        self.spans.clear();
        self.tables.clear();
        self.bytes = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    /// The file at `pbits` partition bits; `None` when no part published runs.
    pub fn at(&self, pbits: u8) -> Option<&dyn SpillFile> {
        debug_assert!(pbits as u32 <= BUCKET_BITS, "an owner phase past the runs' buckets");
        self.pbits.store(pbits, Ordering::Relaxed);
        (!self.tables.is_empty()).then_some(self as &dyn SpillFile)
    }

    /// Span `s`'s records `lo..hi` (entry indexes) as words, borrowed from the table `self` holds.
    #[inline(always)]
    fn words(&self, s: RunSpan, lo: usize, hi: usize) -> &[u64] {
        s.words(&self.tables, WORDS * lo, WORDS * hi)
    }

    /// A tick per part's slice handed out (and per `BATCH` records filtered at the finer partition counts).
    fn each(&self, part: u32, f: &mut dyn FnMut(&[u64]) -> Result<()>) -> Result<()> {
        let mut step = cancelpoll::step("entry_runs each: a partition's records");
        let pbits = self.pbits.load(Ordering::Relaxed) as u32;
        if pbits > BUCKET_BITS {
            // never at M1 (agg_open clamps the bits): the finer partition's records of its bucket, filtered
            let b = (part >> (pbits - BUCKET_BITS)) as usize;
            for s in &self.spans {
                step.tick()?;
                for (i, r) in self.words(*s, s.at(b), s.at(b + 1)).chunks_exact(WORDS).enumerate() {
                    step.batch(i)?;
                    if KeyHash::of(&r[..2]).part(pbits) == part {
                        f(r)?;
                    }
                }
            }
            return Ok(());
        }
        let (b0, b1) = buckets_of(part, pbits as u8);
        // a partition is one short slice per part: a chunk's bounds first (independent loads the core overlaps; a walk
        // that read each part's offsets just before its slice waited on them), then its slices, each `PF` parts ahead
        let mut at = [(0u32, 0u32); CHUNK];
        for c in self.spans.chunks(CHUNK) {
            step.tick()?;
            // bounded("small-fixed: one chunk's parts, at most ENTRY_RUN_BOUNDS_BATCH (256)")
            for (a, s) in at.iter_mut().zip(c) {
                *a = (s.at(b0) as u32, s.at(b1) as u32);
            }
            let fetch = |k: usize| {
                if let (Some(s), Some(&(lo, _))) = (c.get(k), at.get(k)) {
                    let p = s.addr(WORDS * lo as usize);
                    gravity_abi::breaker::prefetch_read(p);
                    gravity_abi::breaker::prefetch_read(p.wrapping_add(64));
                }
            };
            (0..PF).for_each(fetch);
            for (k, (s, &(lo, hi))) in c.iter().zip(&at).enumerate() {
                step.tick()?;
                fetch(k + PF);
                if hi > lo {
                    f(self.words(*s, lo as usize, hi as usize))?;
                }
            }
        }
        Ok(())
    }
}

impl SpillFile for Runs {
    fn write_epoch(&mut self, _parts: &[(u32, &[u8])]) -> Result<()> {
        Err(Error::host("gravity: entry runs are read-only"))
    }

    /// Partition `part`'s runs: one short slice per part at the finer partition counts (at 2048 partitions a few records each), so
    /// they are gathered into runs of `GATHER` words the owner's fold takes in one call (its prefetch ring spans the run);
    /// a slice of `IN_PLACE` words or more is handed out in place.
    fn read_part(&self, part: u32, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        let mut buf = Gather::<GATHER>::new();
        let flush = |sink: &mut dyn FnMut(&[u8]) -> Result<()>, buf: &mut Gather<GATHER>| -> Result<()> {
            if buf.len() > 0 {
                sink(buf.bytes())?;
                buf.clear();
            }
            Ok(())
        };
        self.each(part, &mut |w: &[u64]| {
            if w.len() >= IN_PLACE {
                flush(sink, &mut buf)?;
                return sink(words_as_bytes(w));
            }
            if buf.len() + w.len() > GATHER {
                flush(sink, &mut buf)?;
            }
            buf.push(w);
            Ok(())
        })?;
        flush(sink, &mut buf)
    }

    /// The runs' records are 16 B identities: the pair table's identity fold at that width (kernels fold.rs `ident_iov`).
    fn iov(&self) -> Option<(IovFn, usize)> {
        Some((gravity_kernels::table::owner_fold::ident_iov, 8 * WORDS))
    }

    fn open_part(&self, _part: u32) -> Result<Option<Box<dyn RunReader>>> {
        Ok(None)
    }

    /// The runs' bytes: the one owner's region is sized from them as from spilled records (section.rs `own_all`).
    fn spilled_bytes(&self) -> u64 {
        self.bytes
    }

    /// Partition `part`'s run bytes from the bucket offsets alone (past `BUCKET_BITS` partition bits, its bucket's: an
    /// upper bound).
    fn part_bytes(&self, part: u32) -> u64 {
        let pbits = self.pbits.load(Ordering::Relaxed) as u32;
        let (b0, b1) = match pbits > BUCKET_BITS {
            true => ((part >> (pbits - BUCKET_BITS)) as usize, (part >> (pbits - BUCKET_BITS)) as usize + 1),
            false => buckets_of(part, pbits as u8),
        };
        self.spans.iter().map(|s| (LAYOUT_BYTES * (s.at(b1) - s.at(b0))) as u64).sum()
    }
}

/// Partition `part` of `f`, chunk by chunk (the unit tests' reader).
#[cfg(test)]
fn read_all(f: &dyn SpillFile, part: u32, each: &mut dyn FnMut(&[u8])) {
    f.read_part(part, &mut |b: &[u8]| {
        each(b);
        Ok(())
    })
    .expect("runs read");
}

/// The source side of kernels' `entry_runs_offer`, installed once (the first entries recipe).
pub fn install() {
    gravity_kernels::dict::entries_tables::set_entry_runs_offer(super::raw::entry_runs_offer);
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use gravity_abi::breaker::{KeyKind, Rec, RecordLayout, SlotKind};
    use gravity_abi::hash::KeyHash;

    use super::*;

    fn fps(n: usize) -> Vec<u8> {
        fps_of(n, 0)
    }

    /// `n` pseudo-random images, a distinct stream per `seed`.
    fn fps_of(n: usize, seed: u64) -> Vec<u8> {
        let mut x = 0x9e37_79b9_7f4a_7c15u64 ^ seed.wrapping_mul(0xd1b5_4a32_d192_ed03);
        let mut v = Vec::new(); // alloc: Cold
        for _ in 0..2 * n {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            v.extend_from_slice(&x.to_le_bytes());
        }
        v
    }

    fn pair() -> RecordLayout {
        RecordLayout::with_sentinel(&[KeyKind::Fp128, KeyKind::Fp128], &[], false, None)
    }

    #[test]
    fn the_runs_hold_every_entry_once_as_a_pair_record_in_bucket_order() {
        let e = fps(5000);
        let (recs, off) = build(&e).unwrap();
        assert_eq!((recs.len(), off.len(), off[1 << BUCKET_BITS] as usize), (2 * 5000, (1 << BUCKET_BITS) + 1, 5000));
        let mut seen: Vec<[u64; 2]> = recs.chunks_exact(2).map(|r| [r[0], r[1]]).collect();
        for b in 0..1 << BUCKET_BITS {
            for r in recs[2 * off[b] as usize..2 * off[b + 1] as usize].chunks_exact(2) {
                assert_eq!(bucket(r[0], r[1]), b);
            }
        }
        let mut want: Vec<[u64; 2]> = e.chunks_exact(16).map(|c| [u64::from_le_bytes(c[..8].try_into().unwrap()), u64::from_le_bytes(c[8..].try_into().unwrap())]).collect();
        seen.sort_unstable();
        want.sort_unstable();
        assert_eq!(seen, want);
    }

    /// main 09-24: a rows-path record (its route `Rec::route`, as phase 1 appends it) and its copy in the runs land in the
    /// same owner partition at every partition count the owner phase can run. Parts of 3,000 and 20,000 entries and forty
    /// of 300: slices handed out in place and short ones gathered past the gather's length.
    #[test]
    fn a_rows_record_and_its_run_copy_meet_in_one_owner_partition_at_every_width() {
        let l = pair();
        assert_eq!(l.size(), 24);
        let (mut e, mut runs) = (Vec::new(), Runs::default()); // alloc: Cold
        for (k, n) in [3000usize, 20000].into_iter().chain(core::iter::repeat_n(300, 40)).enumerate() {
            let part = fps_of(n, k as u64 + 1);
            let (recs, off) = build(&part).unwrap();
            runs.push(Arc::new(FpTable::of_runs(recs, off)));
            e.extend_from_slice(&part);
        }
        let at: std::collections::HashMap<&[u8], usize> = e.chunks_exact(16).enumerate().map(|(i, c)| (c, i)).collect();
        assert_eq!(at.len(), e.len() / 16, "distinct images");
        for pbits in 0..=BUCKET_BITS as u8 {
            let f = runs.at(pbits).expect("runs");
            let mut got = vec![0u32; e.len() / 16]; // alloc: Cold
            for part in 0..1u32 << pbits {
                f.read_part(part, &mut |bytes: &[u8]| {
                    for r in bytes.chunks_exact(16) {
                        let (lo, hi) = (u64::from_le_bytes(r[..8].try_into().unwrap()), u64::from_le_bytes(r[8..16].try_into().unwrap()));
                        let mut row = [lo, hi, 1u64];
                        // the phase-1 record of the same image, routed as the owner phase routes it
                        let rec = unsafe { Rec::at(row.as_mut_ptr() as *mut u8, &l) };
                        assert_eq!(rec.route().part(pbits as u32), part, "pbits {pbits}");
                        assert_eq!(KeyHash::of(&[lo, hi]).part(pbits as u32), part);
                        let i = at[&r[..16]];
                        got[i] += 1;
                    }
                    Ok(())
                })
                .unwrap();
                let mut n = 0;
                f.read_part(part, &mut |b: &[u8]| {
                    n += b.len() as u64;
                    Ok(())
                })
                .unwrap();
                assert_eq!(f.part_bytes(part), n / 16 * 24, "the pair layout's bytes");
            }
            assert!(got.iter().all(|&c| c == 1), "every entry once at pbits {pbits}");
        }
    }

    /// The runs' 16 B records fold into the owner's pair table through the fold the file names
    /// (`iov`), at every partition count: the identity table holds each partition's distinct entries once, entries shared
    /// by parts included, as the rows would; a table of another layout (an explicit count slot besides the sentinel) lays
    /// its identities by its own record size, each distinct once.
    #[test]
    fn the_runs_fold_through_their_own_fold_into_the_pair_table() {
        use gravity_abi::breaker::{HashAgg, SubSplit, HASHAGG_HDR};
        let (mut runs, mut all) = (Runs::default(), Vec::new()); // alloc: Cold
        for k in 0..6u64 {
            // parts 1..5 share the first 400 entries of part 0
            let part = if k == 0 { fps_of(3000, 1) } else { [fps_of(3000, 1)[..400 * 16].to_vec(), fps_of(700, k + 1)].concat() };
            let (recs, off) = build(&part).unwrap();
            runs.push(Arc::new(FpTable::of_runs(recs, off)));
            all.extend_from_slice(&part);
        }
        let distinct: std::collections::HashSet<&[u8]> = all.chunks_exact(16).collect();
        let counted = RecordLayout::with_sentinel(&[KeyKind::Fp128, KeyKind::Fp128], &[SlotKind::Count], false, None);
        assert!(gravity_kernels::table::owner_fold::ident_folds(&pair()) && gravity_kernels::table::owner_fold::ident_folds(&counted));
        for pbits in [0u8, 3, 9, 11] {
            let f = runs.at(pbits).expect("runs");
            let (g, width) = f.iov().expect("the runs name their fold");
            assert_eq!(width, 16);
            for l in [pair(), counted] {
                let mut total = 0usize;
                for part in 0..1u32 << pbits {
                    let mut mem = vec![0u64; (1 << 20) / 8]; // alloc: Cold
                    let split = SubSplit { pbits, depth: 0, reread: 0, sub: 0 };
                    let t = unsafe {
                        let p = mem.as_mut_ptr() as *mut u8;
                        core::ptr::write_bytes(p, 0, HASHAGG_HDR);
                        let t = HashAgg::at(p, &l, HashAgg::cap_of(&l, 1 << 20).min(1 << 14), 1 << 20);
                        t.set_route_bits(split.table_depth());
                        t as *mut HashAgg
                    };
                    let mut folded = 0usize;
                    crate::ops::scan::pgrc::entry_runs::read_all(f, part, &mut |b: &[u8]| {
                        let v = unsafe { g(t, &[(b.as_ptr(), b.len() / width)], width, split) };
                        assert!(v >= 0, "a table of 2^14 slots holds the partition");
                        folded += v as usize;
                    });
                    assert_eq!(folded * 16, f.part_bytes(part) as usize / 24 * 16, "every record folded once");
                    total += unsafe { (*t).len } as usize;
                }
                assert_eq!(total, distinct.len(), "pbits {pbits}: each distinct entry once over the partitions ({} B layout)", l.size());
            }
        }
    }

    #[test]
    fn only_the_plain_fp128_pair_at_the_bucket_bits_is_accepted() {
        assert!((0..=11).all(|b| accepts(&pair(), b)));
        assert!(!accepts(&pair(), 12));
        assert!(!accepts(&RecordLayout::with_sentinel(&[KeyKind::I64, KeyKind::I64], &[], false, None), 11));
        assert!(!accepts(&RecordLayout::with_sentinel(&[KeyKind::Fp128, KeyKind::Fp128, KeyKind::I32], &[], false, None), 11));
        assert!(!accepts(&RecordLayout::with_sentinel(&[KeyKind::Fp128, KeyKind::Fp128], &[SlotKind::Count], false, Some(0)), 11));
        assert!(!accepts(&RecordLayout::with_sentinel(&[KeyKind::Fp128, KeyKind::Fp128], &[], true, None), 11));
    }
}
