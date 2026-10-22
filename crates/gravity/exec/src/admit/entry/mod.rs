//! Gravity entry: the executor seams over the v3 runner (docs/gravity/entry.md).

pub mod complete;
pub mod current_of;
mod explain;
pub mod instrument;
pub mod keyed;
mod raw;
pub mod run;
mod shelf;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use pgsync::OnceLock;

use execmain_seams as es;
use execmain_seams::Snapshot;
use tcop_dest::DestReceiver;
use types_dest::CommandDest;
use mcx::Mcx;
use types_error::PgResult;
use types_nodes::nodes_enums::CmdType;
use types_nodes::plannodes::PlannedStmt;
use types_portal::{CachedPlanHandle, ParamListHandle, QueryDescHandle, QueryEnvHandle};
use types_scan::sdir::ScanDirection;
use types_nodes::parsenodes::{CURSOR_OPT_HOLD, CURSOR_OPT_SCROLL};
use types_slot::{EXEC_FLAG_BACKWARD, EXEC_FLAG_EXPLAIN_GENERIC, EXEC_FLAG_EXPLAIN_ONLY, EXEC_FLAG_MARK, EXEC_FLAG_REWIND, EXEC_FLAG_RUN_WHOLE, EXEC_FLAG_SKIP_TRIGGERS};
use types_tuple::TupleDescData;

use raw::with_desc;

pub const VIA: &str = "entry";
pub const VIA_NESTED: &str = "entry:nested";
/// An EXPLAIN without ANALYZE: the admission's verdict is logged, the printer walks the Plan (entry.md §13, explain.rs).
pub const VIA_EXPLAIN: &str = "entry:explain";
/// PERF-OLTP E2: an execution of a serve its unnamed portal retained (`executor_rearm`): no QueryDesc, admission or Serve built.
pub const VIA_REARMED: &str = "entry:rearmed";

/// The support-layer answers the entry asks of execmain (no plan runs there): WHERE CURRENT OF over a portal with no desc
/// (execCurrentOf's own errors: a cursor held from a previous transaction, not positioned on a row), and D1406's
/// rewind-replay question over the plan alone.
pub struct Prev {
    pub exec_current_of: es::exec_current_of::Signature,
    pub cursor_plan_rewind_replays: es::cursor_plan_rewind_replays::Signature,
}

static PREV: OnceLock<Prev> = OnceLock::new();

pub fn prev() -> &'static Prev {
    PREV.get().expect("gravity_entry: seams not installed")
}

/// The executor seams are the entry's alone: every query desc is made here and every statement is served or raises.
/// execmain installs only the support answers wrapped below (`Prev`); the row executor is deleted.
pub fn init_seams() {
    es::create_query_desc::set(create_query_desc);
    es::free_query_desc::set(free_query_desc);
    es::note_cplan_for_query_desc::set(note_cplan_for_query_desc);
    es::release_query_desc::set(release_query_desc);
    es::executor_start::set(executor_start);
    es::executor_run::set(executor_run);
    es::executor_finish::set(executor_finish);
    es::executor_finish_and_park::set(executor_finish_and_park);
    es::executor_rearm::set(executor_rearm);
    es::executor_rewind::set(executor_rewind);
    es::executor_end::set(executor_end);
    es::query_desc_es_processed::set(query_desc_es_processed);
    es::query_desc_snapshot::set(query_desc_snapshot);
    es::query_desc_result_tupdesc::set(query_desc_result_tupdesc);
    es::query_desc_operation::set(query_desc_operation);
    es::query_desc_instrument::set(instrument::query_desc_instrument);
    es::query_desc_trigger_instrument::set(instrument::query_desc_trigger_instrument);
    es::query_desc_prune_result::set(instrument::query_desc_prune_result);
    es::query_desc_rti_unpruned::set(instrument::query_desc_rti_unpruned);
    es::query_desc_agg_instrument::set(instrument::query_desc_agg_instrument);
    es::query_desc_sort_instrument::set(instrument::query_desc_sort_instrument);
    es::query_desc_incsort_instrument::set(instrument::query_desc_incsort_instrument);
    es::query_desc_index_instrument::set(instrument::query_desc_index_instrument);
    es::query_desc_hash_instrument::set(instrument::query_desc_hash_instrument);
    es::query_desc_tuplestore_instrument::set(instrument::query_desc_tuplestore_instrument);
    es::query_desc_memoize_instrument::set(instrument::query_desc_memoize_instrument);
    es::query_desc_bitmap_instrument::set(instrument::query_desc_bitmap_instrument);
    es::query_desc_index_searches::set(instrument::query_desc_index_searches);
    es::query_desc_jit_instr::set(instrument::query_desc_jit_instr);
    es::query_desc_runtime_ea_refusals::set(instrument::query_desc_runtime_ea_refusals);
    es::query_desc_runtime_ea_pipeline::set(instrument::query_desc_runtime_ea_pipeline);
    es::query_desc_gravity_node::set(instrument::query_desc_gravity_node);
    es::query_desc_gravity_report::set(instrument::query_desc_gravity_report);
    es::query_desc_gravity_heap_fetches::set(instrument::query_desc_gravity_heap_fetches);
    es::query_desc_engine_events::set(instrument::query_desc_engine_events);
    es::query_desc_workers_launched::set(instrument::query_desc_workers_launched);
    es::query_desc_merge_instrument::set(instrument::query_desc_merge_instrument);
    es::query_desc_worker_instrument::set(instrument::query_desc_worker_instrument);
    es::query_desc_worker_sort_instrument::set(instrument::query_desc_worker_sort_instrument);
    es::query_desc_worker_bitmap_instrument::set(instrument::query_desc_worker_bitmap_instrument);
    es::query_desc_worker_memoize_instrument::set(instrument::query_desc_worker_memoize_instrument);
    es::query_desc_worker_incsort_instrument::set(instrument::query_desc_worker_incsort_instrument);
    es::query_desc_worker_agg_instrument::set(instrument::query_desc_worker_agg_instrument);
    es::query_desc_fdw_batch_size::set(instrument::query_desc_fdw_batch_size);
    es::query_desc_foreign_explain::set(instrument::query_desc_foreign_explain);
    es::cursor_capture_current::set(instrument::cursor_capture_current);
    es::with_hook_qd::set(with_hook_qd);
    let p = Prev { exec_current_of: es::exec_current_of::wrap(exec_current_of), cursor_plan_rewind_replays: es::cursor_plan_rewind_replays::wrap(cursor_plan_rewind_replays) };
    let _ = PREV.set(p);
}

/// The entry's handles have a zero low word (execmain's registry carried `idx + 1` there): `(gen << 52) | ((idx + 1) << 32)`.
/// D3671: a 20-bit slot, as every nested statement holds a live desc (a 16-bit one wrapped into the generation past 65,535
/// levels of SQL-function recursion), and a 12-bit generation.
const IDX_BITS: u32 = 20;
/// The live descs a thread can hold.
pub const MAX_DESCS: u32 = (1 << IDX_BITS) - 1;
const GEN_MASK: u16 = (1 << (32 - IDX_BITS)) - 1;

pub fn encode(idx: u32, generation: u16) -> QueryDescHandle {
    debug_assert!(idx < MAX_DESCS && generation <= GEN_MASK);
    QueryDescHandle(((generation as u64) << (32 + IDX_BITS)) | ((idx as u64 + 1) << 32))
}

pub fn decode(h: QueryDescHandle) -> (u32, u16) {
    ((((h.0 >> 32) as u32) & MAX_DESCS).wrapping_sub(1), (h.0 >> (32 + IDX_BITS)) as u16)
}

pub fn is_ours(h: QueryDescHandle) -> bool {
    h.0 != 0 && (h.0 & 0xffff_ffff) == 0
}

/// A handle the entry did not make reached an executor seam. Every query desc is the entry's (`create_query_desc`), so this
/// is a caller's bug (a stale or NULL handle), as the removed row executor's registry asserted.
#[cold]
#[track_caller]
fn foreign(h: QueryDescHandle) -> ! {
    panic!("gravity_entry: a query desc handle the entry did not make: {h:?}")
}

/// `foreign`'s error for the seams that return one.
#[cold]
fn foreign_error(h: QueryDescHandle) -> Box<types_error::PgError> {
    crate::glue::internal(&format!("gravity_entry: a query desc handle the entry did not make: {h:?}")) // alloc: Cold
}

/// The kind of statement form the entry cannot serve, worded in its error (`removed`) as what the user asked for: the
/// families that serve these statements are keyed by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Road {
    /// The entry's own conditions (`refusal`): a scan direction, a snapshot, a crosscheck write, parallel mode, a VALUES
    /// row's gate.
    Entry,
    /// EXPLAIN ANALYZE output the served run does not count yet (a write's trigger timing lines).
    ExplainAnalyze,
    /// A statement nested under SPI or a served statement whose form the served run cannot keep (D2044, D2297, D2834).
    Nested,
    /// A cursor or portal form (backward fetch, CURRENT OF, a partial reader of a whole-read program, a rewind, a zoned run).
    Cursor,
    /// A plan kind whose output is its provider's (a foreign table, a custom scan).
    PlanKind,
}

impl Road {
    /// What the user asked for that is not supported, as the refusal's message opens it.
    fn what(self) -> &'static str {
        match self {
            Road::Entry => "this statement's execution mode is not supported",
            Road::ExplainAnalyze => "this EXPLAIN ANALYZE output is not supported",
            Road::Nested => "this statement is not supported inside a function or procedure",
            Road::Cursor => "this cursor operation is not supported",
            Road::PlanKind => "this plan node is not supported",
        }
    }

    /// The refusal's HINT: what the user can do instead.
    fn hint(self) -> &'static str {
        match self {
            Road::Entry => "Run the statement outside a parallel worker and with a snapshot, as a plain top-level statement.",
            Road::ExplainAnalyze => "Run EXPLAIN without ANALYZE, or run the statement itself.",
            Road::Nested => "Run the statement at top level instead of from a function or procedure.",
            Road::Cursor => "Read the cursor forward only and to its end, or run the query without a cursor.",
            Road::PlanKind => "Change the query or the planner settings so that the plan does not need this node.",
        }
    }
}

/// A statement form the entry does not serve: the statement's error (0A000), a refusal (`runner::refusal_of` reads it)
/// that says what is not supported (`Road::what`) and why, with a HINT of what to do instead. Nothing else runs it.
/// D3806 (D3730's follow-up): a cursor refusal (`Road::Cursor`) also logs the `gravity: graph refused (..)` line every
/// named refusal logs (`Backend::note_removed`, under the report), so a test marks it by its name (e2e-refused-by-name.py
/// reads that line's refusal part): a whole-read program a named cursor opens is refused after its admission, at the
/// portal's start, where no admission line names it.
#[cold]
pub(crate) fn removed(road: Road, why: &str) -> Box<types_error::PgError> {
    let why = format!("{}: {why}", road.what()); // alloc: Cold
    if road == Road::Cursor {
        let _ = crate::with_existing_backend(|b| b.note_removed(&why));
    }
    Box::new((*crate::runner::refused(&why)).with_hint(road.hint())) // alloc: Cold
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Created,
    Serving,
    /// Started under EXEC_FLAG_EXPLAIN_ONLY (explain.rs): never run, its relations held to its end.
    Explained,
}

pub struct QueryDesc {
    generation: u16,
    in_use: Cell<bool>,
    pub state: State,
    pstmt: *const PlannedStmt<'static>,
    /// D3198: the statement's text (C's queryDesc->sourceText), for the executor hooks (pg_stat_statements' query text,
    /// auto_explain's Query Text), kept under the seam's retention contract.
    src: (*const u8, usize),
    pub snapshot: Option<Snapshot>,
    crosscheck: Option<Snapshot>,
    pub dest: CommandDest,
    pub params: ParamListHandle,
    query_env: QueryEnvHandle,
    pub instrument_options: i32,
    cplan: CachedPlanHandle,
    pub eflags: i32,
    pub serve: Option<Box<run::Serve>>,
    /// D421: the context of the identity plan copy a CURRENT-OF-eligible cursor is served over (current_of.rs); D3865 L5:
    /// shared with the identity admission, which keeps it while the memo or the shelf holds it (`Admitted::ident`).
    ident: Option<Rc<gravity_driver::mem::root::Root>>,
    /// D3865 L5 / D2710: the first start's identity root while a re-run's plan-order start copies into its own, because an
    /// admission filed in the memo shares the first (`identity_copy`); held until free.
    ident_held: Option<Rc<gravity_driver::mem::root::Root>>,
    /// EXPLAIN ANALYZE of a served plan with run-time pruning: PostgreSQL's initial pruning results (`instrument.rs`'s prune getters).
    pub prune: Option<Box<crate::ops::append::prune::Initial>>,
    /// An EXPLAIN's held relations (`State::Explained`), dropped at its ExecutorEnd.
    explained: Option<Box<explain::Explained>>,
    /// D424: the start of the statement whose FETCH line this desc last logged (one line per FETCH / MOVE).
    fetched: i64,
    /// PERF-OLTP E2: held by a parked portal shell between executions (`executor_finish_and_park` .. `executor_rearm`).
    parked: bool,
    /// PERF-OLTP E2 (V4R-116 condition 2): the grant pool's trim count when the desc parked; a trim since (the pool under
    /// pressure: `GrantPool::trim_depots`) makes the rearm decline, so the portal frees the desc and starts afresh.
    parked_trims: u64,
    /// W9 PERF D1559 S2: a nested serve whose execution may shelve its shed serve at the free (shelf.rs): set at a start
    /// that serves a shelvable nested statement (`Serve::shelvable`), cleared by a failed run, a rewind or a failed finish / end.
    shelf_ok: bool,
    /// v5 errorhold: the plan-order run goes to a receiver the failed licensed run started, no row sent: its startup runs
    /// once (an SPI receiver's second startup is an error), so the served run skips it (`run::executor_run`).
    pub started: bool,
    /// D2926: `run_counted` started this desc again as a partial reader (once: a declined start runs the whole reader).
    counted: bool,
    /// D3381: EXPLAIN ANALYZE of a served write with triggers: its statement level's token (trigger crate `instr`), open
    /// from the start's AFTER trigger query bracket to the finish, and the per-trigger counts the finish took from it
    /// (ExplainPrintTriggers, `instrument::query_desc_trigger_instrument`).
    trig_level: Option<u64>,
    pub triggers: Option<Vec<trigger::instr::StmtTrigRel>>,
    /// D3198: the start tap fired for this execution (`executor_start`, `executor_rearm`): the run, finish and end taps
    /// follow (C's ExecutorStart / Run / Finish / End_hook around the execution).
    hooked: bool,
    /// D3198: C's queryDesc->totaltime: a hook consumer arms it at the start tap; each served ExecutorRun and
    /// ExecutorFinish accumulates into it (standard_ExecutorRun / standard_ExecutorFinish).
    totaltime: Option<Box<types_core::instrument::Instrumentation>>,
    /// D3198: C's es_total_processed: the rows of every served run of this execution.
    total_processed: u64,
    /// B2-31: `zone_restart` started this desc again with no constant converted in the session TimeZone (once).
    zoned: bool,
    /// C5b: a named portal with row marks over a plan that prunes at start: PG's es_unpruned_relids under this execution's
    /// parameters (ExecDoInitialPruning, run at the start), the set `current_of::current_rowmark` reads es_rowmarks by.
    unpruned: Option<Box<[i32]>>,
}

impl QueryDesc {
    pub fn plannedstmt(&self) -> &'static PlannedStmt<'static> {
        raw::plan(self.pstmt)
    }

    fn source_text(&self) -> &'static str {
        raw::text(self.src)
    }
}

struct Descs {
    slots: Vec<Option<Box<QueryDesc>>>,
    free: Vec<u32>,
    generation: u16,
}

// ManuallyDrop (execmain's ENTRIES precedent): a desc's drop reaches seams and contexts, and a panic in a TLS destructor
// aborts the postmaster. The parked descs a session leaves go with their portal shells at its Portals cleanup phase.
leader_tls::leader_tls! {
    thread_local! {
        static DESCS: RefCell<core::mem::ManuallyDrop<Descs>> = const { RefCell::new(core::mem::ManuallyDrop::new(Descs { slots: Vec::new(), free: Vec::new(), generation: 0 })) }; // alloc: Cold
        static PENDING_CPLAN: Cell<CachedPlanHandle> = const { Cell::new(CachedPlanHandle::NULL) };
        static DRAIN: Cell<bool> = const { Cell::new(false) };
        /// D1369: the ExecutorRun / ExecutorFinish calls running on this thread (`Running`).
        static RUNNING: Cell<u32> = const { Cell::new(0) };
    }
}

/// D1506: the session's first desc registers the release of the desc table at its State cleanup phase.
fn ensure_drain() {
    if !DRAIN.replace(true) {
        mcx::register_session_cleanup_phase(mcx::SessionCleanupPhase::State, Box::new(drain_parked)); // alloc: Cold
    }
}

/// The desc table at the session's end. The descs parked portal shells hold are not dropped here: the portal manager's
/// Portals-phase teardown releases every parked shell's desc (`release_query_desc`), while every context and the backend
/// levels are alive, before this State-phase cleanup runs.
fn drain_parked() {
    DRAIN.set(false);
    // D1506: the table's own vectors, once no desc is left in it (a live one stays: its drop is the portal's)
    DESCS.with(|d| {
        let mut d = d.borrow_mut();
        if d.slots.iter().all(Option::is_none) {
            (d.slots, d.free) = (Vec::new(), Vec::new()); // alloc: Cold
        }
    });
}

fn register(qd: QueryDesc) -> PgResult<QueryDescHandle> {
    DESCS.with(|d| {
        let mut d = d.borrow_mut();
        if d.free.is_empty() && d.slots.len() >= MAX_DESCS as usize {
            let msg = format!("more than {MAX_DESCS} nested statements"); // alloc: Cold
            return Err(Box::new(types_error::PgError::error(msg).with_sqlstate(types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED))); // alloc: Cold
        }
        let generation = d.generation.wrapping_add(1) & GEN_MASK;
        d.generation = generation;
        let mut qd = qd;
        qd.generation = generation;
        ensure_drain(); // D1506: every session with a desc drains the table at its end
        let boxed = Box::new(qd); // alloc: Cold
        let idx = match d.free.pop() {
            Some(i) => {
                d.slots[i as usize] = Some(boxed);
                i
            }
            None => {
                d.slots.push(Some(boxed)); // alloc: Cold
                (d.slots.len() - 1) as u32
            }
        };
        Ok(encode(idx, generation))
    })
}

/// D2798: `f` over a desc of ours that is inside `with_desc` now (its own run is suspended further up this thread's stack,
/// evaluating the code that asks), read in the table borrow without `with_desc`'s re-entry claim; `None` when the desc
/// is not in use.
fn running<R>(h: QueryDescHandle, f: impl FnOnce(&QueryDesc) -> R) -> Option<R> {
    let (idx, generation) = decode(h);
    DESCS.with(|d| d.borrow().slots.get(idx as usize).and_then(|s| s.as_deref()).filter(|qd| qd.generation == generation && qd.in_use.get()).map(f))
}

fn remove(h: QueryDescHandle) -> Box<QueryDesc> {
    let (idx, generation) = decode(h);
    DESCS.with(|d| {
        let mut d = d.borrow_mut();
        let taken = match d.slots.get_mut(idx as usize) {
            Some(slot) if slot.as_ref().is_some_and(|q| q.generation == generation) => slot.take().expect("live"),
            _ => panic!("gravity_entry: stale QueryDescHandle {h:?} (already freed)"),
        };
        assert!(!taken.in_use.get(), "gravity_entry: remove of an in-use QueryDescHandle {h:?}");
        d.free.push(idx); // alloc: Cold
        taken
    })
}

fn note_cplan_for_query_desc(cplan: CachedPlanHandle) {
    PENDING_CPLAN.set(cplan);
}

#[allow(clippy::too_many_arguments)]
fn create_query_desc(
    plannedstmt: &PlannedStmt<'_>,
    source_text: &str,
    snapshot: Option<Snapshot>,
    crosscheck_snapshot: Option<Snapshot>,
    dest: CommandDest,
    params: ParamListHandle,
    query_env: QueryEnvHandle,
    instrument_options: i32,
) -> PgResult<QueryDescHandle> {
    let cplan = PENDING_CPLAN.replace(CachedPlanHandle::NULL);
    let snapshot = snapmgr::RegisterSnapshot(snapshot.as_ref())?;
    let crosscheck = snapmgr::RegisterSnapshot(crosscheck_snapshot.as_ref())?;
    let pstmt = raw::retained(plannedstmt);
    register(QueryDesc {
        generation: 0,
        in_use: Cell::new(false),
        state: State::Created,
        pstmt,
        src: (source_text.as_ptr(), source_text.len()),
        snapshot,
        crosscheck,
        dest,
        params,
        query_env,
        instrument_options,
        cplan,
        eflags: 0,
        serve: None,
        ident: None,
        ident_held: None,
        prune: None,
        explained: None,
        fetched: i64::MIN,
        parked: false,
        parked_trims: 0,
        shelf_ok: false,
        started: false,
        counted: false,
        trig_level: None,
        triggers: None,
        hooked: false,
        totaltime: None,
        total_processed: 0,
        zoned: false,
        unpruned: None,
    })
}

/// The instrumented mark on the admission (`admit::instrumented`): an EXPLAIN ANALYZE's admission takes or lowers the
/// statement's instrumented program (its counter cells), filed beside the plain one (admit `instrumented_key`); cleared
/// on every exit. Set only where the mark was off, so a nested admission never clears an outer one's.
struct Instrumenting(bool);

impl Instrumenting {
    fn mark(on: bool) -> Instrumenting {
        Instrumenting(on && !crate::admit::instrumented(true))
    }
}

impl Drop for Instrumenting {
    fn drop(&mut self) {
        if self.0 {
            crate::admit::instrumented(false);
        }
    }
}

/// The nested mark on a start or run (`admit::nested`): set for its extent, the old value put back on every exit, an
/// ERROR that unwinds out of it included.
struct Nesting(bool);

impl Nesting {
    fn enter(on: bool) -> Nesting {
        Nesting(crate::admit::nested(on))
    }
}

impl Drop for Nesting {
    fn drop(&mut self) {
        crate::admit::nested(self.0);
    }
}

/// The explain-only mark on the admission (`admit::explain_only`): set for the entry's admission of an EXPLAIN, cleared on every exit.
struct ExplainOnly(bool);

impl ExplainOnly {
    fn mark(on: bool) -> ExplainOnly {
        if on {
            crate::admit::explain_only(true);
        }
        ExplainOnly(on)
    }
}

impl Drop for ExplainOnly {
    fn drop(&mut self) {
        if self.0 {
            crate::admit::explain_only(false);
        }
    }
}

use crate::admit::stack_met;

/// The conditions the entry decides without the runner: what it cannot drive.
pub fn refusal(operation: CmdType, eflags: i32, snapshot: bool, parallel_mode: bool) -> Option<&'static str> {
    if operation == CmdType::CMD_UTILITY {
        return Some("entry: utility statement");
    }
    if eflags & EXEC_FLAG_EXPLAIN_GENERIC != 0 {
        return Some("entry: explain only (a generic plan whose parameters are unbound)");
    }
    if eflags & (EXEC_FLAG_BACKWARD | EXEC_FLAG_MARK) != 0 {
        return Some(BACKWARD);
    }
    if !snapshot {
        return Some("entry: no snapshot");
    }
    if parallel_mode {
        return Some("entry: parallel mode");
    }
    None
}

/// `refusal`'s scrollable cursor: a cursor road (`Road::Cursor`), the others the entry's own.
const BACKWARD: &str = "entry: backward scan (scrollable cursor)";

/// A statement started inside another: through SPI (a PL body), or on the fmgr_sql road while a served statement runs
/// (a SQL-language body called from a hosted site: `reentry::nested`, some level runs).
fn nested_now() -> bool {
    spi::SPI_connected() || crate::admit::reentry::nested()
}

/// D4313: a hooked ExecutorRun / ExecutorFinish's leave tap (C's PG_FINALLY in pgss_ExecutorRun, explain_ExecutorRun and
/// their Finish twins): fired where the bracket drops it, and on every other way out too, an ERROR that unwinds out of the
/// run (the statement memory limit's out of memory) included, or the modules' nesting level stays +1 for the session.
struct LeaveTap(QueryDescHandle, fn(QueryDescHandle));

impl Drop for LeaveTap {
    fn drop(&mut self) {
        (self.1)(self.0);
    }
}

/// D1369: one ExecutorRun / ExecutorFinish in progress, counted while it runs (an error or unwind drops it too).
struct Running;

impl Running {
    fn enter() -> Running {
        RUNNING.set(RUNNING.get() + 1);
        Running
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.set(RUNNING.get() - 1);
    }
}

/// D1369: the shelf's scope (shelf.rs): a nested statement, or one started while another statement runs (a SQL-language
/// comparator or function body a served statement calls per row). Outside it the statement is top-level and its end drains
/// the shelf.
fn in_statement() -> bool {
    nested_now() || RUNNING.get() > 0
}


/// D2044: a portal a later FETCH can reach (`visible`, as pg_cursors lists it) starting `pstmt`: DECLARE CURSOR, a protocol
/// portal the runner's Bind routes here, and the protocol's unnamed one, which SPI finds by the name '' (cursor_to_xml('', n)
/// fetches n rows of it; a PL/pgSQL refcursor holding ''). The simple query's and EXECUTE's (not visible) run to their end.
fn fetch_portal(pstmt: &'static PlannedStmt<'static>) -> bool {
    let Some(portal) = pquery::ActivePortal() else { return false };
    let p = portal.borrow();
    p.visible && p.queryDesc.is_null() && !p.stmts.is_null() && pquery::stmt_list::resolve(p.stmts).first().is_some_and(|f| core::ptr::eq(f, pstmt))
}

/// `fetch_portal` at a rearm (PortalStartParked): the visible portal whose statement is `pstmt`, its query desc already
/// the retained one.
fn rearmed_portal(pstmt: &'static PlannedStmt<'static>) -> bool {
    let Some(portal) = pquery::ActivePortal() else { return false };
    let p = portal.borrow();
    p.visible && !p.stmts.is_null() && pquery::stmt_list::resolve(p.stmts).first().is_some_and(|f| core::ptr::eq(f, pstmt))
}

/// A serve's `fetch_portal`: `fetch_portal`, or a counted start (`run_counted`, D2926) or a zone restart's (`zone_restart`,
/// B2-31), each of which runs inside the portal's first ExecutorRun, where the portal already holds its query desc: it is
/// the portal a later Execute reaches, so its runs ask for the rows each one owes (D2890's owed cap) and read no further
/// ahead than PostgreSQL computes (a row read ahead would compare in the zone of the run before a SET TimeZone).
fn reached_later(pstmt: &'static PlannedStmt<'static>) -> bool {
    fetch_portal(pstmt) || counted_start(pstmt) || zone_start(pstmt)
}

/// A named cursor's portal (`fetch_portal` with a name: DECLARE's, a PL/pgSQL cursor's, a named protocol portal) starts
/// `pstmt`: its FETCHes take a counted part of the rows at a time (run.rs `Serve::named_cursor`).
pub(crate) fn named_cursor(pstmt: &'static PlannedStmt<'static>) -> bool {
    fetch_portal(pstmt) && pquery::ActivePortal().is_some_and(|p| !p.borrow().name.is_empty())
}

leader_tls::leader_tls! {
    thread_local! {
        /// D2926: the plan whose start `run_counted` makes again as a partial reader (its address; 0: none). Set around that
        /// start only, so a statement the admission runs under SPI (a bind-time word over a PL function) is not read as one.
        static READ_COUNTED: Cell<usize> = const { Cell::new(0) };
        /// D2926: that start found no partial program that serves the count (a refusal, or a program served only to a reader
        /// that takes every row): it left the desc Created, and `run_counted` starts today's whole reader instead.
        static COUNTED_DECLINED: Cell<bool> = const { Cell::new(false) };
        /// B2-31: the plan whose start `zone_restart` makes again with no timestamptz constant converted in the session
        /// TimeZone (its address; 0: none). Set around that start only.
        static ZONE_OFF: Cell<usize> = const { Cell::new(0) };
    }
}

/// B2-31: the admission being made converts no timestamptz constant in the session TimeZone (normalize `Zone::active`,
/// inputs/front.rs): `zone_restart`'s start, or a counted start (`run_counted`), whose rows come over several runs, each
/// compared in the zone of the run that reads it, as PostgreSQL's cross-type compare does.
pub(crate) fn zone_off() -> bool {
    ZONE_OFF.get() != 0 || READ_COUNTED.get() != 0
}

/// B2-31: `zone_restart` is starting this plan again: its admission is neither taken from the memo nor filed under the
/// plan key (the memo's converted program stays for the plan's whole reads in the zone it was converted in).
fn zone_start(pstmt: &PlannedStmt<'_>) -> bool {
    ZONE_OFF.get() == pstmt as *const PlannedStmt<'_> as usize
}

/// B2-31: `ZONE_OFF` for one start of `zone_restart`'s, put back on every way out of it.
struct ZoneScope(usize);

impl ZoneScope {
    fn enter(pstmt: usize) -> ZoneScope {
        ZoneScope(ZONE_OFF.replace(pstmt))
    }
}

impl Drop for ZoneScope {
    fn drop(&mut self) {
        ZONE_OFF.set(self.0);
    }
}

/// The root reader's kind the admission lowers for (decompose `PipelineGraph::partial`, E1 of C1 part 2): a named cursor's
/// portal (`named_cursor`), or, D2926, the protocol's unnamed portal whose first run came under a row count (an Execute's
/// max_rows, an SPI fetch of the portal ''), started again by `run_counted` before anything ran. Every reading of the
/// reader kind takes this: the admission (admit/mod.rs), the memo's take (keyed.rs), the serve's per-FETCH rows.
pub(crate) fn partial_reader(pstmt: &'static PlannedStmt<'static>) -> bool {
    named_cursor(pstmt) || counted_start(pstmt)
}

/// D3828: the start's reader may count the rows it takes before the program's end, as its admission can know: a partial
/// reader (`partial_reader`: a named cursor's portal, `run_counted`'s start) or a nested statement (an SPI fetch's or a
/// PL/pgSQL loop's count, a SQL function's lazy row: `counted_later`'s readers). The admission's counted-pull refusals
/// (runner admission.rs `counted_refused`) read it. The protocol's unnamed portal and the simple query's are admitted as
/// whole readers: an Execute's max_rows reaches their first run, where a program the counted-pull marks make whole-read
/// (`runner::whole_read`) is started again as a partial reader or refused (`counted_partial`, `counted_whole_read`), as
/// PostgreSQL suspends that portal only under such a count. Hibernate's recursive CTEs over pgjdbc (one unbounded Execute)
/// were refused at Bind before.
pub(crate) fn start_counts(pstmt: &'static PlannedStmt<'static>) -> bool {
    partial_reader(pstmt) || crate::admit::is_nested()
}

/// D2926: `run_counted` is starting this plan as a partial reader. Its admission neither raises a refusal (fallback off),
/// nor memoizes one, nor files its program under the plan key (the whole reader's stays there for the uncounted runs).
pub(crate) fn counted_start(pstmt: &PlannedStmt<'_>) -> bool {
    READ_COUNTED.get() == pstmt as *const PlannedStmt<'_> as usize
}

/// D2926: `start_ours` under `run_counted` for this plan whose admission would not serve the count (`a`: none for a
/// refusal; a program served only to a reader that takes every row, or one not lowered partial): true when it declines,
/// and `run_counted` is told so.
fn counted_declines(pstmt: &'static PlannedStmt<'static>, a: Option<&crate::Admitted>) -> bool {
    counted_start(pstmt) && a.is_none_or(|a| crate::runner::whole_read(a) || !a.graph.as_ref().is_some_and(|g| g.partial)) && counted_gives_up(pstmt)
}

/// D2926: a refusal `start_ours` would raise for the partial reader's program (D2044's, D1922's, a grant, a gate):
/// `run_counted` starts the whole reader instead, so no statement the whole reader serves is refused by the re-admission.
fn counted_gives_up(pstmt: &PlannedStmt<'_>) -> bool {
    let gives_up = counted_start(pstmt);
    if gives_up {
        COUNTED_DECLINED.set(true);
    }
    gives_up
}

/// D1922 (the portal-kind fact, shared with D2316 and D2647): how
/// the statement's reader takes its rows, read once when it starts. `partial`: it may stop before the last row, a named
/// cursor's portal a later FETCH reaches (`fetch_portal` with a name, as D2281 reads a named cursor: DECLARE's,
/// PL/pgSQL's, a named protocol portal). `later`: a reader whose count shows only at its run (D3377), a SQL function's
/// row (its receiver: ExecutorRun(1) per lazy row, 0 for a materialized set) or any statement under SPI (an SPI execute's
/// row count: a PL/pgSQL SELECT INTO's 1 or 2, PERFORM's 0), read at the first run as the protocol's unnamed portal under
/// a bounded Execute is (`counted_partial`, `counted_whole_read`, D2678 / D2926), but one whose starter runs it to its end
/// (`runs_whole`: a nested EXPLAIN ANALYZE's own run, a completion run's dest, D3393). `scroll`: that portal may read
/// backward or rewind. `hold`: it outlives its transaction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PortalKind {
    pub partial: bool,
    pub later: bool,
    pub scroll: bool,
    pub hold: bool,
}

impl PortalKind {
    fn of(pstmt: &'static PlannedStmt<'static>, dest: CommandDest, eflags: i32) -> PortalKind {
        let named = named_cursor(pstmt);
        let opts = if named { pquery::ActivePortal().map_or(0, |p| p.borrow().cursorOptions) } else { 0 };
        PortalKind {
            partial: named,
            later: !named && counted_later(dest) && !runs_whole(dest, eflags),
            scroll: opts & CURSOR_OPT_SCROLL != 0 || eflags & (EXEC_FLAG_BACKWARD | EXEC_FLAG_REWIND) != 0,
            hold: opts & CURSOR_OPT_HOLD != 0,
        }
    }
}

/// D3393: a start whose starter runs the desc once, forward, with count 0, to its last row, nested or not: EXPLAIN
/// ANALYZE's own run (ExplainOnePlan marks it EXEC_FLAG_RUN_WHOLE; a PL/pgSQL wrapper's `FOR ln IN EXECUTE 'explain
/// (analyze) ..'` nests it) and a completion run's dest (CREATE TABLE AS, COPY (query) TO, REFRESH MATERIALIZED VIEW:
/// complete.rs, contract C7). Its reader takes every row whatever reads the EXPLAIN's or the utility's own output.
fn runs_whole(dest: CommandDest, eflags: i32) -> bool {
    eflags & EXEC_FLAG_RUN_WHOLE != 0 || matches!(dest, CommandDest::IntoRel | CommandDest::CopyOut | CommandDest::TransientRel)
}

/// D3377: a reader with receiver `dest` whose row count shows only at its run: a SQL function's (lazy or not), or any
/// statement under SPI.
fn counted_later(dest: CommandDest) -> bool {
    dest == CommandDest::SqlFunction || crate::admit::is_nested()
}

/// D1922: a program served only to a reader that takes every row (`runner::whole_read`), started by one that may stop
/// early, raises by this name (as D2044's).
pub const WHOLE_READ: &str = "a program served only to a reader that takes every row (its first row waits on a run PostgreSQL makes as it hands rows on), started by a reader that may stop early: a named cursor's FETCH or a SQL function's lazy row (D1922)";

/// The portal-kind check every road makes (`start_ours` after D2044's, `revive`): a program a family marked for a reader
/// this portal is not raises by the family's name. The portal is read only when a mark is set; a family that needs
/// another reading (D2316 a stop, D2647 a rewind) adds its mark's arm here.
fn portal_refusal(a: &crate::Admitted, pstmt: &'static PlannedStmt<'static>, dest: CommandDest, eflags: i32) -> Option<&'static str> {
    let (whole, rewind) = (crate::runner::whole_read(a), rewind_reruns(a));
    if !whole && !rewind {
        return None;
    }
    let kind = PortalKind::of(pstmt, dest, eflags);
    if whole && kind.partial {
        return Some(WHOLE_READ);
    }
    // D2651: a SCROLL portal's rewind (EXEC_FLAG_REWIND) runs the executor again unless the portal's store replays it:
    // a plan C materializes (D1406) or, for the plans this entry serves, one whose rewind makes no visible call (the
    // KEPT road, `cursor_plan_rewind_replays` below)
    // D2701: or one whose rewind keeps the live execution and runs only the emit again (run.rs `soft_rewind`)
    (rewind && kind.scroll && !rewind_replays(pstmt) && !rewind_kept_replays(pstmt) && !run::soft_rewind(a)).then_some(REWIND_KEPT)
}

/// D2651: a SCROLL portal whose rewind would call a VOLATILE function again where PostgreSQL's rescan keeps its result
/// (lowerv4 effects/visible.rs `rewind_visible`: a randomAccess Sort's, a kept hash table's or a FunctionScan's stored
/// input) raises by this name (as D2044's).
pub const REWIND_KEPT: &str = "a SCROLL cursor whose rewind would call a VOLATILE function again where PostgreSQL keeps its result (a sorted, hashed or stored input: D2651)";

/// D2651: lowering marked the main program or an arm `rewind_reruns` (V4Descs). A graph's InitPlans run once per
/// execution (`keys_done`), so their programs are not read.
fn rewind_reruns(a: &crate::Admitted) -> bool {
    let marked = |p: &gravity_program::Program| p.v4.as_ref().is_some_and(|d| d.rewind_reruns);
    marked(&a.program) || a.graph.as_ref().is_some_and(|g| g.arms.iter().any(|x| marked(&x.program)))
}

/// D2651: the portal's store replays a rewind of `pstmt` (C's planner put a Material over it: D1406), so the executor is
/// never rewound.
fn rewind_replays(pstmt: &PlannedStmt<'_>) -> bool {
    es::cursor_plan_rewind_replays::is_installed() && es::cursor_plan_rewind_replays::call(pstmt)
}

/// D2678 (D1922's open sub-case): a program served only to a reader that takes every row, its first run under a row
/// count: the protocol's unnamed portal under a bounded Execute (max_rows reaches ExecutorRun, after the start
/// `portal_refusal` read; the runner's Bind leaves such a program to this seam) or an SPI fetch of that portal by the name
/// ''. PostgreSQL suspends the portal at the count-th row, its join run only as far as those rows needed. The statement
/// raises by this name (as WHOLE_READ's).
pub const WHOLE_READ_COUNTED: &str = "a program served only to a reader that takes every row (its first row waits on a run PostgreSQL makes as it hands rows on), run first under a row count: an Execute's max_rows on the protocol's portal, where PostgreSQL suspends the portal (D2678)";

/// D2678: a SELECT's whole-read program (`runner::whole_read`) at its first run under a count, before anything ran: no
/// run open, no row, its receiver not started (a plan-order rerun's is: `QueryDesc::started`).
fn counted_whole_read(qd: &QueryDesc, count: u64) -> bool {
    let Some(s) = qd.serve.as_ref() else { return false };
    count > 0 && !qd.started && s.cmd == gravity_program::Cmd::Select && !s.run.is_open() && !s.done && s.rows == 0 && s.sent == 0 && !s.completion && !s.fill_rows && qd.ident.is_none() && qd.prune.is_none() && crate::runner::whole_read(&s.a)
}

/// D2926: the protocol's unnamed portal (or an SPI fetch of the portal '') at its first run under a row count, before
/// anything ran (`counted_whole_read`'s state), over a program the admission lowered for a reader that takes every row
/// (not partial): its group, Unique and merge breaks read their whole input before the first row, where PostgreSQL
/// suspends the portal after `count` rows. `run_counted` starts it again as a partial reader. A named cursor's program
/// is partial from its start (E1). D3377: a whole-read program (`runner::whole_read`) is started again too, before
/// D2678's refusal, by that portal or by a reader whose count shows only at its run (`counted_later`: a SQL function's
/// lazy row, an SPI execute's count), so a partial program that streams the rows the count owes (a drain's exact form:
/// lowerv4 spool_admit.rs `drain_exact`) serves it; one that is whole-read too declines, and the refusal stands.
fn counted_partial(qd: &QueryDesc, count: u64) -> bool {
    let Some(s) = qd.serve.as_ref() else { return false };
    let whole = crate::runner::whole_read(&s.a);
    let reader = !s.named_cursor && (s.fetch_portal || (whole && counted_later(qd.dest)));
    count > 0 && !qd.started && !qd.counted && s.cmd == gravity_program::Cmd::Select && reader && s.a.graph.as_ref().is_some_and(|g| !g.partial)
        && !s.run.is_open() && !s.done && s.rows == 0 && s.sent == 0 && !s.completion && !s.fill_rows && qd.ident.is_none() && qd.prune.is_none() && qd.crosscheck.is_none()
        && (whole || gravity_decompose::facts::stream::partial_stops(qd.plannedstmt()))
}

/// D3377: the desc serves a whole-read program (`runner::whole_read`): `counted_partial` starts it again before D2678's
/// refusal is read.
fn whole_serve(qd: &QueryDesc) -> bool {
    qd.serve.as_ref().is_some_and(|s| crate::runner::whole_read(&s.a))
}

/// D2926: `counted_partial`'s serve released before its first run (nothing ran, no row sent, the receiver not started),
/// the desc back to Created for `run_counted`'s start; its eflags.
/// D3620: out of line, so `run_entry`'s frame (on every nested statement's stack path) does not hold its locals.
#[inline(never)]
fn release_for_counted(qd: &mut QueryDesc) -> PgResult<i32> {
    qd.counted = true;
    run::close(qd, false)?;
    if let Some(serve) = qd.serve.take() {
        let (home, key) = (serve.home, serve.key);
        keyed::release(home, key, qd.cplan, qd.plannedstmt(), serve.a);
    }
    qd.state = State::Created;
    Ok(qd.eflags)
}

/// B2-31: `zone_runs`' serve released before its first run (nothing ran, no row sent, the receiver not started), the desc
/// back to Created for `zone_restart`'s start; its eflags.
/// D3620: out of line, so `run_entry`'s frame (on every nested statement's stack path) does not hold its locals.
#[inline(never)]
fn release_for_zone(qd: &mut QueryDesc) -> PgResult<i32> {
    qd.zoned = true;
    run::close(qd, false)?;
    if let Some(serve) = qd.serve.take() {
        let (home, key) = (serve.home, serve.key);
        keyed::release(home, key, qd.cplan, qd.plannedstmt(), serve.a);
    }
    qd.state = State::Created;
    Ok(qd.eflags)
}

/// B2-31: the statement started again with no timestamptz constant converted in the session TimeZone (`ZONE_OFF`), then
/// run: each run compares its rows in its own zone. Its later runs (a counted portal's D2926 start among them) go as any
/// served statement's.
fn zone_restart(h: QueryDescHandle, eflags: i32, direction: ScanDirection, count: u64, dest: &mut DestReceiver<'_>) -> PgResult<()> {
    executor_start(h, eflags)?;
    executor_run(h, direction, count, dest)
}

/// D2926: the statement started again as a partial reader (`READ_COUNTED`: E1's stop roads, each run asking for the rows
/// it owes, as a named cursor's FETCH n), then run under `count`; later runs (the next Execute's max_rows, 0 for the rest)
/// resume it as a named cursor's later FETCHes do. Never worse than before: when no partial program serves the count
/// (`COUNTED_DECLINED`), the start is today's whole reader again (its program from the memo) and the run is as before.
fn run_counted(h: QueryDescHandle, eflags: i32, direction: ScanDirection, count: u64, dest: &mut DestReceiver<'_>) -> PgResult<()> {
    let pstmt = with_desc(h, |qd| qd.plannedstmt());
    let declined = {
        let scope = CountedScope::enter(pstmt as *const PlannedStmt<'static> as usize);
        executor_start(h, eflags)?;
        scope.declined()
    };
    if declined {
        executor_start(h, eflags)?;
    }
    run_entry(h, direction, count, dest)
}

/// D2926: `READ_COUNTED` and `COUNTED_DECLINED` for one start of `run_counted`'s, put back as they were on every way out of
/// it (its end, an error, a cancel's error, an unwind), so no later start reads its plan as a counted one.
struct CountedScope(usize, bool);

impl CountedScope {
    fn enter(pstmt: usize) -> CountedScope {
        CountedScope(READ_COUNTED.replace(pstmt), COUNTED_DECLINED.replace(false))
    }

    /// The start under this scope declined (`counted_gives_up`): the whole reader starts instead.
    fn declined(&self) -> bool {
        COUNTED_DECLINED.get()
    }
}

impl Drop for CountedScope {
    fn drop(&mut self) {
        READ_COUNTED.set(self.0);
        COUNTED_DECLINED.set(self.1);
    }
}

/// D2926: a top-level start (no run in progress, not nested) that finds `READ_COUNTED` set met a `CountedScope` that
/// outlived its start: cleared, with a WARNING, so the statement starts as its own reader (the witness's simple-protocol
/// statements after each exit of a counted start would show it).
fn counted_leak_check() {
    if READ_COUNTED.get() == 0 || RUNNING.get() > 0 || nested_now() {
        return;
    }
    READ_COUNTED.set(0);
    COUNTED_DECLINED.set(false);
    let mut e = crate::glue::internal("gravity_entry: a counted start's reader flag outlived its start (D2926); cleared");
    e.level = types_error::WARNING;
    elog::emit_error_report_for(&e);
}

/// B2-31: a SELECT holding a timestamptz constant converted in the session TimeZone (normalize retype) at its first run,
/// before anything ran, when its rows may come over several runs or the zone moved since its admission
/// (`run::Serve::zone_elsewhere`): a named cursor's FETCH, the protocol's unnamed portal under an Execute's max_rows or an
/// SPI fetch's count (the runner's Bind leaves a zoned admission to this seam: max_rows comes after it), or a first run
/// after a SET TimeZone. PostgreSQL compares each row in the zone of the run that reads it: the serve is released and the
/// statement started again with no constant converted (`zone_restart`), its compares cross-type as PostgreSQL planned them.
/// Read on `counted_whole_read`'s first-run state, once per desc (a restart that converted again meets run.rs's
/// ZONE_REFUSAL).
fn zone_runs(qd: &QueryDesc, count: u64) -> bool {
    let Some(s) = qd.serve.as_ref() else { return false };
    !qd.zoned && !qd.started && s.cmd == gravity_program::Cmd::Select && !s.run.is_open() && s.rows == 0 && s.sent == 0 && !s.completion && !s.fill_rows && qd.ident.is_none() && qd.prune.is_none() && s.zone_elsewhere(count)
}

/// D2651 (the KEPT road): a rewind of `pstmt` makes no visible call (lowerv4 effects `rewind_quiet`: PostgreSQL's rescan
/// re-evaluates only a quiet chain of Limit / Result / SubqueryScan / Append / scans over what its Sort, Material,
/// FunctionScan, CteScan and InitPlans keep), so replaying the portal's store equals the rescan.
fn rewind_kept_replays(pstmt: &PlannedStmt<'_>) -> bool {
    gravity_lowerv4::effects::visible::rewind_quiet(pstmt, &gravity_expr::PgCatalog)
}

/// D3509 (Workers IO sig-4c3f50662a3c): PostgreSQL's rescan of `pstmt` reads again what its Sort, Material, FunctionScan or
/// CteScan keeps through a chain of IMMUTABLE expressions (lowerv4 effects `rewind_keeps`), so it returns the rows the
/// portal's store holds, where the served program's re-run may order a sort's ties another way or compute a STABLE value
/// again under a changed setting.
fn rewind_kept_rows(pstmt: &PlannedStmt<'_>) -> bool {
    gravity_lowerv4::effects::visible::rewind_keeps(pstmt, &gravity_expr::PgCatalog)
}

/// D2651 (the KEPT road), the wrap of pquery's replay question at a SCROLL portal's rewind (DoPortalRewind, with the
/// portal active): a portal this entry serves with a program marked `rewind_reruns` replays its store when the plan's
/// rewind is quiet (`rewind_kept_replays`), as D2651's arm assumed when it kept the portal served; any portal it serves
/// replays its store when the plan's rewind reads kept rows again (`rewind_kept_rows`). Every other portal keeps
/// execmain's answer over the plan (D1406).
fn cursor_plan_rewind_replays(pstmt: &PlannedStmt<'_>) -> bool {
    (prev().cursor_plan_rewind_replays)(pstmt)
        || pquery::ActivePortal().is_some_and(|p| {
            let h = p.borrow().queryDesc;
            (serving(h, true) && rewind_kept_replays(pstmt)) || (serving(h, false) && rewind_kept_rows(pstmt))
        })
}

/// D3719: a SCROLL cursor with no row marks whose plan C's planner tops with Material, as the plan cannot scan backward
/// (planner.c:444-451; execmain's `cursor_plan_rewind_replays`). execCurrentOf's search_plan_tree does not look through
/// that Material, so PG finds no scan: the cursor is not a simply updatable scan of any table (24000). pgrust's plan has
/// no Material (the portal's store plays it), so its scan would read as reachable.
fn materialized(portal: &types_portal::Portal<'static>, pstmt: &PlannedStmt<'_>) -> bool {
    portal.borrow().cursorOptions & CURSOR_OPT_SCROLL != 0 && pstmt.rowMarks.is_nil() && (prev().cursor_plan_rewind_replays)(pstmt)
}

/// D2651: handle `h` names a desc of ours that serves a program (`marked`: one marked `rewind_reruns`), read without
/// taking it.
fn serving(h: QueryDescHandle, marked: bool) -> bool {
    if !is_ours(h) {
        return false;
    }
    let (idx, generation) = decode(h);
    DESCS.with(|d| {
        d.borrow().slots.get(idx as usize).and_then(|s| s.as_deref()).is_some_and(|qd| {
            qd.generation == generation && qd.state == State::Serving && qd.serve.as_ref().is_some_and(|s| !marked || rewind_reruns(&s.a))
        })
    })
}

fn executor_start(h: QueryDescHandle, eflags: i32) -> PgResult<()> {
    counted_leak_check();
    if !is_ours(h) {
        return Err(foreign_error(h));
    }
    // D3198: C's ExecutorStart_hook runs before standard_ExecutorStart, so a consumer's instrument options (auto_explain's
    // log_analyze) reach the admission and choose its instrumented program. Once per execution: the plan-order starts
    // below and `run_counted`'s start the same execution again.
    if with_desc(h, |qd| qd.state == State::Created && !core::mem::replace(&mut qd.hooked, true)) {
        es::tap_executor_start::call_if(|f| f(h));
    }
    // B2-31: every start of a desc `zone_restart` started again converts nothing in the session TimeZone: its own, a
    // counted start's and the whole reader's after a declined one (`run_counted`), a plan-order start
    let _zone = with_desc(h, |qd| qd.zoned.then(|| ZoneScope::enter(qd.plannedstmt() as *const PlannedStmt<'static> as usize)));
    let _nested = Nesting::enter(nested_now());
    let mut r = start_ours(h, eflags);
    if r.is_ok() && with_desc(h, unserve_unrestartable) {
        r = crate::runner::plan_order(|| start_ours(h, eflags));
    }
    r
}

/// v5 errorhold: a receiver whose startup may run again once no row reached it (`recheck` restarts the statement into it).
fn restartable(dest: CommandDest) -> bool {
    matches!(dest, CommandDest::Remote | CommandDest::RemoteExecute | CommandDest::RemoteSimple | CommandDest::Spi | CommandDest::Tuplestore | CommandDest::None | CommandDest::SqlFunction | CommandDest::CopyOut)
}

/// v5 errorhold: a served program holding a recheck licence whose receiver cannot start again (a table fill's) would
/// have no plan-order run if its licensed departure raised: its serve is released, the desc goes back to Created, and
/// the caller starts the statement again in plan order (PostgreSQL's forms, never filed). `true` when it must.
fn unserve_unrestartable(qd: &mut QueryDesc) -> bool {
    if crate::runner::in_plan_order() || qd.state != State::Serving || restartable(qd.dest) {
        return false;
    }
    if !qd.serve.as_ref().is_some_and(|s| crate::runner::rechecks(&s.a, qd.plannedstmt())) {
        return false;
    }
    if run::close(qd, false).is_err() {
        return false;
    }
    if let Some(serve) = qd.serve.take() {
        let (home, key) = (serve.home, serve.key);
        keyed::release(home, key, qd.cplan, qd.plannedstmt(), serve.a);
    }
    qd.state = State::Created;
    true
}

/// D421 / D2281: the identity copy of `pstmt` (its root emitting the capturable scan's row identity), in the desc's
/// identity root. D2710: the plan-order start of a re-run (`recheck`) copies into the root the first start made, so that
/// start's copy stays valid until free. D3865 L5: the root an identity admission shares (`Admitted::ident`: filed back in
/// the memo at the re-run's release, taken again by the next call) takes no second copy, or every re-run would grow it while
/// the memo keeps it: the plan-order start copies into a root of its own, and the desc holds the first until free.
fn identity_copy(qd: &mut QueryDesc, pstmt: &'static PlannedStmt<'static>) -> PgResult<&'static PlannedStmt<'static>> {
    let fresh = || Rc::new(gravity_driver::mem::root::Root::new("GravityEntryIdentity"));
    let ctx = match qd.ident.take() {
        Some(r) if Rc::strong_count(&r) > 1 => {
            qd.ident_held = Some(r);
            fresh()
        }
        Some(r) => r,
        None => fresh(),
    };
    let copy = current_of::identity_plan(raw::identity_mcx(&ctx), pstmt)?;
    qd.ident = Some(ctx);
    copy.ok_or_else(|| removed(Road::Cursor, current_of::WHY))
}

fn start_ours(h: QueryDescHandle, eflags: i32) -> PgResult<()> {
    // D3360: ExecInitNode's check: each statement nested in a served one's call takes a level and a stack frame chain, so
    // a recursion ends in PostgreSQL's 54001 here, at any depth
    stack_depth_core::check_stack_depth()?;
    with_desc(h, |qd| {
        qd.eflags = eflags;
        if eflags & EXEC_FLAG_EXPLAIN_ONLY != 0 {
            return explain::start(qd, eflags);
        }
        let pstmt = qd.plannedstmt();
        // D3361: a crosscheck snapshot (an RI action under a transaction snapshot) is served: the run hands it to the write
        // hosts (run.rs `executor_run`), which pass it to heap_update / heap_delete as nodeModifyTable.c does
        if let Some(why) = refusal(pstmt.commandType, eflags, qd.snapshot.is_some(), xact::IsInParallelMode()) {
            return Err(removed(if why == BACKWARD { Road::Cursor } else { Road::Entry }, why));
        }
        // D2834: a PL list that leaves a slot to evaluation time (plpgsql's speculative fetch: a field of an unassigned record or
        // one its record lacks, a field whose type changed since planning) is served: the admission keys the slots apart
        // (admit `deferred_ids`) and its program fetches each where it evaluates it, as ExecEvalParamExtern's paramFetch does
        // (gravity_expr emit/host.rs `param_deferred`); a reader that would take one ahead of evaluation refuses by name
        // (gravity_lower `DEFERRED_AHEAD`)
        // D421 / D2281: a CURRENT-OF-eligible cursor is served over the plan copy that emits its row identity
        let orig = pstmt;
        let identity = match current_of::cursor(pstmt) {
            current_of::Cursor::Plain => false,
            current_of::Cursor::Refused(why) => return Err(removed(Road::Cursor, why)),
            current_of::Cursor::Identity => true,
        };
        let completion = complete::eligible(qd.dest, eflags);
        // EXPLAIN ANALYZE (docs/gravity/entry.md §4): the statement's instrumented program (its counter cells; the
        // program caches' take and file under its own key, never the plain program's plan memo) on the plain road (the
        // plain width, the completion driver for a whole read), its run recorded for the printer (`Serve::report`,
        // instrument.rs); never a shelved serve's revival
        let analyze = qd.instrument_options != 0;
        // v5 errorhold: the plan-order admission (every licence denied) neither takes nor files a program or a refusal
        let plan_order = crate::runner::in_plan_order();
        // D2926: never under `run_counted` (its ExecutorRun is in progress): the shelf holds a nested statement's serve
        // B2-31: `zone_restart`'s start (no constant converted in the session TimeZone): no memo take or filing, as `counting`
        // An identity start is neither (its plan copy is never the plan either restarts)
        let counting = !identity && counted_start(pstmt);
        let zoning = !identity && zone_start(pstmt);
        // W9 PERF D1559 S2: the kept road: a nested statement's start the shelf (a shelved serve for the plan's key) and the
        // plan memo serve. D3865 L5: an identity start's too (a PL/pgSQL FOR-IN's or OPEN's portal, called per row), under
        // the plan's identity key (`entry_key_as`): its admission holds its copy (`Admitted::ident`), so a start the shelf or
        // the memo serves makes none, and this start copies only after their look
        let kept_road = !analyze && !completion && in_statement() && !plan_order && !counting && !zoning;
        // D4451: off the kept road too, a start of a cached plan (SQL EXECUTE's portal, a generated name a cursor could
        // take) takes the admission the memo filed under the plan's identity key before any plan copy, and files its own
        let ident_memo = identity && !analyze && !completion && !plan_order && !counting && !zoning;
        let mut ident = match identity && !ident_memo {
            true => Some(identity_copy(qd, orig)?),
            false => None,
        };
        let pstmt = ident.unwrap_or(orig);
        // C5b: a named cursor with row marks over a plan that prunes at start keeps PG's es_unpruned_relids, which its
        // WHERE CURRENT OF reads es_rowmarks by (the pruned leaves' marks are not there)
        qd.unpruned = match !pstmt.rowMarks.is_nil() && !pstmt.partPruneInfos.is_nil() && named_cursor(qd.plannedstmt()) {
            true => Some(crate::ops::append::prune::initial_results(qd.plannedstmt(), qd.params)?.unpruned.into_boxed_slice()), // alloc: Cold
            false => None,
        };
        // D3381: EXPLAIN ANALYZE of a write with triggers: PostgreSQL prints a line per trigger with its calls (and time),
        // which the statement level's counters keep (trigger crate `instr`, opened after the AFTER trigger query bracket
        // below); a write whose lines name relations the served write does not track (several result relations, a
        // partitioned one) is refused (0A000)
        let trig_report = match qd.instrument_options != 0 {
            true => match crate::ops::modifytable::after_query::trigger_report(pstmt, qd.instrument_options)? {
                crate::ops::modifytable::after_query::TriggerReport::Refuse(why) => return Err(removed(Road::ExplainAnalyze, why)),
                crate::ops::modifytable::after_query::TriggerReport::Level(rels) => Some(rels),
                crate::ops::modifytable::after_query::TriggerReport::None => None,
            },
            false => None,
        };
        // EXPLAIN ANALYZE of run-time pruning: the prune getters answer PostgreSQL's initial pruning over this execution's
        // parameters (entry.md §4), pinned for the run's own pruning to check against (`prune::pin`; `finish_pin`, keyed by
        // the desc's own plan: never an identity copy, which only an uninstrumented scroll cursor takes)
        qd.prune = match qd.instrument_options != 0 && !identity && pstmt.partPruneInfos.iter().next().is_some() {
            true => Some(Box::new(crate::ops::append::prune::initial_results(pstmt, qd.params)?)), // alloc: Cold
            false => None,
        };
        let mut pinned = Pinned(qd.prune.as_deref().map(|p| crate::ops::append::prune::pin(pstmt, p)).map(|_| pstmt));
        let params = qd.params;
        let cplan = qd.cplan;
        // W9 PERF D1559 S2: a nested statement's shelved serve for this plan's key (shelf.rs), revived instead of an
        // admission and a Serve built. D3865 L5: an identity start's under the identity key; on a miss, the memo's identity
        // admission is taken here, before this start copies the plan
        let mut taken = None;
        if kept_road {
            let (mut home, mut amcx) = (0, None);
            let got = crate::admit::reentry::enter(Some(crate::admit::reentry::built), None, false, |b, depth| {
                (home, amcx) = (depth, Some(b.admit_mcx()));
                let got = shelf::take(b, depth, cplan, orig, params, identity);
                let memo = match identity && got.as_ref().is_ok_and(|g| g.hit.is_none()) {
                    true => keyed::take(b, cplan, orig, params, true),
                    false => Ok(None),
                };
                (got, memo, depth)
            });
            if let Ok(Some((got, memo, depth))) = got {
                let got = got?;
                drop(got.stale);
                if let Some(serve) = got.hit {
                    if identity {
                        qd.ident = serve.a.ident.clone();
                    }
                    return revive(qd, serve, home, amcx, eflags, orig, params);
                }
                taken = memo?.map(|t| (t, depth));
            }
        }
        // D4451: a top-level identity start's memo take (the kept road's is above)
        if ident_memo && !kept_road {
            if let Ok(Some(memo)) = crate::admit::reentry::enter(Some(crate::admit::reentry::built), None, false, |b, depth| keyed::take(b, cplan, orig, params, true).map(|t| t.map(|t| (t, depth)))) {
                taken = memo?;
            }
        }
        // D3865 L5: a kept-road identity start's copy: the memo's admission's own (`keyed::take` checked it holds one), else
        // made now
        if identity && ident.is_none() {
            ident = Some(match &taken {
                Some(((_, a), _)) => {
                    qd.ident = a.ident.clone();
                    a.pstmt.expect("keyed::take: an identity admission holds its plan copy")
                }
                None => identity_copy(qd, orig)?,
            });
        }
        let pstmt = ident.unwrap_or(orig);
        // D2798: an identity copy whose plan calls user code publishes each row before its quals (lowerv4 frame/publish.rs):
        // that code may say WHERE CURRENT OF this cursor while the run evaluates it
        let publish = run::Publishing::mark(ident.is_some() && !qd.plannedstmt().invalItems.is_nil());
        let (mut home, mut amcx) = (0, None);
        let (t_admit, mut memo) = (analyze.then(gravity_driver::entry::record::now_ns), false);
        // an EXPLAIN ANALYZE admits the instrumented program; one its lowering refuses (an instrumented nest, a per-row
        // SubPlan, a Memoize) runs the plain program, its nodes below the root not counted
        let mut admit = |inst: bool| {
            let _mark = Instrumenting::mark(inst);
            crate::admit::reentry::enter(Some(crate::admit::reentry::built), None, false, |b, depth| -> PgResult<Result<(Box<crate::Admitted>, Option<gravity_program::PlanKey>), String>> {
                (home, amcx) = (depth, Some(b.admit_mcx()));
                // D3865 L5: the memo's identity admission the shelf's look took, at this level (nothing ran between)
                if let Some(((key, a), at)) = taken.take() {
                    debug_assert_eq!(at, depth, "the identity admission's level");
                    memo = true;
                    return Ok(Ok((a, Some(key))));
                }
                let stack0 = stack_depth_core::exceeded_count();
                // the identity copy is never filed or taken under the statement's plan key, nor is the plan-order program;
                // D3865 L5, D4451: an identity admission is filed under the plan's identity key
                if ident.is_some() || plan_order {
                    let key = match identity && ident_memo && !inst {
                        true => crate::runner::entry_key_as(cplan, orig, true),
                        false => None,
                    };
                    // C3 S4c: a top-level one records what it reads (the memo's take checks them)
                    let got = match key.is_some() && !kept_road {
                        true => crate::runner::admit_unkeyed(b, pstmt, params, completion, false),
                        false => crate::admit::unfiled(|| crate::runner::admit_unkeyed(b, pstmt, params, completion, false)),
                    };
                    stack_met(stack0, got.as_ref().is_ok_and(|a| a.is_some()))?;
                    return Ok(got?.map(|a| (Box::new(a), key)).ok_or_else(crate::runner::last_refusal_line)); // alloc: Program
                }
                // the plan memo holds the plain program: the instrumented admission neither takes nor files it
                if !inst && !zoning {
                    if let Some((key, a)) = keyed::take(b, cplan, pstmt, params, false)? {
                        memo = true;
                        return Ok(Ok((a, Some(key))));
                    }
                }
                // D2908: the refusal this plan met at an earlier execution is the statement's error again, without a second
                // lowering
                if !counting && !zoning {
                    if let Some(e) = crate::runner::held_refusal(b, cplan, pstmt) {
                        return Err(e);
                    }
                }
                // C3 S4c: a plan without a plan key is filed nowhere, served or refused: its admission records no reads
                let admitted = if keyed::key_of(cplan, pstmt).is_some() {
                    crate::runner::admit_unkeyed(b, pstmt, params, completion, false)
                } else {
                    crate::admit::unfiled(|| crate::runner::admit_unkeyed(b, pstmt, params, completion, false))
                };
                stack_met(stack0, admitted.as_ref().is_ok_and(|a| a.is_some()))?;
                if let Err(e) = &admitted {
                    // the memo's refusals are the plain program's: an instrumented lowering's refusal is not remembered
                    if let Some(why) = crate::runner::refusal_of(e).filter(|_| !counting && !zoning && !inst) {
                        crate::runner::remember_off_refusal(b, cplan, pstmt, params, why);
                    }
                }
                match admitted? {
                    None => {
                        if !counting && !zoning && !inst {
                            crate::runner::remember_entry_refusal(b, cplan, pstmt);
                        }
                        Ok(Err(crate::runner::last_refusal_line()))
                    }
                    // a parallel program: the driver for a completion run (complete.rs), else the pull adapter runs its producers first (run.rs, chore C1)
                    Some(a) if a.is_parallel() && completion && a.cmd == gravity_program::Cmd::Select => Ok(Ok((Box::new(a), None))), // alloc: Program
                    Some(a) if counting || zoning => Ok(Ok((Box::new(a), None))), // alloc: Program
                    Some(a) if inst => Ok(Ok((Box::new(a), None))), // alloc: Program
                    Some(a) => Ok(Ok((Box::new(a), keyed::key_of(cplan, pstmt)))), // alloc: Program
                }
            }).ok().flatten()
        };
        let mut admitted = admit(analyze);
        // D3381: a routed write whose leaf triggers fire is delegated, never run as the plain program (its trigger lines)
        let routed_triggers = |why: &str| why.contains(gravity_lowerv4::ops::modifytable::strategies::ROUTED_TRIGGER_LINES);
        if analyze && match &admitted { None => true, Some(Ok(Err(why))) => !routed_triggers(why), Some(Err(e)) => crate::runner::refusal_of(e).is_some_and(|w| !routed_triggers(w)), Some(Ok(Ok(_))) => false } {
            admitted = admit(false);
        }
        drop(publish);
        let admit_ns = t_admit.map_or(0, |t| gravity_driver::entry::record::now_ns().saturating_sub(t));
        let (mut a, key) = match admitted {
            None if counted_declines(pstmt, None) => return Ok(()),
            None => return Err(crate::glue::internal("gravity_entry: no backend level for the statement")),
            Some(Err(e)) => return Err(e),
            Some(Ok(Err(_))) if counted_declines(pstmt, None) => return Ok(()),
            Some(Ok(Err(why))) => return Err(crate::runner::refused(&why)),
            Some(Ok(Ok(a))) => a,
        };
        // D3865 L5: an identity admission holds its copy's context (`Admitted::ident`), filed or shelved with it; every
        // release below gives it back under the desc's own plan (`orig`), whose key it was filed under
        if identity && a.ident.is_none() {
            a.ident = qd.ident.clone();
        }
        // D2926: a partial reader's start (`run_counted`) whose program would not serve the count: today's start instead
        if counted_declines(pstmt, Some(&a)) {
            keyed::release(home, key, cplan, orig, a);
            return Ok(());
        }
        // D2044: a store the D1085 retirement serves is served in every portal, a visible one a later FETCH runs under SPI
        // too: a raise after rows of an SPI fetch ends that fetch in the held error (run.rs `PL_LEFT`, keyed on the fetch's
        // receiver), as PG's abort frees what the failed fetch sent SPI
        // D1922: every road's program too (`revive` checks a shelved serve's)
        if let Some(why) = portal_refusal(&a, qd.plannedstmt(), qd.dest, eflags) {
            keyed::release(home, key, cplan, orig, a);
            if counted_gives_up(pstmt) {
                return Ok(());
            }
            return Err(removed(Road::Cursor, why));
        }
        // D622 (A): a nested statement's sites are reserved now, none waited for (a caller holding a grant never waits on its
        // own callee); one that does not fit beside its caller's, or whose floor is past the pool's capacity, takes its
        // floor past the pool (knobs grant.rs `reserve_sites`). D3380: a top-level statement's site whose floor need is past
        // the pool's capacity is admitted alone past the pool (`gravity_driver::Admitted::Solo`), never refused here
        // D866: a table fill with a VOLATILE call is pulled a row at a time (run.rs `fill_rows`), never driven
        let fill_rows = completion && matches!(qd.dest, CommandDest::IntoRel | CommandDest::TransientRel) && gravity_driver::pull::pull_exact(&a.program);
        let drive = completion && a.is_parallel() && !fill_rows;
        crate::runner::grant_start();
        let nested_start = crate::admit::is_nested() && crate::runner::caller_granted();
        let nested = nested_start.then(|| Rc::new(core::cell::RefCell::new(crate::runner::reserve_nested(&a, drive))));
        // D2926: the counted start checks no permission, read-only guard or EXECUTE gate again: PostgreSQL made them at the
        // portal's ExecutorStart (a SET ROLE between the Bind and the Execute changes nothing), and this desc's first start did
        if !counting {
            if let Err(e) = prologue(pstmt, &a, amcx) {
                keyed::release(home, key, cplan, orig, a);
                return Err(e);
            }
        }
        // D2945: the execution's es_query_cxt: values the run evaluates into it (a parallel producer's late words,
        // `admit::eval_late`: an ARRAY[...] of the statement's parameters) stay in it until the shed resets it, as
        // FreeExecutorState deletes es_query_cxt with them, so its reset is wholesale
        let ctx = gravity_driver::mem::root::Root::wholesale("GravityEntry");
        if eflags & EXEC_FLAG_SKIP_TRIGGERS == 0 {
            crate::ops::modifytable::after_query::begin(a.cmd);
            if let Some(rels) = trig_report {
                qd.trig_level = Some(trigger::instr::begin(qd.instrument_options, rels)?);
            }
        }
        let mut serve = run::Serve::new(a, ctx)?;
        serve.hold(params)?;
        if ident.is_some() {
            serve.identity()?;
        }
        serve.home = home;
        serve.pin = crate::admit::reentry::pin(home);
        serve.a.query_env = qd.query_env; // V4R-246: this execution's ENRs (a Named / transition-table read, exec v4/m5.rs)
        serve.key = key;
        serve.completion = completion;
        serve.fill_rows = fill_rows;
        serve.nested = nested;
        serve.report = analyze.then(|| instrument::report_at_start(&serve.a, pstmt, admit_ns, memo));
        serve.named_cursor = partial_reader(qd.plannedstmt());
        serve.fetch_portal = reached_later(qd.plannedstmt());
        // D3865 L5: an identity serve too, once filed under the identity key (`Serve::shelvable`)
        qd.shelf_ok = in_statement() && !crate::runner::once_store(&serve.a) && serve.shelvable() && qd.prune.is_none();
        qd.serve = Some(serve);
        qd.state = State::Serving;
        pinned.0 = None;
        Ok(())
    })
}

/// D3881: the serves shelved at levels `depth` and past, dropped (their levels are freed at their caller's run end).
pub(crate) fn retire_shelved_from(depth: usize) {
    shelf::retire_from(depth);
}

/// W9 PERF D1559 S2: the start of a revived shelved serve (shelf.rs): what `start_ours` does after its admission for a
/// nested non-completion statement (the grant check and the nested reservation, the read-only guard, the after-query
/// begin, the relations held under this execution's params), the shed serve re-armed in place of `Serve::new`.
fn revive(qd: &mut QueryDesc, mut serve: Box<run::Serve>, home: usize, amcx: Option<Mcx<'_>>, eflags: i32, pstmt: &'static PlannedStmt<'static>, params: ParamListHandle) -> PgResult<()> {
    let cplan = qd.cplan;
    let key = serve.key;
    if let Some(why) = portal_refusal(&serve.a, pstmt, qd.dest, eflags) {
        keyed::release(home, key, cplan, pstmt, serve.a);
        return Err(removed(Road::Cursor, why));
    }
    if let Err(e) = serve.recid() {
        keyed::release(home, key, cplan, pstmt, serve.a);
        return Err(e);
    }
    // D622 (b): a nested statement's floor past the pool's capacity overdraws too; D3380: a top-level one runs alone past
    // the pool (`gravity_driver::Admitted::Solo`)
    crate::runner::grant_start();
    let nested_start = crate::runner::caller_granted();
    let nested = nested_start.then(|| Rc::new(core::cell::RefCell::new(crate::runner::reserve_nested(&serve.a, false))));
    let cmd = serve.a.cmd;
    if let Err(e) = prologue(pstmt, &serve.a, amcx) {
        keyed::release(home, key, cplan, pstmt, serve.a);
        return Err(e);
    }
    if eflags & EXEC_FLAG_SKIP_TRIGGERS == 0 {
        crate::ops::modifytable::after_query::begin(cmd);
    }
    serve.sent = 0; // shed at the shelving (`Serve::shed`): the rest of the per-execution state is `new`'s
    serve.home = home;
    serve.pin = crate::admit::reentry::pin(home);
    serve.a.query_env = qd.query_env;
    serve.completion = false;
    serve.fill_rows = false;
    serve.nested = nested;
    serve.report = None;
    serve.named_cursor = partial_reader(pstmt);
    serve.fetch_portal = reached_later(pstmt);
    // D761: the relations held under this execution's params; on an error the serve goes with the desc (its admission
    // back to the memo at the free, as any start's)
    let held = serve.hold(params);
    qd.serve = Some(serve);
    qd.state = State::Serving;
    qd.shelf_ok = held.is_ok();
    held
}

/// The one per-execution prologue of every entry start (`start_ours`: a fresh admission or a keyed take; `revive`; `rearm`),
/// in standard_ExecutorStart's order: the read-only guard (ExecCheckXactReadOnly), the relation permissions (InitPlan's
/// ExecCheckPermissions), the program's EXECUTE gates (ExecInitNode's: a DECLARE raises the denial, not its first FETCH,
/// D2219). A VALUES row's gates are the program's, at the row's visit (D2775: lowerv4 ops/values `gate_row`). Outside the
/// backend's borrow (the catalog reads may run invalidations). `amcx`: the admit context of the level the start just
/// left, the one `with_backend` would enter again.
fn prologue(pstmt: &PlannedStmt<'_>, a: &crate::Admitted, amcx: Option<Mcx<'_>>) -> PgResult<()> {
    match amcx {
        Some(mcx) => prologue_in(pstmt, a, mcx),
        None => crate::with_backend(|b| b.admit_mcx()).map_or(Ok(()), |mcx| prologue_in(pstmt, a, mcx)),
    }
}

fn prologue_in(pstmt: &PlannedStmt<'_>, a: &crate::Admitted, mcx: Mcx<'_>) -> PgResult<()> {
    crate::runner::start_checks(pstmt, a.cmd, mcx)?;
    crate::runner::check_gates(a, mcx)
}

/// The prune pin of a start that did not end serving (a refusal, an error): removed on the way out.
struct Pinned(Option<&'static PlannedStmt<'static>>);

impl Drop for Pinned {
    fn drop(&mut self) {
        if let Some(p) = self.0 {
            crate::ops::append::prune::unpin(p);
        }
    }
}

/// ExecutorFinish of a served EXPLAIN ANALYZE with run-time pruning: every info with initial steps was pruned by the run
/// against the pin (`prune::initial_valid` checked the set; a different one already failed the run), else the printed
/// `Subplans Removed` would not be what ran.
fn finish_pin(qd: &mut QueryDesc) -> PgResult<()> {
    let Some(p) = qd.prune.as_deref() else { return Ok(()) };
    let checked = crate::ops::append::prune::unpin(qd.plannedstmt()).unwrap_or_default();
    match p.valid.iter().enumerate().find(|&(i, v)| v.is_some() && checked.get(i).copied().unwrap_or(0) == 0) {
        Some((i, _)) => Err(crate::glue::internal(&format!("gravity: EXPLAIN ANALYZE's initial pruning of partPruneInfos[{i}] is not the run's (the run did not prune it)"))), // alloc: Cold
        None => Ok(()),
    }
}

/// D3198: ExecutorRun with C's hook and totaltime bracket for a statement the entry serves (`run_entry` is the run):
/// the run tap, InstrStartNode(totaltime), the run, InstrStopNode(totaltime, es_processed) and es_total_processed, the
/// run-leave tap on every way out (PG_FINALLY).
fn executor_run(h: QueryDescHandle, direction: ScanDirection, count: u64, dest: &mut DestReceiver<'_>) -> PgResult<()> {
    if !is_ours(h) || !with_desc(h, |qd| qd.hooked && qd.state == State::Serving) {
        return run_entry(h, direction, count, dest);
    }
    executor_run_hooked(h, direction, count, dest)
}

/// `executor_run` of a hooked statement. D3620: out of line, so an unhooked run (every level of a nested statement's
/// stack path) holds none of its frame.
#[inline(never)]
fn executor_run_hooked(h: QueryDescHandle, direction: ScanDirection, count: u64, dest: &mut DestReceiver<'_>) -> PgResult<()> {
    es::tap_executor_run::call_if(|f| f(h));
    let leave = LeaveTap(h, |h| es::tap_executor_run_leave::call_if(|f| f(h)));
    let timed = with_desc(h, |qd| qd.totaltime.as_deref_mut().map(::instrument::instr_start_node).is_some());
    let r = run_entry(h, direction, count, dest);
    with_desc(h, |qd| {
        let n = if qd.state == State::Serving { qd.serve.as_ref().map_or(0, |s| s.es_processed) } else { 0 };
        qd.total_processed += n;
        if let Some(t) = qd.totaltime.as_deref_mut().filter(|_| timed) {
            ::instrument::instr_stop_node(t, n as f64);
        }
    });
    drop(leave);
    r
}

fn run_entry(h: QueryDescHandle, direction: ScanDirection, count: u64, dest: &mut DestReceiver<'_>) -> PgResult<()> {
    stack_depth_core::check_stack_depth()?;
    let _running = Running::enter();
    if !is_ours(h) {
        return Err(foreign_error(h));
    }
    let nested = Nesting::enter(nested_now());
    let mut again = None;
    let mut counted = None;
    let mut zoned = None;
    let mut r = with_desc(h, |qd| match qd.state {
        State::Serving if counted_partial(qd, count) && whole_serve(qd) => release_for_counted(qd).map(|eflags| counted = Some(eflags)),
        State::Serving if counted_whole_read(qd, count) => Err(removed(Road::Cursor, WHOLE_READ_COUNTED)),
        State::Serving if zone_runs(qd, count) => release_for_zone(qd).map(|eflags| zoned = Some(eflags)),
        State::Serving if counted_partial(qd, count) => release_for_counted(qd).map(|eflags| counted = Some(eflags)),
        State::Serving => {
            // D2573: the PL state the run starts with; an error that left any behind (a PL/pgSQL function's open SPI
            // connection) never re-runs in plan order: it is the statement's
            let pl = crate::runner::PlState::now();
            let r = run::executor_run(qd, direction, count, dest).or_else(|e| {
                if !pl.kept() {
                    return Err(e);
                }
                match recheck(qd, e, dest) {
                    Ok(first) => {
                        again = Some(first);
                        Ok(())
                    }
                    Err(e) => Err(unrechecked(qd, e)),
                }
            });
            // D4131: a run that went on may have dropped an error held inside user code (a pgrcolumnar comparand
            // evaluated at the open, a window argument at the fill, on rows PostgreSQL never reaches): what that callee
            // left (its SPI connection) is undone, as the fetch undoes it (run.rs `step_counted`, D2126); the open runs
            // before the fetch takes its own PL state
            if r.is_ok() && again.is_none() && !pl.kept() {
                pl.restore();
            }
            qd.shelf_ok &= r.is_ok() && again.is_none(); // S2: a failed run's serve is never shelved
            r
        }
        State::Created => Err(crate::glue::internal("gravity_entry: ExecutorRun before ExecutorStart")),
        State::Explained => Err(crate::glue::internal("gravity_entry: ExecutorRun of an EXPLAIN without ANALYZE")),
    });
    drop(nested);
    if let Err(e) = &r {
        crate::runner::note_tag_refused(e); // D4206: a run-ahead error's refusal is the statement's here
        // so is an out of memory a SPILL-DEBT row names: its server-only line
        if let Err(e) = &mut r {
            crate::glue::note_debt_refused(e);
        }
    }
    if let (Ok(()), Some(eflags)) = (&r, counted) {
        return run_counted(h, eflags, direction, count, dest);
    }
    if let (Ok(()), Some(eflags)) = (&r, zoned) {
        return zone_restart(h, eflags, direction, count, dest);
    }
    let Some((eflags, first)) = again else { return r };
    rerun_in_plan_order(h, direction, count, dest, eflags, first)
}

/// `run_entry`'s errorhold re-run in plan order. D3620: out of line, so `run_entry`'s frame (on every nested statement's stack
/// path) does not hold its locals.
#[inline(never)]
fn rerun_in_plan_order(h: QueryDescHandle, direction: ScanDirection, count: u64, dest: &mut DestReceiver<'_>, eflags: i32, first: Box<types_error::PgError>) -> PgResult<()> {
    // v5 errorhold (interfaces §2.3, `Batch::Statement`) on the portal path: the statement again under the desc's
    // snapshot, admitted and run in plan order (every licence denied; bounded under a count: a cursor's FETCH n, D2316);
    // its rows or its error are the statement's. The first run's receiver startup sent the RowDescription: this one sends
    // none again. Any other receiver's startup ran once, no row after it: the served run skips a second one
    // (`QueryDesc::started`). A plan-order start that is not served (it raised) gives the first run's error back where
    // its receiver's startup cannot run again; a FETCH's tuplestore, a MOVE's, a SQL function's or a COPY's may (D2630)
    let described = match dest {
        DestReceiver::PrintTup(dr) => Some(core::mem::replace(&mut dr.sendDescrip, false)),
        _ => None,
    };
    let once = described.is_none();
    let restarts = matches!(dest, DestReceiver::Tuplestore(_) | DestReceiver::DoNothing | DestReceiver::SqlFunction(_) | DestReceiver::CopyOut(_));
    let mut first = Some(first);
    let r = crate::runner::plan_order_for(count, || {
        executor_start(h, eflags)?;
        if once && !restarts && with_desc(h, |qd| qd.state != State::Serving) {
            return Err(first.take().expect("the first run's error"));
        }
        with_desc(h, |qd| qd.started = once);
        run_entry(h, direction, count, dest)
    });
    if let (Some(was), DestReceiver::PrintTup(dr)) = (described, dest) {
        dr.sendDescrip = was;
    }
    // a refusal of the plan-order run (its start's admission, a run-time one) never hides the first run's resource error
    // (`runner::surfaces_over_refusal`): an out-of-memory is the statement's
    match (r, first) {
        (Err(e), Some(first)) if crate::runner::refusal_of(&e).is_some() && crate::runner::surfaces_over_refusal(&first) => Err(first),
        (r, _) => r,
    }
    // the first run's error given back is the statement's (an out of memory a SPILL-DEBT row names: its line)
    .map_err(|mut e| {
        crate::glue::note_debt_refused(&mut e);
        e
    })
}

/// v5 errorhold: the run of a program holding a recheck licence failed before any row reached a receiver whose startup
/// may run again (a SELECT, no row lock: nothing a second run repeats; a COPY (query) TO's completion run, D3543, while its
/// receiver has taken no row): the serve is closed and released and the desc
/// goes back to Created for the plan-order start (`Ok((eflags, the error))`). Anything else gives the error back. A
/// CURRENT-OF-eligible cursor (served over its identity copy) re-runs too: its plan-order start copies into the same
/// identity root (D2710: its first FETCH raised the licensed departure's error where PostgreSQL returns rows).
/// D3620: out of line, so `run_entry`'s frame (on every nested statement's stack path) does not hold its locals.
#[inline(never)]
fn recheck(qd: &mut QueryDesc, e: Box<types_error::PgError>, dest: &DestReceiver<'_>) -> Result<(i32, Box<types_error::PgError>), Box<types_error::PgError>> {
    // D3134: postgres --single's DestDebug too: its startup printed the column header once, and the served plan-order run
    // skips a second startup (`QueryDesc::started`), as for the other receivers whose startup must not run twice
    let restart = matches!(dest, DestReceiver::PrintTup(_) | DestReceiver::Tuplestore(_) | DestReceiver::DoNothing | DestReceiver::SqlFunction(_) | DestReceiver::CopyOut(_) | DestReceiver::SpiPrintTup | DestReceiver::DebugTup(_));
    let Some(serve) = qd.serve.as_ref() else { return Err(e) };
    // D3543: a COPY (query) TO's completion run re-runs too while its receiver has taken no row (a table fill's never: its
    // rows went into the relation)
    let unsent = !serve.completion || matches!(dest, DestReceiver::CopyOut(st) if st.processed == 0);
    if !restart || !crate::runner::reruns(&e) || serve.sent != 0 || !unsent || serve.fill_rows || qd.crosscheck.is_some() || qd.prune.is_some() || !crate::runner::rechecks(&serve.a, qd.plannedstmt()) {
        return Err(e);
    }
    if run::close(qd, false).is_err() {
        return Err(e);
    }
    crate::runner::note_recheck(e.message());
    if let Some(serve) = qd.serve.take() {
        let (home, key) = (serve.home, serve.key);
        keyed::release(home, key, qd.cplan, qd.plannedstmt(), serve.a);
    }
    qd.state = State::Created;
    Ok((qd.eflags, e))
}

/// v5 errorhold: a licensed program's error before any row that `recheck` could not re-run in plan order is a refusal
/// (`RECHECK`), never an error PostgreSQL's order may not raise; a resource error is the statement's
/// (`runner::surfaces_over_refusal`). Any other error is the statement's as it is.
/// D3620: out of line, so `run_entry`'s frame (on every nested statement's stack path) does not hold its locals.
#[inline(never)]
fn unrechecked(qd: &QueryDesc, e: Box<types_error::PgError>) -> Box<types_error::PgError> {
    const RECHECK: &str = "recheck: a licensed departure raised before any row and its plan-order run cannot start here";
    let licensed = crate::runner::reruns(&e) && qd.serve.as_ref().is_some_and(|s| s.sent == 0 && crate::runner::rechecks(&s.a, qd.plannedstmt()));
    if licensed && !crate::runner::surfaces_over_refusal(&e) {
        return crate::runner::refused(RECHECK);
    }
    e
}

fn executor_finish(h: QueryDescHandle) -> PgResult<()> {
    let _running = Running::enter();
    if !is_ours(h) {
        return Err(foreign_error(h));
    }
    // D3198: C's ExecutorFinish_hook bracket for a served statement, the totaltime window over its AFTER triggers
    // (standard_ExecutorFinish: InstrStartNode .. AfterTriggerEndQuery .. InstrStopNode(totaltime, 0))
    if !with_desc(h, |qd| qd.hooked && qd.state == State::Serving) {
        return finish_entry(h);
    }
    es::tap_executor_finish::call_if(|f| f(h));
    let leave = LeaveTap(h, |h| es::tap_executor_finish_leave::call_if(|f| f(h)));
    let timed = with_desc(h, |qd| qd.totaltime.as_deref_mut().map(::instrument::instr_start_node).is_some());
    let r = finish_entry(h);
    with_desc(h, |qd| {
        if let Some(t) = qd.totaltime.as_deref_mut().filter(|_| timed) {
            ::instrument::instr_stop_node(t, 0.0);
        }
    });
    drop(leave);
    r
}

fn finish_entry(h: QueryDescHandle) -> PgResult<()> {
    with_desc(h, |qd| match qd.state {
        State::Serving => {
            let r = crate::ops::modifytable::after_query::finish(qd.serve.as_ref().expect("serving").cmd, qd.eflags & EXEC_FLAG_SKIP_TRIGGERS != 0).and_then(|_| finish_pin(qd));
            // D3381: the statement's trigger counts, its AFTER events fired (ExplainPrintTriggers reads them next)
            if let Some(t) = qd.trig_level.take() {
                qd.triggers = trigger::instr::take(t);
            }
            qd.shelf_ok &= r.is_ok();
            r
        }
        _ => Ok(()),
    })
}

fn executor_end(h: QueryDescHandle) -> PgResult<()> {
    if !is_ours(h) {
        return Err(foreign_error(h));
    }
    // D3198: C's ExecutorEnd_hook (before standard_ExecutorEnd; its ERROR skips it) for a statement the entry served or
    // explained: pg_stat_statements counts it, auto_explain logs its plan
    if with_desc(h, |qd| core::mem::replace(&mut qd.hooked, false)) {
        es::tap_executor_end::call_if_or(Ok(()), |f| f(h))?;
    }
    with_desc(h, |qd| {
        // PostgreSQL's standard_ExecutorEnd: the parallel workers a Gather planned and the ones that ran (the workers the
        // run's sections ran; none at W0)
        if let Some(s) = qd.serve.as_ref().filter(|s| s.done) {
            gravity_hosts::instr::report_parallel_workers(gravity_decompose::explain::gather_workers(qd.plannedstmt()), s.workers as i64);
        }
        qd.explained = None;
        if qd.prune.take().is_some() {
            crate::ops::append::prune::unpin(qd.plannedstmt());
        }
        if let Some(t) = qd.trig_level.take() {
            trigger::instr::drop_level(t);
        }
        // construct C2's run-ahead tracer: a whole-read program (the whole-read licence's guard) whose reader ended its
        // run before it completed
        if gravity_driver::ahead_trace::mode() != gravity_driver::ahead_trace::Mode::Off {
            if let Some(s) = qd.serve.as_ref().filter(|s| gravity_driver::ahead_trace::cut_short(crate::runner::whole_read(&s.a), s.run.is_open(), s.done)) {
                gravity_driver::ahead_trace::whole_read_cut(s.shape);
            }
        }
        // D3865 L2: a run still open here was cut short by its caller's count (fmgr_sql's one row, SELECT INTO, a lazy
        // SQL function's early end); `close` ends it as PostgreSQL's ExecutorEnd ends an unfinished scan, and a clean
        // end is shelved as a finished run's is
        qd.shelf_ok &= qd.serve.is_some();
        let r = run::close(qd, true);
        qd.shelf_ok &= r.is_ok();
        r
    })
}

fn executor_rewind(h: QueryDescHandle) -> PgResult<()> {
    if !is_ours(h) {
        return Err(foreign_error(h));
    }
    with_desc(h, |qd| {
        qd.shelf_ok = false; // S2: a rewound serve is never shelved
        run::rewind(qd)
    })
}

/// Portal retention (PERF-OLTP E2, as execmain's `executor_finish_and_park_seam`): a retainable serve (`Serve::retainable`:
/// a generic plan's SELECT under its plan key) ends its run — the driver parks the Node under the key — and stays in the
/// desc with its filed program, adapter and result descriptor; the snapshot registration ends here (rearm registers the
/// next one) and the params are the portal's, freed with it. Any other served statement ends and frees: `false`.
/// Bounds (V4R-116 condition 2): a parked desc is held only by a parked portal shell, and portalmem keeps at most
/// `PARKED_PORTAL_MAX` (8) shells per session, least recently used out first (a rearm takes the shell, the next park
/// pushes it last), as PostgreSQL's retained executors; the per-execution state and context are shed here, so a
/// parked desc holds its filed program, adapter and result descriptor only; the session's end drains them
/// (`drain_parked`) and a grant pool trim since the park declines the rearm (`pool_trims`).
fn executor_finish_and_park(h: QueryDescHandle) -> PgResult<bool> {
    if !is_ours(h) {
        return Err(foreign_error(h));
    }
    // D3198: this path stands for ExecutorFinish + ExecutorEnd (as execmain's `executor_finish_and_park_seam` fired the
    // same taps): the finish taps here (a retainable serve is a SELECT: nothing to finish), the end tap in `executor_end`
    if with_desc(h, |qd| qd.hooked && qd.state == State::Serving) {
        es::tap_executor_finish::call_if(|f| f(h));
        es::tap_executor_finish_leave::call_if(|f| f(h));
    }
    // a retained serve skips ExecutorFinish (`after_query::finish`): its pgrc read counts land here
    crate::runner::pgrc_statement_end();
    executor_end(h)?;
    let parked = with_desc(h, |qd| {
        let keep = qd.state == State::Serving && qd.prune.is_none() && qd.ident.is_none() && qd.crosscheck.is_none() && qd.serve.as_ref().is_some_and(|s| s.retainable());
        if keep {
            snapmgr::UnregisterSnapshot(qd.snapshot.take().as_ref());
            qd.params = ParamListHandle::NULL;
            qd.parked = true;
            qd.parked_trims = pool_trims();
            qd.serve.as_mut().expect("retainable").shed();
        }
        keep
    });
    if !parked {
        free_query_desc(h);
        return Ok(false);
    }
    ensure_drain();
    Ok(true)
}

/// The grant pool's depot trims so far (its pressure path, `GrantPool::trim_depots`).
pub(crate) fn pool_trims() -> u64 {
    gravity_driver::mem::grant::POOL.stats().trims
}

fn executor_rearm(h: QueryDescHandle, snapshot: Option<Snapshot>, params: ParamListHandle) -> PgResult<bool> {
    if !is_ours(h) {
        return Err(foreign_error(h));
    }
    let rearmed = with_desc(h, |qd| -> PgResult<bool> {
        match qd.state {
            State::Serving => {
                let rearmed = rearm(qd, snapshot, params)?;
                if rearmed {
                    // D3198: a fresh execution of the retained serve (C has no rearm: its start tap stands for
                    // ExecutorStart, as execmain's `executor_rearm_seam` fired it)
                    (qd.hooked, qd.totaltime, qd.total_processed) = (true, None, 0);
                }
                Ok(rearmed)
            }
            State::Created | State::Explained => Ok(false),
        }
    })?;
    if rearmed {
        es::tap_executor_start::call_if(|f| f(h));
    }
    Ok(rearmed)
}

/// PERF-OLTP E2: the retained serve re-armed for this execution, or `false` (the portal releases the desc and starts
/// afresh: an admission decides again). What `start_ours` and `keyed::take` check on every execution is checked here
/// before anything is taken: the entry's switch, the refusals, the retained admission not overtaken by a support change
/// (D2663: `keyed::take`'s `take_served` drops such an admission), the memo still serving the plan's key (an invalidation
/// dropped it: the next start re-admits), the permission checks (a REVOKE between executions) and the read-only guard.
fn rearm(qd: &mut QueryDesc, snapshot: Option<Snapshot>, params: ParamListHandle) -> PgResult<bool> {
    let pstmt = qd.plannedstmt();
    let Some(key) = qd.serve.as_ref().filter(|s| s.retainable() && !s.run.is_open()).and_then(|s| s.key) else { return Ok(false) };
    if nested_now() || refusal(pstmt.commandType, qd.eflags, snapshot.is_some(), xact::IsInParallelMode()).is_some() || types_portal::params::defers_to_evaluation(params) {
        return Ok(false);
    }
    // D2663: a support proc changed since the admission (a pg_amproc row, a pg_proc row under its OID). The plan keeps its
    // identity (PostgreSQL's plancache follows neither), so the key may be served again, by a newer admission, while this
    // shell holds the old one: its programs and Node bound to the old support procs and language handlers
    if crate::Backend::serve_stale(&qd.serve.as_ref().expect("retained").a) || !crate::admit::folded_hold(&qd.serve.as_ref().expect("retained").a, params) {
        return Ok(false);
    }
    if !crate::with_backend(|b| matches!(b.lookup(key), Some(crate::Verdict::Served(_)))).unwrap_or(false) {
        return Ok(false);
    }
    // the grant pool trimmed its depots since the park (memory pressure): the retained serve goes with the portal's release
    if qd.parked_trims != pool_trims() {
        return Ok(false);
    }
    prologue(pstmt, &qd.serve.as_ref().expect("retained").a, None)?;
    qd.snapshot = snapmgr::RegisterSnapshot(snapshot.as_ref())?;
    qd.params = params;
    qd.fetched = i64::MIN;
    qd.parked = false;
    qd.serve.as_mut().expect("retained").rearm();
    qd.serve.as_mut().expect("retained").named_cursor = named_cursor(pstmt);
    // B2-31 / D2890: the portal a rearm starts already holds this desc (PortalStartParked), where a fresh start finds
    // none: the visible portal of the plan is reached by its later Executes all the same (`rearmed_portal`)
    qd.serve.as_mut().expect("retained").fetch_portal = fetch_portal(pstmt) || rearmed_portal(pstmt);
    qd.serve.as_mut().expect("retained").hold(params)?;
    Ok(true)
}

fn release_query_desc(h: QueryDescHandle) {
    if !is_ours(h) {
        foreign(h);
    }
    let mut qd = remove(h);
    drop(run::close(&mut qd, false));
    // D2190: the failed execution's admission is dropped here, never filed back; the Node close parked under its key was
    // built over it
    if let Some(serve) = qd.serve.take() {
        keyed::forsake(serve.home, serve.key, serve.a);
    }
    // S2: a top-level statement's end drops the serves its nested calls kept (shelf.rs)
    if !in_statement() {
        shelf::end_of_statement();
    }
}

fn free_query_desc(h: QueryDescHandle) {
    free_ours(h);
    // S2: a top-level statement's end drops the serves its nested calls kept (shelf.rs)
    if !in_statement() {
        shelf::end_of_statement();
    }
}

fn free_ours(h: QueryDescHandle) {
    if !is_ours(h) {
        foreign(h);
    }
    let mut qd = remove(h);
    let closed = run::close(&mut qd, true);
    let mut serve = qd.serve.take();
    // W9 PERF D1559 S2: a nested serve that ran clean is shed and shelved for the key's next execution (shelf.rs)
    if closed.is_ok() && qd.shelf_ok && qd.state == State::Serving {
        if let Some(mut s) = serve.take() {
            s.shed();
            serve = shelf::put(qd.cplan, qd.plannedstmt(), s);
        }
    }
    if let Some(serve) = serve {
        // D1216: the admission moves out of the box alone (`*serve` copied the whole Serve out first)
        let (home, key) = (serve.home, serve.key);
        keyed::release(home, key, qd.cplan, qd.plannedstmt(), serve.a);
    }
    snapmgr::UnregisterSnapshot(qd.snapshot.take().as_ref());
    snapmgr::UnregisterSnapshot(qd.crosscheck.take().as_ref());
}

/// D3198: the hook view of a desc (`execmain_seams::with_hook_qd`): every query desc is the entry's; its parallel workers
/// as `executor_end` reports them (the workers its Gathers planned, the ones its run's sections ran).
fn with_hook_qd(h: QueryDescHandle, f: &mut dyn FnMut(&mut es::HookQd<'_>)) {
    route(h, |qd| {
        let (to_launch, launched) = match qd.serve.as_ref().filter(|s| s.done) {
            Some(s) => (gravity_decompose::explain::gather_workers(qd.plannedstmt()), i64::from(s.workers)),
            None => (0, 0),
        };
        let (plannedstmt, source_text, params) = (qd.plannedstmt(), qd.source_text(), qd.params);
        f(&mut es::HookQd {
            plannedstmt,
            source_text,
            params,
            instrument_options: &mut qd.instrument_options,
            totaltime: &mut qd.totaltime,
            total_processed: qd.total_processed,
            parallel_workers_to_launch: i32::try_from(to_launch).unwrap_or(i32::MAX),
            parallel_workers_launched: i32::try_from(launched).unwrap_or(i32::MAX),
            explain_only: qd.eflags & EXEC_FLAG_EXPLAIN_ONLY != 0,
        });
    })
}

fn query_desc_es_processed(h: QueryDescHandle) -> u64 {
    route(h, |qd| qd.serve.as_ref().map_or(0, |s| s.es_processed))
}

fn query_desc_snapshot(h: QueryDescHandle) -> Option<Snapshot> {
    route(h, |qd| qd.snapshot.clone())
}

fn query_desc_result_tupdesc(h: QueryDescHandle) -> Option<Rc<TupleDescData<'static>>> {
    route(h, |qd| qd.serve.as_ref().and_then(|s| s.tupdesc.clone()))
}

fn query_desc_operation(h: QueryDescHandle) -> CmdType {
    route(h, |qd| qd.plannedstmt().commandType)
}

/// `WHERE CURRENT OF`: execCurrentOf resolves the cursor's portal; a portal the entry serves answers here (its identity
/// copy's row, its lock host's row), one with no desc keeps execCurrentOf's own errors (a cursor held from a previous
/// transaction, not positioned on a row).
fn exec_current_of(cursor_name: Option<&str>, cursor_param: i32, table_oid: types_core::Oid, table_name: &str) -> PgResult<Option<types_tuple::ItemPointerData>> {
    let Some(portal) = cursor_name.and_then(|n| portalmem::GetPortalByName(Some(n))) else {
        return (prev().exec_current_of)(cursor_name, cursor_param, table_oid, table_name);
    };
    let h = portal.borrow().queryDesc;
    if h.is_null() {
        // D3580: a named protocol portal the runner serves has no desc; its run answers (runner `portal_current_of`)
        let token = portal.borrow().gravity;
        if token != 0 {
            return crate::runner::portal_current_of(&portal, token, cursor_name.unwrap_or(""), table_oid, table_name);
        }
        return (prev().exec_current_of)(cursor_name, cursor_param, table_oid, table_name);
    }
    if !is_ours(h) {
        return Err(foreign_error(h));
    }
    // D2798: the cursor's own run evaluates the code that asks (the run holds the desc): a served run answers from the row
    // it published before that row's quals and projection (an identity copy), or its lock host's last lock (row marks)
    let name = cursor_name.unwrap_or("");
    let now = running(h, |qd| match (&qd.state, qd.serve.as_ref()) {
        (State::Serving, Some(sv)) if !qd.plannedstmt().rowMarks.is_nil() => current_of::current_rowmark(&portal, qd.plannedstmt(), qd.unpruned.as_deref(), sv.run.locked(table_oid), name, table_oid, table_name),
        (_, Some(sv)) if qd.ident.is_some() => current_of::current_running(&portal, qd.plannedstmt(), sv.published(), name, table_oid, table_name),
        _ if current_of::no_scan_of(qd.plannedstmt(), table_oid) || materialized(&portal, qd.plannedstmt()) => Err(current_of::not_simply_updatable(name, table_name)),
        _ => Err(current_of::unsupported(name)),
    });
    if let Some(r) = now {
        return r;
    }
    // C5b: a served portal with row marks: execCurrentOf's es_rowmarks arm over its lock host's curCtid
    let marked = with_desc(h, |qd| match (&qd.state, qd.serve.as_ref()) {
        (State::Serving, Some(sv)) if !qd.plannedstmt().rowMarks.is_nil() => Some(current_of::current_rowmark(&portal, qd.plannedstmt(), qd.unpruned.as_deref(), sv.run.locked(table_oid), cursor_name.unwrap_or(""), table_oid, table_name)),
        _ => None,
    });
    if let Some(r) = marked {
        return r;
    }
    let ident = with_desc(h, |qd| qd.ident.is_some().then(|| (qd.plannedstmt(), qd.serve.as_ref().and_then(|s| s.last_tid))));
    if let Some((pstmt, last)) = ident {
        return current_of::current(&portal, pstmt, last, cursor_name.unwrap_or(""), table_oid, table_name);
    }
    // execCurrentOf's no-FOR UPDATE arm: a plan with no reachable scan of the table is PG's error, and so is a scroll
    // cursor's plan PG tops with Material (D3719)
    if with_desc(h, |qd| current_of::no_scan_of(qd.plannedstmt(), table_oid) || materialized(&portal, qd.plannedstmt())) {
        return Err(current_of::not_simply_updatable(cursor_name.unwrap_or(""), table_name));
    }
    Err(current_of::unsupported(cursor_name.unwrap_or("")))
}

/// The getters' routing: every query desc is the entry's (`foreign` for any other handle).
pub(crate) fn route<R>(h: QueryDescHandle, ours: impl FnOnce(&mut QueryDesc) -> R) -> R {
    if !is_ours(h) {
        foreign(h);
    }
    with_desc(h, ours)
}

#[cfg(test)]
mod d2926_tests {
    use super::{counted_start, CountedScope, COUNTED_DECLINED, READ_COUNTED};
    use types_nodes::plannodes::PlannedStmt;

    fn flags() -> (usize, bool) {
        (READ_COUNTED.get(), COUNTED_DECLINED.get())
    }

    /// A start under the scope that declines, then ends normally or with an error.
    fn start(p: &PlannedStmt<'_>, fail: bool) -> Result<bool, ()> {
        let scope = CountedScope::enter(p as *const PlannedStmt<'_> as usize);
        assert!(counted_start(p));
        COUNTED_DECLINED.set(true);
        if fail {
            return Err(());
        }
        Ok(scope.declined())
    }

    #[test]
    fn counted_scope_is_cleared_on_every_exit() {
        let (p, q) = (PlannedStmt::default(), PlannedStmt::default());
        assert_eq!(start(&p, false), Ok(true));
        assert_eq!(flags(), (0, false));
        assert!(!counted_start(&p));
        assert_eq!(start(&p, true), Err(()));
        assert_eq!(flags(), (0, false));
        // a cancel or an elog unwinding through the start
        let addr = &q as *const PlannedStmt<'_> as usize;
        assert!(std::panic::catch_unwind(|| {
            let _scope = CountedScope::enter(addr);
            panic!("canceled");
        })
        .is_err());
        assert_eq!(flags(), (0, false));
        // a counted start nested in another's (an SPI fetch of '' under the admission) puts the outer one back
        {
            let _outer = CountedScope::enter(addr);
            assert_eq!(start(&p, false), Ok(true));
            assert!(counted_start(&q) && !counted_start(&p));
            assert_eq!(flags(), (addr, false));
        }
        assert_eq!(flags(), (0, false));
    }

    /// B2-31: `zone_restart`'s scope reads only its own plan as a zone start, makes every admission under it convert
    /// nothing (as a counted start's does), and is cleared on every way out, nested in a counted start too.
    #[test]
    fn zone_scope_is_cleared_on_every_exit() {
        use super::{zone_off, zone_start, ZoneScope, ZONE_OFF};
        let (p, q) = (PlannedStmt::default(), PlannedStmt::default());
        let addr = &p as *const PlannedStmt<'_> as usize;
        assert!(!zone_off() && !zone_start(&p));
        {
            let _scope = ZoneScope::enter(addr);
            assert!(zone_off() && zone_start(&p) && !zone_start(&q));
        }
        assert!(!zone_off() && ZONE_OFF.get() == 0);
        assert!(std::panic::catch_unwind(|| {
            let _scope = ZoneScope::enter(addr);
            panic!("canceled");
        })
        .is_err());
        assert!(!zone_off());
        {
            let _counted = CountedScope::enter(&q as *const PlannedStmt<'_> as usize);
            assert!(zone_off() && !zone_start(&q));
            {
                let _scope = ZoneScope::enter(addr);
                assert!(zone_start(&p));
            }
            assert!(zone_off() && !zone_start(&p));
        }
        assert!(!zone_off());
    }
}

#[cfg(test)]
mod removed_tests {
    use super::{removed, Road};

    /// A refusal says what is not supported, with a HINT, and names no internal road or the deleted row executor; it stays
    /// a `gravity: refused (..)` 0A000 the runner reads back.
    #[test]
    fn refusal_says_what_is_unsupported() {
        for road in [Road::Entry, Road::ExplainAnalyze, Road::Nested, Road::PlanKind] {
            let e = removed(road, "why");
            let msg = e.message();
            assert!(msg.starts_with("gravity: refused (this ") && msg.contains(" is not supported") && msg.ends_with(": why)"), "{msg}");
            assert!(!msg.contains("rowexec") && !msg.contains("R1") && !msg.contains("road"), "{msg}");
            assert_eq!(e.sqlstate(), types_error::ERRCODE_FEATURE_NOT_SUPPORTED);
            assert!(e.hint().is_some_and(|h| !h.is_empty()), "{road:?}");
            assert_eq!(crate::runner::refusal_of(&e), Some(&msg["gravity: refused (".len()..msg.len() - 1]));
        }
    }
}
