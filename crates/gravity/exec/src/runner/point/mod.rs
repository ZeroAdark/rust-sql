//! pgrust.point_path: statements served with neither the planner nor a Gravity program. One file per stage, and what
//! they share here: the session's option, the run state kept across statements, the LOG lines, the run-time refusals
//! and the two hooks' dispatch.
//! - nofrom.rs (S0: a SELECT without FROM). Hook A (`plan_skip`, tcop's exec_simple_query) builds the plan the planner
//!   builds for it; hook B (`serve`, `simple_at`) runs a childless Result in ExecutorStart / ExecutorRun's order. A target
//!   list that calls a function that may run SQL is Gravity's (`nofrom::runs_sql`, D4547: its statements nest in a run).
//! - select.rs (S1: a point SELECT after planning). Hook B runs the planner's IndexScan over a unique btree, equality on
//!   every key column against a constant, with no Gravity program.
//! - run.rs: the run every stage takes (the run state on the statement's owner, ExecutorStart's checks, the snapshot).
//! - rel.rs: the table tests; probe.rs: the unique-key probe (the scan keys, the index AM's scan).
//! - out.rs: the wire's out columns of a target list.
//! - portal.rs (S0b: the extended protocol's unnamed portal over a FROM-less SELECT; S1x: over S1's point SELECT). Hooks
//!   C1-C4 in the runner's Bind, Execute, Describe and PortalDrop; a point portal's token has bit 63 set (`POINT_BIT`),
//!   which no Gravity token sets.
//! - delete.rs (S3a: a point DELETE after planning, under pgrust.point_write). Hook B runs the planner's ModifyTable over
//!   S1's IndexScan through the program's own hosts (hosts.rs: the index walk and the write host, no program).
//! - update.rs (S3b: a point UPDATE after planning, under the same switch). Hook B runs the planner's ModifyTable over
//!   S1's IndexScan through the same hosts, the new row ExecGetUpdateNewTuple's (the SET values computed by execexpr).
//! - keep.rs (S3 option (a)): a point write whose plan S5 served from its memo keeps its hosts, recipe and compiled SET
//!   values in that memo entry across statements (`skip::Hit`), under pgrust.point_keep (default off).
//! - pkeep.rs (pgrust.point_portal_keep): an S1x portal's bound probe kept across the portals of one generic plan.
//! `strict` (tests) turns a decline after hook A built the plan into an error.
use std::cell::{Cell, RefCell};

use mcx::MemoryContext;
use tcop_dest::DestReceiver;
use types_dest::CommandDest;
use types_error::{PgError, PgResult};
use types_nodes::plannodes::PlannedStmt;
use types_portal::QueryCompletion;

use crate::inputs::knobs::seam::{point_path, point_write, POINT_PATH_OFF};

mod delete;
mod hosts;
mod keep;
mod nofrom;
mod out;
mod pkeep;
pub(super) mod portal;
mod probe;
mod rel;
mod run;
mod select;
mod skip;
mod update;

/// Hook A: S0's SELECT without FROM (`nofrom`), else S5's point statement (`skip`, pgrust.point_noplan); `Err` hands
/// the queries to the planner.
pub fn plan_skip<'mcx>(mcx: mcx::Mcx<'mcx>, querytrees: mcx::PgVec<'mcx, types_nodes::parsenodes::Query<'mcx>>, dest: CommandDest) -> PgResult<Result<mcx::PgVec<'mcx, PlannedStmt<'mcx>>, mcx::PgVec<'mcx, types_nodes::parsenodes::Query<'mcx>>>> {
    skip::clear_hit();
    match nofrom::plan_skip(mcx, querytrees, dest)? {
        Ok(plans) => Ok(Ok(plans)),
        Err(querytrees) => skip::plan_skip(mcx, querytrees),
    }
}

/// A point portal's token (`portal.gravity`): bit 63, never set by a Gravity token (`ExecSlot::token`: an exec slot's
/// index + 1 in bits 0-7, its generation in bits 8-39).
pub(super) const POINT_BIT: u64 = 1 << 63;

pub(super) fn is_point(token: u64) -> bool {
    token & POINT_BIT != 0
}

leader_tls::leader_tls! {
    thread_local! {
        /// Hook A built the plan hook B is about to see (set per statement by `plan_skip`).
        static BUILT: Cell<bool> = const { Cell::new(false) };
        /// A run is on this thread: a FROM-less SELECT inside one (SPI) takes today's road.
        static RUNNING: Cell<bool> = const { Cell::new(false) };
        /// 0 unread, 1 off, 2 on: PGRUST_GRAVITY_REPORT / PGRUST_GRAVITY_POINT_PLANCHECK, read once.
        static REPORT: Cell<u8> = const { Cell::new(0) };
        static PLANCHECK: Cell<u8> = const { Cell::new(0) };
        /// The run context and its wire node, kept across statements: taken by a run, put back at its end.
        static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    }
}

/// 'GravityPointRun': a wholesale context (an ExprState's result cell lives until its context resets, D842), a session
/// root leaked at the backend's first run and retired in place at its teardown (`mcx::session_root_mut`), so its address
/// never moves; and the wire node.
struct State {
    ctx: &'static mut MemoryContext,
    wire: gravity_wire::WireNode,
}

fn state_new() -> State {
    let ctx = mcx::session_root_mut(gravity_driver::mem::root::context_wholesale("GravityPointRun", true));
    State { ctx, wire: gravity_wire::WireNode::new() }
}

fn env_once(c: &Cell<u8>, name: &str) -> bool {
    if c.get() == 0 {
        c.set(if crate::inputs::knobs::env_on(name) { 2 } else { 1 });
    }
    c.get() == 2
}

fn reporting() -> bool {
    REPORT.with(|c| env_once(c, "PGRUST_GRAVITY_REPORT"))
}

/// Hook B keeps a point statement's pieces in S5's memo entry (keep.rs) under pgrust.point_keep (default off).
fn keeping() -> bool {
    crate::inputs::knobs::seam::point_keep()
}

#[cold]
fn report(msg: &str) {
    let line = elog::with_debug_query_string(|q| format!("gravity: point {msg} :: {}", q.unwrap_or("").replace('\n', " "))); // alloc: Cold
    let _ = elog::ereport(types_error::LOG).errmsg(line).errhidestmt(true).errhidecontext(true).finish(types_error::ErrorLocation::new("gravity", 0, "point"));
}

#[cold]
fn strict_error(why: &str) -> Box<PgError> {
    Box::new(PgError::error(format!("point path: an eligible statement fell back ({why})")).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR)) // alloc: Cold
}

/// What keeps hook B from serving the statement now: the same answer at hook A, so a plan A built is one B takes.
fn runtime_refusal(dest: CommandDest) -> Option<&'static str> {
    if dest != CommandDest::Remote {
        return Some("dest");
    }
    if RUNNING.with(Cell::get) {
        return Some("nested");
    }
    if super::execute::executor_hooks_active() {
        return Some("executor hooks");
    }
    if crate::inputs::knobs::seam::log_executor_stats() {
        return Some("log_executor_stats");
    }
    None
}

/// Hook B: `None` leaves the statement to Gravity, unchanged.
pub(super) fn serve(pstmt: &PlannedStmt<'_>, dest: &mut DestReceiver<'_>) -> Option<PgResult<Option<QueryCompletion>>> {
    let built = BUILT.with(|b| b.replace(false));
    let hit = skip::take_hit(pstmt).filter(|_| keeping());
    let mode = point_path();
    if mode == POINT_PATH_OFF {
        return None;
    }
    if !built && pstmt.planTree.is_some_and(|n| n.as_index_scan().is_some()) {
        return select::serve(pstmt, dest, hit);
    }
    if !built && pstmt.commandType == types_nodes::nodes_enums::CmdType::CMD_DELETE && point_write() {
        return delete::serve(pstmt, dest, hit);
    }
    if !built && pstmt.commandType == types_nodes::nodes_enums::CmdType::CMD_UPDATE && point_write() {
        return update::serve(pstmt, dest, hit);
    }
    nofrom::serve(pstmt, dest, built, mode)
}

/// RUNNING for the run's extent, on every exit.
struct Running;

impl Running {
    fn enter() -> Running {
        RUNNING.with(|r| r.set(true));
        Running
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.with(|r| r.set(false));
    }
}

/// The idle release's reach into the point path (runner/portal.rs `idle_shed`; a cache trims at idle, D4550): S5's memo
/// goes whole, its root context included, and the session's next filing makes it again. REPORT: one 'noplan idle drop
/// <n> slots (<b> bytes)' line when there was a memo.
pub(super) fn idle_release() {
    if let Some((n, b)) = skip::drop_at_idle() {
        if reporting() {
            report(&format!("noplan idle drop {n} slots ({b} bytes)")); // alloc: Cold
        }
    }
    // pgrust.point_portal_keep's kept probes (pkeep.rs) go too; the next portal of each plan makes its probe again
    if let Some((n, b)) = pkeep::drop_all() {
        if reporting() {
            report(&format!("portal keep idle drop {n} slots ({b} bytes)")); // alloc: Cold
        }
    }
}

/// The run state's drop at the session's teardown (`execute::arm_teardown`'s one registration).
pub(super) fn drop_state_at_teardown() {
    portal::drop_at_teardown();
    pkeep::drop_at_teardown();
    skip::drop_at_teardown();
    let _ = STATE.try_with(|s| s.try_borrow_mut().map(|mut g| g.take()));
}
