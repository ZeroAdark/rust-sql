//! The reaper: a mapped reservation (`RegionBuf::map_reserved`) its owner drops goes back to the kernel off the dropping
//! thread. Its pool charge goes with it (`Held::take_pool`): the reaper thread unmaps it, then releases the charge
//! (`grant::give_back`), so the pool never counts as free the room the kernel has not got back, and a statement's end or a
//! parked region's eviction never waits on the kernel tearing down the pages a breaker touched. A reservation under
//! `REAP_MIN` writable bytes, a charge the pool has no room to take over, a full queue or a thread the OS refuses is unmapped
//! by the dropping thread, as before. `PGRUST_GRAVITY_REAP=off` (developer, read once per process): every one is.
//! The one thread lives as long as the process.
//! D4404: the memory ladder's thread never waits for it (`drain` returns inside its pass); what the reaper still owes is
//! the pass's landing credit (`landing_credit`): each handed reservation's resident bytes, until it is unmapped and for at
//! most `cancel_landing_ms` after its hand-off.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use pgsync::{Condvar, Mutex};

/// The least writable extent (`RegionBuf`'s `rw`) a dropped reservation goes to the reaper with (`cost::runtime`).
pub use gravity_program::cost::runtime::REAP_MIN_BYTES as REAP_MIN;

/// The most reservations waiting for the reaper (`cost::runtime`): past it the dropping thread unmaps its own.
use gravity_program::cost::runtime::REAP_QUEUE_MAX as QUEUE_MAX;

/// One dropped reservation: its base and length (`reserve`, as `region::unmap` takes them), and the pool bytes it
/// held (a keep's or not), released once it is unmapped.
struct Job {
    at: usize,
    reserve: usize,
    pool: u64,
    kept: bool,
    /// C8: its writable bytes as the reaper's charge (`Cache(Reaping)`: the ledger's pending account `Acct::REAPING`) from
    /// the hand-off to the unmap, given back after it.
    charge: memledger::Charge,
    /// D3872: its residency entry on `Acct::REAPING` (`resident::NONE` = not listed).
    res: u32,
    /// D4404: its landing credit's slot (`NO_CREDIT` = none).
    credit: u32,
}

struct Queue {
    jobs: Vec<Job>,
    /// `PGRUST_GRAVITY_REAP` read (at the first large drop): the reaper is on.
    on: Option<bool>,
    /// The thread is running (spawned at the first hand-off).
    thread: bool,
    /// The thread is giving back a batch it took off `jobs`.
    busy: bool,
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue { jobs: Vec::new(), on: None, thread: false, busy: false }); // alloc: Cold
static WAKE: Condvar = Condvar::new();
static IDLE: Condvar = Condvar::new();

/// D4404: the landing credit's slots, lock-free (the ladder's pass reads them without the queue's lock): a handed
/// reservation's resident bytes and its hand-off (`credit_ms`; 0 = slot free). At most `QUEUE_MAX` wait in
/// the queue and `QUEUE_MAX` more in the batch the reaper is unmapping, so a hand-off always finds one.
const CREDIT_SLOTS: usize = 2 * QUEUE_MAX;
const NO_CREDIT: u32 = u32::MAX;
static CREDIT_BYTES: [AtomicU64; CREDIT_SLOTS] = [const { AtomicU64::new(0) }; CREDIT_SLOTS];
static CREDIT_AT: [AtomicU64; CREDIT_SLOTS] = [const { AtomicU64::new(0) }; CREDIT_SLOTS];
static CREDIT_LIVE: AtomicU32 = AtomicU32::new(0);

/// D4404: a credit of `bytes` from `at_ms` (from 1): its slot, `NO_CREDIT` for none or no slot free.
fn credit_open(bytes: u64, at_ms: u64) -> u32 {
    if bytes == 0 {
        return NO_CREDIT;
    }
    let at_ms = at_ms.max(1);
    for (i, at) in CREDIT_AT.iter().enumerate() {
        if at.compare_exchange(0, at_ms, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            CREDIT_BYTES[i].store(bytes, Ordering::Release);
            CREDIT_LIVE.fetch_add(1, Ordering::AcqRel);
            return i as u32;
        }
    }
    NO_CREDIT
}

/// D4404: the reservation is unmapped: its credit ends.
fn credit_close(i: u32) {
    let Some(at) = CREDIT_AT.get(i as usize) else { return };
    CREDIT_BYTES[i as usize].store(0, Ordering::Release);
    at.store(0, Ordering::Release);
    CREDIT_LIVE.fetch_sub(1, Ordering::AcqRel);
}

/// D4404: the bytes still owed at `now_ms` whose hand-off is less than `landing_ms` ago (one relaxed load when none).
pub fn landing_credit_at(now_ms: u64, landing_ms: u64) -> u64 {
    if CREDIT_LIVE.load(Ordering::Acquire) == 0 {
        return 0;
    }
    let mut owed = 0u64;
    for (at, bytes) in CREDIT_AT.iter().zip(CREDIT_BYTES.iter()) {
        let at = at.load(Ordering::Acquire);
        if at != 0 && now_ms < at.saturating_add(landing_ms) {
            owed = owed.saturating_add(bytes.load(Ordering::Acquire));
        }
    }
    owed
}

/// D4404: the ladder pass's landing credit now (`mcx::ladder::set_unmap_landing`): each for at most `cancel_landing_ms`.
/// D4692: and the cache runs waiting for the page releaser (`allocarena::own::landing_bytes`, 0 with cache_release_bg off),
/// which this hook replaced when the reaper started. H3 step (4): and the give-back service's job lane
/// (`mem::giveback::landing_bytes`, 0 with give_back_spill off), as the service's own landing account counts it.
/// pagepool P3: plus the page pool's R1 ask its purger has not served yet (`pagepool::landing`, as long):
/// bytes on their way to the OS like the reaper's, landing to the ladder thread's pass, never room to a gate or an asker.
/// None of the pool's past the watchdog's cancel line (`mcx::ladder::past_cancel`, R5's flag).
pub fn landing_credit() -> u64 {
    let ms = mempolicy::consts::get(mempolicy::consts::K::CancelLandingMs);
    // past the watchdog's cancel line the pool's landing holds nothing off: a slow purger pass must not keep R2b / R6
    // waiting while the read climbs
    let pool = if mcx::ladder::past_cancel() { 0 } else { pagepool::landing(u32::try_from(ms).unwrap_or(u32::MAX)) as u64 };
    let (reaper, own, giveback) = (landing_credit_at(credit_ms(), ms), allocarena::own::landing_bytes() as u64, crate::mem::giveback::landing_bytes() as u64);
    // ladder_reconcile: each part with its own kind (resident, mapped, mapped, asked)
    use mempolicy::reconcile::Term;
    mcx::reconcile::put(Term::LandingReaper, reaper);
    mcx::reconcile::put(Term::LandingOwn, own);
    mcx::reconcile::put(Term::LandingGiveback, giveback);
    mcx::reconcile::put(Term::LandingPool, pool);
    reaper.saturating_add(own).saturating_add(giveback).saturating_add(pool)
}

/// The ladder pass's landing credit (`landing_credit`) as the hook's (`mcx::ladder::set_unmap_landing`) from the
/// server's start (the postmaster, once memruntime started, so it replaces the releaser's narrower hook): live before
/// any reservation reaches the reaper, and with no reaper at all. Idempotent: the reaper's first spawn installs it again.
pub fn start() {
    // pppress card 15: its bytes hold the page pool's landing (`landing_credit`), so the claimer's room counts it once
    mcx::ladder::set_unmap_landing_with_pool(landing_credit);
}

/// D4404: what a handed reservation's credit counts: its resident bytes at the residency pass's last read (`res`'s
/// charged bytes less the ones not resident then), never its writable or reserved bytes: a region with a gigabyte
/// writable and 100 MiB touched credits about 100 MiB. Not listed, or no pass has read it yet: none.
pub(crate) fn owed_of(res: u32) -> u64 {
    crate::mem::resident::resident_of(res).unwrap_or(0) as u64
}

/// D4404: the credit's clock (monotonic ms, from 1).
fn credit_ms() -> u64 {
    pg_clock::MonoStamp::now().as_ns() / 1_000_000 + 1
}

/// Reservations handed to the reaper, and given back by it (unmapped, their charge released), this process.
pub static HANDED: AtomicU64 = AtomicU64::new(0);
pub static REAPED: AtomicU64 = AtomicU64::new(0);

/// A mapped reservation of `reserve` bytes at `at` (`rw` of them writable), dropped by its only owner, handed to the reaper
/// with `held`'s pool charge (`Held::take_pool`: a charge to G becomes the pool's first; taken outside the queue's lock, as
/// it may drop the statement's context) and its pages' `charge` (C8: given back, the pages then the reaper's); false = not handed,
/// `held` as it was, the pages' charge given back here: the caller unmaps it now. A queue that filled meanwhile: unmapped
/// here and both charges released, true all the same (nothing is left for the caller).
pub(crate) fn defer(at: *mut u8, reserve: usize, rw: usize, held: &mut crate::mem::grant::Held, res: u32, charge: memledger::Charge) -> bool {
    if rw < REAP_MIN {
        return false;
    }
    {
        let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        if !*q.on.get_or_insert_with(|| crate::env::text("PGRUST_GRAVITY_REAP").is_none_or(|v| v.trim() != "off")) {
            return false;
        }
        if !q.thread {
            q.jobs.reserve_exact(QUEUE_MAX); // alloc: Cold
            match pgsync::thread::Builder::new().name("gravity-reap".into()).stack_size(256 << 10).spawn(pagegive::pinned(reaper)) {
                Ok(_) => {
                    q.thread = true;
                    start();
                }
                Err(_) => return false,
            }
        }
        if q.jobs.len() >= QUEUE_MAX {
            return false;
        }
    }
    let Some((pool, kept)) = held.take_pool() else { return false };
    let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if q.jobs.len() >= QUEUE_MAX {
        drop(q);
        drop(charge);
        crate::mem::resident::unregister(res);
        crate::mem::region::unmap(at, reserve);
        crate::mem::grant::give_back(pool, kept);
        return true;
    }
    // under the queue's lock: the reaper takes the job (and gives its charge back) only after this one is made. The pages
    // leave their breaker's kind with the owner's charge and wait as the reaper's mapped bytes, as before the home.
    drop(charge);
    let charge = memledger::Charge::open(memledger::Owner::Cache(memledger::Holder::Reaping), memledger::Release::Evict, memledger::Kind::Mapped, memledger::charge::Door::Mapped, rw as u64);
    // D3872: its residency entry moves to REAPING with its last numbers, so the residency pass bounds what the reaper
    // still holds by its resident pages
    crate::mem::resident::update_at(res, at as usize, rw, rw, memledger::Acct::REAPING, charge.nr_home());
    // D4404: its landing credit, from now
    let credit = credit_open(owed_of(res), credit_ms());
    q.jobs.push(Job { at: at as usize, reserve, pool, kept, res, charge, credit });
    HANDED.fetch_add(1, Ordering::Relaxed);
    WAKE.notify_one();
    true
}

/// The thread: each batch of handed reservations unmapped, then their pool bytes released.
fn reaper() {
    let mut batch: Vec<Job> = Vec::with_capacity(QUEUE_MAX); // alloc: Cold
    loop {
        {
            let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
            q.busy = false;
            #[cfg(test)]
            {
                // a test's hold (`hold_busy`) outlasts the batches other tests hand it
                q.busy = HOLD.load(Ordering::Acquire);
            }
            IDLE.notify_all();
            while q.jobs.is_empty() {
                q = WAKE.wait(q).unwrap_or_else(|e| e.into_inner());
            }
            q.busy = true;
            core::mem::swap(&mut q.jobs, &mut batch);
        }
        for j in batch.drain(..) {
            crate::mem::resident::unregister(j.res);
            crate::mem::region::unmap(j.at as *mut u8, j.reserve);
            credit_close(j.credit);
            drop(j.charge);
            crate::mem::grant::give_back(j.pool, j.kept);
            REAPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Waits until every reservation handed so far is given back: a trim that frees room for a draw (`GrantPool::trim_depots`,
/// `RegionDepot::trim`, `slots::Depot::trim`) reads the pool's room only after it, so the room the reaper still owes is
/// never missed (a lone draw refuses at once when no other holder can give bytes back, D3239). The statement end's own
/// trims (`trim_idle_holds`) never wait on it.
/// D4404: inside the memory ladder's pass it returns at once: the pass never waits for the reaper (D4098 saw this wait
/// last 323 ms under memory pressure); what it still owes is the pass's landing credit (`landing_credit`), re-read next pass.
pub fn drain() {
    drain_jobs(true);
}

/// `drain` after its caller waited for the give-back service's jobs itself (`RegionDepot::trim`): the reaper's queue alone.
pub(crate) fn drain_reaper() {
    drain_jobs(false);
}

fn drain_jobs(jobs: bool) {
    if memledger::ladder_pass::in_pass() {
        return;
    }
    // ladder step 1.1: the wait ticks a step, so the running kernel call's cancel ends it (`drain_polled`)
    if mempolicy::admit::gate_now() {
        let _ = drain_polled_jobs(&mut cancelpoll::step("reap drain"), jobs);
        return;
    }
    // H3 step (4): the give-back service's jobs hold pool bytes too (`mem::giveback`): no wait for them (the ladder's
    // landing credit), given back here first past the cancel line
    if jobs {
        crate::mem::giveback::drain_here(true);
    }
    let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    while q.busy || !q.jobs.is_empty() {
        q = IDLE.wait(q).unwrap_or_else(|e| e.into_inner());
    }
}

/// Ladder step 1.1 (`gate_now`): `drain` in 2 ms steps that ends early once `stop` says so (an admission's interrupt probe
/// and its step's tick); the reaper gives the rest back on its own. With no stop it is `drain`'s wait.
pub fn drain_until(stop: &mut dyn FnMut() -> bool) {
    if memledger::ladder_pass::in_pass() {
        return;
    }
    crate::mem::giveback::drain_here(true);
    let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    while q.busy || !q.jobs.is_empty() {
        if stop() {
            return;
        }
        q = IDLE.wait_timeout(q, core::time::Duration::from_millis(2)).unwrap_or_else(|e| e.into_inner()).0;
    }
}

/// D4098: `drain` for a draw inside a kernel call (`GrantPool::take_wait`): it wakes every 2 ms (the draw's own pause) to
/// tick the draw's step, and the statement's cancel ends the wait (the reaper gives the rest back on its own). Unpolled, a canceled
/// statement's draw sat through the reaper's whole batch of unmaps (323 ms under memory pressure).
pub fn drain_polled(step: &mut cancelpoll::Step) -> Result<(), cancelpoll::Canceled> {
    drain_polled_jobs(step, true)
}

fn drain_polled_jobs(step: &mut cancelpoll::Step, jobs: bool) -> Result<(), cancelpoll::Canceled> {
    if memledger::ladder_pass::in_pass() {
        return Ok(()); // D4404, as `drain`
    }
    if jobs {
        crate::mem::giveback::drain_here(true);
    }
    let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    while q.busy || !q.jobs.is_empty() {
        q = IDLE.wait_timeout(q, core::time::Duration::from_millis(2)).unwrap_or_else(|e| e.into_inner()).0;
        step.tick()?;
    }
    Ok(())
}

/// D4098's test: the reaper held mid-batch (its queue busy) until the guard drops, which wakes `drain`'s waiters.
/// D4404: one holder at a time (`HOLD`), so two tests holding it never end each other's hold.
#[cfg(test)]
pub(crate) struct Busy;

#[cfg(test)]
static HOLD: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
impl Drop for Busy {
    fn drop(&mut self) {
        QUEUE.lock().unwrap_or_else(|e| e.into_inner()).busy = false;
        IDLE.notify_all();
        HOLD.store(false, Ordering::Release);
    }
}

#[cfg(test)]
pub(crate) fn hold_busy() -> Busy {
    while HOLD.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        std::thread::sleep(core::time::Duration::from_millis(5));
    }
    QUEUE.lock().unwrap_or_else(|e| e.into_inner()).busy = true;
    Busy
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    /// D4404 (i): a handed reservation's credit counts as landing for `cancel_landing_ms` after its hand-off and no
    /// longer (the reaper held past it: the bytes count as present and the ladder's pass climbs), and ends at its unmap.
    #[test]
    fn d4404_the_landing_credit_expires_after_the_landing_window() {
        // a hand-off far past any real one (the reaper's own credits are long expired at these clocks)
        let at = 1u64 << 60;
        let landing = 2_750;
        assert_eq!(credit_open(0, at), NO_CREDIT, "nothing resident: no credit");
        let i = credit_open(64 * MIB, at);
        assert_ne!(i, NO_CREDIT);
        assert_eq!(landing_credit_at(at, landing), 64 * MIB, "landing from its hand-off");
        assert_eq!(landing_credit_at(at + landing - 1, landing), 64 * MIB, "within the window");
        assert_eq!(landing_credit_at(at + landing, landing), 0, "expired: present to the pass");
        let j = credit_open(16 * MIB, at + 1_000);
        assert_eq!(landing_credit_at(at + landing, landing), 16 * MIB, "each expires on its own hand-off");
        credit_close(i);
        assert_eq!(landing_credit_at(at + 1_000, landing), 16 * MIB, "unmapped: no longer owed");
        credit_close(j);
        assert_eq!(landing_credit_at(at + 1_000, landing), 0);
    }
}
