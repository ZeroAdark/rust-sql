//! Shared knobs of the parallel pgrcolumnar parquet loaders
//! (parquet_pgrc.rs, parquet_cluster.rs): the engagement trace, the worker
//! gang size, the decode budget and the file-size floor. The v1
//! (pgrcolumnar) morsel-parallel text COPY pipeline that first owned them is
//! deleted with the v1 access method.

use pgsync::OnceLock;

// ---------------------------------------------------------------------------
// Knobs (env: new real GUCs are barred by pg_settings byte-identity — the
// runtime-lane precedent).
// ---------------------------------------------------------------------------

/// Engagement/refusal trace (PGRUST_PARALLEL_COPY_TRACE=1): the e2e
/// battery's engagement oracle channel. Default-off, zero cost.
pub(crate) fn ptrace_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("PGRUST_PARALLEL_COPY_TRACE").is_ok_and(|v| v.trim() == "1"))
}

pub(crate) fn ptrace(msg: &str) {
    if ptrace_enabled() {
        eprintln!("parallel-copy: {msg}");
    }
}

/// D3343: a parallel load's planned residency outside memory contexts (its
/// decode window), charged to the process footprint and the running
/// statement's ledger while the guard lives, so the memory governor, the
/// watchdog's cancel and the pgrc engine's budget (sized after it) see it.
/// D3808: and to the memory policy's load account
/// (`memledger::LOAD_IN_FLIGHT`), so the broker's snapshot counts it
/// as fixed memory by name. Leader thread only.
pub(crate) struct LoadCharge(usize);

impl LoadCharge {
    pub(crate) fn new(bytes: u64) -> LoadCharge {
        let n = bytes.min(usize::MAX as u64) as usize;
        mcx::global_footprint::charge_raw(n);
        memledger::LOAD_IN_FLIGHT.charge(n as u64);
        LoadCharge(n)
    }

    /// D4399: move the charge to `bytes` (a window that grows and shrinks as
    /// the load runs: a text COPY's chunks in flight). Leader thread only.
    pub(crate) fn move_to(&mut self, bytes: u64) {
        let n = bytes.min(usize::MAX as u64) as usize;
        if n > self.0 {
            mcx::global_footprint::charge_raw(n - self.0);
            memledger::LOAD_IN_FLIGHT.charge((n - self.0) as u64);
        } else if n < self.0 {
            mcx::global_footprint::uncharge_raw(self.0 - n);
            memledger::LOAD_IN_FLIGHT.discharge((self.0 - n) as u64);
        }
        self.0 = n;
    }
}

impl Drop for LoadCharge {
    fn drop(&mut self) {
        mcx::global_footprint::uncharge_raw(self.0);
        memledger::LOAD_IN_FLIGHT.discharge(self.0 as u64);
    }
}

/// D3808: the bytes a load may still take in flight now under the memory
/// policy (`mempolicy::load::load_room` at the resident bytes the
/// governor's probe reads); `None` with no probe or no known limit.
pub(crate) fn load_room_now() -> Option<u64> {
    let resident = mcx::governor::probe_resident()? as u64;
    let dirty = mcx::governor::probe_dirty().map_or(0, |d| d as u64);
    let room = mempolicy::load::load_room(mcx::governor::limit() as u64, mcx::governor::bar() as u64, resident, dirty);
    (room != u64::MAX).then_some(room)
}

/// D3343: `headroom` held to what the governor's published limit leaves
/// past the process's governed use (pgrust.memory_watchdog_limit or the
/// smallest cgroup memory.max over the ancestors); unchanged where no limit
/// is published.
pub(crate) fn governed(headroom: Option<u64>) -> Option<u64> {
    let limit = mcx::governor::limit() as u64;
    if limit == 0 {
        return headroom;
    }
    let room = limit.saturating_sub(mcx::governor::projected() as u64);
    Some(headroom.map_or(room, |h| h.min(room)))
}

/// Worker gang size: PGRUST_PARALLEL_COPY_DOP, default = the runtime pool's
/// execution width, clamped to the external-lane budget.
///
/// Lane dop (2026-09-15): the July pipeline's `min(…, 32)` clamp is gone.
/// It bound only above 32 workers, where it made the knob inert (every
/// `PGRUST_PARALLEL_COPY_DOP` above 32 ran at 32 on the metal-48xl) and held
/// the pgrc decode window at `2·32 + 2`. A ≤ 32-worker box derives the same
/// value as before by construction.
pub(crate) fn dop(rt: &runtime::Runtime) -> i32 {
    static N: OnceLock<Option<u64>> = OnceLock::new();
    let req = *N.get_or_init(|| {
        std::env::var("PGRUST_PARALLEL_COPY_DOP").ok().and_then(|v| v.trim().parse().ok())
    });
    dop_for(req, rt.config().workers) as i32
}

/// The pure rule behind [`dop`]: the request, else the pool's workers, never
/// above the external-lane budget.
pub(crate) fn dop_for(req: Option<u64>, workers: usize) -> u64 {
    req.unwrap_or(workers as u64).clamp(1, runtime::MAX_EXTERNAL_LANES as u64)
}

/// The pgrcolumnar parquet path (parquet_pgrc.rs) is ON by default: it is
/// order-preserving (part bytes serial-exact), so a live runtime pool is the
/// only precondition. PGRUST_PARQUET_PARALLEL=0 is the opt-out (the
/// serial reader verbatim); any other value, or unset, leaves it on.
pub(crate) fn pgrc_parquet_parallel_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| !std::env::var("PGRUST_PARQUET_PARALLEL").is_ok_and(|v| v.trim() == "0"))
}

/// The default: 2048 MB, i.e. three 145 MB row groups decoded ahead at
/// the pgrc driver's 4× expansion estimate.
pub(crate) const LEGACY_PARQUET_BUDGET_BYTES: u64 = 2048 << 20;

/// The knob alone (`PGRUST_PARQUET_BUDGET_MB`, floor 64 MB), `None` unset.
fn parquet_budget_requested() -> Option<u64> {
    std::env::var("PGRUST_PARQUET_BUDGET_MB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|mb| mb.max(64) << 20)
}

/// The pgrc driver's decode budget derived from the machine (lane dop,
/// 2026-09-15): the knob when set; else the legacy 2048 MB on a pool of at
/// most [`LEGACY_BUDGET_MAX_WORKERS`] workers (the 16-core class, where the
/// 2048 MB window is the memory-safe posture parqload measured); else a
/// sixteenth of the memory headroom, never below the legacy value. Resolved
/// once per process. A sixteenth, not a quarter: on the metal-48xl a quarter
/// (94 GB, window 161) held 85 GB more RSS than a window of 28–66 and loaded
/// no faster (parquet-load.md §12.7.2); a sixteenth is window ≈ 40 there.
pub(crate) fn parquet_budget_bytes_auto(workers: usize) -> u64 {
    static N: OnceLock<u64> = OnceLock::new();
    *N.get_or_init(|| {
        parquet_budget_requested().unwrap_or_else(|| {
            parquet_budget_for(workers, crate::memheadroom::memory_headroom_bytes())
        })
    })
}

/// A pool this wide or narrower keeps the legacy decode budget.
pub(crate) const LEGACY_BUDGET_MAX_WORKERS: usize = 16;

/// The pure rule behind [`parquet_budget_bytes_auto`].
pub(crate) fn parquet_budget_for(workers: usize, headroom: Option<u64>) -> u64 {
    match headroom {
        Some(h) if workers > LEGACY_BUDGET_MAX_WORKERS => (h / 16).max(LEGACY_PARQUET_BUDGET_BYTES),
        _ => LEGACY_PARQUET_BUDGET_BYTES,
    }
}

/// File-source engagement floor (bytes): tiny loads keep the serial path
/// (frontend streams engage regardless — their size is unknowable).
pub(crate) fn file_floor() -> u64 {
    static N: OnceLock<u64> = OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("PGRUST_PARALLEL_COPY_MIN_BYTES")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(4 << 20)
    })
}
