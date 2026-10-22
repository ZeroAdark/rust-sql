// params.c ParamListInfo as a registry (stmt_list precedent); stale handle = loud panic.
use core::cell::RefCell;

use ::datum::{Datum, NullableDatum};
use ::types_core::Oid;
use ::types_error::PgResult;

use crate::ParamListHandle;

pub const PARAM_FLAG_CONST: u16 = 0x0001;

#[derive(Clone, Copy, Debug)]
pub struct ParamExternData {
    pub value: Datum,
    pub isnull: bool,
    pub pflags: u16,
    pub ptype: Oid,
}

// C params.h ParamExecData; exec_plan = `void *execPlan` presence bit (write side: nodeSubplan.c lane).
#[derive(Clone, Copy, Debug)]
pub struct ParamExecData {
    pub value: Datum,
    pub isnull: bool,
    pub exec_plan: bool,
}

impl ParamExecData {
    pub const EMPTY: ParamExecData =
        ParamExecData { value: Datum::null(), isnull: true, exec_plan: false };
}

// C ParamListInfo.paramCompile's runtime half: every PARAM_EXTERN becomes an
// EEOP_PARAM_CALLBACK step (execExpr.c:1044-1064) run only when reached; `arg`
// is the owner's per-evaluation data (C paramFetchArg, arm_param_callback_arg).
//
// # Safety
// `arg` is the pointer the list owner armed for the running evaluation.
pub type ParamCallbackFn =
    unsafe fn(arg: *mut (), paramid: i32, paramtype: Oid) -> PgResult<NullableDatum>;

// C VARATT_IS_EXTERNAL_EXPANDED on a fetched container datum: pgrust keeps
// flat images, so the list owner reports which Params hold an expanded
// container (plpgsql array Vars, pl_exec.c exec_assign_value).
pub type ParamExpandedFn = unsafe fn(arg: *mut (), paramid: i32) -> bool;

// C ParamListInfo.paramFetch (speculative = false) as ExecEvalParamExtern
// calls it, for a slot the list was built without.
//
// # Safety
// `arg` is the pointer the list owner registered, live while the list is.
pub type ParamFetchFn = unsafe fn(arg: *mut (), paramid: i32) -> PgResult<ParamExternData>;

#[derive(Clone, Copy, Debug)]
pub struct ParamFetchHook {
    pub func: ParamFetchFn,
    pub arg: *mut (),
}
mcx::forget_safe_nodrop!(ParamFetchHook);

// Resolve-once compile binding (execexpr's AggBind precedent); both arrays are address-stable.
#[derive(Clone, Copy, Debug, Default)]
pub struct ParamBind<'a> {
    pub extern_params: Option<&'a [ParamExternData]>,
    pub exec_vals: Option<core::ptr::NonNull<ParamExecData>>,
    pub n_exec: u32,
    // C ParamListInfo.paramCompile: when set, every PARAM_EXTERN compiles to
    // a callback step instead of a resolved load (execExpr.c:1060-1064) and
    // `extern_params` is not consulted for it.
    pub param_callback: Option<ParamCallbackFn>,
    pub param_expanded: Option<ParamExpandedFn>,
    pub param_rw: Option<(*const (), ParamCallbackFn)>,
    pub param_ro_expanded: Option<(&'a [*const ()], ParamCallbackFn)>,
    // Fetch for an `extern_params` slot with no value (ptype InvalidOid).
    pub extern_fetch: Option<ParamFetchHook>,
}

impl ParamBind<'_> {
    pub const NONE: ParamBind<'static> = ParamBind {
        extern_params: None,
        exec_vals: None,
        n_exec: 0,
        param_callback: None,
        param_expanded: None,
        param_rw: None,
        param_ro_expanded: None,
        extern_fetch: None,
    };
}

#[derive(Clone)]
struct Entry {
    ptr: *const ParamExternData,
    len: usize,
    generation: u32,
    // C paramFetch != NULL (plpgsql setup_param_list): BuildParamLogString
    // consumers (auto_explain's Query Parameters line) read this bit.
    hooked: bool,
    // A PL's variable environment (plpgsql datums, SQL-function arguments),
    // not protocol bind values; the sqe bind-params gate reads this.
    pl_owned: bool,
    // C ParamListInfo.paramValuesStr: textual values for error reporting
    // (exec_bind_message stores BuildParamLogString's result; params.c
    // ParamsErrorCallback reads it). NULL in C = None.
    param_values_str: Option<std::rc::Rc<str>>,
    fetch: Option<ParamFetchHook>,
}

thread_local! {
    static ENTRIES: RefCell<Vec<Option<Entry>>> = const { RefCell::new(Vec::new()) };
    static FREE: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
    static GENERATION: core::cell::Cell<u32> = const { core::cell::Cell::new(0) };
}

fn encode(idx: u32, generation: u32) -> ParamListHandle {
    ParamListHandle((u64::from(generation) << 32) | u64::from(idx + 1))
}

fn decode(h: ParamListHandle) -> (u32, u32) {
    ((h.0 as u32) - 1, (h.0 >> 32) as u32)
}

/// # Safety
/// `params` (with its by-ref datums) must outlive [`free`] (PortalDrop's job).
pub unsafe fn register(params: &[ParamExternData]) -> ParamListHandle {
    // SAFETY: forwarded caller contract.
    unsafe { register_with_hooked(params, false, false) }
}

/// [`register`] for a PL-owned list without a paramFetch hook (functions.c
/// postquel_sub_params: makeParamList). See `Entry::pl_owned`.
///
/// # Safety
/// Same liveness contract as [`register`].
pub unsafe fn register_pl_owned(params: &[ParamExternData]) -> ParamListHandle {
    // SAFETY: forwarded caller contract.
    unsafe { register_with_hooked(params, false, true) }
}

/// [`register`] for a list that C would back with a paramFetch hook (PL-owned
/// variables). See `Entry::hooked`.
///
/// # Safety
/// Same liveness contract as [`register`].
pub unsafe fn register_hooked(params: &[ParamExternData]) -> ParamListHandle {
    // SAFETY: forwarded caller contract.
    unsafe { register_with_hooked(params, true, true) }
}

/// [`register_hooked`] with the owner's fetch for slots left without a value.
///
/// # Safety
/// As [`register`]; `fetch.arg` must stay valid while the list is registered.
pub unsafe fn register_hooked_fetch(
    params: &[ParamExternData],
    fetch: Option<ParamFetchHook>,
) -> ParamListHandle {
    // SAFETY: forwarded caller contract.
    let h = unsafe { register_with_hooked(params, true, true) };
    if fetch.is_some() {
        let (idx, _) = decode(h);
        ENTRIES.with(|e| {
            if let Some(Some(en)) = e.borrow_mut().get_mut(idx as usize) {
                en.fetch = fetch;
            }
        });
    }
    h
}

/// Slot `id` (1-based, as `Param` reads it) of list `h` as it is now; `None` for a NULL handle, one no longer registered
/// (never the stale handle's loud panic: a compiled program's host reads `None` as its internal error) or an id past the
/// list.
pub fn slot(h: ParamListHandle, id: i32) -> Option<ParamExternData> {
    if h.is_null() || id < 1 {
        return None;
    }
    let (idx, generation) = decode(h);
    let e = ENTRIES.with(|e| e.borrow().get(idx as usize).cloned().flatten())?;
    if e.generation != generation {
        return None;
    }
    // SAFETY: register()'s liveness contract (the entry is registered, so its array is live).
    let params = unsafe { core::slice::from_raw_parts(e.ptr, e.len) };
    params.get(id as usize - 1).copied()
}

pub fn fetch_hook(h: ParamListHandle) -> Option<ParamFetchHook> {
    if h.is_null() {
        return None;
    }
    lookup(h).fetch
}

pub fn defers_to_evaluation(h: ParamListHandle) -> bool {
    if h.is_null() || !is_fetch_hooked(h) {
        return false;
    }
    lookup(h).fetch.is_some() || with(h, |ps| ps.iter().any(|p| p.pflags & PARAM_FLAG_CONST == 0))
}

unsafe fn register_with_hooked(
    params: &[ParamExternData],
    hooked: bool,
    pl_owned: bool,
) -> ParamListHandle {
    let generation = GENERATION.with(|g| {
        let v = g.get().wrapping_add(1);
        g.set(v);
        v
    });
    let entry = Entry {
        ptr: params.as_ptr(),
        len: params.len(),
        generation,
        hooked,
        pl_owned,
        param_values_str: None,
        fetch: None,
    };
    let idx = match FREE.with(|f| f.borrow_mut().pop()) {
        Some(i) => {
            ENTRIES.with(|e| e.borrow_mut()[i as usize] = Some(entry));
            i
        }
        None => ENTRIES.with(|e| {
            let mut e = e.borrow_mut();
            e.push(Some(entry));
            (e.len() - 1) as u32
        }),
    };
    encode(idx, generation)
}

fn lookup(h: ParamListHandle) -> Entry {
    assert!(!h.is_null(), "params: NULL handle dereferenced");
    let (idx, generation) = decode(h);
    let entry = ENTRIES.with(|e| e.borrow().get(idx as usize).cloned().flatten());
    match entry {
        Some(e) if e.generation == generation => e,
        _ => panic!("params: stale ParamListHandle {h:?} (freed)"),
    }
}

/// C `params->paramValuesStr = ...` (exec_bind_message).
pub fn set_param_values_str(h: ParamListHandle, s: std::rc::Rc<str>) {
    let (idx, generation) = decode(h);
    ENTRIES.with(|e| {
        let mut e = e.borrow_mut();
        match e.get_mut(idx as usize).and_then(|s| s.as_mut()) {
            Some(en) if en.generation == generation => en.param_values_str = Some(s),
            _ => panic!("params: stale ParamListHandle {h:?} (freed)"),
        }
    });
}

/// C `params->paramValuesStr` read (params.c ParamsErrorCallback); a NULL
/// handle reads as C's params == NULL.
pub fn param_values_str(h: ParamListHandle) -> Option<std::rc::Rc<str>> {
    if h.is_null() {
        return None;
    }
    lookup(h).param_values_str
}

/// # Safety
/// The borrow must not outlive the register/[`free`] window (the portal outlives one execution).
pub unsafe fn resolve<'a>(h: ParamListHandle) -> &'a [ParamExternData] {
    let e = lookup(h);
    // SAFETY: register()'s liveness contract, narrowed by the caller's bound.
    unsafe { core::slice::from_raw_parts(e.ptr, e.len) }
}

pub fn with<R>(h: ParamListHandle, f: impl FnOnce(&[ParamExternData]) -> R) -> R {
    let e = lookup(h);
    // SAFETY: register()'s liveness contract; no RefCell borrow held here.
    let params = unsafe { core::slice::from_raw_parts(e.ptr, e.len) };
    f(params)
}

pub fn num_params(h: ParamListHandle) -> usize {
    if h.is_null() {
        return 0;
    }
    lookup(h).len
}

/// C's `params->paramFetch != NULL` test (see `Entry::hooked`).
pub fn is_fetch_hooked(h: ParamListHandle) -> bool {
    if h.is_null() {
        return false;
    }
    lookup(h).hooked
}

/// The list is a PL's variable environment (see `Entry::pl_owned`).
pub fn is_pl_owned(h: ParamListHandle) -> bool {
    if h.is_null() {
        return false;
    }
    lookup(h).pl_owned
}

pub fn free(h: ParamListHandle) {
    if h.is_null() {
        return;
    }
    let (idx, generation) = decode(h);
    ENTRIES.with(|e| {
        let mut e = e.borrow_mut();
        if let Some(slot) = e.get_mut(idx as usize) {
            if slot.as_ref().map(|en| en.generation) == Some(generation) {
                *slot = None;
                FREE.with(|f| f.borrow_mut().push(idx));
            }
        }
    });
}

pub fn live_count() -> usize {
    ENTRIES.with(|e| e.borrow().iter().filter(|s| s.is_some()).count())
}

pub fn is_live(h: ParamListHandle) -> bool {
    if h.is_null() {
        return false;
    }
    let (idx, generation) = decode(h);
    ENTRIES.with(|e| {
        e.borrow()
            .get(idx as usize)
            .and_then(|s| s.as_ref().map(|en| en.generation))
            == Some(generation)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_with_free_roundtrip() {
        let params = vec![ParamExternData {
            value: Datum::from_i32(42),
            isnull: false,
            pflags: PARAM_FLAG_CONST,
            ptype: 23,
        }];
        let h = unsafe { register(&params) };
        assert!(is_live(h));
        assert_eq!(num_params(h), 1);
        with(h, |p| {
            assert_eq!(p[0].value.as_i32(), 42);
            assert_eq!(p[0].ptype, 23);
        });
        free(h);
        assert!(!is_live(h));
        free(h); /* idempotent */
    }

    /// `slot` reads a registered list's slot by its 1-based id and answers `None`, never a panic, past the list, for
    /// a NULL handle and for a freed one.
    #[test]
    fn slot_reads_a_live_list_only() {
        let params = [ParamExternData { value: Datum::from_i32(7), isnull: false, pflags: 0, ptype: 25 }];
        let h = unsafe { register_hooked(&params) };
        assert_eq!(slot(h, 1).map(|p| (p.value.as_i32(), p.ptype, p.pflags)), Some((7, 25, 0)));
        assert!(slot(h, 0).is_none() && slot(h, 2).is_none() && slot(ParamListHandle::NULL, 1).is_none());
        free(h);
        assert!(slot(h, 1).is_none());
    }

    // functions.c:1490: a SQL function's list is PL-owned but not hooked.
    #[test]
    fn pl_owned_lists_split_the_fetch_hook_bit() {
        let params = [ParamExternData {
            value: Datum::from_i32(41),
            isnull: false,
            pflags: PARAM_FLAG_CONST,
            ptype: 23,
        }];
        let plain = unsafe { register(&params) };
        let sql_fn = unsafe { register_pl_owned(&params) };
        let plpgsql = unsafe { register_hooked(&params) };
        assert!(!is_fetch_hooked(plain) && !is_pl_owned(plain));
        assert!(!is_fetch_hooked(sql_fn) && is_pl_owned(sql_fn));
        assert!(is_fetch_hooked(plpgsql) && is_pl_owned(plpgsql));
        assert!(!is_fetch_hooked(ParamListHandle::NULL) && !is_pl_owned(ParamListHandle::NULL));
        free(plain);
        free(sql_fn);
        free(plpgsql);
    }

    #[test]
    #[should_panic(expected = "stale ParamListHandle")]
    fn stale_handle_is_loud() {
        let params = [ParamExternData {
            value: Datum::null(),
            isnull: true,
            pflags: 0,
            ptype: 23,
        }];
        let h = unsafe { register(&params) };
        free(h);
        with(h, |_| ());
    }
}
