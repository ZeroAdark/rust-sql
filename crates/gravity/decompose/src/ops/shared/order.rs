//! The order an open's rows come in (index, range, union, breaker sources and the ops above them), and the checks a
//! family makes against a wanted order. Read by sort, window, agg, append, join and the graph.
use gravity_program::{ArenaVec, SiteDesc};
use types_nodes::jointype::JoinType;
use types_nodes::list::NodeList;
use types_nodes::primnodes::{INDEX_VAR, INNER_VAR, OUTER_VAR};
use types_nodes::Node;

use crate::admit::Refusal;
use crate::breakers;
use crate::catalog::{Am, Facts, KeyReq};
use crate::expr;
use crate::graph::{Col, KeyForm, Op, Order, OrderKey, PipelineGraph, Source, MAX_ORDER_EQS, MAX_ORDER_KEYS};
use crate::join::JoinDesc;
use crate::ledger::Kind;
use crate::ops::shared::setop::SetDesc;
use crate::walk::{Builder, Open};

/// D858: the btree family of ordering operator `opno` when its input type re-forms the word of the key's column type `typ`
/// (`expr::reforms_word`): an int4 column sorted by oid's `<`, as PG sorts a merge join's side whose clause relabels it
/// (prepare_sort_from_pathkeys matches the tlist column through the relabel). PG's comparator reads the column's word as
/// the operator's type (btoidcmp: `DatumGetObjectId`, unsigned), so the key orders by that family's comparator alone
/// (`SortKey::own_family`, the CompareFn key kind) and its order is declared under that family, never the column type's
/// default one. None for any other key. A source that knows the operator but not its input type (a capture's `-- op` line)
/// answers the bootstrap catalog's for a builtin operator, whose input type is fixed.
pub fn reformed_family(facts: &dyn Facts, opno: u32, typ: Option<u32>) -> Option<u32> {
    let o = facts.key(KeyReq::sort(opno)).filter(|o| o.opcintype != 0).or_else(|| crate::catalog::opdir::builtin_key(opno))?;
    // D3630: a domain column reads as its base (a domain over oid sorted by int4's `<` re-forms as oid does)
    let base = |t: u32| crate::catalog::typefacts::facts_of(facts, t).map_or(t, |f| f.base);
    typ.is_some_and(|t| expr::reforms_word(base(t), base(o.opcintype))).then_some(o.family)
}

/// The type of target-list entry `resno` (1-based); none outside the list or for an entry of unknown type.
pub fn key_type(tlist: &NodeList<'_>, resno: i16) -> Option<u32> {
    let e = (resno as usize).checked_sub(1).filter(|&i| i < tlist.len()).and_then(|i| tlist.nth(i).as_target_entry())?;
    crate::catalog::expr_type::type_of(e.expr)
}

/// The order an index-driven pipeline's rows come in: the index's key columns (an index-only scan's by position,
/// an index scan's by attribute — an expression column by its fingerprint, matched at the projection that
/// computes it) without the equality-probed ones, each in its declared direction (`indoption`: DESC / NULLS
/// FIRST), all flipped by a backward scan. The order is cut at an expression column whose expression the facts
/// do not carry.
fn index_order(b: &Builder<'_, '_>, indexid: u32, probes: &[crate::graph::IndexKey<'_>], only: bool, scanrelid: u16, backward: bool) -> Order {
    let Some(idx) = b.facts.index(indexid) else { return Order::NONE };
    let mut o = Order::NONE;
    if probes.iter().any(|p| p.form == KeyForm::OrderBy) {
        // a nearest-neighbour walk (amcanorderbyop): the rows come by each `column <op> value` ascending, nulls last
        for p in probes.iter().filter(|p| p.form == KeyForm::OrderBy) {
            let (varno, attno) = if only { (INDEX_VAR, p.col as i16) } else { (scanrelid as i32, p.attno) };
            let Some(fp) = b.facts.att(idx.relid, p.attno).filter(|_| p.attno > 0).and_then(|a| crate::catalog::op_expr_fp(p.opno, varno, attno, a.typid, a.typmod, p.value)) else { break };
            o.push_expr(OrderKey { col: Col { varno, attno: 0 }, desc: false, nulls_first: false }, fp);
        }
        return o;
    }
    if !idx.btree {
        return Order::NONE;
    }
    for (i, &col) in idx.keys().iter().enumerate() {
        if col != 0 && probes.iter().any(|p| p.attno == col && p.form == KeyForm::Op && b.facts.op_eq(p.opno)) {
            continue;
        }
        let key = |c| OrderKey { col: c, desc: idx.desc(i) != backward, nulls_first: idx.nulls_first(i) != backward };
        // D2313: a key column of another btree family (text_pattern_ops) orders by that family, never by its type's default
        let fam = b.facts.index_family(indexid, i as u8);
        if only {
            o.push_fam(key(Col { varno: INDEX_VAR, attno: i as i16 + 1 }), fam);
        } else if col != 0 {
            o.push_fam(key(Col { varno: scanrelid as i32, attno: col }), fam);
        } else if fam != 0 {
            break;
        } else {
            let Some(fp) = b.facts.index_expr(indexid, i as u8) else { break };
            o.push_expr(key(Col { varno: scanrelid as i32, attno: 0 }), fp);
        }
    }
    o
}

/// D2061: the keys of a btree index scan's order past the `MAX_ORDER_KEYS` an `Order` holds, in the open's columns: the
/// index's key columns (`indnkeyatts`: never an INCLUDE column) as `index_order` reads them (the equality-probed ones
/// skipped, each in its declared direction, all flipped by a backward scan), moved through the open's projections and
/// equality filters as `step` moves the first ones, eight at a time (a key a projection drops ends the order there). The
/// keys end at the first one of another btree family's order or an expression column. `None` unless the open is the scan
/// under projections and filters only, its first eight keys are plain columns of their types' default families (an
/// order led by another family's key is no default-family order) and keys remain past them.
pub(crate) fn index_past<'p>(b: &Builder<'p, '_>, p: &Open<'p>) -> Option<ArenaVec<OrderKey>> {
    let Source::Index { indexid, keys: probes, only, scanrelid, backward, .. } = &p.source else { return None };
    if p.union.is_some() || !p.ops.iter().all(|op| matches!(op, Op::Project(_) | Op::Filter(_))) || probes.iter().any(|k| k.form == KeyForm::OrderBy) {
        return None;
    }
    let idx = b.facts.index(*indexid).filter(|i| i.btree)?;
    let ext = b.facts.index_ext(*indexid);
    if !ext.known {
        return None;
    }
    let nkey = (ext.nkeyatts as usize).min(idx.keys().len());
    let mut all = ArenaVec::with_capacity_in(nkey, b.arena);
    for (i, &col) in idx.keys()[..nkey].iter().enumerate() {
        if col != 0 && probes.iter().any(|k| k.attno == col && k.form == KeyForm::Op && b.facts.op_eq(k.opno)) {
            continue;
        }
        let c = if *only { Col { varno: INDEX_VAR, attno: i as i16 + 1 } } else { Col { varno: *scanrelid as i32, attno: col } };
        if c.attno == 0 || b.facts.index_family(*indexid, i as u8) != 0 {
            if all.len() < MAX_ORDER_KEYS {
                return None;
            }
            break;
        }
        all.push(OrderKey { col: c, desc: idx.desc(i) != *backward, nulls_first: idx.nulls_first(i) != *backward });
    }
    if all.len() <= MAX_ORDER_KEYS {
        return None;
    }
    let mut out = ArenaVec::with_capacity_in(all.len() - MAX_ORDER_KEYS, b.arena);
    for chunk in all[MAX_ORDER_KEYS..].chunks(MAX_ORDER_KEYS) {
        let mut c = Order::NONE;
        for key in chunk {
            c.push(*key);
        }
        for op in p.ops.iter() {
            c = step(b.facts, &b.graph, c, op);
        }
        out.extend(c.keys().iter().copied());
        if c.n as usize != chunk.len() {
            break;
        }
    }
    Some(out)
}

const fn keeps_outer_order(jt: JoinType) -> bool {
    matches!(jt, JoinType::JOIN_INNER | JoinType::JOIN_LEFT | JoinType::JOIN_SEMI | JoinType::JOIN_ANTI)
}

/// `a = b` over two columns with a btree equality (`Facts::op_eq`): the pair.
fn eq_cols(facts: &dyn Facts, e: Node<'_>) -> Option<(Col, Col)> {
    let op = e.as_op_expr().filter(|o| o.args.len() == 2 && facts.op_eq(o.opno))?;
    Some((Col::of(op.args.nth(0))?, Col::of(op.args.nth(1))?))
}

/// D4666: the column a filter `col = constant` pins on every row it passes: a btree equality (`Facts::op_eq`, as
/// `eq_cols`) between a bare column and a non-NULL constant, over a type without a collation (a nondeterministic
/// collation's equality joins values its order tells apart).
fn pinned_col(facts: &dyn Facts, e: Node<'_>) -> Option<Col> {
    let op = e.as_op_expr().filter(|o| o.args.len() == 2 && o.inputcollid == 0 && facts.op_eq(o.opno))?;
    let (x, y) = (op.args.nth(0), op.args.nth(1));
    let konst = |n: Node<'_>| n.as_const().is_some_and(|c| !c.constisnull);
    match (Col::of(x), Col::of(y)) {
        (Some(c), None) if konst(y) => Some(c),
        (None, Some(c)) if konst(x) => Some(c),
        _ => None,
    }
}

/// The order after an op: a projection remaps, a sort sets its keys, a merge / nest keeps the outer's (an inner or
/// semi merge adds its clauses' equalities), a hash probe keeps only equalities (an inner or semi one adds its
/// clauses'), a RIGHT join's drain keeps its build rows' equalities, a build, identity set, union feed or iteration
/// loses it, a filter adds an equality, a union keeps its slot's declared order, everything else keeps it. D3993: an
/// inner or semi merge or probe and an inner nest add their join quals' equalities too (`join_eqs`): a join qual holds
/// on every row the join hands on, as a filter's does.
fn step<'p>(facts: &dyn Facts, g: &PipelineGraph<'p>, o: Order, op: &Op<'p>) -> Order {
    match op {
        Op::Project(tlist) => o.project(tlist),
        Op::Filter(e) => match eq_cols(facts, *e) {
            Some((a, b)) => {
                let mut n = o;
                n.push_eq(a, b);
                n
            }
            // D4666: a filter pinning a column to a constant leaves every row tied on it (`Order::pinned`)
            None => pinned_col(facts, *e).map_or(o, |c| o.pinned(c)),
        },
        Op::Sort(s) => {
            let mut n = o.unordered();
            // D1346: a key of another family's order is no order a default-family consumer matches (its keys compare by
            // type); record-image order (matview#113): it is declared under its family (`Order::fams`), which only a
            // consumer asking that family matches (a merge join by record_image_ops), and ends the order when unknown
            for key in s.keys.iter() {
                // D858: a key whose operator re-forms its column's word is declared under the operator's family
                let reformed = || reformed_family(facts, key.opno, key_type(s.tlist, key.resno));
                let fam = if key.own_family { facts.key(KeyReq::sort(key.opno)).filter(|o| !o.default_family).map(|o| o.family).or_else(reformed) } else { Some(0) };
                let Some(fam) = fam else { break };
                n.push_fam(OrderKey { col: Col::outer(key.resno), desc: key.desc, nulls_first: key.nulls_first }, fam);
            }
            n
        }
        Op::Join(JoinDesc::Merge { slot, jointype, clauses, qual, .. }) => {
            if !keeps_outer_order(*jointype) {
                return Order::NONE;
            }
            let mut n = o;
            if matches!(jointype, JoinType::JOIN_INNER | JoinType::JOIN_SEMI) {
                for c in clauses.iter() {
                    if let Some((a, b)) = eq_cols(facts, c) {
                        n.push_eq(a, b);
                    }
                }
                join_eqs(facts, &mut n, qual);
            }
            // D3856: and the inner run's own (its Sort's or spool's declared order)
            if *jointype == JoinType::JOIN_INNER {
                inner_eqs(g, *slot, &mut n);
            }
            n
        }
        Op::Join(JoinDesc::Nest { jointype, params, inner, qual, .. }) => {
            if !keeps_outer_order(*jointype) {
                return Order::NONE;
            }
            let mut n = o;
            if *jointype == JoinType::JOIN_INNER {
                nest_eqs(facts, g, &mut n, params, inner);
                join_eqs(facts, &mut n, qual);
            }
            n
        }
        // a probe's rows come in no order but keep their outer row's equalities; an inner or semi probe's rows also meet
        // its hash clauses (a RIGHT / FULL probe hands on build rows NULL-extended, so none of the outer's hold). A catalog
        // probe (join.rs map_catalog) keeps the outer's order too: serial, at most one inner row per outer row, never the
        // spill pair (rnd-2838)
        Op::Join(JoinDesc::Probe { slot, jointype, clauses, qual, catalog, .. }) if keeps_outer_order(*jointype) => {
            let mut n = if *catalog { o } else { o.unordered() };
            if matches!(jointype, JoinType::JOIN_INNER | JoinType::JOIN_SEMI) {
                for c in clauses.iter() {
                    if let Some((a, b)) = eq_cols(facts, c) {
                        n.push_eq(a, b);
                    }
                }
                join_eqs(facts, &mut n, qual);
            }
            // D3856: and the build rows' own (join.rs map_hash_join declares them on the HashBuild)
            if *jointype == JoinType::JOIN_INNER && !*catalog {
                inner_eqs(g, *slot, &mut n);
            }
            n
        }
        // D3906: a RIGHT join's drain hands on every build row, matched or NULL-extended on the probe side: the equalities
        // every build row meets (its source's, the HashBuild's declared ones in its rows' `OUTER_VAR` columns) hold, in the
        // join's `INNER_VAR` ones; the probe side's and the hash clauses hold for the matched rows only
        Op::Join(JoinDesc::Drain { jointype: JoinType::JOIN_RIGHT, .. }) => {
            let mut n = Order::NONE;
            for [a, b] in &o.eqs[..o.neq as usize] {
                if a.varno == OUTER_VAR && b.varno == OUTER_VAR {
                    n.push_eq(Col { varno: INNER_VAR, attno: a.attno }, Col { varno: INNER_VAR, attno: b.attno });
                }
            }
            n
        }
        Op::Join(JoinDesc::Build { .. } | JoinDesc::Probe { .. } | JoinDesc::Drain { .. }) => Order::NONE,
        Op::SetOp(SetDesc::Set { hashed: false, .. }) => o,
        // a union's rows come in feed order across the children: the slot's declared order (an ordered Append's
        // mark checked on every child) or none; a feed is the end of its pipeline
        Op::SetOp(_) | Op::Append(crate::ops::shared::append::AppendDesc::Feed { .. } | crate::ops::shared::append::AppendDesc::Arm { .. }) => Order::NONE,
        _ => o,
    }
}

/// D3856: an inner join's rows each carry an inner row, so the equalities every inner row meets hold too: those slot
/// `slot`'s declaration names in its rows' columns (`OUTER_VAR`), which the join reads as its `INNER_VAR` ones.
fn inner_eqs(g: &PipelineGraph<'_>, slot: u8, n: &mut Order) {
    let Some(d) = breakers::decl_of(g, slot) else { return };
    for [a, b] in &d.order.eqs[..d.order.neq as usize] {
        if a.varno == OUTER_VAR && b.varno == OUTER_VAR {
            n.push_eq(Col { varno: INNER_VAR, attno: a.attno }, Col { varno: INNER_VAR, attno: b.attno });
        }
    }
}

/// D3993: the equalities of a join's own quals (`joinqual`, kept in its op, not a Filter op after it), each conjunct a
/// btree equality of two columns (`eq_cols`, as a filter's): the join hands on only rows they hold on, so an inner
/// column the qual equates with an outer order key is that key (rnd-0838: a sorted group keyed by `o.o_prio`, a nested
/// loop's inner column, over a merge join's order on `t.t_grp` under the nest's qual `o.o_prio = t.t_grp`). Only a join
/// that never NULL-extends a side its qual reads: the caller's inner or semi join.
fn join_eqs(facts: &dyn Facts, o: &mut Order, qual: &NodeList<'_>) {
    for q in qual.iter() {
        if let Some((a, b)) = eq_cols(facts, q) {
            o.push_eq(a, b);
        }
    }
}

/// An inner nest's equalities: the inner scan probed `column = $p` by a btree equality (`Facts::op_eq`), `$p` the nest's
/// parameter bound to an outer column, joins only rows where that column (as the inner's projection names it,
/// `INNER_VAR`) equals the outer one (`OUTER_VAR`) — the same equality a merge join's clause adds (A14 e: a sorted
/// group keyed by the inner's column over the outer's order). An index scan's keys; a bitmap scan's recheck conjuncts;
/// an Append inner (a settled union, `breakers::union_arms`; ExecReScanAppend rescans every child with the parameter):
/// a column of the union's row, which the Append's projection names by position (`OUTER_VAR`), when every arm's scan
/// probes the column at that position by the same parameter (MJ-NOORDER: a partitioned inner).
fn nest_eqs<'p>(facts: &dyn Facts, g: &PipelineGraph<'p>, o: &mut Order, params: &NodeList<'_>, inner: &Open<'p>) {
    if projection(inner).is_none() {
        return;
    }
    let bound = |v: Node<'_>| -> Option<Col> {
        let mut v = v;
        while let Some(a) = expr::through_relabel(v) {
            v = a;
        }
        let p = v.as_param().filter(|p| p.paramkind == types_nodes::primnodes::ParamKind::PARAM_EXEC)?;
        let np = params.iter().filter_map(|x| x.as_nest_loop_param()).find(|x| x.paramno == p.paramid)?;
        Col::of(np.paramval).filter(|c| c.varno == OUTER_VAR)
    };
    let mut pairs: [(Col, Col); MAX_ORDER_EQS] = [(Col::outer(0), Col::outer(0)); MAX_ORDER_EQS];
    let n = match breakers::union_arms(g, inner) {
        None => scan_eqs(facts, &bound, inner, &mut pairs),
        Some(arms) => {
            let mut n = 0;
            for (k, arm) in arms.enumerate() {
                let mut own: [(Col, Col); MAX_ORDER_EQS] = [(Col::outer(0), Col::outer(0)); MAX_ORDER_EQS];
                let m = scan_eqs(facts, &bound, arm, &mut own);
                let Some(at) = projection(arm) else { return };
                // the arm's scan column as the union's row names it: its position in the arm's projection
                let pos = |c: Col| at.iter().position(|e| e.as_target_entry().and_then(|t| Col::of(t.expr)) == Some(c)).map(|i| Col::outer(i as i16 + 1));
                let mut mine: [(Col, Col); MAX_ORDER_EQS] = [(Col::outer(0), Col::outer(0)); MAX_ORDER_EQS];
                let mut j = 0;
                for &(col, outer) in &own[..m] {
                    if let Some(c) = pos(col) {
                        mine[j] = (c, outer);
                        j += 1;
                    }
                }
                if k == 0 {
                    pairs[..j].copy_from_slice(&mine[..j]);
                    n = j;
                } else {
                    // only an equality every arm holds is the union's
                    let mut kept = 0;
                    for i in 0..n {
                        if mine[..j].contains(&pairs[i]) {
                            pairs[kept] = pairs[i];
                            kept += 1;
                        }
                    }
                    n = kept;
                }
                if n == 0 {
                    return;
                }
            }
            n
        }
    };
    for &(col, outer) in &pairs[..n] {
        if let Some(attno) = inner_position(inner, col) {
            o.push_eq(outer, Col { varno: INNER_VAR, attno });
        }
    }
}

/// Column `c` of `inner`'s source as the nest's inner rows name it (their position, from 1): through each projection
/// `inner` runs in turn, so a Memoize's over its scan's carries the scan's parameter equality (D3376: a sorted group keyed
/// by the inner column over an outer index's order, group_stop_any_value); `None` when a projection drops it.
fn inner_position(inner: &Open<'_>, c: Col) -> Option<i16> {
    let (mut c, mut at) = (c, None);
    for op in inner.ops.iter() {
        let Op::Project(l) = op else { continue };
        let i = l.iter().position(|e| e.as_target_entry().and_then(|t| Col::of(t.expr)) == Some(c))? as i16 + 1;
        (c, at) = (Col::outer(i), Some(i));
    }
    at
}

/// A scan's parameter equalities (`nest_eqs`): (the scanned column in the scan's coordinates, the outer column its
/// parameter is bound to), into `out`; the count.
fn scan_eqs(facts: &dyn Facts, bound: &dyn Fn(Node<'_>) -> Option<Col>, scan: &Open<'_>, out: &mut [(Col, Col); MAX_ORDER_EQS]) -> usize {
    let mut n = 0;
    let mut add = |col: Col, outer: Option<Col>| {
        if let Some(outer) = outer.filter(|_| n < MAX_ORDER_EQS) {
            out[n] = (col, outer);
            n += 1;
        }
    };
    match &scan.source {
        Source::Index { keys, only, scanrelid, .. } => {
            for k in keys.iter().filter(|k| k.form == KeyForm::Op && facts.op_eq(k.opno)) {
                let col = if *only { Col { varno: INDEX_VAR, attno: k.col as i16 } } else { Col { varno: *scanrelid as i32, attno: k.attno } };
                if col.attno > 0 {
                    add(col, bound(k.value));
                }
            }
        }
        Source::Bitmap { recheck, scanrelid, .. } => {
            for c in recheck.iter() {
                let Some(op) = c.as_op_expr().filter(|x| x.args.len() == 2 && facts.op_eq(x.opno)) else { continue };
                let (x, y) = (op.args.nth(0), op.args.nth(1));
                for (v, other) in [(x, y), (y, x)] {
                    if let Some(col) = Col::of(v).filter(|c| c.varno == *scanrelid as i32) {
                        add(col, bound(other));
                    }
                }
            }
        }
        _ => {}
    }
    n
}

/// The order a pgrc range scan's rows come in, ASC NULLS LAST lexicographic (the planner's pathkeys for the bare scan):
/// a merged site's key (D4469 road 2: its parts' sorted streams merged, at any part count), else the key the AM vouches
/// for over the whole relation (`RelFact.sorted_by`: the declared `sorted_by` over at most one part, a fact only a capture
/// from before road 1 carries); a heap scan and any other pgrc scan have none.
fn range_order(b: &Builder<'_, '_>, am: Am, site: &SiteDesc, relid: u32, scanrelid: u16) -> Order {
    let mut o = Order::NONE;
    if am != Am::Pgrcolumnar {
        return o;
    }
    let rel = b.facts.rel(relid);
    let key = if site.is_merged() { &site.merge[..] } else { rel.as_ref().map_or(&[][..], |r| r.sorted_by()) };
    for &attno in key {
        o.push(OrderKey { col: Col { varno: scanrelid as i32, attno }, desc: false, nulls_first: false });
    }
    o
}

/// The order an open pipeline's rows come in: its source's (an index's columns, a sorted pgrc table's key, a
/// breaker's declared order — a recursion's work table has none) through its ops.
pub fn order_of<'p>(b: &Builder<'p, '_>, p: &Open<'p>) -> Order {
    // an open over a streamed union: the union's order (the ordered mark, the arms run in child order) or none
    let mut o = match &p.source {
        _ if p.union.is_some() => b.graph.unions[p.union.unwrap() as usize].order,
        Source::Index { indexid, keys, only, scanrelid, backward, .. } => index_order(b, *indexid, keys, *only, *scanrelid, *backward),
        Source::Range { site, am, relid, scanrelid } => range_order(b, *am, site, *relid, *scanrelid),
        Source::Breaker { slot } if !b.work.iter().any(|w| w.1 == *slot) => breakers::decl_of(&b.graph, *slot).map_or(Order::NONE, |d| d.order),
        // a FROM function scan WITH ORDINALITY: its ordinality column ascending (window_core-0120)
        Source::Const => crate::facts::srf::ordinality_order(p.ops.first()),
        _ => Order::NONE,
    };
    for op in p.ops.iter() {
        o = step(b.facts, &b.graph, o, op);
    }
    o
}

/// D3856: the column equalities every row of `p` meets, in its projection's columns (`OUTER_VAR`), no keys; none when
/// its last op is no projection.
pub fn eqs_of<'p>(b: &Builder<'p, '_>, p: &Open<'p>) -> Order {
    if !matches!(p.ops.last(), Some(Op::Project(_))) {
        return Order::NONE;
    }
    let o = order_of(b, p);
    let mut out = Order::NONE;
    for [a, c] in &o.eqs[..o.neq as usize] {
        if a.varno == OUTER_VAR && c.varno == OUTER_VAR {
            out.push_eq(*a, *c);
        }
    }
    out
}

/// The last projection of an open pipeline (its `OUTER_VAR` coordinates).
pub fn projection<'p>(p: &Open<'p>) -> Option<&'p NodeList<'p>> {
    p.ops.iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None })
}

pub fn is_const(proj: Option<&NodeList<'_>>, c: Col) -> bool {
    c.varno == OUTER_VAR && proj.and_then(|l| (c.attno as usize).checked_sub(1).filter(|&i| i < l.len()).map(|i| l.nth(i))).and_then(|e| e.as_target_entry()).is_some_and(|t| t.expr.as_const().is_some())
}

/// Whether `p`'s rows lead with `want` (a column, direction and null placement each): a wanted key that is a
/// constant of the projection, repeats an earlier wanted key or equals an order key already passed needs no key of
/// its own; the rest consume the order's keys in sequence.
pub fn leads_with(o: &Order, proj: Option<&NodeList<'_>>, want: &[OrderKey], dirs: bool) -> bool {
    leads_with_fams(o, proj, want, &[], dirs)
}

/// `leads_with` with each wanted key's btree family (`fams[i]`, 0 or absent: its type's default): an order key matches
/// only a wanted key of its own family (record-image order: a key of another family's order never stands for a
/// default-family one, nor the reverse).
pub fn leads_with_fams(o: &Order, proj: Option<&NodeList<'_>>, want: &[OrderKey], fams: &[u32], dirs: bool) -> bool {
    let fam = |i: usize| fams.get(i).copied().unwrap_or(0);
    // the default family's equalities (`Order::eqs`) carry its order only; another family's key matches its own column
    let same = |f: u32, a: Col, b: Col| if f == 0 { o.same(a, b) } else { a == b };
    let mut j = 0;
    for (i, w) in want.iter().enumerate() {
        let passed = o.keys()[..j].iter().enumerate().any(|(m, x)| o.fams[m] == fam(i) && same(fam(i), x.col, w.col));
        if is_const(proj, w.col) || want[..i].iter().enumerate().any(|(m, x)| fam(m) == fam(i) && same(fam(i), x.col, w.col)) || passed {
            continue;
        }
        match o.keys().get(j) {
            Some(x) if o.fams[j] == fam(i) && same(fam(i), x.col, w.col) && (!dirs || (x.desc == w.desc && x.nulls_first == w.nulls_first)) => j += 1,
            _ => return false,
        }
    }
    true
}

/// The refusal for a consumer whose input is not in `want`'s order: none known, or a different one.
pub fn ordered_as(o: &Order, proj: Option<&NodeList<'_>>, want: &[OrderKey], k: Kind, id: i32, none: &'static str, other: &'static str) -> Result<(), Refusal> {
    if leads_with(o, proj, want, true) {
        return Ok(());
    }
    Err(Refusal::shape(k, id, if o.is_known() { other } else { none }))
}

/// `ordered_as` for a wanted order whose keys name their btree families (`Order::fams`: a merge join's clauses).
pub fn ordered_as_order(o: &Order, proj: Option<&NodeList<'_>>, want: &Order, k: Kind, id: i32, none: &'static str, other: &'static str) -> Result<(), Refusal> {
    if leads_with_fams(o, proj, want.keys(), &want.fams[..want.n as usize], true) {
        return Ok(());
    }
    Err(Refusal::shape(k, id, if o.is_known() { other } else { none }))
}
