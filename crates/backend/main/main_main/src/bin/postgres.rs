// mimalloc is a C allocator that does not build for wasm32; Rust's
// wasm32-wasip1 std defaults to dlmalloc, so the wasm arm simply takes the
// std default allocator and the release hook stays unset (mcx release is a
// no-op without a hook). Native arm unchanged.
//
// Debug builds wrap the allocator in an env-gated leak tracker (FPBUDGET-1
// instrumentation, `PGRUST_ALLOC_TRACK=1`): allocations made on backend
// threads are recorded with a frame-pointer backtrace and surviving records
// are dumped at process exit for offline symbolication (atos). Release/dist
// builds compile the bare allocator — zero added cost.
//
// Both native arms sit under `init_small::oom::Rescue`: a failed allocation releases the emergency reserve and is
// retried once, so one failed small allocation under memory pressure ends its statement with out-of-memory
// instead of aborting every session (init_small oom.rs).
#[cfg(all(not(target_family = "wasm"), not(debug_assertions), not(feature = "pagepool-global")))]
#[global_allocator]
static GLOBAL: init_small::oom::Rescue<mimalloc::MiMalloc> = init_small::oom::Rescue(mimalloc::MiMalloc);

#[cfg(all(not(target_family = "wasm"), debug_assertions, not(feature = "pagepool-global")))]
#[global_allocator]
static GLOBAL: init_small::oom::Rescue<alloc_track::Tracker> = init_small::oom::Rescue(alloc_track::Tracker(mimalloc::MiMalloc));

// P6 (`pagepool-global`, default off): the pool's allocator; the pool starts at the first allocation.
#[cfg(all(not(target_family = "wasm"), feature = "pagepool-global"))]
#[global_allocator]
static GLOBAL: init_small::oom::Rescue<pagepool::PoolAlloc> = init_small::oom::Rescue(pagepool::PoolAlloc);
// The user 2026-10-18 ("for now can we run mimalloc on x86 and hearth on graviton?"; x86_64 too since D4872): with
// PGRUST_ALLOCATOR unset, Hearth on Linux aarch64 and x86_64, mimalloc elsewhere (a constructor at .init_array.00050:
// before `tune_allocator` below, whose env reads are the first allocation).
#[cfg(all(not(target_family = "wasm"), feature = "pagepool-global"))]
pagepool::by_arch_default!();


/// D4682: the calling thread's retained heap: over its default theap's pages, the committed bytes less the live blocks'
/// (mimalloc's area visit, no block walk). The cache arena's heap is the cache ladder's and is not counted here.
#[cfg(not(target_family = "wasm"))]
fn thread_retained_bytes() -> usize {
    unsafe extern "C" {
        fn mi_theap_get_default() -> *mut libmimalloc_sys::mi_theap_t;
        fn mi_theap_visit_blocks(
            theap: *const libmimalloc_sys::mi_theap_t,
            visit_blocks: bool,
            visitor: libmimalloc_sys::mi_block_visit_fun,
            arg: *mut core::ffi::c_void,
        ) -> bool;
    }
    unsafe extern "C" fn area(
        _heap: *const libmimalloc_sys::mi_heap_t,
        area: *const libmimalloc_sys::mi_heap_area_t,
        _block: *mut core::ffi::c_void,
        _block_size: usize,
        arg: *mut core::ffi::c_void,
    ) -> bool {
        // SAFETY: mimalloc passes a valid area for the visit's duration; `arg` is the caller's usize.
        let (a, sum) = unsafe { (&*area, &mut *arg.cast::<usize>()) };
        *sum += a.committed.saturating_sub(a.used.saturating_mul(a.full_block_size));
        true
    }
    let mut sum = 0usize;
    // SAFETY: the calling thread's own default theap, visited on that thread (as mimalloc's own collect does); areas
    // only (visit_blocks false), `sum` outlives the visit.
    unsafe { mi_theap_visit_blocks(mi_theap_get_default(), false, Some(area), (&raw mut sum).cast()) };
    sum
}

#[cfg(all(not(target_family = "wasm"), debug_assertions))]
mod alloc_track {
    use std::alloc::{GlobalAlloc, Layout};
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering::Relaxed};
    use std::sync::Mutex;

    pub static ENABLED: AtomicBool = AtomicBool::new(false);
    // PGRUST_ALLOC_TRACK=all: record every thread, not just pg:backend*
    // (runtime pool / lane workers allocate on their own threads).
    pub static ALL_THREADS: AtomicBool = AtomicBool::new(false);
    // SIGWINCH (PGRUST_ALLOC_TRACK=...,dump) requests a mid-run dump of the
    // live set; the next tracked allocation performs it under the lock, so
    // the query state is still live (an exit-time dump only sees what
    // survives the exiting thread's unwind).
    pub static DUMP_REQ: AtomicBool = AtomicBool::new(false);
    pub extern "C" fn request_dump(_sig: libc::c_int) {
        DUMP_REQ.store(true, Relaxed);
    }
    // PGRUST_ALLOC_TRACK=...,dumpfile=<path>: creating <path> requests a dump
    // (SIGWINCH does not fire in the supervised server). unlink(2) is the
    // test-and-clear, polled every 4096 tracked allocations.
    pub static DUMP_FILE: AtomicPtr<libc::c_char> = AtomicPtr::new(std::ptr::null_mut());
    static DUMP_POLL: AtomicUsize = AtomicUsize::new(0);

    fn dump_requested() -> bool {
        if DUMP_REQ.swap(false, Relaxed) {
            return true;
        }
        let f = DUMP_FILE.load(Relaxed);
        // SAFETY: f is a leaked NUL-terminated CString set once at boot.
        !f.is_null() && DUMP_POLL.fetch_add(1, Relaxed) & 0xfff == 0 && unsafe { libc::unlink(f) } == 0
    }

    const BT_DEPTH: usize = 28;

    struct Rec {
        tname: &'static str,
        size: usize,
        bt: [usize; BT_DEPTH],
        n: usize,
    }

    static LIVE: Mutex<Option<HashMap<usize, Rec>>> = Mutex::new(None);

    thread_local! {
        static IN_HOOK: Cell<bool> = const { Cell::new(false) };
        // Resolved once per thread: only backend threads are tracked.
        // (tracked: 0 unknown, 1 no, 2 yes; name: interned on first use). One
        // slot: the session TLS census is pinned, so no second thread_local.
        static TRACKED: Cell<(u8, Option<&'static str>)> = const { Cell::new((0, None)) };
    }

    fn thread_name() -> &'static str {
        TRACKED.with(|t| {
            let (tracked, name) = t.get();
            if let Some(n) = name {
                return n;
            }
            let n: &'static str = Box::leak(
                std::thread::current().name().unwrap_or("<unnamed>").to_owned().into_boxed_str(),
            );
            t.set((tracked, Some(n)));
            n
        })
    }

    #[inline]
    fn thread_tracked() -> bool {
        match TRACKED.get() {
            (2, _) => true,
            (1, _) => false,
            (_, name) => {
                let is_backend = ALL_THREADS.load(Relaxed)
                    || std::thread::current()
                        .name()
                        .is_some_and(|n| n.starts_with("pg:backend"));
                TRACKED.set((if is_backend { 2 } else { 1 }, name));
                is_backend
            }
        }
    }

    // aarch64 frame-pointer walk (debug builds keep x29 chains intact).
    #[cfg(target_arch = "aarch64")]
    unsafe fn backtrace_fp(buf: &mut [usize; BT_DEPTH]) -> usize {
        let mut fp: usize;
        core::arch::asm!("mov {}, x29", out(reg) fp);
        let mut n = 0;
        let mut prev = 0usize;
        while n < BT_DEPTH
            && fp > prev
            && fp % 8 == 0
            && (prev == 0 || fp - prev < (64 << 20))
        {
            let lr = *((fp + 8) as *const usize);
            if lr < 0x1_0000 {
                break;
            }
            buf[n] = lr;
            n += 1;
            prev = fp;
            fp = *(fp as *const usize);
        }
        n
    }

    // x86_64 frame-pointer walk (rbp chain; Apple targets keep frame
    // pointers, and debug builds elsewhere do too): same guards as above.
    #[cfg(target_arch = "x86_64")]
    unsafe fn backtrace_fp(buf: &mut [usize; BT_DEPTH]) -> usize {
        let mut fp: usize;
        core::arch::asm!("mov {}, rbp", out(reg) fp);
        let mut n = 0;
        let mut prev = 0usize;
        while n < BT_DEPTH
            && fp > prev
            && fp % 8 == 0
            && (prev == 0 || fp - prev < (64 << 20))
        {
            let lr = *((fp + 8) as *const usize);
            if lr < 0x1_0000 {
                break;
            }
            buf[n] = lr;
            n += 1;
            prev = fp;
            fp = *(fp as *const usize);
        }
        n
    }

    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    unsafe fn backtrace_fp(_buf: &mut [usize; BT_DEPTH]) -> usize {
        0
    }

    fn record_alloc(p: *mut u8, l: Layout) {
        if p.is_null() || !ENABLED.load(Relaxed) {
            return;
        }
        IN_HOOK.with(|h| {
            if h.get() {
                return;
            }
            h.set(true);
            if thread_tracked() {
                let mut bt = [0usize; BT_DEPTH];
                // SAFETY: fp walk with monotonic + alignment guards.
                let n = unsafe { backtrace_fp(&mut bt) };
                let mut g = LIVE.lock().unwrap_or_else(|e| e.into_inner());
                g.get_or_insert_with(HashMap::new)
                    .insert(p as usize, Rec { tname: thread_name(), size: l.size(), bt, n });
                if dump_requested() {
                    if let Some(m) = g.as_ref() {
                        dump_map(m, "mid-run");
                    }
                }
            }
            h.set(false);
        });
    }

    fn record_free(p: *mut u8) {
        if p.is_null() || !ENABLED.load(Relaxed) {
            return;
        }
        IN_HOOK.with(|h| {
            if h.get() {
                return;
            }
            h.set(true);
            let mut g = LIVE.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(m) = g.as_mut() {
                m.remove(&(p as usize));
            }
            h.set(false);
        });
    }

    pub extern "C" fn dump() {
        ENABLED.store(false, Relaxed);
        let g = LIVE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(m) = g.as_ref() else { return };
        dump_map(m, "exit");
    }

    fn dump_map(m: &HashMap<usize, Rec>, when: &str) {
        // Group by backtrace.
        // Keyed by backtrace alone: a leak site that runs once per session
        // shows up as one row whose count grows per session, not as one row
        // per dead backend thread (each session runs on its own thread).
        let mut groups: HashMap<(&'static str, &[usize]), (usize, usize)> = HashMap::new();
        for r in m.values() {
            let e = groups.entry(("*", &r.bt[..r.n])).or_insert((0, 0));
            e.0 += 1;
            e.1 += r.size;
        }
        let mut rows: Vec<(&'static str, &[usize], usize, usize)> =
            groups.into_iter().map(|((t, k), (c, b))| (t, k, c, b)).collect();
        rows.sort_by(|a, b| b.3.cmp(&a.3));
        let mut per_thread: HashMap<&'static str, usize> = HashMap::new();
        for r in m.values() {
            *per_thread.entry(r.tname).or_insert(0) += r.size;
        }
        let mut pt: Vec<_> = per_thread.into_iter().collect();
        pt.sort_by(|a, b| b.1.cmp(&a.1));
        for (t, b) in pt.iter().take(8) {
            eprintln!("ALLOC-TRACK thread: {t} live_bytes={b}");
        }
        let slide = unsafe { slide0() };
        eprintln!(
            "ALLOC-TRACK dump ({when}): {} live tracked blocks, {} bytes, slide=0x{:x}",
            m.len(),
            m.values().map(|r| r.size).sum::<usize>(),
            slide,
        );
        for (tname, bt, count, bytes) in rows.iter().take(200) {
            let addrs: Vec<String> = bt.iter().map(|a| format!("0x{a:x}")).collect();
            eprintln!("ALLOC-TRACK leak: thread={} n={} bytes={} bt={}", tname, count, bytes, addrs.join(" "));
        }
    }

    // ASLR slide for offline symbolication (atos) — a macOS dyld concept.
    // _dyld_get_image_vmaddr_slide does not exist on Linux (undefined
    // reference at link time on the CI cluster — t29 fold find); the Linux arm
    // reports 0 and symbolication uses /proc/<pid>/maps offsets instead.
    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        fn _dyld_get_image_vmaddr_slide(image_index: u32) -> isize;
    }

    #[cfg(target_os = "macos")]
    unsafe fn slide0() -> isize {
        unsafe { _dyld_get_image_vmaddr_slide(0) }
    }

    #[cfg(not(target_os = "macos"))]
    unsafe fn slide0() -> isize {
        0
    }

    pub struct Tracker(pub mimalloc::MiMalloc);

    unsafe impl GlobalAlloc for Tracker {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            let p = unsafe { self.0.alloc(l) };
            record_alloc(p, l);
            p
        }
        unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
            let p = unsafe { self.0.alloc_zeroed(l) };
            record_alloc(p, l);
            p
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            record_free(p);
            unsafe { self.0.dealloc(p, l) };
        }
        unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
            record_free(p);
            let np = unsafe { self.0.realloc(p, l, new_size) };
            let nl = Layout::from_size_align(new_size, l.align()).unwrap_or(l);
            record_alloc(np, nl);
            np
        }
    }
}

// Antithesis harness builds only: keep the coverage runtime linked. The
// crate does nothing without the sancov rustflags the harness image build
// supplies (root Dockerfile, ARG ANTITHESIS=1) — crate and flags are one
// unit. Target-scoped to the platform Antithesis runs on.
#[cfg(all(feature = "antithesis", target_os = "linux", target_arch = "x86_64"))]
use antithesis_instrumentation as _;

/// What `PGRUST_MEMPOLICY`'s text (`<knob>=<int>` entries, comma separated, blanks around each part allowed) sets `knob`
/// to: `Some(non-zero)`, the last entry naming it with an integer; `None` when no entry does (the knob's default holds).
/// No allocation (the allocator's start reads it before the first block).
#[cfg(not(target_family = "wasm"))]
fn mempolicy_knob_on(policy: &[u8], knob: &[u8]) -> Option<bool> {
    policy.split(|&b| b == b',').filter_map(|entry| {
        let mut kv = entry.splitn(2, |&b| b == b'=');
        let (Some(k), Some(v)) = (kv.next(), kv.next()) else { return None };
        let (k, v) = (k.trim_ascii(), v.trim_ascii());
        (k == knob && !v.is_empty() && v.iter().all(u8::is_ascii_digit)).then(|| v.iter().any(|&b| b != b'0'))
    }).last()
}

/// `mempolicy_knob_on`'s integer: the last entry naming `knob` with an integer (pagepool P2: the pool's knobs, read at the
/// allocator's start). No allocation.
#[cfg(not(target_family = "wasm"))]
fn mempolicy_knob_u64(policy: &[u8], knob: &[u8]) -> Option<u64> {
    policy.split(|&b| b == b',').filter_map(|entry| {
        let mut kv = entry.splitn(2, |&b| b == b'=');
        let (Some(k), Some(v)) = (kv.next(), kv.next()) else { return None };
        let (k, v) = (k.trim_ascii(), v.trim_ascii());
        if k != knob || v.is_empty() || !v.iter().all(u8::is_ascii_digit) {
            return None;
        }
        v.iter().try_fold(0u64, |n, &d| n.checked_mul(10)?.checked_add(u64::from(d - b'0')))
    }).last()
}

// Opt-in allocator settings (lane memfree, memresearch (c)), PGRUST_ALLOC_TUNE=1:
// purge freed pages at once (purge_delay=0) and neither commit nor reserve a
// 1 GiB arena up front (arena_eager_commit=0, arena_reserve=0). They cut the
// idle server from ~290 to ~128 MB and a 5M-int sort's kept RSS from +484 to
// +156 MB, but each of them alone costs short analytic statements up to
// 1.9x at W8 (two top-N queries, box 34, 2026-10-06): a refault per statement. So the
// default keeps mimalloc's own and frees memory at idle instead
// (pgrust.memory_release_idle). They must be set before mimalloc's first
// arena, so on Linux they run from an .init_array constructor ordered before
// mimalloc's own (constructor(101)). An explicit MIMALLOC_<NAME> variable
// still wins over each. getenv(3), not std::env: nothing here may allocate
// before the options are set.
#[cfg(not(target_family = "wasm"))]
extern "C" fn tune_allocator() {
    // mimalloc v3 mi_option_e (libmimalloc-sys exports no constants for these).
    const ARENA_EAGER_COMMIT: libmimalloc_sys::mi_option_t = 4;
    const PURGE_DELAY: libmimalloc_sys::mi_option_t = 15;
    const ARENA_RESERVE: libmimalloc_sys::mi_option_t = 23;
    // SAFETY: getenv with NUL-terminated literals, before any thread exists.
    let set = |name: &core::ffi::CStr| unsafe { !libc::getenv(name.as_ptr()).is_null() };
    // SAFETY: as above; the value is a NUL-terminated C string when non-null.
    let is_1 = |name: &core::ffi::CStr| unsafe {
        let v = libc::getenv(name.as_ptr());
        !v.is_null() && core::ffi::CStr::from_ptr(v).to_bytes() == b"1"
    };
    // ALLOC-LADDER (c) / D4321 step 1: pgrust's own arenas (general + the cache arena) with the counting commit
    // callback, before mimalloc's first arena (PGRUST_ALLOC_ARENAS=0 keeps mimalloc's own)
    let _ = allocarena::install();
    // REUSE BEFORE RELEASE (default off): general heaps take the cache arena's committed free slices
    // before fresh ones of their own arenas
    if is_1(c"PGRUST_REUSE_BEFORE_RELEASE") {
        allocarena::set_reuse(true);
    }
    // PRESSURE ONLY (default off): mimalloc never purges on a timer or a collect (purge delay -1); freed pages go back to
    // the OS only under ladder pressure, from the allocator releases (mcx::release_retained) and the purger if it runs
    if is_1(c"PGRUST_PURGE_PRESSURE_ONLY") {
        allocarena::set_purge_pressure_only();
        mcx::set_pressure_only(true);
    }
    // IDLE TRIM UNDER PRESSURE ONLY (default off): the idle release keeps the parked regions and slot sets unless ladder
    // pressure holds
    if is_1(c"PGRUST_IDLE_TRIM_PRESSURE_ONLY") {
        mcx::set_idle_trim_pressure_only(true);
    }
    // THE PURGER PURGES UNDER PRESSURE (default off; with pressure only and C3's purger): a thread
    // meeting pressure wakes the purger (inline only past the watchdog's cancel line); the purger trims the parked regions
    // then purges
    if is_1(c"PGRUST_PURGE_PRESSURE_BG") {
        mcx::set_purge_bg(true);
    }
    // REUSE BEFORE RELEASE sizing (diagnostic, default off): the accounts view's cache_run_* rows, the cache arena's free
    // runs by length
    if is_1(c"PGRUST_REUSE_RUNS") {
        mcx::set_cache_runs(allocarena::cache_free_runs);
    }
    let on = is_1(c"PGRUST_ALLOC_TUNE");
    // D3243 diagnostic (PGRUST_ALLOC_COUNT=1): the global allocator counts its live blocks at mimalloc's usable size from
    // its first block on, for the census / watchdog split of RSS into live blocks and the rest. A relaxed load per call
    // when off; on, an atomic add on the thread's counter shard and an mi_usable_size per block.
    // pphatch: PoolAlloc's arms only while it serves (under the hatch, mimalloc's, as without the feature)
    let pool = main_main::pool_allocates();
    if is_1(c"PGRUST_ALLOC_COUNT") {
        #[cfg(feature = "pagepool-global")]
        if pool {
            init_small::oom::count_allocations(true, Some(|p| pagepool::usable_size(p)));
        }
        if !pool {
            // SAFETY: a live block of the global allocator (mimalloc under the wrapper): mi_usable_size's contract.
            init_small::oom::count_allocations(true, Some(|p| unsafe { libmimalloc_sys::mi_usable_size(p as *const core::ffi::c_void) }));
        }
    }
    // D4556 (the memory policy's charge_usable knob, on by default; PGRUST_MEMPOLICY=charge_usable=0 turns it off): door 1 charges each block's rounding
    // (its usable size less the layout's) to the rounding account, so the accounts hold what the allocator holds
    // (mi_good_size disagrees with the block's own size on ~42 % of blocks: the block is read). Read here, before the first
    // block, from the same variable the policy's start validates
    // SAFETY: as above.
    let policy = unsafe {
        let v = libc::getenv(c"PGRUST_MEMPOLICY".as_ptr());
        if v.is_null() { &[][..] } else { core::ffi::CStr::from_ptr(v).to_bytes() }
    };
    let default = mempolicy::consts::KNOBS[mempolicy::consts::K::ChargeUsable as usize].default != 0;
    if mempolicy_knob_on(policy, b"charge_usable").unwrap_or(default) {
        // P6: PoolAlloc hands out exactly its size for the layout: door 1's rounding reads no block
        #[cfg(feature = "pagepool-global")]
        if pool {
            init_small::oom::charge_usable_layout(true, pagepool::good_size);
        }
        if !pool {
            // SAFETY: as above.
            init_small::oom::charge_usable(true, |p| unsafe { libmimalloc_sys::mi_usable_size(p as *const core::ffi::c_void) });
        }
    }
    // pagepool: the reservation only (no thread, no heap); PGRUST_POOL_* unset, nothing. Its knobs from PGRUST_MEMPOLICY
    // first: the policy's start (the postmaster's) applies the text after this, too late for the pool's sizes and mcx's
    // block_min (the same values again there)
    if pagepool::switches().any() {
        use mempolicy::consts::{set, K, KNOBS};
        for k in [
            K::PoolVaMul,
            K::PoolChunk,
            K::PoolShelfMax,
            K::PoolWarmZero,
            K::PoolWarmZeroMax,
            K::PoolWarmCap,
            K::PoolDecayMs,
            K::PoolPurgeBatch,
            K::PoolLazyMin,
            K::PoolIdleDrainMs,
            K::PoolBlockMin,
            K::PoolLazyVaMul,
            K::PoolReleaseMode,
            K::PoolLazyDecayMs,
            K::PoolLazyCap,
            K::PoolAgePassMax,
            K::PoolThpPartial,
            K::PoolHostAgeMs,
        ] {
            if let Some(v) = mempolicy_knob_u64(policy, KNOBS[k as usize].name.as_bytes()) {
                set(k, v);
            }
        }
        let _ = pagepool::init(&pagepool::Config::from_policy());
    }
    if !on {
        return;
    }
    for (option, env, value) in [
        (PURGE_DELAY, c"MIMALLOC_PURGE_DELAY", 0),
        (ARENA_EAGER_COMMIT, c"MIMALLOC_ARENA_EAGER_COMMIT", 0),
        (ARENA_RESERVE, c"MIMALLOC_ARENA_RESERVE", 0),
    ] {
        if !set(env) {
            // SAFETY: plain option store; valid option ids for the linked v3.
            unsafe { libmimalloc_sys::mi_option_set(option, value) };
        }
    }
}

// Runs before mimalloc's constructor(101): .init_array.<prio> sorts ascending.
#[cfg(all(target_os = "linux", not(target_family = "wasm")))]
#[used]
#[unsafe(link_section = ".init_array.00100")]
static TUNE_ALLOCATOR: extern "C" fn() = tune_allocator;

fn main() {
    // Elsewhere (macOS) the constructor above is not ordered: set them first thing.
    #[cfg(all(not(target_os = "linux"), not(target_family = "wasm")))]
    tune_allocator();
    // Antithesis harness builds: initialize the SDK before anything else so
    // assertion cataloging and lifecycle output are wired, then emit the
    // bootstrap reachability property from the one path every run executes.
    #[cfg(feature = "antithesis")]
    {
        antithesis_sdk::antithesis_init();
        antithesis_sdk::assert_reachable!("pgrust: server process entered main");
        // ParkLot steering probe: the parked thread reads its own
        // thread-local interrupt-pending flag, giving per-backend semantics
        // without a pgsync→backend dependency.
        pgsync::set_park_interrupt_probe(init_small::globals::InterruptPending);
    }
    // ipc::proc_exit ends the process by unwinding a ProcExitThread payload
    // rather than calling exit(2) — a thread must never _exit the shared
    // process. Nothing used to catch it here, so Rust's default panic path
    // turned even a CLEAN shutdown into exit code 101: `docker stop` and any
    // k8s liveness/restart policy read that as a crash. Carry the intended
    // code through instead. Real panics are re-raised untouched.
    // unwind-ok: log-then-die
    match std::panic::catch_unwind(run) {
        Ok(()) => {}
        Err(payload) => {
            if let Some(p) = payload.downcast_ref::<::ipc::ProcExitThread>() {
                std::process::exit(p.code);
            }
            std::panic::resume_unwind(payload);
        }
    }
}

/// GL-CONCMEM-1: the POOL-QOS memory governor's fallback usage basis for hosts without /proc (the primary basis is
/// RssAnon): the allocator's process RSS, the same cheap mi_process_info read as the watchdog's.
#[cfg(not(target_family = "wasm"))]
fn qos_mem_rss() -> usize {
    let mut rss = 0usize;
    // SAFETY: nullable out-params per the mi_process_info contract.
    unsafe {
        libmimalloc_sys::mi_process_info(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut rss,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
    }
    rss
}

/// pagepool P2: mcx's block source over the page pool, and the allocator hooks that see the pool.
#[cfg(not(target_family = "wasm"))]
mod pool_mcx {
    use core::ptr::{null_mut, NonNull};
    use mcx::heap::{BlockSource, Lifetime};
    use pagepool::{Class, Hint, Tag, Zero};

    #[inline]
    fn class(life: Lifetime) -> Class {
        match life {
            Lifetime::Task => Class::Task,
            Lifetime::Session => Class::Session,
            Lifetime::Cache => Class::Cache,
            Lifetime::Process => Class::Process,
        }
    }

    unsafe fn take(bytes: usize, align: usize, life: Lifetime, zeroed: bool) -> *mut u8 {
        let hint = Hint::new(class(life), Tag::MCX).zero(if zeroed { Zero::Dense } else { Zero::No });
        pagepool::take(bytes, align, hint).map_or(null_mut(), |b| b.as_ptr().cast())
    }

    /// The take's class known (its context took every pooled block in one): `give_hinted`, no run lookup; else `give`.
    unsafe fn give(p: *mut u8, bytes: usize, align: usize, life: Option<Lifetime>) {
        // SAFETY: BlockSource's contract: `p` is a live block of `bytes` at `align` this source served (never null), and
        // a some `life` is the class it was taken in, with Tag::MCX as `take` names.
        unsafe {
            let p = NonNull::new_unchecked(p);
            match life {
                Some(l) => pagepool::give_hinted(p, bytes, align, Hint::new(class(l), Tag::MCX)),
                None => pagepool::give(p, bytes),
            }
        }
    }

    unsafe fn resize(p: *mut u8, old: usize, new: usize, align: usize) -> *mut u8 {
        // SAFETY: as `give`, the block of `old` bytes.
        unsafe { pagepool::resize(NonNull::new_unchecked(p), old, new, align) }.map_or(null_mut(), |b| b.as_ptr().cast())
    }

    fn release() {
        pagepool::flush_shelf();
        allocarena::collect_cache();
        // SAFETY: mi_collect takes no pointer.
        unsafe { libmimalloc_sys::mi_collect(true) }
    }

    fn in_use() -> Option<usize> {
        init_small::oom::allocated_bytes().map(|b| b.saturating_add(memledger::pagepool_total().max(0) as usize))
    }

    fn committed() -> usize {
        let mut commit = 0usize;
        // SAFETY: nullable out-params per the mi_process_info contract.
        unsafe {
            libmimalloc_sys::mi_process_info(
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                &mut commit,
                null_mut(),
                null_mut(),
            );
        }
        commit.saturating_add(present_or_zero())
    }

    /// The pool's term in the allocator's committed bytes (the governor's probe, the watchdog's `alloc commit`): its
    /// resident bytes as the purger last published them (`pagepool::present`: two atomic loads, never a sweep; 0 with the
    /// pool off or before its first sweep). Counted once: the pool's zones are its own mappings, apart from every mimalloc
    /// arena, and its claimed bytes are no longer added (a Cold take or a lazy span's commit counts there unfaulted).
    /// ACCOUNTING ONLY: it can read low while the pool grows (a fault shows at the next sweep), so no kill-safety decision
    /// reads it; the watchdog's cancel line and the ladder's past-cancel checks keep the resident probe (cgroup / RSS).
    pub(super) fn present_or_zero() -> usize {
        pagepool::present().unwrap_or(0)
    }

    fn reading() -> memledger::PoolReading {
        let s = pagepool::stats();
        memledger::PoolReading { claimed: s.claimed as u64, warm_free: s.warm_free() as u64, shelved: s.shelved as u64, stranded: s.stranded as u64 }
    }

    /// pppress card 9: the pool's bytes door 1 charged (`PoolAlloc`'s blocks at their good size; 0 with it off or not
    /// built in), owned bytes the ledger's pool terms leave out of `pool_unowned`.
    fn door1() -> u64 {
        #[cfg(feature = "pagepool-global")]
        if pagepool::global_on() {
            return pagepool::global_live() as u64;
        }
        0
    }

    static SOURCE: BlockSource = BlockSource { take, give, resize, usable: pagepool::usable };

    /// The pool on, whichever switch turned it on: the ledger sees its claims (the pool door), the views its reading, the
    /// governor its bytes beside the allocator's. A consumer that is not mcx (PGRUST_POOL_GRAVITY=1 alone) claims pool RAM
    /// too, so these never wait for mcx's switch.
    pub(super) fn ledger() {
        pagepool::set_door(memledger::door_pool);
        memledger::set_pool_probe(reading);
        memledger::set_pool_door1(door1);
        mcx::governor::set_allocator_probes(in_use, committed);
    }

    /// PGRUST_POOL_MCX=1, before any session thread: mcx's blocks from the pool, its release over the allocator's.
    /// PGRUST_POOL_KEEPERS=1: keeper blocks come from the pool too (unset: parked as today). The pool's threads start in
    /// the server process (memruntime::start), after the crash supervisor's fork.
    pub(super) fn arm() {
        let (lo, len) = pagepool::range();
        let min = pagepool::Config::from_policy().block_min;
        if pagepool::global_on() {
            mcx::heap::set_block_owner(pagepool::taken);
        }
        mcx::heap::set_block_source(&SOURCE, lo, len, min, !pagepool::switches().keepers);
        mcx::set_allocator_release(release);
    }
}

/// pagepool P2: which of the pool's hooks run() installs, (the ledger's, mcx's): the ledger's whenever the pool is on, mcx's
/// block source and release only under PGRUST_POOL_MCX=1.
#[cfg(not(target_family = "wasm"))]
fn pool_hooks(on: bool, mcx: bool) -> (bool, bool) {
    (on, on && mcx)
}

fn run() {
    // small heaps on base pages, large blocks on huge pages (before any session thread exists)
    main_main::heap_thp_policy();
    // Transport provider resolution (§2.4 seam): one argv peek, once, before
    // any seam install — set-once at boot is the house pattern. Everything
    // but `--stdio-wire` / `--sim-net` (all the C dispatch options included)
    // boots the socket provider, i.e. the unchanged native byte path.
    let arg1 = std::env::args_os().nth(1).map(|a| a.to_string_lossy().into_owned());
    // --help / -? / --version / -V print their text and nothing else: the instruments and the QoS memory governor
    // (whose armed bar is a LOG line on stderr) start only for a real run.
    let info_only = matches!(arg1.as_deref(), Some("--help" | "-?" | "--version" | "-V"));
    let transport = match arg1.as_deref() {
        Some("--stdio-wire") => seams_init::Transport::StdioWire,
        #[cfg(pgrust_sim)]
        Some("--sim-net") => seams_init::Transport::SimNet,
        _ => seams_init::Transport::Socket,
    };
    seams_init::init_all_with_transport(transport);
    // Gravity spills need the session spill files in every mode; --single, --stdio-wire and the wasm build start no
    // runtime pool, whose RuntimePool::new was the only install (now a no-op second set).
    let _ = gravity_pool::install_spills();
    // pphatch: the allocator chosen at the first allocation (PGRUST_ALLOCATOR, else the architecture's default), for
    // `pgrust.allocator` and the arms below
    guc_tables::backing::set_pgrust_allocator_name(main_main::allocator_name());
    #[cfg(not(target_family = "wasm"))]
    let pool = main_main::pool_allocates();
    // the emergency allocation reserve, taken before the first connection (none while PoolAlloc serves)
    #[cfg(all(not(target_family = "wasm"), feature = "pagepool-global"))]
    if !pool {
        init_small::oom::allow_reserve();
    }
    #[cfg(not(target_family = "wasm"))]
    init_small::oom::arm_reserve();
    // mi_collect(force) releases mimalloc's freed-but-retained segments;
    // called only at alloc-churn boundaries (hashagg spill batch resets)
    // where retention would otherwise hold batch-sized RSS for the whole
    // spill pass.
    // LADDER-DESIGN 6a: the calling thread's part of the cache heap too (its pages are freed only on this thread); H3 step
    // (2) (give_back_service): the give-back service's arenas get their forced purge as its job (past the watchdog's
    // cancel line, on the caller as before)
    #[cfg(not(target_family = "wasm"))]
    if !pool {
        mcx::set_allocator_release(allocarena::release);
    }
    // P6 (PoolAlloc serving): first this thread's idle runs and its shelf back to the shared pool (warm, nothing released
    // to the OS), then the same release as above
    #[cfg(all(not(target_family = "wasm"), feature = "pagepool-global"))]
    if pool {
        mcx::set_allocator_release(|| {
            pagepool::release_heap();
            pagepool::flush_shelf();
            allocarena::release();
        });
    }
    #[cfg(not(target_family = "wasm"))]
    allocarena::set_cancel_line_probe(mcx::ladder::past_cancel_line);
    // pagepool P0 (C.9): PGRUST_EXIT_PURGE=0 skips that purge at session end (exit, park, retire, init-file load),
    // read once here before any session thread; unset or any other value = today's purge
    #[cfg(not(target_family = "wasm"))]
    if std::env::var_os("PGRUST_EXIT_PURGE").is_some_and(|v| v == "0") {
        mcx::set_exit_purge(false);
    }
    // D4682 (Broker flip condition 4): a parking client backend's retained heap, read from its own default theap's page
    // areas, so cpool lets a thread over its bound exit instead of parking
    #[cfg(not(target_family = "wasm"))]
    if !pool {
        mcx::set_thread_retained(thread_retained_bytes);
    }
    #[cfg(all(not(target_family = "wasm"), feature = "pagepool-global"))]
    if pool {
        mcx::set_thread_retained(pagepool::thread_retained);
    }
    // ALLOC-LADDER (b): a thread answering the ladder's collect ask frees its own empty pages; the arena purge stays the
    // ladder thread's (mimalloc's purge guard is a try: concurrent purgers skip)
    #[cfg(not(target_family = "wasm"))]
    mcx::set_own_collect(|| {
        #[cfg(feature = "pagepool-global")]
        if main_main::pool_allocates() {
            pagepool::release_heap();
            pagepool::flush_shelf();
        }
        allocarena::collect_cache_local();
        unsafe { libmimalloc_sys::mi_collect(false) }
    });
    // LADDER-DESIGN 6a: a cache give-back's allocator step (own runs' hot pool, this thread's cache heap)
    #[cfg(not(target_family = "wasm"))]
    mcx::set_cache_give_back(|| {
        let _ = allocarena::give_back();
    });
    // C3 (cache_purge_now): the cache arena's purge, run by memruntime's purger alone (the general arenas keep mimalloc's)
    #[cfg(not(target_family = "wasm"))]
    mcx::set_cache_purge(allocarena::purge_cache_now);
    // C3 (purge owned): with the purger running, only it purges the cache arena (dead-man fallback after 1 s)
    #[cfg(not(target_family = "wasm"))]
    mcx::set_cache_purge_owner(allocarena::set_cache_purge_owned);
    #[cfg(not(target_family = "wasm"))]
    mcx::set_cache_purge_fallbacks(allocarena::purge_owned_fallbacks);
    // C4 (query_purge_bg): with the purger running, only it purges the general arenas too (expired ranges, its tick)
    #[cfg(not(target_family = "wasm"))]
    mcx::set_general_purge_owner(allocarena::set_general_purge_owned);
    #[cfg(not(target_family = "wasm"))]
    mcx::set_general_purge(allocarena::purge_general_expired);
    // ladder pressure's purge: every free and committed run decommitted now, whatever the purge delay
    #[cfg(not(target_family = "wasm"))]
    mcx::set_pressure_purge(allocarena::purge_unused);
    // the purger's hold (PGRUST_PURGE_AFTER_IDLE_S above 1): owned arenas purged after an idle stretch; purged bytes by thread
    #[cfg(not(target_family = "wasm"))]
    mcx::set_idle_purge(allocarena::purge_after_idle);
    #[cfg(not(target_family = "wasm"))]
    mcx::set_purged_bytes(allocarena::purged_bytes);
    // purge timing: each purge event's time by thread (the purger, a statement, after a ReadyForQuery, other)
    #[cfg(not(target_family = "wasm"))]
    mcx::set_purge_timing(allocarena::set_purge_phase, allocarena::purge_times);
    // REUSE BEFORE RELEASE: its counters for the broker view
    #[cfg(not(target_family = "wasm"))]
    mcx::set_reuse_stats(allocarena::reuse_stats);
    #[cfg(not(target_family = "wasm"))]
    mcx::set_alloc_sources(allocarena::alloc_sources);
    // REUSE: an inline reclaim's freed images stay committed for the draw that asked (no purge while they cover it)
    #[cfg(not(target_family = "wasm"))]
    if allocarena::reuse_on() {
        mcx::set_reuse_keep(allocarena::collect_cache_local);
    }
    // REUSE BEFORE RELEASE: the arenas' free committed bytes for the accounts view (information, never headroom)
    #[cfg(not(target_family = "wasm"))]
    if allocarena::reuse_on() {
        mcx::ladder::set_reusable_probe(allocarena::reusable_bytes);
    }
    // D4321 step 1: the cache arena's heap behind the global allocator's cache scopes (memledger::arena::cache_scope).
    // P6: not while PoolAlloc serves (its blocks are mimalloc's): the pool allocator's cache class below
    #[cfg(not(target_family = "wasm"))]
    if !pool {
        init_small::oom::set_cache_alloc(allocarena::cache_alloc, allocarena::cache_realloc);
        // LADDER-DESIGN 6a: the 64-512 KiB images' own runs, recognized at free / realloc by their range
        init_small::oom::set_own_pages(allocarena::own::range(), allocarena::own_free, allocarena::own_resize);
        let _ = allocarena::start_cache_heap();
    }
    // P6: a cache scope's blocks are the pool allocator's cache class
    #[cfg(all(not(target_family = "wasm"), feature = "pagepool-global"))]
    if pool {
        init_small::oom::set_cache_alloc(pagepool::cache_alloc, pagepool::cache_realloc);
        init_small::oom::set_cache_test(pagepool::is_cache);
        memledger::arena::set_cache_heap_ready();
    }
    // GL-MEMWATCH-1: allocator statistics for the memory watchdog's ledger —
    // mi_process_info is a cheap stats read (no heap walk). current_commit is
    // the r/w memory mimalloc has committed; the watchdog logs it beside RSS
    // and the accounted context bytes so allocator retention is separable
    // from truly untracked growth.
    #[cfg(not(target_family = "wasm"))]
    memruntime::set_allocator_stats(|| {
        let (mut rss, mut commit) = (0usize, 0usize);
        // SAFETY: nullable out-params per the mi_process_info contract.
        unsafe {
            libmimalloc_sys::mi_process_info(
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut rss,
                std::ptr::null_mut(),
                &mut commit,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
        }
        // pagepool P2: plus the pool's published resident bytes (0 while it is off; accounting only, see present_or_zero)
        memruntime::AllocatorStats { current_rss: rss, current_commit: commit.saturating_add(pool_mcx::present_or_zero()) }
    });
    // D3243 diagnostic: the allocator's live bytes (None unless PGRUST_ALLOC_COUNT=1 armed the count) and committed bytes,
    // beside the governor's resident measure in the idle census and the watchdog's trip / cancel lines.
    #[cfg(not(target_family = "wasm"))]
    mcx::governor::set_allocator_probes(init_small::oom::allocated_bytes, || {
        let mut commit = 0usize;
        // SAFETY: nullable out-params per the mi_process_info contract.
        unsafe {
            libmimalloc_sys::mi_process_info(
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut commit,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
        }
        commit
    });
    // pagepool P2: the pool's ledger hooks whenever it is on (any PGRUST_POOL_* switch), over the ones above; mcx's blocks
    // from it only under PGRUST_POOL_MCX=1
    #[cfg(not(target_family = "wasm"))]
    {
        let (ledger, arm) = pool_hooks(pagepool::on(), pagepool::switches().mcx);
        if ledger {
            pool_mcx::ledger();
        }
        if arm {
            pool_mcx::arm();
        }
    }
    #[cfg(not(target_family = "wasm"))]
    if !info_only {
        runtime::install_qos_mem_probe(qos_mem_rss);
    }
    // FPBUDGET-1 debug instrument: process-global live-context census
    // (name -> count), dumped at each backend exit. Diagnostic only.
    if !info_only && std::env::var_os("PGRUST_MCXT_CENSUS").is_some()
    {
        mcx::debug_census::enable();
        eprintln!("MCXT-CENSUS: enabled (on={})", mcx::debug_census::on());
    }
    // FPBUDGET-1 debug instrument (debug builds): track backend-thread
    // allocations that survive to process exit, with fp backtraces.
    #[cfg(all(not(target_family = "wasm"), debug_assertions))]
    // PGRUST_ALLOC_TRACK=1 | all [,dump] [,dumpfile=<path>]: `all` records
    // every thread, `dump` installs the SIGWINCH mid-run dump (one env read:
    // the determinism lint's raw-env budget for this file is a ratchet).
    if let Some(v) = std::env::var_os("PGRUST_ALLOC_TRACK") {
        let v = v.to_string_lossy();
        let flag = |f: &str| v.split(',').any(|x| x.trim() == f);
        if flag("all") {
            alloc_track::ALL_THREADS.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(path) = v.split(',').find_map(|x| x.trim().strip_prefix("dumpfile=")) {
            if let Ok(c) = std::ffi::CString::new(path) {
                alloc_track::DUMP_FILE.store(c.into_raw(), std::sync::atomic::Ordering::Relaxed);
            }
        }
        if flag("dump") {
            unsafe {
                libc::signal(libc::SIGWINCH, alloc_track::request_dump as extern "C" fn(libc::c_int) as usize);
            }
        }
        unsafe { libc::atexit(alloc_track::dump) };
        alloc_track::ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
        eprintln!("ALLOC-TRACK: enabled");
    }
    let argv: Vec<String> = main_main::argv_from_os(std::env::args_os());
    if let Err(e) = main_main::pg_main(&argv) {
        elog::emit_unhandled_error_report(&e);
        // errfinish FATAL -> proc_exit(1): the registered exit callbacks run
        // (postmaster.pid is unlinked when shared_preload_libraries fails).
        ::ipc::proc_exit(1, init_small::globals::MyProcPid());
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    /// D4556: the allocator's start reads the charge_usable knob from PGRUST_MEMPOLICY's text: no entry keeps the knob's
    /// default (on), an explicit 0 turns it off, the last entry naming it wins, a malformed value counts as no entry.
    #[test]
    fn the_allocator_start_reads_the_charge_usable_knob() {
        let k = b"charge_usable";
        assert_eq!(super::mempolicy_knob_on(b"", k), None);
        assert_eq!(super::mempolicy_knob_on(b"ledger_home=1", k), None);
        assert_eq!(super::mempolicy_knob_on(b"charge_usable=0", k), Some(false));
        assert_eq!(super::mempolicy_knob_on(b" charge_usable = 1 ,ledger_floor=1", k), Some(true));
        assert_eq!(super::mempolicy_knob_on(b"charge_usable=1,charge_usable=0", k), Some(false));
        assert_eq!(super::mempolicy_knob_on(b"charge_usable=x", k), None);
        assert_eq!(mempolicy::consts::KNOBS[mempolicy::consts::K::ChargeUsable as usize].default, 1, "on by default");
    }

    /// pagepool P2: the allocator's start reads the pool's knobs from PGRUST_MEMPOLICY's text (the policy's start is too
    /// late for the pool's sizes): the last integer entry naming the knob, else none; a malformed or overflowing value
    /// counts as no entry.
    #[test]
    fn the_allocator_start_reads_the_pool_knobs() {
        let k = b"pool_block_min";
        assert_eq!(super::mempolicy_knob_u64(b"", k), None);
        assert_eq!(super::mempolicy_knob_u64(b"pool_block_min_x=5", k), None);
        assert_eq!(super::mempolicy_knob_u64(b" pool_block_min = 65536 ,ledger_floor=1", k), Some(65536));
        assert_eq!(super::mempolicy_knob_u64(b"pool_block_min=1,pool_block_min=1073741824", k), Some(1 << 30));
        assert_eq!(super::mempolicy_knob_u64(b"pool_block_min=1k", k), None);
        assert_eq!(super::mempolicy_knob_u64(b"pool_block_min=99999999999999999999999", k), None);
        assert_eq!(mempolicy::consts::KNOBS[mempolicy::consts::K::PoolBlockMin as usize].name, "pool_block_min");
    }

    /// Exercise the real allocator constructor in a fresh process. Applying these values at
    /// postmaster startup is too late: the pool has already copied its immutable configuration.
    #[test]
    fn allocator_constructor_honors_warm_limit_and_host_age() {
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("PGRUST_") {
                child.env_remove(key);
            }
        }
        let output = child
            .args(["--ignored", "--exact", "tests::allocator_constructor_configuration_child", "--test-threads=1"])
            .env("RUSTSQL_ALLOCATOR_CONFIG_CHILD", "1")
            .env("PGRUST_ALLOCATOR", "hearth")
            .env("PGRUST_POOL_MCX", "1")
            .env("PGRUST_MEMPOLICY", "pool_warm_zero=4194304,pool_warm_zero_max=8388608,pool_host_age_ms=5")
            .output()
            .expect("start constructor probe");
        assert!(output.status.success(), "constructor probe failed: {}{}",
            String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    }

    #[test]
    #[ignore = "invoked in a fresh process by allocator_constructor_honors_warm_limit_and_host_age"]
    fn allocator_constructor_configuration_child() {
        assert_eq!(std::env::var("RUSTSQL_ALLOCATOR_CONFIG_CHILD").as_deref(), Ok("1"));
        use mempolicy::consts::{get, source, K, Source};
        assert_eq!(get(K::PoolWarmZeroMax), 8 << 20);
        assert_eq!(get(K::PoolHostAgeMs), 5);
        assert_eq!(source(K::PoolWarmZeroMax), Source::Set);
        assert_eq!(source(K::PoolHostAgeMs), Source::Set);
        assert!(pagepool::on(), "the real constructor enabled the pool");
        assert!(pagepool::stats().warm_target <= 8 << 20, "the initialized pool respects the maximum");
    }

    /// pagepool P2 (ppgrav's find): a pool switched on by another consumer (PGRUST_POOL_GRAVITY=1, MCX unset) gets the
    /// ledger's hooks but not mcx's, and its claims reach the ledger (pool_claimed > 0).
    #[test]
    fn a_pool_on_without_mcx_still_reaches_the_ledger() {
        assert_eq!(super::pool_hooks(true, false), (true, false), "GRAVITY alone: the ledger's hooks, not mcx's");
        assert_eq!(super::pool_hooks(true, true), (true, true));
        assert_eq!(super::pool_hooks(false, true), (false, false), "no pool: nothing");
        assert_eq!(super::pool_hooks(false, false), (false, false));
        if !pagepool::init(&pagepool::Config::from_policy()) {
            return; // no reservation on this host: the pool stays off, as in the server
        }
        super::pool_mcx::ledger();
        let before = memledger::pool_claimed();
        let hint = pagepool::Hint::new(pagepool::Class::Task, pagepool::Tag::REGION);
        let b = pagepool::take(256 << 10, 16, hint).expect("a pool block");
        if cfg!(feature = "pagepool-global") {
            // PoolAlloc claimed before the door (its warm runs may serve this take): the door posted those claims
            assert!(before > 0, "the claims before the door reached the ledger");
        } else {
            assert!(memledger::pool_claimed() > before, "the claim reached the ledger: {} -> {}", before, memledger::pool_claimed());
        }
        assert!(memledger::pool_probed(), "the views read the pool");
        // SAFETY: the block take just served, never used again.
        unsafe { pagepool::give(b.cast(), 256 << 10) };
    }

    /// pagepool P2: the pool's term in the allocator's committed bytes is its published present bytes: at most what it
    /// claimed plus its metadata zone, and cheap enough for a probe (the witness: a read well under 1 us on average; the published form is two
    /// atomic loads, never a sweep).
    #[test]
    fn the_pools_committed_term_is_its_present_bytes_and_cheap() {
        assert_eq!(super::pool_mcx::present_or_zero(), 0, "the pool off: no term");
        if !pagepool::init(&pagepool::Config::from_policy()) {
            return; // no reservation on this host: the pool stays off, as in the server
        }
        super::pool_mcx::ledger();
        let _ = pagepool::refresh_present();
        let present = super::pool_mcx::present_or_zero();
        // claimed counts the superpages; present adds the metadata zone (its own mapping, never claimed through the door)
        let bound = memledger::pool_claimed().max(0) as u64 + pagepool::meta_present().unwrap_or(0) as u64;
        assert!(present as u64 <= bound, "present {present} within claimed + metadata {bound}");
        // RBR's retained = (mimalloc present + pool present) - (mimalloc live + pool in-use) shows the pool's
        // warm present once only if the owners' pool bytes (pagepool_total) are the pool's in-use bytes: a mcx block taken
        // through the source moves both by the same bytes, and its give moves both back
        super::pool_mcx::arm();
        let mcx_in_use = || pagepool::stats().in_use[pagepool::Tag::MCX.0 as usize] as i64;
        let (total0, use0) = (memledger::pagepool_total(), mcx_in_use());
        let ctx = mcx::MemoryContext::new("pool-probe");
        let layout = std::alloc::Layout::from_size_align(256 << 10, 8).unwrap();
        use mcx::Allocator as _;
        let p = ctx.mcx().allocate(layout).expect("a pooled block");
        assert!(pagepool::owns(p.cast::<u8>().as_ptr()), "the source served it");
        let (total, used) = (memledger::pagepool_total() - total0, mcx_in_use() - use0);
        assert!(total > 0, "the owner's pool bytes moved");
        assert_eq!(total, used, "pagepool_total moves as the pool's in-use bytes");
        // SAFETY: the chunk this context handed out for `layout`, freed once
        unsafe { ctx.mcx().deallocate(p.cast(), layout) };
        drop(ctx);
        pagepool::flush_shelf();
        assert_eq!((memledger::pagepool_total() - total0, mcx_in_use() - use0), (0, 0), "and both move back");
        let n = 100_000u32;
        let t = std::time::Instant::now();
        let mut sum = 0usize;
        for _ in 0..n {
            sum = sum.wrapping_add(std::hint::black_box(super::pool_mcx::present_or_zero()));
        }
        let ns = t.elapsed().as_nanos() / u128::from(n);
        std::hint::black_box(sum);
        eprintln!("present_or_zero: {ns} ns a read");
        assert!(ns < 1_000, "a probe read costs {ns} ns");
    }
}
