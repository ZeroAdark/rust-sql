//! The settled union's calm and raise-only reads in the effects home (rerun/union_reads.rs's D1486 / D3373 rules, moved):
//! `raise_only` / `raise_only_in`, work that may raise but shows nothing else wherever it runs (the window rule's
//! arguments, a nest's own parameters bound), and `calm_nest` with its steps (`calm_emit`, `calm_in`, `chain_calm`,
//! `steps_calm`), rule A's nest whose per-row work may only raise, and `calm_write` with `written`, the write the
//! drained-write (D3499) and written-arms (D4202) rules take. Each expression is read through `gx.fx(n)`.
use gravity_decompose::graph::ParamOwner;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::ops::shared::modifytable::WriteDesc;
use gravity_decompose::ops::shared::sort::SortForm;
use gravity_decompose::{Op, PipelineNode, Source};
use gravity_program::Cmd;
use types_nodes::Node;

use super::nestloop::quiet_in;
use crate::graph::Gx;

/// D1486: `n` may raise, but nothing else it does shows wherever it runs (where PostgreSQL raises one error and Gravity
/// another, both answers are an error, and which error is not part of the answer): every call a builtin that is not
/// VOLATILE (`effects::user_fn`, `effects::volatile`) and emits no NOTICE (`effects::may_notice`), no value of a user type
/// (`effects::user_typed`: its support functions are user code), no executor parameter (a SubPlan's or an InitPlan's
/// output, whose run is a call), SubPlan or domain coercion (its CHECK may call anything), and only nodes the walker
/// knows. Run after a union's arms rather than between their rows, such work changes only which of two errors comes
/// first, and that it raises at all is its own rows' question either way.
pub(crate) fn raise_only(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    raise_only_in(gx, n, &|_| false)
}

/// D3373: `raise_only` where the PARAM_EXEC ids `bound` names are bound words: a nest's own parameters, the
/// outer row's values written before the inner's re-open (nodeNestloop.c), whose read runs nothing.
pub(crate) fn raise_only_in(gx: &Gx<'_, '_>, n: Node<'_>, bound: &dyn Fn(i32) -> bool) -> bool {
    if gx.fx(n).user_calls() || gx.fx(n).volatile_any() || gx.fx(n).user_typed() {
        return false;
    }
    gx.fx(n).raise_only_by(bound)
}

/// The spool rules' emit: the Union under filters and projections that call nothing and may only raise (`calm`).
pub(crate) fn calm_emit(gx: &Gx<'_, '_>, emit: usize, calm: &dyn Fn(Node<'_>) -> bool) -> bool {
    gx.pipelines()[emit].ops.iter().all(|op| match op {
        Op::Append(AppendDesc::Union { .. }) => true,
        Op::Filter(x) => calm(*x),
        Op::Project(l) => l.iter().all(|x| calm(x)),
        _ => false,
    })
}

/// D1486 (information_schema.check_constraints under an IN over domain_constraints): rule A's
/// nest (`unconditional`) whose inner is not `quiet_nest`'s one plain scan. The union is the nest's outer, and a NestLoop
/// reads its outer to its end whatever its join type, so PostgreSQL still reads every arm to its end; between two arms'
/// rows it runs the nest's per-row work: its parameters, join qual and inner. When none of that calls what acts (`calm_in`:
/// no user or VOLATILE function, no builtin that may emit a NOTICE, no value of a user type, no SubPlan or domain coercion,
/// a nest's own parameters read as the outer row's values), it may only raise; run after the arms instead, over the same
/// rows, it raises where PostgreSQL does and changes only which of two errors comes first (`raise_only`: which
/// error is not part of the answer). The inner is a heap range or index scan, or a plan Material spool (PostgreSQL's Materialize, filled
/// once and walked again at each re-open) whose fill and every pipeline feeding it read the same way (`chain_calm`), the
/// inner plan's F5 facts below VOLATILE (the effects home's `nest_inner_calm`). When and how often that fill runs stays
/// the Material's own form question.
pub(crate) fn calm_nest(gx: &Gx<'_, '_>, emit: usize, op: &Op<'_>) -> bool {
    let Op::Join(JoinDesc::Nest { params, qual, inner, facts, .. }) = op else { return false };
    let calm = |x: Node<'_>| calm_in(gx, x);
    if !crate::effects::nest_inner_calm(facts.volatility) || !params.iter().all(|p| p.as_nest_loop_param().is_some_and(|p| calm(p.paramval))) || !qual.iter().all(calm) {
        return false;
    }
    let own = |s: &u8| matches!(inner.source, Source::Breaker { slot } if slot == *s);
    if !inner.desc.consumes.iter().all(own) || !steps_calm(gx, inner) {
        return false;
    }
    match inner.source {
        Source::Range { am: gravity_decompose::Am::Heap, .. } | Source::Index { am: gravity_decompose::Am::Heap, .. } => true,
        Source::Breaker { slot } => crate::rerun::union_reads::fill_of(gx, emit, inner, slot).is_some_and(|f| chain_calm(gx, f)),
        _ => false,
    }
}

/// `calm_nest`'s reading of one expression: quiet (`quiet_in`, bound parameters read as words), or it may raise but calls
/// nothing that acts (`raise_only`'s reading, a nest's own parameter allowed: the outer row's value, no call).
pub(crate) fn calm_in(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    if quiet_in(gx, n, true) {
        return true;
    }
    if gx.fx(n).user_calls() || gx.fx(n).volatile_any() || gx.fx(n).user_typed() {
        return false;
    }
    let nest_param = |id: i32| gx.graph.params.iter().any(|(p, r)| *p == id && matches!(r.owner, ParamOwner::Nest { .. }));
    gx.fx(n).raise_only_by(&nest_param)
}

/// Fill pipeline `f` and every pipeline feeding it (hashjoin build_class.rs `feed`: the builds it probes, the spools its
/// nests read, transitively) calls nothing that acts: each one's source and ops are `steps_calm`, and none produces a
/// group (an aggregate's transitions are their own calls).
pub(crate) fn chain_calm(gx: &Gx<'_, '_>, f: usize) -> bool {
    let ps = gx.pipelines();
    let src = if let Source::Breaker { slot } = ps[f].source { Some(slot) } else { None };
    let feeders = crate::ops::hashjoin::build_class::feed(gx, f, ps[f].desc.consumes.iter().copied().chain(src));
    core::iter::once(f).chain(feeders).all(|x| x < ps.len() && steps_calm(gx, &ps[x]) && gx.group_at(x).is_none())
}

/// Pipeline (or nest inner) `p`'s source and ops call nothing that acts (`calm_in`): a heap range, a breaker, a constant,
/// or a heap index scan of calm keys under builtin operators and a calm recheck; filters, gates, projections, hash probes
/// and builds of calm keys, clauses and quals, Memoize caches of calm keys under builtin hash operators, a Material's
/// spool write, a settled union's row moves, a full sort under builtin operators, and nests whose inner plan's F5 facts
/// are below VOLATILE, of calm parameters and qual over an inner read the same way.
pub(crate) fn steps_calm(gx: &Gx<'_, '_>, p: &PipelineNode<'_>) -> bool {
    crate::stack_check(); // D3832
    let calm = |x: Node<'_>| calm_in(gx, x);
    let builtin = |opno: u32| opno == 0 || gravity_decompose::effects::builtin_oper(opno).is_some();
    let src = match &p.source {
        Source::Range { am: gravity_decompose::Am::Heap, .. } | Source::Breaker { .. } | Source::Const => true,
        Source::Index { am: gravity_decompose::Am::Heap, keys, recheck, .. } => keys.iter().all(|k| calm(k.value) && builtin(k.opno)) && recheck.iter().all(calm),
        _ => false,
    };
    src && p.ops.iter().all(|op| match op {
        Op::Filter(x) | Op::Gate(x) => calm(*x),
        Op::Project(l) => l.iter().all(calm),
        Op::Join(JoinDesc::Probe { keys, clauses, qual, .. }) => keys.iter().chain(clauses.iter()).chain(qual.iter()).all(calm),
        Op::Join(JoinDesc::Build { keys, .. }) => keys.iter().all(calm),
        Op::Join(JoinDesc::Memo { keys, ops, .. }) => keys.iter().all(calm) && ops.iter().all(|&o| builtin(o)),
        Op::Join(JoinDesc::Spool) => true,
        Op::Join(JoinDesc::Nest { params, qual, inner, facts, .. }) => {
            crate::effects::nest_inner_calm(facts.volatility)
                && params.iter().all(|x| x.as_nest_loop_param().is_some_and(|x| calm(x.paramval)))
                && qual.iter().all(calm)
                && steps_calm(gx, inner)
        }
        Op::Append(AppendDesc::Feed { .. } | AppendDesc::Union { prune: None, .. }) => true,
        Op::Sort(d) => matches!(d.form, SortForm::Runs) && d.keys.iter().all(|k| builtin(k.opno)) && d.tlist.iter().all(calm),
        _ => false,
    })
}

/// D3499's write: a plain INSERT, UPDATE or DELETE with no ON CONFLICT, MERGE, WITH CHECK OPTION or policy check and no
/// row moved to another partition, a calm RETURNING, and no BEFORE ROW, INSTEAD OF or statement trigger of the command
/// on a relation it writes (`written`): between two of its rows PostgreSQL runs no user code.
pub(crate) fn calm_write(gx: &Gx<'_, '_>, w: &WriteDesc<'_>) -> bool {
    use gravity_decompose::ops::modifytable::trig_facts::cmd_bits;
    let bits = cmd_bits(w.cmd);
    let quiet = |relid: u32| {
        let t = gx.facts.trig_ext(relid);
        t.known && (t.before_row | t.instead_row | t.statement) & bits == 0
    };
    let plain = matches!(w.cmd, Cmd::Insert | Cmd::Update | Cmd::Delete);
    // a partitioned root's UPDATE / DELETE that sets no partition key moves no row (`partColsUpdated` false)
    let stays = w.route.as_ref().is_none_or(|r| !r.cross_partition && w.cmd != Cmd::Insert);
    plain && w.on_conflict.is_none() && w.merge.is_none() && stays && w.checks.is_empty()
        && w.returning.is_none_or(|l| l.iter().all(|x| calm_in(gx, x)))
        && written(w, &mut |r| quiet(r))
}

/// `f` holds for every relation write `w` writes: its target, its partitions' targets and a routed root.
pub(crate) fn written(w: &WriteDesc<'_>, f: &mut dyn FnMut(u32) -> bool) -> bool {
    f(w.plan.target_rel) && w.targets.iter().all(|t| f(t.relid)) && w.route.as_ref().is_none_or(|r| f(r.root_relid))
}

/// A call in `n` that S2q's refill never runs ahead of PostgreSQL's pulls: a user or VOLATILE one the side class reads
/// (hashjoin.rs `side_calls`), or one it cannot see (`hidden`: a SubPlan, a user operator inside another node, a user
/// type's I/O, a domain's CHECK, ..).
pub(crate) fn refill_calls(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    super::hashjoin::side_calls(gx, n) || super::hashjoin::hidden(gx, n)
}
