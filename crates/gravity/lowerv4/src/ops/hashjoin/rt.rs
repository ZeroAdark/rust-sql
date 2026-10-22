//! The build region's runtime (join.inner's build, build.rs's end): `BuildMerge` (the driver's `Merge`, registered as
//! `MergeImpl` under `join.inner`: the workers' records appended into the leader, D254's grow past the grant share, the
//! lay-out once every row is in) over raw.rs's byte work (`Rt` over a region, the merge, the lay-out: settle the images,
//! then the dense, list or KeyHash form); at W >= 2 a large build merges partition-parallel (build_part.rs).
use std::borrow::Cow;

use gravity_driver::{Error, Merge, Partial, PartitionPlan, Region, State};

use crate::face::end::MergeImpl;
use crate::face::kinds::KindId;

use crate::ops::hashjoin::build::{FAILED, FORM, HDR, KIND, N, PHASE};
pub(crate) use crate::ops::hashjoin::raw::{append, dir_need, grow_to, head, headed, lay_out, rd, settle, tail, varlena_len, wr, Rt};

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::JoinInner, &MergeImpl(&BUILD_MERGE))], ..crate::ops::NONE };

/// A lay-out without the room its grow provided (a bug by name; past the grant share the region grows, D254).
pub(crate) fn past_share() -> Error {
    Error::Refused(Cow::Borrowed("hash build region short of its grown size"))
}

/// `defer`: op 14's variant (the BuildSide proto's DEFER bit, HOSTS' `merge_for`), which holds its pipeline's error.
pub(crate) struct BuildMerge {
    defer: bool,
}
pub(crate) static BUILD_MERGE: BuildMerge = BuildMerge { defer: false };
static BUILD_DEFER: BuildMerge = BuildMerge { defer: true };

impl Merge for BuildMerge {
    fn merge_into(&self, leader: &mut State<'_>, partial: Partial<'_>) -> gravity_driver::Result<()> {
        crate::ops::hashjoin::raw::merge_into(leader, partial)
    }

    fn finish(&self, leader: &mut State<'_>) -> gravity_driver::Result<()> {
        crate::ops::hashjoin::raw::finish(leader)
    }

    /// exactsize B1b (raw.rs `presize`).
    fn prepare_serial(&self, leader: &mut State<'_>, partials: &[Partial<'_>]) -> gravity_driver::Result<()> {
        crate::ops::hashjoin::raw::presize(leader, partials)
    }

    /// Decision 5 at W >= 2 (build_part.rs): `None` (the serial merge) for a small build and the serial-only forms.
    fn partition_plan(&self, partials: &[Partial<'_>]) -> Option<PartitionPlan> {
        crate::ops::hashjoin::build_part::plan(partials)
    }

    /// Under the leader's ceiling (the node's hash_mem x its participants): build_part.rs `plan_within`.
    fn partition_plan_within(&self, partials: &[Partial<'_>], ceil: u64) -> Option<PartitionPlan> {
        crate::ops::hashjoin::build_part::plan_within(partials, ceil)
    }

    fn prepare_partitions_in(&self, leader: &mut State<'_>, _plan: PartitionPlan, partials: &[Partial<'_>]) -> gravity_driver::Result<()> {
        crate::ops::hashjoin::build_part::prepare(leader, partials)
    }

    fn merge_partition(&self, leader: Region, part: u32, plan: PartitionPlan, partials: &[Partial<'_>]) -> gravity_driver::Result<()> {
        crate::ops::hashjoin::build_part::merge(leader, part, plan, partials)
    }

    fn room_for(&self, _leader: &[u8], _partial: Partial<'_>) -> bool {
        true
    }

    /// Op 14 (PG's outer-first path): the build's error held for the probe's first row, the region stamped FAILED.
    fn defer_error(&self, leader: &mut State<'_>) -> gravity_driver::Result<bool> {
        if !self.defer || leader.bytes.len() < HDR {
            return Ok(false);
        }
        // the leader's open region, a header long (checked above); ours alone between pipelines
        let d = &mut *leader.bytes;
        (d[15], d[FORM], d[PHASE]) = (KIND, FAILED, 2);
        d[N..N + 8].fill(0);
        // D2717: a spilled build's partitions are never probed (the first row raises the held error before any row is
        // kept): its pair pass reads no partition (spill/load.rs `pair_open` over SPILLED 0); both words lie in the header
        use crate::ops::hashjoin::spill::build::{SPILL, SPILLED};
        leader.bytes[SPILL] = 0;
        leader.bytes[SPILLED..SPILLED + 8].fill(0);
        Ok(true)
    }

    fn defers(&self) -> bool {
        self.defer
    }

    fn with_defer(&'static self, defer: bool) -> Option<&'static dyn Merge> {
        Some(if defer { &BUILD_DEFER } else { &BUILD_MERGE })
    }
}

/// The KeyHash directory's capacity bits for `rows` records on this machine: the cost home's loads (D02 / JOIN-04,
/// `gravity_program::cost::hash`) over the detected machine, the same for every caller in a process (the region's sizing,
/// its lay-out, a pair region's, the partitioned merge's).
pub(crate) fn dir_bits(rows: u64) -> u32 {
    let p = gravity_driver::machine::profile();
    gravity_program::cost::hash::build_cap_bits(rows, &gravity_program::cost::Machine::detected(None, p.l2_bytes, p.smt, p.prefetch))
}

/// JOIN-03: the widest key span `rows` records lay dense on this machine (the cost home's `dense_span_max`, over the
/// detected machine as `dir_bits`): the region's lay-out and the partitioned merge's plan read the same answer.
pub(crate) fn dense_span_max(rows: u64) -> u64 {
    let p = gravity_driver::machine::profile();
    gravity_program::cost::hash::dense_span_max(rows, &gravity_program::cost::Machine::detected(None, p.l2_bytes, p.smt, p.prefetch))
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use crate::ops::hashjoin::build::{ALLOW, A_KEEP, A_MARKS, A_TAGS, DIR, END, FLAGS, FORM, F_RANGE, F_SPLIT, HASH, HDR, HI, KEPT, LO, LOW, MARKS, N, NULLKEY, PHASE, REC, TOP, WORDS};
    use super::{dir_bits, dir_need, lay_out, rd, tail, wr, Rt};
    use crate::ops::hashjoin::raw::kept;

    const LEN: usize = 1 << 16;

    /// A build region in phase 1 of `keys` (None: a kept NULL-key row, L0-d): one image word, one payload word (the row's
    /// number), under `allow`; its bytes after the header pre-filled with 0xAB so a missed zeroing shows.
    fn region(keys: &[Option<i64>], allow: u8) -> Vec<u64> {
        region_of(keys, allow, LEN)
    }

    /// `region` in `len` bytes (a multiple of 8 that holds the records).
    fn region_of(keys: &[Option<i64>], allow: u8, len: usize) -> Vec<u64> {
        let mut buf = vec![0xABAB_ABAB_ABAB_ABABu64; len / 8];
        let d = buf.as_mut_ptr() as *mut u8;
        // SAFETY: `buf` is the `len`-byte region; the records (24 B each) fit below its end.
        unsafe {
            core::ptr::write_bytes(d, 0, HDR);
            (wr::<u8>(d, PHASE, 1), wr::<u8>(d, WORDS, 1), wr::<u32>(d, REC, 24), crate::ops::hashjoin::layout::set_pay(d, 1), wr::<u8>(d, ALLOW, allow), wr::<u8>(d, FLAGS, F_RANGE));
            let some = keys.iter().flatten();
            (wr::<i64>(d, LO, some.clone().copied().min().unwrap_or(i64::MAX)), wr::<i64>(d, HI, some.copied().max().unwrap_or(i64::MIN)));
            (wr::<u64>(d, N, keys.len() as u64), wr::<u64>(d, KEPT, keys.iter().filter(|k| k.is_none()).count() as u64));
            (wr::<u64>(d, TOP, len as u64), wr::<u64>(d, END, len as u64));
            for (i, k) in keys.iter().enumerate() {
                let r = d.add(HDR + 24 * i);
                (wr::<i64>(r, 0, k.unwrap_or(0)), wr::<u64>(r, 8, if k.is_none() { NULLKEY } else { 0 }), wr::<u64>(r, 16, i as u64));
            }
        }
        buf
    }

    /// The rows' numbers of the records `range` (their payload word), sorted.
    unsafe fn rows(d: *mut u8, range: core::ops::Range<usize>) -> Vec<u64> {
        let mut v: Vec<u64> = range.map(|i| rd::<u64>(d.add(HDR + 24 * i), 16)).collect();
        v.sort_unstable();
        v
    }

    /// L0-d: a RIGHT / FULL / RIGHT ANTI build's lay-out moves the NULL-key records past the keyed ones (N the keyed, KEPT
    /// the rest, their links unchained), lays the KeyHash form over the keyed ones only, and zeroes one mark byte per
    /// record before the directory. The reading's count (`kept`) includes them.
    #[test]
    fn a_keeping_build_lays_its_null_key_records_past_the_keyed_ones_with_zeroed_marks() {
        let keys = [Some(5), None, Some(7), None, None, Some(5), Some(9), None];
        let mut buf = region(&keys, A_TAGS | A_KEEP | A_MARKS);
        let d = buf.as_mut_ptr() as *mut u8;
        // SAFETY: the region above, ours alone.
        unsafe {
            lay_out(d, LEN).unwrap();
            let t = Rt::of(d, LEN);
            assert_eq!((t.n, t.kept, kept(d), rd::<u8>(d, FLAGS) & F_SPLIT, rd::<u8>(d, FORM)), (4, 4, 4, F_SPLIT, HASH));
            assert_eq!((rows(d, 0..4), rows(d, 4..8)), (vec![0, 2, 5, 6], vec![1, 3, 4, 7]));
            assert!((4..8).all(|i| *t.link(i) & LOW == 0), "a kept record's link is cleared");
            let m = rd::<u64>(d, MARKS) as usize;
            assert_eq!(m, t.marks_at());
            assert!(m >= HDR + 8 * 24 && (m..m + 8).all(|o| *d.add(o) == 0), "one zeroed mark per record, past the records");
            assert!(rd::<u64>(d, DIR) as usize >= m + 8, "the directory follows the marks");
        }
    }

    /// A keeping build whose NULL-key records take its keyed count down to a sparser load's bound (`dir_bits`): the split leaves the
    /// keyed records at the sparse load, so `dir_need` sizes the heads for them, and a region of exactly the bytes it names
    /// lays out (gen dr-0049 / dr-0060: a spilled FULL partition of 1951 keyed and 1047 NULL-key records failed by name,
    /// "hash build region short of its grown size", when the heads were sized for all 2998).
    #[test]
    fn the_directory_is_sized_for_the_keyed_records_the_split_leaves() {
        let keys: Vec<Option<i64>> = (0..3000i64).map(|i| (i % 3 != 0).then_some(i * 1_000_003)).collect();
        let allow = A_TAGS | A_KEEP | A_MARKS;
        let big = region_of(&keys, allow, 1 << 20);
        // SAFETY: the regions here, ours alone.
        unsafe {
            let t = Rt::of(big.as_ptr() as *mut u8, 1 << 20);
            assert!(dir_need(&t) >= 8 << dir_bits(2000), "heads for the 2000 keyed records at the sparse load");
            let need = (t.dir() + dir_need(&t) + tail(t.d)).next_multiple_of(8);
            let mut buf = region_of(&keys, allow, need);
            let d = buf.as_mut_ptr() as *mut u8;
            lay_out(d, need).expect("a region of dir_need's bytes lays out");
            assert_eq!((rd::<u64>(d, N), kept(d), rd::<u8>(d, FORM)), (2000, 1000, HASH));
        }
    }

    /// L0-d: a build of NULL-key rows alone is not EMPTY (PG's totalTuples counts them, so its outer runs): an empty KeyHash
    /// directory over no keyed record, every record kept; an INNER build of the same keys never keeps (it drops them in
    /// the end), lays no marks and no split.
    #[test]
    fn a_build_of_null_keys_alone_is_not_empty_and_an_inner_build_keeps_its_layout() {
        let mut buf = region(&[None, None, None], A_TAGS | A_KEEP | A_MARKS);
        let d = buf.as_mut_ptr() as *mut u8;
        // SAFETY: the region above, ours alone.
        unsafe {
            lay_out(d, LEN).unwrap();
            assert_eq!((rd::<u64>(d, N), kept(d), rd::<u8>(d, FORM)), (0, 3, HASH));
        }
        let mut buf = region(&[Some(1), Some(2), Some(3)], A_TAGS);
        let d = buf.as_mut_ptr() as *mut u8;
        // SAFETY: as above.
        unsafe {
            lay_out(d, LEN).unwrap();
            let t = Rt::of(d, LEN);
            assert_eq!((t.n, t.kept, t.marks, rd::<u8>(d, FLAGS) & F_SPLIT), (3, 0, false, 0));
            assert_eq!(t.dir(), (HDR + 3 * 24).next_multiple_of(64), "no marks: the directory right past the records");
            assert_eq!(rd::<u64>(d, MARKS), 0, "no MARKS word written");
        }
    }
}
