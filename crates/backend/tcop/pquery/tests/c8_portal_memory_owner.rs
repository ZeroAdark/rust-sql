//! C8 checker 1 (memledger owner_end): a portal its PortalStart starts (PORTAL_ONE_SELECT: DECLARE ... CURSOR, the
//! extended protocol's portals the executor entry serves) keeps its executor state past the statement that started or
//! fetched it. What that executor takes (a part use) must be the portal's, released with it at its PortalDrop, not the
//! statement's: a statement's end that finds a use still held is a residue (on trunk 6cc853abf7e every abandoned cursor
//! and portal read of the stress run left one, 1388 events). The executor here is a fake whose ExecutorStart and
//! ExecutorRun each take a use the way a scan's part open does (`memledger::charge::context`), dropped at PortalCleanup.

use std::cell::RefCell;
use std::sync::Once;

use mcx::MemoryContext;
use memledger::charge::{context, live_portals, OwnerClass};
use memledger::owner_end::{for_each_recent, Source};
use memledger::{Acct, Charge};
use types_dest::CommandDest;
use types_nodes::node_tree::Node;
use types_nodes::nodes_enums::CmdType;
use types_nodes::plannodes::{Plan, PlannedStmt, Result as ResultPlan};
use types_portal::{CachedPlanHandle, ParamListHandle, QueryDescHandle, CMDTAG_SELECT, PORTAL_ONE_SELECT};
use types_snapshot::{SnapshotData, SnapshotType};

/// This test's backend slot (memledger's own tests use 3907..3910).
const PROCNO: u32 = 3931;

thread_local! {
    /// The fake executor's part uses: taken by ExecutorStart and ExecutorRun, dropped by PortalCleanup's ExecutorEnd.
    static USES: RefCell<Vec<Charge>> = const { RefCell::new(Vec::new()) };
    static QD_SNAPSHOT: RefCell<Option<execmain_seams::Snapshot>> = const { RefCell::new(None) };
}

/// A part open of the fake executor: its use is its taker's (the portal, statement or session the backend works for).
fn take_use() {
    let (owner, release) = context();
    USES.with(|u| u.borrow_mut().push(Charge::use_of(owner, release)));
}

fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        guc_tables::init_seams();
        postgres_seams::check_for_interrupts::set(|| Ok(()));
        resowner_seams::current_resource_owner::set(|| types_resowner::ResourceOwner::NULL);
        resowner_seams::set_current_resource_owner::set(|_| {});
        resowner_seams::top_transaction_resource_owner::set(|| types_resowner::ResourceOwner::NULL);
        resowner_portal_seams::resource_owner_create_portal::set(|| types_resowner::ResourceOwner::from_parts(1, 1));
        resowner_portal_seams::resource_owner_release::set(|_, _, _, _| {});
        resowner_portal_seams::resource_owner_delete::set(|_| {});
        xact_seams::get_current_transaction_nest_level::set(|| 1);
        xact_seams::get_current_sub_transaction_id::set(|| 1);
        xact_portal_seams::get_current_statement_start_timestamp::set(|| 424_242);
        ipc_portal_seams::shmem_exit_inprogress::set(|| false);
        // PortalCleanup: the executor's end releases what it took
        portalcmds_seams::portal_cleanup::set(|_| {
            USES.with(|u| u.borrow_mut().clear());
            Ok(())
        });
        execmain_seams::create_query_desc::set(|pstmt, _src, snapshot, _cross, _dest, _params, _env, _instr| {
            assert_eq!(pstmt.commandType, CmdType::CMD_SELECT);
            QD_SNAPSHOT.with(|s| *s.borrow_mut() = snapshot);
            Ok(QueryDescHandle(7))
        });
        execmain_seams::free_query_desc::set(|_| {});
        execmain_seams::release_query_desc::set(|_| {});
        pquery::init_seams();
        execmain_seams::executor_start::set(|_, _| {
            take_use();
            Ok(())
        });
        execmain_seams::executor_run::set(|_, _, _, _| {
            take_use();
            Ok(())
        });
        execmain_seams::executor_finish::set(|_| Ok(()));
        execmain_seams::executor_end::set(|_| Ok(()));
        execmain_seams::query_desc_es_processed::set(|_| 1);
        execmain_seams::query_desc_snapshot::set(|_| QD_SNAPSHOT.with(|s| s.borrow().clone()));
        execmain_seams::query_desc_result_tupdesc::set(|_| None);
        execmain_seams::query_desc_operation::set(|_| CmdType::CMD_SELECT);
    });
    portalmem::EnablePortalManager();
}

/// The planner's output for a one-row SELECT (a Result plan): PortalStart's strategy is PORTAL_ONE_SELECT.
fn select_stmts() -> types_portal::StmtListHandle {
    let base: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("c8-portal-owner")));
    let mcx = base.mcx();
    let tree = Node::mk(mcx, ResultPlan { plan: Plan { total_cost: 0.01, ..Plan::default() }, resconstantqual: None }).unwrap();
    let stmts: &'static [PlannedStmt<'static>] =
        Box::leak(vec![PlannedStmt { commandType: CmdType::CMD_SELECT, canSetTag: true, planTree: Some(tree), ..PlannedStmt::default() }].into_boxed_slice());
    // SAFETY: the statements are leaked 'static.
    unsafe { pquery::stmt_list::register(stmts) }
}

/// The ring's residues on account `a`: (source, class, uses).
fn residues(a: Acct) -> Vec<(Source, OwnerClass, i64)> {
    let mut v = Vec::new();
    for_each_recent(|r| {
        if r.acct == a {
            v.push((r.source, r.class, r.uses));
        }
    });
    v
}

/// DECLARE, two FETCHes and a CLOSE, one statement each: the cursor's uses are its own from its PortalStart to its
/// PortalDrop, so no statement's end finds one and its drop leaves none.
#[test]
fn a_cursors_executor_takes_for_the_portal_not_for_the_statement_that_runs_it() {
    let a = Acct::backend(PROCNO);
    std::thread::spawn(move || {
        install();
        memledger::bind_backend(PROCNO, 9000 + PROCNO);
        let base: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("c8-portal-snap")));
        let snap: snapmgr::Snapshot = std::rc::Rc::new(SnapshotData::sentinel(base.mcx(), SnapshotType::SNAPSHOT_MVCC));
        // the transaction's active snapshot, kept pushed: popping the last one resets MyProc's xmin (no proc here)
        snapmgr::PushActiveSnapshot(&snap).unwrap();

        // DECLARE: PortalStart (its ExecutorStart takes a use)
        memledger::statement_begin();
        let portal = portalmem::CreatePortal("c8_cur", false, false).unwrap();
        portalmem::PortalDefineQuery(&portal, None, "DECLARE c8_cur CURSOR FOR SELECT 1", CMDTAG_SELECT, select_stmts(), CachedPlanHandle::NULL).unwrap();
        pquery::PortalStart(&portal, ParamListHandle::NULL, 0, Some(snap.clone())).unwrap();
        assert_eq!(portal.borrow().strategy, PORTAL_ONE_SELECT);
        memledger::statement_end();
        assert_eq!(residues(a), [], "the DECLARE's end: the cursor's ExecutorStart use is the cursor's");
        assert_eq!(live_portals(a), 1, "the cursor is a memory owner from its PortalStart");

        // two FETCHes: PortalRun forward one row each (ExecutorRun takes a use each time)
        for _ in 0..2 {
            memledger::statement_begin();
            let mut dest = tcop_dest::CreateDestReceiver(CommandDest::None);
            pquery::PortalRun(&portal, 1, true, &mut dest, None, None).unwrap();
            memledger::statement_end();
        }
        assert_eq!(USES.with(|u| u.borrow().len()), 3);
        assert_eq!(residues(a), [], "a FETCH's end: what the suspended cursor's run took is the cursor's");

        // CLOSE: PortalDrop's cleanup ends the executor (the uses go), then the cursor's owner end finds nothing
        memledger::statement_begin();
        portalmem::PortalDrop(&portal, false).unwrap();
        memledger::statement_end();
        assert_eq!(USES.with(|u| u.borrow().len()), 0);
        assert_eq!(residues(a), [], "the CLOSE: no residue anywhere");
        assert_eq!(live_portals(a), 0, "the cursor's owner ended at its PortalDrop");
        memledger::unbind_backend();
        assert_eq!(residues(a), [], "the session's end: nothing left on it");
    })
    .join()
    .unwrap();
}
