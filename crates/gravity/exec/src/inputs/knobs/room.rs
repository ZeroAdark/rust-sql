//! The sizing knobs (breakers.md §2 decision 2): the statement's memory grant from the GUCs and the width a parallel program may run at.

use gravity_driver::Grant;
use gravity_parallel::installed_pool;

use super::{Kind, Knob};

pub static KNOBS: &[Knob] = &[
    Knob { name: "pgrust.gravity_decorrelate_ctes", kind: Kind::Custom, default: "materialized", doc: "an A/B knob, not a tuning one (read by the planner's costed decorrelation, decorrelate.rs, GP-6 / D3055): on: a CTE body that ss_process_ctes inlines is offered the grouped-join form like a subquery; all: also a non-MATERIALIZED body read more than once; materialized: also a MATERIALIZED body (a PG-visible divergence: the keyword is PG's optimization fence); off: CTE bodies keep their SubPlans; a volatile, data-modifying or recursive body is never rewritten; quote the value in SET (ALL is a reserved word: SET pgrust.gravity_decorrelate_ctes = 'all')" },
    Knob { name: "pgrust.gravity_expr_group_image", kind: Kind::Custom, default: "off", doc: "a census knob, not a tuning one (read by the planner's estimate_num_groups, selfuncs.rs expression_group_image, PL-11): on: an IMMUTABLE expression of one column takes its group count from the expression evaluated over the column's ANALYZE values (Duj1) instead of the bounded name lists; off: the name lists (default, until the user's GO)" },
    Knob { name: "pgrust.gravity_grant_mb", kind: Kind::Custom, default: "unset (work_mem x hash_mem_multiplier per worker)", doc: "the execution's memory grant as a whole, in MiB; the width comes from max_parallel_workers_per_gather bounded by max_parallel_workers" },
    Knob { name: "pgrust.gravity_statement_pool_fraction", kind: Kind::Custom, default: "1", doc: "D2990: a ceiling on the part of the grant pool's big lane one statement may hold, its grant and its growth past it together (0 < f <= 1, per session); a draw past it is refused at once and the statement spills or runs on the row executor. 1 (the default) sets no ceiling: past its grant a statement draws what the pool has room for, each breaker up to its work_mem (x hash_mem_multiplier for a hash node) per participant" },
    Knob { name: "pgrust.gravity_twopass", kind: Kind::Custom, default: "cost", doc: "a testing and rollback knob, not a tuning one (read by the planner's two-pass top, gravity_paths.rs): force: the two-pass form wherever its shape applies and PostgreSQL's hash aggregate would spill; cost: the planner's reading of Gravity's two passes against its one pass at this width and grant" },
    Knob { name: "pgrust.gravity_cost_profile", kind: Kind::Custom, default: "measured", doc: "a rollback and A/B knob, not a tuning one (lane costcal CC-1; read by the planner's walk prices in gravity_paths.rs and the run-time group-walk re-check): measured (default): the group walks (clustered and spread) and the keyed walk priced from cost_profile's measured table (measured/<class>.tsv); fitted: the fitted constants of walk.rs" },
    Knob { name: "PGRUST_GRAVITY_T1_DESTAGE", kind: Kind::Env, default: "unset (on)", doc: "spillopt #5 (hybrid T1), an A/B and rollback knob, not a tuning one (read once per process by the driver's SPILL_SCATTER, mem/spill_host.rs): 0: a phase-1 table's full pool is written whole as before; else its flush writes the partitions it wrote before and the longest others until at most half the pool stays, the rest kept in memory for the owner phase (driver mem/spill.rs scatter_t1_destaging; whole pools again past DESTAGE_FLUSHES flushes)" },
    Knob { name: "pgrust.gravity_zonetopk", kind: Kind::Custom, default: "cost", doc: "a testing and rollback knob, not a tuning one (read by the planner's group walk, gravity_paths.rs add_zone_topk_path, RUNS-02): force: the group walk wherever its shape applies and its census skips a granule; cost: the walk's price (cost_profile walk.rs) against the cheapest plain plan at this width; asc: as cost, the ascending walk only (GP-1b rollback: no ORDER BY k DESC group walk)" },
];

/// The execution's memory grant (§2 decision 2): `pgrust.gravity_grant_mb` as the whole, else `work_mem × hash_mem_multiplier` PER WORKER as PostgreSQL grants a parallel hash node — the split hands each worker's phase-1 table its own work_mem (§2.2ak: a machine-wide 2 GiB ask cut 63 ways left 32 MiB shares, 4 KiB partition blocks and a 2.5 GB spill on the metal).
pub(crate) fn granted_memory(width: u16) -> Grant {
    let work_mem = (guc_tables::vars::work_mem.read() as i64).max(64) as u64;
    let per = Grant::of(crate::inputs::hash_mem::hash_memory_limit());
    // each breaker's ceiling, whatever the grant: PostgreSQL's own budget for its node (a Sort work_mem, a hash node
    // hash_mem) per participant, past which it spills (`gravity_driver::ceiling_of`)
    let knobs = super::grant::grant_knobs();
    if let Some(mb) = knobs.grant_mb {
        return Grant::of(mb << 20).with_ceilings(work_mem << 10, per.bytes).with_spill_every(knobs.spill_every).with_revoke_every(knobs.revoke_every).with_refuse_debt(knobs.refuse_debt);
    }
    let w = width.max(1) as u64;
    // D2587: each participant's share at least PostgreSQL's own budget for its node (a Sort work_mem, a hash node hash_mem):
    // one node's, not the width's (`Grant::split_at_least` floors the leader's share and each worker's own share at it)
    Grant::of(per.bytes.saturating_mul(w)).with_floors(work_mem << 10, per.bytes).with_ceilings(work_mem << 10, per.bytes).with_spill_every(knobs.spill_every).with_revoke_every(knobs.revoke_every).with_refuse_debt(knobs.refuse_debt)
}

/// Grow mode (`pgrust.gravity_grow`): whether work_mem was set, its source past the boot default (a SET, postgresql.conf,
/// ALTER SYSTEM / DATABASE / ROLE ... SET, the command line or the environment), so PostgreSQL's per-node ceiling stands;
/// unset, grow mode's breakers have no work_mem ceiling. A thread with no GUC store reads it set (today's ceilings).
pub(crate) fn work_mem_set() -> bool {
    const DYNAMIC_DEFAULT: i32 = 1; // types_guc::GucSource::PGC_S_DYNAMIC_DEFAULT: a default the server chose, still unset
    guc::store::with_store(|reg| reg.find_index("work_mem").map(|i| reg[i].gen().source as i32 > DYNAMIC_DEFAULT)).flatten().unwrap_or(true)
}

/// The width a parallel program may run at: `max_parallel_workers_per_gather` bounded by `max_parallel_workers` and the driver's `MAX_WIDTH` (noted once); 0 without a pool or under SERIALIZABLE.
pub(crate) fn granted_width() -> u16 {
    // SERIALIZABLE: a pool thread takes no predicate locks (the source's SSI check is its backend's thread's), so serial
    if installed_pool().is_none() || xact::IsolationIsSerializable() {
        return 0;
    }
    let per_gather = guc_tables::vars::max_parallel_workers_per_gather.read().max(0);
    let pool = guc_tables::vars::max_parallel_workers.read().max(0);
    clamp_width(per_gather.min(pool).min(u16::MAX as i32) as u16)
}

/// The width a section runs at: the ask bounded by the pool's threads (one worker per thread: more sections than workers time-slice the workers and run slower than one per worker) and then the driver's slot capacity; noted once.
pub(crate) fn clamp_width(width: u16) -> u16 {
    static NOTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let threads = installed_pool().map_or(0, |p| p.threads());
    let (bounded, why) = bound_width(width, threads, gravity_driver::MAX_WIDTH);
    if let Some(why) = why {
        if !NOTED.swap(true, std::sync::atomic::Ordering::AcqRel) {
            let _ = elog::ereport(types_error::LOG).errmsg(format!("gravity: width clamped from {width} to {bounded} ({why})")).finish(types_error::ErrorLocation::new("gravity", 0, "width")); // alloc: Cold
        }
    }
    bounded
}

pub(crate) fn bound_width(asked: u16, pool_threads: u16, capacity: u16) -> (u16, Option<&'static str>) {
    if pool_threads != 0 && asked > pool_threads && pool_threads <= capacity {
        return (pool_threads, Some("the pool's worker threads"));
    }
    if asked > capacity {
        return (capacity, Some("the driver's partial slots"));
    }
    (asked, None)
}

#[cfg(test)]
mod width_tests {
    use super::bound_width;
    #[test]
    fn the_width_is_bounded_by_the_pool_then_the_slots() {
        assert_eq!(bound_width(16, 16, 255), (16, None));
        assert_eq!(bound_width(16, 8, 255), (8, Some("the pool's worker threads")));
        assert_eq!(bound_width(191, 96, 255), (96, Some("the pool's worker threads")));
        assert_eq!(bound_width(300, 0, 255), (255, Some("the driver's partial slots")));
        assert_eq!(bound_width(300, 400, 255), (255, Some("the driver's partial slots")));
        assert_eq!(bound_width(8, 16, 255), (8, None));
        assert_eq!(bound_width(0, 16, 255), (0, None));
    }
}
