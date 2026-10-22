//! `FieldSelect` / `FieldStore` through the tail lane's tuple face (`tuple.rs`); the emitted `FieldSelect` is a strict
//! synthetic site per node (`FIELD_SITE`: the owner's descriptor (`Cell::aux`), a fact word, the composite's image).

use mcx::GrowIn;
use gravity_fn::{Addr, Arg, ArgKind, ErrSlot, Family, NullableDatum, Region, Ret, Shim, ERROR, NULL, OK};
use types_core::Oid;
use types_error::PgResult;
use types_nodes::primnodes::{FieldSelect as FieldSelectTree, FieldStore as FieldStoreTree};

use crate::eval::Cx;
use crate::nodes::func::{ArgConv, Call, How};
use crate::nodes::{Builder, Kids, NodeId, Prog, SiteId, St};
use crate::CompileResult;

#[derive(Clone, Copy, Debug)]
pub struct FieldSelect {
    pub arg: NodeId,
    pub fieldnum: i16,
    pub typ: Oid,
    /// The emitted select's site (`FIELD_SITE`).
    pub site: SiteId,
    /// The argument's named rowtype in `State::descs` (`NO_DESC` for `record`: the image's header names it per row).
    pub desc: u32,
}

pub const NO_DESC: u32 = u32::MAX;

/// `args[0]` the descriptor pointer (0 for `record`), `args[1]` the fact word, `args[2]` the composite's 4-byte-header
/// image (`ArgKind::I`); `byref` when the field's type is by reference (its datum points into the image).
pub fn field_site(b: &mut Builder<'_, '_>, byref: bool) -> SiteId {
    let convs = b.prog.convs.len() as u32;
    b.prog.convs.push_in(ArgConv { kind: ArgKind::V, typlen: 0 });
    b.prog.convs.push_in(ArgConv { kind: ArgKind::V, typlen: 0 });
    b.prog.convs.push_in(ArgConv { kind: ArgKind::I, typlen: -1 });
    b.site(Call { funcid: 0, args: Kids::NONE, nargs: 3, strict: true, collation: 0, how: How::Shim { shim: &FIELD_SITE, convs, byref }, rettype: 0, volatile: false })
}

pub fn word(n: &FieldSelect) -> i64 {
    n.fieldnum as u16 as i64 | (n.typ as i64) << 16
}

gravity_abi::host! {
    #[unwind]
    unsafe fn no_addr() {}
}

pub static FIELD_SITE: Shim = Shim { oid: 0, name: "field select", family: Family::None, nargs: 3, coll: false, region: true, args: &[ArgKind::V, ArgKind::V, ArgKind::I], tyargs: &[], addr: Addr(no_addr), call: field_shim };

fn field_shim(e: *mut ErrSlot, _region: *mut Region, _coll: u32, _ty: &[u32], a: &[Arg]) -> Ret {
    let w = a[1].v();
    let (fieldnum, typ) = (w as u16 as i16, (w >> 16) as u32);
    // SAFETY: `a[0]` is the owner's descriptor pointer (`Cell::aux`, program-long) or 0.
    let desc = unsafe { (a[0].v() as *const types_tuple::TupleDescData<'static>).as_ref() };
    match crate::nodes::tuple::field_of(a[2].r().ptr, fieldnum, typ, desc) {
        Ok(v) if v.isnull => Ret { val: 0, status: NULL },
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

/// A row-valued IS [NOT] NULL's site (nodes/null.rs `NullTestNode::site`, emit/scalar.rs `row_null_test`), strict: `args[0]`
/// the non-NULL composite's datum word, `args[1]` 1 for IS NULL / 0 for IS NOT NULL; the answer a bool word.
pub fn row_null_site(b: &mut Builder<'_, '_>) -> SiteId {
    let convs = b.prog.convs.len() as u32;
    b.prog.convs.push_in(ArgConv { kind: ArgKind::V, typlen: 0 });
    b.prog.convs.push_in(ArgConv { kind: ArgKind::V, typlen: 0 });
    b.site(Call { funcid: 0, args: Kids::NONE, nargs: 2, strict: true, collation: 0, how: How::Shim { shim: &ROW_NULL_SITE, convs, byref: false }, rettype: 0, volatile: false })
}

pub static ROW_NULL_SITE: Shim = Shim { oid: 0, name: "row null test", family: Family::None, nargs: 2, coll: false, region: false, args: &[ArgKind::V, ArgKind::V], tyargs: &[], addr: Addr(no_addr), call: row_null_shim };

/// C `ExecEvalRowNullInt` (nodes/null.rs `fields_all`, the interpreter's walk): the composite's detoast or its rowtype's
/// lookup can raise, into the site's slot. The rowtype comes from the backend's typcache, so a pool thread never calls it
/// (lowerv4 hir.rs `pool_unsafe_site`).
fn row_null_shim(e: *mut ErrSlot, _region: *mut Region, _coll: u32, _ty: &[u32], a: &[Arg]) -> Ret {
    match crate::nodes::null::fields_all(gravity_fn::Datum::from_i64(a[0].v()), a[1].v() != 0) {
        Ok(all) => Ret { val: all as i64, status: OK },
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

#[derive(Clone, Copy, Debug)]
pub struct FieldStore {
    pub arg: NodeId,
    pub newvals: Kids,
    pub fieldnums: u32,
    pub typ: Oid,
    pub desc: u32,
    /// The emitted store's site (`emit::store::FIELD_STORE_SITE`, a host site with no address in the IR).
    pub site: SiteId,
}

pub fn compile_field_select<'mcx>(b: &mut Builder<'_, 'mcx>, f: &FieldSelectTree<'mcx>) -> CompileResult<NodeId> {
    crate::nodes::tuple::compile_field_select(b, f)
}

pub fn compile_field_store<'mcx>(b: &mut Builder<'_, 'mcx>, f: &FieldStoreTree<'mcx>) -> CompileResult<NodeId> {
    crate::nodes::tuple::compile_field_store(b, f)
}

pub fn eval_field_select(p: &Prog<'_, '_>, st: &mut St<'_>, n: &FieldSelect, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    crate::nodes::tuple::eval_field_select(p, st, n, cx)
}

pub fn eval_field_store(p: &Prog<'_, '_>, st: &mut St<'_>, n: &FieldStore, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    crate::nodes::tuple::eval_field_store(p, st, n, cx)
}
