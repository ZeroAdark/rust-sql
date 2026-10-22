//! spillopt #5 (hybrid T1, dynamic destaging): a kernel's full phase-1 pool (`SPILL_SCATTER`) writes the partitions it
//! wrote before and the longest others until at most half the pool stays (`scatter_t1_destaging`); the kept partitions
//! stay in memory for the owner phase. Every row is counted once across the file and the kept chains, a partition is
//! written whole or not at all at each flush (ascending in each epoch), and a table whose input passes its pool by less
//! than a pool writes fewer records than whole-pool flushes (`scatter_t1`) do.
use std::collections::BTreeMap;

use gravity_abi::breaker::{KeyKind, Rec, RecordLayout, SlotKind, T1, T1_HDR};
use gravity_abi::hash::KeyHash;
use gravity_driver::mem::spill::{scatter_t1, scatter_t1_destaging, RunReader, SpillFile, DESTAGE_FLUSHES};
use gravity_driver::Result;

const L: RecordLayout = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]);

#[derive(Default)]
struct Mem {
    parts: BTreeMap<u32, Vec<u8>>,
    records: u64,
}

impl SpillFile for Mem {
    fn write_epoch(&mut self, parts: &[(u32, &[u8])]) -> Result<()> {
        assert!(parts.windows(2).all(|w| w[0].0 <= w[1].0), "an epoch names its partitions in order");
        for (p, b) in parts {
            assert_eq!(b.len() % L.size() as usize, 0, "whole records");
            self.records += (b.len() / L.size() as usize) as u64;
            self.parts.entry(*p).or_default().extend_from_slice(b); // alloc: Cold (a test)
        }
        Ok(())
    }
    fn read_part(&self, part: u32, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        self.parts.get(&part).map_or(Ok(()), |b| sink(b))
    }
    fn open_part(&self, _part: u32) -> Result<Option<Box<dyn RunReader>>> {
        Ok(None)
    }
    fn spilled_bytes(&self) -> u64 {
        self.records * L.size() as u64
    }
}

fn mix(j: u64) -> u64 {
    let z = j.wrapping_add(0x2545_F491_4F6C_DD1D);
    let z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Counts by key of `bytes` laid as `L` records, added into `got`; the records' partitions checked.
fn tally(bytes: &[u8], part: Option<u32>, pbits: u8, got: &mut BTreeMap<u64, i64>) {
    let mut rec = vec![0u64; L.size() as usize / 8]; // alloc: Cold (a test)
    for r in bytes.chunks(L.size() as usize) {
        // SAFETY: `rec` holds one record's words.
        let r = unsafe {
            core::ptr::copy_nonoverlapping(r.as_ptr(), rec.as_mut_ptr() as *mut u8, r.len());
            Rec::at(rec.as_mut_ptr() as *mut u8, &L)
        };
        let k = r.key(0);
        if let Some(p) = part {
            assert_eq!(KeyHash::of(&[k]).part(pbits as u32), p, "key {k} in its partition");
        }
        *got.entry(k).or_default() += r.count(0);
    }
}

struct Run {
    want: BTreeMap<u64, i64>,
    got: BTreeMap<u64, i64>,
    written: u64,
    flushes: u32,
    spilled_parts: usize,
    parts: usize,
}

/// Rows over keys into a table of `limit` region bytes at `pbits` (`rows` names the groups and the rows from the pool's
/// records), each full pool flushed by `destage` (else whole pools), then the kernels' finish (drained until the table is
/// empty); the file's records and the kept chains tallied by key.
fn run(limit: usize, pbits: u8, rows: impl Fn(u64) -> (u64, u64), destage: bool) -> Run {
    let mut buf = vec![0u64; limit / 8]; // alloc: Cold (a test)
    buf[..T1_HDR / 8].fill(0);
    let t = unsafe { T1::at(buf.as_mut_ptr() as *mut u8, &L, pbits, 512 << 10, limit as u32) } as *mut T1;
    let (groups, n) = rows(unsafe { (*t).nblocks as u64 * (*t).block_recs as u64 });
    let mut file = Mem::default();
    let mut spilled = Vec::new(); // alloc: Cold (a test)
    let mut want = BTreeMap::new();
    let mut flushes = 0u32;
    let flush = |file: &mut Mem, spilled: &mut Vec<u64>| unsafe {
        if destage {
            scatter_t1_destaging(t, file, spilled).unwrap();
            let (nb, next) = ((*t).nblocks, (*t).next_block);
            assert!(next < nb, "a flush leaves the pool room");
            assert_eq!((*t).len, 0, "a flush drains the table");
        } else {
            scatter_t1(t, 0, pbits, file).unwrap();
        }
    };
    for i in 0..n {
        let k = mix(i % groups);
        let h = KeyHash::of(&[k]);
        unsafe {
            let r = match T1::probe(t, &[k], h) {
                Some(r) => r,
                None => {
                    let home = (*t).home(h);
                    if T1::evict(t, home) {
                        T1::open_at(t, home, &[k])
                    } else {
                        // kernels' `locate`: the flush drained the table, so the row probes again
                        flush(&mut file, &mut spilled);
                        flushes += 1;
                        match T1::probe(t, &[k], h) {
                            Some(r) => r,
                            None => {
                                assert!(T1::evict(t, home), "a flushed pool takes the occupant");
                                T1::open_at(t, home, &[k])
                            }
                        }
                    }
                }
            };
            Rec::at(r, &L).add_count(0, 1);
        }
        *want.entry(k).or_default() += 1;
    }
    unsafe {
        while !T1::drain(t) {
            flush(&mut file, &mut spilled);
            flushes += 1;
        }
    }
    let mut got = BTreeMap::new();
    for (p, b) in &file.parts {
        tally(b, Some(*p), pbits, &mut got);
    }
    let tr = unsafe { &*t };
    let mut kept = 0u64;
    for p in 0..tr.parts() as u32 {
        for b in tr.chain(p) {
            // SAFETY: the block's records lie in `buf`.
            tally(unsafe { core::slice::from_raw_parts(b.ptr, b.recs * L.size() as usize) }, Some(p), pbits, &mut got);
            kept += b.recs as u64;
        }
    }
    assert_eq!(kept, tr.buffered, "buffered counts the kept chains' records");
    Run { want, got, written: file.records, flushes, spilled_parts: file.parts.len(), parts: tr.parts() }
}

#[test]
fn a_destaging_flush_loses_no_row() {
    // repeated keys (partials evicted again and again) and unique keys, from under two pools to past DESTAGE_FLUSHES
    for (groups, n) in [(6_000u64, 60_000u64), (20_000, 120_000), (60_000, 240_000)] {
        let r = run(64 << 10, 4, |_| (groups, n), true);
        assert_eq!(r.got, r.want, "{groups} groups over {n} rows");
        assert!(r.flushes > 0, "{groups} groups pass a 64 KiB region");
    }
    for over in [1.1f64, 1.5, 2.5, 8.0] {
        let r = run(64 << 10, 4, |pool| ((pool as f64 * over) as u64, (pool as f64 * over) as u64), true);
        assert_eq!(r.got, r.want, "unique keys over {over} pools");
        assert!(r.flushes > 0 || over < 1.2, "unique keys over {over} pools");
        assert!(over < 4.0 || r.flushes as u64 > DESTAGE_FLUSHES, "past {DESTAGE_FLUSHES} flushes the pools are written whole");
    }
}

#[test]
fn an_input_past_its_pool_by_less_than_a_pool_writes_less_than_whole_pools() {
    for over in [1.15f64, 1.3] {
        let rows = |pool: u64| ((pool as f64 * over) as u64, (pool as f64 * over) as u64);
        let whole = run(64 << 10, 4, rows, false);
        let hybrid = run(64 << 10, 4, rows, true);
        assert_eq!(whole.got, whole.want, "whole pools: {over} pools");
        assert_eq!(hybrid.got, hybrid.want, "destaging: {over} pools");
        assert!(whole.flushes > 0 && hybrid.flushes > 0, "{over} pools: the pool fills");
        assert!(2 * hybrid.written < 3 * whole.written / 2 && hybrid.written < whole.written, "{over} pools: destaging wrote {} records, whole pools {}", hybrid.written, whole.written);
        assert!(hybrid.spilled_parts < hybrid.parts, "{over} pools: some partitions never written ({} of {})", hybrid.spilled_parts, hybrid.parts);
    }
}
