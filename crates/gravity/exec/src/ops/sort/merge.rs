//! M3's sort protocol `runs.sort` (ProtoKind::SortMerge; cards/M3.md L3 `exec/src/v4/m3.rs`, Decision 7, I31): the
//! leader merges slot `slot`'s sealed worker runs where they lie into the slot's one sealed run, which the emit's
//! `sort_begin` reads as `SORT_MODE_MEM` (v2 `source/emit_sort.rs`'s sealed mode, past width 1). The producer's section-end
//! open is kernels v4 m3 `sort_open_res` / `_var`: each worker's last run is sealed in its region unless the slot's stream
//! holds a run; a worker past its share of the grant puts runs to its BufFile during the section, and then every worker
//! puts and the stream's k-way merge stays the emit's (the explicit grant ruling: a sort's grant is its need, the BufFile
//! only past it). The merge is the section's phase 2 (`Merge::partition_plan`): splitter keys sampled from the runs cut
//! every run into key ranges, and the pool's threads merge disjoint ranges into disjoint slices of the leader's region,
//! grown to hold them all (a var row's copies packed behind the rows). Under one L2 slice, or with no phase 2, `finish`
//! merges on the leader. `Proto::new(SortMerge, producer).slot(slot)`, `.arg(spec).peer(nkey)` for a fixed row. M3-HOSTS.
use core::sync::atomic::{AtomicUsize, Ordering::AcqRel};

use gravity_abi::breaker::{SORTRUN_HDR, SORTRUN_VAR_WORDS, TABLE_SORT};
use gravity_driver::{Error, Exec, Merge, Partial, PartitionPlan, Region, Result, State};
use gravity_fn::arena::{AVec, Kind, OpArena, Refused};
use gravity_kernels::ops::sort::entries::{RunShape, RESIDENT_MAX, RUN_CUT, RUN_SEALED};
use gravity_program::protos::{Proto, ProtoKind, NONE};
use memledger::ChargedVec;
use types_error::PgResult;

use super::super::{Live, Prepare, Protocol};
use super::{cut, raw};

pub(crate) struct SortMerge;

impl Protocol for SortMerge {
    fn kind(&self) -> ProtoKind {
        ProtoKind::SortMerge
    }
    /// Nothing at width 1 (`sort_last` seals the run in the slot) unless an append.arms run is MergeAppend's (`ArmSort`).
    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        let fixed = (p.peer != NONE).then_some((p.peer as usize, p.arg));
        // D508: a walked top's `n` (lowerv4 runs/walk.rs `TopKDesc.walking`), its bound read from the workers' cut runs
        let walk = cx.program.pipelines.get(p.pipeline as usize).and_then(|x| x.topk).filter(|t| t.walk_attno > 0).map_or(0, |t| t.n as usize);
        if let Some(a) = cx.program.v4.as_deref().and_then(|d| d.protos.iter().find(|a| a.kind == ProtoKind::Arms && a.slot == p.slot && arms_hold(a, p.pipeline))) {
            return Ok(Some(crate::ops::sort::proto::ArmSort::new(Resident::new(a.pipeline as usize..(a.pipeline as u64 + a.arg) as usize, fixed, walk, None), p.slot, cx.program, cx.charged_vec())));
        }
        Ok(match (cx.width > 1, walk) {
            (true, _) => Some(Box::new(Resident { hold_err: cx.program.pipelines.get(p.pipeline as usize).is_some_and(|x| x.held_sort), hold_at: cx.program.pipelines.get(p.pipeline as usize).is_some_and(|x| x.held_at), ..Resident::new(p.pipeline as usize..p.pipeline as usize + 1, fixed, walk, Some(p.slot)) }) as Box<dyn Live>), // alloc: Exec
            (false, 0) => None,
            (false, n) => Some(Box::new(WalkBound { pipeline: p.pipeline as usize, n })), // alloc: Exec
        })
    }
}

/// D555: whether append.arms run `a` holds producer `pipeline`. A slot number is reused once its life ends, so a full sort
/// consumed before a MergeAppend's (or a union's) arms run can hold the arms' slot number: its own SortMerge must not take
/// the arms' run as its merge (the arms' var rows under the sort's fixed spec, "a fixed run holding var copies").
fn arms_hold(a: &Proto, pipeline: u16) -> bool {
    (a.pipeline as u64..a.pipeline as u64 + a.arg).contains(&(pipeline as u64))
}

/// One execution's merge: the phase-2 cut, or the runs `merge_into` handed the leader's own fold.
pub(crate) struct Resident {
    /// The producing pipelines (one; MergeAppend's arms).
    pub(crate) arms: core::ops::Range<usize>,
    /// A fixed row's `(nkey, spec)`; `None`: a var row, its shape read from its runs.
    pub(crate) fixed: Option<(usize, u64)>,
    /// A walked top's `n` (D508), 0 without a walk: `bound` reads the workers' cut runs.
    pub(crate) walk: usize,
    pub(crate) cut: pgsync::Mutex<Cut>,
    /// Phase 2's parts merged: `finish` seals a cut only when every part ran (a refused phase 2 folds on the leader).
    pub(crate) done: AtomicUsize,
    /// The sealed partials `merge_into` noted, `(region, bytes)`.
    pub(crate) held: pgsync::Mutex<Vec<(usize, usize)>>,
    /// D3027 (b): the slot whose stream takes the sealed runs when their merged run would pass `cap` (a plain sort's merge;
    /// `None` under `ArmSort`, which puts on its own: `finish` leaves it the runs). D3259: any sort's sealed runs can pass
    /// it, kernels `resident` seals whenever no worker put (it no longer bounds `width` shares by a region).
    pub(crate) over: Option<u8>,
    /// The merged run's byte cap: `resident_cap`, a test's smaller.
    pub(crate) cap: u64,
    /// The sealed runs `finish` left past `cap`, `(region, bytes)`: put to `over`'s stream after the pipeline.
    pub(crate) spilled: pgsync::Mutex<Vec<(usize, usize)>>,
    /// D1236's held inner: the pipeline is `held_sort` (its error deferred to the merge join's first inner call).
    pub(crate) hold_err: bool,
    /// D2316: the pipeline is `held_at` too (its run stamped with its rows kept, ops/sort/held.rs `stamp_at`).
    pub(crate) hold_at: bool,
}

// the parts `prepare_partial` reads and sums per lock
use gravity_program::cost::SORT_PREP_BATCH as PREP_BATCH;

/// A carve the allocator refused: the statement's out of memory (53200), not the process's abort.
fn refused(r: Refused) -> Error {
    Error::out_of_memory(r.bytes)
}

/// D3259: `seal_over`'s room for a var run's table copy (a few words a column; the live run counts it with its rows).
const SEAL_SLACK: u64 = 64 << 10;

/// The bytes `Resident::lay` lays for `rows` rows of `rb` bytes and `var` bytes of copies (a var row's table copy in it).
pub(crate) fn laid_bytes(rows: u64, rb: u32, var: u64) -> u64 {
    let rb = (rb as u64).max(1);
    SORTRUN_HDR as u64 + ((rows * rb + var).div_ceil(rb) + 1) * rb + SORTRUN_VAR_WORDS as u64
}

/// D3027 (b): the merged run's cap: `RESIDENT_MAX` (a slot region's bytes are a u32), lowered by the witness lever
/// (knobs diag.rs `resident_cap`) so an e2e reaches the put past it without 4 GiB of copies.
fn resident_cap() -> u64 {
    static CAP: pgsync::OnceLock<u64> = pgsync::OnceLock::new();
    *CAP.get_or_init(|| crate::inputs::knobs::diag::resident_cap().map_or(RESIDENT_MAX, |c| c.min(RESIDENT_MAX)))
}

/// Phase 2's cut: the runs `(base, rows)`, row `r * (parts + 1) + p` of `at` run r's first row of part p, each part's
/// first output row, its var bytes per run (`vsum[r * parts + p]`, phase 2a's) and their offsets past `var0`.
#[derive(Default)]
pub(crate) struct Cut {
    at: ChargedVec<u32>,
    vsum: ChargedVec<u64>,
    vbase: ChargedVec<u64>,
    shape: Option<RunShape>,
    pub(crate) runs: Vec<(usize, u32)>,
    pub(crate) out: Vec<u64>,
    var0: usize,
}

/// D508, the row walk's bound: a live run a cut left ordered (`RUN_CUT`) with `n` rows or more holds row `n - 1`'s key-0
/// pair at its row's start, `[flags, word]` (keylane.rs: flags 1 a value; an ASC integer image `word ^ 1 << 63`, the
/// value sign-extended; a DESC one reads back as the NOT of the value, which the descending walk's keys are too, PM13); a
/// NULLS LAST NULL there (flags 2), no cut or fewer rows arm nothing. PL-06: a NULLS FIRST NULL (flags 0) leads the run's
/// order, so it bounds at `i64::MIN`, the walk key of every granule holding a NULL under a NULLS FIRST walk
/// (`TopKDesc::nulls_first`; only such a run holds flags 0): the walk reads those granules alone. D538: a run not
/// cut yet (the cut waits for its soft cap) with `n` rows or more bounds at its `n`-th smallest image, read in place:
/// this worker alone holds `n` rows at or under it. WC-01: rows stored after the cut (lowerv4 bound_first.rs `rearm`: a
/// walk's open arms the cut's bound and cuts no more per claim) bound with the cut's first `n`, at the `n`-th smallest
/// pair of both: row `n - 1` alone went stale. Read by the run's own thread between its claims.
pub(crate) fn run_bound(p: &[u8], n: usize) -> Option<i64> {
    if n == 0 || p.len() < SORTRUN_HDR {
        return None;
    }
    let r = raw::run_at(p);
    let len = r.len as usize;
    if r.kind != TABLE_SORT || len < n || SORTRUN_HDR + len * r.row_bytes as usize > p.len() || r.row_bytes < 16 {
        return None;
    }
    let pair = |i: usize| raw::row_pair(r, i);
    // a value's image, a NULLS FIRST NULL's `i64::MIN` (it orders first); a NULLS LAST NULL none
    let image = |[flags, word]: [u64; 2]| match flags {
        0 => Some(i64::MIN),
        1 => Some((word ^ 1 << 63) as i64),
        _ => None,
    };
    if r.pad[2] & RUN_CUT != 0 {
        let cut = pair(n - 1);
        // only rows past the cut's ordered first n that lie under its row n - 1 can lower the bound (ties on key 0, as a
        // NULLS FIRST top's NULLs, cannot): none, and row n - 1 is the bound, with no copy
        let m = (n..len).filter(|&i| pair(i) < cut).count();
        if m == 0 {
            return image(cut);
        }
        // the n-th smallest of both is the (m + 1)-th largest of those m and the cut's last m + 1 (a cut row under them
        // has m + 1 at or over it); O(len - n), under the soft cap
        let mut c: Vec<[u64; 2]> = (n.saturating_sub(m + 1)..n).map(pair).chain((n..len).map(pair).filter(|p| *p < cut)).collect(); // alloc: Exec
        let k = c.len() - (m + 1);
        return image(*c.select_nth_unstable(k).1);
    }
    // the n smallest images in a max-heap of n (a DESC image is the NOT of the value: its n largest; NULLS LAST NULLs never bound)
    let mut top = std::collections::BinaryHeap::with_capacity(n); // alloc: Exec
    for v in (0..len).filter_map(|i| image(pair(i))) {
        if top.len() < n {
            top.push(v);
        } else if top.peek().is_some_and(|&m| v < m) {
            top.pop();
            top.push(v);
        }
    }
    (top.len() == n).then(|| top.peek().copied()).flatten()
}

/// A walked top at width 1 (D508): the bound alone; the run stays the leader's, as the runner's `ROWS` leaves it.
struct WalkBound {
    pipeline: usize,
    n: usize,
}

impl Live for WalkBound {
    fn answers(&self) -> u16 {
        crate::ops::ask::MERGE
    }

    fn merge_for(&self, pipeline: usize) -> Option<&dyn Merge> {
        (pipeline == self.pipeline).then_some(self as &dyn Merge)
    }
}

impl Merge for WalkBound {
    fn merge_into(&self, _leader: &mut State<'_>, _partial: Partial<'_>) -> Result<()> {
        Ok(())
    }
    fn bound(&self, partial: Partial<'_>) -> Option<i64> {
        run_bound(partial.bytes, self.n)
    }
}

/// A worker's run when it holds sealed rows: `(rows base, rows)`; `Err` for rows left unsealed (a bug's wall).
fn sealed(p: &[u8], var: bool, rb: &mut u32) -> Result<Option<(usize, u32)>> {
    if p.len() < SORTRUN_HDR {
        return Ok(None);
    }
    let r = raw::run_at(p);
    if r.kind != TABLE_SORT || r.len == 0 || r.cap == 0 {
        return Ok(None);
    }
    if r.pad[0] != RUN_SEALED || r.len > r.cap || (*rb != 0 && r.row_bytes != *rb) || (!var && r.var_top() != SORTRUN_HDR as u32 + r.cap * r.row_bytes) {
        return Err(Error::host("gravity: runs.sort: a worker run left unsealed (or a fixed run holding var copies) at its section end"));
    }
    *rb = r.row_bytes;
    Ok(Some((r.rows_ptr() as usize, r.len)))
}

impl Resident {
    pub(crate) fn new(arms: core::ops::Range<usize>, fixed: Option<(usize, u64)>, walk: usize, over: Option<u8>) -> Resident {
        let m = || pgsync::Mutex::new(Vec::new()); // alloc: Exec
        Resident { arms, fixed, walk, cut: pgsync::Mutex::new(Cut::default()), done: AtomicUsize::new(0), held: m(), over, cap: resident_cap(), spilled: m(), hold_err: false, hold_at: false }
    }

    /// D3027 (b): whether `runs` (of shape `s`, `rb` bytes a row) merged would pass `cap` (the run `lay` lays). D3259: for
    /// every merge, `over` or not (an `ArmSort`'s too: `finish` then leaves it the runs).
    fn past_cap(&self, runs: &[(usize, u32)], rb: u32, s: &RunShape) -> bool {
        let (Some(&(first, _)), true) = (runs.first(), rb > 0) else { return false };
        let rows: u64 = runs.iter().map(|r| r.1 as u64).sum();
        let var: u64 = runs.iter().map(|r| raw::var_bytes(s, r.0)).sum();
        let copy = if self.fixed.is_none() { raw::copy_bytes(first) as u64 } else { 0 };
        laid_bytes(rows, rb, var + copy) > self.cap
    }

    /// D3027 (b): the sealed runs `finish` left past `cap` put to `slot`'s stream in their put form, the leader's region
    /// armed afresh first (it holds no laid run), so the emit's open finds no sealed run and merges the stream, as a sort
    /// whose workers put (ArmSort's put past the grant).
    pub(crate) fn put_over(&self, node: &mut dyn crate::ops::sort::proto::ArmNode, slot: u8, spilled: Vec<(usize, usize)>) -> Result<()> {
        let (runs, _, shape) = self.runs(spilled.into_iter())?;
        let Some(s) = shape else { return Ok(()) };
        crate::ops::sort::proto::rearm(node.leader(slot));
        let owner = node.slot_owner(slot);
        for &(base, _) in runs.iter() {
            if self.fixed.is_none() {
                raw::compact(&s, base, owner)?;
            }
            node.put_run(slot, crate::ops::sort::proto::head(base) as *mut u8)?;
        }
        Ok(())
    }

    /// The sealed runs, their row bytes and their shape (`None` without a run).
    pub(crate) fn runs(&self, partials: impl Iterator<Item = (usize, usize)>) -> Result<(Vec<(usize, u32)>, u32, Option<RunShape>)> {
        let (mut rb, mut runs) = (0, Vec::new()); // alloc: Exec
        for (p, n) in partials {
            if let Some(r) = sealed(raw::partial_bytes(p, n), self.fixed.is_none(), &mut rb)? {
                runs.push(r);
            }
        }
        let shape = match (runs.first(), self.fixed) {
            (None, _) => None,
            (Some(_), Some((nkey, spec))) => Some(RunShape::fixed(rb, nkey, spec)),
            (Some(&(b, _)), None) => Some(raw::var_run_shape(b).ok_or_else(|| Error::host("gravity: runs.sort: a var run without its table copy"))?),
        };
        Ok((runs, rb, shape))
    }

    /// The leader laid out as a worker's run of `total` rows of `rb` bytes (grown to hold it): the rows from its base,
    /// `var` bytes of copies and a var row's table copy (`src`'s, `take_table`) below its top; the rows' base and the
    /// copies' offset from the leader's base.
    /// D3259: a run past `max` bytes is laid by none (`OutOfMemory`, which `ArmSort` answers by putting the runs).
    /// C10 (SPILL-DEBT SortResidentMerge paid): nor one past the sort's grant: the leader grows within G only
    /// (`State::grow_in`, nothing drawn when it refuses), and the caller puts the runs to the slot's stream for the emit's
    /// k-way merge (`finish`'s `spilled`, `ArmSort`'s put, a declined phase 2), as PostgreSQL's leader merges its workers'
    /// tapes rather than holding their rows twice.
    pub(crate) fn lay(leader: &mut State<'_>, total: u64, rb: u32, var: u64, src: Option<usize>, max: u64) -> Result<(*mut u8, usize)> {
        let copy = src.map_or(0, raw::copy_bytes) as u64;
        if laid_bytes(total, rb, var + copy) > max {
            return Err(Error::OutOfMemory { bytes: laid_bytes(total, rb, var + copy), held: 0, capacity: max, why: None });
        }
        let cap = u32::try_from((total * rb as u64 + var + copy).div_ceil(rb as u64) + 1).map_err(|_| Error::host("gravity: runs.sort: a merged run past 2^32 rows"))?;
        let need = SORTRUN_HDR + cap as usize * rb as usize + SORTRUN_VAR_WORDS as usize;
        if leader.bytes.len() < need && !leader.grow_in(need)? {
            return Err(Error::OutOfMemory { bytes: need as u64, held: leader.bytes.len() as u64, capacity: max, why: None });
        }
        raw::lay_run(&mut leader.bytes[..], need, cap, rb, total, var, src).ok_or_else(|| Error::host("gravity: runs.sort: a merged run without room for its copies"))
    }
}

impl Live for Resident {
    fn answers(&self) -> u16 {
        crate::ops::ask::MERGE | crate::ops::ask::AFTER
    }

    fn merge_for(&self, pipeline: usize) -> Option<&dyn Merge> {
        self.arms.contains(&pipeline).then_some(self as &dyn Merge)
    }

    /// D3027 (b): the sealed runs `finish` left past the cap put to the slot's stream (`put_over`); nothing otherwise.
    fn after_pipeline(&self, pipeline: usize, exec: &mut Exec) -> Option<Result<()>> {
        let slot = self.over.filter(|_| self.arms.contains(&pipeline))?;
        let spilled = core::mem::take(&mut *self.spilled.lock().unwrap_or_else(|e| e.into_inner()));
        (!spilled.is_empty()).then(|| self.put_over(exec, slot, spilled))
    }
}

impl Merge for Resident {
    fn bound(&self, partial: Partial<'_>) -> Option<i64> {
        run_bound(partial.bytes, self.walk)
    }

    /// D1236's held inner (ops/sort/held.rs): a `held_sort` pipeline's error deferred, the leader's run stamped empty
    /// (D2316: a `held_at` one's with its rows kept).
    fn defer_error(&self, leader: &mut State<'_>) -> Result<bool> {
        Ok(self.hold_err && if self.hold_at { super::held::stamp_at(leader.bytes) } else { super::held::stamp(leader.bytes) })
    }

    /// D3259: whether the section's live runs (each worker's run before its section end: its rows and their copies),
    /// merged, would pass `cap`: then every worker puts its own run at the section end (kernels sort `resident`), at
    /// once, rather than sealing for a leader that finds the merge past the cap and puts every run alone (`put_over`).
    /// `SEAL_SLACK` covers a var run's table copy, which its live run does not count apart.
    fn seal_over(&self, partials: &[Partial<'_>]) -> bool {
        let (mut rows, mut var, mut rb) = (0u64, 0u64, 0u32);
        for p in partials.iter().filter(|p| p.bytes.len() >= SORTRUN_HDR) {
            let r = raw::run_at(p.bytes);
            if r.kind != TABLE_SORT || r.cap == 0 || r.len > r.cap || SORTRUN_HDR + r.cap as usize * r.row_bytes as usize + SORTRUN_VAR_WORDS as usize > p.bytes.len() {
                continue;
            }
            rows += r.len as u64;
            var += r.var_bytes() as u64;
            rb = rb.max(r.row_bytes);
        }
        let over = rb > 0 && laid_bytes(rows, rb, var + SEAL_SLACK) > self.cap;
        if over {
            fires::fire("sort.resident.over"); // shapecheck fire signal (D3259): the section's runs put past the cap, each by its worker
        }
        over
    }

    /// D4373: the rows of the live runs `seal_over` reads (the same runs, by the same test).
    fn seal_rows(&self, partials: &[Partial<'_>]) -> Option<u64> {
        let mut rows = 0u64;
        for p in partials.iter().filter(|p| p.bytes.len() >= SORTRUN_HDR) {
            let r = raw::run_at(p.bytes);
            if r.kind != TABLE_SORT || r.cap == 0 || r.len > r.cap || SORTRUN_HDR + r.cap as usize * r.row_bytes as usize + SORTRUN_VAR_WORDS as usize > p.bytes.len() {
                continue;
            }
            rows += r.len as u64;
        }
        Some(rows)
    }

    /// The leader's own fold (no phase 2): the sealed run noted; `finish` merges them.
    fn merge_into(&self, _leader: &mut State<'_>, partial: Partial<'_>) -> Result<()> {
        if sealed(partial.bytes, self.fixed.is_none(), &mut 0)?.is_some() {
            self.held.lock().unwrap_or_else(|e| e.into_inner()).push((partial.bytes.as_ptr() as usize, partial.bytes.len()));
        }
        Ok(())
    }

    /// Two runs or more past one L2 slice: a part per thread at least, and parts of about one slice each (the machine
    /// profile's destination slice, as the hash merge's), never more parts than rows.
    fn partition_plan(&self, partials: &[Partial<'_>]) -> Option<PartitionPlan> {
        let (runs, rb, shape) = self.runs(partials.iter().map(|p| (p.bytes.as_ptr() as usize, p.bytes.len()))).ok()?;
        // D3027 (b): a merge past the cap is laid by no phase: the leader's fold notes the runs and `finish` leaves them
        if shape.is_some_and(|s| self.past_cap(&runs, rb, &s)) {
            return None;
        }
        let total: u64 = runs.iter().map(|r| r.1 as u64).sum();
        let bytes = total * rb as u64 + runs.iter().map(|r| shape.map_or(0, |s| raw::var_bytes(&s, r.0))).sum::<u64>();
        let slice = gravity_driver::sink::partition_bytes().max(1);
        if runs.len() < 2 || bytes <= slice {
            return None;
        }
        let parts = bytes.div_ceil(slice).max(partials.len() as u64).next_power_of_two().min(total.next_power_of_two() / 2).max(2);
        Some(PartitionPlan { pbits: parts.trailing_zeros() as u8, groups: total, record: rb })
    }

    /// The splitters: `parts` rows of every run at even steps, sorted, every run's `parts`-th of them; each run cut at
    /// each splitter's lower bound, so every row of part p sorts before every row of part p + 1 in every run (D3190:
    /// the cut module's `sample` and `cut`, held monotone under an order that is not total).
    fn prepare_partitions_in(&self, leader: &mut State<'_>, plan: PartitionPlan, partials: &[Partial<'_>]) -> Result<()> {
        let (runs, rb, shape) = self.runs(partials.iter().map(|p| (p.bytes.as_ptr() as usize, p.bytes.len())))?;
        let (Some(s), parts, rbu) = (shape, 1usize << plan.pbits, rb as usize) else { return Ok(()) };
        let total: u64 = runs.iter().map(|r| r.1 as u64).sum();
        let var: u64 = runs.iter().map(|r| raw::var_bytes(&s, r.0)).sum();
        let o = leader.op_owner();
        // C10: a merged run past the sort's grant declines phase 2 (`Unsupported`: the driver rearms the leader, nothing
        // drawn), so the leader's fold notes the runs and `finish` puts them
        let (_, var0) = match Self::lay(leader, total, rb, var, self.fixed.is_none().then_some(runs[0].0), self.cap) {
            Err(Error::OutOfMemory { .. }) => return Err(Error::Unsupported(gravity_abi::status::UNSUPPORTED_SPILL)),
            r => r?,
        };
        let cmp = |a: usize, b: usize| raw::cmp_rows(&s, a, b);
        let mut cut_runs: ChargedVec<cut::Run<'_>> = ChargedVec::new(o, Kind::Sort); // alloc: Exec charged o
        cut_runs.with_vec(|v| v.extend(runs.iter().map(|&(base, rows)| cut::Run::Mem { base, rows: rows as u64, rb: rbu }))); // alloc: Exec charged o
        // CONVERT wave 2: the sample and each run's positions are the sort's working memory, on its slot owner's row
        let a = OpArena::scratch_of(leader.op_owner(), Kind::Sort);
        let splitters = cut::sample_in(&a, &cut_runs, parts, &cmp).map_err(refused)?;
        let splitters = splitters.as_slice(&a);
        let mut pos = AVec::<u64>::default();
        pos.resize(&a, splitters.len() + 2, 0).map_err(refused)?;
        let mut at: ChargedVec<u32> = ChargedVec::new(o, Kind::Sort); // alloc: Exec charged o
        at.with_vec(|v| v.reserve_exact(runs.len() * (parts + 1))); // alloc: Exec charged o
        for r in cut_runs.iter() {
            cut::cut_into(r, splitters, &cmp, pos.as_mut_slice(&a))?;
            // a resident run's positions are under its rows (u32)
            at.with_vec(|v| v.extend(pos.as_slice(&a).iter().map(|&x| x as u32))); // alloc: Exec charged o
        }
        let out = (0..=parts).map(|p| (0..runs.len()).map(|r| at[r * (parts + 1) + p] as u64).sum()).collect();
        let mut vsum: ChargedVec<u64> = ChargedVec::new(o, Kind::Sort); // alloc: Exec charged o
        vsum.with_vec(|v| v.resize(if var > 0 { runs.len() * parts } else { 0 }, 0)); // alloc: Exec charged o
        *self.cut.lock().unwrap_or_else(|e| e.into_inner()) = Cut { shape, runs, at, out, vsum, vbase: ChargedVec::default(), var0 }; // alloc: Exec
        self.done.store(0, core::sync::atomic::Ordering::Release);
        Ok(())
    }

    fn prepares_partials(&self) -> bool {
        self.fixed.is_none()
    }

    /// Phase 2a (var rows): each part's copy bytes in this partial's run.
    /// CONVERT wave 2: the run's positions and sums pass through the stack, `PREP_BATCH` parts at a time (the cut locked only
    /// to copy them), so a pool thread allocates nothing here.
    fn prepare_partial(&self, partial: Region, plan: PartitionPlan) -> Result<()> {
        let (parts, base) = (1usize << plan.pbits, partial.ptr as usize + SORTRUN_HDR);
        let (Some(s), Some(r)) = ({
            let c = self.cut.lock().unwrap_or_else(|e| e.into_inner());
            (c.shape, c.runs.iter().position(|x| x.0 == base).filter(|_| !c.vsum.is_empty()))
        }) else { return Ok(()) };
        let (mut at, mut sums) = ([0u32; PREP_BATCH + 1], [0u64; PREP_BATCH]);
        for p0 in (0..parts).step_by(PREP_BATCH) {
            let n = PREP_BATCH.min(parts - p0);
            at[..=n].copy_from_slice(&self.cut.lock().unwrap_or_else(|e| e.into_inner()).at[r * (parts + 1) + p0..][..=n]);
            for (q, sum) in sums[..n].iter_mut().enumerate() {
                *sum = raw::var_len(&s, base + at[q] as usize * plan.record as usize, at[q + 1] - at[q]);
            }
            self.cut.lock().unwrap_or_else(|e| e.into_inner()).vsum[r * parts + p0..][..n].copy_from_slice(&sums[..n]);
        }
        Ok(())
    }

    fn merge_partition(&self, leader: Region, part: u32, plan: PartitionPlan, _partials: &[Partial<'_>]) -> Result<()> {
        let (p, rb, parts) = (part as usize, plan.record as usize, 1usize << plan.pbits);
        let (s, mut srcs, out, var) = {
            let mut c = self.cut.lock().unwrap_or_else(|e| e.into_inner());
            if !c.vsum.is_empty() && c.vbase.is_empty() {
                let (n, mut acc) = (c.runs.len(), 0);
                let mut vb: ChargedVec<u64> = ChargedVec::new(c.at.owner(), Kind::Sort); // alloc: Exec charged c.at.owner()
                vb.with_vec(|v| v.extend((0..parts).map(|q| { let b = acc; acc += (0..n).map(|r| c.vsum[r * parts + q]).sum::<u64>(); b }))); // alloc: Exec charged c.at.owner()
                c.vbase = vb;
            }
            let mut part: ChargedVec<(usize, u32)> = ChargedVec::new(c.at.owner(), Kind::Sort); // alloc: Exec charged c.at.owner()
            part.with_vec(|v| v.extend(c.runs.iter().enumerate().map(|(r, &(b, _))| (b + c.at[r * (parts + 1) + p] as usize * rb, c.at[r * (parts + 1) + p + 1] - c.at[r * (parts + 1) + p])).filter(|s| s.1 != 0))); // alloc: Exec charged c.at.owner()
            (c.shape, part, c.out[p] as usize, c.var0 + c.vbase.get(p).copied().unwrap_or(0) as usize)
        };
        if let (Some(s), false) = (s, srcs.is_empty()) {
            if !raw::merge_part(&s, &mut srcs, leader.ptr, SORTRUN_HDR + out * rb, var) {
                return Err(Error::host("gravity: runs.sort: a merge compare failed"));
            }
        }
        self.done.fetch_add(1, AcqRel);
        Ok(())
    }

    /// The run sealed: after a whole phase 2 its rows are in place; else the held runs merged here, on the leader.
    fn finish(&self, leader: &mut State<'_>) -> Result<()> {
        let cut = core::mem::take(&mut *self.cut.lock().unwrap_or_else(|e| e.into_inner()));
        let held = core::mem::take(&mut *self.held.lock().unwrap_or_else(|e| e.into_inner()));
        let laid = if held.is_empty() {
            !cut.runs.is_empty() && self.done.swap(0, AcqRel) + 1 == cut.out.len()
        } else {
            // a held run is a sealed worker run, idle until the section's detach, after `finish`
            let (mut runs, rb, shape) = self.runs(held.iter().copied())?;
            let s = shape.expect("a held run");
            // D3027 (b): past the cap the runs stay sealed where they lie for `after_pipeline`'s put; the leader lays none.
            // D3259: under `ArmSort` (no `over`) they go back to `held`, which its `after` merges or puts.
            if self.past_cap(&runs, rb, &s) {
                fires::fire("sort.resident.over");
                *(if self.over.is_some() { &self.spilled } else { &self.held }).lock().unwrap_or_else(|e| e.into_inner()) = held;
                return Ok(());
            }
            let var = runs.iter().map(|r| raw::var_bytes(&s, r.0)).sum();
            // C10: past the sort's grant (`lay`'s grow_in refused, nothing drawn) the runs take the cap's road
            let (rows, var0) = match Self::lay(leader, runs.iter().map(|r| r.1 as u64).sum(), rb, var, self.fixed.is_none().then_some(runs[0].0), self.cap) {
                Err(Error::OutOfMemory { .. }) => {
                    fires::fire("sort.resident.past_grant");
                    *(if self.over.is_some() { &self.spilled } else { &self.held }).lock().unwrap_or_else(|e| e.into_inner()) = held;
                    return Ok(());
                }
                r => r?,
            };
            if !raw::merge_laid(&s, &mut runs, rows, leader.bytes.as_mut_ptr(), var0) {
                return Err(Error::host("gravity: runs.sort: a merge compare failed"));
            }
            true
        };
        if laid {
            fires::fire("sort.resident.laid"); // D3027 (b): the workers' sealed runs merged in place (a witness's signal)
            raw::seal(&mut leader.bytes[..]);
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(unsafe_code)]
pub(crate) mod tests {
    use super::*;
    use gravity_abi::breaker::SortRun;
    use gravity_driver::{BorrowedBatch, Flow, Sink, SinkRef};
    use gravity_kernels::ops::sort::entries::describe;
    use gravity_kernels::host_abi::{sort_image, SortSpec, VAR_TBL, VAR_WIDTH};

    pub(crate) struct Discard;

    impl Sink for Discard {
        fn push(&mut self, _batch: BorrowedBatch<'_>) -> Result<Flow> {
            Ok(Flow::Continue)
        }
    }

    pub(crate) const RB: usize = 24;

    /// A sealed worker run over `keys` (sorted here): rows `[flag 1, key word, tag]`, the var words past them.
    pub(crate) fn run(keys: &[u64], tag: u64) -> Vec<u64> {
        run_flagged(&keys.iter().map(|&k| (1, k)).collect::<Vec<_>>(), tag)
    }

    /// `run` over `(flag, key)` pairs: flag 0 a NULL placed first, 2 a NULL placed last (the producer's null image).
    pub(crate) fn run_flagged(keys: &[(u64, u64)], tag: u64) -> Vec<u64> {
        let mut k = keys.to_vec(); // alloc: Cold
        k.sort_unstable();
        let mut v = vec![0u64; SORTRUN_HDR / 8 + keys.len() * RB / 8 + 1]; // alloc: Cold
        for (i, &(f, x)) in k.iter().enumerate() {
            v[SORTRUN_HDR / 8 + i * 3..][..3].copy_from_slice(&[f, x, tag << 32 | i as u64]);
        }
        let n = keys.len() as u32;
        v[SORTRUN_HDR / 8 + keys.len() * 3] = (SORTRUN_HDR as u32 + n * RB as u32) as u64;
        // SAFETY: the vector holds the header, the rows and the var words.
        unsafe { *(v.as_mut_ptr() as *mut SortRun) = SortRun { cap: n, len: n, row_bytes: RB as u32, pad: [RUN_SEALED, 0, 0], kind: TABLE_SORT } };
        v
    }

    pub(crate) fn keys_of(seed: u64, n: usize, dups: u64) -> Vec<u64> {
        let mut x = seed.wrapping_mul(0x5851_f42d_4c95_7f2d) | 1;
        (0..n).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x % dups }).collect()
    }

    fn resident() -> Resident {
        Resident::new(0..1, Some((2, 0)), 0, None)
    }

    /// The leader's run: sealed, every row once, in key order.
    pub(crate) fn check(leader: &[u64], runs: &[Vec<u64>]) {
        let r = unsafe { &*(leader.as_ptr() as *const SortRun) };
        let row = |v: &[u64], i: usize| (v[SORTRUN_HDR / 8 + i * 3], v[SORTRUN_HDR / 8 + i * 3 + 1], v[SORTRUN_HDR / 8 + i * 3 + 2]);
        let mut want: Vec<(u64, u64, u64)> = runs.iter().flat_map(|v| (0..(v[0] >> 32) as usize).map(move |i| row(v, i))).collect();
        let got: Vec<(u64, u64, u64)> = (0..r.len as usize).map(|i| row(leader, i)).collect();
        assert_eq!((r.pad[0], r.kind, r.len as usize), (RUN_SEALED, TABLE_SORT, want.len()), "one sealed run of every row");
        let fk = |i: usize| (leader[SORTRUN_HDR / 8 + i * 3], leader[SORTRUN_HDR / 8 + i * 3 + 1]);
        assert!((1..r.len as usize).all(|i| fk(i - 1) <= fk(i)), "in (null flag, key) order");
        let mut g = got.clone();
        g.sort_unstable();
        want.sort_unstable();
        assert_eq!(g, want, "the rows' multiset");
    }

    pub(crate) fn partials(runs: &[Vec<u64>]) -> Vec<Partial<'_>> {
        runs.iter().enumerate().map(|(i, v)| Partial { ordinal: i as u16, bytes: unsafe { core::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 8) }, rows_processed: 0 }).collect()
    }

    fn workload(nruns: usize, dups: u64) -> Vec<Vec<u64>> {
        (0..nruns).map(|r| run(&keys_of(r as u64 + 7, (r * 977 + 13) % 3001, dups), r as u64)).collect()
    }

    /// D443's rule is the producer's (a key's null image is its output's NULL); the merge keeps each null image's place:
    /// flag 0 (NULLS FIRST) before every value, flag 2 (NULLS LAST) after, across runs and splitter cuts.
    #[test]
    fn null_images_keep_their_placement_through_both_merges() {
        let runs: Vec<Vec<u64>> = (0..6).map(|r| run_flagged(&keys_of(r + 11, 700, 9).into_iter().map(|k| ([0, 1, 1, 2][k as usize % 4], if k % 4 == 1 { k } else { 0 })).collect::<Vec<_>>(), r)).collect();
        let (m, ps) = (resident(), partials(&runs));
        let mut leader = vec![0u64; 1 << 18]; // alloc: Cold
        let bytes = unsafe { core::slice::from_raw_parts_mut(leader.as_mut_ptr() as *mut u8, leader.len() * 8) };
        let plan = PartitionPlan { pbits: 3, groups: 0, record: RB as u32 };
        m.prepare_partitions_in(&mut State::new(&mut *bytes, SinkRef::of(&mut Discard)), plan, &ps).unwrap();
        let region = Region { ptr: bytes.as_mut_ptr(), len: bytes.len() };
        for part in 0..8 {
            m.merge_partition(region, part, plan, &ps).unwrap();
        }
        m.finish(&mut State::new(bytes, SinkRef::of(&mut Discard))).unwrap();
        check(&leader, &runs);
        let flags: Vec<u64> = (0..runs.iter().map(|v| (v[0] >> 32) as usize).sum()).map(|i| leader[SORTRUN_HDR / 8 + i * 3]).collect();
        assert!(flags.windows(2).all(|w| w[0] <= w[1]) && flags[0] == 0 && flags[flags.len() - 1] == 2, "first NULLs, values, last NULLs");
    }

    #[test]
    fn the_leaders_fold_merges_1_3_17_runs_into_one_sealed_run() {
        for (nruns, dups) in [(1, 1 << 40), (3, 5), (17, 1 << 40), (17, 1)] {
            let runs = workload(nruns, dups);
            let m = resident();
            for p in partials(&runs) {
                m.merge_into(&mut State::new(&mut [], SinkRef::of(&mut Discard)), p).unwrap();
            }
            let mut leader = vec![0u64; 1 << 18]; // alloc: Cold
            let bytes = unsafe { core::slice::from_raw_parts_mut(leader.as_mut_ptr() as *mut u8, leader.len() * 8) };
            m.finish(&mut State::new(bytes, SinkRef::of(&mut Discard))).unwrap();
            check(&leader, &runs);
        }
    }

    #[test]
    fn phase_2_parts_merge_in_any_order_into_one_sealed_run() {
        for (nruns, dups, pbits) in [(3, 1 << 40, 1), (8, 3, 3), (17, 1 << 40, 5), (16, 1, 2)] {
            let runs = workload(nruns, dups);
            let (m, ps) = (resident(), partials(&runs));
            let mut leader = vec![0u64; 1 << 18]; // alloc: Cold
            let bytes = unsafe { core::slice::from_raw_parts_mut(leader.as_mut_ptr() as *mut u8, leader.len() * 8) };
            let plan = PartitionPlan { pbits, groups: 0, record: RB as u32 };
            m.prepare_partitions_in(&mut State::new(&mut *bytes, SinkRef::of(&mut Discard)), plan, &ps).unwrap();
            let region = Region { ptr: bytes.as_mut_ptr(), len: bytes.len() };
            for part in (0..1u32 << pbits).rev() {
                m.merge_partition(region, part, plan, &ps).unwrap();
            }
            m.finish(&mut State::new(bytes, SinkRef::of(&mut Discard))).unwrap();
            check(&leader, &runs);
        }
    }

    #[test]
    fn the_plan_takes_two_runs_past_one_slice_and_leaves_the_rest_to_the_leader() {
        let slice = gravity_driver::sink::partition_bytes() as usize;
        let small = workload(4, 1 << 40);
        assert!(resident().partition_plan(&partials(&small)).is_none(), "under one slice of rows");
        let big: Vec<Vec<u64>> = (0..3).map(|r| run(&keys_of(r, slice / RB, 1 << 40), r)).collect();
        let p = resident().partition_plan(&partials(&big)).expect("three slices of rows");
        assert!(p.pbits >= 2 && p.record == RB as u32, "{} parts", 1 << p.pbits);
        assert!(resident().partition_plan(&partials(&big[..1])).is_none(), "one run needs no merge");
    }

    #[test]
    fn an_unsealed_worker_run_is_a_named_wall_and_an_aborted_phase_2_folds_on_the_leader() {
        let mut runs = workload(3, 9);
        runs[1][1] &= !(0xff << 32);
        let m = resident();
        assert!(m.merge_into(&mut State::new(&mut [], SinkRef::of(&mut Discard)), partials(&runs)[1]).is_err(), "an unsealed run with rows");
        runs[1][1] |= (RUN_SEALED as u64) << 32;
        let (m, ps) = (resident(), partials(&runs));
        let mut leader = vec![0u64; 1 << 18]; // alloc: Cold
        let bytes = unsafe { core::slice::from_raw_parts_mut(leader.as_mut_ptr() as *mut u8, leader.len() * 8) };
        let plan = PartitionPlan { pbits: 2, groups: 0, record: RB as u32 };
        m.prepare_partitions_in(&mut State::new(&mut *bytes, SinkRef::of(&mut Discard)), plan, &ps).unwrap();
        m.merge_partition(Region { ptr: bytes.as_mut_ptr(), len: bytes.len() }, 0, plan, &ps).unwrap();
        // phase 2 refused after one part: the driver re-arms the leader and folds every partial serially
        bytes[..64].fill(0);
        for p in ps.iter() {
            m.merge_into(&mut State::new(&mut [], SinkRef::of(&mut Discard)), *p).unwrap();
        }
        m.finish(&mut State::new(bytes, SinkRef::of(&mut Discard))).unwrap();
        check(&leader, &runs);
    }

    pub(crate) const VRB: usize = 40;

    /// One text key (its image's slot at 16, bytes order), the tag at 24, the record word at 32.
    pub(crate) fn vtbl() -> [u32; 12] {
        let spec = SortSpec::pack(&[(Some((16, VAR_WIDTH)), false)]).unwrap().0;
        [VRB as u32, 2, 3 | VAR_TBL, 0, spec as u32, (spec >> 32) as u32, 1, 32, 2, 16, 0, 0]
    }

    /// A sealed var run of `texts` (sorted here) as `sort_open_res_var` leaves one: the table copy, the rows, their copies.
    pub(crate) fn vrun(texts: &[Vec<u8>], tag: u64) -> Vec<u64> {
        let mut t = texts.to_vec(); // alloc: Cold
        t.sort();
        // a region that holds the rows and their copies (at most 32 bytes a copy of these texts)
        let mut v = vec![0u64; (1 << 13).max(t.len() * (VRB + 32) / 8 + 1024)]; // alloc: Cold
        v[1] = (v.len() * 8) as u64;
        let tbl = vtbl();
        unsafe {
            let run = SortRun::at_share(v.as_mut_ptr() as *mut u8, VRB as u32) as *mut SortRun;
            describe(run, tbl.as_ptr());
            for (i, x) in t.iter().enumerate() {
                let p = (*run).rows_ptr().add((*run).len as usize * VRB) as *mut u64;
                *p.add(4) = 0;
                assert_eq!(sort_image(run as *mut u8, x.as_ptr(), x.len() as u64, 16, 32), 0, "room for the copy");
                let mut w = [0u8; 8];
                w[..x.len().min(8)].copy_from_slice(&x[..x.len().min(8)]);
                assert_eq!((*run).push() as *mut u64, p);
                (*p, *p.add(1), *p.add(3)) = (1, u64::from_be_bytes(w), tag << 32 | i as u64);
            }
            (*run).pad[0] = RUN_SEALED;
        }
        v
    }

    fn texts(seed: u64, n: usize) -> Vec<Vec<u8>> {
        keys_of(seed, n, 1 << 40).into_iter().map(|k| (0..(k % 23) as usize).map(|i| b"abc"[(k >> (i % 13)) as usize % 3]).collect()).collect()
    }

    /// The leader's var run: sealed, every (text, tag) once, in text order, each text read through its row's slot.
    pub(crate) fn vcheck(leader: &[u64], runs: &[Vec<Vec<u8>>]) {
        let r = unsafe { &*(leader.as_ptr() as *const SortRun) };
        let got: Vec<Vec<u8>> = (0..r.len as usize).map(|i| unsafe {
            let slot = (r.rows_ptr() as *const u8).add(i * VRB + 16);
            let (size, off) = ((slot as *const u32).read_unaligned() as usize, (slot.add(4) as *const u32).read_unaligned() as usize);
            core::slice::from_raw_parts(slot.add(off + 4), size - 4).to_vec()
        }).collect();
        assert_eq!((r.pad[0], got.len()), (RUN_SEALED, runs.iter().map(|t| t.len()).sum()));
        assert!(got.windows(2).all(|w| w[0] <= w[1]), "in text order");
        let (mut g, mut want) = (got.clone(), runs.concat());
        g.sort();
        want.sort();
        assert_eq!(g, want, "every copy moved with its row");
    }

    fn var_resident() -> Resident {
        Resident { fixed: None, ..resident() }
    }

    #[test]
    fn var_runs_merge_with_their_copies_on_the_leader_and_in_phase_2() {
        let ts: Vec<Vec<Vec<u8>>> = (0..5).map(|r| texts(r + 3, 40 + 37 * r as usize)).collect();
        let runs: Vec<Vec<u64>> = ts.iter().enumerate().map(|(i, t)| vrun(t, i as u64)).collect();
        let m = var_resident();
        for p in partials(&runs) {
            m.merge_into(&mut State::new(&mut [], SinkRef::of(&mut Discard)), p).unwrap();
        }
        let mut leader = vec![0u64; 1 << 16]; // alloc: Cold
        let bytes = unsafe { core::slice::from_raw_parts_mut(leader.as_mut_ptr() as *mut u8, leader.len() * 8) };
        m.finish(&mut State::new(bytes, SinkRef::of(&mut Discard))).unwrap();
        vcheck(&leader, &ts);
        let (m, ps) = (var_resident(), partials(&runs));
        let mut leader = vec![0u64; 1 << 16]; // alloc: Cold
        let bytes = unsafe { core::slice::from_raw_parts_mut(leader.as_mut_ptr() as *mut u8, leader.len() * 8) };
        let plan = PartitionPlan { pbits: 3, groups: 0, record: VRB as u32 };
        assert!(m.prepares_partials());
        m.prepare_partitions_in(&mut State::new(&mut *bytes, SinkRef::of(&mut Discard)), plan, &ps).unwrap();
        for v in runs.iter() {
            m.prepare_partial(Region { ptr: v.as_ptr() as *mut u8, len: v.len() * 8 }, plan).unwrap();
        }
        let region = Region { ptr: bytes.as_mut_ptr(), len: bytes.len() };
        for part in (0..8).rev() {
            m.merge_partition(region, part, plan, &ps).unwrap();
        }
        m.finish(&mut State::new(bytes, SinkRef::of(&mut Discard))).unwrap();
        vcheck(&leader, &ts);
    }

    /// D3259: the seal answer over the workers' live runs (before their section end: rows, no seal), against the merged
    /// run's bytes: past the cap the workers put (true), under it they seal; an armed share no row reached counts nothing.
    #[test]
    fn the_seal_answer_reads_the_live_runs_against_the_cap() {
        let live = |len: u32| {
            let mut buf = vec![0u64; (SORTRUN_HDR + 8 * 16 + 64) / 8]; // alloc: Cold
            unsafe { *(buf.as_mut_ptr() as *mut SortRun) = SortRun { cap: 8, len, row_bytes: 16, pad: [0; 3], kind: TABLE_SORT } };
            unsafe { core::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 8) }.to_vec() // alloc: Cold
        };
        let mut armed = vec![0u64; (SORTRUN_HDR + 64) / 8]; // alloc: Cold
        unsafe { *(armed.as_mut_ptr() as *mut SortRun) = SortRun { cap: 0, len: 0, row_bytes: 4096, pad: [0; 3], kind: 0 } };
        let armed = unsafe { core::slice::from_raw_parts(armed.as_ptr() as *const u8, armed.len() * 8) }.to_vec(); // alloc: Cold
        let (a, b) = (live(5), live(7));
        let ps = [Partial { ordinal: 0, bytes: &a, rows_processed: 0 }, Partial { ordinal: 1, bytes: &b, rows_processed: 0 }, Partial { ordinal: 2, bytes: &armed, rows_processed: 0 }];
        let at = |cap| Resident { cap, ..Resident::new(0..1, Some((1, 0)), 0, Some(0)) };
        let need = laid_bytes(12, 16, SEAL_SLACK);
        assert!(!at(need).seal_over(&ps), "12 rows merged fit a cap of their bytes: seal");
        assert!(at(need - 1).seal_over(&ps), "a byte under them: every worker puts");
        assert!(!at(RESIDENT_MAX).seal_over(&ps[2..]), "an armed share alone: nothing to merge");
    }

    /// D508: the bound is row n-1's key-0 value of a run a cut marked, only for a value (flags 1) and `n` rows or more.
    #[test]
    fn run_bound_reads_row_n_minus_1_of_a_cut_run() {
        let mut buf = vec![0u64; (SORTRUN_HDR + 8 * 16) / 8]; // alloc: Cold
        let r = buf.as_mut_ptr() as *mut SortRun;
        let rows: [[u64; 2]; 4] = [[1, (-7i64 ^ i64::MIN) as u64], [1, (-3i64 ^ i64::MIN) as u64], [2, 0], [1, (40i64 ^ i64::MIN) as u64]];
        unsafe {
            *r = SortRun { cap: 8, len: 4, row_bytes: 16, pad: [0; 3], kind: TABLE_SORT };
            for (i, w) in rows.iter().enumerate() {
                ((*r).row(i) as *mut [u64; 2]).write_unaligned(*w);
            }
        }
        let bytes = |b: &Vec<u64>| unsafe { core::slice::from_raw_parts(b.as_ptr() as *const u8, b.len() * 8) }.to_vec(); // alloc: Cold
        assert_eq!(run_bound(&bytes(&buf), 2), Some(-3), "no cut (D538): the 2nd smallest value, read in place");
        unsafe { (*r).pad[2] = RUN_CUT };
        let b = bytes(&buf);
        assert_eq!((run_bound(&b, 1), run_bound(&b, 2), run_bound(&b, 4)), (Some(-7), Some(-3), Some(40)));
        assert_eq!((run_bound(&b, 5), run_bound(&b, 0)), (None, None), "fewer rows, no n");
        assert_eq!(run_bound(&b, 3), Some(40), "WC-01: a NULL row n-1, a value stored after the cut: the 3rd smallest of both");
        unsafe { (*r).len = 3 };
        assert_eq!(run_bound(&bytes(&buf), 3), None, "a NULL row n-1, nothing stored after the cut");
    }

    /// WC-01: a cut run holding rows stored after its cut (the open's re-arm: no cut per claim) bounds at the n-th smallest
    /// pair of the cut's ordered first n and those rows, in any order: row n-1 alone is stale.
    #[test]
    fn a_cut_run_with_rows_stored_after_bounds_at_the_nth_of_both() {
        let mut buf = vec![0u64; (SORTRUN_HDR + 16 * 16) / 8]; // alloc: Cold
        let r = buf.as_mut_ptr() as *mut SortRun;
        let img = |v: i64| (v ^ i64::MIN) as u64;
        let vals: [i64; 8] = [1, 5, 9, 12, 3, 7, 20, 2];
        unsafe {
            *r = SortRun { cap: 16, len: 8, row_bytes: 16, pad: [0, 0, RUN_CUT], kind: TABLE_SORT };
            for (i, v) in vals.iter().enumerate() {
                ((*r).row(i) as *mut [u64; 2]).write_unaligned([1, img(*v)]);
            }
        }
        let b = unsafe { core::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 8) }.to_vec(); // alloc: Cold
        // n = 4: the cut kept 1 5 9 12, then 3 7 20 2 (all eight: 1 2 3 5 7 9 12 20); n = 2: kept 1 5, then 9 12 3 7 20 2
        assert_eq!((run_bound(&b, 4), run_bound(&b, 2), run_bound(&b, 1)), (Some(5), Some(2), Some(1)));
        // n = 6, one row after: only the cut's last two can be passed (all seven: 1 4 6 7 8 10 30)
        let vals: [i64; 7] = [1, 4, 6, 8, 10, 30, 7];
        unsafe {
            (*r).len = 7;
            for (i, v) in vals.iter().enumerate() {
                ((*r).row(i) as *mut [u64; 2]).write_unaligned([1, img(*v)]);
            }
        }
        let b = unsafe { core::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 8) }.to_vec(); // alloc: Cold
        assert_eq!(run_bound(&b, 6), Some(10));
    }

    /// D538: an uncut run bounds at its n-th smallest value, NULL rows skipped, in any row order; under n values nothing.
    #[test]
    fn run_bound_selects_the_nth_value_of_an_uncut_run() {
        let mut buf = vec![0u64; (SORTRUN_HDR + 8 * 16) / 8]; // alloc: Cold
        let r = buf.as_mut_ptr() as *mut SortRun;
        let img = |v: i64| (v ^ i64::MIN) as u64;
        let rows: [[u64; 2]; 5] = [[1, img(40)], [2, 0], [1, img(-7)], [1, img(12)], [1, img(-7)]];
        unsafe {
            *r = SortRun { cap: 8, len: 5, row_bytes: 16, pad: [0; 3], kind: TABLE_SORT };
            for (i, w) in rows.iter().enumerate() {
                ((*r).row(i) as *mut [u64; 2]).write_unaligned(*w);
            }
        }
        let b = unsafe { core::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 8) }.to_vec(); // alloc: Cold
        assert_eq!((run_bound(&b, 1), run_bound(&b, 2), run_bound(&b, 3), run_bound(&b, 4)), (Some(-7), Some(-7), Some(12), Some(40)));
        assert_eq!(run_bound(&b, 5), None, "four values, the fifth row a NULL");
    }

    /// PL-06: a NULLS FIRST NULL (flags 0) leads the run, so it bounds at `i64::MIN` (the walk key of a granule holding a
    /// NULL), cut or not; values after the NULLs bound as before.
    #[test]
    fn a_nulls_first_null_bounds_at_the_lowest_key() {
        let mut buf = vec![0u64; (SORTRUN_HDR + 8 * 16) / 8]; // alloc: Cold
        let r = buf.as_mut_ptr() as *mut SortRun;
        let img = |v: i64| (v ^ i64::MIN) as u64;
        let rows: [[u64; 2]; 4] = [[0, 0], [0, 0], [1, img(5)], [1, img(9)]];
        unsafe {
            *r = SortRun { cap: 8, len: 4, row_bytes: 16, pad: [0; 3], kind: TABLE_SORT };
            for (i, w) in rows.iter().enumerate() {
                ((*r).row(i) as *mut [u64; 2]).write_unaligned(*w);
            }
        }
        let bytes = |b: &Vec<u64>| unsafe { core::slice::from_raw_parts(b.as_ptr() as *const u8, b.len() * 8) }.to_vec(); // alloc: Cold
        let b = bytes(&buf);
        assert_eq!((run_bound(&b, 1), run_bound(&b, 2), run_bound(&b, 3), run_bound(&b, 4)), (Some(i64::MIN), Some(i64::MIN), Some(5), Some(9)), "uncut: the NULLs first");
        unsafe { (*r).pad[2] = RUN_CUT };
        let b = bytes(&buf);
        assert_eq!((run_bound(&b, 2), run_bound(&b, 3), run_bound(&b, 5)), (Some(i64::MIN), Some(5), None), "cut: row n-1, a NULL first then a value");
    }
}
