//! Min / max over a Code lane: in-part rank, one render per window. Hooks / levers: -. Budget 220 lines (files.tsv, +25 %
//! gate). Owner M1-FOLD.
//!
//! Registers: kind `class.minmax_code` (`KindId::ClassMinmaxCode`; registered). A code orders by bytes only inside its part (every
//! part's dictionary is byte-rank sorted), and bytes are PG's order only under a collation that orders by bytes: the rows
//! compare by `rep_code_order::rank` under the aggregate's input collation (D247), which refuses `key.coll` (M6) for any
//! other collation. The carry `[word, where, n]` keeps the extreme row's `part << 32 | code` of its window (one part per
//! window) and the site / dictionary lane to render it through; the record keeps the rendered image (`rekind`: a
//! `MinVar` / `MaxVar` slot), one copy overwritten in place (code_copy.rs, D277), merged by bytes across windows,
//! parts, workers. D1750: records merged into a carry (a plain DISTINCT's walk, keyed_distinct_plain) leave it the better
//! copy's address (`where` = code_copy's `IMAGE`), compared by bytes.
use gravity_decompose::ops::shared::agg::{AggDecl, Register as R};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};

use crate::face::fold::{AccShape, Acc, ClassImpl, FoldClass};
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::rep::CellRef;

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::ClassMinmaxCode, &ClassImpl(&CODE))], ..crate::ops::NONE };

/// `.0`: a record slot holds keyed_parted's deferred minimum (a code word, never an image).
struct Code(bool);
static CODE: Code = Code(false);
static WORDS: Code = Code(true);

fn is_min(env: &Env, a: &AggDecl<'_>) -> bool {
    match a.reg {
        R::MinCode => true,
        R::MaxCode => false,
        _ => crate::refuse!(env.node, Reason::Bug { payload: "class.minmax_code over another register" }),
    }
}

/// The carry's identity word: a min's code above every code (u32::MAX, a tie taken by the row), a max's 0.
fn ident(b: &mut FunctionBuilder<'_>, min: bool) -> Value {
    b.iconst(Type::I64, if min { u32::MAX as i64 } else { 0 })
}

/// Whether code `new` takes the place of the carried word `cur`'s code (a tie takes the row: the same code of a part
/// is the same value).
fn better(b: &mut FunctionBuilder<'_>, min: bool, new: Value, cur: Value) -> Value {
    let m = b.iconst(Type::I64, u32::MAX as i64);
    let c = b.int_bin(IntBinOp::And, cur, m);
    b.icmp(if min { IntCond::Ule } else { IntCond::Uge }, new, c)
}

/// The carried word rendered through its site's dictionary (`where = site << 8 | lane`): the image's address, 0 for none;
/// a failed dictionary read raises its stashed error at once (D344).
fn render(b: &mut FunctionBuilder<'_>, env: &mut Env, word: Value, at: Value) -> Value {
    // the lent memo goes back: a dropped Lx would leave the function an empty memo, losing its rows and the bounded loop
    // headers the cancel verifier reads (D4445: a top-N's Fp128 loop before this render, refused as an unchecked loop)
    let cx = env.lx(b, env.node);
    let f = cx.frame();
    env.memo = cx.memo;
    let eight = b.iconst(Type::I64, 8);
    let k = b.int_bin(IntBinOp::Ushr, at, eight);
    let m = b.iconst(Type::I64, 0xff);
    let lane = b.int_bin(IntBinOp::And, at, m);
    let p = b.call_host(f.hosts.render, &[f.ctx, k, lane, word], Some(Type::Ptr)).expect("render returns a pointer");
    let pi = b.conv(ConvOp::PtrToInt, Type::I64, p);
    let failed = b.icmp(IntCond::Slt, pi, env.zero);
    let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
    let l_ok = b.create_block(&[]);
    b.cond_br(failed, (env.l_raise, &[e]), (l_ok, &[]));
    b.switch_to(l_ok);
    pi
}

/// A row into the register carry `w` where `ok`: its code ranked under `coll`, the better row's word kept.
fn carry(cx: &mut crate::rep::Lx<'_, '_, '_>, min: bool, coll: gravity_decompose::catalog::typefacts::KeyColl, c: CellRef<'_>, w: &mut [Value], ok: Value) {
    let r = crate::rep::code::order::rank(cx, c, coll);
    let word = crate::rep::code::datum::word(cx, c);
    let at = (crate::rep::code::datum::site(cx, c.cell) << 8 | crate::rep::code::datum::lane(cx, c.cell) as usize) as i64;
    let take = better(cx.b, min, r, w[0]);
    let take = cx.b.int_bin(IntBinOp::And, take, ok);
    w[0] = cx.b.select(take, word, w[0]);
    w[1] = cx.b.iconst(Type::I64, at);
    let n = cx.b.conv(ConvOp::Zext, Type::I64, ok);
    w[2] = cx.b.int_bin(IntBinOp::Add, w[2], n);
}

/// `step` with the end's image cursor (`images`: `cur, end, chunk`, the keyed ends' `AggState` arena; `None`: a pin per
/// copy).
fn step_at(b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, args: &[CellRef<'_>], images: Option<Value>) {
    let min = is_min(env, a);
    let [c] = args else { crate::refuse!(env.node, Reason::Bug { payload: "class.minmax_code over other than one argument" }) };
    let t = b.bconst(true);
    let ok = match c.null {
        Some(n) => b.int_bin(IntBinOp::Xor, n, t),
        None => t,
    };
    let mut cx = env.lx(b, env.node);
    match acc {
        Acc::Regs(w) => {
            carry(&mut cx, min, a.coll, *c, w, ok);
            env.memo = cx.memo;
        }
        Acc::Slot { base, off, valid } => {
            crate::rep::code::order::rank(&mut cx, *c, a.coll);
            let word = crate::rep::code::datum::word(&mut cx, *c);
            let at = (crate::rep::code::datum::site(&cx, c.cell) << 8 | crate::rep::code::datum::lane(&cx, c.cell) as usize) as i64;
            env.memo = cx.memo;
            let at = b.iconst(Type::I64, at);
            crate::ops::agg::trans::code_copy::fold_word(b, env, base, off, valid, word, at, min, ok, images);
        }
    }
}

/// `merge` with the end's image cursor (see `step_at`); `words`: `from`'s slot is a deferred minimum's code word.
fn merge_at(b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, into: Acc<'_>, from: Acc<'_>, images: Option<Value>, words: bool) {
    let min = is_min(env, a);
    match (into, from) {
        (Acc::Slot { base, off, valid }, Acc::Regs(f)) => {
            let any = b.icmp(IntCond::Ne, f[2], env.zero);
            crate::ops::agg::trans::code_copy::fold_word(b, env, base, off, valid, f[0], f[1], min, any, images);
        }
        (Acc::Slot { base, off, valid }, Acc::Slot { base: fb, off: fo, valid: fv }) => {
            crate::ops::agg::trans::code_copy::fold_slot(b, env, (base, off, valid), (fb, fo, fv), min, images);
        }
        (Acc::Regs(w), Acc::Regs(f)) => {
            // one window's carries: the same part, so the codes compare
            let m = b.iconst(Type::I64, u32::MAX as i64);
            let code = b.int_bin(IntBinOp::And, f[0], m);
            let any = b.icmp(IntCond::Ne, f[2], env.zero);
            let take = better(b, min, code, w[0]);
            let take = b.int_bin(IntBinOp::And, take, any);
            w[0] = b.select(take, f[0], w[0]);
            w[1] = b.select(any, f[1], w[1]);
            w[2] = b.int_bin(IntBinOp::Add, w[2], f[2]);
        }
        (Acc::Regs(_), Acc::Slot { .. }) if words => crate::refuse!(env.node, Reason::Shape { why: ShapeWhy("a deferred minimum's code word merged into a carry (keyed_parted's records under a plain DISTINCT)") }),
        (Acc::Regs(w), Acc::Slot { base, off, valid }) => crate::ops::agg::trans::code_copy::fold_image(b, env, w, (base, off, valid), min),
    }
}

/// A keyed end's step into a record slot with its image cursor (D277: the copies packed into the claim's image arena,
/// not a pinned region each): `class.step`, class.minmax_code folding with `images`.
#[allow(clippy::too_many_arguments, dead_code)] // KEYED's Acc::Slot sites (D277)
pub(crate) fn step_with(class: &'static dyn FoldClass, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, args: &[CellRef<'_>], images: Option<Value>) {
    match crate::props::class_kind(a) {
        KindId::ClassMinmaxCode if !crate::ops::sort::topn::strategies::parted::deferred(class) => step_at(b, env, a, acc, args, images),
        // N13: a keyed numeric extreme's side copies take the cursor too
        KindId::ClassNumPacked if crate::ops::shared::agg::order_keyed(a) => crate::ops::agg::numeric::class_num::step_keyed(b, env, a, acc, args, images),
        _ => class.step(b, env, a, acc, args),
    }
}

/// `step_with`'s merge (a carry into a record slot, a record into another).
#[allow(clippy::too_many_arguments, dead_code)] // KEYED's run close (D277)
pub(crate) fn merge_with(class: &'static dyn FoldClass, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, into: Acc<'_>, from: Acc<'_>, images: Option<Value>) {
    match crate::props::class_kind(a) {
        KindId::ClassMinmaxCode if !crate::ops::sort::topn::strategies::parted::deferred(class) => merge_at(b, env, a, into, from, images, false),
        _ => class.merge(b, env, a, into, from),
    }
}

impl FoldClass for Code {
    fn shape(&self, _a: &AggDecl<'_>) -> AccShape {
        AccShape { words: 3, bytes: 8, align: 8 }
    }

    fn init(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) {
        let min = is_min(env, a);
        match acc {
            Acc::Regs(w) => {
                w[0] = ident(b, min);
                w[1] = env.zero;
                w[2] = env.zero;
            }
            Acc::Slot { base, off, .. } => b.store(base, off, env.zero),
        }
    }

    /// A row: its code's rank under the collation (`key.coll` refuses any collation not ordering by bytes); a register
    /// carry keeps the better word, a record slot folds the row's word into its copy (no image cursor: a pin per copy).
    fn step(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, args: &[CellRef<'_>]) {
        step_at(b, env, a, acc, args, None);
    }

    fn merge(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, into: Acc<'_>, from: Acc<'_>) {
        merge_at(b, env, a, into, from, None, self.0);
    }

    /// The image's address (a text / bytea datum), NULL over no row; a carry holding a record's copy (D1750) is its address.
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _a: &AggDecl<'_>, acc: Acc<'_>) -> (Value, Value) {
        let (p, ok) = match acc {
            Acc::Slot { base, off, valid } => {
                let ok = crate::ops::shared::slots::slot_ok(b, env, base, valid);
                (b.load(Type::I64, base, off), ok)
            }
            Acc::Regs(w) => {
                let any = b.icmp(IntCond::Ne, w[2], env.zero);
                let (l_r, l_code, l_out) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64]));
                b.cond_br(any, (l_r, &[]), (l_out, &[env.zero]));
                b.switch_to(l_r);
                let image = b.iconst(Type::I64, crate::ops::agg::trans::code_copy::IMAGE);
                let copy = b.icmp(IntCond::Eq, w[1], image);
                b.cond_br(copy, (l_out, &[w[0]]), (l_code, &[]));
                b.switch_to(l_code);
                let p = render(b, env, w[0], w[1]);
                b.br(l_out, &[p]);
                b.switch_to(l_out);
                (b.block_params(l_out)[0], any)
            }
        };
        let some = b.icmp(IntCond::Ne, p, env.zero);
        let ok = b.int_bin(IntBinOp::And, ok, some);
        let t = b.bconst(true);
        (p, b.int_bin(IntBinOp::Xor, ok, t))
    }
}

/// class.minmax_code's own class: its registers carry a deferred minimum's rows too (V4R-105, keyed_parted `CodeSlot`),
/// whose record slots hold code words.
pub(crate) fn code() -> &'static dyn FoldClass {
    &WORDS
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use crate::cell::{At, Cell, CellLife, LaneId, Nullable};
    use gravity_abi::{ctx::site_off, site::site};
    use gravity_decompose::catalog::typefacts::KeyColl;
    use gravity_program::{DictRef, NumTypmod, Rep};

    /// Two rows' codes (part 3) folded by the register carry under `coll`: the carried `(word, where, n)`.
    fn fold2(min: bool, coll: KeyColl, codes: [u32; 2]) -> [u64; 3] {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let c = Cell { rep: Rep::Code { dict: DictRef::lane(2) }, typid: 25, num: NumTypmod::NONE, life: CellLife::of_lane(gravity_program::LaneLife::DICTIONARY), at: At::Lane(LaneId::col(2)), null: Nullable::Maybe };
        let mut pb = gravity_ir::builder::ProgramBuilder::new();
        let render = pb.declare_host("agg_render", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Some(Type::Ptr), 0, Default::default());
        let f = pb.declare_func("k", &[Type::Ptr, Type::Ptr], &[Type::I64, Type::I64, Type::I64]);
        let mut b = pb.define(f);
        let (ctx, elem) = (b.params()[0], b.params()[1]);
        let (id, zero, ok) = (ident(&mut b, min), b.iconst(Type::I64, 0), b.bconst(true));
        let mut w = [id, zero, zero];
        {
            let mut cx = crate::rep::Lx::new(&mut b, 1, rig.mcx());
            cx.at = Some(crate::rep::Frame { ctx, site: Some(0), hosts: crate::rep::RepHosts { render }, tables: None });
            for i in 0..2 {
                let val = cx.b.ptr_add(elem, zero, 1, 4 * i);
                carry(&mut cx, min, coll, CellRef { cell: &c, val, null: None }, &mut w, ok);
            }
        }
        b.ret(&w);
        let prog = pb.finish();
        let mut ctxw = vec![0u64; site_off(1) / 8 + 1];
        // SAFETY: the words are the ctx image; PART_ID is a u32 field inside site 0.
        unsafe { (ctxw.as_mut_ptr().cast::<u8>().add(site_off(0) + site::PART_ID) as *mut u32).write_unaligned(3) };
        let got = gravity_interp::Interp::new(&prog).run(f, &[ctxw.as_ptr() as u64, codes.as_ptr() as u64]).expect("runs");
        [got[0], got[1], got[2]]
    }

    const C: KeyColl = KeyColl { oid: 950, bytes_eq: true, bytes_order: true };

    /// Under C the codes order as the bytes: min / max keep the extreme row's `part << 32 | code`, the dictionary lane 2.
    #[test]
    fn codes_rank_as_bytes_under_c() {
        assert_eq!(fold2(true, C, [41, 7]), [3 << 32 | 7, 2, 2]);
        assert_eq!(fold2(false, C, [41, 7]), [3 << 32 | 41, 2, 2]);
        assert_eq!(fold2(true, C, [u32::MAX, u32::MAX]), [3 << 32 | u32::MAX as u64, 2, 2], "the top code is still taken");
    }

    /// A text MIN / MAX under a collation that does not sort by bytes refuses `key.coll` (M6) instead of answering in code
    /// order (D247, main 09-24).
    #[test]
    fn a_non_c_collation_refuses_key_coll() {
        let en = KeyColl { oid: 16390, bytes_eq: true, bytes_order: false };
        for min in [true, false] {
            let r = crate::catch(1, || fold2(min, en, [41, 7])).expect_err("refuses");
            assert!(matches!(r.reason, Reason::Shape { .. }), "{:?}", r.reason);
        }
    }
}
