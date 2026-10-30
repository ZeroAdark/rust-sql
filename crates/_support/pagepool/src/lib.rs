//! pgrust's warm page pool: one reservation (pool chunks of 2 MiB superpages, lazy spans, the frame zone, meta tables);
//! memory stays warm across frees and only the purger gives it back. Take and give use no lock, no pgsync, no heap.

#![cfg_attr(any(target_family = "wasm", loom), allow(dead_code, unused_imports, unused_variables))]

mod affinity;
mod api;
mod central;
mod claimer;
#[cfg(target_family = "wasm")]
mod fallback;
pub mod frames;
#[cfg(all(not(target_family = "wasm"), not(miri)))]
mod os;
#[cfg(miri)]
#[path = "os_miri.rs"]
mod os;
#[cfg(feature = "global")]
mod global;
mod present;
mod purger;
mod reserve;
mod run;
mod shelf;
#[cfg(all(feature = "global", not(target_family = "wasm")))]
mod small;
mod span;
mod state;
mod stats;
mod superpage;
#[cfg(test)]
mod tests;
mod warmres;

#[cfg(target_family = "wasm")]
use fallback as os;

use crate::sync::{AtomicBool, AtomicU16, AtomicU8, AtomicUsize, Ordering::*};

pub use affinity::{helper_pins, set_helper_cpus, HELPER_MAX_CPUS};
pub use api::*;
pub use claimer::{claim_room, prefill, set_claim_room};
pub use present::{build_present, cache_present, free_present, meta_present, present, present_age_ms, present_due_now, present_fresh, present_split, present_split_build, present_sweep, present_sweep_by, present_sweep_ns, present_sweeps, present_zero, refresh_present, in_ram_seam, residency_seam};
pub use purger::{guard_counts, host_counts, set_host_pressed, HostCounts, landing, lazy_pass_now, note_end_give, lazy_pass_timed_now, peak_counts, purge_now, r1_counts, GuardCounts, PeakCounts, CLIMB_MIN, CLIMB_MS, LEVER_AGE, LEVER_DEAD1, LEVER_SKIP, LEVER_WARM1, SKIP_MIN, WALK_MS, GUARD_BINS, GUARD_BIN_ROWS, GUARD_BIN_US, releasable, return_warm, return_warm_now, serve_ask_now, set_pressure, Pressure, R1Counts, R1_BINS, R1_BIN_MS, R1_BIN_ROWS, R1_STEPS, R1_STEP_ROWS};
pub use run::{run_of, Kind, RunInfo};
pub use shelf::{flush_shelf, low_warm};
pub use span::{commit_to, give_span, grow_span, guard_past, shrink_span, span, span_at, Span};
pub use state::{occupancy, Occupancy};
pub use stats::{moved_in, os_big, set_line, stats, Stats};
pub use warmres::{reserve_ask, reserve_bound, reserve_kept, reserve_on, reserve_probe, reserve_stats, set_reserve_budget, Reserve, POP_BINS, SIZES as RESERVE_SIZES};
#[cfg(feature = "global")]
pub use global::*;

/// A lifetime class. A superpage holds runs of one class only.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Class {
    Task = 0,
    Session = 1,
    Cache = 2,
    Process = 3,
    General = 4,
    Frame = 5,
}

pub(crate) const CLASSES: usize = 6;

impl Class {
    pub(crate) const ALL: [Class; CLASSES] = [Class::Task, Class::Session, Class::Cache, Class::Process, Class::General, Class::Frame];

    #[inline]
    pub(crate) fn from_u8(v: u8) -> Class {
        Class::ALL[(v as usize) % CLASSES]
    }
}

/// What a take needs of its bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Zero {
    No = 0,
    Dense = 1,
    /// Zero, faulted page by page as touched.
    Sparse = 2,
}

/// A stats row; never an owner.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct Tag(pub u8);

impl Tag {
    pub const OTHER: Tag = Tag(0);
    pub const MCX: Tag = Tag(1);
    pub const REGION: Tag = Tag(2);
    pub const PROGRAM: Tag = Tag(3);
    pub const OP: Tag = Tag(4);
    pub const CLAIM: Tag = Tag(5);
    pub const BUILD: Tag = Tag(6);
    pub const PART: Tag = Tag(7);
    /// pppress (B): the resident fp tables' build buffers while the cache holds them (`retag` from BUILD at the cache's
    /// take, back at the table's drop); the present sweep counts them as cache. Never a door-1 tag.
    pub const DICT: Tag = Tag(8);
    pub const COND: Tag = Tag(9);
    pub const L2: Tag = Tag(10);
    pub const SMALL: Tag = Tag(11);
    pub const FRAME: Tag = Tag(12);
    pub const BUFFER: Tag = Tag(13);
    /// The global allocator's blocks past the small tier.
    pub const HEAP: Tag = Tag(14);
    /// Blocks and small-tier runs built inside a cache scope (`cache_alloc`): the cache's, never a statement's.
    pub const CACHE: Tag = Tag(15);
    pub const COUNT: usize = 16;

    #[inline]
    pub(crate) fn idx(self) -> usize {
        self.0 as usize % Tag::COUNT
    }
}

/// How a take is served: its class, zero need, stats row and what it may grow to.
#[derive(Clone, Copy, Debug)]
pub struct Hint {
    pub class: Class,
    pub zero: Zero,
    pub tag: Tag,
    /// What the memory may grow to, in KiB (0 = unknown).
    pub expect_kib: u32,
}

impl Hint {
    pub const fn new(class: Class, tag: Tag) -> Hint {
        Hint { class, zero: Zero::No, tag, expect_kib: 0 }
    }

    pub const fn zero(self, z: Zero) -> Hint {
        Hint { zero: z, ..self }
    }

    pub const fn expect_kib(self, k: u32) -> Hint {
        Hint { expect_kib: k, ..self }
    }
}

/// The pool's sizes (bytes unless named otherwise).
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub va: usize,
    pub chunk: usize,
    pub lazy_va: usize,
    pub frame_va: usize,
    pub shelf_max: usize,
    pub warm_zero: usize,
    pub warm_zero_max: usize,
    pub warm_cap: usize,
    pub decay_ms: u32,
    pub purge_batch: usize,
    pub lazy_min: usize,
    pub idle_drain_ms: u32,
    pub block_min: usize,
    /// 0 = age release, 1 = age release on pressure after 60 s idle, 2 = pressure only (`pool_release_mode`).
    pub release_mode: u8,
    /// pagepool P3 (F1): warm lazy spans and dead runs untouched this long go back by age, whatever `release_mode` says
    /// (ms; `pool_lazy_decay_ms`); 0 = on pressure only.
    pub lazy_decay_ms: u32,
    /// pagepool P3 (F2): warm lazy span and dead run bytes past this go back (`pool_lazy_cap`); 0 = no cap.
    pub lazy_cap: usize,
    /// pagepool P3 (F1 / F2): the most one age or cap pass gives back by remap (`pool_age_pass_max`); 0 = no limit.
    pub age_pass_max: usize,
    /// D4760: a give-back of part of a superpage advised huge while it is in use (`pool_thp_partial`): 0 = as before,
    /// 1 = keep (whole superpages only), 2 = split the folio first, 3 = split cold only (`thp_partial`).
    pub thp_partial: u8,
    /// D4872 (the press's churn guard): pressed by the host (`set_host_pressed`), the pool gives back only free
    /// superpages free this long or more (`pool_host_age_ms`); 0 = all its whole free memory at once.
    pub host_age_ms: u32,
}

/// The frame zone's address space (frames' design: 16 TiB; 1 GiB under cfg(pagepool_tsan): no TSan app range holds 16 TiB).
const FRAME_VA: usize = if cfg!(pagepool_tsan) { 1 << 30 } else if usize::BITS >= 64 { (16u64 << 40) as usize } else { 0 };

impl Config {
    #[cfg(loom)]
    pub fn from_policy() -> Config {
        unreachable!("no memory policy in the loom build")
    }

    /// The policy's knobs, sized by physical memory.
    #[cfg(not(loom))]
    pub fn from_policy() -> Config {
        use mempolicy::consts::{get, K};
        let phys = os::phys_bytes().max(1 << 30);
        let chunk = (get(K::PoolChunk) as usize).max(superpage::SUPER).next_multiple_of(superpage::SUPER);
        Config {
            va: phys.saturating_mul(get(K::PoolVaMul) as usize).next_multiple_of(chunk),
            chunk,
            lazy_va: phys.saturating_mul(get(K::PoolLazyVaMul) as usize),
            frame_va: FRAME_VA,
            shelf_max: get(K::PoolShelfMax) as usize,
            warm_zero: get(K::PoolWarmZero) as usize,
            warm_zero_max: get(K::PoolWarmZeroMax) as usize,
            warm_cap: get(K::PoolWarmCap) as usize,
            decay_ms: u32::try_from(get(K::PoolDecayMs)).unwrap_or(u32::MAX),
            purge_batch: get(K::PoolPurgeBatch) as usize,
            lazy_min: get(K::PoolLazyMin) as usize,
            idle_drain_ms: u32::try_from(get(K::PoolIdleDrainMs)).unwrap_or(u32::MAX),
            block_min: get(K::PoolBlockMin) as usize,
            release_mode: get(K::PoolReleaseMode).min(2) as u8,
            lazy_decay_ms: u32::try_from(get(K::PoolLazyDecayMs)).unwrap_or(u32::MAX),
            lazy_cap: get(K::PoolLazyCap) as usize,
            age_pass_max: get(K::PoolAgePassMax) as usize,
            thp_partial: get(K::PoolThpPartial).min(THP_COLD as u64) as u8,
            host_age_ms: u32::try_from(get(K::PoolHostAgeMs)).unwrap_or(u32::MAX),
        }
    }
}

/// Which consumers draw from the pool (`PGRUST_POOL_*=1`, read once). In the pool-global build (`global`, the one that
/// ships) `PGRUST_POOL_MCX` and `PGRUST_POOL_GRAVITY` are ON unless `=0` (the user 2026-10-18: every gate and no-loss take
/// ran them on); the hatch (mimalloc serving: `PGRUST_ALLOCATOR=mimalloc`, or the server's default off Linux)
/// still turns every switch off.
#[derive(Clone, Copy, Debug, Default)]
pub struct Switches {
    pub mcx: bool,
    pub gravity: bool,
    pub cache: bool,
    pub buffers: bool,
    pub keepers: bool,
    pub debug: bool,
}

impl Switches {
    pub fn any(&self) -> bool {
        self.mcx || self.gravity || self.cache || self.buffers || self.keepers || self.debug
    }
}

const SWITCH_NAMES: [&core::ffi::CStr; 6] =
    [c"PGRUST_POOL_MCX", c"PGRUST_POOL_GRAVITY", c"PGRUST_POOL_CACHE", c"PGRUST_POOL_BUFFERS", c"PGRUST_POOL_KEEPERS", c"PGRUST_POOL_DEBUG"];
const SWITCHES_READ: u16 = 1 << 15;
/// The switches on unless `=0` (SWITCH_NAMES' first two, mcx and gravity) in the pool-global build; elsewhere every switch is
/// `=1` to turn on.
const TENANTS_DEFAULT_ON: bool = cfg!(all(feature = "global", not(target_family = "wasm")));
static SWITCHES: AtomicU16 = AtomicU16::new(0);

/// The switches, read from the environment at the first call.
pub fn switches() -> Switches {
    let mut v = SWITCHES.load(Relaxed);
    if v & SWITCHES_READ == 0 {
        v = SWITCHES_READ;
        for (i, name) in SWITCH_NAMES.iter().enumerate() {
            let on = if TENANTS_DEFAULT_ON && i < 2 { os::env_on_by_default(name) } else { os::env_is_one(name) };
            if !hatch_on() && on {
                v |= 1 << i;
            }
        }
        SWITCHES.store(v, Relaxed);
    }
    Switches {
        mcx: v & 1 != 0,
        gravity: v & 2 != 0,
        cache: v & 4 != 0,
        buffers: v & 8 != 0,
        keepers: v & 16 != 0,
        debug: v & 32 != 0,
    }
}

static LO: AtomicUsize = AtomicUsize::new(0);
static LEN: AtomicUsize = AtomicUsize::new(0);
static ON: AtomicBool = AtomicBool::new(false);
static INIT: AtomicU8 = AtomicU8::new(0);
pub(crate) static DEBUG: AtomicBool = AtomicBool::new(false);
#[cfg_attr(not(feature = "global"), allow(dead_code))]
pub(crate) static GLOBAL_ON: AtomicBool = AtomicBool::new(false);

/// A value alone on 128 B (a line and its prefetch pair): written-by-many statics, kept off the lines takes read.
#[repr(align(128))]
pub(crate) struct Line<T>(pub(crate) T);

impl<T> core::ops::Deref for Line<T> {
    type Target = T;
    #[inline(always)]
    fn deref(&self) -> &T {
        &self.0
    }
}
pub(crate) static CAP: AtomicUsize = AtomicUsize::new(0);

/// Reserve the pool's address space: once, before the threads that take from it; no heap. False: the pool stays off.
pub fn init(cfg: &Config) -> bool {
    if hatch_on() {
        return false;
    }
    if INIT.compare_exchange(0, 1, Acquire, Acquire).is_err() {
        while INIT.load(Acquire) == 1 {
            core::hint::spin_loop();
        }
        return ON.load(Acquire);
    }
    // ppres (memacct 9d 22:04:33Z): PGRUST_POOL_RESERVE is read once, here, before the pool's first put
    let _ = warmres::on();
    THP_PARTIAL.store(cfg.thp_partial.min(THP_COLD), Relaxed);
    // pagepool P3 (F1): PGRUST_POOL_FENCE is read once, here, before the first lazy span
    let _ = span::fence_on();
    // pagepool P3 (F1): PGRUST_POOL_TOUCH is read once, here, before the claimer starts
    let _ = claimer::touch_on();
    // D4787: PGRUST_POOL_SPAN_TRIM is read once, here, before the first lazy span
    let _ = span::trim_on();
    // D4760 (pppress): PGRUST_POOL_FAST_RELEASE is read once, here, before the purger starts; PGRUST_POOL_FINE_CLASSES
    // at the first size lookup, at the latest here, before the first block
    let _ = purger::fast_on();
    let _ = run::fine_on();
    // pppress card 6: PGRUST_POOL_GROW_GUARD is read once, here
    let _ = purger::guard_on();
    // D4872: PGRUST_POOL_HOLE_FIT is read once, here, before the first partial superpage is listed
    let _ = state::hole_fit_on();
    // pppress card 7: PGRUST_POOL_SPAN_MOVE is read once, here, before the first lazy span
    let _ = span::move_on();
    // pppress: PGRUST_POOL_SPAN_FIT is read once, here, before the first span
    let _ = api::fit_on();
    // pppress card 11: PGRUST_POOL_PRESENT_MINCORE is read once, here, before the first sweep
    present::init_reader();
    // pppress card 10: PGRUST_POOL_IDLE_TAIL is read once, here, before the first session
    #[cfg(all(feature = "global", not(target_family = "wasm")))]
    let _ = small::tail_on();
    // ppround (card 12): PGRUST_POOL_FIT_TAIL is read once, here, before the first span
    let _ = api::fit_tail_on();
    // ppround (card 14): PGRUST_POOL_PEAK_RELEASE is read once, here, before the purger starts
    let _ = purger::peak_on();
    let ok = match reserve::reserve(cfg) {
        Some(z) => {
            DEBUG.store(switches().debug, Relaxed);
            state::init(z);
            shelf::init();
            frames::init();
            claimer::init();
            LO.store(z.all.lo, Relaxed);
            LEN.store(z.all.len, Relaxed);
            ON.store(true, Release);
            true
        }
        None => false,
    };
    INIT.store(2, Release);
    ok
}

/// Start the claimer and purger threads (after the runtime is up). False when the pool is off or a thread did not start.
pub fn start() -> bool {
    on() && claimer::start() && purger::start()
}

/// Stop the claimer and purger threads at their next wake (process exit, tests).
pub fn stop() {
    claimer::stop();
    purger::stop();
}

static DOOR: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

/// The memory ledger's pool door: every change of the pool's claimed bytes. Under `global` (PoolAlloc claims from the
/// process's first allocation, before the binary installs the door) also, at install, the claims made before it.
pub fn set_door(f: fn(i64)) {
    #[cfg(feature = "global")]
    {
        let before = stats::claimed();
        if DOOR.swap(f as *mut (), AcqRel).is_null() && before > 0 {
            f(before as i64);
        }
    }
    #[cfg(not(feature = "global"))]
    DOOR.store(f as *mut (), Release);
}

/// Whether this build has PoolAlloc compiled in (feature `global`, main_main's `pagepool-global`). A constant, so a
/// caller's branch for the other build folds away and the off build keeps its code.
pub const GLOBAL_BUILD: bool = cfg!(feature = "global");

/// pphatch: mimalloc serves this process's `PoolAlloc` calls, chosen at its first allocation (now, if none yet) by
/// `PGRUST_ALLOCATOR=mimalloc` or, naming no allocator, by the default (`hearth_default`: in the server, mimalloc off
/// Linux aarch64); the pool then neither reserves nor starts a thread, and the `PGRUST_POOL_*` switches read off. False
/// in a build without `PoolAlloc`.
#[inline]
pub fn hatch_on() -> bool {
    #[cfg(all(feature = "global", not(target_family = "wasm")))]
    return global::hatch();
    #[cfg(not(all(feature = "global", not(target_family = "wasm"))))]
    false
}

/// `PoolAlloc` serves this process's allocations: a `global` build without the hatch.
#[inline]
pub fn pool_allocates() -> bool {
    cfg!(all(feature = "global", not(target_family = "wasm"))) && !hatch_on()
}

/// Times `PoolAlloc` read `PGRUST_ALLOCATOR`: 1 in a process and in every child it forks (0 in a build without it).
pub fn allocator_decides() -> u32 {
    #[cfg(all(feature = "global", not(target_family = "wasm")))]
    return global::decides();
    #[cfg(not(all(feature = "global", not(target_family = "wasm"))))]
    0
}

/// The user 2026-10-18 ("for now can we run mimalloc on x86 and hearth on graviton?"), then (D4872, "Yes, after one x86
/// check": x86_64 Linux with SPAN_FIT off bulk-loads a wide table ~5.5 % faster than mimalloc, scan and join suites no
/// worse): in the server, where `PGRUST_ALLOCATOR` names no allocator (unset, or another word), Hearth serves on Linux
/// aarch64 (Graviton) and x86_64, mimalloc everywhere else (macOS on Apple silicon). One build: a default at run time,
/// decided with `PGRUST_ALLOCATOR` at the first allocation. Only a binary that says `by_arch_default!()` (the server)
/// takes it; any other keeps the pool.
pub const HEARTH_BY_ARCH: bool = cfg!(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64")));

/// `by_arch_default!`'s marks: the binary takes the per-architecture default; its constructor ran after the choice.
static BY_ARCH: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static BY_ARCH_LATE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Whether Hearth serves for `PGRUST_ALLOCATOR`'s value `env` under a default of Hearth (`hearth_default`) or mimalloc:
/// "mimalloc" never, "hearth" and the old "pagepool" always (ASCII case ignored), anything else or unset the default.
/// Pure, so a test reads both architectures' rule on either.
pub fn hearth_chosen(env: Option<&str>, hearth_default: bool) -> bool {
    match env {
        Some(v) if v.eq_ignore_ascii_case("mimalloc") => false,
        Some(v) if v.eq_ignore_ascii_case("hearth") || v.eq_ignore_ascii_case("pagepool") => true,
        _ => hearth_default,
    }
}

/// This process's allocator where `PGRUST_ALLOCATOR` names none: in a binary that said `by_arch_default!()` (the
/// server), Hearth on Linux aarch64 and x86_64 (`HEARTH_BY_ARCH`) and mimalloc elsewhere; in any other (tests, tools),
/// Hearth.
pub fn hearth_default() -> bool {
    !BY_ARCH.load(core::sync::atomic::Ordering::Relaxed) || HEARTH_BY_ARCH
}

/// `by_arch_default!()` ran in this binary (it takes the per-architecture default).
pub fn by_arch() -> bool {
    BY_ARCH.load(core::sync::atomic::Ordering::Relaxed)
}

/// `by_arch_default!()`'s constructor ran after the allocator was chosen (another constructor allocated first), so the
/// per-architecture default did not apply: the server's start line says so.
pub fn by_arch_late() -> bool {
    BY_ARCH_LATE.load(core::sync::atomic::Ordering::Relaxed)
}

/// `PGRUST_ALLOCATOR` as set (for the start line's why).
pub fn allocator_env() -> Option<&'static str> {
    os::env_str(c"PGRUST_ALLOCATOR")
}

/// For the binary that installs `PoolAlloc` as its global allocator and ships (the server): where `PGRUST_ALLOCATOR`
/// names no allocator, the per-architecture default (`HEARTH_BY_ARCH`) instead of Hearth. A constructor, so it runs before
/// the binary's first allocation, where the choice is made: on Linux `.init_array.00050`, since the linker runs the
/// prioritised constructors first and in ascending order, and the server's own `tune_allocator` (00100) and std's
/// arguments (00099) come after it (an unprioritised `.init_array` ran after `tune_allocator`'s first allocation on x86:
/// late); Mach-O `__mod_init_func` (unordered there; `by_arch_late` tells).
#[macro_export]
macro_rules! by_arch_default {
    () => {
        #[used]
        #[cfg_attr(target_os = "linux", link_section = ".init_array.00050")]
        #[cfg_attr(target_vendor = "apple", link_section = "__DATA,__mod_init_func")]
        static PAGEPOOL_BY_ARCH_DEFAULT: extern "C" fn() = {
            extern "C" fn by_arch_default() {
                $crate::by_arch_default_now();
            }
            by_arch_default
        };
    };
}

/// `by_arch_default!`'s constructor (before the first allocation; late if a choice was made already).
#[doc(hidden)]
pub fn by_arch_default_now() {
    if allocator_decides() > 0 {
        BY_ARCH_LATE.store(true, core::sync::atomic::Ordering::Relaxed);
    }
    BY_ARCH.store(true, core::sync::atomic::Ordering::Relaxed);
}

/// `PGRUST_ALLOCATOR`'s value when it names no allocator ("mimalloc", "hearth" or the old "pagepool"; ASCII case ignored).
pub fn allocator_env_unknown() -> Option<&'static str> {
    os::env_str(c"PGRUST_ALLOCATOR").filter(|v| !["mimalloc", "hearth", "pagepool"].iter().any(|n| v.eq_ignore_ascii_case(n)))
}

#[inline]
pub(crate) fn door(delta: i64) {
    let f = DOOR.load(Acquire);
    if !f.is_null() && delta != 0 {
        // SAFETY: only `set_door` stores here, always a `fn(i64)`.
        let f: fn(i64) = unsafe { core::mem::transmute::<*mut (), fn(i64)>(f) };
        f(delta);
    }
}

/// The most the pool claims from the OS (0 = the memory budget B; none before B is known). Past it a claim refuses.
pub fn set_cap(bytes: usize) {
    CAP.store(bytes, Relaxed);
}

#[inline]
pub(crate) fn global_uncapped() -> bool {
    #[cfg(all(feature = "global", not(target_family = "wasm")))]
    return global::uncapped();
    #[cfg(not(all(feature = "global", not(target_family = "wasm"))))]
    false
}

#[inline(always)]
pub fn on() -> bool {
    ON.load(Relaxed)
}

/// The session's idle point (after ReadyForQuery, before a thread parks): hand current small runs to other threads
/// for reuse, and send pending remote frees. Live blocks stay valid. One relaxed load unless `PoolAlloc` serves the
/// process; otherwise the scan is bounded by its fixed size classes, without walking partial or full lists.
#[inline]
pub fn idle_point() {
    #[cfg(all(feature = "global", not(target_family = "wasm")))]
    if GLOBAL_ON.load(Relaxed) {
        small::idle_point();
    }
}

/// A cut's end: this thread's pending remote frees out, then abandoned runs with every block home back to the pool
/// (their threads need not run). No allocation, no syscall. The runs given back.
pub fn flush_remote() -> usize {
    #[cfg(all(feature = "global", not(target_family = "wasm")))]
    if GLOBAL_ON.load(Relaxed) {
        return small::flush_remote();
    }
    0
}

/// `PoolAlloc` serves this process's allocations (set at its first: the pool's start from it, or a thread's first small
/// block). False in every binary without it, whatever `on()` says.
pub fn global_on() -> bool {
    GLOBAL_ON.load(Relaxed)
}

/// D4760 (`pool_thp_partial`, set at `init`): what a give-back of part of a superpage the pool advised huge does while the
/// superpage is in use. A MADV_DONTNEED inside a PMD-mapped THP splits the mapping but leaves the 2 MiB folio allocated
/// and charged to the cgroup in full until the kernel splits it (deferred split: under reclaim only, and seldom), so the
/// pages read as given back (RssAnon, the pool's own count) while memory.current keeps them: the D4760 cancels.
static THP_PARTIAL: AtomicU8 = AtomicU8::new(0);
/// 1: keep. Only whole superpages go back; the free and dead runs of a superpage in use stay held (resident, reusable,
/// counted by the pool and in `releasable`'s complement). KNOWN LIMIT (pppress card 13 (3)): a parked Gravity pool
/// region's dead runs there are in neither the ladder's R1 reach (`releasable`) nor its R2p reach (they are off the
/// region's account); in 0, 2 and 3 R1 releases them under pressure.
pub(crate) const THP_KEEP: u8 = 1;
/// 2: split first. A MADV_COLD over one page of each partly given-back superpage splits its folio
/// (`os::split_partial`), then the give-back as before frees and uncharges the pages.
pub(crate) const THP_SPLIT: u8 = 2;
/// 3 (pppress card 8): split only cold superpages. The purger's age passes (`pool_lazy_decay_ms`) give back the free and
/// dead runs of a superpage in use, split first as 2 does, only once nothing was taken from, given back to or committed
/// in that superpage (or lazy span piece) for a whole decay (`state::note_active`, cleared at each age mark); a hot one
/// keeps them held and counted until it goes cold. An inline `give_back_any` over part of a superpage keeps it as 1
/// does. With the ladder pressing (`purger::ladder_pressed`), and on every pressure road (R1, the purge, the guard, the
/// cap), mode 2's split and give-back. Splitting the folio of a superpage a running query still uses costs that query
/// (a top-N query's first try 2.3x A at mode 2, 188 splits, its query threads' minor faults doubled).
pub(crate) const THP_COLD: u8 = 3;

/// The mode now: `pool_thp_partial`, 0 where the host has no THP (no folio to keep whole).
#[inline]
pub(crate) fn thp_partial() -> u8 {
    let m = THP_PARTIAL.load(Relaxed);
    if m == 0 || os::thp() == os::Thp::Never { 0 } else { m }
}

/// Test seam: the `pool_thp_partial` mode from here on (0 = as before, 1 = keep, 2 = split first, 3 = split cold only).
#[doc(hidden)]
pub fn set_thp_partial(mode: u8) {
    THP_PARTIAL.store(mode.min(THP_COLD), Relaxed);
}

/// pppress card 8 (`pool_thp_partial` 3): the age passes' hot skips so far (superpages and lazy span pieces whose free or
/// dead runs stayed held because something was taken, given back or committed there within the decay).
pub fn thp_hot_skips() -> u64 {
    stats::THP_HOT_SKIPS.load(Relaxed)
}

/// D4787: reused warm lazy spans, either switch. `PGRUST_POOL_SPAN_TRIM` (on), zeroings (count, bytes, ns, the longest
/// ns), trims (count, bytes off the commit as counted, resident bytes given back, ns).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpanReuse {
    pub trim_on: bool,
    pub zero_takes: u64,
    pub zero_bytes: u64,
    pub zero_ns: u64,
    pub zero_max_ns: u64,
    pub trims: u64,
    pub trimmed: u64,
    pub trim_resident: u64,
    pub trim_ns: u64,
}

pub fn span_reuse() -> SpanReuse {
    use span::{TRIMMED, TRIMS, TRIM_NS, TRIM_RESIDENT, ZERO_BYTES, ZERO_MAX_NS, ZERO_NS, ZERO_TAKES};
    SpanReuse {
        trim_on: span::trim_on(),
        zero_takes: ZERO_TAKES.load(Relaxed),
        zero_bytes: ZERO_BYTES.load(Relaxed),
        zero_ns: ZERO_NS.load(Relaxed),
        zero_max_ns: ZERO_MAX_NS.load(Relaxed),
        trims: TRIMS.load(Relaxed),
        trimmed: TRIMMED.load(Relaxed),
        trim_resident: TRIM_RESIDENT.load(Relaxed),
        trim_ns: TRIM_NS.load(Relaxed),
    }
}

/// pppress card 7 (`PGRUST_POOL_SPAN_MOVE`): lazy-span commits served by free warm superpages moved in, counted with the
/// switch off too.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpanMoves {
    pub on: bool,
    /// Commits that moved superpages in, the bytes moved, the bytes zeroed first, ns summed and the longest.
    pub moves: u64,
    pub bytes: u64,
    pub zeroed: u64,
    pub ns: u64,
    pub max_ns: u64,
    /// Moves refused (the map budget, or the remap).
    pub refused: u64,
    /// Lazy grows whose remap the kernel refused (the caller copied).
    pub grow_refused: u64,
}

pub fn span_moves() -> SpanMoves {
    use span::{GROW_REMAP_REFUSED, MOVED_BYTES, MOVES, MOVE_MAX_NS, MOVE_NS, MOVE_REFUSED, MOVE_ZEROED};
    SpanMoves {
        on: span::move_on(),
        moves: MOVES.load(Relaxed),
        bytes: MOVED_BYTES.load(Relaxed),
        zeroed: MOVE_ZEROED.load(Relaxed),
        ns: MOVE_NS.load(Relaxed),
        max_ns: MOVE_MAX_NS.load(Relaxed),
        refused: MOVE_REFUSED.load(Relaxed),
        grow_refused: GROW_REMAP_REFUSED.load(Relaxed),
    }
}

/// D4885: warm lazy spans whose address space a new span took, the lazy zone otherwise full (count, bytes of address
/// space).
pub fn warm_va_drops() -> (u64, u64) {
    (span::WARM_VA_DROPS.load(Relaxed), span::WARM_VA_DROPPED.load(Relaxed))
}

/// D4760 (pppress): `PGRUST_POOL_FINE_CLASSES` (four size classes per doubling for 1-128 KiB blocks).
pub fn fine_classes_on() -> bool {
    run::fine_on()
}

/// D4760 (`pool_thp_partial`): (mode, partial give-backs kept and their bytes, split asks and their ns).
pub fn thp_partial_counts() -> (u8, u64, u64, u64, u64) {
    use stats::{THP_KEPT, THP_KEPT_BYTES, THP_SPLITS, THP_SPLIT_NS};
    (thp_partial(), THP_KEPT.load(Relaxed), THP_KEPT_BYTES.load(Relaxed), THP_SPLITS.load(Relaxed), THP_SPLIT_NS.load(Relaxed))
}

/// `pagegive::give_back` over dead whole pages of a live block from either allocator: named the pool's when the pool owns
/// the range, else the global allocator's; `huge`: the range's THP advice.
/// # Safety
/// As `pagegive::give_back`.
#[cfg(target_os = "linux")]
pub unsafe fn give_back_any(lo: usize, len: usize, huge: bool) -> core::ffi::c_int {
    let pool = owns(lo as *const u8);
    if pool && len > 0 && span::huge_at(lo) {
        match thp_partial() {
            // SAFETY: the caller's contract.
            THP_KEEP => return unsafe { give_back_whole(lo, len, huge) },
            THP_SPLIT => os::split_partial(lo, len),
            // pppress card 8: an owner giving back part of its own superpage is using it: kept as 1, unless the ladder
            // presses (then split as 2)
            // SAFETY: the caller's contract.
            THP_COLD if !purger::ladder_pressed() => return unsafe { give_back_whole(lo, len, huge) },
            THP_COLD => os::split_partial(lo, len),
            _ => {}
        }
    }
    let m = if pool { pagegive::Mapping::Pool { huge } } else { pagegive::Mapping::Allocator { huge } };
    // SAFETY: the caller's contract.
    unsafe { pagegive::give_back(lo, len, m) }
}

/// D4760 `THP_KEEP` for `give_back_any`: the whole superpages inside `[lo, lo + len)` go back; the parts of superpages
/// the range only covers in part stay mapped, written zero (the contract: the range reads zero after). With no whole
/// superpage inside, nothing is done and -1 is returned: the callers' refusal road (they zero or keep the bytes
/// themselves), so no superpage is left partly unmapped.
/// # Safety
/// As `give_back_any`, over the pool's own range.
#[cfg(target_os = "linux")]
unsafe fn give_back_whole(lo: usize, len: usize, huge: bool) -> core::ffi::c_int {
    let end = lo + len;
    let (a, b) = (lo.next_multiple_of(superpage::SUPER), end / superpage::SUPER * superpage::SUPER);
    stats::THP_KEPT.fetch_add(1, Relaxed);
    if b <= a {
        stats::THP_KEPT_BYTES.fetch_add(len as u64, Relaxed);
        return -1;
    }
    // SAFETY: whole superpages inside the caller's range (its contract).
    let rc = unsafe { pagegive::give_back(a, b - a, pagegive::Mapping::Pool { huge }) };
    if rc != 0 {
        return rc;
    }
    stats::THP_KEPT_BYTES.fetch_add(((a - lo) + (end - b)) as u64, Relaxed);
    // SAFETY: the head and tail of the caller's range, its bytes dead (its contract).
    unsafe {
        core::ptr::write_bytes(lo as *mut u8, 0, a - lo);
        core::ptr::write_bytes(b as *mut u8, 0, end - b);
    }
    0
}

/// pppress card 10 (`PGRUST_POOL_IDLE_TAIL`, default off): at a session's idle release, each small run the calling thread
/// owns with blocks out gives its whole pages past its last used block back to the OS, for 0.5 ms or so a call. The
/// resident bytes given back (0 with the switch off, the pool off, or no small heap on this thread), and whether the
/// call stopped at its budget with runs left: then call again with `resume` (a timer slice later), which goes on from
/// the run it stopped at; without, a cut starts over at the first class. Idle only: it walks the thread's free lists.
pub fn idle_tails(resume: bool) -> (usize, bool) {
    #[cfg(all(feature = "global", not(target_family = "wasm")))]
    if on() && small::tail_on() {
        return small::give_idle_tails(resume);
    }
    let _ = resume;
    (0, false)
}

/// pppress card 10: (`PGRUST_POOL_IDLE_TAIL` on, runs cut, resident bytes they gave back, tails left for a later slice
/// while the purger split their huge folio, splits the purger made for them).
pub fn idle_tail_counts() -> (bool, u64, u64, u64, u64) {
    #[cfg(all(feature = "global", not(target_family = "wasm")))]
    {
        let (runs, bytes, defers) = small::tail_counts();
        (small::tail_on(), runs, bytes, defers, purger::split_counts().1)
    }
    #[cfg(not(all(feature = "global", not(target_family = "wasm"))))]
    (false, 0, 0, 0, 0)
}

/// pppress card 10: idle cuts' times: (cuts, total ns, longest ns, cuts the budget stopped, cuts by ns: bin 0 under 1 us,
/// bin k under 2^k us, the last bin the rest).
pub fn idle_tail_times() -> (u64, u64, u64, u64, [u64; 16]) {
    #[cfg(all(feature = "global", not(target_family = "wasm")))]
    {
        small::tail_times()
    }
    #[cfg(not(all(feature = "global", not(target_family = "wasm"))))]
    (0, 0, 0, 0, [0; 16])
}

/// C.23: `f`'s zeroed block of `bytes` as never-touched base pages (zero, committed at their first write, no give-back
/// here). `None`, `f` not run, where `PoolAlloc` does not serve the process or `bytes` is under a superpage.
#[inline]
pub fn base_pages<T>(bytes: usize, f: impl FnOnce() -> T) -> Option<T> {
    #[cfg(all(feature = "global", not(target_family = "wasm")))]
    if GLOBAL_ON.load(Relaxed) && bytes >= superpage::SUPER && on() {
        return Some(global::with_base(f));
    }
    let _ = (bytes, f);
    None
}

/// Whether `p` lies in the pool's reservation.
#[inline(always)]
pub fn owns(p: *const u8) -> bool {
    (p as usize).wrapping_sub(LO.load(Relaxed)) < LEN.load(Relaxed)
}

/// `owns` less `PoolAlloc`'s blocks, which share the reservation (D4720): whether `take` handed `p` out.
#[inline]
pub fn taken(p: *const u8) -> bool {
    #[cfg(all(feature = "global", not(target_family = "wasm")))]
    if GLOBAL_ON.load(Relaxed) && owns(p) {
        return !global::is_global_block(p);
    }
    owns(p)
}

/// The reservation as (LO, LEN), for inline range tests elsewhere.
pub fn range() -> (usize, usize) {
    (LO.load(Relaxed), LEN.load(Relaxed))
}

/// The syscall log and host facts, for tests.
#[cfg(all(not(target_family = "wasm"), not(miri)))]
#[doc(hidden)]
pub mod hooks {
    pub use crate::os::log::{clear, each, Op};

    pub fn thp_never() -> bool {
        matches!(crate::os::thp(), crate::os::Thp::Never)
    }

    pub fn page_size() -> usize {
        crate::os::page_size()
    }

    pub fn nodes() -> u8 {
        crate::os::nodes()
    }

    /// pagepool P3 (F1): a swap device is listed, so the age release reads a range's pages by pagemap (a swapped page
    /// apart from one never faulted) and clears one with a swapped page by MADV_DONTNEED (off Linux: no; no residency
    /// is read there, every range is remapped).
    pub fn swap_on() -> bool {
        #[cfg(target_os = "linux")]
        return crate::os::swap_on();
        #[cfg(not(target_os = "linux"))]
        false
    }

    pub fn frame_budget(n: usize) {
        crate::frames::set_budget(n)
    }

    pub fn clean_base() -> usize {
        crate::span::clean_base(usize::MAX)
    }

    pub fn base_dirty() -> u32 {
        crate::span::base_dirty()
    }

    /// A return pass asked for or running.
    pub fn return_pending() -> bool {
        crate::purger::return_pending()
    }

    pub fn zero_fills() -> u64 {
        crate::api::ZERO_FILLS.load(core::sync::atomic::Ordering::Relaxed)
    }

    pub fn shelved_here() -> usize {
        crate::shelf::mine()
    }

    pub fn check() -> Result<(), &'static str> {
        crate::stats::check()
    }

    /// `PGRUST_POOL_RESERVE` as if the environment said `on` (before the claimer starts).
    pub fn reserve_switch(on: bool) {
        crate::warmres::force(on)
    }

    /// pagepool P3 (F1): `PGRUST_POOL_FENCE` as if the environment said `on` (before the pool's first lazy span).
    pub fn fence_switch(on: bool) {
        crate::span::force_fence(on)
    }

    /// pagepool P3 (F1): `PGRUST_POOL_TOUCH` as if the environment said `on` (before the claimer starts).
    pub fn touch_switch(on: bool) {
        crate::claimer::force_touch(on)
    }

    /// D4787: `PGRUST_POOL_SPAN_TRIM` as if the environment said `on` (any time: it is read at each lazy span ask).
    pub fn span_trim_switch(on: bool) {
        crate::span::force_trim(on)
    }

    /// D4760 (pppress): `PGRUST_POOL_FAST_RELEASE` as if the environment said `on` (any time: read at each R1 serve).
    pub fn fast_release_switch(on: bool) {
        crate::purger::force_fast(on)
    }

    /// pppress: `PGRUST_POOL_SPAN_FIT` as if the environment said `on` (any time: read at each span take).
    pub fn span_fit_switch(on: bool) {
        crate::api::force_fit(on)
    }

    /// pppress card 10: `PGRUST_POOL_IDLE_TAIL` as if the environment said `on` (any time: read at each idle release).
    #[cfg(feature = "global")]
    pub fn idle_tail_switch(on: bool) {
        crate::small::force_tail(on)
    }

    /// pppress card 10: an idle cut's time budget in ns (0: one run a call; default 500,000).
    #[cfg(feature = "global")]
    pub fn idle_tail_budget(ns: u64) {
        crate::small::force_tail_budget(ns)
    }

    /// pppress card 10: while `on`, the purger leaves the splits idle tail cuts ask for queued, as if it never ran.
    pub fn purger_split_hold(on: bool) {
        crate::purger::hold_splits(on)
    }

    /// ppround (card 12): `PGRUST_POOL_FIT_TAIL` as if the environment said `on` (before the pool's first span: a live
    /// span's commit must not change rule while it lives).
    pub fn fit_tail_switch(on: bool) {
        crate::api::force_fit_tail(on)
    }

    /// D4872: `PGRUST_POOL_HOLE_FIT` as if the environment said `on` (before the pool's first superpage holds runs: the
    /// lists are binned one way or the other).
    pub fn hole_fit_switch(on: bool) {
        crate::state::force_hole_fit(on)
    }

    /// pppress card 7: `PGRUST_POOL_SPAN_MOVE` as if the environment said `on` (any time: read at each lazy commit).
    pub fn span_move_switch(on: bool) {
        crate::span::force_move(on)
    }

    /// ppround card 14: `PGRUST_POOL_PEAK_RELEASE` as if the environment said `on` (any time: read at each lazy pass).
    pub fn peak_release_switch(on: bool) {
        crate::purger::force_peak(on)
    }

    /// ppround card 14 v3: `PGRUST_POOL_PEAK_RELEASE`'s levers as if the environment said them (`LEVER_SKIP` /
    /// `LEVER_AGE` / `LEVER_DEAD1` / `LEVER_WARM1`; 0: v2 as it was; any time: read at each climbing pass).
    pub fn peak_levers(levers: u32) {
        crate::purger::force_peak_levers(levers)
    }

    /// ppround card 14: the next lazy passes climb (`Some(true)`), never climb (`Some(false)`), or read the claim's
    /// climb as the purger does (`None`).
    pub fn peak_climb(c: Option<bool>) {
        crate::purger::force_climb(c)
    }

    /// pppress card 6: `PGRUST_POOL_GROW_GUARD` as if the environment said `on` (any time: read at each grow).
    pub fn grow_guard_switch(on: bool) {
        crate::purger::force_guard(on)
    }

    /// D4872: `PGRUST_POOL_HOST_PRESSURE` as if the environment said `on` (any time: read at each sample).
    pub fn host_pressure_switch(on: bool) {
        crate::purger::force_host(on)
    }

    /// D4760 (pppress): `PGRUST_POOL_FINE_CLASSES` as if the environment said `on` (before the pool's first block: a
    /// block's size class must not change while it lives).
    pub fn fine_classes_switch(on: bool) {
        crate::run::force_fine(on)
    }

    /// One claimer pass's reserve steps on the calling thread: the window and the target, and (`met`) the episode's end
    /// and the spare's release.
    pub fn reserve_pass(met: bool) {
        crate::warmres::tick(crate::purger::tick());
        if met {
            crate::warmres::refilled();
            crate::warmres::trim_spare(crate::purger::tick());
        }
    }

    /// Hold the claimer's fills (true), or let it run and wake it (false).
    pub fn claimer_hold(on: bool) {
        crate::claimer::HOLD.store(on, core::sync::atomic::Ordering::Relaxed);
        if !on {
            crate::claimer::unpark();
        }
    }

    /// An R1 ask landing (`bytes` > 0) or served (0), with no purger pass: the claimer's landing hold.
    pub fn set_landing(bytes: usize) {
        crate::purger::set_landing(bytes)
    }

    pub fn claimer_met() -> bool {
        crate::claimer::met()
    }

    /// The claimer's room now: the ladder's last `set_claim_room` less the fills since (usize::MAX: no gate).
    pub fn claim_room_left() -> usize {
        crate::claimer::room()
    }

    /// `steps` idle windows passed with no miss (the purger's decay, on a clock far past the last miss).
    pub fn claimer_idle(steps: u32) {
        static LAST: crate::sync::AtomicU32 = crate::sync::AtomicU32::new(0);
        let base = crate::purger::tick().wrapping_add(1 << 24);
        for i in 0..steps {
            crate::claimer::decay(base.wrapping_add(i << 24), 1 << 23, &LAST);
        }
    }
}

#[cfg(loom)]
#[doc(hidden)]
pub mod loom_api {
    pub use crate::central::{Atom64, Nodes, Stack, NONE};
    pub use crate::state::{give_free, take_free, FreeRuns};
    #[cfg(feature = "global")]
    pub use crate::small::{ab_pop, ab_take_all, remote_push, remote_push_chain, remote_take, Links, Spin};
}

mod sync;
