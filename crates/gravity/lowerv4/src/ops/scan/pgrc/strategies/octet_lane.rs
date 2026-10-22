//! D3038: `octet_length` of a pgrc varlena column as a per-row int lane (road `percode.length`'s row form).
//!
//! STRAIGHT FORM (percode.length): the call over the column's code lane reads a per-code length table; on a verbatim
//! part (no dictionary: `DictView::Ranked`) the codes are the part's row ranks, about one per row, so the table is as
//! large as the part and every row's read of it is a random load.
//! ENGAGED: `engage` (face/source.rs `site_draft`) declares on the drafted site one `shred_kind::OCTET_LENGTH` lane per
//! column whose `octet_length` (text's `textoctetlen`, bytea's `byteaoctetlen`, over the bare column or a transparent
//! relabel of it) the pipeline's leading filters, its projection, or a group's keys and aggregates over the scan row read
//! (`row_step`). The host fills it per granule from the column's own streams (hosts `decode_octet`): a verbatim part's
//! entry headers (`StreamCursor::decode_lengths`, no payload read), a coded part's dictionary index lengths through its
//! codes, any other encoding's decoded images; a row is valid where the column is not NULL. `rewrite` replaces each such
//! call with the lane's `Var` (int4, the call's type): the value, the NULLs and the absence of errors are the call's.
//! The column's own lane stays for its other reads. Registers nothing: percode.length's road and per-value length
//! switch gate it; step/expr.rs rewrites, before the per-code roads see the call.
//! DICTIONARY COLUMNS: a column whose rows sit in dictionary-coded parts (`Facts::att_dict`) keeps the straight form:
//! its per-code table is one entry a dictionary value there, and the per-code folds over it (a group's `percode_const`)
//! beat a per-row read. A column stored mostly verbatim takes the lane, which fills its coded parts from their
//! dictionary index. So does a dictionary column only row steps read whose entries are enough of its rows
//! (`Facts::att_entries` over `rel_rows`, cost::octet::lane_pays: about 3 % at both ends) that the table's evaluation an
//! entry costs more than the lane's fill a row.
//! FED GROUP: PostgreSQL computes an aggregate's argument above the scan, so a group fed by a projection (its only op
//! after the leading filters, jsonb_fwd.rs `feed`) reads the column as an `OUTER_VAR` of that projection's entry. The
//! projection carries each such octet lane as a hidden column past jsonb_fwd's (step/project.rs, `lane_cols`), and
//! `rewrite` at the group turns the call into an `Outer` read of it.
//! EMPTINESS (R2): `col = ''` / `col <> ''` (text equality with an empty constant under a deterministic collation,
//! gravity_expr percode `empty_test`: the one value of byte length 0) a row step reads over a column with an octet lane takes a `shred_kind::NONEMPTY` lane
//! the host fills from the same length decode (1 where the length is not 0); `rewrite` reads it for `<> ''` and its NOT
//! for `= ''` (a NULL column stays NULL either way), before the per-code roads see the comparison.
//! NARROW (R4): a column every read of which is one of those (the leading filters' and the projection's over the scan
//! row, and a group's `octet_length` over the row or its feed; a bare feed entry only those group reads consume) is no
//! longer read at all: its own lane becomes its first length lane in place (`SiteDesc::shred_in_place`: the same index,
//! so no lane index the decomposer recorded moves), and the site neither decodes its codes nor opens its dictionary (a
//! verbatim part's whole-part rank image). Only over a group (fed or direct), the end that reads nothing else of the row.
use gravity_decompose::expr::{kids, walk};
use gravity_decompose::{Op, Source};
use gravity_expr::nodes::bool::BoolNode;
use gravity_expr::nodes::var::Var;
use gravity_expr::{Compiled, ExprNode, Kids, NodeId, Source as VarSource};
use gravity_program::{shred_kind, LaneId, SiteDesc};
use mcx::{CollectIn, Mcx, PgVec, GrowIn};
use types_nodes::list::NodeList;
use types_nodes::primnodes::OUTER_VAR;
use types_nodes::Node;

use crate::face::strategy;
use crate::frame::px::Px;
use crate::frame::site::SHRED_AT;
use crate::graph::Gx;
use crate::props::code::entry;

/// The most octet lanes one site declares.
const MAX_LANES: usize = gravity_program::cost::pgrc::OCTET_LANES_MAX;

/// The length lane kind call `funcid` reads, by the C function it runs: a varlena's byte length (`octet_length` of text
/// or bytea) its octet lane; text's `length` / `char_length` (`textlen`) its character lane (R3).
fn kind_of(funcid: u32) -> Option<u8> {
    match gravity_fn::shim_of(funcid)?.name {
        "textoctetlen" | "byteaoctetlen" => Some(shred_kind::OCTET_LENGTH),
        "textlen" => Some(shred_kind::CHAR_LENGTH),
        _ => None,
    }
}

/// The road on: percode.length not switched off and the per-value length form on.
fn on() -> bool {
    !strategy::off().has(crate::dict::percode_len::PERCODE_LENGTH) && crate::dict::face::PerValue::Length.on()
}

/// L0-g: pipeline `k`'s length columns (`cols`: octet, character and emptiness) as length lanes of `site`; a column
/// `narrow` finds no other read of turns its own lane into its first one (R4).
pub(crate) fn engage(px: &Px<'_, '_>, k: usize, site: &mut SiteDesc) {
    if !on() {
        return;
    }
    let lens = cols(px.mcx(), px.gx, k, site);
    for a in narrow(px.mcx(), px.gx, k, site, &lens[..]) {
        // every kind read but the first declared first, so the column turns only once every read has its lane
        let mut kinds = lens.iter().filter(|&&(c, _)| c == a).map(|&(_, kind)| kind);
        let Some(first) = kinds.next() else { continue };
        if !kinds.all(|kind| site.add_shred(a, &[], kind, 0).is_some()) {
            continue;
        }
        let at = site.lanes().iter().position(|l| l.id == LaneId::Attr(a));
        if at.and_then(|at| site.shred_in_place(at, first)).is_some() {
            fires::fire("octet.narrow"); // shapecheck fire signal (D3038 R4): a column read only for its lengths
        }
    }
    for &(attno, kind) in &lens {
        let _ = site.add_shred(attno, &[], kind, 0);
    }
}

/// Whether step `j` of pipeline `k` reads the scan's row (jsonb_nums.rs `row_step`).
fn row_step(gx: &Gx<'_, '_>, k: usize, j: usize) -> bool {
    let ops = &gx.pipelines()[k].ops;
    let lead = ops.iter().take_while(|op| matches!(op, Op::Filter(_) | Op::Gate(_))).count();
    j <= ops.len() && (j <= lead || (j == lead + 1 && matches!(ops.get(lead), Some(Op::Project(_)))))
}

/// `n` as a length call (`kind_of`) of a column of scan `scanrelid` (bare, or under a transparent relabel): its attno
/// and lane kind. An `OUTER_VAR` argument reads entry `varattno` of `proj` (a group's key or aggregate over the last
/// projection).
fn call<'p>(n: Node<'p>, scanrelid: i32, proj: Option<&NodeList<'p>>) -> Option<(i16, u8)> {
    let f = n.as_func_expr()?;
    let kind = kind_of(f.funcid)?;
    let [a] = f.args.as_slice() else { return None };
    column(*a, scanrelid, proj).map(|a| (a, kind))
}

/// R2: `n` as `col = ''` / `col <> ''` over a column of scan `scanrelid` (bare, or under a transparent relabel), a text
/// equality with an empty constant under a deterministic collation (gravity_expr `empty_test`'s rule): its attno.
fn empty_read(n: Node<'_>, scanrelid: i32) -> Option<i16> {
    let (f, args, coll) = match (n.as_op_expr(), n.as_func_expr()) {
        (Some(o), _) => (o.opfuncid, &o.args, o.inputcollid),
        (_, Some(f)) => (f.funcid, &f.args, f.inputcollid),
        _ => return None,
    };
    gravity_expr::percode::text_eq_body(f)?;
    let [a, b] = args.as_slice() else { return None };
    let empty = b.as_const().and_then(gravity_decompose::expr::const_text_empty) == Some(true);
    (empty && gravity_fn::typesem::expr::built_coll_bytes(coll)).then_some(())?;
    column(*a, scanrelid, None)
}

/// R4: whether every read of column `attno` in `n` is a length read: an `octet_length` call of it (over the scan row, or
/// with `proj` an `OUTER_VAR` through its entries) or, with `empty`, an emptiness test of it over the scan row. False for a
/// whole-row read, a column of no row it knows (an `OUTER_VAR` with no `proj`), and a node `kids` does not know.
fn lengths_only<'p>(n: Node<'p>, attno: i16, scanrelid: i32, proj: Option<&NodeList<'p>>, empty: bool) -> bool {
    crate::stack_check();
    if call(n, scanrelid, proj).is_some_and(|(a, _)| a == attno) || (empty && empty_read(n, scanrelid) == Some(attno)) {
        return true;
    }
    let unplaced = |v: &types_nodes::primnodes::Var<'_>| v.varlevelsup == 0 && if v.varno == OUTER_VAR { proj.is_none() } else { v.varno != scanrelid || v.varattno <= 0 };
    if column(n, scanrelid, proj) == Some(attno) || n.as_var().is_some_and(unplaced) {
        return false;
    }
    let mut ok = true;
    kids(n, &mut |x| ok &= lengths_only(x, attno, scanrelid, proj, empty)) && ok
}

/// R4: of pipeline `k`'s length columns (`octet`, `empty`), those it reads for their lengths alone, each a column lane of
/// `site`. Only the shapes `rewrite` covers every read of: leading filters and gates over the scan row, then a group
/// reading the row (`row_step` past the ops) or fed by one projection (`feed`), whose bare entry of the column only the
/// group's `octet_length` reads consume (passed on as no cell, step/project.rs).
fn narrow<'g, 'p>(mcx: Mcx<'g>, gx: &Gx<'_, 'p>, k: usize, site: &SiteDesc, lens: &[(i16, u8)]) -> PgVec<'g, i16> {
    if !site.shreds.is_empty() {
        return PgVec::new_in(mcx);
    }
    let mut out: PgVec<'g, i16> = PgVec::new_in(mcx);
    for &(a, _) in lens {
        if !out.contains(&a) && site.lanes().iter().any(|l| l.id == LaneId::Attr(a)) && lengths_only_reads(gx, k, a) {
            out.push_in(a);
        }
    }
    // a site keeps one column lane at least: a granule's rows are counted off a column's (decompose `driven`)
    let cols = site.lanes().iter().filter(|l| matches!(l.id, LaneId::Attr(x) if x > 0)).count();
    if out.len() >= cols {
        out.clear();
    }
    out
}

/// R4: whether pipeline `k` reads column `a` of its scan for its lengths alone, in the shapes `rewrite` covers: leading
/// filters and gates over the scan row (length calls and emptiness tests), then a group reading the row or fed by one
/// projection (`feed`) whose bare entry of the column only the group's length calls consume.
fn lengths_only_reads(gx: &Gx<'_, '_>, k: usize, a: i16) -> bool {
    let p = &gx.pipelines()[k];
    let Source::Range { scanrelid, .. } = &p.source else { return false };
    let sr = *scanrelid as i32;
    let lead = p.ops.iter().take_while(|op| matches!(op, Op::Filter(_) | Op::Gate(_))).count();
    let fed = feed(gx, k);
    if gx.group_at(k).is_none() || !(lead == p.ops.len() || fed.is_some()) {
        return false;
    }
    let quals = p.ops[..lead].iter().all(|op| matches!(op, Op::Filter(q) | Op::Gate(q) if lengths_only(*q, a, sr, None, true)));
    let entries = fed.is_none_or(|(_, list)| list.iter().map(|e| e.as_target_entry().map_or(e, |t| t.expr)).all(|x| column(x, sr, None) == Some(a) || lengths_only(x, a, sr, None, true)));
    let proj = fed.map(|(_, list)| list);
    // the group's reads are length calls alone: `cols` declares no emptiness lane for a group's test
    let group = gx.group_at(k).into_iter().all(|g| {
        g.keys.iter().all(|x| lengths_only(x.expr.node(), a, sr, proj, false)) && g.aggs.iter().all(|ag| ag.filter.iter().copied().chain([ag.node]).all(|n| lengths_only(n, a, sr, proj, false)))
    });
    quals && entries && group
}

/// D3038 (A): whether pipeline `k` of a dictionary column `attno` (the straight form: its per-code length table, `cols`)
/// reads the column for its lengths and its emptiness alone (`lengths_only_reads`), with the road on: its per-code
/// `OCTET_LENGTH` table carries `table_fn::FLAG_SOLE` (percode.rs), and where it is the code lane's only table a verbatim
/// part that sealed the octet-length lane codes the lane by length from it (hosts `lane_codes`), not by its text.
pub(crate) fn sole(gx: &Gx<'_, '_>, k: usize, attno: i16) -> bool {
    on() && k < gx.pipelines().len() && lengths_only_reads(gx, k, attno)
}

/// `a` as a column of scan `scanrelid` read bare or through relabels, an `OUTER_VAR` through `proj`'s entry (once): its
/// attno.
fn column<'p>(mut a: Node<'p>, scanrelid: i32, mut proj: Option<&NodeList<'p>>) -> Option<i16> {
    loop {
        while let Some(r) = a.as_relabel_type() {
            a = r.arg;
        }
        let v = a.as_var()?;
        if v.varno == OUTER_VAR && v.varlevelsup == 0 {
            a = entry(proj.take()?, v.varattno)?;
            continue;
        }
        return (v.varno == scanrelid && v.varlevelsup == 0 && v.varattno > 0).then_some(v.varattno);
    }
}

/// The projection feeding pipeline `k`'s group: its step and list (jsonb_fwd.rs `feed`'s rule).
fn feed<'g, 'p>(gx: &Gx<'g, 'p>, k: usize) -> Option<(usize, &'g NodeList<'p>)> {
    let ops = &gx.pipelines()[k].ops;
    let lead = ops.iter().take_while(|op| matches!(op, Op::Filter(_) | Op::Gate(_))).count();
    let Some(Op::Project(list)) = ops.get(lead).filter(|_| lead + 1 == ops.len()) else { return None };
    crate::table::emit_group::fed_group(gx, k, lead).map(|_| (lead, *list))
}

/// The `(column, lane kind)` length calls pipeline `k`'s group reads (its keys, its aggregates and their FILTERs), over
/// the scan row or, with `proj`, over that feeding projection's entries.
fn group_reads<'g, 'p>(mcx: Mcx<'g>, gx: &Gx<'_, 'p>, k: usize, scanrelid: i32, proj: Option<&NodeList<'p>>) -> PgVec<'g, (i16, u8)> {
    let mut found: PgVec<'g, (i16, u8)> = PgVec::new_in(mcx);
    let mut see = |n: Node<'p>| {
        walk(n, &mut |x| {
            if let Some(r) = call(x, scanrelid, proj).filter(|r| !found.contains(r) && found.len() < MAX_LANES) {
                found.push_in(r);
            }
            true
        });
    };
    for g in gx.group_at(k) {
        g.keys.iter().for_each(|x| see(x.expr.node()));
        g.aggs.iter().for_each(|a| a.filter.iter().copied().chain([a.node]).for_each(&mut see));
    }
    found
}

/// The site's `kind` length lane over column `attno`: its shred index.
fn lane_of(site: &SiteDesc, attno: i16, kind: u8) -> Option<usize> {
    site.shreds.iter().position(|d| d.kind == kind && site.out_attnos.get(d.lane as usize) == Some(&attno))
}

/// FED GROUP: the length lanes pipeline `k`'s group reads over its feed, which the projection at step `j` carries hidden
/// after jsonb_fwd's lanes (step/project.rs): each `(SHRED_AT + n, INT4OID)`, in the group's reading order.
pub(crate) fn lane_cols<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgVec<'g, (i16, u32)> {
    let gx = px.gx;
    let mcx = px.mcx();
    let (Some((_, list)), Source::Range { scanrelid, .. }) = (feed(gx, k).filter(|&(at, _)| at == j && on()), &gx.pipelines()[k].source) else { return PgVec::new_in(mcx) };
    let reads = group_reads(mcx, gx, k, *scanrelid as i32, Some(list));
    let Some(site) = px.site_mut(k) else { return PgVec::new_in(mcx) };
    reads.iter().filter_map(|&(a, kind)| lane_of(site, a, kind).map(|n| (SHRED_AT + n as i16, types_core::INT4OID))).collect_in(mcx)
}

/// Pipeline `k`'s length lanes `(column, kind)`, each over a column lane of the drafted `site`: the length calls a row
/// step reads (`call`: octet and character lengths) and the emptiness tests (R2) its leading filters or projection make
/// of a column it reads a length of, in that order.
fn cols<'g, 'p>(mcx: Mcx<'g>, gx: &Gx<'_, 'p>, k: usize, site: &SiteDesc) -> PgVec<'g, (i16, u8)> {
    let p = &gx.pipelines()[k];
    let Source::Range { scanrelid, relid, .. } = &p.source else { return PgVec::new_in(mcx) };
    let sr = *scanrelid as i32;
    let mut found: PgVec<'g, (i16, u8)> = PgVec::new_in(mcx);
    let mut empty: PgVec<'g, i16> = PgVec::new_in(mcx);
    let mut see = |n: Node<'p>| {
        walk(n, &mut |x| {
            if let Some(r) = call(x, sr, None).filter(|r| !found.contains(r) && found.len() < MAX_LANES) {
                found.push_in(r);
            }
            if let Some(a) = empty_read(x, sr).filter(|a| !empty.contains(a) && empty.len() < MAX_LANES) {
                empty.push_in(a);
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
    let proj = feed(gx, k).map(|(_, list)| list);
    let mut grouped: PgVec<'g, i16> = PgVec::new_in(mcx);
    if proj.is_some() || row_step(gx, k, p.ops.len()) {
        for r in group_reads(mcx, gx, k, sr, proj) {
            grouped.push_in(r.0);
            if !found.contains(&r) && found.len() < MAX_LANES {
                found.push_in(r);
            }
        }
    }
    // a column whose rows sit in dictionary parts keeps the per-code length table (and its per-code folds): there its
    // table is one entry a dictionary value, read by code; the lane pays where they are verbatim (an entry a row), and
    // over a dictionary column only row steps read where its entries are enough of its rows that a table's cost an
    // entry passes the lane's cost a row (cost::octet::lane_pays; a group's per-code fold keeps the table)
    let lane_pays = |a: i16| match (gx.facts.att_entries(*relid, a), gx.facts.rel_rows(*relid)) {
        (Some(entries), Some(rows)) => !grouped.contains(&a) && gravity_program::cost::octet::lane_pays(entries, rows, gx.facts.width().unwrap_or(0)),
        _ => false,
    };
    found.retain(|&(a, _)| site.lanes().iter().any(|l| l.id == LaneId::Attr(a)) && (!gx.facts.att_dict(*relid, a) || lane_pays(a)));
    // an emptiness lane only beside one of the column's length lanes, whose one length decode a granule it shares (hosts
    // `decode_octet`): alone, the walk of a verbatim granule's entry headers costs more than the per-code test it
    // replaces once that test's tables are warm (a filter of `<> ''` tests alone runs about 4x slower hot)
    empty.retain(|a| found.iter().any(|&(c, _)| c == *a));
    found.extend_in(empty.into_iter().map(|a| (a, shred_kind::NONEMPTY)));
    found
}

/// Each `octet_length` call over a column with an octet lane on pipeline `k`'s drafted site that step `j` reads, as
/// that lane's `Var`. Only over a step the scan's rows reach through filters and gates (its scan columns the site's).
/// At the group step (`j` past the ops) of a compile with no `Outer` list of its own (`bare`), over a feed: a call over
/// an `Outer` column the feed forwards bare reads the lane's hidden column there (FED GROUP). Returns the plan nodes of
/// `node` (by address) whose emptiness test the program no longer evaluates: their literals are not its needs (step/consts.rs
/// `place`'s `skip`).
pub(crate) fn rewrite<'g>(px: &mut Px<'g, '_>, (k, j): (usize, usize), bare: bool, node: Node<'_>, c: &mut Compiled<'_>) -> PgVec<'g, usize> {
    let gx = px.gx;
    let mcx = gx.mcx;
    if k >= gx.pipelines().len() || !row_step(gx, k, j) || !on() {
        return PgVec::new_in(mcx);
    }
    let Source::Range { scanrelid, .. } = &gx.pipelines()[k].source else { return PgVec::new_in(mcx) };
    let sr = *scanrelid as i32;
    let fed = feed(gx, k).filter(|_| bare && j == gx.pipelines()[k].ops.len());
    let (carried, hidden) = match fed {
        Some((at, list)) => {
            let jsonb = crate::ops::scan::pgrc::strategies::jsonb_fwd::lane_cols(px, k, at).len();
            let hidden = list.len() + crate::table::emit_group::hidden_reps(gx, k, at).len() + crate::table::emit_group::image_keys(gx, k, at).len() + jsonb;
            (lane_cols(px, k, at), hidden)
        }
        None => (PgVec::new_in(mcx), 0),
    };
    let Some(site) = px.site_mut(k) else { return PgVec::new_in(mcx) };
    if !site.shreds.iter().any(|d| matches!(d.kind, shred_kind::OCTET_LENGTH | shred_kind::NONEMPTY | shred_kind::CHAR_LENGTH)) {
        return PgVec::new_in(mcx);
    }
    let mut tested: PgVec<'_, i16> = PgVec::new_in(mcx);
    for i in 0..c.prog.nodes.len() {
        let island = c.prog.islands.iter().any(|s| (s.first.0 as usize..=s.root.0 as usize).contains(&i));
        // R2: an emptiness test over a scan column with an emptiness lane reads the lane (`= ''` its NOT)
        if let Some((arg, ne)) = gravity_expr::percode::empty_test(&c.prog, NodeId(i as u32)).filter(|_| !island) {
            let Some((VarSource::Scan, attno)) = operand(c, arg) else { continue };
            let Some(n) = lane_of(site, attno, shred_kind::NONEMPTY) else { continue };
            let lane = Var { src: VarSource::Scan, attno: SHRED_AT + n as i16, typ: types_core::BOOLOID, desc: 0 };
            retarget(c, arg, lane);
            let var = ExprNode::Var(lane);
            if ne {
                c.prog.nodes[i] = var;
            } else {
                let p = &mut c.prog;
                let (lane, ks) = (NodeId(p.nodes.len() as u32), p.kids.len() as u32);
                p.nodes.push_in(var);
                p.kids.push_in(lane);
                p.nodes[i] = ExprNode::Bool(BoolNode { op: types_nodes::primnodes::BoolExprType::NOT_EXPR, args: Kids { start: ks, len: 1 } });
            }
            if !tested.contains(&attno) {
                tested.push_in(attno);
            }
            fires::fire("octet.empty"); // shapecheck fire signal (D3038 R2)
            continue;
        }
        let ExprNode::Func(f) = &c.prog.nodes[i] else { continue };
        let call = &c.prog.sites[f.site.0 as usize];
        let Some(kind) = kind_of(call.funcid).filter(|_| !island && call.args.len == 1) else { continue };
        let arg = c.prog.kids[call.args.start as usize];
        let var = match operand(c, arg) {
            Some((VarSource::Scan, attno)) => {
                let Some(n) = lane_of(site, attno, kind) else { continue };
                Var { src: VarSource::Scan, attno: SHRED_AT + n as i16, typ: types_core::INT4OID, desc: 0 }
            }
            Some((VarSource::Outer, r)) => {
                let Some((_, list)) = fed else { continue };
                let Some(n) = entry(list, r).and_then(|e| column(e, sr, None)).and_then(|a| lane_of(site, a, kind)) else { continue };
                let Some(h) = carried.iter().position(|&(a, _)| a == SHRED_AT + n as i16) else { continue };
                Var { src: VarSource::Outer, attno: (hidden + 1 + h) as i16, typ: types_core::INT4OID, desc: 0 }
            }
            _ => continue,
        };
        retarget(c, arg, var);
        c.prog.nodes[i] = ExprNode::Var(var);
        fires::fire("octet.lane");
    }
    // a test of a column with the lane under a subtree this program does not compile (an aggregate's, a SubPlan's) is no
    // need either way: `place` reaches none of it
    let mut skip = PgVec::new_in(mcx);
    if !tested.is_empty() {
        walk(node, &mut |x| {
            let hit = empty_read(x, sr).is_some_and(|a| tested.contains(&a));
            if hit {
                skip.push_in(x.as_raw().as_ptr() as usize);
            }
            !hit
        });
    }
    skip
}

/// The column `Var` under operand `id` (through transparent relabels) of a call `rewrite` replaced reads `lane` instead:
/// left reading the column, the node no root reaches would still make the step carry it (step/expr.rs `cols_of` reads
/// every `Var` of the program), which a narrowed column (R4) is not.
fn retarget(c: &mut Compiled<'_>, mut id: NodeId, lane: Var) {
    while let ExprNode::Relabel(r) = &c.prog.nodes[id.0 as usize] {
        if !r.transparent() {
            return;
        }
        id = r.arg;
    }
    if matches!(c.prog.nodes[id.0 as usize], ExprNode::Var(_)) {
        c.prog.nodes[id.0 as usize] = ExprNode::Var(lane);
    }
}

/// `id` as a column read bare or through transparent relabels: its source (`Scan`: a scan column; `Outer`: an entry of
/// the step's input projection) and attno.
fn operand(c: &Compiled<'_>, mut id: NodeId) -> Option<(VarSource, i16)> {
    loop {
        match &c.prog.nodes[id.0 as usize] {
            ExprNode::Var(v) if v.src == VarSource::Scan && v.attno > 0 && v.attno < SHRED_AT => return Some((VarSource::Scan, v.attno)),
            ExprNode::Var(v) if v.src == VarSource::Outer && v.attno > 0 => return Some((VarSource::Outer, v.attno)),
            ExprNode::Relabel(r) if r.transparent() => id = r.arg,
            _ => return None,
        }
    }
}
