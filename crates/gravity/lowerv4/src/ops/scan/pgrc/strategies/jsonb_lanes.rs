//! Road `jsonb.lanes` (H02g, R13-L01 L02 L04 L12: path lanes as site lanes, read by source/pgrc.rs, L0-g; L4 straight,
//! L6 engaged) (cards/M6.md L4, M6-plan §3.10). Owner M6-JSONB. Budget 220 lines.
//!
//! The road applies to a pgrc statement over a jsonb column (`Applies::JsonbPgrc`).
//! STRAIGHT FORM: registering the road serves those statements at their other
//! kinds' forms. A `->` / `->>` chain reads the jsonb value the pgrc source hands the row and walks it in place
//! (step/jsonb_inplace.rs, road jsonb.inplace, M2), which is PostgreSQL's own evaluation, so the rows are PG's.
//! ENGAGED, TEXT PATHS AS CODES (M6 R3; D2a's plumbing, V4R-327 measured): `engage` (L0-g, registered as the `SiteDraft`
//! face) declares on the drafted site, before its quals compile, one shredded path lane (`LaneId::JsonbPath(n)`,
//! `SiteDesc::add_shred`) per `col ->> 'key'` a leading filter compares (`compared`; a compare beyond `=` / `<>` only
//! where every electing part sealed the path as a dictionary, `dict_sealed`) whose key the relation's resident
//! parts elected as a Text lane (`Facts::shred_election`, a lowering-time read, never a correctness fact), as the path's
//! code lane (`shred_kind::TEXT_CODES`, with jsonb.codes and percode.table on). The host publishes the lane from each
//! part that elected the path at that kind, all-invalid from any other (hosts `Faces::locate`); jsonb.codes' rewrite
//! (source/jsonb_codes.rs) reads a valid row's code from it and evaluates an invalid row's compare over the column, so
//! every row is PG's `->>` by construction. A column the first filter reads only through those compares is late
//! (source/pgrc_late.rs): its image is filled where a path lane is invalid (hosts `guard_fill`) and for the survivors.
//! jsonops (1): a path is any constant path read as text (`field`): `col ->> 'k'`, a chain of `->` ending in `->>`, or
//! `col #>> '{a,b}'`. All three name one lane, the keys outermost first (`site_path`; exec scan/sites.rs hands them to
//! the hosts' `shred_path`, the one encoding the writer seals with).
//! NOT BUILT HERE: NumericFs / Bool / Uuid16 path lanes and Decision 8b's PackedNum (D2b, parked), zone / bloom / PSMA
//! verdicts on a lane's stats (L04).
//!
//! Registers: road `jsonb.lanes` (`JSONB_LANES`): `SiteDraft(engage)`, then `PathKey` (source/jsonb_keys.rs).
use gravity_abi::breaker::KeyKind;
use gravity_decompose::expr::{const_bytes, walk};
use gravity_decompose::{Op, Source};
use gravity_program::{shred_kind, SiteDesc};
use mcx::{CollectIn, GrowIn, Mcx, PgVec};
use types_nodes::Node;

use crate::dict::face::PathKey;
use crate::face::strategy;
use crate::face::source::SiteDraft;
use crate::frame::px::Px;
use crate::graph::Gx;
use crate::ops::Table;

/// `SiteDraft` first: face/source.rs `site_draft` reads the road's first row (the group key form's `PathKey` after it, face/codes.rs).
const KEYS: PathKey = PathKey {
    path: crate::ops::scan::pgrc::strategies::jsonb_keys::path,
    key: crate::ops::scan::pgrc::strategies::jsonb_keys::key_path,
    late: crate::ops::scan::pgrc::strategies::jsonb_keys::late,
    keyed: crate::ops::scan::pgrc::strategies::jsonb_keys::keyed,
};
crate::strategies! {
    /// Path lanes as site lanes, zone on lanes, bind witness.
    JSONB_LANES = 86, "jsonb.lanes", JsonbPgrc;
}

pub(crate) const ROWS: Table = Table { strategies: &[(JSONB_LANES, &SiteDraft(engage)), (JSONB_LANES, &KEYS)], ..crate::ops::NONE };

/// The most path lanes one site declares (the cost home's choice).
const MAX_PATHS: usize = gravity_program::cost::pgrc::JSONB_PATH_LANES_MAX;

/// L0-g: pipeline `k`'s Text paths (`paths`) as path lanes of `site`, shred `n` the `n`-th.
fn engage(px: &Px<'_, '_>, k: usize, site: &mut SiteDesc) {
    for (attno, path) in paths(px.gx, k) {
        let _ = site.add_shred(attno, &path, shred_kind::TEXT_CODES, 0);
    }
    // jsonops (3): each key a leading filter's `?` / `?|` / `?&` tests, as the key's presence lane (after the Text paths:
    // a Text path's shred index is its place in `paths`)
    for (attno, key) in tested(px.gx, k) {
        let _ = site.add_shred(attno, &key, shred_kind::PRESENCE, 0);
    }
}

/// Pipeline `k`'s Text paths over the scan's jsonb columns, in declaration order: each `col ->> 'key'` its leading filters
/// compare, then (V4R-468: strategies/jsonb_keys.rs) each `Fp128` group key that is one, whose key the relation's
/// parts elected as a Text lane. keyed/emit_group_nocode.rs finds a key's shred by its place here.
pub(crate) fn paths<'g>(gx: &Gx<'g, '_>, k: usize) -> PgVec<'g, (i16, PgVec<'g, u8>)> {
    // R3: a Text path is declared as its code lane, which only jsonb.codes' compares and jsonb_keys' key read
    use crate::dict::face::{CodeRep, PerValue};
    let mcx = gx.mcx;
    let p = &gx.pipelines()[k];
    let Source::Range { scanrelid, relid, .. } = &p.source else { return PgVec::new_in(mcx) };
    if strategy::off().has(JSONB_LANES) || !CodeRep::Jsonb.on() || !PerValue::Table.on() {
        return PgVec::new_in(mcx);
    }
    let mut found: PgVec<'g, (i16, PgVec<'g, u8>)> = PgVec::new_in(mcx);
    let mut add = |f: Option<(i16, PgVec<'g, u8>)>| {
        if let Some(f) = f.filter(|f| !found.contains(f) && found.len() < MAX_PATHS) {
            found.push_in(f);
        }
    };
    for q in p.ops.iter().map_while(|op| if let Op::Filter(q) = op { Some(*q) } else { None }) {
        walk(q, &mut |n: Node<'_>| {
            // jsonops (1): a chain's compare (a path of two keys or more) only where the rows past the filters read no
            // document: there each survivor's document is decoded anyway, its in-place walk costs less than the lanes.
            // sortpath: or over a path the relation is sorted by (`shred_sorted`), whose lane's zones skip granules, where no
            // aggregate reads the document (`agg_read`: there the lane costs each window more than its skips save: a grouped count of a distinct path beside two compares)
            let sorted = |f: &(i16, PgVec<'g, u8>)| segments(&f.1, mcx).is_some_and(|keys| gx.facts.shred_sorted(*relid, f.0, &keys)) && !agg_read(gx, k, *scanrelid as i32, f.0);
            let gate = |f: &(i16, PgVec<'g, u8>)| !f.1.contains(&0) || !read_after(gx, k, *scanrelid as i32, *relid, f.0, true) || sorted(f);
            add(compared(gx, n).and_then(|(a, eq)| field_in(a, *scanrelid as i32, mcx).filter(|(attno, path)| eq || dict_sealed(gx, *relid, *attno, path))).filter(gate));
            // jsonops (2): each string leaf of a `col @> '{..}'` template, an `=` over its path (jsonb_codes `contained`)
            for leaf in contained(n, *scanrelid as i32, mcx).into_iter().flatten() {
                add(Some(leaf).filter(gate));
            }
            true
        });
    }
    for key in gx.group_at(k).iter().flat_map(|g| g.keys.iter()).filter(|x| x.kind == KeyKind::Fp128 && x.resno > 0) {
        add(field_in(key.expr.node(), *scanrelid as i32, mcx));
    }
    found.retain(|(attno, path)| segments(path, mcx).is_some_and(|keys| gx.facts.shred_election(*relid, *attno, &keys) == Some((shred_kind::TEXT, 0))));
    found
}

/// Whether pipeline `k` reads jsonb column `attno` of scan `scanrelid` (relation `relid`) past its leading filters: a
/// projection not feeding a group, a later filter or gate, a group key (with `lane_keys`, not one that is a path the
/// parts elected as a Text lane: `paths` reads that from its lane), an aggregate's argument or FILTER; any other op
/// counts as a read. Then each survivor's document is decoded anyway, and a lane of a filter compare (or a presence
/// lane) saves no document read but costs its own decode. Speed only: either way every compare is PostgreSQL's.
fn read_after(gx: &Gx<'_, '_>, k: usize, scanrelid: i32, relid: u32, attno: i16, lane_keys: bool) -> bool {
    reads_doc(gx, k, scanrelid, relid, attno, lane_keys, false)
}

/// sortpath: whether an aggregate of pipeline `k`'s group (its argument or FILTER) reads jsonb column `attno` of scan
/// `scanrelid`, as `read_after` counts the aggregates' reads.
fn agg_read(gx: &Gx<'_, '_>, k: usize, scanrelid: i32, attno: i16) -> bool {
    reads_doc(gx, k, scanrelid, 0, attno, false, true)
}

/// `read_after`'s walk; `aggs_only`: the aggregates' reads alone (the group's, or an ungrouped aggregate's projection).
fn reads_doc(gx: &Gx<'_, '_>, k: usize, scanrelid: i32, relid: u32, attno: i16, lane_keys: bool, aggs_only: bool) -> bool {
    let (p, group) = (&gx.pipelines()[k], gx.group_at(k));
    // the projection feeding a group: an aggregate's argument names its entries (OUTER_VAR), not the scan's columns
    let fed = group.and(p.ops.iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None }));
    let scan_read = |n: Node<'_>| {
        let mut hit = false;
        walk(n, &mut |x: Node<'_>| {
            hit |= x.as_var().is_some_and(|v| v.varno == scanrelid && v.varlevelsup == 0 && (v.varattno == attno || v.varattno == 0));
            !hit
        });
        hit
    };
    let mut hit = false;
    let mut see = |n: Node<'_>| {
        walk(n, &mut |x: Node<'_>| {
            hit |= x.as_var().is_some_and(|v| {
                v.varlevelsup == 0
                    && (v.varno == scanrelid && (v.varattno == attno || v.varattno == 0)
                        || v.varno == types_nodes::primnodes::OUTER_VAR && fed.map_or(true, |l| l.iter().nth(v.varattno.max(1) as usize - 1).map_or(true, |e| v.varattno <= 0 || scan_read(e) && !super::jsonb_fwd::lane_only(gx, k, v.varattno))))
            });
            !hit
        });
    };
    // an ungrouped aggregate's input is the projection under it (no group names its reads)
    let plain = group.is_none() && matches!(p.end(), gravity_program::End::Breaker(b) if matches!(b.kind, gravity_abi::breaker::BreakerKind::PlainAgg | gravity_abi::breaker::BreakerKind::HashAgg));
    for op in p.ops.iter().skip_while(|op| matches!(op, Op::Filter(_))).filter(|op| !aggs_only || plain && matches!(op, Op::Project(_))) {
        match op {
            Op::Filter(q) | Op::Gate(q) => see(*q),
            // a projection feeding a group is its input row (PostgreSQL's physical tlist names every column): the group's
            // keys and aggregates below say what it reads
            Op::Project(_) if group.is_some() => {}
            Op::Project(l) => l.iter().for_each(&mut see),
            _ => return true,
        }
    }
    if let Some(g) = group {
        let lane = |n: Node<'_>| field_in(n, scanrelid, gx.mcx).is_some_and(|(a, path)| a == attno && segments(&path, gx.mcx).is_some_and(|keys| gx.facts.shred_election(relid, a, &keys) == Some((shred_kind::TEXT, 0))));
        g.keys.iter().filter(|x| !aggs_only && !(lane_keys && x.kind == KeyKind::Fp128 && lane(x.expr.node()))).for_each(|x| see(x.expr.node()));
        g.aggs.iter().for_each(|a| {
            see(a.node);
            a.filter.into_iter().for_each(&mut see);
        });
    }
    hit
}

/// jsonops (3): pipeline `k`'s keys its leading filters test with `?` / `?|` / `?&` (`key_test`) whose path the relation's
/// parts elected as a Text lane, in order, at most `MAX_PATHS` shreds in all.
pub(crate) fn tested<'g>(gx: &Gx<'g, '_>, k: usize) -> PgVec<'g, (i16, PgVec<'g, u8>)> {
    let mcx = gx.mcx;
    let p = &gx.pipelines()[k];
    let Source::Range { scanrelid, relid, .. } = &p.source else { return PgVec::new_in(mcx) };
    if strategy::off().has(JSONB_LANES) {
        return PgVec::new_in(mcx);
    }
    let budget = MAX_PATHS.saturating_sub(paths(gx, k).len());
    let mut found: PgVec<'g, (i16, PgVec<'g, u8>)> = PgVec::new_in(mcx);
    for q in p.ops.iter().map_while(|op| if let Op::Filter(q) = op { Some(*q) } else { None }) {
        walk(q, &mut |n: Node<'_>| {
            for f in key_test(n, *scanrelid as i32, mcx).map(|(_, f)| f).into_iter().flatten() {
                if !found.contains(&f) && found.len() < budget {
                    found.push_in(f);
                }
            }
            true
        });
    }
    found.retain(|(attno, key)| segments(key, mcx).is_some_and(|keys| gx.facts.shred_election(*relid, *attno, &keys) == Some((shred_kind::TEXT, 0)) && gx.facts.shred_object_roots(*relid, *attno, &keys)) && !read_after(gx, k, *scanrelid as i32, *relid, *attno, false));
    found
}

/// `n` as a key test of scan `scanrelid`'s jsonb column (typesem `jsonb_key_test`) of a constant key or a constant text[]
/// with no NULL element: the form (`None` for `?`, `Some(any)` for `?|` / `?&`) and each key's column and site path.
pub(crate) fn key_test<'m>(n: Node<'_>, scanrelid: i32, mcx: Mcx<'m>) -> Option<(Option<bool>, PgVec<'m, (i16, PgVec<'m, u8>)>)> {
    let (f, args) = match (n.as_op_expr(), n.as_func_expr()) {
        (Some(o), _) => (o.opfuncid, &o.args),
        (_, Some(f)) => (f.funcid, &f.args),
        _ => return None,
    };
    let form = gravity_fn::typesem::pgrc::jsonb_key_test(f)?;
    let [a, b] = args.as_slice() else { return None };
    let v = a.as_var().filter(|v| v.varno == scanrelid && v.varlevelsup == 0 && v.varattno > 0 && v.vartype == types_core::JSONBOID)?;
    let keys: PgVec<'_, PgVec<'m, u8>> = match form {
        None => mcx::pg_vec![in mcx; mcx::to_vec_in(const_bytes(b.as_const()?)?, mcx)],
        Some(_) => text_elems(b.as_const()?, mcx).filter(|e| !e.is_empty())?,
    };
    let mut ks = mcx::reserved_in(keys.len(), mcx);
    for k in keys.iter() {
        ks.push_in((v.varattno, site_path(core::slice::from_ref(k), mcx)?));
    }
    Some((form, ks))
}

/// The operand of `n` a jsonb.codes compare may read as a path (source/jsonb_codes.rs `coded`): an IMMUTABLE boolean
/// code-verdict compare (`jsonb_codes::code_verdict`) of a constant, either side, or `ANY` / `ALL` of a constant text
/// array, the left one; and whether it is an `=` / `<>` form (`eq_form`).
fn compared<'p>(gx: &Gx<'_, 'p>, n: Node<'p>) -> Option<(Node<'p>, bool)> {
    use crate::ops::scan::pgrc::strategies::jsonb_codes::code_verdict;
    let pure = |f: u32| f != 0 && gx.catalog.func(f).is_ok_and(|x| x.immutable());
    if let Some(s) = n.as_scalar_array_op_expr() {
        let [a, b] = s.args.as_slice() else { return None };
        let list = b.as_const().is_some_and(|c| c.consttype == types_core::TEXTARRAYOID);
        return (list && pure(s.opfuncid) && code_verdict(s.opfuncid, s.inputcollid)).then_some((*a, eq_form(s.opfuncid, Some(s.useOr))));
    }
    let (f, rtype, coll, args) = match (n.as_op_expr(), n.as_func_expr()) {
        (Some(o), _) => (o.opfuncid, o.opresulttype, o.inputcollid, &o.args),
        (_, Some(f)) => (f.funcid, f.funcresulttype, f.inputcollid, &f.args),
        _ => return None,
    };
    if rtype != types_core::BOOLOID || !pure(f) || !code_verdict(f, coll) {
        return None;
    }
    match args.as_slice() {
        [a, b] if a.as_const().is_some() => Some((*b, eq_form(f, None))),
        [a, b] if b.as_const().is_some() => Some((*a, eq_form(f, None))),
        _ => None,
    }
}

/// The `=` / `<>` forms (texteq / textne of a constant, `= ANY` / `<> ALL` of a constant array; `any_or`: a
/// `ScalarArrayOp`'s `useOr`), whose code set is looked up in a part's byte-ordered entries: they read a path's codes
/// over any seal. Every other compare is a per-code verdict filled once an entry, so it reads them only over a path the
/// parts sealed as a dictionary (`dict_sealed`).
pub(crate) fn eq_form(funcid: u32, any_or: Option<bool>) -> bool {
    let ne = gravity_fn::typesem::pgrc::text_eq_ne(funcid);
    any_or.map_or(ne.is_some(), |or| ne == Some(!or))
}

/// SRC-30: path `key` of jsonb column `attno` of `relid` is the writer's dictionary in every part that elected it
/// (`Facts::shred_dict`). A verbatim path's code lane is the part's byte ranks, about an entry a value, ranked at each
/// open: a per-code verdict over it ran 1.65x the in-place walk at W8 (a path unique per row, 2M rows, QUIET 53).
pub(crate) fn dict_sealed(gx: &Gx<'_, '_>, relid: u32, attno: i16, path: &[u8]) -> bool {
    segments(path, gx.mcx).is_some_and(|keys| gx.facts.shred_dict(relid, attno, &keys))
}

/// The keys of a site path (`SiteDesc::add_shred`'s bytes: the keys, outermost first, joined by a NUL byte, which no
/// jsonb key holds; one key is its own bytes): exec scan/sites.rs hands them to the hosts' `shred_path`, the one encoding.
pub(crate) fn segments<'m, 'a>(path: &'a [u8], mcx: Mcx<'m>) -> Option<PgVec<'m, &'a str>> {
    let mut keys = PgVec::new_in(mcx);
    for k in path.split(|&b| b == 0) {
        keys.push_in(core::str::from_utf8(k).ok()?);
    }
    Some(keys)
}

/// `keys` (outermost first) as a site path (`segments`' form).
pub(crate) fn site_path<'m, K: AsRef<[u8]>>(keys: &[K], mcx: Mcx<'m>) -> Option<PgVec<'m, u8>> {
    if keys.is_empty() || keys.iter().any(|k| k.as_ref().contains(&0)) {
        return None;
    }
    // the keys joined by a NUL byte
    let mut out: PgVec<'m, u8> = mcx::reserved_in(keys.iter().map(|k| k.as_ref().len() + 1).sum::<usize>() - 1, mcx);
    for (i, k) in keys.iter().enumerate() {
        if i > 0 {
            out.push_in(0u8);
        }
        out.extend_from_slice_in(k.as_ref());
    }
    Some(out)
}

/// `n` as a constant path of scan `scanrelid`'s jsonb column read as text: `->>` (jsonb_object_field_text) of a key or
/// `#>>` (jsonb_extract_path_text) of a constant text[] with no NULL element, over the column or a chain of `->`
/// (jsonb_object_field) of constant keys over it (1..=JSONB_PATH_MAX keys): its attno and the site path (`site_path`). A
/// path lane holds a string reached through objects only (the writer never descends an array), so a `#>>` element an
/// array level would read as an index reaches no lane row: such a row's lane is invalid and its compare walks the column.
pub(crate) fn field_in<'m>(n: Node<'_>, scanrelid: i32, mcx: Mcx<'m>) -> Option<(i16, PgVec<'m, u8>)> {
    const PATH_MAX: usize = gravity_program::cost::pgrc::JSONB_PATH_MAX;
    fn call(n: Node<'_>) -> Option<(u32, [Node<'_>; 2])> {
        let (f, args) = match (n.as_op_expr(), n.as_func_expr()) {
            (Some(o), _) => (o.opfuncid, &o.args),
            (_, Some(f)) if !f.funcvariadic || gravity_fn::typesem::pgrc::jsonb_path_text(f.funcid) => (f.funcid, &f.args),
            _ => return None,
        };
        let [a, b] = args.as_slice() else { return None };
        Some((f, [*a, *b]))
    }
    let (f, [mut at, tail]) = call(n)?;
    let path = gravity_fn::typesem::pgrc::jsonb_path_text(f);
    if !path && !gravity_fn::typesem::pgrc::jsonb_field_text(f) {
        return None;
    }
    let mut chain: PgVec<'_, PgVec<'m, u8>> = PgVec::new_in(mcx);
    let v = loop {
        if let Some(v) = at.as_var() {
            break v;
        }
        let (_, [a, k]) = call(at).filter(|(g, _)| gravity_fn::typesem::pgrc::jsonb_field(*g) && chain.len() < PATH_MAX)?;
        chain.push_in(mcx::to_vec_in(const_bytes(k.as_const()?)?, mcx));
        at = a;
    };
    if v.varno != scanrelid || v.varlevelsup != 0 || v.varattno <= 0 || v.vartype != types_core::JSONBOID {
        return None;
    }
    chain.reverse();
    if path {
        let elems = text_elems(tail.as_const()?, mcx)?;
        if elems.is_empty() {
            return None; // an empty path is the whole document as text, not a field
        }
        chain.extend_in(elems);
    } else {
        chain.push_in(mcx::to_vec_in(const_bytes(tail.as_const()?)?, mcx));
    }
    if chain.len() > PATH_MAX {
        return None;
    }
    Some((v.varattno, site_path(&chain[..], mcx)?))
}

/// `n` as `col @> '<template>'` over scan `scanrelid`'s jsonb column whose template is an object of string leaves
/// (typesem `contains_string_leaves`): each leaf's column and site path.
pub(crate) fn contained<'m>(n: Node<'_>, scanrelid: i32, mcx: Mcx<'m>) -> Option<PgVec<'m, (i16, PgVec<'m, u8>)>> {
    let (f, args) = match (n.as_op_expr(), n.as_func_expr()) {
        (Some(o), _) => (o.opfuncid, &o.args),
        (_, Some(f)) => (f.funcid, &f.args),
        _ => return None,
    };
    let [a, b] = args.as_slice() else { return None };
    if !gravity_fn::typesem::pgrc::jsonb_contains(f) {
        return None;
    }
    let v = a.as_var().filter(|v| v.varno == scanrelid && v.varlevelsup == 0 && v.varattno > 0 && v.vartype == types_core::JSONBOID)?;
    let k = b.as_const().filter(|k| k.consttype == types_core::JSONBOID)?;
    let leaves = gravity_fn::typesem::pgrc::contains_string_leaves(const_bytes(k)?, gravity_program::cost::pgrc::JSONB_PATH_MAX)?;
    let mut out = mcx::reserved_in(leaves.len(), mcx);
    for (keys, _) in leaves.iter() {
        out.push_in((v.varattno, site_path(keys, mcx)?));
    }
    Some(out)
}

/// A text[] constant's elements (`raw::const_text_elems`, its image read as a bake); `None` for NULL, a NULL element or
/// an image not plain.
fn text_elems<'m>(c: &types_nodes::primnodes::Const, mcx: Mcx<'m>) -> Option<PgVec<'m, PgVec<'m, u8>>> {
    if c.consttype != types_core::TEXTARRAYOID || c.constisnull || c.constbyval {
        return None;
    }
    let img = gravity_decompose::bake::read(c);
    let k = gravity_expr::nodes::konst::ConstNode::new(gravity_expr::NullableDatum::value(img), c.consttype, -1, false);
    Some(crate::ops::scan::pgrc::raw::const_text_elems(&k)?.into_iter().map(|e| mcx::to_vec_in(e, mcx)).collect_in(mcx))
}
