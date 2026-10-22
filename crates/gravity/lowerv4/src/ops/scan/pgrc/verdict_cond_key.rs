//! The condition cache's key rule (H02i, `verdict.cond`; the road, the fingerprint over the covered quals and the
//! survivor words are verdict_cond.rs): which leading filter quals the cache may take, and each one's canonical shape.
//!
//! CONDITION CACHE CORRECTNESS RULE: a qual consults or fills the cache only when every function and
//! operator in it is IMMUTABLE (`pg_proc.provolatile = 'i'`), it reads only its scan's columns, and each constant is a
//! bind word of a by-value or varlena type: a plan literal, or an extern parameter (`PARAM_EXTERN`) bound for the
//! execution (D289: its value is hashed at prepare as a literal's is; `PARAM_EXEC`, a subplan's and any
//! per-row parameter stay out, and any cast applied to it must be IMMUTABLE too); no folded or by-reference
//! fixed-length literal; anything else (a cast through text I/O, a STABLE `now()` or timestamptz arithmetic, a
//! subplan) declines. The fingerprint is the static shape (every node's functions, result types, typmods and
//! collations, each constant's type / typmod / collation / length form, a parameter's tagged apart from a literal's, the
//! relation and the site's zone predicates, whose drops are clear bits) finished at prepare over the constants' bound
//! words (exec `Verdicts`). Visibility and delete vectors are the host's, applied after the
//! cached words and never cached; the host never caches a temp relation, nor an unlogged one under
//! `pgrust.condition_cache_unlogged = off` (default on; exec src.verdicts; hosts `cond_cache` keys a part's words by its
//! uuid and open part).
//! A helper of `verdict.cond`: no row of its own. Owner M1-SOURCE.
use gravity_expr::emit::ParamNeed;
use gravity_expr::{Catalog, ExprNode, NodeId, Program};
use gravity_lower::Place;
use gravity_program::{table_fn, SiteDesc};
use mcx::PgVec;
use mcx::GrowIn;
use types_nodes::primnodes::ParamKind;
use types_nodes::{Node, NodeList};

use crate::admit::refusal::Reason;
use crate::frame::px::{ExId, Px};
use crate::rep::storage;

/// A cacheable qual: its canonical shape bytes and its constants' bind words in need order, each with its varlena bit.
pub(crate) struct Shape<'g> {
    pub bytes: PgVec<'g, u8>,
    pub words: PgVec<'g, (u16, bool)>,
}

/// Qual `q`'s shape when the cache may take it (the rule above), else `None`. The shape walks the plan's node and the
/// words follow the compiled program's needs: both must see the same constants (types) in the same order, or a word's
/// varlena bit (the host hashes a varlena word through its pointer) could land on another constant.
pub(crate) fn shape_of<'g>(px: &Px<'g, '_>, q: ExId) -> Option<Shape<'g>> {
    let (n, varno, _) = crate::ops::scan::pgrc::lower::qual_node(px, q)?;
    let (mut s, mut types) = (Shape { bytes: PgVec::new_in(px.mcx()), words: PgVec::new_in(px.mcx()) }, PgVec::new_in(px.mcx()));
    walk(n, varno, px.gx.catalog, &mut s.bytes, &mut types)?;
    let e = px.placed(q);
    let mut consts = PgVec::new_in(px.mcx());
    lowered(&e.c.prog, e.c.root, px.gx.catalog, &mut consts)?;
    let same = consts.len() == types.len() && consts.iter().zip(types.iter()).all(|(c, t)| c.0 == *t);
    if !same || e.places.len() != e.needs.len() {
        return None;
    }
    // each constant's word in need order; an IN list's array is its elements' words (emit's `ParamNeed::Elem`), their
    // count and the array's NULL in the shape (an empty and a NULL array both have none)
    let mut at = e.places.iter().zip(e.needs.iter());
    for &(_, v, list) in &consts {
        if let Some((n, null)) = list {
            put(&mut s.bytes, b'n', &[n as u64, null as u64]);
        }
        for _ in 0..list.map_or(1, |l| l.0) {
            match (at.next()?, list) {
                ((Place::Word(w), ParamNeed::Const(_) | ParamNeed::Extern { .. }), None) | ((Place::Word(w), ParamNeed::Elem(_)), Some(_)) => s.words.push_in((*w, v)),
                _ => return None,
            }
        }
    }
    at.next().is_none().then_some(s)
}

/// The compiled program's constants in need order (emit's plan walk) as `(type, varlena, list)`, `list` an IN list's
/// `(element count, NULL)` whose varlena bit is its elements'; `None` at a node the shape walk does not take.
fn lowered(p: &Program<'_>, id: NodeId, cat: &dyn Catalog<'_>, out: &mut PgVec<'_, (u32, bool, Option<(usize, bool)>)>) -> Option<()> {
    crate::stack_check();
    let span = |k: gravity_expr::Kids| p.kids.get(k.start as usize..(k.start + k.len) as usize);
    let kids = match p.nodes.get(id.0 as usize)? {
        ExprNode::Const(c) => {
            out.push_in((c.typ as u32, !c.byval && c.typlen == -1, None));
            return Some(());
        }
        ExprNode::Param(q) if matches!(q.kind, ParamKind::PARAM_EXTERN) => {
            out.push_in((q.typ as u32, storage::of(cat, q.typ).ok()?.len == -1, None));
            return Some(());
        }
        ExprNode::Var(_) | ExprNode::Imm(_) => return Some(()),
        ExprNode::Func(f) | ExprNode::Op(f) | ExprNode::Distinct(f) | ExprNode::NullIf(f) => span(p.sites.get(f.site.0 as usize)?.args)?,
        // `= ANY` / `<> ALL`: the operand, then a constant array as its elements (by value or varlena, else declined)
        ExprNode::ScalarArrayOp(s) => {
            let a = span(p.sites.get(s.site.0 as usize)?.args)?;
            lowered(p, *a.first()?, cat, out)?;
            let (ExprNode::Const(c), false) = (p.nodes.get(a.get(1)?.0 as usize)?, !s.elem.byval && s.elem.len != -1) else { return lowered(p, a[1], cat, out) };
            out.push_in((c.typ as u32, !s.elem.byval, Some((if c.is_null() { 0 } else { c.list_len()? }, c.is_null()))));
            return Some(());
        }
        ExprNode::Bool(n) => span(n.args)?,
        ExprNode::NullTest(n) => core::slice::from_ref(&n.arg),
        ExprNode::BooleanTest(n) => core::slice::from_ref(&n.arg),
        ExprNode::Relabel(n) => core::slice::from_ref(&n.arg),
        // an extension's claimed call (step/text_like.rs claim_calls): `CASE WHEN Var(EXT_AT + 2n) THEN Var(..) ELSE <call>
        // END`, the call's literals kept in order in its cold arm (M6-REPS, jsonb.inplace's C4 / C12 quals)
        // M6 R3 (source/jsonb_codes.rs): `CASE WHEN lane IS NULL THEN <compare> ELSE <compare over the lane> END`, whose
        // lane arm reads the compare's own literal nodes: they are read once, in the compare
        ExprNode::Case(k) if k.arg.is_none() && matches!(span(k.whens)?.first().map(|w| &p.nodes[w.0 as usize]), Some(ExprNode::NullTest(t)) if matches!(p.nodes.get(t.arg.0 as usize), Some(ExprNode::Var(v)) if v.attno >= crate::frame::site::SHRED_AT)) => return lowered(p, *span(k.whens)?.get(1)?, cat, out),
        ExprNode::Case(k) if k.arg.is_none() => return span(k.whens)?.iter().chain(k.default.iter()).try_for_each(|&k| lowered(p, k, cat, out)),
        _ => return None,
    };
    kids.iter().try_for_each(|&k| lowered(p, k, cat, out))
}

/// D3938: qual `q`'s shape (`shape_of`) when the cache may take it, with what makes its verdicts: the catalog reads its
/// shape rests on (its functions' and types' rows, recorded: they are the lowering's too) and its producer (`producer`).
pub(crate) fn covered<'g>(px: &mut Px<'g, '_>, k: usize, site: &SiteDesc, q: ExId, node: i32) -> Option<(Shape<'g>, gravity_deps::Deps, PgVec<'g, u8>)> {
    let (s, deps) = gravity_deps::recording(|| shape_of(px, q));
    let s = s?;
    let mut made = PgVec::new_in(px.mcx());
    producer(px, k, site, q, node, &mut made);
    Some((s, deps, made))
}

/// D3938: what makes covered qual `q`'s verdicts besides its shape (the condition cache keys by it, verdict_cond.rs
/// `finish`): its per-code reads, each table as `site` declares it (function, flags without D3115's sweep bit, collation,
/// encoding, arguments: a `PROGRAM` table's argument is its fill expression's fingerprint, a hosted one's its pattern or
/// values) over its column by attno, never by slot or lane, which another statement's site orders otherwise. A qual whose
/// per-code reads are another making's tables refuses (`percode::owns`, the D3913 class): its verdicts would be another
/// expression's, kept for every later statement with its shape.
pub(crate) fn producer(px: &mut Px<'_, '_>, k: usize, site: &SiteDesc, q: ExId, node: i32, out: &mut PgVec<'_, u8>) {
    if !crate::dict::percode::owns(px, k, q) {
        crate::refuse!(node, Reason::Bug { payload: "a covered qual whose per-code reads are another making's tables (D3913 class)" });
    }
    let Some(pl) = px.placed(q).ext.as_deref().and_then(|x| x.downcast_ref::<crate::dict::percode::Plan>()) else {
        return put(out, b'p', &[0]);
    };
    put(out, b'p', &[pl.tables.len() as u64 + 1]);
    for &(t, class) in &pl.tables {
        let Some(d) = site.tables.get(t as usize) else { crate::refuse!(node, Reason::Bug { payload: "a covered qual's per-code table the site does not declare" }) };
        let attno = pl.cols.iter().find(|c| c.1 == d.over).map_or(0, |c| c.0 as u16 as u64);
        let flags = if d.func == table_fn::PROGRAM { d.flags & !table_fn::FLAG_SWEPT } else { d.flags };
        put(out, b't', &[class as u64, attno, d.func as u64, flags as u64, d.collation as u64, d.encoding as u32 as u64]);
        for a in 0..2 {
            let arg = site.table_arg(d, a);
            out.extend_from_slice_in(&(arg.len() as u32).to_le_bytes());
            out.extend_from_slice_in(arg);
        }
    }
}

pub(crate) fn put(out: &mut PgVec<'_, u8>, tag: u8, xs: &[u64]) {
    out.push_in(tag);
    for x in xs {
        out.extend_from_slice_in(&x.to_le_bytes());
    }
}

/// `n` serialized into `out` (pre-order), its constants' types into `consts`; `None` when a node, function or constant
/// is not cacheable.
fn walk<'p, 'g, 'o>(n: Node<'p>, varno: i32, cat: &dyn Catalog<'p>, out: &mut PgVec<'o, u8>, consts: &mut PgVec<'g, u32>) -> Option<()> {
    crate::stack_check();
    // `provolatile` = 'i' (PostgreSQL utils/cache/lsyscache.c func_volatile), asked of the pg_proc row's own test
    let immutable = |f: u32| f != 0 && cat.func(f).is_ok_and(|x| x.immutable());
    let args = |l: &NodeList<'p>, out: &mut PgVec<'o, u8>, consts: &mut PgVec<'g, u32>| -> Option<()> {
        out.extend_from_slice_in(&(l.len() as u32).to_le_bytes());
        l.iter().try_for_each(|a| walk(a, varno, cat, out, consts))
    };
    if let Some(v) = n.as_var() {
        if v.varno != varno || v.varlevelsup != 0 || v.varattno <= 0 {
            return None;
        }
        put(out, b'V', &[v.varattno as u64, v.vartype as u64, v.vartypmod as u32 as u64, v.varcollid as u64]);
        return Some(());
    }
    // D289: an extern parameter, a bind word of the execution; its record is `P`, never a literal's `C`
    if let Some(q) = n.as_param() {
        let tf = storage::of(cat, q.paramtype).ok().filter(|t| matches!(q.paramkind, ParamKind::PARAM_EXTERN) && (t.byval || t.len == -1))?;
        put(out, b'P', &[q.paramtype as u64, q.paramtypmod as u32 as u64, q.paramcollid as u64, tf.len as u32 as u64, tf.byval as u64, consts.len() as u64]);
        consts.push_in(q.paramtype as u32);
        return Some(());
    }
    if let Some(c) = n.as_const() {
        let varlena = !c.constbyval && c.constlen == -1;
        if !c.constbyval && !varlena {
            return None;
        }
        put(out, b'C', &[c.consttype as u64, c.consttypmod as u32 as u64, c.constcollid as u64, c.constlen as u32 as u64, c.constbyval as u64, consts.len() as u64]);
        consts.push_in(c.consttype as u32);
        return Some(());
    }
    let op = n.as_op_expr().map(|o| (b'O', o.opno, o.opfuncid, o.opresulttype, o.opretset, o.opcollid, o.inputcollid, &o.args));
    let op = op.or_else(|| n.as_distinct_expr().map(|o| (b'D', o.opno, o.opfuncid, o.opresulttype, o.opretset, o.opcollid, o.inputcollid, &o.args)));
    let op = op.or_else(|| n.as_null_if_expr().map(|o| (b'N', o.opno, o.opfuncid, o.opresulttype, o.opretset, o.opcollid, o.inputcollid, &o.args)));
    // a scalar-array op (C12's IN list): ANY / ALL by tag, its result bool, no result collation
    let op = op.or_else(|| n.as_scalar_array_op_expr().map(|o| (if o.useOr { b'Y' } else { b'L' }, o.opno, o.opfuncid, 16, false, 0, o.inputcollid, &o.args)));
    if let Some((tag, opno, func, rtype, retset, coll, incoll, a)) = op {
        if retset || !immutable(func) {
            return None;
        }
        put(out, tag, &[opno as u64, func as u64, rtype as u64, coll as u64, incoll as u64]);
        return args(a, out, consts);
    }
    if let Some(f) = n.as_func_expr() {
        if f.funcretset || !immutable(f.funcid) {
            return None;
        }
        put(out, b'F', &[f.funcid as u64, f.funcresulttype as u64, f.funcvariadic as u64, f.funccollid as u64, f.inputcollid as u64]);
        return args(&f.args, out, consts);
    }
    if let Some(b) = n.as_bool_expr() {
        put(out, b'B', &[b.boolop as u64]);
        return args(&b.args, out, consts);
    }
    if let Some(t) = n.as_null_test() {
        if t.argisrow {
            return None;
        }
        put(out, b'T', &[t.nulltesttype as u64]);
        return walk(t.arg?, varno, cat, out, consts);
    }
    if let Some(t) = n.as_boolean_test() {
        put(out, b'b', &[t.booltesttype as u64]);
        return walk(t.arg?, varno, cat, out, consts);
    }
    if let Some(r) = n.as_relabel_type() {
        put(out, b'R', &[r.resulttype as u64, r.resulttypmod as u32 as u64, r.resultcollid as u64]);
        return walk(r.arg, varno, cat, out, consts);
    }
    None
}
