use std::sync::Arc;

use gravity_parallel::{BlockRange, ClaimSize, Job, Pool};
use gravity_pool::{Ambient, RuntimePool};
use runtime::{Runtime, RuntimeConfig, WorkerPool};

fn pool(threads: usize) -> (WorkerPool, RuntimePool) {
    let rt = Runtime::new(RuntimeConfig::new(threads));
    let workers = WorkerPool::spawn_std(Arc::clone(&rt)).expect("pool threads");
    let pool = RuntimePool::new(Arc::clone(&rt), Ambient::HARNESS);
    (workers, pool)
}

// D4116: a claim's temp-file I/O (a spill's BufFile blocks and times) reaches the section's leader, as PostgreSQL's
// leader accumulates its workers' BufferUsage (InstrAccumParallelQuery), so EXPLAIN (ANALYZE, BUFFERS) and
// pg_stat_statements count a W8 spill as a W0 one.
#[test]
fn a_claims_temp_blocks_land_in_the_leaders_counters() {
    let (workers, pool) = pool(4);
    let claims = BlockRange::new(32, ClaimSize::Fixed(1));
    let job: Box<Job<'_>> = Box::new(|_ordinal, range| {
        for _ in range.lo..range.hi {
            fd::buffile::temp_add(&[1, 2, 3, 4]);
        }
        Ok(())
    });
    let before = fd::buffile::temp_snapshot();
    pool.run(4, &claims, &job).expect("section");
    let after = fd::buffile::temp_snapshot();
    let got: [i64; 4] = core::array::from_fn(|i| after[i] - before[i]);
    assert_eq!(got, [32, 64, 96, 128], "the leader counts every claim's temp blocks and times");
    workers.shutdown();
}
