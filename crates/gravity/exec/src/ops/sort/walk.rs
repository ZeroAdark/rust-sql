//! M3's walk (`topk.walk`, ProtoKind::Walk; cards/M3.md L3, Decision 7): the pipeline's claims are the runner's ordered
//! walk (exec `runner/plan.rs`: `TopKDesc.walking(attno)`, the resident ZoneOrder installed on the granule list,
//! `GranuleRange::ordered`, the stop at the bound the workers' partials prove, `observe_bound`), which the pool's runtime
//! honours at any width (pool `Units` cuts its own unit ranges and never calls a protocol's `claim()`, D504). Declared as
//! `Proto::new(Walk, pipeline).site(site).arg(attno | bit 16 group | bit 18 runner [| bit 17 width])`; bit 18
//! (`WALK_RUNNER`) names that walk and is required, with the pipeline's `walk_attno` the same attribute. Owner M3-HOSTS.
//! A group walk (runs/walk.rs, M3-WALK) names the group's slot: after its pipeline the owners' runs keep only the records
//! whose first key is at most the `n`-th smallest distinct first key they hold (`nth_key`, `cut_runs`). That key is at
//! most every bound a worker proved (a worker's table at any time holds a subset of the merged records), so each granule
//! whose minimum is at most it was visited (a claim is skipped only past the bound at that moment), every row of those
//! groups folded: they are exact, at least `n` distinct first keys of them. Bit 17 (width.walk, runs/walk_width.rs): the section's width
//! is the granules a serial walk can reach, those whose minimum is at most the `n`-th smallest distinct exact minimum
//! (`walk_width`): past it one walker has seen `n` distinct keys and the bound stops the walk. A row walk's (RD-width.walk)
//! is the census by rows (hosts `zone_walk_reach`): the granules in walk order until their value rows (a nullable key's
//! non-null counts) reach `n`, then every one whose key is at most their farthest edge; a reach under 2 runs the walk on
//! the leader alone (W0's).
use gravity_abi::breaker::{RecordLayout, ORDER_KEY, ORDER_NONE};
use gravity_program::protos::{Proto, ProtoKind};
use types_error::PgResult;

use super::super::{Live, Prepare, Protocol};

pub(crate) struct Walk;

/// The protocol's `arg` (lowerv4 runs/walk.rs): the attribute in the low 16 bits; a group walk; width.walk; the runner's
/// ordered walk (D504: required, the only walk the pool's runtime honours at W >= 2).
const WALK_GROUP: u64 = 1 << 16;
const WALK_WIDTH: u64 = 1 << 17;
const WALK_RUNNER: u64 = 1 << 18;

impl Protocol for Walk {
    fn kind(&self) -> ProtoKind {
        ProtoKind::Walk
    }
    /// The walk's cut after its pipeline; under width.walk the section's width from the resident zone column's minima.
    fn prepare(&self, p: &Proto, cx: &mut Prepare<'_>) -> PgResult<Option<Box<dyn Live>>> {
        let (site, attno) = (p.site as usize, (p.arg & 0xffff) as u32);
        let (walk_attno, walk_desc) = cx.program.pipelines.get(p.pipeline as usize).and_then(|x| x.topk).map_or((0, false), |t| (t.walk_attno, t.walk_desc));
        runner_walk(p.arg, walk_attno, attno).map_err(crate::glue::internal)?;
        let Some(desc) = cx.program.sites.get(site).filter(|_| p.site >= 0 && attno > 0) else {
            return Err(crate::glue::internal("gravity: topk.walk over a site or attribute the program does not have"));
        };
        let (k, pipe) = (p.pipeline as usize, &cx.program.pipelines[p.pipeline as usize]);
        let n = pipe.topk.map_or(0, |t| t.n) as usize;
        // a group walk's owner keeps its runs (the generic phase, key-ordered or not): the cut reads them after the pipeline
        let cut = match (p.arg & WALK_GROUP != 0, pipe.owner) {
            (false, _) => None,
            (true, Some(o)) if o.ctor.is_none() && o.ctor_desc.is_none() && matches!(o.order_slot, ORDER_KEY | ORDER_NONE) && p.slot == pipe.produces().unwrap_or(!p.slot) => Some((p.slot, o.acc, n, if walk_desc { !0 } else { 0 })),
            (true, _) => return Err(crate::glue::internal("gravity: topk.walk's group walk over a pipeline whose owner keeps no runs")),
        };
        if p.arg & WALK_WIDTH != 0 && cx.widths[k] > 0 {
            // the walk's order from the resident zone store (runner/plan.rs claims_of read or built it this statement):
            // its minima come sorted, so the reach is a scan to the n-th distinct minimum and a search, no sort here; a row
            // walk's (RD-width.walk) is the census by rows: the granules until n rows, then those under their farthest edge
            let rel = table::table_open(cx.mcx, desc.relation, types_storage::lock::NoLock)?;
            let reach = gravity_hosts::resolve(&rel, Some(&**cx.snap)).and_then(|list| match cut {
                Some(_) => Ok(gravity_hosts::zone_walk_order(&rel, &list, attno, walk_desc)?.filter(|o| !o.mins.is_empty()).map(|o| walk_width(&o.mins, n))),
                None => gravity_hosts::zone_walk_reach(&rel, &list, attno, walk_desc, n as u64),
            });
            rel.close(types_storage::lock::NoLock)?;
            if let Some(reach) = reach? {
                cx.widths[k] = if reach < 2 { 0 } else { cx.widths[k].min(reach.min(u16::MAX as usize) as u16) };
                // shapecheck fire signal (REGISTRY RD-width.walk): the walk's section width came from its reach
                fires::fire(if cut.is_some() { "width.walk.group" } else { "width.walk.rows" });
            }
        }
        Ok(Some(Box::new(WalkLive { pipeline: k, cut }))) // alloc: Exec
    }
}

/// The walk is the runner's ordered walk (D504): `WALK_RUNNER` set and the pipeline's ladder walk on the same attribute.
fn runner_walk(arg: u64, walk_attno: i16, attno: u32) -> Result<(), &'static str> {
    match (arg & WALK_RUNNER != 0, walk_attno as i64 == attno as i64) {
        (true, true) => Ok(()),
        (false, _) => Err("gravity: topk.walk without the runner's ordered walk (WALK_RUNNER)"),
        (true, false) => Err("gravity: topk.walk over another attribute than its pipeline's walk (TopKDesc.walk_attno)"),
    }
}

/// width.walk: the granules a serial walk reaches, those whose minimum is at most the `n`-th smallest distinct exact
/// minimum (a non-exact granule, `i64::MIN`, always), every granule when fewer than `n` distinct exact minima exist.
/// `sorted` is the zone order's minima, ascending (`ZoneOrder::mins`: the non-exact granules first).
pub(crate) fn walk_width(sorted: &[i64], n: usize) -> usize {
    let (mut seen, mut prev) = (0usize, i64::MIN);
    for &m in sorted {
        // a new distinct exact minimum: `i64::MIN` (non-exact) is never above `prev`
        if m > prev {
            (seen, prev) = (seen + 1, m);
            if seen == n {
                return sorted.partition_point(|&x| x <= m);
            }
        }
    }
    sorted.len()
}

/// D518 lever C (the ladder's agg_walk_cut): `keys` holds the `n` smallest distinct keys seen, sorted; `k` goes in by a
/// binary search, the key past `n` drops.
pub(super) fn smallest_insert(keys: &mut Vec<i64>, n: usize, k: i64) {
    if n == 0 || (keys.len() == n && k >= keys[n - 1]) {
        return;
    }
    if let Err(i) = keys.binary_search(&k) {
        keys.insert(i, k);
        keys.truncate(n);
    }
}

struct WalkLive {
    pipeline: usize,
    /// A group walk's slot, owner record, `n` and key flip (GP-1b: `!0` descending): the runs cut after the pipeline.
    cut: Option<(u8, RecordLayout, usize, i64)>,
}

impl Live for WalkLive {
    fn answers(&self) -> u16 {
        crate::ops::ask::AFTER
    }

    /// A group walk's cut, once its pipeline (and owner phase) ran; under `n` distinct keys nothing is cut. `None`: the
    /// runner's own after-pipeline steps still run.
    fn after_pipeline(&self, pipeline: usize, exec: &mut gravity_driver::Exec) -> Option<gravity_driver::Result<()>> {
        let (slot, layout, n, flip) = self.cut.filter(|_| pipeline == self.pipeline)?;
        let owner = exec.node().slot_owner(slot);
        if !super::raw::group_cut(exec.node().slot_region(slot), &layout, n, flip, owner) {
            return Some(Err(gravity_driver::Error::host("gravity: topk.walk's group cut found no owner runs")));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D504: the walk is the runner's ordered walk alone: bit 18 required, the pipeline's ladder walk on the attribute.
    #[test]
    fn the_walk_needs_the_runner_bit_and_the_ladder_walk_on_its_attribute() {
        assert_eq!(runner_walk(10 | WALK_GROUP | WALK_RUNNER, 10, 10), Ok(()));
        assert!(runner_walk(10 | WALK_GROUP, 10, 10).is_err() && runner_walk(10 | WALK_GROUP, 0, 10).is_err());
        assert!(runner_walk(10 | WALK_GROUP | WALK_RUNNER, 0, 10).is_err() && runner_walk(10 | WALK_RUNNER, 3, 10).is_err());
    }

    /// width.walk: the granules up to the n-th smallest distinct exact minimum (non-exact ones always); all under n.
    #[test]
    fn walk_width_reaches_the_nth_distinct_exact_minimum() {
        let mut mins = [50, 40, 70, 60, 30, i64::MIN, 90, 10, 25, 20, 80, 15, 10];
        mins.sort_unstable(); // the zone order's minima
        assert_eq!(walk_width(&mins, 0), mins.len(), "no n: every granule");
        assert_eq!(walk_width(&mins, 1), 3, "10 twice and the non-exact granule");
        assert_eq!(walk_width(&mins, 3), 5, "10 10 15 20 and the non-exact granule");
        assert_eq!(walk_width(&mins, 12), mins.len(), "fewer than 12 distinct exact minima: every granule");
    }

    /// D518 lever C: the bounded insert keeps the n smallest distinct keys, sorted, as the full sort + dedup did.
    #[test]
    fn smallest_insert_keeps_the_n_smallest_distinct_keys() {
        let keys = [9, 3, 3, i64::MAX, -4, 7, 3, i64::MIN, 8, 7, 1];
        for n in 0..=10 {
            let mut all = keys.to_vec(); // alloc: Cold
            all.sort_unstable();
            all.dedup();
            all.truncate(n);
            let mut h = Vec::new(); // alloc: Cold
            keys.iter().for_each(|&k| smallest_insert(&mut h, n, k));
            assert_eq!(h, all, "n = {n}");
        }
    }
}
