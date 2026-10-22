//! Hosted aggregate slots (V4R-277, the M6-FOLD seam; M6-plan L0-i). Hooks / levers: -. Budget 90 lines. Owner M6-FOLD.
//!
//! A class that folds an aggregate through face/aggcall.rs (PG's transfn / finalfn by fmgr) keeps one state per group,
//! abi's `SlotKind::AggState`. Decompose gives such a `Trans::Var` register no slot, so the plain end's layout
//! (slots.rs `layout_of`) appends one per hosted transition state (V4R-424: the planner's `aggtransno`, PG's shared
//! state) after decompose's own, and the end declares the Agg's arena once (`agg.arena`, V4R-269) for the classes to
//! call through (`Env::agg_arena`). A keyed record holds the slots too
//! (V4R-375, `keyed_slots`: each record the group opens points at the group's one home state, `home`, which the owner
//! fold takes); the forms it does not take refuse by name. A top's drain over such a record walks its own copy of the
//! emit's array (`drain_copy`).
//! Registers nothing; plain.rs, keyed/hashagg_rec.rs and keyed/emit_group.rs read it.
use gravity_abi::breaker::{BreakerKind, Budget, HashAgg, RecordLayout, SlotKind, LAYOUT_IMPLICIT_COUNT};
use gravity_abi::status::{raise, unsupported, UNSUPPORTED_SCRATCH};
use gravity_decompose::ops::shared::agg::{AggDecl, FinalForm, GroupDecl, GroupForm, NONE};
use gravity_decompose::ops::shared::agg_class::{Class, Collect as C, Split, Trans};
use gravity_decompose::catalog::Am;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::{Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::agg::trans::hosted::{m6_agg_home, m6_agg_home_open, AGG_ORDERED_FED, AGG_STATE_BYTES, AGG_STREAMED, AGG_WORDS, AGG_W_HEAD};
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::{ArenaRef, ArenaVec, Pipeline};
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::primnodes::Aggref;
use types_nodes::Node;

use crate::face::kinds::KindId;
use crate::frame::cancel;
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::graph::Gx;
use crate::admit::refusal::Reason;
use crate::effects::visible::agg;

/// The class kinds folding an aggregate through face/aggcall.rs, a hosted state per group (`AggState`).
const HOSTED: &[KindId] = &[KindId::ClassNumVarlena, KindId::ClassStats, KindId::ClassText, KindId::ClassOther, KindId::ClassCollect, KindId::ClassUser, KindId::ClassOrdered];

/// Whether `d` folds a hosted state in a slot `layout_of` appends: a hosted class kind whose register decompose gave no
/// slot (`Var`; class.stats' `Stat24` / `Stat48`), with no DISTINCT, ordered input or split (other ends' and forms'), or
/// a host-sorted DISTINCT of any class (D1210, src/regroup.rs `sorted`: the host folds it at its final).
pub(crate) fn hosted(d: &AggDecl<'_>) -> bool {
    let plain = d.mods.distinct == 0 && crate::ops::agg::trans::args::ordered(d) == 0;
    hosted_kind(d) && (plain || crate::ops::shared::agg::sorted(d)) && matches!(d.mods.split, Split::Simple)
}

/// V4R-424: whether hosted aggregate `d` rides a transition state an earlier hosted aggregate of `g` opened (one
/// `aggtransno`: the planner's find_compatible_trans shares a state only between aggregates of one transfn, equal
/// arguments, FILTER and modifiers). The host steps a shared state through the aggregate that opened it and returns at
/// once for the others (raw.rs `trans`), so a rider's per-row step (its arguments, the D2597 check, the host call) does
/// nothing: the program emits none, and the rider's final reads the shared state as before (`hosted_slot`).
pub(crate) fn rides(g: &GroupDecl<'_>, d: &AggDecl<'_>) -> bool {
    let t = transno(d);
    t >= 0 && hosted(d) && g.aggs.iter().take_while(|e| !core::ptr::eq(*e, d)).any(|e| hosted(e) && transno(e) == t)
}

/// A hosted class kind's slotless register, any modifiers: a keyed end refuses it by name (D798), not Bug.
pub(crate) fn hosted_kind(d: &AggDecl<'_>) -> bool {
    d.slot == NONE && !matches!(d.trans, Trans::Abi(_)) && (HOSTED.contains(&crate::props::class_kind(d)) || crate::ops::shared::agg::sorted(d))
}

/// V4R-424: `d`'s transition state, the planner's `aggtransno` (find_compatible_trans: one transfn, inputs and initcond);
/// a decl without one is its own.
fn transno(d: &AggDecl<'_>) -> i64 {
    d.node.as_aggref().filter(|r| r.aggtransno >= 0).map_or(-1 - d.aggno as i64, |r| r.aggtransno as i64)
}

/// V4R-424: whether `g.aggs[i]`, one `pick` takes, opens its transition state: the first of the aggregates `pick` takes
/// with its transno (PG shares one state between the aggregates of one transno, each running its finalfn over it, so
/// they fold one `AggState`; the host steps its first).
fn opens_state(g: &GroupDecl<'_>, pick: fn(&AggDecl<'_>) -> bool, i: usize) -> bool {
    let t = transno(&g.aggs[i]);
    pick(&g.aggs[i]) && !g.aggs[..i].iter().any(|e| pick(e) && transno(e) == t)
}

/// V4R-424: the transition states of `g`'s aggregates `pick` takes (`opens_state`), counted without a vector.
fn state_count(g: &GroupDecl<'_>, pick: fn(&AggDecl<'_>) -> bool) -> usize {
    (0..g.aggs.len()).filter(|&i| opens_state(g, pick, i)).count()
}

/// V4R-424: the position of transition state `t` among `g`'s (`opens_state`), without a vector.
fn state_index(g: &GroupDecl<'_>, pick: fn(&AggDecl<'_>) -> bool, t: i64) -> Option<usize> {
    (0..g.aggs.len()).filter(|&i| opens_state(g, pick, i)).position(|i| transno(&g.aggs[i]) == t)
}

/// One `AggState` slot per hosted transition state of `g` (`state_count`) after decompose's slots; a group with none keeps
/// its layout as it was.
pub(crate) fn hosted_slots(a: ArenaRef, l: &mut RecordLayout, g: &GroupDecl<'_>) {
    let n = state_count(g, hosted) + crate::ops::agg::trans::kept::count(g); // D345: the kept columns' slots first (fold/kept.rs)
    if n == 0 {
        return;
    }
    let implicit = l.flags & LAYOUT_IMPLICIT_COUNT != 0;
    let m = l.nslots() - usize::from(implicit);
    let mut slots: ArenaVec<SlotKind> = ArenaVec::with_capacity_in(m + n, a);
    slots.extend_from_slice(&l.slot_kinds()[..m]);
    slots.extend(core::iter::repeat_n(SlotKind::AggState, n));
    let sentinel = (!implicit).then(|| (0..m).find(|&s| l.slot_off(s) == l.sentinel_off()).expect("the sentinel is a slot"));
    *l = gravity_program::record_layout_at(a, l.key_kinds(), &slots, l.has_validity(), sentinel).partitioned_by(l.pkeys);
}

/// aggtrans T6g: an unfiltered any_value whose every argument is a key of its group: each row of a group holds an equal
/// member (the group's equality), so the one it keeps is PG's value but for an equal member (the equal-member ruling).
/// aggtrans C1b: or a column of a relation whose PRIMARY KEY the keys hold (`pk_member`): every row holds the same datum.
fn key_member(gx: &Gx<'_, '_>, g: &GroupDecl<'_>, d: &AggDecl<'_>) -> bool {
    let key = |n: Node<'_>| n.as_var().is_some_and(|v| v.varno == types_nodes::primnodes::OUTER_VAR && v.varlevelsup == 0 && v.varattno > 0 && g.keys.iter().any(|k| k.resno == v.varattno));
    let args = |r: &Aggref<'_>| r.args.iter().filter_map(|t| t.as_target_entry()).filter(|t| !t.resjunk).all(|t| key(t.expr) || pk_member(gx, g, t.expr));
    d.class == Class::Collect(C::AnyValue) && d.node.as_aggref().is_some_and(|r| r.aggfilter.is_none() && r.aggdirectargs.is_nil() && args(r))
}

/// aggtrans C1b: whether `e` is a column of a scanned relation whose PRIMARY KEY sorted / hashed group `g`'s keys hold
/// (PG's check_functional_grouping, decompose `facts::passenger::pk_determined`), a key column standing for a column an
/// inner join's identity equality holds equal to it on every row of each feed (`chain_equal`). A group's rows then carry
/// the relation's one row (a NULL-extended row is all NULL, its key too), so each holds the same datum.
fn pk_member(gx: &Gx<'_, '_>, g: &GroupDecl<'_>, e: Node<'_>) -> bool {
    use gravity_decompose::facts::passenger::syn_col;
    let ps = gx.pipelines();
    let Some((rti, _)) = syn_col(e).filter(|_| g.set.is_none()) else { return false };
    let relid = (0..ps.end()).filter_map(|k| ps.get(k)).find_map(|p| p.source.relation().filter(|r| u32::from(r.0) == rti).map(|r| r.1));
    let Some((pk, n)) = relid.and_then(|r| gx.facts.primary_key(r)) else { return false };
    // a key compares by the column's own equality: no stripped relabel to bpchar or to another collation (a coarser one
    // groups rows of two primary key values)
    let own = |k: &&gravity_decompose::ops::shared::agg::KeyDecl<'_>| !k.expr.bpchar() && k.expr.node().as_var().is_some_and(|v| k.coll == 0 || k.coll == v.varcollid);
    let keys: PgVec<'_, (u32, i16)> = g.keys.iter().filter(own).filter_map(|k| syn_col(k.expr.node())).collect_in(gx.mcx);
    let feeds: PgVec<'_, usize> = (0..ps.len()).filter(|&k| gx.group_at(k).is_some_and(|x| core::ptr::eq(x, g))).collect_in(gx.mcx);
    let held = |c: (u32, i16)| keys.contains(&c) || (!feeds.is_empty() && feeds.iter().all(|&k| keys.iter().any(|&kc| chain_equal(gx, k, c, kc))));
    n > 0 && pk[..n as usize].iter().all(|&att| held((rti, att)))
}

/// aggtrans C1b: whether every row pipeline `k` emits holds columns `x` and `y` equal (`facts::passenger::equal_cols`)
/// by its own ops or those of a pipeline whose Sort it reads, up the chain; never past an Append.
fn chain_equal(gx: &Gx<'_, '_>, mut k: usize, x: (u32, i16), y: (u32, i16)) -> bool {
    loop {
        let p = &gx.pipelines()[k];
        match gravity_decompose::facts::passenger::equal_cols(gx.facts, &p.ops, x, y) {
            Some(false) => {}
            r => return r.is_some(),
        }
        let Source::Breaker { slot } = p.source else { return false };
        match gx.life_of(k, slot).map(|l| l.0 as usize) {
            Some(q) if matches!(gx.pipelines()[q].ops.last(), Some(Op::Sort(_))) && gx.drained_group(q).is_none() => k = q,
            _ => return false,
        }
    }
}

/// group lane (agg_shop-0224): whether sorted group `g` (streamed, D1071, or a keyed end) reads its rows in
/// PostgreSQL's order, ties too: its feed pipeline reads a btree index scan of a heap relation (index order, equal keys in
/// heap TID order, as PG's scan returns them) through filters, projections, merge joins whose inner pipeline is such a
/// scan through filters and projections, and nested loops whose inner is one (its rows per outer row in index order).
/// No Sort (Gravity's keeps none of PG's tie order), hash probe, Memoize or breaker sits between, and no DISTINCT call
/// (decompose may absorb the Sort under a DISTINCT group).
pub(crate) fn scan_order(gx: &Gx<'_, '_>, g: &GroupDecl<'_>) -> bool {
    use gravity_decompose::catalog::IndexAm;
    let scan = |s: &Source<'_>| matches!(s, Source::Index { am: Am::Heap, iam: IndexAm::Btree, .. });
    let pipe = |op: &Op<'_>| matches!(op, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    let ps = gx.pipelines();
    let k = g.producer.0 as usize;
    let Some(p) = ps.get(k).filter(|p| scan(&p.source) && g.aggs.iter().all(|d| d.mods.distinct == 0)) else { return false };
    let run = p.ops.iter().position(|op| matches!(op, Op::GroupRun(_))).unwrap_or(p.ops.len());
    p.ops[..run].iter().all(|op| match op {
        Op::Join(JoinDesc::Merge { slot, .. }) => gx.life_of(k, *slot).and_then(|l| ps.get(l.0 as usize)).is_some_and(|q| scan(&q.source) && q.ops.iter().all(pipe)),
        Op::Join(JoinDesc::Nest { inner, .. }) => scan(&inner.source) && inner.ops.iter().all(pipe),
        op => pipe(op),
    })
}

/// #530: sorted group `g`'s keyed end folds every group's rows before it hands the first on, where PG's GroupAggregate
/// hands each group on (finals, HAVING, projection; a LIMIT, EXISTS or cursor above may stop) before it reads the next's.
/// Equal only when no per-row step raises and no final acts (a D1071 stream keeps PG's order: its keys and input only):
/// every aggregate `still`, its arguments and FILTER raise_free (join/rtfilter.rs), each feed's ops raise_free over a
/// scan's rows or a full Sort's by its keys' default btree families (Facts::op_dir: a user family's '=' is the group's, PG
/// calls it per row pair; an index's opclass is unknown here: none taken, D2051), every key built in or an enum (a domain's
/// base): a user one's comparator (range_cmp's subtype one) runs at the emit's sort (SK_CMP) or the stream's '=', D2039,
/// and in any Sort of the program keyed by one, upstream too (runs.sort calls it other times than PG's qsort, D2041).
/// aggtrans T6: `Err(feeds)` when only the raise-free half fails (rows made first, keys and sorts built in): the batch may
/// fold ahead under the licence (`effects::fold_ahead`) over those feeds; `Err` of none when the rest fails. T6b: a feed
/// over an index scan's rows only folds ahead under the licence (its batch counts the scan's opclass and key calls).
fn quiet_rows<'g>(gx: &Gx<'g, '_>, g: &GroupDecl<'_>, streamed: bool) -> Result<(), PgVec<'g, usize>> {
    let rf = |n: Node<'_>| crate::effects::raise_free(gx, n);
    let key = |t: u32| agg::builtin_order(gx, t);
    let calls = streamed || g.aggs.iter().all(|d| still(gx, d) && d.node.as_aggref().is_some_and(|r| r.args.iter().all(rf) && r.aggfilter.is_none_or(rf)));
    let typed = |s: &gravity_decompose::ops::shared::sort::SortDesc<'_>, r: i16| s.tlist.iter().filter_map(|n| n.as_target_entry()).find(|t| t.resno == r).and_then(|t| gravity_decompose::catalog::expr_type::type_of(t.expr));
    // aggtrans C2: also a Sort over another group's drain (a subquery's GROUP BY under a sorted GROUP BY): PG's Sort reads
    // that group's every row (its finals, HAVING, projection) before it returns the first, as the keyed end does
    let sort = |q: usize| gx.pipelines().get(q).is_some_and(|x| matches!(x.ops.last(), Some(Op::Sort(s)) if s.presorted == 0 && s.keys.iter().all(|k| gx.facts.key(gravity_decompose::catalog::KeyReq::sort(k.opno)).is_some_and(|o| o.default_family))));
    let drained_sort = |k: usize| match gx.pipelines()[k].source {
        Source::Breaker { slot } => gx.life_of(k, slot).map(|l| l.0 as usize).filter(|&q| sort(q)),
        _ => None,
    };
    // D3504: or a Breaker over an Incremental Sort whose own feed is such a full Sort. PG reads the Incremental Sort a
    // prefix at a time, so its pipeline runs here ahead of PG's; that shows nothing when it raises nothing (no op before the
    // Sort raises, its comparators are quiet), a bare collect's tie order being free (D1428)
    let runs = |k: usize| match gx.pipelines()[k].source {
        Source::Breaker { slot } => gx.life_of(k, slot).map(|l| l.0 as usize).is_some_and(|q| {
            let quiet_sort = |s: &gravity_decompose::ops::shared::sort::SortDesc<'_>| s.presorted > 0 && crate::effects::sort_compare(gx, s) == gravity_decompose::effects::Effects::NONE;
            matches!(gx.pipelines()[q].ops.split_last(), Some((Op::Sort(s), pre)) if quiet_sort(s) && crate::effects::ops_raise_free(gx, pre)) && drained_sort(q).is_some()
        }),
        _ => false,
    };
    let first = |k: usize| match &gx.pipelines()[k].source {
        Source::Breaker { .. } => drained_sort(k).is_some() || runs(k),
        s => matches!(s, Source::Range { .. } | Source::Const),
    };
    let quiet = |k: usize| streamed && matches!(gx.pipelines()[k].source, Source::Breaker { .. }) || crate::effects::ops_raise_free(gx, &gx.pipelines()[k].ops);
    let scan = |k: usize| matches!(gx.pipelines()[k].source, Source::Index { .. }); // T6b: the licence's batch counts the index scan's calls
    let feeds: PgVec<'_, usize> = if streamed { mcx::pg_vec![in gx.mcx; g.producer.0 as usize] } else { (0..gx.pipelines().len()).filter(|&k| gx.group_at(k).is_some_and(|x| core::ptr::eq(x, g))).collect_in(gx.mcx) };
    // group lane, USEROP #530 (kit K6): a feed's full Sort and every pipeline feeding it run whole before PostgreSQL's
    // GroupAggregate reads its first row too, so a comparator they call (a Sort by a user range whose subtype opclass is
    // plpgsql) runs, and any raise of it comes, before any group is handed on in both; its call count is free (the user's
    // ruling 10-04 15:11Z; D2041 closed, DECISIONS 3). A Sort anywhere else (above the group, an incremental one, a
    // SubPlan's) still sorts only by built-in comparators.
    let mut drained: PgVec<'_, usize> = feeds.iter().filter_map(|&k| drained_sort(k)).collect_in(gx.mcx);
    let mut i = 0;
    while let Some(&k) = drained.get(i) {
        let p = &gx.pipelines()[k];
        let src = if let Source::Breaker { slot } = p.source { Some(slot) } else { None };
        let lives = p.desc.consumes.iter().copied().chain(src).filter(|&s| !gx.settled_work(k, s)).filter_map(|s| gx.life_of(k, s).map(|l| l.0 as usize));
        for q in lives.chain(gx.union_feeds(k)).chain(gx.run_arms(k)).filter(|&q| q < gx.pipelines().len()) {
            if !drained.contains(&q) {
                drained.push_in(q);
            }
        }
        i += 1;
    }
    // USEROP: a CompareFn key (a user family's comparator, `SortKey::compare_fn`) is no built-in order whatever its type
    let sorts = gx.pipelines().iter().enumerate().filter(|(k, _)| !drained.contains(k)).flat_map(|(_, p)| p.ops.iter()).all(|op| !matches!(op, Op::Sort(s) if s.keys.iter().any(|k| k.compare_fn() || !typed(s, k.resno).is_some_and(key))));
    if !(g.keys.iter().all(|k| key(k.tf)) && sorts && !feeds.is_empty() && feeds.iter().all(|&k| first(k) || scan(k))) {
        return Err(PgVec::new_in(gx.mcx));
    }
    if calls && feeds.iter().all(|&k| first(k) && quiet(k)) { Ok(()) } else { Err(feeds) }
}

/// D3743 (`DECISIONS.md` 4 (D) b): sorted group `g`'s keyed end folds every group's
/// rows before it hands the first on, and `quiet_rows` found per-row work in or below its feeds that may raise ahead of
/// PG's GroupAggregate with no plan-order run to re-check it: `fold_ahead` denied, or a feed that is no full Sort's
/// drain (an Incremental Sort over a merge join of two groups, postgres_exporter's pg_process_idle collector). That work
/// runs ahead unchecked where `Px::ahead_unchecked` grants it for every feed (nothing in the statement acts, no user code
/// in or below the feed): an error it raises from rows PG's plan never reads (a LIMIT, a cursor or a merge join stopping
/// first) is a genuine data error, tagged; the pull is capped as the licensed fold's (D439), so a cursor's FETCH n
/// finalizes no group past the n-th. `quiet_rows`'s other rules hold: every aggregate `still` with raise-free arguments
/// and FILTER (or the group streams, D1071), every key built in or an enum, and no Sort of the program by a user
/// comparator (they run at the emit's sort and the run test, D2039 / D2041). The licence every feed runs ahead under
/// (`Px::ahead_unchecked`, the same tagged licence for each; construct C2; D4470: else the accepted class, `Px::accept`),
/// or `None`: the caller refuses.
fn rows_unchecked(px: &mut Px<'_, '_>, g: &GroupDecl<'_>, streamed: bool) -> Option<gravity_decompose::effects::Licence> {
    let gx = px.gx;
    let rf = |n: Node<'_>| crate::effects::raise_free(gx, n);
    let key = |t: u32| agg::builtin_order(gx, t);
    let calls = streamed || g.aggs.iter().all(|d| still(gx, d) && d.node.as_aggref().is_some_and(|r| r.args.iter().all(rf) && r.aggfilter.is_none_or(rf)));
    let typed = |s: &gravity_decompose::ops::shared::sort::SortDesc<'_>, r: i16| s.tlist.iter().filter_map(|n| n.as_target_entry()).find(|t| t.resno == r).and_then(|t| gravity_decompose::catalog::expr_type::type_of(t.expr));
    let sorts = gx.pipelines().iter().flat_map(|p| p.ops.iter()).all(|op| !matches!(op, Op::Sort(s) if s.keys.iter().any(|k| k.compare_fn() || !typed(s, k.resno).is_some_and(key))));
    let feeds: PgVec<'_, usize> = if streamed { mcx::pg_vec![in gx.mcx; g.producer.0 as usize] } else { (0..gx.pipelines().len()).filter(|&k| gx.group_at(k).is_some_and(|x| core::ptr::eq(x, g))).collect_in(gx.mcx) };
    if !(calls && sorts && g.keys.iter().all(|k| key(k.tf)) && !feeds.is_empty()) {
        return None;
    }
    let mut licence = None;
    for &k in &feeds {
        // D4470: where the tagged licence is denied (a feed that hides calls from its class, a recursive union whose seed
        // may raise; a part EXPLAIN cannot name), the work run ahead is the accepted class (early work that can only raise
        // an error PG never hits) when it acts nowhere and no user code is in or below the feed (`Px::accept` over
        // `batch_effects`: an act in the statement asks `volatile_free`): what it can show is an error PG never hits; rows
        // never differ
        let own = px.batch_effects(gravity_decompose::effects::Effects::RAISES);
        licence = Some(px.ahead_unchecked(k, gravity_decompose::effects::Effects::RAISES).or_else(|| px.accept(k, own))?);
    }
    px.declare_proto(Proto::new(ProtoKind::Volatile, feeds[0] as u16));
    licence
}

/// #530: effects/visible/agg.rs `still` (regroup.rs `call_order` reads the same).
fn still(gx: &Gx<'_, '_>, d: &AggDecl<'_>) -> bool {
    agg::still(gx, d)
}

/// D3374: whether sorted group `g` may fold a money or interval sum or average in Gravity's tie order where the recheck
/// licence is not granted: in the plan-order run (every licence denied; not a bounded one, a cursor's FETCH n,
/// `denying_bounded`) and in a writing statement (its acting set denies the licence; it is never re-run), where
/// PostgreSQL reads the group whole (decompose's `PipelineGraph::whole`: no LIMIT, SubPlan or cursor stop above, a
/// ModifyTable's source included). Every group then runs in PostgreSQL too, so which error comes first is free (DECISIONS
/// 4); within a group the order is the Sort's ties' or the keyed end's, so an overflow is the answer of some order of the
/// group's rows, which the e2e judge accepts as such (a value only as the group's exact total, a raise only where some
/// order overflows).
fn own_order(px: &Px<'_, '_>, g: &GroupDecl<'_>) -> bool {
    use gravity_decompose::effects::{denying, denying_bounded};
    let writes = px.gx.pipelines().iter().any(|p| p.ops.iter().any(|o| matches!(o, Op::Write(_))));
    (denying() && !denying_bounded() || writes) && px.gx.graph.whole.contains(&g.node_id)
}

/// V4R-375 (D798 part 1): keyed group `g`'s hosted aggregates checked and laid out after decompose's slots in `l`: one
/// `AggState` per hosted aggregate (group order), then one `MaxI64` holding the arena's handle (every record the same
/// word: the owner fold's combine reads it, kernels m6_agg `handle_slot`); `Some(first AggState slot)`, `None` without
/// one. A hashed or sorted group (runs.group: keyed.hashagg's end), refused by name otherwise. D2649: the planner's
/// two-pass form too: pass 2 folds each admitted group's rows into its home state in input order, and a group it skips
/// holds one row (its pass-1 key seen once), whose one transition and final over a Var or Const argument raise nothing
/// for the built-in order-free aggregates, the only ones the planner admits (gravity_paths.rs `admissible_aggregates`, D451).
/// V4R-427 (D1313), #530: its records all point at the group's one home state (hashagg open_rec `home`) and the program
/// runs width 1 (exec v4/m6.rs `keyed`), so no partial is ever combined (no combinefn or serial form needed) and each
/// group folds its rows in input order: PG's AGG_HASHED order, a user subquery ORDER BY's (kept); PG's sort output order
/// within a sorted group is free (no aggregate ORDER BY: every row of the group a tie, D1428, main 03:21Z).
/// What a hosted aggregate folds ahead of PostgreSQL's group-by-group hand-on runs under the licence it was granted
/// (`effects::fold_ahead`'s recheck, `Px::read_whole`'s whole-read one, `rows_unchecked`'s tagged one), handed back beside
/// the slot (the first one granted; `None` when nothing runs ahead).
pub(crate) fn keyed_slots(px: &mut Px<'_, '_>, l: &mut RecordLayout, g: &GroupDecl<'_>, node: i32) -> (Option<u16>, Option<gravity_decompose::effects::Licence>) {
    let (a, shape) = (px.arena, |why: &'static str| -> ! { crate::refuse!(node, Reason::Shape { why: crate::admit::refusal::ShapeWhy(why) }) });
    if g.aggs.iter().any(|d| d.final_ == FinalForm::Distinct && !crate::ops::shared::agg::sorted(d)) {
        shape("a DISTINCT aggregate of a user-defined or ordered-set aggregate (PG's hosted sort)"); // D1210, D1283
    }
    let hs: PgVec<'_, &AggDecl<'_>> = g.aggs.iter().filter(|d| hosted_kind(d)).collect_in(px.mcx());
    if hs.is_empty() {
        return (None, None);
    }
    if hs.iter().any(|d| crate::ops::shared::agg::sorted(d)) && !crate::ops::group::regroup::riders_free(a, px.gx.graph, g) {
        shape("a rider whose value reads the row order beside a DISTINCT (PG's sorted input)");
    }
    if hs.iter().any(|d| !hosted(d)) || !matches!(g.form, GroupForm::Hashed | GroupForm::Sorted) {
        shape("a hosted aggregate state in a keyed record (Decision 6: its combine and spill forms)");
    }
    let streamed = px.gx.streams().any(|s| core::ptr::eq(s, g)); // D1071: PG's order, group by group
    let mut held = None;
    for d in hs.iter().filter(|d| g.form == GroupForm::Sorted && !crate::ops::shared::agg::sorted(d)) {
        // aggtrans T6: a money or interval sum or average folds exactly in any order, but its partial sums may overflow
        // where PG's order does not: sort-free under the licence's recheck (the overflow re-runs the statement in plan order).
        // aggtrans C1a: bit_and / bit_or / bit_xor over bit / varbit fold one value in any order; a group of two lengths raises
        // in any order, ahead of PG's group-by-group hand-on (a LIMIT stops sooner): the same recheck, and the plan-order
        // run's stream raises at PG's group
        let producer = g.producer.0 as usize;
        if agg::sort_free(px, d, streamed) || key_member(px.gx, g, d) || (streamed && matches!(d.class, Class::BitVar(_))) {
            continue;
        }
        let folded = if matches!(d.class, Class::SumMoney | Class::SumInterval | Class::AvgInterval | Class::BitVar(_)) { crate::effects::fold_ahead(px, core::slice::from_ref(*d), &[(producer, 0)]) } else { None };
        if let Some(lc) = folded {
            held = held.or(Some(lc));
            px.declare_proto(Proto::new(ProtoKind::Volatile, producer as u16)); // D439: a pull finalizes no group ahead
            continue;
        }
        // D3374: a money or interval sum or average adds its group's rows in Gravity's tie order where PostgreSQL reads
        // the group whole, its overflow then the answer of some order of them (`own_order`)
        // the sum or average may raise (an overflow) and acts nowhere: the whole-read licence covers it
        if matches!(d.class, Class::SumMoney | Class::SumInterval | Class::AvgInterval) && own_order(px, g) {
            held = held.or(px.read_whole(producer, gravity_decompose::effects::Effects::RAISES)); // a reader that may stop (an SPI cursor, a SQL function's lazy row) delegates
            continue;
        }
        match d.class {
            // a collect folds its group's rows in arrival order (any_value keeps the first non-NULL argument;
            // array_agg, string_agg, json_agg, json_object_agg, xmlagg and array_agg of arrays append each), PG's
            // when the group (streamed or not: one home state per group, folded in input order) reads its rows in
            // PG's order, ties too
            Class::Collect(_) if scan_order(px.gx, g) => {}
            // the names the Hashed-only gate gave them before #530 (e2e markers cite them); the reason is `sort_free`'s
            // K4.riderg: a streamed group (decompose streams a sorted group holding a plain user aggregate, gsets.rs
            // `user_folds`) folds it in PG's order, group by group, to the consumer's stop; within a group ties (D1428)
            Class::User if streamed => {}
            Class::User => shape("a user-defined hosted aggregate in a keyed record (decompose's table knows no combinefn for it)"),
            _ => shape("a hosted aggregate reading its rows' order in a sorted group (PG's sort output order)"),
        }
    }
    let bare = g.form == GroupForm::Sorted && hs.iter().any(|d| agg::collect(d) && !crate::ops::shared::agg::sorted(d));
    if let Some(Err(feeds)) = bare.then(|| quiet_rows(px.gx, g, streamed)) {
        let gx = px.gx;
        // a streamed group (D1071) hands its open group on before it folds the next group's first row, so its aggregates'
        // calls run in PG's order, row by row: only its feed's work up to the run (an index scan's own calls, the ops
        // before `GroupRun`) is read ahead of PostgreSQL's (aggtrans_core-0339: a GroupAggregate over an Index Scan)
        let upto = |k: usize| {
            let ops = &gx.pipelines()[k].ops;
            if streamed { ops.iter().position(|op| matches!(op, Op::GroupRun(_))).unwrap_or(ops.len()) } else { ops.len() }
        };
        let batch: PgVec<'_, (usize, usize)> = feeds.iter().map(|&k| (k, upto(k))).collect_in(px.mcx());
        // aggtrans T6: only the raise-free half fails: the batch folds ahead under the licence, its pull capped (D439)
        let folded = if batch.is_empty() { None } else { crate::effects::fold_ahead(px, if streamed { &[] } else { &g.aggs[..] }, &batch[..]) };
        if let Some(lc) = folded {
            held = held.or(Some(lc));
            px.declare_proto(Proto::new(ProtoKind::Volatile, batch[0].0 as u16));
        } else {
            let Some(lc) = rows_unchecked(px, g, streamed) else {
                shape("#530: a collect in a sorted group beside per-row work that may raise (PG hands each group on before it reads the next group's rows)");
            };
            held = held.or(Some(lc));
        }
    }
    // group lane, USEROP #530 (kit K2.ragg; decompose facts/stream.rs `collect_streams`): a streamed group's collect whose
    // final may run user code or raise (range_agg over a user range sorts by its subtype's plpgsql comparator) is finalized
    // as its group is handed on; D439's pull cap, as K4.riderg's user fold (ops/group/stream.rs), so a cursor's FETCH n
    // finalizes no group past the n-th, as PG's GroupAggregate under its portal. A streamed sorting call (an ORDER BY
    // call or an ordered-set / hypothetical one: facts/stream.rs `sorts_stream`) whose transition or final may run user
    // code or raise (json_agg's json cast of a user range, range_agg's subtype comparator) takes the same cap: PG runs it
    // for the groups its portal pulls and the first row of the next, never past them
    let gx = px.gx;
    let user = |d: &AggDecl<'_>| !still(gx, d);
    let sorting = |d: &AggDecl<'_>| crate::ops::shared::agg::sorted(d) || matches!(d.class, Class::OrderedSet(_) | Class::Hypothetical(_));
    let capped = (bare && hs.iter().any(|d| agg::collect(d) && user(*d))) || hs.iter().any(|d| sorting(*d) && user(*d));
    if streamed && capped {
        px.declare_proto(Proto::new(ProtoKind::Volatile, g.producer.0 as u16));
    }
    let implicit = l.flags & LAYOUT_IMPLICIT_COUNT != 0;
    let (m, n) = (l.nslots() - usize::from(implicit), state_count(g, hosted_kind));
    let mut slots: ArenaVec<SlotKind> = ArenaVec::with_capacity_in(m + n + 1, a);
    slots.extend_from_slice(&l.slot_kinds()[..m]);
    slots.extend(core::iter::repeat_n(SlotKind::AggState, n));
    slots.push(SlotKind::MaxI64);
    let sentinel = (!implicit).then(|| (0..m).find(|&s| l.slot_off(s) == l.sentinel_off()).expect("the sentinel is a slot"));
    *l = gravity_program::record_layout_at(a, l.key_kinds(), &slots, l.has_validity(), sentinel).partitioned_by(l.pkeys);
    (Some(m as u16), held)
}

/// V4R-375: hosted aggregate `d`'s slot (its transition state's, V4R-424) in a keyed record whose first `AggState` slot
/// is `first` (`keyed_slots`).
pub(crate) fn keyed_slot(g: &GroupDecl<'_>, d: &AggDecl<'_>, first: u16) -> u16 {
    first + state_index(g, hosted_kind, transno(d)).expect("a hosted aggregate of the group") as u16
}

/// V4R-375: the top's drain over a record holding hosted states walks its own copy of the emit's array (`kept` pointers
/// at `ptrs`): the drain's fresh call rewinds the scratch window the emit's gather filed the array in, and each hosted
/// final copies its by-reference image into that window (kernels m6_agg `m6_agg_final`). The copy runs from the top when
/// it lands above the array (a memmove); a refused scratch leaves by `l_out` with UNSUPPORTED_SCRATCH.
pub(crate) fn drain_copy(b: &mut FunctionBuilder<'_>, env: &mut Env, ptrs: Value, kept: Value, l_out: Block) -> Value {
    let eight = b.iconst(Type::I64, 8);
    let bytes = b.int_bin(IntBinOp::Mul, kept, eight);
    let to = b.call_host(env.core.scratch, &[env.ctx, bytes], Some(Type::Ptr)).expect("scratch returns its pointer");
    let (toi, fromi) = (b.conv(ConvOp::PtrToInt, Type::I64, to), b.conv(ConvOp::PtrToInt, Type::I64, ptrs));
    let (refused, l_copy, l_done) = (b.icmp(IntCond::Eq, toi, env.zero), b.create_block(&[]), b.create_block(&[]));
    let unsup = b.iconst(Type::I64, unsupported(UNSUPPORTED_SCRATCH));
    b.cond_br(refused, (l_out, &[unsup]), (l_copy, &[]));
    b.switch_to(l_copy);
    let back = b.icmp(IntCond::Ugt, toi, fromi);
    let last = b.int_bin(IntBinOp::Sub, kept, env.one);
    let (h, body) = cancel::header(b, env.ctx, env.l_cancel, &[Type::I64], env.mcx);
    b.br(h, &[env.zero]);
    b.switch_to(body);
    let i = b.block_params(body)[0];
    let (more, l_step) = (b.icmp(IntCond::Slt, i, kept), b.create_block(&[]));
    b.cond_br(more, (l_step, &[]), (l_done, &[]));
    b.switch_to(l_step);
    let top = b.int_bin(IntBinOp::Sub, last, i);
    let j = b.select(back, top, i);
    let at = b.ptr_add(ptrs, j, 8, 0);
    let r = b.load(Type::Ptr, at, 0);
    let dst = b.ptr_add(to, j, 8, 0);
    b.store(dst, 0, r);
    let i2 = b.int_bin(IntBinOp::Add, i, env.one);
    b.br(h, &[i2]);
    b.switch_to(l_done);
    to
}

/// V4R-375 (D798's bound): keyed record `rec` of `l`, just opened by the table (a new group, or the group again after T1
/// evicted or scattered its record), points each `AggState` slot at its group's one home state (kernels `m6_agg_home`
/// over arena `handle` and the record's identity words): a group advances one state in input order however often T1
/// evicts it, never a state per partial. A raise leaves by the HOST bit.
pub(crate) fn home(b: &mut FunctionBuilder<'_>, env: &mut Env, l: &RecordLayout, rec: Value, handle: Value, node: i32) {
    let states: PgVec<'_, usize> = (0..l.nslots()).filter(|&j| l.slot_kind(j) == SlotKind::AggState).collect_in(env.mcx);
    let first = l.slot_off(states[0]) as usize;
    if states.iter().enumerate().any(|(i, &j)| l.slot_off(j) as usize != first + i * AGG_STATE_BYTES as usize) {
        crate::refuse!(node, Reason::Bug { payload: "a keyed record's hosted state slots are not one run" });
    }
    let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
    let f = b.declare_host_once("m6_agg_home", &[Type::Ptr, Type::I64, Type::Ptr, Type::I64, Type::I64, Type::I64], Some(Type::I64), m6_agg_home as *const () as usize, flags);
    let (nk, at, n) = (b.iconst(Type::I64, l.nkeys() as i64), b.iconst(Type::I64, first as i64), b.iconst(Type::I64, states.len() as i64));
    let st = b.call_host(f, &[env.ctx, handle, rec, nk, at, n], Some(Type::I64)).expect("m6_agg_home returns its status");
    let (bad, ok, e) = (b.icmp(IntCond::Ne, st, env.zero), b.create_block(&[]), b.iconst(Type::I64, raise::bit(raise::HOST) as i64));
    b.cond_br(bad, (env.l_raise, &[e]), (ok, &[]));
    b.switch_to(ok);
}

/// V4R-375: right before a keyed hosted table's open (arena word `word`): a table laid out afresh (a new run, a rescan)
/// forgets the arena's homes (kernels `m6_agg_home_open`); a failure (its error stashed) leaves by the HOST bit, as `home`.
pub(crate) fn home_open(b: &mut FunctionBuilder<'_>, env: &mut Env, word: u16) {
    let h = b.load(Type::I64, env.params, 8 * word as i32);
    let f = b.declare_host_once("m6_agg_home_open", &[Type::Ptr, Type::I64], Some(Type::I64), m6_agg_home_open as *const () as usize, HostFlags { touches_memory: true, ..HostFlags::default() });
    let r = b.call_host(f, &[env.ctx, h], Some(Type::I64)).expect("m6_agg_home_open returns its status");
    let (bad, ok, e) = (b.icmp(IntCond::Ne, r, env.zero), b.create_block(&[]), b.iconst(Type::I64, raise::bit(raise::HOST) as i64));
    b.cond_br(bad, (env.l_raise, &[e]), (ok, &[]));
    b.switch_to(ok);
}

/// Hosted aggregate `d`'s slot (its transition state's, V4R-424) in `layout_of`'s layout `l` of `g`.
pub(crate) fn hosted_slot(l: &RecordLayout, g: &GroupDecl<'_>, d: &AggDecl<'_>) -> u16 {
    let (s, i) = (state_count(g, hosted), state_index(g, hosted, transno(d)).expect("a hosted aggregate of the group"));
    (l.nslots() - usize::from(l.flags & LAYOUT_IMPLICIT_COUNT != 0) - s + i) as u16
}

/// Group `g`'s arena handle word: its Agg's `agg.arena` (V4R-269, face/aggcall.rs `arena`'s declaration) declared by the
/// first end or drain of a hosted class, found by the next; frame/program.rs orders it after the width protocols. D1370: a
/// grouping set's arm has an arena of its own (the Proto argument `node | (arm + 1) << 32`; exec reads the Agg from the low
/// word), so each set's keyed homes, states and grant charge are its own, as PG's per-set hash tables. D1596: bit 62 marks a
/// plain group whose aggpresorted DISTINCTs fold per row (fold/plain.rs `fed`; exec v4/m6.rs reads it), bit 61
/// (kernels `AGG_ORDERED_FED`) a group whose aggpresorted ORDER BY aggregates fold per row (effects `ordered_fed`,
/// `ordered_fed_acting`), bit 60
/// (`AGG_STREAMED`) a streamed sorted group's (ops/group/stream.rs: one open group, its context reset at the next group).
pub(crate) fn arena_word(px: &mut Px<'_, '_>, k: usize, g: &GroupDecl<'_>) -> u16 {
    let fed = u64::from(crate::ops::agg::trans::plain::fed(px, g)) * gravity_kernels::ops::agg::trans::hosted::AGG_PRESORTED_FED; // D1596
    let gx = px.gx;
    let ordered = crate::effects::ordered_fed(gx, g, &|d: &AggDecl<'_>| still(gx, d)) || crate::effects::ordered_fed_acting(gx, g);
    let fed = fed | u64::from(ordered) * AGG_ORDERED_FED; // presorted ORDER BY aggregates advanced per row
    let fed = fed | u64::from(gx.streams().any(|s| core::ptr::eq(s, g))) * AGG_STREAMED; // one open group at a time (group/stream.rs)
    let arg = g.node_id as u32 as u64 | g.set.map_or(0, |s| (s.arm as u64 + 1) << 32) | fed;
    let base = match px.protos.iter().find(|p| p.kind == ProtoKind::AggArena && p.arg == arg) {
        Some(p) => p.words.base,
        None => {
            let base = px.proto_words(AGG_WORDS);
            px.declare_proto(Proto::new(ProtoKind::AggArena, k as u16).words(base, AGG_WORDS).arg(arg));
            base
        }
    };
    base + AGG_W_HEAD as u16
}

/// D792: a plain breaker's region is its record's, `HashAgg::bytes` of `layout_of`'s layout (the appended AggState slots
/// included): the end's descriptor takes that size, one source of truth. A fixed region still smaller than the record
/// refuses as a Bug at lowering, so no width writes past it. C10: a regrouped plain region is a program constant of the
/// record's bytes (`spill::Shape::Constant`).
pub(crate) fn region(node: i32, l: &RecordLayout, mut d: Pipeline) -> Pipeline {
    let n = HashAgg::bytes(l, 1);
    let mut regrouped = false;
    if let Some(b) = crate::frame::iface::breaker_of_mut(&mut d) {
        if b.kind == BreakerKind::Distinct {
            // D1210: a pair-form group src/regroup.rs re-declared plain: decompose's Distinct breaker is the plain region
            (b.kind, b.bytes, b.budget, b.spill) = (BreakerKind::PlainAgg, n, Budget::Fixed(n), gravity_abi::breaker::Spill::None);
            (d.owner, d.words) = (None, None);
            regrouped = true;
        }
    }
    if regrouped {
        d = crate::frame::iface::end_holds(d, gravity_program::spill::Shape::Constant(n));
    }
    if let Some(b) = crate::frame::iface::breaker_of_mut(&mut d) {
        if b.kind == BreakerKind::PlainAgg && matches!(b.budget, Budget::Fixed(f) if f == b.bytes && f < n) {
            (b.bytes, b.budget) = (n, Budget::Fixed(n));
        }
        if b.kind == BreakerKind::PlainAgg && matches!(b.budget, Budget::Fixed(f) if f < n) {
            crate::refuse!(node, Reason::Bug { payload: "a plain breaker's region is not its record's (D792)" });
        }
    }
    d
}

#[cfg(test)]
mod tests {
    use gravity_abi::breaker::{Breaker, BreakerKind, Budget, HashAgg, RecordLayout, SlotKind};
    use gravity_program::{End, Pipeline, Reason as Why};

    use super::region;

    /// D792: a plain breaker sized from decompose's layout (no AggState slot) grows to the record `layout_of` lays out; a
    /// region already the record's is left as it is; a fixed region smaller than the record can never be lowered.
    #[test]
    fn a_plain_breaker_region_is_its_records_with_the_hosted_slots() {
        let base = RecordLayout::new(&[], &[SlotKind::Count]);
        let hosted = RecordLayout::new(&[], &[SlotKind::Count, SlotKind::AggState, SlotKind::AggState]);
        let (small, big) = (HashAgg::bytes(&base, 1), HashAgg::bytes(&hosted, 1));
        assert!(big >= small + 32, "two AggState slots take 32 more bytes");
        let desc = |bytes| Pipeline::serial(-1, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, bytes)), Why::NoDrivingSite);
        let End::Breaker(b) = region(1, &hosted, desc(small)).ends_in else { panic!("a breaker") };
        assert_eq!((b.bytes, b.budget), (big, Budget::Fixed(big)), "the region grows to the record's");
        let End::Breaker(b) = region(1, &base, desc(small)).ends_in else { panic!("a breaker") };
        assert_eq!((b.bytes, b.budget), (small, Budget::Fixed(small)), "an unchanged layout keeps its region");
        let short = Pipeline::serial(-1, End::Breaker(Breaker { budget: Budget::Fixed(8), ..Breaker::sized(BreakerKind::PlainAgg, small) }), Why::NoDrivingSite);
        let r = crate::catch(3, || region(1, &hosted, short)).expect_err("a region smaller than the record refuses");
        assert!(format!("{:?}", r.reason).contains("D792"), "{:?}", r.reason);
    }
}
