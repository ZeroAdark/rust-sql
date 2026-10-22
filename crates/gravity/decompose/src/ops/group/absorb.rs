//! D612: a sorted group absorbs its key Sort. A Unique, a Group or a sorted aggregate
//! (GroupAggregate) directly over a full Sort whose keys are exactly the group's keys (the same columns in the same
//! order, each in the group's collation, through a built-in ordering operator) is form K's (runs.unique / runs.group):
//! its records live in a keyed table, and its emit sorts them by the keys, each in the direction and NULL placement
//! the group declares (runs_topk_groups `key_words`). The Sort only re-orders rows the table then hashes, so the node maps
//! the Sort's input (`sort::map` passes it through) and declares the Sort's directions on its keys itself: the scan
//! folds into the key set, and the emit after it is key-ordered. This ports lowerv2's presort / sorted-chain arms
//! (compose/chains/presort.rs `try_unique`, compose.rs `sorted_chain`).
//!
//! The Sort stays a pipe wherever its order could show:
//! - a key the emit would not order as the Sort did, or whose equal values can differ (anything but the word types
//!   catalog/word_order.rs `ordered` names and text under a deterministic collation: an enum, a float, numeric,
//!   interval, uuid, bit string, bytea, a nondeterministic collation, an unpadded bpchar), since the group's printed
//!   key is then its first row's;
//! - an aggregate outside `order_free`, or a DISTINCT, ORDER BY or direct argument inside a call, or a split aggregate;
//!   (`order_free` and the DISTINCT calls' tests are the facts home's: facts/order.rs `absorb_free`)
//! - DISTINCT ON (the first-of-run op reads the order);
//! - a Sort with a qual, an InitPlan, a hint or a Limit of its own;
//! - a Sort PostgreSQL keeps at a rescan (`rescan_kept`: initialized with REWIND, MARK or BACKWARD, e.g. under a nest's
//!   inner with no NestLoopParam): its rewound rows are the keep boundary a rerun set reads (rerun.rs
//!   `declare_breaker_rerun`), so the join below it runs once, where the absorbed group would refill at every re-open.
//!
//! D345 (M6-FOLD): a sorted aggregate over no key whose calls are all DISTINCT and order-free absorbs the presort
//! PostgreSQL puts under them (`distinct_sort`): the grouped distinct form hashes each call's set itself.
use gravity_program::ArenaVec;
use types_nodes::plannodes::{Agg, Plan};
use types_nodes::primnodes::{Aggref, OUTER_VAR};
use types_nodes::Node;

use crate::catalog::word_order;
use crate::facts::order::{call_order_free, distinct_order_free};
use crate::ops::shared::agg::KeyDecl;
use crate::facts::hints::Hint;
use crate::catalog::typefacts;
use crate::walk::Builder;

/// D612's absorb test (sorted aggregate `a`'s calls fold the same in any row order), facts/order.rs
/// `absorb_order_free`; agg's map and the stop road ask it through this name.
pub use crate::facts::order::absorb_order_free as order_free;

/// The Sort a group absorbs: its node id, and per key column the Sort's direction and NULL placement.
pub struct KeySort {
    pub id: i32,
    dirs: ArenaVec<(i16, bool, bool)>,
    /// D4676 (`chain_of`): the Unique between the group and this Sort, which absorbs the Sort and hands its rows on
    /// unordered (the group sets `Builder::chain` to it); None when the Sort is the group's own child.
    pub chain: Option<i32>,
}

impl KeySort {
    /// Each key over one of the Sort's columns takes that column's direction and NULL placement (what `agg::ordered`
    /// reads from the Sort's order when the Sort is a pipe).
    pub fn direct(&self, keys: &mut [KeyDecl<'_>]) {
        for key in keys.iter_mut() {
            if let Some(&(_, desc, nulls_first)) = self.dirs.iter().find(|d| d.0 == key.resno) {
                (key.desc, key.nulls_first) = (desc, nulls_first);
            }
        }
    }
}

/// The key Sort under group node `plan` over key columns `cols` in collations `colls`, when the group may absorb it:
/// D612's test, else under road `group.hashchain` (D4676, default off) where no reader reads the order it would keep
/// (`road_sort`).
pub fn key_sort<'p>(b: &Builder<'p, '_>, plan: &'p Plan<'p>, cols: &[i16], colls: &[u32]) -> Option<KeySort> {
    sort_dirs(b, plan, cols, colls, true).or_else(|| road_sort(b, plan, cols, colls))
}

/// D2061: the same full Sort by any key type, or an Incremental Sort (it orders by every key as a Sort does), which
/// stays a pipe, so a group's printed key is its first row's, as in PostgreSQL: a sorted group by more than eight keys
/// reads its directions past the eight keys an `Order` holds.
pub fn wide_sort<'p>(b: &Builder<'p, '_>, plan: &'p Plan<'p>, cols: &[i16], colls: &[u32]) -> Option<KeySort> {
    sort_dirs(b, plan, cols, colls, false)
}

fn sort_dirs<'p>(b: &Builder<'p, '_>, plan: &'p Plan<'p>, cols: &[i16], colls: &[u32], absorbs: bool) -> Option<KeySort> {
    dirs_of(b, plan, cols, colls, absorbs, absorbs)
}

/// `sort_dirs` with its key-type test apart: `same` asks every key to be one value when equal (`same_when_equal`).
fn dirs_of<'p>(b: &Builder<'p, '_>, plan: &'p Plan<'p>, cols: &[i16], colls: &[u32], absorbs: bool, same: bool) -> Option<KeySort> {
    let s = plain_sort(b, plan, !absorbs).filter(|s| !absorbs || !rescan_kept(b, s))?;
    let sp = &s.plan;
    let n = cols.len();
    if n == 0 || s.numCols.max(0) as usize != n || s.sortColIdx.len() < n || s.sortOperators.len() < n || s.nullsFirst.len() < n {
        return None;
    }
    let mut dirs = ArenaVec::with_capacity_in(n, b.arena);
    for i in 0..n {
        let (c, coll) = (cols[i], colls.get(i).copied().unwrap_or(0));
        if s.sortColIdx[i] != c || s.collations.get(i).copied().unwrap_or(0) != coll || c < 1 || c as usize > sp.targetlist.len() {
            return None;
        }
        let desc = b.facts.key(crate::catalog::KeyReq::sort(s.sortOperators[i])).and_then(crate::catalog::KeyOrder::dir)?;
        let v = sp.targetlist.nth(c as usize - 1).as_target_entry()?.expr.as_var()?;
        if same && !same_when_equal(b, v.vartype, v.vartypmod, coll) {
            return None;
        }
        dirs.push((c, desc, s.nullsFirst[i]));
    }
    Some(KeySort { id: sp.plan_node_id, dirs, chain: None })
}

/// D4676: the road (default off: lowerv4 road_off.rs `DEFAULT_OFF`, read through `Facts::road_on`) under which a sorted
/// chain is served hashed where no reader reads the order it would pay for (`road_sort`).
pub const HASH_CHAIN_ROAD: &str = "group.hashchain";

/// D4676 (road `group.hashchain`): the Sort under group node `plan` (keys `cols` in collations `colls`) that D612's test
/// keeps a pipe, absorbed where nothing reads the order it would give. Each key in a deterministic collation (text
/// equal only when its bytes are); any other key type its hashed table keys (numeric by its image: 1.0 = 1.00, as
/// PostgreSQL's hash opclass). Which of two equal rows the group keeps (its printed key) is then the first to reach its
/// table, where PostgreSQL's Sort leaves equal keys in no fixed order either.
/// - `plan` is the Unique a sorted group above reads (`Builder::chain`, set from `chain_of`'s answer): the Unique hashes
///   its rows with its full Sort absorbed and hands them on unordered (`agg::map_set`).
/// - `plan`'s own rows may come in any order (`Builder::order_free`): its direct full Sort is absorbed and the emit hands
///   the groups on unordered (`agg::key_order`), so no key needs to order by its words. Fires `group.freesort`.
/// - Otherwise (no direct Sort, or the group's order read) `chain_of`: a group over a Unique over the Unique's key Sort.
fn road_sort<'p>(b: &Builder<'p, '_>, plan: &'p Plan<'p>, cols: &[i16], colls: &[u32]) -> Option<KeySort> {
    if cols.is_empty() || !b.facts.road_on(HASH_CHAIN_ROAD) {
        return None;
    }
    let det = |colls: &[u32], n: usize| (0..n).all(|i| colls.get(i).is_none_or(|&c| c == 0 || b.facts.coll(c).bytes_eq));
    if b.chain == Some(plan.plan_node_id) {
        return det(colls, cols.len()).then(|| dirs_of(b, plan, cols, colls, true, false)).flatten();
    }
    if let Some(s) = det(colls, cols.len()).then(|| dirs_of(b, plan, cols, colls, true, false)).flatten().filter(|_| b.order_free) {
        fires::fire("group.freesort"); // shapecheck fire signal (D4676): an order-free group's key Sort absorbed, any key type
        return Some(s);
    }
    chain_of(b, plan, cols, colls)
}

/// D4676: PostgreSQL's Sort -> Unique -> sorted group chain (a UNION's dedupe under a GROUP BY over its leading columns)
/// served hashed. Group node `plan` over keys `cols` in collations `colls` (map_agg asks only for a sorted aggregate whose
/// calls fold the same in any row order, nothing acting per group, no hint; map_set for a Group) directly over a Unique
/// with no qual or InitPlan, keyed on each of its output columns (no DISTINCT ON first-of-run), over a full Sort that only
/// re-orders (`plain_sort`, not kept at a rescan) by exactly the Unique's keys in their collations, each deterministic;
/// the group's keys are the Unique's leading keys in the same collations. Its answer carries the Sort's directions (the
/// group's keys take theirs) and the Unique's node id (`KeySort::chain`): the group sets `Builder::chain`, the Unique
/// then absorbs its Sort whatever the key types and hands its rows on unordered, and the group, which hashes them, orders
/// only its own groups by these keys where a reader above reads that order (`agg::key_order`); then the group's keys
/// must order by their key words as the Sort did (`same_when_equal`, D612's rule). Fires `group.hashchain`.
fn chain_of<'p>(b: &Builder<'p, '_>, plan: &'p Plan<'p>, cols: &[i16], colls: &[u32]) -> Option<KeySort> {
    let u = plan.lefttree?.as_unique()?;
    let up = &u.plan;
    if plan.righttree.is_some() || up.righttree.is_some() || !up.qual.is_nil() || !up.initPlan.is_nil() {
        return None;
    }
    let n = (u.numCols.max(0) as usize).min(u.uniqColIdx.len());
    let (ucols, ucolls) = (&u.uniqColIdx[..n], u.uniqCollations);
    if n < cols.len() || up.targetlist.len() != n || ucolls.len() < n {
        return None;
    }
    // every output column a key, each its input's column passed through (a planner Unique's dummy target list)
    for (i, e) in up.targetlist.iter().enumerate() {
        let v = e.as_target_entry()?.expr.as_var()?;
        if v.varno != OUTER_VAR || v.varlevelsup != 0 || v.varattno as usize != i + 1 || !ucols.contains(&((i + 1) as i16)) {
            return None;
        }
    }
    if (0..cols.len()).any(|i| cols[i] != ucols[i] || colls.get(i).copied().unwrap_or(0) != ucolls[i]) {
        return None;
    }
    if !ucolls[..n].iter().all(|&c| c == 0 || b.facts.coll(c).bytes_eq) {
        return None;
    }
    let mut ks = dirs_of(b, up, ucols, &ucolls[..n], true, false)?;
    if !b.order_free {
        let sp = &up.lefttree?.as_plan()?.targetlist;
        for (i, &c) in cols.iter().enumerate() {
            let v = sp.nth(c as usize - 1).as_target_entry()?.expr.as_var()?;
            if !same_when_equal(b, v.vartype, v.vartypmod, colls.get(i).copied().unwrap_or(0)) {
                return None;
            }
        }
    }
    ks.chain = Some(up.plan_node_id);
    fires::fire("group.hashchain"); // shapecheck fire signal (D4676): a sorted group over a Unique over its Sort, hashed
    Some(ks)
}

/// The full Sort directly under `plan` that only re-orders: no qual, InitPlan, hint or Limit of its own, and a target
/// list passing its input's columns through in order (a planner Sort never projects; a Const entry is its input's own
/// Const, D3735). With `incremental`, an Incremental Sort's sort too (only a pipe reads it: the group never absorbs one).
fn plain_sort<'p>(b: &Builder<'p, '_>, plan: &'p Plan<'p>, incremental: bool) -> Option<&'p types_nodes::plannodes::Sort<'p>> {
    let l = plan.lefttree?;
    let s = l.as_sort().or_else(|| l.as_incremental_sort().filter(|_| incremental).map(|i| &i.sort))?;
    let sp = &s.plan;
    if plan.righttree.is_some() || sp.righttree.is_some() || sp.lefttree.is_none() || !sp.qual.is_nil() || !sp.initPlan.is_nil() {
        return None;
    }
    if Hint::of_sort(s).is_some() || b.limit.is_some_and(|l| l.0 == sp.plan_node_id) {
        return None;
    }
    let below = &sp.lefttree?.as_plan()?.targetlist;
    for (i, e) in sp.targetlist.iter().enumerate() {
        let x = e.as_target_entry()?.expr;
        // D3735: setrefs.c set_dummy_tlist_references keeps a Const as the Const (`SELECT DISTINCT (1) AS extra, ...`),
        // the same entry its input computes at that position, so it passes that column through too
        if x.as_const().is_some() {
            match below.iter().nth(i).and_then(|c| c.as_target_entry()) {
                Some(c) if types_nodes::equal(c.expr, x) => continue,
                _ => return None,
            }
        }
        let v = x.as_var()?;
        if v.varno != OUTER_VAR || v.varlevelsup != 0 || v.varattno as usize != i + 1 {
            return None;
        }
    }
    Some(s)
}

/// Whether PostgreSQL initializes Sort `s` with REWIND, MARK or BACKWARD (lifetime/keep.rs `eflags_in`): it then keeps
/// its sorted rows at a rescan that changes no param and rewinds them (the keep table's kept Sort), never re-running its
/// input. Unknown flags (the node not found) count as kept.
fn rescan_kept(b: &Builder<'_, '_>, s: &types_nodes::plannodes::Sort<'_>) -> bool {
    use crate::lifetime::{BACKWARD, MARK, REWIND};
    crate::lifetime::eflags_in(b.stmt, s.plan.plan_node_id).is_none_or(|ef| ef & (REWIND | MARK | BACKWARD) != 0)
}

/// D345 (M6-FOLD): a sorted aggregate whose every key the planner dropped (numCols 0), or (HITS9-E5) a plain one, over the
/// Sort PostgreSQL puts under its DISTINCT calls (the presorted aggregate input, PG 16+: `count(DISTINCT k)` plans as an
/// Aggregate over a Sort by k): the node's id when one call is DISTINCT and every call folds to one result in any row
/// order (`distinct_order_free`, `call_order_free`), so the distinct form's hashed sets need no order and the pinned keys
/// read the scan. Any order-sensitive call keeps the Sort a pipe; so does, for a plain Agg (map_agg's `keeps` asks it of a
/// sorted one), a VOLATILE argument, FILTER or target list or one that acts per group (D1069 / D2249), which PostgreSQL
/// runs in the Sort's order, and DISTINCT calls over different arguments (or collations): the Sort presorts one of them,
/// the others sort their own input, and that form stays PostgreSQL's. NEW-33: the Sort may read hash joins, but a plain
/// fold over more than one relation (`one_relation`) keeps it unless its input is hash joins over relations
/// (`hash_joined`). NEW-41: a RIGHT or FULL hash join's drain may feed the set too (its probe and drain hand rows on in
/// the probe's parallel arms; a spooled one's end is lowerv4 effects/visible/drain.rs `order_free`'s plain DISTINCT set);
/// a RIGHT SEMI / RIGHT ANTI drain keeps the Sort: its lone drain feeds the set alone, spooled, and its probe then runs
/// on one worker (W8 1.8-3.1x slower than the Sort). Over a nested loop the hashed set measured slower than the Sort
/// (1.4-1.6x at 6 k-57 k rows) and a merge join is unmeasured. It keeps it too beside a call that may raise by its partial
/// sums (`effects::raises_by_order`: money, interval, bit string folds), whose order over a join is the join's, and over a
/// draining join (`drains`) beside a numeric sum / avg / min / max rider (`numeric_rider`): over the drained side the pair
/// set does not hold that rider (agg numeric.rs `num_scale` finds no packed form there), so the plain end would sort the
/// DISTINCT values on the leader (W8 1.18x the Sort's form at 300k rows; catalog NEW-41 NOTE). The Sort also stays over
/// a draining join PostgreSQL estimates at fewer rows than `drain_set_rows` (NEW-51: the cost home's break-even of the two
/// forms for the DISTINCT key's representation at the server's width).
pub fn distinct_sort<'p>(b: &Builder<'p, '_>, a: &'p Agg<'p>) -> Option<i32> {
    use crate::ops::shared::agg::{AGG_PLAIN, AGG_SORTED};
    if !matches!(a.aggstrategy, AGG_SORTED | AGG_PLAIN) || a.numCols > 0 || a.aggsplit != 0 {
        return None;
    }
    if a.aggstrategy == AGG_PLAIN {
        let (cx, lists) = (crate::effects::Cx::new(b.stmt, b.facts, b.arena), [&a.plan.targetlist, &a.plan.qual]);
        if crate::effects::agg::volatile_inputs(&cx, &lists) || crate::effects::agg::per_group_acts(&cx, &lists) {
            return None;
        }
    }
    // D3374 (D345): a sorted Agg whose every key the planner dropped prints a pinned column from its first input row,
    // the Sort's first. A pinned column whose equal values are not one image (agg keys.rs `one_image`: a numeric's
    // dscale, a float's -0 / 0, a bpchar, a nondeterministic collation) is the plain fold's kept column (fold/kept.rs,
    // never a key), so the Sort stays a pipe: lowerv4 feeds the group in its exact order and keeps PostgreSQL's first row
    if a.aggstrategy == AGG_SORTED && pin_shows_member(b, a) {
        return None;
    }
    let s = plain_sort(b, &a.plan, false).filter(|s| !rescan_kept(b, s))?;
    let (mut calls, mut any, mut one, mut raises, mut numeric, mut word) = (true, false, true, false, false, true);
    let mut first: Option<(ArenaVec<Node<'p>>, u32)> = None;
    let mut judge = |r: &Aggref<'p>| {
        calls &= distinct_order_free(b, r) || call_order_free(b, r);
        let class = crate::ops::agg::trans::table::row(r.aggfnoid).map(|row| row.class);
        raises |= class.is_some_and(crate::effects::raises_by_order);
        numeric |= r.aggdistinct.is_nil() && class.is_some_and(numeric_rider);
        if r.aggdistinct.is_nil() {
            return;
        }
        any = true;
        word &= r.aggargtypes.iter().all(|t| one_word(b, t));
        if let Some((args, coll)) = &first {
            one &= *coll == r.inputcollid && args.len() == r.args.len() && args.iter().zip(r.args.iter()).all(|(x, y)| types_nodes::equal(*x, y));
            return;
        }
        let mut args = ArenaVec::new_in(b.arena); // alloc: Cold
        args.extend(r.args.iter());
        first = Some((args, r.inputcollid));
    };
    let seen = [&a.plan.targetlist, &a.plan.qual].iter().all(|l| l.iter().all(|n| aggrefs_in(n, &mut judge)));
    let drained = s.plan.lefttree.is_some_and(drains) && (numeric || crate::admit::plan_rows(&s.plan) < drain_set_rows(b, word));
    let joined = a.aggstrategy == AGG_PLAIN && !s.plan.lefttree.is_some_and(one_relation) && (raises || !s.plan.lefttree.is_some_and(hash_joined) || drained);
    (seen && calls && any && !joined && (one || a.aggstrategy == AGG_SORTED)).then_some(s.plan.plan_node_id)
}

/// D3374 (D345): whether Agg `a` reads, outside its aggregates (its non-junk target list and its qual: facts::demand
/// `agg_reads` under `Junk::Skip`), an input column whose equal values are not one image under its type and collation.
fn pin_shows_member(b: &Builder<'_, '_>, a: &Agg<'_>) -> bool {
    use crate::facts::demand::{agg_reads, Junk};
    let mut many = false;
    agg_reads(&a.plan.targetlist, &a.plan.qual, Junk::Skip, &mut |v| many |= !crate::catalog::keysem::one_image(b.facts, v.vartype, v.varcollid));
    many
}

/// A Sort input that reads one relation: a scan, or a Gather over one.
fn one_relation(n: Node<'_>) -> bool {
    let scan = |n: Node<'_>| n.as_seq_scan().is_some() || n.as_index_scan().is_some() || n.as_index_only_scan().is_some() || n.as_bitmap_heap_scan().is_some();
    scan(n) || n.as_gather().is_some_and(|g| g.plan.lefttree.is_some_and(scan))
}

/// NEW-33: whether the plan under `n` is hash joins over relations only: a scan (`one_relation`), a Gather over one, or a
/// hash join whose two sides are such plans (the inner one under its Hash) and that does not drain its build (inner,
/// left, semi, anti; join.rs `hash_drains`) or, NEW-41, drains it into the joined rows (right, full: `drain_hands`). A
/// RIGHT SEMI / RIGHT ANTI hash join, a nested loop, a merge join or any other node keeps the Sort.
fn hash_joined(n: Node<'_>) -> bool {
    crate::walk::stack_check();
    if one_relation(n) {
        return true;
    }
    if let Some(h) = n.as_hash() {
        return h.plan.lefttree.is_some_and(hash_joined);
    }
    n.as_hash_join().is_some_and(|j| (!crate::join::hash_drains(j.join.jointype) || drain_hands(j.join.jointype)) && j.join.plan.lefttree.is_some_and(hash_joined) && j.join.plan.righttree.is_some_and(hash_joined))
}

/// NEW-41: a draining hash join whose probe hands its joined rows on beside the drain (RIGHT, FULL), so the probe's
/// parallel arms feed the absorbing set and the drain adds the unmatched build rows. A RIGHT SEMI / RIGHT ANTI probe
/// hands nothing on (lowerv4 drain/lower.rs `alone`): its drain alone feeds the set, spooled, the probe on one worker.
fn drain_hands(jt: types_nodes::jointype::JoinType) -> bool {
    use types_nodes::jointype::JoinType;
    matches!(jt, JoinType::JOIN_RIGHT | JoinType::JOIN_FULL)
}

/// NEW-41: whether the hash joins under `n` (as `hash_joined` admits them) hold a draining one (RIGHT, FULL).
fn drains(n: Node<'_>) -> bool {
    crate::walk::stack_check();
    if let Some(h) = n.as_hash() {
        return h.plan.lefttree.is_some_and(drains);
    }
    n.as_hash_join().is_some_and(|j| drain_hands(j.join.jointype) || j.join.plan.lefttree.is_some_and(drains) || j.join.plan.righttree.is_some_and(drains))
}

/// NEW-41 / NEW-51: PostgreSQL's estimate of the presort's rows (the joined rows) under which a draining join keeps the
/// Sort: the cost home's break-even of the Sort's form and the absorbed set's (`cost::drain_set`), fitted on dev data per
/// DISTINCT key representation (`word`: every DISTINCT argument one by-value word; else a varlena or a multi-word key) at
/// the server's width (`Facts::width`, the participants a statement of this server may run at; none off a backend takes
/// PostgreSQL's default). A stale width or estimate only picks the slower of two forms with the same answers.
fn drain_set_rows(b: &Builder<'_, '_>, word: bool) -> f64 {
    use gravity_program::cost::drain_set;
    drain_set::set_rows(word, b.facts.width().unwrap_or(drain_set::DEFAULT_WIDTH))
}

/// NEW-51: a DISTINCT argument of one by-value word (int, float, date, timestamp ...), the representation the set's
/// key holds inline; a type the catalog does not know counts as wide.
fn one_word(b: &Builder<'_, '_>, typid: u32) -> bool {
    typefacts::facts_of(b.facts, typid).is_some_and(|t| t.byval && t.words == 1)
}

/// NEW-41: a numeric sum / avg / min / max, whose packed register reads its argument's column form (`num_scale`).
fn numeric_rider(c: crate::ops::shared::agg_class::Class) -> bool {
    use crate::ops::shared::agg_class::{Class, Ext};
    matches!(c, Class::SumNum | Class::AvgNum | Class::Min(Ext::Num) | Class::Max(Ext::Num))
}

/// The stop road over a DISTINCT group's presort (facts/stream.rs `presort_stop`): one DISTINCT call or more, each folding
/// to one result in any order of its distinct values (`distinct_order_free`), every other call order-free
/// (`call_order_free`), and every call seen.
pub fn distinct_free<'p>(b: &Builder<'p, '_>, a: &'p Agg<'p>) -> bool {
    let (mut calls, mut any) = (true, false);
    let mut judge = |r: &Aggref<'p>| {
        let d = !r.aggdistinct.is_nil();
        calls &= if d { distinct_order_free(b, r) } else { call_order_free(b, r) };
        any |= d;
    };
    let seen = [&a.plan.targetlist, &a.plan.qual].iter().all(|l| l.iter().all(|n| aggrefs_in(n, &mut judge)));
    seen && calls && any
}

/// Each aggregate call in `n` to `f` (not into its arguments: a DISTINCT's clauses are no expression); false when a node
/// the walk cannot see into could hide one.
fn aggrefs_in<'p>(n: Node<'p>, f: &mut dyn FnMut(&Aggref<'p>)) -> bool {
    crate::walk::stack_check(); // D3748
    if let Some(r) = n.as_aggref() {
        f(r);
        return true;
    }
    let mut ok = true;
    crate::expr::kids(n, &mut |k| ok &= aggrefs_in(k, f)) && ok
}

/// A key the emit orders as the Sort did, whose equal values are one value (no group's printed key depends on which of
/// its rows came first): a word type catalog/word_order.rs `ordered` names (a domain by its base), or a text-like one
/// under a deterministic collation (a char(n) padded to n, so its trimmed equals are its byte equals).
pub(crate) fn same_when_equal(b: &Builder<'_, '_>, typid: u32, typmod: i32, coll: u32) -> bool {
    let Some(tf) = typefacts::facts_of(b.facts, typid) else { return false };
    let coll_ok = coll == 0 || b.facts.coll(coll).bytes_eq;
    if tf.image.is_word() {
        return coll_ok && word_order::ordered(&tf);
    }
    tf.image.text_trim().is_some_and(|bpchar_trim| coll_ok && (!bpchar_trim || typmod > 0))
}
