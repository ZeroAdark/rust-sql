use std::sync::Arc;

use gravity_abi::Datum;
use gravity_program::{PlanKey, Program};

use crate::binding::{Binding, BindingParts};
use crate::entry::{Entry, Exec, ExecState, Step};
use crate::mem::budget;
use crate::node::{Call, Derive};
use crate::sink::{BorrowedBatch, Flow, Sink};
use crate::{Error, Result};

mod raw;

pub type Consume<'c> = dyn FnMut(&[Datum], &[u8]) -> Result<()> + 'c;

/// D4480: the most of a pull buffer's by-reference region kept past a run (`PullBuffer::end_run`, at `PullAdapter::close`):
/// a planned region's floor, two heap pages. A portal slot reuses its adapter for every Execute of the session. In RTT and
/// Speedtest over the extended protocol (119,591 statements), every run ended with a region of 0 B or exactly this floor.
/// PostgreSQL frees a portal's executor memory when the portal ends. Before this cap the region kept its largest block for
/// the session: a 50 MB text row left 50 MB.
pub const REGION_KEEP: usize = gravity_program::program::out_region_bytes(0, 0) as usize;

pub struct PullBuffer {
    /// memsig E-pub: one block each, charged to the buffer's holder (`new_on`'s owner; none: uncharged).
    values: memledger::ChargedVec<Datum>,
    isnull: memledger::ChargedVec<u8>,
    /// D4480: `region`'s capacity on the backend's session (`charge_region`). Declared before `region`: at the drop its
    /// bytes leave the session before the region's block is freed.
    region_charge: Option<budget::Charge>,
    /// The pushed blocks' by-reference regions (lower.md §15.13), the datums of the `byref` columns relocated into it.
    /// Reused across a run's blocks; at most `REGION_KEEP` past the run (`end_run`).
    region: Vec<u8>,
    byref: Vec<bool>,
    natts: usize,
    cap: usize,
    count: usize,
    cursor: usize,
}

impl PullBuffer {
    /// An uncharged buffer (a test's, a rig's): `new_on` with no owner.
    pub fn new(natts: usize, cap_rows: usize) -> PullBuffer {
        PullBuffer::new_on(natts, cap_rows, None)
    }

    /// memsig E-pub: a buffer of `cap_rows` rows of `natts` columns, its two arrays charged to `owner` (the holder that keeps
    /// the adapter: a served execution's portal or statement, a slot's session); `None`: uncharged.
    pub fn new_on(natts: usize, cap_rows: usize, owner: Option<(memledger::Owner, memledger::Kind)>) -> PullBuffer {
        let n = (natts * cap_rows).max(1);
        let (o, k) = (owner.map(|o| o.0), owner.map_or(memledger::Kind::Region, |o| o.1));
        let (mut values, mut isnull) = (memledger::ChargedVec::new(o, k), memledger::ChargedVec::new(o, k)); // alloc: Exec charged owner
        values.with_vec(|v| { v.reserve_exact(n); v.resize(n, 0) }); // alloc: Exec charged owner
        isnull.with_vec(|v| { v.reserve_exact(n); v.resize(n, 0) }); // alloc: Exec charged owner
        PullBuffer { values, isnull, region_charge: None, region: Vec::new(), byref: Vec::new(), natts, cap: cap_rows, count: 0, cursor: 0 } // alloc: Exec
    }

    /// Sized for `program` (once per open): room for one block's region, the by-reference columns marked.
    fn arm(&mut self, program: &Program) {
        self.region.clear();
        self.region.reserve(program.out_region as usize); // alloc: Exec
        self.charge_region();
        self.byref.clear();
        self.byref.extend(program.out.iter().map(|c| !c.typbyval)); // alloc: Exec
    }

    fn reset(&mut self) {
        self.count = 0;
        self.cursor = 0;
        self.region.clear();
    }

    pub fn buffered(&self) -> usize {
        self.count - self.cursor
    }

    /// D4480: the run's end (`PullAdapter::close`): a region over `REGION_KEEP` is freed, so a slot's adapter keeps at
    /// most `REGION_KEEP` between runs. The buffered rows go with it, as `reset` drops them.
    pub fn end_run(&mut self) {
        self.reset();
        if self.region.capacity() > REGION_KEEP {
            self.region = Vec::new(); // alloc: Exec
            self.charge_region();
        }
    }

    /// D4480: the region's capacity (at most `REGION_KEEP` past a run's end, `end_run`).
    pub fn region_capacity(&self) -> usize {
        self.region.capacity()
    }

    /// D4480: `region_charge` follows the region's capacity. A block within the capacity costs one compare.
    #[inline]
    fn charge_region(&mut self) {
        let cap = self.region.capacity() as u64;
        if cap != self.region_charge.as_ref().map_or(0, budget::Charge::committed) {
            charge_to(&mut self.region_charge, cap);
        }
    }
}

/// D4480: `charge` follows `cap`, opened on the backend's session at the first nonzero capacity. A thread with no backend
/// (a rig's) charges nothing.
#[cold]
#[inline(never)]
fn charge_to(charge: &mut Option<budget::Charge>, cap: u64) {
    if charge.is_none() {
        let a = budget::current();
        if !a.is_backend() {
            return;
        }
        *charge = Some(budget::Charge::open(budget::Owner::Session(a), budget::Release::Session, budget::Kind::Region, budget::charge::Door::Heap, 0));
    }
    if let Some(c) = charge.as_mut() {
        let have = c.committed();
        if cap > have {
            c.commit(cap - have);
        } else {
            c.free(have - cap);
        }
    }
}

impl Sink for PullBuffer {
    fn push(&mut self, batch: BorrowedBatch<'_>) -> Result<Flow> {
        if self.count + batch.rows > self.cap {
            return Err(Error::host("pull adapter overflow"));
        }
        let n = self.natts;
        if batch.values.len() != batch.rows * n || batch.isnull.len() != batch.rows * n {
            return Err(Error::host("pull adapter row width"));
        }
        let at = self.count * n;
        self.values[at..at + batch.rows * n].copy_from_slice(batch.values);
        self.isnull[at..at + batch.rows * n].copy_from_slice(batch.isnull);
        if !batch.region.is_empty() {
            let base = self.region.len();
            self.region.extend_from_slice(batch.region); // alloc: Exec (past the reserved block)
            self.charge_region();
            let to = self.region[base..].as_ptr();
            relocate(&mut self.values[at..at + batch.rows * n], &self.isnull[at..at + batch.rows * n], &self.byref, (batch.region.as_ptr() as u64, batch.region.len()), to);
        }
        self.count += batch.rows;
        Ok(Flow::Stop)
    }
}

/// The by-reference datums of `values` (the `byref` columns, not NULL) that point into `from` re-pointed
/// into its copy at `to`: the same offset. A datum outside `from` (a dictionary image, statement-stable) stays.
pub fn relocate(values: &mut [Datum], isnull: &[u8], byref: &[bool], from: (u64, usize), to: *const u8) {
    let (lo, hi) = (from.0, from.0 + from.1 as u64);
    let n = byref.len().max(1);
    for (i, v) in values.iter_mut().enumerate() {
        if byref.get(i % n).copied().unwrap_or(false) && isnull[i] == 0 && *v >= lo && *v < hi {
            *v = to as u64 + (*v - lo);
        }
    }
}

pub struct PullAdapter<'a> {
    buf: Box<PullBuffer>,
    binding: Box<Option<Binding<'a>>>,
    exec: Option<Exec>,
    entries: u64,
    /// The rows a `fetch` consumed before it left with a recheck (its count is lost in the error): `take_partial`.
    partial: usize,
    /// The emit pipeline has been called once (a producer's claims count as calls too: the emit's first call is a batch, never a resume).
    started: bool,
    /// The producers are the caller's (`set_external_producers`: the runner's `Producers`, sections or the serial arm):
    /// `fetch` stops before one. Otherwise it runs a producer's calls in line, as the serial driver does.
    external: bool,
    /// The program's error, held behind the rows it completed before it (settle_call pushed them): a FETCH that stops
    /// short of the failing row does not see it, the next one does (PostgreSQL's per-row order; ENTRY E3).
    failed: Option<Error>,
    /// D439: the bound program evaluates a VOLATILE call per row (`volatile`, read once at `open`): each call asks for the
    /// rows the fetch still owes, never a full block.
    volatile: bool,
    /// D588: the caller asks for D439's owed-rows cap on a program without a VOLATILE call (`set_owed_cap`: a root-bound index
    /// walk, whose rows read ahead of the FETCH would be heap visits and SIREAD locks PostgreSQL never takes).
    owed: bool,
    /// M8 (L0-k): the caller takes a step before each emit's first call (`set_stepwise`: its late words, an exec pruning over
    /// an InitPlan decided as the Append's first arm starts): `fetch` stops before an emit other than `ready`.
    stepwise: bool,
    ready: usize,
    /// C5b: the rows the lock hosts left locked when the execution closed (a portal positioned on its last row after the
    /// program finished: WHERE CURRENT OF still reads them; `Node::lockeds`).
    locked: Vec<(Option<u32>, gravity_abi::ScanPos)>,
}

impl<'a> PullAdapter<'a> {
    /// An uncharged adapter (a test's): `new_on` with no owner.
    pub fn new(natts: usize, cap_rows: usize) -> PullAdapter<'a> {
        PullAdapter::new_on(natts, cap_rows, None)
    }

    /// memsig E-pub: the adapter's row buffer charged to `owner` (`PullBuffer::new_on`).
    pub fn new_on(natts: usize, cap_rows: usize, owner: Option<(memledger::Owner, memledger::Kind)>) -> PullAdapter<'a> {
        PullAdapter { buf: Box::new(PullBuffer::new_on(natts, cap_rows, owner)), binding: Box::new(None), exec: None, entries: 0, partial: 0, started: false, failed: None, external: false, volatile: false, owed: false, stepwise: false, ready: usize::MAX, locked: Vec::new() } // alloc: Exec
    }

    pub fn open(&mut self, entry: &mut Entry, key: Option<PlanKey>, program: &Arc<Program>, derive: &mut dyn Derive, parts: BindingParts<'a>, lease: std::rc::Rc<crate::mem::grant::Reservation>) -> Result<()> {
        assert!(self.exec.is_none(), "adapter already open");
        let sink: &'a mut PullBuffer = raw::lend_buffer(&mut self.buf);
        *self.binding = Some(Binding::new(parts, sink));
        let b = self.binding.as_mut().as_mut().expect("binding");
        match entry.start(key, program, derive, b, lease) {
            Ok(exec) => {
                self.exec = Some(exec);
                self.locked.clear();
                self.buf.arm(program);
                self.buf.reset();
                self.entries = 0;
                self.started = false;
                self.stepwise = false;
                self.ready = usize::MAX;
                self.failed = None;
                self.volatile = pull_exact(program);
                Ok(())
            }
            Err(e) => {
                *self.binding = None;
                Err(e)
            }
        }
    }

    pub fn fetch(&mut self, entry: &mut Entry, n: usize, consume: &mut Consume<'_>) -> Result<usize> {
        let exec = self.exec.as_mut().expect("fetch on a closed adapter");
        let binding = self.binding.as_mut().as_mut().expect("binding");
        let buf = &mut *self.buf;
        let mut served = 0;
        while served < n {
            if buf.cursor < buf.count {
                let k = buf.natts;
                let at = buf.cursor * k;
                consume(&buf.values[at..at + k], &buf.isnull[at..at + k])?;
                buf.cursor += 1;
                served += 1;
                continue;
            }
            buf.reset();
            if let Some(e) = self.failed.take() {
                return Err(e);
            }
            if exec.state() == ExecState::Done {
                break;
            }
            // multi-emit: the emits in pipeline order; a producer before the next one is the caller's (`producer_next`)
            if self.external && !is_emit(exec.node().program(), exec.next_pipeline()) {
                break;
            }
            // M8 (L0-k): the caller's step before this emit's first call (`emit_next`, `set_ready`)
            if self.stepwise && exec.next_pipeline() != self.ready {
                break;
            }
            self.entries += 1;
            // D439: a program with a VOLATILE call is asked for the rows this fetch still owes (its sink stops at the cap), so
            // no call runs on a row the cursor may never fetch; any other program fills its block and the rest wait buffered
            let rows = exec.node().program().layout.out_rows;
            let cap = if self.volatile || self.owed { rows.min(u32::try_from(n - served).unwrap_or(u32::MAX)) } else { rows };
            let call = if self.started { Call::resume(cap) } else { Call::batch(cap) };
            self.started = true;
            // D3388 (D2772 POST): the execution's turn before its first post pipeline, which the adapter pulls after the
            // statement's own emit (entry/post.rs `turn`: a no-op for any other pipeline and once turned)
            crate::entry::post_turn(exec, binding, exec.next_pipeline())?;
            let step = match entry.run_pipeline(exec, binding, call) {
                Ok(step) => step,
                Err(e) if buf.count > 0 => {
                    self.failed = Some(e);
                    continue;
                }
                Err(e) => return Err(e),
            };
            if let Step::Recheck { site } = step {
                self.partial = served;
                return Err(Error::Recheck(site));
            }
            // D824: the emit stopped for its lazy build: the caller runs it (`Entry::build_wait`, its producer arm) and fetches on
            if let Step::BuildWait { pipeline } = step {
                self.partial = served;
                return Err(Error::BuildWait(pipeline));
            }
            if buf.count == 0 && exec.state() == ExecState::Done {
                break;
            }
        }
        Ok(served)
    }

    /// The execution stands before a producer (a pipeline that does not end in the sink): `fetch` pulls nothing until the
    /// caller has run it through `with_open` (the producers of the next emit, multi-emit).
    pub fn producer_next(&self) -> bool {
        self.external && self.exec.as_ref().is_some_and(|e| e.state() != ExecState::Done && !is_emit(e.node().program(), e.next_pipeline()))
    }

    /// M8 (L0-k): the caller takes a step before each emit's first call (until `close`): `fetch` pulls nothing from an emit
    /// the caller has not readied (`emit_next`, then `set_ready` once its step ran).
    pub fn set_stepwise(&mut self, on: bool) {
        self.stepwise = on;
    }

    /// M8 (L0-k): the execution stands before an emit the caller has not readied (`set_stepwise`).
    pub fn emit_next(&self) -> bool {
        self.stepwise && self.exec.as_ref().is_some_and(|e| e.state() != ExecState::Done && e.next_pipeline() != self.ready && is_emit(e.node().program(), e.next_pipeline()))
    }

    /// M8 (L0-k): the emit the execution stands before is readied: `fetch` pulls it.
    pub fn set_ready(&mut self) {
        self.ready = self.exec.as_ref().map_or(usize::MAX, |e| e.next_pipeline());
    }

    /// D588: every call asks for the rows the fetch still owes, as a VOLATILE program's (D439), until the caller turns it off:
    /// a program whose rows must not be computed ahead of the FETCH (a root-bound index walk; a lock program's rows).
    pub fn set_owed_cap(&mut self, on: bool) {
        self.owed = on;
    }

    /// The caller runs the producers (until `close`).
    pub fn set_external_producers(&mut self, on: bool) {
        self.external = on;
    }

    /// The open execution and its binding, before a pull: the producers of the next emit run to completion here (the
    /// runner's serial arm or a section), then `fetch` pulls that emit, and the ones after it, as a one-pipeline program's.
    pub fn with_open<E>(&mut self, f: impl FnOnce(&mut Exec, &mut Binding<'a>) -> core::result::Result<(), E>) -> core::result::Result<(), E> {
        let exec = self.exec.as_mut().expect("with_open on a closed adapter");
        let binding = self.binding.as_mut().as_mut().expect("binding");
        f(exec, binding)
    }

    /// The rows the last `fetch` consumed before its recheck, taken once.
    pub fn take_partial(&mut self) -> usize {
        core::mem::take(&mut self.partial)
    }

    /// D2044: the program's held error, taken: the caller ends the fetch with it where the rows before it cannot go on (an
    /// SPI receiver once the raise left its callee's SPI connection behind, exec runner `PlState`).
    pub fn take_failed(&mut self) -> Option<Error> {
        self.failed.take()
    }

    /// Buffered rows only (no pipeline entry): after a recheck put the
    /// surviving row in the buffer, the caller drains it and closes.
    pub fn drain(&mut self, consume: &mut Consume<'_>) -> Result<usize> {
        let buf = &mut *self.buf;
        let mut served = 0;
        while buf.cursor < buf.count {
            let k = buf.natts;
            let at = buf.cursor * k;
            consume(&buf.values[at..at + k], &buf.isnull[at..at + k])?;
            buf.cursor += 1;
            served += 1;
        }
        buf.reset();
        Ok(served)
    }

    /// True once the pipeline has run to `Done` and nothing is buffered.
    pub fn is_done(&self) -> bool {
        self.exec.as_ref().is_none_or(|e| e.state() == ExecState::Done) && self.buf.buffered() == 0
    }

    pub fn is_open(&self) -> bool {
        self.exec.is_some()
    }

    /// D2701: the open execution's emit keeps its input slots for the statement (`Entry::keep_for_rewind`).
    pub fn keep_for_rewind(&mut self, entry: &mut Entry) {
        if let Some(exec) = self.exec.as_mut() {
            entry.keep_for_rewind(exec);
        }
    }

    /// D2701: the open execution's emit runs again from its first row (`Entry::rewind_emit`); the rows buffered and the
    /// error held are the old run's and go.
    pub fn rewind(&mut self, entry: &mut Entry) -> Result<()> {
        let exec = self.exec.as_mut().expect("rewind on a closed adapter");
        entry.rewind_emit(exec)?;
        self.buf.reset();
        self.started = false;
        self.failed = None;
        self.partial = 0;
        self.ready = usize::MAX;
        Ok(())
    }

    /// The sites' positions the node settled (`Node::positions`): what a recheck substitutes.
    pub fn positions(&self) -> &[gravity_abi::ScanPos] {
        self.exec.as_ref().map_or(&[], |e| e.node().positions())
    }

    pub fn recheck_pos(&self) -> gravity_abi::ScanPos {
        self.exec.as_ref().map_or(gravity_abi::ScanPos::NONE, Exec::recheck_pos)
    }

    /// D3580: the row the statement's emit pipeline last took from its source (`Node::taken`), `NONE` with no execution:
    /// WHERE CURRENT OF over a served portal without a lock.
    pub fn taken(&self) -> gravity_abi::ScanPos {
        self.exec.as_ref().map_or(gravity_abi::ScanPos::NONE, |e| e.node().taken())
    }

    /// C5b: the row the lock host of relation `rel` last left locked (`Node::locked`): WHERE CURRENT OF over a served
    /// FOR UPDATE portal.
    pub fn locked(&self, rel: u32) -> gravity_abi::ScanPos {
        self.exec.as_ref().map_or_else(|| crate::node::locked_in(&self.locked, rel), |e| e.node().locked(rel))
    }

    pub fn binding_mut(&mut self) -> Option<&mut Binding<'a>> {
        self.binding.as_mut().as_mut()
    }

    pub fn rows_processed(&self) -> u64 {
        self.binding.as_ref().as_ref().map_or(0, |b| b.rows_processed)
    }

    /// D3547: `close` whose Node parks nowhere, torn down instead: the run ends at another statement level than the one it
    /// was opened at (the runner's named portal FETCHed from a nested level), whose park a later admission of the same
    /// plan key reads (a Node built over another admission: D2190).
    pub fn close_unparked(&mut self, entry: &mut Entry, ok: bool) {
        if let Some(exec) = self.exec.as_mut() {
            exec.forget_key();
        }
        self.close(entry, ok);
    }

    pub fn close(&mut self, entry: &mut Entry, ok: bool) {
        if let Some(exec) = self.exec.take() {
            self.locked = exec.node().lockeds();
            entry.end(exec, ok && self.failed.is_none());
        }
        self.failed = None;
        self.external = false;
        *self.binding = None;
        self.buf.end_run();
    }

    pub fn entries(&self) -> u64 {
        self.entries
    }

    /// The open execution (its node's site blocks carry the per-site counters EXPLAIN ANALYZE reads).
    pub fn exec(&self) -> Option<&Exec> {
        self.exec.as_ref()
    }

    pub fn buffered(&self) -> usize {
        self.buf.buffered()
    }
}

/// An emit: every pipeline that ends in the sink, the last one included (multi-emit).
pub fn is_emit(program: &Program, p: usize) -> bool {
    p + 1 >= program.pipelines.len() || program.pipelines[p].ends_in == gravity_program::End::Sink && !init_cell(program, p)
}

/// M8 (RT-1): pipeline `p` is an InitPlan's (`nest.initplan`'s Init proto names it): it ends in the Init's cell, not the
/// statement's sink, whatever its `ends_in` says (the graph's InitPlan pipeline ends in the sink).
pub(crate) fn init_cell(program: &Program, p: usize) -> bool {
    program.v4.as_ref().is_some_and(|d| d.protos.iter().any(|x| x.kind == gravity_program::protos::ProtoKind::Init && x.pipeline as usize == p))
}

/// D2998: pipeline `p` is an EXISTS InitPlan's (`INIT_EXISTS`): its end only stores true into the cell and stops, so it may
/// run as a section, whose workers send no row to the sink (gravity_parallel `Section::plan`).
pub fn exists_cell(program: &Program, p: usize) -> bool {
    program.v4.as_ref().is_some_and(|d| d.protos.iter().any(|x| x.kind == gravity_program::protos::ProtoKind::Init && x.pipeline as usize == p && x.arg & gravity_abi::limits::INIT_EXISTS != 0))
}

/// D439: the lowering declared a VOLATILE call per row (`V4Descs::pull_exact`), or the program locks rows (`write.lock`,
/// LockRows: PG locks a row per pull, so a FETCH n locks n rows, none ahead): a pull runs no row ahead of its count.
pub fn pull_exact(program: &Program) -> bool {
    program.write.lock != gravity_program::LockKind::None || program.v4.as_ref().is_some_and(|d| d.pull_exact)
}
