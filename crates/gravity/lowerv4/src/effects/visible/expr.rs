//! The effects home, expr's sibling file (constructs census task 12, EFF-EXPR; main's layout 10-05: each lane's moved
//! decisions in effects/visible/<family>.rs, one mod line in visible.rs). The step face's own effect questions over a
//! compiled expression, moved here unchanged from step/expr.rs, step/expr_saop.rs and step/expr_ext_lazy.rs (lint rule
//! RV: no function outside the effects home decides volatility):
//! - `acts`: compiled `c` holds a hosted call that acts (step/expr.rs keeps its `held` rows by it);
//! - `volatile_site`: compiled `c` holds a hosted call read as VOLATILE, fail closed (`Ex::volatile`, read where it is placed);
//! - `volatile_sub`: an operand of an extension's claim holds a VOLATILE call (expr.saop's array after its scalar, D946);
//! - `volatile`: an expression holds a VOLATILE call or reads an InitPlan parameter, in its claims' operands too
//!   (expr_ext_lazy's `on`: the claimed sites emitted at their read, V4R-342);
//! - `operand`: a claim operand's plan node makes a VOLATILE call by the shared reading, or holds a SubPlan whose step
//!   acts (D4143), joined into its `Ex::volatile`;
//! - `user_code`: compiled `c` holds a hosted call to user code (D2044, step/expr.rs's pull cap).
use gravity_expr::Compiled;
use types_nodes::Node;

use crate::face::step::ExtPlan;
use crate::frame::env::Env;
use crate::frame::px::ExId;
use crate::graph::Gx;

/// Item 6: whether compiled `c` holds a hosted call that acts, asked of the effects home per call (`effects::of_call`:
/// VOLATILE, or a function the catalog cannot answer); a site with no function (an island, a store shim) is its own call
/// bit, its inner calls'. A primitive the emit inlines is no hosted call.
pub(crate) fn acts(facts: &dyn gravity_decompose::catalog::Facts, c: &Compiled<'_>) -> bool {
    let call = |s: &gravity_expr::nodes::func::Call| if s.funcid == 0 { s.volatile } else { gravity_decompose::effects::of_call(facts, s.funcid).acts };
    c.prog.sites.iter().any(|s| !gravity_expr::emit::prim::is_prim(&c.prog, s, false) && call(s))
}

/// Whether compiled `c` holds a hosted call read as VOLATILE, fail closed: its call bit (gravity_expr `FuncFacts::call_acts`:
/// VOLATILE, or an fmgr call the catalog cannot answer) or, for a site with a function, effects' per-call answer
/// (`effects::of_call`: VOLATILE, or a function the catalog cannot answer, a direct shim's too). A primitive the emit
/// inlines is no hosted call.
pub(crate) fn volatile_site(facts: &dyn gravity_decompose::catalog::Facts, c: &Compiled<'_>) -> bool {
    let call = |s: &gravity_expr::nodes::func::Call| s.volatile || s.funcid != 0 && gravity_decompose::effects::of_call(facts, s.funcid).acts;
    c.prog.sites.iter().any(|s| !gravity_expr::emit::prim::is_prim(&c.prog, s, false) && call(s))
}

/// Whether claim operand `node` makes a VOLATILE call by the shared fail-closed union (effects.rs `volatile`: a call
/// pg_proc marks VOLATILE or cannot answer, a cast to a domain whose CHECK makes one (D1899), a range function over a
/// range whose comparison is user code (D2065)), read at compile where the node is (step/expr.rs `compile_in`) and joined
/// to its compiled sites' reading (`volatile_site`) in its `Ex::volatile`, which `volatile_sub` and `volatile` read at
/// emit: a builtin range function runs a user comparator no compiled site names, so its operand is emitted at its read
/// (expr_ext_lazy.rs), never ahead of a CASE arm or a raise PostgreSQL stops at. D4143: a SubPlan in it whose run acts
/// by the one SubPlan answer of its step in pipeline `k` (`subplan::whole`, fail closed where no step names it) counts
/// too: read ahead, a sub-SELECT's PL/pgSQL write or nextval ran on rows whose CASE or AND / OR PostgreSQL decided
/// without it.
pub(crate) fn operand(gx: &Gx<'_, '_>, k: usize, node: Node<'_>) -> bool {
    crate::effects::volatile(gx, node) || super::subplan::whole(gx, k, node).effects.acts
}

/// D946 (V4R-342): whether operand `i` of `ex`'s claim holds a VOLATILE call (in its own claims' operands too).
pub(crate) fn volatile_sub(env: &Env, ex: ExId, i: u32) -> bool {
    fn vol(env: &Env, id: ExId) -> bool {
        crate::stack_check();
        let e = env.placed(id);
        e.volatile || ExtPlan::of(e).is_some_and(|x| x.subs.borrow().iter().any(|&s| vol(env, s)))
    }
    ExtPlan::of(env.placed(ex)).and_then(|x| x.subs.borrow().get(i as usize).copied()).is_some_and(|s| vol(env, s))
}

/// Whether expression `id` holds a VOLATILE call or reads an InitPlan parameter (M8 B1: PG runs the InitPlan where the
/// Param is reached), in its claims' operands too.
pub(crate) fn volatile(env: &Env, id: ExId) -> bool {
    // the placed expressions' own bits (`Ex::volatile`, read at compile), its claims' operands' in turn
    fn held(env: &Env, id: ExId) -> bool {
        crate::stack_check();
        let e = env.placed(id);
        e.volatile || !crate::step::expr_ext_lazy::inits(env, id).is_empty() || ExtPlan::of(e).is_some_and(|x| x.subs.borrow().iter().any(|&s| held(env, s)))
    }
    held(env, id)
}

/// D2044: whether compiled `c` holds a hosted call to user code (the effects home's `user_code`: its raise can leave SPI,
/// GUC or snapshot state behind).
pub(crate) fn user_code(c: &Compiled<'_>) -> bool {
    c.prog.sites.iter().any(|s| gravity_decompose::effects::user_code(s.funcid))
}
