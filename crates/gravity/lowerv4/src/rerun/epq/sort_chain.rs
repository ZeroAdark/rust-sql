//! D3802 SORTCHAIN (realapp-typeorm: a find with both an order and a lock on a relation of a relation, `SELECT post.id,
//! categories.id, images.id, .. FROM post JOIN post_categories_category .. JOIN category categories .. JOIN
//! category_images_image .. JOIN image images ON images.id = .."imageId" WHERE post.id = $1 ORDER BY categories.id,
//! images.id FOR UPDATE OF images`, its tables never analyzed: LockRows -> Sort -> Nested Loop (Nested Loop (Nested Loop
//! (the post's pkey scan; Hash Join (category; Hash <- post_categories_category)); category_images_image's pkey scan);
//! image's pkey scan)): D3780 LOCKCHAIN's lock (ref_nest.rs `chain_lock_form`) behind a Sort. The lock reads the sorted
//! rows, where no nest holds the outer row any more, so the recheck redoes EvalPlanQual's rerun whole: PostgreSQL
//! substitutes every REFERENCE scan with the row refetched at the locked row's junk ctid (EvalPlanQualFetchRowMark under
//! SnapshotAny: the version the join read; none for a NULL one) and the locked scan with the newest version, reruns Sort
//! -> the joins over them (each scan's recheck quals, filters and projection, the hash clauses and join quals, the nests'
//! parameters from the rows so far) and LockRows hands the one row the Sort returns to its parent, in the sorted place
//! the old version had (the Sort is not redone over the other rows). Here the IR redo (epq_ir.rs `epq_of_sort_chain`):
//! the producer's source row refetched (`Redo::Source`), each join's REFERENCE row refetched in op order (a nest inner's
//! own hash probes of REFERENCE builds within that nest's fetch, `redo_n`), the locked inner's row the lock site's
//! newest version (`Fetch::held`), then the producer's ops up to its sort build; the row goes on through the ops above the
//! lock, so a LIMIT / OFFSET counts it. Served: the lock pipeline the Sort emit's projection below the lock, projections
//! and a LIMIT / OFFSET above it; the Sort's producer over a REFERENCE mark's own heap scan, filters, gates and
//! projections around INNER nests and INNER hash probes, a projection last before its sort build; each probe's build a
//! REFERENCE mark's quiet own heap scan (lower.rs `ref_chain`'s); each nest's inner a REFERENCE mark's own heap scan under
//! filters and projections (a Memoize over it, or a Materialize filled by one: ref_nest.rs `ref_side`, `ref_spool`), or
//! such a scan probing REFERENCE builds (the Hash Join above), or, once, the locked mark's own heap scan under filters and
//! projections (a Memoize over it); every other mark REFERENCE and read once; no InitPlan or CTE, no family, no SubPlan in
//! what the redo evaluates; the statement's pipelines the form's. With no LIMIT / OFFSET above, only a chain the runner's
//! recheck does not take (a nest whose inner is more than one scan: lockrows/marks.rs NOT_OWN), as LOCKCHAIN.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::modifytable::{LockDesc, RowMark};
use gravity_decompose::{Op, Source};
use gravity_program::LockKind;
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::jointype::JoinType;
use types_nodes::plannodes::RowMarkType;

use crate::ops::lockrows::plan::Census;
use crate::graph::Gx;
use crate::rerun::epq::epq_ir::HELD;
use crate::rerun::epq::ref_nest::{exprs, heap, inner_end, memo_tail, param_values, recheck_quals, ref_side, ref_spool};

/// `form`'s answer.
pub(crate) struct SortChain<'g> {
    /// The Sort's producer and its sort build op.
    pub q: usize,
    pub jq: usize,
    /// The locked mark's junk ctid column (the lock's position).
    pub ctid: i16,
    /// The producer's source: its REFERENCE mark's junk ctid column and relation.
    pub from: (i16, u32),
    /// Each join's fetch in the order epq_ir.rs `redo_n` takes them (pipeline, junk ctid column, relation), the locked
    /// inner's column `HELD`.
    pub fetch: PgVec<'g, (usize, i16, u32)>,
}

/// The lock at op `j` of pipeline `k` in SORTCHAIN's form (module doc), else none; `limit`: whether an op is a LIMIT /
/// OFFSET, `emit`: whether it is the Sort emit's projection (lower.rs `sort_emit`).
pub(crate) fn form<'g>(gx: &Gx<'g, '_>, k: usize, j: usize, d: &LockDesc<'_>, (limit, emit): (fn(&Op<'_>) -> bool, fn(&Op<'_>) -> bool)) -> Option<SortChain<'g>> {
    use gravity_decompose::expr::has_subplan;
    let ps = gx.pipelines();
    let mcx = gx.mcx;
    let p = &ps[k];
    let Source::Breaker { slot } = p.source else { return None };
    if !p.ops[..j].iter().all(emit) || !p.ops[j + 1..].iter().all(|o| matches!(o, Op::Project(_)) || limit(o)) {
        return None;
    }
    if !gx.graph.init_plans.is_empty() || d.family().is_some() || d.family_top().is_some() || d.marks.len() < 3 || p.desc.produces().is_some() || p.desc.consumes.len() != 1 {
        return None;
    }
    let mut locking = d.marks.iter().filter(|m| m.lock != LockKind::None);
    let (Some(m), None) = (locking.next(), locking.next()) else { return None };
    let refs: PgVec<'_, &RowMark> = d.marks.iter().filter(|r| r.rti != m.rti).collect_in(mcx);
    if m.parent || m.ctid_col <= 0 || m.relid != d.plan.target_rel || refs.iter().any(|r| r.parent || r.ctid_col <= 0 || r.mark != RowMarkType::ROW_MARK_REFERENCE) {
        return None;
    }
    let q = (0..k).rev().find(|&x| ps[x].desc.produces() == Some(slot))?;
    let pq = &ps[q];
    let jq = pq.ops.iter().position(|o| matches!(o, Op::Sort(_)))?;
    if !pq.ops[jq..].iter().all(|o| matches!(o, Op::Sort(_))) || !matches!(pq.ops[..jq].last(), Some(Op::Project(_))) {
        return None;
    }
    let src = refs.iter().copied().find(|r| heap(&pq.source) == Some((r.rti, r.relid)))?;
    let mut used = mcx::pg_vec![in mcx; src.rti];
    // a REFERENCE mark not read yet over scan `s` (its own heap scan)
    let free = |used: &[u32], s: &Source<'_>| refs.iter().copied().find(|r| heap(s) == Some((r.rti, r.relid)) && !used.contains(&r.rti));
    let mut fetch: PgVec<'g, (usize, i16, u32)> = PgVec::new_in(gx.mcx);
    // the pipelines a probe's build or a Materialize's fill is, beside the producer and the lock's
    let mut extra: PgVec<'_, usize> = PgVec::new_in(mcx);
    // what the redo evaluates (no SubPlan there)
    let mut subplan = recheck_quals(&pq.source).into_iter().any(|x| has_subplan(x));
    let (mut held, mut runner) = (0, true);
    let plain = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    for (i, o) in pq.ops[..jq].iter().enumerate() {
        match o {
            o if plain(o) => subplan |= exprs(o).into_iter().any(|x| has_subplan(x)),
            Op::Join(JoinDesc::Probe { clauses, keys, qual, .. }) => {
                let (bq, at) = ref_probe(gx, q, o)?;
                let r = free(&used[..], &ps[bq].source).filter(|r| (r.rti, r.relid) == at)?;
                used.push_in(r.rti);
                extra.push_in(bq);
                fetch.push_in((bq, r.ctid_col, r.relid));
                subplan |= clauses.iter().chain(keys.iter()).chain(qual.iter()).chain(build_read(gx, bq)).any(|x| has_subplan(x));
            }
            Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, inner, params, qual, .. }) => {
                subplan |= param_values(params).chain(qual.iter()).any(|x| has_subplan(x));
                runner &= crate::rerun::epq::marks::nest_joins(o) || crate::rerun::epq::marks::nest_memo_joins(o);
                // the locked mark's own scan, once (LOCKCHAIN's inner)
                if heap(&inner.source) == Some((m.rti, m.relid)) {
                    let ik = ps.inner_of(q, i)?;
                    let s = &ps[ik];
                    let end = inner_end(s);
                    if held > 0 || !s.desc.consumes.is_empty() || !s.ops[..end].iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) || !memo_tail(s) {
                        return None;
                    }
                    held += 1;
                    subplan |= recheck_quals(&s.source).into_iter().chain(s.ops[..end].iter().flat_map(exprs)).any(|x| has_subplan(x));
                    fetch.push_in((ik, HELD, m.relid));
                    continue;
                }
                // a REFERENCE mark's own scan, embedded (a Memoize over it or none)
                if let Some((x, r)) = refs.iter().find_map(|r| (!used.contains(&r.rti)).then(|| ref_side(gx, q, i, r).map(|x| (x, *r))).flatten()) {
                    let s = &ps[x];
                    subplan |= recheck_quals(&s.source).into_iter().chain(s.ops[..inner_end(s)].iter().flat_map(exprs)).any(|x| has_subplan(x));
                    used.push_in(r.rti);
                    fetch.push_in((x, r.ctid_col, r.relid));
                    continue;
                }
                // a Materialize filled by one
                if let Some((f, _)) = ref_spool(gx, q, i) {
                    let r = free(&used[..], &ps[f].source)?;
                    let s = &ps[f];
                    subplan |= recheck_quals(&s.source).into_iter().chain(s.ops[..inner_end(s)].iter().flat_map(exprs)).any(|x| has_subplan(x));
                    used.push_in(r.rti);
                    extra.push_in(f);
                    fetch.push_in((f, r.ctid_col, r.relid));
                    continue;
                }
                // such a scan probing REFERENCE builds (the Hash Join a nest's inner is): its fetch, then its probes'
                let ik = ps.inner_of(q, i)?;
                let s = &ps[ik];
                let r = free(&used[..], &s.source)?;
                let end = inner_end(s);
                if matches!(s.source, Source::Breaker { .. }) || !memo_tail(s) || !matches!(s.ops[..end].last(), Some(Op::Project(_))) {
                    return None;
                }
                used.push_in(r.rti);
                fetch.push_in((ik, r.ctid_col, r.relid));
                subplan |= recheck_quals(&s.source).into_iter().any(|x| has_subplan(x));
                let mut probes = 0;
                for o in s.ops[..end].iter() {
                    match o {
                        o if plain(o) => subplan |= exprs(o).into_iter().any(|x| has_subplan(x)),
                        Op::Join(JoinDesc::Probe { clauses, keys, qual, .. }) => {
                            let (bq, at) = ref_probe(gx, ik, o)?;
                            let rb = free(&used[..], &ps[bq].source).filter(|rb| (rb.rti, rb.relid) == at)?;
                            used.push_in(rb.rti);
                            extra.push_in(bq);
                            fetch.push_in((bq, rb.ctid_col, rb.relid));
                            subplan |= clauses.iter().chain(keys.iter()).chain(qual.iter()).chain(build_read(gx, bq)).any(|x| has_subplan(x));
                            probes += 1;
                        }
                        _ => return None,
                    }
                }
                if probes == 0 {
                    return None;
                }
            }
            _ => return None,
        }
    }
    // every mark read once, the statement's pipelines the form's (the lock's, the producer, the builds and fills it reads)
    let mut seen = mcx::to_vec_in(&extra[..], mcx);
    seen.sort_unstable();
    seen.dedup();
    let own = seen.len() == extra.len() && Census(gx).whole(2 + extra.len()) && pq.desc.consumes.len() == extra.len() && extra.iter().all(|&x| x < q && ps[x].desc.consumes.is_empty());
    if held != 1 || used.len() != refs.len() || !own || subplan {
        return None;
    }
    // with no LIMIT / OFFSET above, the runner's recheck keeps a chain of nests over one scan each (marks.rs)
    if runner && !p.ops[j + 1..].iter().any(limit) {
        return None;
    }
    Some(SortChain { q, jq, ctid: m.ctid_col, from: (src.ctid_col, src.relid), fetch })
}

/// An INNER hash probe op `o` of pipeline `x` whose build is a relation's own heap scan, quiet (lower.rs `ref_chain`'s
/// build: effects/visible/lockrows.rs `quiet_ref_build`), reading nothing else: (the build's pipeline, its (rti, relid)).
fn ref_probe(gx: &Gx<'_, '_>, x: usize, o: &Op<'_>) -> Option<(usize, (u32, u32))> {
    let Op::Join(JoinDesc::Probe { slot, jointype: JoinType::JOIN_INNER, catalog: false, .. }) = o else { return None };
    let bq = gx.life_of(x, *slot).map(|l| l.0 as usize)?;
    let pb = &gx.pipelines()[bq];
    let Some(Op::Join(JoinDesc::Build { .. })) = pb.ops.last() else { return None };
    let quiet = crate::effects::visible::lockrows::quiet_ref_build(gx, pb, &pb.ops[..pb.ops.len() - 1]);
    (quiet && pb.desc.consumes.is_empty() && !Census(gx).init_member(bq)).then_some((bq, heap(&pb.source)?))
}

/// What a probe build's redo evaluates: its source's recheck quals and its ops before the build.
fn build_read<'g, 'p>(gx: &Gx<'g, 'p>, bq: usize) -> PgVec<'g, types_nodes::Node<'p>> {
    let pb = &gx.pipelines()[bq];
    recheck_quals(&pb.source).into_iter().chain(pb.ops[..pb.ops.len() - 1].iter().flat_map(exprs)).collect_in(gx.mcx)
}
