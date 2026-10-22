//! `NullTest` and `BooleanTest`. A row-valued IS [NOT] NULL (`argisrow`) walks the composite's
//! fields (C `ExecEvalRowNullInt`): the interpreter evaluates it here, the emitter through a strict
//! synthetic site over the same walk (nodes/field.rs `ROW_NULL_SITE`).

use gravity_fn::{Datum, NullableDatum};
use types_error::PgResult;
use types_nodes::primnodes::{BoolTestType, BooleanTest, NullTest, NullTestType};
use types_tuple::{varatt, HeapTupleData, HeapTupleHeaderData, ItemPointerData};

use crate::eval::{eval, Cx};
use crate::nodes::array::with_scratch;
use crate::nodes::{Builder, ExprNode, Kind, NodeId, Prog, SiteId, St};
use crate::views::varlena_is_plain;
use crate::CompileResult;

#[derive(Clone, Copy, Debug)]
pub struct NullTestNode {
    pub arg: NodeId,
    pub is_null: bool,
    /// `argisrow`: a composite argument is NULL / NOT NULL when every one of its fields is.
    pub row: bool,
    /// The row form's site (nodes/field.rs `row_null_site`), which the emitter calls over a non-NULL composite; `None` for
    /// the scalar form (and a row form built by hand that only the interpreter evaluates).
    pub site: Option<SiteId>,
}

#[derive(Clone, Copy, Debug)]
pub struct BooleanTestNode {
    pub arg: NodeId,
    pub test: BoolTestType,
}

pub fn compile_null_test<'mcx>(b: &mut Builder<'_, 'mcx>, n: &NullTest<'mcx>) -> CompileResult<NodeId> {
    let arg = b.compile(n.arg.ok_or(crate::CompileError::Unsupported(Kind::NullTest))?)?;
    let site = n.argisrow.then(|| crate::nodes::field::row_null_site(b));
    Ok(b.push(ExprNode::NullTest(NullTestNode { arg, is_null: n.nulltesttype == NullTestType::IS_NULL, row: n.argisrow, site })))
}

pub fn compile_boolean_test<'mcx>(b: &mut Builder<'_, 'mcx>, n: &BooleanTest<'mcx>) -> CompileResult<NodeId> {
    let arg = b.compile(n.arg.ok_or(crate::CompileError::Unsupported(Kind::BooleanTest))?)?;
    Ok(b.push(ExprNode::BooleanTest(BooleanTestNode { arg, test: n.booltesttype })))
}

#[inline]
pub fn eval_null_test(p: &Prog<'_, '_>, st: &mut St<'_>, n: &NullTestNode, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let v = eval(p, st, n.arg, cx)?;
    if !n.row || v.isnull {
        return Ok(NullableDatum::value(Datum::from_bool(v.isnull == n.is_null)));
    }
    Ok(NullableDatum::value(Datum::from_bool(fields_all(v.value, n.is_null)?)))
}

/// C `ExecEvalRowNullInt` over a non-NULL composite `d` (detoasted first, as `DatumGetHeapTupleHeader`, into the scratch
/// context: only its null bitmap is read): true when every field that is not dropped is NULL (`nulls`) / is not NULL
/// (none at all: true either way).
pub(crate) fn fields_all(d: Datum, nulls: bool) -> PgResult<bool> {
    let p = d.as_usize() as *const u8;
    with_scratch(|mcx| {
        // SAFETY: a composite datum is a varlena image, live for the call.
        let owned = if unsafe { varlena_is_plain(p) } { None } else { Some(detoast_seams::detoast_attr::call(mcx, unsafe { core::slice::from_raw_parts(p, varatt::varsize_any(p)) })?) };
        let img = owned.as_ref().map_or(p, |v| v.as_ptr());
        // SAFETY: a plain composite image: MAXALIGN'd, its header's datum length long.
        let hdr = unsafe { &*(img as *const HeapTupleHeaderData) };
        // a named rowtype's cached descriptor, borrowed as C ExecEvalRowNullInt's lookup_rowtype_tupdesc borrows it: a copy per
        // row (CreateTupleDescCopyConstr) was a third of a whole-row IS [NOT] NULL under an aggregate FILTER; a registered
        // record's descriptor is copied
        let cached = if hdr.type_id() == types_core::catalog::RECORDOID { None } else { typcache::lookup_type_cache(hdr.type_id(), typcache::TYPECACHE_TUPDESC)?.tupdesc() };
        let copied = if cached.is_none() { Some(typcache::lookup_rowtype_tupdesc_copy(mcx, hdr.type_id(), hdr.typmod())?) } else { None };
        // SAFETY: as above; the view is never dropped (`forget`).
        let tuple = unsafe { HeapTupleData::from_raw_parts(img, hdr.datum_length(), ItemPointerData::invalid(), 0) };
        let all = match (&cached, &copied) {
            (Some(desc), _) => every_field(&tuple, desc, nulls),
            (None, Some(desc)) => every_field(&tuple, desc, nulls),
            (None, None) => unreachable!("a descriptor was looked up or copied"),
        };
        core::mem::forget(tuple);
        Ok(all)
    })
}

/// Every field of `tuple` that is not dropped is NULL (`nulls`) / is not NULL.
fn every_field(tuple: &HeapTupleData<'_>, desc: &types_tuple::TupleDescData<'_>, nulls: bool) -> bool {
    (1..=desc.natts).filter(|&a| !desc.attrs[(a - 1) as usize].attisdropped).all(|a| types_tuple::heap_attisnull(tuple, a, Some(desc)) == nulls)
}

#[inline]
pub fn eval_boolean_test(p: &Prog<'_, '_>, st: &mut St<'_>, n: &BooleanTestNode, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let v = eval(p, st, n.arg, cx)?;
    let (null, t) = (v.isnull, v.value.as_bool());
    let r = match n.test {
        BoolTestType::IS_TRUE => !null && t,
        BoolTestType::IS_NOT_TRUE => null || !t,
        BoolTestType::IS_FALSE => !null && !t,
        BoolTestType::IS_NOT_FALSE => null || t,
        BoolTestType::IS_UNKNOWN => null,
        BoolTestType::IS_NOT_UNKNOWN => !null,
    };
    Ok(NullableDatum::value(Datum::from_bool(r)))
}
