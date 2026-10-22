//! 16-bit tag in the directory head, road `join.bloomtag` (H23, R5-L16; an AB road: its twin A/B cell decides). Budget
//! 160 lines. Owner M1-JOIN.
//!
//! Each KeyHash directory head keeps, above its record bits, one of 16 tag bits per record in its chain (picked by the
//! record's stored tag, `KeyHash::stored`). A probe whose tag bit is clear in its head misses without touching a record;
//! a set bit is a hint only, never an equality (the chain's images decide).
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering::Relaxed;

use gravity_abi::hash::{Depth, KeyHash};
use gravity_expr::emit::hash::emit_stored;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};

use crate::ops::hashjoin::build::{ALLOW, A_MULT, A_TAGS, BITS, DIR, HASH, IDX, LOW};
use crate::ops::hashjoin::rt::{rd, wr, Rt};

mod raw;
pub(crate) use raw::{lay, put};

/// The road's value: the tags are set by `bit` at lay-out and tested by `admits` in the probe.
pub(crate) struct BloomTag;

crate::strategies! {
    /// 16-bit tag in the directory head.
    JOIN_BLOOMTAG = 53, "join.bloomtag", Join;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(crate::face::strategy::JOIN_BLOOMTAG, &BloomTag)], ..crate::ops::NONE };

/// The first tag bit's position in a head (the record bits are below it).
const TAG_AT: u32 = 48;

/// The head's tag bit of a record whose hash is `h`.
pub(crate) fn bit(h: KeyHash) -> u64 {
    1u64 << (TAG_AT + (h.stored() & 15))
}

/// Whether `head`'s tags admit a probe of hash `h` (`Bool`): its tag bit is set.
pub(crate) fn admits(b: &mut FunctionBuilder<'_>, head: Value, h: Value) -> Value {
    let s = emit_stored(b, h);
    let fifteen = b.iconst(Type::I64, 15);
    let low = b.int_bin(IntBinOp::And, s, fifteen);
    let at = b.iconst(Type::I64, TAG_AT as i64);
    let n = b.int_bin(IntBinOp::Add, low, at);
    let tags = b.int_bin(IntBinOp::Ushr, head, n);
    let one = b.iconst(Type::I64, 1);
    let t = b.int_bin(IntBinOp::And, tags, one);
    let zero = b.iconst(Type::I64, 0);
    b.icmp(IntCond::Ne, t, zero)
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use gravity_abi::hash::{Depth, KeyHash};
    use gravity_driver::{BorrowedBatch, Flow, Merge, Partial, PartitionPlan, Region, Sink, SinkRef, State};

    use crate::ops::hashjoin::build::{ALLOW, A_DENSE, A_TAGS, BITS, DCHAIN, DENSE, DIR, END, FLAGS, FORM, F_RANGE, HASH, HDR, HI, IDX, LO, LOW, N, PHASE, REC, TOP, WORDS};
    use crate::ops::hashjoin::rt::{rd, wr, BUILD_MERGE};

    struct NoRows;
    impl Sink for NoRows {
        fn push(&mut self, _: BorrowedBatch<'_>) -> gravity_driver::Result<Flow> {
            unreachable!("a build's merge pushes no rows")
        }
    }

    /// A worker's region of one-word keys (16-byte records: the key word, the link word).
    fn worker(keys: &[i64], allow: u8) -> Vec<u64> {
        let mut buf = vec![0u64; 512];
        let d = buf.as_mut_ptr() as *mut u8;
        // SAFETY: `buf` is the region (4096 bytes).
        unsafe {
            (wr::<u8>(d, PHASE, 1), wr::<u8>(d, WORDS, 1), wr::<u32>(d, REC, 16), wr::<u8>(d, ALLOW, allow), wr::<u8>(d, FLAGS, F_RANGE));
            (wr::<u64>(d, N, keys.len() as u64), wr::<u64>(d, TOP, 4096), wr::<u64>(d, END, 4096));
            (wr::<i64>(d, LO, *keys.iter().min().unwrap()), wr::<i64>(d, HI, *keys.iter().max().unwrap()));
            keys.iter().enumerate().for_each(|(i, &k)| wr::<i64>(d, HDR + 16 * i, k));
        }
        buf
    }

    /// Two workers' keys merged partition-parallel (8 partitions) and finished: the form, and each probe key's records as
    /// a probe walks them.
    fn merged(a: &[i64], b: &[i64], allow: u8, probes: &[i64]) -> (u8, Vec<usize>) {
        let (wa, wb, mut lead) = (worker(a, allow), worker(b, allow), vec![0u64; 8192]);
        // SAFETY: the workers' and the leader's buffers outlive every view; the leader (64 KiB) needs no grow.
        unsafe {
            let bytes = |w: &Vec<u64>| core::slice::from_raw_parts(w.as_ptr() as *const u8, 4096);
            let partials = [Partial { ordinal: 0, bytes: bytes(&wa), rows_processed: 0 }, Partial { ordinal: 1, bytes: bytes(&wb), rows_processed: 0 }];
            let (d, len, sink) = (lead.as_mut_ptr() as *mut u8, 65536, &mut NoRows);
            let plan = PartitionPlan { pbits: 3, groups: (a.len() + b.len()) as u64, record: 16 };
            BUILD_MERGE.prepare_partitions_in(&mut State::new(core::slice::from_raw_parts_mut(d, len), SinkRef::of(sink)), plan, &partials).unwrap();
            (0..8).for_each(|p| BUILD_MERGE.merge_partition(Region { ptr: d, len }, p, plan, &partials).unwrap());
            BUILD_MERGE.finish(&mut State::new(core::slice::from_raw_parts_mut(d, len), SinkRef::of(sink))).unwrap();
            let (form, lo, dir) = (rd::<u8>(d, FORM), rd::<i64>(d, LO), rd::<u64>(d, DIR) as usize);
            let rec = |i1: u64| d.add(HDR + 16 * (i1 as usize - 1));
            let walk = |mut i1: u64, k: i64| {
                let mut c = 0;
                while i1 != 0 {
                    (c, i1) = (c + (rd::<i64>(rec(i1), 0) == k) as usize, rd::<u64>(rec(i1), 8) & LOW);
                }
                c
            };
            let of = |k: i64| match form {
                DENSE => (rd::<u64>(d, dir + 8 * ((k - lo) as usize >> 6)) >> ((k - lo) & 63) & 1) as usize,
                DCHAIN => walk(rd::<u32>(d, dir + 4 * (k - lo) as usize) as u64, k),
                _ => walk(rd::<u64>(d, dir + 8 * KeyHash::of(&[k as u64]).home(rd::<u32>(d, BITS), Depth::TOP)) & IDX, k),
            };
            (form, probes.iter().map(|&k| of(k)).collect())
        }
    }

    /// A serial build's region of one-word `keys` laid out (rt.rs `lay_out`): the form and, per probe key, its records'
    /// indices in the order a probe walks the key's chain. 32 KiB: the KeyHash directory's floor is 2^10 heads.
    fn chains(keys: &[i64], allow: u8, probes: &[i64]) -> (u8, Vec<Vec<usize>>) {
        const LEN: usize = 32768;
        let mut w = vec![0u64; LEN / 8];
        let d = w.as_mut_ptr() as *mut u8;
        // SAFETY: `w` is the region (LEN bytes) and outlives every view; its records and directory need no grow.
        unsafe {
            (wr::<u8>(d, PHASE, 1), wr::<u8>(d, WORDS, 1), wr::<u32>(d, REC, 16), wr::<u8>(d, ALLOW, allow), wr::<u8>(d, FLAGS, F_RANGE));
            (wr::<u64>(d, N, keys.len() as u64), wr::<u64>(d, TOP, LEN as u64), wr::<u64>(d, END, LEN as u64));
            (wr::<i64>(d, LO, *keys.iter().min().unwrap()), wr::<i64>(d, HI, *keys.iter().max().unwrap()));
            keys.iter().enumerate().for_each(|(i, &k)| wr::<i64>(d, HDR + 16 * i, k));
            crate::ops::hashjoin::rt::lay_out(d, LEN).unwrap();
            let (form, lo, dir) = (rd::<u8>(d, FORM), rd::<i64>(d, LO), rd::<u64>(d, DIR) as usize);
            let rec = |i1: u64| d.add(HDR + 16 * (i1 as usize - 1));
            let walk = |mut i1: u64, k: i64| {
                let mut v = Vec::new();
                while i1 != 0 {
                    if rd::<i64>(rec(i1), 0) == k {
                        v.push(i1 as usize - 1);
                    }
                    i1 = rd::<u64>(rec(i1), 8) & LOW;
                }
                v
            };
            let of = |k: i64| match form {
                DCHAIN => walk(rd::<u32>(d, dir + 4 * (k - lo) as usize) as u64, k),
                _ => walk(rd::<u64>(d, dir + 8 * KeyHash::of(&[k as u64]).home(rd::<u32>(d, BITS), Depth::TOP)) & IDX, k),
            };
            (form, probes.iter().map(|&k| of(k)).collect())
        }
    }

    /// nest.eqkey (A_FIFO, a catalog probe's build): each key's records chain first to last, in the KeyHash and the DCHAIN
    /// forms; without it, last to first (PostgreSQL's bucket order).
    #[test]
    fn a_fifo_build_chains_each_keys_records_in_record_order() {
        use crate::ops::hashjoin::build::A_FIFO;
        let wide = [5, 1_000_000, 5, 7, 5, 1_000_000, 7, 5];
        assert_eq!(chains(&wide, A_TAGS | A_FIFO, &[5, 7, 1_000_000]), (HASH, vec![vec![0, 2, 4, 7], vec![3, 6], vec![1, 5]]));
        assert_eq!(chains(&wide, A_TAGS, &[5, 7, 1_000_000]), (HASH, vec![vec![7, 4, 2, 0], vec![6, 3], vec![5, 1]]));
        let near = [3, 1, 3, 2, 3, 1];
        assert_eq!(chains(&near, A_DENSE | A_FIFO, &[1, 2, 3]), (DCHAIN, vec![vec![1, 5], vec![3], vec![0, 2, 4]]));
        assert_eq!(chains(&near, A_DENSE, &[1, 2, 3]), (DCHAIN, vec![vec![5, 1], vec![3], vec![4, 2, 0]]));
    }

    /// AGA-22: a marking build (a mark indexes its record's address) over unique keys in a dense range lays DCHAIN, its
    /// records in place, not DENSE slots; its partitioned merge lays KeyHash.
    #[test]
    fn a_marking_build_keeps_its_records_in_place() {
        use crate::ops::hashjoin::build::A_MARKS;
        assert_eq!(chains(&[3, 1, 2], A_DENSE, &[1, 2, 3]).0, DENSE);
        assert_eq!(chains(&[3, 1, 2], A_DENSE | A_MARKS, &[1, 2, 3]), (DCHAIN, vec![vec![1], vec![2], vec![0]]));
        assert_eq!(chains(&[3, 1, 3, 2], A_DENSE | A_MARKS, &[1, 2, 3]), (DCHAIN, vec![vec![1], vec![3], vec![2, 0]]));
        assert_eq!(merged(&[3, 1, 2], &[4, 6, 5], A_DENSE | A_MARKS, &[1, 2, 3, 4, 5, 6]), (HASH, vec![1; 6]));
    }

    #[test]
    fn a_partitioned_merge_chains_every_workers_records_in_each_form() {
        assert_eq!(merged(&[1, 1_000_000, 7], &[42, 7, 99_999], A_TAGS, &[1, 7, 42, 99_999, 1_000_000, 5]), (HASH, vec![1, 2, 1, 1, 1, 0]));
        assert_eq!(merged(&[3, 1, 2], &[4, 6, 5], A_DENSE, &[1, 2, 3, 4, 5, 6]), (DENSE, vec![1; 6]));
        assert_eq!(merged(&[3, 1, 2], &[4, 3, 5], A_DENSE, &[1, 2, 3, 4, 5]), (DCHAIN, vec![1, 1, 2, 1, 1]));
    }
}
