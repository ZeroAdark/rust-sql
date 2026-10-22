//! Kind `nest.rerun` (a pipeline range re-run per param change; L5, B3) (cards/M8.md, M8-changes.md §1). Owner M8-RERUN
//! (files-override from M8-NEST, main 17:03Z 09-27).
//! - The set. Decompose's rerun set (graph.rs `Rerun`, `rerun::declare_rerun`): the pipelines of a parameterised inner PG
//!   re-executes when the owner's params change (chgParam), with the slots they fill or read leased for the statement. The
//!   owner is a nest (`plan_id` 0: the embedded inner's re-open, nest/inner.rs) or a nested SubPlan (its per-call re-open,
//!   nest.subplan).
//! - The protos. One `ProtoKind::Rerun` per member pipeline of a declared set (`pipeline` the member, so the program
//!   renumbering maps it as any proto's), `peer` the owner's pipeline (the one calling `m8_rerun`, RT-2e's
//!   `Plan::rerun_owner`), `arg` the set's index in graph.reruns, and one word shared by the set's protos:
//!   the handle the runtime lays the set's object into (exec v4/m8_rerun.rs groups a set's members by it). The members
//!   are deferred (frame/program.rs): the producer order steps past them and only `m8_rerun` runs them.
//! - The run. PG's ExecNestLoop / ExecScanSubPlan add every param to the inner's chgParam at every outer row or call,
//!   whatever its value, so the set re-executes at every re-open (main 17:24Z: the card's "unchanged params skip" is
//!   superseded; a skip on equal params is a later road, for sets with no volatile call only). `Rerun::run` emits
//!   `m8_rerun(ctx, h)` after the owner wrote the param words and before the inner opens: the runtime resets the members'
//!   slots in place and runs them nested through the leader's frame (RT-2e), and a failed run raises its stashed error
//!   where the owner re-opens, as PG raises it inside the rescan. Every pipeline whose code calls `m8_rerun` runs on the
//!   leader (frame/program.rs `on_demand_runs`, D1035).
//! - A set no owner declared refuses by this kind's name at the emit (frame/program.rs): its members would
//!   never re-run.
//! - D1902 / D1904 / D1906: a member hash probe whose build order PG decides per call (hj_OuterNotEmpty), whose outer PG
//!   scans whole over a table kept across calls, or whose kept table PG may rebuild per call (multi-batch) while its build
//!   side calls, refuses at its make where that would show, before the owner declares the set (lifetime/hashjoin.rs
//!   `rerun_order`, by the side classes).
//! - The readers (D1337(A), `members_close`): a nested SubPlan's set is served when each member's slot is read by fold.plain's
//!   emit, by a sort run's rows (D1339 row B: a member's, or the owner's inner's at the owner's region, `emit_ok`) or, for a
//!   hash build, by set members' probes or (D1337(H)) the SubPlan's own embedded inner's probe, or (D1385) a CTE's store by a member CteScan, or
//!   a window fill by a member window reader (D2346: each open restarts the object), or a count(DISTINCT) emit by the owner's
//!   own inner at the owner's first consumed region (`emit_ok`), or a hash build's unmarked records and its probe's spool by
//!   a member drain (a Hash Right / Full / Right Anti Join's tail), or a set operation's two groups by its emit when both
//!   fills are members (`set_op_side`), or a sorted group's or Unique's emit as the keyed emit. A feed of the settled union
//!   the owner's own inner streams (S2b, rerun/inner_arms.rs) never runs: that inner opens its arm again at every call. Any
//!   other reader (a spool, a union's record) refuses by nest.subplan's D1337 Shape until its
//!   emit is re-run with its own state. A pipeline embedding the owner (a nest's root
//!   over the inner holding the SubPlan) lists the owner's reads as its own and is no reader (`embeds`).
//! - Shared members (D1337 row B, D2300): a set whose member another rerun set also runs (a LATERAL nest's set holding a
//!   SubPlan's inner) refuses at `declare` by D_RERUN_SHARED's name, for every owner: the other set's run would run that
//!   member before its own owner wrote the parameters it reads, where PG only marks the member's chgParam. An InitPlan's
//!   members are only re-armed by each set holding them, so they are shared freely, and a member of two nested sets (the
//!   inner set's owner a member of the outer, `nested`) is run by the inner set only.
use gravity_abi::breaker::BreakerKind;
use gravity_abi::status::raise;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::setop::{RECURSIVE_SEED_SET, RECURSIVE_SET};
use gravity_decompose::ops::shared::subplan::{SubForm, SubLink};
use gravity_decompose::{Op, PipelineNode, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntCond, Type, Value};
use gravity_kernels::ops::shared::object::entries as m8;
use gravity_kernels::ops::shared::object::raw::{RerunPull, RerunRearm};
use gravity_program::protos::{Proto, ProtoKind, SUBHASH_LAZY};
use mcx::{CollectIn, PgVec};
use types_nodes::jointype::JoinType;

use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;

/// Registered with no maker of its own: the owners (nest/inner.rs, nest.subplan) declare their set through `declare`.
pub(crate) const ROWS: Table = Table { kinds: &[(KindId::NestRerun, &Owned)], ..crate::ops::NONE };

/// `nest.rerun`'s registry row: a kind its owners' makers use, never made alone.
pub(crate) struct Owned;

/// A declared rerun set: the handle word its owner's re-open passes to `m8_rerun`, and the set's owner-run words (the
/// lifetime home's `Scope::OwnerRun`, captured at the owner's declaration), stored just before every run. `runs`: the set
/// runs a member of its own; a set whose every member a set nested in it runs (`nested`: D1337, decorrelate's level-2
/// reference, the inner SubPlan's set inside the outer SubPlan's own inner) lays out no object at `h`, so its owner's
/// re-open stores the owner-run words and calls nothing.
#[allow(dead_code)] // nest/inner.rs's re-open (M8-NEST's D1018 hook row) and nest.subplan (M8-SUB B2) use it
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Rerun {
    pub set: usize,
    pub h: u16,
    pub resets: crate::lifetime::RunResets,
    pub runs: bool,
}

/// The owner's declaration at its make, in pipeline `k` (the one whose code calls `m8_rerun`): the rerun set of the nest at
/// plan node `node_id` (`plan_id` 0) or of the nested SubPlan `plan_id` attached there, `None` when decompose declared
/// none (every pipeline of the inner is built once). Declared once per set: a second call (a re-made inner) gets the same
/// handle. Every read of a slot a member produces is `k`'s or a later member's (RT-2e: only the owner opens over a set
/// that has not run yet), else a Shape by name. A member another rerun set also runs refuses by D_RERUN_SHARED (`shared`).
pub(crate) fn declare(px: &mut Px<'_, '_>, k: usize, node_id: i32, plan_id: i32) -> Option<Rerun> {
    let set = px.gx.graph.reruns.iter().position(|r| r.node_id == node_id && r.plan_id == plan_id)?;
    Some(Rerun::declare(px, k, set))
}

impl Rerun {
    /// `declare` of graph.reruns entry `set` itself: the key `(node, plan)` is not unique where decompose maps a SubPlan
    /// once per reference (`sub_set_index`: a grouping-sets emit's copies, rr-0069), so a SubPlan's owner declares the copy
    /// it reads.
    pub(crate) fn declare(px: &mut Px<'_, '_>, k: usize, set: usize) -> Rerun {
        let gx = px.gx;
        let node_id = gx.graph.reruns[set].node_id;
        if let Some(p) = px.protos.iter().find(|p| p.kind == ProtoKind::Rerun && gravity_program::lifetime::rerun_set_of(p.arg) == set as u64) {
            let h = p.words.base;
            return Rerun { set, h, resets: px.lifetimes.owner_run(set as u16), runs: true };
        }
        let members: PgVec<'_, usize> = gx.graph.reruns[set].pipelines.iter().map(|&q| q as usize).collect_in(px.mcx());
        let ps = &gx.graph.pipelines;
        // by life, not slot number: a slot an earlier pipeline's life left (an InitPlan's aggregate inside the inner) may be
        // refilled by a member, and that pipeline's read is of its own life
        let made = |q: usize, s: u8| gx.life_of(q, s).is_some_and(|l| members.contains(&(l.0 as usize)));
        // D2034: a recursive union's pulled seed fills its working table, which the union's term set (same owner, the driver)
        // reads; the driver starts the first generation only once the seed set ended (setop/recursive.rs `seed_pull`)
        let term = (gx.graph.reruns[set].plan_id == RECURSIVE_SEED_SET).then(|| gx.graph.reruns.iter().find(|r| r.node_id == node_id && r.plan_id == RECURSIVE_SET)).flatten();
        let term_reads = |q: usize| term.is_some_and(|r| r.pipelines.contains(&(q as u16)));
        // a graph pipeline embedding `k` (the root of a nest whose inner holds a SubPlan owner) lists `k`'s reads as its own
        if (0..ps.len()).any(|q| q != k && !members.contains(&q) && !term_reads(q) && !embeds(gx, q, k) && ps[q].desc.consumes.iter().any(|&s| made(q, s))) {
            crate::refuse!(node_id, Reason::Shape { why: ShapeWhy("a rerun set's slot read outside its owner and its members") });
        }
        // D1654 / D1700: a lazy build (D824: a VOLATILE build side on PG's outer-first path) runs at its probe's first row at
        // every re-run (driver `run_nested`), PG's rescan of a LEFT / ANTI hash join (HJ_FILL_OUTER fetches the outer's first
        // row before every build). On the cost path (INNER / SEMI) PG's rescan builds first after an outer with a row, and
        // keeps that over an empty table (hj_OuterNotEmpty): refused (D1701). The probe was made before the owner: `px.lazy`.
        // inherit#404: a build and probe both in `graph.kept` (the fill of a hashed Agg PG keeps, decompose `kept_hashed`) run
        // at the set's first run only (exec `again` skips them): PG's first ExecAgg runs the join once, outer first, never
        // rescanned (ExecReScanAgg keeps the table and does not rescan its input), so no D1701 rescan exists to differ.
        let lazy_slot = |s: u8, q: usize| px.lazy.iter().any(|&(b, p)| p == q && ps[b].desc.produces() == Some(s));
        let fill = |jt: &JoinType| matches!(jt, JoinType::JOIN_LEFT | JoinType::JOIN_ANTI);
        let cost = |q: usize| ps[q].ops.iter().any(|o| matches!(o, Op::Join(JoinDesc::Probe { jointype, slot, outer_first: true, .. }) if lazy_slot(*slot, q) && !fill(jointype)));
        let once = |p: usize| gx.graph.kept.contains(&(p as u16));
        // D3374: D1701 cannot show where the probe's outer side holds the same rows at every call (`steady`): PG then builds
        // at every call or at none, the lazy build at each call's first row or never, and a quiet outer side shows no order
        if members.iter().any(|&m| px.lazy.iter().any(|&(b, q)| b == m && cost(q) && !(once(b) && once(q)) && !steady(gx, b, q))) {
            crate::refuse!(node_id, Reason::Shape { why: ShapeWhy("a lazy hash build (a VOLATILE build side on PG's outer-first path, D824) in a rerun set (D1654)") });
        }
        // D1337 row B, D2300: a member another rerun set also runs (membership only, for every owner) would run under that set's
        // `m8_rerun` before this owner wrote the parameters it reads; checked after the set's own Shapes above, which keep their names
        if shared_at(gx, Some(set), &gx.graph.reruns[set].pipelines) {
            crate::refuse!(node_id, Reason::Shape { why: ShapeWhy(D_RERUN_SHARED) });
        }
        let (h, owner) = (px.proto_words(1), px.ident(k) as u16);
        // setop_rec-0043: a member of a CTE the set re-arms (`rearmed`) that another set runs (a recursive union's term, which
        // the union's driver re-runs per generation inside the CTE's pull) is that set's alone: no proto here
        // a member of a set nested in this one (`nested`) is that set's to run, at its owner's calls: no proto here either
        let set_ms = &gx.graph.reruns[set].pipelines;
        // D2300 (three recursive levels): a set holding every member of this one and more (the outer union's term set around
        // this middle one) re-arms the middle union's CTE and runs none of this set's members: it is no other runner here, so
        // the inner CTE this set re-arms (`rearming_set`) and its pulled seed keep their protos, as an outer term's do
        let outer = |i: usize| gx.graph.reruns[i].pipelines.len() > set_ms.len() && set_ms.iter().all(|m| gx.graph.reruns[i].pipelines.contains(m));
        let other = |q: usize| gx.graph.reruns.iter().enumerate().any(|(i, r)| i != set && r.pipelines.contains(&(q as u16)) && !outer(i));
        let mut runs = false;
        for q in members.into_iter().filter(|&q| !(rearmed(gx, set_ms, q) && other(q)) && !nested_away(gx, set, q as u16)) {
            // D1018 item 9 (b): a member PG keeps (graph.kept) runs at the set's first run only
            let kept = gx.graph.kept.contains(&(q as u16));
            px.declare_proto(Proto::new(ProtoKind::Rerun, q as u16).peer(owner).words(h, 1).arg(gravity_program::lifetime::rerun_arg(set, kept)));
            runs = true;
        }
        Rerun { set, h, resets: px.lifetimes.owner_run(set as u16), runs }
    }
}

/// D2611: whether rerun set `set` holds an InitPlan's own pipeline (non-CTE), which its run re-arms (D1262; exec rerun.rs
/// `inits_of` reads the same membership from the Init protos).
pub(crate) fn holds_init(gx: &crate::graph::Gx<'_, '_>, set: usize) -> bool {
    let ps = &gx.graph.reruns[set].pipelines;
    gx.graph.init_plans.iter().any(|i| !matches!(i.link, SubLink::Cte) && ps.contains(&i.pipeline))
}

/// D1337(A): whether every member of a nested SubPlan's rerun set `set` (owner `k`) is re-run by a proven road: each reader
/// of the slot a member produces is the slot's plain aggregate emit (fold.plain) or a sort run's rows (runs.sort, D1339 row
/// B: a member's, an ordered aggregate's feed re-run with the set, or `k`'s own inner's, `emit_ok`; never a grouping sort's
/// emit: groups), the emits nest/inner.rs re-opens under a rerun, or, for a hash build, a set member probing it (a join
/// probe op) or a member drain of its unmarked records (the build its source, the probe's spool its `matches`; over a build
/// kept outside the set, the marking probe clears its marks at each open: D2592, ops/hashjoin/probe_marks.rs). The runtime
/// resets the build's region (its marks with it) at every run and lays the table out again, and the probe member re-opens
/// after it, then the drain, whose cursor opens at record 0 (ops/hashjoin/drain/source.rs). The owner
/// `k`, which lists its embedded inner's slots, is not a reader here; the embedded inners are (`Pipes::end`): D1337(H), the
/// SubPlan's own inner probing the build (its re-open runs the set first: `owner_set`), is read as a member; any other inner
/// probing it stays refused here.
pub(crate) fn members_close(gx: &crate::graph::Gx<'_, '_>, k: usize, set: &[u16]) -> bool {
    let (ps, ins) = (gx.pipelines(), gx.pipelines().inners());
    // `q` is `k` or an inner `k` embeds (its op, or an inner it embeds)
    let of_k = |mut q: usize| loop {
        match q.checked_sub(ps.len()) {
            _ if q == k => break true,
            Some(i) => q = ins[i].0 .0,
            None => break false,
        }
    };
    // the inner of a join's drain (a Hash Right / Full Join's tail): the arms below that read the owner's region by
    // `Pipes::consumed_at` refuse there (D2331's class); a run reads the drain's own declared list (`run_region`)
    let mut call = k;
    while let Some(i) = call.checked_sub(ps.len()) {
        call = ins[i].0 .0;
    }
    let drain = ps[call].ops.iter().any(|o| matches!(o, Op::Join(JoinDesc::Drain { .. })));
    let own = own_of(gx, k, set);
    // `q` is a member of `set` or an inner one embeds (its op, or an inner it embeds)
    let in_member = |mut q: usize| loop {
        match q.checked_sub(ps.len()) {
            Some(i) => q = ins[i].0 .0,
            None => break set.contains(&(q as u16)),
        }
    };
    set.iter().all(|&m| {
        // D1107 (material): a plan Material's eager refill under this SubPlan (rerun/spool/form.rs `refill_member`): the fill
        // empties its record at each run, and its reader inside the SubPlan's own inner re-reads it from 0
        if crate::rerun::spool::form::refill_member(gx, m as usize) {
            return true;
        }
        // D1262 / setop_rec-0043: a member the set's run only re-arms (`rearm_only`) runs again at its next read
        if rearm_only(gx, set, m as usize) {
            return true;
        }
        // a member a set nested in this one runs (`nested`): that set's owner, a member here, re-runs it at each of its
        // calls, and that owner's own make closes its readers
        if own.is_some_and(|o| nested_away(gx, o, m)) {
            return true;
        }
        // S2b (rerun/inner_arms.rs): a feed of the settled union the owner's own inner streams never runs; that inner opens
        // its arm again at every call, as PG's Append rescans its children (`inner_arms::rerun` declares the set, D2016 for
        // any member past the feeds; regress updatable_views#858: a security-barrier view's correlated LIMIT over a parent).
        // D3374: so is one an inner embedded in a member streams (a Nested Loop Semi Join straight over the Append inside
        // the SubPlan's plan): the member runs at every call, and its nest opens the arms again at each re-open, over the
        // parameters the call wrote, as PG's ExecReScanAppend per outer row
        if crate::pipes::inner_union(gx, m as usize).is_some_and(|u| of_k(u.0) || in_member(u.0)) {
            return true;
        }
        let Some(s) = ps[m as usize].desc.produces() else { return false };
        let build = crate::frame::iface::end_breaker(gx, m as usize) == Some(BreakerKind::HashBuild);
        // by life, not slot number (as `declare`): decompose reuses a slot's number at its last reader, so a later member's
        // slot (an ordered aggregate's record over the sort of a member join) may carry the build's number. A union's arms
        // write one production whose life is the last arm's (`Gx::life_of`, `run_arms`): an arm's reader reads that life
        let arms = gx.run_arms(m as usize);
        // D1483: a MergeAppend's arms each put a run into the merge slot, whose reader reads it as the last arm's life (an
        // inner's merge: ops/append/merge_admit.rs `Merge::of_arm`)
        let merged = crate::props::Merge::of_arm(gx, m as usize);
        let arm = |x: usize| arms.contains(&x) || merged.as_ref().is_some_and(|g| g.arms.contains(&x));
        let mut readers = (0..ps.end()).filter(|&q| q != k && !embeds(gx, q, k) && ps.get(q).is_some_and(|p| p.desc.consumes.contains(&s)) && gx.life_of(q, s).is_none_or(|l| l.0 == m || arm(l.0 as usize))).peekable();
        // D3734: a member, or a nest's inner a member embeds (a Hash Right Join's drain as a Nested Loop's inner inside the
        // SubPlan's plan: drain/lower.rs `inner_spooled`, re-read at every re-open of the nest, the kept table's marks
        // cleared by the marking probe at each open, D2592)
        let tail = |q: usize| set.contains(&(q as u16)) || q >= ps.len() && in_member(q);
        // D4105 (S3 rkeep ra-hx8/m1-nl-left-exists: an EXISTS over a Hash Right Anti Join): or the SubPlan's own inner, as
        // D1337(H)'s probe arm reads it (`sub_set`), the drain straight under the SubPlan; never the inner of a join's drain
        // (D2331's class). Its re-open runs the set first (`owner_set`: its source is the set's build), so the drain reads the
        // table the run laid out again, its marks cleared, from record 0 (drain/source.rs's per-open cursor), as PG's
        // ExecReScanHashJoin rebuilds a param-changed build and resets its match flags; a spooled drain there stays N3's
        let own_inner = |q: usize| !drain && sub_set(gx, q).is_some_and(|r| r.pipelines[..] == *set);
        // D3734: a member that reads `s` only through such a nest's inner (`desc.consumes` carries its inner's slots); the
        // inner is a reader of its own here
        let direct = |q: usize| {
            matches!(ps[q].source, Source::Breaker { slot } if slot == s)
                || ps[q].ops.iter().any(|op| match op {
                    Op::Join(JoinDesc::Probe { slot, .. } | JoinDesc::Merge { slot, .. }) => *slot == s,
                    Op::Join(JoinDesc::Drain { matches, .. }) => *matches == s,
                    Op::SubPlan(d) => matches!(d.form, SubForm::Hashed { slot } if slot == s),
                    _ => false,
                })
        };
        let via_inner = |q: usize| {
            set.contains(&(q as u16)) && !direct(q) && ps[q].ops.iter().enumerate().any(|(j, o)| matches!(o, Op::Join(JoinDesc::Nest { .. })) && ps.inner_of(q, j).is_some_and(|x| ps.get(x).is_some_and(|p| p.desc.consumes.contains(&s))))
        };
        readers.peek().is_some()
            && readers.all(|q| via_inner(q) || match build {
                true => (set.contains(&(q as u16)) || sub_set(gx, q).is_some_and(|r| r.pipelines[..] == *set)) && !matches!(ps[q].source, Source::Breaker { slot } if slot == s) && ps[q].ops.iter().any(|op| matches!(op, Op::Join(JoinDesc::Probe { slot, .. }) if *slot == s))
                    // a member drain, a set member's straight tail (a member's drain is never spooled: drain/spool_admit.rs N3,
                    // and its build never spills: spill/build.rs)
                    || (tail(q) || own_inner(q)) && matches!(ps[q].source, Source::Breaker { slot } if slot == s) && matches!(ps[q].ops.first(), Some(Op::Join(JoinDesc::Drain { .. }))),
                // the probe's spool read by a member drain: the probe member runs the drain's ops into the drain's end. Over a
                // build kept outside the set too: the marking probe clears the kept table's marks at each open (D2592,
                // ops/hashjoin/probe_marks.rs), as PG's rescan resets its match flags (ExecHashTableResetMatchFlags)
                false if (tail(q) || own_inner(q)) && ps[q].ops.iter().any(|op| matches!(op, Op::Join(JoinDesc::Drain { matches, .. }) if *matches == s)) => true,
                // a hashed SubPlan's set probed by a member's step or the owner's own inner's: an eager set empties at its
                // member's re-open and builds again in the run (ops/subplan/hashed.rs `Build::open`), a LAZY one is re-armed
                // by the run (exec subplan/raw.rs `rearm_sets`) and builds at its next probe, as PG's ExecHashSubPlan rebuilds the
                // set at its first probe after a rescan that changed a parameter its plan reads
                false if ps[q].ops.iter().any(|op| matches!(op, Op::SubPlan(d) if matches!(d.form, SubForm::Hashed { slot } if slot == s))) => {
                    set.contains(&(q as u16)) || of_k(q) && !drain
                }
                // a set operation's two groups, both filled by members, read by its emit (the left group its source, a keyed
                // emit gathered fresh at each open; the right consumed whole): the setop.setop protocol files the copy counts
                // after the later fill in every run (exec ops/setop/proto.rs `at`), as ExecReScanSetOp rebuilds the table from
                // both children (a nest inner's: rerun/inner.rs `set_op`)
                false if set_op_side(gx, q, s, set) => set.contains(&(q as u16)) || of_k(q) && !drain,
                // errorhold (D1337, a correlated aggregate SubPlan over a merge join): a member Sort's run read as a merge
                // join's inner by the Merge op of a member or of the owner's own inner: the op re-opens its state words and
                // reads the sealed run at each window (ops/mergejoin/lower.rs `open_words` / `window`), as PG's
                // ExecReScanMergeJoin re-reads its re-sorted inner. A lazy inner (D824: a filler that may act) is re-armed at
                // every run (driver `run_nested`, as a lazy hash build's, D1654) and sorts again at the merge's INITIALIZE_INNER,
                // where PG's ExecReScanSort re-sorts a param-changed inner at its next fetch; a pulled one is never in a set
                // (mergejoin/exact_inner.rs `form`)
                false if ps[q].ops.iter().any(|op| matches!(op, Op::Join(JoinDesc::Merge { slot, .. }) if *slot == s)) => {
                    (set.contains(&(q as u16)) || of_k(q) && !drain)
                        && gx.end_kind(m as usize) == crate::graph::EndKind::Breaker(BreakerKind::Sort)
                }
                false => matches!(ps[q].source, Source::Breaker { slot } if slot == s) && match gx.source_kind(q) {
                    KindId::FoldPlain => true,
                    // a member's run holds rows, never a grouping sort's records: ops/sort/shapes.rs takes no form-G group
                    // sort for a member of a nest's or SubPlan's set (`in_owner_set`)
                    // C-R6f: a top-N's run (runs.topk_rows) is read by the same RunEmit; its bound reads per open (D1634).
                    // D1483: a MergeAppend's merged run (runs.mergeappend), the arms members: the same RunEmit
                    // (ops/append/merge.rs `make_emit`), its held runs merged again at the last arm's end in every run.
                    // D2331: a full sort's run under a join's drain too, at the drain's declared region (`run_region`), read by
                    // projections and filters, and (rnd-3692) by a LIMIT / OFFSET, whose count and offset words restart at each
                    // re-open of the drain's remade inner (D2675), as a top-N's run (its bound per open, D1634) is read
                    KindId::RunsSort | KindId::RunsTopkRows => set.contains(&(q as u16)) || of_k(q) && (!drain || ps[q].ops.iter().all(|o| matches!(crate::graph::step_kind(o), KindId::StepProject | KindId::StepFilter | KindId::StepLimit))),
                    KindId::RunsMergeappend => set.contains(&(q as u16)) || of_k(q) && !drain,
                    // D1339 row C (ported, proto4): a keyed group's emit, a member's (fresh at each run) or the owner's own
                    // inner's (fresh at the owner's region, table/emit_group.rs `at`); never the inner of a join's drain (D2331's
                    // class, as row B's run)
                    // a sorted group's or Unique's emit (runs.group / runs.unique, form K) is the same keyed emit, its gathered
                    // array sorted in the open (`emit_ok`'s row C arm): a set operation's left side over its sorted input
                    KindId::KeyedEmitGroup | KindId::RunsGroup | KindId::RunsUnique => set.contains(&(q as u16)) || of_k(q) && !drain,
                    // D1385's eager form: a CTE's store read by a CteScan the set re-runs after the CTE's pipeline (whose end
                    // zeroes the store at each run; ops/ctescan/lower.rs `eager` refuses every other reader by D1385's name)
                    KindId::NestCte => set.contains(&(q as u16)),
                    // a member's window reader over a member's fill (the Materialize under the WindowAgg): each open of
                    // either restarts the window object, the fill's dropping its stored rows (ops/window/rows.rs
                    // `m10_win_open`, PG's ExecReScanWindowAgg; D2346, window W2), and the set runs both at every call
                    // material W-INNER: or the SubPlan's own inner (`of_k`), re-opened after the set ran at every call
                    KindId::WindowAgg => set.contains(&(q as u16)) || of_k(q) && !drain,
                    // a count(DISTINCT) emit (keyed.distinct_plain) the owner's own inner reads at its region among the owner's
                    // consumed ones, the region its gather reads (table/emit.rs `pairs_total` / `agg_gather` at region 0, else
                    // host_abi `pairs_total_at` / `agg_gather_at`: ops/group/distinct_plain.rs `at`), gathered fresh at every open
                    // over the table the set rebuilt (`emit_ok`); under a join's drain at the tail's declared region (`run_region`,
                    // as D2331's run and fold/plain_drain.rs's record), read whole by projections and filters (gen rnd-0890)
                    KindId::KeyedDistinctPlain => of_k(q) && ps.consumed_at(q, s).is_some() && (!drain || ps[q].ops.iter().all(|o| matches!(o, Op::Project(_) | Op::Filter(_)))),
                    _ => false,
                },
            })
    })
}

/// D2331: the region embedded inner `k` reads its set's run `slot` at (ops/sort/lower.rs `RunEmit`). Where the running
/// call declares its own list (frame/tail.rs `Tail`: a spooled drain's, its spool slot moved last, ops/hashjoin/drain/
/// spool.rs `spool_drain_source`; a fused drain's probe tail), the slot's index there: `Pipes::consumed_at` (the graph
/// pipeline's decompose list) read the spool's region past the spool's place, so the inner of a Hash Right Join's tail
/// read an empty run (the psql domain describe: an ARRAY ... ORDER BY SubPlan in the join's projection). Else
/// `Pipes::consumed_at`, as before. A LAZY hashed SubPlan's set slot before it in the declared list (frame/program.rs
/// drops it from the program descriptor, D2325) refuses by D_RERUN_EMIT, as fold/plain_drain.rs `region` does.
pub(crate) fn run_region(px: &Px<'_, '_>, k: usize, slot: u8) -> Option<usize> {
    let ps = px.gx.pipelines();
    let run = px.ident(k);
    // D2743: the running call's index, past the LAZY hashed SubPlan sets frame/program.rs drops from the list (`Px::consumed`,
    // D2325; `Pipes::consumed_at`'s index there made a sort run's RunEmit read another region and crash)
    let Some(t) = px.tails.iter().find(|t| t.ident == run) else { return px.consumed(k, slot) };
    let at = t.consumes.iter().position(|&s| s == slot)?;
    let lazy = |s: u8| {
        let life = px.gx.life_of(run, s).map(|l| l.0 as usize);
        px.protos.iter().any(|p| p.kind == ProtoKind::SubHash && p.arg & SUBHASH_LAZY != 0 && Some(p.pipeline as usize) == life)
    };
    // a pass's virtual id (frame/program.rs `pass_id`) names no graph pipeline: its call's regions are not proven here
    if run >= ps.end() || t.consumes[..at].iter().any(|&s| lazy(s)) {
        crate::refuse!(ps[k].node_id, Reason::Shape { why: ShapeWhy(D_RERUN_EMIT) })
    }
    Some(at)
}

/// Whether pipeline `q` reads slot `s` as a group of the set operation it emits (its first op, over its left group), both
/// groups filled by members of `set`.
fn set_op_side(gx: &crate::graph::Gx<'_, '_>, q: usize, s: u8, set: &[u16]) -> bool {
    let Some(Op::SetOp(gravity_decompose::ops::shared::setop::SetDesc::Set { left, right, .. })) = gx.pipelines().get(q).and_then(|p| p.ops.first()) else { return false };
    let member = |t: u8| gx.life_of(q, t).is_some_and(|l| set.contains(&l.0));
    (s == *left || s == *right) && matches!(gx.pipelines()[q].source, Source::Breaker { slot } if slot == *left) && member(*left) && member(*right)
}

/// D1339 (M8-NEST): a rerun set's breaker emit other than a plain aggregate's on the owner's first region (its emits' region and resume reads are the owner's).
pub(crate) const D_RERUN_EMIT: &str = "a nest inner over a rerun set's breaker whose emit reads the owner's region (a run, groups, or a slot past the owner's first) (D1339)";

/// The emit of kind `kind` an owner's re-open reads as its inner `ik` (the inner of pipeline `k`, over a breaker of the
/// owner's rerun set; nest/inner.rs `make`): a plain aggregate's one record read as the owner's first consumed region
/// (fold/slots.rs `record_at`), or (D1339 row B) a sort run's rows, a full sort's or a top-N's, never a grouping sort's
/// (groups), read at the owner's consumed region with the owner's resume word kept (runs/sort.rs `RunEmit`,
/// `Pipes::consumed_at`), or (D1339 row C) a keyed group's emit, a hashed, sorted (runs.group) or Unique (runs.unique)
/// group's, gathered fresh at the owner's consumed region (table/emit_group.rs `at`), or a count(DISTINCT) emit
/// (keyed.distinct_plain) at the owner's first consumed region, the one its gather reads. Any other emit reads
/// the owner's own region or first region and its resume word (host_abi `agg_gather`): D_RERUN_EMIT.
pub(crate) fn emit_ok(px: &Px<'_, '_>, (k, ik): (usize, usize), kind: KindId) -> KindId {
    let gx = px.gx;
    let ps = gx.pipelines();
    let slot = match ps[ik].source {
        Source::Breaker { slot } => slot,
        _ => u8::MAX,
    };
    let ok = match kind {
        // D1339 row D (ported, proto4): any region the graph pipeline at the root of the owner's chain consumes the slot at
        // (`Pipes::consumed_at`, an inner owner's too: sp-0123; fold/plain_drain.rs `region`, which refuses by this name where
        // the running call's list differs)
        KindId::FoldPlain => ps[k].desc.consumes.first() == Some(&slot) || ps.consumed_at(k, slot).is_some(),
        // the run's producer is a member of an owner's set, so its run holds rows (no form-G group sort, `in_owner_set`);
        // a top-N's run (runs.topk_rows) is read by the same RunEmit at the owner's region, its bound per open (D1634), a set
        // member never takes the late fetch (ops/sort/strategies/fetch.rs `of`), nor code.heads (D3844: dict/strategies/heads.rs `key`); a MergeAppend's
        // merged run (D1483: runs.mergeappend, its arms the set's) is read by the same RunEmit
        KindId::RunsSort | KindId::RunsTopkRows | KindId::RunsMergeappend => gx.life_of(ik, slot).is_some_and(|l| in_owner_set(gx, l.0 as usize)),
        // D1339 row C (ported, proto4): keyed/emit_group.rs `at`; under an owner a set re-runs too (a nested set's inner,
        // decorrelate-014): its array's cache is a statement pin (kernels table/emit.rs `kept_array`, `PIN_STATEMENT`),
        // never that set's, so the set's next run does not unpin it under the cache word. A sorted group's or Unique's
        // emit (runs.group / runs.unique, form K) is the same keyed emit (ops/group/lower.rs, unique.rs `make_emit`), its
        // gathered array sorted by the group keys in the open (order_slot.rs `run`)
        // table/emit_group.rs gathers at `run_region`'s index (D3545: a spooled drain's tail at its declared list's); outside
        // a tail that is `Pipes::consumed_at`'s, never past a LAZY hashed SubPlan's set, whose slot leaves the call's list
        // after lowering (`Px::consumed` skips it; D2743: it raised XX000 there)
        KindId::KeyedEmitGroup | KindId::RunsGroup | KindId::RunsUnique => ps.consumed_at(ik, slot).is_some_and(|at| px.consumed(ik, slot) == Some(at)),
        // a count(DISTINCT) emit (ops/group/distinct_plain.rs): its gather reads consumed[0] with the resume word NONE around
        // the call (fresh at every open), or an inner's later region by index (`at`: two SubPlans in one select list), as
        // row D's plain record
        KindId::KeyedDistinctPlain => ps[k].desc.consumes.first() == Some(&slot) || ps.consumed_at(k, slot).is_some(),
        KindId::JoinDrain => true,
        // material W-INNER: a WindowAgg inner reads its partition buffer through the window object (ops/window/rows.rs), never
        // a region; its fill a member of an owner's set, each open of either restarts the object (D2346)
        KindId::WindowAgg => gx.life_of(ik, slot).is_some_and(|l| in_owner_set(gx, l.0 as usize)),
        _ => false,
    };
    // the rows D / C arms (not v4's first-region record); a set member holding a window op re-runs over a window object
    // each open restarts (D2346, window W2: ops/window/rows.rs `m10_win_open`), so the window guard these arms had is gone.
    // For a keyed emit (keyed/emit_group.rs, runs.group, runs.unique) two stops of the owner's call mid-row are no refusal:
    // D3883: a lazy build that runs inside the owner's region (D824's group: a VOLATILE hash build on PG's outer-first
    // path, D3014's keyed build-once over a side that may raise, a merge's lazy inner, a lazy spool). Its wait stops the
    // owner's call and the driver re-enters it where it stopped (`BuildWait`): the probe's end keeps its state in frame
    // words (D2147's root fix, the class D2194 / D2201 / D2253 named), the keyed emit's gathered array is a statement pin
    // behind its cache word, and the wait's reattach keeps the held call's pins under its floor (D2641, driver
    // node/mod.rs), so the re-read resumes over the same groups, as PostgreSQL's ExecHashJoin builds inside the call that
    // fetched the row.
    // D3904: an owner inside a CTE's pulled plan (ops/ctescan/lower.rs `fed`). A pull that stops the owner mid-inner parks
    // its call (`Entry::pull_nested`), and the reattach at the reader's depth keeps every pin the call took
    // (`keep_nested`): the set's filed pins stay the set's (D2751, driver node/pull.rs `unheld`), unpinned only at the
    // set's next run, which the resumed call reaches after the emit, or traded from under a later pull's floor (D2641);
    // the gathered array is a statement pin, and the parked call keeps its arena window (D3244). The resumed emit reads
    // the same groups, as PostgreSQL's CteScan resumes the CTE's plan inside the reader's fetch
    if !ok {
        crate::refuse!(ps[ik].node_id, Reason::Shape { why: ShapeWhy(D_RERUN_EMIT) });
    }
    kind
}

/// D1339 row C: whether pipeline `k` is a member of a nest's or nested SubPlan's rerun set (not a recursive union's term
/// set, `RECURSIVE_SET`): such a member re-runs alone, so it takes no HAVING split (ops/sort/topn/strategies/twopass.rs: a
/// remade pass-2 pipeline no Rerun proto re-runs) and no form-G group sort (ops/sort/shapes.rs).
pub(crate) fn in_owner_set(gx: &crate::graph::Gx<'_, '_>, k: usize) -> bool {
    gx.graph.reruns.iter().any(|r| r.plan_id != gravity_decompose::ops::shared::setop::RECURSIVE_SET && r.pipelines.contains(&(k as u16)))
}

/// D2615: whether pipeline `k` is a member of a recursive union's term set (`RECURSIVE_SET`: the driver re-runs it per
/// generation, ops/setop/recursive.rs), the complement of `in_owner_set`'s sets.
pub(crate) fn in_term_set(gx: &crate::graph::Gx<'_, '_>, k: usize) -> bool {
    gx.graph.reruns.iter().any(|r| r.plan_id == gravity_decompose::ops::shared::setop::RECURSIVE_SET && r.pipelines.contains(&(k as u16)))
}

/// The graph pipeline embedding pipeline `q` (`q` itself when it is one).
fn root(gx: &crate::graph::Gx<'_, '_>, mut q: usize) -> usize {
    let ps = gx.pipelines();
    while let Some(i) = q.checked_sub(ps.len()) {
        q = ps.inners()[i].0 .0;
    }
    q
}

/// Whether pipeline `q` embeds pipeline `k` (`k` is an inner of `q`, or of an inner `q` embeds; never `q == k`): a pipeline
/// lists the slots its embedded inners read among its own (`Pipes::end`), so the root of a nest whose inner holds a
/// SubPlan "reads" that SubPlan's set through `k`; the reads are `k`'s and its inners'.
fn embeds(gx: &crate::graph::Gx<'_, '_>, q: usize, mut k: usize) -> bool {
    let ps = gx.pipelines();
    while let Some(i) = k.checked_sub(ps.len()) {
        k = ps.inners()[i].0 .0;
        if k == q {
            return true;
        }
    }
    false
}

/// D3374 (`declare`'s D1654 arm): lazy build `b`'s probe in pipeline `q` reads an outer side that holds the same rows at
/// every call of the set: `q`'s source a scan (heap, index) or a Result, its ops before the probe filters, gates and
/// projections, no slot read but its probes' builds, no executor parameter (probe.rs `reads_param`: D1902's "empty at
/// every call or at none"), the side quiet (`side_by`), and `b`'s side calling nothing (not UserFn, nothing `opaque`). PG
/// then builds at every call (outer first at the first, then first after a call with a row: hj_OuterNotEmpty) or at none,
/// as the lazy build runs at each call's first row or never; a raise there is an error against an error.
fn steady(gx: &crate::graph::Gx<'_, '_>, b: usize, q: usize) -> bool {
    use crate::ops::hashjoin::build_class::{side_by, Side};
    let ps = gx.pipelines();
    let p = &ps[q];
    let Some(j) = p.ops.iter().position(|o| matches!(o, Op::Join(JoinDesc::Probe { slot, outer_first: true, .. }) if ps[b].desc.produces() == Some(*slot))) else { return false };
    matches!(p.source, Source::Range { .. } | Source::Index { .. } | Source::Const)
        && p.ops[..j].iter().all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_)))
        && p.desc.consumes.iter().all(|s| p.ops[j..].iter().any(|o| matches!(o, Op::Join(JoinDesc::Probe { slot, .. }) if slot == s)))
        && !crate::ops::hashjoin::probe::reads_param(gx, q, j)
        && side_by(gx, q, j, false) == Side::Safe
        && side_by(gx, b, usize::MAX, false) != Side::UserFn
        && !crate::ops::hashjoin::probe::opaque(gx, b)
}

/// D1337(H): the rerun set of the nested SubPlan whose embedded inner is `ik` (`sub_set_at`, as `owner_set` keys it;
/// ops/hashjoin/probe.rs restates it for its prefix_safe: keep the two equal).
pub(crate) fn sub_set<'g>(gx: &crate::graph::Gx<'g, '_>, ik: usize) -> Option<&'g gravity_decompose::graph::Rerun> {
    let ((k, j), _) = *gx.pipelines().inners().get(ik.checked_sub(gx.pipelines().len())?)?;
    let Some(Op::SubPlan(d)) = gx.pipelines()[k].ops.get(j) else { return None };
    sub_set_at(gx, k, j, d.plan_id)
}

/// The rerun set of SubPlan op `j` of pipeline `k` (plan `plan_id`): `sub_set_index`'s entry.
pub(crate) fn sub_set_at<'g>(gx: &crate::graph::Gx<'g, '_>, k: usize, j: usize, plan_id: i32) -> Option<&'g gravity_decompose::graph::Rerun> {
    sub_set_index(gx, k, j, plan_id).map(|i| &gx.graph.reruns[i])
}

/// The graph.reruns index of SubPlan op `j` of pipeline `k`'s set (plan `plan_id`). Decompose maps a SubPlan once per
/// reference (ops/subplan `map_subplan`: the inner planned and its set declared at each call), each copy its own set and
/// pipelines: one per plan node it is mapped at (an UPDATE / DELETE over a partitioned target computes its SET / WHERE
/// SubPlan in each leaf's scan: one set per Append arm, mt-0353), and one per reference at one node, all keyed by the same
/// (node, plan) (a grouping-sets aggregate's target list SubPlan, compiled in each grouping set's emit: rr-0069). So the
/// set at the op's node whose member produces a slot the op's embedded inner reads (by life: its source breaker, a
/// probe's build), else any copy the inner reads, else its own InitPlan copy (`init_copy`), else the set at the op's
/// node, else the plan's first (one place, the node of the op elsewhere).
pub(crate) fn sub_set_index(gx: &crate::graph::Gx<'_, '_>, k: usize, j: usize, plan_id: i32) -> Option<usize> {
    let ps = gx.pipelines();
    let node = crate::rerun::inner::op_node(&ps[k], j);
    let ik = ps.inner_of(k, j);
    let reads = |r: &gravity_decompose::graph::Rerun| ik.is_some_and(|ik| ps[ik].desc.consumes.iter().any(|&s| gx.life_of(ik, s).is_some_and(|l| r.pipelines.contains(&l.0))));
    let of = |f: &dyn Fn(&gravity_decompose::graph::Rerun) -> bool| gx.graph.reruns.iter().position(|r| r.plan_id == plan_id && f(r));
    of(&|r| r.node_id == node && reads(r)).or_else(|| of(&reads)).or_else(|| init_copy(gx, ik?, node, plan_id)).or_else(|| of(&|r| r.node_id == node)).or_else(|| of(&|_| true))
}

/// `sub_set_index` for copies at one node no slot read tells apart: each copy's members are all an InitPlan's (its
/// re-arm, `initplan::lower::member`), which the inner reads by parameter, and every copy's Init shares the parameter's
/// handle word (rerun/params.rs `init_handle`), so any one-to-one choice re-arms the one cell at each call. The i-th
/// reference at the node (embedded inners in order) declares the i-th copy, so every copy has its owner (frame/program.rs
/// refuses a set no owner declares: regress groupingsets#66, a grouping-sets emit's SubPlan holding an InitPlan). Any
/// other count of copies and references: none.
fn init_copy(gx: &crate::graph::Gx<'_, '_>, ik: usize, node: i32, plan_id: i32) -> Option<usize> {
    let ps = gx.pipelines();
    let init = |r: &gravity_decompose::graph::Rerun| !r.pipelines.is_empty() && r.pipelines.iter().all(|&q| crate::ops::subplan::initplan::lower::member(gx, q as usize));
    let copies: PgVec<'_, usize> = gx.graph.reruns.iter().enumerate().filter(|(_, r)| r.plan_id == plan_id && r.node_id == node && init(r)).map(|(i, _)| i).collect_in(gx.mcx);
    let at = |&(q, i): &(usize, usize)| matches!(ps[q].ops.get(i), Some(Op::SubPlan(d)) if d.plan_id == plan_id) && crate::rerun::inner::op_node(&ps[q], i) == node;
    let refs: PgVec<'_, usize> = ps.inners().iter().enumerate().filter(|(_, x)| at(&x.0)).map(|(e, _)| ps.len() + e).collect_in(gx.mcx);
    if copies.len() != refs.len() || gx.graph.reruns.iter().filter(|r| r.plan_id == plan_id && r.node_id == node).count() != copies.len() {
        return None;
    }
    copies.get(refs.iter().position(|&e| e == ik)?).copied()
}

/// `owner_set`'s graph.reruns index, the entry rerun/inner.rs `make` declares: the key (node, plan) is not unique where
/// decompose maps a SubPlan once per reference (`sub_set_index`), so the copy the SubPlan's inner reads.
pub(crate) fn owner_set_at(gx: &crate::graph::Gx<'_, '_>, ik: usize) -> Option<usize> {
    owner_set(gx, ik)?;
    let ps = gx.pipelines();
    let ((k, j), _) = *ps.inners().get(ik.checked_sub(ps.len())?)?;
    match ps[k].ops.get(j) {
        Some(Op::SubPlan(d)) => sub_set_index(gx, k, j, d.plan_id),
        _ => nest_set_index(gx, k, j),
    }
}

/// The graph.reruns index of the rerun set of nest op `j` of pipeline `k` (plan 0, keyed by the nest's node). A grouping-sets
/// aggregate's rewrite maps its child once per grouping set, each copy its own pipelines and set at the same node (heldout
/// dsx-0064: a ROLLUP over a union whose arm holds a nest over a union inner; the node's first set was declared by every
/// copy and the others by none, D1262), so the set at the node whose member produces a slot the nest's inner reads (by life,
/// as `sub_set_index` picks a SubPlan's copy), else the node's first.
pub(crate) fn nest_set_index(gx: &crate::graph::Gx<'_, '_>, k: usize, j: usize) -> Option<usize> {
    let ps = gx.pipelines();
    let node = crate::rerun::inner::op_node(&ps[k], j);
    let ik = ps.inner_of(k, j);
    let reads = |r: &gravity_decompose::graph::Rerun| ik.is_some_and(|ik| ps[ik].desc.consumes.iter().any(|&s| gx.life_of(ik, s).is_some_and(|l| r.pipelines.contains(&l.0))));
    let at = |r: &gravity_decompose::graph::Rerun| r.plan_id == 0 && r.node_id == node;
    gx.graph.reruns.iter().position(|r| at(r) && reads(r)).or_else(|| gx.graph.reruns.iter().position(at))
}

/// D1018 (a nest's own set): the rerun set of the nest whose inner is `ik` (plan 0, keyed by the nest's node as `owner_set`
/// keys it, `nest_set_index`; ops/hashjoin/probe.rs reads it for its prefix_safe).
pub(crate) fn nest_set<'g>(gx: &crate::graph::Gx<'g, '_>, ik: usize) -> Option<&'g gravity_decompose::graph::Rerun> {
    let ((k, j), _) = *gx.pipelines().inners().get(ik.checked_sub(gx.pipelines().len())?)?;
    let p = &gx.pipelines()[k];
    if matches!(p.ops.get(j), Some(Op::SubPlan(_))) {
        return None;
    }
    nest_set_index(gx, k, j).map(|i| &gx.graph.reruns[i])
}

/// D1903 / D1337(H): the key of the rerun set inner `ik`'s re-open runs (`Rerun::run`), one predicate for
/// rerun/inner.rs `make` and rerun/inner_limit.rs's LIMIT 0: a nest's (its node, plan 0) or the embedding SubPlan's, when the
/// inner's source is a breaker of the set or (D1337(H); D1018 for a nest's set, rerun/inner_probe.rs) a probe of the inner
/// reads a hash build of the set.
pub(crate) fn owner_set(gx: &crate::graph::Gx<'_, '_>, ik: usize) -> Option<(i32, i32)> {
    let ps = gx.pipelines();
    let ((k, j), ip) = *ps.inners().get(ik.checked_sub(ps.len())?)?;
    let r = match ps[k].ops.get(j) {
        Some(Op::SubPlan(_)) => sub_set(gx, ik)?,
        _ => &gx.graph.reruns[nest_set_index(gx, k, j)?],
    };
    // rangefuncs#223: a merge join's run the set refills (ops/mergejoin/nest_refill.rs) reads as a probe's build does
    let probe = |o: &Op<'_>| matches!(o, Op::Join(JoinDesc::Probe { slot, .. } | JoinDesc::Merge { slot, .. }) if gx.life_of(ik, *slot).is_some_and(|l| r.pipelines.contains(&l.0)));
    // D1262: a set holding an InitPlan's members re-arms it at every re-open, whatever the inner reads
    let init = || r.pipelines.iter().any(|&m| crate::ops::subplan::initplan::lower::member(gx, m as usize));
    // D1107 (material): a plan Material the set refills, read by a nest inner embedded under `ik` (the reader two or more
    // levels below the set's nest, join#782; or anywhere in a nested SubPlan's own inner, the psql trigger list's parent
    // SubPlan): the re-open or call runs the set before the inner that embeds the reader opens
    let deep = || deep_spool(gx, ik, &r.slots);
    // D1262 (subselect#196 / #197): a hashed SubPlan in the inner probing a set the run rebuilds (its plan reads the nest's
    // parameter, PG's ExecHashSubPlan rebuilds it at the first probe after such a rescan; members_close's hashed arm)
    let hashed = |o: &Op<'_>| matches!(o, Op::SubPlan(d) if matches!(d.form, SubForm::Hashed { slot } if gravity_decompose::graph::slot_has(&r.slots, slot)));
    (matches!(ip.source, Source::Breaker { slot } if gravity_decompose::graph::slot_has(&r.slots, slot)) || ip.ops.iter().any(probe) || ip.ops.iter().any(hashed) || init() || deep()).then_some((r.node_id, r.plan_id))
}

/// D1107: an inner embedded under inner `ik` at any depth reads, as nest.spool's reader, a breaker of `slots` (a plan
/// Material's spool a rerun set refills).
fn deep_spool(gx: &crate::graph::Gx<'_, '_>, ik: usize, slots: &gravity_decompose::graph::SlotSet) -> bool {
    let ps = gx.pipelines();
    let n = ps.len();
    let under = |mut q: usize| {
        while q >= n {
            q = ps.inners()[q - n].0 .0;
            if q == ik {
                return true;
            }
        }
        false
    };
    ps.inners().iter().enumerate().any(|(i, (_, ip))| {
        matches!(ip.source, Source::Breaker { slot } if gravity_decompose::graph::slot_has(slots, slot)) && under(n + i) && gx.source_kind(n + i) == KindId::NestSpool
    })
}

/// D2300 (a live SIGSEGV on trunk, not on the NO-WIDEN list: PLAN 09-30 item 6's in-row refusal, the r8 correctness lens
/// 16:27Z): a set D1337(A) newly serves whose member another rerun set also runs (a LATERAL nest's set holding the SubPlan's
/// inner) runs under that other owner's `m8_rerun` before this SubPlan wrote its parameters. The text is D1337 row B's
/// shared-member name byte for byte (main 16:52Z), so row B's every-owner check later refuses these forms by the same name.
pub(crate) const D_RERUN_SHARED: &str = "a rerun set whose member another rerun set also runs (one set's run reads the other owner's parameters before that owner writes them; D1337 rows B / N, D2300)";

/// The name the nested SubPlan at op `j` of pipeline `k` refuses by for its set `set` (v5 subplan 12: the NO-WIDEN names
/// leave the SubPlan's paths, D2194's class with them; D1339 row C's keyed emits since D3883 and D3904, `emit_ok`): a set
/// holding a hash build (only D1337(A) serves one) whose member another rerun set also runs (D2300, `shared`:
/// D_RERUN_SHARED, row B's text).
pub(crate) fn owner_refusal(px: &Px<'_, '_>, k: usize, set: &[u16]) -> Option<&'static str> {
    let gx = px.gx;
    let build = set.iter().any(|&m| crate::frame::iface::end_breaker(gx, m as usize) == Some(BreakerKind::HashBuild));
    (build && shared_at(gx, own_of(gx, k, set), set)).then_some(D_RERUN_SHARED)
}

/// The graph.reruns entry of the set `set` the op in pipeline `k` owns: among the entries with these members, the one whose
/// owner op sits in `k` (`owner_op`), else the first. Two SubPlans' sets can hold the same members (D1337: a SubPlan nested
/// in another's own inner, both re-running the aggregate's scan), so the members alone do not name the set.
fn own_of(gx: &crate::graph::Gx<'_, '_>, k: usize, set: &[u16]) -> Option<usize> {
    let rs = &gx.graph.reruns;
    let equal = |i: &usize| rs[*i].pipelines[..] == *set;
    (0..rs.len()).filter(equal).find(|&i| owner_op(gx, i).is_some_and(|(q, _)| q == k)).or_else(|| (0..rs.len()).find(equal))
}

/// D2300: a member of `set` is also a member of another graph.reruns entry (membership only, Row B's structural test, order
/// board 72); `set` itself (entry `own`: `own_of`, else its index) is excluded once, so a second entry with the same members counts. An
/// InitPlan's member (`initplan::lower::member`) is no shared run: every set holding it only re-arms its cell (exec rerun
/// `inits_of`), and the cell runs at its first read after every owner wrote its parameters, as PG's chgParam marks an
/// InitPlan from each changed parameter and evaluates it at the next read (regress join#784: InitPlans reading a LATERAL
/// nest's parameter and the SubPlan's own). A member of two nested sets (`nested`) is run by the inner one only.
fn shared_at(gx: &crate::graph::Gx<'_, '_>, own: Option<usize>, set: &[u16]) -> bool {
    let rs = &gx.graph.reruns[..];
    // a member either set only re-arms (`rearm_only`) is run by neither set's run, and one a set nested in the other holds
    // (`nested`) by the nested set's alone: no shared run
    let run = |s: &[u16], q: u16| !rearm_only(gx, s, q as usize);
    let apart = |i: usize, q: u16| own.is_some_and(|o| nested(gx, i, o, q) || nested(gx, o, i, q));
    rs.iter().enumerate().any(|(i, r)| Some(i) != own && r.pipelines.iter().any(|&q| set.contains(&q) && run(set, q) && run(&r.pipelines, q) && !apart(i, q)))
}

/// Whether rerun set `set`'s run only re-arms member `q`, never runs it: an InitPlan's member (D1262: it runs again at the
/// InitPlan's next read, PG's chgParam) or a member of a CTE the set re-arms (setop_rec-0043, `rearmed`: it runs again in
/// the CTE's next pull, PG's ExecReScanCteScan clearing the store).
pub(crate) fn rearm_only(gx: &crate::graph::Gx<'_, '_>, set: &[u16], q: usize) -> bool {
    crate::ops::subplan::initplan::lower::member(gx, q) || rearmed(gx, set, q)
}

/// setop_rec-0043: the rerun set (its index) that re-arms CTE pipeline `q` (ops/ctescan/lower.rs `rearms`: one nest's or
/// SubPlan's set re-runs its whole plan, or (with#45) a recursive union's term set holding another recursive union's CTE,
/// which PG's ExecReScanRecursiveUnion empties at every outer generation), if one does.
pub(crate) fn rearming_set(gx: &crate::graph::Gx<'_, '_>, q: usize) -> Option<usize> {
    let set = crate::ops::ctescan::lower::rearm_set(gx, q)?;
    crate::ops::ctescan::lower::rearms(gx, q).then_some(set)
}

/// setop_rec-0043: whether member `q` of rerun set `set` belongs to a CTE the set re-arms (ops/ctescan/lower.rs `rearms`: a
/// member CTE whose whole plan the set re-runs): the CTE's pipeline, its pulled producers and a recursive union's term set
/// inside it. The set's run re-arms the CTE (exec ops/ctescan/proto.rs `rearm`) and runs none of them, as it re-arms an
/// InitPlan; the CTE's next pull runs them again, nested in its reader, as PG's ExecReScanCteScan re-runs the plan lazily.
pub(crate) fn rearmed(gx: &crate::graph::Gx<'_, '_>, set: &[u16], q: usize) -> bool {
    let ctes = set.iter().map(|&m| m as usize).filter(|&k| crate::ops::ctescan::lower::rearms(gx, k));
    ctes.into_iter().any(|k| k == q || crate::ops::ctescan::lower::fed(gx, k).contains(&q))
}

/// Nested sets (D2300's exemption): rerun set `t` runs inside set `s`'s run when `t`'s owner is one of `s`'s members (a
/// correlated SubPlan inside another's plan, reading the outer one's parameter too), so member `q` both hold is `t`'s to
/// run, at each of its owner's calls, after that owner wrote its parameters; `s` never runs it. PG's ExecReScan of `s`'s
/// plan only adds the changed parameters to the nested SubPlan's chgParam, which re-executes at its next call. A member PG
/// keeps across `t`'s calls (graph.kept: run at `t`'s first run only) would miss `s`'s change: no exemption.
/// D1337 (decorrelate's level-2 reference, `SELECT (SELECT (SELECT p.c + count(*) FROM u WHERE u.y = t.b) FROM t WHERE
/// t.n = p.n) FROM p`): `t`'s owner op sits in the inner `s`'s owner re-opens after `s`'s run (`inside`: the outer
/// SubPlan's own inner, or an inner it embeds), so `t`'s every call follows `s`'s and writes `t`'s own parameters (t.b)
/// before `t`'s run; `s`'s run would run `q` before them (D2300's read). Not for a member either set only re-arms (an
/// InitPlan's, whose cell `s`'s inner may read before `t`'s first call), nor where a member of `s` outside `t` reads `q`'s
/// rows within `s`'s run.
fn nested(gx: &crate::graph::Gx<'_, '_>, t: usize, s: usize, q: u16) -> bool {
    let (rs, ms) = (&gx.graph.reruns, |x: usize| &gx.graph.reruns[x].pipelines);
    if t == s || gx.graph.kept.contains(&q) || !rs[t].pipelines.contains(&q) {
        return false;
    }
    if set_owner(gx, t).is_some_and(|o| ms(s).contains(&(o as u16)) && !ms(t).contains(&(o as u16))) {
        return true;
    }
    let ps = gx.pipelines();
    let fed_in_s = ps[q as usize].desc.produces().is_some_and(|sl| ms(s).iter().any(|&m| !ms(t).contains(&m) && ps.get(m as usize).is_some_and(|p| p.desc.consumes.contains(&sl))));
    inside(gx, t, s) && !rearm_only(gx, ms(s), q as usize) && !rearm_only(gx, ms(t), q as usize) && !fed_in_s && read_under(gx, t, q as usize)
}

/// Whether rerun set `t`'s owner op sits in the inner set `s`'s owner op re-opens after each run of `s` (`owner_inner`), or
/// in an inner that one embeds.
fn inside(gx: &crate::graph::Gx<'_, '_>, t: usize, s: usize) -> bool {
    let (Some((h, _)), Some(i)) = (owner_op(gx, t), owner_inner(gx, s)) else { return false };
    h == i || embeds(gx, i, h)
}

/// Whether every read of member `q`'s slot is `t`'s: a member of `t`, the inner `t`'s owner op re-opens or one it embeds,
/// or a pipeline holding that op (it or an outer one) that lists the slot only for the inner it embeds (no source, probe,
/// merge, drain or hashed SubPlan of its own over it): then `q`'s rows are read only after `t`'s run.
fn read_under(gx: &crate::graph::Gx<'_, '_>, t: usize, q: usize) -> bool {
    let ps = gx.pipelines();
    let (Some(sl), Some((h, _)), Some(i)) = (ps[q].desc.produces(), owner_op(gx, t), owner_inner(gx, t)) else { return false };
    let direct = |x: usize| {
        let p = &ps[x];
        matches!(p.source, Source::Breaker { slot } if slot == sl)
            || p.ops.iter().any(|op| matches!(op, Op::Join(JoinDesc::Probe { slot, .. } | JoinDesc::Merge { slot, .. } | JoinDesc::Drain { matches: slot, .. }) if *slot == sl) || matches!(op, Op::SubPlan(d) if matches!(d.form, SubForm::Hashed { slot } if slot == sl)))
    };
    (0..ps.end()).filter(|&x| ps.get(x).is_some_and(|p| p.desc.consumes.contains(&sl))).all(|x| {
        gx.graph.reruns[t].pipelines.contains(&(x as u16)) || x == i || embeds(gx, i, x) || (x == h || embeds(gx, x, h)) && !direct(x)
    })
}

/// The inner rerun set `set`'s owner op re-opens after each run: the SubPlan's own inner, or the nest's (`Pipes::end`
/// numbering); none for a set whose owner op embeds no inner.
fn owner_inner(gx: &crate::graph::Gx<'_, '_>, set: usize) -> Option<usize> {
    let at = owner_op(gx, set)?;
    let ps = gx.pipelines();
    ps.inners().iter().position(|x| x.0 == at).map(|i| ps.len() + i)
}

/// D3217: rerun set `r` is kept whole: every member in decompose `graph.kept` (D1018 item 9 (b): the fill of a hashed Agg
/// or a Sort a nest inner reads, kept by PostgreSQL's rescan that changes no param), so the set runs its members once, at
/// its first run (exec `again` skips them), as PostgreSQL runs them once.
pub(crate) fn kept_whole(gx: &crate::graph::Gx<'_, '_>, r: &gravity_decompose::graph::Rerun) -> bool {
    r.pipelines.iter().all(|p| gx.graph.kept.contains(p))
}

/// Whether set `s` leaves member `q` to a set nested in it (`nested`).
fn nested_away(gx: &crate::graph::Gx<'_, '_>, s: usize, q: u16) -> bool {
    (0..gx.graph.reruns.len()).any(|t| nested(gx, t, s, q))
}

/// Whether set `s` leaves every member to sets nested in it (`nested_away`; D1337, the outer SubPlan's set of decorrelate's
/// level-2 reference): its owner declares no member proto (`Rerun::runs` false), and the sets that run them name them
/// (frame/program.rs `undeclared_set`).
pub(crate) fn all_nested_away(gx: &crate::graph::Gx<'_, '_>, s: usize) -> bool {
    let ms = &gx.graph.reruns[s].pipelines;
    !ms.is_empty() && ms.iter().all(|&q| nested_away(gx, s, q))
}

/// The graph pipeline whose code runs rerun set `set` (`Rerun::run`): the one embedding the SubPlan op `sub_set_index`
/// maps to it, or (a nest's set, plan 0) the inner re-opened at the set's node; none for a recursive union's term set.
pub(crate) fn set_owner(gx: &crate::graph::Gx<'_, '_>, set: usize) -> Option<usize> {
    owner_op(gx, set).map(|(q, _)| root(gx, q))
}

/// `set_owner`'s op itself: the graph pipeline or inner (`Pipes::end` numbering) holding it, and its index there.
fn owner_op(gx: &crate::graph::Gx<'_, '_>, set: usize) -> Option<(usize, usize)> {
    let r = &gx.graph.reruns[set];
    if r.plan_id == gravity_decompose::ops::shared::setop::RECURSIVE_SET {
        return None;
    }
    let ps = gx.pipelines();
    let (n, inners) = (ps.len(), ps.inners());
    let at = |q: usize, p: &PipelineNode<'_>, j: usize, o: &Op<'_>| match o {
        Op::SubPlan(d) => r.plan_id != 0 && d.plan_id == r.plan_id && sub_set_index(gx, q, j, d.plan_id) == Some(set),
        _ => r.plan_id == 0 && inners.iter().any(|x| x.0 == (q, j)) && crate::rerun::inner::op_node(p, j) == r.node_id,
    };
    let all = (0..n).map(|q| (q, &ps[q])).chain(inners.iter().enumerate().map(|(i, x)| (n + i, x.1)));
    all.into_iter().find_map(|(q, p)| p.ops.iter().enumerate().position(|(j, o)| at(q, p, j, o)).map(|j| (q, j)))
}

impl Rerun {
    /// The set re-run now (after the owner wrote its param words, before its inner opens): `m8_rerun(ctx, h)`; a status
    /// below zero leaves by the raise exit with the host's stashed error.
    #[allow(dead_code)] // nest/inner.rs's open (M8-NEST's D1018 hook row) and nest.subplan (M8-SUB B2) call it
    pub(crate) fn run(&self, b: &mut FunctionBuilder<'_>, env: &Env) {
        self.reset(b, env);
        if self.runs {
            self.call(b, env, "m8_rerun", m8::m8_rerun as *const () as usize);
        }
    }

    /// The set's owner-run words stored now, before its members run (the lifetime home's `Scope::OwnerRun`: a streamed
    /// union's written flag, D2121; a CTE's eager store, D1385), as PG's ExecReScan resets each node's state at the start
    /// of the rescan.
    fn reset(&self, b: &mut FunctionBuilder<'_>, env: &Env) {
        if !self.resets.write(b, env.words, env.params) {
            crate::refuse!(env.node, Reason::Bug { payload: "a rerun set's owner-run words without the words site" });
        }
    }

    /// D2611: the set's InitPlans re-armed now, `m8_rerun_rearm(ctx, h)` (kernels subplan/raw.rs `RerunRearm`): a gated
    /// inner's re-open, before its one-time gates read an InitPlan's param (PG re-arms at the rescan, before the Result's
    /// first call), so a failed gate, which skips `run`, still leaves the next re-open reading its own values; the `run`
    /// after a passing gate re-arms nothing. A status below zero leaves by the raise exit with the host's stashed error.
    pub(crate) fn rearm(&self, b: &mut FunctionBuilder<'_>, env: &Env) {
        if self.runs {
            self.call(b, env, "m8_rerun_rearm", gravity_kernels::contract::entry1::<RerunRearm> as *const () as usize);
        }
    }

    /// Host `name` at `addr` called over the set's handle; a status below zero leaves by the raise exit.
    fn call(&self, b: &mut FunctionBuilder<'_>, env: &Env, name: &str, addr: usize) {
        let hv = b.load(Type::I64, env.params, self.h as i32 * 8);
        let f = b.declare_host_once(name, &[Type::Ptr, Type::I64], Some(Type::I64), addr, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() });
        let st = b.call_host(f, &[env.ctx, hv], Some(Type::I64)).unwrap_or(env.zero);
        let bad = b.icmp(IntCond::Slt, st, env.zero);
        let (l_bad, l_ok) = (b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_bad);
        b.cond_br(bad, (l_bad, &[]), (l_ok, &[]));
        b.switch_to(l_bad);
        let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
        b.br(env.l_raise, &[e]);
        b.switch_to(l_ok);
    }

    /// The set re-run now, as `run`, for an owner that keeps the error itself (setop.recursive's generation hold): the
    /// status of `m8_rerun(ctx, h)`, below zero with the host's error stashed.
    pub(crate) fn status(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> Value {
        self.reset(b, env);
        if !self.runs {
            return env.zero;
        }
        let hv = b.load(Type::I64, env.params, self.h as i32 * 8);
        let f = b.declare_host_once("m8_rerun", &[Type::Ptr, Type::I64], Some(Type::I64), m8::m8_rerun as *const () as usize, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() });
        b.call_host(f, &[env.ctx, hv], Some(Type::I64)).unwrap_or(env.zero)
    }

    /// D2034: a streamed recursive union's term set (its run the body alone) pulled on until the body's end took the rows
    /// its want word allows, `m8_rerun_pull(ctx, h, budget, first)` (kernels subplan/raw.rs `RerunPull`); at a generation's
    /// first pull (`first` nonzero) the owner-run words are stored first and the set is re-armed, as `status` re-runs it.
    /// The status: 1 the body ended, 0 it stopped, below zero with the host's error stashed (the owner keeps it).
    pub(crate) fn pull(&self, b: &mut FunctionBuilder<'_>, env: &Env, budget: Value, first: Value) -> Value {
        let (l_reset, l_go) = (b.create_block(&[]), b.create_block(&[]));
        let is_first = b.icmp(IntCond::Ne, first, env.zero);
        b.cond_br(is_first, (l_reset, &[]), (l_go, &[]));
        b.switch_to(l_reset);
        self.reset(b, env);
        b.br(l_go, &[]);
        b.switch_to(l_go);
        let hv = b.load(Type::I64, env.params, self.h as i32 * 8);
        let f = b.declare_host_once("m8_rerun_pull", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Some(Type::I64), gravity_kernels::contract::entry3::<RerunPull> as *const () as usize, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() });
        b.call_host(f, &[env.ctx, hv, budget, first], Some(Type::I64)).unwrap_or(env.zero)
    }
}

#[cfg(test)]
mod tests {
    use gravity_abi::breaker::{Breaker, BreakerKind};
    use gravity_decompose::graph::{slot_set, Rerun as SetDecl};
    use gravity_decompose::{NoFacts, PipelineGraph, PipelineNode, Source};
    use gravity_lower::Sequential;
    use gravity_program::protos::ProtoKind;
    use gravity_program::{Arena, ArenaVec, End, Pipeline};

    use super::{declare, members_close, owner_set, owner_set_at, sub_set, sub_set_at, sub_set_index, Rerun};
    use crate::frame::px::Px;
    use crate::graph::Gx;
    use crate::admit::refusal::Reason;

    /// p0 fills slot 0 (a Sort, the set), p1 is the nest's pipeline reading it (the owner), and `extra` more pipelines read
    /// slot 0 too; the set of the nest at node 5.
    fn graph(a: gravity_program::ArenaRef, extra: usize) -> PipelineGraph<'static> {
        let mut g = PipelineGraph::new(a);
        let sort = Pipeline::serial(-1, End::Breaker(Breaker::sized(BreakerKind::Sort, 64).in_slot(0)), gravity_program::Reason::NoDrivingSite);
        g.pipelines.push(PipelineNode::new(a, 7, Source::Const, sort));
        for _ in 0..1 + extra {
            let read = Pipeline::serial(-1, End::Sink, gravity_program::Reason::NoDrivingSite).consuming_one(0);
            g.pipelines.push(PipelineNode::new(a, 5, Source::Breaker { slot: 0 }, read));
        }
        let (mut pipelines, mut slots) = (ArenaVec::new_in(a), [0u64; 4]);
        pipelines.push(0u16);
        slot_set(&mut slots, 0);
        g.reruns.push(SetDecl { node_id: 5, plan_id: 0, pipelines, slots });
        g
    }

    #[test]
    fn an_owner_declares_its_set_once_as_one_proto_per_member_on_one_handle() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let arena = Arena::new(1 << 16);
        let g = graph(arena.handle(), 0);
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        let mut plan = Sequential::new(arena.handle());
        let mut px = Px::new(arena.handle(), &gx, &mut plan);
        let r = declare(&mut px, 1, 5, 0).expect("the nest at node 5 has a set");
        assert_eq!(declare(&mut px, 1, 5, 0), Some(r.clone()), "a second declaration is the same handle");
        assert_eq!(declare(&mut px, 1, 5, 3), None, "no SubPlan 3 set there");
        assert_eq!(declare(&mut px, 1, 6, 0), None, "no set at node 6");
        let ps: Vec<_> = px.protos.iter().filter(|p| p.kind == ProtoKind::Rerun).map(|p| (p.pipeline, p.peer, p.arg, p.words.base, p.words.n)).collect(); // alloc: Cold
        assert_eq!(ps, [(0, 1, 0, r.h, 1)], "member 0, owner 1, set 0, one shared handle word");
    }

    #[test]
    fn a_set_slot_another_pipeline_reads_refuses_by_name() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let arena = Arena::new(1 << 16);
        let g = graph(arena.handle(), 1);
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        let mut plan = Sequential::new(arena.handle());
        let mut px = Px::new(arena.handle(), &gx, &mut plan);
        let e = crate::catch(5, || drop(declare(&mut px, 1, 5, 0))).expect_err("pipeline 2 reads the set's slot");
        assert!(matches!(e.reason, Reason::Shape { why } if why.0.contains("outside its owner")), "{e:?}");
    }

    /// Landing 12: an earlier life of the set's slot number (p0 fills slot 0, p1 reads it: an InitPlan's aggregate inside
    /// the inner, kept) is not a read of the set: the member p2 refills slot 0 for the owner p3 only, so the set declares.
    #[test]
    fn an_earlier_life_of_a_set_slot_number_is_not_a_read_of_the_set() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let mut g = PipelineGraph::new(a);
        for owner in [false, true] {
            let sort = Pipeline::serial(-1, End::Breaker(Breaker::sized(BreakerKind::Sort, 64).in_slot(0)), gravity_program::Reason::NoDrivingSite);
            g.pipelines.push(PipelineNode::new(a, 7, Source::Const, sort));
            let read = Pipeline::serial(-1, End::Sink, gravity_program::Reason::NoDrivingSite).consuming_one(0);
            g.pipelines.push(PipelineNode::new(a, if owner { 5 } else { 8 }, Source::Breaker { slot: 0 }, read));
        }
        let (mut pipelines, mut slots) = (ArenaVec::new_in(a), [0u64; 4]);
        pipelines.push(2u16);
        slot_set(&mut slots, 0);
        g.reruns.push(SetDecl { node_id: 5, plan_id: 0, pipelines, slots });
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        let mut plan = Sequential::new(arena.handle());
        let mut px = Px::new(arena.handle(), &gx, &mut plan);
        assert!(declare(&mut px, 3, 5, 0).is_some(), "p1 reads p0's life, not the member's");
    }

    /// D1337 row B, D2300: the nest's set and SubPlan 3's set at node 9 both holding p0 refuse at either owner's declaration
    /// by D_RERUN_SHARED; a disjoint second set declares.
    #[test]
    fn a_set_whose_member_another_set_also_runs_refuses_by_name_for_every_owner() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use super::D_RERUN_SHARED;
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let second = |members: &[u16]| {
            let mut g = graph(a, 0);
            let mut pipelines = ArenaVec::new_in(a);
            pipelines.extend_from_slice(members);
            g.reruns.push(SetDecl { node_id: 9, plan_id: 3, pipelines, slots: [0u64; 4] });
            g
        };
        let declared = |g: &PipelineGraph<'_>, node_id: i32, plan_id: i32| {
            let gx = Gx { graph: g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
            let mut plan = Sequential::new(a);
            let mut px = Px::new(a, &gx, &mut plan);
            crate::catch(node_id, || declare(&mut px, 1, node_id, plan_id).is_some()).map_err(|e| e.reason)
        };
        let g = second(&[0]);
        for (node_id, plan_id) in [(5, 0), (9, 3)] {
            let e = declared(&g, node_id, plan_id).expect_err("p0 is in both sets");
            assert!(matches!(e, Reason::Shape { why } if why.0 == D_RERUN_SHARED), "{e:?}");
        }
        assert_eq!(declared(&second(&[1]), 5, 0).ok(), Some(true), "disjoint sets: the nest's set declares");
        // join#784: p0 an InitPlan's pipeline, re-armed by either set, never run by one: both declare
        let mut g = second(&[0]);
        let ints: &types_nodes::list::IntList<'_> = Box::leak(Box::new(types_nodes::list::IntList::nil())); // alloc: Cold
        g.init_plans.push(gravity_decompose::graph::InitPlan { plan_id: 2, pipeline: 0, link: gravity_decompose::ops::shared::subplan::SubLink::Expr, param: 7, params: ints, slot: None, volatility: gravity_decompose::effects::Volatility::Immutable, raise_free: true, parallel_safe: true, attach: -1 });
        for (node_id, plan_id) in [(5, 0), (9, 3)] {
            assert_eq!(declared(&g, node_id, plan_id).ok(), Some(true), "an InitPlan member shared by two sets");
        }
    }

    /// D1337(A): p0 builds a hash table (slot 0), p1 probes it and folds a plain aggregate (slot 1), p2 is that aggregate's
    /// emit (its reader), p3 the SubPlan's owner. `drain` adds p4 reading the build as its source (a join's unmatched
    /// rows); `probe` false leaves p1 consuming slot 0 with no probe op.
    fn probed(a: gravity_program::ArenaRef, probe: bool, drain: bool) -> PipelineGraph<'static> {
        use gravity_decompose::join::JoinDesc;
        use gravity_decompose::Op;
        use types_nodes::jointype::JoinType;
        use types_nodes::list::NodeList;
        let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil()));
        let r = gravity_program::Reason::NoDrivingSite;
        let into = |kind, slot| Pipeline::serial(-1, End::Breaker(Breaker::sized(kind, 64).in_slot(slot)), r);
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(PipelineNode::new(a, 7, Source::Const, into(BreakerKind::HashBuild, 0)));
        let mut p = PipelineNode::new(a, 6, Source::Const, into(BreakerKind::PlainAgg, 1).consuming_one(0));
        if probe {
            p.push(Op::Join(JoinDesc::Probe { slot: 0, jointype: JoinType::JOIN_INNER, clauses: nil, keys: nil, qual: nil, unique: false, marks: false, decls: ArenaVec::new_in(a), inner_node: 7, outer_first: false, catalog: false }));
        }
        g.pipelines.push(p);
        g.pipelines.push(PipelineNode::new(a, 5, Source::Breaker { slot: 1 }, Pipeline::serial(-1, End::Sink, r).consuming_one(1)));
        g.pipelines.push(PipelineNode::new(a, 4, Source::Const, Pipeline::serial(-1, End::Sink, r).consuming_one(1)));
        if drain {
            g.pipelines.push(PipelineNode::new(a, 8, Source::Breaker { slot: 0 }, Pipeline::serial(-1, End::Sink, r).consuming_one(0)));
        }
        g
    }

    #[test]
    fn a_hash_build_only_set_members_probe_closes_and_a_drain_or_an_outside_reader_does_not() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let arena = Arena::new(1 << 16);
        let close = |probe, drain, set: &[u16]| {
            let g = probed(arena.handle(), probe, drain);
            members_close(&Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() }, 3, set)
        };
        assert!(close(true, false, &[0, 1]), "a build its member probes, a plain aggregate its emit reads");
        assert!(close(true, false, &[1]), "the plain aggregate alone (the build kept outside the set)");
        assert!(!close(true, false, &[0]), "the probe outside the set reads the build once");
        assert!(!close(true, true, &[0, 1]), "a reader of the build as its source that is no member drain");
        assert!(!close(false, false, &[0, 1]), "a member reading the build through no probe op");
    }

    /// D1337: a Hash Right Join's members: p0 builds (slot 0), p1 probes it into its spool (slot 2), p2 drains the build's
    /// unmarked records (`matches` slot 2) into a plain aggregate (slot 1), p3 is that aggregate's emit, p4 the owner. The
    /// set holding the drain closes; with the drain outside it, the build and the spool have an outside reader.
    #[test]
    fn a_member_drain_reads_its_sets_build_and_probe_spool() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use gravity_decompose::join::JoinDesc;
        use gravity_decompose::Op;
        use types_nodes::jointype::JoinType;
        use types_nodes::list::NodeList;
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil())); // alloc: Cold
        let r = gravity_program::Reason::NoDrivingSite;
        let into = |kind, slot| Pipeline::serial(-1, End::Breaker(Breaker::sized(kind, 64).in_slot(slot)), r);
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(PipelineNode::new(a, 7, Source::Const, into(BreakerKind::HashBuild, 0)));
        let mut probe = PipelineNode::new(a, 6, Source::Const, into(BreakerKind::Materialize, 2).consuming_one(0));
        probe.push(Op::Join(JoinDesc::Probe { slot: 0, jointype: JoinType::JOIN_RIGHT, clauses: nil, keys: nil, qual: nil, unique: false, marks: true, decls: ArenaVec::new_in(a), inner_node: 7, outer_first: false, catalog: false }));
        g.pipelines.push(probe);
        let mut drain = PipelineNode::new(a, 6, Source::Breaker { slot: 0 }, into(BreakerKind::PlainAgg, 1).consuming_one(0).consuming_in(a, 2));
        drain.push(Op::Join(JoinDesc::Drain { matches: 2, jointype: JoinType::JOIN_RIGHT }));
        g.pipelines.push(drain);
        g.pipelines.push(PipelineNode::new(a, 5, Source::Breaker { slot: 1 }, Pipeline::serial(-1, End::Sink, r).consuming_one(1)));
        g.pipelines.push(PipelineNode::new(a, 4, Source::Const, Pipeline::serial(-1, End::Sink, r).consuming_one(1)));
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        assert!(members_close(&gx, 4, &[0, 1, 2]), "the build, its probe and its drain re-run together");
        assert!(!members_close(&gx, 4, &[0, 1]), "the drain outside the set reads the build and the spool once");
        assert!(members_close(&gx, 4, &[1, 2]), "D2592: over the build kept outside the set, its probe clears the marks at each open");
    }

    /// D1337(H): p0 builds a hash table (slot 0) in the rerun set of plan `plan`; op 0 of p1 (the owner, node 5) is SubPlan 1,
    /// whose embedded inner (pipeline 2) probes the build (`outer_first`: on PG's outer-first path).
    fn embedded(a: gravity_program::ArenaRef, plan: i32, outer_first: bool) -> PipelineGraph<'static> {
        use gravity_decompose::effects::Volatility;
        use gravity_decompose::join::JoinDesc;
        use gravity_decompose::ops::shared::subplan::{SubForm, SubLink, SubPlanDesc, SubSite};
        use gravity_decompose::Op;
        use gravity_program::ArenaBox;
        use types_nodes::jointype::JoinType;
        use types_nodes::list::{IntList, NodeList};
        let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil())); // alloc: Cold
        let ints: &IntList<'_> = Box::leak(Box::new(IntList::nil())); // alloc: Cold
        let r = gravity_program::Reason::NoDrivingSite;
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(PipelineNode::new(a, 7, Source::Const, Pipeline::serial(-1, End::Breaker(Breaker::sized(BreakerKind::HashBuild, 64).in_slot(0)), r)));
        let mut inner = PipelineNode::new(a, 6, Source::Const, Pipeline::serial(-1, End::Sink, r).consuming_one(0));
        inner.push(Op::Join(JoinDesc::Probe { slot: 0, jointype: JoinType::JOIN_INNER, clauses: nil, keys: nil, qual: nil, unique: false, marks: false, decls: ArenaVec::new_in(a), inner_node: 7, outer_first, catalog: false }));
        let form = SubForm::Nested { inner: ArenaBox::new_in(inner, a) };
        let d = SubPlanDesc { plan_id: 1, link: SubLink::Exists, test: None, args: nil, params: ints, sets: ints, ids: ints, site: SubSite::Row, conditional: false, volatility: Volatility::Immutable, raise_free: true, quiet_but_rows: true, invariant: false, unknown_eq_false: false, alternative: false, set_spool: false, stops: false, ahead: None, call: gravity_decompose::effects::found::Found::CLEAN, form };
        let mut owner = PipelineNode::new(a, 5, Source::Const, Pipeline::serial(-1, End::Sink, r));
        owner.push(Op::SubPlan(d));
        g.pipelines.push(owner);
        let (mut pipelines, mut slots) = (ArenaVec::new_in(a), [0u64; 4]);
        pipelines.push(0u16);
        slot_set(&mut slots, 0);
        g.reruns.push(SetDecl { node_id: 5, plan_id: plan, pipelines, slots });
        g
    }

    #[test]
    fn a_subplans_own_inner_probing_a_build_of_its_set_is_read_as_a_member() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let arena = Arena::new(1 << 16);
        let g = embedded(arena.handle(), 1, false);
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        assert_eq!(sub_set(&gx, 2).map(|r| r.plan_id), Some(1), "the inner of SubPlan 1: that plan's set");
        assert_eq!(sub_set(&gx, 1).map(|r| r.plan_id), None, "a graph pipeline is no embedded inner");
        assert_eq!(owner_set(&gx, 2), Some((5, 1)), "its re-open runs the set: a probe of its build, the source no breaker");
        assert!(members_close(&gx, 1, &[0]), "the build read by the SubPlan's own inner's probe closes");
        let g = embedded(arena.handle(), 2, false);
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        assert_eq!(owner_set(&gx, 2), None, "the build is another plan's set's (does not apply)");
        assert!(!members_close(&gx, 1, &[0]), "an inner outside the set reading its build once");
    }

    /// D1337 (`sub_set_at`, mt-0353): a SubPlan mapped at two plan nodes has a set at each (one copy of its inner per leaf of
    /// a partitioned target); the owner at node 9 reads node 9's set, not the plan's first; a plan with no set at the op's
    /// node reads its first.
    #[test]
    fn a_subplan_mapped_at_two_nodes_reads_the_set_at_its_own_node() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let mut g = embedded(a, 1, false);
        g.pipelines[1].node_id = 9;
        let (mut pipelines, mut slots) = (ArenaVec::new_in(a), [0u64; 4]);
        pipelines.push(0u16);
        slot_set(&mut slots, 0);
        g.reruns.push(SetDecl { node_id: 9, plan_id: 1, pipelines, slots });
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        assert_eq!(sub_set_at(&gx, 1, 0, 1).map(|r| r.node_id), Some(9), "the set at the SubPlan op's node, the plan's second");
        assert_eq!(sub_set(&gx, 2).map(|r| r.node_id), Some(9), "its embedded inner keys the same set");
        assert_eq!(owner_set(&gx, 2), Some((9, 1)), "the re-open runs that set");
        g.pipelines[1].node_id = 4;
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        assert_eq!(sub_set_at(&gx, 1, 0, 1).map(|r| r.node_id), Some(5), "no set at the op's node: the plan's first");
        assert_eq!(sub_set_at(&gx, 1, 0, 2).map(|r| r.node_id), None, "a plan with no set");
    }

    /// rr-0069 (`sub_set_index`, `owner_set_at`, `Rerun::declare`): SubPlan 1 mapped twice at one node (a grouping-sets
    /// aggregate's target list SubPlan in two emits), each copy its own set keyed (5, 1). Entry 0 is the other copy (its
    /// member p2 builds slot 1, which the inner does not read); entry 1 holds p0, whose build the inner (3) of the owner p1
    /// probes. The owner reads and re-runs entry 1, not the key's first; each copy declares on its own handle.
    #[test]
    fn a_subplan_mapped_twice_at_one_node_reads_and_declares_its_own_copy() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let mut g = embedded(a, 1, false);
        let r = gravity_program::Reason::NoDrivingSite;
        g.pipelines.push(PipelineNode::new(a, 8, Source::Const, Pipeline::serial(-1, End::Breaker(Breaker::sized(BreakerKind::HashBuild, 64).in_slot(1)), r)));
        g.reruns[0].pipelines[0] = 2;
        g.reruns[0].slots = [0u64; 4];
        slot_set(&mut g.reruns[0].slots, 1);
        let (mut pipelines, mut slots) = (ArenaVec::new_in(a), [0u64; 4]);
        pipelines.push(0u16);
        slot_set(&mut slots, 0);
        g.reruns.push(SetDecl { node_id: 5, plan_id: 1, pipelines, slots });
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        assert_eq!(sub_set_index(&gx, 1, 0, 1), Some(1), "the copy whose member the op's inner reads");
        assert_eq!(sub_set(&gx, 3).map(|r| r.pipelines.to_vec()), Some(vec![0]), "its embedded inner keys the same copy");
        assert_eq!((owner_set(&gx, 3), owner_set_at(&gx, 3)), (Some((5, 1)), Some(1)), "the re-open runs entry 1, not the key's first");
        let mut plan = Sequential::new(a);
        let mut px = Px::new(a, &gx, &mut plan);
        let (ra, rb) = (Rerun::declare(&mut px, 1, 1), Rerun::declare(&mut px, 1, 0));
        assert_ne!(ra.h, rb.h, "one handle per copy");
        assert_eq!(declare(&mut px, 1, 5, 1), Some(rb), "the key alone names its first entry");
        let ps: Vec<_> = px.protos.iter().filter(|p| p.kind == ProtoKind::Rerun).map(|p| (p.pipeline, p.arg)).collect(); // alloc: Cold
        assert_eq!(ps, [(0, 1), (2, 0)], "member 0 on set 1, member 2 on set 0");
    }

    /// groupingsets#66 (`init_copy`): SubPlan 2 mapped in two emits at node 5 (p2, p3), each copy's set holding only its
    /// InitPlan copy (p0, p1), which no inner reads by slot: the i-th reference declares the i-th copy, so both sets have
    /// an owner. A third copy with two references (counts differ) keeps the plan's first.
    #[test]
    fn subplan_copies_holding_only_an_initplan_go_one_to_each_reference_in_order() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use gravity_decompose::effects::Volatility;
        use gravity_decompose::graph::InitPlan;
        use gravity_decompose::ops::shared::subplan::{SubForm, SubLink, SubPlanDesc, SubSite};
        use gravity_decompose::Op;
        use gravity_program::ArenaBox;
        use types_nodes::list::{IntList, NodeList};
        let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil())); // alloc: Cold
        let ints: &IntList<'_> = Box::leak(Box::new(IntList::nil())); // alloc: Cold
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let r = gravity_program::Reason::NoDrivingSite;
        let mut g = PipelineGraph::new(a);
        for q in 0..3u16 {
            g.pipelines.push(PipelineNode::new(a, 9, Source::Const, Pipeline::serial(-1, End::Sink, r)));
            g.init_plans.push(InitPlan { plan_id: 1, pipeline: q, link: SubLink::Expr, param: 7, params: ints, slot: None, volatility: Volatility::Immutable, raise_free: true, parallel_safe: true, attach: -1 });
            let mut pipelines = ArenaVec::new_in(a);
            pipelines.push(q);
            g.reruns.push(SetDecl { node_id: 5, plan_id: 2, pipelines, slots: [0u64; 4] });
        }
        for _ in 0..2 {
            let form = SubForm::Nested { inner: ArenaBox::new_in(PipelineNode::new(a, 6, Source::Const, Pipeline::serial(-1, End::Sink, r)), a) };
            let d = SubPlanDesc { plan_id: 2, link: SubLink::Expr, test: None, args: nil, params: ints, sets: ints, ids: ints, site: SubSite::Row, conditional: false, volatility: Volatility::Immutable, raise_free: true, quiet_but_rows: true, invariant: false, unknown_eq_false: false, alternative: false, set_spool: false, stops: false, ahead: None, call: gravity_decompose::effects::found::Found::CLEAN, form };
            let mut owner = PipelineNode::new(a, 5, Source::Const, Pipeline::serial(-1, End::Sink, r));
            owner.push(Op::SubPlan(d));
            g.pipelines.push(owner);
        }
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        assert_eq!((sub_set_index(&gx, 3, 0, 2), sub_set_index(&gx, 4, 0, 2)), (Some(0), Some(0)), "three copies, two references: the first");
        g.reruns.pop();
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        assert_eq!((sub_set_index(&gx, 3, 0, 2), sub_set_index(&gx, 4, 0, 2)), (Some(0), Some(1)), "each reference its own copy, in order");
        assert_eq!((owner_set_at(&gx, 5), owner_set_at(&gx, 6)), (Some(0), Some(1)), "each inner's re-open re-arms its own copy");
    }

    /// D1337 (`embeds`): SubPlan 2 sits in the embedded inner A of SubPlan 1 (op 0 of p1, the root); its own inner B probes
    /// the build p0 of SubPlan 2's set. The root p1 lists the slot B reads among its own (as `Pipes::end` makes a pipeline
    /// list its inners' reads): it embeds A, so it is no reader of A's set, and A's set closes; a root that does not embed
    /// the owner stays a reader.
    #[test]
    fn a_pipeline_embedding_the_owner_is_no_reader_of_its_set() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use gravity_decompose::effects::Volatility;
        use gravity_decompose::join::JoinDesc;
        use gravity_decompose::ops::shared::subplan::{SubForm, SubLink, SubPlanDesc, SubSite};
        use gravity_decompose::Op;
        use gravity_program::ArenaBox;
        use types_nodes::jointype::JoinType;
        use types_nodes::list::{IntList, NodeList};
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil())); // alloc: Cold
        let ints: &IntList<'_> = Box::leak(Box::new(IntList::nil())); // alloc: Cold
        let r = gravity_program::Reason::NoDrivingSite;
        let sub = |plan_id: i32, inner: PipelineNode<'static>| SubPlanDesc { plan_id, link: SubLink::Exists, test: None, args: nil, params: ints, sets: ints, ids: ints, site: SubSite::Row, conditional: false, volatility: Volatility::Immutable, raise_free: true, quiet_but_rows: true, invariant: false, unknown_eq_false: false, alternative: false, set_spool: false, stops: false, ahead: None, call: gravity_decompose::effects::found::Found::CLEAN, form: SubForm::Nested { inner: ArenaBox::new_in(inner, a) } };
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(PipelineNode::new(a, 7, Source::Const, Pipeline::serial(-1, End::Breaker(Breaker::sized(BreakerKind::HashBuild, 64).in_slot(0)), r)));
        let mut b = PipelineNode::new(a, 6, Source::Const, Pipeline::serial(-1, End::Sink, r).consuming_one(0));
        b.push(Op::Join(JoinDesc::Probe { slot: 0, jointype: JoinType::JOIN_INNER, clauses: nil, keys: nil, qual: nil, unique: false, marks: false, decls: ArenaVec::new_in(a), inner_node: 7, outer_first: false, catalog: false }));
        let mut inner_a = PipelineNode::new(a, 8, Source::Const, Pipeline::serial(-1, End::Sink, r));
        inner_a.push(Op::SubPlan(sub(2, b)));
        let mut root = PipelineNode::new(a, 5, Source::Const, Pipeline::serial(-1, End::Sink, r).consuming_one(0));
        root.push(Op::SubPlan(sub(1, inner_a)));
        g.pipelines.push(root);
        let (mut pipelines, mut slots) = (ArenaVec::new_in(a), [0u64; 4]);
        pipelines.push(0u16);
        slot_set(&mut slots, 0);
        g.reruns.push(SetDecl { node_id: 8, plan_id: 2, pipelines, slots });
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        let (ps, ins) = (gx.pipelines(), gx.pipelines().inners());
        let ia = ps.len() + ins.iter().position(|x| x.0 .0 == 1).expect("inner A of the root");
        let ib = ps.len() + ins.iter().position(|x| x.0 .0 == ia).expect("inner B of A");
        assert!(super::embeds(&gx, 1, ia) && super::embeds(&gx, 1, ib) && super::embeds(&gx, ia, ib), "the root embeds A and B, A embeds B");
        assert!(!super::embeds(&gx, 1, 1) && !super::embeds(&gx, ib, ia) && !super::embeds(&gx, 0, ib), "never itself, an inner, or a pipeline off the chain");
        assert_eq!(sub_set(&gx, ib).map(|r| r.plan_id), Some(2), "B is SubPlan 2's inner");
        assert!(members_close(&gx, ia, &[0]), "the build read by SubPlan 2's own inner; the root lists that read as its own");
    }
    /// D2300 (the r8 correctness lens, 16:27Z): a set holding a hash build whose member another rerun set also runs (a
    /// LATERAL nest's set) refuses by D2300's name; the set alone, a second set over other pipelines and a set without a hash
    /// build (not D1337(A)'s) do not.
    #[test]
    fn a_hash_build_set_whose_member_another_set_runs_refuses_by_d2300() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use super::{owner_refusal, D_RERUN_SHARED};
        fn add(g: &mut PipelineGraph<'_>, a: gravity_program::ArenaRef, node_id: i32, plan_id: i32, ms: &[u16]) {
            let mut pipelines = ArenaVec::new_in(a);
            pipelines.extend_from_slice(ms);
            g.reruns.push(SetDecl { node_id, plan_id, pipelines, slots: [0u64; 4] });
        }
        let arena = Arena::new(1 << 20);
        let a = arena.handle();
        let nw = |g: &PipelineGraph<'_>, set: &[u16]| {
            let gx = Gx { graph: g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
            let mut plan = Sequential::new(a);
            let px = Px::new(a, &gx, &mut plan);
            owner_refusal(&px, 3, set)
        };
        let mut g = probed(a, true, false);
        add(&mut g, a, 6, 1, &[0, 1]);
        assert_eq!(nw(&g, &[0, 1]), None, "the SubPlan's set alone");
        add(&mut g, a, 4, 0, &[2]);
        assert_eq!(nw(&g, &[0, 1]), None, "a second set over other pipelines");
        add(&mut g, a, 4, 0, &[0, 1, 2]);
        assert_eq!(nw(&g, &[0, 1]), Some(D_RERUN_SHARED), "a nest's set also running the SubPlan's members");
        let mut g = probed(a, true, false);
        add(&mut g, a, 6, 1, &[0, 1]);
        add(&mut g, a, 4, 0, &[0, 1]);
        assert_eq!(nw(&g, &[0, 1]), Some(D_RERUN_SHARED), "a second set with the same members");
        assert_eq!(nw(&g, &[1]), None, "no hash build in the set: not D1337(A)'s");
    }

    /// Nested sets (D2300's exemption; tour-subq subq-0257, regress-style decorrelate.sql's two nested aggregate SubPlans):
    /// SubPlan 1's set (entry 0: p0, the build its inner probes) is owned by p1; SubPlan 4's set (entry 1) holds p0 and p1,
    /// so it runs SubPlan 1's owner and leaves p0 to that owner's calls: both declare, entry 1 with p1's proto only. Kept
    /// p0 (PG keeps it across SubPlan 1's calls), or an outer set without SubPlan 1's owner, still refuse by D2300.
    #[test]
    fn a_set_nested_in_another_runs_the_members_both_hold() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use super::D_RERUN_SHARED;
        let arena = Arena::new(1 << 20);
        let a = arena.handle();
        let nested = |outer: &[u16], kept: bool| {
            let mut g = embedded(a, 1, false);
            g.pipelines.push(PipelineNode::new(a, 3, Source::Const, Pipeline::serial(-1, End::Sink, gravity_program::Reason::NoDrivingSite)));
            let mut pipelines = ArenaVec::new_in(a);
            pipelines.extend_from_slice(outer);
            g.reruns.push(SetDecl { node_id: 3, plan_id: 4, pipelines, slots: [0u64; 4] });
            if kept {
                g.kept.push(0);
            }
            g
        };
        let declared = |g: &PipelineGraph<'_>| {
            let gx = Gx { graph: g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
            let mut plan = Sequential::new(a);
            let mut px = Px::new(a, &gx, &mut plan);
            let r = crate::catch(5, || {
                Rerun::declare(&mut px, 1, 0);
                Rerun::declare(&mut px, 2, 1);
            });
            r.map(|_| px.protos.iter().filter(|p| p.kind == ProtoKind::Rerun).map(|p| (p.pipeline, p.arg)).collect::<Vec<_>>()).map_err(|e| e.reason)
        };
        assert_eq!(declared(&nested(&[0, 1], false)).ok(), Some(vec![(0, 0), (1, 1)]), "p0 on SubPlan 1's set only, p1 on the outer set");
        for (outer, kept, why) in [(&[0, 1][..], true, "p0 kept across SubPlan 1's calls"), (&[0][..], false, "SubPlan 1's owner outside the outer set")] {
            let e = declared(&nested(outer, kept)).expect_err(why);
            assert!(matches!(e, Reason::Shape { why } if why.0 == D_RERUN_SHARED), "{e:?}");
        }
    }
}

/// D4237: WindowAgg reader `k`'s pulled fill `fill` (the exact request, ops/window/lower.rs `pulled`) is re-armed by the one
/// rerun set holding it, as PostgreSQL's ExecReScanWindowAgg empties the spool at each rescan and spool_tuples reads the
/// child again from its start as far as the reader's rows need: the set's run empties the window object's spool and starts
/// its pulled set again (lifetime/mod.rs `program`'s `Runner::Window`, exec ops/window/proto.rs `rearm`), never runs it. The set is a nest's or SubPlan's (a
/// recursive union's term set re-runs through its own pull), it holds the fill and every pipeline feeding it, none kept
/// across its runs (`graph.kept`) and none held by another set; the reader is a member of the set and of no other, or the
/// inner whose re-open runs the set with no Memoize in it (a hit replays rows the set's next run does not re-read).
pub(crate) fn rearms_window_fill(gx: &crate::graph::Gx<'_, '_>, k: usize, fill: usize) -> bool {
    let rs = &gx.graph.reruns;
    let ps = gx.pipelines();
    let held = |q: usize| rs.iter().filter(move |r| r.pipelines.contains(&(q as u16)));
    let mut sets = held(fill);
    let (Some(r), None) = (sets.next(), sets.next()) else { return false };
    if r.plan_id == gravity_decompose::ops::shared::setop::RECURSIVE_SET {
        return false;
    }
    let only_r = |q: usize| held(q).count() == 1 && r.pipelines.contains(&(q as u16));
    let reader = match k < ps.len() {
        true => only_r(k),
        false => owner_set(gx, k) == Some((r.node_id, r.plan_id)) && crate::ops::memoize::lower::at(&ps[k]).is_none(),
    };
    reader && crate::ops::ctescan::lower::fed(gx, fill).into_iter().all(|q| only_r(q) && !gx.graph.kept.contains(&(q as u16)))
}
