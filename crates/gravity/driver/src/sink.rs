use gravity_abi::Datum;
pub use gravity_program::cost::runtime::MAX_WIRE_ATTS;

use crate::Result;

mod raw;
pub use raw::{LegClaims, LegRef, SinkCalls, SinkRef};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
}

#[derive(Clone, Copy, Debug)]
pub struct BorrowedBatch<'a> {
    pub natts: usize,
    pub rows: usize,
    pub values: &'a [Datum],
    pub isnull: &'a [u8],
    /// The block's by-reference region (lower.md §15.13): the bytes a by-reference datum of `values` may
    /// point into, valid for this push only; a sink that keeps the datums past it copies and relocates (`PullBuffer`). Empty when no column is by-reference.
    pub region: &'a [u8],
}

impl<'a> BorrowedBatch<'a> {
    pub fn row(&self, i: usize) -> (&'a [Datum], &'a [u8]) {
        let n = self.natts;
        (&self.values[i * n..(i + 1) * n], &self.isnull[i * n..(i + 1) * n])
    }

    pub fn iter(&self) -> impl Iterator<Item = (&'a [Datum], &'a [u8])> + '_ {
        (0..self.rows).map(|i| self.row(i))
    }
}

/// Bound once: `Binding::new` resolves `Program.push` to `hostimpl::sink::push::<Self>` and the block's `sink` word to `self` (abi-v2 §2.3); `leg` is a worker's share, resolved the same at fork.
pub trait Sink {
    fn push(&mut self, batch: BorrowedBatch<'_>) -> Result<Flow>;

    /// `Some` when the sink is a text wire whose every column a leg can frame itself (`push_wire`).
    fn wire_format(&self) -> Option<WireFormat> {
        None
    }
    /// `rows` complete DataRow messages framed per `wire_format`, appended to the wire in one pass.
    fn push_wire(&mut self, _bytes: &[u8], _rows: u64) -> Result<Flow> {
        Err(crate::Error::host("gravity: a wire push on a sink without a wire format"))
    }
    fn leg(&self, _ordinal: u16) -> Option<LegRef<'_>> {
        None
    }
    /// R1D: the legs emit claim by claim in claim order while a section runs (`drain_ready`, `order::ClaimOrder`).
    fn ordered(&self) -> bool {
        false
    }
    /// The leader mid-section: the finished claims starting at `los` (claim order) emitted from their legs.
    fn drain_ready(&mut self, _los: &[u64]) -> Result<Flow> {
        Ok(Flow::Continue)
    }
    /// The bytes of settled claims waiting in the legs (the ordered output's window, held in G by the leader).
    fn buffered(&self) -> u64 {
        0
    }
}

/// The wire's own row framing when every output column is a text-format integer (`kinds[i]` = 2 / 4 / 8, the int width): a leg frames its rows itself on the worker thread and the leader
/// hands the bytes to `Sink::push_wire` — the 100 k-row join emit was a 7.7 ms serial leader epilogue at every width (VM, W = 4 / 8 / 16) when the leader formatted each row itself.
#[derive(Clone, Copy, Debug)]
pub struct WireFormat {
    pub natts: u8,
    pub kinds: [u8; MAX_WIRE_ATTS],
}

pub trait SinkLeg: Sync {
    fn push(&self, batch: BorrowedBatch<'_>) -> Result<Flow>;
    /// A claim starts on this leg's worker thread: its pushes until `claim_end` are that claim's rows (R1D: drained in ascending `claim.lo`).
    fn claim_begin(&self, _claim: gravity_abi::MorselRange) {}
    /// The claim settled (its pins dropped, its scratch rewound next); `ok` is false when it failed.
    fn claim_end(&self, _ok: bool) {}
}

pub struct State<'a> {
    pub bytes: &'a mut [u8],
    pub sink: SinkRef<'a>,
    /// D254: the leader's slot behind `bytes` (`grow` reopens it); `None` = a fixed view.
    pub(crate) home: Option<crate::mem::grow::Leader>,
}

impl<'a> State<'a> {
    /// A fixed view: `grow` refuses by name.
    pub fn new(bytes: &'a mut [u8], sink: SinkRef<'a>) -> State<'a> {
        State { bytes, sink, home: None }
    }

    pub(crate) fn growable(bytes: &'a mut [u8], sink: SinkRef<'a>, home: crate::mem::grow::Leader) -> State<'a> {
        State { bytes, sink, home: Some(home) }
    }

    /// D254: the leader's slot reopened at max(2 x len, `bytes`) when shorter than `bytes` (W, then G, then past both; only a real
    /// allocation failure errors, OutOfMemory), the whole old region copied, the header untouched; `bytes` the new slice.
    pub fn grow(&mut self, bytes: usize) -> Result<()> {
        crate::mem::grow::grow_state(self, bytes)
    }

    /// C10: `grow` (`grow_exact`'s when `counted`) on a spilling breaker's road that still grows past its grant, the debt
    /// named by the SPILL-DEBT row `why` (counted as that row's, never as an unnamed road's).
    pub fn grow_debt(&mut self, bytes: usize, counted: Option<u64>, why: gravity_program::spill::NoSpill) -> Result<()> {
        crate::mem::grow::grow_state_debt(self, bytes, counted, why)
    }

    /// M7-SPILL (V4R-359): `grow`, only within the statement's G: `false` (nothing drawn) past it, so a kind-7 merge spills.
    pub fn grow_in(&mut self, bytes: usize) -> Result<bool> {
        crate::mem::grow::grow_state_in(self, bytes)
    }

    /// exactsize B1a: `grow` where `bytes` is counted (a hash build's end: its `counted` records, their directory and
    /// images): under `pgrust.gravity_exact_size` = build | all the region grows to `bytes` page-rounded, never doubled
    /// (`mem::grow::exact_len`), in place when its reservation holds it; else `grow`, byte for byte.
    pub fn grow_exact(&mut self, bytes: usize, counted: u64) -> Result<()> {
        crate::mem::grow::grow_exact(self, bytes, counted, false).map(|_| ())
    }

    /// exactsize B1a: `grow_in` where `bytes` is counted, as `grow_exact` sizes it.
    pub fn grow_in_exact(&mut self, bytes: usize, counted: u64) -> Result<bool> {
        crate::mem::grow::grow_exact(self, bytes, counted, true)
    }

    /// exactsize B1a: whether this statement sizes its hash builds from their count (`Grant::exact_build`).
    pub fn exact_build(&self) -> bool {
        crate::mem::grow::exact_build(self)
    }

    /// D3625: PostgreSQL's hash_mem for this statement's hash nodes (`Grant::hash_ceil`, get_hash_memory_limit() as exec read
    /// it for the grant); 0 when the leader has no statement reservation or its grant carries no ceilings.
    pub fn hash_ceiling(&self) -> u64 {
        crate::mem::grow::hash_ceiling(self)
    }

    /// exactsize B1d: about the most bytes `grow_in` takes the leader slot to now (its reservation in place, else its
    /// ceiling's region bytes under a ceiling, else the statement's G left; unbounded with no admitted reservation, where
    /// `grow_in` grows as `grow`). A fit's target, never a promise: the draw itself may still be refused.
    pub fn room_in(&self) -> u64 {
        crate::mem::grow::room_in(self)
    }

    /// M7-SPILL (V4R-359): `parts` written as one epoch of the leader's stored file of this State's slot at `bits`.
    pub fn spill_write(&mut self, bits: u8, parts: &[(u32, &[u8])]) -> Result<()> {
        crate::mem::grow::spill_write(self, bits, parts)
    }

    /// CONVERT1: the ledger owner of the leader's slot behind this State (LEDGER2 step 5's breaker owner), which a merge's
    /// working memory charges (gravity_fn `OpArena::scratch_of`); `None` for a fixed view or a slot with no owner.
    pub fn op_owner(&self) -> Option<memledger::Owner> {
        crate::mem::grow::op_owner(self)
    }
}

#[derive(Clone, Copy)]
pub struct Partial<'a> {
    pub ordinal: u16,
    pub bytes: &'a [u8],
    pub rows_processed: u64,
}

/// A breaker region several merge jobs write at once, each inside its own partitions: a raw pointer, never a shared slice (writes through a `&[u8]` are dropped by the optimizer).
#[derive(Clone, Copy, Debug)]
pub struct Region {
    pub ptr: *mut u8,
    pub len: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartitionPlan {
    pub pbits: u8,
    pub groups: u64,
    pub record: u32,
}

/// The destination slice (Graviton4's 2 MiB L2; `machine::profile` on others); `PGRUST_GRAVITY_PARTITION_BYTES` overrides it, 0 = unpartitioned.
pub const CACHE_PARTITION_BYTES: u64 = 2 << 20;
const _: () = assert!(CACHE_PARTITION_BYTES == crate::machine::REFERENCE_L2);

pub fn partition_bytes() -> u64 {
    static ONCE: pgsync::OnceLock<u64> = pgsync::OnceLock::new();
    *ONCE.get_or_init(|| {
        let v = crate::env::num("PGRUST_GRAVITY_PARTITION_BYTES").unwrap_or_else(|| crate::machine::profile().partition_bytes);
        // DR-6: the kernels' fold opens re-cut their bits to the slice the runner's bind reads (a quarter; 0: the reference's)
        gravity_abi::breaker::set_machine_slice(v / 4);
        v
    })
}

/// The gate: `groups × record > slice / 2`; `P = next_pow2(2 × groups × record / slice)`, capped at the route's partition
/// bound `T1_MAX_PARTS` for every record width (the format fact the split depth and the merge's group estimate assume;
/// past it a partition holds more than a slice and its table grows or spills).
pub fn partition_plan(groups: u64, record: u32) -> Option<PartitionPlan> {
    let slice = partition_bytes();
    let bytes = groups.saturating_mul(record as u64);
    if slice == 0 || bytes <= slice / 2 {
        return None;
    }
    let p = (bytes.saturating_mul(2) / slice).next_power_of_two().min(gravity_abi::breaker::T1_MAX_PARTS as u64).max(2);
    Some(PartitionPlan { pbits: p.trailing_zeros() as u8, groups, record })
}

pub type Flush<'a> = &'a mut dyn FnMut() -> Result<()>;

pub trait Merge: Sync {
    /// The leader's serial fold of one partial (the plain aggregate, the top-k run, the ordered rows, the join's build); a record table has none: its partials are the owners'.
    fn merge_into(&self, _leader: &mut State<'_>, _partial: Partial<'_>) -> Result<()> {
        Err(crate::Error::Unsupported(gravity_abi::status::UNSUPPORTED_SCRATCH))
    }

    /// The phase-1 tables' partition count (the owner phase's units; the width-1 arm folds through one owner, `Entry::own_single`).
    fn partition_bits(&self, _partials: &[Partial<'_>]) -> Option<u8> {
        None
    }

    fn finish(&self, _leader: &mut State<'_>) -> Result<()> {
        Ok(())
    }

    /// exactsize B1b: called once before a hash build's serial fold (`merge_into` of each partial), with every partial: a
    /// merge whose whole need its partials' headers count sizes the leader once (`State::grow_exact`) instead of growing it
    /// per partial. Default: nothing.
    fn prepare_serial(&self, _leader: &mut State<'_>, _partials: &[Partial<'_>]) -> Result<()> {
        Ok(())
    }

    /// D3259: before the section end (no worker running), whether the partials merged would pass what the merge lays in
    /// one region (a sort's sealed runs past `RESIDENT_MAX`); filed on the produced slot's stream (`SPILL_SEAL_OVER`) so
    /// every worker's section end takes the same branch (kernels sort `resident`: each puts its own run). Default: never.
    fn seal_over(&self, _partials: &[Partial<'_>]) -> bool {
        false
    }

    /// D4373: the rows `seal_over` read (every partial's live run), so a top's section end knows whether its cut keeps every
    /// row of them (kernels sort `resident_top`); `None`: not known (the top keeps its own branch). Default: not known.
    fn seal_rows(&self, _partials: &[Partial<'_>]) -> Option<u64> {
        None
    }

    /// Decision 5: `Some` when the partials are worth folding partition-parallel.
    fn partition_plan(&self, _partials: &[Partial<'_>]) -> Option<PartitionPlan> {
        None
    }

    /// `partition_plan` with the leader slot's ceiling `ceil` known (`SlotRegion::ceil`, 0 = none): a merge whose leader
    /// table would pass it answers `None`, so the serial fold grows within the ceiling and spills past it.
    fn partition_plan_within(&self, partials: &[Partial<'_>], _ceil: u64) -> Option<PartitionPlan> {
        self.partition_plan(partials)
    }

    fn prepare_partitions(&self, _leader: &mut [u8], _plan: PartitionPlan, _partials: &[Partial<'_>]) -> Result<()> {
        Ok(())
    }

    /// Phase 2's layout over a leader that may grow first (D254: `leader.grow`, before the jobs; its sink refuses a push); the
    /// default lays out in place.
    fn prepare_partitions_in(&self, leader: &mut State<'_>, plan: PartitionPlan, partials: &[Partial<'_>]) -> Result<()> {
        self.prepare_partitions(leader.bytes, plan, partials)
    }

    fn room_for(&self, _leader: &[u8], _partial: Partial<'_>) -> bool {
        true
    }

    /// Phase 2a: each partial prepared on its own thread before the partition jobs (`prepares_partials` gates it).
    fn prepares_partials(&self) -> bool {
        false
    }

    fn prepare_partial(&self, _partial: Region, _plan: PartitionPlan) -> Result<()> {
        Ok(())
    }

    /// One partition of every partial into the shared destination; W threads, disjoint partitions.
    fn merge_partition(&self, _leader: Region, _part: u32, _plan: PartitionPlan, _partials: &[Partial<'_>]) -> Result<()> {
        Err(crate::Error::Unsupported(gravity_abi::status::UNSUPPORTED_SCRATCH))
    }

    /// The bound a worker's partial proves after a claim (the zone walk, §1.10 increment 3 step 5): no row or group with a key past it can still be among the statement's answer. `None` for breakers without one.
    fn bound(&self, _partial: Partial<'_>) -> Option<i64> {
        None
    }

    /// Phase 2 owned per partition (partitioned-record.md §3.2): `Some(pbits)`, and no leader fold follows.
    fn owned_partitions(&self, _partials: &[Partial<'_>], _owner_bytes: u32) -> Option<u8> {
        None
    }

    /// D2724: the least region ONE owner opens (its form's `OwnerFns::min_bytes`); `None` without an owner form. Unlike
    /// `owned_partitions`, which admits a pool whose owners hold a one-round floor only together, each one under it.
    fn owner_floor(&self) -> Option<u64> {
        None
    }

    fn owned_rounds(&self) -> u8 {
        1
    }

    /// D3075: `probe`'s answer (`section::kept_run`: the kept run's word of a count / top-k owner form, probed by opens
    /// over a scratch region), which a merge whose owner form is fixed for the execution keeps after its first probe: a
    /// rescanned owner phase probed it at every run. The default probes every time.
    fn kept_run_memo(&self, probe: &mut dyn FnMut() -> Option<u64>) -> Option<u64> {
        probe()
    }

    /// D3111: every partition's record bytes over the one-round owner's sources (`own_partition`'s: the partials' chains
    /// and tables, the files), `parts[p]` for partition `p < 1 << pbits`, when the merge can count them; `false`: unknown
    /// (the owner sizes its table from the partitions' average).
    fn part_record_bytes(&self, _partials: &[Partial<'_>], _files: &[&dyn crate::SpillFile], _pbits: u8, _parts: &mut [u64]) -> bool {
        false
    }

    /// D2988 (C): the partitions some source holds a record of, bit `p % 64` of `mask[p / 64]`, when the merge can say so
    /// from memory alone (no file, nothing it cannot read): the one-round owner phase then folds only those, the others
    /// being the empty folds `own_partition` makes of a partition without records. `false` = fold every partition.
    fn held_parts(&self, _partials: &[Partial<'_>], _files: &[&dyn crate::SpillFile], _pbits: u8, _mask: &mut [u64]) -> bool {
        false
    }

    /// After an owner's last round-1 partition: its round-2 buffers closed (`round2_pbits` reads their count).
    fn end_round(&self, _owner: Region, _flush: Flush<'_>) -> Result<()> {
        Ok(())
    }

    fn round2_pbits(&self, _owner: Region) -> u8 {
        0
    }

    fn round2_partial(&self, _owner: Region) -> Option<Region> {
        None
    }

    fn open_owner(&self, _owner: Region, _ordinal: u16) -> Result<()> {
        Ok(())
    }

    /// Partition `part` of every partial and of every spill file's epochs folded into the owner's
    /// table and reduced; `site` renders text keys; `flush` spills full round-2 buffers (`REDUCE_FLUSH`).
    /// `cancel`: the statement's abort byte (the owner ctx's `Ctx::cancel`; null: none), checked per source block: a
    /// pending cancel or timeout ends the fold with `Error::Cancelled` (D283). D4170: a merge with no owner phase (this
    /// default) refuses by name (`no_owner_phase`); kind 2 means a region too small, which the owner fold grows.
    #[allow(clippy::too_many_arguments)]
    fn own_partition(&self, _owner: Region, _part: u32, _pbits: u8, _partials: &[Partial<'_>], _files: &[&dyn crate::SpillFile], _site: *mut gravity_abi::Site, _cancel: *const u8, _round: u8, _flush: Flush<'_>) -> Result<()> {
        Err(crate::section::no_owner_phase())
    }

    fn finish_owners(&self, _leader: &mut [u8], _width: u16, _owner_bytes: u32) -> Result<()> {
        Ok(())
    }

    /// Op 14 (JOIN's deferred build error; PG's outer-first hash join never runs its build when the outer is empty): the
    /// pipeline this merge finishes raised. `Ok(true)` takes the error as deferred: the merge marks the leader region failed
    /// (JOIN's FORM FAILED) and its consumer raises the held error at its first row (`program::raise_held`); the runner
    /// holds it and runs on (`Entry::defer_failed`). `Ok(false)`, the default, ends the statement with it.
    fn defer_error(&self, _leader: &mut State<'_>) -> Result<bool> {
        Ok(false)
    }

    /// Op 14, fed (V4R-89): whether this merge is the variant that defers its pipeline's error, so the errors of the
    /// pipelines only it reads are held for it too (`Entry::defer_fed`). `false`, the default.
    fn defers(&self) -> bool {
        false
    }

    /// This merge's variant that defers its pipeline's error (`defer` true) or not, when it has one (JOIN's build: one
    /// static per variant, chosen by the BuildSide proto's DEFER bit in `merge_for`); `None`: the caller keeps this merge.
    fn with_defer(&'static self, _defer: bool) -> Option<&'static dyn Merge> {
        None
    }

    /// C10 2b (D3582): this merge's one-round owner fold over spill files alone, kept past the merge (`BatchFold`), in the
    /// records' layout `partials` hold: a batched slot's refills fold through it while the runner's merge is out of reach
    /// (driver mem/batch.rs). `None`, the default: the form has no batched road (two rounds, a candidate list, a walk).
    fn batch_fold(&self, _partials: &[Partial<'_>]) -> Option<Box<dyn BatchFold>> {
        None
    }
}

/// C10 2b (D3582): a one-round owner fold a batched slot keeps for its refills (`Merge::batch_fold`): one owner region
/// opened, partitions of the slot's spill files folded into it and reduced, the round closed; its result is the owner's
/// runs, as the owner phase leaves them.
pub trait BatchFold: Send {
    /// The phase-1 records' bytes.
    fn rec(&self) -> usize;

    /// The least region one owner opens.
    fn floor(&self) -> u64;

    fn open(&self, owner: Region) -> Result<()>;

    /// Partition `part` at `pbits` of every file folded into the opened `owner` and reduced; `cancel` as `own_partition`'s.
    /// `Unsupported(UNSUPPORTED_SCRATCH)`: the region is too small, or a dense form missed its domain (D597: the plain
    /// form takes over), and the caller folds the batch again.
    fn fold(&self, owner: Region, part: u32, pbits: u8, files: &[&dyn crate::SpillFile], site: *mut gravity_abi::Site, cancel: *const u8) -> Result<()>;

    /// After the batch's last partition: the owner's round closed (a one-round form orders its run here).
    fn end(&self, owner: Region) -> Result<()>;
}

pub const POISON: u8 = 0xA5;

/// `PGRUST_GRAVITY_SCRIBBLE=1` (test only, read once): release builds poison what a claim leaves behind too (R1D's rig).
pub fn scribble() -> bool {
    static ON: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *ON.get_or_init(|| crate::env::text("PGRUST_GRAVITY_SCRIBBLE").is_some_and(|v| v == "1"))
}

pub static P2_DIAG: [core::sync::atomic::AtomicU32; 8] = [const { core::sync::atomic::AtomicU32::new(0) }; 8];
