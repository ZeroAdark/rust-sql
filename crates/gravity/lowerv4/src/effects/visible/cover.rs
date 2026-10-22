//! The effects home's cover (D2212): whether a pipeline the lowering runs ahead of PostgreSQL's order raises only where
//! PG's statement fails too, so that only a raise could show and the error-vs-error rule takes it (the statement fails
//! in both, whichever error comes first). That holds in a statement that neither acts (`PipelineGraph::acts`: a write,
//! a row mark, a VOLATILE call) nor calls user code (`effects::statement_user_code`), for a pipeline PG runs to its end
//! (or the statement fails), or one whose own source and ops are quiet over covered feeders. Asked by merge.join's FULL
//! merge inner (ops/mergejoin/full_whole.rs) and by the hash join's outer side run before a build PG makes first
//! (ops/hashjoin/probe.rs): Hibernate's json_mergepatch, whose seed FULL merges read the document unions' CTE stores
//! whole and whose term (a hash join building first on the working table) merges them again.
use std::cell::Cell;

use gravity_abi::breaker::BreakerKind;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::agg::GroupForm;
use gravity_decompose::ops::shared::setop::SetDesc;
use gravity_decompose::ops::shared::subplan::SubLink;
use gravity_decompose::{Op, Source};
use gravity_program::LifeId;
use types_nodes::jointype::JoinType;

use crate::effects::visible::merge::pipe_quiet;
use crate::graph::Gx;

/// Pipelines past this depth of readers or feeders are not walked: not whole, not covered.
const DEPTH: u8 = 24;

/// Pipeline `x`, run ahead of PG's order, raises only where PG's statement fails too (the file's head): `None` when not,
/// else `Some(at_root)`, `at_root` when the cover rests on the root's client reading every row (the caller then serves
/// the program to such a reader alone: `Px::whole_read`).
pub(crate) fn raises_covered(gx: &Gx<'_, '_>, arena: gravity_program::ArenaRef, x: usize) -> Option<bool> {
    if gx.graph.acts {
        return None;
    }
    let at_root = Cell::new(false);
    (covered(gx, x, 0, &at_root) && !crate::effects::statement_user_code(gx, arena)).then(|| at_root.get())
}

/// Pipeline `x` is in a rerun set (PG may run it again, or not at all, per call of its owner).
pub(crate) fn rerun(gx: &Gx<'_, '_>, x: usize) -> bool {
    gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(x as u16)))
}

/// Every raise pipeline `x`'s run makes, with what feeds it, PG's statement makes too or fails anyway: PG runs `x` to
/// its end (`whole`: x's whole computation is then PG's, however its feeders' runs depart in time), or `x`'s own source
/// and ops are quiet and each pipeline feeding it is covered (a recursive term's once-run inputs over CTE stores the
/// seed read whole).
fn covered(gx: &Gx<'_, '_>, x: usize, depth: u8, at_root: &Cell<bool>) -> bool {
    crate::stack_check();
    let ps = gx.pipelines();
    if depth > DEPTH || x >= ps.len() {
        return false;
    }
    if whole(gx, x, 0, depth, at_root) {
        return true;
    }
    // the slots a nest loop's inner reads too (`pipe_quiet`'s feeds); a slot with no producer found is not covered
    let mut feeds = mcx::PgVec::new_in(gx.mcx);
    pipe_quiet(gx, &ps[x], usize::MAX, &mut feeds)
        && ps[x].desc.consumes.iter().chain(feeds.iter()).all(|&s| {
            let f = crate::ops::mergejoin::lazy_outer::fillers(gx, x, s);
            !f.is_empty() && f.into_iter().all(|y| y != x && covered(gx, y, depth + 1, at_root))
        })
}

/// PG runs pipeline `x` to its end from op `from` on, or the statement fails: its ops from `from` read their input to its
/// end (filters, projections, a sorted group run, a recursive union's iterate, nest loops, FULL merges, which read both
/// sides whole, a last sort); no rerun set holds it but as a recursive term's body whose union runs to its end; and its rows go to the root's client (`at_root`), to an InitPlan of the root's one row whose sublink reads
/// to its end or raises at a second row (EXPR, ROWCOMPARE, MULTIEXPR; ARRAY to its end), or to a breaker a reader runs
/// whole (a FULL merge's inner too), or, one that reads its whole input at its first pull (a full sort, a top-N, a plain
/// or hashed aggregate), a reader PG surely pulls once (`opened`).
fn whole(gx: &Gx<'_, '_>, x: usize, from: usize, depth: u8, at_root: &Cell<bool>) -> bool {
    crate::stack_check();
    let ps = gx.pipelines();
    let root = gx.graph.root as usize;
    if depth > DEPTH || x >= ps.len() || root >= ps.len() {
        return false;
    }
    let p = &ps[x];
    let ops = p.ops.get(from..).unwrap_or(&[]);
    let body = match ops.split_last() {
        Some((Op::Sort(_), r)) => r,
        _ => ops,
    };
    let through = |op: &Op<'_>| matches!(op, Op::Filter(_) | Op::Project(_) | Op::GroupRun(_) | Op::SetOp(SetDesc::Iter(_)) | Op::Join(JoinDesc::Nest { .. } | JoinDesc::Merge { jointype: JoinType::JOIN_FULL, .. }));
    if !body.iter().all(through) {
        return false;
    }
    if rerun(gx, x) {
        // a recursive term's body: ExecRecursiveUnion reads the term to its end at every generation, all of them where the
        // union runs to its end
        let driver = (x + 1..ps.len()).find(|&d| ps[d].ops.iter().any(|op| matches!(op, Op::SetOp(SetDesc::Iter(it)) if it.body as usize == x)));
        return driver.is_some_and(|d| whole(gx, d, 0, depth + 1, at_root));
    }
    if x == root {
        at_root.set(true);
        return true;
    }
    if let Some(ip) = gx.graph.init_plans.iter().find(|i| i.pipeline as usize == x && !matches!(i.link, SubLink::Cte)) {
        return matches!(ip.link, SubLink::Expr | SubLink::RowCompare | SubLink::MultiExpr | SubLink::Array) && opened(gx, x, 0);
    }
    let Some(e) = crate::frame::iface::breaker_of(&p.desc) else { return false };
    let first = match e.kind {
        BreakerKind::Sort => crate::ops::mergejoin::lazy_outer::full(p),
        BreakerKind::TopK | BreakerKind::PlainAgg => true,
        BreakerKind::HashAgg => gx.group_at(x).is_some_and(|g| matches!(g.form, GroupForm::Hashed) && g.set.is_none()),
        BreakerKind::Materialize => false,
        _ => return false,
    };
    (x + 1..ps.len()).filter(|&d| ps[d].desc.consumes.contains(&e.slot) && gx.life_of(d, e.slot) == Some(LifeId(x as u16))).any(|d| {
        let inner = ps[d].ops.iter().position(|op| matches!(op, Op::Join(JoinDesc::Merge { slot, jointype: JoinType::JOIN_FULL, .. }) if *slot == e.slot));
        match ps[d].source {
            Source::Breaker { slot } if slot == e.slot => first && opened(gx, d, 0) || whole(gx, d, 0, depth + 1, at_root),
            _ => inner.is_some_and(|jd| whole(gx, d, jd, depth + 1, at_root)),
        }
    })
}

/// PG surely pulls pipeline `c`'s first row once the statement runs (`whole`'s first-pull readers): `c` is the root; or
/// the plan of an InitPlan (not a CTE's) whose parameter the root's one row reads (a Result over no input, its first
/// projection reading the parameter bare: ExecSetParamPlan pulls the plan at that read); or its rows pass filters and
/// projections only (a recursive union's driver its iterate op first: ExecRecursiveUnion pulls the seed first) into a
/// sort, a top-N, a fold or a Materialize (a CTE's store, a recursive union's working table) that a reader whose source
/// it is pulls, that reader `opened` (each breaker reads its input at its own first pull).
fn opened(gx: &Gx<'_, '_>, c: usize, depth: u8) -> bool {
    crate::stack_check();
    let ps = gx.pipelines();
    let root = gx.graph.root as usize;
    if depth > DEPTH || c >= ps.len() || root >= ps.len() {
        return false;
    }
    // no LIMIT, gate or probe between the source and the end that may end the pipeline before its source's first pull
    let plain = |ops: &[Op<'_>]| ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_)));
    if c == root {
        return plain(&ps[c].ops);
    }
    if let Some(ip) = gx.graph.init_plans.iter().find(|i| i.pipeline as usize == c && !matches!(i.link, SubLink::Cte)) {
        let r = &ps[root];
        let bare = |n: types_nodes::Node<'_>| n.as_target_entry().map_or(n, |t| t.expr).as_param().is_some_and(|p| p.paramid == ip.param);
        return matches!(r.source, Source::Const) && plain(&r.ops) && matches!(r.ops.first(), Some(Op::Project(l)) if l.iter().any(bare));
    }
    let ops = &ps[c].ops;
    let driver = matches!(ops.first(), Some(Op::SetOp(SetDesc::Iter(_))));
    let plain = plain(&ops[usize::from(driver)..]);
    let Some(e) = crate::frame::iface::breaker_of(&ps[c].desc).filter(|e| matches!(e.kind, BreakerKind::Sort | BreakerKind::TopK | BreakerKind::PlainAgg | BreakerKind::HashAgg | BreakerKind::Materialize)) else { return false };
    plain && (c + 1..ps.len()).any(|d| matches!(ps[d].source, Source::Breaker { slot } if slot == e.slot) && gx.life_of(d, e.slot) == Some(LifeId(c as u16)) && opened(gx, d, depth + 1))
}
