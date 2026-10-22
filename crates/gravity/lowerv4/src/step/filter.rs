//! Quals over cells, kind `step.filter` (KWC lowerv3 scan/filter.rs): the row passes on when every qual is true, evaluated
//! in plan order and stopping at the first false or NULL (`ExecQual`), so a later qual never raises for a row an earlier
//! one dropped (D237). The block arm (H01): as the first step over a source with block values, whole 64-row blocks are
//! tested by mask through the mask road that is on (`mask.vec`, else `mask.word`) when every qual has a word form (a
//! per-code qual through road `percode.table`, under a clean bind; a LIKE over a verbatim lane through road `text.frame`,
//! once its window's pass ran); the rows of the mask then run the chain with `env.masked` set and skip the per-row
//! tests. A later filter op joins the block span (frame.rs `block_span`) when its quals are plain word or vector forms
//! (raise-free), after the block's late lanes are filled when it reads one (`block_reads`), and in the packed body a lower
//! and an upper bound in two adjacent spanned ops mask as one range at the first (mask_vec.rs `pair`). Budget 250 lines.
//! Owner M1-FRAME.
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gravity_abi::ctx::lane::KIND_BYVAL;
use gravity_decompose::Op;
use gravity_expr::{ExprNode, Source as VarSource};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, Type, Value};
use gravity_program::LaneId;
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};

use crate::cell::LaneId as CellLane;
use crate::dict::face::{PerValue, PerValueImpl};
use crate::face::kinds::KindId;
use crate::frame::mask_rows::MASK_ROWS;
use crate::face::step::{BlockForm, BlockImpl, ExtPlan, Hoist, HoistAt, StepMech};
use crate::frame::env::Env;
use crate::frame::frame::{Mask, Next, StepMaker, WordWin};
use crate::frame::instr::{self, Ctr};
use crate::frame::px::{ExId, Px};
use crate::frame::row::{Carry, RowVals};

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::StepFilter, &StepMaker(make))], ..crate::ops::NONE };

pub(crate) struct Filter {
    quals: Rc<[ExId]>,
    node: i32,
    /// Every qual has the word form over the site's lanes (probed at make).
    words: bool,
    /// The site's per-code tables, when a qual reads by code (its guard needs the bind clean).
    tabs: Option<Rc<RefCell<crate::dict::percode::Tables>>>,
    /// The block road `block_open` chose for this lowering.
    form: Cell<Option<BlockForm>>,
    /// The op, its pipeline's leading filter ops' lone plain quals (`None` for any other op), and every qual plain (no
    /// per-code table, no text frame: a raise-free word or vector form, which a later op needs to join a block span).
    op: usize,
    ops: Rc<[Option<ExId>]>,
    plain: bool,
    /// A bitmap morsel form's recheck (`bitmapqualorig`, decompose closure.rs `morsel_form`): tested only on a
    /// window whose page asks for it (`site::PAGE_RECHECK`: a lossy page, or an exact page its AM marked), as
    /// PostgreSQL's bitmap heap scan rechecks; a row path (no block form).
    guard: bool,
    /// Road `mask.rows` (frame/mask_rows.rs): this leading op's quals may be evaluated for every row of a block, ANDed with
    /// no branch on one (`row_quals`); (op 0, where the road applies) its counters' frame words and this lowering's view.
    rows: bool,
    counters: Option<[i32; 4]>,
    stats: Cell<Option<crate::frame::mask_rows::Stats>>,
}

/// The by-value lanes `(attno, width)` of pipeline `k`'s site draft (none without one).
fn word_lanes<'g>(px: &mut Px<'_, '_>, k: usize, mcx: Mcx<'g>) -> PgVec<'g, (i16, u8)> {
    let Some(s) = px.site_mut(k) else { return PgVec::new_in(mcx) };
    s.lanes.iter().filter_map(|l| if let LaneId::Attr(a) = l.id { (l.kind == KIND_BYVAL).then_some((a, l.width)) } else { None }).collect_in(mcx)
}

/// Qual `q` reads by code (a per-code plan).
fn coded(px: &Px<'_, '_>, q: ExId) -> bool {
    px.placed(q).ext.as_deref().is_some_and(|x| x.is::<crate::dict::percode::Plan>())
}

/// D2997: qual `q`'s per-code plan carries num.cmp's residual claim (step/expr_ext.rs `residual`): its placeholders are the
/// row path's spliced sites, which the per-code word road does not read, so the filter takes no block form.
fn residual(px: &Px<'_, '_>, q: ExId) -> bool {
    px.placed(q).ext.as_deref().and_then(|x| x.downcast_ref::<crate::dict::percode::Plan>()).is_some_and(|p| p.ext.get().is_some())
}

/// Qual `q` has a text frame or an extension plan.
fn framed(px: &Px<'_, '_>, q: ExId) -> bool {
    crate::dict::strategies::text_frame::frame_of(px.placed(q)).is_some() || ExtPlan::of(px.placed(q)).is_some()
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let p = &px.gx.pipelines()[k];
    let node = p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(p.node_id, |n| n.node_id);
    let quals = crate::step::expr::quals(px, k, j);
    if !quals.iter().all(|&q| crate::dict::percode::owns(px, k, q)) {
        crate::refuse!(node, crate::admit::refusal::Reason::Bug { payload: "a filter qual whose per-code reads are another making's tables (D3913 class)" });
    }
    let mcx = px.mcx();
    let lanes = word_lanes(px, k, mcx);
    let words = quals.iter().all(|&q| match (coded(px, q), framed(px, q)) {
        (true, _) if residual(px, q) => false,
        (true, _) => crate::dict::percode_len::has_word(px, q),
        (_, true) => true,
        _ => crate::frame::block::has_form(px, q, &lanes[..]),
    });
    let tabs = quals.iter().any(|&q| coded(px, q)).then(|| crate::dict::percode::tables(px, k));
    let plain = quals.iter().all(|&q| !coded(px, q) && !framed(px, q));
    let lead = px.gx.pipelines()[k].ops.iter().take_while(|op| matches!(op, Op::Filter(_))).count();
    let ops = if j < lead { lead_ops(px, k, lead) } else { Rc::from([]) }; // alloc: Program
    let guard = matches!(&px.gx.pipelines()[k].source, gravity_decompose::Source::Bitmap { morsel: true, recheck, .. } if j < recheck.len());
    let rows = j < lead && !guard && row_quals(px, k, j, &quals);
    let road = px.strategies.has(MASK_ROWS) && crate::face::source::on(crate::admit::refusal::Variant::Strategy(MASK_ROWS));
    let counters = (rows && j == 0 && road).then(|| {
        [0u8, 1, 2, 3].map(|i| px.frame_word(crate::frame::px::WordKey::Open { face: crate::face::kinds::Face::Step, k, j: crate::frame::px::key_j(j), i }) as i32 * 8)
    });
    mcx::pg_box_dyn!(px.mcx(), Filter { quals, node, words: words && !guard, tabs, form: Cell::new(None), op: j, ops, plain: plain && !guard, guard, rows, counters, stats: Cell::new(None) } => dyn StepMech + 'g)
}

/// Road `mask.rows` (frame/mask_rows.rs): pipeline `k`'s leading filter op `j`'s quals `qs` may each be evaluated for
/// every row of a block, its outcome ANDed with no branch (a later qual of a row an earlier one drops is evaluated too;
/// its raise word is kept, the row then walks the row path): no VOLATILE call or SubPlan in the op (`effects::volatile`,
/// `has_subplan`), no qual that reads by code, through a text frame, an InitPlan's result or a held result, no node that
/// is not a plain expression's, none that branches on its data inside (an IN list, AND / OR, CASE, COALESCE, GREATEST /
/// LEAST: the mask cannot take those branches away, and a later op's would be paid for every row; an IN of two
/// one-character codes at half the rows read 3 % more cycles under the mask, lane brfix); every qual but op 0's first
/// small (at most `ROW_QUAL_NODES` nodes, no extension plan: the row path stops at the first false, so a later qual costs
/// every row only when it is cheap).
fn row_quals(px: &Px<'_, '_>, k: usize, j: usize, qs: &[ExId]) -> bool {
    const ROW_QUAL_NODES: usize = 6;
    let Op::Filter(n) = px.gx.pipelines()[k].ops[j] else { return false };
    if crate::effects::volatile(px.gx, n) || gravity_decompose::expr::has_subplan(n) {
        return false;
    }
    qs.iter().enumerate().all(|(i, &q)| {
        let e = px.placed(q);
        let init = e.places.iter().any(|p| matches!(p, gravity_lower::Place::Exec(x) if crate::rerun::params::init_of(*x).is_some()));
        let plain = e.c.prog.nodes.iter().all(|n| !matches!(n, ExprNode::SubPlan(_) | ExprNode::Srf(_) | ExprNode::NextValue(_) | ExprNode::CurrentOf(_) | ExprNode::Returning(_) | ExprNode::Aggref(_) | ExprNode::WindowFunc(_) | ExprNode::GroupingFunc(_)));
        let straight = e.c.prog.nodes.iter().all(|n| match n {
            ExprNode::Bool(x) => x.op == types_nodes::primnodes::BoolExprType::NOT_EXPR,
            n => !matches!(n, ExprNode::ScalarArrayOp(_) | ExprNode::Case(_) | ExprNode::Coalesce(_) | ExprNode::MinMax(_)),
        });
        let small = (j == 0 && i == 0) || (e.c.prog.nodes.len() <= ROW_QUAL_NODES && ExtPlan::of(e).is_none());
        !coded(px, q) && crate::dict::strategies::text_frame::frame_of(e).is_none() && !init && e.held.is_empty() && plain && straight && small
    })
}

/// Pipeline `k`'s leading `lead` filter ops' lone plain quals (`Filter::ops`), built once per pipeline identity (the
/// quals' own, D3913): every leading op shares one list, so a filter of N ops costs N, not N squared (D1941).
fn lead_ops(px: &mut Px<'_, '_>, k: usize, lead: usize) -> Rc<[Option<ExId>]> {
    let key = (k, px.ident(k));
    if let Some(ops) = crate::step::expr::ext(px).leads.get(&key) {
        return Rc::clone(ops);
    }
    let mcx = px.mcx();
    let mut ops: PgVec<'_, Option<ExId>> = mcx::reserved_in(lead, mcx);
    for i in 0..lead {
        let q = crate::step::expr::quals(px, k, i);
        ops.push_in(q.first().copied().filter(|&q0| q.len() == 1 && !coded(px, q0) && !framed(px, q0)));
    }
    let ops: Rc<[Option<ExId>]> = (&ops[..]).into(); // alloc: Program
    crate::step::expr::ext(px).leads.insert(key, Rc::clone(&ops)); // alloc: Program
    ops
}

/// `pass` true: the row goes on through `next`; else the carry is unchanged. The carry after the join.
pub(crate) fn when<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, pass: Value, next: Next<'_, 'e>) -> Carry<'e> {
    let types: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
    let l_join = b.create_block(&types[..]);
    let l_yes = b.create_block(&[]);
    b.cond_br(pass, (l_yes, &[]), (l_join, &row.carry.params()));
    b.switch_to(l_yes);
    let c = next(b, env, row);
    b.br(l_join, &c.params());
    b.switch_to(l_join);
    Carry::from_params(b.block_params(l_join), env.mcx)
}

/// Quals `qs` over the row, each tested before the next is evaluated (a false or NULL qual skips the rest and the row);
/// the rows that pass go on through `next`. The carry after the join.
pub(crate) fn quals_then<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, node: i32, row: &RowVals, qs: &[ExId], next: Next<'_, 'e>) -> Carry<'e> {
    let types: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
    let l_join = b.create_block(&types[..]);
    // EXPLAIN ANALYZE's rows the node's quals removed (D349), counted where the row fails, for the node whose rows the frame
    // counts, in a frame without node boundaries only (a bounded frame counts rows where they leave a node, frame.rs)
    let params;
    let (l_fail, fail): (_, &[Value]) = match instr::filters_counted(node, env.node) {
        true => (b.create_block(&[]), &[]),
        false => {
            params = row.carry.params();
            (l_join, &params[..])
        }
    };
    for &q in qs.iter() {
        let pass = crate::step::expr::pred(b, env, node, row, q);
        let l_yes = b.create_block(&[]);
        b.cond_br(pass, (l_yes, &[]), (l_fail, fail));
        b.switch_to(l_yes);
    }
    let c = next(b, env, row);
    b.br(l_join, &c.params());
    if l_fail != l_join {
        b.switch_to(l_fail);
        instr::count_one(b, env.ctx, node, Ctr::Filtered1);
        b.br(l_join, &row.carry.params());
    }
    b.switch_to(l_join);
    Carry::from_params(b.block_params(l_join), env.mcx)
}

/// The block road a filter takes: the vector form where it is on, else the scalar word; `None` when neither is.
fn block_road() -> Option<BlockForm> {
    [BlockForm::Vec, BlockForm::Word].into_iter().find(|f| f.on())
}

impl StepMech for Filter {
    /// The block guard: the window's rows are its lanes' rows in order and every qual's word form holds over the window's
    /// lanes (each constant fits its lane); `None` when a qual has no word form or no mask road is on.
    fn block_open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, w: &WordWin) -> Option<Mask> {
        if !self.words || (self.op > 0 && !self.plain) {
            return None;
        }
        let form = block_road()?;
        let clean = self.tabs.as_ref().map(|t| crate::dict::percode_bind::clean(b, env, t));
        let mut ok = None;
        for &q in self.quals.iter() {
            let g = match (crate::step::expr::plan_of(env, q).is_some(), clean) {
                (true, Some(c)) => crate::dict::percode_len::word_open(b, env, w, q, c)?.0,
                _ if crate::dict::strategies::text_frame::frame_of(env.placed(q)).is_some() => crate::dict::strategies::text_frame::open(b, env, q)?.0,
                _ => match ExtPlan::of(env.placed(q)) {
                    Some(x) => x.mech.mask_open(b, env, w, q)?.0,
                    None => crate::frame::block::open(b, env, w, q)?.0,
                },
            };
            ok = Some(match ok {
                Some(o) => b.int_bin(IntBinOp::And, o, g),
                None => g,
            });
        }
        self.form.set(Some(form));
        ok.map(Mask)
    }

    /// The quals' constants, once per window.
    fn hoist(&self, h: &mut Hoist) {
        for &q in self.quals.iter() {
            h.add(HoistAt::Window, q);
        }
    }

    /// The rows of `[base, base + 64)` within `m` that pass every qual, by the chosen mask road; a fused pair's range at
    /// its first op, and `m` at its second.
    fn block(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, w: &WordWin, base: Value, m: Mask) -> Mask {
        let out = self.block_mask(b, env, w, base, m);
        // EXPLAIN ANALYZE: the block's rows these quals dropped (a scan's Rows Removed by Filter)
        instr::count_mask_dropped(b, env.ctx, self.node, env.node, m.0, out.0);
        out
    }

    /// The count form under mask.vec (`EndMech::counts`) of the whole block through the span: a span of this step alone
    /// with one packed compare, or of two ops this step fuses as a range, over a NOT NULL lane (mask_vec.rs `packed_count`).
    fn block_count(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, w: &WordWin, _base: Value) -> Option<Value> {
        let form = self.form.get().filter(|f| matches!(f, BlockForm::Vec) && self.plain)?;
        let range = match (env.span.1, self.pairs(env, w, form).first()) {
            (1, _) => None,
            (2, Some(&p)) if p.0 == self.op => Some(self.pair(env, w, p)),
            _ => return None,
        };
        let n = crate::frame::mask_vec::packed_count(b, env, w, &self.quals, range)?;
        // EXPLAIN ANALYZE: the whole block's rows the quals dropped (a scan's Rows Removed by Filter)
        if instr::filters_counted(self.node, env.node) {
            let sixty_four = b.iconst(Type::I64, 64);
            let dropped = b.int_bin(IntBinOp::Sub, sixty_four, n);
            instr::count(b, env.ctx, self.node, Ctr::Filtered1, dropped);
        }
        Some(n)
    }

    /// The lanes this step reads from their buffers past op 0's (`StepMech::block_reads`; bit `a - 1`, lanes past 64 decode
    /// eagerly): none at a fused pair's second op or for a packed compare (mask_vec.rs `packed_leaf`: its raw deltas), op
    /// 0's are its lone plain qual's (none when not known), and every lane for a column that is not the scan's.
    fn block_reads(&self, env: &Env, w: &WordWin) -> u64 {
        let reads = |q: ExId| match crate::frame::mask_vec::packed_leaf(env, w, q) {
            Some(_) => 0,
            None => env.placed(q).c.prog.nodes.iter().fold(0u64, |r, n| match n {
                ExprNode::Var(v) if v.src != VarSource::Scan || v.attno <= 0 => u64::MAX,
                ExprNode::Var(v) if v.attno <= 64 => r | 1 << (v.attno - 1),
                _ => r,
            }),
        };
        match self.form.get() {
            Some(f) if self.pairs(env, w, f).iter().any(|p| p.1 == self.op) => 0,
            Some(_) => self.quals.iter().fold(0, |r, &q| r | reads(q)) & !self.ops.first().copied().flatten().map(reads).filter(|&r| r != u64::MAX).unwrap_or(0),
            None => u64::MAX,
        }
    }

    /// Road `mask.rows`: the counters' words, zeroed at the run's open.
    fn open_words(&self, w: &mut crate::frame::open::OpenWords) {
        for (&off, what) in self.counters.iter().flatten().zip(["mask.rows passes", "mask.rows flips", "mask.rows windows", "mask.rows rows"]) {
            w.declare(crate::frame::open::OpenWord::open(what, crate::frame::open::Base::Words, off, Type::I64, 0));
        }
    }

    /// Road `mask.rows`: the counters' view where the road applies (the lowering not instrumented).
    fn prologue(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env, _x: &crate::frame::stop::Exits) {
        let on = self.op == 0 && self.rows && env.applies(MASK_ROWS) && !instr::on();
        self.stats.set(self.counters.filter(|_| on).zip(env.words).map(|(off, at)| crate::frame::mask_rows::Stats { at, off }));
    }

    fn row_stats(&self) -> Option<crate::frame::mask_rows::Stats> {
        self.stats.get()
    }

    fn row_maskable(&self) -> bool {
        self.rows
    }

    /// Every qual's `pred` without its raise, the passes ANDed and the raise words ORed (`env.zero` when none can raise).
    fn row_pass(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) -> Option<(Value, Value)> {
        if !self.rows {
            return None;
        }
        let (mut pass, mut err) = (None, None);
        for &q in self.quals.iter() {
            let (p, e) = crate::step::expr::pred_err(b, env, self.node, row, q);
            pass = Some(match pass {
                Some(a) => b.int_bin(IntBinOp::And, a, p),
                None => p,
            });
            if !b.const_of(e).is_some_and(|(_, c)| c == 0) {
                err = Some(match err {
                    Some(a) => b.int_bin(IntBinOp::Or, a, e),
                    None => e,
                });
            }
        }
        pass.map(|p| (p, err.unwrap_or(env.zero)))
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        if env.masked {
            return next(b, env, row);
        }
        if let (true, Some(k)) = (self.guard, env.site) {
            // the window's page asks for the recheck: the quals decide the row; else it passes (one `next`, `when`)
            let s = gravity_abi::ctx::site_off(k) as i32;
            let flags = b.load_zext(Type::I16, env.ctx, s + gravity_abi::site::site::PAGE_FLAGS as i32);
            let bit = b.iconst(Type::I64, gravity_abi::site::site::PAGE_RECHECK as i64);
            let on = b.int_bin(IntBinOp::And, flags, bit);
            let zero = b.iconst(Type::I64, 0);
            let need = b.icmp(gravity_ir::IntCond::Ne, on, zero);
            let (yes, no) = (b.bconst(true), b.bconst(false));
            let l_pass = b.create_block(&[Type::Bool]);
            let l_check = b.create_block(&[]);
            b.cond_br(need, (l_check, &[]), (l_pass, &[yes]));
            b.switch_to(l_check);
            // EXPLAIN ANALYZE's Rows Removed by Index Recheck (the bitmap heap scan's nfiltered2): a row the recheck drops
            let l_fail = instr::filters_counted(self.node, env.node).then(|| b.create_block(&[]));
            let (fail, fail_args) = match l_fail {
                Some(l) => (l, mcx::pg_vec![in env.mcx]),
                None => (l_pass, mcx::pg_vec![in env.mcx; no]),
            };
            // a datum the quals read in this branch does not dominate the join: the row's memo is set aside around it
            env.aside(|env| {
                for &q in self.quals.iter() {
                    let pass = crate::step::expr::pred(b, env, self.node, row, q);
                    let l_yes = b.create_block(&[]);
                    b.cond_br(pass, (l_yes, &[]), (fail, &fail_args[..]));
                    b.switch_to(l_yes);
                }
            });
            b.br(l_pass, &[yes]);
            if let Some(l) = l_fail {
                b.switch_to(l);
                instr::count_one(b, env.ctx, self.node, Ctr::Filtered2);
                b.br(l_pass, &[no]);
            }
            b.switch_to(l_pass);
            let pass = b.block_params(l_pass)[0];
            return when(b, env, row, pass, next);
        }
        quals_then(b, env, self.node, row, &self.quals, next)
    }
}

impl Filter {
    /// `StepMech::block`'s mask: the rows of `[base, base + 64)` within `m` that pass every qual.
    fn block_mask(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, w: &WordWin, base: Value, m: Mask) -> Mask {
        let Some(form) = self.form.get() else { crate::refuse!(self.node, crate::admit::refusal::Reason::Bug { payload: "a filter block before its block_open" }) };
        let road = form.need::<BlockImpl>(&self.node).0;
        let pairs = self.pairs(env, w, form);
        match pairs.iter().find(|p| p.0 == self.op || p.1 == self.op) {
            Some(&(a, c)) if a == self.op => {
                let pr = self.pair(env, w, (a, c));
                return crate::frame::mask_vec::range_mask(b, env, w, base, pr, m);
            }
            Some(_) => return m,
            None => {}
        }
        self.quals.iter().fold(m, |m, &q| match crate::step::expr::plan_of(env, q) {
            Some(pl) => PerValue::Table.need::<PerValueImpl>(&self.node).0.mask(b, env, w, base, CellLane::col(pl.cols[0].0 as u16), q, m),
            None if crate::dict::strategies::text_frame::frame_of(env.placed(q)).is_some() => {
                PerValue::Frame.need::<PerValueImpl>(&self.node).0.mask(b, env, w, base, CellLane::col(0), q, m)
            }
            None => match ExtPlan::of(env.placed(q)) {
                Some(x) => x.mech.mask(b, env, w, base, q, m).unwrap_or_else(|| crate::refuse!(self.node, crate::admit::refusal::Reason::Bug { payload: "an extension qual's block without its mask" })),
                None => road.mask(b, env, w, base, q, m),
            },
        })
    }

    /// The span's fused ops (`Env::span`: this step's place and the span's length): left to right, two adjacent ops whose
    /// lone plain quals `pair` as a packed range, each op in one pair at most. None outside mask.vec's packed body.
    fn pairs<'e>(&self, env: &Env<'e, '_>, w: &WordWin, form: BlockForm) -> PgVec<'e, (usize, usize)> {
        let (mut out, (i, n)) = (PgVec::new_in(env.mcx), env.span);
        let Some(start) = self.op.checked_sub(i).filter(|_| matches!(form, BlockForm::Vec) && env.body.packed) else { return out };
        let q = |a: usize| self.ops.get(a).copied().flatten();
        let mut a = start;
        while a + 1 < start + n {
            match (q(a), q(a + 1)) {
                (Some(x), Some(y)) if crate::frame::mask_vec::pair(env, w, x, y).is_some() => {
                    out.push_in((a, a + 1));
                    a += 2;
                }
                _ => a += 1,
            }
        }
        out
    }

    /// A fused pair's quals as `(lower, upper)`.
    fn pair(&self, env: &Env, w: &WordWin, (a, c): (usize, usize)) -> (ExId, ExId) {
        let q = |a: usize| self.ops.get(a).copied().flatten();
        match (q(a), q(c)) {
            (Some(x), Some(y)) => crate::frame::mask_vec::pair(env, w, x, y),
            _ => None,
        }
        .unwrap_or_else(|| crate::refuse!(self.node, crate::admit::refusal::Reason::Bug { payload: "a fused pair that is not a packed range" }))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gravity_ir::Type;

    use super::Filter;
    use crate::face::step::{ExprExtMech, StepMech};
    use crate::frame::frame::{Mask, WordWin};
    use crate::frame::stop::tests::run1;
    use crate::step::expr::tests::{calls, claimed, CLAIM, CLAIM_ROWS, SERIAL};

    /// L0-c: a qual an extension claimed takes the block path through its `mask_open` guard and its `mask`; with no guard
    /// (`None`) the filter has no block form and its rows take the row path.
    #[test]
    fn an_extension_qual_masks_its_blocks() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let block = |ext: &'static dyn ExprExtMech| {
            let (exs, q) = claimed(&[ext]);
            calls();
            let f = Filter { quals: Rc::from([q]), node: 7, words: true, tabs: None, form: Cell::new(None), op: 0, ops: Rc::from([]), plain: false, guard: false, rows: false, counters: None, stats: Cell::new(None) };
            let v = run1(&exs, &mut |b, env| {
                let w = WordWin { lanes: mcx::PgVec::new_in(env.mcx), valid: mcx::PgVec::new_in(env.mcx), plain: env.one };
                match f.block_open(b, env, &w) {
                    Some(_) => {
                        let all = b.iconst(Type::I64, -1);
                        f.block(b, env, &w, env.zero, Mask(all)).0
                    }
                    None => b.iconst(Type::I64, 0),
                }
            });
            (v as u64, calls())
        };
        assert_eq!(block(&CLAIM), (0x5555_5555_5555_5555, vec!["claim.mask_open", "claim.mask"])); // alloc: Cold
        assert_eq!(block(&CLAIM_ROWS), (0, vec!["claim.mask_open"])); // alloc: Cold
    }
}
