//! D1214 (V4R-393): which input columns an Agg's group keys determine, as PostgreSQL's check_functional_grouping admits
//! an ungrouped column (`pk_determined`: a base relation's primary key; `unique_determined`: a unique index of it over
//! NOT NULL GROUP BY columns), and a column's syntactic identity and equality up the pipeline (`syn_col`,
//! `equal_cols`). The passenger rewrite (D514, V4R-167) and the walks to a column's readers above moved to
//! facts::demand::readers.
use types_nodes::list::NodeList;
use types_nodes::parsenodes::RTEKind;
use types_nodes::primnodes::{Param, ParamKind};
use types_nodes::Node;

use crate::graph::{Op, Source};
use crate::walk::Builder;

/// D1214 (V4R-393): whether the Agg's group keys `keys` determine `e` by a PRIMARY KEY, as PostgreSQL's
/// check_functional_grouping admits an ungrouped column: `e` is a bare `Var` of a base relation's column (the range
/// table entry its `varnosyn` names is a relation, never a join alias, subquery or function) and every column of that
/// relation's non-deferrable primary key (`Facts::primary_key`: pg_constraint contype 'p', conkey) is a key that is a
/// bare `Var` of the same entry. A UNIQUE constraint or index never admits (PostgreSQL uses the primary key alone).
/// Over the grouping-sets union (planner groupingsets_union.rs) each arm is its own Agg grouped by its set: the parser
/// admitted the column only when the key is in every set (gset_common), so it is in each arm's keys, and an arm whose
/// keys do not hold the whole key admits nothing. The key's rows share one value of every column of the relation (a
/// NULL-extended row's too: all NULL), so the column joins the group's keys: the same groups, a sorted input's runs the
/// same, its word the value PostgreSQL emits from the group's representative row.
pub fn pk_determined<'p>(b: &Builder<'p, '_>, keys: &[Node<'p>], e: Node<'p>) -> bool {
    let base = |n: Node<'p>| n.as_var().filter(|v| v.varlevelsup == 0 && v.varnosyn > 0 && v.varattnosyn > 0).map(|v| (v.varnosyn, v.varattnosyn));
    let Some((rti, _)) = base(e) else { return false };
    let Some(rte) = b.rte(rti).filter(|r| r.rtekind == RTEKind::RTE_RELATION && r.relid != 0) else { return false };
    let Some((pk, n)) = b.facts.primary_key(rte.relid) else { return false };
    pk[..n as usize].iter().all(|&att| keys.iter().any(|&k| base(k) == Some((rti, att))))
}

/// D3375 (m7_passenger_fd): PostgreSQL 18.6's remove_useless_groupby_columns (initsplan.c) drops the GROUP BY columns a
/// unique index determines too, not only the primary key's: `GROUP BY u, c_id` over `u int NOT NULL UNIQUE` groups by `u`
/// and emits `c_id` from the group's row. Whether the node's GROUP BY columns `keys` (each its input entry, equality
/// operator and collation) determine `e` so: `e` a bare `Var` of a base relation's column (as `pk_determined` reads it),
/// the relation no traditional-inheritance parent (a child may repeat a key; a partitioned table's unique index holds
/// over every partition), and an index of it that holds at most one row per group (`Facts::unique_bound`: UNIQUE,
/// IMMEDIATE, VALID, not partial; each key column a GROUP BY column of the same entry under an equality of the column's
/// btree family and under its collation, the planner's own test) over key columns NOT NULL under a valid constraint
/// (`AttFact::notnull`; a NULL-extended row holds NULL in every column of the relation, one group of its own). Group-equal
/// keys are then index-equal, so a group's rows carry one row of the relation and `e` one value: the column joins the
/// keys as a primary key's does. A source without the catalog (a fixture) answers no index.
pub fn unique_determined<'p>(b: &Builder<'p, '_>, keys: &[(Node<'p>, u32, u32)], e: Node<'p>) -> bool {
    use crate::catalog::KeyBind;
    let base = |n: Node<'p>| n.as_var().filter(|v| v.varlevelsup == 0 && v.varnosyn > 0 && v.varattnosyn > 0).map(|v| (v.varnosyn, v.varattnosyn));
    let Some((rti, _)) = base(e) else { return false };
    let Some(rte) = b.rte(rti).filter(|r| r.rtekind == RTEKind::RTE_RELATION && r.relid != 0 && !(r.inh && r.relkind != RELKIND_PARTITIONED_TABLE)) else { return false };
    let relid = rte.relid;
    let mut hit = false;
    b.facts.indexes_of(relid, &mut |i| {
        if hit || !i.unique || !i.immediate || i.partial {
            return;
        }
        let ext = b.facts.index_ext(i.indexid);
        let n = usize::from(if ext.known && ext.nkeyatts > 0 { ext.nkeyatts.min(i.nkeys) } else { i.nkeys });
        let mut binds = [KeyBind { col: 0, opno: 0, collid: 0, nullable: false }; gravity_program::limits::KEYS];
        for (col, &att) in i.keys[..n].iter().enumerate() {
            if att <= 0 || !b.facts.att(relid, att).is_some_and(|a| a.notnull) {
                return;
            }
            let Some(&(_, opno, collid)) = keys.iter().find(|(k, ..)| base(*k) == Some((rti, att))) else { return };
            binds[col] = KeyBind { col: col as u8, opno, collid, nullable: false };
        }
        hit = n > 0 && b.facts.unique_bound(i.indexid, &binds[..n], false);
    });
    hit
}

/// `pg_class.relkind` of a partitioned table.
const RELKIND_PARTITIONED_TABLE: u8 = b'p';

/// aggtrans C1b (lowerv4 hosted.rs `key_member`): a column's syntactic identity (`varnosyn`, `varattnosyn`), the one
/// `pk_determined` reads: PG keeps it on every copy of the Var up the plan (setrefs' OUTER_VAR / INNER_VAR copies, a
/// NestLoopParam's value).
pub fn syn_col(n: Node<'_>) -> Option<(u32, i16)> {
    crate::facts::pinned::relabels(n).as_var().filter(|v| v.varlevelsup == 0 && v.varnosyn > 0 && v.varattnosyn > 0).map(|v| (v.varnosyn, v.varattnosyn))
}

/// aggtrans C1b: whether pipeline ops `ops` hold columns `x` and `y` (`syn_col`) equal on every row they emit under an
/// identity equality (`catalog::keysem::image_eq`): an inner hash or merge join's clause or qual conjunct, an inner nested loop's
/// qual conjunct or its inner index key on one column bound to the other by a NestLoopParam. `None` at an Append op: an
/// arm's rows need not pass the join, so the caller takes nothing upstream either. `f` answers the image equalities.
pub fn equal_cols(f: &dyn crate::catalog::Facts, ops: &[Op<'_>], x: (u32, i16), y: (u32, i16)) -> Option<bool> {
    use crate::catalog::keysem::image_eq;
    use crate::facts::pinned::relabels;
    use crate::join::JoinDesc;
    use types_nodes::jointype::JoinType as J;
    fn pair(a: (u32, i16), b: (u32, i16), x: (u32, i16), y: (u32, i16)) -> bool {
        (a == x && b == y) || (a == y && b == x)
    }
    let eq = |l: &NodeList<'_>, x: (u32, i16), y: (u32, i16)| -> bool {
        l.iter().filter_map(|c| c.as_op_expr()).any(|o| o.args.len() == 2 && image_eq(f, o.opno).is_some() && syn_col(o.args.nth(0)).zip(syn_col(o.args.nth(1))).is_some_and(|(a, b)| pair(a, b, x, y)))
    };
    let mut hit = false;
    for op in ops {
        hit |= match op {
            Op::Append(_) => return None,
            Op::Join(JoinDesc::Probe { jointype: J::JOIN_INNER, clauses, qual, .. } | JoinDesc::Merge { jointype: J::JOIN_INNER, clauses, qual, .. }) => eq(clauses, x, y) || eq(qual, x, y),
            Op::Join(JoinDesc::Nest { jointype: J::JOIN_INNER, params, inner, qual, .. }) => {
                let bound = |p: &Param| params.iter().filter_map(|n| n.as_nest_loop_param()).find(|n| n.paramno == p.paramid).and_then(|n| syn_col(n.paramval));
                let key = |k: &crate::graph::IndexKey<'_>, rti: u16| {
                    matches!(k.form, crate::graph::KeyForm::Op | crate::graph::KeyForm::AmOp) && k.attno > 0 && image_eq(f, k.opno).is_some()
                        && relabels(k.value).as_param().filter(|p| p.paramkind == ParamKind::PARAM_EXEC).and_then(|p| bound(p)).is_some_and(|o| pair((u32::from(rti), k.attno), o, x, y))
                };
                eq(qual, x, y) || matches!(&inner.source, Source::Index { scanrelid, keys, .. } if keys.iter().any(|k| key(k, *scanrelid)))
            }
            _ => false,
        };
    }
    Some(hit)
}
