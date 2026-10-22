//! `nest.initplan`'s Init protocol (RT-1): PG's
//! `ExecSetParamPlan` in a v4 program. The InitPlan's pipeline `p` (the proto's) ends in M8-SUB's cell end, which folds its
//! rows into the Init cell's pairs through the handle; `p` and its exclusive producers are the Init's members, stepped past
//! in the producer order (`Live::deferred`) and run on demand: the first read of the param on a thread calls
//! `m8_init_run(ctx, h, paramid)`, which runs the members once for this execution, nested inside the reader's call,
//! through the runner's frame (`Live::frame`, `gravity_driver::OnDemand`). An Init that is never read never runs, and never
//! raises (an empty outer), as PG's lazy `execPlan`.
//!
//! The cell (kernels host_abi/v4/m8.rs `InitCell`): the head, the state, `n`, the pairs at `init_pair(k)`, then one word
//! this crate keeps: the address of the Init's runtime state (`InitRt`). The cell and the state live in the protocol's
//! `Live`, created at every execution's prepare and dropped at its end (a cached program never carries a value into the
//! next execution; a by-reference result's copy, `m8_init_copy`, goes with it).
//!
//! D1035: every pipeline that reads an Init param runs on the leader, which holds
//! the frame (the lowering pins it, as D1005 pins a lazy build's probe); the members run nested through their width-1
//! arm, and every other pipeline keeps its width. A read on a worker finds no frame: a Bug guard by name, never a value.
//! Created by M8-RUNTIME.
use core::ptr::NonNull;
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU32, Ordering};

use gravity_abi::status::ERROR;
use gravity_kernels::ops::shared::object::entries as abi;
use gravity_program::protos::{Proto, ProtoKind};
use types_error::PgResult;

use memledger::ChargedVec;

use crate::ops::{Frame, Live, Prepare, Protocol};
use crate::glue;

/// L0-k's read of an InitPlan param (raw.rs, its unsafe half): exec pruning's and a rerun set's, by this path.
pub(crate) use super::raw::read_param;

pub struct Init;

/// The most pairs one Init cell holds (a MULTIEXPR's setParams): contract::layout's Gravity protocol capacity.
use gravity_abi::limits::INIT_MAX_PAIRS as MAX_PAIRS;

/// The Init cell's `state` (the runtime's; IR never reads it).
pub(super) const NOT_RUN: u32 = 0;
/// D1262: the members ran this execution and a rerun set holding them ran since (`rearm`, PG's ExecReScanSetParamPlan): the
/// next read runs them again (`ensure`), as PG's next ExecSetParamPlan.
pub(super) const STALE: u32 = 5;
pub(super) const RUNNING: u32 = 1;
pub(super) const DONE: u32 = 2;
pub(super) const FAILED: u32 = 3;

/// One Init's runtime state for one execution.
pub(super) struct InitRt {
    /// `m8_init_copy`'s copies, 8-aligned, alive until the execution's end (D2647: or a rewind's reopened run's end), on the
    /// execution's prepare owner (`Copies`).
    pub(super) copies: RefCell<Copies>,
    /// D3274: the keyed rechecks' armings (raw.rs `keyed`), innermost last: a write's recheck, then the lock's inside it.
    /// Each entry is the arming (none when the cell was left alone) and whether another was open below it.
    /// Kept for the whole execution, so on the execution's prepare owner as it grows.
    pub(super) keyed: RefCell<ChargedVec<Option<(super::raw::Armed, bool)>>>,
    /// D3274: the EvalPlanQual states that ran the members again, by (key, nested): such a state's later recheck reads the
    /// value its parent copies in, until the state ends (`INIT_EPQ_END`, PostgreSQL's EvalPlanQualEnd).
    /// On the execution's prepare owner, as `keyed`.
    pub(super) ran: RefCell<ChargedVec<(u32, bool)>>,
    /// The exec param of pair 0; pair `k` is param `first + k` (M8-SUB's B1: an InitPlan's setParams are consecutive).
    pub(super) first: i32,
    /// The members in program order: `p` and its exclusive producers.
    pub(super) members: Box<[u16]>,
    /// The runner's frame while it runs this execution's program (`Live::frame`).
    pub(super) frame: Cell<Option<Frame>>,
    /// D1035 step 2 (V4R-444): the eager Init's one reader (the proto's `peer`, lowerv4 frame/program.rs): at its start the
    /// frame may run the members ahead (`OnDemand::prerun_members`); `None` for every other Init.
    reader: Option<usize>,
    /// D2673: a recheck ran the members again this execution (raw.rs `epq_back`): PostgreSQL's recheck EState evaluates an
    /// InitPlan it armed afresh once, and a later recheck reads the value the statement's run copies in.
    pub(super) epq: Cell<bool>,
    /// D3272: an IR recheck's arming of this cell (raw.rs `epq`), kept from its arm until its put back.
    pub(super) armed: RefCell<Option<super::raw::Armed>>,
    /// D2998: an EXISTS InitPlan (`INIT_EXISTS`): each run starts with its cell false (raw.rs `ensure`); a row stores true.
    pub(super) exists: bool,
}

impl InitRt {
    /// D2673: the runner's frame is published (`Live::frame`): a read can run the members now (`ensure`).
    pub(super) fn has_frame(&self) -> bool {
        self.frame.get().is_some()
    }
}

/// An Init's by-reference copies (`m8_init_copy`) and the charge of every byte they hold, on the
/// execution's prepare owner (`Prepare::owner`: the portal the backend executes, else its running statement; never a cache:
/// the copies live in the execution's `Live`, never in a cached product). They live until the cell's next run again (D4442,
/// `clear`), an EvalPlanQual arming's put back (`take`, then `extend`), or the execution's end; a rewind's carry moves them
/// whole with their charge into the reopened run's cell (D2647: `take`, then `extend`, whose `absorb` moves the bytes to the
/// reopened run's owner). With no owner (`prepare_bare`, the crate rig) the charge is `NONE`: the bytes stay on the
/// allocating thread's account, as a plain `Vec`'s. One thread holds them: the leader (RT-1: an Init's cell and state are
/// written only there). The charge is the bytes' counter: each copy's block plus the list's own, refit after every change.
pub(crate) struct Copies {
    /// Declared first, so dropped first (as `ChargedVec`'s): its door-1 drop moves the bytes to the freeing thread's account,
    /// where the copies' frees land next.
    charge: memledger::Charge,
    /// `charge` is open on an owner (`Charge::NONE` takes no commit).
    on: bool,
    /// The copies' words together (each copy's block is `len * 8` bytes).
    words: usize,
    v: Vec<Box<[u64]>>,
}

impl Copies {
    /// Empty, charged to `owner` (on its kind lane) as it fills; `None`: uncharged.
    pub(super) fn new(owner: Option<(memledger::Owner, memledger::Kind)>) -> Copies {
        match owner {
            Some((o, k)) => Copies { charge: memledger::Charge::open(o, memledger::charge::release_of(o), k, memledger::charge::Door::Heap, 0), on: true, words: 0, v: Vec::new() }, // alloc: Exec charged o
            None => Copies { charge: memledger::Charge::NONE, on: false, words: 0, v: Vec::new() }, // alloc: Exec
        }
    }

    /// Empty, on this one's owner and kind lane (what a `take` leaves behind: the cell may fill again).
    fn empty_like(&self) -> Copies {
        match self.on {
            true => Copies::new(Some((self.charge.owner(), self.charge.kind()))),
            false => Copies::new(None),
        }
    }

    /// The bytes the copies hold: each copy's block and the list's.
    fn bytes(&self) -> u64 {
        (self.v.capacity() * core::mem::size_of::<Box<[u64]>>() + self.words * core::mem::size_of::<u64>()) as u64
    }

    /// The charge brought to `bytes` on this thread (R2: after every change).
    fn refit(&mut self) {
        if self.on {
            let n = self.bytes();
            self.charge.fit_committed(n);
        }
    }

    /// One more copy, its block and the list's growth charged as they are kept.
    pub(super) fn push(&mut self, b: Box<[u64]>) {
        self.words += b.len();
        self.v.push(b); // alloc: Exec charged self.charge
        self.refit();
    }

    /// Every copy moved out whole, with its charge (their addresses hold); this one is left empty on the same owner.
    pub(super) fn take(&mut self) -> Copies {
        let fresh = self.empty_like();
        core::mem::replace(self, fresh)
    }

    /// `other`'s copies moved in whole (their addresses hold), its charge taken over (`Charge::absorb`: moved to this one's
    /// owner when another); an uncharged `other`'s bytes are committed here by the refit (the leader allocated them).
    pub(super) fn extend(&mut self, other: Copies) {
        let Copies { charge, on, words, v } = other;
        self.words += words;
        if self.on && on {
            self.charge.absorb(charge);
        } else {
            // one side uncharged: `other`'s charge (if any) gives its bytes back to this thread, whose refit below takes them
            drop(charge);
        }
        self.v.extend(v); // alloc: Exec charged self.charge
        self.refit();
    }

    /// Every copy freed and the list with them (D4442: the previous run's, at the run again).
    pub(super) fn clear(&mut self) {
        self.v = Vec::new(); // alloc: Exec charged self.charge
        self.words = 0;
        self.refit();
    }
}

impl core::ops::Deref for Copies {
    type Target = [Box<[u64]>];
    fn deref(&self) -> &[Box<[u64]>] {
        &self.v
    }
}

/// The protocol's `Live`: the cell (the handle's target) and the state its trailing word names.
struct InitLive {
    cell: Box<[u64]>,
    rt: Box<InitRt>,
}

impl Live for InitLive {
    fn answers(&self) -> u16 {
        crate::ops::ask::DEFERRED | crate::ops::ask::FRAME | crate::ops::ask::LATE
    }

    fn deferred(&self, pipeline: usize) -> bool {
        u16::try_from(pipeline).is_ok_and(|p| self.rt.members.binary_search(&p).is_ok())
    }

    fn frame(&self, f: Option<Frame>) {
        self.rt.frame.set(f);
    }

    /// D1035 step 2 (V4R-444): the eager reader's start (its late step, on the leader, before any of its claims): the frame
    /// runs the members ahead when the reader runs on workers (PostgreSQL's Gather `initParam`). The run's error is this
    /// start's, before any row left: the lowering asked a recheck licence for the run ahead of a raising InitPlan
    /// (v5 errorhold), so the statement runs once more in plan order, its InitPlan lazy, and PostgreSQL's outcome is that
    /// run's (an InitPlan no row reads never raises); a cancel (class 57) is never re-run.
    fn late_words(&self, pipeline: usize, _out: &mut gravity_parallel::LateWords) -> PgResult<()> {
        if self.rt.reader != Some(pipeline) {
            return Ok(());
        }
        let state = super::raw::own_state(&self.cell);
        let Some(mut f) = self.rt.frame.get() else { return Ok(()) };
        if state.load(Ordering::Acquire) != NOT_RUN {
            return Ok(());
        }
        state.store(RUNNING, Ordering::Relaxed);
        super::raw::exists_false_own(&self.cell, &self.rt);
        let ran = super::raw::on_demand(&mut f).prerun_members(pipeline, &self.rt.members);
        if crate::inputs::knobs::seam::report_on() && !matches!(ran, Ok(false)) {
            let what = if ran.is_ok() { "ran" } else { "failed" };
            let _ = elog::ereport(types_error::LOG).errmsg(format!("gravity: nest.initplan {what} its members {:?} ahead of reader pipeline {pipeline} (D1035 step 2)", self.rt.members)).errhidestmt(true).errhidecontext(true).finish(types_error::ErrorLocation::new("gravity", 0, "initplan")); // alloc: Cold
        }
        match ran {
            Ok(true) => state.store(DONE, Ordering::Release),
            Ok(false) => state.store(NOT_RUN, Ordering::Relaxed),
            Err(e) => {
                state.store(FAILED, Ordering::Relaxed);
                return Err(glue::pg_error(e));
            }
        }
        Ok(())
    }
}

impl Protocol for Init {
    fn kind(&self) -> ProtoKind {
        ProtoKind::Init
    }

    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        // D3865: the members once per admission (a nested statement's Init is laid at every call)
        let program = cx.program;
        let members = match core::ptr::eq(program, &*cx.admitted.program) {
            true => cx.admitted.memo.members(program.pipelines.len(), p.pipeline as usize, || members_of(p, program))?,
            false => members_of(p, program)?,
        };
        lay(p, program, Some(members), cx.owner, cx.words, cx.nulls, cx.nparams).map(Some)
    }
}

/// M8 (RT-1): the M8 lives of `program` alone (Init; RT-2a / RT-2c's SubHash and SubResult), laid as exec's prepare lays
/// them (the handle words): the crate rig's (lowerv4 tests/common), which drives a program through the driver without
/// exec's `State`. The rig skips each live's `deferred` pipelines and publishes its own frame to each (`Live::frame`).
pub fn prepare_bare(program: &gravity_program::Program, words: &mut [u64], nulls: &mut [u8], nparams: &mut usize, out: &mut crate::ops::LiveList) -> PgResult<()> {
    let protos = program.v4.as_ref().map_or(&[][..], |d| &d.protos[..]);
    let mut b = crate::ops::subplan::hash::Bare { program, words, nulls, nparams };
    let lay_one = |p: &Proto, b: &mut crate::ops::subplan::hash::Bare<'_>| match p.kind {
        ProtoKind::Init => lay(p, b.program, None, None, b.words, b.nulls, b.nparams).map(Some),
        ProtoKind::SubHash => crate::ops::subplan::hash::lay_set(p, b).map(Some),
        ProtoKind::SubHashEpq => crate::ops::subplan::hash::lay_twin(p, b).map(Some),
        ProtoKind::SubResult => crate::ops::subplan::hash::lay_result(p, b).map(Some),
        _ => Ok(None),
    };
    for p in protos {
        if let Some(l) = lay_one(p, &mut b)? {
            out.with_vec(|v| v.push(l)); // alloc: Exec charged out
        }
    }
    Ok(())
}

/// D2914 (2a): Init proto `p`'s members run only nested, through the reader's call: every Init but the eager one whose
/// reader (`lay`'s) can run on workers, where its start runs them ahead at their widths (D1035 step 2; an emit or a serial
/// reader runs nothing ahead, `crate::ops::reader_runs_wide`).
pub(crate) fn runs_nested(p: &Proto, program: &gravity_program::Program) -> bool {
    !(eager(p) && crate::ops::reader_runs_wide(program, p.peer as usize))
}

/// D2914 (2a): Init proto `p` is the eager one, its one reader named (`lay`'s run-ahead arm).
pub(crate) fn eager(p: &Proto) -> bool {
    p.peer != gravity_program::protos::NONE && p.arg & abi::INIT_EAGER != 0
}

/// The Init's members (`p` and its exclusive producers), their slots checked as the members' alone.
fn members_of(p: &Proto, program: &gravity_program::Program) -> PgResult<Box<[u16]>> {
    let members = program.members(p.pipeline as usize).members.into_boxed_slice(); // alloc: Exec
    exclusive_slots(program, &members, "nest.initplan")?;
    Ok(members)
}

/// An empty list on `owner` (`None`: uncharged), for the state's lists kept for the execution.
fn charged<T>(owner: Option<(memledger::Owner, memledger::Kind)>) -> ChargedVec<T> {
    match owner {
        Some((o, k)) => ChargedVec::new(Some(o), k), // alloc: Exec charged o
        None => ChargedVec::none(),
    }
}

/// One Init proto's cell and state, its handle laid in its word; its `members` when the caller kept them. The state's
/// lists kept for the execution are charged to `owner` (the execution's prepare owner, `Prepare::owner`; `None`: uncharged).
#[allow(clippy::too_many_arguments)]
fn lay(p: &Proto, program: &gravity_program::Program, members: Option<Box<[u16]>>, owner: Option<(memledger::Owner, memledger::Kind)>, words: &mut [u64], nulls: &mut [u8], nparams: &mut usize) -> PgResult<Box<dyn Live>> {
    let (n, first) = (abi::init_pairs(p.arg) as usize, abi::init_first(p.arg));
    let base = p.words.base as usize;
    if n == 0 || n > MAX_PAIRS || p.words.n != 1 || base >= words.len() || base >= nulls.len() || p.pipeline as usize >= program.pipelines.len() {
        return Err(glue::internal("gravity: nest.initplan's declaration is not its Init cell's (1 to INIT_MAX_PAIRS pairs, one handle word)"));
    }
    let members = match members {
        Some(m) => m,
        None => members_of(p, program)?,
    };
    let reader = (p.peer != gravity_program::protos::NONE && p.arg & abi::INIT_EAGER != 0).then_some(p.peer as usize);
    let rt = Box::new(InitRt { first, members, frame: Cell::new(None), copies: RefCell::new(Copies::new(owner)), reader, epq: Cell::new(false), armed: RefCell::new(None), keyed: RefCell::new(charged(owner)), ran: RefCell::new(charged(owner)), exists: p.arg & abi::INIT_EXISTS != 0 }); // alloc: Exec
    let nwords = abi::init_pair(n) / 8 + 1;
    let mut cell = vec![0u64; nwords].into_boxed_slice(); // alloc: Exec
    super::raw::lay_cell(&mut cell, n, &rt);
    words[base] = cell.as_ptr() as u64;
    nulls[base] = 0;
    *nparams = (*nparams).max(base + 1);
    Ok(Box::new(InitLive { cell, rt })) // alloc: Exec
}

/// The wall behind lowerv4's renumbering of deferred slots (D1005's renumber_deferred, M8's members added): a member's
/// slot number no pipeline outside the members produces or reads, so a nested run never re-opens a slot a running
/// pipeline holds. A miss is an ERROR by name at the execution's prepare, never a clobber.
pub(crate) fn exclusive_slots(program: &gravity_program::Program, members: &[u16], what: &str) -> PgResult<()> {
    let ps = &program.pipelines;
    match shared_slot(ps.len(), members, |q| ps[q].produces(), |q| ps[q].consumes) {
        Some((m, s, q)) => Err(glue::internal(&format!("gravity: {what}: member pipeline {m}'s slot {s} is also pipeline {q}'s (a deferred slot not renumbered)"))), // alloc: Cold
        None => Ok(()),
    }
}

/// The first member slot a non-member also produces or reads: `(member, slot, the other pipeline)`.
fn shared_slot<'a>(n: usize, members: &[u16], produces: impl Fn(usize) -> Option<u8>, consumes: impl Fn(usize) -> &'a [u8]) -> Option<(u16, u8, usize)> {
    let member = |q: usize| u16::try_from(q).is_ok_and(|q| members.binary_search(&q).is_ok());
    members.iter().find_map(|&m| {
        let s = produces(m as usize)?;
        (0..n).filter(|&q| !member(q)).find(|&q| produces(q) == Some(s) || consumes(q).contains(&s)).map(|q| (m, s, q))
    })
}

/// D1262 (PG's ExecReScanSetParamPlan): a rerun set holding the members of the Init whose cell state is `state` runs again,
/// so the members run again at the next read of a param, never now: a cell that ran goes STALE; one that never ran, is stale
/// or failed stays so; one running is a Bug by name. The caller (exec rerun.rs `decide`) clears the params' frame pairs.
pub(crate) fn rearm(state: &AtomicU32) -> PgResult<()> {
    match state.load(Ordering::Acquire) {
        DONE => state.store(STALE, Ordering::Release),
        RUNNING => return Err(bad("m8_rerun: an InitPlan re-armed inside its own run")),
        _ => {}
    }
    Ok(())
}

/// D2647: the InitPlan values an execution computed, carried across its rewind into the reopened run (`carry_out`, then
/// `carry_in`): PostgreSQL's ExecutorRewind keeps every InitPlan value (ExecReScan runs one again only when a param it reads
/// changed, and a rewind changes none), while the served reopen prepares the protocols afresh, each Init cell NOT_RUN.
#[derive(Default)]
pub struct Carried(Vec<CarriedInit>);

struct CarriedInit {
    /// The Init proto's handle word.
    base: usize,
    pairs: Box<[u64]>,
    /// The by-reference copies the pairs point into, moved whole (their addresses hold) with their charge.
    copies: Copies,
}

/// The Init protos a rewind carries: all but those a rerun set holds, which the set re-arms at each of its runs (D1262).
fn carried(protos: &[Proto]) -> impl Iterator<Item = &Proto> {
    protos.iter().filter(move |p| p.kind == ProtoKind::Init && !protos.iter().any(|q| q.kind == ProtoKind::Rerun && q.pipeline == p.pipeline))
}

/// D2647, as the run that may be rewound closes: each carried Init of `program` that ran, read from `words` (the run's bound
/// words; its protocols still alive): its pairs, and its copies moved out.
pub fn carry_out(program: &gravity_program::Program, words: &[u64]) -> Carried {
    carry_from(program.v4.as_ref().map_or(&[][..], |d| &d.protos[..]), words)
}

fn carry_from(protos: &[Proto], words: &[u64]) -> Carried {
    let mut out = Vec::new(); // alloc: Exec
    for p in carried(protos) {
        let base = p.words.base as usize;
        if let Some((pairs, copies)) = words.get(base).and_then(|&w| super::raw::take_done(w)) {
            out.push(CarriedInit { base, pairs, copies }); // alloc: Exec
        }
    }
    Carried(out)
}

/// D2647, at the reopen: each carried value laid into the cell this execution's prepare bound in `words`, DONE, so its next
/// read returns it and never runs the members.
pub fn carry_in(words: &[u64], kept: &mut Carried) -> PgResult<()> {
    for k in kept.0.drain(..) {
        let w = words.get(k.base).copied().unwrap_or(0);
        super::raw::lay_done(w, &k.pairs, k.copies).map_err(|why| bad(&format!("D2647: {why}")))?; // alloc: Cold
    }
    Ok(())
}

#[cold]
#[inline(never)]
pub(super) fn fail(what: &str) -> i64 {
    gravity_fn::stash_error(bad(what));
    ERROR
}

#[cold]
#[inline(never)]
pub(super) fn bad(what: &str) -> Box<types_error::PgError> {
    glue::internal(&format!("gravity: {what}")) // alloc: Cold
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use super::super::raw::op;
    use gravity_abi::Ctx;

    /// A frame that records the member runs and answers with `then`; a run ahead (D1035 step 2) answers `wide`.
    struct Rec {
        runs: Vec<Vec<u16>>,
        then: Option<gravity_driver::Error>,
        wide: bool,
        ahead: Vec<(usize, Vec<u16>)>,
        /// D1262: the runs again of a stale cell's members (`rerun_members`).
        reruns: Vec<Vec<u16>>,
        /// D4442: a cell whose end each run plays (nonzero): its by-reference value copied (`m8_init_copy`) and the copy's
        /// address laid in pair 0, as the members' cell end does (lowerv4 initplan lower.rs).
        copy_to: u64,
    }

    impl Rec {
        fn new(then: Option<gravity_driver::Error>) -> Rec {
            Rec { runs: Vec::new(), then, wide: false, ahead: Vec::new(), reruns: Vec::new(), copy_to: 0 } // alloc: Cold
        }

        /// The run's cell end (`copy_to`): run k's value is the varlena "value k".
        fn cell_end(&mut self) {
            if self.copy_to != 0 {
                // the run was recorded already: run k (from 0) is the (k + 1)th
                let v = format!("\x0bvalue {}", self.runs.len() + self.reruns.len() - 1); // alloc: Cold
                // SAFETY: `copy_to` is a live cell's handle (the test's), pair 0's value word inside it
                unsafe {
                    let at = abi::m8_init_copy(core::ptr::null_mut(), self.copy_to, v.as_ptr(), v.len() as u64);
                    *(self.copy_to as *mut u64).add(abi::init_pair(0) / 8) = at as u64;
                }
            }
        }
    }

    impl gravity_driver::OnDemand for Rec {
        fn run_members(&mut self, members: &[u16]) -> gravity_driver::Result<()> {
            self.runs.push(members.to_vec()); // alloc: Cold
            self.cell_end();
            self.then.take().map_or(Ok(()), Err)
        }

        fn rerun_members(&mut self, members: &[u16]) -> gravity_driver::Result<()> {
            self.reruns.push(members.to_vec()); // alloc: Cold
            self.cell_end();
            self.then.take().map_or(Ok(()), Err)
        }

        fn prerun_members(&mut self, reader: usize, members: &[u16]) -> gravity_driver::Result<bool> {
            self.ahead.push((reader, members.to_vec())); // alloc: Cold
            match self.wide {
                true => self.then.take().map_or(Ok(true), Err),
                false => Ok(false),
            }
        }
    }

    /// An eager Init's live (D1035 step 2): read by pipeline `reader`.
    fn eager(paramid: i32, members: &[u16], reader: usize) -> InitLive {
        let mut l = live(paramid, members);
        l.rt.reader = Some(reader);
        l
    }

    fn state(l: &InitLive) -> u32 {
        unsafe { (*l.cell.as_ptr().cast::<abi::InitCell>()).state }
    }

    fn cell_state(l: &InitLive) -> &AtomicU32 {
        unsafe { &*core::ptr::addr_of!((*l.cell.as_ptr().cast::<abi::InitCell>()).state).cast::<AtomicU32>() }
    }

    /// A read on another thread (a worker: no frame), as the handle word and the returned pair's address.
    fn worker_read(l: &InitLive, paramid: i32) -> (i64, String) {
        let hw = h(l);
        std::thread::scope(|s| s.spawn(move || { let r = unsafe { abi::m8_init_run(core::ptr::null_mut(), hw, paramid as u64) }; (r, message()) }).join().unwrap())
    }

    #[test]
    fn d1035_an_eager_readers_start_runs_the_members_ahead_and_a_worker_reads_the_pair() {
        let l = eager(7, &[0, 1], 3);
        let mut r = Rec::new(None);
        r.wide = true;
        l.frame(Some(frame(&mut r)));
        let mut out = gravity_parallel::LateWords::none();
        l.late_words(2, &mut out).expect("another pipeline's start");
        assert!(r.ahead.is_empty() && state(&l) == NOT_RUN, "only the reader's start runs ahead");
        l.late_words(3, &mut out).expect("the reader's start");
        assert_eq!((r.ahead.clone(), state(&l), out.len()), (vec![(3usize, vec![0u16, 1])], DONE, 0)); // alloc: Cold
        l.late_words(3, &mut out).expect("a later arm's start");
        assert_eq!(r.ahead.len(), 1, "once per execution");
        l.frame(None);
        let (at, _) = worker_read(&l, 7);
        assert_eq!(at, h(&l) as i64 + 24);
        assert!(r.runs.is_empty(), "no nested run");
    }

    #[test]
    fn v4r444_an_eager_flagged_arg_decodes_its_own_pairs_for_every_reader() {
        // the prune / rerun / write-check owner lookups: the INIT_EAGER bit is not a pair (param 8 was the first Init's)
        let arg = 1 | abi::INIT_EAGER | 7u64 << 32;
        assert_eq!((abi::init_pairs(arg), abi::init_first(arg)), (1, 7));
        assert!(abi::init_holds(arg, 7) && !abi::init_holds(arg, 8) && !abi::init_holds(arg, 6));
    }

    #[test]
    fn d1035_a_leader_only_reader_leaves_the_init_lazy() {
        let l = eager(7, &[0], 1);
        let mut r = Rec::new(None);
        l.frame(Some(frame(&mut r)));
        l.late_words(1, &mut gravity_parallel::LateWords::none()).expect("the reader's start");
        assert_eq!((r.ahead.len(), state(&l)), (1, NOT_RUN), "width 1: nothing ahead");
        let ctx: *mut Ctx = core::ptr::null_mut();
        assert_eq!(unsafe { abi::m8_init_run(ctx, h(&l), 7) }, h(&l) as i64 + 24);
        assert_eq!(r.runs, [[0u16]], "the first read runs the members nested, as before");
    }

    #[test]
    fn errorhold_a_run_aheads_error_is_the_readers_start_error_and_the_statement_rechecks() {
        let l = eager(7, &[0], 1);
        let mut r = Rec::new(Some(gravity_driver::Error::Refused("an InitPlan member's error".into())));
        r.wide = true;
        l.frame(Some(frame(&mut r)));
        let e = l.late_words(1, &mut gravity_parallel::LateWords::none()).expect_err("the run ahead's error is the reader's start's");
        assert!(e.message().contains("an InitPlan member's error"), "{}", e.message());
        assert_eq!(state(&l), FAILED);
        l.frame(None);
        let (w, _) = worker_read(&l, 7);
        assert_eq!(w, ERROR, "nothing reads a failed execution's pair");
        assert!(r.runs.is_empty(), "never run again in this execution (the re-run is a new one)");
    }

    #[test]
    fn d1035_a_cancel_in_the_run_ahead_is_the_readers_start_error() {
        // glue::pg_error's abort-class check: no interrupt pending in a unit test (a real install stays)
        static SEAM: pgsync::Once = pgsync::Once::new();
        SEAM.call_once(|| {
            if !postgres_seams::check_for_abort_interrupts::is_installed() {
                postgres_seams::check_for_abort_interrupts::set(|| Ok(()));
            }
        });
        let l = eager(7, &[0], 1);
        let mut r = Rec::new(Some(gravity_driver::Error::Cancelled));
        r.wide = true;
        l.frame(Some(frame(&mut r)));
        let e = l.late_words(1, &mut gravity_parallel::LateWords::none()).expect_err("a cancel is raised at once");
        assert_eq!(e.sqlstate(), types_error::ERRCODE_QUERY_CANCELED);
        assert_eq!(state(&l), FAILED);
    }

    fn live(paramid: i32, members: &[u16]) -> InitLive {
        let rt = Box::new(InitRt { first: paramid, members: members.into(), frame: Cell::new(None), copies: RefCell::new(Copies::new(None)), reader: None, epq: Cell::new(false), armed: RefCell::new(None), keyed: RefCell::new(ChargedVec::none()), ran: RefCell::new(ChargedVec::none()), exists: false }); // alloc: Cold
        let words = abi::init_pair(1) / 8 + 1;
        let mut cell = vec![0u64; words].into_boxed_slice(); // alloc: Cold
        unsafe {
            cell.as_mut_ptr().cast::<abi::InitCell>().write(abi::InitCell { head: abi::Head { magic: abi::INIT_MAGIC, op }, state: NOT_RUN, n: 1 });
            *cell.as_mut_ptr().add(words - 1) = &*rt as *const InitRt as u64;
        }
        InitLive { cell, rt }
    }

    fn h(l: &InitLive) -> u64 {
        l.cell.as_ptr() as u64
    }

    fn frame(r: &mut Rec) -> Frame {
        let p: *mut (dyn gravity_driver::OnDemand + '_) = r;
        // SAFETY: the test keeps `r` alive past every use of the frame.
        NonNull::new(unsafe { core::mem::transmute::<*mut (dyn gravity_driver::OnDemand + '_), *mut (dyn gravity_driver::OnDemand + 'static)>(p) }).unwrap()
    }

    fn message() -> String {
        gravity_fn::take_error().map(|e| e.message().to_owned()).unwrap_or_default()
    }

    #[test]
    fn the_first_read_runs_the_members_once_and_every_read_gets_the_pair() {
        let l = live(7, &[2, 3, 5]);
        let mut r = Rec::new(None);
        l.frame(Some(frame(&mut r)));
        let ctx: *mut Ctx = core::ptr::null_mut();
        let first = unsafe { abi::m8_init_run(ctx, h(&l), 7) };
        let again = unsafe { abi::m8_init_run(ctx, h(&l), 7) };
        l.frame(None);
        let third = unsafe { abi::m8_init_run(ctx, h(&l), 7) };
        assert_eq!(first, h(&l) as i64 + 24);
        assert_eq!((again, third), (first, first), "a done Init needs no frame and runs no member again");
        assert_eq!(r.runs, [[2u16, 3, 5]]);
        assert!(l.deferred(3) && !l.deferred(4));
    }

    #[test]
    fn d1262_a_rerun_set_rearms_a_done_cell_and_the_next_read_runs_the_members_again() {
        let l = live(7, &[2, 3]);
        let mut r = Rec::new(None);
        l.frame(Some(frame(&mut r)));
        let ctx: *mut Ctx = core::ptr::null_mut();
        rearm(cell_state(&l)).expect("a cell that never ran");
        assert_eq!(state(&l), NOT_RUN, "nothing to re-arm: the first read runs it");
        let first = unsafe { abi::m8_init_run(ctx, h(&l), 7) };
        rearm(cell_state(&l)).expect("a done cell");
        assert_eq!(state(&l), STALE, "re-armed, not run");
        assert!(r.reruns.is_empty(), "never run at the re-arm");
        let second = unsafe { abi::m8_init_run(ctx, h(&l), 7) };
        let third = unsafe { abi::m8_init_run(ctx, h(&l), 7) };
        assert_eq!((first, second, third), (h(&l) as i64 + 24, first, first));
        assert_eq!((r.runs.clone(), r.reruns.clone(), state(&l)), (vec![vec![2u16, 3]], vec![vec![2u16, 3]], DONE)); // alloc: Cold
        cell_state(&l).store(RUNNING, Ordering::Relaxed);
        assert!(rearm(cell_state(&l)).is_err(), "a re-arm inside the run errors by name");
    }

    /// D4442: a cell a rerun set re-arms frees its previous run's by-reference copies at its run again, as PostgreSQL's
    /// ExecSetParamPlan frees its curTuple / curArray: one run's copy held however many re-opens (D1262: one per outer
    /// row), never one per re-open to the execution's end; each read gets its own run's value.
    #[test]
    fn d4442_a_run_again_frees_the_previous_runs_copies() {
        let l = live(7, &[2]);
        let mut r = Rec::new(None);
        r.copy_to = h(&l);
        l.frame(Some(frame(&mut r)));
        let ctx: *mut Ctx = core::ptr::null_mut();
        for i in 0..64usize {
            if i > 0 {
                rearm(cell_state(&l)).expect("a done cell");
            }
            let pair = unsafe { abi::m8_init_run(ctx, h(&l), 7) };
            assert_eq!(pair, h(&l) as i64 + 24);
            let at = unsafe { *(pair as *const u64) } as *const u8;
            let want = format!("\x0bvalue {i}"); // alloc: Cold
            assert_eq!(unsafe { core::slice::from_raw_parts(at, want.len()) }, want.as_bytes(), "run {i} reads its own value");
            assert_eq!(l.rt.copies.borrow().len(), 1, "run {i}: one run's copy held, the earlier runs' freed");
        }
        assert_eq!((r.runs.len(), r.reruns.len()), (1, 63));
    }

    /// D4442 with D2673: an EvalPlanQual arming takes the copies its saved pairs point into, so the recheck's run again
    /// (which frees the cell's copies) leaves them alive and the put-back cell reads its own value; the next run again
    /// frees both runs' copies.
    #[test]
    fn d4442_a_rechecks_run_again_keeps_the_copies_the_put_back_pairs_point_into() {
        let l = live(7, &[2]);
        let mut r = Rec::new(None);
        r.copy_to = h(&l);
        l.frame(Some(frame(&mut r)));
        let ctx: *mut Ctx = core::ptr::null_mut();
        let pair = unsafe { abi::m8_init_run(ctx, h(&l), 7) } as *const u64;
        let first = unsafe { *pair };
        let a = super::super::raw::epq_arm(h(&l)).expect("a done cell with a frame is armed");
        assert_eq!((state(&l), l.rt.copies.borrow().len()), (STALE, 0), "the arming holds the first run's copy");
        assert!(unsafe { abi::m8_init_run(ctx, h(&l), 7) } > 0, "the recheck's read runs the members again");
        assert_ne!(unsafe { *pair }, first, "the recheck's own value");
        super::super::raw::epq_back(a);
        assert_eq!((state(&l), unsafe { *pair }), (DONE, first), "the statement's pair back");
        assert!(l.rt.copies.borrow().iter().any(|b| b.as_ptr() as u64 == first), "the copy it points into is the cell's again");
        assert_eq!(unsafe { core::slice::from_raw_parts(first as *const u8, 8) }, b"\x0bvalue 0", "and alive");
        rearm(cell_state(&l)).expect("a done cell");
        assert!(unsafe { abi::m8_init_run(ctx, h(&l), 7) } > 0);
        assert_eq!(l.rt.copies.borrow().len(), 1, "a run again frees the statement's and the recheck's copies");
    }

    #[test]
    fn d1262_a_failed_cell_stays_failed_when_rearmed() {
        let l = live(1, &[0]);
        let mut r = Rec::new(Some(gravity_driver::Error::Cancelled));
        l.frame(Some(frame(&mut r)));
        let ctx: *mut Ctx = core::ptr::null_mut();
        assert_eq!(unsafe { abi::m8_init_run(ctx, h(&l), 1) }, ERROR);
        let _ = message();
        rearm(cell_state(&l)).expect("a failed cell");
        assert_eq!(state(&l), FAILED);
        assert_eq!(unsafe { abi::m8_init_run(ctx, h(&l), 1) }, ERROR);
        assert_eq!((r.runs.len(), r.reruns.len()), (1, 0));
    }

    #[test]
    fn a_failed_run_raises_its_error_at_the_reader_and_never_runs_again() {
        let l = live(1, &[0]);
        let mut r = Rec::new(Some(gravity_driver::Error::Cancelled));
        l.frame(Some(frame(&mut r)));
        let ctx: *mut Ctx = core::ptr::null_mut();
        assert_eq!(unsafe { abi::m8_init_run(ctx, h(&l), 1) }, ERROR);
        let _ = message();
        assert_eq!(unsafe { abi::m8_init_run(ctx, h(&l), 1) }, ERROR);
        assert_eq!(message(), "gravity: m8_init_run: the InitPlan failed earlier in this execution");
        assert_eq!(r.runs.len(), 1);
    }

    #[test]
    fn a_read_without_a_frame_or_of_another_param_is_an_error_by_name() {
        let l = live(4, &[0]);
        let ctx: *mut Ctx = core::ptr::null_mut();
        assert_eq!(unsafe { abi::m8_init_run(ctx, h(&l), 4) }, ERROR);
        assert!(message().starts_with("gravity: m8_init_run: no runner frame"));
        assert_eq!(unsafe { abi::m8_init_run(ctx, h(&l), 5) }, ERROR);
        assert_eq!(message(), "gravity: m8_init_run: exec param 5 is not this InitPlan's (4 to 4)");
    }

    #[test]
    fn a_member_slot_a_non_member_holds_is_found_by_the_prepare_wall() {
        // members 0 -> 1 (slot 0 into the cell end at 1); pipeline 2 reuses slot 0 for the main query's build, 3 reads it
        let produces = |q: usize| [Some(0u8), None, Some(0), None][q];
        let consumes = |q: usize| -> &'static [u8] { [&[][..], &[0], &[], &[0]][q] };
        assert_eq!(shared_slot(4, &[0, 1], produces, consumes), Some((0, 0, 2)));
        // renumbered (the build takes slot 1): no member slot is shared
        let produces = |q: usize| [Some(0u8), None, Some(1), None][q];
        let consumes = |q: usize| -> &'static [u8] { [&[][..], &[0], &[], &[1]][q] };
        assert_eq!(shared_slot(4, &[0, 1], produces, consumes), None);
    }

    #[test]
    fn a_copy_is_verbatim_eight_aligned_and_lives_with_the_execution() {
        let l = live(1, &[0]);
        let bytes = *b"\x0bhello";
        let ctx: *mut Ctx = core::ptr::null_mut();
        let at = unsafe { abi::m8_init_copy(ctx, h(&l), bytes.as_ptr(), bytes.len() as u64) };
        assert!(at > 0 && at % 8 == 0);
        assert_eq!(unsafe { core::slice::from_raw_parts(at as *const u8, bytes.len()) }, &bytes);
        assert_eq!(l.rt.copies.borrow().len(), 1);
    }

    fn init_proto(pipeline: u16, base: u16) -> Proto {
        let mut p = Proto::new(ProtoKind::Init, pipeline);
        p.words = gravity_program::protos::Words { base, n: 1 };
        p
    }

    #[test]
    fn d2647_a_rewind_carries_each_done_cell_and_its_copies_but_never_a_rerun_sets_or_one_not_run() {
        // a ran, b ran but a rerun set holds it, c never ran
        let (a, b, c) = (live(7, &[2]), live(8, &[3]), live(9, &[4]));
        let mut r = Rec::new(None);
        [&a, &b, &c].iter().for_each(|l| l.frame(Some(frame(&mut r))));
        let ctx: *mut Ctx = core::ptr::null_mut();
        let bytes = *b"\x0bhello, world";
        let at = unsafe { abi::m8_init_copy(ctx, h(&a), bytes.as_ptr(), bytes.len() as u64) };
        assert!(unsafe { abi::m8_init_run(ctx, h(&a), 7) } > 0 && unsafe { abi::m8_init_run(ctx, h(&b), 8) } > 0);
        unsafe { *(h(&a) as *mut u64).add(abi::init_pair(0) / 8) = at as u64 };
        let protos = [init_proto(2, 0), init_proto(3, 1), Proto::new(ProtoKind::Rerun, 3), init_proto(4, 2)];
        let mut kept = carry_from(&protos, &[h(&a), h(&b), h(&c)]);
        assert_eq!(kept.0.len(), 1, "only a: b's set re-decides it, c runs at its first read");
        assert!(a.rt.copies.borrow().is_empty(), "the copies moved out with the pairs");
        drop((a, b, c));
        let (a2, b2, c2) = (live(7, &[2]), live(8, &[3]), live(9, &[4]));
        carry_in(&[h(&a2), h(&b2), h(&c2)], &mut kept).expect("the reopened cells");
        assert!(kept.0.is_empty());
        assert_eq!((state(&a2), state(&b2), state(&c2)), (DONE, NOT_RUN, NOT_RUN));
        let pair = unsafe { abi::m8_init_run(ctx, h(&a2), 7) };
        assert_eq!(pair, h(&a2) as i64 + 24, "a done cell answers with no frame and runs nothing");
        assert_eq!(unsafe { *(pair as *const u64) }, at as u64);
        assert_eq!(unsafe { core::slice::from_raw_parts(at as *const u8, bytes.len()) }, &bytes, "the copy outlived its first execution");
        assert_eq!((r.runs.len(), a2.rt.copies.borrow().len()), (2, 1));
        let mut again = carry_from(&protos, &[h(&a2), h(&b2), h(&c2)]);
        assert!(carry_in(&[0, h(&b2), h(&c2)], &mut again).is_err(), "a carried word that is no Init cell errors by name");
    }

    #[test]
    fn a_multiexpr_cells_param_k_reads_pair_k_and_read_param_runs_the_members_first() {
        let rt = Box::new(InitRt { first: 10, members: [4u16].into(), frame: Cell::new(None), copies: RefCell::new(Copies::new(None)), reader: None, epq: Cell::new(false), armed: RefCell::new(None), keyed: RefCell::new(ChargedVec::none()), ran: RefCell::new(ChargedVec::none()), exists: false }); // alloc: Cold
        let words = abi::init_pair(2) / 8 + 1;
        let mut cell = vec![0u64; words].into_boxed_slice(); // alloc: Cold
        unsafe {
            cell.as_mut_ptr().cast::<abi::InitCell>().write(abi::InitCell { head: abi::Head { magic: abi::INIT_MAGIC, op }, state: NOT_RUN, n: 2 });
            *cell.as_mut_ptr().add(words - 1) = &*rt as *const InitRt as u64;
        }
        // pair 0 (param 10): 77, not null; pair 1 (param 11): NULL
        (cell[3], cell[4], cell[5], cell[6]) = (77, 0, 0, 1);
        let l = InitLive { cell, rt };
        let mut r = Rec::new(None);
        l.frame(Some(frame(&mut r)));
        assert_eq!(read_param(h(&l), 10).expect("param 10"), (77, 0));
        assert_eq!(read_param(h(&l), 11).expect("param 11"), (0, 1));
        let ctx: *mut Ctx = core::ptr::null_mut();
        assert_eq!(unsafe { abi::m8_init_run(ctx, h(&l), 11) }, h(&l) as i64 + 40);
        assert_eq!(unsafe { abi::m8_init_run(ctx, h(&l), 12) }, ERROR);
        assert_eq!(message(), "gravity: m8_init_run: exec param 12 is not this InitPlan's (10 to 11)");
        assert_eq!(r.runs.len(), 1, "one run for both params");
        assert!(read_param(0, 10).is_err(), "a word that is no Init cell errors by name");
    }

    /// D2673: a done cell a recheck arms runs its members again at the recheck's read and goes back to the statement's run
    /// with the run's own value; no later recheck arms it. A pending cell the recheck runs goes STALE after it (the run's next
    /// read runs it again); with no frame published nothing is armed.
    #[test]
    fn d2673_a_recheck_runs_a_fresh_init_again_once_and_the_run_keeps_its_value() {
        use super::super::raw::{epq_arm, epq_back};
        let pair0 = |l: &InitLive| unsafe { (l.cell.as_ptr() as *mut u64).add(abi::init_pair(0) / 8) };
        let l = live(7, &[3]);
        assert!(epq_arm(h(&l)).is_none(), "no frame: left alone");
        let mut r = Rec::new(None);
        l.frame(Some(frame(&mut r)));
        read_param(h(&l), 7).expect("the run's read");
        unsafe { *pair0(&l) = 41 };
        let a = epq_arm(h(&l)).expect("armed");
        assert_eq!(state(&l), STALE);
        read_param(h(&l), 7).expect("the recheck's read");
        unsafe { *pair0(&l) = 42 };
        epq_back(a);
        assert_eq!(state(&l), DONE);
        assert_eq!(read_param(h(&l), 7).expect("the run's next read"), (41, 0));
        assert_eq!((r.runs.len(), r.reruns.len()), (1, 1), "one run, one run again (the recheck's)");
        assert!(epq_arm(h(&l)).is_none(), "a later recheck reads the run's value");
        let p = live(8, &[4]);
        p.frame(Some(frame(&mut r)));
        let a = epq_arm(h(&p)).expect("a pending cell is armed");
        assert_eq!(state(&p), NOT_RUN);
        epq_back(a);
        assert_eq!(state(&p), NOT_RUN, "not read by the recheck: as it was");
        let a = epq_arm(h(&p)).expect("still armable");
        read_param(h(&p), 8).expect("the recheck's read");
        epq_back(a);
        assert_eq!(state(&p), STALE, "the recheck ran it: the run's next read runs it again");
        assert!(epq_arm(h(&p)).is_none());
    }

    /// D3272: `m8_init_epq`, an IR recheck's arming: the arm is kept on the cell until its put back (a second arm keeps the
    /// first, a put back with none kept does nothing), the recheck's read runs the members again, and the put back gives
    /// the run its own value; no later recheck arms it again.
    #[test]
    fn d3272_an_ir_recheck_arms_a_cell_until_its_put_back() {
        let pair0 = |l: &InitLive| unsafe { (l.cell.as_ptr() as *mut u64).add(abi::init_pair(0) / 8) };
        let ctx: *mut Ctx = core::ptr::null_mut();
        let l = live(7, &[3]);
        let mut r = Rec::new(None);
        l.frame(Some(frame(&mut r)));
        read_param(h(&l), 7).expect("the run's read");
        unsafe { *pair0(&l) = 41 };
        assert_eq!(unsafe { abi::m8_init_epq(ctx, h(&l), 0) }, 0, "a put back with no arming kept");
        assert_eq!(state(&l), DONE);
        assert_eq!(unsafe { abi::m8_init_epq(ctx, h(&l), 1) }, 0);
        assert_eq!(unsafe { abi::m8_init_epq(ctx, h(&l), 1) }, 0, "a second arm keeps the first");
        assert_eq!(state(&l), STALE);
        read_param(h(&l), 7).expect("the recheck's read");
        unsafe { *pair0(&l) = 42 };
        assert_eq!(unsafe { abi::m8_init_epq(ctx, h(&l), 0) }, 0);
        assert_eq!(state(&l), DONE);
        assert_eq!(read_param(h(&l), 7).expect("the run's next read"), (41, 0));
        assert_eq!((r.runs.len(), r.reruns.len()), (1, 1), "one run, one run again (the recheck's)");
        assert_eq!(unsafe { abi::m8_init_epq(ctx, h(&l), 1) }, 0);
        assert_eq!(state(&l), DONE, "a later recheck reads the run's value");
        assert_eq!(unsafe { abi::m8_init_epq(ctx, h(&l), 0) }, 0);
        assert_eq!(unsafe { abi::m8_init_epq(ctx, 0, 1) }, ERROR, "a word that is no Init cell");
    }

    /// D3274: keyed armings: each EvalPlanQual state, by key and by whether another arming is open below it, runs the
    /// members again once (a later recheck of it reads the value its parent copies in), a lock's state inside a write's
    /// is its own, and `INIT_EPQ_END` ends a state so its next recheck arms afresh.
    #[test]
    fn d3274_a_keyed_state_runs_a_fresh_init_once_until_it_ends() {
        let pair0 = |l: &InitLive| unsafe { (l.cell.as_ptr() as *mut u64).add(abi::init_pair(0) / 8) };
        let ctx: *mut Ctx = core::ptr::null_mut();
        let l = live(7, &[3]);
        let mut r = Rec::new(None);
        l.frame(Some(frame(&mut r)));
        read_param(h(&l), 7).expect("the run's read");
        unsafe { *pair0(&l) = 41 };
        let op = |o: u64, key: u64| o | key << abi::INIT_EPQ_KEY_SHIFT;
        let call = |a: u64| unsafe { abi::m8_init_epq(ctx, h(&l), a) };
        assert_eq!(call(op(abi::INIT_EPQ_ARM, 1)), 0);
        assert_eq!(state(&l), STALE, "the lock's state arms it");
        read_param(h(&l), 7).expect("the lock recheck's read");
        unsafe { *pair0(&l) = 42 };
        assert_eq!(call(op(abi::INIT_EPQ_BACK, 1)), 0);
        assert_eq!(read_param(h(&l), 7).expect("the run's read"), (41, 0));
        assert_eq!(call(op(abi::INIT_EPQ_ARM, 1)), 0);
        assert_eq!(state(&l), DONE, "the same state again reads the run's value");
        assert_eq!(call(op(abi::INIT_EPQ_BACK, 1)), 0);
        assert_eq!(call(op(abi::INIT_EPQ_ARM, 2)), 0);
        assert_eq!(state(&l), STALE, "the write's state arms it");
        read_param(h(&l), 7).expect("the write recheck's read");
        unsafe { *pair0(&l) = 43 };
        assert_eq!(call(op(abi::INIT_EPQ_ARM, 1)), 0);
        assert_eq!(state(&l), STALE, "the lock's state inside the write's is its own");
        read_param(h(&l), 7).expect("the nested lock recheck's read");
        unsafe { *pair0(&l) = 44 };
        assert_eq!(call(op(abi::INIT_EPQ_BACK, 1)), 0);
        assert_eq!(read_param(h(&l), 7).expect("the write recheck's next read"), (43, 0));
        assert_eq!(call(op(abi::INIT_EPQ_BACK, 2)), 0);
        assert_eq!(read_param(h(&l), 7).expect("the run's next read"), (41, 0));
        assert_eq!(call(op(abi::INIT_EPQ_END, 1)), 0);
        assert_eq!(call(op(abi::INIT_EPQ_ARM, 1)), 0);
        assert_eq!(state(&l), STALE, "an ended state arms it afresh");
        assert_eq!(call(op(abi::INIT_EPQ_BACK, 1)), 0);
        assert_eq!(state(&l), DONE, "not read: as it was");
        assert_eq!((r.runs.len(), r.reruns.len()), (1, 3), "one run, three runs again (one per state)");
        assert_eq!(call(op(abi::INIT_EPQ_BACK, 1)), 0, "a put back with none kept");
    }

    /// An Init's by-reference copies are on the execution's prepare owner from their push to their
    /// free. A take (an EvalPlanQual arming, a rewind's carry) moves them whole with their charge, an extend (the put back,
    /// the reopened run's lay) takes the charge over, a run again's clear gives them back, and the drop ends at the base.
    #[test]
    fn e_hold_an_inits_copies_are_on_the_executions_owner_until_freed() {
        std::thread::spawn(|| {
            memledger::bind_backend(3993, 8193);
            memledger::statement_begin();
            let (o, _) = memledger::charge::context();
            let row = || memledger::charge::owner_row(o).committed;
            let base = row();
            {
                let mut c = Copies::new(Some((o, memledger::Kind::Region)));
                c.push(vec![7u64; 1000].into_boxed_slice()); // alloc: Cold
                let one = row() - base;
                assert_eq!(one as u64, c.bytes(), "the copy's block and the list's are on the owner");
                assert!(one >= 8000, "{one}");
                let armed = c.take();
                assert_eq!((c.len(), armed.len()), (0, 1), "the take moves the copies out whole");
                assert_eq!(row() - base, one, "and their bytes with them, still on the owner");
                c.push(vec![1u64; 10].into_boxed_slice()); // alloc: Cold
                let at = armed[0].as_ptr();
                c.extend(armed);
                assert_eq!(c.len(), 2);
                assert!(c.iter().any(|b| b.as_ptr() == at), "the put back keeps the copy's address");
                assert_eq!((row() - base) as u64, c.bytes(), "the put back's charge taken over, no byte twice");
                c.clear();
                assert_eq!(row(), base, "a run again's clear gives every byte back");
                c.push(vec![2u64; 100].into_boxed_slice()); // alloc: Cold
                let mut uncharged = Copies::new(None);
                uncharged.push(vec![3u64; 50].into_boxed_slice()); // alloc: Cold
                assert_eq!(uncharged.charge.committed(), 0, "no owner: the bytes stay on the thread's account");
                c.extend(uncharged);
                assert_eq!((row() - base) as u64, c.bytes(), "an uncharged carry's bytes are committed at the extend");
            }
            assert_eq!(row(), base, "the drop gives them back");
            memledger::statement_end();
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }
}
