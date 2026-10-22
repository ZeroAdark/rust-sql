//! D1120 (V4R-384): a streamed sorted group's DISTINCT aggregate, keyed/stream.rs's step over a `GroupForm::Distinct` group
//! (decompose streams a sorted group whose aggregate argument or FILTER calls something VOLATILE, D1071). PG 18.6's
//! GroupAggregate evaluates each aggregate's FILTER, then its arguments, once per row in transition order (aggno); a
//! DISTINCT aggregate's argument goes into the group's sort, which at the group's end drops equal values (NULL = NULL)
//! and hands each distinct value to the transition function once (process_ordered_aggregate_single). The next group's
//! first row is fetched but never advanced before the group is returned, so a LIMIT above stops the calls there.
//! The stream keeps the one open group as keyed.emit_group's record for a grouped DISTINCT (keyed/pairs_view.rs: the group
//! keys' words, `[sentinel, pairs]`, the riders from `ACC_SLOT_BASE`, one slot per DISTINCT fold), so its hand-on is the
//! emit's. Per row: the GROUP keys alone are imaged (keyed/stream.rs hands the open group on before anything else of
//! the row is read), then the aggregates fold in aggno order: a rider into its slot; at the first DISTINCT aggregate the
//! pair FILTER, then the argument evaluated once, and a non-NULL value offered to the group's distinct set (kind
//! runs.group_distinct's, `kind_impl::<DistinctSetImpl>`): a new one adds one pair and steps each DISTINCT fold. The
//! folds admitted are exact over an integer argument (count, sum, avg, min, max), so first-seen order gives PG's result.
//! RUNS-33: where PG presorted the calls (`sorted`), equal arguments arrive adjacent and the set keeps the last one alone.
//! A group the pair set cannot hold (a DISTINCT fold over another argument, which PG folds in sort order, a second
//! DISTINCT argument, a hosted rider) regroup.rs re-declares host-sorted (G10) when it can; otherwise refused by name
//! (keyed.distinct_multi, NotBuilt M6: `unpaired`), as are a rider that reads the row order and an argument whose image
//! is not its value.
//! Owner M6-AGG. Budget 250 lines.
use gravity_abi::breaker::{KeyKind, SlotKind, ACC_PAIRS};
use gravity_decompose::ops::shared::agg::fold_of;
use gravity_decompose::facts::order::{default_order, presorted_call};
use gravity_decompose::ops::shared::agg::{AggDecl, FinalForm, GroupDecl, GroupForm};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, Type, Value};
use mcx::{Mcx, PgVec};
use mcx::GrowIn;

use crate::table::keyimg;
use crate::table::keyimg::{ImageWords, KeyCell};
use crate::face::kinds::KindId;
use crate::face::runs::DistinctSetMech;
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::frame::row::RowVals;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::rep::{CellRef, Scope};
use mcx::CollectIn;

use crate::ops::agg::hashagg::{AccAt, Arg, GroupRec, KeyVal};

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

/// Any streamed pair-form group (keyed/stream.rs, before its record): a DISTINCT aggregate with no pair slot (a second
/// DISTINCT argument, or a fold the pair set does not take: a float or numeric one, which PG folds in sort order) refuses by
/// keyed.distinct_multi's name, as the keyed form's does (keyed/distinct_plain.rs; D536), never a record without its slot.
/// A group regroup.rs re-declared (G10: sorted, no pair set) folds them host-sorted.
pub(crate) fn unpaired(g: &GroupDecl<'_>) {
    if g.form == GroupForm::Distinct && g.aggs.iter().any(|d| d.final_ == FinalForm::Distinct) {
        crate::not_built!(g.node_id, crate::admit::refusal::Variant::Kind(KindId::KeyedDistinctMulti));
    }
}

/// The DISTINCT side of a streamed group: the declaration's record (the keys' and the argument's sources, the pair
/// FILTER, all compiled), the argument key, the set and its frame words, the aggregates in PG's transition order.
pub(crate) struct Part<'g, 'p> {
    full: GroupRec<'g, 'p>,
    arg: usize,
    pub set: &'static dyn DistinctSetMech,
    /// The set's first frame word, a byte offset from the stream's words (keyed/stream.rs places it after the record).
    pub set_at: i32,
    /// The record's aggregates by aggno: (index, whether it is a DISTINCT one: a pair count or fold).
    order: PgVec<'g, (usize, bool)>,
    /// The arguments arrive sorted within the group (`sorted`): the set's `insert_sorted`.
    sorted: bool,
    mcx: Mcx<'g>,
}

/// RUNS-33: whether group `g`'s DISTINCT arguments arrive sorted within each group, equal values adjacent: PG presorted
/// every DISTINCT call (`presorted_call`: the planner sorted the group's input by its keys, then the argument), the stream
/// reads that Sort's drain through projections alone (presorted.rs `in_sort_order`), and each call's DISTINCT orders by
/// its type's default family under a bytes collation, so the Sort's equal arguments are equal values (one image: the
/// offer refuses an image that is not its value).
fn sorted(px: &Px<'_, '_>, g: &GroupDecl<'_>) -> bool {
    let default = |d: &AggDecl<'_>| d.node.as_aggref().is_some_and(|r| r.aggdistinct.iter().all(|n| n.as_sort_group_clause().is_some_and(|c| default_order(px.gx.facts, c.sortop))));
    let mut calls = g.aggs.iter().filter(|d| d.mods.distinct > 0).peekable();
    calls.peek().is_some() && calls.all(|d| presorted_call(d) && d.coll.bytes_eq && default(d)) && crate::ops::group::presorted::in_sort_order(px.gx.graph, g)
}

/// Group `g`'s streamed record (the pairs view, its riders' arguments compiled at pipeline `k`'s op `j`) and its part.
pub(crate) fn records<'g, 'p>(px: &mut Px<'g, 'p>, g: &'g GroupDecl<'p>, (k, j): (usize, usize), set: &'static dyn DistinctSetMech) -> (GroupRec<'g, 'p>, Option<Part<'g, 'p>>) {
    let node = g.node_id;
    let Some(arg) = g.keys.iter().position(|k| k.resno == 0) else { bug(node, "a streamed DISTINCT group without its argument key") };
    if arg + 1 != g.keys.len() || !crate::graph::grouped(g) {
        bug(node, "a streamed DISTINCT group whose argument is not its last key after a group key");
    }
    if !crate::graph::order_free(g) {
        shape(node, "a rider whose value reads the row order beside a DISTINCT");
    }
    for d in g.aggs.iter().filter(|d| d.final_ == FinalForm::PairFold) {
        let kind = fold_of(d.class, d.fnoid, false, false).1.abi();
        if !matches!(g.keys[arg].kind, KeyKind::I32 | KeyKind::I64) || !matches!(kind, Some(SlotKind::SumI64 | SlotKind::SumI128 | SlotKind::AvgI128 | SlotKind::MinI64 | SlotKind::MaxI64)) {
            shape(node, "a DISTINCT fold over an argument that is not an integer word");
        }
    }
    let sorted = sorted(px, g);
    let mcx = px.mcx();
    let mut full = GroupRec::new(px, g);
    full.compile_args_at(px, false, (k, j));
    let mut view = GroupRec::new(px, g).emit_view(px);
    let riders: PgVec<'_, i16> = full.aggs.iter().map(|a| a.d.aggno).collect_in(mcx);
    for a in view.aggs.iter_mut() {
        if let Some(f) = full.aggs.iter_mut().find(|f| f.d.aggno == a.d.aggno) {
            (a.args, a.filter) = (core::mem::take(&mut f.args), f.filter);
        }
    }
    view.nums = full.nums.take();
    let mut order: PgVec<'g, (usize, bool)> = (0..view.aggs.len()).map(|i| (i, !riders.contains(&view.aggs[i].d.aggno))).collect_in(mcx);
    order.sort_by_key(|&(i, _)| view.aggs[i].d.aggno);
    (view, Some(Part { full, arg, set, set_at: 0, order, sorted, mcx }))
}

impl<'g, 'p> Part<'g, 'p> {
    /// The group keys' image over `row`, the record's key words (`view`'s: the group words, then the flags word where any key
    /// of the declaration is nullable, the argument's bit clear). The argument is not read.
    pub fn image(&self, view: &GroupRec<'g, 'p>, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) -> ImageWords {
        let mut keys: PgVec<'g, KeyVal> = mcx::reserved_in(self.arg, self.mcx);
        for s in &self.full.ksrc[..self.arg] {
            keys.push_in(match s {
                Some(Arg::Col(c)) => row.col(*c).map(|r| (*r.cell, r.val, r.null)).unwrap_or_else(|| bug(self.full.node, "a group key the row does not carry")),
                Some(Arg::Key { id, cell } | Arg::Ex { id, cell }) => crate::ops::group::distinct_plain::key_val(b, env, self.full.node, row, *id, *cell),
                None => bug(self.full.node, "a group key imaged before it was compiled"),
            });
        }
        view.image_of(b, env, &keys[..])
    }

    /// A group opens over the record at `rec`: its sentinel set (the owner's accumulator is present once opened), its set
    /// at `set` emptied.
    pub fn open(&self, view: &GroupRec<'g, 'p>, b: &mut FunctionBuilder<'_>, env: &mut Env, rec: Value, set: Value) {
        b.store(rec, view.layout.sentinel_off() as i32, env.one);
        self.set.clear(b, env, set);
    }

    /// `row` folded into the open group's record at `rec` in PG's transition order (`images`: the stream's copy cursor).
    pub fn fold(&self, view: &GroupRec<'g, 'p>, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, rec: Value, images: Option<Value>, set: Value) {
        let mut offered = false;
        for &(i, pair) in &self.order {
            let a = &view.aggs[i];
            match pair {
                false => view.step(b, env, row, a, AccAt::Slot(rec, a.d.slot, images)),
                true if !offered => {
                    offered = true;
                    self.offer(view, b, env, row, rec, images, set);
                }
                true => {} // every DISTINCT aggregate of the argument folds at its first one (the argument is read once)
            }
        }
    }

    /// The pair FILTER, then the argument read once; a non-NULL value new to the set adds one pair and steps each DISTINCT
    /// fold of the record. A row the FILTER does not pass never reads the argument (PG's FILTER comes first).
    #[allow(clippy::too_many_arguments)]
    fn offer(&self, view: &GroupRec<'g, 'p>, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, rec: Value, images: Option<Value>, set: Value) {
        let (node, l_join) = (self.full.node, b.create_block(&[]));
        if let Some(ex) = self.full.pair_filter {
            let (p, l_pass) = (crate::step::expr::pred(b, env, node, row, ex), b.create_block(&[]));
            b.cond_br(p, (l_pass, &[]), (l_join, &[]));
            b.switch_to(l_pass);
        }
        env.aside(|env| {
            let (cell, val, null) = match &self.full.ksrc[self.arg] {
                Some(Arg::Key { id, cell } | Arg::Ex { id, cell }) => crate::ops::group::distinct_plain::key_val(b, env, node, row, *id, *cell),
                Some(Arg::Col(c)) => row.col(*c).map(|r| (*r.cell, r.val, r.null)).unwrap_or_else(|| bug(node, "a DISTINCT argument the row does not carry")),
                None => bug(node, "a DISTINCT argument read before it was compiled"),
            };
            if let Some(n) = null {
                let l_value = b.create_block(&[]);
                b.cond_br(n, (l_join, &[]), (l_value, &[]));
                b.switch_to(l_value);
            }
            let r = CellRef { cell: &cell, val, null: None };
            let mut cx = env.lx(b, node);
            let img = keyimg::image(&mut cx, &[KeyCell { cell: r, tf: &self.full.tfs[self.arg], scope: Scope::Program, coll: self.full.colls[self.arg] }]);
            env.memo = cx.memo;
            if !img.exact {
                shape(node, "a DISTINCT argument whose image is not its value (the set compares words alone)");
            }
            let new = match self.sorted {
                true => self.set.insert_sorted(b, env, set, &img.words.words),
                false => self.set.insert(b, env, set, &img.words.words),
            };
            let l_new = b.create_block(&[]);
            b.cond_br(new, (l_new, &[]), (l_join, &[]));
            b.switch_to(l_new);
            let off = view.layout.slot_off(ACC_PAIRS as usize) as i32;
            let n = b.load(Type::I64, rec, off);
            let n = b.int_bin(IntBinOp::Add, n, env.one);
            b.store(rec, off, n);
            for &(i, _) in self.order.iter().filter(|&&(i, pair)| pair && view.aggs[i].d.slot != u16::from(ACC_PAIRS)) {
                let a = &view.aggs[i];
                crate::dict::minmax_code::step_with(a.class, b, env, &a.d, view.slot(rec, a.d.slot), &[r], images);
            }
        });
        b.br(l_join, &[]);
        b.switch_to(l_join);
    }
}
