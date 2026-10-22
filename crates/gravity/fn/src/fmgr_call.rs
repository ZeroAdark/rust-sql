//! The function-manager tier (docs/gravity/expr.md §2.1, tier C): PostgreSQL's call-info protocol, built once per bind, written per call.
//! `bind` resolves the `FmgrInfo` (a builtin without the catalog; a user function through the syscache) and arms the site's result context
//! (a child of the statement's grant context under `bind_under`, its `ResultLife` decided there); `call` writes the arguments and null flags into
//! the persistent frame, invokes (the incumbent's `execexpr::interp::invoke`, `invoke_fusage` when `track_functions` covers the site) and reads the result.
use core::ptr::NonNull;

use datum::{Datum, NullableDatum};
use fmgr::{FmgrInfo, FunctionCallInfoBaseData, LocalFcinfo, PGFunctionThin};
use mcx::{Mcx, MemoryContext};
use types_core::Oid;
use types_error::{PgError, PgResult};

use crate::result_life::{fmgr_life, scribble, ResultLife};
use crate::{fail, null_ret, ok, ErrSlot, Ret};

/// PostgreSQL's FUNC_MAX_ARGS (pg_config_manual.h), from its one home in types_core.
pub use types_core::FUNC_MAX_ARGS;
pub const RET_UNKNOWN: i16 = i16::MIN;
pub const FMGR_WINDOW_RETAIN: usize = 4 << 20;

pub struct FmgrCall {
    flinfo: FmgrInfo,
    /// D3620: `frame`'s, sized for the call's arguments (not `FUNC_MAX_ARGS`'s 1.6 KB a call).
    fc: Box<FunctionCallInfoBaseData>,
    cx: Box<MemoryContext>,
    thin: Option<PGFunctionThin>,
    nargs: usize,
    collation: Oid,
    fn_expr: Option<Box<dyn core::any::Any>>,
    fusage: bool,
    life: ResultLife,
    ret_typlen: i16,
    scribble: bool,
}

/// D3620: a call's frame with room for `nargs` arguments, from 8 / 16 / `FUNC_MAX_ARGS` slots (C's
/// `SizeForFunctionCallInfo(nargs)` sizes it exactly). The frame was `FUNC_MAX_ARGS` slots for every call, ~1.6 KB each:
/// a hosted program's owner holds one per call site, and every level of a deep SQL-function recursion holds its owner.
/// The smallest frame has 8 slots: the widest fixed arity a builtin reads through `args_n` is 7, so a body that reads its
/// arguments as one array never meets a frame shorter than the array (`args_n` panics on one).
fn frame(nargs: usize, collation: Oid) -> Box<FunctionCallInfoBaseData> {
    fn sized<const N: usize>(collation: Oid) -> Box<FunctionCallInfoBaseData> {
        let mut fc = Box::new(LocalFcinfo::<N>::new(collation)); // alloc: Node
        fc.rearm(collation);
        fc
    }
    match nargs {
        0..=8 => sized::<8>(collation),
        9..=16 => sized::<16>(collation),
        _ => sized::<FUNC_MAX_ARGS>(collation),
    }
}

impl FmgrCall {
    /// C `fmgr_info_set_expr`: the calling node for the polymorphic readers (`get_fn_expr_argtype`).
    pub fn set_fn_expr<T: core::any::Any>(&mut self, node: T) {
        let boxed = Box::new(node); // alloc: Program
        // SAFETY: the box is owned by this call and freed after the `FmgrInfo` (drop order).
        self.flinfo.fn_expr = Some(unsafe { types_core::fmgr::FnExprErased::from_node_ref(&*boxed) });
        self.fn_expr = Some(boxed);
    }

    pub fn bind(proc_oid: Oid, nargs: usize, collation: Oid) -> PgResult<FmgrCall> {
        Self::bind_in(proc_oid, nargs, collation, Box::new(MemoryContext::new_bump("gravity_fn fmgr window")), RET_UNKNOWN) // alloc: Node
    }

    /// Result context under `parent` (G, or the statement's context when unadmitted; outlives the call); `ret_typlen` 0 = by value.
    pub fn bind_under(proc_oid: Oid, nargs: usize, collation: Oid, parent: &MemoryContext, ret_typlen: i16) -> PgResult<FmgrCall> {
        Self::bind_in(proc_oid, nargs, collation, Box::new(parent.new_child_bump("gravity_fn fmgr result")), ret_typlen) // alloc: Node
    }

    /// `bind_under` for a per-window site: the keeper sized for `rows` x `width`, extra blocks kept across `reset_window`.
    pub fn bind_under_window(
        proc_oid: Oid,
        nargs: usize,
        collation: Oid,
        parent: &MemoryContext,
        ret_typlen: i16,
        rows: usize,
        width: usize,
    ) -> PgResult<FmgrCall> {
        let cx = parent.new_child_bump_window("gravity_fn fmgr window", rows.saturating_mul(width), FMGR_WINDOW_RETAIN);
        Self::bind_in(proc_oid, nargs, collation, Box::new(cx), ret_typlen) // alloc: Node
    }

    /// M6-HOSTED: `bind` of a comparator `crate::resolved::resolve` resolved at lowering, by its id: a copy of that
    /// resolution, no catalog access (a pool worker's). `None` for an id no `resolve` gave.
    pub fn bind_resolved(id: u32, nargs: usize, collation: Oid) -> Option<FmgrCall> {
        let flinfo = crate::resolved::info(id)?;
        let cx = Box::new(MemoryContext::new_bump("gravity_fn fmgr window")); // alloc: Node
        Some(Self::bind_info(flinfo, nargs, collation, cx, RET_UNKNOWN))
    }

    fn bind_in(proc_oid: Oid, nargs: usize, collation: Oid, cx: Box<MemoryContext>, ret_typlen: i16) -> PgResult<FmgrCall> {
        let flinfo = fmgr_core::fmgr_info(proc_oid)?;
        Ok(Self::bind_info(flinfo, nargs, collation, cx, ret_typlen))
    }

    fn bind_info(flinfo: FmgrInfo, nargs: usize, collation: Oid, cx: Box<MemoryContext>, ret_typlen: i16) -> FmgrCall {
        assert!(nargs <= FUNC_MAX_ARGS, "gravity_fn: {nargs} arguments");
        let proc_oid = flinfo.fn_oid;
        let fusage = tracks(&flinfo);
        let thin = fmgr_core::fmgr_thin_builtin(&flinfo, nargs as i16);
        let life = if ret_typlen == RET_UNKNOWN { ResultLife::UntilNextCall } else { fmgr_life(proc_oid, ret_typlen == 0) };
        let mut fc = frame(nargs, collation);
        fc.nargs = nargs as i16;
        // SAFETY: the context box moves into the same FmgrCall as the frame and outlives it (field order).
        unsafe { fc.set_result_mcx(cx.mcx()) };
        FmgrCall { flinfo, fc, cx, thin, nargs, collation, fn_expr: None, fusage, life, ret_typlen, scribble: scribble::on() }
    }

    /// B2: drop the memo this call's body kept in `fn_extra` (the next call rebuilds it, as after `fmgr_info`): a pool
    /// lane's twin entered under a new claim generation, whose catalog images the old memo may point into.
    pub fn reset_extra(&mut self) {
        self.flinfo.fn_extra = None;
    }

    pub fn result_life(&self) -> ResultLife {
        self.life
    }

    /// M8 (D1036): this call again for a nested hosted lane (a run nested inside an outer row on the same thread: an
    /// InitPlan's members, a lazy build; PostgreSQL's separate ExprContext): the same resolved function (C
    /// `fmgr_info_copy`: `fn_extra` reset, the calling node shared), collation and result life, a frame and a result
    /// context of its own, so its calls and window resets never touch this call's result.
    pub fn twin(&self) -> FmgrCall {
        self.twin_in(Box::new(MemoryContext::new_bump("gravity_fn fmgr nested"))) // alloc: Node
    }

    /// D2983: `twin` for a pool lane: made on the pool thread at the lane's first entry and dropped with the
    /// program on the leader, so its result context is unobserved (mcx `new_unobserved`: no entry in either thread's forest).
    pub fn twin_pool(&self) -> FmgrCall {
        self.twin_in(Box::new(MemoryContext::new_unobserved_wholesale("gravity_fn fmgr pool lane"))) // alloc: Node
    }

    fn twin_in(&self, cx: Box<MemoryContext>) -> FmgrCall {
        let mut fc = frame(self.nargs, self.collation);
        fc.nargs = self.nargs as i16;
        // SAFETY: the context is owned by the twin and outlives its frame (field order).
        unsafe { fc.set_result_mcx(cx.mcx()) };
        FmgrCall { flinfo: self.flinfo.clone(), fc, cx, thin: self.thin, nargs: self.nargs, collation: self.collation, fn_expr: None, fusage: self.fusage, life: self.life, ret_typlen: self.ret_typlen, scribble: self.scribble }
    }

    /// M8 (D1036): `f`'s `twin`, kept in `calls` for a nested lane's life; its address, null for a null `f`. D2983: a pool
    /// lane's (`twin_pool`) when `pool`.
    /// # Safety — `f` is null or a live call.
    pub unsafe fn twin_into(f: *mut FmgrCall, calls: &mut Vec<Box<FmgrCall>>, pool: bool) -> *mut FmgrCall {
        let Some(f) = f.as_ref() else { return core::ptr::null_mut() };
        let mut t = Box::new(if pool { f.twin_pool() } else { f.twin() }); // alloc: Node
        let at: *mut FmgrCall = &mut *t;
        calls.push(t); // alloc: Node
        at
    }

    /// The result context's retained blocks (the per-row bound).
    pub fn result_bytes(&self) -> usize {
        self.cx.subtree_allocated()
    }

    /// The call is bracketed by the function-usage counters (`track_functions` covered the site at bind or at the last
    /// `retrack`).
    pub fn tracked(&self) -> bool {
        self.fusage
    }

    /// `track_functions` read again (D412): a cached program's call follows the session's current setting, as C's
    /// ExecInitFunc picks the fusage step per execution. Called once per program entry, never per call.
    #[inline]
    pub fn retrack(&mut self) {
        self.fusage = tracks(&self.flinfo);
    }

    /// The session's `track_functions` as `retrack` reads it: a caller holding many calls retracks them only when it moved
    /// since their last `retrack` (D3196 rearm E4).
    #[inline]
    pub fn track_level() -> i32 {
        pgstat::function::pgstat_track_functions()
    }

    /// D2835: a call PostgreSQL makes through `FunctionCall2Coll`, never bracketed by the function-usage counters whatever
    /// `track_functions` says (a sort or merge comparator: tuplesort's `comparison_shim`). Never `retrack`ed.
    pub fn untracked(mut self) -> FmgrCall {
        self.fusage = false;
        self
    }

    pub fn strict(&self) -> bool {
        self.flinfo.fn_strict
    }

    /// The bound function's oid (`fn_oid`), as its errors name it.
    pub fn oid(&self) -> Oid {
        self.flinfo.fn_oid
    }

    pub fn nargs(&self) -> usize {
        self.nargs
    }

    pub fn collation(&self) -> Oid {
        self.collation
    }

    /// PostgreSQL's contract: a by-reference result is fresh in `mcx` (or one of the arguments); it lives as long as `mcx`
    /// is neither reset nor dropped. The site's own context (`bind_under`, placement by `ResultLife`) is `call_site`.
    #[inline]
    pub fn call<'r>(&mut self, mcx: Mcx<'r>, args: &[Datum], nulls: &[bool]) -> PgResult<NullableDatum> {
        // SAFETY: armed for this call only; the frame is re-armed before every later call (call / call_site).
        unsafe { self.fc.set_result_mcx(mcx) };
        self.call_armed(args, nulls)
    }

    /// The result in the site's own context (`bind` / `bind_under`): live until `reset_window` or the next call's reset.
    #[inline]
    pub fn call_site(&mut self, args: &[Datum], nulls: &[bool]) -> PgResult<NullableDatum> {
        self.arm_site();
        self.call_armed(args, nulls)
    }

    /// The frame armed on the site's own context (`call_site`, `call_site_words`).
    #[inline(always)]
    fn arm_site(&mut self) {
        // SAFETY: the site context is owned by this call and outlives the frame.
        unsafe { self.fc.set_result_mcx(self.cx.mcx()) };
    }

    #[inline]
    fn call_armed(&mut self, args: &[Datum], nulls: &[bool]) -> PgResult<NullableDatum> {
        debug_assert!(args.len() == self.nargs && nulls.len() == self.nargs);
        let mut any_null = false;
        for i in 0..self.nargs {
            if nulls[i] {
                any_null = true;
                self.fc.set_arg_null(i);
            } else {
                self.fc.set_arg(i, args[i]);
            }
        }
        self.invoke_loaded(any_null)
    }

    /// `call_site` over a host cell's argument words and null bytes (Gravity's tier-C host, `gravity_expr::emit::host`):
    /// each argument is written once, straight into the frame (no stack image of `Datum`s / `bool`s first).
    /// `strict_checked`: the emitted code calls only when every argument is non-null (its strict short-circuit,
    /// `emit/scalar.rs` `call`, which then never stores the null bytes), so `nulls` is not read and the frame's
    /// strict re-check is skipped: no argument is NULL.
    #[inline]
    pub fn call_site_words(&mut self, args: &[i64], nulls: &[u8], strict_checked: bool) -> PgResult<NullableDatum> {
        debug_assert!(args.len() == self.nargs && nulls.len() == self.nargs);
        self.arm_site();
        let n = self.nargs.min(args.len()).min(nulls.len());
        let mut any_null = false;
        if strict_checked {
            for (i, &a) in args[..n].iter().enumerate() {
                self.fc.set_arg(i, Datum::from_i64(a));
            }
        } else {
            for (i, (&a, &z)) in args[..n].iter().zip(&nulls[..n]).enumerate() {
                if z != 0 {
                    any_null = true;
                    self.fc.set_arg_null(i);
                } else {
                    self.fc.set_arg(i, Datum::from_i64(a));
                }
            }
        }
        self.invoke_loaded(any_null)
    }

    /// The call over the arguments already in the frame (`any_null`: one of them is NULL).
    #[inline]
    fn invoke_loaded(&mut self, any_null: bool) -> PgResult<NullableDatum> {
        if self.scribble {
            return self.invoke_scribbled(any_null);
        }
        self.invoke_plain(any_null)
    }

    #[cold]
    fn invoke_scribbled(&mut self, any_null: bool) -> PgResult<NullableDatum> {
        scribble::before_call(self.flinfo.fn_oid);
        let r = self.invoke_plain(any_null)?;
        if r.isnull || self.life != ResultLife::UntilNextCall || self.ret_typlen == 0 || self.ret_typlen == RET_UNKNOWN {
            return Ok(r);
        }
        // SAFETY: a non-null by-reference result is a live image of its type until the next call.
        let value = unsafe { scribble::quarantine(self.flinfo.fn_oid, self.owner(), r.value, self.ret_typlen, self.cx.mcx()) };
        Ok(NullableDatum { value, isnull: false })
    }

    fn owner(&self) -> usize {
        &*self.cx as *const MemoryContext as usize
    }

    #[inline]
    fn invoke_plain(&mut self, any_null: bool) -> PgResult<NullableDatum> {
        if any_null && self.flinfo.fn_strict {
            return Ok(NullableDatum::null());
        }
        if let (Some(thin), false, false) = (self.thin, any_null, self.fusage) {
            // SAFETY: the thin contract — a live frame of the registered arity, no null argument.
            let value = unsafe { thin(NonNull::from(&mut *self.fc).cast()) }?;
            return Ok(NullableDatum { value, isnull: false });
        }
        self.fc.isnull = false;
        let value = if self.fusage { self.invoke_fusage()? } else { self.flinfo.invoke(&mut *self.fc)? };
        Ok(NullableDatum { value, isnull: self.fc.isnull })
    }

    // ExecEvalFuncExprFusage: an erroring call unwinds past `end_function_usage`, exactly as C's ereport does
    #[cold]
    fn invoke_fusage(&mut self) -> PgResult<Datum> {
        // pgstat_init_function_usage re-reads track_functions per call: an argument's set_config can turn tracking off
        // after the program entry chose the bracketed path (D2819)
        if !tracks(&self.flinfo) {
            return self.flinfo.invoke(&mut *self.fc);
        }
        let fcu =pgstat::function::pgstat_init_function_usage(self.flinfo.fn_oid)?;
        let value = self.flinfo.invoke(&mut *self.fc)?;
        pgstat::function::pgstat_end_function_usage(&fcu, true);
        Ok(value)
    }

    /// `call` with the frame's `context` node set for this call (C `fcinfo->context`, e.g. an ErrorSaveContext).
    #[inline]
    pub fn call_in_context<'r>(&mut self, mcx: Mcx<'r>, args: &[Datum], nulls: &[bool], context: fmgr::FmNodePtr) -> PgResult<NullableDatum> {
        self.fc.context = context;
        let r = self.call(mcx, args, nulls);
        self.fc.context = None;
        r
    }

    /// `call_site` with the frame's `context` node set for this call.
    #[inline]
    pub fn call_site_in_context(&mut self, args: &[Datum], nulls: &[bool], context: fmgr::FmNodePtr) -> PgResult<NullableDatum> {
        self.fc.context = context;
        let r = self.call_site(args, nulls);
        self.fc.context = None;
        r
    }

    /// The compiled-code entry: the result in the site's own context, as `call_site`.
    #[inline]
    pub fn call_c(&mut self, e: *mut ErrSlot, args: &[Datum], nulls: &[bool]) -> Ret {
        match self.call_site(args, nulls) {
            Ok(v) if v.isnull => null_ret(),
            Ok(v) => ok(v.value.as_i64()),
            Err(err) => fail(e, err),
        }
    }

    pub fn reset_window(&mut self) {
        if self.scribble {
            scribble::forget(self.owner());
        }
        self.cx.reset();
        // SAFETY: the reset context is still the call's own and outlives the frame; the frame is re-armed on it.
        unsafe { self.fc.set_result_mcx(self.cx.mcx()) };
    }

    pub fn error_of(err: &PgError) -> (&str, types_error::SqlState) {
        (err.message(), err.sqlstate())
    }
}

/// C `pgstat_track_functions <= flinfo->fn_stats` picks the untracked path; a builtin's TRACK_FUNC_ALL is never tracked.
fn tracks(flinfo: &fmgr::FmgrInfo) -> bool {
    flinfo.fn_stats < fmgr::TRACK_FUNC_ALL && pgstat::function::pgstat_track_functions() > i32::from(flinfo.fn_stats)
}

impl Drop for FmgrCall {
    fn drop(&mut self) {
        if self.scribble {
            scribble::forget(self.owner());
        }
    }
}

#[cfg(test)]
mod tests {
    /// D3620: a call's frame holds its arguments' slots (8 at the least, the widest fixed arity a builtin reads as one
    /// array), not `FUNC_MAX_ARGS`'s: a two-argument site's frame is 8 slots and still calls, and its twin's is too.
    #[test]
    fn a_frame_is_sized_for_its_arguments() {
        let mut call = super::FmgrCall::bind(177, 2, 0).unwrap(); // int4pl
        assert_eq!(call.fc.args.len(), 8, "the smallest frame class");
        let r = call.call_site_words(&[2, 3], &[0, 0], false).unwrap();
        assert_eq!((r.value.as_i64(), r.isnull), (5, false), "int4pl(2, 3) through an 8-slot frame");
        assert_eq!(call.twin().fc.args.len(), 8, "a twin's frame as its call's");
        for (n, slots) in [(0, 8), (7, 8), (8, 8), (9, 16), (16, 16), (17, super::FUNC_MAX_ARGS), (super::FUNC_MAX_ARGS, super::FUNC_MAX_ARGS)] {
            assert_eq!(super::frame(n, 0).args.len(), slots, "{n} arguments");
        }
    }

    /// D412: a call bound under `track_functions = none` and kept (a cached program) is bracketed once the session tracks
    /// its language, and no longer once it stops; a builtin never is.
    #[test]
    fn retrack_follows_the_session_setting() {
        pgstat::function::set_pgstat_track_functions(0);
        let mut call = super::FmgrCall::bind(177, 2, 0).unwrap();
        call.flinfo.fn_stats = fmgr::TRACK_FUNC_PL; // a PL function's, as fmgr_info_cxt reads prolang
        call.retrack();
        assert!(!call.tracked());
        pgstat::function::set_pgstat_track_functions(i32::from(fmgr::TRACK_FUNC_ALL));
        call.retrack();
        assert!(call.tracked(), "track_functions = all covers a PL function");
        let mut builtin = super::FmgrCall::bind(177, 2, 0).unwrap();
        builtin.retrack();
        assert!(!builtin.tracked(), "a builtin is never bracketed");
        pgstat::function::set_pgstat_track_functions(0);
        call.retrack();
        assert!(!call.tracked());
    }

    /// D2835: a comparator call is unbracketed even when the session tracks its language.
    #[test]
    fn untracked_ignores_the_session_setting() {
        pgstat::function::set_pgstat_track_functions(i32::from(fmgr::TRACK_FUNC_ALL));
        let mut call = super::FmgrCall::bind(177, 2, 0).unwrap();
        call.flinfo.fn_stats = fmgr::TRACK_FUNC_PL;
        call.retrack();
        assert!(call.tracked());
        let call = call.untracked();
        assert!(!call.tracked(), "a comparator is never bracketed");
        pgstat::function::set_pgstat_track_functions(0);
    }
}
