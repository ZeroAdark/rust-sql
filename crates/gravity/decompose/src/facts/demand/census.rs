//! C5 (c)'s checker, debug builds only. Every answer of this home that a column is unread, read only inside a window's
//! grouped entries, or read only as a key (`reads_above`, `entries_read`, and `cte::cte_cols` for a CTE's store) is
//! logged on the Builder (`log`). At the end of decompose an independent census re-reads each logged column the simple
//! way (`check`): up the plan tree, through bare hand-on entries only, no node may name it more strongly than the answer
//! said. An answer the census contradicts refuses by name (`demand: `), so the next missing column read as NULL (the
//! D3728, D3766, D3785, D3855, D3905 class) is a refusal of the debug server, never a wrong answer. A release build logs
//! and checks nothing.
//!
//! The census shares only the plan tree's types with the rest of this home: its own parent index, its own walk up and
//! its own reading of each parent (expressions through `crate::expr::kids`). What the expressions do not show, it
//! models, one premise per line:
//! - T: the statement's top, and a node no tree reaches: its non-junk columns are read (the client), its resjunk ones are
//!   not (the junk filter).
//! - S: a SubPlan's or InitPlan's root: its non-junk columns are read (the SubPlan's test and output parameters).
//! - C: a CTE's root: read as its CteScans read it (their quals; their target entries as read above them); a recursive
//!   union's root also as its WorkTableScans read it, except for the CTE's store, which only its CteScans read.
//! - M: ModifyTable reads its input: an INSERT its non-junk columns, every other command every column.
//! - L: LockRows reads every column of its input (its row marks name resjunk columns).
//! - H: setrefs' dummy target lists (Append, MergeAppend, Sort, Incremental Sort, Unique, Limit, Material, Memoize,
//!   Gather, Gather Merge, Hash, SetOp, RecursiveUnion): an entry `OUTER_VAR` c hands the input's column c on, as any
//!   bare entry of the side does.
//! - P: by position: a RecursiveUnion's right input's column c is the union's column c; a SetOp's right input is only
//!   compared (its compare columns, as keys).
//! - K: key lists read a column as a key: Sort, Incremental Sort, Gather Merge and MergeAppend sort columns, Unique,
//!   Limit WITH TIES, WindowAgg partition and order columns, Agg (its chain too) and Group grouping columns, SetOp's
//!   compare columns, RecursiveUnion's duplicate columns.
//! - W: D4076: a WindowAgg entry with a ressortgroupref and no window call, aggregate or SubPlan reads its columns as
//!   grouped (weaker than a key).
//! - X: anything else a node names reads by value: its qual, join quals, hash and merge clauses, hash keys, nest
//!   parameters, a Limit's count and offset, a window's offsets and run condition, a Result's constant qual, a Memoize's
//!   parameters, a computing target entry, a SubqueryScan's or CteScan's entries over its relation. A whole-row Var of
//!   the side (R), or an expression `crate::expr::kids` does not know, reads every column of the side; an aggregate's
//!   ORDER BY and DISTINCT clauses read nothing its arguments do not (they name its own arguments).
//! - R: a whole-row Var over a SubqueryScan's or CteScan's relation skips its input's resjunk columns (PostgreSQL's
//!   ExecInitWholeRowVar junk filter); every other whole-row Var reads each column of its side.
//! - J: a parent kind not listed above reads its input's non-junk columns (fail closed); a resjunk one only where named.
use core::cell::RefCell;

use gravity_program::{ArenaRef, ArenaVec};
use types_nodes::nodes_enums::CmdType;
use types_nodes::plannodes::{Plan, PlannedStmt};
use types_nodes::primnodes::{TargetEntry, INNER_VAR, OUTER_VAR};
use types_nodes::{Node, NodeTag};

use super::Read;
use crate::admit::Refusal;
use crate::ledger::Kind;
use crate::walk::Builder;

/// A column this home called unread that the census finds read.
pub const UNREAD: &str = "demand: a column facts::demand calls unread that a node above reads (C5)";
/// A column this home reads only inside a window's grouped entries that the census finds read as a key or by value.
pub const GROUPED: &str = "demand: a column facts::demand reads only in a window's grouped entries that a node above reads as a key or by value (C5)";
/// A column this home reads only as a key that the census finds read by value.
pub const KEY_ONLY: &str = "demand: a column facts::demand reads only as a key that a node above reads by value (C5)";

/// Which answer a logged column rests on: how the nodes above read it (`reads_above`, `entries_read`), or whether a CTE's
/// store keeps it (`cte::cte_cols`: only the CteScans read the store).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Site {
    Above,
    Store,
}

/// The logged answers (Builder::demand_log): plan node, output column, the answer, its site.
pub type Log<'p> = RefCell<ArenaVec<(&'p Plan<'p>, i16, Read, Site)>>;

/// An empty log in `arena`.
pub fn new_log<'p>(arena: ArenaRef) -> Log<'p> {
    RefCell::new(ArenaVec::new_in(arena)) // alloc: Cold
}

/// Logs answer `r` on `plan`'s column `attno` when it says less than "read by value".
pub fn log<'p>(b: &Builder<'p, '_>, plan: &'p Plan<'p>, attno: i16, r: Read, site: Site) {
    if r != Read::Value {
        b.demand_log.borrow_mut().push((plan, attno, r, site)); // alloc: Cold
    }
}

/// Re-reads every logged column (module doc); the first contradicted answer refuses.
pub fn check(b: &Builder<'_, '_>) -> Result<(), Refusal> {
    let log = b.demand_log.borrow();
    if log.is_empty() {
        return Ok(());
    }
    let c = Census::of(b.stmt, b.arena);
    for &(plan, attno, r, site) in log.iter() {
        let seen = c.read(plan, attno, site);
        let why = match r {
            Read::No if seen > Seen::No => UNREAD,
            Read::Grouped if seen > Seen::Grouped => GROUPED,
            Read::Key if seen > Seen::Key => KEY_ONLY,
            _ => continue,
        };
        return Err(Refusal::shape(Kind::Result, plan.plan_node_id, why));
    }
    Ok(())
}

/// The census's own reading of a column, strongest last.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Seen {
    No,
    Grouped,
    Key,
    Value,
}

/// How a parent holds a child: its outer (left) input, its inner (right) input, an Append / MergeAppend member, or a
/// SubqueryScan's subplan (the scan's relation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edge {
    Outer,
    Inner,
    Member,
    Sub(i32),
}

/// Where a plan node sits: under a parent, at the statement's top, or at the root of `stmt.subplans[id - 1]`.
#[derive(Clone, Copy)]
enum Up<'p> {
    Parent(Node<'p>, Edge),
    Top,
    Root(i32),
}

struct Census<'p> {
    stmt: &'p PlannedStmt<'p>,
    /// Each plan node's place (`plan address`, sorted).
    up: ArenaVec<(usize, Up<'p>)>,
    /// Every CteScan and WorkTableScan of the statement.
    scans: ArenaVec<Node<'p>>,
    arena: ArenaRef,
}

fn addr(p: &Plan<'_>) -> usize {
    p as *const Plan<'_> as usize
}

/// `p`'s target entry for output column `attno`.
fn entry<'p>(p: &'p Plan<'p>, attno: i16) -> Option<&'p TargetEntry<'p>> {
    p.targetlist.iter().filter_map(|n| n.as_target_entry()).find(|t| t.resno == attno)
}

impl<'p> Census<'p> {
    /// The parent index of every tree of `stmt` (its plan tree and each of `stmt.subplans`).
    fn of(stmt: &'p PlannedStmt<'p>, arena: ArenaRef) -> Census<'p> {
        let mut c = Census { stmt, up: ArenaVec::new_in(arena), scans: ArenaVec::new_in(arena), arena }; // alloc: Cold
        let mut work: ArenaVec<(Node<'p>, Up<'p>)> = ArenaVec::new_in(arena); // alloc: Cold
        if let Some(t) = stmt.planTree {
            work.push((t, Up::Top)); // alloc: Cold
        }
        for (i, n) in stmt.subplans.iter().enumerate() {
            if let Some(n) = n {
                work.push((n, Up::Root(i as i32 + 1))); // alloc: Cold
            }
        }
        while let Some((n, up)) = work.pop() {
            let Some(p) = n.as_plan() else { continue };
            c.up.push((addr(p), up)); // alloc: Cold
            if matches!(n.node_tag(), NodeTag::T_CteScan | NodeTag::T_WorkTableScan) {
                c.scans.push(n); // alloc: Cold
            }
            let mut kid = |k: Option<Node<'p>>, e: Edge| {
                if let Some(k) = k {
                    work.push((k, Up::Parent(n, e))); // alloc: Cold
                }
            };
            kid(p.lefttree, Edge::Outer);
            kid(p.righttree, Edge::Inner);
            if let Some(s) = n.as_subquery_scan() {
                kid(s.subplan, Edge::Sub(s.scan.scanrelid as i32));
            }
            let members = n.as_append().map(|a| &a.appendplans).or_else(|| n.as_merge_append().map(|m| &m.mergeplans));
            for m in members.into_iter().flat_map(|l| l.iter()) {
                kid(Some(m), Edge::Member);
            }
        }
        c.up.sort_unstable_by_key(|e| e.0);
        c
    }

    /// The places of `p` (one in a tree; none for a node no tree reaches).
    fn places(&self, p: &Plan<'_>) -> impl Iterator<Item = Up<'p>> + '_ {
        let a = addr(p);
        let i = self.up.partition_point(|e| e.0 < a);
        self.up[i..].iter().take_while(move |e| e.0 == a).map(|e| e.1)
    }

    /// Whether `stmt.subplans[id - 1]` is a CTE's plan (a CteScan reads it).
    fn is_cte(&self, id: i32) -> bool {
        self.scans.iter().any(|n| n.as_cte_scan().is_some_and(|s| s.ctePlanId == id))
    }

    /// How the nodes above read `plan`'s column `attno`: the strongest read over every path up (module doc).
    fn read(&self, plan: &'p Plan<'p>, attno: i16, site: Site) -> Seen {
        let mut out = Seen::No;
        let mut work: ArenaVec<(&'p Plan<'p>, i16)> = ArenaVec::new_in(self.arena); // alloc: Cold
        let mut done: ArenaVec<(usize, i16)> = ArenaVec::new_in(self.arena); // alloc: Cold
        work.push((plan, attno)); // alloc: Cold
        while let Some((p, a)) = work.pop() {
            if out == Seen::Value {
                break;
            }
            if done.contains(&(addr(p), a)) {
                continue;
            }
            done.push((addr(p), a)); // alloc: Cold
            let Some(te) = entry(p, a) else {
                out = Seen::Value; // a column the target list does not hold: fail closed
                continue;
            };
            let mut placed = false;
            for up in self.places(p) {
                placed = true;
                match up {
                    Up::Top => out = out.max(top(te)),
                    Up::Root(id) if self.is_cte(id) => self.cte(id, a, te, site, &mut work, &mut out),
                    Up::Root(_) => out = out.max(top(te)),
                    Up::Parent(q, e) => self.parent(q, e, a, te, &mut work, &mut out),
                }
            }
            if !placed {
                out = out.max(top(te));
            }
        }
        out
    }

    /// C: CTE `id`'s root, column `a`: as each CteScan of it reads its relation's column `a`; a recursive union's root
    /// also as its WorkTableScans do (not for the store).
    fn cte(&self, id: i32, a: i16, te: &TargetEntry<'p>, site: Site, work: &mut ArenaVec<(&'p Plan<'p>, i16)>, out: &mut Seen) {
        let root = (id as usize).checked_sub(1).and_then(|i| self.stmt.subplans.nth(i));
        let wt = if site == Site::Above { root.and_then(|n| n.as_recursive_union()).map(|r| r.wtParam) } else { None };
        for n in self.scans.iter().copied() {
            let scan = match (n.as_cte_scan(), n.as_work_table_scan()) {
                (Some(s), _) if s.ctePlanId == id => &s.scan,
                (_, Some(w)) if Some(w.wtParam) == wt => &w.scan,
                _ => continue,
            };
            let whole = n.as_cte_scan().is_none() || !te.resjunk; // R
            relation(&scan.plan, scan.scanrelid as i32, a, whole, work, out);
        }
    }

    /// Parent `q` (holding the column's node by `e`) reads that node's column `a` (entry `te`).
    fn parent(&self, q: Node<'p>, e: Edge, a: i16, te: &TargetEntry<'p>, work: &mut ArenaVec<(&'p Plan<'p>, i16)>, out: &mut Seen) {
        let Some(qp) = q.as_plan() else {
            *out = Seen::Value;
            return;
        };
        let tag = q.node_tag();
        // M, L
        if tag == NodeTag::T_ModifyTable {
            let insert = q.as_modify_table().is_some_and(|m| m.operation == CmdType::CMD_INSERT);
            *out = (*out).max(if insert && te.resjunk { Seen::No } else { Seen::Value });
            return;
        }
        if tag == NodeTag::T_LockRows {
            *out = Seen::Value;
            return;
        }
        // P
        if let (Some(s), Edge::Inner) = (q.as_set_op(), e) {
            if s.cmpColIdx.contains(&a) {
                *out = (*out).max(Seen::Key);
            }
            return;
        }
        if let (Some(r), Edge::Inner) = (q.as_recursive_union(), e) {
            if r.dupColIdx.contains(&a) {
                *out = (*out).max(Seen::Key);
            }
            if entry(qp, a).is_some() {
                work.push((qp, a)); // alloc: Cold
            }
            return;
        }
        // K
        if keys(q, e).any(|k| k == a) {
            *out = (*out).max(Seen::Key);
        }
        let side = match e {
            Edge::Outer | Edge::Member => OUTER_VAR,
            Edge::Inner => INNER_VAR,
            Edge::Sub(r) => r,
        };
        let whole = !(matches!(e, Edge::Sub(_)) && te.resjunk); // R
        // X: every expression the parent names besides its target list
        let mut named = false;
        each_expr(q, &mut |n| named |= names(n, side, a, whole));
        if named {
            *out = Seen::Value;
            return;
        }
        // H, W, X: the target list
        for up in qp.targetlist.iter().filter_map(|n| n.as_target_entry()) {
            match up.expr.as_var() {
                Some(v) if v.varno == side && v.varlevelsup == 0 && v.varattno == a => work.push((qp, up.resno)), // alloc: Cold
                _ if !names(up.expr, side, a, whole) => {}
                _ if tag == NodeTag::T_WindowAgg && up.ressortgroupref != 0 && grouped(up.expr) => *out = (*out).max(Seen::Grouped),
                _ => {
                    *out = Seen::Value;
                    return;
                }
            }
        }
        // J
        if !LISTED.contains(&tag) && !te.resjunk {
            *out = Seen::Value;
        }
    }
}

/// The parent kinds the census reads (module doc, H to X); any other is J.
const LISTED: &[NodeTag] = &[
    NodeTag::T_Sort, NodeTag::T_IncrementalSort, NodeTag::T_Unique, NodeTag::T_GatherMerge, NodeTag::T_Gather,
    NodeTag::T_Limit, NodeTag::T_WindowAgg, NodeTag::T_Agg, NodeTag::T_Group, NodeTag::T_Result, NodeTag::T_ProjectSet,
    NodeTag::T_NestLoop, NodeTag::T_HashJoin, NodeTag::T_MergeJoin, NodeTag::T_Hash, NodeTag::T_Material,
    NodeTag::T_Memoize, NodeTag::T_Append, NodeTag::T_MergeAppend, NodeTag::T_SetOp, NodeTag::T_RecursiveUnion,
    NodeTag::T_SubqueryScan, NodeTag::T_LockRows, NodeTag::T_ModifyTable,
];

/// T / S: a tree's top reads its non-junk columns.
fn top(te: &TargetEntry<'_>) -> Seen {
    if te.resjunk { Seen::No } else { Seen::Value }
}

/// C: a scan of a CTE or worktable (`scan`, its relation `r`) reads the relation's column `a`: by value where its qual or
/// a computing entry names it, as its own column above where a bare entry hands it on (`whole`: R).
fn relation<'p>(scan: &'p Plan<'p>, r: i32, a: i16, whole: bool, work: &mut ArenaVec<(&'p Plan<'p>, i16)>, out: &mut Seen) {
    if scan.qual.iter().any(|n| names(n, r, a, whole)) {
        *out = Seen::Value;
        return;
    }
    for t in scan.targetlist.iter().filter_map(|n| n.as_target_entry()) {
        match t.expr.as_var() {
            Some(v) if v.varno == r && v.varlevelsup == 0 && v.varattno == a => work.push((scan, t.resno)), // alloc: Cold
            _ if names(t.expr, r, a, whole) => *out = Seen::Value,
            _ => {}
        }
    }
}

/// K: the key columns parent `q` reads of the child it holds by `e`.
fn keys<'p>(q: Node<'p>, e: Edge) -> impl Iterator<Item = i16> + 'p {
    let one: &'p [i16] = match q.node_tag() {
        NodeTag::T_Sort => q.as_sort().map_or(&[][..], |s| s.sortColIdx),
        NodeTag::T_IncrementalSort => q.as_incremental_sort().map_or(&[][..], |s| s.sort.sortColIdx),
        NodeTag::T_Unique => q.as_unique().map_or(&[][..], |u| u.uniqColIdx),
        NodeTag::T_GatherMerge => q.as_gather_merge().map_or(&[][..], |g| g.sortColIdx),
        NodeTag::T_Limit => q.as_limit().map_or(&[][..], |l| l.uniqColIdx),
        NodeTag::T_Group => q.as_group().map_or(&[][..], |g| g.grpColIdx),
        NodeTag::T_Agg => q.as_agg().map_or(&[][..], |a| a.grpColIdx),
        NodeTag::T_MergeAppend if e == Edge::Member => q.as_merge_append().map_or(&[][..], |m| m.sortColIdx),
        NodeTag::T_SetOp if e == Edge::Outer => q.as_set_op().map_or(&[][..], |s| s.cmpColIdx),
        NodeTag::T_RecursiveUnion if e == Edge::Outer => q.as_recursive_union().map_or(&[][..], |r| r.dupColIdx),
        NodeTag::T_WindowAgg => q.as_window_agg().map_or(&[][..], |w| w.partColIdx),
        _ => &[][..],
    };
    let two: &'p [i16] = q.as_window_agg().map_or(&[][..], |w| w.ordColIdx);
    let chain = q.as_agg().into_iter().flat_map(|a| a.chain.iter()).filter_map(|n| n.as_agg()).flat_map(|a| a.grpColIdx.iter().copied());
    one.iter().copied().chain(two.iter().copied()).chain(chain)
}

/// X: each expression parent `q` names besides its target list.
fn each_expr<'p>(q: Node<'p>, f: &mut dyn FnMut(Node<'p>)) {
    let Some(p) = q.as_plan() else { return };
    let list = |l: &types_nodes::list::NodeList<'p>, f: &mut dyn FnMut(Node<'p>)| l.iter().for_each(|n| f(n));
    list(&p.qual, f);
    if let Some(j) = q.as_nest_loop() {
        list(&j.join.joinqual, f);
        j.nestParams.iter().for_each(|n| f(n.as_nest_loop_param().map_or(n, |p| p.paramval)));
    } else if let Some(j) = q.as_hash_join() {
        list(&j.join.joinqual, f);
        list(&j.hashclauses, f);
        list(&j.hashkeys, f);
    } else if let Some(j) = q.as_merge_join() {
        list(&j.join.joinqual, f);
        list(&j.mergeclauses, f);
    } else if let Some(h) = q.as_hash() {
        list(&h.hashkeys, f);
    } else if let Some(r) = q.as_result() {
        r.resconstantqual.into_iter().for_each(|n| f(n));
    } else if let Some(l) = q.as_limit() {
        l.limitOffset.into_iter().chain(l.limitCount).for_each(|n| f(n));
    } else if let Some(w) = q.as_window_agg() {
        w.startOffset.into_iter().chain(w.endOffset).for_each(|n| f(n));
        list(&w.runCondition, f);
    } else if let Some(m) = q.as_memoize() {
        list(&m.param_exprs, f);
    }
}

/// Whether expression `n` names column `a` of relation `side` (a whole-row Var of it names the column when `whole`: R),
/// or holds an expression `crate::expr::kids` does not know (fail closed). An aggregate's ORDER BY and DISTINCT clauses
/// name entries of its own arguments (SortGroupClauses): they read nothing the arguments do not.
fn names(n: Node<'_>, side: i32, a: i16, whole: bool) -> bool {
    crate::walk::stack_check(); // D3748
    if let Some(v) = n.as_var() {
        return v.varno == side && v.varlevelsup == 0 && (v.varattno == a || (v.varattno == 0 && whole));
    }
    if n.node_tag() == NodeTag::T_SortGroupClause {
        return false;
    }
    let mut hit = false;
    let known = crate::expr::kids(n, &mut |k| hit = hit || names(k, side, a, whole));
    hit || !known
}

/// W: an entry with no window call, aggregate or SubPlan in it.
fn grouped(n: Node<'_>) -> bool {
    crate::expr::walk(n, &mut |x| !matches!(x.node_tag(), NodeTag::T_WindowFunc | NodeTag::T_Aggref | NodeTag::T_SubPlan | NodeTag::T_AlternativeSubPlan))
}
