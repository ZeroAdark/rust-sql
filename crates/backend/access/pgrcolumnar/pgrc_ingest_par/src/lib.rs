//! # pgrc_ingest_par — the rtpool binding for pgrcolumnar parallel ingest
//! (chunk M3-I)
//!
//! The coordination + assembly ENGINE lives in [`pgrc_write::par`] (the
//! single-owner ingest handoff); this crate binds it to the rtpool: a
//! [`runtime::StreamSource`]-fed task set (the runtime's stream-fed morsel
//! source — its doc names parallel COPY's segmentator feed), the leader's
//! feeding session, and the ordered-commit driver. Split-crate shape is
//! FORCED by the dependency graph: since M3-H (#473), `pgrc_am` (table-AM
//! cone, below `runtime`) depends on `pgrc_write`, so the writer crate can
//! never depend on `runtime`; the binding sits above both, and the COPY
//! surface reaches it from executor altitude (declared M3-H follow-on).
//!
//! No new thread population (§7.1 law 6): everything runs on rtpool workers;
//! the leader only feeds and parks. The rtpool-only source pin lives in
//! `tests/runtime_binding.rs`.

use std::sync::Arc;

use pgrc_write::par::{ColChunk, ParEngine, ParIngestOpts, ParProviders, RowChunk};
use pgrc_write::seal::{SealReport, SealedPart};
use pgrc_write::writer::TableWriter;
use pgrc_write::{WriteError, WriteResult};
use pgrc_write::ingest::RawDatum;
use runtime::{
    CompletionWaiter, MorselRange, QuerySpec, RgOutcome, Runtime, StreamSource, TaskSetSpec,
    TaskSetWork,
};

struct IngestWork {
    engine: Arc<ParEngine>,
    /// D3922: the leader's memory account. The pool workers run the chunks on it, so what they allocate (the normalized
    /// chunks, the parts they seal) is the loading statement's, and the leader's frees of it net there.
    acct: memledger::Acct,
}

impl TaskSetWork for IngestWork {
    fn run_morsel(&self, worker: usize, range: MorselRange) {
        let _acct = memledger::charge::enter_as(memledger::charge::context_of(self.acct).0);
        for id in range {
            self.engine.run_chunk(worker, id);
        }
    }

    fn finalize(&self) {}
}

/// The leader's feeding face for one parallel COPY statement.
pub struct ParSession<'a> {
    engine: &'a Arc<ParEngine>,
    stream: &'a Arc<StreamSource>,
    rt: &'a Arc<Runtime>,
    cur: RowChunk,
    published: u64,
    ncols: usize,
}

impl ParSession<'_> {
    /// The engine's chunk grain (== the policy's cut granule, #560): the
    /// aligned-fast-path caller verifies its prebuilt chunks against THIS
    /// before handing any through `append_chunk`.
    pub fn chunk_rows(&self) -> u32 {
        self.engine.chunk_rows()
    }

    /// The engine's resolved bounds (the machine-derived open-parts bound
    /// included) — for the driver's trace line.
    pub fn opts(&self) -> &pgrc_write::par::ParIngestOpts {
        self.engine.opts()
    }

    /// (peak bytes the session held, memory-gate waits, ms gated) so far.
    pub fn memory_report(&self) -> (u64, u64, u64) {
        self.engine.memory_report()
    }

    /// CONVERT W3-2b: the loading statement's owner the session charges (the writer's): a feed that captures chunks
    /// itself (the text COPY helpers) captures them under it.
    pub fn owner(&self) -> Option<memledger::Owner> {
        self.engine.owner()
    }

    /// D3808: (bytes the session holds now, its budget), for a load's trace.
    pub fn memory_now(&self) -> (u64, u64) {
        self.engine.memory_now()
    }

    /// D3808: the bytes the load has planned but not yet made resident
    /// outside the engine (a parquet COPY's decode window: its charge less its
    /// live blocks); the engine's memory gate counts them as resident.
    pub fn set_ahead(&self, bytes: u64) {
        self.engine.set_ahead(bytes)
    }

    /// D4399: one non-blocking step of the engine's publish gate for `bytes`
    /// the leader plans to hold outside the engine (a text COPY's parse
    /// window), before it hands them to its helpers.
    pub fn ahead_step(&self, g: &mut pgrc_write::par::AheadGate, bytes: u64) -> pgrc_write::par::AheadStep {
        self.engine.ahead_step(g, bytes)
    }

    /// Append one row (COPY-shaped, same currency as the serial face). Rows
    /// are captured raw; chunk grain full ⇒ publish to the claim stream
    /// (may block on backpressure).
    pub fn append_row(&mut self, row: &[RawDatum<'_>]) -> WriteResult<()> {
        self.cur.push_row(row)?;
        if self.cur.rows() >= self.engine.chunk_rows() {
            self.flush()?;
        }
        Ok(())
    }

    /// Append one PREBUILT full-grain chunk (the parquet-parallel aligned
    /// fast path: decode workers build the chunk OFF the leader; the
    /// leader hands it through in input order — measured leader-bound at
    /// ~100% of one core pumping per-row without this face). Legal ONLY on
    /// a chunk boundary (no partial rows buffered) with exactly
    /// `chunk_rows` rows of the session's width — anything else is a
    /// Contract error: the caller's alignment invariant is structural
    /// (fragments always end on grain boundaries), never best-effort.
    /// The published-id order is identical to the row path's, so the #560
    /// partition law (chunk grain == cut granule) is preserved by
    /// construction.
    pub fn append_chunk(&mut self, chunk: RowChunk) -> WriteResult<()> {
        if self.cur.rows() != 0 {
            return Err(WriteError::Contract {
                detail: "append_chunk off a chunk boundary (partial rows buffered)",
            });
        }
        if chunk.rows() != self.engine.chunk_rows() || chunk.ncols() != self.ncols {
            return Err(WriteError::Contract {
                detail: "append_chunk shape mismatch (grain or width)",
            });
        }
        let id = self.published;
        self.engine.publish_chunk(chunk, id)?;
        self.published += 1;
        self.stream.publish(self.published);
        self.rt.notify_source_progress();
        Ok(())
    }

    /// [`Self::append_chunk`] for a full-grain chunk already in column
    /// form (built against [`Self::schema`]); the same boundary and shape
    /// contract, the same published-id order.
    pub fn append_col_chunk(&mut self, chunk: ColChunk) -> WriteResult<()> {
        if self.cur.rows() != 0 {
            return Err(WriteError::Contract {
                detail: "append_col_chunk off a chunk boundary (partial rows buffered)",
            });
        }
        if chunk.rows() != self.engine.chunk_rows() || chunk.ncols() != self.ncols {
            return Err(WriteError::Contract {
                detail: "append_col_chunk shape mismatch (grain or width)",
            });
        }
        let id = self.published;
        self.engine.publish_cols(chunk, id)?;
        self.published += 1;
        self.stream.publish(self.published);
        self.rt.notify_source_progress();
        Ok(())
    }

    /// The session's column schemas, in attribute order.
    pub fn schema(&self) -> &[pgrc_write::format::class::ColSchema] {
        self.engine.schema()
    }

    fn flush(&mut self) -> WriteResult<()> {
        if self.cur.rows() == 0 {
            return Ok(());
        }
        let full = std::mem::replace(
            &mut self.cur,
            RowChunk::with_owner(self.ncols, self.engine.chunk_rows(), self.engine.owner()),
        );
        let id = self.published;
        self.engine.publish_chunk(full, id)?;
        self.published += 1;
        // Publish-then-wake: the watermark advance makes the chunk claimable
        // (StreamSource is the runtime's Dekker-checked stream face).
        self.stream.publish(self.published);
        self.rt.notify_source_progress();
        Ok(())
    }
}

struct Drain<'a> {
    engine: &'a Arc<ParEngine>,
    stream: &'a Arc<StreamSource>,
    rt: &'a Arc<Runtime>,
    waiter: Option<CompletionWaiter>,
}

impl Drop for Drain<'_> {
    fn drop(&mut self) {
        if let Some(w) = self.waiter.take() {
            self.engine.fail(WriteError::Contract { detail: "parallel ingest leader unwound" });
            self.stream.close();
            self.rt.notify_source_progress();
            let _ = w.wait();
            let _ = self.engine.cleanup_temps();
        }
        self.engine.release_charge();
    }
}

/// Run one parallel ingest session on the rtpool over a prebuilt engine:
/// submit the stream-fed task set, feed rows through `feed`, drain, and
/// return the ordered seal results. On error the drain still completes and
/// every assigned temp is unlinked (exactly-old-or-new: a failed session
/// leaves nothing readable).
pub fn run_parallel<R>(
    rt: &Arc<Runtime>,
    engine: Arc<ParEngine>,
    feed: impl FnOnce(&mut ParSession<'_>) -> WriteResult<R>,
) -> WriteResult<(Vec<SealedPart>, Vec<SealReport>, R)> {
    // colload (k): the statement's publish fsyncs its parts on the runtime pool too.
    install_fsync_fanout();
    let stream = Arc::new(StreamSource::new());
    let (_rg, waiter): (runtime::RgHandle, CompletionWaiter) = rt.submit(QuerySpec {
        query_id: engine.fxid(),
        tasksets: vec![TaskSetSpec {
            source: Arc::clone(&stream) as Arc<dyn runtime::MorselSource>,
            work: Arc::new(IngestWork {
                engine: Arc::clone(&engine),
                acct: memledger::current(),
            }),
            deps: Vec::new(),
        }],
    });
    let ncols = engine.ncols();
    let mut sess = ParSession {
        engine: &engine,
        stream: &stream,
        rt,
        cur: RowChunk::with_owner(ncols, engine.chunk_rows(), engine.owner()),
        published: 0,
        ncols,
    };
    let mut drain = Drain { engine: &engine, stream: &stream, rt, waiter: Some(waiter) };
    // Feed; flush the tail; on ANY failure record it (first-wins) and fall
    // through to the drain — the stream must close regardless so the RG
    // completes and no task is left holding a claim.
    let fed: WriteResult<R> = feed(&mut sess).and_then(|r| {
        sess.flush()?;
        Ok(r)
    });
    let published = sess.published;
    match &fed {
        Ok(_) => {
            engine.close_input(published);
        }
        Err(e) => engine.fail(e.clone()),
    }
    stream.close();
    rt.notify_source_progress();
    let outcome = drain.waiter.take().expect("drained once").wait();
    drop(drain);
    let e: WriteError = match fed {
        Ok(r) => match engine.collect() {
            Ok(sealed) if outcome == RgOutcome::Completed => {
                let mut parts = Vec::with_capacity(sealed.len());
                let mut reports = Vec::with_capacity(sealed.len());
                for (p, rep) in sealed {
                    parts.push(p);
                    reports.push(rep);
                }
                return Ok((parts, reports, r));
            }
            Ok(_) => WriteError::Contract {
                detail: "parallel ingest resource group aborted",
            },
            Err(e) => e,
        },
        Err(e) => e,
    };
    // Error path: the drain is complete (waiter returned), so no task is
    // mid-file — unlink the assigned temps.
    engine.cleanup_temps()?;
    Err(e)
}

/// The whole-statement convenience over a [`TableWriter`]: build the engine
/// from the writer's facts, run the session, deposit the ordered results
/// (the self-checking `deposit_sealed_parts` face). On `Ok` the writer is
/// exactly where an equivalent serial statement would have left it;
/// `publish`/`abort`/registry lifecycle proceed UNCHANGED above the frozen
/// seal boundary.
pub fn parallel_ingest<R>(
    w: &mut TableWriter,
    rt: &Arc<Runtime>,
    providers: ParProviders,
    opts: ParIngestOpts,
    feed: impl FnOnce(&mut ParSession<'_>) -> WriteResult<R>,
) -> WriteResult<R> {
    let facts = w.par_facts()?;
    // v4 delta (TY-1): the writer's structural-election facts override the
    // provider default — the parallel seal must elect exactly as a serial
    // statement on the SAME writer would (byte-identical-parts law). The
    // facts side carries them since the v4 rebuild; a driver-supplied
    // posture that disagreed with the writer would silently split election
    // behavior across the serial/parallel arms.
    let mut providers = providers;
    providers.structural = facts.structural.clone();
    let engine = Arc::new(ParEngine::new(
        providers,
        facts.schema,
        facts.spec,
        facts.table_dir,
        facts.fxid,
        facts.policy,
        opts,
        facts.base_seq,
    )?
    // CONVERT W3-2b: the writer's statement owner (read once on this leader at the writer's open) charges the session
    .with_owner(w.owner()));
    let (parts, reports, r) = run_parallel(rt, engine, feed)?;
    w.deposit_sealed_parts(parts, reports)?;
    Ok(r)
}

/// A [`pgrc_write::seal::ColumnPool`] on the rtpool (F1 L6's pooled seal): the calls are one-unit morsels of one
/// task set and the caller parks until the group drains. A call an aborted group never ran runs on the caller.
pub struct RtColumnPool {
    rt: Arc<Runtime>,
    /// At most this many threads per call (0 = the whole pool): lane `l` takes items `l, l + lanes, …`.
    lanes: usize,
}

impl RtColumnPool {
    pub fn new(rt: Arc<Runtime>) -> RtColumnPool {
        RtColumnPool { rt, lanes: 0 }
    }

    pub fn with_lanes(self, lanes: usize) -> RtColumnPool {
        RtColumnPool { lanes, ..self }
    }

    /// The process pool, when the postmaster started one.
    pub fn global() -> Option<RtColumnPool> {
        runtime::global().map(|rt| RtColumnPool::new(Arc::clone(rt)))
    }
}

struct ColumnWork {
    /// Valid until `run` returns, which is after the group drained.
    f: *const (dyn Fn(usize) + Sync + 'static),
    lanes: usize,
    done: Vec<std::sync::atomic::AtomicBool>,
    panicked: std::sync::atomic::AtomicBool,
}

// SAFETY: `f` is `Sync` and outlives every call (see `RtColumnPool::run`).
unsafe impl Send for ColumnWork {}
unsafe impl Sync for ColumnWork {}

impl TaskSetWork for ColumnWork {
    fn run_morsel(&self, _worker: usize, range: MorselRange) {
        use std::sync::atomic::Ordering::{Relaxed, Release};
        for i in range.flat_map(|l| (l as usize..self.done.len()).step_by(self.lanes)) {
            // SAFETY: the caller of `run` is parked on the group, which has not drained.
            let f = unsafe { &*self.f };
            // unwind-ok: worker-containment
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(i))).is_err() {
                self.panicked.store(true, Relaxed);
            }
            self.done[i].store(true, Release);
        }
    }

    fn finalize(&self) {}
}

impl RtColumnPool {
    /// `f(i)` for every `i < n` on the pool while the caller runs `beside` (nothing, for `run`); the calls the group
    /// never ran run on the caller after it drained.
    fn run_group(&self, n: usize, f: &(dyn Fn(usize) + Sync), beside: &mut dyn FnMut()) {
        use std::sync::atomic::{AtomicBool, Ordering::{Acquire, Relaxed}};
        // SAFETY: lifetime erasure only; `f` is not called after `waiter.wait()` returns (the drain).
        let f: *const (dyn Fn(usize) + Sync + 'static) = unsafe { std::mem::transmute(f as *const (dyn Fn(usize) + Sync)) };
        let lanes = if self.lanes == 0 { n } else { self.lanes.min(n) };
        let work = Arc::new(ColumnWork { f, lanes, done: (0..n).map(|_| AtomicBool::new(false)).collect(), panicked: AtomicBool::new(false) });
        let (_rg, waiter) = self.rt.submit(QuerySpec {
            query_id: 0,
            tasksets: vec![TaskSetSpec {
                source: Arc::new(runtime::SyntheticMorselSource::with_boundaries(lanes as u64, 1).with_c0(1)),
                work: Arc::clone(&work) as Arc<dyn TaskSetWork>,
                deps: Vec::new(),
            }],
        });
        // The group borrows `f` until it drains: wait for it even when `beside` panics.
        // unwind-ok: log-then-die
        let caller = std::panic::catch_unwind(std::panic::AssertUnwindSafe(beside));
        waiter.wait();
        // SAFETY: as above; the group drained, so these are the only calls left.
        let f = unsafe { &*work.f };
        (0..n).filter(|&i| !work.done[i].load(Acquire)).for_each(f);
        if let Err(p) = caller {
            std::panic::resume_unwind(p);
        }
        assert!(!work.panicked.load(Relaxed), "pgrcolumnar: a pooled seal call panicked");
    }
}

impl pgrc_write::seal::ColumnPool for RtColumnPool {
    fn run(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        if n <= 1 || self.rt.nthreads() <= 1 {
            return (0..n).for_each(f);
        }
        self.run_group(n, f, &mut || {})
    }

    /// K1-MEM: the pool takes `f`'s calls (even one) while the caller runs `beside`.
    fn run_beside(&self, n: usize, f: &(dyn Fn(usize) + Sync), beside: &mut dyn FnMut()) {
        if n == 0 || self.rt.nthreads() <= 1 {
            (0..n).for_each(f);
            return beside();
        }
        self.run_group(n, f, beside)
    }
}

// colload (k): the publish's fsync fan-out on the runtime pool. pgrc_write cannot reach the runtime, so it calls
// through `pgrc_write::wvfs::FsyncFanout`, installed here by every parallel ingest. Each descriptor is one granule;
// a pool worker fsyncs inside a blocking-I/O section (its permit is donated for the wait, so a blocked worker does
// not hold a core), and the calling backend thread claims descriptors from the other end. Once it runs out it aborts
// the resource group (unclaimed morsels are dropped, so a busy pool never delays the commit) and waits for the drain.

const FSYNC_BATCH: usize = pgrc_write::wvfs::FSYNC_BATCH;

struct FsyncSet {
    n: usize,
    raws: [i32; FSYNC_BATCH],
    /// 0 unclaimed, 1 claimed: each descriptor is fsynced by exactly one thread.
    claimed: [std::sync::atomic::AtomicU8; FSYNC_BATCH],
    status: [std::sync::atomic::AtomicI32; FSYNC_BATCH],
}

impl FsyncSet {
    fn run(&self, i: usize, section: bool) {
        use std::sync::atomic::Ordering::SeqCst;
        if i >= self.n || self.claimed[i].swap(1, SeqCst) != 0 {
            return;
        }
        let st = if section {
            let _io = runtime::blocking_io_section();
            pgrc_write::wvfs::fsync_counted(self.raws[i])
        } else {
            pgrc_write::wvfs::fsync_counted(self.raws[i])
        };
        self.status[i].store(st, SeqCst);
    }
}

struct FsyncSource(u64);

impl runtime::MorselSource for FsyncSource {
    fn total_granules(&self) -> u64 {
        self.0
    }

    fn startup_c0(&self) -> u64 {
        1
    }
}

struct FsyncWork(Arc<FsyncSet>);

impl TaskSetWork for FsyncWork {
    fn run_morsel(&self, _worker: usize, range: MorselRange) {
        for i in range {
            self.0.run(i as usize, true);
        }
    }

    fn finalize(&self) {}
}

fn fsync_fanout(raws: &[i32], status: &mut [i32]) {
    use std::sync::atomic::{AtomicI32, AtomicU8, Ordering::SeqCst};
    let n = raws.len().min(FSYNC_BATCH).min(status.len());
    let Some(rt) = runtime::global() else { return };
    if n < 2 {
        return;
    }
    let mut set = FsyncSet {
        n,
        raws: [0; FSYNC_BATCH],
        claimed: [const { AtomicU8::new(0) }; FSYNC_BATCH],
        status: [const { AtomicI32::new(pgrc_write::wvfs::FSYNC_NOT_RUN) }; FSYNC_BATCH],
    };
    set.raws[..n].copy_from_slice(&raws[..n]);
    let set = Arc::new(set);
    let (rg, waiter) = rt.submit(QuerySpec {
        query_id: 0,
        tasksets: vec![TaskSetSpec {
            source: Arc::new(FsyncSource(n as u64)) as Arc<dyn runtime::MorselSource>,
            work: Arc::new(FsyncWork(Arc::clone(&set))),
            deps: Vec::new(),
        }],
    });
    rt.notify_source_progress();
    for i in (0..n).rev() {
        set.run(i, false);
    }
    rg.abort();
    let _ = waiter.wait();
    for (s, a) in status[..n].iter_mut().zip(&set.status[..n]) {
        *s = a.load(SeqCst);
    }
}

/// colload (k): install the runtime-pool fsync fan-out for the pgrcolumnar publish (idempotent).
pub fn install_fsync_fanout() {
    pgrc_write::wvfs::install_fsync_fanout(fsync_fanout);
}
