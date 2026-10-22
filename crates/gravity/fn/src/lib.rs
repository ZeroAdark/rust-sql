//! Gravity's function manager: the tiers, the direct shims and the fmgr call library
//! (docs/gravity/expr.md §2.1).
//!
//! A shim is `extern "C-unwind"`, takes the caller's error slot, a `Region` when its result is
//! by reference, the collation when an argument is collatable, then the SQL arguments in order:
//! by-value as `i64` (the Datum widening below), by-reference as a `View` of the payload. It
//! returns `Ret { val, status }`; the body it calls is the typed Rust routine pgrust's fmgr wrapper
//! calls, so there is one source of truth for the semantics. Strictness is the caller's: no null
//! reaches a shim. Expected failures return `ERROR` with the `PgError` in the slot; nothing panics
//! for an expected failure.
//!
//! Widening of a by-value argument or result into the `i64`: signed integers sign-extend, `bool`
//! is 0/1, `u32` (oid, xid, cid) zero-extends, `u64` is the bits, floats are `to_bits`.
#![allow(clippy::missing_safety_doc, clippy::not_unsafe_ptr_arg_deref)]

use gravity_abi::{word, PtrWord, Word};
use core::cell::Cell;

use adt_datetime::Interval;
use mcx::Mcx;
pub use types_error::PgError;
use types_tuple::NameData;

pub mod agg_final;
/// LEDGER2 step 3: an operator's arena (accounting only, over regions) and its kernel-facing collections.
pub mod arena;
pub mod checked;
/// The effects home's lower layer: what a resolved comparator's call may do (spicmp).
pub mod effects;
/// PostgreSQL's qsort, the exact port (port/qsort.c): kernels orders a run whose comparator may be the user's by it (the
/// kernel boundary admits gravity_fn, not the support crate itself).
pub use pg_qsort;
// C7 runtime half: the kernels declare their leader-class thread-locals through leader_tls! from here (the kernel
// boundary keeps gravity_kernels' own dependencies to gravity_abi, gravity_dict and gravity_fn).
pub use leader_tls;
pub mod fmgr_call;
pub mod inputs;
pub mod pool_catalog;
pub mod resolved;
pub mod result_life;
pub mod scratch;
pub mod session;
pub mod shims;
pub mod srf;
pub mod srf_fmgr;
pub mod tier;

pub use fmgr_call::FmgrCall;
pub use result_life::{fresh_entry, result_life, site_life, CallTier, FreshEntry, ResultLife, FMGR_SITE_ALLOWANCE};
pub use shims::catalog::SHIMS;
pub use tier::{PENDING, ROWS};
/// The session zone's constant UTC offset (kernels host_abi v4 m2.rs `tz_fixed_offset`, M2's expr.fusion).
pub use adt_timestamp::session_fixed_offset;
/// B2-31: the session zone's offset at one instant (kernels `tz_offset_at`, expr.fusion's word claim under a zone with DST).
pub use adt_timestamp::session_offset_at;
/// D2992: PostgreSQL's date vs timestamp(tz) comparators (kernels `zone_cross_bound`, an open-time zone bound's conversion).
pub use adt_date::{date_cmp_timestamp_internal, date_cmp_timestamptz_internal};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    A,
    B,
    C,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    ByVal,
    ByRefVal,
    ByRefRef,
    ViewVal,
    ValView,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Shim,
    ShimCopy,
    Pending,
    Fmgr,
    Srf,
}

pub struct Row {
    pub oid: u32,
    pub name: &'static str,
    pub tier: Tier,
    pub family: Family,
    pub status: Status,
    pub reason: &'static str,
    pub stub: bool,
}

#[derive(Clone, Copy)]
pub enum Arg {
    V(i64),
    R(View),
}

impl Arg {
    #[inline(always)]
    pub fn v(self) -> i64 {
        match self {
            Arg::V(v) => v,
            Arg::R(_) => panic!("gravity_fn: a by-reference argument where a by-value one was declared"),
        }
    }
    #[inline(always)]
    pub fn r(self) -> View {
        match self {
            Arg::R(v) => v,
            Arg::V(_) => panic!("gravity_fn: a by-value argument where a by-reference one was declared"),
        }
    }
}

/// How a shim carries a SQL argument: `V` by value in the `i64`; `P` a payload `View` (text class,
/// numeric, interval); `I` an image `View` (a fixed type's bytes, a varlena's 4-byte-header
/// image); `D` a polymorphic element as the Datum's bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgKind {
    V,
    P,
    I,
    D,
}

/// The dynamic entry: slot, region, collation, the resolved types (one per `Shim.tyargs`), arguments.
pub type DynShim = fn(*mut ErrSlot, *mut Region, u32, &[u32], &[Arg]) -> Ret;
pub type RawFn = unsafe extern "C-unwind" fn();

#[derive(Clone, Copy)]
pub struct Addr(pub RawFn);

pub struct Shim {
    pub oid: u32,
    pub name: &'static str,
    pub family: Family,
    pub nargs: u8,
    pub coll: bool,
    pub region: bool,
    pub args: &'static [ArgKind],
    /// Resolved types taken as leading `u32` arguments: an argument index, or -1 for the result.
    pub tyargs: &'static [i8],
    pub addr: Addr,
    pub call: DynShim,
}

impl Shim {
    pub fn addr(&self) -> usize {
        self.addr.0 as usize
    }
}

pub fn tier_of(oid: u32) -> Tier {
    row_of(oid).map_or(Tier::C, |r| r.tier)
}

pub fn row_of(oid: u32) -> Option<&'static Row> {
    ROWS.binary_search_by_key(&oid, |r| r.oid).ok().map(|i| &ROWS[i])
}

pub fn shim_of(oid: u32) -> Option<&'static Shim> {
    SHIMS.binary_search_by_key(&oid, |s| s.oid).ok().map(|i| &SHIMS[i])
}

/// A by-reference argument: the payload bytes (a varlena's data without its header; a name's
/// bytes without the NUL padding; an interval's 16 bytes; a numeric's payload).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct View {
    pub ptr: *const u8,
    pub len: u64,
}

impl View {
    pub const EMPTY: View = View { ptr: core::ptr::NonNull::<u8>::dangling().as_ptr(), len: 0 };

    #[inline(always)]
    pub fn of(bytes: &[u8]) -> View {
        View { ptr: bytes.as_ptr(), len: bytes.len() as u64 }
    }

    #[inline(always)]
    pub unsafe fn of_image(image: *const u8) -> View {
        let word = image.cast::<u32>().read_unaligned();
        View { ptr: image.add(4), len: ((word >> 2) - 4) as u64 }
    }

    /// The bytes behind the view. The shim boundary's contract: the caller's view is live for
    /// the call, so this is a plain read from a kernel's point of view.
    #[inline(always)]
    pub fn bytes<'a>(self) -> &'a [u8] {
        // SAFETY: the shim boundary contract above; a null pointer only comes with `len == 0`.
        unsafe { core::slice::from_raw_parts(if self.ptr.is_null() { Self::EMPTY.ptr } else { self.ptr }, self.len as usize) }
    }

    /// A fixed-size image (uuid, tid, point, ...) as its byte array.
    #[inline(always)]
    pub fn fixed<const N: usize>(self) -> &'static [u8; N] {
        let b = self.bytes();
        assert!(b.len() >= N, "gravity_fn: a {N}-byte view of {} bytes", b.len());
        // SAFETY: the view is live for the call and holds at least N bytes.
        unsafe { &*b.as_ptr().cast::<[u8; N]>() }
    }

    #[inline(always)]
    pub fn interval_bytes(self) -> [u8; 16] {
        let b = self.bytes();
        assert!(b.len() >= 16, "gravity_fn: an interval view of {} bytes", b.len());
        b[..16].try_into().unwrap()
    }

    #[inline(always)]
    pub fn interval(self) -> Interval {
        let b = self.bytes();
        assert!(b.len() >= 16, "gravity_fn: an interval view of {} bytes", b.len());
        Interval {
            time: i64::from_ne_bytes(b[..8].try_into().unwrap()),
            day: i32::from_ne_bytes(b[8..12].try_into().unwrap()),
            month: i32::from_ne_bytes(b[12..16].try_into().unwrap()),
        }
    }

    #[inline]
    pub fn name64(self) -> [u8; 64] {
        let b = self.bytes();
        let n = b.iter().position(|&c| c == 0).unwrap_or(b.len()).min(63);
        let mut out = [0u8; 64];
        out[..n].copy_from_slice(&b[..n]);
        out
    }

    #[inline]
    pub fn name(self) -> NameData {
        NameData { data: self.name64() }
    }
}

/// A bump region for by-reference results: the ABI's arena window (`Ctx.arena_cur` /
/// `Ctx.arena_end` have this layout), 8-byte aligned allocation, no free.
#[repr(C)]
#[derive(Debug)]
pub struct Region {
    pub cur: Word<*mut u8>,
    pub end: Word<*mut u8>,
}

impl Region {
    pub fn over(bytes: &mut [u8]) -> Region {
        let r = bytes.as_mut_ptr_range();
        Region { cur: word(r.start), end: word(r.end) }
    }

    /// The `n` bytes at the returned pointer are UNINITIALIZED until the caller writes them (D848: a hosted result region
    /// is never zeroed); write all `n` before the pointer is handed out as a datum, and never form a slice over unwritten bytes.
    #[inline(always)]
    pub fn reserve(&mut self, n: usize) -> Option<*mut u8> {
        match self.bump(n) {
            Some(p) => Some(p),
            None => self.spill(n),
        }
    }

    #[inline(always)]
    fn bump(&mut self, n: usize) -> Option<*mut u8> {
        let (cur, end) = (self.cur.ptr(), self.end.ptr());
        let cur = ((cur as usize + 7) & !7) as *mut u8;
        if (end as usize).wrapping_sub(cur as usize) < n || cur as usize > end as usize {
            return None;
        }
        // SAFETY: `cur + n <= end` inside the caller's live region.
        self.cur = word(unsafe { cur.add(n) });
        Some(cur)
    }

    /// D641: a full region the spill hook knows (a hosted expression's result region, lowerv4 hir.rs) moved to a region
    /// of at least `n` more bytes, the bytes already written left where they are; any other region stays full.
    #[cold]
    #[inline(never)]
    fn spill(&mut self, n: usize) -> Option<*mut u8> {
        let h = SPILL_HOOK.load(core::sync::atomic::Ordering::Acquire);
        if h.is_null() {
            return None;
        }
        // SAFETY: only `set_spill_hook` stores the pointer, from a `SpillHook`.
        let hook = unsafe { core::mem::transmute::<*mut (), SpillHook>(h) };
        // SAFETY: `self` is a live region; the hook moves it only when it is one it registered.
        if unsafe { hook(self, n) } {
            self.bump(n)
        } else {
            None
        }
    }

    pub fn used(&self, base: *const u8) -> usize {
        self.cur.ptr() as usize - base as usize
    }
}

/// D641: `Region::reserve`'s grow on a miss: `true` when it moved the region to room for `n` more bytes (a region it
/// registered), `false` to leave it full. # Safety — the region is live.
pub type SpillHook = unsafe fn(&mut Region, usize) -> bool;

static SPILL_HOOK: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

/// D641: the spill hook, set once by the crate that owns spillable regions.
pub fn set_spill_hook(h: SpillHook) {
    SPILL_HOOK.store(h as *mut (), core::sync::atomic::Ordering::Release);
}

/// Where a shim leaves its error: one owned `PgError` pointer, null when none.
#[repr(C)]
#[derive(Debug)]
pub struct ErrSlot {
    pub err: Word<*mut PgError>,
}

impl ErrSlot {
    pub const NONE: ErrSlot = ErrSlot { err: word(core::ptr::null_mut()) };

    pub fn take(&mut self) -> Option<Box<PgError>> {
        let p = core::mem::replace(&mut self.err, word(core::ptr::null_mut())).ptr();
        // SAFETY: a non-null slot holds `Box::into_raw` of the shim's error.
        (!p.is_null()).then(|| unsafe { Box::from_raw(p) })
    }
}

impl Drop for ErrSlot {
    fn drop(&mut self) {
        drop(self.take());
    }
}

/// A shim's return: `status` is `OK` (value in `val`: the widened by-value result, or the
/// address of the image written into the region), `NULL`, `ERROR` (the error is in the slot) or
/// `NO_SPACE` (the region cannot hold the result; the caller grows it and retries).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ret {
    pub val: i64,
    pub status: i64,
}

pub const OK: i64 = gravity_abi::status::OK;
pub const NULL: i64 = 1;
pub const ERROR: i64 = gravity_abi::status::ERROR;
pub const NO_SPACE: i64 = gravity_abi::status::unsupported(gravity_abi::status::UNSUPPORTED_SCRATCH);

#[inline(always)]
pub fn ok(v: i64) -> Ret {
    Ret { val: v, status: OK }
}

#[inline(always)]
pub fn null_ret() -> Ret {
    Ret { val: 0, status: NULL }
}

#[cold]
#[inline(never)]
pub fn fail(e: *mut ErrSlot, err: Box<PgError>) -> Ret {
    // SAFETY: the slot is the caller's, live for the call.
    let slot = unsafe { &mut *e };
    drop(slot.take());
    slot.err = word(Box::into_raw(err));
    Ret { val: 0, status: ERROR }
}

#[cold]
#[inline(never)]
fn no_space() -> Ret {
    Ret { val: 0, status: NO_SPACE }
}

/// A complete image (a varlena with its header, an interval's 16 bytes, a name block) copied
/// into the region; `val` is its address.
#[inline]
pub fn region_copy(r: *mut Region, image: &[u8]) -> Ret {
    // SAFETY: the region is the caller's, live for the call.
    let region = unsafe { &mut *r };
    match region.reserve(image.len()) {
        Some(p) => {
            // SAFETY: `p` has `image.len()` bytes reserved.
            unsafe { core::ptr::copy_nonoverlapping(image.as_ptr(), p, image.len()) };
            ok(p as i64)
        }
        None => no_space(),
    }
}

#[inline]
pub fn region_payload(r: *mut Region, payload: &[u8]) -> Ret {
    // SAFETY: as `region_copy`.
    let region = unsafe { &mut *r };
    let n = payload.len() + 4;
    match region.reserve(n) {
        Some(p) => {
            // SAFETY: `p` has `n` bytes reserved.
            unsafe {
                p.cast::<[u8; 4]>().write_unaligned(datum::set_varsize_4b(n));
                core::ptr::copy_nonoverlapping(payload.as_ptr(), p.add(4), payload.len());
            }
            ok(p as i64)
        }
        None => no_space(),
    }
}

#[inline]
pub fn region_payload2(r: *mut Region, a: &[u8], b: &[u8]) -> Ret {
    // SAFETY: as `region_copy`.
    let region = unsafe { &mut *r };
    let n = a.len() + b.len() + 4;
    match region.reserve(n) {
        Some(p) => {
            // SAFETY: `p` has `n` bytes reserved.
            unsafe {
                p.cast::<[u8; 4]>().write_unaligned(datum::set_varsize_4b(n));
                core::ptr::copy_nonoverlapping(a.as_ptr(), p.add(4), a.len());
                core::ptr::copy_nonoverlapping(b.as_ptr(), p.add(4 + a.len()), b.len());
            }
            ok(p as i64)
        }
        None => no_space(),
    }
}

#[inline]
pub fn region_cstring(r: *mut Region, bytes: &[u8]) -> Ret {
    // SAFETY: as `region_copy`.
    let region = unsafe { &mut *r };
    match region.reserve(bytes.len() + 1) {
        Some(p) => {
            // SAFETY: `p` has `len + 1` bytes reserved.
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
                p.add(bytes.len()).write(0);
            }
            ok(p as i64)
        }
        None => no_space(),
    }
}

/// A result image found through its Datum pointer: `typlen` -1 a varlena (its `VARSIZE_ANY`,
/// any header form), -2 a NUL-terminated cstring, n a fixed-size type; copied into the region.
#[inline]
pub fn region_datum(r: *mut Region, p: *const u8, typlen: i32) -> Ret {
    // SAFETY: the pointer is the typed body's result, live for the call.
    let n = unsafe {
        match typlen {
            -1 => types_tuple::varatt::varsize_any(p),
            -2 => core::ffi::CStr::from_ptr(p.cast()).to_bytes().len() + 1,
            n => n as usize,
        }
    };
    // SAFETY: `n` bytes are readable at `p` per the typlen contract.
    region_copy(r, unsafe { core::slice::from_raw_parts(p, n) })
}

/// A payload view rebuilt as a 4-byte-header image in the scratch context — for the few bodies
/// that read an image (slice fetchers, raw-image helpers); one copy per call.
#[inline]
pub fn scratch_image<'m>(mcx: Mcx<'m>, payload: &[u8]) -> &'m [u8] {
    let mut img = mcx::PgVec::<u8>::with_capacity_in(payload.len() + 4, mcx);
    img.extend_from_slice(&datum::set_varsize_4b(payload.len() + 4));
    img.extend_from_slice(payload);
    img.leak()
}

/// A cstring payload view with its NUL, in the scratch context.
#[inline]
pub fn scratch_cstr<'m>(mcx: Mcx<'m>, bytes: &[u8]) -> &'m [u8] {
    let mut v = mcx::PgVec::<u8>::with_capacity_in(bytes.len() + 1, mcx);
    v.extend_from_slice(bytes);
    v.push(0);
    v.leak()
}

/// A `Varlena` result (what `varlena_result` takes): its image copied into the region.
#[inline]
pub fn region_varlena(r: *mut Region, v: datum::Varlena<'_>) -> Ret {
    region_copy(r, v.as_bytes())
}

#[inline]
pub fn region_interval(r: *mut Region, iv: &Interval) -> Ret {
    let mut img = [0u8; 16];
    img[..8].copy_from_slice(&iv.time.to_ne_bytes());
    img[8..12].copy_from_slice(&iv.day.to_ne_bytes());
    img[12..].copy_from_slice(&iv.month.to_ne_bytes());
    region_copy(r, &img)
}

thread_local! {
    static PENDING_ERROR: Cell<Option<Box<PgError>>> = const { Cell::new(None) };
}

/// A host wrapping a library call parks its error here, returns `ERROR`; the entry's mapping takes it.
pub fn stash_error(e: Box<PgError>) {
    PENDING_ERROR.with(|p| p.set(Some(e)));
}

pub fn take_error() -> Option<Box<PgError>> {
    PENDING_ERROR.with(|p| p.take())
}

/// D2372: `f` runs at this thread's session teardown, in its State phase (mcx `SessionCleanupPhase`: every memory context
/// still alive), for a host's per-thread state that holds session memory (kernels' bound comparators, whose `fn_extra`
/// lives in the session's contexts). A thread with no session teardown (a unit test) never runs it.
pub fn at_session_end(f: fn()) {
    mcx::register_session_cleanup_phase(mcx::SessionCleanupPhase::State, Box::new(f)); // alloc: Cold
}

thread_local! {
    // alloc: Node
    static SCRATCH: scratch::ScratchSlot = scratch::ScratchSlot::new("gravity_fn scratch", "gravity_fn scratch (nested)");
}

/// Run a body that allocates its result in an `Mcx` against the thread's retained scratch
/// context; the caller copies the result into its region inside `f`, the context is reset after.
/// A re-entered call (a body that reaches a shim again) gets a context of its own.
#[inline]
pub fn with_scratch(f: impl FnOnce(Mcx<'_>) -> Ret) -> Ret {
    SCRATCH.with(|s| s.with(f))
}

/// D314: `f` under a landing pad, for a host a kernel's `extern "C"` entry calls (`with_payload`, `collation::sort_key` /
/// `compare`). A PostgreSQL ERROR raised by unwinding comes back as the call's error (the entry stashes it and returns
/// its status). Any other unwind (a thread exit, a PgError above ERROR, a bug's panic) must not cross the entry's frame
/// (Rust aborts there, the whole server): it is parked (`gravity_abi::status::unwind`) and resumes at
/// `gravity_program::Code::call` once the program returns; the call fails meanwhile. The non-unwinding path is `f`.
#[inline(always)]
pub(crate) fn guarded<R>(f: impl FnOnce() -> Result<R, Box<PgError>>) -> Result<R, Box<PgError>> {
    match std::panic::catch_unwind(core::panic::AssertUnwindSafe(f)) { // unwind-ok: c-callback
        Ok(r) => r,
        Err(payload) => Err(caught(payload)),
    }
}

#[cold]
#[inline(never)]
fn caught(payload: Box<dyn core::any::Any + Send>) -> Box<PgError> {
    let payload: Box<dyn core::any::Any + Send> = match payload.downcast::<Box<PgError>>() {
        Ok(e) if e.level() == types_error::ERROR => return *e,
        Ok(e) => e,
        Err(payload) => payload,
    };
    let payload: Box<dyn core::any::Any + Send> = match payload.downcast::<PgError>() {
        Ok(e) if e.level() == types_error::ERROR => return e,
        Ok(e) => e,
        Err(payload) => payload,
    };
    gravity_abi::status::unwind::park(payload);
    Box::new(PgError::error("gravity: a host call unwound (resumed past the program)")) // alloc: Cold
}

/// The payload of the varlena at `datum` in any header form handed to `f`: a short or plain image in place, a compressed inline or external one detoasted into the scratch context for the call. # Safety — `datum` is a live varlena.
pub unsafe fn with_payload<R>(datum: *const u8, f: impl FnOnce(&[u8]) -> R) -> Result<R, Box<PgError>> {
    guarded(|| payload_of(datum, f))
}

/// # Safety — As `with_payload`.
#[inline(always)]
unsafe fn payload_of<R>(datum: *const u8, f: impl FnOnce(&[u8]) -> R) -> Result<R, Box<PgError>> {
    let b0 = *datum;
    if b0 & 1 != 0 && b0 != 1 {
        return Ok(f(core::slice::from_raw_parts(datum.add(1), (b0 >> 1) as usize - 1)));
    }
    if b0 & 3 == 0 {
        let w = datum.cast::<u32>().read_unaligned();
        return Ok(f(core::slice::from_raw_parts(datum.add(4), (w >> 2) as usize - 4)));
    }
    let raw = core::slice::from_raw_parts(datum, types_tuple::varatt::varsize_any(datum));
    let mut out = Err(None);
    with_scratch(|mcx| {
        out = match detoast::detoast_attr(mcx, raw) {
            Ok(flat) => Ok(f(&flat[4..])),
            Err(e) => Err(Some(e)),
        };
        ok(0)
    });
    out.map_err(|e| e.expect("the detoast's error"))
}

/// The datum types the shims and `FmgrCall` speak (a kernel names them through this crate, never `datum` itself).
pub use datum::{Datum, NullableDatum};
pub mod typesem;
