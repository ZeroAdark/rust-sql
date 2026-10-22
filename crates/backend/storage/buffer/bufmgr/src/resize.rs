//! Online buffer pool resize by extent ballooning.
//!
//! Grow activates the lowest retired (or never-used) extents. Shrink drains
//! extents top-down: the extent is marked DRAINING so every victim path
//! rejects its buffers, then each buffer is written out if dirty, moved into
//! a surviving buffer if clean and recently used, or dropped; an extent with
//! no valid and no pinned buffer is RETIRED and its block memory released.
//! Extents holding pins are skipped for lower ones and retried until the
//! deadline, so a long pin yields a partial shrink with its shortfall.
//! The hit path never looks at extent state. With the `buf_giveback` knob a
//! shrink that reaches its deadline keeps its DRAINING extents and the
//! background writer finishes it in bounded slices (`giveback`).

use core::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use init_small::globals;
use lwlock::{LWLockAcquire, LWLockConditionalAcquire, LWLockRelease, LW_EXCLUSIVE, LW_SHARED};
use mcx::{MemoryContext, PgVec};
use types_core::BLCKSZ;
use types_error::{
    PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE, ERRCODE_OBJECT_IN_USE,
    ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE,
};
use types_storage::buf::{
    IOContext, BM_CHECKPOINT_NEEDED, BM_DIRTY, BM_IO_IN_PROGRESS, BM_PERMANENT, BM_PIN_COUNT_WAITER,
    BM_TAG_VALID, BM_VALID, BUF_FLAG_MASK, BUF_USAGECOUNT_MASK,
};
use types_storage::waiteventset::{WL_EXIT_ON_PM_DEATH, WL_LATCH_SET, WL_TIMEOUT};

use crate::buf_hdr::{
    cleared_buftag, BufferDesc, BufferDescriptorGetBuffer, BufferGetBlockPtr, GetBufferDescriptor,
    LockBufHdr, UnlockBufHdr,
};
use crate::buf_table::{BufMappingPartitionLock, BufTableHashCode, BufTableSetId};
use crate::extent::{self, ACTIVE, DRAINING, RETIRED, UNBORN};
use crate::pin::{buffer_refcount, buffer_usagecount, PinBuffer_Locked, UnpinBuffer};
use crate::privref::ReservePrivateRefCountEntry;

static RESIZING: AtomicBool = AtomicBool::new(false);
/// buf_grow: the pool's grow goal (buffers; 0 = none), raised by `giveback::grow_request`, applied by the background
/// writer (`apply_grow`); and the grows applied and the buffers they activated since start.
pub(crate) static GROW_GOAL: AtomicI32 = AtomicI32::new(0);
pub(crate) static GROWS: AtomicU64 = AtomicU64::new(0);
pub(crate) static GROWN: AtomicU64 = AtomicU64::new(0);

/// buf_grow (the background writer's cycle, before a give-back slice): the pending grow goal's extents activated, lowest
/// first (`grow`). A pending give-back drain wins (the goal is dropped), as does recovery; a running resize call is left
/// alone and the goal kept for the next cycle.
pub(crate) fn apply_grow() {
    if GROW_GOAL.load(Ordering::Acquire) == 0 {
        return;
    }
    let recovering = transam_xlog_seams::recovery_in_progress::is_installed() && transam_xlog_seams::recovery_in_progress::call();
    if BG_GOAL.load(Ordering::Acquire) != 0 || recovering {
        GROW_GOAL.store(0, Ordering::Release);
        return;
    }
    if RESIZING.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        return;
    }
    let goal = GROW_GOAL.swap(0, Ordering::AcqRel).min(extent::max_buffers());
    let before = extent::active_buffers();
    if goal > before && BG_GOAL.load(Ordering::Acquire) == 0 {
        grow(goal);
        let after = extent::active_buffers();
        TARGET_BUFFERS.store(after, Ordering::Relaxed);
        GROWS.fetch_add(1, Ordering::Relaxed);
        GROWN.fetch_add((after - before).max(0) as u64, Ordering::Relaxed);
    }
    RESIZING.store(false, Ordering::Release);
}

/// buf_grow: (grows applied, bytes they activated) since start.
pub fn grow_counts() -> (u64, u64) {
    (GROWS.load(Ordering::Relaxed), GROWN.load(Ordering::Relaxed).saturating_mul(BLCKSZ as u64))
}
static TARGET_BUFFERS: AtomicI32 = AtomicI32::new(0);
static RELOCATED: AtomicU64 = AtomicU64::new(0);
static WRITTEN: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// The background drain's target in buffers (0: none pending); written under `RESIZING`, read anywhere.
pub(crate) static BG_GOAL: AtomicI32 = AtomicI32::new(0);
static BG_BACKOFF_MS: AtomicI32 = AtomicI32::new(1);
pub(crate) static BG_T0_NS: AtomicU64 = AtomicU64::new(0);
pub(crate) static BG_END_NS: AtomicU64 = AtomicU64::new(0);
pub(crate) static BG_W0: AtomicU64 = AtomicU64::new(0);
pub(crate) static BG_DIRTY: AtomicI32 = AtomicI32::new(0);
pub(crate) static BG_SLICES: AtomicU64 = AtomicU64::new(0);
static BG_TOKENS: AtomicI64 = AtomicI64::new(0);
static BG_TOKENS_NS: AtomicU64 = AtomicU64::new(0);
static BG_WAL_SINCE_NS: AtomicU64 = AtomicU64::new(0);
const WAL_FORCE_NS: u64 = 200_000_000;
const CARRY_SLOTS: usize = 64;
static CARRY: [AtomicU64; CARRY_SLOTS] = [const { AtomicU64::new(0) }; CARRY_SLOTS];

const BACKOFF_MAX_MS: i64 = 50;
const DEMOTE_AFTER_FAILS: u32 = 2;
/// The background drain works its in-flight extents top-down, at most this many at a time (pinned ones, at
/// DEMOTE_AFTER_FAILS, are retried outside the count), so each extent's clean pages go and its dirty pages are written
/// before the next one starts: the pool comes back in step with the writes, not all at the end.
pub(crate) const GIVEBACK_WINDOW: usize = 8;
/// Clock-sweep candidates a paced relocation looks at for a clean victim before it drops its page instead.
const CLEAN_VICTIM_TRIES: u32 = 8;

/// Result of one resize call, in bytes: the pool size reached, the size a
/// pin-free run of this call reaches, and how far above it the call stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResizeOutcome {
    pub active_bytes: i64,
    pub target_bytes: i64,
    pub shortfall_bytes: i64,
}

/// Snapshot for the `pgrust_buffer_pool` view.
#[derive(Clone, Copy, Debug)]
pub struct PoolStats {
    pub max_bytes: i64,
    pub boot_bytes: i64,
    pub active_bytes: i64,
    pub target_bytes: i64,
    pub resident_bytes: i64,
    pub draining_extents: i64,
    pub retired_extents: i64,
    pub relocated: i64,
    pub written: i64,
    pub dropped: i64,
    pub resizing: bool,
}

fn bytes(buffers: i32) -> i64 {
    buffers as i64 * BLCKSZ as i64
}

#[cold]
fn param_error(msg: String, hint: Option<String>) -> Box<PgError> {
    let mut e = PgError::error(msg).with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE);
    if let Some(h) = hint {
        e = e.with_hint(h);
    }
    Box::new(e)
}

/// Resize the shared pool to `target_bytes` (rounded up to whole extents),
/// giving a shrink `timeout_ms` to drain. Refusals are loud and typed: a
/// target above pgrust.buffer_pool_max or below the pin floor, a negative
/// timeout, recovery in progress, or another resize already running.
pub fn BufferPoolResize(target_bytes: i64, timeout_ms: i32) -> PgResult<ResizeOutcome> {
    if timeout_ms < 0 {
        return Err(param_error(format!("timeout must not be negative: {timeout_ms}"), None));
    }
    let max = extent::max_buffers();
    let floor = extent::pin_floor_buffers();
    let target = (target_bytes.max(0) as i128 + BLCKSZ as i128 - 1) / BLCKSZ as i128;
    if target > max as i128 {
        return Err(param_error(
            format!(
                "buffer pool target {target_bytes} bytes exceeds \"pgrust.buffer_pool_max\" ({} bytes)",
                bytes(max)
            ),
            Some("Raise \"pgrust.buffer_pool_max\" and restart the server.".to_string()),
        ));
    }
    if target < floor as i128 {
        return Err(param_error(
            format!(
                "buffer pool target {target_bytes} bytes is below the pin floor ({} bytes)",
                bytes(floor)
            ),
            Some(format!(
                "Every process that can pin buffers keeps {} buffers.",
                extent::PIN_FLOOR_PER_PROC
            )),
        ));
    }
    if transam_xlog_seams::recovery_in_progress::is_installed()
        && transam_xlog_seams::recovery_in_progress::call()
    {
        return Err(Box::new(
            PgError::error("buffer pool cannot be resized during recovery")
                .with_sqlstate(ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE),
        ));
    }
    if RESIZING
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return Err(Box::new(
            PgError::error("a buffer pool resize is already in progress")
                .with_sqlstate(ERRCODE_OBJECT_IN_USE),
        ));
    }
    let r = run(target as i32, timeout_ms);
    RESIZING.store(false, Ordering::Release);
    r
}

fn run(target: i32, timeout_ms: i32) -> PgResult<ResizeOutcome> {
    abandon_background();
    let active = extent::active_buffers();
    let goal = if target >= active { grow_goal(target) } else { shrink_goal(target) };
    TARGET_BUFFERS.store(goal, Ordering::Relaxed);
    if goal > active {
        grow(target);
    } else if goal < active {
        shrink(target, timeout_ms)?;
    }
    let active = extent::active_buffers();
    Ok(ResizeOutcome {
        active_bytes: bytes(active),
        target_bytes: bytes(goal),
        shortfall_bytes: bytes((active - goal).max(0)),
    })
}

/// Lowest-first activation reaches the first sum >= target.
fn grow_goal(target: i32) -> i32 {
    let mut a = extent::active_buffers();
    for e in 0..extent::n_extents() {
        if a >= target {
            break;
        }
        let st = extent::state(e);
        if st == RETIRED || st == UNBORN {
            let (lo, hi) = extent::extent_range(e);
            a += hi - lo;
        }
    }
    a
}

/// Top-down retirement stops before the sum would drop below target; DRAINING extents are already on their way out.
fn shrink_goal(target: i32) -> i32 {
    let mut a = extent::active_buffers();
    for e in 0..extent::n_extents() {
        if extent::state(e) == DRAINING {
            a -= extent_len(e);
        }
    }
    let mut e = extent::n_extents() - 1;
    while e >= 0 {
        if extent::state(e) == ACTIVE {
            let (lo, hi) = extent::extent_range(e);
            if a - (hi - lo) < target {
                break;
            }
            a -= hi - lo;
        }
        e -= 1;
    }
    a
}

fn grow(target: i32) {
    let mut active = extent::active_buffers();
    for e in 0..extent::n_extents() {
        if active >= target {
            break;
        }
        let st = extent::state(e);
        if st != RETIRED && st != UNBORN {
            continue;
        }
        let (lo, hi) = extent::extent_range(e);
        if st == UNBORN {
            crate::buf_hdr::init_descriptor_range(lo, hi);
        }
        extent::set_state(e, ACTIVE);
        crate::freelist::StrategyFreeBufferRange(lo, hi);
        active += hi - lo;
        extent::set_active_buffers(active);
        if hi > extent::span() {
            extent::set_span(hi);
        }
    }
}

#[derive(Clone, Copy)]
struct InFlight {
    extent: i32,
    fails: u32,
}

/// How a drain writes a dirty buffer. Inline (a resize call): at once, waiting for its content lock and its WAL flush.
/// Paced (the background): within the write budget, only when its content lock is free and its WAL already flushed
/// (`force_wal`: flushed by the drain), else left for a later pass.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Writes {
    Inline,
    Paced { force_wal: bool },
}

/// One buffer's outcome: emptied, held (pinned or under IO: counts toward demotion), or its write left for later.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Drained {
    Done,
    Held,
    Later,
}

struct Drain<'a> {
    writes: Writes,
    later: u32,
    wal_later: u32,
    target: i32,
    deadline: pg_clock::Deadline,
    started: pg_clock::MonoStamp,
    rate: i64,
    relocate: bool,
    visited: u64,
    cursor: i32,
    deferred: PgVec<'a, i32>,
    deferred_next: usize,
    pushback: Option<i32>,
    inflight: PgVec<'a, InFlight>,
    draining: i32,
}

fn shrink(target: i32, timeout_ms: i32) -> PgResult<()> {
    let cx = MemoryContext::new("buffer pool resize");
    let mcx = cx.mcx();
    let n = extent::n_extents().max(1) as usize;
    let started = pg_clock::MonoStamp::now();
    let mut d = Drain {
        writes: Writes::Inline,
        later: 0,
        wal_later: 0,
        target,
        deadline: started.deadline_after(Duration::from_millis(timeout_ms as u64)),
        started,
        rate: read_rate(),
        relocate: relocate_on(),
        visited: 0,
        cursor: extent::n_extents() - 1,
        deferred: mcx::vec_with_capacity_in(mcx, n)?,
        deferred_next: 0,
        pushback: None,
        inflight: mcx::vec_with_capacity_in(mcx, n)?,
        draining: 0,
    };
    let r = drain_loop(&mut d);
    let mut cleanup: PgResult<()> = Ok(());
    if matches!(r, Ok(false)) && giveback_on() {
        save_carry(&d.inflight);
        BG_DIRTY.store(dirty_draining(), Ordering::Relaxed);
        begin_background(target);
    } else {
        for f in d.inflight.iter() {
            if let Err(e) = revert(f.extent) {
                cleanup = Err(e);
            }
        }
    }
    r?;
    cleanup?;
    crate::buf_table::BufTableShrinkToFit(extent::active_buffers())?;
    Ok(())
}

fn relocate_on() -> bool {
    !guc_tables::vars::pgrust_buffer_pool_relocate.installed() || guc_tables::vars::pgrust_buffer_pool_relocate.read()
}

fn read_rate() -> i64 {
    if guc_tables::vars::pgrust_buffer_pool_drain_rate.installed() {
        guc_tables::vars::pgrust_buffer_pool_drain_rate.read() as i64
    } else {
        0
    }
}

fn extent_len(e: i32) -> i32 {
    let (lo, hi) = extent::extent_range(e);
    hi - lo
}

/// Buffers still to release beyond the extents already draining. Never
/// negative: the ACTIVE extents always hold at least the target, so relocated
/// pages always find surviving buffers.
fn room(d: &Drain<'_>) -> i32 {
    extent::active_buffers() - d.draining - d.target
}

/// The next shrink candidate, top-down: first extents with no pin at a
/// racy precheck, then the skipped ones in the same order.
fn next_candidate(d: &mut Drain<'_>) -> Option<i32> {
    if let Some(e) = d.pushback.take() {
        return Some(e);
    }
    while d.cursor >= 0 {
        let e = d.cursor;
        d.cursor -= 1;
        if extent::state(e) != ACTIVE {
            continue;
        }
        if extent_has_pin(e) {
            d.deferred.push(e);
            continue;
        }
        return Some(e);
    }
    while d.deferred_next < d.deferred.len() {
        let e = d.deferred[d.deferred_next];
        d.deferred_next += 1;
        if extent::state(e) == ACTIVE {
            return Some(e);
        }
    }
    None
}

fn extent_has_pin(e: i32) -> bool {
    let (lo, hi) = extent::extent_range(e);
    (lo..hi).any(|b| buffer_refcount(GetBufferDescriptor(b).state.load(Ordering::Relaxed)) > 0)
}

/// The whole plan is marked DRAINING before any buffer moves, so relocated
/// pages land only in extents that survive this call. An extent held by a
/// pin is retried with backoff; after DEMOTE_AFTER_FAILS fruitless passes,
/// while an untried candidate remains, it goes back to ACTIVE and the next
/// candidate takes its place. Each demotion consumes an untried candidate,
/// so the loop never cycles; once none is left it waits on what is in
/// flight until the deadline. True = finished, false = the deadline.
fn drain_loop(d: &mut Drain<'_>) -> PgResult<bool> {
    let mut backoff_ms: i64 = 1;
    loop {
        postgres_seams::check_for_interrupts::call()?;
        plan(d);
        if d.inflight.is_empty() {
            return Ok(true);
        }
        let progressed = match pass_inflight(d, false)? {
            Some(p) => p,
            None => return Ok(false),
        };
        if d.inflight.is_empty() {
            continue;
        }
        if d.deadline.expired() {
            return Ok(false);
        }
        if progressed {
            backoff_ms = 1;
            continue;
        }
        demote_stuck(d)?;
        sleep_ms(backoff_ms.min(d.deadline.remaining_ms().max(1)))?;
        backoff_ms = (backoff_ms * 2).min(BACKOFF_MAX_MS);
    }
}

fn plan(d: &mut Drain<'_>) {
    while let Some(e) = next_candidate(d) {
        if room(d) < extent_len(e) {
            d.pushback = Some(e);
            break;
        }
        extent::set_state(e, DRAINING);
        d.inflight.push(InFlight { extent: e, fails: 0 });
        d.draining += extent_len(e);
    }
}

/// One pass over the extents in flight, retiring the cleared ones. Some(progressed), or None at the deadline
/// (`one`: after at least one extent's pass).
fn pass_inflight(d: &mut Drain<'_>, one: bool) -> PgResult<Option<bool>> {
    let window = if d.writes == Writes::Inline { usize::MAX } else { GIVEBACK_WINDOW };
    let mut open = 0;
    let mut progressed = false;
    let mut i = 0;
    let mut passes = 0;
    while i < d.inflight.len() {
        if (passes > 0 || !one) && d.deadline.expired() {
            return Ok(None);
        }
        let f = d.inflight[i];
        if f.fails < DEMOTE_AFTER_FAILS {
            if open == window {
                break;
            }
            open += 1;
        }
        passes += 1;
        let (clear, held) = drain_pass(d, f.extent)?;
        if clear {
            d.inflight.remove(i);
            d.draining -= extent_len(f.extent);
            retire(f.extent)?;
            progressed = true;
            continue;
        }
        if held || d.writes == Writes::Inline {
            d.inflight[i].fails += 1;
        }
        i += 1;
    }
    Ok(Some(progressed))
}

fn demote_stuck(d: &mut Drain<'_>) -> PgResult<()> {
    let untried = d.pushback.is_some() || d.cursor >= 0 || d.deferred_next < d.deferred.len();
    if !untried {
        return Ok(());
    }
    let stuck = d
        .inflight
        .iter()
        .enumerate()
        .filter(|(_, f)| f.fails >= DEMOTE_AFTER_FAILS)
        .max_by_key(|(_, f)| f.fails)
        .map(|(idx, _)| idx);
    if let Some(idx) = stuck {
        let f = d.inflight.remove(idx);
        d.draining -= extent_len(f.extent);
        revert(f.extent)?;
    }
    Ok(())
}

/// One pass over extent `e`: (clear, held). Clear when every buffer was observed invalid and unpinned under its header
/// lock after the DRAINING flip; held when one was pinned or under IO.
fn drain_pass(d: &mut Drain<'_>, e: i32) -> PgResult<(bool, bool)> {
    let (lo, hi) = extent::extent_range(e);
    let (mut clear, mut held) = (true, false);
    for buf_id in lo..hi {
        match drain_buffer(d, buf_id)? {
            Drained::Done => {}
            Drained::Held => {
                clear = false;
                held = true;
            }
            Drained::Later => {
                clear = false;
                d.later += 1;
            }
        }
    }
    d.visited += (hi - lo) as u64;
    pace(d)?;
    Ok((clear, held))
}

fn pace(d: &Drain<'_>) -> PgResult<()> {
    if d.rate <= 0 {
        return Ok(());
    }
    let due_ns = (d.visited as u128 * 1_000_000_000 / d.rate as u128) as u64;
    let elapsed = d.started.elapsed_ns();
    if due_ns > elapsed {
        let ms = ((due_ns - elapsed) / 1_000_000) as i64;
        let ms = ms.min(d.deadline.remaining_ms());
        if ms > 0 {
            sleep_ms(ms)?;
        }
    }
    Ok(())
}

fn sleep_ms(ms: i64) -> PgResult<()> {
    postgres_seams::check_for_interrupts::call()?;
    if let Some(latch) = globals::MyLatch() {
        latch::WaitLatch(Some(latch), WL_LATCH_SET | WL_TIMEOUT | WL_EXIT_ON_PM_DEATH, ms, 0)?;
        latch::ResetLatch(latch);
    }
    postgres_seams::check_for_interrupts::call()
}

/// Empties one buffer of a DRAINING extent, or says why not (pinned or under IO, or re-pinned while we worked: Held;
/// a paced write left for later: Later).
fn drain_buffer(d: &mut Drain<'_>, buf_id: i32) -> PgResult<Drained> {
    let desc = GetBufferDescriptor(buf_id);
    ReservePrivateRefCountEntry();
    crate::pin::resowner_enlarge_for_pin()?;
    let st = LockBufHdr(desc);
    if buffer_refcount(st) > 0 || st & BM_IO_IN_PROGRESS != 0 {
        UnlockBufHdr(desc, st);
        return Ok(Drained::Held);
    }
    if st & BM_TAG_VALID == 0 {
        UnlockBufHdr(desc, st);
        return Ok(Drained::Done);
    }
    PinBuffer_Locked(desc);
    let r = evacuate(d, desc, st);
    UnpinBuffer(desc);
    r
}

/// Caller holds our pin on `desc` (refcount was 0 before it).
fn evacuate(d: &mut Drain<'_>, desc: &BufferDesc, st: u32) -> PgResult<Drained> {
    if st & BM_DIRTY != 0 {
        if let Writes::Paced { force_wal } = d.writes {
            if !LWLockConditionalAcquire(&desc.content_lock, LW_SHARED)? {
                return Ok(Drained::Later);
            }
            let hdr = LockBufHdr(desc);
            let lsn = crate::ops::buffer_page_get_lsn(crate::buf_hdr::BufferDescriptorGetBuffer(desc));
            UnlockBufHdr(desc, hdr);
            let wal_waits = hdr & BM_PERMANENT != 0 && !force_wal && transam_xlog_seams::xlog_needs_flush::call(lsn);
            if wal_waits || !write_token() {
                LWLockRelease(&desc.content_lock)?;
                d.wal_later += wal_waits as u32;
                return Ok(Drained::Later);
            }
        } else {
            LWLockAcquire(&desc.content_lock, LW_SHARED, globals::MyProcNumber())?;
        }
        let flushed = crate::write::FlushBuffer(desc, IOContext::IOCONTEXT_NORMAL);
        LWLockRelease(&desc.content_lock)?;
        flushed?;
        WRITTEN.fetch_add(1, Ordering::Relaxed);
    }
    let st = desc.state.load(Ordering::Acquire);
    let clean_valid = st & BM_VALID != 0
        && st & (BM_DIRTY | BM_CHECKPOINT_NEEDED | BM_IO_IN_PROGRESS) == 0;
    if d.relocate && clean_valid && buffer_usagecount(st) >= 1 {
        match relocate_buffer(desc, d.writes != Writes::Inline)? {
            Relocated::Moved => {
                RELOCATED.fetch_add(1, Ordering::Relaxed);
                return Ok(Drained::Done);
            }
            Relocated::Busy => return Ok(Drained::Held),
            Relocated::NoCleanVictim => {}
        }
    }
    if crate::read::InvalidateVictimBuffer(desc)? {
        DROPPED.fetch_add(1, Ordering::Relaxed);
        return Ok(Drained::Done);
    }
    Ok(Drained::Held)
}

/// One buffer's write from the background drain's budget (`buf_giveback_write_bw` bytes a second, 100 ms of burst).
fn write_token() -> bool {
    let rate = mempolicy::consts::get(mempolicy::consts::K::BufGivebackWriteBw) as i64;
    if rate <= 0 {
        return true;
    }
    let now = pg_clock::mono_ns();
    let last = BG_TOKENS_NS.swap(now, Ordering::Relaxed);
    let refill = (rate as i128 * now.saturating_sub(last) as i128 / 1_000_000_000) as i64;
    let have = (BG_TOKENS.load(Ordering::Relaxed) + refill).min((rate / 10).max(BLCKSZ as i64));
    let ok = have >= BLCKSZ as i64;
    BG_TOKENS.store(if ok { have - BLCKSZ as i64 } else { have }, Ordering::Relaxed);
    ok
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Relocated {
    Moved,
    /// `src` was pinned or changed meanwhile.
    Busy,
    /// Paced: the sweep offered only dirty victims, and writing one is not the drain's to spend; drop `src` instead.
    NoCleanVictim,
}

/// Moves the clean page in `src` (pinned by us) into a victim buffer of an
/// active extent. Under the exclusive partition lock, with `src` holding
/// only our pin, nobody can reach either buffer: the 8 KiB copy, the tag and
/// state move and the in-place mapping update are one step to every reader.
/// Only clean pages move, so no BM_CHECKPOINT_NEEDED page leaves BufferSync's
/// sorted list. `clean_victim` (the background give-back): only into a victim
/// that needs no write, so the drain's writes stay within its paced budget.
fn relocate_buffer(src: &BufferDesc, clean_victim: bool) -> PgResult<Relocated> {
    let dst_buffer = if clean_victim {
        match crate::read::GetCleanVictimBuffer(CLEAN_VICTIM_TRIES)? {
            Some(b) => b,
            None => return Ok(Relocated::NoCleanVictim),
        }
    } else {
        crate::read::GetVictimBuffer(&None, IOContext::IOCONTEXT_NORMAL)?
    };
    let dst = GetBufferDescriptor(dst_buffer - 1);
    let tag = src.tag();
    let hash = BufTableHashCode(&tag);
    let partition_lock = BufMappingPartitionLock(hash);
    LWLockAcquire(partition_lock, LW_EXCLUSIVE, globals::MyProcNumber())?;
    let sst = LockBufHdr(src);
    let movable = buffer_refcount(sst) == 1
        && sst & BM_VALID != 0
        && sst & (BM_DIRTY | BM_CHECKPOINT_NEEDED | BM_IO_IN_PROGRESS | BM_PIN_COUNT_WAITER) == 0;
    if !movable {
        UnlockBufHdr(src, sst);
        LWLockRelease(partition_lock)?;
        give_back_victim(dst);
        return Ok(Relocated::Busy);
    }
    let dst_st = LockBufHdr(dst);
    debug_assert!(buffer_refcount(dst_st) == 1 && dst_st & BM_TAG_VALID == 0);
    if !BufTableSetId(&tag, hash, src.buf_id, dst.buf_id) {
        UnlockBufHdr(dst, dst_st);
        UnlockBufHdr(src, sst);
        LWLockRelease(partition_lock)?;
        give_back_victim(dst);
        return Err(Box::new(PgError::error("shared buffer hash table corrupted")));
    }
    // SAFETY: both blocks are pinned by us alone and unreachable through the
    // mapping (exclusive partition lock); distinct buffers never overlap.
    unsafe {
        core::ptr::copy_nonoverlapping(
            BufferGetBlockPtr(BufferDescriptorGetBuffer(src)),
            BufferGetBlockPtr(dst_buffer),
            BLCKSZ,
        );
        dst.set_tag(tag);
        src.set_tag(cleared_buftag());
    }
    let moved = sst & (BM_TAG_VALID | BM_VALID | BM_PERMANENT | BUF_USAGECOUNT_MASK);
    UnlockBufHdr(dst, (dst_st & !(BUF_FLAG_MASK | BUF_USAGECOUNT_MASK)) | moved);
    UnlockBufHdr(src, sst & !(BUF_FLAG_MASK | BUF_USAGECOUNT_MASK));
    LWLockRelease(partition_lock)?;
    UnpinBuffer(dst);
    Ok(Relocated::Moved)
}

fn give_back_victim(dst: &BufferDesc) {
    UnpinBuffer(dst);
    crate::freelist::StrategyFreeBuffer(dst.buf_id);
}

fn retire(e: i32) -> PgResult<()> {
    extent::set_state(e, RETIRED);
    let active = extent::active_buffers() - extent_len(e);
    extent::set_active_buffers(active);
    extent::set_span(extent::computed_span());
    extent::release_blocks(e)
}

/// DRAINING -> ACTIVE for an extent the call gave up on: its emptied
/// buffers go back on the freelist.
fn revert(e: i32) -> PgResult<()> {
    let (lo, hi) = extent::extent_range(e);
    extent::set_state(e, ACTIVE);
    crate::freelist::StrategyFreeBufferRange(lo, hi);
    Ok(())
}

pub(crate) fn giveback_on() -> bool {
    mempolicy::consts::get(mempolicy::consts::K::BufGiveback) != 0
}

/// What the background writer does after a give-back slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GivebackStep {
    Idle,
    /// Still pending: the next slice after this many ms.
    Again(u32),
}

/// Caller holds `RESIZING`.
fn begin_background(target: i32) {
    if BG_GOAL.swap(target, Ordering::AcqRel) == 0 {
        start_clock();
    }
    crate::giveback::wake_drainer();
}

pub(crate) fn start_clock() {
    BG_T0_NS.store(pg_clock::mono_ns(), Ordering::Relaxed);
    BG_TOKENS.store(0, Ordering::Relaxed);
    BG_TOKENS_NS.store(pg_clock::mono_ns(), Ordering::Relaxed);
    BG_WAL_SINCE_NS.store(0, Ordering::Relaxed);
    BG_END_NS.store(0, Ordering::Relaxed);
    BG_W0.store(WRITTEN.load(Ordering::Relaxed), Ordering::Relaxed);
    BG_BACKOFF_MS.store(1, Ordering::Relaxed);
}

/// Caller holds `RESIZING`: a pending background drain is dropped and its extents go back to ACTIVE.
fn abandon_background() {
    if BG_GOAL.swap(0, Ordering::AcqRel) != 0 {
        BG_END_NS.store(pg_clock::mono_ns(), Ordering::Relaxed);
    }
    clear_carry();
    for e in 0..extent::n_extents() {
        if extent::state(e) == DRAINING {
            let _ = revert(e);
        }
    }
    BG_DIRTY.store(0, Ordering::Relaxed);
}

fn clear_carry() {
    for c in CARRY.iter() {
        c.store(0, Ordering::Relaxed);
    }
}

/// The fruitless passes of the extents still in flight, for the next slice (at most CARRY_SLOTS; the rest start at 0).
fn save_carry(inflight: &[InFlight]) {
    clear_carry();
    for (slot, f) in CARRY.iter().zip(inflight.iter().filter(|f| f.fails > 0)) {
        slot.store(((f.extent as u32 as u64 + 1) << 32) | f.fails as u64, Ordering::Relaxed);
    }
}

fn carried_fails(e: i32) -> u32 {
    let key = e as u32 as u64 + 1;
    CARRY
        .iter()
        .map(|c| c.load(Ordering::Relaxed))
        .find(|v| v >> 32 == key)
        .map_or(0, |v| v as u32)
}

/// One bounded slice of the background drain (the background writer's cycle). Never sleeps; a running resize call
/// takes precedence and the slice is retried.
pub(crate) fn background_slice(budget: Duration) -> PgResult<GivebackStep> {
    if BG_GOAL.load(Ordering::Acquire) == 0 {
        return Ok(GivebackStep::Idle);
    }
    if RESIZING
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return Ok(GivebackStep::Again(BACKOFF_MAX_MS as u32));
    }
    let r = slice_locked(budget);
    RESIZING.store(false, Ordering::Release);
    r
}

fn slice_locked(budget: Duration) -> PgResult<GivebackStep> {
    let target = BG_GOAL.load(Ordering::Acquire);
    if target == 0 {
        return Ok(GivebackStep::Idle);
    }
    if !giveback_on() {
        abandon_background();
        return Ok(GivebackStep::Idle);
    }
    BG_SLICES.fetch_add(1, Ordering::Relaxed);
    let cx = MemoryContext::new("buffer pool give-back");
    let mcx = cx.mcx();
    let n = extent::n_extents().max(1) as usize;
    let started = pg_clock::MonoStamp::now();
    let wal_since = BG_WAL_SINCE_NS.load(Ordering::Relaxed);
    let mut d = Drain {
        writes: Writes::Paced { force_wal: wal_since != 0 && pg_clock::mono_ns().saturating_sub(wal_since) >= WAL_FORCE_NS },
        later: 0,
        wal_later: 0,
        target,
        deadline: started.deadline_after(budget),
        started,
        rate: 0,
        relocate: relocate_on(),
        visited: 0,
        cursor: extent::n_extents() - 1,
        deferred: mcx::vec_with_capacity_in(mcx, n)?,
        deferred_next: 0,
        pushback: None,
        inflight: mcx::vec_with_capacity_in(mcx, n)?,
        draining: 0,
    };
    for e in (0..extent::n_extents()).rev() {
        if extent::state(e) == DRAINING {
            d.inflight.push(InFlight { extent: e, fails: carried_fails(e) });
            d.draining += extent_len(e);
        }
    }
    TARGET_BUFFERS.store(shrink_goal(target), Ordering::Relaxed);
    let r = background_drain(&mut d);
    save_carry(&d.inflight);
    match r? {
        Some(ms) => {
            BG_DIRTY.store(dirty_draining(), Ordering::Relaxed);
            Ok(GivebackStep::Again(ms))
        }
        None => {
            BG_GOAL.store(0, Ordering::Release);
            BG_END_NS.store(pg_clock::mono_ns(), Ordering::Relaxed);
            BG_DIRTY.store(0, Ordering::Relaxed);
            TARGET_BUFFERS.store(extent::active_buffers(), Ordering::Relaxed);
            crate::buf_table::BufTableShrinkToFit(extent::active_buffers())?;
            Ok(GivebackStep::Idle)
        }
    }
}

/// `drain_loop` as one slice: None = nothing left that fits the goal, Some(ms) = pending, the next slice after `ms`
/// (0 while it progresses; a fruitless pass backs off to BACKOFF_MAX_MS instead of waiting on a pin).
fn background_drain(d: &mut Drain<'_>) -> PgResult<Option<u32>> {
    loop {
        plan(d);
        if d.inflight.is_empty() {
            return Ok(None);
        }
        d.later = 0;
        d.wal_later = 0;
        let progressed = match pass_inflight(d, true)? {
            Some(p) => p,
            None => true,
        };
        note_wal_wait(d.wal_later > 0);
        if progressed {
            BG_BACKOFF_MS.store(1, Ordering::Relaxed);
        }
        if d.inflight.is_empty() {
            continue;
        }
        if progressed {
            if d.deadline.expired() {
                return Ok(Some(0));
            }
            continue;
        }
        if d.later > 0 {
            return Ok(Some(if d.wal_later > 0 { 5 } else { 1 }));
        }
        demote_stuck(d)?;
        let b = BG_BACKOFF_MS.load(Ordering::Relaxed).max(1);
        BG_BACKOFF_MS.store((b * 2).min(BACKOFF_MAX_MS as i32), Ordering::Relaxed);
        return Ok(Some(b as u32));
    }
}

/// A pass that left writes for their WAL starts the clock after which the drain flushes WAL itself (WAL_FORCE_NS); a
/// pass with none stops it.
fn note_wal_wait(waited: bool) {
    if !waited {
        BG_WAL_SINCE_NS.store(0, Ordering::Relaxed);
    } else if BG_WAL_SINCE_NS.load(Ordering::Relaxed) == 0 {
        BG_WAL_SINCE_NS.store(pg_clock::mono_ns(), Ordering::Relaxed);
    }
}

fn dirty_draining() -> i32 {
    let mut n = 0;
    for e in 0..extent::n_extents() {
        if extent::state(e) == DRAINING {
            let (lo, hi) = extent::extent_range(e);
            n += (lo..hi)
                .filter(|&b| GetBufferDescriptor(b).state.load(Ordering::Relaxed) & BM_DIRTY != 0)
                .count() as i32;
        }
    }
    n
}

pub(crate) fn written() -> u64 {
    WRITTEN.load(Ordering::Relaxed)
}

pub fn BufferPoolStats() -> PoolStats {
    let mut draining = 0i64;
    let mut retired = 0i64;
    for e in 0..extent::n_extents() {
        match extent::state(e) {
            DRAINING => draining += 1,
            RETIRED => retired += 1,
            _ => {}
        }
    }
    let target = match TARGET_BUFFERS.load(Ordering::Relaxed) {
        0 => extent::boot_buffers(),
        t => t,
    };
    PoolStats {
        max_bytes: bytes(extent::max_buffers()),
        boot_bytes: bytes(extent::boot_buffers()),
        active_bytes: bytes(extent::active_buffers()),
        target_bytes: bytes(target),
        resident_bytes: extent::resident_block_bytes(crate::buf_hdr::NBuffersInited()) as i64,
        draining_extents: draining,
        retired_extents: retired,
        relocated: RELOCATED.load(Ordering::Relaxed) as i64,
        written: WRITTEN.load(Ordering::Relaxed) as i64,
        dropped: DROPPED.load(Ordering::Relaxed) as i64,
        resizing: RESIZING.load(Ordering::Relaxed) || BG_GOAL.load(Ordering::Relaxed) != 0,
    }
}

/// Crash cycle: the boot extent set comes back; extents a resize activated
/// past it are retired. Every child (a mid-shrink resizer included) is dead.
pub(crate) fn reset_after_crash() {
    let boot = extent::boot_buffers();
    let boot_extents = extent::extents_for(boot);
    for e in 0..extent::n_extents() {
        let st = extent::state(e);
        if e < boot_extents {
            extent::set_state(e, ACTIVE);
        } else if st != UNBORN {
            if st != RETIRED {
                if let Err(err) = extent::release_blocks(e) {
                    let _ = elog::elog(types_error::WARNING, err.message.clone());
                }
            }
            extent::set_state(e, RETIRED);
        }
    }
    extent::set_active_buffers(boot);
    extent::set_span(boot);
    TARGET_BUFFERS.store(0, Ordering::Relaxed);
    BG_GOAL.store(0, Ordering::Relaxed);
    BG_DIRTY.store(0, Ordering::Relaxed);
    clear_carry();
    RELOCATED.store(0, Ordering::Relaxed);
    WRITTEN.store(0, Ordering::Relaxed);
    DROPPED.store(0, Ordering::Relaxed);
    RESIZING.store(false, Ordering::Release);
    crate::wss::reset();
}

#[cfg(test)]
pub(crate) mod testing {
    pub(crate) fn set_resizing(on: bool) {
        super::RESIZING.store(on, core::sync::atomic::Ordering::Relaxed);
    }
}
