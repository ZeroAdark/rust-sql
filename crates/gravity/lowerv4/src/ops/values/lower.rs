//! The VALUES source (KEEP lowerv3 scan/values.rs; the ladder's `lower/src/values.rs`). NEW-29 / CAP-11: every list reads
//! its Const cells from write.literals' per-execution literal table (`LiteralsMech`: no code per row, a cached program,
//! whatever its length); a row holding a cell that is not a Const (marked in the table) is compiled, its expressions over
//! the bind's words (`step/expr.rs`) and its Const cells read from the table, chosen by a bisection over those rows alone
//! (PostgreSQL's order). Without the table every row is compiled so. A stop resumes at the next row, the position kept in
//! the resume word (`RESUME_OPEN + row`, `source/result.rs`), so a list streams across out blocks. CAP-29 / D2907: past
//! the compiled form's cost (more than `cost::VALUES_CODED_CELLS_MAX` cells that are not a Const in the coded rows: the
//! per-row code's compile grows faster than its rows), its capacity (more than `cost::VALUES_CODED_ROWS_MAX` coded rows) or
//! its literal words (`cost::VALUES_CODED_WORDS_MAX`) the list streams them instead (the table's stream form): at a coded row's
//! turn PostgreSQL's evaluator writes the row's other cells into the table (ValuesNext's per-row ExecInitExprList +
//! ExecEvalExpr, `$n` from the execution's list) and the row is read as a row of Consts, with no code per row and no row
//! cap. A cell PostgreSQL's evaluator cannot take alone (a SubPlan) keeps the compiled form, and a list past the rows'
//! capacity refused by name (past the words' bound, the compiled form's own refusal). D3374 (result_cap29): a cell
//! reading an executor parameter (a lateral outer row's, a nest's, an InitPlan's) streams past the capacity: the source
//! emits each such parameter (compiled alone) at a coded row's turn into the hook's parameter slots, which the evaluation
//! reads as es_param_exec_vals; under the capacity such a list keeps the compiled form (`values::streams_for_cost`). D2775: a coded row that holds no SubPlan runs its EXECUTE gates at its turn,
//! before its cells, as ValuesNext compiles it then (`visit_gates`, raw.rs `gate`): a row a LIMIT, a FETCH or an EXISTS
//! never reaches gates nothing; a streamed row's are PostgreSQL's evaluator's own. Registers kind `src.values`. Owner
//! M1-SOURCE.
use gravity_decompose::Source;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, IntBinOp, IntCond, Type, Value};
use gravity_program::{ExprLife, ExprOut, OutCol, Rep};

use gravity_program::SiteDesc;

use crate::cell::{Cell, Nullable, Row};
use crate::face::end::{LitTable, LiteralsMech, LiteralsRoad};
use crate::face::kinds::KindId;
use crate::face::strategy::WRITE_LITERALS;
use crate::face::source::{SourceMech, WinConds};
use crate::frame::env::Env;
use crate::frame::frame::{SourceMaker, Win};
use crate::frame::px::{ExId, Px};
use crate::frame::row::{Carry, RowVals};
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::shared::values;
use crate::ops::Table;
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};
use types_nodes::primnodes::ParamKind;
use types_nodes::Node;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::SrcValues, &SourceMaker(make))], ..crate::ops::NONE };

#[derive(Clone, Copy)]
enum Src {
    Ex(ExId),
    /// Cell `(row, column)` of the per-execution literal table.
    Lit,
}

struct Values<'g> {
    /// Each row's cells; empty for a row the table answers whole.
    rows: PgVec<'g, PgVec<'g, Src>>,
    /// D2775: each row's visit-time EXECUTE gates (`visit_gates`), run before its cells; empty for a row with none.
    gates: PgVec<'g, PgVec<'g, u32>>,
    outs: PgVec<'g, ExprOut>,
    /// With the table: the rows holding an expression, in list order (`LiteralsMech::coded`).
    code: PgVec<'g, usize>,
    /// The per-execution literal table (road write.literals, write/literals.rs).
    lits: Option<(&'static dyn LiteralsMech, LitTable)>,
    /// D3374 (result_cap29): in the stream form, the executor parameters its streamed cells read (`values::exec_params`,
    /// ascending paramid), each compiled alone: their values go to the evaluation at a coded row's turn.
    xparams: PgVec<'g, ExId>,
    node: i32,
    /// D732: the copy site a write's / lock's COPY row mark on this scan stores its row in (`marks.rs`); under a recheck
    /// (`POS_COPY`) the source is that one row, read from the stored record (EvalPlanQualFetchRowMark's copy leg).
    copy: Option<std::rc::Rc<core::cell::Cell<usize>>>,
    mcx: Mcx<'g>,
}

pub(super) fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let mcx = px.mcx();
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Source::Values { lists, scanrelid } = &p.source else { crate::refuse!(node, Reason::Bug { payload: "a VALUES entry over another source" }) };
    if lists.len() == 0 {
        shape(node, "a VALUES list of no row");
    }
    let ncols = lists.iter().next().and_then(|l| l.as_list()).map_or(0, |l| l.len());
    let src = gravity_lower::inputs::key::values_node(p);
    let road = crate::ops::strategy(WRITE_LITERALS).and_then(|m| m.downcast_ref::<LiteralsRoad>());
    // CAP-29 / D2907: past the per-row code's cost (its cells), its capacity (its rows) or the words its expressions'
    // literals would take, the rows holding an expression stream (the table's stream form)
    let holds = |l: &Node<'_>| l.as_list().map_or(true, |l| l.iter().any(|n| n.as_const().is_none()));
    // D4449: a list of Consts alone (`consts_only`) has no row holding an expression, no coded cell and no expression's
    // literal word: it streams only for the plan's own words, and its rows are not walked to learn it
    let consts = values::consts_only(gx.graph, lists);
    let (over_rows, past) = if consts { (false, false) } else { (values::over_rows(lists), values::streams_past_cut(lists)) };
    let can_stream = past || consts || values::streamable(lists);
    if over_rows && !can_stream {
        shape(node, "a VALUES list of more than 65536 rows holding an expression that reads a subplan or a column");
    }
    let words = if consts { 0 } else { literal_words(lists, mcx) };
    let stream = past || (can_stream && (consts || values::streams_for_cost(lists)) && px.plan.nwords() + words > gravity_program::cost::VALUES_CODED_WORDS_MAX);
    let lit = road.and_then(|r| r.0.table(px, k, src, ncols, stream).map(|t| (r.0, t)));
    let stream = lit.is_some_and(|(_, t)| t.stream);
    if past && !stream {
        // the props walk and a plain fold's effects skipped its cells (`streams_past_cut`): never a program compiling them
        crate::refuse!(node, Reason::Bug { payload: "a VALUES list past the coded cells' cost without write.literals' stream form" });
    }
    if past && !over_rows {
        // R5-B: streamed for its coded cells' cost (`values::coded_cells_max`, the class table's rows), not its capacity
        fires::fire("values.coded_cut");
    }
    let coded = lists.iter().filter(|l| !stream && (lit.is_none() || holds(l)));
    if !(consts && lit.is_some()) && coded.count() > gravity_program::cost::VALUES_CODED_ROWS_MAX {
        shape(node, "a VALUES list of more than 65536 rows holding an expression");
    }
    let (mut rows, mut outs, mut code) = (mcx::reserved_in(lists.len(), mcx), PgVec::new_in(mcx), PgVec::new_in(mcx));
    let mut gates = mcx::reserved_in(lists.len(), mcx);
    // D4449: a cell outside the graph's outs takes its type's storage (`cell_out`), read from the catalog once per type of
    // the list, not once per cell (a 25k-row list read pg_type four times per cell)
    let mut typed: Typed = [None; TYPED];
    for (r, l) in lists.iter().enumerate() {
        crate::admit::refusal::check_interrupts(node); // D231: a long list's rows compile one by one, each a cancel point
        let Some(list) = l.as_list() else { shape(node, "a VALUES row that is not a list") };
        if !outs.is_empty() && list.len() != outs.len() {
            shape(node, "VALUES rows of different widths");
        }
        let compiled = !stream && (lit.is_none() || !consts && list.iter().any(|n| n.as_const().is_none()));
        let mut cells = mcx::reserved_in(if compiled { list.len() } else { 0 }, mcx);
        for (c, n) in list.iter().enumerate() {
            let out = match gx.out_of(n) {
                Some(o) => o,
                None => cell_out(px, node, n, &mut typed),
            };
            // CAP-29: a streamed cell lives until the next coded row's evaluation
            let out = if stream && n.as_const().is_none() { ExprOut { life: out.life.min(ExprLife::Row), ..out } } else { out };
            if outs.len() <= c {
                outs.push_in(out);
            } else if outs[c].rep != out.rep {
                shape(node, "a VALUES column whose rows differ in representation");
            } else if stream {
                outs[c].life = outs[c].life.min(out.life);
            }
            match n.as_const() {
                _ if !compiled => {}
                Some(_) if lit.is_some() => cells.push_in(Src::Lit),
                _ => cells.push_in(Src::Ex(crate::step::expr::compile(px, n, None, node))),
            }
        }
        if compiled && lit.is_some() {
            code.push_in(r);
        }
        // D2775: a row holding a SubPlan is compiled at ExecInitValuesScan, its gates exec's at the start (admit gates.rs)
        let visit = compiled && !list.iter().any(gravity_decompose::expr::has_subplan);
        gates.push_in(if visit { visit_gates(list, mcx) } else { PgVec::new_in(mcx) });
        rows.push_in(cells);
    }
    // D3374: the stream form's executor parameters, each a compiled Param (step/expr.rs: a nest's, a lateral outer row's
    // or an InitPlan's word), emitted at each coded row's turn before the evaluation reads them
    let xparams = match stream {
        true => values::exec_params(lists)
            .into_iter()
            .map(|n| {
                // the slot holds a datum word: a by-value word or a pointer to the value's image, never a lane's code
                if gx.out_of(n).is_some_and(|o| !matches!(o.rep, Rep::Word { .. } | Rep::VarlenaPtr | Rep::FixedPtr { .. } | Rep::CstrPtr)) {
                    shape(node, "a VALUES list of more than 65536 rows holding an expression that reads an executor parameter held as no datum word");
                }
                crate::step::expr::compile(px, n, None, node)
            })
            .collect_in(mcx),
        false => PgVec::new_in(mcx),
    };
    let rti = u32::from(*scanrelid);
    let copy = crate::rerun::epq::marks::copied(gx, rti).then(|| {
        // mergecmd 9 (COPYROW): the site marks.rs `copy_pos` made first for a rechecking pipeline's own VALUES scan
        if let Some(at) = px.ext::<crate::rerun::epq::marks::CopySites>(crate::face::kinds::Face::End).and_then(|c| c.0.iter().find(|e| e.0 == rti)).map(|e| e.1.clone()) {
            return at;
        }
        let at = px.add_late_site(SiteDesc::copy(px.arena, rti));
        px.ext_mut::<crate::rerun::epq::marks::CopySites>(crate::face::kinds::Face::End).0.push_in((rti, at.clone()));
        at
    });
    mcx::pg_box_dyn!(mcx, Values { rows, gates, outs, code, lits: lit, xparams, node, copy, mcx } => dyn SourceMech + 'g)
}

/// D4449: the slots of `cell_out`'s memo (a list's column types, by type OID modulo the count; a type that meets another
/// in its slot is read again, never wrong).
const TYPED: usize = 16;
type Typed = [Option<(u32, ExprOut)>; TYPED];

/// A cell's output where the graph holds none: its type's storage (`rep::storage::of`), each type's read from the catalog
/// once per list (`typed`: the outs met so far, by type). The catalog's answer for a type is one within the lowering, so
/// every cell of the type takes the same out, the first refusing as before, and the recorded dependencies are the same.
fn cell_out(px: &Px<'_, '_>, node: i32, n: Node<'_>, typed: &mut Typed) -> ExprOut {
    let typ = gravity_decompose::catalog::expr_type::type_of(n).unwrap_or(0);
    let slot = &mut typed[typ as usize % TYPED];
    if let Some((t, out)) = *slot {
        if t == typ {
            return out;
        }
    }
    let Ok(tf) = crate::rep::storage::of(px.gx.catalog, typ) else { shape(node, "a VALUES cell type the catalog refused") };
    let out = ExprOut { rep: OutCol::new("", typ, -1, tf.len, tf.byval).rep, typid: typ, life: ExprLife::Row };
    *slot = Some((typ, out));
    out
}

impl<'g> SourceMech for Values<'g> {
    fn open(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        env.zero
    }

    /// The rest of the list from the resume word's row; past the last row, the end.
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let pos = crate::frame::resume::resume_pos(b, env);
        let mut end = match self.lits {
            Some((m, t)) => m.rows(b, env, &t),
            None => b.iconst(Type::I64, self.rows.len() as i64),
        };
        if let Some(c) = self.copy.as_ref().map(|c| c.get()) {
            // D1685: the empty copy (a null-extended row's) is no row
            let one = b.iconst(Type::I64, 1);
            let none = crate::rerun::epq::copy_row::none(b, env, c);
            let one = b.select(none, env.zero, one);
            let copied = crate::rerun::epq::copy_row::copying(b, env, c);
            end = b.select(copied, one, end);
        }
        let past = b.icmp(IntCond::Sge, pos, end);
        let l_win = b.create_block(&[]);
        b.cond_br(past, (l_eof, &[env.zero]), (l_win, &[]));
        b.switch_to(l_win);
        let t = b.iconst(Type::I64, 1);
        Win { cursor: pos, end, facts: WinConds { nosel: Some(t), ..WinConds::default() } }
    }

    /// Row `j`'s cells: a bisection to the row's block, each cell evaluated there, the values joined as block params.
    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let ncols = self.outs.len();
        let tys: PgVec<'_, Type> = (0..ncols).flat_map(|_| [Type::I64, Type::Bool]).collect_in(self.mcx);
        let l_join = b.create_block(&tys[..]);
        let bare = RowVals { row: Row::new_in(env.mcx), vals: PgVec::new_in(env.mcx), cols: PgVec::new_in(env.mcx), carry: carry.copy_in(env.mcx), next, at: j };
        if let Some(c) = self.copy.as_ref().map(|c| c.get()) {
            let (l_copy, l_list) = (b.create_block(&[]), b.create_block(&[]));
            let copied = crate::rerun::epq::copy_row::copying(b, env, c);
            b.cond_br(copied, (l_copy, &[]), (l_list, &[]));
            b.switch_to(l_copy);
            self.copied_row(b, env, c, l_join);
            b.switch_to(l_list);
        }
        match self.lits {
            Some((m, t)) => {
                if t.stream {
                    // CAP-29: a coded row's cells evaluated into the table at its turn, then read as a row of Consts
                    let (l_code, l_lits) = (b.create_block(&[]), b.create_block(&[]));
                    let coded = m.coded(b, env, &t, j);
                    b.cond_br(coded, (l_code, &[]), (l_lits, &[]));
                    b.switch_to(l_code);
                    let xs: PgVec<'_, (Value, Value)> = self.xparams.iter().map(|&id| crate::step::expr::emit(b, env, self.node, &bare, id)).collect_in(self.mcx);
                    env.memo.clear();
                    m.stream(b, env, &t, j, &xs[..]);
                    b.br(l_lits, &[]);
                    b.switch_to(l_lits);
                } else if !self.code.is_empty() {
                    let (l_code, l_lits) = (b.create_block(&[]), b.create_block(&[]));
                    let coded = m.coded(b, env, &t, j);
                    b.cond_br(coded, (l_code, &[]), (l_lits, &[]));
                    b.switch_to(l_code);
                    self.bisect(b, env, &bare, j, &|i| self.code[i], 0, self.code.len(), l_join);
                    b.switch_to(l_lits);
                }
                let args: PgVec<'_, Value> = (0..ncols)
                    .flat_map(|c| {
                        let (v, ok) = m.cell(b, env, &t, j, c);
                        [v, ok]
                    })
                    .collect_in(self.mcx);
                b.br(l_join, &args[..]);
            }
            None => self.bisect(b, env, &bare, j, &|i| i, 0, self.rows.len(), l_join),
        }
        b.switch_to(l_join);
        let ps = mcx::to_vec_in(&b.block_params(l_join)[..], self.mcx);
        let t = b.bconst(true);
        let (mut rcells, mut vals) = (mcx::reserved_in(ncols, env.mcx), mcx::reserved_in(ncols, env.mcx));
        for (c, out) in self.outs.iter().enumerate() {
            let null = b.int_bin(IntBinOp::Xor, ps[2 * c + 1], t);
            rcells.push_in(Cell::of_expr(*out, c as u16, c as u16, Nullable::Maybe));
            vals.push_in((ps[2 * c], Some(null)));
        }
        let cols = (0..ncols as u16).map(Some).collect_in(env.mcx);
        RowVals { row: Row { cells: rcells }, vals, cols, carry, next, at: j }
    }

    /// The position after the last row taken, as the resume word.
    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, _out: Value) {
        crate::frame::resume::write_pos(b, env, cur);
    }
}

/// D2775: the functions a VALUES row's ExecInitExprList gates, in its order: cells in column order, each expression
/// pre-order (ExecInitFunc checks a call before compiling its arguments), a ScalarArrayOpExpr's comparison then its hash
/// function; each function once (a later occurrence of a function that passed passes). The node kinds are exec's gate
/// list's (admit gates.rs `gated`); a Const cell gates nothing.
fn visit_gates<'g>(list: &types_nodes::NodeList<'_>, mcx: Mcx<'g>) -> PgVec<'g, u32> {
    let mut out: PgVec<'g, u32> = PgVec::new_in(mcx);
    let mut add = |f: u32| {
        if f != 0 && !out.contains(&f) {
            out.push_in(f);
        }
    };
    for c in list.iter().filter(|c| c.as_const().is_none()) {
        gravity_decompose::expr::walk(c, &mut |n| {
            if let Some(x) = n.as_func_expr() {
                add(x.funcid);
            } else if let Some(x) = n.as_op_expr() {
                add(x.opfuncid);
            } else if let Some(x) = n.as_distinct_expr() {
                add(x.opfuncid);
            } else if let Some(x) = n.as_null_if_expr() {
                add(x.opfuncid);
            } else if let Some(x) = n.as_scalar_array_op_expr() {
                add(x.opfuncid);
                add(x.hashfuncid);
            }
            true
        });
    }
    out
}

/// CAP-29: the words the list's expressions would take compiled (the word plan's Sequential form: each literal occurrence
/// one word, each extern parameter one), counted before any is placed; past `cost::VALUES_CODED_WORDS_MAX` the list streams
/// instead (past the word plan's bound, `limits::PARAM_WORDS`, the compiled form refuses). Every Const under a cell that is
/// not one counts.
fn literal_words(lists: &types_nodes::NodeList<'_>, mcx: Mcx<'_>) -> usize {
    let (mut n, mut ids) = (0, PgVec::new_in(mcx));
    for c in lists.iter().filter_map(|l| l.as_list()).flat_map(|l| l.iter()).filter(|c| c.as_const().is_none()) {
        gravity_decompose::expr::walk(c, &mut |x| {
            n += usize::from(x.as_const().is_some());
            if let Some(p) = x.as_param().filter(|p| p.paramkind == ParamKind::PARAM_EXTERN && !ids.contains(&p.paramid)) {
                ids.push_in(p.paramid);
            }
            true
        });
    }
    n + ids.len()
}

impl<'g> Values<'g> {
    /// D2775: row `r`'s visit-time gates (`visit_gates`), each a host call before the row's cells; a denial raises (stashed).
    fn gate_row(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, r: usize) {
        if self.gates[r].is_empty() {
            return;
        }
        let flags = gravity_ir::HostFlags { may_error: true, ..gravity_ir::HostFlags::default() };
        let h = b.declare_host_once("values::gate", &[Type::I64], Some(Type::I64), super::raw::gate as *const () as usize, flags);
        for &f in self.gates[r].iter() {
            let oid = b.iconst(Type::I64, i64::from(f));
            let ok = b.call_host(h, &[oid], Some(Type::I64)).expect("returns its status");
            let denied = b.icmp(IntCond::Eq, ok, env.zero);
            let l_ok = b.create_block(&[]);
            let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
            b.cond_br(denied, (env.l_raise, &[e]), (l_ok, &[]));
            b.switch_to(l_ok);
        }
    }

    /// D732: the stored record's columns (`copy_row::fields`), joined at `l_join` as a list row's cells are.
    fn copied_row(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, c: usize, l_join: Block) {
        let typids: PgVec<'_, u32> = self.outs.iter().map(|o| o.typid).collect_in(self.mcx);
        let args: PgVec<'_, Value> = crate::rerun::epq::copy_row::fields(b, env, c, &typids[..]).into_iter().flat_map(|(v, ok)| [v, ok]).collect_in(self.mcx);
        b.br(l_join, &args[..]);
    }

    /// The rows `row(lo..hi)` (`row` ascending): halve on `j` until one row is left.
    fn bisect(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, bare: &RowVals, j: Value, row: &dyn Fn(usize) -> usize, lo: usize, hi: usize, l_join: Block) {
        crate::stack_check();
        if hi - lo == 1 {
            self.emit_row(b, env, bare, row(lo), l_join);
            return;
        }
        let mid = lo + (hi - lo) / 2;
        let (l_lo, l_hi) = (b.create_block(&[]), b.create_block(&[]));
        let m = b.iconst(Type::I64, row(mid) as i64);
        let below = b.icmp(IntCond::Slt, j, m);
        b.cond_br(below, (l_lo, &[]), (l_hi, &[]));
        b.switch_to(l_lo);
        self.bisect(b, env, bare, j, row, lo, mid, l_join);
        b.switch_to(l_hi);
        self.bisect(b, env, bare, j, row, mid, hi, l_join);
    }

    fn emit_row(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, bare: &RowVals, r: usize, l_join: Block) {
        crate::admit::refusal::check_interrupts(self.node); // D231: each row's code is emitted here, a cancel point per row
        self.gate_row(b, env, r);
        let cells = &self.rows[r];
        let mut args = mcx::reserved_in(2 * cells.len(), self.mcx);
        for (c, src) in cells.iter().enumerate() {
            let (v, valid) = match (*src, self.lits) {
                (Src::Ex(id), _) => crate::step::expr::emit(b, env, self.node, bare, id),
                (Src::Lit, Some((m, t))) => {
                    let row = b.iconst(Type::I64, r as i64);
                    m.cell(b, env, &t, row, c)
                }
                (Src::Lit, None) => crate::refuse!(self.node, Reason::Bug { payload: "a VALUES table cell without its table" }),
            };
            args.extend_in([v, valid]);
        }
        env.memo.clear();
        b.br(l_join, &args[..]);
    }
}
