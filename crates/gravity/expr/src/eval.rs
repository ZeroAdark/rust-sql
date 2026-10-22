//! Tier 0 (docs/gravity/expr-compiler.md §1.2): the interpreter over the node arena. The `Row`
//! is a view over the incumbent's slot arrays or over lanes + validity words; the per-evaluation
//! cells (CASE test value, domain value) live in `Cx`; nothing here allocates per row.

use gravity_abi::PtrWord;
use gravity_abi::{lane, Lane};
use gravity_fn::{Datum, NullableDatum, Region};
use types_core::Oid;
use types_error::PgResult;
use types_portal::params::ParamBind;
use types_slot::SlotData;
use types_tuple::TupleDescData;

use crate::nodes::{self, ExprNode, NodeId, Prog, St};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Scan,
    Inner,
    Outer,
}

/// A row source: the incumbent's deformed arrays, or lanes with validity words at one index.
#[derive(Clone, Copy)]
pub enum Cols<'a> {
    None,
    /// The incumbent's slot arrays; `types` (the descriptor's attribute types, 0 for a dropped
    /// column, empty when unchecked) and `tdtypeid` give `Var` the slot-compatibility check.
    Slot { values: &'a [Datum], nulls: &'a [bool], types: &'a [Oid], tdtypeid: Oid, desc: Option<&'a TupleDescData<'a>>, slot: Option<&'a SlotData<'a>> },
    Lanes { lanes: &'a [Lane], index: usize },
}

impl Cols<'_> {
    /// Column `attno` (1-based) of this source.
    #[inline(always)]
    pub fn get(&self, attno: i16) -> Option<NullableDatum> {
        let i = attno as usize - 1;
        match *self {
            Cols::None => None,
            Cols::Slot { values, nulls, .. } => {
                if i >= values.len() {
                    return None;
                }
                Some(NullableDatum { value: values[i], isnull: nulls[i] })
            }
            Cols::Lanes { lanes, index } => {
                let l = lanes.get(i)?;
                // SAFETY: the row index is within the window that published the lanes (the caller's contract).
                unsafe {
                    if !l.is_valid(index) {
                        return Some(NullableDatum::null());
                    }
                    let word = match l.kind {
                        lane::KIND_BYREF => *(l.ptr.ptr() as *const u64).add(index),
                        _ => l.datum(index),
                    };
                    Some(NullableDatum { value: Datum::from_u64(word), isnull: false })
                }
            }
        }
    }
}

/// The row: up to three sources and the owning node's aggregate cells (`Aggref { aggno }` reads `aggs[aggno]`, §9).
#[derive(Clone, Copy)]
pub struct Row<'a> {
    pub scan: Cols<'a>,
    pub inner: Cols<'a>,
    pub outer: Cols<'a>,
    pub aggs: &'a [NullableDatum],
}

impl<'a> Row<'a> {
    pub const EMPTY: Row<'static> = Row { scan: Cols::None, inner: Cols::None, outer: Cols::None, aggs: &[] };

    pub fn scan(values: &'a [Datum], nulls: &'a [bool]) -> Row<'a> {
        Row { scan: Cols::Slot { values, nulls, types: &[], tdtypeid: 0, desc: None, slot: None }, inner: Cols::None, outer: Cols::None, aggs: &[] }
    }

    pub fn scan_typed(values: &'a [Datum], nulls: &'a [bool], types: &'a [Oid], tdtypeid: Oid) -> Row<'a> {
        Row { scan: Cols::Slot { values, nulls, types, tdtypeid, desc: None, slot: None }, inner: Cols::None, outer: Cols::None, aggs: &[] }
    }

    pub fn with_aggs(self, aggs: &'a [NullableDatum]) -> Row<'a> {
        Row { aggs, ..self }
    }

    /// A slot with its descriptor (the whole-row `Var` forms its tuple from it) and the column
    /// types for the compatibility check.
    pub fn scan_desc(values: &'a [Datum], nulls: &'a [bool], types: &'a [Oid], desc: &'a TupleDescData<'a>) -> Row<'a> {
        Row { scan: Cols::Slot { values, nulls, types, tdtypeid: desc.tdtypeid, desc: Some(desc), slot: None }, inner: Cols::None, outer: Cols::None, aggs: &[] }
    }

    /// The incumbent's slot itself (its deformed arrays, its descriptor, and the tuple the
    /// system-column var reads through `slot_getsysattr`).
    pub fn scan_slot(values: &'a [Datum], nulls: &'a [bool], types: &'a [Oid], desc: &'a TupleDescData<'a>, slot: &'a SlotData<'a>) -> Row<'a> {
        Row { scan: Cols::Slot { values, nulls, types, tdtypeid: desc.tdtypeid, desc: Some(desc), slot: Some(slot) }, inner: Cols::None, outer: Cols::None, aggs: &[] }
    }

    #[inline(always)]
    pub fn source(&self, s: Source) -> &Cols<'a> {
        match s {
            Source::Scan => &self.scan,
            Source::Inner => &self.inner,
            Source::Outer => &self.outer,
        }
    }
}

/// The evaluation context of one call of `Compiled::eval`.
pub struct Cx<'c, 'a> {
    pub row: &'c Row<'a>,
    pub region: &'c mut Region,
    pub params: &'c ParamBind<'a>,
    pub callback_arg: *mut (),
    pub casetest: NullableDatum,
    pub domainval: NullableDatum,
}

impl<'c, 'a> Cx<'c, 'a> {
    pub fn new(row: &'c Row<'a>, region: &'c mut Region, params: &'c ParamBind<'a>, callback_arg: *mut ()) -> Self {
        Cx { row, region, params, callback_arg, casetest: NullableDatum::null(), domainval: NullableDatum::null() }
    }
}

/// Evaluate node `id`: the dispatcher; each family's `eval` is in its file.
pub fn eval(p: &Prog<'_, '_>, st: &mut St<'_>, id: NodeId, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    crate::stack_check();
    match &p.nodes[id.0 as usize] {
        ExprNode::Var(n) if n.attno == 0 => nodes::tuple::eval_whole_row(n, st, cx),
        ExprNode::Var(n) => nodes::var::eval(n, cx),
        ExprNode::Const(n) => Ok(n.read()),
        ExprNode::Imm(n) => Ok(NullableDatum::value(Datum::from_i64(n.value))),
        ExprNode::Param(n) => nodes::param::eval(n, cx),
        ExprNode::Func(n) | ExprNode::Op(n) => nodes::func::eval_call(p, st, &p.sites[n.site.0 as usize], cx),
        ExprNode::Distinct(n) => nodes::op::eval_distinct(p, st, &p.sites[n.site.0 as usize], cx),
        ExprNode::NullIf(n) => nodes::op::eval_nullif(p, st, &p.sites[n.site.0 as usize], cx),
        ExprNode::ScalarArrayOp(n) => nodes::op::eval_scalar_array_op(p, st, n, cx),
        ExprNode::Bool(n) => nodes::bool::eval_bool(p, st, n, cx),
        ExprNode::NullTest(n) => nodes::null::eval_null_test(p, st, n, cx),
        ExprNode::BooleanTest(n) => nodes::null::eval_boolean_test(p, st, n, cx),
        ExprNode::Case(n) => nodes::case::eval_case(p, st, n, cx),
        ExprNode::CaseTest => Ok(cx.casetest),
        ExprNode::Coalesce(n) => nodes::case::eval_coalesce(p, st, n, cx),
        ExprNode::MinMax(n) => nodes::case::eval_minmax(p, st, n, cx),
        ExprNode::Relabel(n) => eval(p, st, n.arg, cx),
        ExprNode::CoerceViaIO(n) => nodes::cast::eval_coerce_via_io(p, st, n, cx),
        ExprNode::CoerceToDomain(n) => nodes::cast::eval_coerce_to_domain(p, st, n, cx),
        ExprNode::CoerceToDomainValue => Ok(cx.domainval),
        ExprNode::RowCompare(n) => nodes::rowcmp::eval_row_compare(p, st, n, cx),
        ExprNode::FieldSelect(n) => nodes::field::eval_field_select(p, st, n, cx),
        ExprNode::FieldStore(n) => nodes::field::eval_field_store(p, st, n, cx),
        ExprNode::Subscripting(n) => nodes::subscript::eval(p, st, n, cx),
        ExprNode::Array(n) => nodes::array::eval_array_expr(p, st, n, cx),
        ExprNode::ArrayCoerce(n) => nodes::array::eval_array_coerce(p, st, n, cx),
        ExprNode::Row(n) => nodes::row::eval_row_expr(p, st, n, cx),
        ExprNode::ConvertRowtype(n) => nodes::row::eval_convert_rowtype(p, st, n, cx),
        ExprNode::Collate(n) => nodes::collate::eval(p, st, n, cx),
        ExprNode::SqlValueFunction(n) => nodes::misc::eval_sql_value_function(p, st, n, cx),
        ExprNode::Xml(n) => nodes::misc::eval_xml(p, st, n, cx),
        ExprNode::Json(n) => nodes::misc::eval_json(p, st, n, cx),
        ExprNode::NextValue(n) => nodes::misc::eval_next_value(p, st, n, cx),
        ExprNode::CurrentOf(n) => nodes::misc::eval_current_of(p, st, n, cx),
        ExprNode::Aggref(n) => nodes::agg::eval_aggref(p, st, n, cx),
        ExprNode::WindowFunc(n) => nodes::agg::eval_window_func(p, st, n, cx),
        ExprNode::GroupingFunc(n) => nodes::agg::eval_grouping_func(p, st, n, cx),
        ExprNode::SubPlan(n) => nodes::srf::eval_subplan(p, st, n, cx),
        ExprNode::Srf(n) => nodes::srf::eval_srf(p, st, n, cx),
        ExprNode::Returning(n) => nodes::misc::eval_returning(p, st, n, cx),
    }
}
