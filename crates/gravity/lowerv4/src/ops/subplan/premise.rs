//! subplan's premises (v5 landing 4, moved from props/ops.rs's subplan arm): a correlated subplan's test and argument
//! expressions' kinds. The props walk calls `subplan` for each Op::SubPlan.
use gravity_decompose::ops::shared::subplan::SubPlanDesc;

use crate::props::Walk;

impl<'p> Walk<'_, '_, 'p> {
    /// One Op::SubPlan.
    pub(crate) fn subplan(&mut self, s: &SubPlanDesc<'p>) {
        if let Some(t) = s.test {
            self.expr(t);
        }
        self.list(s.args);
    }
}
