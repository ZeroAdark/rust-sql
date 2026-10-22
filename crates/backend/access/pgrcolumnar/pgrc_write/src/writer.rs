//! The serial ingest face + writer lifecycle (chunk M3-D row: "per-(xid,cid)
//! writer lifecycle + eoxact purge, serial ingest face").
//!
//! ## Lifecycle discipline (the proven old-writer laws, carried verbatim)
//!
//! - **One writer per table per backend**, keyed by relfilenumber in a
//!   [`WriterRegistry`]; the txn stamp (fxid, cid) is a STALENESS FIELD, not
//!   part of the key. A stamp mismatch EVICTS the old writer by ABORT —
//!   buffered rows are dropped and its temp files unlinked, never published
//!   (an errored statement's writer must not leak rows into a later
//!   statement of the same transaction).
//! - **Publish is the taking statement's act**: [`WriterRegistry::
//!   take_for_publish`] removes the writer and re-checks the stamp; a stale
//!   writer is aborted, not published (the savepoint-rollback case).
//! - **eoxact purge is unconditional on COMMIT and ABORT alike**: anything
//!   still registered at transaction end is by construction abandoned — the
//!   successful statement already took its writer at publish. (M3-H
//!   registers the xact callback; the registry lives in the session
//!   envelope, never in a crate thread-local — the TLS census is pinned.)
//!
//! ## Freeze parity (the old-writer belt, decision-table-pinned)
//!
//! Frozen writes are sound only when an abort of the writing
//! (sub)transaction also unlinks the table's files: `cur_subxact` valid AND
//! (relation created in it OR its relfilelocator newly minted in it). The
//! belt SILENTLY DOWNGRADES otherwise (the loud C-parity error is COPY
//! FREEZE's, at the command layer). At M3 the decision is recorded as a
//! writer fact ([`FreezeDecision`]) with NO on-disk bit: part visibility is
//! the manifest clog fence, and a table created in the writing transaction
//! is invisible to every other snapshot anyway; where a frozen flag lands
//! on disk (manifest flags) is an A-lane transcription owed with the M5
//! visibility rung — reported, not invented here.
//!
//! ## The part-cut policy
//!
//! Whole-part buffering is bounded by [`PartCutPolicy`]: a part is cut when
//! the row or byte budget trips, tested ONLY at a `cut_granule_rows`
//! boundary. Cut points are a pure function of the accumulated rows —
//! determinism preserved (same input partition ⇒ same parts ⇒ same bytes).
//!
//! The granule gate is shared with the parallel cut cursor and is what
//! makes a parallel-loaded table byte-identical to a serial-loaded one
//! under EITHER budget; see [`PartCutPolicy`] for why an ungated serial cut
//! produced partitions `crate::par` could not reproduce.

use pgrc_format::abi::ColumnMetaBuilder;
use pgrc_format::class::{ColSchema, StorageClass};
use pgrc_format::ident::schema_fingerprint;
use pgrc_format::relopt::ShredOptions;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::elect::CandidateSource;
use crate::ingest::{normalize_varlena, ColBuffer, ExternalDetoast, NoExternalDetoast, RawDatum};
use crate::plan::{apply_reclaim, dir_loc};
use crate::publish::{publish_parts, PublishOutcome, TxnProbe};
use crate::seal::{seal_part, seal_part_pooled, seal_part_streamed, ColumnPool, PartSpec, SealReport, SealedPart, SerialPool, Streamed, StreamedPool, VerifyResolver};
use crate::shred::{validate_lanes, ShredLaneSource};
use crate::structural::StructuralPolicy;
use crate::walsink::{LoggedVfs, NoWal, WalSink};
use crate::wvfs::WriteVfs;
use pgrc_format::wal::ReclaimRec;
use crate::{WriteError, WriteResult};

/// The txn identity stamp: epoch-qualified fxid (the manifest's clog-fence
/// currency) + command id. Passed capabilities — this crate never reads
/// transaction state itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxnStamp {
    pub fxid: u64,
    pub cid: u32,
}

/// Subtransaction evidence for the freeze belt (M3-H passes the relcache
/// facts; tests script them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SubxactEvidence {
    pub cur_subxact_valid: bool,
    pub rel_created_in_cur_subxact: bool,
    pub new_relfilelocator_in_cur_subxact: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreezeDecision {
    Frozen,
    Downgraded,
}

/// The old-writer freeze belt, verbatim: frozen iff the current subxact is
/// valid AND the relation (or its relfilelocator) was created in it.
pub fn freeze_decision(e: &SubxactEvidence) -> FreezeDecision {
    if e.cur_subxact_valid
        && (e.rel_created_in_cur_subxact || e.new_relfilelocator_in_cur_subxact)
    {
        FreezeDecision::Frozen
    } else {
        FreezeDecision::Downgraded
    }
}

/// The default cut-decision granule, in rows.
///
/// Chosen to bound byte-budget overshoot (see [`PartCutPolicy`]): a part of
/// about 1.4 KB rows runs ~190k rows under the 256 MiB default, so an 8,192
/// granule overshoots by at most ~4% (the bound shrinks as rows widen). It
/// equals [`pgrc_format::geom::GRANULE_ROWS`], a format-version-1 geometry
/// parameter (`pgrc_format::FORMAT_VERSION`). The capture-chunk grain (65,536, one
/// band) would overshoot by up to ~34%.
pub const DEFAULT_CUT_GRANULE_ROWS: u32 = 8_192;

/// Deterministic part-cut bounds.
///
/// ## The granule gate (M3-I) — a correctness boundary, not a tuning knob
///
/// The budget predicate is evaluated ONLY at a `cut_granule_rows` boundary,
/// by BOTH ingest paths. This is what makes a parallel-loaded table
/// byte-identical to a serial-loaded one: the parallel cut cursor
/// ([`crate::par::ParEngine`]) can close a part only at a capture-chunk
/// boundary, so a serial writer free to cut after an arbitrary row would
/// produce partitions the parallel path cannot reproduce.
///
/// The gate is necessary but NOT sufficient (#597): the byte OPERAND both
/// paths feed [`PartCutPolicy::should_cut`] must also be identical — see
/// that method's doc for the operand law and the divergence it closed.
///
/// Before this gate the two agreed only when the ROW budget governed; under
/// the byte budget they legally diverged and only the O-10 logical multiset
/// identity held. That was the live case, not the corner — production runs
/// the 256 MiB byte budget, which governs at ~1.4 KB row widths (the 10M
/// ingest-attribution run wrote 55 files where a row-governed cut gives
/// ~10), and a bank MANIFEST carries part geometry, so a divergent
/// partition breaks bank identity.
///
/// The gate costs overshoot: a part closes at the first granule boundary at
/// or after a budget trips, so it may exceed `max_bytes` by up to one
/// granule's worth of rows. That is the price of the identity law and is
/// bounded by [`DEFAULT_CUT_GRANULE_ROWS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartCutPolicy {
    pub max_rows: u64,
    pub max_bytes: u64,
    /// Cut-decision granularity in rows (must be > 0). The parallel path
    /// pins its capture-chunk grain to exactly this value.
    pub cut_granule_rows: u32,
}

impl Default for PartCutPolicy {
    fn default() -> PartCutPolicy {
        PartCutPolicy {
            // 1 Mi rows = exactly 16 bands = exactly 128 default granules.
            max_rows: 1 << 20,
            max_bytes: 256 << 20,
            cut_granule_rows: DEFAULT_CUT_GRANULE_ROWS,
        }
    }
}

impl PartCutPolicy {
    /// True at a cut-decision boundary (never at zero rows).
    pub fn at_granule(&self, rows: u64) -> bool {
        rows > 0 && rows % self.cut_granule_rows as u64 == 0
    }

    /// THE cut predicate — the ONE implementation shared by serial ingest
    /// ([`TableWriter::append_row`]) and the parallel cut cursor
    /// ([`crate::par::ParEngine`]). Both paths must ask this and nothing
    /// else; a second copy of the budget arithmetic is how the partitions
    /// drift apart again.
    ///
    /// The OPERANDS are law too (#597): `bytes` must be the part-accumulated
    /// [`crate::ingest::ColBuffer::approx_bytes`] sum AS SERIAL INGEST
    /// OBSERVES IT (`TableWriter::buffered_bytes`). The parallel cursor once
    /// fed a chunk-LOCAL `approx_bytes` sum, which omits the heap pad8 the
    /// part assembler's splice inserts at each interior chunk seam
    /// (`ColBuffer::splice_chunk` — 0-7 bytes per byref column per seam);
    /// on varlena schemas where the byte budget governs, the skewed operand
    /// crossed `max_bytes` at a later granule boundary than serial and the
    /// two paths cut different partitions — byte-divergent banks from
    /// identical input. The cursor now replays the splice's pad arithmetic
    /// per column so both callers feed identical bytes at every granule
    /// boundary.
    ///
    /// Callers gate the O(ncols) byte scan on [`PartCutPolicy::at_granule`]
    /// first; the granule test is repeated here so the predicate is correct
    /// standalone.
    pub fn should_cut(&self, rows: u64, bytes: u64) -> bool {
        self.at_granule(rows) && (rows >= self.max_rows || bytes >= self.max_bytes)
    }
}

/// Everything a seal needs beyond the writer's own state — bundled so the
/// ingest face stays one call (`append_row` cuts parts internally when the
/// policy trips).
pub struct SealEnv<'a> {
    pub vfs: &'a mut dyn WriteVfs,
    pub sources: &'a [&'a dyn CandidateSource],
    pub resolver: &'a dyn VerifyResolver,
    pub shred: &'a mut dyn ShredLaneSource,
    pub shred_opts: &'a ShredOptions,
}

/// One table's buffered writer for one (fxid, cid) statement.
pub struct TableWriter {
    spec: PartSpec,
    table_dir: String,
    schema: Vec<ColSchema>,
    stamp: TxnStamp,
    freeze: FreezeDecision,
    policy: PartCutPolicy,
    /// Structural-election facts (TY-1 ArrayDual; catalog-declared —
    /// [`TableWriter::set_structural`]). Default: none.
    structural: StructuralPolicy,
    cols: Vec<ColBuffer>,
    sealed: Vec<SealedPart>,
    reports: Vec<SealReport>,
    /// The next tmp-name seq, shared by every writer of this table in one transaction: a tmp name renamed into a
    /// part must never be reused before the transaction ends, or redo of the earlier publish would install the
    /// later file.
    seq: Arc<AtomicU32>,
    sink: Box<dyn WalSink + Send>,
    scratch: Vec<u8>,
    pool: Option<SealPool>,
    /// K1-MEM: a pooled streamed seal's batch ([`crate::seal::STREAMED_BATCH_BYTES`]).
    streamed_batch: u64,
    /// IX0 L3 (DV-3), IDBURN: the row IDs granted to the part being buffered ([`TableWriter::grant_ids`]), in grant
    /// order; its rows take them in order, every range but the last whole. The cut records them with the last one cut
    /// to `align_up` of its rows; the rest of it is released ([`TableWriter::take_released`]).
    ranges: Vec<(u64, u64)>,
    /// The IDs `ranges` hold, and the rows before the last range.
    held: u64,
    last_start: u64,
    /// The last grant's size, 0 before the part's first ([`TableWriter::needs_ids`]).
    last_grant: u64,
    /// The unused tails `[end, hi)` of the parts cut since the last [`TableWriter::take_released`].
    released: Vec<(u64, u64)>,
    /// SORTRUNS-E0 L2: the temps of [`TableWriter::write_file`] not yet published (reclaimed with the parts on abort).
    files: Vec<String>,
    /// IXBULKSORT: where the rows of each part cut since the last [`TableWriter::take_placements`] landed; `None`: not
    /// kept ([`TableWriter::keep_placements`]).
    placements: Option<Vec<Placed>>,
    /// D4368: the buffered bytes at the ladder gate's last granule (0 after a cut).
    ladder_bytes: u64,
    /// D4387: this writer's part reserve in the process-wide one (`par::seal_ahead_all`): (SEAL_X_LADDER - 1) x the
    /// buffered bytes the gate last admitted; out at the cut (the seal makes it resident) or when the writer drops.
    ladder_reserve: u64,
    /// CONVERT W3-2b: the statement this writer loads for ([`TableWriter::set_owner`]; a writer is one (fxid, cid)
    /// statement's): its buffered columns, the parts it seals and a parallel session over it are charged to it.
    owner: Option<memledger::Owner>,
}

impl Drop for TableWriter {
    fn drop(&mut self) {
        crate::par::seal_ahead_shrink(core::mem::take(&mut self.ladder_reserve));
    }
}

/// IXBULKSORT: where a cut part's rows landed. A row takes the ID of its arrival position when it is appended
/// ([`TableWriter::next_id`]: its caller's index entries are made with it); a declared cluster key (`sorted_by`)
/// sorts the part at its seal, and the part's IDs follow the sealed order, so the row the seal put at position `s` has
/// the part's `s`-th ID. [`Placed::place`] maps the ID a row was handed to the one it has: through the inverse of the
/// seal's sort permutation ([`crate::seal::SealReport::cluster_perm`]), no map of ID pairs and nothing on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    /// The part's IDs in row order ([`SealedPart::ids`]): every range but the last whole.
    ids: Vec<(u64, u64)>,
    rows: u64,
    /// Arrival position -> sealed position; `None`: the seal kept arrival order.
    sorted: Option<Vec<u32>>,
}

impl Placed {
    /// The part's own heap bytes (its ID ranges and the inverse permutation), for the holder that keeps it (D4537).
    pub fn heap_bytes(&self) -> usize {
        self.ids.capacity() * core::mem::size_of::<(u64, u64)>() + self.sorted.as_ref().map_or(0, |s| s.capacity() * core::mem::size_of::<u32>())
    }

    #[cfg(test)]
    pub fn new(ids: Vec<(u64, u64)>, rows: u64, perm: Option<Vec<u32>>) -> Placed {
        Placed::try_new(ids, rows, perm).expect("test placement")
    }

    /// A part of `rows` rows homed at `ids`, sealed in the order `perm` (`perm[s]` = the arrival position of the row
    /// sealed at position `s`; `None`: arrival order).
    pub fn try_new(ids: Vec<(u64, u64)>, rows: u64, perm: Option<Vec<u32>>) -> WriteResult<Placed> {
        let sorted = match perm {
            Some(perm) => {
                let mut inv = pgrc_format::try_filled(perm.len(), 0u32, "writer")?;
                for (s, &a) in perm.iter().enumerate() {
                    inv[a as usize] = s as u32;
                }
                Some(inv)
            }
            None => None,
        };
        Ok(Placed { ids, rows, sorted })
    }

    /// The ID of the row handed `handed` at its append; `None`: not one of this part's rows.
    pub fn place(&self, handed: u64) -> Option<u64> {
        let mut pos = 0u64;
        let mut arrival = None;
        for &(lo, hi) in &self.ids {
            if (lo..hi).contains(&handed) {
                arrival = Some(pos + (handed - lo));
                break;
            }
            pos += hi - lo;
        }
        let arrival = arrival.filter(|&a| a < self.rows)?;
        let at = match &self.sorted {
            Some(inv) => u64::from(*inv.get(arrival as usize)?),
            None => arrival,
        };
        let mut pos = at;
        for &(lo, hi) in &self.ids {
            if pos < hi - lo {
                return Some(lo + pos);
            }
            pos -= hi - lo;
        }
        None
    }

    /// The part's rows.
    pub fn rows(&self) -> u64 {
        self.rows
    }
}

/// IDBURN: a part's first grant (one virtual block), and how much each next one grows.
pub const FIRST_GRANT: u64 = pgrc_format::rowid::IDS_PER_BLOCK;
pub const GRANT_GROWTH: u64 = 4;

/// A pooled seal (F1 L6): the pool, and `Sync` handles on the same election sources and resolver the env carries.
pub struct SealPool {
    pub pool: Arc<dyn ColumnPool>,
    pub sources: Vec<Arc<dyn CandidateSource + Send + Sync>>,
    pub resolver: Arc<dyn VerifyResolver + Send + Sync>,
}

impl TableWriter {
    pub fn open(
        table_dir: String,
        schema: Vec<ColSchema>,
        spc: u32,
        db: u32,
        relfilenumber: u64,
        stamp: TxnStamp,
        evidence: &SubxactEvidence,
        policy: PartCutPolicy,
    ) -> WriteResult<TableWriter> {
        if schema.is_empty() {
            return Err(WriteError::Contract {
                detail: "table with zero columns",
            });
        }
        if policy.cut_granule_rows == 0 {
            return Err(WriteError::Contract {
                detail: "cut_granule_rows must be positive",
            });
        }
        for w in schema.windows(2) {
            if w[1].attno <= w[0].attno {
                return Err(WriteError::Contract {
                    detail: "schema attnos must strictly increase",
                });
            }
        }
        let spec = PartSpec {
            spc,
            db,
            relfilenumber,
            schema_fingerprint: schema_fingerprint(&schema),
        };
        let cols = schema.iter().map(|s| ColBuffer::new(*s)).collect();
        Ok(TableWriter {
            spec,
            table_dir,
            schema,
            stamp,
            freeze: freeze_decision(evidence),
            policy,
            structural: StructuralPolicy::default(),
            cols,
            sealed: Vec::new(),
            reports: Vec::new(),
            seq: Arc::new(AtomicU32::new(0)),
            sink: Box::new(NoWal),
            scratch: Vec::new(),
            pool: None,
            streamed_batch: crate::seal::STREAMED_BATCH_BYTES,
            ranges: Vec::new(),
            held: 0,
            last_start: 0,
            last_grant: 0,
            released: Vec::new(),
            files: Vec::new(),
            placements: None,
            ladder_bytes: 0,
            ladder_reserve: 0,
            owner: None,
        })
    }

    /// Log this writer's files and publishes through `sink` (the default is [`NoWal`]).
    pub fn with_sink(mut self, sink: Box<dyn WalSink + Send>) -> TableWriter {
        self.sink = sink;
        self
    }

    /// Log every file of this writer by its bytes ([`crate::walsink::BytesOnly`]).
    pub fn bytes_only(&mut self) {
        let inner = std::mem::replace(&mut self.sink, Box::new(NoWal));
        self.sink = Box::new(crate::walsink::BytesOnly(inner));
    }

    pub fn sink_mut(&mut self) -> &mut dyn WalSink {
        &mut *self.sink
    }

    /// CONVERT W3-2b: charge this writer's buffers to `owner` (the loading statement, read once on the leader where
    /// the writer opens); its column buffers move to it (it is set before the first row).
    pub fn set_owner(&mut self, owner: Option<memledger::Owner>) {
        self.owner = owner;
        for c in &mut self.cols {
            c.set_owner(owner);
        }
    }

    /// CONVERT W3-2b: the owner this writer charges (`None`: uncharged).
    pub fn owner(&self) -> Option<memledger::Owner> {
        self.owner
    }

    /// Declare the table's structural-election facts (TY-1: array columns
    /// + their catalog element facts). M3-H derives these at DDL/COPY setup;
    /// tests declare directly. Must be set before the first appended row of
    /// a statement (per-part permanence — a mid-statement change would
    /// split election behavior across parts).
    pub fn set_structural(&mut self, structural: StructuralPolicy) {
        self.structural = structural;
    }

    /// IXBULKSORT: keep each cut part's [`Placed`] for [`TableWriter::take_placements`] (a writer whose rows' IDs feed
    /// index entries). Before the first row.
    pub fn keep_placements(&mut self) {
        self.placements.get_or_insert_with(Vec::new);
    }

    /// IXBULKSORT: whether this writer keeps placements ([`TableWriter::keep_placements`]).
    pub fn keeps_placements(&self) -> bool {
        self.placements.is_some()
    }

    /// IXBULKSORT: the [`Placed`] of each part with IDs cut since the last call, in cut order.
    pub fn take_placements(&mut self) -> Vec<Placed> {
        self.placements.as_mut().map(std::mem::take).unwrap_or_default()
    }

    /// Seal parts, and append [`crate::par::RowChunk`]s, column by column on `pool`.
    pub fn set_pool(&mut self, pool: SealPool) {
        self.pool = Some(pool);
    }

    /// K1-MEM: the column bytes a pooled [`TableWriter::seal_streamed`] batch holds (tests: several batches).
    pub fn set_streamed_batch(&mut self, bytes: u64) {
        self.streamed_batch = bytes;
    }

    /// The part cut policy (a seal writes one part: F1). Before the first row.
    pub fn set_policy(&mut self, policy: PartCutPolicy) {
        self.policy = policy;
    }

    pub fn structural(&self) -> &StructuralPolicy {
        &self.structural
    }

    pub fn stamp(&self) -> TxnStamp {
        self.stamp
    }

    pub fn freeze(&self) -> FreezeDecision {
        self.freeze
    }

    pub fn spec(&self) -> &PartSpec {
        &self.spec
    }

    pub fn buffered_rows(&self) -> u64 {
        self.cols.first().map(|c| c.rows()).unwrap_or(0)
    }

    pub fn sealed_parts(&self) -> &[SealedPart] {
        &self.sealed
    }

    pub fn seal_reports(&self) -> &[SealReport] {
        &self.reports
    }

    pub fn buffered_bytes(&self) -> u64 {
        self.cols.iter().map(|c| c.approx_bytes()).sum()
    }

    /// The serial ingest face: append one row (COPY-shaped; M3-H routes
    /// here). Varlena inputs are normalized per the detoast law; the part
    /// cut policy seals inline when a budget trips.
    pub fn append_row(
        &mut self,
        row: &[RawDatum<'_>],
        ext: &mut dyn ExternalDetoast,
        env: &mut SealEnv<'_>,
    ) -> WriteResult<()> {
        if row.len() != self.cols.len() {
            return Err(WriteError::Contract {
                detail: "row width mismatch",
            });
        }
        for (i, d) in row.iter().enumerate() {
            let col = &mut self.cols[i];
            match (d, col.schema.class) {
                (RawDatum::Null, _) => col.append_null(),
                (RawDatum::Word(w), StorageClass::ByvalWord { .. })
                | (RawDatum::Word(w), StorageClass::F32)
                | (RawDatum::Word(w), StorageClass::F64)
                | (RawDatum::Word(w), StorageClass::Bool) => col.append_word(*w)?,
                (RawDatum::Bytes(b), StorageClass::Fixed { .. }) => col.append_fixed(b)?,
                (RawDatum::Bytes(b), StorageClass::VarlenaVerbatim) => {
                    let mut scratch = std::mem::take(&mut self.scratch);
                    let res = (|| {
                        let (payload, _) = normalize_varlena(b, ext, &mut scratch)?;
                        self.cols[i].append_varlena_payload(payload)
                    })();
                    self.scratch = scratch;
                    res?;
                }
                _ => {
                    return Err(WriteError::Contract {
                        detail: "datum shape does not match column class",
                    })
                }
            }
        }
        // The granule gate is the byte-identity boundary (see
        // `PartCutPolicy`); it also keeps the O(ncols) byte scan off the
        // per-row path — it now runs once per granule, not once per row.
        // The granule gate is the byte-identity boundary (see
        // `PartCutPolicy`; a byte-budget parallel partition must be
        // byte-identical to serial), so the byte budget is consulted at every
        // granule and never between them. Wide rows inside one granule are
        // bounded by the varlena cap and admitted fallibly (arena ceiling).
        let rows = self.buffered_rows();
        if self.policy.at_granule(rows) {
            // CONVERT W3-2b: the per-row appends above grow the columns uncharged until here: the charge catches up
            // once per granule (at most one granule's growth behind)
            for c in &mut self.cols {
                c.sync_charge();
            }
            let bytes = self.buffered_bytes();
            if self.policy.should_cut(rows, bytes) {
                self.cut_part(env)?;
            } else if mcx::ladder::on() {
                // D4368 (ladder on): the part's seal needs ~3x its bytes (SEAL_X_LADDER), the image 1x of it resident
                // already: the rest of the buffered part's reserve plus the next granule's whole share must fit under the
                // ladder's line, else the gate waits and refuses (SPILL-DEBT LoadSealPastLine), as the parallel engine's
                // D4387: and every other load's seal reserve: `ladder_reserve` (what this writer registered) is in the
                // process-wide one, the pass adds the rest of `reserve`, and while the writer waits (it cannot cut) the
                // other gates leave its registered reserve out
                let grain = bytes.saturating_sub(self.ladder_bytes);
                self.ladder_bytes = bytes;
                let reserve = bytes.saturating_mul(crate::par::SEAL_X_LADDER - 1);
                let admit = reserve.saturating_sub(self.ladder_reserve);
                let own = admit.saturating_add(grain.saturating_mul(crate::par::SEAL_X_LADDER));
                crate::par::ladder_wait(own, admit, self.ladder_reserve)?;
                self.ladder_reserve = reserve;
            }
        }
        Ok(())
    }

    /// A chunk's rows, column by column on the pool when one is set. Only under an unbounded cut policy: a cut
    /// cannot fall inside a chunk. The first failing column's error.
    pub fn append_chunk(&mut self, chunk: &crate::par::RowChunk) -> WriteResult<()> {
        if chunk.ncols() != self.cols.len() {
            return Err(WriteError::Contract { detail: "row width mismatch" });
        }
        if self.policy.max_rows != u64::MAX || self.policy.max_bytes != u64::MAX || chunk.has_dict_lanes() {
            return Err(WriteError::Contract { detail: "chunk append under a bounded cut policy or with dict lanes" });
        }
        if self.pool.is_some() {
            // C1-REG: the columns grow here, on the thread that frees them, not on the workers (only a detoast that
            // inflates a value past its captured image still grows a column on a worker).
            let rows = u64::from(chunk.rows());
            for (c, col) in self.cols.iter_mut().enumerate() {
                col.reserve(rows, chunk.column_heap_bound(c))?;
            }
        }
        let slots: Vec<pgsync::Mutex<(&mut ColBuffer, WriteResult<()>)>> = self.cols.iter_mut().map(|c| pgsync::Mutex::new((c, Ok(())))).collect();
        let run = |c: usize| {
            let mut slot = pgsync::lock(&slots[c]);
            let (col, out) = &mut *slot;
            *out = chunk.append_column(c, col, &mut NoExternalDetoast, &mut Vec::new());
        };
        match &self.pool {
            Some(p) => p.pool.run(slots.len(), &run),
            None => SerialPool.run(slots.len(), &run),
        }
        slots.into_iter().map(|s| s.into_inner().unwrap_or_else(|e| e.into_inner()).1).collect()
    }

    /// Seal the buffered rows into a part (no-op on empty buffers).
    fn cut_part(&mut self, env: &mut SealEnv<'_>) -> WriteResult<()> {
        self.ladder_bytes = 0;
        crate::par::seal_ahead_shrink(core::mem::take(&mut self.ladder_reserve));
        if self.buffered_rows() == 0 {
            self.released.extend(self.ranges.drain(..));
            (self.held, self.last_start, self.last_grant) = (0, 0, 0);
            return Ok(());
        }
        // Derive shred lanes (dual-store, O-4): the image lane is the
        // parent column itself; typed lanes come from the seam.
        let mut lanes = memledger::ChargedVec::new(self.owner(), memledger::Kind::Region);
        for col in &self.cols {
            if col.schema.class == StorageClass::VarlenaVerbatim {
                let derived = env.shred.shred(col, env.shred_opts)?;
                validate_lanes(col, &derived, env.shred_opts)?;
                lanes.with_vec(|v| v.extend(derived)); // alloc: Exec charged self.owner()
            }
        }
        // D3038: the columns whose octet-length lanes `seal_part` derives, after every shred lane (crate::octet)
        let octet = crate::octet::octet_columns(&self.cols, &self.structural, &*env.shred);
        // M3-E's real builders, one per stream in `seal_part`'s own stream
        // order (roots then lanes). Was `StandinMetaBuilder` — see
        // `crate::meta_wire` for what that cost and why the seal driver did
        // not have to change to fix it.
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut lvfs = LoggedVfs::new(env.vfs, &mut *self.sink, &self.table_dir);
        let (sealed, report) = match &self.pool {
            None => {
                let mut builders: Vec<Box<dyn ColumnMetaBuilder>> =
                    crate::meta_wire::builders_for_streams(&self.cols, &lanes, &octet);
                seal_part(
                    &mut lvfs,
                    &self.table_dir,
                    &self.spec,
                    &self.cols,
                    &lanes,
                    &octet,
                    &mut builders,
                    env.sources,
                    env.resolver,
                    &self.structural,
                    self.stamp.fxid,
                    seq,
                )?
            }
            Some(p) => {
                let mut builders = crate::meta_wire::send_builders_for_streams(&self.cols, &lanes, &octet);
                let sources: Vec<&(dyn CandidateSource + Sync)> = p.sources.iter().map(|s| &**s as &(dyn CandidateSource + Sync)).collect();
                seal_part_pooled(
                    &mut lvfs,
                    &self.table_dir,
                    &self.spec,
                    &self.cols,
                    &lanes,
                    &octet,
                    &mut builders,
                    &sources,
                    &*p.resolver,
                    &self.structural,
                    self.stamp.fxid,
                    seq,
                    &*p.pool,
                )?
            }
        };
        let mut sealed = sealed;
        // IXBULKSORT: the sort permutation is the rows' placement, never the report's (a report outlives the cut)
        let mut report = report;
        let perm = report.cluster_perm.take();
        if let Some((lo, hi)) = self.ranges.pop() {
            let used = sealed.rows.checked_sub(self.last_start).filter(|_| sealed.rows <= self.held);
            let Some(used) = used else {
                return Err(WriteError::Contract { detail: "a part past its row-ID grant" });
            };
            let end = lo + pgrc_format::rowid::align_up(used);
            if used > 0 {
                self.ranges.push((lo, end));
            }
            if end < hi {
                self.released.push((end, hi));
            }
            sealed.ids = std::mem::take(&mut self.ranges);
            (self.held, self.last_start, self.last_grant) = (0, 0, 0);
            if let Some(p) = self.placements.as_mut().filter(|_| !sealed.ids.is_empty()) {
                p.push(Placed::try_new(sealed.ids.clone(), sealed.rows, perm)?);
            }
        }
        self.sealed.push(sealed);
        self.reports.push(report);
        let owner = self.owner;
        self.cols = self.schema.iter().map(|s| ColBuffer::with_owner(*s, owner)).collect();
        Ok(())
    }

    /// IX0 L3 (DV-3), IDBURN: the IDs the next row needs granted first, when the cut policy bounds a part's rows and
    /// the held ones are used up: [`FIRST_GRANT`] for a part's first grant, then [`GRANT_GROWTH`] times the last, never
    /// past `align_up` of the rows the cut leaves. `None`: a grant is held, or the writer's parts get their IDs at the
    /// publish (an unbounded policy, the parallel lane's deposits).
    pub fn needs_ids(&self) -> Option<u64> {
        let rows = self.buffered_rows();
        if self.policy.max_rows == u64::MAX || rows < self.held {
            return None;
        }
        let grow = if self.last_grant == 0 { FIRST_GRANT } else { self.last_grant.saturating_mul(GRANT_GROWTH) };
        Some(grow.min(pgrc_format::rowid::align_up(self.policy.max_rows.saturating_sub(rows).max(1))))
    }

    /// The IDs `[lo, hi)` the part being buffered takes next, from the table's counter ([`TableWriter::needs_ids`]): a
    /// grant starting where the last one ended extends its range.
    pub fn grant_ids(&mut self, lo: u64, hi: u64) -> WriteResult<()> {
        let rows = self.buffered_rows();
        if lo >= hi || rows < self.held || self.held + (hi - lo) < rows {
            return Err(WriteError::Contract { detail: "a row-ID grant over a held one, or smaller than the buffered rows" });
        }
        match self.ranges.last_mut() {
            Some(last) if last.1 == lo => last.1 = hi,
            _ => {
                self.last_start = self.held;
                self.ranges.push((lo, hi));
            }
        }
        self.held += hi - lo;
        self.last_grant = hi - lo;
        Ok(())
    }

    /// The ID the next appended row takes, under a grant.
    pub fn next_id(&self) -> Option<u64> {
        self.ranges.last().map(|&(lo, _)| lo + self.buffered_rows().saturating_sub(self.last_start))
    }

    /// IDBURN: the unused tails `[end, hi)` of the parts cut since the last call, for the caller to give back to the
    /// table's counter (`pgrc_am::rowid_alloc::give_back`). An aborted writer gives nothing back.
    pub fn take_released(&mut self) -> Vec<(u64, u64)> {
        std::mem::take(&mut self.released)
    }

    /// The sealed parts not yet published, for the caller to give the ones without IDs theirs before the publish.
    pub fn sealed_parts_mut(&mut self) -> &mut [SealedPart] {
        &mut self.sealed
    }

    /// Statement end: seal any remaining buffered rows.
    pub fn finish(&mut self, env: &mut SealEnv<'_>) -> WriteResult<()> {
        self.cut_part(env)
    }

    /// K1-MEM: seal one part of `rows` rows column by column, never holding the part's columns
    /// ([`crate::seal::seal_part_streamed`]; the bytes are the ones [`TableWriter::append_chunk`] then
    /// [`TableWriter::finish`] seal for the same rows). `feed(c, out)` fills `out` (empty, the schema's column `c`)
    /// with the part's rows of column `c`. With no rows buffered. `Ok(false)`: the part needs a grain finer than the
    /// default; nothing is left (a tmp already written is reclaimed) and the caller appends the rows instead.
    pub fn seal_streamed(
        &mut self,
        env: &mut SealEnv<'_>,
        rows: u64,
        feed: &mut dyn FnMut(usize, &mut ColBuffer) -> WriteResult<()>,
    ) -> WriteResult<bool> {
        if self.buffered_rows() != 0 {
            return Err(WriteError::Contract { detail: "a streamed seal with rows buffered" });
        }
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut created = false;
        let out = {
            let pool = self.pool.as_ref().map(|p| StreamedPool {
                pool: &*p.pool,
                sources: p.sources.iter().map(|s| &**s as &(dyn CandidateSource + Sync)).collect(),
                resolver: &*p.resolver,
            });
            let mut lvfs = LoggedVfs::new(&mut *env.vfs, &mut *self.sink, &self.table_dir);
            seal_part_streamed(
                &mut lvfs,
                &self.table_dir,
                &self.spec,
                &self.schema,
                rows,
                feed,
                &mut *env.shred,
                env.shred_opts,
                env.sources,
                env.resolver,
                pool.as_ref(),
                self.streamed_batch,
                &self.structural,
                self.stamp.fxid,
                seq,
                &mut created,
            )
        };
        match out {
            Ok(Streamed::Sealed(sealed, report)) => {
                self.sealed.push(sealed);
                self.reports.push(report);
                Ok(true)
            }
            Ok(Streamed::FinerGrain) => {
                self.reclaim_tmp(&mut *env.vfs, seq, created)?;
                Ok(false)
            }
            Err(e) => {
                // The seal's own error wins; the tmp goes as `abort` would take it.
                let _ = self.reclaim_tmp(&mut *env.vfs, seq, created);
                Err(e)
            }
        }
    }

    /// Unlink a streamed seal's tmp `tmp-<fxid>-<seq>` under a `RECLAIM` (as [`TableWriter::abort`] does).
    fn reclaim_tmp(&mut self, vfs: &mut dyn WriteVfs, seq: u32, created: bool) -> WriteResult<()> {
        if !created {
            return Ok(());
        }
        let names = vec![pgrc_format::dirlayout::temp_file_name(self.stamp.fxid, seq)];
        let rec = ReclaimRec { dir: dir_loc(&self.spec), xid: self.stamp.fxid as u32, retire_below: None, names };
        let dir = &self.table_dir;
        self.sink.reclaim(&rec, &mut || apply_reclaim(&mut *vfs, dir, &rec))
    }

    /// The parallel-ingest seam, fact side (chunk M3-I). The rtpool BINDING
    /// lives one crate up (`pgrc_ingest_par` — a `runtime` dependency here
    /// would close the `pgrc_am → pgrc_write` cycle #473 introduced); it
    /// builds a [`crate::par::ParEngine`] from these facts and deposits the
    /// ordered results through [`TableWriter::deposit_sealed_parts`].
    ///
    /// Contract: no serial-buffered rows may be pending (one statement is
    /// either serial or parallel, never spliced).
    pub fn par_facts(&self) -> WriteResult<ParFacts> {
        if self.buffered_rows() != 0 {
            return Err(WriteError::Contract {
                detail: "parallel ingest over serial-buffered rows",
            });
        }
        Ok(ParFacts {
            schema: self.schema.clone(),
            spec: self.spec,
            table_dir: self.table_dir.clone(),
            fxid: self.stamp.fxid,
            policy: self.policy,
            structural: self.structural.clone(),
            base_seq: self.seq.load(Ordering::Relaxed),
        })
    }

    /// The parallel-ingest seam, deposit side: the ORDERED COMMIT. Accepts
    /// parts sealed through the frozen face with contiguous seqs starting at
    /// this writer's next seq — self-checking via the temp-name law
    /// (`tmp-<fxid>-<seq>`), so a driver that skipped, reordered, or
    /// cross-stamped a part is refused typed. After deposit, `publish`/
    /// `abort`/registry lifecycle proceed UNCHANGED.
    pub fn deposit_sealed_parts(
        &mut self,
        parts: Vec<SealedPart>,
        reports: Vec<SealReport>,
    ) -> WriteResult<()> {
        if parts.len() != reports.len() {
            return Err(WriteError::Contract {
                detail: "deposit parts/reports length mismatch",
            });
        }
        let base = self.seq.load(Ordering::Relaxed);
        for (i, p) in parts.iter().enumerate() {
            let expect = pgrc_format::dirlayout::temp_file_name(
                self.stamp.fxid,
                base + i as u32,
            );
            if p.tmp_name != expect {
                return Err(WriteError::Contract {
                    detail: "deposit violates the ordered-commit temp-name law",
                });
            }
        }
        self.seq.store(base + parts.len() as u32, Ordering::Relaxed);
        self.sealed.extend(parts);
        self.reports.extend(reports);
        Ok(())
    }

    /// Publish every sealed part as one manifest generation (spec §13.3).
    /// Caller holds the table publish lock and commits AFTER this returns.
    pub fn publish(
        &mut self,
        vfs: &mut dyn WriteVfs,
        probe: &dyn TxnProbe,
    ) -> WriteResult<PublishOutcome> {
        self.publish_with_base(vfs, probe, None, None)
    }

    /// `publish` with the base generation's facts image and answers columns supplied by the
    /// caller (the process catalog's copy), so the publisher's folds read no file.
    pub fn publish_with_base(
        &mut self,
        vfs: &mut dyn WriteVfs,
        probe: &dyn TxnProbe,
        base_image: Option<&pgrc_format::facts::FactsImage>,
        base_answers: Option<&[(u32, Vec<u8>)]>,
    ) -> WriteResult<PublishOutcome> {
        let outcome = publish_parts(
            vfs,
            &mut *self.sink,
            &self.table_dir,
            &self.spec,
            &self.sealed,
            self.stamp.fxid,
            probe,
            (base_image, base_answers),
        )?;
        self.sealed.clear();
        Ok(outcome)
    }

    /// `publish` over the base generation as the process catalog holds it (P-1): its manifest and stats runs, used
    /// when `CURRENT` still names it; or a root-less generation's image and answers ([`crate::publish::BaseStats`]).
    pub fn publish_with_gen<'b>(
        &mut self,
        vfs: &mut dyn WriteVfs,
        probe: &dyn TxnProbe,
        base: impl Into<crate::publish::BaseStats<'b>>,
    ) -> WriteResult<PublishOutcome> {
        let outcome = publish_parts(vfs, &mut *self.sink, &self.table_dir, &self.spec, &self.sealed, self.stamp.fxid, probe, base)?;
        self.sealed.clear();
        Ok(outcome)
    }

    /// Publish a segment's seal (F1): at most one sealed part, installed as `part_no` with `rowmap` as its
    /// `RowIdentMap` (ROWMAP2 M1: a granules-form body, `pgrc_format::rowmap`, flagged so), homing `ids` (IX0: the
    /// segment's extents). Caller holds the table publish lock.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_seal<'b>(
        &mut self,
        vfs: &mut dyn WriteVfs,
        probe: &dyn TxnProbe,
        base_facts: impl Into<crate::publish::BaseStats<'b>>,
        part_no: u32,
        rowmap: &[u8],
        ids: &[(u64, u64)],
        edit: crate::publish::Edit<'_>,
    ) -> WriteResult<PublishOutcome> {
        if self.sealed.len() > 1 {
            return Err(WriteError::Contract { detail: "a seal cut its segment into more than one part" });
        }
        if let Some(s) = self.sealed.first_mut() {
            s.ids = pgrc_format::try_to_vec(ids, "writer")?;
        }
        let part = self.sealed.first().map(|s| (s, part_no, rowmap));
        let outcome = crate::publish::publish_seal(vfs, &mut *self.sink, &self.table_dir, &self.spec, part, self.stamp.fxid, probe, base_facts, edit)?;
        self.sealed.clear();
        Ok(outcome)
    }

    /// Publish a compaction (K1): at most one sealed part, installed under the next part number with its `RowIdentMap`
    /// `map` = `(form, body)`, homing `ids` (IX0: its inputs' ranges); the inputs `retire` = `(part_no, dv_gen)` leave.
    /// Caller holds the table publish lock.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_compact<'b>(
        &mut self,
        vfs: &mut dyn WriteVfs,
        probe: &dyn TxnProbe,
        base_facts: impl Into<crate::publish::BaseStats<'b>>,
        retire: &[(u32, u64)],
        map: (u32, &[u8]),
        ids: &[(u64, u64)],
    ) -> WriteResult<(PublishOutcome, Option<u32>)> {
        if self.sealed.len() > 1 {
            return Err(WriteError::Contract { detail: "a compaction cut its output into more than one part" });
        }
        if let Some(s) = self.sealed.first_mut() {
            s.ids = pgrc_format::try_to_vec(ids, "writer")?;
        }
        let part = self.sealed.first().map(|s| (s, map.0, map.1));
        let out = crate::publish::publish_compact(vfs, &mut *self.sink, &self.table_dir, &self.spec, part, retire, self.stamp.fxid, probe, base_facts)?;
        self.sealed.clear();
        Ok(out)
    }

    /// SORTRUNS-E0 L2: a file of this writer's transaction (a run's reverse), the temp
    /// `tmp-<fxid>-<RUN_FILE_TMP_SEQ | k>`, logged through the writer's sink as its parts are. `fill(write)` gives its
    /// bytes in order (`write` appends; a file at least the sink's `ref_min` may go by reference, so the writes are
    /// sequential from offset 0). Reclaimed with the writer's parts on abort, renamed by its publish
    /// ([`TableWriter::publish_runs`]). Its temp name.
    pub fn write_file(&mut self, vfs: &mut dyn WriteVfs, k: u32, fill: &mut dyn FnMut(&mut dyn FnMut(&[u8]) -> WriteResult<()>) -> WriteResult<()>) -> WriteResult<String> {
        let name = pgrc_format::dirlayout::temp_file_name(self.stamp.fxid, crate::publish::RUN_FILE_TMP_SEQ | k);
        if self.files.contains(&name) || k >= 0x0010_0000 {
            return Err(WriteError::Contract { detail: "a run job's staged file written twice" });
        }
        self.files.push(name.clone());
        let mut lvfs = LoggedVfs::new(vfs, &mut *self.sink, &self.table_dir);
        let fd = lvfs.create_rw(&format!("{}/{name}", self.table_dir))?;
        let mut off = 0u64;
        let filled = fill(&mut |b: &[u8]| {
            lvfs.pwrite_at(&fd, off, b)?;
            off += b.len() as u64;
            Ok(())
        });
        let closed = lvfs.close_file(fd);
        filled.and(closed).map(|()| name)
    }

    /// SORTRUNS-E0 L2: publish a run merge ([`crate::publish::publish_runs`]): every sealed part, in seal order, a
    /// member of `run` (its job read generation `start_gen`) with its map `maps[k]`, the inputs `retire` retired, `staged`
    /// (temps of [`TableWriter::write_file`], with their names) renamed, the rows `carry` routes marked. The members'
    /// numbers.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_runs<'b>(
        &mut self,
        vfs: &mut dyn WriteVfs,
        probe: &dyn TxnProbe,
        base_facts: impl Into<crate::publish::BaseStats<'b>>,
        maps: &[Vec<u8>],
        retire: &[u32],
        run: pgrc_format::sortrun::SortRun,
        start_gen: u64,
        staged: &[(String, String)],
        carry: crate::publish::Carry<'_>,
    ) -> WriteResult<(PublishOutcome, Vec<u32>)> {
        if staged.iter().any(|(t, _)| !self.files.contains(t)) {
            return Err(WriteError::Contract { detail: "a run merge stages a file its writer did not write" });
        }
        let out = crate::publish::publish_runs(vfs, &mut *self.sink, &self.table_dir, &self.spec, &self.sealed, maps, retire, run, start_gen, staged, carry, self.stamp.fxid, probe, base_facts)?;
        self.sealed.clear();
        self.files.retain(|f| !staged.iter().any(|(t, _)| t == f));
        Ok(out)
    }

    /// SORTRUNS-E0 L3b1: publish VACUUM FULL's renumber of a sorted run ([`crate::publish::publish_self_run`]): every
    /// part sealed so far a member of a self-homed run of `level`, in seal order, with its map `maps[k]` and dense
    /// ranges `ranges[k]`; the inputs `retire` retired. The part numbers installed.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_self_run<'b>(
        &mut self,
        vfs: &mut dyn WriteVfs,
        probe: &dyn TxnProbe,
        base_facts: impl Into<crate::publish::BaseStats<'b>>,
        maps: &[(u32, Vec<u8>)],
        ranges: &[pgrc_format::manifest::IdRanges],
        retire: &[(u32, u64)],
        level: u8,
    ) -> WriteResult<(PublishOutcome, Vec<u32>)> {
        let out = crate::publish::publish_self_run(vfs, &mut *self.sink, &self.table_dir, &self.spec, &self.sealed, maps, ranges, retire, level, self.stamp.fxid, probe, base_facts)?;
        self.sealed.clear();
        Ok(out)
    }

    /// SORTRUNS-E0 L3b1: publish a run member's slot swap ([`crate::publish::publish_member_swap`]): this writer's one
    /// sealed part replaces member `from` in its slot, with its RUN map `map`. The part number installed.
    pub fn publish_member_swap<'b>(
        &mut self,
        vfs: &mut dyn WriteVfs,
        probe: &dyn TxnProbe,
        base_facts: impl Into<crate::publish::BaseStats<'b>>,
        map: &[u8],
        from: u32,
        carry: crate::publish::SwapCarry<'_>,
    ) -> WriteResult<(PublishOutcome, u32)> {
        let out = crate::publish::publish_member_swap(vfs, &mut *self.sink, &self.table_dir, &self.spec, &self.sealed, map, from, carry, self.stamp.fxid, probe, base_facts)?;
        self.sealed.clear();
        Ok(out)
    }

    /// Abort: drop buffered rows and unlink this writer's temp files under a `RECLAIM` record (decision W0-4).
    /// Missing files are fine (never written, or already cleaned).
    pub fn abort(&mut self, vfs: &mut dyn WriteVfs) -> WriteResult<()> {
        // dropped rows: nothing more is loaded, so the empty buffers are uncharged
        self.cols = self.schema.iter().map(|s| ColBuffer::new(*s)).collect();
        self.ranges.clear();
        self.released.clear();
        (self.held, self.last_start, self.last_grant) = (0, 0, 0);
        let mut names: Vec<String> = std::mem::take(&mut self.sealed).into_iter().map(|s| s.tmp_name).collect();
        names.append(&mut self.files);
        if names.is_empty() {
            return Ok(());
        }
        let rec = ReclaimRec { dir: dir_loc(&self.spec), xid: self.stamp.fxid as u32, retire_below: None, names };
        let dir = &self.table_dir;
        self.sink.reclaim(&rec, &mut || apply_reclaim(&mut *vfs, dir, &rec))
    }

    fn share_seq(&mut self, seq: Arc<AtomicU32>) {
        self.seq = seq;
    }
}

/// The facts a parallel-ingest driver needs to build an engine for one
/// statement (the M3-I split-crate seam currency).
#[derive(Debug, Clone)]
pub struct ParFacts {
    pub schema: Vec<ColSchema>,
    pub spec: PartSpec,
    pub table_dir: String,
    pub fxid: u64,
    pub policy: PartCutPolicy,
    /// Structural-election facts (TY-1): the parallel seal path must elect
    /// exactly as serial would — byte-identical-parts law.
    pub structural: StructuralPolicy,
    pub base_seq: u32,
}

/// The per-backend writer registry (session-envelope state — M3-H owns
/// placement + the xact-callback registration; this object owns the
/// semantics).
#[derive(Default)]
pub struct WriterRegistry {
    writers: BTreeMap<u64, TableWriter>,
    /// Per (relfilenumber, fxid): the tmp-name seq its writers share until the transaction ends.
    seqs: BTreeMap<(u64, u64), Arc<AtomicU32>>,
}

impl WriterRegistry {
    pub fn new() -> WriterRegistry {
        WriterRegistry::default()
    }

    pub fn len(&self) -> usize {
        self.writers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.writers.is_empty()
    }

    /// D4537 T18c: the registry's own heap bytes: its two maps' nodes and the shared tmp-name seqs (a writer's buffers are the
    /// writer's own charges).
    pub fn shell_bytes(&self) -> usize {
        const NODE: usize = 2 * size_of::<usize>();
        self.writers.len() * (size_of::<(u64, TableWriter)>() + NODE) + self.seqs.len() * (size_of::<((u64, u64), Arc<AtomicU32>)>() + NODE + 2 * size_of::<usize>() + size_of::<AtomicU32>())
    }

    /// IXPERF F5b: no writer and no shared tmp-name seq: [`WriterRegistry::at_eoxact`] has nothing to purge.
    pub fn is_idle(&self) -> bool {
        self.writers.is_empty() && self.seqs.is_empty()
    }

    pub fn registered_stamps(&self) -> impl Iterator<Item = (u64, TxnStamp)> + '_ {
        self.writers.iter().map(|(k, w)| (*k, w.stamp()))
    }

    /// The stamp of the table's registered writer, if any (COPYLOSS: the caller decides whether a writer under another
    /// stamp is stale before [`Self::get_or_open`] or [`Self::take_for_publish`] would abort it).
    pub fn stamp_of(&self, relfilenumber: u64) -> Option<TxnStamp> {
        self.writers.get(&relfilenumber).map(|w| w.stamp())
    }

    /// Get the table's writer, EVICTING a stale one first (stamp mismatch ⇒
    /// abort + drop, the old-writer law). `open` constructs a fresh writer
    /// when none (or a stale one) is registered.
    pub fn get_or_open(
        &mut self,
        vfs: &mut dyn WriteVfs,
        relfilenumber: u64,
        stamp: TxnStamp,
        open: impl FnOnce() -> WriteResult<TableWriter>,
    ) -> WriteResult<&mut TableWriter> {
        if let Some(w) = self.writers.get_mut(&relfilenumber) {
            if w.stamp() != stamp {
                w.abort(vfs)?;
                self.writers.remove(&relfilenumber);
            }
        }
        if !self.writers.contains_key(&relfilenumber) {
            let mut w = open()?;
            if w.stamp() != stamp {
                return Err(WriteError::Contract {
                    detail: "opened writer carries a foreign stamp",
                });
            }
            let seq = self.seqs.entry((relfilenumber, stamp.fxid)).or_insert_with(|| w.seq.clone());
            w.share_seq(seq.clone());
            self.writers.insert(relfilenumber, w);
        }
        Ok(self.writers.get_mut(&relfilenumber).expect("just inserted"))
    }

    /// Give a writer the registry does not hold (a seal's) the tmp-name seq of the table's transaction.
    pub fn share_seq(&mut self, relfilenumber: u64, w: &mut TableWriter) {
        let seq = self.seqs.entry((relfilenumber, w.stamp.fxid)).or_insert_with(|| w.seq.clone());
        w.share_seq(seq.clone());
    }

    /// Remove the writer for publishing, re-checking the stamp (the
    /// finish-time staleness recheck): a stale writer is aborted and None
    /// returned — never published.
    pub fn take_for_publish(
        &mut self,
        vfs: &mut dyn WriteVfs,
        relfilenumber: u64,
        stamp: TxnStamp,
    ) -> WriteResult<Option<TableWriter>> {
        match self.writers.remove(&relfilenumber) {
            None => Ok(None),
            Some(mut w) => {
                if w.stamp() != stamp {
                    w.abort(vfs)?;
                    return Ok(None);
                }
                Ok(Some(w))
            }
        }
    }

    /// End-of-transaction purge — IDENTICAL on commit and abort: everything
    /// still here is abandoned by construction (the successful statement
    /// already took its writer at publish). Abort each and clear.
    pub fn at_eoxact(&mut self, vfs: &mut dyn WriteVfs) -> WriteResult<()> {
        self.seqs.clear();
        let mut writers = std::mem::take(&mut self.writers);
        for (_, w) in writers.iter_mut() {
            w.abort(vfs)?;
        }
        Ok(())
    }
}
