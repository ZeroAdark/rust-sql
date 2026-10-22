//! The memory runtime's loops beside the watchdog's sampler. Today's threads keep running them (the hosts start the
//! `gravity-reuse-idle` thread at the build-buffer pool's first take); the loop is here, it asks `mempolicy::keep` when
//! each step is due, and the mechanisms run behind the hooks their owner installs (`IdleHooks`).

use core::time::Duration;

/// The process-caches idle loop's mechanisms, installed by Gravity's hosts (their build-buffer pool, the grant pool's
/// depots, pgrc's part cache, the reclaimer).
pub struct IdleHooks {
    /// memory step 5b: whether the background reclaimer applies now, its tick, and its period.
    pub reclaim_applies: fn() -> bool,
    pub reclaim_tick: fn(),
    pub reclaim_tick_ms: u64,
    /// The idle threshold now (ms; `mempolicy::keep::IDLE_RELEASE_MS` unless a test set it).
    pub idle_ms: fn() -> u64,
    /// The last take, give or statement start (ms; 0 = none yet), and the clock it is read on.
    pub last_ms: fn() -> u64,
    pub now_ms: fn() -> u64,
    /// What the process caches keep for no running statement given back (the parked slot sets and regions, the part cache
    /// to its trim target, the allocator's retained memory).
    pub release_caches: fn(),
    /// The build-buffer pool's kept pages given back (the buffers stay kept).
    pub release_kept: fn(),
    /// A release past the governor's bar done, and an idle release done (their counts and report lines).
    pub pressure_released: fn(),
    pub idle_released: fn(),
}

/// The idle loop (never returns): every `mempolicy::keep::idle_period_ms` it ticks the reclaimer while it applies, gives
/// the process caches back while the process is past the memory governor's bar (`past_bar`), and once per idle period
/// (`idle_due`) gives back the kept pages and the process caches.
pub fn idle_loop(h: &IdleHooks) -> ! {
    // pagepool P3 (C.11): this is the memory runtime's loop thread (`memledger::ladder_pass::on_ladder_thread`): the
    // reclaimer's tick and the idle release below run here, never on a query thread
    memledger::ladder_pass::mark_ladder_thread();
    // D4692: the trims and releases this loop runs give their cache runs back here (with cache_release_bg on, every
    // thread but this one and the page releaser leaves them to the releaser)
    allocarena::own::mark_inline_release_ok();
    let mut done = 0u64;
    loop {
        let reclaiming = (h.reclaim_applies)();
        pgsync::thread::sleep(Duration::from_millis(mempolicy::keep::idle_period_ms(reclaiming, h.reclaim_tick_ms, (h.idle_ms)())));
        if reclaiming {
            (h.reclaim_tick)();
        }
        if mempolicy::keep::past_bar(mcx::governor::projected() as u64, mcx::governor::bar() as u64) {
            (h.release_caches)();
            (h.pressure_released)();
        }
        let last = (h.last_ms)();
        if !mempolicy::keep::idle_due(last, done, (h.now_ms)(), (h.idle_ms)()) {
            continue;
        }
        (h.release_kept)();
        (h.release_caches)();
        (h.idle_released)();
        done = last;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU8, Ordering::Relaxed};

    /// What `on_ladder_thread` read in the loop's reclaimer tick and in its idle release (bit 0: read; bit 1: true).
    static TICK: AtomicU8 = AtomicU8::new(0);
    static IDLE: AtomicU8 = AtomicU8::new(0);

    fn read(at: &AtomicU8) {
        at.store(1 | (memledger::ladder_pass::on_ladder_thread() as u8) << 1, Relaxed);
    }
    fn yes() -> bool {
        true
    }
    fn tick() {
        read(&TICK);
    }
    fn caches() {
        read(&IDLE);
    }
    fn nothing() {}
    fn one() -> u64 {
        1
    }
    fn later() -> u64 {
        1 << 40
    }

    static HOOKS: IdleHooks = IdleHooks {
        reclaim_applies: yes,
        reclaim_tick: tick,
        reclaim_tick_ms: 1,
        idle_ms: one,
        last_ms: one,
        now_ms: later,
        release_caches: caches,
        release_kept: nothing,
        pressure_released: nothing,
        idle_released: nothing,
    };

    /// pagepool P3 (C.11): the loop's thread is the ladder thread (`on_ladder_thread`) in the reclaimer's tick and in the
    /// idle release; the thread that started it is not. The loop never returns: its thread runs until the test process ends.
    #[test]
    fn the_loops_tick_and_idle_release_run_on_the_ladder_thread() {
        std::thread::spawn(|| idle_loop(&HOOKS));
        let t0 = std::time::Instant::now();
        while (TICK.load(Relaxed) == 0 || IDLE.load(Relaxed) == 0) && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!((TICK.load(Relaxed), IDLE.load(Relaxed)), (3, 3), "both read true on the marked loop thread");
        assert!(!memledger::ladder_pass::on_ladder_thread(), "the starting thread is no ladder thread");
    }
}
