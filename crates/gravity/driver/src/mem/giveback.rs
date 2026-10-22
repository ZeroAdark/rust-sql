//! H3 step (4) (i) + (ii) (`PGRUST_MEMPOLICY=give_back_spill`, default 1, acting only with `give_back_service` = 1;
//! GIVEBACK-SERVICE-DESIGN step (4), S1-S4): a region its owner gives back WHOLE goes to the give-back service
//! (memruntime's pg:memrelease) as one job when it is an allocator region. The sites: C1-a, a spill region given back
//! before the statement's end (`spill_host::v4::give`: ~79 GiB of a coremem stress take's query-thread decommits, all of
//! them its); C1-b, a statement's grown arena chunks past the parked ones at its end (`hostimpl::arena::settle_grow_chunks`).
//! - The enqueue (`give`, on the giving thread): the grant pool's part of the region's hold taken over (`Held::take_pool`,
//!   as `reap::defer`: the statement's W / G free at once, the pool holding the bytes until the give), and its charge moved
//!   whole to `Cache(Reaping)` (`RegionBuf::give_parts`: parked()'s `Charge::transfer`, its residency entry with it). The
//!   owner's account goes down by the charge's bytes, as the region's drop on this thread takes them today, and REAPING up
//!   by them (witness (a)).
//! - The give (`drain_upto`, on the service's thread, its own unbound binding, outside any enter): the region's drop, there
//!   (`GiveParts::give`): its pages given back, off the residency registry, its charge dropped (door 1: REAPING down, that
//!   thread up) and its block freed (door 1: that thread down, so it nets 0; witness (b)), then the pool's bytes released.
//! - (ii): on these paths the MADV_DONTNEED before a region's drop is skipped where the drop gives the pages back itself
//!   (S3): a mapped region of `reap::REAP_MIN` writable bytes or more (its drop hands it to the reaper, or unmaps it), and a
//!   reserved allocator region its job does not take (its drop's own give-back of `len + committed`). A mapped region
//!   under REAP_MIN keeps it.
//! - On the caller as before, each counted (`counts`, the accounts view's give_back_spill_* rows): past the watchdog's
//!   cancel line (S1: `mcx::ladder::past_cancel_line`, live; the relief must be now, and the ladder counts no landing
//!   there), with either knob off or no service in place, at the service's backlog cap (`allocarena::own::service_cap`,
//!   over this lane's bytes) or a full queue, with the service past its dead-man stretch while jobs wait
//!   (`allocarena::own::service_stalled`), and when the pool has no room for the hand-off (`take_pool`).
//! - A grant pool trim for a draw (`reap::drain` and kin, `drain_here`) never waits for the service (take-back TB3): what
//!   the lane holds is landing (the ladder's credit), a draw of a queued give's shape takes it back (T1), and the
//!   service gives its lanes back before it purges its arenas, so nothing on a draw's path queues behind a purge. Past
//!   the watchdog's cancel line (S1) the trim gives the jobs queued before it back on its own thread at once, pages
//!   first, as the service does (counted): the relief must be now.
//! - C1-c (`park`, from `RegionDepot::put`): a region about to park with bytes committed past its share in place gives
//!   them back on the service's thread, which then parks it (`ParkParts`; the depot insertion is the fence, and a take
//!   meanwhile misses, counted). Its account moves are made at the enqueue (E1-E3), so a session that unbinds while the
//!   job waits leaves nothing on its account. A depot trim never gives a park job back itself (the job owns the region);
//!   a draw's trim past the cancel line does (counted).
//! - Take-back (T1, `take_back_give`, from `RegionDepot::take_parked`): a plain region draw that finds no parked region
//!   of its shape takes the newest queued whole give of it back (`GiveParts::take_back`: a plain allocator region within an
//!   eighth over the page-rounded ask, so its fit gives no page back and grows into none), out of the ring under its lock
//!   (the service never gives it) and numbered done. Its pages were never given back: no decommit on either thread, no
//!   fault for the taker. Its charge goes from REAPING to the depot's holder and on to the taker, as a parked region's
//!   does; its pool hold is the region's again; the lane's landing drops by its bytes (counted). One the fit or the
//!   taker's grant refuses goes back to the lane as its owner gave it.
//!   A park job is never taken back (a reserved draw's miss maps fresh, and the job parks later): a reservation taken with
//!   its committed tail would reach its taker with `committed` != 0, and `grow::region_commit` commits from `len +
//!   committed` while the taker's table lies at the share (its growth no longer contiguous); taken with the tail's commit
//!   dropped and its pages kept, they are resident pages no hold charges, which `uncommit` never gives back (D3163's
//!   leak); taken with the tail given back on the taker, it is the query-thread decommit this lane exists to remove.
//!   Park jobs are a few per run against tens of thousands of whole gives.
//! - TB4 (`give_back_spill_overdraft`, default 0, `overdraft_room`): under the cancel line a grant pool take (a draw, a
//!   give's hand-off of its hold) the pool has no room for holds past the pool's limit by at most the pool bytes the
//!   queued jobs hold (and the service's backlog cap): no claim, no decommit, no wait; the overage is gone as the jobs
//!   land (counted). An admission never does (it has its own wait and halving).
//! The queue is two static rings, `QUEUE_MAX` whole gives and `PARK_MAX` park jobs, numbered in one order (no storage
//! grows on the enqueue path, A6). Their bytes are this lane's landing (`landing_bytes`): the REAPING-charged bytes
//! (`charged_landing_bytes`) and an allocator reservation's committed bytes, pending relief with no named charge. It is
//! in the service's one landing account and the ladder's landing credit (`reap::landing_credit`).

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Acquire, Ordering::Relaxed, Ordering::Release};

use mempolicy::consts::{get, K};
use pgsync::Mutex;

use crate::mem::region::{GiveParts, ParkParts, RegionBuf};

/// The most whole gives waiting (CHOSEN: the reaper's queue, `cost::runtime::REAP_QUEUE_MAX`); past it a give runs on its
/// caller.
const QUEUE_MAX: usize = gravity_program::cost::runtime::REAP_QUEUE_MAX;
/// The most park jobs waiting (`cost::runtime::PARK_QUEUE_MAX`); past it a put gives back on its caller as before.
const PARK_MAX: usize = gravity_program::cost::runtime::PARK_QUEUE_MAX;
/// The jobs taken and not yet given back that the ring tracks (`Ring::done`): the queues' and as many in flight.
const SEQS: usize = 2 * (QUEUE_MAX + PARK_MAX);

/// A fixed ring of numbered jobs, oldest first from `head`.
struct Fifo<T, const N: usize> {
    jobs: [Option<(u64, T)>; N],
    head: usize,
    len: usize,
}

impl<T, const N: usize> Fifo<T, N> {
    const fn new() -> Self {
        Fifo { jobs: [const { None }; N], head: 0, len: 0 }
    }

    fn push(&mut self, seq: u64, job: T) {
        let at = (self.head + self.len) % N;
        self.jobs[at] = Some((seq, job));
        self.len += 1;
    }

    fn front(&self) -> Option<u64> {
        (0..self.len).find_map(|k| self.jobs[(self.head + k) % N].as_ref().map(|j| j.0))
    }

    fn pop(&mut self) -> Option<(u64, T)> {
        while self.len != 0 {
            let at = self.head;
            self.head = (at + 1) % N;
            self.len -= 1;
            if let Some(j) = self.jobs[at].take() {
                return Some(j);
            }
        }
        None
    }

    /// Take-back (T1): the newest job `pick` accepts, out of the ring. Its slot is left empty (`pop` and `front` skip it)
    /// unless it is at either end, which the ring's span leaves.
    fn remove_newest(&mut self, mut pick: impl FnMut(&T) -> bool) -> Option<(u64, T)> {
        let k = (0..self.len).rev().find(|&k| self.jobs[(self.head + k) % N].as_ref().is_some_and(|j| pick(&j.1)))?;
        let j = self.jobs[(self.head + k) % N].take();
        while self.len != 0 && self.jobs[(self.head + self.len - 1) % N].is_none() {
            self.len -= 1;
        }
        while self.len != 0 && self.jobs[self.head].is_none() {
            self.head = (self.head + 1) % N;
            self.len -= 1;
        }
        j
    }
}

/// The waiting jobs of both kinds. Jobs are numbered in enqueue order (`next`); `low` is the oldest not yet given back
/// (`done` marks the ones given back past it, by `seq % SEQS`); `parks_held` are the park slots a put holds while it
/// makes its job.
struct Ring {
    gives: Fifo<GiveParts, QUEUE_MAX>,
    parks: Fifo<ParkParts, PARK_MAX>,
    parks_held: usize,
    next: u64,
    low: u64,
    done: [bool; SEQS],
}

impl Ring {
    /// Jobs numbered and not yet given back, slots held included.
    fn outstanding(&self) -> u64 {
        self.next - self.low + self.parks_held as u64
    }
}

static QUEUE: Mutex<Ring> = Mutex::new(Ring { gives: Fifo::new(), parks: Fifo::new(), parks_held: 0, next: 0, low: 0, done: [false; SEQS] });

/// A job of either kind, as taken off the rings.
enum Job {
    Give(GiveParts),
    Park(ParkParts),
}

impl Job {
    /// Its REAPING-charged bytes and its pending (uncharged) ones.
    fn bytes(&self) -> (usize, usize) {
        match self {
            Job::Give(g) => (g.bytes(), 0),
            Job::Park(p) => p.bytes(),
        }
    }

    /// TB4: the grant pool's bytes it holds.
    fn pool_bytes(&self) -> u64 {
        match self {
            Job::Give(g) => g.pool_bytes(),
            Job::Park(p) => p.pool_bytes(),
        }
    }

    /// Given back on this thread: a whole give's drop path; a park job's page work, then the depot's insertion.
    fn give(self) {
        match self {
            Job::Give(g) => g.give(),
            Job::Park(p) => crate::park::REGIONS.insert(p.give()),
        }
    }
}

/// The lane's REAPING-charged bytes enqueued and not yet given back (each job's charge at its enqueue), and its pending
/// ones (an allocator reservation's committed bytes in a park job).
static LANDING: AtomicUsize = AtomicUsize::new(0);
static PENDING: AtomicUsize = AtomicUsize::new(0);
/// TB4: the grant pool's bytes the queued jobs hold (each job's at its enqueue, off before its give or a take-back), the
/// room the pool gets back as they land.
static LANDING_POOL: AtomicU64 = AtomicU64::new(0);
/// Park jobs enqueued and not yet given back (a take that misses meanwhile is counted).
static PARKS_OUT: AtomicUsize = AtomicUsize::new(0);
/// The lane registered with the service (`allocarena::own::set_job_lane`, before the first job is counted).
static REGISTERED: AtomicBool = AtomicBool::new(false);

/// Jobs enqueued and their bytes; jobs given back by the service's drain and their bytes.
static JOBS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static GIVEN: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// S1: gives run on their caller past the cancel line.
static PAST_LINE: AtomicU64 = AtomicU64::new(0);
/// Gives the admission turned away, run on their caller: at the cap or a full queue, past the service's dead-man
/// stretch, the pool with no room for the hand-off.
static TURNED_CAP: AtomicU64 = AtomicU64::new(0);
static TURNED_DEAD_MAN: AtomicU64 = AtomicU64::new(0);
static TURNED_NO_POOL: AtomicU64 = AtomicU64::new(0);
/// A trim's gives on its own thread past the cancel line (`drain_here`): the trims that gave something, and their bytes.
static HERE_PAST: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// (ii): inline give-backs skipped: a mapped region of REAP_MIN or more (its drop's reaper hand-off or unmap), a reserved
/// allocator region its job did not take (its drop's own give-back); and S3's kept one: a mapped region under REAP_MIN.
static SKIPPED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static SMALL_MAPPED: AtomicU64 = AtomicU64::new(0);
/// C1-c: park jobs enqueued and their bytes; those the service gave back and parked, and their bytes; puts past the cancel
/// line (on their caller as before); takes that missed while park jobs waited.
static PARKS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static PARKED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static PARK_PAST_LINE: AtomicU64 = AtomicU64::new(0);
static PARK_MISSES: AtomicU64 = AtomicU64::new(0);
/// C1-c: park jobs a draw's trim gave back on its own thread past the cancel line, and their bytes.
static PARK_HERE: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// Take-back (T1 + T2): whole gives a region draw of their shape took back off the lane, and their bytes.
static TAKEN_BACK: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// Take-back TB4 (give_back_spill_overdraft): grant pool takes that held past the pool's limit on the lane's landing, the
/// bytes past the limit they held, and the largest such overage.
static OVERDRAFT: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static OVERDRAFT_MAX: AtomicU64 = AtomicU64::new(0);
/// A test's probe of a give between its pages' give-back and its pool bytes' release (`set_give_probe`, once).
static GIVE_PROBE: pgsync::OnceLock<fn(usize, usize)> = pgsync::OnceLock::new();

/// Where a whole give of a region goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    /// As before: its pages given back here, then the region dropped here.
    Here,
    /// (ii): the drop alone, which gives the pages back itself (a mapped region of REAP_MIN or more).
    Drop,
    /// A job for the service (an allocator region).
    Job,
}

/// The route of a give: `on` (both knobs and the service in place), `past` (past the watchdog's cancel line), `rw` the
/// region's writable bytes (0: an allocator region).
fn route(on: bool, past: bool, rw: usize) -> Route {
    if !on || past {
        return Route::Here;
    }
    if rw == 0 {
        return Route::Job;
    }
    if rw >= crate::mem::reap::REAP_MIN { Route::Drop } else { Route::Here }
}

/// Whether the lane acts: give_back_spill and give_back_service on, and the service in place (its start made).
fn on() -> bool {
    get(K::GiveBackSpill) != 0 && get(K::GiveBackService) != 0 && allocarena::own::service_on()
}

/// C1-a / C1-b: region `c` given back whole by its owner now (its contents dead, no reader holding it).
pub(crate) fn give(c: RegionBuf) {
    // pagepool P3: a pool region's pages go back to the pool warm with its block at its drop (`RegionBuf::dontneed_all`:
    // never a `pagegive` call on pool memory, the purger's release), so never a job and no page work here
    if c.is_pool() {
        drop(c);
        return;
    }
    let on = on();
    give_as(c, on, on && mcx::ladder::past_cancel_line(), allocarena::own::service_cap());
}

/// `give` with the knobs' and the line's reads given (`cap`: the service's backlog cap).
fn give_as(c: RegionBuf, on: bool, past: bool, cap: usize) {
    match route(on, past, c.writable()) {
        Route::Here => {
            if on && past {
                PAST_LINE.fetch_add(1, Relaxed);
            } else if on {
                SMALL_MAPPED.fetch_add(1, Relaxed);
            }
            here(c);
        }
        Route::Drop => {
            SKIPPED[0].fetch_add(1, Relaxed);
            drop(c);
        }
        Route::Job => {
            if let Err(c) = enqueue(c, cap) {
                if c.is_reserved() {
                    SKIPPED[1].fetch_add(1, Relaxed);
                    drop(c);
                } else {
                    here(c);
                }
            }
        }
    }
}

/// As before: the region's pages given back on this thread, then the region dropped.
fn here(mut c: RegionBuf) {
    // pagepool P3: `dontneed` over its `len` (a pool region's pages go back to the pool at its drop instead)
    c.dontneed_all();
    drop(c);
}

/// Region `c` (an allocator region) onto the queue, or back to the caller (`Err`) when the admission turns it away
/// (counted): the lane at `cap` with it, the service past its dead-man stretch while jobs wait, or no pool room for the
/// hand-off. A queue found full after the hand-off gives the job back here (counted at the cap).
fn enqueue(mut c: RegionBuf, cap: usize) -> Result<(), RegionBuf> {
    let waiting = landing_bytes();
    if waiting.saturating_add(c.charged_bytes() as usize) > cap {
        TURNED_CAP.fetch_add(1, Relaxed);
        return Err(c);
    }
    if waiting != 0 && allocarena::own::service_stalled() {
        TURNED_DEAD_MAN.fetch_add(1, Relaxed);
        return Err(c);
    }
    let Some(job) = c.give_parts() else {
        TURNED_NO_POOL.fetch_add(1, Relaxed);
        return Err(c);
    };
    drop(c);
    register();
    let (b, pool) = (job.bytes(), job.pool_bytes());
    let before = landing_bytes();
    LANDING.fetch_add(b, Relaxed);
    LANDING_POOL.fetch_add(pool, Relaxed);
    let full = {
        let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        if q.gives.len == QUEUE_MAX || q.outstanding() >= SEQS as u64 {
            Some(job)
        } else {
            let seq = q.next;
            q.next += 1;
            q.gives.push(seq, job);
            None
        }
    };
    if let Some(job) = full {
        LANDING.fetch_sub(b, Relaxed);
        LANDING_POOL.fetch_sub(pool, Relaxed);
        TURNED_CAP.fetch_add(1, Relaxed);
        job.give();
        return Ok(());
    }
    JOBS[0].fetch_add(1, Relaxed);
    JOBS[1].fetch_add(b as u64, Relaxed);
    if before == 0 {
        allocarena::own::wake_service();
    }
    Ok(())
}

/// The lane registered with the service, before its first job is counted.
fn register() {
    if !REGISTERED.load(Acquire) {
        allocarena::own::set_job_lane(landing_bytes, drain_upto);
        REGISTERED.store(true, Release);
    }
}

/// C1-c (`RegionDepot::put`): region `c`, about to park, onto the park ring when it has bytes committed past its share to
/// give back (`RegionBuf::into_park`, its account moves made here); the job parks it after the give. `Err` (unchanged):
/// the caller parks it as before: nothing to give back, a knob off or no service, past the watchdog's cancel line
/// (counted), at the service's backlog cap or a full ring, or the service past its dead-man stretch while jobs wait
/// (counted as a give's).
pub(crate) fn park(c: RegionBuf) -> Result<(), RegionBuf> {
    // pagepool P3: a pool region parks on its caller (`uncommit`'s pool_dead), before any admission count, as `give` drops
    // one before `give_as`: no give_back_spill_* row moves for it
    if c.committed() == 0 || c.is_laid() || c.is_pool() || !on() {
        return Err(c);
    }
    if mcx::ladder::past_cancel_line() {
        PARK_PAST_LINE.fetch_add(1, Relaxed);
        return Err(c);
    }
    let waiting = landing_bytes();
    if waiting.saturating_add(c.committed()) > allocarena::own::service_cap() {
        TURNED_CAP.fetch_add(1, Relaxed);
        return Err(c);
    }
    if waiting != 0 && allocarena::own::service_stalled() {
        TURNED_DEAD_MAN.fetch_add(1, Relaxed);
        return Err(c);
    }
    {
        let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        if q.parks.len + q.parks_held >= PARK_MAX || q.outstanding() >= SEQS as u64 {
            drop(q);
            TURNED_CAP.fetch_add(1, Relaxed);
            return Err(c);
        }
        q.parks_held += 1;
    }
    let job = match c.into_park() {
        Ok(j) => j,
        Err(c) => {
            QUEUE.lock().unwrap_or_else(|e| e.into_inner()).parks_held -= 1;
            return Err(c);
        }
    };
    register();
    let (charged, pending) = job.bytes();
    let before = landing_bytes();
    LANDING.fetch_add(charged, Relaxed);
    PENDING.fetch_add(pending, Relaxed);
    LANDING_POOL.fetch_add(job.pool_bytes(), Relaxed);
    PARKS_OUT.fetch_add(1, Relaxed);
    {
        let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        q.parks_held -= 1;
        let seq = q.next;
        q.next += 1;
        q.parks.push(seq, job);
    }
    PARKS[0].fetch_add(1, Relaxed);
    PARKS[1].fetch_add((charged + pending) as u64, Relaxed);
    if before == 0 {
        allocarena::own::wake_service();
    }
    Ok(())
}

/// Take-back (T1 + T2): a plain region draw of a page-rounded `want` that found no parked region takes a queued whole give
/// of its shape back instead of a fresh allocation: the newest plain region at least `want` long and within an eighth
/// over it (`RegionDepot::take_parked`'s fit, with no growth, so fitting it gives no page back), out of the ring under
/// its lock (the service never pops it), numbered done, off the lane's landing (counted). Its pages were never given
/// back: no decommit on either thread and no fault for the taker. With the job's bytes, which the taker counts once its
/// fit holds (`note_taken_back`).
pub(crate) fn take_back_give(want: usize) -> Option<(RegionBuf, usize)> {
    if LANDING.load(Relaxed) == 0 {
        return None;
    }
    let fits = |g: &GiveParts| g.shape().is_some_and(|(len, whole)| len >= want && whole - want <= want / 8);
    let (seq, job) = QUEUE.lock().unwrap_or_else(|e| e.into_inner()).gives.remove_newest(fits)?;
    let b = job.bytes();
    LANDING.fetch_sub(b, Relaxed);
    LANDING_POOL.fetch_sub(job.pool_bytes(), Relaxed);
    complete(seq);
    Some((job.take_back(), b))
}

/// A take-back of `b` bytes its taker kept (fitted and charged), counted.
pub(crate) fn note_taken_back(b: usize) {
    TAKEN_BACK[0].fetch_add(1, Relaxed);
    TAKEN_BACK[1].fetch_add(b as u64, Relaxed);
}

/// TB4's witness: region `c` given back whole by its owner (`give`), as a spill region's or a grown chunk's give is.
#[doc(hidden)]
pub fn give_witness(c: RegionBuf) {
    give(c);
}

/// TB4 (give_back_spill_overdraft): the bytes a grant pool take may hold past the pool's limit now: the pool bytes the
/// lane's queued jobs hold, which come back as they land, at most the service's backlog cap. 0 with the knob off, the
/// lane not acting, nothing landing, or past the watchdog's cancel line (S1: the relief must be now; the draw's trim
/// gives the jobs back on its thread, `drain_here`).
pub(crate) fn overdraft_room() -> u64 {
    if get(K::GiveBackSpillOverdraft) == 0 {
        return 0;
    }
    let landing = LANDING_POOL.load(Relaxed);
    if landing == 0 || !on() || mcx::ladder::past_cancel_line() {
        return 0;
    }
    landing.min(allocarena::own::service_cap() as u64)
}

/// TB4: a take that held `own` of its bytes past the pool's limit on the lane's landing, the pool then `over` past it.
pub(crate) fn note_overdraft(own: u64, over: u64) {
    OVERDRAFT[0].fetch_add(1, Relaxed);
    OVERDRAFT[1].fetch_add(own, Relaxed);
    OVERDRAFT_MAX.fetch_max(over, Relaxed);
}

/// C1-c: a depot take that found nothing, counted when park jobs wait (their regions are not in the depot yet).
pub(crate) fn note_take_miss() {
    if PARKS_OUT.load(Relaxed) != 0 {
        PARK_MISSES.fetch_add(1, Relaxed);
    }
}

/// The oldest waiting job numbered under `until` (`parks`: of either kind, else a whole give's), with its number.
fn pop(until: u64, parks: bool) -> Option<(u64, Job)> {
    let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    let g = q.gives.front().filter(|s| *s < until);
    let p = q.parks.front().filter(|s| parks && *s < until);
    match (g, p) {
        (Some(a), Some(b)) if b < a => q.parks.pop().map(|(s, j)| (s, Job::Park(j))),
        (Some(_), _) => q.gives.pop().map(|(s, j)| (s, Job::Give(j))),
        (None, Some(_)) => q.parks.pop().map(|(s, j)| (s, Job::Park(j))),
        (None, None) => None,
    }
}

/// Job `seq` given back: the oldest not given back moves past every one given back.
fn complete(seq: u64) {
    let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    q.done[(seq % SEQS as u64) as usize] = true;
    while q.low < q.next && q.done[(q.low % SEQS as u64) as usize] {
        let i = (q.low % SEQS as u64) as usize;
        q.done[i] = false;
        q.low += 1;
    }
}

/// The lane's bytes enqueued and not yet given back, charged and pending (its part of the service's landing account).
pub fn landing_bytes() -> usize {
    LANDING.load(Relaxed) + PENDING.load(Relaxed)
}

/// The lane's REAPING-charged bytes enqueued and not yet given back (A7: REAPING less its base is the own runs' landing
/// plus these).
pub fn charged_landing_bytes() -> usize {
    LANDING.load(Relaxed)
}

/// The service's batch over this lane (`allocarena::own::drain_jobs_upto`): the oldest jobs of either kind given back on
/// the calling thread until `max` bytes are (a job never split). The bytes.
pub fn drain_upto(max: usize) -> usize {
    give_until(max, u64::MAX, true, true).0
}

/// The oldest jobs numbered under `until` given back on this thread (pages first, their pool bytes last) until `max`
/// bytes are: of both kinds for the service (`service`, counted as its gives) and for a draw's trim (`parks`), else whole
/// gives only. The bytes and the jobs.
fn give_until(max: usize, until: u64, parks: bool, service: bool) -> (usize, u64) {
    let (mut n, mut jobs) = (0, 0);
    while n < max {
        let Some((seq, j)) = pop(until, parks || service) else { break };
        let ((charged, pending), park) = (j.bytes(), matches!(j, Job::Park(_)));
        // TB4: off the overdraft's room before its pool bytes are released (never counted twice)
        LANDING_POOL.fetch_sub(j.pool_bytes(), Relaxed);
        j.give();
        LANDING.fetch_sub(charged, Relaxed);
        PENDING.fetch_sub(pending, Relaxed);
        if park {
            PARKS_OUT.fetch_sub(1, Relaxed);
        }
        let c = match (service, park) {
            (true, true) => Some(&PARKED),
            (true, false) => Some(&GIVEN),
            (false, true) => Some(&PARK_HERE),
            (false, false) => None,
        };
        if let Some(c) = c {
            c[0].fetch_add(1, Relaxed);
            c[1].fetch_add((charged + pending) as u64, Relaxed);
        }
        complete(seq);
        n += charged + pending;
        jobs += 1;
    }
    (n, jobs)
}

/// The jobs queued before now (`parks`: park jobs too) given back on this thread, counted on `c` (the trims that gave
/// something, their bytes).
fn give_here(c: &[AtomicU64; 2], until: u64, parks: bool) {
    let (n, jobs) = give_until(usize::MAX, until, parks, false);
    if jobs != 0 {
        c[0].fetch_add(1, Relaxed);
        c[1].fetch_add(n as u64, Relaxed);
    }
}

/// A grant pool trim for a draw (`reap::drain` and kin, `RegionDepot::trim`) before it reads the pool's room: no wait for
/// the service (take-back TB3), whose landing the ladder credits; the service woken for what waits. Past the watchdog's
/// cancel line (S1) the jobs queued before it given back on this thread at once, pages first (counted; `parks`: park
/// jobs too, false for the depot's own trim, a park job owning its region until it parks).
pub(crate) fn drain_here(parks: bool) {
    // D4404: the memory ladder's pass gives nothing back here; what the lane still holds is its landing credit
    if landing_bytes() == 0 || memledger::ladder_pass::in_pass() {
        return;
    }
    if mcx::ladder::past_cancel_line() {
        let until = QUEUE.lock().unwrap_or_else(|e| e.into_inner()).next;
        give_here(&HERE_PAST, until, parks);
        return;
    }
    allocarena::own::wake_service();
}

/// A test's probe, called on a give's thread after its pages are given back and before its pool bytes are released, with
/// the block's base and the bytes given (the pool's room never rises before the region's pages are given).
#[doc(hidden)]
pub fn set_give_probe(f: fn(usize, usize)) {
    let _ = GIVE_PROBE.set(f);
}

/// `GiveParts::give`'s call of the probe, if any.
pub(crate) fn probe(at: usize, give: usize) {
    if let Some(f) = GIVE_PROBE.get() {
        f(at, give);
    }
}

/// The lane's counters (the accounts view's give_back_spill_* rows).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// REAPING-charged bytes enqueued and not yet given back, and pending ones (an allocator reservation's park job).
    pub landing: u64,
    pub pending: u64,
    pub jobs: u64,
    pub job_bytes: u64,
    pub given: u64,
    pub given_bytes: u64,
    /// S1: gives run on their caller past the cancel line.
    pub inline_past_cancel: u64,
    pub cap_fallbacks: u64,
    pub dead_man_fallbacks: u64,
    pub pool_fallbacks: u64,
    /// A trim's gives on its own thread past the cancel line, pages first, and their bytes.
    pub drained_here_past_cancel: u64,
    pub drained_here_past_cancel_bytes: u64,
    /// (ii): inline give-backs skipped for a mapped region of REAP_MIN or more, and for a reserved allocator region's own
    /// drop; S3: kept for a mapped region under REAP_MIN.
    pub to_reaper: u64,
    pub reserved_own_drop: u64,
    pub small_mapped_inline: u64,
    /// C1-c: park jobs and their bytes; those the service gave back and parked, and their bytes; puts past the cancel
    /// line; takes that missed while park jobs waited; tails left writable (the kernel refused their PROT_NONE).
    pub park_jobs: u64,
    pub park_job_bytes: u64,
    pub parked: u64,
    pub parked_bytes: u64,
    pub park_inline_past_cancel: u64,
    pub park_take_misses: u64,
    pub park_protect_refused: u64,
    /// Park jobs a draw's trim gave back on its own thread past the cancel line, and their bytes.
    pub park_drained_here: u64,
    pub park_drained_here_bytes: u64,
    /// Take-back: whole gives a region draw of their shape took back off the lane, and their bytes.
    pub taken_back: u64,
    pub taken_back_bytes: u64,
    /// TB4: grant pool takes that held past the pool's limit on the lane's landing, their bytes past it, the largest.
    pub overdraft: u64,
    pub overdraft_bytes: u64,
    pub overdraft_max: u64,
}

pub fn counts() -> Counts {
    let l = |a: &AtomicU64| a.load(Relaxed);
    Counts {
        landing: LANDING.load(Relaxed) as u64,
        pending: PENDING.load(Relaxed) as u64,
        jobs: l(&JOBS[0]),
        job_bytes: l(&JOBS[1]),
        given: l(&GIVEN[0]),
        given_bytes: l(&GIVEN[1]),
        inline_past_cancel: l(&PAST_LINE),
        cap_fallbacks: l(&TURNED_CAP),
        dead_man_fallbacks: l(&TURNED_DEAD_MAN),
        pool_fallbacks: l(&TURNED_NO_POOL),
        drained_here_past_cancel: l(&HERE_PAST[0]),
        drained_here_past_cancel_bytes: l(&HERE_PAST[1]),
        to_reaper: l(&SKIPPED[0]),
        reserved_own_drop: l(&SKIPPED[1]),
        small_mapped_inline: l(&SMALL_MAPPED),
        park_jobs: l(&PARKS[0]),
        park_job_bytes: l(&PARKS[1]),
        parked: l(&PARKED[0]),
        parked_bytes: l(&PARKED[1]),
        park_inline_past_cancel: l(&PARK_PAST_LINE),
        park_take_misses: l(&PARK_MISSES),
        park_protect_refused: l(&crate::mem::region::PARK_PROTECT_REFUSED),
        park_drained_here: l(&PARK_HERE[0]),
        park_drained_here_bytes: l(&PARK_HERE[1]),
        taken_back: l(&TAKEN_BACK[0]),
        taken_back_bytes: l(&TAKEN_BACK[1]),
        overdraft: l(&OVERDRAFT[0]),
        overdraft_bytes: l(&OVERDRAFT[1]),
        overdraft_max: l(&OVERDRAFT_MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1 << 20;

    /// S1 and S3: the route on both sides of the cancel line and of REAP_MIN.
    #[test]
    fn s1_s3_a_give_routes_by_the_line_the_knobs_and_reap_min() {
        let min = crate::mem::reap::REAP_MIN;
        // knobs off (or no service): as before, whatever the region
        for rw in [0, min - 4096, min, 64 * MIB] {
            assert_eq!(route(false, false, rw), Route::Here, "off: rw {rw}");
            assert_eq!(route(false, true, rw), Route::Here, "off past the line: rw {rw}");
        }
        // S1: past the cancel line, on the caller as before
        for rw in [0, min - 4096, min, 64 * MIB] {
            assert_eq!(route(true, true, rw), Route::Here, "past the line: rw {rw}");
        }
        // under the line: an allocator region is a job
        assert_eq!(route(true, false, 0), Route::Job);
        // S3: a mapped region keeps its inline give-back under REAP_MIN and skips it from REAP_MIN up (its drop's reaper)
        assert_eq!(route(true, false, 4096), Route::Here);
        assert_eq!(route(true, false, min - 4096), Route::Here, "one page under REAP_MIN: kept");
        assert_eq!(route(true, false, min), Route::Drop, "at REAP_MIN: the reaper's");
        assert_eq!(route(true, false, 64 * MIB), Route::Drop);
    }

    /// S3 on real mapped reservations, one each side of REAP_MIN, knobs read as on and under the line: the one under it
    /// keeps the inline give-back (counted), the one at it skips it (counted) and its drop hands it to the reaper.
    #[cfg(target_os = "linux")]
    #[test]
    fn s3_a_mapped_region_skips_its_inline_give_back_only_from_reap_min() {
        let min = crate::mem::reap::REAP_MIN;
        let (c0, h0) = (counts(), crate::mem::reap::HANDED.load(Relaxed));
        // reservations of sizes no other test parks (a parked one of another writable extent would be taken first)
        let small = crate::mem::region::reserved_region(64 << 10, 74 * MIB, None, None, true).expect("a small reservation");
        assert!(small.writable() != 0 && small.writable() < min, "mapped, writable {} under REAP_MIN", small.writable());
        give_as(small, true, false, 512 * MIB);
        let c1 = counts();
        assert_eq!((c1.small_mapped_inline - c0.small_mapped_inline, c1.to_reaper - c0.to_reaper), (1, 0), "under REAP_MIN: kept");
        let large = crate::mem::region::reserved_region(min, 78 * MIB, None, None, true).expect("a large reservation");
        assert!(large.writable() >= min, "mapped, writable {} at REAP_MIN", large.writable());
        give_as(large, true, false, 512 * MIB);
        let c2 = counts();
        assert_eq!((c2.small_mapped_inline - c1.small_mapped_inline, c2.to_reaper - c1.to_reaper), (0, 1), "at REAP_MIN: skipped");
        assert!(crate::mem::reap::HANDED.load(Relaxed) > h0, "its drop handed it to the reaper");
        assert_eq!(c2.jobs, c0.jobs, "a mapped region is never a job");
    }

    /// Take-back's ring: the newest job a pick accepts leaves the ring; one from the middle leaves an empty slot that
    /// `front` and `pop` skip, one at either end shrinks the span, and the oldest-first order of the rest holds across
    /// the wrap.
    #[test]
    fn take_back_removes_the_newest_pick_and_the_ring_skips_its_slot() {
        let mut f: Fifo<u32, 4> = Fifo::new();
        // wrap the head: two in, two out
        f.push(0, 0);
        f.push(1, 1);
        assert_eq!(f.pop(), Some((0, 0)));
        assert_eq!(f.pop(), Some((1, 1)));
        for s in 2..6 {
            f.push(s, s as u32 * 10);
        }
        // the newest the pick accepts, from the middle: an empty slot
        assert_eq!(f.remove_newest(|j| *j < 40), Some((3, 30)));
        assert_eq!(f.len, 4, "a middle slot stays in the span");
        assert_eq!(f.remove_newest(|j| *j == 99), None, "no pick: nothing changes");
        assert_eq!(f.front(), Some(2));
        // at the head: the span starts past it and past the empty slot after it
        assert_eq!(f.remove_newest(|j| *j == 20), Some((2, 20)));
        assert_eq!((f.len, f.front()), (2, Some(4)));
        // at the tail: the span ends before it
        assert_eq!(f.remove_newest(|_| true), Some((5, 50)));
        assert_eq!(f.len, 1);
        f.push(6, 60);
        assert_eq!(f.pop(), Some((4, 40)));
        assert_eq!(f.pop(), Some((6, 60)));
        assert_eq!((f.pop(), f.len, f.front()), (None, 0, None));
        // a middle slot then popped past: pop skips it
        for s in 7..10 {
            f.push(s, s as u32);
        }
        assert_eq!(f.remove_newest(|j| *j == 8), Some((8, 8)));
        assert_eq!(f.pop(), Some((7, 7)));
        assert_eq!(f.pop(), Some((9, 9)));
        assert_eq!(f.pop(), None);
    }
}
