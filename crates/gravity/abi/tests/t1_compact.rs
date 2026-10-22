//! D2587 (CAND-M25): a full pool folded by key in memory (`T1::compact`) before it would spill. Over a small region whose
//! table evicts on every collision, a fold with fewer groups than the region holds never flushes and loses no row; with
//! more groups the compaction frees too little and the pool flushes as before, every row still counted once; the chains
//! keep every key in its partition, one record per key, and blocks freed by a compaction are taken again. D2609: at 64
//! partitions (W8) a chain is no sample; the fold folds every chain until `T1::COMPACT_SAMPLE` records say it frees too little.
use std::collections::{BTreeMap, BTreeSet};

use gravity_abi::breaker::{KeyKind, Rec, RecordLayout, SlotKind, T1, T1_HDR};
use gravity_abi::hash::KeyHash;

const L: RecordLayout = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]);
/// A text key's image (two words: rep.varlena.hash's Fp128) and two slots: the 32-byte record of the W8 text cells.
const L2: RecordLayout = RecordLayout::new_nn(&[KeyKind::I64, KeyKind::I64], &[SlotKind::Count, SlotKind::SumI64]);

fn region(bytes: usize) -> Vec<u64> {
    vec![0u64; bytes / 8]
}

fn mix(j: u64) -> u64 {
    let z = j.wrapping_add(0x2545_F491_4F6C_DD1D);
    let z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

struct Fold {
    l: &'static RecordLayout,
    got: BTreeMap<Vec<u64>, i64>,
    flushes: u32,
    compactions: u32,
}

impl Fold {
    fn records(&self, t: *mut T1) -> Vec<(u32, Vec<u64>, i64)> {
        let (tr, l) = (unsafe { &*t }, self.l);
        let mut v = Vec::new();
        for part in 0..tr.parts() as u32 {
            for b in tr.chain(part) {
                for i in 0..b.recs {
                    let r = unsafe { Rec::at(b.ptr.add(i * l.size() as usize) as *mut u8, l) };
                    v.push((part, (0..l.nkeys()).map(|k| r.key(k)).collect(), r.count(0)));
                }
            }
        }
        v
    }

    fn flush(&mut self, t: *mut T1) {
        for (part, k, c) in self.records(t) {
            assert_eq!(KeyHash::of(&k).part(unsafe { (*t).pbits } as u32), part, "key {k:?} in its partition");
            *self.got.entry(k).or_default() += c;
        }
        unsafe { (*t).reset_pool() };
        self.flushes += 1;
    }

    fn compact(&mut self, t: *mut T1) {
        let l = self.l;
        let before = self.records(t).iter().map(|r| r.2).sum::<i64>();
        let mut index = vec![0u32; (2 * unsafe { (*t).longest_chain() }).max(2).next_power_of_two()];
        let mut map = vec![0u32; unsafe { (*t).nblocks } as usize];
        let held = unsafe { T1::compact(t, &mut index, &mut map, &mut |into, from| Rec::at(into, l).fold(&Rec::at(from as *mut u8, l)).is_ok()) };
        let recs = self.records(t);
        assert_eq!(held as usize, recs.len(), "the records held");
        assert_eq!(recs.iter().map(|r| r.2).sum::<i64>(), before, "no row lost or doubled");
        self.compactions += 1;
    }

    /// The pool spent: compacted when that frees a quarter of its blocks, else flushed (kernels `agg_flush`).
    fn spent(&mut self, t: *mut T1, fold: bool) {
        if fold {
            self.compact(t);
            let keys: BTreeSet<Vec<u64>> = self.records(t).into_iter().map(|r| r.1).collect();
            assert_eq!(keys.len(), self.records(t).len(), "one record per key");
            let (nb, next) = unsafe { ((*t).nblocks, (*t).next_block) };
            if 4 * (nb - next) >= nb {
                return;
            }
        }
        self.flush(t);
    }

    /// The pool spent as kernels' `folded_in_memory` takes it: the table drained into the pool and the buffers folded, in
    /// turn, until the table is empty with a quarter of the blocks free; else flushed.
    fn spent_draining(&mut self, t: *mut T1) {
        loop {
            let drained = unsafe { T1::drain(t) };
            let before = unsafe { (*t).next_block };
            self.compact(t);
            let (nb, next) = unsafe { ((*t).nblocks, (*t).next_block) };
            let room = 4 * (nb - next) >= nb;
            if drained && room {
                return;
            }
            if !room || next == before {
                return self.flush(t);
            }
        }
    }

    fn row(&mut self, t: *mut T1, keys: &[u64], fold: bool, draining: bool) {
        let h = KeyHash::of(keys);
        unsafe {
            let r = match T1::probe(t, keys, h) {
                Some(r) => r,
                None => {
                    let home = (*t).home(h);
                    if T1::evict(t, home) {
                        T1::open_at(t, home, keys)
                    } else if draining {
                        // kernels' `locate`: the fold drained the table, so the row probes again
                        self.spent_draining(t);
                        match T1::probe(t, keys, h) {
                            Some(r) => r,
                            None => {
                                assert!(T1::evict(t, home), "a freed pool takes the occupant");
                                T1::open_at(t, home, keys)
                            }
                        }
                    } else {
                        self.spent(t, fold);
                        assert!(T1::evict(t, home), "a freed pool takes the occupant");
                        T1::open_at(t, home, keys)
                    }
                }
            };
            Rec::at(r, self.l).add_count(0, 1);
        }
    }

    fn finish(mut self, t: *mut T1) -> Self {
        while !unsafe { T1::drain(t) } {
            self.flush(t);
        }
        self.flush(t);
        self
    }
}

fn run(limit: usize, groups: u64, n: u64, fold: bool) -> (Fold, BTreeMap<Vec<u64>, i64>) {
    run_at(&L, 4, limit, groups, n, fold, false)
}

fn run_at(l: &'static RecordLayout, pbits: u8, limit: usize, groups: u64, n: u64, fold: bool, draining: bool) -> (Fold, BTreeMap<Vec<u64>, i64>) {
    let mut buf = region(limit);
    buf[..T1_HDR / 8].fill(0);
    let t = unsafe { T1::at(buf.as_mut_ptr() as *mut u8, l, pbits, 512 << 10, limit as u32) } as *mut T1;
    let mut f = Fold { l, got: BTreeMap::new(), flushes: 0, compactions: 0 };
    let mut want = BTreeMap::new();
    for i in 0..n {
        let g = i % groups;
        let k: Vec<u64> = (0..l.nkeys() as u64).map(|w| mix(g ^ (w << 40))).collect();
        f.row(t, &k, fold, draining);
        *want.entry(k).or_default() += 1;
    }
    let flushes = f.flushes;
    let mut f = f.finish(t);
    f.flushes = flushes;
    (f, want)
}

#[test]
fn groups_that_fit_the_region_never_flush_cyclic_rows() {
    for groups in [100u64, 400, 1_000] {
        let (plain, want) = run(64 << 10, groups, 94_000, false);
        assert_eq!(plain.got, want, "{groups} groups without the fold");
        let (f, want) = run(64 << 10, groups, 94_000, true);
        assert_eq!(f.got, want, "{groups} groups");
        assert_eq!(f.flushes, 0, "{groups} groups fit 64 KiB: no flush ({} compactions; {} flushes without the fold)", f.compactions, plain.flushes);
    }
}

#[test]
fn groups_past_the_region_flush_as_before_and_count_every_row() {
    let (f, want) = run(64 << 10, 20_000, 120_000, true);
    assert_eq!(f.got, want);
    assert!(f.flushes > 0, "20000 groups do not fit 64 KiB");
    assert!(f.compactions >= f.flushes, "every spent pool tries the fold first in this harness");
}

/// D2609: the W8 shape of agg_d2587_hashagg_hash_mem's text cells (64 partitions, a 256 KiB share, a two-word key image)
/// past 6,000 inserts: the pool holds every key several times, and a chain of a few keys each evicted once must not stop
/// the fold of the others (it did: the pool spilled, 53400 under temp_file_limit 0 where PostgreSQL answers).
#[test]
fn a_chain_of_distinct_keys_does_not_stop_the_fold_at_64_partitions() {
    for (groups, n) in [(1_000u64, 47_000u64), (1_000, 94_000), (600, 47_000), (1_500, 47_000)] {
        let (f, want) = run_at(&L2, 6, 256 << 10, groups, n, true, true);
        assert_eq!(f.got, want, "{groups} groups over {n} rows");
        assert!(groups != 1_000 || f.compactions > 0, "{groups} groups over {n} rows: the pool was spent and folded");
        assert_eq!(f.flushes, 0, "{groups} groups fit a 256 KiB share at 64 partitions: no flush ({} compactions)", f.compactions);
    }
}
