//! Gravity pool: the `gravity_parallel::Pool` over the runtime crate, installed by the postmaster (docs/gravity/parallel.md).
#![deny(unsafe_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use pgsync::Mutex;

use gravity_abi::status::unwind;
use gravity_abi::MorselRange;
use gravity_driver::mem::budget;
use gravity_driver::mem::root::Root;
use gravity_driver::mem::spill::{READ_BYTES, READ_PART, READ_RUN};
use gravity_driver::Error;
use gravity_parallel::{ErrorFunnel, FirstError, Job, MorselSource, Pool};
use runtime::{CompletionWaiter, QuerySpec, RgHandle, RgOutcome, Runtime, TaskSetSpec, TaskSetWork, WidthRequest};
use types_core::TransactionId;
use types_error::PgResult;

use gravity_hosts::{arm_once, arm_worker, no_arm, stash, take_any_error, ArmFn, SessionCarry};
use types_error::PgError;

mod pgstat_merge;
mod raw;
pub use pgstat_merge::SectionStats;

pub type InterruptsFn = fn() -> PgResult<()>;
/// Sets (true) or clears (false) the leader's abort byte, the one a section's claims poll once per window.
pub type AbortFn = fn(bool);
/// Fills the section's xids, a vec charged to the leader's statement (`RuntimePool::run_led`).
pub type XidsFn = fn(&mut budget::ChargedVec<TransactionId>) -> PgResult<()>;

/// What the pool borrows from the process it runs in: the interrupt check
/// the leader runs between wakes, how a pool thread arms itself, and where
/// the leader's transaction xids come from. `SERVER` under a postmaster,
/// `HARNESS` for the unit tests over a bare `Runtime`.
#[derive(Clone, Copy)]
pub struct Ambient {
    pub interrupts: InterruptsFn,
    pub abort: AbortFn,
    pub arm: ArmFn,
    pub xids: XidsFn,
}

/// The leader's park backstop: it parks on its waiter handle, registered with
/// the section's `CompletionWaiter`, and the runtime's `complete` unparks it
/// the moment the last claim finishes; the quantum only bounds how late a
/// cancel or a lost wake is noticed (§1.10 increment 3 item 2).
pub const PARK_QUANTUM_MS: i64 = 1;
const SERIALIZED_XACT_HEADER: usize = 32;

fn no_interrupts() -> PgResult<()> {
    Ok(())
}

fn server_abort(on: bool) {
    init_small::globals::SetAbortPending(on)
}

fn no_abort(_: bool) {}

/// The leader's transaction XIDs, which its workers see as their own (PG's `SerializeTransactionState`). D2354: the
/// buffer is sized from the transaction state (`EstimateTransactionStateSpace`); a fixed 256-XID buffer raised
/// "transaction state buffer is too small" past 255 subtransaction XIDs, where PG answers.
fn leader_xids(out: &mut budget::ChargedVec<TransactionId>) -> PgResult<()> {
    let mut buf = vec![0u8; xact::EstimateTransactionStateSpace()]; // alloc: Exec
    let n = xact::SerializeTransactionState(&mut buf)?;
    let nxids = (n - SERIALIZED_XACT_HEADER) / 4;
    out.clear();
    out.try_reserve(nxids).map_err(|_| PgError::error("out of memory copying the leader's transaction xids"))?; // alloc: Cold
    // bounded("other: one 4-byte copy per XID of the leader's serialized transaction state, its own and its subtransactions'; PG's SerializeTransactionState walks the same XIDs with no poll")
    for i in 0..nxids {
        let off = SERIALIZED_XACT_HEADER + 4 * i;
        out.push(TransactionId::from_ne_bytes(buf[off..off + 4].try_into().expect("4 bytes"))).map_err(|_| PgError::error("out of memory copying the leader's transaction xids"))?; // alloc: Cold
    }
    Ok(())
}

fn no_xids(out: &mut budget::ChargedVec<TransactionId>) -> PgResult<()> {
    out.clear();
    Ok(())
}

impl Ambient {
    pub const SERVER: Ambient = Ambient { interrupts: cancelpoll::interrupts::check, abort: server_abort, arm: arm_worker, xids: leader_xids };
    pub const HARNESS: Ambient = Ambient { interrupts: no_interrupts, abort: no_abort, arm: no_arm, xids: no_xids };
}

/// A one-off job on a pool thread: the gate and the arming as a section's claim, then the job.
struct Background {
    job: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    arm: ArmFn,
    stamp: SessionCarry,
}

impl TaskSetWork for Background {
    fn before_claims(&self) {
        let _ = parallel::standing::run_pool_gate(); // D611: a stale identity retires RAW here, before the job is taken
    }

    fn run_morsel(&self, _worker: usize, _range: runtime::MorselRange) {
        if parallel::standing::run_pool_gate() == Some(false) || arm_once(self.arm).is_err() {
            return;
        }
        // a job's stashed errors go with it (its structured error is dropped)
        let _stash = gravity_hosts::StashScope::enter();
        let Ok(_armed) = self.stamp.arm() else { return }; // D316 / D2350: the claim registry's state disarms when the job ends
        if let Some(job) = self.job.lock().unwrap_or_else(|p| p.into_inner()).take() {
            // unwind-ok: worker-containment
            // a background job's structured error is dropped, as before; D2933: an exit payload (a PANIC, a
            // FATAL, a kill: never swallowed, stack.rs's durability law) or a bug's plain panic (C's abort())
            // reaches the thread top
            if let Err(unwind) = unwind::catch(job) {
                if !parallel::standing::is_structured_error(&*unwind) {
                    parallel::standing::resume_exit_or_abort(unwind);
                }
            }
        }
    }

    fn finalize(&self) {}
}

struct OneUnit;

impl runtime::MorselSource for OneUnit {
    fn total_granules(&self) -> u64 {
        1
    }

    fn next_boundary_after(&self, _start: u64) -> u64 {
        1
    }

    fn startup_c0(&self) -> u64 {
        1
    }

    fn whole_boundary_claims(&self) -> bool {
        true
    }
}

pub struct RuntimePool {
    rt: Arc<Runtime>,
    ambient: Ambient,
    query_ids: AtomicU64,
    sections: AtomicU64,
    claims_run: AtomicU64,
    wakes: AtomicU64,
    quanta: AtomicU64,
    /// What the process's first section took, submit to completion (0 before it).
    first_section_micros: AtomicU64,
}

impl RuntimePool {
    pub fn new(rt: Arc<Runtime>, ambient: Ambient) -> RuntimePool {
        // the pool's workers are armed for temp files: the process's spill store goes with it
        let _ = install_spills();
        RuntimePool { rt, ambient, query_ids: AtomicU64::new(1), sections: AtomicU64::new(0), claims_run: AtomicU64::new(0), wakes: AtomicU64::new(0), quanta: AtomicU64::new(0), first_section_micros: AtomicU64::new(0) } // alloc: Cold
    }

    /// The process pool, when the postmaster started one; `None` means
    /// every section runs serially (a bind-time verdict, never a failure).
    pub fn from_global() -> Option<RuntimePool> {
        runtime::global().map(|rt| RuntimePool::new(Arc::clone(rt), Ambient::SERVER))
    }

    pub fn threads(&self) -> u16 {
        self.rt.nthreads().min(u16::MAX as usize) as u16
    }

    pub fn sections(&self) -> u64 {
        self.sections.load(Ordering::Relaxed)
    }

    pub fn claims_run(&self) -> u64 {
        self.claims_run.load(Ordering::Relaxed)
    }

    /// Sections whose end reached the leader as a completion wake.
    pub fn wakes(&self) -> u64 {
        self.wakes.load(Ordering::Relaxed)
    }

    /// Park quanta that expired without a wake (the interrupt backstop ran).
    pub fn quanta(&self) -> u64 {
        self.quanta.load(Ordering::Relaxed)
    }
}

/// The section's claims as the runtime's source; `claims` is the cursor `run_led` borrowed (raw.rs `Units::claims`).
struct Units {
    claims: *const (dyn MorselSource + 'static),
    total: u64,
    c0: u64,
    bounded: bool,
    /// The cursor's `whole_until` (`u64::MAX` for a led section: R1D's ordered output takes every claim whole) and
    /// `endgame_split`.
    until: u64,
    endgame: bool,
    /// skewsplit (D3320): the section, when its cursor lets a running claim be split for an idle worker (`Work::board`;
    /// never a led section's): `steal` asks the running claim with the most time left for its back half.
    cutting: Option<Arc<Work>>,
}

impl runtime::MorselSource for Units {
    /// The cursor's units, or none once the section closed it (the walk's bound): the runtime reads the total per claim.
    fn total_granules(&self) -> u64 {
        if self.claims().remaining() == 0 {
            0
        } else {
            self.total
        }
    }

    fn next_boundary_after(&self, start: u64) -> u64 {
        self.claims().next_boundary_after(start)
    }

    fn first_live_at(&self, start: u64) -> u64 {
        self.claims().first_live_at(start)
    }

    fn startup_c0(&self) -> u64 {
        self.c0
    }

    fn whole_boundary_claims(&self) -> bool {
        self.bounded
    }

    fn whole_until(&self) -> u64 {
        self.until
    }

    fn endgame_split(&self) -> bool {
        self.endgame
    }

    /// skewsplit (D3320): the cursor is exhausted; ask the running claims until one hands over its back half, none is worth
    /// cutting (the section's end) or the section closed (an error, a walk's bound).
    fn steal(&self) -> Option<runtime::MorselRange> {
        let work = self.cutting.as_ref()?;
        let board = work.board.as_ref()?;
        // bounded("other: each round asks or waits one PARK slice; it ends when no running claim is worth cutting and nothing handed over is pending, which every claim's end brings")
        loop {
            if self.claims().remaining() == 0 {
                return None;
            }
            match board.ask(&work.claims_run, &mut steal_wait) {
                gravity_driver::claimcut::Ask::Took(lo, hi) => return Some(lo..hi),
                gravity_driver::claimcut::Ask::Wait => steal_wait(),
                gravity_driver::claimcut::Ask::Nothing => return None,
            }
        }
    }
}

/// skewsplit: an asking worker's wait for an owner's answer or a pending cut, its runtime permit given back meanwhile (as a
/// slot-less `lease`): one 200 us slice, an owner answering at its next granule step.
fn steal_wait() {
    let _permit = runtime::over_width_section();
    let _ = ::waiter::park_timeout(core::time::Duration::from_micros(200));
}

struct Work {
    job: *const Job<'static>,
    claims: *const (dyn MorselSource + 'static),
    /// R1D's ordered output (`run_leading`), null otherwise: a claim is admitted before it leases a slot (D278).
    order: *const gravity_driver::ClaimOrder,
    slots: Box<[AtomicBool]>,
    /// Slot-less workers wait here; a slot release notifies one (`lease`). pgsync's condvar: std's on
    /// native, the permit-hooked wrapper under DST (the timed wait is a TimedPark the scheduler sees).
    idle: (Mutex<()>, pgsync::Condvar),
    funnel: ErrorFunnel,
    failed: AtomicBool,
    /// D2933: a claim hit a bug's plain panic (C's abort()): the leader PANICs once the section completed
    bug_panic: AtomicBool,
    rg: Mutex<Option<RgHandle>>,
    pg: Mutex<Option<(u64, u16, Box<PgError>)>>,
    /// The leader's xids, charged to its statement by construction: the last reference to the section's `Work` frees them on
    /// whichever thread it drops, and the charge gives the bytes back there.
    xids: budget::ChargedVec<TransactionId>,
    arm: ArmFn,
    /// The leader's session a claim reads (encoding, default locale, collations, timezone and the carried GUCs),
    /// captured at submit; each claim arms it (`SessionCarry::arm`).
    stamp: SessionCarry,
    stats: SectionStats,
    /// The claims run, one cell per ordinal: a claim adds to its lease's own line (one shared counter, its line beside the
    /// census's `last_end`, was the W16 fold cells' hottest store: 57% of the workers' samples on 511 answered parts).
    claims_run: Box<[Counted]>,
    /// The census (`PGRUST_GRAVITY_PHASES`, read once at submit): the submit instant, each ordinal's first claim start and
    /// the last claim's end, ns after it; stamped only under it (a clock read and a contended max per claim otherwise).
    census: bool,
    t0: std::time::Instant,
    first: Box<[AtomicU64]>,
    last_end: AtomicU64,
    /// D3024 (b) skew census (under `census` only): the longest claim's wall and the claims' summed wall, ns, so a section
    /// whose claims are uneven (one huge owner run) shows its longest claim beside the mean.
    claim_max: AtomicU64,
    claim_sum: AtomicU64,
    /// skewsplit (D3320): the section's split state when its claims may be cut (`Units::cutting`); each claim runs inside
    /// it on its ordinal's cut (`Counted`).
    board: Option<gravity_driver::claimcut::Board>,
}

/// A per-ordinal cell on its own lines (a neighbour's adjacent-line prefetch included): the ordinal's claims-run counter
/// and (skewsplit) its running claim's cut, both written by the ordinal's claim.
#[repr(align(128))]
struct Counted(AtomicU64, gravity_driver::claimcut::Cut);

impl AsRef<gravity_driver::claimcut::Cut> for Counted {
    fn as_ref(&self) -> &gravity_driver::claimcut::Cut {
        &self.1
    }
}

impl Work {
    fn try_lease(&self) -> Option<u16> {
        self.slots.iter().position(|s| s.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_ok()).map(|i| i as u16)
    }

    /// `None` once the section has failed (the claims are closed; nothing to wait for).
    fn lease(&self) -> Option<u16> {
        let i = self.wait_lease()?;
        // D2566: a failed claim marks the section failed before its slot goes back (`Leased`), so this slot may be one whose
        // ordinal state the failing kernel left half-done (a var sort run compacted for a put that failed): never run on it
        if self.failed.load(Ordering::Acquire) {
            self.release(i);
            return None;
        }
        Some(i)
    }

    fn wait_lease(&self) -> Option<u16> {
        if let Some(i) = self.try_lease() {
            return Some(i);
        }
        // The runtime's width is a grant it nudges toward, not a bound on who pulls morsels: every
        // awake pool thread takes claims; one without a slot must neither spin (at W = 4 on 16 vCPUs,
        // 14 spinners burned ~12 cores) nor sleep on a timer (~100 µs idle per handoff): it waits on
        // the slot condvar WITHOUT its runtime permit (retaken on drop, slot in hand — a slot-less
        // waiter holding a permit left K >= W standbys and W spilling slot holders deadlocked).
        let _permit = runtime::over_width_section();
        // bounded("other: a slot-less pool thread's wait for a slot, in 200 us condvar slices that each test the section's failed flag; a cancel ends it through the slot holders: run_led's interrupt branch sets the abort byte, each holder's claim stops at its own poll, and Leased's drop (failed set first on an error) releases the slot and notifies, so the waiter takes the slot or sees failed within a slice")
        loop {
            if self.failed.load(Ordering::Acquire) {
                return None;
            }
            let g = self.idle.0.lock().unwrap_or_else(|p| p.into_inner());
            if self.slots.iter().all(|s| s.load(Ordering::Acquire)) {
                let _ = self.idle.1.wait_timeout(g, core::time::Duration::from_micros(200));
            }
            if let Some(i) = self.try_lease() {
                return Some(i);
            }
        }
    }

    fn release(&self, ordinal: u16) {
        {
            // under the lock, so a waiter that saw every slot busy cannot miss this release
            let _g = self.idle.0.lock().unwrap_or_else(|p| p.into_inner());
            self.slots[ordinal as usize].store(false, Ordering::Release);
        }
        self.idle.1.notify_one();
    }

    fn body(&self, range: runtime::MorselRange) -> Result<(), (u16, Error)> {
        // The pool thread's PGPROC comes up at its first gate (deferred to a
        // serve by the postmaster's crash-window discipline); a section is
        // such a serve. `None`: a bare runtime without pool identity wiring.
        if parallel::standing::run_pool_gate() == Some(false) {
            return Err((0, Error::host("gravity: pool thread without an identity")));
        }
        arm_once(self.arm).map_err(|e| (0, stash(e)))?;
        let _armed = self.stamp.arm().map_err(|e| (0, stash(e)))?; // D316 / D2350: the claim registry's state disarms at the claim's end
        // D278: the ordered window admits the claim BEFORE its lease. Admitted after it, a claim waiting for room held its
        // slot while the head's thread waited in `lease` for one: W slot holders parked until ROOM_WAIT (20 ms) per round.
        let claim = MorselRange::new(range.start, range.end);
        let order = self.order();
        if order.is_some_and(|o| !o.admit(claim)) {
            return Ok(());
        }
        // admitted, it settles on every path: the job's result, or failed (a refused lease, an unwind)
        let mut settle = Settle { order, claim, ok: false };
        let Some(ordinal) = self.lease() else { return Ok(()) };
        // the census's two instants per claim (the DST-REVIEW'd time sites): its start (the ordinal's first claim) and its end
        // (`last_end`); D3024 (b): their difference is the claim's wall (`claim_max`, `claim_sum`)
        let began = self.census.then(|| self.t0.elapsed().as_nanos().max(1) as u64);
        if let Some(b) = began.filter(|_| self.first[ordinal as usize].load(Ordering::Relaxed) == 0) {
            self.first[ordinal as usize].store(b, Ordering::Relaxed);
        }
        xact::enter_section_xids(self.xids.as_slice());
        let mut leased = Leased { work: self, ordinal, ok: false }; // released on the unwind too: a panicked claim's slot held every slot-less thread (a hang)
        let job = self.job();
        // D4242: the smgr table's AtEOXact_SMgr on a thread that ends no transaction (a serve's own ends its)
        if !xact::IsTransactionOrTransactionBlock() {
            smgr::smgr_unit_start().map_err(|e| (ordinal, stash(e)))?;
        }
        let start = SectionStats::claim_begin();
        // skewsplit: the claim's cut, offered to the claim's node and ended (a pending ask declined) when the job returns
        let r = match self.board.as_ref() {
            Some(b) => b.run(&self.claims_run[ordinal as usize].1, claim.lo, claim.hi, || job(ordinal, claim)),
            None => job(ordinal, claim),
        };
        settle.ok = r.is_ok();
        leased.ok = r.is_ok();
        drop(settle);
        self.stats.claim_end(&start);
        if let Some(b) = began {
            let end = self.t0.elapsed().as_nanos() as u64;
            self.last_end.fetch_max(end, Ordering::Relaxed);
            self.claim_max.fetch_max(end.saturating_sub(b), Ordering::Relaxed);
            self.claim_sum.fetch_add(end.saturating_sub(b), Ordering::Relaxed);
        }
        self.claims_run[ordinal as usize].0.fetch_add(1, Ordering::Relaxed);
        drop(leased);
        r.map_err(|e| (ordinal, e))
    }
}

/// An admitted ordered claim's settle with its order (`ClaimOrder::finish`), on drop: the head moves past it only then.
struct Settle<'a> {
    order: Option<&'a gravity_driver::ClaimOrder>,
    claim: MorselRange,
    ok: bool,
}

impl Drop for Settle<'_> {
    fn drop(&mut self) {
        if let Some(o) = self.order {
            o.finish(self.claim, self.ok);
        }
    }
}

struct Leased<'a> {
    work: &'a Work,
    ordinal: u16,
    ok: bool,
}

impl Drop for Leased<'_> {
    fn drop(&mut self) {
        xact::leave_section_xids();
        // D2566: a failed claim (its error or an unwind; `fail` follows) fails the section BEFORE its slot is free: a claim
        // that leased the slot in between ran on the half-done ordinal (a SIGSEGV re-compacting a compacted sort run)
        if !self.ok {
            self.work.failed.store(true, Ordering::Release);
        }
        self.work.release(self.ordinal);
    }
}

impl Work {

    fn fail(&self, claim_lo: u64, ordinal: u16, e: Error) {
        if let Some(pg) = take_any_error() {
            let mut slot = self.pg.lock().unwrap_or_else(|p| p.into_inner());
            if slot.as_ref().is_none_or(|(lo, ord, _)| (claim_lo, ordinal) < (*lo, *ord)) {
                *slot = Some((claim_lo, ordinal, pg));
            }
        }
        self.funnel.offer(claim_lo, ordinal, e);
        self.failed.store(true, Ordering::Release);
        self.claims().close();
        // claims above it are skipped, those below start (their rows and a lower error first); none waits out ROOM_WAIT
        if let Some(order) = self.order() {
            order.stop_above(claim_lo);
        }
        if let Some(rg) = self.rg.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            rg.abort();
        }
    }
}

/// A claim's unwind that is a PostgreSQL ERROR: a `PgError` payload (boxed or not) at level ERROR, nothing above it.
#[cold]
fn raised_error(unwind: Box<dyn std::any::Any + Send>) -> Option<Box<PgError>> {
    let pg = match unwind.downcast::<Box<PgError>>() {
        Ok(pg) => *pg,
        Err(unwind) => unwind.downcast::<PgError>().ok()?,
    };
    (pg.level() == types_error::ERROR).then_some(pg)
}

impl TaskSetWork for Work {
    /// D611: the pool gate before this thread claims: a thread whose identity died across the crash fence unwinds
    /// PoolRetireRaw here holding no range (the launch_backend glue respawns it cold) and the section's other threads
    /// claim every morsel; the claim's own gate (`body`) stays the backstop.
    fn before_claims(&self) {
        let _ = parallel::standing::run_pool_gate();
    }

    fn run_morsel(&self, _worker: usize, range: runtime::MorselRange) {
        if self.failed.load(Ordering::Acquire) {
            return;
        }
        // the claim's stashed errors are its own (StashScope): `fail` takes them before it ends, and what an unwound
        // claim left goes with it, never the thread's next claim's
        let _stash = gravity_hosts::StashScope::enter();
        // unwind-ok: worker-containment
        match unwind::catch(|| self.body(range.clone())) {
            Ok(Ok(())) => {}
            Ok(Err((ordinal, e))) => self.fail(range.start, ordinal, e),
            Err(unwind) => {
                // an exit unwind (a FATAL's thread exit, a PANIC, a kill) fails the section and goes on, never
                // flattened; D611: PoolRetireRaw (a fence that moved between the pre-claim gate and this claim) is an
                // exit too: the thread must die RAW and respawn, never be swallowed here and retire again at every
                // later claim
                if parallel::standing::is_exit_unwind(&*unwind) || unwind.is::<parallel::standing::PoolRetireRaw>() {
                    self.fail(range.start, 0, Error::host("gravity: worker panicked in a claim"));
                    std::panic::resume_unwind(unwind);
                }
                // D2933: a bug's plain panic is C's abort(), here the leader's PANIC: the section fails (no thread
                // waits on this claim) and this thread returns
                // so the runtime settles the morsel (a pool thread that dies inside a section can leave its runtime
                // group waiting for a drain no thread comes to do); the leader PANICs once the section completed
                // (run_led) and the crash restart retires every pool thread (the fence). The hook printed the panic
                if !parallel::standing::is_structured_error(&*unwind) {
                    self.fail(range.start, 0, Error::host("gravity: worker panicked in a claim"));
                    self.bug_panic.store(true, Ordering::Release);
                    return;
                }
                // D314: a PostgreSQL ERROR raised by unwinding in the claim (a host's, parked below the JIT'd frames and
                // resumed at `Code::call`) is the claim's error with its SQLSTATE, as a PostgreSQL worker's reaches the
                // leader; a structured payload above ERROR keeps the host error
                let e = match raised_error(unwind) {
                    Some(pg) => stash(pg),
                    None => Error::host("gravity: worker panicked in a claim"),
                };
                self.fail(range.start, 0, e);
            }
        }
    }

    fn finalize(&self) {}
}

/// The census's `split=`: the live parts of `c` from unit `until` on (the tail rule's parts cut inside), 0 without a tail.
fn tail_parts(c: &dyn MorselSource, until: u64) -> u64 {
    let (total, mut at, mut n) = (c.total(), until, 0);
    // bounded("catalog-sized: one step per part of the relation past the tail, census only")
    while at < total {
        at = c.first_live_at(at);
        if at >= total {
            break;
        }
        n += 1;
        at = c.next_boundary_after(at);
    }
    n
}

fn slots(width: u16) -> Box<[AtomicBool]> {
    let mut v = Vec::with_capacity(width as usize); // alloc: Exec
    v.resize_with(width as usize, || AtomicBool::new(false));
    v.into_boxed_slice()
}

fn counted(width: u16) -> Box<[Counted]> {
    let mut v = Vec::with_capacity(width as usize); // alloc: Exec
    v.resize_with(width as usize, || Counted(AtomicU64::new(0), gravity_driver::claimcut::Cut::new()));
    v.into_boxed_slice()
}

fn stamps(width: u16) -> Box<[AtomicU64]> {
    let mut v = Vec::with_capacity(width as usize); // alloc: Exec
    v.resize_with(width as usize, || AtomicU64::new(0));
    v.into_boxed_slice()
}

impl Pool for RuntimePool {
    fn threads(&self) -> u16 {
        RuntimePool::threads(self)
    }

    fn sections_run(&self) -> u64 {
        self.sections.load(Ordering::Relaxed)
    }

    fn spawn(&self, job: Box<dyn FnOnce() + Send>) -> bool {
        let work: Arc<dyn TaskSetWork> = Arc::new(Background { job: Mutex::new(Some(job)), arm: self.ambient.arm, stamp: SessionCarry::capture() }); // alloc: Cold
        let source: Arc<dyn runtime::MorselSource> = Arc::new(OneUnit); // alloc: Cold
        let spec = QuerySpec { query_id: self.query_ids.fetch_add(1, Ordering::Relaxed), tasksets: vec![TaskSetSpec { source, work, deps: Vec::new() }] }; // alloc: Cold
        let (_rg, _waiter) = self.rt.submit_with_width(spec, WidthRequest::unbounded(1));
        true
    }

    fn cold_micros(&self) -> (u64, u64) {
        (gravity_hosts::first_arm_micros(), self.first_section_micros.load(Ordering::Relaxed))
    }

    fn run(&self, width: u16, claims: &dyn MorselSource, job: &Job<'_>) -> Result<(), FirstError> {
        self.run_led(width, claims, job, None)
    }

    fn run_leading(&self, width: u16, claims: &dyn MorselSource, job: &Job<'_>, lead: &mut dyn FnMut(), order: &gravity_driver::ClaimOrder) -> Result<(), FirstError> {
        self.run_led(width, claims, job, Some((lead, order)))
    }

    fn claims_run(&self) -> u64 {
        RuntimePool::claims_run(self)
    }
}

fn unpark_word(word: u64) {
    let _ = ::waiter::unpark_word(word);
}

/// D2906: a leader leaving `run_led` by an unwind while its section runs (a crash restart's quickdie or a FATAL in the
/// interrupt check, a raise in R1D's `lead`) stops the section and waits for it to complete before its frames go: the
/// claims run the leader's job, read its cursor and its section state (`Open`'s slots, the program, the ordered window)
/// through `Work`'s raw pointers, so without the wait a pool thread ran `run_claim` and its unwind over freed leader
/// memory (a SIGSEGV in `Open::run_claim`, mimalloc's page lists corrupted after it: the whole server down instead of
/// PostgreSQL's crash restart, ~10 % of trials under load). The same stop as the interrupt's error path (D328): the
/// window stopped, the group aborted, the abort byte held set while the claims finish theirs. `waiter.wait` parks on the
/// runtime's waiter (no interrupt check, no unwind in a drop); the claims end at their next abort poll or at the crash
/// fence's gate (PoolRetireRaw), and the runtime completes the group once each has left.
struct Quiesce<'a> {
    rg: &'a RgHandle,
    waiter: &'a CompletionWaiter,
    order: Option<&'a gravity_driver::ClaimOrder>,
    abort: AbortFn,
    armed: bool,
}

impl Drop for Quiesce<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(order) = self.order {
            order.stop();
        }
        self.rg.abort();
        (self.abort)(true);
        let _ = self.waiter.wait();
        (self.abort)(false);
    }
}

/// The running pool thread's park word (R1D: a claim that waits for room in the ordered window).
fn current_word() -> u64 {
    ::waiter::current_handle().as_u64()
}

/// Park the running thread up to `ms` (an unpark of its word ends it early; a recheck wake is a spurious one): a claim
/// waiting for room, admitted before its lease (D278), so it holds no slot; its runtime permit goes back for the wait as
/// in a slot-less `lease` (a parked claim keeping it could hold the permit the head's thread retakes after its lease).
fn park_ms(ms: u64) {
    let _permit = runtime::over_width_section();
    let _ = ::waiter::park_timeout(core::time::Duration::from_millis(ms));
}

impl RuntimePool {
    /// The section on the runtime, the leader parked until it completes; `lead` (R1D's ordered output) runs on every wake.
    fn run_led(&self, width: u16, claims: &dyn MorselSource, job: &Job<'_>, mut lead: Option<(&mut dyn FnMut(), &gravity_driver::ClaimOrder)>) -> Result<(), FirstError> {
        assert!(width >= 1, "width {width}");
        parallel::standing::note_section_leader(); // D611: this leader proves shared memory live: a respawned thread may bind
        // the section's own xids, charged to the leader's statement (or portal): no pool-lifetime buffer to copy them out of
        let mut xids = budget::ChargedVec::new(Some(budget::charge::context().0), budget::Kind::Region);
        if let Err(e) = (self.ambient.xids)(&mut xids) {
            return Err(FirstError { claim_lo: 0, ordinal: 0, error: stash(e) });
        }
        // skewsplit (D3320): a running claim may be cut for an idle worker where the cursor allows it (never in a led section:
        // its claims are emitted in claim order); a half must clear the runtime's photo-finish floor
        let board = (lead.is_none() && width >= 2 && claims.split_running()).then(|| gravity_driver::claimcut::Board::new(runtime::DEFAULT_T_MIN_NS));
        let work = Arc::new(Work { // alloc: Exec
            job: raw::erase_job(job),
            claims: raw::erase_claims(claims),
            order: lead.as_ref().map_or(core::ptr::null(), |(_, o)| *o as *const gravity_driver::ClaimOrder),
            slots: slots(width),
            idle: (Mutex::new(()), pgsync::Condvar::new()),
            funnel: ErrorFunnel::new(),
            failed: AtomicBool::new(false),
            bug_panic: AtomicBool::new(false),
            rg: Mutex::new(None),
            pg: Mutex::new(None),
            xids,
            arm: self.ambient.arm,
            stamp: SessionCarry::capture(),
            stats: SectionStats::new(),
            claims_run: counted(width),
            census: gravity_parallel::census_on(),
            t0: std::time::Instant::now(),
            first: stamps(width),
            last_end: AtomicU64::new(0),
            claim_max: AtomicU64::new(0),
            claim_sum: AtomicU64::new(0),
            board,
        });
        let until = if lead.is_some() { u64::MAX } else { claims.whole_until() };
        let cutting = work.board.is_some().then(|| work.clone());
        let source: Arc<dyn runtime::MorselSource> = Arc::new(Units { claims: work.claims, total: claims.total(), c0: claims.startup_c0(), bounded: claims.whole_boundary_claims(), until, endgame: claims.endgame_split(), cutting }); // alloc: Exec
        let spec = QuerySpec { query_id: self.query_ids.fetch_add(1, Ordering::Relaxed), tasksets: vec![TaskSetSpec { source, work: work.clone(), deps: Vec::new() }] }; // alloc: Exec
        let word = ::waiter::current_handle().as_u64();
        // armed before any worker runs: the first head can finish (and a claim wait for room) before the leader's loop
        if let Some((_, order)) = &lead {
            order.wake.arm(word, unpark_word);
            order.wake.arm_park(current_word, park_ms);
        }
        let started = std::time::Instant::now();
        let (rg, waiter): (RgHandle, CompletionWaiter) = self.rt.submit_with_width(spec, WidthRequest::unbounded(width as u32));
        // D2906: from here the pool's threads run claims over the job, the cursor and the section state this leader's
        // frames own: armed until the section completes, so an unwind out of the loop below quiesces it first
        let mut quiesce = Quiesce { rg: &rg, waiter: &waiter, order: lead.as_ref().map(|(_, o)| *o), abort: self.ambient.abort, armed: true };
        *work.rg.lock().unwrap_or_else(|p| p.into_inner()) = Some(rg.clone());
        let first = self.sections.fetch_add(1, Ordering::Relaxed) == 0;
        let mut interrupted: Option<Error> = None;
        let mut woken = false;
        let outcome = loop {
            if let Some((f, _)) = lead.as_mut() {
                (**f)();
            }
            if let Some(o) = waiter.try_wait() {
                self.wakes.fetch_add(woken as u64, Ordering::Relaxed);
                break o;
            }
            if let Err(e) = (self.ambient.interrupts)() {
                interrupted = Some(stash(e));
                if let Some((_, order)) = &lead {
                    order.stop();
                }
                rg.abort();
                // D328: ProcessInterrupts raised PostgreSQL's error and cleared the abort byte the running claims poll once
                // per window, so each ran on to its claim's end (a 20 ms statement_timeout landed 500 ms late at W2 / W8):
                // set again for them until they stop, then cleared, so it outlives no interrupt. A held-off or a stale
                // byte never comes here (ProcessInterrupts returns Ok for it) and the section goes on.
                (self.ambient.abort)(true);
                let o = waiter.wait();
                (self.ambient.abort)(false);
                break o;
            }
            // The registration is under the completion's lock: `complete`
            // either sees the word and unparks it or this call sees the outcome.
            if waiter.register_waker_word(word) {
                continue;
            }
            match ::waiter::park_timeout(core::time::Duration::from_millis(PARK_QUANTUM_MS as u64)) {
                ::waiter::ParkResult::Notified => woken = true,
                ::waiter::ParkResult::TimedOut | ::waiter::ParkResult::Recheck => {
                    self.quanta.fetch_add(1, Ordering::Relaxed);
                }
            }
        };
        // the section completed: no pool thread holds a claim over the leader's state
        quiesce.armed = false;
        // D2933: a claim's bug panic is the leader's PANIC (C's abort(): the backend dies as crashed, the postmaster
        // runs the crash restart), now that no pool thread holds a claim over the leader's state
        if work.bug_panic.load(Ordering::Acquire) {
            *work.rg.lock().unwrap_or_else(|p| p.into_inner()) = None;
            std::panic::resume_unwind(Box::new(types_error::PanicExitThread)); // alloc: Cold
        }
        if first {
            let _ = self.first_section_micros.compare_exchange(0, started.elapsed().as_micros().max(1) as u64, Ordering::AcqRel, Ordering::Relaxed);
        }
        claims.close();
        work.stats.fold_into_leader();
        let claims_src = claims;
        let claims: u64 = work.claims_run.iter().map(|c| c.0.load(Ordering::Relaxed)).sum();
        if work.census {
            let ret = work.t0.elapsed().as_nanos() as u64;
            // one read of each engaged thread's first claim: their count, the earliest and the latest
            let (engaged, lo, hi) = work.first.iter().map(|a| a.load(Ordering::Relaxed)).filter(|&n| n != 0).fold((0usize, u64::MAX, 0u64), |(c, lo, hi), n| (c + 1, lo.min(n), hi.max(n)));
            let lo = if engaged == 0 { 0 } else { lo };
            let last = work.last_end.load(Ordering::Relaxed);
            let (waits, overruns) = work.order().map_or((0, 0), |o| (o.waits.load(Ordering::Relaxed), o.overruns.load(Ordering::Relaxed)));
            let (cmax, csum) = (work.claim_max.load(Ordering::Relaxed), work.claim_sum.load(Ordering::Relaxed));
            let split = tail_parts(claims_src, until);
            let cuts = work.board.as_ref().map_or(0, |b| b.cuts_made.load(Ordering::Relaxed));
            eprintln!("PHASES pool width={width} engaged={} first_claim_us={:.0} all_engaged_us={:.0} last_end_us={:.0} tail_us={:.0} return_us={:.0} claims={} order_waits={waits} order_overruns={overruns} claim_max_us={:.0} claim_mean_us={:.0} split={split} cuts={cuts}", engaged, lo as f64 / 1e3, hi as f64 / 1e3, last as f64 / 1e3, ret.saturating_sub(last) as f64 / 1e3, ret as f64 / 1e3, claims, cmax as f64 / 1e3, csum as f64 / 1e3 / claims.max(1) as f64); // alloc: Cold
        }
        self.claims_run.fetch_add(claims, Ordering::Relaxed);
        *work.rg.lock().unwrap_or_else(|p| p.into_inner()) = None;
        if let Some(error) = interrupted {
            return Err(FirstError { claim_lo: u64::MAX, ordinal: 0, error });
        }
        match work.funnel.take() {
            Some(first) => {
                if let Some((lo, ord, mut pg)) = work.pg.lock().unwrap_or_else(|p| p.into_inner()).take() {
                    if (lo, ord) == (first.claim_lo, first.ordinal) {
                        // The worker's errstart set the report's destinations on its own thread, which has no client
                        // (output_to_client false). Re-raised here the error is the leader's report, so the leader decides
                        // them when it emits it, as C's ThrowErrorData runs errstart again in the leader.
                        pg.output_to_server = None;
                        pg.output_to_client = None;
                        let _ = stash(pg);
                    }
                }
                Err(first)
            }
            None if outcome == RgOutcome::Aborted => Err(FirstError { claim_lo: u64::MAX, ordinal: 0, error: Error::host("gravity: section aborted by the runtime") }),
            None => Ok(()),
        }
    }
}


/// The process's spill files (§2 decision 3): `spillset` under the driver's `SpillFactory`
/// (`breakers.md`). Installed beside the pool; the leader and the pool's workers have temp
/// file access (`arm_worker`), so a set is created on first spill and its files removed when
/// the Node's `SpillTable` drops it.
pub struct SpillSets;

struct Set {
    set: Arc<spillset::SpillSet>,
}

struct File {
    /// CONVERSION WAVE 2 (alloclint round 3): the directory's extent lists (`spillset::SpillFile::dir_bytes`, grown at each
    /// epoch's finish) charged to the slot's owner (`charge_to`; none: they stay on the writing thread's account). Declared
    /// before `f`, so the charge drops first and its bytes reach the dropping thread before the lists free.
    dir: Option<budget::Charge>,
    owner: Option<budget::Owner>,
    f: spillset::SpillFile,
    ctx: gravity_driver::mem::root::Root,
    /// D1719: an epoch write in progress. `spillset::SpillFile::read_part` requires a frozen file (it iterates the epochs a
    /// write appends to); a read that meets a write fails by name instead of racing it (the driver's stored-stream lock
    /// keeps its pair passes apart; this catches any caller that does not).
    writing: AtomicBool,
}

/// D1719: `File::writing` held for one epoch write, cleared on every path out of it.
struct Writing<'a>(&'a AtomicBool);

impl Drop for Writing<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl File {
    /// The directory's charge fitted to its bytes now, on the thread whose epoch finish grew them (door 1: the growth is
    /// on this thread's account).
    /// Its fields alone (an epoch write holds `writing` borrowed meanwhile).
    fn fit_dir(dir: &mut Option<budget::Charge>, owner: Option<budget::Owner>, f: &spillset::SpillFile) {
        let now = f.dir_bytes();
        if dir.is_none() {
            let Some(o) = owner else { return };
            *dir = Some(budget::Charge::open(o, budget::Release::Statement, budget::Kind::Spill, budget::charge::Door::Heap, 0));
        }
        if let Some(c) = dir.as_mut() {
            let had = c.committed();
            if now > had {
                c.commit(now - had);
            } else if had > now {
                c.free(had - now);
            }
        }
    }

    fn frozen(&self) -> gravity_driver::Result<()> {
        match self.writing.load(Ordering::Acquire) {
            true => Err(Error::host("gravity: a spill file read while its owner writes an epoch (D1719: the read needs a frozen file)")),
            false => Ok(()),
        }
    }
}

impl gravity_driver::SpillFactory for SpillSets {
    fn create(&self) -> gravity_driver::Result<Box<dyn gravity_driver::SpillSet>> {
        Ok(Box::new(Set { set: spillset::SpillSet::create().map_err(stash)? })) // alloc: Exec
    }

    /// D491: the serial owners' chunk past the grant pool, a session temp file (`spillset::with_map_file`: temp_file_limit
    /// charged, blocks allocated) mapped shared; the file is unlinked once mapped.
    fn map_temp(&self, bytes: usize) -> gravity_driver::Result<Option<gravity_driver::TempMap>> {
        spillset::with_map_file(bytes as u64, |fd| gravity_driver::TempMap::shared(fd, bytes)).map_err(stash)
    }

    /// C10 (D4168): a session maps its temp files, so a spool past W and G writes there at once. D4945: only where
    /// `TempMap::shared` maps one; wasm32 maps none, so a spool there keeps the pool's room past G (D225).
    fn maps_temp(&self) -> bool {
        cfg!(not(target_family = "wasm"))
    }
}

impl gravity_driver::SpillSet for Set {
    fn open(&mut self, purpose: &str, generation: u64, worker: usize, nparts: u32) -> gravity_driver::Result<Box<dyn gravity_driver::SpillFile>> {
        let name = spillset::SpillSet::file_name(purpose, generation, worker); // alloc: Exec
        Ok(Box::new(File { dir: None, owner: None, f: spillset::SpillFile::new(Arc::clone(&self.set), name, nparts), ctx: gravity_driver::mem::root::Root::new("GravitySpill"), writing: AtomicBool::new(false) })) // alloc: Exec
    }

    /// D3203: `spillset::SpillSet::delete_files_with`; an error (a directory the leader could not read) unlinks nothing
    /// here and leaves every file to the set's drop, whose walk reports it.
    fn delete_files_with(&mut self, min_bytes: u64, fan: &dyn Fn(usize, &(dyn Fn(usize) + Sync))) -> (usize, u64, bool) {
        self.set.delete_files_with(min_bytes, fan).unwrap_or((0, 0, false))
    }
}

impl gravity_driver::SpillFile for File {
    fn write_epoch(&mut self, parts: &[(u32, &[u8])]) -> gravity_driver::Result<()> {
        // D3964 / D3966: every epoch polls at its start too: a sort run's or a scatter's epochs each fit one write, so a
        // run of them polled nowhere (2.5 s / 1.3 s on the stress run)
        let mut step = cancelpoll::step("pool write_epoch: an epoch's writes of IO_CHUNK bytes");
        step.tick()?;
        self.writing.store(true, Ordering::Release);
        let _writing = Writing(&self.writing);
        let mut w = self.f.begin_epoch(self.ctx.mcx()).map_err(stash)?;
        // spillopt #3: the epoch's spans in gathered writes, not through the BufFile's 8 KB buffer. D3902: at most
        // `IO_CHUNK` bytes a write, the statement's cancel polled between them (a write under the cgroup's reclaim took
        // 1.5-1.9 s whole); a canceled epoch is dropped unfinished, which abandons it (nothing committed)
        let mut chunk: [(u32, &[u8]); 64] = [(0, &[]); 64];
        let (mut n, mut held) = (0usize, 0usize);
        for &(part, bytes) in parts {
            for piece in bytes.chunks(IO_CHUNK) {
                if n == chunk.len() || n != 0 && held + piece.len() > IO_CHUNK {
                    w.write_parts(&chunk[..n]).map_err(stash)?;
                    (n, held) = (0, 0);
                    step.tick()?;
                }
                chunk[n] = (part, piece);
                n += 1;
                held += piece.len();
            }
        }
        if n != 0 {
            w.write_parts(&chunk[..n]).map_err(stash)?;
        }
        w.finish().map_err(stash)?;
        File::fit_dir(&mut self.dir, self.owner, &self.f);
        Ok(())
    }

    fn charge_to(&mut self, owner: Option<budget::Owner>) {
        self.owner = owner;
    }

    fn read_part(&self, part: u32, sink: &mut dyn FnMut(&[u8]) -> gravity_driver::Result<()>) -> gravity_driver::Result<()> {
        // a context per read: the third phase's workers read one file at once (a partition per token)
        let ctx = gravity_driver::mem::root::Root::new("GravitySpillRead");
        self.frozen()?;
        let Some(mut r) = self.f.read_part(ctx.mcx(), part).map_err(stash)? else { return Ok(()) };
        let mut buf = [0u8; 64 << 10];
        // D3902: the statement's cancel polled per 64 KiB read (a read under the cgroup's reclaim stalls)
        let mut step = cancelpoll::step("pool read_part: a part's 64 KiB reads");
        let streamed = (|| -> gravity_driver::Result<()> {
            loop {
                step.tick()?;
                let n = r.read(&mut buf).map_err(stash)?;
                if n == 0 {
                    return Ok(());
                }
                READ_BYTES[READ_PART].fetch_add(n as u64, Ordering::Relaxed);
                sink(&buf[..n])?;
            }
        })();
        // closed on EVERY path: a sink that stops the stream (an owner table that filled: merge.rs refolds or splits the partition) left
        // the BufFile open — `WARNING: resource was not closed: File n` per abandoned stream at the statement's end
        let closed = r.close().map_err(stash);
        streamed.and(closed)
    }

    fn part_bytes(&self, part: u32) -> u64 {
        self.f.part_len(part)
    }

    /// D2572: a sort run is compressed only under the session's `temp_file_limit` (fd's `FileWriteV` reads the same
    /// GUC on this thread, carried to a pool claim by D2351): its bytes then decide 53400; without one, raw blocks.
    fn compress_runs(&self) -> bool {
        gravity_hosts::worker_arm::temp_file_limited()
    }

    /// D2572-d: under the limit, a run is still written raw while the statement's spilled bytes with it stay far under it
    /// (`runcodec::raw_fits`): its bytes cannot decide a 53400 there, and the codec's CPU bought nothing.
    fn compress_run(&self, spilled: u64, bytes: u64) -> bool {
        gravity_hosts::worker_arm::temp_file_limit_bytes().is_some_and(|limit| !gravity_driver::mem::runcodec::raw_fits(spilled, bytes, limit))
    }

    fn open_part(&self, part: u32) -> gravity_driver::Result<Option<Box<dyn gravity_driver::RunReader>>> {
        self.frozen()?;
        Ok(Reader::open(&self.f, part)?.map(|r| Box::new(r) as Box<dyn gravity_driver::RunReader>)) // alloc: Exec
    }

    /// D3190: a put sort run read from a fence (`mem::fence`): the BufFile seeked to the byte, none read before it.
    fn open_part_at(&self, part: u32, offset: u64) -> gravity_driver::Result<Option<Box<dyn gravity_driver::RunReader>>> {
        self.frozen()?;
        Ok(Reader::open_at(&self.f, part, offset)?.map(|r| Box::new(r) as Box<dyn gravity_driver::RunReader>)) // alloc: Exec
    }

    fn spilled_bytes(&self) -> u64 {
        self.f.spilled_bytes()
    }

    fn delete(self: Box<Self>) -> gravity_driver::Result<()> {
        let File { f, ctx, .. } = *self;
        let r = f.delete().map_err(stash);
        drop(ctx);
        r
    }
}

/// A pull reader over one run: `spillset`'s part reader with its own context (declared first:
/// dropped first); closed on drop, else the resource owner warns of a leaked temp file.
struct Reader {
    r: Option<spillset::PartReader<'static>>,
    _ctx: Root,
}

impl Drop for Reader {
    fn drop(&mut self) {
        if let Some(r) = self.r.take() {
            let _ = r.close();
        }
    }
}

impl gravity_driver::RunReader for Reader {
    /// D3902: a long buffer filled `IO_CHUNK` bytes a read, the statement's cancel polled before each (a kernel's reload
    /// read of a spilled partition took up to 2.6 s whole under the cgroup's reclaim); fewer bytes only at the run's end.
    /// D3967: before the first one too: a reload loop of reads that each fit one chunk polled nowhere (1.3 s).
    fn read(&mut self, buf: &mut [u8]) -> gravity_driver::Result<usize> {
        let Some(r) = self.r.as_mut() else { return Ok(0) };
        read_polled(r, buf)
    }
}

/// D4215: a run's reader as `read_polled` reads it (`read_extents`: `spillset::PartReader::read_each`).
trait RunPieces {
    fn read_extents(&mut self, buf: &mut [u8], each: &mut dyn FnMut() -> bool) -> gravity_driver::Result<(usize, bool)>;
}

impl RunPieces for spillset::PartReader<'_> {
    fn read_extents(&mut self, buf: &mut [u8], each: &mut dyn FnMut() -> bool) -> gravity_driver::Result<(usize, bool)> {
        spillset::PartReader::read_each(self, buf, each).map_err(stash)
    }
}

/// `RunReader::read`'s body: `buf` filled at most `IO_CHUNK` bytes a read, the statement's cancel polled before each and,
/// D4215, before each extent's pread inside it: a run's extents lie apart (a spilled join's, of at most 64 KiB, between the
/// other partitions'), so each is its own device wait, and a chunk of them unpolled held a cancel up to 7.7 s.
fn read_polled(r: &mut dyn RunPieces, buf: &mut [u8]) -> gravity_driver::Result<usize> {
    let mut got = 0;
    let mut step = cancelpoll::step("pool read: a run's reads of IO_CHUNK bytes");
    while got < buf.len() {
        step.tick()?;
        let end = buf.len().min(got + IO_CHUNK);
        let mut canceled = None;
        let (n, _) = r.read_extents(&mut buf[got..end], &mut || match step.tick() {
            Ok(()) => true,
            Err(c) => {
                canceled = Some(c);
                false
            }
        })?;
        READ_BYTES[READ_RUN].fetch_add(n as u64, Ordering::Relaxed);
        if let Some(c) = canceled {
            return Err(c.into());
        }
        if n == 0 {
            break;
        }
        got += n;
    }
    Ok(got)
}

/// D3902: `gravity_program::cost::runtime::SPILL_IO_CHUNK`, the most one spill write or read moves between cancel polls.
use gravity_program::cost::runtime::SPILL_IO_CHUNK as IO_CHUNK;

pub static SPILL_SETS: SpillSets = SpillSets;

/// Install the pool's spill files as the process's `SpillFactory` (once, beside the pool), and D3203's fan-out that deletes a
/// statement's spill files across the pool at its end (`gravity_driver::END_FAN_OUT`).
pub fn install_spills() -> bool {
    let _ = gravity_driver::END_FAN_OUT.set(end_fan_out);
    gravity_driver::install_spill_factory(&SPILL_SETS)
}

/// D3203 (`gravity_driver::FanOut`): `job(i)` for each `i < n` on at most `width` of the statement's pool's workers, an
/// index a claim, the leader waiting; the pool's workers are armed for temp files (their spill writes), so a spill file's
/// unlink runs on any of them. False (nothing ran) without a pool of two threads or more; the caller runs what no claim took.
fn end_fan_out(width: usize, n: usize, job: &(dyn Fn(usize) + Sync)) -> bool {
    let Some(pool) = gravity_parallel::installed_pool() else { return false };
    let width = (pool.threads() as usize).min(width).min(n).min(u16::MAX as usize) as u16;
    if width < 2 {
        return false;
    }
    let claims = gravity_parallel::PartitionRange::new(n as u64, 1);
    let each = |_: u16, c: MorselRange| -> gravity_driver::Result<()> {
        (c.lo..c.hi.min(n as u64)).for_each(|i| job(i as usize));
        Ok(())
    };
    pool.run(width, &claims, &each).is_ok()
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicU8, Ordering};

    use super::{read_polled, RunPieces, IO_CHUNK};

    /// A run of `extents` (their lengths) whose every extent read is one device wait (a pread); `cancel_at` = the pread during
    /// which the statement's cancel arrives (its abort bit set, as pg_cancel_backend sets it while the thread waits).
    struct Slow<'a> {
        extents: Vec<usize>,
        cur: usize,
        pos: usize,
        preads: usize,
        cancel_at: usize,
        byte: &'a AtomicU8,
    }

    impl Slow<'_> {
        /// One pread: the current extent's bytes past the place, at most `buf.len()`; 0 at the run's end.
        fn pread(&mut self, buf: &mut [u8]) -> usize {
            let n = (self.extents[self.cur] - self.pos).min(buf.len());
            for (i, b) in buf[..n].iter_mut().enumerate() {
                *b = (self.cur * 31 + self.pos + i) as u8;
            }
            self.pos += n;
            self.preads += 1;
            if self.preads == self.cancel_at {
                self.byte.fetch_or(cancelpoll::ABORT_BIT, Ordering::Release);
            }
            n
        }
    }

    impl RunPieces for Slow<'_> {
        /// As spillset's `read_each`: a piece per extent, `each` asked before every piece past the first.
        fn read_extents(&mut self, buf: &mut [u8], each: &mut dyn FnMut() -> bool) -> gravity_driver::Result<(usize, bool)> {
            let mut got = 0;
            while got < buf.len() {
                if self.cur == self.extents.len() {
                    break;
                }
                if self.pos == self.extents[self.cur] {
                    (self.cur, self.pos) = (self.cur + 1, 0);
                    continue;
                }
                if got != 0 && !each() {
                    return Ok((got, true));
                }
                got += self.pread(&mut buf[got..]);
            }
            Ok((got, false))
        }
    }

    // D4215 witness: a spilled join's partition lies in extents of at most 64 KiB (the stage's) between the other
    // partitions', so one IO_CHUNK read is ~16 preads at scattered offsets. Under a busy device each waited ~40 ms and the
    // read ~0.66 s at p50, up to 7.7 s, with no poll between them: a cancel that came during the first waited for all.
    #[test]
    fn a_cancel_during_a_runs_first_pread_stops_the_read_before_its_next_pread() {
        let byte = AtomicU8::new(0);
        let mut r = Slow { extents: vec![40 << 10; 16], cur: 0, pos: 0, preads: 0, cancel_at: 1, byte: &byte }; // alloc: Cold
        let mut buf = vec![0u8; IO_CHUNK]; // alloc: Cold
        let got = cancelpoll::scoped(&byte, || read_polled(&mut r, &mut buf));
        assert!(got.is_err(), "the read stops for the cancel");
        assert_eq!(r.preads, 1, "no pread after the one the cancel came during");
    }

    #[test]
    fn a_run_read_through_the_polled_reads_gives_its_bytes() {
        let byte = AtomicU8::new(0);
        let extents = vec![40 << 10, 100, IO_CHUNK + 7, 64 << 10, 1]; // alloc: Cold
        let mut r = Slow { extents: extents.clone(), cur: 0, pos: 0, preads: 0, cancel_at: usize::MAX, byte: &byte };
        let mut got = Vec::new(); // alloc: Cold
        let mut buf = vec![0u8; 3 * IO_CHUNK]; // alloc: Cold
        loop {
            let n = cancelpoll::scoped(&byte, || read_polled(&mut r, &mut buf)).expect("no cancel");
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        let want: Vec<u8> = extents.iter().enumerate().flat_map(|(e, &len)| (0..len).map(move |i| (e * 31 + i) as u8)).collect();
        assert_eq!(got.len(), want.len());
        assert!(got == want, "the run's bytes in order");
    }

    /// D4945: the temp-file road is claimed exactly where a temp file maps. On wasm32 none maps, and a claimed road failed
    /// every spool past W and G with out of memory.
    #[test]
    fn the_temp_file_road_is_claimed_only_where_a_temp_file_maps() {
        use gravity_driver::SpillFactory;
        use std::os::fd::AsRawFd;
        let path = std::env::temp_dir().join(format!("gravity-pool-maps-temp-{}", std::process::id())); // alloc: Cold
        let f = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path).expect("a temp file");
        f.set_len(1 << 16).expect("its length");
        let maps = gravity_driver::TempMap::shared(f.as_raw_fd(), 1 << 16).is_some();
        std::fs::remove_file(&path).expect("unlinked once mapped");
        assert_eq!(super::SPILL_SETS.maps_temp(), maps, "maps_temp says whether a temp file maps here");
    }
}
