//! The effects home's drain file (a family's decisions sit in a sibling of visible.rs): join.drain's spooled form's effect
//! readings, moved as they were from ops/hashjoin/drain/spool_admit.rs, which keeps the refusal names and asks here. F5's coverage (lowerv4 holds
//! no PlannedStmt): `acts`, a walk of every expression, source and op a pipeline runs, a SubPlan's and a nest's own
//! pipelines included (`graph_acts`, `pipe_acts`, `inner_acts`, `members_act`, `op_acts`, `group_acts`); N1's D393 quiet
//! probe (`quiet`, `Gx::spool_quiet`); TAILSTOP's raise-free tail op (`effects::op_raise_free`'s tail reading); USER's index probes
//! (`index_acts`, `user_index`, `user_bitmap`) and drain end (`user_end`); A2END's order-free end (`order_free`, `table_free`, `reader_free`, `store_free`, `rows_free`, `call_free`, `order_free_class`; D3637's
//! money / interval sums under their licence: `order_free_in`, `sums_by_order`, `sums_end`). From
//! ops/hashjoin/drain/arms.rs: D393's quiet drain arms (`Gx::quiet_drain_arms`, `arm_quiet`, `source_still`), and the one
//! `user_clause` both files kept a copy of.
use gravity_abi::breaker::BreakerKind;
use gravity_decompose::catalog::IndexAm;
use gravity_decompose::effects::{self, builtin_oper, Volatility};
use gravity_decompose::graph::{Bound, IndexKey, KeyForm, Source};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::append::{AppendDesc, PruneDesc};
use gravity_decompose::ops::shared::agg_class::{Class, Ext};
use gravity_decompose::catalog::typefacts::FIRST_NORMAL_OID;
use gravity_decompose::ops::shared::scan::BitmapPlan;
use gravity_decompose::ops::shared::subplan::SubForm;
use gravity_decompose::{Op, PipelineNode};
use types_nodes::jointype::JoinType;
use types_nodes::plannodes::PartitionPruneStepOp;
use mcx::{CollectIn, Mcx, PgVec};
use types_nodes::Node;

use crate::effects::visible::errorhold::quiet_cmp;
use crate::effects::{bitmap_all, initdb_index, quiet_key};
use crate::face::kinds::KindId;
use crate::graph::{EndKind, Gx};
use crate::ops::hashjoin::drain::lower::{drain_of, inner_drain_of};

impl Gx<'_, '_> {
    /// N1's (a) for probe `k` (nest/spool_stream.rs F6: the streamed form rests on it, whatever N1's exemptions become).
    pub(crate) fn spool_quiet(&self, k: usize) -> bool {
        quiet(self, k)
    }

    /// Whether every drain arm `first..=last` is quiet (join/drain.rs refuses the arms by name when not): a scan (a heap
    /// one, an index one whose keys are quiet, or a bitmap one over initdb btree indexes with quiet keys and recheck) or a
    /// Result's one row, every op before its probe, the probe's keys and quals and its run-time pruning's step expressions
    /// raising nothing and calling no user or VOLATILE function, reading no slot but the builds it probes.
    pub(crate) fn quiet_drain_arms(&self, first: usize, last: usize) -> bool {
        (first..=last).all(|q| arm_quiet(self, q))
    }
}

/// Nesting past this many pipelines is not walked: it acts.
pub(crate) const DEEP: u8 = 24;

/// (a) for pipeline `q` (the probe, or one before the drain): D393's quiet probe, the probe its last op, an end that
/// evaluates nothing (a spool), and nothing in it that acts.
pub(crate) fn quiet(gx: &Gx<'_, '_>, q: usize) -> bool {
    let p = &gx.pipelines()[q];
    gx.quiet_drain_arms(q, q) && matches!(p.ops.last(), Some(Op::Join(JoinDesc::Probe { .. }))) && crate::frame::iface::end_breaker(gx, q) == Some(BreakerKind::Materialize) && !pipe_acts(gx, p, q, 0)
}

pub(crate) fn index_acts(gx: &Gx<'_, '_>, indexid: u32, keys: &[gravity_decompose::graph::IndexKey<'_>]) -> bool {
    // D3800 (River's JobSchedule: the Bitmap Index Scan of the unique index under `unique_key IS NOT NULL`): an IS [NOT]
    // NULL key calls no operator (the btree tests the key's null flag, _bt_checkkeys' SK_ISNULL arm)
    let ops = keys.iter().filter(|k| !matches!(k.form, KeyForm::NotNull | KeyForm::Null)).map(|k| k.opno);
    effects::of_index(gx.facts, indexid, ops).acts
}

/// USER: an index a tail nest's (or nested SubPlan's) inner scans whose probes may act: the effects home's `of_index`
/// (a key column's operator class or a key operator that is a user's, D2063's facts; fail closed), at any depth.
pub(crate) fn user_index(gx: &Gx<'_, '_>, o: &Op<'_>, depth: u8) -> bool {
    crate::stack_check();
    let inner = match o {
        Op::Join(JoinDesc::Nest { inner, .. }) => inner,
        Op::SubPlan(s) => match &s.form {
            SubForm::Nested { inner } => inner,
            _ => return false,
        },
        _ => return false,
    };
    let src = match &inner.source {
        Source::Index { indexid, keys, .. } => index_acts(gx, *indexid, keys),
        Source::Bitmap { plan, .. } => user_bitmap(gx, plan),
        _ => false,
    };
    depth > DEEP || src || inner.ops.iter().any(|x| user_index(gx, x, depth + 1))
}

/// USER: a bitmap leaf whose index probes may act (`index_acts`), at any depth of its AND / OR tree.
pub(crate) fn user_bitmap(gx: &Gx<'_, '_>, p: &BitmapPlan<'_>) -> bool {
    crate::stack_check();
    match p {
        BitmapPlan::Index { indexid, keys, .. } => index_acts(gx, *indexid, keys),
        BitmapPlan::And { plans, .. } | BitmapPlan::Or { plans, .. } => plans.iter().any(|x| user_bitmap(gx, x)),
    }
}

/// USER at the drain's end: a group key compared by its type's `=` (`ByRef`) or an opaque min / max whose type's procs may
/// act (the effects home's `of_type`), or a user aggregate. D2133 narrowed (stats_ext#827 / #832: ORDER BY ROW(x.*), a
/// record compare whose column comparators, an anyarray's element type's among them, are looked up at run time): a sort
/// key whose ordering is not a builtin, non-polymorphic operator's (its family's F1, or a DISPATCH compare) does not count.
/// A Sort compares rows the drain has already handed on, in PostgreSQL's plan as in the spool, after (a full sort) or as
/// (a top-N) they arrive, and Gravity's sort compares the pairs of its own choosing whatever order they arrive in (it
/// never made tuplesort's calls): the spool changes only that order (DECISIONS 6b), and the calls' count and order are free
/// (D2134 gone). Under a stop the Sort op is not raise-free (`op_raise_free`), so TAILSTOP keeps a comparator
/// that may raise; a sort in a pipeline between the probe and the drain is N1's (`graph_acts`: `op_acts`' Sort arm).
pub(crate) fn user_end(gx: &Gx<'_, '_>, d: usize) -> bool {
    let type_acts = |t: u32| effects::of_type(gx.facts, t).acts;
    let group = gx.group_at(d).is_some_and(|g| {
        g.keys.iter().any(|k| k.image.is_by_ref() && type_acts(k.tf))
            || g.aggs.iter().any(|a| match a.class {
                Class::User => true,
                Class::Min(Ext::Opaque) | Class::Max(Ext::Opaque) => a.node.as_aggref().is_none_or(|r| type_acts(r.aggtype)),
                _ => false,
            })
    });
    group
}

/// A2END's other half: a sink, a sort or a top-k, a plan Material (D2247), a hash build read only on the way to the
/// statement's own position-free write (D2131), the next right-family join's spool whose drain's end is order-free, or a
/// plain or hashed fold whose every aggregate's result is its input's multiset's (no order-sensitive class, no ORDER BY
/// or DISTINCT, no overflow a partial sum's order decides), or NEW-41 a plain fold's DISTINCT set, or D2136 a grouped one
/// (`distinct_set_free`).
pub(crate) fn order_free(gx: &Gx<'_, '_>, d: usize) -> bool {
    order_free_in(gx, d, false)
}

/// `order_free`, a fold's money or interval sums and averages (`sums_by_order`) taken as free when `sums`: the spooled
/// form's A2END check (spool_admit.rs), whose lowering asks their licence (`sums_end`, drain/lower.rs).
pub(crate) fn order_free_in(gx: &Gx<'_, '_>, d: usize, sums: bool) -> bool {
    order_free_as(gx, d, sums, false)
}

/// `order_free_in`, the statement's sink read as one its reader takes whole when `whole` (D1922's portal-kind fact: the
/// spooled form's A2END check asks it both ways, spool_admit.rs `spool_refusal_as`, and marks `Px::whole_read` where only
/// the whole read holds). D2136 (Hibernate's joined-inheritance loads: chained Hash Right Joins, each drain into the next
/// one's build, the root's projection dividing a float column): a sink taken whole reads every row a drained table's
/// readers hand on, in PostgreSQL's order and in the spool's, so a tail op that may raise meets the same rows in both and
/// raises in one only where it raises in the other (`reader_free`).
pub(crate) fn order_free_as(gx: &Gx<'_, '_>, d: usize, sums: bool, whole: bool) -> bool {
    crate::stack_check();
    let class = |c: Class| order_free_class(c) || sums && sums_by_order(c);
    match gx.end_kind(d) {
        EndKind::Sink | EndKind::Breaker(BreakerKind::Sort | BreakerKind::TopK) => true,
        // a drain under a drain: its rows reach the next join's drain's end in the spool's order (joined, then drained);
        // D3894 (c-random rnd-3255): that drain a nest's embedded inner, through the nest to its owner's end (`rows_free`)
        EndKind::Breaker(BreakerKind::Materialize) if gx.spool_kind(d) == KindId::JoinDrain => gx.pipelines()[d].desc.produces().is_some_and(|m| match drain_of(gx, d, m) {
            Some((e, _)) => order_free_as(gx, e, sums, whole),
            None => inner_drain_of(gx, d, m).is_some_and(|(e, j)| rows_free(gx, e, j + 1, &|q| order_free_as(gx, q, sums, whole))),
        }),
        // D2247: a plan Material stores the rows as they come and its nest inner re-reads them in that order, as a sink hands
        // them on (the joined rows, then the drained build rows in record order where PostgreSQL goes bucket by bucket)
        EndKind::Breaker(BreakerKind::Materialize) => true,
        // D2131: a hash build whose join hands its rows only to the statement's own position-free root write (its probe's
        // spool into its drain, the root, or the root itself): the table's order reaches the write's rows and calls, which
        // RETURNING and the triggers take in any order (DECISIONS 6b); else a table every reader of which takes its order
        // to an order-free end (`table_free`, D2136 narrowed)
        EndKind::Breaker(BreakerKind::HashBuild) => {
            let (ps, root) = (gx.pipelines(), gx.graph.root as usize);
            let to_root = |q: usize| q == root || ps[q].desc.produces().is_some_and(|m| gx.spool_kind(q) == KindId::JoinDrain && drain_of(gx, q, m).is_some_and(|(e, _)| e == root));
            let readers = |s: u8| (d + 1..ps.len()).filter(move |&q| gx.life_of(q, s).is_some_and(|l| l.0 as usize == d));
            ps[d].desc.produces().is_some_and(|s| (gx.free_write(root, usize::MAX).is_some() && readers(s).all(to_root)) || table_free(gx, d, s, whole))
        }
        EndKind::Breaker(BreakerKind::PlainAgg | BreakerKind::HashAgg) => gx.group_at(d).is_some_and(|g| g.aggs.iter().all(|a| call_free(a, &class))),
        // D3731: a hashed SubPlan's set answers membership only (drain/lower.rs `set_fill`)
        EndKind::Breaker(BreakerKind::Distinct) if crate::ops::hashjoin::drain::lower::set_fill(gx, d) => true,
        EndKind::Breaker(BreakerKind::Distinct) => gx.group_at(d).is_some_and(|g| distinct_set_free(g, &class)),
        // D3800: or a write over the drain's held row whose every row for one build record writes the same
        // (spool_admit.rs `held_write`): only the writes' order differs, which RETURNING and the triggers take in any order
        EndKind::RowOp(_) => gx.a2a_write(d).is_some() || gx.held_write(d).is_some(),
    }
}

/// NEW-41: the Distinct breaker a plain aggregate with a DISTINCT call ends in (no group key), in either form: the pair
/// set (keyed.distinct_plain: `GroupForm::Distinct`) or, where the pair set cannot hold it, the plain end over host-sorted
/// DISTINCT calls (fold.plain: regroup.rs re-declares the group plain, each DISTINCT call `FinalForm::Distinct`, folded at
/// the final in its own sort's order). Either keeps one member of each equal run of a DISTINCT argument and folds the
/// set in an order of its own, so the spooled drain's record order reaches the result only through which member an equal
/// run keeps, which ruling X15 leaves free (D2860 undone: any member of an equal run is PostgreSQL's answer), and through
/// the other calls, which fold every row in arrival order: an order-free class each (`order_free_class`). No call has an
/// ORDER BY.
/// D3732 (a) (Django's EmptyFieldListFilter count over a reverse relation: Aggregate -> Unique -> Sort over a Hash Right
/// Anti Join over a Hash Right Join's drain): a Unique over sorted runs (`runs.unique`) with no aggregate keeps one member
/// of each equal run of its keys, which X15 leaves free in the same way, and hands the runs on in key order.
/// D2136 (c-random rnd-3319: a HashAggregate with a hashed count(DISTINCT) per group over a Hash Right Join's drain): the
/// grouped pair set (keyed.distinct_grouped, ops/group/pairs.rs) is the plain one per group: each pair `[group keys.., x]`
/// is kept once, which member of an equal run X15 leaves free, and its riders fold every row of the pair in arrival
/// order, merged per group, an order-free class each. More than one DISTINCT argument (keyed.distinct_multi) and any
/// other group form stay A2END's.
fn distinct_set_free(g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>, class: &dyn Fn(Class) -> bool) -> bool {
    use gravity_decompose::ops::shared::agg::FinalForm;
    let call = |a: &gravity_decompose::ops::shared::agg::AggDecl<'_>| (class(a.class) || collect_free(a)) && a.mods.order == 0;
    match crate::graph::group_kind(g) {
        KindId::KeyedDistinctPlain | KindId::KeyedDistinctGrouped => g.aggs.iter().all(call),
        KindId::RunsUnique => g.aggs.is_empty(),
        KindId::FoldPlain => g.aggs.iter().any(|a| a.mods.distinct > 0) && g.aggs.iter().all(|a| call(a) && (a.mods.distinct == 0 || (a.final_ == FinalForm::Distinct && crate::ops::shared::agg::sorted(a)))),
        _ => false,
    }
}

/// D2136 narrowed (held-out rnd-3270, jcore-0255: a drain into the build of the next hash join over it): hash build `d`'s
/// table `s` takes the drained records in record order where PostgreSQL inserts them bucket by bucket, so a probe of it
/// meets a probe row's matches in another chain order and its drain hands the unmatched records on in another order.
/// Neither shows when every reader of the table (each graph pipeline that consumes `s` from `d`'s life; a nest's
/// embedded inner that does keeps the name) reads it whole into an end that does not show the order (`reader_free`),
/// and probes it raising nothing where the probe may stop at a first match (SEMI, ANTI, RIGHT SEMI, RIGHT ANTI, a unique
/// inner: it compares the records ahead of that match in one order and not in the other).
fn table_free(gx: &Gx<'_, '_>, d: usize, s: u8, whole: bool) -> bool {
    let ps = gx.pipelines();
    let read = |q: usize| ps[q].desc.consumes.contains(&s) && gx.life_of(q, s).is_some_and(|l| l.0 as usize == d);
    if (ps.len()..ps.len() + ps.inners().len()).any(read) {
        return false;
    }
    let first = |o: &Op<'_>| match o {
        Op::Join(JoinDesc::Probe { slot, jointype: JoinType::JOIN_SEMI | JoinType::JOIN_ANTI | JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT_ANTI, .. }) => *slot == s,
        Op::Join(JoinDesc::Probe { slot, unique: true, .. }) => *slot == s,
        _ => false,
    };
    let still = |o: &Op<'_>, subs: &[i32]| crate::effects::op_raise_free(gx, o, Some(subs)) && matches!(o, Op::Join(JoinDesc::Probe { clauses, .. }) if clauses.iter().all(|n| crate::effects::raise_free(gx, n)));
    let mut readers = (d + 1..ps.len()).filter(|&q| read(q)).peekable();
    readers.peek().is_some()
        && readers.all(|q| {
            let ops = &ps[q].ops;
            let subs = subplans(ops, gx.mcx);
            reader_free(gx, q, whole) && ops.iter().filter(|&o| first(o)).all(|o| still(o, &subs))
        })
}

/// `table_free`'s reader `q`: no LIMIT (which rows it reads would follow the table's order), into an end that reads every
/// row before it hands one on and is `order_free` (a sort, a top-N, an order-free fold, a plain fold's DISTINCT set:
/// D3220, a count(DISTINCT) over the RIGHT SEMI drain of a drained build; another such hash build), into the next
/// right-family join's spool whose drain is as free, or into a sink (a cursor may stop it) through ops that all raise
/// nothing (`op_raise_free`; a Drain op evaluates nothing: D3732): which rows it handed on before the stop is free
/// (DECISIONS 5 (a)). A sink its reader takes whole (`whole`, D2136) takes any ops: no stop decides which rows reach them.
fn reader_free(gx: &Gx<'_, '_>, q: usize, whole: bool) -> bool {
    crate::stack_check();
    let ops = &gx.pipelines()[q].ops;
    let subs = subplans(ops, gx.mcx);
    !ops.iter().any(|o| crate::graph::step_kind(o) == KindId::StepLimit)
        && match gx.end_kind(q) {
            EndKind::Breaker(BreakerKind::Sort | BreakerKind::TopK | BreakerKind::PlainAgg | BreakerKind::HashAgg | BreakerKind::Distinct | BreakerKind::HashBuild) => order_free_as(gx, q, false, whole),
            EndKind::Breaker(BreakerKind::Materialize) if gx.spool_kind(q) == KindId::JoinDrain => gx.pipelines()[q].desc.produces().is_some_and(|m| match drain_of(gx, q, m) {
                Some((e, _)) => reader_free(gx, e, whole),
                None => inner_drain_of(gx, q, m).is_some_and(|(e, j)| rows_free(gx, e, j + 1, &|r| reader_free(gx, r, whole))),
            }),
            // D3732 (TypeORM's and Rails' chained LEFT JOINs with no ORDER BY: a Hash Right Join's drain into the next Hash
            // Right Join's build): a reader that is the table's own drain hands its unmatched records on through its Drain
            // op, which evaluates nothing (N1's `still` reads it so too)
            // D2136: or a sink its reader takes whole (`whole`): every row reaches its ops in either order, so one that may
            // raise does so in both or in neither (the statement fails either way, its error raised from whichever row comes first)
            EndKind::Sink => whole || ops.iter().all(|o| matches!(o, Op::Join(JoinDesc::Drain { .. })) || crate::effects::op_raise_free(gx, o, Some(&subs[..]))),
            // D3894 (c-random rnd-4163: a Hash Join over the drained build under the Material a nest loop re-reads): a plan
            // Material's store (nest.spool) keeps the rows in the order they come, the order D2247's arm of `order_free_in`
            // reads as a sink's; here every reader of the store must take it as free (`store_free`)
            EndKind::Breaker(BreakerKind::Materialize) if gx.spool_kind(q) == KindId::NestSpool => gx.pipelines()[q].desc.produces().is_some_and(|m| store_free(gx, q, m, whole)),
            _ => false,
        }
}

/// D3894: `reader_free`'s plan Material `q` (store `m`): each reader of the store (a pipeline whose source is `m` of `q`'s
/// life: a nest's embedded inner, or a graph pipeline) re-reads every row in store order, so every reader's rows must
/// reach an end that does not show it (`rows_free`, up to a graph pipeline `reader_free` takes with the same `whole`).
fn store_free(gx: &Gx<'_, '_>, q: usize, m: u8, whole: bool) -> bool {
    let ps = gx.pipelines();
    let reads = |j: usize| matches!(ps[j].source, Source::Breaker { slot } if slot == m && gx.life_of(j, slot).is_some_and(|l| l.0 as usize == q));
    let mut readers = (0..ps.len() + ps.inners().len()).filter(|&j| reads(j)).peekable();
    readers.peek().is_some() && readers.all(|j| rows_free(gx, j, 0, &|e| reader_free(gx, e, whole)))
}

/// D3894: whether the rows pipeline `j` reads, from its op `from` on, reach an end that does not show their order. Every op
/// on the way hands each row on by itself: a filter, gate, projection, SubPlan or another join (no LIMIT, window, run or
/// set op that reads the order). A full sort or top-N on the way (no presorted prefix: an ORDER BY aggregate PostgreSQL
/// presorted, an ORDER BY .. LIMIT) hands the rows on in its keys' order, PG's but for the rows tied on every key, an
/// order that is free (D1428: an aggregate's input; a LIMIT's ties; window peers), so the walk ends there. A graph
/// pipeline's end is `at_end`'s. An embedded inner hands its rows to its owner past the nest op when the nest stops at no
/// first match (an inner or left nest, not unique: PG's ExecNestLoop then reads the whole inner for each outer row, as
/// the inner's reader does here, so the same pairs meet its qual in another order); a nested SubPlan's inner (it may stop
/// at its first row) and a nest that stops keep A2END.
fn rows_free(gx: &Gx<'_, '_>, mut j: usize, mut from: usize, at_end: &dyn Fn(usize) -> bool) -> bool {
    use gravity_decompose::ops::shared::sort::SortForm;
    let ps = gx.pipelines();
    let (n, inners) = (ps.len(), ps.inners());
    loop {
        for o in ps[j].ops.get(from..).unwrap_or(&[]) {
            match o {
                Op::Sort(s) if s.presorted == 0 && matches!(s.form, SortForm::Runs | SortForm::TopK { .. }) => return true,
                Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::SubPlan(_) | Op::Join(JoinDesc::Probe { .. } | JoinDesc::Nest { .. }) => {}
                _ => return false,
            }
        }
        if j < n {
            return at_end(j);
        }
        let Some(&((o, at), _)) = inners.get(j - n) else { return false };
        if !matches!(ps[o].ops.get(at), Some(Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER | JoinType::JOIN_LEFT, unique: false, .. }))) {
            return false;
        }
        (j, from) = (o, at + 1);
    }
}

/// A2END's call `a` of a plain or hashed fold end: its class order-free (`class`: `order_free_class`, with D3637's sums
/// where the caller takes them) or a collect `collect_free` takes, and no DISTINCT. D3894 (c-random rnd-4163: a
/// string_agg(.. ORDER BY ..) FILTER over a drained build): an ORDER BY of the call's own too, unless PostgreSQL presorted
/// it (aggpresorted: a Sort under the Agg, so the call reads its input order). PG's nodeAgg sorts the group's rows by it
/// before the call folds them, as the call's host sort does here, so the spool's order shows only among the rows tied on
/// every ORDER BY key, which is free (D1428: any order among an aggregate ORDER BY's ties, checked tie-aware).
fn call_free(a: &gravity_decompose::ops::shared::agg::AggDecl<'_>, class: &dyn Fn(Class) -> bool) -> bool {
    (class(a.class) || collect_free(a)) && a.mods.distinct == 0 && (a.mods.order == 0 || !gravity_decompose::facts::order::presorted_call(a))
}

/// A2END's classes (design r2 item 9; review r1 F2: every class named, so a new one is decided here): the folds whose value
/// and whose error-free run do not depend on the rows' order; money and interval (an overflow their partial sums meet:
/// D3637's licence, `sums_by_order`), bit strings (a length-mismatch raise), collections but range_agg (`collect_free`
/// takes some) and user folds are not. D3894: the rest by the one order-free rule (decompose facts/order.rs
/// `order_free`). The float folds (sums, averages and variances over float4 / float8): a float aggregate may fold in any
/// order (DECISIONS.md 11 (d); breakers.md's last-bit drift of a float sum), and outside the plan-order run its overflow
/// raises wherever some order of the rows could overflow (class_float.rs D2597, under the statement's recheck licence:
/// decompose `license_raising`), so the plan-order re-run, where the one rule keeps PostgreSQL's order and A2END names the
/// spooled form again, decides it. The ordered-set and hypothetical-set calls sort their own input (WITHIN GROUP), and
/// range_agg's multirange is canonical: the same value in any input order.
fn order_free_class(c: Class) -> bool {
    use gravity_decompose::ops::shared::agg_class::{Collect, Mods, Split};
    const BARE: Mods = Mods { distinct: 0, order: 0, filter: false, direct: 0, split: Split::Simple, star: false };
    match c {
        Class::CountStar | Class::CountAny | Class::CountPair | Class::SumI64 | Class::SumI128 | Class::SumNum | Class::AvgI128 | Class::AvgNum => true,
        Class::Min(_) | Class::Max(_) | Class::BoolAnd | Class::BoolOr | Class::BitI64(_) | Class::Stat3I128(_) | Class::Stat3Num(_) => true,
        Class::SumF32 | Class::SumF64 | Class::AvgF64 | Class::Stat3F64(_) | Class::Stat6F64(_) => gravity_decompose::facts::order::order_free(c, &BARE),
        Class::OrderedSet(_) | Class::Hypothetical(_) | Class::Collect(Collect::Range) => gravity_decompose::facts::order::order_free(c, &BARE),
        Class::SumMoney | Class::SumInterval | Class::AvgInterval | Class::BitVar(_) => false,
        Class::Collect(_) | Class::User => false,
    }
}

/// D3732: an aggregate with no ORDER BY that collects its rows may fold a spooled drain's rows in the spool's order (the
/// joined rows in probe order, then the drained records in record order, where PostgreSQL goes bucket by bucket): its
/// value is then a permutation of PostgreSQL's per-group input, which the rig checks as exactly that
/// (scripts/gravity-gate/multiset_elem.py: the elements, or string_agg's pieces between its constant delimiter, as a
/// multiset; NULLs skipped as PostgreSQL skips them). The collects whose value is such a sequence: array_agg (of scalars
/// or of arrays), json_agg / jsonb_agg (and their _strict forms), and string_agg whose delimiter is a constant (a
/// delimiter per row moves with the order: the first row's is the one dropped). No DISTINCT (its emit order is the
/// table's, not the spool's); an aggregate ORDER BY keeps its sort, the spool's order then showing only among its ties
/// (`call_free`, D3894). The object
/// collects (jsonb_object_agg keeps a key's last pair), xmlagg, range_intersect_agg, any_value and every user-defined
/// aggregate stay A2END's (range_agg and the ordered-set calls are `order_free_class`'s).
fn collect_free(a: &gravity_decompose::ops::shared::agg::AggDecl<'_>) -> bool {
    use gravity_decompose::ops::shared::agg_class::Collect as C;
    let Some(r) = a.node.as_aggref() else { return false };
    if a.mods.distinct != 0 {
        return false;
    }
    match a.class {
        Class::Collect(C::Array | C::ArrayOfArrays | C::Json) => true,
        Class::Collect(C::String) => r.args.iter().filter_map(|t| t.as_target_entry()).nth(1).is_some_and(|t| t.expr.as_const().is_some()),
        _ => false,
    }
}

/// D3637: the folds whose value is their input's multiset's in any order and whose order shows only in an
/// overflow the partial sums meet: money and interval sums and averages (cash_pl, interval_pl). Over a spooled drain the
/// records reach them in record order, PostgreSQL's bucket by bucket.
fn sums_by_order(c: Class) -> bool {
    matches!(c, Class::SumMoney | Class::SumInterval | Class::AvgInterval)
}

/// D3637: the group spooled drain `d`'s rows fold into (through drains under drains) when that end is
/// order-free but for its `sums_by_order` calls: the end pipeline and the group. Its lowering takes those calls only under
/// their licence (`effects::drain_sums`), else A2END names it.
pub(crate) fn sums_end<'g, 'p>(gx: &Gx<'g, 'p>, d: usize) -> Option<(usize, &'g gravity_decompose::ops::shared::agg::GroupDecl<'p>)> {
    crate::stack_check();
    match gx.end_kind(d) {
        EndKind::Breaker(BreakerKind::Materialize) if gx.spool_kind(d) == KindId::JoinDrain => {
            gx.pipelines()[d].desc.produces().and_then(|m| drain_of(gx, d, m)).and_then(|(e, _)| sums_end(gx, e))
        }
        EndKind::Breaker(BreakerKind::PlainAgg | BreakerKind::HashAgg | BreakerKind::Distinct) if !order_free_in(gx, d, false) && order_free_in(gx, d, true) => {
            gx.group_at(d).map(|g| (d, g))
        }
        _ => None,
    }
}

/// An operator that is not a builtin, non-polymorphic one calling a builtin function.
fn user_op(opno: u32) -> bool {
    !builtin_oper(opno).is_some_and(|p| !p.poly && p.code != 0 && p.code < FIRST_NORMAL_OID)
}

/// A hash clause whose equality or hash procs may be a user's, both sides as PG's get_op_hash_functions resolves them: no
/// operator clause, an operator or function past the builtin catalog, an operator in no builtin hash family (PG takes the
/// first family holding it, a builtin one first) or not calling its oprcode, or a polymorphic / record equality (array,
/// record, range, multirange: its procs run the element, field or subtype's; D2068's class) over an operand whose type
/// `type_free` does not read free. D3741 (pg_dump's getTableAttrs: `co.conkey = array[a.attnum]`, int2[] = int2[]): over
/// free operands (arrays, composites and builtin ranges of builtin base types or enums, through domains) array_eq /
/// hash_array, record_eq / hash_record and range_eq / hash_range run only initdb's procs per element, field or bound, as
/// PG's typcache resolves them. The one copy both drain files read (drain/arms.rs `user_drain_arms`, `op_acts` here); the
/// home's answer, `Fx::user_clause`.
pub(crate) fn user_clause(gx: &Gx<'_, '_>, c: Node<'_>) -> bool {
    gx.fx(c).user_clause()
}

/// D3730 (Django's `id__in` / `Exists(..)` over a LEFT join, D2129): a hashed SubPlan's set fill (`SubForm::Build`, the
/// producer of `useHashTable`'s identity set) hashes each row it reads by the test's operators' hash procs and compares by
/// the right-hand type's equality (PG's buildSubPlanHash: `tab_hash_funcs` and `tab_eq_funcs` from get_op_hash_functions and
/// get_compatible_hash_operators), and calls nothing else of its own. It acts when one of those may be a user's: by
/// `user_clause` over each operator clause of the test (a row comparison's AND of them, nodeSubplan.c's per-OpExpr loop);
/// a test of any other form acts. The test's own expressions are walked beside it (`op_acts`' `s.test`).
fn set_user(gx: &Gx<'_, '_>, t: Node<'_>) -> bool {
    match t.as_bool_expr() {
        Some(b) if b.boolop == types_nodes::primnodes::BoolExprType::AND_EXPR => b.args.iter().any(|c| user_clause(gx, c)),
        _ => user_clause(gx, t),
    }
}

/// The SubPlan ids a pipeline's ops evaluate: an expression reading one of them is walked at its op.
pub(crate) fn subplans<'m>(ops: &[Op<'_>], mcx: Mcx<'m>) -> PgVec<'m, i32> {
    ops.iter().filter_map(|o| if let Op::SubPlan(s) = o { Some(s.plan_id) } else { None }).collect_in(mcx)
}

/// Whether evaluating `n` may act (F5's coverage): see the file's head. `subs`: the SubPlans its pipeline's ops evaluate.
/// effects/volatile.rs `walk` with `spool(subs)`: a call that is user code, may raise a NOTICE or acts by its own row
/// (functions, operators, a hashed array op's hash and negator functions, row comparisons, aggregates, window functions, a
/// cast's I/O functions), GREATEST / LEAST over a type whose compare is not `cmp_free`, a SubPlan no op of the pipeline
/// evaluates, a domain cast, a sequence's nextval, the XML / JSON nodes, and any node the walk cannot see.
fn acts(gx: &Gx<'_, '_>, n: Node<'_>, subs: &[i32]) -> bool {
    gx.fx(n).volatile_by(&crate::effects::volatile::Reading::spool(subs))
}

/// The graph pipelines one `spool_refusal` may walk (review r1 lens 0: rerun sets and breaker feeders share pipelines, so
/// depth alone does not bound the walk); past it the walk acts (fail closed).
const VISITS: u32 = 4096;

leader_tls::leader_tls! {
    thread_local! {
        /// This `spool_refusal`'s walked graph pipelines.
        pub(crate) static SEEN: core::cell::Cell<u32> = const { core::cell::Cell::new(0) };
    }
}

/// Graph pipeline `q`'s source, ops and end aggregates.
pub(crate) fn graph_acts(gx: &Gx<'_, '_>, q: usize, depth: u8) -> bool {
    let seen = SEEN.with(|c| {
        c.set(c.get().saturating_add(1));
        c.get()
    });
    seen > VISITS || depth > DEEP || pipe_acts(gx, &gx.pipelines()[q], q, depth) || group_acts(gx, q)
}

/// The aggregates pipeline `q`'s end folds (their arguments, FILTER and function).
fn group_acts(gx: &Gx<'_, '_>, q: usize) -> bool {
    // an argument reading a SubPlan the pipeline's own op evaluates (a pulled-up subquery's column) is walked at that op
    let subs = subplans(&gx.pipelines()[q].ops, gx.mcx);
    gx.group_at(q).is_some_and(|g| g.aggs.iter().any(|a| a.class == Class::User || acts(gx, a.node, &subs)))
}

/// Pipeline `p`'s source and ops, `at` the graph pipeline it runs in (a breaker it reads is `at`'s life).
pub(crate) fn pipe_acts(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, at: usize, depth: u8) -> bool {
    let subs = subplans(&p.ops, gx.mcx);
    let a = |n: Node<'_>| acts(gx, n, &subs);
    let src = match &p.source {
        Source::Range { .. } | Source::Const | Source::Breaker { .. } => false,
        Source::Index { keys, recheck, indexid, .. } => index_acts(gx, *indexid, keys) || keys.iter().any(|x| a(x.value)) || recheck.iter().any(a),
        Source::Bitmap { plan, recheck, .. } => user_bitmap(gx, plan) || recheck.iter().any(a),
        Source::Values { lists, .. } => lists.iter().any(a),
        _ => true,
    };
    src || depth > DEEP || p.ops.iter().any(|o| op_acts(gx, o, &subs, at, depth))
}

/// An embedded inner (a nest's, a nested SubPlan's): its pipeline, and what it reads through a breaker source (a producer
/// numbered past `at` runs on demand inside it, a rerun set's member: walked like any other; `at` itself, a cycle, acts).
fn inner_acts(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, at: usize, depth: u8) -> bool {
    let fed = |s: u8| crate::ops::hashjoin::build_class::feed(gx, at, core::iter::once(s)).into_iter().any(|x| x == at || graph_acts(gx, x, depth + 1));
    matches!(p.source, Source::Breaker { slot } if fed(slot)) || pipe_acts(gx, p, at, depth + 1)
}

/// The members of the rerun sets of a nest (`plan_id` 0, its `node`) or a SubPlan (`plan_id`), each walked wherever it is
/// numbered (`at` itself acts).
fn members_act(gx: &Gx<'_, '_>, node: i32, plan_id: i32, at: usize, depth: u8) -> bool {
    let set = |r: &&gravity_decompose::graph::Rerun| if plan_id == 0 { r.plan_id == 0 && r.node_id == node } else { r.plan_id == plan_id };
    gx.graph.reruns.iter().filter(set).flat_map(|r| r.pipelines.iter()).any(|&x| x as usize == at || graph_acts(gx, x as usize, depth + 1))
}

/// Whether op `o` of graph pipeline `at` (or of an inner it runs) may act; an op kind not listed does.
fn op_acts(gx: &Gx<'_, '_>, o: &Op<'_>, subs: &[i32], at: usize, depth: u8) -> bool {
    crate::stack_check();
    let a = |n: Node<'_>| acts(gx, n, subs);
    let fed = |s: u8| crate::ops::hashjoin::build_class::feed(gx, at, core::iter::once(s)).into_iter().any(|x| x == at || graph_acts(gx, x, depth + 1));
    match o {
        Op::Filter(n) | Op::Gate(n) => a(*n),
        Op::Project(l) => l.iter().any(a),
        Op::Limit { count, offset, .. } => [count, offset].into_iter().any(|b| matches!(*b, Bound::Expr(n) if a(n))),
        Op::Sort(s) => s.keys.iter().any(|k| k.own_family || user_op(k.opno)),
        Op::Join(JoinDesc::Drain { .. } | JoinDesc::Spool) => false,
        Op::Join(JoinDesc::Build { keys, .. }) => keys.iter().any(a),
        Op::Join(JoinDesc::Probe { clauses, keys, qual, .. }) => keys.iter().chain(qual.iter()).any(a) || clauses.iter().any(|c| user_clause(gx, c)),
        Op::Join(JoinDesc::Memo { keys, ops, .. }) => keys.iter().any(a) || ops.iter().any(|&x| user_op(x)),
        Op::Join(JoinDesc::Nest { params, inner, qual, facts, .. }) => {
            let param = |p: Node<'_>| p.as_nest_loop_param().is_none_or(|p| a(p.paramval));
            facts.volatility == Volatility::Volatile || params.iter().any(param) || qual.iter().any(a) || inner_acts(gx, inner, at, depth) || members_act(gx, facts.node, 0, at, depth)
        }
        Op::SubPlan(s) => {
            let form = match &s.form {
                SubForm::Nested { inner } => inner_acts(gx, inner, at, depth),
                SubForm::Hashed { slot } => fed(*slot),
                SubForm::Build => s.test.is_none_or(|t| set_user(gx, t)),
            };
            s.volatility == Volatility::Volatile || s.test.is_some_and(a) || s.args.iter().any(a) || form || members_act(gx, 0, s.plan_id, at, depth)
        }
        Op::Srf(s) => s.functions.iter().any(|f| a(f.node)),
        Op::Window(_) => crate::effects::window_acts(o, &a, &user_op),
        Op::Write(_) => gx.a2a_write(at).is_none_or(|j| !core::ptr::addr_eq(o, &gx.pipelines()[at].ops[j])),
        Op::Lock(_) => gx.free_lock(at, at).is_none_or(|j| !core::ptr::addr_eq(o, &gx.pipelines()[at].ops[j])),
        // rows moved, nothing called
        _ if crate::ops::hashjoin::drain::lower::bare_append(o) => false,
        _ => true,
    }
}


/// `quiet`'s source: a heap scan, a Result's one row (its target list is the arm's projection), an index scan whose keys
/// and recheck are `still`, or a bitmap one over initdb btree indexes with quiet keys and a `still` recheck.
pub(crate) fn source_still(gx: &Gx<'_, '_>, p: &gravity_decompose::PipelineNode<'_>, still: &dyn Fn(Node<'_>) -> bool) -> bool {
    match &p.source {
        Source::Range { .. } | Source::Const => true,
        Source::Index { keys, recheck, .. } => keys.iter().map(|x| x.value).chain(recheck.iter()).all(still),
        // D393's bitmap arms: a bitmap PG builds whole before the arm's first row (MultiExecBitmapIndexScan) over
        // btree indexes whose opclasses and key operators are initdb ones that raise on no input (`initdb_index`, D2063's
        // facts), its keys quiet (`quiet_key`; `col = ANY (array)` too: one probe per element under the same leakproof
        // operator, PG 18's OR clauses on one column), its recheck quiet: a partition's bitmap scan arm (held-out rnd-0186)
        Source::Bitmap { plan, recheck, .. } => {
            let key = |x: &IndexKey<'_>| quiet_key(gx, &IndexKey { form: if x.form == KeyForm::Any { KeyForm::Op } else { x.form }, ..*x }, &|_| false) && still(x.value);
            bitmap_all(plan, &|i, k, am| am == IndexAm::Btree && initdb_index(gx, i, k) && k.iter().all(key)) && recheck.iter().all(still)
        }
        _ => false,
    }
}

/// `Gx::quiet_drain_arms`'s test of arm `q` (drain/arms.rs's `quiet`, moved as it was).
fn arm_quiet(gx: &Gx<'_, '_>, q: usize) -> bool {
    let p = &gx.pipelines()[q];
    let still = |n: Node<'_>| (gx.fx(n).quiet() || quiet_cmp(gx, n)) && !gx.fx(n).user_calls() && !gx.fx(n).volatile_any();
    let src = source_still(gx, p, &still);
    let Some(j) = p.ops.iter().rposition(|o| matches!(o, Op::Join(JoinDesc::Probe { .. }))) else { return false };
    src && p.desc.consumes.iter().all(|s| p.ops.iter().any(|o| matches!(o, Op::Join(JoinDesc::Probe { slot, .. }) if slot == s)))
        && p.ops[..=j].iter().all(|op| match op {
            Op::Filter(n) | Op::Gate(n) => still(*n),
            Op::Project(l) => l.iter().all(still),
            Op::Append(AppendDesc::Arm { prune, .. }) => prune.as_ref().is_none_or(|d| steps(d, &mut |o| o.exprs.iter().all(still))),
            Op::Join(JoinDesc::Probe { keys, qual, .. }) => keys.iter().chain(qual.iter()).all(still),
            _ => false,
        })
}

/// Every PartitionPruneStepOp of prune `d` (each PartitionedRelPruneInfo level's initial and exec steps) passes `f`.
pub(crate) fn steps(d: &PruneDesc<'_>, f: &mut dyn FnMut(&PartitionPruneStepOp<'_>) -> bool) -> bool {
    let mut levels = d.info.prune_infos.iter().flat_map(|l| l.as_list().into_iter().flat_map(|l| l.iter()));
    levels.all(|r| r.as_partitioned_rel_prune_info().is_some_and(|r| r.initial_pruning_steps.iter().chain(r.exec_pruning_steps.iter()).all(|s| s.as_partition_prune_step_op().is_none_or(&mut *f))))
}

/// D1752 under DECISIONS 6b: drain `d`'s order decides which of its tail's calls run when the tail holds a LIMIT (`limit`),
/// or when its end is not a breaker every row reaches (a sink: a cursor or a portal may stop it early; a row op: its
/// writes and their triggers run in the drain's order, row by row with the statement's later reads). A breaker end (a
/// fold, a sort, a hash build) takes every row before anything reads it: every call runs, in an order PG's plan decides.
pub(crate) fn order_picks_calls(gx: &Gx<'_, '_>, d: usize, limit: bool) -> bool {
    limit || !matches!(gx.end_kind(d), EndKind::Breaker(_))
}

impl Gx<'_, '_> {
    /// Whether some drain arm `first..=last` runs a support function that may be a user's (join/drain.rs's USER_KEY, until
    /// GUARD's shared predicate lands): PG runs it only for the arms it runs, and how often v4 does
    /// is not PG's count (D2091). An index arm whose scan may reach a user's or a raising function (the effects home's
    /// `initdb_index`: D2063's IndexOpc fact over every key column's operator class, whose btree FUNCTION 1 `_bt_first`
    /// calls, and each key operator; unknown fails closed: D2133 narrowed, a user table's index over builtin opclasses no
    /// longer counts), a run-time pruning step's support function that is no builtin one of a leaf type (S2b's
    /// quiet_prune rule), or a hash clause of any probe in the arm (`user_clause`).
    pub(crate) fn user_drain_arms(&self, first: usize, last: usize) -> bool {
        (first..=last).any(|q| {
            let p = &self.pipelines()[q];
            matches!(&p.source, Source::Index { indexid, keys, .. } if !initdb_index(self, *indexid, keys))
                || p.ops.iter().any(|op| match op {
                    Op::Append(AppendDesc::Arm { prune: Some(d), .. }) => !steps(d, &mut |o| o.cmpfns.iter().all(crate::effects::builtin_fn)),
                    Op::Join(JoinDesc::Probe { clauses, .. }) => clauses.iter().any(|c| user_clause(self, c)),
                    _ => false,
                })
        })
    }
}
