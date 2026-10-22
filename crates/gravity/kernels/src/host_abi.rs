//! The C-ABI entry points of the base utilities, called by lowered IR directly (no trampoline in a lowerer): the site index is an argument.
pub(crate) use core::cmp::Ordering;
pub(crate) use core::mem::size_of;

pub(crate) use gravity_abi::breaker::{table_kind, NUM_SENT_HI, HashAgg, LAYOUT_IMPLICIT_COUNT, TABLE_T1, Owned, OwnerTop, Rec, RecordLayout, SlotKind, SortRun, TopK, T1, ACC_PAIRS, ADAPT_FACTOR, SORTRUN_HDR, HEAD_LEFT, HEAD_POS, LAYOUT_WORDS, TOPK_PAIRS, TOPK_SECOND_PAIR, OWNERTOP_HDR, SHARE, TABLE_OWNED, TABLE_SORT, T1_BYPASS, T1_EST, T1_HOLD, t1_default_bytes, t1_open_bytes, T1_HDR, T1_MAX_PARTS, IMG_BY_REF};
pub(crate) use gravity_abi::hash::KeyHash;
pub(crate) use gravity_abi::ctx::{resume, RESUME_NONE};
pub(crate) use gravity_abi::program::{consumed, partial, SPILL_RUNS, SPILL_RUN_ADOPT, SPILL_RUN_BASE, SPILL_RUN_PUT, SPILL_RUN_ROWS, SPILL_SCATTER};
pub(crate) use gravity_abi::status::{unsupported, DONE, ERROR, UNSUPPORTED_SCRATCH, UNSUPPORTED_SHAPE};
pub(crate) use gravity_abi::Ctx;

use gravity_abi::PtrWord;
use crate::contract::Host;

pub(crate) use crate::ops::sort::runmerge;
pub(crate) use crate::ops::sort::runmerge::MERGING;
pub(crate) use crate::ops::sort::raw::runmerge::merge_to_one;
pub(crate) use crate::util::{self, spill, HOST_STOP};

/// V4R-16: an entry's `$body` under the one landing pad, `contract::pad`: a panic in the kernel's own code (a bounds
/// check, an `expect`, a shape assert, a sort's comparator) is parked and the entry returns `$fail`, the value its caller
/// already reads as a failure. For the entries not yet written as `impl Host` (landing 12); the hosts the body calls
/// through the driver are D314's.
macro_rules! guard {
    ($fail:expr, $body:expr) => {
        match $crate::contract::pad(|| $body) {
            Some(v) => v,
            None => $fail,
        }
    };
}

#[macro_use]
pub mod v4;

pub use crate::table::fold_entries::*;
pub use crate::table::emit::*;
pub use crate::ops::sort::topk::*;
pub use crate::ops::sort::rowsort::*;
pub use crate::dict::codes::*;
pub use crate::typesem::keyimg::*;
pub use crate::ops::limit::limit_negative;
pub use crate::ops::mergejoin::entries::{m11_raise_no_proc, m11_run_held};
pub use crate::ops::sort::runs::{RUN_HELD, RUN_HELD_AT};

/// The record `record` / `record_consumed` hand back after a caught panic (V4R-16): their IR has no failure word and
/// folds into the pointer at once, so it gets `bytes` of zeroed words of its own instead of null (leaked: a bug's path,
/// once per caught panic; no TLS, D375); the program returns and the parked unwind raises before anything reads them.
#[cold]
#[inline(never)]
pub(crate) fn sunk(bytes: u64) -> u64 {
    Box::leak(vec![0u64; bytes as usize / 8 + 8].into_boxed_slice()).as_mut_ptr() as u64 // alloc: Cold
}

// The base entries (landing 12): each an `impl Host`, which the IR calls as `contract::entryN::<H>` under its landing pad.

pub struct Seek;
impl Host for Seek {
    type Args = (u64,);
    unsafe fn run(ctx: *mut Ctx, (site,): (u64,)) -> u64 {
        util::seek(ctx, site as usize) as u64
    }
}

pub struct RangeSeek;
impl Host for RangeSeek {
    type Args = (u64,);
    unsafe fn run(ctx: *mut Ctx, (site,): (u64,)) -> u64 {
        util::range_seek(ctx, site as usize) as u64
    }
}

pub struct Advance;
impl Host for Advance {
    type Args = (u64,);
    unsafe fn run(ctx: *mut Ctx, (site,): (u64,)) -> u64 {
        util::advance(ctx, site as usize) as u64
    }
}

pub struct Push;
impl Host for Push {
    type Args = ();
    unsafe fn run(ctx: *mut Ctx, (): ()) -> u64 {
        util::push(ctx) as u64
    }
}

pub struct Done;
impl Host for Done {
    type Args = ();
    unsafe fn run(ctx: *mut Ctx, (): ()) -> u64 {
        util::done(ctx) as u64
    }
}

/// D3406: a lowered check found its byte set (frame `cancel::check`): `gravity_abi::ctx::poll` says stop (1: the check
/// goes to its `CANCELLED` exit) or go on (0: the owner's CHECK_FOR_INTERRUPTS ran on the service bit). An unwind out of
/// the poll (a FATAL's thread exit) parks and stops.
pub struct Poll;
impl Host for Poll {
    type Args = ();
    unsafe fn run(ctx: *mut Ctx, (): ()) -> u64 {
        gravity_abi::ctx::poll(ctx) as u64
    }
    fn fail((): ()) -> u64 {
        1
    }
}

pub struct Stopped;
impl Host for Stopped {
    type Args = ();
    unsafe fn run(ctx: *mut Ctx, (): ()) -> u64 {
        util::stopped(ctx) as u64
    }
}

pub struct Scratch;
impl Host for Scratch {
    type Args = (u64,);
    unsafe fn run(ctx: *mut Ctx, (bytes,): (u64,)) -> u64 {
        util::scratch(ctx, bytes as usize) as u64
    }
}

pub struct LaneFill;
impl Host for LaneFill {
    type Args = (u64, u64);
    unsafe fn run(ctx: *mut Ctx, (site, k): (u64, u64)) -> u64 {
        gravity_abi::site::lane_fill(gravity_abi::ctx::site_ptr(ctx, site as usize), k as u32) as u64
    }
}

/// The one record of a plain aggregate's partial, opened on first use (count 0, every cell NULL): the fold's open and the emit's open (M2-work §B.5).
pub struct Record;
impl Host for Record {
    type Args = (u64, u64);
    unsafe fn run(ctx: *mut Ctx, (layout, bytes): (u64, u64)) -> u64 {
        HashAgg::at(partial(ctx), &*(layout as *const RecordLayout), 1, bytes as u32).single().as_ptr() as u64
    }
    fn fail((_, bytes): (u64, u64)) -> u64 {
        sunk(bytes)
    }
}

/// The emit's record: the consumed region's (a sink emit owns it too; an emit feeding a sort run owns the run's region instead).
pub struct RecordConsumed;
impl Host for RecordConsumed {
    type Args = (u64, u64);
    unsafe fn run(ctx: *mut Ctx, (layout, bytes): (u64, u64)) -> u64 {
        HashAgg::at(consumed(ctx, 0), &*(layout as *const RecordLayout), 1, bytes as u32).single().as_ptr() as u64
    }
    fn fail((_, bytes): (u64, u64)) -> u64 {
        sunk(bytes)
    }
}

/// D254: the calling pipeline's breaker region grown past its share (`gravity_abi::program::region_grow`, the driver's
/// spill op): `[0, head)` and the old `[len - tail, len)` kept at the new region's two ends, the new length the u32 at
/// the region's offset 8; the new 64-aligned base, or 0 with OutOfMemory held by the driver (the IR returns its HOST raise).
pub unsafe extern "C" fn region_grow(ctx: u64, need: u64, head: u64, tail: u64) -> u64 {
    gravity_abi::program::region_grow(ctx as *mut Ctx, need, head, tail)
}

/// op 14 (V4R-9): a build's held error raised by each probing participant (`gravity_abi::program::raise_held`, the
/// driver's spill op): the error status for the IR's raise exit when the build failed and its error was held, else 0.
pub unsafe extern "C" fn raise_held(ctx: *mut Ctx) -> i64 {
    gravity_abi::program::raise_held(ctx)
}

/// D1339 row C: `agg_gather` over consumed region `at & 0xff` (an embedded inner's group among its owner's regions), fresh
/// by `GATHER_FRESH` alone: the resume word is the owner's, never read or written here. Its array is `kept_array`'s.
pub unsafe extern "C" fn agg_gather_at(ctx: u64, desc: u64, at: u64, cache: u64) -> u64 {
    // the host ABI takes and answers every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
    crate::table::emit::gather_in(ctx as *mut Ctx, desc as *const u8, (at & 0xff) as usize, Some((at & GATHER_FRESH != 0, cache as *mut u64))) as u64
}

/// A count(DISTINCT) emit's one accumulator record (`pairs_total`) over consumed region `at`: an embedded inner's distinct
/// table among its owner's regions (two SubPlans in one select list, the count(DISTINCT) one past the first), gathered
/// fresh at every call; the resume word is the owner's, never read or written here.
pub unsafe extern "C" fn pairs_total_at(ctx: u64, desc: u64, at: u64) -> u64 {
    // the host ABI takes and answers every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
    crate::table::emit::pairs_total_in(ctx as *mut Ctx, desc as *const u8, (at & 0xff) as usize, Some(true)) as u64
}

/// `nest.rerun` (D1339 row D): `record_consumed` over the owner's consumed region `k`, a plain aggregate's record read by an
/// embedded inner that runs in its owner's call (two SubPlans over plain aggregates in one pipeline read regions 0 and 1).
/// No handle: region 0 keeps `record_consumed`, so a served statement's IR is unchanged. A call with no live region `k`
/// (a null inline region, or no `more` table past two) fails closed: the caught panic raises an ERROR, never a SIGSEGV.
pub unsafe extern "C" fn record_consumed_at(ctx: u64, layout: u64, bytes: u64, k: u64) -> u64 {
    guard!(sunk(bytes), {
        let p = &*(*(*(ctx as *mut Ctx)).program.ptr()).partial.ptr();
        let r = if k < 2 { p.consumed[k as usize].ptr() } else if p.more.ptr().is_null() { core::ptr::null_mut() } else { (*p.more.ptr().add(k as usize)).ptr() };
        assert!(!r.is_null(), "gravity: record_consumed_at: the owner's call has no consumed region {k} (D1339 row D)");
        HashAgg::at(r, &*(layout as *const RecordLayout), 1, bytes as u32).single().as_ptr() as u64
    })
}
