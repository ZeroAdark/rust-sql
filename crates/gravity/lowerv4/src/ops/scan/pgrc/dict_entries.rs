//! Value-set consumers read the dictionary entries (H11, `dict.entries`, R3-L08; KEEP lowerv2 compose.rs's plain text
//! set and text/codes.rs `entries_site`): a count(DISTINCT x) whose every aggregate counts x (keyed.distinct_plain's
//! count form) over a pgrc code column, the site reading that column alone, needs only the column's value set, when its
//! collation's equal values are its equal bytes (D1162: a nondeterministic key keys by its sort key). A filter keeps
//! the entries when it is a code verdict (SRC-24): it reads only that column (the site's one lane) and runs in the
//! program on each entry (an entries window's verdict is mixed, hosts `stage_entries`), where PostgreSQL runs it on each
//! row holding that value (every entry stands for a visible row, `entries_safe`), so the survivors' value set is the
//! same; it raises nothing and calls nothing volatile or user-defined (an entry stands for one row or many). PS1-18: a
//! filtered site never takes a part whole as entry runs, which would skip the filter: the site says it is filtered
//! (`entries_filtered`) and the host's `entry_runs_offer` refuses it, whatever form the filter reads its column in (a
//! code-set table, the empty-code test, a length). A NULL key, which no entry holds, the count skips whatever the
//! filter answers. DC-21's row count (count(DISTINCT ROW(x)), a starred PairCount) counts the NULL key as a group, so it
//! asks for each part's NULL row as a value-set group does (`entries_nulls`), and never takes PS1-25's path entries.
//! The site publishes its code lane as each part's dictionary entries, each code once per part (hosts `stage_entries`;
//! a part with deletes, or a walk the host cannot take, still publishes its rows), sparse with the heads prefaulted at a
//! part's first granule. The consumer is unchanged: it images each code by value, so a value in two parts
//! counts once, and a NULL row, which no dictionary holds, is one the count skips anyway. The site's one table may be the
//! consumer's own Fp128 table over that lane (keyed_emit_group `fp_lanes`, declared by pgrc.rs before this runs: the
//! image of each entry, not a filter); any other table keeps the rows (D343). Past an accepted offer of the consumer's
//! plain pair, the host hands such a part to the owner phase as its resident entry runs instead (hosts granule_source/entry_runs.rs).
//! RD-dict.entries: a value-set GROUP is the other consumer: a hashed group with no aggregate and no slot (D3571: a hashed
//! SetOp side counts its rows) whose one key is the site's
//! code column (`SELECT DISTINCT x`, `GROUP BY x` alone, under the same collation and filter rules) reads each entry as its
//! group, where the rows path folds each row into its value's group (code.front_part's per-part front). PostgreSQL keeps
//! a NULL key as a group of its own, which no dictionary holds, so the site asks for each part's NULL row
//! (`entries_nulls`: hosts `stage_entries` adds one invalid row after the entries of a part whose stats records count a
//! NULL; a part without records walks its rows).
//! V4R-310: a top that road code.heads engages (runs/heads.rs `entries`) takes the same entries site.
//! PS1-19 (`every`): a plain end whose every aggregate is a count(DISTINCT) of one of the site's code lanes, each lane
//! counted (two or more), folds each lane's value set alone (host-sorted, ops/group/regroup.rs D1210): the site publishes
//! every lane's entries side by side in one window (`SiteDesc::ENTRIES_EVERY`, hosts `stage_entries_every`), a lane past
//! its own entries invalid, a NULL the count skips. No filter, zone, table or shred: a row's pairing of two lanes means
//! nothing there. A min / max of text folded through its collation (class.text: a collation that does not order by
//! bytes, so no code rank) reads only its lane's value set too: the extreme of a part's entries is the extreme of its
//! rows, and under a deterministic collation it is one value whichever row holds it. Such extremes join the counts; a
//! lone lane read by extremes alone, unfiltered, publishes its entries as the one-lane consumers' site does (`entries`:
//! each row's render and compare become one an entry).
//! PS1-25 (`path`): the count form over `col ->> 'key'`, a jsonb column the site reads alone, whose path the relation's
//! parts elected as text: the site declares the path's code lane after the column and publishes it as each part's
//! entries, where every electing part sealed the path as a dictionary (`dict_sealed`). A part whose path dictionary is
//! sealed puts each entry's one-key document {key: entry} in the column's cell (hosts `path_docs`), so the unchanged key
//! program (`->>`, then the text's Fp128) images it as a row holding that value; any other part (a later verbatim one, a
//! part that did not elect the path) publishes its rows. In an electing part every row's
//! `col ->> 'key'` is the path lane's value or NULL (pgrc_write shred_jsonb.rs: a lane with a value of another type or a
//! JSON null at the path is not emitted), so its entries are its non-NULL value set, each a JSON string as on the row (a
//! part that elected the path at another kind publishes its rows: hosts `Faces::locate`). The cell is synthetic, so the
//! compiled program's every reader of the column is the key: the pipeline's ops are projections whose every entry is a
//! bare column (a pass-through to the end) or that key; the end's one key is it (no second path, which is
//! keyed.distinct_multi's end); no aggregate carries a FILTER, and every aggregate counts that key (decompose's one pair
//! set, agg/mod.rs `same_var`); no grouping-set arm, two-pass floor or pinned column; no site filter, zone, table or shred.
//! Registers road `dict.entries`.
//! Owner M1-SOURCE.
use gravity_abi::breaker::KeyKind;
use gravity_abi::ctx::lane;
use gravity_decompose::ops::shared::agg::{AggDecl, FinalForm, GroupDecl};
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::ops::shared::agg_class::Class;
use gravity_decompose::{Op, Source};
use gravity_program::{shred_kind, LaneId, SiteDesc};
use types_nodes::primnodes::OUTER_VAR;
use types_nodes::Node;

use crate::face::kinds::KindId;
use crate::face::strategy::CODE_HEADS;
use crate::face::runs::CodeTopkImpl;
use crate::frame::px::Px;
use crate::admit::refusal::Variant;
use crate::ops::Table;
use crate::frame::iface::need_strategy;

crate::strategies! {
    /// Value-set consumers read dictionary entries.
    DICT_ENTRIES = 23, "dict.entries", CodeKeyed;
}

pub(crate) const ROWS: Table = Table { strategies: &[(DICT_ENTRIES, &())], ..crate::ops::NONE };

/// Pipeline `k`'s drafted site publishes its one code lane's entries when the pipeline is the value-set consumer above.
pub(crate) fn entries(px: &Px<'_, '_>, k: usize, site: &mut SiteDesc) {
    let gx = px.gx;
    if every(px, k, site) {
        site.sparse_dicts = true;
        site.dict_renders = 0;
        site.head_prefault = true;
        site.entries = Some(SiteDesc::ENTRIES_EVERY);
        fires::fire("dict.entries"); // shapecheck fire signal (REGISTRY SRC-24)
        return;
    }
    if let Some(n) = path(px, k, site) {
        site.sparse_dicts = true;
        site.dict_renders = 0;
        site.head_prefault = true;
        site.entries = u8::try_from(n).ok();
        fires::fire("dict.entries"); // shapecheck fire signal (REGISTRY SRC-24)
        return;
    }
    let [l] = site.lanes()[..site.natts()] else { return };
    let LaneId::Attr(attno) = l.id else { return };
    let ops = &gx.pipelines()[k].ops;
    let filtered = ops.iter().any(|op| matches!(op, Op::Filter(_)));
    let plain = ops.iter().all(|op| match op {
        Op::Project(list) => !filtered || list.iter().filter_map(|n| n.as_target_entry()).all(|t| t.expr.as_var().is_some()),
        // PS1-27: a streamed union's arm marker passes its arm's rows on unchanged (an arm the planner prunes runs no rows)
        Op::Append(AppendDesc::Arm { .. }) => true,
        Op::Filter(q) => crate::effects::raise_free(gx, *q) && !crate::effects::user_or_volatile(gx, *q),
        _ => false,
    });
    // D1162: the key's collation must equate equal bytes only (a nondeterministic key's image is key.coll's sort key, the
    // entry runs' the entry's bytes)
    // AGA-33: PairFold is left out since no coded column reaches it (pgrc codes only text; PairFold is integer-only); it rides when an integer code lane exists
    let counts = gx.end_need(k) == KindId::KeyedDistinctPlain
        && gx.group_at(k).is_some_and(|g| g.keys.len() == 1 && key_reads(px, k, g, attno) && gx.facts.coll(g.keys[0].coll).bytes_eq && !g.aggs.is_empty() && g.aggs.iter().all(|d| matches!(d.final_, FinalForm::PairCount)));
    // RD-dict.entries: or a value-set group, a hashed group with no aggregate whose one key is this column (`SELECT
    // DISTINCT x`, `GROUP BY x` alone): each group is one value of the column, so each part's entries give its groups; a
    // NULL row is a group of its own there, so each part adds its NULL row when its stats count one (`entries_nulls`: asked
    // whatever the column's NOT NULL says, D2838's NOT VALID constraint)
    // D3571: and no slot either: a hashed SetOp side (decompose agg/mod.rs `set_side`) has no aggregate but counts each
    // key's rows in its Count slots, which INTERSECT ALL / EXCEPT ALL read (exec setop/proto.rs CopiesLive); read as
    // entries, each value would count once a part
    let groups = gx.end_need(k) == KindId::KeyedHashagg
        && gx.group_at(k).is_some_and(|g| g.keys.len() == 1 && g.aggs.is_empty() && g.layout.slots.is_empty() && g.keys[0].attno != 0 && scan_col(px, k, g.keys[0].attno) == Some(attno) && gx.facts.coll(g.keys[0].coll).bytes_eq);
    // or an unfiltered plain fold whose every aggregate is a collated extreme of this column (`extreme`): the extreme of
    // each part's value set is its rows'; a NULL, which no entry holds, the extreme skips
    let extremes = !filtered
        && gx.end_need(k) == KindId::FoldPlain
        && gx.group_at(k).is_some_and(|g| g.keys.is_empty() && !g.aggs.is_empty() && g.aggs.iter().all(|d| extreme(d) && arg_lane(px, k, d, site) == Some(0)));
    // D343: the keyed end's own Fp128 table over this lane (fp_lanes' `over` is the lane index) passes, and a filter's
    // per-code tables over it
    let fp = |t: &gravity_program::TableDecl| t.func == gravity_program::table_fn::FINGERPRINT;
    let own_fp = site.tables.iter().all(|t| t.over == 0 && (fp(t) || filtered));
    // V4R-310: or the top code.heads engages over this site (its mech says; the part's safety is the host's, entries_safe)
    let r = CODE_HEADS;
    let heads = px.strategies.has(r) && crate::face::source::on(Variant::Strategy(r)) && need_strategy::<CodeTopkImpl>(&gx.pipelines()[k].node_id, r).0.entries(px, k, site);
    if l.kind == lane::KIND_CODES && ((site.zone.is_empty() || filtered) && plain && (counts || groups || extremes) && own_fp || heads) {
        // an entries window's granules are code blocks, not rows: no row zone map speaks for them (the qual still runs)
        site.clear_zone();
        site.sparse_dicts = true;
        site.dict_renders = 0;
        site.head_prefault = true;
        site.entries = Some(0);
        site.entries_filtered = filtered;
        // DC-21: a row count (every call a starred PairCount, distinct_plain `row`: count(DISTINCT ROW(x))) counts the NULL
        // key as a value of its own, as a value-set group does, so it asks for each part's NULL row too
        let row = counts && gx.group_at(k).is_some_and(|g| g.aggs.iter().all(|d| d.mods.star));
        // B3-24 / RUNS-15: code.heads over a nullable key sends each part's NULL rows as one NULL entry (heads.rs NULL_CODE)
        site.entries_nulls = (groups && !counts || row) && !heads || heads && crate::dict::strategies::heads::nullable_key(gx, k);
        // D2981: a top whose first key descends (code.heads) reads each part's codes from its highest, so code.cut's
        // early end ends a window at its first row past the cut in either direction (dict/strategies/code_cut.rs)
        site.entries_desc = heads && !(counts || groups) && ops.iter().find_map(|op| match op {
            Op::Sort(sd) => sd.keys.as_slice().first().map(|k| k.desc),
            _ => None,
        }) == Some(true);
        // D3007: a value-set group under a top by its key descending (code.cut's group form, dict/strategies/code_cut.rs)
        // walks each part's codes from its highest too, so the cut ends a window at its first entry past it
        site.entries_desc |= groups && !counts && !heads && crate::dict::strategies::code_cut::group_desc(px, k);
        fires::fire("dict.entries"); // shapecheck fire signal (REGISTRY SRC-24)
    }
}

/// PS1-19: pipeline `k`'s end folds only value sets of the site's lanes: a plain group (no key) whose every aggregate is a
/// count(DISTINCT lane) folded host-sorted (`FinalForm::Distinct`; no FILTER, no ORDER BY, D1162's byte-equal collation),
/// every one of the site's two or more lanes a counted code lane, the pipeline projections only.
fn every(px: &Px<'_, '_>, k: usize, site: &SiteDesc) -> bool {
    let gx = px.gx;
    let natts = site.natts();
    let lanes = &site.lanes()[..natts];
    if !(2..=64).contains(&natts) || !lanes.iter().all(|l| l.kind == lane::KIND_CODES && matches!(l.id, LaneId::Attr(_))) {
        return false;
    }
    if !site.zone.is_empty() || !site.tables.is_empty() || !site.shreds.is_empty() {
        return false;
    }
    if !gx.pipelines()[k].ops.iter().all(|op| matches!(op, Op::Project(_))) || gx.end_need(k) != KindId::FoldPlain {
        return false;
    }
    let Some(g) = gx.group_at(k).filter(|g| g.keys.is_empty() && !g.aggs.is_empty()) else { return false };
    let mut counted = 0u64;
    for d in g.aggs.iter() {
        let distinct = matches!(d.class, Class::CountAny) && d.mods.distinct != 0 && d.mods.order == 0 && !d.mods.filter && d.filter.is_none();
        if !(distinct && d.final_ == FinalForm::Distinct && d.coll.bytes_eq) && !extreme(d) {
            return false;
        }
        let Some(l) = arg_lane(px, k, d, site) else { return false };
        counted |= 1 << l;
    }
    counted.count_ones() as usize == natts
}

/// A min / max of text folded through its collation (class.text: no code rank, `MinCode` / `MaxCode` being the rank's)
/// under a deterministic collation that does not order by bytes, with no DISTINCT, ORDER BY or FILTER: its answer is its
/// argument's value set's extreme (`every`).
fn extreme(d: &AggDecl<'_>) -> bool {
    use gravity_decompose::ops::shared::agg::Register;
    use gravity_decompose::ops::shared::agg_class::Ext;
    matches!(d.class, Class::Min(Ext::Text) | Class::Max(Ext::Text))
        && !matches!(d.reg, Register::MinCode | Register::MaxCode)
        && d.coll.bytes_eq
        && !d.coll.bytes_order
        && d.mods.distinct == 0
        && d.mods.order == 0
        && !d.mods.filter
        && d.filter.is_none()
}

/// PS1-25: pipeline `k`'s end is the count form over `col ->> 'key'`, the site's one lane that jsonb column, the path
/// elected as text: the path's code lane declared on `site` (`SiteDesc::add_shred`), its lane index.
fn path(px: &Px<'_, '_>, k: usize, site: &mut SiteDesc) -> Option<usize> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let Source::Range { scanrelid, relid, .. } = &p.source else { return None };
    let [l] = site.lanes()[..site.natts()] else { return None };
    let LaneId::Attr(attno) = l.id else { return None };
    if !site.zone.is_empty() || !site.tables.is_empty() || !site.shreds.is_empty() || gx.end_need(k) != KindId::KeyedDistinctPlain {
        return None;
    }
    // the cell is synthetic, so no reader of the column but the key: an aggregate's FILTER would read the synthetic
    // document, and a pinned column, a grouping-set arm or a two-pass floor reads the rows
    let g = gx.group_at(k).filter(|g| {
        // DC-21: never a row count (a starred PairCount): an electing part's entries are the path's non-NULL value set and
        // no NULL row stands for the rows without the path, which the row count counts as a group
        g.keys.len() == 1 && g.set.is_none() && g.two_pass.is_none() && !g.empty_no_row && g.pinned.is_empty() && !g.aggs.is_empty() && g.aggs.iter().all(|d| matches!(d.final_, FinalForm::PairCount) && d.filter.is_none() && !d.mods.filter && !d.mods.star)
    })?;
    let x = &g.keys[0];
    let f = field(px, k, x.expr.node(), *scanrelid).filter(|f| f.0 == attno && x.kind == KeyKind::Fp128 && gx.facts.coll(x.coll).bytes_eq)?;
    let read = |n: Node<'_>| {
        let e = n.as_target_entry().map_or(n, |t| t.expr);
        crate::step::project::bare_var(e).is_some() || field(px, k, e, *scanrelid) == Some(f)
    };
    if !p.ops.iter().all(|op| matches!(op, Op::Project(list) if list.iter().all(&read))) {
        return None;
    }
    // the lane is declared only where each electing part sealed the path as a dictionary (SRC-30's lowering-time read): a
    // verbatim part's lane would build its byte ranks for a window that walks the rows anyway
    let key = core::str::from_utf8(f.1).ok()?;
    if gx.facts.shred_election(*relid, attno, &[key]) != Some((shred_kind::TEXT, 0)) || !crate::ops::scan::pgrc::strategies::jsonb_lanes::dict_sealed(gx, *relid, attno, f.1) {
        return None;
    }
    site.add_shred(attno, f.1, shred_kind::TEXT_CODES, 0)
}

/// `e` as `->>(Var, 'key')` over a jsonb column of pipeline `k`'s scan `scanrelid`, the Var the scan's or an end row's
/// (`OUTER_VAR`, traced back through the projections): the column's attno and the key's bytes.
fn field<'n>(px: &Px<'_, '_>, k: usize, e: Node<'n>, scanrelid: u16) -> Option<(i16, &'n [u8])> {
    let args = match (e.as_op_expr(), e.as_func_expr()) {
        (Some(o), _) if gravity_fn::typesem::pgrc::jsonb_field_text(o.opfuncid) => &o.args,
        (_, Some(f)) if gravity_fn::typesem::pgrc::jsonb_field_text(f.funcid) => &f.args,
        _ => return None,
    };
    let [a, b] = args.as_slice() else { return None };
    let v = a.as_var().filter(|v| v.varlevelsup == 0 && v.vartype == types_core::JSONBOID)?;
    let col = match v.varno {
        OUTER_VAR => scan_col(px, k, v.varattno)?,
        n if n == scanrelid as i32 && v.varattno > 0 => v.varattno,
        _ => return None,
    };
    Some((col, gravity_decompose::expr::const_bytes(b.as_const()?)?))
}

/// The site lane `d`'s one argument reads: a bare column through each of pipeline `k`'s projections back to a scan lane.
fn arg_lane(px: &Px<'_, '_>, k: usize, d: &AggDecl<'_>, site: &SiteDesc) -> Option<usize> {
    let r = d.node.as_aggref()?;
    let t = r.args.iter().next().filter(|_| r.args.len() == 1)?;
    let c = scan_col(px, k, crate::step::project::bare_var(t.as_target_entry().map_or(t, |t| t.expr))?)?;
    site.lane_index(LaneId::Attr(c)).filter(|&l| l < site.natts())
}

/// PS1-27: the pair key of group `g` reads scan column `attno`: its column traced back through pipeline `k`'s
/// projections, or, for a key decompose keeps as an expression (column 0: an Append's output, a streamed union's arm),
/// every aggregate's one argument so traced (each aggregate counts the key: `PairCount`).
fn key_reads(px: &Px<'_, '_>, k: usize, g: &GroupDecl<'_>, attno: i16) -> bool {
    let arg = |d: &AggDecl<'_>| {
        let r = d.node.as_aggref()?;
        let t = r.args.iter().next().filter(|_| r.args.len() == 1)?;
        scan_col(px, k, crate::step::project::bare_var(t.as_target_entry().map_or(t, |t| t.expr))?)
    };
    match g.keys[0].attno {
        0 => g.aggs.iter().all(|d| arg(d) == Some(attno)),
        c => scan_col(px, k, c) == Some(attno),
    }
}

/// The scan column row column `c` of pipeline `k`'s end reads: `c` through each projection back to a bare scan column
/// (PS1-27: an Append arm's projection names its arm's column; the arm marker passes columns as they are), `None` past a
/// computed one or any other op but a filter.
fn scan_col(px: &Px<'_, '_>, k: usize, c: i16) -> Option<i16> {
    let mut c = c;
    for op in px.gx.pipelines()[k].ops.iter().rev() {
        match op {
            Op::Filter(_) | Op::Append(AppendDesc::Arm { .. }) => {}
            Op::Project(list) => {
                let e = list.iter().nth(usize::try_from(c).ok()?.checked_sub(1)?)?;
                c = crate::step::project::bare_var(e.as_target_entry().map_or(e, |t| t.expr))?;
            }
            _ => return None,
        }
    }
    Some(c)
}
