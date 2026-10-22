use gravity_program::{Cmd, LockKind, WritePlan};
use types_error::PgResult;

use crate::shapes::Admitted;

/// ExecutorStart's `es_output_cid`, taken once at the statement's start (a cursor's DECLARE) and carried in the binding.
pub fn output_cid(a: &Admitted) -> PgResult<u32> {
    if a.cmd == Cmd::Select && a.program.write.lock == LockKind::None { Ok(0) } else { xact::GetCurrentCommandId(true) }
}

pub fn side_program_refusal(w: &WritePlan) -> Option<&'static str> {
    (w.cmd != Cmd::Select || w.lock != LockKind::None).then_some("write: a lock or write inside an Append arm or InitPlan (the output command id is the main program's; subplan lane need)")
}
