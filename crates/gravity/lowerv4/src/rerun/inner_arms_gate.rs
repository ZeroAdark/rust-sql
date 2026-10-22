//! M8-NEST S2d (V4R-563, D2015's form C; a helper of nest/inner_arms.rs, split for its budget): an arm of a union a
//! nest (or nested SubPlan) inner embeds that holds a Result's one-time gate (resconstantqual). PostgreSQL's ExecResult
//! checks the gate at its first call after a rescan, before it pulls its outer or projects, and ExecAppend first calls
//! an arm only when it reaches it: `compile` makes the arm's gates (top-down, nest/inner.rs's D1915 order) and `check`
//! runs them at the arm's open in `Arms::pull`, a false or NULL one ending the arm unopened, once per re-open (the feed
//! never runs its gate step: the Append closes it, so it is in the rerun set, decompose rerun_set). `admit` refuses by
//! name what a gated arm does not serve: `WRITES` (D2194's no-widen name `S2D` left at item 14 part 4). D3829: a gate's
//! hashed SubPlan (`ANY (o.k = (hashed SubPlan 1).col1)`) is lent to the gate at the open (`gate_sub`, `check_lent`), as
//! ExecHashSubPlan runs from the Result's one-time qual with no row of the arm. Registers nothing. Owner M8-NEST.
use gravity_decompose::ops::shared::subplan::{refers, refers_list, SubForm};
use gravity_decompose::{Op, PipelineNode};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Block;
use mcx::{CollectIn, PgBox};

use crate::face::step::{ExprFace, StepMech};
use crate::frame::env::Env;
use crate::frame::px::{ExId, Px};
use crate::frame::row::{Carry, RowVals};
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};

const WRITES: &str = "a union's arm under a one-time gate in a nest inner (S2d) in a statement that writes or locks rows (EvalPlanQual would recheck the gate over the new row's parameters) (D2015)";

/// inner_arms.rs `admit_arm`'s hook: a gated arm refuses a writing or locking statement.
pub(crate) fn admit(gx: &Gx<'_, '_>, body: &[Op<'_>], node: i32) {
    if body.iter().any(|op| matches!(op, Op::Gate(_))) && gx.pipelines().iter().any(crate::rerun::inner_arms_index::writes) {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy(WRITES) })
    }
}

/// Feed `kf`'s gates (its ops before the Feed), compiled under the maker's identity: `(node, qual)`, the upper Result's list
/// first, each list in its order (D1915).
pub(crate) fn compile<'p>(px: &mut Px<'_, 'p>, fp: &PipelineNode<'p>, kf: usize) -> Vec<(i32, ExId)> {
    let mut gates = Vec::new(); // alloc: Program
    for (j, op) in fp.ops[..fp.ops.len() - 1].iter().enumerate() {
        if let Op::Gate(g) = op {
            let (n, ex): (i32, &'static ExprFace) = (crate::rerun::inner::op_node(fp, j), crate::frame::iface::expr_face(&fp.node_id));
            let mcx = px.mcx();
            let quals = g.as_list().map_or_else(|| mcx::pg_vec![in mcx; *g], |l| l.iter().collect_in(mcx));
            gates.splice(0..0, quals.into_iter().map(|q| (n, ex.0.compile_at(px, (kf, j), q, None, n)))); // alloc: Program
        }
    }
    gates
}

/// The arm's gates at its open, each while every earlier one passed: a false or NULL one branches to the block returned,
/// which `shut` joins to the arm's end path. An arm with no gate makes no block (its IR is S2c's).
pub(crate) fn check(b: &mut FunctionBuilder<'_>, env: &mut Env, gates: &[(i32, ExId)]) -> Option<Block> {
    if gates.is_empty() {
        return None;
    }
    let (l_shut, none) = (b.create_block(&[]), RowVals { row: crate::cell::Row::new_in(env.mcx), vals: mcx::PgVec::new_in(env.mcx), cols: mcx::PgVec::new_in(env.mcx), carry: Carry::new(env.zero, env.mcx), next: env.zero, at: env.zero });
    for &(n, q) in gates {
        let ex: &'static ExprFace = crate::frame::iface::expr_face(&n);
        let (p, l_more) = (ex.0.pred(b, env, n, &none, q), b.create_block(&[]));
        b.cond_br(p, (l_more, &[]), (l_shut, &[]));
        b.switch_to(l_more);
    }
    Some(l_shut)
}

/// D3829: op `j` of arm body `body` is a hashed SubPlan's probe only the arm's one-time gates read. PG places it in the
/// Result's resconstantqual, which reads no column of the arm, so the probe's left side is the nest's parameters only.
pub(crate) fn gate_sub(body: &[Op<'_>], j: usize) -> bool {
    let Some(Op::SubPlan(d)) = body.get(j) else { return false };
    if !matches!(d.form, SubForm::Hashed { .. }) {
        return false;
    }
    let id = d.plan_id;
    let mut gated = false;
    for op in &body[j + 1..] {
        match op {
            Op::Gate(q) => gated |= refers(*q, id),
            Op::Filter(q) if refers(*q, id) => return false,
            Op::Project(l) if refers_list(l, id) => return false,
            Op::Filter(_) | Op::Project(_) | Op::SubPlan(_) | Op::Append(_) => {}
            _ => return false,
        }
    }
    gated
}

/// D3829: `check` with each probe of `lenders` lent: such a probe is a `Source` one (site.rs `gated`: read by gates
/// alone), lent from its prologue, which a lender gets only here (the inner's prologue runs the arm's steps' alone, D4062),
/// so it runs here and its entries go after the check.
pub(crate) fn check_lent(b: &mut FunctionBuilder<'_>, env: &mut Env, lenders: &[PgBox<'_, dyn StepMech + '_>], gates: &[(i32, ExId)]) -> Option<Block> {
    let (held, x) = (env.subplans.len(), env.x);
    lenders.iter().for_each(|s| s.prologue(b, env, &x));
    let shut = check(b, env, gates);
    env.subplans.truncate(held);
    shut
}

/// The arm's end path, where a gate `check` stopped joins it.
pub(crate) fn shut(b: &mut FunctionBuilder<'_>, l_shut: Option<Block>) {
    if let Some(l) = l_shut {
        b.br(l, &[]);
        b.switch_to(l);
    }
}
