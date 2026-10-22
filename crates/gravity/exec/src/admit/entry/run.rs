//! `executor_run` over the runner: `Entry::run` through the pull adapter into the dest receiver (docs/gravity/entry.md §3).

use std::rc::Rc;
use std::time::Instant;

use gravity_driver::mem::root::Root;
use gravity_driver::{Error, PullAdapter};
use crate::receivers::DestSink;
use crate::runner::{desc_of, eval_keys, run_inits};
use crate::{glue, Admitted};
use gravity_program::Cmd;
use mcx::Mcx;
use tcop_dest::DestReceiver;
use types_error::PgResult;
use types_scan::sdir::{ScanDirection, ScanDirectionIsBackward, ScanDirectionIsNoMovement};
use types_slot::EXEC_FLAG_REWIND;
use types_tuple::TupleDescData;

use crate::admit::entry::{QueryDesc, VIA, VIA_NESTED, VIA_REARMED};

/// D2044: a fetch's answer once user code's raise left its state behind (`Serve::step_counted`): the held error ends it.
const PL_LEFT: &str = "user code's raise left SPI, GUC or subtransaction state behind (D2044)";

/// One served statement's execution state, boxed in the QueryDesc (address-stable for the adapter's borrows).
pub struct Serve {
    /// D1509: the rows this execution sent its receiver, counted at the step's sink (an error keeps the count)
    pub(crate) sent: u64,
    /// D2355: this executor_run call started its receiver (a fall-back rerun would start it again)
    pub(crate) started: bool,
    /// D1216: boxed from its admission or its keyed take to its release (a 672-byte move at each hop before)
    pub a: Box<Admitted>,
    pub cmd: Cmd,
    pub shape: &'static str,
    pub run: PullAdapter<'static>,
    /// D1216: the key words, sized to the admission's words (`runner::key_words`: a graph admission's `nkparams`, a shape
    /// table's key slots). Inline arrays made the Serve 6.3 KB, copied twice per execution (a nested SPI statement's ~0.34 us).
    kparams: Vec<u64>,
    knulls: Vec<u8>,
    keys_done: bool,
    /// Append arms completed after the main (rung 12 need 3): the next to open is `a.graph.arms[arm]`.
    arm: usize,
    /// The snapshot the run opened under and the statement's `output_cid`, taken at ExecutorStart (the arms reopen under both).
    snap: *const Rc<types_snapshot::SnapshotData<'static>>,
    cid: u32,
    pub tupdesc: Option<Rc<TupleDescData<'static>>>,
    /// Boxed: an `Mcx` points into the context object, which must not move (as `PortalData` boxes its own).
    ctx: Root,
    /// Rows this ExecutorRun handed to the receiver (C's `es_processed`).
    pub es_processed: u64,
    /// Rows over the statement (the completion tag's count; writes count the host's rows).
    pub rows: u64,
    pub entries: u64,
    pub done: bool,
    pub elapsed_ns: u64,
    /// Per site `(rows_in, rows_out)` read when the execution completed (EXPLAIN ANALYZE's cells).
    pub site_rows: Vec<(u64, u64)>,
    /// Per site the heap relation it reads (the leaf ↔ site map of EXPLAIN ANALYZE).
    pub site_rels: Vec<u32>,
    /// Per site an index-only walk's heap fetches over this execution (EXPLAIN ANALYZE's Heap Fetches, D574): the
    /// recipe's count at completion less `fetch_base`, its count when the execution began (a reused admission's recipe
    /// keeps counting); `None` for any other site.
    pub site_fetches: Vec<Option<u64>>,
    fetch_base: Vec<u64>,
    /// The workers the run's sections ran (its widest section's, 0 when none ran wide): pg_stat_database's
    /// parallel_workers_launched at the end (entry `executor_end`).
    pub workers: u16,
    /// M12 Row B: per site a bitmap walk's counters over this execution (hosts v4/m12.rs `BitmapCounts`: tuples, exact /
    /// lossy pages, recheck-removed rows), less `bitmap_base` as `site_fetches`; `None` for any other site.
    pub site_bitmap: Vec<Option<[u64; 5]>>,
    bitmap_base: Vec<[u64; 5]>,
    /// D1536: per site the rows its zone verdicts skipped over this execution (hosts granule_source `GranuleRecipe::skipped`,
    /// an EXPLAIN ANALYZE execution's fresh count; 0 for any other site), every one a row the scan's qual removes.
    pub site_skipped: Vec<u64>,
    pub instr: Vec<u64>, // the counter cells read at completion (`Program.instr`'s order)
    /// M12 Row D (D1717): pgBufferUsage's delta over this execution's runs (the lowering and compile are outside them).
    pub bufusage: types_core::instrument::BufferUsage,
    /// An EXPLAIN ANALYZE's record (docs/gravity/entry.md §4): its admission's at the start, its run's at the end, read by the
    /// printer through instrument.rs; `None` for every other execution. The program and the road are the plain run's.
    pub report: Option<Box<types_core::instrument::GravityReport>>,
    /// The re-entrancy level the statement was admitted at and whether a program of it holds host state (reentry.rs).
    pub home: usize,
    hosted: bool,
    /// The plan key a generic cached plan's program is filed under at free (keyed.rs).
    pub key: Option<gravity_program::PlanKey>,
    /// A completion dest (complete.rs) takes a parallel program to the driver; every other caller pulls it, its `Producers` (runner.rs) run first at the open and dropped with the close.
    pub completion: bool,
    /// D866: a table fill (CREATE TABLE AS, SELECT INTO, REFRESH MATERIALIZED VIEW) whose program evaluates a VOLATILE call
    /// (driver pull.rs `pull_exact`): PG inserts each row before the next row's calls, so a call that reads the target sees the
    /// rows before it. The pull takes one row per fetch (the adapter's D439 cap: no call on a row past it) and the receiver
    /// inserts it before the next; a parallel program is pulled too, never driven.
    pub fill_rows: bool,
    /// D622: the grants a nested statement reserved at its start (its caller holds one), handed to its sites while it runs.
    pub nested: Option<Rc<core::cell::RefCell<crate::runner::NestedGrants>>>,
    producers: Option<Box<crate::runner::Producers>>,
    /// D264: the granule relations of a pull no producers resolved (`resolve_pull`), kept while the adapter runs.
    pulled: Option<Box<dyn core::any::Any + Send + Sync>>,
    /// D280: that pull's v4 protocols and the words they leave for its binding (`pull_words`), kept while it runs.
    serial: Option<Box<crate::runner::SerialWords>>,
    /// The last pull stopped at the caller's count with the pipeline suspended (SPI's tcount): the run is served at its close.
    limited: bool,
    /// D682: the rows EvalPlanQual recheck nodes wrote in this execution (a write resumes past each rechecked row); a write's
    /// count adds them to the run's.
    rechecked: u64,
    via: &'static str,
    /// D421: the program's last two columns are the row identity (current_of.rs), stripped into the tid sidecar.
    ident: bool,
    /// D2281: an identity program's last row `(tableoid, packed tid)` when no store holds the sidecar (a non-scroll
    /// cursor): the row `WHERE CURRENT OF` names (current_of::current).
    pub(crate) last_tid: Option<(u32, u64)>,
    /// D424: the start of the statement whose FETCH line this serve last logged (current_of::fetch_served).
    pub(crate) fetched: i64,
    /// PERF-OLTP E2: an execution of a serve the portal retained (`rearm`): its served line says `via=entry:rearmed`.
    rearmed: bool,
    /// D588 / D590: this execution's parameter Limit rows (count + offset, the smallest over the walks) and whether a walk
    /// is root-bound (a FETCH's count bounds it: each step sets its run cell and the adapter asks for owed rows only).
    run_rows: Option<u64>,
    root_bound: bool,
    /// A named cursor's portal started this execution (DECLARE, a PL/pgSQL cursor, a named protocol portal): each FETCH
    /// asks the adapter for the rows it owes only, as PostgreSQL's ExecutePlan stops at the FETCH's count
    /// (execMain.c `numberTuples`, pquery.c PortalRunSelect); the rest of a block is never computed ahead of a FETCH that
    /// may not come. Set at every start (`entry::named_cursor`).
    pub(crate) named_cursor: bool,
    /// B2-31 / D2890: a portal a later FETCH or Execute can reach started this execution (`entry::fetch_portal`: a named
    /// cursor's, or the protocol's unnamed one), so a run under a row count may be resumed later, and each run computes only
    /// the rows it owes (`bind_runs`' owed cap). Set at every start.
    pub(crate) fetch_portal: bool,
    /// M8 (L0-k): the late-word images of the emits this execution pulled (`Producers::emit_image`), kept until the adapter closes.
    images: Vec<crate::runner::EmitImage>,
    /// D761: the relations the plan's ExecutorStart opens (`hold`), held to the QueryDesc's end; last, so they close last.
    held: crate::runner::held::Held,
    /// D2647: the InitPlan values the last run computed, kept from its close to a rewind's reopen (`keep_inits`).
    inits: crate::ops::subplan::initplan::init::Carried,
    /// D2701: this execution keeps its run open past its end, for a rewind that runs only the emit again (`soft_rewind`).
    soft: bool,
    /// D3881: the deep level the statement is homed at stays while this serve lives (reentry `Pin`).
    pub(crate) pin: crate::admit::reentry::Pin,
}

/// D2798: the publishing mark (`admit::publishing`) over an identity cursor's admission whose plan calls user code,
/// cleared on every exit.
pub(crate) struct Publishing(bool);

impl Publishing {
    pub(crate) fn mark(on: bool) -> Publishing {
        Publishing(on && !crate::admit::publishing(true))
    }
}

impl Drop for Publishing {
    fn drop(&mut self) {
        if self.0 {
            crate::admit::publishing(false);
        }
    }
}

leader_tls::leader_tls! {
    thread_local! {
        // D1506: the descs' context (a leaked Box, no TLS destructor), dropped by `retire_descs` in the session's Roots phase
        static DESC_CTX: core::cell::Cell<*mut Root> = const { core::cell::Cell::new(core::ptr::null_mut()) };
        static DESC_GONE: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    }
}

/// The result descriptors' context: a caller keeps its `Rc` clone past the desc (a portal, COPY), so the bytes live in a
/// session-lifetime context and the last clone's drop returns them. D1506: made at the first desc with its drop registered
/// at the session's Roots phase (after every portal's drop); a desc asked for after that is an error.
fn desc_mcx() -> PgResult<Mcx<'static>> {
    let mut m = DESC_CTX.get();
    if m.is_null() {
        if DESC_GONE.get() {
            return Err(glue::internal("gravity_entry: a result descriptor after the session's teardown (D1506)"));
        }
        m = Box::into_raw(Box::new(Root::new("GravityEntryDescs"))); // alloc: Cold
        DESC_CTX.set(m);
        mcx::register_session_cleanup_phase(mcx::SessionCleanupPhase::Roots, Box::new(retire_descs)); // alloc: Cold
        // D4682: a reused client backend thread's next session may make descs again
        mcx::register_thread_reuse_hook(rearm_descs);
    }
    Ok(super::raw::descs_mcx(m))
}

/// D1506: the Roots-phase end of the descs' context. It is freed only when no desc is left in it (a clone still held keeps
/// its bytes: the context is left allocated, as before this row).
/// D4682: the D1506 refusal is for the session the teardown ended; a reused thread's next session starts unrefused.
fn rearm_descs() {
    DESC_GONE.set(false);
}

fn retire_descs() {
    DESC_GONE.set(true);
    let m = DESC_CTX.replace(core::ptr::null_mut());
    super::raw::free_descs(m);
}

impl Serve {
    pub fn new(a: Box<Admitted>, ctx: Root) -> PgResult<Box<Serve>> {
        let cid = crate::output_cid::output_cid(&a)?;
        // a SELECT's columns, or a RETURNING write's (the runner's `emits_rows`): the receiver gets rows either way
        let tupdesc = (a.cmd == Cmd::Select || a.out_natts > 0).then(|| desc_mcx().map(|m| desc_of(m, &a))).transpose()?;
        let natts = a.program.out.len();
        let cap = a.program.layout.out_rows as usize;
        let fetch_base = (0..a.program.sites.len()).map(|i| a.registry.recipe(i).map_or(0, |r| r.heap_fetches())).collect(); // alloc: Exec
        let bitmap_base = (0..a.program.sites.len()).map(|i| a.registry.bitmap_recipe(i).or_else(|| a.registry.recipe(i)).map_or([0; 5], |r| r.bitmap().load())).collect(); // alloc: Exec
        let nw = crate::runner::key_words(&a);
        let hosted = a.program.owner.is_some() || a.registry.owner_of().is_some() || a.graph.as_ref().is_some_and(|g| g.arms.iter().any(|x| x.program.owner.is_some()) || g.inits.iter().any(|x| x.program.owner.is_some()));
        Ok(Box::new(Serve { sent: 0, started: false, home: 0, key: None, completion: false, fill_rows: false, nested: None, producers: None, pulled: None, serial: None, limited: false, rechecked: 0, via: VIA, hosted, cmd: a.cmd, shape: a.shape, run: PullAdapter::new(natts, cap), a, kparams: vec![0; nw], knulls: vec![0; nw], keys_done: false, arm: 0, snap: core::ptr::null(), cid, tupdesc, ctx, es_processed: 0, rows: 0, entries: 0, done: false, elapsed_ns: 0, site_rows: Vec::new(), site_rels: Vec::new(), site_fetches: Vec::new(), site_skipped: Vec::new(), fetch_base, workers: 0, site_bitmap: Vec::new(), bitmap_base, instr: Vec::new(), bufusage: Default::default(), report: None, ident: false, last_tid: None, fetched: i64::MIN, rearmed: false, run_rows: None, root_bound: false, named_cursor: false, fetch_portal: false, images: Vec::new(), held: Default::default(), inits: Default::default(), soft: false, pin: crate::admit::reentry::Pin::NONE })) // alloc: Exec
    }

    /// D761: the relations the plan's ExecutorStart opens, held from the served ExecutorStart on (before any key
    /// expression, InitPlan or pipeline runs), as PostgreSQL's executor holds them.
    pub fn hold(&mut self, params: types_portal::ParamListHandle) -> PgResult<()> {
        let (Some(pstmt), Some(g)) = (self.a.pstmt, self.a.graph.as_ref()) else { return Ok(()) };
        self.held = crate::runner::held::take(self.mcx(), pstmt, &g.held, params, None)?;
        Ok(())
    }

    /// The served line's `code=`: the first pipeline's code as this execution ran it, read at the line (a tier-0 code
    /// promotes at `Entry::start`, after `new`; a retained serve's program promotes under it)
    pub(crate) fn code(&self) -> &'static str {
        self.a.program.code_of(0).kind()
    }

    /// D2355: a program of this admission holds host state (`Program::owner`) that a pipeline call on this thread's stack
    /// runs now (driver `owner_running`): a run of it away from its home level, inside that call's host call, would share
    /// the state with the running statement. A hosted program whose state no running call holds runs away from a running
    /// home as an unhosted one does.
    pub(crate) fn host_busy(&self) -> bool {
        let running = |o: &dyn core::any::Any| gravity_driver::node::owner_running(o as *const dyn core::any::Any as *const u8);
        let busy = |p: &gravity_program::Program| p.owner.as_deref().is_some_and(running);
        self.hosted && (busy(&self.a.program) || self.a.registry.owner_of().is_some_and(running) || self.a.graph.as_ref().is_some_and(|g| g.arms.iter().any(|x| busy(&x.program)) || g.inits.iter().any(|x| busy(&x.program))))
    }

    pub(crate) fn cid(&self) -> u32 {
        self.cid
    }

    /// PERF-OLTP E2: a serve the portal may retain across executions (`executor_finish_and_park`): a generic plan's SELECT
    /// filed under its plan key, without row locks (its output command id is 0, never the next execution's), not an
    /// EXPLAIN ANALYZE, an identity copy or a completion run.
    pub(crate) fn retainable(&self) -> bool {
        self.cmd == Cmd::Select && self.cid == 0 && self.shelvable() && !self.ident
    }

    /// D3865 L2: a serve the shelf may keep between a nested statement's executions (shelf.rs): filed under its plan key,
    /// not an EXPLAIN ANALYZE or a completion run, whatever its command and row locks (its output command id is read again
    /// at each revival, `recid`). D3865 L5: an identity copy's too, filed under the plan's identity key.
    pub(crate) fn shelvable(&self) -> bool {
        self.key.is_some() && !self.completion && self.report.is_none()
    }

    /// D3865 L2: a revived serve's output command id, ExecutorStart's `es_output_cid` for this execution (a write or a row
    /// lock between two executions of the body sees the commands since).
    pub(crate) fn recid(&mut self) -> PgResult<()> {
        self.cid = crate::output_cid::output_cid(&self.a)?;
        Ok(())
    }

    /// PERF-OLTP E2: the next execution of a retained serve: the per-execution state back to `new`'s and the context
    /// emptied; the filed program, the adapter, the result descriptor and the key stay (the keys and InitPlans are
    /// evaluated again at the open under the next execution's params).
    pub(crate) fn rearm(&mut self) {
        self.shed();
        self.sent = 0;
        self.rearmed = true;
    }

    /// PERF-OLTP E2 (V4R-116 condition 2): the per-execution state dropped and the context emptied, at the park (a parked
    /// serve keeps only its filed program, adapter and result descriptor) and again at the rearm. The producers, the
    /// pulled relations and the serial words go before the reset: they may hold the context's memory.
    pub(crate) fn shed(&mut self) {
        debug_assert!(!self.run.is_open(), "shed of an open serve");
        self.producers = None;
        self.pulled = None;
        self.serial = None;
        self.nested = None; // D726: a parked serve never holds a grant a completed run kept
        self.keys_done = false;
        self.limited = false;
        self.rechecked = 0;
        self.arm = 0;
        self.snap = core::ptr::null();
        self.es_processed = 0;
        self.rows = 0;
        self.entries = 0;
        self.done = false;
        self.elapsed_ns = 0;
        self.bufusage = Default::default();
        self.fetched = i64::MIN;
        self.inits = Default::default();
        self.soft = false;
        // D3865 L5: a revived identity serve is on no row until its first fetch (execCurrentOf's "not positioned")
        self.last_tid = None;
        self.ctx.reset();
    }

    pub(crate) fn mcx(&self) -> Mcx<'static> {
        super::raw::serve_mcx(&self.ctx)
    }

    // D3620: out of line, so `run_step`'s frame (on every nested statement's stack path) does not hold its locals
    #[inline(never)]
    fn open(&mut self, entry: &mut gravity_driver::Entry, snapshot: &Rc<types_snapshot::SnapshotData<'static>>, params: types_portal::ParamListHandle, amcx: Mcx<'static>) -> PgResult<()> {
        let cid = self.cid;
        let nk = self.a.nkparams;
        if !self.keys_done {
            if !eval_keys(&mut self.a, params, &mut self.kparams, &mut self.knulls, amcx, false)? {
                return Err(glue::internal("gravity_entry: key parameters"));
            }
            // a composed statement's InitPlans once, at the first execution (a cursor's rewind keeps them)
            // M8 (RT-1): a program declaring nest.initplan's Init runs its InitPlans itself, each at its param's first read
            if !init_protos(&self.a.program) {
                run_inits(amcx, entry, &mut self.a, snapshot, cid, &mut self.kparams[..nk], &mut self.knulls[..nk])?;
            }
            // exec pruning over the InitPlans' words (rung 12 need 13): the arms decided again, once per execution
            crate::ops::append::prune::exec_skip(&mut self.a, &self.kparams[..nk], &self.knulls[..nk])?;
            self.keys_done = true;
        }
        let s: &'static mut Serve = super::raw::serve_for_run(self);
        let snap: &'static Rc<types_snapshot::SnapshotData<'static>> = super::raw::snapshot(snapshot);
        // a multi-pipeline (or eligible) program: its producers run to completion after the open, the pull takes the emit (chore C1)
        s.producers = if s.a.is_parallel() { Some(crate::runner::Producers::prepare(s.mcx(), &mut s.a, snap, &s.kparams[..nk], &s.knulls[..nk])?) } else { None };
        // D264: a pull no producers resolve (a serial program over a granule relation) resolves its own, kept as they are
        s.pulled = if s.producers.is_none() { crate::runner::resolve_pull(s.mcx(), &s.a.registry, snap)? } else { None };
        // D3663: the relation sets its Appends prune at the start, their survivors filed for the bind
        crate::runner::file_pull_sets(s.mcx(), &s.a.program, &s.a.registry, s.a.pstmt, crate::ops::append::proto::Initial::params_of(&s.a), s.pulled.as_deref_mut())?;
        // D280: its v4 protocols too (a serial pgrc program's src.verdicts), their words bound below
        s.serial = if s.producers.is_none() { crate::runner::pull_words(s.mcx(), &s.a, snap, &s.kparams[..nk], &s.knulls[..nk])? } else { None };
        let resolved: Option<&'static (dyn core::any::Any + Send + Sync)> = s.producers.as_deref().map(|p| super::raw::resolved(p.resolved())).or(s.pulled.as_deref());
        // D274 / D280: the words the producers' plan or the serial protocols bound (the facts and protocol words past the
        // key params), as drive's, held as `resolved` is (raw.rs `bound_words`).
        let (kp, kn) = match s.producers.as_deref().map(|p| p.words()).or(s.serial.as_deref().map(|w| w.words())) {
            Some((w, n)) => super::raw::bound_words(w, n),
            None => (&s.kparams[..nk], &s.knulls[..nk]),
        };
        crate::ops::subplan::initplan::init::carry_in(kp, &mut s.inits)?;
        let parts = glue::binding_parts(snap, cid, kp, kn, resolved);
        s.a.registry.rewind();
        s.arm = 0;
        s.snap = snap;
        // PERF-OLTP E1: a statement's Node parks and its next execution takes it (Node::build and teardown once). NEW-57: under
        // the simple protocol's content park key where its audit allows (runner `park_key`: the program's content and the salt
        // of its recipes; C3 S4c: a parked Node serves only the program it was built over), as that protocol's runs do, so a simple-query DECLARE, an unsaved SPI plan and a
        // statement Parsed again for each execution (a new plan key every time) take the Node the last one parked; else under
        // its plan key. The identity copy and a completion run keep the plan key alone (unkeyed: built and
        // torn down)
        let key = if s.ident || s.completion { s.key } else { crate::runner::park_key(s.mcx(), &s.a).or(s.key) };
        let run = &mut s.run;
        let lease = s.producers.as_deref().map_or_else(|| crate::runner::unadmitted(1), |p| p.reservation());
        run.open(entry, key, &s.a.program, &mut s.a.registry, parts, lease).map_err(glue::pg_error)?;
        self.bind_runs(params);
        if self.producers.is_some() {
            self.run.set_external_producers(true);
            // M8 (L0-k): an Init program's emits each take their late step before their first call (`emit_late`); so does a
            // program with a bitmap morsel site, whose late step builds the bitmap an emit's claims walk (D3051)
            let morsel = (0..self.a.program.sites.len()).any(|i| self.a.registry.bitmap_morsel(i).is_some());
            self.run.set_stepwise(init_protos(&self.a.program) || morsel);
            if let Err(e) = self.run_producers(entry) {
                self.run.close(entry, false);
                self.images.clear();
                return Err(e);
            }
        }
        Ok(())
    }

    /// D588 / D590: each index walk's run cell at the open: a parameter Limit's count + offset from this execution's params
    /// (a generic plan's `LIMIT $1 OFFSET $2`), nothing new otherwise (a parked walk's last value cleared); a root-bound walk
    /// makes the adapter ask for the rows each FETCH owes (`step` sets the cells from its count).
    fn bind_runs(&mut self, params: types_portal::ParamListHandle) {
        let param = |id: u32| -> Option<(u64, bool)> {
            if params.is_null() {
                return None;
            }
            types_portal::params::with(params, |ps| ps.get(id as usize - 1).map(|p| (p.value.as_u64(), p.isnull)))
        };
        let (mut rows, mut root) = (None::<u64>, false);
        for i in 0..self.a.program.sites.len() {
            let Some(r) = self.a.registry.recipe(i) else { continue };
            let b = r.run_bound();
            let n = b.limit.and_then(|(c, o)| Some(c.value(param)?.max(0) as u64 + o.value(param).unwrap_or(0).max(0) as u64));
            r.set_run(n);
            rows = match (rows, n) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            root |= b.root;
        }
        self.run_rows = rows;
        self.root_bound = root;
        // D2890: any portal a later FETCH or Execute reaches (a named cursor's, the protocol's unnamed one under max_rows, an
        // SPI fetch of it) asks for the rows each run owes only: PostgreSQL computes a row in the run that reads it, so a SET
        // between two runs (TimeZone, DateStyle) reaches the rows after it. A run that reads every row (count 0) is unchanged.
        // D2230: so does every run of a program holding a recursive union a qual or a join may drop rows of (an SPI count, a
        // SQL function's lazy row): a block it asks for may never fill over a non-terminating union (`runner::thin_pull`)
        self.run.set_owed_cap(root || self.fetch_portal || crate::runner::thin_pull(&self.a));
    }

    /// D588: a root-bound walk's run cell at a step of `count` rows (0 = all: nothing new), no more than the parameter Limit's.
    fn step_runs(&self, count: u64) {
        if !self.root_bound || count == 0 {
            return;
        }
        let n = self.run_rows.map_or(count, |r| r.min(count));
        for i in 0..self.a.program.sites.len() {
            if let Some(r) = self.a.registry.recipe(i).filter(|r| r.run_bound().root) {
                r.set_run(Some(n));
            }
        }
    }

    /// One executor_run step inside the statement's level (`reentry::enter`): the open at the first, the pull, the close at
    /// the end (D622: split out so a nested statement's step runs with its grants handed, `runner::hand`).
    #[allow(clippy::too_many_arguments)]
    fn run_step(&mut self, b: &mut crate::Backend, depth: usize, count: u64, dest: &mut DestReceiver<'_>, snap_ptr: *const Rc<types_snapshot::SnapshotData<'static>>, params: types_portal::ParamListHandle, instrumented: bool, rewindable: bool) -> PgResult<()> {
        let amcx = b.admit_mcx();
        self.via = if depth > 0 { VIA_NESTED } else if self.rearmed { VIA_REARMED } else { VIA };
        if !self.run.is_open() {
            self.open(&mut b.entry, super::raw::snapshot(snap_ptr), params, amcx)?;
            // D2701: a rewindable run of a program marked `soft_rewind` keeps the emit's input slots for its rewind
            self.soft = rewindable && self.producers.is_some() && soft_rewind(&self.a);
            if self.soft {
                self.run.keep_for_rewind(&mut b.entry);
            }
        }
        self.lease_slot(false);
        let complete = match self.step(&mut b.entry, count, dest) {
            Ok(c) => c,
            Err(e) => {
                let _ = self.close_run(&mut b.entry, false);
                self.done = true;
                return Err(e);
            }
        };
        if !complete {
            self.lease_slot(true);
        }
        if complete {
            return self.complete_step(b, instrumented, rewindable);
        }
        Ok(())
    }

    /// `run_step`'s end of a run that completed. D3620: out of line, so `run_step`'s frame (on every nested statement's
    /// stack path) does not hold its locals.
    #[inline(never)]
    fn complete_step(&mut self, b: &mut crate::Backend, instrumented: bool, rewindable: bool) -> PgResult<()> {
        // D2647: only a portal with EXEC_FLAG_REWIND is ever rewound (pquery DoPortalRewind, a SCROLL hold's persist)
        if rewindable {
            self.keep_inits();
        }
        if instrumented {
            self.snapshot_sites();
        }
        // D2701: a soft run stays open, its slot parked, until its rewind runs the emit again or ExecutorEnd closes it
        if self.soft {
            self.lease_slot(true);
            self.done = true;
            let admit = if self.a.graph.is_some() { "graph" } else { "shape" };
            b.note_served_via(self.shape, self.rows, admit, self.code(), self.via);
            b.note_mechs(self.a.program.key.0, self.a.program.used);
            return Ok(());
        }
        // D726: a nested statement run under its handed grants that its portal may rewind keeps the grant its run held,
        // idle as a suspended portal's: given back, the rerun would reserve again beside its caller's grant, and one
        // that no longer fits could only wait on the caller's own bytes (D622)
        let kept = if rewindable && self.nested.is_some() && crate::admit::is_nested() {
            self.lease_slot(true);
            self.producers.take()
        } else {
            None
        };
        // D703: the kept producers' protocols end before the adapter closes, as `close_run` ends a run's own
        let pre = kept.as_deref().map_or(Ok(()), |p| p.end());
        let ok = pre.is_ok();
        let ended = pre.and(self.close_run(&mut b.entry, ok));
        if let Some(p) = kept {
            self.nested = Some(Rc::new(core::cell::RefCell::new(p.into_kept(&self.a.program))));
        }
        self.done = true;
        ended?;
        let admit = if self.a.graph.is_some() { "graph" } else { "shape" };
        b.note_served_via(self.shape, self.rows, admit, self.code(), self.via);
        b.note_mechs(self.a.program.key.0, self.a.program.used);
        // D376: an interrupt the teardown's spill walk left pending (held there) ends the statement
        crate::runner::check_after_teardown()?;
        Ok(())
    }

    /// D601: a suspended portal's in-flight slot given back (`park`) until its next fetch re-takes it; a pull no producers
    /// admitted holds none.
    fn lease_slot(&self, park: bool) {
        if let Some(p) = self.producers.as_deref() {
            p.set_running(!park); // D622: a suspended portal's lease is no caller's grant
        }
        if let Some(r) = self.producers.as_deref().map(|p| p.reservation()) {
            if park {
                r.park_slot();
            } else {
                r.unpark_slot();
            }
        }
    }

    /// M8 (L0-k): the late step of the emit the adapter stands before (its late words as an image, `Producers::emit_image`),
    /// then the emit readied for the pull. D3620: out of line, as `run_producers`, so the fetch loop's frame (on every nested
    /// statement's stack path) does not hold its locals.
    #[inline(never)]
    fn emit_late(&mut self, entry: &mut gravity_driver::Entry) -> PgResult<()> {
        if let Some(p) = self.producers.as_deref() {
            let registry: *mut _ = &mut self.a.registry;
            let images = &mut self.images;
            self.run.with_open(|exec, binding| {
                p.emit_image(entry, exec, binding, registry).map(|i| images.extend(i)) // alloc: Exec
            })?;
        }
        self.run.set_ready();
        Ok(())
    }

    /// The producers before the adapter's next emit (multi-emit: each arm's own producers run before it).
    #[inline(never)]
    fn run_producers(&mut self, entry: &mut gravity_driver::Entry) -> PgResult<()> {
        let Some(p) = self.producers.as_deref() else { return Ok(()) };
        let registry = &mut self.a.registry;
        self.run.with_open(|exec, binding| p.run(entry, exec, binding, registry))
    }

    /// D2647: the InitPlan values of the run about to close, from its live protocols' words, for a rewind's reopen (a
    /// WITH HOLD cursor's persist rewinds a run that ran to its end too); a run that ran none keeps nothing.
    fn keep_inits(&mut self) {
        if let Some(w) = self.producers.as_deref().map(|p| p.words().0).or(self.serial.as_deref().map(|w| w.words().0)) {
            self.inits = crate::ops::subplan::initplan::init::carry_out(&self.a.program, w);
        }
    }

    /// `ok`: the normal end (PostgreSQL's ExecutorEnd: the v4 protocols' end runs first); else an error's, which only drops.
    /// D3620: out of line, so `run_step`'s frame (on every nested statement's stack path) does not hold its locals.
    #[inline(never)]
    fn close_run(&mut self, entry: &mut gravity_driver::Entry, ok: bool) -> PgResult<()> {
        // M8 (RT-2f): PG's ExecPostprocessPlan before ExecutorEnd: each data-modifying pulled set runs to its end
        let drained = match self.producers.as_deref().filter(|_| ok) {
            Some(p) => {
                let registry: *mut _ = &mut self.a.registry;
                self.run.with_open(|x, b| p.drain_pulled(entry, x, b, registry))
            }
            None => Ok(()),
        };
        let ended = match drained {
            Err(e) => Err(e),
            Ok(()) if !ok => Ok(()),
            Ok(()) => self.serial.as_deref().map_or(Ok(()), |w| w.end()).and_then(|()| self.producers.as_deref().map_or(Ok(()), |p| p.end())),
        };
        self.run.close(entry, ended.is_ok() && ok);
        self.images.clear();
        self.producers = None;
        self.pulled = None;
        self.serial = None;
        // D622: a grant no site took (an arm pruned at the run) goes back with the run (a rewind keeps the producers' own);
        // D726: the emptied set goes too, so a rerun is handed only what a rewind or a rewindable completion kept
        if let Some(g) = self.nested.take() {
            g.replace(crate::runner::NestedGrants::default());
        }
        ended
    }

    /// The main (or an arm) ran to completion: the next Append arm reopens the adapter under the same binding; false without one.
    // D3620: out of line, so the fetch loop's frame (on every nested statement's stack path) does not hold its binding parts
    #[inline(never)]
    fn open_next_arm(&mut self, entry: &mut gravity_driver::Entry) -> PgResult<bool> {
        let s: &'static mut Serve = super::raw::serve_for_run(self);
        let nk = s.a.nkparams;
        let mcx = s.mcx();
        let (pstmt, params) = (s.a.pstmt, crate::ops::append::proto::Initial::params_of(&s.a));
        let Some(arm) = s.a.graph.as_mut().and_then(|g| {
            while g.arms.get(s.arm).is_some_and(|x| x.skip) {
                s.arm += 1; // a pruned arm (initial partition pruning at bind) is stepped over
            }
            g.arms.get_mut(s.arm)
        }) else { return Ok(false) };
        s.run.close(entry, true);
        crate::runner::check_after_teardown()?; // D376: a cancel or timeout landing between the Append arms ends the statement
        // `snap` is the desc's boxed snapshot set at `open`, alive while the Serve is
        s.pulled = crate::runner::resolve_pull(mcx, &arm.registry, super::raw::snapshot(s.snap))?;
        crate::runner::file_pull_sets(mcx, &arm.program, &arm.registry, pstmt, params, s.pulled.as_deref_mut())?;
        s.serial = None;
        let parts = glue::binding_parts(super::raw::snapshot(s.snap), s.cid, &s.kparams[..nk], &s.knulls[..nk], s.pulled.as_deref());
        arm.registry.rewind();
        s.run.open(entry, None, &arm.program, &mut arm.registry, parts, crate::runner::unadmitted(1)).map_err(glue::pg_error)?;
        s.arm += 1;
        Ok(true)
    }

    /// D2798: the row the running identity program published before that row's quals and projection (its RowOf site,
    /// lowerv4 frame/publish.rs), as `last_tid`'s `(tableoid, block << 16 | offset)`: what WHERE CURRENT OF reads while
    /// user code the run evaluates asks. `None` without the site or before a row published there (a source with no
    /// locator publishes none).
    pub(crate) fn published(&self) -> Option<(u32, u64)> {
        let e = self.run.exec()?;
        let n = e.node();
        let i = n.program().sites.iter().position(|d| matches!(d.kind, gravity_program::SiteKind::RowOf { .. }))?;
        let p = n.site_pos(i);
        (p.is_valid() && p.off != 0).then(|| (p.rel, ((p.block as u64) << 16) | p.off as u64))
    }

    /// D421: the program emits the row identity after the statement's columns: the descriptor keeps the statement's.
    pub(crate) fn identity(&mut self) -> PgResult<()> {
        let Some(td) = self.tupdesc.as_deref() else { return Err(glue::internal("gravity_entry: an identity program without rows")) };
        let n = td.natts - crate::admit::entry::current_of::IDENTITY as i32;
        self.tupdesc = Some(Rc::new(tupdesc::CreateTupleDescTruncatedCopy(desc_mcx()?, td, n)?)); // alloc: Cold
        self.ident = true;
        Ok(())
    }

    /// Up to `count` rows (0 = all) to the receiver; `true` when the statement completed.
    fn step(&mut self, entry: &mut gravity_driver::Entry, count: u64, dest: &mut DestReceiver<'_>) -> PgResult<bool> {
        let sent = core::cell::Cell::new(0u64);
        let last = core::cell::Cell::new(None);
        let r = self.step_counted(entry, count, dest, &sent, &last);
        self.sent += sent.get();
        if let Some(t) = last.get() {
            self.last_tid = Some(t);
        }
        r
    }

    /// The fetch's error as the statement's: D2044's `PL_LEFT` ends it in the adapter's held error (the program's own).
    /// D3620: out of line, as the fetch loop's other stops (`build_wait`, `recheck_stop`), so that loop's frame (on every
    /// nested statement's stack path) holds none of their conversions.
    #[inline(never)]
    fn ended(&mut self, e: Error) -> Box<types_error::PgError> {
        match e {
            Error::Host(m) if m == PL_LEFT => match self.run.take_failed() {
                Some(held) => glue::pg_error(held),
                None => glue::internal("gravity_entry: user code left SPI, GUC or subtransaction state behind with no error held (D2044)"),
            },
            e => glue::pg_error(e),
        }
    }

    fn step_counted(&mut self, entry: &mut gravity_driver::Entry, count: u64, dest: &mut DestReceiver<'_>, sent: &core::cell::Cell<u64>, last: &core::cell::Cell<Option<(u32, u64)>>) -> PgResult<bool> {
        let cmd = self.cmd;
        // PORTAL_ONE_RETURNING: a RETURNING write runs to completion before its rows (a count never suspends it)
        let want = if count == 0 || cmd != Cmd::Select { usize::MAX } else { count as usize };
        // D2297: a SELECT over a data-modifying CTE (SPI's tcount) runs its write to completion as PG's ExecutorFinish does,
        // but hands the receiver and counts (es_processed) only the first `count` rows, as PG's ExecutePlan stops there
        let cap = if count != 0 && cmd != Cmd::Select && self.a.tag() == Cmd::Select { count } else { u64::MAX };
        // D421: an identity program's rows leave their last two columns in the receiver's tid sidecar (D2281: without a
        // store, the last row's in `last`)
        let (strip, sidecar) = if self.ident { (crate::admit::entry::current_of::IDENTITY, dest.tuplestore_capture_sidecar()) } else { (0, None) };
        // D2044: user code's raise can leave state the statement cannot hold (runner `PlState`: PL/pgSQL returns its error
        // before its SPI_finish, its SPI connection still pushed), while the adapter holds the program's error behind the
        // rows it completed (pull.rs `failed`). Once that state moved, an SPI receiver takes none of those rows (spi_printtup
        // would meet the callee's connection: improper call to spi_printtup) and the fetch ends in the held error: PG's
        // abort frees what a failed fetch sent SPI, so no caller saw them
        let pl = crate::runner::PlState::now();
        let mut rows = RowSend { sent, last, cap, strip, sidecar, pl, spi: matches!(*dest, DestReceiver::SpiPrintTup), sink: self.tupdesc.clone().map(|td| DestSink::new(self.mcx(), td, dest)) };
        let mut consume = |v: &[u64], n: &[u8]| rows.row(v, n);
        let mut total = 0usize;
        self.limited = false;
        if cmd == Cmd::Select {
            self.step_runs(count);
        }
        let served = loop {
            let n = if self.fill_rows { 1 } else { want - total };
            let fetched = match self.producers.as_deref().filter(|_| on_demand_protos(&self.a.program)) {
                // M8 (RT-1, D1401): the producers' frame while the adapter pulls an emit, for an on-demand run in its calls
                Some(p) => {
                    let mut open = None;
                    let _ = self.run.with_open(|x, b| Ok::<(), ()>(open = Some((x as *mut gravity_driver::Exec, b as *mut gravity_driver::Binding<'_>))));
                    match open {
                        Some((x, b)) => p.with_frame(entry, x, b, &mut self.a.registry, |entry| self.run.fetch(entry, n, &mut consume)),
                        None => self.run.fetch(entry, n, &mut consume),
                    }
                }
                None => self.run.fetch(entry, n, &mut consume),
            };
            match fetched {
                Ok(n) => {
                    total += n;
                    // D866: the row landed in the fill; the next one's calls see it
                    if self.fill_rows && n > 0 && total < want {
                        continue;
                    }
                    if total < want && self.run.producer_next() {
                        self.run_producers(entry)?;
                        continue;
                    }
                    // M8 (L0-k): the next emit's late step, then its pull
                    if total < want && self.run.emit_next() {
                        self.emit_late(entry)?;
                        continue;
                    }
                    if !self.run.is_done() || !self.open_next_arm(entry)? || total >= want {
                        self.limited = total >= want && !self.run.is_done();
                        break total as u64;
                    }
                }
                // D824 (V4R-298): the emit stopped for its lazy build: the producers' arm runs it, the next fetch resumes the emit
                Err(Error::BuildWait(b)) => {
                    total += self.build_wait(entry, b)?;
                    continue;
                }
                Err(Error::Recheck(site)) => match self.recheck_stop(entry, site, cap, &mut consume)? {
                    Some(partial) => {
                        total += partial;
                        continue;
                    }
                    None => return Ok(true),
                },
                Err(e) => return Err(self.ended(e)),
            }
        };
        // D2126: the fetch kept its rows and the error behind them (pull.rs `failed`), which a cursor may never reach:
        // what that error's callee left (its SPI connection) is undone now, as PostgreSQL's abort would have, so it neither
        // meets a later caller nor warns at COMMIT ("transaction left non-empty SPI stack")
        if !pl.kept() {
            pl.restore();
        }
        self.entries = self.run.entries();
        if cmd == Cmd::Select {
            self.es_processed = served;
            self.rows += served;
        }
        if self.run.is_done() {
            if cmd != Cmd::Select {
                self.es_processed = (self.run.rows_processed() + self.rechecked).min(cap);
                self.rows = self.es_processed;
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// D824 (V4R-298): the emit stopped for its lazy build: the producers' arm runs it, the next fetch resumes the emit; the
    /// rows the stopped fetch sent. D3620: out of line (the fetch loop's frame is on every nested statement's stack path).
    #[inline(never)]
    fn build_wait(&mut self, entry: &mut gravity_driver::Entry, b: usize) -> PgResult<usize> {
        let partial = self.run.take_partial();
        let Some(p) = self.producers.as_deref() else { return Err(glue::pg_error(Error::BuildWait(b))) };
        let registry = &mut self.a.registry;
        self.run.with_open(|exec, binding| p.build_wait(entry, exec, binding, registry, b))?;
        Ok(partial)
    }

    /// `step_counted`'s stop at a recheck (`Error::Recheck`): `Some(rows)` when the run resumes past the rechecked row (the
    /// rows the stopped fetch sent), `None` when the statement completed here. D682: a write whose kernel left its resume
    /// word resumes past the rechecked row (runner need 18, as the runner's bounded_step): the rest of the scan runs, a
    /// surviving row's RETURNING buffered in order after the rows before it; a kernel without it (a one-row statement) ends
    /// here. The rows this fetch sent before the stop count toward `want`. D718: a multi-row SELECT .. FOR UPDATE resumes
    /// the same way (ExecLockRows goes on after EvalPlanQual): its surviving row, buffered in the run's sink, is the next
    /// fetch's first row. D3620: out of line (the fetch loop's frame is on every nested statement's stack path).
    #[inline(never)]
    fn recheck_stop(&mut self, entry: &mut gravity_driver::Entry, site: u8, cap: u64, consume: &mut impl FnMut(&[u64], &[u8]) -> Result<(), Error>) -> PgResult<Option<usize>> {
        let more = self.recheck_run(entry, site)?;
        if resumes(&self.run) {
            if self.cmd != Cmd::Select {
                self.rechecked += more;
            }
            return Ok(Some(self.run.take_partial()));
        }
        let drained = match self.run.drain(consume) {
            Ok(n) => n as u64,
            Err(e) => return Err(self.ended(e)),
        };
        self.es_processed = if self.cmd == Cmd::Select { drained } else { (self.rechecked + more + self.run.rows_processed()).min(cap) };
        self.rows += self.es_processed;
        Ok(None)
    }

    /// `step_counted`'s recheck of the row a write stopped at (`Error::Recheck`): its InitPlans, its producers' frame and
    /// the rerun; the rows it lets through. D3620: out of line, so the fetch loop's frame (on every nested statement's stack
    /// path) does not hold its locals.
    #[inline(never)]
    fn recheck_run(&mut self, entry: &mut gravity_driver::Entry, site: u8) -> PgResult<u64> {
        let pos = self.run.recheck_pos();
        let positions: Vec<gravity_abi::ScanPos> = self.run.positions().to_vec(); // alloc: Cold
        // D2112 phase B: the pending InitPlans the recheck plan reads run first, in this execution, under the producers'
        // frame (epq.rs `forced`: PostgreSQL's EvalPlanQualStart forces them before the recheck)
        if let Some(p) = self.producers.as_deref() {
            let registry: *mut gravity_hosts::HostRegistry = &mut self.a.registry;
            self.run.with_open(|x, b| match crate::rerun::epq::forced(x.node().program(), b.parts.params) {
                cells if cells.is_empty() => Ok(()),
                cells => p.with_frame(entry, x, b, registry, |_| crate::rerun::epq::force(&cells)),
            })?;
        }
        // D655: a program of several pipelines reruns the producers the stopped pipeline reads by its plan
        let plan = self.producers.as_deref().zip(self.run.exec().map(|x| x.lease())).map(|(p, lease)| (p.plan(), lease));
        // writesubplan (D652's rest): the producers' frame over the recheck too, as the runner's own recheck runs
        // under its Leader's publish (gravity_parallel run_slotted): a rerun set read after the write on the recheck
        // node (a RETURNING-only SubPlan step) reruns through it, re-entering that node (driver Entry::rerun's
        // publish_current), where without it m8_rerun had no runner frame
        let mut open = None;
        if self.producers.is_some() && on_demand_protos(&self.a.program) {
            let _ = self.run.with_open(|x, b| Ok::<(), ()>(open = Some((x as *mut gravity_driver::Exec, b as *mut gravity_driver::Binding<'_>))));
        }
        match (self.producers.as_deref(), open) {
            (Some(p), Some((x, b))) => {
                let (a, run) = (&mut self.a, &mut self.run);
                let registry: *mut _ = &mut a.registry;
                p.with_frame(entry, x, b, registry, |entry| crate::rerun::epq::recheck_with(entry, a, None, run.binding_mut().expect("open"), &positions, site as usize, pos, plan))
            }
            _ => {
                let binding = self.run.binding_mut().expect("open");
                crate::rerun::epq::recheck_with(entry, &mut self.a, None, binding, &positions, site as usize, pos, plan)
            }
        }
    }

    fn snapshot_sites(&mut self) {
        let Some(exec) = self.run.exec() else { return };
        let ctx = exec.node().ctx();
        let n = self.a.program.sites.len();
        self.site_rows.clear();
        self.instr = exec.node().instr_cells().to_vec(); // alloc: Exec
        for i in 0..n {
            let site = super::raw::site(ctx, i);
            self.site_rows.push((site.rows_in, site.rows_out)); // alloc: Exec
        }
        self.snapshot_fetches();
    }

    /// Each index-only site's heap fetches over this execution (`site_fetches`), read from the recipes: the pull road's
    /// completion (`snapshot_sites`) and the driver road's (complete.rs) both take them.
    pub(crate) fn snapshot_fetches(&mut self) {
        self.site_fetches.clear();
        self.site_bitmap.clear();
        self.site_rels.clear();
        self.site_skipped.clear();
        for i in 0..self.a.program.sites.len() {
            let r = &self.a.registry;
            let rel = r.recipe(i).map(|s| s.heap).or_else(|| r.range_recipe(i).map(|s| s.heap)).or_else(|| r.granule_recipe(i).map(|s| s.rel)).unwrap_or(0);
            self.site_rels.push(rel); // alloc: Exec
            self.site_skipped.push(r.granule_recipe(i).and_then(|g| g.skipped_rows()).unwrap_or(0)); // alloc: Exec
            let base = self.bitmap_base.get(i).copied().unwrap_or([0; 5]);
            self.site_bitmap.push(self.a.registry.bitmap_recipe(i).or_else(|| self.a.registry.recipe(i)).map(|s| s.bitmap().load()).map(|c| core::array::from_fn(|j| c[j].saturating_sub(base[j])))); // alloc: Exec
            let fetched = self.a.registry.recipe(i).filter(|s| s.only).map(|s| s.heap_fetches().saturating_sub(self.fetch_base.get(i).copied().unwrap_or(0)));
            self.site_fetches.push(fetched); // alloc: Exec
        }
    }
}

/// B2-31: `executor_run`'s refusal of an admission holding a timestamptz constant converted in the session TimeZone whose
/// rows may come over several runs, or whose first run is in another zone (`Serve::zone_elsewhere`): the safety net under
/// the entry's restart (`entry::zone_restart`), which takes a SELECT's first run before anything ran, once.
pub const ZONE_REFUSAL: &str = "entry: a timestamptz constant converted in the session TimeZone, run in another zone, by a named cursor or under a portal's row count";

impl Serve {
    /// B2-31: an admission holding a timestamptz constant converted in the session TimeZone (normalize retype,
    /// `Admitted::timezone`) is one call's answer, where PG's cross-type compare reads the zone as each run goes: true at
    /// this execution's first run when its rows may come over several runs (a named cursor's FETCHes, the protocol's
    /// unnamed portal under an Execute's max_rows or an SPI fetch's count) or when the zone moved since the admission. A
    /// named cursor's admission converts nothing (front.rs `ZONE.active`); one filed by another portal of the plan, or by
    /// the runner's Bind, reaches here.
    pub(crate) fn zone_elsewhere(&self, count: u64) -> bool {
        !self.keys_done && !self.done && self.a.timezone.is_some() && (self.named_cursor || (count > 0 && self.fetch_portal) || gravity_decompose::normalize::zone_moved(self.a.timezone))
    }
}

/// `step_counted`'s row callback, its reads in one place: the callback the fetch holds is one word, not a reference per
/// capture (D3620: that frame is on every nested statement's stack path).
struct RowSend<'c> {
    sent: &'c core::cell::Cell<u64>,
    last: &'c core::cell::Cell<Option<(u32, u64)>>,
    cap: u64,
    strip: usize,
    sidecar: Option<types_portal::TuplestoreHandle>,
    pl: crate::runner::PlState,
    spi: bool,
    sink: Option<DestSink>,
}

impl RowSend<'_> {
    fn row(&mut self, v: &[u64], n: &[u8]) -> Result<(), Error> {
        if self.sent.get() >= self.cap {
            return Ok(());
        }
        // D2126: a held error's leftover undone first (`PlState::restore`); only what it cannot undo ends the fetch
        if self.spi && !self.pl.kept() && !self.pl.restore() {
            return Err(Error::host(PL_LEFT));
        }
        self.sent.set(self.sent.get() + 1);
        match self.sink.as_mut() {
            Some(d) if self.strip == 0 => d.send_row(v, n).map(|_| ()).map_err(gravity_hosts::stash),
            Some(d) => {
                let k = v.len() - self.strip;
                d.send_row(&v[..k], &n[..k]).map_err(gravity_hosts::stash)?;
                let (oid, tid) = crate::admit::entry::current_of::pack(if n[k] != 0 { 0 } else { v[k] }, v[k + 1]);
                match self.sidecar {
                    Some(h) => tuplestore::hold::tidstore_put(h, oid, tid).map_err(gravity_hosts::stash),
                    None => Ok(self.last.set(Some((oid, tid)))),
                }
            }
            None => Ok(()),
        }
    }
}

pub fn executor_run(qd: &mut QueryDesc, direction: ScanDirection, count: u64, dest: &mut DestReceiver<'_>) -> PgResult<()> {
    // D3361: the write hosts this run binds take the desc's crosscheck snapshot (PG's es_crosscheck_snapshot)
    let _crosscheck = gravity_hosts::ops::modifytable::write::Crosscheck::set(qd.crosscheck.clone());
    // the binding borrows the desc's snapshot for as long as the adapter is open (a cursor's
    // fetches): the desc is boxed and keeps it until free, so the pointer is stable
    let snap_ptr: *const Rc<types_snapshot::SnapshotData<'static>> = qd.snapshot.as_ref().expect("a served statement has a snapshot");
    let (params, instrumented, rewindable) = (qd.params, qd.instrument_options != 0, qd.eflags & EXEC_FLAG_REWIND != 0);
    let operation = qd.plannedstmt().commandType as i32;
    let started = core::mem::take(&mut qd.started);
    let serve = qd.serve.as_mut().expect("serving");
    serve.es_processed = 0;
    let _sent = gravity_driver::ahead_trace::statement(serve.sent > 0); // construct C2 R3: a portal's earlier fetch sent rows
    if ScanDirectionIsBackward(direction) {
        return Err(glue::internal("gravity_entry: backward scan on a served statement"));
    }
    // D2355: a hosted program whose home level is running (a cursor opened at the top level FETCHed inside a PL/pgSQL
    // call of a running top-level statement) runs away from it unless the running statement's pipeline calls hold its
    // host state (`host_busy`: the same program instance); that one is refused before its receiver starts
    serve.started = false;
    if !ScanDirectionIsNoMovement(direction) && !serve.done && crate::admit::reentry::running(serve.home) && serve.host_busy() {
        return Err(crate::runner::refused(crate::admit::reentry::AWAY_REFUSAL));
    }
    // B2-31: a zoned admission whose rows may come over several runs, or run first in another zone
    // (`Serve::zone_elsewhere`), the entry's restart did not take is refused before the receiver starts, as above
    if serve.zone_elsewhere(count) {
        return Err(crate::runner::refused(ZONE_REFUSAL));
    }
    // D3382: EXECUTE's portal (CREATE TABLE AS EXECUTE) started this desc with no receiver and hands it the table's at its
    // first run, so D866's fill a row at a time is decided here
    if !serve.fill_rows && !serve.run.is_open() && !serve.done && serve.sent == 0 && matches!(dest.mydest(), types_dest::CommandDest::IntoRel | types_dest::CommandDest::TransientRel) && gravity_driver::pull::pull_exact(&serve.a.program) {
        serve.fill_rows = true;
    }
    let send = serve.tupdesc.is_some();
    // v5 errorhold: the plan-order run into a receiver the failed licensed run started skips a second startup
    if let Some(td) = serve.tupdesc.as_deref().filter(|_| !started) {
        dest.startup(operation, td)?;
        serve.started = true;
    }
    // D3039: a portal's first run that takes every row (count 0: EXECUTE, a protocol Execute without max_rows, an EXPLAIN
    // ANALYZE's run), before the adapter opened, of a parallel program no named cursor reads or rewinds, runs as a completion run does
    // (the driver: every pipeline at the granted width) instead of pulling its emit pipeline on the leader alone. Not
    // into a protocol destination (a Bind / Execute portal's RemoteExecute): the driver sends there through the simple
    // protocol's wire node, which the portal path does not hold (its rows go through the destination); EXECUTE's
    // utility path fills the portal's store, a destination the driver writes to
    let local = !matches!(dest.mydest(), types_dest::CommandDest::Remote | types_dest::CommandDest::RemoteExecute | types_dest::CommandDest::RemoteSimple);
    let whole_first = count == 0 && local && !serve.run.is_open() && !serve.done && !serve.named_cursor && !rewindable && serve.nested.is_none();
    // D3490: a run that moves nothing (NoMovementScanDirection: PortalRunSelect's count 0, as CREATE TABLE AS EXECUTE ..
    // WITH NO DATA asks through ExecuteQuery) runs no pipeline, as standard_ExecutorRun skips ExecutePlan; its count 0 is
    // no whole-first run (the receiver's startup and shutdown still bracket it below)
    let moves = !ScanDirectionIsNoMovement(direction);
    if moves && serve.a.is_parallel() && (serve.completion || whole_first) && !serve.fill_rows {
        // a completion run of a parallel program (complete.rs): the driver; a bounded caller pulls it through the adapter
        let r = crate::admit::entry::complete::run(qd, count, dest);
        if send {
            dest.shutdown()?;
        }
        return r;
    }
    if moves && !serve.done {
        let (home, hosted) = (serve.home, serve.host_busy());
        // D3620: an instrumented or recorded run (EXPLAIN ANALYZE) out of line, so a plain run's frame (on every nested
        // statement's stack path) holds none of its clock, counters or recording
        let r = if instrumented || serve.report.is_some() {
            run_measured(serve, (home, hosted), count, dest, snap_ptr, params, instrumented, rewindable)
        } else {
            let nested = serve.nested.clone();
            // D3620: the step goes down the reentry layers by reference: each layer's frame (on every nested statement's
            // stack path) holds a word, not a copy of the captures
            let mut step = |b: &mut crate::Backend, depth: usize| -> PgResult<()> {
                // D622: a nested statement's sites take the grants it reserved at its start (a rewound one's, the grant it
                // kept)
                crate::runner::hand(depth, nested.as_ref(), &mut || serve.run_step(b, depth, count, dest, snap_ptr, params, false, rewindable))
            };
            crate::admit::reentry::enter(Some(crate::admit::reentry::built), Some(home), hosted, &mut step)
        };
        match r {
            Ok(Some(Ok(()))) => crate::admit::entry::current_of::fetch_served(&mut serve.fetched, serve.shape, serve.es_processed, if serve.a.graph.is_some() { "graph" } else { "shape" }),
            Ok(Some(Err(e))) => return Err(crate::runner::run_error(&serve.a, e)),
            Ok(None) => return Err(glue::internal("gravity_entry: no backend state")),
            Err(why) => return Err(crate::runner::refused(why)),
        }
    }
    if send {
        dest.shutdown()?;
    }
    Ok(())
}

/// `executor_run`'s run of an instrumented or recorded statement (EXPLAIN ANALYZE): its wall time (W9b PERF D1559 S3a: an
/// uninstrumented run takes no clock), its buffer counters, and the pipelines the driver runs for it recorded (driver
/// entry/record.rs, closed on every exit). D3620: out of line, so a plain run's frame holds none of it.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn run_measured(serve: &mut Serve, (home, hosted): (usize, bool), count: u64, dest: &mut DestReceiver<'_>, snap_ptr: *const Rc<types_snapshot::SnapshotData<'static>>, params: types_portal::ParamListHandle, instrumented: bool, rewindable: bool) -> Result<Option<PgResult<()>>, &'static str> {
    let t0 = instrumented.then(Instant::now);
    // D3620: boxed, the counters out of this frame
    let b0 = instrumented.then(buffer_snapshot);
    let recording = serve.report.is_some();
    let mut runs = None;
    let nested = serve.nested.clone();
    let mut step = |b: &mut crate::Backend, depth: usize| -> PgResult<()> {
        if recording {
            return step_recorded(serve, b, depth, nested.as_ref(), &mut runs, &mut |serve: &mut Serve, b: &mut crate::Backend| serve.run_step(b, depth, count, dest, snap_ptr, params, instrumented, rewindable));
        }
        crate::runner::hand(depth, nested.as_ref(), &mut || serve.run_step(b, depth, count, dest, snap_ptr, params, instrumented, rewindable))
    };
    let r = crate::admit::reentry::enter(Some(crate::admit::reentry::built), Some(home), hosted, &mut step);
    if let Some(t0) = t0 {
        serve.elapsed_ns += t0.elapsed().as_nanos() as u64;
    }
    if let Some(b0) = b0 {
        buffer_accum(&mut serve.bufusage, &b0);
    }
    if let (Some((runs, run_ns, _)), Some(report)) = (runs, serve.report.as_deref_mut()) {
        super::instrument::report_pull(report, runs, run_ns);
    }
    r
}

/// `executor_run`'s step of an EXPLAIN ANALYZE's pull: the pipelines the driver runs for it recorded (driver
/// entry/record.rs, closed on every exit) around the step `run` (`hand`'s, D622). D3620: out of line, so a plain run's step
/// (every level of a nested statement's stack path) holds none of the recording's frame.
#[inline(never)]
fn step_recorded(serve: &mut Serve, b: &mut crate::Backend, depth: usize, nested: Option<&Rc<core::cell::RefCell<crate::runner::NestedGrants>>>, runs: &mut Option<(Vec<gravity_driver::entry::record::PipeRun>, u64, Vec<u64>)>, run: &mut dyn FnMut(&mut Serve, &mut crate::Backend) -> PgResult<()>) -> PgResult<()> {
    let probe = Some(super::instrument::buffer_probe as fn() -> gravity_driver::entry::record::Bufs);
    let token = b.entry.record_open_probed(&serve.a.program, probe);
    let r = crate::runner::hand(depth, nested, &mut || run(serve, b));
    *runs = Some(b.entry.record_close(token));
    r
}

/// D3620: EXPLAIN ANALYZE's buffer counters since `b0` added to the run's, out of line (the counters now are a temporary
/// `executor_run`'s frame would hold).
#[inline(never)]
fn buffer_accum(acc: &mut types_core::instrument::BufferUsage, b0: &types_core::instrument::BufferUsage) {
    instrument::buffer_usage_accum_diff(acc, &instrument::pg_buffer_usage(), b0);
}

/// D3620: EXPLAIN ANALYZE's buffer-usage snapshot, boxed out of line, so `executor_run`'s frame (on every nested
/// statement's stack path) does not hold the counters, not even as the box's staging copy.
#[inline(never)]
fn buffer_snapshot() -> Box<types_core::instrument::BufferUsage> {
    Box::new(instrument::pg_buffer_usage()) // alloc: Cold
}

/// End the execution: the adapter's node is unbound and parked (or torn down: an unkeyed exec). A run the caller's
/// count stopped with the pipeline suspended (a one-row emit suspends `out_full` at its row) is served here.
pub fn close(qd: &mut QueryDesc, ok: bool) -> PgResult<()> {
    let Some(serve) = qd.serve.as_mut() else { return Ok(()) };
    let mut ended = Ok(());
    if serve.run.is_open() {
        let limited = ok && serve.limited;
        let admit = if serve.a.graph.is_some() { "graph" } else { "shape" };
        let _ = crate::with_backend(|b| {
            ended = serve.close_run(&mut b.entry, ok);
            if limited {
                b.note_served_via(serve.shape, serve.rows, admit, serve.code(), serve.via);
                b.note_mechs(serve.a.program.key.0, serve.a.program.used);
            }
        });
    }
    serve.limited = false;
    serve.producers = None;
    serve.done = true;
    serve.held = Default::default(); // D761: ExecutorEnd's ExecCloseRangeTableRelations
    ended
}

/// `ExecutorRewind`: the next run starts the execution afresh under the same snapshot and params.
pub fn rewind(qd: &mut QueryDesc) -> PgResult<()> {
    // D2701: a soft run keeps its execution (the filled slots, the rerun sets' state, the InitPlan cells) and runs only its
    // emit again, as PostgreSQL's rescan from the top re-evaluates the top chain over what its Sort, Material, FunctionScan
    // and InitPlans keep
    let params = qd.params;
    if let Some(s) = qd.serve.as_mut().filter(|s| s.soft && s.run.is_open()) {
        // the error mapped inside the level's run, whose stashes (gravity_hosts `StashScope`) end with it
        match crate::with_backend(|b| s.run.rewind(&mut b.entry).map_err(glue::pg_error)) {
            Some(Ok(())) => {}
            Some(Err(e)) => return Err(e),
            None => return Err(glue::internal("gravity_entry: no backend state")),
        }
        // D3377: the arms an exec pruning removed stay pruned (the rewind cleared every emit's mark)
        if let Some(p) = s.producers.as_deref() {
            let _ = s.run.with_open(|exec, _| {
                p.prune_emits(exec);
                Ok::<(), ()>(())
            });
        }
        s.bind_runs(params);
        (s.done, s.limited, s.rechecked, s.rows, s.entries) = (false, false, 0, 0, 0);
        return Ok(());
    }
    // D2647: the InitPlans keep their values (a run that completed kept them at its close)
    if let Some(s) = qd.serve.as_mut().filter(|s| s.run.is_open()) {
        s.keep_inits();
    }
    // D622: the producers taken out alive (the adapter's close still reads them), their grant kept for the rerun: a served
    // rewind (a WITH HOLD cursor's persist) never gives its grant back to queue for it again behind what took its slot
    let kept = qd.serve.as_mut().and_then(|s| s.producers.take());
    let closed = close(qd, true);
    // D703: the kept producers' protocols end as the rest of the run's do (ReScanExprContext's callbacks)
    let ended = kept.as_deref().map_or(Ok(()), |p| p.end());
    if let Some(serve) = qd.serve.as_mut() {
        serve.done = false;
        serve.rechecked = 0;
        serve.rows = 0;
        serve.entries = 0;
        if let Some(p) = kept {
            serve.nested = Some(Rc::new(core::cell::RefCell::new(p.into_kept(&serve.a.program))));
        }
    }
    closed.and(ended)
}

/// D682: the run's kernel left with its resume word set (the next call continues past the rechecked row), as the runner's
/// `resumes`.
fn resumes(run: &PullAdapter<'static>) -> bool {
    run.exec().is_some_and(|e| super::raw::resume(e.node().ctx()) != gravity_abi::ctx::RESUME_NONE)
}

/// D2701: lowering marked the program `soft_rewind` (lowerv4 effects/visible.rs), it is the statement's one program (no
/// arm programs: their runs close one after another), its InitPlans are the graph's (an in-program Init steps its emits)
/// and no pulled set may sit parked mid-pull at the rewind (a CTE, a pulled WindowAgg or merge input).
pub(super) fn soft_rewind(a: &crate::Admitted) -> bool {
    let v4 = a.program.v4.as_ref();
    v4.is_some_and(|d| d.soft_rewind && !d.protos.iter().any(gravity_program::protos::pulled)) && a.graph.as_ref().is_none_or(|g| g.arms.is_empty()) && !init_protos(&a.program)
}

/// M8 (RT-1): the program declares nest.initplan's Init (its InitPlans are its own pipelines, run on demand).
fn init_protos(program: &gravity_program::Program) -> bool {
    program.v4.as_ref().is_some_and(|d| d.protos.iter().any(|x| x.kind == gravity_program::protos::ProtoKind::Init))
}

/// D1401: the program declares a protocol whose host runs pipelines on demand through the runner's frame (`m8_init_run`,
/// `m8_rerun`, `m8_subhash_run`, a CTE reader's `m8_cte_pull`: a cursor's FETCH pulls it; a WindowAgg's pulled fill, its
/// object's `m10_win_ask`), which an emit's call reaches as well as a producer's.
fn on_demand_protos(program: &gravity_program::Program) -> bool {
    use gravity_program::protos::ProtoKind as K;
    program.v4.as_ref().is_some_and(|d| d.protos.iter().any(|x| matches!(x.kind, K::Init | K::Rerun | K::SubHash) || gravity_program::protos::pulled(x)))
}

#[cfg(test)]
#[allow(unsafe_code)]
mod d1506 {
    use super::desc_mcx;
    use mcx::{Mcx, SessionCleanupPhase};
    use std::collections::HashMap;
    use pgsync::Mutex;
    use std::thread::ThreadId;
    use types_error::PgResult;

    type Cleanup = Box<dyn FnOnce()>;
    /// Each test thread's registered cleanups (the crate test's own session-cleanup sink), in a process static keyed by
    /// thread (main 09-28: no thread_local! in a test module; the TLS census stays at trunk's count).
    struct Lists(HashMap<ThreadId, Vec<(SessionCleanupPhase, Cleanup)>>);
    // SAFETY: a thread's entries are pushed and run only by that thread (keyed by its ThreadId); the map moves no closure
    // across threads.
    unsafe impl Send for Lists {}
    static LISTS: Mutex<Option<Lists>> = Mutex::new(None);

    fn sink(p: SessionCleanupPhase, f: Cleanup) {
        let mut l = pgsync::lock(&LISTS);
        l.get_or_insert_with(|| Lists(HashMap::new())).0.entry(std::thread::current().id()).or_default().push((p, f)); // alloc: Cold
    }

    /// This thread's session teardown, as mcxt_stats drains it: Portals, then State, then Roots, LIFO within a phase (each
    /// cleanup runs outside the lock: it may register another).
    fn teardown() {
        let me = std::thread::current().id();
        for phase in [SessionCleanupPhase::Portals, SessionCleanupPhase::State, SessionCleanupPhase::Roots] {
            while let Some(f) = {
                let mut l = pgsync::lock(&LISTS);
                l.as_mut().and_then(|l| l.0.get_mut(&me)).and_then(|v| {
                    let i = v.iter().rposition(|(p, _)| *p == phase)?;
                    Some(v.remove(i).1)
                })
            } {
                f();
            }
        }
    }

    /// The desc path's answer after the teardown: a refusal by name (the tip), or a context handed out (before D1506 the
    /// context lived on, never freed, so any later desc was served from it).
    trait Late {
        fn named(self) -> bool;
        fn ctx(self) -> Mcx<'static>;
    }
    impl Late for Mcx<'static> {
        fn named(self) -> bool {
            false
        }
        fn ctx(self) -> Mcx<'static> {
            self
        }
    }
    impl Late for PgResult<Mcx<'static>> {
        fn named(self) -> bool {
            self.is_err_and(|e| e.message().contains("after the session's teardown (D1506)"))
        }
        fn ctx(self) -> Mcx<'static> {
            self.expect("a desc before the teardown")
        }
    }

    #[test]
    fn d1506_the_descs_context_ends_at_the_sessions_roots_phase_and_a_later_desc_is_a_named_error() {
        mcx::set_session_cleanup_sink(sink);
        let _ = desc_mcx();
        teardown();
        assert!(desc_mcx().named(), "a desc after the session's teardown must be refused by name (D1506)");
    }

    #[test]
    fn d1506_a_desc_still_held_at_the_roots_phase_keeps_its_context() {
        mcx::set_session_cleanup_sink(sink);
        let td = std::rc::Rc::new(tupdesc::CreateTemplateTupleDesc(desc_mcx().ctx(), 3).expect("a template desc"));
        teardown();
        // the context was left allocated: the held desc stays readable and its drop returns its bytes to it
        assert_eq!(td.natts, 3);
        drop(td);
    }
}
