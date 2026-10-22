//! nest.selfspool: a nest inner PostgreSQL rescans per outer row with no Materialize over it is read from a synthetic
//! one, the graph of Nested Loop -> Materialize -> scan. A helper of join.rs `map_nest_loop`, called after its catalog
//! form stood down (HOTSPOT RULE). The form: a planner's outer estimate far under the real count leaves a plain Seq Scan
//! as the inner of a nest with no parameters, rescanned once per outer row.
//!
//! Exact when every rescan returns the same rows and nothing under the inner can show it ran: an empty `allParam`, and
//! the effects home's reading of the inner (`effects::of_plan`: acts nowhere, may raise nowhere, the same rows at every
//! run), in a statement that neither writes nor locks rows (one snapshot). The inner is one plain scan pipeline (a range
//! source, filters, projections; no slot read, no union) and the nest sits in the statement's own plan tree, on no nest's
//! inner side and in no Append member (nest.spool's one-level reader), and its outer may hand on more than one row
//! (`rescans_unbounded`). Lowering serves the spool as any plan Material one nest inner reads (lowerv4
//! rerun/spool/form.rs `spool_fill`: the pull form over a heap scan, the eager form by the nest's facts otherwise,
//! lazy.rs's first-outer-row fill where it defers), its record drawn from the statement's grant as `map_material`'s,
//! under `SPOOL_CEILING` by the planner's estimate. nest.eqkey (`keyed`, called first): an INNER or LEFT nest of the same
//! terms with an F2 equality takes map_catalog's keyed form instead, the inner hashed once and probed per outer row.
//! D3014 B6: the keyed form's inner may be a CTE Scan (its filters and projection over the store a MATERIALIZED CTE's
//! InitPlan fills): the build reads the store through its pull, lazily at the first outer row, where PostgreSQL rescans
//! the store per outer row. A body that may raise is taken where PostgreSQL reads the store to its end before that row
//! (`store_whole`) or at it (`raise_whole`, D3248); a store a union fills where lowerv4's spool
//! rule reads the union whole (`spool_whole`, D3347: a CTE over a UNION ALL that the query joins with itself). D3803
//! (`wrap_tree`, the tree form): a nest inner of several pipelines or with a nest of its own (a hash join's probe and
//! its builds over a scan or a FROM-clause function, a nest over an index lookup) whose rescan PostgreSQL answers from
//! what its first run kept is read through the same synthetic Materialize, its fill the inner's first run. D3227 K3:
//! the keyed form takes SEMI and ANTI nests too (`keyed_kind`, road nest.keep.semi), reads a plan Material inner
//! through to its child (`through_material`, road nest.keep.material: the hash table holds the rows the Material
//! would), and a nest under a Material PostgreSQL fills once is top level for it (`top_level_at`): a SEMI nest inside a
//! top nest's Materialize. Not PG-visible: the plan is PostgreSQL's own.
use mcx::GrowIn;
use types_nodes::jointype::JoinType;
use types_nodes::nodes_enums::CmdType;
use types_nodes::list::NodeList;
use types_nodes::plannodes::{Material, NestLoop, Plan, PlannedStmt};
use types_nodes::primnodes::{INNER_VAR, OUTER_VAR};
use types_nodes::Node;

use gravity_abi::breaker::{BreakerKind, Budget, Spill, HASHBUILD_ENTRY};
use gravity_program::{limits, End, Pipeline, Reason};

use crate::admit::Refusal;
use crate::effects::Licence;
use crate::graph::{Op, Source};
use crate::facts::nest::EqConjunct;
use crate::join::JoinDesc;
use crate::ledger::Kind;
use crate::ops::shared::sort::ROW_LAYOUT;
use crate::walk::{Builder, Open};

/// The largest spool the planner's estimate of the inner (its rows times a record and its width) may fill: the bytes a
/// partitioned breaker's table holds before it grows into the grant (the cost home's slice times the kernels' widest
/// fan-out, 1 GiB). nest.spool's record grows from the grant and never spills (exec ops/material/proto.rs `grow`), so a
/// larger inner keeps its rescans (constant memory) rather than risk the pool on an estimate.
const SPOOL_CEILING: u64 = gravity_program::cost::PARTITION_BYTES * gravity_abi::breaker::T1_MAX_PARTS as u64;

/// The nest's inner `inner` (mapped from plan node `inner_plan`, settled) read through a synthetic Materialize when the
/// module doc's terms hold: `inner` becomes the Materialize's emit over a new fill pipeline, closed under the inner's own
/// plan node id, and the licence its fill runs ahead under comes back: the inner's code shows nothing
/// (`effects::unobservable`; `keeps` with no keyed form takes no raising inner). Else `inner` is left as it is
/// (`None`).
pub(crate) fn wrap<'p>(b: &mut Builder<'p, '_>, nl: &'p NestLoop<'p>, inner_plan: Node<'p>, inner: &mut Open<'p>) -> Result<Option<Licence>, Refusal> {
    let Some(p) = inner_plan.as_plan() else { return Ok(None) };
    let Some(Kept::Quiet(licence)) = keeps(b, nl, p, inner_plan, inner, None) else { return Ok(None) };
    let (Some(&Op::Project(tl)), Source::Range { scanrelid, .. }) = (inner.ops.last(), &inner.source) else { return Ok(None) };
    let varno = *scanrelid as i32;
    let nc = tl.len() as u64;
    if Builder::rows(p).saturating_mul(8 * nc + nc.div_ceil(8) * 8 + p.plan_width.max(0) as u64) > SPOOL_CEILING {
        return Ok(None);
    }
    let fill = unread_null(b, p, tl, varno);
    let (Some(fill), Some(cols)) = (fill, identity(b, tl)) else { return Ok(None) };
    spool(b, inner_plan, inner, Some(fill), cols).map(|()| Some(licence))
}

/// `inner` (mapped from plan node `inner_plan`) becomes the emit of a synthetic Materialize over a new fill pipeline (the
/// inner's own, its last projection replaced by `fill` when given, then the spool), closed under the inner's plan node id
/// and projected by `cols` (`identity`).
fn spool<'p>(b: &mut Builder<'p, '_>, inner_plan: Node<'p>, inner: &mut Open<'p>, fill: Option<&'p NodeList<'p>>, cols: &'p NodeList<'p>) -> Result<(), Refusal> {
    let Some(p) = inner_plan.as_plan() else { return Ok(()) };
    let id = p.plan_node_id;
    let placeholder = Open::new(b.arena, id, Source::Const, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite));
    let mut open = core::mem::replace(inner, placeholder);
    if let (Some(last), Some(fill)) = (open.ops.last_mut(), fill) {
        *last = Op::Project(fill);
    }
    open.push(Op::Join(JoinDesc::Spool));
    // the synthetic Materialize has no plan node: its fill and its emit run under the scan's (`node_ops`), the fill's
    // breaker entry no other node's (the nest's would be a producer_top entry the frame's requests read as the emit's child)
    let cur = b.graph.cur_node.replace(id);
    let slot = crate::ops::shared::breaker::close(b, open, BreakerKind::Materialize, ROW_LAYOUT, Budget::Share, Spill::Runs, Kind::Material, id);
    b.graph.cur_node = cur;
    let slot = slot?;
    b.closed_by(inner_plan); // the fill is the scan's: its allParam (empty) is what a rescan reads
    *inner = crate::ops::shared::breaker::emit(b, id, slot);
    crate::ops::project(inner, cols);
    inner.close_node(id);
    Ok(())
}

/// D3803: the tree form of `wrap`. The nest's inner `inner` (mapped from plan node `inner_plan`, its pipelines from `p0`
/// on, settled) is read through a synthetic Materialize when `tree`'s terms hold: its fill is the inner's first run, which
/// lowering runs ahead or at the nest's first outer row by its facts (lowerv4 rerun/spool/form.rs `spool_fill`, lazy.rs),
/// and every re-open reads the record from its start; the licence the record stands under (`tree`) comes back. Else
/// `inner` is left as it is (`None`).
pub(crate) fn wrap_tree<'p>(b: &mut Builder<'p, '_>, nl: &'p NestLoop<'p>, inner_plan: Node<'p>, inner: &mut Open<'p>, p0: usize) -> Result<Option<Licence>, Refusal> {
    let Some(p) = inner_plan.as_plan() else { return Ok(None) };
    let Some(licence) = tree(b, nl, p, inner_plan, inner, p0) else { return Ok(None) };
    let Some(&Op::Project(tl)) = inner.ops.last() else { return Ok(None) };
    let nc = tl.len() as u64;
    if Builder::rows(p).saturating_mul(8 * nc + nc.div_ceil(8) * 8 + p.plan_width.max(0) as u64) > SPOOL_CEILING {
        return Ok(None);
    }
    // D3912: a top that is a scan keeps its physical target list, where a column no consumer reads has no cell in the
    // fill's row (facts::demand `entries_read`), so the record stores NULL there, as `wrap`'s does; a join's or a
    // breaker's projection carries every entry it lists
    let fill = match &inner.source {
        Source::Range { scanrelid, .. } => {
            let Some(fill) = unread_null(b, p, tl, *scanrelid as i32) else { return Ok(None) };
            Some(fill)
        }
        _ => None,
    };
    let Some(cols) = identity(b, tl) else { return Ok(None) };
    spool(b, inner_plan, inner, fill, cols).map(|()| Some(licence))
}

/// `wrap_tree`'s terms over nest `nl` and its mapped inner `inner` (plan `p`, its pipelines from `p0` on). PostgreSQL 18.6
/// runs a nest inner with no nestParams under EXEC_FLAG_REWIND and rescans it with no changed parameter, so a node that
/// keeps state answers the rescan from its first run: ExecReScanFunctionScan rewinds its tuplestore without calling the
/// function, ExecReScanHashJoin keeps its one-batch table and re-reads its outer side, a Sort, Material or CTE store
/// rewinds, a hashed Agg or SetOp keeps its table. Where what the rescan runs again (the effects home's D1920 reader
/// `of_rescan`, no parameter changed) calls nothing VOLATILE and makes the first run's rows, every re-open returns the
/// first run's rows in the first run's order: the record. The first run is PG's first inner scan, at the nest's first
/// outer row, and what it may raise the fill raises at PostgreSQL's moment (`first_placed`): PostgreSQL may cut it
/// short (an empty outer leaves a hash join's build unrun, an empty table its probe side, which every rescan of the
/// kept table then reads: D1764's and D1909's forms), and the nest form serves any other raising inner in PostgreSQL's
/// order (op 14's held build, a FROM function's store: D3378, D1085). Where lowering places the fill changes only how
/// many times a VOLATILE call in it runs (a VOLATILE call's count is free). The inner: not
/// `keeps`'s plain scan, no breaker's emit (its own forms: a sort run, a group, a settled union's arms) and no lone FROM
/// function (lowerv4's function store rewinds it per re-open, srf/function_store.rs): a slot read or a nest among its
/// steps (a hash join's probe, a nest of its own, over any source but a breaker), ending in its projection, its slots
/// filled by its own pipelines (`self_contained`); the nest in a read-only statement's own tree or an InitPlan's
/// (`top_level`). Any outer: where the nest form reads such an inner again it refuses (lowerv4 rerun/inner.rs, D1018),
/// so the spool's copy of a one-pass inner (`rescans_unbounded`'s cost) buys the statement.
fn tree<'p>(b: &mut Builder<'p, '_>, nl: &'p NestLoop<'p>, p: &'p Plan<'p>, inner_plan: Node<'p>, inner: &Open<'p>, p0: usize) -> Option<Licence> {
    let s = b.stmt;
    if s.commandType != CmdType::CMD_SELECT || s.hasModifyingCTE || s.rowMarks.iter().next().is_some() {
        return None;
    }
    let steps = !inner.desc.consumes.is_empty() || inner.ops.iter().any(|op| matches!(op, Op::Join(JoinDesc::Nest { .. })));
    let source = !matches!(inner.source, Source::Breaker { .. } | Source::Named { .. });
    if !source || !steps || inner.union.is_some() || !matches!(inner.ops.last(), Some(Op::Project(_))) {
        return None;
    }
    if !p.allParam.is_empty() || !p.initPlan.is_nil() || !nl.nestParams.is_nil() || !self_contained(b, inner, p0) {
        return None;
    }
    if b.closers.len() != b.graph.pipelines.len() || b.graph.pipelines.len() + 2 > limits::PIPELINES || crate::breakers::free_slot(&b.graph, inner).is_none() {
        return None;
    }
    // what a rescan runs again shows nothing (the record answers it), and the first run raises at PostgreSQL's moment
    let again = crate::effects::Effects::of_found(crate::rerun::of_rescan(s, b.facts, b.arena, inner_plan, &[]));
    let licence = crate::effects::unobservable(again)?;
    (first_placed(b, inner_plan, true) && top_level(b, nl)).then_some(licence)
}

/// `tree`'s first-run term over plan node `n` of the inner (`source`: `n` is on the inner's outer chain, the fill
/// pipeline from its root down the outer sides): what the first run may raise, the fill raises at PostgreSQL's moment.
/// Off the chain a subtree raises nothing (the effects home's walk), and on it each node's own expressions raise
/// nothing. A FROM function at the chain's end may raise: PostgreSQL runs it whole at its first fetch, and so does the
/// fill (lowerv4 rerun/spool/form.rs `first_run`'s source). A hash join's build may raise: lowering builds it where
/// ExecHashJoin does (after the outer's first row on the outer-first path, `join::outer_first`, so never over an empty
/// outer; first otherwise). Where PostgreSQL builds first and then ends at an empty table without its outer side (a
/// join that fills no outer row), that outer side raises nothing. A nest's inner raises nothing (it runs per outer row,
/// and a reader may stop early). What a rescan runs again raises nothing (`tree`'s `again`), so the rows the first run
/// left unread (an empty table's probe side, a stopped reader's) show nothing when PostgreSQL's rescans read them.
fn first_placed<'p>(b: &Builder<'p, '_>, n: Node<'p>, source: bool) -> bool {
    crate::walk::stack_check(); // one level per plan node of the chain
    let s = b.stmt;
    let raises = |e: Node<'p>| crate::effects::Effects::of_found(crate::rerun::of_expr(s, b.facts, b.arena, e)).may_raise;
    let quiet = |l: &NodeList<'p>| !l.iter().any(raises);
    let quiet_tree = |x: Node<'p>| !crate::effects::Effects::of_found(crate::rerun::of_plan(s, b.facts, b.arena, x)).may_raise;
    let Some(p) = n.as_plan() else { return false };
    let own = quiet(&p.targetlist) && quiet(&p.qual) && p.initPlan.is_nil();
    if let Some(hj) = n.as_hash_join() {
        let (Some(outer), Some(hash)) = (p.lefttree, p.righttree) else { return false };
        let jt = hj.join.jointype;
        let first = crate::join::outer_first(jt, p.parallel_aware, outer.as_plan().map_or(0.0, |o| o.startup_cost), hash.as_plan().map_or(0.0, |h| h.total_cost));
        let fills_outer = matches!(jt, JoinType::JOIN_LEFT | JoinType::JOIN_ANTI | JoinType::JOIN_FULL);
        let sides = if first || fills_outer { first_placed(b, outer, source) } else { quiet_tree(outer) };
        return own && quiet(&hj.join.joinqual) && quiet(&hj.hashclauses) && quiet(&hj.hashkeys) && sides;
    }
    if let Some(j) = n.as_nest_loop() {
        let params = j.nestParams.iter().all(|np| np.as_nest_loop_param().is_some_and(|np| !raises(np.paramval)));
        return own && quiet(&j.join.joinqual) && params && p.righttree.is_some_and(quiet_tree) && p.lefttree.is_some_and(|o| first_placed(b, o, source));
    }
    if let Some(q) = n.as_subquery_scan() {
        return own && q.subplan.is_some_and(|c| first_placed(b, c, source));
    }
    if n.as_function_scan().is_some() && source {
        return own;
    }
    quiet_tree(n)
}

/// D3803: every slot inner `inner` reads, and every slot its producers (its pipelines from `p0` on) read, is filled by
/// those producers and is no Materialize's (lowerv4 serves a plan Material to one nest inner alone, D1109); no producer
/// reads a named tuplestore or runs a recursive union: the fill and its producers are one group, run once.
fn self_contained<'p>(b: &Builder<'p, '_>, inner: &Open<'p>, p0: usize) -> bool {
    let own = |slot: u8| crate::breakers::decl_of(&b.graph, slot).is_some_and(|d| d.producer as usize >= p0 && d.breaker.kind != BreakerKind::Materialize);
    let producers = b.graph.pipelines.iter().skip(p0).all(|k| {
        let src = match k.source {
            Source::Breaker { slot } => own(slot),
            Source::Named { .. } => false,
            _ => true,
        };
        src && k.desc.consumes.iter().all(|&s| own(s)) && !k.ops.iter().any(|op| matches!(op, Op::SetOp(crate::ops::shared::setop::SetDesc::Iter(_))))
    });
    producers && inner.desc.consumes.iter().all(|&s| own(s))
}

/// nest.eqkey's form of a nest `wrap` would spool: map_catalog's (join.rs) HashBuild over the inner and its probe,
/// `sides` the F2 conjunct's (outer, inner) arguments, `clauses` that conjunct alone, `qual` the rest of the join qual.
pub(crate) struct Keyed<'p> {
    pub sides: (Node<'p>, Node<'p>),
    pub clauses: &'p NodeList<'p>,
    pub qual: &'p NodeList<'p>,
}

/// nest.eqkey: the nest `nl` over its mapped inner `inner` (plan node `inner_plan`, settled) read as a hash join keyed by
/// its F2 equality `eq` (facts/nest.rs `eq_conjunct`) where the module doc's terms hold. The probe is lowerv4's nest.catalog,
/// already the Nest's own: the build waits for the first outer row and an empty one ends nothing, LEFT fills a miss, it
/// never spills, the pipelines run serial and a key's records chain in record order (the inner's scan order), so the
/// rows come in the Nest's order. Exact beside the conjunct: each conjunct before it is quiet (F2), PostgreSQL runs each
/// one after it only on a pair the equality holds for, which is a match (the probe's join qual, in list order), and the
/// conjunct's outer side cannot raise (F2's `outer_raise_free`: PostgreSQL may never evaluate it). INNER and LEFT (the
/// catalog probe's kinds); a builtin hashable equality over a bare inner column; an inner that calls no user function;
/// the estimate's table under `SPOOL_CEILING`. `None`: `wrap`'s spool, or the Nest form.
pub(crate) fn keyed<'p>(b: &mut Builder<'p, '_>, nl: &'p NestLoop<'p>, inner_plan: Node<'p>, outer: &Open<'p>, inner: &Open<'p>, eq: Option<EqConjunct<'p>>, p0: usize) -> Option<Keyed<'p>> {
    let eq = eq.filter(|e| e.outer_raise_free && crate::catalog::hash_eq::builtin_hash_eq(b.facts, e.opno) && image_keyed(b, nl, e))?;
    let p = inner_plan.as_plan()?;
    if !keyed_kind(b, nl.join.jointype) || keeps(b, nl, p, inner_plan, inner, Some(p0)).is_none() {
        return None;
    }
    // an inner that calls a user function (the effects walk's `user_call`: a `LANGUAGE internal` one too, which `keeps`
    // reads quiet) would make a build lowering runs lazily on PG's outer-first path (lowerv4 build_class.rs, D824), which it
    // refuses where the probe's pipeline is eligible: the spool serves it (regress privileges#255, `b <<< 5` over leak())
    if crate::rerun::of_plan(b.stmt, b.facts, b.arena, inner_plan).user_call {
        return None;
    }
    // catalog_lookup's term: no SubPlan in the nest's join qual, target list or qual (the build row carries the inner
    // columns the probe's steps read, not a SubPlan's arguments)
    let plan = &nl.join.plan;
    if nl.join.joinqual.iter().chain(plan.targetlist.iter()).chain(plan.qual.iter()).any(crate::expr::has_subplan) {
        return None;
    }
    // map_catalog's own refusals (D1353 a bpchar key through a relabel, D1292 a collatable key with no collation) keep the
    // spool instead: the keyed form only ever replaces a served one
    let (os, oside) = crate::ops::agg::key_side(b, outer, eq.outer);
    if os.bpchar() || oside.typid.is_some_and(|t| crate::ops::shared::sort::indeterminate(b.facts, t, eq.coll)) {
        return None;
    }
    let Some(&Op::Project(tl)) = inner.ops.last() else { return None };
    let nc = tl.len() as u64;
    if Builder::rows(p).saturating_mul(HASHBUILD_ENTRY as u64 + 8 * nc + p.plan_width.max(0) as u64) > SPOOL_CEILING {
        return None;
    }
    let qual = &nl.join.joinqual;
    let c = qual.iter().nth(eq.pos as usize)?;
    // facts/nest.rs `eq_of` reads the inner column through a binary-coercible relabel (a varchar compared as text): the
    // build's key is that argument as written
    let bare = |n: &Node<'p>| crate::expr::through_relabel(*n).unwrap_or(*n).as_var().is_some_and(|v| v.varno == INNER_VAR && v.varlevelsup == 0 && v.varattno == eq.inner_attno);
    // a computed inner side (facts/nest.rs `eq_keyed`, inner_attno 0) is the build's key expression itself
    let ie = c.as_op_expr()?.args.iter().find(bare).or((eq.inner_attno == 0).then_some(eq.inner))?;
    let alloc = b.alloc?;
    let (mut one, mut rest) = (NodeList::nil(), NodeList::nil());
    one.lappend(alloc, c).ok()?;
    for (_, n) in qual.iter().enumerate().filter(|&(i, _)| i != eq.pos as usize) {
        rest.lappend(alloc, n).ok()?;
    }
    let clauses = Node::mk_list(alloc, one).ok()?.as_list()?;
    let qual = Node::mk_list(alloc, rest).ok()?.as_list()?;
    fire_widened(b, nl, p, inner_plan.as_cte_scan().map(|c| c.cteParam));
    if matches!(nl.join.jointype, JoinType::JOIN_SEMI | JoinType::JOIN_ANTI) {
        fires::fire("nest.keep.semi"); // shapecheck fire signal (D3227 K3): a SEMI / ANTI nest keyed
    }
    if inner_plan.as_material().is_some() {
        fires::fire("nest.keep.material"); // shapecheck fire signal (D3227 K3): keyed through a plan Material
    }
    Some(Keyed { sides: (eq.outer, ie), clauses, qual })
}

/// D4805: conjunct `eq` of nest `nl` keys by its images, the only key the keyed form makes (join.rs `map_catalog`: one
/// key pair by `pair`, never D2725's hash value of each side, `user_keys`): a builtin hashable equality whose two sides'
/// images pair (`join::hash_valued` false: one type, two integer types, one representation, a name beside a text). Any
/// other pair (float4 = float8, float_ops) lowerv4 refuses as "a cross-type join key" (hashjoin keys.rs `check_keys`), so
/// the nest keeps the form it had before the keyed one: the plan Material (`through_material`; gen:hj-0145 under road
/// nest.keep.material), the spool or the Nest.
fn image_keyed<'p>(b: &Builder<'p, '_>, nl: &'p NestLoop<'p>, eq: &EqConjunct<'p>) -> bool {
    nl.join.joinqual.iter().nth(eq.pos as usize).and_then(|c| c.as_op_expr()).is_some_and(|o| !crate::join::hash_valued(b.facts, o))
}

/// The nest kinds the keyed form takes: INNER and LEFT (the catalog probe's first kinds), and with road nest.keep.semi on
/// (D3227 K3) SEMI and ANTI: the probe stops at a key's first record that passes the rest of the join qual, in record
/// order (the inner's scan order), and hands the outer row on (SEMI) or drops it (ANTI), where ExecNestLoop stops its
/// inner scan at the first inner row the join qual passes (single_match) in that order; a row whose key is NULL or that no
/// record passes is dropped (SEMI) or handed on with no inner row (ANTI), as the Nest finds no match for it.
fn keyed_kind<'p>(b: &Builder<'p, '_>, jt: JoinType) -> bool {
    match jt {
        JoinType::JOIN_INNER | JoinType::JOIN_LEFT => true,
        JoinType::JOIN_SEMI | JoinType::JOIN_ANTI => b.facts.default_on_road_on(ROAD_KEEP_SEMI),
        _ => false,
    }
}

/// D3227 K3 (road nest.keep.material): nest `nl`'s inner `inner_plan` when it is a plan Material the keyed form may read
/// through: PostgreSQL fills it at the first inner scan and rewinds it at every rescan (a nest with no NestLoopParams runs
/// its inner under EXEC_FLAG_REWIND, and no rescan changes a param under it: `fixed_only`), so its rows are the child's
/// first run, which the keyed form's build reads as it reads any inner it builds once. The child is then mapped alone
/// (walk.rs `keyed_material`, join.rs `map_material`), and the nest keys it (`keyed`) or closes the Materialize itself
/// (`material_close`). Asked before the inner is mapped, so it reads `keyed`'s terms the plan answers (the statement,
/// the nest's kind and parameters, the F2 equality `eq` and its sides over the outer `outer`, the inner's params, code
/// and estimate, the nest top level); the mapped inner's shape (`keeps`'s plain or built-once inner) is `keyed`'s.
pub(crate) fn through_material<'p>(b: &mut Builder<'p, '_>, nl: &'p NestLoop<'p>, inner_plan: Node<'p>, outer: &Open<'p>, eq: Option<EqConjunct<'p>>) -> Option<&'p Material<'p>> {
    let m = inner_plan.as_material()?;
    let s = b.stmt;
    if !b.facts.default_on_road_on(ROAD_KEEP_MATERIAL) || b.alloc.is_none() || s.commandType != CmdType::CMD_SELECT || s.hasModifyingCTE || s.rowMarks.iter().next().is_some() {
        return None;
    }
    let p = &m.plan;
    if !p.qual.is_nil() || p.righttree.is_some() || p.lefttree.is_none() || !p.initPlan.is_nil() || !nl.nestParams.is_nil() || !keyed_kind(b, nl.join.jointype) {
        return None;
    }
    let eq = eq.filter(|e| e.outer_raise_free && crate::catalog::hash_eq::builtin_hash_eq(b.facts, e.opno) && image_keyed(b, nl, e))?;
    let plan = &nl.join.plan;
    if nl.join.joinqual.iter().chain(plan.targetlist.iter()).chain(plan.qual.iter()).any(crate::expr::has_subplan) || !fixed_only(b, p, None) {
        return None;
    }
    let e = crate::effects::of_plan(&crate::effects::Cx::new(s, b.facts, b.arena), inner_plan);
    if e.acts || !e.same_rows || e.may_raise || crate::rerun::of_plan(s, b.facts, b.arena, inner_plan).user_call {
        return None;
    }
    let (os, oside) = crate::ops::agg::key_side(b, outer, eq.outer);
    if os.bpchar() || oside.typid.is_some_and(|t| crate::ops::shared::sort::indeterminate(b.facts, t, eq.coll)) {
        return None;
    }
    let nc = p.targetlist.len() as u64;
    if Builder::rows(p).saturating_mul(HASHBUILD_ENTRY as u64 + 8 * nc + p.plan_width.max(0) as u64) > SPOOL_CEILING {
        return None;
    }
    let (_, filled) = top_level_at(b, nl, true)?;
    // the keyed probe runs its pipeline serial where a reader above orders the nest's rows (join.rs map_catalog, B5), and
    // the Material's spool runs a nest over it on every worker where the rows reach an order-free end by lowerv4's reading
    // (nestloop/widen.rs `spool`, D4532): read through the Material only where the keyed form costs no width: no reader
    // orders the rows (`order_free`), the session runs on the leader alone (`serial`), or the nest runs inside another
    // Material's fill (`filled`: that fill runs once, before its nest's outer rows, D2157)
    (b.order_free || b.facts.serial() == Some(true) || filled).then_some(m)
}

/// D3227 K3: the Materialize `through_material` left open, closed where the keyed form stood down: `inner` (the Material
/// `m`'s child rows under its target list, mapped from plan node `inner_plan`) becomes the Materialize's emit, its fill
/// closed under the Material's node id as `map_material` closes it, each column read through as `OUTER_VAR`, at the
/// Material's width and rows.
pub(crate) fn material_close<'p>(b: &mut Builder<'p, '_>, m: &'p Material<'p>, inner_plan: Node<'p>, inner: &mut Open<'p>) -> Result<(), Refusal> {
    let id = m.plan.plan_node_id;
    let cols = identity(b, &m.plan.targetlist).ok_or(Refusal::shape(Kind::Material, id, "a Material's columns read through (no node allocator, or a column of unknown type)"))?;
    spool(b, inner_plan, inner, None, cols)?;
    inner.desc.row_width = m.plan.plan_width.max(0) as u32;
    inner.desc.end_rows = Builder::rows(&m.plan);
    Ok(())
}

/// The fill's projection over the scan's target list `tl`: a bare column entry the nest never reads (a physical target
/// list's, `read` false: facts::demand `entries_read`; the scan carries no lane for it) spooled as a NULL of its type,
/// so the record keeps every position and each read column its own (as the DISTINCT's unread junk in ops/agg). `None`:
/// no node allocator, a column of unknown type.
fn unread_null<'p>(b: &mut Builder<'p, '_>, p: &'p Plan<'p>, tl: &'p NodeList<'p>, varno: i32) -> Option<&'p NodeList<'p>> {
    let Some(read) = crate::facts::demand::entries_read(b, p) else { return Some(tl) };
    let alloc = b.alloc?;
    let mut out = NodeList::with_capacity(alloc, tl.len()).ok()?;
    for (i, n) in tl.iter().enumerate() {
        let te = n.as_target_entry()?;
        let entry = match te.expr.as_var().filter(|v| v.varno == varno && v.varlevelsup == 0 && v.varattno > 0 && !read.get(i).is_none_or(|&r| r)) {
            Some(v) => {
                let (len, byval) = b.facts.type_row(v.vartype).map(|t| (t.typlen as i32, t.byval))?;
                let null = Node::mk_const(alloc, v.vartype, v.vartypmod, v.varcollid, len, datum::Datum::from_i64(0), true, byval).ok()?;
                Node::mk_target_entry(alloc, null, te.resno, te.resname, te.resjunk).ok()?
            }
            None => n,
        };
        out.lappend(alloc, entry).ok()?;
    }
    Node::mk_list(alloc, out).ok()?.as_list()
}

/// The Materialize's own target list over the child's projection `tl`: each column read through as `OUTER_VAR` (PG's
/// Material tlist), so the emit reads every spooled column as a plan Material's emit does. `None`: no node allocator
/// (decompose without `decompose_in`) or a column of unknown type.
pub(crate) fn identity<'p>(b: &Builder<'p, '_>, tl: &'p NodeList<'p>) -> Option<&'p NodeList<'p>> {
    let alloc = b.alloc?;
    let mcx = b.mcx;
    let mut cols: mcx::PgVec<'_, Node<'p>> = mcx::reserved_in(tl.len(), mcx);
    for (j, n) in tl.iter().enumerate() {
        let te = n.as_target_entry()?;
        let typid = crate::catalog::expr_type::type_of(te.expr)?;
        let typmod = te.expr.as_var().map_or(-1, |v| v.vartypmod);
        let var = Node::mk_var(alloc, OUTER_VAR, (j + 1) as i16, typid, typmod, crate::catalog::expr_type::collation_of(te.expr), 0).ok()?;
        cols.push_in(Node::mk_target_entry(alloc, var, (j + 1) as i16, te.resname, te.resjunk).ok()?);
    }
    NodeList::from_slice(alloc, &cols[..]).and_then(|l| mcx::alloc_leak_in(alloc, l)).ok()
}

/// The module doc's terms over nest `nl` and its mapped inner `inner` (plan `p`).
/// `keyed` (the keyed form's terms, the inner's pipelines from `p0` on): D3073, a plain scan outer at any planner estimate;
/// D3014 build-once, any outer at all, and besides a plain scan any inner that ends in its projection whose slot reads are
/// its own pipelines' (a join's build, an aggregate's or a sort's emit: each runs once, before the build it feeds). The
/// spool keeps `rescans_unbounded` (a scan outer estimated at one row spools nothing it re-reads); the keyed form builds
/// the inner once either way, about PostgreSQL's one scan plus its hashing, so an outer estimated at one row that hands on
/// many (stale statistics, or an estimate that multiplies correlated clauses) probes it instead of rescanning it.
fn keeps<'p>(b: &mut Builder<'p, '_>, nl: &'p NestLoop<'p>, p: &'p Plan<'p>, inner_plan: Node<'p>, inner: &Open<'p>, keyed: Option<usize>) -> Option<Kept> {
    let s = b.stmt;
    if s.commandType != CmdType::CMD_SELECT || s.hasModifyingCTE || s.rowMarks.iter().next().is_some() {
        return None;
    }
    let plain = matches!(inner.source, Source::Range { .. })
        && inner.union.is_none()
        && inner.desc.consumes.is_empty()
        && inner.ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_)))
        && matches!(inner.ops.last(), Some(Op::Project(_)));
    let built = keyed.is_some_and(|p0| built_once(b, inner, p0));
    // B6: a CTE Scan's allParam names its CTE's own parameter (PostgreSQL's finalize_plan adds cteParam), which no
    // rescan changes: the CTE runs once per statement (map_init_plans refuses one with parameters)
    let cte_param = inner_plan.as_cte_scan().map(|c| c.cteParam);
    if !(plain || built) || !fixed_only(b, p, cte_param) || !p.initPlan.is_nil() || !nl.nestParams.is_nil() {
        return None;
    }
    // every pipeline so far attributed (the fill alone takes the scan as its closer), and room for the fill's pipeline
    // and slot (`sort::close`'s limits)
    if b.closers.len() != b.graph.pipelines.len() || b.graph.pipelines.len() + 2 > limits::PIPELINES || crate::breakers::free_slot(&b.graph, inner).is_none() {
        return None;
    }
    let e = crate::effects::of_plan(&crate::effects::Cx::new(s, b.facts, b.arena), inner_plan);
    // B6: a store a union fills keeps its rows in the CTE's Materialize form, which lowerv4 admits only under readers that
    // take every row (D1486); a lazily built inner is not one, so such a store is taken only where it is whole before the
    // nest's first outer row, as a raising body is
    let union_store = cte_param.is_some() && built && union_fed(b, inner);
    let raising = e.may_raise || union_store;
    // D3227 (b) (road nest.key.eq): the keyed form takes a raising inner that is not a CTE's too (a date column filtered
    // against a timestamp bound), under `raise_ahead`'s licence
    let key_raise = keyed.is_some() && raising && cte_param.is_none() && b.facts.default_on_road_on(ROAD_KEY_EQ);
    if e.acts || !e.same_rows || raising && !(cte_param.is_some() && built) && !key_raise {
        return None;
    }
    if !(nl.join.plan.lefttree.is_some_and(|o| keyed.is_some() || rescans_unbounded(o)) && top_level_at(b, nl, keyed.is_some()).is_some()) {
        return None;
    }
    if !raising {
        return crate::effects::unobservable(e).map(Kept::Quiet);
    }
    // the licence asked last, so a nest the other terms keep never marks the statement for its recheck
    let whole = if key_raise {
        raise_ahead(b, nl, e).is_some()
    } else {
        store_whole(b, nl, inner_plan).is_some() || if union_store { spool_whole(b, nl, inner_plan, e).is_some() } else { raise_whole(b, nl, e).is_some() }
    };
    if key_raise && whole {
        fires::fire("nest.key.eq.raise"); // shapecheck fire signal (D3227 (b)): a raising inner keyed under its licence
    }
    whole.then_some(Kept::Raising)
}

/// D3227 (b) (road nest.key.eq): the licence a raising inner that is not a CTE's is keyed under. Where PostgreSQL's first
/// inner scan reads the inner to its end (`raise_whole`: an INNER or LEFT nest, not unique, read whole) the build raises
/// where that scan does. Elsewhere (a unique inner's scan stops at its first match, a SEMI or ANTI nest's at its first
/// passing row, so it may never reach a row that raises) only the recheck licence takes it: an error the build raises
/// before any row left re-runs the statement in plan order, where this licence is denied and the Nest is kept; so no row
/// may leave before the build has run (`rows_held_above`). `None`: the Nest.
fn raise_ahead<'p>(b: &mut Builder<'p, '_>, nl: &'p NestLoop<'p>, e: crate::effects::Effects) -> Option<Licence> {
    use crate::effects::{licence, Departure, Effects};
    if let Some(l) = raise_whole(b, nl, e) {
        return Some(l);
    }
    if !rows_held_above(b, nl) {
        return None;
    }
    let ask = e.and(Effects { acts: crate::effects::of_statement_acts(&crate::effects::Cx::new(b.stmt, b.facts, b.arena), &mut b.graph.acts_read), ..Effects::NONE });
    let l = licence(ask, Departure::Ahead).ok().filter(|l| l.recheck())?;
    b.graph.licensed(&l);
    // C2 R3: the inner's build runs this departure (the tracer's Recheck-after-send reads where)
    if let Some(inner) = nl.join.plan.righttree.and_then(|n| n.as_plan()) {
        b.graph.licensed_nodes.push(inner.plan_node_id);
    }
    Some(l)
}

/// D3227 (b): no row leaves the statement before nest `nl`'s first outer row has been joined: walking up the statement's
/// own plan tree, the nest's rows reach a reader that reads its whole input before its first row (a plain or hashed
/// aggregate, a Sort, a Hash) through nodes that hand each row on as it comes (a nest's or a hash join's outer side, a
/// Materialize, a Result, a Subquery Scan), and above that reader no Append, MergeAppend or recursive union whose other
/// members could hand rows on first, to the statement's root (not an InitPlan's or a SubPlan's).
fn rows_held_above<'p>(b: &mut Builder<'p, '_>, nl: &'p NestLoop<'p>) -> bool {
    use types_nodes::NodeTag::*;
    let mut at: &'p Plan<'p> = &nl.join.plan;
    let mut held = false;
    while let Some((up, right)) = crate::facts::demand::readers::parent_of(b, at) {
        let tag = up.node_tag();
        if !held {
            match tag {
                T_Agg => match up.as_agg() {
                    Some(a) if matches!(a.aggstrategy, crate::ops::shared::agg::AGG_PLAIN | crate::ops::shared::agg::AGG_HASHED) => held = true,
                    _ => return false,
                },
                T_Sort | T_Hash => held = true,
                T_NestLoop | T_HashJoin if !right => {}
                T_Material | T_Result | T_SubqueryScan => {}
                _ => return false,
            }
        } else if matches!(tag, T_Append | T_MergeAppend | T_RecursiveUnion | T_ModifyTable | T_LockRows) {
            return false;
        }
        let Some(pp) = up.as_plan() else { return false };
        at = pp;
    }
    held && b.stmt.planTree.and_then(|t| t.as_plan()).is_some_and(|t| core::ptr::eq(t, at))
}

/// Whether no rescan of the inner `p` changes a param it reads: its `allParam` names none but `cte_param` (B6) and, with
/// road nest.keep.initplan on (D4497 K1), the outputs of InitPlans that read no param but such ones (rerun.rs
/// `fixed_params`): PostgreSQL runs such an InitPlan once per statement, at its param's first read (ExecReScan re-runs an
/// InitPlan only for a changed param its plan reads), so every rescan reads its first values (a date inner bounded by two month subqueries).
fn fixed_only<'p>(b: &Builder<'p, '_>, p: &'p Plan<'p>, cte_param: Option<i32>) -> bool {
    let mut rest = p.allParam.iter().filter(|&x| Some(x) != cte_param).peekable();
    if rest.peek().is_none() {
        return true;
    }
    if !b.facts.default_on_road_on(ROAD_KEEP_INITPLAN) {
        return false;
    }
    let fixed = crate::rerun::fixed_params(b.stmt, b.arena);
    rest.all(|x| fixed.contains(&x))
}

/// D4497: the roads (lowerv4 face/strategy.rs names them, so `pgrust.gravity_road_off` takes them) that switch the keep
/// decision's two widenings on: InitPlan outputs as constants (`fixed_only`) and a nest under a Subquery Scan as top
/// level. Both on by default since the D4497 / D3227 flip (read through `Facts::default_on_road_on`, in
/// `ROADS_DEFAULT_ON`; not in lowerv4 road_off.rs `DEFAULT_OFF`): off under a plain `nest.keep.initplan` /
/// `nest.keep.subquery`.
pub const ROAD_KEEP_INITPLAN: &str = "nest.keep.initplan";
pub const ROAD_KEEP_SUBQUERY: &str = "nest.keep.subquery";
/// D3227 K3's two: SEMI and ANTI nests keyed (`keyed_kind`), and a keyed nest's inner read through a plan Material and a
/// nest under a Material filled once top level (`through_material`, `top_level_at`). Both on by default too (read the
/// same way): off under a plain `nest.keep.semi` / `nest.keep.material`.
pub const ROAD_KEEP_SEMI: &str = "nest.keep.semi";
pub const ROAD_KEEP_MATERIAL: &str = "nest.keep.material";
/// D3227 (b): `nest.key.eq`, the keyed form's reach widened: an inner that may raise is keyed under `raise_ahead`'s
/// licence (`keeps`), and a nest under a hashed aggregate PostgreSQL fills once on another nest's inner side is top level
/// for it (`agg_filled_once`). On by default since its flip (read through `Facts::default_on_road_on`, in
/// `ROADS_DEFAULT_ON`; not in lowerv4 road_off.rs `DEFAULT_OFF`): off under a plain `nest.key.eq`.
pub const ROAD_KEY_EQ: &str = "nest.key.eq";

/// The early run `keeps` takes, by what it stands on.
enum Kept {
    /// The inner's code shows nothing run ahead (`effects::unobservable`).
    Quiet(Licence),
    /// A raising keyed CTE inner, whose error PostgreSQL raises before the nest's first outer row too (`store_whole`)
    /// or at it (`spool_whole`, `raise_whole`).
    Raising,
}

/// D3347: a CTE Scan inner over a store a union fills (a UNION ALL CTE the query joins with itself), which nothing
/// reads whole before the nest's first outer row, is built once where lowerv4's spool rule serves the union with it
/// (rerun/union_reads.rs `spool_read`, `spool_build`): PostgreSQL reads every arm to its end, in Append order,
/// whenever the statement returns rows. The nest's outer side is a chain of nests down to a CTE Scan of the same CTE
/// (the reader reads the store as its source), each nest of the chain INNER or LEFT with no parameters over a CTE Scan
/// of it; above the nest only such nests (it their outer), then a full Sort, then Sorts, constant LIMITs of a row or
/// more and Results to the root. The build then reads the store at the first outer row, where PostgreSQL's first inner
/// scan reads it to its end; an outer with no row has read it to its end itself. The licence the build runs under: the
/// exact licence (`effects::exact`) of the inner's code `e`, which `keeps` proved acts nowhere, over the full Sort's
/// `Proof::pg_reads_whole`: that Sort reads its whole input before its first row, the outer chain's CTE Scan of the
/// same CTE among it, so PostgreSQL's demand runs the CTE whole too. `None` (the Nest kept) where the shape does not
/// hold.
fn spool_whole<'p>(b: &mut Builder<'p, '_>, nl: &'p NestLoop<'p>, inner_plan: Node<'p>, e: crate::effects::Effects) -> Option<Licence> {
    use types_nodes::nodes_enums::LimitOption;
    let cte = inner_plan.as_cte_scan().map(|c| c.ctePlanId)?;
    let chain = |j: &NestLoop<'_>| {
        matches!(j.join.jointype, JoinType::JOIN_INNER | JoinType::JOIN_LEFT) && j.nestParams.is_nil() && j.join.plan.righttree.is_some_and(|i| i.as_cte_scan().is_some_and(|c| c.ctePlanId == cte))
    };
    let mut o = nl.join.plan.lefttree;
    loop {
        let n = o?;
        if let Some(c) = n.as_cte_scan() {
            if c.ctePlanId != cte {
                return None;
            }
            break;
        }
        match n.as_nest_loop() {
            Some(j) if chain(j) => o = j.join.plan.lefttree,
            _ => return None,
        }
    }
    let mut at: &'p Plan<'p> = &nl.join.plan;
    let mut sorted = false;
    let mut sort = None;
    while let Some((up, right)) = crate::facts::demand::readers::parent_of(b, at) {
        let ok = match (sorted, up.as_nest_loop()) {
            (false, Some(j)) => !right && chain(j),
            (false, None) => {
                sort = up.as_sort().and(up.as_plan()).map(|p| p.plan_node_id);
                sorted = sort.is_some();
                sorted
            }
            (true, _) => {
                let limit = up.as_limit().is_some_and(|l| {
                    let (count, offset) = crate::ops::limit_bounds(l);
                    l.limitOption != LimitOption::LIMIT_OPTION_WITH_TIES && matches!(count, crate::graph::Bound::Const(c) if c >= 1) && matches!(offset, crate::graph::Bound::All | crate::graph::Bound::Const(0..))
                });
                limit || up.as_sort().is_some() || up.as_result().is_some_and(|r| r.resconstantqual.is_none())
            }
        };
        if !ok {
            return None;
        }
        let pp = up.as_plan()?;
        at = pp;
    }
    if !(sorted && b.stmt.planTree.and_then(|t| t.as_plan()).is_some_and(|t| core::ptr::eq(t, at))) {
        return None;
    }
    crate::effects::exact(e, crate::effects::Proof::pg_reads_whole(b.stmt, sort?)?).ok()
}

/// D3248: a CTE Scan inner that may raise (its CTE's body, or its own filters) is built once lazily, at the first outer
/// row, reading the CTE to its end. PostgreSQL's first inner scan reads it to its end there too when nothing stops that
/// scan: an INNER or LEFT nest, not unique (no single_match stop), whose every row the statement reads (`whole_read`:
/// only nodes reading their whole input above it). A read that stops at a count (a cursor's FETCH n) is the run-ahead
/// licence's, asked as ops/sort `lazy_ahead` asks it: its recheck re-runs an error before any row left in plan order,
/// where the bounded read keeps the Nest.
/// The licence the build runs under: the recheck licence when granted; denied (the plan-order run of a read taken to
/// its end, or a statement that acts elsewhere), the whole-read licence (`effects::whole_read`: the inner's code, which
/// `keeps` proved acts nowhere, runs ahead only of a reader that reads the statement whole, as PostgreSQL's first inner
/// scan reads the CTE to its end then); `None` (the Nest kept) where the inner is not read whole or a bounded read's
/// plan-order run denies it.
fn raise_whole<'p>(b: &mut Builder<'p, '_>, nl: &'p NestLoop<'p>, e: crate::effects::Effects) -> Option<Licence> {
    use crate::effects::{denying_bounded, licence, whole_read, Departure, Effects};
    let whole = matches!(nl.join.jointype, JoinType::JOIN_INNER | JoinType::JOIN_LEFT) && !nl.join.inner_unique && crate::ops::sort::whole_read(b, nl.join.plan.plan_node_id);
    if !whole {
        return None;
    }
    let ask = e.and(Effects { acts: crate::effects::of_statement_acts(&crate::effects::Cx::new(b.stmt, b.facts, b.arena), &mut b.graph.acts_read), ..Effects::NONE });
    match licence(ask, Departure::Ahead) {
        Ok(l) => {
            b.graph.licensed(&l);
            // C2 R3: the inner's build runs this departure (the tracer's Recheck-after-send reads where)
            if let (true, Some(inner)) = (l.recheck(), nl.join.plan.righttree.and_then(|n| n.as_plan())) {
                b.graph.licensed_nodes.push(inner.plan_node_id);
            }
            Some(l)
        }
        Err(_) if denying_bounded() => None,
        Err(_) => whole_read(e).ok(),
    }
}

/// B6: CTE Scan inner `inner_plan` raises only by its CTE's body, which ran to its end before the nest's first
/// outer row: a row reaching the nest from its outer side means (`whole_before`) a node that reads its whole input before
/// its first row (a Sort, a Hash, a plain or hashed Agg) read a CTE Scan of the same CTE to its end, so the store is whole
/// and any error of its body raised there, in PostgreSQL as here. The scan's own filters and projection raise nowhere.
/// The licence of what the build then runs ahead, the scan's own filters and projection over the whole store: none of
/// it shows (`effects::unobservable`).
fn store_whole<'p>(b: &Builder<'p, '_>, nl: &'p NestLoop<'p>, inner_plan: Node<'p>) -> Option<Licence> {
    let c = inner_plan.as_cte_scan()?;
    let of = |n: Node<'p>| crate::effects::Effects::of_found(crate::rerun::of_expr(b.stmt, b.facts, b.arena, n));
    let e = c.scan.plan.qual.iter().chain(c.scan.plan.targetlist.iter()).fold(crate::effects::Effects::NONE, |e, n| e.and(of(n)));
    let licence = crate::effects::unobservable(e)?;
    whole_before(nl.join.plan.lefttree, c.ctePlanId).then_some(licence)
}

/// Whether CTE Scan inner `inner` reads a store a union fills: the store's producer runs append arms or reads a union's
/// slot (ops/shared/append.rs).
fn union_fed<'p>(b: &Builder<'p, '_>, inner: &Open<'p>) -> bool {
    let Source::Breaker { slot } = inner.source else { return false };
    let q = crate::breakers::decl_of(&b.graph, slot).map(|d| d.producer as usize);
    q.and_then(|q| b.graph.pipelines.get(q)).is_some_and(|k| k.ops.iter().any(|op| matches!(op, Op::Append(_))))
}

/// Whether node `n`'s first row means CTE `cte`'s store was read to its end (`store_whole`). A join's first row needs a row
/// of its outer (INNER, LEFT, SEMI, ANTI) and, INNER or SEMI, of its inner too (an inner hash join's whole build); a Sort,
/// Incremental Sort, Limit, Material, Unique, Gather, a Result with no one-time qual, a subquery scan or a hashed or
/// sorted Agg with no grouping sets hands on no row before its input's first. Fail closed elsewhere (an Append's rows may
/// come from any arm; a plain Agg hands on a row over an empty input).
fn whole_before<'p>(n: Option<Node<'p>>, cte: i32) -> bool {
    crate::walk::stack_check(); // D3748
    use types_nodes::jointype::JoinType::{JOIN_ANTI, JOIN_INNER, JOIN_LEFT, JOIN_SEMI};
    let Some(n) = n else { return false };
    let Some(p) = n.as_plan() else { return false };
    let agg = n.as_agg().filter(|a| a.groupingSets.is_nil());
    let drains = matches!(agg, Some(a) if a.aggstrategy == 0 || a.aggstrategy == 2) || n.as_sort().is_some() || n.as_hash().is_some();
    if drains && reads_store(p.lefttree, cte) {
        return true;
    }
    let join = n.as_nest_loop().map(|j| &j.join).or_else(|| n.as_merge_join().map(|j| &j.join)).or_else(|| n.as_hash_join().map(|j| &j.join));
    if let Some(j) = join {
        let outer = matches!(j.jointype, JOIN_INNER | JOIN_LEFT | JOIN_SEMI | JOIN_ANTI) && whole_before(p.lefttree, cte);
        return outer || matches!(j.jointype, JOIN_INNER | JOIN_SEMI) && whole_before(p.righttree, cte);
    }
    let through = n.as_sort().is_some()
        || n.as_hash().is_some()
        || n.as_incremental_sort().is_some()
        || n.as_limit().is_some()
        || n.as_material().is_some()
        || n.as_unique().is_some()
        || n.as_gather().is_some()
        || n.as_gather_merge().is_some()
        || n.as_result().is_some_and(|r| r.resconstantqual.is_none())
        || matches!(agg, Some(a) if a.aggstrategy == 1 || a.aggstrategy == 2);
    match n.as_subquery_scan() {
        Some(s) => whole_before(s.subplan, cte),
        None => through && whole_before(p.lefttree, cte),
    }
}

/// Whether node `n` reads CTE `cte`'s store to its end when read to its own: a CTE Scan of it, under projections alone.
fn reads_store<'p>(n: Option<Node<'p>>, cte: i32) -> bool {
    crate::walk::stack_check(); // D3748
    let Some(n) = n else { return false };
    if let Some(c) = n.as_cte_scan() {
        return c.ctePlanId == cte;
    }
    match (n.as_subquery_scan(), n.as_result()) {
        (Some(s), _) => reads_store(s.subplan, cte),
        (None, Some(r)) if r.resconstantqual.is_none() => reads_store(r.plan.lefttree, cte),
        _ => false,
    }
}

/// D3014 build-once: inner `inner` (its pipelines from `p0` on) is one the keyed form builds once: settled (no union
/// arms of its own), ending in its projection, over a source a build pipeline reads (not a named tuplestore), and every
/// slot it reads, its breaker source's too, filled by its own pipelines, never an outer pipeline's slot nor a Material's
/// (lowerv4 serves a plan Material to one nest inner alone, D1109: such an inner keeps its Nest), and a CTE's store only
/// as a CTE Scan's own read of it (B6: filters and projections over a non-recursive CTE's store). Its
/// producer pipelines (`p0` on) are self-contained the same way: each reads only slots its fellow producers fill (no CTE
/// store mapped before the inner, no named tuplestore) and none runs a recursive union, whose build lowerv4 takes only
/// lazily for its own probe (D2635), never inside a group built once.
fn built_once<'p>(b: &Builder<'p, '_>, inner: &Open<'p>, p0: usize) -> bool {
    let mine = |slot: u8| crate::breakers::decl_of(&b.graph, slot).is_some_and(|d| d.producer as usize >= p0);
    let own = |slot: u8| mine(slot) && crate::breakers::decl_of(&b.graph, slot).is_some_and(|d| d.breaker.kind != BreakerKind::Materialize);
    // B6: a CTE Scan's read of a CTE store mapped before the inner (the CTE's InitPlan pipeline, no recursive union),
    // with filters and its projection alone over it: the build reads the store as any graph reader pulls it, at the first
    // outer row (D824), where PostgreSQL's first inner scan reads it to its end
    let cte = |slot: u8| {
        crate::breakers::decl_of(&b.graph, slot).is_some_and(|d| {
            let q = d.producer as usize;
            b.graph.init_plans.iter().any(|i| i.link == crate::ops::shared::subplan::SubLink::Cte && i.slot == Some(slot) && i.pipeline as usize == q)
                && !b.graph.pipelines.get(q).is_some_and(|k| k.ops.iter().any(|op| matches!(op, Op::SetOp(crate::ops::shared::setop::SetDesc::Iter(_)))))
        })
    };
    // the CTE Scan's emit (map_cte_scan: breaker::emit) consumes the store's slot, and nothing else
    let cte_slot = match inner.source {
        Source::Breaker { slot } if !own(slot) && cte(slot) => Some(slot),
        _ => None,
    };
    let quiet = inner.desc.consumes.iter().all(|&s| Some(s) == cte_slot) && inner.ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_)));
    let src = match inner.source {
        Source::Breaker { slot } => own(slot) || (quiet && cte_slot == Some(slot)),
        Source::Named { .. } => false,
        _ => true,
    };
    let producers = b.graph.pipelines.iter().skip(p0).all(|k| {
        let read = match k.source {
            Source::Breaker { slot } => mine(slot),
            Source::Named { .. } => false,
            _ => true,
        };
        read && k.desc.consumes.iter().all(|&s| mine(s)) && !k.ops.iter().any(|op| matches!(op, Op::SetOp(crate::ops::shared::setop::SetDesc::Iter(_))))
    });
    src && producers && inner.union.is_none() && matches!(inner.ops.last(), Some(Op::Project(_))) && inner.desc.consumes.iter().all(|&s| own(s) || Some(s) == cte_slot)
}

/// Whether the outer `outer` may hand on more than one row by the planner's own reading: an outer whose estimate composes
/// selectivities (a join, an aggregate, a union, a subquery) is the guess that misses (a one-row estimate over thousands
/// of rows), and a SeqScan whose estimate (a base relation's, from its statistics; keyed exactly by the front walk) is two
/// rows or more. A SeqScan expected to hand on one row keeps its one inner scan, which a spool would only copy (a heap
/// inner read once costs three times its scan through the pull form's copy), and another leaf (an index or function
/// scan, VALUES, a CTE read) keeps its rescans: its estimate is not in the front key.
fn rescans_unbounded(outer: Node<'_>) -> bool {
    let Some(op) = outer.as_plan() else { return false };
    let composed = op.lefttree.is_some() || op.righttree.is_some() || outer.as_append().is_some() || outer.as_merge_append().is_some() || outer.as_subquery_scan().is_some();
    composed || outer.as_seq_scan().is_some() && Builder::rows(op) >= 2
}

/// The nest is in the statement's own plan tree or in an InitPlan's (a CTE's body too: D3088), on no nest's inner side
/// and in no Append / MergeAppend member (the walk up stops at a member: facts/demand/readers.rs `parent_of`). An InitPlan's
/// tree is one ops/subplan `map_init_plans` maps as producers of the statement's own program, run once per (re)arm of
/// its attach node with no parameters of its own (it refuses parParam / args), so its nest rescans its inner per outer
/// row just as the top tree's does and the spool's fill joins that body's producers. A per-row SubPlan's tree is not one.
/// D4497 (K2, road nest.keep.subquery): the walk passes a Subquery Scan (readers.rs `parent_through_subquery`), which
/// PostgreSQL rescans as its subplan (a grouped nest read through a subquery that computes over its rows).
fn top_level<'p>(b: &mut Builder<'p, '_>, nl: &'p NestLoop<'p>) -> bool {
    top_level_at(b, nl, false).is_some()
}

/// `top_level`'s walk: `Some((subquery, filled))` when the nest is top level, `subquery` whether the walk went through a
/// Subquery Scan, `filled` whether through a Material filled once. `material` (the keyed form's, road nest.keep.material,
/// D3227 K3): the walk also passes a plan Material on a nest's inner side that PostgreSQL fills once (`filled_once`), and
/// goes on above that nest: the nests under such a Material run once, at its fill, as a top-level nest's inner runs at each
/// outer row. Not a Material over a draining hash join (`drains_under`): with a nest under it kept, lowering's spooled
/// drain refuses N1 (D2129) where the unkept nest's form is served (e2e rxnested_d3545_drain_tail_group_inner).
fn top_level_at<'p>(b: &mut Builder<'p, '_>, nl: &'p NestLoop<'p>, material: bool) -> Option<(bool, bool)> {
    let subquery = b.facts.default_on_road_on(ROAD_KEEP_SUBQUERY);
    // D3227 (b) (road nest.key.eq): the keyed form's walk also passes a hashed aggregate on a nest's inner side that
    // PostgreSQL fills once (`agg_filled_once`)
    let agg = material && b.facts.default_on_road_on(ROAD_KEY_EQ);
    let material = material && b.facts.default_on_road_on(ROAD_KEEP_MATERIAL);
    let mut at: &'p Plan<'p> = &nl.join.plan;
    let mut at_node: Option<Node<'p>> = None;
    let (mut passed, mut filled) = (false, false);
    loop {
        let up = match subquery {
            true => crate::facts::demand::readers::parent_through_subquery(b, at),
            false => crate::facts::demand::readers::parent_of(b, at),
        };
        let Some((up, right)) = up else { break };
        if let (true, Some(j)) = (right, up.as_nest_loop()) {
            let once = material && at_node.filter(|&n| !drains_under(n)).and_then(|n| n.as_material()).is_some_and(|m| filled_once(b, m, j));
            let kept = agg && at_node.filter(|&n| !drains_under(n)).is_some_and(|n| agg_filled_once(b, n, j));
            if !(once || kept) {
                return None;
            }
            if kept {
                fires::fire("nest.key.eq.agg"); // shapecheck fire signal (D3227 (b)): top level through a kept hashed aggregate
            }
            filled = true;
        }
        passed |= up.as_subquery_scan().is_some();
        at = up.as_plan()?;
        at_node = Some(up);
    }
    let top = b.stmt.planTree.and_then(|t| t.as_plan()).is_some_and(|t| core::ptr::eq(t, at)) || init_plan_root(b.stmt, at);
    top.then_some((passed, filled))
}

/// D3227 (b) (road nest.key.eq): hashed aggregate `n`, the inner of nest `j`, is filled once across every rescan that reads
/// the same rows: `j` has no NestLoopParams, no rescan changes a param `n`'s subtree reads (`fixed_only`), and the keep
/// table's answer for `n` at the widest rescan (lifetime `at_any_rescan`: ExecReScanAgg re-reads its filled table, or
/// fills it again from the same input when it spilled), resolved against the fill's effects, keeps it: the subtree acts
/// nowhere and returns the same rows, so no refill shows. The nests under it run once, at its fill, as a top-level nest's
/// inner runs at each outer row (e.g. a second UNION arm under the top nest's inner HashAggregate). Not an
/// aggregate over a draining hash join (`drains_under`, as K3's Material walk).
fn agg_filled_once<'p>(b: &Builder<'p, '_>, n: Node<'p>, j: &'p NestLoop<'p>) -> bool {
    let Some(a) = n.as_agg() else { return false };
    if !j.nestParams.is_nil() || a.aggstrategy != crate::ops::shared::agg::AGG_HASHED || !fixed_only(b, &a.plan, None) {
        return false;
    }
    let Some(child) = a.plan.lefttree else { return false };
    let fill = crate::effects::of_plan(&crate::effects::Cx::new(b.stmt, b.facts, b.arena), child);
    let fixed = crate::rerun::fixed_params(b.stmt, b.arena);
    let rule = crate::lifetime::at_any_rescan(b.stmt, n, &fixed, b.arena);
    matches!(crate::lifetime::resolve(rule, fill, crate::lifetime::Site::default()), crate::lifetime::Kept::Keep { .. })
}

/// D3227 K3: plan Material `m`, the inner of nest `j`, keeps its first fill across every rescan: `j` has no NestLoopParams
/// (ExecInitNestLoop gives its inner EXEC_FLAG_REWIND) and no rescan changes a param `m`'s child reads (`fixed_only`), so
/// ExecReScanMaterial rewinds its tuplestore and never runs the child again.
fn filled_once<'p>(b: &Builder<'p, '_>, m: &'p Material<'p>, j: &'p NestLoop<'p>) -> bool {
    let rewinds = crate::lifetime::eflags_in(b.stmt, m.plan.plan_node_id).is_some_and(|f| f & crate::lifetime::REWIND != 0);
    j.nestParams.is_nil() && rewinds && m.plan.qual.is_nil() && fixed_only(b, &m.plan, None)
}

/// Whether the tree under `n` holds a hash join that drains (RIGHT, FULL, RIGHT SEMI, RIGHT ANTI: join.rs `hash_drains`).
fn drains_under(n: Node<'_>) -> bool {
    let mut found = false;
    crate::rerun::each_node(n, &mut |k| found |= k.as_hash_join().is_some_and(|h| crate::join::hash_drains(h.join.jointype)));
    found
}

/// D4497's fires where a keyed nest took a widening: K1 (its inner reads an InitPlan's output as a constant) and K2 (it sits
/// under a Subquery Scan).
fn fire_widened<'p>(b: &mut Builder<'p, '_>, nl: &'p NestLoop<'p>, p: &'p Plan<'p>, cte_param: Option<i32>) {
    if p.allParam.iter().any(|x| Some(x) != cte_param) {
        fires::fire("nest.keep.initplan"); // shapecheck fire signal (D4497 K1): InitPlan outputs read as constants
    }
    if top_level_at(b, nl, true).is_some_and(|(subquery, _)| subquery) {
        fires::fire("nest.keep.subquery"); // shapecheck fire signal (D4497 K2): a nest under a Subquery Scan kept
    }
}

/// Whether plan `at` is the tree of a subplan some plan node's `initPlan` names with no parameters (a CTE or a once-run
/// expression). PostgreSQL gives every SubPlan its own `subplans` entry (an expression SubPlan never shares one).
fn init_plan_root<'p>(s: &'p PlannedStmt<'p>, at: &'p Plan<'p>) -> bool {
    let Some(i) = s.subplans.iter().position(|n| n.and_then(|n| n.as_plan()).is_some_and(|p| core::ptr::eq(p, at))) else { return false };
    let plan_id = i as i32 + 1;
    let mut init = false;
    crate::rerun::each_plan_node(s, &mut |n| {
        let Some(p) = n.as_plan() else { return };
        init |= p.initPlan.iter().filter_map(|x| x.as_sub_plan()).any(|sp| sp.plan_id == plan_id && sp.parParam.is_nil() && sp.args.is_nil());
    });
    init
}

#[cfg(test)]
mod tests {
    use super::init_plan_root;
    use types_nodes::list::{IntList, NodeList, OptNodeList};
    use types_nodes::plannodes::{CteScan, NestLoop, Plan, PlannedStmt, Scan, SeqScan};
    use types_nodes::primnodes::{SubLinkType, SubPlan};
    use types_nodes::Node;

    /// D3088: a nest in a parameterless InitPlan's tree (a MATERIALIZED CTE's body) is in a once-run tree as the
    /// statement's own is; a subplan no InitPlan names (a per-row SubPlan's) and one with parameters are not.
    #[test]
    fn an_init_plans_tree_is_a_once_run_root() {
        let ctx = mcx::MemoryContext::new("selfspool-root");
        let m = ctx.mcx();
        let scan = |id: i32| Node::mk(m, SeqScan { scan: Scan { plan: Plan { plan_node_id: id, ..Default::default() }, scanrelid: 1 }, ..Default::default() }).unwrap();
        let nest = |id: i32| Node::mk(m, NestLoop { join: types_nodes::plannodes::Join { plan: Plan { plan_node_id: id, lefttree: Some(scan(id + 1)), righttree: Some(scan(id + 2)), ..Default::default() }, ..Default::default() }, nestParams: NodeList::nil() }).unwrap();
        let sub = |plan_id: i32, link: SubLinkType, par: &[i32]| Node::mk(m, SubPlan { subLinkType: link, plan_id, parParam: IntList::from_slice(m, par).unwrap(), ..Default::default() }).unwrap();
        // subplan 1: a CTE's body; 2: an EXPR InitPlan with a parameter; 3: named by no InitPlan (a per-row SubPlan's)
        let (cte, par, per_row) = (nest(10), nest(20), nest(30));
        let inits = NodeList::from_slice(m, &[sub(1, SubLinkType::CTE_SUBLINK, &[]), sub(2, SubLinkType::EXPR_SUBLINK, &[7])]).unwrap();
        let top = Node::mk(m, CteScan { scan: Scan { plan: Plan { plan_node_id: 0, initPlan: inits, ..Default::default() }, scanrelid: 1 }, ctePlanId: 1, cteParam: 0, gravity_fill: false }).unwrap();
        let stmt = PlannedStmt { planTree: Some(top), subplans: OptNodeList::from_slice(m, &[Some(cte), Some(par), Some(per_row)]).unwrap(), ..Default::default() };
        assert!(init_plan_root(&stmt, cte.as_plan().unwrap()), "a CTE's body");
        assert!(!init_plan_root(&stmt, par.as_plan().unwrap()), "an InitPlan with parameters");
        assert!(!init_plan_root(&stmt, per_row.as_plan().unwrap()), "a subplan no InitPlan names");
        assert!(!init_plan_root(&stmt, top.as_plan().unwrap()), "the statement's own tree is no subplan");
        let inner = cte.as_plan().unwrap().righttree.unwrap();
        assert!(!init_plan_root(&stmt, inner.as_plan().unwrap()), "a node under the body is no root");
    }
}
