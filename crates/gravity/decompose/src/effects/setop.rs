//! The set-operation family's planning-side effect decisions, asked by decompose ops/setop.rs. Each reads a plan
//! subtree's facts (rerun.rs `of_plan`, F5) through the shared `Effects` (`of_plan`, `Effects::of_found`: a VOLATILE call
//! acts, a node that can raise on some input may raise); only the user-call test is the family's own.
use types_nodes::Node;

use super::{of_plan, Cx, Effects};
use crate::rerun::Found;
use crate::walk::Builder;

/// The subtree at `n` neither acts nor may raise, and calls no user function.
pub fn quiet<'p>(b: &Builder<'p, '_>, n: Node<'p>) -> bool {
    quiet_in(b.stmt, b.facts, b.arena, n)
}

/// `quiet` over the statement, facts and arena alone (decompose ops/setop.rs `Reach`, D3373).
pub fn quiet_in<'p>(stmt: &'p types_nodes::plannodes::PlannedStmt<'p>, facts: &dyn crate::catalog::Facts, arena: gravity_program::ArenaRef, n: Node<'p>) -> bool {
    let f = crate::rerun::of_plan(stmt, facts, arena, n);
    let e = Effects::of_found(f);
    !e.acts && !e.may_raise && !f.user_call
}

/// D4295: the subtree at `n` acts past call-local builtins: it acts (a VOLATILE call) or may write through user code, and
/// the `call_local` proof over it is denied (a user function, an act past its call, a call that reads what a call-local
/// act moved, a statement that writes), so no licence covers running it ahead of the rows PostgreSQL hands on (a
/// recursive seed read whole). D4323: user code that may write counts whatever its volatility (`Found::may_write`, the
/// one predicate `effects::user_code_may_write` / `node_may_write` D4316's window cell reads): a STABLE or IMMUTABLE
/// function may call a VOLATILE writer, whose act its own volatility does not show.
pub fn acts_past_local<'p>(b: &Builder<'p, '_>, n: Node<'p>) -> bool {
    let cx = Cx::new(b.stmt, b.facts, b.arena);
    let f = crate::rerun::of_plan(b.stmt, b.facts, b.arena, n);
    (Effects::of_found(f).acts || f.may_write) && super::call_local(&cx, [n]).is_none()
}

/// F-SORTED (D1239): the subtree at `n` neither acts nor may raise (a user call is allowed), so PG's stop before its end
/// is unobservable.
pub fn stop_unseen<'p>(b: &Builder<'p, '_>, n: Node<'p>) -> bool {
    let e = of_plan(&Cx::new(b.stmt, b.facts, b.arena), n);
    !e.acts && !e.may_raise
}

/// A recursive term whose facts `f` (rerun.rs `of_plan`) act or call a user function.
pub fn acts(f: &Found) -> bool {
    Effects::of_found(*f).acts || f.user_call
}

/// D2800: the left input `ln` of a sorted SetOp may raise on a row PostgreSQL reads only as far as the SetOp is pulled. A
/// Sort reads its whole input at its first row (nodeSort.c ExecSort runs tuplesort_performsort before returning one) and
/// evaluates no expression of its own, so a Sort as the left is read whole at the SetOp's first pull, as the grouped form
/// reads it; any other left is asked through `of_plan` over its whole subtree (fail closed).
pub fn streamed_left_raises<'p>(b: &Builder<'p, '_>, ln: Node<'p>) -> bool {
    ln.as_sort().is_none() && of_plan(&Cx::new(b.stmt, b.facts, b.arena), ln).may_raise
}

/// AGA-30: a key whose type's default btree comparator reaches no user code (USEROP): a by-reference value of a built-in
/// base type (no pseudo-type, no `record[]`, whose comparator reaches each column's), its default btree comparator a
/// built-in. A built-in type's array is of a built-in element, so btarraycmp reaches built-in code only.
pub fn builtin_cmp_key(f: &dyn crate::catalog::Facts, tf: &crate::catalog::typefacts::TypeFacts) -> bool {
    use crate::catalog::typefacts::FIRST_NORMAL_OID;
    let base = f.type_row(tf.base).is_some_and(|r| r.typtype == b'b');
    !tf.byval && base && tf.base < FIRST_NORMAL_OID && tf.base != types_core::catalog::RECORDARRAYOID && tf.cmp != 0 && tf.cmp < FIRST_NORMAL_OID
}
