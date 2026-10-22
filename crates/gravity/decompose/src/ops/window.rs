use core::fmt::{self, Write};

use gravity_abi::breaker::{BreakerKind, Budget, Spill};
use gravity_program::ArenaVec;
use types_nodes::list::NodeList;
use types_nodes::plannodes::WindowAgg;
use types_nodes::primnodes::{ParamKind, WindowFunc, OUTER_VAR};
use types_nodes::rawnodes::*;
use types_nodes::Node;

use crate::expr;
use crate::facts::pinned::read_decl;
use crate::graph::{Op, Source};
use crate::ledger::Kind;
use crate::ops;
use crate::admit::Refusal;
use crate::graph::{Col, Order, OrderKey};
use crate::ops::shared::sort::{column_kind, dump_eq_keys};
use crate::ops::shared::sort::{keys_layout, EqKey};
use crate::walk::{Builder, Open};
use crate::ops::shared::window::{Exclude, FrameBound, FrameCursor, FrameMode, CURSOR_FRAME, CURSOR_PARTITION, CURSOR_PEERS, CURSOR_ROW};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WinFnKind {
    RowNumber,
    Rank,
    DenseRank,
    PercentRank,
    CumeDist,
    Ntile,
    Lag,
    Lead,
    FirstValue,
    LastValue,
    NthValue,
    Aggregate,
    Other,
}

pub struct WinFn<'p> {
    pub resno: i16,
    pub node: Node<'p>,
    pub func: &'p WindowFunc<'p>,
    pub kind: WinFnKind,
    pub cursor: FrameCursor<'p>,
}

pub struct WindowDesc<'p> {
    pub winref: u32,
    pub partition: ArenaVec<EqKey>,
    pub order: ArenaVec<EqKey>,
    pub frame: FrameCursor<'p>,
    /// The window functions in ExecInitWindowAgg's order, `equal` non-VOLATILE ones once (`window_funcs`; D1354: each
    /// VOLATILE occurrence its own, told apart by node identity): function `i`'s value is
    /// column `natts + 1 + i` of the row the window op hands on, which the rewritten qual, run condition and target
    /// list read as `OUTER_VAR` (V4R-C).
    pub functions: ArenaVec<WinFn<'p>>,
    /// The child's columns (the input row's width).
    pub natts: u16,
    /// The run condition over the window's row (its `Var(0, resno)` of a window function entry rewritten as above; the
    /// varno is the plan's rtoffset under a subquery, see [`rewrite`]).
    pub run_condition: &'p NodeList<'p>,
    pub top: bool,
    /// The plan's own target list (its window functions' arguments included).
    pub tlist: &'p NodeList<'p>,
    pub plan: &'p WindowAgg<'p>,
    /// Some function's argument or FILTER calls a VOLATILE function (unknown = volatile) or user code that may write
    /// (`effects::node_may_write`, D4316: a STABLE PL/pgSQL function may call a VOLATILE writer): PostgreSQL's frame reads
    /// evaluate it, never the fill.
    pub volatile_args: bool,
    /// The rewritten target list (PG's projection, run before the run condition and the qual) calls a VOLATILE function.
    pub volatile_proj: bool,
    /// D1486: PostgreSQL's WindowAgg reads its whole input before its first row, raising nothing between two input rows
    /// but their arguments (`facts::window::reads_whole_in`, an input of one partition whose rows are all peers where its
    /// keys are one value in every row: `map`'s `fixed`). lowerv4 fills a union straight below it whole first.
    pub whole: bool,
    /// wlim-w02: every arm of the union streamed straight below this top window carries its row cap (`cap_arms`; no arm
    /// holds another Limit), so lowerv4 counts the cap across the union's feeds in arm order (rerun/union_arms.rs
    /// `cap_union`). False where some arm took none (a capped arm then counts its own rows).
    pub arms_capped: bool,
    /// D1486 rule C: with `arms_capped`, the cap (`first_rows`) when it is exactly the count of input rows PostgreSQL's top
    /// WindowAgg reads under its LIMIT or its row_number() run condition, not a bound of it; lowerv4 rerun/union_arms.rs
    /// `capped` then admits arms that can raise, PostgreSQL-equal when the statement runs to its LIMIT or its end (its
    /// program is served only to a reader that takes every row: D1922's `whole_read`).
    pub capped: Option<i64>,
}

/// The function emitter's hook: called once per window function with the desc and the function's
/// cursor; the body it emits reads the cursor's positions, never an fmgr call (ruling addendum).
pub trait FrameEmitter<'p> {
    fn function(&mut self, w: &WindowDesc<'p>, f: &WinFn<'p>);
}

impl<'p> WindowDesc<'p> {
    pub fn emit(&self, e: &mut dyn FrameEmitter<'p>) {
        for f in self.functions.iter() {
            e.function(self, f);
        }
    }
}

pub const fn fn_kind(winfnoid: u32, winagg: bool) -> WinFnKind {
    match winfnoid {
        3100 => WinFnKind::RowNumber,
        3101 => WinFnKind::Rank,
        3102 => WinFnKind::DenseRank,
        3103 => WinFnKind::PercentRank,
        3104 => WinFnKind::CumeDist,
        3105 => WinFnKind::Ntile,
        3106..=3108 => WinFnKind::Lag,
        3109..=3111 => WinFnKind::Lead,
        3112 => WinFnKind::FirstValue,
        3113 => WinFnKind::LastValue,
        3114 => WinFnKind::NthValue,
        _ if winagg => WinFnKind::Aggregate,
        _ => WinFnKind::Other,
    }
}

/// D1415: argument `a` of `f` holds a SubPlan and the object reads it only at the current row, as int4 (PG
/// `WinGetFuncArgCurrent`: lead / lag's offset, nth_value's n, ntile's buckets): the window step evaluates it over the
/// stored row (lowerv4 window/lower.rs), so its SubPlan's op goes before the window op (subplan.rs `attach`). Any other
/// argument's or FILTER's SubPlan runs on the partition buffer's fill (`map`), or refuses by name in lowerv4.
pub fn step_arg(f: &WindowFunc<'_>, a: usize) -> bool {
    let at = matches!((fn_kind(f.winfnoid, f.winagg), a), (WinFnKind::Lead | WinFnKind::Lag | WinFnKind::NthValue, 1) | (WinFnKind::Ntile, 0));
    at && f.args.iter().nth(a).is_some_and(|n| expr::has_subplan(n) && crate::catalog::expr_type::type_of(n) == Some(23))
}

/// `numeric`'s pg_type OID.
const NUMERICOID: u32 = 1700;

/// D3584: argument `a` of `f` (of a by-reference type unless `byval`; `base` its type, a domain's base) may be left to the window object, which then
/// evaluates it at each read as PG's WinGetFuncArg* / advance_windowaggregate evaluate it over the stored input row: it
/// computes a by-reference value other than a numeric (not a column, a constant or a parameter, through a RelabelType),
/// holds no SubPlan and is no `step_arg`. The fill still evaluates it; the object's store keeps a wide value (hosts window store) as a cell
/// the object evaluates again, instead of a copy per input row kept for the whole input (first_value(repeat(pad, 80))
/// over a million rows held 16 GB, where PG's spool keeps the input row). lowerv4's window lowering and exec's window
/// prepare both read this rule (the object builds an evaluator for exactly these cells).
pub fn deferred_arg(f: &WindowFunc<'_>, a: usize, base: u32, byval: bool) -> bool {
    let Some(e) = f.args.iter().nth(a) else { return false };
    // numeric's precision bounds its value (a few KB at most): its windows keep their stored copies and their strategies
    if byval || base == NUMERICOID || step_arg(f, a) || expr::has_subplan(e) {
        return false;
    }
    let mut x = e;
    while let Some(r) = x.as_relabel_type() {
        x = r.arg;
    }
    x.as_var().is_none() && x.as_const().is_none() && x.as_param().is_none()
}

const fn needs_of(kind: WinFnKind, mode: FrameMode) -> u8 {
    match kind {
        WinFnKind::RowNumber | WinFnKind::Ntile | WinFnKind::Lag | WinFnKind::Lead => CURSOR_ROW | CURSOR_PARTITION,
        WinFnKind::Rank | WinFnKind::DenseRank | WinFnKind::PercentRank | WinFnKind::CumeDist => CURSOR_ROW | CURSOR_PARTITION | CURSOR_PEERS,
        WinFnKind::FirstValue | WinFnKind::LastValue | WinFnKind::NthValue => CURSOR_FRAME,
        WinFnKind::Aggregate => if matches!(mode, FrameMode::Rows) { CURSOR_FRAME } else { CURSOR_FRAME | CURSOR_PEERS },
        WinFnKind::Other => CURSOR_ROW | CURSOR_PARTITION | CURSOR_PEERS | CURSOR_FRAME,
    }
}

fn frame_of<'p>(w: &'p WindowAgg<'p>, k: Kind, id: i32) -> Result<FrameCursor<'p>, Refusal> {
    let o = w.frameOptions;
    let mode = if o & FRAMEOPTION_ROWS != 0 { FrameMode::Rows } else if o & FRAMEOPTION_GROUPS != 0 { FrameMode::Groups } else { FrameMode::Range };
    let bad = Refusal::shape(k, id, "a frame offset without its expression");
    let start = if o & FRAMEOPTION_START_UNBOUNDED_PRECEDING != 0 {
        FrameBound::UnboundedPreceding
    } else if o & FRAMEOPTION_START_UNBOUNDED_FOLLOWING != 0 {
        FrameBound::UnboundedFollowing
    } else if o & FRAMEOPTION_START_OFFSET_PRECEDING != 0 {
        FrameBound::Preceding(w.startOffset.ok_or(bad)?)
    } else if o & FRAMEOPTION_START_OFFSET_FOLLOWING != 0 {
        FrameBound::Following(w.startOffset.ok_or(bad)?)
    } else {
        FrameBound::CurrentRow
    };
    let end = if o & FRAMEOPTION_END_UNBOUNDED_FOLLOWING != 0 {
        FrameBound::UnboundedFollowing
    } else if o & FRAMEOPTION_END_UNBOUNDED_PRECEDING != 0 {
        FrameBound::UnboundedPreceding
    } else if o & FRAMEOPTION_END_OFFSET_PRECEDING != 0 {
        FrameBound::Preceding(w.endOffset.ok_or(bad)?)
    } else if o & FRAMEOPTION_END_OFFSET_FOLLOWING != 0 {
        FrameBound::Following(w.endOffset.ok_or(bad)?)
    } else {
        FrameBound::CurrentRow
    };
    let exclude = if o & FRAMEOPTION_EXCLUDE_CURRENT_ROW != 0 {
        Exclude::CurrentRow
    } else if o & FRAMEOPTION_EXCLUDE_GROUP != 0 {
        Exclude::Group
    } else if o & FRAMEOPTION_EXCLUDE_TIES != 0 {
        Exclude::Ties
    } else {
        Exclude::None
    };
    Ok(FrameCursor { mode, start, end, exclude, needs: 0 })
}

/// A key column `c` of input `open` that holds one value in every row of one pass: an external parameter, or an
/// InitPlan's output parameter (`ORDER BY (SELECT NULL)`: PostgreSQL runs the InitPlan once, at its first read, and its
/// `setParam` holds until a rescan of the node it is attached to, which reads the window's input again), in `open`'s
/// last projection or handed on unchanged by a WindowAgg below (PG 18 stacks one WindowAgg per window clause, the lower
/// one's columns pass through). One registered in `graph.init_plans` only: an InitPlan mapped before this node
/// (attached here or below); one attached above is not known yet, and the key keeps its refusal.
fn fixed_key<'p>(b: &Builder<'p, '_>, open: &Open<'p>, at: usize, c: i16) -> bool {
    crate::walk::stack_check(); // D3748
    let entry = crate::ops::shared::order::projection(open).and_then(|l| (c as usize).checked_sub(1).filter(|&i| i < l.len()).map(|i| l.nth(i))).and_then(|e| e.as_target_entry());
    let fixed = entry.filter(|t| t.resno == c).and_then(|t| t.expr.as_param()).is_some_and(|p| match p.paramkind {
        ParamKind::PARAM_EXTERN => true,
        ParamKind::PARAM_EXEC => b.graph.init_plans.iter().any(|i| i.params.iter().any(|id| id == p.paramid)),
        _ => false,
    });
    fixed || under_window(b, open, at, c).is_some_and(|(p, at, c)| fixed_key(b, p, at, c))
}

/// A key column `c` of pipeline `open` (index `at`, as `facts::pinned::read_decl`) that holds one image in every row:
/// `facts::pinned::pinned_word_at` (a scan filter, an index clause or a bitmap recheck `column = Const`, D345 / G11) over
/// `open`, or over the fill of a WindowAgg `open` reads (`under_window`: PG 18 stacks one WindowAgg per window clause,
/// and a window hands its input columns on unchanged). `eq`: the key's own equality (operator, collation), which pins
/// too (`facts::pinned::pinned_peer_at`: every row a peer under the window's compare, window_core-0281); none for a
/// column of the input's order that is no key.
fn pinned_key<'p>(b: &Builder<'p, '_>, open: &Open<'p>, at: usize, c: i16, eq: Option<(u32, u32)>) -> bool {
    crate::walk::stack_check(); // D3748
    let here = match eq {
        Some(eq) => crate::facts::pinned::pinned_peer_at(b, open, at, c, eq),
        None => crate::facts::pinned::pinned_word_at(b, open, at, c),
    };
    here || under_window(b, open, at, c).or_else(|| under_sort(b, open, at, c)).is_some_and(|(p, at, c)| pinned_key(b, p, at, c, eq))
}

/// When pipeline `open` (index `at`, as `read_decl`) only filters and projects the rows of a full Sort with one producer
/// whose pipeline reads a WindowAgg's buffer (PG 18 sorts between two stacked WindowAggs whose orders differ): that Sort's
/// pipeline, its index and the column `open`'s column `c` reads there (`under_window` then walks it into the lower
/// window's fill). A Sort moves rows and never changes a value, so a column pinned below it is pinned above it (rnd-1236:
/// `em_id = 42` under `ORDER BY em_hired, em_id`, then a Sort, then `ORDER BY em_sal, em_id`).
fn under_sort<'a, 'p>(b: &'a Builder<'p, '_>, open: &Open<'p>, at: usize, c: i16) -> Option<(&'a Open<'p>, usize, i16)> {
    let Source::Breaker { slot } = open.source else { return None };
    let d = read_decl(b, slot, at).filter(|d| d.breaker.kind == BreakerKind::Sort)?;
    let p = b.graph.pipelines.get(d.producer as usize).filter(|p| matches!(p.ops.last(), Some(Op::Sort(_))))?; // a full sort (a top is TopK)
    if !matches!(p.source, Source::Breaker { .. }) {
        return None;
    }
    let mut resno = c;
    for op in open.ops.iter().rev() {
        match op {
            Op::Project(l) => {
                let t = l.nth((resno as usize).checked_sub(1).filter(|&n| n < l.len())?).as_target_entry().filter(|t| t.resno == resno)?;
                resno = t.expr.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0)?.varattno;
            }
            Op::Filter(_) => {}
            _ => return None,
        }
    }
    Some((p, d.producer as usize, resno))
}

/// When pipeline `open` (index `at`, as `read_decl`) reads a WindowAgg's buffer (its window op, then filters and
/// projections), the window's fill pipeline, its index and the fill's column that `open`'s column `c` hands on; none for
/// a window function's column or any other op. The index only falls, so a walk down these ends.
fn under_window<'a, 'p>(b: &'a Builder<'p, '_>, open: &Open<'p>, at: usize, c: i16) -> Option<(&'a Open<'p>, usize, i16)> {
    let Source::Breaker { slot } = open.source else { return None };
    let d = read_decl(b, slot, at).filter(|d| d.breaker.kind == BreakerKind::Materialize)?;
    let mut resno = c;
    for op in open.ops.iter().rev() {
        match op {
            Op::Project(l) => {
                let t = l.nth((resno as usize).checked_sub(1).filter(|&n| n < l.len())?).as_target_entry().filter(|t| t.resno == resno)?;
                resno = t.expr.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0)?.varattno;
            }
            // a Sort's pipeline (`under_sort`): its row is the column list of its target list
            Op::Sort(s) => {
                let t = s.tlist.nth((resno as usize).checked_sub(1).filter(|&n| n < s.tlist.len())?).as_target_entry().filter(|t| t.resno == resno)?;
                resno = t.expr.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0)?.varattno;
            }
            Op::Filter(_) => {}
            Op::Window(w) if resno >= 1 && resno as usize <= w.natts as usize => return b.graph.pipelines.get(d.producer as usize).map(|p| (p, d.producer as usize, resno)),
            _ => return None,
        }
    }
    None
}

/// What column `c` of pipeline `open` reads, past the projections, sorts and windows that hand a column on and the
/// relabels that keep its bytes (a RelabelType: a collation's or a binary-compatible type's; `ORDER BY t COLLATE "C"`
/// beside `ORDER BY t` sorts one relabelled copy of `t`), into the producer of a breaker it reads (one producer, no
/// union's arms; `at`: `open`'s index, as `read_decl`, falling at each step so the walk ends): the expression that
/// computes it, or none past any other op.
fn source_of<'p>(b: &Builder<'p, '_>, open: &Open<'p>, at: usize, c: i16) -> Option<Node<'p>> {
    crate::walk::stack_check(); // D3748
    let nth = |l: &'p NodeList<'p>, c: i16| l.nth((c as usize).checked_sub(1).filter(|&n| n < l.len())?).as_target_entry().filter(|t| t.resno == c).map(|t| t.expr);
    let mut c = c;
    for op in open.ops.iter().rev() {
        let e = match op {
            Op::Project(l) => nth(l, c)?,
            Op::Sort(s) => nth(s.tlist, c)?,
            Op::Filter(_) => continue,
            // a WindowAgg's row is its input row, then its functions' columns
            Op::Window(w) if c >= 1 && c as usize <= w.natts as usize => continue,
            _ => return None,
        };
        let mut e = e;
        while let Some(r) = e.as_relabel_type() {
            e = r.arg;
        }
        match e.as_var() {
            Some(v) if v.varno == OUTER_VAR && v.varlevelsup == 0 => c = v.varattno,
            _ => return Some(e),
        }
    }
    let Source::Breaker { slot } = open.source else { return None };
    let d = read_decl(b, slot, at).filter(|d| matches!(d.breaker.kind, BreakerKind::Sort | BreakerKind::Materialize))?;
    source_of(b, b.graph.pipelines.get(d.producer as usize)?, d.producer as usize, c)
}

/// Columns `x` and `y` of input `open` hold one image in every row: both read one column of a scan (`source_of`), so an
/// input sorted by one is sorted by the other (the input order's keys name columns, not collations, as everywhere).
fn same_image<'p>(b: &Builder<'p, '_>, open: &Open<'p>, x: i16, y: i16) -> bool {
    let at = b.graph.pipelines.len();
    let (Some(ex), Some(ey)) = (source_of(b, open, at, x), source_of(b, open, at, y)) else { return false };
    let (Some(vx), Some(vy)) = (ex.as_var(), ey.as_var()) else { return false };
    vx.varno > 0 && vx.varno == vy.varno && vx.varattno == vy.varattno && vx.varlevelsup == vy.varlevelsup && vx.vartype == vy.vartype
}

fn eq_keys<'p>(b: &Builder<'p, '_>, tlist: &NodeList<'p>, cols: &[i16], ops: &[u32], n: i32, k: Kind, id: i32) -> Result<ArenaVec<EqKey>, Refusal> {
    let n = n.max(0) as usize;
    if cols.len() < n || ops.len() < n {
        return Err(Refusal::shape(k, id, "key arrays shorter than their count"));
    }
    let mut keys = ArenaVec::with_capacity_in(n, b.arena);
    for i in 0..n {
        keys.push(EqKey { resno: cols[i], opno: ops[i], kind: column_kind(b.facts, tlist, cols[i], k, id)?, typid: crate::ops::shared::sort::column_type(tlist, cols[i], k, id)? });
    }
    Ok(keys)
}

/// The target list of WindowAgg `w`'s child plan (its lefttree): the stored row's first cells, the columns a partition or
/// order key names (lowerv4 window premise.rs / rows.rs read it here, rule R12); none without a child plan.
pub fn child_tlist<'p>(w: &'p WindowAgg<'p>) -> Option<&'p NodeList<'p>> {
    w.plan.lefttree.and_then(|c| c.as_plan()).map(|c| &c.targetlist)
}

/// ExecInitWindowAgg's `winstate->funcs` after its dedup: the window functions of the qual, then of the run condition
/// (none after setrefs), then of the target list, each in ExecInitExprRec's order and each `equal` one once unless it is
/// VOLATILE (`vol`: contain_volatile_functions; D1354: PG gives each volatile occurrence its own perfunc).
pub fn window_funcs<'p>(w: &'p WindowAgg<'p>, vol: &dyn Fn(Node<'p>) -> bool) -> Vec<Node<'p>> {
    let mut out = Vec::new(); // alloc: Exec
    window_funcs_in(w, &mut out, vol);
    out
}

/// Function `f`'s index in `funcs` (`window_funcs`' list): its own node first (D1354: a VOLATILE occurrence is its own
/// entry), else the `equal` entry it was deduplicated into.
pub fn func_index<'p>(funcs: &[Node<'p>], f: Node<'p>) -> Option<usize> {
    funcs.iter().position(|g| g.ptr_eq(f)).or_else(|| funcs.iter().position(|g| types_nodes::equal(*g, f)))
}

/// A list `window_funcs_in` fills: a heap `Vec` (the protocol's) or the decomposer's arena list (no heap allocation).
pub trait FuncList<'p> {
    /// `n` itself, or an entry `equal` to it when `dedup`.
    fn has(&self, n: Node<'p>, dedup: bool) -> bool;
    fn add(&mut self, n: Node<'p>);
}

impl<'p> FuncList<'p> for Vec<Node<'p>> {
    fn has(&self, n: Node<'p>, dedup: bool) -> bool {
        self.iter().any(|g| g.ptr_eq(n) || (dedup && types_nodes::equal(*g, n)))
    }
    fn add(&mut self, n: Node<'p>) {
        self.push(n); // alloc: Exec
    }
}

impl<'p> FuncList<'p> for ArenaVec<Node<'p>> {
    fn has(&self, n: Node<'p>, dedup: bool) -> bool {
        self.iter().any(|g| g.ptr_eq(n) || (dedup && types_nodes::equal(*g, n)))
    }
    fn add(&mut self, n: Node<'p>) {
        self.push(n);
    }
}

/// `window_funcs` into `out`.
pub fn window_funcs_in<'p, L: FuncList<'p>>(w: &'p WindowAgg<'p>, out: &mut L, vol: &dyn Fn(Node<'p>) -> bool) {
    struct Collect<'a, 'p, L>(&'a mut L, &'a dyn Fn(Node<'p>) -> bool);
    impl<'p, L: FuncList<'p>> nodes_core::NodeWalker<'p> for Collect<'_, 'p, L> {
        fn visit(&mut self, n: Node<'p>) -> types_error::PgResult<bool> {
            if n.as_window_func().is_some() {
                if !self.0.has(n, !(self.1)(n)) {
                    self.0.add(n);
                }
                return Ok(false);
            }
            nodes_core::expression_tree_walker(n, self)
        }
    }
    for e in w.plan.qual.iter().chain(w.runCondition.iter()).chain(w.plan.targetlist.iter()) {
        let _ = nodes_core::NodeWalker::visit(&mut Collect(&mut *out, vol), e);
    }
}

/// The stored row of a window's partition buffer past the child's `natts` columns: per function of `window_funcs`, its
/// first argument cell, its argument count and its FILTER's cell (lowerv4's window end evaluates them; hosts v4/m10.rs
/// reads them).
pub fn arg_cells(funcs: &[Node<'_>], natts: usize) -> Vec<(usize, usize, Option<usize>)> {
    let mut at = natts;
    funcs.iter().filter_map(|f| f.as_window_func()).map(|f| {
        let n = f.args.len();
        let cells = (at, n, f.aggfilter.map(|_| at + n));
        at += n + f.aggfilter.is_some() as usize;
        cells
    }).collect()
}

fn functions<'p>(b: &Builder<'p, '_>, w: &'p WindowAgg<'p>, funcs: &[Node<'p>], frame: FrameCursor<'p>, k: Kind, id: i32) -> Result<ArenaVec<WinFn<'p>>, Refusal> {
    let mut fns = ArenaVec::new_in(b.arena);
    for &node in funcs {
        let f: &'p WindowFunc<'p> = node.as_window_func().expect("window_funcs keeps window functions");
        if f.winref != w.winref {
            return Err(Refusal::shape(k, id, "a window function of another WindowAgg"));
        }
        let kind = fn_kind(f.winfnoid, f.winagg);
        if f.aggfilter.is_some() && kind != WinFnKind::Aggregate {
            return Err(Refusal::shape(k, id, "a FILTER on a non-aggregate window function"));
        }
        let has = |same: &dyn Fn(Node<'p>) -> bool| w.plan.targetlist.iter().filter_map(|t| t.as_target_entry()).find(|t| { let mut hit = false; expr::walk(t.expr, &mut |n| { hit |= same(n); !hit }); hit }).map(|t| t.resno);
        let resno = has(&|n| n.ptr_eq(node)).or_else(|| has(&|n| types_nodes::equal(n, node))).unwrap_or(0);
        fns.push(WinFn { resno, node, func: f, kind, cursor: FrameCursor { needs: needs_of(kind, frame.mode), ..frame } });
    }
    if fns.is_empty() {
        return Err(Refusal::shape(k, id, "a WindowAgg without a window function of its own"));
    }
    Ok(fns)
}

/// V4R-C: `l` with each window function of `funcs` read as its column of the window's row (`OUTER_VAR`, `natts + 1 + i`),
/// and a run condition's `Var(0, resno)` of a window function entry the same way. set_windowagg_runcondition_references
/// writes varno 0 and fix_scan_list then adds the plan's rtoffset to it like to any non-special varno (setrefs.c
/// fix_scan_expr_mutator: only IS_SPECIAL_VARNO stays), so a window planned in a subquery reads `Var(rtoffset, resno)`:
/// every non-special varno in these lists is that read (the lists' input columns are `OUTER_VAR` after setrefs).
fn rewrite<'p>(b: &Builder<'p, '_>, w: &'p WindowAgg<'p>, funcs: &[Node<'p>], natts: usize, l: &'p NodeList<'p>, k: Kind, id: i32) -> Result<&'p NodeList<'p>, Refusal> {
    let mcx = b.alloc.ok_or(Refusal::shape(k, id, "a window's lists: no node allocator (decompose_in)"))?;
    let col = |f: Node<'p>| -> types_error::PgResult<Option<Node<'p>>> {
        let Some(i) = func_index(funcs, f) else { return Ok(None) };
        let wf = f.as_window_func().expect("a window function");
        Node::mk_var(mcx, OUTER_VAR, (natts + 1 + i) as i16, wf.wintype, -1, wf.wincollid, 0).map(Some)
    };
    fn swap<'p>(mcx: mcx::Mcx<'p>, x: Node<'p>, w: &'p WindowAgg<'p>, col: &dyn Fn(Node<'p>) -> types_error::PgResult<Option<Node<'p>>>) -> types_error::PgResult<Option<Node<'p>>> {
        crate::walk::stack_check(); // D3748
        if x.as_window_func().is_some() {
            return col(x);
        }
        if let Some(v) = x.as_var().filter(|v| v.varno >= 0 && v.varlevelsup == 0) {
            let te = w.plan.targetlist.iter().filter_map(|t| t.as_target_entry()).find(|t| t.resno == v.varattno);
            return match te.map(|t| t.expr).filter(|e| e.as_window_func().is_some()) {
                Some(e) => col(e),
                None => Ok(None),
            };
        }
        nodes_core::expression_tree_mutator_dyn(mcx, x, &mut |y| swap(mcx, y, w, col))
    }
    let oom = |_| Refusal::shape(k, id, "a window's lists: out of memory rewriting its window functions");
    match nodes_core::mutate_list_dyn(mcx, l, &mut |x| swap(mcx, x, w, &col)).map_err(oom)? {
        Some(new) => Ok(Node::mk_list(mcx, new).map_err(oom)?.as_list().expect("a list node")),
        None => Ok(l),
    }
}

/// btree strategy numbers (stratnum.h), as `effects::builtin_btree_member` answers them.
const BT_LESS: u8 = 1;
const BT_LESS_EQUAL: u8 = 2;
const BT_GREATER_EQUAL: u8 = 4;
const BT_GREATER: u8 = 5;

/// Every run condition of `w` passes the first row of each partition, so the window emits at least one row per partition
/// (nodeWindowAgg.c: once a run condition fails the rest of the partition is not emitted; on the first row it would drop
/// the whole partition). Decided for the forms the planner builds run conditions from over a function whose first row is
/// 1 (row_number, rank, dense_rank: find_window_run_conditions): `f < c` with c > 1, `f <= c` with c >= 1, or the same
/// with the sides swapped. No run condition: true. Each form passes every partition's first row or none, so a program
/// keyed with the constant as a word (lower inputs/key.rs: a by-value literal's value is no part of the key) answers
/// right for any constant: one that fails the first row empties every partition, and a stop over an empty window shows
/// nothing. `f = c` keeps a partition's first row only for c = 1 and its c-th row otherwise (a partition shorter than c
/// is emptied, the rest not), so it would need the constant's class in the key: not taken.
pub fn partitions_never_empty<'p>(w: &'p WindowAgg<'p>) -> bool {
    // a window function read directly or as its target list entry's `Var(0, resno)` (set_windowagg_runcondition_references;
    // varno + rtoffset under a subquery, see `rewrite`)
    let func = |n: Node<'p>| n.as_window_func().or_else(|| {
        let v = n.as_var().filter(|v| v.varno >= 0 && v.varlevelsup == 0)?;
        w.plan.targetlist.iter().filter_map(|t| t.as_target_entry()).find(|t| t.resno == v.varattno)?.expr.as_window_func()
    });
    w.runCondition.iter().all(|cond| {
        let Some(o) = cond.as_op_expr() else { return false };
        let (Some(x), Some(y)) = (o.args.iter().next(), o.args.iter().nth(1)) else { return false };
        let Some((_, _, _, strategy)) = crate::effects::builtin_btree_member(o.opno) else { return false };
        let (on_left, f, c) = match (func(x), func(y)) {
            (Some(f), None) => (true, f, y),
            (None, Some(f)) => (false, f, x),
            _ => return false,
        };
        if f.winref != w.winref || !matches!(fn_kind(f.winfnoid, f.winagg), WinFnKind::RowNumber | WinFnKind::Rank | WinFnKind::DenseRank) {
            return false;
        }
        let Some(c) = c.as_const().filter(|c| !c.constisnull && c.constbyval) else { return false };
        let c = match (c.consttype, c.constlen) {
            (types_core::INT8OID, 8) => c.constvalue.as_i64(),
            (types_core::INT4OID, 4) => i64::from(c.constvalue.as_i32()),
            (types_core::INT2OID, 2) => i64::from(c.constvalue.as_i16()),
            _ => return false,
        };
        match (on_left, strategy) {
            (true, BT_LESS) | (false, BT_GREATER) => c > 1,
            (true, BT_LESS_EQUAL) | (false, BT_GREATER_EQUAL) => c >= 1,
            _ => false,
        }
    })
}

/// btree strategy number of equality (stratnum.h).
const BT_EQUAL: u8 = 3;

/// The `f = 1` form `partitions_never_empty` leaves out, as the qual of a SubqueryScan `scanrelid` over `w` (PostgreSQL keeps
/// the qual and adds `f <= 1` as the run condition: find_window_run_conditions): the Const of `qual` when it is exactly one
/// `f = 1` over a row_number / rank / dense_rank of `w` (read as the scan's column of `w`'s target list entry), under a
/// built-in btree equality with a non-NULL by-value integer constant 1. Every partition's first row numbers 1 under all
/// three, so the qual keeps one row or more of each partition. Only for the constant 1: the caller files the decision as a
/// literal guard on the Const it returns (the content key holds the constant as a word; lowerv4 `lit_decide`).
pub fn first_rows_only<'p>(w: &'p WindowAgg<'p>, scanrelid: u32, qual: &'p NodeList<'p>) -> Option<Node<'p>> {
    let mut conds = qual.iter();
    let (Some(cond), None) = (conds.next(), conds.next()) else { return None };
    let o = cond.as_op_expr()?;
    let (x, y) = (o.args.iter().next()?, o.args.iter().nth(1)?);
    if o.args.iter().nth(2).is_some() || crate::effects::builtin_btree_member(o.opno)?.3 != BT_EQUAL {
        return None;
    }
    let func = |n: Node<'p>| {
        let v = n.as_var().filter(|v| u32::try_from(v.varno).is_ok_and(|r| r == scanrelid) && v.varlevelsup == 0)?;
        w.plan.targetlist.iter().filter_map(|t| t.as_target_entry()).find(|t| t.resno == v.varattno)?.expr.as_window_func()
    };
    let (f, k) = match (func(x), func(y)) {
        (Some(f), None) => (f, y),
        (None, Some(f)) => (f, x),
        _ => return None,
    };
    if f.winref != w.winref || !matches!(fn_kind(f.winfnoid, f.winagg), WinFnKind::RowNumber | WinFnKind::Rank | WinFnKind::DenseRank) {
        return None;
    }
    let c = k.as_const().filter(|c| !c.constisnull && c.constbyval)?;
    let one = match (c.consttype, c.constlen) {
        (types_core::INT8OID, 8) => c.constvalue.as_i64() == 1,
        (types_core::INT4OID, 4) => c.constvalue.as_i32() == 1,
        (types_core::INT2OID, 2) => c.constvalue.as_i16() == 1,
        _ => false,
    };
    one.then_some(k)
}

/// `w` only numbers its rows: every target list entry a plain column or a row_number / rank / dense_rank of `w` with no
/// argument and no FILTER, no frame offset, and every PARTITION BY / ORDER BY equality a built-in btree member. Those
/// functions read only row positions and peer changes (nodeWindowAgg.c / windowfuncs.c window_row_number, window_rank,
/// window_dense_rank), and the built-in equalities that find partition and peer changes neither raise nor act, so
/// computing them over more rows than PostgreSQL reads shows nothing.
pub fn numbering_only<'p>(w: &'p WindowAgg<'p>) -> bool {
    let builtin = |ops: &[u32], k: i32| ops.get(..k.max(0) as usize).is_some_and(|ops| ops.iter().all(|&op| crate::effects::builtin_btree_member(op).is_some()));
    w.startOffset.is_none()
        && w.endOffset.is_none()
        && builtin(w.partOperators, w.partNumCols)
        && builtin(w.ordOperators, w.ordNumCols)
        && w.plan.targetlist.iter().filter_map(|t| t.as_target_entry()).all(|t| {
            t.expr.as_var().is_some_and(|v| v.varlevelsup == 0)
                || t.expr.as_window_func().is_some_and(|f| {
                    f.winref == w.winref
                        && f.args.is_nil()
                        && f.aggfilter.is_none()
                        && matches!(fn_kind(f.winfnoid, f.winagg), WinFnKind::RowNumber | WinFnKind::Rank | WinFnKind::DenseRank)
                })
        })
}

/// window_core-0107 (perf): the input rows a top WindowAgg with no PARTITION BY whose window functions are all
/// row_number() reads under its run condition, and the Sort right below it (no qual) that keeps them as a top. Its row
/// numbers are input positions: `row_number() < c` passes rows 1 .. c - 1 and stops at row max(c, 1), `<= c` at row
/// c + 1, once that row is projected (D1234: PostgreSQL projects a row before its run condition, so the window still sees
/// it). Those are the first rows of the Sort's order, so the Sort keeps them as a top (`Builder::limit`, sort.rs
/// `SortForm::TopK`) where it sorted every row. PostgreSQL's Sort sorts every row too, but its Incremental Sort loads
/// batches of 32 rows or more where a top over a presorted prefix stops once its bound's prefix group closes (V4R-179):
/// so only where nothing below the sort may raise or acts (the bound is then a cost only).
fn top_rows<'p>(b: &Builder<'p, '_>, w: &'p WindowAgg<'p>) -> Option<(i32, i64)> {
    let child = w.plan.lefttree?;
    let sort = child.as_sort().or_else(|| child.as_incremental_sort().map(|i| &i.sort)).filter(|s| s.plan.qual.is_nil())?;
    let rows = run_rows(w)?;
    let below = crate::effects::of_plan(&crate::effects::Cx::new(b.stmt, b.facts, b.arena), sort.plan.lefttree?);
    (!below.may_raise && !below.acts).then_some((sort.plan.plan_node_id, rows))
}

/// The input rows a top WindowAgg with no PARTITION BY whose window functions are all row_number() reads before its run
/// condition stops it (`top_rows`): `row_number() < c` stops at row max(c, 1), `<= c` at row c + 1, the least over its
/// conditions; none where it has no run condition or one of another form. PostgreSQL's WindowAgg reads its input as its
/// rows are asked for (spool_tuples to the current row), projects the row, then ends (WINDOWAGG_DONE) at the first row
/// its run condition fails, reading no input row past it.
fn run_rows<'p>(w: &'p WindowAgg<'p>) -> Option<i64> {
    if !w.topWindow || w.partNumCols > 0 || w.runCondition.is_nil() {
        return None;
    }
    // a window function read directly or as its target list entry's `Var(0, resno)` (set_windowagg_runcondition_references;
    // varno + rtoffset under a subquery, see `rewrite`)
    let func = |n: Node<'p>| n.as_window_func().or_else(|| {
        let v = n.as_var().filter(|v| v.varno >= 0 && v.varlevelsup == 0)?;
        w.plan.targetlist.iter().filter_map(|t| t.as_target_entry()).find(|t| t.resno == v.varattno)?.expr.as_window_func()
    });
    let mut numbers = true;
    for e in w.plan.qual.iter().chain(w.runCondition.iter()).chain(w.plan.targetlist.iter()) {
        expr::walk(e, &mut |n| match n.as_window_func() {
            Some(f) => {
                numbers &= f.winref == w.winref && fn_kind(f.winfnoid, f.winagg) == WinFnKind::RowNumber;
                false
            }
            None => true,
        });
    }
    if !numbers {
        return None;
    }
    let mut rows: Option<i64> = None;
    for cond in w.runCondition.iter() {
        let o = cond.as_op_expr()?;
        let (x, y) = (o.args.iter().next()?, o.args.iter().nth(1)?);
        let (_, left, right, strategy) = crate::effects::builtin_btree_member(o.opno)?;
        let (on_left, c) = match (func(x), func(y)) {
            (Some(f), None) => (true, (f, y)),
            (None, Some(f)) => (false, (f, x)),
            _ => return None,
        };
        // `rn < 6` compares int8 with an int4 literal (int84lt, integer_ops' cross-type members): the constant's own width
        let (f, c) = (c.0, c.1.as_const().filter(|c| !c.constisnull && c.constbyval)?);
        let (fn_side, const_side) = if on_left { (left, right) } else { (right, left) };
        if f.wintype != types_core::INT8OID || fn_side != f.wintype || c.consttype != const_side {
            return None;
        }
        let c = match (c.consttype, c.constlen) {
            (types_core::INT8OID, 8) => c.constvalue.as_i64(),
            (types_core::INT4OID, 4) => i64::from(c.constvalue.as_i32()),
            (types_core::INT2OID, 2) => i64::from(c.constvalue.as_i16()),
            _ => return None,
        };
        let n = match (on_left, strategy) {
            (true, BT_LESS) | (false, BT_GREATER) => c.max(1),
            (true, BT_LESS_EQUAL) | (false, BT_GREATER_EQUAL) => c.max(0).saturating_add(1),
            _ => return None,
        };
        rows = Some(rows.map_or(n, |m| m.min(n)));
    }
    rows
}

/// wpart-0002 (perf), D1486 rule C: the input rows a top WindowAgg under a LIMIT reads, `offset + count`, one more where an
/// aggregate's ROWS frame ends at the current row (eval_windowaggregates fetches the row past the frame's end to find it:
/// PostgreSQL 18.6 reads 3 rows under `count(*) OVER (ROWS UNBOUNDED PRECEDING) .. LIMIT 2`): a constant count of one or
/// more, a constant offset or none, no WITH TIES, no qual on the window (its Limit counts the rows the qual passes), and
/// every window function one whose row reads no later input row (row_number, rank, dense_rank, an aggregate whose ROWS
/// frame ends at the current row or before it). PostgreSQL's WindowAgg reads its input as its rows are asked for
/// (spool_tuples to the current row) and its Limit asks for none past the bound. `exact` (the second): that is the count
/// PostgreSQL reads, not a bound of it: no run condition but one `run_rows` reads (another may stop the window sooner),
/// every aggregate one that raises on no input (`effects::agg_oid_quiet`: its transitions run between two input rows in
/// PostgreSQL) and every frame end known (CURRENT ROW, or a constant `n PRECEDING`; `0 PRECEDING` is the current row).
/// D1486 (rxshape1): a run condition `run_rows` reads ends the window at its own row, under a LIMIT or none: the count is
/// then the least of the two, exact (its window functions are all row_number(), which read no later row).
fn first_rows<'p>(b: &Builder<'p, '_>, w: &'p WindowAgg<'p>, functions: &[WinFn<'p>]) -> Option<(i64, bool)> {
    if !w.topWindow || !w.plan.qual.is_nil() {
        return None;
    }
    let run = run_rows(w);
    let limit = match b.limit {
        Some((node, count, offset, ties)) if node == w.plan.plan_node_id => {
            let (false, crate::graph::Bound::Const(c)) = (ties, count) else { return None };
            let o = match offset {
                crate::graph::Bound::All => 0,
                crate::graph::Bound::Const(o) if o >= 0 => o,
                _ => return None,
            };
            Some((c, o))
        }
        _ => None,
    };
    if limit.is_some_and(|(c, _)| c < 1) || limit.is_none() && run.is_none() {
        return None;
    }
    // a `n PRECEDING` frame end's constant offset (an int8 in ROWS mode), none when it is not one
    // (read as a LIMIT bound is, `ops::bound`: a by-value 8-byte constant)
    let offset_of = |n: Node<'p>| match crate::ops::bound(Some(n)) {
        crate::graph::Bound::Const(k) if n.as_const().is_some_and(|c| c.consttype == types_core::INT8OID) => Some(k),
        _ => None,
    };
    let (mut past, mut exact) = (false, w.runCondition.is_nil() || run.is_some());
    for f in functions {
        match (f.kind, f.cursor.mode, f.cursor.end) {
            (WinFnKind::RowNumber | WinFnKind::Rank | WinFnKind::DenseRank, ..) => {}
            (WinFnKind::Aggregate, FrameMode::Rows, FrameBound::CurrentRow) => past = true,
            (WinFnKind::Aggregate, FrameMode::Rows, FrameBound::Preceding(n)) => match offset_of(n) {
                Some(n) if n > 0 => {}
                Some(_) => past = true,
                None => (past, exact) = (true, false),
            },
            _ => return None,
        }
        if f.kind == WinFnKind::Aggregate {
            exact &= crate::effects::agg_oid_quiet(f.func.winfnoid, || f.func.args.iter().next().is_some_and(crate::effects::num_bounded));
        }
    }
    let n = limit.map_or(i64::MAX, |(c, o)| c.saturating_add(o).saturating_add(past as i64));
    Some((run.map_or(n, |r| r.min(n)), exact))
}

/// wpart-0002 (perf), wlim-w02: a union `open` streams (its arms held, `open.union`) under a window that reads only its
/// first `n` rows (`first_rows`): each arm that is a scan's filters ending in its projection gets a cap of `n` rows (a
/// Limit before the projection), which lowerv4 counts across the union's feeds in arm order (rerun/union_arms.rs
/// `cap_union`), so the union's Materialize form (`breakers::one_fill`) holds the union's first `n` rows in the arms' order
/// and an arm past them reads no row, where it held every row. A loud arm is filled under such a window only by rule C
/// (lowerv4 union_arms.rs `capped`: the cap is PostgreSQL's exact read, delegated where a reader may stop sooner), so the
/// rows an arm leaves unread are rows PostgreSQL never reads. Any other arm (a
/// join, a union, a set function) is left whole, its rows counted by no cap, and each capped arm then counts its own rows
/// (true: every arm took the cap, `WindowDesc::arms_capped`).
fn cap_arms<'p>(b: &mut Builder<'p, '_>, open: &Open<'p>, n: i64) -> bool {
    let Some(u) = open.union else { return false };
    let mut all = true;
    for a in b.graph.unions[u as usize].arms.iter_mut() {
        let plain = a.union.is_none() && a.desc.consumes.is_empty() && a.ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_)));
        if !plain || !matches!(a.ops.last(), Some(Op::Project(_))) || !matches!(a.source, Source::Range { .. } | Source::Index { .. }) {
            all = false;
            continue;
        }
        let at = a.ops.len() - 1;
        a.ops.insert(at, Op::Limit { count: crate::graph::Bound::Const(n), offset: crate::graph::Bound::All, ties: None });
        for x in a.node_ops.iter_mut().filter(|x| x.first_op as usize >= at) {
            x.first_op += 1;
        }
    }
    all
}

/// PSP3-01: `open`, a Merge Append's reader whose ops past its Merge op are filters and projections, ends in a LIMIT of `n`.
fn cap_merge<'p>(mut open: Open<'p>, n: i64) -> Open<'p> {
    use crate::ops::shared::append::AppendDesc;
    let at = open.ops.iter().position(|op| matches!(op, Op::Append(AppendDesc::Merge { .. })));
    if open.union.is_none() && at.is_some_and(|at| open.ops[at + 1..].iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_)))) {
        open.push(Op::Limit { count: crate::graph::Bound::Const(n), offset: crate::graph::Bound::All, ties: None });
    }
    open
}

pub fn map<'p>(b: &mut Builder<'p, '_>, node: Node<'p>, k: Kind) -> Result<Open<'p>, Refusal> {
    let w: &'p WindowAgg<'p> = node.as_window_agg().unwrap();
    let plan = &w.plan;
    let id = plan.plan_node_id;
    let child = plan.lefttree.and_then(|c| c.as_plan()).ok_or(Refusal::shape(k, id, "no subtree"))?;
    // window_core-0107: the Sort below keeps only the rows the run condition reads (`top_rows`), as under a LIMIT
    let outer = b.limit;
    if let Some((sort, n)) = top_rows(b, w) {
        b.limit = Some((sort, crate::graph::Bound::Const(n), crate::graph::Bound::All, false));
    }
    let open = b.child(plan, k);
    b.limit = outer;
    let mut open = open?;
    let frame = frame_of(w, k, id)?;
    let partition = eq_keys(b, &child.targetlist, w.partColIdx, w.partOperators, w.partNumCols, k, id)?;
    let order = eq_keys(b, &child.targetlist, w.ordColIdx, w.ordOperators, w.ordNumCols, k, id)?;
    // D1292: a partition or peer compare under no collation over a collatable column raises in PG (texteq's check). Over
    // a Sort that raises at its first call (`over_raising_sort`) no row reaches a compare: the window's first call pulls
    // the Sort, whose begin raises 42P22, and lowerv4 serves that raise or refuses it by D1292's name (sort_raise.rs).
    let raised = crate::effects::reads_raising_sort(&b.graph, b.facts, &open);
    for (cols, colls, n) in [(w.partColIdx, w.partCollations, w.partNumCols), (w.ordColIdx, w.ordCollations, w.ordNumCols)].into_iter().filter(|_| !raised) {
        for (i, &c) in cols.iter().take(n.max(0) as usize).enumerate() {
            let t = crate::ops::shared::sort::column_type(&child.targetlist, c, k, id)?;
            if crate::ops::shared::sort::indeterminate(b.facts, t, colls.get(i).copied().unwrap_or(0)) {
                return Err(Refusal::shape(k, id, crate::ops::shared::sort::INDETERMINATE_COLL));
            }
        }
    }
    let cx = crate::effects::Cx::new(b.stmt, b.facts, b.arena);
    let vol = move |n: Node<'p>| crate::effects::expr_acts(&cx, n);
    let mut funcs = ArenaVec::new_in(b.arena);
    window_funcs_in(w, &mut funcs, &vol);
    let natts = child.targetlist.len();
    let functions = functions(b, w, &funcs, frame, k, id)?;
    // D1486: one partition whose rows are all peers (`whole`), its keys none or each one value in every row (`fixed` below)
    let (mut one_part, mut peers) = (partition.is_empty(), order.is_empty());
    // D4020: over an input that returns no row (`gate::no_rows`: a Sort over the planner's empty Result, or another
    // WindowAgg over one) no row is out of its partition or order, so the input needs no order (PG may drop a key from
    // that Sort, as its WHERE pins it)
    let empty = plan.lefttree.is_some_and(crate::ops::shared::gate::no_rows);
    if (!partition.is_empty() || !order.is_empty()) && !empty {
        let (all, proj) = (crate::ops::shared::order::order_of(b, &open), crate::ops::shared::order::projection(&open));
        let at = b.graph.pipelines.len();
        // a key every input row holds one image of (`pinned_key`: a scan filter or index clause `column = Const`)
        // partitions and orders nothing: PG's planner drops its pathkey (an equivalence class with a constant), so the
        // input comes sorted without it, and its compare finds every row a peer (facts.input-order, window_core-0113);
        // the input's own order loses such a key too
        let pinned = |c: i16| pinned_key(b, &open, at, c, None);
        let mut o = Order { n: 0, ..all };
        for (i, x) in all.keys().iter().enumerate().filter(|(_, x)| !(x.col.varno == OUTER_VAR && pinned(x.col.attno))) {
            // a key of a non-default btree family (`Order::fams`: a Sort USING bpchar_pattern_ops' ~<~ or record_image_ops'
            // *<, a user opclass's) keeps its family: only a partition or ORDER BY key of its own column takes that family
            // (`key_is` / `fam_of` below), no default-family key matches it (D1346)
            if all.fams[i] != 0 {
                o.push_fam(*x, all.fams[i]);
            } else {
                o.push_expr(*x, all.exprs[i]);
            }
        }
        // window_core-0122 / -0123: a key column beside an input order key that reads the same scan column under a
        // relabel (another collation's copy) is that key: the order equates them
        for w in partition.iter().chain(order.iter()) {
            let x = o.keys().iter().map(|x| x.col).find(|x| x.varno == OUTER_VAR && x.attno != w.resno && same_image(b, &open, x.attno, w.resno));
            if let Some(x) = x.filter(|x| !o.same(*x, Col::outer(w.resno))) {
                o.push_eq(x, Col::outer(w.resno));
            }
        }
        // a key one value in every row of the pass (`fixed_key`) partitions and orders nothing: PostgreSQL's planner
        // drops its pathkey (no Sort below), and the window's compare finds every row a peer (facts.input-order,
        // window_core-0118 / -0119); a constant key `sort::leads_with` skips already
        // a key every row holds equal under the key's own equality (its operator and collation: `pinned_peer_at`) is a
        // peer in every row too, whatever its images (a text column under `symbol = 'NVDA'`, window_core-0281)
        let coll = |colls: &[u32], i: usize| colls.get(i).copied().unwrap_or(0);
        let fixed = |c: i16, eq: (u32, u32)| fixed_key(b, &open, at, c) || pinned_key(b, &open, at, c, Some(eq));
        // USEROP (K1.win / K1.winp / K7.sw, K10.y3): a partition or ORDER BY key whose input key (its own column) is in a
        // non-default btree family (`Order::fams`: a Sort USING a user opclass's operator, the CompareFn key kind) wants that
        // family's order. PostgreSQL sorts a WindowAgg's input by its PARTITION BY and ORDER BY pathkeys, whose family is the
        // one each key's equality operator (`partOperators` / `ordOperators`) is strategy 3 of (a PARTITION BY key the ORDER
        // BY names USING takes that operator's family: K10.y3's `PARTITION BY m ORDER BY m USING <#` sorts by `m USING <#`
        // alone), and the partitions and peers compare through those operators (hosts frame.rs `keys_match`, fmgr): as
        // D2313's sorted group (facts/stream.rs `family_order`) each key takes its input key's family. A key with no such
        // input key wants its default family's, as before. `key_is`: input key `m` orders column `c` (its default family's
        // equalities name a column equal to it; another family's key its own column alone, as `sort::leads_with_fams`).
        let key_is = |m: usize, c: Col| if o.fams[m] == 0 { o.same(o.keys[m].col, c) } else { o.keys[m].col == c };
        let in_partition = |m: usize| partition.iter().any(|p| key_is(m, Col::outer(p.resno)));
        let prefix = (0..o.n as usize).take_while(|&m| in_partition(m)).count();
        let fam_of = |col: Col| (0..o.n as usize).find(|&m| o.fams[m] != 0 && o.keys[m].col == col).map_or(0, |m| o.fams[m]);
        let parts = partition.iter().enumerate().all(|(i, p)| crate::ops::shared::order::is_const(proj, Col::outer(p.resno)) || fixed(p.resno, (p.opno, coll(w.partCollations, i))) || (0..prefix).any(|m| key_is(m, Col::outer(p.resno))));
        one_part = partition.iter().enumerate().all(|(i, p)| crate::ops::shared::order::is_const(proj, Col::outer(p.resno)) || fixed(p.resno, (p.opno, coll(w.partCollations, i))));
        peers = order.iter().enumerate().all(|(i, key)| crate::ops::shared::order::is_const(proj, Col::outer(key.resno)) || fixed(key.resno, (key.opno, coll(w.ordCollations, i))));
        let mut want = Order::NONE;
        for m in 0..prefix {
            want.push_fam(o.keys[m], o.fams[m]);
        }
        for (_, key) in order.iter().enumerate().filter(|(i, key)| !fixed(key.resno, (key.opno, coll(w.ordCollations, *i)))) {
            let col = Col::outer(key.resno);
            want.push_fam(OrderKey { col, desc: false, nulls_first: false }, fam_of(col));
        }
        if !(parts && crate::ops::shared::order::leads_with_fams(&o, proj, want.keys(), &want.fams[..want.n as usize], false)) {
            return Err(Refusal::shape(k, id, if all.is_known() { "a WindowAgg over an input not in its partition and order keys' order" } else { "a WindowAgg over an input with no known order" }));
        }
    }
    // V4R-C: the partition buffer always (PG's tuplestore): lowerv4's window end stores each row with its functions'
    // argument cells, the window object (hosts v4/m10.rs) reads the rows by position
    let layout = keys_layout(b.arena, partition.iter().map(|x| x.kind));
    // the functions' argument and FILTER SubPlans that raise nothing and call nothing VOLATILE, evaluated per input row by
    // the fill ahead of its end, which stores each argument cell (lowerv4 window/rows.rs): PG evaluates them where a
    // frame reads the row, as often as it reads it, which shows nothing for such a SubPlan (subplan.rs `attach_window_args`)
    let cur = b.graph.cur_node.take(); // a per-row SubPlan's hashed set is no node's breaker (walk.rs `map_node`)
    let r = crate::ops::window_subplans(b, &funcs, k, id, open);
    b.graph.cur_node = cur;
    let r = r?;
    // D3374: the argument SubPlans the fill evaluates (`attach_window_args`, an acting one there too), read below
    let mut placed_ids: ArenaVec<i32> = ArenaVec::new_in(b.arena);
    for o in r.ops.iter() {
        if let Op::SubPlan(p) = o {
            placed_ids.push(p.plan_id);
        }
    }
    // wpart-0002: under a LIMIT the window reads only its first rows, so a union's arms read only that many together; rule
    // C (D1486) when that is the count PostgreSQL reads and every arm took the cap
    let first = first_rows(b, w, &functions);
    let arms_capped = first.is_some_and(|(n, _)| cap_arms(b, &r, n));
    let capped = first.filter(|&(_, exact)| arms_capped && exact).map(|(n, _)| n);
    // PSP3-01: a Merge Append right below whose rows cannot raise hands the buffer at most the rows PostgreSQL's WindowAgg
    // reads (`first_rows`' count bounds them, exact or not; the breaker form serves the same merges): its reader's walk
    // ends there, so lowerv4's pull form pulls them (merge_admit.rs `merge_cap`), not every arm whole. D2895: a merge whose
    // rows may raise too when the count is exact (rule C's test above): the pull form reads PostgreSQL's arm rows (a row
    // per arm at the open, then the emitting arm's next when the next row is asked for), so up to that count an arm row
    // raises exactly where PostgreSQL's does; where lowerv4 cannot pull the merge, its breaker keeps D449's rule.
    // `PULL_FEW_ROWS`: a bound under which the pull form costs no more than the breaker's at any arm count and width
    use crate::ops::shared::append::PULL_FEW_ROWS;
    let quiet = |c: Node<'p>| !crate::effects::of_plan(&crate::effects::Cx::new(b.stmt, b.facts, b.arena), c).may_raise;
    let r = match first {
        Some((n, exact)) if n <= PULL_FEW_ROWS && plan.lefttree.is_some_and(|c| c.as_merge_append().is_some() && (exact || quiet(c))) => cap_merge(r, n),
        _ => r,
    };
    // a WindowAgg straight over an Append (every key pinned or none: no Sort between, held-out rnd-0310) fills its buffer
    // as one pass in the arms' order (`breakers::one_fill`: the union's Materialize form)
    open = crate::breakers::one_fill(&mut b.graph, r);
    let slot = crate::ops::shared::breaker::close(b, open, BreakerKind::Materialize, layout, Budget::Share, Spill::Runs, k, id)?;
    open = crate::ops::shared::breaker::emit(b, id, slot);
    let run_condition = rewrite(b, w, &funcs, natts, &w.runCondition, k, id)?;
    let (qual, tlist) = (rewrite(b, w, &funcs, natts, &plan.qual, k, id)?, rewrite(b, w, &funcs, natts, &plan.targetlist, k, id)?);
    // D3374: a SubPlan the fill evaluates (`attach_window_args`, an acting one there too) is the fill's, not the cell's: an
    // argument acts as a cell only by the code outside such SubPlans (their arguments are the fill step's too)
    let placed = |id: i32| placed_ids.contains(&id);
    let cell = |n: Node<'p>| {
        let mut acts = false;
        expr::walk(n, &mut |x| {
            if x.as_sub_plan().is_some_and(|s| placed(s.plan_id)) {
                return false;
            }
            acts |= crate::effects::of_node(&cx, x).acts || crate::effects::node_may_write(cx.cat, x);
            true
        });
        acts
    };
    let volatile_args = funcs.iter().filter_map(|f| f.as_window_func()).any(|f| f.args.iter().any(cell) || f.aggfilter.is_some_and(cell));
    let volatile_proj = tlist.iter().any(vol);
    let whole = crate::facts::window::reads_whole_in(w, one_part || w.partNumCols == 0, peers || w.ordNumCols == 0);
    open.push(Op::Window(WindowDesc { winref: w.winref, partition, order, frame, functions, natts: natts as u16, run_condition, top: w.topWindow, tlist: &plan.targetlist, plan: w, volatile_args, volatile_proj, whole, arms_capped, capped }));
    ops::filters(&mut open, qual);
    ops::project(&mut open, tlist);
    b.graph.note(k);
    Ok(open)
}

fn dump_bound(x: FrameBound<'_>, out: &mut dyn Write) -> fmt::Result {
    match x {
        FrameBound::UnboundedPreceding => write!(out, "unbounded-preceding"),
        FrameBound::UnboundedFollowing => write!(out, "unbounded-following"),
        FrameBound::CurrentRow => write!(out, "current-row"),
        FrameBound::Preceding(n) => {
            expr::print(n, out)?;
            write!(out, "-preceding")
        }
        FrameBound::Following(n) => {
            expr::print(n, out)?;
            write!(out, "-following")
        }
    }
}

impl WindowDesc<'_> {
    pub fn dump(&self, out: &mut dyn Write) -> fmt::Result {
        write!(out, "  window ref={}{} partition=", self.winref, if self.top { " top" } else { "" })?;
        dump_eq_keys(&self.partition, out)?;
        write!(out, " order=")?;
        dump_eq_keys(&self.order, out)?;
        write!(out, " frame={:?} ", self.frame.mode)?;
        dump_bound(self.frame.start, out)?;
        write!(out, "..")?;
        dump_bound(self.frame.end, out)?;
        if self.frame.exclude != Exclude::None {
            write!(out, " exclude={:?}", self.frame.exclude)?;
        }
        if !self.run_condition.is_nil() {
            write!(out, " run_condition=")?;
            expr::args(out, self.run_condition)?;
        }
        for f in self.functions.iter() {
            writeln!(out)?;
            write!(out, "    fn resno={} {:?} cursor=", f.resno, f.kind)?;
            for (bit, name) in [(CURSOR_ROW, "row"), (CURSOR_PARTITION, "partition"), (CURSOR_PEERS, "peers"), (CURSOR_FRAME, "frame")] {
                if f.cursor.needs & bit != 0 {
                    write!(out, "{name},")?;
                }
            }
            write!(out, " ")?;
            expr::print(f.node, out)?;
        }
        Ok(())
    }
}
