//! Invariants:
//! - All link/unlink/traversal happens under [`Pipe::lck`].
//! - The registrant NEVER returns while its node is linked: it exits only
//!   after (a) it unlinked the node itself under the lock, or (b) it
//!   observed `completed == true` — and a completer sets `completed` only
//!   AFTER unlinking the node and never touches the node again afterwards
//!   (`procno` is copied out first).
//! - Completers collect (ptr, procno) under the lock; between unlink and
//!   the `completed` store the registrant is parked or looping (it cannot
//!   free the node before observing `completed`).

use std::cell::UnsafeCell;
use std::ptr;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release, SeqCst};
use std::sync::atomic::{fence, AtomicBool, AtomicU64, AtomicU8};
use std::sync::Arc;
use std::sync::OnceLock;

use types_core::{ProcNumber, XLogRecPtr, INVALID_PROC_NUMBER};
use types_storage::latch::LatchHandle;
use types_storage::waiteventset::{WL_EXIT_ON_PM_DEATH, WL_LATCH_SET, WL_TIMEOUT};

use crate::ctl::{SpinLock, XLogCtl};

const PG_WAIT_IPC: u32 = 0x0800_0000;
pub const WAIT_EVENT_FLUSH_PIPELINE: u32 = PG_WAIT_IPC + 58;

const BACKSTOP_MS: i64 = 100;

/// Group-commit batching refinements of the pipeline (D4574). Internal, read
/// from the same knob as the pipeline itself (no GUC):
///
/// | `PGRUST_FLUSH_PIPELINE` | meaning |
/// |---|---|
/// | `0`, `off` | pipeline off: every committer runs [`crate::write::XLogFlush`] (PG's convoy) |
/// | unset, `1`, `on` | pipeline on with [`DEFAULT_REFINE`] (`absorb+handoff`) |
/// | `v1` | pipeline on, no refinement (the pipeline as first shipped) |
/// | `tok[+tok...]` | pipeline on with exactly those refinements |
///
/// Tokens:
/// - `absorb`: the walwriter flushes every FINISHED insert, not only the max
///   registered LSN it read before waiting for WALWriteLock (that wait can be
///   a whole backend-led fsync); it runs [`crate::write::XLogFlush`]'s leader
///   loop, so the batch is recomputed after any wait. PG's leader batch is the
///   finished-insert position too.
/// - `handoff`: a completer that leaves uncovered waiters wakes the lowest one
///   FIRST with a lead verdict (it retakes the leader path at once, the way a
///   woken LWLockAcquireOrWait waiter does in PG), then the covered ones; so the
///   next fsync never waits for the previous flusher's completion wakes.
/// - `chain`: the walwriter repeats its flush while waiters remain, without
///   going back through its cycle (latch, interrupts, buffer advance, stats).
/// - `quiet` (implies `handoff`): a registrant does not wake the walwriter
///   while a commit flush is in flight; that flusher's completion covers it
///   or hands the lead on (PG's shape: no walwriter on the commit path).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refine {
    pub absorb: bool,
    pub handoff: bool,
    pub chain: bool,
    pub quiet: bool,
}

impl Refine {
    pub const V1: Refine = Refine { absorb: false, handoff: false, chain: false, quiet: false };
}

/// What an unset / `on` knob means. `absorb+handoff`: with it the pipeline
/// matches or beats backend-led flush (`off`) at 1 to 64 clients, where `v1`
/// lost up to a fifth of the commit rate at 64 (D4574).
pub const DEFAULT_REFINE: Refine =
    Refine { absorb: true, handoff: true, chain: false, quiet: false };

/// `None` = pipeline off. Unknown tokens are ignored (the pipeline stays on).
pub fn parse_knob(raw: Option<&str>) -> Option<Refine> {
    match raw.map(str::trim) {
        Some("0") | Some("off") => None,
        None | Some("") | Some("1") | Some("on") => Some(DEFAULT_REFINE),
        Some("v1") => Some(Refine::V1),
        Some(list) => {
            let mut r = Refine::V1;
            for tok in list.split('+').map(str::trim) {
                match tok {
                    "absorb" => r.absorb = true,
                    "handoff" => r.handoff = true,
                    "chain" => r.chain = true,
                    "quiet" => {
                        r.quiet = true;
                        r.handoff = true;
                    }
                    _ => {}
                }
            }
            Some(r)
        }
    }
}

fn knob() -> Option<Refine> {
    static KNOB: OnceLock<Option<Refine>> = OnceLock::new();
    *KNOB.get_or_init(|| parse_knob(std::env::var("PGRUST_FLUSH_PIPELINE").ok().as_deref()))
}

pub fn flush_pipeline_enabled() -> bool {
    knob().is_some()
}

/// The armed refinements (all off when the pipeline is off).
pub fn refine() -> Refine {
    knob().unwrap_or(Refine::V1)
}

pub(crate) fn trace_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        matches!(
            std::env::var("PGRUST_FLUSH_PIPELINE_TRACE").ok().as_deref().map(str::trim),
            Some("1") | Some("on")
        )
    })
}

fn trace(msg: impl FnOnce() -> String) {
    if trace_enabled() {
        let _ = elog::elog(types_error::LOG, format!("flushpipe: {}", msg()));
    }
}

pub struct FlushPipeStats {
    pub registered: AtomicU64,
    pub self_served: AtomicU64,
    pub completed: AtomicU64,
    pub leader_flushes: AtomicU64,
    pub backstop_self_served: AtomicU64,
    pub retry_rearm: AtomicU64,
    pub completed_after_timeout: AtomicU64,
    pub ticket_registered: AtomicU64,
    pub ticket_completed: AtomicU64,
    pub ticket_self_served: AtomicU64,
    pub ticket_retry_rearm: AtomicU64,
    /// `handoff`: lead verdicts given (a waiter woken to retake the leader path).
    pub handoff_leads: AtomicU64,
    /// `chain`: extra walwriter flush laps run without a cycle in between.
    pub chain_laps: AtomicU64,
}

pub static STATS: FlushPipeStats = FlushPipeStats {
    registered: AtomicU64::new(0),
    self_served: AtomicU64::new(0),
    completed: AtomicU64::new(0),
    leader_flushes: AtomicU64::new(0),
    backstop_self_served: AtomicU64::new(0),
    retry_rearm: AtomicU64::new(0),
    completed_after_timeout: AtomicU64::new(0),
    ticket_registered: AtomicU64::new(0),
    ticket_completed: AtomicU64::new(0),
    ticket_self_served: AtomicU64::new(0),
    ticket_retry_rearm: AtomicU64::new(0),
    handoff_leads: AtomicU64::new(0),
    chain_laps: AtomicU64::new(0),
};

/// Commit flushes in flight (`quiet`): taken right before a flusher's XLogWrite
/// under WALWriteLock, dropped right before its [`complete_up_to`] (whose
/// leading fence pairs with the registrant's post-link fence: a registrant
/// that sees a flush in flight is seen by that flush's completion walk).
static FLUSHERS: AtomicU64 = AtomicU64::new(0);

pub(crate) struct FlushMark(bool);

impl FlushMark {
    pub(crate) fn begin() -> FlushMark {
        let on = flush_pipeline_enabled() && refine().quiet;
        if on {
            FLUSHERS.fetch_add(1, SeqCst);
        }
        FlushMark(on)
    }
}

impl Drop for FlushMark {
    fn drop(&mut self) {
        if self.0 {
            FLUSHERS.fetch_sub(1, SeqCst);
        }
    }
}

pub(crate) fn count_chain_lap() {
    STATS.chain_laps.fetch_add(1, Relaxed);
}

pub(crate) fn count_leader_flush() {
    STATS.leader_flushes.fetch_add(1, Relaxed);
    trace(|| "leader flush".into());
}

#[cfg(pgrust_sim)]
pub mod sim {
    use std::sync::atomic::{AtomicBool, Ordering};

    pub static SKIP_NEXT_LINK: AtomicBool = AtomicBool::new(false);

    pub(crate) fn take_skip_link() -> bool {
        SKIP_NEXT_LINK.swap(false, Ordering::Relaxed)
    }
}

struct FlushWaitNode {
    lsn: XLogRecPtr,
    procno: ProcNumber,
    /// Set by a completer strictly AFTER unlinking (Release); the
    /// registrant's exit license on the completion path (Acquire).
    completed: AtomicBool,
    /// `handoff` lead verdict: written by the completer after unlinking and
    /// BEFORE its `completed` store (so the registrant's Acquire load of
    /// `completed` makes it visible). True = not covered: retake the leader
    /// path ([`FlushWaitOutcome::Retry`]) instead of returning.
    lead: AtomicBool,
    prev: UnsafeCell<*mut FlushWaitNode>,
    next: UnsafeCell<*mut FlushWaitNode>,
    linked: UnsafeCell<bool>,
}

struct PipeList {
    head: *mut FlushWaitNode,
    tail: *mut FlushWaitNode,
}

struct Pipe {
    lck: SpinLock,
    list: UnsafeCell<PipeList>,
    /// Conservatively stale reads are safe: a miss on a brand-new
    /// entry is exactly the race the fence pair closes.
    min_pending: AtomicU64,
    max_pending: AtomicU64,
}

// SAFETY: `list` (and node link fields) are only touched under `lck`; the
// node-lifetime invariants are in the module doc.
unsafe impl Sync for Pipe {}

static PIPE: Pipe = Pipe {
    lck: SpinLock::new(),
    list: UnsafeCell::new(PipeList { head: ptr::null_mut(), tail: ptr::null_mut() }),
    min_pending: AtomicU64::new(u64::MAX),
    max_pending: AtomicU64::new(0),
};

impl Pipe {
    /// SAFETY: caller holds `lck`.
    unsafe fn refresh_hints(&self) {
        let l = &*self.list.get();
        if l.head.is_null() {
            self.min_pending.store(u64::MAX, Relaxed);
            self.max_pending.store(0, Relaxed);
        } else {
            self.min_pending.store((*l.head).lsn, Relaxed);
            self.max_pending.store((*l.tail).lsn, Relaxed);
        }
    }

    /// SAFETY: caller holds `lck`; `node` is unlinked and outlives its
    /// list membership (module-doc invariants).
    unsafe fn link(&self, node: *mut FlushWaitNode) {
        let l = &mut *self.list.get();
        debug_assert!(!*(*node).linked.get());
        let mut at = l.tail;
        while !at.is_null() && (*at).lsn > (*node).lsn {
            at = *(*at).prev.get();
        }
        if at.is_null() {
            *(*node).prev.get() = ptr::null_mut();
            *(*node).next.get() = l.head;
            if l.head.is_null() {
                l.tail = node;
            } else {
                *(*l.head).prev.get() = node;
            }
            l.head = node;
        } else {
            let after = *(*at).next.get();
            *(*node).prev.get() = at;
            *(*node).next.get() = after;
            *(*at).next.get() = node;
            if after.is_null() {
                l.tail = node;
            } else {
                *(*after).prev.get() = node;
            }
        }
        *(*node).linked.get() = true;
        self.refresh_hints();
    }

    /// SAFETY: caller holds `lck`; `node` is linked.
    unsafe fn unlink(&self, node: *mut FlushWaitNode) {
        let l = &mut *self.list.get();
        debug_assert!(*(*node).linked.get());
        let prev = *(*node).prev.get();
        let next = *(*node).next.get();
        if prev.is_null() {
            l.head = next;
        } else {
            *(*prev).next.get() = next;
        }
        if next.is_null() {
            l.tail = prev;
        } else {
            *(*next).prev.get() = prev;
        }
        *(*node).linked.get() = false;
        self.refresh_hints();
    }
}

/// `node` is dereferenced exactly once (the `completed` store) and never after it.
struct Covered {
    node: *mut FlushWaitNode,
    procno: ProcNumber,
    lsn: XLogRecPtr,
}

/// Call AFTER publishing a new `logFlushResult` (and never under
/// WALWriteLock — wakes must not lengthen the flush critical path).
pub(crate) fn complete_up_to(flushed: XLogRecPtr) {
    if !flush_pipeline_enabled() {
        return;
    }
    // Dekker fence: pairs with the registrant's post-link fence.
    fence(SeqCst);
    complete_deferred_up_to(flushed);
    let handoff = refine().handoff;
    let min_pending = PIPE.min_pending.load(Relaxed);
    if min_pending > flushed && !(handoff && min_pending != u64::MAX) {
        return;
    }
    let mut covered: Vec<Covered> = Vec::new();
    let mut lead: Option<Covered> = None;
    PIPE.lck.with(|| {
        // SAFETY: under lck.
        unsafe {
            let l = &*PIPE.list.get();
            let mut at = l.head;
            while !at.is_null() && (*at).lsn <= flushed {
                let next = *(*at).next.get();
                PIPE.unlink(at);
                covered.push(Covered { node: at, procno: (*at).procno, lsn: (*at).lsn });
                at = next;
            }
            // `handoff`: the lowest waiter this flush did not cover leads the
            // next one. Its record is inserted (it registered after inserting),
            // so its flush of every finished insert covers the rest.
            if handoff && !at.is_null() {
                PIPE.unlink(at);
                lead = Some(Covered { node: at, procno: (*at).procno, lsn: (*at).lsn });
            }
        }
    });
    if let Some(c) = lead {
        STATS.handoff_leads.fetch_add(1, Relaxed);
        trace(|| format!("lead verdict at {:X}/{:X}", c.lsn >> 32, c.lsn & 0xFFFF_FFFF));
        // SAFETY: as for the covered nodes below; `lead` is written before
        // the Release store of `completed`, which is the LAST touch.
        unsafe {
            (*c.node).lead.store(true, Relaxed);
            (*c.node).completed.store(true, Release);
        }
        latch::SetLatch(LatchHandle::proc(c.procno));
    }
    if covered.is_empty() {
        return;
    }
    STATS.completed.fetch_add(covered.len() as u64, Relaxed);
    trace(|| {
        format!(
            "completed {} waiter(s) up to {:X}/{:X}",
            covered.len(),
            flushed >> 32,
            flushed & 0xFFFF_FFFF
        )
    });
    for c in covered {
        let _ = c.lsn;
        // SAFETY: the node is unlinked and its registrant is still waiting
        // on `completed` (module-doc lifetime invariants). This store is
        // the LAST touch of the node; `procno` was copied under the lock.
        unsafe {
            (*c.node).completed.store(true, Release);
        }
        latch::SetLatch(LatchHandle::proc(c.procno));
    }
}

pub(crate) fn pending_max() -> Option<XLogRecPtr> {
    if !flush_pipeline_enabled() {
        return None;
    }
    let stack = PIPE.max_pending.load(Acquire);
    let deferred = DEFERRED.max_pending.load(Acquire);
    match stack.max(deferred) {
        0 => None,
        lsn => Some(lsn),
    }
}

pub fn pipeline_available() -> bool {
    if !flush_pipeline_enabled() {
        return false;
    }
    if lmgr_proc::ProcGlobal().walwriterProc.load(Relaxed) == INVALID_PROC_NUMBER {
        return false;
    }
    init_small::globals::MyLatch().is_some()
}

fn wake_walwriter() {
    let walwriter = lmgr_proc::ProcGlobal().walwriterProc.load(Relaxed);
    if walwriter != INVALID_PROC_NUMBER {
        latch::SetLatch(LatchHandle::proc(walwriter));
    }
}

pub enum FlushWaitOutcome {
    Completed,
    /// We have self-unlinked; the caller must retake its loop and contend for WALWriteLock ITSELF.
    Retry,
}

/// Caller contract: inside the commit critical section, called ONLY from
/// [`crate::write::XLogFlushPipelined`]'s contended arm after
/// [`pipeline_available`] returned true.
pub(crate) fn wait_for_flush(lsn: XLogRecPtr) -> FlushWaitOutcome {
    let ctl = XLogCtl();
    let node = FlushWaitNode {
        lsn,
        procno: init_small::globals::MyProcNumber(),
        completed: AtomicBool::new(false),
        lead: AtomicBool::new(false),
        prev: UnsafeCell::new(ptr::null_mut()),
        next: UnsafeCell::new(ptr::null_mut()),
        linked: UnsafeCell::new(false),
    };
    let node_ptr = &node as *const FlushWaitNode as *mut FlushWaitNode;

    #[cfg(pgrust_sim)]
    let link_dropped = sim::take_skip_link();
    #[cfg(not(pgrust_sim))]
    let link_dropped = false;

    if !link_dropped {
        // SAFETY: node is unlinked; lifetime invariants per module doc.
        PIPE.lck.with(|| unsafe { PIPE.link(node_ptr) });
    }
    STATS.registered.fetch_add(1, Relaxed);

    let try_self_unlink = || {
        PIPE.lck.with(|| {
            // SAFETY: under lck; unlink only if still linked. If a
            // completer already collected us this returns false and
            // `completed` is imminent.
            unsafe {
                if *node.linked.get() {
                    PIPE.unlink(node_ptr);
                    true
                } else {
                    false
                }
            }
        })
    };

    // Dekker fence: pairs with complete_up_to's post-publish fence.
    fence(SeqCst);
    if ctl.logFlushResult.load(Acquire) >= lsn {
        if try_self_unlink() {
            STATS.self_served.fetch_add(1, Relaxed);
            trace(|| format!("self-served at {:X}/{:X}", lsn >> 32, lsn & 0xFFFF_FFFF));
            return FlushWaitOutcome::Completed;
        }
    } else if !(refine().quiet && FLUSHERS.load(SeqCst) > 0) {
        wake_walwriter();
    }
    trace(|| format!("registered at {:X}/{:X}", lsn >> 32, lsn & 0xFFFF_FFFF));

    // A completed node is either covered or holds a `handoff` lead verdict
    // (not covered: the caller's loop retakes the leader path).
    let outcome = |node: &FlushWaitNode| {
        if node.lead.load(Relaxed) {
            FlushWaitOutcome::Retry
        } else {
            FlushWaitOutcome::Completed
        }
    };
    let my_latch = init_small::globals::MyLatch();
    loop {
        if node.completed.load(Acquire) {
            return outcome(&node);
        }
        // The completer stores `completed` BEFORE its SetLatch, so a set seen
        // here implies the recheck observes it.
        let woke = latch::WaitLatch(
            my_latch,
            WL_LATCH_SET | WL_TIMEOUT | WL_EXIT_ON_PM_DEATH,
            BACKSTOP_MS,
            WAIT_EVENT_FLUSH_PIPELINE,
        );
        if let Some(l) = my_latch {
            latch::ResetLatch(l);
        }
        let timed_out = match woke {
            Ok(ev) => ev & WL_TIMEOUT != 0,
            Err(_) => true,
        };
        if node.completed.load(Acquire) {
            if timed_out {
                STATS.completed_after_timeout.fetch_add(1, Relaxed);
                trace(|| "completed observed on a TIMEOUT lap (lost wake?)".into());
            }
            return outcome(&node);
        }
        if !timed_out {
            continue;
        }
        if ctl.logFlushResult.load(SeqCst) >= lsn {
            if try_self_unlink() {
                STATS.backstop_self_served.fetch_add(1, Relaxed);
                trace(|| "BACKSTOP self-serve (missed-wake tripwire)".into());
                return FlushWaitOutcome::Completed;
            }
        } else if try_self_unlink() {
            STATS.retry_rearm.fetch_add(1, Relaxed);
            trace(|| "backstop re-arm: retaking the leader path".into());
            return FlushWaitOutcome::Retry;
        }
    }
}

// OWNERSHIP INVARIANTS:
//   T1. A ticket is Arc-shared three ways at most: the registry entry, the
//       registrant's handle (becomes the tail host's), and a claimant's
//       transient walk copy. Memory frees when the last Arc drops.
//   T2. REGISTRY REMOVAL IS THE CLAIM: exactly one party removes a given
//       ticket from the registry (the SpinLock arbitrates), and ONLY the
//       remover performs the PENDING->COMPLETED transition. The waiter's
//       COMPLETED->CONSUMED transition has exactly one candidate thread
//       (the single tail host) — both transitions are RMW swaps with
//       loud teeth on the swapped-out value.
//   T3. The completer's last touch is the COMPLETED swap + SetLatch —
//       through its own Arc, so ticket lifetime never depends on the
//       waiter's timing.
//   T4. The Dekker pair is the SAME as the stack path's: registration
//       links under the registry lock, fences, then rechecks
//       logFlushResult (self-claim if covered); complete_up_to emits one
//       post-publish fence for both halves.

const TICKET_PENDING: u8 = 0;
const TICKET_COMPLETED: u8 = 1;
const TICKET_CONSUMED: u8 = 2;

struct TicketInner {
    lsn: XLogRecPtr,
    wake_procno: ProcNumber,
    state: AtomicU8,
}

pub struct DeferredFlushTicket {
    inner: Arc<TicketInner>,
}

impl DeferredFlushTicket {
    pub fn lsn(&self) -> XLogRecPtr {
        self.inner.lsn
    }
}

struct DeferredRegistry {
    lck: SpinLock,
    tickets: UnsafeCell<Vec<Arc<TicketInner>>>,
    min_pending: AtomicU64,
    max_pending: AtomicU64,
}

// SAFETY: `tickets` is only touched under `lck` (T2).
unsafe impl Sync for DeferredRegistry {}

static DEFERRED: DeferredRegistry = DeferredRegistry {
    lck: SpinLock::new(),
    tickets: UnsafeCell::new(Vec::new()),
    min_pending: AtomicU64::new(u64::MAX),
    max_pending: AtomicU64::new(0),
};

impl DeferredRegistry {
    /// SAFETY: caller holds `lck`.
    unsafe fn refresh_hints(&self) {
        let t = &*self.tickets.get();
        let min = t.iter().map(|x| x.lsn).min().unwrap_or(u64::MAX);
        let max = t.iter().map(|x| x.lsn).max().unwrap_or(0);
        self.min_pending.store(min, Relaxed);
        self.max_pending.store(max, Relaxed);
    }

    /// True = the caller now owns the PENDING->COMPLETED transition.
    fn claim(&self, ticket: &Arc<TicketInner>) -> bool {
        self.lck.with(|| unsafe {
            let t = &mut *self.tickets.get();
            match t.iter().position(|x| Arc::ptr_eq(x, ticket)) {
                Some(i) => {
                    t.swap_remove(i);
                    self.refresh_hints();
                    true
                }
                None => false,
            }
        })
    }
}

pub fn register_deferred(
    lsn: XLogRecPtr,
    wake_procno: ProcNumber,
) -> Option<DeferredFlushTicket> {
    if !pipeline_available() {
        return None;
    }
    let ctl = XLogCtl();
    let ticket =
        Arc::new(TicketInner { lsn, wake_procno, state: AtomicU8::new(TICKET_PENDING) });
    DEFERRED.lck.with(|| unsafe {
        let t = &mut *DEFERRED.tickets.get();
        t.push(Arc::clone(&ticket));
        DEFERRED.refresh_hints();
    });
    STATS.ticket_registered.fetch_add(1, Relaxed);
    // Dekker fence (T4): pairs with complete_up_to's post-publish fence.
    fence(SeqCst);
    if ctl.logFlushResult.load(Acquire) >= lsn {
        if DEFERRED.claim(&ticket) {
            let prev = ticket.state.swap(TICKET_COMPLETED, AcqRel);
            debug_assert_eq!(prev, TICKET_PENDING, "claimed ticket must be PENDING (T2)");
            STATS.ticket_self_served.fetch_add(1, Relaxed);
            trace(|| format!("ticket self-served at {:X}/{:X}", lsn >> 32, lsn & 0xFFFF_FFFF));
        }
    } else {
        wake_walwriter();
    }
    trace(|| format!("ticket registered at {:X}/{:X}", lsn >> 32, lsn & 0xFFFF_FFFF));
    Some(DeferredFlushTicket { inner: ticket })
}

fn complete_deferred_up_to(flushed: XLogRecPtr) {
    if DEFERRED.min_pending.load(Relaxed) > flushed {
        return;
    }
    let claimed: Vec<Arc<TicketInner>> = DEFERRED.lck.with(|| unsafe {
        let t = &mut *DEFERRED.tickets.get();
        let mut out = Vec::new();
        let mut i = 0;
        while i < t.len() {
            if t[i].lsn <= flushed {
                out.push(t.swap_remove(i));
            } else {
                i += 1;
            }
        }
        DEFERRED.refresh_hints();
        out
    });
    if claimed.is_empty() {
        return;
    }
    STATS.ticket_completed.fetch_add(claimed.len() as u64, Relaxed);
    trace(|| format!("completed {} ticket(s)", claimed.len()));
    for t in claimed {
        let prev = t.state.swap(TICKET_COMPLETED, AcqRel);
        debug_assert_eq!(prev, TICKET_PENDING, "claimed ticket must be PENDING (T2)");
        latch::SetLatch(LatchHandle::proc(t.wake_procno));
    }
}

/// Non-cancellable by design: a commit whose record is inserted cannot be abandoned.
pub fn ticket_wait(ticket: &DeferredFlushTicket) -> FlushWaitOutcome {
    let ctl = XLogCtl();
    let inner = &ticket.inner;
    let my_latch = init_small::globals::MyLatch();
    loop {
        match inner.state.load(Acquire) {
            TICKET_COMPLETED => {
                let prev = inner.state.swap(TICKET_CONSUMED, AcqRel);
                debug_assert_eq!(prev, TICKET_COMPLETED, "single consumer (T2)");
                return FlushWaitOutcome::Completed;
            }
            TICKET_CONSUMED => unreachable!("deferred ticket consumed twice"),
            _ => {}
        }
        let woke = latch::WaitLatch(
            my_latch,
            WL_LATCH_SET | WL_TIMEOUT | WL_EXIT_ON_PM_DEATH,
            BACKSTOP_MS,
            WAIT_EVENT_FLUSH_PIPELINE,
        );
        if let Some(l) = my_latch {
            latch::ResetLatch(l);
        }
        let timed_out = match woke {
            Ok(ev) => ev & WL_TIMEOUT != 0,
            Err(_) => true,
        };
        if !timed_out {
            continue;
        }
        if inner.state.load(Acquire) == TICKET_COMPLETED {
            continue;
        }
        if ctl.logFlushResult.load(SeqCst) >= inner.lsn {
            if DEFERRED.claim(inner) {
                let prev = inner.state.swap(TICKET_COMPLETED, AcqRel);
                debug_assert_eq!(prev, TICKET_PENDING, "claimed ticket must be PENDING (T2)");
                STATS.ticket_self_served.fetch_add(1, Relaxed);
                trace(|| "ticket BACKSTOP self-serve (missed-wake tripwire)".into());
            }
            continue;
        }
        if DEFERRED.claim(inner) {
            STATS.ticket_retry_rearm.fetch_add(1, Relaxed);
            trace(|| "ticket backstop re-arm: retaking the leader path".into());
            return FlushWaitOutcome::Retry;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mknode(lsn: u64, procno: ProcNumber) -> Box<FlushWaitNode> {
        Box::new(FlushWaitNode {
            lsn,
            procno,
            completed: AtomicBool::new(false),
            lead: AtomicBool::new(false),
            prev: UnsafeCell::new(ptr::null_mut()),
            next: UnsafeCell::new(ptr::null_mut()),
            linked: UnsafeCell::new(false),
        })
    }

    fn collect_covered(flushed: u64) -> Vec<u64> {
        let mut out = Vec::new();
        PIPE.lck.with(|| unsafe {
            let l = &*PIPE.list.get();
            let mut at = l.head;
            while !at.is_null() && (*at).lsn <= flushed {
                let next = *(*at).next.get();
                PIPE.unlink(at);
                out.push((*at).lsn);
                at = next;
            }
        });
        out
    }

    fn drain_all() {
        collect_covered(u64::MAX);
        assert_eq!(PIPE.min_pending.load(Relaxed), u64::MAX);
        assert_eq!(PIPE.max_pending.load(Relaxed), 0);
    }

    #[test]
    fn ordered_link_and_covered_walk() {
        static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = GATE.lock().unwrap();

        drain_all();
        let (a, b, c, d) = (mknode(300, 3), mknode(100, 1), mknode(200, 2), mknode(150, 4));
        let (pa, pb, pc, pd) = (
            &*a as *const _ as *mut FlushWaitNode,
            &*b as *const _ as *mut FlushWaitNode,
            &*c as *const _ as *mut FlushWaitNode,
            &*d as *const _ as *mut FlushWaitNode,
        );
        PIPE.lck.with(|| unsafe {
            PIPE.link(pa);
            PIPE.link(pb);
            PIPE.link(pc);
            PIPE.link(pd);
        });
        assert_eq!(PIPE.min_pending.load(Relaxed), 100);
        assert_eq!(PIPE.max_pending.load(Relaxed), 300);

        assert!(collect_covered(50).is_empty());
        assert_eq!(collect_covered(200), vec![100, 150, 200]);
        assert_eq!(PIPE.min_pending.load(Relaxed), 300);
        assert_eq!(PIPE.max_pending.load(Relaxed), 300);
        assert_eq!(collect_covered(1000), vec![300]);
        assert_eq!(PIPE.min_pending.load(Relaxed), u64::MAX);
        assert_eq!(PIPE.max_pending.load(Relaxed), 0);

        PIPE.lck.with(|| unsafe {
            PIPE.link(pb);
            PIPE.link(pd);
            PIPE.link(pc);
        });
        PIPE.lck.with(|| unsafe {
            assert!(*(*pd).linked.get());
            PIPE.unlink(pd);
        });
        assert_eq!(PIPE.min_pending.load(Relaxed), 100);
        assert_eq!(PIPE.max_pending.load(Relaxed), 200);
        assert_eq!(collect_covered(u64::MAX), vec![100, 200]);
    }

    fn mkticket(lsn: u64) -> Arc<TicketInner> {
        Arc::new(TicketInner {
            lsn,
            wake_procno: INVALID_PROC_NUMBER,
            state: AtomicU8::new(TICKET_PENDING),
        })
    }

    fn deferred_len() -> usize {
        DEFERRED.lck.with(|| unsafe { (*DEFERRED.tickets.get()).len() })
    }

    #[test]
    fn ticket_claim_is_exactly_once_and_hints_track() {
        static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = GATE.lock().unwrap();
        assert_eq!(deferred_len(), 0);

        let (a, b) = (mkticket(100), mkticket(300));
        DEFERRED.lck.with(|| unsafe {
            let t = &mut *DEFERRED.tickets.get();
            t.push(Arc::clone(&a));
            t.push(Arc::clone(&b));
            DEFERRED.refresh_hints();
        });
        assert_eq!(DEFERRED.min_pending.load(Relaxed), 100);
        assert_eq!(DEFERRED.max_pending.load(Relaxed), 300);

        assert!(DEFERRED.claim(&a));
        assert!(!DEFERRED.claim(&a));
        assert_eq!(DEFERRED.min_pending.load(Relaxed), 300);

        assert_eq!(a.state.swap(TICKET_COMPLETED, AcqRel), TICKET_PENDING);
        assert_eq!(a.state.swap(TICKET_CONSUMED, AcqRel), TICKET_COMPLETED);

        assert!(DEFERRED.claim(&b));
        assert_eq!(DEFERRED.min_pending.load(Relaxed), u64::MAX);
        assert_eq!(DEFERRED.max_pending.load(Relaxed), 0);
        assert_eq!(b.state.load(Acquire), TICKET_PENDING);
    }

    #[test]
    fn ticket_register_refuses_without_pipeline() {
        if matches!(
            std::env::var("PGRUST_FLUSH_PIPELINE").ok().as_deref().map(str::trim),
            Some("0") | Some("off")
        ) {
            assert!(register_deferred(42, INVALID_PROC_NUMBER).is_none());
        }
    }

    #[test]
    fn knob_parses_refinements() {
        assert_eq!(parse_knob(Some("off")), None);
        assert_eq!(parse_knob(Some(" 0 ")), None);
        assert_eq!(parse_knob(None), Some(DEFAULT_REFINE));
        assert_eq!(parse_knob(Some("on")), Some(DEFAULT_REFINE));
        assert_eq!(parse_knob(Some("1")), Some(DEFAULT_REFINE));
        assert_eq!(parse_knob(Some("v1")), Some(Refine::V1));
        assert_eq!(
            parse_knob(Some("absorb+handoff")),
            Some(Refine { absorb: true, handoff: true, chain: false, quiet: false })
        );
        assert_eq!(
            parse_knob(Some("chain + absorb + bogus")),
            Some(Refine { absorb: true, handoff: false, chain: true, quiet: false })
        );
        assert_eq!(
            parse_knob(Some("absorb+quiet")),
            Some(Refine { absorb: true, handoff: true, chain: false, quiet: true })
        );
    }

    #[test]
    fn knob_default_on() {
        if std::env::var("PGRUST_FLUSH_PIPELINE").is_err() {
            assert!(flush_pipeline_enabled());
        }
    }
}
