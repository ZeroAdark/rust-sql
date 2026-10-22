// gravity_driver_rig's root: a native integration-test rig that reads the program ABI's words as raw pointers. The
// site's wasm gate checks every workspace member for wasm32, where the rig is empty.
#![cfg(not(target_family = "wasm"))]
#![allow(dead_code)]

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use gravity_abi::ctx::{cancel_pending, hdr, lane, site_ptr, RESUME_NONE};
use gravity_abi::program::{SPILL_PARTS, SPILL_REFILL, SPILL_SCATTER, WRITE_UPDATE};
use gravity_abi::site::{next_row, seek as site_seek, site};
use gravity_abi::status::{recheck, suspend, unsupported, CANCELLED, DONE, ERROR, OK, STOP, UNSUPPORTED_SCRATCH, WRITE_DONE, WRITE_RECHECK, WRITE_SKIPPED};
use gravity_abi::{Ctx, Datum, Lane, ScanPos, Site};
use gravity_driver::{box_locker, box_source, box_writer, ArenaBox, ArenaRef, SourceBox, Binding, BindingParts, BindingRef, BorrowedBatch, CommandId, Cursor, Derive, Error, Flow, Identity, Locker, Merge, Outcome, Partial, PerTuple, Pins, Result, LegRef, Sink, SinkLeg, Snapshot, Source, State, Window, WindowBase, WriteOp, Writer};
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, HashAgg, KeyKind, RecordLayout, SlotKind, Spill, SHARE};
use gravity_program::{Arena, Cmd, Code, End, IndexPlan, Layout, LockKind, OutCol, Pipeline, Program, Reason, SiteDesc, WaitPolicy, WritePlan};

fn stage() -> Arena {
    Arena::new(1 << 14)
}

/// The fixture count kernels' record: `count`, `sum` (a one-record table).
pub const COUNT_SUM: RecordLayout = RecordLayout::new(&[], &[SlotKind::Count, SlotKind::SumI64]);
pub const COUNT_SUM_BYTES: u32 = HashAgg::bytes(&COUNT_SUM, 1);

/// The fixture's one record at `p` (`HashAgg::single` over `COUNT_SUM`).
/// # Safety
/// `p` points at `COUNT_SUM_BYTES` zeroed or live bytes.
pub unsafe fn count_sum<'a>(p: *mut u8) -> gravity_abi::breaker::Rec<'a> {
    HashAgg::at(p, &COUNT_SUM, 1, COUNT_SUM_BYTES).single()
}

pub use gravity_abi::program::{consumed, partial as own};

pub const MAXR: usize = 24;
pub const MAXC: usize = 8;

/// Column-major rows: `cols[k]` is lane `k`, `valid[k]` its validity word.
#[derive(Clone, Copy)]
pub struct FixedRows {
    pub n: usize,
    pub cols: [[Datum; MAXR]; MAXC],
    pub valid: [u64; MAXC],
}

impl FixedRows {
    pub const fn empty() -> FixedRows {
        FixedRows { n: 0, cols: [[0; MAXR]; MAXC], valid: [0; MAXC] }
    }

    pub fn push(&mut self, v: &[Datum]) {
        for (k, x) in v.iter().enumerate() {
            self.cols[k][self.n] = *x;
        }
        for w in self.valid.iter_mut() {
            *w |= 1 << self.n;
        }
        self.n += 1;
    }

    pub fn row(&self, i: usize, natts: usize) -> Vec<Datum> {
        (0..natts).map(|k| self.cols[k][i]).collect()
    }
}

pub type Fill = fn(&[Datum], &mut FixedRows);

pub fn no_rows(_: &[Datum], _: &mut FixedRows) {}

pub fn customer(keys: &[Datum], out: &mut FixedRows) {
    if keys == [1, 2, 3] {
        out.push(&[0xF1, 0x1A57, 500]);
    }
}

pub fn by_last_name(keys: &[Datum], out: &mut FixedRows) {
    for i in 0..5 {
        out.push(&[keys[3] * 10 + i, 100 + i]);
    }
}

pub fn ten_rows(keys: &[Datum], out: &mut FixedRows) {
    for i in 0..10 {
        out.push(&[keys[3] * 10 + i, 200 + i]);
    }
}

pub fn twenty_rows(keys: &[Datum], out: &mut FixedRows) {
    for i in 0..20 {
        out.push(&[keys[3] * 100 + i, 300 + i]);
    }
}

pub fn five_stock(keys: &[Datum], out: &mut FixedRows) {
    for i in 0..5 {
        out.push(&[keys[1] + i]);
    }
}

pub fn stock(keys: &[Datum], out: &mut FixedRows) {
    out.push(&[0x71D, keys[1] + 40]);
}

pub fn orders(keys: &[Datum], out: &mut FixedRows) {
    if keys[2] == 999 {
        return;
    }
    out.push(&[keys[2], 20260912, 7]);
    out.push(&[keys[2] + 1, 20260913, 8]);
}

pub fn order_lines(keys: &[Datum], out: &mut FixedRows) {
    if keys[2] == 999 {
        return;
    }
    for n in 1..=3 {
        out.push(&[n, keys[2] * 100 + n, n * 5]);
    }
}

pub struct FakeSource {
    fill: Fill,
    rows: FixedRows,
    cursor: usize,
    cur: Cursor,
    natts: usize,
    rel: u32,
    substituted: Option<ScanPos>,
    pub bind_fail: bool,
    pub fail_next_after: Option<usize>,
    pub nexts: usize,
    pub bound: bool,
    /// D4602: what its bind took (a failed one's too), until its unbind gives it back.
    pub held: bool,
    pub parked: bool,
    pub snapshot: Snapshot,
    pub identity: Identity,
    pub stats: Option<Rc<Stats>>,
}

#[derive(Default)]
pub struct Stats {
    pub binds: Cell<u64>,
    pub unbinds: Cell<u64>,
    /// D4602: the sources holding what a bind took (a failed bind's included), given back at their unbind.
    pub held: Cell<u64>,
    pub parks: Cell<u64>,
    pub unparks: Cell<u64>,
    pub seeks: Cell<u64>,
    pub writes: Cell<u64>,
    pub locks: Cell<u64>,
    pub last_write: Cell<(u8, ScanPos, [Datum; MAXC])>,
    pub last_lock: Cell<ScanPos>,
}

impl Source for FakeSource {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()> {
        // D4602: the bind takes its state before the step that may fail (D4599: a bitmap source's recheck qual, then a
        // refusal of its heap)
        self.snapshot = b.snapshot;
        if !self.held {
            self.held = true;
            if let Some(s) = &self.stats {
                s.held.set(s.held.get() + 1);
            }
        }
        if self.bind_fail {
            return Err(Error::host("bind refused"));
        }
        self.bound = true;
        self.identity = b.identity;
        if let Some(s) = &self.stats {
            s.binds.set(s.binds.get() + 1);
        }
        Ok(())
    }

    fn seek(&mut self, keys: &[Datum], _keys_null: &[u8]) -> Result<()> {
        assert!(self.bound, "seek on an unbound source");
        self.rows.n = 0;
        match self.substituted.take() {
            // the "newest version at pos": a row spelled from the position
            Some(pos) => self.rows.push(&[pos.block as u64, pos.off as u64, pos.rel as u64]),
            None => (self.fill)(keys, &mut self.rows),
        }
        self.cursor = 0;
        self.cur.clear();
        if let Some(s) = &self.stats {
            s.seeks.set(s.seeks.get() + 1);
        }
        Ok(())
    }

    /// One row per window (so `fail_next_after` counts rows as before).
    fn window(&mut self, w: &mut Window<'_>) -> Result<u32> {
        assert!(self.bound, "window on an unbound source");
        self.nexts += 1;
        if self.fail_next_after.is_some_and(|n| self.nexts > n) {
            return Err(Error::host("index read failed"));
        }
        if self.cursor >= self.rows.n {
            return Ok(0);
        }
        let i = self.cursor;
        for k in 0..self.natts {
            w.set(k, Lane::byval(self.rows.cols[k][i..].as_ptr() as *const u8, core::ptr::null(), 8, lane::REGION_ARENA, lane::id(lane::ID_ATTR, k as u32)));
        }
        w.rows = 1;
        w.set_base(WindowBase::at(ScanPos::new(self.rel, i as u32 + 1, 0)));
        self.cursor += 1;
        Ok(1)
    }

    fn lanes_hint(&self) -> usize {
        self.natts
    }

    fn cursor(&self) -> &Cursor {
        &self.cur
    }

    fn cursor_mut(&mut self) -> &mut Cursor {
        &mut self.cur
    }

    fn refetch(&mut self, values: &mut [Datum], isnull: &mut [u8]) -> Result<bool> {
        let served = self.cur.pos.block as usize;
        if served == 0 {
            return Ok(false);
        }
        values.copy_from_slice(&self.rows.row(served - 1, self.natts));
        isnull.fill(0);
        Ok(true)
    }

    fn substitute(&mut self, pos: ScanPos) {
        self.substituted = Some(pos);
    }

    fn unbind(&mut self) {
        self.bound = false;
        self.nexts = 0;
        self.rows.n = 0;
        self.cursor = 0;
        self.cur.clear();
        if let Some(s) = &self.stats {
            s.unbinds.set(s.unbinds.get() + 1);
            if self.held {
                s.held.set(s.held.get() - 1);
            }
        }
        self.held = false;
    }

    fn park(&mut self) {
        self.parked = true;
        if let Some(s) = &self.stats {
            s.parks.set(s.parks.get() + 1);
        }
    }

    fn unpark(&mut self) {
        self.parked = false;
        if let Some(s) = &self.stats {
            s.unparks.set(s.unparks.get() + 1);
        }
    }
}

pub struct FakeWriter {
    pub outcome: Outcome,
    pub bound: bool,
    pub stats: Option<Rc<Stats>>,
}

impl Writer for FakeWriter {
    fn bind(&mut self, _b: &BindingRef<'_>) -> Result<()> {
        self.bound = true;
        Ok(())
    }

    fn write(&mut self, op: WriteOp, pos: ScanPos, values: &[Datum], _isnull: &[u8]) -> Result<Outcome> {
        assert!(self.bound, "write on an unbound writer");
        if let Some(s) = &self.stats {
            s.writes.set(s.writes.get() + 1);
            let mut v = [0; MAXC];
            v[..values.len()].copy_from_slice(values);
            s.last_write.set((op as u8, pos, v));
        }
        Ok(self.outcome)
    }

    fn unbind(&mut self) {
        self.bound = false;
    }
}

pub struct FakeLocker {
    pub outcomes: [Outcome; 4],
    pub calls: usize,
    pub bound: bool,
    pub stats: Option<Rc<Stats>>,
}

impl Locker for FakeLocker {
    fn bind(&mut self, _b: &BindingRef<'_>) -> Result<()> {
        self.bound = true;
        Ok(())
    }

    fn lock(&mut self, pos: ScanPos) -> Result<Outcome> {
        assert!(self.bound, "lock on an unbound locker");
        let o = self.outcomes[self.calls.min(3)];
        self.calls += 1;
        if let Some(s) = &self.stats {
            s.locks.set(s.locks.get() + 1);
            s.last_lock.set(pos);
        }
        Ok(o)
    }

    fn unbind(&mut self) {
        self.bound = false;
        self.calls = 0;
    }
}

pub struct FakeSink {
    rows: Cell<usize>,
    batches: Cell<usize>,
    last: Cell<[Datum; MAXC]>,
    pub stop_after: Cell<Option<usize>>,
    pub fail_after: Cell<Option<usize>>,
}

impl FakeSink {
    pub fn new() -> FakeSink {
        FakeSink { rows: Cell::new(0), batches: Cell::new(0), last: Cell::new([0; MAXC]), stop_after: Cell::new(None), fail_after: Cell::new(None) }
    }

    pub fn rows(&self) -> usize {
        self.rows.get()
    }

    pub fn batches(&self) -> usize {
        self.batches.get()
    }

    pub fn last(&self) -> [Datum; MAXC] {
        self.last.get()
    }
}

impl Sink for &FakeSink {
    fn push(&mut self, batch: BorrowedBatch<'_>) -> Result<Flow> {
        let s = *self;
        s.batches.set(s.batches.get() + 1);
        for (values, _) in batch.iter() {
            if s.fail_after.get().is_some_and(|n| s.rows.get() >= n) {
                return Err(Error::host("sink full"));
            }
            s.rows.set(s.rows.get() + 1);
            let mut last = s.last.get();
            last[..values.len()].copy_from_slice(values);
            s.last.set(last);
        }
        Ok(if s.stop_after.get().is_some_and(|n| s.rows.get() >= n) { Flow::Stop } else { Flow::Continue })
    }
}

pub struct RetainingSink {
    pub retained: *const Datum,
    pub first_seen: Datum,
}

impl Sink for RetainingSink {
    fn push(&mut self, batch: BorrowedBatch<'_>) -> Result<Flow> {
        self.retained = batch.values.as_ptr();
        self.first_seen = batch.values[0];
        Ok(Flow::Continue)
    }
}

pub struct RangeTable {
    pub blocks: u64,
    pub per_block: u64,
    pub pins: AtomicI64,
    pub fail_block: AtomicU64,
    pub leak: AtomicBool,
    pub binds: AtomicU64,
    /// D3196: the sources bound now (a bind adds one, the unbind of a bound source takes it back), and the binds of a source
    /// still bound (a bind with no unbind between: what a warm slot's open must never do).
    pub bound_now: AtomicI64,
    pub rebinds: AtomicU64,
    /// D3196: the re-arms of a bound source (a warm slot's next section, `Source::rearm`); one of an unbound source is a bug.
    pub rearms: AtomicU64,
    /// D3196: the parameter image's length a bind saw: the leader's last (`leader_image`), and the worker slots' binds that saw
    /// one at all (`worker_images`: a worker slot binds without it, so a warm slot is what a fresh bind would make).
    pub leader_image: AtomicU64,
    pub worker_images: AtomicU64,
    /// Pins released on a thread other than the one that took them (a buffer pin is its backend thread's: bufmgr's private
    /// refcount answers "buffer is not pinned", D227).
    pub foreign_unpins: AtomicU64,
    /// D1507: a source keeps a handle on the table across its unbind (as a granule source keeps its cursors), given back
    /// only by `release_rel`: the table's `Arc` count shows what parked slots still hold.
    pub keep: AtomicBool,
    /// What a keeping source holds (its own `table` handle is counted apart): `Arc::strong_count(&token) - 1` handles kept.
    pub token: Arc<()>,
    /// D1699: each source's drop asks the process-wide slot depot for its stats from another thread and waits up to
    /// `PROBE_WAIT`: `drops_free` counts the drops that got an answer, `drops_locked` those that ran while the depot's lock
    /// was held (the lock is a leaf: nothing drops under it).
    pub drop_probe: AtomicBool,
    pub drops_free: AtomicU64,
    pub drops_locked: AtomicU64,
    /// D3213: what each source reports as kept between statements (`Source::kept_bytes`).
    pub kept_bytes: AtomicUsize,
    /// CONVERT wave 2: with `record_owners` set, each window's `Window::op_owner` and `Window::stmt_owner` as words (the
    /// owner's `words()`, then its kind's or release's `u8`), and whether the window ran on a worker slot's binding.
    pub record_owners: AtomicBool,
    pub owners: Mutex<Vec<SeenOwners>>,
    /// RESIDUE (slot DEPOT holder): with `held_heap` > 0 each source keeps a heap buffer of that many bytes from its first
    /// bind, charged (door 1) to the owner its bind ran under, and moves the charge where `Source::transfer_held` says.
    pub held_heap: AtomicUsize,
    /// D4554: with `shed_on_park` set a source's `Source::park` frees its `held_heap` buffer and its charge (as a granule
    /// source frees its bind-sized buffers, D4464), and its next bind makes it again: `parks` counts the parks, `heaps` the
    /// buffers the worker binds made (a leader's source is the statement's own, never parked).
    pub shed_on_park: AtomicBool,
    pub parks: AtomicU64,
    pub heaps: AtomicU64,
    /// D4630 (b): with `at_claim` set a source allows a worker slot's bind at its first claim (`Source::binds_at_claim`);
    /// `bind_threads` holds the thread of each worker slot's bind.
    pub at_claim: AtomicBool,
    pub bind_threads: Mutex<Vec<std::thread::ThreadId>>,
}

/// CONVERT wave 2: one window's owner handles (`RangeTable::owners`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeenOwners {
    pub worker: bool,
    pub op: Option<([u64; 2], u8)>,
    pub stmt: Option<([u64; 2], u8)>,
}

/// D1699: how long a probing source's drop waits for the depot's lock before it counts the drop as under it.
pub const PROBE_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

impl RangeTable {
    pub fn new(blocks: u64, per_block: u64) -> Arc<RangeTable> {
        Arc::new(RangeTable { blocks, per_block, pins: AtomicI64::new(0), fail_block: AtomicU64::new(u64::MAX), leak: AtomicBool::new(false), binds: AtomicU64::new(0), bound_now: AtomicI64::new(0), rebinds: AtomicU64::new(0), rearms: AtomicU64::new(0), leader_image: AtomicU64::new(0), worker_images: AtomicU64::new(0), foreign_unpins: AtomicU64::new(0), keep: AtomicBool::new(false), token: Arc::new(()), drop_probe: AtomicBool::new(false), drops_free: AtomicU64::new(0), drops_locked: AtomicU64::new(0), kept_bytes: AtomicUsize::new(0), record_owners: AtomicBool::new(false), owners: Mutex::new(Vec::new()), held_heap: AtomicUsize::new(0), shed_on_park: AtomicBool::new(false), parks: AtomicU64::new(0), heaps: AtomicU64::new(0), at_claim: AtomicBool::new(false), bind_threads: Mutex::new(Vec::new()) })
    }

    pub fn rows(&self) -> u64 {
        self.blocks * self.per_block
    }

    pub fn count_below(&self, thr: u64) -> u64 {
        self.rows().min(thr)
    }
}

pub const MAX_PER_BLOCK: usize = 64;

pub struct FakeRangeSource {
    table: Arc<RangeTable>,
    lo: u64,
    hi: u64,
    block: u64,
    pinned: bool,
    pin_thread: Option<std::thread::ThreadId>,
    natts: usize,
    rel: u32,
    bound: bool,
    /// The binding's `ordinal` (`Some` on a worker slot's).
    worker: bool,
    cur: Cursor,
    lanes: [[Datum; MAX_PER_BLOCK]; 2],
    kept: Option<Arc<()>>,
    /// RESIDUE: the charge on `heap` (`RangeTable::held_heap`), declared first: it drops before the buffer frees.
    held: Option<gravity_driver::mem::budget::Charge>,
    heap: Vec<u8>,
}

impl Drop for FakeRangeSource {
    fn drop(&mut self) {
        if !self.table.drop_probe.load(Ordering::Relaxed) {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(gravity_driver::DEPOT.stats());
        });
        let counter = if rx.recv_timeout(PROBE_WAIT).is_ok() { &self.table.drops_free } else { &self.table.drops_locked };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

impl FakeRangeSource {
    fn unpin(&mut self) {
        if self.pinned && !self.table.leak.load(Ordering::Relaxed) {
            self.table.pins.fetch_sub(1, Ordering::Relaxed);
            self.pinned = false;
            if self.pin_thread.take() != Some(std::thread::current().id()) {
                self.table.foreign_unpins.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

impl Source for FakeRangeSource {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()> {
        match b.ordinal {
            None => self.table.leader_image.store(b.params.len() as u64, Ordering::Relaxed),
            Some(_) if !b.params.is_empty() => {
                self.table.worker_images.fetch_add(1, Ordering::Relaxed);
            }
            Some(_) => {}
        }
        if self.bound {
            self.table.rebinds.fetch_add(1, Ordering::Relaxed);
        } else {
            self.table.bound_now.fetch_add(1, Ordering::Relaxed);
        }
        self.bound = true;
        self.worker = b.ordinal.is_some();
        self.table.binds.fetch_add(1, Ordering::Relaxed);
        // only under `at_claim` (D4630's witness): the steady-state allocation tests bind worker slots every section
        if self.worker && self.table.at_claim.load(Ordering::Relaxed) {
            self.table.bind_threads.lock().unwrap().push(std::thread::current().id()); // alloc: Exec
        }
        let n = self.table.held_heap.load(Ordering::Relaxed);
        if n > 0 && self.held.is_none() {
            use gravity_driver::mem::budget as ledger;
            self.heap = vec![1u8; n]; // alloc: Node
            let (o, release) = ledger::charge::context();
            self.held = Some(ledger::Charge::open(o, release, ledger::Kind::Region, ledger::charge::Door::Heap, n as u64));
            if self.worker {
                self.table.heaps.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    fn binds_at_claim(&self) -> bool {
        self.table.at_claim.load(Ordering::Relaxed)
    }

    fn park(&mut self) {
        self.table.parks.fetch_add(1, Ordering::Relaxed);
        if self.table.shed_on_park.load(Ordering::Relaxed) {
            // the charge leaves before the buffer frees (the field order's rule)
            self.held = None;
            self.heap = Vec::new(); // alloc: Node
        }
    }

    fn park_bytes(&self) -> usize {
        if self.table.shed_on_park.load(Ordering::Relaxed) { self.heap.capacity() } else { 0 }
    }

    fn transfer_held(&mut self, to: gravity_driver::mem::budget::Owner, release: gravity_driver::mem::budget::Release) {
        if let Some(c) = self.held.as_mut() {
            c.transfer(to, release);
        }
    }

    fn seek(&mut self, keys: &[Datum], _keys_null: &[u8]) -> Result<()> {
        assert!(self.bound, "seek on an unbound range source");
        self.unpin();
        self.cur.clear();
        self.lo = keys[0];
        self.hi = keys[1].min(self.table.blocks);
        self.block = self.lo;
        if self.block < self.hi {
            self.table.pins.fetch_add(1, Ordering::Relaxed);
            self.pinned = true;
            self.pin_thread = Some(std::thread::current().id());
        }
        Ok(())
    }

    /// One block per window: lane 0 the row number, lane 1 the block.
    fn window(&mut self, w: &mut Window<'_>) -> Result<u32> {
        if self.block >= self.hi {
            self.unpin();
            return Ok(0);
        }
        if self.block == self.table.fail_block.load(Ordering::Relaxed) {
            return Err(Error::host("range read failed"));
        }
        if self.table.record_owners.load(Ordering::Relaxed) {
            let seen = SeenOwners { worker: self.worker, op: w.op_owner().map(|(o, k)| (o.words(), k as u8)), stmt: w.stmt_owner().map(|(o, r)| (o.words(), r as u8)) };
            self.table.owners.lock().unwrap().push(seen);
        }
        let n = self.table.per_block as usize;
        assert!(n <= MAX_PER_BLOCK);
        for r in 0..n {
            self.lanes[0][r] = self.block * self.table.per_block + r as u64;
            self.lanes[1][r] = self.block;
        }
        for k in 0..self.natts {
            w.set(k, Lane::byval(self.lanes[k].as_ptr() as *const u8, core::ptr::null(), 8, lane::REGION_ARENA, lane::id(lane::ID_ATTR, k as u32)));
        }
        w.rows = n as u32;
        let mut base = WindowBase::at(ScanPos::new(self.rel, self.block as u32, 0));
        base.claim_id = self.lo as u32;
        w.set_base(base);
        self.block += 1;
        Ok(n as u32)
    }

    fn lanes_hint(&self) -> usize {
        self.natts + 1
    }

    fn cursor(&self) -> &Cursor {
        &self.cur
    }

    fn cursor_mut(&mut self) -> &mut Cursor {
        &mut self.cur
    }

    fn refetch(&mut self, _values: &mut [Datum], _isnull: &mut [u8]) -> Result<bool> {
        Ok(false)
    }

    fn substitute(&mut self, _pos: ScanPos) {}

    fn unbind(&mut self) {
        self.unpin();
        self.pinned = false;
        if self.bound {
            self.table.bound_now.fetch_sub(1, Ordering::Relaxed);
        }
        self.bound = false;
        self.cur.clear();
        if self.table.keep.load(Ordering::Relaxed) {
            self.kept = Some(self.table.token.clone());
        }
    }

    fn release_rel(&mut self, rel: u32) -> Option<Box<dyn core::any::Any>> {
        if self.bound || (rel != 0 && rel != self.rel) {
            return None;
        }
        self.kept.take().map(|k| Box::new(k) as Box<dyn core::any::Any>)
    }

    fn rel_read(&self) -> u32 {
        self.rel
    }

    /// The table's stated bytes, and (D4554) the `held_heap` buffer a bind made, as a source counts its bind-sized buffers.
    fn kept_bytes(&self) -> usize {
        self.table.kept_bytes.load(Ordering::Relaxed) + self.heap.capacity()
    }

    fn pins(&self) -> u64 {
        self.pinned as u64
    }

    fn release(&mut self) {
        self.unpin();
        self.cur.clear();
    }

    fn rearm(&mut self) -> Result<()> {
        assert!(self.bound, "a re-arm of an unbound range source");
        self.table.rearms.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

pub struct LegSink {
    pub rows: Mutex<Vec<Vec<Datum>>>,
    pub stop_after: Option<usize>,
    pub leg_pushes: AtomicU64,
    pub legs: bool,
}

impl LegSink {
    pub fn new() -> LegSink {
        LegSink { rows: Mutex::new(Vec::new()), stop_after: None, leg_pushes: AtomicU64::new(0), legs: true }
    }

    pub fn sorted(&self) -> Vec<Vec<Datum>> {
        let mut v = self.rows.lock().unwrap().clone();
        v.sort();
        v
    }

    fn take(&self, batch: BorrowedBatch<'_>) -> Result<Flow> {
        let mut rows = self.rows.lock().unwrap();
        for (v, _) in batch.iter() {
            rows.push(v.to_vec());
        }
        Ok(if self.stop_after.is_some_and(|n| rows.len() >= n) { Flow::Stop } else { Flow::Continue })
    }
}

impl Sink for LegSink {
    fn push(&mut self, batch: BorrowedBatch<'_>) -> Result<Flow> {
        self.take(batch)
    }

    fn leg(&self, _ordinal: u16) -> Option<LegRef<'_>> {
        if self.legs {
            Some(LegRef::of(self))
        } else {
            None
        }
    }
}

impl SinkLeg for LegSink {
    fn push(&self, batch: BorrowedBatch<'_>) -> Result<Flow> {
        self.leg_pushes.fetch_add(1, Ordering::Relaxed);
        self.take(batch)
    }
}

pub struct CountMerge;

impl Merge for CountMerge {
    fn merge_into(&self, leader: &mut State<'_>, partial: Partial<'_>) -> Result<()> {
        let l = unsafe { count_sum(leader.bytes.as_mut_ptr()) };
        let p = unsafe { count_sum(partial.bytes.as_ptr() as *mut u8) };
        l.fold(&p).expect("no float overflow");
        Ok(())
    }
}

pub struct NoMerge;

impl Merge for NoMerge {
    fn merge_into(&self, _leader: &mut State<'_>, _partial: Partial<'_>) -> Result<()> {
        Ok(())
    }
}

pub struct FakeDerive {
    pub fills: [Fill; 4],
    pub bind_fail_site: Option<usize>,
    pub fail_next: Option<(usize, usize)>,
    pub write_outcome: Outcome,
    pub lock_outcomes: [Outcome; 4],
    pub stats: Option<Rc<Stats>>,
    pub range: Option<Arc<RangeTable>>,
    /// D1699: a source site's image reservation past its source (0: the source's size); an arena maps it on lazy pages.
    pub source_hint: usize,
    /// D3253: `Derive::identity` (0: none, as before).
    pub identity: u64,
    seen: Vec<SiteDesc>,
    arena: Arena,
}

impl FakeDerive {
    pub fn new(fills: [Fill; 4]) -> FakeDerive {
        FakeDerive { fills, bind_fail_site: None, fail_next: None, write_outcome: Outcome::Done, lock_outcomes: [Outcome::Done; 4], stats: None, range: None, source_hint: 0, identity: 0, seen: Vec::with_capacity(8), arena: Arena::new(1 << 16) }
    }

    pub fn ranges(table: Arc<RangeTable>) -> FakeDerive {
        let mut d = FakeDerive::new([no_rows; 4]);
        d.range = Some(table);
        d
    }

    fn index_of(&mut self, desc: &SiteDesc) -> usize {
        match self.seen.iter().position(|d| d == desc) {
            Some(i) => i,
            None => {
                self.seen.push(desc.copy_in(self.arena.handle()));
                self.seen.len() - 1
            }
        }
    }

    pub fn with_stats(mut self) -> (FakeDerive, Rc<Stats>) {
        let s = Rc::new(Stats::default());
        self.stats = Some(s.clone());
        (self, s)
    }
}

impl Derive for FakeDerive {
    fn identity(&self) -> u64 {
        self.identity
    }

    fn size_hint(&self, _desc: &SiteDesc) -> usize {
        core::mem::size_of::<FakeSource>().max(core::mem::size_of::<FakeRangeSource>()).max(self.source_hint)
    }

    fn derive_source(&mut self, desc: &SiteDesc, a: ArenaRef) -> SourceBox {
        let i = self.index_of(desc);
        if desc.is_range() {
            let table = self.range.clone().expect("a range table for a range site");
            return box_source(FakeRangeSource { table, lo: 0, hi: 0, block: 0, pinned: false, pin_thread: None, natts: desc.natts(), rel: desc.relation, bound: false, worker: false, cur: Cursor::new(), lanes: [[0; MAX_PER_BLOCK]; 2], kept: None, held: None, heap: Vec::new() }, a);
        }
        box_source(
            FakeSource {
                fill: self.fills[i],
                rows: FixedRows::empty(),
                cursor: 0,
                cur: Cursor::new(),
                natts: desc.natts(),
                rel: desc.relation,
                substituted: None,
                bind_fail: self.bind_fail_site == Some(i),
                fail_next_after: self.fail_next.filter(|f| f.0 == i).map(|f| f.1),
                nexts: 0,
                bound: false,
                held: false,
                parked: false,
                snapshot: Snapshot(0),
                identity: Identity::default(),
                stats: self.stats.clone(),
            },
            a,
        )
    }

    fn writer_size_hint(&self) -> usize {
        core::mem::size_of::<FakeWriter>()
    }

    fn derive_writer(&mut self, _plan: &WritePlan, a: ArenaRef) -> ArenaBox<dyn Writer> {
        box_writer(FakeWriter { outcome: self.write_outcome, bound: false, stats: self.stats.clone() }, a)
    }

    fn locker_size_hint(&self) -> usize {
        core::mem::size_of::<FakeLocker>()
    }

    fn derive_locker(&mut self, _plan: &WritePlan, a: ArenaRef) -> ArenaBox<dyn Locker> {
        box_locker(FakeLocker { outcomes: self.lock_outcomes, calls: 0, bound: false, stats: self.stats.clone() }, a)
    }
}

/// pagepool P3: with `PGRUST_POOL_GRAVITY=1` in the test's environment, the page pool reserved once (the server's
/// postgres.rs does it at its start), so every test binary of this rig runs its regions, arenas and buffers from the pool
/// (the forced-on arm); unset, nothing.
pub fn arm_pool() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if pagepool::switches().gravity {
            assert!(pagepool::init(&pagepool::Config::from_policy()), "the pool reserves its zones");
        }
    });
}

/// pagepool P3: the pool serves Gravity's regions and arenas in this process (`arm_pool` ran with the switch on).
pub fn pool_armed() -> bool {
    pagepool::on() && pagepool::switches().gravity
}

pub fn parts<'a>(params: &'a [Datum], nulls: &'a [u8], flag: &'a AtomicU8) -> BindingParts<'a> {
    arm_pool();
    BindingParts { snapshot: Snapshot(0x5A), command_id: CommandId(3), params, param_nulls: nulls, abort_flag: flag, pins: Pins(1), per_tuple: PerTuple(2), identity: Identity { proc_number: 7, resource_owner: 0xAB, latch: 0xCD }, resolved: None }
}

pub fn binding<'a, S: Sink>(params: &'a [Datum], nulls: &'a [u8], sink: &'a mut S, flag: &'a AtomicU8) -> Binding<'a> {
    Binding::new(parts(params, nulls, flag), sink)
}

/// D1711 (D1705's class): the host rewrites the block in place (`next`, `seek`), which disables this reference (Tree
/// Borrows): re-derive it after every host call, never hold it across one.
unsafe fn site_of<'a>(ctx: *mut Ctx, i: usize) -> &'a mut Site {
    &mut *site_ptr(ctx, i)
}

unsafe fn keys_from_params(ctx: *mut Ctx, s: &mut Site, n: usize) {
    for k in 0..n {
        *s.keys.add(k) = *(*ctx).params.add(k);
        *s.keys_null.add(k) = *(*ctx).params_null.add(k);
    }
}

unsafe fn push(ctx: *mut Ctx) -> i64 {
    gravity_abi::program::push(ctx)
}

unsafe fn out_count(ctx: *mut Ctx) -> i64 {
    (*(*ctx).out).count
}

/// The final push: a `STOP` there is not a failure (the rows went out).
unsafe fn flush(ctx: *mut Ctx) -> i64 {
    if out_count(ctx) > 0 {
        match push(ctx) {
            STOP => OK,
            f => f,
        }
    } else {
        OK
    }
}

unsafe fn emit(ctx: *mut Ctx, natts: usize, parts: &[(*const Datum, *const u8, usize)]) -> i64 {
    let o = &mut *(*ctx).out;
    let row = o.count as usize;
    let mut col = 0;
    for &(v, n, cnt) in parts {
        for k in 0..cnt {
            *o.values.add(row * natts + col + k) = *v.add(k);
            *o.isnull.add(row * natts + col + k) = *n.add(k);
        }
        col += cnt;
    }
    o.count += 1;
    if o.count >= o.cap {
        push(ctx)
    } else {
        OK
    }
}

/// The source's seek from the site's key cells happens inside the next `next_row` / `next`.
unsafe fn seek(ctx: *mut Ctx, i: usize) {
    site_seek(site_ptr(ctx, i));
}

const RESUME_OPEN: i64 = 1;
const RESUME_INNER: i64 = 2;

unsafe fn lookup(ctx: *mut Ctx, nkeys: usize, natts: usize) -> i64 {
    if (*ctx).resume == RESUME_NONE {
        keys_from_params(ctx, site_of(ctx, 0), nkeys);
        seek(ctx, 0);
        (*ctx).resume = RESUME_OPEN;
    }
    loop {
        if cancel_pending(ctx) {
            return CANCELLED;
        }
        let r = next_row(site_ptr(ctx, 0), natts);
        if r < 0 {
            return r;
        }
        let s = site_of(ctx, 0);
        if r == 0 {
            s.state = site::STATE_EOF;
            (*ctx).resume = RESUME_NONE;
            return if flush(ctx) < 0 { ERROR } else { DONE };
        }
        let f = emit(ctx, natts, &[(s.values, s.isnull, natts)]);
        if f == STOP {
            return suspend(out_count(ctx) == 0);
        }
        if f < 0 {
            return f;
        }
    }
}

pub unsafe extern "C-unwind" fn k_lookup3(ctx: *mut Ctx) -> i64 {
    lookup(ctx, 3, 3)
}

/// D776: the row a mark names in site 0's own scan (`MARKED_ROW`), stored in site 1 (`SiteKind::RowOf`) per call.
pub const MARKED_ROW: ScanPos = ScanPos::row(1001, 7, 3);

pub unsafe extern "C-unwind" fn k_lookup3_marked(ctx: *mut Ctx) -> i64 {
    site_of(ctx, 1).pos = MARKED_ROW;
    lookup(ctx, 3, 3)
}

pub unsafe extern "C-unwind" fn k_lookup4(ctx: *mut Ctx) -> i64 {
    lookup(ctx, 4, 2)
}

pub unsafe extern "C-unwind" fn k_join(ctx: *mut Ctx) -> i64 {
    if (*ctx).resume == RESUME_NONE {
        keys_from_params(ctx, site_of(ctx, 0), 3);
        seek(ctx, 0);
        (*ctx).resume = RESUME_OPEN;
    }
    let mut stop = false;
    loop {
        if cancel_pending(ctx) {
            return CANCELLED;
        }
        if (*ctx).resume == RESUME_OPEN {
            let r = next_row(site_ptr(ctx, 0), 3);
            if r < 0 {
                return r;
            }
            let (outer, inner) = (site_of(ctx, 0), site_of(ctx, 1));
            if r == 0 {
                outer.state = site::STATE_EOF;
                (*ctx).resume = RESUME_NONE;
                return if flush(ctx) < 0 { ERROR } else { DONE };
            }
            keys_from_params(ctx, inner, 2);
            *inner.keys.add(2) = *outer.values;
            *inner.keys_null.add(2) = *outer.isnull;
            seek(ctx, 1);
            (*ctx).resume = RESUME_INNER;
        }
        let r = next_row(site_ptr(ctx, 1), 3);
        if r < 0 {
            return r;
        }
        let (outer, inner) = (site_of(ctx, 0), site_of(ctx, 1));
        if r == 0 {
            inner.state = site::STATE_EOF;
            (*ctx).resume = RESUME_OPEN;
            if stop {
                return if flush(ctx) < 0 { ERROR } else { suspend(false) };
            }
            continue;
        }
        let f = emit(ctx, 6, &[(outer.values, outer.isnull, 3), (inner.values, inner.isnull, 3)]);
        stop |= f == STOP;
        if f < 0 && f != STOP {
            return f;
        }
    }
}

pub unsafe extern "C-unwind" fn k_update(ctx: *mut Ctx) -> i64 {
    keys_from_params(ctx, site_of(ctx, 0), 2);
    seek(ctx, 0);
    loop {
        if cancel_pending(ctx) {
            return CANCELLED;
        }
        let r = next_row(site_ptr(ctx, 0), 2);
        if r < 0 {
            return r;
        }
        if r == 0 {
            return DONE;
        }
        let (src, w) = (site_of(ctx, 0), site_of(ctx, 1));
        *w.values = *src.values;
        *w.values.add(1) = (*src.values.add(1)).wrapping_add(1);
        *w.isnull = *src.isnull;
        *w.isnull.add(1) = 0;
        w.pos = src.pos;
        let o = gravity_abi::program::write(ctx, WRITE_UPDATE, 1);
        if o < 0 {
            return o;
        }
        if o == WRITE_RECHECK {
            return recheck(0);
        }
    }
}

pub unsafe extern "C-unwind" fn k_lock(ctx: *mut Ctx) -> i64 {
    if (*ctx).resume == RESUME_NONE {
        keys_from_params(ctx, site_of(ctx, 0), 2);
        seek(ctx, 0);
        (*ctx).resume = RESUME_OPEN;
    }
    loop {
        if cancel_pending(ctx) {
            return CANCELLED;
        }
        let r = next_row(site_ptr(ctx, 0), 1);
        if r < 0 {
            return r;
        }
        if r == 0 {
            (*ctx).resume = RESUME_NONE;
            return if flush(ctx) < 0 { ERROR } else { DONE };
        }
        let l = gravity_abi::program::lock(ctx, 0);
        if l < 0 {
            return l;
        }
        if l == WRITE_RECHECK {
            return recheck(0);
        }
        if l == WRITE_SKIPPED {
            continue;
        }
        debug_assert_eq!(l, WRITE_DONE);
        let s = site_of(ctx, 0);
        let f = emit(ctx, 1, &[(s.values, s.isnull, 1)]);
        if f == STOP {
            return suspend(out_count(ctx) == 0);
        }
        if f < 0 {
            return f;
        }
    }
}

pub unsafe extern "C-unwind" fn k_bad(_ctx: *mut Ctx) -> i64 {
    7
}

pub unsafe extern "C-unwind" fn k_noop(_ctx: *mut Ctx) -> i64 {
    DONE
}

fn layout(nsites: u16, out_natts: u16, out_rows: u32) -> Layout {
    Layout { nsites, out_natts, out_rows, scratch_bytes: 0, exec_bytes: 0 }
}

const NAMES: [&str; 8] = ["c0", "c1", "c2", "c3", "c4", "c5", "c6", "c7"];

fn cols<const N: usize>() -> [OutCol; N] {
    core::array::from_fn(|i| OutCol::new(NAMES[i], 20, -1, 8, true))
}

pub fn p_lookup3(out_rows: u32) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::index_source(a, 1001, 1101, &[1, 2, 3], &[0, 1, 2], &[4, 5, 6])];
    Program::new(layout(1, 3, out_rows), &sites, &cols::<3>(), WritePlan::SELECT, Code::hand(k_lookup3))
}

/// D776: `p_lookup3` whose kernel stores a marked row of site 0's scan in site 1.
pub fn p_lookup3_marked(out_rows: u32) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::index_source(a, 1001, 1101, &[1, 2, 3], &[0, 1, 2], &[4, 5, 6]), SiteDesc::row_of(a, 0, 1001)];
    Program::new(layout(2, 3, out_rows), &sites, &cols::<3>(), WritePlan::SELECT, Code::hand(k_lookup3_marked))
}

pub fn p_lookup4(out_rows: u32) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::index_source(a, 1001, 1102, &[1, 2, 7, 8], &[0, 1, 2, 3], &[3, 4])];
    Program::new(layout(1, 2, out_rows), &sites, &cols::<2>(), WritePlan::SELECT, Code::hand(k_lookup4))
}

pub fn p_join(out_rows: u32) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::index_source(a, 1008, 1108, &[1, 2, 3], &[0, 1, 2], &[3, 4, 5]), SiteDesc::index_source(a, 1009, 1109, &[1, 2, 3], &[0, 1, -1], &[4, 5, 6])];
    Program::new(layout(2, 6, out_rows), &sites, &cols::<6>(), WritePlan::SELECT, Code::hand(k_join))
}

pub fn p_update() -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::index_source(a, 1005, 1105, &[1, 2], &[0, 1], &[1, 2]), SiteDesc::write(1005, 2)];
    let write = WritePlan::write(a, Cmd::Update, 1005, &[IndexPlan::new(a, 1105, &[1, 2], true)]);
    Program::new(layout(2, 0, 1), &sites, &[], write, Code::hand(k_update))
}

pub fn p_lock() -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::index_source(a, 1005, 1105, &[1, 2], &[0, 1], &[3])];
    Program::new(layout(1, 1, 64), &sites, &cols::<1>(), WritePlan::locking(1005, LockKind::Exclusive, WaitPolicy::Block), Code::hand(k_lock))
}

unsafe fn range_seek(ctx: *mut Ctx, i: usize) -> i64 {
    let s = site_of(ctx, i);
    *s.keys = (*ctx).claim.lo;
    *s.keys.add(1) = (*ctx).claim.hi;
    *s.keys_null = 0;
    *s.keys_null.add(1) = 0;
    seek(ctx, i);
    0
}

pub unsafe extern "C-unwind" fn k_range_count(ctx: *mut Ctx) -> i64 {
    range_count_at(ctx, 0)
}

/// `k_range_count` over site 1 (a second member's own range site).
pub unsafe extern "C-unwind" fn k_range_count_s1(ctx: *mut Ctx) -> i64 {
    range_count_at(ctx, 1)
}

unsafe fn range_count_at(ctx: *mut Ctx, i: usize) -> i64 {
    if range_seek(ctx, i) < 0 {
        return ERROR;
    }
    let thr = *(*ctx).params;
    let agg = count_sum(own(ctx));
    loop {
        if cancel_pending(ctx) {
            return CANCELLED;
        }
        let r = next_row(site_ptr(ctx, i), 2);
        if r < 0 {
            return r;
        }
        let s = site_of(ctx, i);
        if r == 0 {
            s.state = site::STATE_EOF;
            return DONE;
        }
        if *s.values < thr {
            agg.add_count(0, 1);
            agg.add_i64(1, *s.values as i64);
        }
    }
}

/// The same count as `k_range_count`, over windows: one host call per block, no call in the row loop.
pub unsafe extern "C-unwind" fn k_range_count_w(ctx: *mut Ctx) -> i64 {
    if range_seek(ctx, 0) < 0 {
        return ERROR;
    }
    let thr = *(*ctx).params;
    let agg = count_sum(own(ctx));
    loop {
        let n = gravity_abi::site::next(site_ptr(ctx, 0));
        if n < 0 {
            return n;
        }
        let s = site_of(ctx, 0);
        if n == 0 {
            s.state = site::STATE_EOF;
            return DONE;
        }
        if cancel_pending(ctx) {
            return CANCELLED;
        }
        let l = &*s.lanes;
        debug_assert!(l.all_valid() && l.width == 8 && s.nlanes >= 1);
        let vals = core::slice::from_raw_parts(l.ptr as *const u64, n as usize);
        let mut hits = 0;
        for &v in vals {
            if v < thr {
                agg.add_count(0, 1);
                agg.add_i64(1, v as i64);
                hits += 1;
            }
        }
        s.rows_out += hits;
        s.cursor = n as u32;
    }
}

/// The share-budgeted fixture: `v % params[0]` → count, sum(v) into a `GROUP` table that opens
/// at `GROUP_INIT` records and grows to the share the driver armed (`SHARE`).
pub const GROUP: RecordLayout = RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count, SlotKind::SumI64]);
pub const GROUP_INIT: u32 = 4;

pub unsafe extern "C-unwind" fn k_range_group_share(ctx: *mut Ctx) -> i64 {
    if range_seek(ctx, 0) < 0 {
        return ERROR;
    }
    let modulus = (*(*ctx).params).max(1);
    let table = HashAgg::at(own(ctx), &GROUP, GROUP_INIT, SHARE);
    loop {
        let n = gravity_abi::site::next(site_ptr(ctx, 0));
        if n < 0 {
            return n;
        }
        let s = site_of(ctx, 0);
        if n == 0 {
            s.state = site::STATE_EOF;
            return DONE;
        }
        if cancel_pending(ctx) {
            return CANCELLED;
        }
        let l = &*s.lanes;
        let vals = core::slice::from_raw_parts(l.ptr as *const u64, n as usize);
        for &v in vals {
            match table.find_or_insert(&[v % modulus]) {
                Some(r) => {
                    r.add_count(0, 1);
                    r.add_i64(1, v as i64);
                }
                None => return unsupported(UNSUPPORTED_SCRATCH),
            }
        }
        s.rows_out += n as u64;
        s.cursor = n as u32;
    }
}

pub unsafe extern "C-unwind" fn k_emit_groups(ctx: *mut Ctx) -> i64 {
    let table = HashAgg::at(own(ctx), &GROUP, GROUP_INIT, SHARE);
    let mut i = if (*ctx).resume == RESUME_NONE { 0 } else { (*ctx).resume as usize };
    while i < table.cap as usize {
        let r = table.record(i);
        i += 1;
        if !r.used() {
            continue;
        }
        let v = [r.key(0), r.count(0) as u64, r.i64(1) as u64];
        let n = [0u8; 3];
        let f = emit(ctx, 3, &[(v.as_ptr(), n.as_ptr(), 3)]);
        if f < 0 {
            return ERROR;
        }
        if f == STOP {
            (*ctx).resume = i as i64;
            return suspend(out_count(ctx) == 0);
        }
    }
    if flush(ctx) < 0 {
        return ERROR;
    }
    DONE
}

/// The merge of `GROUP` partials (what `gravity_parallel::HashAggPartial` does; the driver's tests are below that crate).
pub struct GroupMerge;

impl Merge for GroupMerge {
    /// The same rule as `gravity_parallel::HashAggPartial`: both fit the leader's largest capacity at 7/8 load.
    fn room_for(&self, leader: &[u8], partial: Partial<'_>) -> bool {
        if partial.bytes.len() < gravity_abi::breaker::HASHAGG_HDR || leader.len() < gravity_abi::breaker::HASHAGG_HDR {
            return true;
        }
        let (p, l) = unsafe { (HashAgg::view(partial.bytes.as_ptr()), HashAgg::view(leader.as_ptr())) };
        if p.cap == 0 {
            return true;
        }
        let max_cap = HashAgg::cap_of(&p.layout, if l.limit != 0 { l.limit } else { p.limit }) as u64;
        (l.len as u64 + p.len as u64) <= max_cap - max_cap / 8
    }

    fn merge_into(&self, leader: &mut State<'_>, partial: Partial<'_>) -> Result<()> {
        let p = unsafe { HashAgg::view(partial.bytes.as_ptr()) };
        if p.cap == 0 {
            return Ok(());
        }
        let l = unsafe { HashAgg::at(leader.bytes.as_mut_ptr(), &p.layout, p.cap, SHARE) };
        for r in p.records() {
            l.find_or_insert(r.keys()).ok_or(Error::Unsupported(UNSUPPORTED_SCRATCH))?.fold(&r).map_err(|_| Error::Raised(gravity_abi::breaker::FloatOverflow::MASK))?;
        }
        Ok(())
    }
}

/// Two consumed slots: `(count0 + count1, sum0 + sum1)` from `Partials.consumed[0..2]`.
pub unsafe extern "C-unwind" fn k_emit_two(ctx: *mut Ctx) -> i64 {
    let a = count_sum(consumed(ctx, 0));
    let b = count_sum(consumed(ctx, 1));
    let v = [(a.count(0) + b.count(0)) as u64, (a.i64(1) + b.i64(1)) as u64];
    let n = [0u8; 2];
    if emit(ctx, 2, &[(v.as_ptr(), n.as_ptr(), 2)]) < 0 {
        return ERROR;
    }
    if flush(ctx) < 0 {
        return ERROR;
    }
    DONE
}

pub unsafe extern "C-unwind" fn k_emit_count(ctx: *mut Ctx) -> i64 {
    let agg = count_sum(own(ctx));
    let v = [agg.count(0) as u64, agg.i64(1) as u64];
    let n = [0u8; 2];
    if emit(ctx, 2, &[(v.as_ptr(), n.as_ptr(), 2)]) < 0 {
        return ERROR;
    }
    if flush(ctx) < 0 {
        return ERROR;
    }
    DONE
}

pub unsafe extern "C-unwind" fn k_range_rows(ctx: *mut Ctx) -> i64 {
    range_rows(ctx, 0)
}

/// `k_range_rows` over site 1 (a second UNION ALL arm, multi-emit).
pub unsafe extern "C-unwind" fn k_range_rows_1(ctx: *mut Ctx) -> i64 {
    range_rows(ctx, 1)
}

unsafe fn range_rows(ctx: *mut Ctx, i: usize) -> i64 {
    if (*ctx).resume == RESUME_NONE {
        if range_seek(ctx, i) < 0 {
            return ERROR;
        }
        (*ctx).resume = RESUME_OPEN;
    }
    loop {
        if cancel_pending(ctx) {
            return CANCELLED;
        }
        let r = next_row(site_ptr(ctx, i), 2);
        if r < 0 {
            return r;
        }
        let s = site_of(ctx, i);
        if r == 0 {
            s.state = site::STATE_EOF;
            (*ctx).resume = RESUME_NONE;
            return if flush(ctx) < 0 { ERROR } else { DONE };
        }
        let f = emit(ctx, 2, &[(s.values, s.isnull, 2)]);
        if f == STOP {
            return suspend(out_count(ctx) == 0);
        }
        if f < 0 {
            return f;
        }
    }
}

pub fn p_count(out_rows: u32) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2])];
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES)), 1000), Code::hand(k_range_count)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_emit_count)),
    ];
    Program::with_pipelines(layout(1, 2, out_rows), &sites, &cols::<2>(), WritePlan::SELECT, pipes)
}

/// `p_count`'s shape (a range count into a PlainAgg slot, then its emit to the sink) over the given kernels (CONVERT wave 2:
/// kernels that ask the spill host for their arena's owner before they run `k_range_count` / `k_emit_count`).
pub fn p_count_with(scan: Code, emit: Code, out_rows: u32) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2])];
    let pipes = [(Pipeline::eligible(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES)), 1000), scan), (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), emit)];
    Program::with_pipelines(layout(1, 2, out_rows), &sites, &cols::<2>(), WritePlan::SELECT, pipes)
}

pub fn p_count_w(out_rows: u32) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2])];
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES)), 1000), Code::hand(k_range_count_w)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_emit_count)),
    ];
    Program::with_pipelines(layout(1, 2, out_rows), &sites, &cols::<2>(), WritePlan::SELECT, pipes)
}

/// `GROUP BY v % $0`: a share-budgeted hash aggregate in slot 0 and its emit.
pub fn p_group_share(out_rows: u32) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2])];
    let breaker = Breaker { kind: BreakerKind::HashAgg, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::None };
    let pipes = [(Pipeline::eligible(0, End::Breaker(breaker), 1000), Code::hand(k_range_group_share)), (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_emit_groups))];
    Program::with_pipelines(layout(1, 3, out_rows), &sites, &cols::<3>(), WritePlan::SELECT, pipes)
}

/// `p_group_share` whose section merge declares an owner `d` (D193: the one-owner fold's pinned region is in the footprint).
pub fn p_group_share_owned(out_rows: u32, d: gravity_program::OwnerDesc) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2])];
    let breaker = Breaker { kind: BreakerKind::HashAgg, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::None };
    let pipes = [(Pipeline::eligible(0, End::Breaker(breaker), 1000).owner(d), Code::hand(k_range_group_share)), (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_emit_groups))];
    Program::with_pipelines(layout(1, 3, out_rows), &sites, &cols::<3>(), WritePlan::SELECT, pipes)
}

/// `p_group_share` three times in a chain, slots 0 → 1 → 2 (each consumes the one before): slot 0 is dead when slot 2 opens.
pub fn p_group_chain(out_rows: u32) -> Program {
    p_group_chain_rows(out_rows, 1000)
}

/// `p_group_chain` whose breakers the plan estimated at `rows` rows (the breaker cost's modeled need, D2894).
pub fn p_group_chain_rows(out_rows: u32, rows: u64) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2])];
    let b = |slot| End::Breaker(Breaker { kind: BreakerKind::HashAgg, slot, bytes: 0, budget: Budget::Share, spill: Spill::None });
    let pipes = [
        (Pipeline::eligible(0, b(0), rows), Code::hand(k_range_group_share)),
        (Pipeline::eligible(0, b(1), rows).consuming(&[0]), Code::hand(k_range_group_share)),
        (Pipeline::eligible(0, b(2), rows).consuming(&[1]), Code::hand(k_range_group_share)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_emit_groups)),
    ];
    Program::with_pipelines(layout(1, 3, out_rows), &sites, &cols::<3>(), WritePlan::SELECT, pipes)
}

/// Unequal peaks (D192): slot 0 alone (peak 1, no consumer), then slots 1, 2, 3 live together (peak 3) into the emit.
pub fn p_group_peaks(out_rows: u32) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2])];
    let b = |slot| End::Breaker(Breaker { kind: BreakerKind::HashAgg, slot, bytes: 0, budget: Budget::Share, spill: Spill::None });
    let pipes = [
        (Pipeline::eligible(0, b(0), 1000), Code::hand(k_range_group_share)),
        (Pipeline::eligible(0, b(1), 1000), Code::hand(k_range_group_share)),
        (Pipeline::eligible(0, b(2), 1000), Code::hand(k_range_group_share)),
        (Pipeline::eligible(0, b(3), 1000), Code::hand(k_range_group_share)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[1, 2, 3]), Code::hand(k_emit_groups)),
    ];
    Program::with_pipelines(layout(1, 3, out_rows), &sites, &cols::<3>(), WritePlan::SELECT, pipes)
}

/// `p_group_share` declaring a `MinVar` owner slot: a program whose owners pin group images (`program_images`).
pub fn p_group_images(out_rows: u32) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2])];
    let breaker = Breaker { kind: BreakerKind::HashAgg, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::None };
    let images = RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count, SlotKind::SumI64, SlotKind::MinVar]);
    let scan = Pipeline { owner: Some(gravity_program::OwnerDesc::generic(images, gravity_abi::breaker::ORDER_NONE, 0)), ..Pipeline::eligible(0, End::Breaker(breaker), 1000) };
    let pipes = [(scan, Code::hand(k_range_group_share)), (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_emit_groups))];
    Program::with_pipelines(layout(1, 3, out_rows), &sites, &cols::<3>(), WritePlan::SELECT, pipes)
}

/// Two producers into one consumer: slot 0 and slot 1 each a `COUNT_SUM` record, the emit reads both.
pub fn p_two_slots(out_rows: u32) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2])];
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES).in_slot(0)), 1000), Code::hand(k_range_count_w)),
        (Pipeline::eligible(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES).in_slot(1)), 1000), Code::hand(k_range_count_w)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0, 1]), Code::hand(k_emit_two)),
    ];
    Program::with_pipelines(layout(1, 2, out_rows), &sites, &cols::<2>(), WritePlan::SELECT, pipes)
}

/// Multi-emit (main 09-23): two UNION ALL arms, each a Sink pipeline over its own range site.
pub fn p_two_arms(out_rows: u32) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2]), SiteDesc::range_source(a, 2002, &[1, 2])];
    let pipes = [(Pipeline::eligible(0, End::Sink, 1000), Code::hand(k_range_rows)), (Pipeline::eligible(1, End::Sink, 1000), Code::hand(k_range_rows_1))];
    Program::with_pipelines(layout(2, 2, out_rows), &sites, &cols::<2>(), WritePlan::SELECT, pipes)
}

/// Multi-emit with a producer between the arms: arm 0 (rows over site 1), the count fold over site 0, arm 1 (the count's row).
pub fn p_arm_fold_arm(out_rows: u32) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2]), SiteDesc::range_source(a, 2002, &[1, 2])];
    let pipes = [
        (Pipeline::eligible(1, End::Sink, 1000), Code::hand(k_range_rows_1)),
        (Pipeline::eligible(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES)), 1000), Code::hand(k_range_count)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_emit_count)),
    ];
    Program::with_pipelines(layout(2, 2, out_rows), &sites, &cols::<2>(), WritePlan::SELECT, pipes)
}

pub fn p_rows(out_rows: u32) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2])];
    Program::with_pipelines(layout(1, 2, out_rows), &sites, &cols::<2>(), WritePlan::SELECT, [(Pipeline::eligible(0, End::Sink, 1000), Code::hand(k_range_rows))])
}

/// P2 W1: `p_rows` over relation `rel` (its range site's), so a park holds Nodes of more than one relation.
pub fn p_rows_on(rel: u32, out_rows: u32) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, rel, &[1, 2])];
    Program::with_pipelines(layout(1, 2, out_rows), &sites, &cols::<2>(), WritePlan::SELECT, [(Pipeline::eligible(0, End::Sink, 1000), Code::hand(k_range_rows))])
}

/// CONVERT wave 2 (commit 7): `p_rows` with its scan's code: a sink pipeline, which produces and consumes no slot.
pub fn p_rows_with(scan: Code, out_rows: u32) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2])];
    Program::with_pipelines(layout(1, 2, out_rows), &sites, &cols::<2>(), WritePlan::SELECT, [(Pipeline::eligible(0, End::Sink, 1000), scan)])
}

/// A sink over the range whose claim ends at its first row (a LIMIT 1 reached: DONE mid-range, the page still pinned, D227).
pub fn p_first_row(out_rows: u32) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2])];
    Program::with_pipelines(layout(1, 2, out_rows), &sites, &cols::<2>(), WritePlan::SELECT, [(Pipeline::eligible(0, End::Sink, 1000), Code::hand(k_range_first_row))])
}

/// # Safety — `ctx` is bound to `p_first_row`.
pub unsafe extern "C-unwind" fn k_range_first_row(ctx: *mut Ctx) -> i64 {
    if range_seek(ctx, 0) < 0 {
        return ERROR;
    }
    let r = next_row(site_ptr(ctx, 0), 2);
    if r <= 0 {
        return if r < 0 { r } else { DONE };
    }
    let s = site_of(ctx, 0);
    if emit(ctx, 2, &[(s.values, s.isnull, 2)]) < 0 || flush(ctx) < 0 {
        return ERROR;
    }
    DONE
}

pub fn p_code(code: Code, out_rows: u32, exec_bytes: u32) -> Program {
    Program::new(Layout { nsites: 0, out_natts: 0, out_rows, scratch_bytes: 0, exec_bytes }, &[], &[], WritePlan::SELECT, code)
}

pub fn hdr_cell(node: &gravity_driver::Node, off: usize) -> u64 {
    unsafe { *gravity_abi::ctx::cell(node.ctx(), off) }
}

pub const RESUME_CELL: usize = hdr::RESUME;


// ---- the fixture spill store (§2 commit 4): in-memory epochs, live-file accounting

pub static SPILL_FILES_LIVE: AtomicI64 = AtomicI64::new(0);
pub static SPILL_SETS_LIVE: AtomicI64 = AtomicI64::new(0);
pub static SPILL_EPOCHS: AtomicU64 = AtomicU64::new(0);
pub static SPILL_ALLOW: AtomicBool = AtomicBool::new(true);

pub struct MemSpillFile {
    parts: Vec<Vec<u8>>,
    bytes: u64,
}

impl Drop for MemSpillFile {
    fn drop(&mut self) {
        SPILL_FILES_LIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

impl gravity_driver::SpillFile for MemSpillFile {
    fn write_epoch(&mut self, parts: &[(u32, &[u8])]) -> Result<()> {
        assert!(parts.windows(2).all(|w| w[0].0 < w[1].0), "partitions ascend inside an epoch");
        for (part, bytes) in parts {
            let v = &mut self.parts[*part as usize];
            assert!(v.len() + bytes.len() <= v.capacity(), "the fixture's partition buffer is pre-sized");
            v.extend_from_slice(bytes);
            self.bytes += bytes.len() as u64;
        }
        SPILL_EPOCHS.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn read_part(&self, part: u32, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        // odd chunk sizes so records straddle chunks
        for chunk in self.parts[part as usize].chunks(1000) {
            sink(chunk)?;
        }
        Ok(())
    }

    fn open_part(&self, part: u32) -> Result<Option<Box<dyn gravity_driver::RunReader>>> {
        let bytes = self.parts[part as usize].clone();
        Ok((!bytes.is_empty()).then(|| Box::new(MemRunReader { bytes, pos: 0 }) as Box<dyn gravity_driver::RunReader>))
    }

    fn spilled_bytes(&self) -> u64 {
        self.bytes
    }
}

pub struct MemRunReader {
    bytes: Vec<u8>,
    pos: usize,
}

impl gravity_driver::RunReader for MemRunReader {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        // odd chunk sizes so records straddle fills
        let n = buf.len().min(self.bytes.len() - self.pos).min(1000);
        buf[..n].copy_from_slice(&self.bytes[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

pub struct MemSpillSet;

impl Drop for MemSpillSet {
    fn drop(&mut self) {
        SPILL_SETS_LIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

impl gravity_driver::SpillSet for MemSpillSet {
    fn open(&mut self, _purpose: &str, _generation: u64, _worker: usize, nparts: u32) -> Result<Box<dyn gravity_driver::SpillFile>> {
        SPILL_FILES_LIVE.fetch_add(1, Ordering::Relaxed);
        // pre-sized partitions: the store's own growth is not the driver's steady state
        Ok(Box::new(MemSpillFile { parts: (0..nparts).map(|_| Vec::with_capacity(1 << 20)).collect(), bytes: 0 }))
    }
}

pub struct MemSpillFactory;

impl gravity_driver::SpillFactory for MemSpillFactory {
    fn create(&self) -> Result<Box<dyn gravity_driver::SpillSet>> {
        if !SPILL_ALLOW.load(Ordering::Relaxed) {
            return Err(Error::host("fixture: temp files refused"));
        }
        SPILL_SETS_LIVE.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(MemSpillSet))
    }
}

pub static MEM_SPILL: MemSpillFactory = MemSpillFactory;

/// The spilling group-by: `k_range_group_share` that scatters when the share is full, with
/// `params[1]` = the partition bits; its emit refills partition by partition.
pub unsafe extern "C-unwind" fn k_range_group_spill(ctx: *mut Ctx) -> i64 {
    range_group_spill(ctx, 0)
}

/// `k_range_group_spill` over site 1 (a second scan whose fold reuses the first's slot: D165).
pub unsafe extern "C-unwind" fn k_range_group_spill_1(ctx: *mut Ctx) -> i64 {
    range_group_spill(ctx, 1)
}

unsafe fn range_group_spill(ctx: *mut Ctx, site: usize) -> i64 {
    if range_seek(ctx, site) < 0 {
        return ERROR;
    }
    let modulus = (*(*ctx).params).max(1);
    let bits = *(*ctx).params.add(1);
    let table = HashAgg::at(own(ctx), &GROUP, GROUP_INIT, SHARE);
    loop {
        let n = gravity_abi::site::next(site_ptr(ctx, site));
        if n < 0 {
            return n;
        }
        let s = site_of(ctx, site);
        if n == 0 {
            s.state = site::STATE_EOF;
            return DONE;
        }
        if cancel_pending(ctx) {
            return CANCELLED;
        }
        let l = &*s.lanes;
        let vals = core::slice::from_raw_parts(l.ptr as *const u64, n as usize);
        for &v in vals {
            let key = [v % modulus];
            let r = match table.find_or_insert(&key) {
                Some(r) => r,
                None => {
                    // over the share: scatter this table into an epoch and go on (a spill batch is never a suspend point)
                    if gravity_abi::program::spill(ctx, SPILL_SCATTER, bits) < 0 {
                        return ERROR;
                    }
                    match table.find_or_insert(&key) {
                        Some(r) => r,
                        None => return unsupported(UNSUPPORTED_SCRATCH),
                    }
                }
            };
            r.add_count(0, 1);
            r.add_i64(1, v as i64);
        }
        let s = site_of(ctx, site);
        s.rows_out += n as u64;
        s.cursor = n as u32;
    }
}

/// Emits every group: the in-memory table when nothing spilled, else partition by partition
/// through `SPILL_REFILL` (resume = (EMIT, partition << 32 | record index)).
pub unsafe extern "C-unwind" fn k_emit_groups_spill(ctx: *mut Ctx) -> i64 {
    use gravity_abi::ctx::resume;
    let parts = gravity_abi::program::spill(ctx, SPILL_PARTS, 0);
    if parts < 0 {
        return ERROR;
    }
    let table = HashAgg::at(own(ctx), &GROUP, GROUP_INIT, SHARE);
    let word = (*ctx).resume;
    let (mut part, mut i) = if word == RESUME_NONE { (0u32, 0usize) } else { ((resume::position(word) >> 32) as u32, (resume::position(word) & 0xffff_ffff) as usize) };
    loop {
        // the refill queue grows when a partition splits: its length is read per token
        let parts = gravity_abi::program::spill(ctx, SPILL_PARTS, 0);
        if parts < 0 {
            return ERROR;
        }
        if part >= (parts as u32).max(1) {
            break;
        }
        if parts > 0 && i == 0 && gravity_abi::program::spill(ctx, SPILL_REFILL, part as u64) < 0 {
            return ERROR;
        }
        while i < table.cap as usize {
            let r = table.record(i);
            i += 1;
            if !r.used() {
                continue;
            }
            let v = [r.key(0), r.count(0) as u64, r.i64(1) as u64];
            let n = [0u8; 3];
            let f = emit(ctx, 3, &[(v.as_ptr(), n.as_ptr(), 3)]);
            if f < 0 && f != STOP {
                return f;
            }
            if f == STOP {
                (*ctx).resume = resume::word(resume::EMIT, ((part as u64) << 32) | i as u64);
                return suspend(out_count(ctx) == 0);
            }
        }
        part += 1;
        i = 0;
    }
    if flush(ctx) < 0 {
        return ERROR;
    }
    DONE
}

/// `GROUP BY v % $0` spilling to `1 << $1` partitions past its share.
/// Two scans each folding into slot 0 (the second after the first's emit freed it), both spilled: the D165 shape.
pub fn p_two_group_spills(out_rows: u32) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2]), SiteDesc::range_source(a, 2002, &[1, 2])];
    let breaker = Breaker { kind: BreakerKind::HashAgg, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::Partitioned { bits: 3 } };
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(breaker), 1000), Code::hand(k_range_group_spill)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_emit_groups_spill)),
        (Pipeline::eligible(1, End::Breaker(breaker), 1000), Code::hand(k_range_group_spill_1)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_emit_groups_spill)),
    ];
    Program::with_pipelines(layout(2, 3, out_rows), &sites, &cols::<3>(), WritePlan::SELECT, pipes)
}

pub fn p_group_spill(out_rows: u32) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2])];
    let breaker = Breaker { kind: BreakerKind::HashAgg, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::Partitioned { bits: 3 } };
    let pipes = [(Pipeline::eligible(0, End::Breaker(breaker), 1000), Code::hand(k_range_group_spill)), (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_emit_groups_spill))];
    Program::with_pipelines(layout(1, 3, out_rows), &sites, &cols::<3>(), WritePlan::SELECT, pipes)
}

/// DR-12: `p_group_spill`'s shape over `rows` planner rows at `bits` partition bits, its section's merge declaring owner `d`
/// when given (the parallel owner phase, `own_single` past the leader region).
pub fn p_group_spill_rows(out_rows: u32, rows: u64, bits: u8, d: Option<gravity_program::OwnerDesc>) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2])];
    let breaker = Breaker { kind: BreakerKind::HashAgg, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::Partitioned { bits } };
    let scan = Pipeline::eligible(0, End::Breaker(breaker), rows);
    let scan = match d {
        Some(d) => scan.owner(d),
        None => scan,
    };
    let pipes = [(scan, Code::hand(k_range_group_spill)), (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_emit_groups_spill))];
    Program::with_pipelines(layout(1, 3, out_rows), &sites, &cols::<3>(), WritePlan::SELECT, pipes)
}

std::thread_local! {
    static LEASES: std::cell::RefCell<Vec<(gravity_driver::Grant, std::rc::Rc<gravity_driver::Reservation>)>> = const { std::cell::RefCell::new(Vec::new()) };
    /// tests/tier0.rs: (interpreted calls, compiled calls) on this test's thread (here, not a TLS block of its own: the census).
    pub static TIER0_CALLS: Cell<(u32, u32)> = const { Cell::new((0, 0)) };
}

/// An unadmitted statement lease at `grant`, one per grant per thread (a clone never allocates: the allocation-counting rigs
/// see nothing after the first touch). Keyed by the whole grant: a floored grant (D2587) of the same bytes is another lease.
pub fn lease(grant: gravity_driver::Grant) -> std::rc::Rc<gravity_driver::Reservation> {
    arm_pool();
    LEASES.with(|l| {
        let mut l = l.borrow_mut();
        if let Some((_, r)) = l.iter().find(|(g, _)| *g == grant) {
            return r.clone();
        }
        let r = std::rc::Rc::new(gravity_driver::Reservation::unadmitted(grant));
        l.push((grant, r.clone()));
        r
    })
}

/// D254's fixture region: `head` bytes used from the start (records from `GROW_HDR`), the u32 limit the driver arms at offset 8
/// (the region's length; `region_grow` re-arms it), `tail` bytes used from the end, and the rows' count and sum.
#[repr(C)]
pub struct GrowHdr {
    pub head: u64,
    pub limit: u32,
    pub pad: u32,
    pub tail: u64,
    pub count: u64,
    pub sum: u64,
}

pub const GROW_HDR: u64 = 64;

/// The region at `p` as a `GrowHdr` (its head started at `GROW_HDR` on first touch).
pub unsafe fn grow_hdr<'a>(p: *mut u8) -> &'a mut GrowHdr {
    let h = &mut *(p as *mut GrowHdr);
    if h.head == 0 {
        h.head = GROW_HDR;
    }
    h
}

/// D2894: the grows `k_range_grow` saw move its region (a reopen); a grow in place within a floor reservation keeps it.
pub static GROW_MOVES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Every row `v` appended twice: `v` at the head, `!v` at the tail; the region grows (`region_grow`) whenever the next row does
/// not fit, so a breaker past its share completes instead of failing (D254).
pub unsafe extern "C-unwind" fn k_range_grow(ctx: *mut Ctx) -> i64 {
    if range_seek(ctx, 0) < 0 {
        return ERROR;
    }
    loop {
        let n = gravity_abi::site::next(site_ptr(ctx, 0));
        if n < 0 {
            return n;
        }
        let s = site_of(ctx, 0);
        if n == 0 {
            s.state = site::STATE_EOF;
            return DONE;
        }
        let l = &*s.lanes;
        let vals = core::slice::from_raw_parts(l.ptr as *const u64, n as usize);
        let mut base = own(ctx);
        for &v in vals {
            let h = grow_hdr(base);
            if h.head + h.tail + 16 > h.limit as u64 {
                let b = gravity_abi::program::region_grow(ctx, 16, h.head, h.tail);
                if b == 0 {
                    return ERROR;
                }
                assert_eq!(b % 64, 0, "a reopened region is 64-aligned");
                assert_eq!(b as usize as *mut u8, own(ctx), "Partials.own moved to the new base");
                if b as usize as *mut u8 != base {
                    GROW_MOVES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                base = b as usize as *mut u8;
            }
            let h = grow_hdr(base);
            *(base.add(h.head as usize) as *mut u64) = v;
            h.head += 8;
            h.tail += 8;
            *(base.add((h.limit as u64 - h.tail) as usize) as *mut u64) = !v;
            h.count += 1;
            h.sum += v;
        }
        let s = site_of(ctx, 0);
        s.rows_out += n as u64;
        s.cursor = n as u32;
    }
}

/// The leader region's rows checked: `(count, sum, sum of the head records, sum of the complemented tail records)`.
pub unsafe fn grow_totals(p: *mut u8) -> [u64; 4] {
    let h = grow_hdr(p);
    let len = h.limit as u64;
    let head = (GROW_HDR..h.head).step_by(8).map(|o| *(p.add(o as usize) as *const u64)).sum::<u64>();
    let tail = (len - h.tail..len).step_by(8).map(|o| !*(p.add(o as usize) as *const u64)).sum::<u64>();
    [h.count, h.sum, head, tail]
}

pub unsafe extern "C-unwind" fn k_emit_grow(ctx: *mut Ctx) -> i64 {
    let v = grow_totals(consumed(ctx, 0));
    let n = [0u8; 4];
    if emit(ctx, 4, &[(v.as_ptr(), n.as_ptr(), 4)]) < 0 {
        return ERROR;
    }
    if flush(ctx) < 0 {
        return ERROR;
    }
    DONE
}

/// D254: a share-budgeted build (slot 0, `HashBuild`) that grows past its share, and its emit of the totals.
pub fn p_grow_share(out_rows: u32) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2])];
    let breaker = Breaker { kind: BreakerKind::HashBuild, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::None };
    let pipes = [(Pipeline::eligible(0, End::Breaker(breaker), 1000), Code::hand(k_range_grow)), (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_emit_grow))];
    Program::with_pipelines(layout(1, 4, out_rows), &sites, &cols::<4>(), WritePlan::SELECT, pipes)
}

/// The grows `k_range_grow_in` saw refused (`region_grow_in` answered 1: the breaker spills there, the row is dropped here)
/// and the largest region limit it saw armed.
pub static GROW_IN_REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static GROW_IN_LIMIT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `k_range_grow` over the spilling road (`region_grow_in`): a refused grow drops its row (counted in `GROW_IN_REFUSED`)
/// where a spilling breaker puts it to a partition.
pub unsafe extern "C-unwind" fn k_range_grow_in(ctx: *mut Ctx) -> i64 {
    if range_seek(ctx, 0) < 0 {
        return ERROR;
    }
    loop {
        let n = gravity_abi::site::next(site_ptr(ctx, 0));
        if n < 0 {
            return n;
        }
        let s = site_of(ctx, 0);
        if n == 0 {
            s.state = site::STATE_EOF;
            return DONE;
        }
        let l = &*s.lanes;
        let vals = core::slice::from_raw_parts(l.ptr as *const u64, n as usize);
        let mut base = own(ctx);
        for &v in vals {
            let h = grow_hdr(base);
            if h.head + h.tail + 16 > h.limit as u64 {
                match gravity_abi::program::region_grow_in(ctx, 16, h.head, h.tail) {
                    0 => return ERROR,
                    1 => {
                        GROW_IN_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        continue;
                    }
                    b => {
                        base = b as usize as *mut u8;
                        GROW_IN_LIMIT.fetch_max(grow_hdr(base).limit as u64, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
            let h = grow_hdr(base);
            *(base.add(h.head as usize) as *mut u64) = v;
            h.head += 8;
            h.tail += 8;
            *(base.add((h.limit as u64 - h.tail) as usize) as *mut u64) = !v;
            h.count += 1;
            h.sum += v;
        }
        let s = site_of(ctx, 0);
        s.rows_out += n as u64;
        s.cursor = n as u32;
    }
}

/// `p_grow_share` as a spilling hash aggregate (`Spill::Partitioned`): its grows take the spilling road (`k_range_grow_in`).
pub fn p_grow_in(out_rows: u32) -> Program {
    let st = stage();
    let a = st.handle();
    let sites = [SiteDesc::range_source(a, 2001, &[1, 2])];
    let breaker = Breaker { kind: BreakerKind::HashAgg, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::Partitioned { bits: 3 } };
    let pipes = [(Pipeline::eligible(0, End::Breaker(breaker), 1000), Code::hand(k_range_grow_in)), (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_emit_grow))];
    Program::with_pipelines(layout(1, 4, out_rows), &sites, &cols::<4>(), WritePlan::SELECT, pipes)
}

/// Fold partial `p` (a `GrowHdr` region) into the leader's, which grows first when the records do not fit (`State::grow`
/// copies the whole region and leaves the header: the tail moves to the new end here and the limit is re-armed).
pub fn grow_fold(leader: &mut State<'_>, p: &[u8]) -> Result<()> {
    unsafe {
        let pp = p.as_ptr() as *mut u8;
        let ph = grow_hdr(pp);
        assert_eq!(ph.limit as usize, p.len(), "Partial.bytes is the grown partial");
        let (pc, ps, phead, ptail) = (ph.count, ph.sum, ph.head, ph.tail);
        let lh = grow_hdr(leader.bytes.as_mut_ptr());
        let need = (phead - GROW_HDR) + ptail;
        if lh.head + lh.tail + need > lh.limit as u64 {
            let (old, tail) = (lh.limit as usize, lh.tail as usize);
            leader.grow(lh.head as usize + tail + need as usize)?;
            let len = leader.bytes.len();
            assert!(len >= 2 * old, "State::grow reopens at max(2 x len, bytes)");
            leader.bytes.copy_within(old - tail..old, len - tail);
            grow_hdr(leader.bytes.as_mut_ptr()).limit = len as u32;
        }
        let base = leader.bytes.as_mut_ptr();
        let lh = grow_hdr(base);
        core::ptr::copy_nonoverlapping(pp.add(GROW_HDR as usize), base.add(lh.head as usize), (phead - GROW_HDR) as usize);
        lh.head += phead - GROW_HDR;
        lh.tail += ptail;
        core::ptr::copy_nonoverlapping(pp.add(p.len() - ptail as usize), base.add((lh.limit as u64 - lh.tail) as usize), ptail as usize);
        lh.count += pc;
        lh.sum += ps;
    }
    Ok(())
}

pub struct GrowMerge;

impl Merge for GrowMerge {
    fn merge_into(&self, leader: &mut State<'_>, partial: Partial<'_>) -> Result<()> {
        grow_fold(leader, partial.bytes)
    }
}

/// `p` with its lifetimes declaring `members` rerun set 0's, reset and run at every run, a member lazy build (`lazy_for`)
/// naming its member probe (lowerv4 lifetime `program`'s declaration for a set no cell holds).
pub fn rerun_set(mut p: Program, members: &[u16]) -> Program {
    use gravity_program::lifetime::{Lifetimes, RunLife, RunMember, Runner, NO_PROBE};
    let probe = |m: u16| u16::try_from(p.pipelines[m as usize].lazy_for).ok().filter(|q| members.contains(q)).unwrap_or(NO_PROBE);
    let ms = members.iter().map(|&m| RunMember { pipeline: m, runner: Runner::Set(0), life: RunLife::EveryRun, lazy_probe: probe(m) }).collect();
    let mut d = p.v4.as_deref().map(|d| d.thaw()).unwrap_or_default();
    d.lifetimes = Lifetimes::new(ms).expect("one runner per member");
    p.set_v4(&d);
    p
}
