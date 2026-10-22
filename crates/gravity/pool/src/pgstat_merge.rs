//! A section's pgstat bank: the workers' counts per claim, folded into the leader's pending after the section (docs/gravity/parallel.md "The process pool", pgstat).

use core::sync::atomic::{AtomicU64, Ordering};

use pgsync::Mutex;
use types_core::instrument::{BufferUsage, WalUsage};

use pgstat::section::{discard_thread_deltas, fold_thread_deltas, take_thread_deltas, ThreadDeltas};

pub struct SectionStats {
    bank: Mutex<ThreadDeltas>,
    /// The claims' buffer block counts (bufmgr `counters`), the leader's pgBufferUsage after the section, as
    /// PostgreSQL's leader accumulates its workers' (InstrAccumParallelQuery): EXPLAIN's BUFFERS at any width.
    blocks: Mutex<bufmgr::counters::Blocks>,
    /// D4241: the claims' pgBufferUsage block I/O times (track_io_timing), the leader's after the section as their block
    /// counts are: EXPLAIN's I/O Timings at any width.
    times: Mutex<pgstat::io::BlockTimes>,
    /// D4240: the claims' WAL usage, the leader's pgWalUsage after the section, as InstrAccumParallelQuery adds its
    /// workers': EXPLAIN's WAL, pg_stat_wal and pg_stat_statements' wal_* at any width.
    wal: Mutex<WalUsage>,
    /// D4240: the end of the last WAL record a claim wrote (a hint bit's full-page image), the leader's XactLastRecEnd
    /// at least after the section, as ParallelWorkerReportLastRecEnd hands a worker's to its leader.
    rec_end: AtomicU64,
    /// D4116: the claims' temp-file counts (fd buffile `TempUsage`: a spill's BufFile I/O), folded the same way.
    temp: Mutex<fd::buffile::TempUsage>,
}

/// A claim's start on its worker (`SectionStats::claim_begin`): the thread's block counters, block I/O times and
/// temp-file counters then, which `claim_end` takes the claim's counts and times from; its WAL usage and last record
/// end (D4240).
pub struct ClaimStart {
    blocks: bufmgr::counters::Blocks,
    times: pgstat::io::BlockTimes,
    wal: WalUsage,
    rec_end: u64,
    temp: fd::buffile::TempUsage,
}

impl Default for SectionStats {
    fn default() -> SectionStats {
        SectionStats::new()
    }
}

impl SectionStats {
    pub const fn new() -> SectionStats {
        SectionStats {
            bank: Mutex::new(ThreadDeltas::empty()),
            blocks: Mutex::new([0; 9]),
            times: Mutex::new([0; 4]),
            wal: Mutex::new(WalUsage { wal_records: 0, wal_fpi: 0, wal_bytes: 0, wal_buffers_full: 0 }),
            rec_end: AtomicU64::new(0),
            temp: Mutex::new([0; 4]),
        }
    }

    /// On the worker before its claim: the thread's residue from work no statement reports is not this claim's; its
    /// buffer block counters, block I/O times and temp-file counters then, which `claim_end` takes the claim's from.
    pub fn claim_begin() -> ClaimStart {
        discard_thread_deltas();
        ClaimStart {
            blocks: bufmgr::counters::snapshot(),
            times: pgstat::io::block_times(),
            wal: transam_xlog::pgWalUsage(),
            rec_end: transam_xlog::XactLastRecEnd(),
            temp: fd::buffile::temp_snapshot(),
        }
    }

    pub fn claim_end(&self, start: &ClaimStart) {
        if let Some(d) = take_thread_deltas() {
            self.bank.lock().unwrap_or_else(|p| p.into_inner()).add(&d);
        }
        let t = fd::buffile::temp_take_since(&start.temp);
        if t.iter().any(|&n| n != 0) {
            let mut bank = self.temp.lock().unwrap_or_else(|p| p.into_inner());
            bank.iter_mut().zip(t.iter()).for_each(|(a, n)| *a = a.wrapping_add(*n));
        }
        let b = bufmgr::counters::take_since(&start.blocks);
        if b.iter().any(|&n| n != 0) {
            let mut bank = self.blocks.lock().unwrap_or_else(|p| p.into_inner());
            bank.iter_mut().zip(b.iter()).for_each(|(a, n)| *a = a.wrapping_add(*n));
        }
        let t = pgstat::io::take_block_times_since(&start.times);
        if t.iter().any(|&n| n != 0) {
            let mut bank = self.times.lock().unwrap_or_else(|p| p.into_inner());
            bank.iter_mut().zip(t.iter()).for_each(|(a, n)| *a = a.wrapping_add(*n));
        }
        let w = transam_xlog::take_wal_usage_since(&start.wal);
        if w != WalUsage::default() {
            instrument::wal_usage_add(&mut self.wal.lock().unwrap_or_else(|p| p.into_inner()), &w);
        }
        let end = transam_xlog::XactLastRecEnd();
        if end != start.rec_end {
            self.rec_end.fetch_max(end, Ordering::Relaxed);
        }
    }

    pub fn fold_into_leader(&self) {
        let d = core::mem::replace(&mut *self.bank.lock().unwrap_or_else(|p| p.into_inner()), ThreadDeltas::empty());
        if !d.is_empty() {
            fold_thread_deltas(&d);
        }
        let b = core::mem::replace(&mut *self.blocks.lock().unwrap_or_else(|p| p.into_inner()), [0; 9]);
        if b.iter().any(|&n| n != 0) {
            bufmgr::counters::add(&b);
        }
        let t = core::mem::replace(&mut *self.times.lock().unwrap_or_else(|p| p.into_inner()), [0; 4]);
        if t.iter().any(|&n| n != 0) {
            pgstat::io::add_block_times(&t);
        }
        let w = core::mem::take(&mut *self.wal.lock().unwrap_or_else(|p| p.into_inner()));
        if w != WalUsage::default() {
            instrument::instr_accum_parallel_query(&BufferUsage::default(), &w);
        }
        let end = self.rec_end.swap(0, Ordering::Relaxed);
        if end != 0 {
            transam_xlog::raise_xact_last_rec_end(end);
        }
        let t = core::mem::replace(&mut *self.temp.lock().unwrap_or_else(|p| p.into_inner()), [0; 4]);
        if t.iter().any(|&n| n != 0) {
            fd::buffile::temp_add(&t);
        }
    }
}
