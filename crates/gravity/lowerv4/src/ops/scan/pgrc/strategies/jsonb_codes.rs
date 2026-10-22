//! Road `jsonb.codes` (H09 / H10, R13-L03 L13: path lanes served as code lanes, read by source/pgrc.rs, L0-g; L4
//! straight, L6 engaged) (cards/M6.md L4, M6-plan §3.10). Owner M6-JSONB. Budget 160 lines.
//!
//! The road applies with jsonb.lanes (`Applies::JsonbPgrc`). STRAIGHT FORM: with the road off, the `->>` value is walked in place from the jsonb image
//! (step/jsonb_inplace.rs) and compared as text, PostgreSQL's own evaluation.
//! ENGAGED, COMPARES: source/jsonb_lanes.rs declares each `col ->> 'key'` a leading filter compares,
//! whose key the relation's parts elected as a Text lane, as that path's code lane (`shred_kind::TEXT_CODES`: the path's
//! dictionary, a code never stands for NULL). Before step/percode.rs attaches a row step's expression, `rewrite` turns
//! each `->>` compare over such a path (any code-verdict compare, `code_verdict`: `=`, `<>`, `<`, LIKE, a regex, .. of a
//! constant, or `ANY` / `ALL` of a constant text array, under a collation that compares by bytes; one beyond `=` / `<>`
//! only over a path every part sealed as the writer's dictionary, jsonb_lanes `dict_sealed`, SRC-30) into `CASE WHEN lane
//! IS NULL THEN <the compare> ELSE <the compare over the lane> END`. A valid lane row holds the key's string value, so the lane arm is the compare's value, read from a
//! per-code table swept once per part (never per row); a NULL lane row (an absent key, a JSON null, a number, boolean
//! or container, a NULL document, a part that did not elect the path) evaluates the compare itself, and only there is
//! the jsonb column read (its image filled where the lane is invalid: source/pgrc_late.rs, hosts `guard_fill`). The
//! compare's literal is one node both arms read, so it keeps its one word. A parameter or other non-constant operand
//! (a generic plan's `= ANY($1)`, D921's form) and a nondeterministic collation decline: the compare stays whole.
//! A group key over a path is strategies/jsonb_keys.rs's (V4R-468). NOT BUILT HERE: ILIKE over a path's codes.
//!
//! Registers: road `jsonb.codes` (`JSONB_CODES`): `CodeRewrite(rewrite)`.
use gravity_expr::nodes::func::{Call, FuncNode};
use gravity_expr::nodes::{case::Case, null::NullTestNode, op::ScalarArrayOp, var::Var};
use gravity_expr::{Compiled, ExprNode, Kids, NodeId, SiteId, Source as VarSource};
use gravity_abi::ctx::lane::KIND_CODES;
use gravity_decompose::Op;
use gravity_program::{shred_kind, LaneId, SiteDesc};
use mcx::{CollectIn, Mcx, PgVec, GrowIn};

use crate::dict::face::{CodeRewrite, Undo};
use crate::frame::px::Px;
use crate::frame::site::SHRED_AT;
use crate::ops::Table;

crate::strategies! {
    /// Path lanes served as code lanes.
    JSONB_CODES = 87, "jsonb.codes", JsonbPgrc;
}

pub(crate) const ROWS: Table = Table { strategies: &[(JSONB_CODES, &CodeRewrite(rewrite))], ..crate::ops::NONE };

const JSONB_OID: u32 = 3802;
const TEXT_OID: u32 = 25;

/// R3: step `j` of pipeline `k`'s compares over a coded path of its site, each as the module header's CASE; a claim of
/// the first filter counted on the path (`ShredDecl::claims`, source/pgrc_late.rs).
fn rewrite(px: &mut Px<'_, '_>, k: usize, j: usize, c: &mut Compiled<'_>) -> Option<Undo> {
    let mcx = px.mcx();
    // a step the source's rows reach through filters only: its scan columns are the site's
    if !px.gx.pipelines()[k].ops[..j].iter().all(|op| matches!(op, Op::Filter(_) | Op::Gate(_))) {
        return None;
    }
    let (gx, relid) = (px.gx, if let gravity_decompose::Source::Range { relid, .. } = px.gx.pipelines()[k].source { relid } else { return None });
    let site = px.site_mut(k)?;
    if !site.shreds.iter().any(|d| d.kind == shred_kind::TEXT_CODES) {
        return None;
    }
    let len = [c.prog.nodes.len(), c.prog.kids.len(), c.prog.sites.len()];
    let mut u = Undo { k, len, moved: Vec::new(), outer: Vec::new(), claims: Vec::new() }; // alloc: Program
    for i in 0..c.prog.nodes.len() {
        let island = c.prog.islands.iter().any(|s| (s.first.0 as usize..=s.root.0 as usize).contains(&i));
        // jsonops (2): `col @> '{..}'` of string leaves over coded paths
        if let Some(leaves) = (!island).then(|| contained(c, site, i, mcx)).flatten() {
            if let Some(shim) = gx.catalog.shim(F_TEXTEQ).filter(|s| s.nargs == 2 && s.tyargs.is_empty()) {
                contains_case(c, i, &leaves, shim, mcx);
                u.moved.push((i, c.prog.nodes.len() - 1)); // alloc: Program
                if j == 0 {
                    site.shreds[leaves[0].0].claims += 1;
                    u.claims.push(leaves[0].0); // alloc: Program
                }
            }
            continue;
        }
        let Some((x, n, eq)) = (!island).then(|| coded(c, site, i, mcx)).flatten() else { continue };
        let d = &site.shreds[n]; // a compare beyond `=` / `<>` reads the codes of a dictionary-sealed path only (SRC-30)
        if !eq && !site.out_attnos.get(d.lane as usize).is_some_and(|&a| super::jsonb_lanes::dict_sealed(gx, relid, a, site.table_arg_at(d.path))) {
            continue;
        }
        let p = &mut c.prog;
        let (base, ks, sid) = (p.nodes.len() as u32, p.kids.len() as u32, SiteId(p.sites.len() as u32));
        let (lane, nt, cmp, orig) = (NodeId(base), NodeId(base + 1), NodeId(base + 2), NodeId(base + 3));
        let (at, cmp_node) = match &p.nodes[i] {
            ExprNode::Op(f) => (f.site, ExprNode::Op(FuncNode { site: sid })),
            ExprNode::Func(f) => (f.site, ExprNode::Func(FuncNode { site: sid })),
            ExprNode::ScalarArrayOp(s) => (s.site, ExprNode::ScalarArrayOp(ScalarArrayOp { site: sid, ..*s })),
            _ => continue,
        };
        let call = &p.sites[at.0 as usize];
        let nargs = call.args.len;
        let args: PgVec<'_, NodeId> = (0..nargs).map(|a| if a == x { lane } else { p.kids[(call.args.start + a) as usize] }).collect_in(mcx);
        let site_call = Call { args: Kids { start: ks, len: nargs }, ..copy(call) };
        p.kids.extend_in(args.iter().copied().chain([nt, orig]));
        p.sites.push_in(site_call);
        let case = Case { arg: None, whens: Kids { start: ks + nargs, len: 2 }, default: Some(cmp) };
        let moved = core::mem::replace(&mut p.nodes[i], ExprNode::Case(case));
        let var = Var { src: VarSource::Scan, attno: SHRED_AT + n as i16, typ: TEXT_OID, desc: 0 };
        p.nodes.extend_in([ExprNode::Var(var), ExprNode::NullTest(NullTestNode { arg: lane, is_null: true, row: false, site: None }), cmp_node, moved]);
        u.moved.push((i, orig.0 as usize)); // alloc: Program
        if j == 0 {
            site.shreds[n].claims += 1;
            u.claims.push(n); // alloc: Program
        }
    }
    if u.moved.is_empty() {
        return None;
    }
    fires::fire("jsonb.codes"); // shapecheck fire signal (REGISTRY OIDW-16 / NEW-lowerv4-a2-241..243)
    // every other scan column that is not a code lane is read as its datum (step/percode.rs attach's rule, which its probe
    // needs before attach applies it): the compare's jsonb column where the lane is NULL
    let codes: PgVec<'_, i16> = site.lanes().iter().filter_map(|l| if let LaneId::Attr(a) = l.id { (l.kind == KIND_CODES).then_some(a) } else { None }).collect_in(mcx);
    for (i, n) in c.prog.nodes[..len[0]].iter_mut().enumerate() {
        if let ExprNode::Var(v) = n {
            if v.src == VarSource::Scan && v.attno > 0 && v.attno < SHRED_AT && !codes.contains(&v.attno) {
                v.src = VarSource::Outer;
                u.outer.push(i); // alloc: Program
            }
        }
    }
    Some(u)
}

/// jsonops (3): step `j` of pipeline `k`'s key tests (`?` / `?|` / `?&` of constant keys) over keys with a presence lane
/// on its site (jsonb_lanes `tested`), each rewritten in place as `CASE WHEN p1 IS NULL OR .. THEN <the test> ELSE p1
/// [OR | AND] .. END` (the test moved to the program's last node). A presence row is valid only in a part sealed
/// STREAMF_SHRED_OBJECT_ROOTS and only for a non-NULL document: there every root is an object, so the key test is
/// whether the root holds the key, and the path's lane (exception-free: a JSON null or another type at the path forfeits
/// it) is valid exactly where it does. Any other row (a NULL document, a part without the flag or the path) takes PG's
/// test. Not undone: the presence lanes are plain words (no per-code root), read wherever the step reads its row.
pub(crate) fn presence(px: &mut Px<'_, '_>, k: usize, j: usize, c: &mut Compiled<'_>) {
    use gravity_expr::nodes::bool::BoolNode;
    use types_nodes::primnodes::BoolExprType;
    let mcx = px.mcx();
    if !px.gx.pipelines()[k].ops[..j].iter().all(|op| matches!(op, Op::Filter(_) | Op::Gate(_))) {
        return;
    }
    let Some(site) = px.site_mut(k) else { return };
    if !site.shreds.iter().any(|d| d.kind == shred_kind::PRESENCE) {
        return;
    }
    let mut fired = false;
    for i in 0..c.prog.nodes.len() {
        if c.prog.islands.iter().any(|s| (s.first.0 as usize..=s.root.0 as usize).contains(&i)) {
            continue;
        }
        let Some((form, ns)) = tests(c, site, i, mcx) else { continue };
        let p = &mut c.prog;
        let (mut tests_, mut lanes) = (PgVec::new_in(mcx), PgVec::new_in(mcx));
        for &n in ns.iter() {
            let lane = NodeId(p.nodes.len() as u32);
            p.nodes.push_in(ExprNode::Var(Var { src: VarSource::Scan, attno: SHRED_AT + n as i16, typ: types_core::BOOLOID, desc: 0 }));
            lanes.push_in(lane);
            tests_.push_in(NodeId(p.nodes.len() as u32));
            p.nodes.push_in(ExprNode::NullTest(NullTestNode { arg: lane, is_null: true, row: false, site: None }));
        }
        let mut join = |p: &mut gravity_expr::Program<'_>, ids: &[NodeId], op: BoolExprType| -> NodeId {
            if let [one] = ids {
                return *one;
            }
            let ks = p.kids.len() as u32;
            p.kids.extend_in(ids.iter().copied());
            p.nodes.push_in(ExprNode::Bool(BoolNode { op, args: Kids { start: ks, len: ids.len() as u32 } }));
            NodeId(p.nodes.len() as u32 - 1)
        };
        let cond = join(p, &tests_[..], BoolExprType::OR_EXPR);
        let value = join(p, &lanes[..], if form == Some(false) { BoolExprType::AND_EXPR } else { BoolExprType::OR_EXPR });
        let orig = NodeId(p.nodes.len() as u32);
        let ks = p.kids.len() as u32;
        p.kids.extend_in([cond, orig]);
        let moved = core::mem::replace(&mut p.nodes[i], ExprNode::Case(Case { arg: None, whens: Kids { start: ks, len: 2 }, default: Some(value) }));
        p.nodes.push_in(moved);
        if j == 0 {
            site.shreds[ns[0]].claims += 1;
        }
        fired = true;
    }
    if fired {
        fires::fire("jsonb.codes"); // shapecheck fire signal
    }
}

/// Node `i` of `c` as a key test (typesem `jsonb_key_test`) of a scan jsonb column and a constant key or text[] whose
/// every key has a presence lane on `site`: the form and the keys' shred indexes.
fn tests<'m>(c: &Compiled<'_>, site: &SiteDesc, i: usize, mcx: Mcx<'m>) -> Option<(Option<bool>, PgVec<'m, usize>)> {
    let p = &c.prog;
    let (ExprNode::Op(f) | ExprNode::Func(f)) = &p.nodes[i] else { return None };
    let call = &p.sites[f.site.0 as usize];
    let form = gravity_fn::typesem::pgrc::jsonb_key_test(call.funcid)?;
    if call.args.len != 2 || call.volatile {
        return None;
    }
    let (col, arg) = (p.kids[call.args.start as usize], p.kids[call.args.start as usize + 1]);
    let ExprNode::Var(v) = &p.nodes[col.0 as usize] else { return None };
    let ExprNode::Const(k) = &p.nodes[arg.0 as usize] else { return None };
    if v.src != VarSource::Scan || v.attno <= 0 || v.typ != JSONB_OID {
        return None;
    }
    let keys: Vec<&[u8]> = match form {
        None => vec![crate::ops::scan::pgrc::raw::const_inline(k)?], // alloc: Program
        Some(_) => crate::ops::scan::pgrc::raw::const_text_elems(k).filter(|e| !e.is_empty())?,
    };
    let at = |key: &[u8]| site.shreds.iter().position(|d| d.kind == shred_kind::PRESENCE && site.out_attnos.get(d.lane as usize) == Some(&v.attno) && site.table_arg_at(d.path) == key);
    let mut ns = mcx::reserved_in(keys.len(), mcx);
    for key in keys.iter() {
        ns.push_in(at(key)?);
    }
    Some((form, ns))
}

/// `texteq` (pg_proc 67): a string leaf's compare over its path's codes.
const F_TEXTEQ: u32 = 67;
/// The C collation: `texteq` compares bytes under any deterministic collation.
const C_COLLATION: u32 = 950;

/// Node `i` of `c` as `col @> '<template>'` over a scan jsonb column whose template is an object of string leaves
/// (typesem `contains_string_leaves`), every leaf's path a coded path of `site`: each leaf's shred index and string.
fn contained<'m>(c: &Compiled<'_>, site: &SiteDesc, i: usize, mcx: Mcx<'m>) -> Option<PgVec<'m, (usize, PgVec<'m, u8>)>> {
    let p = &c.prog;
    let (ExprNode::Op(f) | ExprNode::Func(f)) = &p.nodes[i] else { return None };
    let call = &p.sites[f.site.0 as usize];
    if !gravity_fn::typesem::pgrc::jsonb_contains(call.funcid) || call.args.len != 2 || call.volatile {
        return None;
    }
    let (col, tmpl) = (p.kids[call.args.start as usize], p.kids[call.args.start as usize + 1]);
    let ExprNode::Var(v) = &p.nodes[col.0 as usize] else { return None };
    let ExprNode::Const(k) = &p.nodes[tmpl.0 as usize] else { return None };
    if v.src != VarSource::Scan || v.attno <= 0 || v.typ != JSONB_OID || k.typ != JSONB_OID {
        return None;
    }
    let img = crate::ops::scan::pgrc::raw::const_inline(k)?;
    let leaves = gravity_fn::typesem::pgrc::contains_string_leaves(img, gravity_program::cost::pgrc::JSONB_PATH_MAX)?;
    let at = |path: &[u8]| site.shreds.iter().position(|d| d.kind == shred_kind::TEXT_CODES && site.out_attnos.get(d.lane as usize) == Some(&v.attno) && site.table_arg_at(d.path) == path);
    let mut out = mcx::reserved_in(leaves.len(), mcx);
    for (keys, s) in leaves.iter() {
        out.push_in((at(&super::jsonb_lanes::site_path(keys, mcx)?)?, mcx::to_vec_in(&s[..], mcx)));
    }
    Some(out)
}

/// Node `i` (`col @> template`, `contained`'s) rewritten in place as `CASE WHEN l1 IS NULL OR .. THEN <the @>> ELSE
/// l1 = 's1' AND .. END` over the leaves' path lanes, the original call moved to the program's last node. A valid lane row
/// holds a string reached through objects only, so the leaf holds there exactly when the strings are equal (an object
/// pair's values must be of one jsonb type, a string compared by value); a row with an invalid lane (the path absent, a
/// JSON null, a number, boolean or container, a NULL document, a part that did not elect the path) takes PG's `@>`.
fn contains_case(c: &mut Compiled<'_>, i: usize, leaves: &[(usize, PgVec<'_, u8>)], shim: &'static gravity_fn::Shim, mcx: Mcx<'_>) {
    use gravity_expr::nodes::bool::BoolNode;
    use gravity_expr::nodes::func::{ArgConv, How};
    use gravity_expr::nodes::misc::Imm;
    use types_nodes::primnodes::BoolExprType;
    let p = &mut c.prog;
    let (mut tests, mut eqs) = (PgVec::new_in(mcx), PgVec::new_in(mcx));
    for (n, s) in leaves {
        let lane = NodeId(p.nodes.len() as u32);
        p.nodes.push_in(ExprNode::Var(Var { src: VarSource::Scan, attno: SHRED_AT + *n as i16, typ: TEXT_OID, desc: 0 }));
        tests.push_in(NodeId(p.nodes.len() as u32));
        p.nodes.push_in(ExprNode::NullTest(NullTestNode { arg: lane, is_null: true, row: false, site: None }));
        // the leaf's text image (a 4-byte varlena header), owned by the program
        let mut img = Vec::with_capacity(4 + s.len()); // alloc: Program
        img.extend_from_slice(&(((4 + s.len()) as u32) << 2).to_le_bytes());
        img.extend_from_slice(s);
        p.owned.push_in(img); // alloc: Program
        let at = p.owned.last().map_or(0, |b| b.as_ptr() as usize);
        let konst = NodeId(p.nodes.len() as u32);
        // an immediate, not a Const: every Const of a program is a literal of the statement, bound per execution in order
        // (step/consts.rs `place`); the leaf is the template's, which the program bakes (`contained` reads it), and its
        // image lives in `owned` for the program's life (the domain raise's baked names are read the same way)
        p.nodes.push_in(ExprNode::Imm(Imm { value: at as i64, typ: TEXT_OID }));
        let (ks, convs) = (p.kids.len() as u32, p.convs.len() as u32);
        p.kids.extend_in([lane, konst]);
        p.convs.extend_in(shim.args.iter().map(|&kind| ArgConv { kind, typlen: -1 }));
        let sid = SiteId(p.sites.len() as u32);
        p.sites.push_in(Call { funcid: F_TEXTEQ, args: Kids { start: ks, len: 2 }, nargs: 2, strict: true, collation: C_COLLATION, how: How::Shim { shim, convs, byref: shim.region }, rettype: types_core::BOOLOID, volatile: false });
        eqs.push_in(NodeId(p.nodes.len() as u32));
        p.nodes.push_in(ExprNode::Op(FuncNode { site: sid }));
    }
    let mut join = |p: &mut gravity_expr::Program<'_>, ids: &[NodeId], op: BoolExprType| -> NodeId {
        if let [one] = ids {
            return *one;
        }
        let ks = p.kids.len() as u32;
        p.kids.extend_in(ids.iter().copied());
        p.nodes.push_in(ExprNode::Bool(BoolNode { op, args: Kids { start: ks, len: ids.len() as u32 } }));
        NodeId(p.nodes.len() as u32 - 1)
    };
    let (cond, all) = (join(p, &tests[..], BoolExprType::OR_EXPR), join(p, &eqs[..], BoolExprType::AND_EXPR));
    let orig = NodeId(p.nodes.len() as u32);
    let ks = p.kids.len() as u32;
    p.kids.extend_in([cond, orig]);
    let moved = core::mem::replace(&mut p.nodes[i], ExprNode::Case(Case { arg: None, whens: Kids { start: ks, len: 2 }, default: Some(all) }));
    p.nodes.push_in(moved);
}

/// A copy of call site `c` (the lane arm's compare calls what the compare calls, over the lane's text).
fn copy(c: &Call) -> Call {
    Call { funcid: c.funcid, args: c.args, nargs: c.nargs, strict: c.strict, collation: c.collation, how: c.how, rettype: c.rettype, volatile: c.volatile }
}

/// Node `i` of `c` as a compare over a coded path of `site`: the field's argument index and the path's shred index. Any
/// code-verdict compare (`code_verdict`) of the field and a non-NULL constant, either side, or of the field and a
/// non-NULL constant text array (`ANY` / `ALL`).
fn coded(c: &Compiled<'_>, site: &SiteDesc, i: usize, mcx: Mcx<'_>) -> Option<(u32, usize, bool)> {
    let p = &c.prog;
    let (s, any, or) = match &p.nodes[i] {
        ExprNode::Op(f) | ExprNode::Func(f) => (f.site, None, None),
        ExprNode::ScalarArrayOp(s) => (s.site, Some(s.elem.typ), Some(s.use_or)),
        _ => return None,
    };
    let call = &p.sites[s.0 as usize];
    let form = any.is_none_or(|elem| elem == TEXT_OID);
    if !form || call.args.len != 2 || call.volatile || !code_verdict(call.funcid, call.collation) {
        return None;
    }
    let arg = |a: u32| p.kids[(call.args.start + a) as usize];
    let x = if any.is_none() && field(c, arg(0), mcx).is_none() { 1 } else { 0 };
    let ExprNode::Const(k) = &p.nodes[arg(1 - x).0 as usize] else { return None };
    if k.is_null() {
        return None;
    }
    let (attno, key) = field(c, arg(x), mcx)?;
    let n = site.shreds.iter().position(|d| d.kind == shred_kind::TEXT_CODES && site.out_attnos.get(d.lane as usize) == Some(&attno) && site.table_arg_at(d.path) == &*key)?;
    Some((x, n, super::jsonb_lanes::eq_form(call.funcid, or)))
}

/// A two-argument text compare the path's code lane answers per dictionary entry (dict percode.rs: a byte-equality code
/// set, a LIKE verdict, any other boolean body swept once per part), under a collation that compares by bytes: every
/// operator or function but an ILIKE, whose verdict table needs a C ctype (D2680) and otherwise reads the lane as datum.
pub(crate) fn code_verdict(funcid: u32, collation: u32) -> bool {
    crate::step::text_like::deterministic(collation) && !gravity_expr::percode::like_body(funcid).is_some_and(|(ilike, _)| ilike)
}

/// `id` as a constant path of a scan jsonb column read as text (`jsonb_inplace::field`: `->>` or `#>>` over the column
/// or a chain of `->`): the column and its site path (jsonb_lanes `site_path`).
pub(crate) fn field<'m>(c: &Compiled<'_>, id: NodeId, mcx: Mcx<'m>) -> Option<(i16, PgVec<'m, u8>)> {
    let ((src, attno, typ), keys) = super::jsonb_inplace::field(c, id)?;
    if src != VarSource::Scan || attno <= 0 || typ != JSONB_OID {
        return None;
    }
    Some((attno, super::jsonb_lanes::site_path(&keys, mcx)?))
}
