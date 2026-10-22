//! The run every stage's hook B takes: the run state kept across statements, on the statement's owner while it runs
//! (`with_run`), and ExecutorStart's checks and the statement's snapshot around the stage's body (`exec_in`).
use std::rc::Rc;

use gravity_program::Cmd;
use mcx::Mcx;
use types_error::PgResult;
use types_nodes::plannodes::PlannedStmt;
use types_snapshot::SnapshotData;

use super::{state_new, Running, State, STATE};

/// The run on the statement's owner: 'GravityPointRun' takes the statement's owner, `body` runs with the run context and
/// the wire node, and at its end, error or not, the wire node's run ends (D4474: the node is kept across statements and
/// the path sends outside `drive`), the context is reset and goes back to the session, before the statement's owner-end
/// check, so only its keeper block stays. The state stays in its thread-local for the run (borrowed, not moved out and
/// back: the wire node is a large value); a run never nests (`Running`: a point statement inside one declines first).
pub(super) fn with_run<T>(body: impl FnOnce(Mcx<'_>, &mut gravity_wire::WireNode) -> T) -> T {
    let _running = Running::enter();
    let r = STATE.with(|s| {
        let mut slot = s.borrow_mut();
        let state = slot.get_or_insert_with(state_new);
        state.ctx.set_owner(memledger::charge::context().0);
        let end = End(state);
        let State { ctx, wire } = &mut *end.0;
        body(ctx.mcx(), wire)
    });
    super::super::execute::arm_teardown();
    r
}

/// The run's end on every exit (a raised error, a panic's unwind too).
struct End<'s>(&'s mut State);

impl Drop for End<'_> {
    fn drop(&mut self) {
        self.0.wire.end_run();
        self.0.ctx.reset();
        self.0.ctx.set_owner_session();
    }
}

/// ExecutorStart's permission checks (start_checks), then the statement's snapshot registered and pushed around `body`
/// (PortalStart / ExecutorStart), popped and unregistered on every exit.
pub(super) fn exec_in<T>(pstmt: &PlannedStmt<'_>, mcx: Mcx<'_>, body: impl FnOnce(&Rc<SnapshotData<'static>>) -> PgResult<T>) -> PgResult<T> {
    exec_cmd_in(pstmt, Cmd::Select, mcx, body)
}

/// `exec_in` for a statement of command `cmd`: a write's checks are a write's (ExecCheckXactReadOnly's read-only guard).
pub(super) fn exec_cmd_in<T>(pstmt: &PlannedStmt<'_>, cmd: Cmd, mcx: Mcx<'_>, body: impl FnOnce(&Rc<SnapshotData<'static>>) -> PgResult<T>) -> PgResult<T> {
    super::super::start_checks(pstmt, cmd, mcx)?;
    let snap = super::super::register_snapshot()?;
    let r = snapmgr::PushActiveSnapshot(&snap).and_then(|()| {
        let r = body(&snap);
        let popped = snapmgr::PopActiveSnapshot();
        r.and_then(|q| popped.map(|()| q))
    });
    snapmgr::UnregisterSnapshot(Some(&snap));
    r
}
