//! The row bounds of an index walk (D583 / D588 / D590), read from the plan at admission: the walk's constant Limit
//! (`limit_budget`, `SiteRecipe::row_budget`) and its run bound (`run_bound`: a parameter Limit's words and whether every
//! node up to the statement's root hands up one row per row, so a cursor's FETCH count bounds the walk). PostgreSQL's
//! Limit and a suspended portal read no row past them; a row a window reads ahead is a heap visit and a SIREAD lock
//! PostgreSQL never takes. hosts scan's index_window.rs re-exports these (exec's index arms call them there).
use gravity_decompose::ops::shared::agg::{AGG_HASHED, AGG_PLAIN};
use types_nodes::node_tree::Node;
use types_nodes::plannodes::PlannedStmt;

/// D583: how many rows an index walk at plan node `id` may read before a window may run ahead of its consumer
/// (`SiteRecipe::row_budget`, set by exec's M1 / M2 index arms). PostgreSQL's Limit never pulls a row past OFFSET + LIMIT,
/// so a row read ahead is a heap visit and a SIREAD lock it never takes. From the node up: a Limit with constant counts
/// bounds the walk by OFFSET + LIMIT when every node between hands up at most one row per row of the walk (PostgreSQL
/// then reads at least that many); a Sort, a Hash or a plain / hashed Agg between reads the whole input, and any other
/// consumer (a join, a parameter count, a subplan's caller) leaves the windows as they were (`None`). A single-match nest
/// loop's index inner reads one row per window (`Some(0)`, `single_match_inner`).
pub fn limit_budget<'p>(pstmt: &PlannedStmt<'p>, id: i32) -> Option<u64> {
    let (up, _) = path_up(pstmt, id)?;
    if single_match_inner(&up) {
        return Some(0);
    }
    let (mut budget, mut one_per_row) = (None, true);
    // bounded("other: one plan node per pass on the path from the walk up to the plan's root, at plan time; the plan's depth bounds them")
    for n in up.iter().rev().skip(1) {
        if breaks(*n) {
            return None;
        }
        if let Some(l) = n.as_limit() {
            use gravity_decompose::Bound;
            let rows = match (gravity_decompose::ops::bound(l.limitCount), gravity_decompose::ops::bound(l.limitOffset)) {
                (Bound::Const(c), Bound::All) if one_per_row => c.max(0) as u64,
                (Bound::Const(c), Bound::Const(o)) if one_per_row => c.max(0) as u64 + o.max(0) as u64,
                _ => continue,
            };
            budget = Some(budget.map_or(rows, |b: u64| b.min(rows)));
        } else if !row_for_row(*n) {
            one_per_row = false;
        }
    }
    budget
}

/// window_core-0172 (perf): the walk `up` ends at (its path from the root) is an index scan that is the inner of a nest
/// loop that stops at its first match (a SEMI or ANTI join, or an inner_unique one): ExecNestLoop pulls the inner one row
/// at a time and asks no row past the first that passes the scan's filter and the join's quals, so a doubling window
/// fetches heap rows (and takes SIREAD locks) PostgreSQL never does: 10,113 rows for its 7,473 in 0172 (303 seeks of
/// tb_k, the first g = 1 row about 25th). An index-only inner keeps its windows (a row the visibility map vouches for
/// costs less than a window).
fn single_match_inner(up: &[Node<'_>]) -> bool {
    use types_nodes::jointype::JoinType;
    let (Some(walk), Some(parent)) = (up.last(), up.len().checked_sub(2).map(|i| up[i])) else { return false };
    let (Some(scan), Some(nl)) = (walk.as_index_scan(), parent.as_nest_loop()) else { return false };
    let j = &nl.join;
    let inner = j.plan.righttree.and_then(|r| r.as_plan()).is_some_and(|r| r.plan_node_id == scan.scan.plan.plan_node_id);
    inner && (j.inner_unique || matches!(j.jointype, JoinType::JOIN_SEMI | JoinType::JOIN_ANTI))
}

/// A node that reads its whole input before its first row (a Sort, a Hash, a plain or hashed Agg): no bound passes it.
fn breaks(n: Node<'_>) -> bool {
    n.as_sort().is_some() || n.as_hash().is_some() || n.as_agg().is_some_and(|a| a.aggstrategy == AGG_PLAIN || a.aggstrategy == AGG_HASHED)
}

/// A node that hands up at most one row per row of its input (`limit_budget`'s one-row-per-row path).
fn row_for_row(n: Node<'_>) -> bool {
    n.as_result().is_some() || n.as_subquery_scan().is_some() || n.as_unique().is_some() || n.as_material().is_some() || n.as_incremental_sort().is_some() || n.as_window_agg().is_some() || n.as_gather().is_some() || n.as_gather_merge().is_some() || n.as_append().is_some() || n.as_merge_append().is_some()
}

/// The plan nodes from the root down to node `id` (its own last) and whether the root is the statement's plan tree (not a
/// subplan's); `None` when no tree holds it.
fn path_up<'p>(pstmt: &PlannedStmt<'p>, id: i32) -> Option<(Vec<Node<'p>>, bool)> {
    fn path<'p>(n: Option<Node<'p>>, id: i32, out: &mut Vec<Node<'p>>) -> bool {
        stack_depth_core::check_stack_depth_or_panic();
        let Some(n) = n else { return false };
        let Some(p) = n.as_plan() else { return false };
        out.push(n);
        let kids = [n.as_append().map(|a| &a.appendplans), n.as_merge_append().map(|m| &m.mergeplans)];
        if p.plan_node_id == id || path(p.lefttree, id, out) || path(p.righttree, id, out) || kids.into_iter().flatten().any(|l| l.iter().any(|c| path(Some(c), id, out))) || n.as_subquery_scan().is_some_and(|s| path(s.subplan, id, out)) {
            return true;
        }
        out.pop();
        false
    }
    let mut up = Vec::new(); // alloc: Program
    if path(pstmt.planTree, id, &mut up) {
        return Some((up, true));
    }
    pstmt.subplans.iter().any(|s| path(s, id, &mut up)).then_some((up, false))
}

/// D590: a LIMIT / OFFSET word a walk's run bound reads at each execution: absent (`All`: no LIMIT, LIMIT ALL / NULL, no
/// OFFSET), a constant, or an extern parameter (`$n`, an int8 / int4 / int2 value: a bare Param or one under its int8 cast).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitWord {
    All,
    Const(i64),
    Param { id: u32, width: u8 },
}

impl LimitWord {
    fn of(n: Option<Node<'_>>) -> Option<LimitWord> {
        use gravity_decompose::Bound;
        const INT8OID: u32 = 20;
        const INT4OID: u32 = 23;
        const INT2OID: u32 = 21;
        match gravity_decompose::ops::bound(n) {
            Bound::All => Some(LimitWord::All),
            Bound::Const(c) => Some(LimitWord::Const(c)),
            Bound::Expr(e) => {
                let e = e.as_func_expr().filter(|f| types_core::quiet::int_widen(f.funcid) && f.args.len() == 1).map_or(e, |f| f.args.nth(0));
                let p = e.as_param().filter(|p| p.paramkind == types_nodes::primnodes::ParamKind::PARAM_EXTERN && p.paramid >= 1)?;
                let width = match p.paramtype {
                    INT8OID => 8,
                    INT4OID => 4,
                    INT2OID => 2,
                    _ => return None,
                };
                Some(LimitWord::Param { id: p.paramid as u32, width })
            }
        }
    }

    /// The word's count this execution (`param(id)`: the extern parameter `$id`'s value word and null flag); `None` = no bound
    /// (All, a null parameter).
    pub fn value(self, param: impl Fn(u32) -> Option<(u64, bool)>) -> Option<i64> {
        match self {
            LimitWord::All => None,
            LimitWord::Const(c) => Some(c),
            LimitWord::Param { id, width } => {
                let (v, null) = param(id)?;
                (!null).then_some(match width {
                    8 => v as i64,
                    4 => v as u32 as i32 as i64,
                    _ => v as u16 as i16 as i64,
                })
            }
        }
    }
}

/// D588 / D590: what bounds an index walk's rows at each execution, beyond `limit_budget`'s constant Limit: a parameter
/// Limit over a one-row-per-row path (its count and offset, read from the execution's params: a generic plan's `LIMIT $1`),
/// and whether every node from the walk to the statement's root hands up at most one row per row of the walk (no join, no
/// breaker, no Gather), so a cursor's FETCH count bounds the walk. PostgreSQL reads no row past either.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunBound {
    pub limit: Option<(LimitWord, LimitWord)>,
    pub root: bool,
}

impl RunBound {
    pub fn any(&self) -> bool {
        self.limit.is_some() || self.root
    }
}

/// The run bound of the walk at plan node `id` (`RunBound`).
pub fn run_bound<'p>(pstmt: &PlannedStmt<'p>, id: i32) -> RunBound {
    let Some((up, top)) = path_up(pstmt, id) else { return RunBound::default() };
    let (mut limit, mut one_per_row) = (None, true);
    // bounded("other: one plan node per pass on the path from the walk up to the plan's root, at plan time; the plan's depth bounds them")
    for n in up.iter().rev().skip(1) {
        if breaks(*n) {
            return RunBound::default();
        }
        if let Some(l) = n.as_limit() {
            if one_per_row && limit.is_none() {
                if let (Some(c), Some(o)) = (LimitWord::of(l.limitCount), LimitWord::of(l.limitOffset)) {
                    if matches!(c, LimitWord::Param { .. }) || matches!(o, LimitWord::Param { .. }) {
                        limit = Some((c, o));
                    }
                }
            }
        } else if !row_for_row(*n) || n.as_gather().is_some() || n.as_gather_merge().is_some() {
            one_per_row = false;
        }
    }
    RunBound { limit, root: top && one_per_row }
}
