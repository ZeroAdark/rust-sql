//! Kind `join.drain` (M7-plan §3.6; L2; cards/M7.md): the drain source, the Drain op, and what becomes of `m`, the probe
//! pipeline's spool of joined rows. Owner M7-DRAIN. Delegating makers (the module rule): the source is
//! join.inner's SourceMaker row (join/drain_src.rs), the Drain op join.inner's step (an identity: the source sends only
//! the wanted records). The `m` rule (§3.6, Decision 3): the joined rows never go through a row spool. `m`'s end makes the
//! drain pipeline's ops after the Drain op a tail of the probe pipeline (`Px::open_tail`; those ops are made twice) and
//! the drain's end its own: a sink end is **fused** (the probe pipeline is an emit), a plain fold, a full sort or an
//! owned keyed end whose arms continue one slot (source/append.rs's rule, D393 / D393b / V4R-159) is **seeded** as the
//! first of two arms (the Arms protocol over the probe and the drain; D1187: the probe .. the drain, one run with an Append's). The drain then reads
//! only the build (its descriptor drops `m`); its order after the probe is a declared edge (`ProtoKind::Follows`, which
//! `Program::producers_of` yields to every scheduler). A
//! tail op is a filter, a projection, a LIMIT (its counters are union words the two pipelines share: step/limit.rs
//! `Scope::Union`, the union this file's makers declare), a full sort's op (the arms continue its runs), an Append's arm marker
//! with no pruning (D1187) or an inner probe built before the join (a LEFT / ANTI one on PG's outer-first path too); any other refuses by name, as does an end that adds a pass. A build end, an owner-less keyed end or a pipeline
//! between the probe and the drain reading the end's slot is the seeded form not built yet (D1187: other pipelines between,
//! such as another partition's join, are served); any other end has no partial form (M8 by the defer rule).
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::join::JoinDesc;
use types_nodes::jointype::JoinType;
use gravity_decompose::ops::shared::sort::SortForm;
use gravity_decompose::ops::shared::modifytable::TargetKind;
use gravity_decompose::Op;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, Type, Value};
use gravity_program::protos::{Proto, ProtoKind};
use gravity_abi::breaker::{BreakerKind, Spill};
use gravity_program::{Cmd, LifeId, LockKind, Pipeline, Reason as Why};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};

use crate::face::end::{EndMech, Parallel, SinkKind};
use crate::face::kinds::KindId;
use crate::face::source::SourceMech;
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::{EndMaker, Mask, SourceMaker, StepMaker, Win, WordWin};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::graph::{EndKind, Gx};
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::hashjoin::drain::spool_arms::union_record;
use crate::frame::iface::{breaker_of, end_breaker, holds_limit, kind_impl, limit_bounds};
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::JoinDrain, &SourceMaker(source)), (KindId::JoinDrain, &StepMaker(step)), (KindId::JoinDrain, &EndMaker(end))], ..crate::ops::NONE };

/// The plan node op `j` of pipeline `k` belongs to (the join, for its Drain op): the union both copies of the tail share.
fn op_node(gx: &Gx<'_, '_>, k: usize, j: usize) -> i32 {
    let p = &gx.pipelines()[k];
    p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(p.node_id, |n| n.node_id)
}

fn source<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let node = px.gx.pipelines()[k].node_id;
    // A1b: a spooled probe's drain reads the probe's spool, then its drained records (nest/spool_drain.rs)
    if let Some((q, jd)) = spooled_probe(px.gx, k) {
        return px.spool_drain_source(k, q, jd);
    }
    (kind_impl::<SourceMaker>(&node, KindId::JoinInner).0)(px, k)
}

/// Drain pipeline `d`'s probe and Drain op when that probe's drain is spooled (`Gx::spooled_drain`).
fn spooled_probe(gx: &Gx<'_, '_>, d: usize) -> Option<(usize, usize)> {
    let (jd, m) = gx.pipelines()[d].ops.iter().enumerate().find_map(|(j, o)| match o {
        Op::Join(JoinDesc::Drain { matches, .. }) => Some((j, *matches)),
        _ => None,
    })?;
    let k = gx.life_of(d, m)?.0 as usize;
    (gx.spooled_drain(k) == Some((d, jd)) || gx.inner_spooled(k) == Some((d, jd))).then_some((k, jd))
}

impl<'g> Gx<'g, '_> {
    /// Row A1b: probe `k`'s drain `(d, jd)` when join.drain spools it: `end` reaches the `m` rule's refusal (no drained
    /// arms, a sink or an end whose arms continue, a tail op the probe cannot run as its own) and join/drain_spool.rs
    /// names no reason. join/build_spill.rs pairs no spill with it (the pair would remake the probe's end).
    pub(crate) fn spooled_drain(&self, k: usize) -> Option<(usize, usize)> {
        let Some(m) = breaker_of(&self.pipelines()[k].desc) else { return None };
        if self.end_maker(k) != KindId::JoinDrain {
            return None;
        }
        // writesubplan (merge#368 group): a streamed probe union's arms only under the drained-arms MERGE (`merge_drained_arms`)
        let run = self.drained_arms(k);
        let (d, jd) = drain_of(self, run.map_or(k, |r| r.1), m.slot)?;
        // drain (regress updatable_views#865, D2132 narrowed): or a drain into any end but a write whose arms are plain
        // (`plain_drained_arms`)
        if run.is_some_and(|r| self.merge_drained_arms(d).is_none() && !self.plain_drained_arms(r, d)) {
            return None;
        }
        let gd = self.pipelines()[d].desc;
        // core-0187: a RIGHT SEMI / RIGHT ANTI drain alone feeds a breaker end the arms do not take (`alone`)
        let mut lone = false;
        let reach = match (self.end_kind(d), breaker_of(&gd)) {
            (EndKind::Sink, _) => true,
            // D3731: a hashed SubPlan's set fill (`set_fill`): PG's buildSubPlanHash reads its plan to the end before the
            // set answers a probe, and the set is read by membership alone
            (EndKind::Breaker(BreakerKind::Distinct), _) if set_fill(self, d) => {
                lone = true;
                true
            }
            // D2247: a plan Material's fill (nest.spool's eager form appends each drained row; its reader re-reads the store);
            // a drain under a drain: the next right-family join's spool, its probe in the tail (no tail op: it marks)
            // writesubplan (heldout ald_joindrain-0014, rnd-1868): or a CTE's store (nest.cte), always spooled (`cte`)
            // drain (regress domain#507): or a settled union's record (spool_arms.rs `union_record`)
            (EndKind::Breaker(BreakerKind::Materialize), _) => matches!(self.spool_kind(d), KindId::NestSpool | KindId::JoinDrain | KindId::NestCte) || union_record(self, d),
            // D3800: or an UPDATE / DELETE through a nest past the drain over its held row (spool_admit.rs `held_write`)
            (EndKind::RowOp(_), _) => self.a2a_write(d).is_some() && a2a_refusal(self, d).is_none() || self.held_write(d).is_some(),
            (EndKind::Breaker(_), Some(b)) => {
                let arms = continues(&gd) && between(self, k, d, b.slot) && Parallel::Arms.on();
                lone = !arms && alone(self, d, jd);
                arms || lone
            }
            _ => false,
        };
        // D1922 (D3377): a RIGHT SEMI drain into a sink a partial reader may stop, whose probe N1 does not find quiet: the fused
        // form hands nothing on before its probe ends (and its build may pair a spill whose drain parts come after it), so
        // `end` marks it whole-read; the spooled form serves that reader where it streams exactly (`drain_exact`: the probe
        // pulled a joined row at a time, each build row handed on at its first match, `semi_exact`), as PostgreSQL 18.6's
        // Hash Right Semi Join returns each build row at its first match
        let semi_cursor = || {
            self.graph.partial
                && matches!(self.pipelines()[d].ops[jd], Op::Join(JoinDesc::Drain { jointype: JoinType::JOIN_RIGHT_SEMI, .. }))
                && self.end_kind(d) == EndKind::Sink
                && !self.spool_quiet(k)
                && self.drain_exact(k, d, jd)
        };
        let spools = reach && (cte_store(self, d) || init_cell(self, d) || lone || !self.pipelines()[d].ops[jd + 1..].iter().all(tail_op) || semi_cursor()) && self.spool_refusal(k, d, jd).is_none();
        spools.then_some((d, jd))
    }

    /// A4-MATEND (D2371): the probe pipeline of drain `d` when `d` writes a MERGE whose MATCHED rows take their position from
    /// that probe's own scan of the target: one heap target, unrouted, with no ON CONFLICT, locking mark or row lock; the
    /// drain a RIGHT or FULL one (PostgreSQL's Hash Right / Full Join under ExecMerge: the source hashed, the target
    /// probing); its probe pipeline scans the target's own heap (its range table index and relation; a TID scan is left
    /// out) and runs only filters, gates and projections before that probe, its last op, which ends in the drain's spool;
    /// and the drain pipeline runs only filters, gates and projections between the Drain op and the write. The spooled
    /// form writes them: the probe's joined rows spooled (each MATCHED or BY SOURCE row carrying the target's junk ctid, in
    /// PostgreSQL's order), then the drained source rows (a NULL ctid: NOT MATCHED). merge/admit.rs names the probe
    /// pipeline the target's producer (D776's form: the write's position is the junk ctid; a recheck reruns the producers,
    /// the target's scan at the host's newest version, the source's at its REFERENCE row, then the drain pipeline).
    pub(crate) fn merge_drained_target(&self, d: usize) -> Option<usize> {
        use gravity_decompose::{Am, Source};
        let ops = &self.pipelines()[d].ops;
        let wj = ops.iter().position(|o| matches!(o, Op::Write(_)))?;
        let Op::Write(w) = &ops[wj] else { return None };
        let (jd, m) = ops.iter().enumerate().find_map(|(j, o)| match o {
            Op::Join(JoinDesc::Drain { matches, jointype: JoinType::JOIN_RIGHT | JoinType::JOIN_FULL }) => Some((j, *matches)),
            _ => None,
        })?;
        // writesubplan (updatable_views#961-963): or the one result relation under a root with an UPDATE or DELETE action
        // (merge/route.rs's (a): its own scan probes; a NOT MATCHED INSERT goes through the root, `insert_beside`)
        let acts = w.merge.as_ref().is_some_and(|m| m.actions.iter().any(|a| matches!(a.cmd, Some(Cmd::Update | Cmd::Delete))));
        let plain = w.cmd == Cmd::Merge && w.merge.is_some() && w.targets.len() == 1 && w.targets[0].kind == TargetKind::Heap && (w.route.is_none() || acts) && w.on_conflict.is_none();
        let quiet = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
        // mergecmd (ACTION-CTE, regress with#239 / #241): or, between the Drain op and the write, a MERGE action's lazily
        // lent SubPlan (graph.rs `lazy_nested_sub` that only the write's actions read, the plan fact `action_only`; merge
        // admit's `action_sub`, D1112): it runs per drained row as the write's action reads it, and a recheck reruns it over
        // the newest target row (rerun/epq/marks.rs `act_ctes`)
        let tail = |o: &Op<'_>| quiet(o) || crate::graph::lazy_nested_sub(o).is_some_and(|id| w.action_only(id));
        if !plain || jd > wj || !ops[jd + 1..wj].iter().all(tail) || ops.iter().any(|o| matches!(o, Op::Lock(_))) || w.mark_descs.iter().any(|m| m.lock != LockKind::None) {
            return None;
        }
        let k = self.life_of(d, m)?.0 as usize;
        let pk = &self.pipelines()[k];
        let own = match &pk.source {
            Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } => {
                u32::from(*scanrelid) == w.rti && *relid == w.plan.target_rel
            }
            _ => false,
        };
        let (last, before) = pk.ops.split_last()?;
        let probe = matches!(last, Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_RIGHT | JoinType::JOIN_FULL, catalog: false, .. }));
        // (`k` makes `m`: `life_of`)
        if own && probe && before.iter().all(quiet) {
            return Some(k);
        }
        // writesubplan (gen mg-0146 / mg-0159, merge#215): or the TARGET hashed, the source probing (PostgreSQL's Hash
        // Right / Full Join with the target inner): the probe's build pipeline scans the target's own heap, runs only
        // filters, gates and projections before its Build op and ends in that hash; its rows carry the target's junk ctid
        // into the probe's joined (MATCHED) rows and the drained (NOT MATCHED BY SOURCE) rows alike, a FULL join's unmatched
        // source rows a NULL ctid (NOT MATCHED). The build pipeline is then the target's producer: a recheck reruns it at
        // the host's newest version (isolation overlay writesubplan-merge-hashed)
        let Source::Breaker { slot } = self.pipelines()[d].source else { return None };
        let b = self.life_of(d, slot)?.0 as usize;
        let pb = &self.pipelines()[b];
        let own_b = match &pb.source {
            Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } => {
                u32::from(*scanrelid) == w.rti && *relid == w.plan.target_rel
            }
            _ => false,
        };
        let built = pb.ops.split_last().is_some_and(|(l, before)| matches!(l, Op::Join(JoinDesc::Build { .. })) && before.iter().all(quiet));
        // D3669: the probe may enter a COPY-marked function or subquery row first (`copy_entry`)
        let entry = copy_entry(self, k);
        let entered = before.iter().enumerate().all(|(i, o)| i < entry || quiet(o));
        (own_b && built && probe && entered && self.end_kind(b) == EndKind::Breaker(BreakerKind::HashBuild)).then_some(b)
    }

    /// The build pipeline of drain `d` when `d` writes an UPDATE or DELETE whose rows are the drained build rows of the
    /// target's own scan: one heap target with no ON CONFLICT, locking mark or row lock; the drain a RIGHT ANTI
    /// one (PostgreSQL's Hash Right Anti Join under ExecUpdate / ExecDelete: `WHERE NOT EXISTS (..)` with the target
    /// hashed, the subquery's relation probing, so every row reaching the write is a drained target row); its build scans
    /// the target's own heap and runs only filters, gates and projections; its probe pipeline runs only those before the
    /// probe, its last op; and the drain pipeline runs only those between the Drain op and the write. The spooled form
    /// writes the drained rows, each carrying the target's junk ctid. The write's admission names the build the target's
    /// producer (D776's form: a recheck reruns the build at the host's newest version, the probe's relation at its
    /// REFERENCE row, which an anti join's NULL ctid makes empty, then the drain pipeline, which hands the newest version
    /// on when it still passes the build's filters). drain (gen mt-0076 group): or a RIGHT SEMI drain (PostgreSQL's Hash
    /// Right Semi Join under ExecUpdate / ExecDelete: `WHERE .. IN (..)` with the target hashed), whose probe hands each
    /// build row on at its first match with the probe row (`semi_hands`): the spool holds those rows in PostgreSQL's order
    /// and the drain hands nothing more; the recheck reruns the build, then the probe from its relation's REFERENCE row or
    /// a COPY-marked subquery's stored row (rerun/epq/marks.rs `copy_pos`), then the drain pipeline. D3664: the one
    /// target may be routed (a partitioned root pruned to one leaf, an inheritance tree's one result relation): the write
    /// goes through the root's plan as any one-target routed write does (modifytable/update.rs `row_op`, delete.rs: the
    /// root's statement triggers; a row that leaves its leaf is moved through the root, `moving`), its position the junk
    /// ctid. The build has read every target row before the probe hands the first one on, so no moved row is read again.
    /// D3668: a check option's SubPlan step may sit between the Drain op and the write (`check_sub`). D3825: the build may
    /// filter by a hashed SubPlan step at the row (a NOT IN that stayed a SubPlan), whose set the recheck probes as built.
    pub(crate) fn drained_build_target(&self, d: usize) -> Option<usize> {
        use gravity_decompose::{Am, Source};
        let pd = &self.pipelines()[d];
        let ops = &pd.ops;
        let wj = ops.iter().position(|o| matches!(o, Op::Write(_)))?;
        let Op::Write(w) = &ops[wj] else { return None };
        // drain (gen mt-0076 group): or a RIGHT SEMI one whose probe hands each build row on at its first match (`semi_hands`)
        // D3373 (m8_drain_spool_write): or a RIGHT one, each target row reaching the write at most once (`right_once`)
        let (jd, m, jt) = ops.iter().enumerate().find_map(|(j, o)| match o {
            Op::Join(JoinDesc::Drain { matches, jointype: jt @ (JoinType::JOIN_RIGHT_ANTI | JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT) }) => Some((j, *matches, *jt)),
            _ => None,
        })?;
        let plain = matches!(w.cmd, Cmd::Update | Cmd::Delete) && w.merge.is_none() && w.targets.len() == 1 && w.targets[0].kind == TargetKind::Heap && w.on_conflict.is_none();
        let quiet = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
        if !plain || jd > wj || !ops[..jd].iter().all(quiet) || !ops[jd + 1..wj].iter().all(|o| quiet(o) || crate::graph::check_sub(w, ops, o)) || ops.iter().any(|o| matches!(o, Op::Lock(_))) || w.mark_descs.iter().any(|m| m.lock != LockKind::None) {
            return None;
        }
        let Source::Breaker { slot } = pd.source else { return None };
        let b = self.life_of(d, slot)?.0 as usize;
        let pb = &self.pipelines()[b];
        let own = match &pb.source {
            Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } => {
                u32::from(*scanrelid) == w.rti && *relid == w.plan.target_rel
            }
            _ => false,
        };
        // its ops: quiet ones, then the Build op its Hash adds last (decompose join.rs `map_hash_join`); D3373: under a RIGHT
        // drain also an inner-unique INNER probe (each target row meets at most one row of a REFERENCE-marked relation's
        // build, so it is at most one build record: `right_once`'s first half)
        let one = |o: &Op<'_>| jt == JoinType::JOIN_RIGHT && matches!(o, Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_INNER, unique: true, catalog: false, .. }));
        // D3825 (Hibernate's DeleteWithSubqueryTest: `DELETE FROM panel WHERE clientid IN (SELECT clientid FROM trtpanel) AND
        // deltastamp NOT IN (SELECT deltastamp FROM trtpanel)`, PostgreSQL 18.6's Hash Right Semi Join whose build, the
        // target's scan, filters by the hashed NOT IN): or a hashed SubPlan step at the row in the build (`hashed_row`): the
        // runner's recheck reruns the build over the newest version, the step as the run takes it, and probes the
        // statement's set (rerun/epq/marks.rs `kept_sets`, D3778; a set it cannot keep refuses there), as EvalPlanQual
        // builds it again from the same rows
        let hashed_row = |o: &Op<'_>| matches!(o, Op::SubPlan(s) if matches!(s.form, gravity_decompose::ops::shared::subplan::SubForm::Hashed { .. }) && s.site == gravity_decompose::ops::shared::subplan::SubSite::Row);
        let built = pb.ops.split_last().is_some_and(|(l, before)| matches!(l, Op::Join(JoinDesc::Build { .. })) && before.iter().all(|o| quiet(o) || one(o) || hashed_row(o)));
        let build = self.end_kind(b) == EndKind::Breaker(BreakerKind::HashBuild) && built;
        let k = self.life_of(d, m)?.0 as usize;
        let (last, before) = self.pipelines()[k].ops.split_last()?;
        // drain (gen mt-0076 group): a RIGHT SEMI probe below a COPY-marked subquery's entry (rerun/epq/copy_row.rs
        // `boundary`): the ops before the entry are the subquery's own, which a recheck never runs (it enters there with the
        // stored row: marks.rs `copy_pos`, the probe pipeline a producer whose product the drain reads)
        let entry = match jt {
            JoinType::JOIN_RIGHT_SEMI => self.graph.record_rows.iter().filter(|r| crate::rerun::epq::marks::copied(self, r.scanrelid)).filter_map(|r| crate::rerun::epq::copy_row::boundary(self, k, r)).max().unwrap_or(0),
            _ => 0,
        };
        // D3737 (Django's update() / delete() filtered across a relation: `pk IN (SELECT U0.id FROM t U0 LEFT JOIN a U1
        // .. JOIN b U2 ..)`, PostgreSQL 18.6's Hash Right Semi Join whose probe is a Hash Join of the subquery's
        // relations): or, before a RIGHT SEMI probe, INNER and LEFT hash probes, each build a REFERENCE-marked
        // relation's the recheck reruns over its refetched row, or empty when the row came null-extended (marks.rs
        // `sites_in`), as EvalPlanQual reruns the probe side's join over EvalPlanQualFetchRowMark's rows
        let joined = |o: &Op<'_>| jt == JoinType::JOIN_RIGHT_SEMI && matches!(o, Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_INNER | JoinType::JOIN_LEFT, catalog: false, marks: false, .. }));
        // D3738 (Django's update() over an m2m-annotated queryset: `pk IN (SELECT U0.id FROM t U0 LEFT OUTER JOIN m U1 ON
        // ..)`, PostgreSQL 18.6's Hash Right Semi Join whose probe is a Hash Right Join, U0 hashed, U1 probing): or, under a
        // RIGHT SEMI drain, the probe pipeline a RIGHT drain reads of REFERENCE-marked relations' scans (`fed_right`), its
        // Drain op first: the right join's joined rows, then its drained records, probe the target's hash
        let fed = |i: usize, o: &Op<'_>| jt == JoinType::JOIN_RIGHT_SEMI && i == 0 && matches!(o, Op::Join(JoinDesc::Drain { jointype: JoinType::JOIN_RIGHT, .. })) && fed_right(self, k, w);
        let probe = matches!(last, Op::Join(JoinDesc::Probe { jointype, catalog: false, .. }) if *jointype == jt) && before.iter().enumerate().all(|(i, o)| i < entry || quiet(o) || joined(o) || fed(i, o));
        // D3373: a RIGHT drain hands a build row on with every probe row it meets, then each one it met none of
        let once = jt != JoinType::JOIN_RIGHT || right_once(self, k);
        (own && build && probe && once).then_some(b)
    }

    /// lockrows lk-0100 / drain (gen mt-0076 group): drain `d`'s RIGHT SEMI probe hands each build row on at its first
    /// match into the spool, with the probe row it matched (probe.rs, walk.rs): under a row lock straight over the drain
    /// (spool_admit.rs `free_lock`), or under an UPDATE / DELETE of the drained build rows (`drained_build_target`), whose
    /// rows carry the probe side's junk (a COPY-marked subquery's whole row) as PostgreSQL's Hash Right Semi Join hands it.
    /// D3880: or whenever the ops after the Drain read a probe column (an `OUTER_VAR` spool column: outside a plain SELECT,
    /// whose hash-key probe columns decompose join.rs reads as their build keys (D3510), any probe column, a write's or a
    /// lock's junk among them; in a plain SELECT a key column whose equality is not one image, D3510's residual): the marks
    /// form would hand the drained build row on with that column NULL, where PostgreSQL's row carries its first match's.
    /// Decompose keeps an UPDATE's or a MERGE's projection whole (modifytable/carry.rs carries the target's columns
    /// through the plan's own targetlist), so the probe row comes from here.
    pub(crate) fn semi_hands(&self, d: usize) -> bool {
        let Some(jd) = self.pipelines().get(d).and_then(|p| p.ops.iter().position(|o| matches!(o, Op::Join(JoinDesc::Drain { jointype: JoinType::JOIN_RIGHT_SEMI, .. })))) else { return false };
        let probe_col = || self.spool_columns(d, jd).is_some_and(|c| c.iter().any(|&((inner, _), _)| !inner));
        self.free_lock(d, d).is_some() || self.drained_build_target(d).is_some() || self.hashed_union_build(d).is_some() || self.semi_exact(d) || probe_col()
    }

    /// D1922 (D3377): drain `d`'s spooled RIGHT SEMI probe is one N1 does not find quiet, pulled a joined row at a time
    /// under a partial reader (spool_admit.rs `drain_exact`): it hands each build row on at its first match into the
    /// spool, as PostgreSQL 18.6's Hash Right Semi Join returns it, where a quiet probe's marks wait for its end.
    fn semi_exact(&self, d: usize) -> bool {
        let drain = |(j, o): (usize, &Op<'_>)| match o {
            Op::Join(JoinDesc::Drain { matches, jointype: JoinType::JOIN_RIGHT_SEMI }) => Some((j, *matches)),
            _ => None,
        };
        let Some((jd, m)) = self.pipelines().get(d).and_then(|p| p.ops.iter().enumerate().find_map(drain)) else { return false };
        let Some(k) = self.life_of(d, m).map(|l| l.0 as usize) else { return false };
        !self.spool_quiet(k) && self.spooled_drain(k) == Some((d, jd)) && self.drain_exact(k, d, jd)
    }

    /// writesubplan (merge#368 / #402, gen mg-0181): the arms of drain `d`'s probe by target index (`targets` order) when
    /// `d` writes a MERGE of several heap result relations (no ON CONFLICT, locking mark or row lock; a junk tableoid) whose
    /// rows are a RIGHT or FULL drain's: PostgreSQL's Hash Right / Full Join with the source hashed and the Append of the
    /// result relations' scans probing it. Decompose streams the union into the probe (`drained_arms`: one pipeline per
    /// arm, each ending in the join's spool), so each arm scans its own result relation's heap and runs only filters,
    /// gates, projections and its bare arm marker before the probe, its last op; the drain pipeline runs only those
    /// between the Drain op and the write. The spooled form writes them: the arms' joined rows spooled in arm order (each
    /// row carrying its relation's junk tableoid and ctid), then the drained source rows (NOT MATCHED). merge/route.rs
    /// takes the arms as the union form's feeds (`union_targets`: a row's relation its junk tableoid, its position its junk
    /// ctid; a recheck reruns the arms, the row's own relation at the host's newest version, then the drain pipeline).
    pub(crate) fn merge_drained_arms(&self, d: usize) -> Option<PgVec<'g, usize>> {
        use gravity_decompose::{Am, Source};
        let ops = &self.pipelines().get(d)?.ops;
        let wj = ops.iter().position(|o| matches!(o, Op::Write(_)))?;
        let Op::Write(w) = &ops[wj] else { return None };
        let (jd, m) = ops.iter().enumerate().find_map(|(j, o)| match o {
            Op::Join(JoinDesc::Drain { matches, jointype: JoinType::JOIN_RIGHT | JoinType::JOIN_FULL }) => Some((j, *matches)),
            _ => None,
        })?;
        let plain = w.cmd == Cmd::Merge && w.merge.is_some() && w.targets.len() > 1 && w.targets.iter().all(|t| t.kind == TargetKind::Heap) && w.on_conflict.is_none() && w.tableoid_col > 0;
        let quiet = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
        if !plain || jd > wj || !ops[jd + 1..wj].iter().all(quiet) || ops.iter().any(|o| matches!(o, Op::Lock(_))) || w.mark_descs.iter().any(|m| m.lock != LockKind::None) {
            return None;
        }
        let last = self.life_of(d, m)?.0 as usize;
        let (first, l) = self.drained_arms(last)?;
        if l != last || last + 1 - first != w.targets.len() {
            return None;
        }
        let mut by = mcx::pg_vec![in self.mcx; usize::MAX; w.targets.len()];
        for q in first..=last {
            let pq = &self.pipelines()[q];
            let (rti, relid) = match &pq.source {
                Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } => (u32::from(*scanrelid), *relid),
                _ => return None,
            };
            let (op, before) = pq.ops.split_last()?;
            let probe = matches!(op, Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_RIGHT | JoinType::JOIN_FULL, catalog: false, .. }));
            let marker = |o: &Op<'_>| matches!(o, Op::Append(AppendDesc::Arm { prune: None, .. }));
            if !probe || !before.iter().all(|o| quiet(o) || marker(o)) || before.iter().filter(|o| marker(o)).count() != 1 {
                return None;
            }
            let i = w.targets.iter().position(|t| t.rti == rti && t.relid == relid)?;
            if by[i] != usize::MAX {
                return None;
            }
            by[i] = q;
        }
        Some(by)
    }

    /// D3483: the build pipeline of drain `d` when `d` writes a MERGE of several heap result relations whose union the
    /// build reads (PostgreSQL's Hash Right / Full Join over the Append of their scans, the source probing), its feeds
    /// rerun/union_arms.rs `target_feeds`'. D3673: or an UPDATE / DELETE of them under `drained_build_target`'s drains.
    /// The recheck reruns the feeds, the build, the probe, then `d`.
    pub(crate) fn hashed_union_build(&self, d: usize) -> Option<usize> {
        use gravity_decompose::Source;
        let ops = &self.pipelines().get(d)?.ops;
        let wj = ops.iter().position(|o| matches!(o, Op::Write(_)))?;
        let Op::Write(w) = &ops[wj] else { return None };
        let merge = w.cmd == Cmd::Merge && w.merge.is_some();
        let dml = matches!(w.cmd, Cmd::Update | Cmd::Delete) && w.merge.is_none();
        let (jd, m, jt) = ops.iter().enumerate().find_map(|(j, o)| match o {
            Op::Join(JoinDesc::Drain { matches, jointype: jt @ (JoinType::JOIN_RIGHT | JoinType::JOIN_FULL) }) if merge => Some((j, *matches, *jt)),
            Op::Join(JoinDesc::Drain { matches, jointype: jt @ (JoinType::JOIN_RIGHT_ANTI | JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT) }) if dml => Some((j, *matches, *jt)),
            _ => None,
        })?;
        let plain = w.targets.len() > 1 && w.targets.iter().all(|t| t.kind == TargetKind::Heap) && w.on_conflict.is_none() && w.tableoid_col > 0;
        let quiet = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
        // D3665: an UPDATE's / DELETE's check option SubPlan step as `drained_build_target`'s
        if !plain || jd > wj || !ops[..jd].iter().all(quiet) || !ops[jd + 1..wj].iter().all(|o| quiet(o) || dml && crate::graph::check_sub(w, ops, o)) || ops.iter().any(|o| matches!(o, Op::Lock(_))) || w.mark_descs.iter().any(|m| m.lock != LockKind::None) {
            return None;
        }
        let Source::Breaker { slot } = self.pipelines()[d].source else { return None };
        let b = self.life_of(d, slot)?.0 as usize;
        let pb = &self.pipelines()[b];
        let (last, body) = pb.ops.split_last()?;
        let reads = crate::rerun::union_arms::target_feeds(self, b, w).is_some();
        let built = matches!(last, Op::Join(JoinDesc::Build { .. })) && body.iter().skip(1).all(|o| matches!(o, Op::Filter(_) | Op::Project(_)));
        let k = self.life_of(d, m)?.0 as usize;
        let (probe, before) = self.pipelines()[k].ops.split_last()?;
        let entry = copy_entry(self, k);
        let joins = match probe {
            Op::Join(JoinDesc::Probe { jointype, catalog: false, .. }) => merge && matches!(jointype, JoinType::JOIN_RIGHT | JoinType::JOIN_FULL) || dml && *jointype == jt,
            _ => false,
        };
        let probes = joins && before.iter().enumerate().all(|(i, o)| i < entry || quiet(o));
        let once = merge || jt != JoinType::JOIN_RIGHT || right_once(self, k);
        (reads && built && probes && once && self.end_kind(b) == EndKind::Breaker(BreakerKind::HashBuild)).then_some(b)
    }

    /// drain (regress updatable_views#865, D2132 narrowed): the arms `first..=last` of a streamed probe union (`drained_arms`)
    /// fill drain `d`'s one spool in arm order (spool.rs, writesubplan's several-arms fill: the first arm declares it, each
    /// later arm appends) when `d` ends in no write (a write's arms are `merge_drained_arms`') and its tail holds no Append
    /// op, and each arm runs only filters, gates, projections and its one bare arm marker (no pruning step) before its
    /// probe, its last op. PostgreSQL's Append under the hash join runs the arms one after the other, each probe's joined
    /// rows handed on as it goes, then the drained build rows once the last arm ends; the spool holds the joined rows in
    /// that arm order and the drain reads them, then its drained records. The probes run whole before the drain's first
    /// row, which spool_admit.rs judges as for one probe (N1 over every arm, TAILSTOP, the whole-read mark).
    pub(crate) fn plain_drained_arms(&self, (first, last): (usize, usize), d: usize) -> bool {
        let ps = self.pipelines();
        let Some(jd) = ps.get(d).and_then(|p| p.ops.iter().position(|o| matches!(o, Op::Join(JoinDesc::Drain { .. })))) else { return false };
        let quiet = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
        let marker = |o: &Op<'_>| matches!(o, Op::Append(AppendDesc::Arm { prune: None, .. }));
        let arm = |q: usize| {
            ps[q].ops.split_last().is_some_and(|(op, before)| {
                matches!(op, Op::Join(JoinDesc::Probe { catalog: false, .. })) && before.iter().all(|o| quiet(o) || marker(o)) && before.iter().filter(|o| marker(o)).count() == 1
            })
        };
        !matches!(self.end_kind(d), EndKind::RowOp(_)) && !ps[d].ops[jd + 1..].iter().any(|o| matches!(o, Op::Append(_))) && first <= last && last < d && (first..=last).all(arm)
    }

    /// INNER-SPOOL: probe `k`'s RIGHT / FULL drain `(d, jd)` when the drain is a nest's embedded inner (`inner_drain_of`) and
    /// `end` spools it: spool_admit.rs `inner_spool_refusal` names no reason. PG 18.6 re-runs the inner Hash Right / Full
    /// Join at every rescan of the nest (ExecReScanHashJoin: the one-batch table kept, its match flags reset, the outer
    /// scanned again) and hands its joined rows on while it probes, then the unmatched build rows; the inner reads the spool
    /// of the probe's joined rows from its first record at every re-open, then the drained records (spool.rs). D3915: the
    /// probe may be a streamed union's plain arms (`drained_arms`, `plain_drained_arms`): they fill the one spool in arm
    /// order, as PG's Append under the join runs them at every rescan; `k` is any arm, the drain read from the last's life.
    /// D4063: a RIGHT ANTI drain too, and a RIGHT SEMI one whose build shows nothing (spool_admit.rs `semi_build_unseen`),
    /// each a lone probe that no rerun set re-runs (`marks_inner`): its probe marks the build and hands no joined row on,
    /// so the spool stays empty and every re-open reads the drained records alone. PG's Hash Right Anti Join is an
    /// HJ_FILL_INNER join, as RIGHT / FULL are: it builds at its first run whatever the probe holds, and its rescan keeps
    /// the one-batch table, resets the match flags and scans the outer again, so each re-open drains the same build rows.
    pub(crate) fn inner_spooled(&self, k: usize) -> Option<(usize, usize)> {
        let m = self.pipelines().get(k)?.desc.produces()?;
        if self.end_maker(k) != KindId::JoinDrain {
            return None;
        }
        let (d, jd) = inner_drain_of(self, self.drained_arms(k).map_or(k, |r| r.1), m)?;
        let rf = match self.pipelines()[d].ops[jd] {
            Op::Join(JoinDesc::Drain { jointype: JoinType::JOIN_RIGHT | JoinType::JOIN_FULL, .. }) => true,
            Op::Join(JoinDesc::Drain { jointype: JoinType::JOIN_RIGHT_ANTI, .. }) => self.marks_inner(k, d),
            Op::Join(JoinDesc::Drain { jointype: JoinType::JOIN_RIGHT_SEMI, .. }) => self.marks_inner(k, d) && self.semi_build_unseen(k),
            _ => false,
        };
        (rf && self.inner_spool_refusal(k, d, jd).is_none()).then_some((d, jd))
    }

    /// D4063: probe `k`'s RIGHT SEMI / RIGHT ANTI drain `d` (a nest's embedded inner) takes the spooled inner form only as a
    /// lone probe (no streamed arms) that no rerun set holds, nor the inner's re-open: a set's re-run keeps the marking form
    /// (`end`'s NoRows, the drain walking the marks the set's probe made).
    fn marks_inner(&self, k: usize, d: usize) -> bool {
        self.drained_arms(k).is_none() && crate::rerun::sets::owner_set_at(self, d).is_none() && !self.graph.reruns.iter().any(|r| r.pipelines.contains(&(k as u16)))
    }

    /// INNER-SPOOL: whether nest inner `d` is a spooled drain's reader (nest/inner.rs takes its emit with no rerun set: the
    /// spool and the drained records are re-read at every re-open).
    pub(crate) fn spooled_inner(&self, d: usize) -> bool {
        d >= self.pipelines().len() && spooled_probe(self, d).is_some()
    }

    /// Row A2a (W2-DRAIN-A2-NOPARTIAL design r3 (1), review r1 F2): the op index of drain `d`'s write when it is an INSTEAD
    /// OF view UPDATE / DELETE: one view target with its INSTEAD OF row trigger for the command, no MERGE, ON CONFLICT, route,
    /// locking mark or row lock. PostgreSQL hands each row to ExecIR{Update,Delete}Triggers: no position, no recheck, no
    /// EvalPlanQual, so the spooled form takes it (join/drain_spool.rs: N5 skips it, op_acts, order_free). D2247: a plain
    /// INSERT into one heap table is the same (a new row has no position, an INSERT never rechecks; the calls it makes per
    /// row run once each whatever the order, DECISIONS 6b), as is a MERGE into one view whose INSTEAD OF row triggers take
    /// each of its actions. D3752: so is such an INSERT's ON CONFLICT when the order its rows come in picks no stored row
    /// (rerun/spool/upsert.rs `order_free`). Any other write keeps its position through the join and has no partial form.
    pub(crate) fn a2a_write(&self, d: usize) -> Option<usize> {
        // A4-MATEND: a MERGE whose MATCHED rows read their position from the probe's own target scan
        if self.merge_drained_target(d).is_some() || self.drained_build_target(d).is_some() || self.merge_drained_arms(d).is_some() || self.hashed_union_build(d).is_some() {
            return self.pipelines()[d].ops.iter().position(|o| matches!(o, Op::Write(_)));
        }
        let ops = &self.pipelines()[d].ops;
        let (j, w) = ops.iter().enumerate().find_map(|(j, o)| if let Op::Write(w) = o { Some((j, w)) } else { None })?;
        let t = w.targets.first()?;
        // D3752: a heap INSERT's ON CONFLICT whose stored rows are PostgreSQL's in any order
        let conflict = w.on_conflict.is_none() || w.cmd == Cmd::Insert && t.kind == TargetKind::Heap && crate::rerun::spool::upsert::order_free(self, ops, j, w).is_ok();
        let free = conflict && w.route.is_none() && w.mark_descs.iter().all(|m| m.lock == LockKind::None);
        let lone = w.targets.len() == 1 && free && !ops.iter().any(|o| matches!(o, Op::Lock(_)));
        if w.cmd == Cmd::Merge {
            // ExecMerge's view arm (merge/view.rs): each action's INSTEAD OF row trigger is the write
            let fires = |c: Option<Cmd>| match c {
                Some(Cmd::Update) => t.instead.update,
                Some(Cmd::Delete) => t.instead.delete,
                Some(Cmd::Insert) => t.instead.insert,
                None => true,
                _ => false,
            };
            let view = t.kind == TargetKind::View && t.instead.known && w.merge.as_ref().is_some_and(|m| m.actions.iter().all(|a| fires(a.cmd)));
            return (view && lone || self.a2a_merge(ops, w)).then_some(j); // D2247 (merge/lower.rs)
        }
        let instead = match w.cmd {
            Cmd::Update => t.kind == TargetKind::View && t.instead.update,
            Cmd::Delete => t.kind == TargetKind::View && t.instead.delete,
            Cmd::Insert => t.kind == TargetKind::Heap,
            _ => false,
        };
        (instead && lone && w.merge.is_none()).then_some(j)
    }
}

/// D3669: the op where pipeline `k` enters a COPY-marked function or subquery row (rerun/epq/copy_row.rs `boundary`), 0
/// for none. The ops before it are the row's own, which a recheck never runs: it enters there with the stored row.
fn copy_entry(gx: &Gx<'_, '_>, k: usize) -> usize {
    gx.graph.record_rows.iter().filter(|r| crate::rerun::epq::marks::copied(gx, r.scanrelid)).filter_map(|r| crate::rerun::epq::copy_row::boundary(gx, k, r)).max().unwrap_or(0)
}

/// D3738 (`drained_build_target`): pipeline `k` reads a RIGHT drain, its Drain op first, whose build and probe each scan
/// a REFERENCE-marked relation's heap (of write `w`, not its target) under filters, gates and projections, the build
/// ending in its Build op, the probe in its RIGHT probe: a recheck reruns both over their relations' refetched rows (the
/// probe's none when the row came null-extended: marks.rs `sites_in`) into a fresh spool, then `k`.
fn fed_right(gx: &Gx<'_, '_>, k: usize, w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>) -> bool {
    use gravity_decompose::{Am, Source};
    use types_nodes::plannodes::RowMarkType;
    let pk = &gx.pipelines()[k];
    let Some(Op::Join(JoinDesc::Drain { matches, jointype: JoinType::JOIN_RIGHT })) = pk.ops.first() else { return false };
    let Source::Breaker { slot } = pk.source else { return false };
    let quiet = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    let marked = |s: &Source<'_>| match s {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } => {
            let rti = u32::from(*scanrelid);
            rti != w.rti && w.mark_descs.iter().any(|mk| mk.rti == rti && mk.relid == *relid && mk.mark == RowMarkType::ROW_MARK_REFERENCE && !mk.parent && mk.ctid_col > 0)
        }
        _ => false,
    };
    let (Some(b), Some(q)) = (gx.life_of(k, slot).map(|l| l.0 as usize), gx.life_of(k, *matches).map(|l| l.0 as usize)) else { return false };
    let (pb, pq) = (&gx.pipelines()[b], &gx.pipelines()[q]);
    let built = pb.ops.split_last().is_some_and(|(l, before)| matches!(l, Op::Join(JoinDesc::Build { .. })) && before.iter().all(quiet)) && gx.end_kind(b) == EndKind::Breaker(BreakerKind::HashBuild);
    let probed = pq.ops.split_last().is_some_and(|(l, before)| matches!(l, Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_RIGHT, catalog: false, .. })) && before.iter().all(quiet));
    built && probed && marked(&pb.source) && marked(&pq.source)
}

/// D3738 (spool_admit.rs `free_write`'s L5 SEMI-PROBE-DRAIN): the probe pipeline of write pipeline `q`'s RIGHT SEMI
/// drain when `q` writes that drain's build rows (`drained_build_target`).
pub(crate) fn semi_probe(gx: &Gx<'_, '_>, q: usize) -> Option<usize> {
    gx.drained_build_target(q)?;
    let m = gx.pipelines()[q].ops.iter().find_map(|o| if let Op::Join(JoinDesc::Drain { matches, jointype: JoinType::JOIN_RIGHT_SEMI }) = o { Some(*matches) } else { None })?;
    gx.life_of(q, m).map(|l| l.0 as usize)
}

/// D3373 (m8_drain_spool_write): `drained_build_target`'s RIGHT drain, an UPDATE / DELETE (a table
/// UPDATE .. FROM, a rule's action) whose target sits in the build: PostgreSQL's Hash Right Join hands each build record on
/// with every probe row it meets, then each record it met none of null-extended, and ModifyTable writes a target row from
/// the first row reaching it (a later one finds TM_SelfModified and is skipped). The spool hands the joined rows on after
/// the probe ran whole (across workers at W8) and the unmatched records in record order where PostgreSQL goes bucket by
/// bucket, so a target row reaching the write twice could take another row's cells. Served only where each target row
/// reaches the write at most once: each is at most one build record (the build's inner probes are inner-unique, checked
/// there), and probe pipeline `k` meets each record at most once: its source a heap scan whose hash keys are plain
/// columns a unique, valid, immediate, non-partial btree index covers (ops/shared/unique.rs `unique_on`). Then only the
/// order of the writes differs, which their triggers and RETURNING see (DECISIONS 6b).
fn right_once(gx: &Gx<'_, '_>, k: usize) -> bool {
    use gravity_decompose::{Am, Source};
    use types_nodes::primnodes::OUTER_VAR;
    let p = &gx.pipelines()[k];
    let (rti, relid) = match &p.source {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } => (i32::from(*scanrelid), *relid),
        _ => return false,
    };
    let Some((pos, Op::Join(JoinDesc::Probe { keys, .. }))) = p.ops.iter().enumerate().next_back() else { return false };
    let mut cols: mcx::PgVec<'_, i16> = mcx::PgVec::new_in(gx.mcx);
    for e in keys.iter() {
        let Some(v) = crate::table::emit_group::bare(e).as_var().filter(|v| v.varlevelsup == 0) else { return false };
        // a scan column, or an outer one a projection of the pipeline made from one (`source_var`)
        let v = if v.varno == OUTER_VAR { crate::table::emit_group::source_var(gx, k, pos, v.varattno) } else { Some(v) };
        match v {
            Some(v) if v.varno == rti && v.varattno > 0 => cols.push_in(v.varattno),
            _ => return false,
        }
    }
    !cols.is_empty() && crate::ops::shared::unique::unique_on(gx, relid, &cols[..])
}

/// D3752: why drain `d`'s write, a heap INSERT .. ON CONFLICT that would be row A2a's but for it, is no A2a write
/// (rerun/spool/upsert.rs `order_free`'s names); none for any other write.
fn conflict_refusal(gx: &Gx<'_, '_>, d: usize) -> Option<&'static str> {
    let ops = &gx.pipelines()[d].ops;
    let (j, w) = ops.iter().enumerate().find_map(|(j, o)| if let Op::Write(w) = o { Some((j, w)) } else { None })?;
    let plain = w.cmd == Cmd::Insert && w.merge.is_none() && w.targets.len() == 1 && w.targets[0].kind == TargetKind::Heap && w.route.is_none();
    let lone = plain && w.mark_descs.iter().all(|m| m.lock == LockKind::None) && !ops.iter().any(|o| matches!(o, Op::Lock(_)));
    if !lone || w.on_conflict.is_none() {
        return None;
    }
    crate::rerun::spool::upsert::order_free(gx, ops, j, w).err()
}

/// Row A2a's own refusal, asked before join/drain_spool.rs's (the name is its A2END's: a test pins it).
const A2END: &str = "a spooled drain under an end the spooled form does not take (D2136)";

/// Why drain `d`'s A2a write is refused on its own path, or `None` (also when it is no A2a write): design r3 (2), the write
/// reads only the projection before it (nothing carried from a target scan, every row mark's junk column one of its items).
/// writesubplan (regress with#242 / #244 / #245 / #247): review r2 F4's CTE name (D2178, fail closed) is lifted as the
/// spooled form's (join/drain_spool.rs `no_widen`): the CTE's readers pull its one store (ctescan/lower.rs, ROOTFIX-CTE-PARK),
/// an InitPlan's value is the statement's, and a recheck reruns the producers below the write, not the CTE.
fn a2a_refusal(gx: &Gx<'_, '_>, d: usize) -> Option<&'static str> {
    let ops = &gx.pipelines()[d].ops;
    let j = gx.a2a_write(d)?;
    let Op::Write(w) = &ops[j] else { return Some(A2END) };
    let proj = ops[..j].iter().rev().find_map(|o| if let Op::Project(l) = o { Some(*l) } else { None });
    let item = |c: i16| proj.is_some_and(|l| l.iter().any(|n| n.as_target_entry().is_some_and(|t| t.resno == c)));
    // an UPDATE of the drained build rows (`drained_build_target`) carries its old row from the target's own scan, the
    // build whose records the drain hands on (carry.rs reads them from the projection as over a plain hash join)
    let carried = !w.old_cols.is_empty() && gx.drained_build_target(d).is_none() && gx.hashed_union_build(d).is_none();
    if carried || w.mark_descs.iter().any(|m| m.ctid_col != 0 && !item(m.ctid_col)) {
        return Some(A2END);
    }
    None
}

/// The Drain op: the union its pipeline's ops after it share with the probe's tail, then join.inner's identity.
fn step<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let node = op_node(px.gx, k, j);
    px.set_union(k, node as u32, j);
    (kind_impl::<StepMaker>(&node, KindId::JoinInner).0)(px, k, j)
}

/// The drain pipeline that reads probe pipeline `k`'s spool, and its Drain op.
pub(crate) fn drain_of(gx: &Gx<'_, '_>, k: usize, slot: u8) -> Option<(usize, usize)> {
    gx.pipelines().iter().enumerate().skip(k + 1).find_map(|(q, p)| {
        let j = p.ops.iter().position(|op| matches!(op, Op::Join(JoinDesc::Drain { matches, .. }) if *matches == slot))?;
        (gx.life_of(q, slot) == Some(LifeId(k as u16))).then_some((q, j))
    })
}

/// D3024 (b) rest (road `emit.marks`, table/emit_group.rs): probe pipeline `k` ends in the `m` spool that `end` makes the
/// seeded form of. That form is the Fused end over the drain's own end: a fold, full sort, hash build or owned keyed end
/// whose arms continue one slot (`continues`, but not a top). The drain's tail is all tail ops, with no LIMIT. The probe is
/// none of these: an arm of a streamed union, spooled, a nest's inner, or a CTE's, InitPlan's or hashed SubPlan's fill.
/// Each claim thread's probe then sets the build's marks with the same plain stores a scan-driven probe section makes,
/// and hands its rows to that end per thread: each worker's partial merges, or its run is put.
pub(crate) fn seeds_drain(gx: &Gx<'_, '_>, k: usize) -> bool {
    if k >= gx.pipelines().len() || gx.end_maker(k) != KindId::JoinDrain || gx.drained_arms(k).is_some() || gx.spooled_drain(k).is_some() || gx.inner_spooled(k).is_some() {
        return false;
    }
    let Some((d, jd)) = breaker_of(&gx.pipelines()[k].desc).and_then(|m| drain_of(gx, k, m.slot)) else { return false };
    let (gd, tail) = (gx.pipelines()[d].desc, &gx.pipelines()[d].ops[jd + 1..]);
    let seeded = match (gx.end_kind(d), breaker_of(&gd)) {
        (EndKind::Breaker(b), Some(e)) => !matches!(b, BreakerKind::Materialize | BreakerKind::TopK) && continues(&gd) && between(gx, k, d, e.slot) && Parallel::Arms.on(),
        _ => false,
    };
    seeded && !holds_limit(tail) && tail.iter().all(tail_op) && !cte_store(gx, d) && !init_cell(gx, d) && !set_fill(gx, d)
}

/// join#212 / join#213: the nest's embedded inner that drains probe pipeline `k`'s spool `slot` (pipes.rs indexes the
/// inners past the graph pipelines), and its Drain op.
pub(crate) fn inner_drain_of(gx: &Gx<'_, '_>, k: usize, slot: u8) -> Option<(usize, usize)> {
    let ps = gx.pipelines();
    (ps.len()..ps.len() + ps.inners().len()).find_map(|q| {
        let j = ps[q].ops.iter().position(|op| matches!(op, Op::Join(JoinDesc::Drain { matches, .. }) if *matches == slot))?;
        (gx.life_of(q, slot) == Some(LifeId(k as u16))).then_some((q, j))
    })
}

/// join#212 / join#213: `m`'s end when its drain is a nest's embedded inner and the join is RIGHT SEMI / RIGHT ANTI: the
/// probe marks the build's records and hands no row on (the inner's drain source walks the marked / unmarked records at
/// every re-open), so nothing reaches this end; its pipeline runs on the leader, as the set's members do.
struct NoRows;

impl EndMech for NoRows {
    fn pipeline_desc(&self, _px: &Px<'_, '_>, _k: usize, d: Pipeline) -> Pipeline {
        Pipeline { parallel: gravity_program::Parallel::Ineligible { site: d.driving_site, reason: Why::Sink }, ..d }
    }

    fn lower<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        row.carry.copy_in(env.mcx)
    }
}

/// The in-row refusals (D393's drain arms, join/drain_arms.rs): a probe arm that may show PG whether it ran, a
/// support function an arm runs that may be a user's (until GUARD's shared predicate lands), and a
/// LIMIT in the tail the arms' copies share with the drain.
/// A RIGHT SEMI / RIGHT ANTI drain's streamed probe arms as a nest's embedded inner (`end`): the arms' end continues no
/// one slot.
const MARKING_ARMS: &str = "a top-k / build end over a streamed union's arms (D393: only a fold's, a full sort's or an owned keyed end's arms continue one slot)";

/// join#212 / join#213: RIGHT / FULL hand the probe's joined rows on, which a nest's embedded inner drain cannot take (the `m` rule
/// fuses them into the drain's end; an inner has none).
const INNER_HANDS: &str = "a RIGHT or FULL hash join's drain as a nest's embedded inner (its probe hands joined rows on)";
const LOUD_ARM: &str = "a drain over a streamed probe union whose arm can raise, call a user or VOLATILE function or read another pipeline before its probe (D393's drain arms)";
const USER_KEY: &str = "a drain over a streamed probe union whose hash clause, run-time pruning or index scan may call a user support function (D393's drain arms)";
pub(crate) const LIMIT_ARMS: &str = "a LIMIT over a drain whose probe is a streamed union's arms (D393's drain arms)";
/// D1922 (the fused RIGHT SEMI drain under a stop).
pub(crate) const SEMI_STOP: &str = "a RIGHT SEMI drain under a LIMIT whose probe may raise or call what acts (PG returns each build row at its first match and may never run the rest of the probe; D1922)";

/// D1752: PG's Append runs its arms in turn, each arm's join whole (its drain included) before the next arm's first row.
/// frame/mechs.rs runs each arm after its own child's pipelines where arms.rs `arm_runs` takes the union; a pipeline that
/// still runs before PG would stop (one between the probe and the drain, or under a LIMIT a later child's pipeline the
/// LIMIT's check does not stop: a build under an outer LIMIT, a computed count, an end that reads fewer rows,
/// rerun/union_arms.rs `full_fed`) and may raise or act would show (arms.rs `arm_order_unseen`; a union the order does not
/// take keeps decompose's numbering, `between_unseen`).
const LATER_ARMS: &str = "a drain an Append's arm under a LIMIT or an end that may stop early, a pipeline PG would not run yet that runs before the stop and may raise or call a user or VOLATILE function (D1752)";

/// An Append op that only moves rows (spool_admit.rs `op_acts`): a feed, an arm marker or a union with no pruning steps (a
/// pruning step calls its comparison functions; a MergeAppend compares by its keys' operators).
pub(crate) fn bare_append(op: &Op<'_>) -> bool {
    matches!(op, Op::Append(AppendDesc::Feed { .. } | AppendDesc::Arm { prune: None, .. } | AppendDesc::Union { prune: None, .. }))
}

/// Condition (a): an op the probe pipeline may run as its tail.
pub(super) fn tail_op(op: &Op<'_>) -> bool {
    match op {
        Op::Filter(_) | Op::Project(_) => true,
        _ if limit_bounds(op).is_some() => true,
        // a full sort's op stores each row's run record (the runs are the end's, continued by the arms: V4R-159)
        Op::Sort(s) => matches!(s.form, SortForm::Runs) && s.presorted == 0,
        Op::Join(JoinDesc::Probe { catalog, outer_first, marks, jointype, .. }) => !catalog && !marks && (!outer_first || fills_outer(*jointype)),
        // D1187: an Append's arm marker (the probe's rows are its arm's); a pruning arm's skip word is the drain pipeline's
        Op::Append(AppendDesc::Arm { prune, .. }) => prune.is_none(),
        // B4d: a hash build end's op (the end's own record append; the probe arm's records are its partial of the build)
        Op::Join(JoinDesc::Build { .. }) => true,
        _ => false,
    }
}

/// A LEFT or ANTI probe on PG's outer-first path (HJ_FILL_OUTER: nodeHashjoin.c
/// fetches the first outer row before it builds) runs as a tail op too. Each outer row's output is its own (its matches
/// or its one NULL-extended row; ANTI: the row when nothing matches), no state crosses rows, and the two copies (the
/// probe's tail, the drain's op) together see exactly the rows PG's one node sees, probe-phase rows first. A fill type
/// never ends a pipeline at an empty build (probe_fill.rs `Fill::of`: not prefix-safe), so the probe pipeline's own ops
/// and marks run in full as PG's outer fetch runs them. The build's held error (op 14) is raised by the first row either
/// copy sees, which is the first row PG's node sees. A lazy build (a volatile side, D824) waits for one probe only: the
/// second copy finds its group lazy and refuses by name (probe_inner.rs). INNER / SEMI on the outer-first path are not
/// fill types: an empty build would end the probe pipeline on the drain's prefix alone, so they stay the `m` rule.
fn fills_outer(jt: JoinType) -> bool {
    matches!(jt, JoinType::JOIN_LEFT | JoinType::JOIN_ANTI)
}

/// D4096: the effects of probe `k` with what feeds it, then of each pipeline between it and drain `d` on its own (one walk
/// each): what a spooled drain's probe runs whole before the drain emits, the reading `spool_ahead` asks the whole-read
/// licence with.
fn drain_effects(px: &Px<'_, '_>, k: usize, d: usize) -> gravity_decompose::effects::Effects {
    let gx = px.gx;
    (k + 1..d).fold(crate::effects::pipeline_effects(gx, px.arena, k, true), |e, q| e.and(crate::effects::pipeline_effects(gx, px.arena, q, false)))
}

/// `m`'s end: the drain's end with the drain's ops as its tail (fused: the sink; seeded: the first of two arms), else
/// the seeded form not built, or refused.
fn end<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let gx = px.gx;
    let node = gx.pipelines()[k].node_id;
    let Some(m) = breaker_of(&gx.pipelines()[k].desc) else { crate::refuse!(node, Reason::Bug { payload: "a drain spool end that is no breaker" }) };
    // V4R-533: an arm of a streamed probe union is a probe of the drain the last arm's spool feeds, each arm fused (or seeded)
    // as a lone probe is; merge_arms joins the arms' seeded runs into one (the probe arms .. the drain)
    let run = gx.drained_arms(k);
    if run.is_some_and(|(f, l)| gx.user_drain_arms(f, l)) {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy(USER_KEY) });
    }
    // the first arm may be a FunctionScan PG fetches before it builds (arms.rs `fetched_arm`)
    if run.is_some_and(|(f, l)| !gx.quiet_drain_arms(f, l) && !(gx.fetched_arm(f) && gx.quiet_drain_arms(f + 1, l))) {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy(LOUD_ARM) });
    }
    let Some((d, jd)) = drain_of(gx, run.map_or(k, |r| r.1), m.slot) else {
        // join#212 / join#213: a RIGHT SEMI / RIGHT ANTI drain as a nest's embedded inner reads the build alone
        return match inner_drain_of(gx, run.map_or(k, |r| r.1), m.slot).map(|(d, jd)| (d, jd, &gx.pipelines()[d].ops[jd])) {
            Some((_, _, Op::Join(JoinDesc::Drain { jointype: JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT_ANTI, .. }))) if run.is_none() && gx.inner_spooled(k).is_none() => mcx::pg_box_dyn!(px.mcx(), NoRows => dyn EndMech + 'g),
            // INNER-SPOOL: a RIGHT / FULL drain as a nest's embedded inner: the probe's joined rows spooled, the inner reading
            // them, then the drained records (spool.rs), where spool_admit.rs `inner_spool_refusal` names no reason; D3915:
            // a streamed union's arms too, each filling the one spool (`inner_spooled`). D4063: a RIGHT ANTI / RIGHT SEMI
            // drain `inner_spooled` takes, its spool empty (the probe marks)
            Some((d, jd, Op::Join(JoinDesc::Drain { jointype: jt @ (JoinType::JOIN_RIGHT | JoinType::JOIN_FULL | JoinType::JOIN_RIGHT_ANTI | JoinType::JOIN_RIGHT_SEMI), .. }))) if matches!(jt, JoinType::JOIN_RIGHT | JoinType::JOIN_FULL) || run.is_none() => match gx.inner_spool_refusal(k, d, jd) {
                // D4096: a form that holds only when the statement's sink is read whole (spool_admit.rs
                // `inner_spool_whole_read`) is served to such a reader alone under the whole-read licence the graph
                // drain's spooled form holds (`shared::whole::read`, `spool_ahead`): over the probe through the drain, with
                // the statement's acting set, as the owner's end and its readers past the drain meet the rows in the
                // spool's order. Code that acts past call-local builtins keeps A2END
                None if gx.inner_spool_whole_read(k, d, jd) => {
                    let own = drain_effects(px, k, d).and(gravity_decompose::effects::Effects { acts: gx.graph.acts, ..gravity_decompose::effects::Effects::NONE });
                    match crate::ops::shared::whole::read(px, k, own) {
                        Some(_) => px.spool_drain_end(k, d, jd),
                        None => crate::refuse!(node, Reason::Shape { why: ShapeWhy(A2END) }),
                    }
                }
                None => px.spool_drain_end(k, d, jd),
                Some(why) => crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) }),
            },
            // a RIGHT SEMI / RIGHT ANTI drain's arms: the marking arms have no form here (the name append/lower.rs gave them)
            Some(_) if run.is_some() => crate::refuse!(node, Reason::Shape { why: ShapeWhy(MARKING_ARMS) }),
            Some(_) => crate::refuse!(node, Reason::Shape { why: ShapeWhy(INNER_HANDS) }),
            None => crate::refuse!(node, Reason::Bug { payload: "a drain spool no drain reads" }),
        };
    };
    // a fold / full sort / hash build end whose arms continue one slot (source/append.rs `continues`, D393 / D393b / V4R-159;
    // B4d V4R-476: a build's arms append records, the last arm lays the table out) takes the probe as its first arm; an
    // owner-less keyed end, or a pipeline between that reads the end's own production, is the seeded form not built
    // (refused by its Shape, D1331); any other end has no partial form
    let gd = gx.pipelines()[d].desc;
    let mut lone = false;
    let arms = match (gx.end_kind(d), breaker_of(&gd)) {
        (EndKind::Sink, _) => None,
        // row A2a: an INSTEAD OF view UPDATE / DELETE takes the spooled form below
        (EndKind::RowOp(_), _) if gx.a2a_write(d).is_some() => None,
        // D3800: or an UPDATE / DELETE through a nest past the drain over its held row (spool_admit.rs `held_write`)
        (EndKind::RowOp(_), _) if gx.held_write(d).is_some() => None,
        // D2247: a plan Material over the drain takes the spooled form below (its Spool op is no tail op)
        (EndKind::Breaker(BreakerKind::Materialize), _) if gx.spool_kind(d) == KindId::NestSpool => None,
        // writesubplan (heldout ald_joindrain-0014, rnd-1868): a CTE's store takes the spooled form below (`cte_store`)
        (EndKind::Breaker(BreakerKind::Materialize), _) if cte_store(gx, d) => None,
        // drain (regress domain#507): a settled union's record takes the spooled form below (spool_arms.rs `union_record`)
        (EndKind::Breaker(BreakerKind::Materialize), _) if union_record(gx, d) => None,
        // a drain under a drain (the next right-family join's spool) takes the spooled form below: its probe marks
        (EndKind::Breaker(BreakerKind::Materialize), _) if gx.spool_kind(d) == KindId::JoinDrain && !gx.pipelines()[d].ops[jd + 1..].iter().all(tail_op) => None,
        (EndKind::Breaker(_), Some(b)) if continues(&gd) && between(gx, k, d, b.slot) && Parallel::Arms.on() => Some(b.slot),
        // D3731: a hashed SubPlan's set fill (no partial form) takes the spooled form below (`set_fill`), whatever its tail
        (EndKind::Breaker(BreakerKind::Distinct), _) if set_fill(gx, d) => {
            lone = true;
            None
        }
        // core-0187: a RIGHT SEMI / RIGHT ANTI drain alone feeds an end the arms do not take: the spooled form below (`alone`)
        (EndKind::Breaker(_), _) if alone(gx, d, jd) => {
            lone = true;
            None
        }
        (EndKind::Breaker(BreakerKind::PlainAgg | BreakerKind::HashAgg | BreakerKind::Distinct | BreakerKind::Sort | BreakerKind::HashBuild), _) => {
            crate::frame::tail::seeded_drain(node)
        }
        // D3752: an INSERT .. ON CONFLICT whose stored rows the drain's order would pick refuses by its own name
        _ => crate::refuse!(node, Reason::Shape { why: ShapeWhy(conflict_refusal(gx, d).unwrap_or("a drain under an end with no partial form")) }),
    };
    // NEW-53 (was D1443's leader-only arms): a keyed end that stays resident (its records keep key copies, emit_key.rs
    // `resident`) runs its arms at the run's width; a continuing arm hands its phase-1 tables to the last arm's owner phase
    // (driver `Node::hand_off`: in memory under the statement's grant, else in disk files)
    let pd = &gx.pipelines()[d];
    // V4R-533: a tail's LIMIT counts in the drain's union, which the arms' copies do not find past their own marker
    let tail_limit = holds_limit(&pd.ops[jd + 1..]);
    if run.is_some() && tail_limit {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy(LIMIT_ARMS) });
    }
    // D1922 (D3377): or a RIGHT SEMI drain `spooled_drain` spools under a partial reader (its `semi_cursor`)
    if cte_store(gx, d) || init_cell(gx, d) || lone || !pd.ops[jd + 1..].iter().all(tail_op) || gx.spooled_drain(k) == Some((d, jd)) {
        // V4R-536 (A1a): the spooled form's refusals by name (join/drain_spool.rs), after row A2a's own; what passes them
        // keeps the `m` rule
        if let Some(why) = a2a_refusal(gx, d).or_else(|| gx.spool_refusal(k, d, jd)) {
            crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) });
        }
        // D3637: an end whose money or interval sums or averages read the records' order only by an overflow
        // takes them under their licence (`effects::drain_sums`), else the spooled form's A2END names it
        if gx.spooled_drain(k) == Some((d, jd)) && !crate::effects::drain_sums(px, k, d) {
            crate::refuse!(node, Reason::Shape { why: ShapeWhy(A2END) });
        }
        // A1b: the probe's joined rows spooled, the drain reading them then its drained records (nest/spool_drain.rs); a
        // form that holds only when its sink is read whole is served to such a reader alone (D1922, `spool_ahead`)
        if gx.spooled_drain(k) == Some((d, jd)) {
            let _ahead = spool_ahead(px, k, d, jd);
            return px.spool_drain_end(k, d, jd);
        }
        crate::refuse!(node, Reason::Shape { why: ShapeWhy("a drain whose tail keeps state per pipeline (the `m` rule)") });
    }
    // D1757's ACTING_TAIL is gone (a VOLATILE or user call's count is free, at least once per row
    // whose value is used, never cached across rows): the drain hands the build's records on in record order where PG's
    // ExecScanHashTableForUnmatched goes bucket by bucket, an order PG's own plan moves (nbuckets, batching, the join
    // method: DECISIONS 6b), so a tail or end whose calls act runs once per row that reaches it as PG's does; under a LIMIT
    // or an end that may stop early that order picks WHICH rows reach it, which DECISIONS 5 (a) leaves free (an unordered
    // LIMIT / FETCH), raise included.
    // D1752: the drain an Append's arm (a RIGHT JOIN per partition), under a LIMIT or an end that may stop early: PG may
    // stop before it runs a later arm, so what still runs before the stop must run unseen. An Append asked for no row
    // (LIMIT 0: frame/iface.rs's request) runs no arm here either (D1921's feeders check the count first)
    let arm = pd.ops[jd + 1..].iter().any(|op| matches!(op, Op::Append(AppendDesc::Arm { .. })));
    let none = gx.arm_append(d).is_some_and(|a| px.request(a).bound == crate::frame::iface::Bound::Rows(0));
    if arm && !none && crate::effects::visible::drain::order_picks_calls(gx, d, tail_limit) {
        let unseen = match gx.arm_runs().into_iter().find(|r| r.arms.contains(&d)) {
            Some(r) => gx.arm_order_unseen(px, &r, k, d),
            None => gx.between_unseen(k, d),
        };
        if !unseen {
            crate::refuse!(node, Reason::Shape { why: ShapeWhy(LATER_ARMS) });
        }
    }
    // D1922: a RIGHT SEMI drain hands nothing on during the probe, where PG's Hash Right Semi Join returns each build row at
    // its first match; under a LIMIT in the tail PG may never run the rest of the probe, so the probe must show nothing of
    // how far it ran (D393's quiet: no raise, no call that acts). Into a sink, a reader that takes every row sees PG run
    // the whole probe too, so the program is served to such a reader alone (`Px::whole_read`, the portal-kind fact): a
    // named cursor's FETCH or a SQL function's lazy row delegates it (exec admit/entry `PortalKind`). The probe holds
    // the whole-read licence (`shared::whole::read`: one that acts past call-local builtins holds none)
    let mut ahead = None;
    if matches!(pd.ops[jd], Op::Join(JoinDesc::Drain { jointype: JoinType::JOIN_RIGHT_SEMI, .. })) && !gx.spool_quiet(k) {
        if tail_limit {
            crate::refuse!(node, Reason::Shape { why: ShapeWhy(SEMI_STOP) });
        }
        if gx.end_kind(d) == EndKind::Sink {
            let own = crate::effects::pipeline_effects(gx, px.arena, k, true);
            ahead = crate::ops::shared::whole::read(px, k, own);
        }
    }
    // the probe's descriptor consumes its own slots, then those the tail's probes read (their builds precede the probe); a
    // probe that is itself a spooled drain's reader (a drain under a drain) reads its spool last (spool.rs's `DrainReader`
    // made its ops' slot indexes so), so its own slots keep that order
    let mut consumes: mcx::PgVec<'g, u8> = mcx::to_vec_in(&gx.pipelines()[k].desc.consumes[..], px.mcx());
    if let Some(Op::Join(JoinDesc::Drain { matches, .. })) = spooled_probe(gx, k).map(|(_, jk)| &gx.pipelines()[k].ops[jk]) {
        consumes.retain(|&s| s != *matches);
        consumes.push_in(*matches);
    }
    let own = consumes.len();
    consumes.extend_in(pd.desc.consumes.iter().copied().filter(|&s| s != m.slot && !gx.pipelines()[k].desc.consumes.contains(&s)));
    let lives: mcx::PgVec<'_, Option<LifeId>> = consumes.iter().enumerate().map(|(i, &s)| gx.life_of(if i < own { k } else { d }, s)).collect_in(px.mcx());
    px.open_tail(d, consumes.to_vec());
    let ident = px.ident(k);
    let passes = px.pass_id();
    let (steps, sink) = px.making(d, ident, |px| {
        px.set_union(d, op_node(gx, d, jd) as u32, jd);
        let mcx = px.mcx();
        let steps: mcx::PgVec<'g, PgBox<'g, dyn StepMech + 'g>> = (jd + 1..pd.ops.len()).map(|j| (kind_impl::<StepMaker>(&node, gx.step_need(d, j)).0)(px, d, j)).collect_in(mcx);
        (steps, (kind_impl::<EndMaker>(&node, gx.end_maker(d)).0)(px, d))
    });
    if px.pass_id() != passes {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy("a drain whose end adds a pass (the tail would run it twice)") });
    }
    px.set_tail(ident, steps);
    // the drain opens only after the probe ended, its section joined (every worker's marks stored): an order edge the
    // producer order carries (Program::producers_of; exec v4/m7.rs), never the pipelines' order alone.
    // B4c-2 (V4R-414): a spill pair P' remakes this end under its id (spill_pair.rs `pair`), so the same line declares
    // Follows(d, P'): the drain opens after every claim of P' closed (each partition's drain part written, the probe's
    // consumed build the resident region again); the run and the sink's order stay the probe's own
    px.declare_proto(Proto::new(ProtoKind::Follows, d as u16).peer(ident as u16));
    if let Some(slot) = arms.filter(|_| ident == k) {
        let mut p = Proto::new(ProtoKind::Arms, k as u16);
        (p.arg, p.slot) = ((d + 1 - k) as u64, slot);
        px.declare_proto(p);
    }
    // D281: a LIMIT's counters are one open's, so a tail holding one runs on the leader; else the probe's sink is ordered
    // when the probe may run parallel (end/sink_ordered.rs, declared here: the sink was made with the drain's shape)
    let limit = holds_limit(&pd.ops[jd + 1..]);
    // D2594: a rerun set's member probe opens at every run of the set, before the drain (`Follows`): its open clears the
    // union's flag, so the tail's union words (a LIMIT's counters, a streamed group's) start again at each run, as PG's
    // ExecReScanLimit; the drain's open finds the flag set and keeps them. The probe then runs serial, one call per run
    // (`pipeline_desc`): each claim's call of an eligible one opens (frame.rs), which would clear the flag mid-run
    let rerun = gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(k as u16)));
    let flag = rerun.then(|| px.frame_word(WordKey::UnionFlag { union: op_node(gx, d, jd) as u32 }) as i32 * 8);
    let par = !limit && !rerun && matches!(gx.pipelines()[k].desc.parallel, gravity_program::Parallel::Eligible);
    let form = match arms.is_none() && par && SinkKind::Ordered.on() {
        true => {
            if !px.protos.iter().any(|p| p.kind == ProtoKind::OrderedSink && p.pipeline as usize == ident) {
                px.declare_proto(Proto::new(ProtoKind::OrderedSink, ident as u16));
            }
            SinkKind::Ordered
        }
        false => sink.sink(),
    };
    let marks_only = matches!(pd.ops[jd], Op::Join(JoinDesc::Drain { jointype: JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT_ANTI, .. }));
    mcx::pg_box_dyn!(px.mcx(), Fused { inner: sink, d, consumes, lives, limit, form, flag, marks_only, ahead } => dyn EndMech + 'g)
}

/// A1b (D1922, D3377): the whole-read licence of a spooled drain's probe, run whole into its spool before the drain
/// emits, where the form holds only for a reader that takes every row (`Gx::spool_whole_read`) and a partial reader's
/// pulls do not stream the joined phase exactly (spool_admit.rs `drain_exact`): served to such a reader alone
/// (`shared::whole::read`, over the probe's effects and those of the pipelines up to the drain). `None` otherwise, or
/// for code that acts past call-local builtins.
fn spool_ahead(px: &mut Px<'_, '_>, k: usize, d: usize, jd: usize) -> Option<gravity_decompose::effects::Licence> {
    let gx = px.gx;
    if !gx.spool_whole_read(k, d, jd) || gx.drain_exact(k, d, jd) {
        return None;
    }
    // the probe with what feeds it, then each pipeline between it and the drain on its own (one walk each)
    let own = (k + 1..d).fold(crate::effects::pipeline_effects(gx, px.arena, k, true), |e, q| e.and(crate::effects::pipeline_effects(gx, px.arena, q, false)));
    crate::ops::shared::whole::read(px, k, own)
}

/// writesubplan (heldout ald_joindrain-0014 / ald_joinchain-0005, rnd-1868 / rnd-3084): drain `d` ends in a CTE's store
/// (nest.cte: `WITH q AS MATERIALIZED (.. a Hash Right / Right Semi Join ..)`). Its end is the CTE's one store, whose pull
/// stops resumably after any stored row (ctescan/lower.rs `CteEnd`), so the fused form, which makes the drain's end in the
/// probe pipeline too, would declare a second store for it: the spooled form instead (row A1b), the probe's joined rows
/// spooled, then the drain reading them and its drained records into the store, as PostgreSQL's CteScan pulls the Hash
/// Join's joined rows and then its unmatched ones. Its refusals are join/drain_spool.rs's (TAILSTOP as under a drain:
/// a reader may stop the pull early).
fn cte_store(gx: &Gx<'_, '_>, d: usize) -> bool {
    gx.end_kind(d) == EndKind::Breaker(BreakerKind::Materialize) && gx.spool_kind(d) == KindId::NestCte
}

/// D1187: the probe may seed the drain's end across the pipelines between them when none reads the end's slot: any
/// producing it is then an arm of the drain's run (a RIGHT JOIN per partition under one Append), which tail.rs's
/// merge_arms joins to the probe's. B4d: a pipeline between that reads another life under the same number (decompose reuses
/// a number at its last reader: a life before the probe, or another partition's build or spool between, whose end is not
/// the drain end's kind) reads no row of the drain's end; tail.rs's renumber_seeded renames the lives between.
fn between(gx: &Gx<'_, '_>, k: usize, d: usize, slot: u8) -> bool {
    let kind = |x: usize| end_breaker(gx, x);
    (k + 1..d).all(|q| !gx.pipelines()[q].desc.consumes.contains(&slot) || gx.life_of(q, slot).is_some_and(|l| (l.0 as usize) < k || kind(l.0 as usize) != kind(d)))
}

/// An end whose arms continue one slot (source/append.rs's rule): a plain fold, a full sort, a top-N (each arm's run cut to
/// the top's count, the runs merged as a full sort's are: D2077's form), a hash build (B4d), an owned keyed end that
/// spills by partition.
fn continues(d: &Pipeline) -> bool {
    match breaker_of(d) {
        Some(b) => match b.kind {
            BreakerKind::PlainAgg | BreakerKind::Sort | BreakerKind::TopK | BreakerKind::HashBuild => true,
            BreakerKind::HashAgg | BreakerKind::Distinct => d.owner.is_some() && matches!(b.spill, Spill::Partitioned { .. }),
            _ => false,
        },
        None => false,
    }
}

/// D3731 (Django's exclude() / ~Q(..) across a nullable relation, an IN over a LEFT join in a HAVING): drain pipeline `d` ends
/// in a hashed SubPlan's set (graph.rs `set_kind`: a Distinct no group declares, its pipeline's SubPlan op the Build): the
/// right join is the SubPlan's own plan. The set has no partial form, so the drain takes the spooled form: the probe's
/// joined rows spooled, then the drain reading them and its drained records into the set, as PG's buildSubPlanHash reads
/// the plan to its end. The set answers membership only, so the records' order does not reach a row (spool_admit.rs,
/// effects/visible/drain.rs `order_free_in`).
pub(crate) fn set_fill(gx: &Gx<'_, '_>, d: usize) -> bool {
    gx.end_kind(d) == EndKind::Breaker(BreakerKind::Distinct) && gx.end_need(d) == KindId::NestSubplan
}

/// drain (gen dr-0195): a drain into a non-CTE InitPlan's cell (nest.initplan's end: an EXISTS, scalar or ARRAY InitPlan
/// over a right-family hash join). The fused form makes the probe a second writer of the cell (its own Init proto beside
/// the drain's), and the probe's on-demand members then share the build with the drain (frame/program.rs: 'an InitPlan
/// whose producer a pipeline outside it also reads'). The spooled form keeps the drain the cell's only writer, the probe
/// its member through the spool (members_in: the build, the probe, the drain), past the spooled form's refusals by name.
fn init_cell(gx: &Gx<'_, '_>, d: usize) -> bool {
    gx.end_need(d) == KindId::NestInitplan
}

/// core-0187 (tour-sortset: a hashed INTERSECT's side group over a Hash Right Semi Join): a RIGHT SEMI / RIGHT ANTI drain
/// hands nothing on during the probe (its arm's tables would stay empty: D1443 in `end`), so the drain alone feeds its end.
/// Its breaker end, other than a plan Material, takes the spooled form where the seeded arms do not take it (no partial
/// form, or a pipeline between reads the end's production: D1331; the callers' `arms`): the probe's spool holds no joined
/// row and the drain pipeline hands its build's matched (unmatched) records on to the end as its only producer, past the
/// spooled form's refusals by name (spool_admit.rs; A2END keeps an end that shows the records' order).
fn alone(gx: &Gx<'_, '_>, d: usize, jd: usize) -> bool {
    let semi = matches!(gx.pipelines()[d].ops.get(jd), Some(Op::Join(JoinDesc::Drain { jointype: JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT_ANTI, .. })));
    semi && matches!(gx.end_kind(d), EndKind::Breaker(b) if !matches!(b, BreakerKind::Materialize))
}

/// The probe pipeline's end in the fused form: the drain's sink, the pipeline an emit over the fused slots.
struct Fused<'g> {
    inner: PgBox<'g, dyn EndMech + 'g>,
    d: usize,
    consumes: mcx::PgVec<'g, u8>,
    lives: mcx::PgVec<'g, Option<LifeId>>,
    limit: bool,
    form: SinkKind,
    /// D2594: the union flag word (a byte offset) a rerun set member's open clears.
    flag: Option<i32>,
    /// D3355: a RIGHT SEMI / RIGHT ANTI drain: the probe marks the build's records and hands no row on (D1922), so every row
    /// the end receives is the drain's.
    marks_only: bool,
    /// Construct C2: a RIGHT SEMI drain's probe, run whole before the drain emits into a sink, under the whole-read
    /// licence (`end`, D1922); `None`: a quiet probe, another end, or a probe that acts past call-local builtins. Held,
    /// not read.
    #[allow(dead_code)]
    ahead: Option<gravity_decompose::effects::Licence>,
}

impl EndMech for Fused<'_> {
    fn state_bytes(&self) -> u32 { self.inner.state_bytes() }
    fn open_words(&self, w: &mut OpenWords) {
        self.inner.open_words(w);
        if let Some(f) = self.flag {
            w.declare(OpenWord::open("join.drain union flag (a rerun set's run)", Base::Words, f, Type::I64, 0));
        }
    }
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) { self.inner.open(b, env) }
    fn carried<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>) -> mcx::PgVec<'e, Value> { self.inner.carried(b, env) }
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, x: &Exits) -> Carry<'e> { self.inner.lower(b, env, row, x) }
    fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, c: &Carry) { self.inner.window_end(b, env, c) }
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) { self.inner.finish(b, env, x) }
    fn sink(&self) -> SinkKind { self.form }
    fn wants_locator(&self) -> bool { self.inner.wants_locator() }
    fn whole(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, win: &Win, c: &Carry, l_wend: Block) -> bool { self.inner.whole(b, env, win, c, l_wend) }
    fn block<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, w: &WordWin, base: Value, m: Mask, c: &Carry) -> Option<Carry<'e>> { self.inner.block(b, env, w, base, m, c) }
    fn block_or_walk<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, w: &WordWin, base: Value, m: Mask, c: &Carry) -> Option<(Carry<'e>, Block)> { self.inner.block_or_walk(b, env, w, base, m, c) }
    fn counts(&self, env: &Env) -> bool { self.inner.counts(env) }
    fn takes_mult(&self) -> bool { self.inner.takes_mult() }
    fn takes_set(&self) -> bool { self.inner.takes_set() }

    /// The drain's end on the probe: its end fields (the sink, or the continued fold's slot, owner, words, admit, top-k,
    /// finish, a hash build's rows) from the drain's descriptor, amended by the end as the drain's own is; the fused slots,
    /// each read from its life; serial under a tail's LIMIT, and in a rerun set (D2594: one open per run). D3355: under a
    /// RIGHT SEMI / ANTI drain the probe's own rows never reach the end, so the end is sized by the drain's rows (a hash
    /// build's Hash rows, else the planner's rows at the drain's end, at least 1), never by the probe's driving scan: an
    /// anti-join's probe whose drain feeds a semi join's build sized that build by the probing scan's rows (refused past
    /// one region by D272's gate), and the semi join's probe sized the GROUP BY table its drain feeds the same way.
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize, d: Pipeline) -> Pipeline {
        let drain = &px.gx.pipelines()[self.d].desc;
        let base = crate::frame::iface::with_end_of(drain, d);
        let base = match self.marks_only {
            true if base.build_rows == 0 => Pipeline { build_rows: drain.end_rows.max(1), ..base },
            _ => base,
        };
        let d = self.inner.pipeline_desc(px, k, base);
        let mut n = Pipeline { consumes: &[], consumes_from: &[], ..d };
        for (&s, l) in self.consumes.iter().zip(self.lives.iter()) {
            n = match l {
                Some(l) => n.consuming_from_in(px.arena, s, *l),
                None => crate::refuse!(px.gx.pipelines()[k].node_id, Reason::Bug { payload: "a fused drain's slot with no producer" }),
            };
        }
        if self.limit || self.flag.is_some() {
            n.parallel = gravity_program::Parallel::Ineligible { site: n.driving_site, reason: Why::Sink };
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use gravity_abi::breaker::Breaker;
    use gravity_decompose::effects::Volatility;
    use gravity_decompose::graph::{Bound, InitPlan};
    use gravity_decompose::facts::nest::NestFacts;
    use gravity_decompose::ops::shared::modifytable::{MergeDesc, OnConflict, RouteDesc, RowMark, Target, WriteDesc};
    use gravity_decompose::{Am, InsteadOf, MemberSet, NoFacts, PipelineGraph, PipelineNode, Source};
    use gravity_program::{Arena, ArenaBox, ArenaVec, End, Reason, SiteDesc, WaitPolicy, WritePlan};
    use types_nodes::list::{IntList, NodeList, OidList};
    use types_nodes::plannodes::RowMarkType;

    use gravity_decompose::ops::shared::subplan::SubLink;

    use super::*;

    /// One change to row A2a's shape: the write's command, target kind, trigger or extras; or beside it a writable
    /// statement's other write, a CTE (its slot or none), a nest loop in the drain's tail over a pgrcolumnar or heap inner,
    /// a pgrcolumnar probe with no SubPlan or nest; a LIMIT in the drain's tail before the write; no write (a read).
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum W {
        Plain,
        Delete,
        Insert,
        Cmd(Cmd),
        Kind(TargetKind),
        NoTrigger,
        MergeView,
        Merge,
        /// A4-MATEND: a MERGE into the heap table the probe pipeline scans (`bool`: or another one)
        MergeHeap(bool),
        /// An UPDATE of the heap table the build scans under a RIGHT ANTI drain (`bool`: or another one)
        DrainedTarget(bool),
        /// The same under a RIGHT SEMI drain (gen mt-0076 group)
        DrainedSemi(bool),
        /// D3664: `DrainedSemi(true)` with the write routed through a partitioned root pruned to its one leaf (the row may move)
        DrainedRouted,
        Conflict,
        /// D3752: a heap INSERT .. ON CONFLICT (`bool`: DO UPDATE, else DO NOTHING)
        Upsert(bool),
        Route,
        LockMark,
        OldCols,
        MarkJunk,
        OtherWrite,
        /// D2131: the drain's rows into pipeline 3, the root, which holds the write (`bool`: a heap UPDATE, not the INSTEAD OF
        /// view UPDATE)
        RootWrite(bool),
        Cte(Option<u8>),
        Nest(Am),
        Rc2Probe,
        Limit,
        Read,
        /// core-0187: no write, the drain (of this join type) into a HashAgg with no owner (a hashed set side group or
        /// DISTINCT: no partial form), pipeline 3 the root reading it
        AggEnd(JoinType),
    }

    /// Build 0, the quiet RIGHT probe 1 into its spool 1, the drain 2 (its Drain op, a nest loop for `Nest`, a LIMIT for
    /// `Limit`, the projection, the write but for `Read`: an INSTEAD OF UPDATE of view 16390 when `Plain`), and a heap
    /// pipeline 3 (a CTE's or another write's); `f` asks the graph.
    fn with<R>(v: W, f: impl FnOnce(&Gx<'_, '_>) -> R) -> R {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil()));
        let ints: &IntList<'_> = Box::leak(Box::new(IntList::nil()));
        let oids: &OidList<'_> = Box::leak(Box::new(OidList::nil()));
        let scan = |am| Source::Range { site: SiteDesc::none_in(a), am, scanrelid: 1, relid: 16384 };
        let into = |kind, slot| Pipeline::serial(-1, End::Breaker(Breaker::sized(kind, 4096).in_slot(slot)), Reason::NoDrivingSite);
        let sink = || Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite);
        let write = |cmd, kind, instead| {
            let mut targets = ArenaVec::new_in(a);
            let (set, returning, merge, to_root) = (None, None, None, ArenaVec::new_in(a));
            let plan = WritePlan::write(a, cmd, 16390, &[]);
            targets.push(Target { rti: 1, relid: 16390, kind, site: SiteDesc::none_in(a), plan, indexes: ArenaVec::new_in(a), junk_col: 0, wholerow_col: 3, instead, generated: ArenaVec::new_in(a), set, returning, merge, to_root });
            let plan = WritePlan::write(a, cmd, 16390, &[]);
            let (checks, leaves, mark_descs, old_cols) = (&[][..], &[][..], &[][..], &[][..]);
            let site = SiteDesc::none_in(a);
            WriteDesc { cmd, rti: 1, site, plan, set: None, returning: None, on_conflict: None, epq: 0, marks: nil, merge: None, targets: targets.leak(), tableoid_col: 0, checks, route: None, leaves, leaves_known: false, old_new: Default::default(), mark_descs, copy_arms: &[], members: MemberSet::default(), old_cols }
        };
        let both = InsteadOf { insert: false, update: true, delete: true, known: true };
        let mut w = match v {
            W::Delete => write(Cmd::Delete, TargetKind::View, both),
            W::Insert | W::Upsert(_) => write(Cmd::Insert, TargetKind::Heap, InsteadOf { update: false, delete: false, ..both }),
            W::Cmd(c) => write(c, TargetKind::View, InsteadOf { insert: true, ..both }),
            W::Kind(k) => write(Cmd::Update, k, both),
            W::NoTrigger => write(Cmd::Update, TargetKind::View, InsteadOf { update: false, ..both }),
            W::MergeView => write(Cmd::Merge, TargetKind::View, InsteadOf { insert: true, ..both }),
            W::MergeHeap(_) => write(Cmd::Merge, TargetKind::Heap, InsteadOf { update: false, delete: false, ..both }),
            W::DrainedTarget(_) | W::DrainedSemi(_) | W::DrainedRouted => write(Cmd::Update, TargetKind::Heap, InsteadOf { update: false, delete: false, ..both }),
            W::RootWrite(true) => write(Cmd::Update, TargetKind::Heap, InsteadOf { update: false, delete: false, ..both }),
            _ => write(Cmd::Update, TargetKind::View, both),
        };
        let mark = |lock, ctid_col| RowMark { rti: 2, relid: 16384, mark: RowMarkType::ROW_MARK_REFERENCE, lock, wait: WaitPolicy::Block, parent: false, prti: 2, ctid_col, toid_col: 0 };
        match v {
            W::Merge | W::MergeView | W::MergeHeap(_) => w.merge = Some(MergeDesc { join_cond: None, actions: ArenaVec::new_in(a), rowid_col: 0, target_cols: ArenaVec::new_in(a), returning_action: false }),
            W::Conflict => w.on_conflict = Some(OnConflict { update: true, arbiter: oids, set: nil, cols: ints, filter: None, excl_rti: 3 }),
            W::Upsert(update) => w.on_conflict = Some(OnConflict { update, arbiter: oids, set: nil, cols: ints, filter: None, excl_rti: 3 }),
            W::Route => w.route = Some(RouteDesc { root_rti: 1, root_relid: 16390, root_natts: 3, cross_partition: false }),
            W::DrainedRouted => {
                w.route = Some(RouteDesc { root_rti: 4, root_relid: 16395, root_natts: 3, cross_partition: true });
                w.mark_descs = &*Box::leak(vec![mark(LockKind::None, 0)].into_boxed_slice());
            }
            W::LockMark => w.mark_descs = &*Box::leak(vec![mark(LockKind::Exclusive, 0)].into_boxed_slice()),
            W::OldCols => w.old_cols = &[(2, 2)],
            W::MarkJunk => w.mark_descs = &*Box::leak(vec![mark(LockKind::None, 4)].into_boxed_slice()),
            _ => w.mark_descs = &*Box::leak(vec![mark(LockKind::None, 0)].into_boxed_slice()),
        }
        let mut g = PipelineGraph::new(a);
        let mut b = PipelineNode::new(a, 1, scan(Am::Heap), into(BreakerKind::HashBuild, 0));
        if v == W::DrainedTarget(true) || v == W::DrainedSemi(true) || v == W::DrainedRouted {
            b.source = Source::Range { site: SiteDesc::none_in(a), am: Am::Heap, scanrelid: 1, relid: 16390 };
        }
        if matches!(v, W::DrainedTarget(_) | W::DrainedSemi(_) | W::DrainedRouted) {
            b.push(Op::Join(JoinDesc::Build { keys: nil, decls: ArenaVec::new_in(a), pg_tuple: u32::MAX }));
        }
        g.pipelines.push(b);
        let jt = match v {
            W::DrainedTarget(_) => JoinType::JOIN_RIGHT_ANTI,
            W::DrainedSemi(_) | W::DrainedRouted => JoinType::JOIN_RIGHT_SEMI,
            W::AggEnd(jt) => jt,
            _ => JoinType::JOIN_RIGHT,
        };
        let mut k = PipelineNode::new(a, 2, scan(if v == W::Rc2Probe { Am::Pgrcolumnar } else { Am::Heap }), into(BreakerKind::Materialize, 1));
        if v == W::MergeHeap(true) {
            k.source = Source::Range { site: SiteDesc::none_in(a), am: Am::Heap, scanrelid: 1, relid: 16390 };
        }
        k.push(Op::Join(JoinDesc::Probe { slot: 0, jointype: jt, clauses: nil, keys: nil, qual: nil, unique: false, marks: true, decls: ArenaVec::new_in(a), inner_node: 1, outer_first: false, catalog: false }));
        g.pipelines.push(k);
        let end = match v {
            W::RootWrite(_) => into(BreakerKind::Materialize, 3),
            W::AggEnd(_) => into(BreakerKind::HashAgg, 3),
            _ => sink(),
        };
        let mut d = PipelineNode::new(a, 2, Source::Breaker { slot: 0 }, end);
        d.push(Op::Join(JoinDesc::Drain { matches: 1, jointype: jt }));
        if let W::Nest(am) = v {
            let facts = NestFacts { node: 77, invariant: true, volatility: Volatility::Immutable, raise_free: true, eq: None };
            let inner = ArenaBox::new_in(PipelineNode::new(a, 9, scan(am), sink()), a);
            d.push(Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, params: nil, inner, qual: nil, unique: false, inner_node: 9, facts }));
        }
        if v == W::Limit {
            d.push(Op::Limit { count: Bound::Const(1), offset: Bound::All, ties: None });
        }
        d.push(Op::Project(nil));
        let mut w = Some(w);
        if v != W::Read && !matches!(v, W::RootWrite(_) | W::AggEnd(_)) {
            d.push(Op::Write(w.take().unwrap()));
        }
        g.pipelines.push(d);
        let mut three = PipelineNode::new(a, 5, scan(Am::Heap), into(BreakerKind::Materialize, 2));
        if v == W::OtherWrite {
            three.push(Op::Write(write(Cmd::Insert, TargetKind::Heap, both)));
        }
        if let W::AggEnd(_) = v {
            three = PipelineNode::new(a, 5, Source::Breaker { slot: 3 }, sink());
            three.push(Op::Project(nil));
            g.root = 3;
        }
        if let (W::RootWrite(_), Some(w)) = (v, w) {
            three = PipelineNode::new(a, 5, Source::Breaker { slot: 3 }, sink());
            three.push(Op::Project(nil));
            three.push(Op::Write(w));
            g.root = 3;
        }
        g.pipelines.push(three);
        if let W::Cte(slot) = v {
            g.init_plans.push(InitPlan { plan_id: 9, pipeline: 3, link: SubLink::Cte, param: -1, params: ints, slot, volatility: Volatility::Immutable, raise_free: true, parallel_safe: true, attach: -1 });
        }
        f(&Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() })
    }

    /// `a2a_write(2)`, `spooled_drain(1)`, drain.rs's own refusal and join/drain_spool.rs's.
    #[allow(clippy::type_complexity)]
    fn verdict(v: W) -> (Option<usize>, Option<(usize, usize)>, Option<&'static str>, Option<&'static str>) {
        with(v, |gx| (gx.a2a_write(2), gx.spooled_drain(1), a2a_refusal(gx, 2), gx.spool_refusal(1, 2, 0)))
    }

    const N5: &str = "a write or row lock in a statement with a spooled drain (D2131)";
    const TAILSTOP: &str = "a spooled drain whose tail may raise under a LIMIT or a sink a cursor may stop (D2135)";

    /// Design r3 (1) / review r1 F2: an INSTEAD OF view UPDATE or DELETE is A2a's (the spooled form takes it, no name); every
    /// other write keeps its position and so the end's 'no partial form' (review r1 F1: MERGE, a table UPDATE .. FROM, ON
    /// CONFLICT; F2's list: a foreign target, the heap table a rule-rewritten or an auto-updatable view writes, a partitioned
    /// target), as do a view with no INSTEAD OF trigger for the command, a route and a locking mark. A MERGE into a view whose
    /// INSTEAD OF row triggers take each of its actions is A2a's (ExecMerge's view arm: no position, no recheck).
    #[test]
    fn a2a_is_an_instead_of_view_update_or_delete_alone() {
        assert_eq!(verdict(W::Plain), (Some(2), Some((2, 0)), None, None));
        assert_eq!(verdict(W::Delete), (Some(2), Some((2, 0)), None, None));
        assert_eq!(verdict(W::MergeView), (Some(2), Some((2, 0)), None, None));
        let kept = [W::Cmd(Cmd::Merge), W::Cmd(Cmd::Insert), W::Kind(TargetKind::Heap), W::Kind(TargetKind::Foreign), W::Kind(TargetKind::Partitioned), W::Kind(TargetKind::OtherAm)];
        let kept = kept.into_iter().chain([W::NoTrigger, W::Merge, W::Conflict, W::Route, W::LockMark]);
        for v in kept {
            let (a2a, spooled, own, _) = verdict(v);
            assert_eq!((a2a, spooled, own), (None, None, None), "{v:?} keeps 'a drain under an end with no partial form'");
        }
    }

    /// A4-MATEND: a MERGE into the heap table its drain's probe pipeline scans (the target probing, the source hashed) is
    /// A2a's kind of write, that probe pipeline the target's producer; a MERGE into another table keeps 'no partial form'.
    #[test]
    fn a_merge_whose_probe_scans_its_target_takes_the_spooled_form() {
        assert_eq!(verdict(W::MergeHeap(true)), (Some(2), Some((2, 0)), None, None));
        assert_eq!(with(W::MergeHeap(true), |gx| gx.merge_drained_target(2)), Some(1));
        assert_eq!(verdict(W::MergeHeap(false)), (None, None, None, Some(N5)), "the probe scans another table");
        assert_eq!(with(W::MergeHeap(false), |gx| gx.merge_drained_target(2)), None);
    }

    /// An UPDATE whose rows are a RIGHT ANTI drain's build rows of the target's own scan (`WHERE NOT EXISTS` with the target
    /// hashed) takes the spooled form, the build its target's producer; over another relation's build it keeps N5.
    #[test]
    fn an_update_of_the_drained_build_rows_takes_the_spooled_form() {
        assert_eq!(verdict(W::DrainedTarget(true)), (Some(2), Some((2, 0)), None, None));
        assert_eq!(with(W::DrainedTarget(true), |gx| (gx.drained_build_target(2), gx.merge_drained_target(2))), (Some(0), None));
        assert_eq!(verdict(W::DrainedTarget(false)), (None, None, None, Some(N5)));
        assert_eq!(with(W::DrainedTarget(false), |gx| gx.drained_build_target(2)), None);
    }

    /// gen mt-0076 group: the same under a RIGHT SEMI drain (`WHERE .. IN (..)` with the target hashed), whose probe then
    /// hands its first matches into the spool (`semi_hands`); a RIGHT ANTI drain's probe never does, nor a RIGHT SEMI one
    /// over another relation's build.
    #[test]
    fn an_update_of_a_right_semi_drains_first_matches_takes_the_spooled_form() {
        assert_eq!(verdict(W::DrainedSemi(true)), (Some(2), Some((2, 0)), None, None));
        assert_eq!(with(W::DrainedSemi(true), |gx| (gx.drained_build_target(2), gx.semi_hands(2))), (Some(0), true));
        assert_eq!(with(W::DrainedTarget(true), |gx| gx.semi_hands(2)), false, "a RIGHT ANTI drain");
        assert_eq!(verdict(W::DrainedSemi(false)), (None, None, None, Some(N5)));
        assert_eq!(with(W::DrainedSemi(false), |gx| (gx.drained_build_target(2), gx.semi_hands(2))), (None, false));
    }

    /// D3664: the same with the one target routed (a partitioned root pruned to its one leaf, the row free to move): the
    /// build stays the target's producer and the spooled form writes through the root's plan.
    #[test]
    fn a_routed_update_of_a_right_semi_drains_first_matches_takes_the_spooled_form() {
        assert_eq!(verdict(W::DrainedRouted), (Some(2), Some((2, 0)), None, None));
        assert_eq!(with(W::DrainedRouted, |gx| (gx.drained_build_target(2), gx.semi_hands(2))), (Some(0), true));
    }

    /// core-0187: a RIGHT SEMI / RIGHT ANTI drain into a HashAgg with no owner (no partial form) is fed by the drain alone, so
    /// the spooled form decides it: with no group to read, A2END names it, never join.drain's seeded form. A RIGHT drain,
    /// whose probe hands its joined rows on, keeps the seeded form's name.
    #[test]
    fn a_right_semi_or_anti_drain_alone_feeds_an_end_with_no_partial_form() {
        for jt in [JoinType::JOIN_RIGHT_SEMI, JoinType::JOIN_RIGHT_ANTI] {
            assert_eq!(with(W::AggEnd(jt), |gx| (alone(gx, 2, 0), gx.spooled_drain(1), gx.spool_refusal(1, 2, 0))), (true, None, Some(A2END)), "{jt:?}");
        }
        assert_eq!(with(W::AggEnd(JoinType::JOIN_RIGHT), |gx| (alone(gx, 2, 0), gx.spooled_drain(1))), (false, None));
        assert!(!with(W::DrainedSemi(true), |gx| alone(gx, 2, 0)), "a RIGHT SEMI drain into a write (no breaker end)");
    }

    /// D2247: a plain INSERT into one heap table takes the spooled form as A2a's view write does (no position, no recheck);
    /// with a route or into a view it keeps the end's 'no partial form' (ON CONFLICT: D3752's test).
    #[test]
    fn a2a_takes_a_plain_heap_insert() {
        assert_eq!(verdict(W::Insert), (Some(2), Some((2, 0)), None, None));
        assert_eq!(verdict(W::Cmd(Cmd::Insert)).0, None, "an INSERT into a view");
    }

    /// D3752: a heap INSERT .. ON CONFLICT is A2a's only when rerun/spool/upsert.rs `order_free` passes it; on a source
    /// that cannot answer the target's triggers it refuses, by its own name (never the end's 'no partial form'), and nothing
    /// is spooled. Another write with ON CONFLICT names nothing of D3752's.
    #[test]
    fn an_upsert_the_order_may_pick_refuses_by_its_name() {
        use crate::rerun::spool::upsert::{PICKS_NOTHING, PICKS_UPDATE};
        for (update, name) in [(true, PICKS_UPDATE), (false, PICKS_NOTHING)] {
            let (a2a, spooled, ..) = verdict(W::Upsert(update));
            assert_eq!((a2a, spooled), (None, None), "update {update}");
            assert_eq!(with(W::Upsert(update), |gx| conflict_refusal(gx, 2)), Some(name), "update {update}");
        }
        assert_eq!(with(W::Conflict, |gx| conflict_refusal(gx, 2)), None, "an UPDATE carrying ON CONFLICT");
        assert_eq!(with(W::Insert, |gx| conflict_refusal(gx, 2)), None, "a plain INSERT");
    }

    /// Condition (2), both ways: the write reads only the projection before it; a column carried from a target scan or a
    /// row mark's junk column that is no projection item refuses by A2END's name (never XX000), and nothing is spooled.
    #[test]
    fn a2a_write_reads_only_the_projection() {
        assert_eq!(verdict(W::Plain).2, None);
        for v in [W::OldCols, W::MarkJunk] {
            assert_eq!((verdict(v).1, verdict(v).2), (None, Some(A2END)), "{v:?}");
        }
    }

    /// Review r2 F4 and N5 (design r3 (3)): a CTE InitPlan, with its slot or none, no longer refuses on A2a's path (D2178
    /// lifted: writesubplan); a heap nest and a pgrcolumnar probe are served; N5 skips only the drain's own write, so
    /// another pipeline's write keeps N5.
    #[test]
    fn a2a_own_names_and_n5_beside_it() {
        assert_eq!(verdict(W::Cte(None)).2, None, "D2178 lifted");
        assert_eq!(verdict(W::Cte(Some(2))).2, None, "D2178 lifted");
        assert_eq!(verdict(W::Nest(Am::Heap)), (Some(3), Some((2, 0)), None, None), "the write is op 3 behind the nest");
        assert_eq!(verdict(W::Rc2Probe), (Some(2), Some((2, 0)), None, None));
        let (a2a, spooled, own, spool) = verdict(W::OtherWrite);
        assert_eq!((a2a, spooled, own, spool), (Some(2), None, None, Some(N5)));
    }

    /// D2131 (triggers#265): the statement's own INSTEAD OF view write in the root pipeline, past the drain, reads no position
    /// and never rechecks: N5 does not name it; a heap UPDATE there keeps its position and N5.
    #[test]
    fn n5_skips_a_positionless_root_write_past_the_drain() {
        let n5 = |v| with(v, |gx| gx.spool_refusal(1, 2, 0) == Some(N5));
        assert!(!n5(W::RootWrite(false)), "an INSTEAD OF view UPDATE in the root");
        assert!(n5(W::RootWrite(true)), "a heap UPDATE in the root");
    }

    /// The name A2a refuses by is join/drain_spool.rs's own (A2END): a change there fails here.
    #[test]
    fn a2a_names_are_drain_spool_own() {
        let me = include_str!("spool_admit.rs");
        for (name, text) in [("A2END", A2END)] {
            assert!(me.contains(&format!("pub(crate) const {name}: &str = \"{text}\";")), "{name}");
        }
    }

    /// Order board 103 (V4R-565's seam, board 69): an A2a write's drain is never streamed. Gx::drain_streams (1) streams
    /// only under a tail LIMIT or a sink end. An A2a end is its row operation, not the sink, and a LIMIT before the write
    /// keeps TAILSTOP (the write is no raise-free tail op), so no spooled A2a drain reaches the stream. A streamed write
    /// under a parked pull would re-read (D2179's class): duplicate writes. Does not apply: the same drain read into the
    /// sink streams.
    #[test]
    fn a2a_row_op_end_is_never_streamed() {
        let streamed = |v| with(v, |gx| gx.spooled_drain(1).is_some_and(|(d, jd)| gx.drain_streams(1, d, jd)));
        for v in [W::Plain, W::Delete, W::Nest(Am::Heap), W::Rc2Probe] {
            assert!(with(v, |gx| gx.spooled_drain(1).is_some() && !gx.drain_streams(1, 2, 0)), "{v:?}: spooled, not streamed");
        }
        assert_eq!(verdict(W::Limit), (Some(3), None, None, Some(TAILSTOP)), "a LIMIT before the write");
        for v in [W::Plain, W::Delete, W::Nest(Am::Heap), W::Rc2Probe, W::Limit] {
            assert!(!streamed(v), "{v:?}");
        }
        assert!(with(W::Read, |gx| gx.a2a_write(2).is_none() && gx.drain_streams(1, 2, 0)), "the sink streams");
    }
}
