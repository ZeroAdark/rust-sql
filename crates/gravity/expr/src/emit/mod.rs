//! The IR form (docs/gravity/expr-compiler.md §2): `Compiled::emit` appends the expression's per-row form to a
//! `FunctionBuilder` — one `I64` datum word, a `Bool` validity, an `I64` raise mask (abi `status::raise`; 0 = no
//! error) — over the lowerer's row references; `emit_word` the 64-row vector form. Literals never fold.

pub mod agg;
mod arrayexpr;
pub use prim::raise_status;
pub mod build;
mod field;
mod fuse;
pub mod hash;
pub mod host;
pub mod jsonb;
pub mod percode;
pub mod prim;
mod row;
pub(crate) mod store;
pub mod scalar;
mod subscript;
pub mod vec;
mod whole;
pub mod word;

use gravity_fn::{NullableDatum, Region};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;
use mcx::{GrowIn, Mcx, PgVec};
use types_core::Oid;
use types_error::PgResult;
use types_nodes::primnodes::ParamKind;

use crate::nodes::{array, ExprNode, Kind, NodeId, Program, SiteId};
use crate::Compiled;

pub use agg::{AggCell, CellForm, CellValid};
pub use host::{cell_size, cell_width, Cell, EmitState};
pub use percode::{CodeRef, TableRef};

use crate::percode::{CodeCol, Plan as PerCodePlan, TableNeed};

/// A lane's window-invariant words as the lowerer loaded them (`frame2::lane_in`'s shape).
#[derive(Clone, Copy, Debug)]
pub struct LaneRef {
    pub ptr: Value,
    pub valid: Value,
    pub valid_null: Option<Value>,
    pub width: u8,
    pub kind: u8,
    pub all_valid: bool,
}

#[derive(Clone, Copy)]
pub enum ColsRef<'a> {
    None,
    Slot { values: Value, nulls: Value },
    Values { values: &'a [Value], valids: &'a [Value] },
    Lanes { row: Value, lanes: &'a [Option<LaneRef>] },
    Record { base: Value, offsets: &'a [u32] },
    RecordNulls { base: Value, offsets: &'a [u32], null_off: u32, bits: &'a [u8] },
}

pub const NULL_BIT_NONE: u8 = 0xff;

/// The whole-row reference's source (`Var` attno 0, emit/whole.rs): the slot's values / nulls and the rowtype's descriptor pointer word (the owner's fact, never in the content key; the slot already checked).
#[derive(Clone, Copy)]
pub struct WholeRef {
    pub values: Value,
    pub nulls: Value,
    pub desc: Value,
}

/// What the emitted code reads: the sources (`Record`: a breaker record's key words, always valid), the constants, the region, the call cells, the record and its aggregate cells (§9).
#[derive(Clone, Copy)]
pub struct RowRefs<'a> {
    pub scan: ColsRef<'a>,
    pub inner: ColsRef<'a>,
    pub outer: ColsRef<'a>,
    pub consts: &'a [Value],
    pub consts_null: &'a [Value],
    pub region: Option<Value>,
    pub cells: Option<Value>,
    /// The shredded jsonb path lanes (§2.7), by `(column, path)`.
    pub shreds: &'a [jsonb::ShredRef],
    pub codes: &'a [CodeRef],
    pub tables: &'a [TableRef],
    pub rec: Option<Value>,
    pub aggs: &'a [AggCell],
    /// A numeric `extract` at the root is read as its integer word (the key form, §2.10).
    pub word_root: bool,
    /// The whole-row reference's source; `None` refuses `Var(0)` (`EmitError::NoColumn(0)`).
    pub whole: Option<WholeRef>,
    pub ctx: Option<Value>,
    /// GV-D1: the raise words of scan columns the caller evaluated ahead of the expression (a claimed site's placeholder),
    /// by attno: the column's read carries it, so it takes its place in PG's order and a guard PG never passes masks it.
    pub col_raises: &'a [(i16, Value)],
    /// V4R-342 (D945): the scan columns the caller emits at their read instead (`LazySites`).
    pub lazy: Option<&'a dyn LazySites>,
    /// D3033: the operands the caller computed once ahead of the row loop (`Compiled::late_roots`, at its hoist scope), by
    /// node, `[value, valid (Bool), err]`: the row reads them in place of the subtree, so a held raise surfaces only where
    /// the row reaches the operand (PG raises there).
    pub late: &'a [(NodeId, [Value; 3])],
    /// The emit's temporaries (its parameter plan, its per-code plan, its walks' lists): the planning context the lowerer
    /// passes, freed with each list.
    pub scratch: Mcx<'a>,
}

/// V4R-342 (D945): claimed sites the caller emits where the expression reads them, never ahead of it: a scan `Var` of
/// `has(attno)` is `emit(b, attno)` at its read and a node over it calls a host (PG evaluates it only where it is reached).
pub trait LazySites {
    fn has(&self, attno: i16) -> bool;
    fn emit(&self, b: &mut FunctionBuilder<'_>, attno: i16) -> [Value; 3];
    /// M8 B1 (nest.initplan): executor parameter `id` is emitted where the expression reads it (`emit_param`): an
    /// InitPlan's result, which PG computes at its Param's first evaluation (`ExecEvalParamExec`), never ahead of it.
    fn has_param(&self, _id: i32) -> bool {
        false
    }
    fn emit_param(&self, _b: &mut FunctionBuilder<'_>, id: i32) -> [Value; 3] {
        unreachable!("a lazy parameter {id} the hook does not hold")
    }
    /// M8 B2 (nest.subplan): SubPlan `plan_id`'s value is the lowerer's (its step wrote it for this row), read here.
    fn has_subplan(&self, _plan_id: i32) -> bool {
        false
    }
    fn emit_subplan(&self, _b: &mut FunctionBuilder<'_>, plan_id: i32) -> [Value; 3] {
        unreachable!("a SubPlan {plan_id} the hook does not hold")
    }
    /// D3374: SubPlan `plan_id` read as an interpreter island's argument (emit/host.rs `island`). Where the hook holds its
    /// second row, that row comes back as a NULL whose word is `nodes::srf::HELD_ROWS`, and the interpreter raises it only
    /// where it reads the SubPlan (nodes/srf.rs `eval_subplan`), as PG raises it only where it runs it. Else `emit_subplan`.
    fn emit_subplan_held(&self, b: &mut FunctionBuilder<'_>, plan_id: i32) -> [Value; 3] {
        self.emit_subplan(b, plan_id)
    }
    /// D2997: a column only a claim's fallback arm reads (`CASE WHEN hot THEN value ELSE <call> END`) is rendered where
    /// the arm reads it, never ahead of the expression: the rows the claim answers never build it.
    fn has_col(&self, _src: crate::eval::Source, _attno: i16) -> bool {
        false
    }
    fn emit_col(&self, _b: &mut FunctionBuilder<'_>, _src: crate::eval::Source, attno: i16) -> [Value; 3] {
        unreachable!("a lazy column {attno} the hook does not hold")
    }
}

impl<'a> RowRefs<'a> {
    pub fn scan(scan: ColsRef<'a>, consts: &'a [Value], consts_null: &'a [Value], scratch: Mcx<'a>) -> RowRefs<'a> {
        RowRefs { scan, inner: ColsRef::None, outer: ColsRef::None, consts, consts_null, region: None, cells: None, shreds: &[], codes: &[], tables: &[], rec: None, aggs: &[], word_root: false, whole: None, ctx: None, col_raises: &[], lazy: None, late: &[], scratch }
    }

    pub fn record(scan: ColsRef<'a>, consts: &'a [Value], consts_null: &'a [Value], rec: Value, aggs: &'a [AggCell], region: Option<Value>, scratch: Mcx<'a>) -> RowRefs<'a> {
        RowRefs { scan, inner: ColsRef::None, outer: ColsRef::None, consts, consts_null, region, cells: None, shreds: &[], codes: &[], tables: &[], rec: Some(rec), aggs, word_root: false, whole: None, ctx: None, col_raises: &[], lazy: None, late: &[], scratch }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Lowered {
    pub value: Value,
    pub valid: Value,
    pub err: Value,
}

/// A boolean root for a branch (§7.7): `cond` is the `Bool` before its datum word.
#[derive(Clone, Copy, Debug)]
pub struct Pred {
    pub cond: Value,
    pub valid: Value,
    pub err: Value,
}

#[derive(Debug, PartialEq, Eq)]
pub enum EmitError {
    Unsupported(Kind),
    /// A `Var` over a lane the row references do not carry, or over an absent source.
    NoColumn(i16),
    CodesLane(i16),
    /// A tier B / C site without `RowRefs::cells` / `RowRefs::region`.
    NoCells,
    LongList,
    /// The vector form does not admit this expression (the caller keeps the scalar rows).
    NotVector,
    PerCodeByRef,
    NoTable(u32),
    NoAggCell(u32),
}

/// One parameter word the caller supplies (`Compiled::param_needs`, in preorder).
#[derive(Clone, Copy, Debug)]
pub enum ParamNeed {
    Const(NullableDatum),
    Elem(NullableDatum),
    Extern { id: i32, typ: Oid },
    Exec { id: i32, typ: Oid },
    /// D2834: a deferred extern parameter's word (nodes/param.rs `DEFERRED`): the execution's parameter list, which the
    /// emitted code hands its evaluation-time fetch (host.rs `param_deferred`); never the slot's value.
    Deferred { id: i32, typ: Oid },
}

/// The parameter words an emit reads (`plan`), in the emit's scratch (the planning context the caller passes).
pub(crate) struct Plan<'s> {
    pub needs: PgVec<'s, ParamNeed>,
    pub elems: PgVec<'s, (NodeId, u32, u32, bool)>,
    /// The need each node is read as, by node (the need's index + 1; 0: none), sized to the program's `nodes` at the first
    /// need (a plan without one allocates none). D3886: a scan of the needs so far per node walked, and per `index`, was
    /// O(nodes x needs): a generic plan of 65535 parameters spent 0.87 s in the walk.
    at: PgVec<'s, u32>,
    nodes: usize,
}

impl Plan<'_> {
    pub fn index(&self, id: NodeId) -> Option<u32> {
        self.at.get(id.0 as usize).and_then(|&k| k.checked_sub(1))
    }
    /// Node `id` is read as the next need (pushed by the caller right after); a node's first need stands (`index`).
    fn put(&mut self, id: NodeId) {
        if self.at.is_empty() {
            self.at.resize_in(self.nodes, 0);
        }
        let k = self.needs.len() as u32 + 1;
        if let Some(x) = self.at.get_mut(id.0 as usize).filter(|x| **x == 0) {
            *x = k;
        }
    }
    pub fn elems_of(&self, id: NodeId) -> Option<(u32, u32, bool)> {
        self.elems.iter().find(|(n, ..)| *n == id).map(|(_, s, n, null)| (*s, *n, *null))
    }
}

/// The children of `id` in evaluation order (the kinds `emit` serves; none for the rest): a walk over the program's own
/// lists (`kids`), nothing allocated.
pub struct KidIter<'p> {
    head: Option<NodeId>,
    a: core::slice::Iter<'p, NodeId>,
    b: core::slice::Iter<'p, NodeId>,
    tail: Option<NodeId>,
    /// An open slice bound (`NodeId::NONE` in `a` / `b`) is no node.
    skip_none: bool,
}

impl<'p> KidIter<'p> {
    fn of(head: Option<NodeId>, a: &'p [NodeId], b: &'p [NodeId], tail: Option<NodeId>) -> KidIter<'p> {
        KidIter { head, a: a.iter(), b: b.iter(), tail, skip_none: false }
    }

    pub fn none() -> KidIter<'p> {
        KidIter::of(None, &[], &[], None)
    }

    /// The walk again from where this one stands: the same lists re-borrowed.
    fn again(&self) -> KidIter<'p> {
        KidIter { head: self.head, a: self.a.as_slice().iter(), b: self.b.as_slice().iter(), tail: self.tail, skip_none: self.skip_none }
    }

    /// How many children (a count of the walk).
    pub fn count_kids(&self) -> usize {
        self.again().count()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.again().next().is_none()
    }

    /// The children as the program's own list, when they are one (a call's arguments, a BoolExpr's operands, none).
    pub fn slice(&self) -> Option<&'p [NodeId]> {
        (self.head.is_none() && self.b.as_slice().is_empty() && self.tail.is_none() && !self.skip_none).then(|| self.a.as_slice())
    }
}

impl Iterator for KidIter<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        if let Some(h) = self.head.take() {
            return Some(h);
        }
        for &k in self.a.by_ref() {
            if !(self.skip_none && k == NodeId::NONE) {
                return Some(k);
            }
        }
        for &k in self.b.by_ref() {
            if !(self.skip_none && k == NodeId::NONE) {
                return Some(k);
            }
        }
        self.tail.take()
    }
}

/// The children of `id` in evaluation order (the kinds `emit` serves; none for the rest).
pub fn kids<'p>(p: &'p Program<'_>, id: NodeId) -> KidIter<'p> {
    let range = |k: crate::nodes::Kids| &p.kids[k.start as usize..k.start as usize + k.len as usize];
    let one = |k: NodeId| KidIter::of(Some(k), &[], &[], None);
    match &p.nodes[id.0 as usize] {
        ExprNode::Func(n) | ExprNode::Op(n) | ExprNode::Distinct(n) | ExprNode::NullIf(n) => KidIter::of(None, range(p.sites[n.site.0 as usize].args), &[], None),
        ExprNode::ScalarArrayOp(n) => KidIter::of(None, range(p.sites[n.site.0 as usize].args), &[], None),
        ExprNode::Bool(n) => KidIter::of(None, range(n.args), &[], None),
        ExprNode::NullTest(n) => one(n.arg),
        ExprNode::BooleanTest(n) => one(n.arg),
        ExprNode::Case(n) => KidIter::of(n.arg, range(n.whens), &[], n.default),
        ExprNode::Coalesce(n) => KidIter::of(None, range(n.args), &[], None),
        ExprNode::MinMax(n) => KidIter::of(None, range(n.args), &[], None),
        ExprNode::Relabel(n) => one(n.arg),
        ExprNode::CoerceViaIO(n) => one(n.arg),
        ExprNode::CoerceToDomain(n) => KidIter::of(Some(n.arg), range(n.checks), &[], None),
        ExprNode::RowCompare(n) => KidIter::of(None, range(n.largs), range(n.rargs), None),
        ExprNode::FieldSelect(n) => one(n.arg),
        ExprNode::FieldStore(n) => KidIter::of(Some(n.arg), range(n.newvals), &[], None),
        ExprNode::Row(n) => KidIter::of(None, range(n.args), &[], None),
        ExprNode::ConvertRowtype(n) => one(n.arg),
        // never run (NULL), walked so the word plan places its literals (nodes/misc.rs `compile_returning`)
        ExprNode::Returning(n) => one(n.arg),
        ExprNode::Array(n) => KidIter::of(None, range(n.elements), &[], None),
        // an open slice bound is no node
        ExprNode::Subscripting(n) => KidIter { skip_none: true, ..KidIter::of(Some(n.container), range(n.upper), range(n.lower), n.assign) },
        _ => KidIter::none(),
    }
}

pub(crate) fn plan<'s>(p: &Program<'_>, root: NodeId, scratch: Mcx<'s>) -> PgResult<Plan<'s>> {
    let mut out = Plan { needs: PgVec::new_in(scratch), elems: PgVec::new_in(scratch), at: PgVec::new_in(scratch), nodes: p.nodes.len() };
    walk(p, root, &mut out)?;
    Ok(out)
}

fn walk(p: &Program<'_>, id: NodeId, out: &mut Plan<'_>) -> PgResult<()> {
    crate::stack_check();
    // a node the program reads twice is one need (lowerv4 M6 R3: a code lane's arm shares its compare's literal)
    if out.index(id).is_some() {
        return Ok(());
    }
    match &p.nodes[id.0 as usize] {
        ExprNode::Const(c) => {
            out.put(id);
            out.needs.push_in(ParamNeed::Const(c.need()));
        }
        ExprNode::Param(n) if n.kind == ParamKind::PARAM_EXTERN => {
            out.put(id);
            out.needs.push_in(ParamNeed::Extern { id: n.id, typ: n.typ });
        }
        ExprNode::Param(n) if n.kind == ParamKind::PARAM_EXEC => {
            out.put(id);
            out.needs.push_in(ParamNeed::Exec { id: n.id, typ: n.typ });
        }
        ExprNode::Param(n) if crate::nodes::param::is_deferred(n) => {
            out.put(id);
            out.needs.push_in(ParamNeed::Deferred { id: n.id, typ: n.typ });
        }
        ExprNode::ScalarArrayOp(n) => {
            let args = p.sites[n.site.0 as usize].args;
            walk(p, p.kids[args.start as usize], out)?;
            let arr = p.kids[args.start as usize + 1];
            if out.elems.iter().any(|e| e.0 == arr) {
                return Ok(());
            }
            if let ExprNode::Const(c) = &p.nodes[arr.0 as usize] {
                let start = out.needs.len() as u32;
                let mut count = 0;
                if !c.is_null() {
                    let mut scratch = [0u8; 256];
                    let mut region = Region::over(&mut scratch);
                    // the element count shapes the needs, keyed by its class (konst.rs `list_class`, V4R-110): no bake
                    // (the elements are words bound per execution)
                    let mut w = array::Walk::over(c.list().value, n.elem, &mut region)?;
                    while let Some(e) = w.next() {
                        out.needs.push_in(ParamNeed::Elem(e));
                        count += 1;
                    }
                }
                out.elems.push_in((arr, start, count, c.is_null()));
                // P5-02: a list past MAX_LIST (one class whatever its length) is also one word, the array itself, after its
                // elements: a per-code table reads the whole list from it per execution (percode `ConstWord::list`)
                if count as usize > gravity_program::cost::expr::MAX_LIST {
                    out.put(arr);
                    out.needs.push_in(ParamNeed::Const(c.need()));
                }
            } else {
                walk(p, arr, out)?;
            }
        }
        _ => {
            for k in kids(p, id) {
                walk(p, k, out)?;
            }
            // an island's extern parameters are its arguments (nodes/mod.rs `island`): words the emitted code loads per execution
            if let Some(isl) = p.islands.iter().find(|i| i.root == id) {
                let args = p.sites[isl.site.0 as usize].args;
                for &k in &p.kids[args.start as usize..(args.start + args.len) as usize] {
                    if matches!(p.nodes[k.0 as usize], ExprNode::Param(_)) {
                        walk(p, k, out)?;
                    }
                }
            }
        }
    }
    Ok(())
}

impl<'mcx> Compiled<'mcx> {
    /// The parameter words the emitted code reads, in the order of `RowRefs::consts`: the walk's lists in `scratch`, the
    /// needs the lowerer keeps with the program's placement (lowerv4 `Ex::needs`) copied out.
    pub fn param_needs(&self, scratch: Mcx<'_>) -> PgResult<Vec<ParamNeed>> {
        let plan = plan(&self.prog, self.root, scratch)?;
        let mut needs = Vec::new(); // alloc: Program
        for &n in plan.needs.iter() {
            needs.push(n); // alloc: Program
        }
        Ok(needs)
    }

    /// How many parameter words the emitted code reads (`param_needs`'s count), its walk's lists in `scratch`.
    pub fn param_count(&self, scratch: Mcx<'_>) -> PgResult<usize> {
        Ok(plan(&self.prog, self.root, scratch)?.needs.len())
    }

    /// NEW-72 entry C: the index in `param_needs` of the word a bound table's constant `w` reads (percode `ConstWord`):
    /// Const `w.node`'s, or element `w.elem` of the array Const `w.node`'s (`ParamNeed::Elem`). `None` when the program
    /// reads no such word (a NULL array, an element past its count).
    pub fn const_need(&self, w: crate::percode::ConstWord, scratch: Mcx<'_>) -> Option<u32> {
        let p = plan(&self.prog, self.root, scratch).ok()?;
        match w.elem {
            None => p.index(w.node),
            Some(k) => p.elems_of(w.node).filter(|&(_, n, null)| !null && k < n).map(|(start, _, _)| start + k),
        }
    }

    /// Whether `emit` serves the program (a refused kind, a call tier), before any IR is built.
    pub fn emit_check(&self) -> Result<(), EmitError> {
        scalar::check(&self.prog, self.root, false)
    }

    /// `emit_check` for a caller that supplies `RowRefs::whole`: a whole-row `Var` is served (a system column is not).
    pub fn emit_check_whole(&self) -> Result<(), EmitError> {
        scalar::check(&self.prog, self.root, true)
    }

    /// The tier B and C sites the emitted code calls per row (the lowerer's admission fact), and the array walks over a
    /// tier-A site (`scalar::walked`: the walk is one host call through the site's cell whatever its tier).
    pub fn host_calls(&self) -> usize {
        self.prog.sites.iter().filter(|s| !prim::is_prim(&self.prog, s, false)).count() + self.prim_walks()
    }

    pub fn host_calls_root(&self, word_root: bool) -> usize {
        let site = |id: NodeId| match self.node(id) {
            ExprNode::Func(n) | ExprNode::Op(n) => Some(n.site),
            _ => None,
        };
        // B2-31: under a root integer cast the word is its argument's, and the cast itself is never called
        let (at, cast) = prim::datetime::word_node(&self.prog, self.root);
        let (root, skipped) = (site(at), if word_root && cast { site(self.root) } else { None });
        self.prog.sites.iter().enumerate().filter(|(i, s)| Some(SiteId(*i as u32)) != skipped && !prim::is_prim(&self.prog, s, word_root && root == Some(SiteId(*i as u32)))).count() + self.prim_walks()
    }

    /// The walked ScalarArrayOps whose site is tier A (`host_calls`).
    fn prim_walks(&self) -> usize {
        let p = &self.prog;
        p.nodes.iter().filter(|n| matches!(n, ExprNode::ScalarArrayOp(s) if scalar::walked(p, s) && prim::is_prim(p, &p.sites[s.site.0 as usize], false))).count()
    }

    /// The cells a program with host sites needs (`RowRefs::cells`); the program outlives them.
    pub fn emit_state<'m>(&mut self, m: mcx::Mcx<'m>) -> EmitState<'m> {
        EmitState::new(self, m)
    }

    pub fn emit(&self, f: &mut FunctionBuilder<'_>, row: &RowRefs<'_>) -> Result<Lowered, EmitError> {
        self.emit_at(f, row, self.root)
    }

    /// `emit` of the subtree at `node` (a per-code body swept over a one-column row holding the entry's image).
    pub fn emit_at(&self, f: &mut FunctionBuilder<'_>, row: &RowRefs<'_>, node: NodeId) -> Result<Lowered, EmitError> {
        let plan = plan(&self.prog, self.root, row.scratch).map_err(|_| EmitError::Unsupported(Kind::Const))?;
        let pc = if row.codes.is_empty() { PerCodePlan::new_in(row.scratch) } else { crate::percode::analyze(&self.prog, self.root, row.codes, row.scratch)? };
        let mut em = scalar::Em::new(&self.prog, f, row, &plan, pc, self.root);
        em.node(node)
    }

    /// D3033: the maximal operands over constants and parameters alone that the caller may compute once at a scope its
    /// parameters hold still over (`RowRefs::late`) instead of per row: a call (or a relabel of one) whose leaves are
    /// literals, compiler words and the parameters `param_ok` passes, whose every call is in line or a by-value direct
    /// shim (`host::direct`: no region, no by-reference image) that `call_ok` passes (the caller's volatility test). D3108:
    /// or a by-value shim whose by-reference arguments are leaves (`host::leaf_images`: a literal's or a parameter's image,
    /// which holds still with the parameters; no call's region image), as `current_date - interval '12 months'`. A bare
    /// leaf is no root (it is a word already); an island is none (the interpreter's).
    pub fn late_roots(&self, param_ok: &dyn Fn(&crate::nodes::param::Param) -> bool, call_ok: &dyn Fn(&crate::nodes::func::Call) -> bool, scratch: Mcx<'_>) -> Vec<NodeId> {
        type Test<'a, T> = &'a dyn Fn(&T) -> bool;
        // D3930: `closed` per node, once (0 not asked yet, 1 no, 2 yes): `walk` asks it at every call above a subtree that
        // is not, so asked afresh a left-deep chain was quadratic in its depth (and in `call_ok`'s catalog reads).
        fn closed(p: &Program<'_>, id: NodeId, param_ok: Test<'_, crate::nodes::param::Param>, call_ok: Test<'_, crate::nodes::func::Call>, memo: &mut [u8]) -> bool {
            crate::stack_check();
            match memo.get(id.0 as usize) {
                Some(1) => return false,
                Some(2) => return true,
                _ => {}
            }
            let r = closed_at(p, id, param_ok, call_ok, memo);
            if let Some(m) = memo.get_mut(id.0 as usize) {
                *m = if r { 2 } else { 1 };
            }
            r
        }
        fn closed_at(p: &Program<'_>, id: NodeId, param_ok: Test<'_, crate::nodes::param::Param>, call_ok: Test<'_, crate::nodes::func::Call>, memo: &mut [u8]) -> bool {
            if p.islands.iter().any(|i| i.root == id) {
                return false;
            }
            match &p.nodes[id.0 as usize] {
                ExprNode::Const(_) | ExprNode::Imm(_) => true,
                ExprNode::Param(q) => param_ok(q),
                ExprNode::Relabel(r) => closed(p, r.arg, param_ok, call_ok, memo),
                ExprNode::Func(n) | ExprNode::Op(n) => {
                    let c = &p.sites[n.site.0 as usize];
                    (prim::is_prim(p, c, false) || host::direct(p, c).is_some() || host::leaf_images(p, c)) && call_ok(c) && kids(p, id).into_iter().all(|k| closed(p, k, param_ok, call_ok, memo))
                }
                _ => false,
            }
        }
        fn walk(p: &Program<'_>, id: NodeId, param_ok: Test<'_, crate::nodes::param::Param>, call_ok: Test<'_, crate::nodes::func::Call>, memo: &mut [u8], out: &mut Vec<NodeId>) {
            crate::stack_check();
            let call = match &p.nodes[id.0 as usize] {
                ExprNode::Func(_) | ExprNode::Op(_) => true,
                ExprNode::Relabel(r) => matches!(p.nodes[r.arg.0 as usize], ExprNode::Func(_) | ExprNode::Op(_)),
                _ => false,
            };
            if call && closed(p, id, param_ok, call_ok, memo) {
                out.push(id); // alloc: Program
                return;
            }
            crate::percode::walk_kids(p, id).into_iter().for_each(|k| walk(p, k, param_ok, call_ok, memo, out));
        }
        let mut memo = mcx::vec_elem_in(scratch, 0u8, self.prog.nodes.len());
        let mut out = Vec::new(); // alloc: Program
        walk(&self.prog, self.root, param_ok, call_ok, &mut memo, &mut out);
        out
    }

    /// `emit` as a branch condition (§7.7); a NULL row's `cond` is unspecified (test `valid`).
    pub fn emit_pred(&self, f: &mut FunctionBuilder<'_>, row: &RowRefs<'_>) -> Result<Pred, EmitError> {
        let plan = plan(&self.prog, self.root, row.scratch).map_err(|_| EmitError::Unsupported(Kind::Const))?;
        let pc = if row.codes.is_empty() { PerCodePlan::new_in(row.scratch) } else { crate::percode::analyze(&self.prog, self.root, row.codes, row.scratch)? };
        let mut em = scalar::Em::new(&self.prog, f, row, &plan, pc, self.root);
        let l = em.node(self.root)?;
        let cond = em.truth_of(&l);
        Ok(Pred { cond, valid: l.valid, err: l.err })
    }

    /// The per-code roots over `codes`; the walk's printing in `scratch`.
    pub fn percode<'m>(&self, codes: &[CodeCol], scratch: Mcx<'m>) -> Result<PerCodePlan<'m>, EmitError> {
        crate::percode::analyze(&self.prog, self.root, codes, scratch)
    }

    /// The whole expression is one per-code root over `codes` (evaluated per dictionary entry, never a host call per row).
    pub fn percode_whole(&self, codes: &[CodeCol], scratch: Mcx<'_>) -> bool {
        self.percode(codes, scratch).is_ok_and(|p| p.root_at(self.root).is_some())
    }

    /// The tables the caller binds per window (`RowRefs::tables`), in order.
    pub fn table_needs<'m>(&self, codes: &[CodeCol], scratch: Mcx<'m>) -> Result<mcx::PgVec<'m, TableNeed<'m>>, EmitError> {
        Ok(self.percode(codes, scratch)?.needs)
    }

    /// `host_calls` with the per-code roots' sites excluded (evaluated per entry, not per row).
    pub fn host_calls_over(&self, codes: &[CodeCol], scratch: Mcx<'_>) -> Result<usize, EmitError> {
        Ok(crate::percode::host_calls(&self.prog, self.root, &self.percode(codes, scratch)?))
    }

    /// The 64-row vector form of a predicate over fixed-width lanes (§2.1): the word's mask.
    pub fn emit_word(&self, f: &mut FunctionBuilder<'_>, w: &word::WordRefs<'_>, base: Value) -> Result<Value, EmitError> {
        self.emit_word_codes(f, w, &[], base)
    }

    /// `emit_word` with the per-code roots over `codes` (a `<> ''` root compares the code lane with the window's empty code).
    pub fn emit_word_codes(&self, f: &mut FunctionBuilder<'_>, w: &word::WordRefs<'_>, codes: &[CodeRef], base: Value) -> Result<Value, EmitError> {
        let plan = plan(&self.prog, self.root, w.scratch).map_err(|_| EmitError::Unsupported(Kind::Const))?;
        let pc = if codes.is_empty() { PerCodePlan::new_in(w.scratch) } else { crate::percode::analyze(&self.prog, self.root, codes, w.scratch)? };
        word::emit(&self.prog, self.root, f, w, &plan, base, (!codes.is_empty()).then_some((&pc, codes)))
    }

    /// `emit_word_codes` gathering a `Bits` root only within `within` (the caller's earlier words) when few rows survive.
    pub fn emit_word_codes_within(&self, f: &mut FunctionBuilder<'_>, w: &word::WordRefs<'_>, codes: &[CodeRef], base: Value, within: Value) -> Result<Value, EmitError> {
        let plan = plan(&self.prog, self.root, w.scratch).map_err(|_| EmitError::Unsupported(Kind::Const))?;
        let pc = if codes.is_empty() { PerCodePlan::new_in(w.scratch) } else { crate::percode::analyze(&self.prog, self.root, codes, w.scratch)? };
        word::emit_within(&self.prog, self.root, f, w, &plan, base, (!codes.is_empty()).then_some((&pc, codes)), Some(within))
    }

    /// Whether the word form of this predicate is one `Bits` root (a per-row table gather).
    pub fn word_is_bits(&self, w: &word::WordRefs<'_>, codes: &[CodeRef]) -> bool {
        let Ok(plan) = plan(&self.prog, self.root, w.scratch) else { return false };
        let Ok(pc) = (if codes.is_empty() { Ok(PerCodePlan::new_in(w.scratch)) } else { crate::percode::analyze(&self.prog, self.root, codes, w.scratch) }) else { return false };
        word::is_bits_root(&self.prog, self.root, w, &plan, (!codes.is_empty()).then_some((&pc, codes)))
    }

    /// The per-window guard of the vector form: every constant fits its lane (`I64` 0 / 1).
    pub fn emit_word_ok(&self, f: &mut FunctionBuilder<'_>, w: &word::WordRefs<'_>) -> Result<Value, EmitError> {
        self.emit_word_ok_codes(f, w, &[])
    }

    pub fn emit_word_ok_codes(&self, f: &mut FunctionBuilder<'_>, w: &word::WordRefs<'_>, codes: &[CodeRef]) -> Result<Value, EmitError> {
        let plan = plan(&self.prog, self.root, w.scratch).map_err(|_| EmitError::Unsupported(Kind::Const))?;
        let pc = if codes.is_empty() { PerCodePlan::new_in(w.scratch) } else { crate::percode::analyze(&self.prog, self.root, codes, w.scratch)? };
        word::ok(&self.prog, self.root, f, w, &plan, (!codes.is_empty()).then_some((&pc, codes)))
    }

    /// The vector value form (§8.1), prepared where the loop is entered; `VecEmit::chunk` per chunk.
    pub fn emit_vec<'s>(&self, f: &mut FunctionBuilder<'_>, w: &word::WordRefs<'s>) -> Result<vec::VecEmit<'s>, EmitError> {
        let plan = plan(&self.prog, self.root, w.scratch).map_err(|_| EmitError::Unsupported(Kind::Const))?;
        vec::emit(&self.prog, self.root, f, w, &plan)
    }
}
