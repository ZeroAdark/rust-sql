//! Grouping sets: the Agg node with a grouping-sets chain, one walk per set (split out of `ops/agg/mod.rs`, v5 agg A2).
use super::*;

/// A grouping-sets aggregate as the planner's rewrite would plan it (ruling 2026-09-12: rewrites go through the planner;
/// decision 70): one hashed (or plain) group per grouping set of the node and its chain, each over its own walk of the
/// input, the arms a streamed union (V4R-130: map_append's form, `push_union` copies the parent's ops into every arm;
/// the Materialize form was nest.spool's; a sorted node keeps it). Every keyed arm carries `SetDecl { arm, arms }` (the empty set's is a plain fold). Each arm's target list and qual
/// are the node's with PG's projection of that set folded in (`arm_lists`). The union declares no order (PG 18 nulls
/// the grouped columns above the node, so no parent's pathkeys name them; decision 70 records the never-nulled prefix).
pub(super) fn map_sets<'p>(b: &mut Builder<'p, '_>, a: &'p Agg<'p>) -> Result<Open<'p>, Refusal> {
    let (id, k) = (a.plan.plan_node_id, Kind::Agg);
    let mut arms: ArenaVec<(&'p Agg<'p>, Node<'p>)> = ArenaVec::new_in(b.arena);
    let mut phase_of: ArenaVec<u16> = ArenaVec::new_in(b.arena);
    for (p, node) in core::iter::once(a).chain(a.chain.iter().filter_map(|n| n.as_agg())).enumerate() {
        for set in node.groupingSets.iter() {
            arms.push((node, set));
            phase_of.push(p as u16);
        }
    }
    if arms.is_empty() {
        return Err(Refusal::shape(k, id, "a MIXED / chained aggregate without a grouping-set list"));
    }
    let n = arms.len();
    // PG's parser caps a statement at 4096 sets (parse_agg.c); the arm index is u16
    if u16::try_from(n).is_err() {
        return Err(Refusal::shape(k, id, "more grouping sets than the u16 arm index holds"));
    }
    // each set's grouped input columns (its members through its node's grpColIdx), and their union over the node and
    // its chain: nodeAgg's all_grouped_cols, the columns a set's projection nulls when the set does not group them
    let mut grouped: ArenaVec<ArenaVec<i16>> = ArenaVec::new_in(b.arena);
    let mut all: ArenaVec<i16> = ArenaVec::new_in(b.arena);
    for &(node, set) in arms.iter() {
        let empty = set.as_list().is_some_and(|l| l.is_nil());
        let members = set.as_int_list().filter(|_| !empty).ok_or(Refusal::shape(k, id, "a grouping set that is not an integer list"));
        let mut cols = ArenaVec::new_in(b.arena);
        for pos in members.map_or(types_nodes::list::IntList::nil().iter(), |m| m.iter()) {
            let c = *node.grpColIdx.get(pos as usize).ok_or(Refusal::shape(k, id, "a grouping-set member outside the node's key list"))?;
            cols.push(c);
            if !all.contains(&c) {
                all.push(c);
            }
        }
        grouped.push(cols);
    }
    // Every arm is hashed (or plain): the rollup's input Sort is not part of the rewrite, its input is (a Sort does not project).
    let input = a.plan.lefttree.and_then(|c| c.as_sort()).map_or(&a.plan, |srt| &srt.plan);
    let order = match a.aggstrategy == AGG_SORTED && n > 1 && !resorted(b.stmt.planTree, id) {
        true => RollupOrder::of(b, a, k, id)?,
        false => None,
    };
    // D1371 (v0.4 refusals): the set's arm streams over the node's Sort (`arm_order`)
    let streams = arm_order(b, a, &grouped);
    // D3029 (gsetsagg form T): one tagged group over the input walked once, where it takes the node
    if n > 1 && !streams {
        let sets = super::gsets_tag::Sets { arms: &arms, phase_of: &phase_of, grouped: &grouped, all: &all };
        match super::gsets_tag::decline(b, a, &sets) {
            None => return super::gsets_tag::map(b, a, &sets, order, k, id),
            Some(why) => fires::fire(why),
        }
    }
    let mut spool = if n > 1 { Spool::of(b, a, &phase_of, k, id)? } else { None };
    let mut opens: ArenaVec<Open<'p>> = ArenaVec::with_capacity_in(n, b.arena);
    // the pipeline count before each arm is mapped, then after the last (`UnionRec::starts`, as map_append records its
    // children's): arm `i`'s own pipelines are `starts[i]..starts[i + 1]`
    let mut starts = ArenaVec::with_capacity_in(n + 1, b.arena);
    for i in 0..n {
        starts.push(b.graph.pipelines.len().min(u16::MAX as usize) as u16);
        let (node, set) = arms[i];
        let empty = set.as_list().is_some_and(|l| l.is_nil());
        let members = set.as_int_list().filter(|_| !empty).ok_or(Refusal::shape(k, id, "a grouping set that is not an integer list"));
        let tl = match &order {
            Some(o) => o.arm_tlist(b, a, phase_of[i], &grouped[i], k, id)?,
            None => &a.plan.targetlist,
        };
        let (tlist, qual) = arm_lists(b, a, tl, &grouped[i], &all, i > 0, k, id)?;
        let (tlist, qual, mut child) = match spool.as_mut() {
            Some(s) => (s.rewrite(b, tlist, k, id)?, s.rewrite(b, qual, k, id)?, s.arm(b, phase_of[i], k, id)?),
            None if streams && i == 0 => (tlist, qual, b.child(&a.plan, k)?),
            None => (tlist, qual, b.child(input, k)?),
        };
        let mut sh = Shape::new(b.arena);
        for pos in members.map_or(types_nodes::list::IntList::nil().iter(), |m| m.iter()) {
            let c = *node.grpColIdx.get(pos as usize).ok_or(Refusal::shape(k, id, "a grouping-set member outside the node's key list"))?;
            sh.key(b, &child, c, node.grpCollations.get(pos as usize).copied().unwrap_or(0), k, id)?;
            sh.eq_op(b, node.grpOperators.get(pos as usize).copied().unwrap_or(0)); // D2163: the arm's own operator
        }
        let nset = sh.keys.len();
        // D416 per set: an input column the set's lists still read outside the aggregates (not nulled: a column the
        // set's keys determine, as check_functional_grouping admits over the columns every set groups) joins its keys
        sh.duplicate_keys(b, &child, &[tlist, qual], query_grouping(tlist), &[], k, id)?;
        let refs = collect(b.arena, &[tlist, qual], k, id)?;
        let mut distinct_key = NONE;
        for r in refs.iter().copied() {
            if expr::has_subplan(r) {
                child = crate::ops::subplan::attach_agg_arg(b, r, k, id, child)?;
            }
            cell(b, &child, r, &mut sh, &mut distinct_key, k, id)?;
        }
        let plain = sh.keys.is_empty();
        let form = match (distinct_key != NONE, plain) {
            (true, _) => GroupForm::Distinct,
            (false, true) => GroupForm::Plain,
            _ => GroupForm::Hashed,
        };
        let numgroups = node.numGroups.max(0) as u64;
        if streams && i == 0 {
            // D1371 (v0.4 refusals): one group at a time over the Sort's run, in its key order (`arm_order`)
            keys::ordered(b, &child, &mut sh.keys[..nset], k, id, None)?;
            let mut emit = super::stream(b, child, id, k, [None; 2], sh, GroupForm::Sorted, numgroups)?;
            crate::ops::filters(&mut emit, qual);
            crate::ops::project(&mut emit, tlist);
            let emit = crate::ops::list_subplans(b, &[tlist, qual], k, id, emit)?;
            if n == 1 {
                b.graph.note(k);
                return Ok(emit);
            }
            opens.push(emit);
            continue;
        }
        let groups = match form {
            GroupForm::Distinct => child.desc.predicted_rows.max(numgroups),
            _ => numgroups,
        };
        // D1370: the arm's group is the head's (`id`): a chained Agg is not in the plan tree (setrefs never numbers it,
        // its plan_node_id stays 0) and the head's lists carry every phase's aggregates (exec agg.arena reads them there)
        let (mut emit, _) = close(b, child, id, k, form, [None; 2], sh, ORDER_NONE, false, groups, numgroups, None)?;
        // the empty set's arm is a plain fold (fold.plain's end and emit: one row over any input, nodeAgg's `()` set), not
        // a keyed arm: graph.rs routes a group with a SetDecl through the keyed emit, which reads an owned table
        if form != GroupForm::Plain {
            b.graph.groups.last_mut().expect("just declared").set = Some(SetDecl { arm: i as u16, arms: n as u16, whole: read_whole(b.stmt.planTree, id) });
        }
        crate::ops::filters(&mut emit, qual);
        crate::ops::project(&mut emit, tlist);
        // the arm's own SubPlans (its lists: GROUPING() and the nulled columns folded in) before their readers
        let emit = crate::ops::list_subplans(b, &[tlist, qual], k, id, emit)?;
        if n == 1 {
            // One set (`GROUP BY ()`, or a single set the parser kept): the arm is the node, no union
            // slot — the shape the planner's union rewrite hands over for a one-arm level.
            b.graph.note(k);
            return Ok(emit);
        }
        opens.push(emit);
    }
    starts.push(b.graph.pipelines.len().min(u16::MAX as usize) as u16);
    if let Some(s) = spool {
        s.release(b);
    }
    // the streamed union (map_append's form): the arms are held until the parent's end is pushed (`push_union`)
    let u = b.graph.unions.len() as u32;
    b.graph.unions.push(crate::ops::shared::append::UnionRec { arms: opens, order: Order::NONE, prune: None, starts, pushed: 0..0, sets: true });
    let mut out = Open::new(b.arena, id, Source::Const, crate::ops::shared::union::BASE);
    out.union = Some(u);
    b.graph.note(k);
    // a sorted node's rows come in its rollup order (each group, then the prefix sets it closes), which the planner can
    // take as the statement's ORDER BY with no Sort above (groupingsets#12): the arms stream into a full sort over the
    // order's keys (D467); a phase without its Sort keeps the Materialize form (nest.spool)
    match order {
        Some(o) => o.sorted(b, out, a, k, id),
        None if a.aggstrategy == AGG_SORTED && !resorted(b.stmt.planTree, id) => Ok(crate::ops::shared::union::settle(&mut b.graph, out)),
        None => Ok(out),
    }
}

/// D1371 (v0.4 refusals): whether the node's first set is mapped as a streamed sorted group over the node's own Sort
/// (mod.rs `stream`), so that its arm finalizes the set's groups one at a time in the Sort's key order, as nodeAgg's
/// rollup does, and before the empty set's arm folds: an aggregate whose ORDER BY call acts (`sorted_calls_stream`: a
/// user transition's NOTICE or sequence call) shows the order the groups are finalized in, which a hashed arm's table
/// order is not. PostgreSQL's finalize order is the arms' order (each arm's groups, then the next arm's) when the node
/// is one sorted phase (no chain) whose sets are one non-empty set then only the empty set (`ROLLUP (k)`, `GROUPING
/// SETS ((k), ())`): nodeAgg finalizes each group of the set as its run ends and the empty set's at the input's end.
/// The arms walk the input again (no spool): only where `Spool::of` keeps none (a plain scan, no VOLATILE input).
fn arm_order<'p>(b: &Builder<'p, '_>, a: &'p Agg<'p>, grouped: &[ArenaVec<i16>]) -> bool {
    let Some(left) = a.plan.lefttree.filter(|c| c.as_sort().is_some()) else { return false };
    if a.aggstrategy != AGG_SORTED || a.chain.iter().next().is_some() || grouped.first().is_none_or(|g| g.is_empty()) || grouped[1..].iter().any(|g| !g.is_empty()) {
        return false;
    }
    let lists = [&a.plan.targetlist, &a.plan.qual];
    let cx = crate::effects::Cx::new(b.stmt, b.facts, b.arena);
    if !crate::effects::agg::sorted_calls_stream(&cx, &lists, &|r| crate::facts::stream::sorts_builtin(b, r)) {
        return false;
    }
    let Ok(refs) = collect(b.arena, &lists, Kind::Agg, a.plan.plan_node_id) else { return false };
    let sampled = refs.iter().any(|&r| crate::effects::agg::agg_inputs(r).any(|i| Sampled::expr_reads(b, i)));
    let volatile = crate::effects::of_plan(&cx, left).acts || crate::effects::agg::volatile_inputs(&cx, &lists);
    !volatile && !sampled && !Sampled::plan_reads(b, left) && (rewalk_cheap(left) || walks_wide(left))
}

/// D1061 (the user's 05:03Z rule: a VOLATILE call runs where and as often as PostgreSQL runs it): when the node's input
/// or an aggregate's input calls something VOLATILE, the arms read the input walked ONCE, not once per set. nodeAgg
/// evaluates the aggregates' inputs once per row per sorted phase, in that phase's order (hashed sets ride the first
/// sorted phase; each later rollup re-sorts the input rows by its chain Sort's keys): each phase evaluates them into
/// columns of a run its arms read, and the arms' aggregates read those columns. Runs are sorts (runs.sort): a zero-key
/// one where the order is moot (the arms are hashed). D1644: so too when the input or an aggregate input reads a
/// TABLESAMPLE whose second walk draws another sample (`Sampled`), and whenever re-walking the input costs more than the
/// run (`rewalk_cheap`: anything but a plain scan of built-in expressions; one walk of a join, not one per set).
pub(super) struct Spool<'p> {
    input: &'p NodeList<'p>,
    /// The input's columns: `OUTER_VAR` 1..=input.len(), a NULL for one no node reads (`input_cols`).
    bare: &'p NodeList<'p>,
    /// `bare`, then each precomputed aggregate input.
    cols: &'p NodeList<'p>,
    steps: ArenaVec<&'p NodeList<'p>>,
    aggs: ArenaVec<(i32, Node<'p>)>,
    /// The original aggregates that call a SubPlan, each with the precompute step its SubPlans run before (D2110: after
    /// the steps of the aggregates before it, PG's per-row transition order): attached to each phase's walk.
    sub: ArenaVec<(usize, Node<'p>)>,
    /// Per node of `[a] + chain`: its phase; per phase: its chain node, its arms, its run.
    phase: ArenaVec<u16>,
    nodes: ArenaVec<&'p Agg<'p>>,
    arms: ArenaVec<u16>,
    runs: ArenaVec<Option<u8>>,
    /// D3054: phase 0's input and every aggregate input are free of VOLATILE calls, TABLESAMPLE reads and SubPlans: only
    /// then may phase 0's run be the record spool (its arms read the stored rows by claims, in no fixed order).
    plain: bool,
    /// Per phase: its arms not yet mapped; per run: its phase, slot, and whether its hold is dropped.
    left: ArenaVec<u16>,
    held: ArenaVec<(u16, u8, bool)>,
}

impl<'p> Spool<'p> {
    pub(super) fn of(b: &mut Builder<'p, '_>, a: &'p Agg<'p>, node_of: &[u16], k: Kind, id: i32) -> Result<Option<Spool<'p>>, Refusal> {
        let Some(left) = a.plan.lefttree else { return Ok(None) };
        let refs = collect(b.arena, &[&a.plan.targetlist, &a.plan.qual], k, id)?;
        let sampled = refs.iter().any(|&r| crate::effects::agg::agg_inputs(r).any(|i| Sampled::expr_reads(b, i)));
        let cx = crate::effects::Cx::new(b.stmt, b.facts, b.arena);
        let volatile = crate::effects::of_plan(&cx, left).acts || crate::effects::agg::volatile_inputs(&cx, &[&a.plan.targetlist, &a.plan.qual]);
        let plain = !volatile && !sampled && !Sampled::plan_reads(b, left);
        if plain && (rewalk_cheap(left) || walks_wide(left)) {
            return Ok(None);
        }
        let mcx = b.alloc.ok_or(Refusal::shape(k, id, "grouping sets: no node allocator (decompose_in)"))?;
        let oom = |_| Refusal::shape(k, id, "grouping sets: out of memory building the input run");
        let input = &left.as_plan().ok_or(Refusal::shape(k, id, "an aggregate over a node that is not a plan"))?.targetlist;
        let (mut phase, mut nodes, mut sorted) = (ArenaVec::new_in(b.arena), ArenaVec::new_in(b.arena), false);
        nodes.push(a);
        for node in core::iter::once(a).chain(a.chain.iter().filter_map(|n| n.as_agg())) {
            if node.aggstrategy == AGG_HASHED || node.aggstrategy == AGG_MIXED || !core::mem::replace(&mut sorted, true) {
                phase.push(0u16);
            } else {
                phase.push(nodes.len() as u16);
                nodes.push(node);
            }
        }
        // D1644: each later phase maps an aggregate input's SubPlans again, where PG's one PlanState reads one sample in
        // every phase: a heap BERNOULLI / SYSTEM sample's maps share its plan node's seed (D1920: hosts
        // HostRegistry::sample_seed), so only another sample refuses
        let unseeded = || refs.iter().any(|&r| crate::effects::agg::agg_inputs(r).any(|i| Sampled::expr_reads_unseeded(b, i)));
        if nodes.len() > 1 && sampled && unseeded() {
            return Err(Refusal::shape(k, id, "grouping sets: an aggregate input's SubPlan over a TABLESAMPLE of an extension method, or of a pgrc relation without REPEATABLE, re-evaluated by a later sorted phase (its source draws its own seed; PG: one seed per PlanState; D1644)"));
        }
        let mut arms = ArenaVec::new_in(b.arena);
        arms.resize(nodes.len(), 0u16);
        for &x in node_of {
            arms[phase[x as usize] as usize] += 1;
        }
        let bare = input_cols(b, a, input).map_err(oom)?;
        let mut left = ArenaVec::new_in(b.arena);
        left.extend(arms.iter().copied());
        let mut s = Spool { input, bare, cols: bare, steps: ArenaVec::new_in(b.arena), aggs: ArenaVec::new_in(b.arena), sub: ArenaVec::new_in(b.arena), phase, nodes, arms, runs: ArenaVec::new_in(b.arena), left, held: ArenaVec::new_in(b.arena), plain };
        s.precompute(b, &refs).map_err(oom)?;
        s.runs.resize(s.nodes.len(), None);
        // phase 0 walks the input with its own Sort: PG's first phase reads it in that order
        let open = b.child(&a.plan, k)?;
        let open = s.phase_walk(b, open, k, id)?;
        s.run_of(b, 0, open, k, id)?;
        Ok(Some(s))
    }

    /// D3029 (form T, gsets_tag.rs): the precompute steps alone, one phase, no run: the tagged group's single walk
    /// computes each aggregate's inputs into columns once per input row, before the fan-out repeats the row per set.
    pub(super) fn steps_of(b: &Builder<'p, '_>, a: &'p Agg<'p>, k: Kind, id: i32) -> Result<Spool<'p>, Refusal> {
        let left = a.plan.lefttree.ok_or(Refusal::shape(k, id, "grouping sets: an aggregate without an input"))?;
        b.alloc.ok_or(Refusal::shape(k, id, "grouping sets: no node allocator (decompose_in)"))?;
        let oom = |_| Refusal::shape(k, id, "grouping sets: out of memory computing the aggregates' inputs");
        let refs = collect(b.arena, &[&a.plan.targetlist, &a.plan.qual], k, id)?;
        let input = &left.as_plan().ok_or(Refusal::shape(k, id, "an aggregate over a node that is not a plan"))?.targetlist;
        let bare = input_cols(b, a, input).map_err(oom)?;
        let mut nodes = ArenaVec::new_in(b.arena);
        nodes.push(a);
        let mut phase = ArenaVec::new_in(b.arena);
        phase.push(0u16);
        let mut s = Spool { input, bare, cols: bare, steps: ArenaVec::new_in(b.arena), aggs: ArenaVec::new_in(b.arena), sub: ArenaVec::new_in(b.arena), phase, nodes, arms: ArenaVec::new_in(b.arena), runs: ArenaVec::new_in(b.arena), left: ArenaVec::new_in(b.arena), held: ArenaVec::new_in(b.arena), plain: false };
        s.precompute(b, &refs).map_err(oom)?;
        Ok(s)
    }

    /// The walk's columns after the precompute steps (the input's, then each aggregate input's).
    pub(super) fn cols(&self) -> &'p NodeList<'p> {
        self.cols
    }

    pub(super) fn width(&self) -> usize {
        self.cols.len()
    }

    /// Each aggregate's inputs as columns after the input's, in `aggno` order (PG's per-row order of its transitions): a
    /// FILTER first, then the arguments, an argument under a FILTER as `CASE WHEN filter THEN arg END` (ExecBuildAggTrans
    /// runs it only on rows the FILTER passes). A column reads the ones before it, so a computed FILTER ends its step.
    pub(super) fn precompute(&mut self, b: &Builder<'p, '_>, refs: &[Node<'p>]) -> types_error::PgResult<()> {
        let mcx = b.alloc.expect("checked by `of`");
        let mut w = Steps { mcx, n: self.input.len(), cols: self.cols, step: None, steps: ArenaVec::new_in(b.arena) };
        for r in refs.iter().copied() {
            let ag = r.as_aggref().expect("collect keeps aggregates");
            // D2110: its SubPlans run after the earlier aggregates' inputs (their step closed here), not ahead of them all
            let sub = crate::effects::agg::agg_inputs(r).any(expr::has_subplan);
            if sub {
                w.cut()?;
                self.sub.push((w.steps.len(), r));
            }
            let (filter, computed) = match ag.aggfilter {
                Some(f) if !trivial(f) => {
                    let v = w.add(f)?;
                    w.cut()?;
                    (Some(v), true)
                }
                f => (f, false),
            };
            let mut args: Option<NodeList<'p>> = None;
            for (j, te) in ag.args.iter().filter_map(|t| t.as_target_entry()).enumerate() {
                if trivial(te.expr) {
                    continue;
                }
                let e = match filter {
                    Some(f) => case_when(b, f, te.expr)?,
                    None => te.expr,
                };
                let v = w.add(e)?;
                let l = match &mut args {
                    Some(l) => l,
                    None => args.insert(ag.args.clone_in(mcx)?),
                };
                let t = TargetEntry { expr: v, resno: te.resno, resname: te.resname, ressortgroupref: te.ressortgroupref, resorigtbl: te.resorigtbl, resorigcol: te.resorigcol, resjunk: te.resjunk };
                l.as_mut_slice()[j] = Node::mk(mcx, t)?;
            }
            if args.is_none() && !computed {
                if sub {
                    // no column of its own (a SubPlan in a direct argument only): its SubPlans run ahead of every step
                    self.sub.last_mut().expect("pushed above").0 = 0;
                }
                continue;
            }
            let args = match args {
                Some(l) => l,
                None => ag.args.clone_in(mcx)?,
            };
            let new = Aggref { aggargtypes: ag.aggargtypes.clone_in(mcx)?, aggdirectargs: ag.aggdirectargs.clone_in(mcx)?, args, aggorder: ag.aggorder.clone_in(mcx)?, aggdistinct: ag.aggdistinct.clone_in(mcx)?, aggfilter: filter, ..*ag };
            self.aggs.push((ag.aggno, Node::mk(mcx, new)?));
        }
        w.cut()?;
        (self.cols, self.steps) = (w.cols, w.steps);
        Ok(())
    }

    /// A phase's walk from its input rows: the precompute steps, each aggregate's SubPlans attached before the step that
    /// holds its inputs (D2110: after the earlier aggregates' steps, as nodeAgg evaluates each aggregate's FILTER and
    /// arguments in transition order; the eager step ran them ahead of every aggregate's inputs). The step is pushed first,
    /// so `place` finds its reader in this walk: a later phase's walk evaluates the arguments again, and with no reader in
    /// it `place` put the op into phase 0's closed walk (D1047's refusal for every later phase). D1911 G5: under a hashed
    /// or mixed head each such step, wherever `place` put it, is marked `set_spool` (the row executor's fallback evaluates
    /// an argument once per hashed set, D2090: lowerv4 keeps the eager step, one call per input row).
    pub(super) fn phase_walk(&self, b: &mut Builder<'p, '_>, mut open: Open<'p>, k: Kind, id: i32) -> Result<Open<'p>, Refusal> {
        let mut ids = ArenaVec::new_in(b.arena);
        for (_, r) in self.sub.iter() {
            expr::walk(*r, &mut |x| {
                ids.extend(x.as_sub_plan().map(|s| s.plan_id));
                true
            });
        }
        let last = self.steps.len().saturating_sub(1);
        for at in 0..self.steps.len().max(1) {
            crate::ops::project(&mut open, self.steps.get(at).copied().unwrap_or(self.bare));
            for (_, r) in self.sub.iter().filter(|s| s.0 == at || at == last && s.0 > last) {
                open = crate::ops::subplan::attach_agg_arg(b, *r, k, id, open)?;
            }
        }
        let hashed = self.nodes[0].aggstrategy == AGG_HASHED || self.nodes[0].aggstrategy == AGG_MIXED;
        for op in b.graph.pipelines.iter_mut().flat_map(|p| p.ops.iter_mut()).chain(open.ops.iter_mut()) {
            if let Op::SubPlan(d) = op {
                d.set_spool |= hashed && d.site == crate::ops::shared::subplan::SubSite::AggArg && ids.contains(&d.plan_id);
            }
        }
        Ok(open)
    }

    /// Phase `p`'s walk into its run (held until `release`), or kept as it is for a lone arm of a later phase whose
    /// aggregates call no SubPlan (one that does keeps its run, as phase 0's: the walk's projection reads the SubPlan, the
    /// arm's aggregates its column).
    pub(super) fn run_of(&mut self, b: &mut Builder<'p, '_>, p: usize, mut open: Open<'p>, k: Kind, id: i32) -> Result<Option<Open<'p>>, Refusal> {
        if p > 0 && self.arms[p] == 1 && self.sub.is_empty() {
            return Ok(Some(open));
        }
        // D3054: phase 0's run as a record spool (lowerv4 runs.gsets_spool: an owned spool its fill writes per worker, the
        // order-free arms reading it by record claims), within the bound a spool holds in memory; past it the Sort run,
        // which spills
        // a streamed union's arms continue a fold's or a full sort's slot, not a spool's (lowerv4 append D393): its run stays
        if p == 0 && self.plain && self.sub.is_empty() && self.resident() && open.union.is_none() && !open.ops.iter().any(|op| matches!(op, Op::Append(_))) {
            crate::ops::project(&mut open, self.cols);
            let slot = crate::ops::shared::breaker::close(b, open, BreakerKind::Materialize, crate::ops::shared::sort::ROW_LAYOUT, Budget::Share, Spill::Runs, k, id)?;
            if let Some(d) = breakers::decl_of(&b.graph, slot) {
                b.graph.spool_runs.push(gravity_program::LifeId(d.producer));
            }
            b.graph.reads[slot as usize] += 1;
            self.held.push((0, slot, false));
            self.runs[0] = Some(slot);
            return Ok(None);
        }
        open.push(Op::Sort(crate::ops::shared::sort::SortDesc { keys: ArenaVec::new_in(b.arena), presorted: 0, form: crate::ops::shared::sort::SortForm::Runs, tlist: self.cols }));
        let slot = crate::ops::shared::breaker::close(b, open, BreakerKind::Sort, crate::ops::shared::sort::keys_layout(b.arena, core::iter::empty()), Budget::Share, Spill::Runs, k, id)?;
        b.graph.reads[slot as usize] += 1;
        self.held.push((p as u16, slot, false));
        self.runs[p] = Some(slot);
        Ok(None)
    }

    /// D3054: whether phase 0's input, as the planner sizes it, fits the bound a record spool keeps in memory (an owned
    /// spool does not spill: `cost::breaker::SPOOL_RESIDENT_BYTES`).
    fn resident(&self) -> bool {
        let Some(c) = self.nodes[0].plan.lefttree.and_then(|n| n.as_plan()) else { return false };
        let rec = 8.0 * self.cols.len() as f64 + (self.cols.len() as f64 / 8.0).ceil() * 8.0;
        c.plan_rows * (rec + c.plan_width.max(0) as f64) <= gravity_program::cost::breaker::SPOOL_RESIDENT_BYTES as f64
    }

    /// The input an arm of node `x` reads: its phase's run, built at the phase's first arm. A run's hold drops once its
    /// last reader holds its read: its phase's arms, and for phase 0's run every later phase's re-sort.
    pub(super) fn arm(&mut self, b: &mut Builder<'p, '_>, x: u16, k: Kind, id: i32) -> Result<Open<'p>, Refusal> {
        let p = self.phase[x as usize] as usize;
        let open = self.arm_open(b, p, k, id)?;
        self.left[p] -= 1;
        let later = (1..self.left.len()).all(|q| self.left[q] < self.arms[q]);
        for h in self.held.iter_mut().filter(|h| !h.2 && self.left[h.0 as usize] == 0 && (h.0 != 0 || later)) {
            breakers::unread(&mut b.graph, h.1);
            h.2 = true;
        }
        Ok(open)
    }

    pub(super) fn arm_open(&mut self, b: &mut Builder<'p, '_>, p: usize, k: Kind, id: i32) -> Result<Open<'p>, Refusal> {
        if self.runs[p].is_none() {
            let open = self.resort(b, p, k, id)?;
            let open = self.phase_walk(b, open, k, id)?;
            if let Some(lone) = self.run_of(b, p, open, k, id)? {
                return Ok(lone);
            }
        }
        let mut e = crate::ops::shared::breaker::emit(b, id, self.runs[p].expect("built above"));
        crate::ops::project(&mut e, self.cols);
        Ok(e)
    }

    /// Phase `p`'s input rows: phase 0's run re-sorted by the chain node's Sort (nodeAgg's next-phase tuplesort).
    pub(super) fn resort(&mut self, b: &mut Builder<'p, '_>, p: usize, k: Kind, id: i32) -> Result<Open<'p>, Refusal> {
        let mut src = crate::ops::shared::breaker::emit(b, id, self.runs[0].expect("phase 0 is built first"));
        crate::ops::project(&mut src, self.bare);
        let Some(s) = self.nodes[p].plan.lefttree.and_then(|c| c.as_sort()).filter(|s| s.numCols > 0) else { return Ok(src) };
        let n = (s.numCols.max(0) as usize).min(s.sortColIdx.len()).min(s.sortOperators.len()).min(s.nullsFirst.len());
        let mut keys = ArenaVec::new_in(b.arena);
        for j in 0..n {
            let (c, opno, coll) = (s.sortColIdx[j], s.sortOperators[j], s.collations.get(j).copied().unwrap_or(0));
            let desc = b.facts.key(crate::catalog::KeyReq::sort(opno)).and_then(crate::catalog::KeyOrder::dir).ok_or(Refusal::shape(k, id, "a sort operator outside the direction table"))?;
            let t = crate::ops::shared::sort::column_type(self.input, c, k, id)?;
            keys.push(crate::ops::shared::sort::SortKey::of_type(b.facts, c, opno, desc, s.nullsFirst[j], t, crate::ops::shared::sort::key_coll(b.facts, t, coll, k, id)?));
        }
        let layout = crate::ops::shared::sort::keys_layout(b.arena, keys.iter().map(|x| x.kind));
        src.push(Op::Sort(crate::ops::shared::sort::SortDesc { keys, presorted: 0, form: crate::ops::shared::sort::SortForm::Runs, tlist: self.bare }));
        let slot = crate::ops::shared::breaker::close(b, src, BreakerKind::Sort, layout, Budget::Share, Spill::Runs, k, id)?;
        let mut e = crate::ops::shared::breaker::emit(b, id, slot);
        crate::ops::project(&mut e, self.bare);
        Ok(e)
    }

    /// `l` with each aggregate reading its precomputed inputs.
    pub(super) fn rewrite(&self, b: &Builder<'p, '_>, l: &'p NodeList<'p>, k: Kind, id: i32) -> Result<&'p NodeList<'p>, Refusal> {
        if self.aggs.is_empty() {
            return Ok(l);
        }
        let mcx = b.alloc.ok_or(Refusal::shape(k, id, "grouping sets: no node allocator (decompose_in)"))?;
        let oom = |_| Refusal::shape(k, id, "grouping sets: out of memory reading the input run");
        match nodes_core::mutate_list_dyn(mcx, l, &mut |x| self.swap(mcx, x)).map_err(oom)? {
            Some(new) => Ok(Node::mk_list(mcx, new).map_err(oom)?.as_list().expect("a list node")),
            None => Ok(l),
        }
    }

    pub(super) fn swap(&self, mcx: mcx::Mcx<'p>, x: Node<'p>) -> types_error::PgResult<Option<Node<'p>>> {
        crate::walk::stack_check(); // D3748
        if let Some(ag) = x.as_aggref() {
            return Ok(self.aggs.iter().find(|(no, _)| *no == ag.aggno).map(|(_, n)| *n));
        }
        nodes_core::expression_tree_mutator_dyn(mcx, x, &mut |y| self.swap(mcx, y))
    }

    /// A hold `arm` has not dropped (none once every arm is mapped).
    pub(super) fn release(self, b: &mut Builder<'p, '_>) {
        for h in self.held.iter().filter(|h| !h.2) {
            breakers::unread(&mut b.graph, h.1);
        }
    }
}

/// The precompute projections being built: each passes the columns before it on and appends its own.
pub(super) struct Steps<'p> {
    mcx: mcx::Mcx<'p>,
    n: usize,
    cols: &'p NodeList<'p>,
    step: Option<NodeList<'p>>,
    steps: ArenaVec<&'p NodeList<'p>>,
}

impl<'p> Steps<'p> {
    /// Column `e` appended; the `OUTER_VAR` a later reader reads it by.
    pub(super) fn add(&mut self, e: Node<'p>) -> types_error::PgResult<Node<'p>> {
        let (mcx, n) = (self.mcx, self.n);
        let l = match &mut self.step {
            Some(l) => l,
            None => self.step.insert(self.cols.clone_in(mcx)?),
        };
        self.n += 1;
        l.lappend(mcx, Node::mk_target_entry(mcx, e, self.n as i16, None, false)?)?;
        Node::mk_var(mcx, OUTER_VAR, self.n as i16, nodes_core::expr_type(e), nodes_core::expr_typmod(e), nodes_core::expr_collation(e), 0)
    }

    /// The open step closed: the columns after it are what the next one reads.
    pub(super) fn cut(&mut self) -> types_error::PgResult<()> {
        if let Some(l) = self.step.take() {
            let l = Node::mk_list(self.mcx, l)?.as_list().expect("a list node");
            self.steps.push(l);
            self.cols = outer_vars(self.mcx, l, self.n)?;
        }
        Ok(())
    }
}

/// D1602 / D1814 / D2221 (errorhold, v5 effects): a sorted group that does not stream folds every group's input rows
/// (its feeders' filters, its arguments) before it hands the first group on, and may finish its groups in another order,
/// where PostgreSQL's GroupAggregate hands each group on before it reads the next group's rows: under a consumer that
/// stops (a LIMIT, a cursor) a later group's input runs where PostgreSQL never runs it. That is a departure ahead. Code
/// that neither raises nor acts departs from nothing; code that may raise is granted with a recheck (an error before any
/// row left re-runs the statement in plan order); in the plan-order run (every licence denied) the group keeps its Sort
/// and streams (D1071's form, PostgreSQL's). D3540: a statement that acts (a write, a row lock, an acting call) has no
/// plan-order run, so a group whose own code may raise but does not act streams there too, and a write never raises a
/// later group's error past the consumer's stop. D4236: a group whose fold may run after a row left (`late_run`: run
/// again per outer row, or in a SubPlan run on demand) streams too, its recheck unanswerable there. An ask denied
/// because the group's own code acts (a user comparator) folds ahead as before: the streamed group is not built for
/// every aggregate (keyed.distinct_multi, a user ORDER BY), and a refusal there would unserve a statement PostgreSQL
/// answers. Whether the fold may go ahead.
pub(super) fn fold_ahead<'p>(b: &mut Builder<'p, '_>, a: &'p Agg<'p>) -> bool {
    use crate::effects::{licence, of_agg_sorts, of_list, of_plan, Cx, Departure, Effects, PLAIN};
    // a consumer above that reads every group before its first row leaves (a Sort, a plain or hashed Agg; a write's
    // ModifyTable) never stops: every group runs in PostgreSQL too, and which of their errors comes first is free
    let cx = Cx::new(b.stmt, b.facts, b.arena);
    let acts = crate::effects::of_statement_acts(&crate::effects::Cx::new(b.stmt, b.facts, b.arena), &mut b.graph.acts_read);
    if drained_above(b, &a.plan, acts) {
        return true;
    }
    // a Sort below drains its whole input at the group's first fetch in PostgreSQL too: only the group's own lists fold ahead
    let below = a.plan.lefttree.filter(|c| c.as_sort().is_none()).map_or(Effects::NONE, |c| of_plan(&cx, c));
    // an aggregate sort that calls user code acts: the fold-ahead form runs a later group's sort where a stopping consumer
    // never reaches it in PostgreSQL (m6_d1371_user_ordered: a cursor's FETCH 1 under `USING <<<`, whose comparator notices)
    let sorts = a.plan.targetlist.iter().fold(Effects::NONE, |e, n| e.and(of_agg_sorts(b.facts, n)));
    let own = below.and(sorts).and(handed_on(b, &cx, &a.plan.targetlist)).and(of_list(&cx, &a.plan.qual, &PLAIN));
    if own == Effects::NONE {
        return true;
    }
    // D4236 / D4249: a fold that may run after a row left the statement (`late_run`) has no plan-order run to answer
    // its error: the group streams, as PostgreSQL's
    if !own.acts && late_run(b, a) {
        return false;
    }
    let ask = if own.may_raise { own.and(Effects { acts, ..Effects::NONE }) } else { own };
    match licence(ask, Departure::Ahead) {
        Ok(l) => {
            b.graph.licensed(&l);
            b.ahead_licensed.push(a.plan.plan_node_id);
            b.graph.licensed_nodes.push(a.plan.plan_node_id);
            true
        }
        // outside the plan-order run a denial is the group's own acting code or (D3540) the statement's acting set
        Err(_) => !crate::effects::denying() && own.acts,
    }
}

/// D4236 / D4249: whether the sorted group of `a` may fold after a row left the statement, as mapping sees it from the
/// plan, where the recheck licence's plan-order run cannot answer the fold's error (exec admit `recheck`: `sent == 0`):
/// - its plan reads a parameter that changes per outer row (`allParam` past `rerun::fixed_params`: a correlated SubPlan,
///   a LATERAL or parameterised nest inner): every run after the first comes after the outer's earlier rows may have
///   left (outer row 6 of `SELECT o.a, (SELECT sum(1 / (u.c - 5)) .. WHERE u.a <= o.a GROUP BY u.c ORDER BY u.c DESC
///   LIMIT 1) FROM o` reached a group past the stop that PostgreSQL never folds);
/// - it sits in the plan of a SubPlan that is no CTE's (an InitPlan or a SubPlan, run on demand at its first read,
///   which may come after a UNION ALL arm sent its row).
/// A group in a CTE's plan is lowerv4's to answer (D4203: ops/ctescan/lower.rs runs the producers first or refuses).
pub(super) fn late_run<'p>(b: &Builder<'p, '_>, a: &'p Agg<'p>) -> bool {
    crate::rerun::runs_late(b.stmt, b.arena, &a.plan)
}

/// The effects of target list `l` as a group hands it on: an aggregate's ORDER BY / DISTINCT comparators sort the group's
/// own rows at its final, as the group is handed on, in PostgreSQL and in the fold-ahead form alike, so an Aggref counts
/// its function, arguments, direct arguments and FILTER only; the rest of the list counts whole (m6_d1371_user_ordered:
/// a VOLATILE comparator of `array_agg(x ORDER BY x USING <<<)` is no departure). No node allocator: the whole list.
fn handed_on<'p>(b: &Builder<'p, '_>, cx: &crate::effects::Cx<'_, 'p>, l: &'p NodeList<'p>) -> crate::effects::Effects {
    use crate::effects::{of_expr, of_list, Effects, PLAIN};
    fn bare<'p>(mcx: mcx::Mcx<'p>, x: Node<'p>) -> types_error::PgResult<Option<Node<'p>>> {
        crate::walk::stack_check(); // D3748
        match x.as_aggref().filter(|g| !g.aggorder.is_nil() || !g.aggdistinct.is_nil()) {
            Some(ag) => {
                let a = Aggref { aggargtypes: ag.aggargtypes.clone_in(mcx)?, aggdirectargs: ag.aggdirectargs.clone_in(mcx)?, args: ag.args.clone_in(mcx)?, aggorder: NodeList::nil(), aggdistinct: NodeList::nil(), ..*ag };
                Ok(Some(Node::mk(mcx, a)?))
            }
            None => nodes_core::expression_tree_mutator(mcx, x, &mut |c| bare(mcx, c)),
        }
    }
    let Some(mcx) = b.alloc else { return of_list(cx, l, &PLAIN) };
    let strip = |n: Node<'p>| bare(mcx, n).map(|m| m.unwrap_or(n));
    l.iter().fold(Effects::NONE, |e, n| e.and(strip(n).map_or(Effects::UNKNOWN, |n| of_expr(cx, n, &PLAIN))))
}

/// Whether walking the aggregate's input once per set costs no more than spooling it: a plain scan of a table (sequential,
/// index, index-only or bitmap heap; under the Agg's own Sort, which the rewrite drops) whose target list and qual call only
/// built-in functions and operators and hold no sub-plan. Anything else (a join, a subquery, a function scan, a CTE
/// reference, a sort below the Agg's, an expression the walk cannot read or that calls user code) the arms read walked
/// once (the spool), as nodeAgg reads its child once (rv Q4: a forced nest under GROUPING SETS ((a), ()) ran twice).
pub(super) fn rewalk_cheap(left: Node<'_>) -> bool {
    let n = match left.as_sort() {
        Some(s) => match s.plan.lefttree {
            Some(c) => c,
            None => return false,
        },
        None => left,
    };
    // a ProjectSet of built-in set-returning calls over such a scan re-walks as cheaply (D3029 NOTE: the spool over one
    // leases a slot past its reads, core_leases tsrf-022 / 023, so it keeps the per-set walk)
    let (n, srf) = match n.as_project_set().and_then(|ps| ps.plan.lefttree) {
        Some(c) => (c, n.as_plan()),
        None => (n, None),
    };
    let scan = n.as_seq_scan().is_some() || n.as_index_scan().is_some() || n.as_index_only_scan().is_some() || n.as_bitmap_heap_scan().is_some();
    let Some(p) = n.as_plan().filter(|_| scan) else { return false };
    fn cheap(x: Node<'_>) -> bool {
        match (x.as_func_expr(), x.as_op_expr()) {
            (Some(f), _) => crate::effects::builtin_func(f.funcid).is_some(),
            (_, Some(o)) => crate::effects::builtin_func(o.opfuncid).is_some(),
            _ => true,
        }
    }
    let srf = srf.into_iter().flat_map(|s| s.targetlist.iter().chain(s.qual.iter()));
    p.targetlist.iter().chain(p.qual.iter()).chain(srf).all(|e| crate::expr::walk(e, &mut cheap))
}

/// D3054: whether each set's own walk of a join input runs wide (a morsel-parallel probe pipeline at W8), so the per-set
/// walks beat the spool, whose fill and arm reads run serially (a run's emit has no driving site): a ROLLUP of five
/// columns over two hash joins ran about 4x slower at W8 spooled than walked per set. A tree of hash joins (their Hash
/// builds) over sequential scans, every list calling only built-in functions and operators, with no sub-plan. A nested
/// loop (its inner re-runs per outer row: one walk costs far more than reading the run, and an index inner makes it
/// serial, the admission's D2984 `IndexSource`; D3029), an index scan, a merge join (a sorted source), or anything else
/// keeps the spool.
fn walks_wide(left: Node<'_>) -> bool {
    let left = match left.as_sort() {
        Some(s) => match s.plan.lefttree {
            Some(c) => c,
            None => return false,
        },
        None => left,
    };
    fn cheap(x: Node<'_>) -> bool {
        match (x.as_func_expr(), x.as_op_expr()) {
            (Some(f), _) => crate::effects::builtin_func(f.funcid).is_some(),
            (_, Some(o)) => crate::effects::builtin_func(o.opfuncid).is_some(),
            _ => x.as_sub_plan().is_none() && x.as_alternative_sub_plan().is_none(),
        }
    }
    fn lists_cheap<'a>(lists: impl IntoIterator<Item = &'a NodeList<'a>>) -> bool {
        lists.into_iter().flat_map(|l| l.iter()).all(|e| crate::expr::walk(e, &mut cheap))
    }
    fn go(n: Node<'_>, scanned: &mut f64) -> bool {
        crate::walk::stack_check(); // D3748
        let Some(p) = n.as_plan() else { return false };
        if n.as_seq_scan().is_some() {
            *scanned = scanned.max(p.plan_rows);
        }
        let (join, extra): (Option<&types_nodes::plannodes::Join<'_>>, Option<&NodeList<'_>>) = if let Some(h) = n.as_hash_join() {
            (Some(&h.join), Some(&h.hashclauses))
        } else if n.as_hash().is_some() || n.as_seq_scan().is_some() {
            (None, None)
        } else {
            return false;
        };
        if !lists_cheap([&p.targetlist, &p.qual].into_iter().chain(join.map(|j| &j.joinqual)).chain(extra)) {
            return false;
        }
        [p.lefttree, p.righttree].into_iter().flatten().all(|c| go(c, scanned))
    }
    // D3054: a join that keeps under 1/16 of its widest scan's rows spools cheaper than it rewalks, at either width
    // (each walk re-reads the whole scan for few rows; the run is read once per set)
    let mut scanned = 0.0;
    left.as_hash_join().is_some() && go(left, &mut scanned) && left.as_plan().is_some_and(|p| p.plan_rows * 16.0 >= scanned)
}

/// Whether a plan node above `target` (through left / right children, Append members and SubqueryScan subplans, from the
/// statement's tree and each sub-plan) reads its whole input before it returns a row: a Sort, or a plain or hashed Agg.
/// D3540: in a statement that acts (`acts`), a ModifyTable reads its input to its end too (ExecModifyTable; a writing CTE
/// runs to completion), and a Limit stops reading its child whatever reads the Limit: no plan-order run re-checks a
/// group folded past the Limit's stop there.
fn drained_above<'p>(b: &Builder<'p, '_>, target: &'p Plan<'p>, acts: bool) -> bool {
    fn go<'p>(n: Node<'p>, target: &'p Plan<'p>, drained: bool, acts: bool) -> Option<bool> {
        crate::walk::stack_check(); // D3748
        let p = n.as_plan()?;
        if std::ptr::eq(p, target) {
            return Some(drained);
        }
        let drains = drained || n.as_sort().is_some() || n.as_agg().is_some_and(|g| matches!(g.aggstrategy, AGG_PLAIN | AGG_HASHED)) || acts && n.as_modify_table().is_some();
        let drains = drains && !(acts && n.as_limit().is_some());
        let members = n.as_append().map(|a| &a.appendplans).or_else(|| n.as_merge_append().map(|m| &m.mergeplans));
        let sub = n.as_subquery_scan().and_then(|s| s.subplan);
        [p.lefttree, p.righttree, sub].into_iter().flatten().chain(members.into_iter().flat_map(|l| l.iter())).find_map(|k| go(k, target, drains, acts))
    }
    b.stmt.planTree.into_iter().chain(b.stmt.subplans.iter().flatten()).find_map(|r| go(r, target, false, acts)).unwrap_or(false)
}

/// D1644: whether a walk reads a TABLESAMPLE that PostgreSQL samples once and each walk here samples again: one without
/// REPEATABLE (nodeSamplescan draws one seed per PlanState) or of an extension method (system_rows / system_time: a
/// handler the facts cannot name). The reach is F5's: the plan tree, the InitPlans and SubPlans its expressions call,
/// the CTEs its CteScans read. A REPEATABLE BERNOULLI / SYSTEM sample, LockRows, a transition table and a WorkTableScan
/// read the same rows on every walk: the per-set walk. `seeded` (D1920 / D1644): a BERNOULLI / SYSTEM sample over a heap
/// table reads the same rows on every walk of one execution too, REPEATABLE or not: its plan node draws one seed there
/// (hosts `HostRegistry::sample_seed`), as PostgreSQL's one PlanState does.
pub(super) struct Sampled<'p, 'f> {
    stmt: &'p types_nodes::plannodes::PlannedStmt<'p>,
    seen: ArenaVec<i32>,
    hit: bool,
    seeded: Option<&'f dyn crate::catalog::Facts>,
}

impl<'p, 'f> Sampled<'p, 'f> {
    pub(super) fn plan_reads(b: &Builder<'p, '_>, n: Node<'p>) -> bool {
        let mut s = Sampled { stmt: b.stmt, seen: ArenaVec::new_in(b.arena), hit: false, seeded: None };
        s.plan(n);
        s.hit
    }

    pub(super) fn expr_reads(b: &Builder<'p, '_>, e: Node<'p>) -> bool {
        let mut s = Sampled { stmt: b.stmt, seen: ArenaVec::new_in(b.arena), hit: false, seeded: None };
        s.expr(Some(e));
        s.hit
    }

    /// D1644: whether `e` reads a sample another walk of one execution samples again: one `expr_reads` names whose rows no
    /// one seed decides (`seeded_rows`: an extension method's, or a pgrc relation's without REPEATABLE, whose source draws
    /// its own).
    pub(super) fn expr_reads_unseeded(b: &Builder<'p, 'f>, e: Node<'p>) -> bool {
        let mut s = Sampled { stmt: b.stmt, seen: ArenaVec::new_in(b.arena), hit: false, seeded: Some(b.facts) };
        s.expr(Some(e));
        s.hit
    }

    /// A BERNOULLI / SYSTEM sample whose rows one seed decides in every walk: a REPEATABLE one's, or a heap relation's
    /// (its plan node's one seed).
    fn seeded_rows(&self, f: &dyn crate::catalog::Facts, ss: &types_nodes::plannodes::SampleScan<'_>, t: Option<&types_nodes::parsenodes::TableSampleClause<'_>>) -> bool {
        let rte = (ss.scan.scanrelid as usize).checked_sub(1).filter(|&i| i < self.stmt.rtable.len()).and_then(|i| self.stmt.rtable.nth(i).as_range_tbl_entry());
        t.is_some_and(|t| matches!(t.tsmhandler, crate::ops::shared::scan::TSM_BERNOULLI | crate::ops::shared::scan::TSM_SYSTEM))
            && (t.is_some_and(|t| t.repeatable.is_some()) || rte.and_then(|r| f.rel(r.relid)).is_some_and(|r| r.am == crate::catalog::Am::Heap))
    }

    pub(super) fn expr(&mut self, e: Option<Node<'p>>) {
        crate::walk::stack_check();
        let Some(e) = e else { return };
        if let Some(l) = e.as_list() {
            return self.list(l); // a VALUES row
        }
        expr::walk(e, &mut |x| {
            if let Some(s) = x.as_sub_plan() {
                self.tree(s.plan_id);
            }
            !self.hit
        });
    }

    pub(super) fn list(&mut self, l: &'p NodeList<'p>) {
        l.iter().for_each(|e| self.expr(Some(e)));
    }

    pub(super) fn tree(&mut self, id: i32) {
        if !self.hit && !self.seen.contains(&id) {
            self.seen.push(id);
            if let Some(r) = crate::rerun::subplan_root(self.stmt, id) {
                self.plan(r);
            }
        }
    }

    pub(super) fn plan(&mut self, n: Node<'p>) {
        crate::walk::stack_check(); // D3748
        let Some(p) = n.as_plan().filter(|_| !self.hit) else { return };
        for l in [&p.targetlist, &p.qual, &p.initPlan] {
            self.list(l);
        }
        match n.node_tag() {
            NodeTag::T_SampleScan => {
                let ss = n.as_sample_scan().unwrap();
                let t = ss.tablesample.and_then(|t| t.as_table_sample_clause());
                self.hit |= match self.seeded {
                    Some(f) => !self.seeded_rows(f, ss, t),
                    None => !t.is_some_and(|t| t.repeatable.is_some() && matches!(t.tsmhandler, crate::ops::shared::scan::TSM_BERNOULLI | crate::ops::shared::scan::TSM_SYSTEM)),
                };
                if let Some(t) = t {
                    self.list(&t.args);
                    self.expr(t.repeatable);
                }
            }
            NodeTag::T_CteScan => self.tree(n.as_cte_scan().unwrap().ctePlanId),
            NodeTag::T_Result => self.expr(n.as_result().unwrap().resconstantqual),
            NodeTag::T_IndexScan => {
                let s = n.as_index_scan().unwrap();
                self.list(&s.indexqual);
                self.list(&s.indexorderby);
            }
            NodeTag::T_IndexOnlyScan => {
                let s = n.as_index_only_scan().unwrap();
                for l in [&s.indexqual, &s.recheckqual, &s.indexorderby] {
                    self.list(l);
                }
            }
            NodeTag::T_BitmapIndexScan => self.list(&n.as_bitmap_index_scan().unwrap().indexqual),
            NodeTag::T_BitmapHeapScan => self.list(&n.as_bitmap_heap_scan().unwrap().bitmapqualorig),
            NodeTag::T_TidScan => self.list(&n.as_tid_scan().unwrap().tidquals),
            NodeTag::T_TidRangeScan => self.list(&n.as_tid_range_scan().unwrap().tidrangequals),
            NodeTag::T_FunctionScan => n.as_function_scan().unwrap().functions.iter().for_each(|f| self.expr(f.as_range_tbl_function().and_then(|r| r.funcexpr))),
            NodeTag::T_ValuesScan => self.list(&n.as_values_scan().unwrap().values_lists),
            NodeTag::T_NestLoop => {
                let j = n.as_nest_loop().unwrap();
                self.list(&j.join.joinqual);
                j.nestParams.iter().for_each(|np| self.expr(np.as_nest_loop_param().map(|np| np.paramval)));
            }
            NodeTag::T_MergeJoin => {
                let j = n.as_merge_join().unwrap();
                self.list(&j.join.joinqual);
                self.list(&j.mergeclauses);
            }
            NodeTag::T_HashJoin => {
                let j = n.as_hash_join().unwrap();
                for l in [&j.join.joinqual, &j.hashclauses, &j.hashkeys] {
                    self.list(l);
                }
            }
            NodeTag::T_Hash => self.list(&n.as_hash().unwrap().hashkeys),
            NodeTag::T_WindowAgg => {
                let w = n.as_window_agg().unwrap();
                self.expr(w.startOffset);
                self.expr(w.endOffset);
                self.list(&w.runCondition);
            }
            NodeTag::T_Limit => {
                let l = n.as_limit().unwrap();
                self.expr(l.limitCount);
                self.expr(l.limitOffset);
            }
            NodeTag::T_Memoize => self.list(&n.as_memoize().unwrap().param_exprs),
            _ => {}
        }
        let lists = [n.as_append().map(|a| &a.appendplans), n.as_merge_append().map(|a| &a.mergeplans), n.as_bitmap_and().map(|a| &a.bitmapplans), n.as_bitmap_or().map(|a| &a.bitmapplans), n.as_agg().map(|a| &a.chain)];
        let kids = p.lefttree.into_iter().chain(p.righttree).chain(n.as_subquery_scan().and_then(|s| s.subplan));
        for k in kids.chain(lists.into_iter().flatten().flat_map(|l| l.iter())) {
            self.plan(k);
        }
    }
}

pub(super) fn trivial(e: Node<'_>) -> bool {
    e.as_var().is_some() || e.as_const().is_some()
}

/// `OUTER_VAR` 1..=n over list `l`'s first n entries, typed as their expressions.
pub(super) fn outer_vars<'p>(mcx: mcx::Mcx<'p>, l: &NodeList<'p>, n: usize) -> types_error::PgResult<&'p NodeList<'p>> {
    let mut out = NodeList::with_capacity(mcx, n)?;
    for (i, te) in l.iter().take(n).filter_map(|x| x.as_target_entry()).enumerate() {
        let v = Node::mk_var(mcx, OUTER_VAR, i as i16 + 1, nodes_core::expr_type(te.expr), nodes_core::expr_typmod(te.expr), nodes_core::expr_collation(te.expr), 0)?;
        out.lappend(mcx, Node::mk_target_entry(mcx, v, i as i16 + 1, None, false)?)?;
    }
    Ok(Node::mk_list(mcx, out)?.as_list().expect("a list node"))
}

/// The Agg's input columns for its run: `OUTER_VAR` c, or an int4 NULL for a column no node of the Agg reads (its
/// keys, lists and Sorts), which a scan's physical target list projects with no lane under it (cb_scan_cols).
pub(super) fn input_cols<'p>(b: &Builder<'p, '_>, a: &'p Agg<'p>, input: &'p NodeList<'p>) -> types_error::PgResult<&'p NodeList<'p>> {
    let mcx = b.alloc.expect("checked by `Spool::of`");
    let mut used: ArenaVec<bool> = ArenaVec::new_in(b.arena);
    used.resize(input.len(), false);
    let mut mark = |c: i32| if let Some(u) = usize::try_from(c - 1).ok().and_then(|i| used.get_mut(i)) { *u = true };
    let mut known = true;
    for node in core::iter::once(a).chain(a.chain.iter().filter_map(|n| n.as_agg())) {
        node.grpColIdx.iter().for_each(|&c| mark(c as i32));
        if let Some(s) = node.plan.lefttree.and_then(|c| c.as_sort()) {
            s.sortColIdx.iter().for_each(|&c| mark(c as i32));
        }
        for e in node.plan.targetlist.iter().chain(node.plan.qual.iter()) {
            known &= expr::walk(e, &mut |x| {
                if let Some(v) = x.as_var().filter(|v| v.varno == OUTER_VAR) {
                    mark(v.varattno as i32);
                }
                true
            });
        }
    }
    let mut out = NodeList::with_capacity(mcx, input.len())?;
    for (i, te) in input.iter().filter_map(|x| x.as_target_entry()).enumerate() {
        let e = match known && !used[i] {
            true => Node::mk_const(mcx, types_core::INT4OID, -1, 0, 4, datum::Datum::from_i64(0), true, true)?,
            false => Node::mk_var(mcx, OUTER_VAR, i as i16 + 1, nodes_core::expr_type(te.expr), nodes_core::expr_typmod(te.expr), nodes_core::expr_collation(te.expr), 0)?,
        };
        out.lappend(mcx, Node::mk_target_entry(mcx, e, i as i16 + 1, None, false)?)?;
    }
    Ok(Node::mk_list(mcx, out)?.as_list().expect("a list node"))
}

/// `CASE WHEN f THEN e END`, typed as `e`.
pub(super) fn case_when<'p>(b: &Builder<'p, '_>, f: Node<'p>, e: Node<'p>) -> types_error::PgResult<Node<'p>> {
    use types_nodes::primnodes::{CaseExpr, CaseWhen};
    let mcx = b.alloc.expect("checked by `Spool::of`");
    let (typ, typmod, coll) = (nodes_core::expr_type(e), nodes_core::expr_typmod(e), nodes_core::expr_collation(e));
    let (len, byval) = b.facts.type_row(typ).map_or((-1, false), |t| (t.typlen as i32, t.byval));
    let null = Node::mk_const(mcx, typ, typmod, coll, len, datum::Datum::from_i64(0), true, byval)?;
    let mut args = NodeList::with_capacity(mcx, 1)?;
    args.lappend(mcx, Node::mk(mcx, CaseWhen { expr: Some(f), result: Some(e), location: -1 })?)?;
    Node::mk(mcx, CaseExpr { casetype: typ, casecollid: coll, arg: None, args, defresult: Some(null), location: -1 })
}

/// D467 (V4R-159): nodeAgg's order of a sorted node no Sort re-orders, as resjunk keys every arm's row carries — the
/// phase (the node's rollup, then each chained one, when there are several), then per key of each phase's Sort the
/// column's rolled flag (a set that groups it first) and its value under that Sort's operator. Within a phase each group
/// comes before the prefix sets it closes, the empty set last. `sorted` pushes the full sort and drops the keys.
pub(super) struct RollupOrder<'p> {
    phases: ArenaVec<&'p types_nodes::plannodes::Sort<'p>>,
    /// The Agg's input columns (the phase-0 Sort's target list): each value key's type.
    input: &'p NodeList<'p>,
    ncols: usize,
    /// D2001: per column of the node's target list, `Some(e)` where the read-back above the full sort evaluates the
    /// column (`e`: its expression over the sorted row's leaf columns) instead of the arms: an expression that may raise
    /// (rerun.rs `raise_free`), so only the rows the statement reads run it, as nodeAgg projects each row it hands on (a
    /// LIMIT 1 over a sorted ROLLUP projects one row; the arms below the sort would project every group of every set).
    /// Its leaves (`leaf`: a read of an input column, an aggregate, a GROUPING()) ride each arm as resjunk columns after
    /// the order's keys, folded per set as the arm's own list is (`arm_lists`).
    deferred: ArenaVec<Option<Node<'p>>>,
    leaves: ArenaVec<Node<'p>>,
}

/// D2001: a node a deferred column reads as one of the arm's columns (`RollupOrder::deferred`).
fn leaf(x: Node<'_>) -> bool {
    x.as_aggref().is_some() || x.as_grouping_func().is_some() || x.as_var().is_some_and(|v| v.varno == OUTER_VAR)
}

/// D2001: `x` with each of its `leaf` nodes (found by identity in `leaves`, at `base` + its index) read as that sorted
/// row column; `miss` when a leaf is not among them.
fn leaf_cols<'p>(mcx: mcx::Mcx<'p>, x: Node<'p>, leaves: &[Node<'p>], base: i16, hit: &mut usize, miss: &mut bool) -> types_error::PgResult<Option<Node<'p>>> {
    crate::walk::stack_check(); // D3748
    if leaf(x) {
        let Some(i) = leaves.iter().position(|l| l.ptr_eq(x)) else {
            *miss = true;
            return Ok(None);
        };
        *hit += 1;
        let v = Node::mk_var(mcx, OUTER_VAR, base + i as i16, nodes_core::expr_type(x), nodes_core::expr_typmod(x), nodes_core::expr_collation(x), 0)?;
        return Ok(Some(v));
    }
    nodes_core::expression_tree_mutator_dyn(mcx, x, &mut |y| leaf_cols(mcx, y, leaves, base, &mut *hit, &mut *miss))
}

/// int4lt: the phase and rolled keys' operator.
pub(super) const INT4_LT: u32 = 97;

impl<'p> RollupOrder<'p> {
    pub(super) fn of(b: &Builder<'p, '_>, a: &'p Agg<'p>, k: Kind, id: i32) -> Result<Option<RollupOrder<'p>>, Refusal> {
        let mut phases = ArenaVec::new_in(b.arena);
        for node in core::iter::once(a).chain(a.chain.iter().filter_map(|n| n.as_agg())) {
            let Some(s) = node.plan.lefttree.and_then(|c| c.as_sort()) else { return Ok(None) };
            if s.sortColIdx.len() < s.numCols.max(0) as usize || s.sortOperators.len() < s.sortColIdx.len() || s.nullsFirst.len() < s.sortColIdx.len() {
                return Err(Refusal::shape(k, id, "a rollup phase's Sort without keys"));
            }
            phases.push(s);
        }
        let mut o = RollupOrder { input: &phases[0].plan.targetlist, phases, ncols: a.plan.targetlist.len(), deferred: ArenaVec::new_in(b.arena), leaves: ArenaVec::new_in(b.arena) };
        o.defer(b, a, k, id)?;
        Ok(Some(o))
    }

    /// D2001: fill `deferred` / `leaves` (see there). A column stays the arms' where it is a leaf or a constant, calls a
    /// SubPlan (the arm attaches its SubPlans), raises nothing, or holds a leaf the read-back could not map.
    fn defer(&mut self, b: &Builder<'p, '_>, a: &'p Agg<'p>, k: Kind, id: i32) -> Result<(), Refusal> {
        let mcx = b.alloc.ok_or(Refusal::shape(k, id, "grouping sets: no node allocator (decompose_in)"))?;
        let oom = |_| Refusal::shape(k, id, "grouping sets: out of memory deferring a raising column");
        let keys = usize::from(self.phases.len() > 1) + 2 * self.keyed().count();
        let base = (self.ncols + keys + 1) as i16;
        for n in a.plan.targetlist.iter() {
            let e = n.as_target_entry().map(|te| te.expr);
            let quiet = |e: Node<'p>| leaf(e) || e.as_const().is_some() || expr::has_subplan(e) || crate::rerun::of_expr(b.stmt, b.facts, b.arena, e).raise_free;
            let Some(e) = e.filter(|&e| !quiet(e)) else {
                self.deferred.push(None);
                continue;
            };
            let start = self.leaves.len();
            let leaves = &mut self.leaves;
            expr::walk(e, &mut |x| {
                if leaf(x) {
                    leaves.push(x);
                    return false;
                }
                true
            });
            let (mut hit, mut miss) = (0usize, false);
            let back = leaf_cols(mcx, e, &self.leaves[start..], base + start as i16, &mut hit, &mut miss).map_err(oom)?;
            match back {
                Some(back) if !miss && hit == self.leaves.len() - start && i16::try_from(self.leaves.len()).is_ok() => self.deferred.push(Some(back)),
                _ => {
                    self.leaves.truncate(start);
                    self.deferred.push(None);
                }
            }
        }
        Ok(())
    }

    pub(super) fn keyed(&self) -> impl Iterator<Item = (usize, usize)> + use<'_, 'p> {
        self.phases.iter().enumerate().flat_map(|(p, s)| (0..s.numCols.max(0) as usize).map(move |j| (p, j)))
    }

    /// Arm `phase`'s target list: the node's, then the phase / rolled / value keys (`arm_lists` nulls a value its set
    /// does not group).
    pub(super) fn arm_tlist(&self, b: &Builder<'p, '_>, a: &'p Agg<'p>, phase: u16, grouped: &[i16], k: Kind, id: i32) -> Result<&'p NodeList<'p>, Refusal> {
        let mcx = b.alloc.ok_or(Refusal::shape(k, id, "grouping sets: no node allocator (decompose_in)"))?;
        let oom = |_| Refusal::shape(k, id, "grouping sets: out of memory building the rollup order");
        // a phase id / rolled flag: fixed by the grouping sets and chain, which the front key holds (`Derived::Fixed`)
        let int4 = |v: i32| Node::mk_const(mcx, types_core::INT4OID, -1, 0, 4, datum::Datum::from_i32(v), false, true).inspect(|n| crate::bake::derived_note(n.as_raw().as_ptr() as usize, crate::bake::Derived::Fixed));
        let mut l = a.plan.targetlist.clone_in(mcx).map_err(oom)?;
        // D2001: a deferred column is a NULL of its type here (the read-back evaluates it), its leaves after the keys
        for (i, d) in self.deferred.iter().enumerate() {
            let Some(te) = (*d).and(l.as_slice().get(i).and_then(|n| n.as_target_entry())) else { continue };
            let (typ, typmod, coll) = (nodes_core::expr_type(te.expr), nodes_core::expr_typmod(te.expr), nodes_core::expr_collation(te.expr));
            let (len, byval) = b.facts.type_row(typ).map_or((-1, false), |t| (t.typlen as i32, t.byval));
            let null = Node::mk_const(mcx, typ, typmod, coll, len, datum::Datum::from_i64(0), true, byval).map_err(oom)?;
            l.as_mut_slice()[i] = Node::mk_target_entry(mcx, null, te.resno, te.resname, te.resjunk).map_err(oom)?;
        }
        let mut push = |l: &mut NodeList<'p>, e: Node<'p>| -> Result<(), Refusal> {
            let te = Node::mk_target_entry(mcx, e, l.len() as i16 + 1, None, true).map_err(oom)?;
            l.lappend(mcx, te).map_err(oom)
        };
        if self.phases.len() > 1 {
            push(&mut l, int4(phase as i32).map_err(oom)?)?;
        }
        for (p, j) in self.keyed() {
            let c = self.phases[p].sortColIdx[j];
            push(&mut l, int4(i32::from(p != phase as usize || !grouped.contains(&c))).map_err(oom)?)?;
            let e = self.input.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == c).ok_or(Refusal::shape(k, id, "a rollup key outside the input's target list"))?.expr;
            let v = Node::mk_var(mcx, OUTER_VAR, c, nodes_core::expr_type(e), nodes_core::expr_typmod(e), nodes_core::expr_collation(e), 0).map_err(oom)?;
            push(&mut l, v)?;
        }
        for &x in self.leaves.iter() {
            push(&mut l, x)?;
        }
        Ok(Node::mk_list(mcx, l).map_err(oom)?.as_list().expect("a list node"))
    }

    /// `out` (the union of the arms) into a full sort over the order's keys, read back without them.
    pub(super) fn sorted(self, b: &mut Builder<'p, '_>, mut out: Open<'p>, a: &'p Agg<'p>, k: Kind, id: i32) -> Result<Open<'p>, Refusal> {
        let mcx = b.alloc.ok_or(Refusal::shape(k, id, "grouping sets: no node allocator (decompose_in)"))?;
        let oom = |_| Refusal::shape(k, id, "grouping sets: out of memory building the rollup order");
        let int4 = |resno: i16| crate::ops::shared::sort::SortKey::of_type(b.facts, resno, INT4_LT, false, false, types_core::INT4OID, b.facts.coll(0));
        let mut keys = ArenaVec::new_in(b.arena);
        let mut at = self.ncols as i16 + 1;
        if self.phases.len() > 1 {
            keys.push(int4(at));
            at += 1;
        }
        for (p, j) in self.keyed() {
            let s = self.phases[p];
            let (c, opno, coll) = (s.sortColIdx[j], s.sortOperators[j], s.collations.get(j).copied().unwrap_or(0));
            let desc = b.facts.key(crate::catalog::KeyReq::sort(opno)).and_then(crate::catalog::KeyOrder::dir).ok_or(Refusal::shape(k, id, "a sort operator outside the direction table"))?;
            keys.push(int4(at));
            let t = crate::ops::shared::sort::column_type(self.input, c, k, id)?;
            keys.push(crate::ops::shared::sort::SortKey::of_type(b.facts, at + 1, opno, desc, s.nullsFirst[j], t, crate::ops::shared::sort::key_coll(b.facts, t, coll, k, id)?));
            at += 2;
        }
        // the sorted row's columns as a Sort's target list (`OUTER_VAR` entries: a key the arms fold to a constant still
        // orders, runs/sort.rs drops only a Sort's own Const keys), and the node's columns read back from it
        let arm0 = self.arm_tlist(b, a, 0, &[], k, id)?;
        let outer = |n: usize| -> Result<&'p NodeList<'p>, Refusal> {
            let mut l = NodeList::with_capacity(mcx, n).map_err(oom)?;
            for (i, te) in arm0.iter().take(n).filter_map(|x| x.as_target_entry()).enumerate() {
                let v = Node::mk_var(mcx, OUTER_VAR, i as i16 + 1, nodes_core::expr_type(te.expr), nodes_core::expr_typmod(te.expr), nodes_core::expr_collation(te.expr), 0).map_err(oom)?;
                l.lappend(mcx, Node::mk_target_entry(mcx, v, i as i16 + 1, te.resname, te.resjunk).map_err(oom)?).map_err(oom)?;
            }
            Ok(Node::mk_list(mcx, l).map_err(oom)?.as_list().expect("a list node"))
        };
        let (all, mut back) = (outer(arm0.len())?, outer(self.ncols)?);
        // D2001: a deferred column's expression over the sorted row's leaf columns, evaluated per row read
        if self.deferred.iter().any(Option::is_some) {
            let mut l = back.clone_in(mcx).map_err(oom)?;
            for (i, d) in self.deferred.iter().enumerate() {
                let (Some(e), Some(te)) = (*d, l.as_slice().get(i).and_then(|n| n.as_target_entry())) else { continue };
                l.as_mut_slice()[i] = Node::mk_target_entry(mcx, e, te.resno, te.resname, te.resjunk).map_err(oom)?;
            }
            back = Node::mk_list(mcx, l).map_err(oom)?.as_list().expect("a list node");
        }
        let layout = crate::ops::shared::sort::keys_layout(b.arena, keys.iter().map(|x| x.kind));
        out.push(Op::Sort(crate::ops::shared::sort::SortDesc { keys, presorted: 0, form: crate::ops::shared::sort::SortForm::Runs, tlist: all }));
        let slot = crate::ops::shared::breaker::close(b, out, BreakerKind::Sort, layout, Budget::Share, Spill::Runs, k, id)?;
        let mut e = crate::ops::shared::breaker::emit(b, id, slot);
        crate::ops::project(&mut e, back);
        Ok(e)
    }
}

/// Whether a Sort or Incremental Sort lies above node `id` on its outer chain from the plan root (a Limit's, a Result's,
/// a SubqueryScan's subplan): the statement then orders what the sorted node emits again. Not found (a join's inner, an
/// Append's child, a SubPlan): `false`, the sorted node's order may be what its consumer reads.
/// Whether PostgreSQL reads node `id`'s whole output before the statement can stop: a Sort above it on the left spine (it
/// reads every input row at its first fetch), no LIMIT between them, and above it only a LIMIT, a Result without a
/// one-time qual or a subquery scan; a plain or hashed Agg above counts as a Sort. A LIMIT above the Sort whose count may
/// be 0 (a constant 0, a parameter) runs none of it then: lowering's step.limit feeder stops the pipeline before its first
/// row, or refuses (D2115: result/gate.rs `SPOOL_ZERO`); any other count reads it whole. Anything else answers no.
pub(super) fn read_whole(root: Option<Node<'_>>, id: i32) -> bool {
    let (mut n, mut sorted) = (root, false);
    while let Some(node) = n {
        let Some(plan) = node.as_plan() else { return false };
        if plan.plan_node_id == id {
            return sorted;
        }
        let pass = match node.node_tag() {
            NodeTag::T_Sort => {
                sorted = true;
                true
            }
            NodeTag::T_Limit => !sorted,
            NodeTag::T_Result => node.as_result().is_some_and(|r| r.resconstantqual.is_none()),
            NodeTag::T_SubqueryScan => true,
            // a plain or hashed aggregate reads its whole input before its first row, as a Sort does
            NodeTag::T_Agg if node.as_agg().is_some_and(|g| matches!(g.aggstrategy, AGG_PLAIN | AGG_HASHED)) => {
                sorted = true;
                true
            }
            _ => false,
        };
        if !pass {
            return false;
        }
        n = node.as_subquery_scan().map_or(plan.lefttree, |sq| sq.subplan);
    }
    false
}

pub(super) fn resorted(root: Option<Node<'_>>, id: i32) -> bool {
    let mut n = root;
    while let Some(node) = n {
        if node.as_sort().is_some() || node.as_incremental_sort().is_some() {
            return true;
        }
        let Some(plan) = node.as_plan() else { return false };
        if plan.plan_node_id == id {
            return false;
        }
        n = node.as_subquery_scan().map_or(plan.lefttree, |sq| sq.subplan);
    }
    false
}

/// Set `grouped`'s target list and qual: the node's, with nodeAgg's projection of that set folded in as constants,
/// outside the aggregates' arguments (an aggregate reads its input rows, never the projected slot):
/// - an `OUTER_VAR` column of `all` (all_grouped_cols) that `grouped` does not group reads NULL (prepare_projection_slot),
///   typed, typmod'd and collated as the column;
/// - `GROUPING(..)` is its mask over its `cols`, a bit set where the set does not group the column (EEOP_GROUPING_FUNC).
/// A `GROUPING` inside an aggregate's arguments refuses by name. Unchanged lists are the node's own (the mutator is
/// copy-on-write); `fresh` (every arm after the first) copies the literals too, so no two arms share a literal node.
pub(super) fn arm_lists<'p>(b: &Builder<'p, '_>, a: &'p Agg<'p>, tl: &'p NodeList<'p>, grouped: &[i16], all: &[i16], fresh: bool, k: Kind, id: i32) -> Result<(&'p NodeList<'p>, &'p NodeList<'p>), Refusal> {
    let lists = [tl, &a.plan.qual];
    check_grouping(&lists, k, id)?;
    let mcx = b.alloc.ok_or(Refusal::shape(k, id, "grouping sets: no node allocator (decompose_in)"))?;
    let oom = |_| Refusal::shape(k, id, "grouping sets: out of memory folding a set's projection");
    let facts = b.facts;
    let mut m = |x: Node<'p>| arm_node(mcx, facts, x, grouped, all, fresh);
    let mut out = [tl, &a.plan.qual];
    for (i, l) in lists.iter().enumerate() {
        if let Some(new) = nodes_core::mutate_list_dyn(mcx, l, &mut m).map_err(oom)? {
            out[i] = Node::mk_list(mcx, new).map_err(oom)?.as_list().expect("a list node");
        }
    }
    Ok((out[0], out[1]))
}

/// The `GROUPING()` forms no set's projection folds: one inside an aggregate's arguments, one of an outer query level.
pub(super) fn check_grouping(lists: &[&NodeList<'_>], k: Kind, id: i32) -> Result<(), Refusal> {
    let mut bad = None;
    for l in lists {
        for e in l.iter() {
            expr::walk(e, &mut |x| {
                if let Some(ag) = x.as_aggref() {
                    let mut inner = false;
                    for arg in ag.args.iter().chain(ag.aggdirectargs.iter()).chain(ag.aggfilter) {
                        expr::walk(arg, &mut |y| {
                            inner |= y.node_tag() == NodeTag::T_GroupingFunc;
                            true
                        });
                    }
                    if inner {
                        bad = Some("a GROUPING() inside an aggregate's arguments");
                    }
                    return false;
                }
                if let Some(g) = x.as_grouping_func() {
                    if g.agglevelsup != 0 {
                        bad = Some("a GROUPING() of an outer query level");
                    }
                }
                true
            });
        }
    }
    match bad {
        Some(why) => Err(Refusal::shape(k, id, why)),
        None => Ok(()),
    }
}

/// Whether `n` holds a `GROUPING()` outside a sub-plan's own tree (D461).
pub(super) fn has_grouping_func(n: Node<'_>) -> bool {
    let mut hit = false;
    expr::walk(n, &mut |x| {
        hit |= x.node_tag() == NodeTag::T_GroupingFunc;
        !hit
    });
    hit
}

/// `arm_lists`' mutator: `Some` where `x` changes (copy-on-write), `None` where it stays the node's.
pub(super) fn arm_node<'p>(mcx: mcx::Mcx<'p>, facts: &dyn crate::catalog::Facts, x: Node<'p>, grouped: &[i16], all: &[i16], fresh: bool) -> types_error::PgResult<Option<Node<'p>>> {
    crate::walk::stack_check(); // D3748
    if let Some(ag) = x.as_aggref() {
        // D1520: finalize_aggregate evaluates the direct arguments over the set's projected slot, so a column the set
        // nulls reads NULL there too (its aggregated arguments, ORDER BY and FILTER read input rows: untouched)
        let Some(direct) = nodes_core::mutate_list_dyn(mcx, &ag.aggdirectargs, &mut |y| arm_node(mcx, facts, y, grouped, all, false))? else { return Ok(None) };
        let new = Aggref { aggargtypes: ag.aggargtypes.clone_in(mcx)?, aggdirectargs: direct, args: ag.args.clone_in(mcx)?, aggorder: ag.aggorder.clone_in(mcx)?, aggdistinct: ag.aggdistinct.clone_in(mcx)?, ..*ag };
        return Node::mk(mcx, new).map(Some);
    }
    if fresh && x.as_const().is_some() {
        // a later arm's literals are its own nodes: a literal's claimed word is per occurrence (lowerv4 Px::claim_const,
        // the HAVING floor's), and each arm's filter compiles its own occurrence
        // a copy of a constant decompose built (an earlier arm's phase / rolled constant) is built as that one was
        let how = crate::bake::derived(x).unwrap_or(crate::bake::Derived::Copy { src: x.as_raw().as_ptr() as usize });
        return copyfuncs::copy_object(mcx, x).inspect(|n| crate::bake::derived_note(n.as_raw().as_ptr() as usize, how)).map(Some);
    }
    if let Some(v) = x.as_var() {
        if v.varno != OUTER_VAR || !all.contains(&v.varattno) || grouped.contains(&v.varattno) {
            return Ok(None);
        }
        let (len, byval) = facts.type_row(v.vartype).map_or((-1, false), |t| (t.typlen as i32, t.byval));
        return Node::mk_const(mcx, v.vartype, v.vartypmod, v.varcollid, len, datum::Datum::from_i64(0), true, byval).map(Some);
    }
    if let Some(g) = x.as_grouping_func() {
        let mut mask: i64 = 0;
        for c in g.cols.iter() {
            mask = (mask << 1) | i64::from(!grouped.contains(&(c as i16)));
        }
        // fixed by the GroupingFunc's columns and the arm's set, which the front key holds (`Derived::Fixed`)
        return Node::mk_const(mcx, types_core::INT4OID, -1, 0, 4, datum::Datum::from_i32(mask as i32), false, true).inspect(|n| crate::bake::derived_note(n.as_raw().as_ptr() as usize, crate::bake::Derived::Fixed)).map(Some);
    }
    nodes_core::expression_tree_mutator_dyn(mcx, x, &mut |y| arm_node(mcx, facts, y, grouped, all, fresh))
}
