//! Memory step 5: the consumers of Gravity's one memory budget that the
//! hosts own, registered with the driver's budget (`gravity_driver::mem::budget`): pgrc's part cache, the resident fp
//! tables, the build-buffer pool, the zone store and the condition cache. Each hook reads atomics or takes its cache's own
//! lock once; none reads a GUC (the census runs on the idle-release thread, which has no GUC store). The caps are today's
//! (the off path's own limits, unchanged); the reclaims run from 5b's reclaimer and the memory watchdog's backstop.

use core::sync::atomic::{AtomicBool, Ordering};

use gravity_driver::mem::budget::{register, Cfg, Consumer, Hooks};
use types_error::PgResult;

/// Register every hosts consumer (exec's `arm_grant_pool`, before the pool reads the budget; the first registration wins).
pub fn register_all() {
    register(Consumer::PartCache, Hooks { current: part_cache_bytes, floor: part_cache_floor, cap: part_cache_cap, reclaim: Some(part_cache_reclaim) });
    register(Consumer::FpTables, Hooks { current: fp_bytes, floor: crate::dict::text_hosts::fp_pinned_bytes, cap: crate::dict::text_hosts::fp_cache_cap_seen, reclaim: Some(crate::dict::text_hosts::fp_cache_trim_to) });
    register(Consumer::BuildPool, Hooks { current: build_pool_bytes, floor: zero, cap: build_pool_cap, reclaim: Some(crate::dict::text_hosts::reuse::release_to) });
    register(Consumer::ZoneStore, Hooks { current: zone_store_bytes, floor: zero, cap: zone_store_cap, reclaim: Some(crate::ops::scan::pgrc::granule_source::zone_store_clear) });
    register(Consumer::CondCache, Hooks { current: crate::ops::scan::pgrc::cond_cache::held_bytes, floor: zero, cap: crate::ops::scan::pgrc::cond_cache::capacity_seen, reclaim: Some(crate::ops::scan::pgrc::cond_cache::trim_to) });
    // C2: the shared catalog L2 bounds itself (its entry and byte caps) and has no reclaim hook: fixed at its bytes
    register(Consumer::CatalogL2, Hooks { current: catalog_l2_bytes, floor: catalog_l2_bytes, cap: catalog_l2_bytes, reclaim: None });
    // D4298 (T4): the memory ladder alone trims it (R2a's tail, after the caches): the hook above stays as it was
    gravity_driver::mem::budget::register_ladder_reclaim(Consumer::CatalogL2, zero, catalog_l2_trim);
    mcx::governor::set_part_pins_probe(part_pins_alone);
    // ladder C1c: the demand probes of the caches the ladder sizes (what each holds; C1d: the bytes it evicted itself to
    // stay under its budget, since start)
    mcx::ladder::set_sized_probe(mcx::ladder::SIZED_PART, || (part_cache_bytes(), pgrc_read::partcache::counters().2));
    mcx::ladder::set_sized_probe(mcx::ladder::SIZED_FP, || (u64::MAX, crate::dict::text_hosts::fp_cap_evicted_bytes()));
    mcx::ladder::set_sized_probe(mcx::ladder::SIZED_COND, || {
        (crate::ops::scan::pgrc::cond_cache::held_bytes(), crate::ops::scan::pgrc::cond_cache::cap_evicted_bytes())
    });
    gravity_driver::mem::budget::register_trimmable(Consumer::PartCache, part_cache_trimmable);
    // C3 (cache_purge_now): memruntime's purger applies the caches' cuts off every query thread
    mcx::set_cache_cut(cut_caches);
}

/// C3 (`cache_purge_now`): the process caches' cut, run by memruntime's purger on its tick and its wake, off every query
/// thread: the part cache's budget re-read and its trim past it (`residency::cut_part_cache`), the condition cache down to
/// `pgrust.condition_cache_size` and the fp tables down to their cap (D4648: a lowered cap applied only at their next
/// insert). Each cache is trimmed only once its cap went DOWN since the purger's last look (a cut: a reload, the broker's
/// target), and only until that cut is met or a trim frees nothing: a cache past an unchanged cap is its statements' to
/// trim, as before C3 (a purger trim of a cache past an unchanged cap ran beside the statement's own trim from the same
/// snapshot and evicted the excess twice, D4659). The bytes the trims freed and the bytes past the caps they could not
/// free (in use, pinned or shared).
fn cut_caches() -> mcx::CacheCut {
    use crate::ops::scan::pgrc::cond_cache;
    static PART: Cut = Cut::new();
    static COND: Cut = Cut::new();
    static FP: Cut = Cut::new();
    let (part_freed, part_held) = crate::ops::scan::pgrc::residency::cut_part_cache(|budget| PART.due(budget));
    PART.after(part_freed, part_held);
    let (mut cond_freed, mut cond_held) = (0, 0);
    let cond_cap = cond_cache_cap_now();
    if COND.due(cond_cap) {
        cond_freed = cond_cache::trim_to(cond_cap);
        cond_held = cond_cache::held_bytes().saturating_sub(cond_cap);
        COND.after(cond_freed, cond_held);
    }
    // the fp tables' cap as their last insert read it (their GUC is a placeholder option: no process-wide read)
    let (mut fp_freed, mut fp_held) = (0, 0);
    let fp_cap = crate::dict::text_hosts::fp_cache_cap_seen();
    if FP.due(fp_cap) {
        fp_freed = crate::dict::text_hosts::fp_cache_trim_to(fp_cap);
        fp_held = crate::dict::text_hosts::fp_tables_resident().3.saturating_sub(fp_cap);
        FP.after(fp_freed, fp_held);
    }
    mcx::CacheCut { freed: part_freed + cond_freed + fp_freed, held: part_held + cond_held + fp_held }
}

/// C3: one cache's cut as the purger tracks it: the cap at its last look, and whether a lowered cap is still to be met.
struct Cut {
    last: core::sync::atomic::AtomicU64,
    due: AtomicBool,
}

impl Cut {
    const fn new() -> Self {
        Self { last: core::sync::atomic::AtomicU64::new(u64::MAX), due: AtomicBool::new(false) }
    }

    /// Whether the purger trims this cache now: its cap went down since the last look, or an earlier cut is not met yet.
    fn due(&self, cap: u64) -> bool {
        if cap < self.last.swap(cap, Ordering::Relaxed) {
            self.due.store(true, Ordering::Relaxed);
        }
        self.due.load(Ordering::Relaxed)
    }

    /// After a trim: the cut is over once nothing is past the cap, or the trim freed nothing (what is left is in use; the
    /// statements' own trims take it as they did before C3).
    fn after(&self, freed: u64, held: u64) {
        if held == 0 || freed == 0 {
            self.due.store(false, Ordering::Relaxed);
        }
    }
}

/// `pgrust.condition_cache_size` now (kB as set; unset or -1: the derived budget an execution set last).
fn cond_cache_cap_now() -> u64 {
    crate::inputs::condition_cache_kb().map_or_else(crate::ops::scan::pgrc::cond_cache::capacity_seen, |kb| kb.saturating_mul(1024))
}

/// D4353: what a content trim of the part cache could free now (the memory ladder's R2a count).
fn part_cache_trimmable() -> u64 {
    pgrc_am::inval::registry().trimmable_bytes()
}

/// D3999 / C8 (D4087): the in-use parts the running owners of one account alone use (its statement and the portal it
/// executes), what its cancel frees.
fn part_pins_alone(f: &mut dyn FnMut(u32, u64)) {
    pgrc_am::inval::registry().in_use_by_running(|a, b| f(a, b));
}

/// The shared catalog L2's census: its live entries' approximate bytes (l2cache `stats`).
fn catalog_l2_bytes() -> u64 {
    l2cache::stats().bytes as u64
}

/// D4298: the memory ladder's trim of the shared catalog L2 down to `to` bytes (an unlinked entry is a laggard miss).
fn catalog_l2_trim(to: u64) -> u64 {
    l2cache::trim_to(usize::try_from(to).unwrap_or(usize::MAX)) as u64
}

fn zero() -> u64 {
    0
}

/// D3793: what the memory limit leaves the part cache's explicit setting (the cache's budget, and its floor under the
/// memory broker). USER ruling: a setting past `max` is FATAL at server start, and a reload to one keeps the old value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartCacheRoom {
    /// The memory limit (a cgroup's memory.max, pgrust.memory_watchdog_limit, or physical memory).
    pub limit: u64,
    /// B, the memory budget inside it (`cost::budget_of`: the limit less the operating system's reserve and the page-cache
    /// floor, under pgrust.memory_target).
    pub b: u64,
    /// The grant pool's minimum: the work pool's least (`cost::WORK_POOL_MIN`), or an explicit pool or per-statement grant
    /// setting when larger (a statement's explicit grant must fit beside the cache).
    pub work: u64,
    pub shared_buffers: u64,
    /// The fixed consumers at their start-up size (none runs before the first statement).
    pub fixed: u64,
    /// The largest setting that fits: B less the other three.
    pub max: u64,
}

/// `PartCacheRoom` over its inputs (a pure function: the start check, the reload check and the tests).
pub fn part_cache_room(limit: u64, cfg: Cfg, work_explicit: u64, shared_buffers: u64, fixed: u64) -> PartCacheRoom {
    let b = gravity_program::cost::budget_of(limit, cfg.target, cfg.page_cache_floor, cfg.reserve, 0).b;
    let work = gravity_program::cost::WORK_POOL_MIN.max(work_explicit);
    let max = b.saturating_sub(work).saturating_sub(shared_buffers).saturating_sub(fixed);
    PartCacheRoom { limit, b, work, shared_buffers, fixed, max }
}

/// The room under the server's settings now: the explicit grant pool (`pgrust.gravity_grant_pool_mb`, else the e2e lever
/// `PGRUST_GRAVITY_GRANT_POOL_MB`, as exec's pool arms) and per-statement grant (`pgrust.gravity_grant_mb`), and
/// shared_buffers.
fn part_cache_room_now(limit: u64) -> PartCacheRoom {
    let work = crate::inputs::grant_explicit_mb().saturating_mul(1 << 20);
    part_cache_room(limit, crate::inputs::memory_budget_cfg(), work, crate::inputs::shared_buffers_bytes(), 0)
}

/// The DETAIL and HINT naming the limit (and where it came from, when known), the budget's parts and the largest setting
/// that fits.
fn part_cache_room_text(room: &PartCacheRoom, source: Option<&str>) -> (String, String) {
    let m = |b: u64| b >> 20;
    let from = source.map_or_else(String::new, |s| format!(" ({s})")); // alloc: Cold
    let detail = format!( // alloc: Cold
        "The memory limit is {} MB{from}. The memory budget inside it is {} MB, of which the grant pool needs {} MB and shared buffers {} MB, which leaves {} MB for the part cache.",
        m(room.limit),
        m(room.b),
        m(room.work),
        m(room.shared_buffers),
        m(room.max)
    );
    let hint = format!( // alloc: Cold
        "The largest \"pgrust.pgrc_part_cache_mb\" that fits the {} MB memory limit is {}. Reduce it, or unset it to size the part cache from the limit.",
        m(room.limit),
        m(room.max)
    );
    (detail, hint)
}

/// The start check passed: from then on a reload (or ALTER SYSTEM) to a setting past the room is refused by the option's
/// check hook. Before it, the configuration files load unchecked and the start check decides on the settled values.
static START_CHECKED: AtomicBool = AtomicBool::new(false);

/// D4552 (a): the ledger's homes and floor from the memory policy's knobs, at every start: with no override too, since
/// `ledger_home` defaults to 1 (and `ledger_floor` to 0: the floor is off unless PGRUST_MEMPOLICY=ledger_floor=1).
fn ledger_switches() {
    let s = ledger_switch_values();
    memledger::set_home(s.home);
    memledger::set_floor(s.floor);
    memledger::set_unbind_wait(s.unbind_wait);
    memledger::set_home_keep(mempolicy::consts::get(mempolicy::consts::K::HomeKeep) != 0);
    memledger::set_fold_on_park(mempolicy::consts::get(mempolicy::consts::K::FoldOnPark) != 0);
}

/// The ledger switches the memory policy's knobs give the server's start (`ledger_switches`), read without setting them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LedgerSwitches {
    home: bool,
    floor: bool,
    unbind_wait: bool,
}

fn ledger_switch_values() -> LedgerSwitches {
    use mempolicy::consts::{get, K};
    LedgerSwitches { home: get(K::LedgerHome) != 0, floor: get(K::LedgerFloor) != 0, unbind_wait: get(K::LedgerUnbindWait) != 0 }
}

/// The postmaster's start, before the memory checks: `PGRUST_MEMPOLICY` applied to the memory policy's constants
/// (`mempolicy::consts::apply`). An unknown key, a value that is not an integer or an unknown `impl` is FATAL: an experiment
/// never runs with a setting silently ignored. `trace=1` sends every decision's record to the server's log.
pub fn mempolicy_at_start() -> PgResult<()> {
    let Some(spec) = crate::inputs::mempolicy_override() else {
        ledger_switches();
        return Ok(());
    };
    match mempolicy::consts::apply(&spec) {
        Ok(applied) => {
            if applied.trace {
                mempolicy::trace::enable(trace_to_log);
            }
            // broker (b)'s door counters: the switch every door reads (memledger::value::ext)
            memledger::value::set_ext(mempolicy::consts::get(mempolicy::consts::K::BrokerValue) != 0);
            ledger_switches();
            Ok(())
        }
        Err(refusal) => {
            let entry = match refusal {
                mempolicy::consts::Refusal::UnknownKey(e) | mempolicy::consts::Refusal::NotAnInteger(e) | mempolicy::consts::Refusal::UnknownImpl(e) => e,
            };
            let what = match refusal {
                mempolicy::consts::Refusal::UnknownKey(_) => "an unknown key",
                mempolicy::consts::Refusal::NotAnInteger(_) => "a value that is not an integer",
                mempolicy::consts::Refusal::UnknownImpl(_) => "an unknown impl",
            };
            elog::ereport(types_error::FATAL)
                .errcode(types_error::ERRCODE_INVALID_PARAMETER_VALUE)
                .errmsg(format!("invalid PGRUST_MEMPOLICY entry \"{entry}\": {what}")) // alloc: Cold
                .errhint(format!("The keys are impl, trace and {}.", mempolicy::consts::KNOBS.iter().map(|k| k.name.to_string()).reduce(|a, b| format!("{a}, {b}")).unwrap_or_default())) // alloc: Cold
                .finish(types_error::ErrorLocation::new("gravity", 0, "mempolicy_at_start"))
        }
    }
}

/// A memory policy decision's record (`trace=1`), on the server's standard error (the log), from any thread, with the
/// default log_line_prefix's time and pid (`%m [%p] `; G1-B: a record joins its statement's lines by them).
fn trace_to_log(decision: &str, record: &str) {
    // D4812: through elog's stderr writer, which hands a memory safety thread's write (the watchdog's tick records) off it
    elog::write_stderr(&format!("{} [{}] LOG:  mempolicy: {decision}: {record}\n", UtcStamp(pg_clock::wall_us()), init_small::globals::MyProcPid())); // alloc: Cold
}

/// A wall time (microseconds since the Unix epoch, `pg_clock::wall_us`) as log_line_prefix's `%m` in UTC: the date, the time
/// to the millisecond and `UTC`.
struct UtcStamp(i64);

impl core::fmt::Display for UtcStamp {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let ms = self.0.div_euclid(1000);
        let (secs, ms) = (ms.div_euclid(1000), ms.rem_euclid(1000));
        let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
        // the civil date of a day count from 1970-01-01 (proleptic Gregorian)
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        write!(f, "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}.{ms:03} UTC", sod / 3600, sod % 3600 / 60, sod % 60)
    }
}

/// D3793 (USER ruling): the postmaster, after the configuration files and before the data directory's lock: an explicit
/// part cache setting (`inputs::part_cache_server_mb`) past what the memory limit `limit` (from `source`) leaves is FATAL, as
/// PostgreSQL's start refuses a shared memory request the machine cannot map. Unset (derived from the limit) or no limit:
/// nothing to check.
pub fn part_cache_check_at_start(limit: u64, source: &str) -> PgResult<()> {
    let room = part_cache_room_now(limit);
    match crate::inputs::part_cache_server_mb().map(|mb| mb.saturating_mul(1 << 20)) {
        Some(setting) if limit != 0 && setting > room.max => {
            let (detail, hint) = part_cache_room_text(&room, Some(source));
            elog::ereport(types_error::FATAL)
                .errcode(types_error::ERRCODE_INVALID_PARAMETER_VALUE)
                .errmsg(format!("part cache budget of {} MB does not fit the memory limit", setting >> 20)) // alloc: Cold
                .errdetail(detail)
                .errhint(hint)
                .finish(types_error::ErrorLocation::new("gravity", 0, "part_cache_check_at_start"))
        }
        _ => {
            START_CHECKED.store(true, Ordering::Release);
            Ok(())
        }
    }
}

/// D3793 (USER ruling): `pgrust.pgrc_part_cache_mb`'s check (`inputs::install_part_cache_check`): after the start check, a
/// value `mb` past what the published memory limit leaves is refused with this DETAIL and HINT (a reload keeps the old value
/// and LOGs PostgreSQL's 'invalid value for parameter'; ALTER SYSTEM raises it). -1 (derived) always passes, as does every
/// value before the start check (the configuration files load first; the start check decides on the settled value).
pub fn part_cache_refusal(mb: i32) -> Option<(String, String)> {
    let limit = mcx::governor::limit() as u64;
    if mb < 0 || limit == 0 || !START_CHECKED.load(Ordering::Acquire) {
        return None;
    }
    let room = part_cache_room_now(limit);
    if (mb as u64).saturating_mul(1 << 20) <= room.max {
        return None;
    }
    Some(part_cache_room_text(&room, None))
}

/// D3793: install the option's check hook (exec's seams).
pub fn install_part_cache_check() {
    crate::inputs::install_part_cache_check();
}

/// The part cache's USED bytes: the resident images (`partcache::bytes`) and the evicted images a handle still holds, which
/// stay charged to the governor's cache account until they go (`mcx::governor::cache_bytes`, charged and released by the
/// part cache's account hook). Equal to the resident bytes when no evicted image is held.
fn part_cache_bytes() -> u64 {
    pgrc_read::partcache::bytes().max(mcx::governor::cache_bytes() as u64)
}

/// The parts in use keep their contents (no trim evicts them), and the server's explicit `pgrust.pgrc_part_cache_mb` is
/// the cache's minimum (memory step 7: the broker never targets it, nor the reclaimer frees it, under that).
fn part_cache_floor() -> u64 {
    let in_use = pgrc_am::inval::registry().in_use_bytes();
    crate::ops::scan::pgrc::residency::part_cache_server_bytes().map_or(in_use, |b| in_use.max(b))
}

/// Today's budget as `residency::arm_part_cache` last set it; before any statement armed it, the auto cap it would set
/// (`cost::part_cache_cap` over RAM and the pool as armed now).
fn part_cache_cap() -> u64 {
    match pgrc_read::partcache::budget() {
        u64::MAX => crate::dict::text_hosts::reuse::store_bytes(gravity_program::cost::part_cache_cap) as u64,
        b => b,
    }
}

fn part_cache_reclaim(to: u64) -> u64 {
    pgrc_am::inval::registry().trim_contents(to)
}

fn fp_bytes() -> u64 {
    crate::dict::text_hosts::fp_tables_resident().3
}

fn build_pool_bytes() -> u64 {
    crate::dict::text_hosts::reuse::held().1 as u64 * 8
}

fn build_pool_cap() -> u64 {
    crate::dict::text_hosts::reuse::pool_bytes() as u64
}

fn zone_store_bytes() -> u64 {
    crate::ops::scan::pgrc::granule_source::zone_store_held() as u64
}

fn zone_store_cap() -> u64 {
    crate::ops::scan::pgrc::granule_source::zone_store_bytes() as u64
}

#[cfg(test)]
mod tests {
    /// G1-B: the trace's time reads as log_line_prefix's `%m` in UTC.
    #[test]
    fn trace_stamps_read_as_the_log_prefix_time() {
        let at = |us: i64| super::UtcStamp(us).to_string(); // alloc: Cold
        const Y2K: i64 = 10_957 * 86_400_000_000;
        assert_eq!(at(0), "1970-01-01 00:00:00.000 UTC");
        assert_eq!(at(Y2K), "2000-01-01 00:00:00.000 UTC");
        // 9779 days and 86232.345678 s past the 2000 epoch
        assert_eq!(at(Y2K + 9_779 * 86_400_000_000 + 86_232_345_678), "2026-10-10 23:57:12.345 UTC");
        assert_eq!(at(Y2K + 59 * 86_400_000_000), "2000-02-29 00:00:00.000 UTC", "a leap day");
        assert_eq!(at(Y2K - 1_000), "1999-12-31 23:59:59.999 UTC");
    }

    use super::*;

    /// D4552 (a): the server's start at the memory policy's defaults (no PGRUST_MEMPOLICY) switches the ledger's homes
    /// and the unbind wait on and leaves the floor OFF (the floor-off legs on 7ec8a2eb030: no fold took a one-thread
    /// backend account below zero and no session ended below zero); PGRUST_MEMPOLICY=ledger_floor=1 brings it back.
    #[test]
    fn the_servers_start_leaves_the_ledger_floor_off() {
        let _one = pgsync::lock(&mempolicy::consts::TEST_LOCK);
        mempolicy::consts::reset();
        assert_eq!(ledger_switch_values(), LedgerSwitches { home: true, floor: false, unbind_wait: true }, "the defaults");
        assert!(mempolicy::consts::apply("ledger_floor=1").is_ok());
        assert_eq!(ledger_switch_values(), LedgerSwitches { home: true, floor: true, unbind_wait: true }, "ledger_floor=1");
        mempolicy::consts::reset();
    }

    /// C3: the purger trims a cache only after its cap went down, until the cut is met or a trim frees nothing; a cache
    /// past an unchanged cap, or a raised cap, is never its to trim.
    #[test]
    fn c3_a_cache_is_cut_only_after_its_cap_went_down_until_the_cut_is_met() {
        let c = Cut::new();
        assert!(c.due(8 << 30), "the first look (from no cap) trims once");
        c.after(0, 0);
        assert!(!c.due(8 << 30), "an unchanged cap: the statements trim, not the purger");
        assert!(!c.due(9 << 30), "a raised cap");
        assert!(c.due(1 << 30), "a lowered cap: a cut");
        c.after(5 << 30, 2 << 30);
        assert!(c.due(1 << 30), "freed some, more past the cap: due again");
        c.after(1 << 30, 1 << 30);
        assert!(c.due(1 << 30));
        c.after(0, 1 << 30);
        assert!(!c.due(1 << 30), "a trim that freed nothing ends the cut (the rest is in use)");
        assert!(c.due(1 << 29), "the next cut");
        c.after(1 << 29, 0);
        assert!(!c.due(1 << 29), "met");
    }

    /// D3793: the part cache's room is B less the grant pool's minimum (an explicit grant when larger), shared_buffers and
    /// the fixed consumers. An install's formulas over the host's RAM do not fit a 50 % container; over the
    /// container's limit they do.
    #[test]
    fn d3793_the_part_cache_room_is_b_less_the_pool_minimum_shared_buffers_and_fixed() {
        const MB: u64 = 1 << 20;
        let limit = 15741 * MB;
        let b = gravity_program::cost::budget_of(limit, 0, 0, 0, 0).b;
        let r = part_cache_room(limit, Cfg::DEFAULT, 0, 128 * MB, 0);
        assert_eq!((r.b, r.work), (b, gravity_program::cost::WORK_POOL_MIN));
        assert_eq!(r.max, b - gravity_program::cost::WORK_POOL_MIN - 128 * MB);
        // the host-RAM formulas: grant 7870 MB beside a part cache of 11806 MB
        let host = part_cache_room(limit, Cfg::DEFAULT, 7870 * MB, 128 * MB, 0);
        assert!(11806 * MB > host.max, "{host:?}");
        // the container's formulas: grant 3935 MB, part cache 5902 MB
        let container = part_cache_room(limit, Cfg::DEFAULT, 3935 * MB, 128 * MB, 0);
        assert!(5902 * MB <= container.max, "{container:?}");
        // pgrust.memory_target lowers B, and so the room
        let target = part_cache_room(limit, Cfg { target: 4096 * MB, ..Cfg::DEFAULT }, 0, 128 * MB, 0);
        assert_eq!(target.max, 4096 * MB - gravity_program::cost::WORK_POOL_MIN - 128 * MB);
        // a room never goes under 0
        assert_eq!(part_cache_room(2 * MB, Cfg::DEFAULT, 0, 128 * MB, 0).max, 0);
    }
    use gravity_driver::mem::budget::{self, Cfg, Mode};

    fn caps() -> [u64; 5] {
        [
            part_cache_cap(),
            build_pool_cap(),
            zone_store_cap(),
            crate::ops::scan::pgrc::cond_cache::capacity_seen(),
            crate::dict::text_hosts::fp_cache_cap_seen(),
        ]
    }

    /// Memory step 5: the hooks report today's caps, and the broker off or at advise changes none of them (the off path is
    /// today's byte for byte; nothing in step 5 reads a target).
    #[test]
    fn the_hooks_report_todays_caps_and_off_or_advise_move_none() {
        register_all();
        let before = caps();
        for mode in [Mode::Off, Mode::Advise, Mode::On] {
            budget::configure(Cfg { mode, ..Cfg::DEFAULT });
            assert_eq!(caps(), before, "{mode:?}");
        }
        let hooked = [Consumer::PartCache, Consumer::BuildPool, Consumer::ZoneStore, Consumer::CondCache, Consumer::FpTables].map(|c| (budget::hooks(c).expect("registered").cap)());
        assert_eq!(hooked, before);
        budget::configure(Cfg::DEFAULT);
    }
}
