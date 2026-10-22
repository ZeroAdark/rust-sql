//! The write-side expression forms as host sites (card E-a, M4-plan Decision 17; lane M4-EXPR): the `SubscriptingRef`
//! store, `FieldStore` and `NextValueExpr`. No interpreter island: an island refuses a Param (every prepared
//! `SET f[1] = $1`), and nextval would pay the interpreter per row.
//!
//! - **Store** (C `ExecInitSubscriptingRef`, assignment): the container, every subscript (evaluated even over a null
//!   container), then — only where no subscript is null and nothing raised — the old value (`fetch_old`, C
//!   `EEOP_SBSREF_OLD`, the `CaseTest` of a nested indirection) and the assigned expression, then the store. A null
//!   subscript is the store's raise, before the assigned expression runs. One site, two calls (`OLD`, `ASSIGN`).
//! - **FieldStore** (C `EEOP_FIELDSTORE_DEFORM` / `FORM`): the composite, then each new value in order, a new value
//!   that reads `CaseTest` (a nested indirection) seeing its field as stored so far: the pending fields are formed
//!   first and the field fetched from that image. The fields between are one call (`FIELD_STORE_BATCH` pairs), so a plain
//!   `SET c.a = .., c.b = ..` forms once, as C does.
//! - **NextValueExpr** (C `ExecEvalNextValueExpr`): `nextval_internal(seqid, false)` as a VOLATILE site with the
//!   nextval oid, so kind `expr.volatile` holds it (never twice on a row, never ahead) and the pool never runs it.
//!   Contract for the write kinds (write/insert.rs, write/update.rs): PG evaluates a target list in resno order and
//!   stops at the first raise, and a sequence advance survives the rollback, so a column holding a NextValueExpr must
//!   be evaluated only when no earlier column of the row raised.
//!
//! Every call here runs only while the row has not raised (the error PG reports is the first one, and nothing after
//! it runs); an operand after a raise is not evaluated (`guarded`).

use gravity_fn::{Arg, ArgKind, Datum, ErrSlot, Family, NullableDatum, Region, Ret, Shim, ERROR, NULL, OK};
use gravity_ir::{IntCond, Type, Value};
use mcx::{GrowIn, PgVec};
use gravity_program::cost::expr::FIELD_STORE_BATCH;
use types_error::PgResult;

use super::host::CELL_AUX;
use super::scalar::Em;
use super::{EmitError, Lowered};
use crate::nodes::array::{assign_array, assign_hstore, assign_jsonb, fetch, index_of, null_subscript_error, Handler, Subs, MAXDIM};
use crate::nodes::field::FieldStore;
use crate::nodes::func::{ArgConv, Call, How};
use crate::nodes::misc::NextValue;
use crate::nodes::subscript::{handler_of, words, Subscripting, SLICE};
use crate::nodes::{Builder, ExprNode, Kids, NodeId, SiteId};

/// The store's arguments before the subscripts: the two fact words, the control word, the container, the new value.
const SBS_HEAD: usize = 5;
const ASSIGN: i64 = 1;
const CUR_NULL: i64 = 2;
const REP_NULL: i64 = 4;
const SUB_NULL: i64 = 8;
const PARAM: i64 = 16;

/// FieldStore: the descriptor, the control word, the composite, then (field number, value) pairs.
const FS_HEAD: usize = 3;
const FS_STORE: i64 = 1;
const FS_CUR_NULL: i64 = 2;

const F_NEXTVAL: u32 = 1574;

fn site(b: &mut Builder<'_, '_>, funcid: u32, nargs: usize, shim: &'static Shim, byref: bool, rettype: u32, volatile: bool) -> SiteId {
    let convs = b.prog.convs.len() as u32;
    for _ in 0..nargs {
        b.prog.convs.push_in(ArgConv { kind: ArgKind::V, typlen: 0 });
    }
    b.site(Call { funcid, args: Kids::NONE, nargs: nargs as u8, strict: false, collation: 0, how: How::Shim { shim, convs, byref }, rettype, volatile })
}

pub(crate) fn sbs_store_site(b: &mut Builder<'_, '_>, provided: usize) -> SiteId {
    site(b, 0, SBS_HEAD + provided, &SBS_STORE_SITE, true, 0, false)
}

pub(crate) fn field_store_site(b: &mut Builder<'_, '_>, nnew: usize) -> SiteId {
    site(b, 0, FS_HEAD + 2 * nnew.clamp(1, FIELD_STORE_BATCH), &FIELD_STORE_SITE, true, 0, false)
}

pub(crate) fn next_value_site(b: &mut Builder<'_, '_>, typ: u32) -> SiteId {
    site(b, F_NEXTVAL, 2, &NEXTVAL_SITE, false, typ, true)
}

impl Em<'_, '_, '_> {
    fn word_arg(&mut self, w: i64) -> Lowered {
        let value = self.b.iconst(Type::I64, w);
        Lowered { value, valid: self.t, err: self.zero }
    }

    fn val_arg(&self, v: Value) -> Lowered {
        Lowered { value: v, valid: self.t, err: self.zero }
    }

    fn fresh(&mut self, err: Value) -> Value {
        self.b.icmp(IntCond::Eq, err, self.zero)
    }

    /// `bit` where `v` is not valid.
    fn null_bit(&mut self, valid: Value, bit: i64) -> Value {
        let b = self.b.iconst(Type::I64, bit);
        self.b.select(valid, self.zero, b)
    }

    /// The site called over `args` only where `go` holds, else a null with no raise.
    pub(super) fn call_when(&mut self, go: Value, site: SiteId, args: &[Lowered]) -> Result<Lowered, EmitError> {
        if go == self.t {
            return self.call(site, args, false);
        }
        let l_do = self.b.create_block(&[]);
        let l_join = self.b.create_block(&[Type::I64, Type::Bool, Type::I64]);
        let (z, f) = (self.zero, self.f);
        self.b.cond_br(go, (l_do, &[]), (l_join, &[z, f, z]));
        self.b.switch_to(l_do);
        let r = self.call(site, args, false)?;
        self.b.br(l_join, &[r.value, r.valid, r.err]);
        self.b.switch_to(l_join);
        let ps = self.b.block_params(l_join);
        Ok(Lowered { value: ps[0], valid: ps[1], err: ps[2] })
    }

    /// The `SubscriptingRef` assignment.
    pub(super) fn store(&mut self, n: &Subscripting) -> Result<Lowered, EmitError> {
        let assign = n.assign.expect("a store");
        let cur = self.node(n.container)?;
        let mut err = cur.err;
        let mut all = self.t;
        let mut subs = mcx::reserved_in((n.upper.len + n.lower.len) as usize, self.row.scratch);
        for list in [n.upper, n.lower] {
            for i in 0..list.len as usize {
                let id = self.p.kids[list.start as usize + i];
                if id == NodeId::NONE {
                    continue;
                }
                let fresh = self.fresh(err);
                let s = self.guarded(fresh, id)?;
                err = self.or(err, s.err);
                all = self.and(all, s.valid);
                let a = self.val_arg(s.value);
                subs.push_in(a);
            }
        }
        let [w0, w1] = words(self.p, n);
        let param = if n.container_is_param { PARAM } else { 0 };
        let cur_null = self.null_bit(cur.valid, CUR_NULL);
        let fresh = self.fresh(err);
        let go = self.and(all, fresh);
        let head = [self.word_arg(w0), self.word_arg(w1), self.val_arg(cur_null), self.val_arg(cur.value), self.val_arg(self.zero)];
        let mut args = mcx::concat_in(&[&head, &subs], self.row.scratch);
        let saved = self.casetest;
        if n.fetch_old {
            // C `EEOP_SBSREF_OLD`: the element (slice) the nested indirection reads, null over a null container
            let old = self.call_when(go, n.site, &args)?;
            err = self.or(err, old.err);
            self.casetest = Some(old);
        }
        let fresh = self.fresh(err);
        let go = self.and(all, fresh);
        let rep = self.guarded(go, assign);
        self.casetest = saved;
        let rep = rep?;
        err = self.or(err, rep.err);
        let sub_null = self.null_bit(all, SUB_NULL);
        let rep_null = self.null_bit(rep.valid, REP_NULL);
        let ctl = self.b.iconst(Type::I64, ASSIGN | param);
        let ctl = self.or(ctl, cur_null);
        let ctl = self.or(ctl, sub_null);
        let ctl = self.or(ctl, rep_null);
        args[2] = self.val_arg(ctl);
        args[4] = self.val_arg(rep.value);
        let fresh = self.fresh(err);
        let r = self.call_when(fresh, n.site, &args)?;
        let err = self.or(err, r.err);
        Ok(Lowered { value: r.value, valid: r.valid, err })
    }

    /// `FieldStore`: the composite, the new values in order, formed in runs broken where a new value reads its field.
    pub(super) fn field_store(&mut self, n: &FieldStore) -> Result<Lowered, EmitError> {
        let Some(cells) = self.row.cells else { return Err(EmitError::NoCells) };
        let mut cur = self.node(n.arg)?;
        let mut err = cur.err;
        let cell = self.site_cell(cells, n.site.0 as usize);
        let desc = self.b.load(Type::I64, cell, CELL_AUX);
        let nargs = self.p.sites[n.site.0 as usize].nargs as usize;
        let mut pending: PgVec<'_, (i64, Lowered)> = mcx::reserved_in(FIELD_STORE_BATCH, self.row.scratch);
        let mut first = n.arg.0 as usize + 1;
        for i in 0..n.newvals.len as usize {
            let id = self.p.kids[n.newvals.start as usize + i];
            let fnum = self.p.oids[n.fieldnums as usize + i] as i64;
            let reads_old = self.p.nodes[first..=id.0 as usize].iter().any(|x| matches!(x, ExprNode::CaseTest));
            first = id.0 as usize + 1;
            let saved = self.casetest;
            if reads_old {
                if !pending.is_empty() {
                    cur = self.form(n.site, desc, nargs, cur, &mut err, &mut pending)?;
                }
                let cur_null = self.null_bit(cur.valid, FS_CUR_NULL);
                let ctl = self.b.iconst(Type::I64, fnum << 16);
                let ctl = self.or(ctl, cur_null);
                let mut args = mcx::vec_array_in(self.row.scratch, [self.val_arg(desc), self.val_arg(ctl), self.val_arg(cur.value)]);
                let z = self.val_arg(self.zero);
                args.resize_in(nargs, z);
                let fresh = self.fresh(err);
                let old = self.call_when(fresh, n.site, &args)?;
                err = self.or(err, old.err);
                self.casetest = Some(old);
            }
            let fresh = self.fresh(err);
            let v = self.guarded(fresh, id);
            self.casetest = saved;
            let v = v?;
            err = self.or(err, v.err);
            pending.push((fnum, v));
            if pending.len() == FIELD_STORE_BATCH {
                cur = self.form(n.site, desc, nargs, cur, &mut err, &mut pending)?;
            }
        }
        if !pending.is_empty() || n.newvals.len == 0 {
            cur = self.form(n.site, desc, nargs, cur, &mut err, &mut pending)?;
        }
        Ok(Lowered { value: cur.value, valid: cur.valid, err })
    }

    /// The pending fields stored into `cur` (a null composite is all nulls first): one call, the formed image.
    fn form(&mut self, site: SiteId, desc: Value, nargs: usize, cur: Lowered, err: &mut Value, pending: &mut PgVec<'_, (i64, Lowered)>) -> Result<Lowered, EmitError> {
        let cur_null = self.null_bit(cur.valid, FS_CUR_NULL);
        let ctl = self.b.iconst(Type::I64, FS_STORE | (pending.len() as i64) << 8);
        let mut ctl = self.or(ctl, cur_null);
        let mut args = mcx::reserved_in(nargs.max(3 + 2 * pending.len()), self.row.scratch);
        args.extend_from_slice_in(&[self.val_arg(desc), self.val_arg(ctl), self.val_arg(cur.value)]);
        for (k, (fnum, v)) in pending.drain(..).enumerate() {
            let bit = self.null_bit(v.valid, 1 << (16 + k));
            ctl = self.or(ctl, bit);
            let (w, x) = (self.word_arg(fnum), self.val_arg(v.value));
            args.extend_from_slice_in(&[w, x]);
        }
        args[1] = self.val_arg(ctl);
        let z = self.val_arg(self.zero);
        args.resize_in(nargs, z);
        let fresh = self.fresh(*err);
        let r = self.call_when(fresh, site, &args)?;
        *err = self.or(*err, r.err);
        Ok(r)
    }

    /// `NextValueExpr`: the volatile site, once per row.
    pub(super) fn next_value(&mut self, n: &NextValue) -> Result<Lowered, EmitError> {
        let args = [self.word_arg(n.seqid as i64), self.word_arg(n.typ as i64)];
        self.call(n.site, &args, false)
    }
}

gravity_abi::host! {
    #[unwind]
    unsafe fn no_addr() {}
}

pub static SBS_STORE_SITE: Shim = Shim { oid: 0, name: "subscript store", family: Family::None, nargs: SBS_HEAD as u8, coll: false, region: true, args: &[ArgKind::V; SBS_HEAD], tyargs: &[], addr: gravity_fn::Addr(no_addr), call: sbs_shim };
pub static FIELD_STORE_SITE: Shim = Shim { oid: 0, name: "field store", family: Family::None, nargs: FS_HEAD as u8, coll: false, region: true, args: &[ArgKind::V; FS_HEAD], tyargs: &[], addr: gravity_fn::Addr(no_addr), call: fs_shim };
pub static NEXTVAL_SITE: Shim = Shim { oid: 0, name: "nextval", family: Family::None, nargs: 2, coll: false, region: true, args: &[ArgKind::V, ArgKind::V], tyargs: &[], addr: gravity_fn::Addr(no_addr), call: nextval_shim };

fn ret(e: *mut ErrSlot, r: PgResult<NullableDatum>) -> Ret {
    match r {
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

fn sbs_shim(e: *mut ErrSlot, region: *mut Region, _coll: u32, _ty: &[u32], a: &[Arg]) -> Ret {
    // SAFETY: the host passes the cell's live region.
    ret(e, sbs_run(a, unsafe { &mut *region }))
}

fn sbs_run(a: &[Arg], region: &mut Region) -> PgResult<NullableDatum> {
    let (w0, w1, ctl) = (a[0].v(), a[1].v(), a[2].v());
    let handler = handler_of(w0, w1);
    if ctl & SUB_NULL != 0 {
        return Err(null_subscript_error(handler));
    }
    let (nupper, slice) = ((w1 & 7) as usize, w1 & SLICE != 0);
    let mut s = Subs::new();
    let mut next = SBS_HEAD;
    for (shift, lower) in [(4, false), (10, true)] {
        for i in 0..if lower && !slice { 0 } else { nupper } {
            let provided = w1 >> (shift + i) & 1 != 0;
            if lower {
                s.lower_provided[i] = provided;
            } else {
                s.upper_provided[i] = provided;
            }
            if !provided {
                continue;
            }
            let v = a[next].v();
            next += 1;
            s.index[i] = index_of(handler, i, v, region)?;
            if lower {
                s.lower[i] = v as i32;
            } else {
                s.upper[i] = v as i32;
            }
        }
    }
    let cur = if ctl & CUR_NULL != 0 { NullableDatum::null() } else { NullableDatum::value(Datum::from_i64(a[3].v())) };
    if ctl & ASSIGN == 0 {
        // the old value: null over a null container (C `array_subscript_fetch_old`, `jsonb_subscript_fetch_old`)
        return if cur.isnull { Ok(cur) } else { fetch(handler, nupper, slice, cur, &mut s, region) };
    }
    let replace = if ctl & REP_NULL != 0 { NullableDatum::null() } else { NullableDatum::value(Datum::from_i64(a[4].v())) };
    match handler {
        Handler::Array { attrlen, elem } => assign_array(nupper, slice, ctl & PARAM != 0, attrlen, elem, cur, replace, &mut s, region),
        Handler::Jsonb { int4_index } => assign_jsonb(int4_index, nupper.min(MAXDIM), cur, replace, &s, region),
        Handler::Hstore => assign_hstore(cur, replace, &s, region),
    }
}

fn fs_shim(e: *mut ErrSlot, region: *mut Region, _coll: u32, _ty: &[u32], a: &[Arg]) -> Ret {
    // SAFETY: the host passes the cell's live region.
    ret(e, fs_run(a, unsafe { &mut *region }))
}

fn fs_run(a: &[Arg], region: &mut Region) -> PgResult<NullableDatum> {
    // SAFETY: `a[0]` is the owner's descriptor pointer (`Cell::aux`, set for every FieldStore site, program-long).
    let desc = unsafe { &*(a[0].v() as *const types_tuple::TupleDescData<'static>) };
    let ctl = a[1].v();
    let natts = desc.natts as usize;
    let (vals, nls) = crate::nodes::tuple::row_cells(region, natts)?;
    if ctl & FS_CUR_NULL != 0 {
        vals.fill(Datum::null());
        nls.fill(true);
    } else {
        let img = crate::views::image_view(Datum::from_i64(a[2].v()), -1, region)?;
        crate::nodes::tuple::deform(img.ptr, desc, vals, nls);
    }
    if ctl & FS_STORE == 0 {
        // the field as stored so far, for a new value that reads it (C: CaseTest over `values[fieldnum - 1]`)
        let f = (ctl >> 16) as usize - 1;
        return Ok(NullableDatum { value: vals[f], isnull: nls[f] });
    }
    let k = (ctl >> 8 & 0xff) as usize;
    for j in 0..k {
        let f = a[FS_HEAD + 2 * j].v() as usize - 1;
        vals[f] = Datum::from_i64(a[FS_HEAD + 2 * j + 1].v());
        nls[f] = ctl >> (16 + j) & 1 != 0;
    }
    crate::nodes::tuple::form(desc, vals, nls, region)
}

fn nextval_shim(e: *mut ErrSlot, _region: *mut Region, _coll: u32, _ty: &[u32], a: &[Arg]) -> Ret {
    ret(e, next_value(a[0].v() as u32, a[1].v() as u32))
}

/// C `ExecEvalNextValueExpr`: `nextval_internal` without the permission check, narrowed to the column's type.
fn next_value(seqid: u32, typ: u32) -> PgResult<NullableDatum> {
    let v = sequence_seams::nextval_internal::call(seqid, false)?;
    Ok(NullableDatum::value(match typ {
        21 => Datum::from_i16(v as i16),
        23 => Datum::from_i32(v as i32),
        _ => Datum::from_i64(v),
    }))
}
