//! AGB-28 / AGC-27 / AGB-22: the one "at most N rows" fact. A plan subtree's row bound per (re)scan, read from the plan
//! and the catalog: a scan whose quals bind every key column of a UNIQUE, IMMEDIATE, VALID index by the column's own
//! equality (`Facts::unique_bound`), a VALUES list, a constant LIMIT, a plain aggregate, a child-less Result, the nodes
//! that pass rows through, and joins, appends and the bitmap trees over bounded children. Every unknown is no bound.
//! A deferrable unique index can hold two equal keys inside a transaction, a partial one bounds only its own rows, a
//! NULL key matches no `=`, and an IS NULL, a range, an `= ANY` or a row comparison binds nothing: none of these proves
//! a bound on its own (each only removes rows beside a full-key bind).
use types_nodes::jointype::JoinType;
use types_nodes::nodes_enums::LimitOption;
use types_nodes::plannodes::PlannedStmt;
use types_nodes::{Node, NodeList, NodeTag};

use gravity_program::limits::KEYS;

use super::{Facts, KeyBind};
use crate::graph::Bound;

/// At most how many rows `node` returns each time it is run (a rescan counts again); none when no bound is proven.
pub fn max_rows<'p>(facts: &dyn Facts, stmt: &PlannedStmt<'_>, node: Node<'p>) -> Option<u64> {
    crate::walk::stack_check(); // D3748
    let p = node.as_plan()?;
    let child = |c: Option<Node<'p>>| c.and_then(|c| max_rows(facts, stmt, c));
    match node.node_tag() {
        NodeTag::T_Result => match p.lefttree {
            None => Some(1),
            Some(c) => max_rows(facts, stmt, c),
        },
        NodeTag::T_Agg => {
            let a = node.as_agg()?;
            if !a.groupingSets.is_nil() {
                return None;
            }
            if a.aggstrategy == crate::ops::shared::agg::AGG_PLAIN { Some(1) } else { child(p.lefttree) }
        }
        NodeTag::T_Limit => {
            let l = node.as_limit()?;
            let below = child(p.lefttree);
            match crate::ops::limit_bounds(l).0 {
                Bound::Const(c) if c >= 0 && l.limitOption != LimitOption::LIMIT_OPTION_WITH_TIES => Some(below.map_or(c as u64, |m| m.min(c as u64))),
                _ => below,
            }
        }
        NodeTag::T_Sort | NodeTag::T_IncrementalSort | NodeTag::T_Material | NodeTag::T_Memoize | NodeTag::T_Unique | NodeTag::T_Group | NodeTag::T_LockRows | NodeTag::T_WindowAgg | NodeTag::T_Hash => child(p.lefttree),
        NodeTag::T_SubqueryScan => child(node.as_subquery_scan()?.subplan),
        NodeTag::T_Append => sum(facts, stmt, &node.as_append()?.appendplans),
        NodeTag::T_MergeAppend => sum(facts, stmt, &node.as_merge_append()?.mergeplans),
        NodeTag::T_NestLoop | NodeTag::T_HashJoin | NodeTag::T_MergeJoin => join(node, child(p.lefttree), child(p.righttree)),
        NodeTag::T_ValuesScan => Some(node.as_values_scan()?.values_lists.len() as u64),
        NodeTag::T_IndexScan | NodeTag::T_IndexOnlyScan | NodeTag::T_BitmapHeapScan => match probe_rows(facts, node) {
            Some(m) if m <= 1 => Some(m),
            probe => min(probe, scan_quals(facts, stmt, node)),
        },
        NodeTag::T_SeqScan => scan_quals(facts, stmt, node),
        _ => None,
    }
}

/// At most how many rows an index probe reads from its relation, before any filter: an Index Scan or Index Only Scan, or
/// a Bitmap Heap Scan over its bitmap tree, whose index quals bind every key column of a unique, immediate, valid index
/// by the column's equality (a partial index too: its scan reads only the index's own rows). A NULL search key matches
/// nothing, so the key's value side is any expression the scan evaluates once per (re)scan.
pub fn probe_rows(facts: &dyn Facts, node: Node<'_>) -> Option<u64> {
    if let Some(s) = node.as_index_scan() {
        return point(facts, s.indexid, &s.indexqual);
    }
    if let Some(s) = node.as_index_only_scan() {
        return point(facts, s.indexid, &s.indexqual);
    }
    if node.as_bitmap_heap_scan().is_some() {
        return bitmap(facts, node.as_plan()?.lefttree?);
    }
    None
}

fn bitmap(facts: &dyn Facts, n: Node<'_>) -> Option<u64> {
    crate::walk::stack_check(); // D3748
    if let Some(b) = n.as_bitmap_index_scan() {
        return point(facts, b.indexid, &b.indexqual);
    }
    if let Some(o) = n.as_bitmap_or() {
        return o.bitmapplans.iter().try_fold(0u64, |s, c| bitmap(facts, c).map(|m| s.saturating_add(m)));
    }
    if let Some(a) = n.as_bitmap_and() {
        return a.bitmapplans.iter().filter_map(|c| bitmap(facts, c)).min();
    }
    None
}

/// The index quals of a probe: each `index column = value` conjunct (the planner puts the index column on the left) is a
/// bind; the rest only narrow the scan.
fn point(facts: &dyn Facts, indexid: u32, quals: &NodeList<'_>) -> Option<u64> {
    let mut binds = [KeyBind { col: 0, opno: 0, collid: 0, nullable: false }; KEYS];
    let mut n = 0;
    for c in quals.iter() {
        let Some(op) = c.as_op_expr().filter(|op| op.args.len() == 2) else { continue };
        let Some(col) = strip(op.args.nth(0)).as_var().and_then(|v| u8::try_from(v.varattno).ok()).and_then(|a| a.checked_sub(1)) else { continue };
        if n < binds.len() {
            binds[n] = KeyBind { col, opno: op.opno, collid: op.inputcollid, nullable: false };
            n += 1;
        }
    }
    (n > 0 && facts.unique_bound(indexid, &binds[..n], true)).then_some(1)
}

/// A scan's own quals (its filter, and an Index Scan's or a Bitmap Heap Scan's original index quals) binding every key
/// column of some unique, immediate, valid, non-partial index of the scanned relation: each `column = value` conjunct
/// whose value is a Const or a Param, the same for every row the scan reads (a value that may be NULL needs a strict
/// operator: the filter calls it).
fn scan_quals<'p>(facts: &dyn Facts, stmt: &PlannedStmt<'_>, node: Node<'p>) -> Option<u64> {
    let p = node.as_plan()?;
    let (scanrelid, orig) = if let Some(s) = node.as_seq_scan() {
        (s.scan.scanrelid, None)
    } else if let Some(s) = node.as_index_scan() {
        (s.scan.scanrelid, Some(&s.indexqualorig))
    } else if let Some(s) = node.as_bitmap_heap_scan() {
        (s.scan.scanrelid, Some(&s.bitmapqualorig))
    } else {
        return None;
    };
    let relid = (scanrelid as usize).checked_sub(1).filter(|&i| i < stmt.rtable.len()).and_then(|i| stmt.rtable.nth(i).as_range_tbl_entry()).map(|r| r.relid)?;
    const NONE: KeyBind = KeyBind { col: 0, opno: 0, collid: 0, nullable: false };
    let mut eqs = [(0i16, NONE); 2 * KEYS];
    let mut n = 0;
    for c in p.qual.iter().chain(orig.into_iter().flat_map(|l| l.iter())) {
        let Some(op) = c.as_op_expr().filter(|op| op.args.len() == 2) else { continue };
        let (l, r) = (strip(op.args.nth(0)), strip(op.args.nth(1)));
        let side = |var: Node<'p>, val: Node<'p>| {
            let v = var.as_var().filter(|v| v.varno as u32 == scanrelid && v.varlevelsup == 0 && v.varattno > 0)?;
            let nullable = match (val.as_const(), val.as_param()) {
                (Some(k), _) if !k.constisnull => false,
                (None, Some(_)) => true,
                _ => return None,
            };
            Some((v.varattno, KeyBind { col: 0, opno: op.opno, collid: op.inputcollid, nullable }))
        };
        if let Some(e) = side(l, r).or_else(|| side(r, l)).filter(|_| n < eqs.len()) {
            eqs[n] = e;
            n += 1;
        }
    }
    if n == 0 {
        return None;
    }
    let eqs = &eqs[..n];
    let mut found = false;
    facts.indexes_of(relid, &mut |i| {
        if found || !i.unique || !i.immediate || i.partial {
            return;
        }
        let mut binds = [NONE; KEYS];
        let mut m = 0;
        for (k, &att) in i.keys().iter().enumerate().filter(|&(_, &att)| att > 0) {
            for &(_, b) in eqs.iter().filter(|(a, _)| *a == att) {
                if m < binds.len() {
                    binds[m] = KeyBind { col: k as u8, ..b };
                    m += 1;
                }
            }
        }
        found = m > 0 && facts.unique_bound(i.indexid, &binds[..m], false);
    });
    found.then_some(1)
}

fn sum(facts: &dyn Facts, stmt: &PlannedStmt<'_>, l: &NodeList<'_>) -> Option<u64> {
    l.iter().try_fold(0u64, |s, c| max_rows(facts, stmt, c).map(|m| s.saturating_add(m)))
}

fn min(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// A join's rows from its children's: an inner join at most the product, an outer row kept unmatched once more, a
/// semi / anti join each outer row once; `inner_unique` (PostgreSQL's own proof, on which its executor stops at the first
/// match) at most one row per outer row. A full join is no bound.
fn join(node: Node<'_>, outer: Option<u64>, inner: Option<u64>) -> Option<u64> {
    let j = node.as_nest_loop().map(|n| &n.join).or(node.as_hash_join().map(|h| &h.join)).or(node.as_merge_join().map(|m| &m.join))?;
    let (o, i) = (outer, inner);
    match j.jointype {
        JoinType::JOIN_SEMI | JoinType::JOIN_ANTI => o,
        JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT_ANTI => i,
        JoinType::JOIN_INNER | JoinType::JOIN_LEFT if j.inner_unique => o,
        JoinType::JOIN_INNER => Some(o?.saturating_mul(i?)),
        JoinType::JOIN_LEFT => Some(o?.saturating_mul(i?.max(1))),
        JoinType::JOIN_RIGHT => Some(i?.saturating_mul(o?.max(1))),
        _ => None,
    }
}

fn strip(n: Node<'_>) -> Node<'_> {
    n.as_relabel_type().map_or(n, |r| r.arg)
}
