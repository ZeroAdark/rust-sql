use mcx::{Mcx, PgVec};
use tcop_dest::DestReceiver;
use types_dest::CommandDest;
use types_error::PgResult;
use types_nodes::parsenodes::Query;
use types_nodes::plannodes::PlannedStmt;
use types_portal::{CachedPlanHandle, ParamListHandle, Portal, QueryCompletion};

seam_core::seam!(
    pub fn bind<'p>(portal: &'p Portal<'static>, cplan: CachedPlanHandle, params: ParamListHandle) -> PgResult<bool>
);

seam_core::seam!(
    pub fn describe<'p>(portal: &'p Portal<'static>) -> PgResult<bool>
);

seam_core::seam!(
    pub fn execute<'p, 'd, 'mcx>(portal: &'p Portal<'static>, max_rows: i64, receiver: &'d mut DestReceiver<'mcx>) -> PgResult<Option<Option<QueryCompletion>>>
);

seam_core::seam!(
    pub fn fetch<'p, 'd, 'mcx>(portal: &'p Portal<'static>, count: i64, receiver: &'d mut DestReceiver<'mcx>) -> PgResult<u64>
);

seam_core::seam!(
    pub fn feedback_flush()
);

seam_core::seam!(
    pub fn simple<'s, 'a, 'd, 'mcx>(pstmt: &'s PlannedStmt<'a>, dest: &'d mut DestReceiver<'mcx>) -> PgResult<Option<QueryCompletion>>
);

seam_core::seam!(
    /// Re-plan and retry: whether the last `simple` call refused its statement at the admission, before anything ran or
    /// reached the client (read once).
    pub fn simple_refused_at_admission() -> bool
);

seam_core::seam!(
    /// Re-plan and retry: tcop may plan the next `simple` statement again if its admission refuses, so the refusal's
    /// verdict line waits for `replan_end`.
    pub fn replan_arm()
);

seam_core::seam!(
    /// Re-plan and retry: the armed statement's road ends; `served` false logs the first plan's refusal line.
    pub fn replan_end(served: bool)
);

seam_core::seam!(
    /// Re-plan and retry: a statement was served by a plan made under `settings` after its first plan was refused.
    pub fn note_replanned<'s>(settings: &'s str)
);

seam_core::seam!(
    /// The statement's plans built without the planner (`Ok`), or its queries handed back for `pg_plan_queries` (`Err`).
    pub fn plan_skip<'mcx>(mcx: Mcx<'mcx>, querytrees: PgVec<'mcx, Query<'mcx>>, dest: CommandDest) -> PgResult<Result<PgVec<'mcx, PlannedStmt<'mcx>>, PgVec<'mcx, Query<'mcx>>>>
);
