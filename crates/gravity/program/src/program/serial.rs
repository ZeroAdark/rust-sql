//! The one serial predicate (D3164): whether a pipeline's source or one
//! of its steps keeps the whole pipeline on the leader, and the width the heap rule gives a heap-driven pipeline. Every
//! reader describes what it sees in a `SourceShape` / `StepShape` built from its own facts (the lowerer's frame, exec's
//! admitted program, the planner's paths) and takes the verdict from here, so the planner's width costing (pathnode
//! gwcost.rs) cannot drift from what the lowerer and exec run: a change of the rule is made here once. The drift between
//! the facts each reader gathers is what the census's drift check measures (the planner's predicted pipelines against
//! the lowered program's descriptors).

use super::Reason;

/// Heap blocks per worker in the heap width rule (`heap_width`). MEASURED-HELDOUT and COST-DERIVED: exec
/// inputs/knobs/width.rs keeps the sweeps and the derivation (the blocks a worker must have before its wake pays).
pub const BLOCK_C0: u64 = 64;

/// The width a heap-driven pipeline runs at over `blocks` heap blocks with `claimed` workers granted: one worker per
/// `BLOCK_C0` blocks up to the grant, and the leader alone (0) when that gives fewer than two.
pub const fn heap_width(blocks: u64, claimed: u16) -> u16 {
    let w = blocks / BLOCK_C0;
    let w = if w < claimed as u64 { w } else { claimed as u64 };
    if w < 2 {
        0
    } else {
        w as u16
    }
}

/// What drives a pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceShape {
    /// A range site: a heap's or a column store's blocks / granules, claimed by every worker.
    Range,
    /// The bitmap morsel form of an index or bitmap scan: the leader walks the index once, the pool the heap pages.
    Morsel,
    /// A site that is not a range (an index walk in index order): a worker's non-range site traps.
    Index,
    /// A range whose order the access method vouches for over the whole relation (read serially, at most one part).
    Sorted,
    /// A range over a temporary relation: the backend's local buffers are the leader thread's.
    Temp,
    /// No driving site: a breaker's emit, a values list, a function's rows.
    NoSite,
}

/// The reason `s` keeps its pipeline on the leader, `None` when the pipeline may run on every worker.
pub const fn source_serial(s: SourceShape) -> Option<Reason> {
    match s {
        SourceShape::Range | SourceShape::Morsel => None,
        SourceShape::Index => Some(Reason::IndexSource),
        SourceShape::Sorted => Some(Reason::SortedSource),
        SourceShape::Temp => Some(Reason::TempSource),
        SourceShape::NoSite => Some(Reason::NoDrivingSite),
    }
}

/// A nested loop's inner when it is one probe of one btree site (D3014O nest.parallel): the facts a worker needs to run
/// its own copy of the inner with nothing shared but the pages it reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NestProbe {
    /// The site is a btree whose keys are plain columns.
    pub btree: bool,
    /// The probed relation is a heap.
    pub heap: bool,
    /// The probe's heap read deforms table columns only (and the row id), no system column.
    pub plain_cols: bool,
    /// Every key is `column op value` over a bare executor parameter (not an InitPlan's), a statement parameter or a
    /// constant: no computed key.
    pub bare_keys: bool,
}

/// A nested loop step, as the lowerer's widen rule (lowerv4 ops/nestloop/widen.rs) reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NestShape {
    /// `Some` when the inner is a bare probe of one site: no rerun set, LIMIT, hash probe, COPY row, streamed group or
    /// one-time gate above it (a Memoize may be: each worker ordinal keeps its own table).
    pub probe: Option<NestProbe>,
    /// The inner's pipeline is filters, projections and a Memoize only.
    pub inner_plain: bool,
    /// The inner makes a VOLATILE call.
    pub inner_volatile: bool,
    /// The outer's rows reach an end whose result shows their order (anything but an order-free fold, or a sort / top-k
    /// with no LIMIT, lock, visible call or error).
    pub end_order_visible: bool,
    /// The step fills a lazy Material (D2157).
    pub lazy_material: bool,
    /// A streamed group follows the nest.
    pub regroup: bool,
    /// The inner holds an owner of its own.
    pub keeps_owner: bool,
    /// D4532 (nest.parallel over a spool): the inner reads a plan Material's eager spool with nothing else above it (no
    /// rerun set, Memoize, LIMIT, hash probe, COPY row, streamed group or one-time gate), the nest has no parameters and
    /// the spool is filled whole before the outer's pipeline runs and only read after: a quiet fill (raise-free, no
    /// VOLATILE call, the same rows at every rescan) that no rerun set runs again and that does not wait for the first
    /// outer row. Each worker walks the one spool from its own position word. The planner's width costing does not
    /// read it yet (pathnode gwcost.rs: false), so no plan or EXPLAIN changes.
    pub spool: bool,
}

/// A step of a pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepShape {
    /// A nested loop's step (its inner embedded in the outer's pipeline).
    Nest(NestShape),
    /// A merge join: one machine per join (its state, mark and matched words are one scan's).
    Merge,
    /// A merge join in the co-split form (D3190, merge.cosplit): one machine per claim, a key range of both sorted inputs
    /// that no equal key straddles.
    MergeCoSplit,
    /// A hash probe: `lazy`, its build waits for the first row of the whole outer (D824, D1005); `clears_marks`, it clears a
    /// kept table's marks in one call (D2592); `ordered`, a RIGHT SEMI probe whose join qual may raise reads the outer in
    /// PostgreSQL's order (D2696).
    HashProbe { lazy: bool, clears_marks: bool, ordered: bool },
    /// A SubPlan evaluated per row: `scan`, an inner that scans a relation (its site is the step's); otherwise a once
    /// inner that reads only frame words, each worker's own.
    SubPlan { scan: bool },
    /// A lazy Material's fill (D2157): the spool's one filler.
    MaterialFill,
    /// A merge join's inner run (lowerv4 face/slot_run.rs): one worker fills it, so its last run is sealed in place, in the
    /// plan's order or sorted.
    MergeInnerRun,
    /// A merge join's inner Sort filled wide (D3190, merge.ksort): runs.sort's end on every worker, its runs merged into one
    /// at the barrier.
    MergeInnerWide,
    /// A step whose expressions call hosted functions: `pool_safe` when a pool thread can run every such call (lowerv4
    /// hir.rs `pool_unsafe_site` finds none: no user or extension code, each function parallel safe, each function-manager
    /// site one the pool hosts).
    HostedCall { pool_safe: bool },
}

/// The reason step `s` keeps its pipeline on the leader, `None` when the pipeline may run on every worker.
pub const fn step_serial(s: &StepShape) -> Option<Reason> {
    match s {
        StepShape::Nest(n) => {
            let probe = match n.probe {
                Some(p) => p.btree && p.heap && p.plain_cols && p.bare_keys,
                None => false,
            };
            if (probe || n.spool) && n.inner_plain && !n.inner_volatile && !n.end_order_visible && !n.lazy_material && !n.regroup && !n.keeps_owner {
                None
            } else {
                Some(Reason::Host)
            }
        }
        StepShape::Merge => Some(Reason::Host),
        StepShape::MergeCoSplit => None,
        StepShape::HashProbe { lazy, clears_marks, ordered } => {
            if *lazy || *clears_marks || *ordered {
                Some(Reason::Host)
            } else {
                None
            }
        }
        StepShape::SubPlan { scan } => {
            if *scan {
                Some(Reason::Host)
            } else {
                None
            }
        }
        StepShape::MaterialFill | StepShape::MergeInnerRun => Some(Reason::Sink),
        StepShape::MergeInnerWide => None,
        StepShape::HostedCall { pool_safe } => {
            if *pool_safe {
                None
            } else {
                Some(Reason::Host)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_heap_rule_gives_one_worker_per_block_c0_blocks_and_the_leader_under_two() {
        assert_eq!(heap_width(0, 16), 0);
        assert_eq!(heap_width(2 * BLOCK_C0 - 1, 16), 0, "fewer blocks than two startup claims: the leader");
        assert_eq!(heap_width(2 * BLOCK_C0, 16), 2);
        assert_eq!(heap_width(5 * BLOCK_C0 + 7, 16), 5);
        assert_eq!(heap_width(1 << 30, 16), 16, "never past the grant");
        assert_eq!(heap_width(1 << 30, 1), 0, "one worker granted: the leader");
        assert_eq!(heap_width(1 << 30, 0), 0);
    }

    #[test]
    fn a_range_or_morsel_source_runs_wide_and_every_other_source_on_the_leader() {
        assert_eq!(source_serial(SourceShape::Range), None);
        assert_eq!(source_serial(SourceShape::Morsel), None);
        assert_eq!(source_serial(SourceShape::Index), Some(Reason::IndexSource));
        assert_eq!(source_serial(SourceShape::Sorted), Some(Reason::SortedSource));
        assert_eq!(source_serial(SourceShape::Temp), Some(Reason::TempSource));
        assert_eq!(source_serial(SourceShape::NoSite), Some(Reason::NoDrivingSite));
    }

    fn wide() -> NestShape {
        let p = NestProbe { btree: true, heap: true, plain_cols: true, bare_keys: true };
        NestShape { probe: Some(p), inner_plain: true, inner_volatile: false, end_order_visible: false, lazy_material: false, regroup: false, keeps_owner: false, spool: false }
    }

    /// D4532: a nest over an eager spool (no probe) widens under the same facts as a probe's; each other fact alone keeps
    /// it on the leader, and with neither a probe nor a spool it stays there.
    #[test]
    fn a_nest_over_a_filled_spool_widens_under_the_same_facts() {
        let spool = NestShape { probe: None, spool: true, ..wide() };
        assert_eq!(step_serial(&StepShape::Nest(spool)), None);
        let host = Some(Reason::Host);
        assert_eq!(step_serial(&StepShape::Nest(NestShape { spool: false, ..spool })), host);
        assert_eq!(step_serial(&StepShape::Nest(NestShape { inner_plain: false, ..spool })), host);
        assert_eq!(step_serial(&StepShape::Nest(NestShape { inner_volatile: true, ..spool })), host);
        assert_eq!(step_serial(&StepShape::Nest(NestShape { end_order_visible: true, ..spool })), host);
        assert_eq!(step_serial(&StepShape::Nest(NestShape { lazy_material: true, ..spool })), host);
        assert_eq!(step_serial(&StepShape::Nest(NestShape { regroup: true, ..spool })), host);
        assert_eq!(step_serial(&StepShape::Nest(NestShape { keeps_owner: true, ..spool })), host);
    }

    /// D3014O: a nest widens only over a bare btree probe of a heap with plain columns and bare keys, a plain inner with no
    /// VOLATILE call, into an end that does not show the outer's order; each fact alone keeps it on the leader.
    #[test]
    fn a_nest_widens_only_when_every_fact_holds() {
        assert_eq!(step_serial(&StepShape::Nest(wide())), None);
        let host = Some(Reason::Host);
        assert_eq!(step_serial(&StepShape::Nest(NestShape { probe: None, ..wide() })), host);
        for p in [NestProbe { btree: false, ..wide().probe.unwrap() }, NestProbe { heap: false, ..wide().probe.unwrap() }, NestProbe { plain_cols: false, ..wide().probe.unwrap() }, NestProbe { bare_keys: false, ..wide().probe.unwrap() }] {
            assert_eq!(step_serial(&StepShape::Nest(NestShape { probe: Some(p), ..wide() })), host);
        }
        assert_eq!(step_serial(&StepShape::Nest(NestShape { inner_plain: false, ..wide() })), host);
        assert_eq!(step_serial(&StepShape::Nest(NestShape { inner_volatile: true, ..wide() })), host);
        assert_eq!(step_serial(&StepShape::Nest(NestShape { end_order_visible: true, ..wide() })), host);
        assert_eq!(step_serial(&StepShape::Nest(NestShape { lazy_material: true, ..wide() })), host);
        assert_eq!(step_serial(&StepShape::Nest(NestShape { regroup: true, ..wide() })), host);
        assert_eq!(step_serial(&StepShape::Nest(NestShape { keeps_owner: true, ..wide() })), host);
    }

    #[test]
    fn merge_lazy_probes_scan_subplans_and_fills_keep_the_leader() {
        assert_eq!(step_serial(&StepShape::Merge), Some(Reason::Host));
        assert_eq!(step_serial(&StepShape::MergeCoSplit), None);
        assert_eq!(step_serial(&StepShape::HashProbe { lazy: false, clears_marks: false, ordered: false }), None);
        assert_eq!(step_serial(&StepShape::HashProbe { lazy: true, clears_marks: false, ordered: false }), Some(Reason::Host));
        assert_eq!(step_serial(&StepShape::HashProbe { lazy: false, clears_marks: true, ordered: false }), Some(Reason::Host));
        assert_eq!(step_serial(&StepShape::HashProbe { lazy: false, clears_marks: false, ordered: true }), Some(Reason::Host));
        assert_eq!(step_serial(&StepShape::SubPlan { scan: true }), Some(Reason::Host));
        assert_eq!(step_serial(&StepShape::SubPlan { scan: false }), None);
        assert_eq!(step_serial(&StepShape::MaterialFill), Some(Reason::Sink));
        assert_eq!(step_serial(&StepShape::MergeInnerRun), Some(Reason::Sink));
        assert_eq!(step_serial(&StepShape::MergeInnerWide), None);
        assert_eq!(step_serial(&StepShape::HostedCall { pool_safe: true }), None);
        assert_eq!(step_serial(&StepShape::HostedCall { pool_safe: false }), Some(Reason::Host));
    }
}
