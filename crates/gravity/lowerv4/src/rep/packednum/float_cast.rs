//! D3139: road `num.short`'s float cast row: numeric_float4 / numeric_float8 (1745 / 1746) of a pgrc packed numeric lane
//! (`Rep::PackedNum` window at its typmod's scale, or `Rep::PackedNumDyn` at the window's elected scale) read as the
//! window's (mantissa, scale) and cast by gravity_fn agg_final `mantissa_float4` / `mantissa_float8` (adt_numeric's
//! fixed_scale_to_f32 / f64, D3106: the cast of the image those make, bit for bit), with no image. Claimed in text.like's
//! form (step/text_like.rs `claim_calls`): `CASE WHEN Var(hot) THEN Var(value) ELSE <the cast> END`, so whatever this
//! row declines (a verbatim window's image, a mantissa past the exact path, a cell of any other form) runs the cast itself
//! over the rendered image, cold. Where the lane is `PackedNumDyn` (decompose shared/scan.rs `packed_minmax_only`) the
//! host decodes no image at all. Budget 120. Owner gdict.
use gravity_expr::{Compiled, ExprNode, Source as VarSource};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntCond, Type, Value};
use gravity_program::{LaneId, NumForm, Rep};
use types_core::catalog::{FLOAT4OID, FLOAT8OID, NUMERICOID};
use types_nodes::Node;

use crate::face::step::{Claim, ExprExtImpl, ExprExtMech, SubEmit, EXT_AT};
use crate::face::strategy::NUM_SHORT;
use crate::frame::{env::Env, px::{ExId, Px}, row::RowVals};
use gravity_decompose::catalog::numeric::{float_cast, F_FLOAT4_NUMERIC, F_FLOAT8_NUMERIC};

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(NUM_SHORT, &ExprExtImpl(&FLOAT))], ..crate::ops::NONE };
struct Float;
static FLOAT: Float = Float;

/// A claimed cast of column `attno` to float4 (`f4`) or float8.
#[derive(Debug, PartialEq)]
struct Site { attno: i16, f4: bool }

/// A cell's packed form: a `PackedNum` window or a `PackedNumDyn` lane.
fn packed(r: &Rep) -> bool {
    matches!(r, Rep::PackedNum { form: NumForm::Window, .. } | Rep::PackedNumDyn)
}

/// Program node `i` as a claimable cast of a numeric column (not a join's inner side) and the value's type.
fn site_of(c: &Compiled<'_>, i: usize) -> Option<(Site, u32)> {
    let ExprNode::Func(f) = &c.prog.nodes[i] else { return None };
    let call = &c.prog.sites[f.site.0 as usize];
    let f4 = float_cast(call.funcid).filter(|_| call.args.len == 1)?;
    let mut col = c.prog.kids[call.args.start as usize];
    while let ExprNode::Relabel(r) = &c.prog.nodes[col.0 as usize] {
        col = r.arg;
    }
    let ExprNode::Var(v) = &c.prog.nodes[col.0 as usize] else { return None };
    (v.src != VarSource::Inner && (1..EXT_AT).contains(&v.attno) && v.typ == NUMERICOID).then_some((Site { attno: v.attno, f4 }, if f4 { FLOAT4OID } else { FLOAT8OID }))
}

impl ExprExtMech for Float {
    fn ops(&self) -> &'static [u32] { &[F_FLOAT4_NUMERIC, F_FLOAT8_NUMERIC] }
    /// Only over a pipeline whose site carries a packed numeric lane (any other column's cast is the shim's, unclaimed).
    fn attach<'p>(&self, px: &mut Px<'_, 'p>, (k, _): (usize, usize), _node: Node<'p>, c: &mut Compiled<'p>, _ex: ExId, _sub: &mut dyn FnMut(Node<'p>) -> u32) -> Option<Claim> {
        let lanes = k != usize::MAX && px.site_mut(k).is_some_and(|s| s.lanes().iter().any(|l| matches!(l.id, LaneId::Attr(_)) && packed(&l.rep)));
        if !lanes {
            return None;
        }
        crate::step::text_like::claim_calls(c, site_of)
    }

    fn emit(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, ex: ExId, site: u32, _sub: SubEmit<'_>) -> [Value; 3] {
        crate::step::text_like::site(b, env, ex, site, |b, env, s: &Site| {
            let (z, f, t) = (env.zero, b.bconst(false), b.bconst(true));
            match row.col(s.attno).filter(|cr| packed(&cr.cell.rep)) {
                Some(cr) => cast(b, env, cr, s),
                None => [f, z, t],
            }
        })
    }
}

/// Site `s` over packed cell `cr`: `[hot, value, valid]`; a NULL cell is NULL, a verbatim window or a mantissa the exact
/// path declines is not hot (the cast itself decides it).
fn cast(b: &mut FunctionBuilder<'_>, env: &mut Env, cr: crate::rep::CellRef<'_>, s: &Site) -> [Value; 3] {
    fires::fire("num.float.packed"); // shapecheck fire signal (D3139: a float cast of a packed lane's mantissa, no image)
    let (z, f, t, node) = (env.zero, b.bconst(false), b.bconst(true), env.node);
    let null = cr.null.unwrap_or(f);
    let (l_out, l_read) = (b.create_block(&[Type::Bool, Type::I64, Type::Bool]), b.create_block(&[]));
    b.cond_br(null, (l_out, &[t, z, f]), (l_read, &[]));
    b.switch_to(l_read);
    let typmod = b.iconst(Type::I64, crate::rep::num_of(cr.cell.rep).scale as i64);
    // the reads under the NULL test do not dominate a later site's (cmp_num.rs `packed_compare`)
    let aside = env.memo.set_aside();
    let mut cx = env.lx(b, node);
    let read = match crate::rep::packednum::datum::window_word(&mut cx, cr) {
        Some((verbatim, w)) => Some((verbatim, w, typmod)),
        None => crate::rep::packednum::datum::dyn_word(&mut cx, cr),
    };
    env.memo = cx.memo;
    env.memo.restore(aside);
    let Some((verbatim, w, scale)) = read else { crate::refuse!(node, crate::admit::refusal::Reason::Bug { payload: "a num.float cell with no packed word" }) };
    let l_hot = b.create_block(&[]);
    b.cond_br(verbatim, (l_out, &[f, z, t]), (l_hot, &[]));
    b.switch_to(l_hot);
    let flags = HostFlags { pure: true, ..HostFlags::default() };
    let h = match s.f4 {
        true => b.declare_host_once("rep_num_float4", &[Type::I64, Type::I64], Some(Type::I64), num_float4 as *const () as usize, flags),
        false => b.declare_host_once("rep_num_float8", &[Type::I64, Type::I64], Some(Type::I64), num_float8 as *const () as usize, flags),
    };
    let v = b.call_host(h, &[w, scale], Some(Type::I64)).expect("returns the float's word");
    let none = b.iconst(Type::I64, DECLINED);
    let hot = b.icmp(IntCond::Ne, v, none);
    b.br(l_out, &[hot, v, t]);
    b.switch_to(l_out);
    [0, 1, 2].map(|i| b.block_params(l_out)[i])
}

/// The word the hosts answer where the exact path declines: no float8's bits (a NaN pattern the exact path never makes)
/// and no float4's (wider than 32 bits).
const DECLINED: i64 = -1;

gravity_kernels::contract::host! {
    /// numeric_float8 of mantissa `m` at `scale`: its float8 word, or `DECLINED` outside the exact path.
    pub(crate) fn num_float8(m: i64, scale: i64) -> i64 {
        gravity_fn::agg_final::mantissa_float8(m, scale).map_or(DECLINED, |x| x.to_bits() as i64)
    }

    /// numeric_float4 of mantissa `m` at `scale`: its float4 word (zero-extended, as the shim's), or `DECLINED`.
    pub(crate) fn num_float4(m: i64, scale: i64) -> i64 {
        gravity_fn::agg_final::mantissa_float4(m, scale).map_or(DECLINED, |x| x.to_bits() as u32 as i64)
    }
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;

    /// Each word is the shim's cast of the image the mantissa renders (`num_image`, the cold arm's datum), bit for bit,
    /// or declined; specials are declined.
    #[test]
    fn words_are_the_casts_of_the_images() {
        let mut out = [0u64; 6];
        let cases = [(0i64, 0i64), (12345, 2), (-7, 3), (1, 0), (-1, 18), (9_007_199_254_740_993, 0), (123_456_789_012_345_678, 9), (999_999_999_999_999_999, 4), (16_777_217, 0), (3, 1)];
        for (m, s) in cases {
            // SAFETY: `out` is 48 writable bytes, at least NUM_IMAGE_BYTES.
            let p = unsafe { super::super::raw::datum::num_image(m, s, out.as_mut_ptr() as u64) };
            assert_ne!(p, 0);
            // the image `num_image` wrote into `out` (a 4-byte varlena header, then the payload)
            let bytes: Vec<u8> = out.iter().flat_map(|w| w.to_ne_bytes()).collect(); // alloc: Program
            let at = p as usize - out.as_ptr() as usize;
            let len = (u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) >> 2) as usize;
            let num = adt_numeric::Num::from_payload(&bytes[at + 4..at + len]);
            let (f8, f4) = (adt_numeric::numeric_float8(num).unwrap(), adt_numeric::numeric_float4(num).unwrap());
            for (got, want, what) in [(num_float8(m, s), f8.to_bits() as i64, "float8"), (num_float4(m, s), f4.to_bits() as u32 as i64, "float4")] {
                assert!(got == DECLINED || got == want, "{what} of {m} at {s}: {got:#x} vs {want:#x}");
            }
        }
        assert_eq!(num_float8(12345, 2), 123.45f64.to_bits() as i64);
        // a negative typmod scale's mantissa counts its units (numeric(7, -2): 5 is 500)
        assert_eq!((num_float8(5, -2), num_float4(5, -2)), (500f64.to_bits() as i64, 500f32.to_bits() as i64));
        for m in [gravity_fn::agg_final::MANT_NAN, gravity_fn::agg_final::MANT_PINF, gravity_fn::agg_final::MANT_NINF] {
            assert_eq!((num_float8(m, 2), num_float4(m, 2)), (DECLINED, DECLINED));
        }
    }
}
