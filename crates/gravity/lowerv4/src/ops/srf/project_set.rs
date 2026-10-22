//! Kind `srf.projectset` (L4; cards/M5.md `srf/project_set.rs`, M5-plan §3.3, §3.9, §3.10): a ProjectSet's set-returning
//! target entries as one expanding step (H32) per ProjectSet op; nested SRFs are stacked ProjectSets, each its own step,
//! which the frame composes. PG's `ExecProjectSRF` (nodeProjectSet.c) is kept call for call, per input row:
//! - lockstep (R8-L09): every function steps together under the row's offset word `k`; a row goes on while any function
//!   yields, an exhausted one gives NULL, and an input row whose sets are all empty emits nothing. The functions' values
//!   are `Env::srfs`' cells (function `i` at `cells[i]`, M5 L0-c (1)), which the Project after this step reads;
//! - PG's first call evaluates the target list left to right: each function's arguments, then its first call (its checks
//!   and, for a host set, its function's first row: `ExecMakeFunctionResultSet` runs it there). This step does the same
//!   once per input row and holds the opened sets across a stop (frame/stop.rs `held`), so a resumed row never
//!   re-evaluates a volatile argument (R8-L02);
//! - the plain entries are the Project's, per output row. PG also evaluates them in the call that finds every set ended
//!   (once more per input row: `nextval` in the select list runs n + 1 times), so this step evaluates the volatile ones
//!   there, and every entry that can raise when no set yielded at all. A plain entry that can raise, placed before a
//!   function whose open can raise, is evaluated before that open (PG's error order); a volatile one there runs on that
//!   open's error path (its effect still n + 1 times), and refuses when a raising entry, a non-plain argument or a
//!   non-IMMUTABLE host function lies between them. An entry that acts placed before a function whose arguments act
//!   (D2032: `nextval` beside `generate_series(1, nextval(..))`) runs in the first call just before those arguments, and
//!   at each later output in the set loop, into a cell the Project reads (`early_entries`);
//! - errors stream around (runtime.md R-G2): an error at output `k` leaves with the rows before it counted;
//! - stop / resume (H17 c): the offset word is `k + 1` while output `k` goes on, 0 between rows. A resumed row re-enters
//!   at `k + 1`, or at `k` when a later expanding step stopped inside output `k`'s rows (its offset word is set);
//! - cancel (§3.10): one input row can yield 10^10 rows, so the set loop's header polls.
//! The functions: srf/iter.rs's open-coded iterators (no host, no memory per element). A LIMIT after the step cuts the set
//! loop where PG's lazy ProjectSet stops (step/limit.rs exits the pipeline after the count; srf/limit_cut.rs's rule).
//! Owner M5-PSET.
use gravity_decompose::effects;
use gravity_decompose::ops::shared::srf::SrfPlace;
use gravity_decompose::Op;
use gravity_expr::emit::agg::{AggCell, CellValid};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};
use types_nodes::{Node, NodeTag};

use crate::face::kinds::{Face, KindId};
use crate::face::step::{Expand, ExpandKind, ExprMech, StepMech};
use crate::frame::cancel;
use crate::frame::env::{AggRow, Env};
use crate::frame::frame::{Next, StepMaker};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{ExId, Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::{self, ResumeWord};
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;

use crate::ops::srf::cell as srf_cell;
use crate::ops::srf::cell::Cells;
use crate::ops::srf::iter::{Iter, Opened};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::SrfProjectset, &StepMaker(make))], ..crate::ops::NONE };

/// One set function of the ProjectSet (`SrfDesc::functions` order: function `i` is cell `i`).
struct Member<'g> {
    args: PgVec<'g, ExId>,
    how: How,
    /// Its first call can raise (PG's argument checks; a host set's function): a plain entry before it is evaluated first.
    raises: bool,
    /// A volatile entry before it can be run on its open's error path only (PG ran it first): its arguments are Vars and
    /// Consts, and its open cannot see the entry's effect (an iterator, or a host set over an IMMUTABLE function).
    after_volatile: bool,
    /// D2032: an argument acts (a VOLATILE call, an acting SubPlan): the early entries before it run first.
    args_act: bool,
}

/// An open-coded iterator (its opened set's first word in `PSet::held`), or a host set over its cell (srf/cell.rs) with
/// the frame word that says it ended in this row.
enum How {
    Iter { it: Iter, at: usize },
    Cell { m: srf_cell::Member, done: i32 },
}

/// A target entry this step reads, in the target list's order: a set function, or a plain entry that can raise
/// (`volatile`: PG evaluates it once more in the ending call; `early`: its index in `PSet::early`, D2032).
enum Ent {
    Set(usize),
    Plain { ex: ExId, volatile: bool, early: Option<usize> },
}

/// D2032: an entry this step evaluates itself, into cell `functions.len() + i` of its row (`early_entries`). `val` / `ok`:
/// the frame words its value and validity are held in across the row (output 0's from the first call; a later output's
/// for a resume inside it); `before`: the target entry index of the function whose arguments it runs ahead of.
struct Early {
    ex: ExId,
    val: i32,
    ok: i32,
    before: usize,
}

struct PSet<'g> {
    /// expr.core's compile / emit (V4R-240's face).
    xm: &'static dyn ExprMech,
    node: i32,
    offset: i32,
    /// The frame words the opened sets are held in across a stop inside the row.
    held: PgVec<'g, i32>,
    members: PgVec<'g, Member<'g>>,
    ents: PgVec<'g, Ent>,
    early: PgVec<'g, Early>,
    cells: Cells,
}

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let mcx = px.mcx();
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(p.node_id, |n| n.node_id);
    let Op::Srf(d) = &p.ops[j] else { crate::refuse!(node, Reason::Bug { payload: "a projectset step over another op" }) };
    if d.place != SrfPlace::TargetList {
        crate::refuse!(node, Reason::Bug { payload: "a FROM function under srf.projectset" });
    }
    ExpandKind::Srf.require(&node);
    // a LIMIT after the step leaves the set loop where PG's lazy ProjectSet stops (srf/limit_cut.rs)
    if crate::frame::iface::holds_limit(&p.ops[j + 1..]) {
        ExpandKind::LimitCut.require(&node);
        if d.functions.iter().any(|f| crate::ops::srf::strategies::limit_cut::cut(gx, f, d.place, true) != crate::ops::srf::strategies::limit_cut::Cut::Lazy) {
            crate::refuse!(node, Reason::Bug { payload: "a target-list set the LIMIT cut does not meet lazily" });
        }
    }
    let xm = crate::frame::iface::expr_face(&node).0;
    let offset = stop::resume_word(px, ResumeWord::StepOffset { k, j: crate::frame::px::key_j(j) });
    let mut members: PgVec<'_, Member<'_>> = mcx::reserved_in(d.functions.len(), mcx);
    let mut held: PgVec<'_, i32> = PgVec::new_in(mcx);
    let mut cells = Cells::default();
    for (mi, f) in d.functions.iter().enumerate() {
        let args = f.args.iter().map(|a| xm.compile_at(px, (k, j), a, None, node)).collect_in(mcx);
        let how = match crate::ops::srf::iter::plan(px, f, node) {
            Some(it) => {
                let at = held.len();
                for i in at..at + it.nwords() {
                    held.push_in(px.frame_word(WordKey::Open { face: Face::Srf, k, j: crate::frame::px::key_j(j), i: i as u8 }) as i32 * 8);
                }
                How::Iter { it, at }
            }
            None => {
                let m = cells.member(px, f, xm.out_type(gx, f.node, node), node);
                How::Cell { m, done: px.frame_word(WordKey::Open { face: Face::Srf, k, j: crate::frame::px::key_j(j), i: 128 + mi as u8 }) as i32 * 8 }
            }
        };
        // a called set that acts may see the volatile entry's effect (STABLE and IMMUTABLE ones do not act: DECISIONS.md 3)
        let ecx = effects::Cx::without_stmt(gx.facts, px.arena);
        let sees = matches!(how, How::Cell { .. }) && effects::of_expr(&ecx, f.node, &effects::PLAIN).acts;
        let after_volatile = !sees && f.args.iter().all(plain);
        let args_act = f.args.iter().any(|a| effects::of_expr(&ecx, a, &effects::PLAIN).acts || acting_subplan(p, a));
        // an iterator is an integer series: its open raises only on a zero step (effects reads the two-argument forms as
        // raise-free, types_core::quiet::int_series; a literal step is a word bound per execution, D691, so every stepped
        // series counts)
        let raises = !matches!(how, How::Iter { .. }) || effects::of_call(gx.facts, f.funcid).may_raise;
        members.push_in(Member { args, raises, after_volatile, args_act, how });
    }
    cells.declare(px, k);
    // D439 / V4R-295: a VOLATILE set (a PL / SQL body above all) never runs ahead of a pull's count: a cursor's FETCH n
    // calls it for the input rows PG pulls, not a block's
    if d.functions.iter().any(|f| gravity_decompose::effects::of_call(gx.facts, f.funcid).acts) {
        px.declare_proto(gravity_program::protos::Proto::new(gravity_program::protos::ProtoKind::Volatile, k as u16));
    }
    let mut ents: PgVec<'_, Ent> = PgVec::new_in(mcx);
    let ahead = early_entries(px, k, j);
    let mut early: PgVec<'_, Early> = PgVec::new_in(mcx);
    for te in d.tlist.iter().filter_map(|n| n.as_target_entry()) {
        match d.functions.iter().position(|f| f.resno == te.resno) {
            Some(i) => ents.push_in(Ent::Set(i)),
            None if plain(te.expr) => {}
            None => {
                let ex = xm.compile_at(px, (k, j), te.expr, None, node);
                // D2021: a lazy SubPlan that acts (subplan/per_row.rs `order`) is read here like a VOLATILE call
                let volatile = px.placed(ex).volatile || acting_subplan(p, te.expr);
                let e = ahead.contains(&te.resno).then(|| {
                    // the cell is a word: an integral by-value entry (nextval's int8) and a bounded count of them
                    let typ = crate::step::project::out_of(gx, te.expr, node).typid;
                    if !EARLY_TYPES.contains(&typ) {
                        shape(node, EARLY_WIDE);
                    }
                    let w = |px: &mut Px<'g, '_>, i: usize| {
                        let Ok(i) = u8::try_from(EARLY_WORD + i) else { shape(node, EARLY_WIDE) };
                        px.frame_word(WordKey::Open { face: Face::Srf, k, j: crate::frame::px::key_j(j), i }) as i32 * 8
                    };
                    let (val, ok) = (w(px, 2 * early.len()), w(px, 2 * early.len() + 1));
                    early.push_in(Early { ex, val, ok, before: usize::MAX });
                    early.len() - 1
                });
                ents.push_in(Ent::Plain { ex, volatile: volatile || e.is_some(), early: e });
            }
        }
    }
    for (i, e) in ents.iter().enumerate() {
        if let Ent::Plain { early: Some(x), .. } = *e {
            early[x].before = ents[i + 1..].iter().position(|y| matches!(*y, Ent::Set(m) if members[m].args_act)).map_or(usize::MAX, |q| i + 1 + q);
        }
    }
    // PG runs a volatile entry's call, then the later function's raising open; this step opens every function first and
    // runs the volatile entries before a raising open only on its error path (`PSet::raise_open`), so nothing that can
    // raise or see the entry's effect may come between them
    for (i, e) in ents.iter().enumerate() {
        if !matches!(e, Ent::Plain { volatile: true, .. }) {
            continue;
        }
        let mut loud = false;
        for x in ents[i + 1..].iter() {
            match *x {
                Ent::Plain { volatile: false, .. } => loud = true,
                Ent::Set(m) if members[m].raises && (loud || !members[m].after_volatile) => {
                    shape(node, "a volatile target entry before a set-returning function whose open can raise");
                }
                _ => {}
            }
        }
    }
    mcx::pg_box_dyn!(px.mcx(), PSet { xm, node, offset, held, members, ents, early, cells } => dyn StepMech + 'g)
}

/// An early entry the step cannot hold: a volatile target entry before a set function whose arguments call a VOLATILE
/// function (PG evaluates the entry first: D2032), of a type the step's word cell cannot hold, or past the frame words'
/// index space.
const EARLY_WIDE: &str = "a volatile target entry before a set-returning function whose arguments call a VOLATILE function, of a type the step cannot hold (PG evaluates the entry first: D2032)";
/// The entry types an early cell holds as its word.
const EARLY_TYPES: [u32; 5] = [types_core::catalog::INT8OID, types_core::catalog::INT4OID, types_core::catalog::INT2OID, types_core::catalog::BOOLOID, types_core::catalog::OIDOID];
/// The early entries' frame words: `WordKey::Open` indices from `EARLY_WORD` (past the iterators' and the host sets'
/// `128 + i`), two per entry.
const EARLY_WORD: usize = 200;

/// D2032 (the user's 19:51Z ruling does not cover it: PG fixes this order under every plan): the resnos of ProjectSet op
/// `j`'s plain entries that act and come before a set function whose arguments act, in the target list's order. PG's first
/// call of the row evaluates the target list left to right, the entry and then those arguments; this step runs every
/// function's arguments at its open, so it runs such an entry itself, just before them, and at each later output in its set
/// loop, into cell `functions.len() + i` of its row. The Project reads the cell (step/project.rs `Entry::early`).
pub(crate) fn early_entries<'g>(px: &Px<'g, '_>, k: usize, j: usize) -> PgVec<'g, i16> {
    let mcx = px.mcx();
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let Some(Op::Srf(d)) = p.ops.get(j) else { return PgVec::new_in(mcx) };
    let ecx = effects::Cx::without_stmt(gx.facts, px.arena);
    // evaluating it acts: a VOLATILE call (the effects home, `DECISIONS.md` 3), or a SubPlan whose step acts
    let acting = |e| effects::of_expr(&ecx, e, &effects::PLAIN).acts || acting_subplan(p, e);
    let (mut out, mut pending): (PgVec<'g, i16>, PgVec<'_, i16>) = (PgVec::new_in(mcx), PgVec::new_in(mcx));
    for te in d.tlist.iter().filter_map(|n| n.as_target_entry()) {
        match d.functions.iter().find(|f| f.resno == te.resno) {
            Some(f) if f.args.iter().any(&acting) => out.extend_in(pending.drain(..)),
            Some(_) => {}
            None if !plain(te.expr) && acting(te.expr) => pending.push_in(te.resno),
            None => {}
        }
    }
    out
}

/// Whether `e` reads a SubPlan whose step acts (`SubPlanDesc::effects`): PG evaluates it again in the ending call.
fn acting_subplan(p: &gravity_decompose::PipelineNode<'_>, e: Node<'_>) -> bool {
    let mut hit = false;
    gravity_decompose::expr::walk(e, &mut |x| {
        hit |= x.as_sub_plan().is_some_and(|s| p.ops.iter().any(|o| matches!(o, Op::SubPlan(t) if t.plan_id == s.plan_id && t.effects().acts)));
        !hit
    });
    hit
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

impl<'g> PSet<'g> {
    /// PG's first call of the row, in the target list's order: the plain entries that can raise before a raising open, then
    /// each function's arguments and open; a host set's open is followed by its first step there (hb2: the function runs
    /// per step, so its first row's error comes before a later entry's), and the set loop does not step it at output 0.
    /// The iterators' opened words (`Iter::nwords` each), for `held`; a host set's first row goes to its cell and whether
    /// it ended to its `done` word.
    fn first_call<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals) -> PgVec<'e, Value> {
        let mut words = mcx::pg_vec![in env.mcx; env.zero; self.held.len()];
        let cw = self.cells.words(b, env);
        let (mut from, mut efrom) = (0, 0);
        for (i, e) in self.ents.iter().enumerate() {
            let Ent::Set(m) = *e else { continue };
            let mb = &self.members[m];
            if mb.raises {
                for e in self.ents[from..i].iter() {
                    if let Ent::Plain { ex, volatile: false, .. } = *e {
                        let _ = self.xm.emit(b, env, self.node, row, ex);
                    }
                }
                from = i;
            }
            // D2032: the entries that act before arguments that act run first, PG's left-to-right order
            if mb.args_act {
                for e in self.ents[efrom..i].iter() {
                    if let Ent::Plain { early: Some(x), .. } = *e {
                        let (v, valid) = self.xm.emit(b, env, self.node, row, self.early[x].ex);
                        self.hold(b, env, x, v, valid);
                    }
                }
                efrom = i;
            }
            let t = b.bconst(true);
            let mcx = env.mcx;
            let args: PgVec<'_, (Value, Option<Value>)> = mb.args.iter().map(|&a| {
                let (v, valid) = self.xm.emit(b, env, self.node, row, a);
                let null = b.const_of(valid).is_none_or(|c| c.1 == 0).then(|| b.int_bin(IntBinOp::Xor, valid, t));
                (v, null)
            }).collect_in(mcx);
            match (&mb.how, cw) {
                (How::Iter { it, at }, _) => {
                    let (o, err) = it.open(b, env, &args[..], false);
                    self.raise_open(b, env, row, i, err);
                    let w = o.words(mcx);
                    if w.len() != it.nwords() {
                        crate::refuse!(self.node, Reason::Bug { payload: "an opened set whose words are not its held words" });
                    }
                    words[*at..*at + w.len()].copy_from_slice(&w[..]);
                }
                (How::Cell { m, done }, Some(cw)) => {
                    let r = m.open(b, env, cw, &args[..]);
                    let f = fail_word(b, env, r);
                    self.raise_open(b, env, row, i, f);
                    let d = done_word(b, env, r);
                    let opened = b.icmp(IntCond::Eq, d, env.zero);
                    let (l_step, l_join) = (b.create_block(&[]), b.create_block(&[Type::I64]));
                    b.cond_br(opened, (l_step, &[]), (l_join, &[d]));
                    b.switch_to(l_step);
                    let r = m.step(b, env, cw);
                    let f = fail_word(b, env, r);
                    self.raise_open(b, env, row, i, f);
                    let d = done_word(b, env, r);
                    b.br(l_join, &[d]);
                    b.switch_to(l_join);
                    let d = b.block_params(l_join)[0];
                    env.store_word(b, *done, d);
                }
                (How::Cell { .. }, None) => crate::refuse!(self.node, Reason::Bug { payload: "a host set without its step's cells" }),
            }
        }
        words
    }

    /// Set entry `i`'s open (or a host set's first step) left with raise word `err`: PG had run the volatile entries before
    /// it (D2021: `nextval` or an acting SubPlan), so they run, left to right, before the error leaves.
    fn raise_open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, i: usize, err: Value) {
        let vols: PgVec<'_, ExId> = self.ents[..i].iter().filter_map(|e| match *e {
            // an early entry that ran before an earlier function's arguments (or this one's) does not run again
            Ent::Plain { ex, volatile: true, early } if early.is_none_or(|x| self.early[x].before > i) => Some(ex),
            _ => None,
        }).collect_in(env.mcx);
        if vols.is_empty() || b.const_of(err).is_some_and(|(_, c)| c == 0) {
            return raise(b, env, row, &row.carry, err);
        }
        let raised = b.icmp(IntCond::Ne, err, env.zero);
        let (l_err, l_ok) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(raised, (l_err, &[]), (l_ok, &[]));
        b.switch_to(l_err);
        for ex in vols {
            let _ = self.xm.emit(b, env, self.node, row, ex);
        }
        match env.l_raise_row {
            Some(l) => b.br(l, &[err, row.carry.out, row.next]),
            None => b.br(env.l_raise, &[err]),
        }
        b.switch_to(l_ok);
    }

    /// Early entry `x`'s value and validity into its frame words (D2032).
    fn hold(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: usize, v: Value, valid: Value) {
        if b.value_type(v) != Type::I64 {
            crate::refuse!(self.node, Reason::Bug { payload: "an early ProjectSet entry whose value is not a word" });
        }
        let w = b.conv(ConvOp::Zext, Type::I64, valid);
        env.store_word(b, self.early[x].val, v);
        env.store_word(b, self.early[x].ok, w);
    }

    /// Output `k`'s early entries into their cells of `rec` (D2032): output 0's, and a resumed output's (`again`), from
    /// their frame words; any other output evaluates them here, after the set functions stepped (PG's ProjectSet
    /// evaluates the plain entries per output row), and holds them for a resume inside it.
    fn early_cells(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, k: Value, again: Option<Value>, rec: Value) {
        let base = 16 * self.members.len() as i32;
        for (x, e) in self.early.iter().enumerate() {
            let first = b.icmp(IntCond::Eq, k, env.zero);
            let saved = again.map_or(first, |a| b.int_bin(IntBinOp::Or, first, a));
            let (l_eval, l_got) = (b.create_block(&[]), b.create_block(&[Type::I64, Type::I64]));
            let (v0, w0) = (env.load_word(b, e.val), env.load_word(b, e.ok));
            b.cond_br(saved, (l_got, &[v0, w0]), (l_eval, &[]));
            b.switch_to(l_eval);
            let memo = env.memo.set_aside();
            let (v, valid) = self.xm.emit(b, env, self.node, row, e.ex);
            env.memo.restore(memo);
            self.hold(b, env, x, v, valid);
            let w = b.conv(ConvOp::Zext, Type::I64, valid);
            b.br(l_got, &[v, w]);
            b.switch_to(l_got);
            let ps = b.block_params(l_got);
            let (p0, p1) = (ps[0], ps[1]);
            b.store(rec, base + 16 * x as i32, p0);
            b.store(rec, base + 16 * x as i32 + 8, p1);
        }
    }

    /// The call that finds every set ended (`k` rows went on): the volatile plain entries, and when `k` is 0 every plain
    /// entry that can raise, in the target list's order.
    fn ending_call(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, k: Value) {
        let eval = |b: &mut FunctionBuilder<'_>, env: &mut Env, all: bool| {
            for e in self.ents.iter() {
                if let Ent::Plain { ex, volatile, .. } = *e {
                    if all || volatile {
                        let _ = self.xm.emit(b, env, self.node, row, ex);
                    }
                }
            }
        };
        if !self.ents.iter().any(|e| matches!(e, Ent::Plain { volatile: false, .. })) {
            return eval(b, env, false);
        }
        let none = b.icmp(IntCond::Eq, k, env.zero);
        let (l_all, l_vol, l_join) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(none, (l_all, &[]), (l_vol, &[]));
        b.switch_to(l_all);
        eval(b, env, true);
        b.br(l_join, &[]);
        b.switch_to(l_vol);
        eval(b, env, false);
        b.br(l_join, &[]);
        b.switch_to(l_join);
    }
}

/// A host's status `r`: a raise word (above 1) leaves through the row's raise block; else the `done` word, 1 when the set
/// ended (0).
fn status(b: &mut FunctionBuilder<'_>, env: &Env, row: &RowVals, carry: &Carry, r: Value) -> Value {
    let err = fail_word(b, env, r);
    raise(b, env, row, carry, err);
    done_word(b, env, r)
}

/// A host's status `r` as a raise word: `r` when above 1, else 0.
fn fail_word(b: &mut FunctionBuilder<'_>, env: &Env, r: Value) -> Value {
    let failed = b.icmp(IntCond::Ugt, r, env.one);
    b.select(failed, r, env.zero)
}

/// A host's status `r` as the `done` word: 1 when the set ended (0).
fn done_word(b: &mut FunctionBuilder<'_>, env: &Env, r: Value) -> Value {
    let ended = b.icmp(IntCond::Eq, r, env.zero);
    b.conv(ConvOp::Zext, Type::I64, ended)
}

/// A nonzero raise word leaves through the row's raise block with `carry`'s rows counted (step/expr.rs `raise_if`).
fn raise(b: &mut FunctionBuilder<'_>, env: &Env, row: &RowVals, carry: &Carry, err: Value) {
    if b.const_of(err).is_some_and(|(_, c)| c == 0) {
        return;
    }
    let raised = b.icmp(IntCond::Ne, err, env.zero);
    let l_ok = b.create_block(&[]);
    match env.l_raise_row {
        Some(l) => b.cond_br(raised, (l, &[err, carry.out, row.next]), (l_ok, &[])),
        None => b.cond_br(raised, (env.l_raise, &[err]), (l_ok, &[])),
    }
    b.switch_to(l_ok);
}

impl<'g> StepMech for PSet<'g> {
    fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("srf.projectset offset", Base::Words, self.offset, Type::I64, 0));
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let saved = env.memo.set_aside();
        let held = stop::held(b, env, &self.held[..], Some(self.offset), &mut |b, env| self.first_call(b, env, row));
        env.memo.restore(saved);
        let opened: PgVec<'_, Option<Opened>> = self.members.iter().map(|m| match &m.how {
            How::Iter { it, at } => Some(it.from_words(&held[*at..*at + it.nwords()])),
            How::Cell { .. } => None,
        }).collect_in(env.mcx);
        // the resume point: past the output a sink stopped after, or at the one a later expanding step stopped inside
        // (`inside`: that output again, a host set's row still in its cell, not stepped)
        let off = env.load_word(b, self.offset);
        let later: PgVec<'_, i32> = env.expands.iter().filter(|e| env.step.is_some_and(|j| e.0 > j)).map(|e| e.1).collect_in(env.mcx);
        let (k0, inside) = match later.is_empty() {
            true => (off, None),
            false => {
                let set = later.iter().fold(env.zero, |a, &o| {
                    let w = env.load_word(b, o);
                    b.int_bin(IntBinOp::Or, a, w)
                });
                let set = b.icmp(IntCond::Ne, set, env.zero);
                let resumed = b.icmp(IntCond::Ne, off, env.zero);
                let inside = b.int_bin(IntBinOp::And, set, resumed);
                let back = b.int_bin(IntBinOp::Sub, off, env.one);
                (b.select(inside, back, off), Some(inside))
            }
        };
        let cw = self.cells.words(b, env);
        // the set loop: (k, every iterator's cursor, the carry); the header polls cancel
        let curs: PgVec<'_, PgVec<'_, Value>> = self.members.iter().zip(opened.iter()).map(|(m, o)| match (&m.how, o) {
            (How::Iter { it, .. }, Some(o)) => it.at(b, o, k0, env.mcx),
            _ => PgVec::new_in(env.mcx),
        }).collect_in(env.mcx);
        let ncur: PgVec<'_, usize> = curs.iter().map(|c| c.len()).collect_in(env.mcx);
        let init: PgVec<'_, Value> = core::iter::once(k0).chain(curs.iter().flatten().copied()).chain(row.carry.params()).collect_in(env.mcx);
        let types: PgVec<'_, Type> = init.iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let (l_head, l_body) = cancel::header(b, env.ctx, env.l_cancel, &types[..], env.mcx);
        let mut etypes: PgVec<'_, Type> = mcx::pg_vec![in env.mcx; Type::I64];
        etypes.extend_in(row.carry.params().iter().map(|&v| b.value_type(v)));
        let l_end = b.create_block(&etypes[..]);
        // D2967: the region's top before the outputs; each output's by-reference bytes go back to it once the output is
        // through (PG's per-tuple reset per ProjectSet output; what a later step or the end keeps it copies, as across
        // windows), so a set of any length runs in one output's bytes
        let mark = env.hir.mark(b);
        b.br(l_head, &init[..]);
        b.switch_to(l_body);
        let ps = mcx::to_vec_in(&b.block_params(l_body)[..], env.mcx);
        let k = ps[0];
        let mut at = 1;
        let curs: PgVec<'_, &[Value]> = ncur.iter().map(|&n| {
            let c = &ps[at..at + n];
            at += n;
            c
        }).collect_in(env.mcx);
        let carry = Carry::from_params(&ps[at..], env.mcx);
        // this call's first output after a stop inside it: a host set's row is already in its cell
        let again = inside.map(|x| {
            let first = b.icmp(IntCond::Eq, k, k0);
            b.int_bin(IntBinOp::And, x, first)
        });
        let slot = b.declare_stack_slot(16 * (self.members.len() + self.early.len()).max(1) as u32, 8);
        let rec = b.stack_addr(slot);
        let mut any = b.bconst(false);
        let t = b.bconst(true);
        for (i, (m, o)) in self.members.iter().zip(opened.iter()).enumerate() {
            let (yields, w, null) = match (&m.how, o, cw) {
                (How::Iter { it, .. }, Some(o), _) => {
                    let yields = b.icmp(IntCond::Ult, k, o.n);
                    let (w, null, err) = it.value(b, env, o, curs[i]);
                    if b.const_of(err).is_none_or(|(_, c)| c != 0) {
                        let err = b.select(yields, err, env.zero);
                        raise(b, env, row, &carry, err);
                    }
                    (yields, w, null)
                }
                (How::Cell { m, done }, _, Some(cw)) => {
                    // an ended set is not called again (PG's ExprEndResult); output 0's row came with the first call; the
                    // output a later step stopped inside is not stepped twice
                    let d0 = env.load_word(b, *done);
                    let skip = b.icmp(IntCond::Ne, d0, env.zero);
                    let first = b.icmp(IntCond::Eq, k, env.zero);
                    let skip = b.int_bin(IntBinOp::Or, skip, first);
                    let skip = again.map_or(skip, |a| b.int_bin(IntBinOp::Or, skip, a));
                    let (l_step, l_got) = (b.create_block(&[]), b.create_block(&[Type::I64]));
                    b.cond_br(skip, (l_got, &[d0]), (l_step, &[]));
                    b.switch_to(l_step);
                    let r = m.step(b, env, cw);
                    let d = status(b, env, row, &carry, r);
                    env.store_word(b, *done, d);
                    b.br(l_got, &[d]);
                    b.switch_to(l_got);
                    let d = b.block_params(l_got)[0];
                    let yields = b.icmp(IntCond::Eq, d, env.zero);
                    let (w, null) = m.value(b, env, cw);
                    (yields, w, null)
                }
                _ => crate::refuse!(self.node, Reason::Bug { payload: "a projectset member without its opened set or cells" }),
            };
            let not_null = b.int_bin(IntBinOp::Xor, null, t);
            let valid = b.int_bin(IntBinOp::And, yields, not_null);
            b.store(rec, 16 * i as i32, w);
            let vw = b.conv(ConvOp::Zext, Type::I64, valid);
            b.store(rec, 16 * i as i32 + 8, vw);
            any = b.int_bin(IntBinOp::Or, any, yields);
        }
        let l_row = b.create_block(&[]);
        let ended: PgVec<'_, Value> = core::iter::once(k).chain(carry.params()).collect_in(env.mcx);
        b.cond_br(any, (l_row, &[]), (l_end, &ended[..]));
        b.switch_to(l_row);
        if !self.early.is_empty() {
            let lrow = RowVals { row: row.row.clone(), vals: row.vals.clone(), cols: row.cols.clone(), carry: carry.copy_in(env.mcx), next: row.next, at: row.at };
            self.early_cells(b, env, &lrow, k, again, rec);
        }
        let k1 = b.int_bin(IntBinOp::Add, k, env.one);
        env.store_word(b, self.offset, k1);
        let cells = (0..(self.members.len() + self.early.len()) as u32).map(|i| AggCell::word(16 * i, CellValid::NonZeroAt(16 * i + 8))).collect_in(env.mcx);
        let outer = env.srfs.replace(AggRow { rec, cells });
        let out = RowVals { row: row.row.clone(), vals: row.vals.clone(), cols: row.cols.clone(), carry, next: row.at, at: row.at };
        let memo = env.memo.set_aside();
        let seen = env.hir.seen();
        let c2 = next(b, env, &out);
        env.memo.restore(memo);
        env.srfs = outer;
        if let Some(m) = mark {
            env.hir.give_back(b, m);
        }
        // D3583: and the function-manager windows of the sites the output's later steps called (not the held plain entries')
        env.hir.reset_seen(b, seen, env.mcx);
        let mut back: PgVec<'_, Value> = mcx::pg_vec![in env.mcx; k1];
        for ((m, o), c) in self.members.iter().zip(opened.iter()).zip(curs.iter()) {
            if let (How::Iter { it, .. }, Some(o)) = (&m.how, o) {
                back.extend_in(it.advance(b, env, o, c));
            }
        }
        back.extend_in(c2.params());
        b.br(l_head, &back[..]);
        // the ending call, then the row is through: the offset word back to 0
        b.switch_to(l_end);
        let (k_end, ce) = {
            let ep = b.block_params(l_end);
            (ep[0], Carry::from_params(&ep[1..], env.mcx))
        };
        let end_row = RowVals { row: row.row.clone(), vals: row.vals.clone(), cols: row.cols.clone(), carry: ce.copy_in(env.mcx), next: row.next, at: row.at };
        let memo = env.memo.set_aside();
        self.ending_call(b, env, &end_row, k_end);
        env.memo.restore(memo);
        env.store_word(b, self.offset, env.zero);
        ce
    }

    fn expand(&self) -> Option<Expand> {
        Some(Expand { kind: ExpandKind::Srf, offset: self.offset, held: self.held.len() as u8 })
    }
}
