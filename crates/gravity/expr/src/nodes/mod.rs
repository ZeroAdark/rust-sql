//! The node arena (docs/gravity/expr-compiler.md §1.1): every executable expression kind as a
//! variant, one file per family, children as ranges of `kids`, function call sites in `sites`.
//! An unimplemented kind is refused at compile with `CompileError::Unsupported(kind)`.

pub mod agg;
pub mod array;
pub mod bool;
pub mod case;
pub mod cast;
pub mod collate;
pub mod domain;
pub mod field;
pub mod func;
pub mod hostcall;
pub mod konst;
pub mod json;
pub mod kindname;
pub mod misc;
pub mod null;
pub mod op;
pub mod param;
pub mod row;
pub mod rowcmp;
pub mod srf;
pub mod subscript;
pub mod tuple;
pub mod var;
pub mod xml;


use gravity_fn::{Datum, FmgrCall, NullableDatum};
use types_core::Oid;
use types_nodes::{Node, NodeList, NodeTag};

use crate::catalog::Catalog;
use mcx::GrowIn;
use crate::{CompileError, CompileResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SiteId(pub u32);

/// A child range in `Program::kids`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Kids {
    pub start: u32,
    pub len: u32,
}

impl Kids {
    pub const NONE: Kids = Kids { start: 0, len: 0 };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Var,
    Const,
    Param,
    Aggref,
    GroupingFunc,
    WindowFunc,
    MergeSupportFunc,
    SubscriptingRef,
    FuncExpr,
    OpExpr,
    DistinctExpr,
    NullIfExpr,
    ScalarArrayOpExpr,
    BoolExpr,
    SubPlan,
    AlternativeSubPlan,
    FieldSelect,
    FieldStore,
    RelabelType,
    CoerceViaIO,
    ArrayCoerceExpr,
    ConvertRowtypeExpr,
    CollateExpr,
    CaseExpr,
    CaseTestExpr,
    ArrayExpr,
    RowExpr,
    RowCompareExpr,
    CoalesceExpr,
    MinMaxExpr,
    SQLValueFunction,
    XmlExpr,
    JsonValueExpr,
    JsonConstructorExpr,
    JsonIsPredicate,
    JsonExpr,
    NullTest,
    BooleanTest,
    CoerceToDomain,
    CoerceToDomainValue,
    SetToDefault,
    CurrentOfExpr,
    NextValueExpr,
    ReturningExpr,
    TargetEntry,
}

impl Kind {
    pub fn of_tag(tag: NodeTag) -> Option<Kind> {
        Some(match tag {
            NodeTag::T_Var => Kind::Var,
            NodeTag::T_Const => Kind::Const,
            NodeTag::T_Param => Kind::Param,
            NodeTag::T_Aggref => Kind::Aggref,
            NodeTag::T_GroupingFunc => Kind::GroupingFunc,
            NodeTag::T_WindowFunc => Kind::WindowFunc,
            NodeTag::T_MergeSupportFunc => Kind::MergeSupportFunc,
            NodeTag::T_SubscriptingRef => Kind::SubscriptingRef,
            NodeTag::T_FuncExpr => Kind::FuncExpr,
            NodeTag::T_OpExpr => Kind::OpExpr,
            NodeTag::T_DistinctExpr => Kind::DistinctExpr,
            NodeTag::T_NullIfExpr => Kind::NullIfExpr,
            NodeTag::T_ScalarArrayOpExpr => Kind::ScalarArrayOpExpr,
            NodeTag::T_BoolExpr => Kind::BoolExpr,
            NodeTag::T_SubPlan => Kind::SubPlan,
            NodeTag::T_AlternativeSubPlan => Kind::AlternativeSubPlan,
            NodeTag::T_FieldSelect => Kind::FieldSelect,
            NodeTag::T_FieldStore => Kind::FieldStore,
            NodeTag::T_RelabelType => Kind::RelabelType,
            NodeTag::T_CoerceViaIO => Kind::CoerceViaIO,
            NodeTag::T_ArrayCoerceExpr => Kind::ArrayCoerceExpr,
            NodeTag::T_ConvertRowtypeExpr => Kind::ConvertRowtypeExpr,
            NodeTag::T_CollateExpr => Kind::CollateExpr,
            NodeTag::T_CaseExpr => Kind::CaseExpr,
            NodeTag::T_CaseTestExpr => Kind::CaseTestExpr,
            NodeTag::T_ArrayExpr => Kind::ArrayExpr,
            NodeTag::T_RowExpr => Kind::RowExpr,
            NodeTag::T_RowCompareExpr => Kind::RowCompareExpr,
            NodeTag::T_CoalesceExpr => Kind::CoalesceExpr,
            NodeTag::T_MinMaxExpr => Kind::MinMaxExpr,
            NodeTag::T_SQLValueFunction => Kind::SQLValueFunction,
            NodeTag::T_XmlExpr => Kind::XmlExpr,
            NodeTag::T_JsonValueExpr => Kind::JsonValueExpr,
            NodeTag::T_JsonConstructorExpr => Kind::JsonConstructorExpr,
            NodeTag::T_JsonIsPredicate => Kind::JsonIsPredicate,
            NodeTag::T_JsonExpr => Kind::JsonExpr,
            NodeTag::T_NullTest => Kind::NullTest,
            NodeTag::T_BooleanTest => Kind::BooleanTest,
            NodeTag::T_CoerceToDomain => Kind::CoerceToDomain,
            NodeTag::T_CoerceToDomainValue => Kind::CoerceToDomainValue,
            NodeTag::T_SetToDefault => Kind::SetToDefault,
            NodeTag::T_CurrentOfExpr => Kind::CurrentOfExpr,
            NodeTag::T_NextValueExpr => Kind::NextValueExpr,
            NodeTag::T_ReturningExpr => Kind::ReturningExpr,
            NodeTag::T_TargetEntry => Kind::TargetEntry,
            _ => return None,
        })
    }
}

pub use konst::ConstNode;

pub enum ExprNode<'mcx> {
    Var(var::Var),
    Const(ConstNode<'mcx>),
    Param(param::Param),
    Func(func::FuncNode),
    Op(func::FuncNode),
    Distinct(func::FuncNode),
    NullIf(func::FuncNode),
    ScalarArrayOp(op::ScalarArrayOp),
    Bool(bool::BoolNode),
    NullTest(null::NullTestNode),
    BooleanTest(null::BooleanTestNode),
    Case(case::Case),
    CaseTest,
    Coalesce(case::Coalesce),
    MinMax(case::MinMax),
    Relabel(cast::Relabel),
    CoerceViaIO(cast::CoerceViaIO),
    CoerceToDomain(cast::CoerceToDomain),
    CoerceToDomainValue,
    RowCompare(rowcmp::RowCompare),
    FieldSelect(field::FieldSelect),
    FieldStore(field::FieldStore),
    Subscripting(subscript::Subscripting),
    Array(array::ArrayExpr),
    ArrayCoerce(array::ArrayCoerce),
    Row(row::RowExpr),
    ConvertRowtype(row::ConvertRowtype),
    Collate(collate::Collate),
    SqlValueFunction(misc::SqlValueFunction),
    Xml(misc::Xml),
    Json(misc::Json),
    NextValue(misc::NextValue),
    CurrentOf(misc::CurrentOf),
    Aggref(agg::Aggref),
    WindowFunc(agg::WindowFunc),
    GroupingFunc(agg::GroupingFunc),
    SubPlan(srf::SubPlan),
    Srf(srf::Srf),
    Returning(misc::Returning),
    /// A by-value word the compiler chose (a typmod for a scale call): no parameter word, no tree literal.
    Imm(misc::Imm),
}

impl ExprNode<'_> {
    pub fn kind(&self) -> Kind {
        match self {
            ExprNode::Var(_) => Kind::Var,
            ExprNode::Const(_) => Kind::Const,
            ExprNode::Param(_) => Kind::Param,
            ExprNode::Func(_) => Kind::FuncExpr,
            ExprNode::Op(_) => Kind::OpExpr,
            ExprNode::Distinct(_) => Kind::DistinctExpr,
            ExprNode::NullIf(_) => Kind::NullIfExpr,
            ExprNode::ScalarArrayOp(_) => Kind::ScalarArrayOpExpr,
            ExprNode::Bool(_) => Kind::BoolExpr,
            ExprNode::NullTest(_) => Kind::NullTest,
            ExprNode::BooleanTest(_) => Kind::BooleanTest,
            ExprNode::Case(_) => Kind::CaseExpr,
            ExprNode::CaseTest => Kind::CaseTestExpr,
            ExprNode::Coalesce(_) => Kind::CoalesceExpr,
            ExprNode::MinMax(_) => Kind::MinMaxExpr,
            ExprNode::Relabel(_) => Kind::RelabelType,
            ExprNode::CoerceViaIO(_) => Kind::CoerceViaIO,
            ExprNode::CoerceToDomain(_) => Kind::CoerceToDomain,
            ExprNode::CoerceToDomainValue => Kind::CoerceToDomainValue,
            ExprNode::RowCompare(_) => Kind::RowCompareExpr,
            ExprNode::FieldSelect(_) => Kind::FieldSelect,
            ExprNode::FieldStore(_) => Kind::FieldStore,
            ExprNode::Subscripting(_) => Kind::SubscriptingRef,
            ExprNode::Array(_) => Kind::ArrayExpr,
            ExprNode::ArrayCoerce(_) => Kind::ArrayCoerceExpr,
            ExprNode::Row(_) => Kind::RowExpr,
            ExprNode::ConvertRowtype(_) => Kind::ConvertRowtypeExpr,
            ExprNode::Collate(_) => Kind::CollateExpr,
            ExprNode::SqlValueFunction(_) => Kind::SQLValueFunction,
            ExprNode::Xml(_) => Kind::XmlExpr,
            ExprNode::Json(_) => Kind::JsonExpr,
            ExprNode::NextValue(_) => Kind::NextValueExpr,
            ExprNode::CurrentOf(_) => Kind::CurrentOfExpr,
            ExprNode::Aggref(_) => Kind::Aggref,
            ExprNode::WindowFunc(_) => Kind::WindowFunc,
            ExprNode::GroupingFunc(_) => Kind::GroupingFunc,
            ExprNode::SubPlan(_) => Kind::SubPlan,
            ExprNode::Srf(_) => Kind::FuncExpr,
            ExprNode::Returning(_) => Kind::ReturningExpr,
            ExprNode::Imm(_) => Kind::Const,
        }
    }
}

/// The immutable program: nodes, child ranges, call sites, the domain constraint names.
/// memsig E-prog: its lists are built in place in the context its compiler gives (`empty_in`): a hosted owner's lists
/// root (lowerv4 `Px::lists_mcx`), a plan or test context; growth raises the context's ERROR (`mcx::GrowIn`).
pub struct Program<'mcx> {
    pub nodes: mcx::PgVec<'mcx, ExprNode<'mcx>>,
    pub kids: mcx::PgVec<'mcx, NodeId>,
    pub sites: mcx::PgVec<'mcx, func::Call>,
    pub convs: mcx::PgVec<'mcx, func::ArgConv>,
    pub names: mcx::PgVec<'mcx, &'static str>,
    pub oids: mcx::PgVec<'mcx, Oid>,
    pub json_vars: mcx::PgVec<'mcx, json::JsonVar>,
    /// The program's copies of what it took from the plan tree (by-reference constant images, names): the
    /// ProgramCache outlives the plan, and an island reads them at execution.
    pub owned: mcx::PgVec<'mcx, Vec<u8>>,
    /// The aggregate cells the program reads (`max aggno + 1`; `Row::aggs` must hold them).
    pub naggs: u32,
    /// The interpreter islands (`Builder::island`): one per `Json` / `Xml` root the emitter may host.
    pub islands: mcx::PgVec<'mcx, Island>,
}

/// An interpreter island: the subtree `first..=root` (compiled contiguously, kids before parents) the emitter hosts
/// through `site` (`How::Island`), whose arguments are the subtree's distinct `Var`s in first-occurrence order, then its
/// distinct `Aggref` / `SubPlan` cells, then its extern `Param`s.
#[derive(Clone, Copy, Debug)]
pub struct Island {
    pub root: NodeId,
    pub first: NodeId,
    pub site: SiteId,
    /// The plan node the island compiles, by address: its literals are the interpreter's, read from the program, never
    /// the emitted code's parameter words (a lowerer's literal walk skips the subtree).
    pub src: usize,
    /// D901 (M6-AGG, main 07:39Z): `Some` where the island's value is its extern parameters' alone (`Once`).
    pub once: Option<Once>,
}

/// D901: an island that reads no column and whose every node is a built-in IMMUTABLE computation (a pg_catalog function,
/// oid below FirstNormalObjectId, provolatile 'i'; a constant, a parameter, a relabel, an array coercion, COALESCE): its
/// value depends on its parameters' values alone, so emit/host.rs `island` computes it on the first call that reaches it
/// and returns the kept value while the arguments' values are the same (a by-value word equal, a plain varlena's image
/// byte-equal). A user function, IMMUTABLE or not, a stable or volatile one, keeps the island per row (PG evaluates a
/// generic plan's call per row; its notices and side effects would show the difference). A nested island (an array
/// coercion over an island: `c = ANY($1::varchar(2)[])` reads `$1::varchar(2)[]` as text[]) lies in this island's range:
/// its nodes and sites are checked as this island's own, its site is no call.
#[derive(Clone, Copy, Debug)]
pub struct Once {
    /// Bit `i`: argument `i` is a varlena (its image the key); clear: a by-value word.
    pub varlena: u32,
    /// The result: a by-value word, or (`ret_byval` false) a varlena image the island keeps a copy of.
    pub ret_byval: bool,
}

impl<'mcx> Program<'mcx> {
    /// An empty program whose lists grow in `m` (memsig E-prog: built in place, never moved after).
    pub fn empty_in(m: mcx::Mcx<'mcx>) -> Self {
        Program { nodes: mcx::PgVec::new_in(m), kids: mcx::PgVec::new_in(m), sites: mcx::PgVec::new_in(m), convs: mcx::PgVec::new_in(m), names: mcx::PgVec::new_in(m), oids: mcx::PgVec::new_in(m), json_vars: mcx::PgVec::new_in(m), owned: mcx::PgVec::new_in(m), naggs: 0, islands: mcx::PgVec::new_in(m) }
    }
}

pub use crate::catalog::CallFacts;

/// The mutable per-program state: one `FmgrCall` per tier-C site, the SQL/JSON caches.
pub struct State {
    pub calls: Vec<FmgrCall>,
    /// Per function-manager call, what its calling node told it (a consumer that outlives the plan re-points `fn_expr` at these).
    pub call_facts: Vec<CallFacts>,
    pub json: Vec<json::JsonSlot>,
    pub descs: Vec<tuple::Desc>,
}

impl State {
    pub const fn empty() -> Self {
        State { calls: Vec::new(), call_facts: Vec::new(), json: Vec::new(), descs: Vec::new() } // alloc: Program
    }

    pub fn reset_window(&mut self) {
        for c in self.calls.iter_mut() {
            c.reset_window();
        }
    }
}

/// What tier 0 reads of a program (`eval` and the node evaluators), wherever its lists live: a compiled
/// program's own (`Program::view`) or a hosted owner's, moved into its product root (`HostedProgram::view`). One interpreter
/// over both, no second instantiation of it.
#[derive(Clone, Copy)]
pub struct Prog<'a, 'mcx> {
    pub nodes: &'a [ExprNode<'mcx>],
    pub kids: &'a [NodeId],
    pub sites: &'a [func::Call],
    pub convs: &'a [func::ArgConv],
    pub names: &'a [&'static str],
    pub oids: &'a [Oid],
    pub json_vars: &'a [json::JsonVar],
    pub owned: &'a [Vec<u8>],
    pub naggs: u32,
    pub islands: &'a [Island],
}

/// The state tier 0 runs over (`State::view`, `HostedState::view`): its calls, SQL/JSON caches and descriptors.
pub struct St<'a> {
    pub calls: &'a mut [FmgrCall],
    pub call_facts: &'a mut [CallFacts],
    pub json: &'a mut [json::JsonSlot],
    pub descs: &'a mut [tuple::Desc],
}

impl<'mcx> Program<'mcx> {
    pub fn view(&self) -> Prog<'_, 'mcx> {
        let p = self;
        Prog { nodes: &p.nodes, kids: &p.kids, sites: &p.sites, convs: &p.convs, names: &p.names, oids: &p.oids, json_vars: &p.json_vars, owned: &p.owned, naggs: p.naggs, islands: &p.islands }
    }
}

impl State {
    pub fn view(&mut self) -> St<'_> {
        St { calls: &mut self.calls, call_facts: &mut self.call_facts, json: &mut self.json, descs: &mut self.descs }
    }
}

/// A hosted owner's program (lowerv4 `hir`): memsig E-prog: its lists were built in place in the owner's lists root
/// (or a template's root), so it is a `Program` at the root's lifetime (`Program::into_hosted`, a lifetime-only change).
/// Each owned image keeps its own heap block, which `names`, `json_vars` and an IR's baked `Imm` (jsonb_codes) point into.
pub type HostedProgram = Program<'static>;

/// `State`'s lists in the same context as their `HostedProgram`.
pub struct HostedState {
    pub calls: mcx::PgVec<'static, FmgrCall>,
    pub call_facts: mcx::PgVec<'static, CallFacts>,
    pub json: mcx::PgVec<'static, json::JsonSlot>,
    pub descs: mcx::PgVec<'static, tuple::Desc>,
}

impl HostedState {
    pub fn view(&mut self) -> St<'_> {
        St { calls: &mut self.calls, call_facts: &mut self.call_facts, json: &mut self.json, descs: &mut self.descs }
    }
}

/// A scan varno's `tableoid` system column (`Builder::tableoid`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableOid {
    /// The relation's OID, a plan-time constant (one relation under the varno).
    Rel(types_core::Oid),
    /// D3467: column `attno` of row source `src`, which holds each row's relation OID (a MERGE over several result
    /// relations reads a row's target relation from its junk tableoid).
    Col(crate::eval::Source, i16),
}

pub struct Builder<'b, 'mcx> {
    pub prog: &'b mut Program<'mcx>,
    pub state: &'b mut State,
    pub catalog: &'b dyn Catalog<'mcx>,
    /// The projection a `Var` of the named varno compiles through (`Expr::compile_outer`: `OUTER_VAR`, a child's
    /// projection; `Expr::compile_over`: a scan's varno, a VALUES row's cells) — entry N for attribute N.
    pub outer: Option<(i32, &'b NodeList<'mcx>)>,
    /// The scan varno whose `tableoid` system column the builder names (`Expr::compile_scan`): a relation OID, compiled as
    /// an immediate, or a column of a row source that holds it per row.
    pub tableoid: Option<(i32, TableOid)>,
    /// lowerv4 M3 decision 15 (D346): an Aggref with ORDER BY inside the call compiles as its cell (`Agg::compile_with`);
    /// false everywhere until every keyed end checks `mods.order` and `runs.group` folds the ordered aggregate.
    pub aggorder: bool,
    /// M4-RET R4a (PG 18 RETURNING OLD / NEW): where an OLD and a NEW `Var` of a RETURNING list read (the lowerer's
    /// write.returning continuation lays the images out); `None` everywhere else, where such a Var is unsupported.
    pub returning: Option<Returning>,
    /// The lowerer folds every aggregate itself (lowerv4, M6-FOLD R2): an Aggref compiles as its cell after
    /// `Agg::cell_gate` alone, never the `AggPlan` fold's transition gates (a non-strict or context-bound transfn).
    pub folded: bool,
}

/// A RETURNING list's image sources (M4-RET R4a; D789): OLD / NEW attribute `a` reads `(source, offset + a)`; MERGE's
/// `merge_action()` reads the text cell `action` names; D669: a column of another relation (`OUTER_VAR`, the child's
/// resno `r`: UPDATE .. FROM, DELETE .. USING, MERGE's source) reads `(source, offset + r)` of `outer`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Returning {
    pub old: (crate::eval::Source, i16),
    pub new: (crate::eval::Source, i16),
    pub action: Option<(crate::eval::Source, i16)>,
    /// Whether the OLD / NEW image exists for every row of the command (EEO_FLAG_OLD_IS_NULL / _NEW_IS_NULL, which
    /// ExecProcessReturning sets per row): `Some(false)` an INSERT's OLD, a DELETE's NEW; `None` where it depends on the
    /// row (MERGE's action, ON CONFLICT DO UPDATE's): a `ReturningExpr` over it reads `old_flag` / `new_flag`, else is
    /// unsupported (`misc::compile_returning`).
    pub old_exists: Option<bool>,
    pub new_exists: Option<bool>,
    /// Where the existence depends on the row (`old_exists` / `new_exists` `None`): the boolean cell, laid per row, that
    /// is true where the OLD / NEW image exists (MERGE: per action, merge/ret.rs; the flags EEO_FLAG_OLD_IS_NULL /
    /// NEW_IS_NULL negated). `None`: a `ReturningExpr` over that image is unsupported.
    pub old_flag: Option<(crate::eval::Source, i16)>,
    pub new_flag: Option<(crate::eval::Source, i16)>,
    pub outer: Option<(crate::eval::Source, i16)>,
    /// R4c: OLD's tableoid when it is not the written row's (an UPDATE through a root: the row's target, which a moved row
    /// left for another leaf), a plan-time constant.
    pub old_tableoid: Option<u32>,
    /// V4R-501: OLD's system columns (`ctid` .. `tableoid`, attno -1 .. -6) are the OLD image's lanes `base - attno`, which
    /// the lowerer lays from the replaced version the host fetched (NULL where the image does not exist).
    pub old_sys: Option<i16>,
}

impl<'b, 'mcx> Builder<'b, 'mcx> {
    pub fn compile(&mut self, n: Node<'mcx>) -> CompileResult<NodeId> {
        crate::stack_check();
        let tag = n.node_tag();
        let kind = Kind::of_tag(tag).ok_or(CompileError::Unsupported(Kind::TargetEntry))?;
        match kind {
            Kind::Var => var::compile(self, n.as_var().unwrap()),
            Kind::Const => {
                let c = n.as_const().unwrap();
                let value = if c.constisnull { NullableDatum::null() } else { NullableDatum::value(c.constvalue) };
                Ok(self.push(ExprNode::Const(ConstNode::new(value, c.consttype, c.constlen as i16, c.constbyval))))
            }
            Kind::Param => param::compile(self, n.as_param().unwrap()),
            Kind::FuncExpr => func::compile_func(self, n.as_func_expr().unwrap(), n),
            Kind::OpExpr => func::compile_op(self, n.as_op_expr().unwrap(), n),
            Kind::DistinctExpr => op::compile_distinct(self, n.as_distinct_expr().unwrap(), n),
            Kind::NullIfExpr => op::compile_nullif(self, n.as_null_if_expr().unwrap(), n),
            Kind::ScalarArrayOpExpr => op::compile_scalar_array_op(self, n.as_scalar_array_op_expr().unwrap(), n),
            Kind::BoolExpr => bool::compile(self, n.as_bool_expr().unwrap()),
            Kind::NullTest => self.covered(n, |b| null::compile_null_test(b, n.as_null_test().unwrap())),
            Kind::BooleanTest => null::compile_boolean_test(self, n.as_boolean_test().unwrap()),
            Kind::CaseExpr => case::compile_case(self, n.as_case_expr().unwrap()),
            Kind::CaseTestExpr => Ok(self.push(ExprNode::CaseTest)),
            Kind::CoalesceExpr => case::compile_coalesce(self, n.as_coalesce_expr().unwrap()),
            Kind::MinMaxExpr => self.covered(n, |b| case::compile_minmax(b, n.as_min_max_expr().unwrap())),
            Kind::RelabelType => cast::compile_relabel(self, n.as_relabel_type().unwrap()),
            Kind::CoerceViaIO => cast::compile_coerce_via_io(self, n.as_coerce_via_io().unwrap()),
            Kind::CoerceToDomain => cast::compile_coerce_to_domain(self, n.as_coerce_to_domain().unwrap()),
            Kind::CoerceToDomainValue => Ok(self.push(ExprNode::CoerceToDomainValue)),
            Kind::RowCompareExpr => self.covered(n, |b| rowcmp::compile(b, n.as_row_compare_expr().unwrap())),
            Kind::FieldSelect => field::compile_field_select(self, n.as_field_select().unwrap()),
            Kind::FieldStore => field::compile_field_store(self, n.as_field_store().unwrap()),
            Kind::SubscriptingRef => subscript::compile(self, n.as_subscripting_ref().unwrap()),
            Kind::ArrayExpr => array::compile_array_expr(self, n.as_array_expr().unwrap()),
            Kind::ArrayCoerceExpr => self.covered(n, |b| array::compile_array_coerce(b, n.as_array_coerce_expr().unwrap())),
            Kind::RowExpr => row::compile_row_expr(self, n.as_row_expr().unwrap()),
            Kind::ConvertRowtypeExpr => row::compile_convert_rowtype(self, n.as_convert_rowtype_expr().unwrap()),
            Kind::CollateExpr => collate::compile(self, n.as_collate_expr().unwrap()),
            Kind::SQLValueFunction => self.covered(n, |b| misc::compile_sql_value_function(b, n.as_sql_value_function().unwrap())),
            Kind::XmlExpr => {
                let (first, sites) = (self.prog.nodes.len(), self.prog.sites.len());
                let root = misc::compile_xml(self, n.as_xml_expr().unwrap())?;
                Ok(self.island(first, sites, root, n))
            }
            Kind::JsonValueExpr | Kind::JsonConstructorExpr | Kind::JsonIsPredicate | Kind::JsonExpr => {
                let (first, sites) = (self.prog.nodes.len(), self.prog.sites.len());
                let root = misc::compile_json(self, n, kind)?;
                Ok(self.island(first, sites, root, n))
            }
            Kind::NextValueExpr => misc::compile_next_value(self, n.as_variant::<types_nodes::primnodes::NextValueExpr>().unwrap()),
            Kind::CurrentOfExpr => misc::compile_current_of(self, n.as_current_of_expr().unwrap()),
            Kind::Aggref => agg::compile_aggref(self, n.as_aggref().unwrap()),
            Kind::WindowFunc => agg::compile_window_func(self, n.as_window_func().unwrap()),
            Kind::GroupingFunc => agg::compile_grouping_func(self, n.as_grouping_func().unwrap()),
            Kind::SubPlan => srf::compile_subplan(self, n.as_sub_plan().unwrap()),
            Kind::ReturningExpr => misc::compile_returning(self, n.as_returning_expr().unwrap()),
            Kind::TargetEntry => self.compile(n.as_target_entry().unwrap().expr),
            // D789: MERGE .. RETURNING merge_action(), the action's text cell the lowerer lays beside the images
            Kind::MergeSupportFunc => match self.returning.and_then(|r| r.action) {
                Some((src, attno)) => Ok(self.push(ExprNode::Var(var::Var { src, attno, typ: 25, desc: 0 }))),
                None => Err(CompileError::Unsupported(kind)),
            },
            Kind::AlternativeSubPlan | Kind::SetToDefault => Err(CompileError::Unsupported(kind)),
        }
    }

    pub fn push(&mut self, node: ExprNode<'mcx>) -> NodeId {
        let id = NodeId(self.prog.nodes.len() as u32);
        self.prog.nodes.push_in(node);
        id
    }

    /// `n` compiled by `f`, as an interpreter island when the emitter does not serve the node it compiles to
    /// (`emit::scalar::check_node`, the emitter's own capability): the coverage kinds (a role `SQLValueFunction`, a
    /// by-reference `MinMax` / `RowCompare`, an `ArrayCoerceExpr`; M1-STEP, main 09-24; a row-valued `NullTest` was one
    /// until the emitter served it, emit/scalar.rs `row_null_test`).
    fn covered<'n>(&mut self, n: Node<'n>, f: impl FnOnce(&mut Self) -> CompileResult<NodeId>) -> CompileResult<NodeId> {
        let (first, sites) = (self.prog.nodes.len(), self.prog.sites.len());
        let root = f(self)?;
        if crate::emit::scalar::check_node(&self.prog, root, false).is_ok() {
            return Ok(root);
        }
        Ok(self.island(first, sites, root, n))
    }

    /// The island over the subtree `first..=root` of plan node `n` (the nodes compiled since `first`, its sites since `sites`): a
    /// `How::Island` site whose arguments are the subtree's distinct column `Var`s, then its distinct extern and executor
    /// `Param`s (their words bound per execution, never baked: emit/host.rs `island` hands them to the interpreter; V4R-454
    /// an executor one as V4R-352 an extern one), recorded in `Program::islands`. No island (the emitter refuses the root by
    /// kind) at a node `island_capable` does not name, or when the subtree reads anything but columns, aggregate cells and
    /// parameters — a whole-row or system `Var`, a window cell, a set — or has more distinct arguments than a site carries;
    /// the interpreter is unchanged.
    /// An `Aggref` is an argument after the columns (JSON_ARRAYAGG / JSON_OBJECTAGG's constructor over its aggregate,
    /// `xmlelement(.., xmlagg(..))`): the emitter reads its finished cell as any call argument, emit/host.rs `island` lays it
    /// into the interpreter row's `aggs` by `aggno`; such an island never keeps a `Once` value (its cell is per group).
    /// A `SubPlan` is one the same way, by its cell (json-0033: `JSON_OBJECT('n': (SELECT max(..) .. WHERE grp = t.grp))`):
    /// the emitter reads it as any expression reads a SubPlan (the step's words, or a lazy SubPlan run where the island is
    /// reached), so the island runs on its value as PG's ExecEvalJsonConstructor runs on the arguments it evaluated first.
    /// The lowerer refuses one inside an island that evaluates part of its subtree conditionally (lowerv4
    /// step/expr_ext_lazy.rs `run`: an argument runs before the island, on every row that reaches it).
    pub fn island(&mut self, first: usize, sites: usize, root: NodeId, n: Node<'_>) -> NodeId {
        // only where the key hashes the constants by value (D337: `island_capable`)
        if !island_capable(n) {
            return root;
        }
        // the island's arguments, in this order: each column read (by source and attno), each aggregate or SubPlan cell, each
        // parameter, the first node of each; written straight into the program's `kids` (one pass per kind)
        for n in self.prog.nodes[first..].iter() {
            if matches!(n, ExprNode::Var(_) | ExprNode::Param(_) | ExprNode::WindowFunc(_) | ExprNode::GroupingFunc(_) | ExprNode::Srf(_) | ExprNode::CurrentOf(_) | ExprNode::NextValue(_) | ExprNode::Returning(_))
                && !matches!(n, ExprNode::Param(q) if matches!(q.kind, types_nodes::primnodes::ParamKind::PARAM_EXTERN | types_nodes::primnodes::ParamKind::PARAM_EXEC))
                && !matches!(n, ExprNode::Var(v) if v.attno > 0)
            {
                return root;
            }
        }
        let start = self.prog.kids.len();
        for kind in 0..3 {
            for i in first..self.prog.nodes.len() {
                let seen = &self.prog.kids[start..];
                let new = match &self.prog.nodes[i] {
                    // V4R-454 (M8-EXPR): an executor parameter (a nest's outer column, a SubPlan's argument) as an extern one
                    ExprNode::Param(q) if kind == 2 => !seen.iter().any(|&k| matches!(&self.prog.nodes[k.0 as usize], ExprNode::Param(w) if w.id == q.id && w.kind == q.kind)),
                    ExprNode::Var(v) if kind == 0 => !seen.iter().any(|&k| matches!(&self.prog.nodes[k.0 as usize], ExprNode::Var(w) if w.src == v.src && w.attno == v.attno)),
                    ExprNode::Aggref(g) if kind == 1 => !seen.iter().any(|&k| cell_of(&self.prog.nodes[k.0 as usize]) == Some(g.aggno)),
                    ExprNode::SubPlan(q) if kind == 1 => !seen.iter().any(|&k| cell_of(&self.prog.nodes[k.0 as usize]) == Some(q.cell)),
                    _ => false,
                };
                if new {
                    self.prog.kids.push_in(NodeId(i as u32));
                }
            }
        }
        let nvars = self.prog.kids.len() - start;
        let columns = nvars > 0 && self.prog.kids[start..].iter().any(|&k| !matches!(self.prog.nodes[k.0 as usize], ExprNode::Param(_)));
        if nvars > func::MAX_ARGS {
            self.prog.kids.truncate(start);
            return root;
        }
        // the island's by-reference constants become the program's own images: the emitted program binds its
        // literals from the live plan tree on every execution, the interpreter reads the nodes, and a ProgramCache
        // hit outlives the plan (a folded literal outside an island keeps the plan's pointer: text.rs zeroes it)
        for i in first..self.prog.nodes.len() {
            // the copy reads the image: a bake (the program keeps these bytes whatever a later statement's literal is)
            let (value, typlen) = match &self.prog.nodes[i] {
                ExprNode::Const(c) if !c.byval && !c.is_null() => (c.read().value, c.typlen),
                _ => continue,
            };
            let Ok(owned) = self.own_image(value, typlen) else {
                self.prog.kids.truncate(start);
                return root;
            };
            if let ExprNode::Const(c) = &mut self.prog.nodes[i] {
                c.own(owned);
            }
        }
        let volatile = self.prog.sites[sites..].iter().any(|s| s.volatile);
        let args = Kids { start: start as u32, len: nvars as u32 };
        let rettype = nodes_core::expr_type(n);
        let once = if columns || volatile { None } else { self.once(first, sites, &self.prog.kids[start..], rettype) };
        let site = self.site(func::Call { funcid: 0, args, nargs: nvars as u8, strict: false, collation: 0, how: func::How::Island { root }, rettype, volatile });
        self.prog.islands.push_in(Island { root, first: NodeId(first as u32), site, src: n.as_raw().as_ptr() as usize, once });
        root
    }

    /// D901: the island `first..` (sites since `sites`, arguments `args`, all extern parameters) as a `Once`, or `None`
    /// where a node is not a built-in IMMUTABLE computation or an argument / the result is neither a by-value word nor a
    /// varlena (a fixed-length by-reference image, a cstring): the island stays per row.
    fn once(&self, first: usize, sites: usize, args: &[NodeId], rettype: Oid) -> Option<Once> {
        let pure = self.prog.nodes[first..].iter().all(|n| {
            matches!(n, ExprNode::Const(_) | ExprNode::Param(_) | ExprNode::CaseTest | ExprNode::Relabel(_) | ExprNode::ArrayCoerce(_) | ExprNode::Func(_) | ExprNode::Op(_) | ExprNode::Coalesce(_) | ExprNode::Collate(_))
        });
        let builtin = |s: &func::Call| crate::catalog::FuncFacts::builtin(s.funcid) && self.catalog.func(s.funcid).is_ok_and(|f| f.immutable());
        // a nested island's own site is no call: its nodes and sites lie in this range, checked above and here as this one's
        let nested = |k: usize| self.prog.islands.iter().any(|i| i.site.0 as usize == k && i.first.0 as usize >= first && i.root.0 as usize >= first);
        if !pure || args.len() > u32::BITS as usize || !self.prog.sites[sites..].iter().enumerate().all(|(k, s)| builtin(s) || nested(sites + k)) {
            return None;
        }
        let word = |typ: Oid| self.catalog.type_facts(typ).ok().and_then(|t| match (t.byval, t.len) {
            (true, _) => Some(false),
            (false, -1) => Some(true),
            _ => None,
        });
        let mut varlena = 0u32;
        for (i, k) in args.iter().enumerate() {
            let ExprNode::Param(q) = &self.prog.nodes[k.0 as usize] else { return None };
            varlena |= (word(q.typ)? as u32) << i;
        }
        Some(Once { varlena, ret_byval: !word(rettype)? })
    }

    /// Compile every element of `list` and record the ids as one contiguous child range.
    /// The list's slots in the program's `kids` first, then each child compiled into its slot: a child's own lists follow
    /// the parent's (any order serves: a list is its `Kids` range), and no list of ids is built aside.
    pub fn kids(&mut self, list: &NodeList<'mcx>) -> CompileResult<Kids> {
        let kids = self.kid_slots(list.len());
        for (i, n) in list.iter().enumerate() {
            let id = self.compile(n)?;
            self.prog.kids[kids.start as usize + i] = id;
        }
        Ok(kids)
    }

    /// `n` slots at the end of the program's `kids` (`NodeId::NONE` until the caller writes each child's id there): a list
    /// whose children are compiled after its range is placed (`kids`), so no list of ids is built aside.
    pub fn kid_slots(&mut self, n: usize) -> Kids {
        let start = self.prog.kids.len() as u32;
        for _ in 0..n {
            self.prog.kids.push_in(NodeId::NONE);
        }
        Kids { start, len: n as u32 }
    }

    pub fn kids_of(&mut self, ids: &[NodeId]) -> Kids {
        let start = self.prog.kids.len() as u32;
        self.prog.kids.extend_from_slice_in(ids);
        Kids { start, len: ids.len() as u32 }
    }

    pub fn site(&mut self, mut call: func::Call) -> SiteId {
        // D2336: the site's result word is its base type's (a domain over int4 canonizes as int4); the function
        // manager's call facts keep the declared type for the polymorphic readers
        call.rettype = func::word_type(self.catalog, call.rettype);
        let id = SiteId(self.prog.sites.len() as u32);
        self.prog.sites.push_in(call);
        id
    }

    pub fn fmgr(&mut self, call: FmgrCall, argtypes: &[Oid], node_arg: bool) -> u32 {
        let id = self.state.calls.len() as u32;
        self.state.calls.push(call);
        self.state.call_facts.push(CallFacts { rettype: 0, variadic: false, node_arg, argtypes: argtypes.to_vec(), stable: 0 }); // alloc: Program
        id
    }

    pub fn name(&mut self, s: &str) -> u32 {
        let id = self.prog.names.len() as u32;
        let s = self.own_str(s);
        self.prog.names.push_in(s);
        id
    }

    /// A copy of `bytes` the program owns; the pointer is stable (`owned` only grows, its buffers never move).
    pub fn own(&mut self, bytes: &[u8]) -> *const u8 {
        self.prog.owned.push_in(bytes.to_vec()); // alloc: Program
        self.prog.owned.last().expect("just pushed").as_ptr()
    }

    /// A by-reference constant's image copied into the program (an external one is refused).
    fn own_image(&mut self, d: Datum, typlen: i16) -> CompileResult<Datum> {
        let p = d.as_usize() as *const u8;
        // SAFETY: a non-null by-reference constant points at a live image of its type for the compile.
        let len = unsafe {
            match typlen {
                -1 if types_tuple::varatt::varatt_is_1b_e(p) => return Err(crate::CompileError::Unsupported(Kind::Const)),
                -1 => types_tuple::varatt::varsize_any(p),
                -2 => core::ffi::CStr::from_ptr(p.cast()).to_bytes().len() + 1,
                n => n as usize,
            }
        };
        // SAFETY: `len` bytes of the image are live (its header or its type's length says so).
        let bytes = unsafe { core::slice::from_raw_parts(p, len) };
        Ok(Datum::from_usize(self.own(bytes) as usize))
    }

    pub fn own_str(&mut self, s: &str) -> &'static str {
        let p = self.own(s.as_bytes());
        // SAFETY: a copy of a `str`'s bytes, owned by the program for its life (`owned` never shrinks).
        unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(p, s.len())) }
    }
}

/// The plan node kinds `Builder::compile` may compile to an interpreter island: the coverage kinds `covered` routes
/// (`NullTest` only row-valued: the emitter serves it now, so `covered` islands none, but the key's walks keep hashing its
/// constants by value) and the Json / Xml kinds, always islands.
pub const ISLAND_TAGS: [NodeTag; 10] = [
    NodeTag::T_NullTest,
    NodeTag::T_MinMaxExpr,
    NodeTag::T_RowCompareExpr,
    NodeTag::T_ArrayCoerceExpr,
    NodeTag::T_SQLValueFunction,
    NodeTag::T_XmlExpr,
    NodeTag::T_JsonValueExpr,
    NodeTag::T_JsonConstructorExpr,
    NodeTag::T_JsonIsPredicate,
    NodeTag::T_JsonExpr,
];

/// Whether plan node `n` may compile to an interpreter island (D337). An island's constants are its program's own (the
/// interpreter reads its Const nodes), never parameter words, so the key's walks (gravity_lower key.rs, gravity_exec
/// front.rs) hash every by-value constant under such a node by value; `Builder::island` makes none at any other node,
/// so the key cannot drift from the islands. By kind, not by the emitter's verdict: a RowCompare / MinMax over a
/// by-value compare is emitted, and its constants are then words the key also hashes (programs by value, never wrong).
/// The `Row::aggs` cell an island argument reads: an `Aggref`'s `aggno`, a `SubPlan`'s cell (one numbering, `naggs`).
fn cell_of(n: &ExprNode<'_>) -> Option<i32> {
    match n {
        ExprNode::Aggref(g) => Some(g.aggno),
        ExprNode::SubPlan(q) => Some(q.cell),
        _ => None,
    }
}

pub fn island_capable(n: Node<'_>) -> bool {
    let tag = n.node_tag();
    let t = tag as u16;
    t < 64 && (ISLAND_MASK >> t) & 1 != 0 && (tag != NodeTag::T_NullTest || n.as_null_test().is_some_and(|t| t.argisrow))
}

/// `ISLAND_TAGS` as one word, bit `tag` per kind (the key walks ask `island_capable` of every expression node: one test
/// in place of a scan of the list). Built at compile time from the list, which stays the one source; a kind whose tag
/// does not fit the word fails the build.
const ISLAND_MASK: u64 = {
    let mut m = 0u64;
    let mut i = 0;
    while i < ISLAND_TAGS.len() {
        let t = ISLAND_TAGS[i] as u16;
        assert!(t < 64, "an island kind's tag does not fit ISLAND_MASK");
        m |= 1 << t;
        i += 1;
    }
    m
};

pub fn unsupported<T>(kind: Kind) -> CompileResult<T> {
    Err(CompileError::Unsupported(kind))
}

#[cfg(test)]
mod island_mask_tests {
    /// `ISLAND_MASK` holds exactly `ISLAND_TAGS`: each listed kind's bit, and no other (the list's tags are distinct).
    #[test]
    fn island_mask_is_the_list() {
        for tag in super::ISLAND_TAGS {
            assert!((super::ISLAND_MASK >> (tag as u16)) & 1 != 0, "{tag:?}");
        }
        assert_eq!(super::ISLAND_MASK.count_ones() as usize, super::ISLAND_TAGS.len());
    }
}
