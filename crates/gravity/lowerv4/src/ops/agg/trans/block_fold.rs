//! Popcount / masked sum / min / max per block. Hooks / levers: H01c R2-L03. Budget 160 lines (files.tsv, +25 % gate).
//! Owner M1-FOLD.
//!
//! Registers: road `mask.fold` (`MASK_FOLD`): a whole 64-row block whose passing rows are a mask folded at once by
//! the end, instead of walking its rows: a count by popcount (a column's count ANDs its validity in), a sum / min / max
//! over the set bits of one by-value lane read as the row path reads it (8 bytes as they are, 4 / 2 sign-extended, 1
//! zero-extended), a min / max of an unsigned word order through its sign-flipped image (FOLD-13). Asked only when every
//! step after the first keeps its rows (frame.rs `block_loop`), so the mask is exactly the rows the end would see.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, IntUnOp, Type, Value};
use mcx::GrowIn;

use crate::face::fold::{Acc, BlockFold, BlockFoldImpl, BlockFoldMech};
use crate::frame::env::Env;
use crate::frame::frame::{Mask, WordWin};
use crate::frame::px::Px;
use crate::frame::row::Carry;

crate::strategies! {
    /// Popcount / masked sum / min / max per block.
    MASK_FOLD = 3, "mask.fold", FoldScan;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(MASK_FOLD, &BlockFoldImpl(&BLOCK))], ..crate::ops::NONE };

struct Block;
static BLOCK: Block = Block;

/// The scan column (the window's lane by attno) row column `c` of pipeline `k`'s end reads: `c` through each projection
/// back to a bare scan column, `None` past anything else (the end then walks rows).
pub(crate) fn scan_attno(px: &Px<'_, '_>, k: usize, c: i16) -> Option<i16> {
    use gravity_decompose::graph::Op;
    let mut c = c;
    for op in px.gx.pipelines()[k].ops.iter().rev() {
        match op {
            Op::Filter(_) | Op::Gate(_) => {}
            Op::Project(list) => {
                let e = list.iter().nth(c as usize - 1)?;
                c = crate::step::project::bare_var(e.as_target_entry().map_or(e, |t| t.expr))?;
            }
            _ => return None,
        }
    }
    Some(c)
}

impl BlockFoldMech for Block {
    /// Lane `lane` (an attno − 1 of `w`; `usize::MAX` for `count(*)`) of block `[base, base + 64)` under `m` into the carry
    /// words `acc` (`[value…, n]`): the lane's validity ANDed in, the set bits walked by count-trailing-zeros.
    fn fold(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, form: BlockFold, w: &WordWin, lane: usize, base: Value, m: Mask, acc: Acc<'_>) {
        let Acc::Regs(acc) = acc else { crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a block fold into a slot" }) };
        let m = if lane == usize::MAX { m.0 } else { crate::frame::block::valid_and(b, env, w, &[lane as i16 + 1], base, m.0) };
        let n = b.int_un(IntUnOp::Popcnt, m);
        let k = acc.len() - 1;
        acc[k] = b.int_bin(IntBinOp::Add, acc[k], n);
        if form == BlockFold::Count {
            return;
        }
        let Some(l) = w.lanes.get(lane).copied().flatten() else { crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a block fold over a lane the window lacks" }) };
        let op = match form {
            BlockFold::Min | BlockFold::MinImage => IntBinOp::Smin,
            BlockFold::Max | BlockFold::MaxImage => IntBinOp::Smax,
            _ => IntBinOp::Add,
        };
        let (l_loop, l_body, l_out) = (b.create_block(&[Type::I64, Type::I64]), b.create_block(&[]), b.create_block(&[Type::I64]));
        b.br(l_loop, &[m, acc[0]]);
        b.mark_loop_header(l_loop);
        env.bounded.push_in(l_loop);
        b.switch_to(l_loop);
        let (bits, v) = (b.block_params(l_loop)[0], b.block_params(l_loop)[1]);
        let more = b.icmp(IntCond::Ne, bits, env.zero);
        b.cond_br(more, (l_body, &[]), (l_out, &[v]));
        b.switch_to(l_body);
        let t = b.int_un(IntUnOp::Ctz, bits);
        let i = b.int_bin(IntBinOp::Add, base, t);
        let x = lane_word(b, env, &l, i);
        // FOLD-13: an unsigned word order folds its sign-flipped image, as the row path does (class_word `row_words`)
        let x = if matches!(form, BlockFold::MinImage | BlockFold::MaxImage) { flip(b, x) } else { x };
        let v2 = b.int_bin(op, v, x);
        let one = b.int_bin(IntBinOp::Sub, bits, env.one);
        let rest = b.int_bin(IntBinOp::And, bits, one);
        b.br(l_loop, &[rest, v2]);
        b.switch_to(l_out);
        acc[0] = b.block_params(l_out)[0];
    }
}

/// `x` with its sign bit flipped: an unsigned word's order as a signed one.
fn flip(b: &mut FunctionBuilder<'_>, x: Value) -> Value {
    let m = b.iconst(Type::I64, i64::MIN);
    b.int_bin(IntBinOp::Xor, x, m)
}

/// Lane `l`'s word at row `i` as the row path reads a by-value cell (expr scalar `lane_cell`): 8 bytes as they are, 4 / 2
/// sign-extended, 1 zero-extended (bool).
fn lane_word(b: &mut FunctionBuilder<'_>, env: &Env, l: &gravity_expr::emit::LaneRef, i: Value) -> Value {
    let p = b.ptr_add(l.ptr, i, l.width, 0);
    match l.width {
        8 => b.load(Type::I64, p, 0),
        4 | 2 => {
            let x = b.load(if l.width == 4 { Type::I32 } else { Type::I16 }, p, 0);
            b.conv(gravity_ir::ConvOp::Sext, Type::I64, x)
        }
        1 => b.load_zext(Type::I8, p, 0),
        _ => crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a block fold over a lane of another width" }),
    }
}

/// `EndMech::block` for a fold end: every aggregate folds by block form (no FILTER, `count(*)` or a column the window's
/// lanes hold) and the end carries its states, else `None` (the rows are walked).
#[allow(clippy::too_many_arguments)]
pub(crate) fn end_block<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, folds: &[crate::ops::agg::trans::plain::Fold<'_, '_>], carry: bool, w: &WordWin, base: Value, m: Mask, c: &Carry) -> Option<Carry<'e>> {
    if !carry || !env.applies(MASK_FOLD) {
        return None;
    }
    let mut plan = mcx::reserved_in(folds.len(), env.mcx);
    for f in folds {
        let form = f.class.block(f.a).filter(|_| f.filter.is_none())?;
        let lane = match (f.args.as_slice(), f.scan) {
            ([], _) => usize::MAX,
            // a count reads only the lane's validity; another form its word (`lane_word`'s widths)
            ([_], Some(a)) if w.lanes.get(a as usize - 1).copied().flatten().is_some_and(|l| form == BlockFold::Count || matches!(l.width, 1 | 2 | 4 | 8)) => a as usize - 1,
            _ => return None,
        };
        plan.push_in((form, lane));
    }
    fires::fire("agg.vec_plain"); // shapecheck fire signal (REGISTRY DC-11): a plain aggregate folds whole masks by its classes' block forms
    let mut accs = mcx::to_vec_in(&c.accs[..], env.mcx);
    for (f, (form, lane)) in folds.iter().zip(plan) {
        BLOCK.fold(b, env, form, w, lane, base, m, Acc::Regs(&mut accs[f.at..f.at + f.words]));
    }
    let n = b.int_un(IntUnOp::Popcnt, m.0);
    Some(Carry { out: b.int_bin(IntBinOp::Add, c.out, n), accs })
}
