//! The heap range source (kind `src.heap`, cards/M2.md L4; KEEP lowerv3 scan/range.rs, the ladder's `heap.rs` range site):
//! one window loop over a heap relation's range site. The claim's `[lo, hi)` seeks at the open and `advance` stages one
//! page per window (hosts `HeapRangeSource`: the visible tuples, hint bits, the look-ahead of heap.stream); a row's cells
//! are the site's attribute lanes at the row's index, each cell's value its element's address (the core's site-window
//! reader, frame/site.rs). Two forms, chosen at the make: `heap.pages` on (the default: `source_heap_pages`, the program
//! walks the pinned page) or off (`gravity_road_off=heap.pages`, or a site the page walk cannot read): the host-deformed
//! windows of this file, the A arm of the page road's twin and the general path.
//!
//! EvalPlanQual substitution is the host's (`window_substituted`: the next window is the substituted row alone). No
//! condition cache: no `granule_ident`, no `CondCache` verdict, `fills() = false`. A heap sink runs on the
//! leader in block order (PostgreSQL's row order for a scan's output, v2's M1-plan §7.3 rule); `ctid` is the window's
//! block and the row's offset lane. Registers kind `src.heap`. Owner M2-HEAP.
use gravity_abi::ctx::{lane, site_off};
use gravity_abi::site::site;
use gravity_decompose::ops::shared::scan::{same_type_zone_op, zone_op};
use gravity_decompose::{Am, Op, Source};
use gravity_expr::emit::ParamNeed;
use gravity_lower::Place;
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_program::protos::{Proto, ProtoKind, CMAX_AT, CMIN_AT, VERDICT_STREAM, XMAX_AT, XMIN_AT};
use gravity_program::{End, ExprLife, ExprOut, LaneDesc, LaneId as SiteLane, OutCol, Parallel, Pipeline, Reason as Why, Rep, SiteDesc, SiteKind, ZoneOp};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};
use types_core::catalog::OIDOID;
use types_nodes::primnodes::ParamKind;
use types_nodes::Node;

use crate::cell::{Cell, Nullable, TIDOID};
use crate::face::kinds::KindId;
use crate::face::source::{LaneForm, Locator, ReadForm, ReadPlan, SourceMech, WinConds};
use crate::frame::env::Env;
use crate::frame::frame::{SourceMaker, Win};
use crate::frame::px::{Px, DRAFT_SITE};
use crate::frame::row::{Carry, RowVals};
use crate::face::end::TABLEOID_AT;
use crate::frame::site::{Reader, CTID_AT};
use crate::admit::refusal::Reason;
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::SrcHeap, &SourceMaker(make))], ..crate::ops::NONE };

/// The heap site and its `ctid`: every by-value attribute lane read as the host's 8-byte datum word (v3 `site_of`); a scan
/// reading no column publishes no attribute lane (the host's count-only recipe, M2-inventory P5). The rowid lane (always last) is not the host's:
/// `ctid` is the window's block and the offset lane. Returns the site, its offset lane's index and whether the rows read `ctid`.
pub(crate) fn site_of(px: &Px<'_, '_>, d: &SiteDesc, node: i32) -> (SiteDesc, Option<usize>, bool) {
    // a scan reading no column stays so (V4R-37): the host's count-only walk touches no tuple header on an all-visible page
    let mut s = d.copy_in(px.arena);
    let rowid = s.lanes().last().is_some_and(|l| l.id == SiteLane::RowId);
    if rowid {
        s.lanes.pop();
    }
    for l in s.lanes.iter_mut() {
        if matches!(l.id, SiteLane::Attr(_)) && l.kind == lane::KIND_BYVAL {
            l.width = 8;
        }
    }
    s.filter_lanes.clear();
    add_sys_lanes(px, &mut s, node);
    let off = s.lanes().iter().position(|l| l.id == SiteLane::Offset);
    if rowid && off.is_none() {
        crate::refuse!(node, Reason::Bug { payload: "a heap site reading ctid without its offset lane" });
    }
    (s, off, rowid)
}

/// V4R-88: the system columns the scan reads (the decomposer's `scan_lanes`: xmin / cmin / xmax / cmax) as attribute
/// lanes right after the site's attributes (`XMIN_AT` ..: the host maps lane `k` to `out_attnos[k]`), by-value words the
/// host fills from each tuple's header as C `heap_getsysattr` does (cmin and cmax both the raw command id, never resolved
/// through the combo CID map, as PostgreSQL). A scan reading none adds nothing (paid only when referenced). A range site's
/// page window fills them (hosts heap_window.rs), an index, bitmap or tid site's fetched row (hosts heap_fetch.rs
/// `project`, D2665).
pub(crate) fn add_sys_lanes(px: &Px<'_, '_>, s: &mut SiteDesc, node: i32) {
    let Some(sl) = px.gx.graph.scan_lanes.iter().find(|l| l.node_id == node) else { return };
    let want: PgVec<'_, i16> = [("xmin", XMIN_AT), ("cmin", CMIN_AT), ("xmax", XMAX_AT), ("cmax", CMAX_AT)].into_iter().filter(|(n, _)| sl.names().any(|x| x == *n)).map(|(_, a)| a).collect_in(px.mcx());
    if want.is_empty() {
        return;
    }
    let (SiteKind::RangeSource { natts, .. } | SiteKind::IndexSource { natts, .. }) = s.kind else { crate::refuse!(node, Reason::Bug { payload: "system lanes on a heap site of another kind" }) };
    // the lanes after the attributes (the offset lane) move behind the system lanes, their driving bits with them
    let n = natts as usize;
    let mut tail = PgVec::new_in(px.mcx());
    while s.lanes.len() > n {
        let k = s.lanes.len() - 1;
        let l = s.lanes.pop().unwrap_or(LaneDesc::NONE);
        tail.push_in((l, s.driving_lanes.contains(k)));
        s.driving_lanes.remove(k);
    }
    for &a in want.iter() {
        s.lanes.push(LaneDesc::attr(a)); // alloc: Program
        s.out_attnos.push(a); // alloc: Program
    }
    let wide = natts + want.len() as u16;
    s.kind = match s.kind {
        SiteKind::IndexSource { nkeys, worker, leaves, ranges, .. } => SiteKind::IndexSource { natts: wide, nkeys, worker, leaves, ranges },
        SiteKind::RangeSource { nkeys, zkeys, .. } => SiteKind::RangeSource { natts: wide, nkeys, zkeys },
        k => k,
    };
    for (l, driving) in tail.into_iter().rev() {
        s.add_lane(l, driving);
    }
}

/// A heap pipeline's descriptor (source/pgrc.rs's rule): a sink runs parallel in claim order (end/sink_ordered.rs,
/// inventory row 19) unless it counts a LIMIT / OFFSET, which runs on the leader in block order; a LIMIT under a breaker
/// counts on one open (D281); a hosted call a pool thread cannot run keeps it on the leader.
pub(crate) fn pipeline_desc(px: &Px<'_, '_>, k: usize) -> Pipeline {
    let p = &px.gx.pipelines()[k];
    let d = p.desc;
    let limit = crate::frame::iface::holds_limit(&p.ops);
    if limit && crate::frame::iface::breaker_of(&d).is_none() {
        return Pipeline { consumes: d.consumes, ..Pipeline::serial(d.driving_site, End::Sink, Why::Sink) };
    }
    if limit {
        return Pipeline { parallel: Parallel::Ineligible { site: d.driving_site, reason: Why::Sink }, ..d };
    }
    match crate::hir::hosted_serial(px, k) {
        Some(reason) => Pipeline { parallel: Parallel::Ineligible { site: d.driving_site, reason }, ..d },
        None => d,
    }
}

/// heap.stream (H03a) when its road is on: the host applies PostgreSQL's read-stream rule (readplan_heap.rs).
pub(crate) fn read_plan() -> ReadPlan {
    if ReadForm::Stream.on() { ReadPlan::PLAIN.with(ReadForm::Stream) } else { ReadPlan::PLAIN }
}

/// heap.stream's switch (Decision 15, V4R-32): the site's `src.verdicts` enables word carries `VERDICT_STREAM` when the
/// road is on; the host applies PostgreSQL's rule itself (hosts v4/m2_heap.rs `ReadAhead`).
fn declare_stream(px: &mut Px<'_, '_>, k: usize) {
    let on = if ReadForm::Stream.on() { VERDICT_STREAM } else { 0 };
    px.declare_proto(Proto::new(ProtoKind::Verdicts, k as u16).site(DRAFT_SITE).slot(0).arg(on));
}

/// The ItemPointerData image `(bi_hi, bi_lo, ip_posid)` of `(block, off)` in a row-lived slot, as the `tid` cell's value
/// (the out row copies a by-reference cell). `block` is I32, `off` I16.
pub(crate) fn tid_image(b: &mut FunctionBuilder<'_>, block: Value, off: Value) -> Value {
    let slot = b.declare_stack_slot(8, 8);
    let img = b.stack_addr(slot);
    let sixteen = b.iconst(Type::I32, 16);
    let hi = b.int_bin(IntBinOp::Ushr, block, sixteen);
    let lo = b.int_bin(IntBinOp::Shl, block, sixteen);
    let bi = b.int_bin(IntBinOp::Or, hi, lo);
    b.store(img, 0, bi);
    b.store(img, 4, off);
    b.conv(ConvOp::PtrToInt, Type::I64, img)
}

/// D3681: a relation-set site's row's own relation, the member its walk is in: the window's position's relation
/// (`POS_REL`, hosts heap_source.rs `enter`), an `I32`.
pub(crate) fn set_rel(b: &mut FunctionBuilder<'_>, env: &Env, site: usize) -> Value {
    b.load(Type::I32, env.ctx, site_off(site) as i32 + site::POS_REL as i32)
}

/// D3681: whether pipeline `k`'s source, relation-set site `s`, reads `tableoid` (decompose `scan_lanes` of its first
/// scan): each row's is its own relation's (`push_tableoid`), never the first relation's constant.
pub(crate) fn set_tableoid(px: &Px<'_, '_>, k: usize, s: &SiteDesc) -> bool {
    let node = px.gx.pipelines()[k].node_id;
    s.set.len() > 1 && px.gx.graph.scan_lanes.iter().any(|l| l.node_id == node && l.names().any(|n| n == "tableoid"))
}

/// D3681: the `tableoid` cell (column `TABLEOID_AT`, step/expr.rs `compile_parts`) appended to `row`: `rel` (`set_rel`),
/// the row's own relation's OID.
pub(crate) fn push_tableoid(b: &mut FunctionBuilder<'_>, row: &mut RowVals, rel: Value) {
    let out = ExprOut { rep: Rep::of_storage(4, true), typid: OIDOID, life: ExprLife::Row };
    let n = row.row.cells.len() as u16;
    let at = TABLEOID_AT as usize - 1;
    if row.cols.len() <= at {
        row.cols.resize_in(at + 1, None);
    }
    row.cols[at] = Some(n);
    row.row.cells.push_in(Cell::of_expr(out, n, 0, Nullable::Never));
    row.vals.push_in((b.conv(ConvOp::Zext, Type::I64, rel), None));
}

/// The `tid` cell column `CTID_AT` names, appended to `row`.
pub(crate) fn push_ctid(row: &mut RowVals, img: Value) {
    let out = ExprOut { rep: OutCol::new("", TIDOID, -1, 6, false).rep, typid: TIDOID, life: ExprLife::Row };
    let n = row.row.cells.len() as u16;
    if let Some(c) = row.cols.get_mut(CTID_AT as usize - 1) {
        *c = Some(n);
    }
    row.row.cells.push_in(Cell::of_expr(out, n, 0, Nullable::Never));
    row.vals.push_in((img, None));
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let p = &px.gx.pipelines()[k];
    let node = p.node_id;
    let Source::Range { site, am: Am::Heap, relid, scanrelid } = &p.source else { crate::refuse!(node, Reason::Bug { payload: "a heap entry over another source" }) };
    // a wide top's fetch form (road topk.fetch) reads its winners' other columns by TID: the scan reads the keys alone
    // (D3585: never a relation set's, whose TIDs are each relation's own: `fetch::of`).
    let narrowed = if site.rows_via_am { // PGRCSERVE G4: the AM's rows keep every lane, never walk pages (am_rows.rs)
        fires::fire("rows.road");
        site.copy_in(px.arena)
    } else if site.set.is_empty() {
        crate::ops::scan::pgrc::pgrc_late::fetch_narrow(px, k, site.copy_in(px.arena))
    } else {
        site.copy_in(px.arena)
    };
    let (mut s, off, ctid) = site_of(px, &narrowed, node);
    // PSZP: a rows-road site's `col op $p` conjuncts, two trailing key cells each (`rows_param_keys`)
    let mut cells = Vec::new(); // alloc: Program
    if site.rows_via_am {
        let params = rows_params(px, k, node, *scanrelid as i32);
        if let SiteKind::RangeSource { nkeys, zkeys, .. } = &mut s.kind {
            for (desc, from) in params {
                cells.push((*nkeys, desc, from)); // alloc: Program
                (*nkeys, *zkeys) = (*nkeys + 2, *zkeys + 2);
            }
        }
    }
    declare_stream(px, k);
    if LaneForm::HeapPages.on() && !site.rows_via_am {
        if let Some(m) = crate::ops::scan::strategies::heap_pages::make(px, k, &s, *relid, off, ctid) {
            return m;
        }
    }
    let rd = Reader::new(px, &s, *relid, node, None, ctid.then_some(CTID_AT));
    let (set, tableoid) = (s.set.len() > 1, set_tableoid(px, k, &s));
    px.set_site(k, s);
    mcx::pg_box_dyn!(px.mcx(), HeapMech { rd, off, ctid, set, tableoid, cells } => dyn SourceMech + 'g)
}

/// PSZP: a rows-road site's pushable conjunct (`SiteDesc::rows_via_am`; hosts am_rows.rs pushes it into the table AM's
/// scan, exec sites_heap.rs `pushed_quals` a constant's): `col op other`, `col` a column of range table entry `varno` or
/// its integer widening, `op` a zone comparison of the native's one list (decompose `zone_op`) or bool's own btree one
/// (`same_type_zone_op`: the AM keys a bool column 0 / 1), turned when the column is on the right. The operator's function
/// is IMMUTABLE and LEAKPROOF (`effects::am_pushable`) and reads no collation: the AM drops a row before any other
/// conjunct sees it, so the comparison raises nothing on any row, and no text reaches the AM.
pub fn rows_zone_form<'p>(q: Node<'p>, varno: i32) -> Option<(i16, ZoneOp, Node<'p>)> {
    let o = q.as_op_expr().filter(|o| o.args.len() == 2 && o.inputcollid == 0)?;
    if !gravity_decompose::effects::am_pushable(o.opno, o.opfuncid) {
        return None;
    }
    let col = |n: Node<'_>| {
        let n = match n.as_func_expr() {
            Some(f) if f.args.len() == 1 && types_core::quiet::int_widen(f.funcid) => f.args.nth(0),
            _ => n,
        };
        n.as_var().filter(|v| v.varno == varno && v.varlevelsup == 0 && v.varattno > 0).map(|v| v.varattno)
    };
    let (a0, a1) = (o.args.nth(0), o.args.nth(1));
    let (attno, other, flipped) = match (col(a0), col(a1)) {
        (Some(a), None) => (a, a1, false),
        (None, Some(a)) => (a, a0, true),
        _ => return None,
    };
    let bool_op = || same_type_zone_op(o.opno).filter(|&(_, t)| t == types_core::BOOLOID).map(|(op, _)| op);
    let op = match (zone_op(o.opno).or_else(bool_op)?, flipped) {
        (ZoneOp::Lt, true) => ZoneOp::Gt,
        (ZoneOp::Le, true) => ZoneOp::Ge,
        (ZoneOp::Gt, true) => ZoneOp::Lt,
        (ZoneOp::Ge, true) => ZoneOp::Le,
        (op, _) => op,
    };
    Some((attno, op, other))
}

/// PSZP: a rows-road param cell's descriptor word (hosts am_rows.rs `rows_param_of` reads it): the column's attno, the
/// comparison (`ZoneOp` as `word_zone_arg` stores it), the comparand's typlen and type.
pub const fn rows_param_desc(attno: u16, op: ZoneOp, len: u8, typid: u32) -> i64 {
    (attno as u64 | (op as u64) << 16 | (len as u64) << 24 | (typid as u64) << 32) as i64
}

/// PSZP: where a rows-road param cell's value comes from at each (re-)open.
#[derive(Clone, Copy)]
enum RowsParam {
    /// A generic plan's extern parameter: its bind word (D289's).
    Word(u16),
    /// D3008: a parameter its producer writes before it (re-)opens this scan: a nest's NestLoopParam, a SubPlan's argument.
    Exec(i32),
    /// Any other (an InitPlan's stays lazy: PG runs it at the first row that reaches the qual): NULL, nothing pushed.
    Unset,
}

/// PSZP: the descriptor and value source of each `col op $p` conjunct ([`rows_zone_form`]) of pipeline `k`'s leading
/// filters that are scan `node`'s own, `$p` of a by-value type of 1, 2, 4 or 8 bytes.
fn rows_params(px: &mut Px<'_, '_>, k: usize, node: i32, varno: i32) -> Vec<(i64, RowsParam)> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let mut out = Vec::new(); // alloc: Program
    for j in 0..p.ops.len() {
        let Op::Filter(f) = p.ops[j] else { break };
        if p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(p.node_id, |n| n.node_id) != node {
            break;
        }
        let qs = crate::step::expr::quals(px, k, j);
        for (i, n) in f.as_list().map_or_else(|| vec![f], |l| l.iter().collect()).into_iter().enumerate() { // alloc: Program
            let Some((attno, op, prm)) = rows_zone_form(n, varno).and_then(|(a, op, c)| Some((a, op, c.as_param()?))) else { continue };
            let Some(tf) = crate::rep::storage::of(gx.catalog, prm.paramtype).ok().filter(|t| t.byval && [1, 2, 4, 8].contains(&t.len)) else { continue };
            let word = |q| px.placed(q).needs.iter().zip(px.placed(q).places.iter()).find_map(|(need, place)| match (need, place) {
                (ParamNeed::Extern { id, .. }, Place::Word(w)) if *id == prm.paramid => Some(*w),
                _ => None,
            });
            let exec = matches!(crate::rerun::params::producer(gx, prm.paramid), Some(KindId::NestParam | KindId::NestSubplan)) && crate::rerun::params::set_before_open(gx, prm.paramid);
            let from = match prm.paramkind {
                ParamKind::PARAM_EXTERN => qs.get(i).and_then(|&q| word(q)).map_or(RowsParam::Unset, RowsParam::Word),
                ParamKind::PARAM_EXEC if exec => RowsParam::Exec(prm.paramid),
                _ => RowsParam::Unset,
            };
            out.push((rows_param_desc(attno as u16, op, tf.len as u8, prm.paramtype), from)); // alloc: Program
        }
    }
    out
}

/// PSZP: each rows-road param cell pair at the open, before the seek reads them: the descriptor, then the value with its
/// NULL byte (an unset or NULL parameter's is NULL: the host pushes nothing for it).
fn rows_param_keys(b: &mut FunctionBuilder<'_>, env: &mut Env, k: usize, cells: &[(u16, i64, RowsParam)]) {
    let s = site_off(k) as i32;
    let keys = b.load(Type::Ptr, env.ctx, s + site::KEYS as i32);
    let keys_null = b.load(Type::Ptr, env.ctx, s + site::KEYS_NULL as i32);
    let (no, yes) = (b.iconst(Type::I8, 0), b.iconst(Type::I8, 1));
    for &(cell, desc, from) in cells {
        let at = cell as i32;
        let d = b.iconst(Type::I64, desc);
        b.store(keys, at * 8, d);
        b.store(keys_null, at, no);
        let (v, isnull) = match from {
            RowsParam::Word(w) => (b.load(Type::I64, env.params, w as i32 * 8), b.load(Type::I8, env.params_null, w as i32)),
            RowsParam::Exec(id) => {
                let (v, null) = crate::rerun::params::load(b, env, env.node, id);
                let set = crate::rerun::params::is_set(b, env, env.node, id);
                let t = b.bconst(true);
                let unset = b.int_bin(IntBinOp::Xor, set, t);
                let none = b.int_bin(IntBinOp::Or, null, unset);
                (v, b.conv(ConvOp::Zext, Type::I8, none))
            }
            RowsParam::Unset => (env.zero, yes),
        };
        b.store(keys, (at + 1) * 8, v);
        b.store(keys_null, at + 1, isnull);
    }
}

/// The bitmap heap scan's morsel form (bitmap.rs `make_morsel`) as this file's host-deformed heap range walk over
/// site `s` (the decomposer's range site, its `param_slots` the bitmap keys' words exec's claims read): the claim's
/// `[lo, hi)` seeks the host's frozen bitmap entries (hosts bitmap_morsel.rs), never the raw page road.
pub(crate) fn make_over<'g>(px: &mut Px<'g, '_>, k: usize, s: SiteDesc, relid: u32, node: i32) -> PgBox<'g, dyn SourceMech + 'g> {
    let (s, off, ctid) = site_of(px, &s, node);
    declare_stream(px, k);
    let rd = Reader::new(px, &s, relid, node, None, ctid.then_some(CTID_AT));
    px.set_site(k, s);
    mcx::pg_box_dyn!(px.mcx(), HeapMech { rd, off, ctid, set: false, tableoid: false, cells: Vec::new() } => dyn SourceMech + 'g)
}

/// The host-deformed windows over `frame::site::Reader`; `off` = the offset lane (the locator's and `ctid`'s offset).
/// D3681: `set`, a relation-set site (each row's relation its window's: `set_rel`); `tableoid`, its rows' `tableoid` cell.
struct HeapMech<'g> {
    rd: Reader<'g>,
    off: Option<usize>,
    ctid: bool,
    set: bool,
    tableoid: bool,
    /// PSZP: a rows-road site's param cells (`rows_param_keys`): the descriptor's cell, the descriptor, the value's source.
    cells: Vec<(u16, i64, RowsParam)>,
}

impl HeapMech<'_> {
    /// Row `i`'s `(block, offset)`: the window's position block and the offset lane's element.
    fn tid(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, i: Value) -> Option<(Value, Value)> {
        let off = self.off?;
        let s = site_off(self.rd.k(env)) as i32;
        let block = b.load(Type::I32, env.ctx, s + site::POS_BLOCK as i32);
        let offs = b.load(Type::Ptr, self.rd.vals().lane_arr, (off * lane::SIZE + lane::PTR) as i32);
        let at = b.ptr_add(offs, i, 2, 0);
        Some((block, b.load(Type::I16, at, 0)))
    }
}

impl SourceMech for HeapMech<'_> {
    fn declare(&mut self, pb: &mut ProgramBuilder) {
        self.rd.declare(pb);
    }

    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline {
        pipeline_desc(px, k)
    }

    fn read_plan(&self) -> ReadPlan {
        read_plan()
    }

    fn fills(&self) -> bool {
        false
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        if !self.cells.is_empty() {
            let k = self.rd.k(env);
            rows_param_keys(b, env, k, &self.cells);
        }
        self.rd.open(b, env)
    }

    /// The window and its facts: no nulls in the nullable lanes, no selection.
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let (cursor, end) = self.rd.window(b, env, l_eof);
        let w = self.rd.vals();
        Win { cursor, end, facts: WinConds { nonulls: self.rd.nonulls(b), nosel: Some(w.sel_null), ..WinConds::default() } }
    }

    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let mut row = self.rd.row(b, env, j, carry, next);
        if self.ctid {
            let i = self.rd.row_index(b, env, j);
            if let Some((block, off)) = self.tid(b, env, i) {
                let img = tid_image(b, block, off);
                push_ctid(&mut row, img);
            }
        }
        if self.tableoid {
            let rel = set_rel(b, env, self.rd.k(env));
            push_tableoid(b, &mut row, rel);
        }
        row
    }

    /// H28: the row's `(block, offset)`; the pinned buffer is not published (`pinned = None`). D3681: a relation set's
    /// row's own relation (`set_rel`).
    fn locator(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) -> Option<Locator> {
        let i = self.rd.row_index(b, env, row.at);
        let (block, off) = self.tid(b, env, i)?;
        let rel = self.set.then(|| set_rel(b, env, self.rd.k(env)));
        Some(Locator { block: b.conv(ConvOp::Zext, Type::I64, block), offset: b.conv(ConvOp::Zext, Type::I64, off), pinned: None, rel })
    }
}

// ---------------------------------------------------------------- the heap tuple format (heap_pages.rs walks it in IR)

// PostgreSQL's page and tuple layout (bufpage.h, itemid.h, htup_details.h): the page header, a line pointer's `lp_off`
// (15 bits), the tuple header's `t_infomask` / `t_hoff`, its null bitmap and HEAP_HASNULL.
pub(crate) const PAGE_HEADER: i32 = 24;
pub(crate) const LP_OFF_MASK: i64 = 0x7fff;
pub(crate) const T_INFOMASK: i32 = 20;
pub(crate) const T_HOFF: i32 = 22;
pub(crate) const T_BITS: i32 = 23;
pub(crate) const HEAP_HASNULL: i64 = 1;

/// A running offset into the tuple's data: a constant until the first varlena, a value after it.
#[derive(Clone, Copy)]
pub(crate) enum Off {
    K(i64),
    V(Value),
}

pub(crate) fn as_value(b: &mut FunctionBuilder<'_>, off: Off) -> Value {
    match off {
        Off::K(k) => b.iconst(Type::I64, k),
        Off::V(v) => v,
    }
}

pub(crate) fn align_off(b: &mut FunctionBuilder<'_>, off: Off, len: i64) -> Off {
    match off {
        Off::K(k) => Off::K((k + len - 1) & !(len - 1)),
        Off::V(v) if len > 1 => {
            let pad = b.iconst(Type::I64, len - 1);
            let mask = b.iconst(Type::I64, !(len - 1));
            let up = b.int_bin(IntBinOp::Add, v, pad);
            Off::V(b.int_bin(IntBinOp::And, up, mask))
        }
        v => v,
    }
}

pub(crate) fn add_off(b: &mut FunctionBuilder<'_>, off: Off, len: i64) -> Off {
    match off {
        Off::K(k) => Off::K(k + len),
        Off::V(v) => {
            let l = b.iconst(Type::I64, len);
            Off::V(b.int_bin(IntBinOp::Add, v, l))
        }
    }
}

/// The by-value column of `w` bytes at `p` as the host's datum word (`fetch_att`: 8 as is, else sign-extended).
pub(crate) fn load_word(b: &mut FunctionBuilder<'_>, p: Value, w: u8) -> Value {
    let ty = match w {
        8 => return b.load(Type::I64, p, 0),
        4 => Type::I32,
        2 => Type::I16,
        _ => Type::I8,
    };
    let v = b.load(ty, p, 0);
    b.conv(ConvOp::Sext, Type::I64, v)
}

/// The offset past a varlena at `data + off` (`att_align_pointer` + `att_addlength_pointer` for `attlen` -1; v2
/// `step_varlena`): a zero first byte is padding, so the offset aligns to `align` and the header is read there; then
/// `VARSIZE_ANY` by the header's form (1-byte `len << 1 | 1`, external `0x01` + tag: 18 bytes on disk, 10 in memory, 4-byte
/// `len << 2`). `present`: `(present, tup)` reads the tuple header (a valid address) for an absent attribute. Returns
/// `(offset past, header address)`.
pub(crate) fn step_varlena(b: &mut FunctionBuilder<'_>, data: Value, off: Off, align: u8, present: Option<(Value, Value)>) -> (Value, Value) {
    let zero = b.iconst(Type::I64, 0);
    let one = b.iconst(Type::I64, 1);
    let probe = |b: &mut FunctionBuilder<'_>, off: Value| -> Value {
        let p = b.ptr_add(data, off, 1, 0);
        match present {
            Some((pr, tup)) => b.select(pr, p, tup),
            None => p,
        }
    };
    let o0 = as_value(b, off);
    let p0 = probe(b, o0);
    let b0 = b.load_zext(Type::I8, p0, 0);
    let pad = b.icmp(IntCond::Eq, b0, zero);
    let oa = align_off(b, off, align as i64);
    let oa = as_value(b, oa);
    let o1 = b.select(pad, oa, o0);
    let p1 = probe(b, o1);
    let hb = b.load_zext(Type::I8, p1, 0);
    let lo = b.int_bin(IntBinOp::And, hb, one);
    let short = b.icmp(IntCond::Ne, lo, zero);
    let (l_1b, l_4b, l_join) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64]));
    b.cond_br(short, (l_1b, &[]), (l_4b, &[]));
    b.switch_to(l_1b);
    let external = b.icmp(IntCond::Eq, hb, one);
    let tag = b.load_zext(Type::I8, p1, 1);
    let eighteen = b.iconst(Type::I64, 18);
    let ten = b.iconst(Type::I64, 10);
    let ondisk = b.icmp(IntCond::Eq, tag, eighteen);
    let len_e = b.select(ondisk, eighteen, ten);
    let len_1 = b.int_bin(IntBinOp::Ushr, hb, one);
    let len = b.select(external, len_e, len_1);
    b.br(l_join, &[len]);
    b.switch_to(l_4b);
    let h = b.load_zext(Type::I32, p1, 0);
    let two = b.iconst(Type::I64, 2);
    let len4 = b.int_bin(IntBinOp::Ushr, h, two);
    b.br(l_join, &[len4]);
    b.switch_to(l_join);
    let len = b.block_params(l_join)[0];
    (b.int_bin(IntBinOp::Add, o1, len), p1)
}
