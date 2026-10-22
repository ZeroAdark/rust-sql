//! lockrows REFNEST (gen queue-0015: `SELECT j.id, w.name FROM jobs j JOIN workers w ON w.id = j.worker_id WHERE j.state =
//! 'running' ORDER BY j.id LIMIT 3 FOR UPDATE OF j SKIP LOCKED`): Limit -> LockRows -> Nested Loop, the outer the locked
//! relation's own scan, the inner the REFERENCE-marked relation's parameterized scan (a Memoize over it once the tables
//! are analyzed). The runner's recheck (lockrows NEST, rerun/epq/marks.rs `nest_scan`) would send the rechecked row past
//! the Limit (LIMIT_ABOVE), and it opens no Memoize inner (NOT_OWN). PostgreSQL's EvalPlanQual reruns the join over the
//! test tuple: the outer scan substituted with the newest version (its recheck quals and filters), the nest's parameters
//! taken from it, the inner scan substituted with the REFERENCE row refetched at the row's junk ctid (EvalPlanQualFetchRowMark
//! under SnapshotAny: the version the join read) and rechecked under those parameters (IndexRecheck's indexqualorig, its
//! filters; a fresh Memoize in the EPQ tree caches nothing), then the join qual; LockRows hands the result to the Limit.
//! Here the recheck runs in the IR (epq_ir.rs `Fetch` over the nest: the lock host fetches that version, the inner's redo
//! runs over its cells under the nest's parameters computed from the row so far, which are restored after it on both ways
//! out, so the inner's walk goes on under the old row's), so the steps above count the rechecked row. Served (`form`): the
//! statement's one pipeline over the locked mark's own heap scan, filters, gates and projections around one INNER nest
//! whose inner is the REFERENCE mark's own heap scan under filters and projections (then, optionally, a Memoize and its
//! pass-through projection), only projections and a LIMIT / OFFSET above the lock, no other mark (an InitPlan: below); with a
//! LIMIT / OFFSET above or a Memoize inner (a nest without either stays lockrows NEST's runner recheck). Or (gen
//! jcore-0294, analyzed: LockRows -> Nested Loop, the inner a Materialize over the REFERENCE relation's unparameterized
//! scan) the inner a Materialize's emit passed through, the graph's other pipeline that scan under filters and projections
//! filling the spool: PostgreSQL's EvalPlanQual reruns the Materialize over the refetched row (a fresh one in the EPQ tree),
//! so the redo is that pipeline's up to its spool; the runner's recheck would rescan the spool (NOT_OWN).
//! AGA-35 form 5: beside InitPlans that lock and write nothing (plan.rs `Census::plain_inits`: their pipelines counted out,
//! none of the form's own, `init_member`). EvalPlanQualStart copies an evaluated InitPlan's value and never runs it again,
//! so the recheck is the same; `Plan::form` asks `admit_ir_nest` of the outer part (the nest read through its own expressions)
//! and `admit_ir_at` of the REFERENCE relation's part, each InitPlan it may read attached at the lock or above it
//! (INIT_BELOW) and read there unconditionally (INIT_READ).
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::modifytable::LockDesc;
use gravity_decompose::{Am, Op, PipelineNode, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, Type, Value};
use gravity_program::LockKind;
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::jointype::JoinType;
use types_nodes::plannodes::RowMarkType;
use types_nodes::primnodes::OUTER_VAR;
use types_nodes::NodeList;

use crate::ops::lockrows::plan::Census;
use crate::face::step::ExprFace;
use crate::frame::env::Env;
use crate::frame::px::{ExId, Px};
use crate::frame::row::RowVals;
use crate::graph::Gx;

/// `form`'s answer: the lock's junk ctid column (its position) and epq_ir.rs `Fetch`'s input, (the pipeline scanning the
/// REFERENCE relation: the nest's inner as its `Pipes` index, or a Materialize's producer; the REFERENCE mark's junk ctid
/// column in the lock's row; that mark's relation).
pub(crate) struct RefNest {
    pub ctid: i16,
    pub fetch: (usize, i16, u32),
}

/// The lock at op `j` of pipeline `k` in REFNEST's form (module doc), else none; `limit`: whether an op is a LIMIT / OFFSET
/// (the lock home's test: the bound is ops/limit's).
/// D3557 (`paired`): or the claim of the statement's own UPDATE / DELETE (`UPDATE t .. WHERE id IN (SELECT t.id FROM t JOIN
/// q .. LIMIT 3 FOR UPDATE OF t SKIP LOCKED)`, the write probing the hashed subquery; or `id = (..)`, its InitPlan): the
/// lock pipeline may make a product and run before others, with any op above a LIMIT / OFFSET (the SubqueryScan's
/// projection, the hash build: the rechecked row goes on through them, as PostgreSQL's LockRows hands its EvalPlanQual
/// row to the Limit); the form's own pipelines (the lock's, a Materialize's producer) read nothing else, and plan.rs
/// `Plan::form` asks `write_pair` for the write (JOBQ's forms), whose recheck reads the subquery's row from its COPY mark or the
/// InitPlan's value and never runs the lock again.
pub(crate) fn form(gx: &Gx<'_, '_>, k: usize, j: usize, d: &LockDesc<'_>, limit: fn(&Op<'_>) -> bool, paired: bool) -> Option<RefNest> {
    let ps = gx.pipelines();
    let p = &ps[k];
    // D3379 INITSINK: or beside the InitPlan whose own row lock is the M4 pair's A (plan.rs `Plan::pair_entry`)
    let n = match paired {
        true => None,
        false => Some(Census(gx).inits(k, k)?),
    };
    // D3779 REFNEST-FEED (TypeORM's paginated find with a lock: `SELECT DISTINCT d.pid FROM (SELECT p.id AS pid, u.name ..
    // FROM post p LEFT JOIN "user" u ON u.id = p."authorId" WHERE p.id = $1 FOR UPDATE OF p) d ORDER BY d.pid LIMIT 1`:
    // Limit -> Unique -> Sort -> Subquery Scan -> LockRows -> Nested Loop Left Join): or the lock pipeline's rows feed
    // the product only the statement's later pipelines read (lockrows/lower.rs `above_only`: they lock and write
    // nothing). PostgreSQL's LockRows hands its EvalPlanQual row to its parent, so the rechecked row goes on through the
    // ops above the lock into the product; the runner's recheck would send it to the client (marks.rs EMIT_ONLY)
    let feed = match paired {
        true => 0,
        false if p.desc.produces().is_some() => Census(gx).above_only(k).filter(|&a| a > 0)?,
        false => 0,
    };
    if !paired && !Census(gx).whole(k + 1 + feed) || d.family().is_some() || d.family_top().is_some() || d.marks.len() != 2 {
        return None;
    }
    let above = match paired {
        true => p.ops[j + 1..].iter().any(limit),
        // the ops up to the product's sink: projections (the Subquery Scan's), filters, gates, a LIMIT / OFFSET
        false if feed > 0 => j + 2 <= p.ops.len() && p.ops[j + 1..p.ops.len() - 1].iter().all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_)) || limit(o)) && !matches!(p.ops.last(), Some(Op::Write(_) | Op::Lock(_))),
        false => p.ops[j + 1..].iter().all(|o| matches!(o, Op::Project(_)) || limit(o)),
    };
    if !above {
        return None;
    }
    let m = d.marks.iter().find(|m| m.lock != LockKind::None)?;
    let r = d.marks.iter().find(|m| m.lock == LockKind::None)?;
    if m.parent || m.ctid_col <= 0 || m.relid != d.plan.target_rel || r.parent || r.ctid_col <= 0 || r.mark != RowMarkType::ROW_MARK_REFERENCE || r.rti == m.rti || heap(&p.source) != Some((m.rti, m.relid)) {
        return None;
    }
    let plain = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    let jn = p.ops[..j].iter().position(|o| !plain(o))?;
    // D3560 REFNEST-LEFT: or a LEFT nest (Django's select_related over a nullable FK): epq_ir.rs's redo null-extends the row
    // when the refetch finds no row (a NULL junk ctid) or the inner's redo or the join qual fails, as the nest loop does
    let Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER | JoinType::JOIN_LEFT, inner, .. }) = &p.ops[jn] else { return None };
    // D3379 REFNEST-SUB (epq-subq-subplan joinv): or a per-row SubPlan step past the nest, evaluated again over the joined
    // row (epq_ir.rs `Redo::Sub`), as EvalPlanQual re-runs it with the newest version's arguments under the statement's
    // snapshot (D1478: its own scan never substituted)
    if !p.ops[jn + 1..j].iter().all(|o| plain(o) || matches!(o, Op::SubPlan(_))) {
        return None;
    }
    let i = ps.inners().iter().position(|x| x.0 == (k, jn))?;
    // the pipeline whose scan reads the REFERENCE relation: the inner itself, or (a Materialize: the inner the spool's emit,
    // passed through) the graph's other pipeline, which fills that spool and nothing else
    let (q, kept) = match inner.source {
        Source::Breaker { slot } => {
            // D3460 REFNEST-LIFE (m8_d652_lock_initplan :383, its cursors): the spool's own producer, the life the lock's
            // pipeline reads (`life_of`): an InitPlan's Aggregate before it may have produced the same slot number first
            let q = gx.life_of(k, slot).map(|l| l.0 as usize).filter(|&x| x < k)?;
            let alone = n.is_none_or(|n| Census(gx).whole(2 + n + feed)) && p.desc.consumes.len() == 1 && p.desc.consumes[0] == slot && ps[q].desc.consumes.is_empty() && (!Census(gx).init_member(q) || paired && Census(gx).init_member(k));
            if !alone || !inner.ops.iter().all(pass_through) || !matches!(ps[q].ops.last(), Some(Op::Join(JoinDesc::Spool))) {
                return None;
            }
            (q, true)
        }
        _ if n.is_none_or(|n| Census(gx).whole(1 + n + feed)) && p.desc.consumes.is_empty() => (ps.len() + i, false),
        _ => return None,
    };
    let s = &ps[q];
    let end = inner_end(s);
    let memo = matches!(s.ops.get(end), Some(Op::Join(JoinDesc::Memo { .. })));
    let below = s.ops[..end].iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) && matches!(s.ops[..end].last(), Some(Op::Project(_)));
    if heap(&s.source) != Some((r.rti, r.relid)) || !below || !s.ops[end..].iter().skip(1).all(pass_through) || !(memo || kept || feed > 0 || p.ops[j + 1..].iter().any(limit)) {
        return None;
    }
    Some(RefNest { ctid: m.ctid_col, fetch: (q, r.ctid_col, r.relid) })
}

/// D3564 REFNEST-CHAIN (an ORM's claim selecting rows of two related relations: `SELECT t.id, q.name, l.owner FROM
/// app_task t JOIN app_queue q ON q.id = t.queue_id LEFT JOIN app_lock l ON l.task_id = t.id WHERE .. ORDER BY .. LIMIT
/// 6 FOR UPDATE OF t SKIP LOCKED`; PostgreSQL's Limit -> LockRows -> Nested Loop Left Join (Nested Loop (the locked
/// relation's scan; Memoize -> the first REFERENCE relation's scan); the second's scan)): `form`'s statement over
/// several nests, INNER or LEFT, each inner the own heap scan of a REFERENCE mark of its own under filters and
/// projections (a Memoize over it or not, `ref_side`), every mark but the locking one such a REFERENCE mark, only
/// projections and at least one LIMIT / OFFSET above the lock. EvalPlanQual reruns the joins over the test tuple and
/// each REFERENCE row refetched at its junk ctid (none for a NULL one: a LEFT nest null-extends the row again); here
/// REFNEST's redo, each nest's fetch in op order (epq_ir.rs `redo_of`). Some((the lock's junk ctid column, the
/// fetches)).
pub(crate) fn chain_form<'g>(gx: &Gx<'g, '_>, k: usize, j: usize, d: &LockDesc<'_>, limit: fn(&Op<'_>) -> bool) -> Option<(i16, PgVec<'g, (usize, i16, u32)>)> {
    let ps = gx.pipelines();
    let p = &ps[k];
    let n = Census(gx).inits(k, k)?;
    if !Census(gx).whole(k + 1) || p.desc.produces().is_some() || d.family().is_some() || d.family_top().is_some() || d.marks.len() < 3 {
        return None;
    }
    if !p.ops[j + 1..].iter().all(|o| matches!(o, Op::Project(_)) || limit(o)) || !p.ops[j + 1..].iter().any(limit) {
        return None;
    }
    let mut locking = d.marks.iter().filter(|m| m.lock != LockKind::None);
    let m = locking.next()?;
    if locking.next().is_some() || m.parent || m.ctid_col <= 0 || m.relid != d.plan.target_rel || heap(&p.source) != Some((m.rti, m.relid)) {
        return None;
    }
    let refs: PgVec<'_, _> = d.marks.iter().filter(|r| r.rti != m.rti).collect_in(gx.mcx);
    if refs.iter().any(|r| r.parent || r.ctid_col <= 0 || r.mark != RowMarkType::ROW_MARK_REFERENCE) {
        return None;
    }
    let plain = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    let mut fetches: PgVec<'g, (usize, i16, u32)> = PgVec::new_in(gx.mcx);
    let mut spools: PgVec<'_, u8> = PgVec::new_in(gx.mcx);
    for (i, o) in p.ops[..j].iter().enumerate() {
        match o {
            o if plain(o) => {}
            // D3379 REFNEST-SUB: a per-row SubPlan step past a nest (epq_ir.rs `Redo::Sub`)
            Op::SubPlan(_) if !fetches.is_empty() => {}
            Op::Join(JoinDesc::Nest { .. }) => {
                let (x, r) = match refs.iter().find_map(|r| ref_side(gx, k, i, r).map(|x| (x, r))) {
                    Some(f) => f,
                    // or a Materialize over its unparameterized scan (`form`'s `kept`: Django's claim filtered on the
                    // joined relation's column), the redo that producer's ops up to its spool
                    None => {
                        let (f, slot) = ref_spool(gx, k, i)?;
                        spools.push_in(slot);
                        (f, refs.iter().find(|r| heap(&ps[f].source) == Some((r.rti, r.relid)))?)
                    }
                };
                if fetches.iter().any(|f| f.1 == r.ctid_col) {
                    return None;
                }
                fetches.push_in((x, r.ctid_col, r.relid));
            }
            _ => return None,
        }
    }
    // the statement's pipelines: the lock's, the spools' producers, the InitPlans'
    let own = Census(gx).whole(1 + n + spools.len()) && p.desc.consumes.len() == spools.len() && p.desc.consumes.iter().all(|s| spools.contains(s));
    (own && fetches.len() >= 2 && fetches.len() == refs.len()).then_some((m.ctid_col, fetches))
}

/// D3564: nest op `i` of pipeline `k`, INNER or LEFT, whose inner is a Materialize's emit passed through, its producer
/// (before `k`, reading nothing, no InitPlan's member) a relation's own heap scan under filters and projections filling
/// the spool: Some((that producer, the spool's slot)).
pub(crate) fn ref_spool(gx: &Gx<'_, '_>, k: usize, i: usize) -> Option<(usize, u8)> {
    let ps = gx.pipelines();
    let Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER | JoinType::JOIN_LEFT, inner, .. }) = ps[k].ops.get(i)? else { return None };
    let Source::Breaker { slot } = inner.source else { return None };
    let f = gx.life_of(k, slot).map(|l| l.0 as usize).filter(|&x| x < k)?;
    let pf = &ps[f];
    let end = inner_end(pf);
    let below = pf.ops[..end].iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) && matches!(pf.ops[..end].last(), Some(Op::Project(_)));
    let fill = matches!(pf.ops.last(), Some(Op::Join(JoinDesc::Spool))) && pf.desc.consumes.is_empty() && !Census(gx).init_member(f);
    (below && fill && inner.ops.iter().all(pass_through) && heap(&pf.source).is_some()).then_some((f, slot))
}

/// D3554 SORTNEST (Django's claim over a join sorted past its index: `SELECT t.id, q.name .. FROM app_task t JOIN app_queue
/// q ON q.id = t.queue_id WHERE .. ORDER BY t.priority DESC, t.run_at, t.id LIMIT 10 FOR UPDATE OF t SKIP LOCKED`): Limit
/// -> LockRows -> (Incremental) Sort -> Nested Loop, `form`'s nest below a Sort. PostgreSQL's EvalPlanQual reruns the
/// Sort's subtree over the test tuple and the REFERENCE row it refetches at the row's junk ctid, as REFNEST's, and LockRows
/// hands the result to the Limit. Here the lock's pipeline holds only the Sort emit's projection below the lock (`emit`)
/// and projections and at least one LIMIT / OFFSET above it; the Sort's producer `q` is `form`'s nest pipeline over the
/// locked mark's own heap scan up to the sort build (filters, gates and projections around the one INNER nest, the last
/// op a projection), its inner the REFERENCE relation's own heap scan (a Memoize, or a Materialize whose producer fills the
/// spool alone). The recheck is REFNEST's redo over `q`'s ops before the sort build (epq_ir.rs `Fetch`). AGA-35 form 5
/// alike: beside InitPlans that lock and write nothing, none of the form's pipelines theirs. D3555 (`cte`): or the lock
/// pipeline a CTE's store (`WITH c AS (SELECT t.id FROM .. JOIN .. ORDER BY .. LIMIT 4 FOR UPDATE OF t SKIP LOCKED)
/// UPDATE t .. FROM c ..`), every InitPlan a CTE, the other pipelines the statement's; ops/lockrows/plan.rs `Plan::form` then asks
/// `write_pair` for the write reading the store (CTESORT's form). Some((q, its ops before the sort build, the lock's
/// position and fetch)).
pub(crate) fn sort_form(gx: &Gx<'_, '_>, k: usize, j: usize, d: &LockDesc<'_>, (limit, emit): (fn(&Op<'_>) -> bool, fn(&Op<'_>) -> bool), cte: bool) -> Option<(usize, usize, RefNest)> {
    let ps = gx.pipelines();
    let p = &ps[k];
    let Source::Breaker { slot } = p.source else { return None };
    if !p.ops[..j].iter().all(emit) || !p.ops[j + 1..].iter().all(|o| matches!(o, Op::Project(_)) || limit(o)) || !p.ops[j + 1..].iter().any(limit) {
        return None;
    }
    if d.family().is_some() || d.family_top().is_some() || d.marks.len() != 2 || p.desc.produces().is_some() != cte || p.desc.consumes.len() != 1 {
        return None;
    }
    let q = (0..k).rev().find(|&x| ps[x].desc.produces() == Some(slot))?;
    // D3555 (`cte`): the CTE's store beside the statement's pipelines, every InitPlan a CTE (ops/lockrows/plan.rs `Plan::form` then asks
    // `write_pair` for the one write reading the store whole); else the statement's pipelines are the form's and those of
    // InitPlans that lock and write nothing
    let n = match cte {
        true => (gx.cte_write(k) && Census(gx).ctes_only()).then_some(None)?,
        false => Some(Census(gx).inits(k, q)?),
    };
    let pq = &ps[q];
    let m = d.marks.iter().find(|m| m.lock != LockKind::None)?;
    let r = d.marks.iter().find(|m| m.lock == LockKind::None)?;
    if Census(gx).init_member(q) || m.parent || m.ctid_col <= 0 || m.relid != d.plan.target_rel || r.parent || r.ctid_col <= 0 || r.mark != RowMarkType::ROW_MARK_REFERENCE || r.rti == m.rti || heap(&pq.source) != Some((m.rti, m.relid)) {
        return None;
    }
    let jq = pq.ops.iter().position(|o| matches!(o, Op::Sort(_)))?;
    if !pq.ops[jq..].iter().all(|o| matches!(o, Op::Sort(_))) || !matches!(pq.ops[..jq].last(), Some(Op::Project(_))) {
        return None;
    }
    let plain = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    let jn = pq.ops[..jq].iter().position(|o| !plain(o))?;
    let Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, inner, .. }) = &pq.ops[jn] else { return None };
    if !pq.ops[jn + 1..jq].iter().all(plain) {
        return None;
    }
    let i = ps.inners().iter().position(|x| x.0 == (q, jn))?;
    let f = match inner.source {
        Source::Breaker { slot: s } => {
            let f = gx.life_of(q, s).map(|l| l.0 as usize).filter(|&x| x < q)?;
            let alone = n.is_none_or(|n| Census(gx).whole(3 + n)) && pq.desc.consumes.len() == 1 && pq.desc.consumes[0] == s && ps[f].desc.consumes.is_empty() && !Census(gx).init_member(f);
            if !alone || !inner.ops.iter().all(pass_through) || !matches!(ps[f].ops.last(), Some(Op::Join(JoinDesc::Spool))) {
                return None;
            }
            f
        }
        _ if n.is_none_or(|n| Census(gx).whole(2 + n)) && pq.desc.consumes.is_empty() => ps.len() + i,
        _ => return None,
    };
    let s = &ps[f];
    let end = inner_end(s);
    let below = s.ops[..end].iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) && matches!(s.ops[..end].last(), Some(Op::Project(_)));
    if heap(&s.source) != Some((r.rti, r.relid)) || !below || !s.ops[end..].iter().skip(1).all(pass_through) {
        return None;
    }
    Some((q, jq, RefNest { ctid: m.ctid_col, fetch: (f, r.ctid_col, r.relid) }))
}

/// D3379 LOCKNEST (REFNEST's mirror: `SELECT .. FROM a JOIN b ON b.id = a.bid .. LIMIT 3 FOR UPDATE OF b`):
/// Limit -> LockRows -> Nested Loop, the outer the REFERENCE-marked relation's own heap scan, the inner the locked
/// relation's parameterized heap scan (a Memoize over it once the tables are analyzed). The runner's recheck (lockrows
/// NEST, marks.rs `nest_scan`) would send the rechecked row past the Limit (LIMIT_ABOVE). PostgreSQL's EvalPlanQual reruns
/// the join over its test tuples: the outer scan over the REFERENCE row refetched at its junk ctid (the version the join
/// read, so its quals pass again), the nest's parameters from it, the inner scan over the locked row's newest version
/// rechecked under them (IndexRecheck's indexqualorig, its filters; a fresh Memoize caches nothing), then the join qual,
/// and LockRows hands the row to the Limit. Here the recheck runs in the IR (epq_ir.rs `epq_of_inner`, modifytable D2739's
/// redo): the inner's part up to its Memoize over the newest version the lock host left at the lock site, joined to the
/// outer row the nest holds (its parameters still that row's), the join quals, then the ops up to the lock, so the steps
/// above count the rechecked row. Served: the statement's one pipeline (no InitPlan) over the REFERENCE mark's own heap
/// scan, filters, gates and projections calling nothing VOLATILE before one INNER nest whose inner is the locking mark's
/// own heap scan under filters and projections (then, optionally, a Memoize and its pass-through projection), filters,
/// gates and projections up to the lock, projections and at least one LIMIT / OFFSET above it, two marks, and no SubPlan
/// in what the recheck evaluates. Some((the locked mark's junk ctid column, the nest op, its inner's `Pipes` index)).
pub(crate) fn lock_form<'p>(gx: &Gx<'_, 'p>, k: usize, j: usize, d: &LockDesc<'_>, limit: fn(&Op<'_>) -> bool) -> Option<(i16, usize, usize)> {
    let p = &gx.pipelines()[k];
    let m = d.marks.iter().find(|m| m.lock != LockKind::None)?;
    let r = d.marks.iter().find(|m| m.lock == LockKind::None)?;
    if d.marks.len() != 2 || m.parent || m.ctid_col <= 0 || m.relid != d.plan.target_rel || r.parent || r.ctid_col <= 0 || r.mark != RowMarkType::ROW_MARK_REFERENCE || r.rti == m.rti || heap(&p.source) != Some((r.rti, r.relid)) {
        return None;
    }
    let (jn, ik) = limited_nest(gx, k, j, d, limit, m, false)?;
    // the outer part is not redone: PostgreSQL's rerun over the version the join read passes it again, as it did, unless it
    // calls what may answer otherwise
    if p.ops[..jn].iter().flat_map(exprs).any(|x| crate::effects::volatile(gx, x)) {
        return None;
    }
    Some((m.ctid_col, jn, ik))
}

/// D3780 LOCKCHAIN (realapp-typeorm: a find locking a relation of a relation, `SELECT post.id, categories.id,
/// images.id, .. FROM post JOIN post_categories_category .. JOIN category categories .. JOIN category_images_image ..
/// JOIN image images ON images.id = .."imageId" WHERE post.id = $1 FOR UPDATE OF images`, its tables never analyzed:
/// LockRows -> Nested Loop (Nested Loop (Nested Loop (the post's pkey scan; Hash Join (category; Hash <-
/// post_categories_category)); category_images_image's pkey scan); image's pkey scan)): LOCKNEST's lock (`lock_form`)
/// on the inner of the last INNER nest below it, its outer any join of relations the statement marks ROW_MARK_REFERENCE
/// (hash joins, nests, their builds, Materializes and inners). PostgreSQL's EvalPlanQual reruns the plan over the test
/// tuples: every REFERENCE row refetched at its junk ctid (SnapshotAny: the very version the join read, none for a NULL
/// one), so the outer joins hand on the row they handed on, as they did (nothing there VOLATILE), and the nest's
/// parameters are that row's; then the inner scan over the locked row's newest version (IndexRecheck's indexqualorig,
/// its filters; a fresh Memoize caches nothing) and the join qual. The outer row is the one the nest holds, so
/// LOCKNEST's redo is that recheck (epq_ir.rs `epq_of_inner`). Served: lock op `j` of the statement's emit `k` (no
/// InitPlan, no family), filters, gates and projections from the nest to the lock, projections and a LIMIT / OFFSET
/// above it; or (REFNEST-FEED's, `form`) `k` feeding a product only the later pipelines read, filters, gates,
/// projections and a LIMIT / OFFSET above the lock, then the product's sink; the inner the locking mark's own heap scan
/// under filters and projections (then, optionally, a Memoize and its pass-through projection), no other scan of that
/// relation, no SubPlan in what the recheck evaluates nor in the outer; every other pipeline and inner neither locks
/// nor writes. With no LIMIT / OFFSET above and no product, only an outer the runner's recheck does not take (a nest
/// whose inner is more than a scan: the Hash Join above; lockrows/marks.rs NOT_OWN); a two-mark outer is `lock_form`'s.
/// Some((the locked mark's junk ctid column, the nest op, its inner's `Pipes` index)).
pub(crate) fn chain_lock_form(gx: &Gx<'_, '_>, k: usize, j: usize, d: &LockDesc<'_>, limit: fn(&Op<'_>) -> bool) -> Option<(i16, usize, usize)> {
    use gravity_decompose::expr::has_subplan;
    let ps = gx.pipelines();
    let p = &ps[k];
    // D3779 REFNEST-FEED's form too: or rows feeding a product only the later pipelines read (`form`'s `feed`)
    let feed = match p.desc.produces() {
        Some(_) => Census(gx).above_only(k).filter(|&a| a > 0)?,
        None => 0,
    };
    if !gx.graph.init_plans.is_empty() || d.family().is_some() || d.family_top().is_some() || !Census(gx).whole(k + 1 + feed) || d.marks.len() < 3 {
        return None;
    }
    let mut locking = d.marks.iter().filter(|m| m.lock != LockKind::None);
    let (Some(m), None) = (locking.next(), locking.next()) else { return None };
    if m.parent || m.ctid_col <= 0 || m.relid != d.plan.target_rel || d.marks.iter().any(|r| r.rti != m.rti && (r.parent || r.mark != RowMarkType::ROW_MARK_REFERENCE)) {
        return None;
    }
    let limited = p.ops[j + 1..].iter().any(limit);
    let plain = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    let above = match feed {
        0 => p.ops[j + 1..].iter().all(|o| matches!(o, Op::Project(_)) || limit(o)),
        _ => j + 2 <= p.ops.len() && p.ops[j + 1..p.ops.len() - 1].iter().all(|o| plain(o) || limit(o)) && !matches!(p.ops.last(), Some(Op::Write(_) | Op::Lock(_))),
    };
    if !above {
        return None;
    }
    let jn = p.ops[..j].iter().rposition(|o| !plain(o))?;
    let Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, qual, .. }) = &p.ops[jn] else { return None };
    let ik = ps.inner_of(k, jn)?;
    let s = &ps[ik];
    let end = inner_end(s);
    if heap(&s.source) != Some((m.rti, m.relid)) || !s.desc.consumes.is_empty() || !s.ops[..end].iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) || !memo_tail(s) {
        return None;
    }
    // the outer: every other pipeline and inner, and the lock pipeline's ops before the nest; none scans the locked
    // relation, locks or writes; none VOLATILE nor a SubPlan (PostgreSQL's rerun over the refetched rows evaluates them)
    let inners = ps.inners();
    // an inner's graph pipeline (its owner's, an inner's inner its owner's in turn): the readers' inners are not the outer
    let root = |mut x: usize| {
        while x >= ps.len() {
            x = inners[x - ps.len()].0 .0;
        }
        x
    };
    let outer: PgVec<'_, &PipelineNode<'_>> = (0..k).map(|x| &ps[x]).chain(inners.iter().enumerate().filter(|&(i, x)| ps.len() + i != ik && root(x.0 .0) <= k).map(|(_, x)| x.1)).collect_in(gx.mcx);
    let nest = |o: &Op<'_>| matches!(o, Op::Join(JoinDesc::Nest { .. }));
    let mut read: PgVec<'_, types_nodes::Node<'_>> = PgVec::new_in(gx.mcx);
    for o in p.ops[..jn].iter().chain(outer.iter().flat_map(|x| x.ops.iter())) {
        match o {
            Op::Filter(_) | Op::Gate(_) | Op::Project(_) => read.extend_in(exprs(o)),
            Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER | JoinType::JOIN_LEFT, params, qual, .. }) => read.extend_in(param_values(params).chain(qual.iter())),
            Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_INNER | JoinType::JOIN_LEFT, clauses, keys, qual, .. }) => read.extend_in(clauses.iter().chain(keys.iter()).chain(qual.iter())),
            // a hash build, a Memoize, a Materialize's spool (EvalPlanQual's fresh one holds the refetched row alone)
            Op::Join(JoinDesc::Build { .. } | JoinDesc::Memo { .. } | JoinDesc::Spool) => {}
            _ => return None,
        }
    }
    // each source a REFERENCE relation's heap scan, or a product's emit (a Materialize's, a Memoize's inner)
    let sources = std::iter::once(&p.source).chain(outer.iter().map(|x| &x.source));
    for src in sources {
        match (src, heap(src)) {
            (Source::Breaker { .. }, _) => {}
            (_, Some((rti, _))) if rti != m.rti => read.extend_in(recheck_quals(src)),
            _ => return None,
        }
    }
    if read.iter().any(|&x| has_subplan(x) || crate::effects::volatile(gx, x)) {
        return None;
    }
    // with no LIMIT / OFFSET above, the runner's recheck keeps every outer it takes (a nest over one scan, a hash probe)
    let runner = p.ops[..jn].iter().all(|o| plain(o) || !nest(o) || crate::rerun::epq::marks::nest_joins(o) || crate::rerun::epq::marks::nest_memo_joins(o));
    if !limited && feed == 0 && runner {
        return None;
    }
    let recheck = recheck_quals(&s.source).into_iter().chain(s.ops[..end].iter().flat_map(exprs)).chain(qual.iter()).chain(p.ops[jn + 1..j].iter().flat_map(exprs));
    recheck.into_iter().all(|x| !has_subplan(x)).then_some((m.ctid_col, jn, ik))
}

/// D3601 (6) COPYNEST (River's completer claim under the plan a large job table and a small batch get: `job_to_update
/// AS (SELECT river_job.id, job_input.attempt, .. FROM river_job JOIN job_input ON river_job.id = job_input.id WHERE
/// river_job.state = 'running' OR job_input.metadata_do_merge FOR UPDATE)` as LockRows -> Nested Loop (CTE Scan of
/// job_input; Index Scan of river_job by the job's id)): COPYPROBE's lock (ref_copy.rs `probe_form`: no LIMIT / OFFSET,
/// inside a CTE that writes nothing, every InitPlan a CTE) over one INNER nest whose outer is the lock's one COPY
/// mark's stored row (its copy entry at or before the nest, copy_row.rs `boundary`) under filters, gates and
/// projections calling nothing VOLATILE, and whose inner is the locking mark's own heap scan under filters and
/// projections (then, optionally, a Memoize and its pass-through projection); filters, gates and projections up to the
/// lock, projections above it, no SubPlan in what the recheck evaluates, neither pipeline in a rerun set. PostgreSQL's
/// EvalPlanQual hands the CTE Scan the COPY mark's stored row (the CTE never runs again), takes the nest's parameters
/// from it and reruns the inner scan over the locked row's newest version (IndexRecheck's indexqualorig, its filters; a
/// fresh Memoize caches nothing), then the join qual. The stored row is the one the nest holds, so LOCKNEST's redo is
/// that recheck (epq_ir.rs `epq_of_inner`). Some((the locked mark's junk ctid column, the nest op, its inner's `Pipes`
/// index)).
pub(crate) fn copy_nest_form(gx: &Gx<'_, '_>, k: usize, j: usize, d: &LockDesc<'_>) -> Option<(i16, usize, usize)> {
    use gravity_decompose::expr::has_subplan;
    if d.family().is_some() || d.family_top().is_some() || d.marks.len() != 2 {
        return None;
    }
    let m = d.marks.iter().find(|m| m.lock != LockKind::None)?;
    let c = d.marks.iter().find(|m| m.lock == LockKind::None)?;
    if m.parent || m.ctid_col <= 0 || m.relid != d.plan.target_rel || c.parent || c.mark != RowMarkType::ROW_MARK_COPY || c.rti == m.rti {
        return None;
    }
    let ps = gx.pipelines();
    let p = &ps[k];
    let rerun = |x: usize| gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&y| usize::from(y) == x));
    let cte = gx.cte_write(k) && p.desc.produces().is_some() && Census(gx).ctes_only();
    if !cte || rerun(k) || !p.ops[j + 1..].iter().all(|o| matches!(o, Op::Project(_))) {
        return None;
    }
    let plain = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    let jn = p.ops[..j].iter().position(|o| !plain(o))?;
    let Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, qual, .. }) = &p.ops[jn] else { return None };
    if !p.ops[jn + 1..j].iter().all(plain) {
        return None;
    }
    // the outer: the COPY mark's stored row entering this pipeline at or before the nest; its part is not redone (the
    // stored row passes it again, as it did), unless it calls what may answer otherwise
    let entry = gx.graph.record_rows.iter().find(|r| r.scanrelid == c.rti).and_then(|r| crate::rerun::epq::copy_row::boundary(gx, k, r));
    if !entry.is_some_and(|at| at <= jn) || p.ops[..jn].iter().flat_map(exprs).any(|x| crate::effects::volatile(gx, x)) {
        return None;
    }
    let ik = ps.inner_of(k, jn)?;
    let s = &ps[ik];
    let end = inner_end(s);
    if rerun(ik) || heap(&s.source) != Some((m.rti, m.relid)) || !s.ops[..end].iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) || !memo_tail(s) {
        return None;
    }
    let read = recheck_quals(&s.source).into_iter().chain(s.ops[..end].iter().flat_map(exprs)).chain(qual.iter()).chain(p.ops[jn + 1..j].iter().flat_map(exprs));
    read.into_iter().all(|x| !has_subplan(x)).then_some((m.ctid_col, jn, ik))
}

/// D3379 LOCKPAIR (LOCKNEST with the outer locked too: `SELECT .. FROM a JOIN b ON b.id = a.bid .. LIMIT 3 FOR UPDATE`):
/// Limit -> LockRows -> Nested Loop whose outer (this pipeline's own heap scan) and inner relations are both locked.
/// ExecLockRows locks each row's marks in PlanRowMark order; when a lock traversed to a newer version, EvalPlanQual reruns
/// the join over every mark's test tuple (the newest version where its lock traversed, else the version it locked): the
/// outer scan's quals over the outer's, the nest's parameters from it, the inner's index condition and filters over the
/// inner's under them, then the join qual, and LockRows hands the row to the Limit. Here each lock is a `Program.locks`
/// entry over a write site, a version its lock did not traverse is fetched into the site (`LockSite::FETCH`), and the
/// recheck runs in the IR over the two sites (epq_ir.rs `epq_of_pair`). Served: `lock_form`'s nest with two locking marks,
/// the outer's on the pipeline's own scan (its ops are redone, as PostgreSQL's, so they may call anything); D3379
/// PAIRPROBE: or that nest in decompose's catalog form (`limited_probe`); D3556 PAIRSPOOL: or the inner a Materialize
/// (`limited_nest`'s `spool`: ActiveRecord's `.joins(..).lock(..)`, a Join Filter over a Materialize of the joined
/// relation's scan); D3777 PAIRMERGE: or PostgreSQL's Merge Join of the two (`limited_merge`). Some((the outer's mark, the
/// inner's: their indexes in `d.marks`, the lock order; the nest op, or the catalog probe, or the merge; its inner's
/// `Pipes` index, or the build's pipeline, or the merge inner's; the merge outer's Sort producer and its sort build op
/// when that side is sorted)).
#[allow(clippy::type_complexity)]
pub(crate) fn pair_form<'p>(gx: &Gx<'_, 'p>, k: usize, j: usize, d: &LockDesc<'_>, limit: fn(&Op<'_>) -> bool) -> Option<(usize, usize, usize, usize, Option<(usize, usize)>)> {
    let p = &gx.pipelines()[k];
    if d.marks.len() != 2 || d.marks.iter().any(|m| m.parent || m.ctid_col <= 0 || m.lock == LockKind::None) || d.marks[0].rti == d.marks[1].rti {
        return None;
    }
    if let Some((io, jn, ik, so)) = limited_merge(gx, k, j, d, limit) {
        return Some((io, 1 - io, jn, ik, so));
    }
    let io = d.marks.iter().position(|m| heap(&p.source) == Some((m.rti, m.relid)))?;
    let (jn, ik) = limited_nest(gx, k, j, d, limit, &d.marks[1 - io], true).or_else(|| limited_probe(gx, k, j, d, limit, &d.marks[1 - io]))?;
    Some((io, 1 - io, jn, ik, None))
}

/// D3777 PAIRMERGE (`pair_form`; realapp-django: `select_for_update()` then `get()` across a relation over its test
/// database's plan, `SELECT sm.id, sm.field FROM sm JOIN mms ON sm.id = mms.sm_id WHERE mms.mm_id = 1 AND sm.field = 2
/// LIMIT 21 FOR UPDATE`: Limit -> LockRows -> Merge Join (Sort -> Seq Scan sm, Sort -> Bitmap Heap Scan mms)): an INNER
/// merge join of the two locked relations, each side its relation's own heap scan under filters and projections
/// (`merge_side`), in the clauses' order (an Index Scan; the inner's spooled) or under a Sort. PostgreSQL's
/// EvalPlanQual reruns the Merge Join over the two test tuples outer first (EXEC_MJ_INITIALIZE_OUTER: an outer that
/// fails its quals never runs the inner), the inner next, then the merge clauses and the join qual; a Sort over one
/// test tuple hands it on. The redo runs in that order (epq_ir.rs `epq_of_pair`): the outer's part (a Sort's producer's
/// ops before its sort build, then this pipeline's ops before the merge), the inner's ops before its Sort over the
/// inner lock site's cells, then the merge clauses and join qual. PostgreSQL never runs the inner for an outer whose
/// merge key is NULL (MJEvalOuterValues), so the inner's part is raise-free. Lock op `j` of pipeline `k`, the
/// statement's pipelines the two sides' and `k` (no InitPlan, no family, no product), filters, gates and projections
/// around the merge up to the lock, projections and at least one LIMIT / OFFSET above, and no SubPlan in what the
/// recheck evaluates. Some((the outer's mark in `d.marks`, the merge op, the inner's pipeline, the outer's Sort
/// producer and its sort build op when that side is sorted)).
#[allow(clippy::type_complexity)]
fn limited_merge<'p>(gx: &Gx<'_, 'p>, k: usize, j: usize, d: &LockDesc<'_>, limit: fn(&Op<'_>) -> bool) -> Option<(usize, usize, usize, Option<(usize, usize)>)> {
    use gravity_decompose::expr::has_subplan;
    let ps = gx.pipelines();
    let p = &ps[k];
    if !gx.graph.init_plans.is_empty() || d.family().is_some() || d.family_top().is_some() || p.desc.produces().is_some() {
        return None;
    }
    if !p.ops[j + 1..].iter().all(|o| matches!(o, Op::Project(_)) || limit(o)) || !p.ops[j + 1..].iter().any(limit) {
        return None;
    }
    let plain = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    let jn = p.ops[..j].iter().position(|o| !plain(o))?;
    let Op::Join(JoinDesc::Merge { slot, jointype: JoinType::JOIN_INNER, clauses, qual, .. }) = &p.ops[jn] else { return None };
    if !p.ops[jn + 1..j].iter().all(plain) {
        return None;
    }
    let ik = gx.life_of(k, *slot).map(|l| l.0 as usize).filter(|&x| x < k)?;
    let ie = merge_side(&ps[ik])?;
    let so = match p.source {
        Source::Breaker { slot: os } => {
            let q = gx.life_of(k, os).map(|l| l.0 as usize).filter(|&q| q < k && q != ik)?;
            let qe = merge_side(&ps[q]).filter(|&e| e < ps[q].ops.len())?;
            Some((q, qe))
        }
        _ => None,
    };
    let sorted = usize::from(so.is_some());
    if p.desc.consumes.len() != 1 + sorted || !p.desc.consumes.contains(slot) || !Census(gx).whole(2 + sorted) {
        return None;
    }
    let (oq, oe) = so.unwrap_or((k, 0));
    let io = d.marks.iter().position(|m| heap(&ps[oq].source) == Some((m.rti, m.relid)))?;
    let m = &d.marks[1 - io];
    if heap(&ps[ik].source) != Some((m.rti, m.relid)) {
        return None;
    }
    let part = |x: usize, e: usize| recheck_quals(&ps[x].source).into_iter().chain(ps[x].ops[..e].iter().flat_map(exprs));
    let mut read = part(oq, oe).chain(p.ops[..j].iter().flat_map(exprs)).chain(part(ik, ie)).chain(clauses.iter()).chain(qual.iter());
    if read.any(has_subplan) {
        return None;
    }
    part(ik, ie).all(|x| crate::effects::raise_free(gx, x)).then_some((io, jn, ik, so))
}

/// D3777 (lockrows lower.rs `ref_merge` / `merge_tail`): whether pipeline `s`'s recheck quals or ops before op `end` (its
/// filters' quals, its projections' entries) hold a SubPlan.
pub(crate) fn part_reads_subplan(s: &PipelineNode<'_>, end: usize) -> bool {
    recheck_quals(&s.source).into_iter().chain(s.ops[..end].iter().flat_map(exprs)).any(gravity_decompose::expr::has_subplan)
}

/// D3777 (`limited_merge`, lockrows lower.rs `ref_merge` / `merge_tail`): a merge join side's pipeline `s`, its relation's
/// own heap scan under filters and projections, then its Sort or nothing (the spool of a side read in the clauses'
/// order), reading no slot: Some(its ops' end before the Sort).
pub(crate) fn merge_side(s: &PipelineNode<'_>) -> Option<usize> {
    let end = s.ops.iter().position(|o| matches!(o, Op::Sort(_))).unwrap_or(s.ops.len());
    let below = s.ops[..end].iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) && matches!(s.ops[..end].last(), Some(Op::Project(_)));
    (below && s.ops[end..].iter().all(|o| matches!(o, Op::Sort(_))) && s.desc.consumes.is_empty() && heap(&s.source).is_some()).then_some(end)
}

/// D3379 PAIRPROBE (`pair_form`; lockrows_d3379_lock_pair's `.. JOIN d80pb b ON b.id = a.bid WHERE b.w = 1 .. LIMIT 6 FOR
/// UPDATE`): `limited_nest`'s nest answered by decompose's catalog lookup (join.rs `catalog_lookup` / `map_catalog`, or
/// nest.eqkey's keyed form, self_spool.rs `keyed`): a parameterless INNER nest whose inner, mark `m`'s own heap scan under
/// filters and projections, is read once into a hash build that this pipeline probes (`catalog`), the build waiting for the
/// first outer row. PostgreSQL's plan is still the Nest Loop, whose EvalPlanQual reruns the outer scan over its test tuple,
/// then (only for a row that passes) the inner scan over the inner's test tuple, then the join qual: the IR's redo in that
/// order is the outer's ops, the build's ops before its Build over the inner lock site's cells, then the probe's clauses and
/// join qual (epq_ir.rs `epq_of_pair`). Lock op `j` of pipeline `k`, the statement's two pipelines (the build's and `k`: no
/// InitPlan, no family, no product), filters, gates and projections before the probe and after it up to the lock,
/// projections and at least one LIMIT / OFFSET above, and no SubPlan in what the recheck evaluates. D3727 PAIRHASH
/// (realapp-django: `select_for_update()` then `get()` across a relation, `.. JOIN mms ON sm.id = mms.sm_id WHERE
/// mms.mm_id = 1 AND sm.field = 2 LIMIT 21 FOR UPDATE`): or PostgreSQL's own Hash Join (not the catalog form), the build
/// the other relation's scan (a Bitmap Heap Scan's too). Its EvalPlanQual reruns the Hash Join over the two test tuples,
/// outer first or the Hash first as the plan's costs say (ExecHashJoin's empty-outer test), so the redo's outer-first order
/// is PostgreSQL's in every way only when nothing either side's part evaluates can raise: both scans' recheck quals and the
/// ops before the probe and before the build raise-free (the hash clauses and join qual run over the two rows either way).
/// Some((the probe op, the build's pipeline)).
fn limited_probe<'p>(gx: &Gx<'_, 'p>, k: usize, j: usize, d: &LockDesc<'_>, limit: fn(&Op<'_>) -> bool, m: &gravity_decompose::ops::shared::modifytable::RowMark) -> Option<(usize, usize)> {
    use gravity_decompose::expr::has_subplan;
    let ps = gx.pipelines();
    let p = &ps[k];
    if !gx.graph.init_plans.is_empty() || !Census(gx).whole(2) || d.family().is_some() || d.family_top().is_some() || p.desc.produces().is_some() {
        return None;
    }
    if !p.ops[j + 1..].iter().all(|o| matches!(o, Op::Project(_)) || limit(o)) || !p.ops[j + 1..].iter().any(limit) {
        return None;
    }
    let plain = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    let jn = p.ops[..j].iter().position(|o| !plain(o))?;
    let Op::Join(JoinDesc::Probe { slot, jointype: JoinType::JOIN_INNER, clauses, qual, catalog, .. }) = &p.ops[jn] else { return None };
    if !p.ops[jn + 1..j].iter().all(plain) || p.desc.consumes.len() != 1 || p.desc.consumes[0] != *slot {
        return None;
    }
    let q = (0..ps.len()).find(|&x| x != k && ps[x].desc.produces() == Some(*slot))?;
    let s = &ps[q];
    let jb = s.ops.iter().position(|o| matches!(o, Op::Join(JoinDesc::Build { .. })))?;
    if jb + 1 != s.ops.len() || !s.desc.consumes.is_empty() || heap(&s.source) != Some((m.rti, m.relid)) || !s.ops[..jb].iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) {
        return None;
    }
    let read = recheck_quals(&p.source).into_iter().chain(p.ops[..j].iter().flat_map(exprs)).chain(recheck_quals(&s.source)).chain(s.ops[..jb].iter().flat_map(exprs)).chain(clauses.iter()).chain(qual.iter());
    if !read.into_iter().all(|x| !has_subplan(x)) {
        return None;
    }
    // D3727 PAIRHASH: PostgreSQL's Hash Join reruns its two sides in an order of its costs; both parts raise-free
    let sides = recheck_quals(&p.source).into_iter().chain(p.ops[..jn].iter().flat_map(exprs)).chain(recheck_quals(&s.source)).chain(s.ops[..jb].iter().flat_map(exprs));
    (*catalog || sides.into_iter().all(|x| crate::effects::raise_free(gx, x))).then_some((jn, q))
}

/// The nest `lock_form` and `pair_form` take: lock op `j` of pipeline `k`, the statement's one pipeline (no InitPlan, no
/// family, no product), filters, gates and projections before one INNER nest whose inner is mark `m`'s own heap scan
/// under filters and projections (then, optionally, a Memoize and its pass-through projection), filters, gates and
/// projections up to the lock, projections and at least one LIMIT / OFFSET above it, and no SubPlan in what a recheck
/// evaluates (the pipeline's and the inner's recheck quals and ops, the join quals). D3556 PAIRSPOOL (`spool`): or the
/// inner a Materialize's emit passed through, the statement's other pipeline that scan under filters and projections
/// filling the spool alone (REFNEST's kept form): PostgreSQL's EvalPlanQual reruns the Materialize over the inner's test
/// tuple (a fresh one in the EPQ tree, no parameters), so the redo is that pipeline's ops up to its spool. Some((the nest
/// op, its inner's `Pipes` index, or the spool's producer)).
#[allow(clippy::too_many_arguments)]
fn limited_nest<'p>(gx: &Gx<'_, 'p>, k: usize, j: usize, d: &LockDesc<'_>, limit: fn(&Op<'_>) -> bool, m: &gravity_decompose::ops::shared::modifytable::RowMark, spool: bool) -> Option<(usize, usize)> {
    use gravity_decompose::expr::has_subplan;
    let ps = gx.pipelines();
    let p = &ps[k];
    if !gx.graph.init_plans.is_empty() || d.family().is_some() || d.family_top().is_some() || p.desc.produces().is_some() {
        return None;
    }
    if !p.ops[j + 1..].iter().all(|o| matches!(o, Op::Project(_)) || limit(o)) || !p.ops[j + 1..].iter().any(limit) {
        return None;
    }
    let plain = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    let jn = p.ops[..j].iter().position(|o| !plain(o))?;
    let Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, qual, .. }) = &p.ops[jn] else { return None };
    if !p.ops[jn + 1..j].iter().all(plain) {
        return None;
    }
    let ik = ps.inner_of(k, jn)?;
    let (ik, kept) = match ps[ik].source {
        Source::Breaker { slot } if spool => {
            let f = gx.life_of(k, slot).map(|l| l.0 as usize).filter(|&x| x < k)?;
            let alone = p.desc.consumes.len() == 1 && p.desc.consumes[0] == slot && ps[f].desc.consumes.is_empty() && ps[ik].ops.iter().all(pass_through);
            (alone && matches!(ps[f].ops.last(), Some(Op::Join(JoinDesc::Spool)))).then_some((f, true))?
        }
        _ => (ik, false),
    };
    if !Census(gx).whole(1 + usize::from(kept)) {
        return None;
    }
    let s = &ps[ik];
    let end = inner_end(s);
    let memo = matches!(s.ops.get(end), Some(Op::Join(JoinDesc::Memo { .. })));
    let tail = match kept {
        true => end + 1 == s.ops.len() && matches!(s.ops[end], Op::Join(JoinDesc::Spool)),
        false => end == s.ops.len() || memo && s.ops[end + 1..].iter().all(pass_through),
    };
    if heap(&s.source) != Some((m.rti, m.relid)) || !s.ops[..end].iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) || !tail {
        return None;
    }
    let read = recheck_quals(&p.source).into_iter().chain(p.ops[..j].iter().flat_map(exprs)).chain(recheck_quals(&s.source)).chain(s.ops[..end].iter().flat_map(exprs)).chain(qual.iter());
    read.into_iter().all(|x| !has_subplan(x)).then_some((jn, ik))
}

/// D3567 REFCHAIN (lower.rs `ref_chain`; D3596: LEFT too), D3564 (`chain_form`): the INNER or LEFT nest at op `i` of
/// pipeline `q` whose inner is REFERENCE mark `r`'s own heap scan under filters and a projection (then, optionally, a
/// Memoize and its pass-through projection), embedded in the nest (no Materialize's spool): Some(the inner's `Pipes`
/// index), else none.
pub(crate) fn ref_side(gx: &Gx<'_, '_>, q: usize, i: usize, r: &gravity_decompose::ops::shared::modifytable::RowMark) -> Option<usize> {
    let ps = gx.pipelines();
    let Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER | JoinType::JOIN_LEFT, inner, .. }) = ps[q].ops.get(i)? else { return None };
    if matches!(inner.source, Source::Breaker { .. }) || heap(&inner.source) != Some((r.rti, r.relid)) {
        return None;
    }
    let end = inner_end(inner);
    let below = inner.ops[..end].iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) && matches!(inner.ops[..end].last(), Some(Op::Project(_)));
    (below && inner.ops[end..].iter().skip(1).all(pass_through)).then(|| ps.inner_of(q, i)).flatten()
}

/// A source's recheck quals (an index or bitmap heap scan's indexqualorig / bitmapqualorig), else none.
pub(crate) fn recheck_quals<'p>(s: &Source<'p>) -> impl Iterator<Item = types_nodes::Node<'p>> {
    let list = match s {
        Source::Index { recheck, .. } | Source::Bitmap { recheck, .. } => Some(*recheck),
        _ => None,
    };
    list.into_iter().flat_map(|l| l.iter())
}

/// An op's expressions: a filter's or gate's quals, a projection's target entries; none for any other op.
pub(crate) fn exprs<'p>(o: &Op<'p>) -> impl Iterator<Item = types_nodes::Node<'p>> {
    let (list, one) = match o {
        Op::Filter(x) | Op::Gate(x) => match x.as_list() {
            Some(l) => (Some(l), None),
            None => (None, Some(*x)),
        },
        Op::Project(l) => (Some(*l), None),
        _ => (None, None),
    };
    list.into_iter().flat_map(|l| l.iter()).chain(one)
}

/// The ops of the REFERENCE relation's pipeline the recheck redoes: those before its Memoize (a cache of the rows below it;
/// its projection passes them through, `form`) or its Materialize's spool, else all.
pub(crate) fn inner_end(inner: &PipelineNode<'_>) -> usize {
    inner.ops.iter().position(|o| matches!(o, Op::Join(JoinDesc::Memo { .. } | JoinDesc::Spool))).unwrap_or(inner.ops.len())
}

/// D3686: the inner's ops from `inner_end` on are none, or its Memoize and the Memoize's pass-through projection: the
/// recheck redoes the ops before it and reads the same row.
pub(crate) fn memo_tail(inner: &PipelineNode<'_>) -> bool {
    let end = inner_end(inner);
    end == inner.ops.len() || matches!(inner.ops[end], Op::Join(JoinDesc::Memo { .. })) && inner.ops[end + 1..].iter().all(pass_through)
}

/// The nest's parameters (its NestLoopParams), each compiled at the nest's node `n` over the row so far, as nest.loop
/// computes them from its outer row (ops/nestloop/lower.rs), for any row (no per-code form).
pub(crate) fn params<'g, 'p>(px: &mut Px<'g, 'p>, params: &'p NodeList<'p>, n: i32, ex: &'static ExprFace) -> PgVec<'g, (i32, ExId)> {
    let mcx = px.mcx();
    params.iter().filter_map(|x| x.as_nest_loop_param()).map(|x| (x.paramno, ex.0.compile(px, x.paramval, None, n))).collect_in(mcx)
}

/// The nest's parameters' values (its NestLoopParams' expressions over the outer row), for lock_init.rs `Part::Nest`.
pub(crate) fn param_values<'p>(params: &'p NodeList<'p>) -> impl Iterator<Item = types_nodes::Node<'p>> + 'p {
    params.iter().filter_map(|x| x.as_nest_loop_param()).map(|x| x.paramval)
}

/// The parameters' words as they stand, each parameter: (id, value word, flags word).
pub(crate) type Saved<'e> = PgVec<'e, (i32, Value, Value)>;

/// The parameters (`params`) over row `r` at the nest's node `n` stored into their words (params.rs `store`), the words
/// they held returned for `restore`.
pub(crate) fn set<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, n: i32, params: &[(i32, ExId)], r: &RowVals, ex: &'static ExprFace) -> Saved<'e> {
    let Some(w) = env.words.filter(|_| !params.is_empty()) else { return PgVec::new_in(env.mcx) };
    let mut saved = mcx::reserved_in(params.len(), env.mcx);
    let mut vals = mcx::reserved_in(params.len(), env.mcx);
    let t = b.bconst(true);
    for &(id, e) in params.iter() {
        let (v, valid) = ex.0.emit(b, env, n, r, e);
        vals.push_in((id, v, b.int_bin(gravity_ir::IntBinOp::Xor, valid, t)));
    }
    for &(id, v, null) in vals.iter() {
        let (v0, f0) = (b.load(Type::I64, w, crate::rerun::params::word(id, 0)), b.load(Type::I64, w, crate::rerun::params::word(id, 1)));
        saved.push_in((id, v0, f0));
        crate::rerun::params::store(b, env, n, id, v, null);
    }
    env.memo.clear();
    saved
}

/// The words `set` replaced, back as they were (value and flags, the seen bits too).
pub(crate) fn restore(b: &mut FunctionBuilder<'_>, env: &mut Env, saved: &[(i32, Value, Value)]) {
    let Some(w) = env.words.filter(|_| !saved.is_empty()) else { return };
    for &(id, v0, f0) in saved.iter() {
        b.store(w, crate::rerun::params::word(id, 0), v0);
        b.store(w, crate::rerun::params::word(id, 1), f0);
    }
    env.memo.clear();
}

/// The block a failing qual of the inner's redo leaves to (its parameters the step's carry, `types`): the words restored,
/// then `l_out`; `l_out` itself when nothing was set.
pub(crate) fn fail_to(b: &mut FunctionBuilder<'_>, env: &mut Env, saved: &[(i32, Value, Value)], l_out: Block, types: &[Type]) -> Block {
    if saved.is_empty() {
        return l_out;
    }
    let (here, l_fail) = (b.current(), b.create_block(types));
    b.switch_to(l_fail);
    restore(b, env, saved);
    let ps = mcx::to_vec_in(b.block_params(l_fail), env.mcx);
    b.br(l_out, &ps[..]);
    b.switch_to(here);
    l_fail
}

/// A heap scan's (range, bitmap, index) (scanrelid, relid).
pub(crate) fn heap(s: &Source<'_>) -> Option<(u32, u32)> {
    match s {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } => Some((u32::from(*scanrelid), *relid)),
        _ => None,
    }
}

/// A projection handing on the row below it as it came (each column the same column of its child: OUTER_VAR, varattno =
/// resno), as a Memoize's.
fn pass_through(o: &Op<'_>) -> bool {
    match o {
        Op::Project(l) => l.iter().all(|n| n.as_target_entry().is_some_and(|te| te.expr.as_var().is_some_and(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && i32::from(v.varattno) == i32::from(te.resno)))),
        _ => false,
    }
}
