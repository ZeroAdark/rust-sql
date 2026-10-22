//! Arrays (the tail lane's file, expr-compiler.md §4.1): the element walk over an array image
//! (`ScalarArrayOpExpr`, `ArrayCoerceExpr`), the constructing nodes `ArrayExpr` (1-D and the
//! multidims concatenation) and `ArrayCoerceExpr` (the element expression over `CaseTest`), and
//! `SubscriptingRef` over the array handler (element and slice fetch / store, the old value for a
//! nested assignment through `CaseTest`) and the jsonb handler. Results are complete images in
//! the region; the arrayfuncs routines that build in an `Mcx` run on a retained scratch context.

use mcx::GrowIn;
use gravity_fn::{Addr, Arg, ArgKind, Datum, ErrSlot, Family, NullableDatum, Region, Ret, Shim, ERROR, OK};

use crate::nodes::func::{ArgConv, Call, How};
use mcx::Mcx;
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_nodes::primnodes::{ArrayCoerceExpr, ArrayExpr as ArrayExprTree, SubscriptingRef};
use types_nodes::NodeTag;
use types_tuple::varatt;

use crate::catalog::Catalog;
use crate::eval::{eval, Cx};
use crate::nodes::{unsupported, Builder, ExprNode, Kids, Kind, NodeId, Prog, SiteId, St};
use crate::views::{copy_into, datum_of, image_view, no_space};
use crate::CompileResult;

pub const MAXDIM: usize = 6;
const INT4OID: Oid = 23;
const F_ARRAY_SUBSCRIPT_HANDLER: Oid = 6179;
const F_RAW_ARRAY_SUBSCRIPT_HANDLER: Oid = 6180;
const F_JSONB_SUBSCRIPT_HANDLER: Oid = 6098;

pub use crate::catalog::ElemFacts;

pub fn elem_facts<'mcx>(cat: &dyn Catalog<'mcx>, array_type: Oid) -> CompileResult<ElemFacts> {
    elem_facts_of(cat, array_type, Kind::ScalarArrayOpExpr)
}

/// The element facts of `array_type`, refused as `kind` when the type has no element.
pub fn elem_facts_of<'mcx>(cat: &dyn Catalog<'mcx>, array_type: Oid, kind: Kind) -> CompileResult<ElemFacts> {
    let af = cat.type_facts(array_type)?;
    if af.elem == 0 {
        return unsupported(kind);
    }
    Ok(facts_of_elem(cat, af.elem)?)
}

pub fn facts_of_elem<'mcx>(cat: &dyn Catalog<'mcx>, elem: Oid) -> PgResult<ElemFacts> {
    let ef = cat.type_facts(elem)?;
    Ok(ElemFacts { typ: elem, len: ef.len, byval: ef.byval, align: ef.align })
}

impl ElemFacts {
    #[inline(always)]
    pub fn len_i32(&self) -> i32 {
        self.len as i32
    }
    #[inline(always)]
    pub fn align_u8(&self) -> u8 {
        self.align as u8
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ArrayExpr {
    pub elements: Kids,
    pub array_type: Oid,
    pub elem: ElemFacts,
    pub multidims: bool,
    /// The emitted constructor's site (`ARRAY_SITE`; the multidims form's `MD_SITE`, V4R-85).
    pub site: SiteId,
}

impl ElemFacts {
    /// The facts as one site word: the type, the length above it, byval at bit 48, the alignment above.
    pub fn word(&self) -> i64 {
        self.typ as i64 | (self.len as u16 as i64) << 32 | (self.byval as i64) << 48 | (self.align as u8 as i64) << 49
    }
    pub fn from_word(w: i64) -> ElemFacts {
        ElemFacts { typ: w as u32, len: (w >> 32) as u16 as i16, byval: w >> 48 & 1 != 0, align: (w >> 49) as u8 as i8 }
    }
}

/// The emitted 1-D constructor's site, a masked one (`func::masked`): the facts word, the null mask's words, then the
/// elements by value; NOT strict (a null element is a null cell, C `ExecEvalArrayExpr`). Past `MAX_EMITTED_ELEMS` elements
/// (PS4-10) it is `ARRAY_WIDE` (`MD_WIDE`) over its own cell, the count and the facts word, followed by its continuation
/// sites (`row::continuation_sites`), whose cells the emitted code stores the mask words and the elements into.
pub const ARRAY_HEAD: usize = 2;
pub const WIDE_HEAD: usize = 3;
pub const MAX_EMITTED_ELEMS: usize = crate::nodes::func::MAX_MASKED;

pub fn array_site(b: &mut Builder<'_, '_>, nelems: usize) -> SiteId {
    site_of(b, nelems, false)
}

fn site_of(b: &mut Builder<'_, '_>, nelems: usize, multidims: bool) -> SiteId {
    if crate::nodes::row::wide(nelems) {
        let convs = b.prog.convs.len() as u32;
        for _ in 0..WIDE_HEAD {
            b.prog.convs.push_in(ArgConv { kind: ArgKind::V, typlen: 0 });
        }
        let shim = if multidims { &MD_WIDE } else { &ARRAY_WIDE };
        let site = b.site(Call { funcid: 0, args: Kids::NONE, nargs: WIDE_HEAD as u8, strict: false, collation: 0, how: How::Shim { shim, convs, byref: true }, rettype: 0, volatile: false });
        crate::nodes::row::continuation_sites(b, site, nelems);
        return site;
    }
    let shim = if multidims { &MD_SITE } else { &ARRAY_SITE };
    let n = 1 + crate::nodes::func::mask_words(nelems) + nelems;
    let convs = b.prog.convs.len() as u32;
    for _ in 0..n {
        b.prog.convs.push_in(ArgConv { kind: ArgKind::V, typlen: 0 });
    }
    b.site(Call { funcid: 0, args: Kids::NONE, nargs: n as u8, strict: false, collation: 0, how: How::Shim { shim, convs, byref: true }, rettype: 0, volatile: false })
}

gravity_abi::host! {
    #[unwind]
    unsafe fn no_addr() {}
}

pub static ARRAY_SITE: Shim = Shim { oid: 0, name: "array constructor", family: Family::None, nargs: ARRAY_HEAD as u8, coll: false, region: true, args: &[ArgKind::V, ArgKind::V], tyargs: &[], addr: Addr(no_addr), call: array_shim };

/// The emitted multidims constructor's site (C `ExecEvalArrayExpr`'s multidims arm): `ARRAY_SITE`'s arguments, each
/// element an array (its pointer word; a null element is a mask bit, as C skips it), concatenated a dimension up (V4R-85).
pub static MD_SITE: Shim = Shim { oid: 0, name: "array constructor (multidims)", family: Family::None, nargs: ARRAY_HEAD as u8, coll: false, region: true, args: &[ArgKind::V, ArgKind::V], tyargs: &[], addr: Addr(no_addr), call: md_shim };

fn md_shim(e: *mut ErrSlot, region: *mut Region, _coll: u32, _ty: &[u32], a: &[Arg]) -> Ret {
    let elem = ElemFacts::from_word(a[0].v());
    let (masks, vals) = crate::nodes::func::masked(a);
    // SAFETY: the host passes the cell's live region.
    let region = unsafe { &mut *region };
    let r = cells(region, vals.len()).and_then(|subs| {
        for (i, x) in vals.iter().enumerate() {
            subs[i] = if crate::nodes::func::masked_null(masks, i) { NullableDatum::null() } else { NullableDatum::value(Datum::from_i64(x.v())) };
        }
        concat_arrays(&elem, subs, region)
    });
    ret_of(e, r)
}

fn array_shim(e: *mut ErrSlot, region: *mut Region, _coll: u32, _ty: &[u32], a: &[Arg]) -> Ret {
    let elem = ElemFacts::from_word(a[0].v());
    let (masks, vals) = crate::nodes::func::masked(a);
    let cells = vals.iter().enumerate().map(|(i, x)| (Datum::from_i64(x.v()), crate::nodes::func::masked_null(masks, i)));
    // SAFETY: the host passes the cell's live region.
    ret_of(e, form_1d(&elem, cells, vals.len(), unsafe { &mut *region }))
}

pub static ARRAY_WIDE: Shim = Shim { oid: 0, name: "array constructor wide", family: Family::None, nargs: WIDE_HEAD as u8, coll: false, region: true, args: &[ArgKind::V, ArgKind::V, ArgKind::V], tyargs: &[], addr: Addr(no_addr), call: wide_shim };

/// `MD_SITE` past one masked site's sub-arrays.
pub static MD_WIDE: Shim = Shim { oid: 0, name: "array constructor (multidims) wide", family: Family::None, nargs: WIDE_HEAD as u8, coll: false, region: true, args: &[ArgKind::V, ArgKind::V, ArgKind::V], tyargs: &[], addr: Addr(no_addr), call: md_wide_shim };

/// A wide site's cells: element `i` with whether it is NULL (mask word `i / 64`, the element word `mask_words(n) + i`).
///
/// # Safety
/// `cell` is the wide site's own cell of the program's live `EmitState` with its `n` elements' continuation cells stored
/// (`row::continued_word`).
unsafe fn wide_cells(cell: *const crate::emit::host::Cell, n: usize) -> impl Iterator<Item = (Datum, bool)> + Clone {
    let masks = crate::nodes::func::mask_words(n);
    // SAFETY: the caller's; every word read is below `wide_words(n)`.
    (0..n).map(move |i| unsafe { (Datum::from_i64(crate::nodes::row::continued_word(cell, masks + i)), (crate::nodes::row::continued_word(cell, i / 64) as u64) >> (i % 64) & 1 != 0) })
}

fn wide_shim(e: *mut ErrSlot, region: *mut Region, _coll: u32, _ty: &[u32], a: &[Arg]) -> Ret {
    let (cell, n, elem) = (a[0].v() as *const crate::emit::host::Cell, a[1].v() as usize, ElemFacts::from_word(a[2].v()));
    // SAFETY: the emitted code passes its own cell with the `more_sites(n)` cells after it holding the mask words and the
    // elements, stored before this call (`Em::wide_array`); the host passes the cell's live region.
    unsafe { ret_of(e, form_1d(&elem, wide_cells(cell, n), n, &mut *region)) }
}

fn md_wide_shim(e: *mut ErrSlot, region: *mut Region, _coll: u32, _ty: &[u32], a: &[Arg]) -> Ret {
    let (cell, n, elem) = (a[0].v() as *const crate::emit::host::Cell, a[1].v() as usize, ElemFacts::from_word(a[2].v()));
    // SAFETY: as `wide_shim`'s.
    let region = unsafe { &mut *region };
    let r = cells(region, n).and_then(|subs| {
        // SAFETY: as `wide_shim`'s.
        for (sub, (v, null)) in subs.iter_mut().zip(unsafe { wide_cells(cell, n) }) {
            *sub = if null { NullableDatum::null() } else { NullableDatum::value(v) };
        }
        concat_arrays(&elem, subs, region)
    });
    ret_of(e, r)
}

/// A constructor site's return: the image, or its error into the cell's slot.
fn ret_of(e: *mut ErrSlot, r: PgResult<NullableDatum>) -> Ret {
    match r {
        Ok(v) => Ret { val: v.value.as_i64(), status: OK },
        Err(err) => {
            // SAFETY: the host passes the cell's live slot; a previous error in it is dropped through `take`.
            unsafe {
                drop((*e).take());
                (*e).err = gravity_abi::word(Box::into_raw(err));
            }
            Ret { val: 0, status: ERROR }
        }
    }
}

/// C `ExecEvalArrayExpr`'s 1-D arm: the array image built in a scratch context, copied into the region; an array of
/// by-value elements is written in the region in place (`byval_1d`).
fn form_1d(elem: &ElemFacts, cells: impl Iterator<Item = (Datum, bool)> + Clone, count: usize, region: &mut Region) -> PgResult<NullableDatum> {
    if elem.byval && matches!(elem.len, 1 | 2 | 4 | 8) && count > 0 {
        return byval_1d(elem, cells, count, region);
    }
    with_scratch(|mcx| {
        let mut values = mcx::vec_with_capacity_in(mcx, count)?;
        let mut nulls = mcx::vec_with_capacity_in(mcx, count)?;
        for (v, n) in cells {
            values.push(v);
            nulls.push(n);
        }
        let img = arrayfuncs::construct_md_array(mcx, &values, Some(&nulls), 1, &[count as i32], &[1], elem.typ, elem.len_i32(), elem.byval, elem.align_u8())?;
        copy_into(region, datum_of(img.as_ptr()), -1).map(NullableDatum::value)
    })
}

/// PS4-18: construct_md_array's 1-D image of `count` by-value elements (C ExecEvalArrayExpr's 1-D arm), written in the
/// region in place: the header, the null bitmap when an element is NULL (bit set = present), then each present element
/// stored at its nominal alignment (ArrayCastAndSet's `store_att_byval`), every pad byte zero as palloc0 leaves it. The
/// same bytes as the scratch build (unit test `a_byval_array_is_construct_md_arrays_image`), without its two vectors,
/// its second pass over them and the copy.
fn byval_1d(elem: &ElemFacts, cells: impl Iterator<Item = (Datum, bool)> + Clone, count: usize, region: &mut Region) -> PgResult<NullableDatum> {
    use arrayfuncs::foundation::{arr_overhead_nonulls, arr_overhead_withnulls, att_align_nominal, ARRAYTYPE_HDRSZ};
    let (len, align) = (elem.len as usize, elem.align_u8());
    let step = att_align_nominal(len, align);
    let nulls = cells.clone().filter(|c| c.1).count();
    let head = if nulls > 0 { arr_overhead_withnulls(1, count as i32) } else { arr_overhead_nonulls(1) };
    let total = head + (count - nulls) * step;
    let p = region.reserve(total).ok_or_else(no_space)?;
    // SAFETY: `p .. p + total` is the region's, reserved above and 8-byte aligned; every byte is zeroed before any is read.
    unsafe {
        core::ptr::write_bytes(p, 0, total);
        let word = |at: usize, v: u32| core::ptr::write_unaligned(p.add(at) as *mut u32, v);
        // ArrayType: the 4-byte varlena header, ndim, dataoffset (0 without a bitmap), elemtype; then dims and lbounds
        p.cast::<[u8; 4]>().write(datum::varlena::set_varsize_4b(total));
        word(4, 1);
        word(8, if nulls > 0 { head as u32 } else { 0 });
        word(12, elem.typ);
        word(ARRAYTYPE_HDRSZ, count as u32);
        word(ARRAYTYPE_HDRSZ + 4, 1);
        let bitmap = p.add(ARRAYTYPE_HDRSZ + 8);
        let mut at = p.add(head);
        for (i, (v, null)) in cells.enumerate() {
            if null {
                continue;
            }
            if nulls > 0 {
                *bitmap.add(i / 8) |= 1 << (i % 8);
            }
            // store_att_byval: the datum's low `len` bytes as their C type
            let w = v.as_i64();
            match len {
                1 => at.write(w as u8),
                2 => core::ptr::write_unaligned(at as *mut u16, w as u16),
                4 => core::ptr::write_unaligned(at as *mut u32, w as u32),
                _ => core::ptr::write_unaligned(at as *mut i64, w),
            }
            at = at.add(step);
        }
    }
    Ok(NullableDatum::value(datum_of(p)))
}

#[derive(Clone, Copy, Debug)]
pub struct ArrayCoerce {
    pub arg: NodeId,
    pub elemexpr: Option<NodeId>,
    pub result_type: Oid,
    pub out: ElemFacts,
    pub inp: ElemFacts,
}

/// Which subscripting handler the container type names.
#[derive(Clone, Copy, Debug)]
pub enum Handler {
    /// The array handler: the container's typlen (> 0 for a fixed-length array type such as
    /// `oidvector`) and its element facts.
    Array { attrlen: i32, elem: ElemFacts },
    /// The jsonb handler: `int4_index[i]` says which subscripts are integers (text otherwise).
    Jsonb { int4_index: [bool; MAXDIM] },
    /// The hstore handler (contrib hstore_subs.c): one text key, no slice.
    Hstore,
}

/// `SubscriptingRef`: a missing subscript (an open slice bound) is `NodeId::NONE` in `upper` / `lower`.
#[derive(Clone, Copy, Debug)]
pub struct Subscript {
    pub container: NodeId,
    pub upper: Kids,
    pub lower: Kids,
    pub assign: Option<NodeId>,
    pub handler: Handler,
    pub result_type: Oid,
    pub container_is_param: bool,
    /// The assigned expression is a nested indirection over `CaseTest` (C
    /// `isAssignmentIndirectionExpr`): the old element is fetched for it.
    pub fetch_old: bool,
    /// The emitted fetch (`subscript::SUBSCRIPT_SITE`, a host site with no address in the IR); unused under an assignment.
    pub site: SiteId,
}

impl NodeId {
    pub const NONE: NodeId = NodeId(u32::MAX);
}

pub fn compile_array_expr<'mcx>(b: &mut Builder<'_, 'mcx>, a: &ArrayExprTree<'mcx>) -> CompileResult<NodeId> {
    let elem = facts_of_elem(b.catalog, a.element_typeid)?;
    let elements = b.kids(&a.elements)?;
    let site = site_of(b, elements.len as usize, a.multidims);
    Ok(b.push(ExprNode::Array(ArrayExpr { elements, array_type: a.array_typeid, elem, multidims: a.multidims, site })))
}

pub fn compile_array_coerce<'mcx>(b: &mut Builder<'_, 'mcx>, a: &ArrayCoerceExpr<'mcx>) -> CompileResult<NodeId> {
    let arg = b.compile(a.arg)?;
    let inp = elem_facts_of(b.catalog, nodes_core::expr_type(a.arg), Kind::ArrayCoerceExpr)?;
    let out = elem_facts_of(b.catalog, a.resulttype, Kind::ArrayCoerceExpr)?;
    let elemexpr = match a.elemexpr {
        Some(e) => Some(b.compile(e)?).filter(|&id| !trivial(b, id)),
        None => None,
    };
    // A trivial coercion over a one-dimension ARRAY[...] is that constructor building the result's element type (a
    // varchar column's generic-plan IN ($1, ..): PG's ARRAY[$1, ..]::varchar[] under a relabel to text[]): C
    // ExecEvalArrayCoerce without an element expression copies the image and sets ARR_ELEMTYPE, which is the image
    // construct_md_array builds from the same elements under the result's element type, as the layouts are one (a relabel
    // never changes it; checked). The emitter serves the constructor, where an ArrayCoerce is an interpreter island
    // (mod.rs `covered`). An element step (a function, a length or typmod check, an I/O coercion), a multidims constructor
    // (its sub-arrays keep their element type) or any other argument keeps the ArrayCoerce.
    if elemexpr.is_none() && (inp.len, inp.byval, inp.align) == (out.len, out.byval, out.align) {
        if let ExprNode::Array(x) = &mut b.prog.nodes[arg.0 as usize] {
            if !x.multidims && x.elem.typ == inp.typ {
                x.array_type = a.resulttype;
                x.elem = out;
                return Ok(arg);
            }
        }
    }
    Ok(b.push(ExprNode::ArrayCoerce(ArrayCoerce { arg, elemexpr, result_type: a.resulttype, out, inp })))
}

/// C ExecInitExprRec's `T_ArrayCoerceExpr` (execExpr.c): an element expression that compiles to its `CaseTestExpr` alone (a
/// `RelabelType` emits no step) is trivial, so the coercion does no per-element work: the image copied with its element
/// type set, its dimensions kept (an empty `int2vector` stays `[0:-1]`; the per-element path builds an empty array).
fn trivial(b: &Builder<'_, '_>, mut id: NodeId) -> bool {
    loop {
        match b.prog.nodes[id.0 as usize] {
            ExprNode::CaseTest => return true,
            ExprNode::Relabel(r) => id = r.arg,
            _ => return false,
        }
    }
}

pub fn compile_subscript<'mcx>(b: &mut Builder<'_, 'mcx>, s: &SubscriptingRef<'mcx>) -> CompileResult<NodeId> {
    let (typsubscript, _) = lsyscache::get_typsubscript(s.refcontainertype)?;
    if typsubscript == 0 {
        return Err(no_subscripting(s.refcontainertype).into());
    }
    let nupper = s.refupperindexpr.len();
    let nlower = s.reflowerindexpr.len();
    if nupper > MAXDIM || nlower > MAXDIM || (nlower != 0 && nlower != nupper) {
        return unsupported(Kind::SubscriptingRef);
    }
    let handler = match typsubscript {
        F_ARRAY_SUBSCRIPT_HANDLER | F_RAW_ARRAY_SUBSCRIPT_HANDLER => {
            let attrlen = b.catalog.type_facts(s.refcontainertype)?.len as i32;
            Handler::Array { attrlen, elem: facts_of_elem(b.catalog, s.refelemtype)? }
        }
        F_JSONB_SUBSCRIPT_HANDLER => {
            if nlower != 0 {
                return unsupported(Kind::SubscriptingRef);
            }
            let mut int4_index = [false; MAXDIM];
            for (i, e) in s.refupperindexpr.iter().enumerate() {
                int4_index[i] = e.is_some_and(|n| nodes_core::expr_type(n) == INT4OID);
            }
            Handler::Jsonb { int4_index }
        }
        // an extension's handler has no fixed oid: matched by its link symbol (prosrc), as the executor dispatches it, so
        // ALTER FUNCTION RENAME keeps the dispatch
        h if nupper == 1 && nlower == 0 && is_hstore_handler(h)? => Handler::Hstore,
        _ => return unsupported(Kind::SubscriptingRef),
    };
    let Some(refexpr) = s.refexpr else { return unsupported(Kind::SubscriptingRef) };
    let container = b.compile(refexpr)?;
    let mut ids = [NodeId::NONE; MAXDIM];
    for (i, e) in s.refupperindexpr.iter().enumerate() {
        if let Some(e) = e {
            ids[i] = b.compile(e)?;
        }
    }
    let upper = b.kids_of(&ids[..nupper]);
    let mut ids = [NodeId::NONE; MAXDIM];
    for (i, e) in s.reflowerindexpr.iter().enumerate() {
        if let Some(e) = e {
            ids[i] = b.compile(e)?;
        }
    }
    let lower = b.kids_of(&ids[..nlower]);
    let assign = match s.refassgnexpr {
        Some(e) => Some(b.compile(e)?),
        None => None,
    };
    let container_is_param = assign.is_some() && refexpr.node_tag() == NodeTag::T_Param;
    let fetch_old = s.refassgnexpr.is_some_and(is_indirection);
    let byref = !matches!(handler, Handler::Array { elem, .. } if nlower == 0 && elem.byval);
    let provided = s.refupperindexpr.iter().chain(s.reflowerindexpr.iter()).filter(|e| e.is_some()).count();
    // an assignment's site is the store's (emit/store.rs: the old value and the assignment); a fetch's the fetch's
    let site = if assign.is_some() { crate::emit::store::sbs_store_site(b, provided) } else { crate::nodes::subscript::fetch_site(b, provided, byref) };
    Ok(b.push(ExprNode::Subscripting(Subscript { container, upper, lower, assign, handler, result_type: s.refrestype, container_is_param, fetch_old, site })))
}

fn is_hstore_handler(funcid: Oid) -> PgResult<bool> {
    with_scratch(|mcx| Ok(lsyscache::get_func_prosrc(mcx, funcid)?.is_some_and(|n| n.as_str() == "hstore_subscript_handler")))
}

/// C `isAssignmentIndirectionExpr`: through a domain coercion and a relabel (a domain-typed field or element's store).
fn is_indirection(n: types_nodes::Node<'_>) -> bool {
    crate::stack_check();
    match n.node_tag() {
        NodeTag::T_FieldStore => n.as_field_store().unwrap().arg.node_tag() == NodeTag::T_CaseTestExpr,
        NodeTag::T_SubscriptingRef => n.as_subscripting_ref().unwrap().refexpr.is_some_and(|r| r.node_tag() == NodeTag::T_CaseTestExpr),
        NodeTag::T_CoerceToDomain => is_indirection(n.as_coerce_to_domain().unwrap().arg),
        NodeTag::T_RelabelType => is_indirection(n.as_relabel_type().unwrap().arg),
        _ => false,
    }
}

/// `n` `NullableDatum` cells in the region (a per-evaluation scratch the window reset reclaims).
#[inline]
fn cells<'a>(region: &mut Region, n: usize) -> PgResult<&'a mut [NullableDatum]> {
    let p = region.reserve(n * core::mem::size_of::<NullableDatum>()).ok_or_else(no_space)?;
    // SAFETY: `reserve` returned `n` cells' worth of 8-aligned bytes inside the caller's region.
    Ok(unsafe { core::slice::from_raw_parts_mut(p.cast::<NullableDatum>(), n) })
}

pub fn eval_array_expr(p: &Prog<'_, '_>, st: &mut St<'_>, n: &ArrayExpr, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let count = n.elements.len as usize;
    let elems = cells(cx.region, count)?;
    for (i, cell) in elems.iter_mut().enumerate() {
        *cell = eval(p, st, p.kids[n.elements.start as usize + i], cx)?;
    }
    if n.multidims {
        return concat_arrays(&n.elem, elems, cx.region);
    }
    form_1d(&n.elem, elems.iter().map(|e| (e.value, e.isnull)), count, cx.region)
}

/// The multidims arm of C `ExecEvalArrayExpr`: the element arrays concatenated into one of a
/// dimension more, straight into the region.
fn concat_arrays(elem: &ElemFacts, elems: &[NullableDatum], region: &mut Region) -> PgResult<NullableDatum> {
    let mut ndims = 0i32;
    let mut elem_ndims = 0i32;
    let mut elem_dims = [0i32; MAXDIM];
    let mut elem_lbs = [0i32; MAXDIM];
    let mut first = true;
    let mut havenulls = false;
    let mut haveempty = false;
    let mut nbytes = 0usize;
    let mut outer = 0usize;
    let subs = cells(region, elems.len())?;
    for e in elems.iter() {
        if e.isnull {
            haveempty = true;
            continue;
        }
        let img = image_view(e.value, -1, region)?;
        let arr = img.bytes();
        if arrayfuncs::arr_elemtype(arr) != elem.typ {
            return Err(array_merge_error(elem.typ, arrayfuncs::arr_elemtype(arr)));
        }
        let this_ndims = arrayfuncs::arr_ndim(arr);
        if this_ndims <= 0 {
            haveempty = true;
            continue;
        }
        if first {
            elem_ndims = this_ndims;
            ndims = elem_ndims + 1;
            if ndims as usize > MAXDIM {
                return Err(too_many_dims(ndims));
            }
            for i in 0..elem_ndims as usize {
                elem_dims[i] = arrayfuncs::arr_dim(arr, i);
                elem_lbs[i] = arrayfuncs::arr_lbound(arr, i);
            }
            first = false;
        } else {
            let same = elem_ndims == this_ndims && (0..elem_ndims as usize).all(|i| elem_dims[i] == arrayfuncs::arr_dim(arr, i) && elem_lbs[i] == arrayfuncs::arr_lbound(arr, i));
            if !same {
                return Err(dims_mismatch_error());
            }
        }
        nbytes += arrayfuncs::arr_size(arr) - arrayfuncs::arr_data_offset(arr);
        if nbytes > arrayfuncs::foundation::MAX_ALLOC_SIZE {
            return Err(array_too_large());
        }
        havenulls |= arrayfuncs::arr_hasnull(arr);
        subs[outer] = NullableDatum::value(datum_of(img.ptr));
        outer += 1;
    }
    if haveempty {
        if ndims == 0 {
            return empty_array(region, elem.typ);
        }
        return Err(dims_mismatch_error());
    }
    let mut dims = [0i32; MAXDIM];
    let mut lbs = [0i32; MAXDIM];
    dims[0] = outer as i32;
    lbs[0] = 1;
    for i in 1..ndims as usize {
        dims[i] = elem_dims[i - 1];
        lbs[i] = elem_lbs[i - 1];
    }
    let nitems = arrayutils::array_get_n_items(ndims, &dims)?;
    arrayutils::array_check_bounds(ndims, &dims, &lbs)?;
    let dataoffset = if havenulls { arrayfuncs::foundation::arr_overhead_withnulls(ndims, nitems) } else { 0 };
    let total = nbytes + if havenulls { dataoffset } else { arrayfuncs::foundation::arr_overhead_nonulls(ndims) };
    let out = region.reserve(total).ok_or_else(no_space)?;
    // SAFETY: `total` bytes reserved; the header, dims and data are written in order below.
    let out = unsafe { core::slice::from_raw_parts_mut(out, total) };
    out.fill(0);
    out[0..4].copy_from_slice(&datum::set_varsize_4b(total));
    out[4..8].copy_from_slice(&ndims.to_ne_bytes());
    out[8..12].copy_from_slice(&(dataoffset as i32).to_ne_bytes());
    out[12..16].copy_from_slice(&elem.typ.to_ne_bytes());
    let mut off = 16usize;
    for d in &dims[..ndims as usize] {
        out[off..off + 4].copy_from_slice(&d.to_ne_bytes());
        off += 4;
    }
    for l in &lbs[..ndims as usize] {
        out[off..off + 4].copy_from_slice(&l.to_ne_bytes());
        off += 4;
    }
    let dest_bitmap = arrayfuncs::foundation::arr_nullbitmap_off(out);
    let mut dat = arrayfuncs::arr_data_offset(out);
    let mut iitem = 0i32;
    for s in &subs[..outer] {
        // SAFETY: each entry is a plain image `image_view` sized above.
        let sub = unsafe { core::slice::from_raw_parts(s.value.as_usize() as *const u8, varatt::varsize_4b(s.value.as_usize() as *const u8)) };
        let data_off = arrayfuncs::arr_data_offset(sub);
        let bytes = arrayfuncs::arr_size(sub) - data_off;
        out[dat..dat + bytes].copy_from_slice(&sub[data_off..data_off + bytes]);
        dat += bytes;
        let (nd, sd, _) = arrayfuncs::read_dims_lbounds(sub);
        let nitems = arrayutils::array_get_n_items(nd, &sd)?;
        if let Some(dbo) = dest_bitmap {
            arrayfuncs::element::array_bitmap_copy(out, dbo, iitem, arrayfuncs::foundation::arr_nullbitmap_off(sub).map(|bo| (sub, bo)), 0, nitems);
        }
        iitem += nitems;
    }
    Ok(NullableDatum::value(datum_of(out.as_ptr())))
}

/// An empty array image (C `construct_empty_array`) in the region.
pub fn empty_array(region: &mut Region, elemtype: Oid) -> PgResult<NullableDatum> {
    let out = region.reserve(16).ok_or_else(no_space)?;
    // SAFETY: 16 bytes reserved.
    let out = unsafe { core::slice::from_raw_parts_mut(out, 16) };
    out.fill(0);
    out[0..4].copy_from_slice(&datum::set_varsize_4b(16));
    out[12..16].copy_from_slice(&elemtype.to_ne_bytes());
    Ok(NullableDatum::value(datum_of(out.as_ptr())))
}

pub fn eval_array_coerce(p: &Prog<'_, '_>, st: &mut St<'_>, n: &ArrayCoerce, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let v = eval(p, st, n.arg, cx)?;
    if v.isnull {
        return Ok(v);
    }
    let img = image_view(v.value, -1, cx.region)?;
    let Some(elemexpr) = n.elemexpr else {
        let d = copy_into(cx.region, datum_of(img.ptr), -1)?;
        // SAFETY: the copy is a plain image of at least 16 bytes; word 3 is the element type.
        unsafe { (d.as_usize() as *mut u8).add(12).cast::<u32>().write_unaligned(n.out.typ) };
        return Ok(NullableDatum::value(d));
    };
    let arr = img.bytes();
    let elemtype = arrayfuncs::arr_elemtype(arr);
    let inp = if elemtype == n.inp.typ { n.inp } else { runtime_elem_facts(elemtype)? };
    let (ndim, dims, lbs) = arrayfuncs::read_dims_lbounds(arr);
    let mut walk = Walk::over(datum_of(img.ptr), inp, cx.region)?;
    let count = walk.len();
    if count == 0 {
        return empty_array(cx.region, n.out.typ);
    }
    let outs = cells(cx.region, count)?;
    let saved = cx.casetest;
    let mut i = 0;
    let r = loop {
        let Some(e) = walk.next() else { break Ok(()) };
        cx.casetest = e;
        // a by-reference element lives in its site's result context only until the site's next call (the next element):
        // copied into the region first, as C's per-element results live in the per-tuple context
        let r = eval(p, st, elemexpr, cx).and_then(|r| if r.isnull || n.out.byval { Ok(r) } else { copy_into(cx.region, r.value, n.out.len).map(NullableDatum::value) });
        match r {
            Ok(r) => outs[i] = r,
            Err(err) => break Err(err),
        }
        i += 1;
    };
    cx.casetest = saved;
    r?;
    with_scratch(|mcx| {
        let mut values = mcx::vec_with_capacity_in(mcx, count)?;
        let mut nulls = mcx::vec_with_capacity_in(mcx, count)?;
        let mut hasnulls = false;
        for o in outs.iter() {
            values.push(o.value);
            nulls.push(o.isnull);
            hasnulls |= o.isnull;
        }
        let img = arrayfuncs::construct_md_array(mcx, &values, hasnulls.then_some(&nulls[..]), ndim, &dims[..ndim as usize], &lbs[..ndim as usize], n.out.typ, n.out.len_i32(), n.out.byval, n.out.align_u8())?;
        copy_into(cx.region, datum_of(img.as_ptr()), -1).map(NullableDatum::value)
    })
}

#[cold]
#[inline(never)]
fn runtime_elem_facts(elemtype: Oid) -> PgResult<ElemFacts> {
    let (len, byval, align) = gravity_fn::typesem::expr::layout(elemtype)?;
    Ok(ElemFacts { typ: elemtype, len, byval, align })
}

/// The subscripts of one evaluation: C `array_subscript_check_subscripts`' output.
pub(crate) struct Subs {
    pub(crate) upper: [i32; MAXDIM],
    pub(crate) lower: [i32; MAXDIM],
    pub(crate) upper_provided: [bool; MAXDIM],
    pub(crate) lower_provided: [bool; MAXDIM],
    /// The jsonb handler's index datums (text images for integer subscripts).
    pub(crate) index: [Datum; MAXDIM],
}

impl Subs {
    pub(crate) fn new() -> Subs {
        Subs { upper: [0; MAXDIM], lower: [0; MAXDIM], upper_provided: [false; MAXDIM], lower_provided: [false; MAXDIM], index: [Datum::null(); MAXDIM] }
    }
}

/// Evaluate the subscripts; `Ok(false)` when a fetch subscript was null (the result is null). Every subscript is
/// evaluated before the nulls are looked at (C: the `SBSREF_SUBSCRIPTS` step follows all of the subscripts' steps),
/// so a later subscript's raise is not lost to an earlier null.
fn eval_subscripts(p: &Prog<'_, '_>, st: &mut St<'_>, n: &Subscript, cx: &mut Cx<'_, '_>, s: &mut Subs) -> PgResult<bool> {
    let assign = n.assign.is_some();
    let mut null_seen = false;
    for (list, idx, provided) in [(n.upper, &mut s.upper, &mut s.upper_provided), (n.lower, &mut s.lower, &mut s.lower_provided)] {
        for i in 0..list.len as usize {
            let id = p.kids[list.start as usize + i];
            if id == NodeId::NONE {
                provided[i] = false;
                continue;
            }
            provided[i] = true;
            let v = eval(p, st, id, cx)?;
            if v.isnull {
                null_seen = true;
                continue;
            }
            s.index[i] = index_of(n.handler, i, v.value.as_i64(), cx.region)?;
            idx[i] = v.value.as_i32();
        }
    }
    if null_seen && assign {
        return Err(null_subscript_error(n.handler));
    }
    Ok(!null_seen)
}

/// Subscript `i`'s value as the jsonb / hstore handler indexes by: its text, an integer one's as text.
pub(crate) fn index_of(h: Handler, i: usize, v: i64, region: &mut Region) -> PgResult<Datum> {
    match h {
        Handler::Jsonb { int4_index } if int4_index[i] => int4_as_text(v as i32, region),
        _ => Ok(Datum::from_i64(v)),
    }
}

/// An integer subscript as the text the jsonb handler indexes by (C `jsonb_subscript_check_subscripts`).
pub(crate) fn int4_as_text(v: i32, region: &mut Region) -> PgResult<Datum> {
    let mut buf = [0u8; 12];
    let len = numutils::pg_ltoa(v, &mut buf);
    let out = region.reserve(len + 4).ok_or_else(no_space)?;
    // SAFETY: `len + 4` bytes reserved.
    unsafe {
        out.cast::<[u8; 4]>().write_unaligned(datum::set_varsize_4b(len + 4));
        core::ptr::copy_nonoverlapping(buf.as_ptr(), out.add(4), len);
    }
    Ok(datum_of(out))
}

pub fn eval_subscript(p: &Prog<'_, '_>, st: &mut St<'_>, n: &Subscript, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let cur = eval(p, st, n.container, cx)?;
    if n.assign.is_none() && cur.isnull {
        return Ok(cur);
    }
    let mut s = Subs::new();
    if !eval_subscripts(p, st, n, cx, &mut s)? {
        return Ok(NullableDatum::null());
    }
    let Some(assign) = n.assign else {
        return fetch(n.handler, n.upper.len as usize, n.lower.len != 0, cur, &mut s, cx.region);
    };
    // The old element reaches a nested indirection through `CaseTest` (C `EEOP_SBSREF_OLD`).
    let saved = cx.casetest;
    if n.fetch_old {
        cx.casetest = if cur.isnull { NullableDatum::null() } else { fetch(n.handler, n.upper.len as usize, n.lower.len != 0, cur, &mut s, cx.region)? };
    }
    let replace = eval(p, st, assign, cx);
    cx.casetest = saved;
    let replace = replace?;
    match n.handler {
        Handler::Array { attrlen, elem } => assign_array(n.upper.len as usize, n.lower.len != 0, n.container_is_param, attrlen, elem, cur, replace, &mut s, cx.region),
        Handler::Jsonb { int4_index } => assign_jsonb(int4_index, n.upper.len as usize, cur, replace, &s, cx.region),
        Handler::Hstore => assign_hstore(cur, replace, &s, cx.region),
    }
}

/// The hstore handler's assignment (C `hstore_subscript_assign`): one body for the interpreter and the emitted store.
pub(crate) fn assign_hstore(cur: NullableDatum, replace: NullableDatum, s: &Subs, region: &mut Region) -> PgResult<NullableDatum> {
    with_scratch(|mcx| {
        let d = hstore_subs_seams::hstore_subs_assign::call(mcx, cur, s.index[0], replace)?;
        copy_into(region, d, -1).map(NullableDatum::value)
    })
}

/// The jsonb handler's assignment: one body for the interpreter and the emitted store (emit/store.rs).
pub(crate) fn assign_jsonb(int4_index: [bool; MAXDIM], nupper: usize, cur: NullableDatum, replace: NullableDatum, s: &Subs, region: &mut Region) -> PgResult<NullableDatum> {
    let expect_array = nupper > 0 && int4_index[0];
    with_scratch(|mcx| {
        let d = adt_jsonb::subs::subscript_assign(mcx, cur, expect_array, &s.index[..nupper], replace)?;
        copy_into(region, d, -1).map(NullableDatum::value)
    })
}

/// The fetch over checked subscripts: one body for the interpreter and for the emitted site (`subscript::SUBSCRIPT_SITE`).
pub(crate) fn fetch(handler: Handler, nupper: usize, slice: bool, cur: NullableDatum, s: &mut Subs, region: &mut Region) -> PgResult<NullableDatum> {
    match handler {
        Handler::Array { attrlen, elem } => {
            let arr = container_bytes(cur.value, attrlen, region)?;
            if !slice {
                let (value, isnull) = arrayfuncs::array_get_element(arr, &s.upper[..nupper], attrlen, elem.len_i32(), elem.byval, elem.align_u8());
                return Ok(NullableDatum { value, isnull });
            }
            with_scratch(|mcx| {
                let img = arrayfuncs::array_get_slice(mcx, arr, nupper as i32, &mut s.upper, &mut s.lower, &s.upper_provided, &s.lower_provided, attrlen, elem.len_i32(), elem.align_u8())?;
                copy_into(region, datum_of(img.as_ptr()), -1).map(NullableDatum::value)
            })
        }
        Handler::Jsonb { .. } | Handler::Hstore => with_scratch(|mcx| {
            let r = match handler {
                Handler::Hstore => hstore_subs_seams::hstore_subs_fetch::call(mcx, cur.value, s.index[0])?,
                _ => adt_jsonb::subs::subscript_fetch(mcx, cur.value, &s.index[..nupper])?,
            };
            if r.isnull {
                return Ok(r);
            }
            copy_into(region, r.value, -1).map(NullableDatum::value)
        }),
    }
}

/// The container's bytes: a fixed-length array type's `attrlen` bytes, or the plain image.
fn container_bytes<'a>(d: Datum, attrlen: i32, region: &mut Region) -> PgResult<&'a [u8]> {
    if attrlen > 0 {
        // SAFETY: a fixed-length by-reference datum addresses `attrlen` bytes.
        return Ok(unsafe { core::slice::from_raw_parts(d.as_usize() as *const u8, attrlen as usize) });
    }
    Ok(image_view(d, -1, region)?.bytes())
}

/// The array handler's assignment (one body for the interpreter and the emitted store, emit/store.rs).
#[allow(clippy::too_many_arguments)]
pub(crate) fn assign_array(nupper: usize, slice: bool, container_is_param: bool, attrlen: i32, elem: ElemFacts, cur: NullableDatum, replace: NullableDatum, s: &mut Subs, region: &mut Region) -> PgResult<NullableDatum> {
    if attrlen > 0 && (cur.isnull || replace.isnull) {
        return Ok(cur);
    }
    let arr: &[u8] = if cur.isnull {
        let e = empty_array(region, elem.typ)?;
        // SAFETY: the 16-byte empty image written just above.
        unsafe { core::slice::from_raw_parts(e.value.as_usize() as *const u8, 16) }
    } else {
        container_bytes(cur.value, attrlen, region)?
    };
    if !slice {
        let mut replace = replace;
        if elem.len == -1 && !replace.isnull {
            replace.value = datum_of(image_view(replace.value, -1, region)?.ptr);
        }
        return with_scratch(|mcx| {
            let img = arrayfuncs::element::array_set_element_ext(mcx, arr, &s.upper[..nupper], replace.value, replace.isnull, attrlen, elem.len_i32(), elem.byval, elem.align_u8(), container_is_param && !cur.isnull)?;
            // D772: a fixed-length container's result is its `attrlen` bytes (array_set_element's copy of the whole
            // datum), not a varlena: copied as one, its first bytes read as a length header lost the value
            copy_into(region, datum_of(img.as_ptr()), if attrlen > 0 { attrlen as i16 } else { -1 }).map(NullableDatum::value)
        });
    }
    // C array_set_slice: a fixed-length array type's slice update raises before the replacement is read (it is not an
    // array image: read as one, a name's bytes were taken for a varlena header)
    if attrlen > 0 {
        return Err(fixed_slice_update());
    }
    if replace.isnull {
        return Ok(if cur.isnull { NullableDatum::value(datum_of(arr.as_ptr())) } else { cur });
    }
    let src = image_view(replace.value, -1, region)?.bytes();
    with_scratch(|mcx| {
        let img = arrayfuncs::array_set_slice(mcx, arr, nupper as i32, &mut s.upper, &mut s.lower, &s.upper_provided, &s.lower_provided, src, attrlen, elem.len_i32(), elem.byval, elem.align_u8())?;
        copy_into(region, datum_of(img.as_ptr()), -1).map(NullableDatum::value)
    })
}

thread_local! {
    // alloc: Node
    static SCRATCH: gravity_fn::scratch::ScratchSlot = gravity_fn::scratch::ScratchSlot::new("gravity_expr array scratch", "gravity_expr array scratch (nested)");
}

/// Run an arrayfuncs routine that builds in an `Mcx` on the thread's retained scratch context;
/// the caller copies its result into the region inside `f`. A re-entered call gets its own.
pub fn with_scratch<R>(f: impl FnOnce(Mcx<'_>) -> PgResult<R>) -> PgResult<R> {
    SCRATCH.with(|s| s.with(f))
}

#[cold]
#[inline(never)]
fn no_subscripting(typ: Oid) -> Box<PgError> {
    let name = format_type::format_type_be(typ).unwrap_or_else(|_| typ.to_string()); // alloc: Cold
    Box::new(PgError::error(format!("cannot subscript type {name} because it does not support subscripting")).with_sqlstate(types_error::ERRCODE_DATATYPE_MISMATCH)) // alloc: Cold
}

#[cold]
#[inline(never)]
fn fixed_slice_update() -> Box<PgError> {
    Box::new(PgError::error("updates on slices of fixed-length arrays not implemented").with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED)) // alloc: Cold
}

#[cold]
#[inline(never)]
pub(crate) fn null_subscript_error(h: Handler) -> Box<PgError> {
    let what = match h {
        Handler::Array { .. } => "array",
        Handler::Jsonb { .. } => "jsonb",
        Handler::Hstore => "hstore",
    };
    Box::new(PgError::error(format!("{what} subscript in assignment must not be null")).with_sqlstate(types_error::ERRCODE_NULL_VALUE_NOT_ALLOWED)) // alloc: Cold
}

#[cold]
#[inline(never)]
fn array_merge_error(want: Oid, got: Oid) -> Box<PgError> {
    let w = format_type::format_type_be(want).unwrap_or_else(|_| want.to_string()); // alloc: Cold
    let g = format_type::format_type_be(got).unwrap_or_else(|_| got.to_string()); // alloc: Cold
    Box::new(PgError::error("cannot merge incompatible arrays").with_sqlstate(types_error::ERRCODE_DATATYPE_MISMATCH).with_detail(format!("Array with element type {g} cannot be included in ARRAY construct with element type {w}."))) // alloc: Cold
}

#[cold]
#[inline(never)]
fn dims_mismatch_error() -> Box<PgError> {
    Box::new(PgError::error("multidimensional arrays must have array expressions with matching dimensions").with_sqlstate(types_error::ERRCODE_ARRAY_SUBSCRIPT_ERROR)) // alloc: Cold
}

#[cold]
#[inline(never)]
fn too_many_dims(ndims: i32) -> Box<PgError> {
    Box::new(PgError::error(format!("number of array dimensions ({ndims}) exceeds the maximum allowed ({MAXDIM})")).with_sqlstate(types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED)) // alloc: Cold
}

#[cold]
#[inline(never)]
fn array_too_large() -> Box<PgError> {
    Box::new(PgError::error(format!("array size exceeds the maximum allowed ({})", arrayfuncs::foundation::MAX_ALLOC_SIZE)).with_sqlstate(types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED)) // alloc: Cold
}

#[inline(always)]
fn align_of(c: i8) -> usize {
    match c as u8 {
        b'd' => 8,
        b'i' => 4,
        b's' => 2,
        _ => 1,
    }
}

#[inline(always)]
fn align_up(p: usize, a: usize) -> usize {
    (p + a - 1) & !(a - 1)
}

/// The elements of one array image in order (C `array_iter_next`): the null bitmap, the element
/// alignment and the varlena / cstring lengths as `ArrayType` lays them out.
pub struct Walk {
    data: *const u8,
    bitmap: *const u8,
    n: usize,
    i: usize,
    off: usize,
    elem: ElemFacts,
}

impl Walk {
    pub fn over(d: Datum, elem: ElemFacts, region: &mut Region) -> PgResult<Walk> {
        let img = image_view(d, -1, region)?;
        let p = img.ptr;
        // SAFETY: `img` is a plain 4-byte-header array image the view sized.
        unsafe {
            let ndim = p.add(4).cast::<i32>().read_unaligned() as usize;
            let dataoffset = p.add(8).cast::<i32>().read_unaligned() as usize;
            let mut n = if ndim == 0 { 0 } else { 1usize };
            for k in 0..ndim {
                n *= p.add(16 + 4 * k).cast::<i32>().read_unaligned() as usize;
            }
            let hdr = 16 + 8 * ndim;
            let (bitmap, data) = if dataoffset == 0 {
                (core::ptr::null(), p.add(align_up(hdr, 8)))
            } else {
                (p.add(hdr), p.add(dataoffset))
            };
            Ok(Walk { data, bitmap, n, i: 0, off: 0, elem })
        }
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    #[inline]
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<NullableDatum> {
        if self.i >= self.n {
            return None;
        }
        let i = self.i;
        self.i += 1;
        // SAFETY: the walk stays inside the image `over` sized; the bitmap has `n` bits.
        unsafe {
            if !self.bitmap.is_null() && (*self.bitmap.add(i / 8) >> (i % 8)) & 1 == 0 {
                return Some(NullableDatum::null());
            }
            let value = if self.elem.byval {
                let q = self.data.add(self.off);
                let v = match self.elem.len {
                    1 => Datum::from_u64(*q as u64),
                    2 => Datum::from_i16(q.cast::<i16>().read_unaligned()),
                    4 => Datum::from_i32(q.cast::<i32>().read_unaligned()),
                    _ => Datum::from_u64(q.cast::<u64>().read_unaligned()),
                };
                self.off += self.elem.len as usize;
                v
            } else if self.elem.len > 0 {
                let q = self.data.add(self.off);
                self.off += self.elem.len as usize;
                Datum::from_usize(q as usize)
            } else if self.elem.len == -1 {
                let q = self.data.add(self.off);
                self.off += varatt::varsize_any(q);
                Datum::from_usize(q as usize)
            } else {
                let q = self.data.add(self.off);
                self.off += core::ffi::CStr::from_ptr(q.cast()).to_bytes().len() + 1;
                Datum::from_usize(q as usize)
            };
            self.off = align_up(self.off, align_of(self.elem.align));
            Some(NullableDatum::value(value))
        }
    }
}

#[cfg(test)]
mod d571 {
    use types_nodes::node_tree::Node;
    use types_nodes::primnodes::{CaseTestExpr, CoerceToDomain, CoercionForm, FieldStore, RelabelType};

    /// D571: C `isAssignmentIndirectionExpr` looks through a CoerceToDomain and a RelabelType, so the old element reaches a
    /// nested store under a domain-typed element (`f[1].x = ..` over an array of a domain over a composite); the
    /// interpreter's `is_indirection` stopped at them and the nested store read no old value.
    #[test]
    fn d571_indirection_looks_through_a_domain_and_a_relabel() {
        let cx = mcx::MemoryContext::new("d571");
        let m = cx.mcx();
        let ct = Node::mk(m, CaseTestExpr { typeId: 2249, typeMod: -1, collation: 0 }).unwrap();
        let fs = Node::mk(m, FieldStore { arg: ct, newvals: types_nodes::list::NodeList::nil(), fieldnums: types_nodes::list::IntList::nil(), resulttype: 2249 }).unwrap();
        let dom = Node::mk(m, CoerceToDomain { arg: fs, resulttype: 90001, resulttypmod: -1, resultcollid: 0, coercionformat: CoercionForm::COERCE_IMPLICIT_CAST, location: -1 }).unwrap();
        let rel = Node::mk(m, RelabelType { arg: fs, resulttype: 2249, resulttypmod: -1, resultcollid: 0, relabelformat: CoercionForm::COERCE_IMPLICIT_CAST, location: -1 }).unwrap();
        assert!(super::is_indirection(fs));
        assert!(super::is_indirection(dom), "through a CoerceToDomain");
        assert!(super::is_indirection(rel), "through a RelabelType");
        let plain = Node::mk(m, RelabelType { arg: ct, resulttype: 2249, resulttypmod: -1, resultcollid: 0, relabelformat: CoercionForm::COERCE_IMPLICIT_CAST, location: -1 }).unwrap();
        assert!(!super::is_indirection(plain), "a relabelled CaseTest is no store");
    }
}

#[cfg(test)]
mod ps418 {
    use super::{byval_1d, with_scratch, ElemFacts};
    use gravity_fn::{Datum, Region};

    /// PS4-18: the in-place build of a by-value 1-D array is construct_md_array's image byte for byte: char / int2 / int4
    /// / int8 / float8 / bool / oid elements, 1 to 97 of them, no NULL, the first, the last, every other one, all NULL.
    #[test]
    fn a_byval_array_is_construct_md_arrays_image() {
        let types = [(18u32, 1i16, b'c'), (21, 2, b's'), (23, 4, b'i'), (20, 8, b'd'), (701, 8, b'd'), (16, 1, b'c'), (26, 4, b'i')];
        for (typ, len, align) in types {
            let elem = ElemFacts { typ, len, byval: true, align: align as i8 };
            for n in [1usize, 2, 7, 8, 9, 16, 62, 64, 97] {
                for pattern in 0..5 {
                    let null = |i: usize| match pattern {
                        0 => false,
                        1 => i == 0,
                        2 => i == n - 1,
                        3 => i % 2 == 1,
                        _ => true,
                    };
                    let cells: Vec<(Datum, bool)> = (0..n).map(|i| (Datum::from_i64((i as i64 * 0x0101_0101_0101_0101 - 3) >> (64 - 8 * len as i64).min(63)), null(i))).collect(); // alloc: Cold
                    let want: Vec<u8> = with_scratch(|mcx| {
                        let (v, k): (Vec<Datum>, Vec<bool>) = cells.iter().copied().unzip(); // alloc: Cold
                        Ok(arrayfuncs::construct_md_array(mcx, &v, Some(&k), 1, &[n as i32], &[1], typ, len as i32, true, align)?.to_vec()) // alloc: Cold
                    })
                    .unwrap();
                    let mut buf = vec![0xa5u8; 4096]; // alloc: Cold
                    let mut region = Region::over(&mut buf);
                    let got = byval_1d(&elem, cells.iter().copied(), n, &mut region).unwrap();
                    // SAFETY: the image lies in `buf`, its length the `want` image's.
                    let got = unsafe { core::slice::from_raw_parts(got.value.as_usize() as *const u8, want.len()) };
                    assert_eq!(got, &want[..], "type {typ}, {n} elements, NULL pattern {pattern}");
                    let pad = (8 - buf.as_ptr() as usize % 8) % 8;
                    assert_eq!(region.used(buf.as_ptr()), pad + want.len(), "type {typ}, {n} elements: the region holds the image alone");
                }
            }
        }
    }
}
