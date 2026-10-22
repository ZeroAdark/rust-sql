//! A', under join.dense (H23a): the build-side hoist. A fold (plain or keyed) over an INNER join that reads a coded build
//! column only as one boolean call's first argument, its other arguments constants (D3019: or extern parameters; a LIKE's
//! `$n` pattern by its execution's value, `effects::pattern_quiet`) (e.g. `col LIKE 'prefix%'`, `col =
//! 'x'`, `col < 'm'`; JOIN-01: any such call, not a LIKE list), has the build evaluate that call once
//! per row instead, its code read by the row's own part's per-entry table (step/percode.rs attaches in the build's row;
//! without the table the row renders, still exact), and keep the verdict as the payload word in the code's place. The
//! probe pipeline's projection forwards it, and every program compiled over the call reads it instead (`rewrite`, from
//! step/expr.rs before its literals are placed: the build decides first, its pipeline made before the probe's). The guards: the call raises on no row (1: a LIKE
//! family call's constant non-NULL pattern not ending in the escape character under a deterministic collation, ILIKE
//! under a C ctype only (D2680), where the table serves it; any other call raise-free by effects, e.g. a leakproof
//! comparison) and is immutable (2); a NULL argument's verdict is the call's own, made by the build (3); only an INNER
//! probe hoists (4, `joins`); each row's verdict is from its own part's dictionary (5); any other read of the column keeps
//! the code (6). Budget 160 lines. Owner M1-JOIN.
use std::any::Any;
use std::rc::Rc;

use gravity_decompose::ops::shared::agg::{GroupDecl, GroupForm};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::Op;
use gravity_expr::nodes::var::Var;
use gravity_expr::{Compiled, ExprNode, Program, Source};
use gravity_program::{ExprOut, LifeId};
use mcx::{CollectIn, GrowIn, Mcx, PgVec};
use types_nodes::jointype::JoinType;
use types_nodes::primnodes::{INNER_VAR, OUTER_VAR};
use types_nodes::Node;

use crate::dict::face::PerValue;
use crate::face::kinds::Face;
use crate::frame::px::{ExId, Px};
use crate::graph::Gx;

use crate::ops::hashjoin::build_side::{JoinExt, Shape};

const BOOLOID: u32 = 16;

/// A hoisted verdict: build payload column `col` (INNER_VAR attno) holds call `func`'s verdict (`call`, the node's raw
/// pointer), and the probe pipeline's projection forwards it as resno `r`, which the fold reads only as that call's value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Hoist {
    pub col: i16,
    pub r: i16,
    pub func: u32,
    pub call: usize,
}

/// Guard 4: only an INNER probe hoists. A null-extended row would read a verdict the build never made where PG evaluates
/// the call over NULLs (M7's outer joins extend this knowingly).
pub(crate) fn joins(jt: JoinType) -> bool {
    jt == JoinType::JOIN_INNER
}

fn plan(px: &mut Px<'_, '_>, sh: &Shape) -> Option<Hoist> {
    let gx = px.gx;
    let mcx = px.mcx();
    // the probe pipeline, where the consumers compile, is made after the build that publishes (index order; CORE's note)
    let q = sh.probe.filter(|&q| q > sh.build && !sh.swapped && PerValue::Table.on())?;
    let p = &gx.pipelines()[q];
    let at = 1 + p.ops.iter().position(|op| matches!(op, Op::Join(JoinDesc::Probe { slot, .. }) if *slot == sh.slot))?;
    let (Op::Join(JoinDesc::Probe { jointype, keys, qual, .. }), Some(Op::Project(l))) = (&p.ops[at - 1], p.ops.get(at)) else { return None };
    let g = group(gx, q)?;
    if !joins(*jointype) || p.ops.len() != at + 1 {
        return None;
    }
    let codes = crate::step::project::code_cols(px, sh.build, gx.pipelines()[sh.build].ops.len() - 1);
    for &col in sh.payload.iter().filter(|a| codes.iter().any(|c| c.0 == **a)) {
        let n = |e: Node<'_>| reads(e, INNER_VAR, col);
        let bare = |e: Node<'_>| e.as_target_entry().map_or(e, |t| t.expr).as_var().is_some_and(|v| v.varno == INNER_VAR && v.varattno == col);
        let mut at_r = l.iter().enumerate().filter(|(_, e)| n(*e) != Some(0));
        let (Some((i, e)), None) = (at_r.next(), at_r.next()) else { continue };
        let r = i as i16 + 1;
        if !bare(e) || keys.iter().chain(qual.iter()).any(|x| n(x) != Some(0)) || g.keys.iter().any(|k| k.resno == r) || g.pinned.contains(&r) {
            continue;
        }
        let Some(c) = the_call(g, r, mcx) else { continue };
        let Some((func, coll, _, pat)) = call(c) else { continue };
        let quiet = match like_form(func) {
            Some(ilike) => {
                let lit = pat.and_then(|p| p.as_const()).filter(|k| !k.constisnull).and_then(gravity_decompose::expr::const_bytes);
                // D3019: a generic plan's `$n` pattern, by this execution's value (the admission keys a loud one apart)
                let even = match pat.and_then(|p| p.as_param()) {
                    Some(prm) => gravity_decompose::effects::pattern_quiet(prm) == Some(true),
                    None => lit.is_some_and(|b| b.iter().rev().take_while(|&&x| x == b'\\').count() % 2 == 0),
                };
                even && gx.catalog.text(coll).is_ok_and(|t| t.deterministic && (!ilike || t.ctype_is_c))
            }
            None => crate::effects::raise_free(gx, c),
        };
        if quiet && crate::effects::immutable_call(gx, func) {
            return Some(Hoist { col, r, func, call: c.as_raw().as_ptr() as usize });
        }
    }
    None
}

/// Probe pipeline `q`'s end when it is a fold, plain or keyed (AGA-23: a hashed or sorted group's arguments and FILTER
/// compile through `compile_at` as a plain fold's do, table/args.rs), with no grouping sets or two passes.
fn group<'g, 'p>(gx: &Gx<'g, 'p>, q: usize) -> Option<&'g GroupDecl<'p>> {
    let fold = |f: GroupForm| matches!(f, GroupForm::Plain | GroupForm::Hashed | GroupForm::Sorted);
    gx.graph.groups.iter().find(|g| g.producer == LifeId(q as u16) && fold(g.form) && g.two_pass.is_none() && g.set.is_none())
}

/// The one hoistable call over bare `outer.r` in `g`'s aggregates, when they read `outer.r` nowhere else (guard 6).
fn the_call<'p>(g: &GroupDecl<'p>, r: i16, mcx: Mcx<'_>) -> Option<Node<'p>> {
    let mut out = PgVec::new_in(mcx);
    let ok = g.aggs.iter().fold(true, |ok, a| hoistable_reads(a.node, r, &mut out) && ok);
    (ok && out.len() == 1).then(|| out[0])
}

/// `the_call`'s walk: whether every read of `outer.r` in `n` sits inside a call over `outer.r` alone, each such call
/// collected into `out` (a node the walk cannot see reads as not); it decides where a column is read, no effect.
fn hoistable_reads<'p>(n: Node<'p>, r: i16, out: &mut PgVec<'_, Node<'p>>) -> bool {
    crate::stack_check();
    if call(n).is_some_and(|(_, _, a, _)| outer_col(a) == Some(r)) {
        out.push_in(n);
        return true;
    }
    if let Some(v) = n.as_var() {
        return !(v.varno == OUTER_VAR && (v.varattno == r || v.varattno == 0));
    }
    let mut ok = true;
    let known = gravity_decompose::expr::kids(n, &mut |k| ok &= hoistable_reads(k, r, out));
    ok && known
}

/// A hoistable call (JOIN-01): an operator or function returning bool whose arguments after the first are constants; its
/// function, input collation, first argument and second (a LIKE call's pattern). The verdict word the probe reads is a
/// bool (`rewrite`).
fn call(n: Node<'_>) -> Option<(u32, u32, Node<'_>, Option<Node<'_>>)> {
    let (f, coll, rt, args) = match (n.as_op_expr(), n.as_func_expr()) {
        (Some(o), _) => (o.opfuncid, o.inputcollid, o.opresulttype, &o.args),
        (_, Some(f)) => (f.funcid, f.inputcollid, f.funcresulttype, &f.args),
        _ => return None,
    };
    let (a, p) = (args.iter().next()?, args.iter().nth(1));
    // D3019: an extern parameter is a constant of the execution (bound before the build runs)
    let bound = |x: Node<'_>| x.as_const().is_some() || x.as_param().is_some_and(|p| p.paramkind == types_nodes::primnodes::ParamKind::PARAM_EXTERN);
    (rt == BOOLOID && args.iter().skip(1).all(bound)).then_some((f, coll, a, p))
}

/// A LIKE-family call's form by its builtin's name (pg_proc.dat `prosrc` through fmgr_core's generated table, the
/// catalog's names: OIDW-31 / OIDW-32): `Some(false)` for LIKE / NOT LIKE over text or bpchar (bpchar's calls textlike),
/// `Some(true)` for ILIKE / NOT ILIKE, `None` for any other function, a user function whatever its name (the table is
/// keyed by the builtin's oid). name's LIKE (namelike) is none: its argument is no coded text column.
fn like_form(func: u32) -> Option<bool> {
    let t = fmgr_core::CANONICAL;
    match t.binary_search_by_key(&func, |e| e.0).ok().map(|i| t[i].1)? {
        "textlike" | "textnlike" => Some(false),
        "texticlike" | "texticnlike" => Some(true),
        _ => None,
    }
}

fn outer_col(n: Node<'_>) -> Option<i16> {
    crate::stack_check();
    match n.as_relabel_type() {
        Some(r) => outer_col(r.arg),
        None => n.as_var().filter(|v| v.varno == OUTER_VAR && v.varattno > 0).map(|v| v.varattno),
    }
}

/// The reads of column `a` of `varno` in `n` (a whole-row reference counts), `None` where the walker cannot see all of `n`.
fn reads(n: Node<'_>, varno: i32, a: i16) -> Option<usize> {
    crate::stack_check();
    if let Some(v) = n.as_var() {
        return Some(usize::from(v.varno == varno && (v.varattno == a || v.varattno == 0)));
    }
    let mut c = Some(0);
    let known = gravity_decompose::expr::kids(n, &mut |k| c = c.zip(reads(k, varno, a)).map(|(x, y)| x + y));
    c.filter(|_| known)
}

/// The build's hoisted payload of shape `sh`: the call compiled over the build row at its build op (`outer.r` becomes the
/// row's coded column, which percode.table reads by code), its column and value form.
pub(crate) fn build(px: &mut Px<'_, '_>, sh: &Shape, node: i32) -> Option<(i16, ExId, ExprOut)> {
    let h = plan(px, sh)?;
    let gx = px.gx;
    let mcx = px.mcx();
    let (k, jb) = (sh.build, gx.pipelines()[sh.build].ops.len() - 1);
    let Some(n) = sh.probe.and_then(|q| group(gx, q)).and_then(|g| the_call(g, h.r, mcx)) else { px.bug("a join hoist without its call") };
    let ex = crate::step::expr::compile_ext(px, n, None, node, |px, c, ex| {
        for x in c.prog.nodes.iter_mut() {
            if let ExprNode::Var(v) = x {
                if (v.src, v.attno) == (Source::Outer, h.r) {
                    (v.src, v.attno) = (Source::Scan, h.col);
                }
            }
        }
        crate::dict::percode::attach(px, k, jb, c, ex, node).map(|p| p as Rc<dyn Any>)
    });
    sh.hoist.set(Some(h)); // published after the build's own compile of the call, which reads the code
    fires::fire("join.hoist"); // shapecheck fire signal (REGISTRY JOIN-01)
    Some((h.col, ex, crate::step::project::out_of(gx, n, node)))
}

/// A program compiled over `node` that holds a hoisted call reads the forwarded verdict there: the call's node becomes the
/// column, its argument nodes left unreached. The calls rewritten (raw node pointers): their literals are not `c`'s needs.
pub(crate) fn rewrite<'g>(px: &Px<'g, '_>, node: Node<'_>, c: &mut Compiled<'_>) -> PgVec<'g, usize> {
    let Some(e) = px.ext::<JoinExt>(Face::Join) else { return PgVec::new_in(px.mcx()) };
    let mut out = PgVec::new_in(px.mcx());
    for h in e.shapes.iter().filter_map(|s| s.hoist.get()).filter(|h| holds(node, h.call)) {
        let hits: PgVec<'_, usize> = (0..c.prog.nodes.len()).filter(|&i| over(&c.prog, i, &h)).collect_in(px.mcx());
        // only a call actually replaced drops its literals (a program evaluating it still places them: CORE's review)
        if !hits.is_empty() {
            out.push_in(h.call);
        }
        for i in hits {
            c.prog.nodes[i] = ExprNode::Var(Var { src: Source::Outer, attno: h.r, typ: BOOLOID, desc: 0 });
        }
    }
    out
}

fn holds(n: Node<'_>, call: usize) -> bool {
    crate::stack_check();
    let mut hit = n.as_raw().as_ptr() as usize == call;
    if !hit {
        gravity_decompose::expr::kids(n, &mut |k| hit |= holds(k, call));
    }
    hit
}

/// Node `i` of `p` is the hoisted call over the forwarded column. It matches the function and its first argument, not the
/// pattern: sound only because `the_call` admits one call over `outer.r` (guard 6); relaxing that needs the pattern here.
fn over(p: &Program<'_>, i: usize, h: &Hoist) -> bool {
    let (ExprNode::Op(f) | ExprNode::Func(f)) = &p.nodes[i] else { return false };
    let s = &p.sites[f.site.0 as usize];
    let Some(mut a) = p.kids.get(s.args.start as usize).filter(|_| s.funcid == h.func && s.args.len > 0).map(|a| a.0 as usize) else { return false };
    while let ExprNode::Relabel(r) = &p.nodes[a] {
        if !r.transparent() {
            return false;
        }
        a = r.arg.0 as usize;
    }
    matches!(&p.nodes[a], ExprNode::Var(v) if v.src == Source::Outer && v.attno == h.r)
}

#[cfg(test)]
mod tests {
    use types_nodes::jointype::JoinType;

    #[test]
    fn only_an_inner_probe_hoists_its_call() {
        let all = [JoinType::JOIN_INNER, JoinType::JOIN_LEFT, JoinType::JOIN_FULL, JoinType::JOIN_RIGHT, JoinType::JOIN_SEMI, JoinType::JOIN_ANTI, JoinType::JOIN_RIGHT_SEMI, JoinType::JOIN_RIGHT_ANTI, JoinType::JOIN_UNIQUE_OUTER, JoinType::JOIN_UNIQUE_INNER];
        assert_eq!(all.iter().filter(|&&jt| super::joins(jt)).collect::<Vec<_>>(), [&JoinType::JOIN_INNER]);
    }

    /// OIDW-31 / OIDW-32: the catalog's names pick the same builtins the oid lists named (text and bpchar LIKE / NOT LIKE,
    /// ILIKE / NOT ILIKE), and nothing else up to the first user oid.
    #[test]
    fn the_like_family_is_named_by_the_catalog() {
        let like: Vec<u32> = (1..16384).filter(|&f| super::like_form(f) == Some(false)).collect();
        let ilike: Vec<u32> = (1..16384).filter(|&f| super::like_form(f) == Some(true)).collect();
        assert_eq!((like, ilike), (vec![850, 851, 1569, 1570, 1631, 1632], vec![1633, 1634, 1660, 1661]));
    }
}
