use core::fmt::{self, Write};

use gravity_program::{ArenaVec, End, Pipeline, Reason};
use types_nodes::list::{NodeList, OidList};
use types_nodes::plannodes::{FunctionScan, ProjectSet};
use types_nodes::{Node, NodeTag};

use crate::expr;
use crate::graph::{Op, Source};
use crate::ledger::Kind;
use crate::ops;
use crate::admit::Refusal;
use crate::walk::{Builder, Open, STMT_NODE};
use crate::ops::shared::srf::{SrfDesc, SrfFn, SrfKind, SrfPlace};

/// The emitter's hook (`window::FrameEmitter`'s twin): called once per function with the loop's desc;
/// the body it emits is the library's open-coded iterator for `SrfFn.kind`, never an fmgr call.
pub trait SrfEmitter<'p> {
    fn function(&mut self, d: &SrfDesc<'p>, f: &SrfFn<'p>);
}

impl<'p> SrfDesc<'p> {
    pub fn emit(&self, e: &mut dyn SrfEmitter<'p>) {
        for f in self.functions.iter() {
            e.function(self, f);
        }
    }
}

/// The kind a call of `funcid` lowers as: the catalog home's table of the builtin set functions (catalog/srf.rs).
pub use crate::catalog::srf::kind_of;

fn srf_of<'p>(n: Node<'p>, resno: i16, cols: u16, coltypes: &'p OidList<'p>, k: Kind, id: i32) -> Result<Option<SrfFn<'p>>, Refusal> {
    match n.node_tag() {
        NodeTag::T_FuncExpr => {
            let f = n.as_func_expr().unwrap();
            let kind = kind_of(f.funcid, f.funcretset);
            if f.args.iter().any(has_srf) {
                return Err(Refusal::shape(k, id, "a set-returning function in an argument"));
            }
            Ok(Some(SrfFn { resno, node: n, funcid: f.funcid, args: &f.args, kind, cols, coltypes }))
        }
        NodeTag::T_OpExpr if n.as_op_expr().unwrap().opretset => {
            let o = n.as_op_expr().unwrap();
            if o.args.iter().any(has_srf) {
                return Err(Refusal::shape(k, id, "a set-returning function in an argument"));
            }
            Ok(Some(SrfFn { resno, node: n, funcid: o.opfuncid, args: &o.args, kind: kind_of(o.opfuncid, true), cols, coltypes }))
        }
        _ => Ok(None),
    }
}

fn has_srf(n: Node<'_>) -> bool {
    let mut found = false;
    expr::walk(n, &mut |x| {
        found |= x.as_func_expr().is_some_and(|f| f.funcretset) || x.as_op_expr().is_some_and(|o| o.opretset);
        !found
    });
    found
}

fn map_project_set<'p>(b: &mut Builder<'p, '_>, ps: &'p ProjectSet<'p>) -> Result<Open<'p>, Refusal> {
    let (plan, k) = (&ps.plan, Kind::ProjectSet);
    let id = plan.plan_node_id;
    if !plan.qual.is_nil() {
        return Err(Refusal::shape(k, id, "a qual on ProjectSet"));
    }
    let mut open = b.child(plan, k)?;
    let mut functions = ArenaVec::new_in(b.arena);
    for te in plan.targetlist.iter() {
        let Some(te) = te.as_target_entry() else { continue };
        match srf_of(te.expr, te.resno, 1, &Nil::<'p>::OIDS, k, id)? {
            Some(f) if f.kind != SrfKind::Scalar => functions.push(f),
            _ if has_srf(te.expr) => return Err(Refusal::shape(k, id, "a set-returning function below the top of a target entry")),
            _ => {}
        }
    }
    if functions.is_empty() {
        return Err(Refusal::shape(k, id, "a ProjectSet without a set-returning function"));
    }
    open.push(Op::Srf(SrfDesc { place: SrfPlace::TargetList, functions, tlist: &plan.targetlist }));
    ops::project(&mut open, &plan.targetlist);
    b.graph.note(k);
    Ok(open)
}

/// The empty argument list of a `SrfKind::Expr` entry, at the plan's lifetime.
struct Nil<'p>(core::marker::PhantomData<&'p ()>);

impl<'p> Nil<'p> {
    const LIST: NodeList<'p> = NodeList::nil();
    const OIDS: OidList<'p> = OidList::nil();
}

fn map_function_scan<'p>(b: &mut Builder<'p, '_>, fs: &'p FunctionScan<'p>) -> Result<Open<'p>, Refusal> {
    let (plan, k) = (&fs.scan.plan, Kind::FunctionScan);
    let id = plan.plan_node_id;
    let mut functions = ArenaVec::new_in(b.arena);
    let mut col = 1i16;
    for f in fs.functions.iter() {
        let rtf = f.as_range_tbl_function().ok_or(Refusal::shape(k, id, "a functions entry that is not a RangeTblFunction"))?;
        let e = rtf.funcexpr.ok_or(Refusal::shape(k, id, "a range-table function without an expression"))?;
        let cols = rtf.funccolcount.max(1) as u16;
        if has_srf(e) && e.node_tag() != NodeTag::T_FuncExpr {
            return Err(Refusal::shape(k, id, "a set-returning function below the top of a FROM-clause expression"));
        }
        let f = srf_of(e, col, cols, &rtf.funccoltypes, k, id)?.unwrap_or(SrfFn { resno: col, node: e, funcid: 0, args: &Nil::<'p>::LIST, kind: SrfKind::Expr, cols, coltypes: &rtf.funccoltypes });
        functions.push(f);
        col += cols as i16;
    }
    if functions.is_empty() {
        return Err(Refusal::shape(k, id, "a FunctionScan without functions"));
    }
    crate::ops::shared::record_row::function_record_row(b, fs.scan.scanrelid, [&plan.targetlist, &plan.qual], &fs.functions, fs.funcordinality);
    let mut open = Open::new(b.arena, id, Source::Const, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite));
    open.push(Op::Srf(SrfDesc { place: SrfPlace::From { scanrelid: fs.scan.scanrelid as u16, ordinality: fs.funcordinality }, functions, tlist: &plan.targetlist }));
    ops::filters(&mut open, &plan.qual);
    ops::project(&mut open, &plan.targetlist);
    b.graph.note(k);
    Ok(open)
}

pub fn map<'p>(b: &mut Builder<'p, '_>, node: Node<'p>, k: Kind) -> Result<Open<'p>, Refusal> {
    match k {
        Kind::ProjectSet => map_project_set(b, node.as_project_set().unwrap()),
        Kind::FunctionScan => map_function_scan(b, node.as_function_scan().unwrap()),
        _ => Err(Refusal::shape(k, STMT_NODE, "not an SRF node")),
    }
}

impl SrfDesc<'_> {
    pub fn dump(&self, out: &mut dyn Write) -> fmt::Result {
        match self.place {
            SrfPlace::TargetList => write!(out, "  srf tlist")?,
            SrfPlace::From { scanrelid, ordinality } => write!(out, "  srf from rti={scanrelid}{}", if ordinality { " ordinality" } else { "" })?,
        }
        for f in self.functions.iter() {
            write!(out, " [{}:{:?} fn{}", f.resno, f.kind, f.funcid)?;
            if f.cols > 1 {
                write!(out, " cols={}", f.cols)?;
            }
            write!(out, " ")?;
            expr::print(f.node, out)?;
            write!(out, "]")?;
        }
        Ok(())
    }
}
