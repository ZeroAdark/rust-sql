//! A union's two forms (decision 73): a streamed union's arms carry the parent's ops, or the union settles into one
//! pipeline over a Materialize slot every arm feeds. Any family that closes, spools or embeds an open asks here.
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
use gravity_program::{ArenaRef, ArenaVec, End, LifeId, Pipeline, Reason};

use crate::admit::Refusal;
use crate::breakers;
use crate::graph::{NodeOps, Op, Order, PipelineGraph, Source};
use crate::ledger::Kind;
use crate::ops::shared::append::{AppendDesc, PruneDesc};
use crate::ops::shared::sort::ROW_LAYOUT;
use crate::walk::Open;

/// The desc an open over a union starts with: the one an emit over the union's slot had (serial, no driving site).
pub(crate) const BASE: Pipeline = Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite);

/// Whether the union streams into `c`'s ops and end: every op copies per arm, `c`'s desc is the base one past what an
/// end sets, and no arm (an inner union's arms included) reads a slot `c` reads. `held`: the check option SubPlans whose
/// per-arm copies are mapped (nested_arms.rs `copy_sets`).
pub(crate) fn streams<'p>(g: &PipelineGraph<'p>, c: &Open<'p>, arms: &[Open<'p>], ordered: bool, held: &dyn Fn(i32) -> bool) -> bool {
    let d = &c.desc;
    let serial = d.parallel == BASE.parallel || owner(c).is_some_and(|r| d.parallel == owned(r).parallel);
    let base = serial && d.driving_site == BASE.driving_site && d.predicted_rows == BASE.predicted_rows && !d.finish;
    base && c.ops.iter().all(|op| copies(op, ordered, arms.len()) || crate::nested_arms::per_arm(g, op, &c.ops, held) || crate::arm_nests::per_arm(g, op, &c.ops, arms)) && !arms.iter().any(|a| reads_any(g, a, d.consumes))
}

/// A row-owning op's open over its union once `write.rs` made it serial (`serial_after`, no driving site): the op owns
/// each row of the arm it came from. L2 (V4R-248): a LockRows over one partitioned / inheritance family
/// (`LockDesc::family`) streams into its Append's arms, one per leaf, so each arm's lock takes its rows in its own leaf
/// (ExecLockRows' per-child ExecRowMark) and EvalPlanQual reruns the arm. lockrows FAMJOIN: so does one beside a join's
/// REFERENCE marks (`LockDesc::family_top`: FOR UPDATE OF the family, the Append the probe side of a hash join), each
/// arm then the lock over a join (D655). The WRITE half (an UPDATE / DELETE of several
/// result relations, M4-ROUTE R2) is one more arm of `owner` with `Reason::Write`.
const fn owned(reason: Reason) -> Pipeline {
    Pipeline::serial(0, End::Sink, reason)
}

/// The serial reason of the row-owning op among `c`'s ops that its union streams into (`owned`), if any.
fn owner(c: &Open<'_>) -> Option<Reason> {
    c.ops.iter().find_map(|op| match op {
        Op::Lock(l) if l.family_top().is_some() => Some(Reason::Lock),
        // V4R-214: an UPDATE / DELETE of several result relations (inheritance, a partitioned target not pruned to one
        // leaf): each arm is one target's rows (its `tableoid`), so the write streams into every arm, ModifyTable's
        // per-row `ExecLookupResultRelByOid` over its child's rows in arm order; the arm's own site is the row's target
        Op::Write(_) if streamed_write(op) => Some(Reason::Write),
        _ => None,
    })
}

/// The write of several result relations a union streams into (`owner` and `copies` alike): an UPDATE / DELETE (V4R-214),
/// or (W6-MERGE-ROUTE-MATCHED) a MERGE whose actions update or delete and never insert (a NOT MATCHED INSERT routes
/// through the root beside the target host: lowerv4 write/merge_route.rs refuses it by name over the Materialize form).
fn streamed_write(op: &Op<'_>) -> bool {
    use gravity_program::Cmd;
    let Op::Write(w) = op else { return false };
    let merge = w.merge.as_ref().is_some_and(|m| m.actions.iter().any(|a| matches!(a.cmd, Some(Cmd::Update | Cmd::Delete))) && m.actions.iter().all(|a| a.cmd != Some(Cmd::Insert)));
    w.targets.len() > 1 && (matches!(w.cmd, Cmd::Update | Cmd::Delete) || w.cmd == Cmd::Merge && merge)
}

fn reads_any<'p>(g: &PipelineGraph<'p>, a: &Open<'p>, slots: &[u8]) -> bool {
    crate::walk::stack_check(); // D3748
    a.desc.consumes.iter().any(|s| slots.contains(s)) || a.union.is_some_and(|u| g.unions[u as usize].arms.iter().any(|x| reads_any(g, x, slots)))
}

/// `d` reading every slot `c` reads, the lives named when both name theirs.
pub(crate) fn consume_all(arena: ArenaRef, mut d: Pipeline, c: &Pipeline) -> Pipeline {
    let named = c.consumes_from.len() == c.consumes.len();
    for (j, &s) in c.consumes.iter().enumerate() {
        if named && d.consumes_from.len() == d.consumes.len() {
            d = d.consuming_from_in(arena, s, c.consumes_from[j]);
        } else {
            d = d.consuming_in(arena, s);
            d.consumes_from = &[];
        }
    }
    d
}

/// The ops an arm can carry a copy of: row ops and the ops whose state is the union's (a limit, a probe of one build,
/// a probe of a hashed subplan's set, a lock, a full sort's keys: every arm's run merges into the one slot, V4R-159); a
/// nest's or nested subplan's inner, a walk, an unbounded incremental sort, window, set op, write, build (a hashed
/// subplan's set too, D393: its keyed end has no owner phase for the arms to continue) or another union's feed keep the
/// union in its Materialize form. D2077: a bounded incremental sort over an ordered union copies too: the arms
/// arrive in its prefix order one after another, so its top (one run, its stop words per union) reads arm 0 and stops
/// where PostgreSQL's does, not after every arm fed the Materialize slot. A top with no presorted key (PG's Limit -> Sort
/// over an Append, ordered or not) copies too over a union of `UNION_TOP_ARMS_MIN` arms or more (the cost home's
/// measured crossover): each arm's run cuts to its own top and exec `ArmSort` merges them, as a full sort's (per-arm
/// tops hold the union's top).
fn copies(op: &Op<'_>, ordered: bool, arms: usize) -> bool {
    use crate::join::JoinDesc;
    use crate::ops::shared::subplan::SubForm;
    use gravity_program::cost::UNION_TOP_ARMS_MIN;
    match op {
        Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::Limit { .. } | Op::Srf(_) | Op::Lock(_) => true,
        Op::Join(JoinDesc::Probe { .. }) => true,
        Op::SubPlan(d) => matches!(d.form, SubForm::Hashed { .. }),
        Op::Append(AppendDesc::Arm { .. }) => true,
        // D1071: a streamed group's one open group is the union's (keyed/stream.rs: its words are per union, the last arm
        // hands the open group on); an ordered union only (`push_union`)
        Op::GroupRun(_) => true,
        Op::Sort(d) => match d.form {
            crate::ops::shared::sort::SortForm::Runs => d.presorted == 0,
            crate::ops::shared::sort::SortForm::TopK { .. } => {
                (ordered && d.presorted > 0) || (d.presorted == 0 && arms >= UNION_TOP_ARMS_MIN)
            }
            crate::ops::shared::sort::SortForm::Walk { .. } => false,
        },
        Op::Write(_) => streamed_write(op),
        _ => false,
    }
}

/// The union's Materialize form (a nest's or nested subplan's inner, a spool, an InitPlan, an op no arm can copy):
/// each arm feeds one slot, the returned emit over it carries `Union` and `c`'s ops and end.
pub(crate) fn settle_arms<'p>(g: &mut PipelineGraph<'p>, mut c: Open<'p>, arms: ArenaVec<Open<'p>>, order: Order, prune: Option<PruneDesc<'p>>) -> Open<'p> {
    crate::walk::stack_check();
    let arena = *g.pipelines.allocator();
    // D2369 (CAP-33): a checked count as wide as the Append's (map_append refuses past u16::MAX children), not a u8
    let n = u16::try_from(arms.len()).unwrap_or_else(|_| {
        g.fault.get_or_insert(Refusal::shape(Kind::Append, c.node_id, "more than 65535 children of a union kept in its Materialize form"));
        u16::MAX
    });
    let mut slot = None;
    for a in arms {
        // an inner union settles first and the slot is the lowest free at the first feed, as the child-by-child form chose
        let mut a = settle(g, a);
        let s = *slot.get_or_insert_with(|| feed_slot(g, &a, &c));
        a.node_ops.push(NodeOps { node_id: c.node_id, first_op: a.ops.len() as u16, producer_top: true });
        a.push(Op::Append(AppendDesc::Feed { slot: s, ordered: order.is_known() }));
        let breaker = Breaker { kind: BreakerKind::Materialize, slot: s, bytes: 0, budget: Budget::Share, spill: Spill::Runs };
        breakers::declare_producers(g, a, breaker, ROW_LAYOUT, order);
    }
    let slot = slot.unwrap_or(0);
    g.reads[slot as usize] += 1; // the emit's read (`consuming`)
    let life = breakers::decl_of(g, slot).map_or(0, |d| d.producer);
    let reads = consume_all(arena, BASE.consuming_from_in(arena, slot, LifeId(life)), &c.desc);
    let desc = Pipeline { consumes: reads.consumes, consumes_from: reads.consumes_from, ..c.desc };
    let mut emit = Open::new(arena, c.node_id, Source::Breaker { slot }, desc);
    emit.push(Op::Append(AppendDesc::Union { slot, children: n, prune }));
    emit.ops.extend(c.ops.drain(..));
    emit.node_ops.extend(c.node_ops.iter().enumerate().map(|(k, x)| NodeOps { first_op: if k == 0 { 0 } else { x.first_op + 1 }, ..*x }));
    emit.mark = c.mark + 1;
    emit
}

/// The union's slot: the lowest free one, not one `c` reads or its end took (an inner union settled at its parent's push).
fn feed_slot<'p>(g: &mut PipelineGraph<'p>, first: &Open<'p>, c: &Open<'p>) -> u8 {
    let held = c.desc.consumes.iter().copied().chain(c.desc.produces());
    for s in held.clone() {
        g.reads[s as usize] += 1;
    }
    let free = breakers::free_slot(g, first);
    for s in held {
        g.reads[s as usize] -= 1;
    }
    free.unwrap_or_else(|| {
        g.fault.get_or_insert(Refusal::shape(Kind::Append, c.node_id, "more than 256 live breaker slots (the u8 breaker slot id, limits::BREAKERS)"));
        0
    })
}

/// `open`, its union settled now when it will not stream: before its end's slot is chosen (`settle_arms` then takes
/// the lowest free slot, as the Materialize form did).
pub fn resolve<'p>(g: &mut PipelineGraph<'p>, open: Open<'p>) -> Open<'p> {
    match open.union {
        Some(u) if !streams(g, &open, &g.unions[u as usize].arms, g.unions[u as usize].order.is_known(), &|id| g.arm_subs.iter().any(|s| s.union == u && s.plan_id == id)) => settle(g, open),
        _ => open,
    }
}

/// `open` with its union kept in the Materialize form: an open that is embedded (a nest's or nested subplan's inner),
/// spooled or pushed as an InitPlan is one pipeline.
pub fn settle<'p>(g: &mut PipelineGraph<'p>, mut open: Open<'p>) -> Open<'p> {
    let Some(u) = open.union.take() else { return open };
    let arena = *g.pipelines.allocator();
    let arms = core::mem::replace(&mut g.unions[u as usize].arms, ArenaVec::new_in(arena));
    let (order, prune) = (g.unions[u as usize].order, g.unions[u as usize].prune);
    settle_arms(g, open, arms, order, prune)
}
