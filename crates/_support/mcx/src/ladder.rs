//! D4324, LADDER-DESIGN section 10 step 3: the ladder's synchronous entry point (`climb`), the free epoch (section 8)
//! and R3's flag, under `pgrust.memory_ladder = on` (memruntime publishes the mode and the line every sample; default
//! off, so nothing here runs).
//!
//! A gate whose take missed (the grant pool's `take_wait`, the D4329 scratch draw through it) climbs on its own thread:
//! R1 allocator return (`crate::give_back_caches`: the arenas' retained pages back, the workers asked to collect at
//! their idle park; cooled per `mempolicy::ladder::r1_cooling`), R2a the elastic bytes of
//! the caches and depots (memruntime's backstop reclaim), R3 fewer whole parts in flight (a flag the planner reads at
//! claim time), R4a the asker's own spill (only past its fair share), R4b revoke others. Each rung's relief is the
//! measured drop of the read (`mempolicy::ladder::relief`), never the bytes asked. The climb stops before R5: the
//! caller's wait is R5, and its budget counts from the last free (`FREE_EPOCH`); a wait spent with the need still there
//! hands R2b and R6 to the ladder thread (`hand_off`) and waits on.
//! D4353: the ladder thread (memruntime's sample) climbs the hook's order every sample past the line (`hook_pass`): R1,
//! R2a, R3, R4b, its one-tick wait, then R2b and R6 (one cancel in flight), before the watchdog's cancel line.
//! D4404: the pass never waits for the reaper; the bytes it still owes are landing to the pass (`set_unmap_landing`).
//! The rules are `mempolicy::ladder`'s; this module only runs them. No allocation, no lock of its own.

use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, Ordering::*};

use mempolicy::ladder::{self as rule, Rung, State, Who};
use mempolicy::relief::{Relief, Source};

/// `pgrust.memory_ladder`'s values as memruntime publishes them.
pub const OFF: u8 = 0;
pub const SHADOW: u8 = 1;
pub const ON: u8 = 2;

static MODE: AtomicU8 = AtomicU8::new(OFF);
/// The ladder's line (bytes; the limit less the margin) at the last sample; 0 = none yet (the climb does nothing).
static LINE: AtomicU64 = AtomicU64::new(0);
/// Section 8: bumped by every give-back of grant pool bytes, a cache reclaim that freed bytes and a rung that ran.
static FREE_EPOCH: AtomicU64 = AtomicU64::new(0);
/// R3: set by a climb, cleared by memruntime's sample once the read is back under the line.
static NARROW: AtomicBool = AtomicBool::new(false);

/// memruntime, every sample: the mode and the line.
pub fn publish(mode: u8, line: u64) {
    MODE.store(mode, Relaxed);
    LINE.store(line, Relaxed);
}

/// The cancel line: the watchdog's (the limit's pgrust.memory_cancel_threshold percent; 0 = no limit known), published
/// with the ladder's line every sample. Past it a thread that meets pressure purges inline even when the purge is the
/// purger's (PGRUST_PURGE_PRESSURE_BG), to keep the kernel from killing the server.
static CANCEL_LINE: AtomicU64 = AtomicU64::new(0);

pub fn publish_cancel_line(cancel_line: u64) {
    CANCEL_LINE.store(cancel_line, Relaxed);
}

/// The cancel line at the last sample (0 = no limit known): D4812 (2)'s block gate tests the governor's projection on it.
pub fn cancel_line() -> u64 {
    CANCEL_LINE.load(Relaxed)
}

/// Whether the reading (the ladder's own when it is on, else the last sample's) is past the cancel line.
pub fn past_cancel_line() -> bool {
    let cancel_line = CANCEL_LINE.load(Relaxed);
    cancel_line != 0 && read_now().unwrap_or_else(|| READ.load(Relaxed)) > cancel_line
}

pub fn mode() -> u8 {
    MODE.load(Relaxed)
}

/// Whether the ladder acts (`pgrust.memory_ladder = on`).
#[inline]
pub fn on() -> bool {
    MODE.load(Relaxed) == ON
}

pub fn line() -> u64 {
    LINE.load(Relaxed)
}

/// The resident bytes of the memory sample the line is published with (memruntime, every sample, whatever the mode;
/// 0 = none yet): ladder pressure's reading when the ladder is not on.
static READ: AtomicU64 = AtomicU64::new(0);

pub fn publish_read(read: u64) {
    READ.store(read, Relaxed);
}

/// `ladder_kill_read` (USER 10-18): the governed resident at the last sample (memruntime's `publish_kill_read`:
/// max(anon + shmem, memory.current less the clean page cache)), the kill decisions' read; `u64::MAX` before one.
static KILL_READ: AtomicU64 = AtomicU64::new(u64::MAX);

/// `ladder_kill_read`: the governed resident of this sample, for R6 (`Rungs::kill_read`).
pub fn publish_kill_read(resident: u64) {
    KILL_READ.store(resident.min(u64::MAX - 1), Relaxed);
}

/// `ladder_kill_read`: the kill decisions' read now, `None` with the switch off or before a sample (then R6 reads the
/// ladder's read, as before).
pub fn kill_read_now() -> Option<u64> {
    match KILL_READ.load(Relaxed) {
        u64::MAX => None,
        _ if !rule::kill_read_on() => None,
        k => Some(k),
    }
}

/// Ladder pressure now: the whole memory reading past the ladder's line (the climb's own test at ask 0), and the purge's
/// trigger (the pressure purge under pressure only, the purger's pressure tick, the idle trims under pressure only). With
/// the ladder on, its own read; otherwise the last sample's resident bytes against the line published with it. No line
/// (no limit known): never. The arenas' free committed bytes are NOT taken off: they are resident until a purge gives them
/// back, and taken off they grow until they cover any reading while nothing purges them (the mapped regions, the page
/// cache and the caches cannot use them).
pub fn pressed_now() -> bool {
    match read_now() {
        Some(read) => read > line() && need_at(Some(read), 0) != 0,
        None => pressed_at(READ.load(Relaxed), line()),
    }
}

/// `pressed_now`'s rule with the ladder not on: `read` past `line`; never with no line.
fn pressed_at(read: u64, line: u64) -> bool {
    line != 0 && read > line
}

/// Section 8: bytes went back (one relaxed add; waiters re-try their take at their next pause and restart their budget).
/// Ladder step 1.1 (`gate_now`): the add is SeqCst and the parked draws wake (`set_free_wake`), so a waiter that
/// published its handle and then read the epoch either sees this move or is woken by it.
#[inline]
pub fn note_free() {
    if mempolicy::admit::gate_now() {
        FREE_EPOCH.fetch_add(1, SeqCst);
        free_wake();
    } else {
        FREE_EPOCH.fetch_add(1, Relaxed);
    }
}

pub fn epoch() -> u64 {
    FREE_EPOCH.load(Relaxed)
}

/// Ladder step 1.1: the epoch as a parking waiter re-reads it after publishing its handle (SeqCst, `note_free`).
pub fn epoch_seq() -> u64 {
    FREE_EPOCH.load(SeqCst)
}

/// Ladder step 1.1: the driver's wake of its parked draws (`GrantPool`'s park table), installed by the first park.
static FREE_WAKE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

pub fn set_free_wake(f: fn()) {
    FREE_WAKE.store(f as *mut (), Release);
}

fn free_wake() {
    let p = FREE_WAKE.load(Acquire);
    if !p.is_null() {
        // SAFETY: only set_free_wake stores here, always from a fn().
        let f: fn() = unsafe { core::mem::transmute(p) };
        f();
    }
}

/// Ladder step 1.1: R4b's asks count as bytes on their way until this (`decide_ms`, `revoke_wait_ms` after the ask).
static RELIEF_DUE_MS: AtomicU64 = AtomicU64::new(0);
/// Ladder step 1.1: the ladder thread's passes (`hook_pass_with`), so an asker's hand-off counts as relief on its way
/// until one pass has read it.
static PASSES: AtomicU64 = AtomicU64::new(0);

/// Ladder step 1.1: the ladder thread's passes so far (an asker notes it at its hand-off).
pub fn passes() -> u64 {
    PASSES.load(Acquire)
}

/// Ladder step 1.1 (`gate_now`): relief the ladder has on its way now: R4b's asks within `revoke_wait_ms`, R6's cancel
/// still landing, the reaper's unmaps (D4404's landing credit), and an asker's hand-off at pass `handed` that no pass has
/// read yet. Reads only; false with the ladder off or no actuators but the reaper's credit.
pub fn relief_coming(handed: Option<u64>) -> bool {
    if !Relief::of(Source::ReaperCredit, unmap_landing()).is_none() {
        return true;
    }
    let Some(a) = acts().filter(|_| on()) else { return false };
    if handed.is_some_and(|p| PASSES.load(Acquire) == p) {
        return true;
    }
    RELIEF_DUE_MS.load(Relaxed) > crate::governor::decide_ms() || (a.cancel_in_flight)()
}

/// R3's flag: claims planned while it is set split inside parts (exec plan.rs `whole_part_claims`).
#[inline]
pub fn narrowed() -> bool {
    NARROW.load(Relaxed)
}

/// memruntime's sample under the line: claims planned from now take whole parts again.
pub fn clear_narrow() {
    NARROW.store(false, Relaxed);
}

/// The actuators memruntime installs (it owns the cgroup read and the backstop's reclaim).
pub struct Acts {
    /// The ladder's read now (memory.current less inactive_file, a fresh pread); `None` when unreadable.
    pub read: fn() -> Option<u64>,
    /// R2a: the bytes the caches and depots could free now, and a reclaim of up to `n` bytes (the bytes freed).
    pub reclaimable: fn() -> u64,
    pub reclaim: fn(u64) -> u64,
    /// R6: cancel the largest holder for a need of `n` bytes (true: a cancel was raised; D4029's terminate when none
    /// qualifies), and whether a cancel is still landing (a busy registry reads as one).
    pub cancel: fn(u64) -> bool,
    pub cancel_in_flight: fn() -> bool,
    /// R2b (section 0b): the protected cache bytes not in use, and their eviction (the bytes freed).
    pub protected: fn() -> u64,
    pub evict_protected: fn(u64) -> u64,
}

static ACTS: AtomicPtr<Acts> = AtomicPtr::new(core::ptr::null_mut());

pub fn install(a: &'static Acts) {
    ACTS.store(a as *const Acts as *mut Acts, Release);
}

fn acts() -> Option<&'static Acts> {
    // SAFETY: only `install` stores here, always a `&'static Acts`.
    unsafe { ACTS.load(Acquire).as_ref() }
}

/// pagepool P3: the page pool's R1 faces, installed by memruntime where it starts the pool (mcx takes no
/// dependency on the pool): `ask` = R1 off the loop thread (the purger asked; the bytes landing now, never freed);
/// `releasable` = what a release could give back now.
pub struct PoolReturn {
    pub ask: fn() -> u64,
    pub releasable: fn() -> u64,
}

static POOL_RETURN: AtomicPtr<PoolReturn> = AtomicPtr::new(core::ptr::null_mut());

pub fn set_pool_return(p: &'static PoolReturn) {
    POOL_RETURN.store(p as *const PoolReturn as *mut PoolReturn, Release);
}

fn pool_return() -> Option<&'static PoolReturn> {
    // SAFETY: only `set_pool_return` stores here, always a `&'static PoolReturn`.
    unsafe { POOL_RETURN.load(Acquire).as_ref() }
}

/// D4404: the reaper's landing credit (the driver's `reap::landing_credit`, installed when its thread starts): the
/// resident bytes of the reservations handed to it and not yet unmapped, each for at most `cancel_landing_ms` after its
/// hand-off.
static UNMAP_LANDING: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

pub fn set_unmap_landing(f: fn() -> u64) {
    UNMAP_LANDING_POOL.store(false, Release);
    UNMAP_LANDING.store(f as *mut (), Release);
}

/// pppress card 15: whether the installed hook's bytes already hold the page pool's landing (`pagepool::landing`): the
/// driver's `reap::landing_credit` does (installed by `set_unmap_landing_with_pool`), the releaser's narrower hooks do
/// not. Stored beside the hook: a read between the two stores sees one sample's landing off by the pool's.
static UNMAP_LANDING_POOL: AtomicBool = AtomicBool::new(false);

/// pppress card 15: `set_unmap_landing` for a hook whose bytes hold the page pool's landing.
pub fn set_unmap_landing_with_pool(f: fn() -> u64) {
    UNMAP_LANDING.store(f as *mut (), Release);
    UNMAP_LANDING_POOL.store(true, Release);
}

/// pppress card 15: the installed hook's bytes hold the page pool's landing.
pub fn unmap_landing_has_pool() -> bool {
    !UNMAP_LANDING.load(Acquire).is_null() && UNMAP_LANDING_POOL.load(Acquire)
}

/// pppress card 15: the page pool claimer's landing (memruntime's prefault room): the hook's bytes (`unmap_landing`),
/// the governor's `pending`, and the pool's landing `pool` once: not again where the hook holds it already.
pub fn claimer_landing(pending: u64, pool: u64) -> u64 {
    let pool = if unmap_landing_has_pool() { 0 } else { pool };
    unmap_landing().saturating_add(pending).saturating_add(pool)
}

/// D4404: the bytes the reaper still owes the ladder's read now (0 with no reaper). P3: with the parked trim's jobs
/// waiting for the give-back service (`post_parked_trim`), their bytes too.
pub fn unmap_landing() -> u64 {
    let p = UNMAP_LANDING.load(Acquire);
    let parked = parked_job_pending();
    if p.is_null() {
        return parked;
    }
    // SAFETY: only set_unmap_landing stores here, always from fn() -> u64.
    let f: fn() -> u64 = unsafe { core::mem::transmute(p) };
    f().saturating_add(parked)
}

// P3 (`parked_trim_rung`, default 0): R2p, the parked regions' and slot sets' trim as a ladder rung. The layer that owns the
// depots installs their bytes and their trim (`set_parked_trim`); the give-back service, when it runs, installs its wake
// (`set_parked_service`), and from then on R2p's trim is a job on the service's thread (`post_parked_trim`,
// `run_parked_job`) whoever climbs; without it the climbing thread trims, as R2a's depot step did (a large mapped region
// goes to the reaper). The idle release never trims under the knob (`crate::idle_trim_now`).

/// Whether the parked_trim_rung knob is on.
#[inline]
pub fn parked_rung_on() -> bool {
    mempolicy::consts::get(mempolicy::consts::K::ParkedTrimRung) != 0
}

static PARKED_BYTES: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static PARKED_TRIM: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static PARKED_WAKE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
/// The bytes R2p asked the service for and it has not taken yet (the ladder's landing credit meanwhile).
static PARKED_ASK: AtomicU64 = AtomicU64::new(0);
/// Counted (`parked_trim_counts`): inline trims and their bytes, jobs posted and their bytes, jobs the service ran and
/// the bytes they trimmed.
static PARKED_COUNTS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// The depots' parked bytes R2p may trim (`parked`) and their trim of up to `bytes`, least recently parked first, which
/// returns the bytes trimmed (`trim(bytes, job)`: `job` when it runs as the give-back service's job, on the service's
/// thread, which never waits there, neither for the reaper nor for the service's own lanes); installed by the layer that
/// owns the depots.
pub fn set_parked_trim(parked: fn() -> u64, trim: fn(u64, bool) -> u64) {
    PARKED_BYTES.store(parked as *mut (), Release);
    PARKED_TRIM.store(trim as *mut (), Release);
}

/// The give-back service's wake (installed at its start): R2p's trims become its jobs.
pub fn set_parked_service(wake: fn()) {
    PARKED_WAKE.store(wake as *mut (), Release);
}

/// The parked bytes R2p may trim now (0 with nothing installed).
pub fn parked_bytes() -> u64 {
    let p = PARKED_BYTES.load(Acquire);
    if p.is_null() {
        return 0;
    }
    // SAFETY: only set_parked_trim stores here, always from fn() -> u64.
    let f: fn() -> u64 = unsafe { core::mem::transmute(p) };
    f()
}

/// The depots' trim of up to `bytes` on this thread (`job`: as the give-back service's job); the bytes trimmed (0 with
/// nothing installed).
fn parked_trim_here(bytes: u64, job: bool) -> u64 {
    let p = PARKED_TRIM.load(Acquire);
    if p.is_null() || bytes == 0 {
        return 0;
    }
    // SAFETY: only set_parked_trim stores here, always from fn(u64, bool) -> u64.
    let f: fn(u64, bool) -> u64 = unsafe { core::mem::transmute(p) };
    f(bytes, job)
}

/// R2p's trim of up to `bytes`: a job for the give-back service when it runs (0 trimmed now: its bytes are landing until
/// the service takes them), else on this thread. The bytes trimmed now.
pub fn trim_parked(bytes: u64) -> u64 {
    if bytes == 0 {
        return 0;
    }
    if post_parked_trim(bytes) {
        return 0;
    }
    let n = parked_trim_here(bytes, false);
    PARKED_COUNTS[0].fetch_add(1, Relaxed);
    PARKED_COUNTS[1].fetch_add(n, Relaxed);
    n
}

/// R2p's trim posted to the give-back service and its wake sent; false when no service runs. At most the parked bytes
/// wait at once (a second climb before the service ran asks for what is still parked, no more).
fn post_parked_trim(bytes: u64) -> bool {
    let p = PARKED_WAKE.load(Acquire);
    if p.is_null() {
        return false;
    }
    let cap = parked_bytes();
    let _ = PARKED_ASK.fetch_update(Relaxed, Relaxed, |a| Some(a.saturating_add(bytes).min(cap.max(a))));
    PARKED_COUNTS[2].fetch_add(1, Relaxed);
    PARKED_COUNTS[3].fetch_add(bytes, Relaxed);
    // SAFETY: only set_parked_service stores here, always from fn().
    let f: fn() = unsafe { core::mem::transmute(p) };
    f();
    true
}

/// The bytes R2p's jobs still wait to trim.
pub fn parked_job_pending() -> u64 {
    PARKED_ASK.load(Relaxed)
}

/// The give-back service's pass: the waiting R2p job run on its thread (the bytes asked taken first); the bytes trimmed.
pub fn run_parked_job() -> u64 {
    let ask = PARKED_ASK.swap(0, Relaxed);
    if ask == 0 {
        return 0;
    }
    let n = parked_trim_here(ask, true);
    PARKED_COUNTS[4].fetch_add(1, Relaxed);
    PARKED_COUNTS[5].fetch_add(n, Relaxed);
    n
}

/// `(inline trims, their bytes, jobs posted, bytes asked, jobs the service ran, bytes they trimmed)` since start.
pub fn parked_trim_counts() -> [u64; 6] {
    core::array::from_fn(|i| PARKED_COUNTS[i].load(Relaxed))
}

/// What a climb reads and does, so the order can be tested without a server.
pub trait Rungs {
    fn read(&self) -> Option<u64>;
    /// `ladder_kill_read`: the kill decisions' read (the governed resident; `None` = `read`).
    fn kill_read(&self) -> Option<u64> {
        None
    }
    /// D4404: the bytes in the read the reaper is unmapping now (`unmap_landing`): the ladder thread's pass counts them
    /// as landing, never as past its line, and re-reads next pass; a gate's or an asker's climb never counts them (not room).
    fn landing(&self) -> u64 {
        0
    }
    fn reclaimable(&self) -> u64;
    fn reclaim(&self, bytes: u64) -> u64;
    /// The asker's (held, fair share, spillable) bytes.
    fn asker(&self) -> (u64, u64, u64);
    /// Others' spillable bytes revoke may ask now.
    fn revocable(&self) -> u64;
    /// R4a: the asker asked to spill `bytes` of its own.
    fn own_spill(&self, bytes: u64);
    /// R4b: others asked to spill `bytes`.
    fn revoke(&self, bytes: u64);
    /// R1: the allocators' retained pages back (`crate::give_back_caches`).
    fn alloc_return(&self);
    /// P3, R2p: the parked regions' and slot sets' bytes it may trim (read only under the parked_trim_rung knob), and
    /// its trim of up to `bytes` (the bytes trimmed now; a job posted to the give-back service trims none now).
    fn parked(&self) -> u64 {
        0
    }
    fn trim_parked(&self, _bytes: u64) -> u64 {
        0
    }
    /// The clock R1's cool-down reads (ms, from 1; 0 = none, so R1 never cools).
    fn now_ms(&self) -> u64;
    /// R2b: the protected cache bytes not in use, and their eviction (section 0b; none until the floors are in force).
    fn protected(&self) -> u64 {
        0
    }
    fn evict_protected(&self, _bytes: u64) -> u64 {
        0
    }
    /// R6: a cancel still landing, and a cancel of the largest holder (true: raised).
    fn cancel_in_flight(&self) -> bool {
        false
    }
    fn cancel(&self, _need: u64) -> bool {
        false
    }
}

/// R1's last run: when (ms, 0 = never), the read before it and its measured yield (`mempolicy::ladder::r1_cooling`).
static R1_AT: AtomicU64 = AtomicU64::new(0);
static R1_READ: AtomicU64 = AtomicU64::new(0);
static R1_YIELD: AtomicU64 = AtomicU64::new(0);

/// Per rung (`Rung`'s order): the runs, and their measured relief in bytes.
static RUNS: [AtomicU64; crate::ladder_stats::RUNGS] = [const { AtomicU64::new(0) }; crate::ladder_stats::RUNGS];
static RELIEF: [AtomicU64; crate::ladder_stats::RUNGS] = [const { AtomicU64::new(0) }; crate::ladder_stats::RUNGS];
static CLIMBS: AtomicU64 = AtomicU64::new(0);

/// (runs, relief bytes) per rung in `Rung`'s order, and the climbs so far.
pub fn counts() -> ([(u64, u64); crate::ladder_stats::RUNGS], u64) {
    (core::array::from_fn(|i| (RUNS[i].load(Relaxed), RELIEF[i].load(Relaxed))), CLIMBS.load(Relaxed))
}

/// LADDER-DESIGN section 3: the gates whose asker climbs (each climb that ran a rung is counted per gate, so the
/// ladder wiring witness, e2e memlead_ladder_wiring_witness, sees every gate climb on a real server).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Gate {
    /// The grant pool's draws (`GrantPool::take_wait`: regions past W/G, worker draws, the purse).
    Draw,
    /// Admission (`GrantPool::admit_need_within`).
    Admit,
    /// D4329's out-of-grant working memory (`mem::region::scratch_draw`).
    Scratch,
}

impl Gate {
    pub const ALL: [Gate; 3] = [Gate::Draw, Gate::Admit, Gate::Scratch];

    pub const fn name(self) -> &'static str {
        match self {
            Gate::Draw => "draw",
            Gate::Admit => "admit",
            Gate::Scratch => "scratch",
        }
    }
}

static GATE_CLIMBS: [AtomicU64; Gate::ALL.len()] = [const { AtomicU64::new(0) }; Gate::ALL.len()];

/// The climbs that ran a rung, per gate in `Gate::ALL`'s order.
pub fn gate_climbs() -> [u64; Gate::ALL.len()] {
    core::array::from_fn(|i| GATE_CLIMBS[i].load(Relaxed))
}

/// One climb's outcome: the rungs run (a bit per `Rung`), their measured relief, and the need left for the wait (R5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Climbed {
    pub ran: u16,
    pub relief: u64,
    pub need: u64,
}

/// REUSE BEFORE RELEASE (default off): the arenas' free committed bytes a new allocation reuses with no fresh fault
/// (allocarena's probe, installed with the switch). They steer where a draw's pages come from, never a judgment: the
/// climb, ladder pressure, the broker's admission and a cancel all read the whole memory, and these bytes are what R1's
/// purge gives back under pressure. Read for the accounts view only.
static REUSABLE: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_reusable_probe(f: fn() -> u64) {
    REUSABLE.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

/// The bytes a new allocation takes with no fresh fault (0 with no probe installed); information only, never headroom.
pub fn reusable_bytes() -> u64 {
    let p = REUSABLE.load(core::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        return 0;
    }
    // SAFETY: only set_reusable_probe stores here, always from fn() -> u64.
    let f: fn() -> u64 = unsafe { core::mem::transmute(p) };
    f()
}

fn state(r: &dyn Rungs, line: u64, ask: u64, who: Who, waited: bool) -> Option<State> {
    let read = r.read()?;
    // D4404: the pass never waits for the reaper (`reap::drain` returns inside it), so what the reaper still owes is
    // landing to it: no rung for it this pass. Its credit expires `cancel_landing_ms` after the hand-off: then the bytes
    // count as present and the pass climbs. Past the watchdog's cancel line nothing is landing (`PAST_CANCEL`): neither
    // the reaper's credit nor shared_buffers' bytes on their way (within their bound)
    let past = PAST_CANCEL.load(Relaxed);
    // Past the cancel line pending relief never nets the hook's read (R5)
    let relief = if past { Relief::NONE } else { Relief::of(Source::ReaperCredit, r.landing()).and(Source::BuffersLanding, buffers_landing_at(r.now_ms())) };
    let read = if who == Who::Hook { relief.read_for(read, past) } else { read };
    // `ladder_kill_read`: R6's read, netted as the read is and never above it
    let kill_read = r.kill_read().map(|k| if who == Who::Hook { relief.read_for(k, past) } else { k }.min(read));
    let (asker_held, asker_fair, asker_spillable) = if who == Who::Asker { r.asker() } else { (0, 0, 0) };
    let r1_cooling = rule::r1_cooling(r.now_ms(), R1_AT.load(Relaxed), R1_YIELD.load(Relaxed), R1_READ.load(Relaxed), read);
    // P3: R2p's parked bytes only under its knob (R2a's elastic count leaves them out then, the driver's `ladder_elastic`)
    let parked = if parked_rung_on() { r.parked() } else { 0 };
    let mut s = State { read, line, ask, r1_cooling, narrowed: narrowed(), asker_held, asker_fair, asker_spillable, parked, waited, kill_read, ..State::default() };
    if rule::need(&s) != 0 {
        s.elastic = r.reclaimable();
        s.revocable = r.revocable();
        if waited {
            s.protected = r.protected();
            s.cancel_in_flight = r.cancel_in_flight();
        }
    }
    Some(s)
}

/// The asker's climb for `ask` bytes against `line`: R1-R4b in the ruled order (`mempolicy::ladder::next`), each run once
/// and re-read after, stopping at the first that covers the need or before R5. Nothing when the ask fits under the line.
pub fn climb_with(r: &dyn Rungs, line: u64, ask: u64) -> Climbed {
    climb_as(Who::Asker, r, line, ask, false)
}

/// One climb by `who` (`waited`: past its wait, so R2b and R6 may run): the ruled order, each rung run once and the read
/// taken again after it, stopping at the first that covers the need, or at R5 (the wait is the caller's).
fn climb_as(who: Who, r: &dyn Rungs, line: u64, ask: u64, waited: bool) -> Climbed {
    let mut out = Climbed::default();
    if line == 0 {
        return out;
    }
    let Some(mut s) = state(r, line, ask, who, waited) else { return out };
    // L6: the hook's pass asks the pool once R1 / R2a are past (a gate's climb never does)
    let mut sb_asked = who != Who::Hook;
    let mut at = None;
    while let Some(rung) = rule::next(who, &s, at) {
        // L6: its bytes are landing from here, so no later rung (no cancel) runs for them
        if !sb_asked && !matches!(rung, Rung::AllocReturn | Rung::TrimParked | Rung::EvictElastic) {
            sb_asked = true;
            if let Some(after) = buffers_took(r, line, ask, who, waited, &s) {
                s = after;
                continue;
            }
        }
        let t = crate::ladder_stats::clock_ns();
        match rung {
            Rung::Wait => break,
            Rung::AllocReturn => r.alloc_return(),
            Rung::TrimParked => {
                let _ = r.trim_parked(rule::need(&s).min(s.parked));
            }
            Rung::EvictElastic => {
                let _ = r.reclaim(rule::need(&s).min(s.elastic));
            }
            Rung::NarrowScans => NARROW.store(true, Relaxed),
            Rung::OwnSpill => r.own_spill(rule::own_spill(&s)),
            Rung::Revoke => r.revoke(rule::need(&s)),
            Rung::EvictProtected => {
                let _ = r.evict_protected(rule::need(&s).min(s.protected));
            }
            Rung::Cancel => {
                if !r.cancel(rule::kill_need(&s)) {
                    break;
                }
            }
        }
        let before = s.read;
        let Some(after) = state(r, line, ask, who, waited) else { break };
        let relief = rule::relief(before, after.read);
        if rung == Rung::AllocReturn {
            R1_READ.store(before, Relaxed);
            R1_YIELD.store(relief, Relaxed);
            R1_AT.store(r.now_ms(), Relaxed);
        }
        RUNS[rung as usize].fetch_add(1, Relaxed);
        RELIEF[rung as usize].fetch_add(relief, Relaxed);
        crate::ladder_stats::note_rung(rung, relief, crate::ladder_stats::clock_ns().saturating_sub(t));
        out.ran |= 1 << rung as u8;
        out.relief += relief;
        s = after;
        at = Some(rung);
    }
    if !sb_asked {
        s = buffers_took(r, line, ask, who, waited, &s).unwrap_or(s);
    }
    if out.ran != 0 {
        CLIMBS.fetch_add(1, Relaxed);
        note_free();
    }
    out.need = rule::need(&s);
    out
}

/// L6: the pool asked for the need R1 / R2a left in `s` (`buffers_take`); the read again when it scheduled bytes.
fn buffers_took(r: &dyn Rungs, line: u64, ask: u64, who: Who, waited: bool, s: &State) -> Option<State> {
    if buffers_take(rule::need(s), s.elastic, r.now_ms()) == 0 {
        return None;
    }
    state(r, line, ask, who, waited)
}

/// D4353: the hook's episode: when a pass ran R1-R4b and left the need (ms, 0 = none), and the read then (R5's yield is
/// its drop to the pass that ends the wait).
static HOOK_SINCE: AtomicU64 = AtomicU64::new(0);
static HOOK_READ: AtomicU64 = AtomicU64::new(0);
/// An asker whose wait was spent with the need still there (`hand_off`): the next pass is past its wait.
static STARVED: AtomicBool = AtomicBool::new(false);
/// Kill safety: this sample's read is past the watchdog's cancel line (`waive_wait`; it lasts the hook's pass after it):
/// no landing of any source holds a rung off. A canceled hog's bytes the next hog reuses never fall from the read, so a
/// landing credit or hold would last its whole landing time while the read climbs on to a kernel kill. D4886: but R6
/// waits for the pool's R1 ask under the top tier (`Acts::cancel_in_flight`, memruntime's `pool_r1_holds`): free pool
/// memory no statement reuses, and the hold ends at the purger's pass that serves the ask.
static PAST_CANCEL: AtomicBool = AtomicBool::new(false);

/// Whether this sample's read is past the watchdog's cancel line (`PAST_CANCEL`): the actuators' cancel-landing hold
/// reads it too (`Acts::cancel_in_flight`).
pub fn past_cancel() -> bool {
    PAST_CANCEL.load(Relaxed)
}

/// R5's measured yield: the read's drop over a wait that began at `from`.
fn note_wait(from: u64, to: u64) {
    RUNS[Rung::Wait as usize].fetch_add(1, Relaxed);
    RELIEF[Rung::Wait as usize].fetch_add(rule::relief(from, to), Relaxed);
    crate::ladder_stats::note_rung(Rung::Wait, rule::relief(from, to), 0);
}

/// The ladder thread's pass at `now_ms` (one sample): under the line nothing, and the episode ends; past it the hook's
/// order, R2b and R6 only once its wait (`mempolicy::ladder::hook_waited`) or an asker's ended with the need still there.
pub fn hook_pass_with(r: &dyn Rungs, line: u64, now_ms: u64) -> Climbed {
    let since = HOOK_SINCE.load(Relaxed);
    let waited = rule::hook_waited(since, now_ms) || STARVED.load(Relaxed);
    let out = climb_as(Who::Hook, r, line, 0, waited);
    // R5 counts once per wait: its yield is the read's drop from the pass that began it to the pass that ended it
    let wait_ended = |r: &dyn Rungs| match HOOK_READ.swap(0, Relaxed) {
        0 => {}
        from => note_wait(from, r.read().unwrap_or(from)),
    };
    if out.need == 0 {
        if since != 0 {
            wait_ended(r);
        }
        HOOK_SINCE.store(0, Relaxed);
        STARVED.store(false, Relaxed);
    } else if since == 0 {
        HOOK_SINCE.store(now_ms.max(1), Relaxed);
        HOOK_READ.store(r.read().unwrap_or(0), Relaxed);
    } else if waited {
        wait_ended(r);
    }
    // ladder step 1.1: the pass is over (its R6 cancel, if any, raised): an asker's hand-off before it has been read
    if mempolicy::admit::gate_now() {
        PASSES.fetch_add(1, Release);
    }
    PAST_CANCEL.store(false, Relaxed);
    out
}

/// D4353: the sample is past the watchdog's cancel line: the hook's wait is over, so this pass runs R2b and R6 before the
/// watchdog's cancel would (in the same sample, `memruntime::refresh_governor`). A hog that crosses the ladder's line and
/// the cancel line within one tick is the ladder's to cancel, never the watchdog's.
pub fn waive_wait() {
    if on() {
        STARVED.store(true, Relaxed);
        PAST_CANCEL.store(true, Relaxed);
    }
}

/// Whether the hook's wait (or an asker's) has ended in the episode open now (the episode lines' plan).
pub fn hook_waited(now_ms: u64) -> bool {
    rule::hook_waited(HOOK_SINCE.load(Relaxed), now_ms) || STARVED.load(Relaxed)
}

/// The ladder thread's pass (`pgrust.memory_ladder = on` only).
pub fn hook_pass(now_ms: u64) -> Climbed {
    match acts() {
        Some(a) if on() => {
            // ladder 1.3 (`fair_share`): every running query's fair share first, the rungs' revoke and own-spill read it
            fair_tick();
            with_live(a, |live| hook_pass_with(live, line(), now_ms))
        }
        _ => Climbed::default(),
    }
}

/// An asker's R5 (section 3): it waits once its climb left a need; the read at its start.
#[derive(Clone, Copy, Debug)]
pub struct Waiting(u64);

/// The asker's wait after its climb `c`: counted as R5 only when the climb left a need (memory short, not only the pool).
pub fn wait_begin(c: &Climbed) -> Option<Waiting> {
    if c.need == 0 {
        return None;
    }
    let a = acts()?;
    Some(Waiting((a.read)()?))
}

/// The asker's wait ended (its take, or its budget): R5's run and its measured yield.
pub fn wait_end(w: Option<Waiting>) {
    let Some(Waiting(from)) = w else { return };
    if let Some(a) = acts() {
        note_wait(from, (a.read)().unwrap_or(from));
    }
}

/// Section 3: an asker's wait was spent; when the need for its `ask` is still there, R2b and R6 go to the ladder thread
/// (its next pass is past its wait) and the asker waits on (true). The asker never cancels anyone itself.
pub fn hand_off(ask: u64) -> bool {
    let Some(a) = acts().filter(|_| on()) else { return false };
    let Some(read) = (a.read)() else { return false };
    if rule::need(&State { read, line: line(), ask, ..State::default() }) == 0 {
        return false;
    }
    STARVED.store(true, Relaxed);
    true
}

/// D4812 (2): the block gate's bound passed with the governor's projection still past the cancel line and its statement
/// not canceled: R2b and R6 go to the ladder thread (its next pass is past its wait, as an asker's `hand_off`), woken now
/// (`memledger::arena::note_due`). No read here (the gate's projection already showed the need).
pub fn block_hand_off() {
    if on() {
        STARVED.store(true, Relaxed);
        #[cfg(feature = "std")]
        memledger::arena::note_due();
    }
}

/// D4812 (2), the block gate's climb (governor `block_gate`) for a block of `ask` bytes at the governor's projected
/// reading `read`. It runs inside an allocation, so only the rungs that store or ask run here, in the asker's order:
/// R3 (the `NARROW` flag), R4a (`revoke::request_own`: an ask; the spill runs at the operator's own safe point, never
/// inside this allocation) and R4b (`revoke::try_request_revoke_within_shares` and `RELIEF_DUE_MS`, as a gate's climb).
/// None of them allocates. Their one lock is the governor's registry (R4b's walks and its input), taken by try_lock,
/// never waited on, and never held across an allocation: the walks read atomics into fixed arrays. R1, R2p and R2a are
/// the ladder pass's: the pass is woken now (`memledger::arena::note_due`, its own wake), and none of them runs here.
/// No read is taken (the asks land later, so none counts relief). Their lines (a settled ask's, a shadow decision's) go
/// through the scoped log defer (`with_gate_log_defer`). The rungs run (a bit per `Rung`), and whether the registry was
/// held (R4b skipped or cut short).
pub fn block_climb(read: u64, ask: u64) -> (u16, bool) {
    let line = line();
    if !on() || line == 0 {
        return (0, false);
    }
    let out = crate::with_gate_log_defer(|| {
        let (asker_held, asker_fair, asker_spillable) = asker_bytes();
        let mut busy = false;
        let revocable = crate::revoke::try_spillable_total().unwrap_or_else(|_| {
            busy = true;
            0
        });
        let mut s = State { read, line, ask, narrowed: narrowed(), asker_held, asker_fair, asker_spillable, revocable, ..State::default() };
        let (mut ran, mut at) = (0u16, None);
        while let Some(rung) = rule::next(Who::Asker, &s, at) {
            at = Some(rung);
            let t = crate::ladder_stats::clock_ns();
            match rung {
                // the pass's (woken below)
                Rung::AllocReturn | Rung::TrimParked | Rung::EvictElastic => continue,
                Rung::NarrowScans => {
                    NARROW.store(true, Relaxed);
                    s.narrowed = true;
                }
                Rung::OwnSpill => crate::revoke::request_own(rule::own_spill(&s)),
                Rung::Revoke => {
                    if revoke_within_shares(rule::need(&s), false).is_err() {
                        busy = true;
                    }
                }
                Rung::Wait | Rung::EvictProtected | Rung::Cancel => break,
            }
            RUNS[rung as usize].fetch_add(1, Relaxed);
            crate::ladder_stats::note_rung(rung, 0, crate::ladder_stats::clock_ns().saturating_sub(t));
            ran |= 1 << rung as u8;
        }
        (ran, busy)
    });
    #[cfg(feature = "std")]
    memledger::arena::note_due();
    out
}

/// D4812 (2): the block gate canceled its own statement past its bound (governor `try_cancel_self`: R6's pick named it
/// and no cancel was in flight). That is R6's run (its pick, its act, its landing), made on the asker's thread for the
/// asker's own statement only: counted as R6's, and its line written as the ladder thread's R6 writes its own (`ladder on
/// R6: read ...`), through the scoped log defer. `read` is the projection with the block of `block` bytes, `ranked` the
/// statement's (rank, charged) bytes, `backend` its (slot, pid).
pub fn block_canceled(read: u64, block: u64, cancel_line: u64, limit: u64, ranked: (u64, u64), backend: Option<(usize, i32)>) {
    RUNS[Rung::Cancel as usize].fetch_add(1, Relaxed);
    crate::ladder_stats::note_rung(Rung::Cancel, 0, 0);
    #[cfg(any(feature = "std", test))]
    crate::with_gate_log_defer(|| {
        use core::fmt::Write as _;
        let mut e = Line { b: [0; 384], n: 0 };
        let _ = write!(
            e,
            "ladder on R6: read {} MB (the gate's projection of the ladder's read with a block of {} MB) is past the cancel line {} MB of the {} MB limit: the block gate canceled its own statement, the largest holder ({} MB) by resident bytes ({} MB charged)",
            read >> 20,
            block >> 20,
            cancel_line >> 20,
            limit >> 20,
            ranked.0 >> 20,
            ranked.1 >> 20
        );
        let _ = match backend {
            Some((slot, pid)) => write!(e, " in pid {pid} (slot {slot})"),
            None => write!(e, " in no backend"),
        };
        e.n = e.n.min(e.b.len() - 1);
        e.b[e.n] = b'\n';
        crate::server_log_line(&e.b[..e.n + 1]);
    });
    #[cfg(not(any(feature = "std", test)))]
    let _ = (read, block, cancel_line, limit, ranked, backend);
}

/// One line on the stack (`block_canceled`): cut at its end, never an allocation.
#[cfg(any(feature = "std", test))]
struct Line {
    b: [u8; 384],
    n: usize,
}

#[cfg(any(feature = "std", test))]
impl core::fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let k = s.len().min(self.b.len() - self.n);
        self.b[self.n..self.n + k].copy_from_slice(&s.as_bytes()[..k]);
        self.n += k;
        Ok(())
    }
}

/// R4b: others asked to spill `bytes` within their shares (ladder 1.3-1.4: under `fair_share` never a query within its
/// share; the rung is no kill-avoidance), the asker's own statement never; `wait`: the registry waited on, else taken by
/// try_lock (`Err(Busy)`, D4812 (2)). Ladder step 1.1: the asks are relief on its way for `revoke_wait_ms`
/// (`relief_coming`). The bytes asked.
fn revoke_within_shares(bytes: u64, wait: bool) -> Result<u64, crate::governor::Busy> {
    let i = crate::governor::decide_inputs();
    let share = i.b / (i.holders.max(1) as u64);
    let me = crate::governor::with_current(|l| l.slot_index()).flatten();
    let asked = if wait {
        crate::revoke::request_revoke_within_shares(bytes, share, me)
    } else {
        crate::revoke::try_request_revoke_within_shares(bytes, share, me)?
    };
    if asked != 0 && mempolicy::admit::gate_now() {
        let due = crate::governor::decide_ms().saturating_add(mempolicy::consts::get(mempolicy::consts::K::RevokeWaitMs));
        RELIEF_DUE_MS.fetch_max(due, Relaxed);
    }
    Ok(asked)
}

/// Ladder 1.3: the running queries' fair shares set (`revoke::fair_tick`) over the budget's elastic part, the total the
/// equal split (`rule::fair`) divides; nothing unless `fair_share` acts.
pub fn fair_tick() {
    if crate::revoke::fair_on() {
        let i = crate::governor::decide_inputs();
        crate::revoke::fair_tick(i.b.saturating_sub(rule::protected_of(i.b)));
    }
}

/// This thread's statement as the live rungs' asker: (held, fair share, spillable) bytes, held as every decision reads it
/// (`StmtMem::held`, D4395). Ladder 1.3: under `fair_share` its fair share is the one the ladder's sample set.
pub fn asker_now() -> (u64, u64, u64) {
    let (v, note) = asker_read();
    if let Some(n) = note {
        n.write();
    }
    v
}

/// LEDGER2 step 4's R4a shadow decision read with an asker (`asker_read`): today's and the owner tree's, and when.
#[derive(Clone, Copy)]
struct OwnSpillNote(crate::ledger2::Choice, crate::ledger2::Choice, u64);

impl OwnSpillNote {
    fn write(self) {
        crate::ledger2::note(crate::ledger2::Decision::OwnSpill, self.0, self.1, self.2);
    }
}

/// The equal split of the budget's elastic part over the statements holding grant memory now (`rule::fair`).
fn split_fair() -> u64 {
    let i = crate::governor::decide_inputs();
    let elastic_budget = i.b.saturating_sub(rule::protected_of(i.b));
    rule::fair(0, elastic_budget, i.holders as u64)
}

/// Statement `l` as the rungs' asker: (held, fair share, spillable) bytes, its fair share the split `fair` unless
/// `fair_share` set its own.
fn asker_of(l: &crate::governor::StmtMem, fair: u64) -> (u64, u64, u64) {
    let fair = match crate::revoke::share_of(l) {
        s if s != 0 && crate::revoke::fair_on() => s,
        _ => fair,
    };
    (l.held(), fair, crate::revoke::spillable_of(l))
}

/// D4812 (2): `asker_now`'s bytes with no R4a shadow decision (the block gate's climb reads no owner tree).
fn asker_bytes() -> (u64, u64, u64) {
    let fair = split_fair();
    crate::governor::with_current(|l| asker_of(l, fair)).unwrap_or((0, fair, 0))
}

/// `asker_now`'s bytes and its R4a shadow decision, not yet noted (D4812: a climb notes it after its rungs ran).
fn asker_read() -> ((u64, u64, u64), Option<OwnSpillNote>) {
    let fair = split_fair();
    crate::governor::with_current(|l| {
        let (held, fair, spillable) = asker_of(l, fair);
        // LEDGER2 step 4: R4a's input as the owner tree reads it, noted beside today's (over its fair share: it spills).
        // Only where R4a can run (main 09:50:42Z): an asker with no spillable bytes (its admission, before any draw) is
        // counted as skipped, not compared
        let note = if spillable != 0 {
            let tree = crate::ledger2::owned(l.slot_index());
            let spills = |b: u64| if b > fair { l.slot_index() } else { None };
            Some(OwnSpillNote((spills(held), held), (spills(tree), tree), crate::governor::decide_ms()))
        } else {
            crate::ledger2::skip(crate::ledger2::Decision::OwnSpill);
            None
        };
        ((held, fair, spillable), note)
    })
    .unwrap_or(((0, fair, 0), None))
}

/// The live rungs: memruntime's actuators, the governor's statement view, C5's revoke; and (D4812) the R4a shadow
/// decisions its asker reads made, written once the climb's rungs ran (act first, then log; past `PENDING` one is
/// written at once).
struct Live(&'static Acts, core::cell::Cell<([Option<OwnSpillNote>; PENDING], usize)>);

/// The R4a shadow decisions a climb holds back: one per read of its state, at most one per rung and the first.
const PENDING: usize = 16;

/// `f` over the live rungs, then the R4a shadow decisions it held back written, in their order: the only way a `Live`
/// is made, so no pass or climb (the hook's included) leaves a decision unwritten; no `Drop` (the no-drop rule). A panic
/// that unwinds out of `f` loses the shadow notes held back (at most `PENDING`), and that is accepted: they are
/// diagnostics, not decisions.
fn with_live<T>(a: &'static Acts, f: impl FnOnce(&Live) -> T) -> T {
    let live = Live(a, core::cell::Cell::new(([None; PENDING], 0)));
    let out = f(&live);
    let (notes, n) = live.1.replace(([None; PENDING], 0));
    for note in notes[..n].iter().flatten() {
        note.write();
    }
    out
}

impl Rungs for Live {
    fn read(&self) -> Option<u64> {
        (self.0.read)()
    }

    fn kill_read(&self) -> Option<u64> {
        kill_read_now()
    }

    fn landing(&self) -> u64 {
        unmap_landing()
    }

    fn reclaimable(&self) -> u64 {
        (self.0.reclaimable)()
    }

    fn reclaim(&self, bytes: u64) -> u64 {
        let freed = (self.0.reclaim)(bytes);
        if freed != 0 {
            note_free();
        }
        freed
    }

    fn asker(&self) -> (u64, u64, u64) {
        let (v, note) = asker_read();
        if let Some(note) = note {
            let (mut notes, n) = self.1.get();
            if n < PENDING {
                notes[n] = Some(note);
                self.1.set((notes, n + 1));
            } else {
                note.write();
            }
        }
        v
    }

    fn revocable(&self) -> u64 {
        crate::revoke::spillable_total()
    }

    fn own_spill(&self, bytes: u64) {
        crate::revoke::request_own(bytes);
    }

    fn revoke(&self, bytes: u64) {
        let _ = revoke_within_shares(bytes, true);
    }

    fn alloc_return(&self) {
        crate::give_back_caches();
        // pagepool P3: the pool's warm pages are allocator-retained free memory: R1 asks the pool's purger
        // for them (no remap here, `PoolReturn::ask`); landing until it ran (in `unmap_landing`'s credit), never freed.
        // Not inside give_back_caches: the idle release calls that too, and an idle pool stays warm
        if let Some(p) = pool_return() {
            let _ = (p.ask)();
        }
    }

    fn parked(&self) -> u64 {
        parked_bytes()
    }

    fn trim_parked(&self, bytes: u64) -> u64 {
        trim_parked(bytes)
    }

    fn now_ms(&self) -> u64 {
        crate::governor::decide_ms()
    }

    fn cancel_in_flight(&self) -> bool {
        (self.0.cancel_in_flight)()
    }

    fn cancel(&self, need: u64) -> bool {
        (self.0.cancel)(need)
    }

    fn protected(&self) -> u64 {
        (self.0.protected)()
    }

    fn evict_protected(&self, bytes: u64) -> u64 {
        let freed = (self.0.evict_protected)(bytes);
        if freed != 0 {
            note_free();
        }
        freed
    }
}

/// D4365: the bytes `ask` would put the ladder's read past its line now (a fresh read; 0 when the ladder is off, has no
/// line yet or cannot read): a gate's re-test while it waits, without a climb.
pub fn need(ask: u64) -> u64 {
    need_at(read_now(), ask)
}

/// D4387: the ladder's read now for a gate (`None` when the ladder is off, has no line yet or cannot read), taken before
/// a decision that runs under a lock ([`need_at`]): the read's sample stays outside the lock.
pub fn read_now() -> Option<u64> {
    match acts() {
        Some(a) if on() && line() != 0 => (a.read)(),
        _ => None,
    }
}

/// D4387: [`need`] at a read already taken ([`read_now`]): arithmetic only, no read, no allocation.
pub fn need_at(read: Option<u64>, ask: u64) -> u64 {
    match read {
        Some(read) if on() && line() != 0 => rule::need(&State { read, line: line(), ask, ..State::default() }),
        _ => 0,
    }
}

/// A gate's climb for `ask` bytes (`pgrust.memory_ladder = on` only; otherwise nothing).
pub fn climb(ask: u64) -> Climbed {
    match acts() {
        Some(a) if on() => with_live(a, |live| climb_with(live, line(), ask)),
        _ => Climbed::default(),
    }
}

/// `climb` from `gate`, counted per gate when it ran a rung (`gate_climbs`).
pub fn climb_at(gate: Gate, ask: u64) -> Climbed {
    let c = climb(ask);
    if c.ran != 0 {
        GATE_CLIMBS[gate as usize].fetch_add(1, Relaxed);
    }
    c
}

/// Ladder C1 (`cache_floor`, CACHE-IMPLEMENTATION-PLAN section 7: the ladder is the one that evicts): the headroom under
/// the line at the last sample (the line less the read; `u64::MAX` = none, so no draw evicts inline), the grant pool's
/// held bytes then (`set_pool_held`) and the limit the marks are of.
static ROOM: AtomicU64 = AtomicU64::new(u64::MAX);
static ROOM_HELD: AtomicU64 = AtomicU64::new(0);
static ROOM_LIMIT: AtomicU64 = AtomicU64::new(0);
/// Ladder C1: the grant pool's held bytes (the driver's, installed at the pool's arming).
static POOL_HELD: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
/// Ladder C1: the ladder's last reclaim that freed bytes (`decide_ms`; 0 = none yet), the refill guard's clock.
static RECLAIM_AT_MS: AtomicU64 = AtomicU64::new(0);
/// Ladder C1: the burst LOW is (the p99 rise of the read over `rule::low_window_ms`, the ladder thread's; `u64::MAX` =
/// none measured yet: LOW at its most), and the room a background drain still aims at (HIGH when it began; 0 = none).
static BURST: AtomicU64 = AtomicU64::new(u64::MAX);
static DRAIN_TO: AtomicU64 = AtomicU64::new(0);
/// Ladder C1: the give-back rate (ns per GiB; 0 = none timed yet: `rule::GIVE_BACK_NS_PER_GIB_PRIOR`).
static GIVE_BACK_NS_PER_GIB: AtomicU64 = AtomicU64::new(0);
/// Ladder C1's counters, in `C1Counts`' order.
static C1: [AtomicU64; 12] = [const { AtomicU64::new(0) }; 12];
const BG_RUNS: usize = 0;
const BG_BYTES: usize = 1;
const IN_RUNS: usize = 2;
const IN_BYTES: usize = 3;
const IN_NS: usize = 4;
const IN_NS_MAX: usize = 5;
const IN_SHORT: usize = 6;
const REFILL_CLOSED: usize = 7;
const GB_RUNS: usize = 8;
const GB_BYTES: usize = 9;
const GB_NS: usize = 10;
const BG_QUIET: usize = 11;

/// Ladder C1's counters since start (the ladder views' `cachefloor` rows).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct C1Counts {
    /// The ladder thread's reclaims from under LOW to HIGH (`background_pass`: one batch of at most `C1_BATCH_MAX` a
    /// sample) and the bytes they freed.
    pub bg_runs: u64,
    pub bg_bytes: u64,
    /// The draws that evicted inline (`note_inline`), the bytes, their time (ns, total and the longest), and how many
    /// were still short when their deadline or the caches' floors stopped them.
    pub inline_runs: u64,
    pub inline_bytes: u64,
    pub inline_ns: u64,
    pub inline_ns_max: u64,
    pub inline_short: u64,
    /// The refill guard's refusals (`refill_open` false).
    pub refill_closed: u64,
    /// The reclaims timed into the give-back rate (`note_give_back`: `GIVE_BACK_TIMED_MIN` or more), their bytes and
    /// their time (ns, each from its start to its bytes given back).
    pub give_back_runs: u64,
    pub give_back_bytes: u64,
    pub give_back_ns: u64,
    /// The samples under LOW (or draining) with less than `C1_QUIET_MIN` reclaimable: no walk, the drain ended.
    pub bg_quiet: u64,
}

pub fn c1_counts() -> C1Counts {
    let g = |i: usize| C1[i].load(Relaxed);
    C1Counts {
        bg_runs: g(BG_RUNS),
        bg_bytes: g(BG_BYTES),
        inline_runs: g(IN_RUNS),
        inline_bytes: g(IN_BYTES),
        inline_ns: g(IN_NS),
        inline_ns_max: g(IN_NS_MAX),
        inline_short: g(IN_SHORT),
        refill_closed: g(REFILL_CLOSED),
        give_back_runs: g(GB_RUNS),
        give_back_bytes: g(GB_BYTES),
        give_back_ns: g(GB_NS),
        bg_quiet: g(BG_QUIET),
    }
}

pub fn set_pool_held(f: fn() -> u64) {
    POOL_HELD.store(f as *mut (), Release);
}

fn pool_held() -> u64 {
    let p = POOL_HELD.load(Acquire);
    if p.is_null() {
        return 0;
    }
    // SAFETY: only set_pool_held stores here, always from fn() -> u64.
    let f: fn() -> u64 = unsafe { core::mem::transmute(p) };
    f()
}

/// The limit of the ladder's last sample (`background_pass`, every sample whether C1 is on or not; 0 before the first).
static SAMPLE_LIMIT: AtomicU64 = AtomicU64::new(0);

/// Whether a reading taken now is at R2a's take target: its headroom under the ladder's line, the reaper's landing bytes
/// out as R2a takes them out (`background_pass`), at or past HIGH for the last sample's limit with the ladder's measured
/// burst (`marks_now`, the marks `background_pass_with` drains to). The purger's pressure mode leaves here, so the purger
/// and R2a stop at one mark. Never with no line or before the first sample.
pub fn at_take_target(read: u64) -> bool {
    let (line, limit) = (line(), SAMPLE_LIMIT.load(Relaxed));
    line != 0 && limit != 0 && line.saturating_sub(Relief::of(Source::ReaperCredit, unmap_landing()).ask_read(read)) >= marks_now(limit).1
}

/// Ladder C1: the burst LOW is now (`None` before the ladder's thread measured one).
pub fn burst() -> Option<u64> {
    Some(BURST.load(Relaxed)).filter(|&b| b != u64::MAX)
}

/// Ladder C1: the (LOW, HIGH) marks for `limit` now: LOW the measured burst (`rule::marks_at`).
pub fn marks_now(limit: u64) -> (u64, u64) {
    rule::marks_at(limit, burst())
}

/// Ladder C1c: (LOW, HIGH, TOP) now (`rule::marks3`).
pub fn marks3_now(limit: u64) -> (u64, u64, u64) {
    rule::marks3(limit, burst())
}

/// Ladder C1: the give-back rate now (ns per GiB): the timed reclaims' (`note_give_back`), else the prior.
pub fn give_back_ns_per_gib() -> u64 {
    match GIVE_BACK_NS_PER_GIB.load(Relaxed) {
        0 => rule::GIVE_BACK_NS_PER_GIB_PRIOR,
        r => r,
    }
}

/// Ladder C1: a reclaim freed `bytes` in `ns` from its start to its bytes given back (the driver's R2a, settled: the
/// ladder's rungs, the background batches and the draws' evictions); one of `GIVE_BACK_TIMED_MIN` or more moves the
/// rate LOW is sized with (`rule::give_back_rate`) and is counted.
pub fn note_give_back(bytes: u64, ns: u64) {
    if bytes < rule::GIVE_BACK_TIMED_MIN {
        return;
    }
    let _ = GIVE_BACK_NS_PER_GIB.fetch_update(Relaxed, Relaxed, |r| Some(rule::give_back_rate(r, bytes, ns)));
    C1[GB_RUNS].fetch_add(1, Relaxed);
    C1[GB_BYTES].fetch_add(bytes, Relaxed);
    C1[GB_NS].fetch_add(ns, Relaxed);
}

/// Ladder C1b (the user via the broker, 10-16): the caches the ladder sizes under `cache_floor`, as indexes: the part
/// cache, the fp tables, the condition cache.
pub const SIZED_PART: usize = 0;
pub const SIZED_FP: usize = 1;
pub const SIZED_COND: usize = 2;
/// The page pool's warm reserve (`PGRUST_POOL_RESERVE=1`): held = its free warm superpages, evicted = its foreground
/// miss bytes; protected floor 0 (no `note_sized_floor`).
pub const SIZED_RESERVE: usize = 3;
/// The sized caches' count.
pub const SIZED_N: usize = 4;
/// Ladder C1b: each sized cache's budget (`u64::MAX` = at its start: the ladder neither took from it nor raised it
/// yet), its owner's start as last read (the derived budget, a server setting or the configured cap; `u64::MAX` = none
/// read yet), and (C1d) its floor at the ladder's last sample: the bytes its trim cannot drop (in use, and the part
/// cache's images Gravity's part lists hold, which only the evicted hook gives back, all at once).
static SIZED_BUDGET: [AtomicU64; SIZED_N] = [const { AtomicU64::new(u64::MAX) }; SIZED_N];
static SIZED_START: [AtomicU64; SIZED_N] = [const { AtomicU64::new(u64::MAX) }; SIZED_N];
static SIZED_FLOOR: [AtomicU64; SIZED_N] = [const { AtomicU64::new(0) }; SIZED_N];
/// Ladder C1b: the part cache's hold reach at the ladder's last sample (`part_hold_reach`; 0 = none).
static HOLD_REACH: AtomicU64 = AtomicU64::new(0);

/// Ladder C1b / C1d: sized cache `c`'s budget now, its owner's `start` noted (read at a statement start or an insert):
/// with C1 on, the ladder's (it comes down to what a ladder take leaves, which is never under the bytes the cache's trim
/// cannot drop then, and goes up past TOP on demand, past the start too: the start is no cap, the marks bound it); off,
/// the start (one relaxed load or two). The floor never lifts it: a statement whose parts the lists hold past the budget
/// is trimmed around while it runs, as with C1 off, and no later cut evicts its parts when the floor falls back (D4715).
#[inline]
pub fn sized_budget(c: usize, start: u64) -> u64 {
    if !floors_on() {
        return start;
    }
    SIZED_START[c].store(start, Relaxed);
    match SIZED_BUDGET[c].load(Relaxed) {
        u64::MAX => start,
        b => b,
    }
}

/// Ladder C1d: sized cache `c`'s floor at the ladder's sample (the driver's `c1_caches_stuck`: in use, and for the part
/// cache the bytes its trim cannot free, `cur - trimmable`, which counts the images part lists hold). The view's; the
/// budget does not read it (a take leaves at least these bytes, and a sample's may be stale by then).
pub fn note_sized_floor(c: usize, bytes: u64) {
    SIZED_FLOOR[c].store(bytes, Relaxed);
}

/// D4829 (`cache_floor_hold`): sized cache `c`'s protected floor (section 0b's share, `mempolicy::ladder::r2a_floor`'s
/// `protected_c`; the driver's, at each sample and each take; 0 = none noted). A take leaves the budget at least the
/// bytes it kept (the driver's), and under the cancel line a budget under this floor (a take past that line, a larger B)
/// comes back up to it at the next sample with no drain to run, whatever the raises' gates: the grant pool already counts
/// the floor as the cache's (`set_pool_floors`), and floors are a hard limit.
static SIZED_PROTECT: [AtomicU64; SIZED_N] = [const { AtomicU64::new(0) }; SIZED_N];
/// D4829, per sized cache: the budgets brought back up to the protected floor (`restore_floors`) and the bytes they rose.
static SIZED_RESTORES: [AtomicU64; SIZED_N] = [const { AtomicU64::new(0) }; SIZED_N];
static SIZED_RESTORED: [AtomicU64; SIZED_N] = [const { AtomicU64::new(0) }; SIZED_N];

pub fn note_sized_protect(c: usize, bytes: u64) {
    SIZED_PROTECT[c].store(bytes, Relaxed);
}

/// D4829: whether `cache_floor_hold` is on (the take's clamp in the driver and `restore_floors`).
#[inline]
pub fn floor_hold_on() -> bool {
    mempolicy::consts::get(mempolicy::consts::K::CacheFloorHold) != 0
}

/// D4829: per sized cache (budgets brought back up to the protected floor, bytes they rose) since start (the view).
pub fn sized_restores() -> [(u64, u64); SIZED_N] {
    core::array::from_fn(|c| (SIZED_RESTORES[c].load(Relaxed), SIZED_RESTORED[c].load(Relaxed)))
}

/// D4829 (`cache_floor_hold`, the background pass with no drain to run): each sized cache whose ladder budget sits under
/// its protected floor gets it back, unless the read is past the cancel line. Only the budget moves (no fill); no
/// raise's clocks are touched.
fn restore_floors() {
    if !floor_hold_on() || past_cancel_line() {
        return;
    }
    for c in 0..SIZED_N {
        let floor = SIZED_PROTECT[c].load(Relaxed);
        let budget = SIZED_BUDGET[c].load(Relaxed);
        if floor != 0 && floor != u64::MAX && budget != u64::MAX && budget < floor {
            SIZED_BUDGET[c].store(floor, Relaxed);
            SIZED_RESTORES[c].fetch_add(1, Relaxed);
            SIZED_RESTORED[c].fetch_add(floor - budget, Relaxed);
        }
    }
}

/// Ladder C1b: the ladder's takes from each sized cache and the bytes they freed, since start (the view).
static SIZED_TAKES: [AtomicU64; SIZED_N] = [const { AtomicU64::new(0) }; SIZED_N];
static SIZED_TAKEN: [AtomicU64; SIZED_N] = [const { AtomicU64::new(0) }; SIZED_N];

/// Ladder C1b: the ladder took `bytes` from sized cache `c` (counted; its budget is `note_taken`'s).
pub fn note_take_bytes(c: usize, bytes: u64) {
    SIZED_TAKES[c].fetch_add(1, Relaxed);
    SIZED_TAKEN[c].fetch_add(bytes, Relaxed);
}

/// Ladder C1b: per sized cache (the ladder's takes, the bytes they freed) since start (the view).
pub fn sized_takes() -> [(u64, u64); SIZED_N] {
    core::array::from_fn(|c| (SIZED_TAKES[c].load(Relaxed), SIZED_TAKEN[c].load(Relaxed)))
}

/// Ladder C1b: the ladder took bytes from sized cache `c`, which holds `held` after: its budget comes down to that, never
/// up: a take from a cache its floor held past its budget leaves it past the budget, and that is no raise.
/// C1c: a take within `C1_REVERSAL_MS` of the cache's last raise is a reversal (counted; it backs the raises off).
/// The take's own trim is not demand: the probe's eviction count is read again here.
pub fn note_taken(c: usize, held: u64) {
    note_taken_at(c, held, crate::governor::decide_ms().max(1));
}

fn note_taken_at(c: usize, held: u64, now: u64) {
    if floors_on() {
        let cur = match SIZED_BUDGET[c].load(Relaxed) {
            u64::MAX => SIZED_START[c].load(Relaxed),
            b => b,
        };
        SIZED_BUDGET[c].store(held.min(cur), Relaxed);
        if let Some((_, evicts)) = sized_probe(c) {
            SIZED_EVICTS[c].store(evicts, Relaxed);
            // C1d: the next raise counts the cache's own evictions from here (the take's bytes are not demand)
            SIZED_EVICT_BASE[c].store(evicts, Relaxed);
        }
        let raised = SIZED_RAISE_AT[c].load(Relaxed);
        if raised != 0 && now.saturating_sub(raised) <= rule::C1_REVERSAL_MS {
            SIZED_REVERSALS[c].fetch_add(1, Relaxed);
            SIZED_RECENT[c].fetch_add(1, Relaxed);
            SIZED_REVERSAL_AT[c].store(now, Relaxed);
            let shift = SIZED_QUIET_SHIFT[c].load(Relaxed);
            SIZED_QUIET_SHIFT[c].store(shift.saturating_add(1).min(20), Relaxed);
        }
        SIZED_TAKEN_AT[c].store(now, Relaxed);
        SIZED_CALM_FROM[c].store(0, Relaxed);
    }
}

/// Ladder C1c: each sized cache's demand probe (its owner's, installed at start): (the bytes it holds, `u64::MAX` when
/// that is not cheap to read; C1d: the bytes of the evictions and trims it made itself to stay under its budget, only
/// growing). An insert evicts only at the budget, so a moved count says the cache was at its budget then.
static SIZED_PROBE: [AtomicPtr<()>; SIZED_N] = [const { AtomicPtr::new(core::ptr::null_mut()) }; SIZED_N];
/// Ladder C1c, per sized cache: the probe's eviction count last seen and when it last moved (`decide_ms`; 0 = never),
/// its last raise, its last reversal and the reversals since (the backoff), and its raises / raised bytes / reversals.
static SIZED_EVICTS: [AtomicU64; SIZED_N] = [const { AtomicU64::new(0) }; SIZED_N];
static SIZED_EVICT_AT: [AtomicU64; SIZED_N] = [const { AtomicU64::new(0) }; SIZED_N];
/// Ladder C1d, per sized cache: the probe's evicted bytes at its last raise or the ladder's last take from it (the next
/// raise gives back what it evicted since; `rule::raise_bytes`).
static SIZED_EVICT_BASE: [AtomicU64; SIZED_N] = [const { AtomicU64::new(0) }; SIZED_N];
static SIZED_RAISE_AT: [AtomicU64; SIZED_N] = [const { AtomicU64::new(0) }; SIZED_N];
static SIZED_REVERSAL_AT: [AtomicU64; SIZED_N] = [const { AtomicU64::new(0) }; SIZED_N];
static SIZED_RECENT: [AtomicU32; SIZED_N] = [const { AtomicU32::new(0) }; SIZED_N];
/// Ladder C1, per sized cache (the refill backoff, `rule::refill_quiet_ms`): the ladder's last take from it (`decide_ms`;
/// 0 = none), its first raise since that take (0 = none yet), and the doublings of its refill quiet period: one per
/// reversal, back to none once a raise lasted a whole quiet period with no take.
static SIZED_TAKEN_AT: [AtomicU64; SIZED_N] = [const { AtomicU64::new(0) }; SIZED_N];
static SIZED_CALM_FROM: [AtomicU64; SIZED_N] = [const { AtomicU64::new(0) }; SIZED_N];
static SIZED_QUIET_SHIFT: [AtomicU32; SIZED_N] = [const { AtomicU32::new(0) }; SIZED_N];
static SIZED_RAISES: [AtomicU64; SIZED_N] = [const { AtomicU64::new(0) }; SIZED_N];
static SIZED_RAISED: [AtomicU64; SIZED_N] = [const { AtomicU64::new(0) }; SIZED_N];
static SIZED_REVERSALS: [AtomicU64; SIZED_N] = [const { AtomicU64::new(0) }; SIZED_N];
/// Ladder C1d v2: the lowest headroom under the line in the last `C1_DEMAND_MS`, in two halves (this one, the last one;
/// `u64::MAX` = none noted), when this half began, and per sized cache the probe's evicted bytes when each half began.
static ROOM_LOW: [AtomicU64; 2] = [const { AtomicU64::new(u64::MAX) }; 2];
static ROOM_LOW_AT: AtomicU64 = AtomicU64::new(0);
/// Ladder C1: the least room the claims left in the same window (`claim_low`), in the same two halves.
static CLAIM_LOW: [AtomicU64; 2] = [const { AtomicU64::new(u64::MAX) }; 2];
static SIZED_EVICT_WIN: [[AtomicU64; 2]; SIZED_N] = [const { [const { AtomicU64::new(0) }; 2] }; SIZED_N];

/// Ladder C1d v2 (the ladder's sample, its one writer): a new half window every `C1_DEMAND_MS / 2`; a sample a whole
/// window or more after the last half began forgets both (the room and the evictions they saw are older than the window).
fn roll_window(now: u64) {
    let half = rule::C1_DEMAND_MS / 2;
    let at = ROOM_LOW_AT.load(Relaxed);
    let since = now.saturating_sub(at);
    if at != 0 && since < half {
        return;
    }
    let stale = at == 0 || since >= 2 * half;
    let cur = ROOM_LOW[0].swap(u64::MAX, Relaxed);
    ROOM_LOW[1].store(if stale { u64::MAX } else { cur }, Relaxed);
    let cur = CLAIM_LOW[0].swap(u64::MAX, Relaxed);
    CLAIM_LOW[1].store(if stale { u64::MAX } else { cur }, Relaxed);
    ROOM_LOW_AT.store(now, Relaxed);
    for (c, win) in SIZED_EVICT_WIN.iter().enumerate() {
        let ev = sized_probe(c).map_or(0, |p| p.1);
        let cur = win[0].swap(ev, Relaxed);
        win[1].store(if stale { ev } else { cur }, Relaxed);
    }
}

/// Ladder C1d v2: a sample's headroom (the read: what is in use, every tick) for the window's low; with `inline_on_draw`
/// also the headroom at each fresh map (`note_mapped`), so the peaks between two samples count too.
#[inline]
fn note_room_low(room: u64) {
    ROOM_LOW[0].fetch_min(room, Relaxed);
}

/// Ladder C1d v2: the lowest headroom of the last `C1_DEMAND_MS` (30-60 s back), `None` before one was noted (the view).
pub fn room_low() -> Option<u64> {
    let low = ROOM_LOW[0].load(Relaxed).min(ROOM_LOW[1].load(Relaxed));
    (low != u64::MAX).then_some(low)
}

/// Ladder C1: the least room the claims left in the last `C1_DEMAND_MS` (`note_pool_room`), `None` before one was noted.
/// A raise is at most this (`rule::raise_room_within`).
pub fn claim_low() -> Option<u64> {
    let low = CLAIM_LOW[0].load(Relaxed).min(CLAIM_LOW[1].load(Relaxed));
    (low != u64::MAX).then_some(low)
}

pub fn set_sized_probe(c: usize, f: fn() -> (u64, u64)) {
    SIZED_PROBE[c].store(f as *mut (), Release);
}

fn sized_probe(c: usize) -> Option<(u64, u64)> {
    let p = SIZED_PROBE[c].load(Acquire);
    if p.is_null() {
        return None;
    }
    // SAFETY: only set_sized_probe stores here, always from fn() -> (u64, u64).
    let f: fn() -> (u64, u64) = unsafe { core::mem::transmute(p) };
    Some(f())
}

/// Ladder C1c: per sized cache (raises, bytes raised, reversals) since start (the view).
pub fn sized_counts() -> [(u64, u64, u64); SIZED_N] {
    core::array::from_fn(|c| (SIZED_RAISES[c].load(Relaxed), SIZED_RAISED[c].load(Relaxed), SIZED_REVERSALS[c].load(Relaxed)))
}

/// Ladder C1c, why no raise (criterion 6, D4911: no raise after HUNGRY in any S1 arm): per sized cache, the samples past
/// the drain whose raise its room held (under one step: no room past TOP, the reclaim quiet or the windows), its timing
/// (`raise_timing_ok`: the refill quiet since its last take, its raise interval) or no demand. Counts only: no decision
/// reads them.
pub const HELD_ROOM: usize = 0;
pub const HELD_TIMING: usize = 1;
pub const HELD_DEMAND: usize = 2;
static RAISE_HELD: [[AtomicU64; 3]; SIZED_N] = [const { [const { AtomicU64::new(0) }; 3] }; SIZED_N];

/// Ladder C1c, the gate before the caches: the samples past the drain with raises on, those a landing take held, those
/// within the reclaim quiet (`cache_refill_quiet_ms` since the last reclaim), those the refault gate held, those quiet
/// whose windows left under one step (room_low, claim_low, TOP), and the samples still draining (`to` > 0).
pub const GATE_SAMPLES: usize = 0;
pub const GATE_LANDING: usize = 1;
pub const GATE_UNQUIET: usize = 2;
pub const GATE_REFAULT: usize = 3;
pub const GATE_BOUND: usize = 4;
pub const GATE_DRAINING: usize = 5;
static RAISE_GATE: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// Ladder C1c (the view): each sized cache's raise holds by why since start (`HELD_*`).
pub fn raise_held() -> [[u64; 3]; SIZED_N] {
    core::array::from_fn(|c| core::array::from_fn(|w| RAISE_HELD[c][w].load(Relaxed)))
}

/// Ladder C1c (the view): the raise gate's samples by why since start (`GATE_*`; refault = `refault_held`).
pub fn raise_gate() -> [u64; 6] {
    let mut g: [u64; 6] = core::array::from_fn(|i| RAISE_GATE[i].load(Relaxed));
    g[GATE_REFAULT] = REFAULT_HELD.load(Relaxed);
    g
}

/// Ladder C1c (the view): sized cache `c`'s refill backoff now: its shift (doubled per reversal, reset only after a raise
/// that lasted a whole quiet period), the refill quiet it gives (ms) and the ms since the ladder's last take from it
/// (`u64::MAX` = none).
pub fn sized_quiet(c: usize) -> (u32, u64, u64) {
    let shift = SIZED_QUIET_SHIFT[c].load(Relaxed);
    let quiet = rule::refill_quiet_ms(mempolicy::consts::get(mempolicy::consts::K::CacheRefillQuietMs), shift);
    let since = match SIZED_TAKEN_AT[c].load(Relaxed) {
        0 => u64::MAX,
        at => crate::governor::decide_ms().saturating_sub(at),
    };
    (shift, quiet, since)
}

/// Ladder C1b: the sized caches' ladder budgets now (the view; `u64::MAX` = at the start) and (C1d) their floors.
pub fn sized_budgets() -> [u64; SIZED_N] {
    core::array::from_fn(|c| SIZED_BUDGET[c].load(Relaxed))
}

pub fn sized_floors() -> [u64; SIZED_N] {
    core::array::from_fn(|c| SIZED_FLOOR[c].load(Relaxed))
}

/// Ladder C1b: the part cache's hold reach at the ladder's last sample: its ladder budget plus the room the sample's
/// raises left (`raise_sized`; past TOP over the window's LOW, quiet since the last reclaim), so the budget plus what a
/// raise could give it then; the budget alone where no raise could give (at or under TOP, not quiet, a take that
/// sample, or the part cache's own raise timing: within its refill quiet period after a take or its raise interval,
/// `raise_timing_ok`); 0 with C1 off, before its first sample or the part cache's first start. The floor's hold bounds
/// the part cache's own trims at the larger of this and the floor plus one step
/// (`mempolicy::cache::part_cache_own_cushion`).
pub fn part_hold_reach() -> u64 {
    HOLD_REACH.load(Relaxed)
}

/// `part_hold_reach` after a sample's raises left `left` of their room.
fn note_hold_reach(left: u64) {
    let reach = match (SIZED_START[SIZED_PART].load(Relaxed), SIZED_BUDGET[SIZED_PART].load(Relaxed)) {
        (u64::MAX, _) => 0,
        (start, u64::MAX) => start.saturating_add(left),
        (_, b) => b.saturating_add(left),
    };
    HOLD_REACH.store(reach, Relaxed);
}

/// Ladder C1c: whether sized cache `c`'s clocks let a raise run at `now`: past the refill quiet period since the
/// ladder's last take from it (`rule::refill_quiet_ms`, doubled per reversal) and past its raise interval since its
/// last raise (`rule::raise_interval_ms`, the backoff). The one timing rule of the raises and (C1b) of the part cache's
/// hold reach (`note_hold_reach`): the hold grows the cache past its budget only where a raise itself could come, so a
/// take is never followed by a regrow through the reach while its raises are held back.
fn raise_timing_ok(c: usize, now: u64) -> bool {
    let base = mempolicy::consts::get(mempolicy::consts::K::CacheRefillQuietMs);
    let taken = SIZED_TAKEN_AT[c].load(Relaxed);
    let quiet = taken == 0 || now.saturating_sub(taken) >= rule::refill_quiet_ms(base, SIZED_QUIET_SHIFT[c].load(Relaxed));
    quiet && now.saturating_sub(SIZED_RAISE_AT[c].load(Relaxed)) >= rule::raise_interval_ms(SIZED_RECENT[c].load(Relaxed))
}

/// Ladder C1c (the user's three marks): past TOP, at least `cache_refill_quiet_ms` after the ladder's last reclaim, the
/// sized caches with demand (`rule::has_demand`: at their budget and an eviction of their own within a minute, and one
/// since their last raise: a raise is never followed by another before the cache filled it) get their budgets raised,
/// in order, while the raises stay within `room` (what brings the headroom back to HIGH: the marks are the only bound,
/// C1d: past the start too), and no cache sooner than its backoff interval after its last raise. C1d: each raise is the
/// bytes the cache evicted itself since its last raise or the ladder's last take (`rule::raise_bytes`: a step at least,
/// the room at most). C1d v2: the evictions within the window only (`C1_DEMAND_MS`), and the room is the window's lowest
/// headroom back to HIGH (`rule::raise_room`). Never a fill: a raise moves only the budget. Every sample, each probe's
/// evicted bytes are read (their last move timed) and an old backoff is reset. The room the raises left.
fn raise_sized(mut room: u64, now: u64) -> u64 {
    for c in 0..SIZED_N {
        let Some((held, evicts)) = sized_probe(c) else { continue };
        if SIZED_EVICTS[c].swap(evicts, Relaxed) != evicts {
            SIZED_EVICT_AT[c].store(now, Relaxed);
        }
        if now.saturating_sub(SIZED_REVERSAL_AT[c].load(Relaxed)) > rule::C1_DEMAND_MS {
            SIZED_RECENT[c].store(0, Relaxed);
        }
        let start = SIZED_START[c].load(Relaxed);
        let budget = match SIZED_BUDGET[c].load(Relaxed) {
            u64::MAX => start,
            b => b,
        };
        // C1d v2: the evictions since the last raise or take, within the window; C1b: the part cache's bytes held past
        // its budget once the floor let its own trim go (`rule::held_past`) are demand too
        let from = SIZED_EVICT_BASE[c].load(Relaxed).max(SIZED_EVICT_WIN[c][1].load(Relaxed));
        let past = if c == SIZED_PART { rule::held_past(held, budget, SIZED_FLOOR[c].load(Relaxed)) } else { 0 };
        let step = rule::raise_bytes(evicts.saturating_sub(from).max(past), room);
        if start == u64::MAX || step == 0 {
            if start != u64::MAX {
                RAISE_HELD[c][HELD_ROOM].fetch_add(1, Relaxed);
            }
            continue;
        }
        // the refill backoff: a raise that lasted a whole quiet period with no take resets it; under it, no raise until
        // the quiet period since the ladder's last take from this cache is over, nor within its raise interval
        let base = mempolicy::consts::get(mempolicy::consts::K::CacheRefillQuietMs);
        let calm = SIZED_CALM_FROM[c].load(Relaxed);
        if calm != 0 && now.saturating_sub(calm) >= rule::refill_quiet_ms(base, SIZED_QUIET_SHIFT[c].load(Relaxed)) {
            SIZED_QUIET_SHIFT[c].store(0, Relaxed);
        }
        if !raise_timing_ok(c, now) {
            RAISE_HELD[c][HELD_TIMING].fetch_add(1, Relaxed);
            continue;
        }
        let (raised, evicted) = (SIZED_RAISE_AT[c].load(Relaxed), SIZED_EVICT_AT[c].load(Relaxed));
        let demand = past != 0 || (rule::has_demand(held, budget, evicted, now) && evicted > raised);
        if !demand {
            RAISE_HELD[c][HELD_DEMAND].fetch_add(1, Relaxed);
            continue;
        }
        if calm == 0 {
            SIZED_CALM_FROM[c].store(now, Relaxed);
        }
        SIZED_BUDGET[c].store(budget.saturating_add(step), Relaxed);
        SIZED_EVICT_BASE[c].store(evicts, Relaxed);
        SIZED_RAISE_AT[c].store(now, Relaxed);
        SIZED_RAISES[c].fetch_add(1, Relaxed);
        SIZED_RAISED[c].fetch_add(step, Relaxed);
        room -= step;
    }
    room
}

/// Ladder C1c, the refault gate (`cache_refault_gate`): the limiting cgroup's file refaults as last sampled (memory.stat
/// workingset_refault_file, bytes, only growing; `u64::MAX` = no count: no cgroup limit), the start of the minute they
/// are counted over and the count then, the bytes refaulted over the last whole minute, and the passes the gate held a
/// raise in.
static REFAULTS: AtomicU64 = AtomicU64::new(u64::MAX);
static REFAULT_MARK_MS: AtomicU64 = AtomicU64::new(0);
static REFAULT_MARK: AtomicU64 = AtomicU64::new(0);
static REFAULT_MINUTE: AtomicU64 = AtomicU64::new(0);
static REFAULT_HELD: AtomicU64 = AtomicU64::new(0);

/// Ladder C1c: the limiting cgroup's file refaults now (bytes since its start), at each sample (`now_ms`, decide's clock).
pub fn note_file_refaults(bytes: u64, now_ms: u64) {
    REFAULTS.store(bytes.min(u64::MAX - 1), Relaxed);
    let at = REFAULT_MARK_MS.load(Relaxed);
    if at == 0 || now_ms < at {
        REFAULT_MARK_MS.store(now_ms.max(1), Relaxed);
        REFAULT_MARK.store(bytes, Relaxed);
        return;
    }
    let span = now_ms - at;
    if span >= rule::C1_REFAULT_WINDOW_MS {
        let n = bytes.saturating_sub(REFAULT_MARK.load(Relaxed));
        REFAULT_MINUTE.store(n.saturating_mul(rule::C1_REFAULT_WINDOW_MS) / span, Relaxed);
        REFAULT_MARK_MS.store(now_ms, Relaxed);
        REFAULT_MARK.store(bytes, Relaxed);
    }
}

/// Ladder C1c: no cgroup limit, so no refault count (the gate stays open).
pub fn no_file_refaults() {
    REFAULTS.store(u64::MAX, Relaxed);
    REFAULT_MARK_MS.store(0, Relaxed);
    REFAULT_MINUTE.store(0, Relaxed);
}

/// Ladder C1c: the file bytes refaulted in a minute: the last whole minute's, or the current one's so far when more
/// (`None`: no count).
pub fn file_refaults_per_min() -> Option<u64> {
    let now = REFAULTS.load(Relaxed);
    if now == u64::MAX {
        return None;
    }
    Some(REFAULT_MINUTE.load(Relaxed).max(now.saturating_sub(REFAULT_MARK.load(Relaxed))))
}

/// Ladder C1c: the background passes whose raise the refault gate held.
pub fn refault_held() -> u64 {
    REFAULT_HELD.load(Relaxed)
}

/// Ladder C1: the ms since the ladder's last reclaim that freed bytes (`u64::MAX` = none yet).
fn since_reclaim_ms() -> u64 {
    match RECLAIM_AT_MS.load(Relaxed) {
        0 => u64::MAX,
        at => crate::governor::decide_ms().saturating_sub(at),
    }
}

/// Ladder C1: whether it acts now (`cache_floor` with the ladder on). Off: two relaxed loads.
#[inline]
pub fn floors_on() -> bool {
    on() && rule::cache_floor()
}

/// Ladder C1: the headroom `room` under the line for `limit` as of now (the ladder's sample; a test's), the pool's held
/// bytes noted beside it.
pub fn set_room(room: u64, limit: u64) {
    ROOM_LIMIT.store(limit, Relaxed);
    ROOM_HELD.store(pool_held(), Relaxed);
    ROOM_MAPPED.store(MAPPED.load(Relaxed), Relaxed);
    ROOM.store(room, Relaxed);
}

/// Ladder C1 try (`inline_on_draw`): the bytes the statements mapped fresh (their
/// arenas' chunks and slot regions: `note_mapped`, the driver's `ladder_mapped`) since start, and at the ladder's last
/// sample. With the switch on, the room now is the last sample's less what was MAPPED since (use), not what the pool
/// reserved since.
static MAPPED: AtomicU64 = AtomicU64::new(0);
static ROOM_MAPPED: AtomicU64 = AtomicU64::new(0);

/// Ladder C1 try: a statement mapped `bytes` fresh (the driver's `ladder_mapped`, with `inline_on_draw` on). The headroom
/// it leaves (the last sample's less every map since, use rather than a reservation) goes into the raise window's low:
/// a W8 statement maps its peak within a second, between two 100 ms samples, which the samples alone miss.
#[inline]
pub fn note_mapped(bytes: u64) {
    let mapped = MAPPED.fetch_add(bytes, Relaxed).saturating_add(bytes);
    let room = ROOM.load(Relaxed);
    if room != u64::MAX {
        note_room_low(room.saturating_sub(mapped.saturating_sub(ROOM_MAPPED.load(Relaxed))));
    }
}

/// Ladder C1 under the memory broker: B's free bytes over its low mark (a reservation's claim counted, as the broker's
/// reclaimer and its draws read them; 0 at a draw B had no room for) go into the claims' window (`claim_low`), as the
/// pool's room does. A raise into the bytes B's draws claim was taken back at the next draw B was short for (the
/// coremem stress at a 16 GB limit: 285 raises and 41 reversals, 40 of the takes inline at such a draw, while the room
/// by use stayed 1.6-7 GB).
#[inline]
pub fn note_b_room(room: u64) {
    CLAIM_LOW[0].fetch_min(room, Relaxed);
}

/// Ladder C1 try (`inline_on_draw`), in a grant pool armed with the caches at their protected floors: the room a pool
/// take that landed left (`room`, with the parked regions the pool's own trim frees first) goes into the claims' window
/// (`claim_low`), which bounds a raise with no margin of its own. That pool counts the cache bytes past the floors the
/// ladder cannot free as held, so a raise past the pool's room made the next W8 admission or draw miss and climb, and
/// its R2a took the raise back (16 passes of an 8-stream scan load: 5 reversals, each a gate climb's R2a, with 6-8 GB
/// of cache bytes counted held in a 16 GiB pool). Noted in the headroom's window (HIGH's margin on the pool's room, the
/// parked regions left out), no raise came at all (a 1.3 GB low over 16 passes); a take that missed notes nothing, as
/// the admissions' first asks past the room and the takes before the depots' trim missed in every pass, and their 0
/// held every raise back too. A draw past its grant the pool missed (the gate, whose climb reaches the caches) notes 0:
/// with the landed takes alone the raises came within a few hundred MB of the pool's edge, and two such draws' climbs
/// took them back.
#[inline]
pub fn note_pool_room(room: u64) {
    CLAIM_LOW[0].fetch_min(room, Relaxed);
}

/// Ladder C1 try (`inline_on_draw`): the bytes the maps since the last sample evict inline (`rule::inline_want` of those
/// maps over the sample's room, the inline evictions since counted back in it: what keeps LOW free after them); 0 with C1
/// off or before the ladder's first sample.
pub fn inline_need_mapped() -> u64 {
    let room = ROOM.load(Relaxed);
    if room == u64::MAX || !floors_on() {
        return 0;
    }
    let since = MAPPED.load(Relaxed).saturating_sub(ROOM_MAPPED.load(Relaxed));
    rule::inline_want(since, room, marks_now(ROOM_LIMIT.load(Relaxed)).0)
}

/// Ladder C1: the headroom now and the limit: the last sample's less what the pool drew since (a release is never
/// counted back before the next sample: its regions may sit parked); `None` before a sample or with C1 off.
pub fn room_now() -> Option<(u64, u64)> {
    let room = ROOM.load(Relaxed);
    if room == u64::MAX || !floors_on() {
        return None;
    }
    // the C1 try (`inline_on_draw`): what the statements mapped since the sample, not what the pool reserved
    let since = if rule::inline_on_draw() { MAPPED.load(Relaxed).saturating_sub(ROOM_MAPPED.load(Relaxed)) } else { pool_held().saturating_sub(ROOM_HELD.load(Relaxed)) };
    Some((room.saturating_sub(since), ROOM_LIMIT.load(Relaxed)))
}

/// Ladder C1 (C0's finding 1): the cache bytes past their protected floors their trims cannot drop (the driver's
/// `reclaim::c1_caches_stuck`, installed at the pool's arming), as of the ladder's last sample; the pool counts them held.
static CACHES_STUCK: AtomicU64 = AtomicU64::new(0);
static CACHES_STUCK_FN: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

pub fn set_caches_stuck(f: fn() -> u64) {
    CACHES_STUCK_FN.store(f as *mut (), Release);
}

/// D4893: when the stuck cache bytes were last re-read (decide's clock; 0 = never).
static STUCK_AT_MS: AtomicU64 = AtomicU64::new(0);

/// D4893: whether the ladder's sample at `now` re-reads the stuck cache bytes: every sample while the headroom is under
/// HIGH (the grant pool's admission near the line reads them), else once `stuck_refresh_ms` passed since the last read
/// (0: every sample). The read walks every part's image maps; at a full part cache it was ~95 % of the ladder thread's
/// idle cost, every 100 ms, with the headroom far above the line.
fn stuck_due(headroom: u64, high: u64, now: u64) -> bool {
    let every = mempolicy::consts::get(mempolicy::consts::K::StuckRefreshMs);
    let at = STUCK_AT_MS.load(Relaxed);
    every == 0 || headroom < high || at == 0 || now.saturating_sub(at) >= every
}

/// Ladder C1: the stuck cache bytes re-read (the ladder's sample; 0 with C1 off); fewer is room, so the draws wake.
pub fn refresh_stuck() {
    STUCK_AT_MS.store(crate::governor::decide_ms().max(1), Relaxed);
    let p = CACHES_STUCK_FN.load(Acquire);
    let now = if p.is_null() || !floors_on() {
        0
    } else {
        // SAFETY: only set_caches_stuck stores here, always from fn() -> u64.
        let f: fn() -> u64 = unsafe { core::mem::transmute(p) };
        f()
    };
    if CACHES_STUCK.swap(now, Relaxed) > now {
        note_free();
    }
}

/// Ladder C1: the stuck cache bytes the grant pool counts as held (`GrantPool::take_leaving`); 0 with C1 off (the
/// ladder's mode or `cache_floor` changed since the last sample's read included).
#[inline]
pub fn caches_stuck() -> u64 {
    if floors_on() { CACHES_STUCK.load(Relaxed) } else { 0 }
}

/// `ladder_reconcile` (`crate::reconcile::sample`): the ladder's own terms at the sample: the room and the window's
/// low (`NONE` before a sample or a note), the maps since the sample (inline_on_draw's note_mapped writes them into the
/// window; 0 with it off), the claims' low, the stuck cache bytes, R2p's jobs and shared_buffers' give-back on its way.
pub(crate) fn reconcile_terms() {
    use crate::reconcile::put;
    use mempolicy::reconcile::{Term, NONE};
    let room = ROOM.load(Relaxed);
    put(Term::Room, if room == u64::MAX { NONE } else { room });
    put(Term::RoomLow, room_low().unwrap_or(NONE));
    let since = MAPPED.load(Relaxed).saturating_sub(ROOM_MAPPED.load(Relaxed));
    put(Term::MapsSince, if room == u64::MAX { NONE } else if rule::inline_on_draw() { since } else { 0 });
    put(Term::ClaimLow, claim_low().unwrap_or(NONE));
    put(Term::Stuck, caches_stuck());
    put(Term::LandingParkedJobs, parked_job_pending());
    put(Term::LandingBuffers, buffers_landing_at(crate::governor::decide_ms().max(1)));
}

/// Ladder C1: a reclaim freed `freed` bytes: the room grows by them, the refill guard's clock restarts, waiters wake, and
/// so does the purger.
fn note_reclaimed(freed: u64) {
    if freed == 0 {
        return;
    }
    let _ = ROOM.fetch_update(Relaxed, Relaxed, |r| (r != u64::MAX).then(|| r.saturating_add(freed)));
    RECLAIM_AT_MS.store(crate::governor::decide_ms(), Relaxed);
    note_free();
    // the purger (memruntime, C3) applies the cut and purges the freed pages, so they leave memory.current now
    crate::wake_purger();
}

/// Ladder C1, the background trigger (the ladder's thread after the hook's pass, every sample): the headroom under `line`
/// (`read` less the reaper's landing bytes) is published for the draws, and under LOW R2a (`Acts::reclaim`: the parked
/// regions and depots, then each cache down to its protected floor, then the tail, each reclaim given back) takes it to
/// HIGH, one batch of at most `C1_BATCH_MAX` a sample (the user's give-back bar: a cut of at most 2 GiB back within
/// 100 ms), the drain going on at the next samples until HIGH or a batch comes back short (the caches at their floors).
/// Quiet when there is nothing to take: under `C1_QUIET_MIN` reclaimable (`Acts::reclaimable`) the sample walks nothing.
/// `burst`: the p99 rise over LOW's window measured this sample (`None`: none, LOW keeps the last). `ran`: the hook's
/// pass ran a rung this sample, so `read` is stale (published, nothing reclaimed). The bytes freed; with C1 off nothing
/// (and no room published, so no draw evicts).
pub fn background_pass(limit: u64, line: u64, read: u64, ran: bool, burst: Option<u64>) -> u64 {
    SAMPLE_LIMIT.store(limit, Relaxed);
    if stuck_due(line.saturating_sub(read), marks_now(limit).1, crate::governor::decide_ms()) {
        refresh_stuck();
    }
    match acts() {
        // pagepool P3: below LOW the pool's warm pages go first (R1 before R2a, as the climb): ASKED of its
        // purger (`PoolReturn::ask`; this thread never remaps), counted as nothing freed: the bytes landing lower the take
        // (here, and as landing in the next pass's read through `unmap_landing`), and the caches are cut only for what is
        // still short of them
        Some(a) if floors_on() => {
            let landing = landing_of_read(unmap_landing(), read);
            background_pass_landed(
                &|n| {
                    let landing = pool_return().map_or(0, |p| (p.ask)());
                    if landing >= n { 0 } else { (a.reclaim)(n - landing) }
                },
                &|| (a.reclaimable)().saturating_add(pool_return().map_or(0, |p| (p.releasable)())),
                limit,
                line,
                Relief::of(Source::ReaperCredit, landing).ask_read(read),
                landing,
                ran,
                burst,
            )
        }
        _ => {
            ROOM.store(u64::MAX, Relaxed);
            DRAIN_TO.store(0, Relaxed);
            HOLD_REACH.store(0, Relaxed);
            0
        }
    }
}

/// `landing_cap_read` (default 1): the landing a pass nets is at most the read it nets it from. A landing past the read
/// (s1rerun (7) at 100 ms: 8-17.6 GiB at a statement's end, read 4.9 GB, limit 15.7 GiB) netted the read to 0 and took
/// the raises' room to 0 for the window's whole minute: none came after the first take.
fn landing_of_read(landing: u64, read: u64) -> u64 {
    if mempolicy::consts::get(mempolicy::consts::K::LandingCapRead) != 0 { landing.min(read) } else { landing }
}

/// `background_pass` over `reclaim` and `reclaimable` (a test's fakes), the landing bytes already out of `read`.
pub fn background_pass_with(reclaim: &dyn Fn(u64) -> u64, reclaimable: &dyn Fn() -> u64, limit: u64, line: u64, read: u64, ran: bool, burst: Option<u64>) -> u64 {
    background_pass_landed(reclaim, reclaimable, limit, line, read, unmap_landing(), ran, burst)
}

/// `background_pass_with` with the `landing` bytes already out of `read` given: the raises' room is the room less them.
#[allow(clippy::too_many_arguments)]
fn background_pass_landed(reclaim: &dyn Fn(u64) -> u64, reclaimable: &dyn Fn() -> u64, limit: u64, line: u64, read: u64, landing: u64, ran: bool, burst: Option<u64>) -> u64 {
    SAMPLE_LIMIT.store(limit, Relaxed);
    if line == 0 || limit == 0 {
        ROOM.store(u64::MAX, Relaxed);
        DRAIN_TO.store(0, Relaxed);
        HOLD_REACH.store(0, Relaxed);
        return 0;
    }
    if let Some(b) = burst {
        BURST.store(b.min(u64::MAX - 1), Relaxed);
    }
    let room = line.saturating_sub(read);
    set_room(room, limit);
    roll_window(crate::governor::decide_ms().max(1));
    // the takes net the reaper's landing bytes (`read` comes without them); the raises count them as used: the room they
    // make is not free until they land
    let landing = Relief::of(Source::ReaperCredit, landing);
    note_room_low(landing.raise_room(room));
    if ran {
        note_hold_reach(0);
        return 0;
    }
    // L6: the pool's bytes on their way are landing to the drain, never room (`set_room` above left them out)
    let now = crate::governor::decide_ms().max(1);
    let room = Relief::of(Source::BuffersLanding, buffers_landing_at(now)).ask_room(room);
    let marks = marks_now(limit);
    if room < marks.0 {
        // REACH (D4550): under LOW memory is short: the backends' per-session caches shed at their next statement
        crate::memory_short_now();
    }
    let to = rule::drain_to(room, marks, DRAIN_TO.load(Relaxed));
    DRAIN_TO.store(to, Relaxed);
    if to == 0 {
        // D4829: a budget under its protected floor comes back up first, ungated
        restore_floors();
        // C1c: past TOP and quiet since the last reclaim, the sized caches with demand get their budgets raised; between
        // HIGH and TOP nothing, and nothing while a take's bytes are still landing (nor ever with cache_raise = 0)
        let top = marks3_now(limit).2;
        let raises = mempolicy::consts::get(mempolicy::consts::K::CacheRaise) != 0;
        let quiet = raises && landing.is_none() && since_reclaim_ms() >= mempolicy::consts::get(mempolicy::consts::K::CacheRefillQuietMs);
        if raises {
            RAISE_GATE[GATE_SAMPLES].fetch_add(1, Relaxed);
            if !landing.is_none() {
                RAISE_GATE[GATE_LANDING].fetch_add(1, Relaxed);
            } else if !quiet {
                RAISE_GATE[GATE_UNQUIET].fetch_add(1, Relaxed);
            }
        }
        // the refault gate: not while the page cache pays for the anon a raise would add (its file pages refaulting)
        let paying = mempolicy::consts::get(mempolicy::consts::K::CacheRefaultGate) != 0
            && file_refaults_per_min().is_some_and(|r| !rule::refaults_allow_raise(r, limit));
        if quiet && paying {
            REFAULT_HELD.fetch_add(1, Relaxed);
        }
        let quiet = quiet && !paying;
        let low = room_low().unwrap_or(room);
        let claimed = claim_low().unwrap_or(u64::MAX);
        // C1b: the part cache's hold reaches its budget plus the room the raises left, none where no raise could give:
        // no room, or its raise's timing (`raise_timing_ok`, read before this sample's raises: a raise now is one it
        // let run)
        let t = crate::governor::decide_ms().max(1);
        let due = raise_timing_ok(SIZED_PART, t);
        let within = if quiet { rule::raise_room_within(room, low, marks.1, top, claimed) } else { 0 };
        if quiet && within < rule::C1_RAISE_STEP {
            RAISE_GATE[GATE_BOUND].fetch_add(1, Relaxed);
        }
        let left = raise_sized(within, t);
        // buf_grow: then shared_buffers, with what the cache raises left
        let left = grow_buffers(left, t);
        let knob = |k| mempolicy::consts::get(k) != 0;
        note_hold_reach(rule::hold_reach_room(left, due, knob(mempolicy::consts::K::CacheHoldReach), knob(mempolicy::consts::K::CacheHoldReachGate)));
        return 0;
    }
    note_hold_reach(0);
    RAISE_GATE[GATE_DRAINING].fetch_add(1, Relaxed);
    if reclaimable() < rule::C1_QUIET_MIN {
        // L6: nothing for R2a: the pool for the drain
        let _ = buffers_take(to - room, 0, now);
        C1[BG_QUIET].fetch_add(1, Relaxed);
        DRAIN_TO.store(0, Relaxed);
        return 0;
    }
    let ask = (to - room).min(rule::C1_BATCH_MAX);
    let freed = reclaim(ask);
    // L6: after R2a's batch, the pool for what it left and the caches cannot give (nothing above its minimum: no recount)
    if buffers().is_some_and(|b| (b.reclaimable)() != 0) {
        let _ = buffers_take(to.saturating_sub(room.saturating_add(freed)), reclaimable(), now);
    }
    C1[BG_RUNS].fetch_add(1, Relaxed);
    C1[BG_BYTES].fetch_add(freed, Relaxed);
    note_reclaimed(freed);
    if freed < ask || room.saturating_add(freed) >= to {
        DRAIN_TO.store(0, Relaxed);
    }
    freed
}

/// Ladder C1, the on-demand path: the bytes a draw of `ask` evicts inline before it takes them (`rule::inline_want`
/// over the room now: what keeps LOW free after it); 0 with C1 off or before the ladder's first sample. (Not the raises'
/// window low: an ask is a grant's reservation, often past what it uses; the v2 re-take noted room - ask here and read 0
/// for ~54 s, so no raise.)
#[inline]
pub fn inline_need(ask: u64) -> u64 {
    match room_now() {
        Some((room, limit)) => rule::inline_want(ask, room, marks_now(limit).0),
        None => 0,
    }
}

/// Ladder C1, the one evicter: the memory broker's background tick (B under its low mark, or a refused draw's demand)
/// freed `freed` through the ladder's R2a (the driver's `reclaim::tick_on` under C1): a background take, its room and
/// refill clock.
pub fn note_reclaimer_take(freed: u64) {
    C1[BG_RUNS].fetch_add(1, Relaxed);
    C1[BG_BYTES].fetch_add(freed, Relaxed);
    note_reclaimed(freed);
}

/// Ladder C1: a draw's inline eviction freed `freed` of the `want` it asked in `ns`.
pub fn note_inline(want: u64, freed: u64, ns: u64) {
    C1[IN_RUNS].fetch_add(1, Relaxed);
    C1[IN_BYTES].fetch_add(freed, Relaxed);
    C1[IN_NS].fetch_add(ns, Relaxed);
    C1[IN_NS_MAX].fetch_max(ns, Relaxed);
    if freed < want {
        C1[IN_SHORT].fetch_add(1, Relaxed);
    }
    note_reclaimed(freed);
}

/// Ladder C1, the refill guard: whether a cache may grow past its protected floor now (C1c: the headroom past TOP, and no
/// reclaim within `cache_refill_quiet_ms`; `rule::refill_open`). Always with C1 off or before the first sample.
pub fn refill_open() -> bool {
    let Some((room, limit)) = room_now() else { return true };
    let open = rule::refill_open(room, marks3_now(limit).2, since_reclaim_ms());
    if !open {
        C1[REFILL_CLOSED].fetch_add(1, Relaxed);
    }
    open
}

/// L6 (plan section 8): shared_buffers above its configured size, after R2a and before any cancel, on the ladder thread's
/// passes only: bufmgr's give-back hooks (D4649; all 0 with `buf_giveback` off), its `landing` never room.
pub struct Buffers {
    pub reclaimable: fn() -> u64,
    pub request: fn(u64) -> u64,
    pub landing: fn() -> u64,
    /// `buf_grow`: grow the pool by up to the bytes asked (whole extents, applied by the background writer): the bytes
    /// scheduled, 0 when off, at the reservation's top, or while a give-back drains.
    pub grow: fn(u64) -> u64,
    /// `buf_grow`: the bytes of the valid pages the pool evicted since start (only growing): its demand.
    pub evicted: fn() -> u64,
}

static BUFFERS: AtomicPtr<Buffers> = AtomicPtr::new(core::ptr::null_mut());
static SB_RUNS: AtomicU64 = AtomicU64::new(0);
static SB_BYTES: AtomicU64 = AtomicU64::new(0);
static SB_DUE_MS: AtomicU64 = AtomicU64::new(0);

pub fn install_buffers(b: &'static Buffers) {
    BUFFERS.store(b as *const Buffers as *mut Buffers, Release);
}

fn buffers() -> Option<&'static Buffers> {
    // SAFETY: only `install_buffers` stores here, always a `&'static Buffers`.
    unsafe { BUFFERS.load(Acquire).as_ref() }
}

pub fn buffers_counts() -> (u64, u64) {
    (SB_RUNS.load(Relaxed), SB_BYTES.load(Relaxed))
}

/// `buf_grow`, the pool's growth into free memory (the ladder's sample, its one writer): the evicted bytes last seen and
/// when they last moved (`decide_ms`; 0 = never), at its last grow, its last grow and the ladder's last take from it
/// (`buffers_take`; 0 = none), the doublings of its quiet period after a take (one per reversal: a take within
/// `C1_REVERSAL_MS` of a grow), and its grows and grown bytes since start.
static SB_EVICTED: AtomicU64 = AtomicU64::new(0);
static SB_EVICT_AT: AtomicU64 = AtomicU64::new(0);
static SB_GROW_BASE: AtomicU64 = AtomicU64::new(0);
static SB_GROW_AT: AtomicU64 = AtomicU64::new(0);
static SB_TAKE_AT: AtomicU64 = AtomicU64::new(0);
static SB_QUIET_SHIFT: AtomicU32 = AtomicU32::new(0);
static SB_GROWS: AtomicU64 = AtomicU64::new(0);
static SB_GROWN: AtomicU64 = AtomicU64::new(0);

/// `buf_grow`: the pool's grows and the bytes they scheduled since start (the view).
pub fn buffers_grow_counts() -> (u64, u64) {
    (SB_GROWS.load(Relaxed), SB_GROWN.load(Relaxed))
}

/// `buf_grow` (user 10-18: the buffer pool grows into free memory): after the cache raises, with the `room` they left
/// (past TOP over the window's low, quiet since the last reclaim; 0 otherwise), the pool grows by the bytes of the valid
/// pages it evicted since its last grow (`rule::raise_bytes`: a step at least, the room at most), once its quiet period
/// since the ladder's last take from it is over (`rule::refill_quiet_ms`, doubled per reversal) and one raise interval
/// after its last grow. Never a fill: the pool's pages are touched only as it reads into them. Every sample the evicted
/// bytes are read (their last move timed). The room the grow left.
fn grow_buffers(room: u64, now: u64) -> u64 {
    let Some(b) = buffers() else { return room };
    let evicted = (b.evicted)();
    if SB_EVICTED.swap(evicted, Relaxed) != evicted {
        SB_EVICT_AT.store(now, Relaxed);
    }
    if room == 0 || mempolicy::consts::get(mempolicy::consts::K::BufGrow) == 0 {
        return room;
    }
    let base = mempolicy::consts::get(mempolicy::consts::K::CacheRefillQuietMs);
    let taken = SB_TAKE_AT.load(Relaxed);
    let quiet_ms = rule::refill_quiet_ms(base, SB_QUIET_SHIFT.load(Relaxed));
    let grown = SB_GROW_AT.load(Relaxed);
    // a grow that lasted a whole quiet period with no take resets the backoff
    if grown != 0 && taken < grown && now.saturating_sub(grown) >= quiet_ms {
        SB_QUIET_SHIFT.store(0, Relaxed);
    }
    if (taken != 0 && now.saturating_sub(taken) < quiet_ms) || now.saturating_sub(grown) < rule::raise_interval_ms(0) {
        return room;
    }
    let evict_at = SB_EVICT_AT.load(Relaxed);
    let since = evicted.saturating_sub(SB_GROW_BASE.load(Relaxed));
    if since == 0 || evict_at == 0 || now.saturating_sub(evict_at) > rule::C1_DEMAND_MS {
        return room;
    }
    let step = rule::raise_bytes(since, room);
    let got = if step == 0 { 0 } else { (b.grow)(step) };
    if got == 0 {
        return room;
    }
    SB_GROW_BASE.store(evicted, Relaxed);
    SB_GROW_AT.store(now, Relaxed);
    SB_GROWS.fetch_add(1, Relaxed);
    SB_GROWN.fetch_add(got, Relaxed);
    room.saturating_sub(got)
}

/// L6: the pool's bytes still landing at `now` (`decide_ms`) within section 8's bound of the last request
/// (`rule::buffers_due_ms`); past it 0: a drain held past it counts as present and the pass climbs on.
pub fn buffers_landing_at(now: u64) -> u64 {
    match buffers() {
        Some(b) if now <= SB_DUE_MS.load(Relaxed) => (b.landing)(),
        _ => 0,
    }
}

/// L6: asks the pool for the `short` bytes R2a's `elastic` cannot cover (`rule::buffers_ask`): the bytes scheduled. A
/// drain landing past its bound is asked nothing more until it lands (the pass climbs on instead).
fn buffers_take(short: u64, elastic: u64, now: u64) -> u64 {
    let Some(b) = buffers() else { return 0 };
    if short <= elastic || ((b.landing)() != 0 && now > SB_DUE_MS.load(Relaxed)) {
        return 0;
    }
    let ask = rule::buffers_ask(short, elastic, (b.reclaimable)());
    let got = if ask == 0 { 0 } else { (b.request)(ask) };
    if got != 0 {
        SB_RUNS.fetch_add(1, Relaxed);
        SB_BYTES.fetch_add(got, Relaxed);
        // buf_grow: a take is the grows' quiet period from here; one within C1_REVERSAL_MS of a grow doubles it
        let grown = SB_GROW_AT.load(Relaxed);
        if grown != 0 && now.saturating_sub(grown) <= rule::C1_REVERSAL_MS {
            let shift = SB_QUIET_SHIFT.load(Relaxed);
            SB_QUIET_SHIFT.store(shift.saturating_add(1).min(20), Relaxed);
        }
        SB_TAKE_AT.store(now.max(1), Relaxed);
        let bw = mempolicy::consts::get(mempolicy::consts::K::BufGivebackWriteBw);
        SB_DUE_MS.store(now.saturating_add(rule::buffers_due_ms((b.landing)(), bw)), Relaxed);
    }
    got
}

#[cfg(test)]
mod tests {

    /// Ladder pressure with the ladder not on: the whole reading past the line; no line, never.
    #[test]
    fn pressed_at_reads_the_whole_reading_against_the_line() {
        assert!(!super::pressed_at(90, 100), "under the line");
        assert!(!super::pressed_at(100, 100), "at the line");
        assert!(super::pressed_at(120, 100), "past it");
        assert!(!super::pressed_at(120, 0), "no line, never");
    }

    /// The climb judges the whole read: with the arenas' reusable bytes far past the overage, state() still reports the
    /// full read and the full need (they are R1's relief, never headroom).
    #[test]
    fn state_reports_the_full_overage_with_reusable_bytes() {
        fn lots() -> u64 {
            1 << 40
        }
        let f = Fake::new(1200 * MIB, 0, 0, 0, 0, 0);
        set_reusable_probe(lots);
        let s = state(&f, 1000 * MIB, 0, Who::Asker, false);
        let reusable = reusable_bytes();
        REUSABLE.store(core::ptr::null_mut(), core::sync::atomic::Ordering::Release);
        assert_eq!(reusable, 1 << 40, "the probe is read");
        let s = s.expect("a read");
        assert_eq!(s.read, 1200 * MIB, "the whole read, nothing taken off");
        assert_eq!(rule::need(&s), 200 * MIB, "the full overage past the line");
    }

    use super::*;
    use core::cell::Cell;

    const MIB: u64 = 1 << 20;

    /// A fake server: the read falls by what the reclaim and the spills free.
    struct Fake {
        read: Cell<u64>,
        elastic: Cell<u64>,
        held: u64,
        fair: u64,
        spillable: Cell<u64>,
        others: Cell<u64>,
        /// what R1 returns when it runs (bytes off the read), and the clock
        r1: Cell<u64>,
        now: Cell<u64>,
        log: core::cell::RefCell<[u8; 8]>,
        n: Cell<usize>,
        /// R6: the cancels raised, one landing now, and the bytes a landed cancel frees
        cancels: Cell<u32>,
        landing: Cell<bool>,
        /// R2b: the protected cache bytes, freed off the read when evicted
        protected: Cell<u64>,
        /// D4404: the reaper's landing credit
        unmap: Cell<u64>,
        /// P3, R2p: the parked regions' and slot sets' bytes, freed off the read when trimmed
        parked: Cell<u64>,
        /// `ladder_kill_read`: the governed resident (`None` = the read)
        kill: Cell<Option<u64>>,
    }

    impl Fake {
        fn new(read: u64, elastic: u64, held: u64, fair: u64, spillable: u64, others: u64) -> Fake {
            Fake { read: Cell::new(read), elastic: Cell::new(elastic), held, fair, spillable: Cell::new(spillable), others: Cell::new(others), r1: Cell::new(0), now: Cell::new(0), log: core::cell::RefCell::new([0; 8]), n: Cell::new(0), cancels: Cell::new(0), landing: Cell::new(false), protected: Cell::new(0), unmap: Cell::new(0), parked: Cell::new(0), kill: Cell::new(None) }
        }
        fn mark(&self, r: Rung) {
            self.log.borrow_mut()[self.n.get()] = r as u8 + 1;
            self.n.set(self.n.get() + 1);
        }
        fn order(&self) -> [u8; 8] {
            *self.log.borrow()
        }
    }

    impl Rungs for Fake {
        fn read(&self) -> Option<u64> {
            Some(self.read.get())
        }
        fn kill_read(&self) -> Option<u64> {
            self.kill.get()
        }
        fn landing(&self) -> u64 {
            self.unmap.get()
        }
        fn reclaimable(&self) -> u64 {
            self.elastic.get()
        }
        fn reclaim(&self, bytes: u64) -> u64 {
            self.mark(Rung::EvictElastic);
            let n = bytes.min(self.elastic.get());
            self.elastic.set(self.elastic.get() - n);
            self.read.set(self.read.get() - n);
            n
        }
        fn asker(&self) -> (u64, u64, u64) {
            (self.held, self.fair, self.spillable.get())
        }
        fn revocable(&self) -> u64 {
            self.others.get()
        }
        fn own_spill(&self, bytes: u64) {
            self.mark(Rung::OwnSpill);
            let n = bytes.min(self.spillable.get());
            self.spillable.set(self.spillable.get() - n);
            self.read.set(self.read.get() - n);
        }
        fn revoke(&self, _bytes: u64) {
            // an ask sent frees nothing until it lands: no relief now
            self.mark(Rung::Revoke);
        }
        fn alloc_return(&self) {
            self.mark(Rung::AllocReturn);
            self.read.set(self.read.get() - self.r1.get());
        }
        fn now_ms(&self) -> u64 {
            self.now.get()
        }
        fn cancel_in_flight(&self) -> bool {
            self.landing.get()
        }
        fn cancel(&self, _need: u64) -> bool {
            self.cancels.set(self.cancels.get() + 1);
            self.landing.set(true);
            true
        }
        fn protected(&self) -> u64 {
            self.protected.get()
        }
        fn evict_protected(&self, bytes: u64) -> u64 {
            let n = bytes.min(self.protected.get());
            self.protected.set(self.protected.get() - n);
            self.read.set(self.read.get() - n);
            n
        }
        fn parked(&self) -> u64 {
            self.parked.get()
        }
        fn trim_parked(&self, bytes: u64) -> u64 {
            self.mark(Rung::TrimParked);
            let n = bytes.min(self.parked.get());
            self.parked.set(self.parked.get() - n);
            self.read.set(self.read.get() - n);
            n
        }
    }

    const R2B: u16 = 1 << Rung::EvictProtected as u8;

    #[test]
    fn past_the_wait_the_protected_cache_goes_before_anyone_is_canceled() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        clear_narrow();
        r1_reset();
        hook_reset();
        // 40 MB past the line, nothing elastic, 512 MiB of protected cache
        let f = Fake::new(1040 * MIB, 0, 0, 0, 0, 0);
        f.protected.set(512 * MIB);
        f.now.set(40_000);
        assert_eq!(hook_pass_with(&f, 1000 * MIB, 40_000).ran & (R2B | R6), 0, "the protected cache stays through the pass that ran R1-R4b");
        let c = hook_pass_with(&f, 1000 * MIB, 40_000 + rule::HOOK_GRACE_MS);
        assert_eq!((c.ran & (R2B | R6), c.need, f.cancels.get()), (R2B, 0, 0), "past the wait R2b covers the need: no cancel");
        assert_eq!(f.protected.get(), 472 * MIB, "R2b evicts the need, no more");
        hook_reset();
        clear_narrow();
    }

    /// `ladder_kill_read` (USER 10-18): past its wait the hook's climb cancels no one while the unreclaimable bytes sit
    /// under the line (the read past it by mapped file pages): R2b still evicts on the read and the need stays, so the
    /// hook climbs on; once the unreclaimable bytes cross, R6 as before.
    #[test]
    fn past_the_wait_r6_waits_for_the_unreclaimable_bytes() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        clear_narrow();
        r1_reset();
        hook_reset();
        let f = Fake::new(1040 * MIB, 0, 0, 0, 0, 0);
        f.protected.set(16 * MIB);
        f.kill.set(Some(600 * MIB));
        f.now.set(10_000);
        assert_eq!(hook_pass_with(&f, 1000 * MIB, 10_000).ran & (R2B | R6), 0, "the pass that ran R1-R4b");
        let c = hook_pass_with(&f, 1000 * MIB, 10_000 + rule::HOOK_GRACE_MS);
        assert_eq!((c.ran & (R2B | R6), f.cancels.get()), (R2B, 0), "past the wait: R2b on the read, no cancel under the line");
        assert_eq!(c.need, 24 * MIB, "the read's need stays: the hook climbs on");
        f.kill.set(Some(1030 * MIB));
        let c = hook_pass_with(&f, 1000 * MIB, 10_000 + 2 * rule::HOOK_GRACE_MS);
        assert_eq!((c.ran & R6, f.cancels.get()), (R6, 1), "the unreclaimable bytes past the line (at most the read): R6");
        hook_reset();
        clear_narrow();
    }

    fn hook_reset() {
        HOOK_SINCE.store(0, Relaxed);
        HOOK_READ.store(0, Relaxed);
        STARVED.store(false, Relaxed);
        PAST_CANCEL.store(false, Relaxed);
    }

    const R6: u16 = 1 << Rung::Cancel as u8;

    #[test]
    fn the_ladder_thread_cancels_past_its_one_tick_wait_one_at_a_time() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        clear_narrow();
        r1_reset();
        hook_reset();
        // the rehearsal's episode 6: 40 MB past the line, nothing the caches can free, nothing revocable, no gate asking
        let f = Fake::new(1040 * MIB, 0, 0, 0, 0, 0);
        f.now.set(10_000);
        let (runs0, _) = counts();
        let c = hook_pass_with(&f, 1000 * MIB, 10_000);
        assert_eq!((c.ran & R6, c.need), (0, 40 * MIB), "the pass that ran R1-R4b cancels no one");
        assert_eq!(hook_pass_with(&f, 1000 * MIB, 10_000 + rule::HOOK_GRACE_MS - 1).ran & R6, 0, "within the tick");
        let c = hook_pass_with(&f, 1000 * MIB, 10_000 + rule::HOOK_GRACE_MS);
        assert_eq!((c.ran & R6, f.cancels.get()), (R6, 1), "a tick on with the need still there: R6, on the ladder's line, before the watchdog's");
        assert_eq!(hook_pass_with(&f, 1000 * MIB, 10_300).ran & R6, 0, "one cancel in flight");
        assert_eq!(f.cancels.get(), 1);
        // the victim's bytes leave: under the line, the episode ends and R5 is counted with its measured yield
        f.landing.set(false);
        f.read.set(900 * MIB);
        assert_eq!(hook_pass_with(&f, 1000 * MIB, 10_400), Climbed::default());
        let (runs1, _) = counts();
        assert_eq!(runs1[Rung::Cancel as usize].0, runs0[Rung::Cancel as usize].0 + 1, "R6's run is counted");
        assert_eq!(runs1[Rung::Wait as usize].0, runs0[Rung::Wait as usize].0 + 1, "R5's run is counted once");
        assert_eq!(HOOK_SINCE.load(Relaxed), 0);
        clear_narrow();
    }

    /// D4404 (i): bytes the reaper is unmapping are landing to the ladder thread's pass (no rung, no episode) while their
    /// credit lasts; once it expires (the reaper held past `cancel_landing_ms`) they count as present and the pass climbs:
    /// its one-tick wait, then R6. A gate's climb never counts them.
    #[test]
    fn the_reapers_unmaps_are_landing_to_the_pass_until_their_credit_expires() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        clear_narrow();
        r1_reset();
        hook_reset();
        // 50 MiB past the line, nothing elastic, nothing revocable; the reaper owes 64 MiB
        let f = Fake::new(1050 * MIB, 0, 0, 0, 0, 0);
        f.unmap.set(64 * MIB);
        f.now.set(50_000);
        assert_eq!(hook_pass_with(&f, 1000 * MIB, 50_000), Climbed::default(), "covered by the reaper's landing: no rung, no episode");
        assert_eq!(HOOK_SINCE.load(Relaxed), 0);
        assert_eq!(climb_with(&f, 1000 * MIB, 0).need, 50 * MIB, "a gate's climb: the reaper's bytes are not room");
        f.unmap.set(0);
        let c = hook_pass_with(&f, 1000 * MIB, 50_100);
        assert_eq!((c.ran & R6, c.need), (0, 50 * MIB), "the credit expired: the pass climbs (R1-R4b, then its wait)");
        let c = hook_pass_with(&f, 1000 * MIB, 50_100 + rule::HOOK_GRACE_MS);
        assert_eq!((c.ran & R6, f.cancels.get()), (R6, 1), "a tick on: R6");
        f.unmap.set(30 * MIB);
        f.landing.set(false);
        f.read.set(1050 * MIB);
        hook_reset();
        assert_eq!(hook_pass_with(&f, 1000 * MIB, 60_000).need, 20 * MIB, "a credit short of the need: the pass climbs for the rest");
        hook_reset();
        clear_narrow();
    }

    /// Kill safety (a stress arm OOM-killed under reuse): past the watchdog's cancel line (`waive_wait`) the
    /// reaper's credit for a canceled hog's bytes, which the next hog reused, counts for nothing: the pass reaches R6.
    /// Under the line the same credit covers the need. The flag lasts one pass.
    #[test]
    fn past_the_cancel_line_the_reapers_credit_holds_off_no_rung() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        clear_narrow();
        r1_reset();
        hook_reset();
        publish(ON, 1000 * MIB);
        // 50 MiB past the line, nothing elastic or revocable; the reaper owes 468 MiB
        let f = Fake::new(1050 * MIB, 0, 0, 0, 0, 0);
        f.unmap.set(468 * MIB);
        f.now.set(80_000);
        assert_eq!(hook_pass_with(&f, 1000 * MIB, 80_000), Climbed::default(), "under the cancel line: the credit covers the need");
        waive_wait();
        assert!(past_cancel());
        let c = hook_pass_with(&f, 1000 * MIB, 80_100);
        assert_eq!((c.ran & R6, f.cancels.get()), (R6, 1), "past the cancel line: R6 in this pass, the credit counts for nothing");
        assert!(!past_cancel(), "one pass only");
        publish(OFF, 0);
        hook_reset();
        clear_narrow();
    }

    #[test]
    fn an_episode_the_earlier_rungs_close_within_the_tick_cancels_no_one() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        clear_narrow();
        r1_reset();
        hook_reset();
        let f = Fake::new(1040 * MIB, 0, 0, 0, 0, 0);
        assert_ne!(hook_pass_with(&f, 1000 * MIB, 20_000).need, 0);
        f.read.set(990 * MIB);
        assert_eq!(hook_pass_with(&f, 1000 * MIB, 20_000 + rule::HOOK_GRACE_MS).need, 0);
        f.read.set(1040 * MIB);
        assert_eq!(hook_pass_with(&f, 1000 * MIB, 20_000 + 2 * rule::HOOK_GRACE_MS).ran & R6, 0, "a new episode waits its own tick");
        assert_eq!(f.cancels.get(), 0);
        clear_narrow();
    }

    static HAND_READ: AtomicU64 = AtomicU64::new(0);
    fn hand_read() -> Option<u64> {
        Some(HAND_READ.load(Relaxed))
    }
    fn zero() -> u64 {
        0
    }
    fn none(_: u64) -> u64 {
        0
    }
    fn no_cancel(_: u64) -> bool {
        false
    }
    fn idle() -> bool {
        false
    }
    static HAND_ACTS: Acts = Acts { read: hand_read, reclaimable: zero, reclaim: none, cancel: no_cancel, cancel_in_flight: idle, protected: zero, evict_protected: none };

    #[test]
    fn a_spent_wait_hands_r2b_and_r6_to_the_ladder_thread() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        clear_narrow();
        r1_reset();
        hook_reset();
        install(&HAND_ACTS);
        publish(ON, 1000 * MIB);
        HAND_READ.store(990 * MIB, Relaxed);
        assert!(!hand_off(8 * MIB), "the ask fits under the line: only the pool is short, nothing to hand off");
        assert!(hand_off(64 * MIB), "past the line with the ask in: the ladder thread takes R2b and R6");
        // its next pass is past the wait at once: no tick of its own
        let f = Fake::new(1040 * MIB, 0, 0, 0, 0, 0);
        let c = hook_pass_with(&f, 1000 * MIB, 30_000);
        assert_eq!((c.ran & R6, f.cancels.get()), (R6, 1));
        publish(OFF, 0);
        hook_reset();
        clear_narrow();
    }

    /// Ladder step 1.1 (`gate_now`): an asker's hand-off is relief on its way until one ladder pass has read it, and a
    /// cancel that pass raised keeps it on its way while it lands; with no victim, nothing is.
    #[test]
    fn a_hand_off_is_relief_on_its_way_until_a_pass_has_read_it() {
        use mempolicy::consts::{set, K};
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        clear_narrow();
        r1_reset();
        hook_reset();
        install(&HAND_ACTS);
        publish(ON, 1000 * MIB);
        HAND_READ.store(990 * MIB, Relaxed);
        set(K::GateNow, 1);
        assert!(!relief_coming(None), "nothing asked, nothing landing");
        let at = passes();
        assert!(hand_off(64 * MIB));
        assert!(relief_coming(Some(at)), "handed off: the ladder thread's next pass is relief on its way");
        let f = Fake::new(1040 * MIB, 0, 0, 0, 0, 0);
        hook_pass_with(&f, 1000 * MIB, 40_000);
        assert_eq!(passes(), at + 1);
        assert!(!relief_coming(Some(at)), "the pass read it and the actuators have no cancel landing: nothing on its way");
        set(K::GateNow, 0);
        let at = passes();
        hook_pass_with(&f, 1000 * MIB, 40_100);
        assert_eq!(passes(), at, "the knob off: no pass counted");
        publish(OFF, 0);
        hook_reset();
        clear_narrow();
    }

    const R1: u16 = 1 << Rung::AllocReturn as u8;

    fn r1_reset() {
        R1_AT.store(0, Relaxed);
    }

    /// The process-wide flag and counters: the tests take turns.
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // The purger's pressure mode leaves where R2a's take stops, for the same sample: one mark, with or without a measured
    // burst, never a second copy of it.
    #[test]
    fn the_purgers_exit_is_r2as_take_target_for_the_same_sample() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let (limit, line) = (1536 * MIB, 1228 * MIB);
        publish(ON, line);
        BURST.store(u64::MAX, Relaxed);
        for burst in [None, Some(64 * MIB), Some(70 * MIB)] {
            // the sample: the background pass at this line and limit (its hook's pass ran a rung, so it takes nothing)
            let _ = background_pass_with(&|_| 0, &|| 0, limit, line, line, true, burst);
            let marks = marks_now(limit);
            assert!(marks.1 > 140 * MIB && marks.1 < 155 * MIB, "HIGH at 1536M: {} MiB", marks.1 >> 20);
            for read in [line - marks.1 - MIB, line - marks.1, line - marks.1 + MIB, line, line + 10 * MIB] {
                let room = line.saturating_sub(Relief::of(Source::ReaperCredit, unmap_landing()).ask_read(read));
                let r2a_done = rule::drain_to(room, marks, marks.1) == 0;
                assert_eq!(at_take_target(read), r2a_done, "read {} MiB, burst {:?}: the purger's exit is R2a's stop", read >> 20, burst);
            }
        }
        BURST.store(u64::MAX, Relaxed);
    }

    const R2P: u16 = 1 << Rung::TrimParked as u8;
    const R2A: u16 = 1 << Rung::EvictElastic as u8;

    /// P3 (`parked_trim_rung`): under the knob a climb trims the parked regions and slot sets (R2p) after R1 and before
    /// R2a, up to the need, and R2a takes only what is left; born red while the climb was never handed the parked bytes.
    #[test]
    fn p3_under_the_knob_r2p_trims_the_parked_bytes_up_to_the_need_before_r2a() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let _trim = crate::tests::TRIM_TURN.lock().unwrap_or_else(|e| e.into_inner());
        clear_narrow();
        r1_reset();
        mempolicy::consts::set(mempolicy::consts::K::ParkedTrimRung, 1);
        // 40 MiB past the line, 30 MiB parked, 512 MiB of elastic cache
        let f = Fake::new(1040 * MIB, 512 * MIB, 0, 0, 0, 0);
        f.parked.set(30 * MIB);
        let c = climb_with(&f, 1000 * MIB, 0);
        // the parked bytes cover the need: R2a stays out
        let g = Fake::new(1040 * MIB, 512 * MIB, 0, 0, 0, 0);
        g.parked.set(512 * MIB);
        let d = climb_with(&g, 1000 * MIB, 0);
        mempolicy::consts::set(mempolicy::consts::K::ParkedTrimRung, 0);
        assert_eq!(f.order()[..3], [Rung::AllocReturn as u8 + 1, Rung::TrimParked as u8 + 1, Rung::EvictElastic as u8 + 1], "R1, R2p, then R2a");
        assert_eq!((c.ran, c.need), (R1 | R2P | R2A, 0));
        assert_eq!((f.parked.get(), f.elastic.get()), (0, 502 * MIB), "R2p trims every parked byte, R2a the 10 MiB left");
        assert_eq!((d.ran, d.need), (R1 | R2P, 0), "R2p covered the need: no cache evicted");
        assert_eq!((g.parked.get(), g.elastic.get()), (472 * MIB, 512 * MIB), "R2p trims the need, no more");
        clear_narrow();
    }

    /// P3: with the knob off (the default) the climb is today's: no R2p whatever is parked (R2a's own depot step, in the
    /// driver, takes the parked bytes).
    #[test]
    fn p3_with_the_knob_off_no_r2p_runs_whatever_is_parked() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let _trim = crate::tests::TRIM_TURN.lock().unwrap_or_else(|e| e.into_inner());
        clear_narrow();
        r1_reset();
        let f = Fake::new(1040 * MIB, 512 * MIB, 0, 0, 0, 0);
        f.parked.set(30 * MIB);
        let c = climb_with(&f, 1000 * MIB, 0);
        assert_eq!(c.ran, R1 | R2A);
        assert_eq!(f.parked.get(), 30 * MIB, "nothing trimmed by R2p");
        clear_narrow();
    }

    /// P3: R2p's trim (the live rung's `trim_parked`): on the climbing thread with no give-back service; with the service
    /// in place a job for it instead (its wake sent, nothing trimmed on the climbing thread, the bytes the ladder's landing
    /// credit until the service's pass runs the job on its own thread); born red while the trim did nothing.
    #[test]
    fn p3_r2p_trims_on_the_climbing_thread_or_as_the_give_back_services_job() {
        use core::sync::atomic::AtomicU64;
        static TRIMMED: AtomicU64 = AtomicU64::new(0);
        static WAKES: AtomicU64 = AtomicU64::new(0);
        fn parked() -> u64 {
            64 * MIB
        }
        fn trim(b: u64, _job: bool) -> u64 {
            TRIMMED.fetch_add(b, Relaxed);
            b
        }
        fn wake() {
            WAKES.fetch_add(1, Relaxed);
        }
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        set_parked_trim(parked, trim);
        PARKED_WAKE.store(core::ptr::null_mut(), Release);
        let c0 = parked_trim_counts();
        assert_eq!(trim_parked(8 * MIB), 8 * MIB, "no service: trimmed on this thread");
        assert_eq!(TRIMMED.load(Relaxed), 8 * MIB);
        set_parked_service(wake);
        let landing0 = unmap_landing();
        let posted = trim_parked(16 * MIB);
        let (trimmed, wakes, pending, landing) = (TRIMMED.load(Relaxed), WAKES.load(Relaxed), parked_job_pending(), unmap_landing());
        let ran = run_parked_job();
        let after = (TRIMMED.load(Relaxed), parked_job_pending());
        PARKED_WAKE.store(core::ptr::null_mut(), Release);
        PARKED_ASK.store(0, Relaxed);
        let c1 = parked_trim_counts();
        assert_eq!((posted, trimmed, wakes), (0, 8 * MIB, 1), "with the service: a job posted and its wake sent, nothing trimmed here");
        assert_eq!((pending, landing), (16 * MIB, landing0 + 16 * MIB), "the job's bytes are landing until it runs");
        assert_eq!((ran, after), (16 * MIB, (24 * MIB, 0)), "the service's pass ran the job");
        assert_eq!([c1[0] - c0[0], c1[1] - c0[1], c1[2] - c0[2], c1[3] - c0[3], c1[4] - c0[4], c1[5] - c0[5]], [1, 8 * MIB, 1, 16 * MIB, 1, 16 * MIB], "counted");
    }

    /// P3 with H3 step (4): R2p's trim is told where it runs. On the climbing thread it is no job (it waits for the reaper
    /// outside the ladder's pass, as R2a's depot step); run by the give-back service it is the service's job, so the trim
    /// never waits on the service's thread: not for the reaper, not for the service's own whole-region job lane (whose
    /// trim-for-a-draw wait, `giveback::drain_here`, would wait for this very thread up to give_back_spill_wait_ms). Born
    /// red while the service's job ran its trim as no job.
    #[test]
    fn p3_the_give_back_services_r2p_job_trims_as_its_job() {
        use core::sync::atomic::{AtomicBool, AtomicU64};
        static JOB: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
        static LAST: AtomicBool = AtomicBool::new(false);
        fn parked() -> u64 {
            64 * MIB
        }
        fn trim(b: u64, job: bool) -> u64 {
            JOB[job as usize].fetch_add(1, Relaxed);
            LAST.store(job, Relaxed);
            b
        }
        fn wake() {}
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        set_parked_trim(parked, trim);
        PARKED_WAKE.store(core::ptr::null_mut(), Release);
        PARKED_ASK.store(0, Relaxed);
        assert_eq!(trim_parked(4 * MIB), 4 * MIB);
        let inline = (JOB[0].load(Relaxed), JOB[1].load(Relaxed), LAST.load(Relaxed));
        set_parked_service(wake);
        assert_eq!(trim_parked(4 * MIB), 0, "posted");
        let ran = run_parked_job();
        let job = (JOB[0].load(Relaxed), JOB[1].load(Relaxed), LAST.load(Relaxed));
        PARKED_WAKE.store(core::ptr::null_mut(), Release);
        PARKED_ASK.store(0, Relaxed);
        assert_eq!(inline, (1, 0, false), "on the climbing thread: no job");
        assert_eq!(ran, 4 * MIB);
        assert_eq!(job, (1, 1, true), "the service's pass ran it as its job");
    }

    #[test]
    fn a_climb_stops_at_the_first_rung_whose_measured_relief_covers_the_need() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        clear_narrow();
        r1_reset();
        // 100 MiB past the line once the 50 MiB ask is in; the caches hold 512 MiB elastic
        let f = Fake::new(1050 * MIB, 512 * MIB, 0, 0, 0, 0);
        let c = climb_with(&f, 1000 * MIB, 50 * MIB);
        assert_eq!(c.ran, R1 | 1 << Rung::EvictElastic as u8, "R1 returned nothing, R2a covered it: no narrowing, no spill, no revoke");
        assert_eq!((c.relief, c.need), (100 * MIB, 0));
        assert!(!narrowed());
    }

    #[test]
    fn past_the_caches_the_scans_narrow_then_the_asker_spills_its_own_then_others_are_asked() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        clear_narrow();
        r1_reset();
        // the asker holds 3 GiB against a 1 GiB fair share with 64 MiB spillable; others hold 1 GiB spillable
        let f = Fake::new(1500 * MIB, 16 * MIB, 3072 * MIB, 1024 * MIB, 64 * MIB, 1024 * MIB);
        let c = climb_with(&f, 1000 * MIB, 0);
        let r = |x: Rung| x as u8 + 1;
        assert_eq!(f.order()[..4], [r(Rung::AllocReturn), r(Rung::EvictElastic), r(Rung::OwnSpill), r(Rung::Revoke)], "R1, R2a, (R3 sets its flag), R4a, R4b");
        assert!(narrowed(), "R3 ran");
        assert_eq!(c.ran, R1 | (1 << Rung::EvictElastic as u8) | (1 << Rung::NarrowScans as u8) | (1 << Rung::OwnSpill as u8) | (1 << Rung::Revoke as u8));
        assert_eq!(c.relief, 80 * MIB, "measured: 16 MiB evicted + 64 MiB spilled; the revoke's ask counts nothing yet");
        assert_eq!(c.need, 420 * MIB, "left for the wait (R5)");
        clear_narrow();
    }

    #[test]
    fn within_its_fair_share_the_asker_does_not_spill_itself() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        clear_narrow();
        r1_reset();
        let f = Fake::new(1500 * MIB, 0, 512 * MIB, 1024 * MIB, 256 * MIB, 0);
        let c = climb_with(&f, 1000 * MIB, 0);
        assert_eq!(c.ran, R1 | 1 << Rung::NarrowScans as u8, "nothing elastic, under its share, nothing revocable: R1 and R3 only");
        assert_eq!(f.order()[..2], [Rung::AllocReturn as u8 + 1, 0], "no spill, no revoke");
        clear_narrow();
    }

    #[test]
    fn under_the_line_or_off_nothing_climbs() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        clear_narrow();
        r1_reset();
        let f = Fake::new(500 * MIB, 512 * MIB, 0, 0, 0, 0);
        assert_eq!(climb_with(&f, 1000 * MIB, 64 * MIB), Climbed::default());
        assert_eq!(climb_with(&f, 0, 64 * MIB), Climbed::default(), "no line published yet");
        publish(OFF, 1000 * MIB);
        assert_eq!(climb(1 << 40), Climbed::default(), "off: the gate's climb does nothing");
        assert_eq!(f.n.get(), 0);
    }

    #[test]
    fn a_climb_that_ran_moves_the_free_epoch() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        clear_narrow();
        r1_reset();
        let e = epoch();
        let f = Fake::new(1100 * MIB, 512 * MIB, 0, 0, 0, 0);
        climb_with(&f, 1000 * MIB, 0);
        assert!(epoch() > e);
    }

    #[test]
    fn r1_runs_first_and_a_return_that_covers_the_need_ends_the_climb() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        clear_narrow();
        r1_reset();
        let f = Fake::new(1050 * MIB, 512 * MIB, 0, 0, 0, 0);
        f.r1.set(120 * MIB);
        f.now.set(10_000);
        let (runs0, _) = counts();
        let c = climb_with(&f, 1000 * MIB, 50 * MIB);
        assert_eq!(c.ran, R1, "the allocators' retained pages covered it: the caches keep their bytes");
        assert_eq!((c.relief, c.need), (120 * MIB, 0));
        assert_eq!(f.elastic.get(), 512 * MIB);
        let (runs1, _) = counts();
        assert_eq!(runs1[Rung::AllocReturn as usize].0, runs0[Rung::AllocReturn as usize].0 + 1, "R1's run is counted");
    }

    #[test]
    fn after_a_small_return_r1_cools_for_its_window_then_runs_again() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        clear_narrow();
        r1_reset();
        let f = Fake::new(1100 * MIB, 4096 * MIB, 0, 0, 0, 0);
        f.r1.set(2 * MIB);
        f.now.set(10_000);
        assert_eq!(climb_with(&f, 1000 * MIB, 0).ran, R1 | 1 << Rung::EvictElastic as u8, "2 MiB back: R2a covers the rest");
        f.read.set(1100 * MIB);
        f.now.set(10_500);
        assert_eq!(climb_with(&f, 1000 * MIB, 0).ran, 1 << Rung::EvictElastic as u8, "within the window after a small yield: R1 cools");
        f.read.set(1100 * MIB);
        f.now.set(11_000 + rule::R1_COOL_MS);
        assert_eq!(climb_with(&f, 1000 * MIB, 0).ran, R1 | 1 << Rung::EvictElastic as u8, "past the window: R1 runs again");
        r1_reset();
    }

    static C1_HELD: AtomicU64 = AtomicU64::new(0);
    fn c1_held() -> u64 {
        C1_HELD.load(Relaxed)
    }
    static C1_WAKES: AtomicU64 = AtomicU64::new(0);
    fn c1_wake() {
        C1_WAKES.fetch_add(1, Relaxed);
    }

    /// Ladder C1 (`cache_floor`, ladder on) at an 8 GiB limit (LOW 409.6 MiB, HIGH 819.2 MiB): under LOW the ladder's
    /// thread reclaims to HIGH; a draw evicts what keeps LOW free after it, counting what the pool drew since the sample;
    /// the refill guard closes at or under HIGH and for a second after a reclaim. Off: nothing published, nothing evicted.
    #[test]
    fn c1_the_background_pass_takes_the_room_to_high_and_a_draw_keeps_low() {
        use mempolicy::consts::{set, K};
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let gib = 1u64 << 30;
        let limit = 8 * gib;
        let (low, high) = rule::marks(limit);
        set_pool_held(c1_held);
        crate::set_purge_wake(c1_wake);
        C1_HELD.store(0, Relaxed);
        let asked = std::cell::Cell::new(0u64);
        let reclaim = |n: u64| {
            asked.set(n);
            n
        };
        publish(ON, 7 * gib);
        assert_eq!(background_pass(limit, 7 * gib, 7 * gib - low / 2, false, None), 0, "cache_floor off: nothing");
        assert_eq!(room_now(), None);
        set(K::CacheFloor, 1);
        let before = c1_counts();
        let epoch = crate::memory_short_epoch();
        assert_eq!(background_pass_with(&reclaim, &|| u64::MAX, limit, 7 * gib, 7 * gib - low, false, None), 0, "at LOW: nothing");
        assert_eq!(crate::memory_short_epoch(), epoch, "at LOW: not short, REACH's epoch stays");
        assert_eq!(inline_need(gib), gib, "a 1 GiB draw with LOW left: it evicts the GiB");
        assert_eq!(background_pass_with(&reclaim, &|| u64::MAX, limit, 7 * gib, 7 * gib - low / 2, true, None), 0, "the hook's pass ran a rung: its read is stale");
        assert_eq!(background_pass_with(&reclaim, &|| u64::MAX, limit, 7 * gib, 7 * gib - low / 2, false, None), high - low / 2, "under LOW: to HIGH");
        assert_eq!(crate::memory_short_epoch(), epoch + 1, "under LOW (C1's trigger): REACH's epoch moved, once (not on the stale read)");
        assert_eq!(asked.get(), high - low / 2);
        assert_eq!(room_now(), Some((high, limit)));
        let after = c1_counts();
        assert_eq!((after.bg_runs - before.bg_runs, after.bg_bytes - before.bg_bytes), (1, high - low / 2));
        assert_eq!(C1_WAKES.load(Relaxed), 1, "the reclaim woke the purger");
        assert!(!refill_open(), "within a second of the reclaim (and at HIGH)");
        C1_HELD.store(gib / 4, Relaxed);
        assert_eq!(room_now(), Some((high - gib / 4, limit)), "the pool drew a quarter GiB since the sample");
        assert_eq!(inline_need(gib / 8), 0, "LOW stays free after it");
        assert_eq!(inline_need(gib), gib + low - (high - gib / 4));
        note_inline(gib, gib / 2, 1_500_000);
        let c = c1_counts();
        assert_eq!((c.inline_runs - after.inline_runs, c.inline_bytes - after.inline_bytes, c.inline_short - after.inline_short), (1, gib / 2, 1));
        assert_eq!(room_now(), Some((high + gib / 4, limit)), "the inline eviction's bytes are room");
        RECLAIM_AT_MS.store(0, Relaxed);
        assert!(!refill_open(), "C1c: past HIGH but under TOP (HIGH + 5 %): closed");
        C1_HELD.store(0, Relaxed);
        assert!(refill_open(), "past TOP, no reclaim yet");
        set(K::CacheFloor, 0);
        assert_eq!(room_now(), None, "off: no room, no draw evicts");
        assert_eq!(inline_need(64 * gib), 0);
        assert!(refill_open());
        publish(OFF, 0);
        ROOM.store(u64::MAX, Relaxed);
        C1_HELD.store(0, Relaxed);
    }

    static C1_STUCK: AtomicU64 = AtomicU64::new(0);
    fn c1_stuck() -> u64 {
        C1_STUCK.load(Relaxed)
    }

    /// Ladder C1 at a 100 GiB limit (the broker's 10-16 note): LOW (5 GiB before a burst is measured) to HIGH (10 GiB) in
    /// batches of at most 2 GiB, one a sample, the drain going on past LOW until HIGH or a short batch; LOW is then the
    /// measured burst; the give-back rate moves with each timed reclaim of 64 MiB or more; the stuck cache bytes are
    /// re-read at each sample (0 with C1 off) and their fall moves the free epoch.
    #[test]
    fn c1_a_drain_takes_batches_of_2gib_low_is_the_burst_and_the_stuck_bytes_follow() {
        use mempolicy::consts::{set, K};
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let gib = 1u64 << 30;
        let mib = 1u64 << 20;
        let limit = 100 * gib;
        let line = 90 * gib;
        assert_eq!(rule::marks(limit), (5 * gib, 10 * gib));
        set(K::CacheFloor, 1);
        publish(ON, line);
        let asked = std::cell::RefCell::new(std::vec::Vec::new());
        let half = std::cell::Cell::new(false);
        let reclaim = |n: u64| {
            asked.borrow_mut().push(n);
            if half.get() { n / 2 } else { n }
        };
        // the room 1 GiB, then as each batch left it
        for room in [1, 3, 5, 7, 9, 10] {
            background_pass_with(&reclaim, &|| u64::MAX, limit, line, line - room * gib, false, None);
        }
        assert_eq!(*asked.borrow(), [2 * gib, 2 * gib, 2 * gib, 2 * gib, gib], "2 GiB a sample, on past LOW, to HIGH");
        asked.borrow_mut().clear();
        // a short batch ends the drain: the caches are at their floors
        half.set(true);
        background_pass_with(&reclaim, &|| u64::MAX, limit, line, line - gib, false, None);
        background_pass_with(&reclaim, &|| u64::MAX, limit, line, line - 6 * gib, false, None);
        assert_eq!(*asked.borrow(), [2 * gib], "past LOW after a short batch: no drain");
        half.set(false);
        asked.borrow_mut().clear();
        // quiet when there is nothing to take: no walk, counted apart, the drain ended
        let q0 = c1_counts().bg_quiet;
        background_pass_with(&reclaim, &|| rule::C1_QUIET_MIN - 1, limit, line, line - gib, false, None);
        background_pass_with(&reclaim, &|| u64::MAX, limit, line, line - 6 * gib, false, None);
        assert!(asked.borrow().is_empty(), "nothing reclaimable: no walk, and past LOW no drain");
        assert_eq!(c1_counts().bg_quiet - q0, 1);
        // LOW the burst (at least MARGIN_MIN, at most 5 % of the limit); no new measure keeps the last
        background_pass_with(&reclaim, &|| u64::MAX, limit, line, line - gib, false, Some(300 * mib));
        assert_eq!(burst(), Some(300 * mib));
        assert_eq!(marks_now(limit), (300 * mib, 300 * mib + 5 * gib));
        assert!(asked.borrow().is_empty(), "1 GiB of room is past a 300 MiB LOW");
        background_pass_with(&reclaim, &|| u64::MAX, limit, line, line - 200 * mib, false, None);
        assert_eq!(*asked.borrow(), [300 * mib + 5 * gib - 200 * mib].map(|w| w.min(2 * gib)), "under the burst's LOW: to its HIGH");
        assert_eq!(burst(), Some(300 * mib), "kept");
        // the give-back rate: the prior, then a quarter of the way to each timed reclaim's
        let before = c1_counts();
        assert_eq!(give_back_ns_per_gib(), rule::GIVE_BACK_NS_PER_GIB_PRIOR);
        note_give_back(mib, 1_000_000_000);
        assert_eq!(give_back_ns_per_gib(), rule::GIVE_BACK_NS_PER_GIB_PRIOR, "too small to time");
        note_give_back(gib, 80_000_000);
        assert_eq!(give_back_ns_per_gib(), 33_000_000 + 20_000_000);
        let after = c1_counts();
        assert_eq!((after.give_back_runs - before.give_back_runs, after.give_back_bytes - before.give_back_bytes, after.give_back_ns - before.give_back_ns), (1, gib, 80_000_000));
        // the stuck cache bytes: re-read at the sample (stuck_refresh_ms 0: every sample), 0 with C1 off; a fall moves the
        // free epoch
        set_caches_stuck(c1_stuck);
        set(K::StuckRefreshMs, 0);
        C1_STUCK.store(3 * gib, Relaxed);
        refresh_stuck();
        assert_eq!(caches_stuck(), 3 * gib);
        let e = epoch();
        C1_STUCK.store(gib, Relaxed);
        background_pass(limit, line, line - 20 * gib, false, None);
        assert_eq!(caches_stuck(), gib, "re-read at the ladder's sample");
        assert!(epoch() > e, "fewer stuck bytes are room: the draws wake");
        // D4893: past HIGH the walk waits stuck_refresh_ms since the last read (60 s here: no host stall reaches it);
        // under HIGH every sample
        set(K::StuckRefreshMs, 60_000);
        C1_STUCK.store(2 * gib, Relaxed);
        background_pass(limit, line, line - 20 * gib, false, None);
        assert_eq!(caches_stuck(), gib, "past HIGH, read just now: not re-read");
        set(K::StuckRefreshMs, 1_000);
        let (at, high) = (STUCK_AT_MS.load(Relaxed), marks_now(limit).1);
        assert!(!stuck_due(high + gib, high, at + 999) && stuck_due(high + gib, high, at + 1_000), "past HIGH: once a second");
        assert!(stuck_due(high - 1, high, at), "under HIGH: every sample");
        set(K::StuckRefreshMs, 0);
        assert!(stuck_due(high + gib, high, at), "0: every sample, as before");
        set(K::StuckRefreshMs, 1_000);
        STUCK_AT_MS.store(0, Relaxed);
        set(K::CacheFloor, 0);
        refresh_stuck();
        assert_eq!(caches_stuck(), 0, "C1 off: none");
        publish(OFF, 0);
        ROOM.store(u64::MAX, Relaxed);
        DRAIN_TO.store(0, Relaxed);
        BURST.store(u64::MAX, Relaxed);
        GIVE_BACK_NS_PER_GIB.store(0, Relaxed);
    }

    /// Ladder C1b at an 8 GiB limit: a sized cache's budget is its bound until the ladder takes bytes from it, then what
    /// it held after; it grows back by the room past HIGH (one batch at most, split) once the refill guard opens (a
    /// second after the last reclaim), and is back at its bound when it gets there. Off: always the bound.
    #[test]
    fn c1b_the_ladder_sizes_a_cache_down_on_a_take_and_raises_it_past_top_with_demand() {
        use mempolicy::consts::{set, K};
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let gib = 1u64 << 30;
        let mib = 1u64 << 20;
        let step = rule::C1_RAISE_STEP;
        let limit = 8 * gib;
        let line = 7 * gib;
        let (_, high, top) = marks3_now(limit);
        assert_eq!(top, high + rule::gap(limit));
        let none = |_: u64| 0u64;
        assert_eq!(sized_budget(SIZED_PART, gib), gib, "off: the start");
        set(K::CacheFloor, 1);
        publish(ON, line);
        assert_eq!(sized_budget(SIZED_PART, gib), gib, "nothing taken: the start");
        assert_eq!(sized_budget(SIZED_FP, 2 * gib), 2 * gib);
        set_sized_probe(SIZED_PART, part_probe);
        PROBE_HELD.store(300 * mib, Relaxed);
        PROBE_EVICTS.store(7, Relaxed);
        note_taken_at(SIZED_PART, 300 * mib, 1_000);
        assert_eq!(sized_budget(SIZED_PART, gib), 300 * mib, "the ladder took bytes: what it held after");
        // the take's own trim is not demand; the cache evicting at its budget is
        let t = 20_000;
        raise_sized(gib, t);
        assert_eq!(sized_budget(SIZED_PART, gib), 300 * mib, "no eviction of its own since the take: no raise");
        PROBE_EVICTS.store(8, Relaxed);
        raise_sized(0, t);
        assert_eq!(sized_budget(SIZED_PART, gib), 300 * mib, "no room past TOP: no raise");
        raise_sized(gib, t + 100);
        assert_eq!(sized_budget(SIZED_PART, gib), 300 * mib + step, "demand and room: one step");
        raise_sized(gib, t + 1_000);
        assert_eq!(sized_budget(SIZED_PART, gib), 300 * mib + step, "no eviction since the raise: none (never a fill)");
        PROBE_HELD.store(300 * mib + step, Relaxed);
        PROBE_EVICTS.store(9, Relaxed);
        raise_sized(gib, t + 1_100);
        assert_eq!(sized_budget(SIZED_PART, gib), 300 * mib + 2 * step, "it filled the raise and evicted: the next step");
        assert_eq!(sized_counts()[SIZED_PART], (2, 2 * step, 0));
        // a take within 10 s of the raise: a reversal, and the next raise waits twice as long
        note_taken_at(SIZED_PART, 300 * mib, t + 1_150);
        assert_eq!(sized_counts()[SIZED_PART].2, 1, "a reversal");
        PROBE_EVICTS.store(10, Relaxed);
        raise_sized(gib, t + 1_250);
        assert_eq!(sized_budget(SIZED_PART, gib), 300 * mib, "the backoff: 150 ms since the last raise, under its 200");
        raise_sized(gib, t + 1_300);
        assert_eq!(sized_budget(SIZED_PART, gib), 300 * mib, "the refill backoff: 150 ms since the take, under its doubled 2 s");
        raise_sized(gib, t + 3_150);
        assert_eq!(sized_budget(SIZED_PART, gib), 300 * mib + step, "2 s after the take: one step");
        // through the background pass: between HIGH and TOP nothing; past TOP a raise; within a second of a reclaim none
        PROBE_EVICTS.store(12, Relaxed);
        SIZED_RAISE_AT[SIZED_PART].store(0, Relaxed);
        SIZED_EVICT_AT[SIZED_PART].store(0, Relaxed);
        SIZED_REVERSAL_AT[SIZED_PART].store(0, Relaxed);
        SIZED_RECENT[SIZED_PART].store(0, Relaxed);
        SIZED_TAKEN_AT[SIZED_PART].store(0, Relaxed);
        SIZED_CALM_FROM[SIZED_PART].store(0, Relaxed);
        SIZED_QUIET_SHIFT[SIZED_PART].store(0, Relaxed);
        RECLAIM_AT_MS.store(0, Relaxed);
        // (the raises' window: the first pass starts it)
        ROOM_LOW_AT.store(0, Relaxed);
        // the pass reads decide's clock: past one raise interval from its start
        while crate::governor::decide_ms() < 2 * rule::TICK_MS {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        background_pass_with(&none, &|| u64::MAX, limit, line, line - top, false, None);
        assert_eq!(sized_budget(SIZED_PART, gib), 300 * mib + step, "at TOP (between HIGH and TOP): nothing");
        assert_eq!(part_hold_reach(), 300 * mib + step, "C1b: at TOP the hold reaches the budget (a step past the floor bounds it)");
        RECLAIM_AT_MS.store(crate::governor::decide_ms().max(1), Relaxed);
        background_pass_with(&none, &|| u64::MAX, limit, line, line - top - gib, false, None);
        assert_eq!(sized_budget(SIZED_PART, gib), 300 * mib + step, "within a second of a reclaim: nothing");
        assert_eq!(part_hold_reach(), 300 * mib + step, "C1b: no raise could give: the budget");
        RECLAIM_AT_MS.store(0, Relaxed);
        background_pass_with(&none, &|| u64::MAX, limit, line, line - high - step + 1, false, None);
        assert_eq!(sized_budget(SIZED_PART, gib), 300 * mib + step, "under TOP: nothing");
        // C1d v2: the window's lowest headroom bounds a raise: a step under HIGH plus a step within it, so none
        background_pass_with(&none, &|| u64::MAX, limit, line, line - top - gib, false, None);
        assert_eq!(room_low(), Some(high + step - 1));
        assert_eq!(sized_budget(SIZED_PART, gib), 300 * mib + step, "past TOP, but the window's low under HIGH + a step: nothing");
        // a window later (its halves forgotten) the room the low held back is the caches' again, but not while a take's
        // bytes are still landing (the reaper's unmaps, which the pass's read comes without): the raises count them as used
        ROOM_LOW_AT.store(0, Relaxed);
        set(K::CacheRaise, 0);
        background_pass_with(&none, &|| u64::MAX, limit, line, line - top - gib, false, None);
        assert_eq!(sized_budget(SIZED_PART, gib), 300 * mib + step, "cache_raise = 0: never a raise");
        set(K::CacheRaise, 1);
        // the refault gate (on here, off by default): the page cache refaulting 10 % of the limit a minute holds it
        set(K::CacheRefaultGate, 1);
        no_file_refaults();
        assert_eq!(file_refaults_per_min(), None, "no cgroup: no count");
        note_file_refaults(0, 1);
        note_file_refaults(limit / 10, 60_001);
        assert_eq!(file_refaults_per_min(), Some(limit / 10), "the last whole minute's");
        let held = refault_held();
        background_pass_with(&none, &|| u64::MAX, limit, line, line - top - gib, false, None);
        assert_eq!(sized_budget(SIZED_PART, gib), 300 * mib + step, "the page cache paying: nothing");
        assert_eq!(refault_held(), held + 1);
        note_file_refaults(limit / 10 + limit / 100, 90_001);
        assert_eq!(file_refaults_per_min(), Some(limit / 10), "the minute so far is under the last whole one");
        note_file_refaults(limit / 10 + limit / 100, 120_001);
        assert_eq!(file_refaults_per_min(), Some(limit / 100), "a quiet minute after it");
        no_file_refaults();
        TEST_LANDING.store(gib, Relaxed);
        // pppress card 15: a hook whose bytes hold the pool's landing says so, and the claimer's landing counts the pool's
        // once: not again with that hook, added with the plain one
        set_unmap_landing_with_pool(test_landing);
        assert!(unmap_landing_has_pool());
        assert_eq!(claimer_landing(5, 7), unmap_landing() + 5, "the hook holds the pool's landing: not again");
        set_unmap_landing(test_landing);
        assert!(!unmap_landing_has_pool());
        assert_eq!(claimer_landing(5, 7), unmap_landing() + 12, "a hook without it: the pool's landing added");
        background_pass_with(&none, &|| u64::MAX, limit, line, line - top - gib, false, None);
        assert_eq!(room_low(), Some(top), "the landing bytes are used in the raises' window");
        assert_eq!(sized_budget(SIZED_PART, gib), 300 * mib + step, "past TOP with demand, a take's bytes landing: nothing");
        TEST_LANDING.store(0, Relaxed);
        ROOM_LOW_AT.store(0, Relaxed);
        // (the refault gate off, the default: the page cache refaulting does not hold it)
        note_file_refaults(0, 1);
        note_file_refaults(limit / 10, 60_001);
        set(K::CacheRefaultGate, 0);
        background_pass_with(&none, &|| u64::MAX, limit, line, line - top - gib, false, None);
        no_file_refaults();
        assert_eq!(room_low(), Some(top + gib));
        assert_eq!(sized_budget(SIZED_PART, gib), 300 * mib + 2 * step, "landed: past TOP with demand, the window's low too: a step");
        assert_eq!(part_hold_reach(), 300 * mib + step + (top + gib - high), "C1b: the budget plus the room a raise could give then");
        // C1b: the raises' one timing rule bounds the reach: within the part cache's raise interval (a backoff after
        // reversals) or a take's refill quiet period no raise could come, so the hold reaches only the budget
        let clocks = [SIZED_RAISE_AT[SIZED_PART].load(Relaxed), SIZED_RECENT[SIZED_PART].load(Relaxed) as u64, SIZED_TAKEN_AT[SIZED_PART].load(Relaxed), SIZED_QUIET_SHIFT[SIZED_PART].load(Relaxed) as u64];
        let budget_now = sized_budget(SIZED_PART, gib);
        ROOM_LOW_AT.store(0, Relaxed);
        SIZED_RAISE_AT[SIZED_PART].store(crate::governor::decide_ms().max(1), Relaxed);
        SIZED_RECENT[SIZED_PART].store(rule::C1_BACKOFF_MAX, Relaxed);
        background_pass_with(&none, &|| u64::MAX, limit, line, line - top - gib, false, None);
        assert_eq!(part_hold_reach(), budget_now, "C1b: within the raise backoff the hold reaches only the budget");
        ROOM_LOW_AT.store(0, Relaxed);
        set(K::CacheHoldReachGate, 0);
        background_pass_with(&none, &|| u64::MAX, limit, line, line - top - gib, false, None);
        assert_eq!(part_hold_reach(), budget_now + (top + gib - high), "H5a: the timing gate off: the reach within the backoff too");
        assert_eq!(sized_budget(SIZED_PART, gib), budget_now, "H5a: and still no raise: the gate's knob is the reach's only");
        ROOM_LOW_AT.store(0, Relaxed);
        set(K::CacheHoldReach, 0);
        background_pass_with(&none, &|| u64::MAX, limit, line, line - top - gib, false, None);
        assert_eq!(part_hold_reach(), budget_now, "H5a: the reach off: the budget");
        set(K::CacheHoldReach, 1);
        set(K::CacheHoldReachGate, 1);
        SIZED_RAISE_AT[SIZED_PART].store(clocks[0], Relaxed);
        SIZED_RECENT[SIZED_PART].store(clocks[1] as u32, Relaxed);
        ROOM_LOW_AT.store(0, Relaxed);
        SIZED_TAKEN_AT[SIZED_PART].store(crate::governor::decide_ms().max(1), Relaxed);
        SIZED_QUIET_SHIFT[SIZED_PART].store(rule::C1_BACKOFF_MAX, Relaxed);
        background_pass_with(&none, &|| u64::MAX, limit, line, line - top - gib, false, None);
        assert_eq!(part_hold_reach(), budget_now, "C1b: within a take's refill quiet period the hold reaches only the budget");
        assert_eq!(sized_budget(SIZED_PART, gib), budget_now, "and no raise either: one timing rule");
        SIZED_TAKEN_AT[SIZED_PART].store(clocks[2], Relaxed);
        SIZED_QUIET_SHIFT[SIZED_PART].store(clocks[3] as u32, Relaxed);
        // C1d: the start (the derived budget) is no cap: free past TOP and demand raise the budget past it
        SIZED_BUDGET[SIZED_PART].store(u64::MAX, Relaxed);
        PROBE_HELD.store(gib, Relaxed);
        PROBE_EVICTS.store(13, Relaxed);
        let t2 = crate::governor::decide_ms().max(1) + 60_000;
        raise_sized(2 * gib, t2);
        assert_eq!(sized_budget(SIZED_PART, gib), gib + step, "at its start, at budget and evicting, free past TOP: past the derived budget");
        PROBE_HELD.store(gib + step, Relaxed);
        PROBE_EVICTS.store(14, Relaxed);
        raise_sized(2 * gib, t2 + 200);
        assert_eq!(sized_budget(SIZED_PART, gib), gib + 2 * step, "and on, while free stays past TOP");
        // C1d: a raise gives back what the cache evicted itself since its last raise: 3 GiB evicted, 2 GiB of room
        let before = sized_counts()[SIZED_PART];
        PROBE_HELD.store(gib + 2 * step, Relaxed);
        PROBE_EVICTS.store(14 + 3 * gib, Relaxed);
        raise_sized(2 * gib, t2 + 400);
        assert_eq!(sized_budget(SIZED_PART, gib), 3 * gib + 2 * step, "the bytes it evicted, within the room");
        assert_eq!(sized_counts()[SIZED_PART], (before.0 + 1, before.1 + 2 * gib, before.2), "one raise of 2 GiB");
        PROBE_HELD.store(3 * gib + 2 * step, Relaxed);
        PROBE_EVICTS.store(15 + 3 * gib, Relaxed);
        raise_sized(2 * gib, t2 + 600);
        assert_eq!(sized_budget(SIZED_PART, gib), 3 * gib + 3 * step, "evicted since that raise: a byte, so a step");
        // C1d v2: only the window's evictions: 3 GiB since that raise, 1 GiB of them since the window's older half began
        PROBE_HELD.store(3 * gib + 3 * step, Relaxed);
        PROBE_EVICTS.store(15 + 6 * gib, Relaxed);
        SIZED_EVICT_WIN[SIZED_PART][1].store(15 + 5 * gib, Relaxed);
        raise_sized(2 * gib, t2 + 800);
        assert_eq!(sized_budget(SIZED_PART, gib), 4 * gib + 3 * step, "the window's GiB, not the 3 since the raise");
        // C1b: a GiB held past the budget once the floor fell back under it is demand (no eviction since the raise)
        PROBE_HELD.store(5 * gib + 3 * step, Relaxed);
        note_sized_floor(SIZED_PART, 6 * gib);
        raise_sized(4 * gib, t2 + 1_000);
        assert_eq!(sized_budget(SIZED_PART, gib), 4 * gib + 3 * step, "while the floor holds it: no raise");
        note_sized_floor(SIZED_PART, gib);
        raise_sized(4 * gib, t2 + 1_200);
        assert_eq!(sized_budget(SIZED_PART, gib), 5 * gib + 3 * step, "the floor back under it: the bytes held past it");
        note_sized_floor(SIZED_PART, 0);
        // C1d: a take sets the budget to what it left (at least the bytes the trim cannot drop: in use, and the images part
        // lists hold); the floor the ladder's sample reads never lifts it, so no cut follows when it falls back (D4715)
        note_sized_floor(SIZED_PART, 3 * gib);
        note_taken_at(SIZED_PART, 700 * mib, t2 + 60_000);
        assert_eq!(sized_budget(SIZED_PART, gib), 700 * mib, "what the take left, not a stale sample's floor");
        note_taken_at(SIZED_PART, 900 * mib, t2 + 60_100);
        assert_eq!(sized_budget(SIZED_PART, gib), 700 * mib, "a take that leaves the cache past its budget: never up");
        note_sized_floor(SIZED_PART, 2 * gib);
        assert_eq!(sized_budget(SIZED_PART, gib), 700 * mib, "a floor risen since (a statement's parts held): no lift");
        note_sized_floor(SIZED_PART, 0);
        assert_eq!(sized_budget(SIZED_PART, gib), 700 * mib, "and none to cut when it falls back");
        SIZED_BUDGET[SIZED_PART].store(u64::MAX, Relaxed);
        note_sized_floor(SIZED_PART, 2 * gib);
        assert_eq!(sized_budget(SIZED_PART, gib), gib, "no take yet: the start, whatever the floor");
        note_sized_floor(SIZED_PART, 0);
        set(K::CacheFloor, 0);
        note_taken(SIZED_PART, 0);
        assert_eq!(sized_budget(SIZED_PART, gib), gib, "off: the start, and a take notes nothing");
        publish(OFF, 0);
        ROOM.store(u64::MAX, Relaxed);
        DRAIN_TO.store(0, Relaxed);
        HOLD_REACH.store(0, Relaxed);
        SIZED_PROBE[SIZED_PART].store(core::ptr::null_mut(), Release);
        for c in 0..SIZED_N {
            SIZED_BUDGET[c].store(u64::MAX, Relaxed);
            SIZED_START[c].store(u64::MAX, Relaxed);
            SIZED_FLOOR[c].store(0, Relaxed);
            SIZED_RAISE_AT[c].store(0, Relaxed);
            SIZED_EVICT_AT[c].store(0, Relaxed);
            SIZED_EVICT_BASE[c].store(0, Relaxed);
            SIZED_REVERSAL_AT[c].store(0, Relaxed);
            SIZED_RECENT[c].store(0, Relaxed);
            SIZED_TAKEN_AT[c].store(0, Relaxed);
            SIZED_CALM_FROM[c].store(0, Relaxed);
            SIZED_QUIET_SHIFT[c].store(0, Relaxed);
            SIZED_EVICT_WIN[c].iter().for_each(|w| w.store(0, Relaxed));
        }
        ROOM_LOW.iter().for_each(|r| r.store(u64::MAX, Relaxed));
        ROOM_LOW_AT.store(0, Relaxed);
    }

    /// Ladder C1c, why no raise: each held raise counts its reason (room under a step, timing, no demand), and the view's
    /// backoff reads the shift a reversal doubled. A take restarts the refill quiet and keeps the shift: only a raise that
    /// lasted a whole quiet period resets it.
    #[test]
    fn c1c_a_held_raise_counts_why_and_the_backoff_reads_its_shift() {
        use mempolicy::consts::{set, K};
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let (gib, mib) = (1u64 << 30, 1u64 << 20);
        set(K::CacheFloor, 1);
        publish(ON, 7 * gib);
        assert_eq!(sized_budget(SIZED_PART, gib), gib);
        set_sized_probe(SIZED_PART, part_probe);
        PROBE_HELD.store(300 * mib, Relaxed);
        PROBE_EVICTS.store(1, Relaxed);
        // the counts since start are the process's: deltas here, and put back at the end for the tests that read them whole
        let counts0 = sized_counts()[SIZED_PART];
        note_taken_at(SIZED_PART, 300 * mib, 1_000);
        let held = |w: usize| raise_held()[SIZED_PART][w];
        let (room0, timing0, demand0) = (held(HELD_ROOM), held(HELD_TIMING), held(HELD_DEMAND));
        PROBE_EVICTS.store(2, Relaxed);
        raise_sized(0, 1_100);
        assert_eq!(held(HELD_ROOM), room0 + 1, "no room: held by its room");
        raise_sized(gib, 1_200);
        assert_eq!(held(HELD_TIMING), timing0 + 1, "200 ms after the take, under the 1 s quiet: held by its timing");
        raise_sized(gib, 20_000);
        assert_eq!(sized_counts()[SIZED_PART].0, counts0.0 + 1, "quiet, evicting, room: a raise");
        raise_sized(gib, 22_000);
        assert_eq!(held(HELD_DEMAND), demand0 + 1, "no eviction since the raise: held by no demand");
        // a take within 10 s of the raise: a reversal doubles the quiet; later takes keep the shift
        note_taken_at(SIZED_PART, 300 * mib, 25_000);
        assert_eq!(sized_counts()[SIZED_PART].2, counts0.2 + 1, "a reversal");
        assert_eq!(SIZED_QUIET_SHIFT[SIZED_PART].load(Relaxed), 1, "a reversal: shift 1");
        note_taken_at(SIZED_PART, 300 * mib, 50_000);
        assert_eq!(SIZED_QUIET_SHIFT[SIZED_PART].load(Relaxed), 1, "a take with no raise before it: the shift stays");
        let (shift, quiet, _) = sized_quiet(SIZED_PART);
        assert_eq!((shift, quiet), (1, 2 * mempolicy::consts::get(K::CacheRefillQuietMs)), "the view: shift 1, the quiet doubled");
        SIZED_RAISES[SIZED_PART].store(counts0.0, Relaxed);
        SIZED_RAISED[SIZED_PART].store(counts0.1, Relaxed);
        SIZED_REVERSALS[SIZED_PART].store(counts0.2, Relaxed);
        set(K::CacheFloor, 0);
        publish(OFF, 0);
        ROOM.store(u64::MAX, Relaxed);
        DRAIN_TO.store(0, Relaxed);
        SIZED_PROBE[SIZED_PART].store(core::ptr::null_mut(), Release);
        for c in 0..SIZED_N {
            SIZED_BUDGET[c].store(u64::MAX, Relaxed);
            SIZED_START[c].store(u64::MAX, Relaxed);
            SIZED_FLOOR[c].store(0, Relaxed);
            SIZED_RAISE_AT[c].store(0, Relaxed);
            SIZED_EVICT_AT[c].store(0, Relaxed);
            SIZED_EVICT_BASE[c].store(0, Relaxed);
            SIZED_REVERSAL_AT[c].store(0, Relaxed);
            SIZED_RECENT[c].store(0, Relaxed);
            SIZED_TAKEN_AT[c].store(0, Relaxed);
            SIZED_CALM_FROM[c].store(0, Relaxed);
            SIZED_QUIET_SHIFT[c].store(0, Relaxed);
            SIZED_EVICT_WIN[c].iter().for_each(|w| w.store(0, Relaxed));
        }
        ROOM_LOW.iter().for_each(|r| r.store(u64::MAX, Relaxed));
        ROOM_LOW_AT.store(0, Relaxed);
    }

    /// D4829 (`cache_floor_hold`) at an 8 GiB limit: a budget under the cache's protected floor comes back up to it at the
    /// next pass with no drain to run, whatever the raises' window and clocks (a take a moment ago, no eviction, no room
    /// past TOP); not while the pass drains (under LOW), not past the cancel line, never with the knob off, never for a
    /// cache with no floor noted or at its start. Only the budget moves: the raises' counts and clocks are untouched.
    #[test]
    fn d4829_a_budget_under_its_protected_floor_comes_back_to_it_ungated() {
        use mempolicy::consts::{set, K};
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let gib = 1u64 << 30;
        let mib = 1u64 << 20;
        let limit = 8 * gib;
        let line = 7 * gib;
        let (low, high, _) = marks3_now(limit);
        let none = |_: u64| 0u64;
        set(K::CacheFloor, 1);
        // the read the cancel line is judged on: the ladder's own (`read_now`, the acts' read)
        install(&HAND_ACTS);
        HAND_READ.store(0, Relaxed);
        publish(ON, line);
        let _ = sized_budget(SIZED_PART, 4 * gib);
        let _ = sized_budget(SIZED_FP, 2 * gib);
        let raises = sized_counts();
        DRAIN_TO.store(0, Relaxed);
        // a take just now left the part cache's budget at 64 MiB under its 1 GiB floor; the fp tables have no floor noted
        let now = crate::governor::decide_ms().max(1);
        note_taken_at(SIZED_PART, 64 * mib, now);
        note_sized_protect(SIZED_PART, gib);
        let between = line - high - mib;
        set(K::CacheFloorHold, 0);
        background_pass_with(&none, &|| u64::MAX, limit, line, between, false, None);
        assert_eq!(sized_budgets()[SIZED_PART], 64 * mib, "off: the raises alone (none: no demand, no room past TOP)");
        set(K::CacheFloorHold, 1);
        background_pass_with(&none, &|| u64::MAX, limit, line, line - low / 2, false, None);
        assert_eq!(sized_budgets()[SIZED_PART], 64 * mib, "under LOW the pass drains: no restore");
        DRAIN_TO.store(0, Relaxed);
        publish_cancel_line(6 * gib);
        HAND_READ.store(6 * gib + 1, Relaxed);
        background_pass_with(&none, &|| u64::MAX, limit, line, between, false, None);
        assert_eq!(sized_budgets()[SIZED_PART], 64 * mib, "past the cancel line: no restore");
        HAND_READ.store(6 * gib, Relaxed);
        background_pass_with(&none, &|| u64::MAX, limit, line, between, false, None);
        assert_eq!(sized_budgets()[SIZED_PART], gib, "between HIGH and TOP, a take a moment ago, no demand: back to the floor");
        assert_eq!(sized_budgets()[SIZED_FP], u64::MAX, "no floor noted: at its start");
        assert_eq!(sized_restores()[SIZED_PART], (1, gib - 64 * mib), "one restore, counted");
        assert_eq!(sized_counts(), raises, "no raise counted");
        assert_eq!(SIZED_TAKEN_AT[SIZED_PART].load(Relaxed), now, "the take's clock untouched");
        background_pass_with(&none, &|| u64::MAX, limit, line, between, false, None);
        assert_eq!(sized_restores()[SIZED_PART].0, 1, "at the floor: nothing more");
        // a budget past the floor is the raises' and the takes': a restore never lowers it
        note_sized_protect(SIZED_PART, gib / 2);
        background_pass_with(&none, &|| u64::MAX, limit, line, between, false, None);
        assert_eq!(sized_budgets()[SIZED_PART], gib, "past a lower floor: unchanged");
        publish_cancel_line(0);
        HAND_READ.store(0, Relaxed);
        note_sized_protect(SIZED_PART, 0);
        set(K::CacheFloor, 0);
        publish(OFF, 0);
        for c in 0..SIZED_N {
            SIZED_BUDGET[c].store(u64::MAX, Relaxed);
            SIZED_START[c].store(u64::MAX, Relaxed);
            SIZED_TAKEN_AT[c].store(0, Relaxed);
            SIZED_RESTORES[c].store(0, Relaxed);
            SIZED_RESTORED[c].store(0, Relaxed);
        }
    }

    /// Ladder C1, the refill backoff: a raise the ladder took back within 10 s (a reversal) doubles the cache's refill
    /// quiet period, counted from the take; a second reversal doubles it again; a raise that then lasts a whole quiet
    /// period with no take resets it, and a later take waits the base period only.
    #[test]
    fn c1_refill_backoff_doubles_per_reversal_and_a_calm_raise_resets_it() {
        use mempolicy::consts::{set, K};
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let (mib, gib, step) = (1u64 << 20, 1u64 << 30, rule::C1_RAISE_STEP);
        let c = SIZED_PART;
        // the view's counters are cumulative and another test reads them whole: put them back at the end
        let counts0: [[u64; 3]; SIZED_N] = core::array::from_fn(|i| [SIZED_RAISES[i].load(Relaxed), SIZED_RAISED[i].load(Relaxed), SIZED_REVERSALS[i].load(Relaxed)]);
        set(K::CacheFloor, 1);
        set(K::CacheRefillQuietMs, 1_000);
        publish(ON, 7 * gib);
        set_sized_probe(c, part_probe);
        assert_eq!(sized_budget(c, gib), gib, "the start");
        // demand before each raise: at its budget, and an eviction of its own since the last raise
        let evicts = AtomicU64::new(100);
        let demand = || {
            PROBE_HELD.store(500 * mib, Relaxed);
            PROBE_EVICTS.store(evicts.fetch_add(1, Relaxed) + 1, Relaxed);
        };
        let budget = || sized_budget(c, gib);
        let t = 100_000;
        PROBE_HELD.store(500 * mib, Relaxed);
        PROBE_EVICTS.store(100, Relaxed);
        note_taken_at(c, 500 * mib, t);
        demand();
        raise_sized(gib, t + 500);
        assert_eq!(budget(), 500 * mib, "half a second after a take: under the base quiet period");
        raise_sized(gib, t + 1_000);
        assert_eq!(budget(), 500 * mib + step, "the base second after it: a step");
        note_taken_at(c, 500 * mib, t + 2_000);
        assert_eq!(SIZED_QUIET_SHIFT[c].load(Relaxed), 1, "taken back within 10 s: a reversal, the quiet period doubled");
        demand();
        raise_sized(gib, t + 3_500);
        assert_eq!(budget(), 500 * mib, "1.5 s after the take: under the doubled 2 s");
        raise_sized(gib, t + 4_000);
        assert_eq!(budget(), 500 * mib + step, "2 s after it: a step");
        note_taken_at(c, 500 * mib, t + 5_000);
        demand();
        raise_sized(gib, t + 8_000);
        assert_eq!(budget(), 500 * mib, "a second reversal: 3 s after the take, under 4 s");
        raise_sized(gib, t + 9_000);
        assert_eq!(budget(), 500 * mib + step, "4 s after it: a step");
        raise_sized(gib, t + 12_999);
        assert_eq!(SIZED_QUIET_SHIFT[c].load(Relaxed), 2, "the raise not yet a whole quiet period old");
        raise_sized(gib, t + 13_000);
        assert_eq!(SIZED_QUIET_SHIFT[c].load(Relaxed), 0, "the raise lasted a whole 4 s with no take: reset");
        note_taken_at(c, 500 * mib, t + 20_000);
        assert_eq!(SIZED_QUIET_SHIFT[c].load(Relaxed), 0, "a take 11 s after the raise: no reversal");
        demand();
        raise_sized(gib, t + 21_000);
        assert_eq!(budget(), 500 * mib + step, "the base second again");
        assert_eq!(rule::refill_quiet_ms(1_000, 20), rule::C1_QUIET_MAX_MS, "never past 10 min");
        set(K::CacheFloor, 0);
        publish(OFF, 0);
        SIZED_PROBE[c].store(core::ptr::null_mut(), Release);
        for c in 0..SIZED_N {
            SIZED_BUDGET[c].store(u64::MAX, Relaxed);
            SIZED_START[c].store(u64::MAX, Relaxed);
            SIZED_RAISE_AT[c].store(0, Relaxed);
            SIZED_EVICT_AT[c].store(0, Relaxed);
            SIZED_EVICT_BASE[c].store(0, Relaxed);
            SIZED_REVERSAL_AT[c].store(0, Relaxed);
            SIZED_RECENT[c].store(0, Relaxed);
            SIZED_TAKEN_AT[c].store(0, Relaxed);
            SIZED_CALM_FROM[c].store(0, Relaxed);
            SIZED_QUIET_SHIFT[c].store(0, Relaxed);
            SIZED_RAISES[c].store(counts0[c][0], Relaxed);
            SIZED_RAISED[c].store(counts0[c][1], Relaxed);
            SIZED_REVERSALS[c].store(counts0[c][2], Relaxed);
        }
    }

    /// Ladder C1d v2: the window a raise looks back over (`C1_DEMAND_MS`, in two halves): the lowest headroom the samples
    /// and the draws saw stays for 30-60 s, then the caches' again; a sample after a quiet spell forgets both halves, and
    /// the evictions before it.
    #[test]
    fn c1d_v2_the_raise_window_keeps_a_low_for_one_window_then_forgets_it() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let gib = 1u64 << 30;
        let half = rule::C1_DEMAND_MS / 2;
        let win = || [SIZED_EVICT_WIN[SIZED_PART][0].load(Relaxed), SIZED_EVICT_WIN[SIZED_PART][1].load(Relaxed)];
        ROOM_LOW_AT.store(0, Relaxed);
        ROOM_LOW.iter().for_each(|r| r.store(u64::MAX, Relaxed));
        set_sized_probe(SIZED_PART, part_probe);
        PROBE_EVICTS.store(5 * gib, Relaxed);
        let t = 1_000_000;
        roll_window(t);
        assert_eq!((room_low(), win()), (None, [5 * gib, 5 * gib]), "a first sample: nothing low, and the evictions before it are not the window's");
        note_room_low(10 * gib);
        note_room_low(2 * gib);
        note_room_low(9 * gib);
        roll_window(t + half - 1);
        assert_eq!(room_low(), Some(2 * gib), "the lowest seen, in the same half");
        PROBE_EVICTS.store(6 * gib, Relaxed);
        roll_window(t + half);
        note_room_low(9 * gib);
        assert_eq!((room_low(), win()), (Some(2 * gib), [6 * gib, 5 * gib]), "30 s on: the last half's low and evictions still count");
        PROBE_EVICTS.store(7 * gib, Relaxed);
        roll_window(t + 2 * half);
        note_room_low(9 * gib);
        assert_eq!((room_low(), win()), (Some(9 * gib), [7 * gib, 6 * gib]), "60 s on: the room the low held back is the caches' again");
        roll_window(t + 5 * half);
        assert_eq!((room_low(), win()), (None, [7 * gib, 7 * gib]), "after a quiet spell: both halves forgotten");
        SIZED_PROBE[SIZED_PART].store(core::ptr::null_mut(), Release);
        SIZED_EVICT_WIN.iter().for_each(|w| w.iter().for_each(|x| x.store(0, Relaxed)));
        ROOM_LOW.iter().for_each(|r| r.store(u64::MAX, Relaxed));
        ROOM_LOW_AT.store(0, Relaxed);
    }

    /// The map-time low: a fresh map notes the headroom it leaves, the sample's room less every map since that sample,
    /// in the raise window's low; the next sample starts the count again from its own room.
    #[test]
    fn c1_try_a_fresh_map_notes_the_headroom_it_leaves_in_the_window_low() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let gib = 1u64 << 30;
        let (room0, low0, at0) = (ROOM.load(Relaxed), [ROOM_LOW[0].load(Relaxed), ROOM_LOW[1].load(Relaxed)], ROOM_LOW_AT.load(Relaxed));
        ROOM_LOW.iter().for_each(|r| r.store(u64::MAX, Relaxed));
        ROOM.store(u64::MAX, Relaxed);
        note_mapped(gib);
        assert_eq!(room_low(), None, "before a sample: no room to note against");
        set_room(6 * gib, 32 * gib);
        note_room_low(6 * gib);
        note_mapped(gib);
        note_mapped(2 * gib);
        assert_eq!(room_low(), Some(3 * gib), "6 GiB at the sample, 3 GiB mapped since");
        set_room(5 * gib, 32 * gib);
        note_mapped(gib);
        assert_eq!(room_low(), Some(3 * gib), "a later sample's maps count from its own room (5 - 1), the window keeps the low");
        note_mapped(8 * gib);
        assert_eq!(room_low(), Some(0), "maps past the room: none left");
        ROOM_LOW.iter().for_each(|r| r.store(u64::MAX, Relaxed));
        let claim0 = [CLAIM_LOW[0].load(Relaxed), CLAIM_LOW[1].load(Relaxed)];
        CLAIM_LOW.iter().for_each(|r| r.store(u64::MAX, Relaxed));
        note_pool_room(5 * gib);
        note_pool_room(7 * gib);
        assert_eq!((claim_low(), room_low()), (Some(5 * gib), None), "the pool's room after a take: the claims' window keeps its least");
        note_pool_room(0);
        assert_eq!(claim_low(), Some(0), "a take that left the pool no room");
        CLAIM_LOW.iter().for_each(|r| r.store(u64::MAX, Relaxed));
        note_b_room(4 * gib);
        note_pool_room(6 * gib);
        assert_eq!((claim_low(), room_low()), (Some(4 * gib), None), "B's free bytes over its low mark bound the raises as the pool's room does");
        CLAIM_LOW[0].store(claim0[0], Relaxed);
        CLAIM_LOW[1].store(claim0[1], Relaxed);
        ROOM.store(room0, Relaxed);
        ROOM_LOW[0].store(low0[0], Relaxed);
        ROOM_LOW[1].store(low0[1], Relaxed);
        ROOM_LOW_AT.store(at0, Relaxed);
    }

    /// `landing_cap_read`: a landing past the read (s1rerun (7): 17.6 GiB at a statement's end, read 4.9 GB) nets the read to
    /// 0; the raises' window then holds the line less the read, not 0 for its minute.
    #[test]
    fn landing_cap_read_keeps_the_raises_room_at_the_line_less_the_read() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        use mempolicy::consts::{set, K};
        let gib = 1u64 << 30;
        let (limit, line, read, landing) = (16 * gib, 12 * gib, 5 * gib, 17 * gib);
        let (low0, at0) = ([ROOM_LOW[0].load(Relaxed), ROOM_LOW[1].load(Relaxed)], ROOM_LOW_AT.load(Relaxed));
        let fresh = || {
            ROOM_LOW.iter().for_each(|r| r.store(u64::MAX, Relaxed));
            ROOM_LOW_AT.store(0, Relaxed);
        };
        set(K::LandingCapRead, 0);
        assert_eq!(landing_of_read(landing, read), landing);
        fresh();
        background_pass_landed(&none, &|| 0, limit, line, read.saturating_sub(landing), landing, true, None);
        assert_eq!(room_low(), Some(0), "landing_cap_read 0: the landing past the read took the window to 0");
        set(K::LandingCapRead, 1);
        assert_eq!(landing_of_read(landing, read), read);
        assert_eq!(landing_of_read(gib, read), gib, "a landing under the read is netted whole");
        fresh();
        let l = landing_of_read(landing, read);
        background_pass_landed(&none, &|| 0, limit, line, read - l, l, true, None);
        assert_eq!(room_low(), Some(line - read), "the line less the read");
        fresh();
        let l = landing_of_read(gib, read);
        background_pass_landed(&none, &|| 0, limit, line, read - l, l, true, None);
        assert_eq!(room_low(), Some(line - read), "a landing under the read: the same room");
        ROOM_LOW[0].store(low0[0], Relaxed);
        ROOM_LOW[1].store(low0[1], Relaxed);
        ROOM_LOW_AT.store(at0, Relaxed);
    }

    static TEST_LANDING: AtomicU64 = AtomicU64::new(0);

    fn test_landing() -> u64 {
        TEST_LANDING.load(Relaxed)
    }

    static PROBE_HELD: AtomicU64 = AtomicU64::new(0);
    static PROBE_EVICTS: AtomicU64 = AtomicU64::new(0);
    fn part_probe() -> (u64, u64) {
        (PROBE_HELD.load(Relaxed), PROBE_EVICTS.load(Relaxed))
    }

    static SB_ABOVE: AtomicU64 = AtomicU64::new(0);
    static SB_LANDING: AtomicU64 = AtomicU64::new(0);
    static SB_ASKS: AtomicU64 = AtomicU64::new(0);
    fn sb_reclaimable() -> u64 {
        SB_ABOVE.load(Relaxed)
    }
    fn sb_request(n: u64) -> u64 {
        SB_ASKS.fetch_add(1, Relaxed);
        let got = (n & !(rule::BUFFERS_EXTENT - 1)).min(SB_ABOVE.load(Relaxed));
        SB_ABOVE.fetch_sub(got, Relaxed);
        SB_LANDING.fetch_add(got, Relaxed);
        got
    }
    fn sb_landing() -> u64 {
        SB_LANDING.load(Relaxed)
    }
    /// buf_grow: what the fake pool may still grow by, its evicted bytes, and the grows asked of it.
    static SB_HEADROOM: AtomicU64 = AtomicU64::new(0);
    static SB_EVICTED_FAKE: AtomicU64 = AtomicU64::new(0);
    static SB_GROW_ASKS: AtomicU64 = AtomicU64::new(0);
    fn sb_grow(n: u64) -> u64 {
        SB_GROW_ASKS.fetch_add(1, Relaxed);
        let got = (n & !(rule::BUFFERS_EXTENT - 1)).min(SB_HEADROOM.load(Relaxed));
        SB_HEADROOM.fetch_sub(got, Relaxed);
        SB_ABOVE.fetch_add(got, Relaxed);
        got
    }
    fn sb_evicted() -> u64 {
        SB_EVICTED_FAKE.load(Relaxed)
    }
    static SB_FAKE: Buffers = Buffers { reclaimable: sb_reclaimable, request: sb_request, landing: sb_landing, grow: sb_grow, evicted: sb_evicted };

    struct SbFake;

    impl SbFake {
        fn on(above: u64) -> SbFake {
            SB_ABOVE.store(above, Relaxed);
            SB_LANDING.store(0, Relaxed);
            SB_ASKS.store(0, Relaxed);
            SB_DUE_MS.store(0, Relaxed);
            PAST_CANCEL.store(false, Relaxed);
            SB_HEADROOM.store(0, Relaxed);
            SB_EVICTED_FAKE.store(0, Relaxed);
            SB_GROW_ASKS.store(0, Relaxed);
            for a in [&SB_EVICTED, &SB_EVICT_AT, &SB_GROW_BASE, &SB_GROW_AT, &SB_TAKE_AT] {
                a.store(0, Relaxed);
            }
            SB_QUIET_SHIFT.store(0, Relaxed);
            install_buffers(&SB_FAKE);
            SbFake
        }
    }

    impl Drop for SbFake {
        fn drop(&mut self) {
            BUFFERS.store(core::ptr::null_mut(), Release);
            SB_DUE_MS.store(0, Relaxed);
            BURST.store(u64::MAX, Relaxed);
            DRAIN_TO.store(0, Relaxed);
            ROOM.store(u64::MAX, Relaxed);
            mempolicy::consts::set(mempolicy::consts::K::CacheFloor, 0);
            publish(OFF, 0);
            clear_narrow();
        }
    }

    /// buf_grow (user 10-18: the buffer pool grows into free memory): the pool grows by the bytes of the valid pages it
    /// evicted since its last grow, within the room the cache raises left; nothing with no new evictions, no room or the
    /// knob off; a take within 10 s of a grow is a reversal, and the next grow waits the doubled quiet period from the take.
    #[test]
    fn buf_grow_the_pool_grows_by_its_evictions_within_the_room_and_backs_off_after_a_take() {
        use mempolicy::consts::{set, K};
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let (gib, mib) = (1u64 << 30, 1u64 << 20);
        let _sb = SbFake::on(0);
        SB_HEADROOM.store(4 * gib, Relaxed);
        let (grows0, grown0) = buffers_grow_counts();
        let t = 100_000;
        assert_eq!(grow_buffers(gib, t), gib, "no evictions: no grow");
        assert_eq!(SB_GROW_ASKS.load(Relaxed), 0);
        SB_EVICTED_FAKE.store(300 * mib, Relaxed);
        assert_eq!(grow_buffers(gib, t + 100), gib - 300 * mib, "300 MiB evicted, 1 GiB of room: 300 MiB");
        assert_eq!(buffers_grow_counts(), (grows0 + 1, grown0 + 300 * mib));
        assert_eq!(grow_buffers(gib, t + 1_000), gib, "no eviction since the grow: none (never a fill)");
        SB_EVICTED_FAKE.store(1300 * mib, Relaxed);
        assert_eq!(grow_buffers(200 * mib, t + 1_100), 0, "1000 MiB evicted, 200 MiB of room: the room");
        SB_EVICTED_FAKE.store(2000 * mib, Relaxed);
        let asks = SB_GROW_ASKS.load(Relaxed);
        assert_eq!(grow_buffers(0, t + 1_200), 0, "no room: none");
        set(K::BufGrow, 0);
        assert_eq!(grow_buffers(gib, t + 1_300), gib, "buf_grow 0: none");
        set(K::BufGrow, 1);
        assert_eq!(SB_GROW_ASKS.load(Relaxed), asks, "neither asked the pool");
        // the ladder takes from the pool 300 ms after its last grow: a reversal
        assert!(buffers_take(100 * mib, 0, t + 1_400) != 0, "the take");
        let quiet = rule::refill_quiet_ms(mempolicy::consts::get(K::CacheRefillQuietMs), 1);
        SB_EVICTED_FAKE.store(2100 * mib, Relaxed);
        assert_eq!(grow_buffers(gib, t + 1_400 + quiet - 1), gib, "within the doubled quiet period from the take: none");
        SB_EVICTED_FAKE.store(2200 * mib, Relaxed);
        assert!(grow_buffers(gib, t + 1_400 + quiet) < gib, "past it: a grow");
    }

    /// L6 on C1's background trigger: the pool only for what R2a's elastic bytes cannot cover; its bytes landing, never
    /// room, until their bound.
    #[test]
    fn l6_c1_takes_shared_buffers_only_past_the_caches_elastic_bytes_and_counts_them_landing_not_room() {
        use mempolicy::consts::{set, K};
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let (gib, mib) = (1u64 << 30, 1u64 << 20);
        let (limit, line) = (8 * gib, 7 * gib);
        let (low, high) = rule::marks(limit);
        let elastic = std::cell::Cell::new(u64::MAX);
        let asked = std::cell::Cell::new(0u64);
        let reclaim = |n: u64| {
            asked.set(asked.get() + n);
            let got = n.min(elastic.get());
            elastic.set(elastic.get() - got);
            got
        };
        let pass = |room: u64| background_pass_with(&reclaim, &|| elastic.get(), limit, line, line - room, false, None);
        {
            let _sb = SbFake::on(0);
            BURST.store(u64::MAX, Relaxed);
            DRAIN_TO.store(0, Relaxed);
            set(K::CacheFloor, 1);
            publish(ON, line);
            elastic.set(100 * mib);
            pass(low / 2);
            assert_eq!(SB_ASKS.load(Relaxed), 0, "nothing above the configured size (the knob off): no ask");
            assert_eq!(asked.get(), high - low / 2, "R2a's batch as before");
        }
        let _sb = SbFake::on(gib);
        BURST.store(u64::MAX, Relaxed);
        DRAIN_TO.store(0, Relaxed);
        set(K::CacheFloor, 1);
        publish(ON, line);
        elastic.set(u64::MAX);
        asked.set(0);
        pass(low / 2);
        assert_eq!(SB_ASKS.load(Relaxed), 0, "the caches cover the drain: shared_buffers stays");
        elastic.set(100 * mib);
        asked.set(0);
        let (runs0, bytes0) = buffers_counts();
        pass(low / 2);
        let want = rule::buffers_ask(high - low / 2, 100 * mib, gib);
        assert_eq!((SB_ASKS.load(Relaxed), SB_LANDING.load(Relaxed)), (1, want));
        assert_eq!(want, (high - low / 2 - 100 * mib).div_ceil(2 * mib) * 2 * mib);
        assert_eq!(asked.get(), high - low / 2, "R2a still runs its batch: its 100 MiB first");
        assert_eq!(buffers_counts(), (runs0 + 1, bytes0 + want));
        let room = low / 2 + 100 * mib;
        asked.set(0);
        pass(room);
        assert_eq!((SB_ASKS.load(Relaxed), asked.get()), (1, 0), "landing: no second ask of the pool, no batch");
        assert_eq!(room_now(), Some((room, limit)), "never the draws' room");
        // held past the bound: present again (decide_ms is 1 at its first read, so the bound is set at 0)
        SB_DUE_MS.store(0, Relaxed);
        assert_eq!(buffers_landing_at(crate::governor::decide_ms().max(1)), 0);
        elastic.set(50 * mib);
        pass(room);
        assert_eq!((SB_ASKS.load(Relaxed), asked.get()), (1, high - room), "past the bound: R2a's batch, no new ask of the late pool");
        assert_eq!(SB_DUE_MS.load(Relaxed), 0);
        SB_LANDING.store(0, Relaxed);
        pass(room);
        assert_eq!(SB_ASKS.load(Relaxed), 2);
        assert!(SB_DUE_MS.load(Relaxed) > 0, "a fresh ask with nothing landing: a fresh credit");
    }

    /// L6 on the hook's pass: the pool after R2a; no wait or R6 for its landing bytes within their bound. A gate's climb
    /// never asks it.
    #[test]
    fn l6_the_hook_asks_shared_buffers_after_r2a_and_cancels_no_one_for_its_landing_bytes() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        clear_narrow();
        r1_reset();
        hook_reset();
        let _sb = SbFake::on(1024 * MIB);
        let g = Fake::new(1100 * MIB, 40 * MIB, 0, 0, 0, 0);
        let c = climb_with(&g, 1000 * MIB, 0);
        assert_eq!((SB_ASKS.load(Relaxed), c.need), (0, 60 * MIB), "a gate's climb: no ask, and no landing counted");
        clear_narrow();
        r1_reset();
        let f = Fake::new(1100 * MIB, 40 * MIB, 0, 0, 0, 0);
        f.now.set(60_000);
        let c = hook_pass_with(&f, 1000 * MIB, 60_000);
        assert_eq!((SB_ASKS.load(Relaxed), SB_LANDING.load(Relaxed)), (1, 60 * MIB));
        assert_eq!((c.ran, c.need), (R1 | 1 << Rung::EvictElastic as u8, 0), "R2a's 40 MiB and the pool's 60 MiB landing: covered");
        assert_eq!(f.order()[..2], [Rung::AllocReturn as u8 + 1, Rung::EvictElastic as u8 + 1]);
        assert_eq!(HOOK_SINCE.load(Relaxed), 0, "no episode left open");
        let due = SB_DUE_MS.load(Relaxed);
        assert_eq!(due, 60_000 + rule::buffers_due_ms(60 * MIB, mempolicy::consts::get(mempolicy::consts::K::BufGivebackWriteBw)));
        for t in [60_100, 60_200, due] {
            f.now.set(t);
            assert_eq!(hook_pass_with(&f, 1000 * MIB, t), Climbed::default(), "at {t}: landing");
        }
        assert_eq!(f.cancels.get(), 0);
        // held past the bound: present again
        f.now.set(due + 1);
        let c = hook_pass_with(&f, 1000 * MIB, due + 1);
        assert_eq!((c.ran & R6, c.need), (0, 60 * MIB), "past the bound: R1-R4b and the wait");
        f.now.set(due + 1 + rule::HOOK_GRACE_MS);
        let c = hook_pass_with(&f, 1000 * MIB, due + 1 + rule::HOOK_GRACE_MS);
        assert_eq!((c.ran & R6, f.cancels.get()), (R6, 1), "a tick on, the need still there: R6");
        assert_eq!((SB_ASKS.load(Relaxed), SB_DUE_MS.load(Relaxed)), (1, due), "no new ask of a drain behind its bound");
        hook_reset();
        r1_reset();
    }

    /// L6 (the broker's condition): past the watchdog's cancel line the pool's landing bytes hold off nothing: the pool
    /// is still asked, and the same pass reaches R6.
    #[test]
    fn l6_past_the_cancel_line_the_pools_landing_bytes_never_hold_off_r6() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        clear_narrow();
        r1_reset();
        hook_reset();
        let _sb = SbFake::on(1024 * MIB);
        publish(ON, 1000 * MIB);
        let f = Fake::new(1100 * MIB, 0, 0, 0, 0, 0);
        f.now.set(70_000);
        waive_wait();
        let c = hook_pass_with(&f, 1000 * MIB, 70_000);
        assert_eq!((SB_ASKS.load(Relaxed), SB_LANDING.load(Relaxed)), (1, 100 * MIB), "the pool is still asked");
        assert_eq!((c.ran & R6, f.cancels.get()), (R6, 1), "past the cancel line: R6 in the same pass, the landing bytes count for nothing");
        assert!(!PAST_CANCEL.load(Relaxed), "one pass only");
        // under the cancel line the same landing bytes cover the need: no wait, no second cancel
        f.landing.set(false);
        hook_reset();
        f.now.set(70_100);
        assert_eq!(hook_pass_with(&f, 1000 * MIB, 70_100), Climbed::default(), "under the cancel line: landing");
        assert_eq!(f.cancels.get(), 1);
        hook_reset();
        r1_reset();
    }
}
