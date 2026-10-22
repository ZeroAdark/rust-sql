//! Parallel ingest (chunk M3-I): ordered-commit parallel COPY on rtpool.
//!
//! ## The design (charter `pgrcolumnar-v2.md` §1 writer disciplines;
//! `lanev3-m3-chunks.md` §1.9 + §2/§5 M3-I)
//!
//! ONE seal implementation shared by serial and parallel ingest —
//! [`crate::seal::seal_part`] is the FROZEN boundary. Parallelism lives
//! entirely ABOVE it:
//!
//! 1. **Capture (leader).** The COPY leader copies incoming rows into
//!    [`RowChunk`]s — raw datum images, NO normalization (detoast is worker
//!    work). Chunk grain is fixed ([`ParIngestOpts::chunk_rows`], a multiple
//!    of 64 so chunk seams fall on validity-word boundaries).
//! 2. **Claim (the claim plane).** Chunks are MORSELS: deterministic
//!    fixed-grain units whose boundaries are a pure function of the input
//!    (never adaptive/arrival-dependent), published through a growing
//!    watermark and claimed by workers through the SHARED
//!    [`pgrc_claim::ClaimCursor`] — the minimal XC-1/XC-2 substrate of the
//!    frozen parallel contract, of which parallel COPY is consumer #1 (the
//!    ruled COPY-on-morsels amendment: COPY is scheduled the same as the
//!    rest of the queries; the M3-L3 reader is consumer #2 on the same
//!    face, and the part-builder below is a SINK OPERATOR any pipeline can
//!    feed — CTAS/INSERT-SELECT land on it without a second path). Claim
//!    advance is the cursor's CAS; NO new thread population exists (§7.1
//!    law 6): work runs on pool workers, the leader only feeds and parks
//!    (the rtpool binding drives [`ParEngine::run_worker`]).
//! 3. **Normalize (workers).** A claimed chunk is normalized (detoast law:
//!    1B/4B-U/4B-C-pglz, external via the passed capability) into chunk-local
//!    [`ColBuffer`]s by the SAME `append_*` calls serial ingest uses.
//! 4. **Cut (in input order, deterministic).** A cut cursor consumes
//!    normalized chunk STATS strictly in input order and closes a part when
//!    the shared [`PartCutPolicy::should_cut`] predicate trips at a chunk
//!    boundary — a pure function of the input stream (schedule- and
//!    dop-independent), so the part partition is deterministic. Because the
//!    chunk grain IS the policy's cut granule and the serial writer asks the
//!    same predicate at the same granularity, the partition coincides with
//!    the serial writer's EXACTLY — under the byte budget as well as the row
//!    budget (M3-I; before the granule gate the byte-budget case legally
//!    diverged and only O-10 logical identity held). Whichever task advances the
//!    cursor over a closing boundary OWNS the closed part (the
//!    decrement-to-zero shape: one lock-section decides readiness AND
//!    admissibility; no task ever waits for another task).
//! 5. **Assemble + seal (workers, parallel across parts).** The owning task
//!    replays the part's chunks IN INPUT ORDER through
//!    [`ColBuffer::splice_chunk`] — the seam-replayed trackers (constancy,
//!    logical multiset hash, counters) — producing part-level `ColBuffer`s
//!    bit-identical to serial accumulation of the same rows, then calls the
//!    frozen `seal_part` with a pre-assigned seq. Byte-identical-parts law:
//!    `seal_part` is deterministic in (ColBuffers, spec, fxid, seq), and the
//!    merged ColBuffers are a pure function of the partition — so ANY claim
//!    schedule yields byte-identical part files (§1.9; witnessed by the
//!    permuted-schedule suite).
//! 6. **Ordered commit (leader).** Sealed parts gather by seq; the ordered
//!    seq list deposits into the [`crate::writer::TableWriter`], and publish
//!    stays the serial path — strictly above the frozen boundary.
//! 7. **WAL (leader, decision W0-3).** rtpool workers have no WAL insert
//!    state, so a logged session's workers seal through a
//!    [`crate::walsink::ForwardSink`] that keeps the part's chunks; the leader
//!    logs each finished part whenever it feeds or waits, and a part counts
//!    against `max_parts_in_flight` until it is logged, which bounds the
//!    bytes held.
//!
//! ## Coordination state (the claim-channel discipline)
//!
//! All shared coordination state lives in ONE `pgsync::Mutex` ([`Coord`])
//! with a single Condvar on which ONLY THE LEADER ever waits (backpressure +
//! drain). Worker tasks never block on other TASKS — run-to-completion, so
//! the pool cannot wedge (the GL-GANGWEDGE class is structurally excluded).
//! The one sanctioned worker wait is the WAKE FACE (FIX-B, the
//! ingest-contention charter): a claim-STARVED worker parks on an
//! eventcount keyed to publish progress ([`ParEngine::park_wake`]) instead
//! of yield-polling the watermark; every wake source is leader/pump
//! progress or cancellation (publish, close, cancel, driver poke) — never
//! another worker — so the no-wedge argument is unchanged. The chunk claim
//! channel itself is the runtime scheduler's claim word; this module adds
//! no bare-atomic channel. The same faces run under loom (`tests/loom.rs`
//! — real faces, real fences, never mirror models; the parked-claimer
//! wake protocol is models 3–5).
//!
//! ## Teardown / byref enumeration (§7.1 law 2)
//!
//! On ANY failure (feed error, worker normalize error, seal error, vfs
//! crash): the first error is recorded (first-wins under the lock), the
//! session is cancelled, remaining claims drain as no-ops, the leader waits
//! for the runtime drain (no writer can be mid-file afterwards), then unlinks
//! every assigned part temp by name (`tmp-<fxid>-<seq>`, ENOENT-tolerant).
//! Stranded state classes at teardown: captured `RowChunk`s and normalized
//! chunk `ColBuffer`s (owned, dropped with the engine — no byref borrows
//! cross the session boundary; every datum was copied at capture), sealed
//! temp FILES (unlinked here; after kill -9 the recovery scan reclaims them
//! by name — the same `tmp-` discipline as serial, proven in the crash
//! matrix at dop>1). Nothing else escapes: providers hand out per-task
//! instances that die with the task.
//!
//! ## What this module deliberately does NOT do
//!
//! - No per-work-item ceremony (§7.1 law 4): per-CHUNK lane instantiation is
//!   a scratch Vec + one Box per 65k-row chunk — nothing RG/GUC/plan-shaped.
//! - No session/txn state: fxid, providers, policy all arrive as passed
//!   capabilities (the crate law). No thread-locals (census pinned).
//! - No new thread population and no std thread spawn anywhere in this
//!   module — the rtpool-only assertion pins this structurally (source-scan
//!   test in the `pgrc_ingest_par` binding crate).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use pgrc_claim::{ClaimCursor, ClaimOutcome, MorselSource, NoObserver};
use pgsync::{lock, Condvar, Mutex};

use pgrc_format::abi::ColumnMetaBuilder;
use pgrc_format::class::{ColSchema, StorageClass};
use pgrc_format::dirlayout::temp_file_name;
use pgrc_format::relopt::ShredOptions;

use crate::elect::CandidateSource;
use crate::held::HeldCharge;
use crate::ingest::{
    normalize_varlena, ColBuffer, DictEntries, ExternalDetoast, NoExternalDetoast, RawDatum,
    DICT_ROW_PLAIN,
};
use crate::seal::{seal_part, PartSpec, SealReport, SealedPart, VerifyResolver};
use crate::shred::{validate_lanes, NoShred, ShredLane, ShredLaneSource};
use crate::plan::{apply_reclaim, dir_loc};
use crate::structural::StructuralPolicy;
use crate::walsink::{log_forwarded, ForwardSink, Forwarded, LoggedVfs, NoWal, WalSink};
use crate::writer::PartCutPolicy;
use crate::wvfs::{MemVfs, RealVfs, WriteVfs, WvFd};
use crate::{WriteError, WriteResult};
use pgrc_format::wal::ReclaimRec;

/// Default capture-chunk grain — pinned to the cut granule.
///
/// The cut cursor closes a part only at a chunk boundary, so the chunk
/// grain MUST equal [`PartCutPolicy::cut_granule_rows`]: a chunk coarser
/// than the granule leaves the serial writer's partition unreachable, which
/// is exactly the byte-budget divergence M3-I exists to close.
/// [`ParEngine::new`] enforces the equality. Must be a multiple of 64
/// (validity-word seam law); 8,192 is.
///
/// This is 8x finer than the previous 65,536 (one band). Coordination cost
/// scales with chunk count — one lock section per chunk, so 12,207 chunks
/// at 100M rows instead of 1,526 — which is negligible against per-chunk
/// normalize+seal work.
pub const DEFAULT_CHUNK_ROWS: u32 = crate::writer::DEFAULT_CUT_GRANULE_ROWS;

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// Parallel-session bounds. Chunk grain is FIXED per session — the part
/// partition is a pure function of (input stream, chunk_rows, policy),
/// independent of dop and claim schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParIngestOpts {
    /// Rows per capture chunk (% 64 == 0, > 0).
    pub chunk_rows: u32,
    /// Leader backpressure: captured-but-unclaimed chunk bound.
    pub max_chunks_in_flight: usize,
    /// Leader backpressure: cut-but-unsealed part bound (memory ceiling —
    /// each open part holds its whole-part buffers, exactly as serial does
    /// for its one part).
    ///
    /// This bound carries TWO roles (the ingest-contention charter's §5
    /// FIX-A finding): it is the open-part MEMORY ceiling and,
    /// accidentally, the seal CONCURRENCY cap — seal is single-threaded
    /// per part, so `max_parts_in_flight` slots ÷ seal cpu-s/part is a
    /// hard parts/s ceiling on the whole pipeline (measured: 8 slots ÷
    /// ~2.0 cpu-s = the 4.0 parts/s wall at 100m). Sessions with a real
    /// byte budget derive it via [`ParIngestOpts::parts_bound_for_budget`]
    /// so memory is bounded by admission arithmetic and seal parallelism
    /// is never capped below the worker count; the default stays the
    /// legacy 8 (the conservative posture for undirected sessions).
    pub max_parts_in_flight: usize,
}

/// The legacy open-parts bound (the glibc-retention-era sizing) — the
/// floor of the derived bound: no platform regresses below the posture
/// every shipped spec ran safely.
pub const LEGACY_MAX_PARTS_IN_FLIGHT: usize = 8;

/// `max_parts_in_flight` sentinel: derive the bound from the machine at
/// engine construction ([`machine_parts_in_flight`]). The lane-metal
/// attribution (`parquet-load.md` §12.7): the constant 8 left a 192-thread
/// box 91–95 % idle for the whole 100 M COPY — eight open parts ≈ 14 busy
/// cores at every decode window. `Default` keeps the legacy constant (the
/// posture every test and undirected session ran); the parquet driver asks
/// for [`ParIngestOpts::auto`].
pub const PARTS_IN_FLIGHT_AUTO: usize = usize::MAX;

/// Explicit override of the derived bound (a positive integer; anything
/// else is ignored with the derivation taking over).
pub const PARTS_IN_FLIGHT_ENV: &str = "PGRUST_PGRC_PARTS_IN_FLIGHT";

/// The open-parts bound for `cores` and `headroom_bytes` (pure): an open
/// part's seal is single-threaded, so one open part per core lets every core
/// seal, and each open part holds ≈ `2 × max_bytes` of memory (the
/// `parts_bound_for_budget` operand), so the bound is
/// `min(cores, headroom / (2 × max_bytes))`, floored at
/// [`LEGACY_MAX_PARTS_IN_FLIGHT`]. `cores / 2` (until 2026-10-08) left a
/// 16-core box at the legacy 8, where a hot-cache parallel text COPY of
/// h/10 lineitem is seal-bound: 16 open parts took 24 % off its wall for
/// +3.3 GB (lane pcopy, s5hot-20261008T0811Z). The memory term still caps
/// it on a small box; the memory broker owns this trade later.
pub fn parts_in_flight_for(cores: usize, headroom_bytes: u64, max_bytes: u64) -> usize {
    let per_part = max_bytes.saturating_mul(2).max(1);
    let by_mem = (headroom_bytes / per_part).min(usize::MAX as u64) as usize;
    let by_cores = cores;
    by_cores.min(by_mem).max(LEGACY_MAX_PARTS_IN_FLIGHT)
}

/// The CPUs this process may run on: `Cpus_allowed_list` from
/// `/proc/self/status` (a cpuset or `taskset` narrows it — the bound must
/// not count cores the workers cannot use), else the online list. Read from
/// procfs, not the standard library's thread module (this file's thread
/// population is the runtime's, pinned by `rtpool_only_no_new_thread_population`).
fn machine_cores() -> Option<usize> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?; // alloc: Cold
    let list = status
        .lines()
        .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))
        .map(str::trim)
        .map(str::to_string)
        .or_else(|| std::fs::read_to_string("/sys/devices/system/cpu/online").ok())?;
    cpu_list_len(list.trim())
}

/// The number of CPUs in a kernel cpu list (`0-3,8,10-11` → 7); `None` on a
/// malformed list.
pub fn cpu_list_len(list: &str) -> Option<usize> {
    let mut n = 0usize;
    for range in list.split(',').map(str::trim).filter(|r| !r.is_empty()) {
        n += match range.split_once('-') {
            Some((a, b)) => b.trim().parse::<usize>().ok()?.checked_sub(a.trim().parse::<usize>().ok()?)? + 1,
            None => {
                range.parse::<usize>().ok()?;
                1
            }
        };
    }
    (n > 0).then_some(n)
}

/// The captured-chunk bound that keeps `parts` open parts fed: the leader
/// parks while `captured.len() >= max_chunks_in_flight`, so with the legacy
/// 128 and 64 open parts each part sees two chunks of read-ahead and the
/// workers starve (the metal's parts64 arm: 19 busy cores, 102 s, against
/// parts32's 31 cores, 54 s). Sixteen chunks per open part, never below the
/// legacy 128 (the ≤ 8-part machines keep their posture).
pub fn chunks_for_parts(parts: usize) -> usize {
    parts.saturating_mul(16).max(128)
}

/// `MemAvailable` from `/proc/meminfo` (Linux), halved: open parts may take
/// at most half of what the machine has free after shared buffers and the
/// decode window are up. D3343: under the governor's published memory limit
/// (a container's memory.max), at most half of what the limit leaves past the
/// process's governed use now. D3808: and at most the open parts' share of the
/// load's room under the memory policy (`mempolicy::load::load_parts` of
/// [`load_room_at`] at the resident bytes read now: what loads already hold in
/// flight comes off, as a decode window is charged before its blocks are
/// up). The governor's projection alone is no measure of use with the
/// governor off (its bar 0, the default): it then counts only the context
/// growth since its last sample, so this half and the decode window's half
/// were each half of the bare limit. `None` where there is no such signal.
fn machine_headroom_bytes() -> Option<u64> {
    let avail = std::fs::read_to_string("/proc/meminfo").ok().and_then(|s| {
        s.lines().find_map(|l| {
            l.strip_prefix("MemAvailable:")?.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()
        })
    }); // alloc: Cold
    let limit = mcx::governor::limit() as u64;
    let governed = (limit != 0).then(|| limit.saturating_sub(mcx::governor::projected() as u64) / 2);
    let room = mcx::governor::probe_resident()
        .map(|r| mempolicy::load::load_parts(load_room_at(r as u64, dirty_now()), memledger::LOAD_IN_FLIGHT.bytes()))
        .filter(|&r| r != u64::MAX);
    [avail.map(|kb| kb * 1024 / 2), governed, room].into_iter().flatten().min()
}

/// D3808: the bytes a load may still take in flight with `resident` bytes
/// resident, `dirty` of them dirty or writeback page cache
/// (`mempolicy::load::load_room` over the limit and bar the governor last
/// published; `u64::MAX` with neither known).
fn load_room_at(resident: u64, dirty: u64) -> u64 {
    mempolicy::load::load_room(mcx::governor::limit() as u64, mcx::governor::bar() as u64, resident, dirty)
}

/// D3808: the dirty + writeback page-cache bytes the resident probe counts,
/// read now (0 with no probe).
fn dirty_now() -> u64 {
    mcx::governor::probe_dirty().map_or(0, |d| d as u64)
}

/// D3343: the refusal a parallel session's leader returns when the memory
/// governor refuses its statement (the watchdog's cancel, the statement's own
/// limit, or the process past the governor's bar by its refusal margin);
/// `pgrc_am::write_error` raises the governor's 53200 for it.
pub const MEMORY_REFUSED: &str = "parallel load refused by the memory governor";

/// D3343: the leader's re-read period at the memory gate (seals and other
/// statements' releases do not wake it).
const MEMORY_GATE_POLL: core::time::Duration = core::time::Duration::from_millis(20);

fn memory_refused() -> bool {
    mcx::governor::refusal(MEMORY_REFUSED).is_some()
}

/// The bound [`PARTS_IN_FLIGHT_AUTO`] resolves to: the env override if it
/// parses positive, else [`parts_in_flight_for`] over the machine's cores and
/// headroom (the legacy constant where the machine gives no headroom signal).
pub fn machine_parts_in_flight(policy: &crate::writer::PartCutPolicy) -> usize {
    if let Some(n) = std::env::var(PARTS_IN_FLIGHT_ENV).ok().and_then(|v| v.trim().parse::<usize>().ok()) {
        if n > 0 {
            return n;
        }
    }
    let cores = machine_cores().unwrap_or(1);
    match machine_headroom_bytes() {
        Some(h) => parts_in_flight_for(cores, h, policy.max_bytes),
        None => LEGACY_MAX_PARTS_IN_FLIGHT,
    }
}

impl ParIngestOpts {
    /// The default bounds with the open-parts bound derived from the machine
    /// at engine construction ([`PARTS_IN_FLIGHT_AUTO`]).
    pub fn auto() -> ParIngestOpts {
        ParIngestOpts { max_parts_in_flight: PARTS_IN_FLIGHT_AUTO, ..ParIngestOpts::default() }
    }

    /// [`PARTS_IN_FLIGHT_AUTO`] replaced by the machine's bound; every other
    /// value passes through untouched.
    pub fn resolved(self, policy: &crate::writer::PartCutPolicy) -> ParIngestOpts {
        if self.max_parts_in_flight == PARTS_IN_FLIGHT_AUTO {
            let parts = machine_parts_in_flight(policy);
            ParIngestOpts {
                max_parts_in_flight: parts,
                max_chunks_in_flight: self.max_chunks_in_flight.max(chunks_for_parts(parts)),
                ..self
            }
        } else {
            self
        }
    }

    /// FIX-A (ingest-contention charter §5): derive the open-parts bound
    /// from a byte budget instead of a bare constant.
    ///
    /// Per-part in-flight footprint operand: an open part holds its
    /// normalized chunk buffers (≈ `policy.max_bytes` at cut — the cut
    /// predicate trips just past the byte budget) and, during assembly,
    /// their spliced whole-part twin (≈ `policy.max_bytes` again;
    /// [`ParEngine`]'s seal path frees each chunk as it splices, so the
    /// pair is the ceiling, not the steady state) plus encode transients —
    /// `2 × max_bytes` brackets the measured 0.39–0.61 GB per open part at
    /// the 100m record spec (`max_bytes` = 256 MiB → operand 512 MiB; the
    /// charter's §3.5/§5 pricing).
    ///
    /// The bound: how many such parts the caller's parts budget holds,
    /// floored at [`LEGACY_MAX_PARTS_IN_FLIGHT`] and ceiled at `2 × dop` —
    /// at most `dop` parts can be actively sealing (seal is worker work),
    /// and one further wave of closed-awaiting-seal parts keeps the cut
    /// cursor from stalling while every worker seals; slots beyond that
    /// buy no concurrency, only deepen the priced worst case.
    pub fn parts_bound_for_budget(
        policy: &crate::writer::PartCutPolicy,
        dop: usize,
        parts_budget_bytes: u64,
    ) -> usize {
        let per_part = policy.max_bytes.saturating_mul(2).max(1);
        let fit = (parts_budget_bytes / per_part).min(usize::MAX as u64) as usize;
        let ceil = dop.saturating_mul(2).max(LEGACY_MAX_PARTS_IN_FLIGHT);
        fit.clamp(LEGACY_MAX_PARTS_IN_FLIGHT, ceil)
    }
}

impl Default for ParIngestOpts {
    fn default() -> ParIngestOpts {
        ParIngestOpts {
            chunk_rows: DEFAULT_CHUNK_ROWS,
            // Chunks are 8x finer than they were, so hold 8x more of them
            // to keep the same ~1Mi-row in-flight capture window (and so a
            // dop-16 pool is never starved by the bound itself).
            max_chunks_in_flight: 128,
            max_parts_in_flight: LEGACY_MAX_PARTS_IN_FLIGHT,
        }
    }
}

// ---------------------------------------------------------------------------
// Provider seams (per-task instantiation — the M2-A face precedent)
// ---------------------------------------------------------------------------

/// Mints a Vfs handle per seal task / cleanup pass. Production: [`RealVfsProvider`]
/// (a ZST per handle — the kernel owns all state, zero contention).
/// Tests/crash-matrix: [`SharedMemVfsProvider`] over one locked [`MemVfs`]
/// universe.
pub trait VfsProvider: Send + Sync {
    fn make(&self) -> Box<dyn WriteVfs + Send>;
}

/// Mints an external-detoast capability per normalize task (COPY under
/// O-M3-1(a) feeds inline datums, so [`NoDetoastProvider`] is the M3
/// default, same as serial).
pub trait DetoastProvider: Send + Sync {
    fn make(&self) -> Box<dyn ExternalDetoast + Send>;
}

/// Mints a shred-lane source per part-assembly task.
pub trait ShredProvider: Send + Sync {
    fn make(&self) -> Box<dyn ShredLaneSource + Send>;
}

/// The session leader's WAL sink, made once on the leader thread when the engine is built.
pub trait WalProvider: Send + Sync {
    fn leader(&self) -> Box<dyn WalSink + Send>;
}

pub struct NoWalProvider;

impl WalProvider for NoWalProvider {
    fn leader(&self) -> Box<dyn WalSink + Send> {
        Box::new(NoWal)
    }
}

/// Production Vfs provider: every handle is a fresh [`RealVfs`] ZST.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealVfsProvider;

impl VfsProvider for RealVfsProvider {
    fn make(&self) -> Box<dyn WriteVfs + Send> {
        Box::new(RealVfs)
    }
}

/// The M3 default detoast provider (typed refusal, unreachable from COPY).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoDetoastProvider;

impl DetoastProvider for NoDetoastProvider {
    fn make(&self) -> Box<dyn ExternalDetoast + Send> {
        Box::new(NoExternalDetoast)
    }
}

/// The M3-D default shred provider (image lane only).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoShredProvider;

impl ShredProvider for NoShredProvider {
    fn make(&self) -> Box<dyn ShredLaneSource + Send> {
        Box::new(NoShred)
    }
}

/// One shared [`MemVfs`] universe behind a lock — the dop>1 test/crash
/// vehicle. File BYTES are schedule-independent (each seal task owns its own
/// temp file exclusively); only op-log interleaving varies. `crash_at_op`
/// composes: after the armed op, EVERY handle's ops fail (`killed`), exactly
/// a process-wide kill -9.
#[derive(Clone)]
pub struct SharedMemVfs(pub Arc<Mutex<MemVfs>>);

impl SharedMemVfs {
    pub fn new(v: MemVfs) -> SharedMemVfs {
        SharedMemVfs(Arc::new(Mutex::new(v)))
    }

    /// Run `f` over the underlying universe (arm crashes, read op logs,
    /// revive).
    pub fn with<R>(&self, f: impl FnOnce(&mut MemVfs) -> R) -> R {
        f(&mut lock(&self.0))
    }
}

impl WriteVfs for SharedMemVfs {
    fn create_rw(&mut self, path: &str) -> WriteResult<WvFd> {
        lock(&self.0).create_rw(path)
    }
    fn open_rw(&mut self, path: &str) -> WriteResult<WvFd> {
        lock(&self.0).open_rw(path)
    }
    fn pwrite_at(&mut self, fd: &WvFd, off: u64, bytes: &[u8]) -> WriteResult<()> {
        lock(&self.0).pwrite_at(fd, off, bytes)
    }
    fn fsync_file(&mut self, fd: &WvFd) -> WriteResult<()> {
        lock(&self.0).fsync_file(fd)
    }
    fn close_file(&mut self, fd: WvFd) -> WriteResult<()> {
        lock(&self.0).close_file(fd)
    }
    fn read_full(&mut self, path: &str) -> WriteResult<Vec<u8>> {
        lock(&self.0).read_full(path)
    }
    fn read_prefix(&mut self, path: &str, max: usize) -> WriteResult<Vec<u8>> {
        lock(&self.0).read_prefix(path, max)
    }
    fn rename_path(&mut self, from: &str, to: &str) -> WriteResult<()> {
        lock(&self.0).rename_path(from, to)
    }
    fn unlink_path(&mut self, path: &str) -> WriteResult<()> {
        lock(&self.0).unlink_path(path)
    }
    fn fsync_dir(&mut self, dir: &str) -> WriteResult<()> {
        lock(&self.0).fsync_dir(dir)
    }
    fn list_dir(&mut self, dir: &str) -> WriteResult<Vec<String>> {
        lock(&self.0).list_dir(dir)
    }
    fn mkdir_path(&mut self, dir: &str) -> WriteResult<()> {
        lock(&self.0).mkdir_path(dir)
    }
    fn exists_path(&mut self, path: &str) -> WriteResult<bool> {
        lock(&self.0).exists_path(path)
    }
    fn link_path(&mut self, from: &str, to: &str) -> WriteResult<()> {
        lock(&self.0).link_path(from, to)
    }
    fn link_count(&mut self, path: &str) -> WriteResult<u64> {
        lock(&self.0).link_count(path)
    }
}

/// Test provider over a [`SharedMemVfs`].
pub struct SharedMemVfsProvider(pub SharedMemVfs);

impl VfsProvider for SharedMemVfsProvider {
    fn make(&self) -> Box<dyn WriteVfs + Send> {
        Box::new(self.0.clone())
    }
}

/// Everything a parallel session's workers need, all `Send + Sync` owned
/// capabilities (`TaskSetWork` is `'static` — nothing borrows the leader's
/// stack).
pub struct ParProviders {
    pub vfs: Arc<dyn VfsProvider>,
    pub detoast: Arc<dyn DetoastProvider>,
    pub shred: Arc<dyn ShredProvider>,
    pub sources: Vec<Arc<dyn CandidateSource + Send + Sync>>,
    pub resolver: Arc<dyn VerifyResolver + Send + Sync>,
    pub shred_opts: ShredOptions,
    /// Structural-election facts (TY-1) — the parallel seal must elect
    /// exactly as serial (byte-identical-parts law). Default: none.
    pub structural: StructuralPolicy,
    pub wal: Arc<dyn WalProvider>,
}

// ---------------------------------------------------------------------------
// RowChunk — leader-side raw capture
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum Cell {
    Null,
    Word(u64),
    Bytes { off: u64, len: u64 },
}

/// One column's D2 side channel over a captured chunk: per-row
/// `(source, code)` slots plus the chunk-local source-dictionary table
/// (SEAL-SPEED-2; see [`crate::ingest::DictSide`]). Valid at normalize
/// only when it covers every chunk row — the coverage law.
#[derive(Default)]
struct DictLaneAcc {
    sources: Vec<std::sync::Arc<dyn DictEntries>>,
    rows: Vec<u64>,
}

impl DictLaneAcc {
    fn source_idx(&mut self, source: &std::sync::Arc<dyn DictEntries>) -> u32 {
        match self
            .sources
            .iter()
            .position(|s| std::sync::Arc::ptr_eq(s, source))
        {
            Some(i) => i as u32,
            None => {
                self.sources.push(source.clone());
                (self.sources.len() - 1) as u32
            }
        }
    }
}

/// One captured input chunk: raw datum images copied row-major into an
/// owned arena (Send-able; normalization deferred to the claiming worker).
pub struct RowChunk {
    /// CONVERT W3-2b: the loading statement's charge on [`Self::held_bytes`] (none for [`RowChunk::new`]). Declared
    /// first: it drops (on the worker that normalized the chunk) before the blocks are freed.
    charge: HeldCharge,
    ncols: usize,
    rows: u32,
    cells: Vec<Cell>,
    arena: Vec<u8>,
    /// Per column, the most heap bytes appending this chunk's captured images takes (C1-REG): what a pooled append
    /// reserves in the column first.
    col_heap: Vec<u64>,
    /// D2 side channels, one slot per column (empty = no channel).
    dict_lanes: Vec<DictLaneAcc>,
    /// CONVERT W3-2b: (cells, arena) capacity at the charge's last sync: a row whose push moved neither costs two
    /// compares, a growth syncs the charge on the growing thread.
    seen: (usize, usize),
}

impl RowChunk {
    /// Public for drivers and the loom/permutation suites (the capture
    /// currency); production capture goes through [`ParSession::append_row`].
    /// Uncharged: a load captures with [`RowChunk::with_owner`].
    pub fn new(ncols: usize, chunk_rows: u32) -> RowChunk {
        RowChunk::with_owner(ncols, chunk_rows, None)
    }

    /// CONVERT W3-2b: a chunk whose blocks are charged to `owner` (the loading statement, read once on the COPY
    /// leader and passed by value), kind `Region`, from its first block; `None`: uncharged, as [`RowChunk::new`].
    /// The columns its normalize builds take the same owner.
    pub fn with_owner(ncols: usize, chunk_rows: u32, owner: Option<memledger::Owner>) -> RowChunk {
        let mut c = RowChunk {
            charge: HeldCharge::new(owner),
            ncols,
            rows: 0,
            cells: Vec::with_capacity(ncols * chunk_rows as usize), // alloc: Exec charged self.charge
            arena: Vec::new(),
            col_heap: vec![0; ncols], // alloc: Exec charged self.charge
            dict_lanes: (0..ncols).map(|_| DictLaneAcc::default()).collect(), // alloc: Exec charged self.charge
            seen: (0, 0),
        };
        c.sync_charge();
        c
    }

    /// CONVERT W3-2b: the owner the chunk is charged to (`None`: uncharged).
    pub fn owner(&self) -> Option<memledger::Owner> {
        self.charge.owner()
    }

    /// CONVERT W3-2b: the charge brought to [`Self::held_bytes`] now, on this thread.
    pub fn sync_charge(&mut self) {
        self.seen = (self.cells.capacity(), self.arena.capacity());
        self.charge.sync(self.held_bytes());
    }

    /// CONVERT W3-2b: sync only if the cells or the arena moved since the last sync (the per-row check).
    #[inline]
    fn sync_if_grown(&mut self) {
        if (self.cells.capacity(), self.arena.capacity()) != self.seen {
            self.sync_charge();
        }
    }

    /// D2: append one row's `(source, code)` slot to column `col`'s side
    /// channel (the pump's seam-stitch face). `code = None` = a PLAIN /
    /// code-free row within a dict-carrying source (the hybrid law).
    pub fn push_dict_code(
        &mut self,
        col: usize,
        source: &std::sync::Arc<dyn DictEntries>,
        code: Option<u32>,
    ) {
        let lane = &mut self.dict_lanes[col];
        let cap = lane.rows.capacity();
        match code {
            Some(c) => {
                let s = lane.source_idx(source);
                lane.rows.push((u64::from(s) << 32) | u64::from(c)); // alloc: Exec charged self.charge
            }
            None => {
                // Source membership still registers (a chunk whose rows are
                // all fallback keeps its dict-carrying provenance).
                let _ = lane.source_idx(source);
                lane.rows.push(DICT_ROW_PLAIN); // alloc: Exec charged self.charge
            }
        }
        if lane.rows.capacity() != cap {
            self.sync_charge();
        }
    }

    /// D2: bulk-attach a whole aligned span of column `col`'s codes (the
    /// decode-worker face; `u32::MAX` slots = PLAIN rows). The span must
    /// align with the rows already pushed for this chunk.
    pub fn attach_dict_span(
        &mut self,
        col: usize,
        source: &std::sync::Arc<dyn DictEntries>,
        codes: &[u32],
    ) -> WriteResult<()> {
        let lane = &mut self.dict_lanes[col];
        let s = lane.source_idx(source);
        pgrc_format::try_reserve_more(&mut lane.rows, codes.len(), "par")?; // alloc: Exec charged self.charge
        for &c in codes {
            lane.rows.push(if c == u32::MAX { // alloc: Exec charged self.charge
                DICT_ROW_PLAIN
            } else {
                (u64::from(s) << 32) | u64::from(c)
            });
        }
        self.sync_charge();
        Ok(())
    }

    pub fn rows(&self) -> u32 {
        self.rows
    }

    pub fn ncols(&self) -> usize {
        self.ncols
    }

    /// The most heap bytes appending column `c` takes, short of a detoast that inflates a value: each captured image's
    /// bytes plus a 4-byte header and 7 bytes of alignment.
    pub fn column_heap_bound(&self, c: usize) -> u64 {
        self.col_heap[c]
    }

    /// D3343: the heap bytes this chunk holds (its cell and arena capacity).
    pub fn held_bytes(&self) -> u64 {
        let lanes: usize = self.dict_lanes.iter().map(|l| l.rows.capacity() * 8).sum();
        (self.cells.capacity() * core::mem::size_of::<Cell>() + self.arena.capacity() + self.col_heap.capacity() * 8 + lanes) as u64
    }

    /// Content digest over the captured rows, row-major, with byte cells
    /// resolved through the arena (layout-free: two chunks that normalize
    /// identically digest identically whatever their arena order). The
    /// fused-decoder identity test's face; never on the load path.
    pub fn content_digest(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |x: u64| {
            h = (h ^ x).wrapping_mul(0x0000_0100_0000_01b3).rotate_left(29);
        };
        mix(u64::from(self.rows));
        for cell in self.cells.iter() {
            match *cell {
                Cell::Null => mix(0xdead),
                Cell::Word(w) => {
                    mix(1);
                    mix(w)
                }
                Cell::Bytes { off, len } => {
                    mix(2);
                    mix(len);
                    for c in self.arena[off as usize..(off + len) as usize].chunks(8) {
                        let mut b = [0u8; 8];
                        b[..c.len()].copy_from_slice(c);
                        mix(u64::from_le_bytes(b));
                    }
                }
            }
        }
        h
    }

    /// Column-fill face (the fused parquet decoder): reserve `n` more rows of cells. The
    /// caller then writes EVERY cell of those rows (each column, each row)
    /// through `fill_*` and publishes them with [`Self::fill_commit`].
    pub fn fill_reserve(&mut self, n: u32) {
        self.cells.reserve(n as usize * self.ncols); // alloc: Exec charged self.charge
        self.sync_if_grown();
    }

    /// # Safety
    /// `row` < rows() + the reserved rows, `col` < ncols.
    #[inline(always)]
    pub unsafe fn fill_word(&mut self, row: u32, col: usize, w: u64) {
        unsafe { self.cells.as_mut_ptr().add(row as usize * self.ncols + col).write(Cell::Word(w)) }
    }

    /// Point a cell at `arena[off..off + len]` (several cells may share one
    /// image — normalize reads through the span).
    /// # Safety
    /// As [`Self::fill_word`].
    #[inline(always)]
    pub unsafe fn fill_bytes(&mut self, row: u32, col: usize, off: u64, len: u64) {
        // C1-REG: the column's heap bound counts every captured image, as `push_row` does.
        self.col_heap[col] += len + 11;
        unsafe {
            self.cells
                .as_mut_ptr()
                .add(row as usize * self.ncols + col)
                .write(Cell::Bytes { off, len })
        }
    }

    /// # Safety
    /// As [`Self::fill_word`].
    #[inline(always)]
    pub unsafe fn fill_null(&mut self, row: u32, col: usize) {
        unsafe { self.cells.as_mut_ptr().add(row as usize * self.ncols + col).write(Cell::Null) }
    }

    /// Publish `n` filled rows.
    /// # Safety
    /// Every cell of rows `rows()..rows() + n` was written through `fill_*`.
    pub unsafe fn fill_commit(&mut self, n: u32) {
        let len = self.cells.len() + n as usize * self.ncols;
        debug_assert!(len <= self.cells.capacity());
        unsafe { self.cells.set_len(len) };
        self.rows += n;
        // the column-fill face grows the arena through `arena_mut` on this thread
        self.sync_if_grown();
    }

    /// The image arena (column-fill face).
    #[inline(always)]
    pub fn arena_mut(&mut self) -> &mut Vec<u8> {
        &mut self.arena
    }

    pub fn push_row(&mut self, row: &[RawDatum<'_>]) -> WriteResult<()> {
        if row.len() != self.ncols {
            return Err(WriteError::Contract {
                detail: "row width mismatch",
            });
        }
        for (c, d) in row.iter().enumerate() {
            match d {
                RawDatum::Null => self.cells.push(Cell::Null), // alloc: Exec charged self.charge
                RawDatum::Word(w) => self.cells.push(Cell::Word(*w)), // alloc: Exec charged self.charge
                RawDatum::Bytes(b) => {
                    let off = self.arena.len() as u64;
                    self.arena.extend_from_slice(b); // alloc: Exec charged self.charge
                    self.col_heap[c] += b.len() as u64 + 11;
                    self.cells.push(Cell::Bytes { // alloc: Exec charged self.charge
                        off,
                        len: b.len() as u64,
                    });
                }
            }
        }
        self.rows += 1;
        self.sync_if_grown();
        Ok(())
    }

    /// Column `c` of every captured row into `col`, by the appends [`RowChunk::normalize`] runs (F1 L6).
    pub fn append_column(&self, c: usize, col: &mut ColBuffer, ext: &mut dyn ExternalDetoast, scratch: &mut Vec<u8>) -> WriteResult<()> {
        for r in 0..self.rows as usize {
            append_cell(self.cells[r * self.ncols + c], &self.arena, col, ext, scratch)?;
        }
        Ok(())
    }

    /// True when a D2 side channel rides some column.
    pub fn has_dict_lanes(&self) -> bool {
        self.dict_lanes.iter().any(|l| !l.sources.is_empty())
    }

    /// Worker-side normalization: replay the captured rows through the SAME
    /// per-class appends serial ingest uses (detoast law included) into
    /// chunk-local `ColBuffer`s.
    fn normalize(
        &self,
        schema: &[ColSchema],
        ext: &mut dyn ExternalDetoast,
    ) -> WriteResult<Vec<ColBuffer>> {
        // CONVERT W3-2b: the chunk's columns are its owner's (each syncs at its reserve and its appends' end below)
        let owner = self.owner();
        let mut cols: Vec<ColBuffer> = schema.iter().map(|s| ColBuffer::with_owner(*s, owner)).collect();
        // PGRC-WRITER-FAST `alloc`: size every chunk buffer once (payload
        // bytes per column from one pass over the cells) — no regrowth.
        // Capacity only: bytes cannot move.
        let mut bytes: memledger::ChargedVec<usize> = memledger::ChargedVec::new(self.owner(), memledger::Kind::Region);
        bytes.resize(self.ncols, 0usize).map_err(|_| WriteError::Format(pgrc_format::FormatError::OutOfMemory { bytes: self.ncols * core::mem::size_of::<usize>() }))?;
        for r in 0..self.rows as usize {
            for (c, b) in bytes.iter_mut().enumerate() {
                if let Cell::Bytes { len, .. } = self.cells[r * self.ncols + c] {
                    *b += len as usize;
                }
            }
        }
        for (c, col) in cols.iter_mut().enumerate() {
            col.reserve_rows(self.rows as usize, bytes[c])?;
        }
        let mut scratch: Vec<u8> = Vec::new();
        for r in 0..self.rows as usize {
            for (c, col) in cols.iter_mut().enumerate() {
                append_cell(self.cells[r * self.ncols + c], &self.arena, col, ext, &mut scratch)?;
            }
        }
        // D2: hand covered side channels to the chunk buffers (the coverage
        // law — an uncovered lane never attaches; splice drops mixed parts).
        for (c, col) in cols.iter_mut().enumerate() {
            let lane = &self.dict_lanes[c];
            if !lane.sources.is_empty() && lane.rows.len() as u32 == self.rows {
                col.attach_dict_side(lane.sources.clone(), lane.rows.clone())?; // alloc: Exec charged col.charge
            }
        }
        for col in cols.iter_mut() {
            col.sync_charge();
        }
        Ok(cols)
    }
}

fn append_cell(cell: Cell, arena: &[u8], col: &mut ColBuffer, ext: &mut dyn ExternalDetoast, scratch: &mut Vec<u8>) -> WriteResult<()> {
    match (cell, col.schema.class) {
        (Cell::Null, _) => col.append_null(),
        (Cell::Word(w), StorageClass::ByvalWord { .. })
        | (Cell::Word(w), StorageClass::F32)
        | (Cell::Word(w), StorageClass::F64)
        | (Cell::Word(w), StorageClass::Bool) => col.append_word(w)?,
        (Cell::Bytes { off, len }, StorageClass::Fixed { .. }) => col.append_fixed(&arena[off as usize..(off + len) as usize])?,
        (Cell::Bytes { off, len }, StorageClass::VarlenaVerbatim) => {
            let (payload, _) = normalize_varlena(&arena[off as usize..(off + len) as usize], ext, scratch)?;
            col.append_varlena_payload(payload)?;
        }
        _ => {
            return Err(WriteError::Contract {
                detail: "datum shape does not match column class",
            })
        }
    }
    Ok(())
}

/// One input chunk already in column form (the column-direct parquet
/// feed): chunk-local buffers built by the same appends
/// [`RowChunk::normalize`] runs, so the worker skips normalization.
pub struct ColChunk {
    cols: Vec<ColBuffer>,
    rows: u32,
}

impl ColChunk {
    pub fn new(cols: Vec<ColBuffer>) -> WriteResult<ColChunk> {
        let rows = cols.first().map_or(0, ColBuffer::rows);
        if cols.iter().any(|c| c.rows() != rows) || rows > u64::from(u32::MAX) {
            return Err(WriteError::Contract {
                detail: "column chunk with ragged columns",
            });
        }
        Ok(ColChunk { cols, rows: rows as u32 })
    }

    pub fn rows(&self) -> u32 {
        self.rows
    }

    pub fn ncols(&self) -> usize {
        self.cols.len()
    }

    pub fn held_bytes(&self) -> u64 {
        self.cols.iter().map(ColBuffer::held_bytes).sum()
    }

    pub fn schema_matches(&self, schema: &[ColSchema]) -> bool {
        self.cols.len() == schema.len() && self.cols.iter().zip(schema).all(|(c, s)| c.schema == *s)
    }
}

enum Captured {
    Rows(RowChunk),
    Cols(ColChunk),
}

impl Captured {
    fn held_bytes(&self) -> u64 {
        match self {
            Captured::Rows(c) => c.held_bytes(),
            Captured::Cols(c) => c.held_bytes(),
        }
    }
}

// ---------------------------------------------------------------------------
// Coordinator
// ---------------------------------------------------------------------------

struct NormChunk {
    cols: Vec<ColBuffer>,
    rows: u64,
    bytes: u64,
}

/// One closed part, owned by exactly one task (the cursor-advancer that
/// closed it).
struct PartJob {
    seq: u32,
    /// First input-chunk ordinal of the part — the error-ordinal currency
    /// (serial-replay error parity: errors report in INPUT order).
    first_chunk: u64,
    chunks: Vec<NormChunk>,
    /// Its chunks' bytes: the seal holds them and the spliced part beside.
    bytes: u64,
}

#[derive(Default)]
struct Coord {
    /// Captured chunks awaiting a worker claim, by chunk id.
    captured: BTreeMap<u64, Captured>,
    /// Normalized chunks awaiting the cut cursor, by chunk id.
    normalized: BTreeMap<u64, NormChunk>,
    /// Next chunk id the cut cursor consumes (ids below are assigned).
    cut_cursor: u64,
    part_start_chunk: u64,
    part_rows: u64,
    /// The open part's byte metric, kept SERIAL-EXACT (#597): the words +
    /// validity contribution, which is seam-invariant (chunk seams sit on
    /// 64-row validity words), sums flat here...
    part_flat_bytes: u64,
    /// ...while each column's heap accumulates through the same
    /// pad-to-8-then-add arithmetic the part assembler's splice runs at
    /// every interior chunk seam ([`ColBuffer::splice_chunk`]). Summing
    /// chunk-LOCAL `approx_bytes` instead — the pre-#597 metric — omits
    /// those seam pads (0-7 bytes per byref column per seam) and cuts a
    /// different granule boundary than serial once the skew straddles
    /// `max_bytes`. Indexed by column; sized at engine construction.
    part_heap_bytes: Vec<u64>,
    /// Parts closed but not yet sealed (backpressure input).
    parts_open: usize,
    /// High-water of `parts_open` (the FIX-A seal-concurrency witness:
    /// the parts-pace gate cell wants observed > the legacy 8).
    peak_parts_open: usize,
    /// Parts closed so far (seq = base_seq + i).
    assigned_parts: u32,
    /// Set at close_input: total published chunks.
    total_chunks: Option<u64>,
    /// Sealed gather, by seq.
    sealed: BTreeMap<u32, (SealedPart, SealReport)>,
    /// Sealed parts' files not yet logged by the leader, one entry per part.
    forwarded: Vec<Vec<Forwarded>>,
    /// Lowest-INPUT-ORDINAL error wins (serial-replay error parity, IN-2):
    /// serial ingest surfaces the FIRST error in input order, so the
    /// parallel session reports the recorded error with the lowest chunk
    /// ordinal — for a session whose first defect is chunk-attributable,
    /// the surfaced error is exactly the serial one. Any error still
    /// cancels the session immediately (claims drain as no-ops), so a
    /// lower-ordinal defect sitting in a never-normalized chunk is not
    /// discovered — exact multi-defect parity is the binding driver's
    /// serial replay, documented at the seam.
    error: Option<(u64, WriteError)>,
    /// Witness: pool worker ordinals that ran ingest work.
    workers_seen: BTreeSet<usize>,
}

/// The parallel-session engine: coordination + assembly + seal, independent
/// of the runtime binding (loom drives these faces directly; the rtpool
/// binding drives [`ParEngine::run_worker`] on pool workers).
pub struct ParEngine {
    mu: Mutex<Coord>,
    /// Leader-only waits (backpressure); workers only ever notify.
    cv: Condvar,
    /// FIX-B cancel face (ingest-contention charter S1): the session-
    /// cancelled flag, readable WITHOUT the coordinator mutex — the claim
    /// loop's not-yet-published arm used to lock the GLOBAL mutex on
    /// every poll just to read this one bool (the measured poll-storm
    /// face). Written only inside [`ParEngine::record_error`] (which
    /// holds the Coord lock); monotone false → true.
    cancelled: AtomicBool,
    /// FIX-B wake face (the parked-claimer protocol, charter S6): a
    /// claim-starved worker parks here instead of yield-polling the
    /// claim plane (the measured ~5.5M sched_yield/s storm). Eventcount
    /// shape: `wake_seq` bumps BEFORE every notify and the parker
    /// re-checks it under `wake_mu`, so a poke landing anywhere after
    /// the caller's [`ParEngine::wake_seq`] snapshot makes
    /// [`ParEngine::park_wake`] return without sleeping — no lost wake
    /// (loom models 3–5 in `tests/loom.rs`). Pokers: `publish_chunk`
    /// (watermark moved), `close_input` (drain), `record_error` (cancel),
    /// and the driver's own faces via [`ParEngine::poke_wake`]. Workers
    /// still never wait on other WORKERS — every poke source is
    /// leader/pump progress or cancellation (the run-to-completion law;
    /// GL-GANGWEDGE stays structurally excluded).
    wake_seq: AtomicU64,
    wake_mu: Mutex<()>,
    wake_cv: Condvar,
    /// The shared claim cursor (PC-2.3): chunk-morsel claims ride its CAS;
    /// dispatch takes no locks.
    claim: ClaimCursor,
    /// Publish watermark (units `< published` are claimable). Monotone;
    /// bumped by the leader AFTER the captured chunk is visible.
    published_units: AtomicU64,
    /// Total chunk count once input closes; `u64::MAX` = still growing.
    closed_total: AtomicU64,
    providers: ParProviders,
    /// Leader-only; the lock is never contended.
    leader_sink: Mutex<Box<dyn WalSink + Send>>,
    logs: bool,
    /// The leader sink's `ref_min`, read once: workers fsync and forward only the reference of a file past it (WR).
    ref_min: Option<u64>,
    schema: Vec<ColSchema>,
    spec: PartSpec,
    table_dir: String,
    fxid: u64,
    policy: PartCutPolicy,
    opts: ParIngestOpts,
    base_seq: u32,
    /// SEAL-EARLY lead-in witnesses (print-only diagnosis marks; bank
    /// bytes cannot see them): the engine's birth instant plus first-event
    /// offsets in microseconds (0 = not yet happened) — first published
    /// chunk, first part close (cut), first completed seal. The measured
    /// law they witness: on a device-bound volume every second before the
    /// first completed seal (= the first device write) is a second of
    /// wall, one-for-one.
    born: std::time::Instant,
    mark_first_publish_us: AtomicU64,
    mark_first_cut_us: AtomicU64,
    mark_first_seal_us: AtomicU64,
    /// D3343: the heap bytes the session holds outside memory contexts (a
    /// captured chunk; a normalized chunk twice, for itself and its share of
    /// its part's spliced image, until the part's seal ends), the part of
    /// them the leader charged to the process footprint and its statement's
    /// ledger (`sync_charge`), and the most it may hold past the floor.
    /// D3808: the charge also goes to the memory policy's load account
    /// (`memledger::LOAD_IN_FLIGHT`).
    held: AtomicI64,
    budget: u64,
    charged: AtomicU64,
    peak_held: AtomicU64,
    gate_waits: AtomicU64,
    gate_polls: AtomicU64,
    /// D3808: bytes the load's other half has planned but not yet made
    /// resident (a parquet COPY's decode window: its charge less its live
    /// blocks), set by the session's caller ([`ParEngine::set_ahead`]); the
    /// memory gate counts them as resident already, since they arrive
    /// whatever the gate decides.
    ahead: AtomicU64,
    /// D4365: `pgrust.memory_ladder = on` when the session began. With it, the memory gate tests every chunk (the floor
    /// too) against the ladder's line ([`ParEngine::ladder_gate`]), and a normalized chunk holds [`SEAL_X_LADDER`] times
    /// its bytes until its part's seal ends (the seal reserve); without it, today's gate and 2x, byte for byte.
    ladder: bool,
    /// D4365: the per-byte hold of a normalized chunk until its part's seal ends: 2 (ladder off) or [`SEAL_X_LADDER`].
    seal_x: u64,
    /// D4365 (ladder on): the seal reserve not yet resident: (seal_x - 1) x the bytes of every normalized chunk whose part
    /// has not finished its seal. The gate counts it as resident already, as it counts `ahead`.
    seal_ahead: AtomicU64,
    /// CONVERT W3-2b: the loading statement's owner (read once on the COPY leader, [`ParEngine::with_owner`]): the
    /// chunks the session captures, the columns it normalizes and the parts it splices and seals are charged to it.
    /// `None`: uncharged (crate tests, loom).
    owner: Option<memledger::Owner>,
}

/// D4365: a normalized chunk's hold with the ladder on: its part's seal peaks at ~3x the part's normalized bytes (measured
/// on a 240 MB part: spliced image 1x, then the text key's dictionary, the float8 and the verbatim pad streams' election
/// scratch took the anon 721-748 MB over the idle base), where the 2x hold counted the image and the splice's pair only.
pub const SEAL_X_LADDER: u64 = 3;

/// D4387 (ladder on): every load's seal reserve not yet resident, process-wide: each parallel engine's `seal_ahead` and
/// each serial writer's part reserve. A gate asks for it whole, so loads at once see one another's seals before those
/// seals take a byte (one load's own reserve alone let three concurrent COPYs each fit, then seal past the kernel's
/// limit together). Moves only with the ladder on.
static SEAL_AHEAD_ALL: AtomicU64 = AtomicU64::new(0);

/// D4387: the process-wide seal reserve ([`SEAL_AHEAD_ALL`]) now.
pub fn seal_ahead_all() -> u64 {
    SEAL_AHEAD_ALL.load(Ordering::Relaxed)
}

/// D4387: a load's seal reserve grows by `n` bytes outside a gate's pass (a normalized chunk's reserve replacing the
/// one its raw bytes were admitted with).
pub(crate) fn seal_ahead_grow(n: u64) {
    SEAL_AHEAD_ALL.fetch_add(n, Ordering::Relaxed);
}

/// D4387: a load's seal reserve shrinks by `n` bytes (its seal began, or the load ended without it): progress for the
/// loads that wait on it ([`RESERVE_EPOCH`]).
pub(crate) fn seal_ahead_shrink(n: u64) {
    if n != 0 {
        SEAL_AHEAD_ALL.fetch_sub(n, Ordering::Relaxed);
        RESERVE_EPOCH.fetch_add(1, Ordering::Relaxed);
    }
}

/// D4387: the part of [`SEAL_AHEAD_ALL`] that loads blocked at their gates cannot seal while they stay blocked (a serial
/// writer's buffered part; a parallel load's open part once every chunk it published is cut): the other gates leave it
/// out, so loads that each fit alone never all wait on one another's reserves (a circular wait that refused all three
/// c3 COPYs at 2G). A blocked load's own ask keeps it.
static WAITING_EXCL: AtomicU64 = AtomicU64::new(0);

/// D4387: one gate's fit decision at a time ([`Seat::try_pass`]): the decision and the counters it moves together, so
/// two blocked loads never both pass on each other's exclusion. Held for arithmetic and atomics only: the ladder's read
/// is taken before it (`mcx::ladder::read_now`), and nothing under it allocates or reads a file.
static GATE_MU: pgsync::Mutex<()> = pgsync::Mutex::new(());

/// D4387: bumped when a load passes its gate or a seal reserve leaves: a waiting gate's budget counts from the last
/// progress it saw, as from the last free (`mcx::ladder::epoch`), so a load waiting its turn behind loads that move is
/// not handed off or refused while they move (`mempolicy::ladder::WAIT_CAP_X` budgets still end any wait).
static RESERVE_EPOCH: AtomicU64 = AtomicU64::new(0);

/// D4387: the progress a waiting gate counts its budget from: the ladder's frees and the loads' passes and seals.
fn wait_epoch() -> u64 {
    mcx::ladder::epoch().wrapping_add(RESERVE_EPOCH.load(Ordering::Relaxed))
}

/// D4387: a load's place at the process-wide gate for one publish or granule: the bytes of its reserve it has left out of
/// the others' asks while it waits ([`WAITING_EXCL`]); passing or leaving puts them back.
#[derive(Default)]
pub(crate) struct Seat {
    excl: u64,
}

impl Seat {
    /// Under [`GATE_MU`]: whether this load's `own` ask plus every load's reserve the blocked loads have not left out
    /// (this one's own exclusion included) fits under the ladder's line at `read`. When it fits, `admit` joins the
    /// process-wide reserve and this load's exclusion ends; when not, its exclusion becomes `excludable` and the ask is
    /// returned (the climb's and hand-off's size).
    fn try_pass(&mut self, read: Option<u64>, own: u64, admit: u64, excludable: u64) -> Result<(), u64> {
        let _g = pgsync::lock(&GATE_MU);
        let shared = SEAL_AHEAD_ALL.load(Ordering::Relaxed).saturating_sub(WAITING_EXCL.load(Ordering::Relaxed));
        let ask = own.saturating_add(shared).saturating_add(self.excl);
        if mcx::ladder::need_at(read, ask) == 0 {
            self.pass_locked(admit);
            return Ok(());
        }
        WAITING_EXCL.fetch_add(excludable, Ordering::Relaxed);
        WAITING_EXCL.fetch_sub(self.excl, Ordering::Relaxed);
        self.excl = excludable;
        Err(ask)
    }

    /// A pass the ladder cannot weigh (no hand-off to make): `admit` joins the reserve as [`Seat::try_pass`]'s does.
    fn pass(&mut self, admit: u64) {
        let _g = pgsync::lock(&GATE_MU);
        self.pass_locked(admit);
    }

    fn pass_locked(&mut self, admit: u64) {
        WAITING_EXCL.fetch_sub(core::mem::take(&mut self.excl), Ordering::Relaxed);
        SEAL_AHEAD_ALL.fetch_add(admit, Ordering::Relaxed);
        RESERVE_EPOCH.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for Seat {
    fn drop(&mut self) {
        if self.excl != 0 {
            let _g = pgsync::lock(&GATE_MU);
            WAITING_EXCL.fetch_sub(core::mem::take(&mut self.excl), Ordering::Relaxed);
        }
    }
}

/// D4365: the gate's wait (the take_wait idiom of the grant pool: R5, then one hand-off), timed in its own polls of
/// [`MEMORY_GATE_POLL`] each: the polls in all, at the last free the ladder saw, and at the hand-off.
pub(crate) struct LadderWait {
    polls: u32,
    since: u32,
    seen: u64,
    handed: Option<u32>,
    wait: Option<mcx::ladder::Waiting>,
    /// lane ladderview: the wait as `pgrust_memory_ladder_stats()` and its episode ring count it (observe only).
    ep: mcx::ladder_stats::Wait,
}

impl LadderWait {
    fn waited(&self, from: u32) -> core::time::Duration {
        MEMORY_GATE_POLL.saturating_mul(self.polls - from)
    }
}

/// D4365: the refusal of a parallel load whose seal does not fit under the memory ladder's line after its climb and
/// waits: SPILL-DEBT's `LoadSealPastLine` (a COPY into pgrcolumnar cannot spill yet, D4370). `pgrc_am::write_error`
/// raises it as the 53200 [`MEMORY_REFUSED`] raises and names the row in the server log only.
pub const LOAD_PAST_LINE: &str = "parallel load whose seal does not fit under the memory ladder's line";

/// D4365: SPILL-DEBT `LoadSealPastLine`'s user sentence (its row's field 8, lint/SPILL-DEBT/LoadSealPastLine, and
/// gravity_program's `NoSpill::limitation`: tests/d4365_ladder_gate.rs holds this to the row), for the server-only line
/// `pgrc_am::write_error` writes beside the refusal (this crate cannot reach gravity_program: a dependency cycle).
pub const LOAD_SEAL_PAST_LINE: &str = "A COPY into a pgrcolumnar table seals each part it writes in memory, at about three times the part's size, where it cannot yet write that part out in pieces, so when the server has no more memory to give it the statement fails with an out-of-memory error instead of finishing more slowly.";

/// D4365: one step of a load's ladder gate (the parallel engine's [`ParEngine::ladder_gate`], the serial writer's
/// [`ladder_wait`]): under the ladder's line it fits; past it, first a climb, then R5 polls, one hand-off of R2b / R6 to
/// the ladder thread after a spent wait, and a refusal after a second.
/// D4387: the ask is this load's `own` plus every load's seal reserve but what blocked loads leave out ([`Seat`]); a
/// pass adds `admit` to the reserve; a wait leaves `excludable` of this load's reserve out of the others' asks.
pub(crate) fn ladder_step(own: u64, admit: u64, excludable: u64, seat: &mut Seat, lw: &mut Option<LadderWait>) -> LadderStep {
    let read = mcx::ladder::read_now();
    let ask = match seat.try_pass(read, own, admit, excludable) {
        Ok(()) => return LadderStep::Fits,
        Err(ask) => ask,
    };
    let Some(w) = lw else { return LadderStep::Climb(ask) };
    let e = wait_epoch();
    if e != w.seen {
        (w.seen, w.since) = (e, w.polls);
    }
    let budget = mempolicy::admit::bytes_wait();
    match w.handed {
        None if mempolicy::ladder::wait_spent(w.waited(w.since), w.waited(0), budget) => {
            if !mcx::ladder::hand_off(ask) {
                seat.pass(admit);
                return LadderStep::Fits;
            }
            w.handed = Some(w.polls);
        }
        Some(at) if mempolicy::ladder::wait_spent(w.waited(at), w.waited(at), budget) => {
            w.ep.refused();
            return LadderStep::Refuse;
        }
        _ => {}
    }
    w.polls = w.polls.saturating_add(1);
    LadderStep::Wait
}

/// D4368: the serial writer's ladder gate (`pgrust.memory_ladder = on` only; a backend thread, no engine lock): the
/// steps of [`ladder_step`], sleeping a [`MEMORY_GATE_POLL`] per R5 poll; a cancel or limit of the running statement
/// ends it with [`MEMORY_REFUSED`], a second spent wait with [`LOAD_PAST_LINE`] (SPILL-DEBT LoadSealPastLine).
/// D4387: `own` and `admit` as [`ladder_step`]'s; Ok: `admit` is in the process-wide reserve. While it waits the writer
/// cannot cut, so its whole registered reserve (`excludable`) is left out of the others' asks.
pub(crate) fn ladder_wait(own: u64, admit: u64, excludable: u64) -> WriteResult<()> {
    let mut lw: Option<LadderWait> = None;
    let mut seat = Seat::default();
    let out = loop {
        if memory_refused() {
            if let Some(w) = &lw { w.ep.canceled(); }
            break Err(WriteError::Refused { what: MEMORY_REFUSED });
        }
        match ladder_step(own, admit, excludable, &mut seat, &mut lw) {
            LadderStep::Fits => break Ok(()),
            LadderStep::Climb(a) => {
                let ep = mcx::ladder_stats::Wait::begin(mcx::ladder_stats::Site::Load, a);
                let w = mcx::ladder::wait_begin(&mcx::ladder::climb(a));
                lw = Some(LadderWait { polls: 0, since: 0, seen: wait_epoch(), handed: None, wait: w, ep });
            }
            LadderStep::Wait => waiter::sleep(MEMORY_GATE_POLL),
            LadderStep::Refuse => break Err(WriteError::Refused { what: LOAD_PAST_LINE }),
        }
    };
    if let Some(w) = lw {
        mcx::ladder::wait_end(w.wait);
    }
    out
}

/// D4365: a chunk's step at the ladder gate ([`ParEngine::ladder_gate`]).
pub(crate) enum LadderStep {
    Fits,
    Climb(u64),
    Wait,
    Refuse,
}

/// D4399: a feed leader's place at the engine's gate for bytes it plans to hold outside the engine (a parallel text
/// COPY's read and parse window), one publish at a time ([`ParEngine::ahead_step`]). Dropping it ends its ladder wait.
#[derive(Default)]
pub struct AheadGate {
    seat: Seat,
    lw: Option<LadderWait>,
    /// The waiter clock's milliseconds at the last counted ask while it waits.
    last: Option<i64>,
}

impl AheadGate {
    /// The gate's re-read period: a waiting leader asks again after it (its R5 polls are counted in it).
    pub const POLL: core::time::Duration = MEMORY_GATE_POLL;
}

impl Drop for AheadGate {
    fn drop(&mut self) {
        if let Some(w) = self.lw.take() {
            mcx::ladder::wait_end(w.wait);
        }
    }
}

/// D4399: one step of [`ParEngine::ahead_step`].
pub enum AheadStep {
    Fits,
    /// Not now: the leader commits what it can and asks again after [`AheadGate::POLL`].
    Wait,
    /// The statement's cancel or limit ([`MEMORY_REFUSED`]), or past the ladder's line after its waits
    /// ([`LOAD_PAST_LINE`], SPILL-DEBT LoadSealPastLine): `pgrc_am::write_error` raises it as 53200.
    Refused(WriteError),
}

impl ParEngine {
    /// D4399: one non-blocking step of the publish gate for `bytes` a feed's leader plans to hold outside the engine,
    /// never under the engine's lock: the statement's cancel or limit first, then [`ParEngine::memory_room`], then (ladder
    /// on) [`ladder_step`] for `bytes` plus what is planned but not resident ([`ParEngine::set_ahead`]), with its one climb
    /// and its R5 polls (one per [`AheadGate::POLL`]: an earlier ask is a Wait, uncounted), its hand-off and its refusal.
    pub fn ahead_step(&self, g: &mut AheadGate, bytes: u64) -> AheadStep {
        if memory_refused() {
            if let Some(w) = &g.lw { w.ep.canceled(); }
            return AheadStep::Refused(WriteError::Refused { what: MEMORY_REFUSED });
        }
        let now = waiter::now_ms();
        if g.last.is_some_and(|t| now.saturating_sub(t) < MEMORY_GATE_POLL.as_millis() as i64) {
            return AheadStep::Wait;
        }
        if !self.memory_room(bytes) {
            g.last = Some(now);
            return AheadStep::Wait;
        }
        if !self.ladder {
            g.last = None;
            return AheadStep::Fits;
        }
        let own = bytes.saturating_add(self.ahead.load(Ordering::Relaxed));
        match ladder_step(own, 0, 0, &mut g.seat, &mut g.lw) {
            LadderStep::Fits => {
                if let Some(w) = g.lw.take() {
                    mcx::ladder::wait_end(w.wait);
                }
                g.last = None;
                AheadStep::Fits
            }
            LadderStep::Climb(ask) => {
                let ep = mcx::ladder_stats::Wait::begin(mcx::ladder_stats::Site::Load, ask);
                let w = mcx::ladder::wait_begin(&mcx::ladder::climb(ask));
                g.lw = Some(LadderWait { polls: 0, since: 0, seen: wait_epoch(), handed: None, wait: w, ep });
                g.last = Some(now);
                AheadStep::Wait
            }
            LadderStep::Wait => {
                g.last = Some(now);
                AheadStep::Wait
            }
            LadderStep::Refuse => AheadStep::Refused(WriteError::Refused { what: LOAD_PAST_LINE }),
        }
    }
}

/// Outcome of one worker claim probe ([`ParEngine::step_claim`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimStep {
    /// A span was claimed and run.
    Progress,
    /// Nothing claimable yet: the space is still growing and the cursor
    /// sits at the watermark. The caller may park on the wake face
    /// (snapshot [`ParEngine::wake_seq`] BEFORE probing, then
    /// [`ParEngine::park_wake`]).
    Starved,
    /// The morsel space is drained (or the session cancelled at the
    /// watermark) — no further claims will ever exist.
    Drained,
}

/// The COPY morsel space (consumer #1 of the claim plane): units are
/// capture chunks — deterministic fixed-grain morsels (boundary placement
/// is a pure function of the input: `chunk_rows` is fixed per session).
/// No interior hard boundaries: part edges are the SINK's (the cut cursor
/// decides them from accumulated bytes; claims are normalize-work, which
/// is part-agnostic).
struct ChunkMorsels<'e>(&'e ParEngine);

impl MorselSource for ChunkMorsels<'_> {
    fn total_units(&self) -> Option<u64> {
        let t = self.0.closed_total.load(Ordering::Acquire);
        if t == u64::MAX {
            None
        } else {
            Some(t)
        }
    }
    fn published(&self) -> u64 {
        self.0.published_units.load(Ordering::Acquire)
    }
    fn next_boundary_after(&self, _unit: u64) -> u64 {
        u64::MAX
    }
}

/// D4387: a load that ends with chunks normalized but never sealed (an error, a cancel) takes their reserve out of the
/// process-wide one.
impl Drop for ParEngine {
    fn drop(&mut self) {
        seal_ahead_shrink(self.seal_ahead.swap(0, Ordering::Relaxed));
    }
}

impl ParEngine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        providers: ParProviders,
        schema: Vec<ColSchema>,
        spec: PartSpec,
        table_dir: String,
        fxid: u64,
        policy: PartCutPolicy,
        opts: ParIngestOpts,
        base_seq: u32,
    ) -> WriteResult<ParEngine> {
        let opts = opts.resolved(&policy);
        if opts.chunk_rows == 0 || opts.chunk_rows % 64 != 0 {
            return Err(WriteError::Contract {
                detail: "chunk_rows must be a positive multiple of 64",
            });
        }
        // The byte-identity invariant: the cursor can only close a part at a
        // chunk boundary, so the chunk grain must BE the cut granule. Any
        // other pairing silently reproduces the divergence M3-I closed.
        if opts.chunk_rows != policy.cut_granule_rows {
            return Err(WriteError::Contract {
                detail: "chunk_rows must equal the policy's cut_granule_rows",
            });
        }
        if opts.max_chunks_in_flight == 0 || opts.max_parts_in_flight == 0 {
            return Err(WriteError::Contract {
                detail: "parallel in-flight bounds must be positive",
            });
        }
        if schema.is_empty() {
            return Err(WriteError::Contract {
                detail: "table with zero columns",
            });
        }
        let leader_sink = providers.wal.leader();
        let logs = leader_sink.logs();
        let ref_min = if logs { leader_sink.ref_min() } else { None };
        Ok(ParEngine {
            mu: Mutex::new(Coord {
                part_heap_bytes: pgrc_format::try_filled(schema.len(), 0, "par")?,
                ..Coord::default()
            }),
            cv: Condvar::new(),
            cancelled: AtomicBool::new(false),
            wake_seq: AtomicU64::new(0),
            wake_mu: Mutex::new(()),
            wake_cv: Condvar::new(),
            claim: ClaimCursor::new(),
            published_units: AtomicU64::new(0),
            closed_total: AtomicU64::new(u64::MAX),
            providers,
            leader_sink: Mutex::new(leader_sink),
            logs,
            ref_min,
            schema,
            spec,
            table_dir,
            fxid,
            policy,
            opts,
            base_seq,
            born: std::time::Instant::now(),
            mark_first_publish_us: AtomicU64::new(0),
            mark_first_cut_us: AtomicU64::new(0),
            mark_first_seal_us: AtomicU64::new(0),
            held: AtomicI64::new(0),
            budget: machine_headroom_bytes().unwrap_or(u64::MAX),
            charged: AtomicU64::new(0),
            peak_held: AtomicU64::new(0),
            gate_waits: AtomicU64::new(0),
            gate_polls: AtomicU64::new(0),
            ahead: AtomicU64::new(0),
            ladder: mcx::ladder::on(),
            seal_x: if mcx::ladder::on() { SEAL_X_LADDER } else { 2 },
            seal_ahead: AtomicU64::new(0),
            owner: None,
        })
    }

    /// CONVERT W3-2b: the engine charges what it holds to `owner` (the loading statement), before it is shared.
    pub fn with_owner(mut self, owner: Option<memledger::Owner>) -> ParEngine {
        self.owner = owner;
        self
    }

    /// CONVERT W3-2b: the owner the session's buffers are charged to; a feed captures its chunks with it
    /// ([`RowChunk::with_owner`], [`ColBuffer::with_owner`]).
    pub fn owner(&self) -> Option<memledger::Owner> {
        self.owner
    }

    fn hold(&self, n: u64) {
        let now = self.held.fetch_add(n as i64, Ordering::Relaxed) + n as i64;
        self.peak_held.fetch_max(now.max(0) as u64, Ordering::Relaxed);
    }

    fn unhold(&self, n: u64) {
        self.held.fetch_sub(n as i64, Ordering::Relaxed);
    }

    /// D3343: whether `need` more bytes keep the process under the governor's
    /// bar and the running statement under its own limit. D3808: and the
    /// process's resident bytes inside the load's room under the memory
    /// policy ([`ParEngine::load_room`]), with the governor off too.
    fn memory_room(&self, need: u64) -> bool {
        let bar = mcx::governor::bar();
        if bar != 0 && mcx::governor::projected().saturating_add(need as usize) > bar {
            return false;
        }
        if need > self.load_room() {
            return false;
        }
        mcx::governor::current().is_none_or(|l| l.limit() == 0 || l.bytes().saturating_add(need as isize) <= l.limit() as isize)
    }

    /// D4365 (ladder on): this chunk's step at the gate. The ask is what the chunk will hold through its part's seal
    /// (`bytes` x seal_x) plus what is planned but not made resident: every load's seal reserve (D4387: the process's,
    /// [`seal_ahead_all`], this load's among them) and this load's parquet decode window; under the ladder's line it fits. Past it: one climb (R1-R4b), then the wait (R5) re-reading the line each
    /// poll; a wait spent with the need still there hands R2b / R6 to the ladder thread once (whose cancel of the largest
    /// holder reaches this leader as `memory_refused`), and a second spent wait refuses the load (53200).
    fn ladder_gate(&self, bytes: u64, excludable: u64, seat: &mut Seat, lw: &mut Option<LadderWait>) -> LadderStep {
        let own = bytes.saturating_mul(self.seal_x).saturating_add(self.ahead.load(Ordering::Relaxed));
        ladder_step(own, bytes.saturating_mul(self.seal_x - 1), excludable, seat, lw)
    }

    /// D4387: the seal reserve of the open part a leader blocked before chunk `id` cannot close while it stays blocked:
    /// every chunk it published is normalized and cut (`cut_cursor == id`), so only a later publish (or its close of the
    /// input) can close the part; 0 while a published chunk is still in flight (it may close the part).
    fn open_part_reserve(&self, c: &Coord, id: u64) -> u64 {
        if c.cut_cursor != id {
            return 0;
        }
        let open: u64 = c.normalized.range(c.part_start_chunk..c.cut_cursor).map(|(_, n)| n.bytes).sum();
        open.saturating_mul(self.seal_x - 1)
    }

    /// D3808: the load's room now ([`load_room_at`]) by the resident bytes
    /// the governor's probe reads now (/proc/self/status and, under a cgroup,
    /// its memory.stat: tens of microseconds, once per chunk the gate passes
    /// or per 20 ms poll while it waits) plus the bytes planned ahead
    /// ([`ParEngine::set_ahead`]), its dirty page cache read beside it;
    /// `u64::MAX` with no probe.
    fn load_room(&self) -> u64 {
        let ahead = self.ahead.load(Ordering::Relaxed);
        mcx::governor::probe_resident().map_or(u64::MAX, |r| load_room_at((r as u64).saturating_add(ahead), dirty_now()))
    }

    /// D3808: the bytes the load has planned but not yet made resident
    /// outside this engine (a parquet COPY's decode window: its charge less
    /// its live blocks); the memory gate counts them as resident. Any thread.
    pub fn set_ahead(&self, bytes: u64) {
        self.ahead.store(bytes, Ordering::Relaxed);
    }

    /// D3808: (bytes held now, the session's budget) for a load's trace.
    pub fn memory_now(&self) -> (u64, u64) {
        (self.held.load(Ordering::Relaxed).max(0) as u64, self.budget)
    }

    /// Leader only: move the charge to what the session holds now, on the
    /// leader's own statement ledger.
    fn sync_charge(&self) {
        let want = self.held.load(Ordering::Relaxed).max(0) as u64;
        let have = self.charged.swap(want, Ordering::Relaxed);
        if want > have {
            mcx::global_footprint::charge_raw((want - have) as usize);
            memledger::LOAD_IN_FLIGHT.charge(want - have);
        } else if have > want {
            mcx::global_footprint::uncharge_raw((have - want) as usize);
            memledger::LOAD_IN_FLIGHT.discharge(have - want);
        }
    }

    /// Leader, after the runtime drain (and on its unwind): give the whole
    /// charge back.
    pub fn release_charge(&self) {
        let n = self.charged.swap(0, Ordering::Relaxed);
        if n != 0 {
            mcx::global_footprint::uncharge_raw(n as usize);
            memledger::LOAD_IN_FLIGHT.discharge(n);
        }
    }

    /// D3343 witness: (peak bytes held, memory-gate waits (each counted when
    /// it ends), ms gated (counted in gate polls, each as it starts)).
    pub fn memory_report(&self) -> (u64, u64, u64) {
        (
            self.peak_held.load(Ordering::Relaxed),
            self.gate_waits.load(Ordering::Relaxed),
            self.gate_polls.load(Ordering::Relaxed) * MEMORY_GATE_POLL.as_millis() as u64,
        )
    }

    /// Stamp a first-event mark once (0 -> now; later calls no-op). A
    /// same-microsecond first event stores 1 — the marks are diagnosis
    /// grain, never arithmetic inputs.
    fn stamp_once(&self, mark: &AtomicU64) {
        if mark.load(Ordering::Relaxed) == 0 {
            let us = self.born.elapsed().as_micros().max(1) as u64;
            let _ = mark.compare_exchange(0, us, Ordering::Relaxed, Ordering::Relaxed);
        }
    }

    /// SEAL-EARLY lead-in marks, seconds since engine birth: (first
    /// publish, first part close, first completed seal); 0.0 = never
    /// happened. Diagnosis witnesses only.
    pub fn leadin_marks(&self) -> (f64, f64, f64) {
        let s = |m: &AtomicU64| m.load(Ordering::Relaxed) as f64 / 1e6;
        (
            s(&self.mark_first_publish_us),
            s(&self.mark_first_cut_us),
            s(&self.mark_first_seal_us),
        )
    }

    /// Leader: publish one captured chunk under backpressure. Returns the
    /// chunk id (the caller owes a `StreamSource::publish(id + 1)` + runtime
    /// wake AFTER this returns). Blocks while in-flight bounds are full;
    /// surfaces the session error if one is recorded.
    /// D3343: the chunk's bytes are charged to the leader's statement; while
    /// a part is sealing, a chunk waits at the memory gate until the session
    /// is within its budget and the process has room for the chunk under the
    /// governor's bar (with no part sealing it passes: one part at a time is
    /// the floor, a serial load's footprint); a governor refusal ends the
    /// session.
    pub fn publish_chunk(&self, chunk: RowChunk, id: u64) -> WriteResult<()> {
        self.publish(Captured::Rows(chunk), id)
    }

    /// [`Self::publish_chunk`] for a chunk already in column form.
    pub fn publish_cols(&self, chunk: ColChunk, id: u64) -> WriteResult<()> {
        if !chunk.schema_matches(&self.schema) {
            return Err(WriteError::Contract {
                detail: "column chunk schema does not match the session",
            });
        }
        self.publish(Captured::Cols(chunk), id)
    }

    /// The session's column schemas (a column-form feed builds against them).
    pub fn schema(&self) -> &[ColSchema] {
        &self.schema
    }

    fn publish(&self, chunk: Captured, id: u64) -> WriteResult<()> {
        let bytes = chunk.held_bytes();
        self.hold(bytes);
        self.sync_charge();
        let mut c = lock(&self.mu);
        let mut polls = 0u64;
        let mut lw: Option<LadderWait> = None;
        let mut seat = Seat::default();
        let admitted = loop {
            if let Some((_, e)) = &c.error {
                break Err(e.clone());
            }
            if memory_refused() {
                if let Some(w) = &lw { w.ep.canceled(); }
                break Err(WriteError::Refused { what: MEMORY_REFUSED });
            }
            if !c.forwarded.is_empty() {
                drop(c);
                self.drain_forwarded()?;
                c = lock(&self.mu);
                continue;
            }
            if c.captured.len() < self.opts.max_chunks_in_flight
                && c.parts_open < self.opts.max_parts_in_flight
            {
                let within = self.held.load(Ordering::Relaxed).max(0) as u64 <= self.budget;
                if self.ladder {
                    // D4365: the floor is no exemption from the ladder's line: one part's seal is ~3x its bytes.
                    // D4387: the load's own room (D3808) first, then the ladder's gate, whose pass admits the chunk's
                    // seal reserve into the process-wide one at once
                    if c.parts_open == 0 || (within && self.memory_room(bytes.saturating_mul(self.seal_x))) {
                        let excludable = self.open_part_reserve(&c, id);
                        match self.ladder_gate(bytes, excludable, &mut seat, &mut lw) {
                            LadderStep::Fits => {
                                self.seal_ahead.fetch_add(bytes.saturating_mul(self.seal_x - 1), Ordering::Relaxed);
                                break Ok(());
                            }
                            LadderStep::Climb(ask) => {
                                // the climb's rungs run off the engine's lock (the workers seal under it)
                                drop(c);
                                let ep = mcx::ladder_stats::Wait::begin(mcx::ladder_stats::Site::Load, ask);
                                let w = mcx::ladder::wait_begin(&mcx::ladder::climb(ask));
                                lw = Some(LadderWait { polls: 0, since: 0, seen: wait_epoch(), handed: None, wait: w, ep });
                                c = lock(&self.mu);
                                continue;
                            }
                            LadderStep::Wait => {}
                            LadderStep::Refuse => break Err(WriteError::Refused { what: LOAD_PAST_LINE }),
                        }
                    }
                } else if c.parts_open == 0 || (within && self.memory_room(bytes.saturating_mul(2))) {
                    break Ok(());
                }
                polls += 1;
                // counted as it happens: a reader of `memory_report` sees a wait in progress
                self.gate_polls.fetch_add(1, Ordering::Relaxed);
                c = self.cv.wait_timeout(c, MEMORY_GATE_POLL).map(|(g, _)| g).unwrap_or_else(|e| e.into_inner().0);
                self.sync_charge();
                continue;
            }
            c = self.cv.wait(c).unwrap_or_else(|e| e.into_inner());
        };
        if polls != 0 {
            self.gate_waits.fetch_add(1, Ordering::Relaxed);
        }
        drop(seat);
        if let Some(w) = lw {
            mcx::ladder::wait_end(w.wait);
        }
        if let Err(e) = admitted {
            drop(c);
            self.unhold(bytes);
            return Err(e);
        }
        c.captured.insert(id, chunk);
        drop(c);
        self.stamp_once(&self.mark_first_publish_us);
        // Publish watermark AFTER the chunk is visible under the lock:
        // a claimer that wins ordinal `id` must find it captured.
        self.published_units.fetch_max(id + 1, Ordering::AcqRel);
        // FIX-B: the watermark moved — wake parked claimers.
        self.poke_wake();
        Ok(())
    }

    /// Wake-face snapshot — sample BEFORE probing any claim face, then
    /// pass to [`ParEngine::park_wake`] if every face says "nothing yet".
    /// The no-lost-wake protocol rides this ordering: any poke after the
    /// snapshot moves the sequence, and `park_wake` re-checks it under
    /// the wake mutex before sleeping.
    pub fn wake_seq(&self) -> u64 {
        self.wake_seq.load(Ordering::Acquire)
    }

    /// Park until the wake sequence moves past `seen` (publish progress,
    /// input close, cancellation, or a driver poke). Returns immediately
    /// if it already has. The caller re-probes its claim faces on return
    /// (the wake is a hint, never a hand-off).
    pub fn park_wake(&self, seen: u64) {
        let mut g = lock(&self.wake_mu);
        while self.wake_seq.load(Ordering::Acquire) == seen {
            g = self.wake_cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Bump the wake face. Engine-internal pokes cover the engine's own
    /// faces (publish / close / cancel); this is public so a driver whose
    /// workers fold MORE faces into the same park (the sortload merge
    /// loop: ticket-queue growth, per-segment decode-window advance,
    /// driver-plane cancellation) can wake them when one of ITS faces
    /// grows. All poke sources are leader/pump progress or cancellation —
    /// never worker-on-worker waiting.
    pub fn poke_wake(&self) {
        // Bump BEFORE the notify, notify with the mutex held (the #941
        // wake-not-lost discipline): a parker that snapshotted before
        // this bump either re-checks under the mutex and skips the wait,
        // or is already registered on the condvar and receives it.
        self.wake_seq.fetch_add(1, Ordering::AcqRel);
        let _g = lock(&self.wake_mu);
        self.wake_cv.notify_all();
    }

    /// Record an error at input ordinal `ord` — LOWEST ordinal wins (the
    /// error-parity selection; see [`Coord::error`]). Always cancels.
    fn record_error(&self, c: &mut Coord, ord: u64, e: WriteError) {
        match &c.error {
            Some((prev, _)) if *prev <= ord => {}
            _ => c.error = Some((ord, e)),
        }
        // Cancel is the atomic flag (FIX-B: claim polls read it lock-free);
        // the store happens under the Coord lock the caller holds, so
        // lock-section readers keep their one consistent view.
        self.cancelled.store(true, Ordering::SeqCst);
        self.cv.notify_all();
        // Parked claimers must observe the cancel (the cancel-wake leg of
        // the parked-claimer protocol; loom model 4). Lock order is
        // Coord → wake only, never the reverse (park_wake holds only the
        // wake mutex; wake_seq() is lock-free).
        self.poke_wake();
    }

    /// Leader: record a session-fatal error (feed failure). Feed errors
    /// carry the ordinal of the chunk being fed (input order); a leader
    /// with no ordinal context passes `u64::MAX` (any chunk-attributed
    /// error is earlier in input order than an end-of-feed failure).
    pub fn fail(&self, e: WriteError) {
        self.fail_at(u64::MAX, e)
    }

    /// [`ParEngine::fail`] with an explicit input ordinal.
    pub fn fail_at(&self, ord: u64, e: WriteError) {
        let mut c = lock(&self.mu);
        self.record_error(&mut c, ord, e);
    }

    /// Leader: end of input. `total` = number of chunks published. Any
    /// final-part close whose chunks are ALREADY all normalized happens here
    /// and the returned jobs are the LEADER's to seal (via [`ParEngine::seal_jobs`]);
    /// otherwise the worker normalizing the last chunk closes it.
    pub fn close_input(&self, total: u64) -> Vec<u32> {
        // Close the morsel space FIRST (watermark == total at close is the
        // MorselSource contract), so parked claimers observe Drained.
        self.published_units.fetch_max(total, Ordering::AcqRel);
        self.closed_total.store(total, Ordering::Release);
        // FIX-B: wake parked claimers so they observe the drain (after
        // BOTH stores above — a woken claimer must see the closed space).
        self.poke_wake();
        let jobs = {
            let mut c = lock(&self.mu);
            c.total_chunks = Some(total);
            let jobs = self.advance_cut(&mut c);
            self.cv.notify_all();
            jobs
        };
        self.seal_jobs(jobs)
    }

    /// The morsel-scheduled worker loop — COPY as the claim plane's
    /// consumer #1 (PC-2.2 worker-local claiming; PC-2.3 shared-cursor
    /// dispatch): claim a chunk-morsel span, run it, repeat until the
    /// space drains. A claim-starved worker PARKS on the wake face
    /// (FIX-B: block-not-poll — the pre-fix yield-poll shape was the
    /// measured ~5.5M sched_yield/s storm) and re-wakes on publish
    /// progress, input close, or cancel. Ordering lives WHOLLY in the
    /// sink: the cut cursor consumes normalized morsels strictly in input
    /// order, so ANY claim schedule — including pathological worker-speed
    /// skew — yields byte-identical parts (the dirsha determinism law,
    /// witnessed by the skew probe in `tests/par_determinism.rs`).
    pub fn run_worker(&self, worker: usize) {
        loop {
            // Snapshot BEFORE the probe (the no-lost-wake ordering).
            let seen = self.wake_seq();
            match self.step_claim(worker) {
                ClaimStep::Progress => {}
                ClaimStep::Drained => return,
                ClaimStep::Starved => self.park_wake(seen),
            }
        }
    }

    /// One claim probe + run (the composable worker face): the caller
    /// owns the idle policy. The sortload merge loop folds this outcome
    /// into its own park decision (several claim families, one park);
    /// [`ParEngine::run_worker`] parks on the engine wake face;
    /// [`ParEngine::run_one_claim`] keeps the legacy yield-poll shape.
    pub fn step_claim(&self, worker: usize) -> ClaimStep {
        let src = ChunkMorsels(self);
        match self.claim.try_claim(&src, worker, &NoObserver) {
            ClaimOutcome::Claimed(span) => {
                for id in span.start..span.end {
                    self.run_chunk(worker, id);
                }
                ClaimStep::Progress
            }
            ClaimOutcome::Drained => ClaimStep::Drained,
            ClaimOutcome::NotYetPublished => {
                // FIX-B (charter S1): the cancel flag is an atomic — the
                // pre-fix shape locked the GLOBAL Coord mutex on every
                // poll just to read this bool.
                if self.cancelled.load(Ordering::SeqCst) {
                    ClaimStep::Drained
                } else {
                    ClaimStep::Starved
                }
            }
        }
    }

    /// One claim advance in the legacy poll shape — split out so a
    /// driver can pace BETWEEN claims (the worker-speed-skew determinism
    /// probe paces exactly here; the passthrough parquet driver's mixed
    /// loop still polls). Returns `false` when the morsel space is
    /// drained (or the session is cancelled at the watermark).
    pub fn run_one_claim(&self, worker: usize) -> bool {
        match self.step_claim(worker) {
            ClaimStep::Progress => true,
            ClaimStep::Drained => false,
            ClaimStep::Starved => {
                // pgsync::thread is the sanctioned surface (native: the
                // verbatim std re-export) — and the rtpool-only source pin
                // (pgrc_ingest_par/tests/runtime_binding.rs leg 5) string-
                // scans this file for raw thread primitives.
                pgsync::thread::yield_now();
                true
            }
        }
    }

    /// Worker face: normalize chunk `id`, feed the cut cursor, seal any
    /// parts this task closed. Run-to-completion; never blocks on another
    /// task. Infallible by the `run_morsel` contract — errors are recorded
    /// and cancel the session.
    pub fn run_chunk(&self, worker: usize, id: u64) {
        let chunk = {
            let mut c = lock(&self.mu);
            c.workers_seen.insert(worker);
            let at_bound = c.captured.len() >= self.opts.max_chunks_in_flight;
            let chunk = c.captured.remove(&id);
            // Notify hygiene (FIX-B): the ONLY cv waiter is the leader in
            // `publish_chunk`, blocked while captured OR parts sit at
            // their bounds. This section changes only the captured arm —
            // notify exactly at the from-full transition (removals are
            // unit-stepped under the lock, so the crossing fires once);
            // the parts arm has its own crossing notify in `seal_jobs`,
            // and the error arm notifies inside `record_error`.
            if at_bound && chunk.is_some() {
                self.cv.notify_all();
            }
            if self.cancelled.load(Ordering::SeqCst) {
                if let Some(ch) = &chunk {
                    self.unhold(ch.held_bytes());
                }
                return;
            }
            match chunk {
                Some(ch) => ch,
                None => {
                    self.record_error(
                        &mut c,
                        id,
                        WriteError::Contract {
                            detail: "claimed chunk was never captured",
                        },
                    );
                    return;
                }
            }
        };
        let raw = chunk.held_bytes();
        let normalized = match chunk {
            Captured::Rows(chunk) => {
                let mut ext = self.providers.detoast.make();
                chunk.normalize(&self.schema, &mut *ext)
            }
            Captured::Cols(chunk) => Ok(chunk.cols),
        };
        let jobs = {
            let mut c = lock(&self.mu);
            match normalized {
                Err(e) => {
                    self.unhold(raw);
                    if self.ladder {
                        // D4387: the chunk's reserve, admitted at its gate by its raw bytes, leaves with it
                        self.seal_ahead.fetch_sub(raw.saturating_mul(self.seal_x - 1), Ordering::Relaxed);
                        seal_ahead_shrink(raw.saturating_mul(self.seal_x - 1));
                    }
                    // Error parity: normalize errors carry their INPUT
                    // ordinal — the same row order serial `append_row`
                    // surfaces its first error in.
                    self.record_error(&mut c, id, e);
                    return;
                }
                Ok(cols) => {
                    let rows = cols.first().map(|cb| cb.rows()).unwrap_or(0);
                    let bytes: u64 = cols.iter().map(ColBuffer::held_bytes).sum();
                    self.hold(bytes.saturating_mul(self.seal_x));
                    if self.ladder {
                        // D4387: the reserve admitted at the gate by the chunk's raw bytes becomes its normalized bytes'
                        let (now, was) = (bytes.saturating_mul(self.seal_x - 1), raw.saturating_mul(self.seal_x - 1));
                        self.seal_ahead.fetch_add(now, Ordering::Relaxed);
                        self.seal_ahead.fetch_sub(was, Ordering::Relaxed);
                        seal_ahead_grow(now);
                        seal_ahead_shrink(was);
                    }
                    self.unhold(raw);
                    c.normalized.insert(id, NormChunk { cols, rows, bytes });
                }
            }
            // No cv notify here (FIX-B hygiene): nothing the leader waits
            // on becomes truer in this section — captured is untouched and
            // `advance_cut` can only GROW parts_open (close_part); the
            // error arm notifies inside `record_error`.
            self.advance_cut(&mut c)
        };
        self.seal_jobs(jobs);
    }

    /// Advance the cut cursor over consecutively-normalized chunks (INPUT
    /// ORDER — the deterministic partition law). Must hold the lock. Closed
    /// parts belong to THIS caller.
    fn advance_cut(&self, c: &mut Coord) -> Vec<PartJob> {
        let mut jobs = Vec::new();
        if self.cancelled.load(Ordering::SeqCst) {
            return jobs;
        }
        loop {
            let Some(nc) = c.normalized.get(&c.cut_cursor) else {
                // The final part closes when every published chunk is
                // consumed and rows remain.
                if c.total_chunks == Some(c.cut_cursor) && c.part_rows > 0 {
                    jobs.push(self.close_part(c));
                }
                break;
            };
            c.part_rows += nc.rows;
            // #597: feed the predicate the SERIAL byte operand, exactly.
            // The chunk-local `approx_bytes` sum is blind to the heap pad8
            // the part assembler's splice inserts at each interior chunk
            // seam (`ColBuffer::splice_chunk`) — 0-7 bytes per byref column
            // per seam, enough to move a cut across `max_bytes` at a
            // granule boundary. Replaying the splice's pad arithmetic per
            // column makes the metric below equal
            // `TableWriter::buffered_bytes` at every granule boundary.
            for (i, cb) in nc.cols.iter().enumerate() {
                let heap = cb.heap_len();
                c.part_flat_bytes += cb.approx_bytes() - heap;
                if heap > 0 {
                    let h = &mut c.part_heap_bytes[i];
                    *h = h.next_multiple_of(8) + heap;
                }
            }
            c.cut_cursor += 1;
            let part_bytes = c.part_flat_bytes + c.part_heap_bytes.iter().sum::<u64>();
            // The ONE shared predicate (see `PartCutPolicy::should_cut`),
            // fed the ONE shared metric. `part_rows` is a multiple of
            // chunk_rows == cut_granule_rows at every chunk boundary, so
            // the granule gate always admits here and the decision matches
            // the serial writer's row for row — and, per above, byte for
            // byte.
            if self.policy.should_cut(c.part_rows, part_bytes) {
                jobs.push(self.close_part(c));
            }
        }
        jobs
    }

    fn close_part(&self, c: &mut Coord) -> PartJob {
        self.stamp_once(&self.mark_first_cut_us);
        let seq = self.base_seq + c.assigned_parts;
        c.assigned_parts += 1;
        c.parts_open += 1;
        if c.parts_open > c.peak_parts_open {
            c.peak_parts_open = c.parts_open;
        }
        let first_chunk = c.part_start_chunk;
        let chunks: Vec<NormChunk> = (c.part_start_chunk..c.cut_cursor)
            .map(|i| c.normalized.remove(&i).expect("cursor passed over it"))
            .collect();
        let bytes = chunks.iter().map(|n| n.bytes).sum();
        c.part_start_chunk = c.cut_cursor;
        c.part_rows = 0;
        c.part_flat_bytes = 0;
        for h in &mut c.part_heap_bytes {
            *h = 0;
        }
        PartJob {
            seq,
            first_chunk,
            chunks,
            bytes,
        }
    }

    /// Seal the given closed parts (caller owns them — worker or leader).
    fn seal_jobs(&self, jobs: Vec<PartJob>) -> Vec<u32> {
        let mut seqs = Vec::with_capacity(jobs.len());
        for job in jobs {
            seqs.push(job.seq);
            let held = job.bytes.saturating_mul(self.seal_x);
            if self.ladder {
                self.seal_ahead.fetch_sub(job.bytes.saturating_mul(self.seal_x - 1), Ordering::Relaxed);
                seal_ahead_shrink(job.bytes.saturating_mul(self.seal_x - 1));
            }
            if self.cancelled.load(Ordering::SeqCst) {
                // Cancelled: drop the job; cleanup unlinks assigned temps by
                // name. parts_open stays high, which is fine — only the
                // leader consults it, and a cancelled leader exits its wait
                // via the error arm.
                drop(job);
                self.unhold(held);
                continue;
            }
            let first_chunk = job.first_chunk;
            let sealed = self.seal_one(job);
            self.unhold(held);
            let mut c = lock(&self.mu);
            match sealed {
                Ok((seq, out, files)) if self.logs => {
                    self.stamp_once(&self.mark_first_seal_us);
                    c.sealed.insert(seq, out);
                    c.forwarded.push(files);
                    self.cv.notify_all();
                }
                Ok((seq, out, _)) => {
                    self.stamp_once(&self.mark_first_seal_us);
                    c.sealed.insert(seq, out);
                    c.parts_open -= 1;
                    // Notify hygiene (FIX-B): wake the leader exactly at
                    // the downward crossing of the parts bound. A burst of
                    // closes can overshoot the bound, but decrements are
                    // unit-stepped under the lock, so the crossing fires
                    // exactly once; above the bound the leader's predicate
                    // stays false and a wake would be spurious.
                    if c.parts_open + 1 == self.opts.max_parts_in_flight {
                        self.cv.notify_all();
                    }
                }
                Err(e) => {
                    // Seal errors attribute to the part's first input
                    // chunk (input-order error selection).
                    self.record_error(&mut c, first_chunk, e);
                }
            }
        }
        seqs
    }

    /// Assemble one part from its chunks (seam replay, input order) and run
    /// the FROZEN seal face. Mirrors `TableWriter::cut_part` exactly.
    fn seal_one(&self, mut job: PartJob) -> WriteResult<(u32, (SealedPart, SealReport), Vec<Forwarded>)> {
        let mut cols: memledger::ChargedVec<ColBuffer> = memledger::ChargedVec::new(self.owner(), memledger::Kind::Region);
        cols.with_vec(|v| v.extend(self.schema.iter().map(|s| ColBuffer::with_owner(*s, self.owner)))); // alloc: Exec charged self.owner()
        // Consume-and-drop per chunk: after a chunk is spliced its buffers
        // are dead — freeing them here keeps the open part's footprint at
        // ONE whole-part image through encode instead of two (the FIX-A
        // budget's 2 × max_bytes per-part operand covers the splice-window
        // pair; this keeps the measured footprint under it with margin).
        // Same chunks, same input order — bytes cannot move.
        // PGRC-WRITER-FAST `alloc`: reserve each part accumulator for the
        // whole splice up front (capacity only).
        for (i, col) in cols.iter_mut().enumerate() {
            col.reserve_splice(job.chunks.iter().map(|c| &c.cols[i]))?;
        }
        for chunk in std::mem::take(&mut job.chunks) {
            for (i, cb) in chunk.cols.iter().enumerate() {
                cols[i].splice_chunk(cb)?;
            }
        }
        let mut shred = self.providers.shred.make();
        let mut lanes: memledger::ChargedVec<ShredLane> = memledger::ChargedVec::new(self.owner(), memledger::Kind::Region);
        for col in &cols[..] {
            if col.schema.class == StorageClass::VarlenaVerbatim {
                let derived = shred.shred(col, &self.providers.shred_opts)?;
                validate_lanes(col, &derived, &self.providers.shred_opts)?;
                lanes.with_vec(|v| v.extend(derived)); // alloc: Exec charged self.owner()
            }
        }
        // D3038: the columns whose octet-length lanes `seal_part` derives, as `cut_part` names them
        let octet = crate::octet::octet_columns(&cols, &self.providers.structural, &*shred);
        // The SAME factory the serial site uses (`seal_one` mirrors
        // `cut_part` exactly — including, now, the metadata plane, so
        // serial and parallel ingest keep producing byte-identical parts).
        let mut builders: Vec<Box<dyn ColumnMetaBuilder>> =
            crate::meta_wire::builders_for_streams(&cols, &lanes, &octet);
        let mut sources: memledger::ChargedVec<&dyn CandidateSource> = memledger::ChargedVec::new(self.owner(), memledger::Kind::Region);
        sources.with_vec(|v| v.extend(self.providers.sources.iter().map(|s| s.as_ref() as &dyn CandidateSource))); // alloc: Exec charged self.owner()
        let mut vfs = self.providers.vfs.make();
        let mut fwd = ForwardSink::with_ref_min(self.ref_min);
        let mut lvfs;
        let target: &mut dyn WriteVfs = if self.logs {
            lvfs = LoggedVfs::new(&mut *vfs, &mut fwd, &self.table_dir);
            &mut lvfs
        } else {
            &mut *vfs
        };
        let out = seal_part(
            target,
            &self.table_dir,
            &self.spec,
            &cols,
            &lanes,
            &octet,
            &mut builders,
            &sources,
            self.providers.resolver.as_ref(),
            &self.providers.structural,
            self.fxid,
            job.seq,
        )?;
        Ok((job.seq, out, fwd.files))
    }

    /// Leader: log every part the workers finished since the last drain, then release their in-flight slots.
    fn drain_forwarded(&self) -> WriteResult<()> {
        loop {
            let batch = std::mem::take(&mut lock(&self.mu).forwarded);
            if batch.is_empty() {
                return Ok(());
            }
            let mut vfs = self.providers.vfs.make();
            {
                let mut sink = lock(&self.leader_sink);
                for f in batch.iter().flatten() {
                    log_forwarded(&mut **sink, &mut *vfs, &self.table_dir, f)?;
                }
            }
            let mut c = lock(&self.mu);
            c.parts_open -= batch.len();
            self.cv.notify_all();
        }
    }

    /// Leader, AFTER the runtime drain: collect the ordered seal results, or
    /// surface the session error (cleanup is the caller's next move).
    pub fn collect(&self) -> WriteResult<Vec<(SealedPart, SealReport)>> {
        if lock(&self.mu).error.is_none() {
            self.drain_forwarded()?;
        }
        let mut c = lock(&self.mu);
        if let Some((_, e)) = c.error.take() {
            return Err(e);
        }
        if c.sealed.len() as u32 != c.assigned_parts {
            return Err(WriteError::Contract {
                detail: "parallel drain left unsealed parts",
            });
        }
        let sealed = std::mem::take(&mut c.sealed);
        // BTreeMap iteration is seq order — the ordered commit.
        Ok(sealed.into_values().collect())
    }

    /// Leader, error path AFTER the drain: unlink every assigned part temp
    /// by name under one `RECLAIM` (ENOENT-tolerant — a cancelled task may
    /// never have created its file).
    pub fn cleanup_temps(&self) -> WriteResult<()> {
        let assigned = {
            let mut c = lock(&self.mu);
            c.forwarded.clear();
            c.assigned_parts
        };
        if assigned == 0 {
            return Ok(());
        }
        let names = (0..assigned).map(|i| temp_file_name(self.fxid, self.base_seq + i)).collect();
        let rec = ReclaimRec { dir: dir_loc(&self.spec), xid: self.fxid as u32, retire_below: None, names };
        let mut vfs = self.providers.vfs.make();
        let dir = self.table_dir.as_str();
        match lock(&self.leader_sink).reclaim(&rec, &mut || apply_reclaim(&mut *vfs, dir, &rec)) {
            // A crashed universe (killed vfs) cannot unlink; kill -9 residue is
            // end-of-recovery cleanup's to reclaim by name.
            Err(WriteError::Io { op: "killed", .. }) => Ok(()),
            r => r,
        }
    }

    /// Binding-crate accessors (the driver builds sessions from these).
    /// The session's resolved bounds (the machine-derived open-parts bound
    /// included).
    pub fn opts(&self) -> &ParIngestOpts {
        &self.opts
    }

    pub fn chunk_rows(&self) -> u32 {
        self.opts.chunk_rows
    }

    pub fn fxid(&self) -> u64 {
        self.fxid
    }

    pub fn ncols(&self) -> usize {
        self.schema.len()
    }

    /// Witness: pool worker ordinals that executed ingest work (the rtpool
    /// engagement witness for the §5 assertion).
    pub fn workers_seen(&self) -> BTreeSet<usize> {
        lock(&self.mu).workers_seen.clone()
    }

    /// Number of parts assigned (test observability).
    pub fn assigned_parts(&self) -> u32 {
        lock(&self.mu).assigned_parts
    }

    /// High-water of concurrently-open (closed-but-unsealed) parts — the
    /// FIX-A seal-concurrency witness (the parts-pace gate cell: observed
    /// peak must be able to exceed the legacy 8-slot cap).
    pub fn peak_parts_open(&self) -> usize {
        lock(&self.mu).peak_parts_open
    }
}

// The rtpool BINDING (task set, feeding session, driver) lives one crate up
// in `pgrc_ingest_par`: since M3-H (#473) wired `pgrc_am` → `pgrc_write`
// into the table-AM cone, a `runtime` dependency here would close a cycle
// (runtime → parallel → … → tableam → pgrc_am → pgrc_write). This module
// is the complete coordination + assembly engine INCLUDING the claim path
// (`run_worker` claims chunk-morsels through the shared `pgrc_claim`
// cursor — the COPY-on-morsels amendment); the binding crate parks/wakes
// pool workers around exactly the faces above (`publish_chunk` /
// `run_worker` / `close_input` / `collect` / `cleanup_temps` / `fail`).
// Error parity at the seam: the engine surfaces the LOWEST-input-ordinal
// recorded error; on a session error the binding driver owes the serial
// replay of the captured input when exact multi-defect parity with serial
// COPY is required (v2-14 #631's replay law) — single-defect inputs are
// exactly parity by the ordinal selection alone (witnessed in
// `tests/par_determinism.rs`).
