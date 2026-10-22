#![allow(dead_code)]

use std::sync::atomic::{AtomicI64, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use gravity_abi::ctx::lane;
use gravity_abi::{Datum, Lane, ScanPos};
use gravity_driver::{box_source, ArenaBox, ArenaRef, SourceBox, Binding, BindingParts, BindingRef, BorrowedBatch, CommandId, Cursor, Derive, Entry, Error, Flow, Identity, Locker, Merge, PerTuple, Pins, Result, LegRef, Sink, SinkLeg, Snapshot, Source, Window, WindowBase, Writer};
use gravity_program::hand::Oids;
use gravity_parallel::{run_program, BlockRange, ClaimLedger, ClaimSize, HashAggPartial, HashBuildPartial, MorselSource, Plan, PlainAggPartial, Pool, RowsUnordered};
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, HashAgg, KeyKind, RecordLayout, SlotKind, Spill, SHARE};
use gravity_program::{Code, End, Layout, Pipeline, Program, Reason, SiteDesc, WritePlan};
use gravity_abi::ctx::{site_ptr, RESUME_NONE};
use gravity_abi::status::{unsupported, CANCELLED, DONE, ERROR, UNSUPPORTED_SCRATCH};
use gravity_kernels::util::HOST_STOP;
use gravity_abi::Ctx;
use gravity_kernels::util::{advance, consumed, done, emit, end, flush, interrupted, lane, partial, range_seek, stopped};

pub const T_A: u32 = Oids::T_A;
pub const T_KV: u32 = Oids::T_KV;
pub const T_INNER: u32 = Oids::T_INNER;
pub const T_OUTER: u32 = Oids::T_OUTER;
pub type Gen = fn(u64) -> Option<[i64; 2]>;

pub struct FixtureTable {
    pub rel: u32,
    pub blocks: u64,
    pub per_block: u64,
    pub gen: Gen,
    pub pins: AtomicI64,
    pub fail_block: AtomicU64,
    pub abort_after: AtomicU64,
    pub served: AtomicU64,
    pub abort: Option<&'static AtomicU8>,
    pub started_after_abort: AtomicU64,
    pub ledger: ClaimLedger,
}

impl FixtureTable {
    pub fn new(rel: u32, blocks: u64, per_block: u64, gen: Gen) -> Arc<FixtureTable> {
        Arc::new(FixtureTable {
            rel,
            blocks,
            per_block,
            gen,
            pins: AtomicI64::new(0),
            fail_block: AtomicU64::new(u64::MAX),
            abort_after: AtomicU64::new(u64::MAX),
            served: AtomicU64::new(0),
            abort: None,
            started_after_abort: AtomicU64::new(0),
            ledger: ClaimLedger::new(blocks),
        })
    }

    pub fn with_abort(mut self: Arc<FixtureTable>, abort: &'static AtomicU8, after: u64) -> Arc<FixtureTable> {
        let t = Arc::get_mut(&mut self).unwrap();
        t.abort = Some(abort);
        t.abort_after.store(after, Ordering::Relaxed);
        self
    }

    pub fn rows(&self) -> impl Iterator<Item = [i64; 2]> + '_ {
        (0..self.blocks * self.per_block).filter_map(|r| (self.gen)(r))
    }

    pub fn fresh(&self) {
        self.ledger.clear();
        self.served.store(0, Ordering::Relaxed);
        self.started_after_abort.store(0, Ordering::Relaxed);
    }
}

pub struct FixtureSource {
    table: Arc<FixtureTable>,
    lo: u64,
    hi: u64,
    block: u64,
    pinned: bool,
    /// The thread that took the pin: a pin is released where it was taken
    /// (R3 on every exit — the buffer private refcounts are per thread).
    pinned_by: Option<std::thread::ThreadId>,
    natts: usize,
    cur: Cursor,
    lanes: Vec<u64>,
}

impl FixtureSource {
    fn unpin(&mut self) {
        if self.pinned {
            assert_eq!(self.pinned_by, Some(std::thread::current().id()), "a pin released on a thread other than the one that took it");
            self.table.pins.fetch_sub(1, Ordering::Relaxed);
            self.pinned = false;
        }
    }
}

impl Source for FixtureSource {
    fn bind(&mut self, _b: &BindingRef<'_>) -> Result<()> {
        Ok(())
    }

    fn seek(&mut self, keys: &[Datum], _keys_null: &[u8]) -> Result<()> {
        self.unpin();
        self.cur.clear();
        let lo = keys[0].min(self.table.blocks);
        self.hi = keys[1].min(self.table.blocks);
        self.table.ledger.record(gravity_abi::MorselRange::new(lo, self.hi));
        if self.table.abort.is_some_and(|a| a.load(Ordering::Acquire) != 0) {
            self.table.started_after_abort.fetch_add(1, Ordering::Relaxed);
        }
        self.lo = lo;
        self.block = lo;
        if self.block < self.hi {
            self.table.pins.fetch_add(1, Ordering::Relaxed);
            self.pinned = true;
            self.pinned_by = Some(std::thread::current().id());
        }
        Ok(())
    }

    /// One block per window, the generator's `None` rows compacted away.
    fn window(&mut self, w: &mut Window<'_>) -> Result<u32> {
        if self.block >= self.hi {
            self.unpin();
            return Ok(0);
        }
        if self.block == self.table.fail_block.load(Ordering::Relaxed) {
            return Err(Error::host("range read failed"));
        }
        let per = self.table.per_block as usize;
        let mut n = 0usize;
        for r in 0..per {
            if let Some(row) = (self.table.gen)(self.block * self.table.per_block + r as u64) {
                for k in 0..self.natts {
                    self.lanes[k * per + n] = row[k] as u64;
                }
                n += 1;
            }
        }
        let served = self.table.served.fetch_add(per as u64, Ordering::Relaxed) + per as u64;
        let after = self.table.abort_after.load(Ordering::Relaxed);
        if served >= after && served - (per as u64) < after {
            if let Some(a) = self.table.abort {
                a.store(1, Ordering::Release);
            }
        }
        for k in 0..self.natts {
            w.set(k, Lane::byval(self.lanes[k * per..].as_ptr() as *const u8, core::ptr::null(), 8, lane::REGION_ARENA, lane::id(lane::ID_ATTR, k as u32)));
        }
        w.rows = n as u32;
        let mut base = WindowBase::at(ScanPos::new(self.table.rel, self.block as u32, 0));
        base.claim_id = self.lo as u32;
        w.set_base(base);
        self.block += 1;
        if n == 0 {
            // an empty block is still a window: the kernel loops on to the next
            w.rows = 0;
            return self.window(w);
        }
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
        self.cur.clear();
    }

    fn pins(&self) -> u64 {
        self.pinned as u64
    }

    fn release(&mut self) {
        self.unpin();
        self.cur.clear();
    }
}

pub struct FixtureDerive {
    pub tables: Vec<Arc<FixtureTable>>,
}

impl FixtureDerive {
    pub fn new(tables: &[&Arc<FixtureTable>]) -> FixtureDerive {
        FixtureDerive { tables: tables.iter().map(|t| (*t).clone()).collect() }
    }

    pub fn table(&self, rel: u32) -> &Arc<FixtureTable> {
        self.tables.iter().find(|t| t.rel == rel).expect("a table for the relation")
    }
}

impl Derive for FixtureDerive {
    fn size_hint(&self, _desc: &SiteDesc) -> usize {
        core::mem::size_of::<FixtureSource>()
    }

    fn derive_source(&mut self, desc: &SiteDesc, a: ArenaRef) -> SourceBox {
        let table = self.table(desc.relation).clone();
        let lanes = vec![0u64; desc.natts() * table.per_block as usize]; // alloc: Node
        box_source(FixtureSource { table, lo: 0, hi: 0, block: 0, pinned: false, pinned_by: None, natts: desc.natts(), cur: Cursor::new(), lanes }, a)
    }

    fn writer_size_hint(&self) -> usize {
        0
    }

    fn derive_writer(&mut self, _plan: &WritePlan, _a: ArenaRef) -> ArenaBox<dyn Writer> {
        unreachable!("no writes in the parallel fixtures")
    }

    fn locker_size_hint(&self) -> usize {
        0
    }

    fn derive_locker(&mut self, _plan: &WritePlan, _a: ArenaRef) -> ArenaBox<dyn Locker> {
        unreachable!("no locks in the parallel fixtures")
    }
}

pub struct LegSink {
    pub rows: Mutex<Vec<Vec<Datum>>>,
    pub count: AtomicU64,
    pub keep: bool,
}

impl LegSink {
    pub fn new(keep: bool) -> LegSink {
        LegSink { rows: Mutex::new(Vec::new()), count: AtomicU64::new(0), keep }
    }

    /// The rows in the order they were pushed (the ordered emit's proof).
    pub fn rows(&self) -> Vec<Vec<Datum>> {
        self.rows.lock().unwrap().clone()
    }

    pub fn sorted(&self) -> Vec<Vec<Datum>> {
        let mut v = self.rows.lock().unwrap().clone();
        v.sort();
        v
    }

    fn take(&self, batch: BorrowedBatch<'_>) -> Result<Flow> {
        self.count.fetch_add(batch.rows as u64, Ordering::Relaxed);
        if self.keep {
            let mut rows = self.rows.lock().unwrap();
            for (v, _) in batch.iter() {
                rows.push(v.to_vec());
            }
        }
        Ok(Flow::Continue)
    }
}

impl Sink for LegSink {
    fn push(&mut self, batch: BorrowedBatch<'_>) -> Result<Flow> {
        self.take(batch)
    }

    fn leg(&self, _ordinal: u16) -> Option<LegRef<'_>> {
        Some(LegRef::of(self))
    }
}

impl SinkLeg for LegSink {
    fn push(&self, batch: BorrowedBatch<'_>) -> Result<Flow> {
        self.take(batch)
    }
}

pub static NO_ABORT: AtomicU8 = AtomicU8::new(0);

pub fn parts<'a>(params: &'a [Datum], nulls: &'a [u8], abort: &'a AtomicU8) -> BindingParts<'a> {
    BindingParts { snapshot: Snapshot(0x5A), command_id: CommandId(3), params, param_nulls: nulls, abort_flag: abort, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity { proc_number: 7, resource_owner: 0, latch: 0 }, resolved: None }
}

pub const PLAIN: PlainAggPartial = PlainAggPartial;
pub const HASHBUILD: HashBuildPartial = HashBuildPartial;

/// The group-by merge with `par_hashagg`'s owner phase under its default words (unordered, no bound).
pub fn hashagg_merge() -> &'static HashAggPartial {
    static M: std::sync::OnceLock<HashAggPartial> = std::sync::OnceLock::new();
    M.get_or_init(|| HashAggPartial::new(0, Some(gravity_kernels::table::params::owner_fns(&gravity_kernels::table::params::DEFAULT_PARAMS))))
}

/// The distinct merge with `par_distinct`'s two-round owner phase (the distinct test's 120,000 pairs).
pub fn distinct_merge() -> &'static HashAggPartial {
    static M: std::sync::OnceLock<HashAggPartial> = std::sync::OnceLock::new();
    M.get_or_init(|| HashAggPartial::new(0, Some(gravity_kernels::par_distinct::owner_fns(&gravity_kernels::par_distinct::params(120_000)))))
}
pub const ROWS: RowsUnordered = RowsUnordered;

pub fn merge_for(program: &Program, p: usize) -> &'static dyn Merge {
    match program.pipelines[p].ends_in {
        End::Breaker(Breaker { kind: BreakerKind::PlainAgg, .. }) => &PLAIN,
        End::Breaker(Breaker { kind: BreakerKind::HashAgg, .. }) => hashagg_merge(),
        End::Breaker(Breaker { kind: BreakerKind::Distinct, .. }) => distinct_merge(),
        End::Breaker(Breaker { kind: BreakerKind::HashBuild, .. }) => &HASHBUILD,
        _ => &ROWS,
    }
}

pub struct FixturePlan {
    pub program: Arc<Program>,
    pub width: u16,
    pub cursors: [BlockRange; 4],
    /// A pipeline over a slot's spill partitions (the join's third phase) claims here, sized after its producer.
    pub parts: gravity_parallel::PartitionRange,
}

impl FixturePlan {
    pub fn new(program: &Arc<Program>, derive: &FixtureDerive, width: u16, claim: u64) -> FixturePlan {
        FixturePlan::sized(program, derive, width, ClaimSize::Fixed(claim))
    }

    pub fn sized(program: &Arc<Program>, derive: &FixtureDerive, width: u16, size: ClaimSize) -> FixturePlan {
        let blocks = |p: usize| {
            let site = program.pipelines.get(p).map_or(-1, |q| q.driving_site);
            if site < 0 {
                0
            } else {
                derive.table(program.sites[site as usize].relation).blocks
            }
        };
        let cursors = [BlockRange::new(blocks(0), size), BlockRange::new(blocks(1), size), BlockRange::new(blocks(2), size), BlockRange::new(blocks(3), size)];
        FixturePlan { program: program.clone(), width, cursors, parts: gravity_parallel::PartitionRange::deferred() }
    }
}

impl Plan for FixturePlan {
    fn width_of(&self, _pipeline: usize) -> u16 {
        self.width
    }

    fn claims_for(&self, pipeline: usize) -> &dyn MorselSource {
        if matches!(self.program.pipelines[pipeline].claims, gravity_program::ClaimKind::SpillPartitions { .. }) {
            return &self.parts;
        }
        &self.cursors[pipeline]
    }

    fn merge_for(&self, pipeline: usize) -> &dyn Merge {
        merge_for(&self.program, pipeline)
    }

    fn after_pipeline(&self, pipeline: usize, exec: &mut gravity_driver::Exec) -> Result<()> {
        if let Some(slot) = self.program.pipelines[pipeline].produces() {
            if self.program.pipelines.iter().any(|q| q.claims == gravity_program::ClaimKind::SpillPartitions { slot }) {
                self.parts.set_total(exec.node().spill_parts_of(slot) as u64);
            }
        }
        Ok(())
    }
}

fn run_with(entry: &mut Entry, program: &Arc<Program>, derive: &mut FixtureDerive, pool: &dyn Pool, width: u16, claim: u64, params: &[Datum], abort: &AtomicU8, sink: &mut LegSink, key: Option<gravity_program::PlanKey>) -> Result<u64> {
    run_sized(entry, program, derive, pool, width, ClaimSize::Fixed(claim), params, abort, sink, key)
}

pub fn run_sized(entry: &mut Entry, program: &Arc<Program>, derive: &mut FixtureDerive, pool: &dyn Pool, width: u16, size: ClaimSize, params: &[Datum], abort: &AtomicU8, sink: &mut LegSink, key: Option<gravity_program::PlanKey>) -> Result<u64> {
    let nulls = [0u8; 16];
    let plan = FixturePlan::sized(program, derive, width, size);
    let mut b = Binding::new(parts(params, &nulls[..params.len().max(1)], abort), sink);
    let mut exec = entry.start(key, program, derive, &mut b, lease(gravity_driver::Grant::of(8 << 20)))?;
    let r = run_program(entry, &mut exec, &mut b, derive, pool, &plan);
    entry.end(exec, r.is_ok());
    r
}

pub fn run_once(entry: &mut Entry, program: &Arc<Program>, derive: &mut FixtureDerive, pool: &dyn Pool, width: u16, claim: u64, params: &[Datum], abort: &AtomicU8, sink: &mut LegSink) -> Result<u64> {
    run_with(entry, program, derive, pool, width, claim, params, abort, sink, None)
}

pub fn run_keyed(entry: &mut Entry, program: &Arc<Program>, derive: &mut FixtureDerive, pool: &dyn Pool, width: u16, claim: u64, params: &[Datum], abort: &AtomicU8, sink: &mut LegSink) -> Result<u64> {
    let key = gravity_program::PlanKey { handle: gravity_program::Handle(1), generation: gravity_program::Generation(1) };
    run_with(entry, program, derive, pool, width, claim, params, abort, sink, Some(key))
}

pub const WIDTHS: [u16; 4] = [1, 2, 4, 8];

pub fn gen_a(r: u64) -> Option<[i64; 2]> {
    Some([(r % 1009) as i64, 0])
}

pub fn gen_kv(r: u64) -> Option<[i64; 2]> {
    Some([(r % 97) as i64 - 3, (r % 1000) as i64])
}

pub fn gen_inner(r: u64) -> Option<[i64; 2]> {
    if r % 5 == 0 {
        Some([(r % 37) as i64, (r * 10) as i64])
    } else {
        None
    }
}

pub fn gen_outer(r: u64) -> Option<[i64; 2]> {
    Some([(r % 37) as i64, r as i64])
}


// ---- fixture kernels for §2 commits 3–4: a share-budgeted group-by and a two-slot emit

/// `key → count, sum(v)` over lane 0 / lane 1 into a share-budgeted table (opens at 16 records).
pub const GROUP: RecordLayout = RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count, SlotKind::SumI64]);

pub unsafe extern "C-unwind" fn k_group_share(ctx: *mut Ctx) -> i64 {
    if range_seek(ctx, 0) < 0 {
        return ERROR;
    }
    let table = HashAgg::at(partial(ctx), &GROUP, 16, SHARE);
    loop {
        let r = advance(ctx, 0);
        if r <= 0 {
            return if r == 0 { DONE } else { r };
        }
        if interrupted(ctx) {
            return CANCELLED;
        }
        let s = &mut *site_ptr(ctx, 0);
        let (lk, lv) = (lane(s, 0), lane(s, 1));
        for i in s.cursor as usize..end(s) as usize {
            match table.find_or_insert(&[lk.datum(i)]) {
                Some(rec) => {
                    rec.add_count(0, 1);
                    rec.add_i64(1, lv.datum(i) as i64);
                }
                None => return unsupported(UNSUPPORTED_SCRATCH),
            }
        }
        s.rows_out += (end(s) - s.cursor) as u64;
        s.cursor = end(s);
    }
}

pub unsafe extern "C-unwind" fn k_emit_groups(ctx: *mut Ctx) -> i64 {
    let table = HashAgg::at(partial(ctx), &GROUP, 16, SHARE);
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
        if f == HOST_STOP {
            (*ctx).resume = i as i64;
            return stopped(ctx);
        }
    }
    done(ctx)
}

/// `(count0 + count1)` from the two consumed one-record tables of `par_scan_count`'s layout.
pub unsafe extern "C-unwind" fn k_emit_two(ctx: *mut Ctx) -> i64 {
    use gravity_kernels::par_scan_count::{BYTES, LAYOUT};
    let a = HashAgg::at(consumed(ctx, 0), &LAYOUT, 1, BYTES).single().count(0);
    let b = HashAgg::at(consumed(ctx, 1), &LAYOUT, 1, BYTES).single().count(0);
    let v = [(a + b) as u64, a as u64, b as u64];
    let n = [0u8; 3];
    if emit(ctx, 3, &[(v.as_ptr(), n.as_ptr(), 3)]) < 0 {
        return ERROR;
    }
    if flush(ctx) < 0 {
        return ERROR;
    }
    DONE
}

fn layout3(nsites: u16, out_natts: u16) -> Layout {
    Layout { nsites, out_natts, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }
}

fn int8_col(name: &str) -> gravity_program::OutCol {
    gravity_program::OutCol::new(name, 20, -1, 8, true)
}

/// `SELECT k, count(*), sum(v) GROUP BY k` over `(k, v)` with a share-budgeted table in slot 0.
pub fn p_group_share(rel: u32, rows: u64) -> Program {
    let st = gravity_program::Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), rel, &[1, 2])];
    let out = [int8_col("k"), int8_col("count"), int8_col("sum")];
    let breaker = Breaker { kind: BreakerKind::HashAgg, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::None };
    let pipes = [(Pipeline::eligible(0, End::Breaker(breaker), rows), Code::hand(k_group_share)), (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_emit_groups))];
    Program::with_pipelines(layout3(1, 3), &sites, &out, WritePlan::SELECT, pipes)
}

/// Two counting producers (slots 0 and 1, `par_scan_count` over the same relation) into one emit.
pub fn p_two_slots(rel: u32, rows: u64) -> Program {
    use gravity_kernels::par_scan_count::{par_scan_count, BYTES};
    let st = gravity_program::Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), rel, &[1])];
    let out = [int8_col("both"), int8_col("a"), int8_col("b")];
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, BYTES).in_slot(0)), rows), Code::hand(par_scan_count)),
        (Pipeline::eligible(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, BYTES).in_slot(1)), rows), Code::hand(par_scan_count)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0, 1]), Code::hand(k_emit_two)),
    ];
    Program::with_pipelines(layout3(1, 3), &sites, &out, WritePlan::SELECT, pipes)
}

/// `run_sized` under a grant and a caller's plan (the partition tests' ledgered merge).
pub fn run_planned(entry: &mut Entry, program: &Arc<Program>, derive: &mut FixtureDerive, pool: &dyn Pool, plan: &dyn Plan, grant: gravity_driver::Grant, params: &[Datum], abort: &AtomicU8, sink: &mut LegSink) -> Result<u64> {
    run_planned_in(entry, program, derive, pool, plan, lease(grant), params, abort, sink)
}

/// `run_planned` under an admitted statement's lease.
#[allow(clippy::too_many_arguments, dead_code)]
pub fn run_planned_in(entry: &mut Entry, program: &Arc<Program>, derive: &mut FixtureDerive, pool: &dyn Pool, plan: &dyn Plan, lease: std::rc::Rc<gravity_driver::Reservation>, params: &[Datum], abort: &AtomicU8, sink: &mut LegSink) -> Result<u64> {
    let nulls = [0u8; 16];
    let mut b = Binding::new(parts(params, &nulls[..params.len().max(1)], abort), sink);
    let mut exec = entry.start(None, program, derive, &mut b, lease)?;
    let r = run_program(entry, &mut exec, &mut b, derive, pool, plan);
    entry.end(exec, r.is_ok());
    r
}

// ---- an in-memory spill store for the tests that spill (the driver's fixture, growable)

pub struct MemSpillFile {
    parts: Vec<Vec<u8>>,
    bytes: u64,
}

impl gravity_driver::SpillFile for MemSpillFile {
    fn write_epoch(&mut self, parts: &[(u32, &[u8])]) -> Result<()> {
        for (part, bytes) in parts {
            self.parts[*part as usize].extend_from_slice(bytes);
            self.bytes += bytes.len() as u64;
        }
        Ok(())
    }

    fn read_part(&self, part: u32, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
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

impl gravity_driver::SpillSet for MemSpillSet {
    fn open(&mut self, _purpose: &str, _generation: u64, _worker: usize, nparts: u32) -> Result<Box<dyn gravity_driver::SpillFile>> {
        Ok(Box::new(MemSpillFile { parts: (0..nparts).map(|_| Vec::new()).collect(), bytes: 0 }))
    }
}

pub struct MemSpillFactory;

/// Spill sets created so far: one per Node that spilled, however many workers spilled at once.
pub static SPILL_SETS_CREATED: AtomicUsize = AtomicUsize::new(0);

impl gravity_driver::SpillFactory for MemSpillFactory {
    fn create(&self) -> Result<Box<dyn gravity_driver::SpillSet>> {
        SPILL_SETS_CREATED.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(MemSpillSet))
    }

    /// D491: the serial owners' chunk past the pool, a file under the system temp dir mapped shared and unlinked at once
    /// (the server's is a session temp file: pool lib.rs `SpillSets::map_temp`).
    fn map_temp(&self, bytes: usize) -> Result<Option<gravity_driver::TempMap>> {
        use std::os::fd::AsRawFd;
        static FILES: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!("gravity-test-owners-{}-{}", std::process::id(), FILES.fetch_add(1, Ordering::Relaxed)));
        let f = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path).map_err(|e| Error::host(format!("test owner map: {e}")))?;
        let _ = std::fs::remove_file(&path);
        f.set_len(bytes as u64).map_err(|e| Error::host(format!("test owner map: {e}")))?;
        Ok(gravity_driver::TempMap::shared(f.as_raw_fd(), bytes))
    }
}

pub static MEM_SPILL: MemSpillFactory = MemSpillFactory;

pub fn install_mem_spill() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        gravity_driver::install_spill_factory(&MEM_SPILL);
    });
}

/// `run_planned` reporting whether the execution spilled and how many partitions / generations slot 0 reached.
pub fn run_spilling(entry: &mut Entry, program: &Arc<Program>, derive: &mut FixtureDerive, pool: &dyn Pool, plan: &dyn Plan, grant: gravity_driver::Grant, params: &[Datum], abort: &AtomicU8, sink: &mut LegSink) -> Result<(u64, bool, u32, u8)> {
    run_spilling_in(entry, program, derive, pool, plan, lease(grant), params, abort, sink)
}

/// `run_spilling` under an admitted statement's lease.
#[allow(clippy::too_many_arguments, dead_code)]
pub fn run_spilling_in(entry: &mut Entry, program: &Arc<Program>, derive: &mut FixtureDerive, pool: &dyn Pool, plan: &dyn Plan, lease: std::rc::Rc<gravity_driver::Reservation>, params: &[Datum], abort: &AtomicU8, sink: &mut LegSink) -> Result<(u64, bool, u32, u8)> {
    let nulls = [0u8; 16];
    let mut b = Binding::new(parts(params, &nulls[..params.len().max(1)], abort), sink);
    let mut exec = entry.start(None, program, derive, &mut b, lease)?;
    let r = run_program(entry, &mut exec, &mut b, derive, pool, plan);
    let census = (exec.node().spilled(), exec.node().spill_parts_of(0), exec.node().spill_generations());
    entry.end(exec, r.is_ok());
    r.map(|rows| (rows, census.0, census.1, census.2))
}

std::thread_local! {
    static LEASES: std::cell::RefCell<Vec<(u64, std::rc::Rc<gravity_driver::Reservation>)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// An unadmitted statement lease at `grant`, one per grant per thread (as the driver rigs' `lease`): a clone never allocates,
/// so the allocation-counting rigs (zero_alloc) see nothing after the first run.
pub fn lease(grant: gravity_driver::Grant) -> std::rc::Rc<gravity_driver::Reservation> {
    LEASES.with(|l| {
        let mut l = l.borrow_mut();
        if let Some((_, r)) = l.iter().find(|(g, _)| *g == grant.bytes) {
            return r.clone();
        }
        let r = std::rc::Rc::new(gravity_driver::Reservation::unadmitted(grant));
        l.push((grant.bytes, r.clone()));
        r
    })
}
