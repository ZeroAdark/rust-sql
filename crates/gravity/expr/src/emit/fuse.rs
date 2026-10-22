//! The fused datetime compares (lowerv2 M2 L5-SHIM (b) / (c)): `extract(unit FROM ts | date) ⋈ K` over an integral
//! numeric constant as the integer field (prim/datetime.rs) compared in the word — an infinite source's monotonic field
//! is ±Infinity, its oscillating field NULL — and `(a − b) ⋈ interval K` as the microsecond difference compared with the
//! constant's span (`interval_cmp_value`), an infinite end as the word's end; a raising row (both ends the same
//! infinity, the difference overflowing) takes the subtraction site's cell road, which names PostgreSQL's error. The
//! fused cast: `extract(unit FROM ts | date)::int2 | int4 | int8` as the integer field itself, a row the cast raises on
//! taking the two sites' cell road.

use gravity_ir::{ConvOp, IntBinOp, IntCond, IntOvfOp, Type, Value};

use super::prim::datetime::{self, DtForm};
use super::scalar::Em;
use super::{EmitError, Lowered};
use crate::nodes::{ExprNode, NodeId, SiteId};

const NUMERIC_CMP: [(u32, IntCond); 6] = [(1718, IntCond::Eq), (1719, IntCond::Ne), (1720, IntCond::Sgt), (1721, IntCond::Sge), (1722, IntCond::Slt), (1723, IntCond::Sle)];
const INTERVAL_CMP: [(u32, IntCond); 6] = [(1162, IntCond::Eq), (1163, IntCond::Ne), (1164, IntCond::Slt), (1165, IntCond::Sle), (1166, IntCond::Sge), (1167, IntCond::Sgt)];
const NUMERICOID: u32 = 1700;
const INTERVALOID: u32 = 1186;
const USECS_PER_DAY: i128 = 86_400_000_000;

fn flip(c: IntCond) -> IntCond {
    match c {
        IntCond::Slt => IntCond::Sgt,
        IntCond::Sle => IntCond::Sge,
        IntCond::Sgt => IntCond::Slt,
        IntCond::Sge => IntCond::Sle,
        c => c,
    }
}

/// `±Infinity ⋈ K` for a finite `K`.
fn inf_answer(c: IntCond, neg: bool) -> bool {
    match c {
        IntCond::Eq => false,
        IntCond::Ne => true,
        IntCond::Slt | IntCond::Sle => neg,
        _ => !neg,
    }
}

fn cmp_word(em: &mut Em<'_, '_, '_>, c: IntCond, x: Value, k: i64) -> Value {
    let kv = em.b.iconst(Type::I64, k);
    let b = em.b.icmp(c, x, kv);
    em.b.conv(ConvOp::Zext, Type::I64, b)
}

/// The fused form of a compare site, or `None` for the site's own road.
pub(super) fn fused(em: &mut Em<'_, '_, '_>, site: SiteId) -> Result<Option<Lowered>, EmitError> {
    let call = &em.p.sites[site.0 as usize];
    if call.nargs == 1 {
        let Some(w) = int_cast(call.funcid) else { return Ok(None) };
        let Some((form, _, ext)) = extract_form(em, em.p.kids[call.args.start as usize]) else { return Ok(None) };
        return extract_int(em, site, ext, form, w).map(Some);
    }
    if call.nargs != 2 {
        return Ok(None);
    }
    let (k0, k1) = (em.p.kids[call.args.start as usize], em.p.kids[call.args.start as usize + 1]);
    if let Some(&(_, c)) = NUMERIC_CMP.iter().find(|(o, _)| *o == call.funcid) {
        for (x, k, c) in [(k0, k1, c), (k1, k0, flip(c))] {
            if let (Some((form, ts, _)), Some(k)) = (extract_form(em, x), integral_numeric(em, k)) {
                return extract_cmp(em, form, ts, c, k).map(Some);
            }
        }
    }
    if let Some(&(_, c)) = INTERVAL_CMP.iter().find(|(o, _)| *o == call.funcid) {
        for (x, k, c) in [(k0, k1, c), (k1, k0, flip(c))] {
            if let (Some((mi, a, b)), Some(k)) = (tsmi_form(em, x), interval_span(em, k)) {
                return tsmi_cmp(em, mi, a, b, c, k).map(Some);
            }
        }
    }
    Ok(None)
}

/// `extract_timestamp(unit, ts)` / `extract_date(unit, d)` with a unit the integer field form serves: the form, the
/// source node and the extract's site.
fn extract_form(em: &Em<'_, '_, '_>, id: NodeId) -> Option<(DtForm, NodeId, SiteId)> {
    let (ExprNode::Func(n) | ExprNode::Op(n)) = &em.p.nodes[id.0 as usize] else { return None };
    let call = &em.p.sites[n.site.0 as usize];
    let name = gravity_fn::row_of(call.funcid)?.name;
    if !matches!(name, "extract_timestamp" | "extract_date") || call.nargs != 2 {
        return None;
    }
    let form = datetime::form_of(em.p, call, name, true)?;
    Some((form, em.p.kids[call.args.start as usize + 1], n.site))
}

/// The result width of PostgreSQL's numeric to integer casts.
fn int_cast(funcid: u32) -> Option<u8> {
    Some(match gravity_fn::row_of(funcid)?.name {
        "numeric_int2" => 2,
        "numeric_int4" => 4,
        "numeric_int8" => 8,
        _ => return None,
    })
}

/// A non-null numeric constant whose value is an integer strictly inside the 64-bit range.
fn integral_numeric(em: &Em<'_, '_, '_>, id: NodeId) -> Option<i64> {
    let ExprNode::Const(c) = &em.p.nodes[id.0 as usize] else { return None };
    if c.typ != NUMERICOID || c.is_null() {
        return None;
    }
    crate::percode::with_const_bytes(c, &mut |bytes| {
        let num = adt_numeric::Num::from_payload(bytes);
        if num.is_special() {
            return None;
        }
        let v = adt_numeric::numeric_int8(num).ok()?;
        let back = adt_numeric::int64_to_numeric(v);
        (v != i64::MIN && v != i64::MAX && adt_numeric::numeric_eq(num, back.num())).then_some(v)
    })
}

/// A non-null interval constant whose `interval_cmp_value` span is strictly inside the 64-bit range.
fn interval_span(em: &Em<'_, '_, '_>, id: NodeId) -> Option<i64> {
    let ExprNode::Const(c) = &em.p.nodes[id.0 as usize] else { return None };
    if c.typ != INTERVALOID || c.is_null() {
        return None;
    }
    crate::percode::with_const_bytes(c, &mut |bytes| {
        if bytes.len() < 16 {
            return None;
        }
        let time = i64::from_ne_bytes(bytes[0..8].try_into().unwrap()) as i128;
        let day = i32::from_ne_bytes(bytes[8..12].try_into().unwrap()) as i128;
        let month = i32::from_ne_bytes(bytes[12..16].try_into().unwrap()) as i128;
        let span = time + (month * 30 + day) * USECS_PER_DAY;
        (span > i64::MIN as i128 && span < i64::MAX as i128).then_some(span as i64)
    })
}

/// `timestamp_mi(a, b)`: its site and the two timestamp nodes.
fn tsmi_form(em: &Em<'_, '_, '_>, id: NodeId) -> Option<(SiteId, NodeId, NodeId)> {
    let (ExprNode::Func(n) | ExprNode::Op(n)) = &em.p.nodes[id.0 as usize] else { return None };
    let call = &em.p.sites[n.site.0 as usize];
    if !gravity_fn::typesem::expr::timestamp_minus(call.funcid) || call.nargs != 2 {
        return None;
    }
    Some((n.site, em.p.kids[call.args.start as usize], em.p.kids[call.args.start as usize + 1]))
}

fn extract_cmp(em: &mut Em<'_, '_, '_>, form: DtForm, ts: NodeId, c: IntCond, k: i64) -> Result<Lowered, EmitError> {
    fires::fire("expr.extract.cmp"); // the fused compare of an integral extract (a cached program does not re-emit)
    let ts = em.node(ts)?;
    let (out, finite) = datetime::emit(em.b, form, ts.value);
    let value = cmp_word(em, c, out.value, k);
    if let Some(finite) = finite {
        let valid = em.and(ts.valid, finite);
        return Ok(Lowered { value, valid, err: ts.err });
    }
    // D3149: a monotonic unit's word spells an infinite row's ±Infinity itself (no validity); the compare decides it here
    let (finite, nobegin) = datetime::source_inf(em.b, form, ts.value);
    let neg = em.b.iconst(Type::I64, inf_answer(c, true) as i64);
    let pos = em.b.iconst(Type::I64, inf_answer(c, false) as i64);
    let inf = em.b.select(nobegin, neg, pos);
    let value = em.b.select(finite, value, inf);
    Ok(Lowered { value, valid: ts.valid, err: ts.err })
}

/// `numeric_int<w>(extract(unit FROM x))` (`cast` over `ext`): the field word, which every finite source's field gives
/// exactly (an integral numeric: the cast neither rounds nor, at int4 / int8, leaves its range). An infinite source's
/// oscillating field is NULL, the cast's too. A row the cast raises on (a monotonic field of an infinite source,
/// PostgreSQL's "cannot convert infinity to ..", or an int2 out of range) takes the two sites' cell road, which raises
/// PostgreSQL's own error.
fn extract_int(em: &mut Em<'_, '_, '_>, cast: SiteId, ext: SiteId, form: DtForm, w: u8) -> Result<Lowered, EmitError> {
    let k = em.p.sites[ext.0 as usize].args.start as usize;
    let (k0, k1) = (em.p.kids[k], em.p.kids[k + 1]);
    fires::fire("expr.extract.int"); // the fused cast: an integral extract under an integer cast as its word (a cached program does not re-emit)
    let x = em.node(k1)?;
    let (out, nonnull) = datetime::emit(em.b, form, x.value);
    let (valid, inf) = match nonnull {
        Some(n) => (em.and(x.valid, n), None),
        None => {
            let (finite, _) = datetime::source_inf(em.b, form, x.value);
            (x.valid, Some(em.not(finite)))
        }
    };
    let past = (w == 2).then(|| {
        let t = em.b.conv(ConvOp::Trunc, Type::I16, out.value);
        let back = em.b.conv(ConvOp::Sext, Type::I64, t);
        em.b.icmp(IntCond::Ne, back, out.value)
    });
    let bad = match (inf, past) {
        (None, None) => return Ok(Lowered { value: out.value, valid, err: x.err }),
        (Some(a), None) | (None, Some(a)) => a,
        (Some(a), Some(b)) => em.or(a, b),
    };
    let bad = em.and(valid, bad);
    let l_cold = em.b.create_block(&[]);
    let l_fast = em.b.create_block(&[]);
    let l_join = em.b.create_block(&[Type::I64, Type::Bool, Type::I64]);
    em.b.cond_br(bad, (l_cold, &[]), (l_fast, &[]));
    em.b.switch_to(l_cold);
    let unit = em.node(k0)?;
    let e = em.call(ext, &[unit, x], false)?;
    let r = em.call(cast, &[e], false)?;
    em.b.br(l_join, &[r.value, r.valid, r.err]);
    em.b.switch_to(l_fast);
    em.b.br(l_join, &[out.value, valid, x.err]);
    em.b.switch_to(l_join);
    let ps = em.b.block_params(l_join);
    Ok(Lowered { value: ps[0], valid: ps[1], err: ps[2] })
}

fn tsmi_cmp(em: &mut Em<'_, '_, '_>, mi: SiteId, a: NodeId, b: NodeId, c: IntCond, k: i64) -> Result<Lowered, EmitError> {
    let a = em.node(a)?;
    let b = em.after(a.err, b)?; // D775: PG's order
    let valid = em.and(a.valid, b.valid);
    let err = em.first(a.err, b.err);
    let lo = em.b.iconst(Type::I64, i64::MIN);
    let hi = em.b.iconst(Type::I64, i64::MAX);
    let a_nb = em.b.icmp(IntCond::Eq, a.value, lo);
    let a_ne = em.b.icmp(IntCond::Eq, a.value, hi);
    let b_nb = em.b.icmp(IntCond::Eq, b.value, lo);
    let b_ne = em.b.icmp(IntCond::Eq, b.value, hi);
    let same_nb = em.and(a_nb, b_nb);
    let same_ne = em.and(a_ne, b_ne);
    let same = em.or(same_nb, same_ne);
    let a_inf = em.or(a_nb, a_ne);
    let b_inf = em.or(b_nb, b_ne);
    let any_inf = em.or(a_inf, b_inf);
    let finite = em.not(any_inf);
    let ovf = em.b.int_ovf(IntOvfOp::SsubOvf, a.value, b.value);
    let ovf = em.and(finite, ovf);
    let bad = em.or(same, ovf);
    let bad = em.and(valid, bad);
    let l_cold = em.b.create_block(&[]);
    let l_fast = em.b.create_block(&[]);
    let l_join = em.b.create_block(&[Type::I64, Type::Bool, Type::I64]);
    em.b.cond_br(bad, (l_cold, &[]), (l_fast, &[]));
    em.b.switch_to(l_cold);
    let r = em.call(mi, &[a, b], false)?;
    let e = em.or(err, r.err);
    let (z, f) = (em.zero, em.f);
    em.b.br(l_join, &[z, f, e]);
    em.b.switch_to(l_fast);
    let d = em.b.int_bin(IntBinOp::Sub, a.value, b.value);
    let d = em.b.select(b_ne, lo, d);
    let d = em.b.select(b_nb, hi, d);
    let d = em.b.select(a_ne, hi, d);
    let d = em.b.select(a_nb, lo, d);
    let value = cmp_word(em, c, d, k);
    em.b.br(l_join, &[value, valid, err]);
    em.b.switch_to(l_join);
    let ps = em.b.block_params(l_join);
    Ok(Lowered { value: ps[0], valid: ps[1], err: ps[2] })
}
