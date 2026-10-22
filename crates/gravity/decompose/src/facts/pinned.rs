//! Plan facts: a column pinned to one image in every row (D345 / G11), and the key-image facts it reads. Moved unchanged
//! out of decompose ops/agg/keys.rs (agg's sorted groups and group's pinned keys used it there; the window family reads
//! it too, and a family names no other family's ops/: lint RA): `pinned_word` and its walk (`pinned_in`, `keeps_outer`,
//! `inner_rows`, `drains_inner`, `producer`, `build_rows`, `sorted_from`, `scan_entry_at`), the image equalities
//! (now read through `Facts::op_image_eq`, catalog/keysem.rs `image_eq` / `image_eq_same`; the list is the key home's,
//! gravity_fn typesem `keysem::image`), `scan_attno` (now over the stripped node, all it read of
//! agg's `Stripped`) and `relabels`.
use gravity_abi::breaker::BreakerKind;
use types_nodes::primnodes::{ParamKind, INDEX_VAR, INNER_VAR, OUTER_VAR};
use types_nodes::Node;

use crate::catalog::keysem::image_eq;
use crate::graph::{BreakerDecl, KeyForm, Op, Source};
use crate::ops::shared::append::AppendDesc;
use crate::walk::{Builder, Open};

/// `e` through the relabels that keep its value (`expr::through_relabel`).
pub fn relabels<'p>(mut e: Node<'p>) -> Node<'p> {
    while let Some(a) = crate::expr::through_relabel(e) {
        e = a;
    }
    e
}

/// The scan column an expression is, when it is one: a `Var` of the pipeline's source (an index-only scan's through the index's columns).
pub fn scan_attno<'p>(b: &Builder<'p, '_>, child: &Open<'p>, e: Node<'p>) -> Option<i16> {
    let v = e.as_var().filter(|v| v.varlevelsup == 0 && v.varattno > 0)?;
    match child.source {
        Source::Index { indexid, only: true, .. } if v.varno == INDEX_VAR => b.facts.index(indexid)?.keys().get(v.varattno as usize - 1).copied(),
        _ => child.source.relation().filter(|(rti, _, _)| v.varno == *rti as i32).map(|_| v.varattno),
    }
}

/// The relation and column an expression of the open pipeline `child` is: its own scan's (`scan_attno`), or through the
/// full Sort PostgreSQL keeps under a DISTINCT aggregate (`sorted_from`) to the column of the scan that Sort reads.
pub fn scan_column<'p>(b: &Builder<'p, '_>, child: &Open<'p>, e: Node<'p>) -> Option<(u32, i16)> {
    if let Some(a) = scan_attno(b, child, e) {
        return child.source.relation().map(|(_, relid, _)| (relid, a));
    }
    let v = e.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0)?;
    let (p, _, c) = sorted_from(b, child, b.graph.pipelines.len(), v.varattno, false, false)?;
    let (n, _) = scan_entry_at(p, c)?;
    Some((p.source.relation()?.1, scan_attno(b, p, n)?))
}

/// D345 / D350 (V4R-144): input column `c`, read outside the aggregates of a GroupAggregate whose every key the planner
/// dropped, is pinned: the pipeline is its scan, filters, one-time gates and projections only (or the input of the Sort or spool
/// it reads, `sorted_from`), and a filter (or an index clause) is `column = Const / Param` under an equality whose equal
/// values are one image of the column's type (`image_eq`), or such an equality among a bitmap heap scan's recheck quals
/// (G11: every row it returns holds them). A plain `=` is strict (NULL never satisfies it), so every row
/// the node folds carries the one word PostgreSQL's first row does: keyed on it the group is the node's one group, and no
/// row over an empty input. G11: the scan's rows may pass through joins as their outer side (`keeps_outer`), and an
/// inner join's inner column is pinned where its inner rows pin it (`inner_rows`: a hash build's pipeline, a nested
/// loop's inner, whose rescans take a Const or a statement parameter only: its executor Params change per outer row,
/// a PARAM_EXTERN never, D3482's generic plan of `c.oid = $1`). IS NOT DISTINCT FROM, an OR, a
/// numeric / float / text or collated column, or an inner column of any other join is none (the node stays refused by
/// name).
pub fn pinned_word<'p>(b: &Builder<'p, '_>, child: &Open<'p>, c: i16) -> bool {
    pinned_in(b, child, b.graph.pipelines.len(), c, true, None)
}

/// D4074: pipeline `child` hands on no row: it holds a one-time gate that is never true (the planner's 'One-Time Filter:
/// false', ops/shared/gate.rs `never`) and every op from the gate on only hands rows on (filters, gates, projections,
/// SubPlan steps, a Sort, joins that keep their outer rows: `keeps_outer`). Never a union's pipeline or one of its arms
/// (an arm is one of several), nor a pipeline with a drain after the gate (a drain hands on build rows of its own). A
/// column of no row holds one value vacuously: agg keys.rs `pins_key` keys it where the kept-column end cannot serve.
pub fn no_row(child: &Open<'_>) -> bool {
    if child.union.is_some() || child.ops.iter().any(|op| matches!(op, Op::Append(_))) {
        return false;
    }
    let Some(i) = child.ops.iter().position(|op| matches!(op, Op::Gate(g) if crate::ops::shared::gate::never(*g))) else { return false };
    child.ops[i..].iter().all(|op| matches!(op, Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::Sort(_) | Op::SubPlan(_)) || keeps_outer(op))
}

/// `pinned_word` over pipeline `child` of index `at` in `graph.pipelines` (`read_decl`): a pushed one (the window
/// family's look through a WindowAgg below reads its fill's).
pub fn pinned_word_at<'p>(b: &Builder<'p, '_>, child: &Open<'p>, at: usize, c: i16) -> bool {
    pinned_in(b, child, at, c, true, None)
}

/// `pinned_word_at`, or the same walk where one more equality pins: `column <op> Const / Param` of operator `eq.0` under
/// input collation `eq.1` (a scan filter, an index clause or a bitmap recheck qual). A consumer whose own compare is that
/// equality (a WindowAgg's partition or order key: its `partOperators` / `ordOperators` and collations) finds every row a
/// peer on the column whatever its images (a text under a collation, a numeric's scales): the planner dropped the key's
/// pathkey (an equivalence class with a constant), so the input comes sorted without it (window_core-0281's
/// `symbol = 'NVDA'` under `ORDER BY symbol, trade_date`). A plain `=` is strict, so no NULL row passes it.
pub fn pinned_peer_at<'p>(b: &Builder<'p, '_>, child: &Open<'p>, at: usize, c: i16, eq: (u32, u32)) -> bool {
    pinned_in(b, child, at, c, true, Some(eq))
}

/// D3873: output column `c` of `child` holds one value on every row: through its projections (and the full Sort or spool
/// it reads, `sorted_from`) it is an expression over no column, Param, aggregate or sub-plan whose calls do not act and
/// return no set: a set operation's literal column under its implicit cast (`(2)::numeric`). PostgreSQL drops such a
/// column from a Sort's keys (its pathkey's equivalence class holds a constant, so the key is redundant), so a sorted
/// group over that Sort finds it in no order key and needs none.
pub fn one_value<'p>(b: &Builder<'p, '_>, child: &Open<'p>, c: i16) -> bool {
    one_value_at(b, child, b.graph.pipelines.len(), c)
}

fn one_value_at<'p>(b: &Builder<'p, '_>, child: &Open<'p>, at: usize, c: i16) -> bool {
    crate::walk::stack_check(); // D3748
    if let Some((p, at, c)) = sorted_from(b, child, at, c, false, false) {
        return one_value_at(b, p, at, c);
    }
    let Some((e, _)) = scan_entry_at(child, c) else { return false };
    let known = crate::expr::walk(e, &mut |x| match x.node_tag() {
        types_nodes::NodeTag::T_Const | types_nodes::NodeTag::T_OpExpr | types_nodes::NodeTag::T_RelabelType | types_nodes::NodeTag::T_CoerceViaIO => true,
        types_nodes::NodeTag::T_FuncExpr => !x.as_func_expr().unwrap().funcretset,
        _ => false,
    });
    known && !crate::effects::expr_acts(&crate::effects::Cx::new(b.stmt, b.facts, b.arena), e)
}

/// `pinned_in`'s `peer` operator that stands for the column type's own `=` (`pinned_bytes`): no operator has oid 0.
const OWN_BYTES_EQ: u32 = 0;

/// Column `c` of collation `coll` pinned by its type's own equality (`Facts::key_eq` `Own`, typesem `eq_form`) when equal
/// values are equal bytes: the type's image is its bytes untrimmed (typefacts `WordImage::Bytes`, text / varchar / name;
/// never bpchar's trailing-blank compare) and the collation is deterministic (`Facts::coll` `bytes_eq`: texteq's memcmp,
/// namecmp's strncmp or varstr_cmp tie-break). The walk is `pinned_peer_at`'s, so every row the consumer folds holds the
/// one image `pinned_word` answers for the word types. The planner drops such a column from the group keys (its
/// equivalence class holds the constant: `GROUP BY n.nspname .. WHERE nspname = 'public'`, pg_stat_xact_user_tables),
/// and a removed SubqueryScan leaves the node no ressortgroupref to key it by (agg `duplicate_keys`).
pub fn pinned_bytes<'p>(b: &Builder<'p, '_>, child: &Open<'p>, c: i16, coll: u32) -> bool {
    coll != 0 && b.facts.coll(coll).bytes_eq && pinned_in(b, child, b.graph.pipelines.len(), c, true, Some((OWN_BYTES_EQ, coll)))
}

/// The declaration of breaker `slot` that pipeline `at` reads (`at`: its index in `graph.pipelines`; an open pipeline's
/// is the length): the latest one a pipeline before it produced. A slot a pushed reader consumed is free again, and a
/// later pipeline may declare it anew (breakers.rs `free_slot`), so the latest declaration overall is not always the one
/// read. None for a union's arms (they declare the slot once each, in a row, as one breaker); a slot freed and declared
/// again by another kind of breaker (an InitPlan's aggregate before a window's buffer) is no arm.
pub fn read_decl<'a, 'p>(b: &'a Builder<'p, '_>, slot: u8, at: usize) -> Option<&'a BreakerDecl<'p>> {
    let br = &b.graph.breakers;
    let i = br.iter().rposition(|d| d.breaker.slot == slot && (d.producer as usize) < at)?;
    let arm = |j: usize| br[j].breaker.slot == slot && br[j].breaker.kind == br[i].breaker.kind && (br[j].producer as usize) < at;
    (!i.checked_sub(1).is_some_and(arm)).then(|| &br[i])
}

/// `pinned_word` over pipeline `child` (index `at`, as `read_decl`); `fixed`: a Param holds one value over every row the
/// pipeline runs (false in a nested loop's inner, rescanned per outer row with its NestLoopParams; a PARAM_EXTERN pins
/// either way); `peer`: the consumer's
/// own equality (operator, input collation) that pins too (`pinned_peer_at`).
fn pinned_in<'p>(b: &Builder<'p, '_>, child: &Open<'p>, at: usize, c: i16, fixed: bool, peer: Option<(u32, u32)>) -> bool {
    pin_in(b, child, at, c, fixed, peer).is_some()
}

/// `pinned_in`'s walk, answering with the value that pins (the `Const` / `Param` side of the equality): the union walk
/// (`union_pin`) asks each arm's.
fn pin_in<'p>(b: &Builder<'p, '_>, child: &Open<'p>, at: usize, c: i16, fixed: bool, peer: Option<(u32, u32)>) -> Option<Node<'p>> {
    crate::walk::stack_check(); // D3748
    if let Some((p, at, c)) = sorted_from(b, child, at, c, peer.is_some(), false) {
        return pin_in(b, p, at, c, fixed, peer);
    }
    // D3874: the image walk (`pinned_word`) reads a union's rows as the peer walk does: every arm pins the column to one
    // literal or one Param (a DISTINCT aggregate's presort over each partition's bitmap scan under `e_cust = 1211`), so
    // every row carries that value's one image; an open still over a union is pinned only so (never by one arm's walk)
    if let Some(v) = union_pin(b, child, at, c, fixed, peer) {
        return Some(v);
    }
    if peer.is_none() && child.union.is_some() {
        return None;
    }
    let build = |op: &Op<'_>| matches!(op, Op::Join(crate::join::JoinDesc::Build { .. }));
    // a union's arm (`union_pin`): its marker or its feed hands the arm's rows on unchanged
    let arm = |op: &Op<'_>| matches!(op, Op::Append(AppendDesc::Arm { .. } | AppendDesc::Feed { .. }));
    // a spool's fill (`sorted_from`) stores its rows unchanged
    let spool = |op: &Op<'_>| matches!(op, Op::Join(crate::join::JoinDesc::Spool));
    // D4074: a SubPlan step sets its parameters or its value for the row and hands the row on unchanged
    let sub = |op: &Op<'_>| matches!(op, Op::SubPlan(_));
    if !child.ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::Sort(_)) || keeps_outer(op) || build(op) || drains_inner(op) || arm(op) || spool(op) || sub(op)) {
        return None;
    }
    // D4074: the rows of a full Sort or a spool that `child` joins as their outer side (a Merge Join over its outer Sort)
    if let Some((p, at, c)) = sorted_from(b, child, at, c, peer.is_some(), true) {
        return pin_in(b, p, at, c, fixed, peer);
    }
    let (e, pos) = scan_entry_at(child, c)?;
    if let Some(v) = e.as_var().filter(|v| v.varno == INNER_VAR && v.varlevelsup == 0) {
        // the join's projection reads its inner row: the join op before it (past the join's own quals, Filter ops);
        // `pos` is that projection's place in `child.ops`, `at` stays the pipeline's index (`read_decl`)
        let join = child.ops[..pos].iter().rev().find(|op| !matches!(op, Op::Filter(_)));
        return join.and_then(|op| inner_rows(b, op, at)).and_then(|(p, at, fixed_p)| pin_in(b, p, at, v.varattno, fixed && fixed_p, peer));
    }
    let e = relabels(e);
    let (Some(attno), Some(v)) = (scan_attno(b, child, e), e.as_var()) else { return None };
    let ty = v.vartype;
    let value = |n: Node<'p>| {
        let n = relabels(n);
        // D3482: a statement's own parameter (PARAM_EXTERN: a prepared statement's generic plan, a SQL function's
        // argument) holds one value over the whole execution, a nested loop's inner rescans too; only an executor
        // Param (a NestLoopParam) changes per outer row
        n.as_const().is_some() || n.as_param().is_some_and(|p| fixed || p.paramkind == ParamKind::PARAM_EXTERN)
    };
    let col = |n: Node<'p>| relabels(n).as_var().is_some_and(|x| x.vartype == ty) && scan_attno(b, child, relabels(n)) == Some(attno);
    // the qual's value side when it pins: an image equality of the column's type, or the consumer's own (`peer`)
    let pins = |q: Node<'p>| -> Option<Node<'p>> {
        let o = q.as_op_expr().filter(|o| o.args.len() == 2)?;
        let (l, r) = (o.args.nth(0), o.args.nth(1));
        let image = image_eq(b.facts, o.opno);
        let own = peer.is_some_and(|(op, coll)| o.inputcollid == coll && (o.opno == op || (op == OWN_BYTES_EQ && crate::catalog::keysem::own_bytes_eq(b.facts, o.opno, ty))));
        if (own || image.is_some_and(|(lt, _)| lt == ty)) && col(l) && value(r) {
            Some(r)
        } else if (own || image.is_some_and(|(_, rt)| rt == ty)) && col(r) && value(l) {
            Some(l)
        } else {
            None
        }
    };
    let filter = child.ops.iter().find_map(|op| match op {
        Op::Filter(q) => pins(*q),
        _ => None,
    });
    // G11 (D345): a bitmap heap scan returns only rows its recheck condition (the index condition, PG's
    // bitmapqualorig, one implicitly ANDed list) holds for, lossy page or exact
    let index = match &child.source {
        // the consumer's own equality reads the index clauses in their original form (`peer`: their collation; an index
        // scan's indexqualorig, an index-only scan's recheckqual over the index's columns, `scan_attno`)
        Source::Index { keys, recheck, .. } => keys.iter().find(|key| key.attno == attno && key.form == KeyForm::Op && image_eq(b.facts, key.opno).is_some_and(|(lt, _)| lt == ty) && value(key.value)).map(|key| key.value).or_else(|| peer.and_then(|_| recheck.iter().find_map(pins))),
        Source::Bitmap { recheck, .. } => recheck.iter().find_map(pins),
        _ => None,
    };
    filter.or(index)
}

/// The peer and image walks (`pinned_peer_at`, `pinned_word`; D3874) over a union's rows: the value that pins column
/// `c` of pipeline `child` (index `at`, as `read_decl`) in every arm, when the arms' values are one (`same_pin`). Three
/// forms hand a union's rows on: `child` still over the union (its `union` holds the arms, append.rs `push_union` not
/// run yet: a WindowAgg straight over an Append, held-out rnd-0310), a Sort the union streamed into (`sorted_arms`: each
/// arm the marker and a copy of the Sort, rnd-2654's bitmap scan per partition under `e_cust = 2952`), and a settled
/// union's emit (`settled_arms`, each arm ending in its feed). PostgreSQL drops a key's pathkey over a union when its
/// equivalence class holds a constant, and each arm's quals then carry that one literal (adjust_appendrel_attrs copies
/// it, its location too); a pin from two tokens is no proof, whatever their values (no image is read: the front key
/// binds a literal per execution).
fn union_pin<'p>(b: &Builder<'p, '_>, child: &Open<'p>, at: usize, c: i16, fixed: bool, peer: Option<(u32, u32)>) -> Option<Node<'p>> {
    if let Some(u) = child.union {
        let arms = &b.graph.unions.get(u as usize)?.arms;
        let resno = row_before(&child.ops, c)?;
        let at = b.graph.pipelines.len();
        return one_pin(arms.iter().map(|a| pin_in(b, a, at, resno, fixed, peer)));
    }
    let pipe = |d: &BreakerDecl<'p>| b.graph.pipelines.get(d.producer as usize).map(|p| (p, d.producer as usize));
    if let Some((run, resno)) = sorted_arms(b, child, at, c) {
        let sorts = |(p, _): &(&Open<'p>, usize)| matches!(p.ops.last(), Some(Op::Sort(_)));
        return one_pin(run.iter().map(|d| pipe(d).filter(sorts).and_then(|(p, at)| pin_in(b, p, at, resno, fixed, peer))));
    }
    let arms = settled_arms(b, child, at)?;
    let resno = row_before(&child.ops[1..], c)?;
    one_pin(arms.map(|d| pipe(d).and_then(|(p, at)| pin_in(b, p, at, resno, fixed, peer))))
}

/// The one value of a union's arms' pins (`union_pin`): the first arm's, when every arm has one and each is the same
/// (`same_pin`); none for no arm.
fn one_pin<'p>(mut pins: impl Iterator<Item = Option<Node<'p>>>) -> Option<Node<'p>> {
    let first = pins.next()??;
    pins.all(|v| v.is_some_and(|v| same_pin(first, v))).then_some(first)
}

/// Two pins of one value (`union_pin`): two copies of one literal (a non-null Const of one type, typmod and collation
/// from one source token, its `location`; -1 names none), or one Param.
fn same_pin(a: Node<'_>, b: Node<'_>) -> bool {
    let (a, b) = (relabels(a), relabels(b));
    if let (Some(x), Some(y)) = (a.as_const(), b.as_const()) {
        return x.location >= 0 && x.location == y.location && (x.consttype, x.consttypmod, x.constcollid) == (y.consttype, y.consttypmod, y.constcollid) && !x.constisnull && !y.constisnull;
    }
    a.as_param().zip(b.as_param()).is_some_and(|(x, y)| x.paramkind == y.paramkind && x.paramid == y.paramid && x.paramtype == y.paramtype)
}

/// Column `c` of the row after `ops` as a column of the row before them: through each projection's or Sort's target list
/// (an `OUTER_VAR` entry), past filters and one-time gates; none at another op or entry.
fn row_before(ops: &[Op<'_>], c: i16) -> Option<i16> {
    let mut resno = c;
    for op in ops.iter().rev() {
        let l = match op {
            Op::Project(l) => *l,
            Op::Sort(s) => s.tlist,
            Op::Filter(_) | Op::Gate(_) => continue,
            _ => return None,
        };
        let t = l.nth((resno as usize).checked_sub(1).filter(|&n| n < l.len())?).as_target_entry().filter(|t| t.resno == resno)?;
        resno = t.expr.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0)?.varattno;
    }
    Some(resno)
}

/// A streamed union's arms under the Sort (or top-k) slot breaker-fed `child` (index `at`) reads: the run of that slot's
/// declarations of one kind, more than one, ending at the one `child` reads (a union's arms declare the slot once each,
/// in a row; an earlier fill of the slot by the same kind joins the run too, which only asks one more pipeline to pin),
/// and the Sort's column for `child`'s column `c`. A single declaration is `sorted_from`'s.
fn sorted_arms<'a, 'p>(b: &'a Builder<'p, '_>, child: &Open<'p>, at: usize, c: i16) -> Option<(&'a [BreakerDecl<'p>], i16)> {
    let Source::Breaker { slot } = child.source else { return None };
    let br = &b.graph.breakers;
    let i = br.iter().rposition(|d| d.breaker.slot == slot && (d.producer as usize) < at)?;
    let kind = br[i].breaker.kind;
    if !matches!(kind, BreakerKind::Sort | BreakerKind::TopK) {
        return None;
    }
    let mut j = i;
    while j > 0 && br[j - 1].breaker.slot == slot && br[j - 1].breaker.kind == kind {
        j -= 1;
    }
    (j < i).then_some(())?;
    Some((&br[j..=i], row_before(&child.ops, c)?))
}

/// A settled union's arms (append.rs `settle_arms`; breakers.rs `union_arms` over the declarations before pipeline `at`):
/// `emit` reads the slot and opens with `Union { slot, children }`, and the slot's last `children` declarations before
/// `at` are Materialize fills whose pipelines each end in the slot's `Feed`.
fn settled_arms<'a, 'p>(b: &'a Builder<'p, '_>, emit: &Open<'p>, at: usize) -> Option<impl Iterator<Item = &'a BreakerDecl<'p>> + 'a> {
    let Source::Breaker { slot } = emit.source else { return None };
    let Some(Op::Append(AppendDesc::Union { slot: s, children, .. })) = emit.ops.first() else { return None };
    let n = *children as usize;
    let decls = b.graph.breakers.iter().rev().filter(move |d| d.breaker.slot == slot && (d.producer as usize) < at).take(n);
    let feeds = |d: &BreakerDecl<'p>| d.breaker.kind == BreakerKind::Materialize && b.graph.pipelines.get(d.producer as usize).is_some_and(|p| matches!(p.ops.last(), Some(Op::Append(AppendDesc::Feed { slot: f, .. })) if *f == slot));
    (*s == slot && n > 0 && decls.clone().count() == n && decls.clone().all(feeds)).then_some(decls)
}

/// G11 (D345): a join the pipeline's own rows pass through as its outer side, never NULL-extended there: an inner, left,
/// semi or anti hash, merge or nested-loop join. Its projection reads an outer column as `OUTER_VAR` of the row before
/// it (`scan_entry`), so the column keeps the scan's value; an inner column is `INNER_VAR` (`inner_rows`). A right or
/// full join's drain hands on build rows whose outer columns are NULL, and a drain is its own op: neither is taken.
fn keeps_outer(op: &Op<'_>) -> bool {
    use crate::join::JoinDesc;
    use types_nodes::jointype::JoinType;
    let jt = match op {
        Op::Join(JoinDesc::Probe { jointype, .. } | JoinDesc::Merge { jointype, .. } | JoinDesc::Nest { jointype, .. }) => *jointype,
        _ => return false,
    };
    matches!(jt, JoinType::JOIN_INNER | JoinType::JOIN_LEFT | JoinType::JOIN_SEMI | JoinType::JOIN_ANTI)
}

/// G11 (D345): the pipeline whose rows an inner join hands on as its inner side, every output row one of them (JOIN_INNER):
/// a hash join's build (the producer of its slot that the join's pipeline `at` reads, ending in the Build) or a nested
/// loop's inner (false: rescanned per outer row, so its Params are not one value over the group). A nest in the hash
/// join's form (join.rs `map_catalog`: a catalog lookup, or a nest inner hashed by its join equality) probes such a build
/// too: its inner, which reads no parameter, filled once into it. A drain (`drains_inner`) hands on its build's rows too:
/// a right join's matched rows carry their build row and its unmatched build rows NULL-extend the outer side only, a
/// right semi / anti join's rows are build rows. D4074: an inner merge join's inner rows are its slot's producer's (the
/// inner Sort's or spool's pipeline, run once as a build is). With each pipeline its index (`read_decl`): a nested
/// loop's inner is part of the pipeline `at` that runs the join.
fn inner_rows<'a, 'p>(b: &'a Builder<'p, '_>, op: &'a Op<'p>, at: usize) -> Option<(&'a Open<'p>, usize, bool)> {
    use crate::join::JoinDesc;
    use types_nodes::jointype::JoinType;
    match op {
        Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, inner, .. }) => Some((&**inner, at, false)),
        Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_INNER, slot, .. }) => build_rows(b, *slot, at),
        Op::Join(JoinDesc::Merge { jointype: JoinType::JOIN_INNER, slot, .. }) => {
            let at = producer(b, *slot, at)?;
            b.graph.pipelines.get(at).map(|p| (p, at, true))
        }
        Op::Join(JoinDesc::Drain { matches, .. }) if drains_inner(op) => {
            // the probe pipeline that filled the drain's match flags, then the build its Probe read
            let probe = producer(b, *matches, at)?;
            let p = b.graph.pipelines.get(probe)?;
            let slot = p.ops.iter().rev().find_map(|op| match op {
                Op::Join(JoinDesc::Probe { slot, catalog: false, .. }) => Some(*slot),
                _ => None,
            })?;
            build_rows(b, slot, probe)
        }
        _ => None,
    }
}

/// G11 (D345): a hash join's drain whose every row holds a build row on its inner side: right, right semi, right anti
/// (a full join's unmatched probe rows NULL-extend the inner side).
fn drains_inner(op: &Op<'_>) -> bool {
    use types_nodes::jointype::JoinType;
    matches!(op, Op::Join(crate::join::JoinDesc::Drain { jointype: JoinType::JOIN_RIGHT | JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT_ANTI, .. }))
}

/// The pipeline whose fill of breaker `slot` pipeline `at` reads (`read_decl`: a slot freed by a pushed reader is
/// declared again by a later pipeline, so a slot can have several producers over the statement; none for a union's
/// arms).
fn producer(b: &Builder<'_, '_>, slot: u8, at: usize) -> Option<usize> {
    read_decl(b, slot, at).map(|d| d.producer as usize)
}

/// A hash build's pipeline (the producer of `slot` that pipeline `at` reads, ending in the Build): run once, so its
/// Params hold one value.
fn build_rows<'a, 'p>(b: &'a Builder<'p, '_>, slot: u8, at: usize) -> Option<(&'a Open<'p>, usize, bool)> {
    let at = producer(b, slot, at)?;
    let p = b.graph.pipelines.get(at)?;
    matches!(p.ops.last(), Some(Op::Join(crate::join::JoinDesc::Build { .. }))).then_some((p, at, true))
}

/// G11 (D345): the Sort's own pipeline and its output column for column `c` of breaker-fed `child`, when `child` only
/// projects the rows of a full Sort with one producer (the presort PostgreSQL keeps under a DISTINCT or ordered
/// aggregate, which sort/mod.rs closes as its own pipeline). A Sort moves rows and never changes a value, so a column
/// pinned in its input is pinned in its output. A union's arms declare the slot once each, in a row: none then. `top`
/// (`pinned_peer_at`'s walk): a top-k sort too (a row-count run condition's Sort under a WindowAgg, window_core-0107's
/// TopK): it hands on some of its input rows, unchanged. A spool (a Materialize breaker whose fill ends in the `Spool`:
/// a Material node's, join.rs `map_material`, or nest.selfspool's over a nest's plain inner, self_spool.rs `wrap`) hands
/// on every row of its fill, unchanged, each rescan the same rows (a fill reading a nest's Params is rescanned under
/// them, and `fixed` stays the caller's). `past` (D4074): `child` may also filter those rows, set a SubPlan's value
/// beside them, sort them on, or join them as the outer side of joins that hand each outer row on unchanged
/// (`keeps_outer`: a Merge Join over the Sort's rows), the column read from the outer row at every step.
fn sorted_from<'a, 'p>(b: &'a Builder<'p, '_>, child: &Open<'p>, at: usize, c: i16, top: bool, past: bool) -> Option<(&'a Open<'p>, usize, i16)> {
    let Source::Breaker { slot } = child.source else { return None };
    // the declaration `child` reads: the latest a pipeline before it produced (`read_decl`'s rule; the arm test as before)
    let i = b.graph.breakers.iter().rposition(|d| d.breaker.slot == slot && (d.producer as usize) < at)?;
    let d = &b.graph.breakers[i];
    let kind = d.breaker.kind;
    if !(kind == BreakerKind::Sort || (top && kind == BreakerKind::TopK) || kind == BreakerKind::Materialize) || i.checked_sub(1).is_some_and(|j| b.graph.breakers[j].breaker.slot == slot) {
        return None;
    }
    // BreakerKind::Sort: a full sort (a top is TopK); a Materialize: the spool's fill only (a settled union's arms end in
    // their Feed, `settled_arms`)
    let ends = |p: &&Open<'p>| match p.ops.last() {
        Some(Op::Sort(_)) => kind != BreakerKind::Materialize,
        Some(Op::Join(crate::join::JoinDesc::Spool)) => kind == BreakerKind::Materialize,
        _ => false,
    };
    let p = b.graph.pipelines.get(d.producer as usize).filter(ends)?;
    let mut resno = c;
    for op in child.ops.iter().rev() {
        let l = match op {
            Op::Project(l) => *l,
            Op::Sort(s) if past => s.tlist,
            Op::Filter(_) | Op::Gate(_) | Op::SubPlan(_) if past => continue,
            op if past && keeps_outer(op) => continue,
            _ => return None,
        };
        let t = l.nth((resno as usize).checked_sub(1).filter(|&n| n < l.len())?).as_target_entry().filter(|t| t.resno == resno)?;
        resno = t.expr.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0)?.varattno;
    }
    Some((p, d.producer as usize, resno))
}

/// Output column `c` of `child` through its projections down to the scan's (agg keys.rs `scan_entry`), and the index in
/// `child.ops` of the op whose target list holds it (G11: a join's projection's
/// `INNER_VAR` names the join op before it).
pub fn scan_entry_at<'p>(child: &Open<'p>, c: i16) -> Option<(Node<'p>, usize)> {
    let mut tlists = child.ops.iter().enumerate().rev().filter_map(|(i, op)| match op {
        Op::Project(l) => Some((*l, i)),
        Op::Sort(s) => Some((s.tlist, i)),
        _ => None,
    });
    let mut resno = c;
    loop {
        let (tlist, i) = tlists.next()?;
        let t = tlist.nth((resno as usize).checked_sub(1).filter(|&n| n < tlist.len())?).as_target_entry().filter(|t| t.resno == resno)?;
        match t.expr.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0) {
            Some(v) => resno = v.varattno,
            None => return Some((t.expr, i)),
        }
    }
}
