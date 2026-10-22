//! D1016 (V4R-475): an index key computed from executor parameters (a cast, a subscript, arithmetic over them, not a bare
//! parameter), the host's at every (re-)open: its cell (`HOST_KEY`) and the parameters it reads as input cells after the
//! scan keys (source/index.rs, source/index_only.rs; hosts / exec v4/m8_param_key.rs). Split from params.rs for its
//! budget (a lib.rs module). A statement holding such a key is newly served whole, so the trunk-live D1634 / D1911 /
//! D2105 / D2121 forms anywhere in its graph (`beside`) and D2070's beside the key (`ahead`) refuse by
//! name here, and D2071's run-ahead is closed for the key by D439's declaration (`user_code`). Row 2's forms refuse the
//! D2193 form anywhere in their graph (`no_widen`). Owner M8-NEST.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::sort::{SortDesc, SortForm};
use gravity_decompose::ops::shared::subplan::SubSite;
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::{Bound, Op, PipelineNode, Source};
use gravity_program::protos::{Proto, ProtoKind};
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::primnodes::ParamKind;
use types_nodes::Node;

use crate::face::kinds::KindId;
use crate::frame::iface;
use crate::frame::px::Px;
use crate::effects::visible::nestloop::{loud, loud_sub};
use crate::graph::Gx;
use crate::frame::site::{EXEC_KEY, HOST_KEY, INIT_KEY};
use crate::rerun::params::exec_key;
use crate::admit::refusal::{Reason, ShapeWhy};

/// D1016 (M6-REPS): an index key computed from executor parameters (a cast, a subscript, arithmetic over them, not a
/// bare parameter), evaluated by the host at every seek with PostgreSQL's evaluator (ExecIndexEvalRuntimeKeys at
/// ExecReScan; hosts / exec v4/m8_param_key.rs).
pub fn computed(value: Node<'_>) -> bool {
    value.as_param().is_none() && gravity_lower::rerun::words::has_exec(value)
}

/// D1630: whether index keys `keys` hold a key bound at bind whose site slot (`slots`, in the host's key word order) is
/// negative: an inner walk's host key (lifetime/bound.rs `per_open`), exec's computed-key arm's, which M1's, M2's and the any-AM arms leave.
pub fn hosts_bound(keys: &[gravity_decompose::IndexKey<'_>], slots: &[i16]) -> bool {
    use gravity_decompose::graph::KeyForm;
    let mut w = 0usize;
    keys.iter().any(|k| {
        let row = k.value.as_row_compare_expr().filter(|_| k.form == KeyForm::Row);
        let (members, single) = match k.form {
            KeyForm::Null | KeyForm::NotNull => (None, None),
            KeyForm::Row => (row, None),
            _ => (None, Some(k.value)),
        };
        members.into_iter().flat_map(|rc| rc.rargs.iter()).chain(single).any(|v| {
            w += 1;
            crate::lifetime::bound::at_bind(v) && slots.get(w - 1).is_some_and(|&s| s < 0)
        })
    })
}

/// D1016: the executor parameters key `value` reads into `ids`, each once in key then walk order (the site's input cells);
/// `false` when the walk meets a node `kids` does not know (a parameter under it would be missed).
pub fn key_ids(value: Node<'_>, ids: &mut Vec<i32>) -> bool {
    key_ids_by(value, &mut |id| if !ids.contains(&id) { ids.push(id) }) // alloc: Program
}

/// `key_ids` into a list in the planning context (the lowering's own callers).
pub fn key_ids_in(value: Node<'_>, ids: &mut PgVec<'_, i32>) -> bool {
    key_ids_by(value, &mut |id| if !ids.contains(&id) { ids.push_in(id) })
}

/// `key_ids`'s walk: each executor parameter `value` reads to `put`, as the walk meets it (repeats included), with no list
/// of its own (memsig E-prog: exec's recipe templates build theirs in their set's root, and a bind checks its walk
/// against the template's with none); `false` over a node the walk does not know.
pub fn key_ids_by(value: Node<'_>, put: &mut dyn FnMut(i32)) -> bool {
    gravity_decompose::expr::walk(value, &mut |x| {
        if let Some(p) = x.as_param().filter(|p| p.paramkind == ParamKind::PARAM_EXEC) {
            put(p.paramid);
        }
        true
    })
}

/// D1016: computed key `value`'s cell, the host's (`HOST_KEY`), its parameters into `ids`; a SubPlan in it keeps a Shape.
pub fn computed_key(node: i32, value: Node<'_>, ids: &mut PgVec<'_, i32>) -> u16 {
    if gravity_decompose::expr::has_subplan(value) {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy("an index key computed by a SubPlan over executor parameters") })
    }
    if !key_ids_in(value, ids) {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy("an index key computed from executor parameters over a node the key walk does not know") })
    }
    HOST_KEY
}

/// D1016 row 2 / D3609: a computed index key on a walk whose pipeline probes a hash build that takes no `Order`: an empty
/// build ends the re-open before the seek, where PG's call may fetch the outer first or reuse the table and evaluate it.
pub(crate) const SEEK_KEY_PROBE: &str = "a computed index key on a walk that probes a hash build (an empty build ends the re-open before the seek; D1016 row 2)";

/// D1016: the input cells after the scan keys, each parameter's (`exec_key`, read at every (re-)open), no key column. An
/// InitPlan's parameter refuses where a computed comparison or row member (`scalar`) can skip it (`skippable`: PG runs the
/// InitPlan only when the key's evaluation reaches the Param, a COALESCE / CASE arm, where its cell runs it at the open,
/// store_keys `init_key`); one keying the walk after a computed key runs ahead of it, licensed as D2174's (`init_ahead`). So
/// does one on a walk whose pipeline (`Px::making`) probes a hash build (nest/inner_probe.rs, D1018 row B0): an empty
/// build ends each re-open before the seek, where PG's later rescans reuse the table, pull the walk and evaluate the key.
/// So does one beside a key word bound at bind (lifetime/bound.rs `at_bind`) or when the pipeline starts (key values `vals`), before the host's seek;
/// then `beside`'s forms anywhere in the graph, and `ahead`'s beside a key that may raise. `true`: the walk has a host key
/// (lifetime/bound.rs `per_open`).
pub fn input_cells<'v>(px: &mut Px<'_, '_>, node: i32, ids: &[i32], scalar: &[i32], vals: impl Iterator<Item = Node<'v>> + Clone, slots: &mut PgVec<'_, u16>, attnos: &mut PgVec<'_, i16>) -> bool {
    input_cells_opening(px, node, ids, scalar, vals, slots, attnos, |_| false)
}

/// `input_cells` where key values `opens` picks are the host's at every (re-)open too, though they read no executor
/// parameter (D1643: a TABLESAMPLE value that is VOLATILE or may raise, decompose `SampleDesc::per_open`): they are
/// `per_open` keys for every rule here.
#[allow(clippy::too_many_arguments)]
pub fn input_cells_opening<'v>(px: &mut Px<'_, '_>, node: i32, ids: &[i32], scalar: &[i32], vals: impl Iterator<Item = Node<'v>> + Clone, slots: &mut PgVec<'_, u16>, attnos: &mut PgVec<'_, i16>, opens: impl Fn(Node<'v>) -> bool) -> bool {
    let opened = |px: &Px<'_, '_>, v: Node<'v>| crate::lifetime::bound::per_open(px, v) || opens(v);
    let init = |&w: &u16| w != HOST_KEY && w & EXEC_KEY != 0 && w & INIT_KEY != 0;
    // D1016 row 2: PG evaluates the runtime keys in index-qual order (ExecIndexEvalRuntimeKeys); the key cells load in that
    // order at the open (frame/site.rs store_keys, an InitPlan key's `init_key` running its InitPlan), the computed keys'
    // input cells after them, the host's keys at the seek. So an InitPlan key before every computed key runs where PG runs
    // it, and one after a computed key runs ahead of it: licensed as a guarded InitPlan key's (D2174 `init_ahead`)
    for id in slots.iter().copied().skip_while(|&w| w != HOST_KEY).filter(|w| init(w)).map(|w| (w & !(EXEC_KEY | INIT_KEY)) as i32) {
        init_ahead(px, node, id, INIT_AFTER_HOST);
    }
    let host = vals.clone().any(|v| opened(px, v));
    let probes = |p: &gravity_decompose::PipelineNode<'_>| p.ops.iter().any(|op| matches!(op, Op::Join(JoinDesc::Probe { .. })));
    // D3609: served where every probe of the walk's pipeline may take D1902's `Order` (probe_calls.rs `seek_orders`): its
    // close keeps the walk from seeking at a call PG builds first over an empty table; a probe that takes none after all
    // refuses by this name at its make (probe.rs), reading `Px::seek_keyed`
    let k = px.making.0;
    if host && px.gx.pipelines().get(k).is_none_or(|p| probes(p) && !crate::ops::hashjoin::probe_calls::seek_orders(px.gx, k) && !crate::ops::hashjoin::probe_calls::seek_calls(px.gx, k)) {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy(SEEK_KEY_PROBE) })
    }
    if host && px.gx.pipelines().get(k).is_some_and(probes) {
        px.seek_keyed.push(k); // alloc: Program
    }
    if slots.contains(&HOST_KEY) && vals.clone().any(|v| crate::lifetime::bound::at_bind(v) && !opened(px, v)) {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy("a computed index key beside a key over no executor parameter (PG evaluates the runtime keys in index-qual order at each rescan; D1016 row 2)") })
    }
    for &id in ids {
        if scalar.contains(&id) && crate::rerun::params::producer(px.gx, id) == Some(KindId::NestInitplan) && (vals.clone().any(|v| skippable(v, id)) || !reached_first(px.gx, vals.clone(), id)) {
            crate::refuse!(node, Reason::Shape { why: ShapeWhy("an index key computed over an InitPlan's parameter (D1016 row 2)") })
        }
        let w = exec_key(px, node, id);
        slots.push_in(w);
        attnos.push_in(0);
    }
    if host {
        beside(px.gx, node);
        let keys = || vals.clone().filter(|&v| opened(px, v));
        let (gx, k) = (px.gx, px.making.0);
        if keys().any(|v| gx.pipelines().get(k).is_some_and(|p| loud(gx, p, v))) {
            ahead(gx, px.arena, node, k);
        }
        let under_guard: PgVec<'_, i32> = ids.iter().copied().filter(|&id| keys().any(|v| guarded(v, id, px.mcx()))).collect_in(px.mcx());
        if keys().any(|v| crate::effects::visible::rerun::key_user_code(gx, v)) {
            px.declare_proto(Proto::new(ProtoKind::Volatile, k as u16));
        }
        for id in under_guard {
            init_ahead(px, node, id, INIT_UNDER_GUARD);
        }
    }
    host
}

/// D1016 row 2: whether executor parameter `id` sits in key `v` where PostgreSQL's interpreter can stop before it (an AND /
/// OR or COALESCE argument past the first, a CASE result or later arm, a row comparison's later pair: decompose
/// `walk_cond`). Its cell runs the InitPlan at the open (frame/site.rs `init_key`); PG runs it when the key's evaluation
/// reaches the Param, so a Param it always reaches runs at the same rescan, once nothing before it can raise (`reached_first`).
fn skippable(v: Node<'_>, id: i32) -> bool {
    let mut hit = false;
    gravity_decompose::ops::shared::subplan::walk_cond(v, false, &mut |x, cond| {
        hit |= cond && x.as_param().is_some_and(|p| p.paramkind == ParamKind::PARAM_EXEC && p.paramid == id);
        !hit
    });
    hit
}

/// D1016 row 2: whether every node PostgreSQL evaluates before it reaches executor parameter `id` (the earlier keys in
/// index-qual order, then the earlier arguments in `id`'s key) raises nothing and calls nothing with an effect
/// (union_arms.rs `quiet_in`). The cell runs the InitPlan at the open, so an earlier raise PG reports (22003 from
/// `(o.a * 1000)::int2 + (SELECT ..)`) would come after the InitPlan's own (21000); an unknown node counts as not quiet.
/// An earlier key that is an InitPlan's bare parameter runs before it either way (its key cell loads before every input
/// cell, store_keys), so it keeps the order whatever its InitPlan does.
fn reached_first<'v>(gx: &Gx<'_, '_>, vals: impl Iterator<Item = Node<'v>>, id: i32) -> bool {
    // (found `id`, every node evaluated before it quiet)
    fn reach(gx: &Gx<'_, '_>, n: Node<'_>, id: i32) -> (bool, bool) {
        crate::stack_check();
        if n.as_param().is_some_and(|p| p.paramkind == ParamKind::PARAM_EXEC && p.paramid == id) {
            return (true, true);
        }
        let (mut found, mut clean) = (false, true);
        let known = gravity_decompose::expr::kids(n, &mut |c| {
            if !found {
                let (f, cl) = reach(gx, c, id);
                found = f;
                clean &= if f { cl } else { crate::rerun::union_arms::quiet_in(gx, c, true) };
            }
        });
        if known { (found, found && clean) } else { (true, false) }
    }
    let mut clean = true;
    for v in vals {
        let (f, cl) = reach(gx, v, id);
        if f {
            return clean && cl;
        }
        let init_key = v.as_param().is_some_and(|p| p.paramkind == ParamKind::PARAM_EXEC && crate::rerun::params::producer(gx, p.paramid) == Some(KindId::NestInitplan));
        clean &= init_key || crate::rerun::union_arms::quiet_in(gx, v, true);
    }
    true
}

/// D2174: whether InitPlan parameter `id` sits in computed key `v` under a CASE, COALESCE, AND or OR: PostgreSQL's key
/// evaluation (ExecIndexEvalRuntimeKeys) may never reach it, so it may never run the InitPlan.
fn guarded(v: Node<'_>, id: i32, mcx: mcx::Mcx<'_>) -> bool {
    use types_nodes::NodeTag as T;
    let mut hit = false;
    gravity_decompose::expr::walk(v, &mut |x| {
        if !hit && matches!(x.node_tag(), T::T_CaseExpr | T::T_CoalesceExpr | T::T_BoolExpr) {
            let mut under = PgVec::new_in(mcx);
            let known = key_ids_in(x, &mut under);
            hit = !known || under.contains(&id);
        }
        true
    });
    hit
}

/// D2174 (errorhold, v5 effects): InitPlan parameter `id` read under a guard of a computed key runs at the walk's first
/// open (frame/site.rs store_keys `init_key`), where PostgreSQL runs it only when the key's evaluation reaches it: a
/// departure ahead. An InitPlan that neither raises nor acts departs from nothing; one that acts (nextval) is denied and
/// refuses by name (the plan-order run is the row executor's); one that may raise is granted with a recheck, so its error
/// before any row left re-runs the statement in plan order (`Batch::Statement`), whose outcome is PostgreSQL's.
/// Construct C2: the licence the InitPlan runs ahead under (`effects::unobservable` for one with no effects), `None`
/// when no InitPlan sets `id`.
fn init_ahead(px: &mut Px<'_, '_>, node: i32, id: i32, why: &'static str) -> Option<gravity_decompose::effects::Licence> {
    use gravity_decompose::effects::{licence, unobservable, Departure, Effects};
    let gx = px.gx;
    let init = gx.graph.init_plans.iter().find(|i| i.params.iter().any(|p| p == id))?;
    let own = Effects::of_init_plan(init);
    if own == Effects::NONE {
        return unobservable(own);
    }
    match licence(px.batch_effects(own), Departure::Ahead) {
        Ok(l) => {
            px.licensed(px.making.0, &l);
            Some(l)
        }
        Err(_) => crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) }),
    }
}

/// D1016 row 2: the refusal of an InitPlan key after a computed key whose run ahead the licence denies (PostgreSQL's
/// plan-order run is the row executor's).
pub const INIT_AFTER_HOST: &str = "an InitPlan's parameter key after a computed index key, whose InitPlan acts or may raise (run at the walk's first open, ahead of the computed key PostgreSQL evaluates first; the licence is denied: D1016 row 2)";

/// D2174: the refusal of an InitPlan key the licence denies (PostgreSQL's plan-order run is the row executor's).
pub const INIT_UNDER_GUARD: &str = "an index key reading, under a CASE / COALESCE / AND / OR guard, an InitPlan that acts or may raise (run at the walk's first open where PostgreSQL may never reach it; the licence is denied: D2174)";

/// D2194 (fail closed, D2147's class): a grouping over a hash build or merge inner run lazily (D824) crashes or hangs.
/// That name left `no_widen` at item 14 part 4 (D2325 fixed, CAND-M39 not reproduced with the ban lifted).
/// D2193: at W >= 2 the program holds its relations' toast locks to transaction end, which pg_lock_status() (1371) shows.
pub const LOCK_STATUS_TOAST: &str = "an index walk with a computed key and an = ANY key in a statement calling pg_lock_status() (a W >= 2 program holds toast locks to transaction end: D2193; D1016 row 2)";

/// D3295: a walk driving its own graph pipeline (not a nest's or SubPlan's inner) with keys the host computes at each open
/// runs that pipeline on the leader, where exec's computed keys live (ops/scan/param_key.rs): frame/mechs.rs makes it
/// `Ineligible { Host }` though the decomposer's descriptor is eligible.
pub fn host_keys_serial(px: &mut Px<'_, '_>) {
    let (src, k) = px.making;
    if src < px.gx.pipelines().len() && !px.serial.contains(&k) {
        px.serial.push(k); // alloc: Program
    }
}

/// A statement row 2 newly serves whole (source/index_only.rs `key_words`: a computed `= ANY` array,
/// or a computed key beside an `= ANY` key) refuses by name when its graph holds a pg_lock_status() call (D2193). D2178 /
/// D2179, D2180 and D2197 left at their root fixes; D2194 (a hash or merge join) at item 14 part 4, once D2325 was fixed
/// and CAND-M39 did not reproduce with the ban lifted.
pub fn no_widen(gx: &Gx<'_, '_>, node: i32) {
    let ps = gx.pipelines();
    let ops = || (0..ps.end()).flat_map(|k| ps[k].ops.iter());
    if ops().any(|op| matches!(op, Op::Srf(d) if d.functions.iter().any(|f| f.funcid == PG_LOCK_STATUS))) {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy(LOCK_STATUS_TOAST) })
    }
}

/// pg_proc.oid of pg_lock_status() (PostgreSQL 18.6 pg_proc.dat), the pg_locks view's FROM call.
const PG_LOCK_STATUS: u32 = 1371;

/// D2070: a nest's outer runs its ops before the nest over a block of outer rows ahead of the inner's rescans, where PG
/// evaluates an outer row's before its rescan evaluates the key: when the key raises at an outer row, the block has run
/// the outer's calls for the rows after it, which PG never makes. Only a call that acts shows that (`DECISIONS.md` 3); a
/// node ahead that may raise only changes which error the statement raises, and any genuine error passes (`DECISIONS.md`
/// 4 (a), item 12 part 2).
pub const OUTER_CALLS_AHEAD: &str = "a computed index key that may raise on a nest inner whose outer runs a VOLATILE or unknown call before the nest, a block of outer rows ahead of the rescans (PG: one outer row at a time; D2070; D1016 row B)";

/// D2070: from walk pipeline `k` up every enclosing nest, the outer's source rows and its ops before the nest: one that
/// acts refuses (`effects::of_expr` with no statement: a VOLATILE or unknown call, a SubPlan in an expression, a node the
/// walk does not know; a SubPlan step by its own facts). A nest before it hands its matches on one row at a time (M8
/// L0-v), a Sort or LIMIT reads its input before any rescan as PG's does, a first-of-run step compares key images, never
/// a call (runs/per_group.rs; regress subselect-128); an op not named counts.
fn ahead<'p>(gx: &Gx<'_, 'p>, arena: gravity_program::ArenaRef, node: i32, mut k: usize) {
    use gravity_decompose::effects::{self, Effects};
    let (ps, inners) = (gx.pipelines(), gx.pipelines().inners());
    let cx = effects::Cx::without_stmt(gx.facts, arena);
    let l = |n: Node<'p>| effects::of_expr(&cx, n, &effects::PLAIN).acts;
    while let Some(&((o, j), _)) = k.checked_sub(ps.len()).and_then(|i| inners.get(i)) {
        let p = &ps[o];
        let src = match &p.source {
            Source::Values { lists, .. } => lists.iter().any(l),
            Source::TableFunc { .. } => true,
            _ => false,
        };
        let op = |op: &Op<'p>| match op {
            Op::Filter(q) | Op::Gate(q) => l(*q),
            Op::Project(list) => list.iter().any(l),
            Op::SubPlan(d) => Effects::of_sub_plan(d).acts,
            Op::Join(JoinDesc::Probe { clauses, keys, qual, .. }) => clauses.iter().chain(keys.iter()).chain(qual.iter()).any(l),
            Op::Join(JoinDesc::Merge { clauses, qual, .. }) => clauses.iter().chain(qual.iter()).any(l),
            Op::Join(JoinDesc::Memo { keys, .. }) => keys.iter().any(l),
            Op::Srf(d) => d.functions.iter().any(|f| l(f.node)) || d.tlist.iter().any(l),
            Op::Join(JoinDesc::Nest { .. } | JoinDesc::Spool | JoinDesc::Drain { .. }) | Op::Sort(_) | Op::FirstOfRun(_) => false,
            _ => iface::limit_bounds(op).is_none(),
        };
        if src || p.ops[..j].iter().any(op) {
            crate::refuse!(node, Reason::Shape { why: ShapeWhy(OUTER_CALLS_AHEAD) })
        }
        k = o;
    }
}

/// D1634: a rerun set's LIMIT / OFFSET bound that is a bare parameter or a constant below its floor is a word bound once at
/// bind (`plan.key`), where PG evaluates it at each rescan, never for a run it skips (an expression bound, or one over
/// executor parameters, is read per open: params_bound.rs `admit`). A rerun set's TID value over no executor parameter is
/// the host's at every open (D3616, ops/scan/tid.rs `reopened`), as a nest inner walk's is (D3600). A TABLESAMPLE argument over no executor
/// parameter is a bind word too only where it is neither VOLATILE nor may raise (one that is is the host's at each open:
/// decompose `SampleDesc::per_open`, D1643), so its value is the one each rescan computes, and the host checks it (a NULL,
/// an out-of-range percent) at the sampler's start, each open: no refusal.
pub const BOUND_ONCE_AT_BIND: &str = "a computed index key in a statement whose rerun set binds a LIMIT / OFFSET bound other than a positive constant once (PG evaluates it at each rescan: D1634; D1016 row B)";
/// D1911: an eager nest.subplan step runs the SubPlan before the nodes its reader evaluates first (nest/subplan.rs `lazy`).
pub const EAGER_SUBPLAN_AFTER: &str = "a computed index key in a statement whose eager SubPlan PG reads after a VOLATILE or raising node (D1911; D1016 row B)";
/// D2105 / D2106: an Append that prunes (run-time or initial) in a pipeline a rerun or a nest re-opens: each re-open still
/// opens and seeks every arm's walk, its marker passing no row (exec pruning), or the prepare raises XX000 (initial). Not
/// an arm whose pruning its rerun set decides (append/prune.rs `skipped_whole`): since D2105's root fix (4bc644e0ef1) and
/// D2106's the runner skips such an arm whole when pruned, so its computed key is never evaluated, as PG's. Nor a nest
/// inner's union S2b streams (D2013 slice 2: its pruned arm never opens, or every arm and key is quiet).
pub const PRUNING_REOPEN: &str = "a computed index key in a statement whose rerun set or nest inner holds an Append that prunes (every arm's walk still opens at each re-open, where PG never opens a pruned arm: D2105 / D2106; D1016 row B)";
/// D2121: a LIMIT / OFFSET or streamed group after a union's marker keeps union words (step/limit.rs, keyed/stream.rs: the
/// first arm to open writes them, once per execution), so a re-open reads the last run's counters (the limit already met).
pub const UNION_COUNTERS_ONCE: &str = "a computed index key in a statement whose rerun set or nest inner holds a LIMIT / OFFSET or a streamed group over a union's arms (the union's counters are written at its first open only, not at each re-open: D2121; D1016 row B)";

/// Row B's in-row refusals: trunk-live forms a statement newly served by a computed key
/// or LIMIT / OFFSET (params_bound.rs) would carry, wherever the graph holds them. A kept hash build is the probe rule's above.
pub(crate) fn beside(gx: &Gx<'_, '_>, node: i32) {
    let ps = gx.pipelines();
    let loose = |m: usize, b: &Bound<'_>, min: i64| !matches!(*b, Bound::All) && !matches!(*b, Bound::Const(n) if n >= min) && !crate::rerun::params_bound::computed(b) && !crate::lifetime::bound::rebinds(gx, m, b);
    for r in gx.graph.reruns.iter() {
        for m in r.pipelines.iter().map(|&m| m as usize) {
            let p = &ps[m];
            // a LIMIT's bounds, and a top-k's or walk's (the bounds of the LIMIT it absorbed)
            let mut bounds = p.ops.iter().filter_map(|op| match op {
                Op::Sort(SortDesc { form: SortForm::TopK { count, offset, .. } | SortForm::Walk { count, offset, .. }, .. }) => Some((count, offset)),
                _ => iface::limit_bounds(op),
            });
            if bounds.any(|(count, offset)| loose(m, count, 1) || loose(m, offset, 0)) {
                crate::refuse!(node, Reason::Shape { why: ShapeWhy(BOUND_ONCE_AT_BIND) })
            }
        }
    }
    // D2013 slice 2: not the union of a nest inner S2b streams (rerun/inner_arms.rs `source`), whose arm PG prunes never
    // opens (`nest_prune`) or, where its pruning and every arm are quiet (keys too), opens with nothing PG could see
    let streamed = |k: usize| crate::pipes::inner_union(gx, k).is_some_and(|u| u.0 == k);
    let prunes = |k: usize, op: &Op<'_>| match op {
        Op::Append(AppendDesc::Arm { prune: Some(d), .. }) => !crate::ops::append::prune::skipped_whole(gx, k, d),
        Op::Append(AppendDesc::Union { prune: Some(_), .. }) => !streamed(k),
        Op::Append(AppendDesc::Merge { prune: Some(_), .. }) => true,
        _ => false,
    };
    let marker = |op: &&Op<'_>| matches!(op, Op::Append(AppendDesc::Arm { .. }) | Op::Join(JoinDesc::Drain { .. })); // Px::union_of's
    // D2105 / D2121 in a pipeline a nest or rerun re-opens; D1911 (as its row's `order`): a SubPlan that may raise or call a
    // VOLATILE function, run by an eager step, after a node its reader evaluates first that may too (in an aggregate's
    // argument, an earlier aggregate's FILTER, argument or transition, or its own; an op between the step and its reader).
    // D1911 is a write's only: in a statement that writes nothing every such SubPlan is a per-row step whose own order rule
    // (ops/subplan/per_row.rs `order`) reads it lazily where PG reads it, keeps it eager where only errors could reorder
    // (PLAN-v5 5.1) or refuses by D1911's ORDER name, so no computed key there needs this row-B guard (regress
    // create_index#108: point(x,x) before a KNN SubPlan's computed ORDER BY key). A write's check-option SubPlan is host-fed
    // (W6-D1112, write/rowop_sub.rs): the IR evaluates it only when the host's qual parks at its read, in PostgreSQL's order,
    // so no eager step runs it ahead of a node (`host_fed`; regress rowsecurity#239: an RLS check SubPlan over a computed
    // current_user key).
    let writes = ps.iter().any(|p| p.ops.iter().any(|o| matches!(o, Op::Write(_))));
    for k in 0..ps.end() {
        let p = &ps[k];
        let again = k >= ps.len() || gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(k as u16)));
        if again && p.ops.iter().any(|op| prunes(k, op)) {
            crate::refuse!(node, Reason::Shape { why: ShapeWhy(PRUNING_REOPEN) })
        }
        // D2121's root fix (rerun/inner_limit.rs `union_reset`): a rerun set's run restarts its union's words, so only a nest
        // inner's union (re-opened by no set's run) keeps them across re-opens
        if k >= ps.len() && p.ops.iter().skip_while(|op| !marker(op)).any(|op| iface::limit_bounds(op).is_some() || matches!(op, Op::GroupRun(_))) {
            crate::refuse!(node, Reason::Shape { why: ShapeWhy(UNION_COUNTERS_ONCE) })
        }
        for (j, d) in p.ops.iter().enumerate().filter_map(|(j, o)| if let Op::SubPlan(d) = o { Some((j, d)) } else { None }) {
            // a join qual's or a VALUES row's SubPlan refuses by nest/subplan.rs's name
            if !writes || !loud_sub(d) || matches!(d.site, SubSite::JoinQual | SubSite::Values) || host_fed(p, d.plan_id) {
                continue;
            }
            // an aggregate's argument: the aggregates before it in transition order, its FILTER and earlier arguments
            // (ops/subplan/per_row.rs `agg_arg`, its row's `order`)
            let (found, loud) = if d.site == SubSite::AggArg {
                crate::ops::subplan::per_row::agg_arg(gx, k, d.plan_id)
            } else {
                let mut loud = false;
                let found = p.ops[j + 1..].iter().filter(|o| !matches!(o, Op::SubPlan(_))).any(|op| match op {
                    Op::Filter(q) | Op::Gate(q) => before(gx, p, *q, d.plan_id, &mut loud),
                    Op::Project(l) => l.iter().any(|n| before(gx, p, n, d.plan_id, &mut loud)),
                    _ => {
                        loud = true;
                        false
                    }
                });
                (found, loud)
            };
            if loud || !found {
                crate::refuse!(node, Reason::Shape { why: ShapeWhy(EAGER_SUBPLAN_AFTER) })
            }
        }
    }
}

/// D1911: SubPlan `id` is read only by pipeline `p`'s write's check options, host-fed at the host qual's read (ops/subplan
/// per_row.rs `make`'s `fed`: the write carries WriteSubPlan and a check reads it), and by no filter, gate or projection
/// of `p` (those would read an eager step).
fn host_fed(p: &PipelineNode<'_>, id: i32) -> bool {
    use gravity_decompose::ops::shared::subplan::refers;
    let fed = p.ops.iter().any(|o| matches!(o, Op::Write(w) if w.members.has(gravity_decompose::ops::shared::modifytable::Member::WriteSubPlan) && w.checks.iter().any(|c| refers(c.qual, id))));
    fed && !p.ops.iter().any(|o| match o {
        Op::Filter(q) | Op::Gate(q) => refers(*q, id),
        Op::Project(l) => l.iter().any(|n| refers(n, id)),
        _ => false,
    })
}

/// D1911: whether `n` reads SubPlan `id`; `*loud` gathers the home's `Fx::loud_here` over the nodes PG evaluates before that read (children
/// first, in order; GREATEST / LEAST compares between them), over all of `n` when it does not read it; an unknown node counts.
fn before(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, n: Node<'_>, id: i32, loud: &mut bool) -> bool {
    crate::stack_check(); // D3832
    let is = |s: Node<'_>| s.as_sub_plan().is_some_and(|s| s.plan_id == id);
    if is(n) || n.as_alternative_sub_plan().is_some_and(|a| a.subplans.iter().any(is)) {
        return true;
    }
    if let Some(l) = n.as_list() {
        return l.iter().any(|c| before(gx, p, c, id, loud));
    }
    let mut found = false;
    let known = gravity_decompose::expr::kids(n, &mut |c| found = found || before(gx, p, c, id, loud));
    *loud |= (!found || n.as_min_max_expr().is_some()) && (!known || gx.fx(n).loud_here(p));
    found
}
