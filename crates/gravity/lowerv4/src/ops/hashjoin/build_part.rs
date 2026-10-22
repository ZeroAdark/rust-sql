//! The build's partitioned merge (at W >= 2), `BuildMerge`'s `partition_plan` / `prepare_partitions_in` /
//! `merge_partition`: the leader sized and its directory zeroed once, then each job copies its partition's records and
//! images from the workers, settles them and puts them into the shared directory (the byte work is raw.rs's
//! `prepare_parts` / `merge_part`; the plan reads the partials' headers through raw.rs `heads`).
use gravity_driver::{Partial, PartitionPlan, Region, State};

use crate::ops::hashjoin::build::{A_DENSE, A_KEEP, A_MARKS, A_MULT, F_COMPACT, F_RANGE};
use crate::ops::hashjoin::strategies::dense_bits as bits;

/// Decision 5 at W >= 2: the records' index range cut into partitions, each copied, settled and chained by its own job;
/// `None` (the serial merge) for a small build and for the forms only a serial lay-out takes (join.mult's collapse unless
/// compact: its bits hold unique keys and a duplicate refuses phase 2 into the serial collapse, lever C; a region holding kept NULL-key records, L0-d: the serial lay-out moves them past the keyed ones, a perf item for R-a).
pub(crate) fn plan(partials: &[Partial<'_>]) -> Option<PartitionPlan> {
    // M7-SPILL (V4R-359): a spilled partial's partitions are the leader's to spill too, which only the serial merge does
    if crate::ops::hashjoin::spill::build::any_spilled(partials) {
        return None;
    }
    let heads = || crate::ops::hashjoin::raw::heads(partials);
    let h0 = heads().next()?;
    let n: u64 = heads().map(|h| h.n).sum();
    let (allow, range) = (h0.allow, heads().all(|h| h.flags & F_RANGE != 0));
    let (lo, hi) = heads().fold((i64::MAX, i64::MIN), |(l, h), x| (l.min(x.lo), h.max(x.hi)));
    let compact = h0.flags & F_COMPACT != 0;
    let unbits = compact && (allow & A_DENSE == 0 || !range || bits::span(lo, hi, n as usize).is_none());
    let kept = allow & A_KEEP != 0 && heads().any(|h| h.kept != 0);
    let fifo = allow & crate::ops::hashjoin::build::A_FIFO != 0;
    // join.dedup: the serial lay-out drops a chain's copies (strategies/dedup.rs); the jobs' CAS pushes do not
    let dedup = h0.flags & crate::ops::hashjoin::build::F_DEDUP != 0;
    if allow & A_MULT != 0 && !compact || n >= u32::MAX as u64 || unbits || kept || fifo || dedup {
        return None;
    }
    let dense = allow & (A_DENSE | A_MARKS) == A_DENSE && range && n > 0 && (hi as i128 - lo as i128 + 1) <= crate::ops::hashjoin::rt::dense_span_max(n) as i128;
    gravity_driver::partition_plan(n, table_record(h0.rec, n, !compact && !dense))
}

/// `plan` under the leader's ceiling `ceil` (`grant::leader_ceiling`: hash_mem x the participants; 0 = none): a build that
/// may spill whose leader table would pass it takes the serial fold (`None`), which grows the leader within the ceiling and
/// spills past it (spill/raw/region.rs `merge_into`), as PostgreSQL's Parallel Hash batches at its budget. The partitioned
/// merge sizes the leader for every record at once, by name.
pub(crate) fn plan_within(partials: &[Partial<'_>], ceil: u64) -> Option<PartitionPlan> {
    if ceil != 0 && crate::ops::hashjoin::raw::past_ceiling(partials, ceil) {
        return None;
    }
    plan(partials)
}

/// JOIN-13: the bytes per record of the table the merge lays out, the partition plan's measure of it: a KeyHash table's
/// record and its share of the directory at the cost home's load (rt.rs `dir_bits`); a dense or bits form's record alone
/// (its slots and bitmaps follow the key span, not a load).
fn table_record(rec: u32, n: u64, keyhash: bool) -> u32 {
    let dir = if keyhash { (8u64 << crate::ops::hashjoin::rt::dir_bits(n)).div_ceil(n.max(1)) } else { 0 };
    (rec as u64 + dir).min(u32::MAX as u64) as u32
}

/// The leader headed, grown for every record, the directory and every image, its directory zeroed for the form its
/// jobs chain into (`DENSE` slots or the KeyHash heads) and laid out (`PHASE` 2) before the jobs run.
pub(crate) fn prepare(leader: &mut State<'_>, partials: &[Partial<'_>]) -> gravity_driver::Result<()> {
    crate::ops::hashjoin::raw::prepare_parts(leader, partials)
}

/// Partition `part` of the records' index range: the leader's own records settled in place, each worker's copied and
/// settled against its images' place in the leader (the worker whose first record falls here copies them), then every
/// record chained into the shared directory (`dense::put`, `bloomtag::put`); a compact build's bits come straight from the
/// regions' records (`bits::put`).
pub(crate) fn merge(leader: Region, part: u32, plan: PartitionPlan, partials: &[Partial<'_>]) -> gravity_driver::Result<()> {
    crate::ops::hashjoin::raw::merge_part(leader, part, plan, partials)
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use gravity_driver::{BorrowedBatch, Error, Flow, Partial, PartitionPlan, Region, Sink, SinkRef, State};

    use crate::ops::hashjoin::build::{ALLOW, A_DENSE, DBITS, DIR, END, FLAGS, FORM, FULL, F_COMPACT, F_RANGE, HDR, HI, LO, N, PHASE, REC, SLOTS, TOP, WORDS};
    use crate::ops::hashjoin::rt::{rd, wr};

    struct NoRows;
    impl Sink for NoRows {
        fn push(&mut self, _: BorrowedBatch<'_>) -> gravity_driver::Result<Flow> {
            unreachable!("a build's merge pushes no rows")
        }
    }

    /// A worker's compact region of `keys` (8 B records: the key's low 32 bits, the payload's value bit 32 = the key odd).
    fn worker(keys: &[i64]) -> Vec<u64> {
        worker_as(keys, A_DENSE, 1)
    }

    /// `worker` under `allow` with `pay` payload columns (join.mult's: none, `A_MULT`).
    fn worker_as(keys: &[i64], allow: u8, pay: u8) -> Vec<u64> {
        let mut buf = vec![0u64; 512];
        let d = buf.as_mut_ptr() as *mut u8;
        // SAFETY: `buf` is the region (4096 bytes).
        unsafe {
            (wr::<u8>(d, PHASE, 1), wr::<u8>(d, WORDS, 1), wr::<u32>(d, REC, 8), wr::<u32>(d, FULL, 16 + 8 * pay as u32), crate::ops::hashjoin::layout::set_pay(d, pay as usize));
            (wr::<u8>(d, ALLOW, allow), wr::<u8>(d, FLAGS, F_RANGE | F_COMPACT), wr::<u64>(d, N, keys.len() as u64));
            (wr::<u64>(d, TOP, 4096), wr::<u64>(d, END, 4096), wr::<i64>(d, LO, *keys.iter().min().unwrap()), wr::<i64>(d, HI, *keys.iter().max().unwrap()));
            keys.iter().enumerate().for_each(|(i, &k)| wr::<u64>(d, HDR + 8 * i, (k as u64 & 0xffff_ffff) | ((k & 1) as u64 * pay as u64) << 32));
        }
        buf
    }

    /// Lever (b): the partition jobs put the bits straight from the workers' records and copy none into the leader; a
    /// duplicate key (across workers) refuses phase 2, so the driver's serial fold copies and expands the records.
    #[test]
    fn a_compact_build_puts_its_bits_from_the_workers_and_a_duplicate_refuses_phase_2() {
        for dup in [false, true] {
            let (a, mut b): (Vec<i64>, Vec<i64>) = ((0..200).filter(|k| k % 3 == 0).collect(), (0..200).filter(|k| k % 3 == 1).collect());
            if dup {
                b[40] = a[7];
            }
            let (wa, wb, mut lead) = (worker(&a), worker(&b), vec![0u64; 8192]);
            // SAFETY: the workers' and the leader's buffers outlive every view; the leader (64 KiB) needs no grow.
            unsafe {
                let bytes = |w: &Vec<u64>| core::slice::from_raw_parts(w.as_ptr() as *const u8, 4096);
                let partials = [Partial { ordinal: 0, bytes: bytes(&wa), rows_processed: 0 }, Partial { ordinal: 1, bytes: bytes(&wb), rows_processed: 0 }];
                let (d, len, sink) = (lead.as_mut_ptr() as *mut u8, 65536, &mut NoRows);
                let plan = PartitionPlan { pbits: 3, groups: (a.len() + b.len()) as u64, record: 8 };
                super::prepare(&mut State::new(core::slice::from_raw_parts_mut(d, len), SinkRef::of(sink)), &partials).unwrap();
                assert_eq!((rd::<u8>(d, FORM), rd::<u8>(d, PHASE)), (DBITS, 2));
                let refused = (0..8).filter(|&p| matches!(super::merge(Region { ptr: d, len }, p, plan, &partials), Err(Error::Unsupported(_)))).count();
                assert_eq!(refused > 0, dup, "a duplicate refuses phase 2");
                assert!((HDR..HDR + 8 * (a.len() + b.len())).all(|o| *d.add(o) == 0), "no record copied into the leader");
                let (dir, stride, lo) = (rd::<u64>(d, DIR) as usize, (rd::<u64>(d, SLOTS) - rd::<u64>(d, DIR)) as usize, rd::<i64>(d, LO));
                let bit = |j: usize, k: i64| rd::<u64>(d.add(dir + j * stride + 8 * ((k - lo) as usize >> 6)), 0) >> ((k - lo) & 63) & 1 == 1;
                for k in (0..200).filter(|_| !dup) {
                    assert_eq!((bit(0, k), bit(1, k), bit(2, k)), (k % 3 != 2, k % 3 != 2 && k & 1 == 1, false), "key {k}");
                }
            }
        }
    }

    /// Lever C: a join.mult build (no payload, `A_MULT`) is compact and partitioned: unique keys lay the bits (a present key
    /// reads as a count of 1); a duplicate, in one worker or across two, refuses phase 2, and the serial fold (the driver's
    /// `merge_into` + `finish`) expands the records and collapses equal keys into counts (the DENSE slots' link words).
    #[test]
    fn a_count_only_build_lays_the_bits_and_a_duplicate_collapses_into_counts() {
        use gravity_driver::Merge;

        use crate::ops::hashjoin::build::{A_MULT, DENSE};
        let (a, b): (Vec<i64>, Vec<i64>) = ((0..200).filter(|k| k % 3 == 0).collect(), (0..200).filter(|k| k % 3 == 1).collect());
        let mut in_a = a.clone();
        in_a[9] = a[4];
        let mut across = b.clone();
        across[40] = a[7];
        for (case, wa_keys, wb_keys, dup) in [("unique", &a, &b, None), ("in one worker", &in_a, &b, Some(a[4])), ("across workers", &a, &across, Some(a[7]))] {
            let (wa, wb, mut lead) = (worker_as(wa_keys, A_DENSE | A_MULT, 0), worker_as(wb_keys, A_DENSE | A_MULT, 0), vec![0u64; 8192]);
            // SAFETY: the workers' and the leader's buffers outlive every view; the leader (64 KiB) needs no grow.
            unsafe {
                let bytes = |w: &Vec<u64>| core::slice::from_raw_parts(w.as_ptr() as *const u8, 4096);
                let partials = [Partial { ordinal: 0, bytes: bytes(&wa), rows_processed: 0 }, Partial { ordinal: 1, bytes: bytes(&wb), rows_processed: 0 }];
                assert_eq!(super::plan(&partials), gravity_driver::partition_plan((a.len() + b.len()) as u64, 8), "{case}: join.mult's compact build is partitioned");
                let (d, len, sink) = (lead.as_mut_ptr() as *mut u8, 65536, &mut NoRows);
                let plan = PartitionPlan { pbits: 3, groups: (a.len() + b.len()) as u64, record: 8 };
                super::prepare(&mut State::new(core::slice::from_raw_parts_mut(d, len), SinkRef::of(sink)), &partials).unwrap();
                let refused = (0..8).filter(|&p| matches!(super::merge(Region { ptr: d, len }, p, plan, &partials), Err(Error::Unsupported(_)))).count();
                assert_eq!(refused > 0, dup.is_some(), "{case}: a duplicate refuses phase 2");
                // each key's count over both workers' records
                let want = |k: i64| wa_keys.iter().chain(wb_keys.iter()).filter(|&&x| x == k).count() as u64;
                if dup.is_none() {
                    // no payload: the exist bitmap alone
                    let (lo, dir) = (rd::<i64>(d, LO), rd::<u64>(d, DIR) as usize);
                    for k in 0..200i64 {
                        let o = (k - lo) as usize;
                        assert_eq!(rd::<u64>(d, dir + 8 * (o >> 6)) >> (o & 63) & 1, want(k), "{case} key {k}: present 1, absent 0");
                    }
                    continue;
                }
                // the driver's serial fold over a fresh leader
                lead.iter_mut().for_each(|w| *w = 0);
                let mut st = State::new(core::slice::from_raw_parts_mut(d, len), SinkRef::of(sink));
                for &p in &partials {
                    crate::ops::hashjoin::rt::BUILD_MERGE.merge_into(&mut st, p).unwrap();
                }
                crate::ops::hashjoin::rt::BUILD_MERGE.finish(&mut st).unwrap();
                assert_eq!((rd::<u8>(d, FORM), rd::<u8>(d, PHASE)), (DENSE, 2), "{case}: the records expanded into DENSE");
                let (lo, dir, slots) = (rd::<i64>(d, LO), rd::<u64>(d, DIR) as usize, rd::<u64>(d, SLOTS) as usize);
                let srec = (rd::<u32>(d, REC) - 8) as usize;
                for k in 0..200i64 {
                    let o = (k - lo) as usize;
                    let set = rd::<u64>(d, dir + 8 * (o >> 6)) >> (o & 63) & 1 != 0;
                    let count = if set { (rd::<u64>(d, slots + o * srec) >> 32) + 1 } else { 0 };
                    assert_eq!(count, want(k), "{case} key {k}");
                }
            }
        }
    }

    /// A worker's keeping region (L0-d: a RIGHT / FULL / RIGHT ANTI build, `A_KEEP | A_MARKS`, or RIGHT SEMI's `A_MARKS`)
    /// of `keys` (None: a NULL-key row), 24 B records (the image word, the link, the payload word: `tag` + the row), phase 1.
    fn keeping(keys: &[Option<i64>], allow: u8, tag: u64) -> Vec<u64> {
        use crate::ops::hashjoin::build::{KEPT, NULLKEY};
        let mut buf = vec![0u64; 512];
        let d = buf.as_mut_ptr() as *mut u8;
        // SAFETY: `buf` is the region (4096 bytes); the records fit below its end.
        unsafe {
            (wr::<u8>(d, PHASE, 1), wr::<u8>(d, WORDS, 1), wr::<u32>(d, REC, 24), crate::ops::hashjoin::layout::set_pay(d, 1), wr::<u8>(d, ALLOW, allow), wr::<u8>(d, FLAGS, F_RANGE));
            (wr::<u64>(d, N, keys.len() as u64), wr::<u64>(d, KEPT, keys.iter().filter(|k| k.is_none()).count() as u64), wr::<u64>(d, TOP, 4096), wr::<u64>(d, END, 4096));
            (wr::<i64>(d, LO, keys.iter().flatten().copied().min().unwrap_or(i64::MAX)), wr::<i64>(d, HI, keys.iter().flatten().copied().max().unwrap_or(i64::MIN)));
            for (i, k) in keys.iter().enumerate() {
                let r = d.add(HDR + 24 * i);
                (wr::<i64>(r, 0, k.unwrap_or(0)), wr::<u64>(r, 8, if k.is_none() { NULLKEY } else { 0 }), wr::<u64>(r, 16, tag + i as u64));
            }
        }
        buf
    }

    /// L0-d at W >= 2: a partial holding NULL-key records takes the serial merge (its lay-out moves them past the keyed
    /// ones; R-a's perf item); partials with none keep decision 5's plan. The serial fold of two workers keeps every row:
    /// the keyed ones first, both workers' NULL-key rows after, one zeroed mark per record.
    #[test]
    fn a_null_key_partial_takes_the_serial_merge_and_keeps_every_row() {
        use gravity_driver::Merge;

        use crate::ops::hashjoin::build::{A_KEEP, A_MARKS, A_TAGS, HASH, KEPT, MARKS};
        let (allow, big) = (A_TAGS | A_KEEP | A_MARKS, 1u64 << 30);
        let (a, b) = ([Some(1), None, Some(2)], [None, Some(3), Some(1)]);
        let (mut wa, mut wb, mut wc) = (keeping(&a, allow, 0), keeping(&b, allow, 100), keeping(&[Some(4), Some(5), Some(6)], allow, 200));
        // SAFETY: the workers' and the leader's buffers outlive every view; the leader (64 KiB) needs no grow.
        unsafe {
            let bytes = |w: &mut Vec<u64>| core::slice::from_raw_parts(w.as_ptr() as *const u8, 4096);
            // the plan reads the headers only: a large N makes decision 5 worth it unless a partial keeps NULL-key records
            let (pa, pb) = (wa.as_mut_ptr() as *mut u8, wc.as_mut_ptr() as *mut u8);
            let (na, nc) = (rd::<u64>(pa, N), rd::<u64>(pb, N));
            (wr::<u64>(pa, N, big), wr::<u64>(pb, N, big));
            let two = |x: &mut Vec<u64>, y: &mut Vec<u64>| [Partial { ordinal: 0, bytes: bytes(x), rows_processed: 0 }, Partial { ordinal: 1, bytes: bytes(y), rows_processed: 0 }];
            let mut wc2 = wc.clone();
            assert_eq!(super::plan(&two(&mut wc, &mut wc2)), gravity_driver::partition_plan(2 * big, super::table_record(24, 2 * big, true)), "no NULL-key record: decision 5 as before");
            assert_eq!(super::plan(&two(&mut wa, &mut wc2)), None, "a NULL-key partial: the serial merge");
            (wr::<u64>(pa, N, na), wr::<u64>(pb, N, nc));
            let mut lead = vec![0u64; 8192];
            let (d, sink) = (lead.as_mut_ptr() as *mut u8, &mut NoRows);
            let mut st = State::new(core::slice::from_raw_parts_mut(d, 65536), SinkRef::of(sink));
            for p in two(&mut wa, &mut wb) {
                crate::ops::hashjoin::rt::BUILD_MERGE.merge_into(&mut st, p).unwrap();
            }
            crate::ops::hashjoin::rt::BUILD_MERGE.finish(&mut st).unwrap();
            let d = st.bytes.as_mut_ptr();
            assert_eq!((rd::<u64>(d, N), rd::<u64>(d, KEPT), rd::<u8>(d, FORM)), (4, 2, HASH));
            let row = |i: usize| rd::<u64>(d.add(HDR + 24 * i), 16);
            let (mut keyed, mut kept): (Vec<u64>, Vec<u64>) = ((0..4).map(row).collect(), (4..6).map(row).collect());
            (keyed.sort_unstable(), kept.sort_unstable());
            assert_eq!((keyed, kept), (vec![0, 2, 101, 102], vec![1, 100]));
            let m = rd::<u64>(d, MARKS) as usize;
            assert!(m >= HDR + 6 * 24 && (m..m + 6).all(|o| *d.add(o) == 0));
        }
    }

    /// L0-d: RIGHT SEMI's build (marks, no NULL-key rows) keeps decision 5; the leader's prepare zeroes its marks before the
    /// directory.
    #[test]
    fn a_partitioned_marking_build_zeroes_its_marks_in_the_prepare() {
        use crate::ops::hashjoin::build::{A_MARKS, A_TAGS, HASH, MARKS};
        let keys: Vec<Option<i64>> = (0..60).map(Some).collect();
        let (wa, wb) = (keeping(&keys[..30], A_TAGS | A_MARKS, 0), keeping(&keys[30..], A_TAGS | A_MARKS, 30));
        let mut lead = vec![u64::MAX; 8192];
        // SAFETY: the workers' and the leader's buffers outlive every view; the leader (64 KiB) needs no grow.
        unsafe {
            let bytes = |w: &Vec<u64>| core::slice::from_raw_parts(w.as_ptr() as *const u8, 4096);
            let partials = [Partial { ordinal: 0, bytes: bytes(&wa), rows_processed: 0 }, Partial { ordinal: 1, bytes: bytes(&wb), rows_processed: 0 }];
            let (d, sink) = (lead.as_mut_ptr() as *mut u8, &mut NoRows);
            core::ptr::write_bytes(d, 0, HDR);
            super::prepare(&mut State::new(core::slice::from_raw_parts_mut(d, 65536), SinkRef::of(sink)), &partials).unwrap();
            let plan = PartitionPlan { pbits: 2, groups: 60, record: 24 };
            (0..4).for_each(|p| super::merge(Region { ptr: d, len: 65536 }, p, plan, &partials).unwrap());
            let m = rd::<u64>(d, MARKS) as usize;
            assert_eq!(rd::<u8>(d, FORM), HASH);
            assert!(m >= HDR + 60 * 24 && (m..m + 60).all(|o| *d.add(o) == 0) && rd::<u64>(d, DIR) as usize >= m + 60);
        }
    }
}
