use core::cell::Cell;
use core::fmt;

use types_core::Oid;
use types_error::{PgError, ERRCODE_FEATURE_NOT_SUPPORTED};
use types_nodes::nodes_enums::LimitOption;
use types_nodes::plannodes::PlannedStmt;
use types_nodes::{Node, NodeTag};

use crate::ledger::{self, Kind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub kind: Kind,
    pub reason: &'static str,
    pub node_id: i32,
}

impl Refusal {
    pub fn kind(kind: Kind, node_id: i32) -> Refusal {
        Refusal { kind, reason: ledger::reason(kind), node_id }
    }

    pub const fn shape(kind: Kind, node_id: i32, reason: &'static str) -> Refusal {
        Refusal { kind, reason, node_id }
    }

    pub fn into_error(self) -> Box<PgError> {
        Box::new(PgError::error(format!("{self}")).with_sqlstate(ERRCODE_FEATURE_NOT_SUPPORTED)) // alloc: Cold
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "gravity_decompose: {} (node {}) is not expressible: {}", self.kind.name(), self.node_id, self.reason)
    }
}

impl From<Refusal> for Box<PgError> {
    fn from(r: Refusal) -> Self {
        r.into_error()
    }
}

/// The family a refusal belongs to (admit design fix 1): the ledger's per-PG-node kind, shared by lower and exec.
pub use crate::ledger::Kind as Family;

/// pathnodes.h `SETOP_HASHED`: a SetOp that builds a hash table (BuildTupleHashTable's grouping equality); a sorted one
/// compares its inputs through sort support (PG 18 nodeSetOp.c), with no gate.
const SETOP_HASHED: u32 = 1;

/// The equality operators PostgreSQL gates (EXECUTE on each one's function) when it initializes a plan node, outside any
/// expression's own init: a Memoize's hash operators (ExecInitMemoize's ExecBuildParamSetEqual, in every cache mode: D2771)
/// and ExecBuildGroupingEqual's operators (execTuplesMatchPrepare or BuildTupleHashTable at the node's init: D2786) of a
/// grouped Agg and each Agg of its chain, a Group, a Unique, a WindowAgg's PARTITION BY and ORDER BY, a hashed SetOp, a
/// RecursiveUnion that removes duplicates and a Limit WITH TIES. Every node of the statement: InitPlan initializes each
/// subplan and the plan tree (execMain.c). Operators, appended to `out`; exec's admission gates each one's function.
pub fn node_gate_ops(stmt: &PlannedStmt<'_>, out: &mut Vec<Oid>) {
    for r in stmt.subplans.iter().flatten().chain(stmt.planTree) {
        plan_gate_ops(r, out);
    }
}

/// `node_gate_ops` over the plan tree under `n`: its own operators, then its plan children's (the trees of the SubPlans
/// its expressions call are the statement's subplans).
fn plan_gate_ops(n: Node<'_>, out: &mut Vec<Oid>) {
    crate::walk::stack_check(); // D3748
    let Some(p) = n.as_plan() else { return };
    let mut ops = |o: &[Oid], k: i32| out.extend(o.iter().take(usize::try_from(k).unwrap_or(0))); // alloc: Program
    match n.node_tag() {
        NodeTag::T_Memoize => {
            let m = n.as_memoize().unwrap();
            ops(m.hashOperators, m.numKeys);
        }
        // each Agg of a chain is a phase ExecInitAgg prepares (the chain's Sorts are never plan nodes)
        NodeTag::T_Agg => {
            for a in core::iter::once(n).chain(n.as_agg().unwrap().chain.iter()).filter_map(|a| a.as_agg()) {
                if a.aggstrategy != crate::ops::shared::agg::AGG_PLAIN {
                    ops(a.grpOperators, a.numCols);
                }
            }
        }
        NodeTag::T_Group => {
            let g = n.as_group().unwrap();
            ops(g.grpOperators, g.numCols);
        }
        NodeTag::T_Unique => {
            let u = n.as_unique().unwrap();
            ops(u.uniqOperators, u.numCols);
        }
        NodeTag::T_WindowAgg => {
            let w = n.as_window_agg().unwrap();
            ops(w.partOperators, w.partNumCols);
            ops(w.ordOperators, w.ordNumCols);
        }
        NodeTag::T_SetOp => {
            let s = n.as_set_op().unwrap();
            if s.strategy == SETOP_HASHED {
                ops(s.cmpOperators, s.numCols);
            }
        }
        NodeTag::T_RecursiveUnion => {
            let r = n.as_recursive_union().unwrap();
            ops(r.dupOperators, r.numCols);
        }
        NodeTag::T_Limit => {
            let l = n.as_limit().unwrap();
            if l.limitOption == LimitOption::LIMIT_OPTION_WITH_TIES {
                ops(l.uniqOperators, l.uniqNumCols);
            }
        }
        _ => {}
    }
    let arms = n.as_append().map(|a| &a.appendplans).or(n.as_merge_append().map(|a| &a.mergeplans));
    let arms = arms.or(n.as_bitmap_and().map(|a| &a.bitmapplans)).or(n.as_bitmap_or().map(|a| &a.bitmapplans));
    let sub = n.as_subquery_scan().and_then(|s| s.subplan);
    for c in p.lefttree.into_iter().chain(p.righttree).chain(arms.into_iter().flat_map(|l| l.iter())).chain(sub) {
        plan_gate_ops(c, out);
    }
}

/// The equality operators an aggregate's DISTINCT over more than one column compares its sorted input with, through
/// ExecBuildGroupingEqual (build_pertrans_for_aggref's execTuplesMatchPrepare: D2786), appended to `out`; one column
/// compares through a bare fmgr_info, with no gate.
pub fn distinct_eq_ops(aggref: Node<'_>, out: &mut Vec<Oid>) {
    let Some(a) = aggref.as_aggref() else { return };
    if a.aggdistinct.len() < 2 {
        return;
    }
    out.extend(a.aggdistinct.iter().filter_map(|c| c.as_sort_group_clause()).map(|c| c.eqop)); // alloc: Program
}

leader_tls::leader_tls! {
    thread_local! {
        /// The root's reader for the admission running under `reading_partial`.
        static PARTIAL: Cell<bool> = const { Cell::new(false) };
        /// D3585: the admission running under `grouping_relsets` reads an Append's twin heap scans as one relation set.
        static RELSETS: Cell<bool> = const { Cell::new(false) };
        /// D4569: the admission running under `copying_union_nests` copies a quiet parameter-free nest into a SELECT's
        /// union arms (arm_nests.rs `per_arm`); off outside an admission scope (exec admission sets it from road
        /// append.nest, on by default), so decompose's own tests read the union's Materialize form.
        static UNION_NESTS: Cell<bool> = const { Cell::new(false) };
        /// GENP-1 (D3004): a generic plan's re-estimated node rows under `rescaling`, `(plan_node_id, rows)`.
        static ROWS: core::cell::RefCell<Vec<(i32, f64)>> = const { core::cell::RefCell::new(Vec::new()) }; // alloc: Program
    }
}

/// GENP-1 (D3004): the admission `f` run with a generic plan's node rows `rows` (`(plan_node_id, rows)`) re-estimated
/// for this execution's bound parameters (exec `admit::genp`: a custom plan's scan rows, PostgreSQL's own selectivity,
/// carried up through the nodes that hand every row on). Every estimate the walk reads (`plan_rows`) takes them; the
/// plan itself is unchanged. Restored on every exit.
pub fn rescaling<T>(rows: Vec<(i32, f64)>, f: impl FnOnce() -> T) -> T {
    struct Restore(Vec<(i32, f64)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let prev = core::mem::take(&mut self.0);
            ROWS.with(|r| *r.borrow_mut() = prev);
        }
    }
    let _r = Restore(ROWS.with(|r| core::mem::replace(&mut *r.borrow_mut(), rows)));
    f()
}

/// GENP-1: plan node `plan`'s estimated rows for this admission: the re-estimate `rescaling` holds for it, else the plan's.
pub fn plan_rows(plan: &types_nodes::plannodes::Plan<'_>) -> f64 {
    let id = plan.plan_node_id;
    ROWS.with(|r| r.borrow().iter().find(|x| x.0 == id).map(|x| x.1)).unwrap_or(plan.plan_rows)
}

/// D3005: the admission `f` run with this execution's integer extern parameters `vals` (`(paramid, value)`, non-NULL
/// int2 / int4 / int8 slots): a LIMIT or OFFSET whose count is one of them (`limit_externs`) is the execution's constant,
/// as a custom plan's would be, so the top-N roads that size by a constant count serve a generic plan's `LIMIT $1`.
/// Returns `f`'s result and the parameters a count took, which the admission's reuse must find unchanged (a generic
/// plan's next execution may bind another value).
pub fn binding_externs<T>(vals: Vec<(i32, i64)>, f: impl FnOnce() -> T) -> (T, Vec<(i32, i64)>) {
    gravity_program::externs::binding(vals, f)
}

/// D3019: a text-like extern parameter's facts in `binding_externs`' list: `QUIET_FACT` plus `QUIET_BIT` when its value
/// is a LIKE pattern no match raises on (`effects::like_image_quiet`); `QUIET_FACT` alone for NULL. No integer's value: an id
/// has one type, and `limit_externs` reads integer parameters only.
pub const QUIET_FACT: i64 = gravity_program::externs::TEXT_FACT;

/// D3019: `QUIET_FACT`'s bit for a quiet LIKE pattern.
pub const QUIET_BIT: i64 = gravity_program::externs::QUIET_BIT;

/// pg_type text, bpchar, varchar.
pub(crate) const fn text_like(t: Oid) -> bool {
    use crate::catalog::{BPCHAROID, TEXTOID, VARCHAROID};
    matches!(t, TEXTOID | VARCHAROID | BPCHAROID)
}

/// D3019: the admission's key salt from this execution's extern parameters: 0 when every text-like one is a quiet LIKE
/// pattern (`effects::pattern_quiet`), else a hash of the others' facts. A lowering that hoists a parameter's LIKE (join.hoist)
/// runs only where its pattern is quiet; the salt keys an execution with a loud pattern apart, so it never takes that
/// program from the program caches, the content key's or the front key's.
/// GENP-1: and the node rows `rescaling` holds (a generic plan's scans re-estimated for these values), so a program
/// lowered under other estimates is never taken by a key, the content key's or the front key's.
/// D2834: and the extern parameters the execution leaves to evaluation time (gravity_program::externs `deferring`), which
/// its program fetches where it evaluates them (a program over their words would read them ahead).
pub fn externs_salt() -> u64 {
    let texts = gravity_program::externs::all(|vals| vals.iter().filter(|x| x.1 < QUIET_FACT + 4 && x.1 != QUIET_FACT + QUIET_BIT).fold(0u64, |h, x| (h ^ (x.0 as u64 + 0x51) ^ ((x.1 - QUIET_FACT) as u64) << 32).wrapping_mul(0x0000_0100_0000_01b3))).unwrap_or(0);
    let rows = ROWS.with(|r| r.borrow().iter().fold(texts, |h, x| (h ^ (x.0 as u64 + 0x6e) ^ x.1.to_bits().rotate_left(17)).wrapping_mul(0x0000_0100_0000_01b3)));
    gravity_program::externs::deferred_ids().iter().fold(rows, |h, &id| (h ^ (id as u64 + 0x2834) ^ 0x6465_6600_0000_0000).wrapping_mul(0x0000_0100_0000_01b3))
}

/// The integer functions a LIMIT / OFFSET count over extern parameters may apply (RUNS-16 / B3-02: `LIMIT $1 + 1`,
/// `OFFSET $2 * 10`, `LIMIT -$1`): `(pg_proc oid, op, argument widths, result width)` in bytes, op `+` `-` `*`, `u` the
/// unary minus and `c` a cast. Each is immutable and strict and raises only where its result leaves its type (int.c,
/// int8.c), which `fold` checks the same way: a raising or NULL count stays the plan's run-time expression.
const LIMIT_FNS: &[(u32, u8, [u8; 2], u8)] = &[
    (176, b'+', [2, 2], 2), (177, b'+', [4, 4], 4), (178, b'+', [2, 4], 4), (179, b'+', [4, 2], 4), (463, b'+', [8, 8], 8),
    (837, b'+', [8, 2], 8), (841, b'+', [2, 8], 8), (1274, b'+', [8, 4], 8), (1278, b'+', [4, 8], 8),
    (180, b'-', [2, 2], 2), (181, b'-', [4, 4], 4), (182, b'-', [2, 4], 4), (183, b'-', [4, 2], 4), (464, b'-', [8, 8], 8),
    (838, b'-', [8, 2], 8), (942, b'-', [2, 8], 8), (1275, b'-', [8, 4], 8), (1279, b'-', [4, 8], 8),
    (152, b'*', [2, 2], 2), (141, b'*', [4, 4], 4), (170, b'*', [2, 4], 4), (171, b'*', [4, 2], 4), (465, b'*', [8, 8], 8),
    (839, b'*', [8, 2], 8), (943, b'*', [2, 8], 8), (1276, b'*', [8, 4], 8), (1280, b'*', [4, 8], 8),
    (213, b'u', [2, 0], 2), (212, b'u', [4, 0], 4), (462, b'u', [8, 0], 8),
    (313, b'c', [2, 0], 4), (754, b'c', [2, 0], 8), (481, b'c', [4, 0], 8), (314, b'c', [4, 0], 2), (480, b'c', [8, 0], 4), (714, b'c', [8, 0], 2),
];

/// D3005, RUNS-16 / B3-02: a LIMIT / OFFSET count `n` built from extern parameters: `$1`, an integer constant, or one of
/// `LIMIT_FNS` over them. `Some((params, value))`: the extern parameters it reads with this execution's values (in the
/// `binding_externs` scope) and its value, `None` there where a parameter is NULL or absent or a function would raise;
/// `None` when `n` reads anything else (a column, an executor parameter, another function) or no extern parameter.
fn extern_count(n: Node<'_>) -> Option<(Vec<(i32, i64)>, Option<i64>)> {
    let (ids, v) = fold(n)?;
    (!ids.is_empty()).then_some((ids, v))
}

/// `extern_count`'s walk: the parameters `n` reads (with their values) and its value.
fn fold(n: Node<'_>) -> Option<(Vec<(i32, i64)>, Option<i64>)> {
    crate::walk::stack_check(); // D3748
    let fit = |v: i128, w: u8| -> Option<i64> {
        let half = 1i128 << (8 * u32::from(w) - 1);
        (-half..half).contains(&v).then(|| v as i64)
    };
    if let Some(c) = n.as_const() {
        let w = match c.consttype {
            types_core::INT2OID => 2u8,
            types_core::INT4OID => 4,
            types_core::INT8OID => 8,
            _ => return None,
        };
        if c.constisnull {
            return Some((Vec::new(), None)); // alloc: Program
        }
        let raw = crate::bake::read(c).as_u64();
        let v = match w {
            2 => raw as u16 as i16 as i64,
            4 => raw as u32 as i32 as i64,
            _ => raw as i64,
        };
        return Some((Vec::new(), Some(v))); // alloc: Program
    }
    if let Some(p) = n.as_param() {
        if p.paramkind != types_nodes::primnodes::ParamKind::PARAM_EXTERN || !matches!(p.paramtype, types_core::INT2OID | types_core::INT4OID | types_core::INT8OID) {
            return None;
        }
        let id = p.paramid;
        // an integer parameter's value (a text-like one's facts sit from QUIET_FACT; a NULL or absent slot has none)
        let v = gravity_program::externs::take(id, |_| false).filter(|&v| v >= QUIET_FACT + 4);
        // a NULL slot's id is kept (the count reads a parameter) but never recorded: `limit_externs` takes no NULL count
        return Some((vec![(id, v.unwrap_or(0))], v)); // alloc: Program
    }
    let (fnoid, args) = match (n.as_func_expr(), n.as_op_expr()) {
        (Some(f), _) => (f.funcid, &f.args),
        (_, Some(o)) => (o.opfuncid, &o.args),
        _ => return None,
    };
    let &(_, op, _, rw) = LIMIT_FNS.iter().find(|r| r.0 == fnoid)?;
    let arity = if matches!(op, b'u' | b'c') { 1 } else { 2 };
    if args.len() != arity {
        return None;
    }
    let mut ids = Vec::new(); // alloc: Program
    let mut vals = [0i128; 2];
    let mut null = false;
    for (i, a) in args.iter().enumerate() {
        let (mut x, v) = fold(a)?;
        ids.append(&mut x);
        match v {
            Some(v) => vals[i] = i128::from(v),
            None => null = true,
        }
    }
    if null {
        return Some((ids, None));
    }
    let v = match op {
        b'+' => vals[0] + vals[1],
        b'-' => vals[0] - vals[1],
        b'*' => vals[0] * vals[1],
        b'u' => -vals[0],
        _ => vals[0],
    };
    Some((ids, fit(v, rw)))
}

/// D3005: a Limit's count and offset over extern parameters as this execution's constants (`(count, offset)`, each
/// `None` where it stays the plan's expression), recorded as taken. Taken only together: every bound that is an extern
/// parameter must hold a count of at least 1 (a count of 0 never pulls the child, D1716) or an offset of at least 0, so
/// a NULL, a negative value (PostgreSQL raises at the node, before any child row) or a zero count keeps every bound of
/// the node a run-time count, as before; a Limit with a constant and a run-time bound would order its child's work and
/// the bound's check apart from PostgreSQL's (m1_d1716_limit_zero_feeders, m8_d1018_limit).
pub fn limit_externs(count: Option<Node<'_>>, offset: Option<Node<'_>>) -> (Option<i64>, Option<i64>) {
    let (c, o) = (count.and_then(extern_count), offset.and_then(extern_count));
    let ok = c.as_ref().is_none_or(|(_, v)| v.is_some_and(|v| v >= 1)) && o.as_ref().is_none_or(|(_, v)| v.is_some_and(|v| v >= 0));
    if !ok || (c.is_none() && o.is_none()) {
        return (None, None);
    }
    for id in [&c, &o].into_iter().flatten().flat_map(|(ids, _)| ids.iter().map(|x| x.0)) {
        gravity_program::externs::take(id, |_| true);
    }
    (c.and_then(|x| x.1), o.and_then(|x| x.1))
}

/// The admission `f` run with the statement's root reader marked `partial` (a named cursor's portal: each FETCH takes a
/// counted part of the rows, the count known only at the FETCH).
pub fn reading_partial<T>(partial: bool, f: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            PARTIAL.with(|p| p.set(self.0));
        }
    }
    let _r = Restore(PARTIAL.with(|p| p.replace(partial)));
    f()
}

/// D3585: the admission `f` run with an Append's runs of twin heap scans read as one relation-set site each (ops/append.rs
/// `group_twins`); off, every arm keeps its own pipeline.
pub fn grouping_relsets<T>(on: bool, f: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            RELSETS.with(|p| p.set(self.0));
        }
    }
    let _r = Restore(RELSETS.with(|p| p.replace(on)));
    f()
}

/// Whether the running admission groups twin heap scans (`grouping_relsets`).
pub fn relsets_on() -> bool {
    RELSETS.with(|p| p.get())
}

/// D4569: the admission `f` run with a SELECT's quiet, parameter-free nest over a plan Material copied into the arms of
/// the union below it (arm_nests.rs `per_arm`), so each arm streams; off, such a union keeps its Materialize form.
pub fn copying_union_nests<T>(on: bool, f: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            UNION_NESTS.with(|p| p.set(self.0));
        }
    }
    let _r = Restore(UNION_NESTS.with(|p| p.replace(on)));
    f()
}

/// Whether the running admission copies a SELECT's union nests into the arms (`copying_union_nests`).
pub fn union_nests_on() -> bool {
    UNION_NESTS.with(|p| p.get())
}

/// Whether the statement's reader may stop before the last row at a count no plan node holds and come back for more (a
/// named cursor's FETCH, PostgreSQL's ExecutePlan stopping at the FETCH's count): the walk records it in the graph
/// (`PipelineGraph::partial`), whose content key holds it.
pub fn partial_root() -> bool {
    PARTIAL.with(|p| p.get())
}
