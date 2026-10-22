//! `shapes`' unsafe step (rule R6, the module's raw home): the key expressions' boxed extern parameters bound for the
//! Admitted's life, and a scan qual compiled into a Program-lived filter (rule R7: the compiled state outlives its call).
#![allow(unsafe_code)]

use std::cell::RefCell;

use std::rc::Rc;

use super::*;
use crate::effects::contains_volatile;

pub(crate) fn param_bind(exprs: &KeyExprs) -> ParamBind<'static> {
    // SAFETY: `stable` is boxed and lives as long as the Admitted (Program lifetime).
    ParamBind { extern_params: Some(unsafe { &*(&exprs.stable[..] as *const [ParamExternData]) }), exec_vals: None, n_exec: 0, param_callback: None, param_expanded: None, param_rw: None, param_ro_expanded: None, extern_fetch: None }
}

/// D2988: a walk's qual pre-check (ops/scan/sites.rs `prefilter`, `bitmap_prefilter`), evaluated on every row the walk
/// fetches, as `compile_filter_fused` compiles it (`None` for a volatile qual, and for one that is not one fused compare when
/// `fused_only`). Its calls' results live in a root of their own (driver mem `Root::wholesale`, D2944's per-row context),
/// reset after every page-sized run of rows (`IOS_MAX_ROWS`) as ExecScan resets its per-tuple context: a qual that
/// allocates (numeric arithmetic, a text function, a detoasted argument) never grows the statement's context row by row.
/// The qual's result is a bool, so nothing a call made outlives the reset.
pub(crate) fn compile_prefilter(mcx: Mcx<'static>, qual: &NodeList<'static>, bind: ParamBind<'static>, fused_only: bool) -> PgResult<Option<FilterFn>> {
    if qual.is_nil() || qual.iter().any(contains_volatile) {
        return Ok(None);
    }
    let Some(mut state) = execexpr::exec_init_qual(mcx, qual, bind)? else { return Ok(None) };
    if fused_only && !matches!(state.kernel(), execexpr::Kernel::QualScanVarCmpConst { .. }) {
        return Ok(None);
    }
    let root = gravity_driver::mem::root::Root::wholesale("GravityPrefilterRow"); // alloc: Program
    // SAFETY: the root sits in the closure's cell after the state, so it drops after it (tuple order), and the state reads
    // the handle only inside the closure; every reset is followed by a fresh handle (`Root::held`)
    state.arm_result_mcx(unsafe { root.held() });
    let cell = Rc::new(RefCell::new((state, root, 0usize))); // alloc: Program
    let owner = std::thread::current().id();
    let f: FilterFn = Rc::new(move |slot: &mut types_slot::SlotData<'static>| {
        // D1356: thread-confined, as `compile_filter_fused`'s
        debug_assert_eq!(std::thread::current().id(), owner, "gravity: a compiled pre-check evaluated off its creating thread (D1356)");
        let mut g = cell.borrow_mut();
        let (st, root, n) = &mut *g;
        let r = execexpr::exec_qual(Some(&mut **st), &mut execexpr::EvalSlots { scan: Some(slot), inner: None, outer: None });
        *n += 1;
        if *n >= gravity_hosts::ops::scan::index_window::IOS_MAX_ROWS {
            *n = 0;
            root.reset();
            // SAFETY: as above
            st.arm_result_mcx(unsafe { root.held() });
        }
        r
    }); // alloc: Program
    Ok(Some(f))
}

/// `compile_filter_bound`, with whether the qual compiled to one fused compare of a scan column with a constant
/// (execexpr `Kernel::QualScanVarCmpConst`) rather than an interpreted step program, the qual's cost per row.
pub(crate) fn compile_filter_fused(mcx: Mcx<'static>, qual: &NodeList<'static>, bind: ParamBind<'static>) -> PgResult<Option<Option<(FilterFn, bool)>>> {
    if qual.is_nil() {
        return Ok(Some(None));
    }
    if qual.iter().any(contains_volatile) {
        return Ok(None);
    }
    let Some(mut state) = execexpr::exec_init_qual(mcx, qual, bind)? else { return Ok(None) };
    let fused = matches!(state.kernel(), execexpr::Kernel::QualScanVarCmpConst { .. });
    state.arm_result_mcx(mcx);
    let cell = Rc::new(RefCell::new(state)); // alloc: Program
    let owner = std::thread::current().id();
    let f: FilterFn = Rc::new(move |slot: &mut types_slot::SlotData<'static>| {
        // D1356: the compiled qual is thread-confined (its index / bitmap site runs on the leader), and a worker evaluating
        // it would race its steps and RefCell: fail loudly in a debug build if that ever changes
        debug_assert_eq!(std::thread::current().id(), owner, "gravity: a compiled filter evaluated off its creating thread (D1356)");
        let mut st = cell.borrow_mut();
        execexpr::exec_qual(Some(&mut **st), &mut execexpr::EvalSlots { scan: Some(slot), inner: None, outer: None })
    }); // alloc: Program
    Ok(Some(Some((f, fused))))
}
