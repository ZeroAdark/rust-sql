//! S0b: the extended protocol's UNNAMED portal over a FROM-less SELECT (S0a's childless Result, `nofrom::bare_result`),
//! with no Gravity admission; S1x: over S1's point SELECT (`select::shape`: the planner's IndexScan over a unique btree,
//! its keys constants or, in a generic plan, the portal's parameters). The plan is the plan cache's (custom or generic).
//! A named portal keeps Gravity's road (SQL FETCH / CLOSE, PL refcursors, the AWAY machinery reach it).
//! - Bind (hook C1, runner portal.rs `bind_owned`) is PortalStart + ExecutorStart: the snapshot registered on the
//!   portal's resource owner, ExecutorStart's checks, every entry's ExprState over the portal's parameters (an EXECUTE
//!   privilege error raises here), the portal's result descriptor and its PORTAL_ONE_SELECT state.
//! - Describe (hook C3) is tcop's own: RowDescription from that descriptor and the plan's target list.
//! - Execute (hook C2) is PortalRunSelect: the snapshot pushed, the entries evaluated in order, one DataRow in the portal's
//!   result formats. `max_rows` 1 suspends after the row (PortalSuspended); the next Execute completes with no row, as
//!   ExecResult's rs_done gives. An Execute after completion sends no row.
//! - S1x: Bind is ExecInitIndexScan too (the table and the index opened, their tests, the scan keys, the scan begun under
//!   the portal's snapshot, the keys given to the AM); Execute is IndexNext to `max_rows` (ExecutePlan's count: a count
//!   reached suspends with no further fetch, the next Execute resumes the scan); PortalDrop is ExecEndIndexScan and the
//!   relations' close. A shape, table or index S1 declines, or a parameter with no value of the plan's type, is Gravity's.
//! - PortalDrop (hook C4: PortalDrop, AtAbort_Portals, AtSubAbort_Portals through portalmem's `end_served`) frees the
//!   portal's state, unregisters its snapshot, then ends the portal as a memory owner.
//!
//! Memory: the portal is a memory owner from Bind to PortalDrop (memledger `portal_begin` / `portal_end`, as Gravity's
//! served portal, runner portal.rs `PortalPins`). Its state lives in 'GravityPointPortal', owned by the portal and freed at
//! PortalDrop; the entries' results are made there, as PostgreSQL's per-tuple context holds them to ExecutorEnd. The
//! DataRow and the output / send functions run in S0a's run context under the portal (`run::with_run`), reset when the
//! Execute ends.
use std::cell::{Cell, RefCell};

use gravity_program::{Cmd, OutCol};
use mcx::{Mcx, McxOwned, PgBox, PgVec};
use types_error::{PgError, PgResult};
use types_nodes::list::NodeList;
use types_nodes::plannodes::PlannedStmt;
use types_portal::params::ParamExternData;
use types_portal::{CachedPlanHandle, ParamListHandle, Portal, QueryCompletion};
use types_rel::{NoLock, Relation};
use types_resowner::ResourceOwner;
use types_snapshot::SnapshotData;

use super::nofrom::{bare_result, runs_sql, RUNS_SQL};
use super::out::out_col;
use super::run::with_run;
use super::select::{self, Shape, Verdict};
use super::{pkeep, probe, report, reporting, runtime_refusal, strict_error, POINT_BIT};
use crate::inputs::knobs::seam::{point_path, POINT_PATH_OFF, POINT_PATH_STRICT};

mcx::bind!(BoundTy => Bound<'mcx>);

/// What Bind built: the run, and the out columns.
struct Bound<'mcx> {
    body: Body<'mcx>,
    cols: PgVec<'mcx, OutCol>,
}

enum Body<'mcx> {
    /// S0b: each entry's ExprState over the portal's parameters (copied into the portal's context, where the states read
    /// them).
    NoFrom(PgVec<'mcx, PgBox<'mcx, execexpr::ExprState<'mcx>>>),
    /// S1x: the probe, from ExecInitIndexScan to ExecEndIndexScan.
    Probe(Probe<'mcx>),
    /// S1x: a table or index S1 declines, or a key with no value (`select::relation_refusal`): nothing is served.
    Declined(&'static str),
}

/// The probe's target list, its scan (taken and ended at the drop) and its relations, closed after it (index, then table:
/// ExecEndIndexScan's index_close, then ExecCloseRangeTableRelations).
struct Probe<'mcx> {
    tlist: &'static NodeList<'static>,
    scan: Option<probe::Scan<'mcx>>,
    _index: Relation<'mcx>,
    _heap: Relation<'mcx>,
}

/// What C1 serves.
enum What {
    NoFrom(&'static NodeList<'static>),
    Probe(Shape<'static>),
}

/// What a served portal runs: its own state (freed at the drop), or a kept probe (pkeep.rs, pgrust.point_portal_keep: given
/// back at the drop) and the plan's target list (the portal holds the plan).
enum State {
    Own(McxOwned<BoundTy>),
    Kept(pkeep::Lent, &'static NodeList<'static>),
}

/// One served portal, from its Bind to its PortalDrop.
struct Point {
    token: u64,
    /// Freed first at the drop: the states, then their context.
    state: State,
    /// Registered at Bind on the portal's resource owner (ExecutorStart's es_snapshot), unregistered at the drop.
    snap: std::rc::Rc<SnapshotData<'static>>,
    owner: ResourceOwner,
    /// The portal as a memory owner: the account it began on and its owner id.
    c8: (memledger::Acct, u64),
    /// The run has no further row (ExecResult's rs_done; the probe's AM at its end): a later Execute sends none.
    done: bool,
    /// The last Execute stopped at its count, so the portal is not at its end (PortalSuspended): a later Execute runs the
    /// executor again (ExecResult's CHECK_FOR_INTERRUPTS) before it finds no row. At its end PortalRunSelect runs no plan.
    suspended: bool,
}

leader_tls::leader_tls! {
    thread_local! {
        /// The live point portal: the unnamed portal is dropped before the next one is made, so there is at most one.
        static LIVE: RefCell<Option<Point>> = const { RefCell::new(None) };
        static SEQ: Cell<u64> = const { Cell::new(0) };
    }
}

/// C1: `None` leaves the Bind to Gravity, unchanged; `Some` is the Bind's answer (`Ok(true)`: the portal is served here).
pub(in crate::runner) fn bind_point(portal: &Portal<'static>, cplan: CachedPlanHandle, params: ParamListHandle) -> Option<PgResult<bool>> {
    let mode = point_path();
    if mode == POINT_PATH_OFF || !portal.borrow().name.is_empty() {
        return None;
    }
    let stmts = plancache::CachedPlanStmtList(cplan);
    let [pstmt] = stmts else { return None };
    let what = match bare_result(pstmt) {
        Some(tlist) => What::NoFrom(tlist),
        None => match select::shape(pstmt) {
            Verdict::Not => return None,
            Verdict::Declined(why) => return declined(why),
            Verdict::Take(s) => What::Probe(s),
        },
    };
    // D4547: a FROM-less portal whose entries call a function that may run SQL is Gravity's in every mode (not strict's
    // error: S0a's hook A would not have built its plan either)
    if matches!(what, What::NoFrom(tl) if runs_sql(tl)) {
        return declined(RUNS_SQL);
    }
    let why = runtime_refusal(elog::config::where_to_send_output())
        .or_else(|| types_portal::params::defers_to_evaluation(params).then_some("params fetched at evaluation"))
        .or_else(|| LIVE.with(|l| l.borrow().is_some()).then_some("a point portal is live"))
        .or_else(|| (matches!(what, What::Probe(_)) && xact::IsolationIsSerializable()).then_some("serializable"));
    if let Some(why) = why {
        // `strict` raises where S0a's hook A would have built the plan (a FROM-less SELECT); S1x serves the planner's
        // own plan, so its decline is a word in the log in every mode, as S1's
        if mode == POINT_PATH_STRICT && matches!(what, What::NoFrom(_)) {
            return Some(Err(strict_error(why)));
        }
        return declined(why);
    }
    // pgrust.point_portal_keep: a generic plan's probe is kept across its portals (pkeep.rs); a custom plan (no plan key)
    // builds per portal
    if let What::Probe(s) = &what {
        if pkeep::on() {
            if let Some(key) = crate::glue::plan_key(cplan, 0) {
                return open_kept(portal, pstmt, s, key, params).transpose();
            }
        }
    }
    open(portal, pstmt, what, params).transpose()
}

#[cold]
fn declined(why: &str) -> Option<PgResult<bool>> {
    if reporting() {
        report(&format!("declined portal ({why})")); // alloc: Cold
    }
    None
}

/// PortalStart's and ExecutorStart's work for the portal, in their order: the snapshot (registered on the portal's owner
/// and pushed), the permission checks, every entry or the probe initialized; then the portal's descriptor and state.
/// `Ok(None)`: S1x's relation tests declined it; all of it is undone and Gravity serves the Bind.
fn open(portal: &Portal<'static>, pstmt: &'static PlannedStmt<'static>, what: What, params: ParamListHandle) -> PgResult<Option<bool>> {
    let id = memledger::charge::new_owner_id();
    memledger::charge::portal_begin(id);
    // the owner ends here on an error below (the portal is not served, so no C4 comes for it)
    let mut c8 = OwnerEnd(Some((memledger::current(), id)));
    let ctx = gravity_driver::mem::root::context("GravityPointPortal", true);
    ctx.set_owner_portal(id);
    let owner = portal.borrow().resowner;
    let snap = snapmgr::RegisterSnapshotOnOwner(&snapmgr::GetTransactionSnapshot()?, owner)?;
    let built = snapmgr::PushActiveSnapshot(&snap).and_then(|()| {
        let r = McxOwned::<BoundTy>::try_new(ctx, |mcx| match what {
            What::NoFrom(tlist) => build(mcx, pstmt, tlist, params),
            What::Probe(s) => build_probe(mcx, pstmt, &s, params),
        });
        let popped = snapmgr::PopActiveSnapshot();
        r.and_then(|s| popped.map(|()| s))
    });
    let refused = built.as_ref().ok().and_then(|s| {
        s.with(|b| match b.body {
            Body::Declined(why) => Some(why),
            _ => None,
        })
    });
    if let Some(why) = refused {
        // its relations closed with the state, before the snapshot goes
        drop(built);
        snapmgr::UnregisterSnapshotFromOwner(&snap, owner);
        declined(why);
        return Ok(None);
    }
    let desc = built.and_then(|s| execmain_seams::exec_clean_type_from_tl::call(pstmt).map(|d| (s, d)));
    let (state, desc) = match desc {
        Ok(v) => v,
        Err(e) => {
            snapmgr::UnregisterSnapshotFromOwner(&snap, owner);
            return Err(e);
        }
    };
    let seq = SEQ.with(|s| {
        s.set(s.get() + 1);
        s.get()
    });
    let token = POINT_BIT | (seq << 8);
    {
        // as bind_owned leaves a served SELECT's portal (D2085), with tcop's descriptor (PORTAL_ONE_SELECT's queryDesc
        // tupDesc: the plan's target list)
        let mut p = portal.borrow_mut();
        p.gravity = token;
        p.tupDesc = Some(desc);
        (p.strategy, p.status, p.atStart, p.atEnd, p.portalPos) = (types_portal::PORTAL_ONE_SELECT, types_portal::PORTAL_READY, true, false, 0);
    }
    let c8 = c8.0.take().expect("set above");
    LIVE.with(|l| *l.borrow_mut() = Some(Point { token, state: State::Own(state), snap, owner, c8, done: false, suspended: false }));
    super::super::execute::arm_teardown();
    if reporting() {
        report("bound portal");
    }
    Ok(Some(true))
}

/// `open` for S1x's probe over generic plan `key` under pgrust.point_portal_keep: the portal's memory owner and snapshot
/// as `open` makes them, then the plan's kept probe taken and re-armed or one made (`pkeep::bind`: ExecutorStart's checks
/// and the probe's per-portal part, in `build_probe`'s order), with no context of the portal's; its descriptor is the
/// kept one. `Ok(None)`: S1's tests or the parameters declined it (undone; Gravity serves the Bind).
fn open_kept(portal: &Portal<'static>, pstmt: &'static PlannedStmt<'static>, s: &Shape<'static>, key: gravity_program::PlanKey, params: ParamListHandle) -> PgResult<Option<bool>> {
    let id = memledger::charge::new_owner_id();
    memledger::charge::portal_begin(id);
    // the owner ends here on an error below (the portal is not served, so no C4 comes for it)
    let mut c8 = OwnerEnd(Some((memledger::current(), id)));
    let owner = portal.borrow().resowner;
    let snap = snapmgr::RegisterSnapshotOnOwner(&snapmgr::GetTransactionSnapshot()?, owner)?;
    let root = pkeep::root_of(pstmt);
    let bound = snapmgr::PushActiveSnapshot(&snap).and_then(|()| {
        let r = with_run(|run, _| with_params(params, |ps| pkeep::bind(pstmt, s, key, root, ps, run)));
        let popped = snapmgr::PopActiveSnapshot();
        r.and_then(|b| popped.map(|()| b))
    });
    let lent = match bound {
        Ok(Ok(lent)) => lent,
        Ok(Err(why)) => {
            snapmgr::UnregisterSnapshotFromOwner(&snap, owner);
            declined(why);
            return Ok(None);
        }
        Err(e) => {
            snapmgr::UnregisterSnapshotFromOwner(&snap, owner);
            return Err(e);
        }
    };
    let desc = lent.desc();
    let seq = SEQ.with(|s| {
        s.set(s.get() + 1);
        s.get()
    });
    let token = POINT_BIT | (seq << 8);
    {
        // as `open` leaves it
        let mut p = portal.borrow_mut();
        p.gravity = token;
        p.tupDesc = Some(desc);
        (p.strategy, p.status, p.atStart, p.atEnd, p.portalPos) = (types_portal::PORTAL_ONE_SELECT, types_portal::PORTAL_READY, true, false, 0);
    }
    let c8 = c8.0.take().expect("set above");
    let tlist: &'static NodeList<'static> = &s.scan.scan.plan.targetlist;
    LIVE.with(|l| *l.borrow_mut() = Some(Point { token, state: State::Kept(lent, tlist), snap, owner, c8, done: false, suspended: false }));
    super::super::execute::arm_teardown();
    if reporting() {
        report("bound portal");
    }
    Ok(Some(true))
}

/// ExecutorStart's part in the portal's context: the permission checks, the parameters copied where the states read them,
/// each entry's ExprState (ExecInitResult's projection: an EXECUTE privilege error raises here), the out columns.
fn build<'m>(mcx: Mcx<'m>, pstmt: &'static PlannedStmt<'static>, tlist: &'static NodeList<'static>, params: ParamListHandle) -> PgResult<Bound<'m>> {
    super::super::start_checks(pstmt, Cmd::Select, mcx)?;
    let n = types_portal::params::num_params(params);
    let mut ext: PgVec<'m, ParamExternData> = PgVec::new_in(mcx);
    ext.try_reserve_exact(n).map_err(|_| mcx.oom(n * core::mem::size_of::<ParamExternData>()))?;
    if n > 0 {
        types_portal::params::with(params, |ps| ext.extend_from_slice(&ps[..n.min(ps.len())]));
    }
    let ext: &'m [ParamExternData] = ext.leak();
    let bind = execexpr::ParamBind { extern_params: Some(ext), ..execexpr::ParamBind::NONE };
    let k = tlist.len();
    let mut states: PgVec<'m, PgBox<'m, execexpr::ExprState<'m>>> = PgVec::new_in(mcx);
    let mut cols: PgVec<'m, OutCol> = PgVec::new_in(mcx);
    states.try_reserve_exact(k).map_err(|_| mcx.oom(k * core::mem::size_of::<usize>()))?;
    cols.try_reserve_exact(k).map_err(|_| mcx.oom(k * core::mem::size_of::<OutCol>()))?;
    for t in tlist.iter() {
        let te = t.as_target_entry().expect("checked by bare_result");
        cols.push(out_col(te)?);
        states.push(execexpr::exec_init_expr(mcx, Some(te.expr), bind)?.expect("a target entry has an expression"));
    }
    Ok(Bound { body: Body::NoFrom(states), cols })
}

/// S1x: ExecutorStart's part for the probe in the portal's context: the permission checks, the table opened
/// (ExecGetRangeTableRelation: AcquireExecutorLocks holds its lock) and the index (ExecInitIndexScan's index_open,
/// `select::index_lockmode`), S1's tests of them under the portal's parameters, the keys' EXECUTE gates
/// (`probe::key_gates`, D4541), the out columns, then the scan begun under
/// the portal's snapshot (the active one, `open` pushed it: es_snapshot) with its keys given to the AM (ExecInitIndexScan's index_rescan; a generic plan's parameter keys
/// are ExecReScanIndexScan's at the first fetch, with nothing between to tell them apart).
fn build_probe<'m>(mcx: Mcx<'m>, pstmt: &'static PlannedStmt<'static>, s: &Shape<'static>, params: ParamListHandle) -> PgResult<Bound<'m>> {
    super::super::start_checks(pstmt, Cmd::Select, mcx)?;
    let heap = table::table_open(mcx, s.relid, NoLock)?;
    let index = indexam::index_open(mcx, s.scan.indexid, select::index_lockmode(s))?;
    let mut ops: probe::KeyOps = [(0, 0); types_core::fmgr::INDEX_MAX_KEYS as usize];
    let begun = with_params(params, |ps| match select::relation_refusal(&heap, &index, s, ps, &mut ops)? {
        Some(why) => Ok(Err(why)),
        None => {
            probe::key_gates(&s.scan.indexqual)?;
            probe::Scan::begin(mcx, &heap, &index, &s.scan.indexqual, ps, &ops, snapmgr::GetActiveSnapshot()).map(Ok)
        }
    })?;
    let mut scan = match begun {
        Ok(scan) => scan,
        Err(why) => return Ok(Bound { body: Body::Declined(why), cols: PgVec::new_in(mcx) }),
    };
    let tlist: &'static NodeList<'static> = &s.scan.scan.plan.targetlist;
    let r = (|| -> PgResult<PgVec<'m, OutCol>> {
        let k = tlist.len();
        let mut cols: PgVec<'m, OutCol> = PgVec::new_in(mcx);
        cols.try_reserve_exact(k).map_err(|_| mcx.oom(k * core::mem::size_of::<OutCol>()))?;
        for t in tlist.iter() {
            cols.push(out_col(t.as_target_entry().expect("checked by shape"))?);
        }
        scan.rescan()?;
        Ok(cols)
    })();
    let cols = match r {
        Ok(cols) => cols,
        Err(e) => {
            let _ = scan.end(mcx);
            return Err(e);
        }
    };
    Ok(Bound { body: Body::Probe(Probe { tlist, scan: Some(scan), _index: index, _heap: heap }), cols })
}

/// The portal's parameters as a slice (none: empty).
fn with_params<R>(params: ParamListHandle, f: impl FnOnce(&[ParamExternData]) -> R) -> R {
    if types_portal::params::num_params(params) == 0 {
        f(&[])
    } else {
        types_portal::params::with(params, f)
    }
}

/// C2: the portal's Execute (`Some(None)`: suspended at `max_rows`; `Some(Some(qc))`: complete).
pub(in crate::runner) fn step(portal: &Portal<'static>, token: u64, max_rows: i64) -> PgResult<Option<Option<QueryCompletion>>> {
    let mut lent = Lent(LIVE.with(|l| l.borrow_mut().take_if(|p| p.token == token)));
    let Some(p) = lent.0.as_mut() else { return Err(missing()) };
    if p.done {
        // D4639: a portal suspended at its count runs ExecResult once more (its CHECK_FOR_INTERRUPTS, then rs_done)
        if std::mem::take(&mut p.suspended) {
            cancelpoll::interrupts::check()?;
        }
        return Ok(Some(Some(gravity_wire::completion(Cmd::Select, 0))));
    }
    let _portal = memledger::charge::enter_portal(p.c8.1);
    snapmgr::PushActiveSnapshot(&p.snap)?;
    let r = with_run(|run, wire| match &mut p.state {
        State::Own(state) => project(portal, state, max_rows, run, wire),
        State::Kept(lent, tlist) => project_kept(portal, lent, tlist, max_rows, run, wire),
    });
    let popped = snapmgr::PopActiveSnapshot();
    let r = r.and_then(|v| popped.map(|()| v));
    if r.is_err() {
        // a failed Execute: the drop ends a kept probe's scan and keeps nothing
        if let State::Kept(lent, _) = &mut p.state {
            lent.failed = true;
        }
    }
    let (rows, at_end, done) = r?;
    p.done = done;
    p.suspended = !at_end;
    if reporting() {
        report(&format!("served portal rows={rows}")); // alloc: Cold
    }
    // PortalRunSelect: a count the rows reached leaves the portal not at its end (PortalSuspended)
    Ok(Some(at_end.then(|| gravity_wire::completion(Cmd::Select, rows))))
}

/// The Execute's rows in the portal's formats, each DataRow built in the run context, to `max_rows` (0: all); returns the
/// rows sent, whether the portal is at its end (the Execute completes) and whether a later Execute has no row. S0b,
/// ExecResult's projection: the entries in order (their results in the portal's context), one row. S1x, IndexNext: a fetch
/// per row, the row projected from the scan's slot (`select::project`), ExecutePlan's count checked after each row sent,
/// before the next fetch.
fn project(portal: &Portal<'static>, state: &mut McxOwned<BoundTy>, max_rows: i64, run: Mcx<'_>, wire: &mut gravity_wire::WireNode) -> PgResult<(u64, bool, bool)> {
    let mut formats: PgVec<'_, i16> = PgVec::new_in(run);
    {
        let p = portal.borrow();
        formats.try_reserve_exact(p.formats.len()).map_err(|_| run.oom(p.formats.len() * 2))?;
        formats.extend_from_slice(&p.formats);
    }
    state.with_mut_mcx(|mcx, b| {
        wire.prepare_cols(&b.cols, (!formats.is_empty()).then_some(&formats[..]))?;
        wire.reset();
        let n = b.cols.len();
        let mut values: PgVec<'_, u64> = PgVec::new_in(run);
        let mut isnull: PgVec<'_, u8> = PgVec::new_in(run);
        values.try_reserve_exact(n).map_err(|_| run.oom(n * 8))?;
        isnull.try_reserve_exact(n).map_err(|_| run.oom(n))?;
        match &mut b.body {
            Body::NoFrom(states) => {
                // ExecResult's CHECK_FOR_INTERRUPTS at its first call
                cancelpoll::interrupts::check()?;
                let mut slots = execexpr::EvalSlots { scan: None, inner: None, outer: None };
                for st in states.iter_mut() {
                    st.arm_result_mcx(mcx);
                    let d = execexpr::exec_eval_expr(st, &mut slots)?;
                    values.push(d.value.as_u64());
                    isnull.push(u8::from(d.isnull));
                }
                wire.send_row_in(&values, &isnull, run)?;
                // D4639: below its count ExecutePlan calls ExecResult again, whose CHECK_FOR_INTERRUPTS comes first (an
                // interrupt the entries raised fires there); a count of 1 the row reached stops before that call
                if max_rows != 1 {
                    cancelpoll::interrupts::check()?;
                }
                // a count of 1 the row reached leaves the portal not at its end; ExecResult's rs_done gives no further row
                Ok((1, max_rows != 1, true))
            }
            Body::Probe(pr) => {
                let scan = pr.scan.as_mut().ok_or_else(missing)?;
                let mut rows = 0u64;
                loop {
                    if max_rows > 0 && rows == max_rows as u64 {
                        return Ok((rows, false, false));
                    }
                    if !scan.next(mcx)? {
                        return Ok((rows, true, true));
                    }
                    select::project(pr.tlist, &mut scan.slot, &mut values, &mut isnull);
                    wire.send_row_in(&values, &isnull, run)?;
                    rows += 1;
                }
            }
            Body::Declined(_) => Err(missing()),
        }
    })
}

/// S1x's Execute over a kept probe (pkeep.rs): `project`'s probe arm, the fetches in the probe's exec context.
fn project_kept(portal: &Portal<'static>, lent: &mut pkeep::Lent, tlist: &'static NodeList<'static>, max_rows: i64, run: Mcx<'_>, wire: &mut gravity_wire::WireNode) -> PgResult<(u64, bool, bool)> {
    let mut formats: PgVec<'_, i16> = PgVec::new_in(run);
    {
        let p = portal.borrow();
        formats.try_reserve_exact(p.formats.len()).map_err(|_| run.oom(p.formats.len() * 2))?;
        formats.extend_from_slice(&p.formats);
    }
    lent.with_probe(|pr| {
        wire.prepare_cols(&pr.cols, (!formats.is_empty()).then_some(&formats[..]))?;
        wire.reset();
        let n = pr.cols.len();
        let mut values: PgVec<'_, u64> = PgVec::new_in(run);
        let mut isnull: PgVec<'_, u8> = PgVec::new_in(run);
        values.try_reserve_exact(n).map_err(|_| run.oom(n * 8))?;
        isnull.try_reserve_exact(n).map_err(|_| run.oom(n))?;
        let em = pr.scratch.exec_mcx();
        let scan = pr.scan.as_mut().ok_or_else(missing)?;
        let mut rows = 0u64;
        loop {
            if max_rows > 0 && rows == max_rows as u64 {
                return Ok((rows, false, false));
            }
            if !scan.next(em)? {
                return Ok((rows, true, true));
            }
            select::project(tlist, &mut scan.slot, &mut values, &mut isnull);
            wire.send_row_in(&values, &isnull, run)?;
            rows += 1;
        }
    })
}

/// C4: the portal's drop: its state freed (a kept probe given back, pkeep.rs `put`), its snapshot unregistered from its
/// owner, then its end as a memory owner (its residue, if any, is noted then).
pub(in crate::runner) fn drop_portal(token: u64) {
    let p = LIVE.try_with(|l| l.try_borrow_mut().ok().and_then(|mut g| g.take_if(|p| p.token == token))).ok().flatten();
    if let Some(Point { state, snap, owner, c8, .. }) = p {
        match state {
            State::Own(mut state) => {
                // S1x: ExecEndIndexScan's index_endscan (its error, an AM's own failure, has no caller to reach: C4 runs
                // from PortalDrop's cleanup), then the relations close with the state
                let _ = state.with_mut_mcx(|mcx, b| match &mut b.body {
                    Body::Probe(pr) => pr.scan.take().map_or(Ok(()), |scan| scan.end(mcx)),
                    _ => Ok(()),
                });
                drop(state);
            }
            State::Kept(lent, _) => pkeep::put(lent),
        }
        snapmgr::UnregisterSnapshotFromOwner(&snap, owner);
        drop(snap);
        drop(OwnerEnd(Some(c8)));
    }
}

/// The session's teardown (`point::drop_state_at_teardown`): a portal still live is dropped with the backend.
pub(super) fn drop_at_teardown() {
    let _ = LIVE.try_with(|l| l.try_borrow_mut().map(|mut g| g.take()));
}

/// The portal's state, out of `LIVE` while its Execute runs (a statement the entries run sees no point portal), back at
/// the end on every exit.
struct Lent(Option<Point>);

impl Drop for Lent {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = LIVE.try_with(|l| l.try_borrow_mut().map(|mut g| *g = Some(p)));
        }
    }
}

/// The portal's end as a memory owner (`memledger::charge::portal_end`), once.
struct OwnerEnd(Option<(memledger::Acct, u64)>);

impl Drop for OwnerEnd {
    fn drop(&mut self) {
        if let Some((a, id)) = self.0.take() {
            let _ = memledger::charge::portal_end(a, id);
        }
    }
}

#[cold]
fn missing() -> Box<PgError> {
    Box::new(PgError::error("point path: an Execute of a point portal whose state is gone").with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR)) // alloc: Cold
}
