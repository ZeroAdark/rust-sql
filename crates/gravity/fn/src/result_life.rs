//! The result-lifetime contract (docs/gravity/lowerv3/design.md §3): each call site's `ResultLife`, the FRESH registry, the SCRIBBLE rig.

use core::sync::atomic::{AtomicU8, Ordering};
use std::cell::RefCell;

use datum::Datum;

use crate::{shim_of, Region, Shim};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResultLife {
    ByValue,
    /// Reused scratch (`OutBuf`, a thread-local like `int4out`'s): the next call of the same builtin on the thread, any site, overwrites it.
    UntilNextCall,
    /// Palloc'd per call into the site's result context (a child of G), which v3's `row()` resets at every row start: `Until::Row`.
    RowContext,
    CallerRegion,
    IntoArgument(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallTier {
    Shim,
    Fmgr,
}

#[derive(Debug)]
pub struct FreshEntry {
    pub oid: u32,
    pub name: &'static str,
    pub evidence: &'static str,
}

/// DENY BY DEFAULT (ruling of 09-23): an unlisted fmgr-tier builtin is `UntilNextCall`; list one only after reading its body (tests/result_life.rs).
pub static FRESH: [FreshEntry; 0] = [];

/// Steady-state bytes of one fmgr site's result context under a per-row reset (the bump keeper block): MEMORY's footprint term per site.
pub const FMGR_SITE_ALLOWANCE: usize = 8192;

pub fn fresh_entry(proc_oid: u32) -> Option<&'static FreshEntry> {
    FRESH.binary_search_by_key(&proc_oid, |e| e.oid).ok().map(|i| &FRESH[i])
}

pub fn shim_site(proc_oid: u32, nargs: usize, strict: bool) -> Option<&'static Shim> {
    shim_of(proc_oid).filter(|s| s.nargs as usize == nargs && s.tyargs.is_empty() && strict)
}

pub fn shim_life(s: &Shim) -> ResultLife {
    if s.region {
        ResultLife::CallerRegion
    } else {
        ResultLife::ByValue
    }
}

pub fn fmgr_life(proc_oid: u32, ret_byval: bool) -> ResultLife {
    match (ret_byval, fresh_entry(proc_oid)) {
        (true, _) => ResultLife::ByValue,
        (false, Some(_)) => ResultLife::RowContext,
        (false, None) => ResultLife::UntilNextCall,
    }
}

pub fn site_life(proc_oid: u32, nargs: usize, ret_byval: bool) -> (CallTier, ResultLife) {
    let strict = fmgr_core::fmgr_isbuiltin(proc_oid).is_some_and(|b| b.strict);
    match shim_site(proc_oid, nargs, strict) {
        Some(s) => (CallTier::Shim, shim_life(s)),
        None => (CallTier::Fmgr, fmgr_life(proc_oid, ret_byval)),
    }
}

/// FACTS' one call per FuncExpr / OpExpr (an OpExpr resolved to its `oprcode` first): a site called with its catalog arguments.
pub fn result_life(proc_oid: u32, ret_byval: bool) -> ResultLife {
    let nargs = fmgr_core::fmgr_isbuiltin(proc_oid).map_or(usize::MAX, |b| b.nargs.max(0) as usize);
    site_life(proc_oid, nargs, ret_byval).1
}

pub unsafe fn result_bytes<'a>(v: Datum, typlen: i16) -> &'a [u8] {
    let p = v.as_usize() as *const u8;
    let n = match typlen {
        -1 => types_tuple::varatt::varsize_any(p),
        -2 => core::ffi::CStr::from_ptr(p.cast()).to_bytes().len() + 1,
        n => n as usize,
    };
    core::slice::from_raw_parts(p, n)
}

/// `retain`'s copy when a result's life is shorter than its consumer needs; `None` when the region is full. # Safety — as `result_bytes`.
pub unsafe fn copy_result(v: Datum, typlen: i16, dst: &mut Region) -> Option<Datum> {
    let bytes = result_bytes(v, typlen);
    let p = dst.reserve(bytes.len())?;
    core::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
    Some(Datum::from_usize(p as usize))
}

/// SCRIBBLE (`PGRUST_GRAVITY_SCRIBBLE=1`, test only): an `UntilNextCall` result is handed out as a copy in its site's context, overwritten
/// with `0x7E` before the next call of the same builtin on the thread, so a consumer that kept it without `retain` reads tildes (D202).
pub mod scribble {
    use super::*;

    pub const BYTE: u8 = 0x7E;
    static STATE: AtomicU8 = AtomicU8::new(0);

    struct Live {
        oid: u32,
        owner: usize,
        ptr: *mut u8,
        len: usize,
    }

    thread_local! {
        static LIVE: RefCell<Vec<Live>> = const { RefCell::new(Vec::new()) }; // alloc: Cold
    }

    pub fn on() -> bool {
        match STATE.load(Ordering::Relaxed) {
            1 => false,
            2 => true,
            _ => {
                let on = crate::inputs::scribble();
                let _ = STATE.compare_exchange(0, if on { 2 } else { 1 }, Ordering::Relaxed, Ordering::Relaxed);
                STATE.load(Ordering::Relaxed) == 2
            }
        }
    }

    /// A test's override: sites bound afterwards see it.
    pub fn force(on: bool) {
        STATE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
    }

    /// Every touch of `LIVE` is `try_with` (D324): a site dropped by another thread-local's destructor at thread exit
    /// (a cached program's `FmgrCall` when a backend ends) can run after `LIVE`'s own destructor, and `with` would panic
    /// inside a destructor, which aborts the process. With `LIVE` gone, there is nothing left to scribble or forget.
    #[cold]
    pub fn before_call(oid: u32) {
        let _ = LIVE.try_with(|l| {
            l.borrow_mut().retain(|x| {
                if x.oid != oid {
                    return true;
                }
                // SAFETY: the copy lives in its owner's context, not reset yet (`forget` runs before a reset or drop).
                unsafe { core::ptr::write_bytes(x.ptr, BYTE, x.len) };
                false
            })
        });
    }

    /// # Safety — as `result_bytes`.
    #[cold]
    pub unsafe fn quarantine(oid: u32, owner: usize, v: Datum, typlen: i16, cx: mcx::Mcx<'_>) -> Datum {
        let bytes = result_bytes(v, typlen);
        let mut img = mcx::PgVec::<u8>::with_capacity_in(bytes.len(), cx);
        img.extend_from_slice(bytes);
        let ptr = img.leak().as_mut_ptr();
        let _ = LIVE.try_with(|l| l.borrow_mut().push(Live { oid, owner, ptr, len: bytes.len() })); // alloc: Cold
        Datum::from_usize(ptr as usize)
    }

    #[cold]
    pub fn forget(owner: usize) {
        let _ = LIVE.try_with(|l| l.borrow_mut().retain(|x| x.owner != owner));
    }

    /// v3's hir `row()` rewind hook: the released bytes become tildes. # Safety — `[p, p + len)` is writable.
    pub unsafe fn released(p: *mut u8, len: usize) {
        if on() {
            core::ptr::write_bytes(p, BYTE, len);
        }
    }
}
