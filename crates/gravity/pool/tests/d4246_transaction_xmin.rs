//! D4246: a claim's visibility checks (snapmgr XidInMVCCSnapshot on a suboverflowed snapshot) walk pg_subtrans through
//! SubTransGetTopmostTransaction, which stops at TransactionXmin because pg_subtrans below it may be truncated. A claim
//! must run under its leader's TransactionXmin, as PostgreSQL's parallel worker restores it (RestoreTransactionSnapshot
//! -> SetTransactionSnapshot), not under its pool thread's default or a stale one. On trunk a claim reads its pool thread's
//! own (FirstNormalTransactionId).

use std::sync::{Arc, Mutex};

use gravity_parallel::{BlockRange, ClaimSize, Job, Pool};
use gravity_pool::{Ambient, RuntimePool};
use runtime::{Runtime, RuntimeConfig, WorkerPool};

fn xmins_seen(pool: &RuntimePool, width: u16) -> Vec<u32> {
    let claims = BlockRange::new(16, ClaimSize::Fixed(1));
    let seen = Mutex::new(Vec::new());
    let job: Box<Job<'_>> = Box::new(|_ordinal, _range| {
        seen.lock().unwrap().push(procarray::TransactionXmin());
        Ok(())
    });
    pool.run(width, &claims, &job).expect("section");
    drop(job);
    seen.into_inner().unwrap()
}

#[test]
fn a_claim_runs_under_its_leaders_transaction_xmin() {
    let rt = Runtime::new(RuntimeConfig::new(3));
    let workers = WorkerPool::spawn_std(Arc::clone(&rt)).expect("pool threads");
    let pool = RuntimePool::new(Arc::clone(&rt), Ambient::HARNESS);
    let own = procarray::TransactionXmin();

    for leader in [4_000_123, 4_000_200, 4_000_150] {
        procarray::set_transaction_xmin(leader);
        let seen = xmins_seen(&pool, 3);
        assert!(!seen.is_empty(), "the section ran claims");
        assert!(seen.iter().all(|&x| x == leader), "every claim reads its leader's TransactionXmin {leader}: {seen:?}");
    }

    procarray::set_transaction_xmin(own);
    workers.shutdown();
}
