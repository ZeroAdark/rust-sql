//! Road `jsonb.lanes`' group key form (V4R-468; e.g. `SELECT j->>'k', count(*) FROM t GROUP
//! BY 1`). Owner M6-JSONB. Budget 110 lines.
//!
//! A GROUP BY key that is `col ->> 'key'` over a Text path the relation's parts elected is declared on the site as that
//! path's code lane (source/jsonb_lanes.rs `paths`: shred `n`, the row's column `SHRED_AT + n`) and keys as percode.table's
//! key form over it (step/percode_key.rs `attach`, `path` here): a valid lane row's image is the path dictionary's `Fp128`
//! entry (`TableDecl::fingerprint`: rule A, the fingerprint of the bytes, equal to any text key's over the same bytes);
//! a NULL lane row (an absent key, a JSON null, a number, boolean or container, a NULL document, a part that did not
//! elect the path) evaluates the key itself, PostgreSQL's `->>`, and hashes its text by the same rule, so the string "5"
//! and the number 5 are one group. Such a row's text rides row table `r + 1` (the key's `stash`) to its record's open,
//! where a group no lane row reached keeps a copy of it (keyed/emit_group_nocode.rs, the key's representative).
//!
//! Registers nothing: source/jsonb_lanes.rs registers `path` and `key_path` as jsonb.lanes' `PathKey` face (face/codes.rs).
use gravity_abi::breaker::KeyKind;
use gravity_decompose::ops::shared::agg::{GroupDecl, GroupForm, KeyDecl};
use gravity_decompose::{expr::walk, Op, Source};
use gravity_expr::Compiled;
use gravity_program::{shred_kind, LaneId, SiteDesc};
use types_nodes::primnodes::OUTER_VAR;
use types_nodes::Node;

use crate::face::kinds::KindId;
use crate::frame::px::Px;
use crate::frame::site::SHRED_AT;
use crate::graph::Gx;

/// Key program `c` of pipeline `k` as the path form: its root is `col ->> 'key'` over a Text path `paths` declared, the
/// drafted site holds as that shred; its code column of `cols` (step/project.rs `code_cols`).
pub(crate) fn path<'a>(px: &mut Px<'_, '_>, k: usize, c: &Compiled<'_>, cols: &'a [(i16, u8, u8)]) -> Option<&'a (i16, u8, u8)> {
    let mcx = px.mcx();
    let (attno, key) = crate::ops::scan::pgrc::strategies::jsonb_codes::field(c, c.root, mcx)?;
    let n = crate::ops::scan::pgrc::strategies::jsonb_lanes::paths(px.gx, k).iter().position(|(a, x)| *a == attno && *x == key)?;
    // D1890: not a key an aggregate reads (a count(DISTINCT key) beside it reads its datum, which a valid lane row lacks);
    // D1730: nor a key of a group whose emit sorts its records by its keys
    let sr = px.gx.pipelines()[k].source.relation().map_or(0, |r| r.0 as i32);
    if px.gx.group_at(k).is_some_and(|g| key_sorted(px.gx, g) || g.keys.iter().any(|x| crate::ops::scan::pgrc::strategies::jsonb_lanes::field_in(x.expr.node(), sr, mcx).is_some_and(|f| f.0 == attno && f.1 == key) && g.aggs.iter().any(|d| agg_reads(d.node, x.resno)))) {
        return None;
    }
    let site = px.site_mut(k)?;
    let d = site.shreds.get(n).filter(|d| d.kind == shred_kind::TEXT_CODES && site.table_arg_at(d.path) == &*key)?;
    let at = SHRED_AT + n as i16;
    cols.iter().find(|x| x.0 == at && x.1 == d.lane)
}

/// Whether aggregate node `n` reads the group's column `r` (OUTER_VAR), or may (a node `kids` does not know). D3023: a
/// DISTINCT or ORDER BY clause (`aggdistinct` / `aggorder`, a SortGroupClause) reads no column: it names an argument.
pub(crate) fn agg_reads(n: Node<'_>, r: i16) -> bool {
    crate::stack_check();
    if n.node_tag() == types_nodes::NodeTag::T_SortGroupClause {
        return false;
    }
    if let Some(v) = n.as_var() {
        return v.varno == OUTER_VAR && v.varattno == r;
    }
    let mut hit = false;
    let known = gravity_decompose::expr::kids(n, &mut |x| hit |= agg_reads(x, r));
    hit || !known
}

/// D1730: whether group `g`'s emit may sort its records by its keys (keyed/emit_group.rs `Emit::new`: runs.group /
/// runs.unique, or a grouped DISTINCT whose breaker declares its key order: keyed/pairs_view.rs `order_read`'s first test,
/// an M1 file M6 does not reach, so a superset; no top's order slot or form G sort ordering them first). A path key's code
/// word has no order there (runs/topk_groups.rs `key_words`): its keys keep the value form, as on trunk (`path`, `key_path`, `late`).
fn key_sorted(gx: &Gx<'_, '_>, g: &GroupDecl<'_>) -> bool {
    let (end, form) = (g.producer.0 as usize, crate::graph::group_kind(g));
    let q = (end + 1..gx.pipelines().len()).find(|&q| matches!(gx.pipelines()[q].source, Source::Breaker { slot } if slot == g.slot));
    let slot = q.is_some_and(|q| gx.end_need(q) == KindId::RunsTopkGroups || !crate::frame::iface::run_holds_rows(gx, q));
    let pairs = g.form == GroupForm::Distinct && crate::graph::grouped(g) && gx.graph.breakers.iter().any(|d| d.producer as usize == end && d.breaker.slot == g.slot && d.order.is_known());
    !slot && (matches!(form, KindId::RunsGroup | KindId::RunsUnique) || pairs)
}

/// Group key `key` of producer `k` as a declared path: its row column `SHRED_AT + n` and its drafted lane (after the scan's
/// attribute lanes: `SiteDesc::add_shred` inserts the `n`-th shred there, before any other lane).
pub(crate) fn key_path(gx: &Gx<'_, '_>, k: usize, key: &KeyDecl<'_>) -> Option<(i16, u8)> {
    let (scanrelid, _, site) = gx.pipelines()[k].source.relation()?;
    let f = crate::ops::scan::pgrc::strategies::jsonb_lanes::field_in(key.expr.node(), scanrelid as i32, gx.mcx).filter(|_| key.resno > 0 && !gx.group_at(k).is_some_and(|g| key_sorted(gx, g)))?;
    let n = crate::ops::scan::pgrc::strategies::jsonb_lanes::paths(gx, k).iter().position(|x| *x == f)?;
    let natts = site.lanes().iter().filter(|l| matches!(l.id, LaneId::Attr(_))).count();
    Some((SHRED_AT + n as i16, u8::try_from(natts + n).ok()?))
}

/// D4854: whether path key `key` of producer `k`, whose representative reads site lane `lane` (`key_path`: the shred at
/// its place in `jsonb_lanes::paths`, read again), reads the drafted site's Text shred of its own column and path, as
/// `path` checks with the site in hand. `None`: no drafted site to ask.
pub(crate) fn drafted(px: &mut Px<'_, '_>, k: usize, key: &KeyDecl<'_>, lane: u8) -> Option<bool> {
    let (scanrelid, _, _) = px.gx.pipelines()[k].source.relation()?;
    let (attno, path) = crate::ops::scan::pgrc::strategies::jsonb_lanes::field_in(key.expr.node(), scanrelid as i32, px.mcx())?;
    let site = px.site_mut(k)?;
    Some(site.shreds.iter().any(|d| d.lane == lane && d.kind == shred_kind::TEXT_CODES && site.out_attnos.get(d.lane as usize) == Some(&attno) && site.table_arg_at(d.path) == &*path))
}

/// W9 PERF C5 S4 (decode.late): the jsonb column lanes of pipeline `k`'s drafted `site` (bit l: site lane l) that only its
/// path group keys read: no leading filter, one projection, and every read of the column an entry that keys by `path`'s
/// form (an Fp128 group key at its resno no aggregate reads, a pair count included: D1890; collation bytes-equal; a
/// group whose records its emit does not sort by its keys: D1730),
/// none by an aggregate, a compiled key (every key its entry's) or a pinned column.
/// source/pgrc_late.rs makes it late outside the fill set: the host fills it where its path lane is invalid (hosts
/// `guard_fill`), the rows whose key re-evaluates `->>` over it (percode_key.rs `cell`, its cold branch); no row reads it
/// otherwise. step/percode_key.rs `compile` refuses such a key that did not take the form.
pub(crate) fn late(px: &Px<'_, '_>, k: usize, site: &SiteDesc) -> u64 {
    let (gx, n) = (px.gx, site.natts().min(64));
    let p = &gx.pipelines()[k];
    let (Source::Range { scanrelid, .. }, [Op::Project(list)], Some(g)) = (&p.source, &p.ops[..], gx.group_at(k).filter(|g| !key_sorted(gx, g))) else { return 0 };
    let (sr, lane) = (*scanrelid as i32, |a: i16| site.lanes()[..n].iter().position(|l| l.id == LaneId::Attr(a)));
    let (mut reads, mut keyed) = ([0u32; 64], [0u32; 64]);
    let mut count = |e: Node<'_>| {
        let mut whole = false;
        let known = walk(e, &mut |x| {
            if let Some(v) = x.as_var().filter(|v| v.varno == sr && v.varlevelsup == 0) {
                whole |= v.varattno <= 0;
                if let Some(l) = lane(v.varattno) {
                    reads[l] += 1;
                }
            }
            true
        });
        known && !whole
    };
    let key_only = |r: i16| g.keys.iter().find(|x| x.kind == KeyKind::Fp128 && x.resno == r).filter(|_| !g.aggs.iter().any(|d| agg_reads(d.node, r)));
    let mut ok = g.pinned.is_empty() && g.keys.iter().all(|x| x.resno > 0);
    for te in list.iter().filter_map(|x| x.as_target_entry()) {
        ok &= count(te.expr);
        let coll = key_only(te.resno).map(|x| x.coll).filter(|&c| gx.facts.coll(c).bytes_eq && crate::dict::face::PerValue::Table.on());
        let f = coll.and_then(|_| crate::ops::scan::pgrc::strategies::jsonb_lanes::field_in(te.expr, sr, gx.mcx));
        let shred = |attno: i16, path: &[u8]| {
            let d = crate::ops::scan::pgrc::strategies::jsonb_lanes::paths(gx, k).iter().position(|x| x.0 == attno && x.1[..] == *path).and_then(|i| site.shreds.get(i));
            d.is_some_and(|d| d.kind == shred_kind::TEXT_CODES && site.table_arg_at(d.path) == path)
        };
        if let Some(l) = f.filter(|f| shred(f.0, &f.1[..])).and_then(|f| lane(f.0)) {
            keyed[l] += 1;
        }
    }
    for a in g.aggs.iter() {
        ok &= count(a.node) && a.filter.is_none_or(&mut count);
    }
    (0..n).filter(|&l| ok && reads[l] > 0 && reads[l] == keyed[l]).fold(0, |m, l| m | 1 << l)
}

/// C5 S4: key `e`'s `ex` (`path`: it keys by the path's code lane, step/percode_key.rs `KeyFp::stash`) over pipeline `k`: a
/// key over a column `late` reads only where the path lane is invalid must take that form (any other reads every row).
pub(crate) fn keyed(path: bool, px: &mut Px<'_, '_>, k: usize, e: Node<'_>, node: i32, ex: crate::frame::px::ExId) -> crate::frame::px::ExId {
    let (arena, sr) = (px.arena, px.gx.pipelines()[k].source.relation().map_or(0, |r| r.0 as i32));
    let Some(site) = px.site_mut(k).map(|s| s.copy_in(arena)).filter(|_| !path) else { return ex };
    let col = crate::ops::scan::pgrc::strategies::jsonb_lanes::field_in(e.as_target_entry().map_or(e, |te| te.expr), sr, px.mcx()).and_then(|f| site.lanes().iter().take(64).position(|l| l.id == LaneId::Attr(f.0)));
    if col.is_some_and(|l| late(px, k, &site) >> l & 1 != 0) {
        crate::refuse!(node, crate::admit::refusal::Reason::Bug { payload: "a path group key over a late jsonb column that did not key by the path's code lane" });
    }
    ex
}
