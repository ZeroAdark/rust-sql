//! The lowering-end agreement check (C1, one effects answer): one question, "may this expression raise, may it run user
//! code", has three readers today: the effects home's `of_expr` (the node's `Effects`), decompose's F5 walk
//! (`rerun::of_expr_own`, its `Found`) and lowerv4's own answers (`Fx::quiet`, `Fx::user_calls`). After a lowering
//! succeeds, every expression a pipeline op evaluates (a filter, a gate, a projection's items; a graph pipeline's or an
//! embedded inner's) is read by all three in the per-expression scope (a SubPlan by its own parts, its plan where its
//! ops run), and a reader of the lowering half that is more permissive than the node's answer is a disagreement:
//! lowerv4 reads quiet where the node may raise (`QuietOverRaise`), or no user code where F5 finds a user call
//! (`NoUserOverUser`). The reverse, the planning half reading no user code where lowerv4 finds some
//! (`PlanningMissesUser`), is reported too: D3923's shape, where a gate that ANDs both halves denies nothing.
//!
//! `pgrust.gravity_effects_agree` (inputs `effects_agree`): off (the default) runs nothing; `report` prints one
//! `gravity effects agree:` line per disagreement to the server log; `assert` also fails a debug build at the first one
//! `KNOWN` does not name. A real disagreement is booked as a D-id and named in `KNOWN` until its fix lands.
use gravity_decompose::effects::{of_expr, Cx, PLAIN};
use gravity_decompose::Op;
use gravity_program::ArenaRef;
use mcx::{CollectIn, GrowIn, Mcx, PgVec};
use types_nodes::Node;

use crate::graph::Gx;

/// The check's mode (`pgrust.gravity_effects_agree`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Off,
    Report,
    Assert,
}

/// Which reader is more permissive than which.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// lowerv4's `Fx::quiet` passes the expression; the effects home's `of_expr` or F5 says it may raise.
    QuietOverRaise,
    /// lowerv4's `Fx::user_calls` finds no user code; F5 finds a user call.
    NoUserOverUser,
    /// F5 finds no user call; lowerv4's `Fx::user_calls` finds user code (the planning half reads less).
    PlanningMissesUser,
}

/// One disagreement: pipeline `pipeline`, op `op` of it, the readers' answers and the expression's node tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Disagreement {
    pub(crate) pipeline: usize,
    pub(crate) op: usize,
    pub(crate) kind: Kind,
    pub(crate) tag: String,
    /// The first function or operator function the expression calls that the readers split on, 0 when none is named.
    pub(crate) funcid: u32,
}

/// Disagreements booked as D-ids, by (kind, node tag, function): reported as known, never asserted. Empty until the first
/// collection run books what it finds.
const KNOWN: &[(Kind, &str, u32, &str)] = &[];

fn known(d: &Disagreement) -> Option<&'static str> {
    KNOWN.iter().find(|k| k.0 == d.kind && k.1 == d.tag && (k.2 == 0 || k.2 == d.funcid)).map(|k| k.3)
}

/// The expressions op `o` evaluates per row that the check reads: a filter's or gate's qual, a projection's items.
fn exprs<'p, 'g>(o: &Op<'p>, mcx: Mcx<'g>) -> PgVec<'g, Node<'p>> {
    match o {
        Op::Filter(n) | Op::Gate(n) => mcx::pg_vec![in mcx; *n],
        Op::Project(l) => l.iter().collect_in(mcx),
        _ => PgVec::new_in(mcx),
    }
}

/// The first call in `n` (a function's oid, an operator's function), 0 when it calls none.
fn first_call(n: Node<'_>) -> u32 {
    let mut f = 0;
    gravity_decompose::expr::walk(n, &mut |x| {
        if f == 0 {
            f = x.as_func_expr().map(|c| c.funcid).or_else(|| x.as_op_expr().map(|o| o.opfuncid)).unwrap_or(0);
        }
        f == 0
    });
    f
}

/// Every disagreement in the lowered graph `gx`: its graph pipelines, then its embedded inners (a nest's, a nested
/// SubPlan's: pipeline `len() + i`), where a SubPlan's plan runs. Each reader is asked in the lowering's per-expression
/// scope: a SubPlan in an expression counts its own parts (F5's `of_expr_own`), its plan being read where its ops run, as
/// lowerv4's readers leave it to the pipeline's `Op::SubPlan` (effects.rs `user_code_of`).
pub(crate) fn disagreements<'g>(gx: &Gx<'g, '_>, arena: ArenaRef) -> PgVec<'g, Disagreement> {
    let cx = Cx::without_stmt(gx.facts, arena);
    let mut out = PgVec::new_in(gx.mcx);
    let ps = gx.pipelines();
    let all = ps.iter().chain(ps.inners().into_iter().map(|x| x.1));
    for (q, p) in all.enumerate() {
        for (j, o) in p.ops.iter().enumerate() {
            for n in exprs(o, gx.mcx) {
                let found = match gx.graph.stmt {
                    Some(s) => gravity_decompose::rerun::of_expr_own(s, gx.facts, arena, n),
                    None => gravity_decompose::rerun::of_expr_alone(gx.facts, arena, n),
                };
                let node_raises = of_expr(&cx, n, &PLAIN).may_raise || !found.raise_free;
                let lowering_user = gx.fx(n).user_calls();
                let mut push = |kind| out.push_in(Disagreement { pipeline: q, op: j, kind, tag: format!("{:?}", n.node_tag()), funcid: first_call(n) }); // alloc: Cold
                if node_raises && gx.fx(n).quiet() {
                    push(Kind::QuietOverRaise);
                }
                if found.user_call && !lowering_user {
                    push(Kind::NoUserOverUser);
                }
                if !found.user_call && lowering_user {
                    push(Kind::PlanningMissesUser);
                }
            }
        }
    }
    out
}

/// At the end of a successful lowering: under `report` or `assert`, read every expression and print each disagreement;
/// under `assert` a debug build fails at the first one `KNOWN` does not name.
pub(crate) fn check(gx: &Gx<'_, '_>, arena: ArenaRef) {
    let mode = crate::inputs::effects_agree();
    if mode == Mode::Off {
        return;
    }
    for d in disagreements(gx, arena) {
        let k = known(&d);
        eprintln!("gravity effects agree: {:?} pipeline {} op {} node {} call {}{}", d.kind, d.pipeline, d.op, d.tag, d.funcid, k.map_or(String::new(), |id| format!(" known {id}"))); // alloc: Cold
        debug_assert!(mode != Mode::Assert || k.is_some(), "gravity effects agree: {d:?} (C1: book it, then name it in end_check.rs KNOWN)");
    }
}
