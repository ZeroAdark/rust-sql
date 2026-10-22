//! D276 (main 09-24): PG's build-first hash join returns at an empty build before its outer child is ever fetched, so the
//! outer subtree's errors and effects never happen. A protocol that finds the build empty masks the probe's outer-side
//! producers (`Exec::run_empty`) from its `after_pipeline`; the runner then runs each as if no claim reached it: no section,
//! no claim, the finish and the seal as over a relation pruned whole, so the consumer reads an empty product.
//! Here: two counting producers over one relation into one emit (`p_two_slots`). After pipeline 0 the plan makes every read
//! of the relation fail. Unmasked, pipeline 1 raises that read's error at every width; masked, it never reads, the statement
//! completes and its slot counts 0. The mask's bit for pipeline 0 (already run) changes nothing. Gen ct-0097 (entry.rs
//! `run_pipeline`): a masked pipeline without a driving site (here the emit, 2) is no longer left alone: it opens and ends
//! at its first call without its program, so with its bit set too the statement completes with no row.

mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use common::*;
use gravity_driver::{Entry, Error, Exec, Grant, Merge, Result};
use gravity_parallel::{FixturePool, MorselSource, Plan};

/// The fixture plan, plus the step after pipeline 0: every read fails from now on, and `mask` (if any) runs empty.
struct AfterBuild<'a> {
    inner: FixturePlan,
    table: &'a FixtureTable,
    mask: u64,
}

impl Plan for AfterBuild<'_> {
    fn width_of(&self, p: usize) -> u16 {
        self.inner.width_of(p)
    }
    fn claims_for(&self, p: usize) -> &dyn MorselSource {
        self.inner.claims_for(p)
    }
    fn merge_for(&self, p: usize) -> &dyn Merge {
        self.inner.merge_for(p)
    }
    fn after_pipeline(&self, p: usize, exec: &mut Exec) -> Result<()> {
        if p == 0 {
            self.table.fail_block.store(0, Ordering::Relaxed);
            exec.run_empty(self.mask);
        }
        self.inner.after_pipeline(p, exec)
    }
}

#[test]
fn a_masked_producer_never_reads_and_its_consumer_reads_an_empty_product_at_every_width() {
    let table = FixtureTable::new(T_A, 120, 50, gen_a);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(p_two_slots(T_A, 120 * 50));
    let pool = FixturePool::new(8);
    let mut entry = Entry::new(2);
    let thr = 700u64;
    let want = table.rows().filter(|[a, _]| (*a as u64) < thr).count() as u64;
    assert!(want > 0);
    for w in WIDTHS {
        for claim in [3u64, 40] {
            // unmasked: pipeline 1 reads, and the read fails
            table.fresh();
            table.fail_block.store(u64::MAX, Ordering::Relaxed);
            let plan = AfterBuild { inner: FixturePlan::new(&program, &derive, w, claim), table: &table, mask: 0 };
            let mut sink = LegSink::new(true);
            let r = run_planned(&mut entry, &program, &mut derive, &pool, &plan, Grant::of(64 << 20), &[thr], &NO_ABORT, &mut sink);
            assert_eq!(r, Err(Error::host("range read failed")), "width {w} claim {claim}: the control raises");

            // masked: pipeline 1 runs empty, its count is 0, pipeline 0's stands
            table.fresh();
            table.fail_block.store(u64::MAX, Ordering::Relaxed);
            let plan = AfterBuild { inner: FixturePlan::new(&program, &derive, w, claim), table: &table, mask: 0b011 };
            let mut sink = LegSink::new(true);
            let n = run_planned(&mut entry, &program, &mut derive, &pool, &plan, Grant::of(64 << 20), &[thr], &NO_ABORT, &mut sink).expect("the masked producer never reads");
            assert_eq!(n, 1, "width {w} claim {claim}");
            assert_eq!(sink.sorted(), vec![vec![want, want, 0]], "width {w} claim {claim}: slot 1 empty, slot 0 whole");
            assert_eq!(table.ledger.twice(), 0, "width {w} claim {claim}: no block claimed by the masked producer");
            assert_eq!(table.pins.load(Ordering::Relaxed), 0, "width {w} claim {claim}");

            // the emit (no driving site) masked too: it ends at its first call without its program, no row and no read
            table.fresh();
            table.fail_block.store(u64::MAX, Ordering::Relaxed);
            let plan = AfterBuild { inner: FixturePlan::new(&program, &derive, w, claim), table: &table, mask: 0b111 };
            let mut sink = LegSink::new(true);
            let n = run_planned(&mut entry, &program, &mut derive, &pool, &plan, Grant::of(64 << 20), &[thr], &NO_ABORT, &mut sink).expect("the masked emit runs nothing");
            assert_eq!(n, 0, "width {w} claim {claim}: the masked emit hands no row");
            assert!(sink.sorted().is_empty(), "width {w} claim {claim}");
            assert_eq!(table.ledger.twice(), 0, "width {w} claim {claim}: no block claimed by the masked producer");
            assert_eq!(table.pins.load(Ordering::Relaxed), 0, "width {w} claim {claim}");
        }
    }
    table.fail_block.store(u64::MAX, Ordering::Relaxed);
}
