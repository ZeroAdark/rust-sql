//! V4R-536 (W2-DRAIN-A1-MRULE row A1a; design drain-a1 r2): join.drain's spooled form's
//! refusals. Where join/drain.rs refuses a drain by the `m` rule (a tail op the probe cannot run as its own tail), the
//! spooled form (row A1b: the probe ends in a spool of its joined rows; the drain pipeline reads them, then the drained
//! rows, through its ops and end made once) runs every probe effect before any tail effect and hands the drained rows on
//! in record order, where PostgreSQL interleaves them per probe row and drains in bucket order. `spool_refusal` names the
//! first reason PostgreSQL 18.6 could tell, in the design's order; A1a refuses by that name and keeps the `m` rule for the
//! rest. lowerv4 holds no PlannedStmt, so F5's coverage (decompose rescan.rs) is `acts`: a walk of every expression,
//! source and op a pipeline runs, a SubPlan's and a nest's own pipelines included. A helper file (no rows). Owner M7-DRAIN.
use gravity_abi::breaker::BreakerKind;
use gravity_decompose::graph::{slot_has, Source};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::subplan::SubForm;
use gravity_decompose::{Op, PipelineNode};
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::jointype::JoinType;
use types_nodes::Node;

use crate::face::kinds::KindId;
use crate::graph::{EndKind, Gx};
use crate::effects::op_raise_free;
use crate::effects::visible::drain::{graph_acts, order_free, order_free_as, pipe_acts, quiet, subplans, user_end, user_index, SEEN};
use crate::ops::hashjoin::drain::lower::drain_of;

pub(crate) const N1: &str = "a spooled drain whose probe or a pipeline before the drain can show it ran (D2129)";
pub(crate) const N3: &str = "a spooled drain inside a rerun set (D2130)";
pub(crate) const N5: &str = "a write or row lock in a statement with a spooled drain (D2131)";
pub(crate) const ARMS: &str = "a spooled drain that is an arm of a union or reads one (D2132)";
pub(crate) const USER: &str = "a spooled drain whose probe, tail or end may call a user support function (D2133)";
pub(crate) const TAILSTOP: &str = "a spooled drain whose tail may raise under a LIMIT or a sink a cursor may stop (D2135)";
pub(crate) const A2END: &str = "a spooled drain under an end the spooled form does not take (D2136)";

impl Gx<'_, '_> {
    /// Why drain `d` (its Drain op `jd`) of probe `k` cannot be spooled, in the design's order, or `None`. A sink is read as
    /// one its reader takes whole (D1922's portal-kind fact): the form then holds `spool_whole_read`'s mark.
    pub(crate) fn spool_refusal(&self, k: usize, d: usize, jd: usize) -> Option<&'static str> {
        self.spool_refusal_as(k, d, jd, true)
    }

    /// D1922 (the portal-kind fact): whether the spooled form of drain `d` holds only for a reader that takes every row: it
    /// is refused when a sink is read as one a cursor may stop (N1's D393 quiet, TAILSTOP), so `end` marks it
    /// (`Px::whole_read`) and the entry delegates it under a reader that may stop early.
    pub(crate) fn spool_whole_read(&self, k: usize, d: usize, jd: usize) -> bool {
        self.spool_refusal_as(k, d, jd, false).is_some()
    }

    /// D1922 (D3377, the spooled half of the portal-kind fact): drain `d` (its Drain op `jd`) of probe `k` streams its
    /// joined phase exactly under a partial reader (decompose `PipelineGraph::partial`: a named cursor's FETCH, a
    /// counted portal), so the spooled form needs no whole-read mark there (drain/lower.rs `end`). A RIGHT, FULL or
    /// RIGHT SEMI drain ends in the statement's sink with no LIMIT in its tail and the streamed form's shape holds
    /// (spool_stream.rs `stream_shape`): its probe is a pulled set the drain pulls a joined row at a time where N1 does
    /// not find it quiet (spool_stream.rs `pull`), so it runs no probe row past the joined row the reader takes, as
    /// PostgreSQL 18.6's Hash Right / Full Join pulls its outer only as its parent asks (nodeHashjoin.c:421); a RIGHT
    /// SEMI drain's probe then hands each build row on at its first match into the spool (drain/lower.rs `semi_hands`),
    /// as its Hash Right Semi Join returns it. Every pipeline between the probe and the drain is one N1 passes under a
    /// stop (a quiet probe, a tail SubPlan's or nest's own set) or a scan that neither raises nor acts
    /// (`between_still`). The tail runs row by row as the reader pulls: the joined rows in PostgreSQL's order, the
    /// drained records after the probe's end in record order where PostgreSQL goes bucket by bucket, which DECISIONS 5
    /// (a) leaves free under a FETCH (rows with no ORDER BY may be decided against an error). So N1's and TAILSTOP's
    /// halves for a sink a cursor may stop do not apply.
    pub(crate) fn drain_exact(&self, k: usize, d: usize, jd: usize) -> bool {
        let pd = &self.pipelines()[d];
        let tail = &pd.ops[jd + 1..];
        let right = matches!(pd.ops.get(jd), Some(Op::Join(JoinDesc::Drain { jointype: JoinType::JOIN_RIGHT | JoinType::JOIN_FULL | JoinType::JOIN_RIGHT_SEMI, .. })));
        if !self.graph.partial || !right || self.end_kind(d) != EndKind::Sink || crate::frame::iface::holds_limit(tail) || !self.stream_shape(k, d, jd) {
            return false;
        }
        SEEN.with(|c| c.set(0));
        (k + 1..d).all(|q| exempt(self, q, tail) || quiet(self, q) || between_still(self, q))
    }

    /// N5's write that shows nothing of spooled drain `d`: its write op's index in pipeline `q` when one of the LICENSED
    /// WRITE-BESIDE-DRAIN FORMS below holds (ONE list: a new form is a new entry here, with its condition and its
    /// witness), else none.
    ///
    /// The terms. A data-modifying CTE counts unless it is STILL (D3824 (a), `still_cte`: a read-only CTE whose pipelines
    /// write, lock and act in nothing; it runs whole before its first reader in every form Gravity has, so its run shows
    /// nothing of the spool's order, and PostgreSQL's lazy pull of it shows nothing either). A data-modifying CTE is LATE
    /// when it runs after the statement's emit and never at a recheck (rerun/epq/marks.rs `after_emit`: read by nothing,
    /// drained after the emit or run by post.rs), or (D3379 CTE-BUILD) `d`'s build alone reads it (`built_whole`: its
    /// ModifyTable has run whole before the join hands its first row on). Row A2a's kind (`a2a_write`): an INSTEAD OF view
    /// write, a plain heap INSERT, an insert-only MERGE, reading no position (no drain's target scan or build records): it
    /// never rechecks, and ExecIR*Triggers / ExecInsert take each row as it comes, whatever a spooled drain under it did
    /// first (PostgreSQL's drained join has run its probe whole before it hands a drained row on, as the spool does).
    ///
    /// - L1 OWN-A2A (triggers#265): `d`'s own write, of row A2a's kind. Witness e2e/gravity/v4/drain_d2131_root_write.sql.
    /// - L2 ROOT-A2A (D2131, triggers#265): the statement's own write in the root pipeline, of row A2a's kind, its rows no
    ///   MERGE's, RIGHT drain's or hashed union's build rows, every counted CTE LATE. Witness
    ///   e2e/gravity/v4/drain_d2131_root_write.sql.
    /// - L3 CTE-POST (D2131, mergecmd_cte_merge): a counted data-modifying CTE's write of row A2a's kind, every counted CTE
    ///   LATE: PostgreSQL runs such a CTE's ModifyTable at ExecPostprocessPlan, after the statement's own has taken every
    ///   row, so nothing of the spool's order reaches it. Witnesses e2e/gravity/v5/mergecmd_cte_merge.sql,
    ///   e2e/gravity/v5/rxnested_d3379_cte_build.sql (CTE-BUILD).
    /// - L4 APART (D3824, Hibernate ORM's CTE mutation strategy: `WITH id_cte AS MATERIALIZED (SELECT ..), delete_cte_a AS
    ///   (DELETE FROM a WHERE a.k IN (SELECT id FROM id_cte) RETURNING ..), delete_cte_b AS (..) SELECT count(*) FROM
    ///   id_cte`, one DELETE a Hash Right Semi Join with its target hashed, the others Hash Semi Joins): a counted
    ///   data-modifying CTE's write of any kind other than `d`'s own, run after the emit (`after_emit`), beside a drain run
    ///   after the emit too, every counted CTE LATE, neither pipeline feeding the other (px.rs `feeds`). PostgreSQL's
    ///   ExecPostprocessPlan runs each such CTE's ModifyTable whole, one after another, and Gravity runs each pipeline
    ///   whole, so the two writes never interleave with each other or with the probe (N1 keeps a probe that acts), and the
    ///   rows the other CTE writes, its recheck and its positions come from its own pipelines alone. Only the drain's own
    ///   write sees the spool's order (DECISIONS 6b). Witnesses e2e/gravity/v5/gravfix3_d3824_cte_mutation.sql, isolation
    ///   gravfix3-d3824-drain-copy-recheck.
    /// - HELD (D3800, River's JobSchedule): `d`'s own UPDATE / DELETE of the target a nest past its RIGHT Drain op scans,
    ///   rechecking in the IR over the drain's held row, each target row reached from one build record (`held_write`).
    ///   Witnesses e2e/gravity/v5/nestfast_d3800_river_hash_right.sql, isolation nestfast-d3800-river-hash-right, and
    ///   e2e/gravity/v4/nestfast_d3800_two_records_refused.sql (the two-records case it refuses).
    /// - L5 SEMI-PROBE-DRAIN (D3738, Django's update() over an m2m-annotated queryset: `UPDATE bar SET .. WHERE bar.id IN
    ///   (SELECT U0.id FROM bar U0 LEFT OUTER JOIN bar_m2m U1 ON (U0.id = U1.bar_id))`, PostgreSQL 18.6's Hash Right Semi
    ///   Join with the target hashed whose probe is a Hash Right Join): the statement's own UPDATE / DELETE in the root
    ///   pipeline, no counted CTE, writing the drained build rows of a RIGHT SEMI drain (drain/lower.rs
    ///   `drained_build_target`) whose probe pipeline is `d`, the spooled RIGHT drain's reader (`semi_probe`). The write
    ///   runs after `d` ran whole, where PostgreSQL's semi join hands each target row on at its first match while the
    ///   right join probes; every relation either join reads is read under the statement's snapshot and the target's hash
    ///   is built before the first write, so the rows written are PostgreSQL's and only their order differs (DECISIONS
    ///   6b); N1 keeps a probe that acts. Witnesses e2e/gravity/v5/modifytable_d3738_django_m2m.sql, isolation
    ///   modifytable-d3738-django-m2m.
    pub(crate) fn free_write(&self, q: usize, d: usize) -> Option<usize> {
        let ps = self.pipelines();
        // a pipeline with no write has nothing to free (row A2a's kinds all hold one)
        let j = ps[q].ops.iter().position(|o| matches!(o, Op::Write(_)))?;
        let n = ps.len();
        let ctes: PgVec<'_, usize> = (0..n).filter(|&k| self.cte_write(k) && !self.still_cte(k)).collect_in(self.mcx);
        let late_in = |after: &[usize]| ctes.iter().all(|&k| after.contains(&k) || self.built_whole(k, d));
        let late = || late_in(&self.after_emit());
        let a2a = || self.a2a_write(q);
        // L1 OWN-A2A
        let own_a2a = || q == d && a2a().is_some();
        // L2 ROOT-A2A
        let root_a2a = || q == self.graph.root as usize && self.merge_drained_target(q).is_none() && self.drained_build_target(q).is_none() && self.hashed_union_build(q).is_none() && (ctes.is_empty() || late()) && a2a().is_some();
        // L3 CTE-POST
        let cte_post = || ctes.contains(&q) && late() && a2a().is_some();
        // L4 APART
        let apart = || {
            let after = self.after_emit();
            q != d && ctes.contains(&q) && after.contains(&q) && after.contains(&d) && late_in(&after) && !crate::frame::px::feeds(self, q, d) && !crate::frame::px::feeds(self, d, q)
        };
        // L5 SEMI-PROBE-DRAIN
        let semi_probe = || q == self.graph.root as usize && ctes.is_empty() && super::lower::semi_probe(self, q) == Some(d) && a2a().is_some();
        // D3800 HELD (nestfast): or the drain's own UPDATE / DELETE over its held row (`held_write`)
        match own_a2a() || root_a2a() || cte_post() || semi_probe() {
            true => a2a(),
            false => apart().then_some(j).or_else(|| self.held_write(q).filter(|_| q == d)),
        }
    }

    /// D3800 (River's JobSchedule at 5 jobs a run over River's job table: Update -> Nested Loop (Hash Right Join (the
    /// conflicts' Hash Join, Hash of jobs_with_rownum job), the job's primary key index scan)): the op index of drain `d`'s
    /// write when it is an UPDATE / DELETE of the target a nest past its RIGHT Drain op scans, rechecking in the IR over
    /// the drain's row held (rowop_epq.rs `drain_held`: EvalPlanQual's Hash Right Join makes that row again from its test
    /// tuples), every row the drain hands on for one build record writes the same (`right_same`), and each target row is
    /// reached from at most one build record (`pinned`). PostgreSQL's Hash Right Join hands a build record on with each
    /// probe row it meets, and ModifyTable writes the target from the first row reaching it (a later one finds
    /// TM_SelfModified and is skipped); the spool hands the joined rows on after the probe ran whole, and the unmatched
    /// records in record order where PostgreSQL goes bucket by bucket, across workers at W8, so which row reaching a target
    /// comes first may differ, never what it writes: all of them are one record's. The probe
    /// reads under the statement's snapshot, which the command's own writes never reach (a scan does not see a version its
    /// own command made, and keeps seeing the one it replaced), so running it whole before the first write reads what
    /// PostgreSQL's interleaving reads; the writes' order is the plan's (DECISIONS 6b), which RETURNING and the triggers see.
    /// D3801: or, in place of `right_same`, the rows of one build record come in PostgreSQL's order (`right_ordered`), so
    /// the first of them reaching a target row is PostgreSQL's whatever it holds.
    pub(crate) fn held_write(&self, d: usize) -> Option<usize> {
        let ops = &self.pipelines().get(d)?.ops;
        let wj = ops.iter().position(|o| matches!(o, Op::Write(_)))?;
        let jd = ops.iter().position(|o| matches!(o, Op::Join(JoinDesc::Drain { jointype: JoinType::JOIN_RIGHT, .. })))?;
        let same = right_same(self, d, jd) || right_ordered(self, d, jd);
        (jd < wj && same && pinned(self, d, jd, wj) && crate::frame::iface::drain_held(self, d)).then_some(wj)
    }

    /// D3800: pipeline `q` ran to its end before drain `d`'s build did: down the sources from the build's pipeline, each
    /// pipeline with no op that stops it early (a LIMIT, a gate) read its source to the end. PostgreSQL's Hash Right / Full
    /// Join builds before it fetches an outer row (nodeHashjoin.c: no empty-outer skip when the join fills the inner) and
    /// MultiExecHash reads its child to the end, so such a pipeline's effects (River's claim, the FOR UPDATE of the CTE
    /// jobs_with_rownum's Sort reads) all came before the join's first row, as they do before the spool's.
    fn built_before(&self, q: usize, d: usize) -> bool {
        let ps = self.pipelines();
        let producer = |x: usize| match ps[x].source {
            Source::Breaker { slot } => self.life_of(x, slot).map(|l| usize::from(l.0)).filter(|&p| p < x && ps[p].desc.produces() == Some(slot)),
            _ => None,
        };
        let stops = |x: usize| ps[x].ops.iter().any(|o| matches!(o, Op::Gate(_)) || crate::frame::iface::holds_limit(core::slice::from_ref(o)));
        let mut x = producer(d);
        while let Some(b) = x {
            if b == q {
                return true;
            }
            x = if stops(b) { None } else { producer(b) };
        }
        false
    }

    /// D3824: CTE producer `k` is read-only and acts in nothing: it and every graph pipeline feeding it (px.rs `feeds`: the
    /// slots each consumes, its breaker source, a union's feeds and arms) hold no write, row lock or nested write
    /// (`writes`), and no source, op, aggregate or group key that acts or calls a user function (`graph_acts`, `user_end`).
    /// The walk keeps the caller's SEEN count (N1 walks after it under the same bound).
    fn still_cte(&self, k: usize) -> bool {
        let ps = self.pipelines();
        let seen = SEEN.with(|c| c.get());
        SEEN.with(|c| c.set(0));
        let still = (0..=k).filter(|&x| crate::frame::px::feeds(self, k, x)).all(|x| !writes(&ps[x], None) && !graph_acts(self, x, 0) && !user_end(self, x));
        SEEN.with(|c| c.set(seen));
        still
    }

    /// D3379 (CTE-BUILD): data-modifying CTE `c`, a plain heap INSERT of row A2a's kind (`a2a_write`) into a relation no
    /// other write of the statement writes, is read by one pipeline alone: the build whose rows spooled drain `d`'s RIGHT /
    /// FULL Drain op hands on (the table the probe that fills the drain's spool reads last), over quiet filters, projections
    /// and keys.
    /// PostgreSQL's Hash Right / Full Join builds before it fetches an outer row (nodeHashjoin.c: no empty-outer skip when
    /// the join fills the inner) and MultiExecHash reads the CTE Scan to its end, so the CTE's ModifyTable has run whole
    /// before the join hands a row on; the spool's build reads the CTE's store whole before the probe as well. Its rows,
    /// triggers and RETURNING list come in the CTE's own order in both, before anything of the spool's order.
    fn built_whole(&self, c: usize, d: usize) -> bool {
        let ps = self.pipelines();
        let Some(pd) = ps.get(d) else { return false };
        let Some(m) = pd.ops.iter().find_map(|o| match o {
            Op::Join(JoinDesc::Drain { matches, jointype: JoinType::JOIN_RIGHT | JoinType::JOIN_FULL }) => Some(*matches),
            _ => None,
        }) else {
            return false;
        };
        let (Some(j), Some(s)) = (self.a2a_write(c), ps[c].desc.produces()) else { return false };
        // the Drain op reads the probe's spool: the build is the producer of the table the probe's last Probe op reads
        let Some(k) = (0..d).rev().find(|&q| ps[q].desc.produces() == Some(m)) else { return false };
        let probed = ps[k].ops.iter().rev().find_map(|o| if let Op::Join(JoinDesc::Probe { slot, .. }) = o { Some(*slot) } else { None });
        let Some(b) = probed.and_then(|h| (0..k).rev().find(|&q| ps[q].desc.produces() == Some(h))) else { return false };
        let readers = (0..ps.len()).filter(|&q| q != c && ps[q].desc.consumes.contains(&s));
        // the build's own work between two CTE rows (its filters, projections and keys) raises nothing and calls nothing,
        // so running it after the CTE's whole write shows nothing PostgreSQL's interleaving does not
        let still = |n: Node<'_>| crate::effects::expr_quiet(self, n);
        let build = ps[b].ops.split_last().is_some_and(|(last, body)| {
            matches!(last, Op::Join(JoinDesc::Build { keys, .. }) if keys.iter().all(still))
                && body.iter().all(|o| match o {
                    Op::Filter(n) => still(*n),
                    Op::Project(l) => l.iter().all(still),
                    _ => false,
                })
        });
        let reused = (c + 1..ps.len()).any(|q| q != b && ps[q].desc.produces() == Some(s));
        let Op::Write(w) = &ps[c].ops[j] else { return false };
        let rel = w.plan.target_rel;
        let shared = ps.iter().enumerate().any(|(q, p)| {
            q != c && p.ops.iter().any(|o| matches!(o, Op::Write(x) if x.cmd != gravity_program::Cmd::Select && (x.plan.target_rel == rel || x.targets.iter().any(|t| t.relid == rel))))
        });
        readers.eq([b]) && b > c && build && !reused && !shared
    }

    /// lockrows lk-0100 (D2131): N5's row lock that shows nothing of spooled drain `d`: a LockRows straight over `d`'s
    /// RIGHT SEMI Drain op (filters and projections between, only projections after, a sink end). ExecLockRows locks each
    /// build row the join hands on at its first match, in an order PostgreSQL's plan decides (DECISIONS 6b); the spool
    /// hands the same rows on after the probe ran whole under the same snapshot, and a recheck reruns the producers under
    /// its own copy of the words (exec rerun/epq.rs `own_words`, A4-MATEND). A probe that may raise or act keeps N1.
    /// D3379 RDRAIN (m7_epq_joins: `es LEFT JOIN eb .. FOR UPDATE OF es`, a Hash Right Join with es hashed): or over a
    /// RIGHT Drain op. FOR UPDATE never reaches the nullable side, so the lock is on the build's relation and the probe's
    /// is a REFERENCE mark; ExecLockRows locks each joined row as the join hands it on, then each unmatched build row
    /// null-extended, which the spool hands on in the same order after the probe ran whole (the drained records in record
    /// order where PostgreSQL goes bucket by bucket: DECISIONS 6b). The recheck reruns the build over the newest version
    /// and the probe over the REFERENCE row (none for a null-extended row's NULL ctid: rerun/epq/marks.rs `sites`, D2048),
    /// then this pipeline: the joined row when the join qual still holds, else the newest version null-extended, as
    /// EvalPlanQual's Hash Right Join over its test tuples.
    /// D3846 (m8_drain_spool_names: a SubPlan in the join's target list, the `m` rule's spooled form, under `FOR UPDATE OF
    /// b`): or with SubPlan steps between the Drain op and the lock. PostgreSQL computes the join's target list, the SubPlan
    /// included, below LockRows, once per row the join hands on, and EvalPlanQual computes it again over the recheck's row;
    /// the tail runs each SubPlan step once per spooled or drained row before the lock, and the recheck reruns this
    /// pipeline's steps, the SubPlan's among them (rerun/epq/epq_ir.rs `Redo::Sub`), over the joined or null-extended
    /// row. Only the order of the rows differs, which the names after N5 judge as for any spooled tail: a SubPlan that
    /// may raise under a stop keeps TAILSTOP, one calling a user support function keeps USER.
    pub(crate) fn free_lock(&self, q: usize, d: usize) -> Option<usize> {
        // D3800: or the one row lock of a pipeline that writes nothing and ran to its end before the drain's build did
        // (`built_before`), the drain's write held (`held_write`)
        let qo = &self.pipelines().get(q)?.ops;
        let mut locks = qo.iter().enumerate().filter(|(_, o)| matches!(o, Op::Lock(_) | Op::Write(_)));
        if let (Some((l, Op::Lock(_))), None) = (locks.next(), locks.next()) {
            if q != d && self.held_write(d).is_some() && self.built_before(q, d) {
                return Some(l);
            }
        }
        let ops = &self.pipelines().get(d)?.ops;
        let jd = ops.iter().position(|o| matches!(o, Op::Join(JoinDesc::Drain { jointype: JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT, .. })))?;
        let j = ops.iter().position(|o| matches!(o, Op::Lock(_))).filter(|&j| q == d && jd < j)?;
        let quiet = ops[jd + 1..j].iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_) | Op::SubPlan(_)));
        let above = ops[j + 1..].iter().all(|o| matches!(o, Op::Project(_)));
        (quiet && above && self.end_kind(d) == EndKind::Sink).then_some(j)
    }

    /// `spool_refusal`, with a sink read whole (`sink_whole`) or as one a cursor may stop.
    fn spool_refusal_as(&self, k: usize, d: usize, jd: usize, sink_whole: bool) -> Option<&'static str> {
        let ps = self.pipelines();
        let (pk, pd) = (&ps[k], &ps[d]);
        let tail = &pd.ops[jd + 1..];
        SEEN.with(|c| c.set(0));
        // no statement-wide name: the no-widen list's forms (D2178 / D2179, D2180, D2194 / D2147 / D2201, D2197) and a LIMIT
        // that may be zero, a bound that may be negative or a one-time gate (D1921) are judged by the names below
        // A RIGHT SEMI drain takes the names a RIGHT one does (D1922): PG hands each build row on at its first match, as a
        // RIGHT join its joined rows, while the spool runs the probe whole first; N1 under a stop, TAILSTOP and a
        // sink's reader (`spool_whole_read`) read that difference the same way
        let Some(m) = crate::frame::iface::breaker_of(&pk.desc) else { return Some(A2END) };
        if self.set_refused(k, d, jd, m.slot) {
            return Some(N3);
        }
        if ps.iter().enumerate().any(|(q, p)| writes(p, self.free_write(q, d).or_else(|| self.free_lock(q, d)))) {
            return Some(N5);
        }
        // writesubplan (merge#368 group): but the drained-arms MERGE (drain/lower.rs `merge_drained_arms`), whose arms' one
        // Append op is their bare marker and whose tail holds none, fills one spool in arm order (spool.rs)
        // drain (regress updatable_views#865): or any other drain whose arms are plain (drain/lower.rs `plain_drained_arms`)
        let arms_merge = self.drained_arms(k).is_some_and(|r| self.merge_drained_arms(d).is_some() || self.plain_drained_arms(r, d)) && !tail.iter().any(|o| matches!(o, Op::Append(_)));
        if !arms_merge && (self.drained_arms(k).is_some() || pk.ops.iter().any(|o| matches!(o, Op::Append(_))) || tail.iter().any(|o| matches!(o, Op::Append(_)) && !crate::ops::hashjoin::drain::spool_arms::arm_whole(self, d, o, tail))) {
            return Some(ARMS);
        }
        // N1 where nothing stops the read: with no LIMIT in the tail and a sort, top-N or fold end (each takes every row
        // before it hands one on), or a sink its reader takes whole (`sink_whole`, D1922), PostgreSQL runs every probe row
        // and every pipeline between, as the spool does, so only a call that acts can show their order: the probe (its
        // last op the probe, its end the spool) and each pipeline between (its end's keys and aggregates too) call none.
        // Under a stop, D393's quiet as before.
        let whole = every_row(self, d, jd, sink_whole);
        let calls_none = |q: usize| match q == k {
            true => matches!(pk.ops.last(), Some(Op::Join(JoinDesc::Probe { .. }))) && m.kind == BreakerKind::Materialize && !pipe_acts(self, pk, k, 0),
            false => !graph_acts(self, q, 0) && !user_end(self, q),
        };
        // under a stop too: a probe over a scan or a breaker (a drain under a drain: its Drain op hands records on) whose every
        // op raises nothing (`op_raise_free`: a nest or SubPlan by its F5 facts) and calls nothing that acts shows nothing of
        // how far PostgreSQL ran it
        let subs = subplans(&pk.ops, self.mcx);
        let src = matches!(pk.source, Source::Range { .. } | Source::Breaker { .. });
        let still = src && calls_none(k) && pk.ops.iter().all(|o| matches!(o, Op::Join(JoinDesc::Drain { .. })) || op_raise_free(self, o, Some(&subs[..])));
        // a RIGHT ANTI drain into a sink (`anti_whole`): PostgreSQL's first fetch runs every probe row, as the spool does, so
        // only a call that acts can show the probe's order
        let anti = anti_whole(self, d, jd) && calls_none(k);
        // D3853: and under a stop, a pipeline between that fills a tail join's build or plan Material from a scan and
        // neither raises nor acts (`between_still`, as `drain_exact` takes it) shows nothing of whether PostgreSQL ran it
        if !(k..d).all(|q| (q > k && (exempt(self, q, tail) || between_still(self, q))) || quiet(self, q) || (q == k && (still || anti)) || (whole && calls_none(q))) {
            return Some(N1);
        }
        if (k..=d).any(|q| self.user_drain_arms(q, q)) || tail.iter().any(|o| user_index(self, o, 0)) || user_end(self, d) {
            return Some(USER);
        }
        // D2134 (TAILVOL) is gone (a VOLATILE or user call's count is free, at least once per row
        // whose value is used, never cached across rows; its order was free already, DECISIONS 6b): a tail or end that calls
        // what acts runs once per row that reaches it, as PostgreSQL's does, only in the spool's order (the drained records
        // in record order, PG's in bucket order) and, under a stop, on the rows that order reaches first. What may raise
        // there under a LIMIT is free too (DECISIONS 5 (a): an unordered LIMIT may decide rows against an error; D2135 below
        // keeps TAILSTOP for a sink a cursor may stop, a CTE store, a tail that writes and one that opens an inner set); an
        // end whose value reads the order stays A2END's.
        let subs = subplans(&pd.ops, self.mcx);
        // D3637: an end order-free but for a money or interval sum or average passes here; its lowering takes
        // those calls only under their licence (drain/lower.rs `end`, `effects::drain_sums`), else names A2END there
        // D2136: a sink read whole (`sink_whole`) takes a drained table's readers' rows through ops that may raise
        if !order_free_as(self, d, true, sink_whole) {
            return Some(A2END);
        }
        // writesubplan (heldout ald_joindrain-0014): a CTE's store as a drain's spool: its readers' pulls may stop it early
        let under = self.end_kind(d) == EndKind::Breaker(BreakerKind::Materialize) && matches!(self.spool_kind(d), KindId::JoinDrain | KindId::NestCte);
        // D2135 (D3377): a LIMIT in the tail stops the drain pipeline (serial: its source the spool) at its count, row by row:
        // the joined rows reach the tail in the probe's order (PostgreSQL's; N1 above keeps a probe run whole quiet), the
        // drained records in record order where PostgreSQL goes bucket by bucket. Which rows reach a tail that may raise
        // before the LIMIT stops is then decided only by a row order with no ORDER BY, which DECISIONS 5 (a) leaves free
        // (rows may be decided against an error). A tail that writes or locks keeps the name: its row operation is no raise
        // only, and a streamed write under a parked pull re-reads (D2179's class: lower.rs `a2a_row_op_end_is_never_streamed`).
        // A sink a cursor may stop, or a CTE store whose readers may stop, keeps it too: those reads may run past the
        // reader's stop, not only in another order.
        let acts = tail.iter().any(|o| matches!(o, Op::Write(_) | Op::Lock(_)));
        let stops = (crate::frame::iface::holds_limit(tail) && acts) || (self.end_kind(d) == EndKind::Sink && !sink_whole);
        let stops = stops || (under && !whole);
        (stops && !tail.iter().all(|o| op_raise_free(self, o, Some(&subs[..])))).then_some(TAILSTOP)
    }

    /// N3 (D2130), narrowed by D3724: whether a rerun set touches spooled drain `d` (its Drain op `jd`) of probe `k`, whose
    /// spool is slot `m`, where its run would not re-run the spool with its reader. Served: every set holding the probe or
    /// the drain holds both (the probe runs first, in program order) and holds the spool's slot only with them; the form
    /// unstreamed (`drain_streams`: no probe pulled inside a set's run); one probe (no drained arms); and no write or row
    /// lock in the statement (no recheck re-runs the set's spool under its own words: exec rerun/epq.rs). The fill's open
    /// then empties the spool at every run (spool.rs `DrainFill::rewind`, as INNER-SPOOL's), and the reader's open re-reads
    /// it from its first record and re-opens the drain over the build's unmarked records, which the run laid out again (a
    /// member build) or the marking probe cleared (a build kept outside the set: D2592, probe_marks.rs). PostgreSQL 18.6's
    /// ExecReScanHashJoin does the same: the outer scanned again over a table rebuilt, or kept with its match flags reset.
    /// The monitoring join of pg_stat_user_tables with pg_statio_user_tables plans such a Hash Right Join (pg_index probing
    /// pg_class) under the views' GROUP BY, which an unparameterised nest loop re-runs (D3724).
    fn set_refused(&self, k: usize, d: usize, jd: usize, m: u8) -> bool {
        let sets = &self.graph.reruns;
        let has = |r: &gravity_decompose::graph::Rerun, q: usize| r.pipelines.iter().any(|&p| p as usize == q);
        if !sets.iter().any(|r| has(r, k) || has(r, d) || self.leases_spool(r, k, d, m)) {
            return false;
        }
        let together = sets.iter().all(|r| has(r, k) == has(r, d) && (has(r, k) || !self.leases_spool(r, k, d, m)));
        !together || self.drained_arms(k).is_some() || self.drain_streams(k, d, jd) || self.pipelines().iter().any(|p| writes(p, None))
    }

    /// D3834: whether rerun set `r` leases spool slot `m` (probe `k`'s, read by drain `d`) in the spool's own life: a member
    /// runs while that life holds the slot (from its first fill, the probe or the first drained arm, to the drain's read),
    /// or reads that life later. A set's slots are slot ids, and a breaker made after the drain's read may reuse the id (a
    /// set declared then leases its own breaker's slot from that point on: breakers::leased), so the id alone names no
    /// touch. Hibernate's entity-graph fetch (a Hash Right Join's drain under a merge join's Materialize, then a nest whose
    /// parameterised inner union fills the same slot id) refused N3 by the id alone. A life no reader names: leased.
    fn leases_spool(&self, r: &gravity_decompose::graph::Rerun, k: usize, d: usize, m: u8) -> bool {
        if !slot_has(&r.slots, m) {
            return false;
        }
        let Some(life) = self.life_of(d, m) else { return true };
        let ps = self.pipelines();
        let lo = self.drained_arms(k).map_or(k, |(f, _)| f).min(life.0 as usize);
        let reads = |q: usize| ps[q].desc.consumes.contains(&m) || matches!(ps[q].source, Source::Breaker { slot } if slot == m);
        r.pipelines.iter().map(|&p| p as usize).any(|q| (lo..=d).contains(&q) || (reads(q) && self.life_of(q, m) == Some(life)))
    }

    /// INNER-SPOOL: why probe `k`'s RIGHT / FULL drain `d` (its Drain op `jd`), a nest's embedded inner, cannot take the
    /// spooled form (drain/lower.rs `end`), or `None`. The spool runs the probe's whole outer before the inner hands its
    /// first joined row on, where PG interleaves them, and hands the drained records on in record order, where PG goes
    /// bucket by bucket (DECISIONS 6b: an order PG's plan decides is free). So, beside the statement-wide names the spooled
    /// form keeps (N5, ARMS but for a streamed union's plain arms): N1 unless the probe (each arm) is quiet (D393's
    /// quiet: no op before it, key or qual that raises or calls a user or VOLATILE function, reading no slot but its
    /// build), so its run shows nothing; USER over the inner's ops past the Drain op, and A2END where the nest's owner's
    /// end (the graph pipeline that runs the inner: its continuation past the Nest op, its end) may show the order
    /// (`order_free`). A call that acts in the inner's tail or the owner runs once per row that reaches it, as PostgreSQL's
    /// does, in the spool's order: free (D2134 gone: call counts; DECISIONS 6b: order). N3 does not apply: a set that re-runs the probe re-opens the
    /// inner after it, and the fill's open notes where the run's records begin (`DrainFill::open`), which the inner's open
    /// reads.
    pub(crate) fn inner_spool_refusal(&self, k: usize, d: usize, jd: usize) -> Option<&'static str> {
        self.inner_spool_refusal_as(k, d, jd, true)
    }

    /// D4096 (c-random rnd-0124: the drain a nest's embedded inner, the nest's owner ending in the next Hash Join's build,
    /// whose reader projects what may raise into the statement's sink): whether the spooled inner form holds only for a
    /// reader that takes every row, as `spool_whole_read` asks it of a graph drain. drain/lower.rs `end` then serves it to
    /// such a reader alone (`Px::read_whole`).
    pub(crate) fn inner_spool_whole_read(&self, k: usize, d: usize, jd: usize) -> bool {
        self.inner_spool_refusal_as(k, d, jd, false).is_some()
    }

    /// `inner_spool_refusal`, with the statement's sink read whole (`sink_whole`) or as one a cursor may stop: A2END's
    /// `order_free_as` takes it as the graph drain's `spool_refusal_as` does.
    fn inner_spool_refusal_as(&self, k: usize, d: usize, jd: usize, sink_whole: bool) -> Option<&'static str> {
        let ps = self.pipelines();
        let (pk, pd) = (&ps[k], &ps[d]);
        let tail = &pd.ops[jd + 1..];
        SEEN.with(|c| c.set(0));
        if self.end_kind(k) != EndKind::Breaker(BreakerKind::Materialize) {
            return Some(A2END);
        }
        if ps.iter().any(|p| writes(p, None)) {
            return Some(N5);
        }
        // D3915: a streamed union's plain arms (drain/lower.rs `plain_drained_arms`: each its one bare marker, no pruning
        // step) fill the one spool in arm order, a tail with no Append of its own reading it; each arm is judged below as a
        // lone probe is (drain/lower.rs `end` asks for every arm)
        let run = self.drained_arms(k);
        let arms = run.is_some_and(|r| self.plain_drained_arms(r, d));
        if !arms && (run.is_some() || pk.ops.iter().any(|o| matches!(o, Op::Append(_)))) || tail.iter().any(|o| matches!(o, Op::Append(_))) {
            return Some(ARMS);
        }
        // V4R-533: a LIMIT in the tail over the arms keeps the name drain/lower.rs `end` gives it for a graph drain
        if run.is_some() && crate::frame::iface::holds_limit(tail) {
            return Some(crate::ops::hashjoin::drain::lower::LIMIT_ARMS);
        }
        // the first arm's open empties the spool (spool.rs `rewind`) and every later arm appends, so a rerun set that re-runs
        // one arm re-runs them all (N3's name otherwise: a set re-running a later arm alone would append to the last run's rows)
        if let Some((f, l)) = run {
            let has = |r: &gravity_decompose::graph::Rerun, q: usize| r.pipelines.iter().any(|&p| p as usize == q);
            if self.graph.reruns.iter().any(|r| (f..=l).any(|q| has(r, q)) && !(f..=l).all(|q| has(r, q))) {
                return Some(N3);
            }
        }
        if !quiet(self, k) && !probe_side_safe(self, k) {
            return Some(N1);
        }
        if self.user_drain_arms(k, k) || tail.iter().any(|o| user_index(self, o, 0)) || user_end(self, d) {
            return Some(USER);
        }
        // the graph pipeline that runs the inner (a nested inner's owner's owner, up to the graph)
        let inners = ps.inners();
        let mut owner = d;
        while owner >= ps.len() {
            owner = inners.get(owner - ps.len())?.0 .0;
        }
        (!order_free_as(self, owner, false, sink_whole)).then_some(A2END)
    }

    /// D4063: whether probe `k`'s RIGHT SEMI build shows nothing when it runs before PostgreSQL's would. A Hash Right
    /// Semi Join is no HJ_FILL_INNER join: ExecHashJoin fetches its outer's first row before it builds, so a probe side
    /// with no row never builds. The spooled inner form builds at the nest's first outer row or in pipeline order
    /// (lazy.rs `drain_wait`), so it takes the drain only when every pipeline writing the build's slot in this life
    /// reads as PG-quiet with its feeders and keys (`side_by`, the reading `drain_wait` makes), or when
    /// `SEMI_LOUD_BUILD` accepts the earlier build.
    pub(crate) fn semi_build_unseen(&self, k: usize) -> bool {
        use crate::ops::hashjoin::build_class::{side_by, Side};
        let ps = self.pipelines();
        let Some(Op::Join(JoinDesc::Probe { slot, jointype: JoinType::JOIN_RIGHT_SEMI, .. })) = ps[k].ops.last() else { return false };
        let Some(kb) = self.life_of(k, *slot).map(|l| l.0 as usize).filter(|&b| b < k) else { return false };
        let writes = |q: usize| ps[q].desc.produces() == Some(*slot);
        let reads = |q: usize| ps[q].desc.consumes.contains(slot) || matches!(ps[q].source, Source::Breaker { slot: s } if s == *slot);
        let from = (0..kb).rev().find(|&q| !writes(q) && reads(q)).map_or(0, |q| q + 1);
        SEMI_LOUD_BUILD || (from..=kb).filter(|&q| writes(q)).all(|q| side_by(self, q, usize::MAX, false) == Side::Safe)
    }
}

/// D4063: whether the spooled inner form takes a RIGHT SEMI drain whose build may raise or call (`semi_build_unseen`): off.
/// Its build would run before PostgreSQL's, which never builds under a probe side with no row, so an error PostgreSQL
/// never reaches could end the statement.
const SEMI_LOUD_BUILD: bool = false;

/// D1922 (D3377, `Gx::drain_exact`): pipeline `q` between a probe and its drain scans a relation or reads a breaker
/// into a plan Material (its Spool op moves rows) or a hash build (its Build op's keys raise on no row), and none of its
/// ops raises (`op_raise_free`) or calls what acts (`graph_acts`): N1's `still`, for a pipeline that is not the probe.
fn between_still(gx: &Gx<'_, '_>, q: usize) -> bool {
    let p = &gx.pipelines()[q];
    let subs = subplans(&p.ops, gx.mcx);
    let end = matches!(crate::frame::iface::end_breaker(gx, q), Some(BreakerKind::Materialize | BreakerKind::HashBuild));
    let still = |o: &Op<'_>| match o {
        Op::Join(JoinDesc::Spool) => true,
        Op::Join(JoinDesc::Build { keys, .. }) => keys.iter().all(|n| crate::effects::raise_free(gx, n)),
        _ => op_raise_free(gx, o, Some(&subs[..])),
    };
    end && matches!(p.source, Source::Range { .. } | Source::Breaker { .. }) && !graph_acts(gx, q, 0) && !user_end(gx, q) && p.ops.iter().all(still)
}

/// N5: a write or row lock anywhere, a nest's or nested SubPlan's inner included, but the drain's own row A2a write `own`.
fn writes(p: &PipelineNode<'_>, own: Option<usize>) -> bool {
    crate::stack_check(); // D3832
    p.ops.iter().enumerate().any(|(j, o)| Some(j) != own && match o {
        Op::Write(_) | Op::Lock(_) => true,
        Op::Join(JoinDesc::Nest { inner, .. }) => writes(inner, None),
        Op::SubPlan(s) => matches!(&s.form, SubForm::Nested { inner } if writes(inner, None)),
        _ => false,
    })
}

/// INNER-SPOOL (`inner_spool_refusal`'s N1, beside `quiet`): probe `q` of a nest inner's RIGHT / FULL drain whose side
/// D1902's reading finds Safe, where D393's quiet reads only scans (join#415 / join#416: a Function Scan of unnest over the
/// nest's parameter probing a kept VALUES build, re-run by the nest's set at every re-open). No reading of its own: the
/// probe its last op, its spool the end, it reads no slot but the builds it probes, nothing in it acts (`pipe_acts`), its
/// side through the probe Safe (build_class.rs `side_by`: its ops before the probe, its source and feeders, the probe's
/// outer hash keys; a PARAM_EXEC a nest or per-row SubPlan binds read as a bound word, a FROM function the effects home
/// reads quiet), and the probe's join qual passing the effects primitives under the same bound words (`raise_free_by`, no
/// user or VOLATILE call): its run raises nothing and shows nothing, so running its whole outer before the inner's first
/// row is PG's interleaving.
/// D3770 (SQLAlchemy's enum load with a schema: the Hash Right Join under the nest probes with a GroupAggregate's groups):
/// the probe may read its own breaker source too when every pipeline feeding that source acts in nothing (`graph_acts`:
/// ops, sort keys, aggregates) and `side_by` (which reads the source's feeders) finds them Safe: the source is whole before
/// the probe's first row in both plans (PG's GroupAggregate hands groups on as the probe pulls, over a Sort that read its
/// input whole), so the feeders' calls are the same calls, raising nothing and showing nothing of when they ran.
/// D3770 (b) (its array form: `array_agg(enumlabel::text ORDER BY enumsortorder)`): over a group's breaker source, an Aggref
/// in the probe's ops before it reads its group's final, the transitions having run in the feeder (`side_by` reads the
/// feeders whole). `side_by` reads it as a fold and finds a collecting one loud; read as topn.rs reads a group's row
/// (`quiet_entry`, D449: a final whose class reads its slots is quiet: counts, sums, extrema, collections), with the
/// feeders by `side_by` from op 0 and the probe's outer keys raise-free, the side is Safe too.
/// D4090 (c-random rnd-1000 over pgrcolumnar copies: a nest whose inner is a Hash Right Join over another Hash Right
/// Join): the probe may be a lower RIGHT / FULL drain's pipeline, its source that join's spool and its own Drain op reading
/// that join's build for the match bits; that build slot is read as the source is (its feeders act in nothing, and
/// `side_by`, which reads every slot the probe consumes, finds them Safe): the drain hands on the build's unmatched rows
/// and evaluates nothing, so the lower join runs whole before the probe's first row in both plans.
fn probe_side_safe(gx: &Gx<'_, '_>, q: usize) -> bool {
    use crate::ops::hashjoin::build_class::{feed, side_by, Side};
    let p = &gx.pipelines()[q];
    let Some((j, Op::Join(JoinDesc::Probe { slot, qual, keys, .. }))) = p.ops.iter().enumerate().last() else { return false };
    let bound = |id: i32| gx.graph.params.iter().any(|x| x.0 == id);
    let still = |n: Node<'_>| crate::effects::raise_free_by(gx, n, &bound, false) && !crate::effects::user_fn(gx, n) && !crate::effects::volatile(gx, n);
    let probed = |s: &u8| p.ops.iter().any(|o| matches!(o, Op::Join(JoinDesc::Probe { slot, .. }) if slot == s));
    let src = if let Source::Breaker { slot } = p.source { Some(slot) } else { None };
    let drained = |s: &u8| p.ops.iter().any(|o| matches!(o, Op::Join(JoinDesc::Drain { matches, .. }) if matches == s));
    let fed_still = || feed(gx, q, src.into_iter().chain(p.desc.consumes.iter().copied().filter(|s| drained(s)))).into_iter().all(|x| x != q && !graph_acts(gx, x, 1));
    let group = src.and_then(|s| gx.life_of(q, s)).and_then(|l| gx.group_at(l.0 as usize));
    let read = |n: Node<'_>| group.is_some_and(|g| crate::effects::visible::topn::quiet_entry(gx, g, n));
    let own = || p.ops[..j].iter().all(|o| match o {
        Op::Project(l) => l.iter().all(read),
        Op::Filter(n) | Op::Gate(n) => read(*n),
        _ => false,
    });
    let finals = || own() && keys.iter().all(|k| crate::effects::raise_free_each(gx, k, &bound)) && side_by(gx, q, 0, false) == Side::Safe;
    let side = || side_by(gx, q, j, true) == Side::Safe || (group.is_some() && finals());
    crate::frame::iface::end_breaker(gx, q) == Some(BreakerKind::Materialize) && p.desc.consumes.contains(slot) && p.desc.consumes.iter().all(|s| probed(s) || src == Some(*s) || drained(s)) && qual.iter().all(still) && !pipe_acts(gx, p, q, 0) && fed_still() && side()
}

/// (a)'s one exemption: `q` is in a rerun set whose owner (the Nest of its node, the SubPlan of its plan id) is a tail op,
/// so the drain pipeline runs it per row as PostgreSQL does.
fn exempt(gx: &Gx<'_, '_>, q: usize, tail: &[Op<'_>]) -> bool {
    let owns = |r: &gravity_decompose::graph::Rerun| tail.iter().any(|o| match o {
        Op::SubPlan(s) => r.plan_id != 0 && s.plan_id == r.plan_id,
        Op::Join(JoinDesc::Nest { facts, .. }) => r.plan_id == 0 && facts.node == r.node_id,
        _ => false,
    });
    gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&p| p as usize == q) && owns(r))
}

/// Whether drain `d` (its Drain op `jd`) hands every row on to an end that takes them all before it hands one on: a tail of
/// filters, projections, sort runs, joins and SubPlans (no LIMIT, no one-time gate) into a sort, top-N, fold, DISTINCT pair
/// set (D2136) or hash build end (D2131: MultiExecHash reads its child to the end before the join hands a row on; whether the join builds at all is
/// its own outer-first rule's, build_class.rs), into row A2a's write (D2247: PostgreSQL runs a write to its end before the portal hands a row on), into the next
/// right-family join's spool whose drain does the same, into a CTE's store its readers read to its end (`cte_read_whole`),
/// or (`sink`) into a sink its reader takes whole (D1922).
fn every_row(gx: &Gx<'_, '_>, d: usize, jd: usize, sink: bool) -> bool {
    crate::stack_check();
    let p = &gx.pipelines()[d];
    let write = gx.a2a_write(d).or_else(|| gx.held_write(d)).and_then(|j| j.checked_sub(jd + 1));
    // a bare arm marker (D2132 narrowed) hands every row on to the union's end; a settled union's Feed (domain#507) to its
    // record, filled whole before its emit reads a row (`union_record`)
    let arm = |o: &Op<'_>| crate::ops::hashjoin::drain::spool_arms::bare_arm(o) || crate::ops::hashjoin::drain::spool_arms::feed_whole(gx, o);
    p.ops[jd + 1..].iter().enumerate().all(|(i, o)| Some(i) == write || arm(o) || matches!(o, Op::Filter(_) | Op::Project(_) | Op::Sort(_) | Op::Join(_) | Op::SubPlan(_)))
        && (write.is_some()
            || match (gx.end_kind(d), p.desc.produces()) {
                (EndKind::Breaker(BreakerKind::Sort | BreakerKind::TopK | BreakerKind::PlainAgg | BreakerKind::HashAgg | BreakerKind::HashBuild), _) => true,
                // D3731: a hashed SubPlan's set (buildSubPlanHash reads its plan to the end)
                (EndKind::Breaker(BreakerKind::Distinct), _) if crate::ops::hashjoin::drain::lower::set_fill(gx, d) => true,
                // D2136: a pair set, plain or grouped (a HashAggregate's hashed DISTINCT aggregate reads its input to the end
                // before it hands a group on, as the pair set's owner does)
                (EndKind::Breaker(BreakerKind::Distinct), _) if gx.group_at(d).is_some_and(|g| matches!(crate::graph::group_kind(g), KindId::KeyedDistinctPlain | KindId::KeyedDistinctGrouped)) => true,
                (EndKind::Sink, _) => sink,
                (EndKind::Breaker(BreakerKind::Materialize), _) if crate::ops::hashjoin::drain::spool_arms::union_record(gx, d) => true,
                (EndKind::Breaker(BreakerKind::Materialize), Some(m)) if gx.spool_kind(d) == KindId::JoinDrain => {
                    drain_of(gx, d, m).is_some_and(|(e, je)| every_row(gx, e, je, sink))
                }
                (EndKind::Breaker(BreakerKind::Materialize), _) if gx.spool_kind(d) == KindId::NestCte => cte_read_whole(gx, d),
                _ => false,
            })
}

/// D3377 (writesubplan c7): CTE producer `d`'s store is read to its end by every reader that runs. Each reader is a graph
/// pipeline driven by the store (PostgreSQL's CteScan from the store's first row), its ops filters, projections, sort runs
/// and SubPlans (no LIMIT, no gate, no join that may end before its outer does), into a sort, top-N, fold or hash build end,
/// which takes every row before it hands one on. PostgreSQL's CteScans then pull the CTE's plan to its end, as the pull does.
fn cte_read_whole(gx: &Gx<'_, '_>, d: usize) -> bool {
    let ps = gx.pipelines();
    let of_d = |j: usize, s: u8| gx.life_of(j, s).is_some_and(|l| l.0 as usize == d);
    let reads = |j: usize| {
        let p = &ps[j];
        let src = if let Source::Breaker { slot } = p.source { Some(slot) } else { None };
        p.desc.consumes.iter().copied().chain(src).any(|s| of_d(j, s))
    };
    let whole = |j: usize| {
        let p = &ps[j];
        j < ps.len()
            && matches!(p.source, Source::Breaker { slot } if of_d(j, slot))
            && p.ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_) | Op::Sort(_) | Op::SubPlan(_)))
            && matches!(gx.end_kind(j), EndKind::Breaker(BreakerKind::Sort | BreakerKind::TopK | BreakerKind::PlainAgg | BreakerKind::HashAgg | BreakerKind::HashBuild))
    };
    let mut readers = (0..ps.len() + ps.inners().len()).filter(|&j| j != d && reads(j)).peekable();
    readers.peek().is_some() && readers.all(whole)
}

/// N1's whole read of the probe alone (D2129, opr_sanity#99): a RIGHT ANTI drain `d` (its Drain op `jd`) into a sink, its
/// tail filters, projections, SubPlans and nested loops (each fetches its outer row first: no LIMIT, no one-time gate, no
/// hash join that may build first and never fetch). An anti join's rows are the build rows no probe row matched, which
/// PostgreSQL's hash join hands on only once its outer is done (ExecHashJoin's HJ_FILL_INNER_TUPLES), so the client's
/// first fetch runs every probe row whatever it takes after, as the spool does.
fn anti_whole(gx: &Gx<'_, '_>, d: usize, jd: usize) -> bool {
    let p = &gx.pipelines()[d];
    matches!(p.ops[jd], Op::Join(JoinDesc::Drain { jointype: JoinType::JOIN_RIGHT_ANTI, .. }))
        && gx.end_kind(d) == EndKind::Sink
        && p.ops[jd + 1..].iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_) | Op::SubPlan(_) | Op::Join(JoinDesc::Nest { .. })))
}

#[cfg(test)]
mod tests {
    use gravity_abi::breaker::{Breaker, KeyKind};
    use gravity_decompose::effects::Volatility;
    use gravity_decompose::graph::{Bound, Rerun};
    use gravity_decompose::facts::nest::NestFacts;
    use gravity_decompose::ops::shared::sort::{KeyImage, SortDesc, SortForm, SortKey};
    use gravity_decompose::catalog::typefacts::KeyColl;
    use gravity_decompose::{Am, NoFacts, PipelineGraph};
    use gravity_program::{Arena, ArenaBox, ArenaVec, End, Pipeline, Reason, SiteDesc};
    use types_nodes::list::NodeList;

    use super::*;

    /// The m-rule shape the spooled form takes (the psql describe family's): build 0, the probe 1 (a RIGHT JOIN's quiet
    /// probe into its spool 1), pipeline 2 (no probe) the rerun set of the set's `owner` node, the drain 3 whose tail runs a
    /// nest loop of node 77 (`vol` / `rf` its F5 facts) into a sink.
    fn verdict(owner: i32, members: &[u16], jt: JoinType, vol: Volatility, rf: bool) -> Option<&'static str> {
        verdict_nw(owner, members, jt, vol, rf, Nw::None)
    }

    /// The same shape under a reader that takes the sink whole (D1922): `spool_refusal`, and `spool_whole_read`.
    fn verdict_whole(owner: i32, members: &[u16], jt: JoinType, vol: Volatility, rf: bool) -> (Option<&'static str>, bool) {
        let (_, loose, whole_read) = shape_nw(owner, members, jt, vol, rf, Nw::None);
        (loose, whole_read)
    }

    /// A no-widen form beside the shape: none, a CTE InitPlan over pipeline 2 (D2178), a set op in pipeline 2 (D2197), a
    /// quiet merge join in pipeline 2 (D2194, fail closed).
    #[derive(Clone, Copy, PartialEq)]
    enum Nw {
        None,
        Cte,
        SetOp,
        Merge,
    }

    /// The shape's sink read as one a cursor may stop (the names below pin that reading).
    fn verdict_nw(owner: i32, members: &[u16], jt: JoinType, vol: Volatility, rf: bool, nw: Nw) -> Option<&'static str> {
        shape_nw(owner, members, jt, vol, rf, nw).0
    }

    /// (a sink read as one a cursor may stop, `spool_refusal`, `spool_whole_read`)
    fn shape_nw(owner: i32, members: &[u16], jt: JoinType, vol: Volatility, rf: bool, nw: Nw) -> (Option<&'static str>, Option<&'static str>, bool) {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil()));
        let scan = || Source::Range { site: SiteDesc::none_in(a), am: Am::Pgrcolumnar, scanrelid: 1, relid: 16384 };
        let into = |kind, slot| Pipeline::serial(-1, End::Breaker(Breaker::sized(kind, 4096).in_slot(slot)), Reason::NoDrivingSite);
        let sink = || Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite);
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(PipelineNode::new(a, 1, scan(), into(BreakerKind::HashBuild, 0)));
        let mut k = PipelineNode::new(a, 2, scan(), into(BreakerKind::Materialize, 1));
        k.push(Op::Join(JoinDesc::Probe { slot: 0, jointype: jt, clauses: nil, keys: nil, qual: nil, unique: false, marks: true, decls: ArenaVec::new_in(a), inner_node: 1, outer_first: false, catalog: false }));
        g.pipelines.push(k);
        let mut two = PipelineNode::new(a, 5, scan(), into(BreakerKind::Materialize, 2));
        if nw == Nw::SetOp {
            two.push(Op::SetOp(gravity_decompose::ops::shared::setop::SetDesc::Identity { keys: ArenaVec::new_in(a) }));
        }
        if nw == Nw::Merge {
            two.push(Op::Join(JoinDesc::Merge { slot: 0, jointype: JoinType::JOIN_INNER, clauses: nil, reversals: &[], nulls_first: &[], families: &[], collations: &[], qual: nil, unique: false, inner_node: 1 }));
        }
        g.pipelines.push(two);
        if nw == Nw::Cte {
            let params: &types_nodes::list::IntList<'_> = Box::leak(Box::new(types_nodes::list::IntList::nil()));
            let link = gravity_decompose::ops::shared::subplan::SubLink::Cte;
            g.init_plans.push(gravity_decompose::graph::InitPlan { plan_id: 9, pipeline: 2, link, param: -1, params, slot: Some(2), volatility: Volatility::Immutable, raise_free: true, parallel_safe: true, attach: -1 });
        }
        let mut d = PipelineNode::new(a, 2, Source::Breaker { slot: 0 }, sink());
        d.push(Op::Join(JoinDesc::Drain { matches: 1, jointype: jt }));
        let facts = NestFacts { node: 77, invariant: true, volatility: vol, raise_free: rf, eq: None };
        let inner = ArenaBox::new_in(PipelineNode::new(a, 9, scan(), sink()), a);
        d.push(Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, params: nil, inner, qual: nil, unique: false, inner_node: 9, facts }));
        g.pipelines.push(d);
        let mut pipelines = ArenaVec::new_in(a);
        members.iter().for_each(|&m| pipelines.push(m));
        g.reruns.push(Rerun { node_id: owner, plan_id: 0, pipelines, slots: [0; 4] });
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        (gx.spool_refusal_as(1, 3, 0, false), gx.spool_refusal(1, 3, 0), gx.spool_whole_read(1, 3, 0))
    }

    /// (a) exact, both ways (design r2 C2): a pipeline between the probe and the drain that may show it ran (here one
    /// holding a set operation, which `op_raise_free` does not take) is exempt only as a member of a rerun set whose owner
    /// op is in the drain's tail; the same pipeline outside it (another node's set) is N1's. D3853: a still one (a scan
    /// into a plan Material and no op, `between_still`) shows nothing, so N1 takes it in any set or none.
    #[test]
    fn a_pipeline_between_is_exempt_only_under_its_tail_owner() {
        let (i, s, v) = (Volatility::Immutable, JoinType::JOIN_RIGHT, true);
        assert_eq!(verdict_nw(77, &[2], s, i, v, Nw::SetOp), None, "the tail nest owns pipeline 2");
        assert_eq!(verdict_nw(78, &[2], s, i, v, Nw::SetOp), Some(N1), "a set another node owns");
        assert_eq!(verdict_nw(77, &[], s, i, v, Nw::SetOp), Some(N1), "a pipeline in no set");
        assert_eq!(verdict(78, &[2], s, i, v), None, "D3853: a still pipeline in a set another node owns");
        assert_eq!(verdict(77, &[], s, i, v), None, "D3853: a still pipeline in no set");
    }

    /// The names in the design's order: a set holding the drain (N3), a tail that may raise into a sink a cursor may stop
    /// (TAILSTOP); a RIGHT SEMI drain reads as a RIGHT one (D1922). A VOLATILE tail is served (D2134 gone: user 2026-10-04
    /// 15:11Z, call counts free; its order free under DECISIONS 6b).
    #[test]
    fn each_name_where_it_holds() {
        let (i, r) = (Volatility::Immutable, JoinType::JOIN_RIGHT);
        assert_eq!(verdict(77, &[2], JoinType::JOIN_RIGHT_SEMI, i, true), None);
        assert_eq!(verdict(77, &[2], JoinType::JOIN_RIGHT_SEMI, i, false), Some(TAILSTOP));
        assert_eq!(verdict(77, &[2, 3], r, i, true), Some(N3));
        assert_eq!(verdict(77, &[2], r, Volatility::Volatile, true), None, "a VOLATILE raise-free tail: D2134 gone");
        assert_eq!(verdict(77, &[2], r, Volatility::Volatile, false), Some(TAILSTOP), "a VOLATILE tail that may raise under a stop");
        assert_eq!(verdict(77, &[2], r, Volatility::Stable, true), None, "a STABLE builtin nest acts on nothing");
        assert_eq!(verdict(77, &[2], r, i, false), Some(TAILSTOP));
        assert_eq!(verdict(77, &[2], JoinType::JOIN_FULL, i, true), None);
    }

    /// No statement-wide name refuses a form of the frozen no-widen list (a CTE: D2178, a merge join: D2194's merge class, a
    /// set operation: D2197; D2180, D2194's lazy build): the drain's own names judge them, and the plain shape stays served.
    #[test]
    fn no_widen_names_refuse_statement_wide() {
        let (i, r) = (Volatility::Immutable, JoinType::JOIN_RIGHT);
        assert_eq!(verdict_nw(77, &[2], r, i, true, Nw::None), None);
        assert_eq!(verdict_nw(77, &[2], r, i, true, Nw::Cte), None, "D2178 lifted (writesubplan, main's GO)");
        assert_eq!(verdict_nw(77, &[2], r, i, true, Nw::SetOp), None, "D2197 lifted at its root fix (main's GO)");
        let full = |nw| verdict_nw(77, &[2], JoinType::JOIN_FULL, Volatility::Volatile, true, nw);
        assert_eq!(full(Nw::SetOp), full(Nw::None), "a set operation adds no name of its own");
        assert_eq!(verdict_nw(77, &[2], r, i, true, Nw::Merge), None, "a merge join in the tail nest's rerun set runs per row, as PG's (exempt)");
    }

    /// Build 0, the RIGHT probe 1 over a VALUES source (not D393's quiet: no scan; a Result's one row is) into its spool 1,
    /// pipeline 2 between (a sort end over a scan, in no rerun set, a nest of volatility `between` its one op), the drain 3
    /// (a quiet nest in its tail, so the `m` rule refuses) into a sort end, or the sink when `sink`.
    fn whole(between: Volatility, sink: bool) -> Option<&'static str> {
        whole_as(between, sink).0
    }

    /// (`whole`'s verdict, the same under a reader that takes the sink whole: `spool_refusal`, `spool_whole_read`)
    fn whole_as(between: Volatility, sink: bool) -> (Option<&'static str>, Option<&'static str>, bool) {
        whole_with(between, sink, None)
    }

    /// `whole_as`, the drain's tail ending in a Sort op over `key` when given (its sort end's own sort).
    fn whole_with(between: Volatility, sink: bool, key: Option<SortKey>) -> (Option<&'static str>, Option<&'static str>, bool) {
        whole_set(between, sink, key, None)
    }

    /// `whole_with` under a rerun set of node 80 (a nest's, plan 0) when given: its members, and the slots it leases.
    fn whole_set(between: Volatility, sink: bool, key: Option<SortKey>, set: Option<(&[u16], &[u8])>) -> (Option<&'static str>, Option<&'static str>, bool) {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil()));
        let scan = || Source::Range { site: SiteDesc::none_in(a), am: Am::Heap, scanrelid: 1, relid: 16384 };
        let into = |kind, slot| Pipeline::serial(-1, End::Breaker(Breaker::sized(kind, 4096).in_slot(slot)), Reason::NoDrivingSite);
        let sink_end = || Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite);
        let nest = |node: i32, volatility| {
            let facts = NestFacts { node, invariant: true, volatility, raise_free: true, eq: None };
            let inner = ArenaBox::new_in(PipelineNode::new(a, 9, scan(), sink_end()), a);
            Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, params: nil, inner, qual: nil, unique: false, inner_node: 9, facts })
        };
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(PipelineNode::new(a, 1, scan(), into(BreakerKind::HashBuild, 0)));
        let mut k = PipelineNode::new(a, 2, Source::Values { scanrelid: 2, lists: nil }, into(BreakerKind::Materialize, 1));
        k.push(Op::Join(JoinDesc::Probe { slot: 0, jointype: JoinType::JOIN_RIGHT, clauses: nil, keys: nil, qual: nil, unique: false, marks: true, decls: ArenaVec::new_in(a), inner_node: 1, outer_first: false, catalog: false }));
        g.pipelines.push(k);
        let mut two = PipelineNode::new(a, 5, scan(), into(BreakerKind::Sort, 2));
        two.push(nest(78, between));
        g.pipelines.push(two);
        let mut d = PipelineNode::new(a, 2, Source::Breaker { slot: 0 }, if sink { sink_end() } else { into(BreakerKind::Sort, 3) });
        d.push(Op::Join(JoinDesc::Drain { matches: 1, jointype: JoinType::JOIN_RIGHT }));
        d.push(nest(77, Volatility::Immutable));
        if let Some(key) = key {
            let mut keys = ArenaVec::new_in(a);
            keys.push(key);
            d.push(Op::Sort(SortDesc { keys, presorted: 0, form: SortForm::Runs, tlist: nil }));
        }
        g.pipelines.push(d);
        // D3834: a member 4 is a pipeline after the drain's read filling slot 1 again (a later life of the spool's slot id)
        if set.is_some_and(|(members, _)| members.contains(&4)) {
            g.pipelines.push(PipelineNode::new(a, 6, scan(), into(BreakerKind::Materialize, 1)));
        }
        if let Some((members, leased)) = set {
            let mut pipelines = ArenaVec::new_in(a);
            members.iter().for_each(|&m| pipelines.push(m));
            let mut slots = [0; 4];
            leased.iter().for_each(|&s| gravity_decompose::graph::slot_set(&mut slots, s));
            g.reruns.push(Rerun { node_id: 80, plan_id: 0, pipelines, slots });
        }
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        (gx.spool_refusal_as(1, 3, 0, false), gx.spool_refusal(1, 3, 0), gx.spool_whole_read(1, 3, 0))
    }

    /// D3724 (N3 narrowed, `set_refused`): the sort-end shape inside a nest's rerun set is served where every set holding
    /// the probe or the drain holds both (the pipeline between in it or not) and leases the spool's slot only with them; a
    /// set holding one of the two, or leasing the spool alone, keeps N3.
    #[test]
    fn a_set_running_the_probe_with_its_drain_is_served() {
        let at = |members: &[u16], leased: &[u8]| whole_set(Volatility::Immutable, false, None, Some((members, leased)));
        assert_eq!(at(&[0, 1, 2, 3], &[0, 1, 2]), (None, None, false), "the whole join in one set, its slots leased");
        assert_eq!(at(&[1, 3], &[1]), (None, None, false), "the probe and its drain, the pipeline between outside");
        assert_eq!(at(&[0, 1, 2], &[0, 1]).1, Some(N3), "the drain outside the set");
        assert_eq!(at(&[2, 3], &[]).1, Some(N3), "the probe outside the set");
        assert_eq!(at(&[2], &[1]).1, Some(N3), "a set leasing the spool without the probe or the drain");
        assert_eq!(at(&[2], &[]), (None, None, false), "a set touching neither");
    }

    /// D3834: a set whose member runs after the drain's read and fills the spool's slot id again (another breaker's life)
    /// does not lease the spool: no N3. A member running while the spool holds the slot (the pipeline between) keeps it.
    #[test]
    fn a_set_leasing_the_spool_slot_id_in_a_later_life_is_no_touch() {
        let at = |members: &[u16], leased: &[u8]| whole_set(Volatility::Immutable, false, None, Some((members, leased)));
        assert_eq!(at(&[4], &[1]), (None, None, false), "slot 1 refilled after the drain read the spool");
        assert_eq!(at(&[2, 4], &[1]).1, Some(N3), "a member while the spool holds slot 1");
        assert_eq!(at(&[4], &[]), (None, None, false), "a later set leasing nothing");
    }

    /// D2136 narrowed (held-out rnd-3270, jcore-0255): build 0, the RIGHT probe 1 into its spool 1, the drain 2 into the
    /// hash build of slot 2, and pipeline 3 (a probe of slot 2 of join type `jt`, unique when `unique`, a LIMIT past it when
    /// `limit`; consuming slot 2 unless `!reads`) into a sort end: `order_free` of the drain's hash build end.
    fn build_end(jt: JoinType, unique: bool, limit: bool, reads: bool) -> bool {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil()));
        let scan = || Source::Range { site: SiteDesc::none_in(a), am: Am::Heap, scanrelid: 1, relid: 16384 };
        let into = |kind, slot| Pipeline::serial(-1, End::Breaker(Breaker::sized(kind, 4096).in_slot(slot)), Reason::NoDrivingSite);
        let probe = |slot, jointype, unique, inner_node| Op::Join(JoinDesc::Probe { slot, jointype, clauses: nil, keys: nil, qual: nil, unique, marks: jointype == JoinType::JOIN_RIGHT, decls: ArenaVec::new_in(a), inner_node, outer_first: false, catalog: false });
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(PipelineNode::new(a, 1, scan(), into(BreakerKind::HashBuild, 0)));
        let mut k = PipelineNode::new(a, 2, scan(), into(BreakerKind::Materialize, 1));
        k.push(probe(0, JoinType::JOIN_RIGHT, false, 1));
        g.pipelines.push(k);
        let mut d = PipelineNode::new(a, 2, Source::Breaker { slot: 0 }, into(BreakerKind::HashBuild, 2));
        d.push(Op::Join(JoinDesc::Drain { matches: 1, jointype: JoinType::JOIN_RIGHT }));
        g.pipelines.push(d);
        let mut r = PipelineNode::new(a, 4, scan(), if reads { into(BreakerKind::Sort, 3).consuming(&[2]) } else { into(BreakerKind::Sort, 3) });
        r.push(probe(2, jt, unique, 2));
        if limit {
            r.push(Op::Limit { count: Bound::Const(3), offset: Bound::Const(0), ties: None });
        }
        g.pipelines.push(r);
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        order_free(&gx, 2)
    }

    /// D2136 narrowed: a drain into the next join's hash build is order-free where every reader of the table takes its
    /// order to an order-free end (an inner, a semi or a unique probe whose keys and quals raise nothing, into a sort); a
    /// LIMIT past the reader's probe keeps the name, and so does a table no pipeline is known to read.
    #[test]
    fn a_hash_build_end_is_free_where_its_readers_are() {
        let (inner, semi) = (JoinType::JOIN_INNER, JoinType::JOIN_SEMI);
        assert!(build_end(inner, false, false, true), "an inner probe into a sort end");
        assert!(build_end(semi, false, false, true), "a semi probe whose keys and quals raise nothing");
        assert!(build_end(inner, true, false, true), "a unique inner whose keys and quals raise nothing");
        assert!(!build_end(inner, false, true, true), "a LIMIT past the probe reads rows in the table's order");
        assert!(!build_end(inner, false, false, false), "no known reader");
    }

    /// D2133 narrowed (stats_ext#827 / #832): a drain into a sort end whose key's comparator is a DISPATCH compare (record
    /// `<`, its column comparators looked up at run time) or a user family's is not USER's: the comparator compares rows the
    /// drain already handed on, as in PostgreSQL's plan. (Whatever else the tail's sort reads as, TAILVOL's reading of it
    /// included, is not this name's.)
    #[test]
    fn a_sort_key_comparator_is_not_user() {
        let key = |opno: u32, own_family: bool| SortKey { resno: 1, opno, desc: false, nulls_first: false, kind: KeyKind::Fp128, image: KeyImage::ByRef, scale: 0, coll: KeyColl { oid: 0, bytes_eq: true, bytes_order: true }, own_family, no_proc: None, eq_tie: false, per_row: false };
        assert_eq!(whole_with(Volatility::Immutable, false, None), (None, None, false), "the plain sort end");
        for (k, what) in [(key(2990, false), "record <"), (key(1072, false), "anyarray <"), (key(97, true), "a user family's int4 <")] {
            let v = whole_with(Volatility::Immutable, false, Some(k));
            assert_ne!(v.0, Some(USER), "{what}");
            assert_ne!(v.1, Some(USER), "{what}");
        }
    }

    /// D1922 (the portal-kind fact): a sink its reader takes whole is read as nothing stops it, so N1 asks only for calls and
    /// TAILSTOP does not hold; the form then holds the whole-read mark, which a sort end never needs.
    #[test]
    fn a_sink_read_whole_takes_the_whole_rule_and_its_mark() {
        let (i, r) = (Volatility::Immutable, JoinType::JOIN_RIGHT);
        assert_eq!(whole_as(Volatility::Immutable, true), (Some(N1), None, true), "N1 asks only for calls");
        assert_eq!(whole_as(Volatility::Volatile, true), (Some(N1), Some(N1), true), "a VOLATILE call between still refuses");
        assert_eq!(whole_as(Volatility::Immutable, false), (None, None, false), "a sort end needs no mark");
        assert_eq!(verdict_whole(77, &[2], r, i, false), (None, true), "TAILSTOP holds only for a reader that may stop");
        assert_eq!(verdict_whole(77, &[2], r, i, true), (None, false), "a quiet tail needs no mark");
        assert_eq!(verdict_whole(77, &[2], r, Volatility::Volatile, true), (None, false), "a VOLATILE raise-free tail: D2134 gone");
    }

    /// N1 where nothing stops the read (a sort end, no LIMIT): a probe and a pipeline between that call nothing pass though
    /// neither is D393's quiet; a VOLATILE call between refuses. Under a stop (the sink a cursor may stop) D393's quiet holds.
    #[test]
    fn n1_asks_only_for_calls_when_every_row_is_read() {
        assert_eq!(whole(Volatility::Immutable, false), None);
        assert_eq!(whole(Volatility::Volatile, false), Some(N1));
        assert_eq!(whole(Volatility::Immutable, true), Some(N1));
    }

    /// A CTE reader of `cte_store`: a sort end, the client's sink, or a LIMIT before a sort end.
    #[derive(Clone, Copy)]
    enum CteReader {
        Sort,
        Sink,
        Limited,
    }

    /// Build 0, the quiet RIGHT probe 1 into its spool 1, the drain 2 (a nest that may raise in its tail) into the store of
    /// CTE InitPlan 2 (slot 2), then one pipeline per reader of `readers` driven by the store.
    fn cte_store(readers: &[CteReader]) -> Option<&'static str> {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil()));
        let scan = || Source::Range { site: SiteDesc::none_in(a), am: Am::Pgrcolumnar, scanrelid: 1, relid: 16384 };
        let into = |kind, slot| Pipeline::serial(-1, End::Breaker(Breaker::sized(kind, 4096).in_slot(slot)), Reason::NoDrivingSite);
        let sink = || Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite);
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(PipelineNode::new(a, 1, scan(), into(BreakerKind::HashBuild, 0)));
        let mut k = PipelineNode::new(a, 2, scan(), into(BreakerKind::Materialize, 1));
        k.push(Op::Join(JoinDesc::Probe { slot: 0, jointype: JoinType::JOIN_RIGHT, clauses: nil, keys: nil, qual: nil, unique: false, marks: true, decls: ArenaVec::new_in(a), inner_node: 1, outer_first: false, catalog: false }));
        g.pipelines.push(k);
        let mut d = PipelineNode::new(a, 2, Source::Breaker { slot: 0 }, into(BreakerKind::Materialize, 2));
        d.push(Op::Join(JoinDesc::Drain { matches: 1, jointype: JoinType::JOIN_RIGHT }));
        let facts = NestFacts { node: 77, invariant: true, volatility: Volatility::Immutable, raise_free: false, eq: None };
        let inner = ArenaBox::new_in(PipelineNode::new(a, 9, scan(), sink()), a);
        d.push(Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, params: nil, inner, qual: nil, unique: false, inner_node: 9, facts }));
        g.pipelines.push(d);
        let params: &types_nodes::list::IntList<'_> = Box::leak(Box::new(types_nodes::list::IntList::nil()));
        let link = gravity_decompose::ops::shared::subplan::SubLink::Cte;
        g.init_plans.push(gravity_decompose::graph::InitPlan { plan_id: 9, pipeline: 2, link, param: -1, params, slot: Some(2), volatility: Volatility::Immutable, raise_free: true, parallel_safe: true, attach: -1 });
        for (i, r) in readers.iter().enumerate() {
            let mut p = PipelineNode::new(a, 10 + i as i32, Source::Breaker { slot: 2 }, match r {
                CteReader::Sink => sink(),
                _ => into(BreakerKind::Sort, 3 + i as u8),
            });
            if let CteReader::Limited = r {
                p.push(Op::Limit { count: Bound::Const(1), offset: Bound::Const(0), ties: None });
            }
            g.pipelines.push(p);
        }
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        gx.spool_refusal(1, 2, 0)
    }

    /// D3377 (writesubplan c7): a tail that may raise into a CTE's store is no stop when every reader of the store reads it to
    /// its end (a sort end): PostgreSQL's CteScans pull the CTE's plan to its end as the spool runs it. A reader that may stop
    /// (the client's sink a cursor may stop, a LIMIT before its sort) keeps TAILSTOP, as does a store no pipeline reads.
    #[test]
    fn a_cte_store_every_reader_reads_whole_is_no_stop() {
        use CteReader::{Limited, Sink, Sort};
        assert_eq!(cte_store(&[Sort]), None, "one sorting reader");
        assert_eq!(cte_store(&[Sort, Sort]), None, "two sorting readers");
        assert_eq!(cte_store(&[Sink]), Some(TAILSTOP), "the client's sink a cursor may stop");
        assert_eq!(cte_store(&[Sort, Limited]), Some(TAILSTOP), "a LIMIT before a reader's sort");
        assert_eq!(cte_store(&[]), Some(TAILSTOP), "no reader");
    }
}

/// D3800: a column of the row a drain's tail reads, by what one build record's rows hold there.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Col {
    /// the same in every row of the record: the build record's own cells, the target's row, what is made of those
    Fixed,
    /// a probe key of the RIGHT probe (a bare probe column): never NULL in a joined row (NULL keys never match), NULL in
    /// the NULL-extended one
    Key,
    /// any other probe column: it may differ from one joined row to the next
    Loose,
}

/// D3801 (`held_write`; Rails' update_all over a LEFT OUTER self-join alias whose SET reads the joined table: `UPDATE
/// pets a SET name = COALESCE(toys.name, ..) FROM pets LEFT JOIN toys ON .. WHERE .. AND pets.pet_id = a.pet_id`): the
/// rows drain `d` (its RIGHT Drain op `jd`) hands on for one build record come in PostgreSQL's order. The probe scans a
/// heap relation (its range, an index or a bitmap: the order PostgreSQL's scan of the same node reads) through filters
/// and projections alone, and the spool's fill runs it serially (DrainFill), so a record's joined rows are in
/// PostgreSQL's outer order, followed by nothing (an unmatched record has no joined row, a matched one no drained row).
/// The tail's ops are `right_same`'s, each keeping its rows' order (projections, filters, INNER nests), and a write with
/// no check option (a failing check raises from the first row PostgreSQL meets across the targets). With `pinned` each
/// target row is reached from one record, and PostgreSQL's first row reaching it, which ModifyTable writes, is the
/// spool's first.
fn right_ordered(gx: &Gx<'_, '_>, d: usize, jd: usize) -> bool {
    use gravity_decompose::Am;
    let ps = gx.pipelines();
    let Some(Op::Join(JoinDesc::Drain { matches, .. })) = ps[d].ops.get(jd) else { return false };
    let Some(k) = gx.life_of(d, *matches).map(|l| usize::from(l.0)) else { return false };
    let heap = matches!(ps[k].source, Source::Range { am: Am::Heap, .. } | Source::Index { am: Am::Heap, .. } | Source::Bitmap { am: Am::Heap, .. });
    let quiet = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    let probe = ps[k].ops.split_last().is_some_and(|(l, before)| matches!(l, Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_RIGHT, catalog: false, .. })) && before.iter().all(quiet));
    let tail = ps[d].ops[jd + 1..].iter().all(|o| match o {
        Op::Join(JoinDesc::Nest { jointype, .. }) => *jointype == JoinType::JOIN_INNER,
        Op::Write(w) => w.checks.is_empty(),
        o => quiet(o),
    });
    heap && probe && tail
}

/// D3800 (`held_write`): every row drain `d` (its RIGHT Drain op `jd`) hands on for one build record writes the same.
/// The tail's first projection reads the probe's columns (OUTER_VAR) and the build's (INNER_VAR); each later op reads
/// the last projection's columns (OUTER_VAR) and the target's row (a nest's inner, the written row). A Key column is
/// read only under IS [NOT] NULL, which every joined row answers alike; a Loose one only carried on as itself, to be a
/// row mark's junk column (its row is held: the recheck never reads it). What the write takes (its SET cells, its
/// carried old cells, its RETURNING list) is Fixed. The ops: projections, filters, INNER nests, the write.
fn right_same(gx: &Gx<'_, '_>, d: usize, jd: usize) -> bool {
    use types_nodes::primnodes::OUTER_VAR;
    let ps = gx.pipelines();
    let Some(Op::Join(JoinDesc::Drain { matches, .. })) = ps[d].ops.get(jd) else { return false };
    let Some(k) = gx.life_of(d, *matches).map(|l| usize::from(l.0)) else { return false };
    let Some(Op::Join(JoinDesc::Probe { keys, .. })) = ps[k].ops.last() else { return false };
    let mut key_cols = PgVec::new_in(gx.mcx);
    for e in keys.iter() {
        match bare_var(e) {
            Some(v) if v.varno == OUTER_VAR && v.varlevelsup == 0 => key_cols.push_in(v.varattno),
            _ => return false,
        }
    }
    // the classes of the row's columns (OUTER_VAR), None before the first projection (the probe's row)
    let mut row: Option<PgVec<'_, Col>> = None;
    let mut written = false;
    for o in &ps[d].ops[jd + 1..] {
        let outer = |a: i16| match &row {
            None if key_cols.contains(&a) => Col::Key,
            None => Col::Loose,
            Some(cols) => usize::try_from(a).ok().and_then(|a| a.checked_sub(1)).and_then(|a| cols.get(a).copied()).unwrap_or(Col::Loose),
        };
        let fixed = |e: Node<'_>| reads_same(e, &outer);
        match o {
            Op::Project(l) => {
                let mut cols = PgVec::new_in(gx.mcx);
                for n in l.iter() {
                    let e = n.as_target_entry().map_or(n, |t| t.expr);
                    let c = match bare_var(e) {
                        Some(v) if v.varno == OUTER_VAR && v.varlevelsup == 0 => outer(v.varattno),
                        _ if fixed(e) => Col::Fixed,
                        _ => return false,
                    };
                    cols.push_in(c);
                }
                // the write's own projection (its RETURNING list's) reads the row the write took
                if !written {
                    row = Some(cols);
                }
            }
            Op::Filter(x) | Op::Gate(x) if fixed(*x) => {}
            Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, params, qual, .. }) => {
                if crate::frame::iface::nest_param_vals(params, &|v| !fixed(v)) || !qual.iter().all(fixed) {
                    return false;
                }
            }
            Op::Write(w) => {
                let n = w.set.map_or(0, |s| s.iter().count());
                let cells = row.as_deref().unwrap_or(&[]);
                let set = cells.len() >= n && cells[..n].iter().all(|&c| c == Col::Fixed);
                let old = w.old_cols.iter().all(|&(_, c)| outer(c) == Col::Fixed);
                if !set || !old || !w.checks.is_empty() || !w.returning.is_none_or(|l| l.iter().all(fixed)) {
                    return false;
                }
                written = true;
            }
            _ => return false,
        }
    }
    written
}

/// D3800 (`held_write`): each target row write `wj` of drain `d` (its RIGHT Drain op `jd`) takes is reached from at most
/// one build record. The target's nest past the drain (an INNER nest whose inner scans the write's target by an index)
/// finds the row by a key `column = value` under an equality whose equal values are one image (keysem `image_eq`: the
/// integer `=`, oid, date, bool, char), its value a nest parameter carrying a build record's column, and that column holds
/// a different value in every record (`distinct`). Two records reaching one target row would hold its column's one image
/// there. Without it (Rails' `UPDATE .. FROM` over a non-unique alias) a SET or RETURNING reading the build could take
/// another record's cells than PostgreSQL's bucket order picks: writes stay exact.
fn pinned(gx: &Gx<'_, '_>, d: usize, jd: usize, wj: usize) -> bool {
    use gravity_decompose::graph::KeyForm;
    use types_nodes::primnodes::{ParamKind, INNER_VAR, OUTER_VAR};
    let ps = gx.pipelines();
    let ops = &ps[d].ops;
    let (Some(Op::Join(JoinDesc::Drain { matches, .. })), Some(Op::Write(w))) = (ops.get(jd), ops.get(wj)) else { return false };
    // the build pipeline: the one whose table the drain's probe reads
    let Some(k) = gx.life_of(d, *matches).map(|l| usize::from(l.0)) else { return false };
    let Some(Op::Join(JoinDesc::Probe { slot, .. })) = ps[k].ops.last() else { return false };
    let Some(b) = gx.life_of(k, *slot).map(|l| usize::from(l.0)).filter(|&b| b < k && ps[b].desc.produces() == Some(*slot)) else { return false };
    let Some(bj) = ps[b].ops.iter().rposition(|o| matches!(o, Op::Join(JoinDesc::Build { .. }))) else { return false };
    // the build record's column `a` (INNER_VAR) column `c` (OUTER_VAR) of the row entering op `i` of the drain's tail
    // carries, through its filters and projections
    let build_col = |mut i: usize, mut c: i16| -> Option<i16> {
        loop {
            let at = ops[jd + 1..i].iter().rposition(|o| !matches!(o, Op::Filter(_) | Op::Gate(_)))? + jd + 1;
            let Op::Project(l) = &ops[at] else { return None };
            let v = l.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == c).and_then(|te| bare_var(te.expr)).filter(|v| v.varlevelsup == 0)?;
            let first = !ops[jd + 1..at].iter().any(|o| matches!(o, Op::Project(_)));
            match v.varno {
                OUTER_VAR if !first => (i, c) = (at, v.varattno),
                INNER_VAR if first => return Some(v.varattno),
                _ => return None,
            }
        }
    };
    (jd + 1..wj).any(|jn| {
        let Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, params, .. }) = &ops[jn] else { return false };
        let Some(ik) = ps.inner_of(d, jn) else { return false };
        let Source::Index { relid, scanrelid, keys, .. } = &ps[ik].source else { return false };
        if *relid != w.plan.target_rel || u32::from(*scanrelid) != w.rti {
            return false;
        }
        keys.iter().any(|key| {
            let Some(p) = gravity_decompose::expr::through_relabel(key.value).unwrap_or(key.value).as_param().filter(|p| p.paramkind == ParamKind::PARAM_EXEC) else { return false };
            let carried = crate::frame::iface::nest_param_val(params, p.paramid).and_then(bare_var);
            key.form == KeyForm::Op
                && gravity_decompose::catalog::keysem::image_eq(gx.facts, key.opno).is_some()
                && carried.is_some_and(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && build_col(jn, v.varattno).is_some_and(|a| distinct(gx, b, bj, a, 0)))
        })
    })
}

/// D3800 (`pinned`): column `a` of the row entering op `pos` of pipeline `k` (an OUTER_VAR, or the scan's column at its
/// source) holds a different value in every row the pipeline hands on there. Walked back through ops that drop rows or
/// keep them, never make two of one (a filter, a gate, a limit, a row lock, a window node's pass-through columns, a
/// sort), a projection's bare column, to the source: a heap scan of a relation a unique index covers on that column
/// (ops/shared/unique.rs `unique_on`), or a breaker that keeps its producer's rows (a sort, a top-k, a distinct, a
/// Material: a CTE's store) and then that producer's column. Anything else (a join, a set-returning function, an Append,
/// an aggregate, an expression) is not known distinct.
fn distinct(gx: &Gx<'_, '_>, k: usize, mut pos: usize, mut a: i16, depth: u8) -> bool {
    crate::stack_check();
    use gravity_decompose::Am;
    use types_nodes::primnodes::{INNER_VAR, INDEX_VAR, OUTER_VAR};
    let ps = gx.pipelines();
    let p = &ps[k];
    // an op that drops rows or keeps them, its columns the row's
    let drops = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Lock(_)) || crate::frame::iface::holds_limit(core::slice::from_ref(o));
    let mut scan = false;
    loop {
        let Some(i) = p.ops[..pos].iter().rposition(|o| !drops(o) && !matches!(o, Op::Window(_) | Op::Sort(_))) else { break };
        let Op::Project(l) = &p.ops[i] else { return false };
        let Some(v) = l.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == a).and_then(|te| bare_var(te.expr)).filter(|v| v.varlevelsup == 0) else { return false };
        match v.varno {
            OUTER_VAR => (pos, a) = (i, v.varattno),
            INNER_VAR | INDEX_VAR => return false,
            _ => {
                // a scan column: the source's own
                let own = match &p.source {
                    Source::Range { scanrelid, .. } | Source::Index { scanrelid, .. } | Source::Bitmap { scanrelid, .. } => i32::from(*scanrelid) == v.varno,
                    Source::Breaker { .. } => true,
                    _ => false,
                };
                if !own || !p.ops[..i].iter().all(drops) {
                    return false;
                }
                (scan, a) = (true, v.varattno);
                break;
            }
        }
    }
    match &p.source {
        Source::Range { am: Am::Heap, relid, .. } | Source::Index { am: Am::Heap, relid, only: false, .. } | Source::Bitmap { am: Am::Heap, relid, .. } => scan && a > 0 && crate::ops::shared::unique::unique_on(gx, *relid, &[a]),
        Source::Breaker { slot } if depth < 8 => {
            let Some(q) = gx.life_of(k, *slot).map(|l| usize::from(l.0)).filter(|&q| q < k) else { return false };
            let keeps = crate::frame::iface::breaker_of(&ps[q].desc).is_some_and(|b| b.slot == *slot && matches!(b.kind, BreakerKind::Sort | BreakerKind::TopK | BreakerKind::Distinct | BreakerKind::Materialize));
            keeps && distinct(gx, q, ps[q].ops.len(), a, depth + 1)
        }
        _ => false,
    }
}

/// `e` through any RelabelType, as a Var.
fn bare_var<'p>(e: Node<'p>) -> Option<&'p types_nodes::primnodes::Var> {
    gravity_decompose::expr::through_relabel(e).unwrap_or(e).as_var()
}

/// `right_same`'s Fixed expression: a Key column only under IS [NOT] NULL, no Loose column, over `outer`'s classes.
fn reads_same(e: Node<'_>, outer: &dyn Fn(i16) -> Col) -> bool {
    crate::stack_check();
    use types_nodes::primnodes::OUTER_VAR;
    if let Some(v) = e.as_null_test().and_then(|t| t.arg).and_then(bare_var) {
        if v.varno == OUTER_VAR && v.varlevelsup == 0 && outer(v.varattno) == Col::Key {
            return true;
        }
    }
    if let Some(v) = e.as_var() {
        return v.varno != OUTER_VAR || v.varlevelsup != 0 || outer(v.varattno) == Col::Fixed;
    }
    let mut ok = true;
    let known = gravity_decompose::expr::kids(e, &mut |x| ok &= reads_same(x, outer));
    ok && known
}
