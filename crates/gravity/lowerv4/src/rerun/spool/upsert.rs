//! D3752: whether a spooled INSERT .. ON CONFLICT stores PostgreSQL's rows whatever order its rows come in. A hash join's
//! drain under its spooled form (hashjoin/drain/lower.rs, row A2a) hands its records on in record order, where
//! PostgreSQL's Hash Right / Right Anti Join goes bucket by bucket; the write it feeds is served only when that order
//! picks no stored row (`order_free`), else refused by the name it gives.
use gravity_decompose::ops::modifytable::trig_facts::{TRIG_INSERT, TRIG_UPDATE};
use gravity_decompose::ops::shared::modifytable::WriteDesc;
use gravity_decompose::Op;
use mcx::{CollectIn, GrowIn, Mcx, PgVec};
use types_nodes::primnodes::INNER_VAR;
use types_nodes::{Node, NodeTag};

use crate::graph::Gx;

/// D3752's refusal of an INSERT .. ON CONFLICT DO NOTHING whose rows' order picks the row it stores (`order_free`).
pub(crate) const PICKS_NOTHING: &str = "an INSERT .. ON CONFLICT DO NOTHING whose rows may repeat a key with other cells, in an order that is not PostgreSQL's (its first row of a key is the one stored: D3752)";
/// D3752's refusal of an INSERT .. ON CONFLICT DO UPDATE whose rows' order picks what it stores (`order_free`).
pub(crate) const PICKS_UPDATE: &str = "an INSERT .. ON CONFLICT DO UPDATE whose rows, in an order that is not PostgreSQL's, may pick the update (a WHERE reading EXCLUDED or acting, a SET or trigger moving a key: D3752)";

/// D3752: whether INSERT `w` (op `j` of `ops`, a heap INSERT with one target) stores PostgreSQL's rows whatever order its
/// rows come in, or the name it refuses by. A drain's spooled form hands its records on in record order where PostgreSQL's
/// Hash Right / Right Anti Join goes bucket by bucket (DECISIONS 6b frees which row gets which VOLATILE value, never which
/// row is stored). No ON CONFLICT: a plain INSERT (D2247). A BEFORE ROW trigger may rewrite a row first: refused.
///
/// DO UPDATE: a key two rows reach raises 21000 at the second whichever comes first (heap_lock_tuple's TM_Invisible on a
/// version this command wrote), and a WHERE that leaves the existing row as it was keeps it locked and unchanged for the
/// next row of its key, which tests the same row. So the order picks nothing unless the WHERE reads EXCLUDED or acts, or
/// the update moves an arbiter's key (a SET of a key column, a generated key, an expression or partial arbiter): a later
/// row of the old key then inserts.
///
/// DO NOTHING: the first of two rows that one arbiter takes for one key is stored, the next skipped. The order picks
/// nothing when, for each arbiter (every unique index with no conflict target), no two rows take one key (a key column a
/// sequence that never cycles fills: its `nextval` calls are distinct), or two that do store one row: every column the
/// projection reads is a plain key column of it whose equal values are one image (`keysem::one_image` under its
/// collation and the type's own built-in operator class), so equal keys mean equal reads and the rows differ only by
/// their VOLATILE calls (which row gets which `nextval` is DECISIONS 6b's).
pub(crate) fn order_free(gx: &Gx<'_, '_>, ops: &[Op<'_>], j: usize, w: &WriteDesc<'_>) -> Result<(), &'static str> {
    let Some(oc) = &w.on_conflict else { return Ok(()) };
    let why = if oc.update { PICKS_UPDATE } else { PICKS_NOTHING };
    let Some(t) = w.targets.first().filter(|_| w.targets.len() == 1 && w.route.is_none()) else { return Err(why) };
    let trig = gx.facts.trig_ext(t.relid);
    if !trig.known || trig.before_row & (TRIG_INSERT | TRIG_UPDATE) != 0 {
        return Err(why);
    }
    let arbiters: PgVec<'_, u32> = if !oc.arbiter.is_nil() {
        oc.arbiter.iter().collect_in(gx.mcx)
    } else if t.indexes.iter().all(|i| i.known) {
        t.indexes.iter().filter(|i| i.unique || i.exclusion).map(|i| i.indexid).collect_in(gx.mcx)
    } else {
        return Err(why);
    };
    // each arbiter's index facts: a valid, immediate unique btree, its key columns past the INCLUDE ones dropped
    let mut idx = mcx::reserved_in(arbiters.len(), gx.mcx);
    for &id in arbiters.iter() {
        let (Some(i), e) = (gx.facts.index(id), gx.facts.index_ext(id)) else { return Err(why) };
        if !e.known || e.exclusion || e.deferrable || !i.unique || !i.btree {
            return Err(why);
        }
        let n = usize::from(if e.nkeyatts == 0 { i.nkeys } else { e.nkeyatts.min(i.nkeys) });
        idx.push_in((id, i, n));
    }
    if oc.update {
        let picks = oc.filter.is_some_and(|f| quals(gx.mcx, f).into_iter().any(|q| reads_inner(q) || !crate::effects::expr_quiet(gx, q)));
        let moves = idx.iter().any(|(_, i, n)| i.partial || i.keys[..*n].iter().any(|&a| a <= 0 || oc.cols.iter().any(|c| c == i32::from(a)) || gx.facts.att_generated(t.relid, a) != 0));
        return if picks || moves { Err(why) } else { Ok(()) };
    }
    let Some(proj) = ops[..j].iter().rev().find_map(|o| if let Op::Project(l) = o { Some(*l) } else { None }) else { return Err(why) };
    let cols: PgVec<'_, (i16, Node<'_>)> = proj.iter().filter_map(|n| n.as_target_entry()).filter(|e| !e.resjunk).map(|e| (e.resno, e.expr)).collect_in(gx.mcx);
    let col = |a: i16| cols.iter().find(|c| c.0 == a).map(|c| c.1);
    // every Var the stored row reads, or none when it reads a row some other way (an outer level, a SubPlan, an exec Param)
    let reads = row_vars(gx.mcx, &cols[..]);
    for (id, i, n) in idx {
        let opc = gx.facts.index_opc(id);
        // key column `k` compares by its type's own built-in `=`, equal values one image under the index's collation
        let image = |k: usize| {
            let a = i.keys[k];
            opc.known && opc.builtin & (1 << k) != 0 && gx.facts.index_family(id, k as u8) == 0 && a > 0 && gx.facts.att(t.relid, a).zip(gx.facts.index_coll(id, k as u8)).is_some_and(|(att, c)| gravity_decompose::catalog::keysem::one_image(gx.facts, att.base, c))
        };
        let fresh = (0..n).any(|k| image(k) && col(i.keys[k]).and_then(sequence).is_some_and(|s| gx.facts.seq_cycles(s) == Some(false)));
        let covered = |v: (i32, i16)| (0..n).any(|k| image(k) && col(i.keys[k]).and_then(bare_var) == Some(v));
        if !fresh && !reads.as_ref().is_some_and(|r| r.iter().all(|&v| covered(v))) {
            return Err(why);
        }
    }
    Ok(())
}

/// ON CONFLICT DO UPDATE's WHERE as its quals (setrefs leaves it an implicit-AND list).
fn quals<'g, 'n>(mcx: Mcx<'g>, f: Node<'n>) -> PgVec<'g, Node<'n>> {
    f.as_list().map_or_else(|| mcx::pg_vec![in mcx; f], |l| l.iter().collect_in(mcx))
}

/// Whether qual `f` of ON CONFLICT DO UPDATE's WHERE reads EXCLUDED (INNER_VAR after setrefs), or a node the walk does
/// not know.
fn reads_inner(f: Node<'_>) -> bool {
    let mut hit = false;
    let known = gravity_decompose::expr::walk(f, &mut |x| {
        hit |= x.as_var().is_some_and(|v| v.varno == INNER_VAR);
        !hit
    });
    hit || !known
}

/// The `(varno, varattno)` of each Var the columns read, or none when one reads its row by anything else: a Var of an
/// outer level, an exec Param (a nest loop's or a SubPlan's), a SubPlan, or a node the walk does not know. A
/// NextValueExpr (an identity column's value) reads nothing.
fn row_vars<'g>(mcx: Mcx<'g>, cols: &[(i16, Node<'_>)]) -> Option<PgVec<'g, (i32, i16)>> {
    let mut vars = PgVec::new_in(mcx);
    cols.iter().all(|&(_, e)| vars_of(e, &mut vars)).then_some(vars)
}

/// `row_vars` over one expression: false when it reads its row by anything but a Var of its own level.
fn vars_of(e: Node<'_>, vars: &mut PgVec<'_, (i32, i16)>) -> bool {
    crate::stack_check();
    use types_nodes::primnodes::ParamKind;
    match e.node_tag() {
        NodeTag::T_Var => match e.as_var() {
            Some(v) if v.varlevelsup == 0 => {
                vars.push_in((v.varno, v.varattno));
                true
            }
            _ => false,
        },
        NodeTag::T_NextValueExpr => true,
        NodeTag::T_Param => e.as_param().is_some_and(|p| p.paramkind != ParamKind::PARAM_EXEC),
        NodeTag::T_SubPlan | NodeTag::T_AlternativeSubPlan | NodeTag::T_Aggref | NodeTag::T_WindowFunc | NodeTag::T_GroupingFunc | NodeTag::T_PlaceHolderVar => false,
        _ => {
            let mut ok = true;
            let known = gravity_decompose::expr::kids(e, &mut |k| ok = ok && vars_of(k, vars));
            ok && known
        }
    }
}

/// A column that is a bare Var (through binary relabels): its `(varno, varattno)`.
fn bare_var(mut e: Node<'_>) -> Option<(i32, i16)> {
    while let Some(r) = e.as_relabel_type() {
        e = r.arg;
    }
    e.as_var().filter(|v| v.varlevelsup == 0).map(|v| (v.varno, v.varattno))
}

/// A column a sequence fills: an identity's NextValueExpr or `nextval(regclass)` (a serial's default), through binary
/// relabels and the integer narrowing casts (`catalog::casts::int_narrowing`: `int4(int8)` / `int2(int8)` raise rather
/// than map two values to one): its sequence.
fn sequence(mut e: Node<'_>) -> Option<u32> {
    use gravity_decompose::catalog::casts::{int_narrowing, nextval};
    loop {
        if let Some(r) = e.as_relabel_type() {
            e = r.arg;
        } else if let Some(f) = e.as_func_expr().filter(|f| int_narrowing(f.funcid) && f.args.len() == 1) {
            e = f.args.iter().next()?;
        } else {
            break;
        }
    }
    if let Some(n) = e.as_variant::<types_nodes::primnodes::NextValueExpr>() {
        return Some(n.seqid);
    }
    let f = e.as_func_expr().filter(|f| nextval(f.funcid) && f.args.len() == 1)?;
    let c = f.args.iter().next()?.as_const().filter(|c| !c.constisnull)?;
    Some(c.constvalue.as_oid())
}
