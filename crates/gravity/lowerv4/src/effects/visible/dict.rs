//! The dict family's visible decision, in the effects home (lint rule R1; from dict/premise.rs's code cell walk):
//! - `call_acts`: whether a call in a code cell may act per row, so the cell is not evaluable once per code.

use gravity_decompose::Facts;

/// Whether a call of function `f` (a FuncExpr's `funcid`) may act per row: decompose effects' per-call answer (`of_call`:
/// its function is VOLATILE, `pg_proc.provolatile` 'v', or the facts cannot answer for it, fail closed).
pub(crate) fn call_acts(facts: &dyn Facts, f: u32) -> bool {
    gravity_decompose::effects::of_call(facts, f).acts
}
