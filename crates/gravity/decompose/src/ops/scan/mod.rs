use gravity_abi::ctx::lane;
use gravity_program::{limits, ArenaRef, ArenaVec, DictRef, End, LaneDesc, LaneLife, NumForm, Pipeline, Rep, Reason, SiteDesc, ZoneOp, ZonePred};
use types_nodes::list::NodeList;
use types_nodes::plannodes::{IndexOnlyScan, IndexScan, Plan, SeqScan, ValuesScan};
use types_nodes::primnodes::{NullTestType, INDEX_VAR};
use types_nodes::{Node, NodeTag};

use crate::expr;
use crate::catalog::casts::domain_base;
use crate::catalog::scan::fixed_value_type;
use crate::catalog::{Am, AttFact, IndexAm, IndexFact, RelFact};
use crate::graph::{IndexKey, KeyForm, Source};
use crate::ledger::Kind;
use crate::ops;
use crate::admit::Refusal;
use crate::walk::{Builder, Open};
use crate::ops::shared::scan::{Atts, SELF_ITEM_POINTER};
use crate::ops::shared::scan::zone_op;
pub mod closure;
pub mod identity;
pub mod named;

impl Atts {
    pub const fn new(arena: ArenaRef) -> Atts {
        Atts { attnos: ArenaVec::new_in(arena), rowid: false, sys: 0 }
    }

    pub fn as_slice(&self) -> &[i16] {
        &self.attnos
    }

    pub fn add(&mut self, a: i16) -> bool {
        if self.attnos.contains(&a) {
            return true;
        }
        if self.attnos.len() == limits::LANES {
            return false;
        }
        let pos = self.attnos.iter().position(|&x| x > a).unwrap_or(self.attnos.len());
        self.attnos.insert(pos, a);
        true
    }

    pub fn lane(&self, a: i16) -> Option<usize> {
        self.attnos.iter().position(|&x| x == a)
    }
}

/// The lanes of a scan beyond its attributes, by the `Var`'s attno: a table entry per lane, the bit is `Atts.sys`'s. `ctid`
/// is the rowid lane (`LaneId::RowId`); the rest have no `LaneId` yet (decompose.md §7 S2), so they ride beside the site.
pub const SYS_LANES: [(i16, &str, u8); 6] = [(0, "wholerow", 8), (-2, "xmin", 4), (-3, "cmin", 4), (-4, "xmax", 4), (-5, "cmax", 4), (-6, "tableoid", 4)];

/// One scan's system lanes (`PipelineGraph.scan_lanes`): the source publishes each per row — the row image formed by the
/// source (the heap tuple; on pgrc the columns re-formed), `tableoid` a per-relation constant, the visibility words from the
/// tuple header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanLanes {
    pub node_id: i32,
    pub scanrelid: u16,
    pub relid: u32,
    pub sys: u8,
}

impl ScanLanes {
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        SYS_LANES.iter().enumerate().filter(|(i, _)| self.sys >> i & 1 != 0).map(|(_, l)| l.1)
    }

    pub fn dump(&self, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        write!(out, "scan-lanes node={} rti={} rel={}", self.node_id, self.scanrelid, self.relid)?;
        for n in self.names() {
            write!(out, " {n}")?;
        }
        writeln!(out)
    }
}

/// A pgrc scan's column its quals read and nothing projects that is not a filter-only lane (decision 12 b: a non-zone
/// qual's column that no output reads), by attno (`PipelineGraph.qual_only`). A side table, never the site's
/// `filter_lanes`: a source may leave a filter lane undecoded in a zone `AllMatch` window, which decides no non-zone qual.
/// lowerv4 elides these only in a window a condition-cache hit decided (`VERDICT_ELIDE_HIT`), where the frame skips every
/// covered qual. A code lane is never one (its dictionary faces read it past the quals).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QualOnly {
    pub node_id: i32,
    pub relid: u32,
    pub attno: i16,
}

/// The scan's projected attributes: the SeqScan's `cb_target_cols` when known (a physical target list projects every
/// column), else its target list's `Var`s; a whole-row reference in the target list projects every lane of `atts`.
fn projected_of(arena: ArenaRef, varno: i32, atts: &Atts, tlist: &NodeList<'_>, target: Option<&types_nodes::bitmapset::Bitmapset<'_>>) -> Atts {
    let mut projected = Atts::new(arena);
    let mut vars = Atts::new(arena);
    _ = scan_vars(&[tlist], varno, Kind::SeqScan, 0, &mut vars);
    match target {
        Some(t) => t.iter().for_each(|a| _ = projected.add(a as i16)),
        None => vars.as_slice().iter().for_each(|&a| _ = projected.add(a)),
    }
    if vars.sys & 1 != 0 {
        atts.as_slice().iter().for_each(|&a| _ = projected.add(a));
    }
    projected
}

/// Record scan `node_id`'s `QualOnly` columns: after `zone`, so a zone predicate's filter-only lane is not one.
#[allow(clippy::too_many_arguments)]
pub fn qual_only(b: &mut Builder<'_, '_>, node_id: i32, relid: u32, site: &SiteDesc, varno: i32, atts: &Atts, qual: &NodeList<'_>, tlist: &NodeList<'_>, target: Option<&types_nodes::bitmapset::Bitmapset<'_>>) {
    let projected = projected_of(b.arena, varno, atts, tlist, target);
    let table = &mut b.graph.qual_only;
    for q in qual.iter() {
        expr::walk(q, &mut |x| {
            if let Some(v) = x.as_var().filter(|v| v.varno == varno && v.varlevelsup == 0 && v.varattno > 0) {
                let fresh = !table.iter().any(|e| e.node_id == node_id && e.attno == v.varattno);
                let unprojected = projected.lane(v.varattno).is_none();
                if atts.lane(v.varattno).is_some_and(|k| fresh && unprojected && !site.filter_lanes.contains(k) && site.lanes[k].kind != lane::KIND_CODES) {
                    table.push(QualOnly { node_id, relid, attno: v.varattno });
                }
            }
            true
        });
    }
}

pub fn scan_lanes(b: &mut Builder<'_, '_>, node_id: i32, scanrelid: u32, relid: u32, atts: &Atts) {
    if atts.sys != 0 {
        b.graph.scan_lanes.push(ScanLanes { node_id, scanrelid: scanrelid as u16, relid, sys: atts.sys });
    }
}

/// A scan's `Var`s: attributes, `ctid` as the rowid lane, whole-row and system columns as `SYS_LANES` bits.
pub fn scan_vars(nodes: &[&NodeList<'_>], varno: i32, k: Kind, id: i32, atts: &mut Atts) -> Result<(), Refusal> {
    vars_in(nodes.iter().flat_map(|l| l.iter()), varno, k, id, atts)
}

/// `scan_vars` over the scan's quals `quals` and the target entries a consumer reads (facts::demand `read_entries`): a
/// bare column entry no consumer reads (a physical target list's) gets no lane (a scan carries only the columns its
/// consumers read); an entry that computes, or one over another relation's or a system column, keeps its `Var`s.
pub fn read_vars<'p>(b: &mut Builder<'p, '_>, plan: &'p Plan<'p>, quals: &[&NodeList<'_>], varno: i32, k: Kind, id: i32, atts: &mut Atts) -> Result<(), Refusal> {
    let entries = crate::facts::demand::read_entries(b, plan, varno);
    vars_in(entries.iter().copied().chain(quals.iter().flat_map(|l| l.iter())), varno, k, id, atts)
}

fn vars_in<'a>(nodes: impl Iterator<Item = Node<'a>>, varno: i32, k: Kind, id: i32, atts: &mut Atts) -> Result<(), Refusal> {
    let mut err = None;
    for n in nodes {
        expr::walk(n, &mut |x| {
            if let Some(v) = x.as_var() {
                if v.varno == varno && v.varlevelsup == 0 {
                    if v.varattno == SELF_ITEM_POINTER {
                        atts.rowid = true;
                    } else if v.varattno <= 0 {
                        match SYS_LANES.iter().position(|s| s.0 == v.varattno) {
                            Some(i) => atts.sys |= 1 << i,
                            None => err = Some(Refusal::shape(k, id, "a system column outside the system lane table")),
                        }
                    } else if !atts.add(v.varattno) {
                        err = Some(Refusal::shape(k, id, "more than 4096 lanes (the sanity limit)"));
                    }
                }
            }
            err.is_none()
        });
        if let Some(e) = err {
            return Err(e);
        }
    }
    Ok(())
}

/// A whole-row reference (the `wholerow` system lane) reads every live column: they join the scan's lanes, so the row
/// carries what the lowerer forms the row image from (v1 columnar.rs `reads_whole_row`). False past the lane limit.
/// D2789: a heap relation's dropped attributes join too (`dropped_fact`): PostgreSQL's whole row keeps a dropped
/// attribute's stored value (`ExecEvalWholeRowVar` flattens the scan slot after `slot_getallattrs`).
pub fn whole_row_atts(b: &Builder<'_, '_>, relid: u32, atts: &mut Atts) -> bool {
    if atts.sys & 1 == 0 {
        return true;
    }
    let rel = b.facts.rel(relid);
    let heap = rel.is_some_and(|r| r.am == Am::Heap || r.rows); // PGRCSERVE G4: rows road reads dropped attrs as heap
    let natts = rel.map_or(0, |r| r.natts);
    (1..=natts).filter(|&a| b.facts.att(relid, a).is_some() || (heap && dropped_fact(b, relid, a).is_some())).all(|a| atts.add(a))
}

/// D2789: dropped attribute `a`'s layout when a lane can carry its stored datum: a by-value word, a varlena or a
/// fixed-length by-reference image (`pg_attribute` keeps `attlen` / `attbyval` / `attalign` after DROP COLUMN; the type is gone).
fn dropped_fact(b: &Builder<'_, '_>, relid: u32, a: i16) -> Option<AttFact> {
    let l = b.facts.dropped_layout(relid, a).filter(|l| l.byval || l.typlen == -1 || l.typlen > 0)?;
    Some(AttFact::plain(0, -1, l.typlen, l.byval, false))
}

/// `scan_vars` for a consumer without system lanes (a write's carried columns, a pass-1 site): they refuse by name.
pub fn vars_of(nodes: &[&NodeList<'_>], varno: i32, k: Kind, id: i32, atts: &mut Atts) -> Result<(), Refusal> {
    let before = atts.sys;
    scan_vars(nodes, varno, k, id, atts)?;
    no_new_sys(before, k, id, atts)
}

/// `vars_of`'s rule: no system column or whole-row reference joined `atts` since `before` (they have no lane here).
fn no_new_sys(before: u8, k: Kind, id: i32, atts: &mut Atts) -> Result<(), Refusal> {
    let new = atts.sys & !before;
    atts.sys = before;
    if new & 1 != 0 {
        return Err(Refusal::shape(k, id, "a whole-row reference (no row-image lane)"));
    }
    if new != 0 {
        return Err(Refusal::shape(k, id, "a system column reference (tableoid / xmin / cmin / xmax / cmax: no system-column lane)"));
    }
    Ok(())
}

/// `SYS_LANES`' `tableoid` bit.
const TABLEOID_BIT: u8 = 1 << 5;

pub fn lanes(b: &Builder<'_, '_>, site: &mut SiteDesc, relid: u32, am: Am, atts: &[i16], k: Kind, id: i32) -> Result<(), Refusal> {
    for (i, &a) in atts.iter().enumerate() {
        // D2789: a dropped attribute a whole row reads (`whole_row_atts`) is a lane by its kept layout, with no type
        let att = b.facts.att(relid, a).or_else(|| (am == Am::Heap).then(|| dropped_fact(b, relid, a)).flatten()).ok_or(Refusal::shape(k, id, "no attribute facts"))?;
        site.lanes[i] = lane_of(i, a, att, am);
    }
    Ok(())
}

/// The scan's tuple position after the attribute lanes when the plan reads `ctid` (a write's or lock's junk column).
pub fn rowid_lane(site: &mut SiteDesc, atts: &Atts) {
    if atts.rowid {
        site.add_lane(LaneDesc::rowid(), false);
    }
}

/// A scan column's lane: the ABI bytes (`kind`/`width`/`region`) and, decided here once, its `Rep` and `LaneLife` (lowerv3 design §1.1).
/// `k` is the lane's index in its site (a code lane's `DictRef`).
pub fn lane_of(k: usize, attno: i16, att: AttFact, am: Am) -> LaneDesc {
    let mut l = LaneDesc::attr(attno);
    let pgrc = am == Am::Pgrcolumnar;
    l.num = att.num_facts();
    if att.byval {
        // the width is the published stride: pgrc lays a by-value column down at the type's length; a heap /
        // index / fetch site publishes every by-value lane as a datum word (`Lane::byval(.., 8, ..)` in
        // hosts' heap_source.rs / index_source.rs / fetch_site.rs; lower.md §15.14) — the type's length is the
        // expression's (`AttFact.typlen`), not the lane's
        l.width = if pgrc { att.typlen.clamp(1, 8) as u8 } else { 8 };
        if pgrc && fixed_value_type(att.typid) {
            l.kind = lane::KIND_FIXED;
        }
        // D198: a float / bool lane is a word whatever its ABI kind byte says
        l.rep = Rep::Word { bytes: l.width };
        l.life = LaneLife::VALUE;
    } else if pgrc && att.typlen == -1 && att.is_text() && k <= usize::from(u8::MAX) {
        // D1749: a code lane's `DictRef::lane` is a u8 (a type bound, not a cap): a text column past site lane 255 is read
        // as its datum (the by-reference arm below), never as the codes of lane `k as u8`
        l.kind = lane::KIND_CODES;
        l.width = 4;
        l.region = lane::REGION_DICTIONARY;
        l.rep = Rep::Code { dict: DictRef::lane(k as u8) };
        l.life = LaneLife::DICTIONARY;
    } else if pgrc && l.num.is_some() {
        l.kind = lane::KIND_FIXED;
        l.width = 8;
        l.rep = Rep::PackedNum { facts: l.num, form: NumForm::Window };
        l.life = LaneLife::PGRC;
    } else {
        by_reference(&mut l, att, pgrc);
    }
    l
}

/// D4568 (road `scan.mergefree`, on by default since D4585, `Facts::scan_mergefree`): a merge mark whose order no node above the scan reads
/// (`Builder::merge_free`, facts/order.rs `merge_free_under`: a hash build whose chain order no reader sees, a probe or a
/// Material under readers that take rows in any order, a plain or hashed aggregate whose calls fold the same in any
/// order, a full Sort) is no merge: the scan reads as an unmarked one, its pipeline wide and its text lanes codes, where
/// one merging reader on the leader buys an order nothing reads. A merge join, a sorted group, a Limit, a window, a
/// Unique and the statement's top read the order, so their scans keep the mark. sortmerge T3's planner prices a one-part
/// merge at nothing, so the sorted path wins on pathkeys under hash joins too (TPC-DS q38 / q87: the customer probes;
/// TPC-H q10 / q20 / q21: the nation and supplier builds, D4520). The road drops a mark only where the session runs wide
/// (`Facts::serial` Some(false)): on the leader alone the unmarked scan buys no parallel read, and its code lanes cost
/// more instructions than the one merging reader (a probe under a hashed set operation or a sorted group paid a quarter
/// to three quarters more), so a serial session (W0) and a lowering off a backend (None) keep the mark. The serial class
/// is a front key input (exec inputs/front.rs), so each class has its own graph.
fn merge_unread(b: &Builder<'_, '_>) -> bool {
    if !(b.merge_free && b.facts.scan_mergefree()) {
        return false;
    }
    if b.facts.serial() != Some(false) {
        fires::fire("scan.mergefree.serial"); // shapecheck fire signal: an unread merge mark kept, the session is serial
        return false;
    }
    fires::fire("scan.mergefree"); // shapecheck fire signal: a merge mark no reader reads, dropped
    true
}

/// sortmerge (D4469 road 2): the site merges its parts' sorted streams on `merge` (the plan's `SeqScan.pgrc_merge`). Its
/// windows are runs cut from each part's in turn, so no lane is a code lane (a part's codes are its own dictionary's
/// ranks, and a run of one part follows a run of another): a text column is its datum (`by_reference`). Each key column's
/// lane is a value the host compares row by row: a by-value integer word (int2 / int4 / int8, date, timestamp) or a text
/// or varchar datum.
fn merge_lanes(b: &Builder<'_, '_>, site: &mut SiteDesc, relid: u32, atts: &Atts, merge: &[i16], k: Kind, id: i32) -> Result<(), Refusal> {
    for (i, &a) in atts.as_slice().iter().enumerate() {
        if site.lanes[i].kind == lane::KIND_CODES {
            let att = b.facts.att(relid, a).ok_or(Refusal::shape(k, id, "no attribute facts"))?;
            let mut l = LaneDesc::attr(a);
            l.num = att.num_facts();
            by_reference(&mut l, att, true);
            site.lanes[i] = l;
        }
    }
    for &a in merge {
        let i = atts.lane(a).ok_or(Refusal::shape(k, id, "a merge key column with no lane"))?;
        let att = b.facts.att(relid, a).ok_or(Refusal::shape(k, id, "no attribute facts"))?;
        let l = &site.lanes[i];
        let value = match l.kind {
            // a signed word's order is the type's (never an unsigned oid's or xid's)
            lane::KIND_BYVAL => {
                use types_core::catalog::{DATEOID, INT2OID, INT4OID, INT8OID, TIMESTAMPOID, TIMESTAMPTZOID};
                matches!(l.rep, Rep::Word { bytes: 2 | 4 | 8 }) && matches!(att.base, INT2OID | INT4OID | INT8OID | DATEOID | TIMESTAMPOID | TIMESTAMPTZOID)
            }
            // bpchar's comparison ignores trailing blanks: never a byte-order key
            lane::KIND_BYREF => matches!(att.base, crate::catalog::TEXTOID | crate::catalog::VARCHAROID),
            _ => false,
        };
        if !value {
            return Err(Refusal::shape(k, id, "a merge key column that is not an integer, date, timestamp or text value"));
        }
    }
    if !site.set_merge(merge) {
        return Err(Refusal::shape(k, id, "a merge key of more columns than an index key"));
    }
    Ok(())
}

/// `lane_of`'s by-reference arm: the column's datum per row, a pointer into the source's arena (pgrc) or page (heap).
fn by_reference(l: &mut LaneDesc, att: AttFact, pgrc: bool) {
    l.kind = lane::KIND_BYREF;
    l.width = 8;
    l.region = if pgrc { lane::REGION_ARENA } else { lane::REGION_PAGE };
    l.rep = Rep::of_storage(att.typlen, false);
    l.life = if pgrc { LaneLife::PGRC } else { LaneLife::PAGE };
}

/// D3016: a pgrcolumnar scan's text code lane whose column holds nearly a value a row (`cost::near_unique` over
/// `Facts::att_entries` / `rel_rows`, PS1-29's rule) of at most `cost::DATUM_TEXT_WIDTH_MAX` bytes on average
/// (`Facts::att_width`) and which is read only as call arguments (`call_reads_only`) is laid
/// down by reference (`lane_of`'s by-reference arm): each row's image in the granule's order, where its codes would make
/// the source build the part's dictionary (a verbatim part's ranked codes) and the program render every row through it,
/// a random read over about a part's rows of entries. Nothing reads the codes: no group, sort or join key, no min / max,
/// no late decode of an output, no comparison, LIKE, IN, null test or zone predicate over it.
fn datum_text_lanes<'p>(b: &mut Builder<'p, '_>, site: &mut SiteDesc, plan: &'p Plan<'p>, varno: i32, scanrelid: u32, relid: u32, atts: &Atts, target: Option<&types_nodes::bitmapset::Bitmapset<'_>>) {
    // a whole-row or system column reads every column; a write's target carries its columns past the scan
    if atts.sys != 0 || b.carry.as_ref().is_some_and(|c| crate::ops::shared::carry::under_target(b, c.rti, scanrelid)) {
        return;
    }
    let Some(rows) = b.facts.rel_rows(relid) else { return };
    for (k, &a) in atts.as_slice().iter().enumerate() {
        if site.lanes[k].kind != lane::KIND_CODES || !b.facts.att_entries(relid, a).is_some_and(|n| gravity_program::cost::near_unique(rows, n)) {
            continue;
        }
        // a granule's images live in each reading source's arena: wide values keep the one shared dictionary copy
        if !b.facts.att_width(relid, a).is_some_and(|w| w <= gravity_program::cost::DATUM_TEXT_WIDTH_MAX) {
            continue;
        }
        let Some(att) = b.facts.att(relid, a) else { continue };
        if !call_reads_only(b, plan, varno, a, target) {
            continue;
        }
        let mut l = LaneDesc::attr(a);
        l.num = att.num_facts();
        by_reference(&mut l, att, true);
        site.lanes[k] = l;
        fires::fire("scan.datum_text"); // shapecheck fire signal (D3016): a near-unique text column read by calls alone
    }
}

/// Every read of scan column `a` is an argument of a function call (`substring(c ..)`, `upper(c)`, `length(c)`; through
/// casts and relabels): in the scan's quals and target list, and where an entry is the bare column, in the parent's
/// expressions that read that entry (a join's, an Agg's or Group's that does not group by it, a Result's: the readers
/// facts::demand `parent_reads` lists; any other parent reads it as it is). A physical target list's bare entry no
/// consumer reads (`target`: `cb_target_cols`) is no read. False for a node the walk does not know.
fn call_reads_only<'p>(b: &mut Builder<'p, '_>, plan: &'p Plan<'p>, varno: i32, a: i16, target: Option<&types_nodes::bitmapset::Bitmapset<'_>>) -> bool {
    let is_col = |n: Node<'_>| n.as_var().is_some_and(|v| v.varno == varno && v.varlevelsup == 0 && v.varattno == a);
    let Some(mut n) = call_reads(plan.qual.iter(), &is_col) else { return false };
    for (i, te) in plan.targetlist.iter().enumerate() {
        let bare = te.as_target_entry().is_some_and(|t| is_col(through_casts(t.expr)));
        let got = match bare {
            false => call_reads(core::iter::once(te), &is_col),
            true if target.is_some_and(|t| !t.is_member(i32::from(a))) => Some((0, 0)),
            true => parent_call_reads(b, plan, i),
        };
        let Some((reads, args)) = got else { return false };
        n = (n.0 + reads, n.1 + args);
    }
    n.0 > 0 && n.0 == n.1
}

/// `call_reads` of `plan`'s target entry `i` (0-based) in its parent's expressions (facts::demand `parent_reads`, the one
/// per-parent read table); None for a parent the table does not list or an Agg / Group that groups by the entry.
fn parent_call_reads<'p>(b: &mut Builder<'p, '_>, plan: &'p Plan<'p>, i: usize) -> Option<(usize, usize)> {
    let pr = crate::facts::demand::parent_reads(b, plan)?;
    if pr.keys.iter().any(|&c| usize::try_from(c).is_ok_and(|c| c == i + 1)) {
        return None;
    }
    let is_entry = |n: Node<'_>| n.as_var().is_some_and(|v| v.varno == pr.side && v.varlevelsup == 0 && usize::try_from(v.varattno).is_ok_and(|x| x == i + 1));
    call_reads(pr.exprs.iter().copied(), &is_entry)
}

/// In `nodes`: (the reads of the column `is_col` names, those that are a function call's argument through casts); None
/// for a node `expr::kids` does not know.
fn call_reads<'p>(nodes: impl Iterator<Item = Node<'p>>, is_col: &dyn Fn(Node<'_>) -> bool) -> Option<(usize, usize)> {
    use types_nodes::primnodes::CoercionForm::{COERCE_EXPLICIT_CALL, COERCE_SQL_SYNTAX};
    let (mut reads, mut args) = (0usize, 0usize);
    for n in nodes {
        let known = expr::walk(n, &mut |x| {
            reads += usize::from(is_col(x));
            if let Some(f) = x.as_func_expr().filter(|f| matches!(f.funcformat, COERCE_EXPLICIT_CALL | COERCE_SQL_SYNTAX)) {
                args += f.args.iter().filter(|&x| is_col(through_casts(x))).count();
            }
            true
        });
        if !known {
            return None;
        }
    }
    Some((reads, args))
}

/// `n` under its casts, relabels and coercions through I/O.
fn through_casts(n: Node<'_>) -> Node<'_> {
    crate::walk::stack_check(); // D3748
    use types_nodes::primnodes::CoercionForm::{COERCE_EXPLICIT_CAST, COERCE_IMPLICIT_CAST};
    if let Some(r) = n.as_relabel_type() {
        return through_casts(r.arg);
    }
    if let Some(c) = n.as_coerce_via_io() {
        return through_casts(c.arg);
    }
    match n.as_func_expr() {
        Some(f) if matches!(f.funcformat, COERCE_EXPLICIT_CAST | COERCE_IMPLICIT_CAST) && !f.args.is_nil() => through_casts(f.args.nth(0)),
        _ => n,
    }
}

/// N14 (D2859's lift): a pgrcolumnar scan's unconstrained numeric column (no typmod, no domain) whose only readers are
/// min / max calls of the Agg right over the scan, or (D3139) float casts in its plain aggregates (`packed_minmax_only`),
/// takes the chunks' packed codes: `KIND_FIXED` words with the chunk's elected scale in the lane (`Rep::PackedNumDyn`), a
/// verbatim chunk's images by reference. The keyed extreme keys a code at its scale with no image (lowerv4 num_side
/// `step`); a cast reads it at its scale (lowerv4 rep/packednum/float_cast.rs).
fn packed_dyn_lanes<'p>(b: &mut Builder<'p, '_>, site: &mut SiteDesc, plan: &'p Plan<'p>, varno: i32, relid: u32, atts: &Atts) {
    for (i, &a) in atts.as_slice().iter().enumerate() {
        let unconstrained = b.facts.att(relid, a).is_some_and(|f| f.typid == types_core::catalog::NUMERICOID && f.typmod < 4);
        if unconstrained && site.lanes[i].rep == Rep::VarlenaPtr && crate::ops::shared::scan::packed_minmax_only(b, plan, varno, a) {
            // as `lane_of` lays a numeric(p, s) lane down (its region the attribute lane's)
            let mut l = LaneDesc::attr(a);
            (l.kind, l.width, l.rep, l.life) = (lane::KIND_FIXED, 8, Rep::PackedNumDyn, LaneLife::PGRC);
            site.lanes[i] = l;
        }
    }
}

/// FLOATZONE: the float comparisons as zone operators (pg_operator float4 620-625, float8 670-675, float48 1120-1125, float84 1130-1135).
/// A float lane's zone records hold the canonical order key (pgrc_meta `KeyDerivation::Float32` / `Float64`), exact under
/// PG's float order (every NaN one value, greatest; -0 = 0), so the constant's key answers the operator.
pub fn float_op(opno: u32) -> Option<ZoneOp> {
    use ZoneOp::{Eq, Ge, Gt, Le, Lt, Ne};
    match opno {
        670..=675 => [Eq, Ne, Lt, Le, Gt, Ge].get((opno - 670) as usize).copied(),
        620..=625 | 1120..=1125 | 1130..=1135 => [Eq, Ne, Lt, Gt, Le, Ge].get((opno % 10) as usize).copied(),
        _ => None,
    }
}
/// The order key of a non-NULL float4 / float8 constant word (`order_key::transform`, the writer's transform).
pub fn float_key(consttype: u32, word: i64) -> Option<i64> {
    match crate::catalog::typefacts::row_of(consttype)?.image.float32()? {
        true => Some(order_key::transform::f32_key_from_datum(word as u32 as u64)),
        false => Some(order_key::transform::f64_key_from_datum(word as u64)),
    }
}

/// SRC-09: the prefix key of a text constant's payload (`order_key::transform::memcmp_var_prefix_key`, the writer's
/// transform for a text column under a C-class collation: 8 bytes, zero-padded, monotone with ties).
pub fn text_prefix_key(payload: &[u8]) -> i64 {
    order_key::transform::memcmp_var_prefix_key(payload)
}

/// P5-11: the probe word of a constant's equality bytes (`order_key::bloom::bloom_pack`: the writer's `meta_hash128` of the
/// bytes its blooms insert: a text payload under a deterministic collation, P5-13 a memcmp-ordered fixed image).
pub fn eq_bloom_key(bytes: &[u8]) -> i64 {
    order_key::bloom::bloom_pack(bytes)
}

/// P5-13: the order key of a memcmp-ordered fixed image (`order_key::transform`, the writer's `MemcmpFixed` derivation): the
/// image itself up to 8 bytes (Exact: macaddr, macaddr8), its 8-byte prefix past that (Coarse, monotone with ties: uuid).
pub fn fixed_order_key(image: &[u8]) -> i64 {
    if image.len() <= 8 {
        order_key::transform::memcmp_fixed_exact_key(image)
    } else {
        order_key::transform::memcmp_fixed_prefix_key(image)
    }
}

const fn flip(op: ZoneOp) -> ZoneOp {
    match op {
        ZoneOp::Lt => ZoneOp::Gt,
        ZoneOp::Le => ZoneOp::Ge,
        ZoneOp::Gt => ZoneOp::Lt,
        ZoneOp::Ge => ZoneOp::Le,
        o => o,
    }
}

fn const_i64(n: Node<'_>) -> Option<i64> {
    crate::ops::shared::scan::zone_word(n.as_const()?)
}

/// `target`: the SeqScan's `cb_target_cols` when known (a physical target list projects every column, so `tlist` alone
/// would leave no lane filter-only).
pub fn zone(b: &Builder<'_, '_>, site: &mut SiteDesc, varno: i32, atts: &Atts, qual: &NodeList<'_>, tlist: &NodeList<'_>, target: Option<&types_nodes::bitmapset::Bitmapset<'_>>) {
    let projected = projected_of(b.arena, varno, atts, tlist, target);
    let relid = site.relation;
    for q in qual.iter() {
        // NEW-72: a bound form's constant is the execution's (lowerv4 binds it over a word lane, declares nothing over any
        // other), so the keyed site holds its shape alone and nothing here reads it
        let pred = match crate::ops::shared::scan::bound_form(q, varno, &|_| true) {
            Some((a, op, c)) => atts.lane(a).filter(|&k| site.lanes[k].kind == lane::KIND_BYVAL && !matches!(site.lanes[k].rep, Rep::PackedNum { .. })).and_then(|k| Some(ZonePred::bound(k as u16, op, 0, c.as_const()?.constlen as u8))),
            None => zone_pred(q, varno, atts, site, &|a| b.facts.att(relid, a)),
        };
        if let Some(p) = pred {
            // one entry per distinct predicate, as lowerv4's verdict_zone declares them: the site is the content key's,
            // so every by-value constant a static predicate bakes must be here (a widening cast's and a float's too)
            if !site.zone_preds().contains(&p) {
                let k = p.lane as usize;
                site.add_zone(p);
                if projected.lane(atts.attnos[k]).is_none() && !site.filter_lanes.contains(k) {
                    site.set_filter_only(k);
                }
            }
        } else if let Some(k) = text_code_compare(q, varno, atts, site, b.facts) {
            // a code lane's text `=` / `<>` with any constant: lowerv4's verdict_zone declares its zone predicate (bound per
            // execution, so nothing of it is keyed here); a lane only quals read is filter-only as a zone lane is
            if projected.lane(atts.attnos[k]).is_none() && !site.filter_lanes.contains(k) {
                site.set_filter_only(k);
            }
        }
    }
}

/// Qual `q` as `lane = c` / `lane <> c` (either side) over a code lane of `varno` and a non-NULL text constant under a
/// deterministic collation (equal values are equal bytes): the lane.
fn text_code_compare(q: Node<'_>, varno: i32, atts: &Atts, site: &SiteDesc, facts: &dyn crate::Facts) -> Option<usize> {
    let op = q.as_op_expr().filter(|o| o.args.len() == 2)?;
    let lane_of = |n: Node<'_>| n.as_var().filter(|v| v.varno == varno && v.varlevelsup == 0).and_then(|v| atts.lane(v.varattno));
    let (a0, a1) = (op.args.nth(0), op.args.nth(1));
    let (k, other) = match (lane_of(a0), lane_of(a1)) {
        (Some(k), None) => (k, a1),
        (None, Some(k)) => (k, a0),
        _ => return None,
    };
    other.as_const().filter(|c| !c.constisnull && !c.constbyval && c.constlen == -1)?;
    let text = matches!(zone_op(op.opno), Some(ZoneOp::Eq | ZoneOp::Ne)) && facts.coll(op.inputcollid).bytes_eq;
    (site.lanes[k].kind == lane::KIND_CODES && text).then_some(k)
}

/// `att`: the scanned relation's column facts, for a domain column's relabel to its base (PS1-10, `casts::domain_base`,
/// the reading lowerv4's `verdict_zone::lane_of` shares).
fn zone_pred(q: Node<'_>, varno: i32, atts: &Atts, site: &SiteDesc, att: &dyn Fn(i16) -> Option<AttFact>) -> Option<ZonePred> {
    let scan_var = |n: Node<'_>| {
        let n = match n.as_func_expr() {
            Some(f) if f.args.len() == 1 && types_core::quiet::int_widen(f.funcid) => f.args.nth(0),
            _ => n,
        };
        domain_base(n, varno, att).as_var().filter(|v| v.varno == varno && v.varlevelsup == 0).and_then(|v| atts.lane(v.varattno))
    };
    if let Some(t) = q.as_null_test() {
        if t.nulltesttype == NullTestType::IS_NOT_NULL && !t.argisrow {
            let k = scan_var(t.arg?)?;
            return Some(ZonePred::word(k as u16, ZoneOp::NotNull, 0));
        }
        return None;
    }
    let op = q.as_op_expr()?;
    if op.args.len() != 2 {
        return None;
    }
    let (a0, a1) = (op.args.nth(0), op.args.nth(1));
    let (k, other, flipped) = match (scan_var(a0), scan_var(a1)) {
        (Some(k), None) => (k, a1, false),
        (None, Some(k)) => (k, a0, true),
        _ => return None,
    };
    let float = float_op(op.opno);
    let zop = zone_op(op.opno).or(float)?;
    let zop = if flipped { flip(zop) } else { zop };
    let l = site.lanes[k];
    if float.is_some() {
        let fixed = l.kind == lane::KIND_FIXED && matches!(l.rep, Rep::Word { bytes: 4 | 8 });
        let c = other.as_const().filter(|_| fixed)?;
        return Some(ZonePred::word(k as u16, zop, float_key(c.consttype, const_i64(other)?)?));
    }
    if l.kind != lane::KIND_BYVAL {
        return None;
    }
    Some(ZonePred::word(k as u16, zop, const_i64(other)?))
}

pub fn map_seq_scan<'p>(b: &mut Builder<'p, '_>, ss: &'p SeqScan<'p>) -> Result<Open<'p>, Refusal> {
    let plan = &ss.scan.plan;
    let id = plan.plan_node_id;
    let k = Kind::SeqScan;
    let rel = b.relation(ss.scan.scanrelid, k, id)?;
    let am = if rel.rows { Am::Heap } else { rel.am }; // PGRCSERVE G4: the rows road is heap over the AM's slot scan
    if !matches!(am, Am::Heap | Am::Pgrcolumnar) {
        return Err(Refusal::shape(k, id, "table access method"));
    }
    let varno = ss.scan.scanrelid as i32;
    let mut atts = Atts::new(b.arena);
    match &ss.cb_scan_cols {
        // D2633: the root of a CTE's plan carries the columns its CteScans read and its quals (demand.rs `read`), not the
        // planner's path target (every column of the CTE)
        _ if crate::facts::demand::cte::cte_root(b, plan).is_some() => read_vars(b, plan, &[&plan.qual], varno, k, id, &mut atts)?,
        Some(cols) => {
            for a in cols.iter() {
                if a <= 0 {
                    return Err(Refusal::shape(k, id, "cb_scan_cols outside the attribute range"));
                }
                if !atts.add(a as i16) {
                    return Err(Refusal::shape(k, id, "more than 4096 lanes (the sanity limit)"));
                }
            }
            let mut extra = Atts::new(b.arena);
            scan_vars(&[&plan.targetlist, &plan.qual], varno, k, id, &mut extra)?;
            (atts.rowid, atts.sys) = (extra.rowid, extra.sys);
        }
        // a system or whole-row column leaves the planner's set unknown: the target entries a consumer reads (a physical
        // target list's unread bare columns get no lane) and the quals' columns; a whole-row reference still reads every one
        None => read_vars(b, plan, &[&plan.qual], varno, k, id, &mut atts)?,
    }
    if !crate::ops::shared::carry::atts_for(b, ss.scan.scanrelid, &mut atts) || !whole_row_atts(b, rel.relid, &mut atts) {
        return Err(Refusal::shape(k, id, "more than 4096 lanes (the sanity limit)"));
    }
    let merge = ss.pgrc_merge;
    if !merge.is_empty() && !rel.rows && rel.am != Am::Pgrcolumnar {
        return Err(Refusal::shape(k, id, "a merge mark on a scan of a relation that is not pgrcolumnar"));
    }
    let merge = if !merge.is_empty() && merge_unread(b) { &[][..] } else { merge };
    // r15 (PGRCSERVE G4 x D4469 road 2): the rows road reads the AM's slot scan, which merges no parts: the order the
    // plan relies on comes from a sort on the mark's key above the scan (`rows_merge_sort`), its site unmarked
    let rows_sort = !merge.is_empty() && rel.rows;
    for &a in merge.iter().filter(|_| !rows_sort) {
        if a <= 0 || !atts.add(a) {
            return Err(Refusal::shape(k, id, "a merge key column outside the attribute range or past the lane limit"));
        }
    }
    let mut site = SiteDesc::range_source(b.arena, rel.relid, atts.as_slice());
    if rel.rows {
        site = site.rows_via_am();
    }
    lanes(b, &mut site, rel.relid, am, atts.as_slice(), k, id)?;
    if am == Am::Pgrcolumnar {
        packed_dyn_lanes(b, &mut site, plan, varno, rel.relid, &atts);
        datum_text_lanes(b, &mut site, plan, varno, ss.scan.scanrelid, rel.relid, &atts, ss.cb_target_cols.as_ref());
    }
    if !merge.is_empty() && !rows_sort {
        merge_lanes(b, &mut site, rel.relid, &atts, merge, k, id)?;
        fires::fire("scan.merged"); // shapecheck fire signal (D4568): a merge mark a reader reads, kept (one merging reader)
    }
    rowid_lane(&mut site, &atts);
    scan_lanes(b, id, ss.scan.scanrelid, rel.relid, &atts);
    if am == Am::Pgrcolumnar {
        zone(b, &mut site, varno, &atts, &plan.qual, &plan.targetlist, ss.cb_target_cols.as_ref());
        qual_only(b, id, rel.relid, &site, varno, &atts, &plan.qual, &plan.targetlist, ss.cb_target_cols.as_ref());
        // PS1-05: what an eager InitPlan's charge is weighed against (graph `scan_charge`)
        b.graph.scan_costs.push((id, crate::graph::scan_charge(plan)));
    }
    // a merged site (D4469 road 2: one merge on the leader; D4568: only where a reader reads its order, `merge_unread`) and
    // a scan the AM vouches an order for (RelFact.sorted_by: a capture's fact from before road 1) are serial sources like
    // an index scan: the order is a whole-scan fact, and the planner too claims it only on the serial path (decision 81).
    // D3002: an aggregate that reads no order (a plain or hashed one over filters and projections only) takes it back
    // eligible and unmerged (ops/agg `unclaimed_sorted`): the planner's rows ride along for that
    // the rows road's tableam scan is this backend's: its pipeline runs on the leader (as a TABLESAMPLE walk's)
    let desc = if rel.rows {
        Pipeline { predicted_rows: Builder::rows(plan), ..Pipeline::serial(0, End::Sink, Reason::Host) }
    } else if rel.nsorted > 0 || site.is_merged() {
        Pipeline { predicted_rows: Builder::rows(plan), ..Pipeline::serial(0, End::Sink, Reason::SortedSource) }
    } else {
        Pipeline::eligible(0, End::Sink, Builder::rows(plan))
    };
    let mut open = Open::new(b.arena, id, Source::Range { site, am, scanrelid: ss.scan.scanrelid as u16, relid: rel.relid }, desc);
    // R15-ROWSORT-B (ROAD2): quals or targets the sort may not run ahead go after it, in the key order (`rows_sort_first`)
    if rows_sort && rows_sort_ahead_ok(b, id).is_none() {
        b.graph.note(k);
        return rows_sort_first(b, plan, open, merge, rel.relid, varno, k, id);
    }
    ops::filters(&mut open, &plan.qual);
    // D4779: the readers' filter a fill-marked CTE's scan takes (facts/cte_fill.rs)
    crate::facts::cte_fill::filters(b, id, varno, &atts, &mut open);
    ops::project(&mut open, &plan.targetlist);
    b.graph.note(k);
    if rows_sort {
        return rows_merge_sort(b, plan, open, merge, rel.relid, varno, k, id);
    }
    Ok(open)
}

pub const ROWS_MERGE_NO_KEY: &str = "a merge mark on a scan on the rows road whose target list carries no key column";

/// r15 (PGRCSERVE G4 x D4469 road 2): a merge mark (`SeqScan.pgrc_merge`: the plan relies on the scan's rows coming in
/// the mark's key order, ASC NULLS LAST lexicographic, the planner's pathkeys for the bare scan) on a scan on the rows
/// road, whose slot scan hands its storage's rows in storage order and merges nothing: a full sort on the key over the
/// scan's output, so the rows leave in the order the plan relies on at any part count (a cursor's portal; a REPEATABLE
/// READ snapshot holding more parts than the plan's claim saw).
/// The keys are the mark's columns the target list carries, in the mark's order. A reader relies only on an order it
/// reads: a mark column the list does not carry is past the prefix the plan reads, or one the quals hold constant (the
/// planner drops its pathkey as redundant), and orders nothing among the rows. Each key sorts as PostgreSQL's own Sort for
/// that pathkey does: its type's default btree `<` (`opdir::builtin_lt`; the mark admits only integer, date, timestamp
/// and text keys) under the column's collation, ascending, NULLS LAST.
/// The sort reads every row of the scan before its first, where PostgreSQL's scan hands each on as it reads it: here
/// only over quals and targets that may run ahead (`rows_sort_ahead_ok`); other ones run after the sort
/// (`rows_sort_first`).
#[allow(clippy::too_many_arguments)]
fn rows_merge_sort<'p>(b: &mut Builder<'p, '_>, plan: &'p Plan<'p>, open: Open<'p>, merge: &[i16], relid: u32, varno: i32, k: Kind, id: i32) -> Result<Open<'p>, Refusal> {
    use crate::catalog::expr_type::{collation_of, type_of};
    use crate::ops::shared::breaker::{close, emit};
    use crate::ops::shared::sort::{key_coll, keys_layout, SortDesc, SortForm, SortKey};
    use gravity_abi::breaker::{BreakerKind, Budget, Spill};
    use mcx::{GrowIn, PgVec};
    use types_nodes::primnodes::OUTER_VAR;
    let alloc = b.alloc.ok_or(Refusal::shape(k, id, "a merge mark on a scan on the rows road with no node allocator"))?;
    let tlist = &plan.targetlist;
    let mut keys = ArenaVec::new_in(b.arena);
    for &a in merge {
        let is_key = |n: &Node<'p>| n.as_target_entry().and_then(|t| t.expr.as_var()).is_some_and(|v| v.varno == varno && v.varattno == a && v.varlevelsup == 0);
        let Some(j) = tlist.iter().position(|n| is_key(&n)) else { continue };
        let te = tlist.nth(j).as_target_entry().ok_or(Refusal::shape(k, id, "a target list entry that is not a TargetEntry"))?;
        let typid = type_of(te.expr).ok_or(Refusal::shape(k, id, "a merge key of unknown type"))?;
        let base = b.facts.att(relid, a).ok_or(Refusal::shape(k, id, "no attribute facts"))?.base;
        let opno = crate::catalog::opdir::builtin_lt(base).filter(|&o| b.facts.key(crate::catalog::KeyReq::sort(o)).and_then(crate::catalog::KeyOrder::dir) == Some(false));
        let opno = opno.ok_or(Refusal::shape(k, id, "a merge key whose type has no default btree ordering"))?;
        keys.push(SortKey::of_type(b.facts, (j + 1) as i16, opno, false, false, typid, key_coll(b.facts, typid, collation_of(te.expr), k, id)?));
    }
    if keys.is_empty() {
        return Err(Refusal::shape(k, id, ROWS_MERGE_NO_KEY));
    }
    // the sort's own list reads each output column (as a planner Sort's and ops/agg `key_sort`'s do)
    let oom = |_| Refusal::shape(k, id, "the rows-road merge sort: out of memory synthesizing its target list");
    let mut out_cols: PgVec<'_, Node<'p>> = mcx::reserved_in(tlist.len(), b.mcx);
    for (j, n) in tlist.iter().enumerate() {
        let te = n.as_target_entry().ok_or(Refusal::shape(k, id, "a target list entry that is not a TargetEntry"))?;
        let typid = type_of(te.expr).ok_or(Refusal::shape(k, id, "an output column of unknown type"))?;
        let var = Node::mk_var(alloc, OUTER_VAR, (j + 1) as i16, typid, -1, collation_of(te.expr), 0).map_err(oom)?;
        out_cols.push_in(Node::mk_target_entry(alloc, var, (j + 1) as i16, te.resname, te.resjunk).map_err(oom)?);
    }
    let list: &'p NodeList<'p> = NodeList::from_slice(alloc, &out_cols[..]).and_then(|l| mcx::alloc_leak_in(alloc, l)).map_err(oom)?;
    let layout = keys_layout(b.arena, keys.iter().map(|x| x.kind));
    let mut open = open;
    open.push(crate::graph::Op::Sort(SortDesc { keys, presorted: 0, form: SortForm::Runs, tlist: list }));
    let slot = close(b, open, BreakerKind::Sort, layout, Budget::Share, Spill::Runs, k, id)?;
    let mut out = emit(b, id, slot);
    ops::project(&mut out, list);
    Ok(out)
}

/// `rows_merge_sort`'s read ahead: the sort runs the scan's quals and targets over every row before its first. Over
/// quals and targets that show nothing (`Effects::NONE`) no reader can tell. Acting ones (a VOLATILE call) would run
/// in storage order where PostgreSQL's run in the key order. Ones that may raise are PostgreSQL's too when it reads
/// every row (no FETCH count, every node above reads its whole input: ops/shared/sort `whole_read`, as `lazy_ahead`'s whole
/// read keeps its form); under a read that may stop (a LIMIT, a merge side, a cursor's FETCH) the sort would raise
/// where PostgreSQL stopped first. None for those two: they run after the sort (`rows_sort_first`), so a scan the
/// plan relies on for its order is served on the rows road, never refused.
fn rows_sort_ahead_ok(b: &Builder<'_, '_>, id: i32) -> Option<crate::effects::Licence> {
    use crate::effects::{denying_bounded, of_plan, quiet_ahead, whole_read, Cx, Effects};
    let cx = Cx::new(b.stmt, b.facts, b.arena);
    let own = crate::lifetime::node_of(b.stmt, id).map_or(Effects::UNKNOWN, |(n, _)| of_plan(&cx, n));
    if let Some(l) = quiet_ahead(own) {
        return Some(l);
    }
    let whole = !b.graph.partial && crate::ops::shared::sort::whole_read(b, id);
    if own.acts || own.may_raise && (!whole || denying_bounded()) {
        return None;
    }
    whole_read(own).ok()
}

/// R15-ROWSORT-B (ROAD2): `rows_merge_sort` for quals or targets the sort may not run ahead (`rows_sort_ahead_ok`). The
/// scan hands its bare columns (each distinct `Var` of this relation its quals and target list read, in first-read
/// order); the sort orders them on the mark's columns the target list carries (`rows_merge_sort`'s keys and operators);
/// then the quals and the targets run over the sorted rows, one row at a time, with each `Var` read from its sorted
/// column. So they run in the key order the plan relies on, each row only when a reader asks for it, as over a merged
/// scan: a read that stops (a LIMIT, a cursor's FETCH, a merge side) stops them too. The sort carries the rows its quals
/// would have dropped.
#[allow(clippy::too_many_arguments)]
fn rows_sort_first<'p>(b: &mut Builder<'p, '_>, plan: &'p Plan<'p>, open: Open<'p>, merge: &[i16], relid: u32, varno: i32, k: Kind, id: i32) -> Result<Open<'p>, Refusal> {
    use crate::catalog::expr_type::collation_of;
    use crate::ops::shared::breaker::{close, emit};
    use crate::ops::shared::sort::{key_coll, keys_layout, SortDesc, SortForm, SortKey};
    use gravity_abi::breaker::{BreakerKind, Budget, Spill};
    use mcx::{GrowIn, PgVec};
    use types_nodes::primnodes::OUTER_VAR;
    let alloc = b.alloc.ok_or(Refusal::shape(k, id, "a merge mark on a scan on the rows road with no node allocator"))?;
    let oom = |_| Refusal::shape(k, id, "the rows-road merge sort: out of memory synthesizing its target lists");
    // the bare columns: each distinct Var of this relation (this query level) the quals and the target list read
    let mut cols: ArenaVec<Node<'p>> = ArenaVec::new_in(b.arena);
    for n in plan.qual.iter().chain(plan.targetlist.iter()) {
        expr::walk(n, &mut |x| {
            if let Some(v) = x.as_var().filter(|v| v.varno == varno && v.varlevelsup == 0) {
                if !cols.iter().any(|c| c.as_var().is_some_and(|c| c.varattno == v.varattno)) {
                    cols.push(x);
                }
            }
            true
        });
    }
    let col_of = |a: i16| cols.iter().position(|c| c.as_var().is_some_and(|c| c.varattno == a));
    // the keys: the mark's columns the target list carries as a bare column (rows_merge_sort's rule), by their column
    let tlist = &plan.targetlist;
    let mut keys = ArenaVec::new_in(b.arena);
    for &a in merge {
        let carried = tlist.iter().any(|n| n.as_target_entry().and_then(|t| t.expr.as_var()).is_some_and(|v| v.varno == varno && v.varattno == a && v.varlevelsup == 0));
        let Some(j) = col_of(a).filter(|_| carried) else { continue };
        let var = cols[j].as_var().ok_or(Refusal::shape(k, id, "a merge key column that is not a Var"))?;
        let typid = var.vartype;
        let base = b.facts.att(relid, a).ok_or(Refusal::shape(k, id, "no attribute facts"))?.base;
        let opno = crate::catalog::opdir::builtin_lt(base).filter(|&o| b.facts.key(crate::catalog::KeyReq::sort(o)).and_then(crate::catalog::KeyOrder::dir) == Some(false));
        let opno = opno.ok_or(Refusal::shape(k, id, "a merge key whose type has no default btree ordering"))?;
        keys.push(SortKey::of_type(b.facts, (j + 1) as i16, opno, false, false, typid, key_coll(b.facts, typid, collation_of(cols[j]), k, id)?));
    }
    if keys.is_empty() {
        return Err(Refusal::shape(k, id, ROWS_MERGE_NO_KEY));
    }
    // the scan's list (its bare columns) and the sort's (each of them by position)
    let mut raw: PgVec<'_, Node<'p>> = mcx::reserved_in(cols.len(), b.mcx);
    let mut sorted: PgVec<'_, Node<'p>> = mcx::reserved_in(cols.len(), b.mcx);
    for (j, &c) in cols.iter().enumerate() {
        let v = c.as_var().ok_or(Refusal::shape(k, id, "a bare column that is not a Var"))?;
        raw.push_in(Node::mk_target_entry(alloc, c, (j + 1) as i16, None, false).map_err(oom)?);
        let out = Node::mk_var(alloc, OUTER_VAR, (j + 1) as i16, v.vartype, v.vartypmod, v.varcollid, 0).map_err(oom)?;
        sorted.push_in(Node::mk_target_entry(alloc, out, (j + 1) as i16, None, false).map_err(oom)?);
    }
    let raw: &'p NodeList<'p> = NodeList::from_slice(alloc, &raw[..]).and_then(|l| mcx::alloc_leak_in(alloc, l)).map_err(oom)?;
    let sorted: &'p NodeList<'p> = NodeList::from_slice(alloc, &sorted[..]).and_then(|l| mcx::alloc_leak_in(alloc, l)).map_err(oom)?;
    // the quals and the target list over the sorted columns
    let mut quals: PgVec<'_, Node<'p>> = mcx::reserved_in(plan.qual.len(), b.mcx);
    for q in plan.qual.iter() {
        quals.push_in(rows_remap(alloc, q, varno, &cols).map_err(oom)?.ok_or(Refusal::shape(k, id, "a qual the rows-road merge sort cannot read over its sorted columns"))?);
    }
    let mut targets: PgVec<'_, Node<'p>> = mcx::reserved_in(tlist.len(), b.mcx);
    for n in tlist.iter() {
        let te = n.as_target_entry().ok_or(Refusal::shape(k, id, "a target list entry that is not a TargetEntry"))?;
        let e = rows_remap(alloc, te.expr, varno, &cols).map_err(oom)?.ok_or(Refusal::shape(k, id, "a target the rows-road merge sort cannot read over its sorted columns"))?;
        targets.push_in(Node::mk_target_entry(alloc, e, te.resno, te.resname, te.resjunk).map_err(oom)?);
    }
    let quals: &'p NodeList<'p> = NodeList::from_slice(alloc, &quals[..]).and_then(|l| mcx::alloc_leak_in(alloc, l)).map_err(oom)?;
    let targets: &'p NodeList<'p> = NodeList::from_slice(alloc, &targets[..]).and_then(|l| mcx::alloc_leak_in(alloc, l)).map_err(oom)?;
    let layout = keys_layout(b.arena, keys.iter().map(|x| x.kind));
    let mut open = open;
    ops::project(&mut open, raw);
    open.push(crate::graph::Op::Sort(SortDesc { keys, presorted: 0, form: SortForm::Runs, tlist: sorted }));
    let slot = close(b, open, BreakerKind::Sort, layout, Budget::Share, Spill::Runs, k, id)?;
    let mut out = emit(b, id, slot);
    ops::filters(&mut out, quals);
    ops::project(&mut out, targets);
    Ok(out)
}

/// `x` with each `Var` of relation `varno` at this query level read from its sorted column: `OUTER_VAR`, its 1-based
/// position in `cols` (`rows_sort_first`); other nodes copied.
fn rows_remap<'p>(mcx: mcx::Mcx<'p>, x: Node<'p>, varno: i32, cols: &[Node<'p>]) -> types_error::PgResult<Option<Node<'p>>> {
    crate::walk::stack_check(); // D3748
    if let Some(v) = x.as_var().filter(|v| v.varno == varno && v.varlevelsup == 0) {
        if let Some(j) = cols.iter().position(|c| c.as_var().is_some_and(|c| c.varattno == v.varattno)) {
            return Node::mk_var(mcx, types_nodes::primnodes::OUTER_VAR, (j + 1) as i16, v.vartype, v.vartypmod, v.varcollid, 0).map(Some);
        }
    }
    nodes_core::expression_tree_mutator_dyn(mcx, x, &mut |y| rows_remap(mcx, y, varno, cols))
}

/// One index clause split by its node form: the key's form under a btree, the operator, the index-column side, the value.
fn clause<'p>(q: Node<'p>, k: Kind, id: i32) -> Result<(KeyForm, u32, Node<'p>, Node<'p>), Refusal> {
    if let Some(op) = q.as_op_expr() {
        if op.args.len() != 2 {
            return Err(Refusal::shape(k, id, "an index clause that is not binary"));
        }
        Ok((KeyForm::Op, op.opno, op.args.nth(0), op.args.nth(1)))
    } else if let Some(sa) = q.as_scalar_array_op_expr() {
        if !sa.useOr || sa.args.len() != 2 {
            return Err(Refusal::shape(k, id, "an ALL(array) index clause"));
        }
        Ok((KeyForm::Any, sa.opno, sa.args.nth(0), sa.args.nth(1)))
    } else if let Some(t) = q.as_null_test() {
        if t.argisrow {
            return Err(Refusal::shape(k, id, "a row-valued null test index clause"));
        }
        let form = if t.nulltesttype == NullTestType::IS_NULL { KeyForm::Null } else { KeyForm::NotNull };
        Ok((form, 0, t.arg.ok_or(Refusal::shape(k, id, "a null test without an argument"))?, q))
    } else if let Some(rc) = q.as_row_compare_expr() {
        if rc.largs.is_nil() || rc.largs.len() != rc.rargs.len() || rc.largs.len() != rc.opnos.len() {
            return Err(Refusal::shape(k, id, "a row comparison index clause with unlike sides"));
        }
        Ok((KeyForm::Row, rc.opnos.nth(0), rc.largs.nth(0), q))
    } else {
        Err(Refusal::shape(k, id, "an index clause that is not a comparison, an array probe, a null test or a row comparison"))
    }
}

/// The key's form under the index AM: a btree's comparison is a probe bound; another AM's operator is its opclass strategy.
const fn am_form(form: KeyForm, iam: IndexAm) -> KeyForm {
    match (form, iam) {
        (KeyForm::Op, IndexAm::Btree) | (KeyForm::Any, IndexAm::Btree) => form,
        (KeyForm::Op, _) => KeyForm::AmOp,
        (KeyForm::Any, _) => KeyForm::AmAny,
        _ => form,
    }
}

fn free_of_vars(n: Node<'_>) -> bool {
    let mut has_var = false;
    expr::walk(n, &mut |x| {
        has_var |= x.node_tag() == NodeTag::T_Var;
        !has_var
    });
    !has_var
}

/// The index column of a clause side: `INDEX_VAR`'s attno, 1-based; the table attno is 0 for an expression column or one
/// past the facts' key set (brin's and gist's columns are all keys; an INCLUDE column never carries a clause).
fn index_col(n: Node<'_>, idx: &IndexFact, k: Kind, id: i32) -> Result<(i16, u16), Refusal> {
    let v = n.as_var().filter(|v| v.varno == INDEX_VAR && v.varattno > 0).ok_or(Refusal::shape(k, id, "an index clause without the index column on the left"))?;
    let col = v.varattno as usize - 1;
    Ok((if col < idx.nkeys as usize { idx.keys[col] } else { 0 }, v.varattno as u16))
}

pub fn index_keys<'p>(b: &Builder<'p, '_>, quals: &'p NodeList<'p>, idx: &IndexFact, k: Kind, id: i32) -> Result<ArenaVec<IndexKey<'p>>, Refusal> {
    let iam = b.facts.index_am(idx.indexid);
    let mut keys = ArenaVec::new_in(b.arena);
    if quals.len() > limits::KEYS {
        return Err(Refusal::shape(k, id, "more than INDEX_MAX_KEYS index keys"));
    }
    for q in quals.iter() {
        let (form, opno, lhs, value) = clause(q, k, id)?;
        let (attno, col) = index_col(lhs, idx, k, id)?;
        let free = match form {
            KeyForm::Op | KeyForm::Any => free_of_vars(value),
            KeyForm::Row => {
                let rc = q.as_row_compare_expr().unwrap();
                for l in rc.largs.iter() {
                    index_col(l, idx, k, id)?;
                }
                rc.rargs.iter().all(free_of_vars)
            }
            _ => true,
        };
        if !free {
            return Err(Refusal::shape(k, id, "an index key value referencing a column"));
        }
        keys.push(IndexKey { attno, col, opno, value, form: am_form(form, iam) });
    }
    Ok(keys)
}

/// `ORDER BY column <op> value` entries after the clause keys (`KeyForm::OrderBy`), in `ORDER BY` position.
fn order_keys<'p>(keys: &mut ArenaVec<IndexKey<'p>>, orderby: &'p NodeList<'p>, idx: &IndexFact, iam: IndexAm, k: Kind, id: i32) -> Result<(), Refusal> {
    if orderby.is_nil() {
        return Ok(());
    }
    if !iam.order_by_op() {
        return Err(Refusal::shape(k, id, "ORDER BY an index operator over an access method without ordering operators"));
    }
    for o in orderby.iter() {
        let op = o.as_op_expr().filter(|op| op.args.len() == 2).ok_or(Refusal::shape(k, id, "an index ordering that is not `column <op> value`"))?;
        let (attno, col) = index_col(op.args.nth(0), idx, k, id)?;
        if !free_of_vars(op.args.nth(1)) {
            return Err(Refusal::shape(k, id, "an index key value referencing a column"));
        }
        keys.push(IndexKey { attno, col, opno: op.opno, value: op.args.nth(1), form: KeyForm::OrderBy });
    }
    Ok(())
}

fn index_site(b: &Builder<'_, '_>, rel: RelFact, idx: &IndexFact, keys: &[IndexKey<'_>], atts: &Atts, k: Kind, id: i32) -> Result<SiteDesc, Refusal> {
    let mut key_attnos = ArenaVec::with_capacity_in(keys.len(), b.arena);
    let mut slots = ArenaVec::with_capacity_in(keys.len(), b.arena);
    for key in keys {
        key_attnos.push(key.attno);
        slots.push(-1i16);
    }
    let mut site = SiteDesc::index_source(b.arena, rel.relid, idx.indexid, &key_attnos, &slots, atts.as_slice());
    lanes(b, &mut site, rel.relid, Am::Heap, atts.as_slice(), k, id)?;
    rowid_lane(&mut site, atts);
    Ok(site)
}

/// The two ordinary index scans differ in where the rows' columns come from, nothing else: one mapper, the option a field.
struct IndexNode<'p> {
    kind: Kind,
    plan: &'p Plan<'p>,
    scanrelid: u32,
    indexid: u32,
    quals: &'p NodeList<'p>,
    orderby: &'p NodeList<'p>,
    dir: i32,
    recheck: &'p NodeList<'p>,
    /// Index-only: the index's target list (the rows' columns are the index's).
    only: Option<&'p NodeList<'p>>,
}

fn map_index<'p>(b: &mut Builder<'p, '_>, n: IndexNode<'p>) -> Result<Open<'p>, Refusal> {
    let (plan, k, id) = (n.plan, n.kind, n.plan.plan_node_id);
    let rel = b.relation(n.scanrelid, k, id)?;
    if !rel.index_fetch() {
        return Err(Refusal::shape(k, id, "an index scan over a non-heap relation"));
    }
    let idx = b.facts.index(n.indexid).ok_or(Refusal::shape(k, id, "no index facts"))?;
    if idx.relid != rel.relid {
        return Err(Refusal::shape(k, id, "the index is not on the scanned relation"));
    }
    let iam = b.facts.index_am(n.indexid);
    if !iam.gettuple() {
        return Err(Refusal::shape(k, id, "an ordinary scan of an index access method without one (gin / brin answer through a bitmap; an unknown access method)"));
    }
    let mut keys = index_keys(b, n.quals, &idx, k, id)?;
    if n.only.is_none() {
        if let Some(open) = crate::ops::scan::closure::index_morsel(b, rel, n.plan, n.scanrelid, n.indexid, &keys, iam, n.recheck, n.orderby, k)? {
            return Ok(open);
        }
    } else if let Some(indextlist) = n.only {
        if let Some(open) = crate::ops::scan::closure::index_only_full(b, rel, n.plan, n.scanrelid, n.indexid, &keys, iam, n.orderby, indextlist, n.dir, k)? {
            return Ok(open);
        }
    }
    order_keys(&mut keys, n.orderby, &idx, iam, k, id)?;
    let mut atts = Atts::new(b.arena);
    match n.only {
        None => {
            read_vars(b, plan, &[&plan.qual], n.scanrelid as i32, k, id, &mut atts)?;
            if !crate::ops::shared::carry::atts_for(b, n.scanrelid, &mut atts) || !whole_row_atts(b, rel.relid, &mut atts) {
                return Err(Refusal::shape(k, id, "more than 4096 lanes (the sanity limit)"));
            }
        }
        Some(indextlist) => {
            // the index columns the scan's consumers read (demand.rs, as an index scan's: PG hands an index-only scan under
            // an Agg or a join the index's whole target list), its quals and its recheck: a column no one reads gets no
            // lane, so count(*) over an index of expressions reads none (random rnd-3147). D2900: a btree's recheckqual is
            // never evaluated (a btree entry is exact: PostgreSQL's IndexOnlyNext rechecks only where the AM sets
            // xs_recheck, which btree never does; no program compiles it, lowerv4 props/premise.rs), so its columns get no
            // lane: count(*) WHERE id <= K reads no index column per row. Another AM's lossy recheck reads the AM's image
            // in the host (hosts index_window.rs only_passes), its lanes kept as before.
            let mut cols = Atts::new(b.arena);
            let reads: &[&NodeList<'p>] = if iam == IndexAm::Btree { &[&plan.qual] } else { &[&plan.qual, n.recheck] };
            read_vars(b, plan, reads, INDEX_VAR, k, id, &mut cols)?;
            no_new_sys(0, k, id, &mut cols)?;
            // C5b-2: a CURRENT OF cursor's identity columns (entry current_of.rs `add`): the scan's own `ctid` (the index
            // entry's heap tid, the RowId lane) and `tableoid`; any other column of the relation is an index column's
            let mut own = Atts::new(b.arena);
            scan_vars(&[&plan.targetlist], n.scanrelid as i32, k, id, &mut own)?;
            if !own.as_slice().is_empty() || own.sys & !TABLEOID_BIT != 0 {
                return Err(Refusal::shape(k, id, "an index-only scan's own column other than ctid / tableoid"));
            }
            (atts.rowid, atts.sys) = (own.rowid, own.sys);
            for &c in cols.as_slice() {
                let e = indextlist.nth(c as usize - 1).as_target_entry().and_then(|t| t.expr.as_var()).ok_or(Refusal::shape(k, id, "an expression index column"))?;
                if e.varattno <= 0 || !atts.add(e.varattno) {
                    return Err(Refusal::shape(k, id, "an index column outside the attribute range"));
                }
            }
        }
    }
    let mut site = index_site(b, rel, &idx, &keys, &atts, k, id)?;
    // an order-free index-only walk the pool reads by leaf pages (closure.rs `index_only_leaves`)
    let leaves = n.only.is_some() && crate::ops::scan::closure::index_only_leaves(b, rel, plan, n.indexid, iam, n.orderby, atts.rowid || atts.sys != 0, &keys);
    if let (true, gravity_program::SiteKind::IndexSource { nkeys, natts, .. }) = (leaves, site.kind) {
        site.kind = gravity_program::SiteKind::IndexSource { nkeys, natts, worker: false, leaves: true, ranges: false };
    }
    // an ordered walk whose order the statement's emit keeps, read by the pool in key ranges (closure.rs `index_ranges`)
    let ranges = !leaves && crate::ops::scan::closure::index_ranges(b, rel, plan, n.indexid, iam, n.orderby, atts.rowid || atts.sys != 0, n.dir < 0, &keys);
    if let (true, gravity_program::SiteKind::IndexSource { nkeys, natts, .. }) = (ranges, site.kind) {
        site.kind = gravity_program::SiteKind::IndexSource { nkeys, natts, worker: false, leaves: false, ranges: true };
    }
    scan_lanes(b, id, n.scanrelid, rel.relid, &atts);
    // PGRCSERVE M3: over a relation on the rows road the walk is the heap protocol, as its range site (the host's fetch is the AM's)
    let source = Source::Index { site, am: if rel.rows { Am::Heap } else { rel.am }, scanrelid: n.scanrelid as u16, relid: rel.relid, indexid: n.indexid, keys, only: n.only.is_some(), backward: n.dir < 0, recheck: n.recheck, iam };
    let pipeline = if leaves || ranges { Pipeline::eligible(0, End::Sink, Builder::rows(plan)) } else { Pipeline::serial(0, End::Sink, Reason::IndexSource) };
    let mut open = Open::new(b.arena, id, source, pipeline);
    ops::filters(&mut open, &plan.qual);
    ops::project(&mut open, &plan.targetlist);
    b.graph.note(k);
    Ok(open)
}

pub fn map_index_scan<'p>(b: &mut Builder<'p, '_>, is: &'p IndexScan<'p>) -> Result<Open<'p>, Refusal> {
    map_index(b, IndexNode { kind: Kind::IndexScan, plan: &is.scan.plan, scanrelid: is.scan.scanrelid, indexid: is.indexid, quals: &is.indexqual, orderby: &is.indexorderby, dir: is.indexorderdir, recheck: &is.indexqualorig, only: None })
}

pub fn map_index_only_scan<'p>(b: &mut Builder<'p, '_>, is: &'p IndexOnlyScan<'p>) -> Result<Open<'p>, Refusal> {
    map_index(b, IndexNode { kind: Kind::IndexOnlyScan, plan: &is.scan.plan, scanrelid: is.scan.scanrelid, indexid: is.indexid, quals: &is.indexqual, orderby: &is.indexorderby, dir: is.indexorderdir, recheck: &is.recheckqual, only: Some(&is.indextlist) })
}

pub fn map_values_scan<'p>(b: &mut Builder<'p, '_>, vs: &'p ValuesScan<'p>) -> Result<Open<'p>, Refusal> {
    let plan: &Plan<'p> = &vs.scan.plan;
    let id = plan.plan_node_id;
    let open = Open::new(b.arena, id, Source::Values { scanrelid: vs.scan.scanrelid as u16, lists: &vs.values_lists }, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite));
    let mut open = crate::ops::subplan::attach_values(b, &vs.values_lists, Kind::ValuesScan, id, open)?;
    crate::ops::shared::record_row::values_record_row(b, vs.scan.scanrelid, plan, &vs.values_lists);
    ops::filters(&mut open, &plan.qual);
    ops::project(&mut open, &plan.targetlist);
    b.graph.note(Kind::ValuesScan);
    Ok(open)
}

/// The source family (D3): one row per scan node kind — where its mapper lives, the `Source` it becomes and its options as
/// the descriptor's fields. A new member is a row here, a `Source` form and a walk arm; `tests/ledger.rs` holds the rows
/// to the ledger (a member is mapped, or excluded by a ruling).
pub struct Member {
    pub kind: Kind,
    pub module: &'static str,
    pub source: &'static str,
    pub options: &'static str,
}

pub const FAMILY: [Member; 18] = [
    Member { kind: Kind::SeqScan, module: "sources", source: "Range", options: "am lanes rowid scan-lanes zone" },
    Member { kind: Kind::IndexScan, module: "sources", source: "Index", options: "iam keys(op any null notnull amop amany row orderby) backward recheck rowid scan-lanes" },
    Member { kind: Kind::IndexOnlyScan, module: "sources", source: "Index only", options: "iam keys backward recheck" },
    Member { kind: Kind::BitmapHeapScan, module: "closure", source: "Bitmap", options: "am plan recheck rowid scan-lanes zone" },
    Member { kind: Kind::BitmapIndexScan, module: "closure", source: "BitmapPlan::Index", options: "keys rows" },
    Member { kind: Kind::BitmapAnd, module: "closure", source: "BitmapPlan::And", options: "plans" },
    Member { kind: Kind::BitmapOr, module: "closure", source: "BitmapPlan::Or", options: "plans" },
    Member { kind: Kind::TidScan, module: "closure", source: "Tid", options: "form(points current) quals" },
    Member { kind: Kind::TidRangeScan, module: "closure", source: "Tid", options: "form(range) quals" },
    Member { kind: Kind::SampleScan, module: "closure", source: "Sample", options: "method handler args repeatable" },
    Member { kind: Kind::FunctionScan, module: "srf", source: "Const + Op::Srf(From)", options: "functions(kind incl. expr) ordinality cols" },
    Member { kind: Kind::ValuesScan, module: "sources", source: "Values", options: "lists" },
    Member { kind: Kind::TableFuncScan, module: "closure", source: "TableFunc", options: "func cols ordinality" },
    Member { kind: Kind::CteScan, module: "subplan", source: "Breaker (the CTE's spool)", options: "slot" },
    Member { kind: Kind::NamedTuplestoreScan, module: "closure", source: "Named", options: "name" },
    Member { kind: Kind::WorkTableScan, module: "setops", source: "Breaker (the work table)", options: "slot" },
    Member { kind: Kind::SubqueryScan, module: "subplan", source: "the child's pipeline", options: "-" },
    Member { kind: Kind::ForeignScan, module: "-", source: "excluded", options: "milestone-2.md §5" },
];

#[cfg(test)]
mod tests {
    #[test]
    fn widening_integer_casts_are_the_three_builtins() {
        // pg_proc: i2toi4 313, int28 754, int48 481; the narrowing int4(int8) 480 / int2(int4) 314 can raise, never widen
        assert!([313, 754, 481].iter().all(|&f| types_core::quiet::int_widen(f)));
        assert!([480, 314, 238, 316, 482].iter().all(|&f| !types_core::quiet::int_widen(f)));
    }

    /// PS1-10: a relabel is read through only from a domain column to the domain's by-value base: never between built-in
    /// types whose words differ (oid's zero-extended word read as int4), never from a domain to another type its base
    /// relabels to, never over a by-reference base (numeric, text).
    #[test]
    fn domain_relabel_reads_through_to_the_base_only() {
        use crate::catalog::{casts::domain_to_base, AttFact};
        let dom = |base: u32| AttFact { base, ..AttFact::plain(90_001, -1, 4, true, false) };
        assert!(domain_to_base(&dom(23), 23));
        assert!(!domain_to_base(&AttFact { base: 1700, ..AttFact::plain(90_002, -1, -1, false, false) }, 1700)); // by-reference base
        assert!(!domain_to_base(&dom(26), 23)); // a domain over oid relabelled to int4
        assert!(!domain_to_base(&AttFact::plain(26, -1, 4, true, false), 23)); // oidcol::int4
        assert!(!domain_to_base(&AttFact::plain(23, -1, 4, true, false), 23)); // no domain: no relabel to strip
    }
}
