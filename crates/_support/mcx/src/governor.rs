//! A block allocation that would take the process past the sampled bar fails
//! the armed statement with 53200, as C's malloc failure does, instead of the
//! kernel OOM killer taking the whole single-process server.

use core::sync::atomic::{AtomicI32, AtomicUsize, Ordering::Relaxed};

static BAR: AtomicUsize = AtomicUsize::new(0);
static RESIDENT: AtomicUsize = AtomicUsize::new(0);
static FOOTPRINT_AT: AtomicUsize = AtomicUsize::new(0);
// bar - resident + footprint_at: the context footprint the bar allows (`BAR_LIMIT`), or less: the block gate's (D4812
// (2): the cancel line's room past the sample), whichever is lower. `admit`'s one compare.
static FOOTPRINT_LIMIT: AtomicUsize = AtomicUsize::new(usize::MAX);
static BAR_LIMIT: AtomicUsize = AtomicUsize::new(usize::MAX);
// D4812 (2): the block gate's sample (`publish_gate`): the ladder's read, at this context footprint, and the context
// footprint the cancel line allows past it (`usize::MAX` with the gate off).
static GATE_READ: AtomicUsize = AtomicUsize::new(0);
static GATE_FOOTPRINT_AT: AtomicUsize = AtomicUsize::new(0);
static GATE_LIMIT: AtomicUsize = AtomicUsize::new(usize::MAX);
static RESIDENT_PROBE: core::sync::atomic::AtomicPtr<()> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
// D3808: the limiting cgroup's dirty + writeback page-cache bytes, read now (`set_dirty_probe`).
static DIRTY_PROBE: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
static EXEMPT_PROBE: core::sync::atomic::AtomicPtr<()> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());


#[cfg(any(feature = "std", test))]
std::thread_local! {
    static ARMED: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    static SLOT: core::cell::Cell<isize> = const { core::cell::Cell::new(-1) };
    static LEDGER: core::cell::Cell<*const StmtMem> = const { core::cell::Cell::new(core::ptr::null()) };
    static OWN: core::cell::RefCell<Option<alloc::sync::Arc<StmtMem>>> = const { core::cell::RefCell::new(None) };
    static PENDING: core::cell::Cell<isize> = const { core::cell::Cell::new(0) };
}

/// A thread adds its block charges to its statement ledger's shared total once they net this much either way, so a block
/// allocated and freed per row costs no shared atomic (ten concurrent W16 statements: one cache line per statement for
/// every worker's every block). The thread's own checks read its exact total; another thread's reads (the watchdog's
/// cancel, a sibling's limit check) miss < 1 MiB per thread.
pub const LEDGER_SLACK: isize = 1 << 20;

/// D3821: a statement's cancel word (`StmtMem::cancel`): the watchdog's percent in its low bits (0 = no cancel) ...
const CANCEL_PCT: usize = 0xff;
/// ... and the statement's generation above them.
const CANCEL_GEN_SHIFT: u32 = 8;
/// C3: a percent with this bit was a projection cancel (`cancel_fair_victim`; the percent itself is under 0x80).
const CANCEL_PROJECTED: usize = 0x80;

/// Per-backend view for the watchdog's victim choice, indexed by ProcNumber;
/// `bytes` is refreshed on the governor's slow path, i.e. whenever the
/// process is near or past its bar.
pub struct Slot {
    pub pid: AtomicI32,
    pub bytes: AtomicUsize,
    /// D3821: the cancel word (`StmtMem::cancel`, generation and percent) of the statement the watchdog canceled; 0 none.
    condemned: AtomicUsize,
    /// D4795 (2a): the running statement's part-cache share, `epoch << 32 | KiB` (`charge_cache` / `release_cache`): the
    /// epoch moves at `register` and every statement start (KiB to 0) and at its end (KiB kept), so a release lands only on
    /// the statement its holder charged, in the same compare-exchange that reads the epoch.
    cache: core::sync::atomic::AtomicU64,
}

pub const SLOTS: usize = 4096;
pub static SLOT_TABLE: [Slot; SLOTS] = [const {
    Slot {
        pid: AtomicI32::new(0),
        bytes: AtomicUsize::new(0),
        condemned: AtomicUsize::new(0),
        cache: core::sync::atomic::AtomicU64::new(0),
    }
}; SLOTS];

/// Bind this thread to slot `procno` (a backend's ProcNumber) as `pid`.
pub fn register(procno: i32, pid: i32) {
    let Ok(i) = usize::try_from(procno) else { return };
    if i >= SLOTS {
        return;
    }
    let s = &SLOT_TABLE[i];
    s.condemned.store(0, Relaxed);
    s.pid.store(pid, Relaxed);
    s.bytes.store(0, Relaxed);
    // D4795 (2a): a reused slot starts with no share, in a new epoch (the earlier backend's holders' frees take nothing)
    #[cfg(any(feature = "std", test))]
    share_epoch(i, false);
    #[cfg(any(feature = "std", test))]
    let _ = SLOT.try_with(|c| c.set(i as isize));
}

/// This thread's governor slot (`register`'s), if any.
pub(crate) fn my_slot_index() -> Option<usize> {
    #[cfg(any(feature = "std", test))]
    {
        let i = SLOT.try_with(|c| c.get()).unwrap_or(-1);
        usize::try_from(i).ok().filter(|&i| i < SLOTS)
    }
    #[cfg(not(any(feature = "std", test)))]
    None
}

fn my_slot() -> Option<&'static Slot> {
    #[cfg(any(feature = "std", test))]
    {
        let i = SLOT.try_with(|c| c.get()).unwrap_or(-1);
        usize::try_from(i).ok().and_then(|i| SLOT_TABLE.get(i))
    }
    #[cfg(not(any(feature = "std", test)))]
    None
}

/// D3821: mark the statement whose cancel word is `word` (`cancel_largest_victim`), running in the backend at `slot`, as
/// the watchdog's victim.
pub fn condemn(slot: usize, word: usize) {
    if let Some(s) = SLOT_TABLE.get(slot) {
        s.condemned.store(word, Relaxed);
    }
}

/// D3821: consume this backend's condemnation: the cancel word it names (0 = none).
pub fn take_condemned() -> usize {
    my_slot().map_or(0, |s| s.condemned.swap(0, Relaxed))
}

/// D3821: the memory watchdog's cancel of this backend's running statement, as its 53200, for ProcessInterrupts (the
/// watchdog raised the backend's interrupt and its Gravity abort byte beside the condemnation). `None` when none is
/// pending, or when the condemnation names a statement that already ended (its cancel word's generation moved): that one
/// is dropped, never raised on the statement after it.
pub fn take_watchdog_cancel() -> Option<types_error::PgError> {
    let word = take_condemned();
    if word == 0 {
        return None;
    }
    #[cfg(any(feature = "std", test))]
    {
        let p = LEDGER.try_with(|c| c.get()).unwrap_or(core::ptr::null());
        if p.is_null() {
            return None;
        }
        // SAFETY: as in `current`.
        let l = unsafe { &*p };
        (l.on.load(Relaxed) && l.cancel.load(Relaxed) == word).then(|| l.error(0))
    }
    #[cfg(not(any(feature = "std", test)))]
    None
}

/// D3821: whether this thread's running statement (a claim's: its leader's) was canceled by the watchdog: one TLS read
/// and two relaxed loads.
#[inline]
pub fn watchdog_cancel_pending() -> bool {
    #[cfg(any(feature = "std", test))]
    {
        let p = LEDGER.try_with(|c| c.get()).unwrap_or(core::ptr::null());
        // SAFETY: as in `current`.
        !p.is_null() && unsafe { &*p }.on.load(Relaxed) && unsafe { &*p }.cancel_pct() != 0
    }
    #[cfg(not(any(feature = "std", test)))]
    false
}

/// Publish a sample; `bar == 0` disables the governor. The block gate's sample is its own (`publish_gate`).
pub fn publish(bar: usize, resident: usize, footprint: usize) {
    RESIDENT.store(resident, Relaxed);
    FOOTPRINT_AT.store(footprint, Relaxed);
    BAR.store(bar, Relaxed);
    let limit = if bar == 0 {
        usize::MAX
    } else {
        bar.saturating_add(footprint).saturating_sub(resident)
    };
    BAR_LIMIT.store(limit, Relaxed);
    FOOTPRINT_LIMIT.store(limit.min(GATE_LIMIT.load(Relaxed)), Relaxed);
}

/// D4812 (2): the block gate's sample: `read`, the memory ladder's own read of the sample (memruntime's
/// `ladder_read_of`: memory.current less the clean inactive page cache, the read the ladder's pass compares to the
/// cancel line, never the governed resident `publish` takes), at the context footprint `footprint`. memruntime
/// publishes it every sample that has a reading, whatever the bar, after `publish`, from the files that sample read; the
/// pass's own climb reads afresh (memruntime's `ladder_read_now`), so its read can be a few ms newer than this one: the
/// same measure, not always the same bytes. The governor's `projected` keeps its own resident reading for its other
/// readers. With the gate on, the context footprint the cancel line allows past the read lowers `admit`'s limit (0 once
/// the read is past the line: every block takes the slow path); with it off the limit is the bar's, as before. The cancel line and the ladder's mode it reads are the previous sample's (memruntime
/// publishes them in its ladder pass, after this): the gate's limit runs one sample behind a change of the limit, the
/// threshold or the mode, never behind the read or the footprint.
pub fn publish_gate(read: usize, footprint: usize) {
    GATE_READ.store(read, Relaxed);
    GATE_FOOTPRINT_AT.store(footprint, Relaxed);
    let gate = if gate_on() {
        mempolicy::admit::cancel_line_footprint(crate::ladder::cancel_line() as usize, read, footprint)
    } else {
        usize::MAX
    };
    GATE_LIMIT.store(gate, Relaxed);
    #[cfg(any(feature = "std", test))]
    GATE_SAMPLE_NS.store(crate::ladder_stats::clock_ns(), Relaxed);
    FOOTPRINT_LIMIT.store(BAR_LIMIT.load(Relaxed).min(gate), Relaxed);
}

/// D4812 (2): the block gate's projection at the context footprint `footprint`: its sample's read and the context bytes
/// taken since.
pub fn gate_projection_at(footprint: usize) -> usize {
    GATE_READ.load(Relaxed).saturating_add(footprint).saturating_sub(GATE_FOOTPRINT_AT.load(Relaxed))
}

/// D4812 (2): the block gate's projection now.
#[cfg(any(feature = "std", test))]
fn gate_projected() -> usize {
    gate_projection_at(crate::global_footprint::bytes())
}

pub fn bar() -> usize {
    BAR.load(Relaxed)
}

static LIMIT: AtomicUsize = AtomicUsize::new(0);

pub fn publish_limit(limit: usize) {
    LIMIT.store(limit, Relaxed);
}

pub fn limit() -> usize {
    LIMIT.load(Relaxed)
}

pub fn projected() -> usize {
    let now = crate::global_footprint::bytes();
    let at = FOOTPRINT_AT.load(Relaxed);
    RESIDENT.load(Relaxed).saturating_add(now).saturating_sub(at)
}

/// whether `n` bytes taken outside memory contexts (a Gravity grant draw: its regions are raw allocations the
/// block-grain check never sees) keep the process under the bar: the last sample's resident bytes, the context growth
/// since, and `n`. Off (bar 0): always. A draw it refuses takes the full pool's road (a wait, a smaller grant, a spill,
/// the statement's fall through to the row executor, where the block-grain check fails it with 53200), never the
/// kernel's OOM kill.
pub fn fits_raw(n: usize) -> bool {
    fits_raw_for(n, false)
}

/// D3941: `fits_raw` for a statement's admission (its grant, not a draw past it). decide(Grow)'s spill-self refuses the
/// draws PAST a grant; refusing the grant too left the statement in the admission queue for room no wait brings back
/// (a grouping query under a 16 GiB limit sat to its 300 s statement_timeout with the pool free and memory flat).
pub fn fits_raw_admit(n: usize) -> bool {
    fits_raw_for(n, true)
}

/// D4439 (`draw_wait_cap`): this thread's statement's draw-gate waits so far (ns, all its threads), `None` with no
/// statement charged on this thread.
pub fn stmt_draw_waited_ns() -> Option<u64> {
    charged_ledger().map(|l| l.draw_waited_ns.load(Relaxed))
}

/// D4439: `ns` more of this thread's statement's draw-gate waits (none with no statement charged here).
pub fn note_stmt_draw_wait(ns: u64) {
    if let Some(l) = charged_ledger() {
        l.draw_waited_ns.fetch_add(ns, Relaxed);
    }
}

/// The bytes field of `StmtMem::refused` (the branch is the top byte).
const REFUSED_BYTES: u64 = (1 << 56) - 1;

/// D4914: this thread's statement refused a draw of `bytes` past its grant on branch `why` (`ladder_stats::Why as u8`);
/// the last one kept (none with no statement charged here).
pub fn note_stmt_refusal(why: u8, bytes: u64) {
    if let Some(l) = charged_ledger() {
        l.refused.store((u64::from(why) << 56) | bytes.min(REFUSED_BYTES), Relaxed);
    }
}

/// D4914: this thread's statement's last refused draw past its grant: (branch, bytes asked, its draw-gate waits so far in
/// ns); `None` with no statement charged here or no refusal since its start.
pub fn stmt_refusal() -> Option<(u8, u64, u64)> {
    let l = charged_ledger()?;
    let r = l.refused.load(Relaxed);
    (r != 0).then(|| ((r >> 56) as u8, r & REFUSED_BYTES, l.draw_waited_ns.load(Relaxed)))
}

/// C3: whether decide(Grow) sent this thread's statement down its own road: a draw past its grant is refused at once (no
/// wait on another holder: the refusal is the statement's own).
pub fn spills_itself() -> bool {
    charged_ledger().is_some_and(StmtMem::spilling)
}

fn fits_raw_for(n: usize, admit: bool) -> bool {
    // C3: a decide due on this thread's statement is served at its draw (a safe point: no raise, a refusal spills)
    if let Some(l) = charged_ledger() {
        if l.due.load(Relaxed) {
            serve_due(l);
        }
        // C3: a statement decide(Grow) sent down its own road takes no raw bytes past its grant (its breakers spill)
        if !admit && l.spilling() {
            note_trip(TRIP_DRAW, "Gravity grant (spills itself)", n);
            return false;
        }
    }
    // D3821: a statement the watchdog canceled takes no more raw bytes while its cancel is delivered
    if watchdog_cancel_pending() {
        note_trip(TRIP_DRAW, "Gravity grant (canceled)", n);
        return false;
    }
    let fits = mempolicy::admit::under_bar(projected(), n, BAR.load(Relaxed));
    if !fits {
        note_trip(TRIP_DRAW, "Gravity grant", n);
    }
    fits
}

pub fn set_resident_probe(f: fn() -> Option<usize>) {
    RESIDENT_PROBE.store(f as *mut (), Relaxed);
}

/// The process's resident bytes read now through the sampler's probe (`set_resident_probe`), whether or not the governor
/// has a bar; None with no probe installed or an unreadable sample. Gravity's memory budget reads it while its broker is
/// on: memory no consumer counts (allocator retention, backends' contexts) then comes out of the budget's free bytes.
pub fn probe_resident() -> Option<usize> {
    let p = RESIDENT_PROBE.load(Relaxed);
    if p.is_null() {
        return None;
    }
    // SAFETY: only set_resident_probe stores here, always from fn() -> Option<usize>.
    let f: fn() -> Option<usize> = unsafe { core::mem::transmute(p) };
    f()
}

/// D3808: the dirty + writeback page-cache bytes of the cgroup whose memory.max is the limit, read now; installed by the
/// sampler's owner next to the resident probe.
pub fn set_dirty_probe(f: fn() -> Option<usize>) {
    DIRTY_PROBE.store(f as *mut (), Relaxed);
}

/// D3808: the dirty + writeback page-cache bytes `probe_resident` counts (under a cgroup limit its resident is the
/// cgroup's unreclaimable bytes, which hold them; 0 under any other limit); None with no probe installed or an
/// unreadable sample. A bulk load's gate leaves them to the write governor's budget, out of the page-cache floor.
pub fn probe_dirty() -> Option<usize> {
    let p = DIRTY_PROBE.load(Relaxed);
    if p.is_null() {
        return None;
    }
    // SAFETY: only set_dirty_probe stores here, always from fn() -> Option<usize>.
    let f: fn() -> Option<usize> = unsafe { core::mem::transmute(p) };
    f()
}

pub fn resample() -> bool {
    let bar = BAR.load(Relaxed);
    let p = RESIDENT_PROBE.load(Relaxed);
    if bar == 0 || p.is_null() {
        return false;
    }
    // SAFETY: only set_resident_probe stores here, always from fn() -> Option<usize>.
    let f: fn() -> Option<usize> = unsafe { core::mem::transmute(p) };
    let footprint = crate::global_footprint::bytes();
    match f() {
        Some(resident) => {
            publish(bar, resident, footprint);
            true
        }
        None => false,
    }
}


/// The limiting cgroup's resident bytes read now (memory.stat anon + shmem), for the memory ledger's views; installed by
/// the sampler's owner. `None` with no probe, no cgroup limit or an unreadable file.
static CGROUP_PROBE: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_cgroup_resident_probe(f: fn() -> Option<usize>) {
    CGROUP_PROBE.store(f as *mut (), Relaxed);
}

/// The allocator's resident bytes read now (the present pages of its own mappings), for the memory ledger's views;
/// installed by the sampler's owner, read only when a view asks. `None` with no probe or an unreadable file.
static ALLOC_RESIDENT_PROBE: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_allocator_resident_probe(f: fn() -> Option<usize>) {
    ALLOC_RESIDENT_PROBE.store(f as *mut (), Relaxed);
}

/// `set_allocator_resident_probe`'s reading now.
pub fn probe_allocator_resident() -> Option<usize> {
    let p = ALLOC_RESIDENT_PROBE.load(Relaxed);
    if p.is_null() {
        return None;
    }
    // SAFETY: only set_allocator_resident_probe stores here, always from fn() -> Option<usize>.
    let f: fn() -> Option<usize> = unsafe { core::mem::transmute(p) };
    f()
}

/// The Gravity region mappings' resident bytes read now (the present pages of the mappings named `pgrust:region...`,
/// named only while the memory policy's trace is on), for the memory ledger's views; installed by the sampler's owner,
/// read only when a view asks. `None` with no probe or an unreadable file.
static REGION_RESIDENT_PROBE: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_region_resident_probe(f: fn() -> Option<usize>) {
    REGION_RESIDENT_PROBE.store(f as *mut (), Relaxed);
}

/// `set_region_resident_probe`'s reading now.
pub fn probe_region_resident() -> Option<usize> {
    let p = REGION_RESIDENT_PROBE.load(Relaxed);
    if p.is_null() {
        return None;
    }
    // SAFETY: only set_region_resident_probe stores here, always from fn() -> Option<usize>.
    let f: fn() -> Option<usize> = unsafe { core::mem::transmute(p) };
    f()
}

/// `set_cgroup_resident_probe`'s reading now (`None`: no probe, no cgroup limit, unreadable).
pub fn probe_cgroup_resident() -> Option<usize> {
    let p = CGROUP_PROBE.load(Relaxed);
    if p.is_null() {
        return None;
    }
    // SAFETY: only set_cgroup_resident_probe stores here, always from fn() -> Option<usize>.
    let f: fn() -> Option<usize> = unsafe { core::mem::transmute(p) };
    f()
}

/// LEDGER2 (`pgrust_memory_reconcile`): the readings the process total is set against, read now by the sampler's owner's
/// probe (`None`: unreadable or no such source): the limiting cgroup's memory.current and its memory.stat anon, file,
/// shmem, kernel and sock; the C library allocator's in-use bytes (glibc's mallinfo2: what C libraries hold beside
/// mimalloc); the process's threads.
pub mod reconcile {
    pub const CURRENT: usize = 0;
    pub const ANON: usize = 1;
    pub const FILE: usize = 2;
    pub const SHMEM: usize = 3;
    pub const KERNEL: usize = 4;
    pub const SOCK: usize = 5;
    pub const C_LIBRARIES: usize = 6;
    pub const THREADS: usize = 7;
    pub const TERMS: usize = 8;
}

static RECONCILE_PROBE: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_reconcile_probe(f: fn() -> [Option<u64>; reconcile::TERMS]) {
    RECONCILE_PROBE.store(f as *mut (), Relaxed);
}

/// `set_reconcile_probe`'s readings now (all `None` with no probe).
pub fn probe_reconcile() -> [Option<u64>; reconcile::TERMS] {
    let p = RECONCILE_PROBE.load(Relaxed);
    if p.is_null() {
        return [None; reconcile::TERMS];
    }
    // SAFETY: only set_reconcile_probe stores here, always from fn() -> [Option<u64>; reconcile::TERMS].
    let f: fn() -> [Option<u64>; reconcile::TERMS] = unsafe { core::mem::transmute(p) };
    f()
}

static ALLOC_IN_USE: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
static ALLOC_COMMITTED: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_allocator_probes(in_use: fn() -> Option<usize>, committed: fn() -> usize) {
    ALLOC_IN_USE.store(in_use as *mut (), Relaxed);
    ALLOC_COMMITTED.store(committed as *mut (), Relaxed);
}

pub fn allocator_in_use() -> Option<usize> {
    let p = ALLOC_IN_USE.load(Relaxed);
    if p.is_null() {
        return None;
    }
    // SAFETY: only set_allocator_probes stores here, always from fn() -> Option<usize>.
    let f: fn() -> Option<usize> = unsafe { core::mem::transmute(p) };
    f()
}

pub fn allocator_committed() -> Option<usize> {
    let p = ALLOC_COMMITTED.load(Relaxed);
    if p.is_null() {
        return None;
    }
    // SAFETY: only set_allocator_probes stores here, always from fn() -> usize.
    let f: fn() -> usize = unsafe { core::mem::transmute(p) };
    Some(f())
}

pub fn allocator_census(resident: usize) -> Option<alloc::string::String> {
    let in_use = allocator_in_use()?;
    let mib = |b: i64| b >> 20;
    let footprint = crate::global_footprint::bytes() as i64;
    Some(alloc::format!(
        "alloc_in_use={} alloc_committed={} live_outside_contexts={} resident_not_live={}",
        mib(in_use as i64),
        allocator_committed().map_or(-1, |c| mib(c as i64)),
        mib(in_use as i64 - footprint),
        mib(resident as i64 - in_use as i64),
    ))
}

pub const TRIP_KINDS: [&str; 4] =
    ["a block past the bar", "a part open past the bar", "a statement past its ledger", "a raw draw past the bar"];
const TRIP_BLOCK: usize = 0;
const TRIP_CACHE: usize = 1;
const TRIP_STATEMENT: usize = 2;
const TRIP_DRAW: usize = 3;

static TRIPS: [core::sync::atomic::AtomicU64; 4] = [const { core::sync::atomic::AtomicU64::new(0) }; 4];
static TRIP_KIND: AtomicUsize = AtomicUsize::new(0);
static TRIP_N: AtomicUsize = AtomicUsize::new(0);
static TRIP_PROJECTED: AtomicUsize = AtomicUsize::new(0);
static TRIP_BAR: AtomicUsize = AtomicUsize::new(0);
static TRIP_IN_USE: AtomicUsize = AtomicUsize::new(usize::MAX);
#[cfg(any(feature = "std", test))]
static TRIP_NAME: std::sync::Mutex<&'static str> = std::sync::Mutex::new("");

#[derive(Clone, Copy, Debug)]
pub struct Trip {
    pub kind: &'static str,
    pub name: &'static str,
    pub n: usize,
    pub projected: usize,
    pub bar: usize,
    pub in_use: Option<usize>,
}

pub fn trip_counts() -> [u64; 4] {
    core::array::from_fn(|k| TRIPS[k].load(Relaxed))
}

pub fn last_trip() -> Trip {
    #[cfg(any(feature = "std", test))]
    let name = TRIP_NAME.try_lock().map_or("", |g| *g);
    #[cfg(not(any(feature = "std", test)))]
    let name = "";
    let in_use = TRIP_IN_USE.load(Relaxed);
    Trip {
        kind: TRIP_KINDS[TRIP_KIND.load(Relaxed).min(3)],
        name,
        n: TRIP_N.load(Relaxed),
        projected: TRIP_PROJECTED.load(Relaxed),
        bar: TRIP_BAR.load(Relaxed),
        in_use: (in_use != usize::MAX).then_some(in_use),
    }
}

fn note_trip(kind: usize, name: &'static str, n: usize) {
    TRIP_KIND.store(kind, Relaxed);
    TRIP_N.store(n, Relaxed);
    TRIP_PROJECTED.store(projected(), Relaxed);
    TRIP_BAR.store(BAR.load(Relaxed), Relaxed);
    TRIP_IN_USE.store(allocator_in_use().unwrap_or(usize::MAX), Relaxed);
    #[cfg(any(feature = "std", test))]
    if let Ok(mut g) = TRIP_NAME.try_lock() {
        *g = name;
    }
    #[cfg(not(any(feature = "std", test)))]
    let _ = name;
    TRIPS[kind].fetch_add(1, Relaxed);
}

/// A probe for "an ERROR here would be promoted to PANIC" (critical sections).
pub fn set_exempt_probe(f: fn() -> bool) {
    EXEMPT_PROBE.store(f as *mut (), Relaxed);
}

pub fn arm(on: bool) {
    #[cfg(any(feature = "std", test))]
    let _ = ARMED.try_with(|a| a.set(on));
    #[cfg(not(any(feature = "std", test)))]
    let _ = on;
}

pub fn armed() -> bool {
    #[cfg(any(feature = "std", test))]
    {
        ARMED.try_with(|a| a.get()).unwrap_or(false)
    }
    #[cfg(not(any(feature = "std", test)))]
    false
}

#[inline]
pub(crate) fn admit(n: usize, context_name: &'static str, context_bytes: usize) {
    if crate::global_footprint::bytes().saturating_add(n) > FOOTPRINT_LIMIT.load(Relaxed) {
        admit_slow(BAR.load(Relaxed), n, context_name);
    }
    ledger_admit(n, context_name, context_bytes);
}

/// D4812 (2): the block gate acts (`ladder_block_gate` = 1, with the memory ladder on).
fn gate_on() -> bool {
    mempolicy::consts::get(mempolicy::consts::K::LadderBlockGate) != 0 && crate::ladder::on()
}

/// D4812 (2), the block gate's counters: the slow-path entries of blocks it does not weigh (`mempolicy::admit::block_gated`; past the gate's
/// limit, they take the slow path to do nothing), the gated blocks taken at once under a critical section or held
/// interrupts, the climbs that ran a rung, the pick tests a held pins table or registry skipped, and the waits a new
/// sample ended: their count, and the ns from that sample's publish to the waiter's next test (the 5 ms sleep's lag; sum
/// and max).
static GATE_SMALL: AtomicUsize = AtomicUsize::new(0);
static GATE_SKIPPED: AtomicUsize = AtomicUsize::new(0);
static GATE_CLIMBS: AtomicUsize = AtomicUsize::new(0);
static GATE_BUSY: AtomicUsize = AtomicUsize::new(0);
#[cfg(any(feature = "std", test))]
static GATE_SAMPLE_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static GATE_LAGS: AtomicUsize = AtomicUsize::new(0);
static GATE_LAG_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static GATE_LAG_NS_TOP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// D4812 (2): the block gate's counters since start (the gate's own row is `ladder_stats::site(Site::Block)`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockGateCounts {
    pub small: u64,
    pub skipped: u64,
    pub climbs: u64,
    pub busy: u64,
    pub sample_ends: u64,
    pub sample_lag_ns: u64,
    pub sample_lag_ns_top: u64,
}

pub fn block_gate_counts() -> BlockGateCounts {
    BlockGateCounts {
        small: GATE_SMALL.load(Relaxed) as u64,
        skipped: GATE_SKIPPED.load(Relaxed) as u64,
        climbs: GATE_CLIMBS.load(Relaxed) as u64,
        busy: GATE_BUSY.load(Relaxed) as u64,
        sample_ends: GATE_LAGS.load(Relaxed) as u64,
        sample_lag_ns: GATE_LAG_NS.load(Relaxed),
        sample_lag_ns_top: GATE_LAG_NS_TOP.load(Relaxed),
    }
}

static SLEEP_PROBE: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
static INTERRUPT_PROBE: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

/// D4812 (2): a probe for "an interrupt is pending on this thread" (tcop's: InterruptPending, a query cancel or a die):
/// the block gate stops waiting on it and takes the block, so the caller's next CHECK_FOR_INTERRUPTS answers a cancel or
/// a statement_timeout at once, never after the gate's bound. None installed: never.
pub fn set_interrupt_probe(f: fn() -> bool) {
    INTERRUPT_PROBE.store(f as *mut (), Relaxed);
}

#[cfg(any(feature = "std", test))]
fn interrupt_pending() -> bool {
    let p = INTERRUPT_PROBE.load(Relaxed);
    if p.is_null() {
        return false;
    }
    // SAFETY: only set_interrupt_probe stores here, always from fn() -> bool.
    let f: fn() -> bool = unsafe { core::mem::transmute(p) };
    f()
}

/// D4812 (2): a probe for "this thread may sleep inside an allocation" (tcop's: no critical section and no interrupts held
/// off; every LWLock acquire holds them, as in C). None installed: any thread not unwinding and not exempt (`raise_ok`).
pub fn set_sleep_probe(f: fn() -> bool) {
    SLEEP_PROBE.store(f as *mut (), Relaxed);
}

#[cfg(any(feature = "std", test))]
fn may_sleep() -> bool {
    if !raise_ok() {
        return false;
    }
    let p = SLEEP_PROBE.load(Relaxed);
    if p.is_null() {
        return true;
    }
    // SAFETY: only set_sleep_probe stores here, always from fn() -> bool.
    let f: fn() -> bool = unsafe { core::mem::transmute(p) };
    f()
}

#[cold]
#[inline(never)]
fn admit_slow(bar: usize, n: usize, context_name: &'static str) {
    // D4812 (2): with the block gate off, today's path exactly (FOOTPRINT_LIMIT is the bar's then)
    if !gate_on() {
        bar_slow(bar, n, context_name);
        return;
    }
    // past the gate's limit every block comes here; one under the gate's least only meets the bar, so it is counted
    // apart and, with no bar, returns before any further load
    let gated = mempolicy::admit::block_gated(n);
    if !gated {
        GATE_SMALL.fetch_add(1, Relaxed);
        if bar == 0 {
            return;
        }
    }
    if bar != 0 && mempolicy::admit::past_footprint(crate::global_footprint::bytes(), n, BAR_LIMIT.load(Relaxed)) {
        bar_slow(bar, n, context_name);
    }
    #[cfg(any(feature = "std", test))]
    if gated {
        block_gate(n);
    }
}

/// The bar's test of a block past its limit (`admit_slow`): 53200 on the armed statement past the bar.
fn bar_slow(bar: usize, n: usize, context_name: &'static str) {
    if let Some(s) = my_slot() {
        s.bytes.store(crate::global_footprint::thread_current().max(0) as usize, Relaxed);
    }
    if bar == 0 || !armed() {
        return;
    }
    resample();
    let p = projected();
    let want = p.saturating_add(n);
    if mempolicy::admit::under_bar(p, n, bar) || mempolicy::admit::small_session_grace(want, bar, crate::global_footprint::thread_current()) {
        return;
    }
    #[cfg(any(feature = "std", test))]
    {
        if std::thread::panicking() {
            return;
        }
        let p = EXEMPT_PROBE.load(Relaxed);
        if !p.is_null() {
            // SAFETY: only set_exempt_probe stores here, always from fn() -> bool.
            let f: fn() -> bool = unsafe { core::mem::transmute(p) };
            if f() {
                return;
            }
        }
        note_trip(TRIP_BLOCK, context_name, n);
        arm(false);
        std::panic::panic_any(alloc::boxed::Box::new(crate::oom_named(context_name, n)));
    }
    #[cfg(not(any(feature = "std", test)))]
    let _ = context_name;
}

/// D4812 (2), the block gate (`ladder_block_gate`): this thread's statement asks for a context block of `n` bytes (one
/// the gate weighs, `mempolicy::admit::block_gated`) that takes the gate's projection (`gate_projected`: the
/// ladder's read at its last sample and the context bytes taken since) past the watchdog's cancel line, which a ladder pass 100
/// ms apart can see too late. Only the rungs that store or ask run here (`ladder::block_climb`: R3, R4a, R4b, the
/// ladder pass woken for the rest); then the block waits at most `cancel_landing_ms` for the projection to fall back
/// under the line (`BLOCK_GATE_STEP` sleeps: a new sample or bytes freed end it; a pending interrupt ends it with the
/// block taken, `set_interrupt_probe`). Past that bound, when the R6 pick names this statement and no cancel is in
/// flight, it cancels its own statement as R6 would (`try_cancel_self`: the block then fails with 53200 in
/// `ledger_admit`); else it hands R2b / R6 to the pass (`ladder::block_hand_off`) and takes the block. Never under a
/// critical section or held interrupts (an LWLock holds them, `may_sleep`): those take the block at once, counted, as
/// does a thread with no running statement (a memory thread). No allocation, no free, no cgroup read.
#[cfg(any(feature = "std", test))]
#[cold]
#[inline(never)]
fn block_gate(n: usize) {
    let p = LEDGER.try_with(|c| c.get()).unwrap_or(core::ptr::null());
    if p.is_null() {
        return;
    }
    // SAFETY: as in `current`.
    let l = unsafe { &*p };
    let line = crate::ladder::cancel_line() as usize;
    let over = || mempolicy::admit::past_cancel_line(gate_projected(), n, line);
    if line == 0 || !l.on.load(Relaxed) || l.cancel_pct() != 0 || !armed() || !gate_on() || !over() {
        return;
    }
    if !may_sleep() {
        GATE_SKIPPED.fetch_add(1, Relaxed);
        return;
    }
    let ep = crate::ladder_stats::Wait::begin_at(crate::ladder_stats::Site::Block, n as u64, gate_projected() as u64);
    let (ran, busy) = crate::ladder::block_climb(gate_projected() as u64, n as u64);
    if ran != 0 {
        GATE_CLIMBS.fetch_add(1, Relaxed);
    }
    if busy {
        GATE_BUSY.fetch_add(1, Relaxed);
    }
    let (t0, t0_ns) = (std::time::Instant::now(), crate::ladder_stats::clock_ns());
    while over() {
        if l.cancel_pct() != 0 {
            // the pass canceled this statement meanwhile: its 53200 follows at this block (`ledger_admit`)
            ep.canceled();
            return;
        }
        if interrupt_pending() {
            // a cancel, a timeout or a die is pending: the block is taken and the caller's next interrupt check answers it
            ep.interrupted();
            return;
        }
        if mempolicy::admit::block_gate_spent(t0.elapsed()) {
            ep.last_try();
            let limit = LIMIT.load(Relaxed).max(1);
            let pct = (gate_projected().saturating_add(n).saturating_mul(100) / limit).clamp(1, 100);
            let min = mempolicy::consts::get(mempolicy::consts::K::CancelMin) as usize;
            let canceled = try_cancel_self(p, pct, min).unwrap_or_else(|Busy| {
                GATE_BUSY.fetch_add(1, Relaxed);
                None
            });
            if let Some((held, charged)) = canceled {
                ep.refused();
                let backend = l.slot.load(Relaxed).checked_sub(1).and_then(|i| SLOT_TABLE.get(i).map(|s| (i, s.pid.load(Relaxed))));
                let v = gate_projected().saturating_add(n) as u64;
                crate::ladder::block_canceled(v, n as u64, line as u64, limit as u64, (held as u64, charged as u64), backend);
            } else {
                // R2b and R6 are the pass's (one cancel in flight, D4374); the block is taken
                crate::ladder::block_hand_off();
                ep.overran();
            }
            return;
        }
        // a sleep, not a park: nothing wakes this wait, and a park would take an unpark meant for this thread's own waits
        std::thread::sleep(mempolicy::admit::BLOCK_GATE_STEP);
    }
    // a sample published during the wait ended it: the lag from that publish to this test (the sleep's cost)
    let at = GATE_SAMPLE_NS.load(Relaxed);
    if at > t0_ns {
        let lag = crate::ladder_stats::clock_ns().saturating_sub(at);
        GATE_LAGS.fetch_add(1, Relaxed);
        GATE_LAG_NS.fetch_add(lag, Relaxed);
        GATE_LAG_NS_TOP.fetch_max(lag, Relaxed);
    }
}

/// D4812 (2), the block gate past its bound: this thread's statement (`own`) takes R6's cancel itself when the R6 pick
/// (`pick_of`) names it and no cancel is in flight (D4374: none unanswered, none of the last cancels' bytes still
/// landing), as R6 would cancel it (`act_cancel`: the same word, rank and landing), so the pass sees it in flight and
/// cancels no second statement while it unwinds and frees. `Ok(Some((rank, charged)))` = canceled, its bytes as R6
/// ranked them; `Err(Busy)` = the pins table or the registry is held (not the pick). Never waits, no allocation. No
/// `condemn`: it stores the slot's word that ProcessInterrupts consumes (`take_condemned`), and with no interrupt raised
/// that word would stay and raise a second 53200 at a later CHECK_FOR_INTERRUPTS; this statement's 53200 is raised at
/// this block. No `note_pick` either: the stale-pick counters see the pass's picks only, by design (a self-cancel reads
/// no residency).
#[cfg(any(feature = "std", test))]
fn try_cancel_self(own: *const StmtMem, pct: usize, min: usize) -> Result<Option<(usize, usize)>, Busy> {
    if pending_bytes(decide_ms()) != 0 {
        return Ok(None);
    }
    let mut pins = locked(&PINS, false)?;
    part_pins(&mut pins);
    let r = locked(&REGISTRY, false)?;
    let now = watchdog_ms();
    // `pick_of` names none while a cancel younger than CANCEL_ACK_MS is unanswered
    let Some(big) = pick_of(&r, &pins, now, min).and_then(|i| r.get(i)?.upgrade()) else {
        return Ok(None);
    };
    drop(r);
    if !core::ptr::eq(alloc::sync::Arc::as_ptr(&big), own) {
        return Ok(None);
    }
    // Acquire: pairs with `next_generation`'s Release, after the statement's reset bytes
    let word = big.cancel.load(core::sync::atomic::Ordering::Acquire);
    let (b, charged) = (rank_of(&big, &pins), big.bytes.load(Relaxed).wrapping_add(big.cache_share() as isize).max(0) as usize);
    drop(pins);
    Ok(act_cancel(&big, word, b, charged, pct, min, now).map(|_| (b, charged)))
}


static CACHE: AtomicUsize = AtomicUsize::new(0);

/// D4795 (2a): a cache holder's bytes on a statement's part-cache share: its governor slot + 1 (0 = none), the share's
/// epoch when it charged, and the bytes still charged (the share holds them as `KiB` rounded up per holder, so a holder's
/// charges and releases net exactly). 16 bytes; plain data the holder keeps beside its bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheCharge {
    pub slot: u32,
    pub epoch: u32,
    pub bytes: u64,
}

/// The share's KiB field (`Slot::cache`'s low 32 bits: 4 TiB).
const CACHE_KIB: u64 = u32::MAX as u64;

#[inline]
fn kib(bytes: u64) -> u64 {
    bytes.div_ceil(1 << 10)
}

/// `n` cache bytes faulted: into the process footprint, and onto the running statement's count, which a limit and the
/// governor's decisions read (D3063 (2a)). With governor_cache_giveback on and a backend slot, the statement's part goes to
/// its slot's share and `rec` (the holder's) records it, so `release_cache` takes it back while that statement runs; a
/// fault while no statement runs is the process footprint's only.
pub fn charge_cache(n: usize, name: &'static str, rec: &mut CacheCharge) {
    if n == 0 {
        return;
    }
    CACHE.fetch_add(n, Relaxed);
    #[cfg(any(feature = "std", test))]
    {
        let p = LEDGER.try_with(|c| c.get()).unwrap_or(core::ptr::null());
        if !p.is_null() {
            // SAFETY: LEDGER points at a live StmtMem while set (the backend's own, or an `enter` guard's Arc).
            let l = unsafe { &*p };
            if let Some(i) = l.slot_index().filter(|_| mempolicy::consts::get(mempolicy::consts::K::GovernorCacheGiveback) != 0) {
                // the statement's part goes on the share only, never on the count itself: a fault at a statement boundary
                // lands on no statement, or on the share's new epoch with a record that takes it back
                crate::global_footprint::add_unledgered(n);
                if l.on.load(Relaxed) {
                    share_charge(i, n as u64, rec);
                    let share = l.cache_share();
                    l.note_consumer(name, share as usize);
                    l.rose(l.bytes.load(Relaxed).wrapping_add(own_pending(l)).wrapping_add(share as isize));
                }
                return;
            }
            if l.on.load(Relaxed) {
                crate::global_footprint::add(n);
                let total = l.cache.fetch_add(n, Relaxed).saturating_add(n);
                l.note_consumer(name, total);
                return;
            }
        }
    }
    #[cfg(not(any(feature = "std", test)))]
    let _ = (name, rec);
    crate::global_footprint::add(n);
}

/// `n` more bytes of `rec`'s holder on slot `i`'s share in one compare-exchange: a record of this statement grows, any
/// other is replaced (a refault keeps only the latest charge; the earlier statement keeps its part until its end).
#[cfg(any(feature = "std", test))]
fn share_charge(i: usize, n: u64, rec: &mut CacheCharge) {
    let w = &SLOT_TABLE[i].cache;
    let mut x = w.load(core::sync::atomic::Ordering::Acquire);
    loop {
        let e = (x >> 32) as u32;
        let before = if rec.slot as usize == i + 1 && rec.epoch == e { rec.bytes } else { 0 };
        let after = before.saturating_add(n);
        let (k, d) = (x & CACHE_KIB, kib(after) - kib(before));
        let nk = k.saturating_add(d).min(CACHE_KIB);
        match w.compare_exchange_weak(x, (x & !CACHE_KIB) | nk, core::sync::atomic::Ordering::AcqRel, core::sync::atomic::Ordering::Acquire) {
            Ok(_) => {
                // a full field took less: the record holds what it took, so its release nets to it
                let bytes = if nk - k == d { after } else { (kib(before) + (nk - k)) << 10 };
                *rec = CacheCharge { slot: (i + 1) as u32, epoch: e, bytes };
                return;
            }
            Err(v) => x = v,
        }
    }
}

/// `n` cache bytes freed (an eviction or a drop, on any thread): out of the process footprint, at most what
/// `charge_cache` put in (bytes cached before the cache was wired in were never charged); and, D4795 (2a), at most `rec`'s
/// bytes off the statement that charged them, only while that statement runs (its slot's epoch is the record's), checked
/// and subtracted in one compare-exchange, never below 0, no lock.
pub fn release_cache(n: usize, rec: &mut CacheCharge) {
    let prev = CACHE.fetch_update(Relaxed, Relaxed, |c| Some(c.saturating_sub(n))).unwrap_or(0);
    let m = n.min(prev);
    if m != 0 {
        crate::global_footprint::sub_process(m);
    }
    let Some(i) = (rec.slot as usize).checked_sub(1).filter(|&i| i < SLOTS) else { return };
    let (e, before) = (rec.epoch, rec.bytes);
    let after = before.saturating_sub(n as u64);
    *rec = if after == 0 { CacheCharge::default() } else { CacheCharge { bytes: after, ..*rec } };
    let d = kib(before) - kib(after);
    if d == 0 {
        return;
    }
    let _ = SLOT_TABLE[i].cache.fetch_update(core::sync::atomic::Ordering::AcqRel, core::sync::atomic::Ordering::Acquire, |x| {
        ((x >> 32) as u32 == e).then(|| (x & !CACHE_KIB) | (x & CACHE_KIB).saturating_sub(d))
    });
}

/// D4795 (2a): slot `i`'s share at a statement's start (the next epoch, no bytes) or end (the next epoch, its bytes kept
/// for the ended statement's reads): a release of an earlier epoch's record then takes nothing.
#[cfg(any(feature = "std", test))]
fn share_epoch(i: usize, keep: bool) {
    let _ = SLOT_TABLE[i].cache.fetch_update(core::sync::atomic::Ordering::AcqRel, core::sync::atomic::Ordering::Acquire, |x| {
        Some(((((x >> 32) as u32).wrapping_add(1) as u64) << 32) | if keep { x & CACHE_KIB } else { 0 })
    });
}

/// the statement error a process cache's check point returns (no allocation and no unwind: the caller
/// returns it as its own error) when this armed thread's statement is past its ledger's limit, or the process past the
/// governor's bar. A cache fault never raises (it runs under the cache's locks), and a scan that faults part after part
/// may allocate no memory-context block until its end, so the next fault's check point is where it stops: Gravity's
/// granule source checks at each part it opens and each dictionary it opens.
#[cfg(any(feature = "std", test))]
pub fn refusal(consumer: &'static str) -> Option<types_error::PgError> {
    if !armed() || std::thread::panicking() {
        return None;
    }
    let p = EXEMPT_PROBE.load(Relaxed);
    if !p.is_null() {
        // SAFETY: only set_exempt_probe stores here, always from fn() -> bool.
        let f: fn() -> bool = unsafe { core::mem::transmute(p) };
        if f() {
            return None;
        }
    }
    let l = LEDGER.try_with(|c| c.get()).unwrap_or(core::ptr::null());
    if !l.is_null() {
        // SAFETY: LEDGER points at a live StmtMem while set.
        let l = unsafe { &*l };
        if l.on.load(Relaxed) && !l.fits(0) {
            note_trip(TRIP_STATEMENT, consumer, 0);
            return Some(l.error(0));
        }
    }
    let bar = BAR.load(Relaxed);
    (bar != 0 && projected() > bar.saturating_add(bar / 20)).then(|| {
        note_trip(TRIP_CACHE, consumer, 0);
        crate::oom_named(consumer, 0)
    })
}

#[cfg(not(any(feature = "std", test)))]
pub fn refusal(_consumer: &'static str) -> Option<types_error::PgError> {
    None
}

pub fn cache_bytes() -> usize {
    CACHE.load(Relaxed)
}


#[derive(Default)]
pub struct StmtMem {
    bytes: core::sync::atomic::AtomicIsize,
    /// C8 move 3c (b): of `bytes`, the Gravity grant's promised room no draw backs yet (`Charge::promise`, `back`,
    /// `unback`): the statement's committed bytes are `bytes - room` (`committed`), what the projection's victim pick and
    /// the backstop's fallback rank read (policy 3.2: RUNNING counts committed bytes).
    room: core::sync::atomic::AtomicIsize,
    peak: core::sync::atomic::AtomicIsize,
    limit: AtomicUsize,
    on: core::sync::atomic::AtomicBool,
    /// The watchdog's cancel word: the percent it canceled at in the low `CANCEL_PCT` bits (0 = none), and above them
    /// the statement's generation, moved at every `statement_start` and `statement_end`, so a cancel chosen for one
    /// statement can never be stored on, or raised in, the next (D3821).
    cancel: AtomicUsize,
    /// When the watchdog stored its cancel (`watchdog_ms`); read only while `cancel` holds a percent.
    canceled_ms: core::sync::atomic::AtomicU64,
    /// D4812: the statement that ended last on this ledger had the watchdog's cancel (`statement_end`), until the next
    /// `statement_start` or `take_ended_condemned`: its backend's error recovery frees its memory with the server log
    /// deferred.
    ended_condemned: core::sync::atomic::AtomicBool,
    /// D3992: the backstop's ranking basis at its cancel (resident bytes) and the charge beside it, for the DETAIL.
    ranked: AtomicUsize,
    ranked_charged: AtomicUsize,
    /// The owner backend's governor slot + 1 (0 = none), set at `statement_start`, for the watchdog to raise its interrupt.
    slot: AtomicUsize,
    cache: AtomicUsize,
    /// C3: the charged bytes at which decide(Grow) is next due (`decide::tranche` past the last); isize::MAX: never.
    check_at: core::sync::atomic::AtomicIsize,
    /// C3: a charge crossed `check_at`; the next safe point of any of the statement's threads serves it (`serve_due`).
    due: core::sync::atomic::AtomicBool,
    /// C3: spill-self at this `SPILL_EPOCH` (0 = no): draws past the grant refused (breakers spill) until the epoch moves.
    spill_epoch: core::sync::atomic::AtomicU64,
    started_ms: core::sync::atomic::AtomicU64,
    top: AtomicUsize,
    #[cfg(any(feature = "std", test))]
    top_name: std::sync::Mutex<&'static str>,
    /// C5: the statement's revoke state (the spill-request registry's entry, `crate::revoke`).
    pub(crate) revoke: crate::revoke::Handle,
    /// D4439 (`draw_wait_cap`): the statement's draw-gate waits so far, all its threads (ns; since its start).
    draw_waited_ns: core::sync::atomic::AtomicU64,
    /// D4914: the statement's last refused draw past its grant, any of its threads: the branch (`ladder_stats::Why`) in
    /// the top byte, the bytes asked below it (0 = none since its start).
    refused: core::sync::atomic::AtomicU64,
}

impl core::fmt::Debug for StmtMem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "StmtMem({} of {} B)", self.bytes(), self.limit())
    }
}

impl StmtMem {
    pub fn bytes(&self) -> isize {
        self.bytes.load(Relaxed).wrapping_add(own_pending(self)).wrapping_add(self.cache_share() as isize)
    }

    /// D4795 (2a): the part-cache bytes its slot's share holds for it (`charge_cache`), 0 with no slot.
    fn cache_share(&self) -> u64 {
        self.slot_index().map_or(0, |i| (SLOT_TABLE[i].cache.load(Relaxed) & CACHE_KIB) << 10)
    }

    /// C8 move 3c (b): the charged bytes less the grant's room no draw backs (`room`): what the statement has committed.
    pub fn committed(&self) -> isize {
        self.bytes().wrapping_sub(self.room.load(Relaxed))
    }

    /// `committed` as the watchdog reads another statement's (the shared total, no thread's pending fold), at least 0.
    fn committed_shared(&self) -> usize {
        self.bytes.load(Relaxed).wrapping_add(self.cache_share() as isize).wrapping_sub(self.room.load(Relaxed)).max(0) as usize
    }

    /// D4395: the statement's bytes in memory as its backend's ledger row measures them since its start (heap the
    /// statement never asked for included), less those the residency pass last found not resident; 0 with no row.
    pub fn measured(&self) -> u64 {
        #[cfg(any(feature = "std", test))]
        {
            ledger_row(self).map_or(0, |(_, bytes, nr, _)| mempolicy::watchdog::resident_of(bytes, nr))
        }
        #[cfg(not(any(feature = "std", test)))]
        0
    }

    /// D4395: what a decision counts the statement as holding: its charge or its measured bytes, the more.
    pub fn held(&self) -> u64 {
        (self.bytes().max(0) as u64).max(self.measured())
    }

    /// `held` with the grant's untouched room left out of the charge.
    fn committed_held(&self) -> u64 {
        (self.committed_shared() as u64).max(self.measured())
    }

    pub fn peak(&self) -> isize {
        self.peak.load(Relaxed).max(self.bytes())
    }

    pub fn limit(&self) -> usize {
        self.limit.load(Relaxed)
    }

    pub fn largest(&self) -> (&'static str, usize) {
        #[cfg(any(feature = "std", test))]
        let name = self.top_name.lock().map(|g| *g).unwrap_or("");
        #[cfg(not(any(feature = "std", test)))]
        let name = "";
        (name, self.top.load(Relaxed))
    }

    #[inline]
    fn charge(&self, delta: isize) {
        let now = self.bytes.fetch_add(delta, Relaxed).wrapping_add(delta);
        if delta > 0 {
            self.rose(now.wrapping_add(self.cache_share() as isize));
        }
    }

    /// The charge rose to `now`: its peak, and C3's decide due once it crosses `check_at`.
    #[inline]
    fn rose(&self, now: isize) {
        self.peak.fetch_max(now, Relaxed);
        // C3: one load and a compare per fold or grant charge (never true under the threshold rule)
        if now >= self.check_at.load(Relaxed) && !self.due.load(Relaxed) {
            self.due.store(true, Relaxed);
        }
    }

    /// C3: decide(Grow) sent the statement down its own road and the backstop has not lifted it since.
    #[inline]
    fn spilling(&self) -> bool {
        let e = self.spill_epoch.load(Relaxed);
        e != 0 && e == SPILL_EPOCH.load(Relaxed)
    }

    fn note_consumer(&self, name: &'static str, bytes: usize) {
        if bytes > self.top.load(Relaxed) {
            self.top.store(bytes, Relaxed);
            // try_lock: a statement's workers grow contexts at once, and a preempted holder must not stall the others; the
            // name is the DETAIL's hint, the bytes stay exact.
            #[cfg(any(feature = "std", test))]
            if let Ok(mut g) = self.top_name.try_lock() {
                *g = name;
            }
            #[cfg(not(any(feature = "std", test)))]
            let _ = name;
        }
    }

    /// C5: the backend running it (its governor slot), if one registered.
    pub(crate) fn slot_index(&self) -> Option<usize> {
        self.slot.load(Relaxed).checked_sub(1).filter(|&i| i < SLOTS)
    }

    /// C5: the statement's generation (its cancel word's, moved at every start and end): an ask chosen for one statement
    /// is never stored on the next.
    pub(crate) fn statement_word(&self) -> usize {
        self.cancel.load(core::sync::atomic::Ordering::Acquire) >> CANCEL_GEN_SHIFT
    }

    /// C5: its revoke state.
    pub fn revoke(&self) -> &crate::revoke::Handle {
        &self.revoke
    }

    /// The percent the watchdog canceled this statement at (0 = not canceled).
    #[inline]
    fn cancel_pct(&self) -> usize {
        self.cancel.load(Relaxed) & CANCEL_PCT
    }

    /// The next generation's cancel word (none pending): a statement starts or ends.
    fn next_generation(&self) {
        let g = (self.cancel.load(Relaxed) >> CANCEL_GEN_SHIFT).wrapping_add(1);
        // Release: the watchdog that reads this word (Acquire) sees the bytes reset before it
        self.cancel.store(g << CANCEL_GEN_SHIFT, core::sync::atomic::Ordering::Release);
    }

    fn fits(&self, n: usize) -> bool {
        if self.cancel_pct() != 0 {
            return false;
        }
        let limit = self.limit.load(Relaxed);
        limit == 0 || self.held().saturating_add(n as u64) <= limit as u64
    }

    fn error(&self, n: usize) -> types_error::PgError {
        let (name, top) = self.largest();
        let held = self.held() as usize;
        let pct = self.cancel_pct();
        let projected = pct & CANCEL_PROJECTED != 0;
        let detail = if projected {
            alloc::format!(
                "The server's memory reached {}% of its limit and was projected to cross it before memory could be freed; this statement held the most past its fair share (peak {} kB); largest consumer \"{name}\" ({} kB).",
                pct & !CANCEL_PROJECTED,
                self.peak().max(0) as usize >> 10,
                top >> 10
            )
        } else if pct != 0 {
            alloc::format!(
                "The server's memory reached {pct}% of its limit and this statement held the most (peak {} kB); largest consumer \"{name}\" ({} kB); ranked by resident memory ({} kB resident, {} kB charged).",
                self.peak().max(0) as usize >> 10,
                top >> 10,
                self.ranked.load(Relaxed) >> 10,
                self.ranked_charged.load(Relaxed) >> 10
            )
        } else {
            alloc::format!(
                "Statement memory limit of {} kB exceeded on a request of {n} bytes ({} kB held); largest consumer \"{name}\" ({} kB).",
                self.limit.load(Relaxed) >> 10,
                held >> 10,
                top >> 10
            )
        };
        let e = types_error::PgError::error("out of memory").with_sqlstate(types_error::ERRCODE_OUT_OF_MEMORY).with_detail(detail);
        if projected {
            e.with_hint("pgrust.memory_cancel_rule = projection canceled the statement furthest past its fair share of memory.")
        } else if pct != 0 {
            e.with_hint("pgrust.memory_cancel_threshold canceled the statement holding the most memory.")
        } else {
            e
        }
    }
}

#[cfg(any(feature = "std", test))]
static REGISTRY: std::sync::Mutex<alloc::vec::Vec<alloc::sync::Weak<StmtMem>>> = std::sync::Mutex::new(alloc::vec::Vec::new());

pub fn statement_start(limit: usize, watch: bool) {
    #[cfg(any(feature = "std", test))]
    {
        flush_pending();
        crate::revoke::clear_own_bit();
        let on = limit != 0 || watch;
        let _ = OWN.try_with(|own| {
            let mut own = own.borrow_mut();
            if own.is_none() {
                if !on {
                    return;
                }
                let l = alloc::sync::Arc::new(StmtMem::default());
                if let Ok(mut r) = REGISTRY.lock() {
                    r.retain(|w| w.strong_count() != 0);
                    r.push(alloc::sync::Arc::downgrade(&l));
                }
                *own = Some(l);
            }
            let l = own.as_ref().unwrap();
            l.bytes.store(0, Relaxed);
            l.room.store(0, Relaxed);
            l.peak.store(0, Relaxed);
            l.top.store(0, Relaxed);
            l.ended_condemned.store(false, Relaxed);
            l.next_generation();
            l.cache.store(0, Relaxed);
            l.revoke.reset();
            l.draw_waited_ns.store(0, Relaxed);
            l.refused.store(0, Relaxed);
            start_credit(l);
            l.slot.store(SLOT.try_with(|c| c.get()).map_or(0, |i| usize::try_from(i).map_or(0, |i| i + 1)), Relaxed);
            if let Some(i) = l.slot_index() {
                share_epoch(i, false);
            }
            l.limit.store(limit, Relaxed);
            l.on.store(on, Relaxed);
            let p: *const StmtMem = &**l;
            let _ = LEDGER.try_with(|c| c.set(p));
        });
    }
    #[cfg(not(any(feature = "std", test)))]
    let _ = (limit, watch);
}

pub fn statement_end() {
    #[cfg(any(feature = "std", test))]
    flush_pending();
    crate::revoke::clear_own_bit();
    #[cfg(any(feature = "std", test))]
    let _ = OWN.try_with(|own| {
        if let Some(l) = own.borrow().as_ref() {
            // REVOKESLA: the statement's last ask settled by its end (its `revoke-sla:` line)
            crate::revoke::settle_end(l);
            l.on.store(false, Relaxed);
            // a second end (the statement boundary's, after the message's) keeps the first's answer
            if l.cancel_pct() != 0 {
                l.ended_condemned.store(true, Relaxed);
            }
            l.next_generation();
            if let Some(i) = l.slot_index() {
                share_epoch(i, true);
            }
        }
    });
}

/// D4812: whether this thread's last statement ended with the memory watchdog's cancel on it (its 53200 raised or not),
/// consumed: its error recovery frees the statement's memory with the server log deferred. One TLS read.
pub fn take_ended_condemned() -> bool {
    #[cfg(any(feature = "std", test))]
    {
        OWN.try_with(|own| own.borrow().as_ref().is_some_and(|l| l.ended_condemned.swap(false, Relaxed))).unwrap_or(false)
    }
    #[cfg(not(any(feature = "std", test)))]
    false
}

/// `f` over this thread's charged ledger (a claim's: its leader's), without taking a reference: one TLS read.
#[inline]
pub(crate) fn with_current<T>(f: impl FnOnce(&StmtMem) -> T) -> Option<T> {
    #[cfg(any(feature = "std", test))]
    {
        let p = LEDGER.try_with(|c| c.get()).unwrap_or(core::ptr::null());
        // SAFETY: as in `current`.
        let l = (!p.is_null()).then(|| unsafe { &*p })?;
        l.on.load(Relaxed).then(|| f(l))
    }
    #[cfg(not(any(feature = "std", test)))]
    {
        let _ = f;
        None
    }
}

/// C5: `f` over every running statement's ledger (the registry's live entries), under the registry's lock: no allocation.
#[cfg(any(feature = "std", test))]
pub(crate) fn for_each_live(f: &mut dyn FnMut(&StmtMem)) {
    let _ = live_walk(true, f);
}

/// D4812 (2): `for_each_live` with the registry taken by try_lock (the block gate never waits on it): `Err(Busy)` when it
/// is held, and `f` never ran.
#[cfg(any(feature = "std", test))]
pub(crate) fn try_for_each_live(f: &mut dyn FnMut(&StmtMem)) -> Result<(), Busy> {
    live_walk(false, f)
}

#[cfg(any(feature = "std", test))]
fn live_walk(wait: bool, f: &mut dyn FnMut(&StmtMem)) -> Result<(), Busy> {
    let r = locked(&REGISTRY, wait)?;
    for w in r.iter() {
        if let Some(l) = w.upgrade() {
            if l.on.load(Relaxed) {
                f(&l);
            }
        }
    }
    Ok(())
}

pub fn current() -> Option<alloc::sync::Arc<StmtMem>> {
    #[cfg(any(feature = "std", test))]
    {
        let p = LEDGER.try_with(|c| c.get()).unwrap_or(core::ptr::null());
        // SAFETY: a non-null LEDGER points into an Arc that this thread's OWN or an `Entered` guard keeps alive.
        if p.is_null() || !unsafe { &*p }.on.load(Relaxed) {
            return None;
        }
        unsafe {
            alloc::sync::Arc::increment_strong_count(p);
            Some(alloc::sync::Arc::from_raw(p))
        }
    }
    #[cfg(not(any(feature = "std", test)))]
    None
}

#[must_use = "the thread runs under the ledger only while the guard lives"]
pub struct Entered {
    #[cfg(any(feature = "std", test))]
    prev: (*const StmtMem, bool),
    _ledger: Option<alloc::sync::Arc<StmtMem>>,
}

pub fn enter(ledger: Option<alloc::sync::Arc<StmtMem>>) -> Entered {
    #[cfg(any(feature = "std", test))]
    {
        let p = ledger.as_deref().map_or(core::ptr::null(), |l| l as *const StmtMem);
        flush_pending();
        let prev_l = LEDGER.try_with(|c| c.replace(p)).unwrap_or(core::ptr::null());
        let prev_a = armed();
        arm(true);
        Entered { prev: (prev_l, prev_a), _ledger: ledger }
    }
    #[cfg(not(any(feature = "std", test)))]
    Entered { _ledger: ledger }
}

impl Drop for Entered {
    fn drop(&mut self) {
        #[cfg(any(feature = "std", test))]
        {
            flush_pending();
            let _ = LEDGER.try_with(|c| c.set(self.prev.0));
            arm(self.prev.1);
        }
    }
}

#[inline]
pub(crate) fn ledger_charge(delta: isize) {
    #[cfg(any(feature = "std", test))]
    {
        let p = LEDGER.try_with(|c| c.get()).unwrap_or(core::ptr::null());
        if !p.is_null() {
            // SAFETY: as in `current`.
            let l = unsafe { &*p };
            if l.on.load(Relaxed) {
                let batched = PENDING.try_with(|c| {
                    let v = c.get().wrapping_add(delta);
                    if v >= LEDGER_SLACK || v <= -LEDGER_SLACK {
                        c.set(0);
                        l.charge(v);
                    } else {
                        c.set(v);
                    }
                });
                if batched.is_err() {
                    l.charge(delta);
                }
            }
        }
    }
    #[cfg(not(any(feature = "std", test)))]
    let _ = delta;
}

#[cfg(any(feature = "std", test))]
fn flush_pending() {
    let v = PENDING.try_with(|c| c.replace(0)).unwrap_or(0);
    let p = LEDGER.try_with(|c| c.get()).unwrap_or(core::ptr::null());
    if v != 0 && !p.is_null() {
        // SAFETY: as in `current`.
        let l = unsafe { &*p };
        if l.on.load(Relaxed) {
            l.charge(v);
        }
    }
}

fn own_pending(l: &StmtMem) -> isize {
    #[cfg(any(feature = "std", test))]
    {
        if LEDGER.try_with(|c| c.get()).unwrap_or(core::ptr::null()) == l as *const StmtMem {
            return PENDING.try_with(|c| c.get()).unwrap_or(0);
        }
    }
    #[cfg(not(any(feature = "std", test)))]
    let _ = l;
    0
}

#[inline]
fn ledger_admit(n: usize, context_name: &'static str, context_bytes: usize) {
    #[cfg(any(feature = "std", test))]
    {
        let p = LEDGER.try_with(|c| c.get()).unwrap_or(core::ptr::null());
        if !p.is_null() {
            // SAFETY: as in `current`.
            let l = unsafe { &*p };
            if l.on.load(Relaxed)
                && (context_bytes.saturating_add(n) > l.top.load(Relaxed)
                    || l.cancel_pct() != 0
                    || l.limit.load(Relaxed) != 0
                    || l.due.load(Relaxed))
            {
                ledger_admit_slow(l, n, context_name, context_bytes);
            }
        }
    }
    #[cfg(not(any(feature = "std", test)))]
    let _ = (n, context_name, context_bytes);
}

/// Whether an ERROR raised here would stay an ERROR: not unwinding, and not in a critical section (the exempt probe).
fn raise_ok() -> bool {
    #[cfg(any(feature = "std", test))]
    {
        if std::thread::panicking() {
            return false;
        }
        let p = EXEMPT_PROBE.load(Relaxed);
        if !p.is_null() {
            // SAFETY: only set_exempt_probe stores here, always from fn() -> bool.
            let f: fn() -> bool = unsafe { core::mem::transmute(p) };
            if f() {
                return false;
            }
        }
        true
    }
    #[cfg(not(any(feature = "std", test)))]
    false
}

#[cold]
#[inline(never)]
fn ledger_admit_slow(l: &StmtMem, n: usize, context_name: &'static str, context_bytes: usize) {
    l.note_consumer(context_name, context_bytes.saturating_add(n));
    // C3: served at an armed block outside a critical section and any unwind; a self-cancel fails this block below
    if l.due.load(Relaxed) && armed() && raise_ok() {
        serve_due(l);
    }
    if l.fits(n) || !armed() {
        return;
    }
    #[cfg(any(feature = "std", test))]
    {
        if !raise_ok() {
            return;
        }
        note_trip(TRIP_STATEMENT, context_name, n);
        arm(false);
        std::panic::panic_any(alloc::boxed::Box::new(l.error(n)));
    }
}

/// Bytes outside memory contexts charged to the statement that took them (the Gravity grant's pool bytes), given back to
/// that same ledger at the drop. `fits` is the draw-time check: a draw it refuses spills or falls through, never raises.
pub struct Charge {
    ledger: Option<alloc::sync::Arc<StmtMem>>,
    bytes: core::cell::Cell<usize>,
    /// C8 move 3c (b): of `bytes`, the promised room no draw backs (`promise`, `grow_room`, `back`, `unback`), on the
    /// statement's `room` too; 0 for a charge of committed bytes (`new`).
    room: core::cell::Cell<usize>,
    name: &'static str,
}

impl core::fmt::Debug for Charge {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Charge({} B)", self.bytes.get())
    }
}

impl Charge {
    pub const NONE: Charge = Charge { ledger: None, bytes: core::cell::Cell::new(0), room: core::cell::Cell::new(0), name: "" };

    pub fn new(name: &'static str, bytes: usize) -> Charge {
        let c = Charge { ledger: current(), bytes: core::cell::Cell::new(0), room: core::cell::Cell::new(0), name };
        c.grow(bytes);
        c
    }

    /// C8 move 3c (b): `bytes` of promised room (the Gravity grant's reservation): charged as `new` charges, and room
    /// until a draw backs it (`back`), so the statement's `committed` leaves them out.
    pub fn promise(name: &'static str, bytes: usize) -> Charge {
        let c = Charge::new(name, 0);
        c.grow_room(bytes);
        c
    }

    /// C8 move 3c (b): `n` more promised room (a lent room taken back).
    pub fn grow_room(&self, n: usize) {
        if let Some(l) = &self.ledger {
            self.grow(n);
            self.room.set(self.room.get() + n);
            l.room.fetch_add(n as isize, Relaxed);
        }
    }

    /// C8 move 3c (b): up to `n` of the room backed by a draw's pages (committed now).
    pub fn back(&self, n: usize) {
        if let Some(l) = &self.ledger {
            let n = n.min(self.room.get());
            self.room.set(self.room.get() - n);
            l.room.fetch_sub(n as isize, Relaxed);
        }
    }

    /// C8 move 3c (b): up to `n` backed bytes promised room again (the draw given back).
    pub fn unback(&self, n: usize) {
        if let Some(l) = &self.ledger {
            let n = n.min(self.bytes.get() - self.room.get());
            self.room.set(self.room.get() + n);
            l.room.fetch_add(n as isize, Relaxed);
        }
    }

    pub fn grow(&self, n: usize) {
        if let Some(l) = &self.ledger {
            l.charge(n as isize);
            self.bytes.set(self.bytes.get() + n);
            l.note_consumer(self.name, self.bytes.get());
        }
    }

    /// `n` of the charged bytes given back before the drop (at most what is charged): the room first (C8 move 3c (b):
    /// a lend gives back room no draw backs).
    pub fn shrink(&self, n: usize) {
        if let Some(l) = &self.ledger {
            let n = n.min(self.bytes.get());
            let r = n.min(self.room.get());
            self.room.set(self.room.get() - r);
            l.room.fetch_sub(r as isize, Relaxed);
            l.charge((n as isize).wrapping_neg());
            self.bytes.set(self.bytes.get() - n);
        }
    }

    /// Whether `n` more bytes fit the statement's limit (no cancel pending, no spill-self); serves a decide due (C3).
    pub fn fits(&self, n: usize) -> bool {
        self.ledger.as_ref().is_none_or(|l| {
            if l.due.load(Relaxed) {
                serve_due(l);
            }
            l.fits(n) && !l.spilling()
        })
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        if let Some(l) = &self.ledger {
            l.room.fetch_sub(self.room.get() as isize, Relaxed);
            l.charge((self.bytes.get() as isize).wrapping_neg());
        }
    }
}

/// C5's act hook (the ladder's one call, `crate::revoke::request_revoke`).
pub use crate::revoke::request_revoke;

/// Each charged statement a backend runs, as `(its governor slot, bytes, peak)`: the numbers the watchdog's cancel ranks.
pub fn for_each_statement(mut f: impl FnMut(usize, isize, isize)) {
    #[cfg(any(feature = "std", test))]
    {
        let Ok(r) = REGISTRY.lock() else { return };
        let live: alloc::vec::Vec<_> = r.iter().filter_map(alloc::sync::Weak::upgrade).collect();
        drop(r);
        for l in live.iter().filter(|l| l.on.load(Relaxed)) {
            if let Some(slot) = l.slot.load(Relaxed).checked_sub(1) {
                f(slot, l.bytes(), l.peak());
            }
        }
    }
    #[cfg(not(any(feature = "std", test)))]
    let _ = &mut f;
}

/// The watchdog's cancel (D3821): the statement it canceled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Victim {
    /// Its ranking bytes when chosen (the backstop's: resident) and its charge then.
    pub bytes: usize,
    pub charged: usize,
    /// Its cancel word once canceled (`condemn` names it).
    pub word: usize,
    /// The backend running it (`register`'s slot and pid), whose interrupt the watchdog raises; `None` for a ledger no
    /// backend registered.
    pub backend: Option<(usize, i32)>,
    /// memfuzz (P4): the live statements the pick ranked highest, largest first (`Cand::rank` 0 = none), so a cancel's
    /// line shows whether the victim outranked the others by its rank's own terms (an innocent kill reads from it).
    pub top: [Cand; PICK_TOP],
}

/// The candidates `Victim::top` keeps.
pub const PICK_TOP: usize = 3;

/// One of R6's ranked candidates at its pick: its backend (`None`: no backend registered), its rank (`ranks_of`: the
/// bytes the pick compared), the resident bytes and the part pins that rank counts (the backend's row), and whether a
/// cancel was already pending on it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cand {
    pub backend: Option<(usize, i32)>,
    pub rank: u64,
    pub resident: u64,
    pub parts: u64,
    pub canceled: bool,
}

/// Milliseconds on the watchdog's clock, from 1.
#[cfg(any(feature = "std", test))]
fn watchdog_ms() -> u64 {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 + 1
}

/// The watchdog's cancel: `cancel_largest_victim`'s bytes, or 0.
pub fn cancel_largest(pct: usize, min: usize) -> usize {
    cancel_largest_victim(pct, min).map_or(0, |v| v.bytes)
}

/// The watchdog's cancel: the running statement with the largest charged ledger (at least `min` bytes) is asked to fail
/// with 53200 (`pct` names the threshold in its DETAIL): at its next block and grant draw, and, once the caller raises
/// its backend's interrupt and Gravity abort byte (procsignal `RaiseAbortByProcNumber`), at its next interrupt poll or
/// kernel back-edge, through its condemnation (D3821). The cancel lands only on the statement it was chosen for: the
/// word is read before the bytes that qualify it and stored by a compare-exchange, and a statement that starts or ends
/// meanwhile has moved the word's generation, so the store fails. Which statement, and whether a pending cancel still
/// blocks, is `mempolicy::watchdog::cancel_victim`'s. `None` when none qualified, a cancel is pending, or the chosen
/// statement ended meanwhile.
pub fn cancel_largest_victim(pct: usize, min: usize) -> Option<Victim> {
    pick_victim(pct, min, true).ok().flatten()
}

/// D4029 (d): the backstop's `cancel_largest_victim`: `Err(Busy)` when the registry or the pins table is locked (the
/// next sample retries); no allocation.
pub fn try_cancel_largest_victim(pct: usize, min: usize) -> Result<Option<Victim>, Busy> {
    pick_victim(pct, min, false)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Busy;

#[cfg(any(feature = "std", test))]
fn locked<T>(m: &'static std::sync::Mutex<T>, wait: bool) -> Result<std::sync::MutexGuard<'static, T>, Busy> {
    if wait { m.lock().map_err(|_| Busy) } else { m.try_lock().map_err(|_| Busy) }
}

fn pick_victim(pct: usize, min: usize, wait: bool) -> Result<Option<Victim>, Busy> {
    #[cfg(any(feature = "std", test))]
    {
        // D4812: the stale statements go to the residency thread, and the owner tree's shadow line is written, once
        // the pins table and the registry are free again and the cancel is raised (act first, then log)
        let (mut hints, mut shadow) = (NO_HINTS, None);
        let v = pick_held(pct, min, wait, &mut hints, &mut shadow);
        post_hints(&hints);
        if let Some((today, tree, now)) = shadow {
            crate::ledger2::note(crate::ledger2::Decision::Cancel, today, tree, now);
        }
        v
    }
    #[cfg(not(any(feature = "std", test)))]
    {
        let _ = (pct, min, wait);
        Ok(None)
    }
}

/// `pick_victim` under the pins table and the registry; the stale statements it found in `hints`, the owner tree's
/// shadow decision in `shadow`.
#[cfg(any(feature = "std", test))]
fn pick_held(
    pct: usize,
    min: usize,
    wait: bool,
    hints: &mut Hints,
    shadow: &mut Option<(crate::ledger2::Choice, crate::ledger2::Choice, u64)>,
) -> Result<Option<Victim>, Busy> {
    let mut pins = locked(&PINS, wait)?;
    part_pins(&mut pins);
    let r = locked(&REGISTRY, wait)?;
    let now = watchdog_ms();
    let i = pick_of(&r, &pins, now, min);
    // D4059: a statement whose regions filled since the residency pass last read them ranks under its resident bytes
    // until the next read. D4812: the pick reads none of them here (each mincore allocates a kernel page, which under
    // host memory pressure waited in direct reclaim for seconds); the samples of a ladder episode hand those that
    // could outrank the pick to the residency thread before it (`try_hint_stale`), the pick hands the ones still
    // stale, and a cancel made while one was is counted stale
    *hints = stale_of(&r, &pins, i, min, now);
    let stale = hints[0].0 != 0;
    // LEDGER2 step 4: the owner tree's pick beside it (the shadow line when they differ, written by pick_victim)
    *shadow = Some(shadow_pick(&r, &pins, i, min, now));
    let Some(big) = i.and_then(|i| r.get(i)?.upgrade()) else {
        return Ok(None);
    };
    let top = top_of(&r, &pins);
    drop(r);
    // Acquire: pairs with `next_generation`'s Release, after the statement's reset bytes
    let word = big.cancel.load(core::sync::atomic::Ordering::Acquire);
    let (b, charged) = (rank_of(&big, &pins), big.bytes.load(Relaxed).wrapping_add(big.cache_share() as isize).max(0) as usize);
    drop(pins);
    let Some(canceled) = act_cancel(&big, word, b, charged, pct, min, now) else {
        return Ok(None);
    };
    note_pick(stale, now);
    let backend = big.slot.load(Relaxed).checked_sub(1).and_then(|i| SLOT_TABLE.get(i).map(|s| (i, s.pid.load(Relaxed))));
    if let Some((slot, _)) = backend {
        condemn(slot, canceled);
    }
    Ok(Some(Victim { bytes: b, charged, word: canceled, backend, top }))
}

/// memfuzz (P4): the `PICK_TOP` live statements of `r` with the largest ranks, largest first, each with its rank's terms
/// (`cand_of`); under the pick's locks, no allocation.
#[cfg(any(feature = "std", test))]
fn top_of(r: &[alloc::sync::Weak<StmtMem>], pins: &[u64; PIN_SLOTS]) -> [Cand; PICK_TOP] {
    let mut top = [Cand::default(); PICK_TOP];
    for l in r.iter().filter_map(alloc::sync::Weak::upgrade).filter(|l| l.on.load(Relaxed)) {
        let c = cand_of(&l, pins);
        if let Some(at) = top.iter().position(|t| c.rank > t.rank) {
            top.copy_within(at..PICK_TOP - 1, at + 1);
            top[at] = c;
        }
    }
    top
}

/// One candidate's rank and its terms: the backend's resident bytes (its statement's committed bytes less those the
/// residency pass found not resident) and its part pins, as `ranks_of` reads them.
#[cfg(any(feature = "std", test))]
fn cand_of(l: &StmtMem, pins: &[u64; PIN_SLOTS]) -> Cand {
    let row = ledger_row(l);
    let backend = l.slot.load(Relaxed).checked_sub(1).and_then(|i| SLOT_TABLE.get(i).map(|s| (i, s.pid.load(Relaxed))));
    Cand {
        backend,
        rank: rank_of(l, pins) as u64,
        resident: row.map_or(0, |(_, bytes, nr, _)| mempolicy::watchdog::resident_of(bytes, nr)),
        parts: row.map_or(0, |(a, ..)| pins.get(a as usize).copied().unwrap_or(0)),
        canceled: l.cancel_pct() != 0,
    }
}

/// R6's act on the pick `big` (its cancel word `word`, read before its rank `b` and charged bytes): unless it no longer
/// qualifies, the word set to `pct` by a compare-exchange (a statement that started or ended meanwhile moved it), its
/// rank and charged bytes kept, and (D4023: the victim answers at once, its bytes leave resident over seconds) its bytes
/// counted landing for `cancel_landing_ms`. The cancel word set; `None` when it did not qualify or the word moved.
#[cfg(any(feature = "std", test))]
fn act_cancel(big: &StmtMem, word: usize, b: usize, charged: usize, pct: usize, min: usize, now: u64) -> Option<usize> {
    if !big.on.load(Relaxed) || b < min.max(1) || word & CANCEL_PCT != 0 {
        return None;
    }
    let canceled = word | pct.clamp(1, CANCEL_PCT);
    big.ranked.store(b, Relaxed);
    big.ranked_charged.store(charged, Relaxed);
    big.canceled_ms.store(now, Relaxed);
    big.cancel.compare_exchange(word, canceled, core::sync::atomic::Ordering::AcqRel, Relaxed).ok()?;
    pending_add(b as u64, mempolicy::consts::get(mempolicy::consts::K::CancelLandingMs), decide_ms());
    Some(canceled)
}

/// D4262: the largest live statement's bytes as the victim pick ranks them (`rank_of`), for the backstop's who-acts rule
/// (`mempolicy::watchdog::acts`); `Busy` when the pins table or the registry is held (the next sample retries).
pub fn try_largest_live() -> Result<usize, Busy> {
    #[cfg(any(feature = "std", test))]
    {
        let mut pins = locked(&PINS, false)?;
        part_pins(&mut pins);
        let r = locked(&REGISTRY, false)?;
        Ok(r.iter().filter_map(alloc::sync::Weak::upgrade).filter(|l| l.on.load(Relaxed)).map(|l| rank_of(&l, &pins)).max().unwrap_or(0))
    }
    #[cfg(not(any(feature = "std", test)))]
    Ok(0)
}

#[cfg(any(feature = "std", test))]
fn rank_of(l: &StmtMem, pins: &[u64; PIN_SLOTS]) -> usize {
    ranks_of(l, pins).0
}

/// `rank_of`, and (D4059) the rank `l` would have with none of its ledger bytes not resident (every range of its read
/// resident again): at least the first, equal when it has no bytes not resident or they do not enter its rank.
#[cfg(any(feature = "std", test))]
fn ranks_of(l: &StmtMem, pins: &[u64; PIN_SLOTS]) -> (usize, usize) {
    let row = ledger_row(l);
    let parts = row.map_or(0, |(a, ..)| pins.get(a as usize).copied().unwrap_or(0));
    // C8 move 3c (b): the fallback (no row, a row below its start) ranks by the committed charge, not the grant's room
    // LEDGER2 step 4: under `pgrust.memory_ledger_v2` the rank is max(what the owner tree says a cancel frees, the
    // backend's row with its parts): the tree replaces the charge fallback (no row, a row below its start), which ranked
    // a statement by bytes it was charged but did not hold (D4374's bystander)
    let v2 = crate::ledger2::on();
    let tree = if v2 { crate::ledger2::owned(l.slot_index()) as usize } else { 0 };
    let charged = if v2 { 0 } else { l.committed_shared() as u64 };
    let rank = |nr: i64| {
        let resident = row.map(|(_, bytes, _, net)| (mempolicy::watchdog::resident_of(bytes, nr), net));
        (mempolicy::watchdog::rank_bytes(resident, parts, charged) as usize).max(tree)
    };
    (rank(row.map_or(0, |(_, _, nr, _)| nr)), rank(0))
}

/// LEDGER2 step 4: R6's pick `i` (today's) beside the owner tree's (the live statement no cancel is pending on whose tree
/// bytes are the most, at least `min`), with `now`: one shadow decision, for `ledger2::note` once the locks are free.
#[cfg(any(feature = "std", test))]
fn shadow_pick(
    r: &[alloc::sync::Weak<StmtMem>],
    pins: &[u64; PIN_SLOTS],
    i: Option<usize>,
    min: usize,
    now: u64,
) -> (crate::ledger2::Choice, crate::ledger2::Choice, u64) {
    let today = i.and_then(|i| r.get(i)?.upgrade()).map_or((None, 0), |l| (l.slot_index(), rank_of(&l, pins) as u64));
    let mut tree: crate::ledger2::Choice = (None, 0);
    for l in r.iter().filter_map(alloc::sync::Weak::upgrade).filter(|l| l.on.load(Relaxed) && l.cancel_pct() == 0) {
        let b = crate::ledger2::owned(l.slot_index());
        if b >= min.max(1) as u64 && b > tree.1 {
            tree = (l.slot_index(), b);
        }
    }
    (today, tree, now)
}

/// The statement `cancel_largest_victim` picks: `mempolicy::watchdog::cancel_victim` over the live statements' ranks.
#[cfg(any(feature = "std", test))]
fn pick_of(r: &[alloc::sync::Weak<StmtMem>], pins: &[u64; PIN_SLOTS], now: u64, min: usize) -> Option<usize> {
    let stmts = r.iter().map(|w| {
        w.upgrade().filter(|l| l.on.load(Relaxed)).map_or((0, 0), |l| {
            let at = if l.cancel_pct() != 0 { l.canceled_ms.load(Relaxed).max(1) } else { 0 };
            (rank_of(&l, pins), at)
        })
    });
    mempolicy::watchdog::cancel_victim(stmts, now, min)
}

/// D4812: a watchdog sample of a ladder episode (its read past LOW; `opens` = the first): the statements that could
/// outrank the pick a cancel would make now go to the residency thread (`stale_of`, `post_hints`), so R6's pick later
/// ranks them by a fresh read; nothing is canceled. `Err(Busy)` (counted) when the pins table or the registry is held: the sample
/// skips it, never waits. The hints posted.
pub fn try_hint_stale(min: usize, opens: bool) -> Result<u64, Busy> {
    if opens {
        HINT_EPISODES.fetch_add(1, Relaxed);
        HINT_EPISODE_SAMPLES.store(0, Relaxed);
        HINT_EPISODE_HINTS.store(0, Relaxed);
    }
    HINT_EPISODE_SAMPLES.fetch_add(1, Relaxed);
    #[cfg(any(feature = "std", test))]
    {
        let held = locked(&PINS, false).and_then(|mut pins| {
            part_pins(&mut pins);
            let r = locked(&REGISTRY, false)?;
            let now = watchdog_ms();
            let i = pick_of(&r, &pins, now, min);
            Ok(stale_of(&r, &pins, i, min, now))
        });
        let Ok(hints) = held else {
            HINT_SAMPLES_BUSY.fetch_add(1, Relaxed);
            return Err(Busy);
        };
        // the locks are free again: each hint's wake is a syscall
        let n = post_hints(&hints);
        HINT_SAMPLES.fetch_add(1, Relaxed);
        HINT_SAMPLE_HINTS.fetch_add(n, Relaxed);
        HINT_EPISODE_HINTS.fetch_add(n, Relaxed);
        Ok(n)
    }
    #[cfg(not(any(feature = "std", test)))]
    {
        let _ = min;
        Ok(0)
    }
}

/// D4812: `try_hint_stale`'s samples that ran and those a busy lock skipped, the hints they posted, the episodes they
/// ran in, and the last episode's samples and hints.
static HINT_SAMPLES: AtomicU64 = AtomicU64::new(0);
static HINT_SAMPLES_BUSY: AtomicU64 = AtomicU64::new(0);
static HINT_SAMPLE_HINTS: AtomicU64 = AtomicU64::new(0);
static HINT_EPISODES: AtomicU64 = AtomicU64::new(0);
static HINT_EPISODE_SAMPLES: AtomicU64 = AtomicU64::new(0);
static HINT_EPISODE_HINTS: AtomicU64 = AtomicU64::new(0);

/// D4812: `try_hint_stale`'s figures (`pgrust_memory_ladder_stats()`'s cancel / sample_hint rows).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SampleHintCounts {
    pub samples: u64,
    pub busy: u64,
    pub hints: u64,
    pub episodes: u64,
    pub last_episode_samples: u64,
    pub last_episode_hints: u64,
}

pub fn sample_hint_counts() -> SampleHintCounts {
    SampleHintCounts {
        samples: HINT_SAMPLES.load(Relaxed),
        busy: HINT_SAMPLES_BUSY.load(Relaxed),
        hints: HINT_SAMPLE_HINTS.load(Relaxed),
        episodes: HINT_EPISODES.load(Relaxed),
        last_episode_samples: HINT_EPISODE_SAMPLES.load(Relaxed),
        last_episode_hints: HINT_EPISODE_HINTS.load(Relaxed),
    }
}

/// D4812: the cancels a pick issued, those issued while a statement that could outrank the pick waited for the
/// residency thread's read (`stale_of`), the watchdog's clock at that thread's last full pass (`note_residency_read`;
/// 0 = none yet; an account a hint read again since is fresher), and that pass's age at the last cancel and the oldest
/// at any (ms).
static PICKS: AtomicU64 = AtomicU64::new(0);
static STALE_PICKS: AtomicU64 = AtomicU64::new(0);
#[cfg(any(feature = "std", test))]
static RESIDENCY_READ_MS: AtomicU64 = AtomicU64::new(0);
static FULL_PASS_AGE_LAST_MS: AtomicU64 = AtomicU64::new(0);
static FULL_PASS_AGE_OLDEST_MS: AtomicU64 = AtomicU64::new(0);

/// D4812: the residency thread ended a full pass (memruntime's pg:memresident).
pub fn note_residency_read() {
    #[cfg(any(feature = "std", test))]
    RESIDENCY_READ_MS.store(watchdog_ms(), Relaxed);
}

#[cfg(any(feature = "std", test))]
fn note_pick(stale: bool, now: u64) {
    PICKS.fetch_add(1, Relaxed);
    if stale {
        STALE_PICKS.fetch_add(1, Relaxed);
    }
    let age = now.saturating_sub(RESIDENCY_READ_MS.load(Relaxed));
    FULL_PASS_AGE_LAST_MS.store(age, Relaxed);
    FULL_PASS_AGE_OLDEST_MS.fetch_max(age, Relaxed);
}

/// D4812: the cancel picks' residency figures (`pgrust_memory_ladder_stats()`'s cancel / stale_pick and
/// full_pass_age_at_pick rows).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PickCounts {
    pub picks: u64,
    pub stale_picks: u64,
    /// The residency thread's last full pass, its age at the last cancel and the oldest at any (ms; the whole run
    /// before the first pass).
    pub full_pass_age_last_ms: u64,
    pub full_pass_age_oldest_ms: u64,
}

pub fn pick_counts() -> PickCounts {
    PickCounts {
        picks: PICKS.load(Relaxed),
        stale_picks: STALE_PICKS.load(Relaxed),
        full_pass_age_last_ms: FULL_PASS_AGE_LAST_MS.load(Relaxed),
        full_pass_age_oldest_ms: FULL_PASS_AGE_OLDEST_MS.load(Relaxed),
    }
}

/// D4812: the stale statements a pick hands to the residency thread: (their rank once read, their account), largest
/// first; a 0 rank ends the list.
type Hints = [(usize, u32); mempolicy::watchdog::STALE_REFRESH_MAX];
const NO_HINTS: Hints = [(0, 0); mempolicy::watchdog::STALE_REFRESH_MAX];

/// D4059 / D4812: the `STALE_REFRESH_MAX` live statements with the largest rank once their ranges are read again, of
/// those but the pick whose rank could then pass the pick's (`pick`, else one under `min`;
/// `mempolicy::watchdog::stale_may_outrank`), for the residency thread to read next (`post_hints`, once the locks are
/// free), never read here; none while a cancel is pending (no cancel goes then). No allocation, no wait, no residency
/// read.
#[cfg(any(feature = "std", test))]
fn stale_of(r: &[alloc::sync::Weak<StmtMem>], pins: &[u64; PIN_SLOTS], pick: Option<usize>, min: usize, now: u64) -> Hints {
    let top = pick.and_then(|i| r.get(i)?.upgrade()).map_or(min.max(1) - 1, |l| rank_of(&l, pins));
    let mut best = NO_HINTS;
    // the pick itself is not stale against itself: read again, it only ranks higher
    let others = r.iter().enumerate().filter(|&(k, _)| Some(k) != pick).filter_map(|(_, w)| w.upgrade());
    for l in others.filter(|l| l.on.load(Relaxed)) {
        if l.cancel_pct() != 0 {
            if now.saturating_sub(l.canceled_ms.load(Relaxed).max(1)) < mempolicy::watchdog::CANCEL_ACK_MS {
                return NO_HINTS;
            }
            continue;
        }
        let (stale, unstale) = ranks_of(&l, pins);
        if !mempolicy::watchdog::stale_may_outrank(stale as u64, unstale as u64, top as u64) {
            continue;
        }
        let Some((a, ..)) = ledger_row(&l) else { continue };
        if let Some(k) = best.iter().position(|&(u, _)| unstale > u) {
            best.copy_within(k..mempolicy::watchdog::STALE_REFRESH_MAX - 1, k + 1);
            best[k] = (unstale, a);
        }
    }
    best
}

/// D4812: `hints` handed to the residency thread (`memledger::resident::hint`: stores and a wake). The statements handed.
#[cfg(any(feature = "std", test))]
fn post_hints(hints: &Hints) -> u64 {
    let mut n = 0;
    for &(_, a) in hints.iter().take_while(|&&(u, _)| u != 0) {
        #[cfg(feature = "std")]
        memledger::resident::hint(memledger::Acct(a));
        #[cfg(not(feature = "std"))]
        let _ = a;
        n += 1;
    }
    n
}

/// D4092: one backend a watchdog tier crossing dumps (`try_top_holders`): its governor slot (its ProcNumber), pid and rank.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TopHolder {
    pub slot: usize,
    pub pid: i32,
    pub bytes: usize,
}

/// D4092: the backends of the running statements the watchdog ranks highest (`rank_of`, its cancel's own rank), largest
/// first (equal ranks in registry order), one per backend (its largest statement), at most `k` and `out.len()`, into `out`;
/// how many. A pure read for the dump at a tier crossing: try-lock only (`Err(Busy)` when the registry or the pins table is
/// locked: no dumps at this crossing), no allocation, no cancel or other state touched.
pub fn try_top_holders(k: usize, out: &mut [TopHolder]) -> Result<usize, Busy> {
    #[cfg(any(feature = "std", test))]
    {
        let k = k.min(out.len());
        if k == 0 {
            return Ok(0);
        }
        let mut pins = locked(&PINS, false)?;
        part_pins(&mut pins);
        let r = locked(&REGISTRY, false)?;
        let mut n = 0;
        for l in r.iter().filter_map(alloc::sync::Weak::upgrade).filter(|l| l.on.load(Relaxed)) {
            let Some((slot, pid)) = l.slot.load(Relaxed).checked_sub(1).and_then(|i| SLOT_TABLE.get(i).map(|s| (i, s.pid.load(Relaxed)))) else { continue };
            if pid == 0 {
                continue;
            }
            let h = TopHolder { slot, pid, bytes: rank_of(&l, &pins) };
            if let Some(j) = out[..n].iter().position(|e| e.slot == slot) {
                if out[j].bytes >= h.bytes {
                    continue;
                }
                out.copy_within(j + 1..n, j);
                n -= 1;
            }
            let at = out[..n].iter().position(|e| h.bytes > e.bytes).unwrap_or(n);
            if at >= k {
                continue;
            }
            n = (n + 1).min(k);
            out.copy_within(at..n - 1, at + 1);
            out[at] = h;
        }
        Ok(n)
    }
    #[cfg(not(any(feature = "std", test)))]
    {
        let _ = (k, out);
        Ok(0)
    }
}

/// D4029: with no victim, whether a cancel is inside its ack window, the oldest unanswered past it, the largest other rank.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoVictimView {
    pub pending_ack: bool,
    pub stalled: Option<Stalled>,
    pub largest_other: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stalled {
    pub backend: Option<(usize, i32)>,
    pub word: usize,
    pub age_ms: u64,
    pub ranked: usize,
    pub now: usize,
}

pub fn no_victim_view() -> NoVictimView {
    view_of(true).unwrap_or_default()
}

/// D4029 (d): the backstop's `no_victim_view`, `Err(Busy)` as `try_cancel_largest_victim`'s.
pub fn try_no_victim_view() -> Result<NoVictimView, Busy> {
    view_of(false)
}

/// Kill safety (R5b): the cancels inside their ack window (`mempolicy::watchdog::CANCEL_ACK_MS`, as `NoVictimView`'s
/// `pending_ack`), split by whose: `own` = the one cancel named by its victim's governor slot and cancel word (R6's
/// `Victim::backend` / `Victim::word`) is still unanswered (its statement has not ended: the word is unchanged), `other` =
/// any other is (the watchdog's, the projection's, a second road's), `other_young` = one of those was raised under
/// `mempolicy::ladder::R6_ACK_BOUND_MS` ago (past the cancel line R6 waits on an unanswered cancel that long at most).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PendingAcks {
    pub own: bool,
    pub other: bool,
    pub other_young: bool,
}

/// Kill safety (R5b): `PendingAcks` for the cancel `own` = (its victim's slot, None for a ledger no backend registered;
/// its cancel word), None = no own cancel (every pending one is `other`). `Err(Busy)` as `try_no_victim_view`'s; no
/// allocation.
pub fn try_pending_acks(own: Option<(Option<usize>, usize)>) -> Result<PendingAcks, Busy> {
    #[cfg(any(feature = "std", test))]
    {
        let r = locked(&REGISTRY, false)?;
        let now = watchdog_ms();
        let mut a = PendingAcks::default();
        for l in r.iter().filter_map(alloc::sync::Weak::upgrade).filter(|l| l.on.load(Relaxed)) {
            let word = l.cancel.load(core::sync::atomic::Ordering::Acquire);
            if word & CANCEL_PCT == 0 || now.saturating_sub(l.canceled_ms.load(Relaxed)) >= mempolicy::watchdog::CANCEL_ACK_MS {
                continue;
            }
            let slot = l.slot.load(Relaxed).checked_sub(1);
            if own == Some((slot, word)) {
                a.own = true;
            } else {
                a.other = true;
                a.other_young |= now.saturating_sub(l.canceled_ms.load(Relaxed)) < mempolicy::ladder::R6_ACK_BOUND_MS;
            }
        }
        Ok(a)
    }
    #[cfg(not(any(feature = "std", test)))]
    {
        let _ = own;
        Ok(PendingAcks::default())
    }
}

fn view_of(wait: bool) -> Result<NoVictimView, Busy> {
    #[cfg(any(feature = "std", test))]
    {
        let mut pins = locked(&PINS, wait)?;
        part_pins(&mut pins);
        let r = locked(&REGISTRY, wait)?;
        let now = watchdog_ms();
        let mut v = NoVictimView::default();
        for l in r.iter().filter_map(alloc::sync::Weak::upgrade).filter(|l| l.on.load(Relaxed)) {
            let word = l.cancel.load(core::sync::atomic::Ordering::Acquire);
            if word & CANCEL_PCT == 0 {
                v.largest_other = v.largest_other.max(rank_of(&l, &pins));
                continue;
            }
            let age_ms = now.saturating_sub(l.canceled_ms.load(Relaxed));
            if age_ms < mempolicy::watchdog::CANCEL_ACK_MS {
                v.pending_ack = true;
            } else if v.stalled.is_none_or(|s| age_ms > s.age_ms) {
                let backend = l.slot.load(Relaxed).checked_sub(1).and_then(|i| SLOT_TABLE.get(i).map(|s| (i, s.pid.load(Relaxed))));
                v.stalled = Some(Stalled { backend, word, age_ms, ranked: l.ranked.load(Relaxed), now: rank_of(&l, &pins) });
            }
        }
        Ok(v)
    }
    #[cfg(not(any(feature = "std", test)))]
    {
        let _ = wait;
        Ok(NoVictimView::default())
    }
}

/// D3992: the ledger's account, the statement's ledger bytes, the account's not-resident bytes (D4059: as of the residency
/// pass's last read of its ranges) and (D4035) the signed statement net for `l`'s running statement; `None` with no
/// backend, statement or ledger. Its resident bytes are `resident_of(bytes, not resident)`.
#[cfg(any(feature = "std", test))]
fn ledger_row(l: &StmtMem) -> Option<(u32, u64, i64, i64)> {
    #[cfg(feature = "std")]
    {
        let procno = u32::try_from(l.slot.load(Relaxed).checked_sub(1)?).ok()?;
        let a = memledger::Acct::backend(procno);
        let r = memledger::account_row(a);
        // below its start by more than the fold's lag
        let net = r.statement_net.saturating_add(r.fold_slack as i64);
        r.running.then(|| (a.0, r.statement_bytes, r.not_resident_bytes, net))
    }
    #[cfg(not(feature = "std"))]
    {
        let _ = l;
        None
    }
}

/// D3999: the in-use part bytes each memory ledger account pins alone (installed by Gravity's hosts).
static PART_PINS: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_part_pins_probe(f: fn(&mut dyn FnMut(u32, u64))) {
    PART_PINS.store(f as *mut (), Relaxed);
}

/// D3999: the in-use part bytes each account pins alone, by account slot (`memledger::ACCOUNT_SLOTS`), refilled in place
/// at each pick: the backstop's ranking allocates nothing (D4029 (d)).
#[cfg(any(feature = "std", test))]
const PIN_SLOTS: usize = 4112;
#[cfg(feature = "std")]
const _: () = assert!(PIN_SLOTS == memledger::ACCOUNT_SLOTS);
#[cfg(any(feature = "std", test))]
static PINS: std::sync::Mutex<[u64; PIN_SLOTS]> = std::sync::Mutex::new([0; PIN_SLOTS]);

#[cfg(any(feature = "std", test))]
fn part_pins(pins: &mut [u64; PIN_SLOTS]) {
    pins.fill(0);
    let p = PART_PINS.load(Relaxed);
    if !p.is_null() {
        // SAFETY: only set_part_pins_probe stores here, always from fn(&mut dyn FnMut(u32, u64)).
        let f: fn(&mut dyn FnMut(u32, u64)) = unsafe { core::mem::transmute(p) };
        f(&mut |a, b| {
            if let Some(e) = pins.get_mut(a as usize) {
                *e = e.saturating_add(b);
            }
        });
    }
}

// ---- C3: decide (Grow / Start / Tick) under pgrust.memory_cancel_rule = projection (docs/memory-watchdog.md) --------

use core::sync::atomic::{AtomicBool, AtomicU64};
use mempolicy::decide::{self, Event, Inputs, Rise99};

static DEC_L: AtomicU64 = AtomicU64::new(0);
static DEC_B: AtomicU64 = AtomicU64::new(0);
static DEC_U: AtomicU64 = AtomicU64::new(0);
static DEC_FAST: AtomicU64 = AtomicU64::new(0);
static DEC_RISE_FAST: AtomicU64 = AtomicU64::new(0);
static DEC_RISE_SLOW: AtomicU64 = AtomicU64::new(0);
const RISE_PRIOR: u64 = 1 << 63;
static DEC_RUNNING: AtomicU64 = AtomicU64::new(0);
/// D3934: decide's R, the backstop's governed resident at its last sample and the context footprint then
/// (`publish_decide_resident`). The governor's own sample (`projected`) is 0 while pgrust.memory_governor_threshold is 0.
static DEC_R: AtomicUsize = AtomicUsize::new(0);
static DEC_R_FOOTPRINT: AtomicUsize = AtomicUsize::new(0);
static REVOKE_ON: AtomicBool = AtomicBool::new(false);
/// D3910 (a): grant holders whose portal is suspended (parked cursors): in the shares, never a victim.
static IDLE_HOLDERS: AtomicU64 = AtomicU64::new(0);
static THROTTLE: AtomicBool = AtomicBool::new(false);
/// D3941: R at the backstop's last tick, and whether it fell fast enough since the tick before (`decide::falling`).
static TICK_R: AtomicU64 = AtomicU64::new(0);
static FALLING: AtomicBool = AtomicBool::new(false);
/// An inline decide asked for the fast rungs (it never runs them: their locks are the backstop's).
static RECLAIM_ASK: AtomicBool = AtomicBool::new(false);
/// The spill-self epoch (`StmtMem::spill_epoch`); the backstop moves it when F is back at F*, lifting every one.
static SPILL_EPOCH: AtomicU64 = AtomicU64::new(1);
static SPILL_SET: AtomicBool = AtomicBool::new(false);
static DECIDING: AtomicBool = AtomicBool::new(false);
/// Bytes asked and not landed yet: (bytes, landing time `decide_ms`), written under `Deciding`.
const ASKED_SLOTS: usize = 16;
static ASKED: [(AtomicU64, AtomicU64); ASKED_SLOTS] = [const { (AtomicU64::new(0), AtomicU64::new(0)) }; ASKED_SLOTS];
static RAISE: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

static INLINE_DECIDES: AtomicU64 = AtomicU64::new(0);
static INLINE_CANCELS: AtomicU64 = AtomicU64::new(0);
static SELF_CANCELS: AtomicU64 = AtomicU64::new(0);
static BACKSTOP_CANCELS: AtomicU64 = AtomicU64::new(0);
static SPILL_SELFS: AtomicU64 = AtomicU64::new(0);
static THROTTLE_WAITS: AtomicU64 = AtomicU64::new(0);
static THROTTLE_HIST: [AtomicU64; 16] = [const { AtomicU64::new(0) }; 16];
static THROTTLE_MAX_MS: AtomicU64 = AtomicU64::new(0);

/// Milliseconds on decide's clock, from 1 (0 without std).
pub fn decide_ms() -> u64 {
    #[cfg(any(feature = "std", test))]
    {
        static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        EPOCH.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 + 1
    }
    #[cfg(not(any(feature = "std", test)))]
    0
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DecidePublish {
    pub l: u64,
    pub b: u64,
    pub u: u64,
    pub fast: u64,
    pub rise_fast: Rise99,
    pub rise_slow: Rise99,
    pub revoke_on: bool,
}

fn pack(r: Rise99) -> u64 {
    r.bytes.min(RISE_PRIOR - 1) | if r.prior { RISE_PRIOR } else { 0 }
}

fn unpack(v: u64) -> Rise99 {
    Rise99 { bytes: v & !RISE_PRIOR, prior: v & RISE_PRIOR != 0 }
}

/// D3934: decide's R (memruntime, every sample under projection, whatever the governor's threshold).
pub fn publish_decide_resident(resident: usize, footprint: usize) {
    DEC_R.store(resident, Relaxed);
    DEC_R_FOOTPRINT.store(footprint, Relaxed);
}

/// decide's R now: the last sample's resident bytes plus the context growth since (as `projected`).
pub fn decide_resident() -> u64 {
    let now = crate::global_footprint::bytes();
    DEC_R.load(Relaxed).saturating_add(now).saturating_sub(DEC_R_FOOTPRINT.load(Relaxed)) as u64
}

/// The backstop's sample for decide (memruntime, every sample under projection); counts the running statements.
pub fn publish_decide(p: &DecidePublish) {
    DEC_L.store(p.l, Relaxed);
    DEC_B.store(p.b, Relaxed);
    DEC_U.store(p.u, Relaxed);
    DEC_FAST.store(p.fast, Relaxed);
    DEC_RISE_FAST.store(pack(p.rise_fast), Relaxed);
    DEC_RISE_SLOW.store(pack(p.rise_slow), Relaxed);
    REVOKE_ON.store(p.revoke_on, Relaxed);
    #[cfg(any(feature = "std", test))]
    if let Ok(r) = REGISTRY.lock() {
        let n = r.iter().filter(|w| w.upgrade().is_some_and(|l| l.on.load(Relaxed))).count();
        DEC_RUNNING.store(n as u64, Relaxed);
    }
}

/// D3910 (a): a grant holder's portal suspended (`true`) or resumed / ended while suspended (`false`).
pub fn idle_holder(suspended: bool) {
    if suspended {
        IDLE_HOLDERS.fetch_add(1, Relaxed);
    } else {
        let _ = IDLE_HOLDERS.fetch_update(Relaxed, Relaxed, |n| Some(n.saturating_sub(1)));
    }
}

pub fn idle_holders() -> u64 {
    IDLE_HOLDERS.load(Relaxed)
}

/// Tests (D4812): `f` runs with the registry and / or the pins table held, as another thread's pick holds them.
#[cfg(test)]
pub(crate) fn with_held(registry: bool, pins: bool, f: impl FnOnce()) {
    let _r = registry.then(|| REGISTRY.lock().unwrap_or_else(|e| e.into_inner()));
    let _p = pins.then(|| PINS.lock().unwrap_or_else(|e| e.into_inner()));
    f();
}

/// Tests: another test's cancels land for `cancel_landing_ms` in the same process.
#[cfg(test)]
pub(crate) fn clear_pending() {
    for (b, at) in ASKED.iter() {
        b.store(0, Relaxed);
        at.store(0, Relaxed);
    }
}

fn pending_bytes(now: u64) -> u64 {
    ASKED.iter().filter(|(_, at)| at.load(Relaxed) > now).map(|(b, _)| b.load(Relaxed)).sum()
}

/// `bytes` asked now, landing `landing_ms` from `now` (under `Deciding`): into the slot that lands first.
fn pending_add(bytes: u64, landing_ms: u64, now: u64) {
    if bytes == 0 {
        return;
    }
    let k = (0..ASKED_SLOTS).min_by_key(|&k| ASKED[k].1.load(Relaxed)).unwrap_or(0);
    ASKED[k].0.store(bytes, Relaxed);
    ASKED[k].1.store(now.saturating_add(landing_ms), Relaxed);
}

/// decide's snapshot now: the backstop's last sample, R read now (`decide_resident`) and what is pending.
pub fn decide_inputs() -> Inputs {
    let holders = DEC_RUNNING.load(Relaxed).saturating_add(IDLE_HOLDERS.load(Relaxed));
    Inputs {
        l: DEC_L.load(Relaxed),
        b: DEC_B.load(Relaxed),
        r: decide_resident(),
        u: DEC_U.load(Relaxed),
        fast: DEC_FAST.load(Relaxed),
        pending: pending_bytes(decide_ms()),
        rise_fast: unpack(DEC_RISE_FAST.load(Relaxed)),
        rise_slow: unpack(DEC_RISE_SLOW.load(Relaxed)),
        holders: holders.min(u32::MAX as u64) as u32,
        revoke_on: REVOKE_ON.load(Relaxed),
        falling: FALLING.load(Relaxed),
    }
}

/// A victim backend's interrupt raise (procsignal's `RaiseAbortByProcNumber`), installed by memruntime.
pub fn set_cancel_raise(f: fn(usize, i32)) {
    RAISE.store(f as *mut (), Relaxed);
}

fn raise(slot: usize, pid: i32) {
    let p = RAISE.load(Relaxed);
    if !p.is_null() {
        // SAFETY: only set_cancel_raise stores here, always from fn(usize, i32).
        let f: fn(usize, i32) = unsafe { core::mem::transmute(p) };
        f(slot, pid);
    }
}

/// The one cross-statement decide at a time (a revoke, a cancel): `try_lock` only, a loser acts on itself alone.
struct Deciding;

impl Deciding {
    fn try_lock() -> Option<Deciding> {
        DECIDING.compare_exchange(false, true, core::sync::atomic::Ordering::Acquire, Relaxed).ok().map(|_| Deciding)
    }
}

impl Drop for Deciding {
    fn drop(&mut self) {
        DECIDING.store(false, core::sync::atomic::Ordering::Release);
    }
}

fn published(i: &Inputs) -> bool {
    i.l != 0 && i.b != 0
}

/// A statement's credit at its start: due once its bytes reach `decide::tranche` (projection), never (threshold).
fn start_credit(l: &StmtMem) {
    l.due.store(false, Relaxed);
    l.spill_epoch.store(0, Relaxed);
    let i = if decide::projection() { decide_inputs() } else { Inputs::default() };
    if published(&i) {
        l.check_at.store(decide::tranche(i.free(), i.holders).min(isize::MAX as u64) as isize, Relaxed);
        l.started_ms.store(decide_ms(), Relaxed);
    } else {
        l.check_at.store(isize::MAX, Relaxed);
    }
}

fn charged_ledger() -> Option<&'static StmtMem> {
    #[cfg(any(feature = "std", test))]
    {
        let p = LEDGER.try_with(|c| c.get()).unwrap_or(core::ptr::null());
        // SAFETY: as in `current`; the reference does not outlive this thread's charging of it.
        (!p.is_null()).then(|| unsafe { &*p }).filter(|l| l.on.load(Relaxed))
    }
    #[cfg(not(any(feature = "std", test)))]
    None
}

/// decide(Grow) for `l` at a safe point: the next credit, spill-self, the fast rungs asked, and under `Deciding` a
/// revoke and the cancel of the statement furthest past its fair share (itself included). Never blocks or allocates.
#[cold]
#[inline(never)]
fn serve_due(l: &StmtMem) {
    if !l.due.swap(false, Relaxed) {
        return;
    }
    if !decide::projection() {
        l.check_at.store(isize::MAX, Relaxed);
        return;
    }
    let asked = l.bytes().max(0) as u64;
    let held = l.held();
    let mut i = decide_inputs();
    if !published(&i) {
        l.check_at.store(isize::MAX, Relaxed);
        return;
    }
    let p = decide::decide(Event::Grow { held }, &i);
    // the credit counts in the charge's units: `charge` compares the charge with it
    l.check_at.store(asked.saturating_add(p.tranche).min(isize::MAX as u64) as isize, Relaxed);
    INLINE_DECIDES.fetch_add(1, Relaxed);
    if p.spill_self {
        let e = SPILL_EPOCH.load(Relaxed);
        if l.spill_epoch.swap(e, Relaxed) != e {
            SPILL_SELFS.fetch_add(1, Relaxed);
        }
        SPILL_SET.store(true, Relaxed);
    }
    if p.reclaim_fast {
        RECLAIM_ASK.store(true, Relaxed);
    }
    let mut victim = None;
    if p.revoke != 0 || p.cancel == decide::Cancel::Victim {
        if let Some(_one) = Deciding::try_lock() {
            let (_, v) = act(&mut i, &p, l.slot.load(Relaxed).checked_sub(1), false);
            victim = v;
            if let Some(v) = v {
                INLINE_CANCELS.fetch_add(1, Relaxed);
                if v.word == l.cancel.load(Relaxed) && l.cancel_pct() != 0 {
                    SELF_CANCELS.fetch_add(1, Relaxed);
                }
            }
        }
    }
    trace_inline(Event::Grow { held }, &i, &p, l.slot.load(Relaxed).checked_sub(1), victim);
}

/// Under `Deciding`: `p`'s revoke, then the cancel if the projection still crosses with that pending.
fn act(i: &mut Inputs, p: &decide::Plan, requester: Option<usize>, backstop: bool) -> (u64, Option<Victim>) {
    let now = decide_ms();
    let asked = if p.revoke != 0 { request_revoke(p.revoke, i.share(), requester) } else { 0 };
    if asked != 0 {
        pending_add(asked, mempolicy::consts::get(mempolicy::consts::K::RevokeWaitMs), now);
        i.pending = i.pending.saturating_add(asked);
    }
    if p.cancel != decide::Cancel::Victim || !i.crosses() {
        return (asked, None);
    }
    (asked, cancel_fair_victim(i, now, backstop))
}

/// The projection cancel: the statement furthest past `i.share()` (ties to the youngest) fails with 53200 as in
/// `cancel_largest_victim`; its bytes pending for `cancel_landing_ms`. Only `try_lock` off the backstop.
fn cancel_fair_victim(i: &Inputs, now: u64, backstop: bool) -> Option<Victim> {
    #[cfg(any(feature = "std", test))]
    {
        let r = if backstop { REGISTRY.lock().ok()? } else { REGISTRY.try_lock().ok()? };
        let cand = |w: &alloc::sync::Weak<StmtMem>| match w.upgrade() {
            // C8 move 3c (b): RUNNING counts committed bytes (policy 3.2), not the grant's untouched room
            Some(l) if l.on.load(Relaxed) => decide::Cand {
                held: l.committed_held(),
                elapsed_ms: now.saturating_sub(l.started_ms.load(Relaxed)),
                idle: false,
                canceled: l.cancel_pct() != 0,
            },
            _ => decide::Cand { held: 0, elapsed_ms: 0, idle: true, canceled: false },
        };
        let today = decide::victim(r.iter().map(cand), i.share(), i.overshoot());
        // LEDGER2 step 4: the same rule over the owner tree's bytes, noted beside today's pick
        let tree_cand = |w: &alloc::sync::Weak<StmtMem>| {
            let mut c = cand(w);
            if !c.idle {
                c.held = w.upgrade().map_or(0, |l| crate::ledger2::owned(l.slot_index()));
            }
            c
        };
        let tree = decide::victim(r.iter().map(tree_cand), i.share(), i.overshoot());
        let choice = |k: Option<usize>, tree: bool| {
            k.and_then(|k| r.get(k)?.upgrade()).map_or((None, 0), |l| {
                (l.slot_index(), if tree { crate::ledger2::owned(l.slot_index()) } else { l.committed_held() })
            })
        };
        let shadow = (choice(today, false), choice(tree, true));
        // D4812: act first, then log: the shadow line is written once the cancel is raised (or not) and the registry
        // is free again, so a slow log sink never holds the cancel or another thread's pick
        let v = (move || {
            let pick = today?;
            let big = r.get(pick)?.upgrade()?;
            drop(r);
            // Acquire: pairs with `next_generation`'s Release, after the statement's reset bytes
            let word = big.cancel.load(core::sync::atomic::Ordering::Acquire);
            if !big.on.load(Relaxed) || word & CANCEL_PCT != 0 {
                return None;
            }
            let pct = (i.r.saturating_mul(100) / i.l.max(1)).clamp(1, CANCEL_PROJECTED as u64 - 1) as usize;
            let canceled = word | pct | CANCEL_PROJECTED;
            // D3934: on the threshold rung's clock, so its backstop waits CANCEL_ACK_MS on this cancel as on its own
            big.canceled_ms.store(watchdog_ms(), Relaxed);
            big.cancel.compare_exchange(word, canceled, core::sync::atomic::Ordering::AcqRel, Relaxed).ok()?;
            let b = big.committed_held() as usize;
            pending_add(b as u64, mempolicy::consts::get(mempolicy::consts::K::CancelLandingMs), now);
            let backend = big.slot.load(Relaxed).checked_sub(1).and_then(|s| SLOT_TABLE.get(s).map(|t| (s, t.pid.load(Relaxed))));
            if let Some((slot, pid)) = backend {
                condemn(slot, canceled);
                raise(slot, pid);
            }
            // (the projection's cancel ranks by committed bytes, not R6's rank: no candidates kept)
            Some(Victim { bytes: b, charged: big.committed_shared(), word: canceled, backend, top: [Cand::default(); PICK_TOP] })
        })();
        crate::ledger2::note(crate::ledger2::Decision::FairCancel, shadow.0, shadow.1, now);
        v
    }
    #[cfg(not(any(feature = "std", test)))]
    {
        let _ = (i, now, backstop);
        None
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Tick {
    pub inputs: Inputs,
    pub plan: decide::Plan,
    pub reclaim_fast: bool,
    pub revoked: u64,
    pub victim: Option<Victim>,
}

/// The backstop's decide(Tick) every sample (`None` under the threshold rule): the throttle, the spill-self lift, the
/// fast rungs (the caller runs them), and under `Deciding` a revoke, then the projection cancel.
pub fn decide_tick() -> Option<Tick> {
    if !decide::projection() {
        THROTTLE.store(false, Relaxed);
        return None;
    }
    let mut i = decide_inputs();
    if !published(&i) {
        THROTTLE.store(false, Relaxed);
        return None;
    }
    // D3941: a start waits only on relief that is landing, re-decided here every tick: R's fall since the last tick
    let short = i.free() < i.target();
    i.falling = short && decide::falling(TICK_R.swap(i.r, Relaxed), i.r, i.target() - i.free());
    FALLING.store(i.falling, Relaxed);
    let plan = decide::decide(Event::Tick, &i);
    THROTTLE.store(decide::decide(Event::Start, &i).throttle, Relaxed);
    // the spill-self lift waits for F >= F*, whether or not a start is held meanwhile
    if !short && SPILL_SET.swap(false, Relaxed) {
        SPILL_EPOCH.fetch_add(1, Relaxed);
    }
    let reclaim_fast = RECLAIM_ASK.swap(false, Relaxed) || plan.reclaim_fast;
    let mut t = Tick { inputs: i, plan, reclaim_fast, ..Tick::default() };
    if plan.revoke != 0 || plan.cancel == decide::Cancel::Victim {
        if let Some(_one) = Deciding::try_lock() {
            let (asked, v) = act(&mut i, &plan, None, true);
            t.revoked = asked;
            t.victim = v;
            if v.is_some() {
                BACKSTOP_CANCELS.fetch_add(1, Relaxed);
            }
        }
    }
    Some(t)
}

/// C3 (decide Start): whether a start waits now (projection only; one relaxed load under the threshold rule).
#[inline]
pub fn throttled() -> bool {
    THROTTLE.load(Relaxed) && decide::projection()
}

pub fn note_throttle_wait(ms: u64) {
    THROTTLE_WAITS.fetch_add(1, Relaxed);
    THROTTLE_HIST[(64 - (ms + 1).leading_zeros() as usize - 1).min(15)].fetch_add(1, Relaxed);
    THROTTLE_MAX_MS.fetch_max(ms, Relaxed);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecideCounts {
    pub inline_decides: u64,
    pub inline_cancels: u64,
    pub self_cancels: u64,
    pub backstop_cancels: u64,
    pub spill_selfs: u64,
    pub throttle_waits: u64,
    pub throttle_hist: [u64; 16],
    pub throttle_max_ms: u64,
}

pub fn decide_counts() -> DecideCounts {
    DecideCounts {
        inline_decides: INLINE_DECIDES.load(Relaxed),
        inline_cancels: INLINE_CANCELS.load(Relaxed),
        self_cancels: SELF_CANCELS.load(Relaxed),
        backstop_cancels: BACKSTOP_CANCELS.load(Relaxed),
        spill_selfs: SPILL_SELFS.load(Relaxed),
        throttle_waits: THROTTLE_WAITS.load(Relaxed),
        throttle_hist: core::array::from_fn(|k| THROTTLE_HIST[k].load(Relaxed)),
        throttle_max_ms: THROTTLE_MAX_MS.load(Relaxed),
    }
}

/// One inline decide for the trace, written by the backstop (`drain_inline_trace`): an inline decide may not allocate.
#[derive(Clone, Copy, Debug)]
pub struct InlineDecide {
    pub at_ms: u64,
    pub event: Event,
    pub inputs: Inputs,
    pub plan: decide::Plan,
    pub slot: Option<usize>,
    pub victim: Option<Victim>,
}

#[cfg(any(feature = "std", test))]
struct InlineRing {
    /// `rec[..n]` are written (`ring_push`), the rest zero bytes: an empty ring is all zero, so `INLINE_TRACE` lands in .bss
    /// (D4514; `Option<InlineDecide>`'s None niche kept its 13.8 KB in .data).
    rec: [core::mem::MaybeUninit<InlineDecide>; 64],
    n: usize,
    dropped: u64,
}

#[cfg(any(feature = "std", test))]
static INLINE_TRACE: std::sync::Mutex<InlineRing> =
    std::sync::Mutex::new(InlineRing { rec: [const { core::mem::MaybeUninit::zeroed() }; 64], n: 0, dropped: 0 });

/// One decide into the ring (dropped, and counted, when the ring is full or a drain holds it).
#[cfg(any(feature = "std", test))]
fn ring_push(d: InlineDecide) {
    if let Ok(mut g) = INLINE_TRACE.try_lock() {
        if g.n < g.rec.len() {
            let n = g.n;
            g.rec[n] = core::mem::MaybeUninit::new(d);
            g.n += 1;
        } else {
            g.dropped += 1;
        }
    }
}

fn trace_inline(event: Event, inputs: &Inputs, plan: &decide::Plan, slot: Option<usize>, victim: Option<Victim>) {
    if !mempolicy::trace::on() {
        return;
    }
    #[cfg(any(feature = "std", test))]
    ring_push(InlineDecide { at_ms: decide_ms(), event, inputs: *inputs, plan: *plan, slot, victim });
    #[cfg(not(any(feature = "std", test)))]
    let _ = (event, inputs, plan, slot, victim);
}

/// The inline decides since the last drain, oldest first; returns how many the full ring dropped.
pub fn drain_inline_trace(mut f: impl FnMut(&InlineDecide)) -> u64 {
    #[cfg(any(feature = "std", test))]
    {
        let (rec, n, dropped) = match INLINE_TRACE.lock() {
            Ok(mut g) => {
                let out = (g.rec, g.n, g.dropped);
                g.n = 0;
                g.dropped = 0;
                out
            }
            Err(_) => return 0,
        };
        // SAFETY: `ring_push` wrote `rec[..n]` before it counted them in `n`.
        rec[..n].iter().map(|r| unsafe { r.assume_init_ref() }).for_each(&mut f);
        dropped
    }
    #[cfg(not(any(feature = "std", test)))]
    {
        let _ = &mut f;
        0
    }
}

#[cfg(test)]
mod inline_trace_tests {
    use super::*;

    /// D4514: the inline trace ring lies in .bss (an anonymous writable mapping: no file bytes). Born red on trunk: the
    /// None niche of its 64 `Option<InlineDecide>` records kept it in .data.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_inline_trace_ring_is_in_bss() {
        let last = core::ptr::addr_of!(INLINE_TRACE) as usize + core::mem::size_of_val(&INLINE_TRACE) - 1;
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        let line = maps
            .lines()
            .find(|l| {
                let (a, z) = l.split(' ').next().unwrap().split_once('-').unwrap();
                (usize::from_str_radix(a, 16).unwrap()..usize::from_str_radix(z, 16).unwrap()).contains(&last)
            })
            .unwrap_or_else(|| panic!("no mapping holds {last:#x}"));
        let f: std::vec::Vec<&str> = line.split_whitespace().collect();
        assert!(f[1].starts_with("rw") && (f.len() == 5 || f[5] == "[heap]"), "the inline trace ring is mapped from the binary (.data), not .bss: {line}");
    }

    /// D4514: the ring hands back exactly what was pushed, oldest first, counts what a full ring dropped, and is empty
    /// after a drain.
    #[test]
    fn the_inline_trace_ring_drains_what_was_pushed() {
        let d = |at_ms| InlineDecide { at_ms, event: Event::Tick, inputs: Inputs::default(), plan: decide::Plan::default(), slot: Some(3), victim: None };
        let _ = drain_inline_trace(|_| {});
        for at in 1..=66 {
            ring_push(d(at));
        }
        let mut seen = std::vec::Vec::new();
        let dropped = drain_inline_trace(|r| seen.push((r.at_ms, r.slot)));
        assert_eq!(seen, (1..=64).map(|at| (at, Some(3))).collect::<std::vec::Vec<_>>());
        assert_eq!(dropped, 2);
        assert_eq!(drain_inline_trace(|_| panic!("drained twice")), 0);
    }
}
