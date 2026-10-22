//! Memory step 6a: the broker's two views' functions, pgrust-native builtins on the reserved oids (the pg_proc rows and the
//! views come from `scripts/memory-broker-views.sql`, created on demand: the shipped catalog carries no delta).
//! `pgrust_memory_broker_stats()`: one row per consumer; `pgrust_memory_budget()`: one row. Both read the broker's latest
//! published interval; with the broker off (or before its first interval) the budget row says so and there are no
//! consumer rows.

use datum::Datum;
use types_core::Oid;
use types_error::PgResult;
use types_fmgr::{varlena_result, FmgrBuiltin, FmgrInfo, FunctionCallInfoBaseData as Fcinfo};

use gravity_driver::mem::budget::Consumer;
use gravity_driver::mem::policy::refill_class;

pub const PGRUST_MEMORY_BROKER_STATS_FOID: Oid = 9070;
pub const PGRUST_MEMORY_BUDGET_FOID: Oid = 9071;
pub const PGRUST_MEMORY_ACCOUNTS_FOID: Oid = 9072;
pub const PGRUST_MEMORY_ACCOUNT_KINDS_FOID: Oid = 9073;
pub const PGRUST_MEMORY_OWNER_END_FOID: Oid = 9074;
pub const PGRUST_MEMORY_OWNERS_FOID: Oid = 9075;
pub const PGRUST_MEMORY_RECONCILE_FOID: Oid = 9076;
pub const PGRUST_MEMORY_LADDER_STATS_FOID: Oid = 9077;
pub const PGRUST_MEMORY_LADDER_EPISODES_FOID: Oid = 9078;
pub const PGRUST_MEMORY_REVOKE_STATS_FOID: Oid = 9079;
pub const PGRUST_MEMORY_CACHE_CURVES_FOID: Oid = 9080;
pub const PGRUST_MEMORY_SETTINGS_FOID: Oid = 9081;

pub const BROKER_BUILTINS: &[FmgrBuiltin] = &[
    FmgrBuiltin {
        foid: PGRUST_MEMORY_BROKER_STATS_FOID,
        name: "pgrust_memory_broker_stats",
        nargs: 0,
        strict: false,
        retset: true,
        func: fc_pgrust_memory_broker_stats,
    },
    FmgrBuiltin {
        foid: PGRUST_MEMORY_BUDGET_FOID,
        name: "pgrust_memory_budget",
        nargs: 0,
        strict: false,
        retset: true,
        func: fc_pgrust_memory_budget,
    },
    FmgrBuiltin {
        foid: PGRUST_MEMORY_ACCOUNTS_FOID,
        name: "pgrust_memory_accounts",
        nargs: 0,
        strict: false,
        retset: true,
        func: fc_pgrust_memory_accounts,
    },
    FmgrBuiltin {
        foid: PGRUST_MEMORY_ACCOUNT_KINDS_FOID,
        name: "pgrust_memory_account_kinds",
        nargs: 0,
        strict: false,
        retset: true,
        func: fc_pgrust_memory_account_kinds,
    },
    FmgrBuiltin {
        foid: PGRUST_MEMORY_OWNER_END_FOID,
        name: "pgrust_memory_owner_end",
        nargs: 0,
        strict: false,
        retset: true,
        func: fc_pgrust_memory_owner_end,
    },
    FmgrBuiltin {
        foid: PGRUST_MEMORY_OWNERS_FOID,
        name: "pgrust_memory_owners",
        nargs: 0,
        strict: false,
        retset: true,
        func: fc_pgrust_memory_owners,
    },
    FmgrBuiltin {
        foid: PGRUST_MEMORY_RECONCILE_FOID,
        name: "pgrust_memory_reconcile",
        nargs: 0,
        strict: false,
        retset: true,
        func: fc_pgrust_memory_reconcile,
    },
    FmgrBuiltin {
        foid: PGRUST_MEMORY_LADDER_STATS_FOID,
        name: "pgrust_memory_ladder_stats",
        nargs: 0,
        strict: false,
        retset: true,
        func: fc_pgrust_memory_ladder_stats,
    },
    FmgrBuiltin {
        foid: PGRUST_MEMORY_LADDER_EPISODES_FOID,
        name: "pgrust_memory_ladder_episodes",
        nargs: 0,
        strict: false,
        retset: true,
        func: fc_pgrust_memory_ladder_episodes,
    },
    FmgrBuiltin {
        foid: PGRUST_MEMORY_REVOKE_STATS_FOID,
        name: "pgrust_memory_revoke_stats",
        nargs: 0,
        strict: false,
        retset: true,
        func: fc_pgrust_memory_revoke_stats,
    },
    FmgrBuiltin {
        foid: PGRUST_MEMORY_CACHE_CURVES_FOID,
        name: "pgrust_memory_cache_curves",
        nargs: 0,
        strict: false,
        retset: true,
        func: super::curves::fc_pgrust_memory_cache_curves,
    },
    FmgrBuiltin {
        foid: PGRUST_MEMORY_SETTINGS_FOID,
        name: "pgrust_memory_settings",
        nargs: 0,
        strict: false,
        retset: true,
        func: super::settings::fc_pgrust_memory_settings,
    },
];

fn text<'m>(mcx: mcx::Mcx<'m>, s: &str) -> Datum {
    let mut img: mcx::PgVec<'_, u8> = mcx::PgVec::new_in(mcx);
    img.extend_from_slice(&[0u8; datum::VARHDRSZ]);
    img.extend_from_slice(s.as_bytes());
    varlena_result(datum::Varlena::from_image(img))
}

/// D3929: a views script of another build declares other columns than this server forms; refused by name (a row formed
/// past the declared columns panics the backend).
fn declared(srf: &funcapi::MaterializedSRF<'_>, cols: usize) -> PgResult<()> {
    if srf.tupdesc.natts as usize == cols {
        return Ok(());
    }
    Err(Box::new(types_error::PgError::error("the memory views' installed declaration is another build's: run scripts/memory-broker-views.sql again").with_sqlstate(types_error::ERRCODE_DATATYPE_MISMATCH))) // alloc: Cold
}

fn i8d(x: u64) -> Datum {
    Datum::from_i64(i64::try_from(x).unwrap_or(i64::MAX))
}

/// An int8[] of `xs` up to its last non-zero element.
fn int8_array<'m>(mcx: mcx::Mcx<'m>, xs: &[u64; memledger::value::BUCKETS]) -> PgResult<Datum> {
    let n = xs.iter().rposition(|&x| x != 0).map_or(0, |i| i + 1);
    let elems: [Datum; memledger::value::BUCKETS] = core::array::from_fn(|k| i8d(xs[k]));
    let img = arrayfuncs::construct_array(mcx, &elems[..n], types_core::catalog::INT8OID, 8, true, b'd')?;
    Ok(varlena_result(datum::Varlena::from_image(img)))
}

/// The consumer rows' columns (scripts/memory-broker-views.sql declares the same, in order).
const STATS_COLS: usize = 36;

/// The first of the consumer rows' value columns (memledger::value, read now, not the interval's).
const VALUE_COL: usize = 19;

/// The first of the consumer rows' tail-hit and ghost columns (memledger::value::ext_stats, read now).
const EXT_COL: usize = 26;

/// D4403: the first of the work_pool row's small-buffer columns (the grant pool's, read now; NULL on every other row).
const SMALL_COL: usize = STATS_COLS - 5;

/// `pgrust_memory_broker_stats()`: consumer, current, cap, floor, ceiling, desired, target, elastic, reused_60s,
/// reused_300s, reused_900s, refill_class, advised_move_bytes (target - current), last_move_bytes, last_move_age_s,
/// reversals, frozen, hold, targeted; then the cache's value counters at its door, read now (memledger::value; NULL for a
/// consumer with no door, the re-miss pair for all but the part cache): hits, misses, miss_bytes, miss_cost_ns (the timed
/// misses' sum), miss_cost_samples, re_misses, re_miss_bytes; then, under `PGRUST_MEMPOLICY=broker_value=1` and for the
/// part cache, the condition cache, the fp tables and the build pool (NULL otherwise), the tail-hit and ghost counters:
/// evicted_bytes (the byte clock), ghost_hits, tail_hits, tail_bytes (the tail the last plan marked), refault_log2 (element
/// `k + 1`: the ghost hits at a distance of `[2^k, 2^(k+1))` evicted bytes; trailing empty buckets cut); then, on the
/// work_pool row only (D4403), the grant pool's small buffer read now: small_buffer_bytes, small_buffer_held_bytes,
/// small_buffer_draws (small statements seated there and draws with no reservation served), small_buffer_waits,
/// small_buffer_refusals.
pub fn fc_pgrust_memory_broker_stats(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let flinfo = flinfo.expect("pgrust_memory_broker_stats: resolved FmgrInfo required");
    let mcx = super::raw::result_mcx(fcinfo);
    let mut srf = funcapi::InitMaterializedSRF(mcx, flinfo, fcinfo, 0)?;
    declared(&srf, STATS_COLS)?;
    // the calling backend's held-back hits in (memledger::value::FOLD)
    memledger::value::flush();
    // D4403: the grant pool's small buffer, on the work_pool row
    let pool = gravity_driver::POOL.stats();
    if let Some(p) = super::published().filter(|p| p.snapshot.is_some()) {
        for c in Consumer::ALL {
            let k = c as usize;
            let row = p.snapshot.as_ref().map(|s| s.row(c)).unwrap_or_default();
            if !row.registered {
                continue;
            }
            let slot = p.state.slots[k];
            let target = p.plan.target[k];
            let moved_age = if slot.last_dir != 0 { p.at.saturating_sub(slot.last_move_at) } else { 0 };
            let v = memledger::value::stats(c);
            let ext = (memledger::value::ext() && memledger::value::ext_door(c)).then(|| memledger::value::ext_stats(c));
            let x = ext.unwrap_or(memledger::value::ExtStats { evicted_bytes: 0, ghost_hits: 0, tail_hits: 0, tail_bytes: 0, refault_log2: [0; memledger::value::BUCKETS] });
            let values: [Datum; STATS_COLS] = [
                text(mcx, c.name()),
                i8d(row.current),
                i8d(row.cap),
                i8d(p.plan.floor[k].max(row.floor)),
                i8d(p.plan.ceiling[k]),
                i8d(p.plan.desired[k]),
                i8d(target),
                i8d(p.plan.elastic[k]),
                i8d(p.census.reused[k][0]),
                i8d(p.census.reused[k][1]),
                i8d(p.census.reused[k][2]),
                text(mcx, refill_class(c)),
                Datum::from_i64(target as i64 - row.current as i64),
                Datum::from_i64(slot.last_move_bytes),
                i8d(moved_age),
                Datum::from_i64(i64::from(slot.reversals)),
                Datum::from_bool(slot.frozen_until > p.at),
                text(mcx, p.plan.hold[k].name()),
                Datum::from_bool(p.plan.targeted[k]),
                i8d(v.hits),
                i8d(v.misses),
                i8d(v.miss_bytes),
                i8d(v.cost_ns),
                i8d(v.cost_samples),
                i8d(v.re_misses),
                i8d(v.re_miss_bytes),
                i8d(x.evicted_bytes),
                i8d(x.ghost_hits),
                i8d(x.tail_hits),
                i8d(x.tail_bytes),
                int8_array(mcx, &x.refault_log2)?,
                i8d(pool.reserve),
                i8d(pool.small_held),
                i8d(pool.small_admits.saturating_add(pool.small_draws)),
                i8d(pool.small_waits),
                i8d(pool.small_refusals),
            ];
            let mut nulls = [false; STATS_COLS];
            if !memledger::value::counted(c) {
                nulls[VALUE_COL..SMALL_COL].fill(true);
            } else if c != Consumer::PartCache {
                nulls[VALUE_COL + 5..EXT_COL].fill(true);
            }
            if ext.is_none() {
                nulls[EXT_COL..SMALL_COL].fill(true);
            }
            if c != Consumer::WorkPool {
                nulls[SMALL_COL..].fill(true);
            }
            // a consumer the policy does not target has no ceiling, desired or move
            if !p.plan.targeted[k] {
                for i in [4, 5, 7, 12, 13, 14, 15, 16] {
                    nulls[i] = true;
                }
            }
            if slot.last_dir == 0 {
                nulls[13] = true;
                nulls[14] = true;
            }
            srf.putvalues(&values, &nulls)?;
        }
    }
    Ok(srf.finish(fcinfo))
}

/// The budget row's columns (scripts/memory-broker-views.sql declares the same, in order).
const MEMORY_VIEW_COLS: usize = 40;

/// `pgrust_memory_budget()`: mode, limit, b_auto, b, free_b, r_low, r_high, page_cache_floor, bar, fixed, unattributed,
/// unattributed_p90, unattributed_counted, work_signal, work_p90_300s, running_floor, queued_bytes, work_demand, fits,
/// registry_removed_parts, registry_removed_bytes, file_bytes_900s, interval_s, plans, plan_us_max, panics, stopped,
/// age_s (seconds since the published interval), parts, parts_used_900s, parts_reused_900s; then memory step 5b's
/// reclaimer, read now (not the interval's): reclaimer (it applies), reclaim_below_low_ticks, reclaim_background_bytes,
/// reclaim_inline_runs, reclaim_inline_bytes, reclaim_inline_us_max, reclaim_inline_short; D3982's worker_cap_refusals, _bytes.
pub fn fc_pgrust_memory_budget(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let flinfo = flinfo.expect("pgrust_memory_budget: resolved FmgrInfo required");
    let mcx = super::raw::result_mcx(fcinfo);
    let mut srf = funcapi::InitMaterializedSRF(mcx, flinfo, fcinfo, 0)?;
    declared(&srf, MEMORY_VIEW_COLS)?;
    let mode = gravity_driver::mem::budget::mode();
    let p = super::published();
    let mut values = [Datum::from_i64(0); MEMORY_VIEW_COLS];
    let mut nulls = [true; MEMORY_VIEW_COLS];
    values[0] = text(mcx, mode.name());
    nulls[0] = false;
    if let Some(p) = p {
        let mut set = |i: usize, d: Datum| {
            values[i] = d;
            nulls[i] = false;
        };
        if let Some(s) = p.snapshot.as_ref().filter(|_| mode != gravity_driver::mem::budget::Mode::Off) {
            let b = s.budget;
            for (i, x) in [b.limit, b.b_auto, b.b, s.free_b, b.r_low, b.r_high, b.page_cache_floor, s.bar, p.signals.fixed, s.unattributed, p.census.unattributed_p90].into_iter().enumerate() {
                set(1 + i, i8d(x));
            }
            set(12, Datum::from_bool(p.census.unattributed_counted));
            for (i, x) in [p.census.work_signal, p.census.work_p90, p.census.running_floor, p.census.queued, p.signals.work_demand].into_iter().enumerate() {
                set(13 + i, i8d(x));
            }
            set(18, Datum::from_bool(p.plan.fits));
            set(19, i8d(p.census.registry_removed.0));
            set(20, i8d(p.census.registry_removed.1));
            set(21, i8d(p.census.file_bytes_900));
            set(28, i8d(p.census.parts));
            set(29, i8d(p.census.used_parts_900));
            set(30, i8d(p.census.reused_parts_900));
        }
        set(22, i8d(p.interval_s));
        set(23, i8d(p.plans));
        set(24, i8d(p.plan_us_max));
        set(25, i8d(p.panics));
        set(26, Datum::from_bool(p.stopped));
        set(27, i8d(super::seconds().saturating_sub(p.at)));
    }
    let r = gravity_driver::mem::reclaim::counters();
    values[31] = Datum::from_bool(gravity_driver::mem::reclaim::applies());
    nulls[31] = false;
    for (i, x) in [r.ticks_below_low, r.background_bytes, r.inline_runs, r.inline_bytes, r.inline_ns_max / 1000, r.inline_short].into_iter().enumerate() {
        values[32 + i] = i8d(x);
        nulls[32 + i] = false;
    }
    let w = gravity_driver::mem::grant::worker_cap_refused();
    for (i, x) in [w.0, w.1].into_iter().enumerate() {
        values[38 + i] = i8d(x);
        nulls[38 + i] = false;
    }
    srf.putvalues(&values, &nulls)?;
    Ok(srf.finish(fcinfo))
}

/// The account rows' columns (scripts/memory-broker-views.sql declares the same, in order).
const ACCOUNT_COLS: usize = 20;

/// G1-A `pgrust_memory_accounts()`: one row per memory ledger account with bytes, a thread, a statement or pool bytes
/// reserved: account, pid (backend rows), statement_seq, running, statement_bytes, statement_peak_bytes,
/// statement_heap_peak_bytes, statement_mapped_peak_bytes, account_bytes, heap_bytes, mapped_bytes, pool_reserved_bytes
/// (C8 move 3c: what the grant pool holds for the account's statements less what their draws already back with pages on
/// the doors, so a drawn byte counts once: account_bytes + pool_reserved_bytes over-reads the claim by no draw; a promise
/// of room, not memory; a worker's W and purse draws stay in it), pool_reserved_peak_bytes (the claim's most, backed or
/// not, since the running statement started), threads, fold_slack_bytes. Read now, in every broker mode. Then a '<cache> evicted_pinned' row for each cache whose
/// account holds more than its own count (account_bytes = heap_bytes = the excess, already inside the cache's account),
/// DEAD_BYTES: a '<cache> dead_bytes' row per cache that can tell (condition_cache), every read: the bytes of its account
/// no lookup reaches again (account_bytes = heap_bytes, inside the account; no class, so no sum takes it as an account),
/// and (G1-B) the resident memory: 'resident' (the memory governor's measure, through its probe: the process's RssAnon +
/// RssShmem, and under a cgroup limit at least the cgroup's memory.current less its clean page cache), under a cgroup
/// limit 'resident_cgroup' (the limiting cgroup's memory.stat anon + shmem), two accounts from the allocator's resident
/// bytes (the present pages of its `mimalloc`-named mappings: /proc/self/maps, then mincore over those ranges, at this
/// read) less what the accounts hold of its live blocks: 'allocator_retained' (RETAINED), the free blocks it keeps
/// resident (at least 0); and 'unattributed' = 'resident_cgroup' (else 'resident') less every account. Signed: an
/// account counts committed bytes, the kernel touched ones; above 0 it is memory no door counts (thread stacks, a C
/// library's memory, other processes in the cgroup). Last, for information only (never accounts, never summed), 'allocator_resident' and 'allocator_live' (the two inputs),
/// and 'allocator_committed_unused': the allocator's committed bytes less
/// its bytes in live blocks (its own count when armed, else door 1's total: every account's heap bytes), an upper bound
/// on its free-but-kept resident pages, since committed address space need not be resident.
/// D3872 / D3884: the view first runs the residency pass (`gravity_driver::mem::resident::pass`), then every account row
/// shows what the account has resident (its doors' bytes less the ones the pass found not resident: a region's unwritten or
/// given-back pages, a reaped mapping's freed ones), and two more information rows show those not-resident bytes, door 1's
/// ('not_resident') and door 2's ('not_resident_mapped'), each account's own in its last two columns. A third, 'given_back', shows the pages handed back to the kernel
/// that the allocator's own count still holds live (a region's fit, the reuse pool's idle release). RETAINED subtracts both
/// from the allocator's live bytes, as the accounts do. The two charged-not-resident lines G1-C had are gone: with the
/// account rows resident-true they subtracted the same pages a second time.
/// D3929: the two columns before the last are the governor's count of a backend's running statement (what the watchdog's
/// cancel ranks; NULL while the governor charges none), beside statement_bytes, the ledger's count on its own account.
/// C2: the last column, `class`, is each account's reclaim class (`memledger::Acct::class`: running, warm, cache, fixed,
/// pending, unbound); an '<cache> evicted_pinned' row is its cache's ('cache'); the derived rows from 'resident' on have
/// none (NULL): they are no account.
pub fn fc_pgrust_memory_accounts(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    use core::fmt::Write as _;
    let flinfo = flinfo.expect("pgrust_memory_accounts: resolved FmgrInfo required");
    let mcx = super::raw::result_mcx(fcinfo);
    let mut srf = funcapi::InitMaterializedSRF(mcx, flinfo, fcinfo, 0)?;
    declared(&srf, ACCOUNT_COLS)?;
    let mut out = Ok(());
    // D3872 / D3884: the regions' resident pages read now, so each account's charge and its residency are of one moment;
    // the rows show what each account has resident, the doors' not-resident bytes their own rows below
    let _ = gravity_driver::mem::resident::pass_all();
    let mut sums = (0i64, 0i64);
    let mut charged: mcx::PgVec<'_, (usize, isize, isize)> = mcx::PgVec::new_in(mcx);
    mcx::governor::for_each_statement(|slot, bytes, peak| charged.push((slot, bytes, peak)));
    memledger::for_each_row(|r| {
        add_row(&mut sums, r);
        if out.is_err() {
            return;
        }
        let stmt = statement_charged(r, &charged);
        let values: [Datum; ACCOUNT_COLS] = [
            text(mcx, r.acct.name()),
            Datum::from_i32(r.pid as i32),
            i8d(r.statement_seq),
            Datum::from_bool(r.running),
            i8d(r.statement_bytes),
            i8d(r.statement_peak_bytes),
            i8d(r.statement_heap_peak_bytes),
            i8d(r.statement_mapped_peak_bytes),
            Datum::from_i64(r.resident_bytes()),
            Datum::from_i64(r.resident_bytes().wrapping_sub(r.resident_mapped_bytes())),
            Datum::from_i64(r.resident_mapped_bytes()),
            Datum::from_i64(r.pool_reserved_bytes),
            Datum::from_i64(r.pool_reserved_peak_bytes),
            Datum::from_i32(r.threads as i32),
            i8d(r.fold_slack),
            Datum::from_i64(r.not_resident_bytes),
            Datum::from_i64(r.not_resident_mapped_bytes),
            i8d(stmt.map_or(0, |s| s.0)),
            i8d(stmt.map_or(0, |s| s.1)),
            text(mcx, r.acct.class().name()),
        ];
        let mut nulls = [false; ACCOUNT_COLS];
        nulls[1] = !r.acct.is_backend() || r.pid == 0;
        nulls[17] = stmt.is_none();
        nulls[18] = stmt.is_none();
        out = srf.putvalues(&values, &nulls);
    });
    out?;
    // step E: a cache's own count drops at an eviction, its account only at the entry's free, so an account above its
    // cache's count holds entries evicted but still pinned (a scan's Arc): one '<cache> evicted_pinned' row each. D4084:
    // the account's resident bytes (the pass above), not its charge: a dictionary image's reservation is on the part
    // cache's account whole, and only its faulted blocks are in the cache's count
    let hooks = memledger::all_hooks();
    for c in [Consumer::PartCache, Consumer::FpTables, Consumer::CondCache] {
        let Some(h) = hooks[c as usize] else { continue };
        let pinned = memledger::account_row(memledger::Acct::of(c)).resident_bytes().saturating_sub((h.current)() as i64);
        if pinned <= 0 {
            continue;
        }
        let zero = Datum::from_i64(0);
        let mut values = [zero; ACCOUNT_COLS];
        values[0] = text(mcx, EVICTED_PINNED[c as usize]);
        values[2] = zero;
        values[3] = Datum::from_bool(false);
        values[8] = Datum::from_i64(pinned);
        values[9] = Datum::from_i64(pinned);
        values[13] = Datum::from_i32(0);
        values[19] = text(mcx, c.class().name());
        let mut nulls = [false; ACCOUNT_COLS];
        nulls[1] = true;
        srf.putvalues(&values, &nulls)?;
    }
    // DEAD_BYTES: per cache that can tell, the bytes of its account no lookup can reach again (entries of a dropped
    // database, table or part not swept yet), one '<cache> dead_bytes' row each, every read (0 included). Information
    // inside the cache's account (no class: never summed as an account); its live LRU fill is not dead
    for (name, bytes) in [("condition_cache dead_bytes", crate::ops::scan::pgrc::cond_cache::dead_bytes())] {
        let zero = Datum::from_i64(0);
        let mut values = [zero; ACCOUNT_COLS];
        values[0] = text(mcx, name);
        values[3] = Datum::from_bool(false);
        values[8] = Datum::from_i64(bytes as i64);
        values[9] = Datum::from_i64(bytes as i64);
        values[13] = Datum::from_i32(0);
        let mut nulls = [false; ACCOUNT_COLS];
        nulls[1] = true;
        nulls[19] = true;
        srf.putvalues(&values, &nulls)?;
    }
    // G1-B: the resident memory the accounts are set against (the limiting cgroup's under a cgroup limit, else the
    // process's), and what no account counts of it
    if let Some(t) = resident_terms(sums) {
        let reuse = mcx::reuse_stats();
        let sources = mcx::alloc_sources();
        let purged = mcx::purged_bytes();
        let policy = mcx::purge_policy();
        let pressure = mcx::pressure_counts();
        let idle_trims = mcx::idle_trim_counts();
        let parked_trims = mcx::ladder::parked_trim_counts();
        let (depot_regions, depot_sets, depot_trimmable) = (
            gravity_driver::park::REGIONS.parked_bytes() as u64,
            gravity_driver::slots::DEPOT.stats().bytes as u64,
            gravity_driver::mem::grant::parked_bytes(),
        );
        let bg = mcx::purge_bg_counts();
        let rc = gravity_driver::mem::reclaim::counters();
        let r6 = memruntime::r6_hold_counts();
        let memlog = memruntime::memlog::counts();
        let resident = memruntime::residency_counts();
        let pmode = memruntime::purge::mode_counts();
        let gb = memruntime::service::counts();
        let gs = gravity_driver::mem::giveback::counts();
        let rows = [
            Some(("resident", t.resident)),
            t.cgroup.map(|c| ("resident_cgroup", c)),
            t.retained.map(|n| ("allocator_retained", n)),
            Some(("unattributed", t.unattributed)),
            // information only, never accounts: RETAINED's two inputs, and the allocator's committed bytes (every
            // heap's) less its live ones; committed is not resident (eager-committed and purged pages count), so that
            // one bounds RETAINED from above and is no part of 'unattributed'
            t.alloc_resident.map(|r| ("allocator_resident", r)),
            t.alloc_resident.map(|_| ("allocator_live", t.live)),
            // D3872: what the doors charge that the residency pass found not resident (off the account rows above)
            Some(("not_resident", t.nr_heap)),
            Some(("not_resident_mapped", t.nr_mapped)),
            Some(("given_back", t.given)),
            t.committed_unused.map(|c| ("allocator_committed_unused", c)),
            // C3 (purge owned): a COUNT, not bytes: the purges mimalloc's dead-man fallback ran on the cache arena
            // while the purger owned it (expected 0)
            Some(("cache_purge_fallbacks", i64::try_from(mcx::cache_purge_fallbacks()).unwrap_or(i64::MAX))),
            // REUSE BEFORE RELEASE (information only): the cache arena's bytes general heaps hold now, and since start
            // the claims (a COUNT), the bytes claimed, the attempts that found no committed free run (a COUNT)
            Some(("reuse_live", i64::try_from(reuse.0).unwrap_or(i64::MAX))),
            Some(("reuse_claims", i64::try_from(reuse.1).unwrap_or(i64::MAX))),
            Some(("reuse_claimed", i64::try_from(reuse.2).unwrap_or(i64::MAX))),
            Some(("reuse_misses", i64::try_from(reuse.3).unwrap_or(i64::MAX))),
            // REUSE (the C4 gate, information only): bytes new pages took from a general arena's own committed free runs,
            // and bytes the arenas committed afresh (general, cache), cumulative
            Some(("reuse_self_claimed", i64::try_from(sources.0).unwrap_or(i64::MAX))),
            Some(("fresh_commit_general", i64::try_from(sources.2).unwrap_or(i64::MAX))),
            Some(("fresh_commit_cache", i64::try_from(sources.3).unwrap_or(i64::MAX))),
            // the cache heap's new pages taken from the cache arena's own committed free runs (part reloads), cumulative
            Some(("reuse_cache_self_claimed", i64::try_from(sources.4).unwrap_or(i64::MAX))),
            Some(("reusable", i64::try_from(mcx::ladder::reusable_bytes()).unwrap_or(i64::MAX))),
            // information only: committed bytes purged so far by the purger thread and by every other thread (query
            // threads' collects, thread exits, the dead-man fallback), every arena
            Some(("purged_by_purger", i64::try_from(purged.0).unwrap_or(i64::MAX))),
            Some(("purged_by_other_threads", i64::try_from(purged.1).unwrap_or(i64::MAX))),
            // the purger's hold (information only): ticks under ladder pressure and the bytes purged on them, idle
            // purges and their bytes
            Some(("purge_ladder_ticks", i64::try_from(policy.0).unwrap_or(i64::MAX))),
            Some(("purge_ladder_bytes", i64::try_from(policy.1).unwrap_or(i64::MAX))),
            Some(("purge_idle_purges", i64::try_from(policy.2).unwrap_or(i64::MAX))),
            Some(("purge_idle_bytes", i64::try_from(policy.3).unwrap_or(i64::MAX))),
            // ladder pressure's purges (the purger's tick, or an allocator release under pressure only) and their bytes
            Some(("ladder_pressure_purges", i64::try_from(pressure.0).unwrap_or(i64::MAX))),
            Some(("ladder_pressure_bytes", i64::try_from(pressure.1).unwrap_or(i64::MAX))),
            // the idle release's trims of the parked regions and slot sets (COUNTS): run, and held for want of ladder
            // pressure (PGRUST_IDLE_TRIM_PRESSURE_ONLY)
            Some(("idle_trims_run", i64::try_from(idle_trims.0).unwrap_or(i64::MAX))),
            Some(("idle_trims_held", i64::try_from(idle_trims.1).unwrap_or(i64::MAX))),
            // P3 (`parked_trim_rung`, COUNTS): the ladder's R2p trims of the parked regions and slot sets: on the climbing
            // thread (runs, bytes), as the give-back service's jobs (posted, bytes asked; run, bytes trimmed)
            Some(("parked_trim_inline", i64::try_from(parked_trims[0]).unwrap_or(i64::MAX))),
            Some(("parked_trim_inline_bytes", i64::try_from(parked_trims[1]).unwrap_or(i64::MAX))),
            Some(("parked_trim_jobs", i64::try_from(parked_trims[2]).unwrap_or(i64::MAX))),
            Some(("parked_trim_jobs_asked_bytes", i64::try_from(parked_trims[3]).unwrap_or(i64::MAX))),
            Some(("parked_trim_jobs_run", i64::try_from(parked_trims[4]).unwrap_or(i64::MAX))),
            Some(("parked_trim_jobs_bytes", i64::try_from(parked_trims[5]).unwrap_or(i64::MAX))),
            // P3 (information, bytes now): the parked regions' lengths (the region depot's count: address space and pool
            // holds, not memory: a mapping's unwritten or purged pages and a heap region's given-back ones are in it) and
            // the parked slot sets' (the slot depot's count), and R2p's reach (every parked region's account bytes but the
            // newest's, and every slot set but the newest). What the depots hold resident is the ledger's: its 'parked'
            // and 'slot_depot' accounts (a creep reading takes those off anon)
            Some(("parked_depot_regions_mapped_bytes", i64::try_from(depot_regions).unwrap_or(i64::MAX))),
            Some(("parked_depot_slot_sets_mapped_bytes", i64::try_from(depot_sets).unwrap_or(i64::MAX))),
            Some(("parked_trim_reach_bytes", i64::try_from(depot_trimmable).unwrap_or(i64::MAX))),
            // PGRUST_PURGE_PRESSURE_BG (COUNTS): purger wakes from an allocator release under pressure, inline purges past
            // the watchdog's cancel line, the purger's trims of the parked regions and slot sets
            Some(("purge_bg_wakes", i64::try_from(bg.0).unwrap_or(i64::MAX))),
            Some(("purge_cancel_line_inline", i64::try_from(bg.1).unwrap_or(i64::MAX))),
            Some(("pressure_trims", i64::try_from(bg.2).unwrap_or(i64::MAX))),
            // the inline reclaim (a grant draw that found B full, on the query thread; information only): runs, summed and
            // longest microseconds, settles that gave back with a purge vs kept the freed pages for the draw (REUSE), bytes
            // purged by any thread while one ran
            Some(("reclaim_inline_runs", i64::try_from(rc.inline_runs).unwrap_or(i64::MAX))),
            Some(("reclaim_inline_us_sum", i64::try_from(rc.inline_ns_sum / 1000).unwrap_or(i64::MAX))),
            Some(("reclaim_inline_us_max", i64::try_from(rc.inline_ns_max / 1000).unwrap_or(i64::MAX))),
            Some(("reclaim_inline_give_backs", i64::try_from(rc.inline_give_backs).unwrap_or(i64::MAX))),
            Some(("reclaim_inline_reuse_kept", i64::try_from(rc.inline_reuse_kept).unwrap_or(i64::MAX))),
            Some(("reclaim_inline_purged", i64::try_from(rc.inline_purged_bytes).unwrap_or(i64::MAX))),
            // the ladder's R6 landing hold (information only): milliseconds of hook passes that still needed bytes while R6
            // was held for the last cancel's landing, and those passes with the read past the cancel line (a COUNT)
            Some(("r6_landing_held_ms", i64::try_from(r6.0).unwrap_or(i64::MAX))),
            Some(("r6_held_past_cancel", i64::try_from(r6.1).unwrap_or(i64::MAX))),
            // after a victim let go, the wait on the purger's tick that gives its pages back: milliseconds summed, and the
            // waits its bound (two purger periods) ended first (a COUNT)
            Some(("r6_purge_pending_ms", i64::try_from(r6.2).unwrap_or(i64::MAX))),
            Some(("r6_purge_wait_bounded", i64::try_from(r6.3).unwrap_or(i64::MAX))),
            // D4812 (information only): the memory threads' log writes handed to pg:memlog (COUNTS: queued, dropped with
            // its ring full, kept cut to its record size) and its longest write to the server log in ms (the sink's
            // stall the threads did not wait on)
            Some(("memlog_writes_queued", i64::try_from(memlog.0).unwrap_or(i64::MAX))),
            Some(("memlog_writes_dropped", i64::try_from(memlog.1).unwrap_or(i64::MAX))),
            Some(("memlog_writes_cut", i64::try_from(memruntime::memlog::cut_writes()).unwrap_or(i64::MAX))),
            Some(("memlog_write_ms_longest", i64::try_from(memlog.2).unwrap_or(i64::MAX))),
            // D4812 part 2 (information only): the residency pass on pg:memresident: its last pass's age on the watchdog's
            // clock (ms), the watchdog's samples that found it older than two periods (a COUNT), its passes (a COUNT)
            Some(("residency_age_ms", i64::try_from(resident.0).unwrap_or(i64::MAX))),
            Some(("residency_stale_samples", i64::try_from(resident.1).unwrap_or(i64::MAX))),
            Some(("residency_passes", i64::try_from(resident.2).unwrap_or(i64::MAX))),
            // the purger's pressure mode: ms in it, and its entries on the purger's own read past the line, on a ladder
            // pass that left a need, on an R6 victim's let-go (COUNTS)
            Some(("purger_pressure_mode_ms", i64::try_from(pmode.0).unwrap_or(i64::MAX))),
            Some(("purger_pressure_mode_entries_own_read", i64::try_from(pmode.1).unwrap_or(i64::MAX))),
            Some(("purger_pressure_mode_entries_ask", i64::try_from(pmode.2).unwrap_or(i64::MAX))),
            Some(("purger_pressure_mode_entries_let_go", i64::try_from(pmode.3).unwrap_or(i64::MAX))),
            // H3 step (1) (give_back_service, information only): the give-back service's one landing account (bytes
            // enqueued, not yet given back; the own runs' landing in step (1)) and its backlog cap (0 = no service); the
            // batches its thread gave back (a COUNT) and their bytes; the runs it turned away, given back on their freeing
            // threads, at the cap and past its dead-man stretch (COUNTS) and their bytes; the drains of its landing on a
            // drawing thread, make_room past the governor's bar (a COUNT), and their bytes
            Some(("give_back_landing", i64::try_from(gb.landing).unwrap_or(i64::MAX))),
            Some(("give_back_cap", i64::try_from(gb.cap).unwrap_or(i64::MAX))),
            Some(("give_back_batches", i64::try_from(gb.batches).unwrap_or(i64::MAX))),
            Some(("give_back_given", i64::try_from(gb.given).unwrap_or(i64::MAX))),
            Some(("give_back_cap_fallbacks", i64::try_from(gb.cap_runs).unwrap_or(i64::MAX))),
            Some(("give_back_cap_fallback_bytes", i64::try_from(gb.cap_bytes).unwrap_or(i64::MAX))),
            Some(("give_back_dead_man_fallbacks", i64::try_from(gb.dead_runs).unwrap_or(i64::MAX))),
            Some(("give_back_dead_man_fallback_bytes", i64::try_from(gb.dead_bytes).unwrap_or(i64::MAX))),
            Some(("give_back_drawer_drains", i64::try_from(gb.drawer_drains).unwrap_or(i64::MAX))),
            Some(("give_back_drawer_drained_bytes", i64::try_from(gb.drawer_bytes).unwrap_or(i64::MAX))),
            // H3 step (2) (give_back_service, information only): the arenas whose purge the service owns and the owner takes
            // refused (either owner's; COUNTS); the forced purges handed to the service and those run on their callers past
            // the watchdog's cancel line (COUNTS); the service's arena purges that purged and its forced passes (COUNTS), its
            // decommits through the arenas' commit callback (a COUNT) and their bytes; the ladder's pressure purge's bytes
            // from arenas an owner held (purged_by_pressure: owner-agnostic, so no double purge)
            Some(("give_back_arenas_owned", i64::try_from(gb.arenas_owned).unwrap_or(i64::MAX))),
            Some(("give_back_owner_refusals", i64::try_from(gb.owner_refusals).unwrap_or(i64::MAX))),
            Some(("give_back_forced_jobs", i64::try_from(gb.forced_jobs).unwrap_or(i64::MAX))),
            Some(("give_back_forced_inline_past_cancel", i64::try_from(gb.forced_inline).unwrap_or(i64::MAX))),
            Some(("give_back_forced_guard_busy", i64::try_from(gb.forced_guard_busy).unwrap_or(i64::MAX))),
            // C2's measure: the busy-guard hand-offs served, and the longest from a hand-off to the end of its forced pass
            Some(("give_back_forced_handoffs", i64::try_from(gb.forced_handoffs).unwrap_or(i64::MAX))),
            Some(("give_back_forced_handoff_max_us", i64::try_from(gb.forced_handoff_max_us).unwrap_or(i64::MAX))),
            Some(("give_back_purges", i64::try_from(gb.purges).unwrap_or(i64::MAX))),
            Some(("give_back_forced_passes", i64::try_from(gb.forced_passes).unwrap_or(i64::MAX))),
            Some(("give_back_purge_decommits", i64::try_from(gb.purged_calls).unwrap_or(i64::MAX))),
            Some(("give_back_purged_bytes", i64::try_from(gb.purged_bytes).unwrap_or(i64::MAX))),
            Some(("purged_by_pressure", i64::try_from(gb.pressure_owned_bytes).unwrap_or(i64::MAX))),
        ];
        // one information row (a name and its bytes or count)
        let mut put = |name: &str, bytes: i64| {
            let zero = Datum::from_i64(0);
            let mut values = [zero; ACCOUNT_COLS];
            values[0] = text(mcx, name);
            values[3] = Datum::from_bool(false);
            values[8] = Datum::from_i64(bytes);
            values[13] = Datum::from_i32(0);
            let mut nulls = [false; ACCOUNT_COLS];
            nulls[1] = true;
            nulls[19] = true;
            srf.putvalues(&values, &nulls)
        };
        for (name, bytes) in rows.into_iter().flatten() {
            put(name, bytes)?;
        }
        for (name, n) in give_back_spill_rows(&gs) {
            put(name, n)?;
        }
        // REUSE BEFORE RELEASE sizing (PGRUST_REUSE_RUNS=1, information only): the cache arena's free bytes by run length,
        // still committed (cache_run_c_<slices>) and all (cache_run_a_<slices>), log2 buckets from <slices> (64 KiB each);
        // each name is written on the stack
        if let Some((c, a)) = mcx::cache_runs() {
            for (tag, h) in [("c", c), ("a", a)] {
                for (k, b) in h.iter().enumerate() {
                    if *b > 0 {
                        let mut name = RungText { b: [0; 256], n: 0 };
                        let _ = write!(name, "cache_run_{tag}_{}", 1u64 << k);
                        put(name.as_str(), i64::try_from(*b).unwrap_or(i64::MAX))?;
                    }
                }
            }
        }
        // purge timing (information only): purge events by thread (the purger; a session inside a statement, on the
        // client's latency; a session after its ReadyForQuery, off it; any other thread; an executor worker), each a COUNT,
        // summed and longest MICROSECONDS, and bytes decommitted, cumulative
        for (class, t) in ["purger", "statement", "after_rfq", "other", "worker"].iter().zip(mcx::purge_times()) {
            if t[0] > 0 {
                for (what, v) in [("n", t[0]), ("us", t[1] / 1000), ("max_us", t[2] / 1000), ("bytes", t[3])] {
                    let mut name = RungText { b: [0; 256], n: 0 };
                    let _ = write!(name, "purge_time_{class}_{what}");
                    put(name.as_str(), i64::try_from(v).unwrap_or(i64::MAX))?;
                }
            }
        }
    }
    Ok(srf.finish(fcinfo))
}

/// The give-back lane's information rows (H3 step (4)), in the accounts view after the service's rows.
fn give_back_spill_rows(gs: &gravity_driver::mem::giveback::Counts) -> impl IntoIterator<Item = (&'static str, i64)> {
    [
        // H3 step (4) (give_back_spill under give_back_service, information only): the whole-region job lane's bytes
        // waiting (in give_back_landing too); its jobs (a COUNT) and their bytes; those given back by the service's drain
        // (a COUNT) and their bytes; the gives run on their callers past the watchdog's cancel line, at the cap or a full
        // queue, past the service's dead-man stretch, and with no pool room for the hand-off (COUNTS); the trims that
        // gave jobs back on their own thread past the cancel line, pages first (a COUNT, and their bytes: a trim never
        // waits for the service); and (ii)'s inline give-backs skipped for a mapped region of REAP_MIN or more and for a
        // reserved region's own drop, and kept for a mapped one under REAP_MIN (COUNTS); C1-c's park jobs (a COUNT) and
        // their bytes, those the service gave back and parked (a COUNT) and their bytes, puts past the cancel line, takes
        // that missed while park jobs waited, and tails the kernel left writable (COUNTS), and those a draw's trim gave
        // back on its own thread past the cancel line (a COUNT) and their bytes;
        // give_back_spill_pending: an allocator reservation's committed bytes queued; take-back's whole gives a region draw
        // of their shape took back off the lane (a COUNT) and their bytes; the grant pool takes that held past the pool's
        // limit on the lane's landing (a COUNT), their bytes past it, and the largest overage (bytes)
        ("give_back_spill_landing", i64::try_from(gs.landing).unwrap_or(i64::MAX)),
        ("give_back_spill_pending", i64::try_from(gs.pending).unwrap_or(i64::MAX)),
        ("give_back_spill_jobs", i64::try_from(gs.jobs).unwrap_or(i64::MAX)),
        ("give_back_spill_job_bytes", i64::try_from(gs.job_bytes).unwrap_or(i64::MAX)),
        ("give_back_spill_given", i64::try_from(gs.given).unwrap_or(i64::MAX)),
        ("give_back_spill_given_bytes", i64::try_from(gs.given_bytes).unwrap_or(i64::MAX)),
        ("give_back_spill_inline_past_cancel", i64::try_from(gs.inline_past_cancel).unwrap_or(i64::MAX)),
        ("give_back_spill_cap_fallbacks", i64::try_from(gs.cap_fallbacks).unwrap_or(i64::MAX)),
        ("give_back_spill_dead_man_fallbacks", i64::try_from(gs.dead_man_fallbacks).unwrap_or(i64::MAX)),
        ("give_back_spill_pool_fallbacks", i64::try_from(gs.pool_fallbacks).unwrap_or(i64::MAX)),
        ("give_back_spill_drained_here_past_cancel", i64::try_from(gs.drained_here_past_cancel).unwrap_or(i64::MAX)),
        ("give_back_spill_drained_here_past_cancel_bytes", i64::try_from(gs.drained_here_past_cancel_bytes).unwrap_or(i64::MAX)),
        ("give_back_spill_to_reaper", i64::try_from(gs.to_reaper).unwrap_or(i64::MAX)),
        ("give_back_spill_reserved_own_drop", i64::try_from(gs.reserved_own_drop).unwrap_or(i64::MAX)),
        ("give_back_spill_small_mapped_inline", i64::try_from(gs.small_mapped_inline).unwrap_or(i64::MAX)),
        ("give_back_park_jobs", i64::try_from(gs.park_jobs).unwrap_or(i64::MAX)),
        ("give_back_park_job_bytes", i64::try_from(gs.park_job_bytes).unwrap_or(i64::MAX)),
        ("give_back_parked", i64::try_from(gs.parked).unwrap_or(i64::MAX)),
        ("give_back_parked_bytes", i64::try_from(gs.parked_bytes).unwrap_or(i64::MAX)),
        ("give_back_park_inline_past_cancel", i64::try_from(gs.park_inline_past_cancel).unwrap_or(i64::MAX)),
        ("give_back_park_take_misses", i64::try_from(gs.park_take_misses).unwrap_or(i64::MAX)),
        ("give_back_park_protect_refused", i64::try_from(gs.park_protect_refused).unwrap_or(i64::MAX)),
        ("give_back_park_drained_here", i64::try_from(gs.park_drained_here).unwrap_or(i64::MAX)),
        ("give_back_park_drained_here_bytes", i64::try_from(gs.park_drained_here_bytes).unwrap_or(i64::MAX)),
        ("give_back_spill_taken_back", i64::try_from(gs.taken_back).unwrap_or(i64::MAX)),
        ("give_back_spill_taken_back_bytes", i64::try_from(gs.taken_back_bytes).unwrap_or(i64::MAX)),
        ("give_back_spill_overdraft", i64::try_from(gs.overdraft).unwrap_or(i64::MAX)),
        ("give_back_spill_overdraft_bytes", i64::try_from(gs.overdraft_bytes).unwrap_or(i64::MAX)),
        ("give_back_spill_overdraft_max", i64::try_from(gs.overdraft_max).unwrap_or(i64::MAX)),
    ]
}

/// One account row into the two sums G1-B sets against the resident memory: every account's resident bytes, and door
/// 1's live bytes as charged (the allocator's live blocks, resident or not).
fn add_row(sums: &mut (i64, i64), r: &memledger::AccountRow) {
    sums.0 = sums.0.wrapping_add(r.resident_bytes());
    sums.1 = sums.1.wrapping_add(r.account_bytes.wrapping_sub(r.mapped_bytes));
}

/// G1-B's terms (`resident_terms`): what `pgrust_memory_accounts()` shows from its 'resident' row on, and C8 checker 2's
/// global term ('unattributed') in `pgrust_memory_owner_end()`.
struct ResidentTerms {
    resident: i64,
    cgroup: Option<i64>,
    retained: Option<i64>,
    unattributed: i64,
    alloc_resident: Option<i64>,
    live: i64,
    nr_heap: i64,
    nr_mapped: i64,
    given: i64,
    committed_unused: Option<i64>,
    /// pppress: `alloc_resident`'s mimalloc term (its named mappings' present bytes; the rest is the pool's present
    /// bytes), and `live`'s front term (`init_small::oom::allocated_bytes`: the global allocator's live blocks,
    /// PoolAlloc's among them; the rest is the pool's mcx bytes), each from the SAME read as the sum; door 1's pool bytes
    /// (`memledger::pool_door1`: PoolAlloc's live blocks, 0 while it does not serve) read right after.
    mim_maps: Option<i64>,
    front: Option<i64>,
    door1: i64,
}

/// The resident memory read now, set against `sums` (`add_row` over every account row, read after the residency pass):
/// None when the governor has no resident probe. One formula for both views.
fn resident_terms(sums: (i64, i64)) -> Option<ResidentTerms> {
    let (accounts, heap) = sums;
    let bytes = |b: usize| b.min(i64::MAX as usize) as i64;
    let resident = mcx::governor::probe_resident().map(bytes)?;
    let cgroup = mcx::governor::probe_cgroup_resident().map(bytes);
    // D3872: read beside the account rows, before the allocator's mappings are walked: the not-resident bytes the
    // residency pass took off the accounts, and the pages given back (a region's fit, the reuse pool's idle release),
    // which the allocator's own count still holds live
    let (nr_heap, nr_mapped) = memledger::not_resident_bytes();
    let given = memledger::given_back_bytes();
    let _ = init_small::oom::take_last_allocated();
    let counted = mcx::governor::allocator_in_use().map(bytes);
    // pppress: the front's term of that same read, and door 1's pool bytes beside it
    let front = counted.and(init_small::oom::take_last_allocated()).map(bytes);
    let door1 = memledger::pool_door1();
    let live = counted.unwrap_or(heap);
    // the allocator's resident bytes (its named mappings' present pages) less what the accounts hold of it (its live
    // blocks less the given-back and the not-resident bytes): RETAINED, the free blocks it keeps resident. An account,
    // 0 when the accounts read above it (a region moved between the account rows and this read); with the account rows
    // resident-true there is no charged-but-not-resident line any more (D3872: it subtracted the same pages twice)
    let _ = memruntime::take_allocator_resident_split();
    let alloc_resident = mcx::governor::probe_allocator_resident().map(bytes);
    // pppress: the mimalloc term of that same read
    let mim_maps = alloc_resident.and(memruntime::take_allocator_resident_split()).map(|(maps, _)| bytes(maps));
    let charged_live = counted.map_or(heap, |c| c.wrapping_sub(given));
    let retained = alloc_resident.map(|r| memledger::arena::retained_of(r, charged_live, nr_heap));
    let unattributed = cgroup.unwrap_or(resident).wrapping_sub(accounts).wrapping_sub(retained.unwrap_or(0));
    let committed_unused = mcx::governor::allocator_committed().map(|c| bytes(c).wrapping_sub(live));
    Some(ResidentTerms { resident, cgroup, retained, unattributed, alloc_resident, live, nr_heap, nr_mapped, given, committed_unused, mim_maps, front, door1 })
}

/// A row's governor count from `charged` (slot = process number, bytes, peak); `None`: a running statement uncharged.
fn statement_charged(r: &memledger::AccountRow, charged: &[(usize, isize, isize)]) -> Option<(u64, u64)> {
    let procno = if r.acct.is_backend() { Some((r.acct.0 - memledger::BACKEND_SLOT_BASE) as usize) } else { None };
    match procno.and_then(|p| charged.iter().find(|c| c.0 == p)) {
        Some(&(_, bytes, peak)) => Some((bytes.max(0) as u64, peak.max(bytes).max(0) as u64)),
        None if r.running => None,
        None => Some((0, 0)),
    }
}

/// The sub-rows' names, by consumer (only the three caches whose account moves back at the free have one).
const EVICTED_PINNED: [&str; memledger::CONSUMERS] =
    ["", "part_cache evicted_pinned", "fp_tables evicted_pinned", "", "", "cond_cache evicted_pinned", "", "", "", "", "", ""];

/// The kind rows' columns (scripts/memory-broker-views.sql declares the same, in order).
const KIND_COLS: usize = 7;

/// G1-A `pgrust_memory_account_kinds()`: each running statement's Gravity region bytes by the breaker of the pipeline that
/// made them ('region' outside every breaker's; allocator and mapped regions alike) and every other mapped byte as
/// 'mapped': pid, statement_seq, kind, bytes (now, less at the statement's start), peak_bytes (since its start), then door
/// 2's alone: mapped_bytes, mapped_peak_bytes.
pub fn fc_pgrust_memory_account_kinds(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let flinfo = flinfo.expect("pgrust_memory_account_kinds: resolved FmgrInfo required");
    let mcx = super::raw::result_mcx(fcinfo);
    let mut srf = funcapi::InitMaterializedSRF(mcx, flinfo, fcinfo, 0)?;
    declared(&srf, KIND_COLS)?;
    let mut out = Ok(());
    memledger::for_each_kind_row(|r| {
        if out.is_err() {
            return;
        }
        let values: [Datum; KIND_COLS] = [
            Datum::from_i32(r.pid as i32),
            i8d(r.statement_seq),
            text(mcx, r.kind.name()),
            Datum::from_i64(r.bytes),
            i8d(r.peak_bytes),
            Datum::from_i64(r.mapped_bytes),
            i8d(r.mapped_peak_bytes),
        ];
        out = srf.putvalues(&values, &[false; KIND_COLS]);
    });
    out?;
    Ok(srf.finish(fcinfo))
}

/// The owner-end rows' columns (scripts/memory-broker-views.sql declares the same, in order).
const OWNER_END_COLS: usize = 10;

/// C8 checker 2's cache owners: each one's account set against its own count (`Hooks::current`).
const RECONCILED: [Consumer; 5] = [Consumer::PartCache, Consumer::CondCache, Consumer::ZoneStore, Consumer::FpTables, Consumer::ProgramCaches];

/// C8 checker 1 `pgrust_memory_owner_end()` (memledger::owner_end): kind, source, class, events, committed_bytes,
/// reserved_bytes, uses, account, pid, seq. The kinds:
/// - 'total': per source ('owner': an owner cell's residue at its end; 'legacy': the ledger's books before the area moves)
///   and owner class, the ends that found a residue since the server started, their committed bytes and uses.
/// - 'counter': 'stale' (charges that outlived their owner), 'overflow' (owner begins past a full table) and
///   'door_overflow' (mcx mapped blocks the home's address table had no room for: C8 C1) in events;
///   'unbind' the unbinds (events) and the bytes they moved to UNBOUND (committed_bytes); 'unbound' UNBOUND's bytes now;
///   source 'crashed', 'unbind': the dead sessions unbound at crash cycles (C8 move 6, D2953: a server that reinitializes
///   in its own process), the bytes moved to UNBOUND (committed_bytes) and the pool reservations dropped (reserved_bytes).
/// - 'now': each bound backend's session that owns something now or has a live portal (events = its live portals): a use
///   on a session is a kept cursor (D1507), what a parked Node keeps between statements; and each cache holder that owns
///   something (class 'cache', account the holder's: a use on 'parked' is a parked worker set's kept cursor, D4087).
/// - 'reconcile' (C8 checker 2, report only: the stress kit judges each term against its tolerance, max(256 MiB,
///   memory.max / 64)): source 'global', class 'unattributed': committed_bytes = `pgrust_memory_accounts()`'s signed
///   'unattributed' (the cgroup's resident memory, else the process's, less every account and the allocator's retained
///   bytes), one formula for both views; reserved_bytes = the basis it subtracted from (the cgroup's resident memory,
///   else the governed resident), read with it (D4342); and source 'cache', one row per cache owner (`RECONCILED`), class the cache:
///   committed_bytes = its account's resident bytes less its own count (the census, reserved_bytes), account the cache's.
///   A cache holding bytes its census misses (D4135) reads above 0, one counting what no door charged reads below.
///   pagepool P2, only while the pool is armed: source 'pool', class 'pool_owned' (the owners' pool bytes: the contexts'
///   pooled bytes and, pppress card 9, door 1's, `memledger::pool_door1`, `PoolAlloc`'s blocks that `doors_total` counts on
///   their accounts, so no pool byte is both on an account and unowned), 'pool_door1' (of 'pool_owned', door 1's), 'pool_free'
///   (pppress card 9b: 'pool_small_slack', 'pool_owner_rounding' and 'pool_run_tails' name the rest of 'pool_unowned' past
///   'pool_free' and 'pool_stranded', `pool_unowned_terms`; pppress card 13 (2): 'pool_commit_past_ask', the live lazy
///   spans' commit past their takes' asks, a warm commit kept with PGRUST_POOL_SPAN_TRIM off, and
///   'pool_commit_past_ask_tag' rows by tag, events the tag),
///   (warm, central, shelved, dead), 'pool_stranded', 'pool_unowned' (held less owned: free, shelved, stranded and class
///   rounding; retained, never in the unattributed row; held = claimed, or the pool's resident bytes where they read below
///   it, D4790; below 0 where owners count bytes that are not resident, an owner charging a declared span's untouched
///   pages: itself a finding, never clamped) and 'pool_untouched' (claimed less held: declared, never touched) in
///   committed_bytes, the pool's claimed bytes in reserved_bytes. The resident bytes are the purger's last sweep
///   (`pagepool::present`, as the unattributed row's allocator term) while claimed and owned read now, so
///   'pool_unowned' / 'pool_untouched' carry the sweep's age in ms as events (NULL before the first sweep): an A/B reads
///   them right after a sweep. pppress card 11: 'pool_present' (the sweep's bytes, pool and metadata, events its age),
///   'pool_present_zero' (the zero-page bytes it left out; events 1 where mincore read it, which counts them resident)
///   and 'pool_present_sweep' (events its cost in microseconds). pppress card V (report only, the allocator gate's creep
///   key): 'pool_free_present' (the sweep's present bytes in runs that hold no live block: free superpages, free and dead
///   runs, blocks runs whose blocks are all home, warm lazy spans; classified in the same pass, so 0 <= it <= present;
///   threads' shelves, the central lists and the small tier's free slots are not in it) and 'pool_cache_present' (its
///   present bytes in CACHE-tag allocations holding a live block), events the sweep's age; pppress (C, report only):
///   'pool_build_present' (the same read's present bytes in BUILD-tag allocations holding a live block: the resident fp
///   tables' build buffers, the build pool's kept ones and in-flight builds'; never in cache), events the sweep's age;
///   and 'pool_free_listed' (report
///   only: the CLAIMED bytes of free blocks pool_free_present cannot tell from live ones, on threads' shelves, on the
///   central lists and parked in runs not wholly free: parts of 'pool_free' and 'pool_stranded', present or not; with it
///   the free present bytes lie between pool_free_present and pool_free_present + it). The watchdog's projection
///   (`memledger::pool_unowned`) stays claimed less owned (door 1's included), so it and this row differ by design.
///   pppress (report only): 'mimalloc_resident' and 'mimalloc_live', the mimalloc terms of the allocator's resident
///   and live bytes from the same read as 'unattributed' (reserved_bytes = the sums, so reserved - committed = the
///   pool's part of each).
/// - 'in_use' (D3204, report only): source 'part_cache', one row per holder (`memledger::charge::HOLDER_NAMES`): the part
///   cache's in-use floor (the cached bytes of parts with a live use, which no trim may evict) split by who holds the uses,
///   events = (part, owner) entries, committed_bytes = their bytes.
/// - 'recent': the latest residues, newest first (seq: the residue's number since the server started).
pub fn fc_pgrust_memory_owner_end(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    use memledger::owner_end::Source;
    use memledger::OwnerClass;
    let flinfo = flinfo.expect("pgrust_memory_owner_end: resolved FmgrInfo required");
    let mcx = super::raw::result_mcx(fcinfo);
    let mut srf = funcapi::InitMaterializedSRF(mcx, flinfo, fcinfo, 0)?;
    declared(&srf, OWNER_END_COLS)?;
    let null = Datum::from_i64(0);
    // (kind, source, class, events, committed, reserved, uses, account, pid, seq); None = NULL
    let mut put = |kind: &str, src: &str, class: &str, n: [Option<i64>; 4], acct: Option<&str>, pid: Option<u32>, seq: Option<u64>| {
        let values: [Datum; OWNER_END_COLS] = [
            text(mcx, kind),
            text(mcx, src),
            text(mcx, class),
            n[0].map_or(null, Datum::from_i64),
            n[1].map_or(null, Datum::from_i64),
            n[2].map_or(null, Datum::from_i64),
            n[3].map_or(null, Datum::from_i64),
            acct.map_or(null, |a| text(mcx, a)),
            pid.map_or(null, |p| Datum::from_i32(p as i32)),
            seq.map_or(null, i8d),
        ];
        let nulls = [false, false, false, n[0].is_none(), n[1].is_none(), n[2].is_none(), n[3].is_none(), acct.is_none(), pid.is_none(), seq.is_none()];
        srf.putvalues(&values, &nulls)
    };
    let sat = |x: u64| i64::try_from(x).unwrap_or(i64::MAX);
    for src in Source::ALL {
        for class in OwnerClass::ALL {
            let t = memledger::owner_end::totals(src, class);
            put("total", src.name(), class.name(), [Some(sat(t.events)), Some(sat(t.committed)), None, Some(sat(t.uses))], None, None, None)?;
        }
    }
    let (stale, overflow, _, _) = memledger::charge::counters();
    put("counter", "owner", "stale", [Some(sat(stale)), None, None, None], None, None, None)?;
    put("counter", "owner", "overflow", [Some(sat(overflow)), None, None, None], None, None, None)?;
    // C8 C1: the mcx mapped blocks the home's address table had no room for (counted on an account only; checker 1 reads it red)
    put("counter", "owner", "door_overflow", [Some(sat(memledger::charge::door_overflow())), None, None, None], None, None, None)?;
    let (unbinds, moved, unbound) = memledger::owner_end::unbinds();
    put("counter", "legacy", "unbind", [Some(sat(unbinds)), Some(moved), None, None], None, None, None)?;
    put("counter", "legacy", "unbound", [None, Some(unbound), None, None], None, None, None)?;
    let (crashed, moved, dropped) = memledger::owner_end::crashed_unbinds();
    put("counter", "crashed", "unbind", [Some(sat(crashed)), Some(moved), Some(dropped), None], None, None, None)?;
    // D4552 (a) backstop (the `ledger_floor` knob): the folds that credited UNBOUND, not a backend row below zero
    let (floor_events, floored) = memledger::floored();
    put("counter", "legacy", "floored", [Some(sat(floor_events)), Some(floored), None, None], None, None, None)?;
    // D4552 (a) (the `ledger_unbind_wait` knob): the unbinds that waited for a thread inside their account, the longest
    // wait (ns), and the waits that hit memledger::UNBIND_WAIT_CAP_NS
    let (waited, longest_ns, capped) = memledger::unbind_waits();
    put("counter", "legacy", "unbind_wait", [Some(sat(waited)), Some(sat(longest_ns)), Some(sat(capped)), None], None, None, None)?;
    // D3204: the part cache's in-use floor by holder
    let names = memledger::charge::HOLDER_NAMES;
    let mut in_use = [(0u64, 0u64); memledger::charge::HOLDER_NAMES.len()];
    pgrc_am::inval::registry().in_use_by_owner(|a, key, bytes| {
        let h = memledger::charge::holder_name(memledger::Acct(a), key);
        if let Some(i) = names.iter().position(|&n| n == h) {
            in_use[i].0 += 1;
            in_use[i].1 = in_use[i].1.saturating_add(bytes);
        }
    });
    for (name, (n, bytes)) in names.iter().zip(in_use) {
        put("in_use", "part_cache", name, [Some(sat(n)), Some(sat(bytes)), None, None], None, None, None)?;
    }
    let mut out = Ok(());
    memledger::charge::for_each_session(|a, row, live| {
        if out.is_ok() {
            let n = [Some(live as i64), Some(row.committed), Some(row.reserved), Some(row.uses)];
            out = put("now", "owner", "session", n, Some(a.name()), Some(memledger::backend_pid(a)), None);
        }
    });
    out?;
    let mut out = Ok(());
    memledger::charge::for_each_holder(|h, row| {
        if out.is_ok() {
            let n = [None, Some(row.committed), Some(row.reserved), Some(row.uses)];
            out = put("now", "owner", "cache", n, Some(h.acct().name()), None, None);
        }
    });
    out?;
    // checker 2 (report): the reconcile terms, read as `pgrust_memory_accounts()` reads them (after the residency pass)
    let _ = gravity_driver::mem::resident::pass_all();
    let mut sums = (0i64, 0i64);
    memledger::for_each_row(|r| add_row(&mut sums, r));
    let terms = resident_terms(sums);
    if let Some(t) = &terms {
        // D4342: on this row only, reserved_bytes = the BASIS the term subtracted from (the cgroup's resident memory, else
        // the governed resident), so a check can take the server's own part (committed - reserved = -(accounts +
        // retained)) apart from the basis a shared scope's neighbours move between two reads. The builtin's columns are a
        // pg_proc row (9074): a new named column would be a catalog change, so the NULL column carries it.
        put("reconcile", "global", "unattributed", [None, Some(t.unattributed), Some(t.cgroup.unwrap_or(t.resident)), None], None, None, None)?;
    }
    let hooks = memledger::all_hooks();
    for c in RECONCILED {
        let Some(h) = hooks[c as usize] else { continue };
        let a = memledger::Acct::of(c);
        let resident = memledger::account_row(a).resident_bytes();
        let census = sat((h.current)());
        put("reconcile", "cache", c.name(), [None, Some(resident.wrapping_sub(census)), Some(census), None], Some(a.name()), None, None)?;
    }
    // pagepool P2: the pool's bytes beside the accounts (committed_bytes; reserved_bytes = its claimed), only while armed
    if memledger::pool_probed() {
        let p = memledger::pool_reading();
        let claimed = Some(sat(p.claimed));
        // pppress card 9: door 1's pool bytes (`PoolAlloc`'s blocks, on their accounts) are owned too, counted once
        let door1 = memledger::pool_door1();
        let pooled = memledger::pagepool_total();
        let owned = pooled.wrapping_add(door1);
        // pppress card 9b: the rest of unowned by name, from the pool's in-use bytes by tag (one snapshot)
        let st = pagepool::stats();
        let (small_slack, owner_rounding, run_tails) = pool_unowned_terms(&st, door1, pooled);
        // D4790: the pool's resident bytes (present less its metadata zone, which claimed never counts)
        let resident = pagepool::present().map(|r| sat(r.saturating_sub(pagepool::meta_present().unwrap_or(0)) as u64));
        // the sweep's age (ms) beside the two rows read from it
        let age = resident.and(pagepool::present_age_ms()).map(sat);
        let (unowned, untouched) = pool_ownerless(sat(p.claimed), resident, owned);
        let rows = [
            ("pool_owned", owned, None),
            ("pool_door1", door1, None),
            ("pool_free", sat(p.warm_free), None),
            ("pool_stranded", sat(p.stranded), None),
            // pppress (report only): the claimed free bytes the present sweep cannot class, beside pool_free_present
            ("pool_free_listed", sat((st.shelved + st.central + st.parked) as u64), None),
            ("pool_unowned", unowned, age),
            ("pool_small_slack", small_slack, None),
            ("pool_owner_rounding", owner_rounding, None),
            ("pool_run_tails", run_tails, None),
            ("pool_untouched", untouched, age),
            ("pool_commit_past_ask", sat(st.commit_past_ask.iter().sum::<usize>() as u64), None),
        ];
        for (class, b, events) in rows {
            put("reconcile", "pool", class, [events, Some(b), claimed, None], None, None, None)?;
        }
        // pppress card 13 (2): 'pool_commit_past_ask' by tag (events the tag), the tags holding any
        for (t, &b) in st.commit_past_ask.iter().enumerate().filter(|&(_, &b)| b > 0) {
            put("reconcile", "pool", "pool_commit_past_ask_tag", [Some(t as i64), Some(sat(b as u64)), claimed, None], None, None, None)?;
        }
        // pppress card 11: the last sweep: 'pool_present' (its bytes, metadata included; events its age ms),
        // 'pool_present_zero' (the zero-page bytes it left out; events 1 where its reader, mincore, counts them in) and
        // 'pool_present_sweep' (events its cost, us). pppress card V (report only): 'pool_free_present' and
        // 'pool_cache_present', the same sweep's present bytes in runs holding no live block and in CACHE-tag allocations
        // holding one, events its age (ms); one read with 'pool_present' (`present_split`), so free + cache <= present.
        // pppress (C, report only): 'pool_build_present', the same read's present bytes in BUILD-tag allocations holding
        // a live block (the resident fp tables' buffers, the build pool's kept ones, in-flight builds'), never in cache
        if let (Some((all, free, cache, build)), Some((scanned, zero))) = (pagepool::present_split_build(), pagepool::present_zero()) {
            let age = pagepool::present_age_ms().map(sat);
            put("reconcile", "pool", "pool_present", [age, Some(sat(all as u64)), claimed, None], None, None, None)?;
            put("reconcile", "pool", "pool_present_zero", [Some(i64::from(!scanned)), Some(sat(zero as u64)), claimed, None], None, None, None)?;
            put("reconcile", "pool", "pool_present_sweep", [Some(sat(pagepool::present_sweep_ns() / 1000)), None, None, None], None, None, None)?;
            put("reconcile", "pool", "pool_free_present", [age, Some(sat(free as u64)), claimed, None], None, None, None)?;
            put("reconcile", "pool", "pool_cache_present", [age, Some(sat(cache as u64)), claimed, None], None, None, None)?;
            put("reconcile", "pool", "pool_build_present", [age, Some(sat(build as u64)), claimed, None], None, None, None)?;
        }
        // pppress (report only; the allocator gate's mimalloc term on a pool bin), from the SAME read as the unattributed
        // row's allocator terms (`resident_terms`, the accounts view's 'allocator_resident' / 'allocator_live'):
        // 'mimalloc_resident' = that read's mimalloc term (present bytes of the [anon:mimalloc] mappings; reserved_bytes
        // = allocator_resident, so reserved - committed = the pool's present_fresh bytes; events = the pool sweep's
        // age, ms); 'mimalloc_live' = its front term (`oom::allocated_bytes`, PGRUST_ALLOC_COUNT=1, else no row) less
        // door 1's pool bytes. The front is the #[global_allocator] (`oom::Rescue<PoolAlloc>` here), so PoolAlloc's
        // blocks are in it; door 1 (`pagepool::global_live`) takes them off. reserved_bytes = allocator_live, so
        // reserved - committed = the pool's part of in_use: the pool's mcx bytes + door 1. Not clamped (a read skew
        // between the front and door 1 shows below 0).
        if let Some(t) = &terms {
            let age = pagepool::present_age_ms().map(sat);
            if let (Some(maps), Some(all)) = (t.mim_maps, t.alloc_resident) {
                put("reconcile", "pool", "mimalloc_resident", [age, Some(maps), Some(all), None], None, None, None)?;
            }
            if let Some(front) = t.front {
                put("reconcile", "pool", "mimalloc_live", [None, Some(front.wrapping_sub(t.door1)), Some(t.live), None], None, None, None)?;
            }
        }
        pool_reserve_rows(&mut put)?;
    }
    // pphatch (a build with PoolAlloc): 'allocator_decides', the times PoolAlloc read PGRUST_ALLOCATOR (events: 1 a server
    // process, its forks and sessions included) and the allocator serving (committed_bytes 1 = the page pool, 0 = mimalloc)
    if pagepool::GLOBAL_BUILD {
        let decides = [Some(i64::from(pagepool::allocator_decides())), Some(i64::from(pagepool::pool_allocates())), None, None];
        put("counter", "allocator", "allocator_decides", decides, None, None, None)?;
    }
    let mut out = Ok(());
    memledger::owner_end::for_each_recent(|r| {
        if out.is_ok() {
            let n = [Some(1), Some(r.committed), Some(r.reserved), Some(r.uses)];
            out = put("recent", r.source.name(), r.class.name(), n, Some(r.acct.name()), Some(r.pid), Some(r.seq));
        }
    });
    out?;
    Ok(srf.finish(fcinfo))
}

/// pppress card 9b: the parts of `pool_unowned` past the pool's free and stranded bytes, from its in-use bytes by tag:
/// (small_slack, owner_rounding, run_tails). small_slack: the small tier's runs (tags SMALL, and CACHE's runs) and door 1's
/// big blocks (HEAP, CACHE) less door 1's live bytes (`pool_door1`): the free slots inside runs a thread owns. owner_rounding:
/// every other tag's in-use bytes less the owners' pooled posts (`pagepool_total`): the pool's rounding of the blocks it
/// served them (an owner posts what it asked for), and in-use bytes no owner posts. run_tails: the bytes past the last
/// block of each blocks run (`Stats::waste`). Each reads 0 with the pool off; in-use counts committed bytes, so a block's
/// untouched pages land here while `pool_unowned` reads what is resident.
fn pool_unowned_terms(st: &pagepool::Stats, door1: i64, pooled: i64) -> (i64, i64, i64) {
    use pagepool::Tag;
    let tag = |t: Tag| i64::try_from(st.in_use[t.0 as usize]).unwrap_or(i64::MAX);
    let door1_tags = tag(Tag::SMALL) + tag(Tag::HEAP) + tag(Tag::CACHE);
    let all = st.in_use.iter().fold(0i64, |s, &v| s.saturating_add(i64::try_from(v).unwrap_or(i64::MAX)));
    let waste = i64::try_from(st.waste).unwrap_or(i64::MAX);
    (door1_tags.wrapping_sub(door1), all.wrapping_sub(door1_tags).wrapping_sub(pooled), waste)
}

/// D4790: the pool's bytes no owner holds, from what is resident, and its claimed bytes nothing touched: (unowned,
/// untouched). A lazy span's `commit_to` (the global allocator's big blocks, `take_slow`) declares its length before a
/// page is touched, so claimed counts pages that are no one's memory, resident or not; where the pool's resident bytes
/// read below claimed they bound what it holds. Before the pool's first sweep (`resident` None), claimed as before.
/// Unowned reads below 0 where owners count more than is held (never clamped: an accounting finding).
fn pool_ownerless(claimed: i64, resident: Option<i64>, owned: i64) -> (i64, i64) {
    let held = resident.map_or(claimed, |r| r.min(claimed));
    (held.wrapping_sub(owned), claimed.wrapping_sub(held))
}

/// The pool's warm reserve (`PGRUST_POOL_RESERVE`; counted with the switch off too): kind 'counter', source 'pool'
/// classes 'warm_zero' / 'warm_dirty' (pool_free's whole free superpages by state, committed_bytes) and D4760's
/// 'thp_partial' / 'thp_kept' / 'thp_split' (`pagepool::thp_partial_counts`) and D4787's 'span_trim_switch' /
/// 'span_zero' / 'span_trim' (`pagepool::span_reuse`), then source 'pool_reserve': sizes in committed_bytes (budget
/// NULL while the ladder does not size it), counts in events; misses by class and ask size, warm takes by state,
/// POPULATE (events = superpages, committed = ns, reserved = the longest ns) and its log2 µs bins, the in-place recycles
/// (committed = ns).
fn pool_reserve_rows<F>(put: &mut F) -> PgResult<()>
where
    F: FnMut(&str, &str, &str, [Option<i64>; 4], Option<&str>, Option<u32>, Option<u64>) -> PgResult<()>,
{
    const CLASSES: [&str; 6] = ["miss_task", "miss_session", "miss_cache", "miss_process", "miss_general", "miss_frame"];
    const SIZES: [&str; pagepool::RESERVE_SIZES] = ["miss_runs", "miss_1_super", "miss_2_15_super", "miss_16_super"];
    const BINS: [&str; pagepool::POP_BINS] = [
        "populate_us_lt1", "populate_us_lt2", "populate_us_lt4", "populate_us_lt8", "populate_us_lt16", "populate_us_lt32",
        "populate_us_lt64", "populate_us_lt128", "populate_us_lt256", "populate_us_lt512", "populate_us_lt1024",
        "populate_us_lt2048", "populate_us_lt4096", "populate_us_lt8192", "populate_us_lt16384", "populate_us_ge16384",
    ];
    let r = pagepool::reserve_stats();
    let b = |x: usize| Some(i64::try_from(x).unwrap_or(i64::MAX));
    let n = |x: u64| Some(i64::try_from(x).unwrap_or(i64::MAX));
    put("counter", "pool", "warm_zero", [None, b(r.warm_zero), None, None], None, None, None)?;
    put("counter", "pool", "warm_dirty", [None, b(r.warm_dirty), None, None], None, None, None)?;
    // D4760 (`pool_thp_partial`): the mode (events), partial give-backs of huge superpages kept (events, bytes as asked)
    // and the folio splits asked first (events, ns)
    let (mode, kept, kept_bytes, splits, split_ns) = pagepool::thp_partial_counts();
    put("counter", "pool", "thp_partial", [n(mode as u64), None, None, None], None, None, None)?;
    put("counter", "pool", "thp_kept", [n(kept), n(kept_bytes), None, None], None, None, None)?;
    put("counter", "pool", "thp_split", [n(splits), n(split_ns), None, None], None, None, None)?;
    // pppress card 8 (`pool_thp_partial` 3): 'thp_hot' (events = superpages / lazy pieces an age pass left held, hot)
    put("counter", "pool", "thp_hot", [n(pagepool::thp_hot_skips()), None, None, None], None, None, None)?;
    // D4787: reused warm lazy spans, either switch: 'span_trim_switch' (events = PGRUST_POOL_SPAN_TRIM on), 'span_zero'
    // (events = zeroings, committed = bytes, reserved = ns, the fourth = the longest ns), 'span_trim' (events = trims,
    // committed = bytes off the commit as counted, reserved = ns, the fourth = resident bytes given back)
    let u = pagepool::span_reuse();
    put("counter", "pool", "span_trim_switch", [n(u.trim_on as u64), None, None, None], None, None, None)?;
    put("counter", "pool", "span_zero", [n(u.zero_takes), n(u.zero_bytes), n(u.zero_ns), n(u.zero_max_ns)], None, None, None)?;
    put("counter", "pool", "span_trim", [n(u.trims), n(u.trimmed), n(u.trim_ns), n(u.trim_resident)], None, None, None)?;
    // pppress card 7: 'span_move_switch' (events = PGRUST_POOL_SPAN_MOVE on); 'span_move' (lazy commits that moved free
    // superpages in: events, committed = bytes moved, reserved = ns summed, the fourth = the longest ns);
    // 'span_move_zeroed' (committed = bytes zeroed before the move); 'span_move_refused' (events = moves the map budget or
    // the remap refused); 'grow_remap_refused' (events = lazy grows whose remap the kernel refused, the caller copied)
    let m = pagepool::span_moves();
    put("counter", "pool", "span_move_switch", [n(m.on as u64), None, None, None], None, None, None)?;
    put("counter", "pool", "span_move", [n(m.moves), n(m.bytes), n(m.ns), n(m.max_ns)], None, None, None)?;
    put("counter", "pool", "span_move_zeroed", [None, n(m.zeroed), None, None], None, None, None)?;
    put("counter", "pool", "span_move_refused", [n(m.refused), None, None, None], None, None, None)?;
    put("counter", "pool", "grow_remap_refused", [n(m.grow_refused), None, None, None], None, None, None)?;
    // D4885: 'warm_va_drop' (events = warm lazy spans whose address space a new span took, the lazy zone otherwise full;
    // committed = bytes of address space)
    let (vd, vb) = pagepool::warm_va_drops();
    put("counter", "pool", "warm_va_drop", [n(vd), n(vb), None, None], None, None, None)?;
    // pppress: 'span_fit' (PGRUST_POOL_SPAN_FIT: events = spans served on base pages, committed = the unused tails kept off
    // huge pages, reserved = the switch on)
    let (fit_on, fits, fit_tails) = pagepool::span_fits();
    put("counter", "pool", "span_fit", [n(fits), n(fit_tails), n(fit_on as u64), None], None, None, None)?;
    // D4760 (pppress): R1's clock, either switch: 'fast_release' (events = PGRUST_POOL_FAST_RELEASE on); 'r1_land' (the
    // R1 asks the purger served: events, committed = ns ask -> land summed, reserved = the longest ns, the fourth = bytes
    // released) and its bins 'r1_land_lt<ms>' / 'r1_land_ge200' (events); 'r1_call' (every return_warm_now: events, ns,
    // the longest ns); 'r1_<step>' (events = calls, committed = bytes released, reserved = ns); 'r1_cut_in' (events)
    let r1 = pagepool::r1_counts();
    put("counter", "pool", "fast_release", [n(r1.fast_on as u64), None, None, None], None, None, None)?;
    // D4760 (pppress): 'fine_classes' (events = PGRUST_POOL_FINE_CLASSES on)
    put("counter", "pool", "fine_classes", [n(pagepool::fine_classes_on() as u64), None, None, None], None, None, None)?;
    put("counter", "pool", "r1_land", [n(r1.lands), n(r1.land_ns), n(r1.land_max_ns), n(r1.land_bytes)], None, None, None)?;
    for (class, &v) in pagepool::R1_BIN_ROWS.iter().zip(&r1.land_bins) {
        put("counter", "pool", class, [n(v), None, None, None], None, None, None)?;
    }
    put("counter", "pool", "r1_call", [n(r1.calls), n(r1.call_ns), n(r1.call_max_ns), None], None, None, None)?;
    for (k, class) in pagepool::R1_STEP_ROWS.iter().enumerate() {
        put("counter", "pool", class, [n(r1.step_calls[k]), n(r1.step_bytes[k]), n(r1.step_ns[k]), None], None, None, None)?;
    }
    put("counter", "pool", "r1_cut_in", [n(r1.cut_ins), None, None, None], None, None, None)?;
    // D4878: 'end_give' (statement-end hand-backs under ladder pressure: events = released now on the ending thread,
    // committed = bytes released then, reserved = asked of the purger instead)
    put("counter", "pool", "end_give", [n(r1.end_now), n(r1.end_now_bytes), n(r1.end_ask), None], None, None, None)?;
    // pppress card 6: 'grow_guard' (PGRUST_POOL_GROW_GUARD: events = inline releases on query threads, committed = bytes
    // released, reserved = ns summed, the fourth = the longest ns), its bins 'grow_guard_us_lt<us>' / '_ge10000'
    // (events), 'grow_guard_skips' (events = settles another releaser held off; the second = the switch on) and
    // 'warm_first' (events = zeroed takes served warm before Cold while pressed)
    let g = pagepool::guard_counts();
    put("counter", "pool", "grow_guard", [n(g.releases), n(g.bytes), n(g.ns), n(g.max_ns)], None, None, None)?;
    for (class, &v) in pagepool::GUARD_BIN_ROWS.iter().zip(&g.bins) {
        put("counter", "pool", class, [n(v), None, None, None], None, None, None)?;
    }
    put("counter", "pool", "grow_guard_skips", [n(g.skips), n(g.on as u64), None, None], None, None, None)?;
    put("counter", "pool", "warm_first", [n(g.warm_first), None, None, None], None, None, None)?;
    // D4880: 'os_big' (big global blocks the ready pool refused for want of address space, the system served: events,
    // committed = their bytes)
    let (ob, ob_bytes) = pagepool::os_big();
    put("counter", "pool", "os_big", [n(ob), n(ob_bytes), None, None], None, None, None)?;
    // D4872: 'host_press' (the host's memory pressure, PSI / file refaults: events = presses, committed = bytes released
    // by its asks, reserved = the asks, the fourth = 1 while pressed; the second row 'host_press_switch' = the switch on)
    let h = pagepool::host_counts();
    put("counter", "pool", "host_press", [n(h.presses), n(h.bytes), n(h.asks), n(h.pressed as u64)], None, None, None)?;
    put("counter", "pool", "host_press_switch", [n(h.on as u64), None, None, None], None, None, None)?;
    let budget = if r.budget == u64::MAX { None } else { n(r.budget) };
    let sizes: [(&str, Option<i64>); 12] = [
        ("held", b(r.warm_zero + r.warm_dirty)),
        ("target", b(r.target)),
        ("want", b(r.want)),
        ("budget", budget),
        ("bound", b(r.bound)),
        ("floor", b(r.floor)),
        ("drawdown", b(r.drawdown)),
        ("deepest", b(r.deepest)),
        ("deepest_ever", b(r.deepest_ever)),
        ("churn", n(r.churn)),
        ("spare_released", n(r.spare_released)),
        ("miss_bytes", n(r.miss_bytes)),
    ];
    put("counter", "pool_reserve", "switch", [n(r.on as u64), None, None, None], None, None, None)?;
    for (class, v) in sizes {
        put("counter", "pool_reserve", class, [None, v, None, None], None, None, None)?;
    }
    let counts = CLASSES.iter().zip(r.miss_class).chain(SIZES.iter().zip(r.miss_size)).chain(BINS.iter().zip(r.populate_us));
    for (class, v) in counts.chain([(&"takes_zero", r.zero_takes), (&"takes_dirty", r.dirty_takes), (&"refills", r.refills)]) {
        put("counter", "pool_reserve", *class, [n(v), None, None, None], None, None, None)?;
    }
    put("counter", "pool_reserve", "populate", [n(r.populate_n), n(r.populate_ns), n(r.populate_max_ns), None], None, None, None)?;
    put("counter", "pool_reserve", "shed", [n(r.shed_runs), n(r.shed), None, None], None, None, None)?;
    put("counter", "pool_reserve", "shed_clock_ns", [None, n(r.shed_clock_ns), n(r.shed_clock_max_ns), None], None, None, None)?;
    // the ladder's sized cache 3: its budget (u64::MAX = at its start), raises, bytes raised, reversals
    let (raises, raised, reversals) = mcx::ladder::sized_counts()[mcx::ladder::SIZED_RESERVE];
    put("counter", "pool_reserve", "ladder_budget", [None, n(mcx::ladder::sized_budgets()[mcx::ladder::SIZED_RESERVE]), None, None], None, None, None)?;
    put("counter", "pool_reserve", "ladder_raises", [n(raises), n(raised), n(reversals), None], None, None, None)?;
    put("counter", "pool_reserve", "recycled", [n(r.recycled), n(r.recycle_ns), None, None], None, None, None)
}

/// The owner rows' columns (scripts/memory-broker-views.sql declares the same, in order).
const OWNER_COLS: usize = 24;

/// `pgrust_memory_owners`' reads of (accounts, owners, accounts) at most, and the move between its two account reads past
/// which (or past 2 % of the account) it reads again: alloclint's runtime-check bound.
const MOMENT_READS: usize = 4;
const MOMENT_BAND: u64 = 16 << 20;

/// The 'unnamed_root' rows: the most-holding unnamed roots' names.
const UNNAMED_ROOTS_SHOWN: usize = 20;

/// LEDGER2 `pgrust_memory_owners()`: one row per owner that is keyed or holds anything (memledger::charge::for_each_owner),
/// then one 'unattributed' row per account and one for the process. Columns: kind, owner_id, cell, parent_cell (the
/// owner tree: session > statement / portal > operator > worker_arena), account, pid, label (an operator's breaker kind;
/// 'account' / 'process' on the unattributed rows), ended, held_ctx_named, held_ctx_inferred (memory-context blocks: a
/// named root's, or a root that took the creating thread's owner), held_charges (its charges' committed bytes), held_mapped
/// (of held, door 2's bytes), held (ctx + charges: measured), budget (promised: its charges' reservations, an operator's share),
/// claimed_unused (budget - held, signed: below 0 = held past its promise; a statement's and a portal's budget - subtree_held,
/// as their reservation backs their operators' draws too; NULL with budget on an owner that promises none: a cache, a
/// session with no reservation), reserved_unbacked (the reservation no draw
/// backs with pages), spilled (its spills' bytes on disk), subtree_held, subtree_spilled (itself and everything under
/// it), uses, held_not_resident and subtree_not_resident (design A: of held and of subtree_held, the bytes the residency
/// pass found not resident in the ranges their charges list; held - held_not_resident is in memory). Then up to 20
/// 'unnamed_root' rows (a root context no code names takes its creating thread's owner): the
/// unnamed roots' names (label) whose contexts hold the most (held_ctx_inferred = held), whichever owner they took. An
/// unattributed row: its account's bytes less what the account's owners hold (held; held_mapped: door 2's alone) and less
/// its keepers, signed, never clamped. A 'keepers' row (one per account that has any, then one for the process; label
/// 'account' / 'process'): the keeper blocks mcx parked for the next context on the account (its context free lists,
/// `mcx::parked_keeper_bytes`), on the account and no owner's, as held_ctx_inferred = held: free context memory kept for
/// reuse, not unattributed; of them, parked_growth_bytes and parked_growth_count (D4679): the blocks on the account's
/// threads' growth shelves (`memledger::growth`; the process row's: every account's, a slot no session holds included;
/// NULL on every other row).
pub fn fc_pgrust_memory_owners(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    use memledger::charge::{OwnerView, NO_OWNER, OWNER_CELLS};
    let flinfo = flinfo.expect("pgrust_memory_owners: resolved FmgrInfo required");
    let mcx = super::raw::result_mcx(fcinfo);
    let mut srf = funcapi::InitMaterializedSRF(mcx, flinfo, fcinfo, 0)?;
    declared(&srf, OWNER_COLS)?;
    memledger::fold();
    // The owners and the accounts as one moment (alloclint's one-sample spikes): every account read right before and right
    // after the owner scan, nothing emitted in between, and the three reads retried (MOMENT_READS at most) while an account
    // moved past max(16 MiB, 2 %) between its two reads (a region committed or freed mid-scan). The unattributed rows
    // subtract the owners from the after-read.
    let mut rows: mcx::PgVec<'_, OwnerView> = mcx::PgVec::new_in(mcx);
    let mut before: mcx::PgVec<'_, i64> = mcx::PgVec::new_in(mcx);
    mcx::pgvec_resize(&mut before, memledger::ACCOUNT_SLOTS, 0)?;
    let mut after: mcx::PgVec<'_, memledger::AccountRow> = mcx::PgVec::new_in(mcx);
    for _ in 0..MOMENT_READS {
        rows.clear();
        after.clear();
        before.iter_mut().for_each(|b| *b = 0);
        memledger::for_each_row(|r| before[(r.acct.0 as usize).min(memledger::ACCOUNT_SLOTS - 1)] = r.account_bytes);
        memledger::charge::for_each_owner(|v| rows.push(*v));
        memledger::for_each_row(|r| after.push(*r));
        let moved = |r: &memledger::AccountRow| {
            let d = r.account_bytes.wrapping_sub(before[(r.acct.0 as usize).min(memledger::ACCOUNT_SLOTS - 1)]).unsigned_abs();
            d > MOMENT_BAND.max(r.account_bytes.unsigned_abs() / 50)
        };
        if !after.iter().any(moved) {
            break;
        }
    }
    let mut parent: mcx::PgVec<'_, u32> = mcx::PgVec::new_in(mcx);
    mcx::pgvec_resize(&mut parent, OWNER_CELLS, NO_OWNER)?;
    let mut sub: mcx::PgVec<'_, (i64, i64, i64)> = mcx::PgVec::new_in(mcx);
    mcx::pgvec_resize(&mut sub, OWNER_CELLS, (0, 0, 0))?;
    let mut by_acct: mcx::PgVec<'_, (i64, i64)> = mcx::PgVec::new_in(mcx);
    mcx::pgvec_resize(&mut by_acct, memledger::ACCOUNT_SLOTS, (0, 0))?;
    for v in rows.iter() {
        parent[v.cell as usize] = v.parent;
        let a = &mut by_acct[(v.acct.0 as usize).min(memledger::ACCOUNT_SLOTS - 1)];
        *a = (a.0.wrapping_add(v.heap()), a.1.wrapping_add(v.mapped));
    }
    for v in rows.iter() {
        let mut c = v.cell as usize;
        for _ in 0..8 {
            sub[c] = (sub[c].0.wrapping_add(v.held()), sub[c].1.wrapping_add(v.spilled), sub[c].2.wrapping_add(v.nr));
            match parent[c] {
                NO_OWNER => break,
                p => c = p as usize,
            }
        }
    }
    let i4 = |x: u32| Datum::from_i32(x as i32);
    for v in rows.iter() {
        let label = if v.label != 0 { Some(memledger::Kind::ALL[(v.label & 0xff) as usize % memledger::KINDS].name()) } else { None };
        let pid = memledger::backend_pid(v.acct);
        let values: [Datum; OWNER_COLS] = [
            text(mcx, v.name),
            Datum::from_i64(v.key as i64),
            i4(v.cell),
            i4(v.parent),
            text(mcx, v.acct.name()),
            Datum::from_i32(pid as i32),
            label.map_or(Datum::from_i64(0), |l| text(mcx, l)),
            Datum::from_bool(v.ended),
            Datum::from_i64(v.ctx_named),
            Datum::from_i64(v.ctx_inferred),
            Datum::from_i64(v.committed),
            Datum::from_i64(v.mapped),
            Datum::from_i64(v.held()),
            Datum::from_i64(v.promised()),
            Datum::from_i64(v.promised().wrapping_sub(if matches!(v.name, "statement" | "portal") { sub[v.cell as usize].0 } else { v.held() })),
            Datum::from_i64(v.reserved.wrapping_sub(v.backed)),
            Datum::from_i64(v.spilled),
            Datum::from_i64(sub[v.cell as usize].0),
            Datum::from_i64(sub[v.cell as usize].1),
            Datum::from_i64(v.uses),
            Datum::from_i64(v.nr),
            Datum::from_i64(sub[v.cell as usize].2),
            Datum::from_i64(0),
            Datum::from_i64(0),
        ];
        let mut nulls = [false; OWNER_COLS];
        nulls[3] = v.parent == NO_OWNER;
        nulls[22] = true;
        nulls[23] = true;
        nulls[5] = pid == 0;
        nulls[6] = label.is_none();
        // a promise is a statement's, a portal's or an operator's: a cache's or a session's with none shows none
        let promises = matches!(v.name, "statement" | "portal" | "operator" | "worker_arena") || v.promised() != 0;
        nulls[13] = !promises;
        nulls[14] = !promises;
        nulls[15] = !promises;
        srf.putvalues(&values, &nulls)?;
    }
    // the unnamed roots holding the most, by name, so the big ones get named one by one
    let mut roots: mcx::PgVec<'_, (&'static str, i64)> = mcx::PgVec::new_in(mcx);
    memledger::charge::unnamed_roots(|n, b| roots.push((n, b)));
    roots.sort_unstable_by(|x, y| y.1.cmp(&x.1));
    for &(name, bytes) in roots.iter().take(UNNAMED_ROOTS_SHOWN) {
        let zero = Datum::from_i64(0);
        let mut values = [zero; OWNER_COLS];
        values[0] = text(mcx, "unnamed_root");
        values[4] = text(mcx, "-");
        values[6] = text(mcx, if name.is_empty() { "(past the name table)" } else { name });
        values[7] = Datum::from_bool(false);
        values[9] = Datum::from_i64(bytes);
        values[12] = Datum::from_i64(bytes);
        let mut nulls = [false; OWNER_COLS];
        for i in [1, 2, 3, 5, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23] {
            nulls[i] = true;
        }
        srf.putvalues(&values, &nulls)?;
    }
    let mut keepers = 0i64;
    for r in after.iter() {
        let k = memledger::keepers(r.acct);
        if k != 0 {
            keepers = keepers.wrapping_add(k);
            keepers_row(&mut srf, mcx, r.acct.name(), r.pid, "account", k, memledger::growth(r.acct))?;
        }
    }
    // D4679: the process row's growth is every account's, a slot no session holds now included (a shelf that outlived its
    // session shows as the process's growth past its account rows')
    keepers_row(&mut srf, mcx, "process", 0, "process", keepers, memledger::growth_total())?;
    let mut process = (0i64, 0i64);
    for r in after.iter() {
        let (h, m) = by_acct[(r.acct.0 as usize).min(memledger::ACCOUNT_SLOTS - 1)];
        // D4556: the rounding account's bytes are the allocator's rounding of every live block, attributed at door 1 by
        // construction (no owner cell holds a block's rounding)
        let rounding = if r.acct == memledger::Acct::ROUNDING { r.account_bytes.wrapping_sub(r.mapped_bytes) } else { 0 };
        let heap = r.account_bytes.wrapping_sub(r.mapped_bytes).wrapping_sub(h).wrapping_sub(memledger::keepers(r.acct)).wrapping_sub(rounding);
        let mapped = r.mapped_bytes.wrapping_sub(m);
        process = (process.0.wrapping_add(heap), process.1.wrapping_add(mapped));
        unattributed_row(&mut srf, mcx, r.acct.name(), r.pid, "account", heap, mapped)?;
    }
    unattributed_row(&mut srf, mcx, "process", 0, "process", process.0, process.1)?;
    Ok(srf.finish(fcinfo))
}

/// A 'keepers' row: `bytes` of keeper blocks mcx parked on `account` (door 1's, no owner's), context bytes (held_ctx_inferred);
/// of them, `growth` (bytes, blocks) on mcx's growth shelf (D4679: parked_growth_bytes, parked_growth_count).
fn keepers_row(srf: &mut funcapi::MaterializedSRF<'_>, mcx: mcx::Mcx<'_>, account: &str, pid: u32, label: &str, bytes: i64, growth: (i64, i64)) -> PgResult<()> {
    let zero = Datum::from_i64(0);
    let mut values = [zero; OWNER_COLS];
    values[0] = text(mcx, "keepers");
    values[4] = text(mcx, account);
    values[5] = Datum::from_i32(pid as i32);
    values[6] = text(mcx, label);
    values[7] = Datum::from_bool(false);
    values[9] = Datum::from_i64(bytes);
    values[12] = Datum::from_i64(bytes);
    values[17] = values[12];
    values[22] = Datum::from_i64(growth.0);
    values[23] = Datum::from_i64(growth.1);
    let mut nulls = [false; OWNER_COLS];
    for i in [1, 2, 3, 13, 14, 15] {
        nulls[i] = true;
    }
    nulls[5] = pid == 0;
    srf.putvalues(&values, &nulls)
}

fn unattributed_row(srf: &mut funcapi::MaterializedSRF<'_>, mcx: mcx::Mcx<'_>, account: &str, pid: u32, label: &str, heap: i64, mapped: i64) -> PgResult<()> {
    let zero = Datum::from_i64(0);
    let mut values = [zero; OWNER_COLS];
    values[0] = text(mcx, "unattributed");
    values[4] = text(mcx, account);
    values[5] = Datum::from_i32(pid as i32);
    values[6] = text(mcx, label);
    values[7] = Datum::from_bool(false);
    values[11] = Datum::from_i64(mapped);
    values[12] = Datum::from_i64(heap.wrapping_add(mapped));
    values[17] = values[12];
    let mut nulls = [false; OWNER_COLS];
    for i in [1, 2, 3, 10, 13, 14, 15, 20, 21, 22, 23] {
        nulls[i] = true;
    }
    nulls[5] = pid == 0;
    srf.putvalues(&values, &nulls)
}

/// The reconcile rows' columns (scripts/memory-broker-views.sql declares the same, in order).
const RECONCILE_COLS: usize = 4;

/// LEDGER2 `pgrust_memory_reconcile()`: the process total term by term against the cgroup's reading (term, bytes, source,
/// note), read after the residency pass, as the accounts view reads. 'reading' is the limiting cgroup's memory.current
/// ('reading_u': D4393's read, NULL on this branch). The terms: 'owners_held' (every owner's held bytes, as charged),
/// 'owners_not_resident' (less the pages the residency pass found not resident: they are region pages, every region an
/// owner's charge), 'keepers' (the keeper blocks mcx parked for the next context: on the accounts and no owner's),
/// 'unattributed_heap' and 'unattributed_mapped' (every account's bytes less its owners' and its keepers),
/// 'allocator_retained', 'c_libraries' (glibc's in-use bytes), 'page_cache' (memory.stat file - shmem), 'kernel', 'sock',
/// 'thread_stacks' (not measured: NULL; the 'threads' row counts them). 'gap' = reading - the terms, signed; the band is the
/// user's to set, so no verdict here. Then information rows (never summed): the cgroup's anon and shmem, the accounts'
/// resident total, the threads, and the cache road's reading (`memledger::arena::road`): 'cache_road_live',
/// 'cache_road_holders', 'cache_road_unheld' (live less holders), 'cache_road_plain', 'cache_road_retention' (NULL before
/// the arenas are installed).
pub fn fc_pgrust_memory_reconcile(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    use mcx::governor::reconcile as r;
    let flinfo = flinfo.expect("pgrust_memory_reconcile: resolved FmgrInfo required");
    let mcx = super::raw::result_mcx(fcinfo);
    let mut srf = funcapi::InitMaterializedSRF(mcx, flinfo, fcinfo, 0)?;
    declared(&srf, RECONCILE_COLS)?;
    let _ = gravity_driver::mem::resident::pass_all();
    let mut sums = (0i64, 0i64);
    let mut accounts = (0i64, 0i64);
    memledger::for_each_row(|a| {
        add_row(&mut sums, a);
        accounts = (accounts.0.wrapping_add(a.account_bytes.wrapping_sub(a.mapped_bytes)), accounts.1.wrapping_add(a.mapped_bytes));
    });
    let mut owners = (0i64, 0i64);
    memledger::charge::for_each_owner(|v| owners = (owners.0.wrapping_add(v.heap()), owners.1.wrapping_add(v.mapped)));
    let t = resident_terms(sums);
    let p = mcx::governor::probe_reconcile();
    let s = |x: Option<u64>| x.map(|b| i64::try_from(b).unwrap_or(i64::MAX));
    let (nr_heap, nr_mapped) = memledger::not_resident_bytes();
    let page_cache = match (p[r::FILE], p[r::SHMEM]) {
        (Some(f), Some(sh)) => Some((f as i64).wrapping_sub(sh as i64)),
        _ => None,
    };
    let keepers = memledger::keepers_total();
    let terms: [(&str, Option<i64>, &str); 11] = [
        ("owners_held", Some(owners.0.wrapping_add(owners.1)), "measured: owner cells"),
        ("owners_not_resident", Some(nr_heap.wrapping_add(nr_mapped).wrapping_neg()), "inferred: the residency pass's not-resident bytes (regions, mapped caches' untouched pages), all on owners' charges"),
        ("keepers", Some(keepers), "measured: mcx's parked keeper blocks (its context free lists), on the accounts and no owner's"),
        ("unattributed_heap", Some(accounts.0.wrapping_sub(owners.0).wrapping_sub(keepers)), "measured: door-1 account bytes less the owners' and the keepers"),
        ("unattributed_mapped", Some(accounts.1.wrapping_sub(owners.1)), "measured: door-2 account bytes less the owners'"),
        ("allocator_retained", t.as_ref().and_then(|t| t.retained), "measured: mimalloc's resident pages less its live blocks"),
        ("c_libraries", s(p[r::C_LIBRARIES]), "measured: glibc mallinfo2 in use"),
        ("page_cache", page_cache, "measured: memory.stat file - shmem"),
        ("kernel", s(p[r::KERNEL]), "measured: memory.stat kernel"),
        ("sock", s(p[r::SOCK]), "measured: memory.stat sock"),
        ("thread_stacks", None, "not measured"),
    ];
    let reading = s(p[r::CURRENT]);
    let mut put = |term: &str, bytes: Option<i64>, source: &str, note: &str| {
        let values = [text(mcx, term), Datum::from_i64(bytes.unwrap_or(0)), text(mcx, source), text(mcx, note)];
        srf.putvalues(&values, &[false, bytes.is_none(), false, false])
    };
    put("reading", reading, "reading", "memory.current of the limiting cgroup")?;
    put("reading_u", None, "reading", "D4393's read: not on this branch")?;
    let mut sum = 0i64;
    for (term, bytes, note) in terms {
        sum = sum.wrapping_add(bytes.unwrap_or(0));
        put(term, bytes, "term", note)?;
    }
    put("gap", reading.map(|x| x.wrapping_sub(sum)), "gap", "reading - terms, signed; band unset (the user sets it)")?;
    put("cgroup_anon", s(p[r::ANON]), "info", "memory.stat anon")?;
    put("cgroup_shmem", s(p[r::SHMEM]), "info", "memory.stat shmem")?;
    put("accounts_resident", Some(sums.0), "info", "every account's resident bytes")?;
    put("threads", s(p[r::THREADS]), "info", "the process's threads (a count, not bytes): thread_stacks is not measured")?;
    // the cache road (memledger::arena::road): its live blocks against what the cache holders hold on the heap
    let road = memledger::arena::road();
    put("cache_road_live", Some(road.arena_live.wrapping_add(road.own_live)), "info", "measured: the live blocks of the cache arena and the own range")?;
    put("cache_road_holders", Some(road.holders_heap), "info", "measured: the cache holders' door-1 bytes")?;
    put("cache_road_unheld", Some(road.unheld()), "info", "measured: cache_road_live - cache_road_holders; read between statements (a running statement's cache-road images are its own)")?;
    put("cache_road_plain", Some(road.plain), "info", "measured: bytes kept by cache scopes that name no owner, since start")?;
    put("cache_road_retention", road.retention(), "info", "measured: the cache arena's committed bytes - its live blocks")?;
    // D4809: the pgrc pgstat counting's two late paths, both 0 once fixed (counts, not bytes)
    put("pgstat_states_after_shutdown", Some(i64::try_from(pgstat::pgstat_states_after_shutdown()).unwrap_or(i64::MAX)), "info", "count since start: pgstat states built after their session's shutdown hook (D4809 F4)")?;
    put("scanstats_late_lands", Some(i64::try_from(pgrc_am::scanstats::late_lands()).unwrap_or(i64::MAX)), "info", "count since start: Gravity scan entries landed at an idle flush, not at their transaction's end (D4809 F1)")?;
    Ok(srf.finish(fcinfo))
}

// ---------------------------------------------------------------------------------------------------------------------
// Lane ladderview: the memory ladder in SQL (mcx::ladder_stats, observe only).
// ---------------------------------------------------------------------------------------------------------------------

/// The ladder rows' columns (scripts/memory-broker-views.sql declares the same, in order).
const LADDER_COLS: usize = 14;

/// `pgrust_memory_ladder_stats()`, read now, since the server started, in every `pgrust.memory_ladder` mode: kind, name,
/// misses, waits, wait_ns, wait_max_ns, refusals, last_tries, climbs, runs, relief_bytes, ns, cancels, line_bytes. First one
/// 'ladder' row: name = the mode the memory runtime's last sample published (off, shadow, on), line_bytes = the ladder's line
/// it published (NULL before any). Kind 'gate' (one row
/// per site of `mcx::ladder_stats::Site`: draw, admit, scratch, small_draw, load, sample, hook, block): misses (waits + refusals
/// with no wait), waits, wait_ns / wait_max_ns (a wait's whole length), refusals (refused or interrupted asks; the
/// sample gate's serial road; the hook's victim canceled), last_tries (draws whose wait ended in `DrawWait::LastTry`),
/// climbs (the climbs that ran a rung, `mcx::ladder::gate_climbs`: draw, admit and scratch; block: D4812 (2)'s block gate,
/// `mcx::governor::block_gate_counts`, its refusals its own statement's 53200); the hook's row counts
/// its episodes past the ladder's line (shadow or on). Kind 'rung' (R1 R2p R2a R3 R4a R4b R5 R2b R6; R2p's row shows with
/// its knob off too, at 0): runs, relief_bytes
/// (measured, `mcx::ladder::counts`), ns (the runs' time; R5's is its waits', NULL here). Kind 'cancel' (watchdog,
/// ladder_r6, terminate: D4029; r6_ack_timeout: an R6 cancel unanswered past the cancel line for `R6_ACK_BOUND_MS`;
/// projection_tick / projection_inline: the projection rule's `decide_tick` / `serve_due`):
/// cancels. Kind 'refusal' (D4914, one row per branch of `mcx::ladder_stats::Why` that refused a draw past W and G:
/// past_cap, draw_capped, spills_itself, last_try, wait_spent, canceled, no_wait, hold, max_region): runs the refusals,
/// relief_bytes the bytes they asked. Kind 'pick' (D4812, the R6 cancel pick and its residency reads, never a cancel path): stale_pick: cancels
/// the picks' cancels issued while a statement that could outrank the pick waited for the residency thread's read, runs
/// every pick's cancel; full_pass_age_at_pick: ns / wait_max_ns the age of the residency thread's last full pass at the
/// last / the oldest pick; residency_hint: runs the statements handed to the residency thread (by a pick or a sample),
/// climbs the ones it read again, refusals the hints dropped with every slot taken; sample_hint: runs the ladder
/// episodes' samples that handed them (read past LOW), refusals the samples a busy lock skipped, climbs the statements
/// they handed, cancels the episodes; sample_hint_last_episode: runs / climbs the last episode's. Kind 'block_gate'
/// (D4812 (2), `ladder_block_gate`): small (runs: the slow-path entries of blocks under 1 MiB past the gate's limit),
/// skipped (runs: gated blocks taken at once in a critical section or with interrupts held off, every LWLock holder),
/// busy (runs: the pins table or registry held at the gate's R4b or pick test), sample_wake (runs: the waits a new sample
/// ended, ns / wait_max_ns the lag from that sample's publish to the waiter's next test, summed / the longest). Kind 'cachefloor' (ladder C1, `cache_floor`): low / high / top (the marks now; C1c: TOP = HIGH + 5 %), room (the headroom under the line
/// now; NULL with C1 off), burst (the p99 rise over LOW's window, LOW before its clamp; NULL before one is measured; ns =
/// the window), batch (one background batch: the gap, at most 2 GiB), stuck (the cache bytes past their floors their
/// trims cannot drop, which the grant pool counts as held) and per cache of the reclaim order `<cache>.floor` / `.held` /
/// `.pinned` (the bytes its trim cannot drop now: in use or untrimmable), all in line_bytes; background (the ladder
/// thread's reclaims from under LOW to HIGH, a batch a sample: runs, relief_bytes; misses the quiet samples, under 4 MiB
/// reclaimable), inline (the draws' evictions: runs, relief_bytes, ns, wait_max_ns the longest, misses
/// the ones still short), refill_closed (refusals), giveback (the reclaims of 64 MiB or more timed from their start to
/// their bytes given back: runs, relief_bytes, ns), giveback_ns_per_gib (ns: the rate LOW's window is sized with) and,
/// ladder C1b, `<cache>.budget` for the part cache, fp tables and condition cache (the ladder's budget in line_bytes,
/// NULL at the cache's start, which C1d raises past), C1c `<cache>.raises` (the ladder's raises of its budget past TOP:
/// runs, relief_bytes the bytes raised), `<cache>.reversals` (runs: the takes within 10 s of a raise), `<cache>.takes`
/// (the ladder's takes from it: runs, relief_bytes the bytes taken), why no raise (criterion 6): `<cache>.held_room` /
/// `.held_timing` / `.held_demand` (runs: the samples past the drain whose raise its room under one step, its timing or
/// no demand held), `<cache>.quiet` (climbs the refill backoff's shift, ns the refill quiet it gives) and
/// `<cache>.since_take` (ns since the ladder's last take from it; NULL none), raise_gate.samples / landing / unquiet /
/// refault / bound / draining (runs: the samples past the drain with raises on, those a landing take, the reclaim quiet,
/// the refault gate or the windows (under one step) held, and the samples still draining), part_cache.reach (line_bytes: the floor's hold
/// bound past TOP at the last sample, the budget plus the room a raise could give; 0 = none) and part_cache.trims (the
/// part cache's trims since start: runs, relief_bytes the bytes evicted, climbs the parts; D4802: the faces tier's bytes
/// count at every trim that reaches it, faces a running statement holds again each time; with trim_real_frees on, the
/// bytes that left memory, as part_cache.evicted_real counts them), part_cache.evicted_real (D4802:
/// the trims that took bytes out of the census: runs, relief_bytes those bytes, a part's faces once as they leave it),
/// part_cache.faces (the trims that let faces go: runs, relief_bytes the bytes they let go, which part_cache.trims counts
/// with trim_real_frees off),
/// part_cache.loads (relief_bytes: the bytes into the census since start), part_cache.short_releases / older_releases (runs:
/// the short hook's releases of every parked set, and of all but the newest), slot_depot.moves (asks the depot's takes,
/// waits its puts, runs its evictions), fp_tables.cap_evicted (the
/// tables its inserts evicted at its cap: runs, relief_bytes) and fp_tables.over_cap_pinned (L7, cache_fp_cap_pins: the
/// inserts kept past the cap with every table left held: runs, relief_bytes their bytes past it). Kind
/// 'fair' (ladder 1.3-1.4, `fair_share`): level (runs the samples that set the shares, climbs the queries over their
/// share at the last, line_bytes the max-min level, NULL when all fit) and lifts (runs the cuts lifted by a sample,
/// relief_bytes the participants' uncuts). Kind 'reconcile' (`ladder_reconcile`, `mempolicy::reconcile`; counts move
/// only with the knob on): samples (runs: the ladder samples checked), split.<field> (line_bytes: the limiting cgroup's
/// memory.current, anon, shmem, file, clean inactive_file, clean page cache, the line and the limit at the last sample)
/// and split.tolerance (line_bytes: max(64 MiB, 2 % of the limit) plus that sample's growth), term.<name> (<kind>) ->
/// <readers> (line_bytes the term's last value, NULL before it was stored; relief_bytes its most since start), and a row
/// per rule a term or check can break: r1 (a kind its readers may not read), r2 (larger than memory.current), r3 (the
/// readers' terms against the split named in the row), r4 (the allocator's free bytes counted as used): runs the samples
/// that broke it, relief_bytes the most bytes past the tolerance, line_bytes the value compared at the last sample,
/// climbs the value at the worst break. With the knob on, D4910's pin rows: pins.parts_live / parts_claimed / parts_deferred
/// (runs: the parts with a live use, those a live claim holds, those only deferring statements hold), pins.users_max (runs:
/// the most live users one part has), pins.images / passed / dicts / pinned (line_bytes: the claimed parts' segment images,
/// passed extents, dictionary images, pinned bytes), pins.fully_passed (runs: the claimed parts with every image passed,
/// line_bytes their pinned bytes), pins.top_statement (line_bytes: the most in-use bytes one running statement's
/// account holds), pins.untrimmable.live / deferred / parked / none (line_bytes: the cache's bytes its trim cannot drop
/// now, by what holds the part: a live claim, deferring statements only, parked executors only, no use), pins.parked_gate
/// (runs: 1 when a trim may evict the parked-only parts now) and pins.deferred_pinned (line_bytes: the pinned bytes of the
/// parts only deferring statements hold). Kind 'takefail' (D4923 diag): draw.<why> / admit.<why> (runs: the grant pool's
/// takes that failed by why: bar the governor's bar, stuck past the limit only by the caches' stuck bytes, limit past it
/// regardless, broker B's room or the bar under the one budget applied; relief_bytes the bytes asked) and stuck_skipped
/// (runs: the stuck samples whose trimmable read skipped a lock, relief_bytes the stuck bytes they gave).
pub fn fc_pgrust_memory_ladder_stats(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    use mcx::ladder_stats::{Cancel, Site};
    let flinfo = flinfo.expect("pgrust_memory_ladder_stats: resolved FmgrInfo required");
    let mcx = super::raw::result_mcx(fcinfo);
    let mut srf = funcapi::InitMaterializedSRF(mcx, flinfo, fcinfo, 0)?;
    declared(&srf, LADDER_COLS)?;
    let climbs = mcx::ladder::gate_climbs();
    let (counts, _) = mcx::ladder::counts();
    let ns = mcx::ladder_stats::rung_ns();
    // a row's numbers by column index (2..LADDER_COLS); None = NULL
    let mut put = |kind: &str, name: &str, cols: &[(usize, u64)]| {
        let mut values = [Datum::from_i64(0); LADDER_COLS];
        let mut nulls = [true; LADDER_COLS];
        values[0] = text(mcx, kind);
        values[1] = text(mcx, name);
        nulls[0] = false;
        nulls[1] = false;
        for &(i, x) in cols {
            values[i] = i8d(x);
            nulls[i] = false;
        }
        srf.putvalues(&values, &nulls)
    };
    let mode = match mcx::ladder::mode() {
        mcx::ladder::ON => "on",
        mcx::ladder::SHADOW => "shadow",
        _ => "off",
    };
    let line = mcx::ladder::line();
    let published = [(13, line)];
    put("ladder", mode, &published[..usize::from(line != 0)])?;
    for s in Site::ALL {
        let x = mcx::ladder_stats::site(s);
        let mut cols = [(2, x.misses), (3, x.waits), (4, x.wait_ns), (5, x.wait_max_ns), (6, x.refusals), (7, x.last_tries), (8, 0)];
        let n = match s {
            Site::Draw | Site::Admit | Site::Scratch => {
                let g = [mcx::ladder::Gate::Draw, mcx::ladder::Gate::Admit, mcx::ladder::Gate::Scratch][s as usize];
                cols[6].1 = climbs[g as usize];
                7
            }
            Site::Block => {
                cols[6].1 = mcx::governor::block_gate_counts().climbs;
                7
            }
            Site::Hook => 5,
            _ => 6,
        };
        put("gate", s.name(), &cols[..n])?;
    }
    for r in mempolicy::ladder::ASKER {
        let (runs, relief) = counts[r as usize];
        let rung = [(9, runs), (10, relief), (11, ns[r as usize])];
        put("rung", r.name(), if r == mempolicy::ladder::Rung::Wait { &rung[..2] } else { &rung[..] })?;
    }
    let c = mcx::ladder_stats::cancels();
    for k in Cancel::ALL {
        put("cancel", k.name(), &[(12, c[k as usize])])?;
    }
    let d = mcx::governor::decide_counts();
    put("cancel", "projection_tick", &[(12, d.backstop_cancels)])?;
    put("cancel", "projection_inline", &[(12, d.inline_cancels)])?;
    for (w, (n, bytes)) in mcx::ladder_stats::Why::ALL.iter().zip(mcx::ladder_stats::whys()) {
        put("refusal", w.name(), &[(9, n), (10, bytes)])?;
    }
    // D4923 diag: the pool's failed takes by why, draws and admissions apart (runs, relief_bytes the bytes asked), and
    // C-03: the stuck samples whose trimmable read skipped a lock (runs, relief_bytes the stuck bytes they gave)
    let fails = gravity_driver::mem::grant::take_fails();
    for (a, side) in [(0usize, "draw"), (1, "admit")] {
        for (w, name) in mempolicy::admit::TAKE_FAIL_NAMES.iter().enumerate() {
            put("takefail", &format!("{side}.{name}"), &[(9, fails[a][w].0), (10, fails[a][w].1)])?; // alloc: Cold
        }
    }
    let (skips, skip_bytes) = gravity_driver::mem::reclaim::stuck_skipped();
    put("takefail", "stuck_skipped", &[(9, skips), (10, skip_bytes)])?;
    let p = mcx::governor::pick_counts();
    let ms = |x: u64| x.saturating_mul(1_000_000);
    put("pick", "stale_pick", &[(12, p.stale_picks), (9, p.picks)])?;
    put("pick", "full_pass_age_at_pick", &[(11, ms(p.full_pass_age_last_ms)), (5, ms(p.full_pass_age_oldest_ms))])?;
    let (hinted, dropped, read) = memledger::resident::hint_counts();
    put("pick", "residency_hint", &[(9, hinted), (8, read), (6, dropped)])?;
    let h = mcx::governor::sample_hint_counts();
    put("pick", "sample_hint", &[(9, h.samples), (6, h.busy), (8, h.hints), (12, h.episodes)])?;
    put("pick", "sample_hint_last_episode", &[(9, h.last_episode_samples), (8, h.last_episode_hints)])?;
    // D4812 (2), the block gate's counters beside its 'gate' row
    let b = mcx::governor::block_gate_counts();
    put("block_gate", "small", &[(9, b.small)])?;
    put("block_gate", "skipped", &[(9, b.skipped)])?;
    put("block_gate", "busy", &[(9, b.busy)])?;
    put("block_gate", "sample_wake", &[(9, b.sample_ends), (11, b.sample_lag_ns), (5, b.sample_lag_ns_top)])?;
    // ladder C1 (`cache_floor`): the marks and the room under the line (NULL with C1 off or before its first sample),
    // each cache's protected floor / bytes held / bytes in use, the ladder thread's reclaims, the draws' inline
    // evictions and the refill guard's refusals
    let room = mcx::ladder::room_now();
    let limit = room.map_or(mcx::governor::limit() as u64, |r| r.1);
    let (low, high, top) = mcx::ladder::marks3_now(limit);
    put("cachefloor", "low", &[(13, low)])?;
    put("cachefloor", "high", &[(13, high)])?;
    put("cachefloor", "top", &[(13, top)])?;
    let room_col = [(13, room.map_or(0, |r| r.0))];
    put("cachefloor", "room", &room_col[..usize::from(room.is_some())])?;
    let rate = mcx::ladder::give_back_ns_per_gib();
    let window_ns = mempolicy::ladder::low_window_ms(limit, rate).saturating_mul(1_000_000);
    let burst = mcx::ladder::burst();
    let burst_col = [(11, window_ns), (13, burst.unwrap_or(0))];
    put("cachefloor", "burst", &burst_col[..1 + usize::from(burst.is_some())])?;
    put("cachefloor", "batch", &[(13, mempolicy::ladder::batch(limit))])?;
    put("cachefloor", "stuck", &[(13, mcx::ladder::caches_stuck())])?;
    // ladder C1d v2: the lowest headroom of the raises' window (NULL before one was noted)
    let low = mcx::ladder::room_low();
    put("cachefloor", "room_low", &[(13, low.unwrap_or(0))][..usize::from(low.is_some())])?;
    // ladder C1: the least room the claims (the grant pool, B) left in the same window, a raise's bound
    let claimed = mcx::ladder::claim_low();
    put("cachefloor", "claim_low", &[(13, claimed.unwrap_or(0))][..usize::from(claimed.is_some())])?;
    for (c, floor, held, pinned) in gravity_driver::mem::reclaim::cache_floors_view() {
        put("cachefloor", &format!("{}.floor", c.name()), &[(13, floor)])?; // alloc: Cold
        put("cachefloor", &format!("{}.held", c.name()), &[(13, held)])?; // alloc: Cold
        put("cachefloor", &format!("{}.pinned", c.name()), &[(13, pinned)])?; // alloc: Cold
    }
    // ladder C1b / C1c: the sized caches' ladder budgets (NULL at their bounds), their raises past TOP and the takes that
    // reversed one, and the part cache's trims since start
    let sized = mcx::ladder::sized_budgets();
    let raised = mcx::ladder::sized_counts();
    let taken = mcx::ladder::sized_takes();
    let held = mcx::ladder::raise_held();
    for (c, i) in [(Consumer::PartCache, mcx::ladder::SIZED_PART), (Consumer::FpTables, mcx::ladder::SIZED_FP), (Consumer::CondCache, mcx::ladder::SIZED_COND)] {
        let col = [(13, sized[i])];
        put("cachefloor", &format!("{}.budget", c.name()), &col[..usize::from(sized[i] != u64::MAX)])?; // alloc: Cold
        put("cachefloor", &format!("{}.raises", c.name()), &[(9, raised[i].0), (10, raised[i].1)])?; // alloc: Cold
        put("cachefloor", &format!("{}.reversals", c.name()), &[(9, raised[i].2)])?; // alloc: Cold
        put("cachefloor", &format!("{}.takes", c.name()), &[(9, taken[i].0), (10, taken[i].1)])?; // alloc: Cold
        // why no raise: the samples its room, its timing or no demand held a raise in; its refill backoff now
        put("cachefloor", &format!("{}.held_room", c.name()), &[(9, held[i][mcx::ladder::HELD_ROOM])])?; // alloc: Cold
        put("cachefloor", &format!("{}.held_timing", c.name()), &[(9, held[i][mcx::ladder::HELD_TIMING])])?; // alloc: Cold
        put("cachefloor", &format!("{}.held_demand", c.name()), &[(9, held[i][mcx::ladder::HELD_DEMAND])])?; // alloc: Cold
        let (shift, quiet, since) = mcx::ladder::sized_quiet(i);
        put("cachefloor", &format!("{}.quiet", c.name()), &[(8, u64::from(shift)), (4, quiet.saturating_mul(1_000_000))])?; // alloc: Cold
        let since_col = [(4, since.saturating_mul(1_000_000))];
        put("cachefloor", &format!("{}.since_take", c.name()), &since_col[..usize::from(since != u64::MAX)])?; // alloc: Cold
    }
    // why no raise, before the caches: the samples past the drain with raises on, and those the landing, the reclaim
    // quiet, the refault gate or the windows held; the samples still draining
    let gate = mcx::ladder::raise_gate();
    for (name, i) in [("raise_gate.samples", mcx::ladder::GATE_SAMPLES), ("raise_gate.landing", mcx::ladder::GATE_LANDING), ("raise_gate.unquiet", mcx::ladder::GATE_UNQUIET), ("raise_gate.refault", mcx::ladder::GATE_REFAULT), ("raise_gate.bound", mcx::ladder::GATE_BOUND), ("raise_gate.draining", mcx::ladder::GATE_DRAINING)] {
        put("cachefloor", name, &[(9, gate[i])])?;
    }
    // ladder C1b: the part cache's hold reach at the last sample (its budget plus the room a raise could give; 0 = none)
    put("cachefloor", "part_cache.reach", &[(13, mcx::ladder::part_hold_reach())])?;
    let (evicts, over) = (crate::dict::text_hosts::fp_cap_evictions(), crate::dict::text_hosts::fp_over_cap_pinned());
    put("cachefloor", "fp_tables.cap_evicted", &[(9, evicts), (10, crate::dict::text_hosts::fp_cap_evicted_bytes())])?;
    put("cachefloor", "fp_tables.over_cap_pinned", &[(9, over.0), (10, over.1)])?;
    // ladder 1.3-1.4 (`fair_share`): the samples that set the shares, the level (NULL: all fit), the queries over their
    // share at the last, and the cuts lifted (by the sample) and uncut (by a participant)
    let f = |i: usize| mcx::revoke::FAIR[i].load(core::sync::atomic::Ordering::Relaxed);
    let level = f(mcx::revoke::FAIR_LEVEL);
    let level_col = [(9, f(mcx::revoke::FAIR_TICKS)), (8, f(mcx::revoke::FAIR_OVER)), (13, level)];
    put("fair", "level", &level_col[..2 + usize::from(level != u64::MAX)])?;
    put("fair", "lifts", &[(9, f(mcx::revoke::FAIR_LIFTS)), (10, f(mcx::revoke::FAIR_UNCUTS))])?;
    let (trims, parts, trimmed) = pgrc_read::partcache::counters();
    put("cachefloor", "part_cache.trims", &[(8, parts), (9, trims), (10, trimmed)])?;
    // D4802: the real evictions (faces once, as they leave the census), the faces tier's count and the bytes loaded
    let reg = pgrc_am::inval::registry();
    let (real_runs, real_bytes) = reg.real_evictions();
    put("cachefloor", "part_cache.evicted_real", &[(9, real_runs), (10, real_bytes)])?;
    let (faces_runs, faces_counted, _) = reg.faces_trims();
    put("cachefloor", "part_cache.faces", &[(9, faces_runs), (10, faces_counted)])?;
    put("cachefloor", "part_cache.loads", &[(10, pgrc_read::partcache::loaded_bytes())])?;
    // D3268's short hook (a content trim ended past its target with parts in use): its releases of every parked set
    // (pressure) and of all but the newest; and the slot depot's takes / puts / evictions: the paths that let parked
    // sets go with no R2q count (Job M cell 3b)
    put("cachefloor", "part_cache.short_releases", &[(9, crate::ops::scan::pgrc::residency::part_cache_short_releases())])?;
    put("cachefloor", "part_cache.older_releases", &[(9, crate::ops::scan::pgrc::residency::part_cache_older_releases())])?;
    let d = gravity_driver::slots::DEPOT.stats();
    put("cachefloor", "slot_depot.moves", &[(2, d.takes), (3, d.puts), (9, d.evictions)])?;
    // the part cache's LOAD GATE: the loads that asked past the budget (misses), their waits for other statements' pins
    // (waits, wait_ns), the waits that ran out at bytes_wait_ms (refusals), the loads that went on past the budget
    // (climbs), the bytes their trims evicted (relief_bytes) and (ladder C1b) the loads the floor held to its cushion
    // (runs)
    let (asks, load_trimmed, waits, wait_ms, over) = pgrc_read::partcache::load_counters();
    let gate_col = [(2, asks), (3, waits), (4, wait_ms.saturating_mul(1_000_000)), (6, pgrc_read::partcache::load_wait_timeouts()), (8, over), (9, pgrc_read::partcache::load_held()), (10, load_trimmed)];
    put("cachefloor", "load_gate", &gate_col)?;
    // cachepolicy (C5, cache_policy = 2): the condition cache's W-TinyLFU moves (runs) and its segments' bytes:
    // tlfu.cond.admitted / rejected / promoted / demoted (9), tlfu.cond.window / probation / protected (10)
    let c = crate::ops::scan::pgrc::cond_cache::tinylfu_census();
    for (i, name) in ["admitted", "rejected", "promoted", "demoted", "window", "probation", "protected"].into_iter().enumerate() {
        put("cachepolicy", &format!("tlfu.cond.{name}"), &[(if i < 4 { 9 } else { 10 }, c[i])])?; // alloc: Cold
    }
    let k = mcx::ladder::c1_counts();
    put("cachefloor", "background", &[(2, k.bg_quiet), (9, k.bg_runs), (10, k.bg_bytes)])?;
    put("cachefloor", "inline", &[(2, k.inline_short), (5, k.inline_ns_max), (9, k.inline_runs), (10, k.inline_bytes), (11, k.inline_ns)])?;
    put("cachefloor", "refill_closed", &[(6, k.refill_closed)])?;
    // ladder C1c, the refault gate: the cgroup's file bytes refaulted in a minute (NULL: no cgroup count) and the passes
    // it held a raise in
    let refaults = mcx::ladder::file_refaults_per_min();
    let refault_col = [(9, mcx::ladder::refault_held()), (13, refaults.unwrap_or(0))];
    put("cachefloor", "refaults", &refault_col[..1 + usize::from(refaults.is_some())])?;
    put("cachefloor", "giveback", &[(9, k.give_back_runs), (10, k.give_back_bytes), (11, k.give_back_ns)])?;
    put("cachefloor", "giveback_ns_per_gib", &[(11, rate)])?;
    let (sb_runs, sb_bytes) = mcx::ladder::buffers_counts();
    put("cachefloor", "shared_buffers", &[(9, sb_runs), (10, sb_bytes)])?;
    // buf_grow: the ladder's grows of the pool into free memory (runs, bytes scheduled)
    let (sb_grows, sb_grown) = mcx::ladder::buffers_grow_counts();
    put("cachefloor", "shared_buffers.grows", &[(9, sb_grows), (10, sb_grown)])?;
    // ladder_reconcile: the samples checked, the split and tolerance of the last, every term with its kind and readers,
    // and every row a rule can break
    let (samples, split, tol) = mcx::reconcile::split();
    put("reconcile", "samples", &[(9, samples)])?;
    for (name, x) in mcx::reconcile::SPLIT_NAMES.iter().zip(split) {
        put("reconcile", &format!("split.{name}"), &[(13, x)])?; // alloc: Cold
    }
    put("reconcile", "split.tolerance", &[(13, tol)])?;
    use core::fmt::Write as _;
    use mempolicy::reconcile::{self as rc, Row, Term};
    // the consumers in `bits`, comma-separated, after what `name` holds (each name is written on the stack)
    let readers = |name: &mut RungText, bits: u8| {
        let mut sep = "";
        for c in rc::Consumer::ALL.iter().filter(|c| bits & (1 << **c as u8) != 0) {
            let _ = write!(name, "{sep}{}", c.name());
            sep = ",";
        }
    };
    for t in Term::ALL {
        let (last, most) = mcx::reconcile::term(t);
        let d = t.def();
        let mut name = RungText { b: [0; 256], n: 0 };
        let _ = write!(name, "term.{} ({}) -> ", d.name, d.kind.name());
        readers(&mut name, d.feeds);
        let col = [(10, most), (13, last)];
        put("reconcile", name.as_str(), &col[..1 + usize::from(last != rc::NONE)])?;
    }
    for i in 0..rc::ROWS {
        let r = Row::of(i);
        if !r.applies() {
            continue;
        }
        let mut name = RungText { b: [0; 256], n: 0 };
        let _ = match r {
            Row::WrongKind(t) => {
                let _ = write!(name, "r1.{} ({}) -> ", t.name(), t.def().kind.name());
                readers(&mut name, t.wrong_feeds());
                Ok(())
            }
            Row::OverCurrent(t) => write!(name, "r2.{} > current", t.name()),
            Row::Split(c) => write!(name, "r3.{}", c.name()),
            Row::RetainedAsUsed => write!(name, "r4.b.retained (allocator free) counted as used"),
        };
        let k = mcx::reconcile::row(i);
        put("reconcile", name.as_str(), &[(8, k.at_worst), (9, k.hits), (10, k.worst), (13, k.last)])?;
    }
    // D4910 step 1 (with the knob on: a walk of the in-use parts): what the part cache's pins are made of, and the most one
    // running statement's account holds
    if mcx::reconcile::on() {
        let reg = pgrc_am::inval::registry();
        let b = reg.pin_breakdown();
        let mut top = 0u64;
        reg.in_use_by_running(|_, bytes| top = top.max(bytes));
        put("reconcile", "pins.parts_live", &[(9, b.parts_live)])?;
        put("reconcile", "pins.parts_claimed", &[(9, b.parts_claimed)])?;
        put("reconcile", "pins.parts_deferred", &[(9, b.parts_deferred)])?;
        put("reconcile", "pins.users_max", &[(9, b.users_max)])?;
        put("reconcile", "pins.images", &[(13, b.images)])?;
        put("reconcile", "pins.passed", &[(13, b.passed)])?;
        put("reconcile", "pins.dicts", &[(13, b.dicts)])?;
        put("reconcile", "pins.pinned", &[(13, b.pinned)])?;
        put("reconcile", "pins.fully_passed", &[(9, b.parts_fully_passed), (13, b.fully_passed_pinned)])?;
        put("reconcile", "pins.top_statement", &[(13, top)])?;
        // D4910 / D4908: the bytes the cache's trim cannot drop now by holder, the parked gate, the deferred parts' bytes
        let u = reg.untrimmable_by_holder();
        put("reconcile", "pins.untrimmable.live", &[(13, u.live)])?;
        put("reconcile", "pins.untrimmable.deferred", &[(13, u.deferred)])?;
        put("reconcile", "pins.untrimmable.parked", &[(13, u.parked)])?;
        put("reconcile", "pins.untrimmable.none", &[(13, u.none)])?;
        put("reconcile", "pins.parked_gate", &[(9, u64::from(u.parked_gate))])?;
        put("reconcile", "pins.deferred_pinned", &[(13, b.deferred_pinned)])?;
    }
    Ok(srf.finish(fcinfo))
}

/// The episode rows' columns (scripts/memory-broker-views.sql declares the same, in order).
const EPISODE_COLS: usize = 11;

/// A short text on the stack: a rung list ("R2a 3145728, R4b 1048576": each rung that ran and its measured relief,
/// bytes), or an accounts row's name.
struct RungText {
    b: [u8; 256],
    n: usize,
}

impl RungText {
    /// The text written so far (cut at a character boundary when the buffer filled).
    fn as_str(&self) -> &str {
        match core::str::from_utf8(&self.b[..self.n]) {
            Ok(t) => t,
            Err(e) => core::str::from_utf8(&self.b[..e.valid_up_to()]).unwrap_or(""),
        }
    }
}

impl core::fmt::Write for RungText {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let k = s.len().min(self.b.len() - self.n);
        self.b[self.n..self.n + k].copy_from_slice(&s.as_bytes()[..k]);
        self.n += k;
        Ok(())
    }
}

/// `pgrust_memory_ladder_episodes()`: the last `mcx::ladder_stats::RING` ladder episodes, oldest first: seq (since the
/// server started), started, ended (timestamptz, from the ladder's monotonic clock against the wall clock read now),
/// wait_us, site (a gate's wait, or the hook's episode past the line), need_bytes (the ask; the hook: its need at the
/// start), read_bytes and line_bytes (the ladder's read and line at the start; NULL when the ladder had none: the read
/// needs `pgrust.memory_ladder = on`), rungs (each rung that ran on the episode's thread and its measured relief; NULL:
/// none), relief_bytes (their sum), outcome (covered, halved, refused, canceled, victim_canceled; the block gate's
/// overran: its bound passed and the block was taken, interrupted: a pending interrupt ended its wait).
pub fn fc_pgrust_memory_ladder_episodes(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    use core::fmt::Write as _;
    let flinfo = flinfo.expect("pgrust_memory_ladder_episodes: resolved FmgrInfo required");
    let mcx = super::raw::result_mcx(fcinfo);
    let mut srf = funcapi::InitMaterializedSRF(mcx, flinfo, fcinfo, 0)?;
    declared(&srf, EPISODE_COLS)?;
    // the wall clock now as a timestamptz (microseconds since 2000-01-01, PostgreSQL's epoch), as GetCurrentTimestamp reads it
    let (sec, usec) = pg_clock::wall_timeval();
    let now_tz = (sec - 946_684_800) * 1_000_000 + usec as i64;
    let now_ns = mcx::ladder_stats::clock_ns();
    let at = |ns: u64| Datum::from_i64(now_tz.saturating_sub((now_ns.saturating_sub(ns) / 1000) as i64));
    let mut out: PgResult<()> = Ok(());
    mcx::ladder_stats::for_each_episode(&mut |e| {
        if out.is_err() {
            return;
        }
        let mut t = RungText { b: [0; 256], n: 0 };
        let mut relief = 0u64;
        for r in mempolicy::ladder::ASKER {
            if e.ran & (1 << r as u16) != 0 {
                let b = e.relief[r as usize];
                relief = relief.saturating_add(b);
                let _ = write!(t, "{}{} {b}", if t.n == 0 { "" } else { ", " }, r.name());
            }
        }
        let rungs = core::str::from_utf8(&t.b[..t.n]).unwrap_or("");
        let values: [Datum; EPISODE_COLS] = [
            i8d(e.seq),
            at(e.start_ns),
            at(e.end_ns),
            i8d(e.end_ns.saturating_sub(e.start_ns) / 1000),
            text(mcx, e.site.name()),
            i8d(e.need),
            i8d(e.read),
            i8d(e.line),
            text(mcx, rungs),
            i8d(relief),
            text(mcx, e.outcome.name()),
        ];
        let nulls = [false, false, false, false, false, false, e.read == 0, e.line == 0, e.ran == 0, false, false];
        out = srf.putvalues(&values, &nulls);
    });
    out?;
    Ok(srf.finish(fcinfo))
}

/// The revoke row's columns (scripts/memory-broker-views.sql declares the same, in order).
const REVOKE_COLS: usize = 15;

/// An int8[] of `xs` up to its last non-zero element.
fn int8_array_n<'m, const N: usize>(mcx: mcx::Mcx<'m>, xs: &[u64; N]) -> PgResult<Datum> {
    let n = xs.iter().rposition(|&x| x != 0).map_or(0, |i| i + 1);
    let elems: [Datum; N] = core::array::from_fn(|k| i8d(xs[k]));
    let img = arrayfuncs::construct_array(mcx, &elems[..n], types_core::catalog::INT8OID, 8, true, b'd')?;
    Ok(varlena_result(datum::Varlena::from_image(img)))
}

/// `pgrust_memory_revoke_stats()`: one row, read now, since the server started: asks, asked_bytes (`mcx::revoke`'s, every
/// ask: R4b's and the projection rule's of others, R4a's and the test lever's of the asker itself), give_backs,
/// given_bytes (the participants' give-backs to the kernel for an ask), settled (asks settled: by the statement's next
/// ask or its end; the clocks' samples), then the two REVOKESLA clocks of the settled asks, each as samples, misses (no cut
/// / nothing given), p50 and p99 (the log2 bucket's bound, ms) and the histogram (element `k + 1`: bucket k, 0 ms for
/// k = 0, else [2^(k-1), 2^k) ms; trailing empty buckets cut): stopped_* (clock 1: the ask to its last cut, when the
/// statement stopped growing) and given_* (clock 2: the ask to its last give-back).
pub fn fc_pgrust_memory_revoke_stats(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    use mcx::ladder_stats::quantile_ms;
    let flinfo = flinfo.expect("pgrust_memory_revoke_stats: resolved FmgrInfo required");
    let mcx = super::raw::result_mcx(fcinfo);
    let mut srf = funcapi::InitMaterializedSRF(mcx, flinfo, fcinfo, 0)?;
    declared(&srf, REVOKE_COLS)?;
    let r = mcx::ladder_stats::revoke();
    let q = |h: &[u64; mcx::ladder_stats::BUCKETS], p: u64| quantile_ms(h, p).map_or(Datum::from_i64(0), i8d);
    let values: [Datum; REVOKE_COLS] = [
        i8d(r.asks),
        i8d(r.asked_bytes),
        i8d(r.give_backs),
        i8d(r.given_bytes),
        i8d(r.settled),
        i8d(r.clock1.iter().sum()),
        i8d(r.no_cut),
        q(&r.clock1, 500),
        q(&r.clock1, 990),
        int8_array_n(mcx, &r.clock1)?,
        i8d(r.clock2.iter().sum()),
        i8d(r.nothing_given),
        q(&r.clock2, 500),
        q(&r.clock2, 990),
        int8_array_n(mcx, &r.clock2)?,
    ];
    let (none1, none2) = (quantile_ms(&r.clock1, 500).is_none(), quantile_ms(&r.clock2, 500).is_none());
    let nulls = [false, false, false, false, false, false, false, none1, none1, false, false, false, none2, none2, false];
    srf.putvalues(&values, &nulls)?;
    Ok(srf.finish(fcinfo))
}

#[cfg(test)]
mod tests {
    #[test]
    fn pool_ownerless_reads_resident_where_it_is_below_claimed() {
        const M: i64 = 1 << 20;
        // claimed 100, resident 60 (40 declared, never touched), owners 50: 10 unowned, 40 untouched
        assert_eq!(super::pool_ownerless(100 * M, Some(60 * M), 50 * M), (10 * M, 40 * M));
        // resident at or past claimed (a fault since the last sweep): claimed bounds it, nothing untouched
        assert_eq!(super::pool_ownerless(100 * M, Some(120 * M), 50 * M), (50 * M, 0));
        // no sweep yet: claimed, as before
        assert_eq!(super::pool_ownerless(100 * M, None, 50 * M), (50 * M, 0));
        // owners past what is resident (held 30 < owned 50): unowned reads below 0, not clamped
        assert_eq!(super::pool_ownerless(100 * M, Some(30 * M), 50 * M), (-20 * M, 70 * M));
        // owners past claimed itself, no sweep yet: below 0 too
        assert_eq!(super::pool_ownerless(40 * M, None, 50 * M), (-10 * M, 0));
    }

    #[test]
    fn pool_unowned_terms_name_slack_rounding_and_tails() {
        const M: usize = 1 << 20;
        let mut st = pagepool::Stats::default();
        st.in_use[pagepool::Tag::SMALL.0 as usize] = 10 * M; // small runs
        st.in_use[pagepool::Tag::HEAP.0 as usize] = 20 * M; // door 1's big blocks
        st.in_use[pagepool::Tag::MCX.0 as usize] = 33 * M; // mcx blocks at the pool's usable size
        st.in_use[pagepool::Tag::REGION.0 as usize] = 7 * M;
        st.waste = 2 * M;
        // door 1 live: 26 MiB (4 MiB free slots in the runs); owners posted 38 of the 40 MiB the other tags hold
        assert_eq!(super::pool_unowned_terms(&st, 26 << 20, 38 << 20), (4 << 20, 2 << 20, 2 << 20));
        // the pool off: nothing
        assert_eq!(super::pool_unowned_terms(&pagepool::Stats::default(), 0, 0), (0, 0, 0));
    }

    use super::{give_back_spill_rows, statement_charged};
    use gravity_driver::mem::giveback::Counts;

    /// Criterion 11 (every number readable in SQL): the draw trim's inline park gives have their rows. With the knob off
    /// (this process never queues a park job) both read 0; a draw trim's fallback that gave one park job back on its own
    /// thread (the driver's h3s4c test counts it in Counts) reads as one give and its bytes.
    #[test]
    fn the_park_drained_here_rows_read_0_with_the_knob_off_and_count_an_inline_park_give() {
        let row = |c: &Counts, name: &str| give_back_spill_rows(c).into_iter().find(|r| r.0 == name).map(|r| r.1);
        let off = gravity_driver::mem::giveback::counts();
        assert_eq!(off.park_jobs, 0, "knob off: no park job ever queued here");
        assert_eq!(row(&off, "give_back_park_drained_here"), Some(0));
        assert_eq!(row(&off, "give_back_park_drained_here_bytes"), Some(0));
        let bytes = gravity_driver::mem::reap::REAP_MIN as u64;
        let one = Counts { park_drained_here: 1, park_drained_here_bytes: bytes, ..off };
        assert_eq!(row(&one, "give_back_park_drained_here"), Some(1));
        assert_eq!(row(&one, "give_back_park_drained_here_bytes"), Some(bytes as i64));
    }

    /// Criterion 11: TB4's overdraft has its rows. With the knob off (this process never overdraws) all read 0; one take
    /// past the limit on the landing (the driver's tb4 test counts it in Counts) reads as one, its bytes and the largest.
    #[test]
    fn the_overdraft_rows_read_0_with_the_knob_off_and_count_a_take_on_the_landing() {
        let row = |c: &Counts, name: &str| give_back_spill_rows(c).into_iter().find(|r| r.0 == name).map(|r| r.1);
        let off = gravity_driver::mem::giveback::counts();
        assert_eq!(off.jobs, 0, "knob off: no job ever queued here");
        for name in ["give_back_spill_overdraft", "give_back_spill_overdraft_bytes", "give_back_spill_overdraft_max"] {
            assert_eq!(row(&off, name), Some(0), "{name}");
        }
        let bytes = gravity_driver::mem::reap::REAP_MIN as u64;
        let one = Counts { overdraft: 1, overdraft_bytes: bytes, overdraft_max: 2 * bytes, ..off };
        assert_eq!(row(&one, "give_back_spill_overdraft"), Some(1));
        assert_eq!(row(&one, "give_back_spill_overdraft_bytes"), Some(bytes as i64));
        assert_eq!(row(&one, "give_back_spill_overdraft_max"), Some(2 * bytes as i64));
    }

    /// Criterion 11: take-back's whole gives a region draw took back off the lane have their rows. With the knob off (this
    /// process never queues a job) both read 0; one take-back (the driver's tb1 test counts it in Counts) reads as one
    /// and its bytes.
    #[test]
    fn the_taken_back_rows_read_0_with_the_knob_off_and_count_a_take_back() {
        let row = |c: &Counts, name: &str| give_back_spill_rows(c).into_iter().find(|r| r.0 == name).map(|r| r.1);
        let off = gravity_driver::mem::giveback::counts();
        assert_eq!(off.jobs, 0, "knob off: no job ever queued here");
        assert_eq!(row(&off, "give_back_spill_taken_back"), Some(0));
        assert_eq!(row(&off, "give_back_spill_taken_back_bytes"), Some(0));
        let bytes = gravity_driver::mem::reap::REAP_MIN as u64;
        let one = Counts { taken_back: 1, taken_back_bytes: bytes, ..off };
        assert_eq!(row(&one, "give_back_spill_taken_back"), Some(1));
        assert_eq!(row(&one, "give_back_spill_taken_back_bytes"), Some(bytes as i64));
    }

    #[test]
    fn a_backend_row_shows_the_governors_count_of_its_running_statement() {
        let mut r = memledger::account_row(memledger::Acct::backend(7));
        r.running = true;
        r.statement_bytes = 0;
        assert_eq!(statement_charged(&r, &[(6, 1, 1), (7, 5 << 30, 6 << 30)]), Some((5 << 30, 6 << 30)));
        assert_eq!(statement_charged(&r, &[(6, 1, 1)]), None, "a statement the governor does not charge: NULL");
        r.running = false;
        assert_eq!(statement_charged(&r, &[]), Some((0, 0)));
        let mut unbound = memledger::account_row(memledger::Acct::UNBOUND);
        unbound.running = false;
        assert_eq!(statement_charged(&unbound, &[(0, 9, 9)]), Some((0, 0)), "not a backend: no statement");
    }
}
