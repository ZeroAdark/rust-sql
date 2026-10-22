//! Order-preserving parallel COPY FROM text / CSV into pgrcolumnar
//! (reports/parallel-copy-pgrc-20261008.md section 4).
//!
//! The leader is the only reader of the client stream or file and never
//! parses a field: a [`Segmentator`] cuts the input into chunks of whole
//! lines (whole engine grains each, but the last) with the `cur_lineno` the
//! serial reader holds at each chunk's start. Full-identity helpers (the
//! vacuum-morsels ceremony) parse chunks with the UNCHANGED serial reader
//! over `CopySrc::Chunk`, run the real input functions and NOT NULL checks,
//! and build one `RowChunk` per grain with `raw_row`. The leader hands the
//! grains to the pgrc parallel engine in input order (`append_chunk`) and
//! the final chunk's rows through `append_row`, so the part partition and
//! bytes equal a serial COPY's (the #560 law the parquet driver relies on).
//!
//! Errors: each helper error carries its reader's serial-exact context; the
//! leader raises the lowest erroring chunk's error once every chunk below it
//! finished. A source read error rides the final chunk's cursor. Seal-time
//! errors carry no line context (environment class).
//!
//! Admission is fail-closed (every refusal is the serial body), and
//! engagement is lazy: below PGRUST_PARALLEL_TEXT_COPY_MIN_ROWS rows and
//! PGRUST_PARALLEL_TEXT_COPY_MIN_BYTES bytes the serial body runs over the
//! buffered prefix (the original source parked for EndCopyFrom).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use pgsync::{lock, Mutex, OnceLock};

use mcx::{vec_from_elem_in, Mcx, MemoryContext, PgVec};
use types_core::Oid;
use types_error::{PgError, PgResult, ERRCODE_INTERNAL_ERROR, ERROR, FATAL};
use types_fmgr::FmgrInfo;
use types_rel::Relation;

use backend_progress::pgstat_progress_update_param;
use backend_progress::progress::{PROGRESS_COPY_BYTES_PROCESSED, PROGRESS_COPY_TUPLES_PROCESSED};

use pgrc_ingest_par::ParSession;
use pgrc_write::held::HeldCharge;
use pgrc_write::ingest::RawDatum;
use pgrc_write::par::{AheadGate, AheadStep, ParIngestOpts, RowChunk, DEFAULT_CHUNK_ROWS};
use pgrc_write::WriteError;

use crate::from::{copy_from_error_context, CopyFromState, CopySrc};
use crate::fromparse::{EolType, RAW_BUF_SIZE};
use crate::parallel::{ptrace, LoadCharge};
use crate::textseg::{ChunkCursor, ChunkDesc, SegBlock, SegOpts, Segmentator};
use crate::{CopyFormatOptions, CopyHeaderChoice, CopyOnErrorChoice};

macro_rules! refuse {
    ($why:expr) => {{
        ptrace(&format!("pgrc-text refused: {}", $why)); // alloc: Cold
        return Ok(None);
    }};
}

/// The feed-side tunnel error: the PgError is stashed on the leader; the
/// engine sees a contract error (cancels, unlinks temps) and the driver
/// re-raises the stashed error.
const STASHED: WriteError = WriteError::Contract {
    detail: "parallel text COPY feed error (see raised error)",
};

const READ_BLOCK: usize = 4 << 20;

/// A chunk may end early (on a grain multiple) once it holds this many bytes.
const CHUNK_TARGET_BYTES: u64 = 32 << 20;

/// Env knobs (new GUCs are barred by pg_settings byte-identity), read once.
struct Knobs {
    on: bool,
    grains: u64,
    min_rows: u64,
    min_bytes: u64,
    min_file_bytes: u64,
    max_line_bytes: u64,
    helpers: Option<u64>,
}

fn knobs() -> &'static Knobs {
    static K: OnceLock<Knobs> = OnceLock::new();
    K.get_or_init(|| {
        let var = |n: &str| std::env::var(n).ok().map(|v| v.trim().to_string());
        let num = |n: &str| var(n).and_then(|v| v.parse::<u64>().ok());
        let grains = num("PGRUST_PARALLEL_TEXT_COPY_GRAINS").unwrap_or(8).max(1);
        Knobs {
            on: var("PGRUST_PARALLEL_TEXT_COPY").as_deref() != Some("0"),
            grains,
            min_rows: num("PGRUST_PARALLEL_TEXT_COPY_MIN_ROWS")
                .unwrap_or(2 * grains * DEFAULT_CHUNK_ROWS as u64),
            min_bytes: num("PGRUST_PARALLEL_TEXT_COPY_MIN_BYTES").unwrap_or(64 << 20),
            min_file_bytes: num("PGRUST_PARALLEL_TEXT_COPY_MIN_FILE_BYTES").unwrap_or(1 << 20),
            max_line_bytes: num("PGRUST_PARALLEL_COPY_MAX_CHUNK_BYTES").unwrap_or(1024).max(1) << 20,
            helpers: num("PGRUST_PARALLEL_TEXT_COPY_HELPERS"),
        }
    })
}

/// Default a third of the pool (measured lineitem split: ~1.6 us parse +
/// input per row vs ~3.6 us ingest + seal), at least 2.
fn helpers_for(req: Option<u64>, workers: usize) -> u64 {
    req.unwrap_or((workers as u64).div_ceil(3).max(2)).clamp(1, runtime::MAX_EXTERNAL_LANES as u64)
}

/// Chunks published but not committed, at most (bounds read-ahead memory).
fn window(k: i32) -> u64 {
    2 * k.max(1) as u64 + 4
}

/// What a helper needs to rebuild the serial reader (owned, Send + Sync).
struct ParsePlan {
    relid: Oid,
    relname: String,
    csv: bool,
    delim: u8,
    quote: u8,
    escape: u8,
    null_print: String,
    header: CopyHeaderChoice,
    opt_file_encoding: i32,
    file_encoding: i32,
    freeze: bool,
    attnumlist: Vec<i16>,
    force_notnull_flags: Vec<bool>,
    force_null_flags: Vec<bool>,
    natts: usize,
    grain: u32,
}

/// The serial reader over a chunk source plus its slot and constraint
/// caches (a helper's, or the leader's when no helper runs).
struct ParseCx<'a, 'mcx> {
    mcx: Mcx<'mcx>,
    rel: &'a Relation<'mcx>,
    st: CopyFromState<'mcx, 'a>,
    slot: types_slot::SlotData<'mcx>,
    check_exprs: Option<PgVec<'mcx, nodemodifytable::CheckExpr<'mcx>>>,
    virtual_nn: Option<PgVec<'mcx, nodemodifytable::VirtualNnExpr<'mcx>>>,
    inserted_cols: types_nodes::Bitmapset<'mcx>,
    row_cx: MemoryContext,
}

impl<'a, 'mcx> ParseCx<'a, 'mcx> {
    /// BeginCopyFrom's state for the plan, input functions resolved in THIS
    /// backend.
    fn build(mcx: Mcx<'mcx>, rel: &'a Relation<'mcx>, plan: &'a ParsePlan) -> PgResult<ParseCx<'a, 'mcx>> {
        let tup_desc = &rel.rd_att;
        let num_phys_attrs = tup_desc.natts as usize;
        let mut attnumlist: PgVec<'mcx, i16> = PgVec::new_in(mcx);
        let mut in_functions: PgVec<'mcx, FmgrInfo> = PgVec::new_in(mcx);
        let mut typioparams: PgVec<'mcx, Oid> = PgVec::new_in(mcx);
        let mut atttypmods: PgVec<'mcx, i32> = PgVec::new_in(mcx);
        for &attnum in &plan.attnumlist {
            attnumlist.push(attnum);
            let att = tup_desc.attr(attnum as usize - 1);
            let (func_oid, typioparam) = lsyscache::typ::getTypeInputInfo(att.atttypid)?;
            in_functions.push(fmgr_core::fmgr_info(func_oid)?);
            typioparams.push(typioparam);
            atttypmods.push(att.atttypmod);
        }
        let mut attnames: PgVec<'mcx, types_tuple::NameData> = PgVec::new_in(mcx);
        let mut defexprs: PgVec<'mcx, Option<mcx::PgBox<'mcx, execexpr::ExprState<'mcx>>>> =
            PgVec::new_in(mcx);
        let mut force_notnull_flags: PgVec<'mcx, bool> = PgVec::new_in(mcx);
        let mut force_null_flags: PgVec<'mcx, bool> = PgVec::new_in(mcx);
        for i in 0..num_phys_attrs {
            attnames.push(tup_desc.attr(i).attname);
            defexprs.push(None);
            force_notnull_flags.push(plan.force_notnull_flags.get(i).copied().unwrap_or(false));
            force_null_flags.push(plan.force_null_flags.get(i).copied().unwrap_or(false));
        }
        let inserted_cols = {
            const FLIHAN: i32 = types_tuple::htup::FirstLowInvalidHeapAttributeNumber;
            let mut b = types_nodes::Bitmapset::empty();
            for &a in attnumlist.iter() {
                b.add_member(mcx, a as i32 - FLIHAN)?;
            }
            b
        };
        let max_fields = attnumlist.len();
        let st = CopyFromState {
            opts: CopyFormatOptions {
                file_encoding: plan.opt_file_encoding,
                binary: false,
                csv_mode: plan.csv,
                parquet: false,
                parquet_match_by_name: false,
                parquet_coerce_epoch: false,
                freeze: plan.freeze,
                delim: plan.delim,
                quote: plan.quote,
                escape: plan.escape,
                null_print: &plan.null_print,
                default_print: None,
                header_line: plan.header,
                force_quote: None,
                force_quote_all: false,
                force_notnull: None,
                force_notnull_all: false,
                force_null: None,
                force_null_all: false,
                convert_selectively: false,
                convert_select: None,
                on_error: CopyOnErrorChoice::Stop,
                log_verbosity: crate::CopyLogVerbosityChoice::Default,
                reject_limit: 0,
            },
            src: CopySrc::Chunk { cur: ChunkCursor::new(Vec::new(), None), parked: None },
            raw_buf: vec_from_elem_in(mcx, 0u8, RAW_BUF_SIZE + 1),
            raw_buf_index: 0,
            raw_buf_len: 0,
            raw_reached_eof: false,
            input_reached_eof: false,
            cur_attval_stale: PgVec::new_in(mcx),
            relname_only: false,
            input_reached_error: false,
            input_buf: None,
            input_buf_index: 0,
            input_buf_len: 0,
            line_buf: PgVec::new_in(mcx),
            line_buf_valid: false,
            attribute_buf: PgVec::new_in(mcx),
            binary_attr_buf: stringinfo::StringInfo::new_in(mcx)?,
            raw_fields: PgVec::new_in(mcx),
            max_fields,
            eol_type: EolType::Unknown,
            cur_lineno: 0,
            cur_attidx: None,
            cur_attval_off: None,
            file_encoding: plan.file_encoding,
            need_transcoding: false,
            conversion_proc: 0,
            convertcx: MemoryContext::new("COPY convert"),
            attnumlist,
            in_functions,
            typioparams,
            atttypmods,
            attnames,
            force_notnull_flags,
            force_null_flags,
            convert_select_flags: None,
            defexprs,
            defmap: PgVec::new_in(mcx),
            defaults: vec_from_elem_in(mcx, false, num_phys_attrs),
            where_clause: types_nodes::NodeList::nil(),
            relname: plan.relname.clone(),
            escontext: None,
            num_errors: 0,
            bytes_processed: 0,
            volatile_defexprs: false,
        };
        let slot = tableam::table_slot_create(mcx, rel)?;
        Ok(ParseCx {
            mcx,
            rel,
            st,
            slot,
            check_exprs: None,
            virtual_nn: None,
            inserted_cols,
            row_cx: MemoryContext::new_bump("ParallelTextCopyRow"),
        })
    }
}

/// Owned rows: the final chunk's ragged tail (never a whole grain), fed
/// through `ParSession::append_row`.
struct TailRows {
    /// CONVERT W3-2b: the loading statement's charge on [`TailRows::held_bytes`], synced where a row grew the
    /// blocks (on the parsing helper). Declared first: it drops before the blocks are freed.
    charge: HeldCharge,
    ncols: usize,
    rows: u64,
    cells: Vec<TailCell>,
    arena: Vec<u8>,
}

#[derive(Clone, Copy)]
enum TailCell {
    Null,
    Word(u64),
    Bytes(usize, usize),
}

impl TailRows {
    fn new(ncols: usize, owner: Option<memledger::Owner>) -> TailRows {
        TailRows { charge: HeldCharge::new(owner), ncols, rows: 0, cells: Vec::new(), arena: Vec::new() }
    }

    fn push_row(&mut self, row: &[RawDatum<'_>]) {
        let seen = (self.cells.capacity(), self.arena.capacity());
        for d in row {
            self.cells.push(match d { // alloc: Exec charged self.charge
                RawDatum::Null => TailCell::Null,
                RawDatum::Word(w) => TailCell::Word(*w),
                RawDatum::Bytes(b) => {
                    let off = self.arena.len();
                    self.arena.extend_from_slice(b); // alloc: Exec charged self.charge
                    TailCell::Bytes(off, b.len())
                }
            });
        }
        self.rows += 1;
        if (self.cells.capacity(), self.arena.capacity()) != seen {
            self.charge.sync(self.held_bytes());
        }
    }

    fn pump(&self, sess: &mut ParSession<'_>) -> PgResult<()> {
        let mut row: Vec<RawDatum<'_>> = ::mcx::stdvec::try_vec(self.ncols, "text_pgrc")?;
        for r in 0..self.rows as usize {
            row.clear();
            for c in &self.cells[r * self.ncols..(r + 1) * self.ncols] {
                row.push(match *c {
                    TailCell::Null => RawDatum::Null,
                    TailCell::Word(w) => RawDatum::Word(w),
                    TailCell::Bytes(off, len) => RawDatum::Bytes(&self.arena[off..off + len]),
                });
            }
            sess.append_row(&row).map_err(pgrc_am::write_error)?;
            if r % 8192 == 8191 {
                postgres_seams::check_for_interrupts::call()?;
            }
        }
        Ok(())
    }

    /// D4399: the heap bytes the rows hold (their cell and arena capacity).
    fn held_bytes(&self) -> u64 {
        (self.cells.capacity() * core::mem::size_of::<TailCell>() + self.arena.capacity()) as u64
    }
}

#[derive(Default)]
struct ChunkOut {
    grains: Vec<RowChunk>,
    tail: Option<TailRows>,
    rows: u64,
    /// D4399: the bytes the grains and the tail hold (the parse's true-up, [`Window::parsed`]).
    held: u64,
}

/// D4399: a value's bytes in a RowChunk before this load has parsed a chunk: its cell and a D2 side channel's slot, at
/// least what `RowChunk::held_bytes` counts per value (a test holds it): the prior of [`Window::planned`].
const PRIOR_VALUE_BYTES: u64 = 32;

/// D4399: the text bytes of a chunk's segments.
fn chunk_text(c: &ChunkDesc) -> u64 {
    c.segs.iter().map(|s| (s.end - s.start) as u64).sum()
}

/// D4399: the leader's read and parse window in bytes: the chunks published to the helpers and not yet committed. Each
/// is charged from its publish to its commit (a [`LoadCharge`]: the governor's footprint, the statement's ledger and the
/// load account): its planned residency when published ([`Window::planned`]), the bytes it parsed into once parsed (the
/// true-up, [`Window::parsed`]), and each grain's bytes move to the engine as the leader appends it. What is planned
/// but not resident (the parses still to come) is the engine's `set_ahead` ([`Window::ahead`]). Leader only.
struct Window {
    slots: VecDeque<Slot>,
    /// The id of `slots[0]`: the next chunk to commit.
    base: u64,
    charge: LoadCharge,
    total: u64,
    peak: u64,
    /// Over the chunks parsed so far: their text bytes and the bytes they parsed into.
    parsed_text: u64,
    parsed_held: u64,
}

struct Slot {
    text: u64,
    charged: u64,
    parsed: bool,
}

impl Window {
    fn new() -> Window {
        Window {
            slots: VecDeque::new(),
            base: 0,
            charge: LoadCharge::new(0),
            total: 0,
            peak: 0,
            parsed_text: 0,
            parsed_held: 0,
        }
    }

    fn in_flight(&self) -> u64 {
        self.slots.len() as u64
    }

    /// A chunk's planned residency: its `text` bytes and what its `rows` x `ncols` values parse into, estimated as
    /// text x the expansion this load measured over its parsed chunks, or before any is parsed [`PRIOR_VALUE_BYTES`] a
    /// value plus twice the text (the arena and its growth).
    fn planned(&self, text: u64, rows: u64, ncols: usize) -> u64 {
        let parse = if self.parsed_text != 0 {
            (u128::from(text) * u128::from(self.parsed_held) / u128::from(self.parsed_text)).min(u128::from(u64::MAX)) as u64
        } else {
            rows.saturating_mul(ncols as u64).saturating_mul(PRIOR_VALUE_BYTES).saturating_add(text.saturating_mul(2))
        };
        text.saturating_add(parse)
    }

    /// Chunk `text` bytes published, charged `planned`.
    fn publish(&mut self, text: u64, planned: u64) {
        self.slots.push_back(Slot { text, charged: planned, parsed: false });
        self.set(self.total.saturating_add(planned));
    }

    /// The true-up: chunk `g` parsed into `held` bytes, its charge moves from the estimate to them (up or down).
    fn parsed(&mut self, g: u64, held: u64) {
        let Some(s) = g.checked_sub(self.base).and_then(|i| self.slots.get_mut(i as usize)) else { return };
        if s.parsed {
            return;
        }
        s.parsed = true;
        self.parsed_text = self.parsed_text.saturating_add(s.text);
        self.parsed_held = self.parsed_held.saturating_add(held);
        let total = self.total - s.charged + held;
        s.charged = held;
        self.set(total);
    }

    /// `bytes` of the next chunk to commit moved to the engine (a grain appended).
    fn moved(&mut self, bytes: u64) {
        let Some(s) = self.slots.front_mut() else { return };
        let n = bytes.min(s.charged);
        s.charged -= n;
        self.set(self.total - n);
    }

    /// The next chunk is committed: the rest of its charge goes.
    fn retire(&mut self) {
        if let Some(s) = self.slots.pop_front() {
            self.set(self.total - s.charged);
        }
        self.base += 1;
    }

    /// Planned but not resident: the parses still to come of the chunks published and not parsed.
    fn ahead(&self) -> u64 {
        self.slots.iter().filter(|s| !s.parsed).map(|s| s.charged.saturating_sub(s.text)).sum()
    }

    fn set(&mut self, total: u64) {
        self.total = total;
        self.peak = self.peak.max(total);
        self.charge.move_to(total);
    }
}

fn internal(msg: String) -> Box<PgError> {
    Box::new(PgError::error(msg).with_sqlstate(ERRCODE_INTERNAL_ERROR)) // alloc: Cold
}

/// Parse one chunk with the serial reader; an error returns with the serial
/// context of this reader's state. A chunk above `floor` (the lowest
/// erroring chunk) stops early with `Ok(None)`. `rows_mode` collects owned
/// rows for every chunk (the engine grain differs from the cut grain).
fn parse_chunk(
    cx: &mut ParseCx<'_, '_>,
    plan: &ParsePlan,
    chunk: ChunkDesc,
    g: u64,
    floor: Option<&AtomicU64>,
    rows_mode: bool,
    owner: Option<memledger::Owner>,
) -> Result<Option<ChunkOut>, Box<PgError>> {
    {
        let st = &mut cx.st;
        st.src = CopySrc::Chunk { cur: ChunkCursor::new(chunk.segs, chunk.tail_err), parked: None };
        st.raw_buf_index = 0;
        st.raw_buf_len = 0;
        st.raw_reached_eof = false;
        st.input_reached_eof = false;
        st.input_reached_error = false;
        st.input_buf_index = 0;
        st.input_buf_len = 0;
        st.line_buf.clear();
        st.line_buf_valid = false;
        st.eol_type = chunk.eol_at_start;
        st.cur_lineno = chunk.lineno_before;
        st.cur_attidx = None;
        st.cur_attval_off = None;
        st.bytes_processed = 0;
    }
    let natts = plan.natts;
    let grain = plan.grain;
    let owned = chunk.last || rows_mode;
    let mut out = ChunkOut::default();
    let mut cur = if owned { None } else { Some(RowChunk::with_owner(natts, grain, owner)) };
    let mut tail = if owned { Some(TailRows::new(natts, owner)) } else { None };
    let relid = cx.rel.rd_id;
    loop {
        if out.rows % 4096 == 0 {
            postgres_seams::check_for_interrupts::call()?;
            if floor.is_some_and(|f| g > f.load(Ordering::SeqCst)) {
                return Ok(None);
            }
        }
        cx.row_cx.reset();
        exectuples::exec_clear_tuple(&mut cx.slot, cx.mcx);
        let row_mcx = cx.row_cx.mcx();
        let more = {
            let base = cx.slot.base_mut();
            cx.st.next_copy_from(row_mcx, &mut base.tts_values, &mut base.tts_isnull)
        }
        .map_err(|e| copy_from_error_context(&cx.st, e))?;
        if !more {
            break;
        }
        exectuples::exec_store_virtual_tuple(&mut cx.slot);
        cx.slot.base_mut().tts_tableOid = relid;
        nodemodifytable::exec_constraints_in_row(
            cx.mcx,
            row_mcx,
            &mut cx.check_exprs,
            &mut cx.virtual_nn,
            cx.rel,
            &mut cx.slot,
            None,
            Some(&cx.inserted_cols),
        )
        .map_err(|e| copy_from_error_context(&cx.st, e))?;
        let base = cx.slot.base();
        let row = pgrc_am::ingest::raw_row(cx.rel, &base.tts_values, &base.tts_isnull, None)
            .map_err(|e| copy_from_error_context(&cx.st, e))?;
        if let Some(t) = tail.as_mut() {
            t.push_row(&row);
        } else if let Some(c) = cur.as_mut() {
            c.push_row(&row).map_err(pgrc_am::write_error)?;
            if c.rows() == grain {
                out.grains.push(std::mem::replace(c, RowChunk::with_owner(natts, grain, owner)));
            }
        }
        out.rows += 1;
    }
    if cur.as_ref().is_some_and(|c| c.rows() != 0) || out.rows != chunk.rows {
        return Err(internal(format!( // alloc: Cold
            "parallel COPY: chunk {g} parsed {} rows where the segmentation counted {}",
            out.rows, chunk.rows
        )));
    }
    out.tail = tail;
    out.held = out.grains.iter().map(RowChunk::held_bytes).sum::<u64>() + out.tail.as_ref().map_or(0, TailRows::held_bytes);
    Ok(Some(out))
}

/// D4399: with the window (the helpers' chunks), each grain's charge moves to the engine as it is appended, and the
/// tail's goes once its rows are in the engine.
fn commit_out(sess: &mut ParSession<'_>, out: ChunkOut, processed: &mut u64, mut win: Option<&mut Window>) -> PgResult<()> {
    for rc in out.grains {
        if let Some(w) = win.as_deref_mut() {
            w.moved(rc.held_bytes());
        }
        sess.append_chunk(rc).map_err(pgrc_am::write_error)?;
    }
    if let Some(t) = out.tail {
        t.pump(sess)?;
    }
    if let Some(w) = win {
        w.retire();
    }
    *processed += out.rows;
    pgstat_progress_update_param(PROGRESS_COPY_TUPLES_PROCESSED, *processed as i64);
    Ok(())
}

/// The parallel context's private payload AND the task set's work body.
struct Shared {
    rt: &'static Arc<runtime::Runtime>,
    rg: OnceLock<runtime::WeakRgHandle>,
    source: Arc<runtime::StreamSource>,
    plan: ParsePlan,
    /// The leader inserts a chunk BEFORE the watermark passes its index.
    chunks: Mutex<HashMap<u64, ChunkDesc>>,
    done: Mutex<BTreeMap<u64, ChunkOut>>,
    errors: Mutex<BTreeMap<u64, Box<PgError>>>,
    error_floor: AtomicU64,
    failed_hard: AtomicBool,
    hard_error: Mutex<Option<Box<PgError>>>,
    refused: AtomicUsize,
    started: AtomicUsize,
    leader_proc: types_core::ProcNumber,
    /// CONVERT W3-2b: the loading statement's memory owner (the engine's, read once on the leader where the writer
    /// opened): the helpers capture their chunks under it.
    owner: Option<memledger::Owner>,
}

impl Shared {
    fn record_error(&self, g: u64, e: Box<PgError>) {
        lock(&self.errors).insert(g, e);
        self.error_floor.fetch_min(g, Ordering::SeqCst);
        self.wake_leader();
    }

    fn fail_hard(&self, e: Box<PgError>) {
        {
            let mut h = lock(&self.hard_error);
            if h.is_none() {
                *h = Some(e);
            }
        }
        self.failed_hard.store(true, Ordering::SeqCst);
        if let Some(rg) = self.rg.get().and_then(|w| w.upgrade()) {
            rg.abort();
        }
        self.wake_leader();
    }

    fn wake_leader(&self) {
        latch::SetLatch(types_storage::latch::LatchHandle::proc(self.leader_proc));
    }

    fn error_seen(&self) -> bool {
        self.error_floor.load(Ordering::SeqCst) != u64::MAX
    }

    fn take_min_error(&self) -> Option<Box<PgError>> {
        let mut m = lock(&self.errors);
        let k = *m.keys().next()?;
        m.remove(&k)
    }

    fn take_hard_error(&self) -> Option<Box<PgError>> {
        lock(&self.hard_error).take()
    }

    fn morsel_body(&self, range: runtime::MorselRange) -> PgResult<()> {
        let p = WORKER_CX.with(|c| c.get());
        if p.is_null() {
            return Err(PgError::new(ERROR, "parallel COPY chunk without a bound helper").into()); // alloc: Cold
        }
        // SAFETY: set by THIS thread's entry frame around drive_pinned; the
        // frame outlives the drive, and run_morsel only executes on the
        // claiming thread.
        let wcx: &mut ParseCx<'_, '_> = unsafe { &mut *p };
        for g in range {
            let chunk = lock(&self.chunks).remove(&g);
            let Some(chunk) = chunk else {
                return Err(internal(format!("parallel COPY: chunk {g} claimed before publish"))); // alloc: Cold
            };
            if g > self.error_floor.load(Ordering::SeqCst) {
                continue;
            }
            match parse_chunk(wcx, &self.plan, chunk, g, Some(&self.error_floor), false, self.owner) {
                Ok(Some(out)) => {
                    lock(&self.done).insert(g, out);
                    self.wake_leader();
                }
                Ok(None) => {}
                Err(e) => self.record_error(g, e),
            }
        }
        Ok(())
    }
}

impl runtime::TaskSetWork for Shared {
    fn run_morsel(&self, _worker: usize, range: runtime::MorselRange) {
        if self.failed_hard.load(Ordering::SeqCst) {
            return;
        }
        // unwind-ok: worker-containment
        let r = catch_unwind(AssertUnwindSafe(|| self.morsel_body(range)));
        match r {
            Ok(Ok(())) => {}
            Ok(Err(e)) => self.fail_hard(e),
            Err(unwind) => match unwound(unwind) {
                Ok(e) => self.fail_hard(e),
                Err(unwind) => {
                    let what = gone(&*unwind).unwrap_or("parallel COPY helper panicked in a chunk");
                    self.fail_hard(PgError::new(ERROR, what).into()); // alloc: Cold
                    // D2933: an exit payload or a plain panic (C's abort()) goes on
                    parallel::standing::resume_exit_or_abort(unwind);
                }
            },
        }
    }

    fn finalize(&self) {}
}

/// D3350: an unwind out of a helper's work (`run_morsel`, `parallel_text_copy_main`). A
/// structured error (D2933: a `PgError` raised by unwinding, such as a memory limit's out of
/// memory or a MaxAllocSize ceiling) is the load's error as raised, its SQLSTATE, message and
/// context kept; every other payload (an exit, a plain panic) comes back for the thread top.
fn unwound(
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Box<PgError>, Box<dyn std::any::Any + Send>> {
    match payload.downcast::<Box<PgError>>() {
        Ok(e) => Ok(*e),
        Err(payload) => payload.downcast::<PgError>(),
    }
}

/// D3350: an exit payload (a FATAL's thread exit: a terminate or a fast shutdown; a pool slot's
/// retirement) is the helper going away, never a panic: PostgreSQL's leader words a worker it
/// lost so. The helper's own report (its FATAL, relayed as an ERROR) usually reaches the leader
/// first.
fn gone(payload: &(dyn std::any::Any + Send)) -> Option<&'static str> {
    let exit = parallel::standing::is_exit_unwind(payload)
        || payload.is::<parallel::standing::PoolRetireRaw>();
    exit.then_some("lost connection to parallel worker")
}

thread_local! {
    static WORKER_CX: std::cell::Cell<*mut ParseCx<'static, 'static>> =
        const { std::cell::Cell::new(std::ptr::null_mut()) };
}

/// Clears WORKER_CX when the helper's drive frame ends, an unwinding drive included (C7 clause (d)): the published
/// pointer never outlives the frame's `ParseCx`.
struct WorkerCxPublished;

impl Drop for WorkerCxPublished {
    fn drop(&mut self) {
        WORKER_CX.with(|c| c.set(std::ptr::null_mut()));
    }
}

fn parallel_text_copy_main(pshared: &parallel::ParallelShared) -> PgResult<()> {
    let Some(private) = pshared.private() else { return Ok(()) };
    let Ok(shared) = private.downcast::<Shared>() else { return Ok(()) };
    // unwind-ok: worker-containment
    let r = catch_unwind(AssertUnwindSafe(|| worker_drive(&shared)));
    let outcome = match r {
        Ok(o) => o,
        Err(unwind) => match unwound(unwind) {
            Ok(e) => {
                shared.fail_hard(e);
                Err(Box::new(PgError::new(ERROR, "parallel COPY helper failed (see leader error)"))) // alloc: Cold
            }
            Err(unwind) => {
                let what = gone(&*unwind).unwrap_or("parallel COPY helper panicked");
                shared.fail_hard(PgError::new(ERROR, what).into()); // alloc: Cold
                // D2933: an exit payload or a plain panic (C's abort()) goes on
                latch::SetLatch(types_storage::latch::LatchHandle::proc(
                    pshared.parallel_leader_proc_number,
                ));
                parallel::standing::resume_exit_or_abort(unwind)
            }
        },
    };
    latch::SetLatch(types_storage::latch::LatchHandle::proc(pshared.parallel_leader_proc_number));
    outcome
}

fn worker_drive(shared: &Arc<Shared>) -> PgResult<()> {
    let Some(rg) = shared.rg.get().and_then(|w| w.upgrade()) else {
        shared.refused.fetch_add(1, Ordering::SeqCst);
        return Ok(());
    };
    let Some(lane) = shared.rt.acquire_external_lane() else {
        shared.refused.fetch_add(1, Ordering::SeqCst);
        return Ok(());
    };
    let mut lane_local = lane.local();

    let ctx = MemoryContext::new("parallel text COPY helper");
    let mcx = ctx.mcx();
    let rel = match table::table_open(mcx, shared.plan.relid, types_rel::lock::RowExclusiveLock) {
        Ok(rel) => rel,
        Err(e) => {
            // A pinned RG needs SOME driver to run its protocol cleanup.
            shared.fail_hard(e);
            if rg.try_outcome().is_none() {
                rg.abort();
                let _ = shared.rt.drive_pinned(&mut lane_local, &rg);
            }
            return Ok(());
        }
    };
    let mut wcx = match ParseCx::build(mcx, &rel, &shared.plan) {
        Ok(w) => w,
        Err(e) => {
            shared.fail_hard(e);
            if rg.try_outcome().is_none() {
                rg.abort();
                let _ = shared.rt.drive_pinned(&mut lane_local, &rg);
            }
            table::table_close(rel, types_rel::lock::RowExclusiveLock)?;
            return Ok(());
        }
    };
    shared.started.fetch_add(1, Ordering::SeqCst);

    // SAFETY (lifetime erasure): wcx outlives the drive on this frame; the
    // pointer is cleared (WorkerCxPublished's drop, declared after wcx, so
    // first on an unwind too) before wcx drops.
    WORKER_CX.with(|c| {
        c.set(unsafe {
            core::mem::transmute::<*mut ParseCx<'_, '_>, *mut ParseCx<'static, 'static>>(
                &mut wcx as *mut ParseCx<'_, '_>,
            )
        })
    });
    let published = WorkerCxPublished;
    let _outcome = shared.rt.drive_pinned(&mut lane_local, &rg);
    drop(published);
    drop(wcx);
    table::table_close(rel, types_rel::lock::RowExclusiveLock)?;

    if shared.failed_hard.load(Ordering::SeqCst) {
        // Abort the helper transaction so resowner releases residue; the
        // leader rethrows the recorded error, never this message.
        return Err(Box::new(PgError::new( // alloc: Cold
            ERROR,
            "parallel COPY helper failed (see leader error)",
        )));
    }
    Ok(())
}

/// DestroyParallelContext (the abort path included) calls this BEFORE it
/// waits for helper exit: every helper's drive must observe completion.
fn private_shutdown(private: &(dyn std::any::Any + Send + Sync)) {
    let Some(payload) = private.downcast_ref::<Shared>() else { return };
    payload.source.close();
    payload.rt.notify_source_progress();
    if let Some(rg) = payload.rg.get().and_then(|w| w.upgrade()) {
        if rg.try_outcome().is_none() {
            drain_rg(payload.rt, &rg);
        }
    }
}

fn ensure_hooks_registered() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        parallel::register_parallel_worker_entrypoint(
            "pgrust_parallel_text_copy_main",
            parallel_text_copy_main,
        );
        parallel::register_parallel_private_shutdown(private_shutdown);
    });
}

/// Abort + BOUNDED drain of a pinned RG no helper will drive. False =
/// leaked (dead participant).
fn drain_rg(rt: &'static Arc<runtime::Runtime>, rg: &runtime::RgHandle) -> bool {
    rg.abort();
    rt.notify_source_progress();
    let mut lane = None;
    for _ in 0..4000 {
        if let Some(l) = rt.acquire_external_lane() {
            lane = Some(l);
            break;
        }
        std::thread::sleep(std::time::Duration::from_micros(500));
    }
    let Some(lane) = lane else { return false };
    let mut local = lane.local();
    rt.try_drain_pinned(&mut local, rg, 4000).is_some()
}

struct Lead {
    seg: Segmentator,
    pending: VecDeque<ChunkDesc>,
    /// EOF, the end-of-copy marker, or a read error: the final chunk is cut.
    input_done: bool,
    bytes_read: u64,
    frontend: bool,
    /// The serial reader's last error: the frontend drain after the marker
    /// failed (contexted).
    post_err: Option<Box<PgError>>,
    /// CONVERT W3-2b: the loading statement's owner (read once on this leader): the read blocks are charged to it.
    owner: Option<memledger::Owner>,
}

/// CONVERT W3-2b: a read block the allocator refused (the statement's out of memory, never an abort).
#[cold]
fn read_block_refused() -> PgError {
    PgError::new(ERROR, "out of memory")
        .with_sqlstate(types_error::ERRCODE_OUT_OF_MEMORY)
        .with_detail(format!("Failed on request of size {READ_BLOCK} in parallel COPY.")) // alloc: Cold
}

impl Lead {
    /// Read and segment one block. A read ERROR is not raised here: it
    /// rides the final chunk's cursor, after the bytes delivered before it.
    fn read_more(&mut self, cstate: &mut CopyFromState<'_, '_>) -> PgResult<()> {
        debug_assert!(!self.input_done);
        // CONVERT W3-2b: the block is the statement's until the helper that parses its last line drops it
        let mut buf = SegBlock::new(self.owner, memledger::Kind::Region);
        buf.resize(READ_BLOCK, 0).map_err(|_| read_block_refused())?; // alloc: Exec charged self.owner
        let mut out = Vec::new();
        let (n, err) = cstate.copy_read_stream(buf.as_mut_slice());
        self.bytes_read += n as u64;
        pgstat_progress_update_param(PROGRESS_COPY_BYTES_PROCESSED, self.bytes_read as i64);
        if n > 0 {
            buf.resize(n, 0).map_err(|_| read_block_refused())?; // a truncate: the block (and its charge) stays whole
            self.seg.feed(&Arc::new(buf), &mut out)?;
        }
        if let Some(e) = err {
            // D3350: an error past ERROR (the session's FATAL: a terminate or a fast shutdown
            // arriving during the client read) ends the session at once, as PostgreSQL's read
            // raises it; only an ERROR rides the final chunk.
            if e.level() >= FATAL {
                return Err(e);
            }
            if self.seg.input_complete() {
                self.post_err = Some(self.drain_context(cstate, e));
            } else {
                self.seg.finish(&mut out);
                if let Some(last) = out.last_mut() {
                    last.tail_err = Some(e);
                }
                self.input_done = true;
                self.pending.extend(out);
                return Ok(());
            }
        }
        if self.seg.input_complete() && self.frontend && !cstate.raw_reached_eof && self.post_err.is_none() {
            let mut sink = memledger::ChargedVec::new(self.owner, memledger::Kind::Region);
            sink.with_vec(|v| v.resize(READ_BLOCK, 0u8)); // alloc: Exec charged self.owner
            loop {
                let (k, e) = cstate.copy_read_stream(&mut sink);
                if let Some(e) = e {
                    if e.level() >= FATAL {
                        return Err(e);
                    }
                    self.post_err = Some(self.drain_context(cstate, e));
                    break;
                }
                if k == 0 || cstate.raw_reached_eof {
                    break;
                }
            }
        }
        if n == 0 || cstate.raw_reached_eof || self.seg.input_complete() {
            self.seg.finish(&mut out);
            self.input_done = true;
        }
        self.pending.extend(out);
        Ok(())
    }

    /// `copy_read_line`'s drain fails before `line_buf_valid`: bare context.
    fn drain_context(&self, cstate: &CopyFromState<'_, '_>, e: Box<PgError>) -> Box<PgError> {
        Box::new(e.add_context(format!("COPY {}, line {}", cstate.relname, self.seg.eoc_lineno))) // alloc: Cold
    }
}

/// Base or enum, or an array of one: domains, composites and ranges run
/// nested input functions admission cannot vet.
fn simple_type(typid: Oid) -> PgResult<bool> {
    use lsyscache::typ::{get_element_type, get_typtype, TYPTYPE_BASE, TYPTYPE_ENUM};
    let t = get_typtype(typid)?;
    if t == TYPTYPE_ENUM {
        return Ok(true);
    }
    if t != TYPTYPE_BASE {
        return Ok(false);
    }
    let elem = get_element_type(typid)?;
    if elem == 0 {
        return Ok(true);
    }
    let te = get_typtype(elem)?;
    Ok(te == TYPTYPE_BASE || te == TYPTYPE_ENUM)
}

fn admit(
    cstate: &CopyFromState<'_, '_>,
    rel: &Relation<'_>,
    has_triggers: bool,
) -> PgResult<Option<(&'static Arc<runtime::Runtime>, ParsePlan)>> {
    if !knobs().on || !runtime::runtime_enabled() {
        return Ok(None);
    }
    let o = &cstate.opts;
    if o.binary || o.parquet {
        return Ok(None);
    }
    if tableam_vocab::TableAm::of(rel) != Some(tableam_vocab::TableAm::Pgrcolumnar) {
        return Ok(None);
    }
    // RL OC-4 (a), as the parquet path: the parallel feed never reaches the tableam bulk arm, which logs BULK_ROWS.
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
    if rel.rd_rel.relispartition {
        refuse!("relation is a partition");
    }
    if rel.uses_local_buffers() {
        refuse!("temporary relation");
    }
    if let Some(c) = rel.rd_att.constr.as_deref() {
        if c.has_generated_stored || c.has_generated_virtual {
            refuse!("generated columns");
        }
        if c.num_check > 0 {
            refuse!("CHECK constraints");
        }
    }
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
    if cstate.need_transcoding || cstate.file_encoding != mbutils::GetDatabaseEncoding() {
        refuse!("encoding conversion");
    }
    if cstate.escontext.is_some() {
        refuse!("soft-error context");
    }
    let natts = rel.rd_att.natts as usize;
    if natts == 0 || cstate.attnumlist.is_empty() {
        refuse!("no columns");
    }
    for &attnum in cstate.attnumlist.iter() {
        let att = rel.rd_att.attr(attnum as usize - 1);
        if !att.attbyval && att.attlen != -1 {
            refuse!("fixed-length by-reference column");
        }
        if !simple_type(att.atttypid)? {
            refuse!("a domain, composite or range column");
        }
        let (func_oid, _) = lsyscache::typ::getTypeInputInfo(att.atttypid)?;
        if lsyscache::function::func_parallel(func_oid)? != b's' as i8 {
            refuse!("an input function that is not parallel safe");
        }
    }
    match &cstate.src {
        CopySrc::File { fd, .. } => {
            // fstat, not pg_file_size_raw: that one is lseek(SEEK_END) and
            // leaves the read position at the end of the file.
            let size = fd::with_allocated_stdio(*fd, |f| {
                use std::os::fd::AsRawFd;
                let mut st = fd::FileInfo::zeroed();
                if fd::pg_fstat(f.as_raw_fd(), &mut st) == 0 { st.size } else { 0 }
            })
            .unwrap_or(0);
            if size < knobs().min_file_bytes as i64 {
                refuse!(format!("file smaller than the {}B floor", knobs().min_file_bytes)); // alloc: Cold
            }
        }
        CopySrc::Frontend { .. } => {}
        _ => refuse!("source is not a file or the client stream"),
    }
    Ok(Some((
        rt,
        ParsePlan {
            relid: rel.rd_id,
            relname: cstate.relname.clone(),
            csv: o.csv_mode,
            delim: o.delim,
            quote: o.quote,
            escape: o.escape,
            null_print: o.null_print.to_string(),
            header: o.header_line,
            opt_file_encoding: o.file_encoding,
            file_encoding: cstate.file_encoding,
            freeze: o.freeze,
            attnumlist: cstate.attnumlist.iter().copied().collect(),
            force_notnull_flags: (0..natts).map(|i| cstate.force_notnull_flags.get(i).copied().unwrap_or(false)).collect(),
            force_null_flags: (0..natts).map(|i| cstate.force_null_flags.get(i).copied().unwrap_or(false)).collect(),
            natts,
            grain: DEFAULT_CHUNK_ROWS,
        },
    )))
}

/// `Ok(None)` = run the serial body: refused (cstate untouched) or below the
/// engagement floor (cstate reads the buffered prefix, its source parked
/// for EndCopyFrom). `Ok(Some(n))` = n rows loaded and published through
/// `table_finish_bulk_insert`. Errors return FULLY CONTEXTED.
pub(crate) fn copy_from_text_pgrc<'mcx>(
    mcx: Mcx<'mcx>,
    cstate: &mut CopyFromState<'mcx, '_>,
    rel: &Relation<'mcx>,
    has_triggers: bool,
    ti_options: i32,
) -> PgResult<Option<u64>> {
    let Some((rt, plan)) = admit(cstate, rel, has_triggers)? else {
        return Ok(None);
    };
    let kn = knobs();
    let o = &cstate.opts;
    let mut lead = Lead {
        seg: Segmentator::new(SegOpts {
            csv: o.csv_mode,
            quote: o.quote,
            escape: o.escape,
            header: o.header_line != CopyHeaderChoice::False,
            grain: plan.grain as u64,
            grains: kn.grains,
            target_bytes: CHUNK_TARGET_BYTES,
            max_line_bytes: kn.max_line_bytes,
        }),
        pending: VecDeque::new(),
        input_done: false,
        bytes_read: 0,
        frontend: matches!(cstate.src, CopySrc::Frontend { .. }),
        post_err: None,
        owner: pgrc_am::ingest::load_owner(),
    };

    let below = |l: &Lead| l.seg.rows_total < kn.min_rows && l.bytes_read < kn.min_bytes;
    while !lead.input_done && below(&lead) {
        lead.read_more(cstate)?;
    }
    if lead.input_done && below(&lead) && lead.post_err.is_none() {
        ptrace(&format!( // alloc: Cold
            "pgrc-text serial below the floor rows={} bytes={}",
            lead.seg.rows_total, lead.bytes_read
        ));
        let mut segs = Vec::new();
        let mut tail_err = None;
        for c in lead.pending.drain(..) {
            segs.extend(c.segs);
            if c.tail_err.is_some() {
                tail_err = c.tail_err;
            }
        }
        let parked = std::mem::replace(
            &mut cstate.src,
            CopySrc::Chunk { cur: ChunkCursor::new(segs, tail_err), parked: None },
        );
        if let CopySrc::Chunk { parked: slot, .. } = &mut cstate.src {
            *slot = Some(Box::new(parked));
        }
        cstate.raw_buf_index = 0;
        cstate.raw_buf_len = 0;
        cstate.raw_reached_eof = false;
        cstate.input_reached_eof = false;
        cstate.input_reached_error = false;
        cstate.input_buf_index = 0;
        cstate.input_buf_len = 0;
        cstate.bytes_processed = 0;
        return Ok(None);
    }

    let k = helpers_for(kn.helpers, rt.config().workers) as i32;
    // CONVERT W3-2b: built once the session exists, so the helpers take the engine's owner
    let new_shared = move |owner: Option<memledger::Owner>| Arc::new(Shared {
        rt,
        rg: OnceLock::new(),
        source: Arc::new(runtime::StreamSource::new()),
        plan,
        chunks: Mutex::new(HashMap::new()),
        done: Mutex::new(BTreeMap::new()),
        errors: Mutex::new(BTreeMap::new()),
        error_floor: AtomicU64::new(u64::MAX),
        failed_hard: AtomicBool::new(false),
        hard_error: Mutex::new(None),
        refused: AtomicUsize::new(0),
        started: AtomicUsize::new(0),
        leader_proc: init_small::globals::MyProcNumber(),
        owner,
    });
    ensure_hooks_registered();

    let mut stash: Option<Box<PgError>> = None;
    let fed: PgResult<u64> = pgrc_am::ingest::with_parallel_copy_writer(rel, |writer, providers| {
        let r = pgrc_ingest_par::parallel_ingest(writer, rt, providers, ParIngestOpts::auto(), |sess| {
            ptrace(&format!( // alloc: Cold
                "pgrc-text engaged helpers={k} grains={} grain={} parts_in_flight={} chunks_in_flight={}",
                kn.grains,
                sess.chunk_rows(),
                sess.opts().max_parts_in_flight,
                sess.opts().max_chunks_in_flight
            ));
            let shared = new_shared(sess.owner());
            let fed = lead_feed(mcx, cstate, rel, &shared, rt, k, &mut lead, sess);
            let (peak, waits, ms) = sess.memory_report();
            ptrace(&format!("pgrc-text memory peak_held={peak} gate_waits={waits} gate_ms={ms}")); // alloc: Cold
            match fed {
                Ok(n) => Ok(n),
                Err(e) => {
                    stash = Some(e);
                    Err(STASHED)
                }
            }
        });
        r.map_err(|we| stash.take().unwrap_or_else(|| pgrc_am::write_error(we)))
    });
    let processed = fed?;

    tableam::table_finish_bulk_insert(rel, ti_options)?;
    pgstat_progress_update_param(PROGRESS_COPY_TUPLES_PROCESSED, processed as i64);
    ptrace(&format!("pgrc-text done rows={processed}")); // alloc: Cold
    Ok(Some(processed))
}

enum Ceremony {
    /// No helper took part: `published` chunks wait unclaimed in the map.
    Refused { published: u64 },
    Done(u64),
}

#[allow(clippy::too_many_arguments)]
fn lead_feed<'mcx>(
    mcx: Mcx<'mcx>,
    cstate: &mut CopyFromState<'mcx, '_>,
    rel: &Relation<'mcx>,
    shared: &Arc<Shared>,
    rt: &'static Arc<runtime::Runtime>,
    k: i32,
    lead: &mut Lead,
    sess: &mut ParSession<'_>,
) -> PgResult<u64> {
    if sess.chunk_rows() != shared.plan.grain {
        ptrace(&format!( // alloc: Cold
            "pgrc-text engine grain {} != cut grain {}: the leader parses",
            sess.chunk_rows(),
            shared.plan.grain
        ));
        return leader_parse_all(mcx, cstate, rel, shared, lead, sess, 0, true);
    }
    xact::EnterParallelMode();
    let r = ceremony(cstate, shared, rt, k, lead, sess);
    xact::ExitParallelMode();
    match r? {
        Ceremony::Done(n) => Ok(n),
        Ceremony::Refused { published } => {
            ptrace(&format!("pgrc-text no helper took part: the leader parses ({published} chunks published)")); // alloc: Cold
            leader_parse_all(mcx, cstate, rel, shared, lead, sess, published, false)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn leader_parse_all<'mcx>(
    mcx: Mcx<'mcx>,
    cstate: &mut CopyFromState<'mcx, '_>,
    rel: &Relation<'mcx>,
    shared: &Arc<Shared>,
    lead: &mut Lead,
    sess: &mut ParSession<'_>,
    published: u64,
    rows_mode: bool,
) -> PgResult<u64> {
    let mut cx = ParseCx::build(mcx, rel, &shared.plan)?;
    let mut processed = 0u64;
    let mut g = 0u64;
    loop {
        let chunk = if g < published { lock(&shared.chunks).remove(&g) } else { lead.pending.pop_front() };
        let Some(chunk) = chunk else {
            if g < published {
                return Err(internal(format!("parallel COPY: published chunk {g} vanished"))); // alloc: Cold
            }
            if lead.input_done {
                break;
            }
            lead.read_more(cstate)?;
            continue;
        };
        let Some(out) = parse_chunk(&mut cx, &shared.plan, chunk, g, None, rows_mode, shared.owner)? else {
            return Err(internal(format!("parallel COPY: chunk {g} stopped without an error"))); // alloc: Cold
        };
        commit_out(sess, out, &mut processed, None)?;
        g += 1;
    }
    if let Some(e) = lead.post_err.take() {
        return Err(e);
    }
    Ok(processed)
}

/// Commit ready chunks in order; none once an error is recorded (the
/// statement fails and the engine work would be discarded).
fn commit_ready(
    shared: &Shared,
    sess: &mut ParSession<'_>,
    next_commit: &mut u64,
    processed: &mut u64,
    win: &mut Window,
) -> PgResult<bool> {
    let mut any = false;
    while !shared.error_seen() {
        let out = lock(&shared.done).remove(next_commit);
        let Some(out) = out else { break };
        win.parsed(*next_commit, out.held);
        commit_out(sess, out, processed, Some(win))?;
        *next_commit += 1;
        any = true;
    }
    Ok(any)
}

fn ceremony(
    cstate: &mut CopyFromState<'_, '_>,
    shared: &Arc<Shared>,
    rt: &'static Arc<runtime::Runtime>,
    k: i32,
    lead: &mut Lead,
    sess: &mut ParSession<'_>,
) -> PgResult<Ceremony> {
    let pcxt = parallel::CreateParallelContext("postgres", "pgrust_parallel_text_copy_main", k)?;
    // Held outside the body so ?-propagated errors reap the RG too.
    let mut submitted: Option<runtime::RgHandle> = None;

    let body = (|submitted: &mut Option<runtime::RgHandle>| -> PgResult<Ceremony> {
        parallel::InitializeParallelDSM(pcxt)?;
        if parallel::nworkers(pcxt) <= 0 {
            return Ok(Ceremony::Refused { published: 0 });
        }
        parallel::set_private(pcxt, Arc::clone(shared) as _);

        static NEXT_QUERY_ID: AtomicU64 = AtomicU64::new(1);
        let work: Arc<dyn runtime::TaskSetWork> = Arc::clone(shared) as _;
        let source: Arc<dyn runtime::MorselSource> = Arc::clone(&shared.source) as _;
        let (rg, waiter) = rt.submit_pinned(runtime::QuerySpec {
            query_id: NEXT_QUERY_ID.fetch_add(1, Ordering::SeqCst),
            tasksets: vec![runtime::TaskSetSpec { source, work, deps: vec![] }],
        });
        shared
            .rg
            .set(rg.downgrade())
            .unwrap_or_else(|_| unreachable!("rg set once per statement payload"));
        *submitted = Some(rg.clone());

        let launched = parallel::LaunchParallelWorkers(pcxt)?;
        if launched <= 0 {
            drain_rg(rt, &rg);
            return Ok(Ceremony::Refused { published: 0 });
        }
        let window = window(k);
        ptrace(&format!("pgrc-text launched helpers={launched} window={window}")); // alloc: Cold

        let mut published = 0u64;
        let mut next_commit = 0u64;
        let mut processed = 0u64;
        let mut closed = false;
        // D4399: the window in bytes too, each chunk asked at the engine's gate before its publish (the leader alone
        // waits there; the helpers never wait on memory). One chunk in flight is the floor: the engine's own gate weighs
        // it at its commit.
        let ncols = shared.plan.natts;
        let mut win = Window::new();
        let mut gate = AheadGate::default();
        let (mut held_back, mut gate_waits) = (false, 0u64);
        let outcome = loop {
            // D4399: the true-up of the chunks parsed since the last pass, before the engine's gate weighs a commit
            {
                let d = lock(&shared.done);
                for (&g, out) in d.iter() {
                    win.parsed(g, out.held);
                }
            }
            sess.set_ahead(win.ahead());
            let committed_any = commit_ready(shared, sess, &mut next_commit, &mut processed, &mut win)?;

            let mut read_any = false;
            let was_held = held_back;
            held_back = false;
            if !shared.error_seen() {
                let mut newly = 0u64;
                while published + newly - next_commit < window {
                    let Some(c) = lead.pending.front() else { break };
                    let text = chunk_text(c);
                    let planned = win.planned(text, c.rows, ncols);
                    if win.in_flight() == 0 {
                        gate = AheadGate::default();
                    } else {
                        sess.set_ahead(win.ahead());
                        match sess.ahead_step(&mut gate, planned) {
                            AheadStep::Fits => {}
                            AheadStep::Wait => {
                                held_back = true;
                                break;
                            }
                            AheadStep::Refused(we) => return Err(pgrc_am::write_error(we)),
                        }
                    }
                    let Some(c) = lead.pending.pop_front() else { break };
                    win.publish(text, planned);
                    lock(&shared.chunks).insert(published + newly, c);
                    newly += 1;
                }
                sess.set_ahead(win.ahead());
                if held_back && !was_held {
                    gate_waits += 1;
                }
                if newly > 0 {
                    published += newly;
                    shared.source.publish(published);
                    rt.notify_source_progress();
                }
                if !lead.input_done && lead.pending.is_empty() && published - next_commit < window {
                    lead.read_more(cstate)?;
                    read_any = true;
                }
                if lead.input_done && lead.pending.is_empty() && !closed {
                    shared.source.close();
                    rt.notify_source_progress();
                    closed = true;
                    ptrace(&format!( // alloc: Cold
                        "pgrc-text input closed chunks={published} rows={} bytes={}",
                        lead.seg.rows_total, lead.bytes_read
                    ));
                }
            } else if !closed {
                // Claims above the error floor drain as no-ops.
                shared.source.close();
                rt.notify_source_progress();
                closed = true;
            }

            if let Some(o) = waiter.try_wait() {
                break o;
            }
            if let Err(e) = postgres_seams::check_for_interrupts::call()
                .and_then(|()| parallel::ProcessParallelMessages())
            {
                drain_rg(rt, &rg);
                return Err(e);
            }
            let started = shared.started.load(Ordering::SeqCst);
            if parallel::parallel_workers_all_stopped(pcxt) {
                if let Some(o) = waiter.try_wait() {
                    break o;
                }
                let claimed = rg.stats().tasks_claimed;
                let drained = drain_rg(rt, &rg);
                if started == 0 && claimed == 0 && drained {
                    return Ok(Ceremony::Refused { published });
                }
                if let Some(e) = shared.take_hard_error() {
                    return Err(e);
                }
                return Err(Box::new(PgError::new( // alloc: Cold
                    ERROR,
                    "parallel COPY helpers exited before completing the load",
                )));
            }
            let refused = shared.refused.load(Ordering::SeqCst);
            if started == 0 && refused >= launched as usize {
                let claimed = rg.stats().tasks_claimed;
                if drain_rg(rt, &rg) && claimed == 0 {
                    return Ok(Ceremony::Refused { published });
                }
                return Err(Box::new(PgError::new( // alloc: Cold
                    ERROR,
                    "parallel COPY: every helper refused participation",
                )));
            }
            if !committed_any && !read_any {
                // D4399: a leader held back at the gate asks again after its poll period
                let waited = if held_back {
                    parallel::wait_parallel_finish_quantum_bounded(AheadGate::POLL.as_millis() as i64)
                } else {
                    parallel::wait_parallel_finish_quantum()
                };
                if let Err(e) = waited {
                    drain_rg(rt, &rg);
                    return Err(e);
                }
            }
        };

        commit_ready(shared, sess, &mut next_commit, &mut processed, &mut win)?;
        sess.set_ahead(0);
        ptrace(&format!("pgrc-text window peak_bytes={} gate_waits={gate_waits}", win.peak)); // alloc: Cold
        // D3350: the leader's own pending interrupt first (its FATAL on a terminate or a fast
        // shutdown, which signals the helpers too), before anything a helper recorded: the helpers'
        // exits are its consequence, and PostgreSQL's session ends on that FATAL.
        postgres_seams::check_for_interrupts::call()?;
        if let Some(e) = shared.take_hard_error() {
            return Err(e);
        }
        // Every chunk below the lowest erroring one completed or recorded
        // its own, earlier error.
        if let Some(e) = shared.take_min_error() {
            return Err(e);
        }
        if outcome == runtime::RgOutcome::Aborted {
            postgres_seams::check_for_interrupts::call()?;
            return Err(Box::new(PgError::new(ERROR, "parallel COPY aborted"))); // alloc: Cold
        }
        if next_commit != published || !lead.input_done || !lead.pending.is_empty() {
            return Err(internal(format!( // alloc: Cold
                "parallel COPY: the load completed with chunks uncommitted ({next_commit} of {published})"
            )));
        }
        if let Some(e) = lead.post_err.take() {
            return Err(e);
        }
        Ok(Ceremony::Done(processed))
    })(&mut submitted);

    // The RG must be COMPLETE before the context is destroyed (helpers
    // reference the payload until then).
    if let Some(rg) = &submitted {
        if rg.try_outcome().is_none() {
            shared.source.close();
            rt.notify_source_progress();
            drain_rg(rt, rg);
        }
    }
    // D4399: a load that ends early (a cancel during the gate's wait among them) gives its parsed and unclaimed chunks
    // back now (their window charge went with the body)
    if body.is_err() {
        lock(&shared.done).clear();
        lock(&shared.chunks).clear();
    }
    let destroy = parallel::DestroyParallelContext(pcxt);
    let out = body?;
    destroy?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{gone, helpers_for, unwound, window, Window, PRIOR_VALUE_BYTES};
    use pgrc_write::ingest::RawDatum;
    use pgrc_write::par::RowChunk;
    use types_error::{PgError, ERRCODE_OUT_OF_MEMORY, ERROR};

    const MIB: u64 = 1 << 20;

    /// D4399: the window tests read the load account, process-wide: one at a time.
    static SERIAL: pgsync::Mutex<()> = pgsync::Mutex::new(());

    /// D4399: the bytes the window charges (the load account; the footprint and the statement's ledger move with it).
    fn charged() -> i64 {
        memledger::LOAD_IN_FLIGHT.bytes() as i64
    }

    /// D4399: the prior is at least what a RowChunk holds per value (its cells, before any arena).
    #[test]
    fn d4399_the_prior_covers_a_row_chunks_cells() {
        let (ncols, grain) = (5usize, 64u32);
        let mut c = RowChunk::new(ncols, grain);
        for i in 0..grain {
            let row: Vec<RawDatum<'_>> = (0..ncols).map(|_| RawDatum::Word(u64::from(i))).collect();
            c.push_row(&row).expect("push");
        }
        assert!(c.held_bytes() <= ncols as u64 * u64::from(grain) * PRIOR_VALUE_BYTES, "{} bytes", c.held_bytes());
    }

    /// D4399: the window's arithmetic. A chunk is charged its plan when published (the prior before any parse, the
    /// measured expansion after), the bytes it parsed into once parsed (a shortfall charged at once: the ledger carries
    /// the real bytes), and each grain's bytes leave as they move to the engine; `ahead` is the parses still to come.
    #[test]
    fn d4399_the_window_charges_the_plan_then_the_parse_then_moves_it_to_the_engine() {
        let _g = pgsync::lock(&SERIAL);
        let base = charged();
        let mut w = Window::new();
        let p0 = w.planned(10 * MIB, 1000, 4);
        assert_eq!(p0, 10 * MIB + 1000 * 4 * PRIOR_VALUE_BYTES + 20 * MIB, "the prior: text + values + twice the text");
        w.publish(10 * MIB, p0);
        w.publish(10 * MIB, p0);
        assert_eq!(w.in_flight(), 2);
        assert_eq!(charged() - base, 2 * p0 as i64);
        assert_eq!(w.ahead(), 2 * (p0 - 10 * MIB), "unparsed: all but the text is still to come");
        // chunk 0 parsed into 30 MiB: 3x its text from now on
        w.parsed(0, 30 * MIB);
        assert_eq!(charged() - base, (p0 + 30 * MIB) as i64, "the true-up: the actual bytes, not the plan");
        assert_eq!(w.ahead(), p0 - 10 * MIB);
        assert_eq!(w.planned(10 * MIB, 1, 1), 40 * MIB, "text + text x the measured expansion");
        // chunk 1 parsed into more than its plan: the shortfall is charged at once
        w.parsed(1, p0 + 5 * MIB);
        assert_eq!(charged() - base, (30 * MIB + p0 + 5 * MIB) as i64);
        assert_eq!(w.ahead(), 0);
        w.parsed(1, 1);
        assert_eq!(charged() - base, (30 * MIB + p0 + 5 * MIB) as i64, "a chunk is trued up once");
        // commit chunk 0: two grains move to the engine, then the rest goes
        w.moved(12 * MIB);
        w.moved(12 * MIB);
        assert_eq!(charged() - base, (6 * MIB + p0 + 5 * MIB) as i64);
        w.retire();
        assert_eq!(w.in_flight(), 1);
        assert_eq!(charged() - base, (p0 + 5 * MIB) as i64);
        w.moved(u64::MAX);
        w.retire();
        assert_eq!(charged(), base, "the window is given back whole");
        assert_eq!(w.peak, 30 * MIB + p0 + 5 * MIB);
    }

    /// D4399: a cancel during the gate's wait ends the load with its window given back: the chunks published and parsed
    /// (charged) and the ones still parsing (planned) leave the ledger with the window.
    #[test]
    fn d4399_a_cancel_during_the_wait_gives_the_window_back() {
        let _g = pgsync::lock(&SERIAL);
        let base = charged();
        let mut w = Window::new();
        for _ in 0..4 {
            let p = w.planned(8 * MIB, 500, 10);
            w.publish(8 * MIB, p);
        }
        w.parsed(0, 20 * MIB);
        w.parsed(2, 25 * MIB);
        assert!(charged() - base > (45 * MIB) as i64, "charged while it waits");
        // the leader's next step at the gate returns the statement's refusal: the body returns its error
        drop(w);
        assert_eq!(charged(), base, "the window's charge is given back whole");
    }

    #[test]
    fn helper_default_is_a_third_of_the_pool() {
        assert_eq!(helpers_for(None, 16), 6);
        assert_eq!(helpers_for(None, 4), 2);
        assert_eq!(helpers_for(None, 48), 16);
        assert_eq!(helpers_for(Some(3), 16), 3);
        assert_eq!(helpers_for(Some(0), 16), 1);
        assert_eq!(window(6), 16);
    }

    /// D3350: a helper's structured error unwound (`Box<PgError>` or `PgError`, as a memory
    /// limit raises it) is the load's error with its SQLSTATE and message, never "parallel COPY
    /// helper panicked"; any other payload comes back for the thread top.
    #[test]
    fn a_helpers_structured_unwind_keeps_its_sqlstate_and_message() {
        type Payload = Box<dyn std::any::Any + Send>;
        let oom = || {
            PgError::new(ERROR, "out of memory")
                .with_sqlstate(ERRCODE_OUT_OF_MEMORY)
                .with_detail("Statement memory limit exceeded")
        };
        let boxed: Payload = Box::new(Box::new(oom()));
        let plain: Payload = Box::new(oom());
        for payload in [boxed, plain] {
            let e = unwound(payload).expect("a structured error is the load's error");
            assert_eq!(e.sqlstate(), ERRCODE_OUT_OF_MEMORY);
            assert_eq!(e.message(), "out of memory");
        }
        let panic: Payload = Box::new("a bug's panic");
        let back = unwound(panic).expect_err("a plain panic goes on");
        assert_eq!(back.downcast_ref::<&str>(), Some(&"a bug's panic"));
        assert_eq!(gone(&*back), None, "a plain panic is named a panic");
        let retired: Payload = Box::new(parallel::standing::PoolRetireRaw);
        let back = unwound(retired).expect_err("an exit goes on");
        assert_eq!(gone(&*back), Some("lost connection to parallel worker"));
    }
}
