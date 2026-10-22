//! The cluster merge: parallel COPY FROM parquet into a pgrcolumnar table
//! that declares `sorted_by`, producing parts whose key ranges are DISJOINT
//! across the whole table (lane load-sort, 2026-09-19; parquet-load.md §14).
//!
//! ## Why
//!
//! The seal sorts every part by the declared key, so a file-order load is
//! clustered WITHIN each part only. A parquet file whose row groups are
//! each sorted on the key, but some of which restart low (a file written
//! in a few sorted batches), leaves a file-order load with parts whose key
//! ranges overlap others'; a globally clustered table then needed an
//! external sort before the load. This module makes the COPY itself
//! produce parts with disjoint key ranges.
//!
//! ## Shape: a streaming k-way merge over row groups, planned from exact bounds
//!
//! 1. KEY PRE-PASS (rtpool tasks, one per row group): decode ONLY the key
//!    bindings through the real conversions ([`DecodeShared::decode_words`])
//!    and take each row group's composite lower/upper bound — the minimum
//!    and maximum row under the seal's own comparator ([`key_cmp`]: signed /
//!    unsigned / float words, NULLS FIRST/LAST as absolute placement, DESC
//!    inverting values). Footer statistics are never consulted: they are
//!    advisory in the format and would have to be re-converted per binding.
//! 2. PLAN (leader): the decode order is row groups sorted by (lower bound,
//!    file index) — the order in which a merge cursor walking the key space
//!    first NEEDS each group. The residency the merge requires is the
//!    maximum number of groups whose [lower, upper] interval contains one
//!    key (the interval depth `D`, a sweep); with the per-group resident
//!    estimate ([`est_resident`]: compressed chunks + key lanes + blocks in
//!    flight) it is checked against the budget BEFORE anything is decoded,
//!    and a file that does not fit is handed back to the file-order driver
//!    (the load still succeeds; only the layout is the per-part one). A file
//!    of 226 row groups with 8 restarts has D = 9 (one sweep group + the 8
//!    restarts, all released 15 % into the load).
//! 3. DECODE (rtpool tasks, on the leader's DEMANDS): a resident group is
//!    its KEY LANES in key order (decoded once, 9 bytes per row per key)
//!    plus a table of data BLOCKS of `grain` rows, each decoded when the
//!    merge's runs reach it ([`decode_block`]: the group's reader is a
//!    cursor advanced in order, `BLOCK_AHEAD` blocks ahead of the merge)
//!    and evicted once every spec that gathers from it is resolved — a
//!    group is never whole in memory (a whole 105-column group is ~0.9 GB
//!    lane-major; nine of them plus read-ahead swapped a 30 GB box, the
//!    first design's lesson). A group whose rows are NOT in key order is
//!    decoded whole at its key demand and sliced into key-order blocks
//!    then (the file's rare case).
//! 4. MERGE (leader): the active set is every group with keys whose rows
//!    are not exhausted; the pending list is the rest in decode order. Each
//!    step takes the active head with the smallest (key, file index), finds
//!    by binary search over the key lanes how far that group can be emitted
//!    before the runner-up — the next active head, or the pending front's
//!    lower bound — would come first, and emits that RUN. Runs are appended
//!    to the current output chunk spec; a spec closes at exactly the engine
//!    grain and waits until its blocks have landed, then is resolved (its
//!    blocks pinned) and handed to a build task, in chunk order.
//! 5. BUILD (rtpool tasks, one per output chunk): copy the spec's runs
//!    from the pinned blocks column at a time into chunk-local column
//!    buffers ([`ColChunk`]: what the engine's normalize would build).
//! 6. APPEND (leader): chunks go through [`ParSession::append_col_chunk`] in
//!    chunk order; the final sub-grain tail through `append_row`. The
//!    engine's cut cursor then sees a globally key-ordered stream, so the
//!    seal's per-part sort is verify-only and every part's range is
//!    disjoint from every other's.
//!
//! ## ORDER LAW under `sorted_by`
//!
//! The insertion order is the total order (key, file index, row-in-group):
//! a stable sort of the file by the declared key. It is a pure function of
//! the file and the declaration (no worker schedule reaches it), so the part
//! partition is reproducible, and it equals a serial COPY of the same file
//! pre-sorted with the same stable sort. It is NOT the file order the
//! `parquet_pgrc` ORDER LAW describes — that law now applies to tables
//! without a declared key, and to the fall-back cases named in
//! [`copy_clustered`]. Every row is present exactly once either way.
//!
//! ## Error parity
//!
//! Workers raise nothing. A decode failure in either pass is replayed by
//! the leader through the SERIAL machinery over row groups 0..=f in FILE
//! order ([`replay_rg_serial`]), so the error a user sees is the first one
//! a serial COPY meets, with the same message, sqlstate and line context.
//!
//! ## Opt-out
//!
//! `PGRUST_PARQUET_CLUSTER=0` keeps the file-order driver for a `sorted_by`
//! table. No other
//! knob: the declaration is the switch.

use pgrc_write::held::HeldCharge;
use std::cmp::Ordering as Ord_;
use std::collections::{BTreeMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use pgsync::OnceLock;
use std::time::Duration;

use pgsync::{lock, Mutex};

use mcx::Mcx;
use types_error::{PgError, PgResult};
use types_rel::Relation;

use backend_progress::pgstat_progress_update_param;
use backend_progress::progress::{PROGRESS_COPY_BYTES_PROCESSED, PROGRESS_COPY_TUPLES_PROCESSED};

use pgrc_ingest_par::ParSession;
use pgrc_write::format::class::ColSchema;
use pgrc_write::ingest::{ColBuffer, NoExternalDetoast, RawDatum};
use pgrc_write::par::{ColChunk, ParIngestOpts};
use pgrc_write::structural::{ClusterKeyDecl, NullsOrder, SortDir};
use pgrc_write::WriteError;
use pgrc_write::format::class::StorageClass;

use crate::from::CopyFromState;
use crate::parallel::{parquet_budget_bytes_auto, ptrace, ptrace_enabled, LoadCharge};
use crate::parquet_pgrc::{lanes_held, replay_rg_serial, DecodeShared, Fragment, Lane};

/// The feed-side tunnel error (the `parquet_pgrc` convention): a PgError
/// is stashed on the leader; the engine sees a contract error and the
/// driver re-raises the stashed one.
const STASHED: WriteError = WriteError::Contract {
    detail: "parquet cluster feed error (see raised error)",
};

/// The planning estimate of one RESIDENT group: its compressed column
/// chunks (the cursor holds them), its key lanes (9 bytes per row per
/// key), and the blocks in flight (`BLOCK_AHEAD` + the specs' backlog =
/// `RESIDENT_BLOCKS`) at 16 bytes per cell. A group decoded whole (rows
/// out of key order, the file's rare case) is not planned for.
const RESIDENT_BLOCKS: u64 = 14;

fn est_resident(base: &DecodeShared, file_idx: usize, nkeys: u64) -> u64 {
    let rows = base.meta.row_groups.get(file_idx).map_or(0, |rg| rg.num_rows.max(0) as u64);
    let natts = base.byval.len() as u64;
    parquet_read::rg_compressed_bytes(&base.meta, file_idx, &base.plan.cols)
        + rows * nkeys * 9
        + RESIDENT_BLOCKS * base.grain * natts * 16
}

/// `PGRUST_PARQUET_CLUSTER=0` opts a `sorted_by` table out of the merge.
fn cluster_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| !std::env::var("PGRUST_PARQUET_CLUSTER").is_ok_and(|v| v.trim() == "0"))
}

// ---------------------------------------------------------------------------
// The key
// ---------------------------------------------------------------------------

/// How one key column's stored word compares (the seal's `KeySlot` shapes
/// for the word classes; text / fixed keys are not merged here).
#[derive(Clone, Copy)]
enum KeyClass {
    Signed,
    Unsigned,
    F32,
    F64,
}

/// One declared key column, resolved to its parquet binding.
#[derive(Clone, Copy)]
struct KeyCol {
    /// Index into `plan.bindings`.
    binding: usize,
    class: KeyClass,
    dir: SortDir,
    nulls: NullsOrder,
}

/// PostgreSQL float ordering: NaN sorts after every number and equals
/// itself (`float8_cmp_internal`).
fn pg_float_cmp(a: f64, b: f64) -> Ord_ {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ord_::Equal,
        (true, false) => Ord_::Greater,
        (false, true) => Ord_::Less,
        (false, false) => a.partial_cmp(&b).unwrap_or(Ord_::Equal),
    }
}

/// Compare two cells of one key column (`None` = NULL). NULLS FIRST/LAST
/// is absolute placement; DESC inverts value comparisons only — the seal's
/// `key_cmp`, on words.
fn key_cmp(k: &KeyCol, a: Option<u64>, b: Option<u64>) -> Ord_ {
    let ord = match (a, b) {
        (None, None) => return Ord_::Equal,
        (None, _) => return if k.nulls == NullsOrder::First { Ord_::Less } else { Ord_::Greater },
        (_, None) => return if k.nulls == NullsOrder::First { Ord_::Greater } else { Ord_::Less },
        (Some(x), Some(y)) => match k.class {
            KeyClass::Signed => (x as i64).cmp(&(y as i64)),
            KeyClass::Unsigned => x.cmp(&y),
            KeyClass::F32 => pg_float_cmp(f32::from_bits(x as u32) as f64, f32::from_bits(y as u32) as f64),
            KeyClass::F64 => pg_float_cmp(f64::from_bits(x), f64::from_bits(y)),
        },
    };
    if k.dir == SortDir::Desc {
        ord.reverse()
    } else {
        ord
    }
}

/// One composite key value (a bound, or a copied head).
type KeyVal = Vec<Option<u64>>;

fn composite_cmp(keys: &[KeyCol], a: &[Option<u64>], b: &[Option<u64>]) -> Ord_ {
    for (i, k) in keys.iter().enumerate() {
        let o = key_cmp(k, a[i], b[i]);
        if o != Ord_::Equal {
            return o;
        }
    }
    Ord_::Equal
}

/// One key column's cell (`None` = NULL) out of a word lane.
#[inline]
fn lane_cell(lane: &Lane, row: usize) -> Option<u64> {
    match lane {
        Lane::Words { words, nulls } => {
            if nulls[row] {
                None
            } else {
                Some(words[row])
            }
        }
        Lane::Blobs { .. } => unreachable!("key lanes are word lanes (admission)"),
    }
}

/// Compare rows `a` and `b` across the key lanes.
fn cmp_rows(keys: &[KeyCol], lanes: &[&Lane], a: usize, b: usize) -> Ord_ {
    for (i, k) in keys.iter().enumerate() {
        let o = key_cmp(k, lane_cell(lanes[i], a), lane_cell(lanes[i], b));
        if o != Ord_::Equal {
            return o;
        }
    }
    Ord_::Equal
}

/// Compare row `a` against a composite value.
fn row_val(lanes: &[&Lane], row: usize) -> KeyVal {
    lanes.iter().map(|l| lane_cell(l, row)).collect()
}

/// The stable permutation sorting `n` rows by the key; `None` when the rows
/// are already ordered (the verification is the attestation, as in the
/// seal).
fn sort_perm(keys: &[KeyCol], lanes: &[&Lane], n: usize) -> Option<Vec<u32>> {
    if (1..n).all(|r| cmp_rows(keys, lanes, r - 1, r) != Ord_::Greater) {
        return None;
    }
    let mut perm: Vec<u32> = (0..n as u32).collect();
    perm.sort_by(|&a, &b| cmp_rows(keys, lanes, a as usize, b as usize));
    Some(perm)
}

// ---------------------------------------------------------------------------
// Groups, blocks, specs
// ---------------------------------------------------------------------------

/// One block of a group's rows in KEY order (`Shared::block` rows, the
/// engine grain; the last one shorter), pinned by the build specs that
/// gather from it; the drop releases its bytes from the residency gauge.
struct Block {
    frag: Fragment,
    bytes: u64,
    live: Arc<AtomicU64>,
}

impl Block {
    fn new(frag: Fragment, live: &Arc<AtomicU64>) -> Arc<Block> {
        let bytes = frag.heap_bytes();
        live.fetch_add(bytes, Ordering::SeqCst);
        Arc::new(Block {
            frag,
            bytes,
            live: Arc::clone(live),
        })
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        self.live.fetch_sub(self.bytes, Ordering::SeqCst);
    }
}

/// A sorted group's data cursor: the reader (the group's compressed column
/// chunks) advanced one block per demand, in order.
struct Cursor {
    /// CONVERT wave 3 (cvwrite job 3): the load's owner's charge on `batches`, brought to them after each fill on the
    /// pass's thread. Declared first: it drops before the batches are freed.
    charge: HeldCharge,
    reader: parquet_read::RowGroupReader,
    batches: Vec<parquet_read::ColumnBatch>,
}

/// One row group in the merge: its key lanes (key order) and its data as
/// a table of blocks decoded on demand behind the merge and evicted behind
/// the resolve frontier — a sorted group is never whole in memory. A group
/// whose rows are NOT in key order is decoded whole at its key demand and
/// sliced into key-order blocks then (the file's rare case).
struct Group {
    fseq: u64,
    file_idx: usize,
    rows: u32,
    /// CONVERT W3-2b: the key lanes' charge on the load's owner (set with them; declared first: drops first).
    keys_charge: OnceLock<HeldCharge>,
    /// Key lanes in KEY order, one per `KeyCol`.
    keys: OnceLock<Vec<Lane>>,
    resorted: AtomicBool,
    blocks: Mutex<Vec<Option<Arc<Block>>>>,
    cursor: Mutex<Option<Cursor>>,
    /// colload (n): which worker owns the cursor ([`DecodeTurn`]); a demand that finds it owned returns at once.
    turn: DecodeTurn,
    /// Blocks decoded (the cursor's position; whole groups: all).
    decoded: AtomicU32,
    /// Blocks demanded (published; the cursor decodes through them).
    demanded: AtomicU32,
    /// Leader-side: the block the decode-ahead aims at, blocks evicted.
    target: AtomicU32,
    evicted: AtomicU32,
}

impl Group {
    fn nblocks(&self, block: u32) -> u32 {
        self.rows.div_ceil(block)
    }

    /// Key column `j` (in `KeyCol` order) of the i-th row IN KEY ORDER.
    #[inline]
    fn cell(&self, j: usize, i: usize) -> Option<u64> {
        lane_cell(&self.keys.get().expect("keys ready before activation")[j], i)
    }

    fn cmp_val(&self, keys: &[KeyCol], i: usize, v: &[Option<u64>]) -> Ord_ {
        for (j, k) in keys.iter().enumerate() {
            let o = key_cmp(k, self.cell(j, i), v[j]);
            if o != Ord_::Equal {
                return o;
            }
        }
        Ord_::Equal
    }

    fn val(&self, keys: &[KeyCol], i: usize) -> KeyVal {
        (0..keys.len()).map(|j| self.cell(j, i)).collect()
    }

    fn has_blocks(&self, b0: u32, b1: u32) -> bool {
        let t = lock(&self.blocks);
        (b0..=b1).all(|b| t.get(b as usize).is_some_and(Option::is_some))
    }
}

/// Two heads under (key, file seq) — no materialized key values (the
/// leader's per-run cost).
fn cmp_heads(keys: &[KeyCol], a: &Active, b: &Active) -> Ord_ {
    for (j, k) in keys.iter().enumerate() {
        let o = key_cmp(k, a.g.cell(j, a.pos as usize), b.g.cell(j, b.pos as usize));
        if o != Ord_::Equal {
            return o;
        }
    }
    a.g.fseq.cmp(&b.g.fseq)
}

/// A run of `n` key-ordered rows of one group from `start`.
struct Run {
    g: Arc<Group>,
    start: u32,
    n: u32,
}

/// One output chunk as the merge emits it: runs by group + row range.
struct ChunkSpec {
    runs: memledger::ChargedVec<Run>,
    rows: u32,
}

impl ChunkSpec {
    fn new(owner: Option<memledger::Owner>) -> ChunkSpec {
        ChunkSpec {
            runs: memledger::ChargedVec::new(owner, memledger::Kind::Region),
            rows: 0,
        }
    }

    fn push_run(&mut self, g: &Arc<Group>, start: u32, n: u32) {
        if let Some(last) = self.runs.last_mut() {
            if Arc::ptr_eq(&last.g, g) && last.start + last.n == start {
                last.n += n;
                self.rows += n;
                return;
            }
        }
        self.runs.with_vec(|v| v.push(Run { // alloc: Exec charged self.runs.owner()
            g: Arc::clone(g),
            start,
            n,
        }));
        self.rows += n;
    }
}

/// A run with its blocks pinned (`b0` = the index of `blocks[0]`).
struct ResolvedRun {
    blocks: Vec<Arc<Block>>,
    b0: u32,
    start: u32,
    n: u32,
}

/// The build task's input: every row it gathers is resident.
struct BuildSpec {
    runs: Vec<ResolvedRun>,
}

/// What the decode tasks are asked for, in demand order.
enum Demand {
    Keys(Arc<Group>),
    Block(Arc<Group>, u32),
}

enum Out<T> {
    Done(T),
    Failed(String),
}

/// The leader's handoff board (the `parquet_pgrc::ReadyBoard` discipline:
/// wakers registered under the same lock as the inserts, no lost wake).
struct Boards {
    wakers: memledger::ChargedVec<u64>,
    /// Key pre-pass results by FILE seq.
    bounds: BTreeMap<u64, Out<(KeyVal, KeyVal)>>,
    /// Decode demands served (any: the leader re-reads the group tables).
    served: u64,
    /// Decode failures by FILE seq.
    fails: BTreeMap<u64, String>,
    /// Specs handed to the build tasks, by chunk id (taken by the builder).
    specs: BTreeMap<u64, BuildSpec>,
    /// Built chunks by chunk id.
    built: BTreeMap<u64, Out<ColChunk>>,
}

struct Shared {
    /// Decode demands by morsel id (taken by the worker).
    demands: Mutex<memledger::ChargedVec<Option<Demand>>>,
    base: Arc<DecodeShared>,
    keys: Vec<KeyCol>,
    /// Decode seq → FILE seq (an index into `base.rg_order` / `row_base`);
    /// set once by the plan, before any decode task is published.
    order: OnceLock<memledger::ChargedVec<u64>>,
    /// Residency estimate per FILE seq (the plan's check).
    est: memledger::ChargedVec<u64>,
    /// Rows per block = the engine grain.
    block: u32,
    /// The session's column schemas (set before the first spec is built).
    schema: OnceLock<Vec<ColSchema>>,
    /// By attidx: the reader validated the column's strings as UTF-8.
    utf8: Vec<bool>,
    boards: Mutex<Boards>,
    /// Bytes of resident blocks (the residency gauge).
    live: Arc<AtomicU64>,
    /// D3808: the decode window's charge (the plan's residency `need`, held
    /// by its `LoadCharge`); 0 before the plan. The merge tells the engine's
    /// memory gate the part of it not yet resident (`ParSession::set_ahead`).
    window: AtomicU64,
}

impl Shared {
    fn post<T>(&self, f: impl FnOnce(&mut Boards) -> T) -> T {
        let mut g = lock(&self.boards);
        let r = f(&mut g);
        let wakers = core::mem::take(&mut g.wakers);
        drop(g);
        for &w in wakers.iter() {
            let _ = waiter::unpark_word(w);
        }
        r
    }

    fn rg_file_idx(&self, file_seq: u64) -> usize {
        self.base.rg_order[file_seq as usize]
    }

    fn order(&self) -> &[u64] {
        self.order.get().map(memledger::ChargedVec::as_slice).unwrap_or(&[])
    }
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

struct KeyPass {
    shared: Arc<Shared>,
}

impl runtime::TaskSetWork for KeyPass {
    fn run_morsel(&self, _worker: usize, range: runtime::MorselRange) {
        let _acct = memledger::charge::enter_as(memledger::charge::context_of(self.shared.base.acct).0);
        for seq in range {
            if self.shared.base.cancelled.load(Ordering::SeqCst) {
                continue;
            }
            let sh = &self.shared;
            // unwind-ok: worker-containment
            let out = match catch_unwind(AssertUnwindSafe(|| {
                let mut bindings: memledger::ChargedVec<usize> = memledger::ChargedVec::new(sh.base.owner, memledger::Kind::Region);
                bindings.with_vec(|v| v.extend(sh.keys.iter().map(|k| k.binding))); // alloc: Exec charged bindings.owner()
                // CONVERT W3-2b: the row group's key lanes on the load's owner while the pre-pass scans them
                let mut kc = HeldCharge::new(sh.base.owner);
                let owned: Vec<Lane> = sh
                    .base
                    .decode_words(sh.rg_file_idx(seq), &bindings, &mut kc)?
                    .into_iter()
                    .map(|(words, nulls)| Lane::Words { words, nulls })
                    .collect(); // alloc: Exec charged kc
                kc.sync(lanes_held(&owned));
                let mut lanes: memledger::ChargedVec<&Lane> = memledger::ChargedVec::new(sh.base.owner, memledger::Kind::Region);
                lanes.with_vec(|v| v.extend(owned.iter())); // alloc: Exec charged lanes.owner()
                let n = match owned.first() {
                    Some(Lane::Words { words, .. }) => words.len(),
                    _ => 0,
                };
                let (mut lo, mut hi) = (0usize, 0usize);
                for r in 1..n {
                    if cmp_rows(&sh.keys, &lanes, r, lo) == Ord_::Less {
                        lo = r;
                    }
                    if cmp_rows(&sh.keys, &lanes, r, hi) != Ord_::Less {
                        hi = r;
                    }
                }
                let bounds = (row_val(&lanes, lo), row_val(&lanes, hi));
                drop(kc);
                Ok::<_, Box<PgError>>(bounds)
            })) {
                Ok(Ok(b)) => Out::Done(b),
                Ok(Err(e)) => Out::Failed(e.message().to_string()), // alloc: Cold
                Err(_) => Out::Failed("key pre-pass worker panicked".to_string()), // alloc: Cold
            };
            sh.post(|b| b.bounds.insert(seq, out));
        }
    }

    fn finalize(&self) {}
}

/// Append one gathered row to a lane-major fragment (the whole-group
/// re-sort's slicing into key-order blocks).
fn push_raw(frag: &mut Fragment, row: &[RawDatum<'_>]) {
    for (lane, d) in frag.lanes.iter_mut().zip(row.iter()) {
        match lane {
            Lane::Words { words, nulls } => match d {
                RawDatum::Word(w) => {
                    words.push(*w); // alloc: Exec charged frag.charge
                    nulls.push(false); // alloc: Exec charged frag.charge
                }
                _ => {
                    words.push(0); // alloc: Exec charged frag.charge
                    nulls.push(true); // alloc: Exec charged frag.charge
                }
            },
            Lane::Blobs { arena, spans, nulls } => match d {
                RawDatum::Bytes(b) => {
                    let off = arena.len() as u32;
                    arena.extend_from_slice(b); // alloc: Exec charged frag.charge
                    spans.push((off, b.len() as u32)); // alloc: Exec charged frag.charge
                    nulls.push(false); // alloc: Exec charged frag.charge
                }
                _ => {
                    spans.push((0, 0)); // alloc: Exec charged frag.charge
                    nulls.push(true); // alloc: Exec charged frag.charge
                }
            },
        }
    }
    frag.rows += 1;
}

/// The key demand: the group's key lanes in key order. A group out of key
/// order is decoded whole here and sliced into key-order blocks.
fn decode_keys(sh: &Shared, g: &Group) -> PgResult<()> {
    let base = &sh.base;
    let mut bindings: memledger::ChargedVec<usize> = memledger::ChargedVec::new(base.owner, memledger::Kind::Region);
    bindings.with_vec(|v| v.extend(sh.keys.iter().map(|k| k.binding))); // alloc: Exec charged bindings.owner()
    // CONVERT W3-2b: the group's key lanes on the load's owner for the group's life (the charge moves into the group)
    let mut kc = HeldCharge::new(base.owner);
    let mut lanes: Vec<Lane> = base
        .decode_words(g.file_idx, &bindings, &mut kc)?
        .into_iter()
        .map(|(words, nulls)| Lane::Words { words, nulls })
        .collect(); // alloc: Exec charged kc
    kc.sync(lanes_held(&lanes));
    let perm = {
        let mut refs: memledger::ChargedVec<&Lane> = memledger::ChargedVec::new(base.owner, memledger::Kind::Region);
        refs.with_vec(|v| v.extend(lanes.iter())); // alloc: Exec charged refs.owner()
        sort_perm(&sh.keys, &refs, g.rows as usize) // alloc: Exec charged kc
    };
    if let Some(perm) = perm {
        kc.sync(lanes_held(&lanes) + perm.capacity() as u64 * 4);
        g.resorted.store(true, Ordering::SeqCst);
        let whole = base.decode_whole(g.file_idx)?;
        let natts = base.byval.len();
        let nb = g.nblocks(sh.block);
        let mut blocks: Vec<Option<Arc<Block>>> = ::mcx::stdvec::try_vec(nb as usize, "parquet_cluster")?;
        let mut row: Vec<RawDatum<'_>> = ::mcx::stdvec::try_vec(natts, "parquet_cluster")?;
        for b in 0..nb {
            let lo = b * sh.block;
            let hi = (lo + sh.block).min(g.rows);
            // CONVERT W3-2b: push_raw grows it uncharged until the shrink syncs it (one block's rows)
            let mut frag = Fragment::empty(natts, &base.byval, (hi - lo) as usize, base.owner)?;
            for i in lo..hi {
                row.clear();
                gather_row(&whole, perm[i as usize] as usize, natts, &mut row);
                push_raw(&mut frag, &row);
            }
            frag.shrink();
            blocks.push(Some(Block::new(frag, &sh.live)));
        }
        lanes = lanes
            .into_iter()
            .map(|l| match l {
                Lane::Words { words, nulls } => Lane::Words {
                    words: perm.iter().map(|&p| words[p as usize]).collect(), // alloc: Exec charged kc
                    nulls: perm.iter().map(|&p| nulls[p as usize]).collect(), // alloc: Exec charged kc
                },
                b => b,
            })
            .collect(); // alloc: Exec charged kc
        kc.sync(lanes_held(&lanes) + perm.capacity() as u64 * 4);
        *lock(&g.blocks) = blocks;
        g.decoded.store(nb, Ordering::SeqCst);
    }
    kc.sync(lanes_held(&lanes));
    let _ = g.keys_charge.set(kc);
    let _ = g.keys.set(lanes);
    Ok(())
}

/// colload (n): a group's decode turn. One worker at a time owns the group's cursor; a demand that arrives while it is
/// owned marks the turn RERUN and returns at once (its worker goes back to the pool), and the owner, before it lets
/// go, decodes again through every block demanded meanwhile. Before (n) the second demand's worker slept on the cursor
/// mutex for the whole of the owner's pass and then found its block done: on a 100M-row load (box 10, 2026-10-14) 185
/// worker-s asleep in DecodePass while normalize chunks sat unclaimed. One word, so the hand-over is one location's
/// modification order: a RERUN written before the owner's release CAS makes that CAS fail (the owner decodes again,
/// reading `demanded` after the demand's CAS), one written after finds IDLE and takes the turn itself.
struct DecodeTurn(AtomicU32);

const TURN_IDLE: u32 = 0;
const TURN_RUN: u32 = 1;
const TURN_RERUN: u32 = 2;
/// D4919: the owner's pass failed: the cursor may have stopped mid-block, so no demand takes the turn again.
const TURN_FAILED: u32 = 3;

impl DecodeTurn {
    fn new() -> DecodeTurn {
        DecodeTurn(AtomicU32::new(TURN_IDLE))
    }

    /// A demand arrives: true = this worker owns the turn now and decodes; false = the owner decodes through it, or a
    /// failed pass closed the turn (`TURN_FAILED`: nobody decodes the group again).
    fn enter(&self) -> bool {
        loop {
            let s = self.0.load(Ordering::SeqCst);
            if s == TURN_FAILED {
                return false;
            }
            let next = if s == TURN_IDLE { TURN_RUN } else { TURN_RERUN };
            if self.0.compare_exchange(s, next, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                return s == TURN_IDLE;
            }
        }
    }

    /// The owner ended a pass: true = a demand arrived meanwhile (decode again, still owning the turn).
    fn leave(&self) -> bool {
        match self.0.compare_exchange(TURN_RUN, TURN_IDLE, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => false,
            Err(_) => {
                self.0.store(TURN_RUN, Ordering::SeqCst);
                true
            }
        }
    }
}

/// D4919: the owner's error or panic leaves the turn FAILED, never IDLE. The pass can fail after the cursor advanced: a
/// column's `read_batch` after the columns before it read their rows, or a lane fill after the reader counted the batch.
/// A later demand that took an IDLE turn decoded from there into the next block slot, so every block after it held rows
/// shifted from their key order positions and the group's last block came up short of the rows its runs slice (a build
/// worker's slice panic). The session fails on the recorded error; its remaining demands for the group decode nothing.
struct TurnGuard<'a>(&'a DecodeTurn);

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        self.0 .0.store(TURN_FAILED, Ordering::SeqCst);
    }
}

/// The block demand: block `b` decoded, by this worker if it takes the group's turn ([`DecodeTurn`]), else by the
/// worker that owns it (the leader keeps at most two demands in flight per group; a cursor is one worker's work).
fn decode_block(sh: &Shared, g: &Group, b: u32) -> PgResult<()> {
    if g.decoded.load(Ordering::SeqCst) > b {
        return Ok(());
    }
    if !g.turn.enter() {
        return Ok(());
    }
    let guard = TurnGuard(&g.turn);
    loop {
        decode_pass(sh, g, b)?;
        if !g.turn.leave() {
            core::mem::forget(guard);
            return Ok(());
        }
    }
}

/// One pass of the turn's owner: advance the group's cursor through block `b` and every block demanded meanwhile.
fn decode_pass(sh: &Shared, g: &Group, b: u32) -> PgResult<()> {
    let base = &sh.base;
    let natts = base.byval.len();
    let mut cur = lock(&g.cursor);
    if cur.is_none() {
        let reader = parquet_read::RowGroupReader::open_on(
            &base.file,
            &base.path,
            &base.meta,
            g.file_idx,
            &base.plan.cols,
            &base.plan.vutf8,
            base.owner,
        )?;
        base.bytes_read.fetch_add(reader.compressed_bytes, Ordering::Relaxed);
        let batches = base.plan.make_batches(&base.meta);
        *cur = Some(Cursor { charge: HeldCharge::new(base.owner), reader, batches });
    }
    let c = cur.as_mut().expect("opened");
    const BATCH_ROWS: usize = 1024;
    while g.decoded.load(Ordering::SeqCst) < g.demanded.load(Ordering::SeqCst).max(b + 1) {
        if base.cancelled.load(Ordering::SeqCst) {
            return Err(Box::new(PgError::error("parquet parallel decode cancelled".to_string()))); // alloc: Cold
        }
        let want = (c.reader.rows_remaining().min(u64::from(sh.block))) as usize;
        let mut frag = Fragment::empty(natts, &base.byval, want, base.owner)?;
        while (frag.rows as usize) < want {
            let n = (want - frag.rows as usize).min(BATCH_ROWS);
            c.reader.read_batches(&mut c.batches, n)?;
            c.charge.sync(parquet_read::batches_held(&c.batches));
            base.lanes_append(&mut frag, &c.batches, 0, n)?;
        }
        frag.shrink();
        let idx = g.decoded.load(Ordering::SeqCst);
        {
            let mut t = lock(&g.blocks);
            if t.is_empty() {
                ::mcx::stdvec::try_reserve_more(&mut t, g.nblocks(sh.block) as usize, "parquet_cluster")?;
                t.extend(core::iter::repeat_with(|| None).take(g.nblocks(sh.block) as usize));
            }
            t[idx as usize] = Some(Block::new(frag, &sh.live));
        }
        g.decoded.store(idx + 1, Ordering::SeqCst);
    }
    Ok(())
}

struct DecodePass {
    shared: Arc<Shared>,
}

impl runtime::TaskSetWork for DecodePass {
    fn run_morsel(&self, _worker: usize, range: runtime::MorselRange) {
        let _acct = memledger::charge::enter_as(memledger::charge::context_of(self.shared.base.acct).0);
        for id in range {
            let sh = &self.shared;
            let d = lock(&sh.demands)[id as usize].take();
            let Some(d) = d else { continue };
            if sh.base.cancelled.load(Ordering::SeqCst) {
                continue;
            }
            let (fseq, r) = match &d {
                // unwind-ok: worker-containment
                Demand::Keys(g) => (g.fseq, catch_unwind(AssertUnwindSafe(|| decode_keys(sh, g)))),
                Demand::Block(g, b) => (g.fseq, catch_unwind(AssertUnwindSafe(|| decode_block(sh, g, *b)))),
            };
            let fail = match r {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(e.message().to_string()), // alloc: Cold
                Err(_) => Some("decode worker panicked".to_string()), // alloc: Cold
            };
            drop(d);
            sh.post(|b| {
                b.served += 1;
                if let Some(detail) = fail {
                    b.fails.entry(fseq).or_insert(detail);
                }
            });
        }
    }

    fn finalize(&self) {}
}

struct BuildPass {
    shared: Arc<Shared>,
    natts: usize,
    grain: u32,
}

impl BuildPass {
    /// The spec's rows in column form: each run is a contiguous row range
    /// of its key-ordered blocks, so every column is a sequence of slice
    /// copies in run order (the rows `push_row` + normalize would replay).
    fn build(&self, spec: &BuildSpec) -> Result<ColChunk, WriteError> {
        let schema = self.shared.schema.get().ok_or(WriteError::Contract {
            detail: "column chunk built before the session schema",
        })?;
        if schema.len() != self.natts {
            return Err(WriteError::Contract {
                detail: "session schema width differs from the decode width",
            });
        }
        let block = self.shared.block;
        let mut segs: memledger::ChargedVec<(&Fragment, usize, usize)> = memledger::ChargedVec::new(self.shared.base.owner, memledger::Kind::Region);
        for run in spec.runs.iter() {
            let (mut r, end) = (run.start, run.start + run.n);
            while r < end {
                let b = r / block;
                let hi = end.min((b + 1) * block);
                let blk = &run.blocks[(b - run.b0) as usize];
                segs.with_vec(|v| v.push((&blk.frag, (r - b * block) as usize, (hi - b * block) as usize))); // alloc: Exec charged segs.owner()
                r = hi;
            }
        }
        let rows: usize = segs.iter().map(|&(_, lo, hi)| hi - lo).sum();
        let mut cols: Vec<ColBuffer> = pgrc_write::format::try_vec(self.natts, "parquet_cluster")?;
        let mut ext = NoExternalDetoast;
        let mut scratch: Vec<u8> = Vec::new();
        for (c, s) in schema.iter().enumerate() {
            let mut col = ColBuffer::with_owner(*s, self.shared.base.owner);
            // D4919: every segment lies inside its fragment's lanes (a block decoded whole for its slot: `TurnGuard`)
            #[cfg(debug_assertions)]
            for (j, &(frag, lo, hi)) in segs.iter().enumerate() {
                let (len, nl) = match &frag.lanes[c] {
                    Lane::Words { words, nulls } => (words.len(), nulls.len()),
                    Lane::Blobs { spans, nulls, .. } => (spans.len(), nulls.len()),
                };
                debug_assert!(lo <= hi && hi <= len && hi <= nl, "segment {j} of {} (column {c}): rows {lo}..{hi} of a fragment of {} rows, lane {len}, nulls {nl}", segs.len(), frag.rows);
            }
            let mut bytes = 0usize;
            for &(frag, lo, hi) in segs.iter() {
                if let Lane::Blobs { spans, nulls, .. } = &frag.lanes[c] {
                    for i in lo..hi {
                        if !nulls[i] {
                            bytes += spans[i].1 as usize;
                        }
                    }
                }
            }
            col.reserve_rows(rows, bytes)?;
            for &(frag, lo, hi) in segs.iter() {
                match &frag.lanes[c] {
                    Lane::Words { words, nulls } => col.append_word_lane(&words[lo..hi], &nulls[lo..hi])?,
                    Lane::Blobs { arena, spans, nulls } => {
                        col.append_image_lane(arena, &spans[lo..hi], &nulls[lo..hi], &mut ext, &mut scratch)?
                    }
                }
            }
            if self.shared.utf8[c] {
                col.set_utf8_proven();
            }
            cols.push(col);
        }
        ColChunk::new(cols)
    }
}

/// One fragment row as the capture currency (`RawDatum` borrows the lanes).
fn gather_row<'a>(frag: &'a Fragment, r: usize, natts: usize, row: &mut Vec<RawDatum<'a>>) {
    for lane in frag.lanes.iter().take(natts) {
        row.push(match lane {
            Lane::Words { words, nulls } => {
                if nulls[r] {
                    RawDatum::Null
                } else {
                    RawDatum::Word(words[r])
                }
            }
            Lane::Blobs { arena, spans, nulls } => {
                if nulls[r] {
                    RawDatum::Null
                } else {
                    let (off, len) = spans[r];
                    RawDatum::Bytes(&arena[off as usize..(off + len) as usize])
                }
            }
        });
    }
}

impl runtime::TaskSetWork for BuildPass {
    fn run_morsel(&self, _worker: usize, range: runtime::MorselRange) {
        let _acct = memledger::charge::enter_as(memledger::charge::context_of(self.shared.base.acct).0);
        for id in range {
            if self.shared.base.cancelled.load(Ordering::SeqCst) {
                continue;
            }
            let spec = lock(&self.shared.boards).specs.remove(&id);
            let out = match spec {
                None => Out::Failed("build spec missing".to_string()), // alloc: Cold
                // unwind-ok: worker-containment
                Some(spec) => match catch_unwind(AssertUnwindSafe(|| self.build(&spec))) {
                    Ok(Ok(c)) => Out::Done(c),
                    Ok(Err(e)) => Out::Failed(format!("{e}")), // alloc: Cold
                    Err(_) => Out::Failed("build worker panicked".to_string()), // alloc: Cold
                },
            };
            // `spec` (its block Arcs) dropped here — residency released.
            self.shared.post(|b| b.built.insert(id, out));
        }
    }

    fn finalize(&self) {}
}

// ---------------------------------------------------------------------------
// Task-set lifecycle
// ---------------------------------------------------------------------------

struct Pass {
    stream: Arc<runtime::StreamSource>,
    waiter: Option<runtime::CompletionWaiter>,
}

impl Pass {
    fn submit(rt: &Arc<runtime::Runtime>, work: Arc<dyn runtime::TaskSetWork>) -> Pass {
        static NONCE: AtomicU64 = AtomicU64::new(1);
        let stream = Arc::new(runtime::StreamSource::new());
        let (_rg, waiter) = rt.submit(runtime::QuerySpec {
            query_id: NONCE.fetch_add(1, Ordering::SeqCst),
            tasksets: vec![runtime::TaskSetSpec {
                source: Arc::clone(&stream) as Arc<dyn runtime::MorselSource>,
                work,
                deps: Vec::new(),
            }],
        });
        Pass {
            stream,
            waiter: Some(waiter),
        }
    }

    /// Close + wait (every exit path; idempotent).
    fn drain(&mut self, rt: &Arc<runtime::Runtime>) {
        if let Some(w) = self.waiter.take() {
            self.stream.close();
            rt.notify_source_progress();
            w.wait();
        }
    }
}

/// Every pass drained on every exit path, cancellation raised first so the
/// workers skip what they have not started.
struct Drain<'a> {
    rt: &'a Arc<runtime::Runtime>,
    shared: &'a Arc<Shared>,
    passes: Vec<&'a mut Pass>,
}

impl Drop for Drain<'_> {
    fn drop(&mut self) {
        self.shared.base.cancelled.store(true, Ordering::SeqCst);
        for p in self.passes.iter_mut() {
            p.drain(self.rt);
        }
    }
}

// ---------------------------------------------------------------------------
// The leader
// ---------------------------------------------------------------------------

/// Park until a board changes (50 ms cadence; interrupts polled).
fn park(shared: &Shared, stash: &mut Option<Box<PgError>>) -> Result<(), WriteError> {
    {
        let mut g = lock(&shared.boards);
        let h = waiter::current_handle().as_u64();
        if !g.wakers.contains(&h) {
            g.wakers.with_vec(|v| v.push(h)); // alloc: Exec charged g.wakers.owner()
        }
    }
    let _ = waiter::park_timeout(Duration::from_millis(50));
    if let Err(e) = postgres_seams::check_for_interrupts::call() {
        *stash = Some(e);
        return Err(STASHED);
    }
    Ok(())
}

/// Replay row groups `0..=f` (FILE seqs) through the serial machinery and
/// raise the first error it meets — a serial COPY's first error in input
/// order. `Ok(())` = none found (the caller raises the inconsistency).
fn replay_upto<'mcx>(
    mcx: Mcx<'mcx>,
    cstate: &mut CopyFromState<'mcx, '_>,
    rel: &Relation<'mcx>,
    base: &DecodeShared,
    f: u64,
) -> PgResult<()> {
    for s in 0..=f as usize {
        let rg_raw = base.rg_order[s];
        let rows = base.meta.row_groups[rg_raw].num_rows as u64;
        replay_rg_serial(mcx, cstate, rel, rg_raw, base.row_base[s], rows)?;
    }
    Ok(())
}

/// The cluster admission: the declared key resolved to word bindings.
fn resolve_keys(rel: &Relation<'_>, base: &DecodeShared) -> PgResult<Option<Vec<KeyCol>>> {
    let decls: Vec<ClusterKeyDecl> = pgrc_am::reloptions::cluster_key(rel);
    if decls.is_empty() {
        return Ok(None);
    }
    let schemas = pgrc_am::schema::col_schemas(rel)?;
    let mut keys = ::mcx::stdvec::try_vec(decls.len(), "parquet_cluster")?;
    for d in decls.iter() {
        let attidx = d.attno as usize - 1;
        let Some(schema) = schemas.iter().find(|s| s.attno == d.attno) else {
            ptrace("pgrc-cluster refused: key attno without a schema");
            return Ok(None);
        };
        let class = match schema.class {
            StorageClass::ByvalWord { signed: true, .. } => KeyClass::Signed,
            StorageClass::ByvalWord { signed: false, .. } | StorageClass::Bool => KeyClass::Unsigned,
            StorageClass::F32 => KeyClass::F32,
            StorageClass::F64 => KeyClass::F64,
            _ => {
                ptrace("pgrc-cluster refused: a text/fixed key column (word keys only)");
                return Ok(None);
            }
        };
        let Some(binding) = base.plan.bindings.iter().position(|b| b.attidx == attidx) else {
            ptrace("pgrc-cluster refused: key column not bound");
            return Ok(None);
        };
        keys.push(KeyCol {
            binding,
            class,
            dir: d.dir,
            nulls: d.nulls,
        });
    }
    Ok(Some(keys))
}

/// The residency budget for the merge: the driver's decode budget, or half
/// the machine's memory headroom when that is larger. The merge needs the
/// interval depth resident (D = 9: 9 groups of ~0.7 GB decoded lane-major)
/// plus the decodes in flight; a quarter of a 30 GB
/// box refused it. D3343: the headroom is held to the governed limit's room.
/// D3808: and to the decode window's share of the load's room under the
/// memory policy (`mempolicy::load::load_window`: half of B less its low
/// mark less what is resident), the rest left to the open parts:
/// with the governor off (the default) the governed room is the bare limit,
/// and the window and the open parts each sized to half of it.
fn cluster_budget(workers: usize) -> u64 {
    let base = parquet_budget_bytes_auto(workers);
    let budget = match crate::parallel::governed(crate::memheadroom::memory_headroom_bytes()) {
        Some(h) => base.max(h / 2),
        None => base,
    };
    crate::parallel::load_room_now().map_or(budget, |room| budget.min(mempolicy::load::load_window(room)))
}

/// The merge entry, called by the file-order driver after its own
/// admission. `Ok(None)` = not for this statement (no key, opted out, a
/// key shape the merge does not take, or a residency the budget does not
/// hold) — the caller proceeds in file order.
#[allow(clippy::too_many_arguments)]
pub(crate) fn copy_clustered<'mcx>(
    mcx: Mcx<'mcx>,
    cstate: &mut CopyFromState<'mcx, '_>,
    rel: &Relation<'mcx>,
    ti_options: i32,
    rt: &'static Arc<runtime::Runtime>,
    base: &Arc<DecodeShared>,
    total_rows: u64,
) -> PgResult<Option<u64>> {
    if !cluster_enabled() {
        return Ok(None);
    }
    let Some(keys) = resolve_keys(rel, base)? else {
        return Ok(None);
    };
    let n_rgs = base.rg_order.len() as u64;
    let t0 = std::time::Instant::now();
    // CONVERT c2: the per-row-group residency estimates (8 B each) on the load's owner for the merge's life
    let mut est: memledger::ChargedVec<u64> = memledger::ChargedVec::new(base.owner, memledger::Kind::Region);
    est.with_vec(|v| v.extend(base.rg_order.iter().map(|&i| est_resident(base, i, keys.len() as u64)))); // alloc: Exec charged est.owner()
    let mut utf8 = ::mcx::stdvec::try_filled(base.byval.len(), false, "parquet_cluster")?;
    for b in base.plan.bindings.iter() {
        utf8[b.attidx] = base.plan.vutf8[b.batch];
    }
    let shared = Arc::new(Shared {
        base: Arc::clone(base),
        keys,
        order: OnceLock::new(),
        est,
        block: base.grain as u32,
        schema: OnceLock::new(),
        utf8,
        boards: Mutex::new(Boards {
            bounds: BTreeMap::new(),
            served: 0,
            fails: BTreeMap::new(),
            specs: BTreeMap::new(),
            built: BTreeMap::new(),
            wakers: memledger::ChargedVec::new(base.owner, memledger::Kind::Region),
        }),
        demands: Mutex::new(memledger::ChargedVec::new(base.owner, memledger::Kind::Region)),
        live: Arc::new(AtomicU64::new(0)),
        window: AtomicU64::new(0),
    });

    // ---- 1. Key pre-pass: every group's composite bounds. ----
    let mut stash: Option<Box<PgError>> = None;
    enum KeyErr {
        Failed(u64, String),
        Interrupted,
        Oom(Box<PgError>),
    }
    let bounds: Vec<(KeyVal, KeyVal)> = {
        let mut pass = Pass::submit(
            rt,
            Arc::new(KeyPass {
                shared: Arc::clone(&shared),
            }),
        );
        pass.stream.publish(n_rgs);
        pass.stream.close();
        rt.notify_source_progress();
        let r = (|| -> Result<Vec<(KeyVal, KeyVal)>, KeyErr> {
            loop {
                {
                    let g = lock(&shared.boards);
                    if g.bounds.len() as u64 == n_rgs {
                        break;
                    }
                }
                park(&shared, &mut stash).map_err(|_| KeyErr::Interrupted)?;
            }
            let mut g = lock(&shared.boards);
            let all = core::mem::take(&mut g.bounds);
            let mut out = ::mcx::stdvec::try_vec(n_rgs as usize, "parquet_cluster").map_err(KeyErr::Oom)?;
            for (seq, o) in all {
                match o {
                    Out::Done(b) => out.push(b),
                    Out::Failed(detail) => return Err(KeyErr::Failed(seq, detail)),
                }
            }
            Ok(out)
        })();
        pass.drain(rt);
        match r {
            Ok(b) => b,
            Err(KeyErr::Failed(f, detail)) => {
                ptrace(&format!("pgrc-cluster key pass failed at rg seq {f}: serial replay 0..={f}")); // alloc: Cold
                return match replay_upto(mcx, cstate, rel, base, f) {
                    Err(e) => Err(e),
                    Ok(()) => Err(Box::new(PgError::error(format!( // alloc: Cold
                        "parquet cluster key pass failed but the serial replay found no error: {detail}"
                    )))),
                };
            }
            Err(KeyErr::Oom(e)) => return Err(e),
            Err(KeyErr::Interrupted) => {
                return Err(stash.take().unwrap_or_else(|| pgrc_am::write_error(STASHED)))
            }
        }
    };
    let t_keys = t0.elapsed();

    // ---- 2. Plan: decode order by lower bound; interval depth vs budget. ----
    let keys = &shared.keys[..];
    // CONVERT c2: the decode order (8 B per row group) on the load's owner; the plan hands it to `Shared::order`
    let mut order: memledger::ChargedVec<u64> = memledger::ChargedVec::new(base.owner, memledger::Kind::Region);
    order.with_vec(|v| {
        v.extend(0..n_rgs); // alloc: Exec charged order.owner()
        v.sort_by(|&a, &b| {
            composite_cmp(&keys, &bounds[a as usize].0, &bounds[b as usize].0).then(a.cmp(&b))
        });
    });
    // Sweep: (key, +1 activations before −1 releases at equal keys).
    let mut events: memledger::ChargedVec<(&KeyVal, i32, u64)> = memledger::ChargedVec::new(base.owner, memledger::Kind::Region);
    events.with_vec(|v| -> PgResult<()> {
        *v = ::mcx::stdvec::try_vec(2 * n_rgs as usize, "parquet_cluster")?;
        Ok(())
    })?;
    for s in 0..n_rgs {
        events.with_vec(|v| v.push((&bounds[s as usize].0, 1, s))); // alloc: Exec charged events.owner()
        events.with_vec(|v| v.push((&bounds[s as usize].1, -1, s))); // alloc: Exec charged events.owner()
    }
    events.with_vec(|v| v.sort_by(|a, b| composite_cmp(&keys, a.0, b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)))); // alloc: Exec charged events.owner()
    let (mut depth, mut max_depth, mut depth_bytes, mut max_bytes) = (0i64, 0i64, 0u64, 0u64);
    for (_, d, s) in events.iter() {
        depth += i64::from(*d);
        if *d > 0 {
            depth_bytes += shared.est[*s as usize];
        } else {
            depth_bytes -= shared.est[*s as usize];
        }
        max_depth = max_depth.max(depth);
        max_bytes = max_bytes.max(depth_bytes);
    }
    let budget = cluster_budget(rt.config().workers);
    let max_est = shared.est.iter().copied().max().unwrap_or(0);
    // Residency = the depth + the pending groups decoded ahead of the front.
    let need = max_bytes + PENDING_AHEAD * max_est;
    ptrace(&format!( // alloc: Cold
        "pgrc-cluster plan rgs={n_rgs} keys={} depth={max_depth} need_mb={} budget_mb={} keypass_ms={}",
        keys.len(),
        need >> 20,
        budget >> 20,
        t_keys.as_millis()
    ));
    if need > budget {
        ptrace("pgrc-cluster refused: interval depth exceeds the residency budget (file order)");
        return Ok(None);
    }
    let _resident = LoadCharge::new(need);
    shared.window.store(need, Ordering::Relaxed);
    let file_sorted = order.iter().enumerate().all(|(i, &s)| i as u64 == s)
        && (1..n_rgs as usize)
            .all(|s| composite_cmp(&keys, &bounds[s - 1].1, &bounds[s].0) != Ord_::Greater);
    ptrace(&format!("pgrc-cluster order file_sorted={file_sorted}")); // alloc: Cold
    let _ = shared.order.set(order);

    // ---- 3–6. Decode + merge + build + append. ----
    let natts = base.byval.len();
    let grain = base.grain as u32;
    let mut decode = Pass::submit(
        rt,
        Arc::new(DecodePass {
            shared: Arc::clone(&shared),
        }),
    );
    let mut build = Pass::submit(
        rt,
        Arc::new(BuildPass {
            shared: Arc::clone(&shared),
            natts,
            grain,
        }),
    );
    let dstream = Arc::clone(&decode.stream);
    let bstream = Arc::clone(&build.stream);
    let mut fail: Option<(u64, String)> = None;
    let mut counters = Counters::default();
    let fed: PgResult<u64> = pgrc_am::ingest::with_parallel_copy_writer(rel, |writer, providers| {
        let r = pgrc_ingest_par::parallel_ingest(writer, rt, providers, ParIngestOpts::auto(), |sess| {
            let _guard = Drain {
                rt,
                shared: &shared,
                passes: vec![&mut decode, &mut build],
            };
            let fed = merge_feed(
                cstate, &shared, &dstream, &bstream, rt, sess, &bounds, &mut stash, &mut fail, &mut counters,
            );
            let (peak, waits, ms) = sess.memory_report();
            ptrace(&format!( // alloc: Cold
                "pgrc-cluster memory peak_held={peak} gate_waits={waits} gate_ms={ms} load_peak={}",
                memledger::LOAD_IN_FLIGHT.peak()
            ));
            fed
        });
        r.map_err(|we| stash.take().unwrap_or_else(|| pgrc_am::write_error(we)))
    });
    late_fail(&shared.boards, &mut fail);
    // A decode failure: byte-exact error parity through the serial replay.
    if let Some((f, detail)) = fail {
        ptrace(&format!("pgrc-cluster decode failed at rg seq {f}: serial replay 0..={f}")); // alloc: Cold
        return match replay_upto(mcx, cstate, rel, base, f) {
            Err(e) => Err(e),
            Ok(()) => Err(Box::new(PgError::error(format!( // alloc: Cold
                "parquet cluster decode failed but the serial replay found no error: {detail}"
            )))),
        };
    }
    let processed = fed?;
    debug_assert_eq!(processed, total_rows);
    tableam::table_finish_bulk_insert(rel, ti_options)?;
    pgstat_progress_update_param(PROGRESS_COPY_TUPLES_PROCESSED, processed as i64);
    ptrace(&format!( // alloc: Cold
        "pgrc-cluster done rows={processed} chunks={} runs={} resorted_rgs={} peak_live_mb={} wall_ms={}",
        counters.chunks,
        counters.runs,
        counters.resorted,
        counters.peak_live >> 20,
        t0.elapsed().as_millis()
    ));
    Ok(Some(processed))
}
/// D4919: a decode failure the leader's last look at the boards missed fails the load as one it saw does (`fail`, unless
/// it holds one): a worker posts its failure after its pass returned, so one posted after the merge appended the last
/// chunk is on the boards only once the passes drained. No COPY commits past a failed pass.
fn late_fail(boards: &Mutex<Boards>, fail: &mut Option<(u64, String)>) {
    if fail.is_none() {
        *fail = lock(boards).fails.pop_first();
    }
}

/// D3808: how many chunks the merge cuts between two [`mem_trace`] lines
/// (a 100M-row file: about 12,200 chunks, so about 95 lines).
const MEM_TRACE_CHUNKS: u64 = 128;

/// D3808: the load's memory, every [`MEM_TRACE_CHUNKS`] chunks under the trace: the resident
/// bytes the governor's probe reads, the process footprint, the statement's
/// ledger, the load account, the open parts' held bytes and budget, the
/// decode window's live blocks and the load's room.
fn mem_trace(sess: &ParSession<'_>, shared: &Shared) {
    let mb = |b: u64| b >> 20;
    let (held, budget) = sess.memory_now();
    ptrace(&format!( // alloc: Cold
        "pgrc-cluster mem resident_mb={} footprint_mb={} stmt_mb={} load_mb={} held_mb={} budget_mb={} live_mb={} ahead_mb={} dirty_mb={} room_mb={}",
        mcx::governor::probe_resident().map_or(0, |r| mb(r as u64)),
        mb(mcx::global_footprint::bytes() as u64),
        mcx::governor::current().map_or(0, |l| mb(l.bytes().max(0) as u64)),
        mb(memledger::LOAD_IN_FLIGHT.bytes()),
        mb(held),
        mb(budget),
        mb(shared.live.load(Ordering::SeqCst)),
        mb(shared.window.load(Ordering::Relaxed).saturating_sub(shared.live.load(Ordering::Relaxed))),
        mcx::governor::probe_dirty().map_or(0, |d| mb(d as u64)),
        crate::parallel::load_room_now().map_or(-1, |r| mb(r) as i64),
    ));
}

#[derive(Default)]
struct Counters {
    chunks: u64,
    runs: u64,
    resorted: u64,
    peak_live: u64,
}

/// One active group in the merge and its cursor over its key-ordered rows.
struct Active {
    g: Arc<Group>,
    pos: u32,
}

/// Key demands run this many groups ahead of the activation front (decode
/// order); the next `PENDING_AHEAD` pending groups' cursors run beside the
/// active ones (decode parallelism across groups: a cursor is one worker);
/// every pumped group is decoded `BLOCK_AHEAD` blocks past the merge, at
/// most `INFLIGHT` demands outstanding per group.
const KEY_AHEAD: u64 = 4;
const PENDING_AHEAD: u64 = 3;
const BLOCK_AHEAD: u32 = 12;
const INFLIGHT: u32 = 2;

fn demand(shared: &Shared, stream: &runtime::StreamSource, rt: &runtime::Runtime, d: Demand) {
    let n = {
        let mut q = lock(&shared.demands);
        q.with_vec(|v| v.push(Some(d))); // alloc: Exec charged q.owner()
        q.len() as u64
    };
    stream.publish(n);
    rt.notify_source_progress();
}

/// Publish the group's next block demands toward its target (idempotent;
/// called on every leader turn).
fn pump(shared: &Shared, stream: &runtime::StreamSource, rt: &runtime::Runtime, g: &Arc<Group>) {
    let nb = g.nblocks(shared.block);
    let target = g.target.load(Ordering::Relaxed).min(nb.saturating_sub(1));
    let dec = g.decoded.load(Ordering::SeqCst);
    let mut d = g.demanded.load(Ordering::SeqCst);
    while d <= target && d < nb && d < dec + INFLIGHT {
        // The frontier moves before the publish: the cursor holder reads it.
        g.demanded.store(d + 1, Ordering::SeqCst);
        demand(shared, stream, rt, Demand::Block(Arc::clone(g), d));
        d += 1;
    }
}

fn aim(g: &Group, target: u32) {
    if g.target.load(Ordering::Relaxed) < target {
        g.target.store(target, Ordering::Relaxed);
    }
}

/// Pin a run's blocks (all present, by the readiness check) and evict the
/// group's blocks before it: runs of one group arrive in row order, so an
/// earlier block has no later reference.
fn resolve_run(shared: &Shared, run: &Run) -> ResolvedRun {
    let block = shared.block;
    let b0 = run.start / block;
    let b1 = (run.start + run.n - 1) / block;
    let mut t = lock(&run.g.blocks);
    let blocks: Vec<Arc<Block>> = (b0..=b1)
        .map(|b| Arc::clone(t[b as usize].as_ref().expect("resolved block present")))
        .collect();
    let mut e = run.g.evicted.load(Ordering::Relaxed);
    while e < b0 {
        t[e as usize] = None;
        e += 1;
    }
    run.g.evicted.store(e, Ordering::Relaxed);
    ResolvedRun {
        blocks,
        b0,
        start: run.start,
        n: run.n,
    }
}

fn spec_ready(shared: &Shared, spec: &ChunkSpec) -> bool {
    let block = shared.block;
    spec.runs
        .iter()
        .all(|r| r.g.has_blocks(r.start / block, (r.start + r.n - 1) / block))
}

/// The leader: activation by lower bound, run emission bounded by the
/// runner-up, specs resolved as their blocks land, built chunks appended in
/// order. `Err(STASHED)` = a stashed PgError or a decode failure in `fail`.
#[allow(clippy::too_many_arguments)]
fn merge_feed<'mcx>(
    cstate: &mut CopyFromState<'mcx, '_>,
    shared: &Arc<Shared>,
    decode_stream: &Arc<runtime::StreamSource>,
    build_stream: &Arc<runtime::StreamSource>,
    rt: &Arc<runtime::Runtime>,
    sess: &mut ParSession<'_>,
    bounds: &[(KeyVal, KeyVal)],
    stash: &mut Option<Box<PgError>>,
    fail: &mut Option<(u64, String)>,
    counters: &mut Counters,
) -> Result<u64, WriteError> {
    let keys = &shared.keys;
    let order = shared.order();
    let n_rgs = order.len() as u64;
    let grain = shared.base.grain as u32;
    let natts = shared.base.byval.len();
    let build_ahead = (2 * rt.config().workers as u64 + 2).max(4);
    let mut schema = pgrc_write::format::try_vec(sess.schema().len(), "parquet_cluster")?;
    schema.extend_from_slice(sess.schema());
    let _ = shared.schema.set(schema);
    ptrace(&format!( // alloc: Cold
        "pgrc-cluster feed parts_in_flight={} chunks_in_flight={} build_ahead={build_ahead}",
        sess.opts().max_parts_in_flight,
        sess.opts().max_chunks_in_flight
    ));

    // Groups by decode seq; taken at activation (the specs keep what they need).
    let mut groups: memledger::ChargedVec<Option<Arc<Group>>> =
        memledger::ChargedVec::new(shared.base.owner, memledger::Kind::Region);
    groups.with_vec(|v| v.extend(order // alloc: Exec charged groups.owner()
        .iter()
        .map(|&fseq| {
            let file_idx = shared.rg_file_idx(fseq);
            Some(Arc::new(Group {
                fseq,
                file_idx,
                rows: shared.base.meta.row_groups[file_idx].num_rows.max(0) as u32,
                keys_charge: OnceLock::new(),
                keys: OnceLock::new(),
                resorted: AtomicBool::new(false),
                blocks: Mutex::new(Vec::new()),
                cursor: Mutex::new(None),
                turn: DecodeTurn::new(),
                decoded: AtomicU32::new(0),
                demanded: AtomicU32::new(0),
                target: AtomicU32::new(0),
                evicted: AtomicU32::new(0),
            }))
        })));
    let mut keys_demanded: u64 = 0;
    let mut next_pending: u64 = 0;
    let mut active: memledger::ChargedVec<Active> = memledger::ChargedVec::new(shared.base.owner, memledger::Kind::Region);
    // Exhausted groups whose last blocks are still to be demanded: the
    // cursor's in-flight cap means the merge can outrun the demands, and a
    // spec waiting on an undemanded block would wait forever.
    let mut draining: memledger::ChargedVec<Arc<Group>> = memledger::ChargedVec::new(shared.base.owner, memledger::Kind::Region);
    let mut cur = ChunkSpec::new(shared.base.owner);
    let mut waiting: VecDeque<(u64, ChunkSpec)> = VecDeque::new();
    let mut emitted_chunks: u64 = 0;
    let mut appended: u64 = 0;
    let mut processed: u64 = 0;
    let mut merge_done = false;
    let mut tail: Option<ChunkSpec> = None;
    let mut traced_at: u64 = 0;

    loop {
        // D3808: the window's blocks still to come count as resident at the
        // engine's memory gate: they arrive whatever the gate decides.
        sess.set_ahead(shared.window.load(Ordering::Relaxed).saturating_sub(shared.live.load(Ordering::Relaxed)));
        if ptrace_enabled() && counters.chunks >= traced_at + MEM_TRACE_CHUNKS {
            traced_at = counters.chunks;
            mem_trace(sess, shared);
        }
        let mut progress = false;
        // A. Drain the boards.
        {
            let mut g = lock(&shared.boards);
            if g.served > 0 {
                g.served = 0;
                progress = true;
            }
            if let Some((&f, d)) = g.fails.iter().next() {
                *fail = Some((f, d.clone()));
                return Err(STASHED);
            }
        }
        // A2. Keys ahead of the activation front.
        while keys_demanded < n_rgs && keys_demanded < next_pending + KEY_AHEAD {
            if let Some(g) = groups[keys_demanded as usize].as_ref() {
                demand(shared, decode_stream, rt, Demand::Keys(Arc::clone(g)));
            }
            keys_demanded += 1;
        }
        // A3. Decode-ahead: the active groups' cursors and the next pending
        // ones' (their keys ready) run in parallel.
        for a in active.iter() {
            pump(shared, decode_stream, rt, &a.g);
        }
        draining.with_vec(|v| v.retain(|g| {
            pump(shared, decode_stream, rt, g);
            g.demanded.load(Ordering::SeqCst) < g.nblocks(shared.block)
        }));
        for i in next_pending..(next_pending + PENDING_AHEAD).min(n_rgs) {
            if let Some(g) = groups[i as usize].as_ref() {
                if g.keys.get().is_some() {
                    aim(g, BLOCK_AHEAD - 1);
                    pump(shared, decode_stream, rt, g);
                }
            }
        }
        // B. Merge: emit runs while the backlog allows.
        while !merge_done && emitted_chunks - appended < build_ahead {
            let mut stalled = false;
            while next_pending < n_rgs {
                let fseq = order[next_pending as usize];
                let due = match min_active(keys, &active) {
                    None => true,
                    Some(i) => {
                        let a = &active[i];
                        // lower(pending) <= head(a) under (key, fseq)
                        a.g.cmp_val(keys, a.pos as usize, &bounds[fseq as usize].0)
                            .reverse()
                            .then(fseq.cmp(&a.g.fseq))
                            != Ord_::Greater
                    }
                };
                if !due {
                    break;
                }
                let ready = groups[next_pending as usize].as_ref().is_some_and(|g| g.keys.get().is_some());
                if !ready {
                    stalled = true;
                    break;
                }
                let g = groups[next_pending as usize].take().expect("pending group");
                aim(&g, BLOCK_AHEAD);
                pump(shared, decode_stream, rt, &g);
                if g.rows > 0 {
                    active.with_vec(|v| v.push(Active { g, pos: 0 })); // alloc: Exec charged active.owner()
                }
                next_pending += 1;
                progress = true;
            }
            if stalled {
                break;
            }
            if active.is_empty() {
                if next_pending >= n_rgs {
                    merge_done = true;
                    if cur.rows > 0 {
                        tail = Some(core::mem::replace(&mut cur, ChunkSpec::new(shared.base.owner)));
                    }
                }
                break;
            }
            let mi = min_active(keys, &active).expect("non-empty");
            // The runner-up bound: the smallest other active head, or the
            // pending front's lower bound, as (key, fseq).
            let mut ru: Option<usize> = None;
            for (i, a) in active.iter().enumerate() {
                if i != mi && ru.map_or(true, |r| cmp_heads(keys, a, &active[r]) == Ord_::Less) {
                    ru = Some(i);
                }
            }
            let mut bound: Option<(KeyVal, u64)> =
                ru.map(|r| (active[r].g.val(keys, active[r].pos as usize), active[r].g.fseq));
            if next_pending < n_rgs {
                let fseq = order[next_pending as usize];
                let v = &bounds[fseq as usize].0;
                let better = match &bound {
                    None => true,
                    Some((bv, bf)) => composite_cmp(keys, v, bv).then(fseq.cmp(bf)) == Ord_::Less,
                };
                if better {
                    bound = Some((v.clone(), fseq));
                }
            }
            let a = &mut active[mi];
            let n_rows = a.g.rows;
            // First key-ordered row at or after pos that must NOT precede
            // the bound: rows r with (key(r), fseq) < bound are ours.
            let end = match &bound {
                None => n_rows,
                Some((bv, bf)) => {
                    let (mut lo, mut hi) = (a.pos, n_rows);
                    while lo < hi {
                        let mid = lo + (hi - lo) / 2;
                        let o = a.g.cmp_val(keys, mid as usize, bv).then(a.g.fseq.cmp(bf));
                        if o == Ord_::Less {
                            lo = mid + 1;
                        } else {
                            hi = mid;
                        }
                    }
                    lo
                }
            };
            debug_assert!(end > a.pos, "merge run must make progress");
            let mut start = a.pos;
            let mut left = end.max(a.pos + 1) - a.pos;
            while left > 0 {
                let room = grain - cur.rows;
                let n = left.min(room);
                cur.push_run(&a.g, start, n);
                counters.runs += 1;
                start += n;
                left -= n;
                if cur.rows == grain {
                    let spec = core::mem::replace(&mut cur, ChunkSpec::new(shared.base.owner));
                    waiting.push_back((emitted_chunks, spec));
                    emitted_chunks += 1;
                    counters.chunks += 1;
                }
            }
            aim(&a.g, (start.max(1) - 1) / shared.block + BLOCK_AHEAD);
            pump(shared, decode_stream, rt, &a.g);
            a.pos = start;
            progress = true;
            if start >= n_rows {
                if a.g.resorted.load(Ordering::SeqCst) {
                    counters.resorted += 1;
                }
                let done = active.with_vec(|v| v.swap_remove(mi));
                if done.g.demanded.load(Ordering::SeqCst) < done.g.nblocks(shared.block) {
                    draining.with_vec(|v| v.push(done.g)); // alloc: Exec charged draining.owner()
                }
            }
        }
        // B2. Resolve specs whose blocks have landed, in order.
        while let Some((id, spec)) = waiting.front() {
            if !spec_ready(shared, spec) {
                break;
            }
            let id = *id;
            let (_, spec) = waiting.pop_front().expect("front");
            let resolved = BuildSpec {
                runs: spec.runs.iter().map(|r| resolve_run(shared, r)).collect(),
            };
            shared.post(|b| b.specs.insert(id, resolved));
            build_stream.publish(id + 1);
            rt.notify_source_progress();
            progress = true;
        }
        if merge_done && waiting.is_empty() && !build_stream.is_closed() {
            build_stream.close();
            rt.notify_source_progress();
        }
        // C. Append built chunks in order.
        loop {
            let got = {
                let mut g = lock(&shared.boards);
                g.built.remove(&appended)
            };
            match got {
                None => break,
                Some(Out::Failed(detail)) => {
                    *stash = Some(Box::new(PgError::error(format!( // alloc: Cold
                        "parquet cluster chunk build failed: {detail}"
                    ))));
                    return Err(STASHED);
                }
                Some(Out::Done(chunk)) => {
                    let rows = u64::from(chunk.rows());
                    sess.append_col_chunk(chunk)?;
                    appended += 1;
                    processed += rows;
                    progress = true;
                    if let Err(e) = postgres_seams::check_for_interrupts::call() {
                        *stash = Some(e);
                        return Err(STASHED);
                    }
                    pgstat_progress_update_param(PROGRESS_COPY_TUPLES_PROCESSED, processed as i64);
                    let bytes = shared.base.bytes_read.load(Ordering::Relaxed);
                    if bytes != cstate.bytes_processed {
                        cstate.bytes_processed = bytes;
                        pgstat_progress_update_param(PROGRESS_COPY_BYTES_PROCESSED, bytes as i64);
                    }
                }
            }
        }
        counters.peak_live = counters.peak_live.max(shared.live.load(Ordering::SeqCst));
        // D. Finished?
        if merge_done && waiting.is_empty() && appended == emitted_chunks {
            if let Some(t) = tail.take() {
                // The sub-grain tail: its blocks may still be landing.
                if !spec_ready(shared, &t) {
                    tail = Some(t);
                    park(shared, stash)?;
                    continue;
                }
                let mut runs: memledger::ChargedVec<ResolvedRun> = memledger::ChargedVec::new(shared.base.owner, memledger::Kind::Region);
                runs.with_vec(|v| v.extend(t.runs.iter().map(|r| resolve_run(shared, r)))); // alloc: Exec charged runs.owner()
                let mut row: Vec<RawDatum<'_>> = pgrc_write::format::try_vec(natts, "parquet_cluster")?;
                for run in runs.iter() {
                    for r in run.start..run.start + run.n {
                        let blk = &run.blocks[(r / shared.block - run.b0) as usize];
                        row.clear();
                        gather_row(&blk.frag, (r % shared.block) as usize, natts, &mut row);
                        sess.append_row(&row)?;
                        processed += 1;
                    }
                }
            }
            break;
        }
        if !progress {
            park(shared, stash)?;
        }
    }
    Ok(processed)
}

/// The active group whose head is smallest under (key, file seq).
fn min_active(keys: &[KeyCol], active: &[Active]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, a) in active.iter().enumerate() {
        if best.map_or(true, |b| cmp_heads(keys, a, &active[b]) == Ord_::Less) {
            best = Some(i);
        }
    }
    best
}


#[cfg(test)]
mod late_fail_tests {
    //! D4919: [`late_fail`] over the leader's boards after the passes drained.
    use super::{late_fail, lock, Boards, Mutex};
    use std::collections::BTreeMap;

    fn boards() -> Mutex<Boards> {
        Mutex::new(Boards {
            wakers: memledger::ChargedVec::new(None, memledger::Kind::Region),
            bounds: BTreeMap::new(),
            served: 0,
            fails: BTreeMap::new(),
            specs: BTreeMap::new(),
            built: BTreeMap::new(),
        })
    }

    #[test]
    fn a_failure_posted_after_the_merge_fails_the_load() {
        let b = boards();
        let mut fail = None;
        late_fail(&b, &mut fail);
        assert_eq!(fail, None, "no failure on the boards: the load stands");
        lock(&b).fails.insert(7, "decode failed".to_string());
        late_fail(&b, &mut fail);
        assert_eq!(fail, Some((7, "decode failed".to_string())), "a failure posted after the leader's last look fails the load");
    }

    #[test]
    fn the_failure_the_leader_saw_stands() {
        let b = boards();
        lock(&b).fails.insert(9, "later".to_string());
        let mut fail = Some((2, "first".to_string()));
        late_fail(&b, &mut fail);
        assert_eq!(fail, Some((2, "first".to_string())));
    }
}

#[cfg(test)]
mod decode_turn_tests {
    //! colload (n): "a demand is never lost" for [`DecodeTurn`], with real threads. The group's cursor is a counter
    //! (`decoded`) the owner advances through `demanded`; a leader publishes demands with at most two in flight (the
    //! pump's INFLIGHT) to workers that each run one demand the way `decode_block` does. Every round must end with
    //! every demanded block decoded and every worker returned (a lost demand leaves `decoded` short).
    //! D4457: no wait here is bounded by a count of yields. Under load (box 45, load1 33 on 16 cpus) the leader's
    //! 1,000,000-yield wait for a pair's first demand ran out before that thread was scheduled, and the round failed as
    //! "a demand was lost". Every wait is now on an event the harness itself guarantees, and the one case that used to
    //! need the bound (a first demand that finds the turn not idle at a pair's start, so nobody decodes) is checked
    //! directly: the round fails instead of hanging.
    use super::{DecodeTurn, TURN_IDLE};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    /// The two hand-over protocols under test: [`DecodeTurn`], and the naive try_lock (a demand that finds the cursor
    /// owned returns, and nothing tells the owner).
    trait Turn: Sync {
        fn enter(&self) -> bool;
        fn leave(&self) -> bool;
    }

    impl Turn for DecodeTurn {
        fn enter(&self) -> bool {
            DecodeTurn::enter(self)
        }
        fn leave(&self) -> bool {
            DecodeTurn::leave(self)
        }
    }

    struct TryLockDrop(AtomicU32);

    impl Turn for TryLockDrop {
        fn enter(&self) -> bool {
            self.0.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst).is_ok()
        }
        fn leave(&self) -> bool {
            self.0.store(0, Ordering::SeqCst);
            false
        }
    }

    /// One demand for block `b`, as `decode_block` runs it: decode through `demanded` while owning the turn. The
    /// pair's FIRST demand records whether it took the turn (`first_owns`, before it counts itself in `arrived`) and,
    /// its pass done, holds the cursor until the second demand has arrived (`arrived` counts demands past `enter`), so
    /// every pair puts the second demand in the hand-over window a lossy protocol drops. That wait always ends: the
    /// leader starts the second demand once block `b` is decoded, which this pass did, and `enter` returns either way.
    fn demand(
        turn: &dyn Turn,
        decoded: &AtomicU32,
        demanded: &AtomicU32,
        arrived: &AtomicU32,
        first_owns: &AtomicBool,
        b: u32,
        first: bool,
    ) {
        let owns = decoded.load(Ordering::SeqCst) <= b && turn.enter();
        if first {
            first_owns.store(owns, Ordering::SeqCst);
        }
        arrived.fetch_add(1, Ordering::SeqCst);
        if !owns {
            return;
        }
        loop {
            while decoded.load(Ordering::SeqCst) < demanded.load(Ordering::SeqCst).max(b + 1) {
                decoded.fetch_add(1, Ordering::SeqCst);
            }
            if first {
                while arrived.load(Ordering::SeqCst) < 2 {
                    pgsync::thread::yield_now();
                }
            }
            if !turn.leave() {
                return;
            }
        }
    }

    /// One group of `blocks` blocks, demanded in pairs (the pump's INFLIGHT = 2): block d, then block d + 1 once the
    /// first demand's pass has decoded block d (it still owns the cursor, waiting for the second to arrive); the leader
    /// waits for both before the next pair, so a lost demand is never picked up by a later one. Returns the blocks
    /// decoded when the leader stopped (== blocks unless a demand was lost, or a pair's first demand found the turn
    /// taken: nobody decodes that pair, and the round stops there rather than wait for it).
    fn round(turn: &dyn Turn, blocks: u32) -> u32 {
        let (decoded, demanded) = (AtomicU32::new(0), AtomicU32::new(0));
        pgsync::thread::scope(|s| {
            let mut d = 0u32;
            'pairs: while d + 1 < blocks {
                let (arrived, first_owns) = (AtomicU32::new(0), AtomicBool::new(false));
                let ok = pgsync::thread::scope(|p| {
                    demanded.store(d + 1, Ordering::SeqCst);
                    let (dec, dem, arr, fo) = (&decoded, &demanded, &arrived, &first_owns);
                    p.spawn(move || demand(turn, dec, dem, arr, fo, d, true));
                    // the first demand passes `enter` (it returns either way) however late its thread runs
                    while arrived.load(Ordering::SeqCst) < 1 {
                        pgsync::thread::yield_now();
                    }
                    if !first_owns.load(Ordering::SeqCst) {
                        return false;
                    }
                    // it owns the turn, so its pass decodes block d
                    while decoded.load(Ordering::SeqCst) < d + 1 {
                        pgsync::thread::yield_now();
                    }
                    demanded.store(d + 2, Ordering::SeqCst);
                    p.spawn(move || demand(turn, dec, dem, arr, fo, d + 1, false));
                    true
                });
                // both demands have returned (scope join): a lost one leaves the pair short
                if !ok || decoded.load(Ordering::SeqCst) < d + 2 {
                    break 'pairs;
                }
                d += 2;
            }
            let _ = s;
        });
        decoded.load(Ordering::SeqCst)
    }

    #[test]
    fn decode_turn_never_loses_a_demand() {
        for r in 0..300 {
            let turn = DecodeTurn::new();
            let got = round(&turn, 64);
            assert_eq!(got, 64, "round {r}: a demand was lost ({got} of 64 blocks decoded)");
            assert_eq!(turn.0.load(Ordering::SeqCst), TURN_IDLE, "round {r}: the turn is idle at the end");
        }
    }

    /// D4457: a pair's first demand whose thread runs late (its `enter` starts 2 s late, once) is not a lost demand.
    /// Born red on the harness before D4457, which gave up on that demand after 1,000,000 yields (0.3-1 s on an idle
    /// box 40) and failed the round as "a demand was lost (1 of 4 blocks decoded)".
    struct LateEnter(DecodeTurn, AtomicBool);

    impl Turn for LateEnter {
        fn enter(&self) -> bool {
            if !self.1.swap(true, Ordering::SeqCst) {
                pgsync::thread::sleep(std::time::Duration::from_secs(2));
            }
            DecodeTurn::enter(&self.0)
        }
        fn leave(&self) -> bool {
            DecodeTurn::leave(&self.0)
        }
    }

    /// D4919: a pass that fails (its error or panic drops the guard) closes the turn: a later demand never takes it, so
    /// nobody decodes from a cursor the failed pass may have left mid-block (the guard handed the turn back IDLE before:
    /// the next demand decoded the rest of the group into shifted blocks, the last one short of its runs' slices).
    #[test]
    fn a_failed_pass_closes_the_turn() {
        let t = DecodeTurn::new();
        assert!(t.enter(), "the first demand takes the turn");
        assert!(!t.enter(), "a demand during the pass leaves it to the owner");
        drop(super::TurnGuard(&t));
        assert!(!t.enter(), "a demand after a failed pass decodes nothing");
        assert!(!t.enter(), "nor does the next one");
    }

    #[test]
    fn a_late_first_demand_is_not_a_lost_demand() {
        let turn = LateEnter(DecodeTurn::new(), AtomicBool::new(false));
        let got = round(&turn, 4);
        assert_eq!(got, 4, "a late first demand was taken for a lost one ({got} of 4 blocks decoded)");
        assert_eq!(turn.0 .0.load(Ordering::SeqCst), TURN_IDLE, "the turn is idle at the end");
    }

    /// Born-red witness for the test above: the naive try_lock protocol does lose demands under the same rounds
    /// (so the test can see the race it guards).
    #[test]
    fn naive_try_lock_loses_a_demand_under_the_same_rounds() {
        let lost = (0..300).filter(|_| round(&TryLockDrop(AtomicU32::new(0)), 64) < 64).count();
        eprintln!("NTURN naive try_lock lost a demand in {lost} of 300 rounds");
        assert!(lost > 0, "the harness never hit the hand-over window: it cannot tell a lossy protocol from a sound one");
    }
}
