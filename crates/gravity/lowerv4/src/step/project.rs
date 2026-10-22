//! Projection cells, kind `step.project` (KEEP lowerv3 scan/project.rs): every target entry evaluated over the row and
//! passed on as a cell (column `resno` = entry k + 1), no stage and no store; the sink or the next step reads the cells
//! through `datum`. Each cell carries its FACTS `ExprOut` (Rep and life), else the type's storage row. A bare column entry
//! passes the row's cell on as it is (a code stays a code, rendered only where read); a text key over one code column read
//! as a key alone is its per-entry Fp128 (step/percode_key.rs), its representative column carried hidden when dropped. A projection over a joined row
//! collapses it: the steps after it read only the new cells (`env.inner` cleared for them). Budget 250 lines.
//! Owner M1-STEP.
use gravity_decompose::ops::shared::srf::SrfPlace;
use gravity_decompose::Op;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type};
use gravity_abi::ctx::lane::KIND_CODES;
use gravity_program::{ExprLife, ExprOut, LaneId, OutCol, Rep};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};
use types_core::catalog::INT4OID;
use types_nodes::primnodes::{INNER_VAR, OUTER_VAR};
use types_nodes::{Node, NodeTag};

use crate::cell::{Cell, Nullable, Row};
use crate::face::kinds::KindId;
use crate::face::step::StepMech;
use crate::frame::env::{AggRow, Env};
use crate::frame::frame::{Next, StepMaker};
use crate::frame::px::{ExId, Px};
use crate::frame::row::{Carry, RowVals};
use crate::graph::Gx;
use crate::admit::refusal::Reason;

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::StepProject, &StepMaker(make))], ..crate::ops::NONE };

pub(crate) struct Entry {
    /// `None`: a bare column entry over a column the row does not carry, which no later step reads (decompose gives a
    /// lane to every column the pipeline reads; a physical target list forwards the others): passed on as no cell.
    pub ex: Option<ExId>,
    pub out: ExprOut,
    /// A bare column entry: the input column whose cell passes on.
    pub fwd: Option<i16>,
    /// A finals.cse member (fold/finals_cse.rs): its index in `Project::fams`' values.
    pub fam: Option<u16>,
    /// A ProjectSet's set-returning entry: its function's index in the `SrfDesc` (`Env::srfs`' cell it reads).
    pub srf: Option<u16>,
    /// D2032: an entry the Srf step evaluated itself (srf/project_set.rs `early_entries`): its word cell of `Env::srfs`.
    pub early: Option<u16>,
    /// join.payload_fp: a bare INNER_VAR entry over a code payload that carries its Fp128 words (`payload_fp::forwards`):
    /// the record's cell passes on as it is where the probe linked its words, rendered only where read. D3324 (b) (`.1`):
    /// a dependent group key's column (ops/agg/strategies/dependent.rs `forwards`), passed on always, its code keyed by
    /// its render (no table).
    pub inner: Option<(i16, bool)>,
}

pub(crate) struct Project<'g> {
    pub entries: PgVec<'g, Entry>,
    pub node: i32,
    /// Every entry is a Var or a Const (through RelabelType): nothing computed that could raise (`keeps_rows`).
    pub plain: bool,
    /// finals.cse's families: the members' records, emitted once per row before the entries (fold/finals_cse.rs).
    pub fams: Option<crate::ops::agg::trans::strategies::finals_cse::Families<'g>>,
}

/// The column of a bare scan or outer column entry (a Var through any RelabelType).
pub(crate) fn bare_var(e: Node<'_>) -> Option<i16> {
    crate::stack_check();
    match e.node_tag() {
        // M4-RET R4a: a RETURNING old / new Var reads its image (write.returning's inner row), never the written row's column
        NodeTag::T_Var => e.as_var().filter(|v| v.varattno > 0 && v.varno != INNER_VAR && v.varreturningtype == types_nodes::primnodes::VarReturningType::VAR_RETURNING_DEFAULT).map(|v| v.varattno),
        NodeTag::T_RelabelType => gravity_decompose::expr::through_relabel(e).and_then(bare_var),
        _ => None,
    }
}

/// D669: an `OUTER_VAR` entry (through any RelabelType).
fn outer_var(e: Node<'_>) -> bool {
    crate::stack_check();
    match e.node_tag() {
        NodeTag::T_Var => e.as_var().is_some_and(|v| v.varno == OUTER_VAR),
        NodeTag::T_RelabelType => gravity_decompose::expr::through_relabel(e).is_some_and(outer_var),
        _ => false,
    }
}

/// The code columns of the row step `j` of pipeline `k` reads: the site's code lanes `(attno, lane, width)`, followed
/// through the steps before `j` (a projection keeps a bare column's code, as its entry's resno).
pub(crate) fn code_cols<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgVec<'g, (i16, u8, u8)> {
    let mcx = px.mcx();
    let Some(s) = px.site_mut(k) else { return PgVec::new_in(mcx) };
    let mut cols: PgVec<'g, (i16, u8, u8)> = s.lanes().iter().enumerate().filter_map(|(i, l)| match l.id {
        // D1749: a code lane is a u8 site index (Plan.cols, Tables.lanes), a type bound: past it the column is read as its datum
        LaneId::Attr(a) if l.kind == KIND_CODES => u8::try_from(i).ok().map(|i| (a, i, l.width)),
        // M6 R3: a shredded path's code lane (jsonb.codes) is the row's column SHRED_AT + n (frame/site.rs)
        LaneId::JsonbPath(n) if l.kind == KIND_CODES => u8::try_from(i).ok().map(|i| (crate::frame::site::SHRED_AT + n as i16, i, l.width)),
        _ => None,
    }).collect_in(mcx);
    for op in px.gx.pipelines()[k].ops[..j].iter() {
        match op {
            // M8 B2: a SubPlan step writes frame words, never the row
            Op::Filter(_) | Op::Gate(_) | Op::SubPlan(_) => {}
            Op::Project(list) => {
                cols = list.iter().enumerate().filter_map(|(r, e)| {
                    let a = bare_var(e.as_target_entry().map_or(e, |t| t.expr))?;
                    cols.iter().find(|c| c.0 == a).map(|c| (r as i16 + 1, c.1, c.2))
                }).collect_in(mcx);
            }
            _ => return PgVec::new_in(mcx),
        }
    }
    cols
}

/// The columns the row step `j` of pipeline `k` reads carries, `None` when not known (no site and no CteScan over a narrowed
/// store, or a step other than a filter, gate, SubPlan or projection before `j`): the site's attribute lanes (a CteScan's:
/// its store's columns), followed through the projections before `j` (a bare column entry over an absent column is absent;
/// any other entry is carried).
fn carried<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> Option<PgVec<'g, i16>> {
    let mcx = px.mcx();
    let lanes: Option<PgVec<'g, i16>> = px.site_mut(k).map(|s| s.lanes().iter().filter_map(|l| if let LaneId::Attr(a) = l.id { Some(a) } else { None }).collect_in(mcx));
    // D2633: a CteScan's row carries the columns its CTE's store keeps (ops/ctescan/lower.rs `carried`)
    let mut cols = match lanes {
        Some(cols) => crate::ops::scan::index_only::row_cols(px.gx, k, cols),
        None => crate::ops::ctescan::lower::carried(px.gx, k)?,
    };
    for op in px.gx.pipelines()[k].ops[..j].iter() {
        match op {
            Op::Filter(_) | Op::Gate(_) | Op::SubPlan(_) => {}
            Op::Project(list) => {
                let kept = |e: Node<'_>| bare_var(e.as_target_entry().map_or(e, |t| t.expr)).is_none_or(|a| cols.contains(&a));
                cols = list.iter().enumerate().filter(|&(_, e)| kept(e)).map(|(r, _)| r as i16 + 1).collect_in(mcx);
            }
            _ => return None,
        }
    }
    Some(cols)
}

/// Function `i`'s cell of the Srf step's row (`Env::srfs`) as the one-cell aggregate row its entry's program reads.
fn srf_row<'e>(env: &Env<'e, '_>, node: i32, i: u16) -> AggRow<'e> {
    let Some((rec, cell)) = env.srfs.as_ref().and_then(|r| Some((r.rec, *r.cells.get(i as usize)?))) else {
        crate::refuse!(node, Reason::Bug { payload: "a set-returning entry without its Srf step's cell" })
    };
    AggRow { rec, cells: mcx::pg_vec![in env.mcx; cell] }
}

/// `e` is a Var or a Const, through any RelabelType: evaluating it cannot raise.
fn plain(e: Node<'_>) -> bool {
    crate::stack_check();
    match e.node_tag() {
        NodeTag::T_Var | NodeTag::T_Const => true,
        NodeTag::T_RelabelType => e.as_relabel_type().is_some_and(|r| plain(r.arg)),
        _ => false,
    }
}

/// An entry's cell facts: FACTS' `ExprOut`, else the type's storage row living for the row.
pub(crate) fn out_of<'p>(gx: &Gx<'_, 'p>, e: Node<'p>, node: i32) -> ExprOut {
    if let Some(o) = gx.out_of(e) {
        return o;
    }
    let typ = gravity_decompose::catalog::expr_type::type_of(e).unwrap_or_else(|| crate::rep::expr_type::result_type(gx, e, node));
    let tf = crate::end::output::facts(gx, typ, node);
    ExprOut { rep: OutCol::new("", typ, -1, tf.typlen, tf.byval).rep, typid: typ, life: ExprLife::Row }
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let mcx = px.mcx();
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(p.node_id, |n| n.node_id);
    let Op::Project(list) = p.ops[j] else { crate::refuse!(node, Reason::Bug { payload: "a project entry over another op" }) };
    let mut entries: PgVec<'g, Entry> = mcx::reserved_in(list.len(), mcx);
    let mut all_plain = true;
    let have = carried(px, k, j);
    let fams = crate::ops::agg::trans::strategies::finals_cse::families(px, k, list);
    // M5 L0-c (1): the Srf step right before a ProjectSet's projection, whose functions' cells its set-returning entries read
    let srf = j.checked_sub(1).and_then(|i| match &p.ops[i] {
        Op::Srf(d) if d.place == SrfPlace::TargetList => Some(d),
        _ => None,
    });
    // D669: a RETURNING list's OUTER_VAR is the child's column write.returning lays in the inner row (gravity_expr
    // `Returning.outer`), never the written row's column at that resno
    let ret = j.checked_sub(1).is_some_and(|i| matches!(&p.ops[i], Op::Write(w) if w.returning.is_some()));
    let early = srf.map_or_else(|| PgVec::new_in(mcx), |_| crate::ops::srf::project_set::early_entries(px, k, j - 1));
    // errorhold (D1344): a volatile order column a top's drain reads back from its slot is not computed again here (no step
    // reads it: the sort hands the row on, the top keeps records; order_slot.rs `skipped`)
    let back = crate::ops::sort::topn::order_slot::skipped(gx, k, j);
    let carried = crate::ops::hashjoin::strategies::payload_fp::forwards(px.gx, k, j);
    for (r, n) in list.iter().enumerate() {
        let Some(te) = n.as_target_entry() else { crate::refuse!(node, Reason::Bug { payload: "a projection entry that is not a TargetEntry" }) };
        // D531: an integral extract the group keys by its integer word (keyed/emit_group.rs `extract_word_key`) is that word
        let word = j + 1 == p.ops.len() && gx.group_at(k).is_some_and(|g| g.keys.iter().any(|kd| crate::table::emit_group::extract_word_key(gx, k, g, kd) == Some(r as i16 + 1)));
        // D3080: an integer's text the group keys by the integer (keyed/emit_group.rs `int_text_word_key`) is that integer
        let int_text = (j + 1 == p.ops.len() && !word && gx.group_at(k).is_some_and(|g| g.keys.iter().any(|kd| crate::table::emit_group::int_text_word_key(gx, k, g, kd) == Some(r as i16 + 1))))
            .then(|| gravity_decompose::ops::shared::agg::int_text_arg(crate::table::emit_group::bare(te.expr))).flatten();
        let out = if word { ExprOut { rep: Rep::Word { bytes: 8 }, typid: INT4OID, life: ExprLife::Row } } else { out_of(gx, int_text.unwrap_or(te.expr), node) };
        all_plain &= plain(te.expr);
        let fwd = bare_var(te.expr).filter(|_| !((ret || crate::ops::modifytable::returning::ret_write(p, j).is_some()) && outer_var(te.expr))); // D1112: or past the list's SubPlan steps
        let absent = fwd.is_some_and(|a| have.as_ref().is_some_and(|h| !h.contains(&a))) || back.contains(&te.resno);
        let key = fwd.is_none().then(|| crate::dict::strategies::percode_key::key_only(px, k, j, r as i16 + 1)).flatten();
        let fam = fams.as_ref().and_then(|f| f.of[r]);
        let early = srf.and_then(|d| early.iter().position(|&e| e == te.resno).map(|i| (d.functions.len() + i) as u16));
        let ex = (!absent && fam.is_none() && early.is_none()).then(|| match key {
            Some(coll) => crate::dict::strategies::percode_key::compile(px, k, j, te.expr, coll, node),
            None if word => crate::step::expr::compile_word(px, (k, j), te.expr, node),
            None if int_text.is_some() => crate::step::expr::compile_at(px, k, j, int_text.unwrap_or(te.expr), None, node),
            None => crate::step::expr::compile_at(px, k, j, te.expr, None, node),
        });
        let srf = srf.and_then(|d| d.functions.iter().position(|f| f.resno == te.resno)).map(|i| i as u16);
        let inner = crate::table::emit_group::bare(te.expr).as_var().filter(|v| v.varno == INNER_VAR && v.varlevelsup == 0).map(|v| v.varattno);
        let inner = inner.filter(|&a| carried.has(a)).map(|a| (a, false)).or_else(|| inner.filter(|&a| crate::ops::agg::strategies::dependent::forwards(px, k, j, a)).map(|a| (a, true)));
        entries.push_in(Entry { ex, out, fwd, fam, srf, early, inner });
    }
    // a derived group key's representative column the entries drop, carried hidden past them (resno `list.len() + 1 + i`):
    // the last op before the group (a pipeline's end, or D1071's streamed group marker), so no sink reads it
    // (keyed_emit_group `hidden_reps`, empty for any other projection)
    let relid = p.source.relation().map_or(0, |r| r.1);
    for a in crate::table::emit_group::hidden_reps(gx, k, j) {
        let Some(f) = gx.facts.att(relid, a).or_else(|| crate::ops::agg::emit_group_nocode::att(a)) else { crate::refuse!(node, Reason::Bug { payload: "a hidden representative without its attribute" }) };
        entries.push_in(Entry { ex: None, out: ExprOut { rep: Rep::of_storage(f.typlen, f.byval), typid: f.typid, life: ExprLife::Row }, fwd: Some(a), fam: None, srf: None, early: None, inner: None });
    }
    // RD-key.codeif: a constant-arm group key's image, hidden after the representatives (keyed_emit_group `image_keys`),
    // read off the key's own entry
    for (kd, e) in crate::table::emit_group::image_keys(gx, k, j) {
        let Some(d) = list.iter().position(|n| n.as_target_entry().is_some_and(|te| te.resno == kd.resno)) else { crate::refuse!(node, Reason::Bug { payload: "a constant-arm key image without its key's entry" }) };
        let ex = crate::dict::strategies::key_consts::compile(px, k, e, d as u16, node);
        entries.push_in(Entry { ex: Some(ex), out: out_of(gx, e, node), fwd: None, fam: None, srf: None, early: None, inner: None });
    }
    // P1c: the path lanes the group reads over a forwarded jsonb column, hidden after the key images (strategies/jsonb_fwd.rs)
    for (a, typid) in crate::ops::scan::pgrc::strategies::jsonb_fwd::lane_cols(px, k, j) {
        let rep = if typid == types_core::TEXTOID { Rep::VarlenaPtr } else { Rep::Word { bytes: 8 } };
        entries.push_in(Entry { ex: None, out: ExprOut { rep, typid, life: ExprLife::Row }, fwd: Some(a), fam: None, srf: None, early: None, inner: None });
    }
    // D3038: the octet lanes the group reads over the feed's bare columns, hidden after jsonb_fwd's (strategies/octet_lane.rs)
    for (a, typid) in crate::ops::scan::pgrc::strategies::octet_lane::lane_cols(px, k, j) {
        entries.push_in(Entry { ex: None, out: ExprOut { rep: Rep::Word { bytes: 8 }, typid, life: ExprLife::Row }, fwd: Some(a), fam: None, srf: None, early: None, inner: None });
    }
    mcx::pg_box_dyn!(px.mcx(), Project { entries, node, plain: all_plain, fams } => dyn StepMech + 'g)
}

impl<'g> StepMech for Project<'g> {
    fn keeps_rows(&self) -> bool {
        self.plain
    }

    /// D3168: a plain projection (bare columns and constants, no family, no set-returning or early entry) passes column `a`
    /// (entry `a - 1`) on from its bare column.
    fn forwards(&self, a: i16) -> Option<Option<i16>> {
        let bare = self.plain && self.fams.is_none() && self.entries.iter().all(|e| e.srf.is_none() && e.early.is_none());
        bare.then(|| usize::try_from(a - 1).ok().and_then(|i| self.entries.get(i)).and_then(|e| e.fwd))
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        // D4005: the entries' lines run once per row the node emits, not per row the statement reads (tier 0's break-even)
        let from = b.pushed();
        let t = b.bconst(true);
        let mcx = env.mcx;
        let mut cells = mcx::reserved_in(self.entries.len(), mcx);
        let mut vals = mcx::reserved_in(self.entries.len(), mcx);
        let mut cols = mcx::reserved_in(self.entries.len(), mcx);
        let fv = self.fams.as_ref().map_or_else(|| PgVec::new_in(mcx), |f| crate::ops::agg::trans::strategies::finals_cse::emit(b, env, row, f));
        for e in self.entries.iter() {
            let at = cells.len() as u16;
            if let Some(cr) = e.fwd.and_then(|a| row.col(a)).or_else(|| e.inner.and_then(|(a, d)| row.inner_col(env, a).filter(|c| d || env.linked(c.val)))) {
                let rep = match (e.inner, cr.cell.rep) {
                    (Some((_, true)), Rep::Code { dict }) => Rep::Code { dict: gravity_program::DictRef { fp_table: gravity_program::DictRef::NO_TABLE, ..dict } },
                    (_, r) => r,
                };
                cells.push_in(Cell { typid: e.out.typid, rep, ..*cr.cell });
                vals.push_in((cr.val, cr.null));
                cols.push_in(Some(at));
                continue;
            }
            if let Some(m) = e.fam {
                cells.push_in(Cell::of_expr(e.out, at, at, Nullable::Maybe));
                vals.push_in((fv[m as usize].0, Some(fv[m as usize].1)));
                cols.push_in(Some(at));
                continue;
            }
            if let Some(c) = e.early {
                let Some(rec) = env.srfs.as_ref().map(|r| r.rec) else { crate::refuse!(self.node, Reason::Bug { payload: "an early entry without its Srf step's row" }) };
                let (v, w) = (b.load(Type::I64, rec, 16 * c as i32), b.load(Type::I64, rec, 16 * c as i32 + 8));
                let null = b.icmp(IntCond::Eq, w, env.zero);
                cells.push_in(Cell::of_expr(e.out, at, at, Nullable::Maybe));
                vals.push_in((v, Some(null)));
                cols.push_in(Some(at));
                continue;
            }
            let Some(ex) = e.ex else {
                cols.push_in(None);
                continue;
            };
            if let Some(d) = crate::dict::strategies::key_consts::datum_of(env, ex) {
                let Some(&(v, Some(null))) = cols.get(d as usize).copied().flatten().and_then(|c| vals.get(c as usize)) else { crate::refuse!(self.node, Reason::Bug { payload: "a constant-arm key image before its key's datum" }) };
                let (cell, v, null) = crate::dict::strategies::key_consts::cell(b, env, self.node, row, ex, v, null);
                cells.push_in(cell);
                vals.push_in((v, Some(null)));
                cols.push_in(Some(at));
                continue;
            }
            if let Some((cell, v, null)) = crate::dict::strategies::percode_key::cell(b, env, self.node, row, ex) {
                cells.push_in(cell);
                vals.push_in((v, Some(null)));
                cols.push_in(Some(at));
                continue;
            }
            // M5 L0-c (1): each entry is its own program, so every set-returning call numbers its cell 0 (`compile_srf`):
            // the entry reads its own function's cell of the Srf step's row, never the first function's
            let outer = e.srf.map(|i| {
                let one = srf_row(env, self.node, i);
                env.aggs.replace(one)
            });
            let (v, valid) = crate::step::expr::emit(b, env, self.node, row, ex);
            if let Some(a) = outer {
                env.aggs = a;
            }
            let null = b.int_bin(IntBinOp::Xor, valid, t);
            cells.push_in(Cell::of_expr(e.out, at, at, Nullable::Maybe));
            vals.push_in((v, Some(null)));
            cols.push_in(Some(at));
        }
        let lines = b.pushed() - from;
        b.tag_lines(self.node, lines);
        let out = RowVals { row: Row { cells }, vals, cols, carry: row.carry.copy_in(env.mcx), next: row.next, at: row.at };
        // the memo keys cells by `At`: the new row's `Value(k)` must not answer from the input row's `Value(k)`
        env.memo.clear();
        let inner = env.inner.take();
        let c = next(b, env, &out);
        env.inner = inner;
        env.memo.clear();
        c
    }
}
