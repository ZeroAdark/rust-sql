//! Road `jsonb.lanes`' number form (P1b, lanes/shreddesign/DESIGN.md §3; e.g. `(data->>'time_us')::bigint`).
//! Budget 320 lines.
//!
//! STRAIGHT FORM: a `text::bigint` cast of a `->>` / `#>>` path walks the jsonb image in place (step/jsonb_inplace.rs),
//! where a number value at the last key is PostgreSQL's own call, and casts its text: PostgreSQL's evaluation.
//! ENGAGED: `engage` (face/source.rs `site_draft`, after jsonb.lanes' text paths) declares on the drafted site one int8
//! path lane (`shred_kind::NUMERIC_FS` at scale 0, hosts `ShredLane::lane_of`: an int8 word per row) for each such cast
//! of a path the relation's resident parts elected as a number lane at scale 0 (`Facts::shred_election`, a lowering-time
//! read, never a correctness fact) that the pipeline's leading filters or its projection read. `rewrite` turns each such
//! cast into `CASE WHEN lane IS NULL THEN <the cast> ELSE lane END`. A valid lane row holds a jsonb number of scale 0
//! that fits 64 bits (the writer's fixed-scale fit, pgrc_write shred_jsonb.rs: an emitted lane has no exception in its
//! part), whose `->>` text is its digits, so the cast of that text is that number. A NULL lane row (an absent key, a JSON
//! null, a string, a container, a NULL document, a part that did not elect the path at that kind and scale) evaluates
//! the cast itself over the column, errors included.
//! LATE (`late`, source/pgrc_late.rs's guard fill): where a filter then a projection are the pipeline's ops, the filter
//! reads the column only through claimed path compares and every read of it in the projection is such a cast, the host
//! fills its image only where a path lane is invalid, never for the filter's survivors; `rewrite` refuses a projection
//! that keeps a read of such a column outside a cast it rewrote. NOT BUILT HERE: `::numeric` / `::int4` / `::float8`
//! casts and number lanes at a scale.
//!
//! Registers nothing: jsonb.lanes' road gates it (`strategy::off`), face/source.rs drafts it, step/expr.rs rewrites.
use gravity_decompose::expr::walk;
use gravity_decompose::{Op, Source};
use gravity_expr::nodes::{case::Case, null::NullTestNode, var::Var};
use gravity_expr::{Compiled, ExprNode, Kids, NodeId, Source as VarSource};
use gravity_program::{shred_kind, SiteDesc};
use mcx::{CollectIn, GrowIn, Mcx, PgVec};
use types_nodes::Node;

use crate::face::strategy;
use crate::frame::px::Px;
use crate::frame::site::SHRED_AT;
use crate::graph::Gx;
use crate::ops::scan::pgrc::strategies::jsonb_lanes::{field_in, segments, JSONB_LANES};

/// The most number path lanes one site declares (beside jsonb.lanes' text paths).
const MAX_PATHS: usize = gravity_program::cost::pgrc::JSONB_PATH_LANES_MAX;

/// L0-g: pipeline `k`'s number paths (`paths`) as int8 path lanes of `site`.
pub(crate) fn engage(px: &Px<'_, '_>, k: usize, site: &mut SiteDesc) {
    for (attno, path) in paths(px.gx, k) {
        let _ = site.add_shred(attno, &path, shred_kind::NUMERIC_FS, 0);
    }
}

/// Whether step `j` of pipeline `k` reads the scan's row (its scan columns the site's): every op before it a filter or a
/// gate, but for a projection right after them (an aggregate's argument past it reads the scan row: table/args.rs).
fn row_step(gx: &Gx<'_, '_>, k: usize, j: usize) -> bool {
    let ops = &gx.pipelines()[k].ops;
    let lead = ops.iter().take_while(|op| matches!(op, Op::Filter(_) | Op::Gate(_))).count();
    j <= ops.len() && (j <= lead || (j == lead + 1 && matches!(ops.get(lead), Some(Op::Project(_)))))
}

/// Pipeline `k`'s number paths over the scan's jsonb columns, in reading order: each `text::bigint` cast of a path
/// (`cast`) its leading filters, gates or the op after them read, whose key the relation's parts elected as a number
/// lane at scale 0.
pub(crate) fn paths<'g>(gx: &Gx<'g, '_>, k: usize) -> PgVec<'g, (i16, PgVec<'g, u8>)> {
    let mcx = gx.mcx;
    let p = &gx.pipelines()[k];
    let Source::Range { scanrelid, relid, .. } = &p.source else { return PgVec::new_in(mcx) };
    if strategy::off().has(JSONB_LANES) {
        return PgVec::new_in(mcx);
    }
    let mut found: PgVec<'g, (i16, PgVec<'g, u8>)> = PgVec::new_in(mcx);
    let mut see = |n: Node<'_>| {
        walk(n, &mut |x| {
            if let Some(f) = cast(x, *scanrelid as i32, mcx).filter(|f| !found.contains(f) && found.len() < MAX_PATHS) {
                found.push_in(f);
            }
            true
        });
    };
    for (j, op) in p.ops.iter().enumerate().take_while(|&(j, _)| row_step(gx, k, j)) {
        match op {
            Op::Filter(q) | Op::Gate(q) => see(*q),
            Op::Project(list) if j + 1 == p.ops.len() => list.iter().for_each(&mut see),
            _ => {}
        }
    }
    // a group's keys and aggregates' arguments over the scan row (no projection before them)
    for g in gx.group_at(k).filter(|_| row_step(gx, k, p.ops.len())) {
        g.keys.iter().for_each(|x| see(x.expr.node()));
        g.aggs.iter().for_each(|a| a.filter.iter().copied().chain([a.node]).for_each(&mut see));
    }
    found.retain(|(attno, path)| segments(path, mcx).is_some_and(|keys| gx.facts.shred_election(*relid, *attno, &keys) == Some((shred_kind::NUMERIC_FS, 0))));
    found
}

/// `n` as a `text::bigint` cast (a CoerceViaIO to int8) of a constant path of scan `scanrelid`'s jsonb column read as
/// text (jsonb_lanes `field`): the column and the site path.
fn cast<'m>(n: Node<'_>, scanrelid: i32, mcx: Mcx<'m>) -> Option<(i16, PgVec<'m, u8>)> {
    let io = n.as_coerce_via_io().filter(|io| io.resulttype == types_core::INT8OID)?;
    field_in(io.arg, scanrelid, mcx)
}

/// P1b: each `text::bigint` cast of an int8 path lane of pipeline `k`'s drafted site that step `j` reads, as the module
/// header's CASE. Only over a step the scan's rows reach through filters and gates (its scan columns are the site's).
pub(crate) fn rewrite(px: &mut Px<'_, '_>, (k, j): (usize, usize), c: &mut Compiled<'_>) {
    if k >= px.gx.pipelines().len() || !row_step(px.gx, k, j) || strategy::off().has(JSONB_LANES) {
        return;
    }
    let mcx = px.mcx();
    let Some(site) = px.site_mut(k) else { return };
    if !site.shreds.iter().any(|d| d.kind == shred_kind::NUMERIC_FS) {
        return;
    }
    for i in 0..c.prog.nodes.len() {
        let island = c.prog.islands.iter().any(|s| (s.first.0 as usize..=s.root.0 as usize).contains(&i));
        let ExprNode::CoerceViaIO(io) = &c.prog.nodes[i] else { continue };
        if island || !gravity_fn::typesem::pgrc::int8_in(c.prog.sites[io.inp.0 as usize].funcid) {
            continue;
        }
        let Some((attno, key)) = super::jsonb_codes::field(c, io.arg, mcx) else { continue };
        let at = |d: &gravity_program::ShredDecl| d.kind == shred_kind::NUMERIC_FS && d.scale == 0 && site.out_attnos.get(d.lane as usize) == Some(&attno) && site.table_arg_at(d.path) == &*key;
        let Some(n) = site.shreds.iter().position(at) else { continue };
        let p = &mut c.prog;
        let (base, ks) = (p.nodes.len() as u32, p.kids.len() as u32);
        let (lane, nt, orig) = (NodeId(base), NodeId(base + 1), NodeId(base + 2));
        p.kids.extend_in([nt, orig]);
        let moved = core::mem::replace(&mut p.nodes[i], ExprNode::Case(Case { arg: None, whens: Kids { start: ks, len: 2 }, default: Some(lane) }));
        let var = Var { src: VarSource::Scan, attno: SHRED_AT + n as i16, typ: types_core::INT8OID, desc: 0 };
        p.nodes.extend_in([ExprNode::Var(var), ExprNode::NullTest(NullTestNode { arg: lane, is_null: true, row: false, site: None }), moved]);
        if j == 0 {
            site.shreds[n].claims += 1;
        }
    }
}

/// LATE's step check (step/expr.rs, after every rewrite of step `j` of pipeline `k`): a column `late` guards is filled
/// only where a path lane is invalid, so each read of it must sit in the NULL arm of a path lane's CASE (this module's,
/// jsonb.codes'): reached from the root only under `CASE WHEN <path lane> IS NULL THEN ..`; else the step refuses.
pub(crate) fn check(px: &mut Px<'_, '_>, (k, j): (usize, usize), c: &Compiled<'_>, node: i32) {
    if k >= px.gx.pipelines().len() || j == 0 || !row_step(px.gx, k, j) {
        return;
    }
    let arena = px.arena;
    let Some(site) = px.site_mut(k).map(|s| s.copy_in(arena)) else { return };
    let cols = late(px, k, &site);
    let attnos: PgVec<'_, i16> = guarded_attnos(&site, cols).collect_in(px.mcx());
    if attnos.is_empty() {
        return;
    }
    let scan = |v: &Var| v.src == VarSource::Scan;
    // P1c: a projection entry that is the bare column, forwarded to a group that reads it only through path lanes
    let bare = match &c.prog.nodes[c.root.0 as usize] {
        ExprNode::Var(v) if scan(v) && attnos.contains(&v.attno) => Some(v.attno),
        _ => None,
    };
    if bare.is_some_and(|a| super::jsonb_fwd::bare_served(px.gx, k, j, &site, a)) {
        return;
    }
    if !under_lanes(c, &|v| scan(v) && attnos.contains(&v.attno), &|v| scan(v) && v.attno >= SHRED_AT, px.mcx()) {
        crate::refuse!(node, crate::admit::refusal::Reason::Bug { payload: "a read of a late guarded jsonb column outside a path lane's NULL arm" });
    }
}

/// Whether every `Var` of `c` that `target` names sits in the NULL arm of a path lane's CASE: reached from the root only
/// under `CASE WHEN <lane> IS NULL THEN ..` (`lane` names the lane's `Var`, this module's or jsonb_fwd.rs's).
pub(super) fn under_lanes(c: &Compiled<'_>, target: &dyn Fn(&Var) -> bool, lane: &dyn Fn(&Var) -> bool, mcx: Mcx<'_>) -> bool {
    let p = &c.prog;
    let total = p.nodes.iter().filter(|n| matches!(n, ExprNode::Var(v) if target(v))).count();
    let (mut seen, mut bad) = (PgVec::new_in(mcx), false);
    let mut stack = mcx::pg_vec![in mcx; (c.root, false)];
    while let Some((id, under)) = stack.pop() {
        let range = |k: Kids| p.kids[k.start as usize..(k.start + k.len) as usize].iter().map(move |&x| (x, under));
        match &p.nodes[id.0 as usize] {
            ExprNode::Var(v) if target(v) => {
                bad |= !under;
                if !seen.contains(&id) {
                    seen.push_in(id);
                }
            }
            ExprNode::Case(x) => {
                let w = &p.kids[x.whens.start as usize..(x.whens.start + x.whens.len) as usize];
                let lane_null = |t: NodeId| matches!(&p.nodes[t.0 as usize], ExprNode::NullTest(nt) if nt.is_null && matches!(&p.nodes[nt.arg.0 as usize], ExprNode::Var(v) if lane(v)));
                for pair in w.chunks(2) {
                    stack.push_in((pair[0], under));
                    if let Some(&r) = pair.get(1) {
                        stack.push_in((r, under || (x.arg.is_none() && lane_null(pair[0]))));
                    }
                }
                stack.extend_in(x.arg.iter().chain(x.default.iter()).map(|&a| (a, under)));
            }
            ExprNode::Func(f) | ExprNode::Op(f) | ExprNode::Distinct(f) | ExprNode::NullIf(f) => stack.extend_in(range(p.sites[f.site.0 as usize].args)),
            ExprNode::ScalarArrayOp(f) => stack.extend_in(range(p.sites[f.site.0 as usize].args)),
            ExprNode::Bool(b) => stack.extend_in(range(b.args)),
            ExprNode::Coalesce(b) => stack.extend_in(range(b.args)),
            ExprNode::MinMax(b) => stack.extend_in(range(b.args)),
            ExprNode::NullTest(n) => stack.push_in((n.arg, under)),
            ExprNode::BooleanTest(n) => stack.push_in((n.arg, under)),
            ExprNode::Relabel(n) => stack.push_in((n.arg, under)),
            ExprNode::CoerceViaIO(n) => stack.push_in((n.arg, under)),
            ExprNode::Collate(n) => stack.push_in((n.arg, under)),
            _ => {}
        }
    }
    // a read the walk did not reach (below a node it does not open) counts as unguarded
    !bad && seen.len() >= total
}

/// LATE's key check (step/percode_key.rs `compile`): a key over a column `late` guards must have taken the path form
/// (`stash`), whose NULL lane rows alone read the column; any other form reads every row's image, which is not filled.
pub(crate) fn keyed(stash: bool, px: &mut Px<'_, '_>, k: usize, e: Node<'_>, node: i32) {
    let (arena, sr) = (px.arena, px.gx.pipelines()[k].source.relation().map_or(0, |r| r.0 as i32));
    let Some(site) = px.site_mut(k).map(|s| s.copy_in(arena)).filter(|_| !stash) else { return };
    let col = field_in(e.as_target_entry().map_or(e, |te| te.expr), sr, px.mcx()).and_then(|f| site.lanes().iter().take(64).position(|l| l.id == gravity_program::LaneId::Attr(f.0)));
    if col.is_some_and(|l| late(px, k, &site) >> l & 1 != 0) {
        crate::refuse!(node, crate::admit::refusal::Reason::Bug { payload: "a path group key over a late guarded jsonb column that did not key by the path's code lane" });
    }
}

/// The attnos of the site lanes in `cols` (bit l: site lane l).
fn guarded_attnos(site: &SiteDesc, cols: u64) -> impl Iterator<Item = i16> + '_ {
    site.lanes().iter().take(64).enumerate().filter(move |(l, _)| cols >> l & 1 != 0).filter_map(|(_, d)| if let gravity_program::LaneId::Attr(a) = d.id { Some(a) } else { None })
}

/// LATE: the jsonb column lanes of pipeline `k`'s drafted `site` (bit l: site lane l) whose every read in its projection is
/// a cast of a declared int8 path (`cast`): the ops a filter then that projection; no group key keyed by its fingerprint
/// (step/percode_key.rs compiles it with no step) and no aggregate reads the column. source/pgrc_late.rs fills such a
/// late column only where a path lane is invalid (its filter reads it, if at all, through claimed compares only).
pub(crate) fn late(px: &Px<'_, '_>, k: usize, site: &SiteDesc) -> u64 {
    if strategy::off().has(JSONB_LANES) || !site.shreds.iter().any(|d| d.kind == shred_kind::NUMERIC_FS) && super::jsonb_fwd::paths(px.gx, k).is_empty() {
        return 0;
    }
    late_cols(px, k, site)
}

fn late_cols<'g>(px: &Px<'g, '_>, k: usize, site: &SiteDesc) -> u64 {
    use gravity_abi::breaker::KeyKind;
    let gx = px.gx;
    let mcx = px.mcx();
    let p = &gx.pipelines()[k];
    let lead = p.ops.iter().take_while(|op| matches!(op, Op::Filter(_))).count();
    let list = match p.ops.get(lead) {
        Some(Op::Project(list)) => Some(list),
        _ => None,
    };
    let Source::Range { scanrelid, relid, .. } = &p.source else { return 0 };
    if lead == 0 || lead + list.is_some() as usize != p.ops.len() {
        return 0;
    }
    let (sr, n) = (*scanrelid as i32, site.natts().min(64));
    let lane = |a: i16| site.lanes()[..n].iter().position(|l| l.id == gravity_program::LaneId::Attr(a));
    let declared = |f: &(i16, PgVec<'g, u8>)| site.shreds.iter().any(|d| d.kind == shred_kind::NUMERIC_FS && d.scale == 0 && site.out_attnos.get(d.lane as usize) == Some(&f.0) && site.table_arg_at(d.path) == &*f.1);
    let coded = |f: &(i16, PgVec<'g, u8>)| site.shreds.iter().any(|d| d.kind == shred_kind::TEXT_CODES && site.out_attnos.get(d.lane as usize) == Some(&f.0) && site.table_arg_at(d.path) == &*f.1);
    let (mut reads, mut cast_reads, mut bad) = ([0u32; 64], [0u32; 64], 0u64);
    // `fp`: a fingerprint key not in the path form (any read bad); `keyed`: one in it (each read lane-served)
    let mut count = |e: Node<'_>, fp: bool, keyed: bool| {
        let known = walk(e, &mut |x| {
            if let Some(v) = x.as_var().filter(|v| v.varno == sr && v.varlevelsup == 0) {
                match lane(v.varattno) {
                    Some(l) if fp => bad |= 1 << l,
                    Some(l) => (reads[l], cast_reads[l]) = (reads[l] + 1, cast_reads[l] + keyed as u32),
                    None => {}
                }
            }
            if let Some(l) = cast(x, sr, mcx).filter(|f| declared(f)).or_else(|| coded_compare(gx, *relid, x, sr).filter(|f| coded(f))).and_then(|f| lane(f.0)) {
                cast_reads[l] += 1;
            }
            true
        });
        if !known {
            bad = u64::MAX;
        }
    };
    // the filters past the first: each read a compare jsonb.codes rewrites over a declared text path (the first's reads
    // are source/pgrc_late.rs's: claimed compares only, else the column is not late)
    for op in &p.ops[1..lead] {
        if let Op::Filter(q) = op {
            count(*q, false, false);
        }
    }
    let g = gx.group_at(k);
    // an Fp128 key keyed by a text path's code lane (strategies/jsonb_keys.rs `path`: the path form, `keyed` below) reads
    // the column only where that lane is invalid, as a cast does
    for (r, te) in list.iter().flat_map(|l| l.iter()).filter_map(|x| x.as_target_entry()).enumerate() {
        let key = g.and_then(|g| g.keys.iter().find(|x| x.kind == KeyKind::Fp128 && crate::table::emit_group::key_col(gx, k, x) == Some(r as i16 + 1)));
        // jsonb_keys `path`'s own tests: `key_path` (a declared path, no emit sorting by keys: D1730), no aggregate reading the key (D1890)
        let agg_reads = |r: i16| g.is_some_and(|g| g.aggs.iter().any(|d| super::jsonb_keys::agg_reads(d.node, r)));
        let path = key.filter(|x| gx.facts.coll(x.coll).bytes_eq && crate::dict::face::PerValue::Table.on() && crate::dict::strategies::percode_key::key_only(px, k, lead, r as i16 + 1).is_some() && super::jsonb_keys::key_path(gx, k, x).is_some() && !agg_reads(r as i16 + 1));
        let keyed = path.and_then(|_| field_in(te.expr, sr, mcx)).is_some_and(|f| coded(&f));
        // P1c: a bare column the group reads only through its forwarded path lanes (jsonb_fwd.rs `served`)
        let fwd = super::jsonb_fwd::served(gx, k, site, te.resno);
        count(te.expr, key.is_some() && !keyed && !fwd, keyed || fwd);
    }
    // no projection: the keys read the scan row, compiled in the path form or with no step (table/args.rs, by its
    // `image_only`): a read of the column there is not counted as a lane's
    for x in g.iter().flat_map(|g| g.keys.iter()).filter(|_| list.is_none()) {
        count(x.expr.node(), true, false);
    }
    // an aggregate's argument over the scan row compiles at the pipeline's end (table/args.rs `compile_args_at`), but a
    // numeric fold's tree (num_keyed.rs), whose leaves this module does not see
    for a in g.iter().flat_map(|g| g.aggs.iter()) {
        let tree = a.node.as_aggref().is_none_or(|r| r.aggtype == types_core::NUMERICOID);
        count(a.node, tree, false);
        if let Some(f) = a.filter {
            count(f, false, false);
        }
    }
    (0..n).filter(|&l| bad >> l & 1 == 0 && reads[l] > 0 && reads[l] == cast_reads[l]).fold(0, |m, l| m | 1 << l)
}

/// `n` as a compare jsonb.codes rewrites over a text path's code lane (jsonb_lanes `compared`, restated: an IMMUTABLE
/// code-verdict compare of the path and a constant, or `ANY` / `ALL` of a constant text array; beyond `=` / `<>` only over a
/// dictionary-sealed path): the column and the site path.
fn coded_compare<'g>(gx: &Gx<'g, '_>, relid: u32, n: Node<'_>, scanrelid: i32) -> Option<(i16, PgVec<'g, u8>)> {
    use super::jsonb_codes::code_verdict;
    use super::jsonb_lanes::{dict_sealed, eq_form};
    let pure = |f: u32| f != 0 && gx.catalog.func(f).is_ok_and(|x| x.immutable());
    let (a, eq) = if let Some(s) = n.as_scalar_array_op_expr() {
        let [a, b] = s.args.as_slice() else { return None };
        let list = b.as_const().is_some_and(|c| c.consttype == types_core::TEXTARRAYOID);
        (list && pure(s.opfuncid) && code_verdict(s.opfuncid, s.inputcollid)).then_some((*a, eq_form(s.opfuncid, Some(s.useOr))))?
    } else {
        let (f, rtype, coll, args) = match (n.as_op_expr(), n.as_func_expr()) {
            (Some(o), _) => (o.opfuncid, o.opresulttype, o.inputcollid, &o.args),
            (_, Some(f)) => (f.funcid, f.funcresulttype, f.inputcollid, &f.args),
            _ => return None,
        };
        if rtype != types_core::BOOLOID || !pure(f) || !code_verdict(f, coll) {
            return None;
        }
        match args.as_slice() {
            [a, b] if a.as_const().is_some() => (*b, eq_form(f, None)),
            [a, b] if b.as_const().is_some() => (*a, eq_form(f, None)),
            _ => return None,
        }
    };
    field_in(a, scanrelid, gx.mcx).filter(|(attno, path)| eq || dict_sealed(gx, relid, *attno, path))
}
