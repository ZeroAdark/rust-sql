//! Order-preserving parallel COPY FROM parquet into pgrcolumnar (M4X
//! parquet-parallel lane — feeds the M4X-S 100m re-cut).
//!
//! ## Shape (the premise-measured composition)
//!
//! The serial parquet→pgrc wall is ~85% pgrc write work (shred, dict,
//! meta builders, bank hash, verify, seal), ~9% parquet decode, ~5% row
//! ceremony (laptop attribution 2026-08-10; the #521 "parallelism buys
//! nothing" verdict was measured on the TSV cell whose text front-end was
//! 41% of wall, and inverts here). Both big terms parallelize on the
//! rtpool with no new thread population:
//!
//!  1. DECODE (rtpool tasks): row groups are the parallel unit — footer
//!     per-chunk offsets are already parsed, and workers open
//!     [`parquet_read::RowGroupReader`]s over a dup'd file handle
//!     (positioned reads only). Decode + [`RawCell`] image building is
//!     CATALOG-FREE (pure functions of batch bytes; all word arithmetic is
//!     `const fn` datum constructors), so decode tasks need no backend
//!     identity — unlike the v1 pipeline's full-identity helpers.
//!     Claims ride a [`runtime::StreamSource`] watermark the LEADER
//!     advances as it consumes, which bounds decoded-RG memory to the
//!     admission window; workers that find nothing claimable return to the
//!     pool (never block).
//!  2. INGEST (rtpool tasks): the decoded rows feed
//!     [`pgrc_ingest_par`]'s chunk protocol through the ONE production
//!     leader face ([`ParSession::append_row`]); normalize + assemble +
//!     seal parallelize behind the frozen `seal_part` boundary exactly as
//!     the scaling e2e measures.
//!
//! ## ORDER LAW (part-layout reproducibility)
//!
//! Insertion order is IDENTICAL to the source: workers decode row groups
//! in any schedule, but the leader consumes decoded RGs strictly in file
//! order and appends rows one by one. The engine's cut cursor consumes
//! chunk stats in input order with the capture grain pinned to
//! `cut_granule_rows` (enforced by [`ParEngine::new`]; #560 granule gate)
//! and the splice replays the serial pad8 heap arithmetic (#606), so the
//! part partition — and per #560's law, the part BYTES — equal a serial
//! COPY of the same stream. The local oracle: serial-vs-parallel table-dir
//! sha256 at 1m AND 10m, plus the smoke e2e battery.
//!
//! ## Error parity (byte-exact, via serial replay)
//!
//! Workers do not raise user-facing errors. A worker that fails to decode
//! a row group — malformed page, out-of-range value, a NULL in a NOT NULL
//! column — records the failure against the RG's sequence number; the
//! leader, consuming in order, REPLAYS that row group through the serial
//! machinery (`copy_from_parquet_one_row` + `exec_constraints` on a real
//! slot, cur_lineno pre-set to the RG's global row base) and raises
//! whatever it raises — the same message, sqlstate, and line context as a
//! serial COPY, first-error-in-input-order. Any error tears the parallel
//! session down (temps unlinked, nothing published) exactly like a serial
//! abort. Non-data errors (seal I/O, engine contract) surface through the
//! same `write_error` mapping serial ingest uses.
//!
//! ## Admission (FAIL-CLOSED: every refusal is today's serial COPY)
//!
//! ON by default under a live runtime pool (`pgrust.runtime`, on by default);
//! PGRUST_PARQUET_PARALLEL=0 opts out to the serial reader verbatim — no
//! new knobs, and the v1 pgrcolumnar pipeline keeps its own opt-in pair
//! untouched. Then: pgrcolumnar relation;
//! FORMAT 'parquet' file source with >= 2 non-empty row groups; no
//! triggers, indexes, generated columns, CHECK constraints, defaults,
//! WHERE clause, ON_ERROR ignore, or soft-error context; every column
//! listed and every column byval or varlena. Typed refusals for
//! undecodable inputs (unsupported parquet shapes) stay exactly where they
//! are — `open_source`/`resolve_plan` run before this module is consulted.

use std::collections::BTreeMap;
use std::fs::File;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pgsync::{lock, Mutex};

use mcx::{Mcx, MemoryContext};
use types_error::{PgError, PgResult};
use types_rel::Relation;

use backend_progress::pgstat_progress_update_param;
use backend_progress::progress::{
    PROGRESS_COPY_BYTES_PROCESSED, PROGRESS_COPY_TUPLES_PROCESSED,
};

use pgrc_ingest_par::ParSession;
use pgrc_write::held::{cap_bytes, HeldCharge};
use pgrc_write::ingest::RawDatum;
use pgrc_write::par::{ParIngestOpts, RowChunk, DEFAULT_CHUNK_ROWS};
use pgrc_write::WriteError;

use crate::from::{copy_from_error_context, CopyFromState, CopySrc};
use crate::fromparquet::{images_lane, raw_cell, words_lane, ParquetPlan, RawCell};
use crate::parallel::{dop, file_floor, parquet_budget_bytes_auto, pgrc_parquet_parallel_enabled, ptrace, LoadCharge};
use crate::{CopyOnErrorChoice};

macro_rules! refuse {
    ($why:expr) => {{
        ptrace(&format!("pgrc-parquet refused: {}", $why)); // alloc: Cold
        return Ok(None);
    }};
}

/// The feed-side tunnel error: a PgError was stashed on the leader; the
/// engine sees a contract error (cancels + unlinks temps) and the driver
/// re-raises the stashed PgError afterwards.
const STASHED: WriteError = WriteError::Contract {
    detail: "parquet parallel feed error (see raised error)",
};

// ---------------------------------------------------------------------------
// Decoded-RG currency
// ---------------------------------------------------------------------------

/// One decoded column lane, indexed by attidx. `nulls` is row-indexed;
/// cells at null rows carry a zero word / empty span.
pub(crate) enum Lane {
    Words { words: Vec<u64>, nulls: Vec<bool> },
    Blobs { arena: Vec<u8>, spans: Vec<(u32, u32)>, nulls: Vec<bool> },
}

/// A ragged (sub-grain) run of decoded rows, lane-indexed — the leader
/// pumps these through `ParSession::append_row` (partial-chunk state).
pub(crate) struct Fragment {
    /// CONVERT W3-2b: the loading statement's charge on [`Fragment::heap_bytes`] (`DecodeShared::owner`), synced
    /// after each batch's lanes, a block's shrink and the fast fill. Declared first: it drops before the lanes are freed.
    charge: HeldCharge,
    pub(crate) rows: u32,
    /// One lane per attidx (admission requires the full column list).
    pub(crate) lanes: Vec<Lane>,
}

/// CONVERT W3-2b: the heap bytes of decoded lanes, capacity (as [`Fragment::heap_bytes`]).
pub(crate) fn lanes_held(lanes: &[Lane]) -> u64 {
    lanes
        .iter()
        .map(|l| match l {
            Lane::Words { words, nulls } => (words.capacity() * 8 + nulls.capacity()) as u64,
            Lane::Blobs { arena, spans, nulls } => (arena.capacity() + spans.capacity() * 8 + nulls.capacity()) as u64,
        })
        .sum::<u64>()
        + (lanes.len() * core::mem::size_of::<Lane>()) as u64
}

impl Fragment {
    /// Heap bytes held by the lanes (capacity, the residency currency).
    pub(crate) fn heap_bytes(&self) -> u64 {
        self.lanes
            .iter()
            .map(|l| match l {
                Lane::Words { words, nulls } => (words.capacity() * 8 + nulls.capacity()) as u64,
                Lane::Blobs { arena, spans, nulls } => {
                    (arena.capacity() + spans.capacity() * 8 + nulls.capacity()) as u64
                }
            })
            .sum()
    }

    /// Release the growth slack (arenas double while filling; a resident
    /// group pays for its bytes, not its doublings).
    pub(crate) fn shrink(&mut self) {
        for l in self.lanes.iter_mut() {
            match l {
                Lane::Words { words, nulls } => {
                    words.shrink_to_fit(); // alloc: Exec charged self.charge
                    nulls.shrink_to_fit(); // alloc: Exec charged self.charge
                }
                Lane::Blobs { arena, spans, nulls } => {
                    arena.shrink_to_fit(); // alloc: Exec charged self.charge
                    spans.shrink_to_fit(); // alloc: Exec charged self.charge
                    nulls.shrink_to_fit(); // alloc: Exec charged self.charge
                }
            }
        }
        // a shrink gives bytes back: the charge must not keep them
        self.sync_charge();
    }

    /// CONVERT W3-2b: the charge brought to [`Fragment::heap_bytes`] now, on this thread.
    pub(crate) fn sync_charge(&mut self) {
        let held = self.heap_bytes();
        self.charge.sync(held);
    }

    /// `owner`: the loading statement's (`DecodeShared::owner`), charged from the first lane; `None`: uncharged.
    pub(crate) fn empty(natts: usize, byval: &[bool], cap: usize, owner: Option<memledger::Owner>) -> PgResult<Fragment> {
        let mut lanes = ::mcx::stdvec::try_vec(natts, "parquet_pgrc")?;
        for i in 0..natts {
            lanes.push(if byval[i] {
                Lane::Words {
                    words: ::mcx::stdvec::try_vec(cap, "parquet_pgrc")?,
                    nulls: ::mcx::stdvec::try_vec(cap, "parquet_pgrc")?,
                }
            } else {
                Lane::Blobs {
                    arena: Vec::new(),
                    spans: ::mcx::stdvec::try_vec(cap, "parquet_pgrc")?,
                    nulls: ::mcx::stdvec::try_vec(cap, "parquet_pgrc")?,
                }
            });
        }
        let mut f = Fragment { charge: HeldCharge::new(owner), rows: 0, lanes };
        f.sync_charge();
        Ok(f)
    }
}

/// One decoded row group, carved on the GLOBAL chunk grain (the engine's
/// cut granule): `head` = rows before the first aligned boundary inside
/// this RG, `chunks` = full-grain aligned [`RowChunk`]s the workers built
/// OFF the leader (the leader was measured saturated at ~100% of one core
/// pumping these per-row — the whole point of this shape), `tail` = rows
/// after the last aligned boundary. head/tail always END on a grain
/// boundary in the global stream (a tail is continued by the NEXT RG's
/// head, which ends aligned, or by end-of-input), so the leader's partial
/// chunk is provably empty at every `append_chunk`.
pub(crate) struct DecodedRg {
    pub(crate) head: Fragment,
    pub(crate) chunks: Vec<RowChunk>,
    pub(crate) tail: Fragment,
}

enum RgOut {
    Done(DecodedRg),
    /// Worker-side failure. The message is only the fallback for the
    /// replay-succeeded inconsistency arm — the user-facing error comes
    /// from the leader's serial replay.
    Failed(String),
}

/// The decoded-RG handoff board, on the Waiter/eventcount discipline (the
/// runtime/src/rg.rs `Completion` precedent, dst-and-wasm.md #3.3 — NOT a
/// Condvar): the leader registers its waiter handle under the SAME lock
/// that guards `done`, then parks; a worker's insert either sees the
/// registered handle (and unparks it after dropping the lock) or the
/// leader's re-test sees the insert — a wake cannot be lost. Unparks latch
/// in the waiter besides (an unpark landing before the park makes that
/// park return immediately), and the leader's timed park re-tests on a
/// bounded cadence for interrupt polling, which also absorbs wakes for
/// out-of-order seqs (re-test, re-register, re-park).
struct ReadyBoard {
    /// Decoded RGs awaiting the leader, by seq.
    done: BTreeMap<u64, RgOut>,
    /// Registered leader waker words (`waiter::WakerHandle` as u64).
    /// Leader-only waits ⇒ at most one live registration.
    wakers: Vec<u64>,
}

pub(crate) struct DecodeShared {
    pub(crate) file: File,
    pub(crate) path: String,
    pub(crate) meta: Arc<parquet_read::FileMeta>,
    pub(crate) plan: Arc<ParquetPlan>,
    /// Non-empty row groups in file order; decode seq g reads rg_order[g].
    pub(crate) rg_order: memledger::ChargedVec<usize>,
    /// Global first row (0-based) of each seq — the serial cur_lineno base.
    pub(crate) row_base: memledger::ChargedVec<u64>,
    /// Per-attidx facts (attnum order; the full column list is admitted).
    pub(crate) byval: Vec<bool>,
    pub(crate) notnull: Vec<bool>,
    /// The engine chunk grain (== cut granule; `ParEngine::new` re-checks
    /// it against the table policy, `append_chunk` re-checks every chunk).
    pub(crate) grain: u64,
    /// The decoded-RG handoff board (leader-only waits; workers only
    /// notify — see [`ReadyBoard`] for the no-lost-wake discipline).
    ready: Mutex<ReadyBoard>,
    pub(crate) cancelled: AtomicBool,
    /// Compressed chunk bytes read (COPY progress).
    pub(crate) bytes_read: AtomicU64,
    /// D3922: the loading statement's memory account (the leader's, taken where the load is admitted). The pool workers
    /// decode on it, so the decoded groups they allocate and the leader frees are the statement's on the ledger, not
    /// the pool threads' own.
    pub(crate) acct: memledger::Acct,
    /// CONVERT W3-2b: the loading statement's memory owner, read on the leader where the load is admitted (the same
    /// read the writer's open makes, `pgrc_am::ingest::load_owner`): the chunks the pool workers capture are charged
    /// to it.
    pub(crate) owner: Option<memledger::Owner>,
}

impl DecodeShared {
    /// Test face (the fused-decoder identity test): a catalog-free decode
    /// context over one file — every non-empty row group in file order,
    /// the engine grain.
    #[cfg(test)]
    pub(crate) fn standalone(
        file: File,
        path: String,
        meta: Arc<parquet_read::FileMeta>,
        plan: Arc<ParquetPlan>,
        byval: Vec<bool>,
        notnull: Vec<bool>,
    ) -> DecodeShared {
        let mut rg_order = memledger::ChargedVec::new(None, memledger::Kind::Region);
        let mut row_base = memledger::ChargedVec::new(None, memledger::Kind::Region);
        let mut rows = 0u64;
        for (i, rg) in meta.row_groups.iter().enumerate() {
            if rg.num_rows > 0 {
                rg_order.with_vec(|v| v.push(i)); // alloc: Exec charged rg_order.owner() (None here: the standalone probe / test build is uncharged)
                row_base.with_vec(|v| v.push(rows)); // alloc: Exec charged row_base.owner() (None here)
                rows += rg.num_rows as u64;
            }
        }
        DecodeShared {
            file,
            path,
            meta,
            plan,
            rg_order,
            row_base,
            byval,
            notnull,
            grain: DEFAULT_CHUNK_ROWS as u64,
            ready: Mutex::new(ReadyBoard { done: BTreeMap::new(), wakers: Vec::new() }),
            cancelled: AtomicBool::new(false),
            bytes_read: AtomicU64::new(0),
            acct: memledger::current(),
            owner: None,
        }
    }

    /// Append batch rows `k0..k1` to a fragment's lanes (column-major, the
    /// ragged-run currency).
    pub(crate) fn lanes_append(
        &self,
        frag: &mut Fragment,
        batches: &[parquet_read::ColumnBatch],
        k0: usize,
        k1: usize,
    ) -> PgResult<()> {
        // CONVERT W3-2b: the batch's growth is charged once it is in
        let r = self.lanes_fill(frag, batches, k0, k1);
        frag.sync_charge();
        r
    }

    fn lanes_fill(
        &self,
        frag: &mut Fragment,
        batches: &[parquet_read::ColumnBatch],
        k0: usize,
        k1: usize,
    ) -> PgResult<()> {
        for b in self.plan.bindings.iter() {
            let batch = &batches[b.batch];
            let notnull = self.notnull[b.attidx];
            match &mut frag.lanes[b.attidx] {
                Lane::Words { words, nulls } => words_lane(b.conv, batch, k0, k1, notnull, words, nulls)?,
                Lane::Blobs { arena, spans, nulls } => {
                    images_lane(b.conv, batch, k0, k1, notnull, self.plan.vutf8[b.batch], arena, spans, nulls)?
                }
            }
        }
        frag.rows += (k1 - k0) as u32;
        Ok(())
    }

    /// Append batch rows `k0..k1` ROW-major into the current chunk (the
    /// aligned fast path: the per-row pump the leader used to run, moved
    /// onto the decode worker). Two passes per row — cells staged as spans
    /// against a per-row scratch arena, then the borrow-shaped
    /// [`RawDatum`] row is pushed — because `raw_cell` needs the arena
    /// mutable while `RawDatum::Bytes` borrows it.
    #[allow(clippy::too_many_arguments)]
    fn chunk_append(
        &self,
        cur: &mut RowChunk,
        chunks: &mut Vec<RowChunk>,
        scratch: &mut Vec<u8>,
        staged: &mut Vec<RawCell>,
        row: &mut Vec<RawDatum<'static>>,
        batches: &[parquet_read::ColumnBatch],
        k0: usize,
        k1: usize,
    ) -> PgResult<()> {
        const NULL_SENTINEL: RawCell = RawCell::Bytes { off: u32::MAX, len: 0 };
        let natts = self.byval.len();
        let grain = self.grain as u32;
        for k in k0..k1 {
            scratch.clear();
            staged.clear();
            ::mcx::stdvec::try_resize(staged, natts, NULL_SENTINEL, "parquet_pgrc")?;
            for b in self.plan.bindings.iter() {
                let batch = &batches[b.batch];
                if batch.is_null(k) {
                    if self.notnull[b.attidx] {
                        return Err(Box::new(PgError::error( // alloc: Cold
                            "not-null violation (serial replay owns the message)".to_string(), // alloc: Cold
                        )));
                    }
                    // NULL_SENTINEL already staged.
                } else {
                    let cell = raw_cell(b.conv, batch, k, scratch)?;
                    if self.byval[b.attidx] != matches!(cell, RawCell::Word(_)) {
                        return Err(Box::new(PgError::error( // alloc: Cold
                            "parquet lane shape does not match column byval".to_string(), // alloc: Cold
                        )));
                    }
                    staged[b.attidx] = cell;
                }
            }
            row.clear();
            for cell in staged.iter() {
                row.push(match *cell {
                    RawCell::Word(w) => RawDatum::Word(w),
                    RawCell::Bytes { off, len } => {
                        if off == u32::MAX && len == 0 {
                            RawDatum::Null
                        } else {
                            // SAFETY (lifetime erasure): the slice borrows
                            // `scratch`, which outlives `row`'s use below —
                            // `push_row` copies the bytes into the chunk
                            // arena before this loop iteration ends, and
                            // `row` is cleared before `scratch` mutates.
                            let s: &[u8] =
                                &scratch[off as usize..(off + len) as usize];
                            RawDatum::Bytes(unsafe {
                                core::mem::transmute::<&[u8], &'static [u8]>(s)
                            })
                        }
                    }
                });
            }
            cur.push_row(row).map_err(|we| {
                Box::new(PgError::error(format!( // alloc: Cold
                    "parquet parallel chunk build failed: {we}"
                )))
            })?;
            row.clear();
            if cur.rows() >= grain {
                let full = core::mem::replace(cur, RowChunk::with_owner(natts, grain, self.owner));
                chunks.push(full);
            }
        }
        Ok(())
    }

    /// Decode one whole row group into ONE lane-major fragment (the
    /// cluster merge's currency, `parquet_cluster.rs`: rows are gathered
    /// by index in key order, so the file-order carve is meaningless
    /// here). `rg_idx` is the FILE index. Catalog-free like [`Self::decode`].
    pub(crate) fn decode_whole(&self, rg_idx: usize) -> PgResult<Fragment> {
        let mut rgr = parquet_read::RowGroupReader::open_on(
            &self.file,
            &self.path,
            &self.meta,
            rg_idx,
            &self.plan.cols,
            &self.plan.vutf8,
            self.owner,
        )?;
        self.bytes_read.fetch_add(rgr.compressed_bytes, Ordering::Relaxed);
        let rows_total = rgr.rows_remaining();
        let natts = self.byval.len();
        let mut frag = Fragment::empty(natts, &self.byval, rows_total as usize, self.owner)?;
        let mut batches = self.plan.make_batches(&self.meta);
        // CONVERT wave 3 (cvwrite job 3): the batches' buffers on the load's owner, brought to them after each fill on
        // this (the decoding) thread; declared after them, so it drops first
        let mut batch_charge = HeldCharge::new(self.owner);
        const BATCH_ROWS: u64 = 1024;
        while rgr.rows_remaining() > 0 {
            if self.cancelled.load(Ordering::SeqCst) {
                return Err(Box::new(
                    PgError::error("parquet parallel decode cancelled".to_string()), // alloc: Cold
                ));
            }
            let n = rgr.rows_remaining().min(BATCH_ROWS) as usize;
            rgr.read_batches(&mut batches, n)?;
            batch_charge.sync(parquet_read::batches_held(&batches));
            self.lanes_append(&mut frag, &batches, 0, n)?;
        }
        debug_assert_eq!(u64::from(frag.rows), rows_total);
        Ok(frag)
    }

    /// Decode ONLY the given bindings of one row group (the cluster
    /// merge's key pre-pass: the composite-key bounds of every row group
    /// come from the real conversions, never from footer statistics).
    /// Returns one word lane per binding, in `bindings` order; a NULL
    /// under NOT NULL is the same detection-only error as [`Self::decode`].
    /// CONVERT W3-2b: `charge` (the caller's, on the load's owner) is brought to the key lanes' bytes as soon as they
    /// are reserved (whole row group, up front); the caller keeps it beside the lanes and drops it first.
    pub(crate) fn decode_words(
        &self,
        rg_idx: usize,
        bindings: &[usize],
        charge: &mut HeldCharge,
    ) -> PgResult<Vec<(Vec<u64>, Vec<bool>)>> {
        let cols: Vec<usize> = bindings.iter().map(|&b| self.plan.cols[self.plan.bindings[b].batch]).collect();
        let vutf8: Vec<bool> = bindings.iter().map(|&b| self.plan.vutf8[self.plan.bindings[b].batch]).collect();
        let mut rgr = parquet_read::RowGroupReader::open_on(
            &self.file, &self.path, &self.meta, rg_idx, &cols, &vutf8, self.owner,
        )?;
        let rows_total = rgr.rows_remaining() as usize;
        let mut out: Vec<(Vec<u64>, Vec<bool>)> = ::mcx::stdvec::try_vec(bindings.len(), "parquet_pgrc")?;
        for _ in bindings.iter() {
            out.push((::mcx::stdvec::try_vec(rows_total, "parquet_pgrc")?, ::mcx::stdvec::try_vec(rows_total, "parquet_pgrc")?)); // alloc: Exec charged charge (the caller's)
        }
        charge.sync(out.iter().map(|(w, n)| cap_bytes(w.capacity(), w) + cap_bytes(n.capacity(), n)).sum());
        let mut batches: Vec<parquet_read::ColumnBatch> =
            cols.iter().map(|&c| parquet_read::ColumnBatch::new_for(self.meta.columns[c].phys)).collect();
        // CONVERT wave 3 (cvwrite job 3): the batches' buffers on the load's owner, brought to them after each fill on
        // this (the decoding) thread; declared after them, so it drops first
        let mut batch_charge = HeldCharge::new(self.owner);
        const BATCH_ROWS: u64 = 1024;
        while rgr.rows_remaining() > 0 {
            if self.cancelled.load(Ordering::SeqCst) {
                return Err(Box::new(
                    PgError::error("parquet parallel decode cancelled".to_string()), // alloc: Cold
                ));
            }
            let n = rgr.rows_remaining().min(BATCH_ROWS) as usize;
            rgr.read_batches(&mut batches, n)?;
            batch_charge.sync(parquet_read::batches_held(&batches));
            for (j, &b) in bindings.iter().enumerate() {
                let bind = &self.plan.bindings[b];
                let (words, nulls) = &mut out[j];
                words_lane(bind.conv, &batches[j], 0, n, self.notnull[bind.attidx], words, nulls)?;
            }
        }
        Ok(out)
    }

    /// Decode one row group, carved on the global chunk grain (worker
    /// body; catalog-free). The batch path: the fused decoder's fallback
    /// ([`Self::decode_fast`]) and the error path of record.
    pub(crate) fn decode(&self, seq: u64) -> PgResult<DecodedRg> {
        let rg_idx = self.rg_order[seq as usize];
        let mut rgr = parquet_read::RowGroupReader::open_on(
            &self.file,
            &self.path,
            &self.meta,
            rg_idx,
            &self.plan.cols,
            &self.plan.vutf8,
            self.owner,
        )?;
        self.bytes_read.fetch_add(rgr.compressed_bytes, Ordering::Relaxed);
        let rows_total = rgr.rows_remaining();
        let natts = self.byval.len();
        let grain = self.grain;
        // Global carve: [base, head_end) ragged, [head_end, full_end)
        // whole aligned chunks, [full_end, end) ragged.
        let base = self.row_base[seq as usize];
        let end = base + rows_total;
        let head_end = end.min(base.div_ceil(grain) * grain);
        let full_end = head_end + (end - head_end) / grain * grain;
        let mut head =
            Fragment::empty(natts, &self.byval, (head_end - base) as usize, self.owner)?;
        let mut tail = Fragment::empty(natts, &self.byval, (end - full_end) as usize, self.owner)?;
        let mut chunks: Vec<RowChunk> =
            ::mcx::stdvec::try_vec(((full_end - head_end) / grain) as usize, "parquet_pgrc")?;
        let mut cur = RowChunk::with_owner(natts, grain as u32, self.owner);
        let mut scratch: Vec<u8> = Vec::new();
        let mut staged: Vec<RawCell> = Vec::new();
        let mut row: Vec<RawDatum<'static>> = ::mcx::stdvec::try_vec(natts, "parquet_pgrc")?;
        let mut batches = self.plan.make_batches(&self.meta);
        // CONVERT wave 3 (cvwrite job 3): the batches' buffers on the load's owner, brought to them after each fill on
        // this (the decoding) thread; declared after them, so it drops first
        let mut batch_charge = HeldCharge::new(self.owner);
        const BATCH_ROWS: u64 = 1024;
        let mut cursor = base;
        while rgr.rows_remaining() > 0 {
            if self.cancelled.load(Ordering::SeqCst) {
                // Session already failed; produce a placeholder failure —
                // the leader never consumes past its error.
                return Err(Box::new(
                    PgError::error("parquet parallel decode cancelled".to_string()), // alloc: Cold
                ));
            }
            let n = rgr.rows_remaining().min(BATCH_ROWS) as usize;
            rgr.read_batches(&mut batches, n)?;
            batch_charge.sync(parquet_read::batches_held(&batches));
            let bend = cursor + n as u64;
            // Head segment of this batch (ragged, lane-major).
            if cursor < head_end {
                let k1 = (head_end.min(bend) - cursor) as usize;
                self.lanes_append(&mut head, &batches, 0, k1)?;
            }
            // Full-grain segment (row-major into chunks).
            if bend > head_end && cursor < full_end {
                let k0 = (head_end.max(cursor) - cursor) as usize;
                let k1 = (full_end.min(bend) - cursor) as usize;
                self.chunk_append(
                    &mut cur, &mut chunks, &mut scratch, &mut staged, &mut row,
                    &batches, k0, k1,
                )?;
            }
            // Tail segment (ragged, lane-major).
            if bend > full_end {
                let k0 = (full_end.max(cursor) - cursor) as usize;
                self.lanes_append(&mut tail, &batches, k0, n)?;
            }
            cursor = bend;
        }
        debug_assert_eq!(cur.rows(), 0, "aligned carve left a partial chunk");
        debug_assert_eq!(
            u64::from(head.rows)
                + chunks.len() as u64 * grain
                + u64::from(tail.rows),
            rows_total,
        );
        Ok(DecodedRg { head, chunks, tail })
    }
}

struct DecodeWork {
    shared: Arc<DecodeShared>,
}

impl runtime::TaskSetWork for DecodeWork {
    fn run_morsel(&self, _worker: usize, range: runtime::MorselRange) {
        let _acct = memledger::charge::enter_as(memledger::charge::context_of(self.shared.acct).0);
        for seq in range {
            if self.shared.cancelled.load(Ordering::SeqCst) {
                continue;
            }
            // The fused decoder first; any error (an unsupported shape or
            // bad data) re-decodes the row group through the batch path,
            // whose result — rows or error — is the one of record.
            // unwind-ok: worker-containment
            let out = match catch_unwind(AssertUnwindSafe(|| match self.shared.decode_fast(seq) {
                Ok(d) => Ok(d),
                Err(e) => {
                    ptrace(&format!("pgrc-parquet fastdec fallback seq={seq}: {}", e.message())); // alloc: Cold
                    self.shared.decode(seq)
                }
            })) {
                Ok(Ok(d)) => RgOut::Done(d),
                Ok(Err(e)) => RgOut::Failed(e.message().to_string()), // alloc: Cold
                Err(_) => RgOut::Failed("decode worker panicked".to_string()), // alloc: Cold
            };
            let mut g = lock(&self.shared.ready);
            g.done.insert(seq, out);
            let wakers = core::mem::take(&mut g.wakers);
            drop(g);
            for w in wakers {
                let _ = waiter::unpark_word(w);
            }
        }
    }

    fn finalize(&self) {}
}

// ---------------------------------------------------------------------------
// Admission
// ---------------------------------------------------------------------------

struct Admitted {
    rt: &'static Arc<runtime::Runtime>,
    shared: Arc<DecodeShared>,
    /// Decoded-RG read-ahead bound (claim watermark = consumed + window).
    window: u64,
    total_rows: u64,
    /// D3343: the decode window's planned bytes (window x the 4x-expanded largest row group).
    resident: u64,
}

fn admit<'mcx>(
    cstate: &CopyFromState<'mcx, '_>,
    rel: &Relation<'mcx>,
    has_triggers: bool,
) -> PgResult<Option<Admitted>> {
    if !pgrc_parquet_parallel_enabled() || !runtime::runtime_enabled() {
        return Ok(None);
    }
    if !cstate.opts.parquet {
        return Ok(None);
    }
    if tableam_vocab::TableAm::of(rel) != Some(tableam_vocab::TableAm::Pgrcolumnar) {
        return Ok(None);
    }
    // RL OC-4 (a): the parallel feed never reaches the tableam bulk arm, which logs BULK_ROWS.
    if pgrc_front::logical::active(rel) {
        refuse!("wal_level=logical: bulk rows are logged by the serial path");
    }
    let Some(rt) = runtime::global() else { refuse!("no runtime pool") };
    if parallel::IsParallelWorker() || !init_small::globals::IsUnderPostmaster() {
        refuse!("not a postmaster session leader");
    }
    if has_triggers {
        refuse!("relation has triggers");
    }
    if rel.rd_rel.relhasindex {
        refuse!("relation has indexes");
    }
    if let Some(c) = rel.rd_att.constr.as_deref() {
        if c.has_generated_stored || c.has_generated_virtual {
            refuse!("generated columns");
        }
        if c.num_check > 0 {
            refuse!("CHECK constraints (worker decode is catalog-free)");
        }
    }
    let o = &cstate.opts;
    if o.on_error != CopyOnErrorChoice::Stop {
        refuse!("ON_ERROR ignore");
    }
    if o.default_print.is_some() || !cstate.defmap.is_empty() || cstate.volatile_defexprs {
        refuse!("column defaults");
    }
    if cstate.convert_select_flags.is_some() {
        refuse!("convert_select");
    }
    if !cstate.where_clause.is_nil() {
        refuse!("WHERE clause");
    }
    if cstate.need_transcoding {
        refuse!("encoding conversion");
    }
    if cstate.escontext.is_some() {
        refuse!("soft-error context");
    }
    let natts = rel.rd_att.natts as usize;
    if cstate.attnumlist.len() != natts {
        refuse!("partial column list");
    }
    // Per-attidx facts. Every admitted class is byval or varlena — the raw
    // capture currency; anything else (cstring/fixed-byref) refuses.
    let mut byval = ::mcx::stdvec::try_filled(natts, false, "parquet_pgrc")?;
    let mut notnull = ::mcx::stdvec::try_filled(natts, false, "parquet_pgrc")?;
    for (i, att) in rel.rd_att.attrs.iter().enumerate() {
        byval[i] = att.attbyval;
        notnull[i] = att.attnotnull;
        if !att.attbyval && att.attlen != -1 {
            refuse!("fixed-length by-reference column");
        }
    }
    let CopySrc::Parquet(psrc) = &cstate.src else {
        refuse!("parquet source not initialized");
    };
    let reader = psrc.reader();
    if reader.file_len() < file_floor() {
        refuse!(format!("parquet file smaller than the {}B floor", file_floor())); // alloc: Cold
    }
    let meta = reader.meta_arc();
    let plan = psrc.plan_arc();
    if plan.bindings.iter().any(|b| b.conv.serial_only()) {
        refuse!("a column conversion that needs the serial path (numeric, uuid, interval, json, arrays)");
    }
    // CONVERT c3: the per-row-group index and row-base tables (16 B per kept row group) on the load's owner for the pass's life
    let owner = pgrc_am::ingest::load_owner();
    let mut rg_order = memledger::ChargedVec::new(owner, memledger::Kind::Region);
    let mut row_base = memledger::ChargedVec::new(owner, memledger::Kind::Region);
    let mut rows = 0u64;
    let mut max_rg_bytes = 0u64;
    for (i, rg) in meta.row_groups.iter().enumerate() {
        if rg.num_rows == 0 {
            continue;
        }
        rg_order.with_vec(|v| v.push(i)); // alloc: Exec charged rg_order.owner()
        row_base.with_vec(|v| v.push(rows)); // alloc: Exec charged row_base.owner()
        rows += rg.num_rows as u64;
        max_rg_bytes = max_rg_bytes.max(parquet_read::rg_compressed_bytes(&meta, i, &plan.cols));
    }
    if rg_order.len() < 2 {
        refuse!("parquet file with fewer than 2 row groups (serial reader)");
    }
    let file = match reader.try_clone_file() {
        Ok(f) => f,
        Err(_) => refuse!("could not duplicate the parquet file handle"),
    };
    // Decoded read-ahead bound: decoded lanes are roughly the uncompressed
    // size; the footer only carries compressed bytes, so budget with a 4x
    // decode-expansion estimate against PGRUST_PARQUET_BUDGET_MB (the v1
    // knob, reused — zero new knobs).
    let k = dop(rt) as u64;
    let budget = parquet_budget_bytes_auto(rt.config().workers);
    let by_budget = (budget / (4 * max_rg_bytes.max(1))).max(1);
    let window = (2 * k + 2).min(by_budget).max(2);
    ptrace(&format!( // alloc: Cold
        "pgrc-parquet parallel admitted rgs={} rows={rows} max_rg_mb={} dop={k} budget_mb={} window={window}",
        rg_order.len(),
        max_rg_bytes >> 20,
        budget >> 20,
    ));
    let resident = window.saturating_mul(4 * max_rg_bytes);
    Ok(Some(Admitted {
        rt,
        resident,
        shared: Arc::new(DecodeShared {
            file,
            path: reader.path().to_string(),
            meta,
            plan,
            rg_order,
            row_base,
            byval,
            notnull,
            grain: DEFAULT_CHUNK_ROWS as u64,
            ready: Mutex::new(ReadyBoard {
                done: BTreeMap::new(),
                wakers: Vec::new(),
            }),
            cancelled: AtomicBool::new(false),
            bytes_read: AtomicU64::new(0),
            acct: memledger::current(),
            owner,
        }),
        window,
        total_rows: rows,
    }))
}

// ---------------------------------------------------------------------------
// Serial replay (the byte-exact error path)
// ---------------------------------------------------------------------------

/// Replay row group `seq` through the SERIAL machinery to raise the exact
/// serial error (message, sqlstate, COPY line context). `Ok(())` = the
/// replay found no error — the caller raises the inconsistency.
pub(crate) fn replay_rg_serial<'mcx>(
    mcx: Mcx<'mcx>,
    cstate: &mut CopyFromState<'mcx, '_>,
    rel: &Relation<'mcx>,
    rg_raw_idx: usize,
    lineno_base: u64,
    rg_rows: u64,
) -> PgResult<()> {
    let mut slot = tableam::table_slot_create(mcx, rel)?;
    let inserted_cols = {
        const FLIHAN: i32 = types_tuple::htup::FirstLowInvalidHeapAttributeNumber;
        let mut b = types_nodes::Bitmapset::empty();
        for &a in cstate.attnumlist.iter() {
            b.add_member(mcx, a as i32 - FLIHAN)?;
        }
        b
    };
    let mut check_exprs = None;
    let mut virtual_nn = None;
    {
        let CopySrc::Parquet(src) = &mut cstate.src else {
            return Ok(());
        };
        src.seek_rg(rg_raw_idx);
    }
    cstate.cur_lineno = lineno_base;
    let mut row_cx = MemoryContext::new_bump("PgrcParquetReplayRow");
    for _ in 0..rg_rows {
        row_cx.reset();
        exectuples::exec_clear_tuple(&mut slot, mcx);
        // SAFETY (lifetime erasure): per-row datums land in row_cx and are
        // dead before the next reset — the v1 parquet worker's exact
        // contract (parallel.rs parquet_decode_rg).
        let row_mcx: Mcx<'_> = unsafe { core::mem::transmute(row_cx.mcx()) };
        let more = {
            let base = slot.base_mut();
            cstate.copy_from_parquet_one_row(row_mcx, &mut base.tts_values, &mut base.tts_isnull)
        }
        .map_err(|e| copy_from_error_context(cstate, e))?;
        if !more {
            break;
        }
        exectuples::exec_store_virtual_tuple(&mut slot);
        slot.base_mut().tts_tableOid = rel.rd_id;
        nodemodifytable::exec_constraints(
            mcx,
            &mut check_exprs,
            &mut virtual_nn,
            rel,
            &mut slot,
            None,
            Some(&inserted_cols),
        )
        .map_err(|e| copy_from_error_context(cstate, e))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------

/// Cancel + drain the decode task set on EVERY exit path: remaining claims
/// become no-ops, the stream closes, and the leader waits the runtime
/// drain so no task can touch shared state after the statement unwinds.
struct DecodeDrain<'a> {
    shared: &'a Arc<DecodeShared>,
    stream: &'a Arc<runtime::StreamSource>,
    rt: &'a Arc<runtime::Runtime>,
    waiter: Option<runtime::CompletionWaiter>,
}

impl Drop for DecodeDrain<'_> {
    fn drop(&mut self) {
        self.shared.cancelled.store(true, Ordering::SeqCst);
        self.stream.close();
        self.rt.notify_source_progress();
        if let Some(w) = self.waiter.take() {
            let _ = w.wait();
        }
    }
}

/// Order-preserving parallel COPY FROM parquet into pgrcolumnar.
/// `Ok(None)` = refused, run the serial path (cstate untouched).
/// `Ok(Some(n))` = n rows loaded, sealed, deposited AND published (the
/// publish is `table_finish_bulk_insert` → pgrc_am `finish_bulk`,
/// identical to serial). Errors return FULLY CONTEXTED.
pub(crate) fn copy_from_parquet_pgrc<'mcx>(
    mcx: Mcx<'mcx>,
    cstate: &mut CopyFromState<'mcx, '_>,
    rel: &Relation<'mcx>,
    has_triggers: bool,
    ti_options: i32,
) -> PgResult<Option<u64>> {
    let Some(adm) = admit(cstate, rel, has_triggers)? else {
        return Ok(None);
    };
    let Admitted { rt, shared, window, total_rows, resident } = adm;
    // A declared `sorted_by` routes to the cluster merge (global key order
    // across parts, parquet_cluster.rs); its own admission may hand the
    // statement back to the file-order path below.
    if let Some(n) = crate::parquet_cluster::copy_clustered(
        mcx, cstate, rel, ti_options, rt, &shared, total_rows,
    )? {
        return Ok(Some(n));
    }
    let _resident = LoadCharge::new(resident);
    let n_rgs = shared.rg_order.len() as u64;

    // Submit the decode task set as its own resource group; claims open as
    // the leader's consumption advances the watermark.
    static QUERY_NONCE: AtomicU64 = AtomicU64::new(1);
    let stream = Arc::new(runtime::StreamSource::new());
    let (_rg, waiter) = rt.submit(runtime::QuerySpec {
        query_id: QUERY_NONCE.fetch_add(1, Ordering::SeqCst),
        tasksets: vec![runtime::TaskSetSpec {
            source: Arc::clone(&stream) as Arc<dyn runtime::MorselSource>,
            work: Arc::new(DecodeWork {
                shared: Arc::clone(&shared),
            }),
            deps: Vec::new(),
        }],
    });
    let mut drain = DecodeDrain {
        shared: &shared,
        stream: &stream,
        rt,
        waiter: Some(waiter),
    };
    stream.publish(window.min(n_rgs));
    rt.notify_source_progress();

    // The stashed user-facing error (feed tunnels WriteError::Contract).
    let mut stash: Option<Box<PgError>> = None;

    let fed: PgResult<u64> = pgrc_am::ingest::with_parallel_copy_writer(rel, |writer, providers| {
        let r = pgrc_ingest_par::parallel_ingest(
            writer,
            rt,
            providers,
            ParIngestOpts::auto(),
            |sess| {
                let fed = feed_rows(
                    mcx, cstate, rel, &shared, &stream, rt, n_rgs, sess, &mut stash,
                );
                let (peak, waits, ms) = sess.memory_report();
                ptrace(&format!("pgrc-parquet memory peak_held={peak} gate_waits={waits} gate_ms={ms}")); // alloc: Cold
                fed
            },
        );
        r.map_err(|we| stash.take().unwrap_or_else(|| pgrc_am::write_error(we)))
    });
    // Drain the decode task set BEFORE publish/return on every path.
    drain.waiter.take().map(|w| {
        shared.cancelled.store(true, Ordering::SeqCst);
        stream.close();
        rt.notify_source_progress();
        w.wait()
    });
    let processed = fed?;
    debug_assert_eq!(processed, total_rows);

    // Publish through the serial statement-end path: pgrc_am::finish_bulk
    // (fence armed + witnessed, publish lock, DEBUG1 outcome) — UNCHANGED.
    tableam::table_finish_bulk_insert(rel, ti_options)?;
    pgstat_progress_update_param(PROGRESS_COPY_TUPLES_PROCESSED, processed as i64);
    ptrace(&format!("pgrc-parquet parallel done rows={processed}")); // alloc: Cold
    Ok(Some(processed))
}

/// Pump one ragged fragment through `ParSession::append_row` (the
/// partial-chunk row path; fragments are sub-grain by construction).
fn pump_fragment(
    frag: &Fragment,
    natts: usize,
    sess: &mut ParSession<'_>,
    processed: &mut u64,
    stash: &mut Option<Box<PgError>>,
) -> Result<(), WriteError> {
    let mut row: Vec<RawDatum<'_>> = pgrc_write::format::try_vec(natts, "parquet_pgrc")?;
    for k in 0..frag.rows as usize {
        row.clear();
        for lane in frag.lanes.iter().take(natts) {
            row.push(match lane {
                Lane::Words { words, nulls } => {
                    if nulls[k] {
                        RawDatum::Null
                    } else {
                        RawDatum::Word(words[k])
                    }
                }
                Lane::Blobs { arena, spans, nulls } => {
                    if nulls[k] {
                        RawDatum::Null
                    } else {
                        let (off, len) = spans[k];
                        RawDatum::Bytes(&arena[off as usize..(off + len) as usize])
                    }
                }
            });
        }
        sess.append_row(&row)?;
        *processed += 1;
        if processed.is_multiple_of(8192) {
            if let Err(e) = postgres_seams::check_for_interrupts::call() {
                *stash = Some(e);
                return Err(STASHED);
            }
            pgstat_progress_update_param(
                PROGRESS_COPY_TUPLES_PROCESSED,
                *processed as i64,
            );
        }
    }
    Ok(())
}

/// The leader feed: consume decoded RGs strictly in input order, append
/// rows through the ONE production capture face. Returns the row count.
#[allow(clippy::too_many_arguments)]
fn feed_rows<'mcx>(
    mcx: Mcx<'mcx>,
    cstate: &mut CopyFromState<'mcx, '_>,
    rel: &Relation<'mcx>,
    shared: &Arc<DecodeShared>,
    stream: &Arc<runtime::StreamSource>,
    rt: &Arc<runtime::Runtime>,
    n_rgs: u64,
    sess: &mut ParSession<'_>,
    stash: &mut Option<Box<PgError>>,
) -> Result<u64, WriteError> {
    let natts = shared.byval.len();
    let window = stream.watermark();
    ptrace(&format!( // alloc: Cold
        "pgrc-parquet parallel bounds parts_in_flight={} chunks_in_flight={} decode_window={window}",
        sess.opts().max_parts_in_flight,
        sess.opts().max_chunks_in_flight
    ));
    // Decode-stream completion: once the watermark reaches n_rgs nothing
    // more will EVER be published — close the stream right then (claims
    // survive a close; only exhaustion needs it), so the decode task set
    // EXHAUSTS and its slot retires as soon as the last RG is decoded,
    // instead of sitting starved-open for the rest of the statement. A
    // starved-open slot is dead weight the scheduler must probe around
    // (see sched.rs's STARVED-SHADOW fix — this lane measured the 1m
    // deadlock that motivated both changes).
    let mut published_hi = window.min(n_rgs);
    let mut decode_closed = false;
    if published_hi >= n_rgs {
        stream.close();
        decode_closed = true;
        rt.notify_source_progress();
    }
    let mut processed = 0u64;
    for seq in 0..n_rgs {
        // Wait for seq's decode (leader-only wait; workers notify).
        // Waiter park per the rg.rs Completion precedent: register the
        // handle under the board lock (no lost wake — see [`ReadyBoard`]),
        // park with the 50ms interrupt-poll cadence, re-test on Notified /
        // TimedOut / Recheck alike.
        let out = loop {
            {
                let mut g = lock(&shared.ready);
                if let Some(o) = g.done.remove(&seq) {
                    break o;
                }
                let h = waiter::current_handle().as_u64();
                if !g.wakers.contains(&h) {
                    g.wakers.push(h);
                }
            }
            let _ = waiter::park_timeout(Duration::from_millis(50));
            if let Err(e) = postgres_seams::check_for_interrupts::call() {
                *stash = Some(e);
                return Err(STASHED);
            }
        };
        // Open the next claim as this one is consumed (bounded read-ahead);
        // never publish past a close (the stream contract: closed means
        // "the watermark is final").
        let target = (seq + 1 + window).min(n_rgs);
        if target > published_hi {
            stream.publish(target);
            published_hi = target;
            if !decode_closed && published_hi >= n_rgs {
                stream.close();
                decode_closed = true;
            }
            rt.notify_source_progress();
        }
        match out {
            RgOut::Failed(detail) => {
                // Byte-exact error parity: replay THIS row group serially.
                let rg_raw = shared.rg_order[seq as usize];
                let base = shared.row_base[seq as usize];
                let rows = shared.meta.row_groups[rg_raw].num_rows as u64;
                let e = match replay_rg_serial(mcx, cstate, rel, rg_raw, base, rows) {
                    Err(e) => e,
                    Ok(()) => Box::new(PgError::error(format!( // alloc: Cold
                        "parquet parallel worker failed but the serial replay \
                         found no error: {detail}"
                    ))),
                };
                *stash = Some(e);
                return Err(STASHED);
            }
            RgOut::Done(decoded) => {
                // Ragged head rows through the row pump (partial-chunk
                // state), then the worker-built aligned chunks handed
                // through whole (the leader's per-row pump was the
                // measured throughput ceiling at ~100% of one core), then
                // the ragged tail. Fragment/chunk order preserves the
                // global row order exactly.
                pump_fragment(&decoded.head, natts, sess, &mut processed, stash)?;
                for chunk in decoded.chunks {
                    let rows = u64::from(chunk.rows());
                    sess.append_chunk(chunk)?;
                    processed += rows;
                    if let Err(e) = postgres_seams::check_for_interrupts::call() {
                        *stash = Some(e);
                        return Err(STASHED);
                    }
                    pgstat_progress_update_param(
                        PROGRESS_COPY_TUPLES_PROCESSED,
                        processed as i64,
                    );
                }
                pump_fragment(&decoded.tail, natts, sess, &mut processed, stash)?;
                // COPY byte progress = compressed bytes decoded so far
                // (the serial reader's exact currency).
                let bytes = shared.bytes_read.load(Ordering::Relaxed);
                if bytes != cstate.bytes_processed {
                    cstate.bytes_processed = bytes;
                    pgstat_progress_update_param(
                        PROGRESS_COPY_BYTES_PROCESSED,
                        bytes as i64,
                    );
                }
            }
        }
    }
    Ok(processed)
}

#[cfg(test)]
mod lanes_tests {
    use std::sync::Arc;

    use types_core::Oid;

    use super::*;
    use crate::fromparquet::resolve_plan_types;

    /// The per-value lanes build `lanes_append` replaced (`raw_cell` per
    /// cell, row by row within a column).
    fn lanes_by_cell(sh: &DecodeShared, frag: &mut Fragment, batches: &[parquet_read::ColumnBatch], n: usize) -> PgResult<()> {
        let shape = || Box::new(PgError::error("shape".to_string()));
        for b in sh.plan.bindings.iter() {
            let batch = &batches[b.batch];
            for k in 0..n {
                let null = batch.is_null(k);
                if null && sh.notnull[b.attidx] {
                    return Err(shape());
                }
                match &mut frag.lanes[b.attidx] {
                    Lane::Words { words, nulls } => {
                        nulls.push(null);
                        words.push(if null {
                            0
                        } else {
                            match raw_cell(b.conv, batch, k, &mut Vec::new())? {
                                RawCell::Word(w) => w,
                                RawCell::Bytes { .. } => return Err(shape()),
                            }
                        });
                    }
                    Lane::Blobs { arena, spans, nulls } => {
                        nulls.push(null);
                        spans.push(if null {
                            (0, 0)
                        } else {
                            match raw_cell(b.conv, batch, k, arena)? {
                                RawCell::Bytes { off, len } => (off, len),
                                RawCell::Word(_) => return Err(shape()),
                            }
                        });
                    }
                }
            }
        }
        frag.rows += n as u32;
        Ok(())
    }

    /// Lanes as values: words with nulls, images resolved through the arena.
    fn values(f: &Fragment) -> Vec<Vec<Option<Vec<u8>>>> {
        f.lanes
            .iter()
            .map(|l| match l {
                Lane::Words { words, nulls } => words
                    .iter()
                    .zip(nulls)
                    .map(|(w, &n)| (!n).then(|| w.to_le_bytes().to_vec()))
                    .collect(),
                Lane::Blobs { arena, spans, nulls } => spans
                    .iter()
                    .zip(nulls)
                    .map(|(&(o, l), &n)| (!n).then(|| arena[o as usize..(o + l) as usize].to_vec()))
                    .collect(),
            })
            .collect()
    }

    fn walk(path: &str, atts: &[(&str, Oid, i32, bool)], coerce: bool, notnull: bool) -> Option<Result<usize, ()>> {
        let reader = parquet_read::FileReader::open(path).ok()?;
        let meta = reader.meta_arc();
        let list: Vec<(String, Oid, i32)> = atts.iter().map(|a| (a.0.to_string(), a.1, a.2)).collect();
        let plan = Arc::new(resolve_plan_types(&meta, &list, coerce).ok()?);
        let file = reader.try_clone_file().ok()?;
        let byval: Vec<bool> = atts.iter().map(|a| a.3).collect();
        let sh = DecodeShared::standalone(file, path.to_string(), meta, plan, byval.clone(), vec![notnull; atts.len()]);
        let mut rows = 0usize;
        for &rg in sh.rg_order.iter() {
            let mut rgr = parquet_read::RowGroupReader::open(&sh.file, &sh.path, &sh.meta, rg, &sh.plan.cols, &sh.plan.vutf8).ok()?;
            let mut batches = sh.plan.make_batches(&sh.meta);
            let mut a = Fragment::empty(atts.len(), &byval, 0, None).ok()?;
            let mut b = Fragment::empty(atts.len(), &byval, 0, None).ok()?;
            while rgr.rows_remaining() > 0 {
                let n = rgr.rows_remaining().min(700) as usize;
                rgr.read_batches(&mut batches, n).ok()?;
                let ra = sh.lanes_append(&mut a, &batches, 0, n);
                let rb = lanes_by_cell(&sh, &mut b, &batches, n);
                match (ra.is_ok(), rb.is_ok()) {
                    (true, true) => {}
                    (false, false) => return Some(Err(())),
                    _ => panic!("{path}: lanes {} vs per cell {}", ra.is_ok(), rb.is_ok()),
                }
            }
            assert_eq!(values(&a), values(&b), "{path} rg {rg}");
            rows += a.rows as usize;
            let keys: Vec<usize> = (0..atts.len()).filter(|&i| atts[i].3).collect();
            if let Ok(w) = sh.decode_words(rg, &keys, &mut HeldCharge::none()) {
                for (j, &i) in keys.iter().enumerate() {
                    let Lane::Words { words, nulls } = &b.lanes[i] else { unreachable!() };
                    assert_eq!((&w[j].0, &w[j].1), (words, nulls), "{path} rg {rg} key {i}");
                }
            }
        }
        Some(Ok(rows))
    }

    /// The typed column loops build exactly the per-cell lanes (words,
    /// nulls, image bytes) on the fixtures, and fail where it fails.
    #[test]
    fn typed_lanes_equal_per_cell_lanes() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../access/parquet_read/testdata/");
        let par = [("id", 20, -1, true), ("v16", 21, -1, true), ("cat", 25, -1, false), ("payload", 25, -1, false)];
        let par_vc = [("id", 20, -1, true), ("v16", 21, -1, true), ("cat", 1042, 4 + 9, false), ("payload", 1043, 4 + 40, false)];
        let par_cut = [("id", 20, -1, true), ("v16", 23, -1, true), ("cat", 1042, 4 + 4, false), ("payload", 1043, 4 + 30, false)];
        let basic = [
            ("id", 20, -1, true),
            ("i32req", 23, -1, true),
            ("i32opt", 23, -1, true),
            ("i64opt", 20, -1, true),
            ("f32req", 700, -1, true),
            ("f64opt", 701, -1, true),
            ("breq", 16, -1, true),
            ("bopt", 16, -1, true),
            ("dict", 25, -1, false),
            ("uniq", 25, -1, false),
            ("sopt", 25, -1, false),
            ("d", 1082, -1, true),
            ("ts", 1114, -1, true),
        ];
        let epoch = [
            ("a", 1114, -1, true),
            ("b", 1114, -1, true),
            ("c", 1114, -1, true),
            ("d", 1114, -1, true),
            ("e", 1082, -1, true),
            ("f", 1082, -1, true),
            ("g", 1082, -1, true),
            ("h", 1082, -1, true),
        ];
        let uints = [("u8", 21, -1, true), ("u16", 23, -1, true), ("u32", 20, -1, true), ("u64", 20, -1, true)];
        let cases: [(&str, &[(&str, Oid, i32, bool)], bool, bool, bool); 9] = [
            ("pgrcpar.parquet", &par, false, true, true),
            ("pgrcpar.parquet", &par_vc, false, true, true),
            ("pgrcpar.parquet", &par_cut, false, true, false),
            ("pgrcpar-poison.parquet", &par, false, true, false),
            ("pgrcpar-nullpoison.parquet", &par, false, true, false),
            ("pgrcpar-nullpoison.parquet", &par, false, false, true),
            ("basic.parquet", &basic, false, false, true),
            ("epochvals.parquet", &epoch, true, false, true),
            ("uints.parquet", &uints, false, false, true),
        ];
        let mut ran = 0;
        for (f, atts, coerce, notnull, want_ok) in cases {
            match walk(&format!("{dir}{f}"), atts, coerce, notnull) {
                None => {}
                Some(r) => {
                    assert_eq!(r.is_ok(), want_ok, "{f} {atts:?}: {r:?}");
                    ran += 1;
                }
            }
        }
        assert!(ran >= 7, "cases run: {ran}");
    }
}
