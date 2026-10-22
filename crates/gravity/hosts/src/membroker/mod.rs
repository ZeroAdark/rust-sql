//! Memory step 6a: the advise broker. It runs on the memory watchdog's
//! thread, once a second (`memruntime::set_broker_tick`, installed at postmaster start): the budget's configuration, the
//! part cache's re-use clock, the 1 Hz samples; every `pgrust.memory_broker_interval` seconds the census, the policy's
//! plan (`gravity_driver::mem::policy`), the targets stored for the actuators (nothing reads one before step 7), the
//! view's copy and, under PGRUST_GRAVITY_REPORT, one log line. Off: the clock stops (the re-use counters record nothing)
//! and nothing else runs; the view says off.

pub mod curves;
mod raw;
pub mod settings;
pub mod signals;
pub mod view;

use gravity_driver::mem::budget::{self, Cfg, Consumer, Mode, Snapshot};
use gravity_driver::mem::policy::{self, Plan, PolicyCfg, PolicyState, Signals};

use self::signals::{Census, Ring};

/// The latest interval as the view shows it.
#[derive(Clone, Copy, Debug)]
pub struct Published {
    pub mode: Mode,
    /// The broker second of the plan (0: none yet).
    pub at: u64,
    pub interval_s: u64,
    pub snapshot: Option<Snapshot>,
    pub census: Census,
    pub signals: Signals,
    pub plan: Plan,
    pub state: PolicyState,
    pub plans: u64,
    pub plan_us_max: u64,
    pub panics: u64,
    pub stopped: bool,
}

/// After this many consecutive failed intervals the broker stops planning (one LOG) until its configuration changes.
const STOP_AFTER: u32 = 5;

struct Broker {
    state: PolicyState,
    work: Ring,
    unattributed: Ring,
    last_plan: u64,
    /// An interval is running: found set at the next tick, the last one unwound (the watchdog contains it).
    in_plan: bool,
    failed_run: u32,
    panics: u64,
    stopped: Option<Cfg>,
    plan_us_max: u64,
    was_off: bool,
}

static BROKER: pgsync::Mutex<Option<Broker>> = pgsync::Mutex::new(None);
/// The latest tick's broker second.
static SEC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static PUBLISHED: pgsync::Mutex<Option<Published>> = pgsync::Mutex::new(None);

/// The broker second of the latest tick (0: none yet).
pub fn seconds() -> u64 {
    SEC.load(core::sync::atomic::Ordering::Relaxed)
}

/// The latest published interval (None: the broker never ran).
pub fn published() -> Option<Published> {
    *pgsync::lock(&PUBLISHED)
}

/// `pgrust.memory_broker_interval` in seconds, at least 1 (`inputs::memory_broker_interval_s`).
pub fn interval_s() -> u64 {
    crate::inputs::memory_broker_interval_s()
}

fn report() -> bool {
    gravity_driver::env::text("PGRUST_GRAVITY_REPORT").is_some_and(|v| !v.is_empty() && v != "0")
}

/// C3: the fast rungs of `mempolicy::decide` (installed by `memruntime::set_fast_rungs`): bytes freeable now, and reclaim.
pub fn fast_bytes() -> u64 {
    gravity_driver::mem::reclaim::fast_bytes()
}

pub fn fast_reclaim(want: u64) -> u64 {
    gravity_driver::mem::reclaim::reclaim_fast(want)
}

/// B0: a derived pool's capacity follows free room on the memory watchdog's 100 ms tick under the work pool's actuator,
/// growing past what it holds only by `room`, the room under the headroom margin (`mempolicy::sizes::POOL_HEADROOM_PCT`).
pub fn pool_tick(resident: u64, room: Option<u64>) {
    if !budget::pool_follows() {
        return;
    }
    let mut pinned = [0u64; budget::CONSUMERS];
    pinned[Consumer::PartCache as usize] = pgrc_am::inval::registry().in_use_bytes();
    pinned[Consumer::FpTables as usize] = crate::dict::text_hosts::fp_pinned_bytes();
    if let Some(capacity) = budget::pool_now(resident, &pinned) {
        let held = gravity_driver::POOL.held();
        // `grant_room_counts_cache` (USER 10-19: evict as memory is used): the reservation stays whole, the caches' bytes past
        // their floors are room it may take; the draws evict them inline as they map
        // (ladder_reconcile reads `room` itself, the resident room: the evictable bytes are their own term)
        let grant = room.map(|r| mempolicy::sizes::grant_room(r, budget::caches_evictable_now(), mempolicy::consts::get(mempolicy::consts::K::GrantRoomCountsCache) != 0));
        let capacity = mempolicy::sizes::pool_tick_capacity(capacity, held, grant);
        gravity_driver::POOL.arm(mempolicy::sizes::pool_follow(capacity, None, held));
        if mcx::reconcile::on() {
            reconcile_terms(capacity, held, room);
        }
    }
}

/// `ladder_reconcile`: the grant pool's terms at its tick (the capacity it armed with, its held bytes, the room it read),
/// and the parked depot's: the parked regions' lengths and the parked and slot_depot accounts' resident bytes.
fn reconcile_terms(capacity: u64, held: u64, room: Option<u64>) {
    use mempolicy::reconcile::{Term, NONE};
    mcx::reconcile::put(Term::PoolCapacity, capacity);
    mcx::reconcile::put(Term::PoolHeld, held);
    mcx::reconcile::put(Term::PoolRoom, room.unwrap_or(NONE));
    mcx::reconcile::put(Term::DepotMapped, gravity_driver::park::REGIONS.parked_bytes() as u64);
    let resident = |a: memledger::Acct| {
        let r = memledger::account_row(a);
        r.account_bytes.saturating_sub(r.not_resident_bytes).max(0) as u64
    };
    mcx::reconcile::put(Term::DepotResident, resident(memledger::Acct::PARKED).saturating_add(resident(memledger::Acct::SLOT_DEPOT)));
}

/// D3992: the backstop's rungs (installed by `memruntime::set_backstop_rungs`): bytes it could free now, and reclaim.
pub fn backstop_bytes() -> u64 {
    gravity_driver::mem::reclaim::backstop_bytes()
}

pub fn backstop_reclaim(want: u64) -> u64 {
    gravity_driver::mem::reclaim::reclaim_backstop(want)
}

/// D4353: the memory ladder's R2a count (`memruntime::set_ladder_elastic`): what its reclaim can free now.
pub fn ladder_elastic() -> u64 {
    gravity_driver::mem::reclaim::ladder_elastic()
}

/// D4353 (C): the ladder's R2a reclaim (to each cache's protected floor) and R2b's protected bytes (LADDER-DESIGN 0b).
pub fn ladder_reclaim(want: u64) -> u64 {
    gravity_driver::mem::reclaim::ladder_reclaim(want)
}

pub fn ladder_protected() -> u64 {
    gravity_driver::mem::reclaim::ladder_protected()
}

/// The ladder pass's landing credit made the ladder hook's at the server's start (`gravity_driver::mem::reap::start`),
/// called by the postmaster once memruntime started.
pub fn landing_credit_start() {
    gravity_driver::mem::reap::start();
}

/// D3872: the residency pass (`gravity_driver::mem::resident::pass`), run on the memory watchdog's thread at the memory
/// policy's cadence (`memruntime::set_residency_pass`, installed at postmaster start) and by the accounts view. Under
/// PGRUST_RESIDENCY_REPORT (developer) a server log line every `RESIDENCY_REPORT_NS`: the passes since the last line, their
/// mean and longest time, and the regions and bytes the last one read. Not under PGRUST_GRAVITY_REPORT: a line no statement
/// asked for, written inside a statement's lines, breaks the harnesses that read those lines per statement.
pub fn residency_pass() -> bool {
    let ok = gravity_driver::mem::resident::pass();
    if gravity_driver::env::set("PGRUST_RESIDENCY_REPORT") {
        residency_report();
    }
    ok
}

const RESIDENCY_REPORT_NS: u64 = 10_000_000_000;

/// One thread calls it (pg:memresident, the residency pass's): the window's state is its own.
fn residency_report() {
    use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
    use gravity_driver::mem::resident::{PASSES, PASS_NS_LAST, PASS_NS_SUM, PASS_REGIONS_LAST, PASS_SCANNED_LAST};
    static AT: AtomicU64 = AtomicU64::new(0);
    static N: AtomicU64 = AtomicU64::new(0);
    static NS: AtomicU64 = AtomicU64::new(0);
    static MAX: AtomicU64 = AtomicU64::new(0);
    let (now, n, ns) = (pg_clock::mono_ns(), PASSES.load(Relaxed), PASS_NS_SUM.load(Relaxed));
    MAX.fetch_max(PASS_NS_LAST.load(Relaxed), Relaxed);
    let at = AT.load(Relaxed);
    let restart = || {
        AT.store(now, Relaxed);
        N.store(n, Relaxed);
        NS.store(ns, Relaxed);
    };
    if at == 0 {
        restart();
        return;
    }
    if now.saturating_sub(at) < RESIDENCY_REPORT_NS {
        return;
    }
    let (dn, dns) = (n.saturating_sub(N.load(Relaxed)), ns.saturating_sub(NS.load(Relaxed)));
    // D4812: through elog's stderr writer, which hands a memory thread's write off the thread (the residency pass runs on
    // pg:memresident)
    elog::write_stderr(&format!( // alloc: Cold
        "LOG:  gravity: residency passes={dn} in {}ms mean_us={} max_us={} busy_ppm={} regions={} scanned_mib={}\n",
        now.saturating_sub(at) / 1_000_000,
        dns / dn.max(1) / 1000,
        MAX.swap(0, Relaxed) / 1000,
        dns.saturating_mul(1_000_000) / now.saturating_sub(at).max(1),
        PASS_REGIONS_LAST.load(Relaxed),
        PASS_SCANNED_LAST.load(Relaxed) >> 20
    ));
    restart();
}

/// The broker's tick (the memory watchdog's thread, once a second; `sec` counts from 1).
pub fn tick(sec: u64) {
    SEC.store(sec, core::sync::atomic::Ordering::Relaxed);
    let cfg = crate::inputs::memory_budget_cfg();
    budget::configure(cfg);
    let mut g = pgsync::lock(&BROKER);
    let b = g.get_or_insert_with(|| Broker {
        state: PolicyState::default(),
        work: Ring::default(),
        unattributed: Ring::default(),
        last_plan: 0,
        in_plan: false,
        failed_run: 0,
        panics: 0,
        stopped: None,
        plan_us_max: 0,
        was_off: false,
    });
    if cfg.mode == Mode::Off {
        if !b.was_off {
            // the re-use counters stop with the clock; the view says off
            pgrc_read::partcache::set_now_sec(0);
            b.was_off = true;
            publish_off(b, sec);
        }
        return;
    }
    b.was_off = false;
    // memory step 5b: the caches the reclaimer manages declared (undeclared when it stops applying), and its thread running
    gravity_driver::mem::reclaim::sync_managed();
    if gravity_driver::mem::reclaim::applies() {
        crate::dict::text_hosts::reuse::start_reclaimer();
    }
    let now = u32::try_from(sec).unwrap_or(u32::MAX).max(1);
    pgrc_read::partcache::set_now_sec(now);
    sample(b);
    if b.in_plan {
        b.in_plan = false;
        b.panics += 1;
        b.failed_run += 1;
        if b.failed_run >= STOP_AFTER && b.stopped.is_none() {
            b.stopped = Some(cfg);
            let _ = elog::ereport(types_error::LOG)
                .errmsg(format!("gravity: memory broker stopped after {STOP_AFTER} failed intervals; a configuration change re-arms it")) // alloc: Cold
                .finish(types_error::ErrorLocation::new("gravity", 0, "membroker"));
        }
    }
    if let Some(at) = b.stopped {
        if at == cfg {
            return;
        }
        b.stopped = None;
        b.failed_run = 0;
    }
    if b.last_plan != 0 && sec.saturating_sub(b.last_plan) < interval_s() {
        return;
    }
    b.last_plan = sec;
    b.in_plan = true;
    let t0 = pg_clock::MonoStamp::now();
    let Some(snap) = budget::snapshot() else {
        b.in_plan = false;
        return;
    };
    let census = signals::census(now, &snap, &b.work, &b.unattributed);
    let s = signals::signals(now, &snap, &census);
    let (plan, next) = policy::plan(&b.state, &s, &PolicyCfg::DEFAULT);
    b.state = next;
    for c in Consumer::ALL {
        if plan.targeted[c as usize] {
            budget::set_target(c, plan.target[c as usize]);
        }
        budget::set_loss(c, plan.loss[c as usize]);
    }
    b.plan_us_max = b.plan_us_max.max(t0.elapsed_ns() / 1000);
    b.in_plan = false;
    b.failed_run = 0;
    let p = Published {
        mode: cfg.mode,
        at: sec,
        interval_s: interval_s(),
        snapshot: Some(snap),
        census,
        signals: s,
        plan,
        state: b.state,
        plans: b.state.plans,
        plan_us_max: b.plan_us_max,
        panics: b.panics,
        stopped: false,
    };
    drop(g);
    if memledger::value::ext() {
        publish_tails();
    }
    *pgsync::lock(&PUBLISHED) = Some(p);
    if report() {
        let _ = elog::ereport(types_error::LOG).errmsg(line(&p)).finish(types_error::ErrorLocation::new("gravity", 0, "membroker"));
    }
}

/// Broker (b), under `PGRUST_MEMPOLICY=broker_value=1`: each plan re-marks the tails of the caches whose doors count tail
/// hits (the part cache, the condition cache, the fp tables, the build pool), each its coldest tenth of bytes in its own
/// eviction order; a hit there until the next plan is a tail hit (`memledger::value::tail_hit`). Outside the broker's lock,
/// on the memory watchdog's thread under the knob only, which runs it under memory pressure: no allocation and no wait,
/// as D4029 (d)'s cancel path (each cache's walk under `try_lock`, a busy cache keeping its last mark; a fixed buffer for
/// the walks that rank, `memledger::value::coldest_tenth`).
pub fn publish_tails() {
    pgrc_am::inval::registry().publish_tail();
    crate::ops::scan::pgrc::cond_cache::publish_tail();
    crate::dict::text_hosts::fp_publish_tail();
    crate::dict::text_hosts::reuse::publish_tail();
}

/// The 1 Hz samples: the work pool's signal (`signals::work_sample`) and the bytes nobody attributes (when the governor
/// samples resident memory).
fn sample(b: &mut Broker) {
    let cur = signals::currents();
    let held = cur[Consumer::WorkPool as usize].unwrap_or(0);
    let sum = cur.iter().flatten().fold(0u64, |a, x| a.saturating_add(*x));
    let projected = if mcx::governor::bar() != 0 { mcx::governor::projected() as u64 } else { 0 };
    b.work.push(signals::work_sample(held, sum.saturating_sub(held), projected));
    if projected != 0 {
        b.unattributed.push(projected.saturating_sub(sum));
    }
}

fn publish_off(b: &Broker, sec: u64) {
    *pgsync::lock(&PUBLISHED) = Some(Published {
        mode: Mode::Off,
        at: sec,
        interval_s: interval_s(),
        snapshot: None,
        census: Census::default(),
        signals: Signals::default(),
        plan: Plan::default(),
        state: b.state,
        plans: b.state.plans,
        plan_us_max: b.plan_us_max,
        panics: b.panics,
        stopped: b.stopped.is_some(),
    });
}

/// The REPORT line: B, free_B, fixed, the work demand, and per targeted consumer current / floor / target / re-used
/// (900 s) / moved, in MiB.
pub fn line(p: &Published) -> String {
    let mib = |b: u64| b >> 20;
    let Some(snap) = p.snapshot.as_ref() else {
        return "gravity: memory broker: mode off".to_string(); // alloc: Cold
    };
    let mut s = format!( // alloc: Cold
        "gravity: memory broker: mode {} B {} free_B {} r_high {} fixed {} work_demand {} queued {} fits {}",
        p.mode.name(),
        mib(snap.budget.b),
        mib(snap.free_b),
        mib(snap.budget.r_high),
        mib(p.signals.fixed),
        mib(p.signals.work_demand),
        mib(p.signals.queued),
        p.plan.fits,
    );
    for c in Consumer::ALL {
        let k = c as usize;
        if p.plan.targeted[k] {
            s.push_str(&format!( // alloc: Cold
                " {}={}/{}/{}/{}/{}",
                c.name(),
                mib(p.signals.inputs[k].current),
                mib(p.plan.floor[k]),
                mib(p.plan.target[k]),
                mib(p.census.reused[k][2]),
                p.plan.moved[k] >> 20,
            ));
        }
    }
    s.push_str(" (MiB: current/floor/target/reused_900s/moved) "); // alloc: Cold
    // memory step 5b: the reclaimer's counters since the process started
    s.push_str(&gravity_driver::mem::reclaim::line()); // alloc: Cold
    s
}
