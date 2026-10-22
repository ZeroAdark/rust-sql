#![allow(non_snake_case)]

use core::cell::RefCell;

use ::elog::ereport;
use ::mcx::{Mcx, MemoryContext, PgVec};
use ::types_dest::CommandDest;
use ::types_error::{
    ErrorLocation, PgResult, DEBUG3, ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE, ERROR,
};
use ::types_nodes::node_tree::Node;
use ::types_nodes::nodes_enums::CmdType;
use ::types_nodes::plannodes::PlannedStmt;
use ::types_nodes::primnodes::TargetEntry;
use ::types_nodes::NodeTag;
use ::types_portal::{
    FetchDirection, ParamListHandle, Portal, PortalData, PortalStrategy, QueryCompletion,
    QueryDescHandle, QueryEnvHandle, StmtListHandle, TuplestoreHandle, CMDTAG_DELETE,
    CMDTAG_INSERT, CMDTAG_MERGE, CMDTAG_SELECT, CMDTAG_UNKNOWN, CMDTAG_UPDATE, CURSOR_OPT_HOLD,
    CURSOR_OPT_NO_SCROLL, CURSOR_OPT_SCROLL, FETCH_ALL, PORTAL_DEFINED, PORTAL_MULTI_QUERY,
    PORTAL_ONE_MOD_WITH, PORTAL_ONE_RETURNING, PORTAL_ONE_SELECT, PORTAL_READY,
    PORTAL_UTIL_SELECT,
};
use ::types_scan::sdir::{
    BackwardScanDirection, ForwardScanDirection, NoMovementScanDirection, ScanDirection,
    ScanDirectionIsForward, ScanDirectionIsNoMovement,
};
use ::types_slot::{TupleSlotKind, EXEC_FLAG_BACKWARD, EXEC_FLAG_REWIND};

use ::cmdtag::InitializeQueryCompletion;
use ::snapmgr::Snapshot;
use ::tcop_dest::DestReceiver;
use ::utility_seams::{PROCESS_UTILITY_QUERY, PROCESS_UTILITY_TOPLEVEL};

#[track_caller]
fn loc(funcname: &'static str) -> ErrorLocation {
    let site = core::panic::Location::caller();
    ErrorLocation::new(site.file(), site.line() as i32, funcname)
}

pub mod stmt_list;
#[cfg(test)]
mod tests;

pub use pquery_seams::TargetEntrySummary;

pub fn init_seams() {
    pquery_seams::fetch_portal_target_list::set(FetchPortalTargetList);
    pquery_seams::fetch_utility_statement_target_list::set(FetchUtilityStatementTargetList);
    pquery_seams::stmt_list_free::set(stmt_list::free);
    pquery_seams::ensure_portal_snapshot_exists::set(EnsurePortalSnapshotExists);
}

thread_local! {
    static ACTIVE_PORTAL: RefCell<Option<Portal<'static>>> = const { RefCell::new(None) };
}

pub fn ActivePortal() -> Option<Portal<'static>> {
    ACTIVE_PORTAL.with(|p| p.borrow().clone())
}

fn swap_active_portal(new: Option<Portal<'static>>) -> Option<Portal<'static>> {
    ACTIVE_PORTAL.with(|p| p.replace(new))
}

#[inline]
fn set_query_completion(qc: &mut QueryCompletion, tag: types_core::CommandTag, nprocessed: u64) {
    qc.commandTag = tag;
    qc.nprocessed = nprocessed;
}

// A utility inside the portal can commit and destroy the saved owner, so a saved
// TopTransactionResourceOwner re-targets the exit-time one.
pub fn run_protected<R>(
    portal: &Portal<'static>,
    may_commit: bool,
    body: impl FnOnce() -> PgResult<R>,
) -> PgResult<R> {
    // C8 checker 1: a portal its PortalStart made a memory owner is the one the backend executes for the body
    let _memory_owner = enter_memory_owner(portal);
    let save = swap_active_portal(Some(portal.clone()));
    let save_owner = resowner_seams::current_resource_owner::call();
    let save_top_owner = resowner_seams::top_transaction_resource_owner::call();
    let portal_owner = portal.borrow().resowner;
    if !portal_owner.is_null() {
        resowner_seams::set_current_resource_owner::call(portal_owner);
    }
    // unwind-ok: c-callback
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
    let restore = |save: Option<Portal<'static>>| {
        if may_commit && save_owner == save_top_owner {
            resowner_seams::set_current_resource_owner::call(
                resowner_seams::top_transaction_resource_owner::call(),
            );
        } else {
            resowner_seams::set_current_resource_owner::call(save_owner);
        }
        swap_active_portal(save);
    };
    match outcome {
        Ok(Ok(r)) => {
            restore(save);
            Ok(r)
        }
        Ok(Err(e)) => {
            let _ = portalmem::MarkPortalFailed(portal);
            restore(save);
            Err(e)
        }
        Err(payload) => {
            let _ = portalmem::MarkPortalFailed(portal);
            restore(save);
            std::panic::resume_unwind(payload);
        }
    }
}

/// C8 checker 1 (memledger owner_end): the portal becomes a memory owner at its PortalStart (PORTAL_ONE_SELECT, whose
/// executor state a suspended portal keeps past the statement that started or fetched it: DECLARE ... CURSOR, the extended
/// protocol's portals) and the backend executes it until the guard drops. What its executor takes from here to its
/// PortalDrop (a part use, a region) is the portal's, not that statement's; portalmem's PortalDrop ends it after the
/// cleanup hook's ExecutorEnd. A runner-bound portal skips PortalStart: it is its own owner (the runner's PortalPins).
fn begin_memory_owner(portal: &Portal<'static>) -> memledger::charge::PortalScope {
    let id = {
        let mut p = portal.borrow_mut();
        match p.memory_owner {
            Some((_, id)) => id,
            None => {
                let id = memledger::charge::new_owner_id();
                memledger::charge::portal_begin(id);
                p.memory_owner = Some((memledger::current().0, id));
                // LEDGER2 step 2: its PortalContext's bytes are the portal's from here (the contexts its executor makes
                // under it take the portal too)
                if let Some(c) = p.portalContext.as_ref() {
                    c.set_owner_portal(id);
                }
                id
            }
        }
    };
    memledger::charge::enter_portal(id)
}

/// C8 checker 1: the backend executes `portal` until the guard drops, when its PortalStart made it a memory owner
/// (`begin_memory_owner`): Run, RunFetch and PersistHoldablePortal's executor calls take for the portal.
fn enter_memory_owner(portal: &Portal<'static>) -> Option<memledger::charge::PortalScope> {
    let owner = portal.borrow().memory_owner;
    owner.map(|(_, id)| memledger::charge::enter_portal(id))
}

pub fn with_portal_owner<R>(portal: &Portal<'static>, body: impl FnOnce() -> R) -> R {
    struct Restore(types_resowner::ResourceOwner);
    impl Drop for Restore {
        fn drop(&mut self) {
            resowner_seams::set_current_resource_owner::call(self.0);
        }
    }
    let owner = portal.borrow().resowner;
    if owner.is_null() {
        return body();
    }
    let _restore = Restore(resowner_seams::current_resource_owner::call());
    resowner_seams::set_current_resource_owner::call(owner);
    body()
}

// The registry entry is owning; both Err returns and loud panics between create
// and free must release it, or the EState's relcache refs survive past
// AtEOXact_RelationCache.
pub struct QueryDescOwner(pub QueryDescHandle);

impl QueryDescOwner {
    pub fn disarm(&mut self) {
        self.0 = QueryDescHandle::NULL;
    }
}

impl Drop for QueryDescOwner {
    fn drop(&mut self) {
        if !self.0.is_null() {
            execmain_seams::release_query_desc::call(self.0);
        }
    }
}

fn with_source_text<R>(portal: &Portal<'static>, f: impl FnOnce(&str) -> R) -> R {
    let p = portal.borrow();
    f(p.sourceText.unwrap_or(""))
}

pub fn CreateQueryDesc<'p, 'a, 's>(
    plannedstmt: &'p PlannedStmt<'a>,
    source_text: &'s str,
    snapshot: Option<Snapshot>,
    crosscheck_snapshot: Option<Snapshot>,
    dest: CommandDest,
    params: ParamListHandle,
    query_env: QueryEnvHandle,
    instrument_options: i32,
) -> PgResult<QueryDescHandle> {
    execmain_seams::create_query_desc::call(
        plannedstmt,
        source_text,
        snapshot,
        crosscheck_snapshot,
        dest,
        params,
        query_env,
        instrument_options,
    )
}

pub fn FreeQueryDesc(query_desc: QueryDescHandle) {
    execmain_seams::free_query_desc::call(query_desc);
}

fn ProcessQuery(
    plan: &PlannedStmt<'_>,
    source_text: &str,
    params: ParamListHandle,
    query_env: QueryEnvHandle,
    dest: &mut DestReceiver<'_>,
    qc: Option<&mut QueryCompletion>,
) -> PgResult<()> {
    let query_desc = CreateQueryDesc(
        plan,
        source_text,
        Some(snapmgr::GetActiveSnapshot()),
        None,
        dest.mydest(),
        params,
        query_env,
        0,
    )?;

    let mut owner = QueryDescOwner(query_desc);
    run_process_query(query_desc, dest, qc)?;
    owner.disarm();

    FreeQueryDesc(query_desc);

    Ok(())
}

fn run_process_query(
    query_desc: QueryDescHandle,
    dest: &mut DestReceiver<'_>,
    qc: Option<&mut QueryCompletion>,
) -> PgResult<()> {
    execmain_seams::executor_start::call(query_desc, 0)?;

    execmain_seams::executor_run::call(query_desc, ForwardScanDirection, 0, dest)?;

    if let Some(qc) = qc {
        let es_processed = execmain_seams::query_desc_es_processed::call(query_desc);
        let tag = match execmain_seams::query_desc_operation::call(query_desc) {
            CmdType::CMD_SELECT => CMDTAG_SELECT,
            CmdType::CMD_INSERT => CMDTAG_INSERT,
            CmdType::CMD_UPDATE => CMDTAG_UPDATE,
            CmdType::CMD_DELETE => CMDTAG_DELETE,
            CmdType::CMD_MERGE => CMDTAG_MERGE,
            _ => CMDTAG_UNKNOWN,
        };
        set_query_completion(qc, tag, es_processed);
    }

    execmain_seams::executor_finish::call(query_desc)?;
    execmain_seams::executor_end::call(query_desc)
}

pub fn ChoosePortalStrategy(stmts: &[PlannedStmt<'_>]) -> PortalStrategy {
    if stmts.len() == 1 {
        let pstmt = &stmts[0];
        if pstmt.canSetTag {
            if pstmt.commandType == CmdType::CMD_SELECT {
                if pstmt.hasModifyingCTE {
                    return PORTAL_ONE_MOD_WITH;
                }
                return PORTAL_ONE_SELECT;
            }
            if pstmt.commandType == CmdType::CMD_UTILITY {
                let u = pstmt.utilityStmt.expect("CMD_UTILITY stmt has utilityStmt");
                if utility_seams::utility_returns_tuples::call(u) {
                    return PORTAL_UTIL_SELECT;
                }
                return PORTAL_MULTI_QUERY;
            }
        }
    }

    let mut n_set_tag = 0i32;
    for pstmt in stmts {
        if pstmt.canSetTag {
            n_set_tag += 1;
            if n_set_tag > 1 {
                return PORTAL_MULTI_QUERY;
            }
            if pstmt.commandType == CmdType::CMD_UTILITY || !pstmt.hasReturning {
                return PORTAL_MULTI_QUERY;
            }
        }
    }
    if n_set_tag == 1 {
        return PORTAL_ONE_RETURNING;
    }

    PORTAL_MULTI_QUERY
}

pub fn PortalGetPrimaryStmt(stmts: &[PlannedStmt<'_>]) -> Option<usize> {
    stmts.iter().position(|s| s.canSetTag)
}

pub fn FetchPortalTargetList<'a, 'mcx>(
    mcx: Mcx<'mcx>,
    portal: &'a PortalData<'a>,
) -> PgResult<PgVec<'mcx, TargetEntrySummary>> {
    let mut out: PgVec<'mcx, TargetEntrySummary> = PgVec::new_in(mcx);
    if portal.strategy == PORTAL_MULTI_QUERY || portal.stmts.is_null() {
        return Ok(out);
    }
    stmt_list::with(portal.stmts, |stmts| -> PgResult<()> {
        let Some(primary) = PortalGetPrimaryStmt(stmts) else {
            return Ok(());
        };
        let pstmt = &stmts[primary];
        if pstmt.commandType == CmdType::CMD_UTILITY {
            out = FetchUtilityStatementTargetList(mcx, pstmt.utilityStmt)?;
            return Ok(());
        }
        if pstmt.commandType == CmdType::CMD_SELECT || pstmt.hasReturning {
            let plan = pstmt
                .planTree
                .and_then(Node::as_plan)
                .expect("PlannedStmt has a planTree");
            out.try_reserve(plan.targetlist.len())
                .map_err(|_| mcx.oom(plan.targetlist.len()))?;
            for node in plan.targetlist.iter() {
                let tle = node
                    .as_variant::<TargetEntry>()
                    .expect("targetlist entry is a TargetEntry");
                out.push(TargetEntrySummary {
                    resjunk: tle.resjunk,
                    resorigtbl: tle.resorigtbl,
                    resorigcol: tle.resorigcol,
                });
            }
        }
        Ok(())
    })?;
    Ok(out)
}

pub fn FetchUtilityStatementTargetList<'a, 'mcx>(
    mcx: Mcx<'mcx>,
    utility_stmt: Option<Node<'a>>,
) -> PgResult<PgVec<'mcx, TargetEntrySummary>> {
    match utility_stmt.map(Node::node_tag) {
        Some(NodeTag::T_FetchStmt) => {
            let fstmt =
                utility_stmt.and_then(Node::as_fetch_stmt).expect("utilityStmt is FetchStmt");
            if fstmt.ismove {
                return Ok(PgVec::new_in(mcx));
            }
            let sub =
                portalmem::GetPortalByName(fstmt.portalname).expect("PortalIsValid(subportal)");
            let p = sub.borrow();
            let out = FetchPortalTargetList(mcx, &p);
            drop(p);
            out
        }
        Some(NodeTag::T_ExecuteStmt) => {
            let name = utility_stmt
                .and_then(Node::as_execute_stmt)
                .expect("utilityStmt is ExecuteStmt")
                .name
                .expect("EXECUTE has a name");
            let psrc = prepare_seams::fetch_prepared_statement_plansource::call(name, true)?
                .expect("throw_error=true never returns None");
            plancache::CachedPlanGetTargetList(mcx, psrc, QueryEnvHandle::NULL)
        }
        _ => Ok(PgVec::new_in(mcx)),
    }
}

pub fn PortalStart(
    portal: &Portal<'static>,
    params: ParamListHandle,
    eflags: i32,
    snapshot: Option<Snapshot>,
) -> PgResult<()> {
    debug_assert_eq!(portal.borrow().status, PORTAL_DEFINED);

    run_protected(portal, false, || -> PgResult<()> {
        portal.borrow_mut().portalParams = params;

        let stmts_handle = portal.borrow().stmts;
        let stmts: &[PlannedStmt<'static>] = if stmts_handle.is_null() {
            &[]
        } else {
            stmt_list::resolve(stmts_handle)
        };
        let strategy = ChoosePortalStrategy(stmts);
        portal.borrow_mut().strategy = strategy;

        match strategy {
            PORTAL_ONE_SELECT => {
                let _memory_owner = begin_memory_owner(portal);
                match &snapshot {
                    Some(snap) => snapmgr::PushActiveSnapshot(snap)?,
                    None => {
                        let snap = snapmgr::GetTransactionSnapshot()?;
                        snapmgr::PushActiveSnapshot(&snap)?;
                    }
                }

                let query_desc = {
                    let p = portal.borrow();
                    let source_text = p.sourceText.unwrap_or("");
                    let query_env = p.queryEnv;
                    if !p.cplan.is_null() && execmain_seams::note_cplan_for_query_desc::is_installed()
                    {
                        execmain_seams::note_cplan_for_query_desc::call(p.cplan);
                    }
                    CreateQueryDesc(
                        &stmts[0],
                        source_text,
                        Some(snapmgr::GetActiveSnapshot()),
                        None,
                        CommandDest::None,
                        params,
                        query_env,
                        0,
                    )?
                };

                // A store-armed SCROLL portal is a plain forward plan — backward
                // is consumed by the store — so the child never gets
                // EXEC_FLAG_BACKWARD.
                let scroll = (portal.borrow().cursorOptions & CURSOR_OPT_SCROLL) != 0;
                let store_armed = scroll
                    && execmain_seams::cursor_store_fill_enabled::is_installed()
                    && execmain_seams::cursor_store_fill_enabled::call()
                    && !(execmain_seams::plan_supports_backward_scan::is_installed()
                        && execmain_seams::plan_supports_backward_scan::call(&stmts[0]));
                let current_of_eligible = store_armed
                    && execmain_seams::cursor_plan_current_of_eligible::is_installed()
                    && execmain_seams::cursor_plan_current_of_eligible::call(&stmts[0], scroll);
                let myeflags = if scroll {
                    eflags | EXEC_FLAG_REWIND | if store_armed { 0 } else { EXEC_FLAG_BACKWARD }
                } else {
                    eflags
                };

                let mut qd_owner = QueryDescOwner(query_desc);
                execmain_seams::executor_start::call(query_desc, myeflags)?;

                let tup_desc = execmain_seams::query_desc_result_tupdesc::call(query_desc);
                let mut p = portal.borrow_mut();
                p.queryDesc = query_desc;
                qd_owner.disarm();
                p.tupDesc = tup_desc;
                p.atStart = true;
                p.atEnd = false;
                p.portalPos = 0;
                p.cursorStoreArmed = store_armed;
                if store_armed {
                    p.currentOfEligible = Some(current_of_eligible);
                }
                drop(p);
                if store_armed {
                    execmain_seams::cursor_store_armed_note::call();
                }

                snapmgr::PopActiveSnapshot()?;
            }
            PORTAL_ONE_RETURNING | PORTAL_ONE_MOD_WITH => {
                let primary = PortalGetPrimaryStmt(stmts)
                    .expect("PORTAL_ONE_RETURNING portal has a primary stmt");
                let tup_desc = execmain_seams::exec_clean_type_from_tl::call(&stmts[primary])?;
                let mut p = portal.borrow_mut();
                p.tupDesc = Some(tup_desc);
                p.atStart = true;
                p.atEnd = false;
                p.portalPos = 0;
            }
            PORTAL_UTIL_SELECT => {
                let primary = PortalGetPrimaryStmt(stmts)
                    .expect("PORTAL_UTIL_SELECT portal has a primary stmt");
                let pstmt = &stmts[primary];
                debug_assert_eq!(pstmt.commandType, CmdType::CMD_UTILITY);
                let u = pstmt.utilityStmt.expect("utility stmt present");
                let tup_desc = utility_seams::utility_tuple_descriptor::call(u)?;
                let mut p = portal.borrow_mut();
                p.tupDesc = tup_desc;
                p.atStart = true;
                p.atEnd = false;
                p.portalPos = 0;
            }
            PORTAL_MULTI_QUERY => {
                portal.borrow_mut().tupDesc = None;
            }
        }
        Ok(())
    })?;

    portal.borrow_mut().status = PORTAL_READY;

    Ok(())
}

/// No C counterpart: the retained QueryDesc rearms in place of CreateQueryDesc +
/// ExecutorStart, falling back to PortalStart on param-shape mismatch.
pub fn PortalStartParked(portal: &Portal<'static>, params: ParamListHandle) -> PgResult<()> {
    debug_assert_eq!(portal.borrow().status, PORTAL_DEFINED);
    debug_assert_eq!(portal.borrow().strategy, PORTAL_ONE_SELECT);
    let rearmed = run_protected(portal, false, || -> PgResult<bool> {
        let _memory_owner = begin_memory_owner(portal);
        portal.borrow_mut().portalParams = params;
        let snap = snapmgr::GetTransactionSnapshot()?;
        snapmgr::PushActiveSnapshot(&snap)?;
        let query_desc = portal.borrow().queryDesc;
        let rearmed = execmain_seams::executor_rearm::call(
            query_desc,
            Some(snapmgr::GetActiveSnapshot()),
            params,
        )?;
        if rearmed {
            let mut p = portal.borrow_mut();
            p.atStart = true;
            p.atEnd = false;
            p.portalPos = 0;
        }
        snapmgr::PopActiveSnapshot()?;
        Ok(rearmed)
    })?;
    if rearmed {
        let mut p = portal.borrow_mut();
        p.cleanup = types_portal::PortalCleanupHook::PortalCleanup;
        p.status = PORTAL_READY;
        return Ok(());
    }
    let query_desc = {
        let mut p = portal.borrow_mut();
        p.tupDesc = None;
        p.cleanup = types_portal::PortalCleanupHook::PortalCleanup;
        core::mem::replace(&mut p.queryDesc, QueryDescHandle::NULL)
    };
    execmain_seams::release_query_desc::call(query_desc);
    PortalStart(portal, params, 0, None)
}

pub fn PortalSetResultFormat(portal: &Portal<'static>, formats: &[i16]) -> PgResult<()> {
    let n_formats = formats.len();

    let natts = match portal.borrow().tupDesc.as_ref() {
        None => return Ok(()),
        Some(td) => td.natts as usize,
    };

    let mut p = portal.borrow_mut();
    p.formats.clear();
    p.formats
        .try_reserve_exact(natts)
        .map_err(|_| mcx::oom_named("TopPortalContext", natts * 2))?;
    if n_formats > 1 {
        if n_formats != natts {
            return Err(ereport(ERROR)
                .errcode(types_error::ERRCODE_PROTOCOL_VIOLATION)
                .errmsg(format!(
                    "bind message has {n_formats} result formats but query has {natts} columns"
                ))
                .into_error()
                .into());
        }
        for &f in &formats[..natts] {
            p.formats.push(f);
        }
    } else if n_formats > 0 {
        for _ in 0..natts {
            p.formats.push(formats[0]);
        }
    } else {
        for _ in 0..natts {
            p.formats.push(0);
        }
    }
    Ok(())
}

pub fn PortalRun<'mcx>(
    portal: &Portal<'static>,
    count: i64,
    is_top_level: bool,
    dest: &mut DestReceiver<'mcx>,
    mut altdest: Option<&mut DestReceiver<'mcx>>,
    mut qc: Option<&mut QueryCompletion>,
) -> PgResult<bool> {
    if let Some(qc) = qc.as_deref_mut() {
        InitializeQueryCompletion(qc);
    }

    let strategy = portal.borrow().strategy;
    let log_stats = guc_tables::backing::log_executor_stats();
    if log_stats && strategy != PORTAL_MULTI_QUERY {
        ereport(DEBUG3).errmsg_internal("PortalRun").finish(loc("PortalRun"))?;
        postgres_seams::reset_usage::call();
    }

    portalmem::MarkPortalActive(portal)?;

    let result = run_protected(portal, true, || -> PgResult<bool> {
        match strategy {
            PORTAL_ONE_SELECT | PORTAL_ONE_RETURNING | PORTAL_ONE_MOD_WITH
            | PORTAL_UTIL_SELECT => {
                if strategy != PORTAL_ONE_SELECT && portal.borrow().holdStore.is_null() {
                    FillPortalStore(portal, is_top_level)?;
                }

                let nprocessed = PortalRunSelect(portal, true, count, dest)?;

                if let Some(qc) = qc.as_deref_mut() {
                    let portal_qc = portal.borrow().qc;
                    if portal_qc.commandTag != CMDTAG_UNKNOWN {
                        *qc = portal_qc;
                        qc.nprocessed = nprocessed;
                    }
                }

                portal.borrow_mut().status = PORTAL_READY;

                Ok(portal.borrow().atEnd)
            }
            PORTAL_MULTI_QUERY => {
                PortalRunMulti(
                    portal,
                    is_top_level,
                    false,
                    dest,
                    altdest.as_deref_mut(),
                    qc.as_deref_mut(),
                )?;

                portalmem::MarkPortalDone(portal)?;

                Ok(true)
            }
        }
    })?;

    if guc_tables::backing::log_executor_stats() && strategy != PORTAL_MULTI_QUERY {
        postgres_seams::show_usage::call("EXECUTOR STATISTICS")?;
    }

    Ok(result)
}

fn PortalRunSelect(
    portal: &Portal<'static>,
    forward: bool,
    mut count: i64,
    dest: &mut DestReceiver<'_>,
) -> PgResult<u64> {
    let query_desc = portal.borrow().queryDesc;
    let hold_store = portal.borrow().holdStore;
    let store_armed = portal.borrow().cursorStoreArmed && !query_desc.is_null();

    let gravity = portal.borrow().gravity != 0;

    debug_assert!(!query_desc.is_null() || !hold_store.is_null() || gravity);

    let nprocessed: u64;
    let direction: ScanDirection;

    if forward {
        if portal.borrow().atEnd || count <= 0 {
            direction = NoMovementScanDirection;
            count = 0;
        } else {
            direction = ForwardScanDirection;
        }

        if count == FETCH_ALL {
            count = 0;
        }

        if store_armed {
            if !ScanDirectionIsNoMovement(direction) {
                let target = if count == 0 {
                    0
                } else {
                    portal.borrow().portalPos.saturating_add(count as u64)
                };
                fill_portal_store_to(portal, target)?;
            }
            nprocessed =
                RunFromStore(portal, direction, count as u64, dest, cursor_read_store(portal))?;
        } else if !hold_store.is_null() {
            nprocessed = RunFromStore(portal, direction, count as u64, dest, hold_store)?;
        } else if gravity {
            let tupdesc = portal.borrow().tupDesc.clone().expect("a served SELECT portal has its result descriptor");
            dest.startup(CmdType::CMD_SELECT as i32, &tupdesc)?;
            nprocessed = if ScanDirectionIsNoMovement(direction) {
                0
            } else {
                gravity_entry_seams::fetch::call(portal, count, dest)?
            };
            dest.shutdown()?;
        } else {
            let snap = execmain_seams::query_desc_snapshot::call(query_desc)
                .expect("queryDesc->snapshot set while executor is active");
            snapmgr::PushActiveSnapshot(&snap)?;
            execmain_seams::executor_run::call(query_desc, direction, count as u64, dest)?;
            nprocessed = execmain_seams::query_desc_es_processed::call(query_desc);
            snapmgr::PopActiveSnapshot()?;
        }

        if !ScanDirectionIsNoMovement(direction) {
            let mut p = portal.borrow_mut();
            if nprocessed > 0 {
                p.atStart = false;
            }
            if count == 0 || nprocessed < count as u64 {
                p.atEnd = true;
            }
            p.portalPos += nprocessed;
        }
    } else {
        if (portal.borrow().cursorOptions & CURSOR_OPT_NO_SCROLL) != 0 {
            return Err(no_scroll_error());
        }

        if portal.borrow().atStart || count <= 0 {
            direction = NoMovementScanDirection;
            count = 0;
        } else {
            direction = BackwardScanDirection;
        }

        if count == FETCH_ALL {
            count = 0;
        }

        if store_armed {
            nprocessed =
                RunFromStore(portal, direction, count as u64, dest, cursor_read_store(portal))?;
        } else if !hold_store.is_null() {
            nprocessed = RunFromStore(portal, direction, count as u64, dest, hold_store)?;
        } else {
            let snap = execmain_seams::query_desc_snapshot::call(query_desc)
                .expect("queryDesc->snapshot set while executor is active");
            snapmgr::PushActiveSnapshot(&snap)?;
            execmain_seams::executor_run::call(query_desc, direction, count as u64, dest)?;
            nprocessed = execmain_seams::query_desc_es_processed::call(query_desc);
            snapmgr::PopActiveSnapshot()?;
        }

        if !ScanDirectionIsNoMovement(direction) {
            let mut p = portal.borrow_mut();
            if nprocessed > 0 && p.atEnd {
                p.atEnd = false;
                p.portalPos += 1;
            }
            if count == 0 || nprocessed < count as u64 {
                p.atStart = true;
                p.portalPos = 0;
            } else {
                p.portalPos -= nprocessed;
            }
        }
    }

    Ok(nprocessed)
}

fn FillPortalStore(portal: &Portal<'static>, is_top_level: bool) -> PgResult<()> {
    let mut qc = QueryCompletion::default();
    InitializeQueryCompletion(&mut qc);

    portalmem::PortalCreateHoldStore(portal)?;
    let mut treceiver = tcop_dest::CreateDestReceiver(CommandDest::Tuplestore);
    tcop_dest::SetTuplestoreDestReceiverParams(
        &mut treceiver,
        portal.borrow().holdStore,
        false,
        portal.borrow().tupDesc.clone(),
        Some("query result type does not match portal result type"),
    );

    let strategy = portal.borrow().strategy;
    match strategy {
        PORTAL_ONE_RETURNING | PORTAL_ONE_MOD_WITH => {
            let mut none = tcop_dest::DestReceiver::DoNothing;
            PortalRunMulti(
                portal,
                is_top_level,
                true,
                &mut treceiver,
                Some(&mut none),
                Some(&mut qc),
            )?;
        }
        PORTAL_UTIL_SELECT => {
            let h = portal.borrow().stmts;
            PortalRunUtility(portal, h, 0, is_top_level, true, &mut treceiver, Some(&mut qc))?;
        }
        other => {
            return Err(ereport(ERROR)
                .errmsg_internal(format!("unrecognized portal strategy: {}", other as u32))
                .into_error()
                .into());
        }
    }

    if qc.commandTag != CMDTAG_UNKNOWN {
        portal.borrow_mut().qc = qc;
    }

    treceiver.destroy();

    Ok(())
}

// A NULL handle is legal only under NoMovement (loop never entered).
fn RunFromStore(
    portal: &Portal<'static>,
    direction: ScanDirection,
    count: u64,
    dest: &mut DestReceiver<'_>,
    hold_store: TuplestoreHandle,
) -> PgResult<u64> {
    let mut current_tuple_count: u64 = 0;

    let tup_desc = portal
        .borrow()
        .tupDesc
        .clone()
        .expect("RunFromStore: portal has a tupDesc");

    // SAFETY: portalContext is PgBox'd for address stability and outlives this
    // call (freed only in PortalDrop); the Ref is released before use.
    let ctx: &MemoryContext = unsafe {
        let p = portal.borrow();
        &*(&**p.portalContext.as_ref().expect("portal has portalContext")
            as *const MemoryContext)
    };
    let mcx = ctx.mcx();

    dest.startup(CmdType::CMD_SELECT as i32, &tup_desc)?;

    let mut slot =
        exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(tup_desc));

    if ScanDirectionIsNoMovement(direction) {
    } else {
        let fwd = ScanDirectionIsForward(direction);
        loop {
            let ok = tuplestore_hold_seams::tuplestore_gettupleslot::call(
                hold_store, fwd, false, &mut slot,
            )?;
            if !ok {
                break;
            }

            if !dest.receive_slot(&mut slot)? {
                break;
            }

            exectuples::exec_clear_tuple(&mut slot, mcx);

            current_tuple_count += 1;
            if count != 0 && count == current_tuple_count {
                break;
            }
        }
    }

    dest.shutdown()?;

    drop(slot);

    Ok(current_tuple_count)
}

fn PortalRunUtility(
    portal: &Portal<'static>,
    stmts: StmtListHandle,
    idx: usize,
    is_top_level: bool,
    set_hold_snapshot: bool,
    dest: &mut DestReceiver<'_>,
    qc: Option<&mut QueryCompletion>,
) -> PgResult<()> {
    let pstmt = &stmt_list::resolve(stmts)[idx];
    let requires_snapshot = PlannedStmtRequiresSnapshot(pstmt);

    if requires_snapshot {
        let mut snapshot = snapmgr::GetTransactionSnapshot()?;

        if set_hold_snapshot {
            let registered = snapmgr::RegisterSnapshot(Some(&snapshot))?
                .expect("RegisterSnapshot of a live snapshot");
            portal.borrow_mut().holdSnapshot = Some(registered.clone());
            snapshot = registered;
        }

        let create_level = portal.borrow().createLevel;
        snapmgr::PushActiveSnapshotWithLevel(&snapshot, create_level)?;
        portal.borrow_mut().portalSnapshot = Some(snapmgr::GetActiveSnapshot());
    } else {
        portal.borrow_mut().portalSnapshot = None;
    }

    let context = if is_top_level {
        PROCESS_UTILITY_TOPLEVEL
    } else {
        PROCESS_UTILITY_QUERY
    };
    let read_only_tree = !portal.borrow().cplan.is_null();

    // SAFETY: portalContext is PgBox'd for address stability and outlives this
    // call (freed only in PortalDrop); the Ref is released before use.
    let ctx: &MemoryContext = unsafe {
        let p = portal.borrow();
        &*(&**p.portalContext.as_ref().expect("portal has portalContext")
            as *const MemoryContext)
    };
    let mcx = ctx.mcx();

    // No portal Ref may be held across ProcessUtility: VACUUM commits its
    // transaction mid-command and PreCommit_Portals re-enters this portal.
    // SAFETY: sourceText is set at portal define time, address-stable in the
    // portal's memory, and never mutated while the portal runs (C contract).
    let source_text: &str = unsafe {
        let p = portal.borrow();
        core::mem::transmute::<&str, &str>(
            p.sourceText.unwrap_or(""),
        )
    };
    let (params, query_env) = {
        let p = portal.borrow();
        (p.portalParams, p.queryEnv)
    };
    utility_seams::process_utility::call(
        mcx,
        pstmt,
        source_text,
        read_only_tree,
        context,
        params,
        query_env,
        dest,
        qc,
    )?;

    let portal_snapshot = portal.borrow_mut().portalSnapshot.take();
    if let Some(snap) = portal_snapshot {
        if snapmgr::ActiveSnapshotSet() {
            debug_assert!(std::rc::Rc::ptr_eq(&snap, &snapmgr::GetActiveSnapshot()));
            snapmgr::PopActiveSnapshot()?;
        }
    }

    Ok(())
}

fn PortalRunMulti<'mcx>(
    portal: &Portal<'static>,
    is_top_level: bool,
    set_hold_snapshot: bool,
    dest: &mut DestReceiver<'mcx>,
    mut altdest: Option<&mut DestReceiver<'mcx>>,
    mut qc: Option<&mut QueryCompletion>,
) -> PgResult<()> {
    let mut active_snapshot_set = false;

    let mut none_dest = DestReceiver::DoNothing;
    let mut none_alt = DestReceiver::DoNothing;
    let demote_dest = dest.mydest() == CommandDest::RemoteExecute;
    let demote_alt = match altdest.as_deref() {
        Some(a) => a.mydest() == CommandDest::RemoteExecute,
        None => demote_dest,
    };

    let stmts = portal.borrow().stmts;
    let nstmts = if stmts.is_null() {
        0
    } else {
        stmt_list::resolve(stmts).len()
    };

    for i in 0..nstmts {
        postgres_seams::check_for_interrupts::call()?;

        // Re-resolved per iteration: a utility in an earlier statement can
        // release the portal's stmts.
        let pstmt = &stmt_list::resolve(stmts)[i];
        let (is_plannable, can_set_tag) = (pstmt.utilityStmt.is_none(), pstmt.canSetTag);

        if is_plannable {
            if guc_tables::backing::log_executor_stats() {
                postgres_seams::reset_usage::call();
            }

            if !active_snapshot_set {
                let mut snapshot = snapmgr::GetTransactionSnapshot()?;

                if set_hold_snapshot {
                    let registered = snapmgr::RegisterSnapshot(Some(&snapshot))?
                        .expect("RegisterSnapshot of a live snapshot");
                    portal.borrow_mut().holdSnapshot = Some(registered.clone());
                    snapshot = registered;
                }

                snapmgr::PushCopiedSnapshot(&snapshot)?;
                active_snapshot_set = true;
            } else {
                snapmgr::UpdateActiveSnapshotCommandId()?;
            }

            let receiver: &mut DestReceiver<'mcx> = if can_set_tag {
                if demote_dest { &mut none_dest } else { &mut *dest }
            } else {
                match altdest.as_deref_mut() {
                    Some(a) if !demote_alt => a,
                    Some(_) => &mut none_alt,
                    None => {
                        if demote_dest { &mut none_dest } else { &mut *dest }
                    }
                }
            };
            let stmt_qc = if can_set_tag { qc.as_deref_mut() } else { None };

            let (params, query_env, cplan) = {
                let p = portal.borrow();
                (p.portalParams, p.queryEnv, p.cplan)
            };
            if !cplan.is_null() && execmain_seams::note_cplan_for_query_desc::is_installed() {
                execmain_seams::note_cplan_for_query_desc::call(cplan);
            }
            with_source_text(portal, |source_text| {
                ProcessQuery(pstmt, source_text, params, query_env, receiver, stmt_qc)
            })?;

            if guc_tables::backing::log_executor_stats() {
                postgres_seams::show_usage::call("EXECUTOR STATISTICS")?;
            }
        } else {
            if can_set_tag {
                debug_assert!(!active_snapshot_set);
                let receiver: &mut DestReceiver<'mcx> =
                    if demote_dest { &mut none_dest } else { &mut *dest };
                PortalRunUtility(
                    portal,
                    stmts,
                    i,
                    is_top_level,
                    false,
                    receiver,
                    qc.as_deref_mut(),
                )?;
            } else {
                let receiver: &mut DestReceiver<'mcx> = match altdest.as_deref_mut() {
                    Some(a) if !demote_alt => a,
                    Some(_) => &mut none_alt,
                    None => {
                        if demote_dest { &mut none_dest } else { &mut *dest }
                    }
                };
                PortalRunUtility(portal, stmts, i, is_top_level, false, receiver, None)?;
            }
        }


        if portal.borrow().stmts.is_null() {
            break;
        }

        if i + 1 < nstmts {
            xact::CommandCounterIncrement()?;
        }
    }

    if active_snapshot_set {
        snapmgr::PopActiveSnapshot()?;
    }

    if let Some(qc) = qc {
        let portal_qc = portal.borrow().qc;
        if qc.commandTag == CMDTAG_UNKNOWN && portal_qc.commandTag != CMDTAG_UNKNOWN {
            *qc = portal_qc;
        }
    }

    Ok(())
}

#[cold]
#[inline(never)]
fn no_scroll_error() -> Box<types_error::PgError> {
    Box::new(
        ereport(ERROR)
            .errcode(ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE)
            .errmsg("cursor can only scan forward")
            .errhint("Declare it with SCROLL option to enable backward scan.")
            .into_error(),
    )
}

pub fn PortalRunFetch(
    portal: &Portal<'static>,
    fdirection: FetchDirection,
    count: i64,
    dest: &mut DestReceiver<'_>,
) -> PgResult<u64> {
    portalmem::MarkPortalActive(portal)?;

    let _p8 = execmain_seams::p8ctx::cursor_scope();
    let result = run_protected(portal, false, || -> PgResult<u64> {
        let strategy = portal.borrow().strategy;
        match strategy {
            PORTAL_ONE_SELECT => DoPortalRunFetch(portal, fdirection, count, dest),
            PORTAL_ONE_RETURNING | PORTAL_ONE_MOD_WITH | PORTAL_UTIL_SELECT => {
                if portal.borrow().holdStore.is_null() {
                    FillPortalStore(portal, false)?;
                }
                DoPortalRunFetch(portal, fdirection, count, dest)
            }
            _ => Err(ereport(ERROR)
                .errmsg_internal("unsupported portal strategy")
                .into_error()
                .into()),
        }
    })?;

    portal.borrow_mut().status = PORTAL_READY;

    Ok(result)
}

fn DoPortalRunFetch(
    portal: &Portal<'static>,
    mut fdirection: FetchDirection,
    mut count: i64,
    dest: &mut DestReceiver<'_>,
) -> PgResult<u64> {
    match fdirection {
        FetchDirection::FETCH_FORWARD => {
            if count < 0 {
                fdirection = FetchDirection::FETCH_BACKWARD;
                count = -count;
            }
        }
        FetchDirection::FETCH_BACKWARD => {
            if count < 0 {
                fdirection = FetchDirection::FETCH_FORWARD;
                count = -count;
            }
        }
        FetchDirection::FETCH_ABSOLUTE => {
            let mut none = DestReceiver::DoNothing;
            if count > 0 {
                // portalPos >= i64::MAX excluded so counts never look like FETCH_ALL.
                let portal_pos = portal.borrow().portalPos;
                if (count - 1) as u64 <= portal_pos / 2 || portal_pos >= i64::MAX as u64 {
                    DoPortalRewind(portal)?;
                    if count > 1 {
                        PortalRunSelect(portal, true, count - 1, &mut none)?;
                    }
                } else {
                    let mut pos = portal_pos as i64;
                    if portal.borrow().atEnd {
                        pos += 1;
                    }
                    if count <= pos {
                        PortalRunSelect(portal, false, pos - count + 1, &mut none)?;
                    } else if count > pos + 1 {
                        PortalRunSelect(portal, true, count - pos - 1, &mut none)?;
                    }
                }
                return PortalRunSelect(portal, true, 1, dest);
            } else if count < 0 {
                PortalRunSelect(portal, true, FETCH_ALL, &mut none)?;
                if count < -1 {
                    PortalRunSelect(portal, false, -count - 1, &mut none)?;
                }
                return PortalRunSelect(portal, false, 1, dest);
            } else {
                DoPortalRewind(portal)?;
                return PortalRunSelect(portal, true, 0, dest);
            }
        }
        FetchDirection::FETCH_RELATIVE => {
            let mut none = DestReceiver::DoNothing;
            if count > 0 {
                if count > 1 {
                    PortalRunSelect(portal, true, count - 1, &mut none)?;
                }
                return PortalRunSelect(portal, true, 1, dest);
            } else if count < 0 {
                if count < -1 {
                    PortalRunSelect(portal, false, -count - 1, &mut none)?;
                }
                return PortalRunSelect(portal, false, 1, dest);
            } else {
                fdirection = FetchDirection::FETCH_FORWARD;
            }
        }
    }

    let mut forward = fdirection == FetchDirection::FETCH_FORWARD;

    // Zero count re-fetches the current row, if any (per SQL).
    if count == 0 {
        let on_row = {
            let p = portal.borrow();
            !p.atStart && !p.atEnd
        };
        if dest.mydest() == CommandDest::None {
            // MOVE 0 reports whether FETCH 0 would return a row.
            return Ok(u64::from(on_row));
        }
        if on_row {
            let mut none = DestReceiver::DoNothing;
            PortalRunSelect(portal, false, 1, &mut none)?;
            count = 1;
            forward = true;
        }
    }

    if !forward && count == FETCH_ALL && dest.mydest() == CommandDest::None {
        let mut result = portal.borrow().portalPos;
        if result > 0 && !portal.borrow().atEnd {
            result -= 1;
        }
        DoPortalRewind(portal)?;
        return Ok(result);
    }

    PortalRunSelect(portal, forward, count, dest)
}

fn DoPortalRewind(portal: &Portal<'static>) -> PgResult<()> {
    {
        let p = portal.borrow();
        if p.atStart && !p.atEnd {
            return Ok(());
        }
        if (p.cursorOptions & CURSOR_OPT_NO_SCROLL) != 0 {
            return Err(no_scroll_error());
        }
    }

    let query_desc = portal.borrow().queryDesc;
    let mut rewind_executor = !query_desc.is_null();
    if portal.borrow().cursorStoreArmed && !query_desc.is_null() {
        let store = cursor_read_store(portal);
        let sidecar = portal.borrow().cursorTidStore;
        if rewind_replays(portal) {
            for s in [store, sidecar] {
                if !s.is_null() {
                    tuplestore_hold_seams::tuplestore_rescan::call(s)?;
                }
            }
            rewind_executor = false;
        } else {
            for s in [store, sidecar] {
                if !s.is_null() {
                    tuplestore_hold_seams::tuplestore_clear::call(s);
                }
            }
            portal.borrow_mut().cursorFillExhausted = false;
        }
    } else {
        let hold_store = portal.borrow().holdStore;
        if !hold_store.is_null() {
            tuplestore_hold_seams::tuplestore_rescan::call(hold_store)?;
        }
    }

    if rewind_executor {
        let snap = execmain_seams::query_desc_snapshot::call(query_desc)
            .expect("queryDesc->snapshot set while executor is active");
        snapmgr::PushActiveSnapshot(&snap)?;
        execmain_seams::executor_rewind::call(query_desc)?;
        snapmgr::PopActiveSnapshot()?;
    }

    let mut p = portal.borrow_mut();
    p.atStart = true;
    p.atEnd = false;
    p.portalPos = 0;
    Ok(())
}

fn rewind_replays(portal: &Portal<'static>) -> bool {
    let p = portal.borrow();
    if (p.cursorOptions & CURSOR_OPT_SCROLL) == 0
        || p.stmts.is_null()
        || !execmain_seams::cursor_plan_rewind_replays::is_installed()
    {
        return false;
    }
    let stmts: &[PlannedStmt<'static>] = stmt_list::resolve(p.stmts);
    stmts.first().is_some_and(execmain_seams::cursor_plan_rewind_replays::call)
}


fn cursor_read_store(portal: &Portal<'static>) -> TuplestoreHandle {
    let p = portal.borrow();
    if !p.cursorStore.is_null() {
        p.cursorStore
    } else {
        p.holdStore
    }
}

fn ensure_cursor_store(portal: &Portal<'static>) -> PgResult<()> {
    debug_assert!(portal.borrow().cursorStoreArmed);
    let hold = (portal.borrow().cursorOptions & CURSOR_OPT_HOLD) != 0;
    let store_missing = {
        let p = portal.borrow();
        p.cursorStore.is_null() && p.holdStore.is_null()
    };
    if store_missing {
        if hold {
            portalmem::PortalCreateHoldStore(portal)?;
        } else {
            let store = tuplestore_hold_seams::tuplestore_begin_heap_cursor::call(true, false)?;
            portal.borrow_mut().cursorStore = store;
        }
    }
    // A never-run WITH HOLD cursor reaches its first fill at COMMIT with the
    // holdStore already minted by HoldPortal — the sidecar must still ride along.
    if portal.borrow().currentOfEligible == Some(true)
        && portal.borrow().cursorTidStore.is_null()
    {
        let sidecar = tuplestore_hold_seams::tuplestore_begin_heap_cursor::call(true, hold)?;
        portal.borrow_mut().cursorTidStore = sidecar;
    }
    Ok(())
}

/// The tuplestore keeps the ACTIVE eof reader at EOF across appends. RunFromStore
/// can overshoot only when the store is short of the fetch — which fill_to only
/// permits at fill exhaustion, after which no append ever happens.
pub fn fill_portal_store_to(portal: &Portal<'static>, target_rows: u64) -> PgResult<()> {
    ensure_cursor_store(portal)?;
    if portal.borrow().cursorFillExhausted {
        return Ok(());
    }
    let store = cursor_read_store(portal);
    let have = tuplestore_hold_seams::tuplestore_tuple_count::call(store) as u64;
    if target_rows != 0 && have >= target_rows {
        return Ok(());
    }
    let query_desc = portal.borrow().queryDesc;
    debug_assert!(!query_desc.is_null(), "fill_to on a portal without an executor");
    let deficit = if target_rows == 0 { 0 } else { target_rows - have };
    let hold = (portal.borrow().cursorOptions & CURSOR_OPT_HOLD) != 0;
    let eligible = portal.borrow().currentOfEligible == Some(true);
    let tid_store = portal.borrow().cursorTidStore;

    let mut treceiver = tcop_dest::CreateDestReceiver(CommandDest::Tuplestore);
    tcop_dest::SetTuplestoreDestReceiverParams(&mut treceiver, store, hold, None, None);

    let snap = execmain_seams::query_desc_snapshot::call(query_desc)
        .expect("queryDesc->snapshot set while executor is active");
    snapmgr::PushActiveSnapshot(&snap)?;
    if eligible {
        // Deficit 0 (FETCH ALL) is a u64::MAX-count run: a fill-to-EOF must still
        // capture because MOVE BACKWARD after FETCH ALL resolves CURRENT OF from the sidecar.
        debug_assert!(!tid_store.is_null());
        tcop_dest::SetTuplestoreCaptureSidecar(&mut treceiver, tid_store);
        let effective = if deficit == 0 { u64::MAX } else { deficit };
        execmain_seams::executor_run::call(
            query_desc,
            ForwardScanDirection,
            effective,
            &mut treceiver,
        )?;
        let nprocessed = execmain_seams::query_desc_es_processed::call(query_desc);
        if deficit == 0 || nprocessed < deficit {
            portal.borrow_mut().cursorFillExhausted = true;
        }
    } else {
        execmain_seams::executor_run::call(
            query_desc,
            ForwardScanDirection,
            deficit,
            &mut treceiver,
        )?;
        let nprocessed = execmain_seams::query_desc_es_processed::call(query_desc);
        if deficit == 0 || nprocessed < deficit {
            portal.borrow_mut().cursorFillExhausted = true;
        }
    }
    treceiver.destroy();
    snapmgr::PopActiveSnapshot()?;
    Ok(())
}

/// A pinned plpgsql cursor auto-held at intra-procedure COMMIT was armed WITHOUT
/// CURSOR_OPT_HOLD, so its store is the transaction-scoped `cursorStore`. Persist
/// copies the filled store into the fresh holdStore through the detoasting receiver
/// (source rows are read pre-COMMIT while toast data is alive), then drops the
/// transaction-scoped store and sidecar.
pub fn cursor_store_persist_into_hold(portal: &Portal<'static>) -> PgResult<()> {
    let src = portal.borrow().cursorStore;
    if src.is_null() {
        return Ok(());
    }
    let dst = portal.borrow().holdStore;
    debug_assert!(!dst.is_null(), "HoldPortal creates the holdStore before persist");
    let tup_desc = portal
        .borrow()
        .tupDesc
        .clone()
        .expect("cursor portal has a tupDesc");
    // SAFETY: portalContext is PgBox'd for address stability and outlives
    // this call (freed only in PortalDrop) — the RunFromStore pattern.
    let ctx: &MemoryContext = unsafe {
        let p = portal.borrow();
        &*(&**p.portalContext.as_ref().expect("portal has portalContext")
            as *const MemoryContext)
    };
    let mcx = ctx.mcx();
    let mut treceiver = tcop_dest::CreateDestReceiver(CommandDest::Tuplestore);
    tcop_dest::SetTuplestoreDestReceiverParams(&mut treceiver, dst, true, None, None);
    treceiver.startup(CmdType::CMD_SELECT as i32, &tup_desc)?;
    let mut slot =
        exectuples::make_tuple_table_slot(mcx, TupleSlotKind::MinimalTuple, Some(tup_desc));
    tuplestore_hold_seams::tuplestore_rescan::call(src)?;
    while tuplestore_hold_seams::tuplestore_gettupleslot::call(src, true, false, &mut slot)? {
        // C copies through ExecutorRun, whose scan nodes CHECK_FOR_INTERRUPTS
        // per tuple (execScan.h).
        postgres_seams::check_for_interrupts::call()?;
        treceiver.receive_slot(&mut slot)?;
        exectuples::exec_clear_tuple(&mut slot, mcx);
    }
    treceiver.shutdown()?;
    treceiver.destroy();
    drop(slot);
    let (src, sidecar) = {
        let mut p = portal.borrow_mut();
        (
            core::mem::replace(&mut p.cursorStore, TuplestoreHandle::NULL),
            core::mem::replace(&mut p.cursorTidStore, TuplestoreHandle::NULL),
        )
    };
    tuplestore_hold_seams::tuplestore_end::call(src);
    if !sidecar.is_null() {
        tuplestore_hold_seams::tuplestore_end::call(sidecar);
    }
    Ok(())
}
pub fn PlannedStmtRequiresSnapshot(pstmt: &PlannedStmt<'_>) -> bool {
    let Some(utility_stmt) = pstmt.utilityStmt else {
        return true;
    };

    !matches!(
        utility_stmt.node_tag(),
        NodeTag::T_TransactionStmt
            | NodeTag::T_LockStmt
            | NodeTag::T_VariableSetStmt
            | NodeTag::T_VariableShowStmt
            | NodeTag::T_ConstraintsSetStmt
            | NodeTag::T_FetchStmt
            | NodeTag::T_ListenStmt
            | NodeTag::T_NotifyStmt
            | NodeTag::T_UnlistenStmt
            | NodeTag::T_CheckPointStmt
    )
}

pub fn EnsurePortalSnapshotExists() -> PgResult<()> {
    if snapmgr::ActiveSnapshotSet() {
        return Ok(());
    }

    let Some(portal) = ActivePortal() else {
        return Err(ereport(ERROR)
            .errmsg_internal("cannot execute SQL without an outer snapshot or portal")
            .into_error()
            .into());
    };
    debug_assert!(portal.borrow().portalSnapshot.is_none());

    let snapshot = snapmgr::GetTransactionSnapshot()?;
    let create_level = portal.borrow().createLevel;
    snapmgr::PushActiveSnapshotWithLevel(&snapshot, create_level)?;
    portal.borrow_mut().portalSnapshot = Some(snapmgr::GetActiveSnapshot());
    Ok(())
}
