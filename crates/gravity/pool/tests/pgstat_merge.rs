use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use gravity_abi::MorselRange;
use gravity_parallel::{BlockRange, ClaimSize, Job, MorselSource, Pool};
use gravity_pool::{Ambient, RuntimePool};
use pgstat::io::{pgstat_count_io_op, pgstat_count_io_op_time, pgstat_have_pending_io, pgstat_pending_io, pgstat_prepare_io_time, shared_blk_read_time, IOContext, IOObject, IOOp};
use pgstat::relation::{find_tabstat_entry, pgstat_count_buffer_reads};
use pgstat::section::{discard_thread_deltas, fold_thread_deltas, take_thread_deltas, ThreadDeltas};
use pgstat::slru::{pgstat_count_slru_page_hit, pgstat_slru_pending};
use runtime::{Runtime, RuntimeConfig, WorkerPool};
use types_core::instrument::WalUsage;
use types_core::Oid;

const REL: usize = IOObject::Relation as usize;
const NORMAL: usize = IOContext::IOCONTEXT_NORMAL as usize;
const READ: usize = IOOp::Read as usize;
const HIT: usize = IOOp::Hit as usize;

fn pool(threads: usize) -> (WorkerPool, RuntimePool) {
    let rt = Runtime::new(RuntimeConfig::new(threads));
    let workers = WorkerPool::spawn_std(Arc::clone(&rt)).expect("pool threads");
    let pool = RuntimePool::new(Arc::clone(&rt), Ambient::HARNESS);
    (workers, pool)
}

fn count_blocks(range: MorselRange, reads: u32, hits: u32) {
    for _ in range.lo..range.hi {
        pgstat_count_io_op(IOObject::Relation, IOContext::IOCONTEXT_NORMAL, IOOp::Read, reads, 0);
        pgstat_count_io_op(IOObject::Relation, IOContext::IOCONTEXT_NORMAL, IOOp::Hit, hits, 0);
    }
}

#[test]
fn a_sections_worker_io_lands_in_the_leaders_pending_as_its_own() {
    discard_thread_deltas();
    let (workers, pool) = pool(4);
    let claims = BlockRange::new(64, ClaimSize::Fixed(1));
    let ran = AtomicU64::new(0);
    let job: Box<Job<'_>> = Box::new(|_ordinal, range| {
        assert!(!pgstat_have_pending_io(), "a claim starts with nothing pending on its thread");
        count_blocks(range, 1, 2);
        ran.fetch_add(1, Ordering::Relaxed);
        Ok(())
    });
    pool.run(4, &claims, &job).expect("section");
    assert!((1..=64).contains(&ran.load(Ordering::Relaxed)), "the runtime's sizer cuts the claims");
    let io = pgstat_pending_io();
    assert!(pgstat_have_pending_io());
    assert_eq!(io.counts[REL][NORMAL][READ], 64);
    assert_eq!(io.counts[REL][NORMAL][HIT], 128);

    claims.reset();
    pool.run(2, &claims, &job).expect("second section");
    let io = pgstat_pending_io();
    assert_eq!(io.counts[REL][NORMAL][READ], 128);
    assert_eq!(io.counts[REL][NORMAL][HIT], 256);
    discard_thread_deltas();
    assert!(!pgstat_have_pending_io());
    workers.shutdown();
}

#[test]
fn a_workers_relation_counts_fold_under_the_relations_key() {
    const REL_OID: Oid = 4_000_101;
    discard_thread_deltas();
    let (workers, pool) = pool(3);
    let claims = BlockRange::new(30, ClaimSize::Fixed(2));
    let job: Box<Job<'_>> = Box::new(|_ordinal, range| {
        for _ in range.lo..range.hi {
            pgstat_count_buffer_reads(REL_OID, false, 2, 1);
        }
        Ok(())
    });
    pool.run(3, &claims, &job).expect("section");
    let counts = find_tabstat_entry(REL_OID).expect("the leader's pending entry");
    assert_eq!(counts.blocks_fetched, 60);
    assert_eq!(counts.blocks_hit, 30);
    discard_thread_deltas();
    assert!(find_tabstat_entry(REL_OID).is_none());
    workers.shutdown();
}

#[test]
fn deltas_add_and_fold_as_values() {
    discard_thread_deltas();
    let mut bank = ThreadDeltas::empty();
    assert!(bank.is_empty());
    count_blocks(MorselRange::new(0, 3), 1, 0);
    let a = take_thread_deltas().expect("pending");
    assert!(!pgstat_have_pending_io());
    count_blocks(MorselRange::new(0, 5), 0, 1);
    let b = take_thread_deltas().expect("pending");
    bank.add(&a);
    bank.add(&b);
    assert_eq!(bank.io.counts[REL][NORMAL][READ], 3);
    assert_eq!(bank.io.counts[REL][NORMAL][HIT], 5);
    fold_thread_deltas(&bank);
    let io = pgstat_pending_io();
    assert_eq!((io.counts[REL][NORMAL][READ], io.counts[REL][NORMAL][HIT]), (3, 5));
    assert!(take_thread_deltas().is_some());
    assert!(take_thread_deltas().is_none());
}

/// D4241: a claim's track_io_timing time and its SLRU counts are its leader's after the section, as a PostgreSQL
/// parallel worker's are its leader's (InstrAccumParallelQuery; the worker's own pgstat flush): pgBufferUsage's shared
/// read time (EXPLAIN's I/O Timings), the pending database block read time (pg_stat_database.blk_read_time) and the
/// pending SLRU counts (pg_stat_slru). On trunk 5b9d474089d all stay on the pool thread, which never reports.
#[test]
fn a_workers_io_time_and_slru_counts_reach_the_leader() {
    const SLRU: i32 = 6; // "transaction" (pg_xact)
    const TOOK_NS: i64 = 2_000_000;
    discard_thread_deltas();
    let (workers, pool) = pool(3);
    let claims = BlockRange::new(12, ClaimSize::Fixed(1));
    let blocks = AtomicU64::new(0);
    let read0 = shared_blk_read_time();
    let hit0 = pgstat_slru_pending(SLRU as usize).blocks_hit;
    let job: Box<Job<'_>> = Box::new(|_ordinal, range| {
        for _ in range.lo..range.hi {
            // a read that took TOOK_NS under track_io_timing: its start stamped that long ago
            let start = pgstat_prepare_io_time(true) - TOOK_NS;
            pgstat_count_io_op_time(IOObject::Relation, IOContext::IOCONTEXT_NORMAL, IOOp::Read, start, 1, 8192);
            pgstat_count_slru_page_hit(SLRU);
            blocks.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    });
    pool.run(3, &claims, &job).expect("section");
    let n = blocks.load(Ordering::Relaxed) as i64;
    assert_eq!(n, 12, "every block of the range ran once");
    let took = shared_blk_read_time() - read0;
    assert!(took >= n * TOOK_NS, "the claims' read time is the leader's pgBufferUsage after the section: {took} ns of at least {} ns", n * TOOK_NS);
    assert_eq!(pgstat_slru_pending(SLRU as usize).blocks_hit - hit0, n, "the claims' SLRU hits are the leader's pending after the section");
    let (read_us, _) = pgstat::database::take_block_time_pending();
    assert!(read_us >= n * TOOK_NS / 1000, "the claims' read time is the leader's pending database block read time: {read_us} us of at least {} us", n * TOOK_NS / 1000);
    discard_thread_deltas();
    workers.shutdown();
}

/// D4240: a claim's WAL usage and the end of its last WAL record are its leader's after the section, as a PostgreSQL
/// parallel worker hands them over (InstrAccumParallelQuery's WalUsageAdd, ParallelWorkerReportLastRecEnd): EXPLAIN's WAL,
/// pg_stat_wal, and the record end the leader's commit flushes to. On trunk both stay on the pool thread.
#[test]
fn a_workers_wal_usage_and_last_record_end_reach_the_leader() {
    static XLOG: std::sync::Once = std::sync::Once::new();
    XLOG.call_once(transam_xlog::init_seams);
    discard_thread_deltas();
    let (workers, pool) = pool(3);
    let claims = BlockRange::new(12, ClaimSize::Fixed(1));
    let (wal0, end0) = (transam_xlog::pgWalUsage(), transam_xlog::XactLastRecEnd());
    let base = end0 + (1 << 20);
    let blocks = AtomicU64::new(0);
    let job: Box<Job<'_>> = Box::new(|_ordinal, range| {
        for b in range.lo..range.hi {
            // a hint bit's full-page image: one record with its image, and the record's end
            transam_xlog_seams::wal_usage_add::call(&WalUsage { wal_records: 1, wal_fpi: 1, wal_bytes: 8192, wal_buffers_full: 0 });
            transam_xlog_seams::set_xact_last_rec_end::call(base + (b + 1) * 8192);
            blocks.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    });
    pool.run(3, &claims, &job).expect("section");
    assert_eq!(blocks.load(Ordering::Relaxed), 12, "every block of the range ran once");
    let wal = transam_xlog::pgWalUsage();
    let took = (wal.wal_records - wal0.wal_records, wal.wal_fpi - wal0.wal_fpi, wal.wal_bytes.wrapping_sub(wal0.wal_bytes));
    assert_eq!(took, (12, 12, 12 * 8192), "the claims' WAL usage is the leader's pgWalUsage after the section");
    assert_eq!(transam_xlog::XactLastRecEnd(), base + 12 * 8192, "the leader's XactLastRecEnd reaches the claims' last record end");
    transam_xlog_seams::set_xact_last_rec_end::call(end0);
    discard_thread_deltas();
    workers.shutdown();
}
