//! A program without a tree or a catalog (expr-compiler.md §2.6): the probe programs build the
//! expression an admission fixed — a `Var`, a placeholder `Const` (the value reaches the code
//! through the caller's hoisted word), a by-value tier-A / tier-B call, AND / OR — as `ExprNode`s.


use mcx::GrowIn;
use gravity_fn::{ArgKind, NullableDatum};
use types_core::Oid;
use types_nodes::primnodes::BoolExprType;
use types_portal::params::ParamBind;

use crate::eval::Source;
use crate::nodes::func::{ArgConv, Call, FuncNode, How};
use crate::nodes::{bool::BoolNode, var::Var, ConstNode, ExprNode, Kids, NodeId, Program, SiteId, State};
use crate::Compiled;

pub struct Direct {
    prog: Program<'static>,
}

impl Direct {
    /// A program built straight, its lists in `m` (memsig E-prog: `Program::empty_in`), which outlives what `finish`
    /// returns (a probe's or a test's context).
    pub fn new_in(m: mcx::Mcx<'static>) -> Direct {
        Direct { prog: Program::empty_in(m) }
    }

    fn push(&mut self, n: ExprNode<'static>) -> NodeId {
        let id = NodeId(self.prog.nodes.len() as u32);
        self.prog.nodes.push_in(n);
        id
    }

    fn kids(&mut self, ids: &[NodeId]) -> Kids {
        let start = self.prog.kids.len() as u32;
        self.prog.kids.extend_from_slice_in(ids);
        Kids { start, len: ids.len() as u32 }
    }

    pub fn var(&mut self, src: Source, attno: i16, typ: Oid) -> NodeId {
        self.push(ExprNode::Var(Var { src, attno, typ, desc: 0 }))
    }

    /// A by-value constant; `value` is the tier-0 value only — `emit` reads the caller's word.
    pub fn konst(&mut self, value: NullableDatum, typ: Oid, typlen: i16) -> NodeId {
        self.push(ExprNode::Const(ConstNode::new(value, typ, typlen, true)))
    }

    /// A strict by-value builtin with a `gravity_fn` shim (tiers A and B); `None` without one.
    pub fn op(&mut self, funcid: Oid, args: &[NodeId]) -> Option<NodeId> {
        let shim = gravity_fn::shim_of(funcid)?;
        if shim.nargs as usize != args.len() || !shim.tyargs.is_empty() || shim.args.iter().any(|k| !matches!(k, ArgKind::V | ArgKind::D)) {
            return None;
        }
        let convs = self.prog.convs.len() as u32;
        for &k in shim.args {
            self.prog.convs.push_in(ArgConv { kind: k, typlen: 0 });
        }
        let kids = self.kids(args);
        let site = SiteId(self.prog.sites.len() as u32);
        self.prog.sites.push_in(Call { funcid, args: kids, nargs: args.len() as u8, strict: true, collation: 0, how: How::Shim { shim, convs, byref: shim.region }, rettype: 0, volatile: false });
        Some(self.push(ExprNode::Op(FuncNode { site })))
    }

    /// A strict builtin with a `gravity_fn` shim over any argument kinds (a by-reference argument
    /// takes its `typlen`), with the plan's result type; `None` without a shim of that arity.
    pub fn call(&mut self, funcid: Oid, args: &[NodeId], typlens: &[i16], rettype: Oid, collation: Oid) -> Option<NodeId> {
        let shim = gravity_fn::shim_of(funcid)?;
        if shim.nargs as usize != args.len() || !shim.tyargs.is_empty() || typlens.len() != args.len() {
            return None;
        }
        let convs = self.prog.convs.len() as u32;
        for (i, &k) in shim.args.iter().enumerate() {
            self.prog.convs.push_in(ArgConv { kind: k, typlen: typlens[i] });
        }
        let kids = self.kids(args);
        let site = SiteId(self.prog.sites.len() as u32);
        self.prog.sites.push_in(Call { funcid, args: kids, nargs: args.len() as u8, strict: true, collation, how: How::Shim { shim, convs, byref: shim.region }, rettype, volatile: false });
        Some(self.push(ExprNode::Op(FuncNode { site })))
    }

    /// A by-reference constant (a text image the tier-0 fill reads; a placeholder for the row form).
    pub fn konst_byref(&mut self, value: NullableDatum, typ: Oid, typlen: i16) -> NodeId {
        self.push(ExprNode::Const(ConstNode::new(value, typ, typlen, false)))
    }

    /// The owning node's aggregate cell `aggno` of the plan's result type (expr-compiler.md §9).
    pub fn aggref(&mut self, aggno: u32, typ: Oid) -> NodeId {
        self.prog.naggs = self.prog.naggs.max(aggno + 1);
        self.push(ExprNode::Aggref(crate::nodes::agg::Aggref { aggno: aggno as i32, typ }))
    }

    pub fn and(&mut self, args: &[NodeId]) -> NodeId {
        let kids = self.kids(args);
        self.push(ExprNode::Bool(BoolNode { op: BoolExprType::AND_EXPR, args: kids }))
    }

    pub fn or(&mut self, args: &[NodeId]) -> NodeId {
        let kids = self.kids(args);
        self.push(ExprNode::Bool(BoolNode { op: BoolExprType::OR_EXPR, args: kids }))
    }

    pub fn finish(self, root: NodeId, result_type: Oid) -> Compiled<'static> {
        Compiled { prog: self.prog, state: State::empty(), root, params: ParamBind::NONE, callback_arg: core::ptr::null_mut(), result_type }
    }
}
