//! D2772 POST (modifytable, regress triggers#500): PostgreSQL's ExecPostprocessPlan for a data-modifying CTE nothing
//! reads. PostgreSQL runs such a CTE's ModifyTable once the statement's own ModifyTable has ended (`es_auxmodifytables`):
//! the CTE's BEFORE STATEMENT triggers fire after the statement's rows, and its row and AFTER STATEMENT events queue after
//! the statement's AFTER STATEMENT events. D3449: two such CTEs run one after the other, the later CTE plan first, each to
//! its end. The lowerer orders their pipelines last, group by group, and flags them `Pipeline::post` (lowerv4
//! ops/modifytable/post.rs), so every runner runs them after the statement's own in program order, and a recheck of the
//! statement's own write (which reruns only pipelines before it) reruns none of them, as EvalPlanQual never runs an unread
//! CTE's plan. Before each group's first pipeline the execution turns (`turn`, the runners' loops): the runner's hook
//! `POST_TURN` queues the AFTER STATEMENT events so far (the statement's, then the group before's), then the group's writer
//! binds, which fires its BEFORE STATEMENT triggers (node/ends.rs: until then no host walk binds it). A program without
//! post pipelines never turns.

use pgsync::OnceLock;

use super::{Exec, ExecState};
use crate::binding::Binding;
use crate::Result;

/// The runner's turn hook (exec `after_query::turn`): the AFTER STATEMENT events so far queued, before a post group's
/// writer binds. Unset: nothing queued at the turn.
pub static POST_TURN: OnceLock<fn() -> Result<()>> = OnceLock::new();

/// The turn before pipeline `p` (the runners' loops call it before each pipeline they run or pass): when `p` is a post
/// pipeline whose group's writer has not bound (its first), the runner's hook, then that writer bound on the execution's
/// node. An error leaves the execution failed.
pub fn turn(exec: &mut Exec, binding: &mut Binding<'_>, p: usize) -> Result<()> {
    let Ok(at) = u16::try_from(p) else { return Ok(()) };
    if !exec.node.program().pipelines.get(p).is_some_and(|x| x.post) || !exec.node.binds_at(at) {
        return Ok(());
    }
    let r = POST_TURN.get().map_or(Ok(()), |f| f()).and_then(|()| exec.node.bind_at(&binding.as_ref(), at));
    if r.is_err() {
        exec.state = ExecState::Failed;
    }
    r
}
