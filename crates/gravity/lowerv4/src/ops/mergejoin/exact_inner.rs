//! merge.join's pulled inner (the exact request, PLAN-v5 §2.5; jmerge-0012: the request's second consumer after window
//! W4's fill). PostgreSQL's ExecMergeJoin fetches its inner one tuple at a time (ExecProcNode at INITIALIZE_INNER,
//! SKIPINNER_ADVANCE, NEXTINNER) and fetches none once the join ends, so an inner scan whose filter acts (a VOLATILE or
//! user function) runs it on exactly the rows the merge reached. decompose's merge asks Exact of such an inner
//! (decompose ops/mergejoin.rs); here the inner's spool becomes a pulled set (`MERGE_PULLED`: deferred, run by exec's
//! merge.markable object one row per pull, exec ops/mergejoin/proto.rs) whose run the merge reads while it fills
//! (runs.sort's `pulled_end` / `pulled_reader`, face/slot_run.rs): a fetch past the stored rows pulls (`m11_pull`) and
//! opens the run again before the machine decides the inner ended (the shared pulled-run reader, face/pull.rs). Mark and
//! restore stay run indexes, exact where PG's restore re-evaluates nothing, so the first form asks for one of: a Material
//! inner (PG's restore reads its tuplestore), a GroupAggregate inner streamed over the scan (PG never restores it: no Material over it means the merge
//! skips mark and restore), or an outer whose merge keys a unique index proves distinct (PG never restores: TESTOUTER's new
//! outer is above the mark); an Index Scan inner PG restores re-fetches from the mark and runs its filter again. Also in the
//! first form: INNER / LEFT / SEMI / ANTI (no inner fill), a quiet outer, an inner that is a relation scan with filters
//! and projections only, every inner column the merge reads a by-value word (a fetched row's cells hold until the next
//! pull), outside every rerun set. Any other shape keeps D1236's rules (lower.rs). Owner M11-MERGE.
//!
//! MJ-AHEAD (the budgeted pull, as window W10's fill): an inner not asked Exact whose code is calm (no user, VOLATILE or
//! NOTICE-raising call: it may raise) under PG's Material, or PG's scan itself, or a sorted group streamed over the scan
//! (mergejoin r106: PG's GroupAggregate fetches its feed a group at a time), is pulled too (`Form::Grows`), as PG's Material
//! fetches its scan one tuple per merge fetch: under a LIMIT the whole inner was read before the first outer row (79,122
//! rows for a LIMIT 5 merge PG answers from 4). Each pull stores up to the merge's want (face/pull.rs `W_WANT`: the stored
//! rows plus a budget doubling from one row to `AHEAD_STEP`, then growing by an eighth of the stored rows, `W_AHEAD`), so
//! a merge that stops reads at most PG's rows plus the larger of the two, and a whole read pays a pull count logarithmic
//! in its rows; an Exact inner's budget stays one row. A pull that raises after storing the row the merge wanted holds
//! its error for the next pull (`m11_hold`, exec ops/mergejoin/raw.rs), as PG raises it only when the merge fetches the
//! row that raised. A budgeted pull's rows carry by-reference inner columns only to a step that
//! keeps copies (a streamed group) or to an end that copies each row's images as it is handed on: the client's sink or a
//! row store (`words`).
use gravity_abi::breaker::BreakerKind;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::{Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntCond, Value};
use mcx::{CollectIn, PgVec};
use types_nodes::jointype::JoinType;
use types_nodes::list::NodeList;
use types_nodes::primnodes::INNER_VAR;

use crate::face::kinds::Face;
use crate::face::pull::{Form, PullWords};
use crate::frame::env::Env;
use crate::frame::open::OpenWords;
use crate::frame::px::Px;
use crate::graph::Gx;
use crate::ops::mergejoin::lower::Merge;

/// The merge `(k, j)` reading inner pipeline `inner`'s run.
fn merge_of(gx: &Gx<'_, '_>, inner: usize) -> Option<(usize, usize)> {
    let reads = |k: usize, op: &Op<'_>| matches!(op, Op::Join(JoinDesc::Merge { slot, .. }) if gx.life_of(k, *slot).is_some_and(|l| l.0 as usize == inner));
    gx.pipelines().iter().enumerate().find_map(|(k, p)| p.ops.iter().position(|op| reads(k, op)).map(|j| (k, j)))
}

/// Whether inner pipeline `inner` is a merge's pulled inner (asked Exact, in the first form; or calm, MJ-AHEAD): its spool's
/// end stops after the rows a pull wants and its reader reads the run as it fills (face/slot_run.rs), the merge pulls it
/// (`Merge::ended`).
pub(crate) fn pulled(px: &Px<'_, '_>, inner: usize) -> bool {
    form(px, inner).is_some()
}

/// Inner pipeline `inner`'s pull form, when pulled. Both: INNER / LEFT / SEMI / ANTI, a relation scan with filters and
/// projections only, by-value read columns (`words`), outside every rerun set. Exact: asked Exact, a quiet outer, a Material, a
/// group inner node or a distinct outer (PG's restore re-runs nothing). Grows: not asked Exact (or asked where the Exact form
/// does not reach, D2846), its code calm (`calm`: a restore that re-ran it
/// would change nothing), under PG's Material or PG's scan itself or a sorted group streamed over the scan as the merge's
/// inner node (so never a Sort's run), filled by one pipeline (no
/// union arms), the merge feeding no InitPlan or CTE set (`unnested`).
pub(crate) fn form(px: &Px<'_, '_>, inner: usize) -> Option<Form> {
    let gx = px.gx;
    let (k, j) = merge_of(gx, inner)?;
    let Some(Op::Join(JoinDesc::Merge { slot, jointype, clauses, collations, inner_node, .. })) = gx.pipelines()[k].ops.get(j) else { return None };
    let p = &gx.pipelines()[inner];
    let exact = px.request(*inner_node).delivery == crate::frame::iface::Delivery::Exact;
    let kinds = matches!(*jointype, JoinType::JOIN_INNER | JoinType::JOIN_LEFT | JoinType::JOIN_SEMI | JoinType::JOIN_ANTI);
    let scan = matches!(p.source, Source::Range { .. } | Source::Index { .. } | Source::Bitmap { .. }) && p.desc.consumes.is_empty();
    // the sorted group the merge reads as its inner node, streamed over the scan (decompose facts/stream.rs `merge_inner`,
    // D1071: PG's GroupAggregate reads one feed row past each group a fetch takes, that row's arguments run with the next)
    let group = |op: &Op<'_>| matches!(op, Op::GroupRun(i) if gx.graph.streams.get(*i as usize).is_some_and(|g| g.node_id == *inner_node));
    let plain = p.ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_) | Op::Join(JoinDesc::Spool)) || group(op));
    let once = !gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(k as u16)) || r.pipelines.contains(&(inner as u16)));
    let material = p.ops.iter().any(|op| matches!(op, Op::Join(JoinDesc::Spool)));
    let quiet = !crate::effects::visible::merge::may_act_but(gx, k, j, Some(*slot));
    if !(kinds && scan && plain && once) {
        return None;
    }
    // a group inner node: PG adds no Material over its GroupAggregate (no mark / restore) only where the merge never
    // restores (the path's skip_mark_restore), so no restore re-runs it
    let grouped = p.ops.iter().any(group);
    if exact && quiet && words(px, (k, j), inner, false) && (material || grouped || distinct_outer(gx, (k, j), clauses, collations)) {
        return Some(Form::Exact);
    }
    // D2846: an Exact ask the first form does not reach (a by-reference column the merge reads, a loud outer, a restore
    // that re-runs the scan) takes the budgeted form where its own rules hold, as an inner not asked does: it runs the
    // outer in PG's order and reads ahead only calm inner code, its raises held (any outer); an inner asked Exact because
    // its code acts is never calm, so it keeps the spool
    if !words(px, (k, j), inner, true) {
        return None;
    }
    // PG's Material over the scan, or the scan itself (the merge's inner node is the pipeline's source node: PG marks and
    // restores the index scan, its restore re-running calm code to the same rows), or the group streamed over it; never a
    // Sort's run
    let own = material || grouped || p.node_ops.first().is_some_and(|n| n.node_id == *inner_node);
    (own && gx.run_arms(inner).is_empty() && crate::face::pull::unnested(gx, k) && crate::effects::visible::pull::calm(px, inner)).then(Form::grows).flatten()
}

/// Whether the merge reading inner pipeline `inner`'s run reads no by-reference cell of it (its clauses, join qual,
/// filters and target list read by-value inner columns only, `words`): its reader's view need not point the run's images
/// (face/slot_run.rs `view_table`).
pub(crate) fn lean_view(px: &Px<'_, '_>, inner: usize) -> bool {
    merge_of(px.gx, inner).is_some_and(|kj| words(px, kj, inner, false))
}

/// Every column of the inner's run the merge reads (its clauses' and join qual's inner side, the join's filters and target
/// list: INNER_VAR) a by-value word: a fetched row's cells hold only until the next pull (the run may move), so no cell
/// pointing into it leaves the merge's row. A column no one reads is never loaded (D1521). MJ-AHEAD (`escaping`, the
/// budgeted form): only the target list's columns, the ones a row handed on carries past a later pull; the clauses, the
/// join qual and the filters read the row each state fetched after its pull (merge/machine.rs: every state fetches again).
/// A streamed group right after the merge's projection (mj-0161: GROUP BY over a merge to a numeric-sum group) ends a
/// by-reference column there: it folds the row it is handed before the merge fetches again, and its record keeps copies,
/// never a cell of its input (ops/group/stream.rs: key copies, a min / max's kept text; the finals read the record). So
/// does the end the pipeline reaches through filters, projections, a LIMIT and a LockRows, when it copies the row's images
/// as the row is handed on: the client's sink, or a Sort, TopK, Materialize or Hash build breaker's row store. PG's merge
/// under a LIMIT, or over an outer that ends early, reads only the inner rows it reaches, where a whole spool read all.
/// D3373 (lockrows_refmerge): a LockRows reads a REFERENCE mark's junk ctid (a by-reference tid) while it handles the row
/// and keeps it by value (the lock site's position, the in-IR recheck's refetch at it), then hands the row on or skips it
/// before the merge pulls again.
fn words(px: &Px<'_, '_>, (k, j): (usize, usize), inner: usize, escaping: bool) -> bool {
    let mcx = px.mcx();
    let (p, q) =(&px.gx.pipelines()[k], &px.gx.pipelines()[inner]);
    let Some(cols) = q.ops.iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None }) else { return false };
    let Some(Op::Join(JoinDesc::Merge { clauses, qual, .. })) = p.ops.get(j) else { return false };
    let upto = p.ops[j + 1..].iter().position(|op| matches!(op, Op::Project(_))).map_or(p.ops.len(), |i| j + 2 + i);
    if !p.ops[j + 1..upto].iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_))) {
        return false;
    }
    let mut after = p.ops[j + 1..upto].iter().filter_map(|op| match op { Op::Filter(n) if !escaping => Some(mcx::pg_vec![in mcx; *n]), Op::Project(l) => Some(l.iter().collect_in(mcx)), _ => None }).flatten();
    let byval = |a: i16| (a > 0).then(|| cols.iter().nth(a as usize - 1)).flatten().and_then(|n| n.as_target_entry()).is_some_and(|te| {
        let typ = gravity_decompose::catalog::expr_type::type_of(te.expr).unwrap_or_else(|| gravity_decompose::catalog::expr_type::result_type(te.expr));
        gravity_decompose::catalog::typefacts::facts_of(px.gx.facts, typ).is_some_and(|tf| tf.byval)
    });
    let folds =escaping && matches!(p.ops.get(upto), Some(Op::GroupRun(_)));
    // the client's sink or a row store the pipeline ends in copies each row's by-reference images as the row is handed on
    // (end/sink.rs; kernels sort rowsort.rs `point_images` points a run's cells at its copies), before the merge fetches
    // again; a LIMIT WITH TIES keeps its boundary row, so it is not among the steps between
    let tail = &p.ops[upto..];
    // the breaker's own fill op ends the pipeline (a Sort's or top-k's keys, a Material spool, a hash build)
    let fill = |op: &Op<'_>| matches!(op, Op::Sort(_) | Op::Join(JoinDesc::Spool | JoinDesc::Build { .. }));
    let body = match tail.split_last() {
        Some((last, rest)) if fill(last) => rest,
        _ => tail,
    };
    let limit = |op: &Op<'_>| crate::frame::iface::limit_bounds(op).is_some() && !crate::frame::iface::limit_with_ties(op);
    let between = body.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_) | Op::Lock(_)) || limit(op));
    let copies = match crate::frame::iface::end_breaker(px.gx, k) {
        None => crate::frame::iface::ends_in_sink(px.gx, k),
        Some(kind) => matches!(kind, BreakerKind::Sort | BreakerKind::TopK | BreakerKind::Materialize | BreakerKind::HashBuild),
    };
    let stored = escaping && between && copies;
    let ok = |n: types_nodes::Node<'_>| gravity_decompose::expr::walk(n, &mut |x| x.as_var().is_none_or(|v| v.varno != INNER_VAR || byval(v.varattno)));
    folds || stored || if escaping { after.all(ok) } else { clauses.iter().chain(qual.iter()).chain(after).all(ok) }
}

/// PG never restores the inner: the outer (an index scan through filters and projections) reads a unique btree index
/// whose every key column is a merge clause's outer key, under no collation, so no two outer rows tie on the merge keys.
fn distinct_outer<'p>(gx: &Gx<'_, 'p>, (k, j): (usize, usize), clauses: &NodeList<'p>, collations: &[u32]) -> bool {
    let p = &gx.pipelines()[k];
    let Source::Index { indexid, only, .. } = &p.source else { return false };
    let Some(idx) = gx.facts.index(*indexid).filter(|i| i.unique && i.btree && i.nkeys > 0) else { return false };
    if !p.ops[..j].iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_))) || collations.iter().any(|&c| c != 0) {
        return false;
    }
    // an index-only scan's Vars name index columns (INDEX_VAR), a heap index scan's the relation's
    let att = |a: i16| if *only { (a as usize).checked_sub(1).and_then(|i| idx.keys().get(i)).copied() } else { Some(a) };
    let keys: PgVec<'_, i16> = clauses.iter().filter_map(|c| c.as_op_expr()).filter_map(|o| crate::props::code::scan_attno(p.ops[..j].iter(), o.args.nth(0))).filter_map(att).collect_in(gx.mcx);
    idx.keys().iter().all(|a| *a != 0 && keys.contains(a))
}

/// Merge `(k, j)`'s inner `inner`, when pulled: the shared reader's pull of it (face/pull.rs `declare`) in its form.
pub(crate) fn declare(px: &mut Px<'_, '_>, (k, j): (usize, usize), inner: usize) -> Option<PullWords> {
    let form = form(px, inner)?;
    Some(crate::face::pull::declare(px, (Face::Merge, k, j), inner, form))
}

impl Merge<'_> {
    /// A pulled inner's stored rows and want words, 0 at the merge's open, and its budget, 1 row.
    pub(crate) fn pulled_words(&self, w: &mut OpenWords) {
        if let Some(pw) = self.pull {
            pw.open_words(w, ["merge.join pulled inner rows", "merge.join pulled inner want", "merge.join pulled inner budget"]);
        }
    }

    /// At each window: a pulled inner's rows as its run's open counted them.
    pub(crate) fn keep(&self, b: &mut FunctionBuilder<'_>, env: &Env, n: Value) {
        if let Some(pw) = self.pull {
            pw.keep(b, env, n);
        }
    }

    /// Whether inner row `ip` is past the run (`n` rows at the window); a pulled inner is pulled first (face/pull.rs
    /// `PullWords::ended`).
    pub(crate) fn ended(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, ip: Value, n: Value) -> Value {
        match self.pull {
            Some(pw) => pw.ended(b, env, &*self.run, ip),
            None => b.icmp(IntCond::Sge, ip, n),
        }
    }
}
