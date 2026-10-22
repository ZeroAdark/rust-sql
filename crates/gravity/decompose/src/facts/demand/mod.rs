//! C5 (c): column demand, one fact. `reads_above(plan, attno)`: how a node above plan node `plan` reads its output column
//! `attno` (No, only as a key over rows, or its value), computed top-down ONCE per statement from the top of the plan
//! tree and of each SubPlan / InitPlan / CTE plan, through left and right children, every Append / MergeAppend member
//! and every SubqueryScan's subplan. Every "null this resjunk
//! entry / drop this column / pin this column" decision reads it (lint RC; the local walks not yet moved are rows of
//! lint/DEMAND-DEBT), so no consumer re-derives a premise about its readers (D3728, D3766, D3785: three local walks, three
//! premises).
//!
//! The rules, per edge from a parent to its child's column `c` (a target entry of the child):
//! - The top of a tree (the statement's, a SubPlan's or InitPlan's, a CTE's): its non-junk columns are read (the client,
//!   the SubPlan's test, the CteScan); its resjunk ones are not (the junk filter drops them; a reader names visible
//!   columns only).
//! - A pass-through parent (Sort, Incremental Sort, Unique, Gather Merge, Limit, WindowAgg, Material, Gather): `c` in one
//!   of its key indexes (sortColIdx, uniqColIdx, partColIdx / ordColIdx) is read as a key; read by its qual, a Limit's
//!   count / offset, a window's offsets or run condition, or a target entry that computes over it, its value is read;
//!   a target entry that hands `c` on bare (an `OUTER_VAR`) hands on that entry's own demand above.
//! - LockRows reads every column (its row marks are resjunk entries); ModifyTable too, but an INSERT reads only the
//!   non-junk columns (its projection to the result relation drops the junk).
//! - Any other parent, an inner child, an Append / MergeAppend member and a SubqueryScan's subplan: fail closed for a
//!   non-junk column (read); a resjunk column is not read (PostgreSQL's junk design: a resjunk entry is only the top's
//!   and its target-list sharers', createplan.c `plan->targetlist = lefttree->targetlist`, and a projection of an outer
//!   query level names a subquery's visible columns only). Below such a member or subplan the rules above apply again:
//!   a subquery's own Sort reads its Agg's resjunk sort key as the statement's top Sort does (the walks this fact
//!   replaced searched left / right children only, so a UNION ALL arm's or a computed-over subquery's Agg had no parent
//!   and its sort key read as NULL).
//! - A node the walk does not reach (a CustomScan's plans) answers by the top's rule.
//! - A column of an expression the walker does not know (`expr::walk`'s false) is read (fail closed).
//!
//! The immediate parent's own reads, one table (`parent_reads`): which expressions of a join, an Agg / Group or a Result
//! name its child's columns. `entries_read` reads it for a scan's lanes (a scan PG hands its physical target list carries
//! only the entries its parent reads), ops/scan `call_reads_only` for a column read only as a call's argument; the
//! top-down `Demand` keeps such a parent's fail-closed rule (above) until its consumers read the table too.
use gravity_program::{ArenaRef, ArenaVec};
use types_nodes::list::NodeList;
use types_nodes::nodes_enums::CmdType;
use types_nodes::plannodes::{Plan, PlannedStmt};
use types_nodes::primnodes::{TargetEntry, Var, INNER_VAR, OUTER_VAR};
use types_nodes::{Node, NodeTag};

use crate::expr;
use crate::ops::shared::agg::{FinalForm, GroupDecl, GroupForm};
use crate::walk::Builder;

#[cfg(debug_assertions)]
pub mod census;
pub mod cte;
pub mod readers;

/// How the nodes above read one output column; ordered, so the strongest read of several readers is the `max`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Read {
    /// No node above reads it: its value never shows (a consumer may emit a typed NULL for it, or not carry it).
    No,
    /// D4076: read only inside a target entry a WindowAgg computes under a ressortgroupref (a GROUP BY or ORDER BY
    /// expression make_window_input_target flattened to its columns: one the planner dropped from the keys below, whose
    /// value is one per group of a grouping node under the window; the node reads it from any member of the group).
    Grouped,
    /// Read only as a key over rows (a sort, unique, partition or order key): its order and equality matter, so a node
    /// with many rows carries it; over at most one row no key compares it.
    Key,
    /// Its value is read (shown, computed over, filtered on, or handed to a reader the rules above do not know).
    Value,
}

/// The statement's column demand: each plan node's `(address of its Plan, start, width)` (sorted by address) over `reads`,
/// where `reads[start + resno - 1]` is the demand on output column `resno`.
pub struct Demand {
    at: ArenaVec<(usize, u32, u16)>,
    reads: ArenaVec<Read>,
    arena: ArenaRef,
}

/// How the nodes above plan node `plan` read its output column `attno`. A column `plan`'s target list does not hold is
/// read (fail closed). The statement's `Demand` is `Builder::demand` once `fill` ran (agg.rs `map_agg` fills it); unfilled,
/// it is computed for this answer (the same answer, more work).
pub fn reads_above<'p>(b: &Builder<'p, '_>, plan: &'p Plan<'p>, attno: i16) -> Read {
    let r = match b.demand.as_ref() {
        Some(d) => d.read(plan, attno),
        None => Demand::of(b.stmt, b.arena).read(plan, attno),
    };
    #[cfg(debug_assertions)]
    census::log(b, plan, attno, r, census::Site::Above);
    r
}

/// Builds the statement's `Demand` into `Builder::demand` once (a plain memo: the caller holds the Builder exclusively).
pub fn fill(b: &mut Builder<'_, '_>) {
    if b.demand.is_none() {
        b.demand = Some(Demand::of(b.stmt, b.arena));
    }
}

impl Demand {
    /// The demand of every node of `stmt`, top-down from each tree's top.
    pub fn of<'p>(stmt: &'p PlannedStmt<'p>, arena: ArenaRef) -> Demand {
        let mut d = Demand { at: ArenaVec::new_in(arena), reads: ArenaVec::new_in(arena), arena }; // alloc: Cold
        for root in stmt.planTree.into_iter().chain(stmt.subplans.iter().flatten()) {
            let Some(p) = root.as_plan() else { continue };
            let got = d.fallbacks(p);
            let mine = d.record(p, &got);
            d.down(root, p, mine);
        }
        d.at.sort_unstable_by_key(|e| e.0);
        d
    }

    /// The demand on `plan`'s column `attno`: the strongest over every record of `plan` (a node met twice is read as its
    /// strongest reader reads it); a node no tree reached answers by the fail-closed rule (`fallback`).
    pub fn read<'p>(&self, plan: &'p Plan<'p>, attno: i16) -> Read {
        let key = plan as *const Plan<'p> as usize;
        let Some(te) = entry(plan, attno) else { return Read::Value };
        let i = self.at.partition_point(|e| e.0 < key);
        let mut out = None;
        for &(_, start, width) in self.at[i..].iter().take_while(|e| e.0 == key) {
            let r = if attno >= 1 && (attno as u16) <= width { self.reads[start as usize + attno as usize - 1] } else { Read::Value };
            out = Some(out.map_or(r, |o: Read| o.max(r)));
        }
        out.unwrap_or_else(|| fallback(te))
    }

    /// Records `p`'s demand: `got[resno]` for each output column (index 0 unused); a resno past `got` is read.
    fn record<'p>(&mut self, p: &'p Plan<'p>, got: &[Read]) -> (u32, u16) {
        let width = width(p);
        let start = self.reads.len();
        self.reads.extend((1..=width as usize).map(|r| got.get(r).copied().unwrap_or(Read::Value))); // alloc: Cold
        self.at.push((p as *const Plan<'p> as usize, start as u32, width)); // alloc: Cold
        (start as u32, width)
    }

    /// The fail-closed rule over each of `p`'s columns, indexed by resno (`record`'s layout).
    fn fallbacks<'p>(&self, p: &'p Plan<'p>) -> ArenaVec<Read> {
        let mut out = ArenaVec::new_in(self.arena); // alloc: Cold
        out.resize(width(p) as usize + 1, Read::Value); // alloc: Cold
        for te in p.targetlist.iter().filter_map(|n| n.as_target_entry()).filter(|t| t.resno >= 1) {
            out[te.resno as usize] = fallback(te);
        }
        out
    }

    /// Each child of `n` (`p` its plan, `mine` its own demand): its demand from `n`'s rules, then its children's. The left
    /// child by `n`'s rules; the right child, every Append / MergeAppend member and a SubqueryScan's subplan by the
    /// fail-closed rule (the module doc's reach).
    fn down<'p>(&mut self, n: Node<'p>, p: &'p Plan<'p>, mine: (u32, u16)) {
        crate::walk::stack_check(); // D3748
        let members = n.as_append().map(|a| &a.appendplans).or_else(|| n.as_merge_append().map(|m| &m.mergeplans));
        let sub = n.as_subquery_scan().and_then(|s| s.subplan);
        let kids = [(p.lefttree, true), (p.righttree, false), (sub, false)].into_iter().filter_map(|(k, left)| k.map(|k| (k, left)));
        for (k, left) in kids.chain(members.into_iter().flat_map(|l| l.iter()).map(|k| (k, false))) {
            let Some(kp) = k.as_plan() else { continue };
            let got = if left { self.by_parent(n, p, mine, kp) } else { self.fallbacks(kp) };
            let theirs = self.record(kp, &got);
            self.down(k, kp, theirs);
        }
    }

    /// How parent `n` (its plan `p`, its own demand `mine`) reads each column of its left (outer) child `kp`, indexed by
    /// resno: one pass over the parent's key indexes, expressions and target list.
    fn by_parent<'p>(&self, n: Node<'p>, p: &'p Plan<'p>, mine: (u32, u16), kp: &'p Plan<'p>) -> ArenaVec<Read> {
        let mut idx: [&[i16]; 2] = [&[], &[]];
        let mut exprs: [Option<Node<'p>>; 2] = [None, None];
        let mut cond: Option<&'p NodeList<'p>> = None;
        match n.node_tag() {
            NodeTag::T_LockRows => return self.all(kp, Read::Value),
            NodeTag::T_ModifyTable => {
                return match n.as_modify_table().map(|m| m.operation) {
                    Some(CmdType::CMD_INSERT) => self.fallbacks(kp),
                    _ => self.all(kp, Read::Value),
                };
            }
            NodeTag::T_Sort => idx[0] = n.as_sort().map_or(&[][..], |s| &s.sortColIdx[..]),
            NodeTag::T_IncrementalSort => idx[0] = n.as_incremental_sort().map_or(&[][..], |s| &s.sort.sortColIdx[..]),
            NodeTag::T_Unique => idx[0] = n.as_unique().map_or(&[][..], |u| &u.uniqColIdx[..]),
            NodeTag::T_GatherMerge => idx[0] = n.as_gather_merge().map_or(&[][..], |g| &g.sortColIdx[..]),
            NodeTag::T_Limit => {
                let Some(l) = n.as_limit() else { return self.all(kp, Read::Value) };
                idx[0] = &l.uniqColIdx[..];
                exprs = [l.limitOffset, l.limitCount];
            }
            NodeTag::T_WindowAgg => {
                let Some(w) = n.as_window_agg() else { return self.all(kp, Read::Value) };
                idx = [&w.partColIdx[..], &w.ordColIdx[..]];
                exprs = [w.startOffset, w.endOffset];
                cond = Some(&w.runCondition);
            }
            NodeTag::T_Material | NodeTag::T_Gather => {}
            _ => return self.fallbacks(kp),
        }
        let width = width(kp) as usize;
        let mut out = ArenaVec::new_in(self.arena); // alloc: Cold
        out.resize(width + 1, Read::No); // alloc: Cold
        for &c in idx.iter().flat_map(|l| l.iter()) {
            if c >= 1 && (c as usize) <= width {
                out[c as usize] = Read::Key;
            }
        }
        // every column an expression reads is read by value; an expression the walker does not know reads every column
        let mut unknown = false;
        let mut mark = |e: Node<'p>, out: &mut ArenaVec<Read>| {
            unknown |= !crate::expr::walk(e, &mut |x| {
                if let Some(v) = x.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattno >= 1 && v.varattno as usize <= width) {
                    out[v.varattno as usize] = Read::Value;
                }
                true
            });
        };
        for e in exprs.into_iter().flatten().chain(cond.into_iter().flat_map(|l| l.iter())).chain(p.qual.iter()) {
            mark(e, &mut out);
        }
        // D4076: a WindowAgg's own grouping or sort entry (a ressortgroupref; no window call, aggregate or sub-plan)
        let grouped = |up: &TargetEntry<'p>| {
            n.node_tag() == NodeTag::T_WindowAgg
                && up.ressortgroupref != 0
                && crate::expr::walk(up.expr, &mut |x| !matches!(x.node_tag(), NodeTag::T_WindowFunc | NodeTag::T_Aggref | NodeTag::T_SubPlan | NodeTag::T_AlternativeSubPlan))
        };
        let mut inside: ArenaVec<usize> = ArenaVec::new_in(self.arena); // alloc: Cold
        for up in p.targetlist.iter().filter_map(|t| t.as_target_entry()) {
            match up.expr.as_var() {
                // a bare entry hands its own demand above down to the column it copies
                Some(v) if v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattno >= 1 && v.varattno as usize <= width => {
                    out[v.varattno as usize] = out[v.varattno as usize].max(self.mine(mine, up.resno));
                }
                _ if grouped(up) => {
                    crate::expr::walk(up.expr, &mut |x| {
                        if let Some(v) = x.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattno >= 1 && v.varattno as usize <= width) {
                            inside.push(v.varattno as usize); // alloc: Cold
                        }
                        true
                    });
                }
                _ => mark(up.expr, &mut out),
            }
        }
        for &c in inside.iter() {
            out[c] = out[c].max(Read::Grouped);
        }
        if unknown {
            out.iter_mut().for_each(|r| *r = Read::Value);
        }
        out
    }

    /// `r` for each of `p`'s columns, indexed by resno.
    fn all<'p>(&self, p: &'p Plan<'p>, r: Read) -> ArenaVec<Read> {
        let mut out = ArenaVec::new_in(self.arena); // alloc: Cold
        out.resize(width(p) as usize + 1, r); // alloc: Cold
        out
    }

    /// The parent's own demand on its column `resno` (`mine`); a resno past its list is read.
    fn mine(&self, (start, width): (u32, u16), resno: i16) -> Read {
        if resno >= 1 && (resno as u16) <= width { self.reads[start as usize + resno as usize - 1] } else { Read::Value }
    }
}

/// `p`'s highest output column number.
fn width(p: &Plan<'_>) -> u16 {
    p.targetlist.iter().filter_map(|n| n.as_target_entry()).map(|t| t.resno.max(0) as u16).max().unwrap_or(0)
}

/// The fail-closed rule (module doc): a non-junk column is read, a resjunk one is not.
fn fallback(te: &TargetEntry<'_>) -> Read {
    if te.resjunk { Read::No } else { Read::Value }
}

/// `plan`'s target entry for output column `attno`.
fn entry<'p>(plan: &'p Plan<'p>, attno: i16) -> Option<&'p TargetEntry<'p>> {
    plan.targetlist.iter().filter_map(|n| n.as_target_entry()).find(|t| t.resno == attno)
}

/// What plan node `plan`'s immediate parent reads of `plan`'s output: the one per-parent read table (module doc).
/// `parent`: that node (a consumer that asks how a call reads a column checks its kind); `side`: the varno the parent
/// names `plan`'s columns by (`INNER_VAR` for a join's inner, else `OUTER_VAR`); `keys`: the input columns an Agg / Group
/// groups by (1-based, as `grpColIdx`); `exprs`: every expression of the parent that names them (a join's target list,
/// quals, hash / merge clauses and nest parameters; an Agg's or Group's target list and qual; a Result's projection, qual
/// and constant qual).
pub struct ParentReads<'p> {
    pub parent: Node<'p>,
    pub side: i32,
    pub keys: ArenaVec<i16>,
    pub exprs: ArenaVec<Node<'p>>,
}

/// `plan`'s parent's reads (`ParentReads`). `None` for every other parent, which reads every entry (fail closed): the plan
/// root (the client reads the row), a sub-plan's root (D2633: a CTE's root reads what its CteScans read, facts/demand/cte.rs
/// `cte_root`), a node under an Append member, a node that stores its input's rows (Hash, Material, Memoize, Sort,
/// Unique: the store holds every entry; the planner gives their inputs a small target list anyway, CP_SMALL_TLIST), a
/// Limit (an ordered index source under one reads more than its consumer: m8_memo_scan_lanes), a parent kind this table
/// does not list (ModifyTable and LockRows want the whole row; WindowAgg, SetOp, Gather, SubqueryScan ...), and an Agg
/// with grouping sets, a chain or a Gravity rewrite.
pub fn parent_reads<'p>(b: &mut Builder<'p, '_>, plan: &'p Plan<'p>) -> Option<ParentReads<'p>> {
    let (parent, right) = readers::parent_of(b, plan)?;
    let side = if right { INNER_VAR } else { OUTER_VAR };
    let mut keys = ArenaVec::new_in(b.arena); // alloc: Cold
    let mut exprs = ArenaVec::new_in(b.arena); // alloc: Cold
    if let Some(j) = parent.as_nest_loop() {
        add(&mut exprs, &[&j.join.plan.targetlist, &j.join.plan.qual, &j.join.joinqual]);
        j.nestParams.iter().for_each(|p| exprs.push(p.as_nest_loop_param().map_or(p, |p| p.paramval)));
    } else if let Some(j) = parent.as_hash_join() {
        add(&mut exprs, &[&j.join.plan.targetlist, &j.join.plan.qual, &j.join.joinqual, &j.hashclauses, &j.hashkeys]);
    } else if let Some(j) = parent.as_merge_join() {
        add(&mut exprs, &[&j.join.plan.targetlist, &j.join.plan.qual, &j.join.joinqual, &j.mergeclauses]);
    } else if let Some(a) = parent.as_agg() {
        if !a.groupingSets.is_nil() || !a.chain.is_nil() || a.gravity_rewrite.is_some() {
            return None;
        }
        add(&mut exprs, &[&a.plan.targetlist, &a.plan.qual]);
        a.grpColIdx.iter().take(a.numCols.max(0) as usize).for_each(|&c| keys.push(c));
    } else if let Some(g) = parent.as_group() {
        add(&mut exprs, &[&g.plan.targetlist, &g.plan.qual]);
        g.grpColIdx.iter().take(g.numCols.max(0) as usize).for_each(|&c| keys.push(c));
    } else if let Some(r) = parent.as_result() {
        add(&mut exprs, &[&r.plan.targetlist, &r.plan.qual]);
        r.resconstantqual.into_iter().for_each(|n| exprs.push(n));
    } else {
        return None;
    }
    Some(ParentReads { parent, side, keys, exprs })
}

/// The output column of `plan` that the Sort right above it orders by first: the Sort's first key, through the Sort's
/// bare entry of that column (`OUTER_VAR`, relabels looked through). `None` when `plan`'s parent is no Sort, or `plan`
/// sits under an Append / MergeAppend member or is met twice (readers.rs `parent_of`). A two-pass top's extreme
/// (ops/agg `top_extreme`) is the aggregate in that column.
pub fn first_sort_key<'p>(b: &mut Builder<'p, '_>, plan: &'p Plan<'p>) -> Option<i16> {
    let (parent, false) = readers::parent_of(b, plan)? else { return None };
    let sort = parent.as_sort()?;
    let te = entry(&sort.plan, *sort.sortColIdx.first()?)?;
    let v = crate::facts::pinned::relabels(te.expr).as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0)?;
    Some(v.varattno)
}

/// A DISTINCT group `g`'s argument key `e` (resno 0, `count(DISTINCT expr)`'s) whose value no aggregate of the group
/// reads: every aggregate over it is a pair count (`PairCount` counts distinct key words; only a `PairFold` folds the
/// value), so its record needs no representative (lowerv4 table/emit_group.rs `unread_key`).
pub fn distinct_arg_unread(g: &GroupDecl<'_>, e: Node<'_>) -> bool {
    g.form == GroupForm::Distinct && g.keys.iter().any(|x| x.resno == 0 && x.expr.node().ptr_eq(e)) && !g.aggs.iter().any(|d| d.final_ == FinalForm::PairFold)
}

/// Every expression of the lists `ls`, onto `v`.
fn add<'p>(v: &mut ArenaVec<Node<'p>>, ls: &[&'p NodeList<'p>]) {
    ls.iter().for_each(|l| l.iter().for_each(|n| v.push(n)));
}

/// Per entry of `plan`'s target list (by position): whether its parent reads it (`parent_reads`); `None` = every entry
/// (fail closed: a parent the table does not list, a grouping column outside the target list, an expression
/// `expr::kids` does not know, a whole-row reference of the side). PG hands a scan under a join, an Agg or a Sort its
/// physical target list (createplan.c use_physical_tlist: every column, no projection; the executor deforms only what a
/// reader asks of the slot), so the scan's target list says nothing about what is read: a bare column entry no consumer
/// reads gets no lane; the lowerer forwards it as no cell (lowerv4 step/project.rs `Entry::ex` None), the road a
/// SeqScan's `cb_scan_cols` already takes. The root of a CTE's plan reads the entries its CteScans read (D2633,
/// facts/demand/cte.rs `cte_root`).
pub fn entries_read<'p>(b: &mut Builder<'p, '_>, plan: &'p Plan<'p>) -> Option<ArenaVec<bool>> {
    let hit = entries_of(b, plan)?;
    #[cfg(debug_assertions)]
    for (t, &r) in plan.targetlist.iter().zip(hit.iter()) {
        if let Some(t) = t.as_target_entry().filter(|_| !r) {
            census::log(b, plan, t.resno, Read::No, census::Site::Above);
        }
    }
    Some(hit)
}

/// `entries_read`'s answer.
fn entries_of<'p>(b: &mut Builder<'p, '_>, plan: &'p Plan<'p>) -> Option<ArenaVec<bool>> {
    if let Some(hit) = cte::cte_root(b, plan) {
        return Some(hit);
    }
    let pr = parent_reads(b, plan)?;
    let mut hit = ArenaVec::new_in(b.arena); // alloc: Cold
    hit.resize(plan.targetlist.len(), false);
    (cols(&pr.keys, &mut hit) && pr.exprs.iter().all(|&n| mark(n, pr.side, &mut hit))).then_some(hit)
}

/// The target entries of scan `plan` (range table entry `varno`) its consumers read, in order: every entry but a bare
/// column of `varno` no consumer reads (`entries_read`); an entry that computes, or one over another relation's or a
/// system column, stays. A scan's lanes (ops/scan `read_vars`) and a CteScan's columns (facts/demand/cte.rs `cte_read`)
/// are the `Var`s of these and of the scan's quals.
pub fn read_entries<'p>(b: &mut Builder<'p, '_>, plan: &'p Plan<'p>, varno: i32) -> ArenaVec<Node<'p>> {
    let read = entries_read(b, plan);
    let bare = |t: Node<'_>| t.as_target_entry().and_then(|t| t.expr.as_var()).is_some_and(|v| v.varno == varno && v.varlevelsup == 0 && v.varattno > 0);
    let mut out = ArenaVec::new_in(b.arena); // alloc: Cold
    for (i, t) in plan.targetlist.iter().enumerate() {
        if read.as_ref().is_none_or(|r| r.get(i).is_none_or(|&r| r)) || !bare(t) {
            out.push(t); // alloc: Cold
        }
    }
    out
}

/// Which resjunk entries of an Agg's or Group's target list `agg_reads` walks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Junk {
    /// None: the shown entries and the qual.
    Skip,
    /// The resjunk entries that compute too (any but a bare column), after the qual: D345 / D3728, PostgreSQL's Agg
    /// projects them over the group's row (a Sort key, `ORDER BY 1 / (id - 1)` under `GROUP BY id`); a bare resjunk
    /// column reaches no client (the junk filter drops it) and a node above reads it only as a sort or unique key.
    Computing,
    /// Every entry in target-list order, then the qual.
    All,
}

/// An Agg's or Group's own reads of its input row outside its aggregates: each `OUTER_VAR` `Var` of this level in its
/// target list `tlist` (the resjunk entries `junk` names) and its qual `qual`, as `f` meets them (repeats too), never
/// inside an aggregate (its arguments are the aggregate's). The one walk behind a group's kept columns (`pinned`:
/// a keyless group keeps them from its first row, `Junk::Computing`), the columns the keys must determine (ops/agg
/// `pinned_vars`) and a sorted DISTINCT's shown member (ops/group absorb.rs `pin_shows_member`).
pub fn agg_reads<'p>(tlist: &NodeList<'p>, qual: &NodeList<'p>, junk: Junk, f: &mut dyn FnMut(&'p Var<'p>)) {
    let is_junk = |n: &Node<'p>| n.as_target_entry().is_some_and(|t| t.resjunk);
    let bare = |n: &Node<'p>| n.as_target_entry().is_some_and(|t| t.expr.as_var().is_some());
    let mut read = |n: Node<'p>| {
        expr::walk(n, &mut |x| {
            if let Some(v) = x.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0) {
                f(v);
            }
            x.as_aggref().is_none()
        });
    };
    match junk {
        Junk::All => tlist.iter().chain(qual.iter()).for_each(&mut read),
        Junk::Skip => tlist.iter().filter(|n| !is_junk(n)).chain(qual.iter()).for_each(&mut read),
        Junk::Computing => {
            tlist.iter().filter(|n| !is_junk(n)).chain(qual.iter()).for_each(&mut read);
            tlist.iter().filter(|n| is_junk(n) && !bare(n)).for_each(&mut read);
        }
    }
}

/// The input columns (`OUTER_VAR`) a keyless group keeps from its first row: `agg_reads` under `Junk::Computing`, each
/// column once, in first-seen order. The one list behind decompose's `GroupDecl::pinned` (ops/agg `pinned_cols`, by
/// number) and exec's agg.arena kept slots (by type: slot `naggs + j` is the `j`-th column `f` gets).
pub fn pinned<'p>(tlist: &NodeList<'p>, qual: &NodeList<'p>, f: &mut dyn FnMut(&'p Var<'p>)) {
    // an OUTER_VAR's number indexes the child's target list, at most MaxTupleAttributeNumber entries; one outside that
    // range (none the planner makes) is matched against the reads met before it
    const MAX: usize = gravity_abi::limits::MAX_TUPLE_ATTRIBUTE_NUMBER;
    let mut seen = [0u64; MAX / 64 + 1];
    let mut at = 0usize;
    agg_reads(tlist, qual, Junk::Computing, &mut |v| {
        let a = v.varattno;
        let new = match usize::try_from(a).ok().filter(|i| (1..=MAX).contains(i)) {
            Some(i) => {
                let (w, bit) = (i / 64, 1u64 << (i % 64));
                let new = seen[w] & bit == 0;
                seen[w] |= bit;
                new
            }
            None => {
                let (mut j, mut met) = (0usize, false);
                agg_reads(tlist, qual, Junk::Computing, &mut |u| {
                    met |= j < at && u.varattno == a;
                    j += 1;
                });
                !met
            }
        };
        at += 1;
        if new {
            f(v);
        }
    });
}

/// lowerv4 jsonb_fwd.rs `lane_read` (D3023 LATE, a forwarded jsonb column the host fills only where a path lane is
/// invalid): whether group `g` reads input column `r` (each `OUTER_VAR` read of this level in its aggregates' FILTERs and
/// nodes, arguments included, then in its computed keys, `resno == 0`) only under nodes `lane` takes, one per read: the
/// reads are as many as the nodes `lane` takes, and not none. Every node met passes `open`; a node the walk does not know
/// answers false.
pub fn group_reads_only_under(g: &GroupDecl<'_>, r: i16, open: &dyn Fn(Node<'_>) -> bool, lane: &dyn Fn(Node<'_>) -> bool) -> bool {
    let (mut vars, mut lanes, mut ok, mut opened) = (0u32, 0u32, true, true);
    let keys = g.keys.iter().filter(|x| x.resno == 0).map(|x| x.expr.node());
    for n in g.aggs.iter().flat_map(|a| a.filter.iter().copied().chain([a.node])).chain(keys) {
        ok &= expr::walk(n, &mut |x| {
            opened &= open(x);
            vars += x.as_var().is_some_and(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattno == r) as u32;
            lanes += lane(x) as u32;
            true
        });
    }
    ok && opened && vars > 0 && vars == lanes
}

/// Mark each input column `idx` names (1-based); false for one outside the target list.
fn cols(idx: &[i16], read: &mut [bool]) -> bool {
    idx.iter().all(|&c| match usize::try_from(c - 1).ok().and_then(|i| read.get_mut(i)) {
        Some(r) => {
            *r = true;
            true
        }
        None => false,
    })
}

/// Mark the input columns (`varno` = the side's) expression `n` reads; false when it holds a kind `expr::kids` does not
/// know or reads a column outside the target list (a whole-row reference of the side: every entry). An aggregate's
/// ORDER BY and DISTINCT clauses (`aggorder`, `aggdistinct`) are SortGroupClauses that name entries of its own `args` by
/// `tleSortGroupRef` (parse_agg.c transformAggregateCall): they read no column the arguments do not, so only the
/// arguments, the direct arguments and the FILTER are searched.
fn mark<'p>(n: Node<'p>, varno: i32, read: &mut [bool]) -> bool {
    crate::walk::stack_check(); // D3748
    if let Some(v) = n.as_var() {
        if v.varno != varno || v.varlevelsup != 0 {
            return true;
        }
        return match usize::try_from(v.varattno as i32 - 1).ok().and_then(|i| read.get_mut(i)) {
            Some(r) => {
                *r = true;
                true
            }
            None => false,
        };
    }
    if let Some(a) = n.as_aggref() {
        let clauses = a.aggorder.iter().chain(a.aggdistinct.iter()).all(|c| c.as_sort_group_clause().is_some());
        return clauses && a.aggdirectargs.iter().chain(a.args.iter()).chain(a.aggfilter).all(|k| mark(k, varno, read));
    }
    let mut ok = true;
    let known = expr::kids(n, &mut |k| ok = ok && mark(k, varno, read));
    known && ok
}
