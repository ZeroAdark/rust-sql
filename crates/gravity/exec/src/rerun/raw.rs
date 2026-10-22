//! The Rerun protocol's unsafe steps (rule R6, the module's raw home): the set's object laid in its words, the object's
//! `Head::op`, the InitPlan cells and frame words read through the caller's context, and the runner's frame borrowed for
//! one nested run of the members.
#![allow(unsafe_code)]

use gravity_abi::status::ERROR;
use gravity_abi::Ctx;
use gravity_kernels::ops::shared::object::entries as abi;
use types_error::PgResult;

use super::{bad, decide, pull, run, RerunRt, WORDS};
use crate::ops::Frame;

/// The set's object for `rt` (`lay`): the `Head` (RERUN_MAGIC, `op`), then the state's address. The state lives in the
/// protocol's `Live` beside the object, both until the execution's end.
pub(super) fn object(rt: &RerunRt) -> Box<[u64]> {
    let mut cell = vec![0u64; WORDS].into_boxed_slice(); // alloc: Exec
    // SAFETY: `cell` holds WORDS 8-aligned words: the Head, then the state's address.
    unsafe {
        let c = cell.as_mut_ptr();
        c.cast::<abi::Head>().write(abi::Head { magic: abi::RERUN_MAGIC, op });
        c.add(WORDS - 1).write(rt as *const RerunRt as u64);
    }
    cell
}

/// D1262 (`decide`): each InitPlan the set holds re-armed (`init::rearm`) and its params' frame-word pairs cleared, so its
/// next read runs it again, as PG's ExecReScanSetParamPlan re-decides an InitPlan lazily. `ctx` is null or the caller's
/// context, bound for this call; `words_site` is that program's words site.
pub(super) fn rearm_inits(ctx: *mut Ctx, words_site: usize, inits: &[(usize, i32)]) -> PgResult<()> {
    if ctx.is_null() || words_site == usize::MAX {
        return Err(bad("m8_rerun: an InitPlan to re-arm with no frame words (D1262)"));
    }
    // SAFETY: as `frame_param`'s: the caller's bound context; its params hold the handle words (a nonzero one is the
    // Init cell exec's prepare laid, its magic checked before any other read), its words site's keys the frame words,
    // one pair per param id up to the largest a producer sets (lowerv4 params.rs `reserve`).
    unsafe {
        let at = ctx as *const u8;
        let params = *at.add(gravity_abi::ctx::hdr::PARAMS).cast::<*const u64>();
        let keys = *at.add(gravity_abi::ctx::site_off(words_site) + gravity_abi::site::site::KEYS).cast::<*mut u64>();
        for &(h, first) in inits.iter() {
            let c = *params.add(h) as *mut abi::InitCell;
            if c.is_null() || (*c).head.magic != abi::INIT_MAGIC {
                return Err(bad("m8_rerun: an InitPlan's handle word is not its Init cell (D1262)"));
            }
            crate::ops::subplan::initplan::init::rearm(&*core::ptr::addr_of_mut!((*c).state).cast::<core::sync::atomic::AtomicU32>())?;
            for id in (first..).take((*c).n as usize) {
                *keys.add(2 * id as usize + 1) = 0;
            }
        }
    }
    Ok(())
}

/// Exec param `id`'s value and null from the frame words of `ctx` (its words site's keys). `ctx` is null or the caller's
/// context, bound for this call; `ws` is that program's words site.
pub(super) fn frame_param(ctx: *mut Ctx, ws: usize, id: i32) -> PgResult<(u64, u8)> {
    if ctx.is_null() || ws == usize::MAX || id < 0 {
        return Err(bad(&format!("m8_rerun: src.prune reads exec param {id} with no frame words (L0-h)"))); // alloc: Cold
    }
    // SAFETY: `ctx` is the caller's context, bound while its call runs; site `ws` is the program's words site, whose keys
    // array holds the frame words (lowerv4 frame.rs: `env.words` is that pointer).
    let (v, f) = unsafe {
        let keys = *(ctx as *const u8).add(gravity_abi::ctx::site_off(ws) + gravity_abi::site::site::KEYS).cast::<*const u64>();
        (*keys.add(2 * id as usize), *keys.add(2 * id as usize + 1))
    };
    if f & 1 == 0 {
        return Err(bad(&format!("m8_rerun: exec param {id} read by src.prune before its owner set it (L0-h)"))); // alloc: Cold
    }
    Ok((v, (f & 2 != 0) as u8))
}

/// The object's `Head::op`: `OP_RERUN`, `OP_RERUN_REARM` (D2611: `decide`'s re-arm ahead of a gated re-open) and
/// `OP_RERUN_PULL` (D2034: `a` the budget, `b` nonzero on a generation's first pull).
///
/// # Safety
/// `h` is an object `object` made (its last word the live `RerunRt`), and `ctx` the caller's context, bound for this call.
unsafe fn op(h: *mut abi::Head, ctx: *mut Ctx, code: u64, a: u64, b: u64) -> i64 {
    // SAFETY: the trailing word names the state `lay` made beside this object; both live until the execution's end.
    let rt = &*(*h.cast::<u64>().add(WORDS - 1) as *const RerunRt);
    match code {
        abi::OP_RERUN => match run(rt, ctx) {
            Ok(()) => 0,
            Err(e) => {
                gravity_fn::stash_error(e);
                ERROR
            }
        },
        abi::OP_RERUN_REARM => match decide(rt, ctx, true) {
            Ok(()) => 0,
            Err(e) => {
                gravity_fn::stash_error(e);
                ERROR
            }
        },
        abi::OP_RERUN_PULL => match pull(rt, ctx, a, b != 0) {
            Ok(s) => s,
            Err(e) => {
                gravity_fn::stash_error(e);
                ERROR
            }
        },
        _ => {
            gravity_fn::stash_error(bad(&format!("a rerun set has no operation {code}"))); // alloc: Cold
            ERROR
        }
    }
}

/// The members run nested, in program order, through the runner's frame (`OnDemand::rerun_members`, RT-2e; `run_set`).
pub(super) fn rerun_members(mut frame: Frame, members: &[u16]) -> gravity_driver::Result<()> {
    // SAFETY: the frame is the runner's, published on this thread while it runs the program (gravity_parallel `publish`,
    // whose guard takes it back on every exit, an unwind included); this call is inside one of that runner's calls, and
    // nothing else uses the frame while the members run.
    unsafe { frame.as_mut() }.rerun_members(members)
}

/// D4120: `rerun_members` of a stale set, its kept members filled again (`OnDemand::refill_members`).
pub(super) fn refill_members(mut frame: Frame, members: &[u16]) -> gravity_driver::Result<()> {
    // SAFETY: as `rerun_members`'s: the runner's frame, published on this thread for its run, used by nothing else meanwhile.
    unsafe { frame.as_mut() }.refill_members(members)
}

/// D4120 (`run`): each nested set `words` names (its handle word) whose kept members a run filled is marked stale, so its
/// next run fills them again over the params this set's owner just changed. `ctx` is null or the caller's context, bound
/// for this call. A word that is no rerun object, or a set inside its own run, is a Bug by name.
pub(super) fn stale_sets(ctx: *mut Ctx, words: &[usize]) -> PgResult<()> {
    if words.is_empty() {
        return Ok(());
    }
    if ctx.is_null() {
        return Err(bad("m8_rerun: a nested set to mark stale with no context (D4120)"));
    }
    for &h in words {
        // SAFETY: the caller's bound context (above); its params hold the handle words, a nonzero one the object's head,
        // its magic checked before any other read
        let o = unsafe { *(*(ctx as *const u8).add(gravity_abi::ctx::hdr::PARAMS).cast::<*const u64>()).add(h) } as *const abi::Head;
        // SAFETY: as above: a nonzero handle word is a laid object's head
        if o.is_null() || unsafe { (*o).magic } != abi::RERUN_MAGIC {
            return Err(bad("m8_rerun: a nested set's handle word is not its rerun set (D4120)"));
        }
        // SAFETY: a rerun object (magic checked) `object` made: its last word names the live `RerunRt` laid beside it
        let rt = unsafe { &*(*o.cast::<u64>().add(WORDS - 1) as *const RerunRt) };
        if rt.running.get() {
            return Err(bad("m8_rerun: a nested set marked stale inside its own run (D4120)"));
        }
        if rt.filled.get() {
            rt.stale.set(true);
        }
    }
    Ok(())
}

/// D2034: a recursive union's term set re-armed as a pulled set, its body's pull state and kept slot reset
/// (`OnDemand::rearm_pulled`), so its next pull runs the body from its start.
pub(super) fn rearm_pulled(mut frame: Frame, members: &[u16]) -> gravity_driver::Result<()> {
    // SAFETY: as `rerun_members`'s: the runner's frame, published on this thread for its run, used by nothing else meanwhile.
    unsafe { frame.as_mut() }.rearm_pulled(members)
}

/// D2034: a recursive union's term set pulled on until its body's end took the rows its want word allows, or ended
/// (`OnDemand::pull_members`, RT-2f).
pub(super) fn pull_members(mut frame: Frame, members: &[u16], budget: u64) -> gravity_driver::Result<gravity_driver::Pulled> {
    // SAFETY: as `rerun_members`'s: the runner's frame, published on this thread for its run, used by nothing else meanwhile.
    unsafe { frame.as_mut() }.pull_members(members, budget)
}
