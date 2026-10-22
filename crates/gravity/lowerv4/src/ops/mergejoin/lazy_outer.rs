//! merge.join's lazy inner (MERGE R3 (ii), D824's lazy build at INITIALIZE_INNER) under an outer that may act, and the
//! outer feeders the inner's group leaves out (W8-D1236-RESIDUE, row B). PG runs a merge join's outer at INITIALIZE_OUTER,
//! the first fetch, where its Sort (ExecSort) reads the whole subtree; v4 runs the outer pipelines first, whole. The two
//! agree wherever PG surely runs the merge once the statement runs (`reached`) over a full Sort's run, so the lazy inner
//! (its acts at the first matchable outer tuple, as PG's) serves such a join exactly; a merge PG may leave unrun (a LIMIT,
//! a gate, a probe, a SubPlan, a CTE above it: D1716's class) keeps D1236's name. A helper file (ROWS = NONE). Owner
//! M11-MERGE. Budget 190 lines (D1289's one-NULL-row outer, `one_null_row`, is an outer fact too).
use gravity_abi::breaker::BreakerKind;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::sort::SortForm;
use gravity_decompose::{Op, PipelineNode, Source};
use gravity_program::LifeId;
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::primnodes::OUTER_VAR;
use types_nodes::Node;

use crate::effects::visible::merge::quiet;
use crate::graph::Gx;

/// D2026: the pipelines that fill slot `s` for its reader `k`: the life `Gx::life_of` names (the nearest producer, a UNION
/// ALL's LAST arm) and the rest of that arm's run (`Gx::run_arms`: decompose's append.rs gives each arm the parent's ops
/// and end, so every arm writes the slot), each with its settled union's feeds (`settled`), as join/build_class.rs `feed`
/// reads a side. Every walk of this family reads all of them (an earlier arm that raises or calls nextval counts as the
/// last one does).
pub(crate) fn fillers<'g>(gx: &Gx<'g, '_>, k: usize, s: u8) -> PgVec<'g, usize> {
    let Some(l) = gx.life_of(k, s).map(|l| l.0 as usize).filter(|&l| l < gx.pipelines().len()) else { return PgVec::new_in(gx.mcx) };
    let arms = gx.run_arms(l);
    arms.iter().copied().chain([l]).flat_map(|x| settled(gx, x)).chain(arms.iter().copied().chain([l])).collect_in(gx.mcx) // (the life last)
}

/// D2026: the feeds of the settled union whose emit is pipeline `p` (its Union op's slot: rerun/union_join_arms.rs `feeds`,
/// every arm whatever the statement holds), none when `p` is no union's emit.
pub(crate) fn settled(gx: &Gx<'_, '_>, p: usize) -> Vec<usize> {
    crate::rerun::union_arms::union_of(&gx.pipelines()[p]).map_or_else(Vec::new, |u| crate::rerun::union_join_arms::feeds(gx, p, u.0))
}

/// The pipelines feeding pipeline `k` through `slots`, transitively (each slot's `fillers` and all they read, a settled
/// union's feeds too: build_class.rs `feed`).
pub(crate) fn lives<'g>(gx: &Gx<'g, '_>, k: usize, slots: impl Iterator<Item = u8>) -> PgVec<'g, usize> {
    lives_by(gx, k, slots, true)
}

/// `lives`; without `every`, each slot's nearest producer only (`Gx::life_of`: the walk D824's group is built by).
pub(crate) fn lives_by<'g>(gx: &Gx<'g, '_>, k: usize, slots: impl Iterator<Item = u8>, every: bool) -> PgVec<'g, usize> {
    let fill = |k: usize, s: u8| if every { fillers(gx, k, s) } else { gx.life_of(k, s).map(|l| l.0 as usize).filter(|&l| l < gx.pipelines().len()).into_iter().collect_in(gx.mcx) };
    let (mut out, mut todo): (PgVec<'g, usize>, PgVec<'_, usize>) = (PgVec::new_in(gx.mcx), slots.flat_map(|s| fill(k, s)).collect_in(gx.mcx));
    while let Some(p) = todo.pop() {
        if !out.contains(&p) {
            out.push_in(p);
            todo.extend_in(gx.pipelines()[p].desc.consumes.iter().flat_map(|&s| fill(p, s)).chain(if every { settled(gx, p) } else { Vec::new() }));
        }
    }
    out
}

/// D3799 (River's JobSchedule over a large job table: a Merge Left Join whose outer is the CTE Scan of jobs_with_rownum
/// and whose inner is a Materialize of a merge join that reads it too): the pipelines PG ran to their end once pipeline
/// `k`, whose rows come from the breaker slot `slot` (its source), handed its first row. The producer of a full breaker
/// (a sort's run, a hash build, an aggregate, a distinct, a top-N) ran whole with all that feeds it; any other producer
/// (a CTE's store, a window's partition buffer, a spool) only made its first row: what its own source's first row ran.
/// Only sources are followed (a slot an op reads may be read on a condition: a SubPlan's set). PG's merge fetches its
/// outer first (ExecMergeJoin's MJ_INITIALIZE_OUTER), so they ran before its first inner fetch: jobs_with_rownum's Sort
/// read the claim jobs_to_schedule, its FOR UPDATE, to the end.
pub(crate) fn first_whole<'g>(gx: &Gx<'g, '_>, k: usize, slot: u8) -> PgVec<'g, usize> {
    let ps = gx.pipelines();
    let src = |x: usize| if let Source::Breaker { slot } = ps[x].source { Some(slot) } else { None };
    let (mut out, mut seen): (PgVec<'g, usize>, PgVec<'_, usize>) = (PgVec::new_in(gx.mcx), PgVec::new_in(gx.mcx));
    let mut todo = mcx::pg_vec![in gx.mcx; (k, slot)];
    while let Some((x, s)) = todo.pop() {
        for q in fillers(gx, x, s) {
            if seen.contains(&q) {
                continue;
            }
            seen.push_in(q);
            if matches!(crate::frame::iface::end_breaker(gx, q), Some(BreakerKind::Sort | BreakerKind::HashBuild | BreakerKind::HashAgg | BreakerKind::PlainAgg | BreakerKind::Distinct | BreakerKind::TopK)) {
                out.push_in(q);
                out.extend_in(lives(gx, q, ps[q].desc.consumes.iter().copied().chain(src(q))));
            } else {
                todo.extend_in(src(q).map(|s| (q, s)));
            }
        }
    }
    out
}

pub(crate) fn arms(gx: &Gx<'_, '_>, xs: &[usize]) -> bool {
    xs.iter().any(|&x| gx.pipelines()[x].ops.iter().any(|op| matches!(op, Op::Append(_))))
}

/// Pipeline `q`'s merge op `j` over an outer that may act (`before`: the outer's feeders; `all`: the inner's, its own
/// pipeline last): the outer is a full Sort's run as the inner is (runs/slot_run.rs; an Incremental Sort reads a group at a
/// time), the ops before the merge quiet (no value held over the wait), every other pipeline below `q` quiet, PG runs it.
/// D2026: a UNION ALL's arms on either side are not this form (D824's lazy group holds the last arm only: `leaves`); the
/// merge then takes lower.rs's other roads (an inner that may raise runs whole first under errorhold's licence).
pub(crate) fn acting_outer(gx: &Gx<'_, '_>, q: usize, j: usize, before: &[usize], all: &[usize]) -> bool {
    outer_form(gx, q, j, before, all) && reached(gx, q, j + 1)
}

/// `acting_outer` less PG's surely running the merge (`reached`; fed_first.rs `term` asks it in a recursive term).
pub(crate) fn outer_form(gx: &Gx<'_, '_>, q: usize, j: usize, before: &[usize], all: &[usize]) -> bool {
    if arms(gx, before) || arms(gx, all) {
        return false;
    }
    let ps = gx.pipelines();
    let outer = matches!(ps[q].source, Source::Breaker { slot } if gx.life_of(q, slot).is_some_and(|o| full(&ps[o.0 as usize])));
    let own = ps[q].ops[..j].iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_))) && quiet(gx, q, j);
    outer && own && (0..q).filter(|c| !before.contains(c) && !all.contains(c)).all(|c| quiet(gx, c, usize::MAX))
}

/// D2026: the pipelines of the inner's feeders `all` that only a UNION ALL's earlier arms reach (outside `near`, the
/// nearest-producer walk D824's group is built by) and the group `group` leaves out run in program order ahead of the
/// merge, outside the outer's feeders `before`: each must be quiet.
pub(crate) fn group_holds(gx: &Gx<'_, '_>, group: &[usize], (before, all): (&[usize], &[usize]), near: &[usize]) -> bool {
    all.iter().filter(|x| !near.contains(x) && !group.contains(x) && !before.contains(x)).all(|&x| quiet(gx, x, usize::MAX))
}

/// Pipeline `p` ends in a full Sort's run (`Runs`, no presorted prefix).
pub(crate) fn full(p: &PipelineNode<'_>) -> bool {
    crate::frame::iface::breaker_of(&p.desc).is_some_and(|e| e.kind == BreakerKind::Sort) && matches!(p.ops.last(), Some(Op::Sort(d)) if d.presorted == 0 && matches!(d.form, SortForm::Runs))
}

/// PG surely runs pipeline `q`'s rows from op `from` on once the statement runs: they are only filtered, projected, written
/// or a nest loop's outer (ExecNestLoop fetches its outer first), into the root's client, or into a full Sort, a plain
/// aggregate or a hashed one that one such pipeline alone reads, or into a hash table PG surely builds (`built`), every
/// pipeline between quiet (a LIMIT, gate, probe, memo, SubPlan, Append or CTE on the way may leave the merge unrun).
/// D3691: a hashed aggregate without grouping sets is PG's AGG_HASHED, which reads its whole input (agg_fill_hash_table,
/// spilling past hash_mem but reading on) before it hands any group on, as a plain aggregate does.
pub(crate) fn reached(gx: &Gx<'_, '_>, q: usize, from: usize) -> bool {
    crate::stack_check();
    use gravity_decompose::ops::shared::agg::GroupForm;
    let (ps, plain) = (gx.pipelines(), |ops: &[Op<'_>]| ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_) | Op::Write(_) | Op::Join(JoinDesc::Nest { .. }))));
    let ops = ps[q].ops.get(from..).unwrap_or(&[]);
    let hashed = || gx.group_at(q).is_some_and(|g| matches!(g.form, GroupForm::Hashed) && g.set.is_none());
    let slot = match crate::frame::iface::breaker_of(&ps[q].desc) {
        None => return plain(ops) && q == gx.graph.root as usize,
        Some(e) if e.kind == BreakerKind::PlainAgg && plain(ops) || full(&ps[q]) && ops.split_last().is_some_and(|(_, r)| plain(r)) => e.slot,
        Some(e) if e.kind == BreakerKind::HashAgg && plain(ops) && hashed() => e.slot,
        Some(e) if e.kind == BreakerKind::HashBuild => return ops.split_last().is_some_and(|(b, r)| matches!(b, Op::Join(JoinDesc::Build { .. })) && plain(r)) && built(gx, q, e.slot),
        _ => return false,
    };
    let readers: PgVec<'_, usize> = (q + 1..ps.len()).filter(|&c| ps[c].desc.consumes.contains(&slot) && gx.life_of(c, slot) == Some(LifeId(q as u16))).collect_in(gx.mcx);
    matches!(readers[..], [c] if matches!(ps[c].source, Source::Breaker { slot: s } if s == slot) && (q + 1..c).all(|x| quiet(gx, x, usize::MAX)) && reached(gx, c, 0))
}

/// D3178: build `q`'s hash table (slot `slot`) is surely built once the statement runs: its join's one probe builds first
/// (decompose `outer_first` false: FILL_INNER, a shared table or the cost rule, so ExecHashJoin's first call runs the Hash,
/// which reads its whole input) and PG surely calls that join: its rows from the probe on (a draining join's, which close
/// into the matches spool its drain reads: from the drain on) are `reached`, every pipeline between but the probe's quiet
/// (the probe's own outer runs after the build, as PG fetches it).
fn built(gx: &Gx<'_, '_>, q: usize, slot: u8) -> bool {
    let ps = gx.pipelines();
    let readers: PgVec<'_, usize> = (q + 1..ps.len()).filter(|&c| ps[c].desc.consumes.contains(&slot) && gx.life_of(c, slot) == Some(LifeId(q as u16))).collect_in(gx.mcx);
    let at = |c: usize, f: &dyn Fn(&Op<'_>) -> bool| ps[c].ops.iter().position(f);
    let probe = |op: &Op<'_>| matches!(op, Op::Join(JoinDesc::Probe { slot: s, outer_first: false, .. }) if *s == slot);
    let between = |end: usize, c: usize| (q + 1..end).all(|x| x == c || quiet(gx, x, usize::MAX));
    let ok = match readers[..] {
        [c] => at(c, &probe).is_some_and(|j| !matches!(ps[c].ops[j], Op::Join(JoinDesc::Probe { marks: true, .. })) && between(c, c) && reached(gx, c, j + 1)),
        [c, d] => at(c, &probe).is_some_and(|j| {
            let m = crate::frame::iface::breaker_of(&ps[c].desc).filter(|e| e.kind == BreakerKind::Materialize).map(|e| e.slot);
            let drain = |op: &Op<'_>| matches!(op, Op::Join(JoinDesc::Drain { matches, .. }) if Some(*matches) == m);
            matches!(ps[c].ops[j], Op::Join(JoinDesc::Probe { marks: true, .. })) && matches!(ps[d].source, Source::Breaker { slot: s } if s == slot)
                && gx.life_of(d, m.unwrap_or(u8::MAX)) == Some(LifeId(c as u16)) && between(d, c) && at(d, &drain).is_some_and(|i| reached(gx, d, i + 1))
        }),
        _ => false,
    };
    if ok {
        fires::fire("merge.reached.built"); // shapecheck fire signal (D3178: a build PG surely makes counts as reached)
    }
    ok
}

/// The outer feeders D824's `group` leaves out of the inner's (in `all` and `before`: build_class.rs `lazy_group`) run
/// before `q`: each only where its slot outlives the group's reads (its last reader by index a group member, no pipeline
/// before `q` refilling its number: the driver's `free_after`), no UNION ALL arm or hash join on either side, under an acting
/// outer every outer feeder reading whole (PG computes a CTE only as far as its readers read: a LIMIT, probe or gate on
/// the outer side leaves its rest to the inner's run or to none).
pub(crate) fn leaves(gx: &Gx<'_, '_>, q: usize, group: &[usize], (before, all): (&[usize], &[usize]), acting: bool) -> bool {
    let ps = gx.pipelines();
    let out: PgVec<'_, usize> = all.iter().copied().filter(|x| before.contains(x) && !group.contains(x)).collect_in(gx.mcx);
    let reads = |c: usize, x: usize| ps[c].desc.consumes.iter().any(|&s| gx.life_of(c, s) == Some(LifeId(x as u16)));
    let refill = |x: usize, s: u8| (x + 1..q).any(|c| crate::frame::iface::breaker_of(&ps[c].desc).is_some_and(|f| f.slot == s));
    let lives = |x: usize| crate::frame::iface::breaker_of(&ps[x].desc).is_some_and(|e| !refill(x, e.slot)) && (x + 1..ps.len()).filter(|&c| reads(c, x)).max().is_some_and(|c| group.contains(&c));
    // a hash join's sides may swap pipelines (join/build_side.rs), so graph order is program order only without one
    let hash = |x: &usize| ps[*x].ops.iter().any(|op| matches!(op, Op::Join(JoinDesc::Build { .. } | JoinDesc::Probe { .. })));
    out.is_empty() || out.iter().all(|&x| lives(x)) && !arms(gx, before) && !arms(gx, all) && !before.iter().chain(all).any(hash) && (!acting || before.iter().all(|&x| crate::ops::mergejoin::fed_first::whole(&ps[x])))
}

/// D1289 (rnd-3686), D3188: whether at most one outer row (pipeline `k`'s rows at the merge, op `j`) has a NULL first
/// merge key `key`, so no outer row follows the first NULLS LAST NULL one and v4 runs, past it, nothing PG runs not. A
/// property derived back from the merge to where it starts: a group keyed by that column alone emits one row per key
/// value (no grouping set; a DISTINCT aggregate's pair keys aside), and its feeders fold whole in PG too (the NULL group
/// sorts last, a group finishes only at its input's end). An op that never hands on more rows than it reads passes it
/// on with the column it keeps (`source_col`), and so does a store that keeps its producer's rows (a Sort, Materialize
/// or TopK breaker: a CTE's store, a re-sorted group's emit); any other op or source breaks it.
pub(crate) fn one_null_row<'p>(gx: &Gx<'_, 'p>, k: usize, j: usize, key: Node<'p>) -> bool {
    let Some(v) = strip(key).as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0) else { return false };
    let ps = gx.pipelines();
    let (mut p, mut ops, mut col) = (k, &ps[k].ops[..j], v.varattno);
    for _ in 0..ps.len() {
        let Some((c, outer)) = source_col(gx, p, ops, col) else { return false };
        if let Some(g) = gx.drained_group(p) {
            let mut keys = g.keys.iter().filter(|key| key.resno > 0);
            return outer && g.set.is_none() && matches!((keys.next(), keys.next()), (Some(only), None) if only.resno == c);
        }
        let Source::Breaker { slot } = ps[p].source else { return false };
        let Some(l) = gx.life_of(p, slot).map(|l| l.0 as usize).filter(|&l| l < p) else { return false };
        let store = crate::frame::iface::breaker_of(&ps[l].desc).is_some_and(|b| b.slot == slot && matches!(b.kind, BreakerKind::Sort | BreakerKind::Materialize | BreakerKind::TopK));
        if !store {
            return false;
        }
        (p, ops, col) = (l, &ps[l].ops[..], c);
    }
    false
}

/// The column of pipeline `p`'s source row that column `col` of `ops`' last row is, through each projection's bare
/// column (nearest first: a subquery or CTE scan's, then the one over the source row, whose `OUTER_VAR` ends the walk),
/// and whether it ended at an `OUTER_VAR` or met no projection (a group's row, not a scan's); `None` when a projection
/// computes it or an op may hand on more rows than it reads (all but a filter, a gate, a projection and a Sort by
/// built-in comparators).
fn source_col<'p>(gx: &Gx<'_, 'p>, p: usize, ops: &[Op<'p>], col: i16) -> Option<(i16, bool)> {
    let mut at = (col, true);
    let mut mapping = true;
    for op in ops.iter().rev() {
        match op {
            Op::Filter(_) | Op::Gate(_) => {}
            Op::Sort(_) if !crate::effects::visible::merge::user_sort(gx, &gx.pipelines()[p]) => {}
            Op::Project(l) if mapping => {
                let v = strip(l.iter().filter_map(|n| n.as_target_entry()).find(|t| t.resno == at.0)?.expr).as_var().filter(|v| v.varlevelsup == 0)?;
                at = (v.varattno, v.varno == OUTER_VAR);
                mapping = v.varno != OUTER_VAR;
            }
            Op::Project(_) => {}
            _ => return None,
        }
    }
    Some(at)
}

fn strip(n: Node<'_>) -> Node<'_> {
    n.as_relabel_type().map_or(n, |r| r.arg)
}
