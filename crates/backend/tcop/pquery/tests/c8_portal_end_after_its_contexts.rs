//! C8 checker 1 (memledger owner_end), B1's follow-up (main 2026-10-12T17:39:52Z): a portal's owner ends at its PortalDrop,
//! and what the portal keeps in its own memory (portalContext: the plan and query copies, the executor state's parent;
//! holdContext: a held cursor's store) is freed late in PortalDrop, after the executor's release. A mapped block there
//! (mcx's door: >= MMAP_THRESHOLD, charged to the portal it was mapped under, remembered by address) must be freed before
//! the portal's owner end reads its cell, or the end reads it as the portal's residue and its free lands as a negative on
//! the session. Here the fake ExecutorStart takes a part use and maps a block in the portal's context that only
//! PortalDrop's context free gives back.

use std::cell::{Cell, RefCell};
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

/// This test's backend slot (memledger's own tests use 3907..3910; c8_portal_memory_owner 3931).
const PROCNO: u32 = 3932;

/// A block past mcx's mapping threshold (`mcx::heap::MMAP_THRESHOLD`, 32 MiB).
const BLOCK: usize = 40 << 20;

thread_local! {
    /// The fake executor's part uses: taken by ExecutorStart and ExecutorRun, dropped by PortalCleanup's ExecutorEnd.
    static USES: RefCell<Vec<Charge>> = const { RefCell::new(Vec::new()) };
    static QD_SNAPSHOT: RefCell<Option<execmain_seams::Snapshot>> = const { RefCell::new(None) };
    /// The running portal's context (the test sets it after CreatePortal): ExecutorStart maps its block there.
    static PORTAL_CTX: Cell<Option<&'static MemoryContext>> = const { Cell::new(None) };
}

/// A part open of the fake executor: its use is its taker's (the portal, statement or session the backend works for).
fn take_use() {
    let (owner, release) = context();
    USES.with(|u| u.borrow_mut().push(Charge::use_of(owner, release)));
}

/// What the portal keeps for its life in its own context: a mapped block that PortalDrop's context free gives back.
fn keep_block() {
    let ctx = PORTAL_CTX.with(Cell::get).expect("the portal's context");
    ctx.mcx().alloc_bytes(BLOCK).expect("a mapped block in the portal's context");
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
            keep_block();
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

/// DECLARE and CLOSE: the cursor's block (mapped under its PortalStart, in its portalContext) is the portal's until
/// PortalDrop frees its context, and the portal's owner end comes after that free: no residue anywhere.
#[test]
fn a_portals_end_comes_after_its_contexts_are_freed() {
    let a = Acct::backend(PROCNO);
    std::thread::spawn(move || {
        install();
        memledger::bind_backend(PROCNO, 9000 + PROCNO);
        let base: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("c8-portal-end-snap")));
        let snap: snapmgr::Snapshot = std::rc::Rc::new(SnapshotData::sentinel(base.mcx(), SnapshotType::SNAPSHOT_MVCC));
        // the transaction's active snapshot, kept pushed: popping the last one resets MyProc's xmin (no proc here)
        snapmgr::PushActiveSnapshot(&snap).unwrap();

        // DECLARE: PortalStart (its ExecutorStart takes a use and maps the block in the portal's context)
        memledger::statement_begin();
        let portal = portalmem::CreatePortal("c8_end", false, false).unwrap();
        // SAFETY: portalContext is PgBox'd for address stability and freed only in PortalDrop, after the last
        // ExecutorStart that reads it here (as pquery's RunFromStore borrows it).
        let ctx: &'static MemoryContext = unsafe {
            let p = portal.borrow();
            &*(&**p.portalContext.as_ref().expect("portal has portalContext") as *const MemoryContext)
        };
        PORTAL_CTX.with(|c| c.set(Some(ctx)));
        portalmem::PortalDefineQuery(&portal, None, "DECLARE c8_end CURSOR FOR SELECT 1", CMDTAG_SELECT, select_stmts(), CachedPlanHandle::NULL).unwrap();
        pquery::PortalStart(&portal, ParamListHandle::NULL, 0, Some(snap.clone())).unwrap();
        assert_eq!(portal.borrow().strategy, PORTAL_ONE_SELECT);
        memledger::statement_end();
        PORTAL_CTX.with(|c| c.set(None));
        assert_eq!(residues(a), [], "the DECLARE's end: the cursor's block and use are the cursor's");

        // CLOSE: PortalDrop's cleanup ends the executor (the use goes), its context free unmaps the block, then the
        // cursor's owner end finds nothing
        memledger::statement_begin();
        portalmem::PortalDrop(&portal, false).unwrap();
        memledger::statement_end();
        assert_eq!(USES.with(|u| u.borrow().len()), 0);
        assert_eq!(residues(a), [], "the CLOSE: the portal's end came after its block's free; no residue anywhere");
        assert_eq!(live_portals(a), 0, "the cursor's owner ended at its PortalDrop");
        memledger::unbind_backend();
        assert_eq!(residues(a), [], "the session's end: nothing left on it");
    })
    .join()
    .unwrap();
}
