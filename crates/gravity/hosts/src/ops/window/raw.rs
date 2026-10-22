//! The window family's raw half (rule R6): every `unsafe` of the window object, each a named function with its SAFETY
//! text, bodies as they were in the family's files: the object's runtime behind `Head::op` (`op`), the stored row's put
//! and cell reads (`Store::put`, `cell_at`), the result cells' validity bytes (`set_valid`), the frame offsets' read
//! (`Frame::set_offsets`), the aggregates' function binding, datum copies and result contexts (`bind`, `datum_copy`,
//! `read_only`, `result_into`), the lazy arguments' context, parameter list and row result context (`lazy_mcx`,
//! `spec_params`, `arm_row_result`), the executor-parameter path (`m10_win_params`, W3), a rerun set's re-arm of the object
//! by its handle word (`rearm_at`, D4237), and the row store's crate test (`store_tests`, D3345).
#![allow(unsafe_code)]

use datum::{Datum, NullableDatum};
use execexpr::ExprState;
use gravity_abi::status::ERROR;
use gravity_abi::Ctx;
use gravity_fn::arena::OpArena;
use mcx::{Mcx, MemoryContext, PinnedContext};
use types_core::fmgr::{AggFnArgTypes, FnExprErased};
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_fmgr::FmgrInfo;
use types_nodes::rawnodes::*;
use types_portal::params::ParamExternData;

use crate::ops::window::aggs::Frame as Fcinfo;
use crate::ops::window::frame::Frame;
use crate::ops::window::object::{abi, bad, WinObj, WinSpec, CHUNK_ROWS};
use crate::ops::window::store::Store;

/// D4237: the window object whose handle word is `h` re-armed (`WinObj::rearm`) by the run of the rerun set holding its
/// pulled fill (exec rerun/mod.rs `run`, through exec ops/window/raw.rs `rearm_windows`). A null handle or one whose cell
/// is not a live window object's is a Bug by name.
///
/// # Safety
/// `h` is zero or the handle word exec's prepare laid for this execution (`WinObj::handle`), the object live until the
/// execution's end; no call of the object is running on this thread except through `WinObj::rearm`'s own borrow check.
pub unsafe fn rearm_at(h: u64) -> PgResult<()> {
    let cell = h as *const abi::WinCell;
    // SAFETY: the caller's contract: a nonzero `h` is a laid cell's address, its magic read before any other field
    if cell.is_null() || unsafe { (*cell).head.magic } != abi::WIN_MAGIC {
        return Err(bad("window.agg: a re-armed window's handle word is not its object (D4237)"));
    }
    // SAFETY: the cell is the first field of the `#[repr(C)]` `WinObj` that laid it, live for the execution
    unsafe { &*cell.cast::<WinObj>() }.rearm()
}

/// The words of `m10_win_params`' `n` records at `vals` (`PARAM_WORDS` each); `None` for a null `vals` with `n` > 0.
pub(crate) fn param_records<'a>(vals: *const u64, n: u64) -> Option<&'a [u64]> {
    if n == 0 {
        return Some(&[]);
    }
    if vals.is_null() {
        return None;
    }
    // SAFETY: the reader's open (lowerv4 window/rows.rs `open`) writes `n` records of `PARAM_WORDS` words into a stack
    // slot it passes as `vals`; the slot lives for the call, and the slice does not outlive it (`set_params` reads it).
    Some(unsafe { core::slice::from_raw_parts(vals, PARAM_WORDS * n as usize) })
}

/// One `m10_win_params` record's words: paramid, datum, NULL (0 / 1), the type's `typlen | byval << 16`.
pub(crate) const PARAM_WORDS: usize = 4;

/// By-reference datum `v` of a type of length `typlen` copied into `keep` (PG `datumCopy`).
pub(crate) fn copy_param(keep: &MemoryContext, v: Datum, typlen: i16) -> PgResult<Datum> {
    // SAFETY: `v` is the parameter's by-reference value, live while the reader's open runs (its owner's row or slot), and
    // `typlen` its type's, as the lowerer's type facts give it.
    unsafe { execexpr::agg_datum_copy(keep.mcx(), v, typlen) }
}

/// A heap segment's zeroed words: the global allocator's block (the zeroed vector's calloc road: fresh pages stay untouched
/// until carved), or pagepool P3's (`PGRUST_POOL_GRAVITY=1`, an owned store's, 1 KiB and up): the page pool's block, all
/// of what it handed out. Freed to whichever served it.
pub(super) struct Words {
    ptr: core::ptr::NonNull<u64>,
    len: usize,
    pooled: bool,
}

impl Words {
    pub(super) fn len(&self) -> usize {
        self.len
    }

    pub(super) fn as_ptr(&self) -> *const u64 {
        self.ptr.as_ptr()
    }

    /// The page pool's block (its bytes on the store's pool charge).
    pub(super) fn pooled(&self) -> bool {
        self.pooled
    }
}

impl Drop for Words {
    fn drop(&mut self) {
        if self.pooled {
            // SAFETY: the pool block `zeroed_words` took, given back once.
            unsafe { pagepool::give(self.ptr.cast(), self.len * 8) };
        } else if self.len > 0 {
            // SAFETY: the global allocator's block of `len` u64s `zeroed_words` allocated with this layout, freed once.
            unsafe { std::alloc::dealloc(self.ptr.as_ptr().cast(), std::alloc::Layout::array::<u64>(self.len).expect("layout")) };
        }
    }
}

/// pagepool P3: a window store's segment from the page pool, statement-lived and zeroed.
const POOL_HINT: pagepool::Hint = pagepool::Hint::new(pagepool::Class::Task, pagepool::Tag::OP).zero(pagepool::Zero::Dense);

/// pagepool P3: an owned store's segments of 1 KiB and up come from the page pool (the server's switch, read by the pool,
/// with the pool reserved).
pub(super) fn pooled() -> bool {
    pagepool::on() && pagepool::switches().gravity
}

/// A heap segment's `n` zeroed words (`pool`: the page pool's when it serves them, `pooled`), the allocator's refusal the
/// statement's 53200 (PG's palloc failure in the tuplestore's context).
pub(super) fn zeroed_words(n: usize, pool: bool) -> PgResult<Words> { // alloc: Exec charged the store's charge (Store::sync)
    let oom = || {
        Box::new(PgError::error("out of memory".to_string()).with_sqlstate(types_error::ERRCODE_OUT_OF_MEMORY).with_detail(format!("Failed on request of size {} in a window's stored rows.", n * 8))) // alloc: Cold
    };
    let layout = std::alloc::Layout::array::<u64>(n).map_err(|_| oom())?;
    if layout.size() == 0 {
        return Ok(Words { ptr: core::ptr::NonNull::dangling(), len: 0, pooled: false });
    }
    if pool && layout.size() >= 1 << 10 {
        if let Some(b) = pagepool::take(layout.size(), 8, POOL_HINT) {
            return Ok(Words { ptr: b.cast::<u64>(), len: b.len() / 8, pooled: true });
        }
    }
    // SAFETY: a nonzero-size layout of `n` u64s.
    let p = unsafe { std::alloc::alloc_zeroed(layout) } as *mut u64; // alloc: Exec charged the store's charge (Store::sync)
    let ptr = core::ptr::NonNull::new(p).ok_or_else(oom)?;
    // `p` is a fresh global-allocator block of `n` zeroed (valid) u64s with `layout`, the layout `Words` frees with.
    Ok(Words { ptr, len: n, pooled: false })
}

/// The object's runtime (`Head::op`): the entry's operation, ERROR with the error stashed else.
pub(super) unsafe fn op(h: *mut abi::Head, ctx: *mut Ctx, code: u64, a: u64, b: u64) -> i64 {
    // SAFETY: `h` is the first field of a live `WinObj` (`call` checked its magic), laid for this execution.
    let obj = &*h.cast::<WinObj>();
    // the fill's operations take the store alone: a pulled fill runs them inside the object's own call
    let fill = match code {
        // CONVERT wave 2 (site 12): the store's heap segments go to the breaker the fill's pipeline runs for under its
        // kind (the statement's, Region, when none), looked up at its first stored row
        abi::OP_PUT => Some(obj.store.try_borrow_mut().map_err(|_| bad("m10_win_put: the stored rows in use")).and_then(|mut s| {
            if s.needs_owner() {
                s.own(OpArena::slot_owner_kind(ctx));
            }
            s.put(a as *const u64)
        })),
        abi::OP_ARGERR => Some(obj.store.try_borrow_mut().map_err(|_| bad("m10_win_argerr: the stored rows in use")).and_then(|mut s| s.argerr(a, b))),
        // the fill's open: its rows go; the walk restarts too unless a pull of the object's own call opened it (the
        // reader's open restarted it then)
        abi::OP_OPEN if a == abi::OPEN_FILL => Some(obj.store.try_borrow_mut().map_err(|_| bad("m10_win_open: the stored rows in use")).map(|mut s| {
            s.clear();
            if let Ok(mut rt) = obj.rt.try_borrow_mut() {
                rt.open(false);
            }
            0
        })),
        abi::OP_ROWS => Some(obj.store.try_borrow().map_err(|_| bad("m10_win_rows: the stored rows in use")).map(|s| s.nrows as i64)),
        abi::OP_REC => Some(obj.store.try_borrow().map_err(|_| bad("m10_win_rec: the stored rows in use")).and_then(|s| s.rec(a).map(|p| p as i64))),
        _ => None,
    };
    if let Some(r) = fill {
        return r.unwrap_or_else(|e| {
            gravity_fn::stash_error(e);
            ERROR
        });
    }
    let Ok(mut rt) = obj.rt.try_borrow_mut() else {
        gravity_fn::stash_error(bad("window.agg: a window object entered inside its own call"));
        return ERROR;
    };
    let r = match code {
        abi::OP_ROW => rt.row(a),
        abi::OP_RUNFAIL => rt.runfail(),
        abi::OP_OFFSETS => {
            let r = &mut *rt;
            r.f.set_offsets(&r.spec, a as *const u64).map(|()| 0)
        }
        // a reader's open: D3951, whether a later open walks the same stored rows again (no partition freed then)
        abi::OP_OPEN if a == abi::OPEN_READER || a == abi::OPEN_REWALK => {
            rt.rewalks = a == abi::OPEN_REWALK;
            rt.open(false);
            Ok(0)
        }
        abi::OP_OPEN => Err(bad("m10_win_open: no such open")),
        abi::OP_READS => rt.reads(a, b),
        abi::OP_SAMEPART => rt.same_part(a as *const u64).map(i64::from),
        abi::OP_ASK => rt.ask(a),
        abi::OP_PARAMS => match (rt.lazy.as_mut(), crate::ops::window::raw::param_records(a as *const u64, b)) {
            (Some(lz), Some(recs)) => lz.set_params(recs).map(|()| 0),
            (None, _) => Err(bad("m10_win_params: a window with no lazy argument")),
            (_, None) => Err(bad("m10_win_params: no values")),
        },
        _ => Err(bad(&format!("window.agg: no operation {code}"))), // alloc: Cold
    };
    r.unwrap_or_else(|e| {
        gravity_fn::stash_error(e);
        ERROR
    })
}

impl Store {
    /// PG `spool_tuples`' store: the row at `p` copied (its by-reference cells carved beside it, D3345), the marked
    /// cells' errors kept with it. 1 once the rows a pull wants are stored (the fill stops after this row), else 0.
    /// # Safety
    /// `p` is null or the IR's row to store (`m10_win_put`'s `rec`: `record_bytes(ncols)` bytes, live for the call).
    pub(crate) unsafe fn put(&mut self, p: *const u64) -> PgResult<i64> {
        if p.is_null() {
            return Err(bad("m10_win_put: a null row"));
        }
        if self.nrows as usize % CHUNK_ROWS == 0 {
            self.lay_chunk(self.nrows)?;
        }
        let ncols = self.cols.len();
        let at = self.at(self.nrows);
        core::ptr::copy_nonoverlapping(p, at, self.rw);
        let nulls = at.add(ncols) as *mut u8;
        // bounded("other: one cell per stored column of a row: the child's columns and the node's window function arguments, which the query text bounds; PG's spool stores the same row with no poll per column")
        for c in 0..ncols {
            let (typlen, byval) = self.cols[c];
            if byval || nulls.add(c).read() != 0 {
                continue;
            }
            let d = Datum::from_u64(at.add(c).read());
            if d.as_u64() == 0 {
                return Err(bad("m10_win_put: a by-reference cell with no datum"));
            }
            // D3584: a computed argument's wide value is not kept: the object evaluates it again where a function reads
            // it (`Rt::eval_lazy`), as PG's spool keeps the input row and WinGetFuncArg* evaluates the argument over it
            if self.defer.get(c).is_some_and(|&x| x) && wide(d, typlen) {
                at.add(c).write(0);
                nulls.add(c).write(abi::CELL_LAZY);
                continue;
            }
            at.add(c).write(self.copy_cell(d, typlen)?.as_u64());
        }
        // bounded("other: one kept error per marked cell of the row being stored, at most the row's stored columns, which the query text bounds")
        for (c, e) in self.pending.drain(..) {
            nulls.add(c as usize).write(2);
            self.errs.insert((self.nrows, c), e); // alloc: Exec
        }
        self.nrows += 1;
        Ok((self.nrows >= self.want) as i64)
    }
}

impl Store {
    /// D3345: by-reference value `d` of `typlen` copied into the store's segment for the row being stored (PG `datumCopy`'s
    /// bytes: a varlena's whole size, a toast pointer as it is; a cstring and its terminator); an expanded object's flat
    /// copy goes to `copies`.
    /// # Safety
    /// `d` is a live by-reference datum of `typlen`.
    unsafe fn copy_cell(&mut self, d: Datum, typlen: i16) -> PgResult<Datum> {
        let p = d.as_u64() as usize as *const u8;
        let size = match typlen {
            -1 if types_tuple::varatt::varatt_is_external_expanded(p) => return execexpr::agg_datum_copy(self.copies.mcx(), d, typlen),
            -1 => types_tuple::varatt::varsize_any(p),
            -2 => core::ffi::CStr::from_ptr(p as *const core::ffi::c_char).to_bytes().len() + 1,
            n if n > 0 => n as usize,
            _ => return execexpr::agg_datum_copy(self.copies.mcx(), d, typlen),
        };
        let dst = self.carve(size, self.nrows)?;
        core::ptr::copy_nonoverlapping(p, dst, size);
        Ok(Datum::from_u64(dst as u64))
    }
}

/// D3584: by-reference value `d` (of `typlen`) is wider than `gravity_program::cost::WINDOW_DEFER_BYTES`, or an expanded
/// object (its flat size is not read here).
/// # Safety
/// `d` is a live by-reference datum of `typlen`.
unsafe fn wide(d: Datum, typlen: i16) -> bool {
    let p = d.as_u64() as usize as *const u8;
    let limit = gravity_program::cost::WINDOW_DEFER_BYTES;
    match typlen {
        -1 => types_tuple::varatt::varatt_is_external_expanded(p) || types_tuple::varatt::varsize_any(p) > limit,
        -2 => core::ffi::CStr::from_ptr(p as *const core::ffi::c_char).to_bytes().len() >= limit,
        _ => false,
    }
}

impl Frame {
    /// PG `calculate_frame_offsets` over the IR's values at `vals` (`[start, start NULL, end, end NULL]`): NULL and
    /// negative (ROWS / GROUPS) offsets are PG's errors; the first call's values are kept.
    /// # Safety
    /// `vals` is null or the reader's four words (`m10_win_offsets`' `vals`, live for the call).
    pub(crate) unsafe fn set_offsets(&mut self, s: &WinSpec, vals: *const u64) -> PgResult<()> {
        if self.off.is_some() {
            return Ok(());
        }
        if vals.is_null() {
            return Err(bad("m10_win_offsets: no values"));
        }
        let mut out = [Datum::null(); 2];
        for (i, (flag, starting)) in [(FRAMEOPTION_START_OFFSET, true), (FRAMEOPTION_END_OFFSET, false)].into_iter().enumerate() {
            if self.fo & flag == 0 {
                continue;
            }
            let which = if starting { "starting" } else { "ending" };
            if vals.add(2 * i + 1).read() != 0 {
                return Err(Box::new(PgError::error(format!("frame {which} offset must not be null")).with_sqlstate(types_error::ERRCODE_NULL_VALUE_NOT_ALLOWED))); // alloc: Cold
            }
            let v = Datum::from_u64(vals.add(2 * i).read());
            let (typlen, byval) = s.offsets[i];
            out[i] = if byval { v } else { execexpr::agg_datum_copy(self.keep.mcx(), v, typlen)? };
            if self.fo & (FRAMEOPTION_ROWS | FRAMEOPTION_GROUPS) != 0 && v.as_i64() < 0 {
                return Err(Box::new(PgError::error(format!("frame {which} offset must not be negative")).with_sqlstate(types_error::ERRCODE_INVALID_PRECEDING_OR_FOLLOWING_SIZE))); // alloc: Cold
            }
        }
        self.off = Some(out);
        Ok(())
    }
}

/// Stored row `at`'s cell `c`: its datum word and its NULL byte (the row's layout: `ncols` datum words, then the NULL
/// bytes). Every caller passes a stored row's address (`Store::at` of a row below `nrows`, its positions inside the
/// partition) and a cell `c < ncols`; a chunk never moves while the store lives.
pub(super) fn cell_at(at: *const u64, ncols: usize, c: usize) -> (u64, u8) {
    // SAFETY: as above: `at` points at a stored row of `ncols` datum words and `ncols` NULL bytes, `c` one of its cells.
    unsafe { (at.add(c).read(), (at.add(ncols) as *const u8).add(c).read()) }
}

/// Validity byte `i` of the result cells' validity words `valid` set to `v`; every caller passes `i` below the
/// function count, so the byte lies inside the words (`nf.div_ceil(8)` of them).
pub(super) fn set_valid(valid: &mut [u64], i: usize, v: u8) {
    let bytes = valid.as_mut_ptr() as *mut u8;
    // SAFETY: byte `i` of the validity words, `i < nf`.
    unsafe { bytes.add(i).write(v) };
}

/// `f` bound with its `fn_expr` carrier (`get_fn_expr_argtype` reads `types`; its result type `rettype`; `coll` the
/// WindowFunc's inputcollid, the polymorphic OUT columns' input collation).
pub(super) fn bind(f: Oid, rettype: Oid, types: Vec<Oid>, coll: Oid, carriers: &mut Vec<(Box<AggFnArgTypes>, Box<[Oid]>)>) -> PgResult<FmgrInfo> {
    let mut info = fmgr_core::fmgr_info(f)?;
    let types = types.into_boxed_slice();
    // SAFETY: the slice's box lives in `carriers` as long as the FmgrInfo reading it.
    let argtypes: &'static [Oid] = unsafe { &*(&*types as *const [Oid]) };
    let carrier = Box::new(AggFnArgTypes { rettype, argtypes, variadic: false, stable: 0, inputcollid: coll }); // alloc: Exec
    // SAFETY: as above, the carrier's box outlives the FmgrInfo.
    info.fn_expr = Some(unsafe { FnExprErased::from_node_ref(&*carrier) });
    carriers.push((carrier, types)); // alloc: Exec
    Ok(info)
}

/// MakeExpandedObjectReadOnly.
pub(super) fn read_only(v: NullableDatum, typlen: i16) -> Datum {
    if v.isnull || typlen != -1 {
        return v.value;
    }
    // SAFETY: a non-NULL varlena datum points at a live image.
    unsafe { datum::expandeddatum::make_expanded_object_read_only_internal(v.value) }
}

/// By-reference datum `v` of a type of length `typlen` copied into `mcx` (PG `datumCopy`). Every caller passes a
/// non-NULL by-reference value alive for the call: an initcond its type's input function made (alive while its
/// FmgrInfo is) or kept for the execution, an argument cell, a transition result in the call context, or a final
/// result in the row context.
pub(super) fn datum_copy(mcx: Mcx<'_>, v: Datum, typlen: i16) -> PgResult<Datum> {
    // SAFETY: as above: `v` is a live by-reference datum of its type's length `typlen`.
    unsafe { execexpr::agg_datum_copy(mcx, v, typlen) }
}

/// The function call frame `fc` returns its by-reference result into `mcx`. Every caller's context lives across the
/// call and is not reset before the result is copied out or used (the call context, reset after the transition's copy;
/// the row context, until the object's next row).
pub(super) fn result_into(fc: &mut Fcinfo, mcx: Mcx<'_>) {
    // SAFETY: as above: the armed context stays live and un-reset across the call.
    unsafe { fc.set_result_mcx(mcx) };
}

/// The lazy arguments' context handle. Every state and the slot allocated in it drop before `cx` (the Lazy's field
/// order), and the handle is used only while the Lazy lives.
pub(super) fn lazy_mcx(cx: &PinnedContext) -> Mcx<'static> {
    // SAFETY: as above: every borrower of the handle ends before `cx` drops.
    unsafe { cx.handle() }
}

/// D1355: the execution's parameter list the lazy states read (`$n`), copied at prepare. `spec.params` is a heap box the
/// object's `spec` keeps (its address stable when the spec moves into the object) and drops after the states are last
/// evaluated (the object's lifetime).
pub(super) fn spec_params(spec: &WinSpec) -> &'static [ParamExternData] {
    // SAFETY: as above: the box outlives every state that reads the slice.
    unsafe { &*(&spec.params[..] as *const [ParamExternData]) }
}

/// Lazy state `st`'s results go to the row context `mcx` for one evaluation: the value is read within the row, before
/// the row context's next reset, and the caller restores the state's own context after the evaluation.
pub(super) fn arm_row_result(st: &mut ExprState<'_>, mcx: Mcx<'_>) {
    // SAFETY: as above: the row context outlives the evaluation and its struct stays put (the object owns it).
    unsafe { st.arm_result_mcx_raw(mcx) };
}

#[cfg(test)]
mod store_tests {
    /// A test store's layout lists, charged to no one.
    fn uncharged<T: Clone>(s: &[T]) -> memledger::ChargedVec<T> {
        let mut v = memledger::ChargedVec::<T>::new(None, memledger::Kind::Region);
        v.with_vec(|x| x.extend_from_slice(s)); // alloc: Cold
        v
    }

    use crate::ops::window::object::CHUNK_ROWS;
    use crate::ops::window::store::Store;
    use gravity_program::cost::WINDOW_SEG_MAX_BYTES;
    use mcx::MemoryContext;

    /// One stored row `n` as `Store::put` lays it: its chunk at a chunk's first row, its word, a by-reference copy carved
    /// beside it.
    fn store_row(s: &mut Store, n: u64, copy: usize) {
        if n as usize % CHUNK_ROWS == 0 {
            s.lay_chunk(n).unwrap();
        }
        let p = s.carve(copy, n).unwrap();
        // SAFETY: `at(n)` is row `n`'s words (at least two) in its laid chunk; `p` is `copy` bytes carved for it
        unsafe {
            s.at(n).write(n);
            core::ptr::write_bytes(p, n as u8, copy);
            s.at(n).add(1).write(p as u64);
        }
        s.nrows = n + 1;
    }

    fn read_row(s: &Store, n: u64, copy: usize) {
        s.held(n).unwrap();
        // SAFETY: row `n` is stored and held: its words and its copy live in unreleased segments
        unsafe {
            assert_eq!(s.at(n).read(), n, "row {n}'s word");
            let p = s.at(n).add(1).read() as *const u8;
            assert!((0..copy).all(|i| p.add(i).read() == n as u8), "row {n}'s copy");
        }
    }

    /// D3345: rows and their copies carved across segments read back after the segments before their partition are freed
    /// (PG release_partition): the heap drops to the live partitions' segments, the chunk numbers stay the rows', the
    /// watermark is each partition's start, and `clear` (the fill's next open) starts from no rows.
    #[test]
    fn partitions_release_their_segments() {
        let mut s = Store::new(uncharged(&[(8, true), (-1, false)]), uncharged(&[false; 2]), MemoryContext::new("t")); // alloc: Cold
        let (part, copy) = (3000u64, 200usize);
        let mut peak = 0;
        for n in 0..20 * part {
            if n > 0 && n % part == 0 {
                s.release_before(n);
                assert!(s.released == n && s.held(n).is_ok(), "the watermark at row {n}");
            }
            store_row(&mut s, n, copy);
            peak = peak.max(s.census().0);
            if n % part == part - 1 {
                for r in [n + 1 - part, n + 2 - part, n - CHUNK_ROWS as u64, n] {
                    read_row(&s, r, copy);
                }
            }
        }
        let one = part as usize * (copy + s.rw * 8);
        assert!(peak < 3 * one + WINDOW_SEG_MAX_BYTES, "the heap held {peak} bytes for partitions of {one}");
        assert_eq!(s.released, 19 * part, "the last partition's start");
        let (_, base, chunks, _) = s.census();
        assert!(base > 0 && chunks < 2 * part as usize / CHUNK_ROWS + 4, "{chunks} chunks held from {base}");
        s.clear();
        assert_eq!((s.nrows, s.released, s.census()), (0, 0, (0, 0, 0, 0)));
        for n in 0..2 * CHUNK_ROWS as u64 {
            store_row(&mut s, n, 24);
        }
        (0..2 * CHUNK_ROWS as u64).for_each(|n| read_row(&s, n, 24));
    }

    /// CONVERT wave 2 (site 12): the heap segments and the spare are on the owner's charge as they are made, released
    /// (a partition's start), kept as the spare and cleared, and leave it with the store.
    #[test]
    fn the_heap_segments_and_the_spare_follow_the_owners_charge() {
        std::thread::spawn(|| {
            let mut s = Store::new(uncharged(&[(8, true), (-1, false)]), uncharged(&[false; 2]), MemoryContext::new("t")); // alloc: Cold
            memledger::bind_backend(3996, 0);
            memledger::statement_begin();
            let (o, _) = memledger::charge::context();
            let committed = || memledger::charge::owner_row(o).committed;
            let base = committed();
            s.own(Some((o, memledger::Kind::Region)));
            let (part, copy) = (3000u64, 200usize);
            let mut peak = 0;
            for n in 0..6 * part {
                if n > 0 && n % part == 0 {
                    s.release_before(n);
                }
                store_row(&mut s, n, copy);
                let (held, charged) = s.charge_census();
                assert_eq!((committed() - base, charged), (held as i64, held), "row {n}");
                peak = peak.max(held);
            }
            assert!(peak >= (part as usize * copy) as u64, "the store held {peak} bytes");
            s.clear();
            let (held, _) = s.charge_census();
            assert!(held > 0 && committed() - base == held as i64, "the spare kept after a clear: {held}");
            drop(s);
            assert_eq!(committed(), base, "the store's bytes left the owner with it");
            memledger::statement_end();
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }
}
