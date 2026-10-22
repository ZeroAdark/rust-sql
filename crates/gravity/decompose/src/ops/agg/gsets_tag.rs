//! Grouping sets, form T (lane gsetsagg, D3029): the Agg node with a grouping-sets chain as ONE keyed group over the
//! input walked once, where `map_sets`' arm form walks it once per set.
//!
//! nodeAgg reads its child once and advances every hashed set's table from each row (DuckDB's grouping hash aggregate
//! does the same with a table per set). Form T keeps one breaker (one grant, admission, spill and parallel finish) and
//! keys it by the set: each input row, after the aggregates' inputs are computed into columns (`Spool::steps_of`, once
//! per row), is fanned out N times by a ProjectSet of `generate_series(0, N - 1)` (the set's tag), and the group's keys
//! are the tag and every grouped column of any set, a column read NULL where the tag's set does not group it (nodeAgg's
//! `prepare_projection_slot`). Two sets never share a group (their tags differ), so each set's groups are its own.
//! The emitted row reads the keys: a grouped column is its key (NULL where the set nulls it), `GROUPING()` and a rollup
//! order's phase / rolled constants differ by set and read `CASE WHEN tag = i THEN .. END` over the tag key.
//!
//! Declined (the arm form serves, `fires` names why) where a set's semantics would not survive the shared key: see
//! `decline`.
use super::*;
use types_nodes::primnodes::{CaseExpr, CaseWhen, FuncExpr, OpExpr};

/// The empty set `()` named more than once (each needs its own sentinel row, `sentinel`), or with an aggregate calling a
/// SubPlan (the sentinel's union arm would carry it).
pub(super) const EMPTY_SET: &str = "gsets.tag_no_empty_set";
/// A column two chain nodes group under a different collation or equality operator: one key cannot compare both ways.
pub(super) const KEY_SEMANTICS: &str = "gsets.tag_no_key_semantics";
/// A VOLATILE aggregate input or a re-sampled TABLESAMPLE under more than one sorted phase: nodeAgg evaluates the inputs
/// once per row per phase (D1061), form T once per row.
pub(super) const PHASED_INPUTS: &str = "gsets.tag_no_phased_inputs";
/// An ordered-set aggregate's direct argument reads a grouped column (D1520: evaluated over the set's projected slot).
pub(super) const DIRECT_ARGS: &str = "gsets.tag_no_direct_args";
/// A target entry or HAVING conjunct that differs by set calls a SubPlan (its plan would be placed once per set's branch).
pub(super) const SUBPLAN_BY_SET: &str = "gsets.tag_no_subplan_by_set";
/// A plain scan of built-in expressions under no VOLATILE call or re-sampled TABLESAMPLE (`gsets::rewalk_cheap`, the spool's
/// own test): walking it once per set into per-set groups beats one tagged group. Measured (box 39, 20M-row heap, W0 / W8):
/// CUBE (a, b, c) per-set walks 7.6 s / 0.73 s, form T 30 s / 3 s: the tagged keys lose each set's dense small-int table
/// and its narrow record. Form T replaces the spool (a join, a subquery, any input costlier than a scan).
pub(super) const CHEAP_REWALK: &str = "gsets.tag_no_cheap_rewalk";
/// An input that is a union (Append / MergeAppend, through a subquery scan or a projecting Result): the arm form's spool
/// reads it as one run, where form T would stream the fan-out into every arm (R-KEEP c-app dsx-0065: a ROLLUP
/// over a UNION ALL of left joins refused D2129 in lowerv4 under T, served by the spool).
pub(super) const UNION_INPUT: &str = "gsets.tag_no_union_input";
/// A serial session (D3047) whose tagged table would leave the cache: its estimated bytes (every set's planner groups
/// times the record) exceed the machine's L2, so each row's probes miss into DRAM where the spool's per-set tables, read
/// one set at a time, stay cached (box 39, a 20M-row join at W0, T / spool: CUBE (a, b, c) ~1.1M groups 16.6 / 8.3 s,
/// ROLLUP (u) ~2M groups 1.85 / 1.68 s; under L2, T wins: GROUPING SETS ((a), (b)) 0.92 / 2.0 s, ROLLUP over a join
/// 2.0 / 4.4 s). No constant: the bound is the machine profile's L2.
pub(super) const TABLE_LOCALITY: &str = "gsets.tag_no_table_locality";
/// A join under a VOLATILE input: the spool walks it once in its own fill, where T's walk carries the join's build into
/// the keyed group's pipeline, which lowerv4 refuses for a volatile build PG reads outer first (agg_gsets_sample_join:
/// random() on a sampled build side, "a volatile build side on PG's outer-first path that is not deferred").
pub(super) const VOLATILE_JOIN: &str = "gsets.tag_no_volatile_join";
/// No node allocator (decompose_in) or no input.
pub(super) const NO_ALLOC: &str = "gsets.tag_no_alloc";

/// D4620 (road `agg.gsets.cascade`): form T folds each row once, under the sets no requested set is a strict superset of
/// (and the empty set, its sentinel kept); every other set is folded from a finer set's owned records (`Cascade`).
pub(super) const CASCADE: &str = "gsets.cascade";
/// The cascade declines: an aggregate whose partial states cannot be merged as the owners merge them (DISTINCT, ORDER BY,
/// an ordered-set or hypothetical call, a user or collecting one, a merge that depends on the order the records come in,
/// a hosted state, a SubPlan argument), a VOLATILE aggregate input, a node that runs late (a rerun set's member), no set
/// with a superset or more than `CASCADE_MAX` of them, a layout the record merge cannot take (a hosted state, an
/// order-keyed numeric extreme), or a key the sets do not declare (a functionally dependent column).
pub(super) const CASCADE_NO_AGG: &str = "gsets.cascade_no_agg";
pub(super) const CASCADE_NO_INPUT: &str = "gsets.cascade_no_input";
pub(super) const CASCADE_NO_LATE: &str = "gsets.cascade_no_late";
pub(super) const CASCADE_NO_SETS: &str = "gsets.cascade_no_sets";
pub(super) const CASCADE_NO_LAYOUT: &str = "gsets.cascade_no_layout";
/// `CASCADE_NO_LAYOUT`'s cause: a grouped DISTINCT's form, a key the sets do not declare, a hosted state (no record slot
/// of a frozen kind), an order-keyed numeric extreme or a final the owners' merge does not take.
const CASCADE_NO_LAYOUT_FORM: &str = "gsets.cascade_no_layout.form";
const CASCADE_NO_LAYOUT_KEYS: &str = "gsets.cascade_no_layout.keys";
const CASCADE_NO_LAYOUT_HOSTED: &str = "gsets.cascade_no_layout.hosted";
const CASCADE_NO_LAYOUT_FOLD: &str = "gsets.cascade_no_layout.fold";
/// The most sets one node folds from records (gravity_abi `limits::CASCADE_SETS_MAX`: the emit reads them all).
const CASCADE_MAX: usize = gravity_abi::limits::CASCADE_SETS_MAX;

/// int4eq / its operator: the tag test.
const INT4EQ_OP: u32 = 96;
const INT4EQ_FN: u32 = 65;

/// D4620: form T's sets ordered for the cascade (`perm[i]`: the arm index of tag `i`): the first `m` folded from the input,
/// each later one (`parents[i - m]`) from a finer set's records, its candidates' tags (strict supersets) finest first.
pub(super) struct CascadePlan {
    perm: ArenaVec<usize>,
    m: usize,
    parents: ArenaVec<ArenaVec<usize>>,
}

/// Whether set `i`'s columns are a strict subset of set `j`'s.
fn strict_sub(s: &Sets<'_, '_>, i: usize, j: usize) -> bool {
    s.grouped[i].len() < s.grouped[j].len() && s.grouped[i].iter().all(|c| s.grouped[j].contains(c))
}

/// Whether aggregate `r` merges as the owners merge a group's partial records: a built-in call (no DISTINCT, ORDER BY,
/// direct argument or SubPlan) whose class has a combine function and folds alike in any order (`facts::order::order_free`,
/// the check the parallel fold reads: a float sum's partial order is the parallel fold's already).
fn cascade_agg(r: Node<'_>) -> bool {
    let Some(ag) = r.as_aggref() else { return false };
    let Some(row) = super::trans::table::row(ag.aggfnoid) else { return false };
    let mods = Mods { distinct: ag.aggdistinct.len() as u8, order: ag.aggorder.len() as u8, filter: ag.aggfilter.is_some(), direct: ag.aggdirectargs.len() as u8, split: Split::of(ag.aggsplit), star: ag.aggstar };
    row.kind == b'n' && ag.aggkind as u8 == b'n' && !row.combinefn.is_empty() && mods.distinct == 0 && mods.order == 0 && mods.direct == 0 && !expr::has_subplan(r) && !matches!(row.class, Class::User | Class::Collect(_) | Class::OrderedSet(_) | Class::Hypothetical(_)) && crate::facts::order::order_free(row.class, &mods)
}

/// D4620: the cascade's plan for the node's sets, when road `agg.gsets.cascade` is on and the node takes it (`None`: form T
/// fans every row out to every set, the declining reason fired unless `quiet`). Read before anything is declared.
pub(super) fn cascade_plan<'p>(b: &Builder<'p, '_>, a: &'p Agg<'p>, s: &Sets<'_, 'p>, quiet: bool) -> Option<CascadePlan> {
    if !b.facts.default_on_road_on(crate::ops::shared::agg::GSETS_CASCADE_ROAD) || b.alloc.is_none() {
        return None;
    }
    let decline = |why: &'static str| {
        if !quiet {
            fires::fire(why);
        }
        None
    };
    let mut aggs = true;
    for e in a.plan.targetlist.iter().chain(a.plan.qual.iter()) {
        expr::run_aggrefs(e, &mut |r| aggs &= cascade_agg(r));
    }
    if !aggs {
        return decline(CASCADE_NO_AGG);
    }
    let cx = crate::effects::Cx::new(b.stmt, b.facts, b.arena);
    if crate::effects::agg::volatile_inputs(&cx, &[&a.plan.targetlist, &a.plan.qual]) {
        return decline(CASCADE_NO_INPUT);
    }
    if super::gsets::late_run(b, a) {
        return decline(CASCADE_NO_LATE);
    }
    let n = s.arms.len();
    // a set with a strict superset among the requested ones folds from records; the empty set keeps its sentinel
    let folded = |i: usize| !s.grouped[i].is_empty() && (0..n).any(|j| strict_sub(s, i, j));
    let cascaded = (0..n).filter(|&i| folded(i)).count();
    if cascaded == 0 || cascaded > CASCADE_MAX {
        return decline(CASCADE_NO_SETS);
    }
    let mut perm: ArenaVec<usize> = ArenaVec::with_capacity_in(n, b.arena);
    for i in (0..n).filter(|&i| !folded(i)) {
        perm.push(i);
    }
    let m = perm.len();
    // the folded sets widest first: every strict superset of a set comes before it
    let mut rest: ArenaVec<usize> = ArenaVec::with_capacity_in(cascaded, b.arena);
    for i in (0..n).filter(|&i| folded(i)) {
        rest.push(i);
    }
    // the index breaks ties, so the unstable sort (no scratch buffer) orders as a stable one would
    rest.sort_unstable_by_key(|&i| (core::cmp::Reverse(s.grouped[i].len()), i));
    for &i in rest.iter() {
        perm.push(i);
    }
    let mut parents: ArenaVec<ArenaVec<usize>> = ArenaVec::with_capacity_in(cascaded, b.arena);
    for t in m..n {
        let mut c: ArenaVec<usize> = ArenaVec::new_in(b.arena);
        for u in (0..t).filter(|&u| strict_sub(s, perm[t], perm[u])) {
            c.push(u);
        }
        c.sort_unstable_by_key(|&u| (s.grouped[perm[u]].len(), u));
        parents.push(c);
    }
    Some(CascadePlan { perm, m, parents })
}

/// Whether plan `n` has a join node anywhere under it (outer and inner subtrees, through subquery scans).
fn has_join(n: Node<'_>) -> bool {
    crate::walk::stack_check();
    if n.as_hash_join().is_some() || n.as_merge_join().is_some() || n.as_nest_loop().is_some() {
        return true;
    }
    if let Some(q) = n.as_subquery_scan() {
        return q.subplan.is_some_and(has_join);
    }
    n.as_plan().is_some_and(|p| p.lefttree.is_some_and(has_join) || p.righttree.is_some_and(has_join))
}

/// One hardware thread's L2 in bytes: this host's cache facts (`gravity_program::cost::machine::host`, the one reading plan
/// cost and run time share; the reference machine's, 2 MiB, under the reference profile).
fn l2_bytes() -> Option<u64> {
    Some(gravity_program::cost::machine::host().l2_share())
}

/// The sets form T takes: `arms` (each set with its node), `phase_of` (each arm's node in `[a] + chain`), `grouped` (each
/// set's grouped input columns), `all` (their union, nodeAgg's all_grouped_cols).
pub(super) struct Sets<'a, 'p> {
    pub arms: &'a [(&'p Agg<'p>, Node<'p>)],
    pub phase_of: &'a [u16],
    pub grouped: &'a [ArenaVec<i16>],
    pub all: &'a [i16],
}

/// Why form T does not take the node (`None`: it does). Read before anything is declared, so a decline costs nothing.
pub(super) fn decline<'p>(b: &Builder<'p, '_>, a: &'p Agg<'p>, s: &Sets<'_, 'p>) -> Option<&'static str> {
    let Some(left) = a.plan.lefttree.filter(|_| b.alloc.is_some()) else { return Some(NO_ALLOC) };
    {
        let cx = crate::effects::Cx::new(b.stmt, b.facts, b.arena);
        let volatile = crate::effects::of_plan(&cx, left).acts || crate::effects::agg::volatile_inputs(&cx, &[&a.plan.targetlist, &a.plan.qual]);
        let mut sampled = Sampled::plan_reads(b, left);
        for e in a.plan.targetlist.iter().chain(a.plan.qual.iter()) {
            expr::run_aggrefs(e, &mut |r| sampled |= crate::effects::agg::agg_inputs(r).any(|i| Sampled::expr_reads(b, i)));
        }
        if !volatile && !sampled && rewalk_cheap(left) {
            return Some(CHEAP_REWALK);
        }
        if crate::effects::of_plan(&cx, left).acts && has_join(left) {
            return Some(VOLATILE_JOIN);
        }
    }
    let mut top = left.as_sort().and_then(|s| s.plan.lefttree).unwrap_or(left);
    loop {
        match (top.as_subquery_scan().and_then(|q| q.subplan), top.as_result().and_then(|r| r.plan.lefttree)) {
            (Some(n), _) | (None, Some(n)) => top = n,
            _ => break,
        }
    }
    if top.as_append().is_some() || top.as_merge_append().is_some() {
        return Some(UNION_INPUT);
    }
    let empties = s.grouped.iter().filter(|g| g.is_empty()).count();
    if empties > 1 || (empties == 1 && a.plan.targetlist.iter().chain(a.plan.qual.iter()).any(|e| {
        let mut sub = false;
        expr::run_aggrefs(e, &mut |r| sub |= expr::has_subplan(r));
        sub
    })) {
        return Some(EMPTY_SET);
    }
    // each grouped column's collation and equality operator, as every node that groups it names them
    let mut seen: ArenaVec<(i16, u32, u32)> = ArenaVec::new_in(b.arena);
    for node in core::iter::once(a).chain(a.chain.iter().filter_map(|n| n.as_agg())) {
        let ncols = (node.numCols.max(0) as usize).min(node.grpColIdx.len());
        for j in 0..ncols {
            let c = node.grpColIdx[j];
            let (coll, op) = (node.grpCollations.get(j).copied().unwrap_or(0), node.grpOperators.get(j).copied().unwrap_or(0));
            match seen.iter().find(|x| x.0 == c) {
                Some(&(_, c2, o2)) if c2 != coll || o2 != op => return Some(KEY_SEMANTICS),
                Some(_) => {}
                None => seen.push((c, coll, op)),
            }
        }
    }
    // more than one sorted phase: nodeAgg evaluates the aggregates' inputs once per phase
    let mut sorted = 0usize;
    for node in core::iter::once(a).chain(a.chain.iter().filter_map(|n| n.as_agg())) {
        if node.aggstrategy != AGG_HASHED && node.aggstrategy != AGG_MIXED {
            sorted += 1;
        }
    }
    if sorted > 1 {
        let cx = crate::effects::Cx::new(b.stmt, b.facts, b.arena);
        let lists = [&a.plan.targetlist, &a.plan.qual];
        let mut sampled = false;
        for l in lists {
            for e in l.iter() {
                expr::run_aggrefs(e, &mut |r| sampled |= crate::effects::agg::agg_inputs(r).any(|i| Sampled::expr_reads(b, i)));
            }
        }
        if sampled || crate::effects::agg::volatile_inputs(&cx, &lists) {
            return Some(PHASED_INPUTS);
        }
    }
    // D3047: only in a session whose statements run on the leader alone (`Facts::serial`, the width the statement runs at;
    // exec's front key carries the class, so each class gets its own graph). With workers T's parallel fold beats the
    // spool, whose fill and reads run serial (CUBE (a, b, c) over a 20M-row join at W8: T 2.1 s, spool 8.7 s).
    // D4620: a cascade's tagged table holds the sets it folds from the input alone (the finest), each coarser set folded
    // from a finer set's records one set at a time, so the locality rule (every set's groups in one table) does not hold
    let cascades = || cascade_plan(b, a, s, true).is_some();
    if let Some(l2) = l2_bytes().filter(|_| b.facts.serial() == Some(true) && !cascades()) {
        let mut aggs = 0u64;
        for e in a.plan.targetlist.iter().chain(a.plan.qual.iter()) {
            expr::run_aggrefs(e, &mut |_| aggs += 1);
        }
        let record = 8 * (s.all.len() as u64 + 1) + 16 * aggs + 16;
        let mut total = 0u64;
        for &(node, _) in s.arms.iter() {
            let sets = node.groupingSets.len().max(1) as u64;
            let bytes = (node.numGroups.max(1) as u64 / sets).max(1).saturating_mul(record);
            total = total.saturating_add(bytes);
        }
        if total > l2 {
            return Some(TABLE_LOCALITY);
        }
    }
    // D1520: an ordered-set aggregate's direct arguments read the set's projected slot
    let mut direct = false;
    for e in a.plan.targetlist.iter().chain(a.plan.qual.iter()) {
        expr::walk(e, &mut |x| {
            if let Some(ag) = x.as_aggref() {
                for d in ag.aggdirectargs.iter() {
                    expr::walk(d, &mut |y| {
                        direct |= y.as_var().is_some_and(|v| v.varno == OUTER_VAR && s.all.contains(&v.varattno));
                        true
                    });
                }
                return false;
            }
            true
        });
    }
    if direct {
        return Some(DIRECT_ARGS);
    }
    // an entry differs by set only through a GROUPING() (or a rollup order's constants, never a SubPlan): one that also
    // calls a SubPlan would be placed once per set's CASE branch
    if a.plan.targetlist.iter().chain(a.plan.qual.iter()).any(|e| has_grouping_func(e) && expr::has_subplan(e)) {
        return Some(SUBPLAN_BY_SET);
    }
    None
}

/// Where form T's columns sit in the row its group reads. The walk's columns are 1..=m (the input's, then the aggregates'
/// inputs). With the empty set (`empty`): m + 1 `live` (true in the input's rows, NULL in the sentinel's). Then the tag
/// at `tag_at`, then each column some set does not group (NULL there), at `key[j]`; a column every set groups keys as the
/// walk's own column.
struct Places {
    m: usize,
    base: usize,
    tag_at: i16,
    empty: Option<usize>,
    key: ArenaVec<i16>,
}

impl Places {
    fn of(arena: gravity_program::ArenaRef, m: usize, s: &Sets<'_, '_>) -> Places {
        let n = s.arms.len();
        let empty = s.grouped.iter().position(|g| g.is_empty());
        let base = m + usize::from(empty.is_some());
        let tag_at = base as i16 + 1;
        let mut key = ArenaVec::new_in(arena);
        let mut next = tag_at + 1;
        for &c in s.all.iter() {
            if (0..n).all(|i| s.grouped[i].contains(&c)) {
                key.push(c);
            } else {
                key.push(next);
                next += 1;
            }
        }
        Places { m, base, tag_at, empty, key }
    }

    fn live_at(&self) -> i16 {
        self.m as i16 + 1
    }
}

/// The node as form T (`decline` said yes). `order`: a sorted node's rollup order (D467), pushed over the group's emit.
pub(super) fn map<'p>(b: &mut Builder<'p, '_>, a: &'p Agg<'p>, s0: &Sets<'_, 'p>, order: Option<RollupOrder<'p>>, k: Kind, id: i32) -> Result<Open<'p>, Refusal> {
    let mcx = b.alloc.ok_or(Refusal::shape(k, id, "grouping sets: no node allocator (decompose_in)"))?;
    let oom = |_| Refusal::shape(k, id, "grouping sets: out of memory building the tagged group");
    let n = s0.arms.len();
    // D4620: under the cascade the tags number the sets in its order (the ones folded from the input first), so the
    // fan-out is the tags `0..m`
    let mut plan = cascade_plan(b, a, s0, false);
    let (mut arms, mut phase_of, mut grouped) = (ArenaVec::new_in(b.arena), ArenaVec::new_in(b.arena), ArenaVec::new_in(b.arena));
    for &i in plan.as_ref().map_or(&[][..], |p| &p.perm[..]) {
        arms.push(s0.arms[i]);
        phase_of.push(s0.phase_of[i]);
        let mut g: ArenaVec<i16> = ArenaVec::with_capacity_in(s0.grouped[i].len(), b.arena);
        for &c in s0.grouped[i].iter() {
            g.push(c);
        }
        grouped.push(g);
    }
    let permuted = Sets { arms: &arms, phase_of: &phase_of, grouped: &grouped, all: s0.all };
    let s = if plan.is_some() { &permuted } else { s0 };
    let m = plan.as_ref().map_or(n, |p| p.m);
    let spool = Spool::steps_of(b, a, k, id)?;
    let at = Places::of(b.arena, spool.width(), s);
    let tag_var = Node::mk_var(mcx, OUTER_VAR, at.tag_at, types_core::INT4OID, -1, 0, 0).map_err(oom)?;
    let live = match at.empty {
        Some(_) => Some(Node::mk_var(mcx, OUTER_VAR, at.live_at(), types_core::BOOLOID, -1, 0, 0).map_err(oom)?),
        None => None,
    };
    let mut tls: ArenaVec<&'p NodeList<'p>> = ArenaVec::new_in(b.arena);
    let mut quals: ArenaVec<&'p NodeList<'p>> = ArenaVec::new_in(b.arena);
    for i in 0..n {
        let tl = match &order {
            Some(o) => o.arm_tlist(b, a, s.phase_of[i], &s.grouped[i], k, id)?,
            None => &a.plan.targetlist,
        };
        check_grouping(&[tl, &a.plan.qual], k, id)?;
        let (tl, qual) = (spool.rewrite(b, tl, k, id)?, spool.rewrite(b, &a.plan.qual, k, id)?);
        let facts = b.facts;
        let mut f = |x: Node<'p>| tag_node(mcx, facts, x, &s.grouped[i], s.all, &at.key, live, i > 0);
        let tl = match nodes_core::mutate_list_dyn(mcx, tl, &mut f).map_err(oom)? {
            Some(new) => Node::mk_list(mcx, new).map_err(oom)?.as_list().expect("a list node"),
            None => tl,
        };
        let qual = match nodes_core::mutate_list_dyn(mcx, qual, &mut f).map_err(oom)? {
            Some(new) => Node::mk_list(mcx, new).map_err(oom)?.as_list().expect("a list node"),
            None => qual,
        };
        tls.push(tl);
        quals.push(qual);
    }
    let Some(tlist) = merge(b, &tls, tag_var, true).map_err(oom)? else { return Err(Refusal::shape(k, id, SUBPLAN_BY_SET)) };
    let Some(qual) = merge(b, &quals, tag_var, false).map_err(oom)? else { return Err(Refusal::shape(k, id, SUBPLAN_BY_SET)) };
    fires::fire("gsets.tag"); // shapecheck fire signal: grouping sets as one tagged group over one walk
    // the input walked once (with its own Sort when an aggregate input acts: PG's first phase reads it in that order),
    // the aggregates' inputs computed into columns, then each row fanned out once per set
    let cx = crate::effects::Cx::new(b.stmt, b.facts, b.arena);
    let acts = crate::effects::agg::volatile_inputs(&cx, &[&a.plan.targetlist, &a.plan.qual]);
    let input = a.plan.lefttree.and_then(|c| c.as_sort()).filter(|_| !acts).map_or(&a.plan, |srt| &srt.plan);
    let open = b.child(input, k)?;
    let walk = spool.phase_walk(b, open, k, id)?;
    let cols = outer_vars(mcx, spool.cols(), at.m).map_err(oom)?;
    let mut child = match at.empty {
        None => fan_out(b, walk, cols, None, &at, 0, m - 1).map_err(oom)?,
        Some(t0) => sentinel(b, walk, cols, &at, m, t0, id).map_err(oom)?,
    };
    crate::ops::project(&mut child, keys_of(b, cols, &at, s, n, tag_var).map_err(oom)?);
    // the keys: the tag, then each grouped column under its plan collation and operator
    let mut sh = Shape::new(b.arena);
    sh.key(b, &child, at.tag_at, 0, k, id)?;
    for (j, &c) in s.all.iter().enumerate() {
        let (coll, op) = key_semantics(a, c);
        sh.key(b, &child, at.key[j], coll, k, id)?;
        sh.eq_op(b, op);
    }
    sh.duplicate_keys(b, &child, &[tlist, qual], query_grouping(tlist), &[], k, id)?;
    let refs = collect(b.arena, &[tlist, qual], k, id)?;
    let mut distinct_key = NONE;
    for r in refs.iter().copied() {
        if expr::has_subplan(r) {
            child = crate::ops::subplan::attach_agg_arg(b, r, k, id, child)?;
        }
        cell(b, &child, r, &mut sh, &mut distinct_key, k, id)?;
    }
    let form = if distinct_key != NONE { GroupForm::Distinct } else { GroupForm::Hashed };
    // D4620: the record merge takes every slot of the group's record (no hosted state, no order-keyed numeric extreme) and
    // its keys are the tag and the sets' columns alone; else every row fans out to every set after all
    let layout_why = if form != GroupForm::Hashed {
        Some(CASCADE_NO_LAYOUT_FORM)
    } else if sh.keys.len() != 1 + s.all.len() {
        Some(CASCADE_NO_LAYOUT_KEYS)
    } else if sh.aggs.iter().any(|d| d.slot == NONE || d.trans.abi().is_none()) {
        Some(CASCADE_NO_LAYOUT_HOSTED)
    } else if sh.aggs.iter().any(|d| matches!(d.reg, Register::MinNumDyn | Register::MaxNumDyn) || matches!(d.final_, FinalForm::Partial | FinalForm::Distinct | FinalForm::OrderedFold | FinalForm::PairCount | FinalForm::PairFold)) {
        Some(CASCADE_NO_LAYOUT_FOLD)
    } else {
        None
    };
    if let Some(why) = layout_why.filter(|_| plan.is_some()) {
        fires::fire(CASCADE_NO_LAYOUT);
        fires::fire(why);
        plan = None;
        let arena = b.arena;
        let walk = match child.union {
            Some(u) => b.graph.unions[u as usize].arms.last_mut().expect("the sentinel's union holds the walk"),
            None => &mut child,
        };
        if !refan(mcx, arena, walk, &at, n - 1).map_err(oom)? {
            return Err(Refusal::shape(k, id, "grouping sets: the tagged fan-out's series not found to widen (D4620)"));
        }
    }
    // the planner's groups of every set (each node's estimate covers its own sets)
    let numgroups = core::iter::once(a).chain(a.chain.iter().filter_map(|x| x.as_agg())).fold(0u64, |t, x| t.saturating_add(x.numGroups.max(0) as u64));
    let groups = match form {
        GroupForm::Distinct => child.desc.predicted_rows.saturating_mul(n as u64).max(numgroups),
        _ => numgroups,
    };
    // D4620: under the cascade the base holds the sets folded from the input alone, a share of the planner's groups (each
    // cascaded set sizes its own from its share: `cascade`); the tables grow from what they observe
    let groups = plan.as_ref().map_or(groups, |p| (groups / n as u64).saturating_mul(p.m as u64).max(1));
    let (mut emit, base_slot) = close(b, child, id, k, form, [None; 2], sh, ORDER_NONE, false, groups, numgroups, None)?;
    if let Some(p) = plan.as_ref() {
        emit = cascade(b, emit, base_slot, p, s, numgroups, k, id)?;
    }
    crate::ops::filters(&mut emit, qual);
    crate::ops::project(&mut emit, tlist);
    let emit = crate::ops::list_subplans(b, &[tlist, qual], k, id, emit)?;
    b.graph.note(k);
    match order {
        Some(o) => o.sorted(b, emit, a, k, id),
        None => Ok(emit),
    }
}

/// The collation and equality operator node `[a] + chain` group input column `c` by (`decline` checked they agree).
fn key_semantics(a: &Agg<'_>, c: i16) -> (u32, u32) {
    for node in core::iter::once(a).chain(a.chain.iter().filter_map(|n| n.as_agg())) {
        if let Some(j) = node.grpColIdx.iter().position(|&x| x == c) {
            return (node.grpCollations.get(j).copied().unwrap_or(0), node.grpOperators.get(j).copied().unwrap_or(0));
        }
    }
    (0, 0)
}

fn int4<'p>(mcx: mcx::Mcx<'p>, v: i32) -> types_error::PgResult<Node<'p>> {
    Node::mk_const(mcx, types_core::INT4OID, -1, 0, 4, datum::Datum::from_i32(v), false, true).inspect(|c| crate::bake::derived_note(c.as_raw().as_ptr() as usize, crate::bake::Derived::Fixed))
}

/// A typed NULL of `e`'s type.
fn null_of<'p>(b: &Builder<'p, '_>, e: Node<'p>) -> types_error::PgResult<Node<'p>> {
    let mcx = b.alloc.expect("checked by `decline`");
    let (typ, typmod, coll) = (nodes_core::expr_type(e), nodes_core::expr_typmod(e), nodes_core::expr_collation(e));
    let (len, byval) = b.facts.type_row(typ).map_or((-1, false), |t| (t.typlen as i32, t.byval));
    Node::mk_const(mcx, typ, typmod, coll, len, datum::Datum::from_i64(0), true, byval)
}

/// The empty set `()` (nodeAgg emits its row over an empty input too, where a keyed group emits none): the input's rows
/// and ONE sentinel row in a union, each fanned out by its own ProjectSet with constant bounds (the input's rows to every
/// tag, the sentinel to the empty set's alone), so the empty set's group always exists. The sentinel's columns are NULL,
/// so a strict aggregate skips it (nodeAgg skips a row whose argument is NULL); `live` (true in the input's rows, NULL in
/// the sentinel) is what every other aggregate reads (`tag_node`: count(live), or FILTER (WHERE live)). The sentinel arm
/// is first: the group's producer life is the last arm's (`close`), the input's walk.
fn sentinel<'p>(b: &mut Builder<'p, '_>, walk: Open<'p>, cols: &'p NodeList<'p>, at: &Places, n: usize, t0: usize, id: i32) -> types_error::PgResult<Open<'p>> {
    let mcx = b.alloc.expect("checked by `decline`");
    let mut walk = fan_out(b, walk, cols, Some(Node::mk_const(mcx, types_core::BOOLOID, -1, 0, 1, datum::Datum::from_bool(true), false, true)?), at, 0, n - 1)?;
    // NEW-48: the input arm's own end rate (`close` reads the union's, which names no join)
    walk.desc.end_rate = end_rate(b, &walk, id);
    let mut l = NodeList::with_capacity(mcx, at.m)?;
    for (i, te) in cols.iter().filter_map(|x| x.as_target_entry()).enumerate() {
        l.lappend(mcx, Node::mk_target_entry(mcx, null_of(b, te.expr)?, i as i16 + 1, None, false)?)?;
    }
    let mut one = Open::new(b.arena, id, Source::Const, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite));
    crate::ops::project(&mut one, Node::mk_list(mcx, l)?.as_list().expect("a list node"));
    let null = Node::mk_const(mcx, types_core::BOOLOID, -1, 0, 1, datum::Datum::from_i64(0), true, true)?;
    let one = fan_out(b, one, cols, Some(null), at, t0, t0)?;
    let mut arms = ArenaVec::new_in(b.arena);
    arms.push(one);
    arms.push(walk);
    let u = b.graph.unions.len() as u32;
    b.graph.unions.push(crate::ops::shared::append::UnionRec { arms, order: Order::NONE, prune: None, starts: ArenaVec::new_in(b.arena), pushed: 0..0, sets: false });
    let mut out = Open::new(b.arena, id, Source::Const, crate::ops::shared::union::BASE);
    out.union = Some(u);
    Ok(out)
}

/// `open` (its last projection the walk's columns 1..=m) with `live` at m + 1 when given, then a ProjectSet of
/// `generate_series(lo, hi)` (constant bounds: the set tags this arm's rows fan out to) at `tag_at`.
fn fan_out<'p>(b: &Builder<'p, '_>, mut open: Open<'p>, cols: &'p NodeList<'p>, live: Option<Node<'p>>, at: &Places, lo: usize, hi: usize) -> types_error::PgResult<Open<'p>> {
    use crate::ops::shared::srf::{SrfDesc, SrfFn, SrfPlace};
    let mcx = b.alloc.expect("checked by `decline`");
    let mut pass = cols.clone_in(mcx)?;
    if let Some(live) = live {
        let mut l = cols.clone_in(mcx)?;
        l.lappend(mcx, Node::mk_target_entry(mcx, live, at.live_at(), None, false)?)?;
        crate::ops::project(&mut open, Node::mk_list(mcx, l)?.as_list().expect("a list node"));
        pass.lappend(mcx, Node::mk_target_entry(mcx, Node::mk_var(mcx, OUTER_VAR, at.live_at(), types_core::BOOLOID, -1, 0, 0)?, at.live_at(), None, false)?)?;
    }
    let call = Node::mk(mcx, FuncExpr { funcid: crate::catalog::srf::F_GENERATE_SERIES_INT4_INT4, funcresulttype: types_core::INT4OID, funcretset: true, args: NodeList::make2(mcx, int4(mcx, lo as i32)?, int4(mcx, hi as i32)?)?, location: -1, ..Default::default() })?;
    let mut l = pass;
    l.lappend(mcx, Node::mk_target_entry(mcx, call, at.tag_at, None, false)?)?;
    let tlist = Node::mk_list(mcx, l)?.as_list().expect("a list node");
    let f = call.as_func_expr().expect("just built");
    let mut functions = ArenaVec::new_in(b.arena);
    functions.push(SrfFn { resno: at.tag_at, node: call, funcid: f.funcid, args: &f.args, kind: crate::catalog::srf::kind_of(f.funcid, true), cols: 1, coltypes: &NoOids::<'p>::OIDS });
    open.push(Op::Srf(SrfDesc { place: SrfPlace::TargetList, functions, tlist }));
    crate::ops::project(&mut open, tlist);
    Ok(open)
}

/// D4620: `open`'s fan-out (`fan_out`'s ProjectSet, the tag's series) widened to the tags `lo..=hi`: the cascade's record
/// merge declined after the group's cells were made, so every row fans out to every set. Whether the series was found.
fn refan<'p>(mcx: mcx::Mcx<'p>, arena: gravity_program::ArenaRef, open: &mut Open<'p>, at: &Places, hi: usize) -> types_error::PgResult<bool> {
    use crate::ops::shared::srf::{SrfDesc, SrfFn};
    let series = |op: &Op<'p>| matches!(op, Op::Srf(d) if d.functions.len() == 1 && d.functions[0].resno == at.tag_at && d.functions[0].funcid == crate::catalog::srf::F_GENERATE_SERIES_INT4_INT4);
    let Some(j) = open.ops.iter().rposition(series) else { return Ok(false) };
    let (place, old, lo, kind) = match &open.ops[j] {
        Op::Srf(d) => (d.place, d.tlist, d.functions[0].args.nth(0), d.functions[0].kind),
        _ => return Ok(false),
    };
    let call = Node::mk(mcx, FuncExpr { funcid: crate::catalog::srf::F_GENERATE_SERIES_INT4_INT4, funcresulttype: types_core::INT4OID, funcretset: true, args: NodeList::make2(mcx, lo, int4(mcx, hi as i32)?)?, location: -1, ..Default::default() })?;
    let mut l = NodeList::with_capacity(mcx, old.iter().count())?;
    for e in old.iter() {
        match e.as_target_entry().filter(|te| te.resno == at.tag_at) {
            Some(_) => l.lappend(mcx, Node::mk_target_entry(mcx, call, at.tag_at, None, false)?)?,
            None => l.lappend(mcx, e)?,
        }
    }
    let tlist = Node::mk_list(mcx, l)?.as_list().expect("a list node");
    let f = call.as_func_expr().expect("just built");
    let mut functions = ArenaVec::new_in(arena);
    functions.push(SrfFn { resno: at.tag_at, node: call, funcid: f.funcid, args: &f.args, kind, cols: 1, coltypes: &NoOids::<'p>::OIDS });
    open.ops[j] = Op::Srf(SrfDesc { place, functions, tlist });
    if let Some(Op::Project(p)) = open.ops.get_mut(j + 1) {
        if core::ptr::eq(*p, old) {
            *p = tlist;
        }
    }
    Ok(true)
}

/// D4620: the cascaded sets (tags `m..n` of `plan`), each a group of the base's record (`base_slot`, the tagged group just
/// closed: its layout, keys and aggregates) whose producer reads its candidate parents' owned records (the base for a
/// parent among the first `m` tags, its narrowest such tag; an earlier cascaded set's group for the others), takes the
/// records of the parent's tag from the parent with the fewest records, rewrites each (its tag, the columns the set does
/// not group NULL) and folds it by the aggregates' merge (lowerv4 ops/agg/cascade.rs). Each group is a hashed group of its
/// own (its grant, owner phase and spill), sized from its share of the planner's groups and grown to what it folds; the
/// base's emit (`emit`) reads each after its own records.
#[allow(clippy::too_many_arguments)]
fn cascade<'p>(b: &mut Builder<'p, '_>, mut emit: Open<'p>, base_slot: u8, plan: &CascadePlan, s: &Sets<'_, 'p>, numgroups: u64, k: Kind, id: i32) -> Result<Open<'p>, Refusal> {
    use crate::ops::shared::agg::{Cascade, CascadeParent};
    let (layout, base) = {
        let g = b.graph.groups.last().expect("the base group was just declared");
        (g.layout, g.producer)
    };
    let order = breakers::decl_of(&b.graph, base_slot).map_or(Order::NONE, |d| d.order);
    // each set's share of the planner's groups (its node's estimate covers all its sets): its table's partitions and its
    // pipeline's rows (the parent's records it reads), so the grant asks no more than form T's one table did
    let share = (numgroups / s.arms.len().max(1) as u64).max(1);
    let bits = spill_bits(share, layout.size());
    let words = WordsDesc { topk: WordsDesc::NONE, dense_lo: W_DENSE_LO, dense_hi: W_DENSE_HI, bits: W_BITS, order: W_ORDER, nparams: NWORDS };
    let n = s.arms.len();
    let mut made: ArenaVec<(u8, LifeId)> = ArenaVec::with_capacity_in(n - plan.m, b.arena);
    for t in plan.m..n {
        let cands = &plan.parents[t - plan.m];
        let mut parents: ArenaVec<CascadeParent> = ArenaVec::new_in(b.arena);
        if let Some(&u) = cands.iter().find(|&&u| u < plan.m) {
            parents.push(CascadeParent { slot: base_slot, life: base, tag: u as i32 });
        }
        for &u in cands.iter().filter(|&&u| u >= plan.m) {
            let (slot, life) = made[u - plan.m];
            parents.push(CascadeParent { slot, life, tag: u as i32 });
        }
        let Some(first) = parents.first().copied() else { return Err(Refusal::shape(k, id, "grouping sets: a cascaded set without a parent (D4620)")) };
        let mut nulls: ArenaVec<u16> = ArenaVec::new_in(b.arena);
        for (j, _) in s.all.iter().enumerate().filter(|(_, c)| !s.grouped[t].contains(c)) {
            nulls.push(j as u16 + 1);
        }
        let mut l = Open::new(b.arena, id, Source::Breaker { slot: first.slot }, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite));
        for p in parents.iter() {
            if !l.desc.consumes.contains(&p.slot) {
                l.desc = breakers::consuming(b, l.desc, p.slot);
            }
        }
        l.desc.predicted_rows = share;
        let g = &mut b.graph;
        if g.pipelines.len() + 2 > limits::PIPELINES {
            return Err(Refusal::shape(k, id, "more than 65535 pipelines (the u16 pipeline id)"));
        }
        let slot = breakers::free_slot(g, &l).ok_or(Refusal::shape(k, id, "more than 256 live breaker slots (the u8 breaker slot id, limits::BREAKERS)"))?;
        let breaker = Breaker { kind: BreakerKind::HashAgg, slot, bytes: 0, budget: Budget::Share, spill: Spill::Partitioned { bits } };
        let ends = breakers::declare_producers(g, l, breaker, layout, order);
        for &p in ends.iter() {
            let p = p as usize;
            g.pipelines[p].desc = g.pipelines[p].desc.owner(OwnerDesc::generic(layout.record(b.arena), ORDER_NONE, 0));
            g.pipelines[p].desc = g.pipelines[p].desc.words(words);
        }
        let life = LifeId(*ends.last().expect("a push ends at least one pipeline"));
        made.push((slot, life));
        g.cascades.push(Cascade { producer: life, slot, base_slot, base, tag: t as i32, parents, nulls });
        emit.desc = breakers::consuming(b, emit.desc, slot);
    }
    fires::fire(CASCADE); // shapecheck fire signal (D4620): coarser grouping sets folded from a finer set's records
    Ok(emit)
}

/// The keyed projection after the fan-out: the row's columns 1..=base, the tag, then each column some set does not group
/// at its `Places::key`, NULL where the tag's set does not group it.
fn keys_of<'p>(b: &Builder<'p, '_>, cols: &'p NodeList<'p>, at: &Places, s: &Sets<'_, 'p>, n: usize, tag: Node<'p>) -> types_error::PgResult<&'p NodeList<'p>> {
    let mcx = b.alloc.expect("checked by `decline`");
    let mut l = cols.clone_in(mcx)?;
    if at.empty.is_some() {
        l.lappend(mcx, Node::mk_target_entry(mcx, Node::mk_var(mcx, OUTER_VAR, at.live_at(), types_core::BOOLOID, -1, 0, 0)?, at.live_at(), None, false)?)?;
    }
    l.lappend(mcx, Node::mk_target_entry(mcx, tag, at.tag_at, None, false)?)?;
    for (j, &c) in s.all.iter().enumerate() {
        if at.key[j] == c {
            continue; // every set groups it: the key is the walk's own column
        }
        let src = cols.iter().filter_map(|x| x.as_target_entry()).find(|te| te.resno == c).expect("a grouped column is an input column").expr;
        let (typ, typmod, coll) = (nodes_core::expr_type(src), nodes_core::expr_typmod(src), nodes_core::expr_collation(src));
        let var = Node::mk_var(mcx, OUTER_VAR, c, typ, typmod, coll, 0)?;
        let null = null_of(b, src)?;
        // the sets that group c (the shorter WHEN list; the other side is the default)
        let grouping = (0..n).filter(|&i| s.grouped[i].contains(&c)).count();
        let few = grouping * 2 <= n;
        let (then, other) = if few { (var, null) } else { (null, var) };
        let mut whens = NodeList::with_capacity(mcx, n)?;
        for i in (0..n).filter(|&i| s.grouped[i].contains(&c) == few) {
            whens.lappend(mcx, Node::mk(mcx, CaseWhen { expr: Some(tag_is(mcx, tag, i)?), result: Some(then), location: -1 })?)?;
        }
        let e = Node::mk(mcx, CaseExpr { casetype: typ, casecollid: coll, arg: None, args: whens, defresult: Some(other), location: -1 })?;
        l.lappend(mcx, Node::mk_target_entry(mcx, e, at.key[j], None, false)?)?;
    }
    Ok(Node::mk_list(mcx, l)?.as_list().expect("a list node"))
}

/// The empty column-definition list of the fan-out's call (a scalar `int4` series), at the plan's lifetime.
struct NoOids<'p>(core::marker::PhantomData<&'p ()>);

impl<'p> NoOids<'p> {
    const OIDS: types_nodes::list::OidList<'p> = types_nodes::list::OidList::nil();
}

/// `tag = i` (int4eq), its constant fixed by the plan's sets (`Derived::Fixed`).
fn tag_is<'p>(mcx: mcx::Mcx<'p>, tag: Node<'p>, i: usize) -> types_error::PgResult<Node<'p>> {
    let c = Node::mk_const(mcx, types_core::INT4OID, -1, 0, 4, datum::Datum::from_i32(i as i32), false, true)?;
    crate::bake::derived_note(c.as_raw().as_ptr() as usize, crate::bake::Derived::Fixed);
    Node::mk(mcx, OpExpr { opno: INT4EQ_OP, opfuncid: INT4EQ_FN, opresulttype: types_core::BOOLOID, opretset: false, opcollid: 0, inputcollid: 0, args: NodeList::make2(mcx, tag, c)?, location: -1 })
}

/// One list from the sets' lists (same length, entry by entry): an entry every set reads alike stays the first set's;
/// one that differs reads `CASE WHEN tag = 0 THEN e0 WHEN tag = 1 THEN e1 .. ELSE e(n-1) END`. `None` when such an entry
/// calls a SubPlan (`SUBPLAN_BY_SET`). `tl`: the lists are target lists (the entries' expressions compared).
fn merge<'p>(b: &Builder<'p, '_>, lists: &[&'p NodeList<'p>], tag: Node<'p>, tl: bool) -> types_error::PgResult<Option<&'p NodeList<'p>>> {
    let mcx = b.alloc.expect("checked by `decline`");
    let first = lists[0];
    let expr_of = |n: Node<'p>| if tl { n.as_target_entry().map_or(n, |te| te.expr) } else { n };
    let mut out: Option<NodeList<'p>> = None;
    for (j, n0) in first.iter().enumerate() {
        let e0 = expr_of(n0);
        let same = lists[1..].iter().all(|l| l.as_slice().get(j).is_some_and(|&x| types_nodes::equal(expr_of(x), e0)));
        if same {
            continue;
        }
        let es: ArenaVec<Node<'p>> = { let mut v = ArenaVec::new_in(b.arena); v.extend(lists.iter().map(|l| expr_of(l.as_slice()[j]))); v };
        if es.iter().any(|&e| expr::has_subplan(e)) {
            return Ok(None);
        }
        let (typ, coll) = (nodes_core::expr_type(e0), nodes_core::expr_collation(e0));
        let last = es.len() - 1;
        let mut whens = NodeList::with_capacity(mcx, last)?;
        for (i, &e) in es.iter().enumerate().take(last) {
            whens.lappend(mcx, Node::mk(mcx, CaseWhen { expr: Some(tag_is(mcx, tag, i)?), result: Some(e), location: -1 })?)?;
        }
        let case = Node::mk(mcx, CaseExpr { casetype: typ, casecollid: coll, arg: None, args: whens, defresult: Some(es[last]), location: -1 })?;
        let new = match n0.as_target_entry().filter(|_| tl) {
            Some(te) => Node::mk(mcx, types_nodes::primnodes::TargetEntry { expr: case, ..*te })?,
            None => case,
        };
        let l = match &mut out {
            Some(l) => l,
            None => out.insert(first.clone_in(mcx)?),
        };
        l.as_mut_slice()[j] = new;
    }
    Ok(Some(match out {
        Some(l) => Node::mk_list(mcx, l)?.as_list().expect("a list node"),
        None => first,
    }))
}

/// A set's lists for form T (`arm_node`'s twin over the tagged keys): an `OUTER_VAR` read of a grouped column (outside
/// the aggregates' arguments) reads its key column (`key`), NULL where the set does not group it; `GROUPING()` is the
/// set's mask. `fresh` (every set after the first) copies the literals, as `arm_node` does, so no two CASE branches share
/// a literal node. `live` (the empty set's sentinel, `sentinel`): an aggregate the sentinel row's NULLs do not skip reads
/// `FILTER (WHERE live)`.
#[allow(clippy::too_many_arguments)]
fn tag_node<'p>(mcx: mcx::Mcx<'p>, facts: &dyn crate::catalog::Facts, x: Node<'p>, grouped: &[i16], all: &[i16], key: &[i16], live: Option<Node<'p>>, fresh: bool) -> types_error::PgResult<Option<Node<'p>>> {
    crate::walk::stack_check(); // D3748
    if let Some(ag) = x.as_aggref() {
        // its arguments read input rows; its direct arguments read no grouped column (`decline`)
        let Some(live) = live.filter(|_| counts_null_row(facts, ag)) else { return Ok(None) };
        if super::trans::table::row(ag.aggfnoid).is_some_and(|r| r.class == crate::ops::shared::agg_class::Class::CountStar) && ag.args.is_nil() {
            // count(*) -> count(live): the count register over a column the sentinel reads NULL, no FILTER per row
            // (measured: FILTER live on count(*) cost a CUBE over a 20M-row join 16.0 s against 11.0 s without the
            // empty set)
            let te = Node::mk_target_entry(mcx, live, 1, None, false)?;
            let new = Aggref { aggfnoid: crate::normalize::F_COUNT_ANY, aggargtypes: types_nodes::list::OidList::make1(mcx, types_core::BOOLOID)?, aggdirectargs: ag.aggdirectargs.clone_in(mcx)?, args: NodeList::make1(mcx, te)?, aggorder: ag.aggorder.clone_in(mcx)?, aggdistinct: ag.aggdistinct.clone_in(mcx)?, aggstar: false, ..*ag };
            return Node::mk(mcx, new).map(Some);
        }
        let filter = match ag.aggfilter {
            None => live,
            Some(f) => Node::mk(mcx, types_nodes::primnodes::BoolExpr { boolop: types_nodes::primnodes::BoolExprType::AND_EXPR, args: NodeList::make2(mcx, f, live)?, location: -1 })?,
        };
        let new = Aggref { aggargtypes: ag.aggargtypes.clone_in(mcx)?, aggdirectargs: ag.aggdirectargs.clone_in(mcx)?, args: ag.args.clone_in(mcx)?, aggorder: ag.aggorder.clone_in(mcx)?, aggdistinct: ag.aggdistinct.clone_in(mcx)?, aggfilter: Some(filter), ..*ag };
        return Node::mk(mcx, new).map(Some);
    }
    if let Some(v) = x.as_var().filter(|v| v.varno == OUTER_VAR) {
        let Some(j) = all.iter().position(|&c| c == v.varattno) else { return Ok(None) };
        if key[j] == v.varattno {
            return Ok(None);
        }
        return Node::mk_var(mcx, OUTER_VAR, key[j], v.vartype, v.vartypmod, v.varcollid, 0).map(Some);
    }
    if x.as_grouping_func().is_some() || (fresh && x.as_const().is_some()) {
        return arm_node(mcx, facts, x, grouped, all, fresh);
    }
    nodes_core::expression_tree_mutator_dyn(mcx, x, &mut |y| tag_node(mcx, facts, y, grouped, all, key, live, fresh))
}

/// Whether aggregate `ag` would fold the empty set's sentinel row (all its columns NULL): nodeAgg skips a row only for a
/// strict transition function with a NULL argument, so any aggregate but a built-in strict one over arguments the
/// sentinel reads NULL (a column, never a non-NULL literal) counts it: `count(*)`, an ordered-set or user aggregate,
/// `array_agg`.
fn counts_null_row(facts: &dyn crate::catalog::Facts, ag: &Aggref<'_>) -> bool {
    let Some(row) = crate::ops::agg::trans::table::row(ag.aggfnoid) else { return true };
    if row.kind != b'n' || ag.args.is_nil() || !facts.func(row.transfn_oid).is_some_and(|p| p.strict) {
        return true;
    }
    ag.args.iter().filter_map(|t| t.as_target_entry()).any(|te| te.expr.as_const().is_some_and(|c| !c.constisnull))
}
