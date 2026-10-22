//! Cache images of 64-512 KiB on pages of their own (D4321 step 1, LADDER-DESIGN 6a).
//!
//! mimalloc v3 packs blocks of 84-512 KiB up to 8 per 4 MiB "large" page, so an evicted part-cache image beside a live
//! one leaves a hole no purge returns (memlead's sizing, box 19: 779 of the +815 MiB). Its singleton pages are no road:
//! past 512 KiB a singleton is committed whole (up to 9x a 64 KiB image), and an alignment past 64 KiB leaves the
//! arenas for an uncounted OS mapping.
//!
//! So these images get runs of 64 KiB slices in one range of pgrust's own: MAP_NORESERVE address space (named like
//! mimalloc's, so the allocator's resident read covers it), MADV_NOHUGEPAGE. A run of k slices (1..=8) comes from that
//! length's free-run stack, else from the bump pointer. A free gives the run's pages back at once (MADV_DONTNEED: a
//! run is at most 512 KiB, so it never covers a whole 2 MiB-aligned range, the vendored CVE-2026-74674 rule) and pushes
//! it. A run's pages read as zero when taken again, so a zeroed request costs nothing. `CACHE_TAKEN` counts each run's
//! bytes while it is taken.
//!
//! The global allocator recognizes a run with one range compare (`init_small::oom::set_own_pages`) and routes its free
//! and realloc here before anything else. The stacks sit under a spin flag per run length: cache fills are rare next to
//! the global allocator's traffic, and nothing here allocates, sleeps or calls into mimalloc.

#![cfg_attr(not(all(target_os = "linux", not(target_family = "wasm"))), allow(dead_code))]

use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering::{Acquire, Relaxed, Release}};

use crate::SLICE;

/// The smallest image served here, and the largest (past it mimalloc's own singleton pages return whole).
pub const OWN_MIN: usize = 64 << 10;
pub const OWN_MAX: usize = 512 << 10;
/// Run lengths in slices: 1..=MAX_RUN.
const MAX_RUN: usize = OWN_MAX / SLICE;

static BASE: AtomicUsize = AtomicUsize::new(0);
static SPAN: AtomicUsize = AtomicUsize::new(0);
/// The next never-used slice.
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// One run length's free runs: a stack of slice indices (u32) in its own NORESERVE mapping, under a spin flag.
struct Stack {
    lock: AtomicBool,
    items: AtomicUsize,
    len: AtomicUsize,
    cap: AtomicUsize,
}

static STACKS: [Stack; MAX_RUN] =
    [const { Stack { lock: AtomicBool::new(false), items: AtomicUsize::new(0), len: AtomicUsize::new(0), cap: AtomicUsize::new(0) } }; MAX_RUN];
/// Freed runs kept COMMITTED (their pages still present) for the next image of their length: a scan's transient images
/// (SegBuf's rebuilds) would otherwise fault their pages back in at every reuse (D3892 guard: cold page faults doubled).
/// At most `hot_cap()` bytes (max(64 MiB, 2 % of B)); past it, and at every give-back (`flush`), a freed run's pages go back.
static HOT: [Stack; MAX_RUN] =
    [const { Stack { lock: AtomicBool::new(false), items: AtomicUsize::new(0), len: AtomicUsize::new(0), cap: AtomicUsize::new(0) } }; MAX_RUN];
static HOT_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Runs taken now and in all (information, tests).
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// D4692 (ladder L4, `cache_release_bg`): freed runs past the hot pool that a thread not marked `mark_inline_release_ok`
/// left for the page releaser: still committed (`CACHE_TAKEN` counts them) until `drain_landing` gives them back on the
/// releaser's thread, and the next take of their length reuses them first. `LANDING_BYTES` is the ladder's landing credit
/// (`landing_bytes`), counted before each push, so a pass that ends with it above 0 has a run still to find.
static LANDING: [Stack; MAX_RUN] =
    [const { Stack { lock: AtomicBool::new(false), items: AtomicUsize::new(0), len: AtomicUsize::new(0), cap: AtomicUsize::new(0) } }; MAX_RUN];
static LANDING_BYTES: AtomicUsize = AtomicUsize::new(0);
/// D4692: the releaser's wake (memruntime's `pg:memrelease`), called when the landing count leaves 0; null (no releaser,
/// the knob off): every thread gives its runs back inline, as before.
static RELEASE_WAKE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
/// H3 step (1) (`give_back_service`): the give-back service's admission at the enqueue (`leave`). Its backlog cap in hot
/// pools (`hot_cap`); 0 = no service: cache_release_bg's releaser takes every run, as before.
static SERVICE_HOT_CAPS: AtomicUsize = AtomicUsize::new(0);
/// H3 step (1): the service's liveness check (memruntime's `service::stalled`: no pass for the dead-man stretch). Called
/// inside a free, only while runs wait; null: never stalled.
static SERVICE_STALLED: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
/// H3 step (1): the runs the service's admission sent back to the freeing thread, and their bytes: at the backlog cap
/// (runs, bytes), then past the dead-man stretch (runs, bytes).
static FALLBACKS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
/// H3 step (1): drains of the landing stack on a thread not marked inline-ok (make_room past the governor's bar: the
/// drawing thread gives the runs back itself), and their bytes.
static DRAWER: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

std::thread_local! {
    /// D4692: this thread gives runs back itself (the releaser, the memory runtime's idle loop). Plain data: a free reads it.
    static INLINE_OK: Cell<bool> = const { Cell::new(false) };
    /// D4692: the runs this thread gave back (`give`), the witness's count.
    static GIVES: Cell<u64> = const { Cell::new(0) };
}

/// The range's bytes: physical memory, at least 16 GiB, at most 1 TiB (address space only; a run is resident only while
/// an image fills it).
#[cfg(all(target_os = "linux", not(target_family = "wasm")))]
fn range_bytes() -> usize {
    // SAFETY: plain sysconf reads.
    let phys = unsafe { (libc::sysconf(libc::_SC_PHYS_PAGES).max(0) as usize).saturating_mul(libc::sysconf(libc::_SC_PAGESIZE).max(0) as usize) };
    phys.clamp(16 << 30, 1 << 40).next_multiple_of(SLICE)
}

/// Map the range and its stacks (once; from `install`). Whether it is in place.
#[cfg(all(target_os = "linux", not(target_family = "wasm")))]
pub(crate) fn install() -> bool {
    if BASE.load(Relaxed) != 0 {
        return true;
    }
    let bytes = range_bytes();
    let Some(raw) = map(bytes + SLICE) else { return false };
    let start = raw.next_multiple_of(SLICE);
    // SAFETY: advice on our own fresh mapping.
    unsafe { libc::madvise(start as *mut libc::c_void, bytes, libc::MADV_NOHUGEPAGE) };
    crate::name(start, bytes);
    let slices = bytes / SLICE;
    for (i, (s, h)) in STACKS.iter().zip(HOT.iter()).enumerate() {
        let cap = slices / (i + 1);
        let Some(items) = map(cap * 4) else { return false };
        s.items.store(items, Relaxed);
        s.cap.store(cap, Relaxed);
        // D4692: as long as the free stack (never full)
        let Some(items) = map(cap * 4) else { return false };
        LANDING[i].items.store(items, Relaxed);
        LANDING[i].cap.store(cap, Relaxed);
        // room for the largest cap B can ask (B <= the range's bytes, which is physical memory or more)
        let hcap = HOT_CAP.max(bytes / 100 * HOT_B_PCT) / ((i + 1) * SLICE) + 1;
        let Some(items) = map(hcap * 4) else { return false };
        h.items.store(items, Relaxed);
        h.cap.store(hcap, Relaxed);
    }
    BASE.store(start, Relaxed);
    SPAN.store(bytes, Release);
    true
}

#[cfg(all(target_os = "linux", not(target_family = "wasm")))]
fn map(bytes: usize) -> Option<usize> {
    // SAFETY: a fresh private anonymous mapping; nothing else names it.
    let p = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            bytes,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    (p != libc::MAP_FAILED).then_some(p as usize)
}

/// The range (base, bytes); (0, 0) when none.
pub fn range() -> (usize, usize) {
    (BASE.load(Relaxed), SPAN.load(Acquire))
}

/// Whether `p` lies in the range.
#[inline]
pub fn owns(p: *const u8) -> bool {
    (p as usize).wrapping_sub(BASE.load(Relaxed)) < SPAN.load(Relaxed)
}

/// The run length (slices) a request takes here, or None (served elsewhere).
#[inline]
pub fn run_of(size: usize, align: usize) -> Option<usize> {
    (size >= OWN_MIN && size <= OWN_MAX && align <= SLICE).then(|| size.div_ceil(SLICE))
}

fn locked<R>(s: &Stack, f: impl FnOnce(&Stack) -> R) -> R {
    while s.lock.compare_exchange_weak(false, true, Acquire, Relaxed).is_err() {
        core::hint::spin_loop();
    }
    let r = f(s);
    s.lock.store(false, Release);
    r
}

fn pop(s: &Stack) -> Option<usize> {
    locked(s, |s| {
        let n = s.len.load(Relaxed);
        if n == 0 {
            return None;
        }
        s.len.store(n - 1, Relaxed);
        // SAFETY: index n-1 < cap of this stack's own mapping.
        Some(unsafe { *(s.items.load(Relaxed) as *const u32).add(n - 1) } as usize)
    })
}

/// Push slice index `i`; false when the stack is full (only the hot stacks can be).
fn push(s: &Stack, i: usize) -> bool {
    locked(s, |s| {
        let n = s.len.load(Relaxed);
        if n >= s.cap.load(Relaxed) {
            return false;
        }
        // SAFETY: index n < cap of this stack's own mapping.
        unsafe { *(s.items.load(Relaxed) as *mut u32).add(n) = i as u32 };
        s.len.store(n + 1, Relaxed);
        true
    })
}

/// The hot pool's floor cap. CHOSEN (main, D4346): the pool keeps at most max(64 MiB, 2 % of B) (`hot_cap`), the order of
/// a wide scan's transient images in flight (8 workers); every give-back (`flush`) empties it.
pub const HOT_CAP: usize = 64 << 20;
/// B's share the pool may keep, in hundredths.
const HOT_B_PCT: usize = 2;

/// The hot pool's cap now: max(HOT_CAP, 2 % of B, the last snapshot's: `memledger::b_at`, 0 before one). One relaxed load.
#[inline]
fn hot_cap() -> usize {
    HOT_CAP.max(memledger::b_at() as usize / 100 * HOT_B_PCT)
}

/// A run of `k` slices, or null when the range is full or absent. `zeroed`: its bytes read zero (a run from the hot
/// pool is cleared; a fresh or given-back one already reads zero).
pub fn alloc(k: usize, size: usize, zeroed: bool) -> *mut u8 {
    let base = BASE.load(Relaxed);
    if base == 0 || k == 0 || k > MAX_RUN {
        return core::ptr::null_mut();
    }
    // D4692: a run left for the releaser is reused before a fresh one (its pages are still present); the count goes up
    // before a push, so at 0 the stack is empty and its lock is not taken (the knob off: always)
    let landed = || (LANDING_BYTES.load(Relaxed) != 0).then(|| pop(&LANDING[k - 1])).flatten();
    let warm = pop(&HOT[k - 1]).map(|i| (i, false)).or_else(|| landed().map(|i| (i, true)));
    if let Some((i, landed)) = warm {
        if landed {
            // off the REAPING row; the allocator posts the claim on the claimer's account after (once)
            LANDING_BYTES.fetch_sub(k * SLICE, Relaxed);
            reaping(-((k * SLICE) as i64));
        } else {
            HOT_BYTES.fetch_sub(k * SLICE, Relaxed);
        }
        LIVE.fetch_add(1, Relaxed);
        let p = (base + i * SLICE) as *mut u8;
        if zeroed {
            // SAFETY: the run is ours now, size <= k * SLICE.
            unsafe { core::ptr::write_bytes(p, 0, size) };
        }
        return p;
    }
    let reused = pop(&STACKS[k - 1]);
    let slice = match reused {
        Some(i) => i,
        None => {
            let slices = SPAN.load(Relaxed) / SLICE;
            let i = NEXT.fetch_add(k, Relaxed);
            if i + k > slices {
                NEXT.fetch_sub(k, Relaxed);
                return core::ptr::null_mut();
            }
            i
        }
    };
    LIVE.fetch_add(1, Relaxed);
    memledger::arena::note_taken(true, (k * SLICE) as i64);
    (base + slice * SLICE) as *mut u8
}

/// Free a run of `k` slices: kept committed in the hot pool while it has room, else its pages go back at once.
pub fn free(p: *mut u8, k: usize) {
    let base = BASE.load(Relaxed);
    debug_assert!(owns(p) && (p as usize - base).is_multiple_of(SLICE) && (1..=MAX_RUN).contains(&k));
    let i = (p as usize - base) / SLICE;
    if HOT_BYTES.fetch_add(k * SLICE, Relaxed) + k * SLICE <= hot_cap() && push(&HOT[k - 1], i) {
        LIVE.fetch_sub(1, Relaxed);
        return;
    }
    HOT_BYTES.fetch_sub(k * SLICE, Relaxed);
    LIVE.fetch_sub(1, Relaxed);
    if defers() && leave(i, k) {
        return;
    }
    give(base, i, k);
}

/// D4692: run `i` of `k` slices onto the landing stack for the releaser: counted first, its bytes on the REAPING row, the
/// releaser woken when the count leaves 0. False when the stack is full (it never is: as long as the free stack), the
/// caller then gives the run back itself. H3 step (1): false too when the give-back service turns it away (`admission`),
/// counted.
fn leave(i: usize, k: usize) -> bool {
    if let Some(why) = admission(k * SLICE) {
        FALLBACKS[why].fetch_add(1, Relaxed);
        FALLBACKS[why + 1].fetch_add((k * SLICE) as u64, Relaxed);
        return false;
    }
    let before = LANDING_BYTES.fetch_add(k * SLICE, Relaxed);
    if !push(&LANDING[k - 1], i) {
        LANDING_BYTES.fetch_sub(k * SLICE, Relaxed);
        return false;
    }
    reaping((k * SLICE) as i64);
    if before == 0 {
        wake_releaser();
    }
    true
}

/// D4692: whether a free on this thread leaves its run for the releaser: one is registered and this thread is not marked.
#[inline]
fn defers() -> bool {
    !RELEASE_WAKE.load(Relaxed).is_null() && !INLINE_OK.try_with(Cell::get).unwrap_or(true)
}

/// `FALLBACKS`' pairs: a run turned away at the backlog cap, and with the service stalled.
const AT_CAP: usize = 0;
const STALLED: usize = 2;

/// H3 step (1) (`give_back_service`): whether the service turns a run of `bytes` away, and why (`FALLBACKS`' pair), so
/// the freeing thread gives it back itself. Two cases. First, the backlog cap: the run would take the landing past
/// `SERVICE_HOT_CAPS` hot pools. Second, the dead-man rule (C3's): runs wait and the service made no pass for its
/// dead-man stretch. None when no service is set (cache_release_bg's releaser takes every run, as before) or the run is
/// admitted. A few loads; the liveness check (a clock read) runs only while runs wait.
#[inline]
fn admission(bytes: usize) -> Option<usize> {
    let caps = SERVICE_HOT_CAPS.load(Relaxed);
    if caps == 0 {
        return None;
    }
    let waiting = LANDING_BYTES.load(Relaxed);
    if waiting.saturating_add(bytes) > caps.saturating_mul(hot_cap()) {
        return Some(AT_CAP);
    }
    let p = SERVICE_STALLED.load(Acquire);
    if waiting == 0 || p.is_null() {
        return None;
    }
    // SAFETY: only set_service stores here, always from fn() -> bool.
    let stalled: fn() -> bool = unsafe { core::mem::transmute(p) };
    stalled().then_some(STALLED)
}

/// H3 step (1) (`give_back_service`): the give-back service runs (memruntime's pg:memrelease, before its wake is set):
/// `stalled` is its liveness check (no allocation, no lock: it is called inside a free) and `hot_caps` its backlog cap
/// in hot pools. From now on a run the cap or a stalled service turns away is given back on the freeing thread, counted
/// (`service_fallbacks`).
pub fn set_service(stalled: fn() -> bool, hot_caps: usize) {
    SERVICE_STALLED.store(stalled as *mut (), Release);
    SERVICE_HOT_CAPS.store(hot_caps, Release);
}

/// H3 step (1): the service's backlog cap now (bytes; 0 = no service).
pub fn service_cap() -> usize {
    SERVICE_HOT_CAPS.load(Relaxed).saturating_mul(hot_cap())
}

/// H3 step (4): the give-back service is in place (`set_service` ran: memruntime's pg:memrelease runs as the service). A
/// job lane above (`set_job_lane`) enqueues only then.
pub fn service_on() -> bool {
    SERVICE_HOT_CAPS.load(Relaxed) != 0
}

/// H3 step (4): the service's dead-man check (C3's, `set_service`'s `stalled`) for a job lane's admission while its jobs
/// wait; false with no service.
pub fn service_stalled() -> bool {
    let p = SERVICE_STALLED.load(Acquire);
    if p.is_null() {
        return false;
    }
    // SAFETY: only set_service stores here, always from fn() -> bool.
    let stalled: fn() -> bool = unsafe { core::mem::transmute(p) };
    stalled()
}

/// H3 step (4): the service woken for a job lane's first waiting job (the releaser's wake, `set_release_wake`; nothing
/// without one).
pub fn wake_service() {
    wake_releaser();
}

/// H3 step (4): the job lane a crate above registers (gravity_driver's whole-region give-backs, `mem::giveback`): its bytes
/// waiting, and its give-back of at most `max` bytes (a job never split), which returns the bytes. The service's second
/// lane (memruntime's `service::LANES`): without one both read 0.
static JOB_LANDING: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static JOB_DRAIN: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// H3 step (4): the job lane registered (before its first job is counted); the last registration holds.
pub fn set_job_lane(landing: fn() -> usize, drain_upto: fn(usize) -> usize) {
    JOB_DRAIN.store(drain_upto as *mut (), Release);
    JOB_LANDING.store(landing as *mut (), Release);
}

/// H3 step (4): the job lane's bytes waiting (0 with none registered).
pub fn job_landing_bytes() -> usize {
    let p = JOB_LANDING.load(Acquire);
    if p.is_null() {
        return 0;
    }
    // SAFETY: only set_job_lane stores here, always from fn() -> usize.
    let landing: fn() -> usize = unsafe { core::mem::transmute(p) };
    landing()
}

/// H3 step (4): the job lane's jobs given back on the calling thread up to `max` bytes (the service's batch); the bytes
/// (0 with none registered).
pub fn drain_jobs_upto(max: usize) -> usize {
    let p = JOB_DRAIN.load(Acquire);
    if p.is_null() {
        return 0;
    }
    // SAFETY: only set_job_lane stores here, always from fn(usize) -> usize.
    let drain: fn(usize) -> usize = unsafe { core::mem::transmute(p) };
    drain(max)
}

/// H3 step (1): the runs the service turned away so far, which their freeing threads gave back themselves:
/// `[at the cap: runs, bytes, past the dead-man stretch: runs, bytes]`.
pub fn service_fallbacks() -> [u64; 4] {
    core::array::from_fn(|i| FALLBACKS[i].load(Relaxed))
}

/// H3 step (1): the drains of the landing on a thread not marked inline-ok so far (make_room past the governor's bar),
/// and their bytes.
pub fn drawer_drains() -> (u64, u64) {
    (DRAWER[0].load(Relaxed), DRAWER[1].load(Relaxed))
}

/// D4692 (memory accounting's condition 1): `n` bytes of parked runs onto (+) or off (-) the REAPING row (named: committed,
/// no owner, pending the releaser), never the calling thread's account: door 1 counts them there and the post moves them
/// over, as a door-1 owner's post does. Allocator context: thread-locals and atomics only.
fn reaping(n: i64) {
    memledger::door_heap(n as isize);
    memledger::charge::CtxOwner::named(memledger::Owner::Cache(memledger::Holder::Reaping)).post(n);
}

fn wake_releaser() {
    let p = RELEASE_WAKE.load(Acquire);
    if !p.is_null() {
        // SAFETY: only set_release_wake stores here, always from fn().
        let f: fn() = unsafe { core::mem::transmute(p) };
        f();
    }
}

/// D4692 (`cache_release_bg`): the page releaser is running; `wake` (no allocation, no lock: it is called inside a free)
/// starts its pass. From now on a free past the hot pool on a thread not marked leaves its run for it.
pub fn set_release_wake(wake: fn()) {
    RELEASE_WAKE.store(wake as *mut (), Release);
}

/// D4692: this thread gives runs back itself (the releaser and the memory runtime's idle loop mark themselves).
pub fn mark_inline_release_ok() {
    let _ = INLINE_OK.try_with(|c| c.set(true));
}

/// D4692: the runs left for the releaser given back on the calling thread (make_room past the governor's bar). The bytes
/// returned.
pub fn drain_landing() -> usize {
    drain_landing_upto(usize::MAX)
}

/// D4692: `drain_landing`, stopping once `max` bytes are given back (the releaser's batch): a run is never split, so the
/// last one may take the batch past `max` by less than one run. The bytes returned.
pub fn drain_landing_upto(max: usize) -> usize {
    let base = BASE.load(Relaxed);
    if base == 0 {
        return 0;
    }
    let mut n = 0;
    for (j, l) in LANDING.iter().enumerate() {
        let k = j + 1;
        while n < max {
            let Some(i) = pop(l) else { break };
            give(base, i, k);
            reaping(-((k * SLICE) as i64));
            LANDING_BYTES.fetch_sub(k * SLICE, Relaxed);
            n += k * SLICE;
        }
    }
    // H3 step (1): a drain on a thread that is not the releaser's or the idle loop's is counted (never silent)
    if n != 0 && !INLINE_OK.try_with(Cell::get).unwrap_or(true) {
        DRAWER[0].fetch_add(1, Relaxed);
        DRAWER[1].fetch_add(n as u64, Relaxed);
    }
    n
}

/// D4692: bytes left for the releaser now (the ladder's landing credit: resident, about to go, never room).
pub fn landing_bytes() -> usize {
    LANDING_BYTES.load(Relaxed)
}

/// D4692: the runs the calling thread gave back so far (its MADV_DONTNEED calls here).
pub fn thread_gives() -> u64 {
    GIVES.try_with(Cell::get).unwrap_or(0)
}

/// Every hot run's pages given back (a cache give-back: `crate::give_back`). The bytes returned. D4692: on a thread that
/// defers (C1's inline take on the asker's thread), handed to the releaser instead (`leave`), counted in the bytes.
pub fn flush() -> usize {
    let base = BASE.load(Relaxed);
    if base == 0 {
        return 0;
    }
    let defer = defers();
    let mut n = 0;
    for (j, h) in HOT.iter().enumerate() {
        let k = j + 1;
        while let Some(i) = pop(h) {
            HOT_BYTES.fetch_sub(k * SLICE, Relaxed);
            if !(defer && leave(i, k)) {
                give(base, i, k);
            }
            n += k * SLICE;
        }
    }
    n
}

/// Bytes in the hot pool now.
pub fn hot_bytes() -> usize {
    HOT_BYTES.load(Relaxed)
}

/// A run's pages to the system and the run to its length's stack.
fn give(base: usize, i: usize, k: usize) {
    let p = (base + i * SLICE) as *mut u8;
    let _ = p;
    let _ = GIVES.try_with(|g| g.set(g.get() + 1));
    #[cfg(all(target_os = "linux", not(target_family = "wasm")))]
    // SAFETY: the run is the caller's (freed now) and lies in our range.
    unsafe {
        libc::madvise(p as *mut libc::c_void, k * SLICE, libc::MADV_DONTNEED)
    };
    // a stack holds at most slices / k runs of its length: never full
    let pushed = push(&STACKS[k - 1], i);
    debug_assert!(pushed);
    memledger::arena::note_taken(true, -((k * SLICE) as i64));
}

/// Runs taken now.
pub fn live_runs() -> usize {
    LIVE.load(Relaxed)
}
