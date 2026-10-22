//! Road `jsonb.lanes`' forwarded form (P1c, lanes/shreddesign/DESIGN.md §3): a path read at a
//! group over the jsonb column its scan's projection forwards. Budget 240 lines.
//!
//! PostgreSQL computes an aggregate's argument above the scan: its projection forwards the bare jsonb column and the
//! argument reads `OUTER.col ->> 'k'` there (`count(DISTINCT data ->> 'did')`, `min(.. (data ->> 'time_us')::bigint)`).
//! STRAIGHT FORM: the argument walks the forwarded image (step/jsonb_inplace.rs), PostgreSQL's evaluation.
//! ENGAGED: `engage` (face/source.rs `site_draft`) declares on the drafted site a path lane for each such read of a path
//! the relation's resident parts elected (`Facts::shred_election`): a `text::bigint` cast of a number path at scale 0 as
//! its int8 lane (jsonb_nums.rs's lane), a `->>` text read of a text path as its text lane (`shred_kind::TEXT`, the
//! string's bytes). The projection carries each lane hidden past its other hidden columns (step/project.rs, `lane_cols`:
//! resno `list.len() + 1 + hidden_reps + image_keys + i`), and `rewrite` turns each read at the group into `CASE WHEN
//! lane IS NULL THEN <the read> ELSE lane END`. A valid text lane row holds the path's JSON string (`->>` is its text); a
//! valid int8 lane row a number of scale 0 that fits 64 bits (jsonb_nums.rs). A NULL lane row (an absent key, a JSON
//! null, another type, a container, a NULL document, a part that did not elect the path) evaluates the read itself over
//! the forwarded column.
//! LATE (`served`, jsonb_nums.rs `late`): a forwarded column the group reads only through such reads of declared lanes
//! (no key over it, no aggregate a numeric fold's tree, no node the expression compiler may host as an island) counts as
//! read by its lanes, so the host fills its image only where a path lane is invalid; `check` refuses a step at the group
//! that keeps a read of it outside a lane's NULL arm.
//!
//! Registers nothing: jsonb.lanes' road gates it, face/source.rs drafts it, step/project.rs carries it, step/expr.rs rewrites.
use gravity_decompose::expr::walk;
use gravity_decompose::{Op, Source};
use gravity_expr::nodes::{case::Case, null::NullTestNode, var::Var};
use gravity_expr::{Compiled, ExprNode, Kids, NodeId, Source as VarSource};
use gravity_program::{shred_kind, SiteDesc};
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::primnodes::OUTER_VAR;
use types_nodes::{Node, NodeTag};

use crate::face::strategy;
use crate::frame::px::Px;
use crate::frame::site::SHRED_AT;
use crate::graph::Gx;
use crate::ops::scan::pgrc::strategies::jsonb_lanes::{field_in, segments, site_path, JSONB_LANES};

/// The most forwarded path lanes one site declares.
const MAX_PATHS: usize = gravity_program::cost::pgrc::JSONB_PATH_LANES_MAX;

/// L0-g: pipeline `k`'s forwarded path reads (`paths`) as path lanes of `site`.
pub(crate) fn engage(px: &Px<'_, '_>, k: usize, site: &mut SiteDesc) {
    for (kind, attno, path) in paths(px.gx, k) {
        let _ = site.add_shred(attno, &path, kind, 0);
    }
}

/// Pipeline `k`'s projection when it is the last op after leading filters / gates and feeds a group (step/project.rs's
/// hidden columns ride it): its op index and entries.
fn feed<'g, 'p>(gx: &Gx<'g, 'p>, k: usize) -> Option<(usize, &'g types_nodes::NodeList<'p>)> {
    let ops = &gx.pipelines()[k].ops;
    let lead = ops.iter().take_while(|op| matches!(op, Op::Filter(_) | Op::Gate(_))).count();
    let Some(Op::Project(list)) = ops.get(lead).filter(|_| lead + 1 == ops.len()) else { return None };
    crate::table::emit_group::fed_group(gx, k, lead).map(|_| (lead, *list))
}

/// Projection entry `r` of pipeline `k` (its feed) as a bare scan Var of a jsonb column: the column.
fn fwd_col(gx: &Gx<'_, '_>, k: usize, r: i16) -> Option<i16> {
    let (Source::Range { scanrelid, .. }, Some((_, list))) = (&gx.pipelines()[k].source, feed(gx, k)) else { return None };
    let te = list.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == r)?;
    let v = te.expr.as_var().filter(|v| v.varno == *scanrelid as i32 && v.varlevelsup == 0 && v.varattno > 0 && v.vartype == types_core::JSONBOID)?;
    Some(v.varattno)
}

/// `n` as a forwarded path read of pipeline `k`'s group: a `text::bigint` cast (int8 lane) or a `->>` / `#>>` text read
/// (text lane) of a constant path of an OUTER column the feed forwards bare: `(kind, column, site path)`.
fn read<'g>(gx: &Gx<'g, '_>, k: usize, n: Node<'_>) -> Option<(u8, i16, PgVec<'g, u8>)> {
    let (kind, at) = match n.as_coerce_via_io() {
        Some(io) if io.resulttype == types_core::INT8OID => (shred_kind::NUMERIC_FS, io.arg),
        Some(_) => return None,
        None => (shred_kind::TEXT, n),
    };
    let (r, path) = field_in(at, OUTER_VAR, gx.mcx)?;
    Some((kind, fwd_col(gx, k, r)?, path))
}

/// Pipeline `k`'s forwarded path reads, in reading order: each read (`read`) its group's aggregates' arguments and
/// FILTERs hold, whose path the relation's parts elected at the read's kind (a number at scale 0, a text).
pub(crate) fn paths<'g>(gx: &Gx<'g, '_>, k: usize) -> PgVec<'g, (u8, i16, PgVec<'g, u8>)> {
    let mcx = gx.mcx;
    let Source::Range { relid, .. } = &gx.pipelines()[k].source else { return PgVec::new_in(mcx) };
    let g = gx.group_at(k).filter(|_| feed(gx, k).is_some());
    if strategy::off().has(JSONB_LANES) || g.is_none() {
        return PgVec::new_in(mcx);
    }
    let mut found: PgVec<'g, (u8, i16, PgVec<'g, u8>)> = PgVec::new_in(mcx);
    for n in g.iter().flat_map(|g| g.aggs.iter()).flat_map(|a| a.filter.iter().copied().chain([a.node])) {
        walk(n, &mut |x| {
            // a cast's own `->>` is the cast's read (the int8 lane), not a text one
            let f = read(gx, k, x).filter(|f| !found.iter().any(|g| g.1 == f.1 && g.2 == f.2));
            if let Some(f) = f.filter(|_| found.len() < MAX_PATHS) {
                found.push_in(f);
            }
            true
        });
    }
    found.retain(|(kind, attno, path)| segments(path, mcx).is_some_and(|keys| gx.facts.shred_election(*relid, *attno, &keys) == Some((*kind, 0))));
    found
}

/// step/project.rs: the path lanes the projection at op `j` of pipeline `k` carries hidden after its other hidden
/// columns, each its row column `SHRED_AT + n` and type (int8 for a number lane, text for a text one).
pub(crate) fn lane_cols<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgVec<'g, (i16, u32)> {
    let mcx = px.mcx();
    if feed(px.gx, k).is_none_or(|(at, _)| at != j) {
        return PgVec::new_in(mcx);
    }
    let reads = paths(px.gx, k);
    let Some(site) = px.site_mut(k) else { return PgVec::new_in(mcx) };
    reads.iter().filter_map(|(kind, attno, path)| shred_of(site, *kind, *attno, path).map(|n| (SHRED_AT + n as i16, if *kind == shred_kind::TEXT { types_core::TEXTOID } else { types_core::INT8OID }))).collect_in(mcx)
}

/// The site's shred `n` of path `path` of column `attno` at `kind` (scale 0).
fn shred_of(site: &SiteDesc, kind: u8, attno: i16, path: &[u8]) -> Option<usize> {
    site.shreds.iter().position(|d| d.kind == kind && d.scale == 0 && site.out_attnos.get(d.lane as usize) == Some(&attno) && site.table_arg_at(d.path) == path)
}

/// P1c: each forwarded path read of a declared lane that step `j` (pipeline `k`'s end: its group's argument) compiles,
/// as the module header's CASE over the lane's hidden column.
pub(crate) fn rewrite(px: &mut Px<'_, '_>, (k, j): (usize, usize), c: &mut Compiled<'_>) {
    let gx = px.gx;
    let mcx = px.mcx();
    if k >= gx.pipelines().len() || j != gx.pipelines()[k].ops.len() || strategy::off().has(JSONB_LANES) {
        return;
    }
    let Some((at, list)) = feed(gx, k) else { return };
    let cols = lane_cols(px, k, at);
    if cols.is_empty() {
        return;
    }
    let hidden = list.len() + crate::table::emit_group::hidden_reps(gx, k, at).len() + crate::table::emit_group::image_keys(gx, k, at).len();
    let Some(site) = px.site_mut(k) else { return };
    for i in 0..c.prog.nodes.len() {
        let island = c.prog.islands.iter().any(|s| (s.first.0 as usize..=s.root.0 as usize).contains(&i));
        let (kind, arg) = match &c.prog.nodes[i] {
            ExprNode::CoerceViaIO(io) if gravity_fn::typesem::pgrc::int8_in(c.prog.sites[io.inp.0 as usize].funcid) => (shred_kind::NUMERIC_FS, io.arg),
            ExprNode::Op(_) | ExprNode::Func(_) => (shred_kind::TEXT, NodeId(i as u32)),
            _ => continue,
        };
        let Some(((VarSource::Outer, r, typ), keys)) = super::jsonb_inplace::field(c, arg).filter(|_| !island) else { continue };
        let (Some(attno), Some(path)) = (fwd_col(gx, k, r).filter(|_| typ == types_core::JSONBOID), site_path(&keys, mcx)) else { continue };
        let Some(n) = shred_of(site, kind, attno, &path) else { continue };
        let Some(h) = cols.iter().position(|&(a, _)| a == SHRED_AT + n as i16) else { continue };
        let p = &mut c.prog;
        let (base, ks) = (p.nodes.len() as u32, p.kids.len() as u32);
        let (lane, nt, orig) = (NodeId(base), NodeId(base + 1), NodeId(base + 2));
        p.kids.extend_in([nt, orig]);
        let moved = core::mem::replace(&mut p.nodes[i], ExprNode::Case(Case { arg: None, whens: Kids { start: ks, len: 2 }, default: Some(lane) }));
        let var = Var { src: VarSource::Outer, attno: (hidden + 1 + h) as i16, typ: cols[h].1, desc: 0 };
        p.nodes.extend_in([ExprNode::Var(var), ExprNode::NullTest(NullTestNode { arg: lane, is_null: true, row: false, site: None }), moved]);
    }
}

/// The node kinds `served` lets an aggregate hold: each compiles to a node jsonb_nums.rs `under_lanes` opens (or a leaf),
/// never to an interpreter island (gravity_expr nodes/mod.rs: XML / JSON forms, `covered`'s MinMax / RowCompare / array
/// coercion / SQL value function), which `rewrite` does not enter.
fn opened(n: Node<'_>) -> bool {
    use NodeTag::*;
    matches!(
        n.node_tag(),
        T_Var | T_Const | T_Param | T_OpExpr | T_FuncExpr | T_DistinctExpr | T_NullIfExpr | T_ScalarArrayOpExpr | T_BoolExpr | T_CoalesceExpr | T_NullTest | T_BooleanTest | T_RelabelType | T_CoerceViaIO | T_CollateExpr | T_CaseExpr | T_CaseWhen | T_CaseTestExpr | T_TargetEntry | T_Aggref | T_SortGroupClause
    )
}

/// LATE: whether projection entry `r` of pipeline `k`'s feed, a bare forwarded jsonb column, is read by the group only
/// through forwarded path reads (`read`) of lanes `site` declares (`rewrite` turns each into the lane's CASE): every OUTER
/// read of `r` in its aggregates' arguments, FILTERs and DISTINCT keys is one, no key is the column itself, no aggregate is
/// a numeric fold's tree (num_keyed.rs binds its leaves to the row), and every node is one `opened` lets through.
pub(crate) fn served(gx: &Gx<'_, '_>, k: usize, site: &SiteDesc, r: i16) -> bool {
    lane_read(gx, k, r, &|kind, a, path| shred_of(site, kind, a, path).is_some())
}

/// D3023 (jsonb_lanes.rs `read_after`, a lowering-time cost gate): `served` before the site is drafted, a read counting
/// as a lane's where `paths` (the parts' elections) holds it.
pub(crate) fn lane_only(gx: &Gx<'_, '_>, k: usize, r: i16) -> bool {
    let elected = paths(gx, k);
    lane_read(gx, k, r, &|kind, a, path| elected.iter().any(|(x, y, z)| *x == kind && *y == a && &**z == path))
}

fn lane_read(gx: &Gx<'_, '_>, k: usize, r: i16, declared: &dyn Fn(u8, i16, &[u8]) -> bool) -> bool {
    let (Some(attno), Some(g)) = (fwd_col(gx, k, r), gx.group_at(k)) else { return false };
    if strategy::off().has(JSONB_LANES) || g.keys.iter().any(|x| x.resno > 0 && crate::table::emit_group::key_col(gx, k, x) == Some(r)) {
        return false;
    }
    let numeric = g.aggs.iter().any(|a| a.node.as_aggref().is_none_or(|x| x.aggtype == types_core::NUMERICOID));
    let lane = |x: Node<'_>| read(gx, k, x).is_some_and(|(kind, a, path)| a == attno && declared(kind, a, &path));
    !numeric && gravity_decompose::facts::demand::group_reads_only_under(g, r, &opened, &lane)
}

/// jsonb_nums.rs `check`: a projection entry at op `j` of pipeline `k` that is the bare column `attno` is the feed's
/// forwarded column, and each of the feed's entries that is that bare column is `served`.
pub(crate) fn bare_served(gx: &Gx<'_, '_>, k: usize, j: usize, site: &SiteDesc, attno: i16) -> bool {
    let Some((_, list)) = feed(gx, k).filter(|(at, _)| *at == j) else { return false };
    let mut bare = list.iter().filter_map(|n| n.as_target_entry()).filter(|te| fwd_col(gx, k, te.resno) == Some(attno)).peekable();
    bare.peek().is_some() && bare.all(|te| served(gx, k, site, te.resno))
}

/// LATE's step check (step/expr.rs, after every rewrite of step `j` of pipeline `k`, its group's arguments and keys at its
/// end): each OUTER read of a forwarded column jsonb_nums.rs `late` guards sits in the NULL arm of a forwarded lane's CASE
/// (`rewrite`'s, over a hidden column past the projection's entries); else the step refuses.
pub(crate) fn check(px: &mut Px<'_, '_>, (k, j): (usize, usize), c: &Compiled<'_>, node: i32) {
    let gx = px.gx;
    if k >= gx.pipelines().len() || j != gx.pipelines()[k].ops.len() {
        return;
    }
    let Some((_, list)) = feed(gx, k) else { return };
    let arena = px.arena;
    let mcx = px.mcx();
    let Some(site) = px.site_mut(k).map(|s| s.copy_in(arena)) else { return };
    let cols = super::jsonb_nums::late(px, k, &site);
    let rs: PgVec<'_, i16> = list.iter().filter_map(|n| n.as_target_entry()).filter(|te| fwd_col(gx, k, te.resno).is_some_and(|a| site.lanes().iter().take(64).enumerate().any(|(l, d)| cols >> l & 1 != 0 && matches!(d.id, gravity_program::LaneId::Attr(x) if x == a)))).map(|te| te.resno).collect_in(mcx);
    if rs.is_empty() {
        return;
    }
    let n = list.len() as i16;
    let outer = |v: &Var| v.src == VarSource::Outer;
    if !super::jsonb_nums::under_lanes(c, &|v| outer(v) && rs.contains(&v.attno), &|v| outer(v) && v.attno > n, mcx) {
        crate::refuse!(node, crate::admit::refusal::Reason::Bug { payload: "a read of a late guarded forwarded jsonb column outside a path lane's NULL arm" });
    }
}
