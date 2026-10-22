//! Road `mask.vec` (H01a, R1-L02 R2-L10; KEEP lowerv2-final step/filter.rs `word_mask`): on arm64 a qual with gravity_expr's
//! vector form tests a 64-row block as `VecCmp` chunks (16 bytes of lanes each) whose sign bits are gathered into the word,
//! then ANDed with the lanes' validity and the frame's mask as the scalar word is (block.rs), only where the chunk divides
//! the block and no chunk lane can raise (an overflowing `+` / `-`: a masked row skips its per-row test); any other qual
//! takes the scalar word, and so does the guard (block.rs `open`). In body.form's packed body a one-compare qual over a
//! ByteFor lane masks the raw deltas (body_form.rs), a lower and an upper bound in two adjacent spanned steps as one
//! range (`pair`), and under mask.fold a counting end takes their count form (`packed_count`). Budget 160 lines.
use gravity_expr::emit::word::WordRefs;
use gravity_expr::emit::{LaneRef, ParamNeed};
use gravity_expr::{Compiled, ExprNode, Source as VarSource};
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use mcx::{CollectIn, PgVec};
use types_core::Oid;

use crate::face::source::Packed;
use crate::face::step::{BlockImpl, BlockMech};
use crate::frame::env::Env;
use crate::frame::frame::{Mask, WordWin};
use crate::frame::px::ExId;

crate::strategies! {
    /// Arm64 VecCmp chunk form.
    MASK_VEC = 1, "mask.vec", ScanFilter;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(MASK_VEC, &BlockImpl(&VEC))], ..crate::ops::NONE };

struct Vector;
static VEC: Vector = Vector;

/// Whether `c` has a usable vector form over `lanes` (widths and kinds as the window's): a mask whose chunk divides 64
/// rows and whose chunks cannot raise, on arm64. Probed in a scratch function with stand-in lanes and its `n` constants.
pub(crate) fn has_form(c: &Compiled<'_>, n: usize, lanes: &[Option<LaneRef>], scratch: mcx::Mcx<'_>) -> bool {
    if !cfg!(target_arch = "aarch64") {
        return false;
    }
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("vec_form_probe", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let p = b.params()[0];
    let zero = b.iconst(Type::I64, 0);
    let stand: PgVec<'_, Option<LaneRef>> = lanes.iter().map(|l| l.map(|l| LaneRef { ptr: p, valid: p, valid_null: None, ..l })).collect_in(scratch);
    let cv = mcx::pg_vec![in scratch; zero; n];
    let cn: PgVec<'_, Value> = (0..n).map(|_| b.bconst(false)).collect_in(scratch);
    let wr = WordRefs { lanes: &stand[..], consts: &cv[..], consts_null: &cn[..], tables: &[], scratch };
    let ok = match c.emit_vec(&mut b, &wr) {
        Ok(mut e) => e.mask && e.rows > 0 && 64 % e.rows as u32 == 0 && e.chunk(&mut b, &wr, zero, 0).err.is_none(),
        Err(_) => false,
    };
    b.ret(&[zero]);
    ok
}

/// Whether mask.vec takes `qual` over window `w`: the road is on and the qual has a usable vector form there.
pub(crate) fn takes(env: &Env, w: &WordWin, qual: ExId) -> bool {
    env.applies(MASK_VEC) && has_form(env.ex(qual), env.placed(qual).places.len(), &w.lanes, env.mcx)
}

/// In the packed body, `qual`'s packed form: its one compare `lane cond constant` in the i64 order of the lane's packed
/// image (`packed_cond`; the unsigned delta order) over a lane the source serves as deltas (`Env::packed`), the constant
/// its only word, with the lane's attno. `None` masks it decoded (floats, numerics, text, IN lists, several compares, a
/// parameter).
pub(crate) fn packed_leaf(env: &Env, w: &WordWin, qual: ExId) -> Option<(i16, Packed, IntCond)> {
    let e = env.placed(qual);
    if !env.body.packed || e.places.len() != 1 || !matches!(e.needs.first(), Some(ParamNeed::Const(_))) {
        return None;
    }
    let p = &e.c.prog;
    let mut id = e.c.root;
    while let ExprNode::Relabel(r) = &p.nodes[id.0 as usize] {
        id = r.arg;
    }
    let (ExprNode::Op(n) | ExprNode::Func(n)) = &p.nodes[id.0 as usize] else { return None };
    let call = &p.sites[n.site.0 as usize];
    let k = &p.kids[call.args.start as usize..][..call.args.len as usize];
    let (v, swapped) = match (k.len(), k.first().map(|a| &p.nodes[a.0 as usize]), k.get(1).map(|a| &p.nodes[a.0 as usize])) {
        (2, Some(ExprNode::Var(v)), Some(ExprNode::Const(_))) => (v, false),
        (2, Some(ExprNode::Const(_)), Some(ExprNode::Var(v))) => (v, true),
        _ => return None,
    };
    let lane = w.lanes.get(usize::try_from(v.attno).ok()?.checked_sub(1)?).copied().flatten().filter(|_| v.src == VarSource::Scan)?;
    let cond = packed_cond(call.funcid, lane.width, swapped, v.typ)?;
    let pk = env.packed.iter().find(|x| x.0 == v.attno)?.1;
    (lane.kind == gravity_abi::ctx::lane::KIND_BYVAL).then_some((v.attno, pk, cond))
}

/// The packed form's compare (also pgrc_packed.rs `leaf_attno`'s): `funcid`'s vector compare of a `width`-byte lane of
/// type `typ` with a literal (`swapped`: the literal first), in the i64 order of the lane's packed image, where typesem
/// says that image orders as the type (`packed_order`); an unsigned compare (oid's) runs signed there, the image being
/// the zero-extended word. `None`: the qual masks decoded.
pub(crate) fn packed_cond(funcid: Oid, width: u8, swapped: bool, typ: Oid) -> Option<IntCond> {
    let signed = gravity_fn::typesem::pgrc::packed_order(typ)?;
    Some(match (gravity_expr::emit::prim::vector_cond(funcid, width, swapped)?, signed) {
        (c @ (IntCond::Eq | IntCond::Ne), _) | (c @ (IntCond::Slt | IntCond::Sle | IntCond::Sgt | IntCond::Sge), true) => c,
        (IntCond::Ult, false) => IntCond::Slt,
        (IntCond::Ule, false) => IntCond::Sle,
        (IntCond::Ugt, false) => IntCond::Sgt,
        (IntCond::Uge, false) => IntCond::Sge,
        _ => return None,
    })
}

fn leaf_word(b: &mut FunctionBuilder<'_>, env: &mut Env, q: ExId, (_, pk, cond): (i16, Packed, IntCond), count: bool) -> Value {
    let (cv, cn) = crate::frame::hoist::words(b, env, env.node, q);
    crate::frame::body_form::packed_word(b, &pk, cond, cv[0], cn[0], count)
}

/// `a` and `b` as a packed range `(lower, upper)`: a lower bound (`>=`, `>`) and an upper bound (`<=`, `<`) on one packed
/// lane, in either order (step/filter.rs fuses two adjacent spanned steps' quals by it).
pub(crate) fn pair(env: &Env, w: &WordWin, a: ExId, b: ExId) -> Option<(ExId, ExId)> {
    let (la, lb) = (packed_leaf(env, w, a)?, packed_leaf(env, w, b)?);
    let lower = |c: IntCond| matches!(c, IntCond::Sge | IntCond::Sgt);
    let upper = |c: IntCond| matches!(c, IntCond::Sle | IntCond::Slt);
    match (la.0 == lb.0, la.2, lb.2) {
        (true, x, y) if lower(x) && upper(y) => Some((a, b)),
        (true, x, y) if upper(x) && lower(y) => Some((b, a)),
        _ => None,
    }
}

/// A `pair`'s mask over the block at `base` within `m`: one unsigned compare per row (body_form.rs `packed_range`).
pub(crate) fn range_mask(b: &mut FunctionBuilder<'_>, env: &mut Env, w: &WordWin, base: Value, pair: (ExId, ExId), m: Mask) -> Mask {
    let word = range_word(b, env, w, pair, false);
    crate::frame::block::valid_within(b, env, w, base, pair.0, word, m)
}

fn range_word(b: &mut FunctionBuilder<'_>, env: &mut Env, w: &WordWin, (lo, hi): (ExId, ExId), count: bool) -> Value {
    let (Some((_, pk, lc)), Some((_, _, hc))) = (packed_leaf(env, w, lo), packed_leaf(env, w, hi)) else {
        crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a packed range whose bounds are not packed" })
    };
    let ((lv, ln), (hv, hn)) = (crate::frame::hoist::words(b, env, env.node, lo), crate::frame::hoist::words(b, env, env.node, hi));
    crate::frame::body_form::packed_range(b, &pk, (lc, lv[0], ln[0]), (hc, hv[0], hn[0]), count)
}

/// The count form over a whole block (step/filter.rs `block_count`, under mask.fold): one packed compare (`quals`), or a
/// `pair`'s range, over a NOT NULL lane, its passing rows counted without a movemask. Every row of the block is live
/// (frame.rs block_loop's all-ones block). `None` for any other quals.
pub(crate) fn packed_count(b: &mut FunctionBuilder<'_>, env: &mut Env, w: &WordWin, quals: &[ExId], range: Option<(ExId, ExId)>) -> Option<Value> {
    let notnull = |a: i16| w.valid.get(a as usize - 1).copied().flatten().is_none();
    if let Some(r) = range {
        return notnull(packed_leaf(env, w, r.0)?.0).then(|| range_word(b, env, w, r, true));
    }
    let &[q] = quals else { return None };
    let leaf = packed_leaf(env, w, q).filter(|l| notnull(l.0))?;
    Some(leaf_word(b, env, q, leaf, true))
}

impl BlockMech for Vector {
    fn mask(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, w: &WordWin, base: Value, qual: ExId, m: Mask) -> Mask {
        if let Some(leaf) = packed_leaf(env, w, qual) {
            let word = leaf_word(b, env, qual, leaf, false);
            return crate::frame::block::valid_within(b, env, w, base, qual, word, m);
        }
        if !takes(env, w, qual) {
            return crate::frame::block::word_mask(b, env, w, base, qual, m);
        }
        let (cv, cn) = crate::frame::hoist::words(b, env, env.node, qual);
        let refs = WordRefs { lanes: &w.lanes, consts: &cv, consts_null: &cn, tables: &[], scratch: env.mcx };
        let Ok(mut e) = env.ex(qual).emit_vec(b, &refs) else {
            crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a qual's vector form that the probe accepted" })
        };
        let per = e.rows as u32;
        let mut word = env.zero;
        for k in 0..64 / per {
            let c = e.chunk(b, &refs, base, k);
            let mut bits = b.vmovmask(c.value);
            if k > 0 {
                let sh = b.iconst(Type::I64, (k * per) as i64);
                bits = b.int_bin(IntBinOp::Shl, bits, sh);
            }
            word = b.int_bin(IntBinOp::Or, word, bits);
        }
        crate::frame::block::valid_within(b, env, w, base, qual, word, m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_cond_runs_a_compare_in_the_image_order_its_type_has() {
        assert_eq!(packed_cond(66, 4, false, 23), Some(IntCond::Slt), "int4lt over int4");
        assert_eq!(packed_cond(716, 4, false, 26), Some(IntCond::Slt), "oidlt over oid: its zero-extended image");
        assert_eq!(packed_cond(716, 4, true, 26), Some(IntCond::Sgt), "a literal first");
        assert_eq!(packed_cond(1639, 4, false, 26), Some(IntCond::Sge), "oidge");
        assert_eq!(packed_cond(184, 4, false, 26), Some(IntCond::Eq), "oideq");
        assert_eq!(packed_cond(716, 4, false, 23), None, "an unsigned compare over a signed image");
        assert_eq!(packed_cond(66, 4, false, 700), None, "no packed image order");
        assert_eq!(packed_cond(716, 8, false, 26), None, "no vector form");
    }
}
