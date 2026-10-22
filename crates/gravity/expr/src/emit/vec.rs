//! The vector value form (expr-compiler.md §8): a `Var`, `Var ± Const`, the compares and their
//! AND / OR over fixed-width all-valid lanes, `rows` rows per 128-bit chunk, the constants' splats
//! hoisted where the loop is entered; and the aggregates' vector transitions (`VecAgg`: the
//! widen-add, the mask count, min / max, the 128-bit carry pair) with their horizontal finals.

use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value, VecBinOp, VecShiftOp};
use mcx::{GrowIn, Mcx, PgVec};
use types_nodes::primnodes::{BoolExprType, ParamKind};

use super::word::{fits, vec_ty, WordRefs};
use super::{prim, EmitError, Plan};
use crate::agg::VecAgg;
use crate::eval::Source;
use crate::nodes::{ExprNode, NodeId, Program};

/// One chunk's lanes: the datum values at the lane's width, or a compare mask (all-ones lanes).
#[derive(Clone, Copy, Debug)]
pub struct VecLowered {
    pub value: Value,
    pub ty: Type,
    pub rows: u8,
    pub mask: bool,
    /// The lanes whose row would have raised (an overflowing `+` / `-`); `None`: cannot raise.
    pub err: Option<Value>,
}

/// One node of the vector form's tree: the tree is one list in the emit's scratch (`Check::shapes`, `VecEmit::shapes`),
/// each operand by its index there.
enum Shape<'s> {
    Lane(i16),
    Const(u32),
    Arith { op: VecBinOp, a: u32, b: u32 },
    Cmp { cond: IntCond, a: u32, b: u32 },
    /// An IN list's compares: a run of the plan's needs.
    In { attno: i16, consts: core::ops::Range<u32> },
    And(PgVec<'s, u32>),
    Or(PgVec<'s, u32>),
}

impl Shape<'_> {
    fn is_mask(&self) -> bool {
        matches!(self, Shape::Cmp { .. } | Shape::In { .. } | Shape::And(_) | Shape::Or(_))
    }
}

/// The constants under shape `i` of `sh`, each with the compare it feeds (`Eq` outside a compare).
fn consts_of(sh: &[Shape<'_>], i: u32, out: &mut PgVec<'_, (u32, IntCond)>) {
    crate::stack_check();
    match &sh[i as usize] {
        Shape::Lane(_) => {}
        Shape::Const(c) => out.push_in((*c, IntCond::Eq)),
        Shape::Arith { a, b, .. } => {
            consts_of(sh, *a, out);
            consts_of(sh, *b, out);
        }
        Shape::Cmp { cond, a, b } => {
            let n = out.len();
            consts_of(sh, *a, out);
            consts_of(sh, *b, out);
            out[n..].iter_mut().for_each(|x| x.1 = *cond);
        }
        Shape::In { consts, .. } => out.extend_in((consts.start..consts.end).map(|c| (c, IntCond::Eq))),
        Shape::And(v) | Shape::Or(v) => v.iter().for_each(|&x| consts_of(sh, x, out)),
    }
}

fn has_arith(sh: &[Shape<'_>], i: u32) -> bool {
    crate::stack_check();
    match &sh[i as usize] {
        Shape::Arith { .. } => true,
        Shape::Cmp { a, b, .. } => has_arith(sh, *a) || has_arith(sh, *b),
        Shape::And(v) | Shape::Or(v) => v.iter().any(|&x| has_arith(sh, x)),
        _ => false,
    }
}

struct Check<'c, 'a, 's> {
    p: &'c Program<'a>,
    plan: &'c Plan<'c>,
    w: &'c WordRefs<'s>,
    width: Option<u8>,
    shapes: PgVec<'s, Shape<'s>>,
}

impl<'s> Check<'_, '_, 's> {
    fn lane(&mut self, id: NodeId) -> Result<i16, EmitError> {
        let ExprNode::Var(v) = &self.p.nodes[id.0 as usize] else { return Err(EmitError::NotVector) };
        if v.src != Source::Scan {
            return Err(EmitError::NotVector);
        }
        let lane = self.w.lanes.get(v.attno as usize - 1).copied().flatten().ok_or(EmitError::NotVector)?;
        if lane.kind != gravity_abi::ctx::lane::KIND_BYVAL || !matches!(lane.width, 1 | 2 | 4 | 8) || self.width.is_some_and(|w| w != lane.width) {
            return Err(EmitError::NotVector);
        }
        self.width = Some(lane.width);
        Ok(v.attno)
    }

    fn put(&mut self, s: Shape<'s>) -> u32 {
        self.shapes.push_in(s);
        self.shapes.len() as u32 - 1
    }

    /// The shape of node `id`: its index in `shapes`.
    fn shape(&mut self, id: NodeId) -> Result<u32, EmitError> {
        crate::stack_check();
        match &self.p.nodes[id.0 as usize] {
            ExprNode::Var(_) => {
                let attno = self.lane(id)?;
                Ok(self.put(Shape::Lane(attno)))
            }
            ExprNode::Const(_) => {
                let ci = self.plan.index(id).ok_or(EmitError::NotVector)?;
                Ok(self.put(Shape::Const(ci)))
            }
            ExprNode::Param(q) if q.kind == ParamKind::PARAM_EXTERN || q.kind == ParamKind::PARAM_EXEC => {
                let ci = self.plan.index(id).ok_or(EmitError::NotVector)?;
                Ok(self.put(Shape::Const(ci)))
            }
            ExprNode::Relabel(r) if r.transparent() => self.shape(r.arg),
            ExprNode::Op(n) | ExprNode::Func(n) => {
                let call = &self.p.sites[n.site.0 as usize];
                if call.args.len != 2 {
                    return Err(EmitError::NotVector);
                }
                let a = self.shape(self.p.kids[call.args.start as usize])?;
                let b = self.shape(self.p.kids[call.args.start as usize + 1])?;
                let width = self.width.ok_or(EmitError::NotVector)?;
                if self.shapes[a as usize].is_mask() || self.shapes[b as usize].is_mask() {
                    return Err(EmitError::NotVector);
                }
                // the operands stay in the node's order, so a commuted compare keeps its condition
                if let Some(cond) = prim::vector_cond(call.funcid, width, false) {
                    return Ok(self.put(Shape::Cmp { cond, a, b }));
                }
                let op = prim::vector_arith(call.funcid, width).ok_or(EmitError::NotVector)?;
                Ok(self.put(Shape::Arith { op, a, b }))
            }
            ExprNode::ScalarArrayOp(n) => {
                let call = &self.p.sites[n.site.0 as usize];
                let (a, arr) = (self.p.kids[call.args.start as usize], self.p.kids[call.args.start as usize + 1]);
                let attno = self.lane(a)?;
                let cond = prim::vector_cond(call.funcid, self.width.unwrap_or(0), false);
                let (start, count, null) = self.plan.elems_of(arr).ok_or(EmitError::NotVector)?;
                if !n.use_or || cond != Some(IntCond::Eq) || null || count == 0 || count as usize > gravity_program::cost::expr::MAX_LIST {
                    return Err(EmitError::NotVector);
                }
                Ok(self.put(Shape::In { attno, consts: start..start + count }))
            }
            ExprNode::Bool(n) => {
                let ks = crate::percode::walk_kids(self.p, id);
                let mut parts = mcx::reserved_in(ks.count_kids(), self.w.scratch);
                for k in ks {
                    let x = self.shape(k)?;
                    parts.push_in(x);
                }
                if parts.iter().any(|&x| !self.shapes[x as usize].is_mask()) {
                    return Err(EmitError::NotVector);
                }
                match n.op {
                    BoolExprType::AND_EXPR => Ok(self.put(Shape::And(parts))),
                    BoolExprType::OR_EXPR => Ok(self.put(Shape::Or(parts))),
                    BoolExprType::NOT_EXPR => Err(EmitError::NotVector),
                }
            }
            _ => Err(EmitError::NotVector),
        }
    }
}

/// The program's vector form, prepared where the loop is entered (§8.1): the chunk's shape and
/// the hoisted splats; `chunk` per chunk, `ok` the fits guard.
pub struct VecEmit<'s> {
    /// The tree (`Shape`), its root the shape at `root`, in the emit's scratch with the lists below.
    shapes: PgVec<'s, Shape<'s>>,
    root: u32,
    pub width: u8,
    pub rows: u8,
    pub ty: Type,
    pub mask: bool,
    consts: PgVec<'s, (u32, IntCond)>,
    splats: PgVec<'s, (u32, Value)>,
    zero: Option<Value>,
    addrs: PgVec<'s, (Value, i16, Value)>,
    scratch: Mcx<'s>,
}

pub fn emit<'s>(p: &Program<'_>, root: NodeId, b: &mut FunctionBuilder<'_>, w: &WordRefs<'s>, plan: &Plan) -> Result<VecEmit<'s>, EmitError> {
    let m = w.scratch;
    let mut c = Check { p, plan, w, width: None, shapes: PgVec::new_in(m) };
    let top = c.shape(root)?;
    let width = c.width.ok_or(EmitError::NotVector)?;
    let shapes = c.shapes;
    let (ty, lty) = vec_ty(width);
    let mut consts = PgVec::new_in(m);
    consts_of(&shapes, top, &mut consts);
    let mut splats = PgVec::new_in(m);
    for &(ci, _) in &consts {
        if splats.iter().any(|(i, _)| *i == ci) {
            continue;
        }
        let v = w.consts[ci as usize];
        let t = if width == 8 { v } else { b.conv(ConvOp::Trunc, lty, v) };
        splats.push_in((ci, b.vsplat(ty, t)));
    }
    let zero = has_arith(&shapes, top).then(|| {
        let z = b.iconst(lty, 0);
        b.vsplat(ty, z)
    });
    let mask = shapes[top as usize].is_mask();
    Ok(VecEmit { shapes, root: top, width, rows: 16 / width, ty, mask, consts, splats, zero, addrs: PgVec::new_in(m), scratch: m })
}

impl VecEmit<'_> {
    pub fn ok(&self, b: &mut FunctionBuilder<'_>, w: &WordRefs<'_>) -> Value {
        let mut acc = b.iconst(Type::I64, 1);
        for &(ci, cond) in &self.consts {
            let f = fits(b, w.consts[ci as usize], w.consts_null[ci as usize], self.width, cond);
            acc = b.int_bin(IntBinOp::And, acc, f);
        }
        acc
    }

    /// The chunk over rows `base + k·rows ..`; the lane address of `base` shared across its chunks.
    pub fn chunk(&mut self, b: &mut FunctionBuilder<'_>, w: &WordRefs<'_>, base: Value, k: u32) -> VecLowered {
        let shapes = core::mem::replace(&mut self.shapes, PgVec::new_in(self.scratch));
        let (value, err) = self.eval(b, w, base, k, &shapes, self.root);
        self.shapes = shapes;
        VecLowered { value, ty: self.ty, rows: self.rows, mask: self.mask, err }
    }

    fn load(&mut self, b: &mut FunctionBuilder<'_>, w: &WordRefs<'_>, base: Value, k: u32, attno: i16) -> Value {
        let at = match self.addrs.iter().find(|(v, a, _)| *v == base && *a == attno) {
            Some((_, _, at)) => *at,
            None => {
                let lane = w.lanes[attno as usize - 1].expect("the shape checked the lane");
                let at = b.ptr_add(lane.ptr, base, lane.width, 0);
                self.addrs.push_in((base, attno, at));
                at
            }
        };
        b.vload_unaligned(self.ty, at, (k * 16) as i32)
    }

    fn splat(&self, ci: u32) -> Value {
        self.splats.iter().find(|(i, _)| *i == ci).expect("hoisted at emit").1
    }

    fn eval(&mut self, b: &mut FunctionBuilder<'_>, w: &WordRefs<'_>, base: Value, k: u32, sh: &[Shape<'_>], i: u32) -> (Value, Option<Value>) {
        crate::stack_check();
        let s = &sh[i as usize];
        match s {
            Shape::Lane(attno) => (self.load(b, w, base, k, *attno), None),
            Shape::Const(ci) => (self.splat(*ci), None),
            Shape::Arith { op, a, b: y } => {
                let (x, ex) = self.eval(b, w, base, k, sh, *a);
                let (y, ey) = self.eval(b, w, base, k, sh, *y);
                let r = b.vbin(*op, x, y);
                // signed overflow: `+` when both operands' signs differ from the result's, `-` when the
                // operands' signs differ and the result's is the subtrahend's
                let (t1, t2) = if *op == VecBinOp::Add { (b.vbin(VecBinOp::Xor, x, r), b.vbin(VecBinOp::Xor, y, r)) } else { (b.vbin(VecBinOp::Xor, x, y), b.vbin(VecBinOp::Xor, x, r)) };
                let m = b.vbin(VecBinOp::And, t1, t2);
                let zero = self.zero.expect("hoisted at emit");
                let ovf = b.vcmp(IntCond::Slt, m, zero);
                (r, Some(or3(b, ex, ey, ovf)))
            }
            Shape::Cmp { cond, a, b: y } => {
                let (x, ex) = self.eval(b, w, base, k, sh, *a);
                let (y, ey) = self.eval(b, w, base, k, sh, *y);
                (b.vcmp(*cond, x, y), or2(b, ex, ey))
            }
            Shape::In { attno, consts } => {
                let v = self.load(b, w, base, k, *attno);
                let mut m: Option<Value> = None;
                for ci in consts.start..consts.end {
                    let s = self.splat(ci);
                    let x = b.vcmp(IntCond::Eq, v, s);
                    m = Some(match m {
                        Some(a) => b.vbin(VecBinOp::Or, a, x),
                        None => x,
                    });
                }
                (m.expect("a non-empty list"), None)
            }
            Shape::And(v) | Shape::Or(v) => {
                let op = if matches!(s, Shape::And(_)) { VecBinOp::And } else { VecBinOp::Or };
                let mut acc: Option<(Value, Option<Value>)> = None;
                for &x in v.iter() {
                    let (m, e) = self.eval(b, w, base, k, sh, x);
                    acc = Some(match acc {
                        Some((a, ea)) => (b.vbin(op, a, m), or2(b, ea, e)),
                        None => (m, e),
                    });
                }
                acc.expect("a non-empty chain")
            }
        }
    }
}

fn or2(b: &mut FunctionBuilder<'_>, x: Option<Value>, y: Option<Value>) -> Option<Value> {
    match (x, y) {
        (Some(x), Some(y)) => Some(b.vbin(VecBinOp::Or, x, y)),
        (x, None) => x,
        (None, y) => y,
    }
}

fn or3(b: &mut FunctionBuilder<'_>, x: Option<Value>, y: Option<Value>, z: Value) -> Value {
    let xy = or2(b, x, y);
    or2(b, xy, Some(z)).expect("z")
}

/// An aggregate's vector state: the loop-carried accumulator(s) (`hi` for `Sum128`).
#[derive(Clone, Copy, Debug)]
pub struct VecAcc {
    pub lo: Value,
    pub hi: Option<Value>,
    pub ty: Type,
}

/// The horizontal final: an `I64` (`hi` the 128-bit sum's high word).
#[derive(Clone, Copy, Debug)]
pub struct VecFinal {
    pub value: Value,
    pub hi: Option<Value>,
}

fn ceil_log2(n: u32) -> u32 {
    32 - n.max(1).saturating_sub(1).leading_zeros()
}

fn lane_of_bits(bits: u32) -> Option<Type> {
    match bits {
        ..=8 => Some(Type::V16I8),
        ..=16 => Some(Type::V8I16),
        ..=32 => Some(Type::V4I32),
        ..=64 => Some(Type::V2I64),
        _ => None,
    }
}

impl VecAgg {
    /// The narrowest state lane holding `rows_bound` rows of the chunk (§8.2's table); `None`: refused.
    pub fn state_type(self, e: &VecEmit, rows_bound: u32) -> Option<Type> {
        let bits = e.width as u32 * 8;
        match self {
            VecAgg::Sum => (!e.mask && e.width >= 2).then(|| lane_of_bits(bits + ceil_log2(rows_bound))).flatten(),
            VecAgg::Count => {
                let mut b = bits;
                while (rows_bound as u64) > (1u64 << (b - 1)) - 1 {
                    b *= 2;
                }
                e.mask.then(|| lane_of_bits(b)).flatten()
            }
            VecAgg::Min | VecAgg::Max | VecAgg::UMin | VecAgg::UMax => (!e.mask && e.width >= 2).then_some(e.ty),
            VecAgg::Sum128 => (!e.mask && e.ty == Type::V2I64).then_some(Type::V2I64),
        }
    }

    pub fn init(self, b: &mut FunctionBuilder<'_>, ty: Type) -> VecAcc {
        let lane = ty.lane();
        let bits = lane_bits(lane);
        let seed = match self {
            VecAgg::Min => !(-1i64 << (bits - 1)),
            VecAgg::Max => -1i64 << (bits - 1),
            VecAgg::UMin => -1,
            _ => 0,
        };
        let s = b.iconst(lane, seed);
        let lo = b.vsplat(ty, s);
        let hi = (self == VecAgg::Sum128).then(|| {
            let z = b.iconst(lane, 0);
            b.vsplat(ty, z)
        });
        VecAcc { lo, hi, ty }
    }

    fn op(self) -> VecBinOp {
        match self {
            VecAgg::Sum | VecAgg::Sum128 => VecBinOp::Add,
            VecAgg::Count => VecBinOp::Sub,
            VecAgg::Min => VecBinOp::Smin,
            VecAgg::Max => VecBinOp::Smax,
            VecAgg::UMin => VecBinOp::Umin,
            VecAgg::UMax => VecBinOp::Umax,
        }
    }

    pub fn transition(self, b: &mut FunctionBuilder<'_>, acc: VecAcc, c: &VecLowered) -> VecAcc {
        match self {
            VecAgg::Sum128 => {
                let (lo, hi) = carry_add(b, acc.lo, acc.hi.expect("a pair"), c.value);
                VecAcc { lo, hi: Some(hi), ty: acc.ty }
            }
            VecAgg::Sum | VecAgg::Count => VecAcc { lo: widen_fold(b, self.op(), acc.lo, c.value, c.ty, acc.ty), ..acc },
            _ => VecAcc { lo: b.vbin(self.op(), acc.lo, c.value), ..acc },
        }
    }

    pub fn combine(self, b: &mut FunctionBuilder<'_>, x: VecAcc, y: VecAcc) -> VecAcc {
        match self {
            VecAgg::Sum128 => {
                // the pair add: `y.lo` is an unsigned low word (no sign term, unlike a chunk's value)
                let lo = b.vbin(VecBinOp::Add, x.lo, y.lo);
                let carry = b.vcmp(IntCond::Ult, lo, x.lo);
                let hi = b.vbin(VecBinOp::Add, x.hi.expect("a pair"), y.hi.expect("a pair"));
                VecAcc { lo, hi: Some(b.vbin(VecBinOp::Sub, hi, carry)), ty: x.ty }
            }
            VecAgg::Count => VecAcc { lo: b.vbin(VecBinOp::Add, x.lo, y.lo), ..x },
            _ => VecAcc { lo: b.vbin(self.op(), x.lo, y.lo), ..x },
        }
    }

    pub fn finish(self, b: &mut FunctionBuilder<'_>, acc: VecAcc) -> VecFinal {
        match self {
            VecAgg::Sum | VecAgg::Count => VecFinal { value: b.vreduce_add(acc.lo), hi: None },
            VecAgg::Sum128 => {
                let hi = acc.hi.expect("a pair");
                let (l0, l1, h0, h1) = (b.vextract(acc.lo, 0), b.vextract(acc.lo, 1), b.vextract(hi, 0), b.vextract(hi, 1));
                let lo = b.int_bin(IntBinOp::Add, l0, l1);
                let c = b.icmp(IntCond::Ult, lo, l0);
                let c64 = b.conv(ConvOp::Zext, Type::I64, c);
                let h = b.int_bin(IntBinOp::Add, h0, h1);
                VecFinal { value: lo, hi: Some(b.int_bin(IntBinOp::Add, h, c64)) }
            }
            _ => {
                let unsigned = matches!(self, VecAgg::UMin | VecAgg::UMax);
                let op = match self {
                    VecAgg::Min => IntBinOp::Smin,
                    VecAgg::Max => IntBinOp::Smax,
                    VecAgg::UMin => IntBinOp::Umin,
                    _ => IntBinOp::Umax,
                };
                let mut out: Option<Value> = None;
                for k in 0..acc.ty.lanes() as u8 {
                    let x = b.vextract(acc.lo, k);
                    let x = if acc.ty.lane() == Type::I64 { x } else { b.conv(if unsigned { ConvOp::Zext } else { ConvOp::Sext }, Type::I64, x) };
                    out = Some(match out {
                        Some(o) => b.int_bin(op, x, o),
                        None => x,
                    });
                }
                VecFinal { value: out.expect("lanes"), hi: None }
            }
        }
    }
}

fn lane_bits(lane: Type) -> u32 {
    match lane {
        Type::I8 => 8,
        Type::I16 => 16,
        Type::I32 => 32,
        _ => 64,
    }
}

/// `acc op v` with `v` widened (signed, low then high half) from `from` to `to`, one doubling per step.
fn widen_fold(b: &mut FunctionBuilder<'_>, op: VecBinOp, acc: Value, v: Value, from: Type, to: Type) -> Value {
    crate::stack_check();
    if from == to {
        return b.vbin(op, acc, v);
    }
    let next = from.widened().expect("a narrower lane");
    let lo = b.vwiden(v, true, false);
    let hi = b.vwiden(v, true, true);
    let a = widen_fold(b, op, acc, lo, next, to);
    widen_fold(b, op, a, hi, next, to)
}

/// `hi:lo += sext(v)` per `V2I64` lane: the carry is the unsigned wrap of the low word.
fn carry_add(b: &mut FunctionBuilder<'_>, lo: Value, hi: Value, v: Value) -> (Value, Value) {
    let lo2 = b.vbin(VecBinOp::Add, lo, v);
    let carry = b.vcmp(IntCond::Ult, lo2, lo);
    let sgn = b.vshift(VecShiftOp::Sshr, v, 63);
    let hi1 = b.vbin(VecBinOp::Add, hi, sgn);
    (lo2, b.vbin(VecBinOp::Sub, hi1, carry))
}
