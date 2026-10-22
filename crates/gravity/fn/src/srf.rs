//! The set-returning function library (P6, docs/gravity/lower.md §12): the bodies the lowered
//! SRF loop calls where the iterator is not emitted as IR. Every entry is a pure function of its
//! arguments over a state cell the caller owns (`STATE_BYTES`, on its stack) — `open` places the
//! state from the arguments, `step` writes the next row into `out` — and allocates only through
//! the per-slot bump context (reset at each `open`; a by-reference datum in the row points into it); the
//! error goes to the caller's `ErrSlot`, the status is the ABI's. The integer series, the array
//! walk over by-value elements and `generate_subscripts` are emitted as IR by the lowerer over
//! the same state layout (`ar::*`); `array_open` normalises the array image for them.
#![allow(clippy::missing_safety_doc, clippy::not_unsafe_ptr_arg_deref, clippy::too_many_arguments)]

use core::cell::{Cell, RefCell};

use adt_datetime::Interval;
use adt_jsonb::container::{container_is_array, container_is_object, container_is_scalar, JsonbItem};
use adt_jsonb::iter::{JsonbIterator, WjbToken};
use adt_numeric::series::GenerateSeriesNumeric;
use adt_numeric::Num;
use gravity_abi::Ctx;
use mcx::{Mcx, MemoryContext, PgVec};
use types_core::catalog::TEXTOID;
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};
use types_tuple::varatt;

use crate::{fail, ErrSlot, View, ERROR, OK};

/// The caller's state cell: room for the largest entry's state plus its header; not a tuning knob, `place` checks at
/// compile time that each entry's state fits (a larger state grows this bound, never spills).
pub const STATE_BYTES: usize = 512;
/// The state's first word is the slot whose bump context the entry allocates in (one per function of the loop).
pub const STATE_SLOT: usize = 0;
const HDR: usize = 8;
/// `step`'s row: `(datum, isnull)` per output column; the library's entries yield two at most, the
/// function-manager face (`srf_fmgr`) up to `OUT_WORDS / 2`.
pub const OUT_WORDS: usize = 128;

/// The array state after `array_open` (offsets into the cell; the IR walks it).
pub mod ar {
    pub const IMG: usize = 8;
    pub const NITEMS: usize = 16;
    pub const NEXT: usize = 24;
    pub const POS: usize = 32;
    pub const BITMAP: usize = 40;
    pub const ELMLEN: usize = 48;
    pub const BYVAL: usize = 56;
    pub const ALIGN: usize = 57;
    pub const NDIM: usize = 64;
    pub const DIMS: usize = 72;
    pub const LBOUND: usize = 80;
}

thread_local! {
    // alloc: Node
    static SLOTS: RefCell<Vec<Vec<Box<MemoryContext>>>> = const { RefCell::new(Vec::new()) }; // alloc: Node
    static LEVEL: Cell<usize> = const { Cell::new(0) };
    /// Per re-entry level, the slots a live execution holds (`reserve_slots`).
    static HELD: RefCell<Vec<Vec<bool>>> = const { RefCell::new(Vec::new()) }; // alloc: Node
}

/// Runs `f` on re-entry level `level`'s slot bank (M5 E1, M5-spi-reentrancy.md H8b): exec reentry.rs `enter` wraps
/// every served statement in its level, so a statement nested inside an open (a SQL / PL body run through SPI) numbers
/// its slots from 0 in its own bank and never resets a context the outer level's open still reads. Unwinding restores it.
// D3620: inlined, so a nested statement's stack path holds no frame of its own for it
#[inline(always)]
pub fn at_level<R>(level: usize, f: impl FnOnce() -> R) -> R {
    struct Back(usize);
    impl Drop for Back {
        fn drop(&mut self) {
            LEVEL.with(|l| l.set(self.0));
        }
    }
    let _back = Back(LEVEL.with(|l| l.replace(level)));
    f()
}

/// D3881: the re-entry levels past `levels` are gone (exec reentry frees deep levels): their slot banks go with them, from
/// the last one no live execution holds a slot of.
pub fn trim_levels(levels: usize) {
    let keep = HELD.with(|h| {
        let mut h = h.borrow_mut();
        let keep = h.iter().rposition(|x| x.iter().any(|&b| b)).map_or(0, |i| i + 1).max(levels);
        if h.len() > keep {
            h.truncate(keep);
            if h.capacity() > 4 * keep.max(16) {
                h.shrink_to(2 * keep.max(16));
            }
        }
        keep
    });
    SLOTS.with(|s| {
        let mut s = s.borrow_mut();
        if s.len() > keep {
            s.truncate(keep);
            if s.capacity() > 4 * keep.max(16) {
                s.shrink_to(2 * keep.max(16));
            }
        }
    });
}

/// M5 H-f (exec v4/m5.rs `SetFn` prepare): `(level, base)`, the first of `n` consecutive slots of this re-entry level's
/// bank no live execution holds, now held (first fit), so an idle cursor and the next statement at one level never
/// share a slot context.
pub fn reserve_slots(n: usize) -> (usize, usize) {
    let level = LEVEL.with(Cell::get);
    HELD.with(|h| {
        let mut h = h.borrow_mut();
        if h.len() <= level {
            h.resize_with(level + 1, Vec::new);
        }
        let held = &mut h[level];
        let base = (0..=held.len()).find(|&b| held.iter().skip(b).take(n).all(|&x| !x)).unwrap_or(held.len());
        if held.len() < base + n {
            held.resize(base + n, false);
        }
        held[base..base + n].fill(true);
        (level, base)
    })
}

/// M5 H-f (at the statement's end and on error): `reserve_slots`' slots reset (their memory given back but the keeper
/// block; a slot never opened is skipped) and released.
pub fn release_slots(level: usize, base: usize, n: usize) {
    SLOTS.with(|s| {
        if let Some(bank) = s.borrow_mut().get_mut(level) {
            bank.iter_mut().skip(base).take(n).for_each(|cx| cx.reset());
        }
    });
    HELD.with(|h| {
        if let Some(held) = h.borrow_mut().get_mut(level) {
            held.iter_mut().skip(base).take(n).for_each(|x| *x = false);
        }
    })
}

/// The slot's context, reset: the previous set of this function is done (its rows are pushed or
/// copied into the arena) when the function opens again; a slot first seen grows the pool. The
/// pool is one bank per re-entry level (`at_level`).
pub(crate) fn fresh_mcx(state: *const u8) -> Mcx<'static> {
    slot_cx(state, true)
}

/// The slot's context, reset first when `reset` (an open); else as the open left it (what the open adds after its body).
pub(crate) fn slot_cx(state: *const u8, reset: bool) -> Mcx<'static> {
    // SAFETY: the slot word is the caller's; one open at a time per slot per level per thread, the
    // boxes never move, and the state placed in the context is dropped by the reset.
    unsafe {
        let k = state.add(STATE_SLOT).cast::<u64>().read() as usize;
        let level = LEVEL.with(Cell::get);
        SLOTS.with(|s| {
            let mut banks = s.borrow_mut();
            while banks.len() <= level {
                banks.push(Vec::new()); // alloc: Node
            }
            let v = &mut banks[level];
            while v.len() <= k {
                v.push(Box::new(MemoryContext::new_bump("gravity_fn srf"))); // alloc: Node
            }
            let cx: *mut MemoryContext = &mut *v[k];
            if reset {
                (*cx).reset();
            }
            core::mem::transmute::<Mcx<'_>, Mcx<'static>>((*cx).mcx())
        })
    }
}

#[cold]
#[inline(never)]
fn invalid(msg: &str) -> Box<PgError> {
    Box::new(PgError::error(msg.to_string()).with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE)) // alloc: Cold
}

/// A varlena datum as a plain 4-byte-header image in `mcx` (M5 H-a: always a copy, rebuilt when short or toasted: an
/// open keeps pieces of its arguments, and a stop resumes the set after the caller's row memory is gone).
unsafe fn plain_image<'m>(mcx: Mcx<'m>, d: i64) -> PgResult<&'m [u8]> {
    let p = d as usize as *const u8;
    if varatt::varatt_is_4b_u(p) {
        return Ok(mcx::slice_in(mcx, core::slice::from_raw_parts(p, varatt::varsize_4b(p)))?.leak());
    }
    if varatt::varatt_is_1b(p) && !varatt::varatt_is_1b_e(p) {
        let total = varatt::varsize_1b(p);
        let n = total - 1 + 4;
        let mut img: PgVec<'m, u8> = mcx::vec_with_capacity_in(mcx, n)?;
        mcx::vec_append_bytes(&mut img, &datum::set_varsize_4b(n))?;
        mcx::vec_append_bytes(&mut img, core::slice::from_raw_parts(p.add(1), total - 1))?;
        return Ok(img.leak());
    }
    let raw = core::slice::from_raw_parts(p, varatt::varsize_any(p));
    Ok(detoast_seams::detoast_attr::call(mcx, raw)?.leak())
}

unsafe fn payload<'m>(mcx: Mcx<'m>, d: i64) -> PgResult<&'m [u8]> {
    Ok(&plain_image(mcx, d)?[4..])
}

unsafe fn arg(args: *const i64, nulls: *const u8, i: usize) -> Option<i64> {
    if nulls.add(i).read() != 0 {
        None
    } else {
        Some(args.add(i).read())
    }
}

pub(crate) unsafe fn place<T>(state: *mut u8, v: T) {
    const { assert!(core::mem::size_of::<T>() + HDR <= STATE_BYTES) };
    state.add(HDR).cast::<T>().write_unaligned(v);
}

pub(crate) unsafe fn state_of<'a, T>(state: *mut u8) -> &'a mut T {
    &mut *state.add(HDR).cast::<T>()
}

// ---- the array face: the plain image and its facts for the IR walk (unnest, generate_subscripts) ----

gravity_abi::host! {
    /// `array_open(e, ctx, state, datum, elmlen, byval, align)`: the array's plain image and its
    /// element facts into the state; an empty array (nitems 0) opens with nothing to walk.
    #[unwind]
    pub unsafe fn array_open(e: *mut ErrSlot, ctx: *mut Ctx, state: *mut u8, datum: i64, elmlen: i64, byval: i64, align: i64) -> i64 {
        let _ = ctx;
        let mcx = fresh_mcx(state);
        let img = match plain_image(mcx, datum) {
            Ok(i) => i,
            Err(err) => return fail(e, err).status,
        };
        let ndim = arrayfuncs::foundation::arr_ndim(img);
        let (_, dims) = arrayfuncs::foundation::read_dims(img);
        let nitems = match arrayutils::array_get_n_items(ndim, &dims) {
            Ok(n) => n,
            Err(err) => return fail(e, err).status,
        };
        let base = img.as_ptr();
        let w = |off: usize, v: u64| state.add(off).cast::<u64>().write_unaligned(v);
        w(ar::IMG, base as u64);
        w(ar::NITEMS, nitems as u64);
        w(ar::NEXT, 0);
        w(ar::POS, arrayfuncs::foundation::arr_data_offset(img) as u64);
        w(ar::BITMAP, arrayfuncs::foundation::arr_nullbitmap_off(img).map_or(0, |o| base.add(o) as u64));
        w(ar::ELMLEN, elmlen as u64);
        state.add(ar::BYVAL).write(byval as u8);
        state.add(ar::ALIGN).write(align as u8);
        w(ar::NDIM, ndim as u64);
        // header: 4 varlena + ndim 4 + dataoffset 4 + elemtype 4, then dims[ndim], lbound[ndim]
        w(ar::DIMS, base.add(16) as u64);
        w(ar::LBOUND, base.add(16 + 4 * ndim.max(0) as usize) as u64);
        OK
    }
}

gravity_abi::host! {
    /// The next element of an opened array with by-reference or variable-length elements:
    /// `out[0]` its datum (a pointer into the image), `out[1]` its null flag; 1, or 0 when exhausted.
    #[unwind]
    pub unsafe fn unnest_step(state: *mut u8, out: *mut i64) -> i64 {
        let r = |off: usize| state.add(off).cast::<u64>().read_unaligned();
        let (nitems, next) = (r(ar::NITEMS), r(ar::NEXT));
        if next >= nitems {
            return 0;
        }
        state.add(ar::NEXT).cast::<u64>().write_unaligned(next + 1);
        let bitmap = r(ar::BITMAP) as *const u8;
        if !bitmap.is_null() && bitmap.add(next as usize / 8).read() & (1 << (next % 8)) == 0 {
            out.write(0);
            out.add(1).write(1);
            return 1;
        }
        let img = r(ar::IMG) as *const u8;
        let mut pos = r(ar::POS) as usize;
        let elmlen = r(ar::ELMLEN) as i64 as i32;
        let p = img.add(pos);
        let d = arrayfuncs::foundation::fetch_att(p, state.add(ar::BYVAL).read() != 0, elmlen);
        pos = arrayfuncs::foundation::att_addlength_pointer(pos, elmlen, p);
        pos = arrayfuncs::foundation::att_align_nominal(pos, state.add(ar::ALIGN).read());
        state.add(ar::POS).cast::<u64>().write_unaligned(pos as u64);
        out.write(d.as_i64());
        out.add(1).write(0);
        1
    }
}

// ---- the series over numeric and time: a Rust step per row over the accumulator in the cell ----

gravity_abi::host! {
    /// `generate_series(numeric, numeric[, numeric])`.
    #[unwind]
    pub unsafe fn series_numeric_open(e: *mut ErrSlot, ctx: *mut Ctx, state: *mut u8, args: *const i64, nulls: *const u8, nargs: i64, _aux: i64) -> i64 {
        let _ = ctx;
        let mcx = fresh_mcx(state);
        let (Some(a0), Some(a1)) = (arg(args, nulls, 0), arg(args, nulls, 1)) else { return strict_empty::<SeriesNum>(state) };
        let step = if nargs >= 3 {
            match arg(args, nulls, 2) {
                Some(s) => Some(s),
                None => return strict_empty::<SeriesNum>(state),
            }
        } else {
            None
        };
        let r = (|| -> PgResult<GenerateSeriesNumeric> {
            let start = Num::from_payload(payload(mcx, a0)?);
            let stop = Num::from_payload(payload(mcx, a1)?);
            let step = match step {
                Some(s) => Some(Num::from_payload(payload(mcx, s)?)),
                None => None,
            };
            GenerateSeriesNumeric::new(start, stop, step)
        })();
        match r {
            Ok(g) => {
                place(state, Some(SeriesNum { g, last: None }));
                OK
            }
            Err(err) => fail(e, err).status,
        }
    }
}

/// The numeric series in the cell: the generator and the row it last handed out (M5 H-a: the datum points at it, valid
/// until the cell's next step, across a suspend; no arena copy per row).
struct SeriesNum {
    g: GenerateSeriesNumeric,
    last: Option<adt_numeric::var::NumericImage>,
}

fn strict_empty<T>(state: *mut u8) -> i64 {
    // SAFETY: the cell is the caller's.
    unsafe { place::<Option<T>>(state, None) };
    OK
}

gravity_abi::host! {
    #[unwind]
    pub unsafe fn series_numeric_step(e: *mut ErrSlot, _ctx: *mut Ctx, state: *mut u8, out: *mut i64) -> i64 {
        let cell = state_of::<Option<SeriesNum>>(state);
        let Some(s) = cell else { return 0 };
        match s.g.next() {
            Ok(Some(img)) => {
                out.write(s.last.insert(img).as_bytes().as_ptr() as i64);
                out.add(1).write(0);
                1
            }
            Ok(None) => {
                // the accumulators' digits may be heap-held (a wide numeric): dropped at the set's end, not left in the cell
                *cell = None;
                0
            }
            Err(err) => fail(e, err).status,
        }
    }
}

struct SeriesTime {
    current: i64,
    finish: i64,
    step: Interval,
    step_sign: i32,
    attimezone: Option<adt_datetime::tz::PgTzRef>,
    tz: bool,
}

gravity_abi::host! {
    /// `generate_series(timestamp[tz], timestamp[tz], interval[, zone text])`; `aux` bit 0: the tz form.
    #[unwind]
    pub unsafe fn series_time_open(e: *mut ErrSlot, ctx: *mut Ctx, state: *mut u8, args: *const i64, nulls: *const u8, nargs: i64, aux: i64) -> i64 {
        let _ = ctx;
        let mcx = fresh_mcx(state);
        let (Some(start), Some(finish), Some(sp)) = (arg(args, nulls, 0), arg(args, nulls, 1), arg(args, nulls, 2)) else { return strict_empty::<SeriesTime>(state) };
        let tz = aux & 1 != 0;
        let r = (|| -> PgResult<SeriesTime> {
            let step = View { ptr: sp as usize as *const u8, len: 16 }.interval();
            let attimezone = if tz && nargs >= 4 {
                match arg(args, nulls, 3) {
                    Some(z) => Some(adt_timestamp::lookup_timezone(payload(mcx, z)?)?),
                    None => return Err(invalid("strict")),
                }
            } else {
                None
            };
            let step_sign = adt_timestamp::interval::interval_sign(&step);
            if step_sign == 0 {
                return Err(invalid("step size cannot equal zero"));
            }
            if step.is_nobegin() || step.is_noend() {
                return Err(invalid("step size cannot be infinite"));
            }
            Ok(SeriesTime { current: start, finish, step, step_sign, attimezone, tz })
        })();
        match r {
            Ok(s) => {
                place(state, Some(s));
                OK
            }
            Err(err) if err.message() == "strict" => strict_empty::<SeriesTime>(state),
            Err(err) => fail(e, err).status,
        }
    }
}

gravity_abi::host! {
    #[unwind]
    pub unsafe fn series_time_step(e: *mut ErrSlot, _ctx: *mut Ctx, state: *mut u8, out: *mut i64) -> i64 {
        let Some(s) = state_of::<Option<SeriesTime>>(state) else { return 0 };
        let result = s.current;
        let cmp = adt_timestamp::timestamp_cmp_internal(result, s.finish);
        let more = if s.step_sign > 0 { cmp <= 0 } else { cmp >= 0 };
        if !more {
            return 0;
        }
        let next = if s.tz { adt_timestamp::interval::timestamptz_pl_interval_internal(s.current, &s.step, s.attimezone.as_deref()) } else { adt_timestamp::interval::timestamp_pl_interval(s.current, &s.step) };
        match next {
            Ok(n) => {
                s.current = n;
                out.write(result);
                out.add(1).write(0);
                1
            }
            Err(err) => fail(e, err).status,
        }
    }
}

// ---- the materialised kinds: the set computed at open into the slot's context, copied per row ----

#[derive(Clone, Copy)]
enum Item {
    Null,
    Text(*const u8, usize),
    Image(*const u8),
}

struct Mat {
    rows: PgVec<'static, [Item; 2]>,
    next: usize,
}

fn text_item(bytes: &[u8]) -> Item {
    Item::Text(bytes.as_ptr(), bytes.len())
}

fn image_item(bytes: &[u8]) -> Item {
    Item::Image(bytes.as_ptr())
}

unsafe fn finish_mat(e: *mut ErrSlot, state: *mut u8, r: PgResult<PgVec<'static, [Item; 2]>>) -> i64 {
    // M5 H-a: each text row made a varlena image once, in the slot's context, so a step hands out a pointer that stands
    // until the slot's next open (across a suspend and a resume), never a per-row arena copy
    let r = r.and_then(|mut rows| {
        let mcx = slot_cx(state, false);
        for it in rows.iter_mut().flat_map(|row| row.iter_mut()) {
            if let Item::Text(p, n) = *it {
                let mut img: PgVec<'static, u8> = mcx::vec_with_capacity_in(mcx, n + 4)?;
                mcx::vec_append_bytes(&mut img, &datum::set_varsize_4b(n + 4))?;
                mcx::vec_append_bytes(&mut img, core::slice::from_raw_parts(p, n))?;
                *it = Item::Image(img.leak().as_ptr());
            }
        }
        Ok(rows)
    });
    match r {
        Ok(rows) => {
            place(state, Mat { rows, next: 0 });
            OK
        }
        Err(err) => fail(e, err).status,
    }
}

unsafe fn empty_mat(state: *mut u8, mcx: Mcx<'static>) -> i64 {
    match mcx::vec_with_capacity_in(mcx, 0) {
        Ok(rows) => {
            place(state, Mat { rows, next: 0 });
            OK
        }
        Err(_) => ERROR,
    }
}

gravity_abi::host! {
    /// The next row of a materialised set: `out[2k]` / `out[2k + 1]` per column (a by-reference value points at its image in
    /// the slot's context, valid until the slot's next open); 1, 0 when exhausted.
    #[unwind]
    pub unsafe fn mat_step(_e: *mut ErrSlot, _ctx: *mut Ctx, state: *mut u8, out: *mut i64) -> i64 {
        let m = state_of::<Mat>(state);
        let Some(row) = m.rows.get(m.next) else { return 0 };
        m.next += 1;
        for (k, item) in row.iter().enumerate() {
            let (d, null) = match *item {
                Item::Null => (0, 1),
                // `finish_mat` made every text row an image
                Item::Text(p, _) | Item::Image(p) => (p as i64, 0),
            };
            out.add(2 * k).write(d);
            out.add(2 * k + 1).write(null);
        }
        1
    }
}

fn one<'m>(mcx: Mcx<'m>, n: usize) -> PgResult<PgVec<'m, [Item; 2]>> {
    mcx::vec_with_capacity_in(mcx, n)
}

gravity_abi::host! {
    /// `regexp_matches(text, text[, text])`: one `text[]` per match.
    #[unwind]
    pub unsafe fn regexp_matches_open(e: *mut ErrSlot, ctx: *mut Ctx, state: *mut u8, args: *const i64, nulls: *const u8, nargs: i64, coll: i64) -> i64 {
        let _ = ctx;
        let mcx = fresh_mcx(state);
        let (Some(s), Some(p)) = (arg(args, nulls, 0), arg(args, nulls, 1)) else { return empty_mat(state, mcx) };
        let flags = if nargs >= 3 {
            match arg(args, nulls, 2) {
                Some(f) => Some(f),
                None => return empty_mat(state, mcx),
            }
        } else {
            None
        };
        let r = (|| -> PgResult<PgVec<'static, [Item; 2]>> {
            let s = payload(mcx, s)?;
            let p = payload(mcx, p)?;
            let flags = match flags {
                Some(f) => Some(payload(mcx, f)?),
                None => None,
            };
            let mut cx = adt_regexp::matches::regexp_matches_setup(mcx, s, p, flags, coll as u32)?;
            let nm = cx.nmatches.max(0) as usize;
            let mut rows = one(mcx, nm)?;
            let np = cx.npatterns.max(0) as usize;
            while cx.next_match < cx.nmatches {
                let mut datums: PgVec<'_, datum::Datum> = mcx::vec_with_capacity_in(mcx, np)?;
                let mut nl: PgVec<'_, bool> = mcx::vec_with_capacity_in(mcx, np)?;
                adt_regexp::matches::build_regexp_match_result(&cx, |v| {
                    match v {
                        Some(bytes) => {
                            datums.push(fmgr::varlena_result(varlena::cstring_to_text(mcx, bytes.as_slice())?));
                            nl.push(false);
                        }
                        None => {
                            datums.push(datum::Datum::null());
                            nl.push(true);
                        }
                    }
                    Ok(())
                })?;
                let img = arrayfuncs::construct_md_array(mcx, &datums, Some(&nl), 1, &[np as i32], &[1], TEXTOID, -1, false, b'i')?;
                rows.push([image_item(img.leak()), Item::Null]);
                cx.next_match += 1;
            }
            Ok(rows)
        })();
        finish_mat(e, state, r)
    }
}

gravity_abi::host! {
    /// `regexp_split_to_table(text, text[, text])`.
    #[unwind]
    pub unsafe fn regexp_split_open(e: *mut ErrSlot, ctx: *mut Ctx, state: *mut u8, args: *const i64, nulls: *const u8, nargs: i64, coll: i64) -> i64 {
        let _ = ctx;
        let mcx = fresh_mcx(state);
        let (Some(s), Some(p)) = (arg(args, nulls, 0), arg(args, nulls, 1)) else { return empty_mat(state, mcx) };
        let flags = if nargs >= 3 {
            match arg(args, nulls, 2) {
                Some(f) => Some(f),
                None => return empty_mat(state, mcx),
            }
        } else {
            None
        };
        let r = (|| -> PgResult<PgVec<'static, [Item; 2]>> {
            let s = payload(mcx, s)?;
            let p = payload(mcx, p)?;
            let flags = match flags {
                Some(f) => Some(payload(mcx, f)?),
                None => None,
            };
            let mut cx = adt_regexp::matches::regexp_split_setup(mcx, s, p, flags, coll as u32, "regexp_split_to_table()")?;
            let mut rows = one(mcx, (cx.nmatches + 1).max(1) as usize)?;
            while cx.next_match <= cx.nmatches {
                let piece = adt_regexp::matches::build_regexp_split_result(&cx)?;
                rows.push([text_item(piece.leak()), Item::Null]);
                cx.next_match += 1;
            }
            Ok(rows)
        })();
        finish_mat(e, state, r)
    }
}

gravity_abi::host! {
    /// `string_to_table(text, text[, text])` (non-strict: a NULL string is no row, a NULL separator
    /// splits per character, a NULL null-string matches nothing).
    #[unwind]
    pub unsafe fn string_to_table_open(e: *mut ErrSlot, ctx: *mut Ctx, state: *mut u8, args: *const i64, nulls: *const u8, nargs: i64, coll: i64) -> i64 {
        let _ = ctx;
        let mcx = fresh_mcx(state);
        let Some(s) = arg(args, nulls, 0) else { return empty_mat(state, mcx) };
        let sep = arg(args, nulls, 1);
        let ns = if nargs >= 3 { arg(args, nulls, 2) } else { None };
        let coll = coll as u32;
        let r = (|| -> PgResult<PgVec<'static, [Item; 2]>> {
            let input = payload(mcx, s)?;
            let sep = match sep {
                Some(v) => Some(payload(mcx, v)?),
                None => None,
            };
            let ns = match ns {
                Some(v) => Some(payload(mcx, v)?),
                None => None,
            };
            let mut rows = one(mcx, 8)?;
            let push = |rows: &mut PgVec<'static, [Item; 2]>, field: &'static [u8]| -> PgResult<()> {
                let is_null = match ns {
                    Some(n) => varlena::texteq(field, n, coll)?,
                    None => false,
                };
                rows.push([if is_null { Item::Null } else { text_item(field) }, Item::Null]);
                Ok(())
            };
            match sep {
                Some(sep) => {
                    if input.is_empty() {
                    } else if sep.is_empty() {
                        push(&mut rows, input)?;
                    } else {
                        let mut st = varlena::text_position_setup(input, sep, coll)?;
                        let mut start = 0usize;
                        loop {
                            let found = varlena::text_position_next(&mut st)?;
                            let chunk = if found { &input[start..varlena::text_position_get_match_off(&st)] } else { &input[start..] };
                            push(&mut rows, chunk)?;
                            if !found {
                                break;
                            }
                            start = varlena::text_position_get_match_off(&st) + varlena::text_position_get_match_len(&st);
                        }
                    }
                }
                None => {
                    let mut off = 0usize;
                    while off < input.len() {
                        let l = mbutils::pg_mblen_range(&input[off..])? as usize;
                        push(&mut rows, &input[off..off + l])?;
                        off += l;
                    }
                }
            }
            Ok(rows)
        })();
        finish_mat(e, state, r)
    }
}

fn jsonb_text<'m>(mcx: Mcx<'m>, v: &JsonbItem<'_>) -> PgResult<Item> {
    Ok(match adt_jsonb::getfield::value_as_text(mcx, v)? {
        Some(t) => text_item(&t.into_image().leak()[4..]),
        None => Item::Null,
    })
}

fn jsonb_image<'m>(mcx: Mcx<'m>, v: JsonbItem<'_>) -> PgResult<Item> {
    Ok(image_item(adt_jsonb::build::item_to_jsonb_image(mcx, v)?.leak()))
}

gravity_abi::host! {
    /// `jsonb_array_elements[_text](jsonb)`; `aux` bit 0: the text form.
    #[unwind]
    pub unsafe fn jsonb_elements_open(e: *mut ErrSlot, ctx: *mut Ctx, state: *mut u8, args: *const i64, nulls: *const u8, _nargs: i64, aux: i64) -> i64 {
        let _ = ctx;
        let mcx = fresh_mcx(state);
        let Some(d) = arg(args, nulls, 0) else { return empty_mat(state, mcx) };
        let text = aux & 1 != 0;
        let r = (|| -> PgResult<PgVec<'static, [Item; 2]>> {
            let payload = payload(mcx, d)?;
            if container_is_scalar(payload) {
                return Err(invalid("cannot extract elements from a scalar"));
            }
            if !container_is_array(payload) {
                return Err(invalid("cannot extract elements from an object"));
            }
            let mut it = JsonbIterator::init(mcx, payload)?;
            let mut rows = one(mcx, 8)?;
            loop {
                let (tok, v) = it.next(true);
                match tok {
                    WjbToken::Done => break,
                    WjbToken::Elem => rows.push([if text { jsonb_text(mcx, &v)? } else { jsonb_image(mcx, v)? }, Item::Null]),
                    _ => {}
                }
            }
            Ok(rows)
        })();
        finish_mat(e, state, r)
    }
}

gravity_abi::host! {
    /// `jsonb_each[_text](jsonb)`: (key text, value jsonb | text); `aux` bit 0: the text form.
    #[unwind]
    pub unsafe fn jsonb_each_open(e: *mut ErrSlot, ctx: *mut Ctx, state: *mut u8, args: *const i64, nulls: *const u8, _nargs: i64, aux: i64) -> i64 {
        let _ = ctx;
        let mcx = fresh_mcx(state);
        let Some(d) = arg(args, nulls, 0) else { return empty_mat(state, mcx) };
        let text = aux & 1 != 0;
        let r = (|| -> PgResult<PgVec<'static, [Item; 2]>> {
            let payload = payload(mcx, d)?;
            if !container_is_object(payload) {
                return Err(invalid(if text { "cannot call jsonb_each_text on a non-object" } else { "cannot call jsonb_each on a non-object" }));
            }
            let mut it = JsonbIterator::init(mcx, payload)?;
            let mut rows = one(mcx, 8)?;
            loop {
                let (tok, k) = it.next(true);
                match tok {
                    WjbToken::Done => break,
                    WjbToken::Key => {
                        let JsonbItem::String(key) = k else { return Err(invalid("object key is not a string")) };
                        let (_, v) = it.next(true);
                        rows.push([text_item(key), if text { jsonb_text(mcx, &v)? } else { jsonb_image(mcx, v)? }]);
                    }
                    _ => {}
                }
            }
            Ok(rows)
        })();
        finish_mat(e, state, r)
    }
}

gravity_abi::host! {
    /// `jsonb_object_keys(jsonb)`.
    #[unwind]
    pub unsafe fn jsonb_keys_open(e: *mut ErrSlot, ctx: *mut Ctx, state: *mut u8, args: *const i64, nulls: *const u8, _nargs: i64, _aux: i64) -> i64 {
        let _ = ctx;
        let mcx = fresh_mcx(state);
        let Some(d) = arg(args, nulls, 0) else { return empty_mat(state, mcx) };
        let r = (|| -> PgResult<PgVec<'static, [Item; 2]>> {
            let payload = payload(mcx, d)?;
            if container_is_scalar(payload) {
                return Err(invalid("cannot call jsonb_object_keys on a scalar"));
            }
            if container_is_array(payload) {
                return Err(invalid("cannot call jsonb_object_keys on an array"));
            }
            let mut it = JsonbIterator::init(mcx, payload)?;
            let mut rows = one(mcx, 8)?;
            loop {
                let (tok, k) = it.next(true);
                match tok {
                    WjbToken::Done => break,
                    WjbToken::Key => {
                        let JsonbItem::String(key) = k else { return Err(invalid("object key is not a string")) };
                        rows.push([text_item(key), Item::Null]);
                    }
                    _ => {}
                }
            }
            Ok(rows)
        })();
        finish_mat(e, state, r)
    }
}

fn json_texts<'m>(mcx: Mcx<'m>, texts: Vec<Option<Vec<u8>>>) -> PgResult<PgVec<'m, [Item; 2]>> {
    let mut rows = one(mcx, texts.len())?;
    for t in texts {
        rows.push([
            match t {
                None => Item::Null,
                Some(b) => text_item(mcx::slice_in(mcx, &b)?.leak()),
            },
            Item::Null,
        ]);
    }
    Ok(rows)
}

gravity_abi::host! {
    /// `json_array_elements[_text](json)`; `aux` bit 0: the text form.
    #[unwind]
    pub unsafe fn json_elements_open(e: *mut ErrSlot, ctx: *mut Ctx, state: *mut u8, args: *const i64, nulls: *const u8, _nargs: i64, aux: i64) -> i64 {
        let _ = ctx;
        let mcx = fresh_mcx(state);
        let Some(d) = arg(args, nulls, 0) else { return empty_mat(state, mcx) };
        let text = aux & 1 != 0;
        let r = (|| -> PgResult<PgVec<'static, [Item; 2]>> {
            let json = payload(mcx, d)?;
            let name = if text { "json_array_elements_text" } else { "json_array_elements" };
            match adt_json::srfs::elements_rows(mcx, json, name, text)? {
                adt_json::srfs::SrfRows::Texts(t) => json_texts(mcx, t),
                adt_json::srfs::SrfRows::Tuples(_) => Err(invalid("json_array_elements yields no tuples")),
            }
        })();
        finish_mat(e, state, r)
    }
}

gravity_abi::host! {
    /// `json_each[_text](json)`: (key text, value json | text).
    #[unwind]
    pub unsafe fn json_each_open(e: *mut ErrSlot, ctx: *mut Ctx, state: *mut u8, args: *const i64, nulls: *const u8, _nargs: i64, aux: i64) -> i64 {
        let _ = ctx;
        let mcx = fresh_mcx(state);
        let Some(d) = arg(args, nulls, 0) else { return empty_mat(state, mcx) };
        let text = aux & 1 != 0;
        let r = (|| -> PgResult<PgVec<'static, [Item; 2]>> {
            let json = payload(mcx, d)?;
            let pairs = adt_json::srfs::each_pairs(mcx, json, text)?;
            let mut rows = one(mcx, pairs.len())?;
            for (k, v) in pairs {
                let key = text_item(mcx::slice_in(mcx, &k)?.leak());
                let val = match v {
                    None => Item::Null,
                    Some(b) => text_item(mcx::slice_in(mcx, &b)?.leak()),
                };
                rows.push([key, val]);
            }
            Ok(rows)
        })();
        finish_mat(e, state, r)
    }
}

gravity_abi::host! {
    /// `json_object_keys(json)`.
    #[unwind]
    pub unsafe fn json_keys_open(e: *mut ErrSlot, ctx: *mut Ctx, state: *mut u8, args: *const i64, nulls: *const u8, _nargs: i64, _aux: i64) -> i64 {
        let _ = ctx;
        let mcx = fresh_mcx(state);
        let Some(d) = arg(args, nulls, 0) else { return empty_mat(state, mcx) };
        let r = (|| -> PgResult<PgVec<'static, [Item; 2]>> {
            let json = payload(mcx, d)?;
            match adt_json::srfs::object_keys_rows(mcx, json)? {
                adt_json::srfs::SrfRows::Texts(t) => json_texts(mcx, t),
                adt_json::srfs::SrfRows::Tuples(_) => Err(invalid("json_object_keys yields no tuples")),
            }
        })();
        finish_mat(e, state, r)
    }
}

gravity_abi::host! {
    /// `jsonb_path_query(jsonb, jsonpath[, jsonb, bool])`; `aux` bit 0: the `_tz` form.
    #[unwind]
    pub unsafe fn jsonb_path_query_open(e: *mut ErrSlot, ctx: *mut Ctx, state: *mut u8, args: *const i64, nulls: *const u8, nargs: i64, aux: i64) -> i64 {
        let _ = ctx;
        let mcx = fresh_mcx(state);
        let (Some(jb), Some(jp)) = (arg(args, nulls, 0), arg(args, nulls, 1)) else { return empty_mat(state, mcx) };
        let (vars, silent) = if nargs >= 4 {
            match (arg(args, nulls, 2), arg(args, nulls, 3)) {
                (Some(v), Some(s)) => (Some(v), s != 0),
                _ => return empty_mat(state, mcx),
            }
        } else {
            (None, false)
        };
        let tz = aux & 1 != 0;
        let r = (|| -> PgResult<PgVec<'static, [Item; 2]>> {
            let jb = payload(mcx, jb)?;
            let path = plain_image(mcx, jp)?;
            let vars = match vars {
                Some(v) => adt_jsonpath_exec::JsonPathVars::Jsonb(payload(mcx, v)?),
                None => adt_jsonpath_exec::JsonPathVars::None,
            };
            let found = adt_jsonpath_exec::jsonb_path_query_core(mcx, jb, path, vars, silent, tz)?;
            let mut rows = one(mcx, found.len())?;
            for i in 0..found.len() {
                let img = found.image(mcx, i)?.expect("a row below the set's length");
                rows.push([image_item(img.leak()), Item::Null]);
            }
            Ok(rows)
        })();
        finish_mat(e, state, r)
    }
}

pub type OpenFn = unsafe extern "C-unwind" fn(*mut ErrSlot, *mut Ctx, *mut u8, *const i64, *const u8, i64, i64) -> i64;
pub type StepFn = unsafe extern "C-unwind" fn(*mut ErrSlot, *mut Ctx, *mut u8, *mut i64) -> i64;

/// The library entries by `pg_proc` OID: the open, the step, the output columns, `aux`'s value.
pub struct Entry {
    pub oid: u32,
    pub name: &'static str,
    pub open: OpenFn,
    pub step: StepFn,
    pub cols: u8,
    pub aux: i64,
    /// Every argument NULL-tested by the caller (a NULL: no row); `false`: the body decides.
    pub strict: bool,
    /// `aux` is the call's input collation.
    pub coll: bool,
}

const fn en(oid: u32, name: &'static str, open: OpenFn, step: StepFn, cols: u8, aux: i64, strict: bool) -> Entry {
    Entry { oid, name, open, step, cols, aux, strict, coll: false }
}

const fn co(oid: u32, name: &'static str, open: OpenFn, step: StepFn, strict: bool) -> Entry {
    Entry { oid, name, open, step, cols: 1, aux: 0, strict, coll: true }
}

/// The Rust-bodied entries (the emitted ones — the integer series, `generate_subscripts`, `unnest`
/// — are `EMITTED`); oid-ascending.
pub static ENTRIES: [Entry; 23] = [
    en(938, "generate_series_timestamp", series_time_open, series_time_step, 1, 0, true),
    en(939, "generate_series_timestamptz", series_time_open, series_time_step, 1, 1, true),
    en(1179, "jsonb_path_query_tz", jsonb_path_query_open, mat_step, 1, 1, true),
    co(2763, "regexp_matches", regexp_matches_open, mat_step, true),
    co(2764, "regexp_matches", regexp_matches_open, mat_step, true),
    co(2765, "regexp_split_to_table", regexp_split_open, mat_step, true),
    co(2766, "regexp_split_to_table", regexp_split_open, mat_step, true),
    en(3208, "jsonb_each", jsonb_each_open, mat_step, 2, 0, true),
    en(3219, "jsonb_array_elements", jsonb_elements_open, mat_step, 1, 0, true),
    en(3259, "generate_series_numeric", series_numeric_open, series_numeric_step, 1, 0, true),
    en(3260, "generate_series_numeric", series_numeric_open, series_numeric_step, 1, 0, true),
    en(3465, "jsonb_array_elements_text", jsonb_elements_open, mat_step, 1, 1, true),
    en(3931, "jsonb_object_keys", jsonb_keys_open, mat_step, 1, 0, true),
    en(3932, "jsonb_each_text", jsonb_each_open, mat_step, 2, 1, true),
    en(3955, "json_array_elements", json_elements_open, mat_step, 1, 0, true),
    en(3957, "json_object_keys", json_keys_open, mat_step, 1, 0, true),
    en(3958, "json_each", json_each_open, mat_step, 2, 0, true),
    en(3959, "json_each_text", json_each_open, mat_step, 2, 1, true),
    en(3969, "json_array_elements_text", json_elements_open, mat_step, 1, 1, true),
    en(4006, "jsonb_path_query", jsonb_path_query_open, mat_step, 1, 0, true),
    co(6160, "string_to_table", string_to_table_open, mat_step, false),
    co(6161, "string_to_table", string_to_table_open, mat_step, false),
    en(6274, "generate_series_timestamptz", series_time_open, series_time_step, 1, 1, true),
];

/// The entries the lowerer emits as IR over `array_open` / the argument words (no Rust body).
pub static EMITTED: [(u32, &str); 7] = [(1066, "generate_series_int4"), (1067, "generate_series_int4"), (1068, "generate_series_int8"), (1069, "generate_series_int8"), (1191, "generate_subscripts"), (1192, "generate_subscripts"), (2331, "unnest")];

pub fn entry_of(oid: u32) -> Option<&'static Entry> {
    ENTRIES.binary_search_by_key(&oid, |e| e.oid).ok().map(|i| &ENTRIES[i])
}

/// Every OID the library serves (a row of the tier table with status `srf`).
pub fn serves(oid: u32) -> bool {
    entry_of(oid).is_some() || EMITTED.iter().any(|(o, _)| *o == oid)
}
