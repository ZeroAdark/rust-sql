//! The cell (v3 design §3.1, kept): a lowered value with its representation and life, decided once where it is made. v4
//! changes: a source lane is named by `LaneId = (attno, path_ord)` (the jsonb path lanes, H02 g), and a source's row
//! locator (H28: TID or granule row) is a cell of its own.
use gravity_decompose::ops::shared::agg::KeyDecl;
use gravity_program::{ExprLife, ExprOut, Home, LaneDesc, LaneLife, LifeId, NumTypmod, Rep, Until};
use mcx::CollectIn;
use mcx::{Mcx, PgVec};

use crate::graph::Gx;

/// A source lane: `attno` = the lane's position in its site (the column), `path_ord` = 0 for the column itself, k > 0 for the
/// column's k-th declared jsonb path lane (M6, `jsonb.lanes`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LaneId {
    pub attno: u16,
    pub path_ord: u16,
}

impl LaneId {
    pub const fn col(attno: u16) -> LaneId {
        LaneId { attno, path_ord: 0 }
    }

    pub const fn path(attno: u16, path_ord: u16) -> LaneId {
        LaneId { attno, path_ord }
    }

    pub const fn is_path(self) -> bool {
        self.path_ord != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CellHome {
    Lane(Home),
    SiteCx(u16),
    RowCx,
    FrameSlot,
    Record(LifeId),
    Arg(u16),
    /// An image-only key cell (REP 09-24, a key derived from a text expression): a `Rep::Code` whose code indexes a per-entry Fp128 table
    /// (step/percode_key.rs), not a dictionary code that a render, a rank or a kept word may read. It lives in the frame
    /// slot as `FrameSlot` does; only its hash image (the Fp128 load) is valid: rep/code datum / retain / block / word and
    /// order's rank / image refuse it (`Reason::Bug`), hash's same-dictionary scope and keyimg's code front do not take it.
    ImageOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CellLife {
    pub until: Until,
    pub home: CellHome,
}

impl CellLife {
    pub const fn of_lane(l: LaneLife) -> CellLife {
        CellLife { until: l.until, home: CellHome::Lane(l.home) }
    }

    /// A computed value's life: a lane's as the lane's; `Row` in the row context; `Call` until the builtin's next call.
    pub const fn of_expr(l: ExprLife, site: u16) -> CellLife {
        match l {
            ExprLife::Lane(l) => CellLife::of_lane(l),
            ExprLife::Row => CellLife { until: Until::Row, home: CellHome::RowCx },
            ExprLife::Call { .. } => CellLife { until: Until::Call, home: CellHome::SiteCx(site) },
        }
    }

    /// Whether bytes of this life are still valid at `until` (a target that names no record's life).
    pub fn reaches(self, until: Until) -> bool {
        self.until >= until
    }

    /// Whether bytes of this life are still valid at `until` in the record of life `life` (M3 L0-a, D323 / D333, main 09-24:
    /// "a Life claim reaches only the same LifeId; anything else copies"): a record's `Life` cell reaches `Life` only in its
    /// own record; every other pair decides by `until` as `reaches` does.
    pub fn reaches_into(self, until: Until, life: LifeId) -> bool {
        match self.home {
            CellHome::Record(l) if self.until == Until::Life && until == Until::Life => l == life,
            _ => self.reaches(until),
        }
    }
}

/// Where a cell's IR value comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum At {
    Lane(LaneId),
    /// The source's row locator (H28): the value a fetch form (`topk.fetch`) or a row op keys a row by.
    Locator,
    Slot(u16),
    Value(u16),
    Step { k: u8, col: u16 },
    Const(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Nullable {
    Never,
    Maybe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Cell {
    pub rep: Rep,
    pub typid: u32,
    pub num: NumTypmod,
    pub life: CellLife,
    pub at: At,
    pub null: Nullable,
}

/// `tid`: the locator's SQL type (a heap TID; a pgrc granule row is carried in the same word).
pub const TIDOID: u32 = 27;

impl Cell {
    /// Whether only this cell's hash image may be read (`CellHome::ImageOnly`).
    pub fn image_only(&self) -> bool {
        matches!(self.life.home, CellHome::ImageOnly)
    }

    /// A source lane's cell: every fact from decompose's `LaneDesc`.
    pub fn of_lane(lane: LaneId, d: &LaneDesc, typid: u32, null: Nullable) -> Cell {
        Cell { rep: d.rep, typid, num: d.num, life: CellLife::of_lane(d.life), at: At::Lane(lane), null }
    }

    /// A computed cell: every fact from the node's `ExprOut` (`graph.out_of`).
    pub fn of_expr(out: ExprOut, value: u16, site: u16, null: Nullable) -> Cell {
        let num = crate::rep::num_of(out.rep);
        Cell { rep: out.rep, typid: out.typid, num, life: CellLife::of_expr(out.life, site), at: At::Value(value), null }
    }

    /// A record column read back (§3.4): the stored `Rep`, living as long as the record's life.
    pub fn of_record(rep: Rep, typid: u32, num: NumTypmod, life: LifeId, slot: u16, null: Nullable) -> Cell {
        Cell { rep, typid, num, life: CellLife { until: Until::Life, home: CellHome::Record(life) }, at: At::Slot(slot), null }
    }

    /// The source's row locator (H28): one by-value word, never NULL, valid for the window it was read in.
    pub fn of_locator() -> Cell {
        Cell { rep: Rep::Word { bytes: 8 }, typid: TIDOID, num: NumTypmod::NONE, life: CellLife { until: Until::Window, home: CellHome::FrameSlot }, at: At::Locator, null: Nullable::Never }
    }
}

/// The cells a row carries, in the pipeline's column order.
#[derive(Clone, Debug)]
pub struct Row<'e> {
    pub cells: PgVec<'e, Cell>,
}

impl<'e> Row<'e> {
    /// No cells, in `mcx`.
    pub fn new_in(mcx: Mcx<'e>) -> Row<'e> {
        Row { cells: PgVec::new_in(mcx) }
    }

    /// The site's column lanes as cells (`Row::from_site`): `types` names each lane's SQL type, `nulls` its nullability.
    pub fn from_site(lanes: &[LaneDesc], types: &[u32], nulls: &[Nullable], mcx: Mcx<'e>) -> Row<'e> {
        let cells = lanes.iter().enumerate().map(|(k, d)| Cell::of_lane(LaneId::col(k as u16), d, types.get(k).copied().unwrap_or(0), nulls.get(k).copied().unwrap_or(Nullable::Maybe))).collect_in(mcx);
        Row { cells }
    }

    pub fn get(&self, k: usize) -> Option<&Cell> {
        self.cells.get(k)
    }

    /// The cell of lane `lane`, if the row carries it.
    pub fn lane(&self, lane: LaneId) -> Option<&Cell> {
        self.cells.iter().find(|c| c.at == At::Lane(lane))
    }
}
/// A group / DISTINCT key's cell rep in pipeline `producer`'s row, known before compile (face/keyimg.rs `record_words`
/// sizes a narrow numeric key by it, D1391): a scan column's lane, a computed expression's
/// decompose output, a projection column's entry's, a sort run's drained varlena column, a join probe's inner column (the
/// build row's, D1391) or a keyed group emit's kept numeric copy (V4R-569) (`expr_rep`); `None` otherwise.
/// D3703: worked out once per (producer, resno, expression) for the lowering (memo.rs `key_reps`): a key over a drained group's emit
/// asks the drained group's key in turn, down every level of a chain of nested set operations, so each level asking again
/// made the lowering quadratic in the levels.
pub(crate) fn key_cell_rep(gx: &Gx<'_, '_>, producer: usize, k: &KeyDecl<'_>) -> Option<Rep> {
    let memo = crate::memo::key_of(&gx.graph.pipelines);
    let who = (gx.over.map_or(0, |o| o as *const _ as usize), gx.facts as *const _ as *const () as usize, gx.catalog as *const _ as *const () as usize);
    let expr = k.expr.node().as_raw().as_ptr() as usize;
    let known = crate::memo::read(memo, |m| match &m.key_reps {
        Some((w, reps)) if *w == who => reps.get(producer).and_then(|r| r.iter().find(|e| e.0 == k.resno && e.1 == expr)).map(|e| e.2),
        _ => None,
    });
    if let Some(Some(rep)) = known {
        return rep;
    }
    let rep = key_cell_rep_of(gx, producer, k);
    if known.is_some() && producer < gx.graph.pipelines.len() {
        let n = gx.graph.pipelines.len();
        crate::memo::write(memo, |m| {
            if !matches!(&m.key_reps, Some((w, _)) if *w == who) {
                m.key_reps = Some((who, vec![Vec::new(); n])); // alloc: Program
            }
            if let Some((_, reps)) = &mut m.key_reps {
                reps[producer].push((k.resno, expr, rep)); // alloc: Program
            }
        });
    }
    rep
}

fn key_cell_rep_of(gx: &Gx<'_, '_>, producer: usize, k: &KeyDecl<'_>) -> Option<Rep> {
    // a group key is its resno's column of the producer's row (keyed/emit_group.rs key_col); its expr is over the row entering
    // the last Project (a join's, whose own columns name the probe's outer and inner rows). D3839: so is a DISTINCT argument
    // (resno 0) key_col finds a column of that row for (the entry whose expr is the key's, which GroupRec reads as its
    // `ksrc`): its expr read at the row's end named another entry of the projection, or none (count(DISTINCT n) over a
    // join's probe column was unresolved, a varlena, so its record took two words where the probe's packed lane images one).
    // A streamed group's (its GroupRun marker) keeps the expr's reading.
    use gravity_decompose::Op;
    let ops = &gx.pipelines()[producer].ops;
    let col = match k.resno {
        0 if ops.iter().any(|op| matches!(op, Op::GroupRun(_))) => None,
        0 => crate::table::emit_group::key_col(gx, producer, k),
        r => Some(r),
    };
    match col.filter(|_| ops.iter().any(|op| matches!(op, Op::Project(_)))) {
        Some(c) => col_at(gx, producer, ops.len(), types_nodes::primnodes::OUTER_VAR, c),
        None => expr_rep(gx, producer, k.expr.node()),
    }
}

/// `cell_rep` of expression `e` over the producer's row at its end (a key's, a DISTINCT argument's over outer.N, HO).
fn expr_rep(gx: &Gx<'_, '_>, producer: usize, e: types_nodes::Node<'_>) -> Option<Rep> {
    rep_at(gx, producer, gx.pipelines()[producer].ops.len(), e)
}

/// `expr_rep` of `e` over the row entering op `pos` of `producer`: a computed expression's decompose output, a Var's column
/// (`col_at`).
fn rep_at(gx: &Gx<'_, '_>, producer: usize, pos: usize, e: types_nodes::Node<'_>) -> Option<Rep> {
    let Some(v) = e.as_var() else { return gx.out_of(e).map(|o| o.rep) };
    (v.varlevelsup == 0).then(|| col_at(gx, producer, pos, v.varno, v.varattno)).flatten()
}

/// Column `a` of `varno` over the row entering op `pos` of `producer`, shaped by the last Project or join before `pos`
/// (D1391): a Project's column is its entry's, read at that Project; a join's outer column is the row entering the join; a
/// hash probe's inner column is its build row's (the build pipeline's column `a` at its end: the build keeps the cell,
/// join/build.rs `rep::retain`, a varlena staying a VarlenaPtr), so each Var resolves through the op that owns it along a
/// chain of probes. A merge, nest or drain join's inner column is not resolved; a scan Var is its lane; an outer Var with no
/// shaper is the source row's (`source_rep`). D3839: a scan Var of another varno than the source's (a SubqueryScan's, its
/// subquery RTE's) is column `a` of its subplan's row, the Project before it: decompose maps the scan as its subplan's ops then
/// a Project of its target list, whose Vars read the row's columns (step/expr.rs `Outer`); `source_rep` alone missed it on the
/// varno, so a union's Subquery Scan arm over a packed lane read as unresolved (a varlena) and keyed its record in two words.
fn col_at(gx: &Gx<'_, '_>, producer: usize, pos: usize, varno: i32, a: i16) -> Option<Rep> {
    crate::stack_check();
    use gravity_decompose::{join::JoinDesc, Op};
    use types_nodes::primnodes::{INNER_VAR, OUTER_VAR};
    let p = &gx.pipelines()[producer];
    let shapes = |op: &Op<'_>| matches!(op, Op::Project(_) | Op::Join(JoinDesc::Probe { .. } | JoinDesc::Merge { .. } | JoinDesc::Nest { .. } | JoinDesc::Drain { .. }));
    if varno != OUTER_VAR && varno != INNER_VAR {
        let own = p.source.relation().is_some_and(|s| s.0 as i32 == varno);
        return match p.ops[..pos].iter().rposition(shapes).map(|i| (i, &p.ops[i])) {
            Some((i, Op::Project(l))) if !own => rep_at(gx, producer, i, l.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == a)?.expr),
            _ => source_rep(gx, producer, varno, a),
        };
    }
    match p.ops[..pos].iter().rposition(shapes).map(|i| (i, &p.ops[i])) {
        Some((i, Op::Project(l))) if varno == OUTER_VAR => rep_at(gx, producer, i, l.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == a)?.expr),
        // D3839: past a draining hash join's Drain (RIGHT / FULL), an outer column is the probe's row's: the probe pipeline
        // runs the ops after the Drain as its tail (hashjoin/drain/lower.rs, the `m` rule) and the drain pipeline's own outer
        // cells are NULLs (drain/source.rs), which image at the record's words (rec.rs `mantissa_cell`); the pipeline's
        // breaker source is the build, whose columns are the inner ones
        Some((_, Op::Join(JoinDesc::Drain { matches, .. }))) if varno == OUTER_VAR => {
            let probe = gx.life_of(producer, *matches)?.0 as usize;
            col_at(gx, probe, gx.pipelines()[probe].ops.len(), OUTER_VAR, a)
        }
        Some((i, _)) if varno == OUTER_VAR => col_at(gx, producer, i, OUTER_VAR, a),
        Some((_, Op::Join(JoinDesc::Probe { slot, .. }))) => {
            let build = gx.life_of(producer, *slot)?.0 as usize;
            col_at(gx, build, gx.pipelines()[build].ops.len(), OUTER_VAR, a)
        }
        Some(_) => None,
        None => (varno == OUTER_VAR).then(|| source_rep(gx, producer, OUTER_VAR, a)).flatten(),
    }
}

/// D3931: column `a` of pipeline `producer`'s row entering op `pos`, as the cell lowerv4 builds it there (`col_at` of an
/// outer column): a MergeAppend arm's column as the arm writes it into the merge's run (rep/keylane.rs `packed_words`).
pub(crate) fn row_col_rep(gx: &Gx<'_, '_>, producer: usize, pos: usize, a: i16) -> Option<Rep> {
    col_at(gx, producer, pos, types_nodes::primnodes::OUTER_VAR, a)
}

/// The source row's column `a` of pipeline `k`, read as `varno`: a scan's lane, or (an outer Var over a breaker) a sort run's
/// drained varlena column, which the drain hands out as its var copy (runs/sort.rs `Form::Var`), or a keyed group emit's
/// kept numeric copy (`emit_copy`) or packed key word (`emit_packed`).
fn source_rep(gx: &Gx<'_, '_>, k: usize, varno: i32, a: i16) -> Option<Rep> {
    let p = &gx.pipelines()[k];
    if varno == types_nodes::primnodes::OUTER_VAR {
        let gravity_decompose::Source::Breaker { slot } = p.source else { return None };
        let life = gx.life_of(k, slot)?;
        let sorted = gx.pipelines()[life.0 as usize].ops.iter().any(|op| matches!(op, gravity_decompose::Op::Sort(_)));
        return (sorted || emit_copy(gx, k, a)).then_some(Rep::VarlenaPtr).or_else(|| emit_packed(gx, k, a));
    }
    let (_, _, site) = p.source.relation().filter(|s| varno == s.0 as i32)?;
    site.lanes.iter().find(|l| l.id == gravity_program::LaneId::Attr(a)).map(|l| l.rep)
}

/// V4R-569 (D1391): column `a` of pipeline `k` draining a keyed group's emit (keyed.emit_group; runs.group / runs.unique emit
/// through it) is key resno `a` (emit_group.rs `record_row`); a NumMantissa key whose own cell is no packed lane leaves as its
/// kept copy, a VarlenaPtr (emit_group.rs `rep_src`'s datum arm, emit_key.rs `KeyOut::Datum`), a packed one as its word
/// (`KeyOut::Num`, `emit_packed`). Asked only where the sort line answers None: a group key trunk refused (D1391, the
/// width Bug) or a DISTINCT argument trunk served on one mantissa word (V4R-569 review F1); either way `key_copy_wall`.
fn emit_copy(gx: &Gx<'_, '_>, k: usize, a: i16) -> bool {
    let copy = emit_key(gx, k, a).is_some_and(|(g, kd)| kd.image.is_mantissa() && !matches!(key_cell_rep(gx, g.producer.0 as usize, kd), Some(Rep::PackedNum { .. })));
    if copy {
        key_copy_wall(gx, gx.pipelines()[k].node_id);
    }
    copy
}

/// D3208: column `a` of pipeline `k` draining a keyed group's emit whose key resno `a` is a packed lane's (copy_words
/// `packed_key`): the emit renders it from its record's mantissa word (emit_key.rs `KeyOut::Num`), so the cell is that
/// word at the lane's facts and a group over it keeps one word (keyimg.rs `record_words`), the width its image is.
fn emit_packed(gx: &Gx<'_, '_>, k: usize, a: i16) -> Option<Rep> {
    let (g, kd) = emit_key(gx, k, a)?;
    let facts = crate::ops::sort::topn::copy_words::packed_key(gx, g.producer.0 as usize, kd)?;
    Some(Rep::PackedNum { facts, form: gravity_program::NumForm::Word })
}

/// The drained group's key resno `a` when pipeline `k` drains a keyed group's emit (keyed.emit_group; runs.group /
/// runs.unique emit through it; emit_group.rs `record_row`).
fn emit_key<'a, 'p>(gx: &'a Gx<'_, 'p>, k: usize, a: i16) -> Option<(&'a gravity_decompose::ops::shared::agg::GroupDecl<'p>, &'a KeyDecl<'p>)> {
    use crate::face::kinds::KindId::{KeyedEmitGroup, RunsGroup, RunsUnique};
    let g = (a > 0 && matches!(gx.source_kind(k), KeyedEmitGroup | RunsGroup | RunsUnique)).then(|| gx.drained_group(k)).flatten()?;
    Some((g, g.keys.iter().find(|kd| kd.resno == a)?))
}

/// V4R-569's no-widen (D2253; D2180, D2197, D2178 / D2179 and D2213 left it at their fixes, D2213 by D3286: a SubPlan
/// reads a pgrc inner's cells under the inner's site): statement-wide over the graph pipelines and every embedded inner,
/// fail closed, by D-id name.
fn key_copy_wall(gx: &Gx<'_, '_>, node: i32) {
    use gravity_decompose::join::JoinDesc;
    use gravity_decompose::{Op, Source};
    let (ps, inners) = (gx.pipelines(), gx.pipelines().inners());
    let all = || ps.iter().chain(inners.iter().map(|x| x.1));
    let op = |f: &dyn Fn(&Op<'_>) -> bool| all().any(|p| p.ops.iter().any(f));
    let why = match () {
        _ if op(&|o| matches!(o, Op::Join(JoinDesc::Merge { .. }))) && all().any(|p| matches!(p.source, Source::Sample { .. })) => "a keyed group emit's numeric key copy read as a key in a statement with a merge join beside a TABLESAMPLE scan (D2253)",
        _ if op(&|o| matches!(o, Op::Join(JoinDesc::Probe { .. } | JoinDesc::Merge { .. }))) => "a keyed group emit's numeric key copy read as a key in a statement with a hash or merge join (D2194 / D2147 / D2201)",
        _ => return,
    };
    crate::refuse!(node, crate::admit::refusal::Reason::Shape { why: crate::admit::refusal::ShapeWhy(why) })
}
