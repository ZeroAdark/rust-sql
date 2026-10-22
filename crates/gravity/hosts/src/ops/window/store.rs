//! M10's stored rows (window.agg): the window object's row store behind `m10_win_put` / `m10_win_argerr` /
//! `m10_win_rec` (PG `spool_tuples`' tuplestore): fixed chunks of stored rows and their by-reference cells' copies, carved
//! in order from the store's segments (a row's address lives until its partition is released, the fill's next open or the
//! execution's end), each cell whose evaluation raised kept with its error
//! (null byte 2) until the object reads it, each lazy cell (`CELL_LAZY`, D1232) evaluated at every read (m10_lazy.rs);
//! and the result cells the IR reads. D3345: a new partition's first row releases every segment wholly below it (PG's
//! release_partition), and past work_mem of heap segments the next ones are temp files mapped shared (PG's tuplestore
//! spills past work_mem). Split from m10.rs. Created by M10-WINDOW.
use datum::{Datum, NullableDatum};
use gravity_abi::limits::{MAPPED_ROAD_MAX_BYTES, MAPPED_ROAD_MIN_BYTES};
use gravity_abi::status::raise;
use types_error::PgError;
use types_error::PgResult;

use std::collections::{HashMap, VecDeque};

use mcx::MemoryContext;
use memledger::charge::{Charge, Door, Owner, Release};
use memledger::Kind;

use crate::ops::window::object::{abi, bad, Rt, CHUNK_ROWS};

// D3345: a heap segment's bytes (the store's heap bytes halved, within these bounds); a streaming fill's largest pull
use gravity_program::cost::{WINDOW_PULL_MAX_ROWS as PULL_MAX, WINDOW_SEG_MAX_BYTES as SEG_MAX, WINDOW_SEG_MIN_BYTES as SEG_MIN};

/// The stored rows, apart from the object's walk over them (`Rt`): the fill writes them (`m10_win_put` /
/// `m10_win_argerr` / the fill's `m10_win_open`) while the object's call that pulled it is still running (the exact
/// request: a pulled fill runs inside the object's spool), so they live in their own cell.
pub(crate) struct Store {
    /// CONVERT wave 2 (site 12): the heap segments' and the spare's bytes (`heap_bytes`) charged to the owner `own` gives at the
    /// first stored row (the breaker the window's fill pipeline runs for), `charged` of them committed: re-synced at every
    /// segment made, freed or kept (each >= SEG_MIN). A mapped segment is its `TempMap`'s own charge. Declared first, so
    /// dropped first (door 1: the bytes move to the freeing thread's account, where the segments' frees land next).
    charge: Charge,
    charged: u64,
    /// pagepool P3 (`PGRUST_POOL_GRAVITY=1`): the pool's segments and spare (`pool_bytes`) on the same owner, `Door::Pool`
    /// (`pool`: `own` found the switch on); `pool_charged` of them committed. Dropped before the segments, as `charge`.
    pool_charge: Charge,
    pool_charged: u64,
    pool: bool,
    /// `own` ran (with `None`: charged to no one; a unit test's store, a pipeline with no owner); `on`: with an owner.
    owned: bool,
    on: bool,
    /// Every stored cell's `(typlen, byval)` (the spec's).
    pub(crate) cols: memledger::ChargedVec<(i16, bool)>,
    /// D3584: per cell, whether the object can evaluate it at a read (`WinSpec::lazy_cells`): `put` keeps such a cell's
    /// wide value (`wide`) as `CELL_LAZY` instead of copying it.
    pub(crate) defer: memledger::ChargedVec<bool>,
    /// Words per stored row (`abi::record_bytes(ncols) / 8`).
    pub(crate) rw: usize,
    /// Each held chunk's first row: chunk number `base + i` at `i` (row `j` in chunk `j / CHUNK_ROWS`); a released
    /// segment's chunks are dropped from the front.
    chunks: VecDeque<*mut u64>, // alloc: Exec
    base: usize,
    pub(crate) nrows: u64,
    /// D3345: rows below it are released (their partition ended); a read of one is refused.
    pub(crate) released: u64,
    segs: VecDeque<Seg>, // alloc: Exec
    /// D3345: the live heap segments' bytes, the work_mem bound past which a new segment is a mapped temp file, and the
    /// bytes mapped so far (each new mapping as large, within the mapped road's bounds).
    heap: usize,
    /// pagepool P3: of `heap`, the pool's segments' bytes.
    pooled: usize,
    budget: usize,
    mapped: usize,
    /// A released or cleared heap segment kept for the next one.
    spare: Option<crate::ops::window::raw::Words>, // alloc: Exec
    /// The expanded objects' flat copies (the segments take every other by-reference cell), kept until the fill's next
    /// open or the execution's end.
    pub(crate) copies: MemoryContext,
    /// The errors `m10_win_argerr` marked for the next row, and every stored row's by (row, cell).
    pub(crate) pending: Vec<(u32, Box<PgError>)>, // alloc: Exec
    pub(crate) errs: HashMap<(u64, u32), Box<PgError>>, // alloc: Exec
    /// The rows a pull of the fill takes (the exact request): `put` answers 1 once `nrows` reaches it, the fill then
    /// stopping resumably after that row; `u64::MAX` (an eager fill): never.
    pub(crate) want: u64,
}

/// D3345: one block the store carves in order: chunks and by-reference copies. `last` is the last row whose words or
/// copy live in it, `chunks` the chunk numbers laid in it: it is released once every row it holds is.
struct Seg {
    mem: SegMem,
    at: usize,
    end: usize,
    last: u64,
    chunks: core::ops::Range<usize>,
}

enum SegMem {
    Heap(crate::ops::window::raw::Words), // alloc: Exec
    Mapped(gravity_driver::TempMap),
}

impl Store {
    pub(crate) fn new(cols: memledger::ChargedVec<(i16, bool)>, defer: memledger::ChargedVec<bool>, copies: MemoryContext) -> Store {
        let budget = (crate::inputs::work_mem_kb().max(64) as usize) << 10;
        Store { charge: Charge::NONE, charged: 0, pool_charge: Charge::NONE, pool_charged: 0, pool: false, owned: false, on: false, rw: abi::record_bytes(cols.len()) / 8, cols, defer, chunks: VecDeque::new(), base: 0, nrows: 0, released: 0, segs: VecDeque::new(), heap: 0, pooled: 0, budget, mapped: 0, spare: None, copies, pending: Vec::new(), errs: HashMap::new(), want: u64::MAX } // alloc: Exec
    }

    /// Whether the store still waits for its owner (`own`): the first `m10_win_put`'s caller reads it, so the lookup is once.
    pub(crate) fn needs_owner(&self) -> bool {
        !self.owned
    }

    /// The owner the heap segments are charged to from now (`None`: no one), with what the store holds already.
    pub(crate) fn own(&mut self, owner: Option<(Owner, Kind)>) {
        self.owned = true;
        if let Some((o, k)) = owner {
            (self.charge, self.charged, self.on) = (Charge::open(o, Release::Statement, k, Door::Heap, 0), 0, true);
            if crate::ops::window::raw::pooled() {
                (self.pool_charge, self.pool_charged, self.pool) = (Charge::open(o, Release::Statement, k, Door::Pool, 0), 0, true);
            }
            self.sync();
        }
    }

    /// The heap bytes the store holds: the live heap segments and the spare.
    fn heap_bytes(&self) -> u64 {
        (self.heap + self.spare.as_ref().map_or(0, |b| b.len() * 8)) as u64
    }

    /// pagepool P3: of `heap_bytes`, the pool's.
    fn pool_bytes(&self) -> u64 {
        (self.pooled + self.spare.as_ref().filter(|b| b.pooled()).map_or(0, |b| b.len() * 8)) as u64
    }

    /// The charge re-synced to `heap_bytes`: a commit of a segment just allocated (on this thread: the allocator counted it
    /// here), a free of the segments just dropped (on this thread too). Nothing without an owner.
    fn sync(&mut self) {
        if !self.on {
            return;
        }
        let q = self.pool_bytes();
        let h = self.heap_bytes() - q;
        if h > self.charged {
            self.charge.commit(h - self.charged);
        } else if h < self.charged {
            self.charge.free(self.charged - h);
        }
        self.charged = h;
        // pagepool P3: no door counted the pool's segments on this thread: on the owner as they come, off as they go
        if q > self.pool_charged {
            self.pool_charge.commit(q - self.pool_charged);
        } else if q < self.pool_charged {
            self.pool_charge.give(self.pool_charged - q);
        }
        self.pool_charged = q;
    }

    /// The stored rows dropped (the fill's open, `m10_win_open`: its input runs again): their segments (a heap one kept
    /// for the next fill), copies and kept errors freed.
    pub(crate) fn clear(&mut self) {
        self.nrows = 0;
        self.pending.clear();
        self.errs.clear();
        self.copies.reset();
        self.release_segs(u64::MAX);
        self.chunks.clear();
        (self.base, self.released) = (0, 0);
    }

    /// D3345 (PG release_partition): rows below `j` are never read again (the partition starting at stored row `j`
    /// began): every segment holding only such rows is freed, their kept errors too.
    pub(crate) fn release_before(&mut self, j: u64) {
        if j <= self.released {
            return;
        }
        self.released = j;
        if !self.errs.is_empty() {
            self.errs.retain(|&(r, _), _| r >= j);
        }
        self.release_segs(j);
    }

    fn release_segs(&mut self, j: u64) {
        // most calls free nothing (a partition that starts inside the front segment): they open no step
        if !self.segs.front().is_some_and(|s| s.last < j) {
            return;
        }
        // a label-only step (no tick): a long release is checker 2's under its own label
        let _step = cancelpoll::step("window store release_segs: the segments of released rows freed");
        // bounded("other: each pass only frees one segment of the rows a partition's start or the fill's reopen leaves behind (a heap block dropped or kept as the spare, a mapped file unmapped; each SEG_MIN, 64 KiB, of stored rows or more) and reads no row; a release runs to its end (a stop would keep freed rows' segments), as PG's tuplestore_trim frees the same rows with no poll")
        while self.segs.front().is_some_and(|s| s.last < j) {
            let Some(s) = self.segs.pop_front() else { break };
            // the segments before it are gone, so its chunks and every one before them are the front's
            // bounded("other: each pass only pops one chunk number (CHUNK_ROWS, 256, rows a chunk) laid in the segment being freed and reads no row; a release runs to its end, as PG's tuplestore_trim frees the same rows with no poll")
            while self.base < s.chunks.end && self.chunks.pop_front().is_some() {
                self.base += 1;
            }
            if let SegMem::Heap(b) = s.mem {
                self.heap -= b.len() * 8;
                if b.pooled() {
                    self.pooled -= b.len() * 8;
                }
                if self.spare.as_ref().is_none_or(|k| k.len() < b.len()) {
                    self.spare = Some(b);
                }
            }
        }
        self.sync();
    }

    /// D3345: `bytes` (rounded up to 8) carved from the newest segment for row `last`'s words or copy, a new segment when
    /// it has no room: on the heap while the heap segments hold at most work_mem, else a temp file mapped shared
    /// (`gravity_driver::map_temp`: temp_file_limit charged, 53400 past it), the heap when the session has no file road.
    pub(crate) fn carve(&mut self, bytes: usize, last: u64) -> PgResult<*mut u8> {
        let bytes = bytes.next_multiple_of(8);
        if let Some(s) = self.segs.back_mut().filter(|s| s.end - s.at >= bytes) {
            let p = s.at;
            (s.at, s.last) = (p + bytes, s.last.max(last));
            return Ok(p as *mut u8);
        }
        let want = bytes.max((self.heap / 2).clamp(SEG_MIN, SEG_MAX));
        let map = if self.heap + want > self.budget {
            let len = bytes.max(self.mapped.clamp(MAPPED_ROAD_MIN_BYTES, MAPPED_ROAD_MAX_BYTES));
            match gravity_driver::map_temp(len) {
                Ok(m) => m,
                Err(_) => return Err(crate::take_error().unwrap_or_else(|| bad("a temp file map for the window's stored rows failed"))),
            }
        } else {
            None
        };
        let mem = match map {
            Some(m) => {
                self.mapped += m.len();
                SegMem::Mapped(m)
            }
            None => {
                let b = match self.spare.take() {
                    Some(b) if b.len() * 8 >= want => b,
                    old => {
                        drop(old);
                        crate::ops::window::raw::zeroed_words(want / 8, self.pool)?
                    }
                };
                self.heap += b.len() * 8;
                if b.pooled() {
                    self.pooled += b.len() * 8;
                }
                self.sync();
                SegMem::Heap(b)
            }
        };
        let (at, len) = match &mem {
            SegMem::Heap(b) => (b.as_ptr() as usize, b.len() * 8),
            SegMem::Mapped(m) => (m.as_mut_ptr() as usize, m.len()),
        };
        let c = self.base + self.chunks.len();
        self.segs.push_back(Seg { mem, at: at + bytes, end: at + len, last, chunks: c..c }); // alloc: Exec
        Ok(at as *mut u8)
    }

    /// Row `n`'s chunk laid (`n` its first row): carved for its last row, its number noted in the segment.
    pub(crate) fn lay_chunk(&mut self, n: u64) -> PgResult<()> {
        let p = self.carve(CHUNK_ROWS * self.rw * 8, n + CHUNK_ROWS as u64 - 1)?;
        self.chunks.push_back(p as *mut u64); // alloc: Exec
        let end = self.base + self.chunks.len();
        if let Some(s) = self.segs.back_mut() {
            s.chunks = if s.chunks.is_empty() { end - 1..end } else { s.chunks.start..end };
        }
        Ok(())
    }

    /// Cell `col` of the next row raised: PG's error for the lowest code of `mask` (the host's stashed error for `HOST`).
    pub(crate) fn argerr(&mut self, col: u64, mask: u64) -> PgResult<i64> {
        if col as usize >= self.cols.len() || mask == 0 {
            return Err(bad("m10_win_argerr: no such cell, or no error"));
        }
        let code = raise::first(mask);
        let e = match raise::describe(code) {
            Some((state, msg)) => {
                drop(crate::take_any_error());
                Box::new(PgError::error(msg.to_string()).with_sqlstate(types_error::make_sqlstate(*state))) // alloc: Cold
            }
            None => crate::take_any_error().unwrap_or_else(|| bad(&format!("window.agg: an argument's error without a message (raise code {code})"))), // alloc: Cold
        };
        self.pending.push((col as u32, e)); // alloc: Exec
        Ok(0)
    }

    /// Stored row `j`'s address.
    pub(crate) fn rec(&self, j: u64) -> PgResult<*mut u64> {
        if j >= self.nrows {
            return Err(bad("m10_win_rec: a row past the stored rows"));
        }
        self.held(j)?;
        Ok(self.at(j))
    }

    /// D3345: row `j` is not released. No PostgreSQL plan reaches a released row (a walk never reads back past its
    /// partition's start, as nodeWindowAgg's positions after release_partition): a debug build asserts it, a release build
    /// refuses by name rather than read freed memory.
    pub(crate) fn held(&self, j: u64) -> PgResult<()> {
        debug_assert!(j >= self.released, "D3345: stored row {j} read after its partition was released (rows below {} are)", self.released);
        if j < self.released {
            return Err(bad("window.agg: a stored row read after its partition was released"));
        }
        Ok(())
    }

    pub(crate) fn at(&self, j: u64) -> *mut u64 {
        let (c, r) = (j as usize / CHUNK_ROWS, j as usize % CHUNK_ROWS);
        self.chunks[c - self.base].wrapping_add(r * self.rw)
    }

}

impl Rt {
    /// The exact request (PLAN-v5 §2.5; W4): one more row from a pulled fill (the store wants one past its rows, the fill
    /// stops after storing it); false once the fill ended with no new row. An eager fill has ended. W10: a fill that
    /// streams (`WinSpec::grow`) stores `budget` rows per pull, the budget doubling after each pull (1, 2, 4, ..): a consumer
    /// that stops leaves under twice the rows PG's spool read, a whole read costs one pull per doubling. A row's error that
    /// pull raised past the row the spool wanted (`holdable`) is held: the rows stored before it are answered, the next
    /// pull raises it, as PG's spool raises it only when a row needs the row that raised.
    pub(crate) fn more(&mut self) -> PgResult<bool> {
        if let Some(e) = self.held.take() {
            return Err(e);
        }
        if self.ended {
            return Ok(false);
        }
        let budget = self.budget;
        if self.spec.grow {
            // D3345: the read-ahead stops doubling at `WINDOW_PULL_MAX_ROWS`, so the rows stored ahead of the walk stay
            // bounded (a whole read pays a pull per that many rows past the doublings, ~0.25 us each)
            self.budget = budget.saturating_mul(2).min(PULL_MAX);
        }
        let before = {
            let mut s = self.store.borrow_mut();
            s.want = s.nrows.saturating_add(budget);
            s.nrows
        };
        let r = match self.spec.pull.as_mut() {
            Some(pull) => pull(),
            None => Ok(false),
        };
        let mut s = self.store.borrow_mut();
        s.want = u64::MAX;
        let stopped = match r {
            Ok(stopped) => stopped,
            // the fill raised after storing the row the spool wanted, reading ahead: the pulled set is over (failed)
            Err(e) if self.spec.grow && s.nrows > before && holdable(&e) => {
                (self.held, self.ended) = (Some(e), true);
                return Ok(true);
            }
            Err(e) => return Err(e),
        };
        self.ended |= !stopped;
        Ok(s.nrows > before)
    }

    /// `m10_win_ask(j)`: the fill pulled until row `j` is stored or the fill ended; the stored rows, 0 when `j` is past them.
    pub(crate) fn ask(&mut self, j: u64) -> PgResult<i64> {
        // bounded("other: each pass is one pull of the fill (`more`): a kernel run through the runner's nested pull (driver `Entry::pull_nested`, which reads the statement's stop), and it stores a row or more or ends the loop")
        while j >= self.store.borrow().nrows {
            if !self.more()? {
                break;
            }
        }
        let n = self.store.borrow().nrows;
        Ok(if j < n { n as i64 } else { 0 })
    }

    /// Stored row `r`'s cell `c`: its value, or the error its evaluation raised (taken: the statement ends with it).
    pub(crate) fn cell(&mut self, r: u64, c: usize) -> PgResult<NullableDatum> {
        let at = {
            let s = self.store.borrow();
            s.held(r)?;
            s.at(r)
        };
        // `r` is a stored, held row (the positions stay inside the partition) and `c` one of its cells
        let (v, nb) = crate::ops::window::raw::cell_at(at, self.spec.cols.len(), c);
        match nb {
            0 => Ok(NullableDatum { value: Datum::from_u64(v), isnull: false }),
            1 => Ok(NullableDatum::null()),
            abi::CELL_LAZY => self.eval_lazy(r, c),
            _ => Err(self.store.borrow_mut().errs.remove(&(r, c as u32)).unwrap_or_else(|| bad("window.agg: a failed cell read twice"))),
        }
    }

    /// Function `i`'s result cell.
    pub(crate) fn set_res(&mut self, i: usize, v: NullableDatum) {
        let nf = self.spec.funcs.len();
        self.res[i] = if v.isnull { 0 } else { v.value.as_u64() };
        // byte `i` of the validity words, `i < nf`
        crate::ops::window::raw::set_valid(&mut self.res[nf..], i, !v.isnull as u8);
    }
}

/// W10: an error PostgreSQL raises at the row whose code raised it, which a budgeted pull reads ahead of the spool: not a
/// cancel, a resource, system or internal error, nor a refusal, which the statement raises wherever it meets them (exec
/// setop/generation.rs `holdable`'s rule).
fn holdable(e: &PgError) -> bool {
    use types_error::{errcode_to_category as class, ERRCODE_FEATURE_NOT_SUPPORTED, ERRCODE_INSUFFICIENT_RESOURCES, ERRCODE_INTERNAL_ERROR, ERRCODE_QUERY_CANCELED, ERRCODE_SYSTEM_ERROR};
    let c = class(e.sqlstate());
    e.level() == types_error::ERROR
        && ![ERRCODE_QUERY_CANCELED, ERRCODE_SYSTEM_ERROR, ERRCODE_INSUFFICIENT_RESOURCES, ERRCODE_INTERNAL_ERROR].iter().any(|&k| class(k) == c)
        && !(e.sqlstate() == ERRCODE_FEATURE_NOT_SUPPORTED && e.message().starts_with("gravity: refused ("))
}

#[cfg(test)]
impl Store {
    /// The store's live heap bytes, first held chunk number, held chunks and segments (raw.rs `store_tests`).
    pub(crate) fn census(&self) -> (usize, usize, usize, usize) {
        (self.heap, self.base, self.chunks.len(), self.segs.len())
    }

    /// The heap bytes held (segments + spare) and the bytes the charge has committed (raw.rs `store_tests`).
    #[cfg(test)]
    pub(crate) fn charge_census(&self) -> (u64, u64) {
        (self.heap_bytes(), self.charged + self.pool_charged)
    }
}
