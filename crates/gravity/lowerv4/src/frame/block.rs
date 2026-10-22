//! Road `mask.word` (H01, R1 A4; KEEP v3-r1-a4 f416a2c2682 scan/filter.rs): a pipeline's first filter takes whole 64-row
//! blocks when its qual has gravity_expr's word form over by-value lanes. The block's mask is the word, ANDed with the
//! validity words of the lanes the qual reads (the two words the block straddles, shifted to its base; none in a body
//! whose lanes have no nulls) and with the frame's mask. A window whose constants do not fit the lanes, or that carries
//! a selection, takes rows one at a time, never a refusal. The filter step (step/filter.rs) asks `has_form` when it is
//! made and `open` per window (its `block_open`); the frame walks the mask's rows by ctz (frame.rs, mask_run.rs).
//! Budget 160 lines.
use gravity_expr::emit::word::WordRefs;
use gravity_expr::emit::LaneRef;
use gravity_expr::{ExprNode, Source as VarSource};
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use mcx::{CollectIn, GrowIn, Mcx, PgVec};

use crate::face::step::{BlockImpl, BlockMech};
use crate::frame::env::Env;
use crate::frame::frame::{Mask, WordWin};
use crate::frame::px::{ExId, Px};
use crate::admit::refusal::Reason;

crate::strategies! {
    /// 64-row scalar word mask, rows by ctz.
    MASK_WORD = 0, "mask.word", ScanFilter;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(MASK_WORD, &BlockImpl(&WORD))], ..crate::ops::NONE };

struct Word;
static WORD: Word = Word;

/// The scan columns `qual` reads, each once, in first-read order.
pub(crate) fn attnos<'m>(nodes: &[ExprNode], mcx: Mcx<'m>) -> PgVec<'m, i16> {
    let mut out: PgVec<'m, i16> = PgVec::new_in(mcx);
    for n in nodes {
        if let ExprNode::Var(v) = n {
            if v.src == VarSource::Scan && v.attno > 0 && !out.contains(&v.attno) {
                out.push_in(v.attno);
            }
        }
    }
    out
}

/// Whether `qual` has a block form over the source's by-value lanes `(attno, width)` (widths 2 / 4 / 8): the word form, or
/// mask.vec's vector form, probed in a scratch function when the filter is made. A filter without one has no block form.
pub(crate) fn has_form(px: &Px<'_, '_>, qual: ExId, lanes: &[(i16, u8)]) -> bool {
    let natts = lanes.iter().map(|l| l.0 as usize).max().unwrap_or(0);
    // M8 B1: a qual reading an InitPlan's result has no block form (its Param is loaded where the row reads it)
    if natts == 0 || px.placed(qual).places.iter().any(|p| matches!(p, gravity_lower::Place::Exec(x) if crate::rerun::params::init_of(*x).is_some())) {
        return false;
    }
    let mcx = px.mcx();
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("word_form_probe", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let p = b.params()[0];
    let zero = b.iconst(Type::I64, 0);
    let mut by_attno: PgVec<'_, Option<LaneRef>> = mcx::pg_vec![in mcx; None; natts];
    for &(a, width) in lanes.iter().filter(|l| l.0 > 0 && matches!(l.1, 2 | 4 | 8)) {
        by_attno[a as usize - 1] = Some(LaneRef { ptr: p, valid: p, valid_null: None, width, kind: gravity_abi::ctx::lane::KIND_BYVAL, all_valid: true });
    }
    let e = px.placed(qual);
    let cv = mcx::pg_vec![in mcx; zero; e.places.len()];
    let cn: PgVec<'_, Value> = (0..e.places.len()).map(|_| b.bconst(false)).collect_in(mcx);
    let wr = WordRefs { lanes: &by_attno[..], consts: &cv[..], consts_null: &cn[..], tables: &[], scratch: mcx };
    let ok = e.c.emit_word_ok(&mut b, &wr).is_ok() && e.c.emit_word(&mut b, &wr, zero).is_ok();
    b.ret(&[zero]);
    ok || crate::frame::mask_vec::has_form(&e.c, e.places.len(), &by_attno[..], mcx)
}

/// The block path's guard for `qual` over window `w` (the filter's `block_open`): every lane the qual reads is in `w`,
/// its constants fit the lanes (the vector form's guard where mask.vec takes the qual, else `emit_word_ok`), and the
/// window's rows are its lanes' rows (`plain`). `None` when a lane is missing: this window form has no block path.
pub(crate) fn open(b: &mut FunctionBuilder<'_>, env: &mut Env, w: &WordWin, qual: ExId) -> Option<Mask> {
    if attnos(&env.ex(qual).prog.nodes, env.mcx).iter().any(|&a| w.lanes.get(a as usize - 1).copied().flatten().is_none()) {
        return None;
    }
    let (cv, cn) = crate::frame::hoist::words(b, env, env.node, qual);
    let refs = WordRefs { lanes: &w.lanes, consts: &cv, consts_null: &cn, tables: &[], scratch: env.mcx };
    let ok = match crate::frame::mask_vec::takes(env, w, qual) {
        true => env.ex(qual).emit_vec(b, &refs).ok()?.ok(b, &refs),
        false => env.ex(qual).emit_word_ok(b, &refs).ok()?,
    };
    let fits = b.icmp(IntCond::Ne, ok, env.zero);
    let plain = if b.value_type(w.plain) == Type::Bool { w.plain } else { b.icmp(IntCond::Ne, w.plain, env.zero) };
    Some(Mask(b.int_bin(IntBinOp::And, fits, plain)))
}

/// The qual's scalar word over block `[base, base + 64)`, ANDed with its lanes' validity and `m` (the mask.word form,
/// and mask.vec's for a qual without a vector form).
pub(crate) fn word_mask(b: &mut FunctionBuilder<'_>, env: &mut Env, w: &WordWin, base: Value, qual: ExId, m: Mask) -> Mask {
    let (cv, cn) = crate::frame::hoist::words(b, env, env.node, qual);
    let refs = WordRefs { lanes: &w.lanes, consts: &cv, consts_null: &cn, tables: &[], scratch: env.mcx };
    let Ok(mask) = env.ex(qual).emit_word(b, &refs, base) else {
        crate::refuse!(env.node, Reason::Bug { payload: "a qual's word form that the probe accepted" })
    };
    valid_within(b, env, w, base, qual, mask, m)
}

/// `mask` ANDed with the validity words of the lanes `qual` reads (none when the body's lanes have no nulls) and `m`.
pub(crate) fn valid_within(b: &mut FunctionBuilder<'_>, env: &mut Env, w: &WordWin, base: Value, qual: ExId, mask: Value, m: Mask) -> Mask {
    let attnos = attnos(&env.ex(qual).prog.nodes, env.mcx);
    let mask = valid_and(b, env, w, &attnos, base, mask);
    Mask(b.int_bin(IntBinOp::And, mask, m.0))
}

/// `mask` ANDed with the 64 validity bits from row `base` of each lane `attnos` names (a lane with no validity words is
/// all valid); nothing when the body's lanes have no nulls (`Env::body.nonulls`).
pub(crate) fn valid_and(b: &mut FunctionBuilder<'_>, env: &Env, w: &WordWin, attnos: &[i16], base: Value, mut mask: Value) -> Value {
    if env.body.nonulls {
        return mask;
    }
    for &a in attnos {
        if let Some((valid, vnull)) = w.valid.get(a as usize - 1).copied().flatten() {
            let v = validity(b, env, valid, vnull, base);
            mask = b.int_bin(IntBinOp::And, mask, v);
        }
    }
    mask
}

impl BlockMech for Word {
    fn mask(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, w: &WordWin, base: Value, qual: ExId, m: Mask) -> Mask {
        word_mask(b, env, w, base, qual, m)
    }
}

/// The 64 validity bits of `valid` from row `base`: the two words it straddles, shifted together (`vnull`: no validity
/// words, all valid).
fn validity(b: &mut FunctionBuilder<'_>, env: &Env, valid: Value, vnull: Value, base: Value) -> Value {
    let (six, sixty_three, sixty_four) = (b.iconst(Type::I64, 6), b.iconst(Type::I64, 63), b.iconst(Type::I64, 64));
    let lo_at = b.int_bin(IntBinOp::Ushr, base, six);
    let last = b.int_bin(IntBinOp::Add, base, sixty_three);
    let hi_at = b.int_bin(IntBinOp::Ushr, last, six);
    let s = b.int_bin(IntBinOp::And, base, sixty_three);
    let all = b.iconst(Type::I64, -1);
    let lo_p = b.ptr_add(valid, lo_at, 8, 0);
    let hi_p = b.ptr_add(valid, hi_at, 8, 0);
    let (l_load, l_join) = (b.create_block(&[]), b.create_block(&[Type::I64]));
    b.cond_br(vnull, (l_join, &[all]), (l_load, &[]));
    b.switch_to(l_load);
    let lo = b.load(Type::I64, lo_p, 0);
    let hi = b.load(Type::I64, hi_p, 0);
    let lo_s = b.int_bin(IntBinOp::Ushr, lo, s);
    let back = b.int_bin(IntBinOp::Sub, sixty_four, s);
    let back = b.int_bin(IntBinOp::And, back, sixty_three);
    let hi_s = b.int_bin(IntBinOp::Shl, hi, back);
    let aligned = b.icmp(IntCond::Eq, s, env.zero);
    let hi_s = b.select(aligned, env.zero, hi_s);
    let v = b.int_bin(IntBinOp::Or, lo_s, hi_s);
    b.br(l_join, &[v]);
    b.switch_to(l_join);
    b.block_params(l_join)[0]
}
