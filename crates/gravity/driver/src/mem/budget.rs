//! Memory step 5: Gravity's ONE memory budget B and the registry of every consumer inside it: the ledger itself is the
//! `memledger` crate and the snapshot's composition `mempolicy::snapshot` (both re-exported here whole); this module reads
//! the ledger with the driver's own probes (the memory governor, the grant pool's held bytes) into a snapshot.
//!
//! Under on the snapshot also reads the process's resident bytes (`mcx::governor::probe_resident`), so memory no consumer
//! counts comes out of the free bytes. Nothing here runs per statement: snapshots run at the pool's arming, in the REPORT
//! census, every broker interval (6a) and every reclaimer tick (5b).

pub use memledger::*;
pub use mempolicy::snapshot::*;

/// B's free bytes now: the last snapshot's, less what the grant pool drew since (`u64::MAX` before any snapshot). The
/// reclaimer's tick refreshes the snapshot every `RECLAIM_TICK_MS` while it applies.
pub fn free_now() -> u64 {
    free_after(crate::mem::grant::POOL.held())
}

/// The tests that set the process-wide mode hold it, one at a time (a concurrent one would read the other's mode).
#[cfg(test)]
pub(crate) static TEST_MODE: pgsync::Mutex<()> = pgsync::Mutex::new(());

/// B and every registered consumer now; None under off (nothing is read). Refreshes `fits`'s free bytes.
pub fn snapshot() -> Option<Snapshot> {
    let cfg = cfg();
    if cfg.mode == Mode::Off {
        return None;
    }
    let hooks = all_hooks();
    let mut rows = [Row::default(); CONSUMERS];
    for c in Consumer::ALL {
        if let Some(h) = hooks[c as usize] {
            rows[c as usize] = Row { registered: true, current: (h.current)(), floor: (h.floor)(), cap: (h.cap)(), target: target(c) };
        }
    }
    let limit = match mcx::governor::limit() as u64 {
        0 => fallback_limit(),
        l => l,
    };
    let bar = mcx::governor::bar() as u64;
    let resident = match mempolicy::snapshot::resident_from(bar, cfg.mode) {
        mempolicy::snapshot::ResidentFrom::Probe => mcx::governor::probe_resident().unwrap_or(0) as u64,
        mempolicy::snapshot::ResidentFrom::None => 0,
        mempolicy::snapshot::ResidentFrom::Projected => mcx::governor::projected() as u64,
    };
    let held = crate::mem::grant::POOL.held();
    // C2: the accounts' resident bytes by reclaim class beside B (one pass over the account slots, as the view's)
    // ALLOC-LADDER (a): the allocator's retained bytes count as used until returned (the watchdog's last reading)
    // (REUSE: the arenas' free committed bytes are in both, resident until a purge gives them back: never headroom)
    let s = with_classes(compose_retained(cfg, limit, bar, resident, rows, managed_mask(), accounts_total(), memledger::arena::retained()), class_bytes());
    note_snapshot(held, s.free_b);
    memledger::note_b(s.budget.b);
    // D4403: the small buffer re-clamped to B / 16 wherever B is read, mode on (its takes skip make_room)
    crate::mem::grant::POOL.note_b(if cfg.mode == Mode::On { s.budget.b } else { 0 });
    Some(s)
}

/// B0: the work pool's capacity now under on (`sizes::work_pool_capacity` over the registered hooks, each cache at
/// `sizes::cache_kept` with `pinned` its in-use bytes, `resident` the memory watchdog's sample); `None` unless on. The 100 ms
/// tick's: nothing here allocates.
pub fn pool_now(resident: u64, pinned: &[u64; CONSUMERS]) -> Option<u64> {
    let cfg = cfg();
    if cfg.mode != Mode::On {
        return None;
    }
    let hooks = all_hooks();
    let mut rows = [Row::default(); CONSUMERS];
    for c in Consumer::ALL {
        if let Some(h) = hooks[c as usize] {
            rows[c as usize] = Row { registered: true, current: (h.current)(), floor: (h.floor)(), cap: (h.cap)(), target: target(c) };
        }
    }
    let limit = match mcx::governor::limit() as u64 {
        0 => fallback_limit(),
        l => l,
    };
    let bar = mcx::governor::bar() as u64;
    let resident = if bar != 0 { mcx::governor::projected() as u64 } else { resident };
    let s = compose_retained(cfg, limit, bar, resident, rows, managed_mask(), accounts_total(), memledger::arena::retained());
    ROW_CAP.store(row_cap_of(&s, pinned), core::sync::atomic::Ordering::Relaxed);
    let evictable = caches_evictable(&s, pinned);
    CACHES_EVICTABLE.store(evictable, core::sync::atomic::Ordering::Relaxed);
    if mcx::reconcile::on() {
        reconcile_terms(&s, pinned, evictable);
    }
    Some(pool_capacity(s, pinned))
}

/// `grant_room_counts_cache`: the bytes `s`'s managed caches hold past what they keep from the pool (`sizes::cache_kept`:
/// above their floors, never their pinned bytes), what a draw's inline take can give as its statement maps.
fn caches_evictable(s: &Snapshot, pinned: &[u64; CONSUMERS]) -> u64 {
    let mask = managed_mask();
    Consumer::ALL.iter().filter(|c| c.is_cache() && mask & (1u64 << (**c as u32)) != 0).fold(0u64, |a, c| {
        let r = s.rows[*c as usize];
        a.saturating_add(r.current.saturating_sub(mempolicy::sizes::cache_kept(true, r.current, r.floor, r.cap, pinned[*c as usize])))
    })
}

static CACHES_EVICTABLE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// `caches_evictable` at the last `pool_now` (0 before the first).
pub fn caches_evictable_now() -> u64 {
    CACHES_EVICTABLE.load(core::sync::atomic::Ordering::Relaxed)
}

/// `ladder_reconcile`: B's terms at the pool's tick: its used side (the accounts and the allocator's retained bytes),
/// the retained bytes alone, its free bytes, the shared_buffers account above its configured size (buf_grow's extents;
/// the row's floor is the configured size), the managed caches' evictable bytes, and the pool's capacity term by term
/// (`pool_capacity`'s).
fn reconcile_terms(s: &Snapshot, pinned: &[u64; CONSUMERS], evictable: u64) {
    use mempolicy::reconcile::Term;
    let sb = s.rows[Consumer::SharedBuffers as usize];
    mcx::reconcile::put(Term::BUsed, s.sum_current.saturating_add(s.retained));
    mcx::reconcile::put(Term::BRetained, s.retained);
    mcx::reconcile::put(Term::BFree, s.free_b);
    mcx::reconcile::put(Term::BufGrown, sb.current.saturating_sub(sb.floor));
    mcx::reconcile::put(Term::PoolEvictable, evictable);
    mcx::reconcile::put(Term::CapB, s.budget.b);
    mcx::reconcile::put(Term::CapRHigh, s.budget.r_high);
    mcx::reconcile::put(Term::CapFixed, s.fixed);
    mcx::reconcile::put(Term::CapUnattributed, s.unattributed);
    mcx::reconcile::put(Term::CapSharedBuffers, sb.current);
    mcx::reconcile::put(Term::CapCachesKept, caches_kept(s, pinned));
}

/// B0: `s`'s pool capacity with each cache at what it keeps (`sizes::cache_kept`).
pub fn pool_capacity(s: Snapshot, pinned: &[u64; CONSUMERS]) -> u64 {
    mempolicy::sizes::work_pool_capacity(&s.budget, s.fixed, s.unattributed, s.rows[Consumer::SharedBuffers as usize].current, caches_kept(&s, pinned))
}

/// B0: the bytes `s`'s caches keep from the pool (`sizes::cache_kept`, `pinned` their in-use bytes).
fn caches_kept(s: &Snapshot, pinned: &[u64; CONSUMERS]) -> u64 {
    let mask = managed_mask();
    Consumer::ALL.iter().filter(|c| c.is_cache()).fold(0u64, |a, c| {
        let r = s.rows[*c as usize];
        let managed = mask & (1u64 << (*c as u32)) != 0;
        a.saturating_add(mempolicy::sizes::cache_kept(managed, r.current, r.floor, r.cap, pinned[*c as usize]))
    })
}

/// D4078: the work pool's row cap: `pool_capacity` with no unattributed bytes (they swing 0-1.6 GiB per snapshot, the cap
/// must not), what the pool can reach under the broker, as its view prints it.
/// ALLOC-LADDER (a), memlead's change 2: it does take the allocator's fragmented bytes, as the least of the last three
/// readings (`memledger::arena::retained_floor`: steady across snapshots, never over-cutting the wall).
fn row_cap_of(s: &Snapshot, pinned: &[u64; CONSUMERS]) -> u64 {
    mempolicy::sizes::work_pool_capacity(&s.budget, s.fixed, memledger::arena::retained_floor(), s.rows[Consumer::SharedBuffers as usize].current, caches_kept(s, pinned))
}

/// D4078: the last row cap (`row_cap_of`) a B0 tick computed (`pool_now`); 0 before the first.
static ROW_CAP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// D4078: the work pool's row cap now, for an admission's one-owner wall (`grant::owner_wall_for`): the last B0 tick's,
/// else (before the broker's first tick) one computed from a snapshot now, its caches at no pinned bytes, never the tick's
/// armed capacity; 0 under off.
pub fn row_cap() -> u64 {
    match ROW_CAP.load(core::sync::atomic::Ordering::Relaxed) {
        0 => snapshot().map_or(0, |s| row_cap_of(&s, &[0; CONSUMERS])),
        c => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    /// The process-global registry: off reads nothing; registration is first-wins; enforcing needs on, the actuator bit
    /// and a registration. One test owns the statics (the others are pure).
    #[test]
    fn registry_off_reads_nothing_and_enforcing_needs_on_the_bit_and_a_registration() {
        fn ten() -> u64 {
            10
        }
        fn zero() -> u64 {
            0
        }
        let _one = pgsync::lock(&TEST_MODE);
        configure(Cfg { mode: Mode::Off, ..Cfg::DEFAULT });
        assert!(snapshot().is_none());
        assert!(fits(u64::MAX - 1), "off: always fits");
        assert!(register(Consumer::WorkPool, Hooks { current: ten, floor: zero, cap: ten, reclaim: None }));
        assert!(!register(Consumer::WorkPool, Hooks { current: zero, floor: zero, cap: zero, reclaim: None }), "first wins");
        assert!(!enforcing(Consumer::WorkPool), "off");
        configure(Cfg { mode: Mode::Advise, ..Cfg::DEFAULT });
        assert!(!enforcing(Consumer::WorkPool), "advise applies nothing");
        set_fallback_limit(8 * GIB);
        let s = snapshot().expect("advise snapshots");
        assert_eq!(s.row(Consumer::WorkPool).current, 10, "the first registration's hooks");
        assert!(!s.row(Consumer::PartCache).registered);
        configure(Cfg { mode: Mode::On, actuators: actuator::PART_CACHE, ..Cfg::DEFAULT });
        assert!(!enforcing(Consumer::WorkPool), "on, but not its actuator");
        configure(Cfg { mode: Mode::On, actuators: actuator::WORK_POOL, ..Cfg::DEFAULT });
        assert!(enforcing(Consumer::WorkPool));
        assert!(!enforcing(Consumer::PartCache), "not registered");
        set_target(Consumer::WorkPool, 5);
        assert_eq!(target(Consumer::WorkPool), Some(5));
        configure(Cfg::DEFAULT);
    }

    /// C2: the snapshot carries the accounts' resident bytes by reclaim class: a GiB moved onto a cache's account shows in
    /// the cache class's sum and leaves the unbound one (other tests' threads allocate meanwhile: a 64 MiB margin). The GiB
    /// moves through the home (C8): a door-1 charge of the cache opened on this thread, whose account is the unbound one
    /// (a test thread never takes an account), and dropped back onto it.
    #[test]
    fn the_snapshot_sums_the_accounts_by_reclaim_class() {
        const SLACK: i64 = 64 << 20;
        let _one = pgsync::lock(&TEST_MODE);
        configure(Cfg { mode: Mode::Advise, ..Cfg::DEFAULT });
        set_fallback_limit(8 * GIB);
        assert_eq!(current(), Acct::UNBOUND, "a test thread's account is the unbound one");
        let before = snapshot().expect("advise snapshots").by_class;
        let cache = memledger::Owner::Cache(memledger::Holder::Of(Consumer::ZoneStore));
        let held = memledger::Charge::open(cache, memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, GIB);
        let after = snapshot().expect("advise snapshots").by_class;
        drop(held);
        configure(Cfg::DEFAULT);
        let moved = |c: Class| after[c as usize].wrapping_sub(before[c as usize]);
        assert!((moved(Class::Cache) - GIB as i64).abs() < SLACK, "the cache class gains the GiB: {before:?} -> {after:?}");
        assert!((moved(Class::Unbound) + GIB as i64).abs() < SLACK, "the unbound class loses it: {before:?} -> {after:?}");
    }
}
