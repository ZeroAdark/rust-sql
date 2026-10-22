//! Every expression node's output (lowerv3 FACTS, Michael 09-23): its Rep from the result type's storage (a bare Var or
//! Relabel over a lane keeps the lane's, D207), its life from the lane, the constant, the record or the fmgr registry.
use gravity_program::{ArenaRef, ArenaVec, ExprLife, ExprOut, FnLife, LaneId, LaneLife, Rep, SiteDesc};
use types_nodes::primnodes::{INDEX_VAR, INNER_VAR, OUTER_VAR};
use types_nodes::list::NodeList;
use types_nodes::{Node, NodeTag};

use crate::catalog::Facts;
use crate::graph::{Op, PipelineGraph, PipelineNode, Source};
use crate::join::JoinDesc;
use crate::{expr, catalog::typefacts};

struct Row<'a, 'p> {
    scan: Option<(u16, &'a SiteDesc)>,
    breaker: bool,
    outer: ArenaVec<Option<ExprOut>>,
    _p: core::marker::PhantomData<&'p ()>,
}

struct Cx<'a> {
    facts: &'a dyn Facts,
    arena: ArenaRef,
    /// every node's output, in the order first met (`annotate` files it sorted by address)
    memo: ArenaVec<(usize, Option<ExprOut>)>,
    /// D3615 (a): each node's place in `memo` by address, an open-addressing table in the arena (a memo place + 1 per slot,
    /// 0 empty; at most half full). The memo was kept sorted, and inserting into it moved its tail: a quarter of an
    /// admission at 4000 partitions went to that memmove.
    at: ArenaVec<u32>,
}

impl<'a> Cx<'a> {
    fn new(facts: &'a dyn Facts, arena: ArenaRef) -> Cx<'a> {
        Cx { facts, arena, memo: ArenaVec::new_in(arena), at: ArenaVec::new_in(arena) }
    }

    /// The first slot `a` probes in a table of `len` slots (a power of two, at least 64).
    fn home(a: usize, len: usize) -> usize {
        gravity_abi::hash::phi_top((a >> 3) as u64, len.trailing_zeros()) as usize
    }

    /// `n`'s place in the memo.
    fn find(&self, a: usize) -> Option<usize> {
        if self.at.is_empty() {
            return None;
        }
        let mask = self.at.len() - 1;
        let mut i = Self::home(a, self.at.len());
        loop {
            match self.at[i] {
                0 => return None,
                p if self.memo[p as usize - 1].0 == a => return Some(p as usize - 1),
                _ => i = (i + 1) & mask,
            }
        }
    }

    fn place(at: &mut [u32], a: usize, k: usize) {
        let mask = at.len() - 1;
        let mut i = Self::home(a, at.len());
        while at[i] != 0 {
            i = (i + 1) & mask;
        }
        at[i] = k as u32 + 1;
    }

    /// Files `a`'s output (an address not in the memo).
    fn add(&mut self, a: usize, o: Option<ExprOut>) {
        self.memo.push((a, o));
        let n = self.memo.len();
        if n * 2 > self.at.len() {
            // a table twice the size, every entry placed again (the arena keeps the old one until the lowering ends)
            let len = (n * 4).next_power_of_two().max(64);
            let mut at = ArenaVec::with_capacity_in(len, self.arena);
            at.resize(len, 0u32);
            for (k, &(x, _)) in self.memo.iter().enumerate() {
                Self::place(&mut at, x, k);
            }
            self.at = at;
        } else {
            Self::place(&mut self.at, a, n - 1);
        }
    }
}

fn addr(n: Node<'_>) -> usize {
    n.as_raw().as_ptr() as usize
}

fn storage(cx: &Cx<'_>, t: u32) -> Option<(Rep, bool)> {
    let r = typefacts::facts_of(cx.facts, t)?;
    Some((Rep::of_storage(r.typlen, r.byval), r.byval))
}

fn value_or(byval: bool, life: ExprLife) -> ExprLife {
    if byval { ExprLife::Lane(LaneLife::VALUE) } else { life }
}

fn var_out(cx: &Cx<'_>, row: &Row<'_, '_>, n: Node<'_>) -> Option<ExprOut> {
    let v = n.as_var()?;
    let (rep, byval) = storage(cx, v.vartype)?;
    let mk = |life| Some(ExprOut { rep, typid: v.vartype, life: value_or(byval, life) });
    if v.varlevelsup != 0 {
        return mk(ExprLife::Row);
    }
    match v.varno {
        OUTER_VAR if !row.outer.is_empty() => row.outer.get((v.varattno as usize).checked_sub(1)?).copied().flatten().map(|o| ExprOut { typid: v.vartype, ..o }),
        OUTER_VAR if row.breaker => mk(ExprLife::Lane(LaneLife::BREAKER)),
        INNER_VAR => mk(ExprLife::Lane(LaneLife::BREAKER)),
        INDEX_VAR => mk(ExprLife::Lane(LaneLife::PAGE)),
        r => match row.scan.filter(|(rti, _)| *rti as i32 == r).and_then(|(_, s)| s.lanes().iter().find(|l| l.id == LaneId::Attr(v.varattno))) {
            Some(l) => Some(ExprOut { rep: l.rep, typid: v.vartype, life: ExprLife::Lane(l.life) }),
            None => mk(ExprLife::Row),
        },
    }
}

fn call_life<'p>(cx: &mut Cx<'_>, row: &Row<'_, 'p>, proc: u32, byval: bool, args: Option<&NodeList<'p>>) -> ExprLife {
    match cx.facts.result_life(proc, byval) {
        FnLife::Value => ExprLife::Lane(LaneLife::VALUE),
        FnLife::Call => ExprLife::Call { proc },
        FnLife::Row => ExprLife::Row,
        FnLife::Arg(k) => args.and_then(|l| l.iter().nth(k as usize)).and_then(|a| out(cx, row, a)).map_or(ExprLife::Call { proc }, |o| o.life),
    }
}

/// `n`'s output (and every node under it), recorded in the graph's table.
fn out<'p>(cx: &mut Cx<'_>, row: &Row<'_, 'p>, n: Node<'p>) -> Option<ExprOut> {
    crate::walk::stack_check(); // D3748
    if let Some(i) = cx.find(addr(n)) {
        return cx.memo[i].1;
    }
    let mut first: Option<Option<ExprOut>> = None;
    if !matches!(n.node_tag(), NodeTag::T_Aggref | NodeTag::T_FuncExpr | NodeTag::T_OpExpr) {
        expr::kids(n, &mut |k| {
            let o = out(cx, row, k);
            first.get_or_insert(o);
        });
    }
    let t = crate::catalog::expr_type::type_of(n);
    let o = match n.node_tag() {
        NodeTag::T_Var => var_out(cx, row, n),
        NodeTag::T_TargetEntry | NodeTag::T_RelabelType | NodeTag::T_CollateExpr => first.flatten().map(|o| ExprOut { typid: t.unwrap_or(o.typid), ..o }),
        NodeTag::T_Const => storage(cx, t?).map(|(rep, byval)| ExprOut { rep, typid: t.unwrap(), life: value_or(byval, ExprLife::Lane(LaneLife::IMMEDIATE)) }),
        NodeTag::T_FuncExpr | NodeTag::T_OpExpr => {
            let (proc, args) = match n.as_func_expr() {
                Some(f) => (f.funcid, &f.args),
                None => (n.as_op_expr()?.opfuncid, &n.as_op_expr()?.args),
            };
            for a in args.iter() {
                out(cx, row, a);
            }
            let (rep, byval) = storage(cx, t?)?;
            Some(ExprOut { rep, typid: t?, life: value_or(byval, call_life(cx, row, proc, byval, Some(args))) })
        }
        NodeTag::T_CoerceViaIO => {
            let (rep, byval) = storage(cx, t?)?;
            let input = typefacts::facts_of(cx.facts, t?).map_or(0, |r| r.input);
            Some(ExprOut { rep, typid: t?, life: value_or(byval, call_life(cx, row, input, byval, None)) })
        }
        NodeTag::T_CaseExpr | NodeTag::T_CoalesceExpr | NodeTag::T_MinMaxExpr => {
            let (rep, byval) = storage(cx, t?)?;
            let life = branch_life(cx, row, n).unwrap_or(ExprLife::Call { proc: 0 });
            Some(ExprOut { rep, typid: t?, life: value_or(byval, life) })
        }
        // a by-reference final lasts one call (PG's lasts one output tuple; lowerv4 renders it into host_final's reused
        // slot): a reader keeping it longer (a join build, a sort or top record) copies it (D323)
        NodeTag::T_Aggref => {
            let proc = n.as_aggref().map_or(0, |a| a.aggfnoid);
            storage(cx, t?).map(|(rep, byval)| ExprOut { rep, typid: t.unwrap(), life: value_or(byval, ExprLife::Call { proc }) })
        }
        NodeTag::T_Param => storage(cx, t?).map(|(rep, byval)| ExprOut { rep, typid: t.unwrap(), life: value_or(byval, ExprLife::Row) }),
        _ => storage(cx, t?).map(|(rep, byval)| ExprOut { rep, typid: t.unwrap(), life: value_or(byval, ExprLife::Call { proc: 0 }) }),
    };
    match cx.find(addr(n)) {
        Some(i) => cx.memo[i].1 = o,
        None => cx.add(addr(n), o),
    }
    o
}

/// A CASE / COALESCE / GREATEST result is one of its branches: the shortest of their lives.
fn branch_life<'p>(cx: &mut Cx<'_>, row: &Row<'_, 'p>, n: Node<'p>) -> Option<ExprLife> {
    let mut life: Option<ExprLife> = None;
    let mut take = |o: Option<ExprOut>| {
        let l = o.map_or(ExprLife::Call { proc: 0 }, |o| o.life);
        life = Some(life.map_or(l, |x| x.min(l)));
    };
    if let Some(c) = n.as_case_expr() {
        for w in c.args.iter() {
            take(w.as_case_when().and_then(|w| w.result).and_then(|r| out(cx, row, r)));
        }
        take(c.defresult.and_then(|d| out(cx, row, d)));
    } else {
        expr::kids(n, &mut |k| take(out(cx, row, k)));
    }
    life
}

fn list<'p>(cx: &mut Cx<'_>, row: &Row<'_, 'p>, l: &types_nodes::list::NodeList<'p>) {
    for n in l.iter() {
        out(cx, row, n);
    }
}

/// FACTS-1b F6 (`ParamRep`): with `params`, the pass records each param a nest or per-row SubPlan binds, read on the row
/// the owner's op sees, and walks the nested SubPlans' inners too; without it (`annotate`) neither.
type Params = ArenaVec<(i32, crate::graph::ParamRep)>;

fn param<'p>(cx: &mut Cx<'_>, row: &Row<'_, 'p>, id: i32, n: Node<'p>, owner: crate::graph::ParamOwner) -> (i32, crate::graph::ParamRep) {
    let typmod = match n.node_tag() {
        NodeTag::T_Var => n.as_var().map_or(-1, |v| v.vartypmod),
        NodeTag::T_Const => n.as_const().map_or(-1, |c| c.consttypmod),
        NodeTag::T_Param => n.as_param().map_or(-1, |p| p.paramtypmod),
        NodeTag::T_RelabelType => n.as_relabel_type().map_or(-1, |r| r.resulttypmod),
        _ => -1,
    };
    let rep = crate::graph::ParamRep { owner, typid: crate::catalog::expr_type::type_of(n).unwrap_or(0), typmod, coll: crate::catalog::expr_type::collation_of(n), out: out(cx, row, n) };
    (id, rep)
}

fn pipeline<'p>(cx: &mut Cx<'_>, g: &PipelineGraph<'p>, i: Option<usize>, p: &PipelineNode<'p>, mut params: Option<&mut Params>) {
    crate::walk::stack_check(); // D3748
    let mut row = Row { scan: p.source.relation().map(|(rti, _, s)| (rti, s)), breaker: matches!(p.source, Source::Breaker { .. }), outer: ArenaVec::new_in(cx.arena), _p: core::marker::PhantomData };
    let mut drained = i.is_some_and(|i| drains_sort(g, i, p));
    for op in p.ops.iter() {
        match op {
            Op::Filter(n) | Op::Gate(n) => {
                out(cx, &row, *n);
            }
            Op::Project(l) => {
                let mut next = ArenaVec::with_capacity_in(l.len(), cx.arena);
                for n in l.iter() {
                    let o = out(cx, &row, n);
                    next.push(if drained { o.map(var_copy) } else { o });
                }
                row.outer = next;
                drained = false;
            }
            Op::Sort(s) => list(cx, &row, s.tlist),
            Op::Join(JoinDesc::Build { keys, .. } | JoinDesc::Memo { keys, .. }) => list(cx, &row, keys),
            Op::Join(JoinDesc::Probe { keys, clauses, qual, .. }) => {
                list(cx, &row, keys);
                list(cx, &row, clauses);
                list(cx, &row, qual);
            }
            Op::Join(JoinDesc::Merge { clauses, qual, .. }) => {
                list(cx, &row, clauses);
                list(cx, &row, qual);
            }
            Op::Join(JoinDesc::Nest { inner, qual, params: np, facts, .. }) => {
                list(cx, &row, qual);
                if let Some(sink) = params.as_deref_mut() {
                    for x in np.iter().filter_map(|x| x.as_nest_loop_param()) {
                        sink.push(param(cx, &row, x.paramno, x.paramval, crate::graph::ParamOwner::Nest { node: facts.node }));
                    }
                }
                pipeline(cx, g, None, inner, params.as_deref_mut());
            }
            Op::SubPlan(d) if params.is_some() => {
                let sink = params.as_deref_mut().unwrap();
                for (id, a) in d.params.iter().zip(d.args.iter()) {
                    sink.push(param(cx, &row, id, a, crate::graph::ParamOwner::SubPlan { plan_id: d.plan_id }));
                }
                if let crate::ops::shared::subplan::SubForm::Nested { inner } = &d.form {
                    pipeline(cx, g, None, inner, params.as_deref_mut());
                }
            }
            // D1071: a streamed group reads its arguments and keys over the row here (a breaker's group does at its
            // producer's end), and hands on its group rows, read as an emit over a group's record reads them
            Op::GroupRun(s) => {
                if let Some(gd) = g.streams.get(*s as usize) {
                    group_outs(cx, &row, gd);
                }
                row = Row { scan: None, breaker: true, outer: ArenaVec::new_in(cx.arena), _p: core::marker::PhantomData };
            }
            // past a streamed union's marker the row is the union's, read as an emit over its slot read it
            Op::Append(crate::ops::shared::append::AppendDesc::Arm { .. }) => {
                row = Row { scan: None, breaker: true, outer: ArenaVec::new_in(cx.arena), _p: core::marker::PhantomData };
            }
            _ => {}
        }
    }
    let Some(i) = i else { return };
    for gd in g.groups.iter().filter(|gd| gd.producer.0 as usize == i) {
        group_outs(cx, &row, gd);
    }
}

/// D3841: whether pipeline `i` drains a sort run: its breaker source's producer (the pipeline lowerv4's `Gx::life_of`
/// names: its consumed life, else the nearest earlier pipeline ending in the slot) holds a Sort op. The run hands each
/// varlena column on as its var copy (lowerv4 face/runs.rs `Form::Var`; cell.rs `source_rep` reads it so), while the
/// sort's target list, which the drain's first projection re-reads, is filed in the memo over the fill's row: a pgrc
/// packed lane's numeric there stayed `PackedNum` past the drain, so a later top sort gave the column no tail and its
/// varlena cell no exact word (keylane `pairs`).
fn drains_sort(g: &PipelineGraph<'_>, i: usize, p: &PipelineNode<'_>) -> bool {
    let Source::Breaker { slot } = p.source else { return false };
    let d = &p.desc;
    let from = d.consumes.iter().position(|&s| s == slot).and_then(|j| d.consumes_from.get(j)).map(|l| l.0 as usize);
    let from = from.or_else(|| (0..i.min(g.pipelines.len())).rev().find(|&q| g.pipelines[q].desc.produces() == Some(slot)));
    from.and_then(|q| g.pipelines.get(q)).is_some_and(|q| q.ops.iter().any(|op| matches!(op, Op::Sort(_))))
}

/// A drained sort run's column as the drain hands it (`drains_sort`): a packed numeric lane's value as its varlena copy.
fn var_copy(o: ExprOut) -> ExprOut {
    match o.rep {
        Rep::PackedNum { .. } => ExprOut { rep: Rep::VarlenaPtr, life: ExprLife::Lane(LaneLife::BREAKER), ..o },
        _ => o,
    }
}

/// Group `gd`'s aggregate arguments and its keys that are expressions, over the row that feeds it.
fn group_outs<'p>(cx: &mut Cx<'_>, row: &Row<'_, 'p>, gd: &crate::ops::shared::agg::GroupDecl<'p>) {
    for a in gd.aggs.iter() {
        expr::kids(a.node, &mut |k| {
            out(cx, row, k);
        });
    }
    for k in gd.keys.iter().filter(|k| k.expr.agg()) {
        out(cx, row, k.expr.node());
    }
}

/// Annotate every pipeline's expressions; the table is sorted by node address for `PipelineGraph::out_of`.
pub fn annotate(g: &mut PipelineGraph<'_>, facts: &dyn Facts) {
    let arena = *g.outs.allocator();
    let mut cx = Cx::new(facts, arena);
    for (i, p) in g.pipelines.iter().enumerate() {
        pipeline(&mut cx, g, Some(i), p, None);
    }
    g.outs.clear();
    g.outs.extend(cx.memo.iter().filter_map(|&(a, o)| Some((a, o?))));
    // the table is read by address (`PipelineGraph::out_of`); every address is met once
    g.outs.sort_unstable_by_key(|&(a, _)| a);
}

/// FACTS-1b F6: `PipelineGraph::params`, every PARAM_EXEC id a nest (`nestParams`) or a per-row SubPlan (`args`) binds with
/// the output of the expression its owner writes, in its own pass (`g.outs` stays what `annotate` made).
pub fn param_reps(g: &mut PipelineGraph<'_>, facts: &dyn Facts) {
    let arena = *g.outs.allocator();
    let mut cx = Cx::new(facts, arena);
    let mut sink: Params = ArenaVec::new_in(arena); // alloc: Cold
    for (i, p) in g.pipelines.iter().enumerate() {
        pipeline(&mut cx, g, Some(i), p, Some(&mut sink));
    }
    g.params = sink;
}
