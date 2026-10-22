//! M12 COUNTERS (hosts): what a served EXPLAIN ANALYZE reads back from the hosts beside its sites' rows (entry
//! instrument.rs). Row B (main 09-29 GO): a bitmap heap scan's counters as PostgreSQL's two nodes keep them: the Bitmap
//! Index Scan's tuples (`MultiExecBitmapIndexScan`: the sum of `index_getbitmap`'s returns over the scan's array-key
//! passes), the Bitmap Heap Scan's exact and lossy pages (`BitmapHeapScanInstrumentation`, "Heap Blocks") and the rows its
//! recheck of `bitmapqualorig` removed (`InstrCountFiltered2`, "Rows Removed by Index Recheck"). One `BitmapCounts` per
//! bitmap recipe, shared by every clone of it (each rescan's source), read by entry run.rs through the registry per site
//! as an index-only walk's heap fetches are (D574): the count at completion less the count when the execution began.
//! Owner M12-HOSTS. Budget 200 lines.
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

use gravity_driver::{ArenaRef, SourceBox};
use gravity_program::SiteDesc;

use crate::ops::Recipe;

/// One bitmap recipe's counters, indexed by `NTIDS` / `EXACT` / `LOSSY` / `REMOVED` / `FILTERED`.
#[derive(Default, Debug)]
pub struct BitmapCounts([AtomicU64; 5]);

impl BitmapCounts {
    /// The tuples the index's `amgetbitmap` returned (a lossy AM counts what it reports: BRIN, pages x 10).
    pub const NTIDS: usize = 0;
    /// The heap pages the walk read whose bitmap entry was exact / lossy.
    pub const EXACT: usize = 1;
    pub const LOSSY: usize = 2;
    /// The rows of recheck pages the recheck qual rejected.
    pub const REMOVED: usize = 3;
    /// The rows the scan's own qual dropped in the walk (D2988's pre-filter), before the program's Filter sees them.
    pub const FILTERED: usize = 4;

    pub fn add(&self, which: usize, n: u64) {
        if n > 0 {
            self.0[which].fetch_add(n, Relaxed);
        }
    }

    /// The pages a heap walk counted since `at` (its exact, lossy counts before the call) up to `now`.
    pub fn pages(&self, at: (u64, u64), now: (u64, u64)) {
        self.add(Self::EXACT, now.0.saturating_sub(at.0));
        self.add(Self::LOSSY, now.1.saturating_sub(at.1));
    }

    pub fn load(&self) -> [u64; 5] {
        core::array::from_fn(|i| self.0[i].load(Relaxed))
    }
}

/// M12 derives no source of its own (its counters ride the M2 bitmap source's recipe).
pub fn derive(_: &Recipe, _: usize, _: &SiteDesc, _: ArenaRef) -> Option<SourceBox> {
    None
}

pub const SIZE: usize = 0;

#[cfg(test)]
mod tests {
    use super::BitmapCounts;

    #[test]
    fn counts_add_and_pages() {
        let c = BitmapCounts::default();
        c.add(BitmapCounts::NTIDS, 50);
        c.add(BitmapCounts::REMOVED, 0);
        c.pages((2, 1), (5, 1));
        c.add(BitmapCounts::REMOVED, 999);
        c.add(BitmapCounts::FILTERED, 7);
        assert_eq!(c.load(), [50, 3, 0, 999, 7]);
    }
}

/// PostgreSQL's standard_ExecutorEnd report of a statement's parallel workers (pg_stat_database's
/// parallel_workers_to_launch / parallel_workers_launched): a served statement whose plan has a Gather or Gather Merge
/// reports the workers they plan and the workers its sections ran.
pub fn report_parallel_workers(to_launch: i64, launched: i64) {
    if to_launch > 0 {
        pgstat::database::pgstat_update_parallel_workers_stats(to_launch, launched);
    }
}
