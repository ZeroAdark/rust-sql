use gravity_abi::PtrWord;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use gravity_abi::ctx::Partials;
use gravity_abi::MorselRange;
use gravity_abi::breaker::Budget;
use gravity_program::cost::runtime::KEPT_TABLE_MIN;
use gravity_program::{Parallel, Program};
use memledger::ChargedVec;

use gravity_abi::status::UNSUPPORTED_SCRATCH;
use crate::binding::{Binding, WorkerBinding};
use crate::entry::{Entry, Exec, ExecState};
use crate::node::{Derive, NodeState};
use crate::sink::{Merge, Partial, PartitionPlan, State};
use crate::slots::{Warm, WorkerSlot};
use crate::{Error, Result};

mod raw;
use raw::detach;
pub(crate) use raw::owned_header;

/// The partial-slot capacity: the leader and `SLOTS - 1` workers (`[Partial; SLOTS]` below, the
/// parallel crate's `opened` set, the kernels' owner merges). A statement uses its width's worth,
/// sized from the pool's threads; the runner clamps a wider grant to `MAX_WIDTH` and notes it once.
pub const SLOTS: usize = gravity_abi::breaker::MAX_OWNERS;
/// The widest grant: a worker's ordinal stays below the nested lanes (`NESTED_LANE_FIRST`, the leader's runs nested inside
/// a row), so no worker shares one.
pub const MAX_WIDTH: u16 = if SLOTS - 1 < crate::NESTED_LANE_FIRST as usize { (SLOTS - 1) as u16 } else { crate::NESTED_LANE_FIRST - 1 };
const EMPTY: Partial<'static> = Partial { ordinal: 0, bytes: &[], rows_processed: 0 };

struct BusyMark(*const AtomicBool);

/// D1408: a claim's pins released on its own thread on an unwind too (a host's panic resumed at `Code::call`): dropped in the
/// claim's frame, before the pool's catch (`unwind::catch`) hands the failure to the leader, whose detach would release them off their
/// thread ("buffer is not pinned", then a Drop's second panic in the unwind: an abort). A release that panics is swallowed.
struct ReleaseOnUnwind(*mut crate::node::Inner);

impl Drop for ReleaseOnUnwind {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let inner = self.inner();
            let _ = gravity_abi::status::unwind::catch(|| inner.for_each_host(&mut |h| h.release()));
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claimed {
    Done,
    Stopped,
}

pub struct Open<'e> {
    exec: &'e mut Exec,
    /// An instrumented program's counter cells as the section opened (empty for a plain program): its close counts one
    /// loop of each node the section started, however many claims opened it (`settle_loops`).
    cells0: ChargedVec<u64>,
    slots: *mut WorkerSlot,
    program: *const Program,
    width: u16,
    pipeline: usize,
    /// The produced slot's leader region (the merge target), for phase 2's jobs; phase 2's layout may reopen it larger (D254,
    /// `plan_partitions` on the leader before the jobs).
    leader: core::cell::Cell<(*mut u8, usize)>,
    /// An owner's part of the leader region (`share(..).owner`, S2), or of the pinned chunk `own_past_grant` laid (KN-02 (B)).
    owner: core::cell::Cell<u32>,
    /// KN-02 (B): the owners' regions' base when `own_past_grant` pinned them past the leader region; null = behind the
    /// leader region's header.
    owners_base: core::cell::Cell<*mut u8>,
    /// Phase 2 folded the partials partition-parallel: `close_section` runs `finish` only.
    merged: AtomicBool,
    /// D3140 (`open_owners`): the leader region's table is one more partial (an arm run's last arm folded it on the
    /// leader), never an owner's region or `own_single`'s spare.
    lead: bool,
    /// D4043: the statement's purse its claims draw past their W (`grant::claim_purse`), filled at the open; `None`
    /// unadmitted.
    purse: Option<std::sync::Arc<crate::mem::grant::Purse>>,
}

impl Open<'_> {
    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn pipeline(&self) -> usize {
        self.pipeline
    }

    /// The section's pipeline ends in the emit (its rows reach the sink, through the workers' legs); an InitPlan's ends in
    /// its cell (D2998).
    pub fn ends_in_sink(&self) -> bool {
        self.program().pipelines[self.pipeline].ends_in == gravity_program::End::Sink && !crate::pull::init_cell(self.program(), self.pipeline)
    }

    /// The pipeline's index into the program.
    pub fn produced_slot(&self) -> Option<u8> {
        self.program().pipelines[self.pipeline].produces()
    }

    /// Worker `ordinal`'s bound context (a host call from its thread: the images rig pins through it).
    pub fn worker_ctx(&self, ordinal: u16) -> *mut gravity_abi::Ctx {
        self.slot(ordinal).ctx()
    }

    /// Worker `ordinal`'s unspent W (its image cut, carved out of the statement's G).
    pub fn worker_room(&self, ordinal: u16) -> u64 {
        self.slot(ordinal).inner.w
    }

    pub fn partial(&self, ordinal: u16) -> Partial<'_> {
        let slot = self.slot(ordinal);
        Partial { ordinal, bytes: slot.state(), rows_processed: slot.rows_processed() }
    }

    /// The partials of the ordinals that claimed (a slot that never claimed has none), in ordinal order. An owners' open
    /// (`open_owners`): the leader's table, then empty partials up to the width, so the merge sizes `width` owners as it
    /// does after a section every ordinal of which claimed (`Merge::owned_partitions`).
    pub fn partials<'a>(&'a self, out: &mut [Partial<'a>]) -> usize {
        if self.lead {
            let n = (self.width as usize).min(out.len());
            // bounded("small-fixed: one partial per worker slot, at most SLOTS (256)")
            for (i, p) in out.iter_mut().take(n).enumerate() {
                *p = if i == 0 { Partial { ordinal: 0, bytes: self.leader_region(), rows_processed: 0 } } else { EMPTY };
            }
            return n;
        }
        let mut n = 0;
        // bounded("small-fixed: one partial per worker slot, at most SLOTS (256)")
        for o in 0..self.width.min(out.len() as u16) {
            if !self.slot(o).state_stale {
                out[n] = self.partial(o);
                n += 1;
            }
        }
        n
    }

    /// Phase 2 (decision 5), on the leader before the jobs: the plan when the merge is worth
    /// splitting, the destination laid out over the (zeroed) leader region.
    ///
    /// D1713 (its `open_section` sibling): the leader's grow home is taken here, from the open's own `exec` (before the
    /// partials borrow the Open: they only read the Node, which leaves a home not yet written usable). A home taken
    /// in `open_section`, before `exec` moved into the `Open`, was a sibling of the Open's borrow: a grow through it
    /// (`State::grow` -> `fold` writing the Node's counters) disabled that borrow, and `close_section`'s reborrow of the
    /// Open's `exec` was then undefined (Miri tree borrows on tests/region_grow.rs, its phase-2 arm).
    pub fn plan_partitions(&mut self, merge: &dyn Merge) -> Result<Option<PartitionPlan>> {
        let ceil = self.produced_slot().map_or(0, |s| self.exec.node().slot_ceil(s));
        let home = self.produced_slot().map(|s| self.exec.node_mut().leader_home(s));
        let mut partials: [Partial<'_>; SLOTS] = [EMPTY; SLOTS];
        let n = self.partials(&mut partials[..self.width.min(SLOTS as u16) as usize]);
        let Some(plan) = merge.partition_plan_within(&partials[..n], ceil) else { return Ok(None) };
        let mut leader = State { bytes: self.leader_region(), sink: crate::sink::SinkRef::none(), home };
        merge.prepare_partitions_in(&mut leader, plan, &partials[..n])?;
        self.leader.set((leader.bytes.as_mut_ptr(), leader.bytes.len()));
        Ok(Some(plan))
    }

    /// Phase 2's job: fold partitions `lo..hi` of every partial into the destination (disjoint per job).
    pub fn merge_partitions(&self, merge: &dyn Merge, plan: PartitionPlan, lo: u32, hi: u32) -> Result<()> {
        let mut partials: [Partial<'_>; SLOTS] = [EMPTY; SLOTS];
        let n = self.partials(&mut partials[..self.width.min(SLOTS as u16) as usize]);
        let (ptr, len) = self.leader.get();
        let leader = crate::sink::Region { ptr, len };
        // a pool may size a claim past the partition count: a job never touches a slice past the table
        // C9: a pool job runs outside a kernel call: the statement's byte is lent to it, and a tick per partition reads it
        raw::lend_cancel(self.exec.node().ctx(), || -> Result<()> {
            let mut step = cancelpoll::step("section merge: a phase-2 job's partitions");
            for part in lo..hi.min(1u32 << plan.pbits) {
                step.tick()?;
                if let Err(e) = merge.merge_partition(leader, part, plan, &partials[..n]) {
                    let h = raw::head_words(leader);
                    self.exec.node().counters.p2_fail.set([part, h[0], (h[3] & 0xff) as u32, h[1]]);
                    return Err(e);
                }
            }
            Ok(())
        })
    }

    /// D3259: the merge's `seal_over` answer over every partial, filed on the produced slot's stream for the section end
    /// about to run (every worker reads it there, `SPILL_SEAL_OVER`); `over: false` clears it after the section end.
    pub fn file_seal_over(&self, merge: Option<&dyn Merge>) {
        let Some(slot) = self.produced_slot() else { return };
        let (over, rows) = merge.map_or((false, None), |m| {
            let mut partials: [Partial<'_>; SLOTS] = [EMPTY; SLOTS];
            let n = self.partials(&mut partials[..self.width.min(SLOTS as u16) as usize]);
            (m.seal_over(&partials[..n]), m.seal_rows(&partials[..n]))
        });
        let spill = &self.exec.node().spill;
        if (slot as usize) < spill.streams.len() {
            spill.stream(slot).set_seal_over(over);
            spill.stream(slot).set_seal_rows(rows);
        }
    }

    /// D393b: the produced slot's spilled-stream partition count, once the stream spilled (an arm run's earlier arms' files).
    pub fn spill_bits(&self) -> Option<u8> {
        Some(self.exec.node().run_stream_bits(self.pipeline)).filter(|&b| b != 0)
    }

    /// The owner phase's partition count when the merge takes it (partitioned-record.md §3.2).
    pub fn owned_partitions(&self, merge: &dyn Merge) -> Option<u8> {
        let mut partials: [Partial<'_>; SLOTS] = [EMPTY; SLOTS];
        let n = self.partials(&mut partials[..self.width.min(SLOTS as u16) as usize]);
        merge.owned_partitions(&partials[..n], self.owner_bytes())
    }

    pub fn owner_bytes(&self) -> u32 {
        self.owner.get()
    }

    /// KN-02 (B): the owner phase's partition count when the grant's owner cut misses the merge's floor (`owned_partitions`
    /// said `None`): `width` owners sized from the partials' real records, their regions pinned in one chunk
    /// (`owner_share_bytes` each; the arena's pin: a parked region first, then W, G, then past G within the pool's room as
    /// it is now), the owner regions read from it (`owner_region`, the `Owned` header's base). `None`, and the one owner of
    /// `own_single` as before, when the merge takes no owner phase, the owners would pass the wall, or the pool has no room
    /// now. The pin never waits and keeps no refusal: a kept one would name the statement's next scratch refusal out of
    /// memory (`Entry`'s `pool_refused`). D4280: the pin runs with the statement's abort byte lent, so a two-round
    /// chunk's zero fill (the arena's `zero_stepped`, a tick a piece) answers a cancel as `Error::Cancelled`.
    pub fn own_past_grant(&self, merge: &dyn Merge) -> Result<Option<u8>> {
        self.own_past_grant_at(merge, 1)
    }

    /// D3004: `own_past_grant` with each owner sized for `scale` times its share of the real records: a one-round owner
    /// phase whose owners overflowed claims its partitions again into larger regions (a group-routed layout puts a skewed
    /// group's every pair in one partition, so one owner's claims may pass its even share).
    pub fn own_past_grant_at(&self, merge: &dyn Merge, scale: usize) -> Result<Option<u8>> {
        let mut partials: [Partial<'_>; SLOTS] = [EMPTY; SLOTS];
        let n = self.partials(&mut partials[..self.width.min(SLOTS as u16) as usize]);
        let Some(pbits) = self.spill_bits().or_else(|| merge.partition_bits(&partials[..n])) else { return Ok(None) };
        let (held, rec) = self.held_records(&partials[..n], pbits);
        let held = held.saturating_mul(scale.max(1));
        let two_round = merge.owned_rounds() == 2;
        let fits = |b: usize| merge.owned_partitions(&partials[..n], b as u32).is_some();
        let ctx = self.exec.node().ctx();
        let none = |what: &str, chunk: usize| {
            if crate::env::set("PGRUST_GRAVITY_PHASES") {
                eprintln!("OWN past_grant none: {what} held={held} rec={rec} width={} chunk={chunk} room={:?}", self.width, raw::pin_room(ctx));
            }
            Ok(None)
        };
        let Some(bytes) = owner_share_bytes(held, rec, self.width, pbits, two_round, fits) else { return none("no owner size", 0) };
        let Some(chunk) = bytes.checked_mul(self.width as usize) else { return none("overflow", 0) };
        // the gate (`Node::owner_refuses`): the owners' chunk the produced slot's fault refuses is not pinned; the one owner
        // on this thread then maps its region (`own_all`)
        if self.exec.node().owner_refuses(self.produced_slot()) {
            return none("gated", chunk);
        }
        let zeroed = cut_zeroed(bytes, two_round);
        // D4280: the leader runs here between kernel calls, where no abort byte is lent, so a two-round chunk's zero
        // fill ticked none: near the cgroup's limit each fresh page is reclaimed first, and a W8 cb09's leader zeroed for
        // 1.4 s with its cancel unanswered while the cgroup grew. The statement's byte is lent across the pin; a stop is
        // the cancel.
        let (base, stopped) = raw::lend_cancel_seen(ctx, || raw::pin_zeroed_now(ctx, chunk, if two_round { chunk } else { zeroed }));
        if stopped {
            return Err(Error::Cancelled);
        }
        if base.is_null() {
            return none("no pool room", chunk);
        }
        // D3236 (B): the produced slot's owners' chunk, given back when the slot frees
        raw::file_slot_pin(ctx, self.produced_slot(), base);
        if !two_round {
            raw::zero_owners(base, self.width as usize, bytes, zeroed);
        }
        OWN_PAST_GRANT.fetch_add(1, Ordering::Relaxed);
        fires::fire_shared("owner.past_grant"); // shapecheck D3212: the owners pinned past G in one chunk, each owner's prefix zeroed here
        self.owners_base.set(base);
        self.owner.set(bytes as u32);
        Ok(Some(pbits))
    }

    /// A leader region that started small (`grant::starts_small`) commits in place, before the owner phase
    /// cuts its owners from it, what `width` owners sized from the partials' counted records take (`owner_share_bytes`, as
    /// `own_past_grant` sizes them), at most its reservation; the owner cut follows what it then holds. Nothing changes for
    /// a region that is no laid reservation, already holds them, or whose draw is refused (the owner phase as before).
    pub fn owners_in_place(&mut self, merge: &dyn Merge) {
        let Some(slot) = self.produced_slot() else { return };
        let (_, len) = self.leader.get();
        let want = {
            let mut partials: [Partial<'_>; SLOTS] = [EMPTY; SLOTS];
            let n = self.partials(&mut partials[..self.width.min(SLOTS as u16) as usize]);
            let Some(pbits) = self.spill_bits().or_else(|| merge.partition_bits(&partials[..n])) else { return };
            let (held, rec) = self.held_records(&partials[..n], pbits);
            let two_round = merge.owned_rounds() == 2;
            let fits = |b: usize| merge.owned_partitions(&partials[..n], b as u32).is_some();
            let Some(bytes) = owner_share_bytes(held, rec, self.width, pbits, two_round, fits) else { return };
            bytes.saturating_mul(self.width as usize).saturating_add(gravity_abi::breaker::OWNED_HDR + 4096)
        };
        if want <= len {
            return;
        }
        let (ptr, bytes) = self.exec.node_mut().grow_leader_in_place(slot, want as u64);
        if bytes > len {
            self.leader.set((ptr, bytes));
            self.owner.set(crate::mem::grant::owner_cut(crate::mem::grant::Share { bytes: bytes as u32 }, self.width).bytes);
        }
    }

    /// The bytes of every record the owner phase folds (the partials' held records, the produced stream's spilled files, the
    /// entry runs), and the widest record.
    fn held_records(&self, partials: &[Partial<'_>], pbits: u8) -> (usize, usize) {
        let (mut held, mut rec) = (0usize, 8usize);
        // bounded("small-fixed: one partial per worker slot, at most SLOTS (256), a header read each")
        for p in partials {
            if let Some(t) = gravity_abi::breaker::T1::of(p.bytes) {
                held += t.held() as usize * t.layout.size() as usize;
                rec = rec.max(t.layout.size() as usize);
            }
        }
        if let Some(slot) = self.produced_slot() {
            held += self.exec.node().spill.stream(slot).files.iter().flatten().map(|f| f.spilled_bytes() as usize).sum::<usize>();
        }
        held += self.entry_runs(pbits).map(|f| f.spilled_bytes() as usize).sum::<usize>();
        (held, rec)
    }

    /// D3004: the grant's owner cut (`owner_bytes` each, from an estimate) holds less than the real records (an even share each):
    /// a one-round owner phase over it overflows (seconds of folding first), so the owners are regrown before it runs.
    pub fn owners_short(&self, merge: &dyn Merge, pbits: u8) -> bool {
        let mut partials: [Partial<'_>; SLOTS] = [EMPTY; SLOTS];
        let n = self.partials(&mut partials[..self.width.min(SLOTS as u16) as usize]);
        let (held, _) = self.held_records(&partials[..n], pbits);
        merge.owned_rounds() == 1 && (self.owner_bytes() as usize).saturating_mul(self.width as usize) < held
    }

    /// The owner phase's job: partitions `lo..hi` of every partial folded into owner `ordinal`'s
    /// region (opened at its first), reduced by the merge; the owner's worker site renders.
    pub fn own_partitions(&self, merge: &dyn Merge, ordinal: u16, pbits: u8, lo: u32, hi: u32, opened: &AtomicBool) -> Result<()> {
        let mut partials: [Partial<'_>; SLOTS] = [EMPTY; SLOTS];
        let n = self.partials(&mut partials[..self.width.min(SLOTS as u16) as usize]);
        let region = self.owner_region(ordinal);
        if !opened.swap(true, Ordering::AcqRel) {
            merge.open_owner(region, ordinal)?;
        }
        // D4695: the owner renders through its own slot's site; a slot no claim reached (`bind_at_claim`) binds first, on
        // this owner's thread (an ordinal's owner jobs run on one thread at a time, as its claims did)
        raw::bind_pending(self.slot(ordinal))?;
        let site = self.owner_site(ordinal);
        self.own_with(merge, ordinal, pbits, lo, hi, &partials[..n], site, 1)
    }

    /// The second round (§3.5): every owner's round-2 buffers and round-2 files are the sources.
    pub fn own_partitions2(&self, merge: &dyn Merge, ordinal: u16, pbits: u8, lo: u32, hi: u32) -> Result<()> {
        let mut partials: [Partial<'_>; SLOTS] = [EMPTY; SLOTS];
        let mut n = 0;
        // bounded("small-fixed: one owner's round-2 buffers per worker slot, at most SLOTS (256)")
        for o in 0..self.width.min(SLOTS as u16) {
            if let Some(r) = merge.round2_partial(self.owner_region(o)) {
                partials[n] = Partial { ordinal: o, bytes: raw::bytes_of(r), rows_processed: 0 };
                n += 1;
            }
        }
        // D4695: as round 1 (`own_partitions`)
        raw::bind_pending(self.slot(ordinal))?;
        let site = self.owner_site(ordinal);
        self.own_with(merge, ordinal, pbits, lo, hi, &partials[..n], site, 2)
    }

    /// Between the rounds: every owner opened (one that saw no partition, empty) and its round closed.
    pub fn end_round(&self, merge: &dyn Merge, opened: &[AtomicBool]) -> Result<u8> {
        // C9: the statement's byte lent (the leader, between kernel calls); a tick per owner, whose round's end flushes its
        // round-2 buffers
        raw::lend_cancel(self.exec.node().ctx(), || -> Result<u8> {
            let mut step = cancelpoll::step("section owners: the round's end");
            for o in 0..self.width {
                step.tick()?;
                if !opened[o as usize].swap(true, Ordering::AcqRel) {
                    merge.open_owner(self.owner_region(o), o)?;
                }
                merge.end_round(self.owner_region(o), &mut self.round2_flush(merge, o))?;
            }
            Ok(merge.round2_pbits(self.owner_region(0)))
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn own_with(&self, merge: &dyn Merge, ordinal: u16, pbits: u8, lo: u32, hi: u32, partials: &[Partial<'_>], site: *mut gravity_abi::Site, round: u8) -> Result<()> {
        // shapecheck D06/D08/D15: the parallel owner phase over the partitioned record table is taken (pool worker: shared)
        fires::fire_shared(if round == 2 { "owner.round2" } else { "owner.parallel" });
        fire_small_open(partials);
        // round 1: the produced slot's spill files in ctx-owner order (the leader's, then the workers'), then the slots' entry
        // runs; round 2: the owners' round-2 files
        let mut files: [Option<&dyn crate::SpillFile>; 2 * SLOTS + 1] = [None; 2 * SLOTS + 1];
        let mut nf = 0usize;
        if let Some(slot) = self.produced_slot() {
            let stream = self.exec.node().spill.stream(slot);
            let source = if round == 1 { &stream.files } else { &stream.round2 };
            // bounded("small-fixed: one spill file per worker and the slots' entry runs, at most the array's 2 * SLOTS + 1")
            for f in source.iter().flatten().map(|f| f.as_ref()).chain((round == 1).then(|| self.entry_runs(pbits)).into_iter().flatten()) {
                if nf < files.len() {
                    files[nf] = Some(f);
                    nf += 1;
                }
            }
        }
        let files: [&dyn crate::SpillFile; 2 * SLOTS + 1] = core::array::from_fn(|i| files[i].unwrap_or(files[0].unwrap_or(&NO_FILE)));
        let region = self.owner_region(ordinal);
        let mut flush = self.round2_flush(merge, ordinal);
        // the owner's worker ctx, bound for the section (its `cancel` is the statement's abort byte)
        let cancel = raw::cancel_of(self.slot(ordinal).ctx());
        // C9: a pool job runs outside a kernel call: that byte is lent to it, and a tick per partition reads it
        raw::lend_cancel(self.slot(ordinal).ctx(), || -> Result<()> {
            let mut step = cancelpoll::step("section owners: an owner's partitions");
            for part in lo..hi.min(1u32 << pbits) {
                step.tick()?;
                merge.own_partition(region, part, pbits, partials, &files[..nf], site, cancel, round, &mut flush)?;
            }
            Ok(())
        })
    }

    /// Road dict.entries' runs (hosts granule_source/entry_runs.rs, a count(DISTINCT) over a text column): every slot's source's parts published as resident runs, read as
    /// files beside the partials at `pbits`. Phase 1 is over, so the sources are only read.
    fn entry_runs(&self, pbits: u8) -> impl Iterator<Item = &dyn crate::SpillFile> + '_ {
        let site = usize::try_from(self.program().pipelines[self.pipeline].driving_site).ok();
        (0..self.width).filter_map(move |o| site.and_then(|i| self.slot_source(o, i)).and_then(|s| s.owner_input(pbits)))
    }

    /// Some slot's source published parts as entry runs: the owner phase is the pool's, never the leader's few records.
    pub fn has_entry_runs(&self) -> bool {
        let site = usize::try_from(self.program().pipelines[self.pipeline].driving_site).ok();
        (0..self.width).any(|o| site.and_then(|i| self.slot_source(o, i)).is_some_and(|s| s.has_owner_input()))
    }

    /// The partials' partition count (phase-1 tables), whatever the owner regions hold.
    pub fn partition_bits(&self, merge: &dyn Merge) -> Option<u8> {
        let mut partials: [Partial<'_>; SLOTS] = [EMPTY; SLOTS];
        let n = self.partials(&mut partials[..self.width.min(SLOTS as u16) as usize]);
        merge.partition_bits(&partials[..n])
    }

    /// One owner over a region of the leader's arena, on the calling thread (a share the `W` owner
    /// regions could not hold): every partition of every partial and every file, both rounds; the
    /// leader region carries a one-owner header pointing at the region.
    pub fn own_single(&self, merge: &dyn Merge, pbits: u8) -> Result<()> {
        let mut partials: [Partial<'_>; SLOTS] = [EMPTY; SLOTS];
        let n = self.partials(&mut partials[..self.width.min(SLOTS as u16) as usize]);
        let mut files: [Option<&dyn crate::SpillFile>; 2 * SLOTS + 1] = [None; 2 * SLOTS + 1];
        let mut nf = 0usize;
        if let Some(slot) = self.produced_slot() {
            // bounded("small-fixed: one spill file per worker and the slots' entry runs, at most the array's 2 * SLOTS + 1")
            for f in self.exec.node().spill.stream(slot).files.iter().flatten().map(|f| f.as_ref()).chain(self.entry_runs(pbits)) {
                if nf < files.len() {
                    files[nf] = Some(f);
                    nf += 1;
                }
            }
        }
        let files: [&dyn crate::SpillFile; 2 * SLOTS + 1] = core::array::from_fn(|i| files[i].unwrap_or(files[0].unwrap_or(&NO_FILE)));
        let node = self.exec.node();
        // D4695: the one owner renders through slot 0's site: a slot no claim reached (`bind_at_claim`) binds first, here
        // (the claims are over: no other thread holds the slot)
        raw::bind_pending(self.slot(0))?;
        let site = self.owner_site(0);
        let leader = self.leader_region();
        let spare: &mut [u8] = if self.lead { &mut [] } else { leader.get_mut(gravity_abi::breaker::OWNED_HDR..).unwrap_or(&mut []) };
        // KN-02 (B): owners pinned past the grant's cut that overflowed (an owner phase whose partitions landed unevenly)
        // leave their chunk idle; the one owner is laid there when it is the larger spare, so the redo takes no second pin
        let spare = match self.owners_chunk() {
            Some(chunk) if chunk.len() > spare.len() => chunk,
            _ => spare,
        };
        let gate = || node.owner_refuses(self.produced_slot());
        let region = own_all(merge, node.ctx(), &node.spill, self.produced_slot(), &partials[..n], &files[..nf], site, pbits, spare, &gate, node.owner_give_floor())?;
        owned_header(self.leader_region(), region)
    }

    /// C10 2b (D3582): the section's batched road, when the produced slot takes it (`mem::batch::fold_for`): every claimed
    /// partial scattered to the slot's files (`Node::spill_partial`), then `mem::batch::lay` (the first batch folded on
    /// this thread, the leader region its one owner's header); the pool's owner phase does not run. `false`: the owner
    /// phase as before.
    pub fn own_batched(&mut self, merge: &dyn Merge) -> Result<bool> {
        let Some(slot) = self.produced_slot() else { return Ok(false) };
        // D3140: an owners' open folds the leader's table; D393b: an arm run's last arm folds every arm's records
        if self.lead || self.exec.node().in_arm_run(self.pipeline) || self.has_entry_runs() {
            return Ok(false);
        }
        let ctx = self.exec.node().ctx();
        let room = crate::mem::batch::room(ctx);
        let (fold, pbits) = {
            let mut partials: [Partial<'_>; SLOTS] = [EMPTY; SLOTS];
            let n = self.partials(&mut partials[..self.width.min(SLOTS as u16) as usize]);
            let stream = self.exec.node().spill.stream(slot);
            let (held, rec) = crate::mem::batch::held_of(&partials[..n], stream);
            let pbits = match stream.bits0() {
                0 => merge.partition_bits(&partials[..n]),
                b => Some(b),
            };
            (crate::mem::batch::fold_for(self.program(), self.pipeline, slot, stream, merge, &partials[..n], held, rec, room, self.leader_region().len().saturating_sub(gravity_abi::breaker::OWNED_HDR), &mut || kept_run(merge)), pbits)
        };
        let (Some(fold), Some(pbits)) = (fold, pbits) else { return Ok(false) };
        let (width, pipeline) = (self.width, self.pipeline);
        raw::lend_cancel(ctx, || -> Result<()> {
            let mut step = cancelpoll::step("section own_batched: the partials scattered");
            for o in 0..width {
                step.tick().map_err(|_| Error::Cancelled)?;
                if !self.slot(o).state_stale {
                    let ptr = self.slot(o).state_ptr;
                    self.exec.node_mut().spill_partial(pipeline, ptr)?;
                }
            }
            Ok(())
        })?;
        let site = (!self.program().sites.is_empty()).then_some(self.program().pipelines[pipeline].driving_site.max(0) as usize);
        let budget = crate::mem::batch::budget(ctx, self.program(), pipeline);
        let spill: *mut crate::mem::spill::SpillTable = &mut *self.exec.node_mut().spill;
        crate::mem::batch::lay(ctx, spill, slot, fold, pbits, budget, self.leader_region(), site)?;
        Ok(true)
    }

    /// D533: every owner's round-2 file of the produced slot dropped (a two-round owner phase redone by `own_single`).
    pub fn reset_round2(&self) {
        if let Some(slot) = self.produced_slot() {
            let spill = &self.exec.node().spill;
            // bounded("small-fixed: one round-2 file per owner, at most SLOTS (256)")
            for o in 0..spill.stream(slot).round2.len() {
                // the owners' pool run is over; the leader thread alone holds the stream until `own_single`
                raw::reset_round2(spill, slot, o);
            }
        }
    }

    /// After the owners: every owner that saw no partition opened empty, the leader region's header written.
    pub fn finish_owners(&self, merge: &dyn Merge, opened: &[AtomicBool]) -> Result<()> {
        // bounded("small-fixed: one owner per worker slot, at most SLOTS (256); an owner that saw no partition lays its header")
        for o in 0..self.width {
            if !opened[o as usize].load(Ordering::Acquire) {
                merge.open_owner(self.owner_region(o), o)?;
            }
        }
        let bytes = self.owner_bytes();
        merge.finish_owners(self.leader_region(), self.width, bytes)?;
        match self.owners_base.get() {
            // KN-02 (B): the header points every reader (`Owned::owner`) at the pinned chunk
            base if !base.is_null() => owned_header(self.leader_region(), Owners { base, k: self.width as u32, bytes: bytes as usize }),
            _ => Ok(()),
        }
    }

    /// The partials that claimed (phase 2a's unit count).
    pub fn partials_count(&self) -> usize {
        (0..self.width).filter(|&o| !self.slot(o).state_stale).count()
    }

    /// Phase 2a's job: the `index`-th partial (ordinal order) prepared in place by the merge.
    pub fn prepare_partial(&self, merge: &dyn Merge, plan: PartitionPlan, index: usize) -> Result<()> {
        let mut n = 0;
        // bounded("small-fixed: one partial per worker slot, at most SLOTS (256)")
        for o in 0..self.width {
            let s = self.slot(o);
            if s.state_stale {
                continue;
            }
            if n == index {
                return merge.prepare_partial(crate::sink::Region { ptr: s.state_ptr, len: s.state_bytes as usize }, plan);
            }
            n += 1;
        }
        Ok(())
    }

    /// Whether the execution spilled so far (phase 2 is skipped then: the seal scatters the leader's fold), or (NEW-53) the
    /// produced slot holds records an earlier arm handed on in memory: files the owner phase reads beside the partials.
    pub fn spilled(&self) -> bool {
        self.exec.node().spilled() || self.produced_slot().is_some_and(|s| self.exec.node().spill.stream(s).handed())
    }

    /// Phase 2 done: the fold is complete, `close_section` runs `finish` only.
    pub fn set_merged(&self) {
        self.merged.store(true, Ordering::Release);
        let c = &self.exec.node().counters;
        c.merged.set(c.merged.get() + 1);
    }

    /// Undo a failed phase 2: the destination's header is zeroed and re-armed for the serial fold
    /// (a share region is armed with its own length; the tables zero what they lay out).
    pub fn reset_leader(&self, step: usize) {
        let c = &self.exec.node().counters;
        c.p2_refused.set(c.p2_refused.get() + 1);
        c.p2_refused_at[step.min(2)].set(c.p2_refused_at[step.min(2)].get() + 1);
        let r = self.leader_region();
        let shared = self.program().pipelines[self.pipeline].breaker().is_some_and(|b| b.budget.is_share());
        raw::rearm(r, shared);
    }
}

/// The bytes of an owner region `own_all` zeroes (cut from the spare region or pinned): a one-round owner's header prefix alone (`uninit_region`'s rule, node.rs: its
/// `open` writes the result header, fold_sub's `HashAgg::at` zeroes every slot a partition's fold reads, the kept rows and
/// the candidates are counted before they are read; a count(DISTINCT) over a text column spent 8.5 ms of 86.8 at W0 zeroing the rest), a two-round owner's
/// whole cut (its round-2 T1 reads the header prefix `open` lays it at). Debug and `PGRUST_GRAVITY_SCRIBBLE` runs poison the rest.
/// D2988 (C): the words of `own_all`'s held-partition mask (2,048 partitions; a deeper split folds every partition).
const HELD_MASK_WORDS: usize = 32;

/// D2988's fire (R-KEEP's count): a partial's phase-1 table opened under `T1_MIN_SLOTS` slots with room to grow in place
/// reached this owner phase: `agg_open_grown`'s `T1_GROWN_MIN_SLOTS` floor took it (no other open lays a table that can grow,
/// and a share-clipped table has no room, `max_bits` 0).
fn fire_small_open(partials: &[Partial<'_>]) {
    use gravity_abi::breaker::{T1, T1_MIN_SLOTS};
    if !fires::on() {
        return;
    }
    if partials.iter().filter_map(|p| T1::of(p.bytes)).any(|t| (t.cap as u64) < T1_MIN_SLOTS && t.max_bits != 0) {
        fires::fire_shared("table.sized_small");
    }
}

fn cut_zeroed(len: usize, two_round: bool) -> usize {
    if two_round { len } else { crate::node::ZERO_PREFIX.min(len) }
}

/// One owner's fold on the calling thread over a region sized from the partials' records in memory and in the files
/// (`one_owner_bytes`; a two-round shape its least fitting floor), opened, every partition of every partial and file folded
/// and reduced, the round closed, a second round over the owner's round-2 buffers and file (owner 0's of `slot`) when the
/// shape has one. Returns the owners (one region; k past the wall, D485).
#[allow(clippy::too_many_arguments)]
/// `spare`: the idle leader region behind its header, where the region is cut when it fits (`owner_place`).
/// `gate`: whether the next pin is refused (`Node::owner_refuses` of the produced slot: the forced-spill test fault, a
/// revoke's cut); a refused pin is mapped from a temp file, as one the pool refuses.
/// `give`: D4078 (ADMITFP (A)) the pin floor under which the region a round 1 overflowed in is given back before a split
/// pins its chunk (`Node::owner_give_floor`); `None`: it stays to the slot's free.
#[allow(clippy::too_many_arguments)]
pub(crate) fn own_all(merge: &dyn Merge, ctx: *mut gravity_abi::Ctx, spill: &crate::mem::spill::SpillTable, slot: Option<u8>, partials: &[Partial<'_>], files: &[&dyn crate::SpillFile], site: *mut gravity_abi::Site, pbits: u8, spare: &mut [u8], gate: &dyn Fn() -> bool, give: Option<usize>) -> Result<Owners> {
    // D3902: the owner phase runs between kernel calls, where D3887 lent no byte: its draws past the grant (`pin`, the pool's
    // `take_wait`, up to bytes_wait) waited out a cancel or a timeout whole (2 s beside a cursor holding the pool)
    // C9: the owner phase is one Step on that byte; its partitions and regrowths tick it
    raw::lend_cancel(ctx, || {
        let mut step = cancelpoll::step("section own_all: the owner phase");
        own_all_lent(&mut step, merge, ctx, spill, slot, partials, files, site, pbits, spare, gate, give)
    })
}

#[allow(clippy::too_many_arguments)]
fn own_all_lent(step: &mut cancelpoll::Step, merge: &dyn Merge, ctx: *mut gravity_abi::Ctx, spill: &crate::mem::spill::SpillTable, slot: Option<u8>, partials: &[Partial<'_>], files: &[&dyn crate::SpillFile], site: *mut gravity_abi::Site, pbits: u8, spare: &mut [u8], gate: &dyn Fn() -> bool, give: Option<usize>) -> Result<Owners> {
    use gravity_abi::breaker::T1;
    let wall = owner_wall_at();
    // D4078: a one-round owner splits past its statement's wall (`Grant::owner_wall`: half the pool, its armed big lane or
    // the broker's row cap, read once at its admission, whose footprint pinned the owner at it), not the format's: one owner past half the pool leaves
    // no room for a second statement, and its parked region was the next one's watchdog cancel. The kept-run and two-round
    // forms have no split road and keep the format's wall: their growth past G is the pool's to refuse (`pin`'s temp file)
    let split_wall = raw::owner_wall_of(ctx, wall);
    fires::fire_shared("owner.single"); // shapecheck D06/D08: one owner over every partition
    fire_small_open(partials);
    let (mut held, mut rec) = (0usize, 8usize);
    // bounded("small-fixed: one partial per worker slot, at most SLOTS (256), a header read each")
    for p in partials {
        if let Some(t) = T1::of(p.bytes) {
            held += t.held() as usize * t.layout.size() as usize;
            rec = rec.max(t.layout.size() as usize);
        }
    }
    // bounded("small-fixed: one spill file per worker and entry run, at most the callers' arrays (2 * SLOTS + 1)")
    for f in files {
        held += f.spilled_bytes() as usize;
    }
    // a one-round owner from the partials' real records, never the group estimate; a two-round one takes its least floor
    let ask = one_owner_bytes(held, rec);
    // D2724: ONE owner's region holds its form's floor by itself (`owner_floor`). `owned_partitions` also admits a pool whose
    // owners hold a one-round floor only together. A hosted grouped distinct fold over no row (held 0, a 16712-byte records'
    // region, floor 31976) "fit" by that clause at 4 workers: its refused open raised kind 2 where 3 workers laid the floor.
    let floor = merge.owner_floor().unwrap_or(0);
    let fits = |b: usize| b as u64 >= floor && merge.owned_partitions(partials, b as u32).is_some();
    let floored =|| -> Result<usize> { Ok(least_fit(owner_region_bytes(ask, held, |b| fits(b as usize))?, ask, fits)) };
    // F7: a two-round owner folds one partition at a time into the table half of its region, so its least floor also holds
    // one partition's table (twice its records' bytes): at the shape's floor alone a spilled partition overfilled the table,
    // fold_sub re-read it at every split depth and D533 re-ran round 1 at each doubling (a 2 M-row count(DISTINCT) per group
    // spilled at work_mem: 12 s against PG's 1 s)
    let part = 2 * (gravity_abi::breaker::HASHAGG_HDR + (4 * held >> pbits).max(64 * rec));
    // D4078: the spare is the statement's already (the leader region, or the owners' chunk an overflowed owner phase left
    // idle): a round 1 laid in it pins nothing, so the statement's wall bounds only a fresh pin (lifetime_d3437's union at
    // W8: a 128 MiB cut of a 200 MB owners' chunk overflowed and split into a second ~200 MB pin)
    let in_spare = spare.len().min(wall) & !7;
    let bytes = if merge.owned_rounds() == 1 { ask.min(split_wall.max(in_spare)) } else if fits(ask) { least_fit(ask, 0, fits).max(part.min(ask)) } else { floored()? };
    // D485: a form whose run keeps at most n records (the count / top-k owners, `OwnerTop.n`) has no run half of the records:
    // its region is the run and one partition's table (the fold takes a partition at a time), grown when a partition overflows
    let kept_word = if merge.owned_rounds() == 1 { merge.kept_run_memo(&mut || kept_run(merge)) } else { None };
    let kept = kept_word.map(Kept::of);
    // D3111: the table holds four times the LARGEST partition's records, not the average's: a source routed by its group
    // (a grouped count(DISTINCT)'s pairs) keeps a group's records in one partition, so a few big groups put partitions far
    // past the average, and one past its table refolded every partition before it in the grown region
    let mut bufs = OwnBufs::take();
    let two_round = merge.owned_rounds() == 2;
    let counted = (kept.is_some() || two_round) && {
        bufs.parts.resize(1 << pbits, 0); // alloc: Cold
        merge.part_record_bytes(partials, files, pbits, &mut bufs.parts)
    };
    let table = if counted { 4 * bufs.parts.iter().copied().max().unwrap_or(0) as usize } else { 4 * held >> pbits };
    let bytes = match kept {
        Some(k) => k.region(table.max(64 * rec).max(KEPT_TABLE_MIN)).min(wall).min(owner_first_at()).next_multiple_of(8),
        None => bytes,
    };
    let spare_len = spare.len().min(wall) & !7;
    let spare_ptr = spare.as_mut_ptr();
    let cut = |len: usize| {
        OWN_CUT.fetch_add(1, Ordering::Relaxed);
        let zeroed = cut_zeroed(len, two_round);
        raw::lay_cut(spare_ptr, len, zeroed);
        crate::sink::Region { ptr: spare_ptr, len }
    };
    // D4078 (ADMITFP (A)): the last region the pin below took from the pool and the last it mapped from a temp file, so the
    // one a round 1 overflowed in can be given back before a split pins its chunk
    let (pinned, mapped) = (core::cell::Cell::new(core::ptr::null_mut::<u8>()), core::cell::Cell::new(core::ptr::null_mut::<u8>()));
    let pin = |bytes: usize| -> Result<crate::sink::Region> {
        let _pin = cancelpoll::step("section own_all: an owner region's pin");
        // a pinned pool chunk: the arena window is rewound at the next pipeline's Call, the region must outlive it
        let from_pool = || -> Result<Option<crate::sink::Region>> {
            OWN_PINNED.fetch_add(1, Ordering::Relaxed);
            let ptr = raw::pin_zeroed(ctx, bytes, cut_zeroed(bytes, two_round));
            if !ptr.is_null() {
                // D3236 (B): the produced slot's owner region, given back when the slot frees
                raw::file_slot_pin(ctx, slot, ptr);
                pinned.set(ptr);
                return Ok(Some(crate::sink::Region { ptr, len: bytes }));
            }
            // D3902: the pool's wait stopped for the statement's cancel: the cancel, never the temp-file road below
            if cancelpoll::seen() {
                return Err(Error::Cancelled);
            }
            Ok(None)
        };
        // the gate (`Node::owner_refuses`): a draw the produced slot's fault refuses (the forced-spill test fault, a
        // revoke's cut) asks the pool nothing and takes the temp-file road below, the owner phase's spill exit
        let gated = gate();
        if !gated {
            if let Some(r) = from_pool()? {
                return Ok(r);
            }
        }
        // D2990: a region the pool (or the statement's part of it) refuses is a temp file mapped shared, D491's road for the
        // serial owners: the one owner folds into it as into a pinned chunk (a fresh file reads zero), and the page cache
        // writes it back under memory pressure, so the statement answers in Gravity rather than falling through
        let ptr = raw::map_owners(ctx, bytes)?;
        if ptr.is_null() {
            // a gated draw in a session with no file road is the pool's, as before the gate
            if gated {
                if let Some(r) = from_pool()? {
                    return Ok(r);
                }
            }
            // NR-3: refused by the pool and by the temp-file road: the owner region's road, named
            let pool = &crate::mem::grant::POOL;
            return Err(Error::OutOfMemory { bytes: bytes as u64, held: pool.held(), capacity: pool.capacity(), why: Some(gravity_program::spill::NoSpill::AggOwnerAll) });
        }
        OWN_SPILLED.fetch_add(1, Ordering::Relaxed);
        mapped.set(ptr);
        Ok(crate::sink::Region { ptr, len: bytes })
    };
    // D220: `held` counts a group once per worker holding it (scheduling decides how many). A one-round records' bound past
    // the leader region and the grant's room takes the larger of the two and folds; real groups past it re-place (D285, below).
    // D2988: the one-pin room read only where a refused place takes it (a context-tree walk per owner phase before); the
    // grant's room (`pin_room`, a walk of the statement's context tree) only where the region is not cut from the spare
    // (`owner_place` cuts before it reads the room): a rescanned set's small owner is cut at every rescan
    let place = |bytes: usize| -> Result<(crate::sink::Region, bool)> {
        let holds = two_round && fits(spare_len);
        let room = if spare_len >= bytes || holds { None } else { raw::pin_room(ctx) };
        match owner_place(bytes, spare_len, holds, room) {
            OwnerPlace::Cut(len) => Ok((cut(len), false)),
            OwnerPlace::Pin => Ok((pin(bytes)?, false)),
            OwnerPlace::Refuse(_) if !two_round => match raw::pin_room_one(ctx) {
                Some(r) if (r as usize & !7) > spare_len => Ok((pin(r as usize & !7)?, true)),
                _ => Ok((cut(spare_len), true)),
            },
            // D520 (D285 / D254's road): a two-round floor past the room is pinned past the grant, counted grown
            OwnerPlace::Refuse(_) => Ok((pin(bytes)?, false)),
        }
    };
    let (mut region, bounded) = place(bytes)?;
    OWN_BOUNDED.fetch_add(bounded as u64, Ordering::Relaxed);
    // D285: a bounded region the real groups outgrow (a stale estimate's underestimate) is placed again at the records'
    // bound past the grant, D254's road (the pin draws W, then G, then the pool past both, counted as grown), and the round
    // folds again from the partials and files, which it only read; the real groups never pass the records' bound
    // D3084: a bounded region under a records' bound the form's floor passes (an empty side's one owner) is laid at the
    // floor past the grant: pinned again at the records' bound its open refused it again, kind 2 with no refusal kept,
    // whenever the statement's own regions had filled G before its owner phase
    if let Err(e) = merge.open_owner(region, 0) {
        // only kind 2 is a region too small (D4170: a merge with no owner phase refuses by name)
        if e != Error::Unsupported(UNSUPPORTED_SCRATCH) {
            return Err(e);
        }
        if two_round || fits(bytes) {
            if !bounded {
                return Err(e);
            }
            region = pin(bytes)?;
        } else {
            // a count / top-k form's kept rows past the records' size: laid at that form's floor
            let floor = floored()?;
            region = if bounded { pin(floor)? } else { place(floor)?.0 };
        }
        merge.open_owner(region, 0)?;
    }
    // D3145: a two-round owner's table space holds its largest partition's records at the fold's load (7/8) before round
    // 1: a source routed by its group keeps a group's pairs in one partition that no split divides, and one past the table
    // space folded round 1 again from partition 0 at each doubling (the D533 arm below)
    if two_round && counted {
        let recs = bufs.parts.iter().copied().max().unwrap_or(0) as usize / rec.max(1);
        let need = gravity_abi::breaker::HASHAGG_HDR + (recs + recs / 7 + 2).next_power_of_two().max(64) * rec;
        for _ in 0..4 {
            let off = raw::owner_top(region).table_off as usize;
            let space = region.len.saturating_sub(off);
            if off == 0 || space >= need || region.len >= wall {
                break;
            }
            let want = (region.len as u128 * need as u128).div_ceil(space.max(1) as u128).min(wall as u128) as usize;
            region = pin(want.saturating_add(64 << 10).min(wall) & !7)?;
            merge.open_owner(region, 0)?;
            fires::fire_shared("owner.part_sized");
        }
    }
    // the node's ctx, bound for the statement (its `cancel` is the statement's abort byte)
    let cancel = raw::cancel_of(ctx);
    // D2988 (C): a one-round fold reads only the partitions a source holds (a rescanned DISTINCT of a row or two folded
    // every one of its 64 partitions, each a table laid and the partial's records read again, per rescan)
    let mut held_mask = [0u64; HELD_MASK_WORDS];
    let masked = !two_round && (1usize << pbits) <= 64 * HELD_MASK_WORDS && merge.held_parts(partials, files, pbits, &mut held_mask);
    // D3111: a kept form whose kept state is known saves it (`owner_save`) before each partition where that costs at most
    // the partition's records (or `KEPT_SAVE_FREE`): a partition that overflows the region then folds again from that state
    // in the grown one, and the partitions before it are not folded again. `round1` folds from partition `from` on; its
    // error carries the partition it stopped at (`1 << pbits`: the round's end).
    let save_end = kept.and_then(|k| k.end);
    let round1 = |step: &mut cancelpoll::Step, region: crate::sink::Region, from: u32, bufs: &mut OwnBufs| -> core::result::Result<(), (u32, Error)> {
        let mut flush = || flush_round2(merge, spill, slot, region);
        for part in from..1u32 << pbits {
            step.tick().map_err(|c| (part, Error::from(c)))?;
            if masked && held_mask[part as usize / 64] >> (part % 64) & 1 == 0 {
                continue;
            }
            if let Some(end) = save_end.filter(|&e| e <= KEPT_SAVE_FREE || (counted && e as u64 <= bufs.parts[part as usize])) {
                raw::owner_save(region, end, &mut bufs.snap);
                bufs.saved = part;
            }
            merge.own_partition(region, part, pbits, partials, files, site, cancel, 1, &mut flush).map_err(|e| (part, e))?;
        }
        let _end = cancelpoll::step("section own_all: the round's end");
        merge.end_round(region, &mut flush).map_err(|e| (1u32 << pbits, e))
    };
    // D438: a one-round region at the records' bytes has a table half of the records' bytes: a few groups' partition table
    // holds one record, so two groups whose routes share the partition bits and the three splits' nine bits fill it at every
    // depth (fold_sub's wall). One-round round 1 only reads its sources: it folds again over a region whose table half holds
    // twice every held record (`one_owner_whole_bytes`), so the region no longer sizes a partition's table.
    let whole_all = one_owner_whole_bytes(held, rec);
    let whole = whole_all.min(split_wall);
    // D485: a one-round owner that overflows the wall is split into k serial owners (`own_split`), never kind 2 at the wall.
    // Only once a wall-sized owner overflowed: `held` counts a group once per record that spilled or was held (D220), so the
    // records' ask past the wall is often a region the real groups fit (a join-and-aggregate at W0: a split many times the wall where the wall held them)
    let split = || own_split(merge, ctx, partials, files, site, pbits, held, rec, (split_wall, wall), cancel, spill, slot, gate);
    // D4078 (ADMITFP (A)): the region round 1 overflowed in leaves before the split pins its chunk,
    // so the phase holds the larger of the two, never their sum (lifetime_d3437's union at W0: a 128 MiB round 1 held beside
    // a ~200 MB chunk). A pool pin goes under `give` (`Node::owner_give_floor`); a temp-file map as D491's rerun gives its
    // own back; the spare's cut is the leader's and stays.
    let give_back = |r: crate::sink::Region| {
        let Some(floor) = give else { return };
        if r.ptr == pinned.get() {
            raw::give_owner_pin(ctx, slot, r.ptr, floor);
        } else if r.ptr == mapped.get() {
            raw::unmap_owners(ctx, r.ptr);
        }
    };
    let mut r1 = round1(step, region, 0, &mut bufs);
    while let Err((at, e)) = r1 {
        region = match e {
            Error::Unsupported(UNSUPPORTED_SCRATCH) if bounded && region.len < bytes => pin(bytes)?,
            Error::Unsupported(UNSUPPORTED_SCRATCH) if kept.is_some() && region.len < wall => {
                let part = bufs.parts.get(at as usize).copied().filter(|_| counted);
                pin(kept.map_or(2 * region.len, |k| k.grown(region.len, part)).min(wall) & !7)?
            }
            Error::Unsupported(UNSUPPORTED_SCRATCH) if kept.is_some() => return Err(owner_wall(region.len, held)),
            Error::Unsupported(UNSUPPORTED_SCRATCH) if !two_round && region.len < whole => pin(whole)?,
            Error::Unsupported(UNSUPPORTED_SCRATCH) if !two_round && whole_all > split_wall => {
                give_back(region);
                return split();
            }
            // D3878: `whole` is the runs layout's need (half runs, half table); a one-round form laid otherwise (hosts
            // distinct_fold.rs: a quarter of runs, each a group's accumulator, wider than its pair) overflows it, and folds
            // again in a region twice the size up to the wall, as D533's two-round owner does, never kind 2 below the wall
            Error::Unsupported(UNSUPPORTED_SCRATCH) if !two_round && region.len < wall => pin((2 * region.len).min(wall) & !7)?,
            Error::Unsupported(UNSUPPORTED_SCRATCH) if !two_round => return Err(owner_wall(region.len, held)),
            // D533: a two-round partition past its route bits (parallel merge.rs fold_sub SCRATCH) folds round 1 again in a region
            // twice the size, up to the wall; round 1 only reads its sources, and its round-2 file restarts empty
            Error::Unsupported(UNSUPPORTED_SCRATCH) if two_round && region.len < wall => {
                if let Some(s) = slot {
                    // the one owner's round-2 file (entry 0), this thread's alone; round 1 wrote it and nothing reads it yet
                    raw::reset_round2(spill, s, 0);
                }
                OWN_GROWN1.fetch_add(1, Ordering::Relaxed);
                fires::fire_shared("owner.regrown1");
                pin((2 * region.len).min(wall) & !7)?
            }
            Error::Unsupported(UNSUPPORTED_SCRATCH) if two_round => return Err(owner_wall(region.len, held)),
            e => return Err(e),
        };
        // a regrowth folds round 1 again: the cancel is read before it
        step.tick()?;
        merge.open_owner(region, 0)?;
        // D3111: the state saved before the partition that overflowed, laid over the grown region (the same form: a D597
        // domain miss changes it and folds again from partition 0), and round 1 goes on from that partition; its sources
        // are only read, and the state is what the partitions before it left
        let from = if bufs.saved == at && merge.kept_run_memo(&mut || kept_run(merge)) == kept_word && raw::owner_restore(region, &bufs.snap) {
            OWN_RESUMED.fetch_add(1, Ordering::Relaxed);
            fires::fire_shared("owner.resumed");
            at
        } else {
            0
        };
        bufs.saved = u32::MAX;
        r1 = round1(step, region, from, &mut bufs);
    }
    if merge.owned_rounds() == 2 {
        let pbits2 = merge.round2_pbits(region);
        if let Some(r) = merge.round2_partial(region) {
            let p2 = [Partial { ordinal: 0, bytes: raw::bytes_of(r), rows_processed: 0 }];
            let round2 = slot.and_then(|s| spill.stream(s).round2.first().and_then(Option::as_ref));
            let files2: [&dyn crate::SpillFile; 1] = [round2.map_or(&NO_FILE as &dyn crate::SpillFile, |f| f.as_ref())];
            let nf2 = round2.is_some() as usize;
            let parts2 = 1u32 << pbits2;
            // D520: the runs are a quarter of the region, which round 1 sized at the planner's groups; real groups past them rerun
            // round 2 alone from round 1's buffers and file (it only reads them) into a region grown by the partitions' share left
            let mut out = region;
            loop {
                let mut flush = || flush_round2(merge, spill, slot, out);
                let mut short = None;
                for q in 0..parts2 {
                    step.tick()?;
                    match merge.own_partition(out, q, pbits2, &p2, &files2[..nf2], site, cancel, 2, &mut flush) {
                        Err(Error::Unsupported(UNSUPPORTED_SCRATCH)) => {
                            short = Some(q);
                            break;
                        }
                        r => r?,
                    }
                }
                let Some(q) = short else { break };
                if out.len >= wall {
                    return Err(owner_wall(out.len, held));
                }
                OWN_GROWN2.fetch_add(1, Ordering::Relaxed);
                out = pin(round2_grown(out.len, q, parts2).min(wall) & !7)?;
                merge.open_owner(out, 0)?;
                merge.end_round(out, &mut || Err(Error::Unsupported(UNSUPPORTED_SCRATCH)))?;
            }
            region = out;
        }
    }
    Ok(Owners::one(region))
}

/// D520: a two-round region whose runs overflowed at round-2 partition `q` of `parts`: the partitions' share left over the done
/// ones, a quarter over for the hash's unevenness, at least twice the region. CR2-03: the size is the measured fill of the
/// `q + 1` partitions done (an estimate from this statement's own groups, not a tuned count); the quarter is slack, not a
/// threshold: a short estimate costs one more rerun, never an answer, and every rerun at least doubles the region up to the
/// wall, so a region reruns at most log2(wall / len) times whatever the factor.
fn round2_grown(len: usize, q: u32, parts: u32) -> usize {
    let scaled = len as u128 * 5 * parts as u128 / (4 * (q as u128 + 1));
    (scaled.min(usize::MAX as u128) as usize).max(2 * len)
}

/// D520: two-round owners whose round 2 reran in a grown region.
pub static OWN_GROWN2: AtomicU64 = AtomicU64::new(0);

/// D533: two-round owners whose round 1 folded again in a grown region (a partition past its route bits).
pub static OWN_GROWN1: AtomicU64 = AtomicU64::new(0);

/// D485: a split owner's table space past its runs (exec owner.rs `OWNER_TABLE_CAP`'s default: the runs form's table at most).
pub(crate) const SPLIT_TABLE: usize = 64 << 20;

/// D485: the probe that reads a kept-run form (its least table is `cost::runtime::KEPT_TABLE_MIN`).
const KEPT_PROBE: usize = 1 << 20;
/// The probe's zeroed prefix (a page: the open's header and more).
const KEPT_PROBE_ZEROED: usize = 4096;
const _: () = assert!(KEPT_PROBE_ZEROED >= gravity_abi::breaker::OWNERTOP_HDR && KEPT_PROBE_ZEROED <= KEPT_PROBE);

/// D485: the form's kept run (`OwnerTop.n > 0`: a count / top-k owner keeps at most n records) as its table offset, read from
/// an owner opened over a probe; `None`: the runs form, which keeps every group (or a run past the probe).
/// The probe keeps its length (an open may lay its table by it) but only its first page is zeroed: every one-round owner
/// open writes its `OwnerTop` header and reads nothing, and a merge with no owner form leaves a zero header (n = 0). A
/// zeroed probe was a 1 MiB memset per owner phase, a tenth of a SubPlan whose set operation's two groups re-run per row.
/// D2988: the probe is this thread's, made once and kept (its untouched pages cost no memory): a fresh 1 MiB allocation per
/// owner phase was drawn, faulted in and freed at every rescan of a SubPlan whose DISTINCT re-runs per outer row (~2% of
/// such a statement). A nested owner phase while the probe is out (none today) takes a fresh one.
/// D4531: the thread's probe is charged whole to the per-backend holder (`KEPT_PROBE_OWNER`): it outlives every statement
/// and session on its thread (a backend thread is pooled), so no session owns it.
pub(crate) fn kept_run(merge: &dyn Merge) -> Option<u64> {
    KEPT_PROBE_BUF.with(|b| match b.try_borrow_mut() {
        Ok(mut probe) => kept_run_in(merge, &mut probe, Some(KEPT_PROBE_OWNER)),
        Err(_) => kept_run_in(merge, &mut ChargedVec::none(), None),
    })
}

/// D4531: the holder a thread's kept-run probe is charged to.
const KEPT_PROBE_OWNER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PerBackend));

std::thread_local! {
    /// D2988: `kept_run`'s probe buffer on this thread.
    static KEPT_PROBE_BUF: core::cell::RefCell<ChargedVec<u64>> = const { core::cell::RefCell::new(ChargedVec::none()) }; // alloc: Node charged KEPT_PROBE_OWNER
}

fn kept_run_in(merge: &dyn Merge, probe: &mut ChargedVec<u64>, owner: Option<memledger::Owner>) -> Option<u64> {
    if probe.capacity() < KEPT_PROBE / 8 {
        *probe = ChargedVec::new(owner, memledger::Kind::Region); // alloc: Cold charged owner
        probe.try_reserve(KEPT_PROBE / 8).ok()?; // alloc: Cold charged owner
    }
    // the first page zeroed; the rest of the capacity (`KEPT_PROBE` bytes, the region's length) only `open` writes
    probe.clear();
    probe.resize(KEPT_PROBE_ZEROED / 8, 0).ok()?;
    let r = crate::sink::Region { ptr: probe.as_mut_ptr() as *mut u8, len: KEPT_PROBE };
    merge.open_owner(r, 0).ok()?;
    // the probe's first bytes, the header `open` wrote (a merge with no owner form leaves it zero: n = 0)
    let t = raw::owner_top(r);
    if t.n == 0 {
        return None;
    }
    let (off, n, resume) = (t.table_off as usize, t.n, t.resume as usize);
    // D3111: the kept state's end, from a second open over half the probe: a form whose table offset stays put keeps its
    // state before it (the count / top-k runs); one whose offset moves with the region keeps the state its `resume` names
    // when that stays put too (`OwnerTop::resume`, D3111: hosts distinct_fold's results before its accumulator space);
    // any other's is unknown (a run area that grows with the region), and an overflow folds again from partition 0
    probe[..KEPT_PROBE_ZEROED / 8].fill(0);
    let half = crate::sink::Region { ptr: r.ptr, len: KEPT_PROBE / 2 };
    let end = match merge.open_owner(half, 0) {
        Ok(()) => {
            let h = raw::owner_top(half);
            let (off2, resume2) = (h.table_off as usize, h.resume as usize);
            if h.n != n {
                None
            } else if off2 == off {
                Some(off)
            } else {
                (resume != 0 && resume2 == resume && resume <= off.min(off2)).then_some(resume)
            }
        }
        Err(_) => None,
    };
    Some(Kept { off, end: end.filter(|&e| e % 8 == 0 && e >= gravity_abi::breaker::OWNERTOP_HDR) }.word())
}

/// D485 / D3111: a kept-run form's probe: its table offset over the probe (`KEPT_PROBE` bytes) and the end of the state its
/// reduces keep across partitions (its header and kept rows, at offsets its region's size does not move), when known.
/// Packed in one word for `Merge::kept_run_memo`: the offset in the low 32 bits, the end + 1 above (0: unknown).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Kept {
    off: usize,
    end: Option<usize>,
}

// wasm32: the word is a u64 on every target, so the offset's 32 bits hold on 32-bit usize too.
const _: () = assert!((KEPT_PROBE as u64) < 1 << 32);

impl Kept {
    fn of(word: u64) -> Kept {
        Kept { off: (word & 0xffff_ffff) as usize, end: (word >> 32).checked_sub(1).map(|e| e as usize) }
    }

    fn word(self) -> u64 {
        self.off as u64 | self.end.map_or(0, |e| e as u64 + 1) << 32
    }

    /// D3111: the region whose table space holds `table` bytes behind its header: the probe's offset and the table, or for
    /// a form whose offset moves with the region (its own space past the kept state: a distinct fold's accumulators) the
    /// kept state and the table at the probe's share of table space past it.
    fn region(self, table: usize) -> usize {
        let t = gravity_abi::breaker::HASHAGG_HDR + table;
        match self.end {
            Some(e) if e < self.off && self.off < KEPT_PROBE => {
                let past = (t as u128 * (KEPT_PROBE - e) as u128).div_ceil((KEPT_PROBE - self.off) as u128);
                e.saturating_add(past.min(usize::MAX as u128) as usize)
            }
            _ => self.off.saturating_add(t),
        }
    }

    /// D3111: the region past an overflow of `len` at a partition of `part` record bytes (when counted): twice it, or at once
    /// one whose table holds four times that partition's records (a region the grant bounded below the records' size).
    fn grown(self, len: usize, part: Option<u64>) -> usize {
        let want = part.map_or(0, |b| self.region((4 * b as usize).max(KEPT_TABLE_MIN)));
        (2 * len).max(want)
    }
}

/// D3111: the kept state a partition saves when it costs no more than this whatever the partition's records (a count /
/// top-k run of a few rows: a page).
const KEPT_SAVE_FREE: usize = 4096;

/// D3111: one-round owners that resumed at the partition that overflowed, in the grown region (not from partition 0).
pub static OWN_RESUMED: AtomicU64 = AtomicU64::new(0);

/// D3111: `own_all`'s buffers on this thread, taken for one owner phase and given back after it (a nested phase on the
/// same thread takes empty ones): every partition's record bytes (`Merge::part_record_bytes`) and the kept state saved
/// before the partition `saved` (`raw::owner_save`; `u32::MAX`: none).
struct OwnBufs {
    parts: Vec<u64>,
    snap: Vec<u64>,
    saved: u32,
}

std::thread_local! {
    static OWN_BUFS: core::cell::Cell<(Vec<u64>, Vec<u64>)> = const { core::cell::Cell::new((Vec::new(), Vec::new())) }; // alloc: Node
}

impl OwnBufs {
    fn take() -> OwnBufs {
        let (parts, snap) = OWN_BUFS.with(core::cell::Cell::take);
        OwnBufs { parts, snap, saved: u32::MAX }
    }
}

impl Drop for OwnBufs {
    fn drop(&mut self) {
        let kept = (core::mem::take(&mut self.parts), core::mem::take(&mut self.snap));
        OWN_BUFS.with(|c| c.set(kept));
    }
}

/// The owners `own_all` leaves: `k` regions of `bytes` each from `base` (`Owned::owner`); one unless D485 split it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Owners {
    pub base: *mut u8,
    pub k: u32,
    pub bytes: usize,
}

impl Owners {
    fn one(r: crate::sink::Region) -> Owners {
        Owners { base: r.ptr, k: 1, bytes: r.len }
    }
}

/// D485: a one-round owner whose records ask past the wall (`owner_wall_at`; the u32 words of `OwnerTop` / `Owned`, D089) as
/// `k` serial owners in one pinned chunk, the parallel owner phase's layout (`Owned { nowners: k, owner_bytes, base }`, which
/// every consumer reads): owner `o` folds partitions `o·P/k ..` of every partial and file on this thread; the partitions are
/// disjoint, so are the owners' groups. `k` starts at the least power of two whose share of the records fits an owner
/// bounded by `walls.0` (D4078: the statement's wall, `raw::owner_wall_of`). An owner past its bytes (a skewed route: one
/// partition holding most groups) reruns from the sources, which the fold only reads (D285), at twice the owners, then at
/// twice the bytes up to `walls.1` (the format's wall); past both a named refusal.
/// D491: a chunk past the grant pool is a temp file mapped shared, so the owners never end the statement out of memory where
/// the session can write a temp file.
#[allow(clippy::too_many_arguments)]
fn own_split(merge: &dyn Merge, ctx: *mut gravity_abi::Ctx, partials: &[Partial<'_>], files: &[&dyn crate::SpillFile], site: *mut gravity_abi::Site, pbits: u8, held: usize, rec: usize, walls: (usize, usize), cancel: *const u8, spill: &crate::mem::spill::SpillTable, slot: Option<u8>, gate: &dyn Fn() -> bool) -> Result<Owners> {
    fires::fire_shared("owner.split"); // shapecheck D06/D08/D15: serial split owners over the partitioned records
    // C9: under `own_all`'s lent byte; each owner's partitions tick it
    let mut step = cancelpoll::step("section own_all: the split owners");
    let parts = 1u32 << pbits;
    let most = parts.min(gravity_abi::breaker::MAX_OWNERS as u32);
    // an owner's runs hold its share of the records (a quarter over, for the hash's unevenness); its table folds one partition
    // at a time at the region's end (exec owner.rs `owner_open`: half the region up to `SPLIT_TABLE`, grown after a split), so
    // the region is the runs and one table, never D438's twice-every-record `whole` (the pin is never given back before the
    // statement ends, so a rerun draws again)
    let per = |k: u32| {
        let runs = held.div_ceil(k as usize);
        let runs = (runs + runs / 4).max(gravity_abi::breaker::HASHAGG_HDR + 2 * rec).next_multiple_of(8);
        one_owner_bytes(0, rec).max(gravity_abi::breaker::OWNER_RUN_OFF + runs + runs.min(SPLIT_TABLE)).next_multiple_of(8)
    };
    let (split_wall, wall) = walls;
    let mut k = 2u32;
    // bounded("small-fixed: k doubles from 2 up to MAX_OWNERS (256), at most 7 rounds of arithmetic")
    while k < most && per(k) > split_wall {
        k *= 2;
    }
    // under the statement's wall unless even the most owners ask past it, then at most the format's (as before D4078)
    let mut bytes = per(k).min(wall);
    // D491: a chunk the grant pool refuses is a temp file mapped shared (`raw::map_owners`): the owners fold into it one at a
    // time as into a pinned chunk, and the page cache writes the finished ones back under memory pressure, so the statement
    // answers past the pool as PostgreSQL's hash aggregate spills (the chunk was the named out of memory). Once refused, a
    // rerun maps again (the last map given back) without a second wait on the pool.
    let mut mapped: Option<*mut u8> = None;
    loop {
        // a rerun at twice the owners or the bytes folds every partition again: the cancel is read before its pin
        step.tick()?;
        OWN_SPLIT.fetch_add(1, Ordering::Relaxed);
        let chunk = k as usize * bytes;
        // the node's ctx, bound for the statement; the chunk is pinned past the pipeline's arena rewind
        // the gate (`Node::owner_refuses`): a chunk the produced slot's fault refuses is mapped without asking the pool, as
        // `own_all`'s one owner is
        let gated = mapped.is_none() && gate();
        let from_pool = || {
            let base = raw::pin_zeroed(ctx, chunk, crate::node::ZERO_PREFIX.min(bytes));
            // D3236 (B): the produced slot's owners' chunk, given back when the slot frees
            raw::file_slot_pin(ctx, slot, base);
            base
        };
        let base = match mapped.take() {
            None if !gated => from_pool(),
            None => core::ptr::null_mut(),
            Some(last) => {
                raw::unmap_owners(ctx, last);
                core::ptr::null_mut()
            }
        };
        let base = if !base.is_null() {
            base
        } else if cancelpoll::seen() {
            // D3902: the pool's wait stopped for the statement's cancel (`own_all` lent its byte)
            return Err(Error::Cancelled);
        } else {
            let p = raw::map_owners(ctx, chunk)?;
            // a gated chunk in a session with no file road is the pool's, as before the gate
            let pinned = if p.is_null() && gated { from_pool() } else { core::ptr::null_mut() };
            if !pinned.is_null() {
                pinned
            } else if p.is_null() {
                if cancelpoll::seen() {
                    return Err(Error::Cancelled);
                }
                // NR-3: refused by the pool and by the temp-file road: the owners' chunk's road, named
                let pool = &crate::mem::grant::POOL;
                return Err(Error::OutOfMemory { bytes: chunk as u64, held: pool.held(), capacity: pool.capacity(), why: Some(gravity_program::spill::NoSpill::AggOwnerAll) });
            } else {
                OWN_SPILLED.fetch_add(1, Ordering::Relaxed);
                mapped = Some(p);
                p
            }
        };
        let fold = |o: u32| -> Result<()> {
            let region = raw::split_owner(base, o, bytes);
            merge.open_owner(region, 0)?;
            let mut flush = || flush_round2(merge, spill, slot, region);
            for part in o * parts / k..(o + 1) * parts / k {
                step.tick()?;
                merge.own_partition(region, part, pbits, partials, files, site, cancel, 1, &mut flush)?;
            }
            merge.end_round(region, &mut flush)
        };
        match (0..k).try_for_each(fold) {
            Ok(()) => return Ok(Owners { base, k, bytes }),
            Err(Error::Unsupported(UNSUPPORTED_SCRATCH)) if k < most => k *= 2,
            Err(Error::Unsupported(UNSUPPORTED_SCRATCH)) if bytes < wall => bytes = (2 * bytes).min(wall),
            Err(Error::Unsupported(UNSUPPORTED_SCRATCH)) => return Err(owner_wall(chunk, held)),
            Err(e) => return Err(e),
        }
    }
}

/// D1443: the records a continuing arm's worker partial holds when it is a phase-1 table (`T1::held`), else `None`.
fn continued_t1(cont: bool, bytes: &[u8]) -> Option<u64> {
    // a worker's partial region, idle after the section; a T1 header names its shape
    if cont { gravity_abi::breaker::T1::of(bytes).map(|t| t.held()) } else { None }
}

/// The one owner's round-2 buffers flushed into its round-2 file (entry 0 of `slot`'s), a reduce's REDUCE_FLUSH.
fn flush_round2(merge: &dyn Merge, spill: &crate::mem::spill::SpillTable, slot: Option<u8>, region: crate::sink::Region) -> Result<()> {
    let (Some(slot), Some(r2)) = (slot, merge.round2_partial(region)) else { return Err(Error::Unsupported(UNSUPPORTED_SCRATCH)) };
    // the one owner's round-2 buffers, this thread's alone; entry 0 of the slot's round-2 files is its own
    raw::flush_round2_at(spill, slot, 0, r2.ptr).map(|_| ())
}

/// The one owner's region over `held` bytes of records: `4 MiB + 2 × held` capped at the wall (D159: `held` counts every spilled
/// partial record, the run half needs only the distinct groups, a full table splits; a result past the run half is the fold's named
/// refusal), grown to the shape's floor (`fits`); past the wall a named refusal.
pub(crate) fn owner_region_bytes(ask: usize, held: usize, fits: impl Fn(u32) -> bool) -> Result<usize> {
    let mut bytes = ask.min(OWNER_WALL);
    // bounded("other: a doubling up to OWNER_WALL, at most 64 rounds of one `fits` test each")
    while !fits(bytes as u32) {
        if bytes >= OWNER_WALL {
            return Err(owner_wall(ask.max(bytes), held));
        }
        bytes = bytes.saturating_mul(2).min(OWNER_WALL);
    }
    Ok(bytes)
}

/// The least region (64 KiB steps) at or over the records' ask that holds the shape's floor (the doubling overshoots it).
fn least_fit(top: usize, ask: usize, fits: impl Fn(usize) -> bool) -> usize {
    if top <= ask {
        return top;
    }
    let mut lo = if ask == 0 { 0 } else { ask.max(top / 2) & !7 };
    if fits(lo) {
        return lo;
    }
    let mut hi = top;
    // bounded("other: a bisection of [lo, top) down to 64 KiB, at most 64 rounds of one `fits` test each")
    while hi - lo > 1 << 16 {
        let mid = ((lo + hi) / 2) & !7;
        if fits(mid) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    hi
}

/// The one-round owner's runs layout over `held` bytes of `rec`-byte records (exec/src/owner.rs:31-33 `RUN_OFF`, :809-820 `owner_open`):
/// header and run directory, then two equal halves, the result runs (at most `held`) and the fold's table (its header, two records).
pub fn one_owner_bytes(held: usize, rec: usize) -> usize {
    use gravity_abi::breaker::{HASHAGG_HDR, OWNER_RUN_OFF};
    let half = held.max(HASHAGG_HDR + 2 * rec).next_multiple_of(8);
    OWNER_RUN_OFF.next_multiple_of(8) + 2 * half
}

/// D438: `one_owner_bytes` with a table half of twice the `held` bytes' records in slots (a power of two): a partition's table
/// (`fold_sub`: twice its estimated records, at least 64) is never cut under twice its records, whatever the routes.
pub fn one_owner_whole_bytes(held: usize, rec: usize) -> usize {
    use gravity_abi::breaker::HASHAGG_HDR;
    let slots = (2 * held.div_ceil(rec.max(1))).max(2).next_power_of_two();
    one_owner_bytes(held.max(HASHAGG_HDR + slots * rec), rec)
}

/// KN-02 (B): one of `width` owners' regions over `held` bytes of `rec`-byte records: its share of them, a quarter over for
/// the hash's unevenness (`own_split`'s share), laid as one owner's runs layout (`one_owner_bytes`), one partition's table
/// for a two-round form (F7, as `own_all`), grown to the least region the merge's owners take (`fits`, its floor at the
/// partials' owners) and trimmed to it (`least_fit`); `None` past the wall.
pub(crate) fn owner_share_bytes(held: usize, rec: usize, width: u16, pbits: u8, two_round: bool, fits: impl Fn(usize) -> bool) -> Option<usize> {
    let share = held.div_ceil(width.max(1) as usize);
    let ask = one_owner_bytes(share + share / 4, rec);
    let ask = if two_round { ask.max(2 * (gravity_abi::breaker::HASHAGG_HDR + (4 * held >> pbits).max(64 * rec))) } else { ask };
    let top = owner_region_bytes(ask, share, |b| fits(b as usize)).ok()?;
    Some(least_fit(top, ask, fits))
}

/// The largest one-owner region: `Owned::owner_bytes` and the merges' region words are `u32` (D089: an 8.2 GB ask truncated to a
/// wrong size and came back `unsupported execution (kind 2)`).
pub(crate) const OWNER_WALL: usize = u32::MAX as usize & !7;

/// D485's test wall: 0 is `OWNER_WALL`; a test sets a small wall so a few thousand records split the one owner.
#[doc(hidden)]
pub static TEST_OWNER_WALL: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// D491's test lever: `PGRUST_GRAVITY_OWNER_WALL` (bytes, the server's environment, read once per process) lowers the wall as
/// `TEST_OWNER_WALL` does, so an e2e reaches the serial owners (`own_split`) at test scale (an e2e file's .env sets it;
/// PostgreSQL ignores it). Unset, 0 or unparsable: `OWNER_WALL`; at least 64 KiB. Cached in an atomic (0 = not read yet; the
/// value is never 0), not a once: two first readers compute the same value from the same environment.
fn env_owner_wall() -> usize {
    static WALL: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
    match WALL.load(Ordering::Relaxed) {
        0 => {
            let w = crate::env::num::<usize>("PGRUST_GRAVITY_OWNER_WALL").filter(|&w| w != 0).map_or(OWNER_WALL, |w| w.clamp(64 << 10, OWNER_WALL) & !7);
            WALL.store(w, Ordering::Relaxed);
            w
        }
        w => w,
    }
}

/// D3111's test lever: a kept-run form's first one-owner region at most `TEST_OWNER_FIRST` bytes (0: the server's
/// `PGRUST_GRAVITY_OWNER_FIRST`, read once per process; unset, 0 or unparsable: no cap; at least 64 KiB), so a test reaches
/// a partition past the region, grown and resumed at that partition, at test scale (an e2e file's .env sets it; PostgreSQL
/// ignores it).
pub static TEST_OWNER_FIRST: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

fn owner_first_at() -> usize {
    static FIRST: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
    match TEST_OWNER_FIRST.load(Ordering::Relaxed) {
        0 => match FIRST.load(Ordering::Relaxed) {
            0 => {
                let f = crate::env::num::<usize>("PGRUST_GRAVITY_OWNER_FIRST").filter(|&f| f != 0).map_or(usize::MAX, |f| f.max(64 << 10) & !7);
                FIRST.store(f, Ordering::Relaxed);
                f
            }
            f => f,
        },
        f => f.max(64 << 10) & !7,
    }
}

pub(crate) fn owner_wall_at() -> usize {
    match TEST_OWNER_WALL.load(Ordering::Relaxed) {
        0 => env_owner_wall(),
        w => w.min(OWNER_WALL) & !7,
    }
}

/// D485: one-round owners split into k serial owners past the wall (`own_split`), each rerun counted.
pub static OWN_SPLIT: AtomicU64 = AtomicU64::new(0);

/// D491: serial owners' chunks the grant pool refused, mapped from a temp file instead (`own_split`), each rerun counted.
pub static OWN_SPILLED: AtomicU64 = AtomicU64::new(0);

/// Owner draws the produced slot's fault refused (`Node::owner_refuses`): the owners' region mapped from a
/// temp file instead of pinned (`own_all`, `own_split`), the pooled owners not pinned (`own_past_grant_at`), the leader
/// not committed in place (`owners_in_place`).
pub static OWN_GATED: AtomicU64 = AtomicU64::new(0);

pub static OWN_CUT: AtomicU64 = AtomicU64::new(0);
/// KN-02 (B): owner phases whose owners `own_past_grant` pinned past the leader region.
pub static OWN_PAST_GRANT: AtomicU64 = AtomicU64::new(0);
/// D3322: section opens whose owner cut `owner_within` held to a leader region smaller than the grant's split.
pub static OWNER_CUT_HELD: AtomicU64 = AtomicU64::new(0);
pub static OWN_PINNED: AtomicU64 = AtomicU64::new(0);
pub static OWN_BOUNDED: AtomicU64 = AtomicU64::new(0);
/// D4078 (ADMITFP (A)): the bytes of owner regions a round 1 overflowed at its statement's wall,
/// given back before the split pinned its chunk (`own_all_lent`), this process.
pub static OWN_PIN_GIVEN: AtomicU64 = AtomicU64::new(0);

/// D3322: an owner's cut (`Open::owner_region`: the leader region behind its `Owned` header, `width` ways) of the leader region
/// the produced slot holds (`leader_len` bytes), never of more: the grant's cut (`Split::owner`) when the region holds it.
/// A leader share the full pool refused opens at the pool's room (D3226 (c1), `share_at_room`; `Node::open_slot` arms the
/// slot at it): owners cut at the grant's share from such a region ran past its end (a 15780 MiB grant under a
/// 56 GiB limit: a 4 GiB leader share refused with the pool full, then owners SIGSEGV at their cuts' edges, two faults one
/// W8 cut (0x1FFFFFF0) apart). A cut held smaller takes the owner phase's own roads (`owned_partitions` at it, else
/// `own_past_grant`, else the one owner).
pub(crate) fn owner_within(split_owner: u32, leader_len: usize, width: u16) -> u32 {
    let held = crate::mem::grant::owner_cut(crate::mem::grant::Share { bytes: leader_len.min(u32::MAX as usize) as u32 }, width).bytes;
    if held < split_owner {
        OWNER_CUT_HELD.fetch_add(1, Ordering::Relaxed);
    }
    split_owner.min(held)
}

/// A one-owner region of `bytes`: cut in the idle leader region (`spare`; all of it for a two-round shape's floor), else pinned
/// inside the grant's `room` (`None`: unbounded), else refused by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerPlace {
    Cut(usize),
    Pin,
    Refuse(u64),
}

pub fn owner_place(bytes: usize, spare: usize, spare_holds_floor: bool, room: Option<u64>) -> OwnerPlace {
    match room {
        _ if spare >= bytes => OwnerPlace::Cut(bytes),
        _ if spare_holds_floor => OwnerPlace::Cut(spare),
        Some(r) if r < bytes as u64 => OwnerPlace::Refuse(r),
        _ => OwnerPlace::Pin,
    }
}

/// The wall met: a named refusal, the ask and the records behind it in the text.
pub(crate) fn owner_wall(bytes: usize, held: usize) -> Error {
    Error::Refused(format!("graph: owner: one serial owner over {held} bytes of held and spilled records asks a {bytes}-byte region, past the {OWNER_WALL}-byte one-owner wall").into()) // alloc: Cold
}

/// D4170: an owner fold over a merge with no owner phase (`Merge::own_partition`'s default, a hash aggregate's merge with no
/// owner functions), refused by name: `own_all` returns it at once. Read as kind 2 (a region too small) it ended in the
/// one-owner wall's refusal, the region grown and pinned again up to the wall, or an open's floor sought past it.
pub fn no_owner_phase() -> Error {
    Error::Refused("graph: owner: an owner fold over a merge with no owner phase".into())
}

/// A file that holds nothing (the array's filler).
pub(crate) struct NoFile;

pub(crate) static NO_FILE: NoFile = NoFile;

impl crate::SpillFile for NoFile {
    fn write_epoch(&mut self, _parts: &[(u32, &[u8])]) -> Result<()> {
        Ok(())
    }

    fn read_part(&self, _part: u32, _sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        Ok(())
    }

    fn open_part(&self, _part: u32) -> Result<Option<Box<dyn crate::RunReader>>> {
        Ok(None)
    }

    fn spilled_bytes(&self) -> u64 {
        0
    }
}

impl Entry {
    pub fn open_section<'e>(&mut self, exec: &'e mut Exec, binding: &Binding<'_>, derive: &mut dyn Derive, pipeline: usize, width: u16) -> Result<Open<'e>> {
        assert!(matches!(exec.state(), ExecState::Ready | ExecState::Running), "section on a {:?} exec", exec.state());
        assert_eq!(exec.next_pipeline(), pipeline, "sections run in pipeline order");
        assert!(width >= 2, "a width-{width} grant is serial");
        let node = exec.node_mut();
        assert_eq!(node.state(), NodeState::Bound);
        let program = node.program().clone();
        let p = program.pipelines[pipeline];
        assert!(p.parallel == Parallel::Eligible, "section over an ineligible pipeline");
        if !node.is_opened(pipeline) {
            crate::ahead_trace::first_run(&program, pipeline); // construct C2 (b)
        }
        // sort give-back: a revoke asked since the leader's last service puts the statement's finished sorts no reader has
        // begun before the section reads the regions it consumes (`Entry::claim_at`'s, for a serial claim)
        if let Err(e) = node.give_back_sorts() {
            exec.set_state(ExecState::Failed);
            return Err(e);
        }
        let t0 = std::time::Instant::now();
        let salt = exec.slot_salt();
        if let Err(e) = exec.node_mut().provision(width, derive, salt) {
            exec.set_state(ExecState::Failed);
            return Err(e);
        }
        self.stats.provision_us += t0.elapsed().as_micros() as u64;
        let t0 = std::time::Instant::now();
        let arena = crate::mem::grant::worker_arena_for(exec.node().program(), exec.grant(), width);
        let split = exec.node().share(pipeline, exec.grant(), width);
        let node = exec.node_mut();
        let consumed = match node.consumed_ptrs(pipeline) {
            Ok(c) => c,
            Err(e) => {
                exec.set_state(ExecState::Failed);
                return Err(e);
            }
        };
        let breaker = p.breaker();
        if let Some(b) = breaker {
            // a refused region (the pool) is the statement's error here: a leader left unopened
            // reached the owner phase as a 0-byte region and failed as `UNSUPPORTED_SCRATCH`
            // D2648: an arm run's earlier serial arms keep their records in the leader's table (node `end_production`); the
            // run's owner after a section reads the leader's records from the slot's files (close_section), so they go there
            // before this section's workers start, as every continuing arm's did before
            // (NEW-53: a resident keyed end's leader records are handed on: `Node::hand_leader`)
            let opened = node.open_slot(b.slot, pipeline, split.breaker, split.images.bytes, width).and_then(|()| if node.in_arm_run(pipeline) { node.spill_leader(pipeline).and_then(|_| node.hand_leader(pipeline)).map(|_| ()) } else { Ok(()) });
            if let Err(e) = opened {
                exec.set_state(ExecState::Failed);
                return Err(e);
            }
        }
        let mut bound = 0;
        let mut failed = None;
        let own_slot = node.own_slot_of(pipeline);
        // LEDGER2 step 5: the slot's owner (`open_slot` began it above): each worker's draws for this pipeline charge it
        let op_owner = node.slot_owner(own_slot);
        // CONVERT wave 2 (R2, R3): the produced slot's owner and kind, and the statement's owner, for the workers' sources
        let (feed_owner, stmt_owner) = (node.feed_owner_of(pipeline), node.stmt_owner());
        // CONVERT wave 2 (commit 7): the own slot's breaker kind, beside `op_owner` (the spill host's own-slot answer)
        let own_kind = node.own_kind_of(pipeline);
        let reads = node.reads_of(pipeline);
        let arm = node.arm_flag(pipeline);
        // D3014 S2': a section inside a nested run (a rerun's member) takes the leader's frame words (`copy_frame_words`)
        let (nested, leader_ctx) = (node.is_nested(), node.ctx());
        let stmt = raw::stmt_of(node);
        // option c': a set operation side's worker regions bind inside a reservation of its rows bound (0 = none)
        // D2894: or of the breaker's PostgreSQL floor where it passes the worker's modeled share (`grow::floor_reserve`)
        let (mut reserve, mut floor) = crate::mem::grow::share_reserve(&p, stmt.map(|s| s.grant), split.worker.bytes as u64, width, false);
        // each worker's own ceiling (`grant::ceiling_of`): its partial's grows' bound, as each PostgreSQL worker's work_mem
        let worker_ceil = stmt.map_or(0, |s| crate::mem::grant::ceiling_of(s.grant, &p));
        let worker_fault = crate::mem::grow::Fault::every(stmt.map_or(0, |s| s.grant.spill_every)).with_revoke_every(stmt.map_or(0, |s| s.grant.revoke_every)).with_refuse_debt(stmt.is_some_and(|s| s.grant.refuse_debt));
        // C10 / NR-1: `WorkerSlot::share_spills`, `share_debt`
        let worker_holds = if program.lowered() { p.holds() } else { None };
        let (worker_spills, worker_debt) = (worker_holds.is_some_and(|h| h.spills()), worker_holds.and_then(|h| h.debt()));
        // a hash breaker's workers start small: the share is the layout each grows into in place, the region commits the start
        let start = crate::mem::grant::start_bytes(split.worker.bytes);
        let laid = crate::mem::grant::starts_small_in(&program, pipeline, stmt.map_or(crate::mem::grant::Grant::of(0), |s| s.grant)) && start < split.worker.bytes;
        let worker_bytes = if laid {
            (reserve, floor) = (crate::mem::grow::start_reserve(&p, stmt.map(|s| s.grant), split.worker.bytes as u64, width, false), true);
            start
        } else {
            split.worker.bytes
        };
        // the ordinals past this width hold no partial (a section's partials merge or spill at its close): a share-budgeted
        // section gives their share regions back first, so its own workers' shares (the breaker cut `width` ways) fit G as the
        // footprint counts them, not beside an earlier wider section's regions (a union's arms at widths 3 then 2 drew a
        // 32 MiB region past W and G every statement, zeroed)
        if breaker.is_some_and(|b| b.budget.is_share()) {
            // bounded("small-fixed: one per worker slot past the section's width, at most SLOTS (256)")
            for slot in node.workers.iter_mut().skip(width as usize) {
                slot.release_share();
            }
        }
        // D3196 (rearm E1): a slot a nested run's last section left warm under this binding keeps its hosts
        let (key, mut kept) = (Warm::of(&binding.parts), 0u16);
        // D4725: the sites no worker reads, which no slot binds
        let skip = if crate::slots::serial_skip() { crate::slots::serial_sites(&program.pipelines) } else { 0 };
        // bounded("small-fixed: one per worker slot of the section's width, at most SLOTS (256)")
        for (i, slot) in node.workers.iter_mut().take(width as usize).enumerate() {
            let tz = std::time::Instant::now();
            slot.zero_regions();
            slot.rewind(program.layout.out_rows);
            self.stats.zero_us += tz.elapsed().as_micros() as u64;
            let wb: WorkerBinding<'_> = binding.fork(i as u16, MorselRange::ALL);
            let wb: WorkerBinding<'static> = raw::erase(wb);
            slot.binding = Some(wb);
            // D4630 (b): under `bind_at_claim` a slot not warm for this binding binds at its first claim, on the claiming
            // thread (`Open::claim_on`), so the open binds none of the width's slots one after another on the leader
            slot.skip = skip;
            if crate::slots::bind_at_claim() && slot.warm.is_none_or(|w| w != key) && binds_at_claim(&mut slot.inner, skip) {
                if slot.warm.take().is_some() {
                    slot.inner.for_each_host(&mut |h| h.unbind());
                }
                slot.bind_pending = true;
            } else {
                let b = slot.binding.as_mut().unwrap();
                let bref = b.as_ref();
                match bind_hosts(&mut slot.warm, &mut slot.inner, &bref, key, skip) {
                    Ok(warm) => kept += warm as u16,
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                }
            }
            let b = slot.binding.as_mut().unwrap();
            raw::bind_ctx(&slot.inner, b, i as u16, width, arm, split.worker.bytes as u64, leader_ctx);
            if nested {
                raw::copy_frame_words(&program, leader_ctx, slot.inner.ctx);
            }
            // G1-A: the worker's breaker region counts under its kind in the ledger's per-kind split
            let _kind = memledger::with_kind(crate::mem::region::ledger_kind(breaker.map(|b| b.kind)));
            let own = match breaker {
                Some(b) => {
                    let region = match b.budget {
                        Budget::Fixed(n) => slot.fixed_region(n, stmt, &node.counters).map(|ptr| (ptr, n, 0)),
                        Budget::Share | Budget::Pool => {
                            node.counters.armed(worker_bytes);
                            slot.share_region(worker_bytes, stmt, &node.counters, worker_bytes as u64 + split.images.bytes as u64, reserve, floor, if laid { split.worker.bytes } else { 0 }).map(|(ptr, grown, held)| {
                                node.counters.grown_bytes.set(node.counters.grown_bytes.get() + grown as u64);
                                let short = (split.images.bytes as u64).saturating_sub(slot.inner.w);
                                slot.inner.w += crate::mem::grant::POOL.carve_worker(stmt, short);
                                // D3226 (c1): a share the pool's room opened smaller is armed at what it holds (`state_arm`), so it spills there
                                (ptr, held, held)
                            })
                        }
                    };
                    let (ptr, bytes, arm) = match region {
                        Ok(r) => r,
                        Err(e) => {
                            // a refused share of a breaker that names a SPILL-DEBT row names it (`grow::debt_refused`)
                            failed = Some(crate::mem::grow::debt_refused(e, worker_debt, None));
                            slot.state_ptr = core::ptr::null_mut();
                            slot.state_bytes = 0;
                            slot.state_arm = 0;
                            slot.state_stale = false;
                            slot.share_ceil = 0;
                            slot.share_fault = crate::mem::grow::Fault::OFF;
                            slot.share_spills = false;
                            slot.share_debt = None;
                            slot.share_cut = 0;
                            break;
                        }
                    };
                    slot.state_ptr = ptr;
                    slot.state_bytes = bytes;
                    slot.state_arm = arm;
                    slot.state_stale = true;
                    slot.share_ceil = if b.budget.is_share() { worker_ceil } else { 0 };
                    slot.share_fault = if b.budget.is_share() { worker_fault } else { crate::mem::grow::Fault::OFF };
                    slot.share_spills = worker_spills;
                    slot.share_debt = worker_debt;
                    // D4033: a share the pool's room opened smaller is cut by the section's share (`WorkerSlot::share_cut`)
                    slot.share_cut = if b.budget.is_share() && arm < worker_bytes { split.worker.bytes } else { 0 };
                    ptr
                }
                None => {
                    slot.state_ptr = core::ptr::null_mut();
                    slot.state_bytes = 0;
                    slot.state_arm = 0;
                    slot.state_stale = false;
                    slot.share_ceil = 0;
                    slot.share_fault = crate::mem::grow::Fault::OFF;
                    slot.share_spills = false;
                    slot.share_debt = None;
                    slot.share_cut = 0;
                    consumed.consumed[0].ptr()
                }
            };
            slot.inner.own_slot = own_slot;
            (slot.inner.op_owner, slot.inner.own_kind) = (op_owner, own_kind);
            (slot.inner.feed_owner, slot.inner.stmt_owner) = (feed_owner, stmt_owner);
            // LEDGER2 step 5: the worker's partial region for this slot charged to its owner
            if let Some(o) = op_owner {
                slot.own_partial(o);
            }
            (slot.inner.consumes, slot.inner.spill_parts) = reads;
            slot.inner.home = if slot.state_ptr.is_null() { crate::mem::grow::Home::None } else { crate::mem::grow::Home::Worker(slot as *mut WorkerSlot) };
            slot.inner.refill_off = 0;
            slot.inner.clear_mapped();
            // S4 (D187): the worker's arena carved into its W beside the image cut; an admitted worker draws W alone
            slot.inner.w_only = stmt.is_some_and(crate::mem::grant::Reservation::admitted);
            let images = if breaker.is_some_and(|b| b.budget.is_share()) { split.images.bytes as u64 } else { 0 };
            slot.inner.w += crate::mem::grant::POOL.carve_worker(stmt, (arena + images).saturating_sub(slot.inner.w));
            slot.inner.fill_base = core::ptr::null_mut();
            slot.inner.set_partials(Partials { own: gravity_abi::word(own), ..consumed });
            let attached: &mut WorkerBinding<'static> = slot.binding.as_mut().unwrap();
            slot.inner.attach(attached);
            bound += 1;
        }
        self.stats.bind_us += t0.elapsed().as_micros() as u64;
        if let Some(e) = failed {
            // bounded("small-fixed: one per worker slot of the section's width, at most SLOTS (256)")
            for slot in node.workers.iter_mut().take(bound + 1) {
                detach(slot);
            }
            exec.set_state(ExecState::Failed);
            return Err(e);
        }
        if kept != 0 {
            fires::fire("exec.rerun.warm");
            // a wider open: the warm slots kept, the ones past them bound
            if kept < width {
                fires::fire("exec.rerun.warm_wider");
            }
        }
        let slots = node.workers.as_mut_ptr();
        let program_ptr: *const Program = &**node.program();
        let leader = match breaker {
            Some(b) => {
                let r = node.slot_region(b.slot);
                (r.as_mut_ptr(), r.len())
            }
            None => (core::ptr::null_mut(), 0),
        };
        // D3322: the owners cut the leader region the slot holds, which a refused share opened smaller than the grant's split
        // (`past_g_share`). Only a share-budgeted breaker's region opens at the share (and so may open smaller); a fixed one's
        // region is its fixed state, never the owners', and keeps the grant's cut as before. A leader region that started small
        // (`grant::starts_small`) is held the same way: its owners are cut from what it holds, and past that the owner phase
        // sizes them from the partials' records (`owners_in_place`, `own_past_grant`)
        let owner = if breaker.is_some_and(|b| b.budget.is_share()) && leader.1 > 0 { owner_within(split.owner.bytes, leader.1, width) } else { split.owner.bytes };
        // D4043: G's room past the W carves in the statement's purse, which its workers draw past W before the pool
        let purse = stmt.and_then(crate::mem::grant::Reservation::fill_purse);
        exec.set_state(ExecState::Running);
        // an instrumented program's counter cells only: a plain program's are empty and open no charge
        let cells = exec.node().instr_cells();
        let mut cells0 = ChargedVec::none();
        if !cells.is_empty() {
            cells0 = ChargedVec::new(stmt_owner.map(|o| o.0), memledger::Kind::Region); // alloc: Exec charged stmt_owner.map(|o| o.0)
            cells0.with_vec(|v| v.extend_from_slice(cells)); // alloc: Exec charged stmt_owner
        }
        Ok(Open { exec, cells0, slots, program: program_ptr, width, pipeline, leader: core::cell::Cell::new(leader), owner: core::cell::Cell::new(owner), owners_base: core::cell::Cell::new(core::ptr::null_mut()), merged: AtomicBool::new(false), lead: false, purse })
    }

    pub fn close_section(&mut self, open: Open<'_>, binding: &mut Binding<'_>, merge: &dyn Merge, ok: bool) -> Result<()> {
        let merged = open.merged.load(Ordering::Acquire);
        let Open { exec, width, pipeline, cells0, purse, .. } = open;
        let node = exec.node_mut();
        // D4043: the claims are done: the purse's bytes back in G's room for the close's folds (the outermost section's)
        let stmt = purse.as_ref().and_then(|_| raw::stmt_of(node));
        if let Some(s) = stmt {
            s.drain_purse(true);
        }
        let produced = node.program().pipelines[pipeline].produces();
        let mut r = Ok(());
        // D393b: an arm that continues its run folds only: its partials go to the slot's files (a record table has no serial
        // fold) and the leader's own table is laid empty in their shape when no claim laid it, so the run's last arm owns
        // from a live table even when it has no rows of its own (the owner reads the files' records in the partials' layout);
        // `end_production` scatters the leader; no finish: the run's last arm owns, finishes and seals every arm's records
        let cont = node.arm_continues(pipeline);
        if ok {
            let spilling = node.program().pipelines[pipeline].breaker().is_some_and(|b| matches!(b.spill, gravity_abi::breaker::Spill::Partitioned { .. }));
            // B4d (M7-DRAIN): a hash build's continuing arm merges its partials into the leader's region like any build (a v4
            // build spills its own partitions, join/build_spill.rs; spill_partial skips one): the T1 scatter below is a record table's
            let build = node.program().pipelines[pipeline].breaker().is_some_and(|b| b.kind == gravity_abi::breaker::BreakerKind::HashBuild);
            // NEW-53 (D1443): a resident keyed end's continuing arm hands its phase-1 tables to the run's last arm as the word
            // form scatters them (in memory while the grant holds them, else to disk: `Node::hand_off`)
            let hands = cont && node.hands_off(pipeline);
            let n = node.workers.iter().take(width as usize).filter(|s| !s.state_stale).count();
            // exactsize B1b: a hash build's serial fold sized once from its partials' counted headers (BuildMerge under
            // pgrust.gravity_exact_size = build | all; a no-op otherwise)
            if !merged && !cont && build && n > 0 {
                let home = produced.map(|s| node.leader_home(s));
                let (state, slots) = node.slot_and_workers(produced);
                let mut partials: [Partial<'_>; SLOTS] = [EMPTY; SLOTS];
                let mut k = 0;
                for slot in slots.iter().take(width as usize).filter(|s| !s.state_stale).take(SLOTS) {
                    partials[k] = Partial { ordinal: slot.binding.as_ref().map_or(0, |b| b.ordinal), bytes: slot.state(), rows_processed: slot.rows_processed() };
                    k += 1;
                }
                let mut leader = State { bytes: state, sink: binding.sink, home };
                r = merge.prepare_serial(&mut leader, &partials[..k]);
            }
            let mut shape: Option<(gravity_abi::breaker::RecordLayout, u8, u32)> = None;
            // C9: the leader's folds (`merge_into`) run between kernel calls: the statement's byte is lent to them, so their
            // steps (parallel merge.rs) read it
            let lead = node.ctx();
            // bounded("small-fixed: one partial per worker slot of the section's width, at most SLOTS (256); each fold ticks its own Step")
            for i in 0..n {
                if r.is_err() {
                    break;
                }
                // D1713: the leader's grow home is taken per fold, right before the slot's reborrow (as `leader_state`
                // does): one home taken for the whole close was frozen by the next fold's `slot_and_workers`, and a
                // second grow of the leader in one close wrote through it.
                let home = produced.map(|s| node.leader_home(s));
                let (state, slots) = node.slot_and_workers(produced);
                let slot = slots.iter().take(width as usize).filter(|s| !s.state_stale).nth(i).expect("a partial");
                let ordinal = slot.binding.as_ref().map_or(0, |b| b.ordinal);
                let partial = Partial { ordinal, bytes: slot.state(), rows_processed: slot.rows_processed() };
                let rows = slot.rows_processed();
                if !merged && cont && spilling && !build {
                    let ptr = slot.state_ptr;
                    // the worker's own partial region (`ptr`), idle after the section; a T1 header names its shape
                    if let Some(t) = shape.is_none().then(|| gravity_abi::breaker::T1::of(partial.bytes)).flatten() {
                        shape = Some((t.layout, t.pbits, t.cap.saturating_mul(t.layout.size() as u32)));
                    }
                    if let Err(e) = node.spill_partial(pipeline, ptr) {
                        r = Err(e);
                        break;
                    }
                } else if let (false, true, Some(t1)) = (merged, hands, gravity_abi::breaker::T1::of(partial.bytes).map(|t| (t.layout, t.pbits, t.cap.saturating_mul(t.layout.size() as u32)))) {
                    let ptr = slot.state_ptr;
                    // NEW-53: a phase-1 table, as the word form's above; the leader's table is laid in the shape once a record went
                    // on (a RIGHT SEMI / ANTI probe's tables stay empty: nothing handed, the leader left as before)
                    match node.hand_off(pipeline, ptr) {
                        Ok(0) => {}
                        Ok(_) => shape = shape.or(Some(t1)),
                        Err(e) => {
                            r = Err(e);
                            break;
                        }
                    }
                } else if !merged {
                    // a spilling breaker never half-folds a partial: no room → scatter the leader first;
                    // a partial too big for the emptied leader table goes to the files as it is
                    let needs_spill = spilling && !merge.room_for(state, partial);
                    if needs_spill {
                        if let Err(e) = node.spill_leader(pipeline) {
                            r = Err(e);
                            break;
                        }
                        let (state, slots) = node.slot_and_workers(produced);
                        let slot = slots.iter().take(width as usize).filter(|s| !s.state_stale).nth(i).expect("a partial");
                        let partial = Partial { ordinal, bytes: slot.state(), rows_processed: slot.rows_processed() };
                        if !merge.room_for(state, partial) {
                            let ptr = slot.state_ptr;
                            if let Err(e) = node.spill_partial(pipeline, ptr) {
                                r = Err(e);
                                break;
                            }
                        } else {
                            continue_fold(node, produced, ordinal, i, width, binding, merge, &mut r);
                        }
                    } else if let Some(held) = continued_t1(cont, partial.bytes) {
                        // D1443: a continuing arm whose end stays resident (no partitioned spill: a varlena key, D6) and hands
                        // nothing on (NEW-53: a keyed end hands its tables, above) has no serial fold for its phase-1 tables;
                        // an empty one has nothing to fold, the run's last arm owns the rest, any other is refused by name
                        if held != 0 {
                            r = Err(Error::Refused(format!("graph: keyed end: pipeline {pipeline}, an arm continuing its run left {held} records in a resident phase-1 table with no fold (D1443)").into())); // alloc: Cold
                        }
                    } else {
                        let mut leader = State { bytes: state, sink: binding.sink, home };
                        if let Err(e) = raw::lend_cancel(lead, || merge.merge_into(&mut leader, partial)) {
                            r = Err(e);
                        }
                    }
                    if r.is_err() {
                        break;
                    }
                }
                binding.rows_processed += rows;
            }
            if let (true, false, Some((layout, pbits, bytes)), Some(slot)) = (r.is_ok() && cont, merged, shape, produced) {
                node.lay_leader_t1(slot, &layout, pbits, bytes);
            }
            if r.is_ok() && !merged {
                r = node.end_production(pipeline);
            }
            if r.is_ok() && !cont {
                let home = produced.map(|s| node.leader_home(s));
                let (state, _) = node.slot_and_workers(produced);
                let mut leader = State { bytes: state, sink: binding.sink, home };
                r = merge.finish(&mut leader);
            }
        }
        // D254: the workers' grows past their shares counted (the statement's first logged), the buffers they replaced settled
        let kind = produced.and_then(|s| crate::mem::grow::kind_of(node.program(), s));
        // KEYED-22: each worker's site rows (in / out) summed into the node ctx's and zeroed in its slot, so the node counts
        // the execution's rows at any width (observe's rows folded, EXPLAIN's rows scanned / survived)
        let (lead, nsites) = (node.ctx(), node.program().sites.len());
        let (lead_instr, cells) = (node.instr_ptr(), node.program().instr.len());
        // bounded("small-fixed: one per worker slot of the section's width, at most SLOTS (256)")
        for slot in node.workers.iter_mut().take(width as usize) {
            slot.settle_retired();
            crate::mem::grow::fold(&mut slot.inner.grew, &node.counters, kind);
            raw::fold_site_rows(slot.ctx(), lead, nsites);
            raw::fold_instr(slot.inner.block.instr.ptr(), lead_instr, cells);
        }
        raw::settle_loops(lead_instr, &node.program().instr, &cells0);
        node.fold_worker_arenas(width);
        // D1408: a pin a claim left behind is its thread's: the source leaks it (a WARNING) and the section fails by name
        if r.is_ok() && node.workers.iter().take(width as usize).any(|s| s.binding.is_some() && s.pins() != 0) {
            r = Err(Error::host("gravity: D1408 a worker slot still holds buffer pins after its section"));
        }
        // D3196 (rearm E1): a nested run's section (a rescan's member, which the next rescan opens again) leaves its slots'
        // hosts bound, warm for this binding (an open of any pipeline at any width under it keeps them: `open_section`); a
        // failed section and every other one unbind them, and the statement's end unbinds the warm ones (`Node::cool_workers`)
        let warm = (ok && r.is_ok() && node.is_nested() && crate::slots::warm_on()).then(|| Warm::of(&binding.parts));
        // bounded("small-fixed: one per worker slot of the section's width, at most SLOTS (256)")
        for slot in node.workers.iter_mut().take(width as usize) {
            match warm {
                Some(w) => raw::detach_warm(slot, w),
                None => detach(slot),
            }
        }
        // D4043: what the workers' draws gave back to the purse during the close (retired buffers) back in G's room too
        if let Some(s) = stmt {
            s.drain_purse(false);
        }
        let last = pipeline + 1 == node.program().pipelines.len();
        match (&r, ok) {
            (Ok(()), true) => {
                node.free_after(pipeline);
                exec.mark_opened(pipeline);
                exec.advance_pipeline();
                exec.set_state(if last { ExecState::Done } else { ExecState::Running });
            }
            _ => exec.set_state(ExecState::Failed),
        }
        r
    }

    /// D3140: the pool's owners for `pipeline`, an arm run's last arm that ran on the leader (a join's drain after the run's
    /// sections scattered their partials to the slot's files): `width` worker slots bound as a section binds them, with no
    /// claim and no partial region. The leader region's table is the one partial (`Open::partials`); the owners' regions are
    /// pinned past it (`own_past_grant`). `close_owners` ends it.
    pub fn open_owners<'e>(&mut self, exec: &'e mut Exec, binding: &Binding<'_>, derive: &mut dyn Derive, pipeline: usize, width: u16) -> Result<Open<'e>> {
        assert!(width >= 2 && exec.next_pipeline() > pipeline, "owners of a pipeline that has not run");
        let t0 = pg_clock::MonoStamp::now();
        let salt = exec.slot_salt();
        let node = exec.node_mut();
        assert_eq!(node.state(), NodeState::Bound);
        let program = node.program().clone();
        let Some(slot_of) = program.pipelines[pipeline].produces() else { return Err(Error::Unsupported(UNSUPPORTED_SCRATCH)) };
        if let Err(e) = node.provision(width, derive, salt) {
            exec.set_state(ExecState::Failed);
            return Err(e);
        }
        let consumed = node.consumed_ptrs_after(pipeline);
        let own_slot = node.own_slot_of(pipeline);
        let op_owner = node.slot_owner(own_slot);
        // CONVERT wave 2 (R2, R3): as the section bind above
        let (feed_owner, stmt_owner) = (node.feed_owner_of(pipeline), node.stmt_owner());
        // CONVERT wave 2 (commit 7): the own slot's breaker kind, beside `op_owner` (the spill host's own-slot answer)
        let own_kind = node.own_kind_of(pipeline);
        let reads = node.reads_of(pipeline);
        let arm = node.arm_flag(pipeline);
        let (nested, leader_ctx) = (node.is_nested(), node.ctx());
        let (mut bound, mut failed) = (0, None);
        let key = Warm::of(&binding.parts);
        let skip = if crate::slots::serial_skip() { crate::slots::serial_sites(&program.pipelines) } else { 0 }; // D4725
        // bounded("small-fixed: one per worker slot of the section's width, at most SLOTS (256)")
        for (i, slot) in node.workers.iter_mut().take(width as usize).enumerate() {
            slot.binding = Some(raw::erase(binding.fork(i as u16, MorselRange::ALL)));
            let b = slot.binding.as_mut().unwrap();
            let bref = b.as_ref();
            slot.skip = skip;
            if let Err(e) = bind_hosts(&mut slot.warm, &mut slot.inner, &bref, key, skip) {
                failed = Some(e);
                break;
            }
            raw::bind_ctx(&slot.inner, b, i as u16, width, arm, 0, leader_ctx);
            if nested {
                raw::copy_frame_words(&program, leader_ctx, slot.inner.ctx);
            }
            slot.state_stale = true;
            slot.inner.own_slot = own_slot;
            (slot.inner.op_owner, slot.inner.own_kind) = (op_owner, own_kind);
            (slot.inner.feed_owner, slot.inner.stmt_owner) = (feed_owner, stmt_owner);
            (slot.inner.consumes, slot.inner.spill_parts) = reads;
            slot.inner.home = crate::mem::grow::Home::None;
            slot.inner.clear_mapped();
            slot.inner.set_partials(Partials { own: gravity_abi::word(core::ptr::null_mut()), ..consumed });
            let attached: &mut WorkerBinding<'static> = slot.binding.as_mut().unwrap();
            slot.inner.attach(attached);
            bound += 1;
        }
        self.stats.bind_us += t0.elapsed_ns() / 1000;
        if let Some(e) = failed {
            // bounded("small-fixed: one per worker slot of the section's width, at most SLOTS (256)")
            for slot in node.workers.iter_mut().take(bound + 1) {
                detach(slot);
            }
            exec.set_state(ExecState::Failed);
            return Err(e);
        }
        fires::fire("owner.run_end");
        let slots = node.workers.as_mut_ptr();
        let program_ptr: *const Program = &**node.program();
        let r = node.slot_region(slot_of);
        let leader = (r.as_mut_ptr(), r.len());
        // D4043: as `open_section`'s
        let purse = raw::stmt_of(node).and_then(crate::mem::grant::Reservation::fill_purse);
        Ok(Open { exec, cells0: ChargedVec::none(), slots, program: program_ptr, width, pipeline, leader: core::cell::Cell::new(leader), owner: core::cell::Cell::new(0), owners_base: core::cell::Cell::new(core::ptr::null_mut()), merged: AtomicBool::new(false), lead: true, purse })
    }

    /// D3140: `open_owners`' slots folded and detached; the pipeline stays where its arm left it.
    pub fn close_owners(&mut self, open: Open<'_>) {
        let Open { exec, width, pipeline, purse, .. } = open;
        let node = exec.node_mut();
        // D4043: as `close_section`'s
        let stmt = purse.as_ref().and_then(|_| raw::stmt_of(node));
        if let Some(s) = stmt {
            s.drain_purse(true);
        }
        let kind = node.program().pipelines[pipeline].produces().and_then(|s| crate::mem::grow::kind_of(node.program(), s));
        // bounded("small-fixed: one per worker slot of the section's width, at most SLOTS (256)")
        for slot in node.workers.iter_mut().take(width as usize) {
            slot.settle_retired();
            crate::mem::grow::fold(&mut slot.inner.grew, &node.counters, kind);
        }
        node.fold_worker_arenas(width);
        // bounded("small-fixed: one per worker slot of the section's width, at most SLOTS (256)")
        for slot in node.workers.iter_mut().take(width as usize) {
            detach(slot);
        }
        if let Some(s) = stmt {
            s.drain_purse(false);
        }
    }
}

/// D3196 (rearm E1): a slot's hosts ready for an open of `key` from `b`: kept bound when the slot is warm for exactly `key`
/// and each re-armed (`Source::rearm`; `Ok(true)`, no bind), else unbound first when warm for another open, then each bound
/// (`Ok(false)`). The first error is the open's; its failure path unbinds them all. A worker slot holds sources alone, and
/// they bind from `b` without its parameter image (a rescan's own words): what a warm slot keeps across rescans is what a
/// fresh bind would make. `BindingRef`'s fields are named one by one: a new one is a build error here until the warm key
/// (`Warm::of`) weighs it.
fn bind_hosts(warm: &mut Option<Warm>, inner: &mut crate::node::Inner, b: &crate::binding::BindingRef<'_>, key: Warm, skip: u64) -> Result<bool> {
    let crate::binding::BindingRef { snapshot, command_id, abort_flag, pins, per_tuple, identity, resolved, params: _, ordinal } = *b;
    let b = crate::binding::BindingRef { snapshot, command_id, abort_flag, pins, per_tuple, identity, resolved, params: &[], ordinal };
    let mut err = None;
    match warm.take() {
        Some(w) if w == key => {
            inner.for_each_host_but(skip, &mut |h| {
                if err.is_none() {
                    err = h.rearm().err();
                }
            });
            return err.map_or(Ok(true), Err);
        }
        Some(_) => inner.for_each_host(&mut |h| h.unbind()),
        None => {}
    }
    inner.for_each_host_but(skip, &mut |h| {
        debug_assert!(matches!(h, crate::node::HostRef::Source(_)), "a worker slot's host that is no source (D3196: its bind may read the image)");
        if err.is_none() {
            err = h.bind(&b).err();
        }
    });
    err.map_or(Ok(false), Err)
}

/// D4630 (b): whether every host of a worker slot binds at its claim (`Source::binds_at_claim`); a slot with none binds at
/// the open. D4725: the hosts of the sites in `skip` are never bound, so they do not count.
fn binds_at_claim(inner: &mut crate::node::Inner, skip: u64) -> bool {
    let (mut all, mut any) = (true, false);
    inner.for_each_host_but(skip, &mut |h| {
        any = true;
        all &= matches!(h, crate::node::HostRef::Source(s) if s.binds_at_claim());
    });
    all && any
}

/// Fold partial `i` after the leader was scattered for it (the table is empty: it fits).
#[allow(clippy::too_many_arguments)]
fn continue_fold(node: &mut crate::node::Node, produced: Option<u8>, ordinal: u16, i: usize, width: u16, binding: &mut Binding<'_>, merge: &dyn Merge, r: &mut Result<()>) {
    let lead = node.ctx();
    let home = produced.map(|s| node.leader_home(s));
    let (state, slots) = node.slot_and_workers(produced);
    let slot = slots.iter().take(width as usize).filter(|s| !s.state_stale).nth(i).expect("a partial");
    let partial = Partial { ordinal, bytes: slot.state(), rows_processed: slot.rows_processed() };
    let mut leader = State { bytes: state, sink: binding.sink, home };
    // C9: as `close_section`'s folds: the statement's byte lent to the fold's steps
    if let Err(e) = raw::lend_cancel(lead, || merge.merge_into(&mut leader, partial)) {
        *r = Err(e);
    }
}

#[cfg(test)]
mod owner_region_tests {
    use super::*;

    #[test]
    fn a_serial_owner_over_held_records_past_the_wall_takes_the_wall_not_a_refusal() {
        let held = 2_399_440_640usize;
        assert_eq!(owner_region_bytes(one_owner_bytes(held, 32), held, |_| true).unwrap(), OWNER_WALL, "D159: a W0 owner holding 2.4 GB asked 4.8 GB");
        let mib = one_owner_bytes(1 << 20, 32);
        assert_eq!(owner_region_bytes(mib, 1 << 20, |_| true).unwrap(), mib);
        assert_eq!(owner_region_bytes(mib, 1 << 20, |b| b as usize >= 4 * mib).unwrap(), 4 * mib);
        assert!(matches!(owner_region_bytes(one_owner_bytes(held, 32), held, |b| b as usize > OWNER_WALL), Err(Error::Refused(_))), "a floor past the wall refuses by name");
    }

    /// D3322: the owners' cuts lie inside the leader region the slot holds. The crash's numbers: a 15780 MiB grant at W8 puts
    /// the leader's share at the u32 cap (`Grant::split`: 0xFFFFFFF8, its region 2^32 B), the grant's owner cut 0x1FFFFFF0
    /// (two of the three faults sat exactly that far apart); the full pool refused the 2^32 B draw and the region opened at
    /// the pool's room (here 64 MiB).
    #[test]
    fn the_owner_cut_stays_inside_a_leader_region_the_pool_opened_smaller() {
        use crate::mem::grant::Grant;
        let split = Grant::of(15780 << 20).split(1, 8);
        assert_eq!(split.breaker.bytes, u32::MAX & !7, "the leader's share at the u32 cap");
        assert_eq!(crate::park::region_bytes(split.breaker.bytes as usize), 1 << 32, "its region: the 4294967296 B draw");
        assert_eq!(split.owner.bytes, 0x1FFF_FFF0, "the grant's owner cut at W8");
        let held = OWNER_CUT_HELD.load(Ordering::Relaxed);
        // the region holds the grant's share: the grant's cut, nothing counted
        assert_eq!(owner_within(split.owner.bytes, split.breaker.bytes as usize, 8), split.owner.bytes);
        assert_eq!(OWNER_CUT_HELD.load(Ordering::Relaxed), held);
        for (leader, width) in [(64usize << 20, 8u16), (1 << 30, 8), (64 << 20, 16), (4 << 20, 2)] {
            let split = Grant::of(15780 << 20).split(1, width);
            let cut = owner_within(split.owner.bytes, leader, width) as usize;
            assert!(cut < split.owner.bytes as usize && cut > 0, "{leader} B leader at W{width}: cut {cut}");
            assert!(gravity_abi::breaker::OWNED_HDR + width as usize * cut <= leader, "{leader} B leader at W{width}: the last owner ends inside it");
            assert_eq!(cut, owner_cut_of(leader, width), "the cut of the region held");
        }
        assert!(OWNER_CUT_HELD.load(Ordering::Relaxed) >= held + 4, "each held cut counted");
        // a leader region larger than the grant's share (a kept arm region) never widens the grant's cut
        assert_eq!(owner_within(1 << 20, 1 << 30, 4), 1 << 20);
    }

    fn owner_cut_of(leader: usize, width: u16) -> usize {
        crate::mem::grant::owner_cut(crate::mem::grant::Share { bytes: leader as u32 }, width).bytes as usize
    }
}

#[cfg(test)]
mod cut_tests {
    /// Lever (4) of the distinct-count owners' closing lever: a one-round owner's cut is zeroed at its header prefix alone, a two-round one's whole.
    #[test]
    fn a_one_round_owner_cut_zeroes_its_header_prefix_alone() {
        assert_eq!(super::cut_zeroed(64 << 20, false), crate::node::ZERO_PREFIX);
        assert_eq!(super::cut_zeroed(16, false), 16);
        assert_eq!(super::cut_zeroed(64 << 20, true), 64 << 20);
    }
}

#[cfg(test)]
mod kept_tests {
    use super::{Kept, KEPT_PROBE};
    use gravity_abi::breaker::HASHAGG_HDR;

    /// D3111: the kept probe's word round-trips its table offset and its kept state's end (none too).
    #[test]
    fn a_kept_word_keeps_its_offset_and_end() {
        for k in [Kept { off: 4096, end: Some(4096) }, Kept { off: 600_000, end: Some(800) }, Kept { off: 72, end: None }] {
            assert_eq!(Kept::of(k.word()), k);
        }
    }

    /// D3111: a fixed-offset form's region is its offset and the table; one whose offset moves with the region (a distinct
    /// fold's accumulators past its results) takes the table at the probe's share of table space past its kept state.
    #[test]
    fn a_kept_region_holds_the_table_behind_the_form_own_share() {
        assert_eq!(Kept { off: 1000, end: Some(1000) }.region(1 << 20), 1000 + HASHAGG_HDR + (1 << 20));
        assert_eq!(Kept { off: 1000, end: None }.region(1 << 20), 1000 + HASHAGG_HDR + (1 << 20));
        // 60 % of the probe past the kept state is the form's own: the table space is 40 % of the region past it
        let (e, off) = (800, 800 + (KEPT_PROBE - 800) * 3 / 5);
        let r = Kept { off, end: Some(e) }.region(4 << 20);
        let share = (r - e) as f64 * (KEPT_PROBE - off) as f64 / (KEPT_PROBE - e) as f64;
        assert!(share >= (HASHAGG_HDR + (4 << 20)) as f64 && share < (HASHAGG_HDR + (4 << 20)) as f64 + 8.0, "{r} {share}");
    }

    /// D4531: a thread's kept-run probe is charged whole to the per-backend holder, not to the backend bound on the thread
    /// (a pooled thread outlives its sessions), and a second owner phase reuses it.
    #[test]
    fn a_threads_kept_probe_is_the_per_backend_holders_not_its_backends() {
        std::thread::spawn(|| {
            struct Plain;
            impl crate::Merge for Plain {}
            let a = memledger::Acct::backend(4530);
            memledger::bind_backend(4530, 9530);
            let row = |x| {
                memledger::fold();
                memledger::account_row(x).account_bytes
            };
            let probe = || super::KEPT_PROBE_BUF.with(|b| (b.borrow().owner(), b.borrow().charged_bytes()));
            let a0 = row(a);
            assert_eq!(super::kept_run(&Plain), None, "no owner form: the runs form");
            assert_eq!(probe(), (Some(super::KEPT_PROBE_OWNER), KEPT_PROBE as u64), "the probe is the per-backend holder's, whole");
            assert!(row(memledger::Acct::of(memledger::Consumer::PerBackend)) >= KEPT_PROBE as i64);
            // the commit moved the block off the bound backend (door 1 counts it there where the allocator is installed)
            assert!(row(a) - a0 <= 0, "nothing of it on the backend");
            assert_eq!(super::kept_run(&Plain), None);
            assert_eq!(probe(), (Some(super::KEPT_PROBE_OWNER), KEPT_PROBE as u64), "reused");
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }

    /// D3111: past an overflow a kept region doubles, or grows at once to hold four times the overflowed partition's records.
    #[test]
    fn a_kept_region_past_an_overflow_grows_to_its_partition_or_twice() {
        let k = Kept { off: 1000, end: Some(1000) };
        assert_eq!(k.grown(64 << 10, None), 128 << 10);
        assert_eq!(k.grown(64 << 10, Some(4 << 20)), 1000 + HASHAGG_HDR + (16 << 20));
        assert_eq!(k.grown(256 << 20, Some(4 << 10)), 512 << 20);
    }
}
