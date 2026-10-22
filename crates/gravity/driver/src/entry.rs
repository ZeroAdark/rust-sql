use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use gravity_abi::{Datum, MorselRange, ScanPos};
use gravity_program::{Parallel, PlanKey, Program};

use crate::binding::Binding;
use crate::mem::grant::{Grant, Reservation};
use crate::node::{Call, Derive, Node, NodeState, Ret};
use crate::park::Park;
use crate::sink::{BorrowedBatch, Flow, Merge, Partial, Sink};
use crate::source::Source;
use crate::{Error, Result};

mod raw;
pub use raw::current_or;

pub(crate) mod post;
pub use post::{turn as post_turn, POST_TURN};
pub mod record;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecState {
    Ready,
    Running,
    Done,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Suspended { out_full: bool },
    Recheck { site: u8 },
    /// D824 (V4R-298): the pipeline stopped at its outer-first probe's first row; its lazy build `pipeline` runs now
    /// (`Entry::build_wait`), then the same call resumes (`Call::resume` / `Entry::resume_claim`).
    BuildWait { pipeline: usize },
    Done,
}

/// A recheck's surviving row. `region` receives the row's by-reference images and its `byref` datums point into it
/// (D470): the node's out region they were pushed in is the node's again once the recheck releases it.
pub struct RowBuf<'a> {
    pub values: &'a mut [Datum],
    pub isnull: &'a mut [u8],
    pub region: &'a mut Vec<u8>,
    /// Per column: by reference (`i % len`, as `relocate` reads it).
    pub byref: &'a [bool],
}

/// D2744: a MERGE recheck's second surviving row, ExecMerge's pending NOT MATCHED row (mt_merge_pending_not_matched): a
/// MATCHED row whose recheck no longer joins runs the NOT MATCHED BY SOURCE action over the newest version and then the
/// source row's NOT MATCHED one, each returning its row under RETURNING. The recheck node's second pushed row, its images
/// owned here as `RowBuf`'s are (`got`: it came).
#[derive(Default)]
pub struct Pending {
    pub values: Vec<Datum>,
    pub isnull: Vec<u8>,
    pub region: Vec<u8>,
    pub got: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recheck {
    Pass,
    Skip,
}

/// Whether `EntryStats::build_us` / `apply_us` are timed (two clock reads per statement, ~0.1 µs of a served OLTP statement):
/// only while someone prints them, the runner's `PGRUST_GRAVITY_PHASES` line (exec phases.rs sets it at its switch read).
pub static TIMED: AtomicBool = AtomicBool::new(false);

/// D3203: the statement end's parts in microseconds, summed while `TIMED` (the runner's PHASES stmt line prints the
/// statement's share as `endparts=`): 0 the leader's slot regions to the depot, 1 the leader's grow chunks and spills,
/// 2 the workers' regions, 3 the Node's remove (the hosts unbound), 4 its reset up to the spill table, 5 the spill
/// table's files dropped, 6 the rest of the reset (the exec regions zeroed), 7 the worker slots to the depot and the
/// release log, 8 the park (or the teardown).
pub static END_US: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];

/// D3203: the time since the last lap added to `END_US[i]`; no clock read unless `TIMED` started `t`.
pub(crate) fn end_lap(t: &mut Option<u64>, i: usize) {
    if let Some(t0) = t {
        let now = pg_clock::mono_ns();
        END_US[i].fetch_add(now.saturating_sub(*t0) / 1000, Ordering::Relaxed);
        *t = Some(now);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EntryStats {
    pub built: u64,
    pub reused: u64,
    pub ended_ok: u64,
    pub ended_failed: u64,
    pub rechecks: u64,
    /// Microseconds `open_section` spent provisioning worker slots (a fresh Node builds them,
    /// a parked one has them) and binding the workers' hosts, accumulated: the census splits
    /// a session's first section open from the section proper.
    pub provision_us: u64,
    pub bind_us: u64,
    /// The slots' zeroing and rewind inside `open_section`; the Node build and the leader's bind inside `start`.
    pub zero_us: u64,
    pub build_us: u64,
    pub apply_us: u64,
    /// D3253: Nodes parked under a plan key and dropped at a take by another registry than their builder (`acquire`).
    pub other_admission: u64,
}

pub struct Exec {
    /// Boxed: an execution moves between the park, `start` and its caller several times per statement (a nested
    /// statement once per call), each a copy of the whole Node inline.
    node: Box<Node>,
    key: Option<PlanKey>,
    state: ExecState,
    calls: u64,
    next_pipeline: usize,
    binding_id: *const (),
    /// The slot depot's salt for this statement's program (the runner's: the database and the
    /// recipes' identity); `None` keeps the worker slots with the Node.
    slot_salt: Option<u64>,
    /// The statement's admission (its grant and reservation), kept alive while the Node draws from it; declared after `node`.
    lease: std::rc::Rc<Reservation>,
    /// D276: the pipelines the runner runs as if no claim reached them (bit p = pipeline p < 64; `run_empty`).
    empty: u64,
    /// D4050: the same past pipeline 63 (word w bit b = pipeline 64 (w + 1) + b; `run_empty_words`), sized at the start
    /// by the program's pipelines (`past_words`), so a program of at most 64 pipelines holds none and the run never grows it.
    empty_past: Box<[u64]>,
    /// D824: the next `run_pipeline` re-enters the call a `BuildWait` stopped (a producer's too: its calls are batches otherwise).
    resume_next: bool,
    /// D4101 (op 14 in a nested run): the pipeline whose call an out-of-order run holds (`out_of_order`'s `q`, the owner of
    /// the members running), `NO_OWNER` outside one. `defer_fed` passes it by: it keeps its members' slots for the
    /// statement (D3515) and reads none of their rows.
    nest_owner: usize,
}

/// `Exec::nest_owner` outside an out-of-order run.
const NO_OWNER: usize = usize::MAX;

impl Exec {
    pub fn state(&self) -> ExecState {
        self.state
    }

    /// D3547: the execution ends under no key: its Node is torn down at `Entry::end`, never parked (a run ended at another
    /// statement level than the one whose park it came from: `PullAdapter::close_unparked`).
    pub(crate) fn forget_key(&mut self) {
        self.key = None;
    }

    /// The execution's memory grant: its admission's (`Entry::start`).
    pub fn grant(&self) -> Grant {
        self.lease.grant
    }

    pub fn reservation(&self) -> &Reservation {
        &self.lease
    }

    /// The admission's lease, shared (a hold beside a mutable borrow of the execution).
    pub fn lease(&self) -> std::rc::Rc<Reservation> {
        self.lease.clone()
    }

    pub fn set_slot_salt(&mut self, salt: Option<u64>) {
        self.slot_salt = salt;
    }

    pub fn slot_salt(&self) -> Option<u64> {
        self.slot_salt
    }

    pub(crate) fn mark_opened(&mut self, pipeline: usize) {
        self.node.mark_opened(pipeline);
    }

    pub(crate) fn set_state(&mut self, s: ExecState) {
        self.state = s;
    }

    pub fn next_pipeline(&self) -> usize {
        self.next_pipeline
    }

    /// D276 (PG's build-first hash join: ExecHashJoin returns at an empty build before its outer child is ever fetched):
    /// the producers in `mask` (bit p = pipeline p) run as if no claim reached them, so their errors and effects never
    /// happen. The runner opens no section and runs no claim for them; the finish, the owner fold and `end_claims` run as
    /// for a relation every granule of which was pruned, so each consumer reads a well-formed empty product. A pipeline
    /// already run (before `next_pipeline`) or one without a driving site is left alone; `mask` names pipelines 0..64
    /// (`run_empty_words` any). A protocol calls it from its `after_pipeline`. D2584: an emit with a driving site in the
    /// mask (an Append arm its pruning removed, which streams into the sink: the runner marks it before its first call) is
    /// never opened nor called (`run_pipeline`): it pushes no row and its source never walks, as PostgreSQL never
    /// initializes or runs a pruned arm.
    pub fn run_empty(&mut self, mask: u64) {
        self.empty |= mask & !ran_of(self.next_pipeline, 0);
    }

    /// `run_empty` over a set of any width (word w bit b = pipeline 64 w + b): D4050, a set operation's right side past
    /// pipeline 63, which a u64 mask cannot name (PG's SetOp never reads its inner after an empty outer). A bit past the
    /// program's pipelines is dropped.
    pub fn run_empty_words(&mut self, mask: &[u64]) {
        let Some((&first, past)) = mask.split_first() else { return };
        self.run_empty(first);
        let next = self.next_pipeline;
        for (w, (e, &m)) in self.empty_past.iter_mut().zip(past).enumerate() {
            *e |= m & !ran_of(next, 64 * (w + 1));
        }
    }

    /// Pipeline `p` runs with its claims again (an earlier run's `run_empty` named it).
    fn unmask(&mut self, p: usize) {
        match p.checked_sub(64) {
            None => self.empty &= !(1u64 << p),
            Some(q) => {
                if let Some(e) = self.empty_past.get_mut(q / 64) {
                    *e &= !(1u64 << (q % 64));
                }
            }
        }
    }

    /// Op 14: the statement's held (deferred) pipeline error, if any, without a raise.
    pub fn held_error(&self) -> Option<Error> {
        self.node.spill.held()
    }

    /// Whether pipeline `p` runs as if no claim reached it (`run_empty`). Gen ct-0097: `run_pipeline` reads it too, so a
    /// pipeline without a driving site (a reader of a CTE's pulled store, a VALUES, an emit) that `run_empty` names is no
    /// longer left alone: it is opened and ends at its first call without its program, pulling no store.
    pub fn runs_empty(&self, p: usize) -> bool {
        match p.checked_sub(64) {
            None => self.empty >> p & 1 != 0,
            Some(q) => self.empty_past.get(q / 64).is_some_and(|e| e >> (q % 64) & 1 != 0),
        }
    }

    pub(crate) fn advance_pipeline(&mut self) {
        self.next_pipeline += 1;
    }

    pub fn node_mut(&mut self) -> &mut Node {
        &mut self.node
    }

    pub fn calls(&self) -> u64 {
        self.calls
    }

    pub fn node(&self) -> &Node {
        &self.node
    }

    pub fn recheck_pos(&self) -> ScanPos {
        self.node.recheck_pos()
    }
}

/// The bits of pipelines `base..base + 64` that ran before `next` (`run_empty` leaves them alone).
fn ran_of(next: usize, base: usize) -> u64 {
    match next.saturating_sub(base) {
        r if r >= 64 => u64::MAX,
        r => (1u64 << r) - 1,
    }
}

/// D4050: `Exec::empty_past` for a program of `n` pipelines: one word per 64 pipelines past the 64th, none up to 64.
fn past_words(n: usize) -> Box<[u64]> {
    vec![0u64; n.saturating_sub(1) / 64].into_boxed_slice() // alloc: Exec
}

struct Capture<'o> {
    out: RowBuf<'o>,
    got: bool,
    /// D2744: a MERGE recheck's second row (`Pending`), taken after the first; the recheck stops once it has both
    pending: Option<&'o mut Pending>,
}

impl Sink for Capture<'_> {
    fn push(&mut self, batch: BorrowedBatch<'_>) -> Result<Flow> {
        let first = !self.got;
        if batch.rows > 0 && !self.got {
            let (v, n) = batch.row(0);
            self.out.values[..v.len()].copy_from_slice(v);
            self.out.isnull[..n.len()].copy_from_slice(n);
            // D470: the images copied out of the node's region, which it reuses (and poisons, debug / SCRIBBLE) at its release
            self.out.region.clear();
            self.out.region.extend_from_slice(batch.region); // alloc: Cold
            crate::pull::relocate(&mut self.out.values[..v.len()], &self.out.isnull[..n.len()], self.out.byref, (batch.region.as_ptr() as u64, batch.region.len()), self.out.region.as_ptr());
            self.got = true;
        }
        // D2744: the next row into the pending buffer, then stop (no buffer: stop at the first row, as before)
        let byref = self.out.byref;
        let Some(p) = self.pending.as_deref_mut() else { return Ok(Flow::Stop) };
        let r = usize::from(first && batch.rows > 0);
        if r < batch.rows && self.got && !p.got {
            let (v, n) = batch.row(r);
            p.values.clear();
            p.values.extend_from_slice(v); // alloc: Cold
            p.isnull.clear();
            p.isnull.extend_from_slice(n); // alloc: Cold
            p.region.clear();
            p.region.extend_from_slice(batch.region); // alloc: Cold
            crate::pull::relocate(&mut p.values[..], &p.isnull[..], byref, (batch.region.as_ptr() as u64, batch.region.len()), p.region.as_ptr());
            p.got = true;
        }
        Ok(if p.got { Flow::Stop } else { Flow::Continue })
    }
}

/// `run_pipeline_at`'s first call of pipeline `p`: its share of the grant, its open, marked opened (a failed open fails
/// the exec). D3620: out of line, so the run's frame (on every nested statement's stack path) does not hold the share.
#[inline(never)]
fn open_first(exec: &mut Exec, p: usize) -> Result<()> {
    let split = exec.node.share(p, exec.grant(), 1);
    if let Err(e) = exec.node.open_pipeline(p, split.breaker, split.images.bytes) {
        exec.state = ExecState::Failed;
        return Err(e);
    }
    exec.node.mark_opened(p);
    crate::ahead_trace::first_run(exec.node.program(), p); // construct C2 (b)
    Ok(())
}

/// D824: the lazy build pipeline `q`'s `consumed`-th slot names (its producer with `lazy_for == q`).
fn lazy_build(program: &Program, q: usize, consumed: u8) -> Result<usize> {
    let slot = program.pipelines[q].consumes.get(consumed as usize).copied();
    let b = (0..q).rev().find(|&b| program.pipelines[b].lazy_for as usize == q && program.pipelines[b].produces() == slot && slot.is_some());
    // rowsecurity#343 (lowerv4 rerun/spool/arm_fill.rs): a later reader of a lazy build's slot (a streamed write's later
    // arm, its nest copy over the same Material) waits on it when no earlier reader ran it
    let b = b.or_else(|| slot.and_then(|s| program.producer_of_slot(q, s)).filter(|&b| program.pipelines[b].lazy_for >= 0 && (program.pipelines[b].lazy_for as usize) < q));
    b.ok_or_else(|| Error::BadReturn(gravity_abi::status::build_wait(consumed)))
}

/// D1005: pipeline `p` runs in lazy build `b`'s wait for `q`: `b` itself, or a producer of `b`'s deferred with it (`lazy_for`
/// `q`, read by `b` or by another such producer), never another probe's lazy build in `q` nor a lazy build of a producer's
/// own probe (that one waits in turn).
/// D2622 (errorhold): an earlier arm of a streamed union's run is fed with its last arm (`run_mate`): the slot names that
/// arm alone as its producer, and the earlier arms, stepped past with it (`skip_lazy`), never ran, so only the last arm's
/// rows reached the build (a HashAggregate over `pg_partition_tree(..) UNION ALL ..` under an outer-first lazy Hash).
/// D2652 (material): so is a settled union's earlier feed (`feed_mate`), whose slot names no reader at all.
fn fed_lazy(program: &Program, p: usize, b: usize, q: usize) -> bool {
    stack_depth_core::check_stack_depth_or_panic();
    p == b || program.pipelines[p].lazy_for as usize == q && (p + 1..=b).any(|m| (program.producers_of(m).any(|x| x == p) || run_mate(program, p, m) || feed_mate(program, p, m)) && fed_lazy(program, m, b, q))
}

/// D2652: pipeline `p` is a deferred producer whose production no pipeline reads (`last_consumer`): a settled union's feed
/// before its last (lowerv4 rerun/union_arms.rs: each feed appends its rows to the record the union's emit reads, whose
/// slot names its last feed alone as the producer; D1005's renumbering gives each deferred feed a slot of its own). It is
/// fed with `m`, the next pipeline deferred for the same probe (the next feed, or the first pipeline of the next arm), so
/// it runs in exactly one wait: a lazy Hash over `Result(f(x)) <- Append(scan, scan)` lost every feed but the last.
fn feed_mate(program: &Program, p: usize, m: usize) -> bool {
    let ps = &program.pipelines;
    ps[p].produces().is_some() && program.last_consumer(p) == p && (p + 1..ps.len()).find(|&x| ps[x].lazy_for == ps[p].lazy_for) == Some(m)
}

/// D2622: pipeline `p` is an earlier arm of the append.arms run holding pipeline `m` (D393: consecutive pipelines producing
/// one slot as one production, the run's `Arms` proto; the driver opens the slot at the run's first arm, `Node::arms_of`).
fn run_mate(program: &Program, p: usize, m: usize) -> bool {
    let Some(s) = program.pipelines[p].produces() else { return false };
    let run = |a: &gravity_program::protos::Proto| a.pipeline as usize..a.pipeline as usize + a.arg as usize;
    p < m && program.pipelines[m].produces() == Some(s) && program.v4.as_deref().is_some_and(|v4| v4.protos.iter().any(|a| a.kind == gravity_program::protos::ProtoKind::Arms && a.slot == s && run(a).contains(&p) && run(a).contains(&m)))
}

/// D1876: the rows the outer call left in the out block (its count and region fill), set aside while a nested run's members
/// run and put back after it. A member's end flushes the out block (the kernels' `done`): the outer call's rows were pushed
/// mid-row and the receiver's stop dropped, so the outer call ran past a pull's cap (a cursor's FETCH n, an Execute's max
/// rows, a PL/pgSQL FOR loop), each re-run calling its VOLATILE members again, until the pull buffer overflowed. No member
/// ends in the out block (a slot or an Init's cell): rows left there after the run are a Bug by name.
struct OutRows(i64, i64);

/// M8 (RT-1): an out-of-order run the leader makes from inside one of its pipeline calls: a host in the call (an InitPlan
/// read, `m8_init_run`) runs `members` (program order, width 1) nested, and the call goes on after it
/// (`Entry::run_nested`). The runner publishes one while it runs the program (gravity_parallel `publish`); nothing else
/// uses it while a run is in progress.
pub trait OnDemand {
    fn run_members(&mut self, members: &[u16]) -> Result<()>;
    /// M8 (RT-2e): a rerun set's `members` (program order) run again now, as PG re-executes a subplan on chgParam: each
    /// member's kept slot reset in place, then the run nested as `run_members` (`Entry::rerun_nested`). A frame that runs
    /// no rerun refuses by name.
    fn rerun_members(&mut self, _members: &[u16]) -> Result<()> {
        Err(Error::Refused("gravity: Bug: a rerun through a frame that runs none (M8 RT-2e)".into()))
    }
    /// D4120: `rerun_members` whose kept members fill again (an enclosing owner changed a param their fill reads): their
    /// kept slots reset in place as every member's (`Entry::rerun_nested`'s `refill`). A frame that runs no rerun refuses
    /// by name.
    fn refill_members(&mut self, _members: &[u16]) -> Result<()> {
        Err(Error::Refused("gravity: Bug: a refill through a frame that runs none (D4120)".into()))
    }
    /// M8 (RT-2f): a pulled set's `members` (program order; the last one the pulled pipeline, PG's CTE plan under
    /// CteScanNext) run on until the last one stops resumably after the rows its host's budget word allows, or ends
    /// (`Entry::pull_nested`). `budget` is the protocol's (the driver never reads it). A frame that pulls none refuses by name.
    fn pull_members(&mut self, _members: &[u16], _budget: u64) -> Result<Pulled> {
        Err(Error::Refused("gravity: Bug: a pull through a frame that pulls none (M8 RT-2f)".into()))
    }
    /// setop_rec-0043 (PG's ExecReScanCteScan): a pulled set's `members` (program order, the pulled pipeline last) re-armed
    /// by a rerun set's run, so its next pull starts the set again (`Entry::rearm_pulled`). A frame that pulls none refuses
    /// by name.
    fn rearm_pulled(&mut self, _members: &[u16]) -> Result<()> {
        Err(Error::Refused("gravity: Bug: a pulled set re-armed through a frame that pulls none (setop_rec-0043)".into()))
    }
    /// D1035 step 2 (V4R-444): `reader` (the next pipeline, not opened yet: its late step) is about to start. When it runs
    /// on workers (a section at width >= 2), the frame runs the Init's `members` now, out of order at their own widths
    /// (`Entry::run_ahead`), and answers true: PostgreSQL's Gather evaluates its `initParam` before launching workers. A
    /// reader that runs on the leader alone runs nothing here (false): its first read runs the members nested, as before.
    /// The default is the leader-only frame's (the crate rig's).
    fn prerun_members(&mut self, _reader: usize, _members: &[u16]) -> Result<bool> {
        Ok(false)
    }
    /// D2605: the main plan ended, before the statement-end drain of the pulled sets (PG's ExecutorRun before
    /// ExecPostprocessPlan): the statement's own writers queue their AFTER STATEMENT events (node/ends.rs `end_main`).
    fn end_main(&mut self) -> Result<()> {
        Ok(())
    }
}

/// M8 (RT-2f): what a pull did: the pulled pipeline stopped after its budget (a later pull resumes it), or ran to its end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pulled {
    Stopped,
    Done,
}

/// M8 (RT-1): a published `OnDemand` (the runner's, alive while it is published: gravity_parallel `publish`).
pub type Frame = core::ptr::NonNull<dyn OnDemand>;

/// D1702: the execution the innermost runner call (`run_pipeline`, a claim, a late step: `with_current`) is making its call
/// over, raw. A nested run (`OnDemand`) re-enters through it, never through its frame's own (the run's outermost) pointers:
/// each write it makes derives from every `&mut Entry` / `&mut Exec` / `&mut Binding` live across the call, as a host's
/// write to the `Inner` derives from `env` (D1744).
#[derive(Clone, Copy)]
pub struct Current {
    pub entry: *mut Entry,
    pub exec: *mut Exec,
    pub binding: *mut Binding<'static>,
}

/// D1702: the runner's slot for `Current` (`Entry::with_current_slot`), outside everything a frame holds `&mut` to.
pub type CurrentCell = core::cell::Cell<Option<Current>>;

/// D1702: the current execution before a call's own, put back on every exit of the call, an unwind included.
struct PutBack(*const CurrentCell, Option<Current>);

pub struct Entry {
    park: Park,
    pub(crate) stats: EntryStats,
    /// D610: the pipeline whose run last stopped on a recheck (`run_pipeline`), the one `recheck` runs on its second node
    /// (a program of several pipelines: an Append arm's write under run_parallel, the pull adapter's producers).
    recheck_pipeline: usize,
    /// D1507: the release-log generation this park has applied (`slots::park_released_since`).
    released_seen: u64,
    /// D1702: the running runner's `Current` slot (`with_current_slot`); null outside a run.
    current: *const CurrentCell,
    /// D2050 / D2674: the statement (its admission, held weakly so no later statement's can take its address) whose
    /// EvalPlanQual rechecks reached a marked probe, and the stopped pipelines whose last called recheck set its record
    /// (bit p = pipeline p; `epq_word`): PostgreSQL's `hj_OuterNotEmpty` of that join, which ExecReScanHashJoin keeps
    /// across one statement's rechecks and which a skipped call (`rerun`) leaves as it was.
    epq_outer: Option<(std::rc::Weak<Reservation>, u64)>,
    /// D3500: the counter region of the node whose run last stopped on a recheck (`run_pipeline`, `claim_at`; null for a
    /// plain program): `recheck` takes it and adds its recheck node's counts the statement keeps (node `fold_recheck`);
    /// `end` drops it with its node.
    recheck_instr: *mut u64,
    /// D2033 step 3: the running recheck's `seen` (`rerun`): a recheck of the statement set the stopped pipeline's epq
    /// probe record, so PG's join builds first in this one (`rerun_seen`).
    rerun_seen: bool,
    /// EXPLAIN ANALYZE's per-pipeline record of one program's run (record.rs), `None` outside one.
    record: Option<Box<record::Record>>,
}

/// H16 (d): a tier-0 program's tier for this execution, decided at its start and never changed inside it.
/// Its first execution interprets when every pipeline runs serial this execution (`Widths`) and predicts at most the cap's
/// rows (the knob at the compile); any other execution compiles first, synchronously. A compile that fails
/// interprets this execution and every later one (`Tiered::promote`, counted), never the statement's error: error-vs-value
/// is WRONG; a resource error PG would raise too (OOM, a cancel) unwinds out of the compile as any ereport does.
fn settle_tier(program: &Program, widths: Widths<'_>) {
    if !gravity_program::code::TIER0_LIVE.load(Ordering::Relaxed) {
        return;
    }
    let mut pending: Option<(bool, u64, u64)> = None;
    for i in 0..program.pipelines.len() {
        if let Some(t) = program.code_of(i).tier0().filter(|t| !t.settled()) {
            let (first, cap, bound) = pending.unwrap_or((true, u64::MAX, 0));
            pending = Some((first && t.runs() == 0, cap.min(t.cap()), bound.max(t.bound())));
        }
    }
    let Some((first, cap, bound)) = pending else { return };
    // D3867: a pipeline's prediction held to the statement's bound (a relation's counted line pointers under PostgreSQL's
    // 10-block guess for a never-analyzed one)
    let interpret = first && program.pipelines.iter().enumerate().all(|(i, p)| widths.serial(i, p) && p.predicted_rows.min(bound) <= cap);
    for i in 0..program.pipelines.len() {
        if let Some(t) = program.code_of(i).tier0() {
            // a failed compile interprets this execution and every later one (never the statement's error)
            if interpret || !t.promote() {
                t.note_run();
            }
        }
    }
}

/// The widths an execution runs its pipelines at, as tier 0's settle reads them (M4-OLTP-COLD lever T): a
/// pipeline the execution runs on the leader alone is serial whatever the lowerer declared, so a parallel-eligible scan of a
/// small heap (by_work's width 0) or any pipeline at W0 interprets its first execution as an ineligible one does.
#[derive(Clone, Copy, Debug)]
pub enum Widths<'w> {
    /// Unknown here: a pipeline is serial only when declared `Parallel::Ineligible` (the rule before lever T).
    Declared,
    /// Every pipeline runs on the leader alone (the serial runner).
    Serial,
    /// The runner's per-pipeline widths for this execution (0 = the leader alone); past the slice, `Declared`.
    Planned(&'w [u16]),
}

impl Widths<'_> {
    fn serial(&self, i: usize, p: &gravity_program::Pipeline) -> bool {
        let declared = matches!(p.parallel, Parallel::Ineligible { .. });
        match *self {
            Widths::Declared => declared,
            Widths::Serial => true,
            Widths::Planned(w) => w.get(i).map_or(declared, |&w| w == 0),
        }
    }
}

impl Entry {
    pub fn new(park_capacity: usize) -> Entry {
        Entry { park: Park::new(park_capacity), stats: EntryStats::default(), recheck_pipeline: 0, released_seen: crate::slots::park_released_current(), current: core::ptr::null(), epq_outer: None, recheck_instr: core::ptr::null_mut(), rerun_seen: false, record: None }
    }

    /// D3881: this entry as `new` makes it, its park's slots kept (a freed statement level's state, emptied for the next
    /// level built): its parked Nodes drop first.
    pub fn empty(&mut self) {
        let Entry { park, stats, recheck_pipeline, released_seen, current, epq_outer, recheck_instr, rerun_seen, record } = self;
        park.clear();
        *stats = EntryStats::default();
        *recheck_pipeline = 0;
        *released_seen = crate::slots::park_released_current();
        *current = core::ptr::null();
        *epq_outer = None;
        *recheck_instr = core::ptr::null_mut();
        *rerun_seen = false;
        *record = None;
    }

    /// D3881: the bytes of this entry's park slots (each holds its Node inline), parked or not.
    pub fn park_slot_bytes(&self) -> usize {
        self.park.slot_bytes()
    }

    /// D1702: `f` (a runner's run) with `cell` the slot this entry's calls publish their execution in, the slot before it
    /// back on every exit of `f`, an unwind included. D1709: the slot is stored only while `cell` is borrowed here.
    /// D3620: inlined, so a nested statement's stack path holds no frame of its own for it.
    #[inline(always)]
    pub fn with_current_slot<R>(&mut self, cell: &CurrentCell, f: impl FnOnce(&mut Entry) -> R) -> R {
        struct Back<'e>(&'e mut Entry, *const CurrentCell);
        impl Drop for Back<'_> {
            fn drop(&mut self) {
                self.0.current = self.1;
            }
        }
        let before = core::mem::replace(&mut self.current, cell);
        let mut back = Back(self, before);
        // construct C2 (b): a runner's frame runs its pipelines in program order (a nested statement's too)
        let _order = crate::ahead_trace::scope(crate::ahead_trace::Origin::Order);
        f(&mut *back.0)
    }

    /// D1702: a runner's slot is installed (`with_current_slot`): true only inside a run.
    pub fn has_current_slot(&self) -> bool {
        !self.current.is_null()
    }

    pub fn park(&mut self) -> &mut Park {
        &mut self.park
    }

    pub fn stats(&self) -> EntryStats {
        self.stats
    }

    /// D1507: the relations recorded since this park last looked (`slots::note_released`, from any thread's relcache
    /// callback, which never touches the park) released from its parked Nodes, here where the entry owns the park; the
    /// released cursors drop after.
    fn release_recorded(&mut self) {
        if let Some(record) = crate::slots::park_released_since(&mut self.released_seen) {
            let mut doomed: Vec<Box<dyn core::any::Any>> = Vec::new(); // alloc: Cold
            self.park.release_recorded(&record, &mut doomed);
            drop(doomed);
        }
    }

    fn acquire(&mut self, key: Option<PlanKey>, program: &Arc<Program>, derive: &mut dyn Derive) -> Result<Box<Node>> {
        self.release_recorded();
        let parked = match key.and_then(|k| self.park.take(k, program.key)) {
            // C3 S4c: a Node serves only the program it was built over (the same `Arc`): a content key names a program's shape
            // and the objects it reads, not the catalog state it was lowered under, in which a program filed again under the
            // key since (its recorded reads moved) differs
            Some(n) if !Arc::ptr_eq(n.program(), program) => {
                self.stats.other_admission += 1;
                fires::fire("park.other_program");
                n.teardown();
                None
            }
            // D3438: a Node runs over the host state its image names; a registry with another (the program filed again
            // from its template) builds its own
            Some(n) if derive.owner().is_some_and(|o| !n.runs_over(&o)) => {
                self.stats.other_admission += 1;
                n.teardown();
                None
            }
            // D3253: a plan key's Node serves only the registry it was built from: its sources may hold closures over that
            // admission (a bitmap's recheck and pre-check read its extern parameters, freed with it), and a later admission of
            // the same program under the same generic plan (a nested body's next statement) binds its own
            Some(n) if key.is_some_and(|k| !k.is_content()) && n.builder != derive.identity() => {
                self.stats.other_admission += 1;
                fires::fire("park.other_admission");
                n.teardown();
                None
            }
            // D3128: a content key's Node built from another registry whose recipes carry other counter cells (another
            // content key over the same program: an EXPLAIN ANALYZE EXECUTE's custom plan after the EXPLAIN ANALYZE of the
            // literal statement) would count its walk's heap fetches into the builder's cells, and read its run bound there:
            // EXPLAIN ANALYZE printed "Heap Fetches: 0" (m4_d574)
            Some(n) if n.builder != derive.identity() && n.cells != derive.cells() => {
                self.stats.other_admission += 1;
                fires::fire("park.content_other_cells");
                n.teardown();
                None
            }
            // A content key's Node holds nothing of its builder (`park_key` refuses rung, fetch and filtered or am-carrying
            // index recipes; a one-index bitmap source drops its recheck qual at the unbind and takes the executing
            // statement's at the bind) but its counter cells, the same ones (above); `Node::apply` rebinds every host per
            // start. The fire witnesses that reuse.
            Some(n) if n.builder != derive.identity() => {
                fires::fire("park.content_other_admission");
                Some(n)
            }
            p => p,
        };
        Ok(match parked {
            Some(n) => {
                self.stats.reused += 1;
                n
            }
            None => {
                self.stats.built += 1;
                let t0 = TIMED.load(Ordering::Relaxed).then(std::time::Instant::now);
                let n = Box::new(Node::try_build(program.clone(), derive)?); // alloc: Exec
                if let Some(t0) = t0 {
                    self.stats.build_us += t0.elapsed().as_micros() as u64;
                }
                n
            }
        })
    }

    fn release(&mut self, key: Option<PlanKey>, mut node: Box<Node>) {
        node.cool_workers();
        node.arena_census();
        // RULE 7: the statement's unspent reservation stays out of the depot's park budget while its regions park
        let room = node.stmt_room();
        node.set_stmt(core::ptr::null());
        let mut t = TIMED.load(Ordering::Relaxed).then(pg_clock::mono_ns);
        crate::park::REGIONS.parking_beside(room, || node.settle_regions(&mut t));
        if node.state() == NodeState::Bound {
            node.remove();
        }
        end_lap(&mut t, 3);
        node.reset_timed(&mut t);
        node.release_workers();
        if mempolicy::keep::kept_parked() {
            node.settle_kept();
        }
        self.release_recorded();
        end_lap(&mut t, 7);
        match key {
            Some(k) => {
                self.park.put(k, node);
            }
            None => node.teardown(),
        }
        end_lap(&mut t, 8);
    }

    /// A statement's execution under its admission `lease` (the runner's `admit_grant`; `Reservation::unadmitted` outside the pool).
    pub fn start(&mut self, key: Option<PlanKey>, program: &Arc<Program>, derive: &mut dyn Derive, binding: &mut Binding<'_>, lease: std::rc::Rc<Reservation>) -> Result<Exec> {
        self.start_at(key, program, derive, binding, lease, Widths::Declared)
    }

    /// `start` with the execution's pipeline widths (`Widths`), which tier 0's settle reads.
    pub fn start_at(&mut self, key: Option<PlanKey>, program: &Arc<Program>, derive: &mut dyn Derive, binding: &mut Binding<'_>, lease: std::rc::Rc<Reservation>, widths: Widths<'_>) -> Result<Exec> {
        settle_tier(program, widths);
        let mut node = self.acquire(key, program, derive)?;
        node.set_stmt(&*lease);
        let t0 = TIMED.load(Ordering::Relaxed).then(std::time::Instant::now);
        let applied = node.apply(binding);
        if let Some(t0) = t0 {
            self.stats.apply_us += t0.elapsed().as_micros() as u64;
        }
        if let Err(e) = applied {
            self.release(key, node);
            return Err(e);
        }
        let empty_past = past_words(program.pipelines.len());
        Ok(Exec { node, key, state: ExecState::Ready, calls: 0, next_pipeline: 0, binding_id: binding as *const Binding<'_> as *const (), slot_salt: None, lease, empty: 0, empty_past, resume_next: false, nest_owner: NO_OWNER })
    }

    pub fn run(&mut self, exec: &mut Exec, binding: &mut Binding<'_>, call: Call) -> Result<Step> {
        loop {
            let (p, last) = (exec.next_pipeline, exec.next_pipeline + 1 >= exec.node.program().pipelines.len());
            let step = self.run_pipeline(exec, binding, call).inspect_err(|e| {
                crate::note_unsupported(Some(p), e, false);
                crate::note_ahead(exec.node.program(), p, e);
            })?;
            if last || step != Step::Done {
                return Ok(step);
            }
        }
    }

    pub fn run_pipeline(&mut self, exec: &mut Exec, binding: &mut Binding<'_>, call: Call) -> Result<Step> {
        self.run_pipeline_at(exec, binding, call).map_err(|e| pool_refused(exec.reservation(), e))
    }

    fn run_pipeline_at(&mut self, exec: &mut Exec, binding: &mut Binding<'_>, call: Call) -> Result<Step> {
        assert!(matches!(exec.state, ExecState::Ready | ExecState::Running), "run on a {:?} exec", exec.state);
        assert!(core::ptr::eq(exec.binding_id, binding as *const Binding<'_> as *const ()), "run with a binding other than start's");
        let n = exec.node.program().pipelines.len();
        let out_rows = exec.node.program().layout.out_rows;
        let p = exec.next_pipeline;
        let last = p + 1 >= n;
        // an emit is every pipeline that ends in the sink (multi-emit: a UNION ALL's arms), the last one included
        let emit = crate::pull::is_emit(exec.node.program(), p);
        let first = !exec.node.is_opened(p);
        exec.calls += 1;
        // D2584: an emit over a source marked to run empty before it opened (`run_empty`) ends unopened: no call, no row,
        // the slots it was the last reader of freed as at any end
        if emit && first && exec.runs_empty(p) && exec.node.program().pipelines[p].driving_site >= 0 {
            exec.node.free_after(p);
            exec.next_pipeline += 1;
            exec.state = if last { ExecState::Done } else { ExecState::Running };
            return Ok(Step::Done);
        }
        // sort give-back: as `claim_at`'s, before the open reads the regions it consumes
        if let Err(e) = exec.node.give_back_sorts() {
            exec.state = ExecState::Failed;
            return Err(e);
        }
        if first {
            open_first(exec, p)?;
        }
        // an emit's first call is a batch (the caller's `call` resumes the arm before it); a producer runs whole batches
        let resumed = core::mem::take(&mut exec.resume_next);
        let call = if resumed { Call::resume(if emit { call.out_cap } else { out_rows }) } else if !emit { Call::batch(out_rows) } else if first { Call::batch(call.out_cap) } else { call };
        // D276 (gen ct-0097): a site-less pipeline an empty build masked (`run_empty`) ends at its first call, opened, its
        // program never called: its end holds what its open laid (no row), it pulls no CTE store and raises nothing, as a
        // claim-driven producer run with no claim. Once opened it runs on (a mask set mid-run is that run's no longer)
        let empty = first && exec.runs_empty(p);
        let (this, xp, bp): (*mut Entry, *mut Exec, *mut Binding<'_>) = (self, exec, binding);
        let called = if empty {
            Ok(Ret::Done)
        } else {
            raw::call_pipeline_published(this, xp, bp, p, call)
        };
        let r = called.and_then(|ret| {
            if gravity_abi::ctx::abort_set(binding.abort_flag) {
                return Err(Error::Cancelled);
            }
            // decision 9: a suspend is legal for an emit, or for any pipeline whose resume word says it
            // is emitting over a finished breaker (`resume::resumable`); the next call resumes the same pipeline
            let resumable = emit || gravity_abi::ctx::resume::resumable(raw::resume_of(exec.node.ctx()));
            match ret {
                Ret::Done => Ok(Step::Done),
                Ret::Suspended { out_full } if resumable => Ok(Step::Suspended { out_full }),
                Ret::Suspended { .. } => Err(Error::BadReturn(gravity_abi::status::SUSPEND)),
                Ret::BuildWait { consumed } => lazy_build(exec.node.program(), p, consumed).map(|b| Step::BuildWait { pipeline: b }),
                // D610: any pipeline the leader runs stops on a recheck (run_parallel's emit arm and the producers resume it too)
                Ret::Recheck { site } => {
                    self.recheck_pipeline = p;
                    self.recheck_instr = exec.node.instr_ptr();
                    Ok(Step::Recheck { site })
                }
            }
        });
        let r = match r {
            Ok(Step::Done) => exec.node.end_production(p).map(|()| Step::Done),
            other => other,
        };
        exec.state = match &r {
            Ok(Step::Done) => {
                exec.node.ended_site_words(p);
                exec.node.free_after(p);
                exec.next_pipeline += 1;
                if last {
                    ExecState::Done
                } else {
                    ExecState::Running
                }
            }
            Ok(_) => ExecState::Running,
            Err(_) => ExecState::Failed,
        };
        r
    }

    /// The width-1 arm of a producer, one claim at a time on the leader (decision 13; a serial source: one `ALL` claim):
    /// opens the pipeline's slot at the first claim; `Done` for a finished claim, `Suspended` when
    /// the sink said stop; the caller ends the pipeline with `end_claims`.
    pub fn run_claim(&mut self, exec: &mut Exec, binding: &mut Binding<'_>, claim: MorselRange) -> Result<Step> {
        self.claim_at(exec, binding, claim, false)
    }

    /// D824: the claim a `BuildWait` stopped, re-entered at its row after the lazy build ran (`build_wait`).
    pub fn resume_claim(&mut self, exec: &mut Exec, binding: &mut Binding<'_>, claim: MorselRange) -> Result<Step> {
        self.claim_at(exec, binding, claim, true)
    }

    fn claim_at(&mut self, exec: &mut Exec, binding: &mut Binding<'_>, claim: MorselRange, resume: bool) -> Result<Step> {
        exec.resume_next = false;
        assert!(matches!(exec.state, ExecState::Ready | ExecState::Running), "run on a {:?} exec", exec.state);
        assert!(core::ptr::eq(exec.binding_id, binding as *const Binding<'_> as *const ()), "run with a binding other than start's");
        let p = exec.next_pipeline;
        exec.calls += 1;
        // D2881: a claim after the run's first opens the pipeline again but is no new EXPLAIN ANALYZE loop (`FLAG_LATER_CLAIM`)
        let later = exec.node.is_opened(p) && !resume;
        // sort give-back: a revoke asked since the last claim puts the statement's finished sorts no reader has begun and
        // gives their pages back, before this pipeline's open reads the regions it consumes
        if let Err(e) = exec.node.give_back_sorts() {
            exec.state = ExecState::Failed;
            return Err(e);
        }
        if !exec.node.is_opened(p) {
            let split = exec.node.share(p, exec.grant(), 1);
            if let Err(e) = exec.node.open_pipeline(p, split.breaker, split.images.bytes) {
                exec.state = ExecState::Failed;
                return Err(e);
            }
            exec.node.mark_opened(p);
            crate::ahead_trace::first_run(exec.node.program(), p); // construct C2 (b)
        }
        // C5: a revoke asked since the leader's last service cuts the region this claim fills
        exec.node.revoke_home();
        exec.node.later_claim = later;
        let (this, xp, bp): (*mut Entry, *mut Exec, *mut Binding<'_>) = (self, exec, binding);
        let called = raw::call_claim_published(this, xp, bp, p, claim, resume);
        let r = called.and_then(|ret| {
            if gravity_abi::ctx::abort_set(binding.abort_flag) {
                return Err(Error::Cancelled);
            }
            match ret {
                Ret::Done => Ok(Step::Done),
                Ret::Suspended { out_full } => Ok(Step::Suspended { out_full }),
                Ret::BuildWait { consumed } => lazy_build(exec.node.program(), p, consumed).map(|b| Step::BuildWait { pipeline: b }),
                // D655: a claim's stop names its pipeline too (lowerv4 marks.rs refuses a rechecking producer by name)
                Ret::Recheck { site } => {
                    self.recheck_pipeline = p;
                    self.recheck_instr = exec.node.instr_ptr();
                    Err(Error::Recheck(site))
                }
            }
        });
        exec.state = match &r {
            Ok(_) => ExecState::Running,
            Err(_) => ExecState::Failed,
        };
        r
    }

    /// D824 (V4R-298): the next pipeline is a lazy build (`Pipeline::lazy_for`: PG's outer-first hash join fetches the outer's
    /// first row before it builds): its slot opens zeroed and pending, so its probe's pipeline opens over it, and the runner
    /// steps past it; it runs only when that probe stops with `BuildWait` (`build_wait`). D1005: a fed build's producers
    /// carry the same `lazy_for` and step past with it, their slots unopened (only the build reads them). `true`: stepped past.
    pub fn skip_lazy(&mut self, exec: &mut Exec) -> Result<bool> {
        let p = exec.next_pipeline;
        let program = exec.node.program();
        let Some(q) = program.pipelines.get(p).and_then(|x| usize::try_from(x.lazy_for).ok()) else { return Ok(false) };
        if program.pipelines[p].produces().is_some_and(|s| program.producer_of_slot(q, s) == Some(p)) {
            let split = exec.node.share(p, exec.grant(), 1);
            if let Err(e) = exec.node.open_pending(p, split.breaker, split.images.bytes) {
                exec.state = ExecState::Failed;
                return Err(e);
            }
        }
        exec.next_pipeline += 1;
        exec.state = ExecState::Running;
        Ok(true)
    }

    /// D824 (V4R-298): pipeline `q` (the next one) stopped on `BuildWait` for lazy build `b`: `q`'s leader attachment kept
    /// (`Node::hold`: the resume word, the claim, the arena, the sources' pins), `b` run out of order by the caller's own
    /// producer arm (`run`, with `b` as the next pipeline), then `q` re-attached (`Node::reattach`, the build's region read
    /// after its grows) as the next pipeline again, for the caller's resume. D1005: a fed build's producers (`fed_lazy`) run
    /// first, in program order, `b` last; a lazy build of theirs waits in turn (nested, `Node::hold` stacks). A failed or
    /// cancelled build releases `q`'s sources' holds and fails the execution.
    pub fn build_wait(&mut self, exec: &mut Exec, binding: &mut Binding<'_>, b: usize, run: &mut dyn FnMut(&mut Entry, &mut Exec, &mut Binding<'_>, usize) -> Result<()>) -> Result<()> {
        let q = exec.next_pipeline;
        // the reader the build is lazy for (`lazy_build`: a later reader may wait on it too)
        let q0 = exec.node.program().pipelines[b].lazy_for as usize;
        debug_assert!(q0 <= q, "a build wait for a later pipeline's lazy build");
        let program = exec.node.program().clone();
        let _demand = crate::ahead_trace::demand(crate::ahead_trace::Origin::BuildWait, &program, b);
        let r = self.out_of_order(exec, binding, &mut (0..q).filter(|&p| fed_lazy(&program, p, b, q0)), run);
        match r {
            Ok(()) => {
                exec.node.built_pending(b);
                exec.state = ExecState::Running;
                exec.resume_next = true;
                Ok(())
            }
            Err(e) => {
                exec.node.release_hosts();
                exec.state = ExecState::Failed;
                Err(e)
            }
        }
    }

    /// The one out-of-order run (D824 / D1005's build waits, M8's on-demand members): the next pipeline `q` held
    /// (`Node::hold`), each of `pipelines` run by the caller's own producer arm (`run`, with it as the next pipeline), a
    /// cancel checked after each, then `q` re-attached as the next pipeline again (`Node::reattach`), Ok or Err. A `q` not
    /// opened yet (M8: an Init read by a late step as `q` starts, L0-k) has no attachment to hold. The run takes a hosted lane
    /// of its own (D1036, `Node::nested_lane`), `q`'s ordinal back after it. D4101: `q` is the run's owner meanwhile
    /// (`Exec::nest_owner`), and a pipeline a hold passed (`hold_and_pass`: op 14's fed build and its feeders, the next
    /// pipeline left past them) is stepped past, as the top level's runner steps past it.
    fn out_of_order(&mut self, exec: &mut Exec, binding: &mut Binding<'_>, pipelines: &mut dyn Iterator<Item = usize>, run: &mut dyn FnMut(&mut Entry, &mut Exec, &mut Binding<'_>, usize) -> Result<()>) -> Result<()> {
        let q = exec.next_pipeline;
        // M8 (D1036): the run's hosted calls on a lane of their own (the outer row's pending results stay whole)
        let outer = exec.node.nested_lane()?;
        let h = exec.node.is_opened(q).then(|| exec.node.hold(q));
        exec.state = ExecState::Running;
        let owner = core::mem::replace(&mut exec.nest_owner, q);
        let mut r = Ok(());
        let mut passed = 0;
        for p in pipelines {
            if p < passed {
                continue;
            }
            exec.next_pipeline = p;
            r = run(self, exec, binding, p).and_then(|()| if gravity_abi::ctx::abort_set(binding.abort_flag) { Err(Error::Cancelled) } else { Ok(()) });
            if r.is_err() {
                break;
            }
            if exec.next_pipeline > p + 1 {
                passed = exec.next_pipeline;
            }
        }
        exec.nest_owner = owner;
        if let Some(h) = h {
            let back = exec.node.reattach(q, h);
            r = r.and(back);
        }
        exec.node.outer_lane(outer);
        exec.next_pipeline = q;
        r
    }

    /// M8 (RT-1): an Init's `members` (program order) run now, nested inside the leader's call of the next pipeline `q`, which
    /// is still on the stack (its host `m8_init_run` called the runner's `OnDemand`): `out_of_order` over them, with what
    /// `hold` does not keep saved around it (the call's flags and out cap, `Node::nest`; its rows in the out block, `OutRows`;
    /// the execution's resume and state), and the node's nested count up, so a member's stopped claim releases no pin `q`'s
    /// row still reads. `q`'s call goes on after it; an error is `q`'s host's to raise (the execution fails with it there).
    /// Each member's slot is one no other pipeline holds (lowerv4's renumbering, exec's prepare wall), so a member's opens
    /// and frees touch nothing `q` reads.
    pub fn run_nested(&mut self, exec: &mut Exec, binding: &mut Binding<'_>, members: &[u16], run: &mut dyn FnMut(&mut Entry, &mut Exec, &mut Binding<'_>, usize) -> Result<()>) -> Result<()> {
        // an Init's members run once per execution: a lazy build of a member probe (its `lazy_for`) opens pending before it
        let _demand = crate::ahead_trace::demand(crate::ahead_trace::Origin::Nested, exec.node.program(), members.last().map_or(usize::MAX, |&m| m as usize));
        self.run_nested_with(exec, binding, members, Some(&|p: &Program, m: u16| u16::try_from(p.pipelines[m as usize].lazy_for).ok().filter(|q| members.contains(q))), run)
    }

    /// `run_nested`, with `lazy_of` naming each member that is the lazy build (D824) of a member probe: that probe (`None`:
    /// no member is one).
    fn run_nested_with(&mut self, exec: &mut Exec, binding: &mut Binding<'_>, members: &[u16], lazy_of: Option<&dyn Fn(&Program, u16) -> Option<u16>>, run: &mut dyn FnMut(&mut Entry, &mut Exec, &mut Binding<'_>, usize) -> Result<()>) -> Result<()> {
        exec.node.rerun_without_reset(members)?;
        // D1700 (D1654): a member that is the lazy build (D824) of a member probe, or a producer fed into it (D1005), runs
        // only at the probe's first row (`build_wait`), as the top level's `skip_lazy`: PG's rescan of a LEFT / ANTI hash
        // join fetches the outer's first row before it builds, every time (HJ_FILL_OUTER; lowerv4 declares no rerun set
        // over a cost-path lazy build, D1701). Its slot is pending before the probe opens: opened so, or the kept slot
        // `rerun_nested` reset in place adopted so (the build ran first, then again at the probe's wait, over its own table).
        // D2086: the program's Arc is cloned (two atomic counts per nested run) only when a member is a lazy build here
        let lazy_in = |p: &Program, m: u16| lazy_of.is_some_and(|f| f(p, m).is_some());
        let program = (lazy_of.is_some() && members.iter().any(|&m| lazy_in(exec.node.program(), m))).then(|| exec.node.program().clone());
        let lazy = |m: &u16| program.as_deref().is_some_and(|p| lazy_in(p, *m));
        if let Some(program) = program.as_deref() {
            for &b in members.iter().filter(|&m| lazy_in(program, *m)) {
                let Some(q) = lazy_of.and_then(|f| f(program, b)) else { continue };
                let (b, q) = (b as usize, q as usize);
                if program.pipelines[b].produces().is_some_and(|s| program.producer_of_slot(q, s) == Some(b)) {
                    let split = exec.node.share(b, exec.grant(), 1);
                    exec.node.open_pending(b, split.breaker, split.images.bytes)?;
                }
            }
        }
        let (nest, resume_next, state) = (exec.node.nest(), exec.resume_next, exec.state);
        let rows = OutRows::hold(exec);
        let r = self.out_of_order(exec, binding, &mut members.iter().filter(|&m| !lazy(m)).map(|&m| m as usize), run);
        let back = rows.put_back(exec);
        exec.node.unnest(nest);
        (exec.resume_next, exec.state) = (resume_next, state);
        r.and(back)
    }

    /// D1035 step 2 (V4R-444): an Init's `members` (program order) run now, ahead of the next pipeline `q` (an eager
    /// Init's reader), at its start: `q` is not opened and no call is on the stack, so the members run as any producer does
    /// (the caller's `run`, sections at their widths, drawn on the statement's grant), with no nested lane and nothing to
    /// hold; a cancel is checked after each. `q` is the next pipeline again after it, Ok or Err; an error is the caller's.
    pub fn run_ahead(&mut self, exec: &mut Exec, binding: &mut Binding<'_>, members: &[u16], run: &mut dyn FnMut(&mut Entry, &mut Exec, &mut Binding<'_>, usize) -> Result<()>) -> Result<()> {
        let q = exec.next_pipeline;
        if exec.node.is_opened(q) {
            return Err(Error::Refused(format!("gravity: Bug: an Init run ahead of pipeline {q}, already opened (D1035 step 2)").into())); // alloc: Cold
        }
        exec.node.rerun_without_reset(members)?;
        let (resume_next, state) = (exec.resume_next, exec.state);
        exec.state = ExecState::Running;
        let _ahead = crate::ahead_trace::scope(crate::ahead_trace::Origin::Ahead);
        let mut r = Ok(());
        for &m in members {
            exec.next_pipeline = m as usize;
            r = run(self, exec, binding, m as usize).and_then(|()| if gravity_abi::ctx::abort_set(binding.abort_flag) { Err(Error::Cancelled) } else { Ok(()) });
            if r.is_err() {
                break;
            }
        }
        exec.next_pipeline = q;
        (exec.resume_next, exec.state) = (resume_next, state);
        r
    }

    /// M8 (RT-2e): `members` (program order) of one runner the program declares (gravity_program `lifetime`: a rerun set, a
    /// re-armed InitPlan's cell, a re-armed LAZY hashed set) run again, nested inside the caller's call as `run_nested`: each
    /// member's slot, kept for the statement (`skip_rerun`), is reset in place first (its region, life and grant kept: no
    /// region or grant drawn after the set's first run; its header zeroed; its spill life restarted), so the member's open
    /// adopts it instead of refusing a live slot produced again (L3). A slot of a later member that is no member keeps its
    /// rows. The first run opens a member's slot as any open does. A member the program does not declare, or members of two
    /// runners, are a Bug by name before anything is reset or run.
    pub fn rerun_nested(&mut self, exec: &mut Exec, binding: &mut Binding<'_>, members: &[u16], run: &mut dyn FnMut(&mut Entry, &mut Exec, &mut Binding<'_>, usize) -> Result<()>) -> Result<()> {
        self.rerun_nested_as(exec, binding, members, false, run)
    }

    /// D4120: `rerun_nested` of a set whose kept members fill again (an enclosing owner changed a param their fill reads,
    /// PG's ExecReScanAgg rebuilding a kept table): a kept member's produced slot is reset in place as every member's.
    pub fn refill_nested(&mut self, exec: &mut Exec, binding: &mut Binding<'_>, members: &[u16], run: &mut dyn FnMut(&mut Entry, &mut Exec, &mut Binding<'_>, usize) -> Result<()>) -> Result<()> {
        self.rerun_nested_as(exec, binding, members, true, run)
    }

    fn rerun_nested_as(&mut self, exec: &mut Exec, binding: &mut Binding<'_>, members: &[u16], refill: bool, run: &mut dyn FnMut(&mut Entry, &mut Exec, &mut Binding<'_>, usize) -> Result<()>) -> Result<()> {
        // one look at each member's declaration: one runner's, and whether one is kept or a member probe's lazy build
        let needs = match exec.node.program().v4.as_deref().and_then(|d| d.lifetimes.run_of(members)) {
            Some(needs) => needs,
            None if members.is_empty() => gravity_program::lifetime::RunNeeds::default(),
            None => return Err(Error::Refused(format!("gravity: Bug: a run again of pipelines {members:?} the program's lifetimes do not declare as one runner's (gravity_program lifetime)").into())), // alloc: Cold
        };
        // D1339 row C (ported, proto4): the set's last pins unpinned, this run's kept past the owner's reattach (`Node::rerun_keep_begin`)
        let keep = exec.node.rerun_keep_begin(members.first().copied().unwrap_or(u16::MAX));
        for &m in members {
            let kept = |p: &Program| p.v4.as_deref().and_then(|d| d.lifetimes.member(m)).is_some_and(|x| x.life == gravity_program::lifetime::RunLife::FirstRun);
            let (product, kept) = (exec.node.program().pipelines.get(m as usize).map(|x| x.produces()), needs.kept && kept(exec.node.program()));
            match product {
                // a member PG keeps (lifetime::RunLife::FirstRun) is reset only before a run produced it: the set's first run
                // adopts its slot; after that its open over the live slot refuses by name (`Node::rerun_without_reset`, L3),
                // unless the run refills it (D4120)
                Some(Some(slot)) if kept && !refill && !exec.node.is_rerun_unrun(slot) => {}
                // D3515: every member's slot is kept for the statement, so its reader leaves it and each run resets it here.
                // `skip_rerun` keeps the members it steps past; a lazy build's feeder (D1005) steps past with its build
                // (`skip_lazy`), unkept: its reset was a no-op, the member stayed opened, and its next claim ran over the
                // region the waiting probe's pipeline pointed at (a grouped side's phase-1 table: 'phase-1 table shape')
                Some(Some(slot)) => {
                    exec.node.keep_member(slot);
                    exec.node.reset_in_place(slot);
                }
                // D1262: an InitPlan's cell end produces no slot: it opens again as a slot's producers do
                Some(None) => exec.node.reopen(m as usize),
                None => {}
            }
            // CAND-M03: a member an earlier run's empty build masked (D276, `run_empty`) runs again with its claims; the
            // mask is that run's (PG skips the outer of an empty build per rescan), never the statement's
            exec.unmask(m as usize);
        }
        // D1902 (errorhold): the call's held errors are its own (a member build's, raised by a member probe at the call's
        // first row, PG's rebuild per rescan); one an earlier call held for a member's region, reset above, is dropped
        exec.node.spill.call_begin(&|b| members.iter().any(|&m| m as usize == b));
        // a member lazy build runs at its declared probe's first row, at every run (D1700)
        let lazy_of = |p: &Program, m: u16| p.v4.as_deref().and_then(|d| d.lifetimes.member(m)).and_then(|x| x.lazy_probe()).filter(|q| members.contains(q));
        let _demand = crate::ahead_trace::demand(crate::ahead_trace::Origin::Rerun, exec.node.program(), members.last().map_or(usize::MAX, |&m| m as usize));
        let r = self.run_nested_with(exec, binding, members, needs.lazy.then_some(&lazy_of as &dyn Fn(&Program, u16) -> Option<u16>), run);
        exec.node.spill.call_end();
        exec.node.rerun_keep_end(keep);
        exec.node.rerun_done(members);
        r
    }

    /// M8 (RT-2f): a pulled set's `members` run on, nested inside the reader's call as `run_nested` (the reader held, a lane
    /// of its own, the reader's state put back after): each producer (every member but the last) not yet run runs whole by
    /// the caller's arm (`run`), once per execution; the last member, the pulled pipeline, is called on the leader at width
    /// 1 until it stops resumably (`Step::Suspended`: its end took its budget) or ends. A stop parks its call (resume word,
    /// claim, live sites: `Node::park`) outside every hold, and the next pull resumes it where it stopped; its end is sticky
    /// (`Pulled::Done` again, nothing run). An error is this pull's (raised by the reader's host), and every later pull of
    /// the set is an error by name; a cancel drops the parked call with it. The pulled pipeline ends in a breaker (its store:
    /// the CTE's rows), a slot its readers consume: the runner steps it past as a kept slot (`skip_rerun`, its reader the
    /// owner), so the slot is live, zeroed, at the reader's open and adopted by the first pull; its end runs no merge
    /// finish (a width-1 append the reader reads as it stands).
    pub fn pull_nested(&mut self, exec: &mut Exec, binding: &mut Binding<'_>, members: &[u16], run: &mut dyn FnMut(&mut Entry, &mut Exec, &mut Binding<'_>, usize) -> Result<()>) -> Result<Pulled> {
        let Some((&last, producers)) = members.split_last() else { return Err(Error::Refused("gravity: Bug: a pull of no member (M8 RT-2f)".into())) };
        if exec.node.program().pipelines.get(last as usize).is_none_or(|x| crate::pull::is_emit(exec.node.program(), last as usize) || x.produces().is_none()) {
            return Err(Error::Refused(format!("gravity: Bug: pulled pipeline {last} does not end in a breaker (its store; M8 RT-2f)").into())); // alloc: Cold
        }
        match exec.node.pull_phase(last) {
            1 => return Ok(Pulled::Done),
            2 => return Err(Error::Refused(format!("gravity: a pulled set (pipeline {last}) read again after its error (M8 RT-2f)").into())), // alloc: Cold
            _ => {}
        }
        // D2605: a data-modifying CTE's writer binds at its set's first pull (PG's fireBSTriggers at its ModifyTable's first
        // call) and queues its AFTER STATEMENT events once the set ran to its end (node/ends.rs)
        exec.node.bind_at(&binding.as_ref(), last)?;
        let (nest, resume_next, state) = (exec.node.nest(), exec.resume_next, exec.state);
        let rows = OutRows::hold(exec);
        let q = exec.next_pipeline;
        let _demand = crate::ahead_trace::demand(crate::ahead_trace::Origin::Pull, exec.node.program(), last as usize);
        let r = match exec.node.nested_lane() {
            Err(e) => Err(e),
            Ok(outer) => {
                let h = exec.node.is_opened(q).then(|| exec.node.hold(q));
                exec.state = ExecState::Running;
                let program = exec.node.program().clone();
                let lazy = |m: u16| u16::try_from(program.pipelines[m as usize].lazy_for).is_ok_and(|q| members.contains(&q));
                let mut r: Result<Pulled> = 'pull: {
                    for &m in producers {
                        if exec.node.pull_ran(last, m) {
                            continue;
                        }
                        // D824 in a pulled set (a CTE's): a member that is the lazy build of a member probe, or a producer fed
                        // into it (D1005), runs only at the probe's first row (`build_wait`), as `run_nested`'s: its slot
                        // opens pending once, before the probe opens. PG's outer-first hash join under a CTE Scan builds
                        // only when its outer has a row
                        if lazy(m) {
                            let q = program.pipelines[m as usize].lazy_for as usize;
                            if program.pipelines[m as usize].produces().is_some_and(|s| program.producer_of_slot(q, s) == Some(m as usize)) {
                                let split = exec.node.share(m as usize, exec.grant(), 1);
                                if let Err(e) = exec.node.open_pending(m as usize, split.breaker, split.images.bytes) {
                                    break 'pull Err(e);
                                }
                            }
                            exec.node.mark_pull_ran(last, m);
                            continue;
                        }
                        exec.next_pipeline = m as usize;
                        if let Err(e) = run(self, exec, binding, m as usize) {
                            crate::note_ahead(exec.node.program(), m as usize, &e);
                            break 'pull Err(e);
                        }
                        if gravity_abi::ctx::abort_set(binding.abort_flag) {
                            break 'pull Err(Error::Cancelled);
                        }
                        exec.node.mark_pull_ran(last, m);
                        exec.state = ExecState::Running;
                    }
                    exec.next_pipeline = last as usize;
                    exec.resume_next = match exec.node.unpark_pull(last) {
                        Ok(parked) => parked,
                        Err(e) => break 'pull Err(e),
                    };
                    let out_rows = exec.node.program().layout.out_rows;
                    let mut call = Call::batch(out_rows);
                    loop {
                        match self.run_pipeline(exec, binding, call) {
                            Ok(Step::Done) => break Ok(Pulled::Done),
                            Ok(Step::Suspended { .. }) => {
                                exec.node.park_pull(last);
                                break Ok(Pulled::Stopped);
                            }
                            // D824: the probe's first row reached its lazy build: the build runs, the call resumes at its row
                            Ok(Step::BuildWait { pipeline }) => match self.build_wait(exec, binding, pipeline, run) {
                                Ok(()) => call = Call::resume(out_rows),
                                Err(e) => {
                                    crate::note_ahead(exec.node.program(), pipeline, &e);
                                    break Err(e);
                                }
                            },
                            Ok(s) => break Err(Error::Refused(format!("gravity: Bug: pulled pipeline {last} stopped with {s:?} (M8 RT-2f)").into())), // alloc: Cold
                            Err(e) => {
                                crate::note_ahead(exec.node.program(), last as usize, &e);
                                break Err(e);
                            }
                        }
                    }
                };
                match &r {
                    Ok(Pulled::Done) => exec.node.pull_over(last, true),
                    Ok(Pulled::Stopped) => {}
                    Err(_) => exec.node.pull_over(last, false),
                }
                if let Some(h) = h {
                    // D2344 (b): a parked member's scratch is its resume's (agg_gather's pointer array): kept past the reader
                    // setop_rec-0043: the pins it keeps past the reader filed as the pull's (a re-arm unpins them)
                    let back = exec.node.reattach_pull(q, h, matches!(r, Ok(Pulled::Stopped)), last);
                    r = r.and_then(|pulled| back.map(|()| pulled));
                }
                exec.node.outer_lane(outer);
                r
            }
        };
        let back = rows.put_back(exec);
        exec.node.unnest(nest);
        exec.next_pipeline = q;
        (exec.resume_next, exec.state) = (resume_next, state);
        let r = r.and_then(|p| back.map(|()| p));
        match r {
            Ok(Pulled::Done) => exec.node.end_at(last).map(|()| Pulled::Done),
            r => r,
        }
    }

    /// setop_rec-0043 (PG's ExecReScanCteScan: tuplestore_clear, the CTE's plan re-run lazily at the next read): pulled set
    /// `members` (program order, the pulled pipeline last) re-armed by the run of a rerun set holding them: a parked call is
    /// dropped (its resume never runs), the producers that ran count as not run, and every member's kept slot is reset in
    /// place (`Node::reset_in_place`: its region, life and grant kept), so the next pull runs the producers again, each
    /// adopting its slot, and calls the pulled pipeline from its start. Its store's words are the owner run's (lowerv4
    /// ops/ctescan/lower.rs `eager_resets`).
    pub fn rearm_pulled(&mut self, exec: &mut Exec, members: &[u16]) -> Result<()> {
        let Some(&last) = members.last() else { return Ok(()) };
        exec.node.pull_rearm(last, members);
        // D3670: a recursive union's term set re-armed at a generation's first pull: the pulled pipeline (never a producer
        // run, `count_scan`'s other caller) runs again from its start, as PG's ExecRecursiveUnion rescans its term
        exec.node.count_scan(last as usize);
        for &m in members {
            if let Some(Some(slot)) = exec.node.program().pipelines.get(m as usize).map(|x| x.produces()) {
                exec.node.reset_in_place(slot);
            }
        }
        Ok(())
    }

    /// M8 (RT-2e): the next pipeline is a rerun set's member (the runner's plan: `Live::deferred`, owned by `owner`, the
    /// pipeline that calls `m8_rerun`): stepped past unopened as `skip_deferred`, its slot kept for the statement and opened
    /// zeroed now when `owner` reads it (`Node::defer_rerun`); `rerun_nested` runs it at each rerun, or nothing does.
    pub fn skip_rerun(&mut self, exec: &mut Exec, owner: usize) -> Result<()> {
        self.skip_kept(exec, &[owner], true)
    }

    /// D2701: the slots the emits (the last pipelines, `rewound_emits`) read stay live for the statement, as a rerun
    /// member's kept slots do, so that `rewind_emit` reads them again.
    pub fn keep_for_rewind(&mut self, exec: &mut Exec) {
        for p in rewound_emits(exec.node.program()) {
            exec.node.keep_consumed(p);
        }
    }

    /// D2701: a live execution's emits run again from their first row over what the pipelines before them filled
    /// (`keep_for_rewind`), as PostgreSQL's rescan from the top re-reads a Sort's or a Material's kept rows: the emits'
    /// hosts let go of what the stopped call held, and each opens again at its next call (claims, sites, resume word).
    /// D3377: an Append's arms streamed to the sink are emits too, run again in pipeline (arm) order as ExecReScanAppend
    /// starts over at its first arm; they are the program's last pipelines (lowerv4 effects/visible.rs `soft_rewind`), so
    /// no fill runs again. The D2584 marks of the arms an exec pruning removed are the runner's to set again (exec
    /// `Producers::prune_emits`).
    pub fn rewind_emit(&mut self, exec: &mut Exec) -> Result<()> {
        let program = exec.node.program();
        let emits = rewound_emits(program);
        if emits.is_empty() || (0..emits.start).any(|q| crate::pull::is_emit(program, q)) {
            return Err(Error::Refused("gravity: Bug: a rewind of emits that are not the program's last pipelines (D2701)".into()));
        }
        exec.node.release_hosts();
        for p in emits.clone() {
            exec.node.reopen(p);
            exec.unmask(p);
        }
        exec.next_pipeline = emits.start;
        exec.resume_next = false;
        exec.state = ExecState::Running;
        Ok(())
    }

    /// M8 (RT-2f): the next pipeline is a pulled set's last member (a CTE's plan, deferred): stepped past as `skip_rerun`,
    /// its slot (the CTE's store edge) kept for the statement and opened zeroed now for its `readers` (every CteScan of the
    /// CTE, each pulling before it reads: PG shares one tuplestore among them), who alone may open over it before its first
    /// pull.
    pub fn skip_pulled(&mut self, exec: &mut Exec, readers: &[usize]) -> Result<()> {
        self.skip_kept(exec, readers, false)
    }

    /// setop_rec-0043: the next pipeline is a CTE's plan a rerun set re-arms (its member, owned by `owner`, pulled by
    /// `readers`): stepped past as `skip_rerun`, its store kept for the statement (each re-arm resets it in place,
    /// `rearm_pulled`) and opened zeroed now for the owner and every reader, each of which may open over it before the CTE's
    /// next pull fills it (the reader pulls before it reads, as `skip_pulled`'s).
    pub fn skip_rearmed(&mut self, exec: &mut Exec, owner: usize, readers: &[usize]) -> Result<()> {
        let owners: Vec<usize> = core::iter::once(owner).chain(readers.iter().copied()).collect(); // alloc: Exec
        self.skip_kept(exec, &owners, true)
    }

    fn skip_kept(&mut self, exec: &mut Exec, owners: &[usize], every: bool) -> Result<()> {
        let p = exec.next_pipeline;
        let split = exec.node.share(p, exec.grant(), 1);
        if let Err(e) = exec.node.defer_rerun(p, owners, every, split.breaker, split.images.bytes) {
            exec.state = ExecState::Failed;
            return Err(e);
        }
        exec.next_pipeline += 1;
        exec.state = ExecState::Running;
        Ok(())
    }

    /// M8 (RT-1): the next pipeline runs only on demand (the runner's plan says so: an Init's member, `Live::deferred`): the
    /// runner steps past it unopened; `run_nested` runs it at the first read, or nothing does.
    pub fn skip_deferred(&mut self, exec: &mut Exec) {
        exec.node.defer(exec.next_pipeline);
        exec.next_pipeline += 1;
        exec.state = ExecState::Running;
    }

    /// The width-1 arm's owner fold (partitioned-record.md §3.2): the leader's own phase-1 table
    /// and its spill file folded a partition at a time, on this thread, into a region of the
    /// leader's arena sized from the records it holds (never below the shape's floor), both rounds;
    /// the slot region carries a one-owner header pointing at it.
    pub fn own_single(&mut self, exec: &mut Exec, merge: &dyn Merge, pbits: u8) -> Result<()> {
        let p = exec.next_pipeline;
        self.own_single_of(exec, p, merge, pbits)
    }

    /// `own_single` of pipeline `p`: a site-less producer's `run_pipeline` has already advanced past it on `Done` (D109).
    pub fn own_single_of(&mut self, exec: &mut Exec, p: usize, merge: &dyn Merge, pbits: u8) -> Result<()> {
        let Some(slot) = exec.node.program().pipelines[p].produces() else { return Ok(()) };
        // D393b: once the stream spilled its files are at the stream's partition count; an arm run's last arm may hold an
        // empty, unlaid region whose own rule differs
        let pbits = match exec.node.run_stream_bits(p) {
            0 => pbits,
            b => b,
        };
        // D273: a pipeline no claim reached (every granule pruned, or cut by pass 1's marks) opens its slot here, zeroed, as
        // `end_claims` would after the fold: the one-owner header needs the region, and the consumer then reads an Owned
        // region of zero records, never the unowned zeroed slot
        Self::open_unclaimed(exec, p)?;
        // a two-round owner at its floor spills round 2 into the one owner's round-2 file
        exec.node.spill.provision(1);
        // C10 2b (D3582): a spilled one-round group past the statement's room, read by a refilling emit, folds one batch
        // of its partitions at a time (mem/batch.rs)
        if Self::own_batched_of(exec, p, merge, slot, pbits)? {
            return Ok(());
        }
        let (src, src_len) = {
            let r = exec.node.slot_region(slot);
            (r.as_ptr() as *const u8, r.len())
        };
        let partial = Partial { ordinal: 0, bytes: raw::slot_bytes(src, src_len), rows_processed: 0 };
        let ctx = exec.node.ctx();
        // the pipeline's driving site renders (a two-pass form's second pass runs over site 1)
        let site = raw::site_of(ctx, exec.node.program().pipelines[p].driving_site.max(0) as usize);
        // road dict.entries' runs (hosts granule_source/entry_runs.rs): the parts this source published as resident runs, after the files
        let runs = usize::try_from(exec.node.program().pipelines[p].driving_site).ok().and_then(|i| raw::source_of(ctx, i)).and_then(|s| s.owner_input(pbits));
        // D3075: one array, filled in place (a rescanned owner phase built two of 66 entries each)
        let mut files: [&dyn crate::SpillFile; 66] = [&crate::section::NO_FILE; 66];
        let mut nf = 0usize;
        for f in exec.node.spill.stream(slot).files.iter().flatten().map(|f| f.as_ref()).chain(runs) {
            if nf < files.len() {
                files[nf] = f;
                nf += 1;
            }
        }
        let gate = || exec.node.owner_refuses(Some(slot));
        let region = crate::section::own_all(merge, ctx, &exec.node.spill, Some(slot), &[partial], &files[..nf], site, pbits, &mut [], &gate, exec.node.owner_give_floor())?;
        let dst = exec.node.slot_region(slot);
        crate::section::owned_header(dst, region)
    }

    /// C10 2b (D3582): the width-1 arm's batched road, when producer `p`'s slot takes it (`mem::batch::fold_for`): the
    /// leader's phase-1 table scattered to the slot's files (`spill_leader`), then `mem::batch::lay` (the first batch
    /// folded, the slot's region its one owner's header). `false`: the one owner over every partition, as before.
    fn own_batched_of(exec: &mut Exec, p: usize, merge: &dyn Merge, slot: u8, pbits: u8) -> Result<bool> {
        let node = &mut exec.node;
        let ctx = node.ctx();
        let driving = node.program().pipelines[p].driving_site;
        // D393b: an arm run's last arm folds every arm's records; road dict.entries' runs are its source's, read in place
        if node.in_arm_run(p) || usize::try_from(driving).ok().and_then(|i| raw::source_of(ctx, i)).is_some_and(|s| s.has_owner_input()) {
            return Ok(false);
        }
        let room = crate::mem::batch::room(ctx);
        let fold = {
            let r = node.slot_region(slot);
            let partial = Partial { ordinal: 0, bytes: raw::slot_bytes(r.as_ptr(), r.len()), rows_processed: 0 };
            let (held, rec) = crate::mem::batch::held_of(&[partial], node.spill.stream(slot));
            crate::mem::batch::fold_for(node.program(), p, slot, node.spill.stream(slot), merge, &[partial], held, rec, room, 0, &mut || crate::section::kept_run(merge))
        };
        let Some(fold) = fold else { return Ok(false) };
        node.spill_leader(p)?;
        let pbits = match node.spill.stream(slot).bits0() {
            0 => pbits,
            b => b,
        };
        let site = (!node.program().sites.is_empty()).then_some(driving.max(0) as usize);
        let leader = raw::slot_bytes_mut(node.slot_region(slot));
        let budget = crate::mem::batch::budget(ctx, node.program(), p);
        let spill: *mut crate::mem::spill::SpillTable = &mut *node.spill;
        crate::mem::batch::lay(ctx, spill, slot, fold, pbits, budget, leader, site)?;
        Ok(true)
    }

    /// The width-1 arm's own partial does not fit its consumer's table (the merge's `finish`
    /// refused): scatter it whole into the leader's file, as a section scatters an oversized
    /// worker partial; the seal then finds the leader's region empty.
    pub fn spill_leader_partial(&mut self, exec: &mut Exec) -> Result<u64> {
        let p = exec.next_pipeline;
        self.spill_leader_partial_of(exec, p)
    }

    pub fn spill_leader_partial_of(&mut self, exec: &mut Exec, p: usize) -> Result<u64> {
        exec.node.spill_leader(p)
    }

    /// D2033 step 3: whether the running EvalPlanQual recheck's epq probe builds first, as PG's join does once a recheck
    /// of the statement set its hj_OuterNotEmpty (`rerun`'s `seen`): a `recheck_hold_epq` build's error is then the
    /// recheck's at once (exec runner `rerun_producer`).
    pub fn rerun_seen(&self) -> bool {
        self.rerun_seen
    }

    /// Op 14 (JOIN's deferred build error; PG's outer-first hash join never runs its build when the outer is empty):
    /// pipeline `p` failed with `e`. When `e` is a statement's error (never a cancel, a recheck or a bad return), the
    /// pipeline's leader region is open (an error before it opens is never deferred) and its merge takes the error
    /// (`Merge::defer_error`: it marks the region failed), the error and the host's stashed error behind it are held on
    /// the node's spill table for the consumer's raise (`program::raise_held`, or dropped with the statement), the pipeline
    /// passes as done (nothing sealed: a failed region is never read past its mark) and the statement runs on: `Ok(())`.
    /// Otherwise `Err(e)`, unchanged: so too for a deferring build that is lazy (`waited`, D4800). A held run's fill (exec
    /// ops/sort/held.rs, D1108's plan Material) holds its error over a lazy fill too: its reader raises it at its record.
    pub fn defer_failed(&mut self, exec: &mut Exec, binding: &mut Binding<'_>, merge: &dyn Merge, p: usize, e: Error) -> Result<()> {
        crate::note_unsupported(Some(p), &e, false);
        crate::note_ahead(exec.node.program(), p, &e);
        if !crate::mem::spill::holdable(&e) || exec.next_pipeline != p {
            return Err(e);
        }
        let Some(slot) = exec.node.program().pipelines[p].produces() else { return Err(e) };
        if exec.node.slot_region(slot).is_empty() || Self::waited(exec, merge, slot) {
            return Err(e);
        }
        if !matches!(merge.defer_error(&mut exec.node.leader_state(slot, binding.sink)), Ok(true)) {
            return Err(e);
        }
        Self::hold_and_pass(exec, p, p, p, e);
        Ok(())
    }

    /// Op 14, fed (V4R-89; regress subselect: a hash join's inner is a HashAgg over a scan that can raise): pipeline `p`
    /// failed with `e` and its rows reach, through pipelines that read nothing else and are read by nothing else, only a
    /// later build `b` whose merge defers (`Merge::defers`: the lowerer sets a build's DEFER bit over feeders only when
    /// they run just before it, feed it alone and call no user function; lowerv4 join/hold.rs). PG's outer-first hash
    /// join runs that build, and so its feeders, only once a row reaches the probe: the error is held as `defer_failed`
    /// holds the build's own, pipelines `p..=b` pass unrun and `b`'s region opens failed (that region alone: `open_held`, D3230:
    /// a feeder after `p` never opened the slot it makes for `b`), so the probe's first row raises
    /// it and an empty outer never does. `Err(e)` unchanged when no such build takes it.
    /// RISK (multi-row, parallel): at width >= 2 with several raising rows the error held is the first participant's to
    /// fail, not necessarily the one PG's serial scan meets first (as for an unfed build's own error); the statement
    /// still fails with that error class and SQLSTATE, and only a message naming the row's value can differ.
    /// D4101 (k2): inside an out-of-order run (a nest's rerun set run per outer row) the run's owner (`Exec::nest_owner`)
    /// lists its members' slots among those it consumes, kept for the statement (D3515), and reads none of their rows: the
    /// walk and the read-by-nothing-past-`b` test pass it by. A top-level run has no owner, and the walk is as before.
    /// D4800: never a lazy build `b` (`waited`): its feeders' error is the wait's, `Err(e)`.
    pub fn defer_fed<'m>(&mut self, exec: &mut Exec, binding: &mut Binding<'_>, merge_for: &dyn Fn(usize) -> &'m dyn Merge, p: usize, e: Error) -> Result<()> {
        crate::note_ahead(exec.node.program(), p, &e);
        let n = exec.node.program().pipelines.len();
        if !crate::mem::spill::holdable(&e) || exec.next_pipeline != p {
            return Err(e);
        }
        // the last pipeline that reads the slot `y` produces before its next producer, the run's owner apart
        let owner = exec.nest_owner;
        let last = |y: usize| {
            let program = exec.node.program();
            match program.pipelines[y].produces() {
                Some(s) if owner != NO_OWNER => (y + 1..n).filter(|&c| c != owner && program.pipelines[c].consumes.contains(&s) && program.producer_of_slot(c, s) == Some(y)).last().unwrap_or(y),
                _ => program.last_consumer(y),
            }
        };
        // the first deferring merge down `p`'s last consumers; every pipeline from `p` to it is read by it at the latest
        let mut x = p;
        let b = loop {
            let c = last(x);
            if c <= x || c >= n || exec.node.program().pipelines[c].produces().is_none() {
                return Err(e);
            }
            if merge_for(c).defers() {
                break c;
            }
            x = c;
        };
        if (p..b).any(|y| last(y) > b) {
            return Err(e);
        }
        let Some(slot) = exec.node.program().pipelines[b].produces() else { return Err(e) };
        if Self::waited(exec, merge_for(b), slot) {
            return Err(e);
        }
        // D1806: the slots whose last reader is one of `p..b` free before `b` opens its own, which may reuse one's number
        // (a failed `p` never reached its `free_after`; opening first trips L3)
        for y in p..b {
            exec.node.free_after(y);
        }
        exec.next_pipeline = b;
        Self::open_held(exec, b)?;
        if !matches!(merge_for(b).defer_error(&mut exec.node.leader_state(slot, binding.sink)), Ok(true)) {
            return Err(e);
        }
        Self::hold_and_pass(exec, p, b, b, e);
        Ok(())
    }

    /// D4800 (op 14 over D824's lazy build): build slot `slot`, whose merge is op 14's deferring variant (`Merge::defers`,
    /// JOIN's DEFER bit), is a lazy build's that has not run (`Node::is_pending`), so its build runs now, in its reader's
    /// `build_wait`, because a row reached that reader: where PG's outer-first join builds and raises. Its probe takes no
    /// deferral over a lazy build (lowerv4 hashjoin probe.rs: `defer && !lazy`, as D2756 found for a recursive union's), so
    /// a held error was never raised: the build read as failed-empty and the statement returned rows PG never returns (a
    /// keyed nest's raising inner under road nest.key.eq: (0, 0) for a count over a join where PG raises). Not held: the
    /// error is the wait's, raised there. A held run's fill (exec ops/sort/held.rs `HeldRows` / `HeldAt`, whose merge
    /// defers no feeder) is not one: D1108's plan Material run whole at the nest's first outer row is a lazy fill too, and
    /// its reader raises the held error at the record the fill stopped at (kernels `m11_run_held`), past the rows PG hands
    /// on before it (a cursor's first FETCH, a LIMIT that stops short).
    fn waited(exec: &Exec, merge: &dyn Merge, slot: u8) -> bool {
        merge.defers() && exec.node.is_pending(slot)
    }

    /// Op 14: `e` (and the host's stashed error behind it) held for the consumer's raise, pipeline `p`'s driving site
    /// closed, and pipelines `p..=b` passed as done (`free_after` from `freed` on: the caller ran it for those before).
    fn hold_and_pass(exec: &mut Exec, p: usize, b: usize, freed: usize, e: Error) {
        let payload = crate::mem::spill::HELD_HOOKS.get().and_then(|h| (h.take)());
        exec.node.spill.hold_for(crate::mem::spill::HeldError { err: e, payload }, b);
        // the failed run's driving site left open mid-claim closes (a later pipeline's open finds it clean: L8). D1018
        // (hashjoin, regress join#346 / #347): never a site a held call keeps (`Node::kept_site`, as an open's sweep skips
        // it): a member with no driving site (a Result's build) failing inside a nested rerun call closed the owner's,
        // open mid-claim, and the owner's scan started over at every call (an endless statement)
        let (nsites, d) = (exec.node.program().sites.len(), exec.node.program().pipelines[p].driving_site);
        let ctx = exec.node.ctx();
        for i in (if d >= 0 { d as usize..(d as usize + 1).min(nsites) } else { 0..nsites }).filter(|&i| !exec.node.kept_site(i)) {
            // `i` is one of the program's sites, laid out in the Node's ctx; the leader thread, between pipelines
            let s = raw::site_mut(ctx, i);
            (s.state, s.cont) = (gravity_abi::site::site::STATE_CLOSED, 0);
        }
        let last = b + 1 >= exec.node.program().pipelines.len();
        for y in p..=b {
            if y >= freed {
                exec.node.free_after(y);
            }
            exec.node.mark_opened(y);
        }
        exec.next_pipeline = b + 1;
        exec.state = if last { ExecState::Done } else { ExecState::Running };
    }

    /// After the last claim of a claim-driven pipeline: the pipeline is done, its remainder
    /// sealed into the spill files when the execution spilled, its slots settled.
    pub fn end_claims(&mut self, exec: &mut Exec) -> Result<()> {
        let p = exec.next_pipeline;
        let last = p + 1 >= exec.node.program().pipelines.len();
        // no claim at all (an empty relation): the pipeline still opens its slot, zeroed, so its
        // consumer finds the empty partial it would have found after a claim
        Self::open_unclaimed(exec, p)?;
        if let Err(e) = exec.node.end_production(p) {
            exec.state = ExecState::Failed;
            return Err(e);
        }
        exec.node.ended_site_words(p);
        exec.node.free_after(p);
        exec.next_pipeline += 1;
        exec.state = if last { ExecState::Done } else { ExecState::Running };
        Ok(())
    }

    /// D3918: the next pipeline is a later arm of a run that ended at its first arm (`Node::arm_ended`: an empty pair
    /// pass): stepped past unopened, as `end_claims` ends a pipeline, but with no open, no finish and no seal (the run's first
    /// arm sealed the slot; this arm's open or finish would write over it). `true`: stepped past.
    pub fn skip_ended(&mut self, exec: &mut Exec) -> Result<bool> {
        let p = exec.next_pipeline;
        if !exec.node.arm_ended(p) {
            return Ok(false);
        }
        exec.node.ended_site_words(p);
        exec.node.free_after(p);
        exec.next_pipeline += 1;
        exec.state = if p + 1 >= exec.node.program().pipelines.len() { ExecState::Done } else { ExecState::Running };
        Ok(true)
    }

    /// D292: a width-1 producer's slot opened (zeroed, at the width-1 share) before its merge's owner fold and finish, when no
    /// claim reached it (an empty relation, a `run_empty` mask): a finish that grows and writes the leader's header writes the
    /// region the consumer reads, never an unopened slot's empty view that `end_claims` would then open over.
    pub fn open_unclaimed_of(&mut self, exec: &mut Exec, p: usize) -> Result<()> {
        Self::open_unclaimed(exec, p)
    }

    /// D3230 (op 14, fed): deferring build `b`, whose feeders `defer_fed` passes unrun, opens its slot, zeroed, at the
    /// width-1 share, for the FAILED stamp alone (`Node::open_produced`): it never runs, so it binds none of the slots it
    /// consumes. A feeder after the failed one never ran: the slot it produces for `b` never opened, and binding it failed
    /// 'pipeline b consumes slot s that is not live' (a panic that restarted the server: an INTERSECT under an outer-first
    /// Hash Semi Join, its first arm's held build error raised by the arm's probe).
    fn open_held(exec: &mut Exec, b: usize) -> Result<()> {
        if !exec.node.is_opened(b) {
            let split = exec.node.share(b, exec.grant(), 1);
            if let Err(e) = exec.node.open_produced(b, split.breaker, split.images.bytes) {
                exec.state = ExecState::Failed;
                return Err(e);
            }
            exec.node.mark_opened(b);
        }
        Ok(())
    }

    /// A pipeline no claim reached opens its slot, zeroed, at the width-1 share (once: `end_claims` and the owner fold).
    fn open_unclaimed(exec: &mut Exec, p: usize) -> Result<()> {
        if !exec.node.is_opened(p) {
            let split = exec.node.share(p, exec.grant(), 1);
            if let Err(e) = exec.node.open_pipeline(p, split.breaker, split.images.bytes) {
                exec.state = ExecState::Failed;
                return Err(e);
            }
            exec.node.mark_opened(p);
        }
        Ok(())
    }

    /// The leader's own partial of the pipeline in flight (the produced slot's region).
    pub fn leader_partial<'a>(&self, exec: &'a Exec) -> Partial<'a> {
        self.leader_partial_of(exec, exec.next_pipeline)
    }

    /// The owner fold's partition count for pipeline `p`'s single owner: a spilled stream's own (D393b: an arm run's earlier
    /// arms scattered their records into files at it, and the last arm's region may be empty and unlaid), else the leader
    /// partial's; `None`: the merge does not partition and nothing spilled.
    pub fn owner_bits_of(&self, exec: &Exec, p: usize, merge: &dyn Merge) -> Option<u8> {
        match exec.node.run_stream_bits(p) {
            0 => merge.partition_bits(&[self.leader_partial_of(exec, p)]),
            b => Some(b),
        }
    }

    /// D3140: the record bytes pipeline `p`'s owners fold: its slot's phase-1 table's and its stream's files'.
    pub fn owner_held_of(&self, exec: &Exec, p: usize) -> u64 {
        exec.node.program().pipelines[p].produces().map_or(0, |slot| exec.node.owner_held(slot))
    }

    pub fn leader_partial_of<'a>(&self, exec: &'a Exec, p: usize) -> Partial<'a> {
        let bytes = match exec.node.program().pipelines[p].produces() {
            Some(slot) => &*exec.node.slot_region(slot),
            None => &[],
        };
        Partial { ordinal: 0, bytes, rows_processed: 0 }
    }

    pub fn run_to_end(&mut self, exec: &mut Exec, binding: &mut Binding<'_>) -> Result<u64> {
        let cap = exec.node.program().layout.out_rows;
        let mut call = Call::batch(cap);
        loop {
            match self.run(exec, binding, call)? {
                Step::Done => return Ok(exec.calls),
                Step::Suspended { .. } => call = Call::resume(cap),
                Step::Recheck { site } => {
                    exec.state = ExecState::Failed;
                    return Err(Error::Recheck(site));
                }
                Step::BuildWait { pipeline } => {
                    exec.state = ExecState::Failed;
                    return Err(Error::BuildWait(pipeline));
                }
            }
        }
    }

    pub fn end(&mut self, exec: Exec, ok: bool) {
        if self.recheck_instr == exec.node.instr_ptr() {
            self.recheck_instr = core::ptr::null_mut();
        }
        if ok {
            self.stats.ended_ok += 1;
        } else {
            self.stats.ended_failed += 1;
        }
        exec.node.finish_feedback(ok);
        self.release(exec.key, exec.node);
    }

    /// EPQ: a second node of the key, every site substituted by the caller
    /// (the rechecked site with the newest version's position, the others
    /// held at theirs), one call under the same Binding parts (same
    /// snapshot). `Pass` when a row came out or a write happened; the
    /// second value is the rows the recheck node processed (pushed or written).
    /// `copies`: the stopped node's positions (`Node::positions`), whose COPY mark sites a recheck without producers
    /// flags `POS_COPY` (mergecmd 9, D732's copy leg on the stopped pipeline's own scan).
    pub fn recheck(
        &mut self,
        key: Option<PlanKey>,
        program: &Arc<Program>,
        derive: &mut dyn Derive,
        binding: &Binding<'_>,
        substitute: &mut dyn FnMut(usize, &mut dyn Source),
        out: RowBuf<'_>,
        pending: Option<&mut Pending>,
        rerun: Option<Rerun<'_>>,
        copies: &[ScanPos],
    ) -> Result<(Recheck, u64)> {
        self.stats.rechecks += 1;
        let stopped_instr = core::mem::replace(&mut self.recheck_instr, core::ptr::null_mut());
        let mut node = self.acquire(key, program, derive)?;
        for i in 0..program.sites.len() {
            if let Some(s) = node.source_mut(i) {
                substitute(i, s);
            }
        }
        // D2744: room for a MERGE recheck's two rows in the stopped pipeline's one call
        let out_cap = if pending.is_some() { 2 } else { 1 };
        let mut cap = Capture { out, got: false, pending };
        let mut b = Binding::new(binding.parts, &mut cap);
        if let Err(e) = node.apply(&mut b) {
            self.release(key, node);
            return Err(e);
        }
        // D604: the recheck node re-evaluates, it never locks again (EvalPlanQual after ExecLockRows' one lock)
        node.for_each_host(|h| {
            if let crate::node::HostRef::Locker(l) = h {
                l.epq(true);
            }
        });
        // D655: a stop past producers (a write or lock over a join) reruns them first on this node, every site substituted
        let stopped = core::mem::take(&mut self.recheck_pipeline);
        let (mut node, called) = match program.pipelines[..stopped].iter().any(|p| p.produces().is_some()) {
            false => {
                // mergecmd 9 (COPYROW): a COPY mark's scan in the stopped pipeline itself (a MERGE nest's VALUES outer, a
                // subquery's copy entry) emits the stopped row's stored datum alone, as `rerun` sets a producer's (D732)
                node.set_copies(Some(copies));
                let called = node.call_pipeline(stopped, Call::batch(out_cap));
                node.set_copies(None);
                (node, called)
            }
            true => self.rerun(key, node, &mut b, stopped, rerun, out_cap),
        };
        node.for_each_host(|h| {
            if let crate::node::HostRef::Locker(l) = h {
                l.epq(false);
            }
        });
        let written = b.rows_processed;
        drop(b);
        // D3500: the counts the statement keeps from its recheck (EXPLAIN ANALYZE's MERGE lines)
        if called.is_ok() {
            node.fold_recheck(stopped_instr);
        }
        let r = match called {
            Ok(Ret::Recheck { site }) => Err(Error::Recheck(site)),
            Ok(_) => Ok((if cap.got || written > 0 { Recheck::Pass } else { Recheck::Skip }, written)),
            Err(e) => Err(e),
        };
        self.release(key, node);
        r
    }

    /// D655: the producers before the stopped pipeline, each run whole by the caller's `rerun` on the recheck node (the
    /// runner's width-1 arm), then one call of the stopped pipeline; an emit before it (a sibling Append arm) is stepped
    /// over. Without `rerun` (a caller with no runner plan) the recheck refuses: the stopped pipeline would probe an
    /// empty product.
    fn rerun(&mut self, key: Option<PlanKey>, node: Box<Node>, b: &mut Binding<'_>, stopped: usize, rerun: Option<Rerun<'_>>, out_cap: u32) -> (Box<Node>, Result<Ret>) {
        let Some(r) = rerun else { return (node, Err(Error::host("gravity: a recheck past producers without the runner's plan (D655)"))) };
        let _demand = crate::ahead_trace::scope(crate::ahead_trace::Origin::Recheck);
        let binding_id = b as *const Binding<'_> as *const ();
        // D2050: the last called recheck of this statement set the stopped pipeline's probe record (PG's flag is set)
        let seen = stopped < 64 && self.epq_outer.as_ref().is_some_and(|(w, mask)| mask >> stopped & 1 != 0 && w.upgrade().is_some_and(|l| std::rc::Rc::ptr_eq(&l, &r.lease)));
        self.rerun_seen = seen;
        let lease = std::rc::Rc::downgrade(&r.lease);
        // its breakers draw from the statement's admission (released with the node: `release` clears it)
        let mut node = node;
        node.set_stmt(&*r.lease);
        // D732: a COPY mark's scan emits the stopped row's stored datum alone
        node.set_copies(Some(r.positions));
        let empty_past = past_words(node.program().pipelines.len());
        let mut exec = Exec { node, key, state: ExecState::Ready, calls: 0, next_pipeline: 0, binding_id, slot_salt: None, lease: r.lease, empty: 0, empty_past, resume_next: false, nest_owner: NO_OWNER };
        let mut ran = Ok(());
        while exec.next_pipeline < stopped && ran.is_ok() {
            let q = exec.next_pipeline;
            if exec.node.program().pipelines[q].produces().is_none() {
                exec.next_pipeline += 1;
                continue;
            }
            // D732-rest: a producer only a COPY-marked subquery reads is never rerun (PG never re-evaluates it): its slot
            // opens empty, and the consumer's copy entry hands the stopped row's stored record in. D2636: the slots it was the
            // last consumer of end their lives as at any producer's end (`free_after`): a later producer of one of those slot
            // numbers (a HashAggregate's group table after the window Sort it read) opens it, never finding it live (L3).
            // D3285: a product no pipeline the recheck runs reads (a LAZY hashed set's member, its probe reading the set's
            // object; an InitPlan's, its reader the cell) is stepped past unopened, as the statement's run steps a deferred
            // member (`skip_deferred`): the set's first probe builds it here, opening the slot then (PG builds a check
            // option's set in the main EState after the recheck), and a set built before the recheck is probed as built
            if exec.node.program().pipelines[q].recheck_skip {
                if read_by_recheck(exec.node.program(), q) {
                    let split = exec.node.share(q, exec.grant(), 1);
                    ran = exec.node.open_pipeline(q, split.breaker, split.images.bytes);
                } else {
                    exec.node.defer(q);
                }
                if ran.is_ok() {
                    exec.node.free_after(q);
                }
                exec.next_pipeline += 1;
                continue;
            }
            // D2033: a lazy build of the stopped pipeline (D824: PG's outer-first hash join fetches the outer's first row before
            // it builds) and the producers fed into it are stepped past, its slot opened pending, as the statement's run steps
            // them: the stopped call runs them at its probe's first row (`BuildWait` below), so a test row that fails the
            // quals below the probe never builds, as PG's recheck. Not once PG builds first (`seen`: the last called recheck
            // set the probe's hj_OuterNotEmpty, D2050 / D2674): then it reruns here, before the call, as PG's build-first join
            // D2033 step 2: a lazy build the recheck keeps in the statement run's order (`recheck_hold`: lowerv4 lockrows
            // rerun.rs `first_held`, the first probe's under the epq probe) is stepped past even then: PG builds it only when
            // its join is asked for a row, after the epq probe's build
            let pipe = &exec.node.program().pipelines[q];
            if (!seen || pipe.recheck_hold) && usize::try_from(pipe.lazy_for).is_ok_and(|l| l == stopped) {
                ran = self.skip_lazy(&mut exec).map(|_| ());
                continue;
            }
            ran = (r.run)(self, &mut exec, b).and_then(|()| match exec.next_pipeline == q {
                true => Err(Error::host("gravity: a recheck producer ran without advancing past its pipeline")),
                false => Ok(()),
            });
        }
        // D2050: from then on PostgreSQL's outer-first INNER / SEMI hash join builds first (ExecReScanHashJoin keeps
        // hj_OuterNotEmpty when the recheck rebuilds its table) and, over an empty build, returns before its outer side runs:
        // the stopped pipeline is not called (no row, `Recheck::Skip`), so its outer's calls and errors never happen
        if ran.is_ok() && seen && build_first_empty(&exec.node, stopped) {
            let Exec { mut node, .. } = exec;
            node.set_copies(None);
            return (node, Ok(Ret::Done));
        }
        // D2674: the stopped pipeline is called: its marked probe records PG's points that set hj_OuterNotEmpty in this
        // call alone (zeroed first; PG resets the flag after a non-empty build, and its outer-first call sets it from the
        // first row), and the bit is that record after the call
        let word = epq_word(&exec.node, stopped);
        if let Some(i) = word {
            crate::mem::region::set_frame_word(&exec.node, i, 0);
        }
        // the stopped pipeline opened over the rebuilt products (its consumed slots' pointers), then its one call
        // writesubplan: marked opened as `run_pipeline` marks it, so a nested run its call makes holds its partials and puts
        // them back after (`out_of_order`'s hold / reattach): a rerun member's open rewrites the node's partials
        let opened = ran.and_then(|()| {
            let split = exec.node.share(stopped, exec.grant(), 1);
            exec.node.open_pipeline(stopped, split.breaker, split.images.bytes).map(|()| exec.node.mark_opened(stopped))
        });
        // D1706: the stopped pipeline's call attached to its own pointer to `b` (the producers' calls put `apply`'s back).
        // writesubplan: published as the current execution (D1702), so a nested run the call makes (a MERGE action's SubPlan
        // rerunning its set, lowerv4 marks.rs) re-enters this node, never the statement's
        // D2033: a stop at a lazy build's probe (its first row reached it) runs the build by the runner's rerun arm
        // (`build_wait`, the producers fed into it first), then the call resumes at that row
        let mut call = Call::batch(out_cap);
        let called = opened.and_then(|()| loop {
            let (ep, xp, bp): (*mut Entry, *mut Exec, *mut Binding<'_>) = (&mut *self, &mut exec, &mut *b);
            match raw::call_stopped_published(ep, xp, bp, stopped, call) {
                Ok(Ret::BuildWait { consumed }) => {
                    let waited = lazy_build(exec.node.program(), stopped, consumed).and_then(|lb| {
                        exec.next_pipeline = stopped;
                        // a producer the recheck steps past (`recheck_skip`: a COPY-marked subquery's, D732-rest / D2636 /
                        // D3285) was stepped in the loop above, its slot opened or deferred there: never run here
                        let program = exec.node.program().clone();
                        self.build_wait(&mut exec, b, lb, &mut |e, x, bb, p| match program.pipelines[p].recheck_skip {
                            true => Ok(()),
                            false => (r.run)(e, x, bb),
                        })
                    });
                    if let Err(e) = waited {
                        break Err(e);
                    }
                    exec.resume_next = false;
                    call = Call::resume(out_cap);
                }
                other => break other,
            }
        });
        let Exec { mut node, .. } = exec;
        node.set_copies(None);
        if let (Ok(ret), Some(i)) = (&called, word) {
            if !matches!(ret, Ret::Recheck { .. }) {
                let set = crate::mem::region::frame_word(&node, i).is_some_and(|w| w != 0);
                self.epq_note(lease, stopped, set);
            }
        }
        (node, called)
    }

    /// D2674: the stopped pipeline `stopped`'s bit for the statement `lease` after a called recheck: `set` is its probe's record.
    fn epq_note(&mut self, lease: std::rc::Weak<Reservation>, stopped: usize, set: bool) {
        if stopped >= 64 {
            return;
        }
        match &mut self.epq_outer {
            Some((had, mask)) if had.ptr_eq(&lease) => *mask = (*mask & !(1 << stopped)) | ((set as u64) << stopped),
            _ if set => self.epq_outer = Some((lease, 1 << stopped)),
            _ => {}
        }
    }
}

/// D2845: a kernel's scratch refusal (kind 2) in a statement whose draw past W and G the grant pool refused (an arena chunk or a
/// pin: `region::road_region`, the kernel reads the refused draw as its scratch running out) is that refusal, the named out of
/// memory (53200) a refused breaker region raises. Alone in a 256 MiB pool, a 2 M-group GROUP BY at W8 grew its owner 128 MB
/// past its 64 MiB grant and the emit's 16 MB gather array found the pool full: "unsupported execution (kind 2)". Any other
/// error, and every error of a statement the pool refused nothing, as it is.
/// D2701 / D3377: the emits a soft rewind runs again: the program's last pipelines that are emits (the last one always
/// is: `pull::is_emit`), in pipeline order.
fn rewound_emits(program: &Program) -> core::ops::Range<usize> {
    let n = program.pipelines.len();
    let first = (0..n).rev().take_while(|&p| crate::pull::is_emit(program, p)).last().unwrap_or(n);
    first..n
}

fn pool_refused(stmt: &Reservation, e: Error) -> Error {
    match e {
        Error::Unsupported(gravity_abi::status::UNSUPPORTED_SCRATCH) if stmt.refused() != 0 => {
            let pool = &crate::mem::grant::POOL;
            Error::OutOfMemory { bytes: stmt.refused(), held: pool.held(), capacity: pool.capacity(), why: None }
        }
        e => e,
    }
}

/// D3285: whether the recheck opens the slot of `recheck_skip` producer `q` empty: a pipeline it runs (one not
/// `recheck_skip`) reads q's product through the slot, or a `recheck_skip` consumer it opens does (its open reads every
/// slot it consumes live).
fn read_by_recheck(program: &Program, q: usize) -> bool {
    stack_depth_core::check_stack_depth_or_panic();
    program.consumers_of(gravity_program::LifeId(q as u16)).any(|r| !program.pipelines[r].recheck_skip || read_by_recheck(program, r))
}

/// D2050: the build of pipeline `stopped`'s marked probe (`marked`), rerun by the recheck and unspilled, came out empty:
/// its reading (`read[0]`, the one D276's empty-build skip reads) counts no row. An unlaid region or one without a reading is
/// never empty here.
fn build_first_empty(node: &Node, stopped: usize) -> bool {
    let program = node.program();
    marked(node, stopped).any(|p| {
        let rerun = program.pipelines.get(p.pipeline as usize).is_some_and(|q| !q.recheck_skip && q.produces() == Some(p.slot));
        rerun && node.spill_parts_of(p.slot) == 0 && p.read_rows(0, node.slot_region(p.slot)) == Some(0)
    })
}

/// D2674: the frame word where pipeline `stopped`'s marked probe records PG's hj_OuterNotEmpty points (`Proto::epq_word`).
fn epq_word(node: &Node, stopped: usize) -> Option<u16> {
    marked(node, stopped).find_map(|p| p.epq_word())
}

/// D2050 / D2674: the BuildSide protos of pipeline `stopped`'s marked probe (`BUILDSIDE_EPQ_FIRST` with its frame word,
/// lowerv4 rerun/epq/marks.rs `epq_first`).
fn marked(node: &Node, stopped: usize) -> impl Iterator<Item = &gravity_program::protos::Proto> {
    let protos = node.program().v4.as_ref().map_or(&[][..], |v4| &v4.protos[..]);
    protos.iter().filter(move |p| p.peer as usize == stopped && p.epq_word().is_some())
}

/// D655: what `Entry::recheck` reruns a stopped pipeline's producers with: the statement's admission (the recheck node's
/// execution draws its breakers' shares from it) and the runner's width-1 run of one producer (exec epq.rs).
pub struct Rerun<'r> {
    pub lease: std::rc::Rc<Reservation>,
    pub run: &'r mut dyn FnMut(&mut Entry, &mut Exec, &mut Binding<'_>) -> Result<()>,
    /// D732: the stopped node's settled positions (a COPY mark site's stored row).
    pub positions: &'r [gravity_abi::ScanPos],
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;

    /// D4050: the run-empty words past pipeline 63 are sized by the program (none up to 64 pipelines), and a word's ran
    /// bits are those of its 64 pipelines before `next`.
    #[test]
    fn the_run_empty_words_past_63_are_sized_by_the_program() {
        assert_eq!([0, 1, 64, 65, 128, 129, 200].map(|n| past_words(n).len()), [0, 0, 0, 1, 1, 2, 3]);
        assert_eq!((ran_of(0, 0), ran_of(3, 0), ran_of(64, 0), ran_of(99, 0)), (0, 0b111, u64::MAX, u64::MAX));
        assert_eq!((ran_of(10, 64), ran_of(70, 64), ran_of(128, 64), ran_of(200, 64)), (0, 0b11_1111, u64::MAX, u64::MAX));
    }

    /// D2652: two lazy builds of probe 6, each over a settled union (p0 / p1 feed the record p2 reads; p3 / p4 the record
    /// p5 reads; each earlier feed's renumbered slot read by none): each wait runs its own union's every feed, the other's
    /// none.
    #[test]
    fn a_lazy_builds_wait_runs_every_feed_of_its_settled_union_and_no_other() {
        use gravity_abi::breaker::{Breaker, BreakerKind};
        use gravity_program::{Arena, Code, End, Layout, Pipeline, Reason, WritePlan};
        unsafe extern "C-unwind" fn noop(_: *mut gravity_abi::Ctx) -> i64 {
            0
        }
        let arena = Arena::new(1 << 14);
        let a = arena.handle();
        let end = |kind, slot: u8| End::Breaker(Breaker::sized(kind, 4096).in_slot(slot));
        let lazy = |kind, slot: u8, reads: Option<u8>| {
            let p = Pipeline { lazy_for: 6, ..Pipeline::serial(-1, end(kind, slot), Reason::NoDrivingSite) };
            reads.map_or(p, |s| p.consuming_in(a, s))
        };
        let probe = Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_in(a, 0).consuming_in(a, 1);
        let ps = [
            lazy(BreakerKind::Materialize, 3, None),
            lazy(BreakerKind::Materialize, 2, None),
            lazy(BreakerKind::HashBuild, 0, Some(2)),
            lazy(BreakerKind::Materialize, 5, None),
            lazy(BreakerKind::Materialize, 4, None),
            lazy(BreakerKind::HashBuild, 1, Some(4)),
            probe,
        ];
        let program = Program::with_declared_pipelines(Layout { nsites: 0, out_natts: 0, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &[], &[], WritePlan::SELECT, ps.into_iter().map(|p| (p, Code::hand(noop))));
        let run = |b: usize| (0..6).filter(|&p| fed_lazy(&program, p, b, 6)).collect::<Vec<_>>(); // alloc: Cold
        assert_eq!(run(2), [0, 1, 2], "the first build's wait: both feeds of its union, in order");
        assert_eq!(run(5), [3, 4, 5], "the second build's wait: its own union only");
    }

    /// D3285: a `recheck_skip` producer's slot opens at the recheck only when a pipeline the recheck opens reads it: a rerun
    /// one, or a skipped one the chain leads to a rerun reader through (its open reads every slot it consumes live). A LAZY
    /// set's build, which no pipeline consumes (its probe reads the set through the param handle), steps past unopened.
    #[test]
    fn a_skipped_producer_opens_only_when_a_pipeline_the_recheck_opens_reads_it() {
        use gravity_abi::breaker::{Breaker, BreakerKind};
        use gravity_program::{Arena, Code, End, Layout, Pipeline, Reason, WritePlan};
        unsafe extern "C-unwind" fn noop(_: *mut gravity_abi::Ctx) -> i64 {
            0
        }
        let arena = Arena::new(1 << 14);
        let a = arena.handle();
        let skip = |kind, slot: u8, reads: Option<u8>| {
            let p = Pipeline { recheck_skip: true, ..Pipeline::serial(-1, End::Breaker(Breaker::sized(kind, 4096).in_slot(slot)), Reason::NoDrivingSite) };
            reads.map_or(p, |s| p.consuming_in(a, s))
        };
        let ps = [
            skip(BreakerKind::HashBuild, 0, None),
            skip(BreakerKind::Materialize, 1, None),
            skip(BreakerKind::Materialize, 2, Some(1)),
            skip(BreakerKind::Materialize, 3, None),
            Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_in(a, 2),
        ];
        let program = Program::with_declared_pipelines(Layout { nsites: 0, out_natts: 0, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &[], &[], WritePlan::SELECT, ps.into_iter().map(|p| (p, Code::hand(noop))));
        let opened = (0..4).filter(|&q| read_by_recheck(&program, q)).collect::<Vec<_>>(); // alloc: Cold
        assert_eq!(opened, [1, 2], "the chain the rerun reader reads opens; the unconsumed build and the unread one step past");
    }

    /// D470: a recheck's surviving row keeps its by-reference images in the caller's buffer, its datums re-pointed there:
    /// the node's region they were pushed in is overwritten (reused, poisoned or freed) once the recheck releases it.
    #[test]
    fn a_recheck_row_owns_its_by_reference_images() {
        let (mut values, mut isnull, mut region) = ([0u64; 3], [0u8; 3], Vec::new()); // alloc: Cold
        let mut node_region = *b"imageone";
        {
            let mut cap = Capture { out: RowBuf { values: &mut values, isnull: &mut isnull, region: &mut region, byref: &[false, true, true] }, got: false, pending: None };
            let v = [7u64, node_region.as_ptr() as u64, 0];
            assert_eq!(cap.push(BorrowedBatch { natts: 3, rows: 1, values: &v, isnull: &[0, 0, 1], region: &node_region }).unwrap(), Flow::Stop);
            assert!(cap.got);
        }
        node_region.fill(b'!');
        assert_eq!((values[0], values[1], isnull), (7, region.as_ptr() as u64, [0, 0, 1]));
        assert_eq!(unsafe { core::slice::from_raw_parts(values[1] as *const u8, 8) }, b"imageone");
    }

    /// D2744: with a `Pending`, a MERGE recheck's sink takes the first row into `out` and the next into the pending buffer
    /// (one batch of two rows, or two batches of one), owning both rows' images, and stops once it has both; without one it
    /// stops at the first row.
    #[test]
    fn a_merge_recheck_keeps_its_pending_second_row() {
        for split in [false, true] {
            let (mut values, mut isnull, mut region) = ([0u64; 2], [0u8; 2], Vec::new()); // alloc: Cold
            let mut pending = Pending::default();
            let mut node_region = *b"by-sourceinserted";
            {
                let mut cap = Capture { out: RowBuf { values: &mut values, isnull: &mut isnull, region: &mut region, byref: &[false, true] }, got: false, pending: Some(&mut pending) };
                let base = node_region.as_ptr() as u64;
                let v = [1u64, base, 2, base + 9];
                if split {
                    assert_eq!(cap.push(BorrowedBatch { natts: 2, rows: 1, values: &v[..2], isnull: &[0, 0], region: &node_region }).unwrap(), Flow::Continue, "one row: the pending one may come");
                    assert_eq!(cap.push(BorrowedBatch { natts: 2, rows: 1, values: &v[2..], isnull: &[0, 0], region: &node_region }).unwrap(), Flow::Stop);
                } else {
                    assert_eq!(cap.push(BorrowedBatch { natts: 2, rows: 2, values: &v, isnull: &[0, 0, 0, 0], region: &node_region }).unwrap(), Flow::Stop);
                }
            }
            node_region.fill(b'!');
            assert!(pending.got);
            assert_eq!((values[0], pending.values[0]), (1, 2));
            assert_eq!(unsafe { core::slice::from_raw_parts(values[1] as *const u8, 9) }, b"by-source");
            assert_eq!(unsafe { core::slice::from_raw_parts(pending.values[1] as *const u8, 8) }, b"inserted");
        }
    }

    /// D1702 (main's condition 1): a call publishes its execution over the one before for the call only, and puts that one
    /// back on every exit, an unwind out of the call (a kernel's panic) too. The published pointers are only stored here.
    #[test]
    fn a_call_puts_the_current_before_it_back_on_every_exit_an_unwind_too() {
        let mut e = Entry::new(1);
        let cell = CurrentCell::new(None);
        assert!(!e.has_current_slot());
        e.with_current_slot(&cell, |e| {
            assert!(e.has_current_slot());
            let outer = core::ptr::NonNull::<Exec>::dangling().as_ptr();
            let inner = outer.wrapping_add(1);
            let b: *mut Binding<'static> = core::ptr::NonNull::dangling().as_ptr();
            let this: *mut Entry = e;
            cell.set(Some(Current { entry: this, exec: outer, binding: b }));
            let exec_now = || cell.get().map(|c| c.exec);
            assert_eq!(unsafe { Entry::publish_current(this, inner, b, || exec_now()) }, Some(inner), "the call's own");
            assert_eq!(exec_now(), Some(outer), "the one before, back after a return");
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                Entry::publish_current(this, inner, b, || {
                    assert_eq!(exec_now(), Some(inner));
                    std::panic::resume_unwind(Box::new(())) // alloc: Cold
                })
            }));
            assert!(r.is_err());
            assert_eq!(exec_now(), Some(outer), "the one before, back after an unwind");
        });
        assert!(!e.has_current_slot(), "the slot is off the entry after the run");
    }

    /// D1709: a run's slot comes off the entry when the run unwinds, and a nested run's slot gives back the outer one.
    #[test]
    fn a_runs_slot_comes_off_on_unwind_and_a_nested_run_gives_back_the_outer() {
        let mut e = Entry::new(1);
        let (outer, inner) = (CurrentCell::new(None), CurrentCell::new(None));
        e.with_current_slot(&outer, |e| {
            e.with_current_slot(&inner, |e| assert_eq!(e.current, &inner as *const CurrentCell));
            assert_eq!(e.current, &outer as *const CurrentCell, "the outer run's slot, back");
        });
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| e.with_current_slot(&outer, |_| std::panic::resume_unwind(Box::new(()))))); // alloc: Cold
        assert!(r.is_err());
        assert!(!e.has_current_slot(), "the slot is off the entry after the unwind");
    }
}
