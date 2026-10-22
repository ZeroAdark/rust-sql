//! The grant pool knobs (breakers.md "The grant pool", "Grant admission by estimate and the small lane"): the pool armed once per process, the statement's grant through it under the mode, and the lease that gives it back.

use super::room::{granted_memory, granted_width};
use gravity_driver::mem::budget;
use super::{Kind, Knob};

pub static KNOBS: &[Knob] = &[
    Knob { name: "pgrust.gravity_grant_pool_mb", kind: Kind::Custom, default: "derived (3/4 of RAM past shared_buffers, each mapped admission arena's resident 64 MiB held in it; the headroom split under an address-space rail)", doc: "the pool's capacity in MiB" },
    Knob { name: "PGRUST_GRAVITY_GRANT_POOL_MB", kind: Kind::Env, default: "unset", doc: "pgrust.gravity_grant_pool_mb from the server's environment, under the GUC (an e2e file's .env sizes the pool before the harness probe arms it: m4_d622; read once, when the pool arms)" },
    Knob { name: "pgrust.gravity_max_concurrent", kind: Kind::Custom, default: "derived (0 = no gate; 1..64 under a rail)", doc: "statements in flight through the pool; a serial, breakerless, single-pipeline program runs outside the pool on both protocols and is never counted (H16 (a): nothing to reserve, the premise guarded, XX000 on a region draw)" },
    Knob { name: "PGRUST_GRAVITY_MAX_CONCURRENT", kind: Kind::Env, default: "unset", doc: "pgrust.gravity_max_concurrent from the server's environment, under the GUC (an e2e file's .env gates the pool before the harness probe arms it: m4_d599 / m4_d601 / m4_d591; read once, when the pool arms)" },
    Knob { name: "pgrust.gravity_grant_wait_ms", kind: Kind::Custom, default: "600000", doc: "how long a statement waits for its grant (a floor need past the pool: for the pool to empty, then it runs alone past it): past it the statement ERRORs out of memory, never admitted with nothing reserved" },
    Knob { name: "pgrust.gravity_grant_mode", kind: Kind::Custom, default: "estimate", doc: "estimate = the program's estimated need (floored, scaled) up to the cap; flat = the GUCs' grant asked whole; lanes = a small lane beside the pool for estimates under the small size" },
    Knob { name: "pgrust.gravity_grant_floor_mb", kind: Kind::Custom, default: "64", doc: "estimate mode: the least a statement asks" },
    Knob { name: "pgrust.gravity_grant_estimate_scale", kind: Kind::Custom, default: "1", doc: "estimate mode: the estimate's multiplier (1..16)" },
    Knob { name: "pgrust.gravity_grant_small_mb", kind: Kind::Custom, default: "64", doc: "lanes mode: a statement estimated at or under this takes the small lane at this size" },
    Knob { name: "pgrust.gravity_grant_small_reserve_mb", kind: Kind::Custom, default: "lanes: 1024; every other mode: 128 on a derived pool, none on an explicit one", doc: "the small buffer (D4403): a part of the pool no big admission takes, where statements reserving at most 16 MiB and draws with no reservation of at most 1 MiB go first. Lanes mode: the small lane's slice, at most half the capacity. Every other mode: at most the capacity / 16 and, with the memory budget on, B / 16, re-clamped as they move; on a derived pool by default, on an explicit pool (pgrust.gravity_grant_pool_mb, PGRUST_GRAVITY_GRANT_POOL_MB) only when this is set. A server-start setting, read once with the mode" },
    Knob { name: "PGRUST_GRAVITY_REAP", kind: Kind::Env, default: "unset (on)", doc: "off: a dropped mapped reservation is unmapped by the thread that drops it and its pool charge released at once, as before the reaper; else one of at least 4 MiB writable goes to the reaper thread (driver mem/reap.rs), which unmaps it off the statement's path and then releases its charge (a rollback knob, read once per process)" },
    Knob { name: "PGRUST_GRAVITY_START", kind: Kind::Env, default: "unset", doc: "the A/B override of D3798 S2's grant::decide_start (unset: hash breakers start small when the statement is admitted into an idle pool, else at their shares): share = every hash breaker region commits its whole share at its open, as before start-small; small = they always start small, as E1 had it (grant::start_forced, read once)" },
    Knob { name: "pgrust.gravity_grant_feedback", kind: Kind::Custom, default: "memory", doc: "Controls sizing feedback independently of pgrust.condition_cache: observed groups, key ranges and phase-1 record counts stay in memory; memory (default) = each breaker's charged peak over its recent runs (memory step 3a, in this process only) is its next run's start: the max of its clean peaks, their p90 from five, x2 after a spill and x3 after a refusal; a breaker with no history starts small and grows; on = memory plus the history kept across restarts in pg_stat/pgrust_gravity_grant_feedback (step 3b); off = nothing recorded or consulted" },
    Knob { name: "pgrust.gravity_test_spill_every", kind: Kind::Custom, default: "0", doc: "a test lever, not a tuning one (the forced-spill test fault, driver mem/grow.rs `Fault`): N > 0 refuses every Nth room ask of each breaker region's spill-capable roads (a hash build's grow, a sort run's place, a hash aggregate's floor commit, the leader's build merge), so the breaker spills there as at its ceiling; read with the grant knobs into the statement's grant (`Grant::spill_every`); 0 = off" },
    Knob { name: "pgrust.gravity_test_revoke_every", kind: Kind::Custom, default: "0", doc: "a test lever, not a tuning one (C5's revoke road, driver mem/grow.rs `Fault`): N > 0 makes every Nth room ask of each breaker region's spill-capable roads ask a revoke of its own statement (mcx::revoke::request_self: the interrupt byte's REVOKE_BIT), so the kernels' poll cuts the region and its next ask spills; read with the grant knobs into the statement's grant (`Grant::revoke_every`); 0 = off, no effect" },
    Knob { name: "pgrust.gravity_test_refuse_debt", kind: Kind::Custom, default: "off", doc: "a test lever, not a tuning one (NR-6, driver mem/grow.rs `Fault`): on refuses each breaker region's draw past its cap on a road a SPILL-DEBT row names (crates/gravity/lint/SPILL-DEBT), as the pool refuses one: a refused grow is the statement's 53200 and the server log names the row; read with the grant knobs into the statement's grant (`Grant::refuse_debt`); off = no effect" },
    Knob { name: "pgrust.gravity_grow", kind: Kind::Custom, default: "off", doc: "grow mode (off by default): on = a statement's admission claims its estimate start (pgrust.gravity_grant_mode estimate: the estimate x the scale, no grant floor, each breaker's start share at most its PostgreSQL per-node budget, no learned raise), every hash breaker starts small beside other statements too, and the breakers grow on demand from the pool's room; with work_mem unset (its default source) there is no work_mem ceiling (the region wall, at most the pool's big lane), an explicit work_mem still caps; a refused grow spills where the breaker can, else 53200; nothing is taken back. Read at the statement's admission and carried in its grant" },
    Knob { name: "pgrust.gravity_exact_size", kind: Kind::Builtin, default: "none", doc: "developer (exactsize): build = a hash build asks for its records and grows to its counted need at its end; final = the owner side sized from its count; all = both; none = today's sizes" },
];

/// `pgrust.gravity_exact_size` (exactsize) as the grant's bits (`gravity_driver::EXACT_BUILD` | `EXACT_FINAL`), read at the
/// admission and carried in the statement's grant (`Grant::exact`); 0 (none) on a backend without the GUC machinery.
pub(crate) fn exact_size() -> u8 {
    let v = &guc_tables::vars::pgrust_gravity_exact_size;
    if !v.installed() {
        return 0;
    }
    let mut bits = 0;
    let set = v.read();
    if set & guc_tables::consts::GRAVITY_EXACT_SIZE_BUILD != 0 {
        bits |= gravity_driver::EXACT_BUILD;
    }
    if set & guc_tables::consts::GRAVITY_EXACT_SIZE_FINAL != 0 {
        bits |= gravity_driver::EXACT_FINAL;
    }
    bits
}

/// D2845 / D2861: a backend's admission arena (`admit::ARENA_BYTES` of address space), mapped at its level's first admission
/// and kept until the session ends; each reset hands back every page past `arena::KEEP_BYTES`, so at most that much of it
/// stays resident. The address-space rail keeps every admitting backend's mapping up front (a refused mapping is refused at
/// map time); the derived memory pool holds a mapped arena's resident prefix while it is mapped (`admission_arena_kept`).
const ADMISSION_ARENA: u64 = crate::admit::ARENA_BYTES as u64;
const ADMISSION_KEPT: u64 = gravity_program::arena::KEEP_BYTES as u64;
/// Set when the pool arms at the derived memory cap: an explicit capacity (the GUC, the environment) bounds statements
/// alone, and a rail's already keeps the arenas whole.
static KEEP_ARENAS: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// The pool's hold on a level's admission arena, taken when exec's `Backend::graph_arena` maps it and given back with it.
pub(crate) fn admission_arena_kept() -> gravity_driver::Held {
    if KEEP_ARENAS.load(core::sync::atomic::Ordering::Relaxed) {
        gravity_driver::POOL.keep(ADMISSION_KEPT)
    } else {
        gravity_driver::Held::NONE
    }
}

/// The backends that can run a served statement and so keep an admission arena: the connections (`max_connections`) and
/// the background workers (`max_worker_processes`: an SPI statement is served too). Autovacuum, the WAL senders and the
/// runtime's gang run none.
fn admitting_backends() -> u64 {
    (init_small::globals::MaxConnections().max(1) as u64).saturating_add(init_small::globals::max_worker_processes().max(0) as u64)
}

/// Under a rail, the address space kept for the backends still to come: every backend slot (`MaxBackends`: connections,
/// autovacuum, the background and parallel workers, the WAL senders) maps a stack the size of this thread's and a thread
/// arena up to the arenas' bound; every admitting backend its admission arena.
fn connection_reserve() -> u64 {
    let slots = init_small::globals::MaxBackends().max(1) as u64;
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get() as u64);
    mempolicy::sizes::rail_reserve(slots, admitting_backends().min(slots), own_stack_mapping(), cpus, ADMISSION_ARENA)
}

/// The size of the mapping that holds this thread's stack (a local's address in /proc/self/maps); 0 when unread.
fn own_stack_mapping() -> u64 {
    let local = 0u8;
    let at = core::ptr::addr_of!(local) as u64;
    let Some(text) = proc_text("/proc/self/maps") else { return 0 };
    for line in text.lines() {
        let Some((lo, hi)) = line.split_whitespace().next().and_then(|range| range.split_once('-')) else { continue };
        let (Ok(lo), Ok(hi)) = (u64::from_str_radix(lo, 16), u64::from_str_radix(hi, 16)) else { continue };
        if (lo..hi).contains(&at) {
            return hi - lo;
        }
    }
    0
}

/// Armed at the session's first serve (`reentry::built`), so the process's first statement to take a grant does not
/// pay the /proc reads and the LOG line in its prepare (PERF-COLD 09-25: ~100 us of a W0 cold cell's first statement
/// once H16 (a) left the probe `select 1` outside the pool); `admit_grant` still arms it first where no session did.
pub(crate) fn arm_grant_pool() {
    static ARMED: pgsync::OnceLock<()> = pgsync::OnceLock::new();
    ARMED.get_or_init(|| {
        // D3314: the memory limit the server runs under (the watchdog's one limit: a cgroup's memory.max, its ancestors
        // included, never past MemTotal), not MemTotal: under a 16 GiB container the pool armed at 45 GiB, admission never
        // throttled and the kernel OOM-killed the server. D3175: the host-declared memory on wasm32.
        let mem = gravity_driver::machine::ram_reading().unwrap_or(0);
        let limit = proc_bytes("/proc/self/limits", "Max address space", 1);
        let mapped = proc_bytes("/proc/self/status", "VmSize:", 1024);
        let shared = (guc_tables::vars::NBuffers.read().max(0) as u64) * 8192;
        let railed = mempolicy::sizes::railed(limit, mem);
        // under a rail (breakers.md "The grant pool") the headroom is split between statements in flight; a statement's regions at the GUCs' grant, one breaker live
        let per_regions = gravity_driver::regions_of(granted_memory(granted_width()), 1, granted_width().max(1));
        // what a statement holds in the pool: its regions and its tables (the arena's chunks, up to the grant)
        let per_pooled = per_regions + granted_memory(granted_width()).bytes;
        let reserve = if railed { connection_reserve() } else { 0 };
        let (gate, today_cap) = if railed {
            mempolicy::sizes::railed_pool(limit, mapped, reserve, per_pooled)
        } else {
            (0, gravity_program::cost::derived_pool_cap(mem, shared))
        };
        // memory step 5: Gravity's one memory budget B. Its settings, the limit when the memory watchdog
        // published none, and the consumers this layer owns. The capacity comes from B only when the broker APPLIES it to the
        // work pool (pgrust.memory_broker = on with the work_pool actuator, no rail); off and advise keep today's line.
        budget::configure(gravity_hosts::inputs::memory_budget_cfg());
        budget::set_fallback_limit(mem);
        register_budget_consumers(shared);
        let from_budget = if !railed && budget::enforcing(budget::Consumer::WorkPool) { budget::snapshot().map(|b| b.work_pool_capacity) } else { None };
        // R5 (D3821): a derived capacity comes out of B at arm time in advise and on (`derived_pool_in_b`: B less its
        // free reserve, the fixed memory, shared_buffers and every cache at its derived cap, never under one statement's
        // pooled bytes; today's when the server sets the part cache's size). At the default config under a 16 GiB unit
        // the pool armed at 11,185 MiB beside 6.4 GB of cache caps in a 13.4 GiB B and the kernel OOM-killed the server
        // every ~80 s.
        let derived_cap = today_cap;
        let today_cap = if railed || budget::mode() == budget::Mode::Off {
            today_cap
        } else {
            budget::snapshot().map_or(today_cap, |s| {
                let explicit_part = gravity_hosts::inputs::part_cache_server_mb().is_some();
                // ladder C1 (`cache_floor`, only with the ladder on: it evicts above the floors): the caches at their
                // protected floors, HIGH as the reserve. The ladder's mode as the server started (a reload re-arms nothing)
                let floors = mempolicy::ladder::cache_floor() && guc_tables::backing::pgrust_memory_ladder() == guc_tables::consts::MEMORY_LADDER_ON;
                mempolicy::sizes::derived_pool_in_b(today_cap, &s.budget, s.fixed, shared, mem, explicit_part, per_pooled, floors)
            })
        };
        // R5: a pool fitted inside B (the derived capacity brought under today's) holds no flat arena prefixes (`keeps_arenas`)
        let fitted = mempolicy::sizes::fitted(railed, from_budget.is_some(), today_cap, derived_cap);
        // PGRUST_GRAVITY_GRANT_POOL_MB: the capacity from the server's environment, under the GUC (an e2e file's .env, D622)
        let env_pool = super::env_u64("PGRUST_GRAVITY_GRANT_POOL_MB");
        let explicit = gravity_hosts::inputs::custom_guc_u64("pgrust.gravity_grant_pool_mb").or(env_pool);
        let cap = mempolicy::sizes::pool_capacity(explicit, from_budget, today_cap);
        // memory step 5b: a derived capacity follows B at every broker interval under the work pool's actuator; an
        // explicit one or a rail's stays where it armed
        budget::set_pool_derived(!railed && explicit.is_none());
        KEEP_ARENAS.store(mempolicy::sizes::keeps_arenas(railed, explicit.is_some(), fitted, cap), core::sync::atomic::Ordering::Relaxed);
        // PGRUST_GRAVITY_MAX_CONCURRENT: the same gate from the server's environment, under the GUC (an e2e file's .env: the
        // pool arms at the process's first serve, the harness's probe, before any statement of the file can SET it)
        let env_gate = super::env_u64("PGRUST_GRAVITY_MAX_CONCURRENT");
        let gate = gravity_hosts::inputs::custom_guc_u64("pgrust.gravity_max_concurrent").or(env_gate).unwrap_or(gate);
        // ladder C1: the cache bytes past their floors the ladder cannot free count as held only in a pool sized with the
        // caches at those floors: not an explicit or broker-applied capacity, a rail's, or today's pool beside an explicit
        // part cache (the operator's call; that setting is the part cache's floor)
        let floored = explicit.is_none()
            && from_budget.is_none()
            && !railed
            && budget::mode() != budget::Mode::Off
            && mempolicy::ladder::cache_floor()
            && guc_tables::backing::pgrust_memory_ladder() == guc_tables::consts::MEMORY_LADDER_ON
            && gravity_hosts::inputs::part_cache_server_mb().is_none()
            && budget::snapshot().is_some_and(|s| s.budget.b != 0);
        gravity_driver::mem::reclaim::set_pool_floors(floored);
        gravity_driver::POOL.arm(cap);
        gravity_driver::POOL.gate(gate);
        // the small lane's slice is a server-start setting: read once here with the mode. D4403: every other mode keeps
        // the small buffer on a derived pool (`small_buffer_default`, or the knob's size when set); an explicit capacity
        // (the GUC, the environment: a rig's pool sized for its waits) has none unless the knob is set
        let small_mb = gravity_hosts::inputs::custom_guc_u64("pgrust.gravity_grant_small_reserve_mb");
        if grant_mode() == GrantMode::Lanes {
            gravity_driver::POOL.set_reserve(small_mb.unwrap_or(1024) << 20);
        } else if let Some(bytes) = small_mb.map(|m| m << 20).or_else(|| explicit.is_none().then(mempolicy::admit::small_buffer_default)) {
            gravity_driver::POOL.set_buffer(bytes);
        }
        if cap != 0 {
            let _ = elog::ereport(types_error::LOG).errmsg(format!("gravity: grant pool armed at {} MiB, {} statements in flight (memory limit {} MiB, mapped {} MiB, address-space limit {} MiB, kept for backends {} MiB, shared_buffers {} MiB, a statement's regions {} MiB + tables {} MiB, mode {:?}, small lane {} MiB)", cap >> 20, gate, mem >> 20, mapped >> 20, limit >> 20, reserve >> 20, shared >> 20, per_regions >> 20, granted_memory(granted_width()).bytes >> 20, grant_mode(), gravity_driver::POOL.reserve() >> 20)).finish(types_error::ErrorLocation::new("gravity", 0, "grant")); // alloc: Cold
        }
        // memory step 5: B and its consumers beside the armed line (REPORT only; nothing under off)
        if grant_report() && budget::mode() != budget::Mode::Off {
            if let Some(b) = budget::snapshot() {
                let _ = elog::ereport(types_error::LOG).errmsg(format!("gravity: memory budget: {} pool_today {} pool_armed {} explicit {}", b.line(), today_cap >> 20, cap >> 20, explicit.is_some())).finish(types_error::ErrorLocation::new("gravity", 0, "grant")); // alloc: Cold
            }
        }
    });
}

/// Memory step 5: shared_buffers' bytes as the pool armed (NBuffers x BLCKSZ; a postmaster setting, kept here so the
/// budget's readers, on any thread, never read a GUC store).
static SHARED_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Memory step 5: the budget's consumers exec owns (shared_buffers: its live size, never below the configured bytes, which
/// are its floor and cap (D4064); the grant pool; the program cache; the backends' kept holds). Registration stores fn
/// pointers only (nothing is read under off); the first registration wins.
fn register_budget_consumers(shared: u64) {
    use budget::{register, Consumer, Hooks};
    SHARED_BYTES.store(shared, core::sync::atomic::Ordering::Relaxed);
    fn shared_bytes() -> u64 {
        SHARED_BYTES.load(core::sync::atomic::Ordering::Relaxed)
    }
    // what the pool holds less the kept holds (D2845's level arenas), which the per-backend row counts: the statements in
    // flight and the parked depots, so no byte is counted in two rows
    fn pool_held() -> u64 {
        gravity_driver::POOL.held_by_statements()
    }
    // until step 2c gives each statement a guarantee: the bytes the statements in flight hold, the parked depots excluded
    fn pool_floor() -> u64 {
        let idle = (gravity_driver::REGIONS.stats().bytes as u64).saturating_add(gravity_driver::DEPOT.stats().bytes as u64);
        gravity_driver::POOL.held_by_statements().saturating_sub(idle)
    }
    fn pool_cap() -> u64 {
        gravity_driver::POOL.capacity()
    }
    fn program_cache_bytes() -> u64 {
        gravity_backend::cache::stats().bytes as u64
    }
    fn kept_bytes() -> u64 {
        gravity_driver::POOL.held().saturating_sub(gravity_driver::POOL.held_by_statements())
    }
    // D4064 (C8 area move 4): the pool's live size, never below the configured bytes. An online resize grows the active
    // extents past shared_buffers, and B and the work pool's capacity must count those bytes as held. One relaxed load of
    // the size bufmgr publishes at every resize: no lock, no allocation (the hook runs on the watchdog's and B0's ticks).
    // Floor and cap stay the configured bytes until step 8 sets them.
    fn shared_live() -> u64 {
        let active = bufmgr_seams::active_buffers_or(0).max(0) as u64;
        shared_bytes().max(active.saturating_mul(8192))
    }
    register(Consumer::SharedBuffers, Hooks { current: shared_live, floor: shared_bytes, cap: shared_bytes, reclaim: None });
    register(Consumer::WorkPool, Hooks { current: pool_held, floor: pool_floor, cap: pool_cap, reclaim: None });
    register(Consumer::ProgramCaches, Hooks { current: program_cache_bytes, floor: program_cache_bytes, cap: program_cache_bytes, reclaim: None });
    // D4298 (T4 E2): the memory ladder alone trims the code cache (R2a's tail, after the catalog L2) down to the modules
    // running code holds; the hook above stays as it was, so the broker and the watchdog's backstop read it as before
    fn program_cache_in_use() -> u64 {
        gravity_backend::cache::in_use_bytes() as u64
    }
    fn program_cache_trim(to: u64) -> u64 {
        gravity_backend::cache::trim_to(usize::try_from(to).unwrap_or(usize::MAX)) as u64
    }
    gravity_driver::mem::budget::register_ladder_reclaim(Consumer::ProgramCaches, program_cache_in_use, program_cache_trim);
    register(Consumer::PerBackend, Hooks { current: kept_bytes, floor: kept_bytes, cap: kept_bytes, reclaim: None });
    // the hosts' caches (part cache, fp tables, build pool, zone store, condition cache)
    gravity_hosts::memory_budget::register_all();
}

/// A kernel pseudo-file's text (/proc), `None` when unread: the one raw fs site of the pool's host probe.
/// A kernel pseudo-file's text (/proc), `None` when unread: the one raw fs site of the pool's host probe.
fn proc_text(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok() // alloc: Cold
}

fn proc_bytes(path: &str, key: &str, scale: u64) -> u64 {
    let Some(text) = proc_text(path) else { return 0 };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            return rest.split_whitespace().next().and_then(|t| t.parse::<u64>().ok()).map_or(0, |n| n.saturating_mul(scale));
        }
    }
    0
}

/// `pgrust.gravity_grant_mode` (breakers.md "Grant admission by estimate and the small lane"): unset = `estimate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantMode {
    Flat,
    Estimate,
    Lanes,
    /// Outside the pool (`GrantLease::strict`): never `grant_mode`'s answer, only the lease's report.
    Unadmitted,
}

pub(crate) fn grant_mode() -> GrantMode {
    grant_knobs().mode
}

/// The per-statement grant knobs (room.rs's `pgrust.gravity_grant_mb` too), with their defaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GrantKnobs {
    pub mode: GrantMode,
    pub grant_mb: Option<u64>,
    pub wait_ms: u64,
    pub floor_mb: u64,
    pub estimate_scale: u64,
    pub small_mb: u64,
    /// D2990: `pgrust.gravity_statement_pool_fraction` in parts per million (`pool_fraction_ppm`).
    pub pool_ppm: u32,
    /// Steps 3a / 3b: `pgrust.gravity_grant_feedback` (off | memory | on, default memory).
    pub feedback: Feedback,
    /// The forced-spill test fault: `pgrust.gravity_test_spill_every` (`Grant::spill_every`; 0 = off).
    pub spill_every: u32,
    /// C5's revoke test lever: `pgrust.gravity_test_revoke_every` (`Grant::revoke_every`; 0 = off).
    pub revoke_every: u32,
    /// NR-6's test lever: `pgrust.gravity_test_refuse_debt` (`Grant::refuse_debt`; off by default).
    pub refuse_debt: bool,
    /// Grow mode: `pgrust.gravity_grow` (`Grant::grow`; off by default).
    pub grow: bool,
    /// Grow mode: work_mem set (`room::work_mem_set`: its source past the default), so its ceiling stands under grow mode;
    /// read only under grow mode (false off).
    pub work_mem_set: bool,
}

/// `pgrust.gravity_grant_feedback`: `Off` = nothing recorded or consulted; `Memory` = step 3a, the history in this process
/// only (the cluster file is never read or written); `On` = 3a plus step 3b's file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feedback {
    Off,
    Memory,
    On,
}

impl Feedback {
    /// "on" (or true / 1 / yes) = `On`; "off" (or false / 0 / no) = `Off`, the rollback; anything else, unset included,
    /// = `Memory`, the default (until step 3b's witnesses pass: pg_checksums and basebackup with the file present).
    fn parse(v: Option<&str>) -> Feedback {
        match v.map(str::trim) {
            Some("on" | "true" | "1" | "yes") => Feedback::On,
            Some("off" | "false" | "0" | "no") => Feedback::Off,
            _ => Feedback::Memory,
        }
    }
}

leader_tls::leader_tls! {
    thread_local! {
        static KNOB_SNAPSHOT: core::cell::Cell<Option<(u64, GrantKnobs)>> = const { core::cell::Cell::new(None) };
        /// The options' registry slots, in `GrantKnobs::read`'s order: each name is looked up once, then read by its index
        /// (D2896).
        static SLOTS: [core::cell::Cell<gravity_hosts::inputs::OptionSlot>; 12] = const { [const { core::cell::Cell::new(gravity_hosts::inputs::OptionSlot::Unknown) }; 12] };
    }
}

/// Read again only when this backend's GUC store has mutated since the last read (SET, RESET, a SET LOCAL's rollback,
/// a reload, a new placeholder all bump `store_mutation_count`): a clean statement looks nothing up, and a read after a
/// mutation reads each option by the registry index its first lookup found (`SLOTS`), not by name.
pub fn grant_knobs() -> GrantKnobs {
    KNOB_SNAPSHOT.with(|s| snapshot(s, guc::store::store_mutation_count(), GrantKnobs::read))
}

pub(crate) fn feedback_enabled() -> bool {
    grant_knobs().feedback != Feedback::Off
}

#[cfg(test)]
pub(crate) fn set_feedback_for_test(feedback: Feedback) {
    let mut knobs = grant_knobs();
    knobs.feedback = feedback;
    KNOB_SNAPSHOT.with(|s| s.set(Some((guc::store::store_mutation_count(), knobs))));
}

fn snapshot(s: &core::cell::Cell<Option<(u64, GrantKnobs)>>, at: u64, read: impl FnOnce() -> GrantKnobs) -> GrantKnobs {
    match s.get() {
        Some((seen, k)) if seen == at => k,
        _ => {
            let k = read();
            s.set(Some((at, k)));
            k
        }
    }
}

impl GrantKnobs {
    fn read() -> GrantKnobs {
        use gravity_hosts::inputs::{custom_guc_text_at, custom_guc_u64_at};
        SLOTS.with(|s| {
            // grow mode; work_mem's source is read only under it (`GrantKnobs::work_mem_set`)
            let grow = custom_guc_text_at(&s[11], "pgrust.gravity_grow").is_some_and(|v| ["on", "true", "1", "yes"].iter().any(|w| v.eq_ignore_ascii_case(w)));
            let mode = match custom_guc_text_at(&s[0], "pgrust.gravity_grant_mode").as_deref() {
                Some("flat") => GrantMode::Flat,
                Some("lanes") => GrantMode::Lanes,
                _ => GrantMode::Estimate,
            };
            GrantKnobs {
                mode,
                grant_mb: custom_guc_u64_at(&s[1], "pgrust.gravity_grant_mb"),
                wait_ms: custom_guc_u64_at(&s[2], "pgrust.gravity_grant_wait_ms").unwrap_or(600_000),
                floor_mb: custom_guc_u64_at(&s[3], "pgrust.gravity_grant_floor_mb").unwrap_or(64),
                estimate_scale: custom_guc_u64_at(&s[4], "pgrust.gravity_grant_estimate_scale").unwrap_or(1).clamp(1, 16),
                small_mb: custom_guc_u64_at(&s[5], "pgrust.gravity_grant_small_mb").unwrap_or(64),
                pool_ppm: pool_fraction_ppm(custom_guc_text_at(&s[6], "pgrust.gravity_statement_pool_fraction").as_deref()),
                feedback: Feedback::parse(custom_guc_text_at(&s[7], "pgrust.gravity_grant_feedback").as_deref()),
                spill_every: custom_guc_u64_at(&s[8], "pgrust.gravity_test_spill_every").map_or(0, |n| n.min(u32::MAX as u64) as u32),
                revoke_every: custom_guc_u64_at(&s[9], "pgrust.gravity_test_revoke_every").map_or(0, |n| n.min(u32::MAX as u64) as u32),
                refuse_debt: custom_guc_text_at(&s[10], "pgrust.gravity_test_refuse_debt").is_some_and(|v| ["on", "true", "1", "yes"].iter().any(|k| v.eq_ignore_ascii_case(k))),
                grow,
                work_mem_set: grow && super::room::work_mem_set(),
            }
        })
    }
}

/// D2990: `pgrust.gravity_statement_pool_fraction` (0 < f <= 1, default 1): a ceiling on the part of the grant pool's big
/// lane one statement may hold, its reservation and its growth past its grant together (driver `Reservation::grow_fits`), in
/// parts per million. 1 sets no ceiling (each breaker's own bound is its work_mem budget); unset, unparsable or outside (0, 1] reads
/// the default.
pub(crate) fn pool_fraction_ppm(v: Option<&str>) -> u32 {
    match v.and_then(|t| t.parse::<f64>().ok()) {
        Some(f) if f > 0.0 && f <= 1.0 => ((f * gravity_driver::POOL_FRACTION_WHOLE as f64).round() as u32).clamp(1, gravity_driver::POOL_FRACTION_WHOLE),
        _ => gravity_driver::POOL_FRACTION_DEFAULT,
    }
}

/// The per-statement grant lines (ask, estimate, drawn) under PGRUST_GRAVITY_REPORT, read once per process.
fn grant_report() -> bool {
    static ON: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *ON.get_or_init(|| super::env_on("PGRUST_GRAVITY_REPORT"))
}

/// The statement's grant through the pool (`pgrust.gravity_grant_wait_ms`, default 600 s), reported when trimmed; the lease holds it.
/// A pool that cannot hold the floor grant's need within the wait is the statement's out-of-memory ERROR (S7). A statement
/// its caller's grant runs under (D622: a nested statement, `caller_granted` at its ExecutorStart) never queues: the entry
/// reserved each of its sites' grants at its start (`reserve_sites`) and this takes the one reserved for `program` at `width`.
/// `pipelines`: the program's, with the statement's estimates bound (KEY-PREDICTED-ROWS road (b), exec admit `bound_all`):
/// the estimate the grant asks for.
/// `plan`: the statement's plan, read for DR-7's planner-default groups (`groups_default`); `None`: no plan (an InitPlan's).
pub(crate) fn admit_grant(program: &gravity_program::Program, pipelines: &[gravity_program::Pipeline], width: u16, plan: Option<&'static types_nodes::plannodes::PlannedStmt<'static>>) -> Result<GrantLease, Box<types_error::PgError>> {
    if let Some(handed) = handed_lease(program, width, false) {
        return handed.map(|(lease, _)| lease);
    }
    queued(program, pipelines, width, plan)
}

/// `admit_grant` for a site that may run narrower than `width` (the pull's producers): with the width it runs at. A handed
/// grant reserved narrower than the run's width (a SET of the worker GUCs between a cursor's OPEN and its FETCH) that does
/// not fit again at the run's width beside the caller's is kept, and the site runs at the reserved width (a narrower width
/// never changes the rows).
pub(crate) fn admit_grant_narrowing(program: &gravity_program::Program, pipelines: &[gravity_program::Pipeline], width: u16, plan: Option<&'static types_nodes::plannodes::PlannedStmt<'static>>) -> Result<(GrantLease, u16), Box<types_error::PgError>> {
    if let Some(handed) = handed_lease(program, width, true) {
        return handed;
    }
    queued(program, pipelines, width, plan).map(|lease| (lease, width))
}

/// `admit_grant` through the pool's queue (up to `pgrust.gravity_grant_wait_ms`).
fn queued(program: &gravity_program::Program, pipelines: &[gravity_program::Pipeline], width: u16, plan: Option<&'static types_nodes::plannodes::PlannedStmt<'static>>) -> Result<GrantLease, Box<types_error::PgError>> {
    let knobs = grant_knobs();
    let fb = if knobs.feedback != Feedback::Off { feedback_begin(program, width, knobs.feedback == Feedback::On) } else { None };
    let ask = ask(program, pipelines, width, plan);
    let wait = core::time::Duration::from_millis(knobs.wait_ms);
    let pool = &gravity_driver::POOL;
    // the reservation is what the Node draws at each grant tried (D192: the shares follow each slot's peak); D4403: the
    // ask's own read once (the small test and the big lane's first try both read it)
    let want_need: core::cell::Cell<Option<u64>> = core::cell::Cell::new(None);
    let need = &|g: gravity_driver::Grant| match want_need.get().filter(|_| g == ask.want) {
        Some(n) => n,
        None => {
            let n = gravity_driver::footprint(g, program, width.max(1));
            if g == ask.want {
                want_need.set(Some(n));
            }
            n
        }
    };
    // NEW-50: a streamed Incremental Sort's raise, only if the pool seats it at once; else today's ask below
    if let Some(a) = ask.raise.and_then(|r| raise_now(pool, r, need)) {
        return Ok(granted(a, ask.estimate, ask.mode).with_feedback(fb));
    }
    // D4403: a small statement (the lanes slice's ask, else a small reservation at its unchanged ask) takes the small
    // buffer, else the big lane at once, else waits for either; any other statement the big lane's queue as before
    let a = match small_ask(pool, &ask, need).map(|g| admit_small_waiting(pool, g, need, wait)).transpose().inspect_err(|_| feedback_end(fb))?.flatten() {
        Some(a) => a,
        // waiting holds nothing: under a rail a statement queues for a slot rather than running at the floor
        None => admit_waiting(pool, ask.want, need, wait).inspect_err(|_| feedback_end(fb))?,
    };
    let a = match a {
        Ok(a) => a,
        Err(r) => {
            feedback_end(fb);
            let _ = elog::ereport(types_error::LOG).errmsg(format!("gravity: grant refused: the {} MiB floor needs {} MiB, pool held {} of {} MiB after {} ms", r.grant.bytes >> 20, r.need >> 20, r.held >> 20, r.capacity >> 20, r.waited_ms)).finish(types_error::ErrorLocation::new("gravity", 0, "grant")); // alloc: Cold
            return Err(crate::glue::pg_error(gravity_driver::Error::OutOfMemory { bytes: r.need, held: r.held, capacity: r.capacity, why: None }));
        }
    };
    Ok(granted(a, ask.estimate, ask.mode).with_feedback(fb))
}

/// Step 3a: `program`'s share breakers at `width` take their starts for this statement (`mem::feedback::begin`), each at
/// most the statement's fair share of the big lane once it is admitted (the lane over the admitted statements and it).
/// A breaker that starts small (`grant::starts_small`) charges what it commits, so its peak is its need (`measured`).
/// `file`: step 3b's file is read once (`pgrust.gravity_grant_feedback = on`). Its id, and whether a breaker had no history.
fn feedback_begin(program: &gravity_program::Program, width: u16, file: bool) -> Option<(u64, bool)> {
    use gravity_abi::breaker::Budget;
    if file {
        super::feedback_file::load_once();
    }
    let pool = &gravity_driver::POOL;
    let lane = pool.capacity().saturating_sub(pool.reserve());
    let cap = if lane == 0 { u64::MAX } else { lane / (gravity_driver::mem::grant::running() + 1) };
    let parallel = |p: &gravity_program::Pipeline| width >= 2 && p.parallel == gravity_program::Parallel::Eligible;
    let slots = program.pipelines.iter().enumerate().filter_map(|(i, p)| p.breaker().filter(|b| matches!(b.budget, Budget::Share | Budget::Pool)).map(|b| (b.slot, parallel(p), gravity_driver::mem::feedback::measured(b.kind) || gravity_driver::mem::grant::may_start_small_in(program, i))));
    gravity_driver::mem::feedback::begin(program.key.0, program.front_key().0, width, init_small::globals::MyDatabaseId(), slots, cap)
}

/// Step 3a: a statement refused before it ran gives its feedback entry back (nothing recorded).
fn feedback_end(fb: Option<(u64, bool)>) {
    if let Some((id, _)) = fb {
        gravity_driver::mem::feedback::end(id);
    }
}

/// DR-7 + KN-02 (breakers.md "Grant admission by estimate"): `stmt` has a grouping Agg (not AGG_PLAIN: a share breaker)
/// whose groups the planner estimated from defaults (`Agg::gravity_groups_default`, its estinfo SELFLAG_USED_DEFAULT): a
/// key without statistics, where 200 groups may be 2 M. Read where the plan's rows are bound, from the planner's own flag.
fn groups_default(stmt: &'static types_nodes::plannodes::PlannedStmt<'static>) -> bool {
    let mut any = false;
    gravity_decompose::rerun::each_plan_node(stmt, &mut |n| {
        any |= n.as_agg().is_some_and(|a| a.gravity_groups_default && a.aggstrategy != gravity_decompose::ops::shared::agg::AGG_PLAIN);
    });
    any
}

/// DR-7: what `pipelines`' share breakers hold when every row a breaker receives is its own group: each share (every
/// worker's and the owner's fold, as `grant_need_of` counts them) its rows x record. The planner knows the rows without
/// statistics (a relation's size), not the groups, so this bounds a defaulted breaker's ask: a small table never asks the cap.
/// D3132 (B): a pass over a slot's spill partitions (`ClaimKind::SpillPartitions`: D3057's `P'`, a spilled build's kept
/// probe rows) bounds nothing of its own: its rows are rows its probe pipeline already counts, handed again into the same
/// slot, and none before the build's first spill. Counted, a count per key over a LEFT join under a GROUP BY of those
/// counts (`P'` with the probe's 15 M predicted rows copied) asked 2304 MiB for 1280 at W0 on a run that never spills,
/// and every share-budgeted breaker's share grew with it.
fn rows_bound(pipelines: &[gravity_program::Pipeline], width: u16) -> u64 {
    let shares = width.max(1) as u64 + 1;
    // D3024 (b): a build drain claiming its build's records counts as a source's claims: its rows are the build's
    // records, which no other pipeline's breaker counts
    let own = pipelines.iter().filter(|p| matches!(p.claims, gravity_program::ClaimKind::Source | gravity_program::ClaimKind::BuildRecords { .. }));
    own.filter_map(gravity_program::cost::breaker::of).map(|c| gravity_program::cost::breaker::rows_bytes(c.rows, c.record).saturating_mul(shares)).fold(0, u64::saturating_add)
}

/// DR-7's raise from `base` toward `to`, halved until its reservation (`need`: the grant + its regions, the pool's
/// `footprint`) fits `room` = half the pool's big lane (`u64::MAX` unarmed), never under `base`. A raise built on a guess
/// never takes more than half the pool for one statement, so the statements beside it keep the other half; and a raise the
/// pool can never seat would only wait out the bytes wait and halve (a 1024 MiB ask reserves ~2.5 GiB at W8 under
/// work_mem 64MB: under a 2 GiB pool it waited 2 s, then took 512). Half: CHOSEN, not measured, as the small lane's slice.
fn raise_within(base: u64, to: u64, room: u64, need: &dyn Fn(u64) -> u64) -> u64 {
    let mut g = to;
    while g > base && need(g) > room {
        g /= 2;
    }
    g.max(base)
}

/// What a statement asks of the pool for `program` at `width` under `pgrust.gravity_grant_mode`: the grant tried first
/// (`want`), the small lane's size when it takes that lane, the estimate reported, and (NEW-50, `stream_raise`) a raise
/// past the cap tried before `want`, taken only when the pool seats it at once (`raise_now`).
struct Ask {
    want: gravity_driver::Grant,
    small: Option<gravity_driver::Grant>,
    raise: Option<gravity_driver::Grant>,
    estimate: u64,
    mode: GrantMode,
}

/// The GUCs' grant for a statement admitted now at `width` (`granted_memory`) with its exact sizing, its start (D3798 S2,
/// decided once here: `decide_start_in`, small into an idle pool, else the share; small beside others too under grow mode),
/// its one-owner wall (D4078, `owner_wall_for`, which its footprint's owner pins and its owner phases read) and grow mode
/// (`grow_cap`).
fn statement_cap(width: u16, knobs: &GrantKnobs) -> gravity_driver::Grant {
    let cap = grow_cap(granted_memory(width).with_exact(exact_size()), knobs);
    cap.with_start(gravity_driver::mem::grant::start_for_in(knobs.grow)).with_owner_wall(gravity_driver::mem::grant::owner_wall_for())
}

/// Grow mode (`pgrust.gravity_grow`): `cap` marked grow (`Grant::grow`) and, with work_mem unset (`GrantKnobs::work_mem_set`),
/// its ceilings the region wall held to the pool's big lane (`mempolicy::grow::grow_ceiling`): no work_mem ceiling. An
/// explicit work_mem keeps PostgreSQL's per-node ceilings. Off: `cap` as it is.
fn grow_cap(cap: gravity_driver::Grant, knobs: &GrantKnobs) -> gravity_driver::Grant {
    if !knobs.grow {
        return cap;
    }
    let cap = cap.with_grow(true);
    if knobs.work_mem_set {
        return cap;
    }
    let pool = &gravity_driver::POOL;
    let c = mempolicy::grow::grow_ceiling(pool.capacity().saturating_sub(pool.reserve()), gravity_driver::mem::grow::MAX_REGION);
    cap.with_ceilings(c, c)
}

fn ask(program: &gravity_program::Program, pipelines: &[gravity_program::Pipeline], width: u16, plan: Option<&'static types_nodes::plannodes::PlannedStmt<'static>>) -> Ask {
    arm_grant_pool();
    let knobs = grant_knobs();
    let cap = statement_cap(width, &knobs);
    let mode = knobs.mode;
    // exactsize B1c: under build | all a hash build asks for its records; its directory is drawn at its end from the count.
    // D3205: under final | all a serial pipeline's breaker asks by the planner's rows at its end, not 0
    let estimate = if mode == GrantMode::Flat && !grant_report() { 0 } else { gravity_program::cost::grant_need_of_at(pipelines, program.slots.max_live, width.max(1), cap.exact_build(), cap.exact_rows()).unwrap_or(0) };
    match mode {
        GrantMode::Flat | GrantMode::Unadmitted => Ask { want: cap, small: None, raise: None, estimate, mode },
        // grow mode: the estimate start (`grow_want`), no grant floor, no DR-7 or NEW-50 raise: its breakers grow on demand
        // past it. Not a trim: the floors stay the PostgreSQL budgets, which under grow mode cap each breaker's start share
        // and never raise it (`gravity_driver::mem::grant::share_of`)
        GrantMode::Estimate if cap.grow => {
            let want = mempolicy::grow::grow_want(estimate, knobs.estimate_scale, cap.bytes);
            Ask { want: cap.trimmed(want).with_floors(cap.sort_floor, cap.hash_floor), small: None, raise: None, estimate, mode }
        }
        // D2587: the estimate sizes the grant, never a breaker under PostgreSQL's own budget for its node (the cap's floors)
        GrantMode::Estimate => {
            let want = estimate.saturating_mul(knobs.estimate_scale).max(knobs.floor_mb << 20).min(cap.bytes);
            // DR-7 + KN-02: a breaker whose groups the planner took from defaults may hold a record for every row it
            // receives: such a statement asks up to that (`rows_bound`), the cap at most; every other one its estimate
            let want = if want < cap.bytes && plan.is_some_and(groups_default) {
                let pool = &gravity_driver::POOL;
                let room = if pool.capacity() == 0 { u64::MAX } else { gravity_program::cost::fraction(pool.capacity().saturating_sub(pool.reserve()), 1, 2) };
                let need = |g: u64| gravity_driver::footprint(cap.trimmed(g).with_floors(cap.sort_floor, cap.hash_floor), program, width.max(1));
                raise_within(want, rows_bound(pipelines, width).min(cap.bytes), room, &need)
            } else {
                want
            };
            // NEW-50: a streamed Incremental Sort's largest prefix group past the cap raises the ask, never past the floor
            // nor half the pool's big lane (DR-7's room); not under an explicit `gravity_grant_mb`, the user's cap
            let floor = knobs.floor_mb << 20;
            let raise = (knobs.grant_mb.is_none() && want < floor).then(|| stream_raise(program, plan, want, floor)).flatten().map(|to| {
                let pool = &gravity_driver::POOL;
                let room = if pool.capacity() == 0 { u64::MAX } else { gravity_program::cost::fraction(pool.capacity().saturating_sub(pool.reserve()), 1, 2) };
                let need = |g: u64| gravity_driver::footprint(cap.trimmed(g).with_floors(cap.sort_floor, cap.hash_floor), program, width.max(1));
                raise_within(want, to, room, &need)
            });
            let floors = |g: u64| cap.trimmed(g).with_floors(cap.sort_floor, cap.hash_floor);
            Ask { want: floors(want), small: None, raise: raise.filter(|&r| r > want).map(floors), estimate, mode }
        }
        GrantMode::Lanes => {
            let small = knobs.small_mb << 20;
            Ask { want: cap, small: (estimate <= small).then(|| cap.trimmed(small.min(cap.bytes))), raise: None, estimate, mode }
        }
    }
}

/// The lease of an admitted reservation, its trim or wait reported.
/// NEW-50: a streamed Incremental Sort (lowerv4 ops/sort/inc_stream.rs, INCSTREAM) holds one presorted-prefix group at a
/// time in its run, so its run's bytes are its largest group's, which the estimate (the sort's rows) does not see and the
/// cap (work_mem x hash_mem_multiplier x the width) does not hold when the group is skewed: a group past the run's share
/// falls back to the breaker form (NEW-43 (A): put + merge, ~19 ms more per 200,000 rows of 80 bytes at W0 than in
/// memory). The ask the stream's run needs: the breaker cost function's bytes for the planner's largest prefix group
/// (`largest_prefix_group`) at the stream run's record (`cost::breaker::record`), at each of the program's live shares
/// (`Grant::split` divides by them), capped at `floor` (the least a statement asks where the cap allows it: every W8
/// statement's grant); `None` without a stream, statistics, or a raise past `want`.
fn stream_raise(program: &gravity_program::Program, plan: Option<&'static types_nodes::plannodes::PlannedStmt<'static>>, want: u64, floor: u64) -> Option<u64> {
    use gravity_program::cost::breaker;
    let record = gravity_lowerv4::ops::sort::stream_producers(program).filter_map(|k| program.pipelines.get(k)).map(breaker::record).max()?;
    let rows = largest_prefix_group(plan?)?;
    let to = breaker::rows_bytes(rows, record).saturating_mul(program.slots.max_live.max(1) as u64).min(floor);
    (to > want).then_some(to)
}

/// NEW-50: the rows of the largest presorted-prefix group over `stmt`'s Incremental Sorts whose leading presorted key is a
/// column of the scan below them, as ANALYZE saw it: the column's most common value's frequency (`pg_statistic` MCV), else
/// one over its distinct values (`stadistinct`), times the scan's rows. A key over several columns reads its leading one
/// (an upper bound: a group of the prefix lies inside a group of its first column). None without one or statistics.
fn largest_prefix_group(stmt: &'static types_nodes::plannodes::PlannedStmt<'static>) -> Option<u64> {
    if !syscache_seams::lookup_pg_statistic_bundle::is_installed() {
        return None;
    }
    let ctx = gravity_driver::mem::root::Root::new("GravityStreamGroups");
    let mut most: Option<u64> = None;
    gravity_decompose::rerun::each_plan_node(stmt, &mut |n| {
        if let Some(g) = n.as_incremental_sort().and_then(|s| prefix_group(ctx.mcx(), stmt, s)) {
            most = Some(most.map_or(g, |m| m.max(g)));
        }
    });
    most
}

/// `largest_prefix_group` for one Incremental Sort `s`.
fn prefix_group(mcx: mcx::Mcx<'_>, stmt: &types_nodes::plannodes::PlannedStmt<'_>, s: &types_nodes::plannodes::IncrementalSort<'_>) -> Option<u64> {
    const STATISTIC_KIND_MCV: i16 = 1;
    if s.nPresortedCols < 1 {
        return None;
    }
    let child = s.sort.plan.lefttree?;
    let plan = child.as_plan()?;
    let rows = plan.plan_rows;
    if !(rows.is_finite() && rows >= 1.0) {
        return None;
    }
    let col = usize::try_from(*s.sort.sortColIdx.first()?).ok()?.checked_sub(1)?;
    let var = plan.targetlist.iter().nth(col)?.as_target_entry()?.expr.as_var()?;
    let scan = child.as_seq_scan().map(|x| x.scan.scanrelid).or_else(|| child.as_index_scan().map(|x| x.scan.scanrelid)).or_else(|| child.as_index_only_scan().map(|x| x.scan.scanrelid)).or_else(|| child.as_bitmap_heap_scan().map(|x| x.scan.scanrelid))?;
    if var.varlevelsup != 0 || var.varattno <= 0 || i64::from(var.varno) != i64::from(scan) {
        return None;
    }
    let rte = stmt.rtable.iter().nth(usize::try_from(scan).ok()?.checked_sub(1)?)?.as_range_tbl_entry()?;
    let b = syscache_seams::lookup_pg_statistic_bundle::call(mcx, rte.relid, var.varattno, false).ok().flatten()?;
    let mcv = b.slots.iter().filter(|x| x.kind == STATISTIC_KIND_MCV).filter_map(|x| x.numbers().ok()).flat_map(|n| n.iter().copied()).fold(None, |m: Option<f32>, f| Some(m.map_or(f, |m| m.max(f))));
    let share = match (mcv, b.stadistinct as f64) {
        (Some(f), _) => f as f64,
        (None, d) if d >= 1.0 => 1.0 / d,
        (None, d) if d < 0.0 => 1.0 / (-d * rows).max(1.0),
        _ => return None,
    };
    Some((share.clamp(0.0, 1.0) * rows).ceil() as u64)
}

/// NEW-50: `raise` admitted only when the pool seats it at once and stays at most half held with it: what the pool holds
/// now plus the raise's regions within half its big lane (the half DR-7's raise leaves the statements beside it), and an
/// in-flight slot free (else `None`, nothing tried); a reservation the pool hands back other than `raise` whole and with
/// its slot (another statement took the bytes in between) is given back. So a raise never waits, halves or refuses, and a
/// pool under pressure takes none: the statement then asks what it asked before (`want`, the queue's way).
fn raise_now(pool: &gravity_driver::GrantPool, raise: gravity_driver::Grant, need: &dyn Fn(gravity_driver::Grant) -> u64) -> Option<gravity_driver::Reservation> {
    let gated = pool.max_inflight() != 0;
    if pool.capacity() != 0 {
        let half = gravity_program::cost::fraction(pool.capacity().saturating_sub(pool.reserve()), 1, 2);
        if (gated && pool.inflight() >= pool.max_inflight()) || pool.held().saturating_sub(pool.small_held()).saturating_add(need(raise)) > half {
            return None;
        }
    }
    let a = pool.admit_need_until(raise, need, core::time::Duration::ZERO, &|| false).ok()?;
    (a.grant == raise && (!gated || a.holds_slot())).then_some(a)
}

fn granted(a: gravity_driver::Reservation, estimate: u64, mode: GrantMode) -> GrantLease {
    a.set_pool_fraction(grant_knobs().pool_ppm);
    let pool = &gravity_driver::POOL;
    if a.grant != a.asked || a.waited_ms > 0 || a.how == gravity_driver::Admitted::Solo {
        let s = pool.stats();
        let _ = elog::ereport(types_error::LOG).errmsg(format!("gravity: grant {} MiB of {} MiB asked after {} ms (pool held {} of {} MiB, in flight {}/{}, waits {}, sheds {}; {})", a.grant.bytes >> 20, a.asked.bytes >> 20, a.waited_ms, s.held >> 20, s.capacity >> 20, s.inflight, s.max_inflight, s.waits, s.sheds, a.how.name())).finish(types_error::ErrorLocation::new("gravity", 0, "grant")); // alloc: Cold
    }
    GrantLease::live(std::rc::Rc::new(a), estimate, mode) // alloc: Exec charged GrantLease::held
}

/// D622 floor past the pool: the least `program` at `width` can be admitted with (its floor grant's need) is more than the
/// pool's big lane holds when empty. D3034 (f3) runs such a statement narrower where a narrower width fits
/// (`narrow_past_pool`); D3380: one that does not fit at any width runs alone past the pool (`gravity_driver::Admitted::Solo`).
pub(crate) fn floor_past_pool(program: &gravity_program::Program, width: u16) -> bool {
    arm_grant_pool();
    let pool = &gravity_driver::POOL;
    if pool.capacity() == 0 {
        return false;
    }
    // cheap first: the floor grant itself fits (the least grant tried is at most it, under the same ceilings: a breaker's
    // ceiling may give the leader more than the floor grant's split, so a grant without them can fit where the one tried
    // does not), so no estimate is asked
    let room = pool.capacity().saturating_sub(pool.reserve());
    let cap = statement_cap(width, &grant_knobs());
    if !pool.past_lane(gravity_driver::footprint(cap.trimmed(gravity_driver::GRANT_FLOOR.min(cap.bytes)), program, width.max(1)), room) {
        return false;
    }
    let a = ask(program, &program.pipelines, width, None);
    // the least grant the pool's halving tries (GrantPool::admit_need_until): the floor, or the ask when it is under it
    let floor = a.want.trimmed(gravity_driver::GRANT_FLOOR.min(a.want.bytes));
    let need = gravity_driver::footprint(floor, program, width.max(1));
    // D3804: past the lane beside the kept holds (`GrantPool::past_lane`), as the pool's floor road tests it
    pool.past_lane(need, pool.capacity().saturating_sub(pool.reserve())) && a.small.is_none_or(|g| gravity_driver::footprint(g, program, width.max(1)) > pool.reserve())
}

/// D3034 (f3): `width` halved until the floor of `program` fits the pool (`floor_past_pool`), so a statement whose floor
/// need at its granted width is past the pool runs narrower in Gravity instead of being refused to the row engine (a
/// narrower width never changes the rows); `width` itself when it fits, or when even one worker's floor does not (the
/// refusal stands). Each narrowing is counted (`narrowed`, the grant report line's field).
pub(crate) fn narrow_past_pool(program: &gravity_program::Program, width: u16) -> u16 {
    if width < 2 || !floor_past_pool(program, width) {
        return width;
    }
    let mut w = width / 2;
    while w >= 1 {
        if !floor_past_pool(program, w) {
            if crate::with_backend(|b| b.narrow.replace((width, w))).is_some_and(|was| was.is_none()) {
                NARROWED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
            return w;
        }
        w /= 2;
    }
    width
}

/// D3034 (f3): forget a narrowing no lease reported (a statement refused after its width was asked).
pub(crate) fn clear_narrow() {
    let _ = crate::with_backend(|b| b.narrow = None);
}

/// D3034 (f3): the statements this process ran narrower than their granted width because the floor at that width was past
/// the pool (`narrow_past_pool`): the grant report line's `narrowed` count.
static NARROWED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);


/// D622: the grants a nested statement reserved at its ExecutorStart, one per admission site it will reach (`runner::grant_sites`:
/// its InitPlans, its producers or completion run, its Append arms), each for its program at its width; `hand` gives them to
/// the sites while the statement runs.
#[derive(Default)]
pub struct NestedGrants(Vec<(usize, u16, GrantLease)>);

/// The sites' grants together, none waited for (a nested statement never queues on the pool: its caller holds a grant, and
/// waiting on it is a self-wait). D622 (A): a site whose grant does not fit beside its caller's
/// takes its floor past the pool (`overdraw`), never refused, as PostgreSQL runs the nested statement at work_mem (it has no
/// pool; without the row executor the refusal was a load-dependent 0A000 where PostgreSQL answers); (b): a site
/// whose floor is past the pool's capacity too (a top-level statement's runs alone past the pool, D3380). Logged once per client
/// statement with both roads named (`note_overdraw`); the grant report counts them (`nested_past`).
pub fn reserve_sites(sites: &[(std::sync::Arc<gravity_program::Program>, u16)]) -> NestedGrants {
    let mut out = NestedGrants::default();
    let mut past = Overdrawn::default();
    for (program, width) in sites {
        let lease = reserve_now(program, *width).unwrap_or_else(|| past.take(program, *width));
        out.0.push((std::sync::Arc::as_ptr(program) as usize, *width, lease));
    }
    past.note(sites.len());
    out
}

/// D622 (A): a nested statement's overdrawn sites by road (beside its caller's grant, or a floor past the pool's capacity)
/// and their bytes.
#[derive(Default)]
struct Overdrawn {
    caller: usize,
    capacity: usize,
    bytes: u64,
}

impl Overdrawn {
    /// `program`'s floor at `width` past the pool (`overdraw`), counted by its road.
    fn take(&mut self, program: &gravity_program::Program, width: u16) -> GrantLease {
        if floor_past_pool(program, width) {
            self.capacity += 1;
        } else {
            self.caller += 1;
        }
        let (lease, need) = overdraw(program, width);
        self.bytes += need;
        lease
    }

    fn note(&self, sites: usize) {
        if self.caller + self.capacity > 0 {
            note_overdraw(self, sites);
        }
    }
}

/// D622 (A): `program`'s floor grant at `width` taken past the pool (`GrantPool::admit_overdrawn`, `Overdraw::Nested`): the
/// least grant the pool's halving tries (GrantPool::admit_need_until), the floor or the ask when it is under it, as
/// `floor_past_pool` reads it. The lease and its need.
fn overdraw(program: &gravity_program::Program, width: u16) -> (GrantLease, u64) {
    let a = ask(program, &program.pipelines, width, None);
    let floor = a.want.trimmed(gravity_driver::GRANT_FLOOR.min(a.want.bytes));
    let need = gravity_driver::footprint(floor, program, width.max(1));
    (granted(gravity_driver::POOL.admit_overdrawn(floor, need, gravity_driver::Overdraw::Nested), a.estimate, a.mode), need)
}

/// D622 (A): the overdraw's LOG line, once per client statement (a nested statement per row of its caller overdraws per row),
/// naming both roads: beside the caller's grant (the pool's room taken), or a floor past the pool's capacity ((b)).
#[cold]
fn note_overdraw(o: &Overdrawn, sites: usize) {
    let stmt = xact::GetCurrentStatementStartTimestamp();
    if OVERDRAW_LOGGED.with(|l| l.replace(stmt)) == stmt {
        return;
    }
    let _ = elog::ereport(types_error::LOG).errmsg(format!("gravity: D622 overdraw: {} of {sites} grant sites of a nested statement run at their floor past the pool ({} beside its caller's grant, {} with a floor past the pool's capacity; {} KiB)", o.caller + o.capacity, o.caller, o.capacity, o.bytes >> 10)).finish(types_error::ErrorLocation::new("gravity", 0, "grant")); // alloc: Cold
}

/// A grant kept for `program`'s rerun at `width` (`Producers::into_kept`: a rewound portal's).
pub(crate) fn kept(program: &std::sync::Arc<gravity_program::Program>, width: u16, lease: GrantLease) -> NestedGrants {
    NestedGrants(vec![(std::sync::Arc::as_ptr(program) as usize, width, lease)]) // alloc: Cold
}

/// One site's grant for `program` at `width`, none waited for (the small lane, else the pool's halving at wait 0).
fn reserve_now(program: &gravity_program::Program, width: u16) -> Option<GrantLease> {
    let pool = &gravity_driver::POOL;
    let ask = ask(program, &program.pipelines, width, None);
    let need = &|g: gravity_driver::Grant| gravity_driver::footprint(g, program, width.max(1));
    // D4403: a small site takes the small buffer first (no wait: a nested site never waits)
    let a = match small_ask(pool, &ask, need).and_then(|g| pool.admit_small_need(g, need(g))) {
        Some(a) => a,
        None => pool.admit_need(ask.want, need, core::time::Duration::ZERO).ok()?,
    };
    Some(granted(a, ask.estimate, ask.mode))
}

leader_tls::leader_tls! {
    thread_local! {
        /// D622: the leases of the statements running on this thread (`GrantLease::running`; a suspended portal's is idle):
        /// read at a statement's ExecutorStart, where more than none means its caller holds a grant. Gravity's pool threads
        /// never start a statement (a user-defined function's site is pool-unsafe, lowerv4 hir.rs `pool_unsafe_site`: a
        /// nested statement starts on the leader only), so the count is the leader's own chain of levels.
        static RUNNING: core::cell::Cell<u32> = const { core::cell::Cell::new(0) };
        /// D622: per reentry level, the grants `hand` gives the admission sites of the statement running at that level.
        /// Its block is on the per-backend holder from its first growth (`handed`): the thread keeps it across statements
        /// and sessions (a pooled thread), so no statement owns it.
        static HANDED: core::cell::RefCell<memledger::ChargedVec<Option<std::rc::Rc<core::cell::RefCell<NestedGrants>>>>> = const { core::cell::RefCell::new(memledger::ChargedVec::none()) }; // alloc: Exec charged crate::BACKEND_OWNER
        /// D622 (A): the client statement (its start timestamp) whose nested overdraw was last logged (`note_overdraw`): a log
        /// throttle, not session state.
        static OVERDRAW_LOGGED: core::cell::Cell<i64> = const { core::cell::Cell::new(i64::MIN) };
    }
}

/// A statement starting now runs under a caller's grant (D622): a statement running on this thread holds a lease.
pub fn caller_granted() -> bool {
    RUNNING.with(|r| r.get() > 0)
}

/// `f` with `grants` handed to the admission sites of the statement running at reentry level `level` (its open, its run).
/// D3620: inlined, so a nested statement's stack path holds no frame of its own for it.
#[inline(always)]
pub fn hand<R>(level: usize, grants: Option<&std::rc::Rc<core::cell::RefCell<NestedGrants>>>, f: impl FnOnce() -> R) -> R {
    let Some(grants) = grants else { return f() };
    let _restore = handed(level, grants);
    f()
}

/// `hand`'s handing: `grants` at `level`, the previous ones kept in the guard. D3620: out of line, as the guard's drop, so
/// the caller's frame (on every nested statement's stack path) holds the guard alone.
#[inline(never)]
fn handed(level: usize, grants: &std::rc::Rc<core::cell::RefCell<NestedGrants>>) -> Restore {
    let prev = HANDED.with(|h| {
        let mut h = h.borrow_mut();
        if h.len() <= level {
            if h.owner().is_none() && h.capacity() == 0 {
                *h = memledger::ChargedVec::new(Some(crate::BACKEND_OWNER), memledger::Kind::Region); // alloc: Exec charged crate::BACKEND_OWNER
            }
            h.with_vec(|h| h.resize(level + 1, None)); // alloc: Exec charged crate::BACKEND_OWNER
        }
        h[level].replace(grants.clone())
    });
    Restore(level, prev)
}

/// `hand`'s guard: the level's previous grants back on every exit, an unwind's included, so no step's grants outlive it.
struct Restore(usize, Option<std::rc::Rc<core::cell::RefCell<NestedGrants>>>);

impl Drop for Restore {
    #[inline(never)]
    fn drop(&mut self) {
        let prev = self.1.take();
        let replaced = HANDED.with(|h| h.borrow_mut().get_mut(self.0).map(|slot| core::mem::replace(slot, prev)));
        drop(replaced);
    }
}

/// The grant handed to this level's statement for `program`, with the width its site runs at: `None` when the level's
/// statement was given none (it queues as any statement does); Reason::Bug (XX000) when it was and none is left for this
/// program (a site `grant_sites` missed, never a wait). A parallel site's width is the run's (`statement_width`): when it
/// is not the one reserved at the start (a combo CID the transaction made since leaves the leader alone; a SET of the
/// worker GUCs between a cursor's OPEN and its FETCH), the grant is reserved again at the run's width, none waited for,
/// beside the one reserved. When that does not fit, a `narrow` site wider than its reservation keeps it and runs at the
/// reserved width; otherwise the grant is reserved again in the reserved one's place, and when that does not fit either
/// the site takes its floor at the run's width past the pool (D622 (A) `overdraw`: a nested site never waits on room its
/// own caller holds; it queued here, and a fetch at another width than its OPEN's ended 53200 out of memory).
fn handed_lease(program: &gravity_program::Program, width: u16, narrow: bool) -> Option<Result<(GrantLease, u16), Box<types_error::PgError>>> {
    let level = crate::admit::reentry::current()?;
    let grants = HANDED.with(|h| h.borrow().get(level).cloned().flatten())?;
    let addr = program as *const gravity_program::Program as usize;
    let taken = {
        let mut g = grants.borrow_mut();
        g.0.iter().position(|(p, _, _)| *p == addr).map(|i| g.0.swap_remove(i))
    };
    let Some((_, reserved, lease)) = taken else {
        return Some(Err(crate::glue::internal("gravity grant: Reason::Bug: a nested statement's grant site has no reserved grant (D622 grant_sites)")));
    };
    if reserved == width {
        return Some(Ok((lease, width)));
    }
    if let Some(again) = reserve_now(program, width) {
        return Some(Ok((again, width)));
    }
    if narrow && width > reserved {
        let _ = elog::ereport(types_error::LOG).errmsg(format!("gravity: D622 a nested cursor's fetch at width {width} runs at the width its grant was reserved for ({reserved}): the wider grant does not fit beside its caller's")).finish(types_error::ErrorLocation::new("gravity", 0, "grant")); // alloc: Cold
        return Some(Ok((lease, reserved)));
    }
    drop(lease);
    let lease = reserve_now(program, width).unwrap_or_else(|| {
        let mut past = Overdrawn::default();
        let lease = past.take(program, width);
        past.note(1);
        lease
    });
    Some(Ok((lease, width)))
}

/// D4403: the grant a small statement asks of the small buffer: the lanes slice's ask (`Ask::small`), else, with a buffer,
/// the unchanged ask when its reservation is small (`mempolicy::admit::small_statement`: its grant never trimmed, so its
/// plan and spills stay as they were); `None`: the big lane's road.
fn small_ask(pool: &gravity_driver::GrantPool, ask: &Ask, need: &dyn Fn(gravity_driver::Grant) -> u64) -> Option<gravity_driver::Grant> {
    ask.small.or_else(|| (pool.reserve() != 0 && mempolicy::admit::small_statement(small_size(ask.want.grow, need(ask.want), ask.estimate))).then_some(ask.want))
}

/// What the small buffer's test reads of a statement whose start's reservation is `need`: `need`; under grow mode
/// (`grow`) the larger of it and its estimate, so a statement big by its estimate that starts small (its breakers grow past
/// the start) is no small statement and takes the big lane's road.
fn small_size(grow: bool, need: u64, estimate: u64) -> u64 {
    if grow { need.max(estimate) } else { need }
}

/// D4403: `GrantPool::admit_small_until` through the backend's interrupts, as `admit_waiting`; `None` when the buffer does
/// not apply (no buffer, or a need past it: the big lane's road follows).
/// D4414: a memory-canceled statement's small ask that finds the buffer full is refused with the cancel (`memory_canceled`,
/// as `admit_waiting` refuses it, D4402 B) where its wait would begin: the pool asks `stop` only after the buffer's and the
/// big lane's takes both failed, so with room the buffer still admits it (`take_small` reads no ledger), as before.
/// Public for its integration test (tests/d4414_canceled_small_admission.rs: the grant pool is process-wide).
#[doc(hidden)]
pub fn admit_small_waiting(pool: &gravity_driver::GrantPool, want: gravity_driver::Grant, need: &dyn Fn(gravity_driver::Grant) -> u64, wait: core::time::Duration) -> Result<Option<Result<gravity_driver::Reservation, gravity_driver::AdmitRefused>>, Box<types_error::PgError>> {
    use cancelpoll::interrupts;
    let mut left = wait;
    let mut abort_only = false;
    // off once a cancel the governor's refusal does not raise (an exempt statement) woke the wait: it waits as before
    let mut canceled_ends = true;
    loop {
        let wake = || interrupts::pending_abort() || (!abort_only && interrupts::pending()) || (canceled_ends && mcx::governor::watchdog_cancel_pending());
        match pool.admit_small_until(want, need, left, &wake) {
            Some(Err(r)) if r.interrupted => {
                interrupts::check()?;
                if interrupts::pending() {
                    interrupts::check()?;
                }
                if let Some(e) = memory_canceled() {
                    return Err(e);
                }
                canceled_ends &= !mcx::governor::watchdog_cancel_pending();
                abort_only = interrupts::flagged();
                left = left.saturating_sub(core::time::Duration::from_millis(r.waited_ms));
                if left.is_zero() {
                    return Ok(Some(Err(r)));
                }
            }
            r => return Ok(r),
        }
    }
}

/// The pool's wait under the backend's interrupts (D599, D623): a statement queued for its slot or bytes ends at a pending
/// interrupt as PostgreSQL's waits do (ProcessInterrupts: a statement timeout or cancel raises 57014, a termination 57P01).
/// The wait wakes only for an interrupt that can be processed now (`INTERRUPTS_CAN_BE_PROCESSED`); held off, it waits on,
/// as PostgreSQL's waits under HOLD_INTERRUPTS do. An interrupt that raises nothing (a sinval catch-up, the idle-stats
/// timer mid-statement) goes back to waiting out what is left of `wait` (the refusal's `waited_ms`), still checked: D623's
/// held-off branch took a pending InterruptPending for a hold-off and waited the rest unchecked, but ProcessInterrupts runs
/// the pended timeout handlers after its clear (D355), so the idle-stats timer's handler re-raises InterruptPending with
/// nothing held off. A second check consumes that re-raise (its handler already ran). A flag still pending after it neither
/// ends the wait nor raises anything: the wait goes on checked, woken next (no spin) by the abort-class byte a cancel, a
/// timeout or a termination sets (`AbortPending`). Every ERROR here is ProcessInterrupts' own or the pool's refusal at `wait`.
/// D4402: or a memory-canceled statement's cancel (`memory_canceled`), raised before any wait and after each interrupt.
/// Public for its integration test (tests/d4402_canceled_admission.rs: the grant pool is process-wide, so the test that
/// arms it runs in its own binary, never beside the lib's tests that read its capacity).
#[doc(hidden)]
pub fn admit_waiting(pool: &gravity_driver::GrantPool, want: gravity_driver::Grant, need: &dyn Fn(gravity_driver::Grant) -> u64, wait: core::time::Duration) -> Result<Result<gravity_driver::Reservation, gravity_driver::AdmitRefused>, Box<types_error::PgError>> {
    use cancelpoll::interrupts;
    if let Some(e) = memory_canceled() {
        return Err(e);
    }
    let mut left = wait;
    let mut abort_only = false;
    loop {
        let wake = || interrupts::pending_abort() || (!abort_only && interrupts::pending());
        match pool.admit_need_until(want, need, left, &wake) {
            Err(r) if r.interrupted => {
                interrupts::check()?;
                if interrupts::pending() {
                    interrupts::check()?;
                }
                if let Some(e) = memory_canceled() {
                    return Err(e);
                }
                abort_only = interrupts::flagged();
                left = left.saturating_sub(core::time::Duration::from_millis(r.waited_ms));
                if left.is_zero() {
                    return Ok(Err(r));
                }
            }
            r => return Ok(r),
        }
    }
}

/// D4402: the cancel's 53200 (its ledger's text, `mcx::governor::refusal`) when the memory governor canceled this
/// thread's statement (the ladder's R6, the watchdog), else `None`. Such a statement's ledger refuses every grant byte
/// until it ends (`fits_raw_admit`), so no wait can admit it, and the cancel's interrupt may be consumed already: an
/// errorhold re-run after the first run's poll raised it, a PL/pgSQL handler that caught it and queries again. Its
/// admission waited for room it could never take, through `pgrust.gravity_grant_wait_ms`, and the cancel went
/// unanswered to statement_timeout; it is refused now with the cancel's own error.
fn memory_canceled() -> Option<Box<types_error::PgError>> {
    if !mcx::governor::watchdog_cancel_pending() {
        return None;
    }
    mcx::governor::refusal("Gravity grant (canceled)").map(Box::new) // alloc: Cold
}

/// A statement the runner does not admit (the pull, F8): its grant from the GUCs, nothing reserved.
pub fn unadmitted(width: u16) -> std::rc::Rc<gravity_driver::Reservation> {
    std::rc::Rc::new(gravity_driver::Reservation::unadmitted(granted_memory(width)))
}

/// A serial, breakerless, single-pipeline program: it has nothing to reserve, so it runs outside the pool on either protocol
/// (H16 (a), main 09-24: the extended pull's F8 posture, 1.12 µs of GUC reads and pool admission a simple statement).
pub(crate) fn breakerless(program: &gravity_program::Program) -> bool {
    program.pipelines.len() == 1 && program.pipelines[0].breaker().is_none()
}

/// D3865 L4: a program of one emit and InitPlans that run on demand inside its reader's call (nest.initplan's Init road,
/// `runs_nested`), no pipeline ending in a breaker and none eligible, has nothing more to reserve than a `breakerless` one:
/// each InitPlan is one pipeline folding into its Init cell. It runs outside the pool too (H16 (a)), so a nested statement
/// whose body holds an InitPlan (obj_description's pg_class lookup) reserves no grant at each call. `bitmap`: a site of
/// the program builds a bitmap (the runner's, drawn at its pipeline's start): never outside the pool.
pub(crate) fn inits_only(program: &gravity_program::Program, bitmap: bool) -> bool {
    use gravity_program::protos::ProtoKind;
    let ps = &program.pipelines;
    let Some(d) = program.v4.as_deref() else { return false };
    let init = |q: usize| d.protos.iter().any(|p| p.kind == ProtoKind::Init && p.pipeline as usize == q && crate::ops::subplan::initplan::init::runs_nested(p, program));
    !bitmap && ps.len() > 1 && ps.iter().all(|p| p.breaker().is_none() && !p.is_eligible() && !p.post) && (0..ps.len()).filter(|&q| !init(q)).count() == 1
}

/// The pull's reservation for `program`: strict when it is `breakerless` (a region drawn under it is XX000, the premise
/// guarded), else the plain unadmitted one.
pub fn unadmitted_for(program: &gravity_program::Program, width: u16) -> std::rc::Rc<gravity_driver::Reservation> {
    if breakerless(program) {
        std::rc::Rc::new(gravity_driver::Reservation::unadmitted_strict(granted_memory(width)))
    } else {
        unadmitted(width)
    }
}

pub(crate) struct GrantLease {
    /// `res`'s `Rc` block on the owner the lease starts under (`memledger::charge::context()`: the portal it runs in, else
    /// its statement), fixed: a cursor's portal keeps its lease past the statement. FIRST field: a door-1 charge drops
    /// before the bytes it counts are freed.
    held: memledger::Charge,
    res: std::rc::Rc<gravity_driver::Reservation>,
    estimate: u64,
    mode: GrantMode,
    /// D622: counted in `RUNNING` (a lease holding pool bytes whose statement runs, not a suspended portal's)
    running: core::cell::Cell<bool>,
    /// Step 3a: the statement's feedback entry (`mem::feedback::begin`), given back and reported when the lease drops.
    feedback: Option<u64>,
}

/// D562: the live leases (count, reserved bytes), process-wide as POOL is. A cursor's portal keeps its lease across the
/// statements of its transaction, so the pool holds other live leases' bytes beside a statement's own: the grant end line
/// names them (`leases n of k KiB`, its own included), and rig.py's leak check reads them.
static LIVE: [core::sync::atomic::AtomicU64; 2] = [core::sync::atomic::AtomicU64::new(0), core::sync::atomic::AtomicU64::new(0)];

impl GrantLease {
    fn live(res: std::rc::Rc<gravity_driver::Reservation>, estimate: u64, mode: GrantMode) -> GrantLease {
        LIVE[0].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        LIVE[1].fetch_add(res.reserved, core::sync::atomic::Ordering::Relaxed);
        // the block every caller made just before on this thread (`Rc::new`) moves to the lease's owner
        let (o, release) = memledger::charge::context();
        let held = memledger::Charge::open(o, release, memledger::Kind::Region, memledger::charge::Door::Heap, memledger::arc_bytes::<gravity_driver::Reservation>());
        let lease = GrantLease { held, res, estimate, mode, running: core::cell::Cell::new(false), feedback: None };
        lease.set_running(true);
        lease
    }

    /// D622: the lease's statement runs (`true`) or sits suspended (`false`, a portal between its fetches: D601's park); only
    /// a lease holding pool bytes counts, so a caller's statement outside the pool never makes its callee's `caller_granted`.
    pub(crate) fn set_running(&self, on: bool) {
        if self.res.reserved > 0 && self.running.replace(on) != on {
            RUNNING.with(|r| r.set(if on { r.get() + 1 } else { r.get().saturating_sub(1) }));
        }
    }

    /// A `breakerless` program's lease on the simple protocol: outside the pool, strict (`unadmitted_for`).
    pub(crate) fn strict(width: u16) -> GrantLease {
        GrantLease::live(std::rc::Rc::new(gravity_driver::Reservation::unadmitted_strict(granted_memory(width))), 0, GrantMode::Unadmitted) // alloc: Exec charged GrantLease::held
    }

    /// D3865 L4: an `inits_only` program's lease: outside the pool, its grant from the GUCs (not strict: an InitPlan's
    /// by-reference value is copied by its cell, never drawn, but nothing here proves every member draws nothing).
    pub(crate) fn unadmitted(width: u16) -> GrantLease {
        GrantLease::live(unadmitted(width), 0, GrantMode::Unadmitted)
    }

    /// D3865 L4: `program`'s lease at `width`: outside the pool when it is `inits_only` (`bitmap`: a site builds a bitmap),
    /// else `admit`'s.
    pub(crate) fn or_admit(program: &gravity_program::Program, bitmap: bool, width: u16, admit: impl FnOnce() -> Result<(GrantLease, u16), Box<types_error::PgError>>) -> Result<(GrantLease, u16), Box<types_error::PgError>> {
        if inits_only(program, bitmap) {
            return Ok((GrantLease::unadmitted(width), width));
        }
        admit()
    }

    pub(crate) fn grant(&self) -> gravity_driver::Grant {
        self.res.grant
    }

    fn with_feedback(mut self, fb: Option<(u64, bool)>) -> GrantLease {
        self.feedback = fb.map(|f| f.0);
        self
    }

    /// The admission for `Entry::start` (the Exec keeps it while its Node draws).
    pub(crate) fn reservation(&self) -> std::rc::Rc<gravity_driver::Reservation> {
        self.res.clone()
    }
}

/// The grant end line's overdrafts (the pool's, process-wide as its other counts): `<site>_past=<n>/<bytes>` per site
/// (`gravity_driver::Overdraw`: D622's nested admissions, D1378's seen sets).
#[cold]
fn overdraws() -> String {
    let o = gravity_driver::POOL.overdraws();
    let mut line = String::new(); // alloc: Cold
    for (i, (s, (n, b))) in gravity_driver::Overdraw::ALL.iter().zip(o).enumerate() {
        if i > 0 {
            line.push(' '); // alloc: Cold
        }
        line.push_str(&format!("{}={n}/{b}", s.name())); // alloc: Cold
    }
    line
}

impl Drop for GrantLease {
    fn drop(&mut self) {
        self.set_running(false);
        if let Some(l) = self.feedback.and_then(gravity_driver::mem::feedback::end) {
            if grant_report() {
                report_feedback(&l);
            }
        }
        let (n, live) = (LIVE[0].fetch_sub(1, core::sync::atomic::Ordering::Relaxed), LIVE[1].fetch_sub(self.res.reserved, core::sync::atomic::Ordering::Relaxed));
        let narrow = crate::with_backend(|b| b.narrow.take()).flatten();
        if grant_report() {
            let s = gravity_driver::POOL.stats();
            let d = gravity_driver::REGIONS.stats();
            let mapped = gravity_driver::mem::spill::mapped_past();
            let r = &self.res;
            let _ = elog::ereport(types_error::LOG).errmsg(format!("gravity: grant end: asked {} MiB granted {} MiB reserved {} MiB estimate {} KiB drawn {} KiB peak {} KiB grown {} KiB charged {} KiB charged_peak {} KiB borrowed {} KiB hosted {} KiB mode {:?} small {} (pool held {} of {} MiB, small lane {} of {} MiB, small admits {} fallbacks {} overflows {}, trims {}, borrows {}; depot {} regions {} KiB, {} maps {} takes; leases {} of {} KiB; mapped past the pool {} draws {} KiB; width {} narrowed {}; ceilings {} / {} KiB, {} admitted, {} in flight; draws exact {} grown {}; overdraw {}; owner pins given back {} KiB)", r.asked.bytes >> 20, r.grant.bytes >> 20, r.reserved >> 20, self.estimate >> 10, r.drawn() >> 10, r.resident_peak() >> 10, r.grown() >> 10, r.charged() >> 10, r.charged_peak() >> 10, r.borrowed() >> 10, r.hosted() >> 10, self.mode, r.small, s.held >> 20, s.capacity >> 20, s.small_held >> 20, s.reserve >> 20, s.small_admits, s.small_fallbacks, s.small_overflows, s.trims, s.borrows, d.entries, d.bytes >> 10, d.allocs, d.takes, n, live >> 10, mapped.0, mapped.1 >> 10, narrow.map_or_else(|| "as granted".to_string(), |(f, t)| format!("{f} -> {t}")), NARROWED.load(core::sync::atomic::Ordering::Relaxed), r.grant.sort_ceil >> 10, r.grant.hash_ceil >> 10, gravity_driver::mem::grant::running(), s.inflight, gravity_driver::mem::grow::draws().0, gravity_driver::mem::grow::draws().1, overdraws(), gravity_driver::OWN_PIN_GIVEN.load(core::sync::atomic::Ordering::Relaxed) >> 10)).finish(types_error::ErrorLocation::new("gravity", 0, "grant")); // alloc: Cold
        }
    }
}

/// Step 3a's report (under the grant end line's switch): the starts found and, per breaker slot and role (L the leader's
/// share, W each worker's), the start given, its source, this run's charged peak and the clean samples' spread.
#[cold]
fn report_feedback(l: &gravity_driver::mem::feedback::Live) {
    use core::fmt::Write;
    let mut s = format!("gravity: grant feedback: seen {} miss {}", l.hits, l.misses); // alloc: Cold
    for k in (0..gravity_driver::mem::feedback::SLOTS).filter(|&k| l.slots & (1 << k) != 0) {
        let sp = gravity_driver::mem::feedback::spreads(l, k as u8);
        for (r, name) in [(0, "L"), (1, "W")] {
            if r == 1 && l.peak[k][1] == 0 && l.start[k][1] == 0 {
                continue;
            }
            let _ = write!(s, "; slot {k} {name} start {} KiB {} peak {} KiB flags {} spread n {} min {} p50 {} p90 {} max {} KiB", l.start[k][r], if l.start[k][r] == 0 { "est" } else { l.source[k][r].name() }, l.peak[k][r], l.flags[k], sp[r].n, sp[r].min, sp[r].p50, sp[r].p90, sp[r].max); // alloc: Cold
        }
    }
    let _ = elog::ereport(types_error::LOG).errmsg(s).finish(types_error::ErrorLocation::new("gravity", 0, "grant"));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// E-hold: the handed grants' stack is the thread's, kept across statements, so it is on the per-backend holder from
    /// its first growth, by its capacity.
    #[test]
    fn e_hold_the_handed_stack_is_on_the_per_backend_holder() {
        HANDED.with(|h| assert_eq!(h.borrow().owner(), None, "no growth yet: uncharged"));
        let g = std::rc::Rc::new(core::cell::RefCell::new(NestedGrants::default()));
        {
            let _restore = handed(3, &g);
            HANDED.with(|h| {
                let h = h.borrow();
                assert_eq!(h.owner(), Some(crate::BACKEND_OWNER));
                assert_eq!(h.charged_bytes(), (h.capacity() * core::mem::size_of::<Option<std::rc::Rc<core::cell::RefCell<NestedGrants>>>>()) as u64);
                assert!(h.len() >= 4);
            });
        }
    }

    fn knobs(grow: bool, work_mem_set: bool) -> GrantKnobs {
        GrantKnobs { mode: GrantMode::Estimate, grant_mb: None, wait_ms: 600_000, floor_mb: 64, estimate_scale: 1, small_mb: 64, pool_ppm: gravity_driver::POOL_FRACTION_DEFAULT, feedback: Feedback::Memory, spill_every: 0, revoke_every: 0, refuse_debt: false, grow, work_mem_set }
    }

    /// Grow mode's admission grant (`grow_cap`): off, the GUCs' grant as it is (flag off = today's, byte for byte); on with
    /// work_mem unset, no work_mem ceiling (the region wall, at most the pool's big lane, never 0); on with an explicit
    /// work_mem, PostgreSQL's per-node ceilings stand.
    #[test]
    fn grow_mode_lifts_the_work_mem_ceiling_only_while_work_mem_is_unset() {
        const MIB: u64 = 1 << 20;
        let cap = gravity_driver::Grant::of(64 * MIB).with_floors(4 * MIB, 8 * MIB).with_ceilings(4 * MIB, 8 * MIB);
        assert_eq!(grow_cap(cap, &knobs(false, false)), cap, "off: today's grant");
        assert_eq!(grow_cap(cap, &knobs(false, true)), cap, "off: today's grant");
        let set = grow_cap(cap, &knobs(true, true));
        assert_eq!((set.grow, set.sort_ceil, set.hash_ceil), (true, 4 * MIB, 8 * MIB), "explicit work_mem: its ceilings");
        let unset = grow_cap(cap, &knobs(true, false));
        assert!(unset.grow && unset.sort_ceil == unset.hash_ceil, "one ceiling for every kind");
        assert!(unset.hash_ceil > 8 * MIB && unset.hash_ceil <= gravity_driver::mem::grow::MAX_REGION, "no work_mem ceiling: {}", unset.hash_ceil);
        assert_eq!((unset.sort_floor, unset.hash_floor, unset.bytes), (cap.sort_floor, cap.hash_floor, cap.bytes), "the floors stay the budgets (the start's caps), the cap stays");
    }

    /// D4403 under grow mode: a statement big by its estimate is no small statement however small its start.
    #[test]
    fn a_statement_big_by_its_estimate_never_takes_the_small_buffer_by_starting_small() {
        const MIB: u64 = 1 << 20;
        assert_eq!(small_size(false, 4 * MIB, 900 * MIB), 4 * MIB, "off: its reservation, as today");
        assert_eq!(small_size(true, 4 * MIB, 900 * MIB), 900 * MIB, "grow: its estimate");
        assert_eq!(small_size(true, 4 * MIB, MIB), 4 * MIB, "grow: a small estimate, its start");
    }

    /// D4064 (C8 area move 4): the budget's shared_buffers row reads the pool's live size as bufmgr publishes it at every
    /// resize (`publish_active_buffers`; CALL pgrust_buffer_pool_resize, up to pgrust.buffer_pool_max). Its current follows
    /// a pool grown past the configured bytes and never reads below them; floor and cap stay the configured bytes. A grown
    /// pool used to read as free room to B and to the work pool's capacity (B0's tick could over-admit by the growth).
    #[test]
    fn d4064_the_shared_buffers_row_follows_a_resized_pool() {
        register_budget_consumers(128 << 20);
        let h = budget::hooks(budget::Consumer::SharedBuffers).expect("the shared_buffers row is registered");
        let configured = SHARED_BYTES.load(core::sync::atomic::Ordering::Relaxed);
        // grown to 4 GiB (524,288 buffers of 8 KiB)
        bufmgr_seams::publish_active_buffers(524_288);
        let grown = (h.current)();
        bufmgr_seams::publish_active_buffers(1);
        let shrunk = (h.current)();
        bufmgr_seams::publish_active_buffers(0);
        assert_eq!(grown, configured.max(4 << 30), "a grown pool's bytes are held");
        assert_eq!(shrunk, configured.max(8192), "never below the configured bytes");
        assert_eq!(((h.floor)(), (h.cap)()), (configured, configured), "floor and cap stay the configured bytes");
    }

    /// The rail's reserve reads this thread's stack mapping: a spawned thread's mapping holds at least the stack it asked.
    #[test]
    #[cfg(target_os = "linux")]
    fn the_stack_mapping_holds_the_threads_stack() {
        let read = std::thread::Builder::new().stack_size(8 << 20).spawn(own_stack_mapping).unwrap().join().unwrap();
        assert!((8 << 20..1 << 30).contains(&read), "{read}");
    }

    /// e2e v4/m1_grant_knobs shows SET, a SET LOCAL's rollback and RESET each reaching the next statement's grant.
    #[test]
    fn a_read_only_when_the_mutation_count_moved() {
        let s = core::cell::Cell::new(None);
        let k = |mode, floor_mb| GrantKnobs { mode, grant_mb: None, wait_ms: 600_000, floor_mb, estimate_scale: 1, small_mb: 64, pool_ppm: gravity_driver::POOL_FRACTION_DEFAULT, feedback: Feedback::Off, spill_every: 0, revoke_every: 0, refuse_debt: false, grow: false, work_mem_set: false };
        let (flat, lanes) = (k(GrantMode::Flat, 128), k(GrantMode::Lanes, 256));
        assert_eq!(snapshot(&s, 7, || flat), flat);
        assert_eq!(snapshot(&s, 7, || unreachable!("the count did not move")), flat);
        assert_eq!(snapshot(&s, 8, || lanes), lanes);
        assert_eq!(snapshot(&s, 8, || unreachable!("the count did not move")), lanes);
        assert_eq!(snapshot(&s, 7, || flat), flat);
    }

    /// DR-7: a defaulted statement's bound follows the rows its breakers receive: a 1,000-row input stays under the 64 MiB
    /// floor, a 2,000,000-row one reaches past a 1 GiB cap at W8 (each of the 9 shares may hold every row's record); a
    /// pipeline without a share breaker adds nothing.
    #[test]
    fn the_rows_bound_follows_the_rows_a_breaker_receives() {
        use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
        let agg = |rows| gravity_program::Pipeline::eligible(0, gravity_program::End::Breaker(Breaker { spill: Spill::Partitioned { bits: 3 }, budget: Budget::Share, ..Breaker::sized(BreakerKind::HashAgg, 0) }), rows);
        assert!(rows_bound(&[agg(1_000)], 8) < 64 << 20);
        assert!(rows_bound(&[agg(2_000_000)], 8) > 1 << 30);
        assert_eq!(rows_bound(&[gravity_program::Pipeline::eligible(0, gravity_program::End::Sink, 2_000_000)], 8), 0);
    }

    /// D3132 (B): `P'` (a pass over the probe's slot's spill partitions, the probe's rows copied) adds nothing to the bound
    /// its probe sets: a count-of-counts query's W0 ask (outer GROUP BY over a count, defaulted) stays where it was
    /// before D3057 gave its keyed group a `P'` (1280 MiB, not 2304 MiB, at the perf bar's 8192 MB cap).
    #[test]
    fn a_spill_partitions_pass_adds_nothing_to_the_rows_bound() {
        use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
        let agg = |rows| gravity_program::Pipeline::eligible(1, gravity_program::End::Breaker(Breaker { spill: Spill::Partitioned { bits: 7 }, budget: Budget::Share, ..Breaker::sized(BreakerKind::HashAgg, 2) }), rows);
        let probe = agg(14_998_500);
        let pair = agg(14_998_500).over_spill_partitions(0);
        for w in [0, 8] {
            assert_eq!(rows_bound(&[probe, pair], w), rows_bound(&[probe], w), "W{w}");
            assert!(rows_bound(&[probe], w) > 0);
        }
    }

    /// D2845 / D2861: the rail keeps every admitting backend's admission arena (~1 GiB of address space) beside the slots'
    /// stacks and the thread arenas; the derived memory pool holds a mapped arena's resident prefix (64 MiB) instead.
    #[test]
    fn the_admission_arena_counts_on_the_rail_it_loads() {
        const M: u64 = 1 << 20;
        assert!((1023 * M..=1024 * M).contains(&ADMISSION_ARENA), "{ADMISSION_ARENA}");
        use mempolicy::sizes::{keeps_arenas, rail_reserve};
        assert_eq!(rail_reserve(140, 116, 8 * M, 16, ADMISSION_ARENA), 140 * 8 * M + 128 * 64 * M + 116 * ADMISSION_ARENA);
        assert_eq!(rail_reserve(100, 100, 8 * M, 16, ADMISSION_ARENA), 100 * 8 * M + 100 * 64 * M + 100 * ADMISSION_ARENA, "under 8 thread arenas a CPU");
        assert_eq!(ADMISSION_KEPT, 64 * M);
        assert!(keeps_arenas(false, false, false, 22_844 * M), "the derived memory pool holds each mapped arena's resident prefix");
        assert!(!keeps_arenas(true, false, false, 4_000 * M), "a rail's pool keeps the arenas whole up front");
        assert!(!keeps_arenas(false, true, false, 256 * M), "an explicit capacity bounds statements alone");
        assert!(!keeps_arenas(false, false, true, 5_162 * M), "a pool R5 fitted inside B bounds statements alone");
        assert!(!keeps_arenas(false, false, false, 0), "unarmed: nothing held");
    }

    /// DR-7's raise fits half the pool's big lane: halved until its reservation does, never under the ordinary ask.
    #[test]
    fn the_raise_stays_within_half_the_pool() {
        const M: u64 = 1 << 20;
        let need = |g: u64| g + 1536 * M; // the grant + its regions, about what a W8 statement reserves at work_mem 64MB
        assert_eq!(raise_within(112 * M, 1024 * M, u64::MAX, &need), 1024 * M, "unarmed: the raise");
        assert_eq!(raise_within(112 * M, 1024 * M, 22844 * M / 2, &need), 1024 * M, "the default pool's half seats it");
        assert_eq!(raise_within(112 * M, 1024 * M, 2048 * M, &need), 512 * M, "a 4 GiB pool: halved to fit");
        assert_eq!(raise_within(112 * M, 1024 * M, 1024 * M, &need), 112 * M, "a 2 GiB pool: the ordinary ask");
        assert_eq!(raise_within(112 * M, 64 * M, u64::MAX, &need), 112 * M, "never under the ordinary ask");
    }
}
