//! The 64-row vector form (expr-compiler.md §2.1): a predicate over fixed-width lanes — compares
//! of a lane against hoisted constants, IN lists, AND / OR — as `vcmp` per 128-bit vector and
//! `vmovmask` into the word; `ok` the per-window guard that every constant fits its lane.

use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntCond, Type, Value, VecBinOp};
use mcx::{GrowIn, Mcx, PgVec};
use types_nodes::primnodes::BoolExprType;

use super::percode::{CodeRef, TableRef};
use super::{prim, EmitError, LaneRef, Plan};
use crate::eval::Source;
use crate::nodes::{ExprNode, NodeId, Program};
use crate::percode::{Form, Plan as PerCodePlan};

/// The per-code roots of the program over the caller's code lanes (`emit_word_codes`): a `<> ''`
/// root is a compare of the code lane against the window's empty code, a `Bits` root the gather of
/// its table's bit per row (the dictionary's `gather_mask`).
pub type Codes<'a> = Option<(&'a PerCodePlan<'a>, &'a [CodeRef])>;

#[derive(Clone, Copy)]
pub struct WordRefs<'a> {
    pub lanes: &'a [Option<LaneRef>],
    pub consts: &'a [Value],
    pub consts_null: &'a [Value],
    /// The per-code tables in `table_needs` order (`RowRefs::tables`), bound per window.
    pub tables: &'a [TableRef],
    /// The emit's temporaries (`RowRefs::scratch`).
    pub scratch: Mcx<'a>,
}

/// One compare of the word form: the lane (by attno), the condition and the constants' indices (a run of the plan's
/// needs: one constant, an IN list's elements, none).
struct Leaf {
    attno: i16,
    cond: IntCond,
    consts: core::ops::Range<u32>,
    /// A per-code `Bits` root: the code lane and its table's words (no faults).
    bits: Option<(LaneRef, Value)>,
}

impl Leaf {
    fn lane(&self, w: &WordRefs<'_>) -> LaneRef {
        match self.bits {
            Some((l, _)) => l,
            None => w.lanes[self.attno as usize - 1].expect("the shape checked the lane"),
        }
    }
}

/// The word form's tree, in the emit's scratch (`WordRefs::scratch`).
enum Shape<'s> {
    Leaf(Leaf),
    And(PgVec<'s, Shape<'s>>),
    Or(PgVec<'s, Shape<'s>>),
}

fn leaf_of(p: &Program<'_>, plan: &Plan, w: &WordRefs<'_>, site: crate::nodes::SiteId) -> Result<(Leaf, bool), EmitError> {
    let call = &p.sites[site.0 as usize];
    if call.args.len != 2 {
        return Err(EmitError::NotVector);
    }
    let (a, b) = (p.kids[call.args.start as usize], p.kids[call.args.start as usize + 1]);
    let (var, konst, swapped) = match (&p.nodes[a.0 as usize], &p.nodes[b.0 as usize]) {
        (ExprNode::Var(v), ExprNode::Const(_)) => (v, b, false),
        (ExprNode::Const(_), ExprNode::Var(v)) => (v, a, true),
        _ => return Err(EmitError::NotVector),
    };
    if var.src != Source::Scan {
        return Err(EmitError::NotVector);
    }
    let lane = w.lanes.get(var.attno as usize - 1).copied().flatten().ok_or(EmitError::NotVector)?;
    let cond = prim::vector_cond(call.funcid, lane.width, swapped).ok_or(EmitError::NotVector)?;
    let ci = plan.index(konst).ok_or(EmitError::NotVector)?;
    Ok((Leaf { attno: var.attno, cond, consts: ci..ci + 1, bits: None }, lane.kind == gravity_abi::ctx::lane::KIND_BYVAL))
}

fn shape<'s>(p: &Program<'_>, plan: &Plan, w: &WordRefs<'s>, id: NodeId, pc: Codes<'_>) -> Result<Shape<'s>, EmitError> {
    crate::stack_check();
    if let Some((pc, codes)) = pc {
        if let Some(r) = pc.root_at(id) {
            let cr = codes.iter().find(|c| c.attno == r.attno).filter(|_| r.null.is_none()).ok_or(EmitError::NotVector)?;
            return match r.form {
                Form::Bits => match r.table.and_then(|t| w.tables.get(t as usize)) {
                    Some(TableRef::Bits { words, faults: None }) => Ok(Shape::Leaf(Leaf { attno: r.attno, cond: IntCond::Ne, consts: 0..0, bits: Some((cr.lane, *words)) })),
                    _ => Err(EmitError::NotVector),
                },
                _ => Err(EmitError::NotVector),
            };
        }
    }
    match &p.nodes[id.0 as usize] {
        ExprNode::Op(n) | ExprNode::Func(n) => {
            let (leaf, byval) = leaf_of(p, plan, w, n.site)?;
            if !byval {
                return Err(EmitError::NotVector);
            }
            Ok(Shape::Leaf(leaf))
        }
        ExprNode::ScalarArrayOp(n) => {
            if !n.use_or {
                return Err(EmitError::NotVector);
            }
            let call = &p.sites[n.site.0 as usize];
            let (a, arr) = (p.kids[call.args.start as usize], p.kids[call.args.start as usize + 1]);
            let ExprNode::Var(v) = &p.nodes[a.0 as usize] else { return Err(EmitError::NotVector) };
            let lane = w.lanes.get(v.attno as usize - 1).copied().flatten().ok_or(EmitError::NotVector)?;
            let cond = prim::vector_cond(call.funcid, lane.width, false).ok_or(EmitError::NotVector)?;
            if cond != IntCond::Eq || v.src != Source::Scan || lane.kind != gravity_abi::ctx::lane::KIND_BYVAL {
                return Err(EmitError::NotVector);
            }
            let (start, count, null) = plan.elems_of(arr).ok_or(EmitError::NotVector)?;
            if null || count == 0 || count as usize > gravity_program::cost::expr::MAX_LIST {
                return Err(EmitError::NotVector);
            }
            Ok(Shape::Leaf(Leaf { attno: v.attno, cond, consts: start..start + count, bits: None }))
        }
        ExprNode::Bool(n) => {
            let ks = crate::percode::walk_kids(p, id);
            let mut parts = mcx::reserved_in(ks.count_kids(), w.scratch);
            for k in ks {
                parts.push_in(shape(p, plan, w, k, pc)?);
            }
            match n.op {
                BoolExprType::AND_EXPR => Ok(Shape::And(parts)),
                // an OR is a word only over ONE lane (an IN list): a null in another lane would not
                // kill the row where the validity words the caller ANDs in do
                BoolExprType::OR_EXPR => {
                    let (mut first, mut mixed) = (None, false);
                    parts.iter().for_each(|x| {
                        each_leaf(x, &mut |l| match first {
                            None => first = Some(l.attno),
                            Some(a) => mixed |= l.attno != a,
                        })
                    });
                    if mixed {
                        return Err(EmitError::NotVector);
                    }
                    Ok(Shape::Or(parts))
                }
                BoolExprType::NOT_EXPR => Err(EmitError::NotVector),
            }
        }
        ExprNode::Relabel(r) if r.transparent() => shape(p, plan, w, r.arg, pc),
        _ => Err(EmitError::NotVector),
    }
}

/// Each leaf of `s`, left to right.
fn each_leaf(s: &Shape<'_>, f: &mut dyn FnMut(&Leaf)) {
    crate::stack_check();
    match s {
        Shape::Leaf(l) => f(l),
        Shape::And(v) | Shape::Or(v) => v.iter().for_each(|x| each_leaf(x, f)),
    }
}

pub(super) fn vec_ty(width: u8) -> (Type, Type) {
    match width {
        8 => (Type::V2I64, Type::I64),
        4 => (Type::V4I32, Type::I32),
        2 => (Type::V8I16, Type::I16),
        _ => (Type::V16I8, Type::I8),
    }
}

/// Whether the constant compares at the lane's width as it does sign-extended (`I64` 0 / 1); a
/// null constant never vectorises.
pub(super) fn fits(b: &mut FunctionBuilder<'_>, v: Value, null: Value, width: u8, cond: IntCond) -> Value {
    let nn = {
        let t = b.bconst(true);
        b.int_bin(gravity_ir::IntBinOp::Xor, null, t)
    };
    let nn = b.conv(ConvOp::Zext, Type::I64, nn);
    if width == 8 {
        return nn;
    }
    if width == 1 && !matches!(cond, IntCond::Eq | IntCond::Ne) {
        return b.iconst(Type::I64, 0);
    }
    let (_, lty) = vec_ty(width);
    let t = b.conv(ConvOp::Trunc, lty, v);
    let back = if width == 1 { b.conv(ConvOp::Zext, Type::I64, t) } else { b.conv(ConvOp::Sext, Type::I64, t) };
    let same = b.icmp(IntCond::Eq, back, v);
    let s = b.conv(ConvOp::Zext, Type::I64, same);
    b.int_bin(gravity_ir::IntBinOp::And, s, nn)
}

pub fn ok(p: &Program<'_>, root: NodeId, b: &mut FunctionBuilder<'_>, w: &WordRefs<'_>, plan: &Plan, pc: Codes<'_>) -> Result<Value, EmitError> {
    let s = shape(p, plan, w, root, pc)?;
    let mut acc = b.iconst(Type::I64, 1);
    each_leaf(&s, &mut |l| {
        let lane = l.lane(w);
        for c in l.consts.start..l.consts.end {
            let f = fits(b, w.consts[c as usize], w.consts_null[c as usize], lane.width, l.cond);
            acc = b.int_bin(gravity_ir::IntBinOp::And, acc, f);
        }
    });
    Ok(acc)
}

struct Word<'w, 'b, 'a> {
    b: &'w mut FunctionBuilder<'b>,
    w: &'w WordRefs<'a>,
    base: Value,
    /// Each lane's vectors once (by attno) and each constant's splat once (by index and width), in the scratch.
    loads: PgVec<'a, (i16, PgVec<'a, Value>)>,
    splats: PgVec<'a, (u32, u8, Value)>,
    /// The rows still alive (the caller's earlier masks): a `Bits` leaf gathers only those when few.
    within: Option<Value>,
}

/// At most this many live rows gather sparsely (a walk of the set bits); above it the dense 64-row form. The one bound
/// gravity_dict's `gather_mask` reads too (abi site.rs).
const SPARSE_MAX: i64 = gravity_abi::site::SPARSE_GATHER_MAX as i64;

impl Word<'_, '_, '_> {
    fn code_of(b: &mut FunctionBuilder<'_>, lane: LaneRef, cp: Value) -> Value {
        match lane.width {
            8 => {
                let word = b.load(Type::I64, cp, 0);
                let mask = b.iconst(Type::I64, u32::MAX as i64);
                b.int_bin(gravity_ir::IntBinOp::And, word, mask)
            }
            4 => b.load_zext(Type::I32, cp, 0),
            _ => b.load_zext(Type::I16, cp, 0),
        }
    }

    fn bit_of(b: &mut FunctionBuilder<'_>, words: Value, code: Value) -> Value {
        let (six, m63, one) = (b.iconst(Type::I64, 6), b.iconst(Type::I64, 63), b.iconst(Type::I64, 1));
        let wi = b.int_bin(gravity_ir::IntBinOp::Ushr, code, six);
        let wp = b.ptr_add(words, wi, 8, 0);
        let word = b.load(Type::I64, wp, 0);
        let sh = b.int_bin(gravity_ir::IntBinOp::And, code, m63);
        let bit = b.int_bin(gravity_ir::IntBinOp::Ushr, word, sh);
        b.int_bin(gravity_ir::IntBinOp::And, bit, one)
    }

    /// `gather` over the set bits of `within` when at most `SPARSE_MAX` are set, dense otherwise.
    fn gather_within(&mut self, lane: LaneRef, words: Value, within: Value) -> Value {
        let l_dense = self.b.create_block(&[]);
        let l_walk = self.b.create_block(&[Type::I64, Type::I64]);
        let l_step = self.b.create_block(&[]);
        let l_join = self.b.create_block(&[Type::I64]);
        self.b.mark_loop_header(l_walk);
        let pop = self.b.int_un(gravity_ir::IntUnOp::Popcnt, within);
        let lim = self.b.iconst(Type::I64, SPARSE_MAX);
        let dense = self.b.icmp(IntCond::Ugt, pop, lim);
        let zero = self.b.iconst(Type::I64, 0);
        self.b.cond_br(dense, (l_dense, &[]), (l_walk, &[within, zero]));
        self.b.switch_to(l_dense);
        let m = self.gather(lane, words);
        self.b.br(l_join, &[m]);
        self.b.switch_to(l_walk);
        let (rem, acc) = {
            let p = self.b.block_params(l_walk);
            (p[0], p[1])
        };
        let done = self.b.icmp(IntCond::Eq, rem, zero);
        self.b.cond_br(done, (l_join, &[acc]), (l_step, &[]));
        self.b.switch_to(l_step);
        let b = &mut *self.b;
        let t = b.int_un(gravity_ir::IntUnOp::Ctz, rem);
        let i = b.int_bin(gravity_ir::IntBinOp::Add, self.base, t);
        let cp = b.ptr_add(lane.ptr, i, lane.width, 0);
        let code = Self::code_of(b, lane, cp);
        let bit = Self::bit_of(b, words, code);
        let at = b.int_bin(gravity_ir::IntBinOp::Shl, bit, t);
        let acc2 = b.int_bin(gravity_ir::IntBinOp::Or, acc, at);
        let one = b.iconst(Type::I64, 1);
        let r1 = b.int_bin(gravity_ir::IntBinOp::Sub, rem, one);
        let rem2 = b.int_bin(gravity_ir::IntBinOp::And, rem, r1);
        b.br(l_walk, &[rem2, acc2]);
        b.switch_to(l_join);
        b.block_params(l_join)[0]
    }

    /// The index in `loads` of the lane's vectors, loaded at its first leaf.
    fn lane_vectors(&mut self, attno: i16, lane: LaneRef) -> usize {
        if let Some(i) = self.loads.iter().position(|(a, _)| *a == attno) {
            return i;
        }
        let (vty, _) = vec_ty(lane.width);
        let per = (16 / lane.width) as usize;
        let at = self.b.ptr_add(lane.ptr, self.base, lane.width, 0);
        let mut vs = mcx::reserved_in(64 / per, self.w.scratch);
        for k in 0..64 / per {
            vs.push_in(self.b.vload_unaligned(vty, at, (k * 16) as i32));
        }
        self.loads.push_in((attno, vs));
        self.loads.len() - 1
    }

    fn splat(&mut self, c: u32, width: u8) -> Value {
        if let Some((_, _, s)) = self.splats.iter().find(|(i, w, _)| *i == c && *w == width) {
            return *s;
        }
        let s = self.splat_value(self.w.consts[c as usize], width);
        self.splats.push_in((c, width, s));
        s
    }

    fn splat_value(&mut self, v: Value, width: u8) -> Value {
        let (vty, lty) = vec_ty(width);
        let t = if width == 8 { v } else { self.b.conv(ConvOp::Trunc, lty, v) };
        self.b.vsplat(vty, t)
    }

    /// The table's bit per row of the word: eight accumulator chains (`gather_mask`'s dense form),
    /// one code load and one word load per row, no branch.
    fn gather(&mut self, lane: LaneRef, words: Value) -> Value {
        let b = &mut *self.b;
        let mut acc: [Option<Value>; 8] = [None; 8];
        for i in 0..64u32 {
            let cp = b.ptr_add(lane.ptr, self.base, lane.width, (i * lane.width as u32) as i32);
            let code = Self::code_of(b, lane, cp);
            let bit = Self::bit_of(b, words, code);
            let at = if i == 0 {
                bit
            } else {
                let iv = b.iconst(Type::I64, i as i64);
                b.int_bin(gravity_ir::IntBinOp::Shl, bit, iv)
            };
            let a = &mut acc[i as usize % 8];
            *a = Some(match *a {
                Some(x) => b.int_bin(gravity_ir::IntBinOp::Or, x, at),
                None => at,
            });
        }
        acc.iter().flatten().copied().reduce(|x, y| b.int_bin(gravity_ir::IntBinOp::Or, x, y)).expect("eight chains")
    }

    fn leaf(&mut self, l: &Leaf) -> Value {
        let lane = l.lane(self.w);
        if let Some((_, words)) = l.bits {
            return match self.within {
                Some(m) => self.gather_within(lane, words, m),
                None => self.gather(lane, words),
            };
        }
        let per = (16 / lane.width) as usize;
        let li = self.lane_vectors(l.attno, lane);
        let mut splats = mcx::reserved_in(l.consts.len() + 1, self.w.scratch);
        for c in l.consts.start..l.consts.end {
            let s = self.splat(c, lane.width);
            splats.push_in(s);
        }
        let mut m: Option<Value> = None;
        for k in 0..self.loads[li].1.len() {
            let v = self.loads[li].1[k];
            let mut c: Option<Value> = None;
            for &s in &splats {
                let x = self.b.vcmp(l.cond, v, s);
                c = Some(match c {
                    Some(a) => self.b.vbin(VecBinOp::Or, a, x),
                    None => x,
                });
            }
            let bits = self.b.vmovmask(c.expect("a constant"));
            let sh = if k == 0 {
                bits
            } else {
                let kk = self.b.iconst(Type::I64, (k * per) as i64);
                self.b.int_bin(gravity_ir::IntBinOp::Shl, bits, kk)
            };
            m = Some(match m {
                Some(a) => self.b.int_bin(gravity_ir::IntBinOp::Or, a, sh),
                None => sh,
            });
        }
        m.expect("a lane has vectors")
    }

    fn shape(&mut self, s: &Shape<'_>) -> Value {
        crate::stack_check();
        match s {
            Shape::Leaf(l) => self.leaf(l),
            Shape::And(v) | Shape::Or(v) => {
                let and = matches!(s, Shape::And(_));
                let op = if and { gravity_ir::IntBinOp::And } else { gravity_ir::IntBinOp::Or };
                let outer = self.within;
                let mut acc: Option<Value> = None;
                // an AND's `Bits` leaves last, gathered within the earlier members' mask
                let order = v.iter().filter(|x| !(and && is_bits(x))).chain(v.iter().filter(|x| and && is_bits(x)));
                for x in order {
                    if and && is_bits(x) {
                        self.within = acc.or(outer);
                    }
                    let m = self.shape(x);
                    acc = Some(match acc {
                        Some(a) => self.b.int_bin(op, a, m),
                        None => m,
                    });
                }
                self.within = outer;
                acc.expect("a non-empty chain")
            }
        }
    }
}

pub fn emit(p: &Program<'_>, root: NodeId, b: &mut FunctionBuilder<'_>, w: &WordRefs<'_>, plan: &Plan, base: Value, pc: Codes<'_>) -> Result<Value, EmitError> {
    emit_within(p, root, b, w, plan, base, pc, None)
}

/// `emit` with `within`: the rows the caller's earlier words keep — a `Bits` leaf gathers only those
/// when at most `SPARSE_MAX` are set. The result is exact over every row (the caller ANDs it in).
pub fn emit_within(p: &Program<'_>, root: NodeId, b: &mut FunctionBuilder<'_>, w: &WordRefs<'_>, plan: &Plan, base: Value, pc: Codes<'_>, within: Option<Value>) -> Result<Value, EmitError> {
    let s = shape(p, plan, w, root, pc)?;
    let mut wd = Word { b, w, base, loads: PgVec::new_in(w.scratch), splats: PgVec::new_in(w.scratch), within };
    Ok(wd.shape(&s))
}

/// Whether the word is one `Bits` leaf (a per-row table gather): the driver orders it after the compares.
pub fn is_bits_root(p: &Program<'_>, root: NodeId, w: &WordRefs<'_>, plan: &Plan, pc: Codes<'_>) -> bool {
    shape(p, plan, w, root, pc).map(|s| is_bits(&s)).unwrap_or(false)
}

fn is_bits(s: &Shape<'_>) -> bool {
    matches!(s, Shape::Leaf(Leaf { bits: Some(_), .. }))
}
