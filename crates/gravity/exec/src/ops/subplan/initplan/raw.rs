//! `ops::subplan::initplan`'s raw reads and writes of an Init cell for a rewind's carry (PLAN-v5 §3.2 R6; D2647) and around
//! a recheck (D2673: `epq_arm` / `epq_back`): through a handle word exec's prepare laid (init.rs `lay`), its magic checked
//! before any other read. Also the cell itself (construct bundle initplan-02, moved unchanged from init.rs): its layout
//! (`lay_cell`), its state word (`own_state`), the runner's frame an Init holds (`on_demand`), the cell's operation `op`
//! with `run` / `ensure` / `copy` behind it, and L0-k's `read_param`.
#![allow(unsafe_code)]
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use gravity_abi::status::ERROR;
use gravity_abi::Ctx;
use gravity_kernels::ops::shared::object::entries as abi;
use types_error::PgResult;

use super::init::{bad, fail, Copies, InitRt, DONE, FAILED, NOT_RUN, RUNNING, STALE};
use crate::glue;
use crate::ops::Frame;

/// The Init cell `w` names, `None` for any other word. `w` is a handle word of an Init proto in a run's bound words, whose
/// protocols (the cell's owner) the caller holds alive.
fn cell(w: u64) -> Option<*mut abi::InitCell> {
    let h = w as *mut abi::Head;
    // SAFETY: a nonzero handle word is an object's head (`lay`); the magic is read before any other field
    (!h.is_null() && unsafe { (*h).magic } == abi::INIT_MAGIC).then_some(h.cast::<abi::InitCell>())
}

/// The cell's state word, its pairs (value then null word per param) and its runtime state.
///
/// # Safety
/// `c` is a live cell (`cell`): the header, `n` pairs, then the address of its `InitRt` (`lay`).
unsafe fn parts<'a>(c: *mut abi::InitCell) -> (&'a AtomicU32, *mut u64, usize, &'a InitRt) {
    let n = (*c).n as usize;
    let state = &*core::ptr::addr_of!((*c).state).cast::<AtomicU32>();
    let rt = &*(*c.cast::<u64>().add(abi::init_pair(n) / 8) as *const InitRt);
    (state, c.cast::<u8>().add(abi::init_pair(0)).cast::<u64>(), 2 * n, rt)
}

/// D2647: the pairs and the by-reference copies of the done Init cell `w` names, the copies moved out whole (the pairs keep
/// pointing into them); `None` for a word that names no cell or a cell that has not run (or is stale or failed). The run
/// that owns the cell is closing: no member runs.
pub(super) fn take_done(w: u64) -> Option<(Box<[u64]>, Copies)> {
    let c = cell(w)?;
    // SAFETY: a live cell (`cell`)
    let (state, pairs, n, rt) = unsafe { parts(c) };
    if state.load(Ordering::Acquire) != DONE {
        return None;
    }
    // SAFETY: `n` words of the live cell's pairs
    let pairs = unsafe { core::slice::from_raw_parts(pairs, n) }.into(); // alloc: Exec
    Some((pairs, rt.copies.borrow_mut().take()))
}

/// D2647: `pairs` and `copies` laid into the Init cell `w` names, which goes DONE; an error by name when `w` names no cell or
/// a cell of another width. The reopened run's prepare just laid it: nothing reads it before the run opens.
pub(super) fn lay_done(w: u64, pairs: &[u64], copies: Copies) -> Result<(), &'static str> {
    let c = cell(w).ok_or("a carried InitPlan's handle word is not its Init cell at the reopen")?;
    // SAFETY: a live cell (`cell`)
    let (state, at, n, rt) = unsafe { parts(c) };
    if pairs.len() != n {
        return Err("a carried InitPlan's pairs are not its Init cell's at the reopen");
    }
    // SAFETY: `n` words of the live cell's pairs, disjoint from `pairs` (another execution's copy)
    unsafe { core::ptr::copy_nonoverlapping(pairs.as_ptr(), at, n) };
    rt.copies.borrow_mut().extend(copies);
    state.store(DONE, Ordering::Release);
    Ok(())
}

/// D2673: an Init cell a recheck may run again (`epq_arm`), with its state and pairs before it, put back after (`epq_back`).
/// D4442: and the by-reference copies those pairs point into, moved out of the cell while the arming lasts, so a run again
/// inside the recheck (which frees the cell's copies, `ensure`) never frees what the put-back pairs point into.
pub(crate) struct Armed {
    c: *mut abi::InitCell,
    state: u32,
    pairs: Box<[u64]>,
    copies: Copies,
}

/// D2673 (PostgreSQL's EvalPlanQualStart: every SubPlan's plan is initialized afresh in the recheck's EState, its InitPlans
/// armed): the Init cell `w` names, armed for the coming recheck, its state and pairs kept: a done cell goes STALE, so the
/// recheck's first read runs the members again (`ensure`); a pending or stale one runs there as it would. `None` (left
/// alone) for a word that names no cell, a cell running or failed, one a recheck already ran again this execution (the
/// recheck EState evaluates it once; a later recheck reads the value the statement's run copies in), or one with no runner
/// frame published (no read could run the members).
pub(crate) fn epq_arm(w: u64) -> Option<Armed> {
    let c = cell(w)?;
    // SAFETY: a live cell (`cell`)
    if unsafe { parts(c) }.3.epq.get() {
        return None;
    }
    arm_cell(c)
}

/// `epq_arm` without the execution's mark: the live cell `c` armed, or `None` when it is running or failed or no runner
/// frame is published.
fn arm_cell(c: *mut abi::InitCell) -> Option<Armed> {
    // SAFETY: a live cell (`cell`)
    let (state, at, n, rt) = unsafe { parts(c) };
    let s = state.load(Ordering::Acquire);
    if !rt.has_frame() || !matches!(s, DONE | NOT_RUN | STALE) {
        return None;
    }
    // SAFETY: `n` words of the live cell's pairs
    let pairs = unsafe { core::slice::from_raw_parts(at, n) }.into(); // alloc: Cold
    if s == DONE {
        state.store(STALE, Ordering::Release);
    }
    let copies = rt.copies.borrow_mut().take();
    Some(Armed { c, state: s, pairs, copies })
}

/// D2673: after the recheck, the statement's run gets its cell back: its pairs and state as `epq_arm` found them, but a
/// cell the recheck ran that was not done before goes STALE (its members ran: the statement's next read runs them again,
/// as the parent EState evaluates its own pending InitPlan). A cell the recheck ran is marked (`InitRt::epq`): no later
/// recheck arms it. A failed run stays failed (its error is the recheck's).
pub(crate) fn epq_back(a: Armed) {
    // SAFETY: the live cell `epq_arm` found (the execution's protocols outlive its rechecks)
    let rt = unsafe { parts(a.c) }.3;
    let ran = put_back(a);
    rt.epq.set(rt.epq.get() || ran);
}

/// `epq_back` without the execution's mark: whether the recheck ran the members (false for a failed run, left failed).
fn put_back(a: Armed) -> bool {
    // SAFETY: the live cell `epq_arm` found (the execution's protocols outlive its rechecks)
    let (state, at, n, rt) = unsafe { parts(a.c) };
    // D4442: the copies the restored pairs point into go back to the cell; the recheck's own copies stay beside them (its
    // rows were the recheck's) until the cell's next run again or the execution's end
    rt.copies.borrow_mut().extend(a.copies);
    let ran = match state.load(Ordering::Acquire) {
        DONE => true,
        NOT_RUN | STALE => false,
        _ => return false,
    };
    // SAFETY: `n` words of the live cell's pairs, disjoint from `a.pairs` (a copy)
    unsafe { core::ptr::copy_nonoverlapping(a.pairs.as_ptr(), at, n.min(a.pairs.len())) };
    let back = if ran && a.state != DONE { STALE } else { a.state };
    state.store(back, Ordering::Release);
    ran
}

/// One Init cell laid in `cell` (its words: the InitCell header for `n` pairs, the pairs, then the address of its runtime
/// state `rt`), the head's operation `op`. init.rs `lay` sizes `cell` (`init_pair(n) / 8 + 1` words) and keeps `rt` and the
/// cell alive together until the execution's end.
pub(super) fn lay_cell(cell: &mut [u64], n: usize, rt: &InitRt) {
    let nwords = cell.len();
    // SAFETY: `cell` holds `nwords` 8-aligned words: the InitCell header, `n` pairs, then the state's address.
    unsafe {
        let c = cell.as_mut_ptr();
        c.cast::<abi::InitCell>().write(abi::InitCell { head: abi::Head { magic: abi::INIT_MAGIC, op }, state: NOT_RUN, n: n as u32 });
        c.add(nwords - 1).write(rt as *const InitRt as u64);
    }
}

/// The state word of an Init's own cell (`lay_cell`'s), for the leader's late step (D1035 step 2, init.rs `late_words`).
pub(super) fn own_state(cell: &[u64]) -> &AtomicU32 {
    // SAFETY: the cell's state word; nothing runs the members concurrently (the leader's late step, no call on the stack)
    unsafe { &*core::ptr::addr_of!((*cell.as_ptr().cast::<abi::InitCell>()).state).cast::<AtomicU32>() }
}

/// The runner's frame `f` an Init holds (published by the runner on this thread while it runs the program), for one call
/// made inside one of the runner's calls or late steps, nothing else using the frame meanwhile.
pub(crate) fn on_demand(f: &mut Frame) -> &mut (dyn gravity_driver::OnDemand + 'static) {
    // SAFETY: the runner's frame, published on this thread while it runs the program (as `ensure`'s)
    unsafe { f.as_mut() }
}

/// The Init cell's `Head::op`: `OP_INIT_RUN` (`a` = the exec param), `OP_INIT_COPY` (`a` = the bytes, `b` = their length)
/// and `OP_INIT_EPQ` (`a` nonzero: arm, else put back).
pub(super) unsafe fn op(h: *mut abi::Head, _ctx: *mut Ctx, code: u64, a: u64, b: u64) -> i64 {
    let cell = h.cast::<abi::InitCell>();
    let n = (*cell).n as usize;
    // SAFETY: the trailing word names the state `prepare` laid beside this cell; both live until the execution's end.
    let rt = &*(*h.cast::<u64>().add(abi::init_pair(n) / 8) as *const InitRt);
    match code {
        abi::OP_INIT_RUN => run(cell, rt, a as u32 as i32),
        abi::OP_INIT_COPY => copy(rt, a as *const u8, b as usize),
        abi::OP_INIT_EPQ => match (a >> abi::INIT_EPQ_KEY_SHIFT) as u32 {
            0 => epq(cell, rt, a != 0),
            key => keyed(cell, rt, a & ((1 << abi::INIT_EPQ_KEY_SHIFT) - 1), key),
        },
        _ => fail(&format!("an Init cell has no operation {code}")), // alloc: Cold
    }
}

/// D3272 `OP_INIT_EPQ` (`m8_init_epq`, an EvalPlanQual recheck in the IR): `arm` arms the cell for the recheck (`epq_arm`,
/// the arming kept in `InitRt::armed` until its put back; nothing when one is kept already or `epq_arm` leaves the cell
/// alone), else puts it back (`epq_back` of the kept arming; none kept, nothing). 0.
unsafe fn epq(cell: *mut abi::InitCell, rt: &InitRt, arm: bool) -> i64 {
    if arm {
        if rt.armed.borrow().is_none() {
            let a = epq_arm(cell as u64);
            *rt.armed.borrow_mut() = a;
        }
    } else {
        let a = rt.armed.borrow_mut().take();
        if let Some(a) = a {
            epq_back(a);
        }
    }
    0
}


/// D3274 `OP_INIT_EPQ` with a state key (`key`, nonzero): PostgreSQL keeps one recheck EState per EvalPlanQual state (a
/// LockRows' or a ModifyTable's EPQState), from its EvalPlanQualStart to its EvalPlanQualEnd (a LockRows at its input's
/// end), and each initializes every SubPlan afresh once: an InitPlan in one runs again at that state's first read, and a
/// later recheck of the same state reads the value its parent copies in. A lock's recheck inside a write's recheck is a
/// state of its own (the write's estate holds its own LockRows), so the key comes with whether another arming is open
/// below it (`nested`). `INIT_EPQ_ARM` arms the cell (`arm_cell`) unless that state ran it, an arming kept on the stack
/// (`InitRt::keyed`) until its put back; `INIT_EPQ_BACK` puts the innermost back (`put_back`), marking its state when its
/// recheck ran the members (`InitRt::ran`); `INIT_EPQ_END` ends the state at the current depth, so its next recheck arms
/// afresh. 0.
unsafe fn keyed(cell: *mut abi::InitCell, rt: &InitRt, op: u64, key: u32) -> i64 {
    let nested = !rt.keyed.borrow().is_empty();
    match op {
        abi::INIT_EPQ_ARM => {
            // the room first, so a refusal leaves the cell as it was (never an arming dropped with its copies)
            if rt.keyed.borrow_mut().try_reserve(1).is_err() {
                return fail("m8_init_epq: no room for a keyed recheck's arming");
            }
            let a = match rt.ran.borrow().contains(&(key, nested)) {
                true => None,
                false => arm_cell(cell),
            };
            if rt.keyed.borrow_mut().push(a.map(|a| (a, nested))).is_err() {
                return fail("m8_init_epq: no room for a keyed recheck's arming");
            }
        }
        abi::INIT_EPQ_BACK => {
            let top = rt.keyed.borrow_mut().with_vec(|v| v.pop()).flatten();
            if let Some((a, below)) = top {
                if put_back(a) && rt.ran.borrow_mut().push((key, below)).is_err() {
                    return fail("m8_init_epq: no room for a recheck state's mark");
                }
            }
        }
        _ => rt.ran.borrow_mut().with_vec(|v| v.retain(|&e| e != (key, nested))),
    }
    0
}

/// `OP_INIT_RUN`: the address of `paramid`'s pair (`ensure`), or ERROR with the error stashed for the reader's host return.
unsafe fn run(cell: *mut abi::InitCell, rt: &InitRt, paramid: i32) -> i64 {
    match ensure(cell, rt, paramid) {
        Ok(pair) => pair,
        Err(e) => {
            gravity_fn::stash_error(e);
            ERROR
        }
    }
}

/// The address of `paramid`'s pair once this execution's run is done: run now (nested, through the frame), or earlier.
unsafe fn ensure(cell: *mut abi::InitCell, rt: &InitRt, paramid: i32) -> PgResult<i64> {
    let n = (*cell).n as i64;
    let k = paramid as i64 - rt.first as i64;
    if !(0..n).contains(&k) {
        return Err(bad(&format!("m8_init_run: exec param {paramid} is not this InitPlan's ({} to {})", rt.first, rt.first as i64 + n - 1))); // alloc: Cold
    }
    // SAFETY: `state` is a u32 of the live cell, written only here (the leader, RT-1).
    let state = &*core::ptr::addr_of_mut!((*cell).state).cast::<AtomicU32>();
    let pair = cell as i64 + abi::init_pair(k as usize) as i64;
    match state.load(Ordering::Acquire) {
        DONE => return Ok(pair),
        RUNNING => return Err(bad("m8_init_run: an InitPlan's param read inside its own run")),
        FAILED => return Err(bad("m8_init_run: the InitPlan failed earlier in this execution")),
        _ => {}
    }
    let Some(mut f) = rt.frame.get() else {
        return Err(bad("m8_init_run: no runner frame (bug: a read on a worker before the Init ran; D1035: the lowering pins every reader to the leader but an eager Init's, whose members run ahead of it)"));
    };
    let again = state.load(Ordering::Acquire) == STALE;
    state.store(RUNNING, Ordering::Relaxed);
    if again {
        // D4442 (PG's ExecSetParamPlan frees the previous curTuple / curArray as it sets the params again): the previous
        // run's by-reference copies are freed before the run again, so a cell holds one run's values however many times a
        // rerun set re-opens it (D1262: once per outer row), not one copy per re-open to the execution's end. Nothing still
        // reads them: the cell's pairs are this run's next, the re-arm cleared the readers' frame pairs (rerun/raw.rs
        // `rearm_inits`), a consumer that keeps a value past its call copies it (a spool's copies, `m8_sub_copy`), and an
        // EvalPlanQual arming took the copies its saved pairs point into (`arm_cell`).
        rt.copies.borrow_mut().clear();
    }
    exists_false(cell, rt);
    // SAFETY: the frame is the runner's, published on this thread while it runs the program (gravity_parallel `publish`,
    // whose guard takes it back on every exit, an unwind included); this read is inside one of that runner's calls or
    // late steps, and nothing else uses the frame while the members run. D1262: a stale run's members ran before, each
    // kept slot reset in place first (`rerun_members`).
    let ran = if again { f.as_mut().rerun_members(&rt.members) } else { f.as_mut().run_members(&rt.members) };
    match ran {
        Ok(()) => {
            state.store(DONE, Ordering::Release);
            Ok(pair)
        }
        Err(e) => {
            state.store(FAILED, Ordering::Relaxed);
            Err(glue::pg_error(e))
        }
    }
}

/// D2998: an EXISTS InitPlan's run starts with its pair false and not NULL; its rows store only true (lowerv4 initplan
/// lower.rs), so a section's worker that saw no row leaves the pair alone.
/// # Safety
/// `cell` is a live Init cell of at least one pair; nothing reads or writes its pairs while its run starts (the leader).
unsafe fn exists_false(cell: *const abi::InitCell, rt: &InitRt) {
    if rt.exists {
        let pair = cell.cast::<u8>().add(abi::init_pair(0)).cast::<AtomicU64>();
        (*pair).store(0, Ordering::Relaxed);
        (*pair.add(1)).store(0, Ordering::Relaxed);
    }
}

/// `exists_false` on an Init's own cell (`lay_cell`'s), for the leader's late step (D1035 step 2, init.rs `late_words`).
pub(super) fn exists_false_own(cell: &[u64], rt: &InitRt) {
    // SAFETY: `cell` is `lay_cell`'s: the header, then `n >= 1` pairs (`lay` refuses 0), then the state's address
    unsafe { exists_false(cell.as_ptr().cast::<abi::InitCell>(), rt) }
}

/// L0-k: the value and null of exec param `paramid` from the Init cell at `cell` (a handle word exec's prepare laid), its
/// members run first if this execution has not run them: exec pruning's read, as PostgreSQL's ExecFindMatchingSubPlans
/// evaluates an InitPlan param (the pair: the value word, then the null word, nonzero for NULL).
pub(crate) fn read_param(cell: u64, paramid: i32) -> PgResult<(u64, u8)> {
    let h = cell as *mut abi::Head;
    // SAFETY: a nonzero handle word is a cell `lay` made for this execution (its magic checked before any other read);
    // the cell and its state live until the execution's end.
    unsafe {
        if h.is_null() || (*h).magic != abi::INIT_MAGIC {
            return Err(bad(&format!("src.prune: exec param {paramid}'s word is not an Init cell ({cell:#x})"))); // alloc: Cold
        }
        let c = h.cast::<abi::InitCell>();
        let rt = &*(*h.cast::<u64>().add(abi::init_pair((*c).n as usize) / 8) as *const InitRt);
        let pair = ensure(c, rt, paramid)? as *const u64;
        Ok((*pair, (*pair.add(1) != 0) as u8))
    }
}

/// A by-reference result copied verbatim into the Init's copies (8-aligned; until the cell's next run again, D4442, or the
/// execution's end).
unsafe fn copy(rt: &InitRt, ptr: *const u8, len: usize) -> i64 {
    if ptr.is_null() && len > 0 {
        return fail("m8_init_copy: a null value of nonzero length");
    }
    // The block is the copies' from here (`Copies::push` charges it to the execution's owner before the call returns)
    let mut buf = vec![0u64; len.div_ceil(8).max(1)].into_boxed_slice(); // alloc: Exec charged rt.copies
    if len > 0 {
        core::ptr::copy_nonoverlapping(ptr, buf.as_mut_ptr().cast::<u8>(), len);
    }
    let at = buf.as_ptr() as i64;
    rt.copies.borrow_mut().push(buf);
    at
}
