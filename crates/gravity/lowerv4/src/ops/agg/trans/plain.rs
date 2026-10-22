//! Plain aggregate end. Hooks / levers: -. Budget 250 lines (files.tsv, +25 % gate). Owner M1-FOLD.
//!
//! Registers: kind `fold.plain` (`KindId::FoldPlain`): the end folding a PlainAgg breaker's one record (per row each
//! aggregate's class steps its state, in the carry under frame.carry and folded into the record at the window's end, else
//! in the record's slot), and the drain source emitting its finals (every final before HAVING, as nodeAgg). The helpers
//! every fold end reads (an aggregate's class and arguments: args.rs; a record slot's validity: slots.rs) are re-exported
//! here, their path for the keyed ends.
use core::cell::Cell as Once;

use gravity_abi::breaker::RecordLayout;
use gravity_decompose::ops::shared::agg::{AggDecl, Register, NONE, NULL_ARM};
use gravity_decompose::graph::Op;
use gravity_program::Pipeline;
use gravity_expr::emit::CellValid;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, IntBinOp, IntCond, Type, Value};
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};

use crate::face::end::EndMech;
use crate::face::fold::{Acc, FoldClass};
use crate::face::kinds::KindId;
use crate::face::strategy::{Strategy, FRAME_CARRY, MASK_FOLD, NUM_MEMO, NUM_WINDOW};
use crate::frame::env::Env;
use crate::frame::frame::{EndMaker, Mask, Win, WordWin};
use crate::frame::px::{ExId, Px};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::admit::refusal::{Reason, Variant};

pub(crate) use crate::ops::agg::trans::args::{args_of, args_ok, n_of, weighted, with_args, Arg};
pub(crate) use crate::ops::agg::trans::class_of::class_of;
use crate::props::class_kind;
pub(crate) use crate::ops::agg::trans::slots::host_final;
use crate::ops::agg::trans::hosted::{arena_word, hosted, hosted_kind, hosted_slot};
use crate::ops::agg::trans::slots::{layout_of, record, sentinel};
use crate::ops::shared::slots::slot_valid;

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::FoldPlain, &EndMaker(make))], ..crate::ops::NONE };

/// The fold of one aggregate: its class, arguments, FILTER, carry words and record slot.
pub(crate) struct Fold<'g, 'p> {
    pub a: &'g AggDecl<'p>,
    pub class: &'static dyn FoldClass,
    pub args: PgVec<'g, Arg>,
    pub filter: Option<ExId>,
    pub at: usize,
    pub words: usize,
    pub off: i32,
    pub valid: CellValid,
    /// A column argument's scan attno (its window lane), through the pipeline's projections.
    pub scan: Option<i16>,
    /// num.window: a `class.num_packed` argument folded as a mantissa tree (`args` then empty); V4R-38: a class.num_wide
    /// one as its wide tree (`wide`, num_tree `compile_wide`), folded as an i256 term.
    pub num: Option<crate::ops::agg::numeric::strategies::num_tree::Tree<'g>>,
    pub wide: bool,
    /// V4R-424: a hosted aggregate riding a transition state an earlier one opened (hosted.rs `rides`): no per-row step.
    pub rides: bool,
    /// PL-20: a `MIN / MAX(f(x, c))` an answered window folds from x's record (strategies/monotone.rs), its constants'
    /// words placed before the argument's compile reads them (D2958: the occurrence's words, `Px::root_const`).
    pub mono: Option<crate::ops::agg::trans::strategies::monotone::Bound>,
    /// aggregates#251: its ORDER BY entries evaluated per row and dropped (args.rs `discards`); none of the end's fast forms.
    pub discard: PgVec<'g, ExId>,
}

pub(crate) struct PlainEnd<'g, 'p> {
    pub folds: PgVec<'g, Fold<'g, 'p>>,
    layout: RecordLayout,
    carry: bool,
    node: i32,
    rec: Once<Option<Value>>,
    /// Each fold's window guard of the current window (class_num lever a), set by `carried`.
    wins: PgVec<'g, Once<Option<Value>>>,
    pub answer: Option<crate::ops::agg::trans::answer::Plan<'g>>,
    memo: bool,
    /// The aggregate arena's word when a fold is hosted or a column kept (`Env::agg_arena`).
    arena: Option<u16>,
    /// D345: the pinned columns kept from the group's first row (fold/kept.rs).
    kept: PgVec<'g, crate::ops::agg::trans::kept::Kept>,
    /// D3028: a fold evaluates an expression per row (an argument, FILTER or dropped ORDER BY entry), so `lower` gives the
    /// row's result-region bytes back past its folds (PostgreSQL's per-tuple reset): every state is words or the host's
    /// copy in the aggregate context (a hosted transition, a kept column), so none reads them past the row.
    reclaim: bool,
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let gx = px.gx;
    let mcx = px.mcx();
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let (Some(g), Some(_)) = (gx.group_at(k), crate::frame::iface::breaker_of(&p.desc)) else { crate::refuse!(node, Reason::Bug { payload: "a plain fold end without its group" }) };
    if let Err(why) = crate::ops::group::regroup::call_order(px, g, (k, p.ops.len()), &|n| crate::effects::raise_free(gx, n)) {
        crate::refuse!(node, Reason::Shape { why: crate::admit::refusal::ShapeWhy(why) }); // D1371: PG's transfn / finalfn order
    }
    let layout = layout_of(px, g);
    let kept = crate::ops::agg::trans::kept::of(g, &layout, node, mcx); // D345: the first row's pinned columns
    let in_order = crate::props::fed_in_order(px.gx.graph, g) || fed(px, g);
    crate::ops::agg::trans::kept::refuse_unfed(g, in_order, node);
    let carry = carry_head(px, k, g);
    let j = p.ops.len();
    let mut folds = mcx::reserved_in(g.aggs.len(), mcx);
    let mut at = 0;
    let mut whens_of: PgVec<'_, (types_nodes::Node<'_>, ExId)> = PgVec::new_in(mcx);
    // an unbuilt class refuses by its own name first; then DISTINCT past distinct_plain's pair fold is the pair set's
    // (M3 keyed.distinct_grouped), and an ORDER BY inside the call is the sorted aggregate's (M3 runs.group; main 09-24,
    // D345 / D346): the end is handed to that kind's maker before anything compiles (M3 L0-c; NotBuilt by name at M1)
    for a in g.aggs.iter() {
        crate::ops::agg::trans::class_of::class_in(&node, a, true, mcx);
        // D1210: a host-sorted DISTINCT folds here beside riders that read no row order (else keyed.distinct_multi names why)
        let sorted = crate::ops::shared::agg::sorted(a); // its ORDER BY is its DISTINCT's own sort
        let distinct = match (sorted, a.mods.distinct) {
            (true, _) if crate::ops::group::regroup::riders_free(px.arena, px.gx.graph, g) || fed(px, g) => (0, KindId::KeyedDistinctMulti),
            (true, 0) => (1, KindId::RunsGroup), // D458: an ORDER BY beside a rider reading the row order: runs.group names it
            (true, n) => (n, KindId::KeyedDistinctMulti),
            (false, n) => (n, KindId::KeyedDistinctGrouped),
        };
        let order = if sorted || !hosted_kind(a) && crate::effects::visible::agg::order_free(a, true) { 0 } else { crate::ops::agg::trans::args::ordered(a) };
        let to = [distinct, (order, KindId::RunsGroup)].into_iter().find(|m| m.0 != 0);
        if let Some((_, to)) = to {
            crate::ops::agg::trans::kept::refuse_handoff(g, node); // D345: the other end keeps no first row
            crate::mechs_used::declined(crate::admit::refusal::Variant::Kind(KindId::FoldPlain)); // MECHCORPUS: the node is another kind's form
            return (crate::frame::iface::kind_impl::<EndMaker>(&node, to).0)(px, k);
        }
    }
    for a in g.aggs.iter() {
        let class = crate::ops::agg::trans::class_of::class_in(&node, a, true, mcx);
        crate::ops::agg::numeric::num_datum::guard(gx, node, class); // V4R-549: main 02:53Z's no-widen list over DATUM_DS
        if a.slot == gravity_decompose::ops::shared::agg::NONE && !hosted(a) {
            crate::refuse!(node, Reason::Bug { payload: "a plain fold over a slotless aggregate of a built class" });
        }
        let slot = if hosted(a) { hosted_slot(&layout, g, a) } else { a.slot };
        let wide = class_kind(a) == KindId::ClassNumWide;
        let discard = crate::ops::agg::trans::args::discards(px, k, j, a, node);
        let tree = match class_kind(a) {
            _ if !discard.is_empty() => None,
            KindId::ClassNumPacked => of_agg(a, mcx),
            KindId::ClassNumWide => crate::ops::agg::numeric::strategies::num_keyed::of_wide(a, mcx),
            _ => None,
        };
        let num = road_on(px, NUM_WINDOW).then_some(tree).flatten().map(|(mut t, whens)| {
            // an equal WHEN compiles once per end (one percode table lane: two of one key in a bind waited on each other)
            let mut id = |w| match whens_of.iter().find(|x| types_nodes::equal(x.0, w)) {
                Some(x) => x.1,
                None => {
                    let id = crate::step::expr::compile_at(px, k, j, w, None, node);
                    whens_of.push_in((w, id));
                    id
                }
            };
            let ids: PgVec<'_, ExId> = whens.into_iter().map(&mut id).collect_in(mcx);
            crate::ops::agg::numeric::strategies::num_tree::bind(&mut t, &ids);
            t
        });
        let mono = if num.is_some() || !discard.is_empty() { None } else { crate::ops::agg::trans::strategies::monotone::bind(px, a) };
        let args = if num.is_some() { PgVec::new_in(mcx) } else { args_of(px, k, j, a, node) };
        let filter = crate::ops::agg::trans::filter::compile(px, k, j, a, node);
        let words = class.shape(a).words as usize;
        let scan = match args.as_slice() {
            [Arg::Col(c)] => crate::ops::agg::trans::block_fold::scan_attno(px, k, *c),
            _ => None,
        };
        folds.push_in(Fold { a, class, args, filter, at, words, off: layout.slot_off(slot as usize) as i32, valid: slot_valid(&layout, slot), scan, num, wide, rides: crate::ops::agg::trans::hosted::rides(g, a), mono, discard });
        at += words;
    }
    let every_row = folds.iter().any(|f| !f.discard.is_empty()); // aggregates#251: each row's entries run, no answered window
    let answer = if every_row { None } else { crate::ops::agg::trans::answer::plan(px, k, &folds) };
    let memo = road_on(px, NUM_MEMO);
    let wins = folds.iter().map(|_| Once::new(None)).collect_in(mcx);
    let arena = (g.aggs.iter().any(|a| hosted(a)) || !kept.is_empty()).then(|| arena_word(px, k, g));
    let carry = carry && carry_tail(g, every_row, folds.iter().any(|f| f.class.record_only(f.a)));
    let reclaim = folds.iter().any(|f| f.filter.is_some() || !f.discard.is_empty() || f.args.iter().any(|a| matches!(a, Arg::Ex(..))));
    mcx::pg_box_dyn!(px.mcx(), PlainEnd { folds, layout, carry, node, rec: Once::new(None), wins, answer, memo, arena, kept, reclaim } => dyn EndMech + 'g)
}

/// D1596: plain group `g` beside a rider reading the row order folds each aggpresorted DISTINCT per input row, in PG's order
/// (src/regroup_presorted.rs `presorted`; every slotless aggregate hosted): the end steps them per row in aggno order, and the arena
/// (fold/hosted.rs `arena_word`'s PRESORTED bit) tells the host.
pub(crate) fn fed(px: &Px<'_, '_>, g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>) -> bool {
    let hosted_all = g.aggs.iter().all(|a| a.slot != NONE || hosted(a));
    g.form == gravity_decompose::ops::shared::agg::GroupForm::Plain && hosted_all && !crate::ops::group::regroup::riders_free(px.arena, px.gx.graph, g) && crate::ops::group::regroup::presorted(px.gx.graph, px.gx.facts, g).is_ok()
}

fn road_on(px: &Px<'_, '_>, r: Strategy) -> bool {
    px.strategies.has(r) && crate::face::source::on(crate::admit::refusal::Variant::Strategy(r))
}

/// `a`'s argument as a num.window tree at its scale with its WHEN nodes, when a tree takes it (a sum / avg: min / max
/// keep the mantissa word with its sentinels); a CASE whose arms differ in scale matches decompose's dscales (D256).
pub(crate) fn of_agg<'g, 'p>(a: &AggDecl<'p>, mcx: Mcx<'g>) -> Option<(crate::ops::agg::numeric::strategies::num_tree::Tree<'g>, PgVec<'g, types_nodes::Node<'p>>)> {
    // D1320: a host-sorted DISTINCT (src/regroup.rs `sorted`) folds its inputs in the host, never in a tree: the tree
    // wrote a packed sum into the hosted slot, which the final then read as the DISTINCT run
    if crate::ops::shared::agg::sorted(a) {
        return None;
    }
    let r = a.node.as_aggref().filter(|r| r.args.len() == 1 && matches!(a.reg, Register::SumNum | Register::AvgNum))?;
    let e = r.args.iter().next().map(|t| t.as_target_entry().map_or(t, |t| t.expr))?;
    let mut whens = PgVec::new_in(mcx);
    let t = crate::ops::agg::numeric::strategies::num_tree::compile(e, a.scale, &mut whens, mcx)?;
    let arm = |t: &Option<crate::ops::agg::numeric::strategies::num_tree::Tree<'_>>| t.as_ref().map_or(NULL_ARM, |t| crate::ops::agg::numeric::strategies::num_tree::scale_of(t) as u8);
    let agree = match (&t, a.dscale_slot != NONE) {
        (crate::ops::agg::numeric::strategies::num_tree::Tree::Case(c), true) => c.arms.len() <= a.dscales.len() && c.arms.iter().zip(a.dscales.iter().copied()).all(|(x, d)| arm(&x.1) == d),
        (crate::ops::agg::numeric::strategies::num_tree::Tree::Case(c), false) => !c.mixed,
        // D4567 (a): a division's display scale is its dscale word's (decompose agg numeric.rs `div_form`)
        (crate::ops::agg::numeric::strategies::num_tree::Tree::Div { .. }, ds) => ds,
        (_, ds) => !ds,
    };
    agree.then_some((t, whens))
}

/// A carried sum's window guard: rows x 10^digits keep the carried sum inside +-2^125, so no row's add can
/// leave D299's range and the check waits for the window's merge (add_sum). Typmod digits alone; `None` past them.
fn window_guard(b: &mut FunctionBuilder<'_>, env: &Env, digits: u32) -> Option<Value> {
    let lim = 10u128.checked_pow(digits).map_or(0, |p| (1u128 << 125) / p).min(i64::MAX as u128) as i64;
    let w = env.win.as_ref().filter(|_| lim > 0)?;
    let (n, l) = (b.int_bin(IntBinOp::Sub, w.end, w.cursor), b.iconst(Type::I64, lim));
    Some(b.icmp(IntCond::Ule, n, l))
}

impl PlainEnd<'_, '_> {
    fn rec(&self) -> Value {
        self.rec.get().unwrap_or_else(|| crate::refuse!(self.node, Reason::Bug { payload: "the plain record before its window" }))
    }
}

impl EndMech for PlainEnd<'_, '_> {
    fn carried<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>) -> PgVec<'e, Value> {
        self.rec.set(Some(record(b, env, &self.layout)));
        env.agg_arena = self.arena;
        let mut w = mcx::pg_vec![in env.mcx; env.zero; if self.carry { self.folds.iter().map(|f| f.words).sum() } else { 0 }];
        for (f, g) in self.folds.iter().zip(&self.wins).filter(|_| self.carry) {
            f.class.init(b, env, f.a, Acc::Regs(&mut w[f.at..f.at + f.words]));
            g.set(f.num.as_ref().filter(|_| !f.wide).and_then(|t| window_guard(b, env, crate::ops::agg::numeric::strategies::num_tree::digits_of(t))));
        }
        w
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        let mut accs = mcx::to_vec_in(&row.carry.accs[..], env.mcx);
        let rec = self.rec();
        let mark = if self.reclaim { env.hir.mark(b) } else { None };
        let mut memo = crate::ops::agg::numeric::strategies::memo_num::Memo::new(self.memo, env.mcx);
        // D3233 (road case.select): a fold that branches on its CASE's selection reads its tree inside the branch
        let cs = env.applies(crate::face::strategy::CASE_SELECT);
        let selects = |f: &Fold<'_, '_>| f.num.as_ref().is_some_and(|t| crate::ops::agg::numeric::strategies::num_case::selects(t, cs));
        memo.cover(self.folds.iter().filter(|f| f.filter.is_none() && !selects(f)).filter_map(|f| f.num.as_ref()));
        for (f, g) in self.folds.iter().zip(&self.wins).filter(|(f, _)| !f.rides) {
            let (mark, g) = (memo.len(), g.get());
            let memo = &mut memo;
            let step = |b: &mut FunctionBuilder<'_>, env: &mut Env, accs: &mut [Value]| {
                // a hosted state is never carried: its slot is the group's one state (face/aggcall.rs); nor is a keyed
                // numeric extreme (N13): a value past its key folds into the record's side copy, which a carry cannot hold
                let acc = match self.carry && !hosted(f.a) && !crate::ops::shared::agg::order_keyed(f.a) {
                    true => Acc::Regs(&mut accs[f.at..f.at + f.words]),
                    false => Acc::Slot { base: rec, off: f.off, valid: f.valid },
                };
                for &id in &f.discard {
                    crate::step::expr::emit(b, env, self.node, row, id);
                }
                match &f.num {
                    Some(t) => {
                        let mcx = memo.mcx;
                        match crate::ops::agg::numeric::strategies::num_case::select(b, env, row, t, mcx) {
                            None => crate::ops::agg::numeric::strategies::num_keyed::fold_term(b, env, row, f.a, f.class, t, f.wide, memo, acc, g),
                            Some((at, sel, on)) => {
                                memo.sel = Some((at, sel));
                                crate::ops::agg::trans::filter::acc_when(b, env, on, acc, |b, env, acc| crate::ops::agg::numeric::strategies::num_keyed::fold_term(b, env, row, f.a, f.class, t, f.wide, &mut *memo, acc, g));
                                memo.sel = None;
                            }
                        }
                    }
                    None => with_args(b, env, self.node, row, &f.args, |b, env, args| f.class.step(b, env, f.a, acc, args)),
                }
            };
            crate::ops::agg::trans::filter::step_if(b, env, self.node, row, f.filter, &mut accs, step);
            if f.filter.is_some() || selects(f) {
                memo.truncate(mark);
            }
        }
        crate::ops::agg::trans::kept::step(b, env, self.node, row, rec, &self.kept);
        if let Some(m) = mark {
            env.hir.give_back(b, m);
            env.memo.clear();
        }
        let out = b.int_bin(IntBinOp::Add, row.carry.out, env.one);
        if !self.carry {
            self.sentinel(b, env, env.one);
        }
        Carry { out, accs }
    }

    fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, c: &Carry) {
        if !self.carry {
            return;
        }
        let rec = self.rec();
        let mut accs = mcx::to_vec_in(&c.accs[..], env.mcx);
        for f in self.folds.iter().filter(|f| !crate::ops::shared::agg::order_keyed(f.a)) {
            f.class.merge(b, env, f.a, Acc::Slot { base: rec, off: f.off, valid: f.valid }, Acc::Regs(&mut accs[f.at..f.at + f.words]));
        }
        self.sentinel(b, env, c.out);
    }

    fn whole(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, win: &Win, c: &Carry, l_wend: Block) -> bool {
        self.carry && crate::ops::agg::trans::answer::whole(b, env, self, win, c, l_wend)
    }

    fn block<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, w: &WordWin, base: Value, m: Mask, c: &Carry) -> Option<Carry<'e>> {
        crate::ops::agg::trans::block_fold::end_block(b, env, &self.folds, self.carry, w, base, m, c)
    }

    fn counts(&self, env: &Env) -> bool {
        self.carry && env.applies(MASK_FOLD) && self.folds.iter().all(|f| f.a.reg == Register::CountStar && f.filter.is_none())
    }

    /// `lower` adds one per row, `block` the mask's popcount (block_fold.rs), `whole` the window's rows (answer.rs).
    fn out_counts_rows(&self) -> bool {
        true
    }

    fn takes_mult(&self) -> bool {
        self.folds.iter().all(|f| weighted(f.a) && f.filter.is_none())
    }

    fn pipeline_desc(&self, _px: &Px<'_, '_>, _k: usize, d: Pipeline) -> Pipeline {
        crate::ops::agg::trans::hosted::region(self.node, &self.layout, d)
    }
}

impl PlainEnd<'_, '_> {
    fn sentinel(&self, b: &mut FunctionBuilder<'_>, env: &Env, n: Value) {
        sentinel(b, env, &self.layout, self.rec(), n);
    }
}

/// The plain end carries its accumulators per window, so it takes a window whole (`EndMech::whole`), only under the
/// frame.carry road. A source answers a window (decodes no lane of it) only then: answer_site.rs `declare_site`.
pub(crate) fn carries(px: &Px<'_, '_>) -> bool {
    px.strategies.has(FRAME_CARRY) && crate::face::source::on(crate::admit::refusal::Variant::Strategy(FRAME_CARRY))
}

/// D4703: whether pipeline `k`'s plain end over group `g` carries (`make`'s `carry`, `carry_head` then `carry_tail`), from
/// the group alone. A source leaves a window's lanes undecoded (answered, or counted covered) only for an end that
/// carries, which takes the window whole: one that walks its rows reads the lanes left null (D301's SIGSEGV).
pub(crate) fn carries_group(px: &Px<'_, '_>, k: usize, g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>) -> bool {
    let node = px.gx.pipelines()[k].node_id;
    let every_row = g.aggs.iter().any(|a| crate::ops::agg::trans::args::discarding(a));
    carry_head(px, k, g) && carry_tail(g, every_row, g.aggs.iter().any(|a| crate::ops::agg::trans::class_of::class_in(&node, a, true, px.mcx()).record_only(a)))
}

/// D853: the window carry only when no aggregate reads the row order, and only where it rides the pipeline's loops.
fn carry_head(px: &Px<'_, '_>, k: usize, g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>) -> bool {
    carries(px) && crate::graph::order_free(g) && crate::ops::shared::agg::carry_rides(px, k, crate::ops::shared::agg::FOLD_RATE)
}

/// D862: min / max are order-free now (the equal-member ruling); the carry, like the reverse page walk, still keeps the
/// member PG's forward scan meets first (`walk_free`). `every_row`: an aggregate's ORDER BY entries run per row
/// (aggregates#251). `record_only`: a record-only class (`FoldClass::record_only`) beside a hosted fold steps into the
/// record: carried across that fold's per-row host call, its words would spill around the call at every row
/// (agg_typed-0058's two interval sums + money).
fn carry_tail(g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>, every_row: bool, record_only: bool) -> bool {
    crate::graph::walk_free(g) && !every_row && !(record_only && g.aggs.iter().any(|a| hosted(a)))
}

/// Pipeline `k`'s ops before its end keep every row (projections only): the end sees each source row. A count(*)-only
/// fold then counts a window whole and never runs the projections, which skips evaluations PostgreSQL makes: only
/// projections with no effects take that road (D2101: `SELECT count(*) FROM (SELECT nextval(..) FROM t OFFSET 0) s` made
/// no call; a raising projection would lose its error). Otherwise the rows run through the projections, PG's form.
pub(crate) fn rows_kept(px: &Px<'_, '_>, k: usize) -> bool {
    use gravity_decompose::effects::{self, Departure, Effects};
    let ops = &px.gx.pipelines()[k].ops;
    if !ops.iter().all(|o| matches!(o, Op::Project(_))) {
        return false;
    }
    // no SubPlan's plan is reachable from here: a SubPlan in a projection is unknown, so it acts (fail closed)
    let cx = effects::Cx::without_stmt(px.gx.facts, px.arena);
    let mut e = effects::of_ops(&cx, ops, &effects::PLAIN);
    // a VALUES source's rows are evaluated by the source: their calls are the pipeline's too (vo-0130)
    if let gravity_decompose::Source::Values { lists, .. } = &px.gx.pipelines()[k].source {
        // D2907: a list streaming past the coded cells' cost runs its rows (PostgreSQL's form), its cells never walked
        // (D4449: a list of Consts alone never streams, `consts_only`)
        if !crate::ops::shared::values::consts_only(px.gx.graph, lists) && crate::ops::shared::values::streams_past_cut(lists) {
            return false;
        }
        for row in lists.iter() {
            e = e.and(row.as_list().map_or(Effects::UNKNOWN, |l| effects::of_list(&cx, l, &effects::PLAIN)));
        }
    }
    e == Effects::NONE || effects::licence(e, Departure::Skip).is_ok()
}
